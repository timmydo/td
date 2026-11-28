//! Two cold boots of the real headless deployment on a private writable volume.
use super::{
    check_persistent_volume, create_persistent_volume_layout, kernel_append, provision_selector,
    release_delivery::fixture_temp::TempDir,
    server_image::{root_owned_identity, ImageInputs},
    RecipeCheckRunner, VolumeLayout, VolumePurpose, PERSIST_READ_CMDLINE_TOKEN,
    PERSIST_WRITE_CMDLINE_TOKEN, SYSTEM_GUEST_MEMORY_MIB, SYSTEM_PERSIST_READ_MARKER,
    SYSTEM_PERSIST_WRITE_MARKER, SYSTEM_ROOT_RO_MARKER, SYSTEM_STATE_WRITABLE_MARKER,
    TD_FIRSTBOOT_NEW_MARKER, TD_FIRSTBOOT_STABLE_MARKER,
};
use std::io::{self, Read};
use std::os::{fd::OwnedFd, unix::net::UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn error(context: &str, cause: impl std::fmt::Display) -> String {
    td_engine::allocation::format(format_args!("{context}: {cause}")).unwrap_or_default()
}

pub(crate) fn run(runner: &RecipeCheckRunner, vmm: &Path) -> Result<(), String> {
    let inputs = ImageInputs::build(runner)?;
    let scratch = TempDir::new_in(runner.scratch_dir(), "native-server")
        .map_err(|e| error("create server fixture", e))?;
    let identity = root_owned_identity(runner)?;
    let selector = provision_selector(&inputs.selector, scratch.path(), &identity)?;
    let parent = scratch.to_str().ok_or("non-UTF-8 server scratch path")?;
    let volume = PathBuf::from(
        td_engine::allocation::format(format_args!("{parent}/server.btrfs"))
            .map_err(|_| String::new())?,
    );
    let fixture = create_persistent_volume_layout(
        &inputs.deployment,
        &inputs.mkfs,
        &inputs.btrfs,
        &volume,
        VolumeLayout::Basic,
        false,
        &identity,
        None,
        VolumePurpose::Fixture,
        None,
        None,
        None,
    )?;
    let selection = td_engine::allocation::format(format_args!(
        "{} {}",
        td_recipe::td_boot_protocol::SELECTED_CURRENT_MARKER,
        fixture.initial_id
    ))
    .map_err(|_| String::new())?;
    for (token, identity, persist) in [
        (
            PERSIST_WRITE_CMDLINE_TOKEN,
            TD_FIRSTBOOT_NEW_MARKER,
            SYSTEM_PERSIST_WRITE_MARKER,
        ),
        (
            PERSIST_READ_CMDLINE_TOKEN,
            TD_FIRSTBOOT_STABLE_MARKER,
            SYSTEM_PERSIST_READ_MARKER,
        ),
    ] {
        let (mut serial, guest) =
            UnixStream::pair().map_err(|e| error("create serial channel", e))?;
        let mut command = Command::new(vmm);
        command
            .arg(&inputs.kernel)
            .arg(&selector)
            .arg(kernel_append(token))
            .args(["--memory-mib", SYSTEM_GUEST_MEMORY_MIB, "--disk-rw"])
            .arg(&volume)
            .stdin(Stdio::null())
            .stdout(Stdio::from(OwnedFd::from(guest)));
        let mut child = Guest(command.spawn().map_err(|e| error("start td-vmm", e))?);
        drop(command);
        let markers = [
            selection.as_str(),
            "kexec_core: Starting new kernel",
            identity,
            SYSTEM_ROOT_RO_MARKER,
            SYSTEM_STATE_WRITABLE_MARKER,
            persist,
        ];
        let timeout = super::boot_timeout();
        observe(&mut serial, &markers, timeout).map_err(|e| error(token, e))?;
        child
            .stop()
            .map_err(|e| error("stop td-vmm at the guest marker", e))?;
        check_persistent_volume(&inputs.btrfs, &volume)?;
    }
    Ok(())
}

struct Guest(Child);

impl Guest {
    fn stop(&mut self) -> io::Result<()> {
        use std::os::unix::process::ExitStatusExt;
        self.0.kill()?;
        if self.0.wait()?.signal() != Some(9) {
            return Err(io::Error::other(
                "guest stopped before the oracle cut power",
            ));
        }
        Ok(())
    }
}

impl Drop for Guest {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

// Only complete serial lines count. Retain at most one marker-sized prefix
// and discard the rest of an oversized line through its newline.
fn observe(serial: &mut UnixStream, markers: &[&str], timeout: Duration) -> Result<(), String> {
    let start = Instant::now();
    let mut expected = markers.iter().copied();
    let mut next = expected.next();
    let mut chunk = [0; 4096];
    let mut line = [0; 128];
    let mut length = 0usize;
    while let Some(waiting) = next {
        let remaining = timeout
            .checked_sub(start.elapsed())
            .filter(|t| !t.is_zero())
            .ok_or_else(|| error("guest timed out waiting for", waiting))?;
        serial
            .set_read_timeout(Some(remaining))
            .map_err(|e| error("serial timeout", e))?;
        let count = match serial.read(&mut chunk) {
            Ok(0) => return Err(error("serial closed while waiting for", waiting)),
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(error(waiting, e)),
        };
        for &byte in chunk.get(..count).ok_or("invalid serial read size")? {
            if byte == b'\n' {
                if let (Some(record), Some(marker)) = (line.get(..length), next) {
                    if record.strip_suffix(b"\r").unwrap_or(record) == marker.as_bytes() {
                        println!("native-boot-server: {marker}");
                        next = expected.next();
                    }
                }
                length = 0;
            } else if let Some(slot) = line.get_mut(length) {
                *slot = byte;
                length += 1;
            } else {
                length = line.len() + 1;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn serial_requires_complete_ordered_lines_within_the_deadline() -> io::Result<()> {
        let (mut reader, mut writer) = UnixStream::pair()?;
        writer.write_all(b"second\nfirstish\n")?;
        writer.write_all(&[b'x'; 150])?;
        writer.write_all(b"first\nfir")?;
        writer.write_all(b"st\r\nsecond")?;
        drop(writer);
        assert!(observe(&mut reader, &["first", "second"], Duration::from_secs(1)).is_err());
        let (mut reader, mut writer) = UnixStream::pair()?;
        writer.write_all(b"first\r\nsecond\n")?;
        assert!(observe(&mut reader, &["first", "second"], Duration::from_secs(1)).is_ok());
        assert!(observe(&mut writer, &["absent"], Duration::ZERO).is_err());
        Ok(())
    }
}

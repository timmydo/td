//! The saved hostname, `lib/td/hostname` in `@var` (td-install/INSTALLER.md),
//! and the synced temporary-file and rename write that publishes it and
//! td-firstboot's other provisioned files. One source: td-firstboot's
//! provisioning and td-authd's `set-hostname` both compile it.

use crate::hostname::Hostname;
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// The name saved at `path`, or none when nothing is: a regular file of
/// `owner`, mode 0644 and at most 64 bytes, read through the inode it was
/// inspected as, holding one name and at most one trailing newline.
pub(crate) fn read_hostname(path: &Path, owner: u32) -> Result<Option<Hostname>, String> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("inspect {}: {error}", path.display())),
    };
    if !metadata.is_file()
        || metadata.uid() != owner
        || metadata.permissions().mode() & 0o7777 != 0o644
        || metadata.len() > 64
    {
        return Err(format!(
            "{} is not a bounded owner-{owner} mode-0644 hostname file",
            path.display()
        ));
    }
    let file =
        std::fs::File::open(path).map_err(|error| format!("open {}: {error}", path.display()))?;
    let opened = file
        .metadata()
        .map_err(|error| format!("inspect opened {}: {error}", path.display()))?;
    if (
        opened.dev(),
        opened.ino(),
        opened.uid(),
        opened.mode(),
        opened.len(),
    ) != (
        metadata.dev(),
        metadata.ino(),
        metadata.uid(),
        metadata.mode(),
        metadata.len(),
    ) {
        return Err(format!("{} changed while opening", path.display()));
    }
    let mut text = String::new();
    file.take(65)
        .read_to_string(&mut text)
        .map_err(|error| format!("read {}: {error}", path.display()))?;
    if text.len() > 64 {
        return Err(format!(
            "{} exceeds the hostname file bound",
            path.display()
        ));
    }
    Hostname::parse(text.strip_suffix('\n').unwrap_or(&text))
        .map(Some)
        .map_err(|error| format!("{}: {error}", path.display()))
}

/// `name` saved at `path` canonically: one newline-terminated line, mode
/// 0644, owned by the writer.
pub(crate) fn write_hostname(path: &Path, name: &Hostname) -> Result<(), String> {
    write_synced(path, format!("{}\n", name.name()).as_bytes(), 0o644, None)
}

/// Writes through a same-directory `.new` temporary and a rename, so no
/// reader and no interrupted writer ever sees a half-written file. Any
/// leftover temporary is unlinked and the new one created exclusively, so
/// `mode` is the creation mode; it is set again through the descriptor,
/// where no umask narrows it, as is `owner`'s UID and GID when given,
/// before the rename publishes the file. The file and then its directory
/// are synced.
pub(crate) fn write_synced(
    path: &Path,
    bytes: &[u8],
    mode: u32,
    owner: Option<(u32, u32)>,
) -> Result<(), String> {
    let directory = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    let mut name = path.as_os_str().to_owned();
    name.push(".new");
    let temporary = PathBuf::from(name);
    match std::fs::remove_file(&temporary) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("clear stale {}: {e}", temporary.display())),
    }
    let write = || -> std::io::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&temporary)?;
        file.set_permissions(std::fs::Permissions::from_mode(mode))?;
        if let Some((uid, gid)) = owner {
            std::os::unix::fs::fchown(&file, Some(uid), Some(gid))?;
        }
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temporary, path)?;
        std::fs::File::open(directory)?.sync_all()
    };
    write().map_err(|e| {
        format!(
            "write {} (mode {mode:o}) through {}: {e}",
            path.display(),
            temporary.display()
        )
    })
}

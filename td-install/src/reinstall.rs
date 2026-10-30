//! Offline replacement of an existing installation. No formatter,
//! partition-table writer or recursive removal belongs on this path.

use std::ffi::OsString;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use td_protector::release::Runner;

use crate::installation_plan::{Mode, Plan, Settings, Storage};
use crate::{invalid, paths, protocol, Workspace};

/// The disk claim owns any successful unlock; no public plan holds it.
pub(crate) struct Claim {
    pub(crate) disk: File,
    pub(crate) unlocked: Option<Unlocked>,
}
impl From<File> for Claim {
    fn from(disk: File) -> Self {
        Self {
            disk,
            unlocked: None,
        }
    }
}
impl std::ops::Deref for Claim {
    type Target = File;
    fn deref(&self) -> &File {
        &self.disk
    }
}
impl std::ops::DerefMut for Claim {
    fn deref_mut(&mut self) -> &mut File {
        &mut self.disk
    }
}

#[derive(PartialEq, Eq)]
struct Snapshot {
    offset: u64,
    length: u64,
    sector: u64,
    uuid: [u8; 16],
    header: [u8; 32],
    gpt: [u8; 32],
}

pub(crate) struct Unlocked {
    snapshot: Snapshot,
    recovery: Option<td_protector::recovery::RecoveryKey>,
}

fn snapshot(claim: &mut File) -> io::Result<Snapshot> {
    let sector = crate::logical_sector_size(claim)?;
    let capacity = claim.seek(SeekFrom::End(0))?;
    let (offset, length) = crate::volume_region(claim, sector, capacity / sector)?;
    let mut window = crate::device_bound::Window::new(claim, offset, length);
    let profile = td_luks::write::load(&mut window).map_err(|e| invalid(e.to_string()))?;
    let header = td_protector::luks2::read(&mut window).map_err(invalid)?;
    td_protector::transition::reinstall_eligible(&header)
        .map_err(|e| invalid(format!("unsupported reinstall header: {e:?}")))?;
    if header.label != protocol::VOLUME_LABEL || profile.uuid != header.uuid {
        return Err(invalid(
            "unsupported encrypted installation identity".into(),
        ));
    }
    let volume = td_luks::header::volume(&profile.json).map_err(invalid)?;
    if volume.reencryption
        || !volume.reencrypt_modes.is_empty()
        || volume.segment.is_none()
        || volume.bound != header.keyslots
    {
        return Err(invalid("unsupported encrypted installation layout".into()));
    }
    crate::VolumeUuid::parse(&header.uuid)?;
    let compact = header.uuid.replace('-', "");
    let mut uuid = [0; 16];
    for (out, pair) in uuid.iter_mut().zip(compact.as_bytes().chunks_exact(2)) {
        *out = u8::from_str_radix(
            std::str::from_utf8(pair).map_err(|_| invalid("UUID encoding".into()))?,
            16,
        )
        .map_err(|_| invalid("UUID digits".into()))?;
    }
    let header_bytes = crate::read_at(claim, offset, 32768)?;
    let mut gpt = crate::read_at(claim, 0, sector * 34)?;
    let tail = capacity
        .checked_sub(sector * 33)
        .ok_or_else(|| invalid("short disk".into()))?;
    gpt.extend_from_slice(&crate::read_at(claim, tail, sector * 33)?);
    let (esp_offset, esp_length) = crate::esp_region(claim, sector, capacity / sector)?;
    let esp = td_fat::Esp::open(claim.try_clone()?, esp_offset, esp_length)
        .map_err(|e| invalid(e.to_string()))?;
    if esp.boot().staged.is_some() {
        return Err(invalid("a selector operation is pending".into()));
    }
    Ok(Snapshot {
        offset,
        length,
        sector,
        uuid,
        header: td_fido::digest(&header_bytes),
        gpt: td_fido::digest(&gpt),
    })
}

fn luks_magic(claim: &mut File, offset: u64) -> io::Result<bool> {
    let secondary = offset
        .checked_add(16384)
        .ok_or_else(|| invalid("header offset overflow".into()))?;
    Ok(crate::read_at(claim, offset, 6)? == b"LUKS\xba\xbe"
        || crate::read_at(claim, secondary, 6)? == b"SKUL\xba\xbe")
}

pub(crate) fn encrypted(claim: &mut File) -> io::Result<bool> {
    let sector = crate::logical_sector_size(claim)?;
    let capacity = claim.seek(SeekFrom::End(0))?;
    let (offset, _) = crate::volume_region(claim, sector, capacity / sector)?;
    if !luks_magic(claim, offset)? {
        return Ok(false);
    }
    snapshot(claim)?;
    Ok(true)
}

pub(crate) fn unlock(
    claim: &mut Claim,
    root: &Path,
    workspace: &Workspace,
    entry: &[u8],
    peer: Option<&UnixStream>,
) -> Result<([u8; 16], Settings), crate::installation_protocol::Refusal> {
    use crate::installation_protocol::Refusal;
    claim.unlocked = None;
    let key = td_protector::recovery::RecoveryKey::parse(entry)
        .map_err(|_| Refusal::RecoveryKeyRejected)?;
    let mut unlocked = Unlocked {
        snapshot: snapshot(&mut claim.disk).map_err(|_| Refusal::EncryptedUnsupported)?,
        recovery: Some(key),
    };
    let observed = with_volume(
        &mut claim.disk,
        root,
        workspace,
        false,
        Some(&mut unlocked),
        peer,
        |volume, _, uuid, _, _| Ok((uuid, settings(volume)?)),
    )
    .map_err(|error| {
        if error.kind() == io::ErrorKind::PermissionDenied {
            Refusal::RecoveryKeyRejected
        } else {
            Refusal::SourceUnavailable
        }
    })?;
    claim.unlocked = Some(unlocked);
    Ok(observed)
}

// The installer reader waits for this request's reply. No next request is
// legal yet, so either a byte or EOF abandons the unlock. Restore its timeout
// before returning ownership to the normal reader.
fn disconnected(mut peer: &UnixStream) -> bool {
    if peer
        .set_read_timeout(Some(Duration::from_millis(1)))
        .is_err()
    {
        return true;
    }
    let mut byte = [0];
    let gone = match peer.read(&mut byte) {
        Ok(_) => true,
        Err(error) => !matches!(
            error.kind(),
            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted
        ),
    };
    let restored = peer.set_read_timeout(None);
    crate::installation_protocol::scrub(&mut byte);
    gone || restored.is_err()
}

struct BoundedLuks {
    tool: td_protector::luks::Luks,
    peer: Option<UnixStream>,
}
impl BoundedLuks {
    fn new(root: &Path) -> Self {
        Self {
            tool: td_protector::luks::Luks::new(root.join("bin/td-luks")),
            peer: None,
        }
    }
}
impl Runner for BoundedLuks {
    fn run(&mut self, args: &[OsString], input: &[u8]) -> io::Result<()> {
        let deadline = Instant::now()
            .checked_add(Duration::from_secs(60))
            .ok_or_else(|| invalid("child deadline overflow".into()))?;
        match self.tool.run_bounded(args, input, deadline, &mut || {
            self.peer.as_ref().is_some_and(disconnected)
        })? {
            td_protector::luks::Bounded::Exited(status) if status.success() => Ok(()),
            td_protector::luks::Bounded::Exited(status) if status.code() == Some(2) => {
                Err(io::ErrorKind::PermissionDenied.into())
            }
            _ => Err(invalid("encrypted reinstall operation failed".into())),
        }
    }
    fn metadata(&mut self, device: &Path) -> io::Result<Vec<u8>> {
        // Same bounded profile reader as td-luks metadata, under our claim.
        td_luks::write::metadata(&mut paths::open_read(device)?)
            .map(String::into_bytes)
            .map_err(|e| invalid(e.to_string()))
    }
}

struct Mapping<'a> {
    runner: BoundedLuks,
    name: &'a str,
    path: PathBuf,
    open: bool,
}
impl Mapping<'_> {
    fn close(&mut self) -> io::Result<()> {
        if self.open {
            let status = self.runner.tool.run_bounded(
                &td_protector::luks::status_args(self.name),
                &[],
                Instant::now() + Duration::from_secs(10),
                &mut || false,
            )?;
            match status {
                td_protector::luks::Bounded::Exited(status)
                    if status.code() == Some(td_protector::luks::STATUS_INACTIVE) => {}
                td_protector::luks::Bounded::Exited(status) if status.success() => {
                    self.runner
                        .run(&td_protector::luks::close_args(self.name), &[])?;
                }
                _ => return Err(invalid("cannot determine owned mapping state".into())),
            }
            self.open = false;
        }
        Ok(())
    }
}
impl Drop for Mapping<'_> {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

fn mapping_matches(
    path: &Path,
    name: &str,
    raw: &Path,
    uuid: &[u8; 16],
    length: u64,
    write: bool,
) -> io::Result<()> {
    let metadata = paths::metadata(path)?;
    if !metadata.file_type().is_block_device() {
        return Err(invalid("mapping is not a block device".into()));
    }
    let (major, minor) = crate::device_numbers(metadata.rdev());
    let base = PathBuf::from(format!("/sys/dev/block/{major}:{minor}"));
    let expected = format!(
        "CRYPT-LUKS2-{}-{name}",
        crate::canonical_uuid(uuid).replace('-', "")
    );
    if paths::read_to_string(&base.join("dm/uuid"))?.trim_end() != expected
        || paths::read_to_string(&base.join("dm/name"))?.trim_end() != name
        || paths::read_to_string(&base.join("ro"))?.trim_end() != if write { "0" } else { "1" }
        || paths::read_to_string(&base.join("size"))?
            .trim_end()
            .parse::<u64>()
            .ok()
            != length
                .checked_sub(td_luks::write::DATA_OFFSET)
                .map(|n| n / 512)
    {
        return Err(invalid(
            "encrypted mapping identity or geometry differs".into(),
        ));
    }
    let slaves = paths::read_dir_bounded(&base.join("slaves"), 2)?;
    let Some(slave) = slaves.first().filter(|_| slaves.len() == 1) else {
        return Err(invalid("mapping must have one backing device".into()));
    };
    let (major, minor) = crate::device_numbers(paths::metadata(raw)?.rdev());
    if paths::read_to_string(&slave.join("dev"))?.trim_end() != format!("{major}:{minor}") {
        return Err(invalid(
            "mapping backing device differs from the claimed loop".into(),
        ));
    }
    Ok(())
}

fn wait_child(
    child: &mut std::process::Child,
    program: &str,
    peer: Option<&UnixStream>,
) -> io::Result<()> {
    let deadline =
        Instant::now() + Duration::from_secs(if program == "td-boot" { 1800 } else { 60 });
    loop {
        let failure = match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => {
                return Err(invalid(format!("reinstall: {program} failed ({status})")))
            }
            Err(error) => error,
            Ok(None) if Instant::now() >= deadline || peer.is_some_and(disconnected) => {
                invalid(format!("reinstall: {program} expired or was abandoned"))
            }
            Ok(None) => {
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }
        };
        let _ = child.kill();
        child.wait()?;
        return Err(failure);
    }
}

fn command(
    root: &Path,
    program: &str,
    args: &[&std::ffi::OsStr],
    peer: Option<&UnixStream>,
) -> io::Result<()> {
    let mut child = Command::new(root.join("bin").join(program))
        .env_clear()
        .current_dir("/")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    wait_child(&mut child, program, peer)
}

fn available_bytes(output: &[u8]) -> io::Result<u64> {
    if output.len() > 256 {
        return Err(invalid("filesystem space output exceeds bound".into()));
    }
    let text = std::str::from_utf8(output)
        .map_err(|_| invalid("invalid filesystem space output".into()))?;
    let mut lines = text.lines();
    if lines.next().map(str::trim) != Some("Avail") {
        return Err(invalid("invalid filesystem space heading".into()));
    }
    let bytes = lines
        .next()
        .map(str::trim)
        .filter(|line| !line.is_empty() && line.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|line| line.parse().ok())
        .ok_or_else(|| invalid("invalid available byte count".into()))?;
    if lines.next().is_some() {
        return Err(invalid("multiple filesystem space records".into()));
    }
    Ok(bytes)
}

fn require_publish_capacity(
    available: u64,
    mut payloads: impl Iterator<Item = u64>,
) -> io::Result<()> {
    // Publication copies the payloads again while retaining the old deployment.
    // Keep a conservative metadata reserve beyond that copy's data bytes.
    let needed = payloads.try_fold(32 * 1024 * 1024u64, |sum, size| {
        sum.checked_add(size)
            .ok_or_else(|| invalid("publication space overflow".into()))
    })?;
    if available < needed {
        return Err(io::Error::new(
            io::ErrorKind::StorageFull,
            "insufficient space for deployment publication and metadata reserve",
        ));
    }
    Ok(())
}

fn preflight_publish_space(root: &Path, volume: &Path, stage: &Path) -> io::Result<()> {
    // df is the installed uutils applet. It is asked for one numeric field
    // of one held mount, with no ambient locale or caller-supplied options.
    let mut child = Command::new(root.join("bin/df"))
        .env_clear()
        .current_dir("/")
        .args(["--block-size=1", "--output=avail", "--"])
        .arg(volume)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    wait_child(&mut child, "df", None)?;
    let mut output = Vec::new();
    child
        .stdout
        .take()
        .ok_or_else(|| invalid("space reader has no output".into()))?
        .take(257)
        .read_to_end(&mut output)?;
    let available = available_bytes(&output)?;
    let sizes: io::Result<Vec<u64>> = STAGED_FILES
        .iter()
        .map(|name| paths::metadata(&stage.join(name)).map(|metadata| metadata.len()))
        .collect();
    require_publish_capacity(available, sizes?.into_iter())
}

fn directory(path: &Path) -> io::Result<()> {
    owned_directory(path, 0)
}

fn owned_directory(path: &Path, owner: u32) -> io::Result<()> {
    let metadata = paths::symlink_metadata(path)?;
    if !metadata.is_dir() || metadata.uid() != owner || metadata.mode() & 0o022 != 0 {
        return Err(invalid(format!(
            "{} must be a real root-owned directory",
            path.display()
        )));
    }
    Ok(())
}

fn setting(volume: &Path, name: &str, limit: u64) -> io::Result<String> {
    let path = volume
        .join(protocol::VOLUME_SUBVOL)
        .join("lib/td")
        .join(name);
    let (file, metadata) = crate::td_fs::open_real_file(&path, "saved installation setting")?;
    if metadata.uid() != 0
        || metadata.nlink() != 1
        || metadata.mode() & 0o7777 != 0o644
        || metadata.len() == 0
        || metadata.len() > limit
    {
        return Err(invalid(format!("invalid saved {name}")));
    }
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    let value =
        std::str::from_utf8(&bytes).map_err(|_| invalid(format!("invalid saved {name}")))?;
    value
        .strip_suffix('\n')
        .map(str::to_owned)
        .ok_or_else(|| invalid(format!("saved {name} lacks its final newline")))
}

fn settings(volume: &Path) -> io::Result<Settings> {
    for relative in [
        "",
        "@var",
        "@var/lib",
        "@var/lib/td",
        "@var/home",
        "td",
        "td/boot",
        "td/deployments",
    ] {
        directory(&volume.join(relative))?;
    }
    let settings = Settings::new(
        &setting(volume, "username", 33)?,
        &setting(volume, "hostname", 64)?,
        "us",
        &setting(volume, "timezone", 65)?,
    )
    .map_err(invalid)?;
    let home = volume.join("@var/home").join(settings.username());
    let metadata = paths::symlink_metadata(&home)?;
    if !metadata.is_dir() || metadata.uid() != 1000 {
        return Err(invalid(
            "the saved account has no real UID-1000 home directory".into(),
        ));
    }
    Ok(settings)
}

struct Mounted<'a> {
    root: &'a Path,
    at: &'a Path,
    mounted: bool,
}

impl Mounted<'_> {
    fn close(&mut self) -> io::Result<()> {
        if self.mounted {
            command(self.root, "umount", &[self.at.as_os_str()], None)?;
            self.mounted = false;
        }
        Ok(())
    }
}

impl Drop for Mounted<'_> {
    fn drop(&mut self) {
        if let Err(error) = self.close() {
            let _ = writeln!(io::stderr(), "reinstall: unmount failed: {error}");
        }
    }
}

fn with_volume<T>(
    claim: &mut File,
    root: &Path,
    workspace: &Workspace,
    write: bool,
    mut unlocked: Option<&mut Unlocked>,
    peer: Option<&UnixStream>,
    action: impl FnOnce(&Path, &Path, [u8; 16], &Path, Option<&mut Unlocked>) -> io::Result<T>,
) -> io::Result<T> {
    let sector = crate::logical_sector_size(claim)?;
    let capacity = claim.seek(SeekFrom::End(0))?;
    let (offset, length) = crate::volume_region(claim, sector, capacity / sector)?;
    if let Some(access) = &unlocked {
        if snapshot(claim)? != access.snapshot {
            return Err(invalid(
                "encrypted installation changed after review".into(),
            ));
        }
    } else if luks_magic(claim, offset)? {
        return Err(invalid(
            "encrypted reinstall requires recovery authorization".into(),
        ));
    }
    let device = crate::loop_device::attach(claim, offset, length, sector)?;
    let mut mapping = Mapping {
        runner: BoundedLuks::new(root),
        name: &workspace.name,
        path: PathBuf::from("/dev/mapper").join(&workspace.name),
        open: false,
    };
    if let Some(access) = &unlocked {
        match paths::symlink_metadata(&mapping.path) {
            Ok(_) => return Err(invalid("inspection mapping already exists".into())),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let status = mapping.runner.tool.run_bounded(
            &td_protector::luks::status_args(mapping.name),
            &[],
            Instant::now() + Duration::from_secs(10),
            &mut || false,
        )?;
        if !matches!(status, td_protector::luks::Bounded::Exited(status) if status.code() == Some(td_protector::luks::STATUS_INACTIVE))
        {
            return Err(invalid("inspection mapping name is already held".into()));
        }
        mapping.runner.peer = peer.map(UnixStream::try_clone).transpose()?;
        let key = access
            .recovery
            .as_ref()
            .ok_or_else(|| invalid("recovery authorization was consumed".into()))?;
        let args = vec![
            OsString::from(if write { "open" } else { "open-read-only" }),
            "0".into(),
            device.path().as_os_str().to_owned(),
            workspace.name.clone().into(),
        ];
        // This unguessable name was absent in both devtmpfs and the kernel.
        // Own cleanup before spawning: a killed child may have created it.
        mapping.open = true;
        let opened = mapping.runner.run(&args, key.passphrase().expose());
        mapping.runner.peer = None;
        if let Err(error) = opened {
            mapping.close()?;
            return Err(error);
        }
        mapping_matches(
            &mapping.path,
            &workspace.name,
            device.path(),
            &access.snapshot.uuid,
            length,
            write,
        )?;
    }
    let node = if mapping.open {
        mapping.path.as_path()
    } else {
        device.path()
    };
    let mut inner = paths::open_read(node)?;
    let bytes = crate::read_at(&mut inner, 65536, 4096)?;
    if bytes.get(64..72) != Some(b"_BHRfS_M") {
        return Err(invalid("the existing td volume is not Btrfs".into()));
    }
    let uuid: [u8; 16] = bytes
        .get(32..48)
        .and_then(|b| b.try_into().ok())
        .ok_or_else(|| invalid("missing existing filesystem UUID".into()))?;
    if unlocked
        .as_ref()
        .is_some_and(|access| access.snapshot.uuid != uuid)
    {
        return Err(invalid("inner and outer volume UUIDs differ".into()));
    }
    drop(inner);
    let mounted_result = command(
        root,
        "mount",
        &[
            "-t".as_ref(),
            "btrfs".as_ref(),
            "-o".as_ref(),
            if write {
                "rw,nodev,nosuid,noexec,subvolid=5".as_ref()
            } else {
                "ro,rescue=nologreplay,nodev,nosuid,noexec,subvolid=5".as_ref()
            },
            node.as_os_str(),
            workspace.mountpoint.as_os_str(),
        ],
        peer,
    );
    let mut mounted = Mounted {
        root,
        at: &workspace.mountpoint,
        mounted: true,
    };
    mounted_result?;
    // The kernel must have mounted the UUID read above as a single-device td
    // filesystem. A backup superblock or a different device cannot substitute.
    let fs = Path::new("/sys/fs/btrfs").join(crate::canonical_uuid(&uuid));
    let checked = (|| {
        if paths::read_to_string(&fs.join("label"))?.trim_end() != protocol::VOLUME_LABEL {
            return Err(invalid(
                "the mounted filesystem is not labelled td-system".into(),
            ));
        }
        let devices = paths::read_dir_bounded(&fs.join("devices"), 2)?;
        if devices.len() != 1
            || devices
                .first()
                .and_then(|p| paths::metadata(&p.join("dev")).ok())
                .is_none()
        {
            return Err(invalid(
                "reinstall requires the claimed single-device filesystem".into(),
            ));
        }
        let mounted_device = devices
            .first()
            .ok_or_else(|| invalid("missing mounted device".into()))?;
        let (major, minor) = crate::device_numbers(paths::metadata(node)?.rdev());
        if paths::read_to_string(&mounted_device.join("dev"))?.trim_end()
            != format!("{major}:{minor}")
        {
            return Err(invalid("mounted filesystem uses another device".into()));
        }
        action(
            &workspace.mountpoint,
            node,
            uuid,
            device.path(),
            unlocked.take(),
        )
    })();
    let unmounted = mounted.close();
    unmounted?;
    drop(mounted);
    mapping.close()?;
    drop(mapping);
    device.release()?;
    checked
}

pub(crate) fn inspect(
    claim: &mut File,
    root: &Path,
    workspace: &Workspace,
) -> io::Result<([u8; 16], Settings)> {
    with_volume(
        claim,
        root,
        workspace,
        false,
        None,
        None,
        |volume, _, uuid, _, _| Ok((uuid, settings(volume)?)),
    )
}

fn approved_manifest(path: &Path, expected: &[u8; 32]) -> io::Result<()> {
    let mut file = paths::open_read(path)?;
    let length = file.metadata()?.len();
    if length > protocol::MAX_MANIFEST_BYTES
        || crate::digest_range(&mut file, 0, length)? != *expected
    {
        return Err(invalid(
            "staged reinstall manifest differs from the approved review".into(),
        ));
    }
    Ok(())
}

fn copy(source: &Path, destination: &Path, limit: u64) -> io::Result<()> {
    let (input, metadata) = crate::td_fs::open_real_file(source, "reinstall source")?;
    if metadata.len() == 0 || metadata.len() > limit {
        return Err(invalid("reinstall input exceeds its bound".into()));
    }
    let mut output = paths::create_new_with_mode(destination, 0o600)?;
    if io::copy(&mut input.take(metadata.len() + 1), &mut output)? != metadata.len() {
        return Err(invalid("reinstall input changed size".into()));
    }
    output.sync_all()
}

const STAGED_FILES: &[&str] = &["manifest", "bzImage", "initramfs.cpio", "root.erofs"];

/// Where, under the volume root, the approved deployment is copied before
/// td-boot publishes it: root-private state on the volume being replaced,
/// so the bytes published are the bytes the review approved whatever the
/// medium does meanwhile.
const STAGE_PARENT: &str = "@var/lib/td-install";

/// The stage's parent on the writable volume, made if absent. Every
/// directory above it is checked first, under this mount, so a link in the
/// volume being replaced cannot send root's writes off it. `owner` is root
/// but in tests.
fn stage_parent(volume: &Path, owner: u32) -> io::Result<std::path::PathBuf> {
    for ancestor in ["", "@var", "@var/lib"] {
        owned_directory(&volume.join(ancestor), owner)?;
    }
    let state = volume.join(STAGE_PARENT);
    match paths::symlink_metadata(&state) {
        Ok(_) => owned_directory(&state, owner)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            paths::create_dir_with_mode(&state, 0o700)?;
        }
        Err(error) => return Err(error),
    }
    Ok(state)
}

// The caller validates root ownership first. Inspect every entry before
// removing anything, and never follow links or recurse into directories.
fn clear_stage(path: &Path) -> io::Result<()> {
    if !paths::symlink_metadata(path)?.is_dir() {
        return Err(invalid("reinstall stage is not a real directory".into()));
    }
    let entries = paths::read_dir_bounded(path, STAGED_FILES.len() + 1)?;
    for entry in &entries {
        if !entry
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| STAGED_FILES.contains(&n))
            || !paths::symlink_metadata(entry)?.is_file()
        {
            return Err(invalid(
                "unexpected entry in interrupted reinstall stage".into(),
            ));
        }
    }
    for entry in entries {
        paths::remove_file_if_present(&entry)?;
    }
    paths::remove_dir_if_present(path)
}

pub(crate) fn execute(
    execution: &crate::LiveExecution,
    plan: &Plan,
    mut claim: File,
    mut unlocked: Option<Unlocked>,
    workspace: &Workspace,
    progress: &mut dyn FnMut(crate::installation_protocol::Phase),
) -> io::Result<()> {
    use crate::installation_protocol::Phase;
    if plan.mode() != Mode::Preserve
        || (plan.storage() == Storage::DeviceBound) != unlocked.is_some()
    {
        return Err(invalid(
            "reinstall requires a preserve plan with its held authorization".into(),
        ));
    }
    // Recheck before the first writable mount. No installer-selected account
    // setting is applied, even when a later retry changes the medium.
    let observed = with_volume(
        &mut claim,
        &execution.root,
        workspace,
        false,
        unlocked.as_mut(),
        None,
        |volume, _, uuid, _, _| Ok((uuid, settings(volume)?)),
    )?;
    if observed != (*plan.volume_uuid(), plan.settings().clone()) {
        return Err(invalid(
            "the existing installation changed after review".into(),
        ));
    }
    // Reject an undersized or damaged ESP before staging or publishing a
    // replacement. Its geometry is read from the existing GPT.
    let sector = crate::logical_sector_size(&claim)?;
    let capacity = claim.seek(SeekFrom::End(0))?;
    let (offset, length) = crate::esp_region(&mut claim, sector, capacity / sector)?;
    let esp = td_fat::Esp::open(claim.try_clone()?, offset, length)
        .map_err(|e| invalid(e.to_string()))?;
    let (kernel, _) = crate::deployment_bytes(&execution.source)?;
    let selector = crate::selector_parts(
        &execution.root.join(protocol::SELECTOR_TEMPLATE_PATH),
        &crate::VolumeUuid::parse(&crate::canonical_uuid(plan.volume_uuid()))?,
    )?
    .total()
    .ok_or_else(|| invalid("selector size overflow".into()))?;
    esp.fits(kernel, selector, false)
        .map_err(|e| invalid(e.to_string()))?;
    drop(esp);
    crate::validate_source(&execution.td_boot, &execution.source).and_then(|id| {
        if plan.matches_deployment_id(&id) {
            Ok(())
        } else {
            Err(invalid("the source changed after review".into()))
        }
    })?;
    let expected_kernel = execution.kernel_digest(plan.deployment())?;
    crate::stage_kernel(&execution.kernel().map_err(|(_, e)| e)?, &workspace.kernel)?;
    let mut staged_kernel = paths::open_read(&workspace.kernel)?;
    let staged_length = staged_kernel.metadata()?.len();
    if crate::digest_range(&mut staged_kernel, 0, staged_length)? != expected_kernel {
        return Err(invalid(
            "reinstall kernel differs from the approved manifest".into(),
        ));
    }
    with_volume(
        &mut claim,
        &execution.root,
        workspace,
        true,
        unlocked.as_mut(),
        None,
        |volume, device, uuid, raw, access| {
            progress(Phase::PublishingDeployment);
            crate::prepare_selector(
                &execution.root.join(protocol::SELECTOR_TEMPLATE_PATH),
                &crate::VolumeUuid::parse(&crate::canonical_uuid(&uuid))?,
                &workspace.selector,
            )?;
            let state = stage_parent(volume, 0)?;
            let stage = state.join("reinstall");
            match paths::symlink_metadata(&stage) {
                Ok(_) => {
                    directory(&stage)?;
                    clear_stage(&stage)?;
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            paths::create_dir_with_mode(&stage, 0o700)?;
            let staged: io::Result<()> = (|| {
                for (name, limit) in [
                    ("manifest", protocol::MAX_MANIFEST_BYTES),
                    ("bzImage", 256 * 1024 * 1024),
                    ("initramfs.cpio", 512 * 1024 * 1024),
                    ("root.erofs", 128 * 1024 * 1024 * 1024),
                ] {
                    copy(
                        &crate::source_payload(&execution.source, name)?,
                        &stage.join(name),
                        limit,
                    )?;
                }
                // td-boot checks every payload against this manifest.
                approved_manifest(&stage.join("manifest"), plan.deployment())?;
                let publish = workspace.dir.join("publish");
                paths::create_dir_with_mode(&publish, 0o700)?;
                let staged_id = crate::validate_source(&execution.td_boot, &stage)?;
                if !plan.matches_deployment_id(&staged_id) {
                    return Err(invalid("staged deployment changed".into()));
                }
                preflight_publish_space(&execution.root, volume, &stage)?;
                if let Some(access) = access {
                    let header =
                        td_protector::luks2::read(&mut paths::open_read(raw)?).map_err(invalid)?;
                    let key = access
                        .recovery
                        .take()
                        .ok_or_else(|| invalid("recovery authorization was consumed".into()))?;
                    let mut runner = BoundedLuks::new(&execution.root);
                    td_protector::release::reinstall(
                        &header,
                        raw,
                        &key,
                        &mut td_protector::release::DeviceTpm,
                        &mut runner,
                    )
                    .map_err(invalid)?;
                    drop(key);
                }
                let installed = command(
                    &execution.root,
                    "td-boot",
                    &[
                        "reinstall".as_ref(),
                        device.as_os_str(),
                        publish.as_os_str(),
                        stage.as_os_str(),
                    ],
                    None,
                );
                // Never recursively remove a mountpoint, including on failure.
                let removed = paths::remove_dir_if_present(&publish);
                installed?;
                removed?;
                paths::open_read(&volume.join("td"))?.sync_all()?;
                Ok(())
            })();
            let cleared = clear_stage(&stage);
            staged?;
            cleared?;
            progress(Phase::VerifyingBoot);
            Ok(())
        },
    )?;
    // The filesystem is synced and unmounted before replacing firmware files.
    let sector = crate::logical_sector_size(&claim)?;
    let capacity = claim.seek(SeekFrom::End(0))?;
    let (offset, length) = crate::esp_region(&mut claim, sector, capacity / sector)?;
    let mut esp = td_fat::Esp::open(claim, offset, length).map_err(|e| invalid(e.to_string()))?;
    let mut kernel = paths::open_read(&workspace.kernel)?;
    let mut selector = paths::open_read(&workspace.selector)?;
    esp.write_direct(
        td_fat::Input {
            len: kernel.metadata()?.len(),
            reader: &mut kernel,
        },
        td_fat::Input {
            len: selector.metadata()?.len(),
            reader: &mut selector,
        },
    )
    .map_err(|e| invalid(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;

    #[test]
    fn publication_capacity_includes_the_second_copy_and_reserve() {
        let bundle = 1024 * 1024 * 1024u64;
        assert!(require_publish_capacity(64 * 1024 * 1024, [bundle].into_iter()).is_err());
        assert!(
            require_publish_capacity(bundle + 32 * 1024 * 1024 - 1, [bundle].into_iter()).is_err()
        );
        require_publish_capacity(bundle + 32 * 1024 * 1024, [bundle].into_iter()).unwrap();
        assert!(require_publish_capacity(u64::MAX, [u64::MAX].into_iter()).is_err());
        assert_eq!(available_bytes(b"Avail\n 123456789\n").unwrap(), 123456789);
        for malformed in [
            b"Avail\n-1\n".as_slice(),
            b"Avail\n0\n1\n",
            b"Size Avail\n123\n",
            b"Avail\n18446744073709551616\n",
        ] {
            assert!(available_bytes(malformed).is_err());
        }
    }

    #[test]
    fn encryption_discovery_recognizes_the_surviving_secondary_magic() {
        let temp = ::td_fs::TempDir::new("reinstall-secondary").unwrap();
        let path = temp.path().join("volume");
        let mut bytes = vec![0; 32768];
        bytes[16384..16390].copy_from_slice(b"SKUL\xba\xbe");
        fs::write(&path, &bytes).unwrap();
        let mut disk = File::open(&path).unwrap();
        assert!(luks_magic(&mut disk, 0).unwrap());
        bytes[16384..16390].fill(0);
        fs::write(&path, &bytes).unwrap();
        assert!(!luks_magic(&mut disk, 0).unwrap());
    }

    #[test]
    fn unlock_peer_probe_detects_disconnect_and_forbidden_pipelining() {
        let (reader, mut writer) = UnixStream::pair().unwrap();
        assert!(!disconnected(&reader));
        assert_eq!(reader.read_timeout().unwrap(), None);
        writer.write_all(b"x").unwrap();
        assert!(disconnected(&reader));
        assert_eq!(reader.read_timeout().unwrap(), None);
        drop(writer);
        assert!(disconnected(&reader));
    }

    #[test]
    fn changed_media_manifest_cannot_be_published_as_the_approved_deployment() {
        let temp = ::td_fs::TempDir::new("reinstall-manifest").unwrap();
        let source = temp.path().join("source");
        let staged = temp.path().join("manifest");
        let reviewed = b"approved manifest with original root digest";
        let mut hash = crate::sha256::Sha256::new();
        hash.update(reviewed);
        let expected = hash.finalize();
        fs::write(&source, reviewed).unwrap();
        copy(&source, &staged, protocol::MAX_MANIFEST_BYTES).unwrap();
        approved_manifest(&staged, &expected).unwrap();
        fs::remove_file(&staged).unwrap();
        fs::write(
            &source,
            b"replacement manifest with unchanged kernel and substituted root",
        )
        .unwrap();
        copy(&source, &staged, protocol::MAX_MANIFEST_BYTES).unwrap();
        assert!(approved_manifest(&staged, &expected).is_err());
    }

    #[test]
    fn iso_payload_names_are_resolved_before_staging() {
        let temp = ::td_fs::TempDir::new("reinstall-iso-names").unwrap();
        let source = temp.path().join("source");
        let stage = temp.path().join("stage");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&stage).unwrap();
        for (iso, name) in protocol::MEDIA_DEPLOYMENT_FILES {
            fs::write(source.join(iso.to_ascii_lowercase()), name.as_bytes()).unwrap();
            copy(
                &crate::source_payload(&source, name).unwrap(),
                &stage.join(name),
                1024,
            )
            .unwrap();
            assert_eq!(fs::read(stage.join(name)).unwrap(), name.as_bytes());
        }
    }

    #[test]
    fn workspace_never_walks_a_publish_mountpoint() {
        let temp = ::td_fs::TempDir::new("reinstall-workspace").unwrap();
        for held in [false, true] {
            let workspace =
                Workspace::create(temp.path(), &[if held { 2 } else { 1 }; 32]).unwrap();
            let publish = workspace.dir.join("publish");
            fs::create_dir(&publish).unwrap();
            if held {
                fs::write(publish.join("personal"), b"keep").unwrap();
            }
            assert_eq!(workspace.remove().is_err(), held);
            if held {
                assert_eq!(fs::read(publish.join("personal")).unwrap(), b"keep");
            } else {
                assert!(!publish.exists());
            }
        }
    }

    #[test]
    fn the_stage_parent_is_made_only_beneath_checked_directories() {
        use std::os::unix::fs::PermissionsExt;
        let temp = ::td_fs::TempDir::new("reinstall-stage-parent").unwrap();
        let owner = fs::metadata(temp.path()).unwrap().uid();
        let volume = temp.path().join("volume");
        let elsewhere = temp.path().join("elsewhere");
        for directory in [&volume, &volume.join("@var"), &elsewhere] {
            fs::create_dir(directory).unwrap();
            fs::set_permissions(directory, fs::Permissions::from_mode(0o755)).unwrap();
        }
        // A link where `@var/lib` belongs: refused, and nothing made there.
        symlink(&elsewhere, volume.join("@var/lib")).unwrap();
        assert!(stage_parent(&volume, owner).is_err());
        assert!(!elsewhere.join("td-install").exists());
        fs::remove_file(volume.join("@var/lib")).unwrap();
        fs::create_dir(volume.join("@var/lib")).unwrap();
        fs::set_permissions(volume.join("@var/lib"), fs::Permissions::from_mode(0o755)).unwrap();
        // A parent others may write is refused too.
        fs::set_permissions(volume.join("@var"), fs::Permissions::from_mode(0o775)).unwrap();
        assert!(stage_parent(&volume, owner).is_err());
        fs::set_permissions(volume.join("@var"), fs::Permissions::from_mode(0o755)).unwrap();
        // Another owner's tree is refused; ours is made at 0700.
        assert!(stage_parent(&volume, owner + 1).is_err());
        let state = stage_parent(&volume, owner).unwrap();
        assert_eq!(state, volume.join(STAGE_PARENT));
        assert_eq!(fs::metadata(&state).unwrap().mode() & 0o777, 0o700);
        assert_eq!(stage_parent(&volume, owner).unwrap(), state);
    }

    #[test]
    fn interrupted_stage_is_reclaimed_without_touching_personal_files() {
        let temp = ::td_fs::TempDir::new("reinstall-stage").unwrap();
        let home = temp.path().join("home");
        fs::write(&home, b"personal bytes").unwrap();
        let stage = temp.path().join("reinstall");
        for _ in 0..2 {
            fs::create_dir(&stage).unwrap();
            fs::write(stage.join("manifest"), b"partial manifest").unwrap();
            fs::write(stage.join("root.erofs"), b"partial payload").unwrap();
            clear_stage(&stage).unwrap();
            assert!(!stage.exists());
            assert_eq!(fs::read(&home).unwrap(), b"personal bytes");
        }
    }

    #[test]
    fn unexpected_stage_contents_and_links_refuse_before_removing_any_file() {
        let temp = ::td_fs::TempDir::new("reinstall-stage-refusal").unwrap();
        let stage = temp.path().join("reinstall");
        fs::create_dir(&stage).unwrap();
        let manifest = stage.join("manifest");
        fs::write(&manifest, b"keep until validated").unwrap();
        let extra = stage.join("unexpected");
        fs::write(&extra, b"keep").unwrap();
        assert!(clear_stage(&stage).is_err());
        assert!(manifest.exists());
        fs::remove_file(&extra).unwrap();
        symlink(&manifest, stage.join("root.erofs")).unwrap();
        assert!(clear_stage(&stage).is_err());
        assert!(manifest.exists());
        let alias = temp.path().join("alias");
        symlink(&stage, &alias).unwrap();
        assert!(clear_stage(&alias).is_err());
        assert_eq!(fs::read(&manifest).unwrap(), b"keep until validated");
    }
}

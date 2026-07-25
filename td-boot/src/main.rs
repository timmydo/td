//! td-boot verifies a deployment on the persistent volume, preferring
//! `current` and falling back to `previous`, then invokes the confined kexec
//! helper. Hashes detect corruption; they do not authenticate a deployment.
#![forbid(unsafe_code)]

mod protocol;
#[path = "../../engine/src/sha256.rs"]
#[allow(dead_code)]
mod sha256;

use std::ffi::{OsStr, OsString};
use std::fs::{self, File, Metadata, OpenOptions, TryLockError};
use std::io::{self, Read, Seek, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{
    DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt, symlink,
};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

const BUSYBOX: &str = "/bin/busybox";
const TD_KEXEC: &str = "/bin/td-kexec";
// root-loop requires procfs so losetup reopens the verified inode, not its path.
const STDIN_PATH: &str = "/proc/self/fd/0";
const MANIFEST_HEADER: &[u8] = b"td-deployment-v1";
const MANIFEST_NAME: &str = "manifest";
const MAX_MANIFEST_BYTES: u64 = 4096;
const MAX_CMDLINE_BYTES: usize = 2048;
const MAX_MOUNTINFO_BYTES: u64 = 1024 * 1024;
const UPDATE_LOCK_DIR: &str = "/run/td-boot-locks";

enum Mode {
    Verify {
        root: PathBuf,
    },
    RootLoop {
        root: PathBuf,
        deployment_id: String,
        loop_device: PathBuf,
    },
    Boot {
        device: PathBuf,
        mountpoint: PathBuf,
        cmdline: OsString,
    },
    Install {
        device: PathBuf,
        mountpoint: PathBuf,
        source: PathBuf,
    },
    Rollback {
        device: PathBuf,
        mountpoint: PathBuf,
    },
}

struct Manifest {
    kernel: String,
    initramfs: String,
    root: String,
}

struct Deployment {
    id: String,
    kernel: File,
    initramfs: File,
}

struct Selection {
    slot: &'static str,
    deployment: Deployment,
    current_error: Option<String>,
}

struct VerifiedBundle {
    id: String,
    manifest: Vec<u8>,
    kernel: File,
    initramfs: File,
    root: File,
}

struct RemoveDirectory {
    path: Option<PathBuf>,
}

impl Drop for RemoveDirectory {
    fn drop(&mut self) {
        if let Some(path) = self.path.take() {
            let _ = fs::remove_dir_all(path);
        }
    }
}

struct RemoveFile {
    path: Option<PathBuf>,
}

impl Drop for RemoveFile {
    fn drop(&mut self) {
        if let Some(path) = self.path.take() {
            let _ = fs::remove_file(path);
        }
    }
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn usage_error() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "usage: td-boot verify <volume-root>\n       td-boot root-loop <volume-root> <deployment-id> <loop-device>\n       td-boot boot <device> <mountpoint> <cmdline>\n       td-boot install <device> <mountpoint> <deployment-directory>\n       td-boot rollback <device> <mountpoint>",
    )
}

fn parse_deployment_id(value: OsString) -> io::Result<String> {
    if !valid_digest(value.as_bytes()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "deployment id must be exactly 64 lowercase hexadecimal characters",
        ));
    }
    String::from_utf8(value.into_vec())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "deployment id is not ASCII"))
}

fn parse_args<I: Iterator<Item = OsString>>(mut args: I) -> io::Result<Mode> {
    match args.next().as_deref() {
        Some(mode) if mode == OsStr::new("verify") => {
            let root = args.next().ok_or_else(usage_error)?;
            if args.next().is_some() {
                return Err(usage_error());
            }
            Ok(Mode::Verify {
                root: PathBuf::from(root),
            })
        }
        Some(mode) if mode == OsStr::new("root-loop") => {
            let root = args.next().ok_or_else(usage_error)?;
            let deployment_id = parse_deployment_id(args.next().ok_or_else(usage_error)?)?;
            let loop_device = args.next().ok_or_else(usage_error)?;
            if args.next().is_some() {
                return Err(usage_error());
            }
            Ok(Mode::RootLoop {
                root: PathBuf::from(root),
                deployment_id,
                loop_device: PathBuf::from(loop_device),
            })
        }
        Some(mode) if mode == OsStr::new("boot") => {
            let device = args.next().ok_or_else(usage_error)?;
            let mountpoint = args.next().ok_or_else(usage_error)?;
            let cmdline = args.next().ok_or_else(usage_error)?;
            if args.next().is_some() {
                return Err(usage_error());
            }
            Ok(Mode::Boot {
                device: PathBuf::from(device),
                mountpoint: PathBuf::from(mountpoint),
                cmdline,
            })
        }
        Some(mode) if mode == OsStr::new("install") => {
            let device = args.next().ok_or_else(usage_error)?;
            let mountpoint = args.next().ok_or_else(usage_error)?;
            let source = args.next().ok_or_else(usage_error)?;
            if args.next().is_some() {
                return Err(usage_error());
            }
            Ok(Mode::Install {
                device: PathBuf::from(device),
                mountpoint: PathBuf::from(mountpoint),
                source: PathBuf::from(source),
            })
        }
        Some(mode) if mode == OsStr::new("rollback") => {
            let device = args.next().ok_or_else(usage_error)?;
            let mountpoint = args.next().ok_or_else(usage_error)?;
            if args.next().is_some() {
                return Err(usage_error());
            }
            Ok(Mode::Rollback {
                device: PathBuf::from(device),
                mountpoint: PathBuf::from(mountpoint),
            })
        }
        _ => Err(usage_error()),
    }
}

fn require_absolute(path: &Path, label: &str) -> io::Result<()> {
    if path.is_absolute() {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{label} must be an absolute path: {}", path.display()),
        ))
    }
}

fn require_real_directory(path: &Path, label: &str) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        io::Error::new(error.kind(), format!("{label} {}: {error}", path.display()))
    })?;
    if metadata.file_type().is_dir() {
        Ok(())
    } else {
        Err(invalid(format!(
            "{label} must be a real directory: {}",
            path.display()
        )))
    }
}

fn open_real_file(path: &Path, label: &str) -> io::Result<(File, Metadata)> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        io::Error::new(error.kind(), format!("{label} {}: {error}", path.display()))
    })?;
    if !metadata.file_type().is_file() {
        return Err(invalid(format!(
            "{label} must be a real regular file: {}",
            path.display()
        )));
    }
    let file = File::open(path)?;
    let opened = file.metadata()?;
    if !opened.file_type().is_file()
        || opened.dev() != metadata.dev()
        || opened.ino() != metadata.ino()
    {
        return Err(invalid(format!(
            "{label} changed while opening: {}",
            path.display()
        )));
    }
    Ok((file, opened))
}

fn read_bounded_real_file(path: &Path, label: &str, limit: u64) -> io::Result<Vec<u8>> {
    let (file, metadata) = open_real_file(path, label)?;
    if metadata.len() > limit {
        return Err(invalid(format!(
            "{label} exceeds {limit} bytes: {}",
            path.display()
        )));
    }

    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(limit.saturating_add(1)).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(invalid(format!(
            "{label} changed while reading or exceeds {limit} bytes: {}",
            path.display()
        )));
    }
    Ok(bytes)
}

fn valid_digest(bytes: &[u8]) -> bool {
    bytes.len() == 64
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

fn parse_manifest_entry(line: &[u8], label: &[u8]) -> io::Result<String> {
    let mut fields = line.splitn(2, |byte| *byte == b' ');
    let digest = fields
        .next()
        .ok_or_else(|| invalid("missing manifest digest"))?;
    let rest = fields
        .next()
        .ok_or_else(|| invalid("missing manifest label"))?;
    if !valid_digest(digest) || rest.strip_prefix(b" ") != Some(label) {
        return Err(invalid(format!(
            "manifest entry must be `<64 lowercase hex>  {}`",
            String::from_utf8_lossy(label)
        )));
    }
    String::from_utf8(digest.to_vec()).map_err(|_| invalid("manifest digest is not ASCII"))
}

fn parse_manifest(bytes: &[u8]) -> io::Result<Manifest> {
    if bytes.is_empty() {
        return Err(invalid("deployment manifest is empty"));
    }
    let mut lines = bytes.split(|byte| *byte == b'\n');
    let header = lines
        .next()
        .ok_or_else(|| invalid("deployment manifest is empty"))?;
    let kernel = lines
        .next()
        .ok_or_else(|| invalid("deployment manifest has no bzImage entry"))?;
    let initramfs = lines
        .next()
        .ok_or_else(|| invalid("deployment manifest has no initramfs entry"))?;
    let root = lines
        .next()
        .ok_or_else(|| invalid("deployment manifest has no root entry"))?;
    let terminator = lines
        .next()
        .ok_or_else(|| invalid("deployment manifest lacks a trailing newline"))?;
    if header != MANIFEST_HEADER || !terminator.is_empty() || lines.next().is_some() {
        return Err(invalid(
            "deployment manifest must have the exact td-deployment-v1 four-line form",
        ));
    }
    Ok(Manifest {
        kernel: parse_manifest_entry(kernel, b"bzImage")?,
        initramfs: parse_manifest_entry(initramfs, b"initramfs.cpio")?,
        root: parse_manifest_entry(root, b"root.erofs")?,
    })
}

fn read_selector(root: &Path, slot: &str) -> io::Result<String> {
    let selector = root.join(protocol::BOOT_DIR).join(slot);
    let metadata = fs::symlink_metadata(&selector).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("{slot} selector {}: {error}", selector.display()),
        )
    })?;
    if !metadata.file_type().is_symlink() {
        return Err(invalid(format!(
            "{slot} selector must be a symlink: {}",
            selector.display()
        )));
    }
    let target = fs::read_link(&selector)?;
    let bytes = target.as_os_str().as_bytes();
    let prefix = protocol::SELECTOR_PREFIX.as_bytes();
    let id = bytes
        .strip_prefix(prefix)
        .ok_or_else(|| invalid(format!("{slot} selector has an invalid target")))?;
    if !valid_digest(id) {
        return Err(invalid(format!(
            "{slot} selector must target ../deployments/<64 lowercase hex>"
        )));
    }
    String::from_utf8(id.to_vec()).map_err(|_| invalid(format!("{slot} selector id is not ASCII")))
}

fn verify_payload(directory: &Path, name: &str, expected: &str) -> io::Result<File> {
    let path = directory.join(name);
    let (mut file, _) = open_real_file(&path, "deployment payload")?;
    let actual = sha256::sha256_reader(&mut file)?;
    if actual == expected {
        file.rewind()?;
        Ok(file)
    } else {
        Err(invalid(format!(
            "{name} hash mismatch: expected {expected}, got {actual}"
        )))
    }
}

fn open_bundle(directory: &Path) -> io::Result<VerifiedBundle> {
    require_absolute(directory, "deployment directory")?;
    require_real_directory(directory, "deployment directory")?;
    let manifest_path = directory.join(MANIFEST_NAME);
    let manifest =
        read_bounded_real_file(&manifest_path, "deployment manifest", MAX_MANIFEST_BYTES)?;
    let parsed = parse_manifest(&manifest)?;
    let id = sha256::hex_digest(&manifest);
    let kernel = verify_payload(directory, "bzImage", &parsed.kernel)?;
    let initramfs = verify_payload(directory, "initramfs.cpio", &parsed.initramfs)?;
    let root = verify_payload(directory, "root.erofs", &parsed.root)?;
    Ok(VerifiedBundle {
        id,
        manifest,
        kernel,
        initramfs,
        root,
    })
}

fn sync_directory(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

fn parse_decimal_u32(bytes: &[u8]) -> Option<u32> {
    if bytes.is_empty() || !bytes.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let text = std::str::from_utf8(bytes).ok()?;
    text.parse::<u32>().ok().filter(|pid| *pid != 0)
}

fn temporary_owner_pid(name: &[u8], prefix: &[u8], has_digest: bool) -> Option<u32> {
    let mut suffix = name.strip_prefix(prefix)?;
    if has_digest {
        let digest = suffix.get(..64)?;
        if !valid_digest(digest) {
            return None;
        }
        suffix = suffix.get(64..)?.strip_prefix(b"-")?;
    }
    let separator = suffix.iter().position(|byte| *byte == b'-')?;
    let pid = parse_decimal_u32(suffix.get(..separator)?)?;
    let attempt = suffix.get(separator.saturating_add(1)..)?;
    if attempt.is_empty() || !attempt.iter().all(u8::is_ascii_digit) {
        return None;
    }
    Some(pid)
}

fn reap_install_temporaries(deployments: &Path) -> io::Result<()> {
    let mut changed = false;
    for entry in fs::read_dir(deployments)? {
        let entry = entry?;
        let name = entry.file_name();
        if temporary_owner_pid(name.as_bytes(), b".install-", true).is_none() {
            continue;
        }
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_dir() {
            fs::remove_dir_all(&path)?;
        } else {
            fs::remove_file(&path)?;
        }
        changed = true;
    }
    if changed {
        sync_directory(deployments)?;
    }
    Ok(())
}

fn reap_selector_temporaries(boot: &Path) -> io::Result<()> {
    let mut changed = false;
    for entry in fs::read_dir(boot)? {
        let entry = entry?;
        let name = entry.file_name();
        let pid = temporary_owner_pid(name.as_bytes(), b".current-", false)
            .or_else(|| temporary_owner_pid(name.as_bytes(), b".previous-", false));
        if pid.is_none() {
            continue;
        }
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_dir() {
            fs::remove_dir_all(&path)?;
        } else {
            fs::remove_file(&path)?;
        }
        changed = true;
    }
    if changed {
        sync_directory(boot)?;
    }
    Ok(())
}

fn create_install_directory(parent: &Path, id: &str) -> io::Result<PathBuf> {
    for attempt in 0..1024u32 {
        let path = parent.join(format!(".install-{id}-{}-{attempt}", std::process::id()));
        match fs::create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(io::Error::new(
                    error.kind(),
                    format!(
                        "create deployment staging directory {}: {error}",
                        path.display()
                    ),
                ));
            }
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!(
            "could not create a unique deployment staging directory below {}",
            parent.display()
        ),
    ))
}

fn copy_verified_payload(source: &mut File, destination: &Path) -> io::Result<()> {
    source.rewind()?;
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)?;
    io::copy(source, &mut output)?;
    output.sync_all()
}

fn write_synced_file(destination: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)?;
    output.write_all(bytes)?;
    output.sync_all()
}

fn existing_bundle_matches(directory: &Path, id: &str) -> io::Result<bool> {
    open_bundle(directory).map(|bundle| bundle.id == id)
}

fn publish_bundle(root: &Path, mut bundle: VerifiedBundle) -> io::Result<String> {
    let deployments = root.join(protocol::DEPLOYMENTS_DIR);
    require_real_directory(&deployments, "deployments directory")?;
    let destination = deployments.join(&bundle.id);
    match fs::symlink_metadata(&destination) {
        Ok(_) => {
            if existing_bundle_matches(&destination, &bundle.id)? {
                sync_directory(&deployments)?;
                return Ok(bundle.id);
            }
            return Err(invalid(format!(
                "existing deployment {} does not verify as {}",
                destination.display(),
                bundle.id
            )));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    let staging = create_install_directory(&deployments, &bundle.id)?;
    let mut cleanup = RemoveDirectory {
        path: Some(staging.clone()),
    };
    copy_verified_payload(&mut bundle.kernel, &staging.join("bzImage"))?;
    copy_verified_payload(&mut bundle.initramfs, &staging.join("initramfs.cpio"))?;
    copy_verified_payload(&mut bundle.root, &staging.join("root.erofs"))?;
    write_synced_file(&staging.join(MANIFEST_NAME), &bundle.manifest)?;
    sync_directory(&staging)?;

    let staged = open_bundle(&staging)?;
    if staged.id != bundle.id {
        return Err(invalid(format!(
            "staged deployment id {} changed from verified source {}",
            staged.id, bundle.id
        )));
    }

    match fs::rename(&staging, &destination) {
        Ok(()) => cleanup.path = None,
        Err(rename_error) => {
            match existing_bundle_matches(&destination, &bundle.id) {
                Ok(true) => {
                    sync_directory(&deployments)?;
                    return Ok(bundle.id);
                }
                Ok(false) => {}
                Err(existing_error) => {
                    return Err(io::Error::new(
                        rename_error.kind(),
                        format!(
                            "publish deployment {} -> {}: {rename_error}; concurrent \
                             destination verification failed: {existing_error}",
                            staging.display(),
                            destination.display()
                        ),
                    ));
                }
            }
            return Err(io::Error::new(
                rename_error.kind(),
                format!(
                    "publish deployment {} -> {}: {rename_error}",
                    staging.display(),
                    destination.display()
                ),
            ));
        }
    }
    sync_directory(&deployments)?;
    Ok(bundle.id)
}

fn create_selector_link(boot: &Path, slot: &str, id: &str) -> io::Result<PathBuf> {
    for attempt in 0..1024u32 {
        let path = boot.join(format!(".{slot}-{}-{attempt}", std::process::id()));
        match symlink(format!("{}{id}", protocol::SELECTOR_PREFIX), &path) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(io::Error::new(
                    error.kind(),
                    format!(
                        "create temporary {slot} selector {}: {error}",
                        path.display()
                    ),
                ));
            }
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!(
            "could not create a unique temporary {slot} selector below {}",
            boot.display()
        ),
    ))
}

fn replace_selector(root: &Path, slot: &str, id: &str) -> io::Result<()> {
    if !valid_digest(id.as_bytes()) {
        return Err(invalid("selector deployment id is not a valid digest"));
    }
    let boot = root.join(protocol::BOOT_DIR);
    require_real_directory(&boot, "boot selector directory")?;
    let temporary = create_selector_link(&boot, slot, id)?;
    let mut cleanup = RemoveFile {
        path: Some(temporary.clone()),
    };
    let destination = boot.join(slot);
    fs::rename(&temporary, &destination).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "replace {slot} selector {} -> {}: {error}",
                temporary.display(),
                destination.display()
            ),
        )
    })?;
    cleanup.path = None;
    sync_directory(&boot)
}

fn selectors_are_absent(root: &Path) -> io::Result<bool> {
    let boot = root.join(protocol::BOOT_DIR);
    for slot in ["current", "previous"] {
        match fs::symlink_metadata(boot.join(slot)) {
            Ok(_) => return Ok(false),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(true)
}

fn activate_install(root: &Path, previous: &str, current: &str) -> io::Result<()> {
    // Preserve a verified fallback across every crash prefix.
    replace_selector(root, "previous", previous)?;
    replace_selector(root, "current", current)
}

fn install_deployment(root: &Path, source: &Path) -> io::Result<String> {
    require_absolute(root, "volume root")?;
    require_absolute(source, "deployment source")?;
    require_real_directory(root, "volume root")?;
    require_real_directory(&root.join("td"), "td directory")?;
    require_real_directory(&root.join(protocol::BOOT_DIR), "boot selector directory")?;
    require_real_directory(
        &root.join(protocol::DEPLOYMENTS_DIR),
        "deployments directory",
    )?;
    reap_install_temporaries(&root.join(protocol::DEPLOYMENTS_DIR))?;
    reap_selector_temporaries(&root.join(protocol::BOOT_DIR))?;

    let bundle = open_bundle(source)?;
    let candidate = bundle.id.clone();
    let selection = match select_deployment(root) {
        Ok(selection) => Some(selection),
        Err(_) if selectors_are_absent(root)? => None,
        Err(error) => {
            return Err(invalid(format!(
                "refusing activation without a verified existing deployment: {error}"
            )));
        }
    };
    let known_good = match &selection {
        Some(selected) => selected.deployment.id.clone(),
        None => candidate.clone(),
    };
    let installed = publish_bundle(root, bundle)?;
    if selection
        .as_ref()
        .is_some_and(|selected| selected.slot == "current" && selected.deployment.id == installed)
    {
        if verify_slot(root, "previous").is_err() {
            replace_selector(root, "previous", &installed)?;
        }
        return Ok(installed);
    }
    activate_install(root, &known_good, &installed)?;
    Ok(installed)
}

fn rollback_deployment(root: &Path) -> io::Result<String> {
    require_absolute(root, "volume root")?;
    require_real_directory(root, "volume root")?;
    require_real_directory(&root.join(protocol::BOOT_DIR), "boot selector directory")?;
    reap_selector_temporaries(&root.join(protocol::BOOT_DIR))?;
    let previous = verify_slot(root, "previous")?;
    let previous_id = previous.id;
    replace_selector(root, "current", &previous_id)?;
    Ok(previous_id)
}

fn verified_manifest(root: &Path, id: &str) -> io::Result<(PathBuf, Manifest)> {
    require_absolute(root, "volume root")?;
    require_real_directory(root, "volume root")?;
    require_real_directory(&root.join("td"), "td directory")?;
    require_real_directory(
        &root.join(protocol::DEPLOYMENTS_DIR),
        "deployments directory",
    )?;
    if !valid_digest(id.as_bytes()) {
        return Err(invalid(
            "deployment id must be exactly 64 lowercase hexadecimal characters",
        ));
    }

    let directory = root.join(protocol::DEPLOYMENTS_DIR).join(id);
    require_real_directory(&directory, "deployment")?;
    let manifest_path = directory.join(MANIFEST_NAME);
    let manifest_bytes =
        read_bounded_real_file(&manifest_path, "deployment manifest", MAX_MANIFEST_BYTES)?;
    let manifest_id = sha256::hex_digest(&manifest_bytes);
    if manifest_id.as_str() != id {
        return Err(invalid(format!(
            "deployment id {id} does not match manifest hash {manifest_id}"
        )));
    }
    let manifest = parse_manifest(&manifest_bytes)?;
    Ok((directory, manifest))
}

fn verify_slot(root: &Path, slot: &str) -> io::Result<Deployment> {
    let id = read_selector(root, slot)?;
    let (directory, manifest) = verified_manifest(root, &id)?;
    let kernel = verify_payload(&directory, "bzImage", &manifest.kernel)?;
    let initramfs = verify_payload(&directory, "initramfs.cpio", &manifest.initramfs)?;
    // Verify root here so corruption selects previous; root-loop repeats the hash
    // after kexec to bind the verified inode at the actual mount boundary.
    verify_payload(&directory, "root.erofs", &manifest.root)?;

    Ok(Deployment {
        id,
        kernel,
        initramfs,
    })
}

fn verify_root_payload(root: &Path, deployment_id: &str) -> io::Result<File> {
    let (directory, manifest) = verified_manifest(root, deployment_id)?;
    verify_payload(&directory, "root.erofs", &manifest.root)
}

fn select_deployment(root: &Path) -> io::Result<Selection> {
    require_absolute(root, "volume root")?;
    require_real_directory(root, "volume root")?;
    require_real_directory(&root.join("td"), "td directory")?;
    require_real_directory(&root.join(protocol::BOOT_DIR), "boot selector directory")?;
    require_real_directory(
        &root.join(protocol::DEPLOYMENTS_DIR),
        "deployments directory",
    )?;

    match verify_slot(root, "current") {
        Ok(deployment) => Ok(Selection {
            slot: "current",
            deployment,
            current_error: None,
        }),
        Err(current) => match verify_slot(root, "previous") {
            Ok(deployment) => Ok(Selection {
                slot: "previous",
                deployment,
                current_error: Some(current.to_string()),
            }),
            Err(previous) => Err(invalid(format!(
                "no verified deployment: current rejected ({current}); previous rejected ({previous})"
            ))),
        },
    }
}

fn kernel_cmdline(base: &OsStr, deployment_id: &str) -> io::Result<OsString> {
    let bytes = base.as_bytes();
    if !bytes
        .iter()
        .all(|byte| matches!(byte, b' '..=b'~') && !matches!(byte, b'"' | b'\'' | b'\\'))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "kernel cmdline must contain unquoted printable ASCII only",
        ));
    }
    const SELECTOR: &[u8] = b"td.deployment=";
    if bytes
        .windows(SELECTOR.len())
        .any(|window| window == SELECTOR)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "kernel cmdline already contains td.deployment=",
        ));
    }

    let token = format!("td.deployment={deployment_id}");
    let separator = usize::from(!bytes.is_empty());
    if bytes.len() + separator + token.len() + 1 > MAX_CMDLINE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("kernel cmdline exceeds {MAX_CMDLINE_BYTES} bytes"),
        ));
    }
    let mut output = Vec::with_capacity(bytes.len() + separator + token.len());
    output.extend_from_slice(bytes);
    if !bytes.is_empty() {
        output.push(b' ');
    }
    output.extend_from_slice(token.as_bytes());
    Ok(OsString::from_vec(output))
}

fn run_command(command: &mut Command, label: &str) -> io::Result<()> {
    let status = command
        .status()
        .map_err(|error| io::Error::new(error.kind(), format!("could not run {label}: {error}")))?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "{label} exited with status {status}"
        )))
    }
}

fn btrfs_mount_command(device: &Path, mountpoint: &Path, options: &str) -> Command {
    let mut command = Command::new(BUSYBOX);
    command.args([
        OsStr::new(protocol::MOUNT_APPLET),
        OsStr::new("-t"),
        OsStr::new("btrfs"),
        OsStr::new("-o"),
        OsStr::new(options),
        device.as_os_str(),
        mountpoint.as_os_str(),
    ]);
    command
}

fn mount_command(device: &Path, mountpoint: &Path) -> Command {
    btrfs_mount_command(device, mountpoint, "ro,nodev,nosuid,noexec")
}

fn writable_mount_command(device: &Path, mountpoint: &Path) -> Command {
    btrfs_mount_command(device, mountpoint, "rw,nodev,nosuid,noexec")
}

fn unmount_command(mountpoint: &Path) -> Command {
    let mut command = Command::new(BUSYBOX);
    command
        .arg(protocol::UMOUNT_APPLET)
        .arg(mountpoint.as_os_str());
    command
}

fn kexec_command(kernel: File, initramfs: File, cmdline: &OsStr) -> Command {
    let mut command = Command::new(TD_KEXEC);
    command
        .arg("--fds")
        .arg(cmdline)
        .stdin(Stdio::from(kernel))
        .stdout(Stdio::from(initramfs));
    command
}

fn loop_command(root: File, loop_device: &Path) -> Command {
    let mut command = Command::new(BUSYBOX);
    command
        .args([
            OsStr::new(protocol::LOSETUP_APPLET),
            OsStr::new("-r"),
            loop_device.as_os_str(),
            OsStr::new(STDIN_PATH),
        ])
        .stdin(Stdio::from(root));
    command
}

fn report_fallback(selection: &Selection) -> io::Result<()> {
    if let Some(error) = &selection.current_error {
        // Rejection is boot protocol, not best-effort diagnostics: an unobservable
        // fallback must not masquerade as a healthy primary selection.
        writeln!(
            io::stderr(),
            "{}: current rejected ({error}); using previous {}",
            protocol::CURRENT_REJECTED_MARKER,
            selection.deployment.id
        )?;
    }
    Ok(())
}

fn report_boot_selection(selection: &Selection) -> io::Result<()> {
    let marker = match selection.slot {
        "current" => protocol::SELECTED_CURRENT_MARKER,
        "previous" => protocol::SELECTED_PREVIOUS_MARKER,
        _ => {
            return Err(invalid(format!(
                "internal: unknown deployment slot {}",
                selection.slot
            )));
        }
    };
    writeln!(io::stderr(), "{marker} {}", selection.deployment.id)
}

fn run_verify(root: &Path) -> io::Result<()> {
    let selection = select_deployment(root)?;
    report_fallback(&selection)?;
    writeln!(
        io::stdout(),
        "{} {}",
        selection.slot,
        selection.deployment.id
    )
}

fn run_root_loop(root: &Path, deployment_id: &str, loop_device: &Path) -> io::Result<()> {
    require_absolute(loop_device, "loop device")?;
    let root_file = verify_root_payload(root, deployment_id)?;
    run_command(
        &mut loop_command(root_file, loop_device),
        "read-only root loop setup",
    )
}

fn best_effort_unmount(mountpoint: &Path) {
    let _ = unmount_command(mountpoint).status();
}

fn acquire_lock_at(path: &Path) -> io::Result<File> {
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)?;
    let entry = fs::symlink_metadata(path)?;
    let opened = lock.metadata()?;
    if !entry.file_type().is_file()
        || !opened.file_type().is_file()
        || entry.dev() != opened.dev()
        || entry.ino() != opened.ino()
    {
        return Err(invalid(format!(
            "update lock must be a stable real file: {}",
            path.display()
        )));
    }
    match lock.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => {
            let _ = writeln!(
                io::stderr(),
                "td-boot: waiting for deployment transaction lock {}",
                path.display()
            );
            lock.lock()?;
        }
        Err(TryLockError::Error(error)) => return Err(error),
    }
    Ok(lock)
}

fn update_lock_directory() -> io::Result<PathBuf> {
    let path = PathBuf::from(UPDATE_LOCK_DIR);
    let mut builder = fs::DirBuilder::new();
    builder.mode(0o700);
    match builder.create(&path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    let metadata = fs::symlink_metadata(&path)?;
    if !metadata.file_type().is_dir()
        || metadata.uid() != 0
        || metadata.permissions().mode() & 0o777 != 0o700
    {
        return Err(invalid(format!(
            "update lock directory must be a root-owned real directory with mode 0700: {}",
            path.display()
        )));
    }
    Ok(path)
}

fn acquire_update_locks_at(directory: &Path, device_id: u64) -> io::Result<(File, File)> {
    let transaction = acquire_lock_at(&directory.join("transactions.lock"))?;
    let device = acquire_lock_at(&directory.join(format!("block-{device_id:016x}.lock")))?;
    Ok((transaction, device))
}

fn acquire_update_locks(device: &Path) -> io::Result<(File, File, u64)> {
    let metadata = fs::metadata(device).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("volume device {}: {error}", device.display()),
        )
    })?;
    if !metadata.file_type().is_block_device() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("volume device must be a block device: {}", device.display()),
        ));
    }
    let directory = update_lock_directory()?;
    let device_id = metadata.rdev();
    let (transaction, device) = acquire_update_locks_at(&directory, device_id)?;
    Ok((transaction, device, device_id))
}

fn decode_mountinfo_field(field: &[u8]) -> io::Result<OsString> {
    let mut decoded = Vec::with_capacity(field.len());
    let mut bytes = field.iter().copied();
    while let Some(byte) = bytes.next() {
        if byte != b'\\' {
            decoded.push(byte);
            continue;
        }
        let a = bytes
            .next()
            .filter(u8::is_ascii_digit)
            .ok_or_else(|| invalid("mountinfo contains a malformed escape"))?;
        let b = bytes
            .next()
            .filter(u8::is_ascii_digit)
            .ok_or_else(|| invalid("mountinfo contains a malformed escape"))?;
        let c = bytes
            .next()
            .filter(u8::is_ascii_digit)
            .ok_or_else(|| invalid("mountinfo contains a malformed escape"))?;
        if a > b'7' || b > b'7' || c > b'7' {
            return Err(invalid("mountinfo contains a malformed escape"));
        }
        decoded.push((a - b'0') * 64 + (b - b'0') * 8 + (c - b'0'));
    }
    Ok(OsString::from_vec(decoded))
}

fn mounted_btrfs_source(
    mountinfo: &[u8],
    mountpoint: &Path,
) -> io::Result<Option<OsString>> {
    let mut found = None;
    for line in mountinfo.split(|byte| *byte == b'\n') {
        let mut fields = line.split(|byte| *byte == b' ');
        let Some(root) = fields.nth(3) else {
            continue;
        };
        let Some(encoded_mountpoint) = fields.next() else {
            continue;
        };
        if decode_mountinfo_field(encoded_mountpoint)?.as_os_str() != mountpoint.as_os_str() {
            continue;
        }
        if root != b"/" {
            return Err(invalid(format!(
                "update mountpoint has an unexpected mounted root: {}",
                mountpoint.display()
            )));
        }
        let options = fields
            .next()
            .ok_or_else(|| invalid("mountinfo entry is missing mount options"))?;
        for required in [
            b"rw".as_slice(),
            b"nodev".as_slice(),
            b"nosuid".as_slice(),
            b"noexec".as_slice(),
        ] {
            if !options.split(|byte| *byte == b',').any(|option| option == required) {
                return Err(invalid(format!(
                    "update mountpoint has unexpected mount options: {}",
                    mountpoint.display()
                )));
            }
        }
        let mut filesystem = None;
        while let Some(field) = fields.next() {
            if field == b"-" {
                filesystem = fields.next().zip(fields.next());
                break;
            }
        }
        let Some((kind, source)) = filesystem else {
            return Err(invalid("mountinfo entry is missing filesystem fields"));
        };
        if kind != b"btrfs" {
            return Err(invalid(format!(
                "update mountpoint has an unexpected filesystem: {}",
                mountpoint.display()
            )));
        }
        if found.is_some() {
            return Err(invalid(format!(
                "update mountpoint has stacked mounts: {}",
                mountpoint.display()
            )));
        }
        found = Some(decode_mountinfo_field(source)?);
    }
    Ok(found)
}

fn prepare_update_mountpoint(mountpoint: &Path, device_id: u64) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true).mode(0o700);
    match builder.create(mountpoint) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    let parent = mountpoint.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("update mountpoint has no parent: {}", mountpoint.display()),
        )
    })?;
    let parent_metadata = fs::metadata(parent)?;
    let mut metadata = fs::symlink_metadata(mountpoint)?;
    if !metadata.file_type().is_dir() {
        return Err(invalid(format!(
            "update mountpoint must be a real directory: {}",
            mountpoint.display()
        )));
    }
    if metadata.dev() != parent_metadata.dev() {
        let mountinfo = read_bounded_real_file(
            Path::new("/proc/self/mountinfo"),
            "process mount table",
            MAX_MOUNTINFO_BYTES,
        )?;
        let source = mounted_btrfs_source(&mountinfo, mountpoint)?.ok_or_else(|| {
            invalid(format!(
                "update mountpoint is mounted but absent from mountinfo: {}",
                mountpoint.display()
            ))
        })?;
        let source_path = Path::new(&source);
        let source_metadata = fs::metadata(source_path)?;
        if !source_metadata.file_type().is_block_device() || source_metadata.rdev() != device_id {
            return Err(invalid(format!(
                "update mountpoint is mounted from a different device: {}",
                mountpoint.display()
            )));
        }
        run_command(
            &mut unmount_command(mountpoint),
            "stale Btrfs update unmount",
        )?;
        metadata = fs::symlink_metadata(mountpoint)?;
    }
    if metadata.dev() != parent_metadata.dev()
        || metadata.uid() != 0
        || metadata.permissions().mode() & 0o777 != 0o700
    {
        return Err(invalid(format!(
            "update mountpoint must be a root-owned unmounted directory with mode 0700: {}",
            mountpoint.display()
        )));
    }
    Ok(())
}

fn run_on_writable_volume<T>(
    device: &Path,
    mountpoint: &Path,
    operation: impl FnOnce(&Path) -> io::Result<T>,
) -> io::Result<T> {
    require_absolute(device, "volume device")?;
    require_absolute(mountpoint, "mountpoint")?;
    let (_transaction_lock, _device_lock, device_id) = acquire_update_locks(device)?;
    prepare_update_mountpoint(mountpoint, device_id)?;
    run_command(
        &mut writable_mount_command(device, mountpoint),
        "read-write Btrfs mount",
    )?;

    let result = operation(mountpoint);
    let unmounted = run_command(&mut unmount_command(mountpoint), "Btrfs unmount");
    match (result, unmounted) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(error)) => Err(io::Error::new(
            error.kind(),
            format!("deployment transaction committed, but {error}"),
        )),
        (Err(operation_error), Err(unmount_error)) => Err(io::Error::new(
            operation_error.kind(),
            format!("{operation_error}; additionally {unmount_error}"),
        )),
    }
}

fn run_boot(device: &Path, mountpoint: &Path, base_cmdline: &OsStr) -> io::Result<()> {
    require_absolute(device, "volume device")?;
    require_absolute(mountpoint, "mountpoint")?;
    fs::create_dir_all(mountpoint)?;
    require_real_directory(mountpoint, "mountpoint")?;

    run_command(
        &mut mount_command(device, mountpoint),
        "read-only Btrfs mount",
    )?;

    let result = (|| {
        let selection = select_deployment(mountpoint)?;
        report_fallback(&selection)?;
        report_boot_selection(&selection)?;
        let cmdline = kernel_cmdline(base_cmdline, &selection.deployment.id)?;
        let Deployment {
            kernel, initramfs, ..
        } = selection.deployment;
        run_command(
            &mut kexec_command(kernel, initramfs, cmdline.as_os_str()),
            "td-kexec",
        )?;
        Err(io::Error::other(
            "td-kexec returned without booting the verified deployment",
        ))
    })();
    best_effort_unmount(mountpoint);
    result
}

fn run_install(device: &Path, mountpoint: &Path, source: &Path) -> io::Result<()> {
    require_absolute(source, "deployment source")?;
    let id = run_on_writable_volume(device, mountpoint, |root| install_deployment(root, source))?;
    writeln!(io::stdout(), "{id}")
}

fn run_rollback(device: &Path, mountpoint: &Path) -> io::Result<()> {
    let id = run_on_writable_volume(device, mountpoint, rollback_deployment)?;
    writeln!(io::stdout(), "{id}")
}

fn run() -> io::Result<()> {
    match parse_args(std::env::args_os().skip(1))? {
        Mode::Verify { root } => run_verify(&root),
        Mode::RootLoop {
            root,
            deployment_id,
            loop_device,
        } => run_root_loop(&root, &deployment_id, &loop_device),
        Mode::Boot {
            device,
            mountpoint,
            cmdline,
        } => run_boot(&device, &mountpoint, &cmdline),
        Mode::Install {
            device,
            mountpoint,
            source,
        } => run_install(&device, &mountpoint, &source),
        Mode::Rollback { device, mountpoint } => run_rollback(&device, &mountpoint),
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let _ = writeln!(io::stderr(), "td-boot: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::mpsc;
    use std::time::Duration;

    static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

    struct Fixture {
        root: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let sequence = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir()
                .join(format!("td-boot-test-{}-{sequence}", std::process::id()));
            fs::create_dir_all(root.join("td/boot")).unwrap();
            fs::create_dir_all(root.join("td/deployments")).unwrap();
            Fixture { root }
        }

        fn valid_deployment(&self) -> String {
            let kernel = b"kernel-payload\n";
            let initramfs = b"initramfs-payload\n";
            let root = b"root-payload\n";
            let manifest = format!(
                "td-deployment-v1\n{}  bzImage\n{}  initramfs.cpio\n{}  root.erofs\n",
                sha256::hex_digest(kernel),
                sha256::hex_digest(initramfs),
                sha256::hex_digest(root)
            );
            let id = sha256::hex_digest(manifest.as_bytes());
            let directory = self.root.join("td/deployments").join(&id);
            fs::create_dir(&directory).unwrap();
            fs::write(directory.join("bzImage"), kernel).unwrap();
            fs::write(directory.join("initramfs.cpio"), initramfs).unwrap();
            fs::write(directory.join("root.erofs"), root).unwrap();
            fs::write(directory.join("manifest"), manifest).unwrap();
            id
        }

        fn source_bundle(&self, name: &str, tag: &str) -> (PathBuf, String) {
            let directory = self.root.join(name);
            fs::create_dir(&directory).unwrap();
            let kernel = format!("kernel-{tag}\n");
            let initramfs = format!("initramfs-{tag}\n");
            let root = format!("root-{tag}\n");
            let manifest = format!(
                "td-deployment-v1\n{}  bzImage\n{}  initramfs.cpio\n{}  root.erofs\n",
                sha256::hex_digest(kernel.as_bytes()),
                sha256::hex_digest(initramfs.as_bytes()),
                sha256::hex_digest(root.as_bytes())
            );
            let id = sha256::hex_digest(manifest.as_bytes());
            fs::write(directory.join("bzImage"), kernel).unwrap();
            fs::write(directory.join("initramfs.cpio"), initramfs).unwrap();
            fs::write(directory.join("root.erofs"), root).unwrap();
            fs::write(directory.join("manifest"), manifest).unwrap();
            (directory, id)
        }

        fn selector(&self, slot: &str, id: &str) {
            symlink(
                format!("../deployments/{id}"),
                self.root.join("td/boot").join(slot),
            )
            .unwrap();
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn args(values: &[&str]) -> std::vec::IntoIter<OsString> {
        values
            .iter()
            .map(|value| OsString::from(*value))
            .collect::<Vec<_>>()
            .into_iter()
    }

    #[test]
    fn parse_requires_exact_modes_and_arity() {
        assert!(matches!(
            parse_args(args(&["verify", "/volume"])),
            Ok(Mode::Verify { .. })
        ));
        assert!(matches!(
            parse_args(args(&["boot", "/dev/vda", "/volume", "quiet"])),
            Ok(Mode::Boot { .. })
        ));
        assert!(matches!(
            parse_args(args(&[
                "root-loop",
                "/volume",
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "/dev/loop0",
            ])),
            Ok(Mode::RootLoop { .. })
        ));
        assert!(matches!(
            parse_args(args(&[
                "install",
                "/dev/vda",
                "/run/td-update",
                "/incoming/deployment"
            ])),
            Ok(Mode::Install { .. })
        ));
        assert!(matches!(
            parse_args(args(&["rollback", "/dev/vda", "/run/td-update"])),
            Ok(Mode::Rollback { .. })
        ));
        assert!(parse_args(args(&["verify"])).is_err());
        assert!(parse_args(args(&[
            "root-loop",
            "/volume",
            "not-a-digest",
            "/dev/loop0"
        ]))
        .is_err());
        assert!(parse_args(args(&["boot", "/dev/vda", "/volume"])).is_err());
        assert!(parse_args(args(&["install", "/dev/vda", "/volume"])).is_err());
        assert!(parse_args(args(&["rollback", "/volume"])).is_err());
        assert!(parse_args(args(&["rollback", "/dev/vda", "/volume", "extra"])).is_err());
        assert!(parse_args(args(&["unknown", "/volume"])).is_err());
    }

    #[test]
    fn update_locks_serialize_different_devices_before_mounting() {
        let fixture = Fixture::new();
        let directory = fixture.root.join("locks");
        fs::create_dir(&directory).unwrap();
        let first = acquire_update_locks_at(&directory, 1).unwrap();
        let contender_directory = directory.clone();
        let (started_tx, started_rx) = mpsc::channel();
        let (acquired_tx, acquired_rx) = mpsc::channel();
        let contender = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            let result = acquire_update_locks_at(&contender_directory, 2);
            let acquired = result.is_ok();
            let _held_locks = result.ok();
            acquired_tx.send(acquired).unwrap();
        });

        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(acquired_rx.recv_timeout(Duration::from_millis(50)).is_err());
        drop(first);
        assert!(acquired_rx.recv_timeout(Duration::from_secs(1)).unwrap());
        contender.join().unwrap();
    }

    #[test]
    fn install_publishes_then_moves_previous_before_current() {
        let fixture = Fixture::new();
        let initial = fixture.valid_deployment();
        fixture.selector("current", &initial);
        fixture.selector("previous", &initial);
        let (source, candidate) = fixture.source_bundle("candidate", "next");

        assert_eq!(
            install_deployment(&fixture.root, &source).unwrap(),
            candidate
        );
        assert_eq!(read_selector(&fixture.root, "previous").unwrap(), initial);
        assert_eq!(read_selector(&fixture.root, "current").unwrap(), candidate);
        assert_eq!(verify_slot(&fixture.root, "current").unwrap().id, candidate);
        let entries = fs::read_dir(fixture.root.join("td/deployments"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        assert!(entries
            .iter()
            .all(|name| !name.as_bytes().starts_with(b".install-")));
    }

    #[test]
    fn repeated_install_preserves_the_rollback_target() {
        let fixture = Fixture::new();
        let initial = fixture.valid_deployment();
        fixture.selector("current", &initial);
        fixture.selector("previous", &initial);
        let (source, candidate) = fixture.source_bundle("candidate", "next");

        install_deployment(&fixture.root, &source).unwrap();
        install_deployment(&fixture.root, &source).unwrap();

        assert_eq!(read_selector(&fixture.root, "current").unwrap(), candidate);
        assert_eq!(read_selector(&fixture.root, "previous").unwrap(), initial);
    }

    #[test]
    fn repeated_install_repairs_a_broken_previous_selector() {
        let fixture = Fixture::new();
        let initial = fixture.valid_deployment();
        fixture.selector("current", &initial);
        fixture.selector("previous", &initial);
        let (source, candidate) = fixture.source_bundle("candidate", "next");

        install_deployment(&fixture.root, &source).unwrap();
        fs::remove_dir_all(fixture.root.join("td/deployments").join(initial)).unwrap();
        install_deployment(&fixture.root, &source).unwrap();

        assert_eq!(read_selector(&fixture.root, "current").unwrap(), candidate);
        assert_eq!(
            read_selector(&fixture.root, "previous").unwrap(),
            candidate
        );
    }

    #[test]
    fn install_rejects_a_self_consistent_bundle_under_the_wrong_id() {
        let fixture = Fixture::new();
        let initial = fixture.valid_deployment();
        fixture.selector("current", &initial);
        fixture.selector("previous", &initial);
        let (source, candidate) = fixture.source_bundle("candidate", "next");
        let (wrong_source, _) = fixture.source_bundle("wrong", "wrong");
        fs::rename(
            wrong_source,
            fixture.root.join("td/deployments").join(&candidate),
        )
        .unwrap();

        let error = install_deployment(&fixture.root, &source).unwrap_err();

        assert!(error.to_string().contains("does not verify as"));
        assert_eq!(read_selector(&fixture.root, "current").unwrap(), initial);
        assert_eq!(read_selector(&fixture.root, "previous").unwrap(), initial);
    }

    #[test]
    fn install_reaps_reserved_temporaries_under_the_transaction_lock() {
        let fixture = Fixture::new();
        let initial = fixture.valid_deployment();
        fixture.selector("current", &initial);
        fixture.selector("previous", &initial);
        let (source, candidate) = fixture.source_bundle("candidate", "next");
        let pid = std::process::id();
        let staging = fixture
            .root
            .join("td/deployments")
            .join(format!(".install-{candidate}-{pid}-0"));
        fs::create_dir(&staging).unwrap();
        fs::write(staging.join("root.erofs"), b"partial").unwrap();
        let malformed_staging = fixture
            .root
            .join("td/deployments")
            .join(format!(".install-{candidate}-{pid}-1"));
        fs::write(&malformed_staging, b"partial").unwrap();
        let selector_temporary = fixture.root.join(format!("td/boot/.current-{pid}-0"));
        symlink(format!("../deployments/{initial}"), &selector_temporary).unwrap();
        let malformed_selector = fixture.root.join(format!("td/boot/.previous-{pid}-0"));
        fs::create_dir(&malformed_selector).unwrap();

        install_deployment(&fixture.root, &source).unwrap();

        assert!(!staging.exists());
        assert!(!malformed_staging.exists());
        assert!(!selector_temporary.exists());
        assert!(!malformed_selector.exists());
        assert_eq!(read_selector(&fixture.root, "current").unwrap(), candidate);
        assert_eq!(read_selector(&fixture.root, "previous").unwrap(), initial);
    }

    #[test]
    fn corrupt_install_is_fail_closed_and_keeps_selectors() {
        let fixture = Fixture::new();
        let initial = fixture.valid_deployment();
        fixture.selector("current", &initial);
        fixture.selector("previous", &initial);
        let (source, candidate) = fixture.source_bundle("candidate", "corrupt");
        fs::write(source.join("root.erofs"), b"tampered\n").unwrap();

        let error = install_deployment(&fixture.root, &source).unwrap_err();
        assert!(error.to_string().contains("root.erofs hash mismatch"));
        assert_eq!(read_selector(&fixture.root, "current").unwrap(), initial);
        assert_eq!(read_selector(&fixture.root, "previous").unwrap(), initial);
        assert!(!fixture.root.join("td/deployments").join(candidate).exists());
    }

    #[test]
    fn install_refuses_activation_without_a_verified_existing_deployment() {
        let fixture = Fixture::new();
        let initial = fixture.valid_deployment();
        fixture.selector("current", &initial);
        fixture.selector("previous", &initial);
        fs::write(
            fixture
                .root
                .join("td/deployments")
                .join(&initial)
                .join("root.erofs"),
            b"tampered\n",
        )
        .unwrap();
        let (source, candidate) = fixture.source_bundle("candidate", "recovery");

        let error = install_deployment(&fixture.root, &source).unwrap_err();

        assert!(error
            .to_string()
            .contains("refusing activation without a verified existing deployment"));
        assert_eq!(read_selector(&fixture.root, "current").unwrap(), initial);
        assert_eq!(read_selector(&fixture.root, "previous").unwrap(), initial);
        assert!(!fixture.root.join("td/deployments").join(candidate).exists());
    }

    #[test]
    fn rollback_selects_previous_and_retains_displaced_deployment() {
        let fixture = Fixture::new();
        let initial = fixture.valid_deployment();
        fixture.selector("current", &initial);
        fixture.selector("previous", &initial);
        let (source, candidate) = fixture.source_bundle("candidate", "next");
        install_deployment(&fixture.root, &source).unwrap();

        assert_eq!(rollback_deployment(&fixture.root).unwrap(), initial);
        assert_eq!(read_selector(&fixture.root, "current").unwrap(), initial);
        assert_eq!(read_selector(&fixture.root, "previous").unwrap(), initial);
        assert!(fixture.root.join("td/deployments").join(candidate).is_dir());
    }

    #[test]
    fn repeated_rollback_remains_on_verified_previous() {
        let fixture = Fixture::new();
        let initial = fixture.valid_deployment();
        fixture.selector("current", &initial);
        fixture.selector("previous", &initial);
        let (source, candidate) = fixture.source_bundle("candidate", "next");
        install_deployment(&fixture.root, &source).unwrap();

        rollback_deployment(&fixture.root).unwrap();
        rollback_deployment(&fixture.root).unwrap();

        assert_eq!(read_selector(&fixture.root, "current").unwrap(), initial);
        assert_eq!(read_selector(&fixture.root, "previous").unwrap(), initial);
        assert!(fixture.root.join("td/deployments").join(candidate).is_dir());
    }

    #[test]
    fn failed_current_replace_leaves_verified_previous_bootable() {
        let fixture = Fixture::new();
        let initial = fixture.valid_deployment();
        fixture.selector("previous", &initial);
        fs::create_dir(fixture.root.join("td/boot/current")).unwrap();
        let (source, candidate) = fixture.source_bundle("candidate", "next");

        let error = install_deployment(&fixture.root, &source).unwrap_err();
        assert!(error.to_string().contains("replace current selector"));
        assert_eq!(read_selector(&fixture.root, "previous").unwrap(), initial);
        let selected = select_deployment(&fixture.root).unwrap();
        assert_eq!(selected.slot, "previous");
        assert_eq!(selected.deployment.id, initial);
        assert!(fixture.root.join("td/deployments").join(candidate).is_dir());
    }

    #[test]
    fn rollback_repairs_a_corrupt_current_to_verified_previous() {
        let fixture = Fixture::new();
        let previous = fixture.valid_deployment();
        fixture.selector("previous", &previous);
        let (source, current) = fixture.source_bundle("candidate", "current");
        let installed = fixture.root.join("td/deployments").join(&current);
        fs::rename(&source, &installed).unwrap();
        fixture.selector("current", &current);
        fs::write(installed.join("root.erofs"), b"tampered\n").unwrap();

        assert_eq!(rollback_deployment(&fixture.root).unwrap(), previous);
        assert_eq!(read_selector(&fixture.root, "current").unwrap(), previous);
        assert_eq!(read_selector(&fixture.root, "previous").unwrap(), previous);
    }

    #[test]
    fn rollback_rejects_a_corrupt_previous_without_moving_current() {
        let fixture = Fixture::new();
        let current = fixture.valid_deployment();
        fixture.selector("current", &current);
        let (source, previous) = fixture.source_bundle("previous", "previous");
        let installed = fixture.root.join("td/deployments").join(&previous);
        fs::rename(source, &installed).unwrap();
        fixture.selector("previous", &previous);
        fs::write(installed.join("root.erofs"), b"tampered\n").unwrap();

        let error = rollback_deployment(&fixture.root).unwrap_err();

        assert!(error.to_string().contains("root.erofs hash mismatch"));
        assert_eq!(read_selector(&fixture.root, "current").unwrap(), current);
        assert_eq!(read_selector(&fixture.root, "previous").unwrap(), previous);
    }

    #[test]
    fn first_install_bootstraps_both_selectors() {
        let fixture = Fixture::new();
        let (source, candidate) = fixture.source_bundle("candidate", "first");

        assert_eq!(
            install_deployment(&fixture.root, &source).unwrap(),
            candidate
        );
        assert_eq!(read_selector(&fixture.root, "current").unwrap(), candidate);
        assert_eq!(read_selector(&fixture.root, "previous").unwrap(), candidate);
    }

    #[test]
    fn current_is_selected_when_every_payload_verifies() {
        let fixture = Fixture::new();
        let id = fixture.valid_deployment();
        fixture.selector("current", &id);
        let selected = select_deployment(&fixture.root).unwrap();
        assert_eq!(selected.slot, "current");
        assert_eq!(selected.deployment.id, id);
        assert!(selected.current_error.is_none());
    }

    #[test]
    fn invalid_current_falls_back_to_verified_previous() {
        let fixture = Fixture::new();
        let previous = fixture.valid_deployment();
        let invalid = "0".repeat(64);
        fs::create_dir(fixture.root.join("td/deployments").join(&invalid)).unwrap();
        fs::write(
            fixture
                .root
                .join("td/deployments")
                .join(&invalid)
                .join("manifest"),
            b"td-deployment-v1\n",
        )
        .unwrap();
        fixture.selector("current", &invalid);
        fixture.selector("previous", &previous);

        let selected = select_deployment(&fixture.root).unwrap();
        assert_eq!(selected.slot, "previous");
        assert_eq!(selected.deployment.id, previous);
        assert!(selected.current_error.is_some());
    }

    #[test]
    fn corrupt_payload_rejects_the_deployment() {
        let fixture = Fixture::new();
        let id = fixture.valid_deployment();
        fixture.selector("current", &id);
        fixture.selector("previous", &id);
        fs::write(
            fixture
                .root
                .join("td/deployments")
                .join(&id)
                .join("root.erofs"),
            b"tampered\n",
        )
        .unwrap();
        let error = select_deployment(&fixture.root).err().unwrap();
        assert!(error.to_string().contains("root.erofs hash mismatch"));
    }

    #[test]
    fn exact_root_verification_is_bound_to_the_manifest_id() {
        let fixture = Fixture::new();
        let id = fixture.valid_deployment();
        let root = verify_root_payload(&fixture.root, &id).unwrap();
        assert_eq!(
            root.metadata().unwrap().len(),
            b"root-payload\n".len() as u64
        );
        assert!(verify_root_payload(&fixture.root, &"0".repeat(64)).is_err());

        fs::write(
            fixture
                .root
                .join("td/deployments")
                .join(&id)
                .join("root.erofs"),
            b"tampered\n",
        )
        .unwrap();
        let error = verify_root_payload(&fixture.root, &id).err().unwrap();
        assert!(error.to_string().contains("root.erofs hash mismatch"));
    }

    #[test]
    fn selector_cannot_escape_the_deployments_directory() {
        let fixture = Fixture::new();
        let id = "a".repeat(64);
        symlink(
            format!("../../elsewhere/{id}"),
            fixture.root.join("td/boot/current"),
        )
        .unwrap();
        assert!(read_selector(&fixture.root, "current").is_err());
    }

    #[test]
    fn manifest_requires_exact_labels_order_and_newline() {
        let digest = "a".repeat(64);
        let reordered = format!(
            "td-deployment-v1\n{digest}  initramfs.cpio\n{digest}  bzImage\n{digest}  root.erofs\n"
        );
        assert!(parse_manifest(reordered.as_bytes()).is_err());
        let no_newline = format!(
            "td-deployment-v1\n{digest}  bzImage\n{digest}  initramfs.cpio\n{digest}  root.erofs"
        );
        assert!(parse_manifest(no_newline.as_bytes()).is_err());
    }

    #[test]
    fn cmdline_appends_one_selector_and_rejects_ambiguity() {
        let id = "b".repeat(64);
        assert_eq!(
            kernel_cmdline(OsStr::new("console=ttyS0 quiet"), &id).unwrap(),
            OsString::from(format!("console=ttyS0 quiet td.deployment={id}"))
        );
        assert!(kernel_cmdline(OsStr::new("td.deployment=old"), &id).is_err());
        assert!(kernel_cmdline(OsStr::new("\"td.deployment=old\""), &id).is_err());
        assert!(kernel_cmdline(OsStr::new("foo=\"unterminated"), &id).is_err());
        assert!(kernel_cmdline(OsStr::new("foo\\ bar"), &id).is_err());
        assert!(kernel_cmdline(OsStr::from_bytes(b"quiet\nbad"), &id).is_err());
    }

    #[test]
    fn cmdline_limit_reserves_the_kernel_nul() {
        let id = "c".repeat(64);
        let token_len = format!("td.deployment={id}").len();
        let accepted = "x".repeat(MAX_CMDLINE_BYTES - token_len - 2);
        let output = kernel_cmdline(OsStr::new(&accepted), &id).unwrap();
        assert_eq!(output.as_bytes().len() + 1, MAX_CMDLINE_BYTES);

        let rejected = "x".repeat(accepted.len() + 1);
        assert!(kernel_cmdline(OsStr::new(&rejected), &id).is_err());
    }

    #[test]
    fn stale_mount_source_requires_one_top_level_btrfs_mount() {
        let mountpoint = Path::new("/run/td update");
        let line = b"36 25 0:35 / /run/td\\040update rw,nodev,nosuid,noexec - btrfs /dev/td\\040volume rw\n";
        assert_eq!(
            mounted_btrfs_source(line, mountpoint)
                .unwrap()
                .unwrap()
                .as_bytes(),
            b"/dev/td volume"
        );

        let wrong_root =
            b"36 25 0:35 /@var /run/td\\040update rw,nodev,nosuid,noexec - btrfs /dev/vda rw\n";
        assert!(mounted_btrfs_source(wrong_root, mountpoint).is_err());
        let read_only =
            b"36 25 0:35 / /run/td\\040update ro,nodev,nosuid,noexec - btrfs /dev/vda ro\n";
        assert!(mounted_btrfs_source(read_only, mountpoint).is_err());
        let stacked = b"36 25 0:35 / /run/td\\040update rw,nodev,nosuid,noexec - btrfs /dev/vda rw\n\
37 25 0:36 / /run/td\\040update rw,nodev,nosuid,noexec - btrfs /dev/vda rw\n";
        assert!(mounted_btrfs_source(stacked, mountpoint).is_err());
        assert!(decode_mountinfo_field(b"/run/bad\\09x").is_err());
    }

    #[test]
    fn boot_commands_pin_mount_options_and_fd_handoff() {
        let mount = mount_command(Path::new("/dev/vda"), Path::new("/run/td-volume"));
        assert_eq!(mount.get_program(), OsStr::new(BUSYBOX));
        assert_eq!(
            mount.get_args().collect::<Vec<_>>(),
            vec![
                OsStr::new("mount"),
                OsStr::new("-t"),
                OsStr::new("btrfs"),
                OsStr::new("-o"),
                OsStr::new("ro,nodev,nosuid,noexec"),
                OsStr::new("/dev/vda"),
                OsStr::new("/run/td-volume"),
            ]
        );
        let mount = writable_mount_command(Path::new("/dev/vda"), Path::new("/run/td-update"));
        assert_eq!(
            mount.get_args().collect::<Vec<_>>(),
            vec![
                OsStr::new("mount"),
                OsStr::new("-t"),
                OsStr::new("btrfs"),
                OsStr::new("-o"),
                OsStr::new("rw,nodev,nosuid,noexec"),
                OsStr::new("/dev/vda"),
                OsStr::new("/run/td-update"),
            ]
        );
        let unmount = unmount_command(Path::new("/run/td-update"));
        assert_eq!(
            unmount.get_args().collect::<Vec<_>>(),
            vec![OsStr::new("umount"), OsStr::new("/run/td-update")]
        );

        let fixture = Fixture::new();
        let id = fixture.valid_deployment();
        fixture.selector("current", &id);
        let deployment = select_deployment(&fixture.root).unwrap().deployment;
        let Deployment {
            kernel, initramfs, ..
        } = deployment;
        let command = kexec_command(kernel, initramfs, OsStr::new("quiet td.deployment=test"));
        assert_eq!(command.get_program(), OsStr::new(TD_KEXEC));
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            vec![OsStr::new("--fds"), OsStr::new("quiet td.deployment=test")]
        );

        let root = verify_root_payload(&fixture.root, &id).unwrap();
        let command = loop_command(root, Path::new("/dev/loop0"));
        assert_eq!(command.get_program(), OsStr::new(BUSYBOX));
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            vec![
                OsStr::new("losetup"),
                OsStr::new("-r"),
                OsStr::new("/dev/loop0"),
                OsStr::new(STDIN_PATH),
            ]
        );
    }
}

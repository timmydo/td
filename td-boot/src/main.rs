//! td-boot verifies a deployment on the persistent volume, preferring
//! `current` and falling back to `previous`, then invokes the confined kexec
//! helper. Hashes detect corruption; they do not authenticate a deployment.
#![forbid(unsafe_code)]

#[path = "../../engine/src/sha256.rs"]
#[allow(dead_code)]
mod sha256;

use std::ffi::{OsStr, OsString};
use std::fs::{self, File, Metadata};
use std::io::{self, Read, Seek, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

const BUSYBOX: &str = "/bin/busybox";
const TD_KEXEC: &str = "/bin/td-kexec";
const MANIFEST_HEADER: &[u8] = b"td-deployment-v1";
const MANIFEST_NAME: &str = "manifest";
const MAX_MANIFEST_BYTES: u64 = 4096;
const MAX_CMDLINE_BYTES: usize = 2048;

enum Mode {
    Verify {
        root: PathBuf,
    },
    Boot {
        device: PathBuf,
        mountpoint: PathBuf,
        cmdline: OsString,
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

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn usage_error() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "usage: td-boot verify <volume-root>\n       td-boot boot <device> <mountpoint> <cmdline>",
    )
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
    let selector = root.join("td").join("boot").join(slot);
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
    let prefix = b"../deployments/";
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

fn verify_slot(root: &Path, slot: &str) -> io::Result<Deployment> {
    let id = read_selector(root, slot)?;
    let directory = root.join("td").join("deployments").join(&id);
    require_real_directory(&directory, "deployment")?;

    let manifest_path = directory.join(MANIFEST_NAME);
    let manifest_bytes =
        read_bounded_real_file(&manifest_path, "deployment manifest", MAX_MANIFEST_BYTES)?;
    let manifest_id = sha256::hex_digest(&manifest_bytes);
    if manifest_id != id {
        return Err(invalid(format!(
            "deployment id {id} does not match manifest hash {manifest_id}"
        )));
    }
    let manifest = parse_manifest(&manifest_bytes)?;
    let kernel = verify_payload(&directory, "bzImage", &manifest.kernel)?;
    let initramfs = verify_payload(&directory, "initramfs.cpio", &manifest.initramfs)?;
    verify_payload(&directory, "root.erofs", &manifest.root)?;

    Ok(Deployment {
        id,
        kernel,
        initramfs,
    })
}

fn select_deployment(root: &Path) -> io::Result<Selection> {
    require_absolute(root, "volume root")?;
    require_real_directory(root, "volume root")?;
    require_real_directory(&root.join("td"), "td directory")?;
    require_real_directory(&root.join("td").join("boot"), "boot selector directory")?;
    require_real_directory(
        &root.join("td").join("deployments"),
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

fn mount_command(device: &Path, mountpoint: &Path) -> Command {
    let mut command = Command::new(BUSYBOX);
    command.args([
        OsStr::new("mount"),
        OsStr::new("-t"),
        OsStr::new("btrfs"),
        OsStr::new("-o"),
        OsStr::new("ro,nodev,nosuid,noexec"),
        device.as_os_str(),
        mountpoint.as_os_str(),
    ]);
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

fn report_fallback(selection: &Selection) {
    if let Some(error) = &selection.current_error {
        let _ = writeln!(
            io::stderr(),
            "td-boot: current rejected ({error}); using previous {}",
            selection.deployment.id
        );
    }
}

fn run_verify(root: &Path) -> io::Result<()> {
    let selection = select_deployment(root)?;
    report_fallback(&selection);
    writeln!(
        io::stdout(),
        "{} {}",
        selection.slot,
        selection.deployment.id
    )
}

fn best_effort_unmount(mountpoint: &Path) {
    let _ = Command::new(BUSYBOX).arg("umount").arg(mountpoint).status();
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
        report_fallback(&selection);
        let cmdline = kernel_cmdline(base_cmdline, &selection.deployment.id)?;
        let Deployment {
            kernel,
            initramfs,
            ..
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

fn run() -> io::Result<()> {
    match parse_args(std::env::args_os().skip(1))? {
        Mode::Verify { root } => run_verify(&root),
        Mode::Boot {
            device,
            mountpoint,
            cmdline,
        } => run_boot(&device, &mountpoint, &cmdline),
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
        assert!(parse_args(args(&["verify"])).is_err());
        assert!(parse_args(args(&["boot", "/dev/vda", "/volume"])).is_err());
        assert!(parse_args(args(&["unknown", "/volume"])).is_err());
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

        let fixture = Fixture::new();
        let id = fixture.valid_deployment();
        fixture.selector("current", &id);
        let deployment = select_deployment(&fixture.root).unwrap().deployment;
        let Deployment {
            kernel,
            initramfs,
            ..
        } = deployment;
        let command = kexec_command(kernel, initramfs, OsStr::new("quiet td.deployment=test"));
        assert_eq!(command.get_program(), OsStr::new(TD_KEXEC));
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            vec![OsStr::new("--fds"), OsStr::new("quiet td.deployment=test")]
        );
    }
}

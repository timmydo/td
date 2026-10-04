//! td-install lays out a td disk: a protective-MBR GPT carrying a FAT32 EFI
//! System Partition and the td volume td-boot selects from.
//!
//! One code path, two destinations — a block device or a regular file (D9).
//! The file case is not a convenience: it is what makes the installer testable
//! headlessly, and an installer whose tested path differs from its shipped one
//! is an installer nobody has tested. Neither the size nor the sector size is
//! assumed from which of the two it is; both are asked of the destination.
//!
//! `td-install/DESIGN.md` is the normative specification for this path.
//! `loop_sys.rs` holds its one `unsafe` surface (UNSAFE.md §21), which the
//! `confinement` tests below pin.
#![deny(unsafe_code)]

#[path = "../../td-boot/src/protocol.rs"]
#[allow(dead_code)]
mod protocol;
// The real-regular-bounded file rule, td-boot's and now shared rather than
// reimplemented here — DESIGN §10 item 10b. A rule spelled in both crates is
// one they can come to disagree about, and this one did, three ways, on the
// day the second copy was written.
#[path = "../../td-boot/src/realfile.rs"]
#[allow(dead_code)]
mod realfile;
// `gpt.rs` reaches its checksum as `crate::crc32`, the spelling that resolves
// identically inside the engine lib and here, so the two are declared as a PAIR.
#[path = "../../engine/src/cpio.rs"]
mod cpio;
#[path = "../../engine/src/crc32.rs"]
#[allow(dead_code)]
mod crc32;
#[path = "../../engine/src/fat.rs"]
#[allow(dead_code)]
mod fat;
#[path = "../../engine/src/gpt.rs"]
#[allow(dead_code)]
mod gpt;
// A live installation checks the kernel it copies to the ESP against the
// manifest its plan names (DESIGN.md "Executing a consented installation").
// Only the hasher and hex encoder are used here; `sha256_file` serves its other
// consumers.
#[path = "../../engine/src/sha256.rs"]
#[allow(dead_code)]
mod sha256;
// Test-only, and declared with the same redundant `#[path]` as td-boot's own
// files, so that `every_compiled_file_is_one_the_guards_read` counts it and
// both guards read it: a file compiled only into the test binary ships in
// nothing, but the audit counts sources, not shipped ones.
#[cfg(test)]
#[path = "scratch.rs"]
mod scratch;

#[path = "inventory.rs"]
mod inventory;

#[path = "installation_plan.rs"]
#[allow(dead_code)]
mod installation_plan;

#[path = "installation_protocol.rs"]
#[allow(dead_code)]
mod installation_protocol;

#[path = "installation_service.rs"]
mod installation_service;

// The service speaks one direction; td-authd's half is unused here.
#[path = "installation_consent.rs"]
#[allow(dead_code)]
mod installation_consent;

#[path = "timezones.rs"]
mod timezones;

#[path = "../../td-firstboot/src/hostname.rs"]
mod hostname;

// The one raw-syscall layer, and the one module that calls it.
#[path = "loop_sys.rs"]
mod loop_sys;

#[path = "loop_device.rs"]
mod loop_device;

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, IsTerminal, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

/// The sector size assumed for a regular file, and the smallest a disk may
/// report. A 4Kn device says so itself — see `logical_sector_size`.
const FILE_SECTOR_BYTES: u64 = 512;

fn invalid(message: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

const USAGE: &str =
    "usage: td-install new-volume-uuid\n       td-install inventory\n       td-install destinations\n       td-install candidate-record\n       td-install observe-plan < plan.bin\n       td-install observe-source-plan <td-boot> <deployment-directory> <trusted-key> < plan.bin\n       td-install serve <td-boot> <deployment-directory> <trusted-key> <verified-root> <td-firstboot> (stdin: connected Unix stream socket)\n       td-install prepare-selector <template> <trusted-key> <volume-uuid> <output>\n       td-install timezones\n       td-install layout-preview <logical-sector-bytes> <capacity-bytes>\n       td-install format <efi-kernel> <selector-initramfs> <volume-options-and-operands>\n       td-install layout <destination> [<efi-kernel> <selector-initramfs>]\n       \
                     td-install volume [--uuid <uuid>] [--timezone <IANA-id>] [--hostname <name>] [--username <name> <verified-root> <td-firstboot>] <destination> <mkfs.btrfs> <scratch-dir> \
                     [<td-boot> <deployment> <trusted-key> | --trusted-key <trusted-key>]\n       \
                     td-install format ... --trusted-key <trusted-key> --publish <td-boot> <deployment> <mountpoint>";

fn candidate_output_allowed(terminal: bool) -> io::Result<()> {
    if terminal {
        Err(invalid(
            "candidate-record is binary; redirect stdout to a pipe or file".into(),
        ))
    } else {
        Ok(())
    }
}

#[derive(Debug, Eq, PartialEq)]
enum Mode {
    NewVolumeUuid,
    Inventory,
    Destinations,
    CandidateRecord,
    ObservePlan,
    ObserveSourcePlan {
        td_boot: PathBuf,
        source: PathBuf,
        trusted_key: PathBuf,
    },
    /// The installation service core, for one installer on the stdin socket.
    Serve(LiveHost),
    PrepareSelector {
        template: PathBuf,
        trusted_key: PathBuf,
        uuid: VolumeUuid,
        output: PathBuf,
    },
    Timezones,
    LayoutPreview {
        sector_bytes: u64,
        capacity_bytes: u64,
    },
    Layout {
        destination: PathBuf,
        boot: Option<BootFiles>,
    },
    /// `mkfs` is passed rather than looked up: this crate execs exactly what it
    /// is told to and never resolves a program through an ambient `PATH`, which
    /// is what makes the one third-party program on the install path (D7) a
    /// declared input of whoever calls it. `scratch` is the caller's too — the
    /// image needs room the size of the volume's real contents, and only the
    /// caller knows where there is any. With a publish that is the deployment
    /// TWICE over: td-boot copies the bundle into the staging tree and mkfs
    /// then copies the tree into the image, so a scratch sized from the volume
    /// alone runs out inside mkfs rather than here.
    Volume {
        /// Present for the combined format command; volume alone keeps GPT.
        boot: Option<BootFiles>,
        uuid: Option<VolumeUuid>,
        timezone: Option<String>,
        hostname: Option<hostname::Hostname>,
        username: Option<Box<PrimarySelection>>,
        destination: PathBuf,
        mkfs: PathBuf,
        scratch: PathBuf,
        /// Publish requires all three operands and authenticated publication.
        /// Trust initializes only the key/layout and claims no deployment.
        seed: Option<VolumeSeed>,
    },
}

/// The caller binds this read-only validator to an authenticated deployment.
#[derive(Debug, Eq, PartialEq)]
struct PrimarySelection {
    name: String,
    root: PathBuf,
    firstboot: PathBuf,
}

impl PrimarySelection {
    // Keep this cheap CLI grammar aligned with td-authd/src/primary_account.rs;
    // the bound firstboot validator still owns complete account admission.
    fn syntax(name: &str) -> io::Result<()> {
        if name.len() > 32
            || !name.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
            || !name.bytes().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"_-".contains(&byte)
            })
        {
            return Err(invalid("username requires 1-32 lowercase ASCII letters, digits, underscores or hyphens, starting with a letter".into()));
        }
        Ok(())
    }

    fn check(&self) -> io::Result<()> {
        Self::syntax(&self.name)?;
        let status = std::process::Command::new(&self.firstboot)
            .arg("check-primary-name")
            .arg(&self.root)
            .arg(&self.name)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .status()
            .map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!("validate selected primary account: {error}"),
                )
            })?;
        if !status.success() {
            return Err(invalid(format!(
                "selected primary account validation failed: {status}"
            )));
        }
        Ok(())
    }
}

/// A preselected identity shared with the selector before the ESP is written.
#[derive(Debug, Eq, PartialEq)]
struct VolumeUuid(String);

impl VolumeUuid {
    fn parse(text: &str) -> io::Result<Self> {
        if text.len() != 36 {
            return Err(invalid(
                "volume UUID must contain exactly 36 ASCII bytes".into(),
            ));
        }
        let guid = gpt::Guid::parse(text).map_err(invalid)?;
        if guid == gpt::Guid::ZERO || guid.to_string().to_ascii_lowercase() != text {
            return Err(invalid(
                "volume UUID must be nonzero canonical lowercase hex".into(),
            ));
        }
        Ok(Self(text.to_owned()))
    }
}

/// Fixed firmware entry, independent of the deployment selected on Btrfs.
#[derive(Debug, Eq, PartialEq)]
struct BootFiles {
    kernel: PathBuf,
    initramfs: PathBuf,
}

const MAX_BOOT_FILE: u64 = 256 * 1024 * 1024;

struct BootInput {
    path: PathBuf,
    file: File,
    len: u64,
}

impl BootInput {
    fn open(path: &Path, destination: &File) -> io::Result<Self> {
        use std::os::unix::fs::MetadataExt;
        let (file, metadata) = realfile::open_real_file(path, "EFI input")?;
        let target = destination.metadata()?;
        if metadata.dev() == target.dev() && metadata.ino() == target.ino() {
            return Err(invalid(format!(
                "EFI input is the destination: {}",
                path.display()
            )));
        }
        if metadata.len() == 0 || metadata.len() > MAX_BOOT_FILE {
            return Err(invalid(format!(
                "EFI input must contain 1..={MAX_BOOT_FILE} bytes: {}",
                path.display()
            )));
        }
        Ok(Self {
            path: path.into(),
            file,
            len: metadata.len(),
        })
    }

    fn copy_to(&mut self, destination: &mut File, destination_path: &Path) -> io::Result<()> {
        let copied =
            io::copy(&mut (&mut self.file).take(self.len), destination).map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!(
                        "copy EFI input {} to {}: {error}",
                        self.path.display(),
                        destination_path.display()
                    ),
                )
            })?;
        let mut extra = [0u8; 1];
        if copied != self.len
            || self.file.read(&mut extra).map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!("check EFI input {}: {error}", self.path.display()),
                )
            })? != 0
            || self
                .file
                .metadata()
                .map_err(|error| {
                    io::Error::new(
                        error.kind(),
                        format!("stat EFI input {}: {error}", self.path.display()),
                    )
                })?
                .len()
                != self.len
        {
            return Err(invalid(format!(
                "EFI input changed size: {}",
                self.path.display()
            )));
        }
        Ok(())
    }

    /// Require the pinned bytes to hash to `expected`, the digest of `what`,
    /// then rewind for the copy, which still checks the length: one
    /// descriptor, read twice.
    fn check_digest(&mut self, expected: &[u8; 32], what: &str) -> io::Result<()> {
        let digest = digest_range(&mut self.file, 0, self.len).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("hash EFI input {}: {error}", self.path.display()),
            )
        })?;
        self.file.seek(SeekFrom::Start(0))?;
        if digest != *expected {
            return Err(invalid(format!(
                "EFI input {} is not {what}",
                self.path.display()
            )));
        }
        Ok(())
    }
}

/// The SHA-256 of `len` bytes of `file` from `offset`.
fn digest_range(file: &mut File, offset: u64, len: u64) -> io::Result<[u8; 32]> {
    file.seek(SeekFrom::Start(offset))?;
    let mut hasher = sha256::Sha256::new();
    let mut buffer = vec![0; 64 * 1024];
    let mut remaining = len;
    while remaining > 0 {
        let want = remaining.min(buffer.len() as u64);
        let chunk = usize::try_from(want)
            .ok()
            .and_then(|want| buffer.get_mut(..want))
            .ok_or_else(|| invalid("digest buffer".into()))?;
        file.read_exact(chunk)?;
        hasher.update(chunk);
        remaining -= want;
    }
    Ok(hasher.finalize())
}

/// Admit the complete EFI input before creating the selector copy.
fn selector_join_padding(len: u64, appendix_bytes: usize) -> io::Result<usize> {
    let length =
        usize::try_from(len).map_err(|_| invalid("selector template is too large".into()))?;
    let padding = cpio::alignment_padding(length);
    let total = len
        .checked_add(padding as u64)
        .and_then(|size| size.checked_add(appendix_bytes as u64))
        .ok_or_else(|| invalid("prepared selector size overflow".into()))?;
    if len == 0 || total > MAX_BOOT_FILE {
        return Err(invalid(format!(
            "template and identity must fit in 1..={MAX_BOOT_FILE} bytes"
        )));
    }
    Ok(padding)
}

/// The pinned template and what preparing a selector appends to it.
struct SelectorParts {
    file: File,
    len: u64,
    padding: usize,
    appendix: Vec<u8>,
}

impl SelectorParts {
    /// The prepared selector's length.
    fn total(&self) -> Option<u64> {
        self.len
            .checked_add(u64::try_from(self.padding).ok()?)?
            .checked_add(u64::try_from(self.appendix.len()).ok()?)
    }
}

/// Prepare the selector copy: the template, which carries neither, then the
/// trust root and the volume identity it boots.
fn prepare_selector(
    template: &Path,
    trusted_key: &Path,
    uuid: &VolumeUuid,
    output: &Path,
) -> io::Result<()> {
    let SelectorParts {
        file,
        len,
        padding,
        appendix,
    } = selector_parts(template, trusted_key, uuid)?;
    // Existing files, links and device nodes refuse before any output write.
    let mut destination = paths::create_new_with_mode(output, 0o600)?;
    let mut source = BootInput {
        path: template.into(),
        file,
        len,
    };
    source.copy_to(&mut destination, output)?;
    let output_error = |operation: &str, error: io::Error| {
        io::Error::new(
            error.kind(),
            format!(
                "{operation} prepared selector {}: {error}",
                output.display()
            ),
        )
    };
    let zeros = [0u8; 3];
    destination
        .write_all(
            zeros
                .get(..padding)
                .ok_or_else(|| invalid("invalid selector padding".into()))?,
        )
        .map_err(|error| output_error("write padding to", error))?;
    destination
        .write_all(&appendix)
        .map_err(|error| output_error("append identity to", error))?;
    use std::os::unix::fs::PermissionsExt;
    destination
        .set_permissions(std::fs::Permissions::from_mode(0o600))
        .map_err(|error| output_error("set permissions on", error))?;
    if destination
        .metadata()
        .map_err(|error| output_error("stat", error))?
        .permissions()
        .mode()
        & 0o7777
        != 0o600
    {
        return Err(invalid(format!(
            "{}: prepared selector did not retain mode 0600",
            output.display()
        )));
    }
    destination
        .sync_all()
        .map_err(|error| output_error("sync", error))
}

fn selector_parts(
    template: &Path,
    trusted_key: &Path,
    uuid: &VolumeUuid,
) -> io::Result<SelectorParts> {
    let key = read_trusted_key(trusted_key)?;
    // A key the booted selector could not decode makes an unbootable disk,
    // found only after the destructive write, so its shape is refused here
    // by td-boot's rule: 64 hex digits once surrounding whitespace is gone.
    let digits = key.trim_ascii();
    if digits.len() != 64 || !digits.iter().all(u8::is_ascii_hexdigit) {
        return Err(invalid(format!(
            "trusted deployment key {} is not 64 hexadecimal digits",
            trusted_key.display()
        )));
    }
    let (file, metadata) = realfile::open_real_file(template, "selector template")?;
    let len = metadata.len();
    let uuid_line = format!("{}\n", uuid.0);
    let mut entries: Vec<cpio::Entry> = Vec::new();
    for (path, bytes) in [
        (protocol::TRUSTED_KEY_PATH, key.as_slice()),
        (protocol::VOLUME_UUID_PATH, uuid_line.as_bytes()),
    ] {
        // Every proper directory prefix, shallowest first and once: the
        // kernel creates nothing under a parent the archive does not name.
        let mut end = 0usize;
        while let Some(slash) = path.get(end..).and_then(|tail| tail.find('/')) {
            end = end
                .checked_add(slash)
                .ok_or_else(|| invalid("selector parent offset overflow".into()))?;
            let parent = path
                .get(..end)
                .ok_or_else(|| invalid("invalid selector parent".into()))?;
            if !entries.iter().any(|entry| entry.name == parent) {
                entries.push(cpio::Entry {
                    name: parent,
                    mode: 0o755,
                    kind: cpio::Kind::Directory,
                });
            }
            end = end
                .checked_add(1)
                .ok_or_else(|| invalid("selector parent offset overflow".into()))?;
        }
        entries.push(cpio::Entry {
            name: path,
            mode: 0o644,
            kind: cpio::Kind::File(bytes),
        });
    }
    let appendix = cpio::build(&entries).map_err(invalid)?;
    let padding = selector_join_padding(len, appendix.len())
        .map_err(|error| invalid(format!("{}: {error}", template.display())))?;
    Ok(SelectorParts {
        file,
        len,
        padding,
        appendix,
    })
}

#[derive(Debug, Eq, PartialEq)]
struct Publish {
    /// Where `td-boot` is. Passed, never resolved: this crate execs what it is
    /// told to, as it does for `mkfs.btrfs`.
    td_boot: PathBuf,
    deployment: PathBuf,
    /// Named EXPLICITLY rather than left to td-boot's probe, and DESIGN §10
    /// item 7c says why: absence is what the probe reads as "no trust root",
    /// and absence is indistinguishable from a key provisioned under the wrong
    /// name or behind a dangling symlink. Naming it means a wrong path is an
    /// error instead of an unverified publish.
    trusted_key: PathBuf,
}

/// Publication onto the volume just formatted, through a loop over the
/// destination `format` still holds (DESIGN.md "Publishing through a loop
/// over the claim"). Every path is absolute: td-boot requires its two, and a
/// bare program name would resolve through `PATH`.
#[derive(Debug, Eq, PartialEq)]
struct LoopPublish {
    td_boot: PathBuf,
    deployment: PathBuf,
    mountpoint: PathBuf,
}

/// Formatting with a trust root alone prepares for a later mounted publish;
/// `Loop` is that publish, made before the destination is released.
#[derive(Debug, Eq, PartialEq)]
enum VolumeSeed {
    Publish(Publish),
    Trust(PathBuf),
    Loop(PathBuf, LoopPublish),
}

impl VolumeSeed {
    fn trusted_key(&self) -> &Path {
        match self {
            Self::Publish(publish) => &publish.trusted_key,
            Self::Trust(key) | Self::Loop(key, _) => key,
        }
    }

    fn publish(&self) -> Option<&Publish> {
        match self {
            Self::Publish(publish) => Some(publish),
            Self::Trust(_) | Self::Loop(..) => None,
        }
    }

    fn through_loop(&self) -> Option<&LoopPublish> {
        match self {
            Self::Loop(_, publish) => Some(publish),
            Self::Publish(_) | Self::Trust(_) => None,
        }
    }
}

fn parse_args(mut args: impl Iterator<Item = OsString>) -> io::Result<Mode> {
    let verb = args.next().ok_or_else(|| invalid(USAGE.to_string()))?;
    let (verb, boot) = if verb == "format" {
        let kernel = args.next().ok_or_else(|| invalid(USAGE.into()))?;
        let initramfs = args.next().ok_or_else(|| invalid(USAGE.into()))?;
        if [&kernel, &initramfs]
            .iter()
            .any(|path| path.as_encoded_bytes().starts_with(b"-"))
        {
            return Err(invalid("format requires EFI kernel and selector paths before volume options; prefix relative paths beginning with '-' with './'".into()));
        }
        (
            OsString::from("volume"),
            Some(BootFiles {
                kernel: kernel.into(),
                initramfs: initramfs.into(),
            }),
        )
    } else {
        (verb, None)
    };
    let rest: Vec<PathBuf> = args.map(PathBuf::from).collect();
    let (uuid, rest) = if rest.first().is_some_and(|arg| arg.as_os_str() == "--uuid") {
        if verb != "volume" {
            return Err(invalid("--uuid is only supported by volume".into()));
        }
        let text = rest
            .get(1)
            .and_then(|arg| arg.to_str())
            .ok_or_else(|| invalid("--uuid requires a canonical volume UUID".into()))?;
        let uuid = VolumeUuid::parse(text)?;
        (
            Some(uuid),
            rest.get(2..).ok_or_else(|| invalid(USAGE.into()))?,
        )
    } else {
        (None, rest.as_slice())
    };
    let (timezone, rest) = if rest
        .first()
        .is_some_and(|arg| arg.as_os_str() == "--timezone")
    {
        if verb != "volume" {
            return Err(invalid("--timezone is only supported by volume".into()));
        }
        let id = rest
            .get(1)
            .and_then(|arg| arg.to_str())
            .ok_or_else(|| invalid("--timezone requires an IANA identifier".into()))?;
        (
            Some(id.to_owned()),
            rest.get(2..).ok_or_else(|| invalid(USAGE.into()))?,
        )
    } else {
        (None, rest)
    };
    let (hostname, rest) = if rest
        .first()
        .is_some_and(|arg| arg.as_os_str() == "--hostname")
    {
        if verb != "volume" {
            return Err(invalid("--hostname is only supported by volume".into()));
        }
        let name = rest
            .get(1)
            .and_then(|arg| arg.to_str())
            .ok_or_else(|| invalid("--hostname requires a canonical name".into()))?;
        (
            Some(hostname::Hostname::parse(name).map_err(invalid)?),
            rest.get(2..).ok_or_else(|| invalid(USAGE.into()))?,
        )
    } else {
        (None, rest)
    };
    let (username, rest) = if rest
        .first()
        .is_some_and(|arg| arg.as_os_str() == "--username")
    {
        if verb != "volume" {
            return Err(invalid("--username is only supported by volume".into()));
        }
        let Some([name, root, firstboot, tail @ ..]) = rest.get(1..) else {
            return Err(invalid(
                "--username requires NAME VERIFIED-ROOT TD-FIRSTBOOT".into(),
            ));
        };
        let name = name
            .to_str()
            .filter(|name| !name.is_empty() && name.len() <= 32)
            .ok_or_else(|| invalid("--username requires a UTF-8 name of 1..=32 bytes".into()))?;
        PrimarySelection::syntax(name)?;
        if !root.is_absolute() || !firstboot.is_absolute() {
            return Err(invalid(
                "--username requires absolute deployment-root and validator paths".into(),
            ));
        }
        (
            Some(Box::new(PrimarySelection {
                name: name.into(),
                root: root.clone(),
                firstboot: firstboot.clone(),
            })),
            tail,
        )
    } else {
        (None, rest)
    };
    if rest.iter().any(|arg| arg.as_os_str() == "--username") {
        return Err(invalid(
            "--username must appear once after regional settings and before volume operands".into(),
        ));
    }
    if rest.iter().any(|arg| arg.as_os_str() == "--hostname") {
        return Err(invalid(
            "--hostname must appear once after timezone and before the volume operands".into(),
        ));
    }
    if rest.iter().any(|arg| arg.as_os_str() == "--timezone") {
        return Err(invalid(
            "--timezone must appear once before --hostname and the volume operands".into(),
        ));
    }
    if rest.iter().any(|arg| arg.as_os_str() == "--uuid") {
        return Err(invalid(if verb == "volume" {
            "--uuid must appear once, before other volume options and operands".into()
        } else {
            "--uuid is only supported by volume".into()
        }));
    }
    let (through_loop, rest) = match rest.iter().position(|arg| arg.as_os_str() == "--publish") {
        None => (None, rest),
        Some(at) => {
            if boot.is_none() {
                return Err(invalid("--publish is only supported by format".into()));
            }
            let (head, tail) = rest
                .split_at_checked(at)
                .ok_or_else(|| invalid(USAGE.into()))?;
            let [_, td_boot, deployment, mountpoint] = tail else {
                return Err(invalid(
                    "--publish requires TD-BOOT DEPLOYMENT MOUNTPOINT and nothing after them"
                        .into(),
                ));
            };
            if head.len() != 5
                || !head
                    .get(3)
                    .is_some_and(|arg| arg.as_os_str() == "--trusted-key")
            {
                return Err(invalid(
                    "--publish follows the volume operands and --trusted-key KEY".into(),
                ));
            }
            if [td_boot, deployment, mountpoint]
                .iter()
                .any(|path| !path.is_absolute())
            {
                return Err(invalid("--publish requires absolute paths".into()));
            }
            (
                Some(LoopPublish {
                    td_boot: td_boot.clone(),
                    deployment: deployment.clone(),
                    mountpoint: mountpoint.clone(),
                }),
                head,
            )
        }
    };
    if rest.iter().any(|arg| arg.as_os_str() == "--trusted-key")
        && !(verb == "volume"
            && rest.len() == 5
            && rest
                .get(3)
                .is_some_and(|arg| arg.as_os_str() == "--trusted-key")
            && rest
                .iter()
                .filter(|arg| arg.as_os_str() == "--trusted-key")
                .count()
                == 1)
    {
        return Err(invalid(if verb == "volume" {
            "--trusted-key requires exactly one key after the volume operands".into()
        } else {
            "--trusted-key is only supported by volume".into()
        }));
    }
    match (verb.to_str(), rest) {
        (Some("inventory"), []) => Ok(Mode::Inventory),
        (Some("destinations"), []) => Ok(Mode::Destinations),
        (Some("candidate-record"), []) => Ok(Mode::CandidateRecord),
        (Some("observe-plan"), []) => Ok(Mode::ObservePlan),
        (Some("observe-source-plan"), [td_boot, source, trusted_key]) => {
            Ok(Mode::ObserveSourcePlan {
                td_boot: PathBuf::from(td_boot),
                source: PathBuf::from(source),
                trusted_key: PathBuf::from(trusted_key),
            })
        }
        (Some("serve"), [td_boot, source, trusted_key, root, firstboot]) => {
            let host = LiveHost {
                td_boot: td_boot.clone(),
                source: source.clone(),
                trusted_key: trusted_key.clone(),
                root: root.clone(),
                firstboot: firstboot.clone(),
                timezones: PathBuf::from(TIMEZONE_ROOT),
                catalog: None,
                booted: PathBuf::from(BOOTED_DEPLOYMENT),
            };
            if [
                &host.td_boot,
                &host.source,
                &host.trusted_key,
                &host.root,
                &host.firstboot,
            ]
            .iter()
            .any(|path| !path.is_absolute())
            {
                return Err(invalid("serve operands must be absolute paths".into()));
            }
            Ok(Mode::Serve(host))
        }
        (Some("new-volume-uuid"), []) => Ok(Mode::NewVolumeUuid),
        (Some("prepare-selector"), [template, trusted_key, uuid, output]) => {
            Ok(Mode::PrepareSelector {
                template: template.clone(),
                trusted_key: trusted_key.clone(),
                uuid: VolumeUuid::parse(
                    uuid.to_str()
                        .ok_or_else(|| invalid("volume UUID must be UTF-8".into()))?,
                )?,
                output: output.clone(),
            })
        }
        (Some("timezones"), []) => Ok(Mode::Timezones),
        (Some("layout-preview"), [sector, capacity]) => Ok(Mode::LayoutPreview {
            sector_bytes: preview_number(sector.as_os_str(), "logical sector bytes")?,
            capacity_bytes: preview_number(capacity.as_os_str(), "capacity bytes")?,
        }),
        (Some("layout"), [destination]) => Ok(Mode::Layout {
            destination: destination.clone(),
            boot: None,
        }),
        (Some("layout"), [destination, kernel, initramfs]) => Ok(Mode::Layout {
            destination: destination.clone(),
            boot: Some(BootFiles {
                kernel: kernel.clone(),
                initramfs: initramfs.clone(),
            }),
        }),
        (Some("volume"), [destination, mkfs, scratch, flag, trusted_key])
            if flag.as_os_str() == "--trusted-key" =>
        {
            Ok(Mode::Volume {
                boot,
                uuid,
                timezone,
                hostname,
                username,
                destination: destination.clone(),
                mkfs: mkfs.clone(),
                scratch: scratch.clone(),
                seed: Some(match through_loop {
                    Some(publish) => VolumeSeed::Loop(trusted_key.clone(), publish),
                    None => VolumeSeed::Trust(trusted_key.clone()),
                }),
            })
        }
        (Some("volume"), [destination, mkfs, scratch, td_boot, deployment, trusted_key]) => {
            Ok(Mode::Volume {
                boot,
                uuid,
                timezone,
                hostname,
                username,
                destination: destination.clone(),
                mkfs: mkfs.clone(),
                scratch: scratch.clone(),
                seed: Some(VolumeSeed::Publish(Publish {
                    td_boot: td_boot.clone(),
                    deployment: deployment.clone(),
                    trusted_key: trusted_key.clone(),
                })),
            })
        }
        (Some("volume"), [destination, mkfs, scratch]) => Ok(Mode::Volume {
            boot,
            uuid,
            timezone,
            hostname,
            username,
            destination: destination.clone(),
            mkfs: mkfs.clone(),
            scratch: scratch.clone(),
            seed: None,
        }),
        _ => Err(invalid(USAGE.to_string())),
    }
}

/// Decode the bounded wire representation supplied by the caller.
fn read_plan(input: &mut impl Read) -> io::Result<installation_plan::Plan> {
    let mut bytes = Vec::new();
    input
        .take(installation_plan::MAX_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > installation_plan::MAX_BYTES {
        return Err(invalid("installation plan exceeds maximum length".into()));
    }
    installation_plan::Plan::decode(&bytes).map_err(invalid)
}

/// Check only the current observation. The held claim ends when this call
/// returns, so its result cannot authorize a later format invocation.
fn observe_plan(input: &mut impl Read, output: &mut impl Write) -> io::Result<()> {
    let plan = read_plan(input)?;
    let _claim = inventory::claim_plan(&plan)?;
    writeln!(
        output,
        "{{\"version\":1,\"scope\":\"plan-observation-only\",\"destination\":\"{}\"}}",
        plan.destination().name()
    )?;
    output.flush()
}

/// Keep the reviewed disk claimed while td-boot authenticates every payload.
/// The claim ends before return; the report grants no later write authority.
fn observe_source_plan(
    input: &mut impl Read,
    output: &mut impl Write,
    td_boot: &Path,
    source: &Path,
    trusted_key: &Path,
) -> io::Result<()> {
    if !td_boot.is_absolute() || !source.is_absolute() || !trusted_key.is_absolute() {
        return Err(invalid(
            "td-boot, source and trusted key must be absolute paths".into(),
        ));
    }
    let plan = read_plan(input)?;
    observe_source_plan_with_claim(
        &plan,
        output,
        td_boot,
        source,
        trusted_key,
        inventory::claim_plan,
        inventory::recheck_claim_plan,
    )
}

fn observe_source_plan_with_claim<C>(
    plan: &installation_plan::Plan,
    output: &mut impl Write,
    td_boot: &Path,
    source: &Path,
    trusted_key: &Path,
    claim: impl FnOnce(&installation_plan::Plan) -> io::Result<C>,
    recheck: impl FnOnce(&installation_plan::Plan, &mut C) -> io::Result<()>,
) -> io::Result<()> {
    let mut claim = claim(plan)?;
    let id = validate_source_plan(plan, td_boot, source, trusted_key)?;
    recheck(plan, &mut claim)?;
    write_source_plan_report(output, plan, &id)?;
    output.flush()
}

fn write_source_plan_report(
    output: &mut impl Write,
    plan: &installation_plan::Plan,
    id: &str,
) -> io::Result<()> {
    writeln!(output, "{{\"version\":1,\"scope\":\"held-source-plan-observation-only\",\"destination\":\"{}\",\"deployment\":\"{id}\"}}", plan.destination().name())
}

fn validate_source_plan(
    plan: &installation_plan::Plan,
    td_boot: &Path,
    source: &Path,
    trusted_key: &Path,
) -> io::Result<String> {
    let id = authenticate_source(td_boot, source, trusted_key)?;
    if !plan.matches_deployment_id(&id) {
        return Err(invalid(
            "reviewed deployment differs from authenticated source".into(),
        ));
    }
    Ok(id)
}

/// The canonical manifest ID td-boot prints after authenticating `source`.
fn authenticate_source(td_boot: &Path, source: &Path, trusted_key: &Path) -> io::Result<String> {
    let result = std::process::Command::new(td_boot)
        .arg("validate-source")
        .arg(source)
        .arg(trusted_key)
        .output()?;
    if !result.status.success() {
        return Err(invalid(format!(
            "source validation failed: {}: {}",
            result.status,
            String::from_utf8_lossy(&result.stderr).trim_end()
        )));
    }
    let id = std::str::from_utf8(&result.stdout)
        .map_err(|_| invalid("source validator returned a non-UTF-8 ID".into()))?
        .strip_suffix('\n')
        .ok_or_else(|| invalid("source validator ID lacks newline".into()))?;
    if id.len() != 64
        || !id
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    {
        return Err(invalid(
            "source validator returned a noncanonical ID".into(),
        ));
    }
    Ok(id.to_owned())
}

/// The only keyboard layout admitted until a keyboard catalog exists.
const KEYBOARD: &str = "us";

/// The machine the service observes. The wire carries only a refusal's
/// code, so each refusal's cause is also written to stderr.
#[derive(Debug, Eq, PartialEq)]
struct LiveHost {
    td_boot: PathBuf,
    source: PathBuf,
    trusted_key: PathBuf,
    root: PathBuf,
    firstboot: PathBuf,
    timezones: PathBuf,
    /// The catalog, read once: it is immutable deployment data, so a
    /// repeated request costs root nothing and a broken one logs once.
    catalog: Option<Result<installation_plan::Zones, installation_protocol::Refusal>>,
    /// Which deployment the running root was authenticated as.
    booted: PathBuf,
}

// Whatever the catalog reader admits, the protocol's record carries.
const _: () = assert!(
    timezones::MAX_ZONES <= installation_plan::MAX_ZONES
        && timezones::MAX_ID_BYTES <= installation_plan::TIMEZONE_BYTES
);

impl LiveHost {
    fn catalog(&mut self) -> Result<&installation_plan::Zones, installation_protocol::Refusal> {
        use installation_protocol::Refusal;
        let root = &self.timezones;
        self.catalog
            .get_or_insert_with(|| {
                let catalog = timezones::Catalog::load(root)
                    .map_err(|error| refuse(Refusal::TimezonesUnavailable, error))?;
                installation_plan::Zones::new(catalog.ids().map(String::from).collect())
                    .map_err(|error| refuse(Refusal::TimezonesUnavailable, error))
            })
            .as_ref()
            .map_err(|refusal| *refusal)
    }
}

fn refuse(
    refusal: installation_protocol::Refusal,
    cause: impl std::fmt::Display,
) -> installation_protocol::Refusal {
    let _ = writeln!(io::stderr(), "td-install serve: {refusal:?}: {cause}");
    refusal
}

impl installation_service::Host for LiveHost {
    type Claim = File;

    fn candidates(
        &mut self,
    ) -> Result<installation_plan::Candidates, installation_protocol::Refusal> {
        use installation_protocol::Refusal;
        use std::os::unix::fs::MetadataExt;
        // The disk the source is stored on is never a destination, whatever
        // the claim probe finds: resolved from the kernel's inventory, and a
        // source it cannot resolve refuses rather than guessing.
        let source = paths::metadata(&self.source)
            .and_then(|metadata| inventory::backing_disk(metadata.dev()))
            .map_err(|error| {
                refuse(
                    Refusal::DiscoveryFailed,
                    format!("the source's disk: {error}"),
                )
            })?;
        let candidates =
            inventory::candidates().map_err(|error| refuse(Refusal::DiscoveryFailed, error))?;
        excluding(candidates, &source).map_err(|error| refuse(Refusal::DiscoveryFailed, error))
    }

    fn timezones(&mut self) -> Result<installation_plan::Zones, installation_protocol::Refusal> {
        self.catalog().cloned()
    }

    fn check_settings(
        &mut self,
        settings: &installation_plan::Settings,
    ) -> Result<(), installation_protocol::Refusal> {
        use installation_protocol::Refusal;
        PrimarySelection {
            name: settings.username().into(),
            root: self.root.clone(),
            firstboot: self.firstboot.clone(),
        }
        .check()
        .map_err(|error| refuse(Refusal::InvalidUsername, error))?;
        hostname::Hostname::parse(settings.hostname())
            .map_err(|error| refuse(Refusal::InvalidHostname, error))?;
        if settings.keyboard() != KEYBOARD {
            return Err(refuse(
                Refusal::UnsupportedKeyboard,
                format!(
                    "keyboard layout {} is not in the catalog",
                    settings.keyboard()
                ),
            ));
        }
        // The catalog the installer is offered is the one checked here.
        let zone = settings.timezone();
        if self
            .catalog()?
            .as_slice()
            .binary_search_by(|id| id.as_str().cmp(zone))
            .is_err()
        {
            return Err(refuse(
                Refusal::UnsupportedTimezone,
                format!("time zone {zone} is not in the catalog"),
            ));
        }
        Ok(())
    }

    fn claim(
        &mut self,
        destination: &installation_plan::Destination,
    ) -> Result<File, installation_protocol::Refusal> {
        inventory::claim_destination(destination)
            .map_err(|error| refuse(claim_refusal(error.kind()), error))
    }

    fn authenticate_source(&mut self) -> Result<[u8; 32], installation_protocol::Refusal> {
        // The settings are checked against the running root and its catalog;
        // they are checked against this source only if that root is its.
        authenticate_source(&self.td_boot, &self.source, &self.trusted_key)
            .and_then(|id| {
                booted_as(&self.booted, &id)?;
                digest_bytes(&id)
            })
            .map_err(|error| refuse(installation_protocol::Refusal::SourceUnavailable, error))
    }

    fn check_fit(
        &mut self,
        plan: &installation_plan::Plan,
    ) -> Result<(), installation_protocol::Refusal> {
        use installation_protocol::Refusal;
        let unavailable = |error: io::Error| refuse(Refusal::SourceUnavailable, error);
        let (kernel, payloads) = deployment_bytes(&self.source).map_err(unavailable)?;
        // The execution's own bound on what it copies to the ESP.
        if kernel == 0 || kernel > MAX_BOOT_FILE {
            return Err(unavailable(invalid(format!(
                "the source's kernel is {kernel} bytes, not 1..={MAX_BOOT_FILE}"
            ))));
        }
        let uuid = VolumeUuid::parse(&canonical_uuid(plan.volume_uuid())).map_err(unavailable)?;
        let selector = selector_parts(
            &self.root.join(protocol::SELECTOR_TEMPLATE_PATH),
            &self.trusted_key,
            &uuid,
        )
        .map_err(unavailable)?
        .total()
        .ok_or_else(|| unavailable(invalid("the selector's length overflowed".into())))?;
        let sector = u64::from(plan.destination().sector());
        let capacity = plan.destination().capacity();
        volume_fit(sector, capacity, payloads)
            .map_err(|error| refuse(Refusal::InsufficientSpace, error))?;
        // The ESP is the same size on every disk, its FAT capacity differing
        // only by a sector size's reserved sectors: what it cannot hold is
        // the source's to shrink.
        esp_fit(sector, capacity, kernel, selector)
            .map_err(|error| refuse(Refusal::SourceUnavailable, error))
    }

    fn recheck(&mut self, destination: &installation_plan::Destination, claim: &mut File) -> bool {
        match inventory::recheck_claim_destination(destination, claim) {
            Ok(()) => true,
            Err(error) => {
                refuse(installation_protocol::Refusal::DestinationChanged, error);
                false
            }
        }
    }

    fn entropy(&mut self) -> io::Result<([u8; 32], [u8; 16])> {
        let mut bytes = [0; 48];
        let urandom = Path::new("/dev/urandom");
        paths::open_read(urandom)?
            .read_exact(&mut bytes)
            .map_err(|error| {
                io::Error::new(error.kind(), format!("read {}: {error}", urandom.display()))
            })?;
        Ok(plan_identity(bytes))
    }

    fn restart(&mut self) -> Result<(), installation_protocol::Refusal> {
        let supervisor = self.root.join("bin/td-svc");
        request_reboot(&supervisor, RESTART_TIMEOUT)
            .map_err(|error| refuse(installation_protocol::Refusal::RestartUnavailable, error))
    }
}

/// How long td-svc's client may take to have the reboot accepted; the
/// supervisor answers before it stops anything.
const RESTART_TIMEOUT: Duration = Duration::from_secs(10);

/// Asks the supervisor at `supervisor` for its orderly reboot (td-svc/DESIGN.md
/// section 8): its client, with nothing of the service's, and only its two
/// acceptances count.
fn request_reboot(supervisor: &Path, timeout: Duration) -> io::Result<()> {
    // A socket rather than a pipe, so the read is nonblocking and the one
    // deadline bounds it, whatever holds the other end.
    let (mut reply, output) = std::os::unix::net::UnixStream::pair()?;
    reply.set_nonblocking(true)?;
    let mut child = std::process::Command::new(supervisor)
        .arg("reboot")
        .env_clear()
        .current_dir("/")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(std::os::fd::OwnedFd::from(
            output,
        )))
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("start {}: {error}", supervisor.display()),
            )
        })?;
    let result = settle_reboot(&mut child, &mut reply, timeout);
    if result.is_err() {
        // Only the child this call started.
        let _ = child.kill();
        let _ = child.wait();
    }
    result
}

fn settle_reboot(
    child: &mut std::process::Child,
    output: &mut std::os::unix::net::UnixStream,
    timeout: Duration,
) -> io::Result<()> {
    let deadline = std::time::Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| invalid("restart deadline overflow".into()))?;
    let mut reply = Vec::new();
    let mut buffer = [0; 128];
    let mut ended = false;
    let mut exited = None;
    let status = loop {
        if !ended {
            match output.read(&mut buffer) {
                Ok(0) => ended = true,
                Ok(read) => {
                    reply.extend_from_slice(buffer.get(..read).unwrap_or_default());
                    if reply.len() > 128 {
                        return Err(invalid("the supervisor's answer is too long".into()));
                    }
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) => {}
                Err(error) => return Err(error),
            }
        }
        if exited.is_none() {
            exited = child.try_wait()?;
        }
        if let (Some(status), true) = (exited, ended) {
            break status;
        }
        if std::time::Instant::now() >= deadline {
            return Err(invalid("the supervisor did not answer the restart".into()));
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    match reply.as_slice() {
        b"reboot requested\n" | b"shutdown already in progress (reboot)\n" if status.success() => {
            Ok(())
        }
        _ => Err(invalid(format!(
            "the supervisor did not accept the restart ({status}): {}",
            String::from_utf8_lossy(&reply).trim_end()
        ))),
    }
}

/// A consented installation onto the held disk (DESIGN.md "Executing a
/// consented installation"). Every path is serve's caller's or derived from
/// the plan; the installer supplies none.
struct LiveExecution {
    td_boot: PathBuf,
    source: PathBuf,
    trusted_key: PathBuf,
    root: PathBuf,
    firstboot: PathBuf,
    timezones: PathBuf,
    /// The deployment id this boot's root was authenticated as.
    booted: PathBuf,
    /// Where the private workspace is made.
    run: PathBuf,
}

/// The live root's record of its deployment, written by its init.
const BOOTED_DEPLOYMENT: &str = "/run/td-deployment";

/// The running root was authenticated as `deployment`, by the record a live
/// boot's init writes.
fn booted_as(record: &Path, deployment: &str) -> io::Result<()> {
    let booted = realfile::read_bounded_real_file(record, "booted deployment record", 65)?;
    if booted.strip_suffix(b"\n") != Some(deployment.as_bytes()) {
        return Err(invalid(format!(
            "{} does not name deployment {deployment}",
            record.display()
        )));
    }
    Ok(())
}

/// The candidates without the disk the source is stored on.
fn excluding(
    candidates: installation_plan::Candidates,
    disk: &str,
) -> Result<installation_plan::Candidates, String> {
    installation_plan::Candidates::new(
        candidates
            .as_slice()
            .iter()
            .filter(|candidate| candidate.name() != disk)
            .cloned()
            .collect(),
    )
}

/// Why an execution stopped, and the cause written to standard error.
type Stopped = (installation_protocol::Failure, io::Error);

/// `failure`, unless the cause is space: before the first write, a full
/// filesystem is insufficient space whichever step met it.
fn before_writing(failure: installation_protocol::Failure) -> impl Fn(io::Error) -> Stopped {
    move |error| {
        let failure = match error.kind() {
            io::ErrorKind::StorageFull | io::ErrorKind::QuotaExceeded => {
                installation_protocol::Failure::InsufficientSpace
            }
            _ => failure,
        };
        (failure, error)
    }
}

impl LiveExecution {
    fn for_host(host: &LiveHost) -> Self {
        Self {
            td_boot: host.td_boot.clone(),
            source: host.source.clone(),
            trusted_key: host.trusted_key.clone(),
            root: host.root.clone(),
            firstboot: host.firstboot.clone(),
            timezones: host.timezones.clone(),
            booted: host.booted.clone(),
            run: PathBuf::from("/run"),
        }
    }

    fn install(
        &self,
        plan: &installation_plan::Plan,
        claim: File,
        workspace: &Workspace,
        progress: &mut dyn FnMut(installation_protocol::Phase),
    ) -> Result<(), Stopped> {
        use installation_protocol::{Failure, Phase};
        let verification = before_writing(Failure::VerificationFailed);
        let settings_failed = before_writing(Failure::SettingsFailed);
        let deployment = sha256::to_base16(plan.deployment());
        // Everything taken from the root below — the account and catalog
        // checks, the selector template, mkfs.btrfs — is the planned
        // deployment's only if the root is.
        booted_as(&self.booted, &deployment).map_err(&verification)?;
        // The review's volume fit, asked again of the source as it is now
        // and before anything is staged; the layout's own refusal of a disk
        // too small for it is part of it.
        let (_, payloads) = deployment_bytes(&self.source).map_err(&verification)?;
        volume_fit(
            u64::from(plan.destination().sector()),
            plan.destination().capacity(),
            payloads,
        )
        .map_err(|error| (Failure::InsufficientSpace, invalid(error)))?;
        let kernel_sha256 = self
            .kernel_digest(plan.deployment())
            .map_err(&verification)?;
        // A private copy, so the bytes checked and the bytes copied to the
        // ESP are one root-only file's whatever the source does meanwhile.
        stage_kernel(&self.kernel()?, &workspace.kernel).map_err(&verification)?;
        let uuid = VolumeUuid::parse(&canonical_uuid(plan.volume_uuid())).map_err(&verification)?;
        prepare_selector(
            &self.root.join(protocol::SELECTOR_TEMPLATE_PATH),
            &self.trusted_key,
            &uuid,
            &workspace.selector,
        )
        .map_err(&verification)?;
        let chosen = plan.settings();
        let timezone = timezones::Selection::load(&self.timezones, chosen.timezone())
            .map_err(&settings_failed)?;
        let hostname = hostname::Hostname::parse(chosen.hostname())
            .map_err(|error| settings_failed(invalid(error)))?;
        let username = PrimarySelection {
            name: chosen.username().to_owned(),
            root: self.root.clone(),
            firstboot: self.firstboot.clone(),
        };
        let seed = VolumeSeed::Loop(
            self.trusted_key.clone(),
            LoopPublish {
                td_boot: self.td_boot.clone(),
                deployment: self.source.clone(),
                mountpoint: workspace.mountpoint.clone(),
            },
        );
        let mkfs = self.root.join("bin").join(protocol::MKFS_BTRFS);
        let settings = VolumeSettings {
            timezone: Some(&timezone),
            hostname: Some(&hostname),
            username: Some(&username),
        };
        let prepared = prepare_volume(
            settings,
            Some(&uuid),
            &mkfs,
            &workspace.scratch,
            Some(&seed),
        )
        .map_err(&settings_failed)?;
        let mut destination = FormatDestination {
            file: claim,
            label: PathBuf::from(format!("/dev/{}", plan.destination().name())),
        };
        let boot = BootFiles {
            kernel: workspace.kernel.clone(),
            initramfs: workspace.selector.clone(),
        };
        // What the selector copied to the ESP must be, through the descriptor
        // the copy reads, and read back afterwards: the private copy prepared
        // above, which nothing else writes.
        let selector_sha256 = realfile::open_real_file(&workspace.selector, "prepared selector")
            .and_then(|(mut file, metadata)| digest_range(&mut file, 0, metadata.len()))
            .map_err(&verification)?;
        let formatted = format_held(
            prepared,
            &mut destination,
            &boot,
            Some(&[kernel_sha256, selector_sha256]),
            &mut io::sink(),
            &mut |step| {
                progress(match step {
                    FormatStep::Writing => Phase::WritingFilesystems,
                    FormatStep::Publishing => Phase::PublishingDeployment,
                })
            },
        )
        .map_err(|failure| match failure {
            HeldFailure::Layout(error) => verification(error),
            // Writing the volume image in scratch: the disk is untouched.
            HeldFailure::Staging(error) => before_writing(Failure::WriteFailed)(error),
            HeldFailure::Written(error) => (Failure::WriteFailed, error),
        })?;
        progress(Phase::VerifyingBoot);
        finish_installation(
            &mut destination.file,
            &formatted,
            &deployment,
            &[kernel_sha256, selector_sha256],
        )
        .map_err(|error| (Failure::VerificationFailed, error))
    }

    /// The `bzImage` digest the authenticated manifest names, once the
    /// manifest is the one the plan names.
    fn kernel_digest(&self, deployment: &[u8; 32]) -> io::Result<[u8; 32]> {
        let path = self.source.join(protocol::MANIFEST_NAME);
        let manifest = realfile::read_bounded_real_file(
            &path,
            "deployment manifest",
            protocol::MAX_MANIFEST_BYTES,
        )?;
        let mut hasher = sha256::Sha256::new();
        hasher.update(&manifest);
        if hasher.finalize() != *deployment {
            return Err(invalid(format!(
                "{} is not the authenticated deployment's manifest",
                path.display()
            )));
        }
        // The bytes hashed are the bytes parsed. td-boot's four-line form
        // puts `bzImage`'s entry straight after the header.
        let mut lines = manifest.split(|byte| *byte == b'\n');
        if lines.next() != Some(protocol::MANIFEST_HEADER) {
            return Err(invalid(format!(
                "{} lacks the td-deployment-v1 header",
                path.display()
            )));
        }
        lines
            .next()
            .and_then(|entry| entry.strip_suffix(b"  bzImage"))
            .filter(|digest| protocol::valid_digest(digest))
            .and_then(|digest| std::str::from_utf8(digest).ok())
            .ok_or_else(|| invalid(format!("{} has no kernel entry", path.display())))
            .and_then(digest_bytes)
    }

    fn kernel(&self) -> Result<PathBuf, Stopped> {
        source_payload(&self.source, "bzImage").map_err(before_writing(
            installation_protocol::Failure::VerificationFailed,
        ))
    }
}

/// Copy the source kernel, a real regular file of at most the EFI bound,
/// to `private`, created fresh at mode 0600.
fn stage_kernel(source: &Path, private: &Path) -> io::Result<()> {
    let (file, metadata) = realfile::open_real_file(source, "EFI kernel")?;
    let len = metadata.len();
    if len == 0 || len > MAX_BOOT_FILE {
        return Err(invalid(format!(
            "EFI kernel must contain 1..={MAX_BOOT_FILE} bytes: {}",
            source.display()
        )));
    }
    let mut copy = paths::create_new_with_mode(private, 0o600)?;
    let copied = io::copy(&mut file.take(len.saturating_add(1)), &mut copy).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "copy {} to {}: {error}",
                source.display(),
                private.display()
            ),
        )
    })?;
    if copied != len {
        return Err(invalid(format!(
            "EFI kernel {} changed size while it was copied",
            source.display()
        )));
    }
    Ok(())
}

/// Canonical lowercase text of a UUID held in network byte order.
fn canonical_uuid(bytes: &[u8; 16]) -> String {
    let mut text = String::with_capacity(36);
    for (at, byte) in bytes.iter().enumerate() {
        if matches!(at, 4 | 6 | 8 | 10) {
            text.push('-');
        }
        text.push_str(&format!("{byte:02x}"));
    }
    text
}

impl installation_service::Execute<File> for LiveExecution {
    fn execute(
        &self,
        plan: &installation_plan::Plan,
        claim: File,
        progress: &mut dyn FnMut(installation_protocol::Phase),
    ) -> Result<(), installation_protocol::Failure> {
        let report = |(failure, error): Stopped| {
            let _ = writeln!(io::stderr(), "td-install serve: {failure:?}: {error}");
            failure
        };
        // Nothing is written yet; a workspace that cannot be made is space,
        // or a name an earlier execution left.
        let workspace = Workspace::create(&self.run, plan.nonce())
            .map_err(|error| report((installation_protocol::Failure::InsufficientSpace, error)))?;
        let outcome = self.install(plan, claim, &workspace, progress);
        if let Err(error) = workspace.remove() {
            let _ = writeln!(io::stderr(), "td-install serve: workspace: {error}");
        }
        outcome.map_err(report)
    }
}

/// A source payload under td-boot's rule for a source directory: the
/// deployment spelling, or the medium's where it differs, never two files,
/// looked up without following a link as td-boot looks.
fn source_payload(source: &Path, name: &str) -> io::Result<PathBuf> {
    use std::os::unix::fs::MetadataExt;
    let medium = protocol::MEDIA_DEPLOYMENT_FILES
        .iter()
        .find(|(_, deployment)| *deployment == name)
        .map(|(iso, _)| iso.to_ascii_lowercase())
        .ok_or_else(|| invalid(format!("{name} is not a deployment payload")))?;
    let deployment = source.join(name);
    if medium == name {
        return Ok(deployment);
    }
    let spelling = medium;
    let medium = source.join(&spelling);
    let identity = |path: &Path| match paths::symlink_metadata(path) {
        Ok(metadata) => Ok(Some((metadata.dev(), metadata.ino()))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    };
    match (identity(&deployment)?, identity(&medium)?) {
        (Some(one), Some(other)) if one != other => Err(invalid(format!(
            "source directory {} holds both {name} and {spelling}",
            source.display()
        ))),
        (None, None) => Err(invalid(format!(
            "source directory {} holds no {name}",
            source.display()
        ))),
        (None, Some(_)) => Ok(medium),
        _ => Ok(deployment),
    }
}

/// The source's kernel length and its three payloads' total, each a real
/// regular file under td-boot's naming.
fn deployment_bytes(source: &Path) -> io::Result<(u64, u64)> {
    let mut total = 0u64;
    let mut kernel = 0;
    for name in ["bzImage", "initramfs.cpio", "root.erofs"] {
        let len = payload_len(source, name)?;
        if name == "bzImage" {
            kernel = len;
        }
        total = total
            .checked_add(len)
            .ok_or_else(|| invalid("the deployment's size overflowed".into()))?;
    }
    Ok((kernel, total))
}

/// A source payload's length, as a real regular file.
fn payload_len(source: &Path, name: &str) -> io::Result<u64> {
    let path = source_payload(source, name)?;
    let metadata = paths::symlink_metadata(&path)?;
    if !metadata.file_type().is_file() {
        return Err(invalid(format!("{} is not a regular file", path.display())));
    }
    Ok(metadata.len())
}

/// The execution's private directory under `/run`: the staged kernel, the
/// prepared selector, the formatter's scratch and td-boot's mountpoint.
/// Dropped without `remove`, as an unwinding panic drops it, it removes
/// what it can.
struct Workspace {
    dir: PathBuf,
    kernel: PathBuf,
    selector: PathBuf,
    scratch: PathBuf,
    mountpoint: PathBuf,
    removed: bool,
}

impl Workspace {
    /// Named by the first eight bytes of the plan's nonce and made fresh:
    /// one that exists refuses.
    fn create(run: &Path, nonce: &[u8; 32]) -> io::Result<Self> {
        let tag: String = nonce
            .iter()
            .take(8)
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let dir = run.join(format!("td-install-{tag}"));
        paths::create_dir_with_mode(&dir, 0o700)?;
        // From here a failure drops it, which removes what was made.
        let workspace = Self {
            kernel: dir.join("bzImage"),
            selector: dir.join("selector.cpio"),
            scratch: dir.join("scratch"),
            mountpoint: dir.join("volume"),
            dir,
            removed: false,
        };
        for directory in [&workspace.scratch, &workspace.mountpoint] {
            paths::create_dir_with_mode(directory, 0o700)?;
        }
        Ok(workspace)
    }

    /// Remove what it holds, every part attempted, and report the first
    /// failure. The mountpoint goes only if empty, so a volume td-boot left
    /// mounted there is never walked.
    fn remove(mut self) -> io::Result<()> {
        self.removed = true;
        self.clean()
    }

    fn clean(&self) -> io::Result<()> {
        [
            paths::remove_file_if_present(&self.kernel),
            paths::remove_file_if_present(&self.selector),
            paths::remove_dir_all_if_present(&self.scratch),
            paths::remove_dir_if_present(&self.mountpoint),
            paths::remove_dir_if_present(&self.dir),
        ]
        .into_iter()
        .collect()
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        if !self.removed {
            let _ = self.clean();
        }
    }
}

/// A proposal nonce and a version-4 volume UUID in network byte order, as
/// the plan carries it (unlike `random_guid`'s GPT layout).
fn plan_identity(bytes: [u8; 48]) -> ([u8; 32], [u8; 16]) {
    let mut nonce_bytes = [0; 32];
    let mut uuid_bytes = [0; 16];
    for (slot, byte) in nonce_bytes
        .iter_mut()
        .chain(uuid_bytes.iter_mut())
        .zip(bytes)
    {
        *slot = byte;
    }
    if let Some(byte) = uuid_bytes.get_mut(6) {
        *byte = (*byte & 0x0f) | 0x40;
    }
    if let Some(byte) = uuid_bytes.get_mut(8) {
        *byte = (*byte & 0x3f) | 0x80;
    }
    (nonce_bytes, uuid_bytes)
}

fn digest_bytes(id: &str) -> io::Result<[u8; 32]> {
    let mut digest = [0; 32];
    // `from_str_radix` alone would admit a sign.
    if id.len() != 64 || !id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(invalid("deployment ID must be 64 hex digits".into()));
    }
    for (slot, pair) in digest.iter_mut().zip(id.as_bytes().as_chunks::<2>().0) {
        *slot = std::str::from_utf8(pair)
            .ok()
            .and_then(|pair| u8::from_str_radix(pair, 16).ok())
            .ok_or_else(|| invalid("deployment ID must be 64 hex digits".into()))?;
    }
    Ok(digest)
}

/// How a failed claim is reported: a disk held elsewhere is busy, one that
/// changed or vanished is changed, and anything else failed discovery.
fn claim_refusal(kind: io::ErrorKind) -> installation_protocol::Refusal {
    use installation_protocol::Refusal;
    match kind {
        io::ErrorKind::ResourceBusy => Refusal::DestinationBusy,
        // Discovery just listed the disk; failing to claim it means it is
        // no longer the disk that was observed.
        _ => Refusal::DestinationChanged,
    }
}

/// Serve only as uid 0, with the installer's channel on stdin and
/// td-authd's consent channel on stdout, each its own connected Unix stream
/// socket; refuse before any byte. `euid` is this process's own: a sanity
/// gate for a misplaced start, not proof of the caller's privilege (uid 0
/// in a user namespace passes).
fn admit_serve(
    euid: u32,
    stdin: File,
    stdout: File,
) -> io::Result<(
    std::os::unix::net::UnixStream,
    std::os::unix::net::UnixStream,
)> {
    use std::os::unix::fs::MetadataExt;
    if euid != 0 {
        return Err(invalid("serve requires the installation authority".into()));
    }
    let identity = |file: &File| file.metadata().map(|meta| (meta.dev(), meta.ino()));
    let (one, other) = (identity(&stdin)?, identity(&stdout)?);
    let installer = admit_channel(stdin, "serve requires its installer channel on stdin")?;
    let consent = admit_channel(stdout, "serve requires its consent channel on stdout")?;
    // One socket on both would interleave the two protocols.
    if one == other {
        return Err(invalid(
            "serve requires distinct installer and consent channels".into(),
        ));
    }
    Ok((installer, consent))
}

fn admit_channel(file: File, refusal: &str) -> io::Result<std::os::unix::net::UnixStream> {
    use std::os::unix::fs::FileTypeExt;
    use std::os::unix::fs::MetadataExt;
    let channel = || invalid(refusal.into());
    let metadata = file.metadata()?;
    if !metadata.file_type().is_socket() {
        return Err(channel());
    }
    let stream = std::os::unix::net::UnixStream::from(std::os::fd::OwnedFd::from(file));
    // Another socket family, or an unconnected socket, has no Unix peer.
    stream.peer_addr().map_err(|_| channel())?;
    // A datagram or sequenced-packet peer would truncate frames and never
    // close as a stream does, so its claim could outlive it.
    let table = io::BufReader::new(paths::open_read(Path::new("/proc/net/unix"))?);
    if !unix_stream_in(table, metadata.ino())? {
        return Err(channel());
    }
    // Blocking reads frame the channel; an inherited O_NONBLOCK would not.
    stream.set_nonblocking(false)?;
    Ok(stream)
}

/// Whether a `/proc/net/unix` table lists socket inode `inode` as
/// SOCK_STREAM (Type `0001`); its columns are Num, RefCount, Protocol,
/// Flags, Type, St, Inode and an optional path. A path is printed raw, so
/// a bound name holding a newline can forge a row: every row naming the
/// inode must say stream, so a forgery can only refuse a socket listed
/// here. The caller creates the socket in this namespace (INSTALLER.md).
fn unix_stream_in(table: impl io::BufRead, inode: u64) -> io::Result<bool> {
    let inode = inode.to_string();
    let mut listed = false;
    for line in table.split(b'\n').skip(1) {
        let line = line?;
        let mut fields = line
            .split(|byte| *byte == b' ')
            .filter(|field| !field.is_empty());
        let (Some(kind), Some(named)) = (fields.nth(4), fields.nth(1)) else {
            continue;
        };
        if named == inode.as_bytes() {
            if kind != b"0001" {
                return Ok(false);
            }
            listed = true;
        }
    }
    Ok(listed)
}

fn run_serve(host: LiveHost) -> io::Result<()> {
    use std::os::fd::AsFd;
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::fs::PermissionsExt;
    // A missing or misplaced control-plane input is the caller's error;
    // without this it would reach the installer as a refused choice.
    enum Kind {
        Executable,
        Directory,
        File,
    }
    for (operand, kind) in [
        (&host.td_boot, Kind::Executable),
        (&host.source, Kind::Directory),
        (&host.trusted_key, Kind::File),
        (&host.root, Kind::Directory),
        (&host.firstboot, Kind::Executable),
    ] {
        let (admits, kind): (fn(&std::fs::Metadata) -> bool, _) = match kind {
            Kind::Executable => (
                |metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0,
                "an executable file",
            ),
            Kind::Directory => (|metadata| metadata.is_dir(), "a directory"),
            Kind::File => (|metadata| metadata.is_file(), "a file"),
        };
        match paths::metadata_if_present(operand) {
            None => {
                return Err(invalid(format!(
                    "serve operand {} is not present",
                    operand.display()
                )))
            }
            Some(metadata) if !admits(&metadata) => {
                return Err(invalid(format!(
                    "serve operand {} is not {kind}",
                    operand.display()
                )))
            }
            Some(_) => {}
        }
    }
    // Held for the service's life: the source's filesystem, and with it the
    // kernel's exclusive claim on the disk it is stored on, outlives even a
    // lazy unmount, so that disk is never claimable as a destination. The
    // source itself is read by path; detached, propose refuses it.
    let _source = paths::open_read(&host.source)?;
    let euid = paths::open_read(Path::new("/proc/self"))?.metadata()?.uid();
    let stdin = File::from(io::stdin().as_fd().try_clone_to_owned()?);
    // Nothing serve runs writes to stdout: every child's is piped or null.
    let stdout = File::from(io::stdout().as_fd().try_clone_to_owned()?);
    let (stream, consent) = admit_serve(euid, stdin, stdout)?;
    let execution = LiveExecution::for_host(&host);
    installation_service::serve(
        stream,
        installation_service::Service::with_execution(host, execution),
        Some(consent),
    )
}

fn preview_number(value: &OsStr, label: &str) -> io::Result<u64> {
    let text = value
        .to_str()
        .ok_or_else(|| invalid(format!("{label}: expected 1..=20 ASCII decimal digits")))?;
    if text.is_empty() || text.len() > 20 || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(invalid(format!(
            "{label}: expected 1..=20 ASCII decimal digits"
        )));
    }
    text.parse()
        .map_err(|_| invalid(format!("{label}: byte count exceeds u64")))
}

/// The destination's size in bytes, asked of the destination itself.
///
/// `seek` to the end rather than `metadata().len()`, which reports 0 for a
/// block device: seeking answers for both destinations, in safe `std`, and D8
/// keeps this crate's syscalls to its one recorded loop surface — a
/// `BLKGETSIZE64` ioctl would be an amendment to `UNSAFE.md` for something
/// ordinary file I/O already does.
fn destination_bytes(file: &mut File) -> io::Result<u64> {
    let size = file.seek(SeekFrom::End(0))?;
    file.rewind()?;
    Ok(size)
}

/// Split a 64-bit `st_rdev` into its major and minor numbers.
///
/// glibc's `gnu_dev_major`/`gnu_dev_minor` in `<sys/sysmacros.h>`, which is the
/// encoding the kernel writes and `/sys/dev/block/<major>:<minor>/` is named
/// for: minor is bits 0..=7 and 20..=43, major is bits 8..=19 and 44..=63, so
/// the two INTERLEAVE and neither is a contiguous field.
///
/// Each half is masked to its own width rather than by clearing the other's low
/// bits, which is the mistake this had: shifting the extended minor down puts
/// the extended MAJOR just above it, and a mask that only clears the bottom
/// leaves it there. Dormant while block majors stay under 4096 — they all do
/// today — and a sysfs path naming a device that does not exist when one does
/// not, which reads as "cannot read", not as a wrong sector size.
fn device_numbers(rdev: u64) -> (u64, u64) {
    let major = ((rdev >> 8) & 0xfff) | ((rdev >> 32) & 0xffff_f000);
    let minor = (rdev & 0xff) | ((rdev >> 12) & 0xffff_ff00);
    (major, minor)
}

/// The destination's logical sector size.
///
/// A regular file has none, so it takes `FILE_SECTOR_BYTES`. A block device is
/// asked through sysfs, by the device NUMBER off the opened file rather than by
/// its path — the same argument that makes `td-init`'s `losetup` read its
/// read-only flag out of `/sys/dev/block/<major>:<minor>/`: a path can name a
/// different device than the one the descriptor is open on. Getting this wrong
/// on a 4Kn disk writes a table whose every LBA is off by a factor of eight,
/// which firmware reads as no table at all.
fn logical_sector_size(file: &File) -> io::Result<u64> {
    use std::os::linux::fs::MetadataExt;
    use std::os::unix::fs::FileTypeExt;

    let metadata = file.metadata()?;
    if metadata.is_file() {
        return Ok(FILE_SECTOR_BYTES);
    }
    if !metadata.file_type().is_block_device() {
        return Err(invalid(
            "a destination must be a regular file or a block device".to_string(),
        ));
    }
    let (major, minor) = device_numbers(metadata.st_rdev());
    let path = PathBuf::from(format!(
        "/sys/dev/block/{major}:{minor}/queue/logical_block_size"
    ));
    let text = paths::read_to_string(&path)?;
    let trimmed = text.trim();
    let size = trimmed.parse::<u64>().map_err(|_| {
        invalid(format!(
            "{} reads {trimmed:?}, which is not a sector size",
            path.display()
        ))
    })?;
    // Refused HERE, where the number arrives from outside, rather than at each
    // division downstream. A device with no media reports 0 bytes and can read 0
    // here, and `0.is_multiple_of(0)` is TRUE — so a sector check written the
    // obvious way passes and the division after it aborts the process, this
    // crate being `panic = "abort"`.
    if size == 0 {
        return Err(invalid(format!(
            "{} reads a sector size of 0",
            path.display()
        )));
    }
    Ok(size)
}

/// Where the two partitions go, in sectors.
#[derive(Debug, Eq, PartialEq)]
struct Plan {
    sector_size: u64,
    /// Partition alignment in SECTORS, computed and validated once. Carried
    /// rather than recomputed at the call site for the reason the layout
    /// constants live in `protocol.rs`: two expressions of one value can
    /// disagree.
    align_sectors: u64,
    disk_sectors: u64,
    esp_start: u64,
    esp_end: u64,
    volume_start: u64,
    volume_end: u64,
}

impl Plan {
    fn esp_offset(&self) -> Option<u64> {
        self.esp_start.checked_mul(self.sector_size)
    }

    fn esp_sectors(&self) -> Option<u64> {
        self.esp_end.checked_sub(self.esp_start)?.checked_add(1)
    }

    fn volume_bytes(&self) -> Option<u64> {
        self.volume_end
            .checked_sub(self.volume_start)?
            .checked_add(1)?
            .checked_mul(self.sector_size)
    }
}

/// Round `sectors` up to the next multiple of the alignment.
fn align_up(sectors: u64, align: u64) -> Option<u64> {
    if align == 0 {
        return None;
    }
    let remainder = sectors % align;
    if remainder == 0 {
        return Some(sectors);
    }
    sectors.checked_add(align - remainder)
}

/// Compute the layout, or say why the disk cannot hold one.
///
/// Every partition boundary is INCLUSIVE, because that is how GPT stores it and
/// an exclusive end written into that field is an off-by-one no reader detects.
fn plan(sector_size: u64, disk_bytes: u64) -> Result<Plan, String> {
    if sector_size == 0 {
        return Err("the destination reports a sector size of 0".to_string());
    }
    if !disk_bytes.is_multiple_of(sector_size) {
        return Err(format!(
            "destination is {disk_bytes} bytes, not a whole number of \
             {sector_size}-byte sectors"
        ));
    }
    let disk_sectors = disk_bytes / sector_size;
    let minimum = gpt::minimum_disk_sectors(sector_size)?;
    if disk_sectors < minimum {
        return Err(format!(
            "destination holds {disk_sectors} sectors, and a GPT alone \
             needs {minimum}"
        ));
    }
    let align = protocol::PARTITION_ALIGN_BYTES / sector_size;
    if align == 0 {
        return Err(format!(
            "a {sector_size}-byte sector is larger than the \
             {}-byte partition alignment",
            protocol::PARTITION_ALIGN_BYTES
        ));
    }
    let first_usable = gpt::first_usable_lba(sector_size)?;
    let last_usable = gpt::last_usable_lba(sector_size, disk_sectors)?;

    let esp_start = align_up(first_usable, align)
        .ok_or_else(|| "aligning the ESP start overflowed".to_string())?;
    let esp_sectors = protocol::ESP_BYTES / sector_size;
    let esp_end = esp_start
        .checked_add(esp_sectors)
        .and_then(|end| end.checked_sub(1))
        .ok_or_else(|| "the ESP does not fit in an LBA".to_string())?;

    let volume_start = align_up(
        esp_end
            .checked_add(1)
            .ok_or_else(|| "the volume start overflowed".to_string())?,
        align,
    )
    .ok_or_else(|| "aligning the volume start overflowed".to_string())?;
    if volume_start > last_usable {
        return Err(format!(
            "destination is too small — the ESP alone reaches LBA \
             {esp_end} and the last usable LBA is {last_usable}"
        ));
    }
    let volume_sectors = last_usable
        .checked_sub(volume_start)
        .and_then(|s| s.checked_add(1))
        .unwrap_or(0);
    let volume_bytes = volume_sectors.saturating_mul(sector_size);
    if volume_bytes < protocol::MIN_VOLUME_BYTES {
        return Err(format!(
            "the td volume would be {volume_bytes} bytes and needs at \
             least {} — a disk this size cannot retain two deployments while \
             publishing an update",
            protocol::MIN_VOLUME_BYTES
        ));
    }
    Ok(Plan {
        sector_size,
        align_sectors: align,
        disk_sectors,
        esp_start,
        esp_end,
        volume_start,
        volume_end: last_usable,
    })
}

/// Pure geometry for a future review page; no disk identity or write authority.
fn layout_preview(sector_bytes: u64, capacity_bytes: u64, out: &mut dyn Write) -> io::Result<()> {
    if !matches!(sector_bytes, 512 | 4096) {
        return Err(invalid(
            "layout preview supports 512-byte and 4096-byte logical sectors".into(),
        ));
    }
    let layout = plan(sector_bytes, capacity_bytes)
        .map_err(|error| invalid(format!("layout preview: {error}")))?;
    let byte_range = |start: u64, end: u64| -> io::Result<(u64, u64)> {
        let offset = start
            .checked_mul(sector_bytes)
            .ok_or_else(|| invalid("preview partition offset overflow".into()))?;
        let length = end
            .checked_sub(start)
            .and_then(|sectors| sectors.checked_add(1))
            .and_then(|sectors| sectors.checked_mul(sector_bytes))
            .ok_or_else(|| invalid("invalid or overflowing preview partition range".into()))?;
        Ok((offset, length))
    };
    let (esp_offset, esp_bytes) = byte_range(layout.esp_start, layout.esp_end)?;
    let (volume_offset, volume_bytes) = byte_range(layout.volume_start, layout.volume_end)?;
    writeln!(out, concat!(
        "{{\"version\":1,\"scope\":\"layout-preview\",\"logical_sector_bytes\":{},\"capacity_bytes\":{},",
        "\"partitions\":[{{\"number\":1,\"purpose\":\"efi-system\",\"start_lba\":{},\"end_lba\":{},\"offset_bytes\":{},\"capacity_bytes\":{}}},",
        "{{\"number\":2,\"purpose\":\"system-volume\",\"start_lba\":{},\"end_lba\":{},\"offset_bytes\":{},\"capacity_bytes\":{}}}]}}"
    ), sector_bytes, capacity_bytes,
        layout.esp_start, layout.esp_end, esp_offset, esp_bytes,
        layout.volume_start, layout.volume_end, volume_offset, volume_bytes)
}

/// Choose once before preparing the selector and formatting the same volume.
fn new_volume_uuid(output: &mut dyn Write) -> io::Result<()> {
    let uuid = random_guid()?.to_string().to_ascii_lowercase();
    writeln!(output, "{uuid}")
}

/// 16 bytes from `/dev/urandom`, as an RFC 4122 version-4 GUID.
///
/// A disk and its partitions are identified by these, so they are per-install
/// rather than derived from anything: two disks laid out by the same build must
/// not claim the same GUID, or firmware and udev each pick one of them.
/// `/dev/urandom` is an ordinary file, which is what keeps D8 intact.
fn random_guid() -> io::Result<gpt::Guid> {
    let mut bytes = [0u8; 16];
    let urandom = Path::new("/dev/urandom");
    let mut file = paths::open_read(urandom)?;
    file.read_exact(&mut bytes).map_err(|error| {
        io::Error::new(error.kind(), format!("read {}: {error}", urandom.display()))
    })?;
    // Version 4 and the RFC 4122 variant, in the on-disk mixed-endian layout:
    // the version nibble is the high nibble of byte 7's field, which little-endian
    // encoding of the third group puts at index 7, and the variant at index 8.
    if let Some(byte) = bytes.get_mut(7) {
        *byte = (*byte & 0x0f) | 0x40;
    }
    if let Some(byte) = bytes.get_mut(8) {
        *byte = (*byte & 0x3f) | 0x80;
    }
    Ok(gpt::Guid(bytes))
}

/// EVERY filesystem call this program makes, and the only place a path is
/// paired with the error that names it.
///
/// `io::Error` carries an errno and nothing else, so a destination that is not
/// there refuses with a bare `No such file or directory` on a command line
/// that names up to five paths — and an operator has no way to tell which one
/// it meant. Same argument `td-boot`'s `read_trusted_key` makes, on the other
/// half of the deployment path.
///
/// The property is STRUCTURAL rather than checked. Each wrapper takes its path
/// as a parameter and holds no other, so `.at()` cannot be handed a file the
/// operation never touched: there is no second path in scope to hand it. What
/// the test outside enforces is only that nothing else in the crate opens
/// anything, which is a question about where a call IS rather than about what
/// it means — and four rounds of review are why it is put that way round.
///
/// Scoped to operations that take a PATH. A call on an already-open `File` — a
/// write, a `sync_all`, a `set_len` — is deliberately not here: it is about a
/// descriptor rather than a name, and the name it would be given is the one
/// the open already reported.
///
/// The wrap costs `raw_os_error()` and any `source()` chain, neither of which
/// `io::Error::new` can carry. `kind()` survives, which is what the callers'
/// `!= NotFound` tests read.
#[allow(clippy::disallowed_methods)]
mod paths {
    use std::fs::{DirBuilder, File, Metadata, OpenOptions, Permissions};
    use std::io::{self, Read};
    use std::path::{Path, PathBuf};

    /// Name the file an IO failure was about.
    fn named(error: io::Error, path: &Path) -> io::Error {
        io::Error::new(error.kind(), format!("{}: {error}", path.display()))
    }

    /// `named` where the result is being propagated, which is most of them.
    trait NamePath<T> {
        fn at(self, path: &Path) -> io::Result<T>;
    }

    impl<T> NamePath<T> for io::Result<T> {
        fn at(self, path: &Path) -> io::Result<T> {
            self.map_err(|error| named(error, path))
        }
    }

    pub fn open_read(path: &Path) -> io::Result<File> {
        File::open(path).at(path)
    }

    /// An existing node, for reading and writing; nothing is created.
    pub fn open_read_write(path: &Path) -> io::Result<File> {
        OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .at(path)
    }

    /// Consume directory iteration here so late errors also name the path.
    pub fn read_dir_bounded(path: &Path, limit: usize) -> io::Result<Vec<PathBuf>> {
        let mut entries = Vec::new();
        for entry in std::fs::read_dir(path).at(path)? {
            let entry = entry.at(path)?;
            if entries.len() == limit {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{}: directory exceeds {limit} entries", path.display()),
                ));
            }
            entries.push(entry.path());
        }
        entries.sort();
        Ok(entries)
    }

    pub fn read_bounded(path: &Path, limit: usize) -> io::Result<Vec<u8>> {
        let ceiling = u64::try_from(limit)
            .ok()
            .and_then(|n| n.checked_add(1))
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("{}: invalid read limit", path.display()),
                )
            })?;
        let mut bytes = Vec::new();
        File::open(path)
            .at(path)?
            .take(ceiling)
            .read_to_end(&mut bytes)
            .at(path)?;
        if bytes.len() > limit {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{}: attribute exceeds {limit} bytes", path.display()),
            ));
        }
        Ok(bytes)
    }

    pub fn open_destination_claim(path: &Path, writable: bool) -> io::Result<File> {
        use std::os::unix::fs::OpenOptionsExt;
        if !cfg!(all(target_os = "linux", target_arch = "x86_64")) {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!(
                    "{}: destination claims require x86-64 Linux",
                    path.display()
                ),
            ));
        }
        // Linux claims block devices through O_EXCL, including their partitions.
        // Without O_CREAT this flag does not change regular-image opens.
        const O_EXCL: i32 = 0x80;
        OpenOptions::new()
            .read(true)
            .write(writable)
            .custom_flags(O_EXCL)
            .open(path)
            .at(path)
    }

    /// Create, refusing anything already there — which is what keeps a symlink
    /// left at the path from being followed and truncated.
    pub fn create_new(path: &Path) -> io::Result<File> {
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .at(path)
    }

    /// The same, at a chosen creation mode.
    pub fn create_new_with_mode(path: &Path, mode: u32) -> io::Result<File> {
        use std::os::unix::fs::OpenOptionsExt;
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(path)
            .at(path)
    }

    pub fn create_dir_all(path: &Path) -> io::Result<()> {
        std::fs::create_dir_all(path).at(path)
    }

    /// A directory created AT `mode` rather than widened to it, so there is no
    /// window in which it is more permissive than asked for.
    pub fn create_dir_with_mode(path: &Path, mode: u32) -> io::Result<()> {
        use std::os::unix::fs::DirBuilderExt;
        DirBuilder::new().mode(mode).create(path).at(path)
    }

    pub fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, Permissions::from_mode(mode)).at(path)
    }

    pub fn canonicalize(path: &Path) -> io::Result<PathBuf> {
        std::fs::canonicalize(path).at(path)
    }

    pub fn symlink_metadata(path: &Path) -> io::Result<Metadata> {
        std::fs::symlink_metadata(path).at(path)
    }

    /// What is at `path`, symlinks followed.
    pub fn metadata(path: &Path) -> io::Result<Metadata> {
        std::fs::metadata(path).at(path)
    }

    /// What is at `path`, or nothing — the one call here whose error is
    /// DISCARDED, because each caller asks only whether a usable file is
    /// there (the same file, or a serve operand), and an unreadable path is
    /// not one.
    pub fn metadata_if_present(path: &Path) -> Option<Metadata> {
        std::fs::metadata(path).ok()
    }

    /// Whether `path` is a directory, and an ERROR rather than `false` where
    /// the question cannot be answered.
    ///
    /// `Path::is_dir` is the obvious spelling and is wrong twice over: it is a
    /// filesystem call outside this module, and it reports a directory it was
    /// REFUSED as one that is not there. Its caller turns that answer into
    /// "td-boot published nothing", which would be a false accusation against
    /// the program that had just done the work.
    pub fn is_dir(path: &Path) -> io::Result<bool> {
        match std::fs::metadata(path) {
            Ok(metadata) => Ok(metadata.is_dir()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(named(error, path)),
        }
    }

    /// A rename names BOTH paths: either can be the one at fault, and the
    /// errno alone does not say which.
    pub fn rename(from: &Path, to: &Path) -> io::Result<()> {
        std::fs::rename(from, to).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!(
                    "cannot rename {} to {}: {error}",
                    from.display(),
                    to.display()
                ),
            )
        })
    }

    /// A whole small file, named as a READ failure so it cannot be mistaken
    /// for the parse that follows it — `device_numbers` turns on the
    /// difference between a sysfs path that is not there and a sector size
    /// that does not parse.
    pub fn read_to_string(path: &Path) -> io::Result<String> {
        std::fs::read_to_string(path).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("cannot read {}: {error}", path.display()),
            )
        })
    }

    /// Absent is the state wanted, so an absent path is not a failure.
    pub fn remove_file_if_present(path: &Path) -> io::Result<()> {
        match std::fs::remove_file(path) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => Err(named(error, path)),
            _ => Ok(()),
        }
    }

    /// The same for a tree, which is how both verbs empty a staging root.
    pub fn remove_dir_all_if_present(path: &Path) -> io::Result<()> {
        match std::fs::remove_dir_all(path) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => Err(named(error, path)),
            _ => Ok(()),
        }
    }

    /// The same for one directory, which must be empty.
    pub fn remove_dir_if_present(path: &Path) -> io::Result<()> {
        match std::fs::remove_dir(path) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => Err(named(error, path)),
            _ => Ok(()),
        }
    }
}

/// Write `bytes` at `offset`, seeking first.
fn write_at(file: &mut File, offset: u64, bytes: &[u8]) -> io::Result<()> {
    file.seek(SeekFrom::Start(offset))?;
    file.write_all(bytes)
}

/// Zero `len` bytes at `offset`, in bounded chunks.
fn zero_at(file: &mut File, offset: u64, len: u64) -> io::Result<()> {
    const CHUNK: usize = 1024 * 1024;
    let span = usize::try_from(len).unwrap_or(CHUNK).min(CHUNK);
    let zeros = vec![0u8; span];
    file.seek(SeekFrom::Start(offset))?;
    let mut remaining = len;
    while remaining > 0 {
        let take = usize::try_from(remaining.min(span as u64)).unwrap_or(span);
        let chunk = zeros
            .get(..take)
            .ok_or_else(|| invalid("zero chunk out of range".to_string()))?;
        file.write_all(chunk)?;
        remaining -= take as u64;
    }
    Ok(())
}

/// Empty-ESP zeroing minimum: reserved sectors, both FATs and the root cluster.
/// The emitter may omit zero FAT suffixes, which must not retain old chains.
/// Populated layouts extend this through every directory cluster, then stream
/// all live file bytes and clear their final-cluster padding. Free data
/// clusters stay unreferenced; they are not erased by formatting.
fn metadata_bytes(image: &fat::Image) -> Option<u64> {
    let sector = u64::from(image.bytes_per_sector);
    let reserved = u64::from(fat::RESERVED_SECTORS);
    let fats = u64::from(fat::NUM_FATS).checked_mul(u64::from(image.sectors_per_fat))?;
    let root = u64::from(image.sectors_per_cluster);
    reserved
        .checked_add(fats)?
        .checked_add(root)?
        .checked_mul(sector)
}

/// Zero whatever table the destination already carries.
///
/// Over exactly the two ranges the new one will occupy, which is what makes
/// this complete without reading the old table: a GPT's primary and backup live
/// at fixed positions for a given disk size and sector size, so the ranges are
/// the same ones regardless of what wrote them. The protective MBR is inside
/// the primary range and goes with it — a disk carrying that and no header is
/// one every tool reads as unpartitioned.
fn invalidate_table(file: &mut File, table: &gpt::Image) -> io::Result<()> {
    for (offset, len) in [
        (table.primary_offset, table.primary.len()),
        (table.backup_offset, table.backup.len()),
    ] {
        zero_at(file, offset, len as u64)?;
    }
    Ok(())
}

fn run_layout(destination: &Path, out: &mut dyn Write) -> io::Result<()> {
    run_layout_with_boot(destination, None, out)
}

/// One open destination; the label is diagnostic data and is never reopened.
struct FormatDestination {
    file: File,
    label: PathBuf,
}

impl FormatDestination {
    fn open(path: &Path) -> io::Result<Self> {
        Ok(Self {
            file: paths::open_destination_claim(path, true)?,
            label: path.to_owned(),
        })
    }
}

fn run_layout_with_boot(
    destination: &Path,
    boot: Option<&BootFiles>,
    out: &mut dyn Write,
) -> io::Result<()> {
    let mut destination = FormatDestination::open(destination)?;
    format_layout(&mut destination, boot, out)
}

fn format_layout(
    destination: &mut FormatDestination,
    boot: Option<&BootFiles>,
    out: &mut dyn Write,
) -> io::Result<()> {
    prepare_layout(destination, boot, None)?.write_to(destination, out)
}

struct PreparedLayout {
    plan: Plan,
    table: gpt::Image,
    esp: fat::Image<'static>,
    metadata: u64,
    payloads: Vec<(BootInput, u64, u64, u64)>,
}

/// `digests`, when given, are what the kernel's and initramfs's pinned bytes
/// must hash to before anything is written.
fn prepare_layout(
    destination: &mut FormatDestination,
    boot: Option<&BootFiles>,
    digests: Option<&[[u8; 32]; 2]>,
) -> io::Result<PreparedLayout> {
    let file = &mut destination.file;
    let disk_bytes = destination_bytes(file)?;
    let sector_size = logical_sector_size(file)?;
    let plan = plan(sector_size, disk_bytes).map_err(invalid)?;

    // Pin and size both sources before any destructive write. The caller owns
    // their content and must keep it stable throughout the operation.
    let mut inputs = match boot {
        Some(boot) => Some((
            BootInput::open(&boot.kernel, file)?,
            BootInput::open(&boot.initramfs, file)?,
        )),
        None => None,
    };
    if let Some([kernel_sha256, initramfs_sha256]) = digests {
        let (kernel, initramfs) = inputs
            .as_mut()
            .ok_or_else(|| invalid("boot digests without boot files".into()))?;
        kernel.check_digest(kernel_sha256, "the kernel the authenticated manifest names")?;
        initramfs.check_digest(initramfs_sha256, "the selector this execution prepared")?;
    }
    let kernel_path = format!("\\EFI\\BOOT\\{}", protocol::EFI_BOOT_FILE);
    let layout = gpt::Layout {
        sector_size,
        disk_sectors: plan.disk_sectors,
        disk_guid: random_guid()?,
        align_sectors: plan.align_sectors,
        partitions: vec![
            gpt::Partition {
                type_guid: gpt::TYPE_ESP,
                unique_guid: random_guid()?,
                start_lba: plan.esp_start,
                end_lba: plan.esp_end,
                attributes: 0,
                name: protocol::ESP_PARTITION_NAME.to_string(),
            },
            gpt::Partition {
                type_guid: gpt::TYPE_LINUX_FS,
                unique_guid: random_guid()?,
                start_lba: plan.volume_start,
                end_lba: plan.volume_end,
                attributes: 0,
                name: protocol::VOLUME_PARTITION_NAME.to_string(),
            },
        ],
    };
    let table = gpt::build(&layout).map_err(invalid)?;

    let esp_offset = plan
        .esp_offset()
        .ok_or_else(|| invalid("the ESP offset overflowed".to_string()))?;
    // Derived from the ESP's own GUID rather than from a clock, so the same
    // disk laid out twice differs only where GPT already says it does.
    let volume = esp_volume(
        sector_size,
        &plan,
        volume_serial(&layout)?,
        inputs
            .as_ref()
            .map(|(kernel, initramfs)| (kernel.len, initramfs.len)),
    )?;
    let esp = fat::build(&volume).map_err(invalid)?;
    // Clear the reserved sectors, FATs and every directory cluster, including
    // gaps between emitted metadata extents. Free data clusters stay untouched.
    let metadata = esp.extents.iter().try_fold(
        metadata_bytes(&esp).ok_or_else(|| invalid("ESP metadata overflow".into()))?,
        |high, extent| {
            extent
                .offset
                .checked_add(extent.bytes.len() as u64)
                .map(|end| high.max(end))
                .ok_or_else(|| invalid("ESP extent overflow".into()))
        },
    )?;

    let cluster_bytes = u64::from(esp.bytes_per_sector)
        .checked_mul(u64::from(esp.sectors_per_cluster))
        .filter(|bytes| *bytes != 0)
        .ok_or_else(|| invalid("invalid EFI cluster size".into()))?;
    let data_start = metadata_bytes(&esp)
        .and_then(|end| end.checked_sub(cluster_bytes))
        .ok_or_else(|| invalid("EFI data offset overflow".into()))?;
    // Round relative to the data area, not the start of the FAT filesystem.
    let metadata = metadata
        .checked_sub(data_start)
        .and_then(|span| span.checked_add(cluster_bytes - 1))
        .and_then(|span| (span / cluster_bytes).checked_mul(cluster_bytes))
        .and_then(|span| data_start.checked_add(span))
        .filter(|end| *end <= esp.total_bytes)
        .ok_or_else(|| invalid("EFI metadata exceeds the ESP".into()))?;
    let esp_end = esp_offset
        .checked_add(esp.total_bytes)
        .ok_or_else(|| invalid("EFI partition end overflow".into()))?;

    // Resolve every placement and offset before invalidating the old GPT.
    let mut payloads = Vec::new();
    if let Some((kernel, initramfs)) = inputs {
        for (name, input) in [
            (kernel_path.as_str(), kernel),
            (protocol::EFI_INITRD_PATH, initramfs),
        ] {
            let matching: Vec<_> = esp.placements.iter().filter(|p| p.path == name).collect();
            let [placement] = matching.as_slice() else {
                return Err(invalid(format!(
                    "EFI file must have exactly one placement: {name}"
                )));
            };
            if placement.len != input.len {
                return Err(invalid(format!(
                    "EFI placement length disagrees with input: {name}"
                )));
            }
            let offset = esp_offset
                .checked_add(placement.offset)
                .ok_or_else(|| invalid("EFI file offset overflow".into()))?;
            let end = offset
                .checked_add(placement.len)
                .ok_or_else(|| invalid("EFI file padding offset overflow".into()))?;
            let padding = (cluster_bytes - placement.len % cluster_bytes) % cluster_bytes;
            if end
                .checked_add(padding)
                .is_none_or(|padded_end| padded_end > esp_end)
            {
                return Err(invalid(format!("EFI file padding exceeds the ESP: {name}")));
            }
            payloads.push((input, offset, end, padding));
        }
    }
    if payloads.len() != esp.placements.len() {
        return Err(invalid("unexpected EFI file placement".into()));
    }

    Ok(PreparedLayout {
        plan,
        table,
        esp,
        metadata,
        payloads,
    })
}

impl PreparedLayout {
    /// Where the kernel and initramfs go on the disk, in that order, as
    /// `(offset, len, padding)`: the zeroed rest of a file's last cluster
    /// follows it.
    fn boot_extents(&self) -> io::Result<[(u64, u64, u64); 2]> {
        match self.payloads.as_slice() {
            [(kernel, kernel_at, _, kernel_padding), (initramfs, initramfs_at, _, initramfs_padding)] => {
                Ok([
                    (*kernel_at, kernel.len, *kernel_padding),
                    (*initramfs_at, initramfs.len, *initramfs_padding),
                ])
            }
            _ => Err(invalid("the layout carries no kernel and initramfs".into())),
        }
    }

    /// The ESP's filesystem metadata as `write_to` leaves it: the region it
    /// zeroes, with the FAT's extents written over it, at its disk offset.
    fn metadata_image(&self) -> io::Result<(u64, Vec<u8>)> {
        let at = self
            .plan
            .esp_offset()
            .ok_or_else(|| invalid("the ESP offset overflowed".into()))?;
        let len = usize::try_from(self.metadata)
            .map_err(|_| invalid("the ESP metadata exceeds this address space".into()))?;
        let mut image = vec![0u8; len];
        for extent in &self.esp.extents {
            let start = usize::try_from(extent.offset)
                .map_err(|_| invalid("an ESP extent exceeds this address space".into()))?;
            let slot = start
                .checked_add(extent.bytes.len())
                .and_then(|end| image.get_mut(start..end))
                .ok_or_else(|| invalid("an ESP extent lies past its metadata".into()))?;
            slot.copy_from_slice(&extent.bytes);
        }
        Ok((at, image))
    }

    /// The refusals `write_to` makes before its first write: the destination's
    /// geometry and the EFI inputs' lengths are still those prepared.
    fn check_unchanged(&self, destination: &mut File) -> io::Result<()> {
        let disk_bytes = destination_bytes(destination)?;
        let sector_size = logical_sector_size(destination)?;
        if plan(sector_size, disk_bytes).map_err(invalid)? != self.plan {
            return Err(invalid(
                "destination geometry changed during format preparation".into(),
            ));
        }
        for (input, _, _, _) in &self.payloads {
            if input.file.metadata()?.len() != input.len {
                return Err(invalid(format!(
                    "EFI input changed size before layout: {}",
                    input.path.display()
                )));
            }
        }
        Ok(())
    }

    fn write_to(self, destination: &mut FormatDestination, out: &mut dyn Write) -> io::Result<()> {
        self.check_unchanged(&mut destination.file)?;
        let Self {
            plan,
            table,
            esp,
            metadata,
            payloads,
        } = self;
        let file = &mut destination.file;
        let label = destination.label.as_path();
        let esp_offset = plan
            .esp_offset()
            .ok_or_else(|| invalid("the ESP offset overflowed".into()))?;

        // A REINSTALL is the case this order exists for. On a disk that already
        // carries a table, that table stays valid while the ESP beneath it is being
        // rewritten, so an install that dies part way leaves a table pointing at a
        // filesystem that is half replaced — which is worse than no table, because
        // firmware will try it. So the old table goes FIRST and the disk spends the
        // install carrying none.
        //
        // Each stage is flushed before the next. Nothing else orders one write
        // against another across a power cut: without the barriers the table can
        // reach the platter before the filesystem it describes. The primary table
        // is written LAST because it is the commit point — it is what firmware
        // reads first, and it is only correct once everything it points at is
        // durable.
        invalidate_table(file, &table)?;
        file.sync_all()?;

        zero_at(file, esp_offset, metadata)?;
        for extent in &esp.extents {
            let at = esp_offset
                .checked_add(extent.offset)
                .ok_or_else(|| invalid("an ESP extent overflowed".to_string()))?;
            write_at(file, at, &extent.bytes)?;
        }
        for (mut input, offset, end, padding) in payloads {
            file.seek(SeekFrom::Start(offset))?;
            input.copy_to(file, label)?;
            // A partial final cluster must not disclose bytes from a previous ESP.
            zero_at(file, end, padding)?;
        }
        file.sync_all()?;

        write_at(file, table.backup_offset, &table.backup)?;
        file.sync_all()?;
        write_at(file, table.primary_offset, &table.primary)?;
        file.sync_all()?;

        // NUMBERS ONLY, whitespace-separated, and every one a BYTE OFFSET. The
        // destination is deliberately not echoed back: a caller already knows what
        // it passed, and a path is the one field here that can contain a space —
        // which shifts every field a caller reads by position — or a newline, which
        // would break the one-line promise outright. Nothing that can carry either
        // goes on this channel.
        //
        // Bytes rather than the LBAs this function works in, because `volume`
        // reports bytes and two verbs of one program reporting the same-shaped line
        // in different units is a caller reading 2048 where the ESP is at 1048576 —
        // with nothing on either line to say which it got.
        let esp = plan
            .esp_offset()
            .ok_or_else(|| invalid("the ESP offset overflowed".to_string()))?;
        let volume = plan
            .volume_start
            .checked_mul(plan.sector_size)
            .ok_or_else(|| invalid("the volume offset overflowed".to_string()))?;
        writeln!(out, "{esp} {volume}")
    }
}

/// The two byte ranges a table occupies, as `(offset, len)` pairs.
///
/// The same positions `gpt::build` writes to, derived rather than remembered:
/// the primary runs from LBA 0 through the end of its entry array (which is
/// `first_usable_lba`), and the backup is the entry array plus the header on
/// the LAST sector.
fn table_ranges(sector_size: u64, disk_sectors: u64) -> Result<[(u64, u64); 2], String> {
    let entries = gpt::entry_array_sectors(sector_size)?;
    let primary_sectors = gpt::first_usable_lba(sector_size)?;
    let backup_sectors = entries
        .checked_add(1)
        .ok_or_else(|| "the backup table length overflowed".to_string())?;
    let backup_start = disk_sectors
        .checked_sub(backup_sectors)
        .ok_or_else(|| "the disk is too small to hold a backup table".to_string())?;
    let bytes = |sectors: u64| {
        sectors
            .checked_mul(sector_size)
            .ok_or_else(|| "a table range overflowed".to_string())
    };
    Ok([
        (0, bytes(primary_sectors)?),
        (bytes(backup_start)?, bytes(backup_sectors)?),
    ])
}

fn read_at(file: &mut File, offset: u64, len: u64) -> io::Result<Vec<u8>> {
    let len = usize::try_from(len)
        .map_err(|_| invalid("a table range exceeds this address space".to_string()))?;
    let mut bytes = vec![0u8; len];
    file.seek(SeekFrom::Start(offset))?;
    file.read_exact(&mut bytes)?;
    Ok(bytes)
}

/// Where the td volume is, read back off the disk rather than recomputed.
///
/// `plan()` would answer the same for a disk this installer laid out, and that
/// is exactly why it is not asked: the partition the filesystem goes in must be
/// the one the TABLE describes, or a `plan` that changed between the layout and
/// the volume would write a filesystem outside its own partition. So the table
/// is parsed — which also refuses a destination that was never laid out, and a
/// disk whose two copies of the table disagree.
fn volume_region(file: &mut File, sector_size: u64, disk_sectors: u64) -> io::Result<(u64, u64)> {
    let [primary, backup] = table_ranges(sector_size, disk_sectors).map_err(invalid)?;
    let primary = read_at(file, primary.0, primary.1)?;
    let backup = read_at(file, backup.0, backup.1)?;
    let table = gpt::parse(&primary, &backup, sector_size).map_err(invalid)?;
    // The table must be a table OF THIS DISK. `gpt::parse` is handed two byte
    // slices and never learns where they came from, so it cannot tell; here the
    // real count is known, and a header describing a different-sized disk is a
    // table that was copied from one rather than written on this one.
    if table.disk_sectors != disk_sectors {
        return Err(invalid(format!(
            "the table describes a {}-sector disk, not the {disk_sectors} sectors \
             this destination has",
            table.disk_sectors
        )));
    }
    // A loop rather than the searching iterator adaptor, whose name this file
    // may not spell: it is staged into a recipe as a `WriteFile` body, and the
    // ladder's host-tool guard tokenises those bodies and reads that name as an
    // invocation of the GNU tool it shares
    // (`no_bootstrap_step_invokes_host_find_or_xargs`).
    // A NAME is not an identity: GPT does not require partition names to be
    // unique, so a table carrying two of them is one this program cannot choose
    // between and must not guess at — the wrong choice formats a partition
    // somebody else's data is in. The TYPE is checked with it for the same
    // reason, since a name is 36 characters anyone can write and the type GUID
    // is what says what the partition is FOR.
    let mut found = None;
    let mut matches = 0usize;
    for part in &table.partitions {
        if part.name == protocol::VOLUME_PARTITION_NAME && part.type_guid == gpt::TYPE_LINUX_FS {
            matches += 1;
            if found.is_none() {
                found = Some(part);
            }
        }
    }
    if matches > 1 {
        return Err(invalid(format!(
            "this disk has {matches} partitions named {}",
            protocol::VOLUME_PARTITION_NAME
        )));
    }
    let part = found.ok_or_else(|| {
        invalid(format!(
            "no {} partition on this disk — run `layout` first",
            protocol::VOLUME_PARTITION_NAME
        ))
    })?;
    let offset = part
        .start_lba
        .checked_mul(sector_size)
        .ok_or_else(|| invalid("the volume offset overflowed".to_string()))?;
    // INCLUSIVE end, as GPT stores it.
    let len = part
        .end_lba
        .checked_sub(part.start_lba)
        .and_then(|span| span.checked_add(1))
        .and_then(|sectors| sectors.checked_mul(sector_size))
        .ok_or_else(|| invalid("the volume length overflowed".to_string()))?;
    // ...and the region must not overlap either copy of the TABLE that named
    // it. `gpt::parse` bounds partitions by the header's OWN `first_usable` and
    // `last_usable`, which are fields in the same table — so a table declaring
    // a usable range over its own entry array is self-consistent, and this is
    // the only place the REAL positions are known. A volume overlapping them is
    // an install that destroys the table on its way to using it.
    let [primary, backup] = table_ranges(sector_size, disk_sectors).map_err(invalid)?;
    let primary_end = primary.0.saturating_add(primary.1);
    let end = offset.saturating_add(len);
    if offset < primary_end || end > backup.0 {
        return Err(invalid(format!(
            "the volume at {offset}..{end} overlaps a partition table \
             ({}..{primary_end} and {}..)",
            primary.0, backup.0
        )));
    }
    Ok((offset, len))
}

/// The unit both the sparse copy and the edge zeroing work in. Named once
/// because `run_volume` orders the copy by it: the chunk it defers has to be
/// the one holding the superblock, and two spellings of a megabyte could
/// disagree about which that is.
const COPY_CHUNK: u64 = 1024 * 1024;

/// Copy the parts of `image` that are not all zero to `offset` in `file`.
///
/// A freshly made Btrfs is nearly all hole, so copying the holes would be a
/// write of the whole volume — on a 100 GB partition, minutes of writes to say
/// nothing. Reading them costs no I/O: a hole reads from the zero page.
///
/// What that skip gives up is stated in DESIGN §10 item 7 and is why the
/// caller zeroes the region's first bytes: a chunk the image leaves as a hole
/// is a chunk the destination KEEPS, so a signature mkfs erased by writing
/// zeros would survive here, being indistinguishable from the holes around it.
///
/// Returns the bytes actually written, which the caller reports — an install
/// that copied nothing is one whose mkfs wrote nothing.
///
/// `from`..`to` is a range WITHIN the image, so the caller can order the copy;
/// see `run_volume` for why the first chunk goes last.
fn copy_sparse(
    image: &mut File,
    file: &mut File,
    offset: u64,
    from: u64,
    to: u64,
) -> io::Result<u64> {
    let span = usize::try_from(COPY_CHUNK).unwrap_or(1).max(1);
    let mut buffer = vec![0u8; span];
    // Compared as slices rather than scanned byte by byte: slice equality on
    // bytes is std's `memcmp`, which is optimized however this crate was
    // compiled, and this loop reads every byte of a volume measured in
    // gigabytes. An unoptimized per-byte scan cost the test suite tens of
    // seconds a test.
    let zeros = vec![0u8; span];
    let mut at = from;
    let mut written = 0u64;
    image.seek(SeekFrom::Start(from))?;
    while at < to {
        let take = usize::try_from(to.saturating_sub(at).min(span as u64)).unwrap_or(span);
        let chunk = buffer
            .get_mut(..take)
            .ok_or_else(|| invalid("copy chunk out of range".to_string()))?;
        image.read_exact(chunk)?;
        let blank = zeros
            .get(..take)
            .ok_or_else(|| invalid("blank chunk out of range".to_string()))?;
        if chunk != blank {
            let dest = offset
                .checked_add(at)
                .ok_or_else(|| invalid("a copy offset overflowed".to_string()))?;
            write_at(file, dest, chunk)?;
            written = written.saturating_add(take as u64);
        }
        at = at.saturating_add(take as u64);
    }
    Ok(written)
}

/// Zero both ENDS of the region before the copy lands on it.
///
/// This is the whole of what the sparse copy gives up. mkfs erases a previous
/// filesystem's signature by WRITING ZEROS, and zeros in a fresh sparse image
/// are holes the copy skips — so a signature the new filesystem believes it
/// erased survives underneath it, and a prober that finds two says the disk is
/// ambiguous or, worse, assembles the older one.
///
/// BOTH ends, because "the first megabyte covers every signature" is false: XFS
/// at 0, ext* at 1 KiB and Btrfs at 64 KiB are all at the front, but MD RAID
/// 0.90 and 1.0 metadata and ZFS's L2/L3 labels sit in the LAST few hundred
/// kilobytes of the device. An alignment's worth at each end covers both sets,
/// and costs two megabytes against a partition measured in gigabytes.
///
/// Btrfs's own superblock mirrors need no such care: they are at fixed offsets
/// and the new mkfs writes every one this volume is large enough to hold.
fn zero_edges(file: &mut File, offset: u64, len: u64) -> io::Result<()> {
    let edge = protocol::PARTITION_ALIGN_BYTES.min(len);
    // A region too small to hold two disjoint edges is zeroed once, whole,
    // rather than twice over its own middle.
    if len <= edge.saturating_mul(2) {
        return zero_at(file, offset, len);
    }
    zero_at(file, offset, edge)?;
    let tail = offset
        .checked_add(len)
        .and_then(|end| end.checked_sub(edge))
        .ok_or_else(|| invalid("the volume tail overflowed".to_string()))?;
    zero_at(file, tail, edge)
}

/// Read the trusted key under td-boot's rule, which is now literally
/// td-boot's: `realfile.rs` is one implementation both crates include.
///
/// Applied here rather than left to td-boot because this program SNAPSHOTS
/// the key and hands td-boot the copy — a copy is a small regular file
/// whatever the original was, so without this the snapshot would launder a
/// key past every refusal the real reader makes.
fn read_trusted_key(path: &Path) -> io::Result<Vec<u8>> {
    realfile::read_bounded_real_file(
        path,
        "trusted deployment key",
        protocol::MAX_PUBLIC_KEY_BYTES,
    )
}

/// Initialize trust directories and optionally publish through `td-boot`.
///
/// D1: this crate does not learn to write a deployment directory, update a
/// selector, or account for attempts — it hands the whole transaction to the
/// one writer. What it does here is make the directories that writer requires,
/// which are the LAYOUT rather than the transaction. Everything this function
/// shares with `td-boot` — the two nested directory names, the VERB, and the
/// shape of a deployment id — is `protocol.rs`'s, for that file's own stated
/// reason: a thing spelled in both crates is a thing they can come to disagree
/// about, at the first boot after an install rather than at build time.
///
/// The child's stdout is the deployment id, and it is replayed on OUR stderr
/// for `mkfs.btrfs`'s reason: this program's stdout is a machine-readable line
/// of byte offsets, and an id is neither a byte offset nor something a caller
/// reading by position expects to see there. It is also READ, which is the
/// whole of what stops a successful exit standing in for a publish — see below.
fn seed_into(staging: &Path, seed: &VolumeSeed, key: &[u8]) -> io::Result<()> {
    // The four `install_deployment` requires, in its order and its spelling —
    // `td` is a literal there too, and the two nested constants make it
    // redundant only for as long as they stay under it. Mirroring the check
    // rather than deriving from it is what makes a moved constant a missing
    // directory td-boot names, instead of one this loop silently stopped
    // creating.
    //
    // MODE PINNED rather than left to the ambient umask, because `--rootdir` copies a
    // staging directory's mode into the filesystem verbatim: these are baked
    // onto a machine's disk, not scratch. Under `umask 000` the selector
    // directory — which holds the `current`/`previous` symlinks the boot path
    // follows — shipped as 0777, world-writable on the installed system, and
    // nothing downstream pins it: td-boot pins the mode of everything it
    // writes ITSELF, and `require_real_directory` asks only whether these are
    // directories. It also makes two installs of the same inputs produce the
    // same image, which is the oracle's comparison.
    //
    // What this does NOT fix is OWNERSHIP: the tree is owned by whoever ran
    // the installer, and changing that needs `chown`, which is a syscall this
    // crate deliberately does not have (DESIGN D8). An installer runs as root
    // on the path that matters, where the answer is already right.
    // SET rather than passed to `mkdir`, because a creation mode is masked by
    // the umask and a `chmod` is not: `DirBuilder::mode(0o755)` still yields
    // 0700 under `umask 077`, which is a different image for the same inputs.
    // `VOLUME_CHANNEL_DIR` is here because `td-boot update` treats a MISSING
    // channel as a configuration fault rather than as nothing to do — which is
    // right, and which means an installed machine whose channel was never
    // created fails every timer tick. Empty is the correct initial state: the
    // machine has an update channel and nothing has been offered in it yet.
    for directory in [
        "td",
        protocol::BOOT_DIR,
        protocol::DEPLOYMENTS_DIR,
        protocol::VOLUME_CHANNEL_DIR,
    ] {
        let path = staging.join(directory);
        paths::create_dir_all(&path)?;
        paths::set_mode(&path, 0o755)?;
    }
    // The key is SNAPSHOT and td-boot is handed the snapshot, so the file that
    // authenticates the bundle is the file the volume keeps — two reads of one
    // path are two chances for it to say different things, and nothing would
    // report the disagreement. Beside the staging tree rather than in it, so
    // td-boot never reads a trust root out of the volume root it is writing.
    // `staging` is canonical, so its parent is the scratch directory.
    //
    // In a directory of its own at 0700, because the scratch directory is not
    // private and `td-trusted.pub` is a guessable name: everything below
    // narrows the window in which the snapshot can be swapped, and a directory
    // nothing else may write into is what removes it rather than narrowing it.
    // Restricting needs no chmod after the fact — a umask can only take mode
    // bits away, and 0700 is already the fewest this needs — which is the
    // reverse of the widening two blocks down.
    let private = staging
        .parent()
        .ok_or_else(|| {
            invalid(format!(
                "the staging tree {} has no parent",
                staging.display()
            ))
        })?
        .join("td-install-key");
    paths::remove_dir_all_if_present(&private)?;
    paths::create_dir_with_mode(&private, 0o700)?;
    let snapshot = private.join("td-trusted.pub");
    let identity;
    {
        use std::os::unix::fs::PermissionsExt;
        // Created 0600 and WIDENED, not created 0644: a creation mode is
        // masked by the umask, so 0644 asked for directly is 0600 under `umask
        // 077` with nothing to correct it — and the other order would leave a
        // window where a machine's trust root is world-writable.
        //
        // Widened through the open DESCRIPTOR, not the path: a path-based
        // chmod follows a symlink, so one swapped in after the write would
        // take the 0644 instead — the same race `create_new` closes at the
        // other end, and closing only one end closes neither.
        let mut file = paths::create_new_with_mode(&snapshot, 0o600)?;
        file.write_all(key)?;
        file.set_permissions(std::fs::Permissions::from_mode(0o644))?;
        use std::os::linux::fs::MetadataExt;
        let written = file.metadata()?;
        identity = (written.st_dev(), written.st_ino());
    }
    if let VolumeSeed::Publish(publish) = seed {
        let output = std::process::Command::new(&publish.td_boot)
            .arg(protocol::PUBLISH_VERB)
            .arg(staging)
            .arg(&publish.deployment)
            .arg(&snapshot)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .output()
            // A failure to SPAWN reports the errno and no path, so a mistyped or
            // unbuilt `td-boot` says only `No such file or directory` — on a
            // command line that names four other paths any of which a reader would
            // suspect first.
            .map_err(|error| {
                invalid(format!("cannot run {}: {error}", publish.td_boot.display()))
            })?;
        let _ = io::stderr().write_all(&output.stdout);
        if !output.status.success() {
            return Err(invalid(format!(
                "{} publish failed ({})",
                publish.td_boot.display(),
                output.status
            )));
        }
        // A SUCCESSFUL EXIT IS NOT A PUBLISH. Nothing about a zero status says a
        // deployment landed, and the failure that hides behind one is the worst
        // this verb has: a complete, correct, mountable volume with an empty
        // `td/deployments` — a disk that installs, formats, reports its offsets and
        // then cannot boot, discovered by the machine rather than by the installer.
        // So the id the child prints is READ BACK against the tree it claims to
        // have written, which is a fact this crate already has: it made the
        // directory the id has to appear in.
        //
        // This is not the crate learning the transaction (D1) — it does not know
        // what a deployment CONTAINS, only that the writer named one and that the
        // name resolves. `valid_digest` is `protocol.rs`'s so the shape is stated
        // once, and it is checked BEFORE the join rather than after: an id is
        // otherwise a path component out of a program's stdout, and `..` in it
        // would answer this question with a directory outside the staging tree.
        let id = std::str::from_utf8(&output.stdout)
            .map_err(|_| {
                invalid(format!(
                    "{} printed a deployment id that is not ASCII",
                    publish.td_boot.display()
                ))
            })?
            .trim();
        if !protocol::valid_digest(id.as_bytes()) {
            return Err(invalid(format!(
                "{} published no deployment id ({id:?})",
                publish.td_boot.display()
            )));
        }
        let published = staging.join(protocol::DEPLOYMENTS_DIR).join(id);
        if !paths::is_dir(&published)? {
            return Err(invalid(format!(
                "{} reported {id} but {} is not there",
                publish.td_boot.display(),
                published.display()
            )));
        }
    }
    // The volume keeps the key, so the machine this installs can authenticate
    // its own updates: it has none otherwise, since `TRUSTED_KEY_PATH` is the
    // SELECTOR initramfs's copy and `switch_root` replaces that rootfs
    // (DESIGN §10 item 10a).
    //
    // A publishing seed promotes only after authentication succeeds. A trust-only
    // seed provisions bytes for the later mounted publisher; it asserts no
    // authenticated deployment. Rename preserves the snapshot's identity.
    //
    // A rename moves whatever the path names AT RENAME TIME, though, not the
    // file td-boot just read, so the inode is checked against the one written
    // above — the `st_dev`/`st_ino` comparison the scratch image already makes
    // one screen down. The 0700 directory is what makes this a check nothing
    // is expected to trip; it is here because the alternative to tripping it
    // is a volume carrying a key that authenticated nothing, which is the one
    // outcome this whole path exists to prevent.
    {
        use std::os::linux::fs::MetadataExt;
        let now = paths::symlink_metadata(&snapshot)?;
        if !now.is_file() || (now.st_dev(), now.st_ino()) != identity {
            return Err(invalid(format!(
                "the trusted key {} was replaced while the volume was seeded",
                snapshot.display()
            )));
        }
    }
    let destination = staging.join(protocol::VOLUME_TRUSTED_KEY);
    paths::rename(&snapshot, &destination)?;
    Ok(())
}

const TIMEZONE_ROOT: &str = "/etc/zoneinfo";
const TIMEZONE_STATE_RELATIVE: &str = "lib/td/timezone";
const HOSTNAME_STATE_RELATIVE: &str = "lib/td/hostname";
const USERNAME_STATE_RELATIVE: &str = "lib/td/username";

fn seed_timezone(subvol: &Path, timezone: &timezones::Selection) -> io::Result<()> {
    seed_setting(subvol, TIMEZONE_STATE_RELATIVE, timezone.id())
}

fn seed_setting(subvol: &Path, relative: &str, value: &str) -> io::Result<()> {
    for directory in ["lib", "lib/td"] {
        let path = subvol.join(directory);
        paths::create_dir_all(&path)?;
        paths::set_mode(&path, 0o755)?;
    }
    let path = subvol.join(relative);
    let mut file = paths::create_new_with_mode(&path, 0o644)?;
    writeln!(file, "{value}").map_err(|error| {
        io::Error::new(error.kind(), format!("write {}: {error}", path.display()))
    })?;
    paths::set_mode(&path, 0o644)?;
    file.sync_all()
        .map_err(|error| io::Error::new(error.kind(), format!("sync {}: {error}", path.display())))
}

#[derive(Default)]
struct VolumeSettings<'a> {
    timezone: Option<&'a timezones::Selection>,
    hostname: Option<&'a hostname::Hostname>,
    username: Option<&'a PrimarySelection>,
}

fn run_volume(
    settings: VolumeSettings<'_>,
    uuid: Option<&VolumeUuid>,
    destination: &Path,
    mkfs: &Path,
    scratch: &Path,
    seed: Option<&VolumeSeed>,
    out: &mut dyn Write,
) -> io::Result<()> {
    let prepared = prepare_volume(settings, uuid, mkfs, scratch, seed)?;
    let mut destination = FormatDestination::open(destination)?;
    format_volume(prepared, &mut destination, out)
}

/// Prepare both filesystems before the first destination write.
fn run_format(
    prepared: PreparedVolume<'_>,
    destination: &Path,
    boot: &BootFiles,
    out: &mut dyn Write,
) -> io::Result<()> {
    let mut destination = FormatDestination::open(destination)?;
    format_held(prepared, &mut destination, boot, None, out, &mut |_| {})
        .map(drop)
        .map_err(HeldFailure::into_error)
}

/// The step a held format is about to begin.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FormatStep {
    /// The first destination write.
    Writing,
    /// Publication through the loop.
    Publishing,
}

/// Where a held format stopped: laying out the disk or staging the volume,
/// both before its first destination write, or after it.
#[derive(Debug)]
enum HeldFailure {
    Layout(io::Error),
    Staging(io::Error),
    Written(io::Error),
}

impl HeldFailure {
    fn into_error(self) -> io::Error {
        match self {
            Self::Layout(error) | Self::Staging(error) | Self::Written(error) => error,
        }
    }
}

/// What a held format wrote, for a caller to read back.
#[derive(Debug)]
struct Formatted {
    /// The volume's `(offset, len)` as the layout placed it.
    volume: (u64, u64),
    /// Both copies of the table, as written.
    table: gpt::Image,
    /// The ESP's filesystem metadata region, as written, at its offset.
    metadata: (u64, Vec<u8>),
    /// The kernel's and initramfs's `(offset, len, padding)` in the ESP.
    boot: [(u64, u64, u64); 2],
    /// td-boot's standard output, when the seed published through the loop:
    /// the caller reads the deployment id out of it.
    published: Option<Vec<u8>>,
}

/// Format the destination this process already holds, telling `step` as each
/// step begins.
fn format_held(
    prepared: PreparedVolume<'_>,
    destination: &mut FormatDestination,
    boot: &BootFiles,
    digests: Option<&[[u8; 32]; 2]>,
    out: &mut dyn Write,
    step: &mut dyn FnMut(FormatStep),
) -> Result<Formatted, HeldFailure> {
    let layout = prepare_layout(destination, Some(boot), digests).map_err(HeldFailure::Layout)?;
    let len = layout
        .plan
        .volume_bytes()
        .ok_or_else(|| HeldFailure::Layout(invalid("planned volume length overflowed".into())))?;
    let volume = layout
        .plan
        .volume_start
        .checked_mul(layout.plan.sector_size)
        .map(|offset| (offset, len))
        .ok_or_else(|| HeldFailure::Layout(invalid("planned volume offset overflowed".into())))?;
    let boot_extents = layout.boot_extents().map_err(HeldFailure::Layout)?;
    let metadata = layout.metadata_image().map_err(HeldFailure::Layout)?;
    let table = layout.table.clone();
    let seed = prepared.seed;
    let image = prepare_volume_image(prepared, destination, len).map_err(HeldFailure::Staging)?;
    // Guard layout too; write_to keeps its own check for standalone volume use.
    image
        .check_source(&destination.file)
        .map_err(HeldFailure::Staging)?;
    // Both refuse again inside their writes; asked here, a refusal is still
    // before the step that reports writing.
    layout
        .check_unchanged(&mut destination.file)
        .map_err(HeldFailure::Layout)?;
    step(FormatStep::Writing);
    layout
        .write_to(destination, &mut io::sink())
        .map_err(HeldFailure::Written)?;
    let Some((key, publish)) = seed.and_then(|seed| {
        seed.through_loop()
            .map(|publish| (seed.trusted_key(), publish))
    }) else {
        return image
            .write_to(destination, out)
            .map(|()| Formatted {
                volume,
                table,
                metadata,
                boot: boot_extents,
                published: None,
            })
            .map_err(HeldFailure::Written);
    };
    // The report waits for publication: a caller reading it has a published
    // disk.
    let mut report = Vec::new();
    image
        .write_to(destination, &mut report)
        .map_err(HeldFailure::Written)?;
    step(FormatStep::Publishing);
    let stdout = publish_through_loop(destination, publish, key).map_err(HeldFailure::Written)?;
    out.write_all(&report).map_err(HeldFailure::Written)?;
    Ok(Formatted {
        volume,
        table,
        metadata,
        boot: boot_extents,
        published: Some(stdout),
    })
}

/// The boot artifacts as the disk holds them once installed (INSTALLER.md
/// "complete"), against what the layout wrote: both copies of the table
/// byte for byte, so the ESP's entry with them; the ESP's filesystem
/// metadata byte for byte, the directories firmware resolves the files by
/// among it; each file hashing to `digests` and its cluster's rest zeroed.
/// Read back through the claim, so this checks what was placed where, not
/// the medium: the reads may be served from the kernel's cache, and
/// durability rests on the syncs before them.
fn verify_boot(file: &mut File, formatted: &Formatted, digests: &[[u8; 32]; 2]) -> io::Result<()> {
    let table = &formatted.table;
    for (copy, offset, bytes) in [
        ("primary", table.primary_offset, &table.primary),
        ("backup", table.backup_offset, &table.backup),
    ] {
        if read_at(file, offset, bytes.len() as u64)? != *bytes {
            return Err(invalid(format!(
                "the installed {copy} table is not the one written"
            )));
        }
    }
    if destination_volume(file)? != formatted.volume {
        return Err(invalid(
            "the installed table does not place the volume where the layout did".into(),
        ));
    }
    let (at, metadata) = &formatted.metadata;
    if read_at(file, *at, metadata.len() as u64)? != *metadata {
        return Err(invalid(
            "the installed ESP's filesystem metadata is not what was written".into(),
        ));
    }
    for ((name, (offset, len, padding)), digest) in ["kernel", "initramfs"]
        .into_iter()
        .zip(formatted.boot)
        .zip(digests)
    {
        let read = |error: io::Error| {
            io::Error::new(error.kind(), format!("read back the ESP {name}: {error}"))
        };
        if digest_range(file, offset, len).map_err(read)? != *digest {
            return Err(invalid(format!(
                "the installed ESP {name} is not the one meant for it"
            )));
        }
        let rest = offset
            .checked_add(len)
            .ok_or_else(|| invalid(format!("the ESP {name} overflowed")))?;
        if read_at(file, rest, padding)
            .map_err(read)?
            .iter()
            .any(|byte| *byte != 0)
        {
            return Err(invalid(format!(
                "the rest of the installed ESP {name}'s last cluster is not zeroed"
            )));
        }
    }
    Ok(())
}

/// Verifying boot (DESIGN.md "Executing a consented installation"): td-boot
/// published the plan's `deployment`, and the boot artifacts read back as
/// written. An installation that fails either is withdrawn from firmware by
/// invalidating its table again, as `write_to` does before it writes: a
/// table over a disk that cannot be trusted is worse than none.
fn finish_installation(
    file: &mut File,
    formatted: &Formatted,
    deployment: &str,
    digests: &[[u8; 32]; 2],
) -> io::Result<()> {
    let verified = formatted
        .published
        .as_deref()
        .and_then(published_id)
        .ok_or_else(|| invalid("td-boot printed no deployment id".into()))
        .and_then(|id| {
            if id == deployment {
                Ok(())
            } else {
                Err(invalid(format!(
                    "td-boot published {id}, not deployment {deployment}"
                )))
            }
        })
        .and_then(|()| verify_boot(file, formatted, digests));
    let Err(error) = verified else {
        return Ok(());
    };
    match invalidate_table(file, &formatted.table).and_then(|()| file.sync_all()) {
        Ok(()) => Err(error),
        Err(withdrawal) => Err(io::Error::new(
            error.kind(),
            format!("{error}; and withdrawing its table failed: {withdrawal}"),
        )),
    }
}

/// Publish onto the volume just written, through a loop over the destination
/// this process holds. The kernel's partition table may still describe the
/// disk as it was, and the claim refuses a mount of any partition node, so
/// the volume is reached by its byte range instead; the loop holds the claim's
/// own open file, so the claim lasts until the loop clears.
fn publish_through_loop(
    destination: &mut FormatDestination,
    publish: &LoopPublish,
    key: &Path,
) -> io::Result<Vec<u8>> {
    let sector_size = logical_sector_size(&destination.file)?;
    let (offset, len) = destination_volume(&mut destination.file)?;
    let device =
        loop_device::attach(&destination.file, offset, len, sector_size).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!(
                    "{}: loop over the volume: {error}",
                    destination.label.display()
                ),
            )
        })?;
    // td-boot's stdout is the deployment id; this program's is a report.
    let output = std::process::Command::new(&publish.td_boot)
        .arg("install")
        .arg(device.path())
        .arg(&publish.mountpoint)
        .arg(&publish.deployment)
        .arg(key)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .output()
        .map_err(|error| invalid(format!("cannot run {}: {error}", publish.td_boot.display())))?;
    let _ = io::stderr().write_all(&output.stdout);
    if !output.status.success() {
        return Err(invalid(format!(
            "{} install on {} failed ({})",
            publish.td_boot.display(),
            device.path().display(),
            output.status
        )));
    }
    // td-boot has unmounted, so this is the last opener and the loop clears;
    // release confirms it did, so no loop outlives a reported success.
    device.release()?;
    destination.file.sync_all()?;
    Ok(output.stdout)
}

/// td-boot's install output: one deployment id and a newline.
fn published_id(stdout: &[u8]) -> Option<String> {
    let id = stdout.strip_suffix(b"\n")?;
    protocol::valid_digest(id)
        .then(|| std::str::from_utf8(id).ok().map(str::to_owned))
        .flatten()
}

struct PreparedVolume<'a> {
    settings: VolumeSettings<'a>,
    uuid: Option<&'a VolumeUuid>,
    mkfs: &'a Path,
    scratch: &'a Path,
    seed: Option<&'a VolumeSeed>,
    key: Option<Vec<u8>>,
}

/// Validate caller-bound inputs before acquiring or changing the destination.
fn prepare_volume<'a>(
    settings: VolumeSettings<'a>,
    uuid: Option<&'a VolumeUuid>,
    mkfs: &'a Path,
    scratch: &'a Path,
    seed: Option<&'a VolumeSeed>,
) -> io::Result<PreparedVolume<'a>> {
    // `Command::new` SEARCHES `PATH` for a name with no separator in it, which
    // is the ambient resolution the declared-input contract above rules out —
    // and the one form of it a caller cannot see they asked for.
    //
    // All supplied programs are checked before anything is opened or removed. An
    // argv-shaped mistake should cost nothing, and the td-boot check began life
    // inside `seed_into` — which runs after the destination is open and,
    // worse, after the caller's staging tree has been emptied. A bare name in
    // the fourth argument therefore destroyed a directory before saying it did
    // not like the fourth argument.
    let publish = seed.and_then(VolumeSeed::publish);
    let through_loop = seed.and_then(VolumeSeed::through_loop);
    for (label, program) in [
        ("mkfs.btrfs", Some(mkfs)),
        ("td-boot", publish.map(|publish| publish.td_boot.as_path())),
        (
            "td-boot",
            through_loop.map(|publish| publish.td_boot.as_path()),
        ),
        (
            "td-firstboot",
            settings
                .username
                .map(|selection| selection.firstboot.as_path()),
        ),
    ] {
        if let Some(program) = program {
            if !program.is_absolute() {
                // The value is QUOTED because the label and it collide in the
                // likely mistake: passing the conventional bare name gives
                // `mkfs.btrfs mkfs.btrfs is not an absolute path`, which reads
                // as a typo in the diagnostic rather than as the argument.
                return Err(invalid(format!(
                    "{label} {:?} is not an absolute path, and a bare name resolves \
                     through PATH",
                    program.display()
                )));
            }
        }
    }
    // td-boot takes these two in another order than `--publish` does, so a
    // swap is the likely mistake, and it would surface only after the disk
    // was written. Both are refused here instead: the deployment must be a
    // directory, and the mountpoint an empty one.
    if let Some(publish) = through_loop {
        if !paths::is_dir(&publish.deployment)? {
            return Err(invalid(format!(
                "deployment {} is not a directory",
                publish.deployment.display()
            )));
        }
        if !paths::is_dir(&publish.mountpoint)?
            || paths::read_dir_bounded(&publish.mountpoint, 0).is_err()
        {
            return Err(invalid(format!(
                "mountpoint {} is not an empty directory",
                publish.mountpoint.display()
            )));
        }
    }
    // The KEY is read here for the same reason and in the same place. It is an
    // argv-shaped mistake like the two above — a path that is not there, or is
    // not a key — and reading it inside `seed_into` put the refusal after
    // the staging tree had been emptied, so a mistyped fifth argument
    // destroyed a directory before saying it did not like the fifth argument.
    // That is verbatim the failure the paragraph above records for the fourth.
    let key = seed
        .map(|seed| read_trusted_key(seed.trusted_key()))
        .transpose()?;
    if let Some(selection) = settings.username {
        selection.check()?;
    }
    Ok(PreparedVolume {
        settings,
        uuid,
        mkfs,
        scratch,
        seed,
        key,
    })
}

fn format_volume(
    prepared: PreparedVolume<'_>,
    destination: &mut FormatDestination,
    out: &mut dyn Write,
) -> io::Result<()> {
    // Size staging from this table; write_to rechecks the table at the write boundary.
    let (_, len) = destination_volume(&mut destination.file)?;
    let image = prepare_volume_image(prepared, destination, len)?;
    image.write_to(destination, out)
}

fn destination_volume(file: &mut File) -> io::Result<(u64, u64)> {
    let disk_bytes = destination_bytes(file)?;
    let sector_size = logical_sector_size(file)?;
    if !disk_bytes.is_multiple_of(sector_size) {
        return Err(invalid(format!(
            "destination is {disk_bytes} bytes, not a whole number of \
             {sector_size}-byte sectors"
        )));
    }
    let (offset, len) = volume_region(file, sector_size, disk_bytes / sector_size)?;
    // The partition the TABLE describes must fit in the destination the table
    // is on. `gpt::parse` cannot check this — it is handed two byte slices and
    // never learns where they came from — so a header claiming a larger disk
    // than it sits on passes every checksum and puts `td-volume` past the end.
    // On a regular file the copy would then EXTEND it; on a block device it
    // would write over the real backup table before running out of room.
    let end = offset
        .checked_add(len)
        .ok_or_else(|| invalid("the volume region overflowed".to_string()))?;
    if end > disk_bytes {
        return Err(invalid(format!(
            "the table puts the volume at {offset}..{end} on a {disk_bytes}-byte \
             destination"
        )));
    }

    Ok((offset, len))
}

/// Admitted source bytes; ownership separates staging from destination writes.
struct PreparedVolumeImage {
    file: File,
    len: u64,
}

fn prepare_volume_image(
    prepared: PreparedVolume<'_>,
    destination: &FormatDestination,
    len: u64,
) -> io::Result<PreparedVolumeImage> {
    let PreparedVolume {
        settings,
        uuid,
        mkfs,
        scratch,
        seed,
        key,
    } = prepared;
    let file = &destination.file;
    // The image is the volume's own size, because `--byte-count` is what the
    // filesystem records as the device it lives on: a smaller one would make a
    // volume that reports less space than the partition it is copied into, and
    // a larger one a volume whose tail is off the end of the partition.
    let image_path = scratch.join("td-volume.img");
    let staging = scratch.join("td-volume-root");
    let subvol = staging.join(protocol::VOLUME_SUBVOL);
    // The staging tree is not a working directory but the volume's CONTENTS:
    // `--rootdir` copies whatever is under it into the filesystem. So it is
    // emptied rather than merely ensured — a scratch directory a previous run
    // or another program left something in would otherwise put that something
    // on a machine's /var, with nothing about the install saying so.
    paths::remove_dir_all_if_present(&staging)?;
    paths::create_dir_all(&subvol)?;
    paths::set_mode(&subvol, 0o755)?;
    if let Some(timezone) = settings.timezone {
        seed_timezone(&subvol, timezone)?;
    }
    if let Some(hostname) = settings.hostname {
        seed_setting(&subvol, HOSTNAME_STATE_RELATIVE, hostname.name())?;
    }
    if let Some(selection) = settings.username {
        seed_setting(&subvol, USERNAME_STATE_RELATIVE, &selection.name)?;
    }
    // BEFORE the mkfs that bakes this tree into the image, which is the whole
    // of why the publish can happen without a mount: `--rootdir` is what puts
    // it in the filesystem.
    //
    // CANONICALIZED first, because `td-boot` requires an absolute volume root
    // and a relative `<scratch-dir>` is otherwise accepted by every other part
    // of this verb — a `volume ./scratch` that worked would start failing the
    // moment a deployment was passed to it, which is a difference between the
    // two forms that nothing about either says. Resolved once and used for
    // `--rootdir` too, so the publish and the filesystem cannot be given two
    // different names for one directory.
    let staging = paths::canonicalize(&staging)?;
    if let Some(seed) = seed {
        let key = key.ok_or_else(|| invalid("the trusted key was not read".to_string()))?;
        seed_into(&staging, seed, &key)?;
    }
    // `File::create` TRUNCATES, so a scratch directory that puts the image on
    // top of the DESTINATION destroys the disk whose table was just read — and
    // reports success, since everything after this writes a filesystem into
    // what is left. Compared by device and inode rather than by name: a symlink
    // or a hard link is the same file under a different path, and `metadata`
    // follows the one while a string comparison sees neither.
    {
        use std::os::unix::fs::MetadataExt;
        if let Some(existing) = paths::metadata_if_present(&image_path) {
            let target = file.metadata()?;
            if (existing.dev(), existing.ino()) == (target.dev(), target.ino()) {
                return Err(invalid(format!(
                    "the scratch image {} is the destination itself",
                    image_path.display()
                )));
            }
        }
    }
    // UNLINK then CREATE NEW, rather than `File::create`, which opens what is
    // there — following a symlink to wherever it points and truncating THAT.
    // The inode check above covers the destination and nothing else, so a
    // `td-volume.img` pointing at some other file or a block device would be
    // truncated and grown to the partition's size, under an installer that is
    // usually root. Removing the ENTRY affects only the link, and `create_new`
    // then refuses anything that appeared in between rather than opening it.
    paths::remove_file_if_present(&image_path)?;
    // Retain the original inode through mkfs so replacement cannot reuse it.
    let staged_image = paths::create_new(&image_path)?;
    staged_image.set_len(len)?;
    let created = staged_image.metadata()?;
    let got = created.len();
    if got != len {
        return Err(invalid(format!(
            "the scratch image is {got} bytes, not the {len} the volume needs"
        )));
    }

    let uuid = match uuid {
        Some(uuid) => uuid.0.clone(),
        None => random_guid()?.to_string(),
    };
    // The child's stdout is CAPTURED and replayed on ours, because this
    // program's stdout is a machine-readable line and mkfs.btrfs opens with a
    // banner. Inherited, that banner is the first line of what a caller parses
    // — which is exactly how this was found, the recipe check reading `v7.0`
    // where it wanted an offset. stderr is inherited, so a failure still says
    // what went wrong as it happens.
    let output = std::process::Command::new(mkfs)
        .arg("--byte-count")
        .arg(len.to_string())
        .arg("--uuid")
        .arg(&uuid)
        .arg("--label")
        .arg(protocol::VOLUME_LABEL)
        // The one directory in the staged root becomes the read-write subvolume
        // the boot path mounts on /var. An empty volume without it is a disk
        // that lays out, formats, and then cannot boot.
        .arg("--rootdir")
        .arg(&staging)
        .arg("--subvol")
        .arg(format!("rw:{}", protocol::VOLUME_SUBVOL))
        .arg(&image_path)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .output()
        .map_err(|error| invalid(format!("cannot run {}: {error}", mkfs.display())))?;
    // `let _`, as every other write to the diagnostic channel in this crate is:
    // a closed or full stderr is not a reason to abandon an install half done,
    // and reporting its ENOSPC would name the wrong disk entirely.
    let _ = io::stderr().write_all(&output.stdout);
    if !output.status.success() {
        return Err(invalid(format!(
            "{} failed on the scratch image ({})",
            mkfs.display(),
            output.status
        )));
    }

    let (image, prepared) = realfile::open_real_file(&image_path, "prepared Btrfs image")?;
    {
        use std::os::unix::fs::MetadataExt;
        if created.dev() != prepared.dev() || created.ino() != prepared.ino() {
            return Err(invalid(format!(
                "prepared Btrfs image was replaced: {}",
                image_path.display()
            )));
        }
    }
    if prepared.len() != len {
        return Err(invalid(format!(
            "prepared Btrfs image {} is {} bytes, not the required {len}",
            image_path.display(),
            prepared.len()
        )));
    }
    // Surface delayed scratch allocation/write errors before erasing a disk.
    image.sync_all()?;
    drop(staged_image);
    Ok(PreparedVolumeImage { file: image, len })
}

impl PreparedVolumeImage {
    fn check_source(&self, destination: &File) -> io::Result<()> {
        use std::os::unix::fs::MetadataExt;
        let source = self.file.metadata()?;
        let target = destination.metadata()?;
        if (source.dev(), source.ino()) == (target.dev(), target.ino()) {
            return Err(invalid(
                "prepared Btrfs image is the destination itself".into(),
            ));
        }
        let got = source.len();
        if got != self.len {
            return Err(invalid(format!(
                "prepared Btrfs image changed size: {got} bytes, not the required {}",
                self.len
            )));
        }
        Ok(())
    }

    fn write_to(self, destination: &mut FormatDestination, out: &mut dyn Write) -> io::Result<()> {
        let file = &mut destination.file;
        self.check_source(file)?;
        let (offset, len) = destination_volume(file)?;
        if len != self.len {
            return Err(invalid(format!(
                "prepared Btrfs image needs {} bytes, but the destination volume has {len}",
                self.len
            )));
        }
        let mut image = self.file;

        zero_edges(file, offset, len)?;
        // Durable BEFORE the copy starts, or the ordering below buys nothing: a
        // power loss could otherwise persist new filesystem blocks while the zero
        // over the old superblock is still only in page cache, which is exactly the
        // mixed, apparently-valid volume the deferral exists to prevent.
        file.sync_all()?;
        // The copy is ORDERED, for `run_layout`'s reason one level down: the
        // superblock is this filesystem's commit point, as the primary table is the
        // disk's, and it is only true once everything it points at is durable.
        // Btrfs puts it 64 KiB in, so the FIRST chunk is the commit point — write it
        // last, behind a barrier, and an interrupted `volume` leaves nothing at the
        // offset a mount reads. Written first, the same interruption leaves a
        // superblock a prober calls valid over chunks that are still the PREVIOUS
        // install's bytes, which is a disk that reports a good btrfs and fails to
        // mount.
        //
        // The PRIMARY only. A mirror 64 MiB in is written during the first pass,
        // and deferring it too would not buy the same thing: the zeroing does not
        // reach that far, so what stands there meanwhile is the previous install's
        // mirror rather than nothing. `btrfs rescue super-recover` can promote a
        // mirror, so an interrupted install is recoverable-into-nonsense by a tool
        // asked to try; every path that MOUNTS reads the primary.
        //
        // This is also what makes the head half of `zero_edges` load-bearing rather
        // than merely tidy: with the chunk deferred, those zeros are what stands in
        // the superblock's place for the length of the copy.
        let head = COPY_CHUNK.min(len);
        let rest = copy_sparse(&mut image, file, offset, head, len)?;
        file.sync_all()?;
        let first = copy_sparse(&mut image, file, offset, 0, head)?;
        file.sync_all()?;
        let written = rest.saturating_add(first);

        // The scratch directory is the CALLER's, and so is what is left in it. Not
        // tidiness deferred: the image is the only artifact anything can check the
        // filesystem itself against — `btrfs check` wants a device or a file, and
        // the copy on the destination begins half a gigabyte in, where no tool can
        // be pointed at it. Deleting it here would leave the strongest available
        // check with nothing to run on.
        writeln!(out, "{offset} {len} {written}")
    }
}

/// The ESP's FAT: empty, or the kernel and initramfs of these lengths where
/// the firmware looks for them.
fn esp_volume(
    sector_size: u64,
    plan: &Plan,
    volume_id: u32,
    boot: Option<(u64, u64)>,
) -> io::Result<fat::Volume<'static>> {
    let initrd_name = protocol::EFI_INITRD_PATH
        .rsplit('\\')
        .next()
        .ok_or_else(|| invalid("missing EFI initrd filename".into()))?;
    let root = match boot {
        Some((kernel, initramfs)) => vec![(
            "EFI".into(),
            fat::Node::Dir(vec![(
                "BOOT".into(),
                fat::Node::Dir(vec![
                    (protocol::EFI_BOOT_FILE.into(), fat::Node::Stream(kernel)),
                    (initrd_name.into(), fat::Node::Stream(initramfs)),
                ]),
            )]),
        )],
        None => Vec::new(),
    };
    let esp_sectors = plan
        .esp_sectors()
        .ok_or_else(|| invalid("the ESP length overflowed".to_string()))?;
    let esp_start_lba = u32::try_from(plan.esp_start)
        .map_err(|_| invalid("the ESP starts past what a FAT32 BPB can record".to_string()))?;
    Ok(fat::Volume {
        bytes_per_sector: u32::try_from(sector_size)
            .map_err(|_| invalid("sector size exceeds a FAT32 BPB".to_string()))?,
        total_sectors: esp_sectors,
        hidden_sectors: esp_start_lba,
        volume_id,
        label: protocol::ESP_VOLUME_LABEL.to_string(),
        sectors_per_cluster: None,
        root,
    })
}

/// The system volume holds what installing writes (DESIGN.md "Payload
/// fit"): one copy of the deployment's payloads, and the GiB td-boot's
/// `MIN_VOLUME_BYTES` reserves for Btrfs metadata and `@var`. The layout's
/// own refusal of a disk too small for it comes first.
fn volume_fit(sector_size: u64, capacity: u64, payloads: u64) -> Result<(), String> {
    const GIB: u64 = 1 << 30;
    let volume = plan(sector_size, capacity)?
        .volume_bytes()
        .ok_or("the system volume's length overflowed")?;
    let needed = payloads
        .checked_add(GIB)
        .ok_or("the deployment's size overflowed")?;
    if volume < needed {
        return Err(format!(
            "the system volume holds {volume} bytes; this deployment's \
             {payloads} bytes and 1 GiB need {needed}"
        ));
    }
    Ok(())
}

/// The ESP's FAT, built from lengths alone, holds the kernel and selector.
fn esp_fit(sector_size: u64, capacity: u64, kernel: u64, selector: u64) -> Result<(), String> {
    let layout = plan(sector_size, capacity)?;
    let esp = esp_volume(sector_size, &layout, 0, Some((kernel, selector)))
        .map_err(|error| error.to_string())?;
    fat::build(&esp)
        .map(drop)
        .map_err(|error| format!("the ESP cannot hold the kernel and selector: {error}"))
}

/// The FAT volume serial, taken from the ESP partition's own GUID.
///
/// Not a clock: an installer that stamped the time would make two otherwise
/// identical installs differ in a field nothing needs to differ in, and the
/// oracle compares images. The GUID is already per-install and already random.
fn volume_serial(layout: &gpt::Layout) -> io::Result<u32> {
    let esp = layout
        .partitions
        .first()
        .ok_or_else(|| invalid("the layout has no ESP".to_string()))?;
    let bytes = esp
        .unique_guid
        .0
        .get(..4)
        .and_then(|slice| <[u8; 4]>::try_from(slice).ok())
        .ok_or_else(|| invalid("the ESP GUID is too short".to_string()))?;
    Ok(u32::from_le_bytes(bytes))
}

fn main() -> ExitCode {
    let mode = match parse_args(std::env::args_os().skip(1)) {
        Ok(mode) => mode,
        Err(error) => {
            let _ = writeln!(io::stderr(), "td-install: {error}");
            return ExitCode::FAILURE;
        }
    };
    let result = match mode {
        Mode::NewVolumeUuid => {
            let stdout = io::stdout();
            let mut output = io::BufWriter::new(stdout.lock());
            new_volume_uuid(&mut output).and_then(|()| output.flush())
        }
        Mode::Timezones => {
            let stdout = io::stdout();
            let mut output = io::BufWriter::new(stdout.lock());
            timezones::run(Path::new(TIMEZONE_ROOT), &mut output).and_then(|()| output.flush())
        }
        Mode::PrepareSelector {
            template,
            trusted_key,
            uuid,
            output,
        } => prepare_selector(&template, &trusted_key, &uuid, &output),
        Mode::Destinations => {
            let stdout = io::stdout();
            let mut output = io::BufWriter::new(stdout.lock());
            inventory::destinations(&mut output).and_then(|()| output.flush())
        }
        Mode::CandidateRecord => {
            let stdout = io::stdout();
            candidate_output_allowed(stdout.is_terminal()).and_then(|()| {
                let mut output = io::BufWriter::new(stdout.lock());
                inventory::candidate_record(&mut output).and_then(|()| output.flush())
            })
        }
        Mode::ObservePlan => {
            let stdout = io::stdout();
            let mut output = io::BufWriter::new(stdout.lock());
            observe_plan(&mut io::stdin().lock(), &mut output)
        }
        Mode::ObserveSourcePlan {
            td_boot,
            source,
            trusted_key,
        } => {
            let stdout = io::stdout();
            let mut output = io::BufWriter::new(stdout.lock());
            observe_source_plan(
                &mut io::stdin().lock(),
                &mut output,
                &td_boot,
                &source,
                &trusted_key,
            )
        }
        Mode::Serve(host) => run_serve(host),
        Mode::Inventory => {
            let stdout = io::stdout();
            let mut output = io::BufWriter::new(stdout.lock());
            inventory::run(Path::new("/sys/class/block"), &mut output).and_then(|()| output.flush())
        }
        Mode::LayoutPreview {
            sector_bytes,
            capacity_bytes,
        } => {
            let stdout = io::stdout();
            let mut output = io::BufWriter::new(stdout.lock());
            layout_preview(sector_bytes, capacity_bytes, &mut output).and_then(|()| output.flush())
        }
        Mode::Layout { destination, boot } => match boot {
            Some(boot) => run_layout_with_boot(&destination, Some(&boot), &mut io::stdout()),
            None => run_layout(&destination, &mut io::stdout()),
        },
        Mode::Volume {
            boot,
            uuid,
            timezone,
            hostname,
            username,
            destination,
            mkfs,
            scratch,
            seed,
        } => timezone
            .as_deref()
            .map(|id| timezones::Selection::load(Path::new(TIMEZONE_ROOT), id))
            .transpose()
            .and_then(|timezone| {
                let settings = VolumeSettings {
                    timezone: timezone.as_ref(),
                    hostname: hostname.as_ref(),
                    username: username.as_deref(),
                };
                if let Some(boot) = boot {
                    let prepared =
                        prepare_volume(settings, uuid.as_ref(), &mkfs, &scratch, seed.as_ref())?;
                    return run_format(prepared, &destination, &boot, &mut io::stdout());
                }
                run_volume(
                    settings,
                    uuid.as_ref(),
                    &destination,
                    &mkfs,
                    &scratch,
                    seed.as_ref(),
                    &mut io::stdout(),
                )
            }),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let _ = writeln!(io::stderr(), "td-install: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // The shipped half reaches the filesystem only through `mod paths` now, so
    // this is the test half's own — a test opening a fixture is not a path the
    // installer takes from an operator, and the scan reads only the half above.
    use std::fs::OpenOptions;

    #[test]
    fn observe_plan_accepts_no_operands_and_refuses_bad_wire_before_discovery() {
        assert_eq!(
            parse_args([OsString::from("observe-plan")].into_iter()).unwrap(),
            Mode::ObservePlan
        );
        for extra in ["/dev/vda", "--uuid", "plan.bin"] {
            assert!(parse_args(
                [OsString::from("observe-plan"), OsString::from(extra)].into_iter()
            )
            .is_err());
        }
        for bytes in [
            Vec::new(),
            b"TDPLAN01".to_vec(),
            vec![0; installation_plan::MAX_BYTES + 1],
        ] {
            let mut output = Vec::new();
            assert!(observe_plan(&mut io::Cursor::new(bytes), &mut output).is_err());
            assert!(output.is_empty());
        }
    }

    #[test]
    fn serve_binds_five_absolute_operands() {
        let full = [
            "serve",
            "/bin/td-boot",
            "/source",
            "/trusted.pub",
            "/root",
            "/bin/td-firstboot",
        ];
        assert_eq!(
            parse_args(args(&full)).unwrap(),
            Mode::Serve(LiveHost {
                td_boot: PathBuf::from("/bin/td-boot"),
                source: PathBuf::from("/source"),
                trusted_key: PathBuf::from("/trusted.pub"),
                root: PathBuf::from("/root"),
                firstboot: PathBuf::from("/bin/td-firstboot"),
                timezones: PathBuf::from(TIMEZONE_ROOT),
                catalog: None,
                booted: PathBuf::from(BOOTED_DEPLOYMENT),
            })
        );
        for count in 1..full.len() {
            assert!(parse_args(args(&full[..count])).is_err(), "{count}");
        }
        assert!(parse_args(args(&[full.as_slice(), &["/extra"]].concat())).is_err());
        for index in 1..full.len() {
            let mut relative = full;
            relative[index] = "relative";
            assert_eq!(
                parse_args(args(&relative)).unwrap_err().to_string(),
                "serve operands must be absolute paths"
            );
        }
    }

    #[test]
    fn serve_admits_only_a_root_caller_on_two_sockets() {
        let (socket, mut peer) = std::os::unix::net::UnixStream::pair().unwrap();
        let (consent, mut authority) = std::os::unix::net::UnixStream::pair().unwrap();
        let descriptor = |socket: &std::os::unix::net::UnixStream| {
            File::from(std::os::fd::OwnedFd::from(socket.try_clone().unwrap()))
        };
        let refusal =
            |stdin: File, stdout: File| admit_serve(0, stdin, stdout).unwrap_err().to_string();
        assert_eq!(
            admit_serve(1000, descriptor(&socket), descriptor(&consent))
                .unwrap_err()
                .to_string(),
            "serve requires the installation authority"
        );
        let directory = scratch::path("serve-admission");
        std::fs::create_dir(&directory).unwrap();
        let _cleanup = ScratchDirectory(directory.clone());
        let regular = directory.join("stdin");
        std::fs::write(&regular, b"").unwrap();
        let unconnected =
            std::os::unix::net::UnixListener::bind(directory.join("listener")).unwrap();
        let unconnected =
            || File::from(std::os::fd::OwnedFd::from(unconnected.try_clone().unwrap()));
        let (datagram, _peer) = std::os::unix::net::UnixDatagram::pair().unwrap();
        let datagram = || File::from(std::os::fd::OwnedFd::from(datagram.try_clone().unwrap()));
        let installer = "serve requires its installer channel on stdin";
        let channel = "serve requires its consent channel on stdout";
        for (stdin, stdout, expected) in [
            (
                paths::open_read(&regular).unwrap(),
                descriptor(&consent),
                installer,
            ),
            (unconnected(), descriptor(&consent), installer),
            (datagram(), descriptor(&consent), installer),
            (
                descriptor(&socket),
                paths::open_read(&regular).unwrap(),
                channel,
            ),
            (descriptor(&socket), unconnected(), channel),
            (descriptor(&socket), datagram(), channel),
            (
                descriptor(&socket),
                descriptor(&socket),
                "serve requires distinct installer and consent channels",
            ),
        ] {
            assert_eq!(refusal(stdin, stdout), expected);
        }
        // An inherited O_NONBLOCK is cleared on both: a read waits for a
        // late peer.
        socket.set_nonblocking(true).unwrap();
        consent.set_nonblocking(true).unwrap();
        let (admitted, answered) =
            admit_serve(0, descriptor(&socket), descriptor(&consent)).unwrap();
        for (mut admitted, peer) in [(admitted, &mut peer), (answered, &mut authority)] {
            let mut late = peer.try_clone().unwrap();
            let writer = std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(50));
                late.write_all(b"y").unwrap();
            });
            let mut byte = [0];
            admitted.read_exact(&mut byte).unwrap();
            assert_eq!(&byte, b"y");
            writer.join().unwrap();
            admitted.write_all(b"x").unwrap();
            let mut byte = [0];
            peer.read_exact(&mut byte).unwrap();
            assert_eq!(&byte, b"x");
        }
    }

    /// The restart runs the supervisor's client with nothing of the
    /// service's and counts only td-svc's two acceptances; a client that
    /// says anything else, fails, or does not answer in time refuses.
    #[test]
    fn a_restart_counts_only_the_supervisors_acceptance() {
        let dir = ScratchDirectory(scratch::path("restart"));
        std::fs::create_dir(&dir.0).unwrap();
        let client = |name: &str, body: &str| {
            let path = dir.0.join(name);
            std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o755))
                .unwrap();
            path
        };
        // A script just written may be busy while another test's child is
        // between fork and exec: ask again only then.
        let request_reboot = |path: &std::path::Path, timeout| loop {
            match request_reboot(path, timeout) {
                Err(error) if error.kind() == io::ErrorKind::ExecutableFileBusy => {
                    std::thread::sleep(Duration::from_millis(10))
                }
                result => break result,
            }
        };
        let timeout = Duration::from_secs(5);
        for (name, body, accepted) in [
            (
                "requested",
                "[ \"$*\" = reboot ] && [ -z \"$HOME\" ] && echo 'reboot requested'",
                true,
            ),
            (
                "again",
                "echo 'shutdown already in progress (reboot)'",
                true,
            ),
            ("poweroff", "echo 'poweroff requested'", false),
            ("failed", "echo 'reboot requested'; exit 1", false),
            ("silent", "exit 0", false),
        ] {
            let result = request_reboot(&client(name, body), timeout);
            assert_eq!(result.is_ok(), accepted, "{name}: {result:?}");
        }
        // A descendant that keeps the output open cannot hold the service
        // past the deadline either.
        let held = client(
            "held",
            "(i=0; while [ $i -lt 1000000 ]; do i=$((i + 1)); done) & echo 'reboot requested'",
        );
        let started = std::time::Instant::now();
        assert!(request_reboot(&held, Duration::from_millis(50)).is_err());
        assert!(started.elapsed() < Duration::from_secs(10));
        let started = std::time::Instant::now();
        let hung = request_reboot(
            &client("hung", "while :; do :; done"),
            Duration::from_millis(200),
        );
        assert!(hung.unwrap_err().to_string().contains("did not answer"));
        assert!(started.elapsed() < Duration::from_secs(10));
        assert!(request_reboot(&dir.0.join("absent"), timeout).is_err());
    }

    /// Under serve, standard input is the installer's channel and standard
    /// output td-authd's, so no child may inherit either: each child this
    /// file starts, and no other production module starts one, has its
    /// output captured (and its input null), or sets both. A source check,
    /// since no unprivileged test reaches them all.
    #[test]
    fn no_child_inherits_the_service_channels() {
        let source = include_str!("main.rs");
        let production = source.split("\n#[cfg(test)]\nmod tests {").next().unwrap();
        let mut children = 0;
        for (at, _) in production.match_indices("process::Command::new(") {
            let call = &production[at..];
            let call = &call[..call.find(';').unwrap()];
            assert!(
                call.contains(".output()") || call.contains(".stdin(") && call.contains(".stdout("),
                "{call}"
            );
            children += 1;
        }
        assert_eq!(children, 6);
    }

    #[test]
    fn only_a_listed_stream_inode_is_a_stream() {
        let table: &[u8] = b"Num       RefCount Protocol Flags    Type St Inode Path\n\
            0000000000000000: 00000002 00000000 00000000 0005 03 17\n\
            0000000000000000: 00000002 00000000 00000000 0002 01 18 /run/\xff\n\
            0000000000000000: 00000003 00000000 00000000 0001 03 1\n\
            0000000000000000: 00000003 00000000 00000000 0001 03 19\n\
            0000000000000000: 00000002 00000000 00010000 0001 01 21 @x\n\
            0: 0 0 0 0001 03 22\n\
            0000000000000000: 00000002 00000000 00000000 0002 01 22\n";
        // Inode 22's first row is forged by a bound name; its real row wins.
        for (inode, stream) in [
            (19, true),
            (21, true),
            (22, false),
            (17, false),
            (18, false),
            (20, false),
            (0, false),
        ] {
            assert_eq!(unix_stream_in(table, inode).unwrap(), stream, "{inode}");
        }
    }

    #[test]
    fn claim_failures_map_to_their_refusals() {
        use installation_protocol::Refusal;
        for (kind, refusal) in [
            (io::ErrorKind::ResourceBusy, Refusal::DestinationBusy),
            (io::ErrorKind::InvalidData, Refusal::DestinationChanged),
            (io::ErrorKind::NotFound, Refusal::DestinationChanged),
            (io::ErrorKind::PermissionDenied, Refusal::DestinationChanged),
            (io::ErrorKind::Other, Refusal::DestinationChanged),
        ] {
            assert_eq!(claim_refusal(kind), refusal, "{kind:?}");
        }
    }

    #[test]
    fn plan_identity_is_a_version_four_uuid_in_network_order() {
        for fill in [0x00, 0x5a, 0xff] {
            let (nonce, uuid) = plan_identity([fill; 48]);
            assert_eq!(nonce, [fill; 32]);
            assert_eq!(uuid[6] >> 4, 4);
            assert_eq!(uuid[6] & 0x0f, fill & 0x0f);
            assert_eq!(uuid[8] >> 6, 2);
            assert_eq!(uuid[8] & 0x3f, fill & 0x3f);
            for (index, byte) in uuid.iter().enumerate() {
                if index != 6 && index != 8 {
                    assert_eq!(*byte, fill);
                }
            }
        }
    }

    #[test]
    fn deployment_ids_decode_only_as_64_hex_digits() {
        assert_eq!(digest_bytes(&"ab".repeat(32)).unwrap(), [0xab; 32]);
        let mut sequence = [0; 32];
        for (index, byte) in sequence.iter_mut().enumerate() {
            *byte = index as u8;
        }
        let text: String = sequence.iter().map(|byte| format!("{byte:02x}")).collect();
        assert_eq!(digest_bytes(&text).unwrap(), sequence);
        for bad in [
            "ab".repeat(31),
            "ab".repeat(33),
            format!("{}zz", "ab".repeat(31)),
            format!("{}+1", "ab".repeat(31)),
            format!("{} 1", "ab".repeat(31)),
        ] {
            assert!(digest_bytes(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn live_settings_refuse_with_the_failed_choice() {
        use installation_protocol::Refusal;
        use installation_service::Host;
        let directory = scratch::path("serve-settings");
        std::fs::create_dir(&directory).unwrap();
        let _cleanup = ScratchDirectory(directory.clone());
        let firstboot = directory.join("td-firstboot");
        scratch::executable(
            &firstboot,
            "#!/bin/sh\n[ \"$1\" = check-primary-name ] && [ \"$3\" != taken ]\n",
        )
        .unwrap();
        let zones = directory.join("zoneinfo");
        std::fs::create_dir_all(zones.join("Europe")).unwrap();
        std::fs::create_dir(zones.join("Etc")).unwrap();
        std::fs::write(zones.join("iso3166.tab"), b"GB\tBritain\n").unwrap();
        std::fs::write(
            zones.join("zone1970.tab"),
            b"GB\t+5130-00007\tEurope/London\n",
        )
        .unwrap();
        let mut header = vec![0; 44];
        header[..5].copy_from_slice(b"TZif2");
        for id in ["Europe/London", "Etc/UTC"] {
            std::fs::write(zones.join(id), &header).unwrap();
        }
        let mut host = LiveHost {
            td_boot: PathBuf::from("/nonexistent/td-boot"),
            source: PathBuf::from("/nonexistent/source"),
            trusted_key: PathBuf::from("/nonexistent/trusted.pub"),
            root: directory.clone(),
            firstboot,
            timezones: zones,
            catalog: None,
            booted: PathBuf::from(BOOTED_DEPLOYMENT),
        };
        for (choice, expected) in [
            (["alice", "td-laptop", "us", "Europe/London"], Ok(())),
            (
                ["taken", "td-laptop", "us", "Europe/London"],
                Err(Refusal::InvalidUsername),
            ),
            (
                ["Alice", "td-laptop", "us", "Europe/London"],
                Err(Refusal::InvalidUsername),
            ),
            (
                ["alice", "-laptop", "us", "Europe/London"],
                Err(Refusal::InvalidHostname),
            ),
            (
                ["alice", "td-laptop", "de", "Europe/London"],
                Err(Refusal::UnsupportedKeyboard),
            ),
            (
                ["alice", "td-laptop", "us", "Europe/Paris"],
                Err(Refusal::UnsupportedTimezone),
            ),
        ] {
            let [username, hostname, keyboard, timezone] = choice;
            let settings =
                installation_plan::Settings::new(username, hostname, keyboard, timezone).unwrap();
            assert_eq!(host.check_settings(&settings), expected, "{choice:?}");
        }
        // No source is present, so authentication refuses rather than guessing.
        assert_eq!(host.authenticate_source(), Err(Refusal::SourceUnavailable));
        // Settings are offered exactly the catalog the check admits from.
        let offered = host.timezones().unwrap();
        assert_eq!(offered.as_slice(), ["Etc/UTC", "Europe/London"]);
        for zone in offered.as_slice() {
            let settings =
                installation_plan::Settings::new("alice", "td-laptop", "us", zone).unwrap();
            assert_eq!(host.check_settings(&settings), Ok(()), "{zone}");
        }
        // Read once: a later change does not reach this service.
        std::fs::remove_file(host.timezones.join("Etc/UTC")).unwrap();
        assert_eq!(host.timezones(), Ok(offered));
        // A broken catalog refuses both the offer and the check, as such.
        let mut broken = LiveHost {
            catalog: None,
            ..host
        };
        assert_eq!(broken.timezones(), Err(Refusal::TimezonesUnavailable));
        let settings =
            installation_plan::Settings::new("alice", "td-laptop", "us", "Europe/London").unwrap();
        assert_eq!(
            broken.check_settings(&settings),
            Err(Refusal::TimezonesUnavailable)
        );
    }

    #[test]
    fn source_plan_requires_explicit_paths_and_rejects_bad_wire_before_execution() {
        assert_eq!(
            parse_args(
                [
                    OsString::from("observe-source-plan"),
                    OsString::from("/bin/td-boot"),
                    OsString::from("/source"),
                    OsString::from("/trusted.pub"),
                ]
                .into_iter()
            )
            .unwrap(),
            Mode::ObserveSourcePlan {
                td_boot: PathBuf::from("/bin/td-boot"),
                source: PathBuf::from("/source"),
                trusted_key: PathBuf::from("/trusted.pub"),
            }
        );
        for args in [
            vec![OsString::from("observe-source-plan")],
            vec![
                OsString::from("observe-source-plan"),
                OsString::from("/source"),
            ],
            vec![
                OsString::from("observe-source-plan"),
                OsString::from("/bin/td-boot"),
                OsString::from("/source"),
            ],
            vec![
                OsString::from("observe-source-plan"),
                OsString::from("/bin/td-boot"),
                OsString::from("/source"),
                OsString::from("/trusted.pub"),
                OsString::from("extra"),
            ],
        ] {
            assert!(parse_args(args.into_iter()).is_err());
        }
        let mut output = Vec::new();
        assert!(observe_source_plan(
            &mut io::Cursor::new(b"bad"),
            &mut output,
            Path::new("/bin/td-boot"),
            Path::new("/source"),
            Path::new("/trusted.pub")
        )
        .is_err());
        assert!(output.is_empty());
        let error = observe_source_plan(
            &mut io::Cursor::new(Vec::<u8>::new()),
            &mut output,
            Path::new("/bin/td-boot"),
            Path::new("source"),
            Path::new("/trusted.pub"),
        )
        .unwrap_err();
        assert!(error.to_string().contains("absolute paths"));
        assert!(output.is_empty());
    }

    #[test]
    fn source_plan_reports_only_a_matching_validated_id() {
        let directory = scratch::path("source-plan-validator");
        std::fs::create_dir(&directory).unwrap();
        let _cleanup = ScratchDirectory(directory.clone());
        let validator = directory.join("td-boot");
        let source = directory.join("source");
        let key = directory.join("trusted.pub");
        let destination =
            installation_plan::Destination::new(installation_plan::DestinationObservation {
                name: "vda",
                major: 253,
                minor: 0,
                sequence: 1,
                capacity: DISK,
                sector: 512,
                removable: false,
                model: None,
                serial: None,
                wwid: None,
            })
            .unwrap();
        let settings = installation_plan::Settings::new("tester", "td", "us", "Etc/UTC").unwrap();
        let mut uuid = [0; 16];
        uuid[6] = 0x40;
        uuid[8] = 0x80;
        let plan =
            installation_plan::Plan::new([1; 32], destination, [0xab; 32], uuid, settings).unwrap();
        for (body, reason) in [
            (format!("printf '{}\\n'\n", "ab".repeat(32)), None),
            (
                format!("printf '{}\\n'\n", "ac".repeat(32)),
                Some("reviewed deployment differs"),
            ),
            ("printf 'bad\\n'\n".into(), Some("noncanonical ID")),
            (
                "printf 'invalid source\\n' >&2; exit 1\n".into(),
                Some("invalid source"),
            ),
        ] {
            scratch::executable(&validator, &format!("#!/bin/sh\n[ \"$1\" = validate-source ] || exit 2\n[ \"$2\" = \"{}\" ] || exit 3\n[ \"$3\" = \"{}\" ] || exit 4\n{body}", source.display(), key.display())).unwrap();
            let result = validate_source_plan(&plan, &validator, &source, &key);
            if let Some(reason) = reason {
                assert!(result.unwrap_err().to_string().contains(reason));
            } else {
                assert_eq!(result.unwrap(), "ab".repeat(32));
                let mut report = Vec::new();
                write_source_plan_report(&mut report, &plan, &"ab".repeat(32)).unwrap();
                assert_eq!(report, format!("{{\"version\":1,\"scope\":\"held-source-plan-observation-only\",\"destination\":\"vda\",\"deployment\":\"{}\"}}\n", "ab".repeat(32)).as_bytes());
            }
        }
    }

    #[test]
    fn source_plan_refuses_identity_changed_during_validation() {
        let directory = scratch::path("source-plan-recheck");
        std::fs::create_dir(&directory).unwrap();
        let _cleanup = ScratchDirectory(directory.clone());
        let validator = directory.join("td-boot");
        let observed_sequence = directory.join("diskseq");
        let source = directory.join("source");
        let key = directory.join("trusted.pub");
        let destination =
            installation_plan::Destination::new(installation_plan::DestinationObservation {
                name: "vda",
                major: 253,
                minor: 0,
                sequence: 1,
                capacity: DISK,
                sector: 512,
                removable: false,
                model: None,
                serial: None,
                wwid: None,
            })
            .unwrap();
        let settings = installation_plan::Settings::new("tester", "td", "us", "Etc/UTC").unwrap();
        let mut uuid = [0; 16];
        uuid[6] = 0x40;
        uuid[8] = 0x80;
        let plan =
            installation_plan::Plan::new([1; 32], destination, [0xab; 32], uuid, settings).unwrap();
        std::fs::write(&observed_sequence, "1").unwrap();
        scratch::executable(
            &validator,
            &format!(
                "#!/bin/sh\nprintf 2 > '{}'\nprintf '{}\\n'\n",
                observed_sequence.display(),
                "ab".repeat(32),
            ),
        )
        .unwrap();
        let mut report = Vec::new();
        let error = observe_source_plan_with_claim(
            &plan,
            &mut report,
            &validator,
            &source,
            &key,
            |_| Ok(()),
            |_, _| {
                let sequence = std::fs::read_to_string(&observed_sequence)?;
                if sequence == "1" {
                    Ok(())
                } else {
                    Err(invalid(
                        "claimed destination changed during validation".into(),
                    ))
                }
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("claimed destination changed"));
        assert!(report.is_empty());
    }

    #[test]
    fn volume_identity_cli_admits_no_operands_and_reports_output_failure() {
        assert_eq!(
            parse_args([OsString::from("new-volume-uuid")].into_iter()).unwrap(),
            Mode::NewVolumeUuid
        );
        for extra in ["/dev/vda", "--uuid", "--help"] {
            assert!(parse_args(
                [OsString::from("new-volume-uuid"), OsString::from(extra)].into_iter()
            )
            .is_err());
        }
        let mut bytes = Vec::new();
        new_volume_uuid(&mut bytes).unwrap();
        let line = std::str::from_utf8(&bytes).unwrap();
        let value = line.strip_suffix('\n').unwrap();
        VolumeUuid::parse(value).unwrap();
        assert_eq!(value.as_bytes().get(14), Some(&b'4'));
        assert!(matches!(
            value.as_bytes().get(19),
            Some(b'8' | b'9' | b'a' | b'b')
        ));
        assert!(new_volume_uuid(&mut &mut [][..]).is_err());
    }

    #[test]
    fn selector_preparation_cli_requires_exact_operands_and_canonical_identity() {
        let good = "12345678-1234-4234-8234-123456789abc";
        let args = |parts: &[&str]| {
            parts
                .iter()
                .map(OsString::from)
                .collect::<Vec<_>>()
                .into_iter()
        };
        assert!(matches!(
            parse_args(args(&["prepare-selector", "template", "key", good, "out"])).unwrap(),
            Mode::PrepareSelector { .. }
        ));
        for parts in [
            vec!["prepare-selector"],
            vec!["prepare-selector", "template", good, "out"],
            vec!["prepare-selector", "template", "key", good],
            vec!["prepare-selector", "template", "key", good, "out", "extra"],
            vec![
                "prepare-selector",
                "template",
                "key",
                "00000000-0000-0000-0000-000000000000",
                "out",
            ],
            vec![
                "prepare-selector",
                "template",
                "key",
                "12345678-1234-4234-8234-123456789ABC",
                "out",
            ],
            vec!["prepare-selector", "template", "key", "../../state", "out"],
        ] {
            assert!(parse_args(args(&parts)).is_err());
        }
    }

    #[test]
    fn selector_copy_preserves_template_and_appends_aligned_root_owned_identity() {
        use std::os::unix::fs::PermissionsExt;
        let dir = ScratchDirectory(scratch::path("selector-copy"));
        std::fs::create_dir_all(&dir.0).unwrap();
        let template = dir.0.join("template");
        let key = key_file(&dir.0);
        let key_bytes = std::fs::read(&key).unwrap();
        let uuid = VolumeUuid::parse("12345678-1234-4234-8234-123456789abc").unwrap();
        for length in 1..=8usize {
            let base = vec![b'k'; length];
            std::fs::write(&template, &base).unwrap();
            let output = dir.0.join(format!("out-{length}"));
            prepare_selector(&template, &key, &uuid, &output).unwrap();
            assert_eq!(std::fs::read(&template).unwrap(), base);
            let bytes = std::fs::read(&output).unwrap();
            assert_eq!(&bytes[..length], base);
            assert_eq!(
                std::fs::metadata(&output).unwrap().permissions().mode() & 0o777,
                0o600
            );
            let mut at = (length + 3) & !3;
            assert_eq!(
                bytes.len() - at,
                532 + 132 + key_bytes.len().next_multiple_of(4),
                "keep the exact-limit accounting tied to the real appendix"
            );
            assert!(bytes[length..at].iter().all(|b| *b == 0));
            let mut names = Vec::new();
            loop {
                assert_eq!(&bytes[at..at + 6], b"070701");
                let field = |n: usize| {
                    u32::from_str_radix(
                        std::str::from_utf8(&bytes[at + 6 + n * 8..at + 14 + n * 8]).unwrap(),
                        16,
                    )
                    .unwrap()
                };
                let mode = field(1);
                assert_eq!((field(2), field(3)), (0, 0));
                let size = field(6) as usize;
                let name_end = at + 110 + field(11) as usize;
                assert_eq!(bytes[name_end - 1], 0);
                let name = std::str::from_utf8(&bytes[at + 110..name_end - 1]).unwrap();
                let data = (name_end + 3) & !3;
                if name == "TRAILER!!!" {
                    assert_eq!(size, 0);
                    assert_eq!(data, bytes.len());
                    break;
                }
                names.push(name.to_owned());
                if name == "etc/td/volume-uuid" {
                    assert_eq!(mode, 0o100644);
                    assert_eq!(field(4), 1);
                    assert_eq!(
                        &bytes[data..data + size],
                        format!("{}\n", uuid.0).as_bytes()
                    );
                } else if name == "etc/td/deployment.pub" {
                    assert_eq!(mode, 0o100644);
                    assert_eq!(field(4), 1);
                    assert_eq!(&bytes[data..data + size], key_bytes);
                } else {
                    assert_eq!(mode, 0o040755);
                    assert_eq!(field(4), 2);
                    assert_eq!(size, 0);
                }
                at = (data + size + 3) & !3;
            }
            assert_eq!(
                names,
                [
                    "etc",
                    "etc/td",
                    "etc/td/deployment.pub",
                    "etc/td/volume-uuid"
                ]
            );
        }
    }

    #[test]
    fn selector_preparation_accepts_the_exact_complete_file_limit() {
        let dir = ScratchDirectory(scratch::path("selector-limit"));
        std::fs::create_dir_all(&dir.0).unwrap();
        let template = dir.0.join("template");
        let key = key_file(&dir.0);
        let key_len = std::fs::read(&key).unwrap().len();
        let uuid = VolumeUuid::parse("12345678-1234-4234-8234-123456789abc").unwrap();
        // Five newc records: two directories, key file, UUID file, and
        // trailer. Independently account for each aligned header/name and
        // file body.
        let appendix_bytes =
            116 + 120 + (132 + key_len.next_multiple_of(4) as u64) + 132 + 40 + 124;
        let maximum_template = MAX_BOOT_FILE - appendix_bytes;
        // Test the real admission helper without allocating a dense 256 MiB
        // output on the fixture tmpfs; small copies exercise the same writer.
        assert_eq!(
            selector_join_padding(maximum_template, appendix_bytes as usize).unwrap(),
            0
        );
        assert!(selector_join_padding(maximum_template + 1, appendix_bytes as usize).is_err());
        assert!(selector_join_padding(u64::MAX, appendix_bytes as usize).is_err());
        let too_large = dir.0.join("too-large");
        File::create(&template)
            .unwrap()
            .set_len(maximum_template + 1)
            .unwrap();
        assert!(prepare_selector(&template, &key, &uuid, &too_large).is_err());
        assert!(!too_large.exists());
    }

    #[test]
    fn selector_preparation_refuses_bad_sources_and_existing_outputs_without_changes() {
        use std::os::unix::fs::symlink;
        let dir = ScratchDirectory(scratch::path("selector-refusal"));
        std::fs::create_dir_all(&dir.0).unwrap();
        let template = dir.0.join("template");
        let output = dir.0.join("output");
        let key = key_file(&dir.0);
        let uuid = VolumeUuid::parse("12345678-1234-4234-8234-123456789abc").unwrap();
        for size in [0, MAX_BOOT_FILE, MAX_BOOT_FILE + 1] {
            File::create(&template).unwrap().set_len(size).unwrap();
            assert!(prepare_selector(&template, &key, &uuid, &output).is_err());
            assert!(!output.exists());
            assert_eq!(std::fs::metadata(&template).unwrap().len(), size);
        }
        std::fs::write(&template, b"verified template bytes").unwrap();
        let link = dir.0.join("link");
        symlink(&template, &link).unwrap();
        for input in [&link, &dir.0, &dir.0.join("missing")] {
            assert!(prepare_selector(input, &key, &uuid, &output).is_err());
            assert!(!output.exists());
        }
        // A key the trusted-key reader or td-boot's decoder would refuse is
        // refused before any output, and before the template is read: a
        // missing template would refuse too, for the wrong reason.
        let key_link = dir.0.join("key-link");
        symlink(&key, &key_link).unwrap();
        let mut bad_keys = vec![key_link, dir.0.clone(), dir.0.join("missing")];
        for (name, text) in [
            ("short", "ab".repeat(31)),
            ("long", "ab".repeat(33)),
            ("not-hex", format!("{}zz", "ab".repeat(31))),
            ("empty", String::new()),
        ] {
            let path = dir.0.join(name);
            std::fs::write(&path, text).unwrap();
            bad_keys.push(path);
        }
        let absent = dir.0.join("absent-template");
        for bad_key in &bad_keys {
            let refused = prepare_selector(&absent, bad_key, &uuid, &output)
                .unwrap_err()
                .to_string();
            assert!(refused.contains("trusted deployment key"), "{refused}");
            assert!(!output.exists());
        }
        // td-boot trims surrounding whitespace and reads either case.
        let spaced = dir.0.join("spaced");
        std::fs::write(&spaced, format!("  {}\r\n", "AB".repeat(32))).unwrap();
        prepare_selector(&template, &spaced, &uuid, &output).unwrap();
        std::fs::remove_file(&output).unwrap();
        for existing in [&template, &link, &dir.0] {
            assert!(prepare_selector(&template, &key, &uuid, existing).is_err());
            assert_eq!(
                std::fs::read(&template).unwrap(),
                b"verified template bytes"
            );
        }
        std::fs::write(&output, b"preserve existing output").unwrap();
        assert!(prepare_selector(&template, &key, &uuid, &output).is_err());
        assert_eq!(std::fs::read(&output).unwrap(), b"preserve existing output");
    }

    #[test]
    fn cli_accepts_no_root_or_other_operands() {
        use std::ffi::OsString;
        assert_eq!(
            parse_args([OsString::from("timezones")].into_iter()).unwrap(),
            Mode::Timezones
        );
        for operand in ["/tmp", "--root", "--uuid", "--trusted-key"] {
            assert!(
                parse_args([OsString::from("timezones"), OsString::from(operand)].into_iter())
                    .is_err()
            );
        }
    }

    #[test]
    fn volume_timezone_is_an_optional_prefix_after_the_uuid() {
        let args = |values: &[&str]| {
            values
                .iter()
                .map(OsString::from)
                .collect::<Vec<_>>()
                .into_iter()
        };
        let mode = parse_args(args(&[
            "volume",
            "--timezone",
            "Europe/London",
            "disk",
            "/mkfs",
            "scratch",
        ]))
        .unwrap();
        assert!(
            matches!(mode, Mode::Volume { timezone: Some(ref id), uuid: None, .. } if id == "Europe/London")
        );
        let mode = parse_args(args(&[
            "volume",
            "--uuid",
            "12345678-1234-4234-8234-123456789abc",
            "--timezone",
            "Etc/UTC",
            "disk",
            "/mkfs",
            "scratch",
        ]))
        .unwrap();
        assert!(
            matches!(mode, Mode::Volume { timezone: Some(ref id), uuid: Some(_), .. } if id == "Etc/UTC")
        );
        for values in [
            vec!["volume", "--timezone"],
            vec![
                "volume",
                "disk",
                "/mkfs",
                "scratch",
                "--timezone",
                "Etc/UTC",
            ],
            vec![
                "volume",
                "--timezone",
                "Etc/UTC",
                "--timezone",
                "Etc/UTC",
                "disk",
                "/mkfs",
                "scratch",
            ],
            vec!["layout", "--timezone", "Etc/UTC", "disk"],
            vec![
                "volume",
                "--timezone",
                "Etc/UTC",
                "--uuid",
                "12345678-1234-4234-8234-123456789abc",
                "disk",
                "/mkfs",
                "scratch",
            ],
        ] {
            assert!(parse_args(args(&values)).is_err(), "{values:?}");
        }
    }

    #[test]
    fn hostname_selection_is_validated_before_volume_operands() {
        let parse = |values: &[&str]| parse_args(values.iter().map(OsString::from));
        assert!(
            matches!(parse(&["volume", "--hostname", "my-td", "disk", "/mkfs", "scratch"]).unwrap(),
            Mode::Volume { hostname: Some(ref name), .. } if name.name() == "my-td")
        );
        assert!(parse(&[
            "volume",
            "--timezone",
            "Etc/UTC",
            "--hostname",
            "my-td",
            "disk",
            "/mkfs",
            "scratch"
        ])
        .is_ok());
        for args in [
            vec!["volume", "--hostname"],
            vec!["volume", "--hostname", "MY-TD", "disk", "/mkfs", "scratch"],
            vec!["volume", "disk", "/mkfs", "scratch", "--hostname", "my-td"],
            vec![
                "volume",
                "--hostname",
                "my-td",
                "--hostname",
                "my-td",
                "disk",
                "/mkfs",
                "scratch",
            ],
            vec![
                "volume",
                "--hostname",
                "my-td",
                "--timezone",
                "Etc/UTC",
                "disk",
                "/mkfs",
                "scratch",
            ],
            vec!["layout", "--hostname", "my-td", "disk"],
        ] {
            assert!(parse(&args).is_err(), "{args:?}");
        }
        use std::os::unix::ffi::OsStringExt;
        assert!(parse_args(
            [
                OsString::from("volume"),
                OsString::from("--hostname"),
                OsString::from_vec(vec![0xff]),
                OsString::from("disk"),
                OsString::from("/mkfs"),
                OsString::from("scratch")
            ]
            .into_iter()
        )
        .is_err());
    }

    #[test]
    fn username_operands_require_one_bound_validator_and_verified_root() {
        let parsed = parse_args(args(&[
            "volume",
            "--timezone",
            "Etc/UTC",
            "--hostname",
            "my-td",
            "--username",
            "alice",
            "/verified",
            "/firstboot",
            "disk",
            "/mkfs",
            "scratch",
        ]))
        .unwrap();
        assert!(
            matches!(parsed, Mode::Volume { username: Some(ref choice), .. }
            if choice.as_ref() == &PrimarySelection { name: "alice".into(), root: "/verified".into(), firstboot: "/firstboot".into() })
        );
        for bad in [
            vec!["volume", "--username"],
            vec!["volume", "--username", "alice", "/verified"],
            vec![
                "volume",
                "--username",
                "",
                "/verified",
                "/firstboot",
                "disk",
                "/mkfs",
                "scratch",
            ],
            vec![
                "volume",
                "--username",
                "alice",
                "verified",
                "/firstboot",
                "disk",
                "/mkfs",
                "scratch",
            ],
            vec![
                "volume",
                "--username",
                "alice",
                "/verified",
                "firstboot",
                "disk",
                "/mkfs",
                "scratch",
            ],
            vec![
                "volume",
                "disk",
                "/mkfs",
                "scratch",
                "--username",
                "alice",
                "/verified",
                "/firstboot",
            ],
            vec![
                "volume",
                "--username",
                "alice",
                "/verified",
                "/firstboot",
                "--username",
                "bob",
                "/verified",
                "/firstboot",
                "disk",
                "/mkfs",
                "scratch",
            ],
            vec![
                "layout",
                "--username",
                "alice",
                "/verified",
                "/firstboot",
                "disk",
            ],
        ] {
            assert!(parse_args(args(&bad)).is_err(), "{bad:?}");
        }
        for name in ["a", "tester", "a-b_c1", &"a".repeat(32)] {
            assert!(
                parse_args(args(&[
                    "volume",
                    "--username",
                    name,
                    "/verified",
                    "/firstboot",
                    "disk",
                    "/mkfs",
                    "scratch"
                ]))
                .is_ok(),
                "{name:?}"
            );
        }
        for name in [
            "",
            "Alice",
            "alice\n",
            "alice:root",
            "alice,root",
            "a b",
            "../alice",
            "1alice",
            "-alice",
            "_alice",
            "é",
            &"a".repeat(33),
        ] {
            assert!(
                parse_args(args(&[
                    "volume",
                    "--username",
                    name,
                    "/verified",
                    "/firstboot",
                    "disk",
                    "/mkfs",
                    "scratch"
                ]))
                .is_err(),
                "{name:?}"
            );
        }
        use std::os::unix::ffi::OsStringExt;
        let mut bad = args(&["volume", "--username"]).collect::<Vec<_>>();
        bad.push(OsString::from_vec(vec![0xff]));
        bad.extend(args(&[
            "/verified",
            "/firstboot",
            "disk",
            "/mkfs",
            "scratch",
        ]));
        assert!(parse_args(bad.into_iter()).is_err());
    }

    #[test]
    fn username_validation_refuses_before_destination_or_scratch_changes() {
        let disk = Scratch::disk(DISK);
        let dir = fake_mkfs("#!/bin/sh\nexit 1\n");
        let sentinel = dir.join("td-volume-root/keep");
        std::fs::create_dir(sentinel.parent().unwrap()).unwrap();
        std::fs::write(&sentinel, b"untouched").unwrap();
        let selection = PrimarySelection {
            name: "root".into(),
            root: "/verified".into(),
            firstboot: dir.join("mkfs.btrfs"),
        };
        let mut output = Vec::new();
        // Neither a valid layout nor even an existing destination is required
        // to reach the account refusal, and the preexisting stage survives it.
        for destination in [&disk.path, &dir.join("absent")] {
            let error = run_volume(
                VolumeSettings {
                    username: Some(&selection),
                    ..VolumeSettings::default()
                },
                None,
                destination,
                &dir.join("mkfs.btrfs"),
                &dir,
                None,
                &mut output,
            )
            .unwrap_err();
            assert!(error
                .to_string()
                .contains("selected primary account validation failed"));
        }
        assert!(output.is_empty());
        assert_eq!(std::fs::read(&sentinel).unwrap(), b"untouched");
        assert!(!dir.join("absent").exists());
        assert!(!dir.join("td-volume.img").exists());
        let mut bytes = [1; 512];
        File::open(&disk.path)
            .unwrap()
            .read_exact(&mut bytes)
            .unwrap();
        assert_eq!(bytes, [0; 512]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn username_volume_binds_validator_argv_and_seeds_exact_readable_state() {
        use std::os::unix::fs::PermissionsExt;
        let disk = Scratch::disk(DISK);
        run_layout(&disk.path, &mut Vec::new()).unwrap();
        let validator = fake_mkfs(RECORDING_MKFS);
        let formatter = fake_mkfs(RECORDING_MKFS);
        let selection = PrimarySelection {
            name: "alice".into(),
            root: "/verified root".into(),
            firstboot: validator.join("mkfs.btrfs"),
        };
        run_volume(
            VolumeSettings {
                username: Some(&selection),
                ..VolumeSettings::default()
            },
            None,
            &disk.path,
            &formatter.join("mkfs.btrfs"),
            &formatter,
            None,
            &mut Vec::new(),
        )
        .unwrap();
        assert_eq!(
            std::fs::read(validator.join("argv")).unwrap(),
            b"check-primary-name\n/verified root\nalice\n"
        );
        let state = formatter.join("td-volume-root/@var");
        let file = state.join(USERNAME_STATE_RELATIVE);
        assert_eq!(std::fs::read(&file).unwrap(), b"alice\n");
        assert_eq!(
            std::fs::metadata(&file).unwrap().permissions().mode() & 0o7777,
            0o644
        );
        for parent in ["", "lib", "lib/td"] {
            assert_eq!(
                std::fs::metadata(state.join(parent))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o7777,
                0o755
            );
        }
        assert!(seed_setting(&state, USERNAME_STATE_RELATIVE, "bob").is_err());
        assert_eq!(std::fs::read(file).unwrap(), b"alice\n");
        std::fs::remove_dir_all(validator).unwrap();
        std::fs::remove_dir_all(formatter).unwrap();
    }

    #[test]
    fn hostname_setting_is_readable_and_cannot_replace_existing_state() {
        use std::os::unix::fs::PermissionsExt;
        let root = scratch::path("hostname-state");
        std::fs::create_dir(&root).unwrap();
        seed_setting(&root, "lib/td/hostname", "my-td").unwrap();
        let path = root.join("lib/td/hostname");
        assert_eq!(std::fs::read(&path).unwrap(), b"my-td\n");
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o7777,
            0o644
        );
        assert!(seed_setting(&root, "lib/td/hostname", "other").is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"my-td\n");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn timezone_state_is_readable_persistent_data_and_is_never_overwritten() {
        use std::os::unix::fs::PermissionsExt;
        let root = scratch::path("timezone-state");
        std::fs::create_dir(&root).unwrap();
        let catalog = root.join("catalog");
        std::fs::create_dir_all(catalog.join("Europe")).unwrap();
        std::fs::create_dir(catalog.join("Etc")).unwrap();
        std::fs::write(catalog.join("iso3166.tab"), b"GB\tBritain\n").unwrap();
        std::fs::write(
            catalog.join("zone1970.tab"),
            b"GB\t+5130-00007\tEurope/London\n",
        )
        .unwrap();
        let mut header = vec![0; 44];
        header[..5].copy_from_slice(b"TZif2");
        for id in ["Europe/London", "Etc/UTC"] {
            std::fs::write(catalog.join(id), &header).unwrap();
        }
        let choice = timezones::Selection::load(&catalog, "Europe/London").unwrap();
        let subvol = root.join("@var");
        std::fs::create_dir(&subvol).unwrap();
        std::fs::set_permissions(&subvol, std::fs::Permissions::from_mode(0o755)).unwrap();
        seed_timezone(&subvol, &choice).unwrap();
        let state = subvol.join("lib/td/timezone");
        assert_eq!(std::fs::read(&state).unwrap(), b"Europe/London\n");
        assert_eq!(
            std::fs::metadata(&state).unwrap().permissions().mode() & 0o777,
            0o644
        );
        for directory in [subvol.clone(), subvol.join("lib"), subvol.join("lib/td")] {
            assert_eq!(
                std::fs::metadata(directory).unwrap().permissions().mode() & 0o777,
                0o755
            );
        }
        std::fs::write(&state, b"preserve existing state").unwrap();
        assert!(seed_timezone(&subvol, &choice).is_err());
        assert_eq!(std::fs::read(&state).unwrap(), b"preserve existing state");
        std::fs::remove_dir_all(root).unwrap();
    }
    const MIB: u64 = 1024 * 1024;
    const GIB: u64 = 1024 * MIB;
    /// Big enough for the ESP plus the smallest volume td-install accepts.
    /// These are sparse files, but every byte a test READS is real.
    const DISK: u64 = 6 * GIB;

    struct Scratch {
        path: PathBuf,
    }

    struct ScratchDirectory(PathBuf);

    impl Drop for ScratchDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    impl Scratch {
        /// A sparse file of `bytes`, which is a destination `td-install` treats
        /// exactly as it treats a disk (D9).
        fn disk(bytes: u64) -> Scratch {
            let path = scratch::path("disk");
            let file = File::create(&path).unwrap();
            file.set_len(bytes).unwrap();
            Scratch { path }
        }

        /// Read a RANGE. Never the whole file: these destinations are sparse
        /// and gigabytes wide, and `fs::read` would materialize every zero.
        fn read_at(&self, offset: u64, len: usize) -> Vec<u8> {
            let mut file = File::open(&self.path).unwrap();
            file.seek(SeekFrom::Start(offset)).unwrap();
            let mut bytes = vec![0u8; len];
            file.read_exact(&mut bytes).unwrap();
            bytes
        }

        fn table(&self, disk_bytes: u64) -> gpt::Table {
            let sectors = disk_bytes / 512;
            let primary = self.read_at(0, 34 * 512);
            let backup = self.read_at((sectors - 33) * 512, 33 * 512);
            gpt::parse(&primary, &backup, 512).unwrap()
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    /// A stand-in that RECORDS the argv it was given, one word a line, beside
    /// itself. Every check in this file and in the recipe reads what mkfs
    /// produced, and the arguments are invisible to all of them: `--byte-count`
    /// half the real size still puts the superblock at 64 KiB, the label at
    /// 64 KiB+299 and a mirror at 64 MiB, so a volume sized wrongly passes every
    /// one. The only place the request itself can be seen is here.
    const RECORDING_MKFS: &str = "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$(dirname \"$0\")/argv\"\n";

    /// A directory holding an executable `mkfs.btrfs` stand-in with `body`.
    fn fake_mkfs(body: &str) -> PathBuf {
        let dir = scratch::path("mkfs");
        std::fs::create_dir(&dir).unwrap();
        scratch::executable(&dir.join("mkfs.btrfs"), body).unwrap();
        dir
    }

    fn args(values: &[&str]) -> std::vec::IntoIter<OsString> {
        values
            .iter()
            .map(|value| OsString::from(*value))
            .collect::<Vec<_>>()
            .into_iter()
    }

    #[test]
    fn layout_preview_accepts_only_two_bounded_decimal_counts() {
        assert_eq!(
            parse_args(args(&[
                "layout-preview",
                "00000000000000000512",
                "18446744073709551615"
            ]))
            .unwrap(),
            Mode::LayoutPreview {
                sector_bytes: 512,
                capacity_bytes: u64::MAX
            }
        );
        assert_eq!(
            parse_args(args(&["layout-preview", "512", "6442450944"])).unwrap(),
            Mode::LayoutPreview {
                sector_bytes: 512,
                capacity_bytes: DISK
            }
        );
        for bad in [
            "",
            "+512",
            "-1",
            " 512",
            "512 ",
            "5.12",
            "1e9",
            "/dev/sda",
            "18446744073709551616",
            "000000000000000000001",
            "５１２",
        ] {
            assert!(parse_args(args(&["layout-preview", bad, "6442450944"])).is_err());
            assert!(parse_args(args(&["layout-preview", "512", bad])).is_err());
        }
        for bad in [
            vec!["layout-preview"],
            vec!["layout-preview", "512"],
            vec!["layout-preview", "512", "6442450944", "disk"],
            vec!["layout-preview", "--uuid", "512", "6442450944"],
        ] {
            assert!(parse_args(args(&bad)).is_err(), "{bad:?}");
        }
        use std::os::unix::ffi::OsStringExt;
        assert!(parse_args(
            [
                OsString::from("layout-preview"),
                OsString::from("512"),
                OsString::from_vec(vec![0xff])
            ]
            .into_iter()
        )
        .is_err());
    }

    #[test]
    fn layout_preview_reports_exact_512_and_4kn_ranges() {
        for (sector, esp_start, esp_end, volume_start, volume_end, volume_bytes) in [
            (512, 2048, 1050623, 1050624, 12582878, 5904514560u64),
            (4096, 256, 131327, 131328, 1572858, 5904510976u64),
        ] {
            let expected = format!(concat!(
                "{{\"version\":1,\"scope\":\"layout-preview\",\"logical_sector_bytes\":{},\"capacity_bytes\":6442450944,",
                "\"partitions\":[{{\"number\":1,\"purpose\":\"efi-system\",\"start_lba\":{},\"end_lba\":{},\"offset_bytes\":1048576,\"capacity_bytes\":536870912}},",
                "{{\"number\":2,\"purpose\":\"system-volume\",\"start_lba\":{},\"end_lba\":{},\"offset_bytes\":537919488,\"capacity_bytes\":{}}}]}}\n"
            ), sector, esp_start, esp_end, volume_start, volume_end, volume_bytes);
            let mut output = Vec::new();
            layout_preview(sector, DISK, &mut output).unwrap();
            assert_eq!(String::from_utf8(output).unwrap(), expected);
        }
        let disk = Scratch::disk(DISK);
        run_layout(&disk.path, &mut Vec::new()).unwrap();
        let table = disk.table(DISK);
        let ranges: Vec<_> = table
            .partitions
            .iter()
            .map(|part| (part.start_lba, part.end_lba))
            .collect();
        assert_eq!(ranges, [(2048, 1050623), (1050624, 12582878)]);
    }

    #[test]
    fn layout_preview_refuses_bad_geometry_before_output_and_propagates_io_errors() {
        assert!(layout_preview(512, 0, &mut Vec::new())
            .unwrap_err()
            .to_string()
            .contains("layout preview:"));
        for (sector, capacity) in [
            (0, DISK),
            (1024, DISK),
            (8192, DISK),
            (u64::MAX, DISK),
            (512, 0),
            (512, protocol::ESP_BYTES),
            (512, DISK + 1),
            (4096, u64::MAX),
        ] {
            let mut output = Vec::new();
            assert!(layout_preview(sector, capacity, &mut output).is_err());
            assert!(output.is_empty());
        }
        struct Closed;
        impl Write for Closed {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::ErrorKind::BrokenPipe.into())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        assert_eq!(
            layout_preview(512, DISK, &mut Closed).unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
    }

    #[test]
    fn efi_inputs_are_a_pair_and_bad_inputs_preserve_the_disk() {
        assert!(parse_args(args(&["layout", "disk", "kernel", "initrd"])).is_ok());
        assert!(parse_args(args(&["layout", "disk", "kernel", "initrd", "extra"])).is_err());
        let disk = Scratch::disk(DISK);
        run_layout(&disk.path, &mut Vec::new()).unwrap();
        let snapshot = || {
            let mut file = File::open(&disk.path).unwrap();
            let mut bytes = Vec::new();
            for (offset, len) in table_ranges(512, DISK / 512)
                .unwrap()
                .into_iter()
                .chain([(MIB, 16 * MIB)])
            {
                bytes.extend_from_slice(&read_at(&mut file, offset, len).unwrap());
            }
            bytes
        };
        let before = snapshot();
        let kernel = Scratch::disk(1);
        let initramfs = Scratch::disk(0);
        let boot = BootFiles {
            kernel: kernel.path.clone(),
            initramfs: initramfs.path.clone(),
        };
        assert!(run_layout_with_boot(&disk.path, Some(&boot), &mut Vec::new()).is_err());
        assert_eq!(before, snapshot());
        File::options()
            .write(true)
            .open(&initramfs.path)
            .unwrap()
            .set_len(MAX_BOOT_FILE)
            .unwrap();
        File::options()
            .write(true)
            .open(&kernel.path)
            .unwrap()
            .set_len(MAX_BOOT_FILE)
            .unwrap();
        // Each source meets its bound, but together they cannot fit this ESP.
        assert!(run_layout_with_boot(&disk.path, Some(&boot), &mut Vec::new()).is_err());
        assert_eq!(before, snapshot());
        let alias = scratch::path("efi-alias");
        std::fs::hard_link(&disk.path, &alias).unwrap();
        let boot = BootFiles {
            kernel: alias.clone(),
            initramfs: initramfs.path.clone(),
        };
        let error = run_layout_with_boot(&disk.path, Some(&boot), &mut Vec::new()).unwrap_err();
        assert!(error.to_string().contains("is the destination"), "{error}");
        assert_eq!(before, snapshot());
        std::fs::remove_file(alias).unwrap();
    }

    #[test]
    fn efi_stream_refuses_growth_and_shortening_after_open() {
        for len in [2, 4] {
            let source = Scratch::disk(3);
            let destination = Scratch::disk(0);
            let mut out = File::options().write(true).open(&destination.path).unwrap();
            let mut input = BootInput::open(&source.path, &out).unwrap();
            File::options()
                .write(true)
                .open(&source.path)
                .unwrap()
                .set_len(len)
                .unwrap();
            assert!(input
                .copy_to(&mut out, &destination.path)
                .unwrap_err()
                .to_string()
                .contains("changed size"));
        }
    }

    #[test]
    fn efi_files_and_cluster_padding_replace_old_esp_bytes() {
        let disk = Scratch::disk(DISK);
        let kernel = Scratch::disk(0);
        let initramfs = Scratch::disk(0);
        let kernel_bytes = vec![0x4b; 9001];
        let initrd_bytes = vec![0x49; 5003];
        std::fs::write(&kernel.path, &kernel_bytes).unwrap();
        std::fs::write(&initramfs.path, &initrd_bytes).unwrap();
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&disk.path)
            .unwrap();
        // Dirty enough of the old ESP to cover metadata and both file chains.
        write_at(&mut file, MIB, &vec![0xa5; 16 * MIB as usize]).unwrap();
        let boot = BootFiles {
            kernel: kernel.path.clone(),
            initramfs: initramfs.path.clone(),
        };
        let retained = Scratch::disk(0);
        let mut destination = FormatDestination::open(&disk.path).unwrap();
        std::fs::rename(&disk.path, &retained.path).unwrap();
        std::fs::write(&disk.path, b"replacement must survive").unwrap();
        let mut output = Vec::new();
        format_layout(&mut destination, Some(&boot), &mut output).unwrap();
        assert_eq!(
            std::fs::read(&disk.path).unwrap(),
            b"replacement must survive"
        );
        assert_eq!(String::from_utf8(output).unwrap(), "1048576 537919488\n");
        let bpb = read_at(&mut file, MIB, 512).unwrap();
        let u16le = |bytes: &[u8]| u16::from_le_bytes(bytes.try_into().unwrap()) as u64;
        let u32le = |bytes: &[u8]| u32::from_le_bytes(bytes.try_into().unwrap()) as u64;
        let sector = u16le(&bpb[11..13]);
        let cluster = sector * u64::from(bpb[13]);
        let data = MIB + sector * (u16le(&bpb[14..16]) + u64::from(bpb[16]) * u32le(&bpb[36..40]));
        let at = |cluster_no| data + (cluster_no - 2) * cluster;
        let entry = |file: &mut File, parent, name: &[u8]| {
            let directory = read_at(file, at(parent), cluster).unwrap();
            let index = directory
                .chunks_exact(32)
                .position(|entry| &entry[..11] == name)
                .unwrap();
            let entry = &directory[index * 32..(index + 1) * 32];
            (
                u16le(&entry[20..22]) << 16 | u16le(&entry[26..28]),
                u32le(&entry[28..32]),
            )
        };
        // Read FAT directory entries independently of the formatter's placements.
        let (efi, _) = entry(&mut file, u32le(&bpb[44..48]), b"EFI        ");
        let (boot, _) = entry(&mut file, efi, b"BOOT       ");
        for (name, expected) in [
            (b"BOOTX64 EFI", &kernel_bytes),
            (b"INITRD     ", &initrd_bytes),
        ] {
            let (first, len) = entry(&mut file, boot, name);
            assert_eq!(len, expected.len() as u64);
            assert_eq!(read_at(&mut file, at(first), len).unwrap(), *expected);
            let padding = (cluster - len % cluster) % cluster;
            assert_eq!(
                read_at(&mut file, at(first) + len, padding).unwrap(),
                vec![0; padding as usize]
            );
        }
        let directory = read_at(&mut file, at(boot), cluster).unwrap();
        // Dot, dotdot and the two files occupy four entries; the rest terminates
        // the directory and must not retain the old 0xa5 entries.
        assert!(directory
            .get(4 * 32..)
            .unwrap()
            .iter()
            .all(|byte| *byte == 0));
        assert_eq!(retained.table(DISK).partitions.len(), 2);
    }

    #[test]
    fn the_verb_and_its_arity_are_exact() {
        assert_eq!(
            parse_args(args(&["candidate-record"])).unwrap(),
            Mode::CandidateRecord
        );
        assert!(parse_args(args(&["candidate-record", "/dev/vda"])).is_err());
        assert!(parse_args(args(&["candidate-record", "--uuid"])).is_err());
        assert!(parse_args(args(&["candidate-record", "--trusted-key"])).is_err());
        assert!(candidate_output_allowed(true).is_err());
        assert!(candidate_output_allowed(false).is_ok());
        assert_eq!(
            parse_args(args(&["layout", "/dev/sda"])).unwrap(),
            Mode::Layout {
                destination: PathBuf::from("/dev/sda"),
                boot: None,
            }
        );
        assert!(
            parse_args(args(&["layout"])).is_err(),
            "missing destination"
        );
        assert!(
            parse_args(args(&["layout", "/dev/sda", "extra"])).is_err(),
            "a third argument is not silently ignored"
        );
        assert!(parse_args(args(&["format", "/dev/sda"])).is_err(), "verb");
        assert!(parse_args(args(&[])).is_err(), "no arguments");
        assert_eq!(
            parse_args(args(&["volume", "/dev/sda", "/bin/mkfs.btrfs", "/tmp"])).unwrap(),
            Mode::Volume {
                boot: None,
                uuid: None,
                timezone: None,
                hostname: None,
                username: None,
                destination: PathBuf::from("/dev/sda"),
                mkfs: PathBuf::from("/bin/mkfs.btrfs"),
                scratch: PathBuf::from("/tmp"),
                seed: None,
            }
        );
        // Each of the three is REQUIRED, and none is defaulted: a `volume` that
        // guessed where mkfs.btrfs is would resolve it out of an ambient PATH,
        // and one that guessed a scratch directory would put a
        // partition-sized image somewhere nobody chose.
        for short in [
            vec!["volume", "/dev/sda"],
            vec!["volume", "/dev/sda", "/bin/mkfs.btrfs"],
        ] {
            assert!(parse_args(args(&short)).is_err(), "{short:?} is incomplete");
        }
        assert!(
            parse_args(args(&[
                "volume",
                "/dev/sda",
                "/bin/mkfs.btrfs",
                "/tmp",
                "x"
            ]))
            .is_err(),
            "a fourth argument is not silently ignored"
        );
    }

    /// The ranges a table is read back from are the ones `gpt::build` wrote to.
    /// Derived rather than remembered, so this pins them at BOTH sector sizes —
    /// the 4Kn arithmetic is where a hardcoded 34/33 would be wrong by eight.
    #[test]
    fn the_table_ranges_are_where_the_table_was_written() {
        for (sector, disk) in [(512u64, DISK), (4096, DISK)] {
            let sectors = disk / sector;
            let [primary, backup] = table_ranges(sector, sectors).unwrap();
            let layout = gpt::Layout {
                sector_size: sector,
                disk_sectors: sectors,
                disk_guid: gpt::Guid::parse("12345678-1234-4234-8234-123456789abc").unwrap(),
                align_sectors: protocol::PARTITION_ALIGN_BYTES / sector,
                partitions: Vec::new(),
            };
            let image = gpt::build(&layout).unwrap();
            assert_eq!(primary, (image.primary_offset, image.primary.len() as u64));
            assert_eq!(backup, (image.backup_offset, image.backup.len() as u64));
        }
    }

    #[test]
    fn held_formatting_never_reopens_a_replaced_destination_name() {
        let disk = Scratch::disk(DISK);
        let retained = Scratch::disk(0);
        let dir = fake_mkfs(RECORDING_MKFS);
        let _directory = ScratchDirectory(dir.clone());
        let mkfs = dir.join("mkfs.btrfs");
        let prepared = prepare_volume(VolumeSettings::default(), None, &mkfs, &dir, None).unwrap();
        let mut destination = FormatDestination::open(&disk.path).unwrap();
        std::fs::rename(&disk.path, &retained.path).unwrap();
        std::fs::write(&disk.path, b"replacement must survive").unwrap();

        let mut layout_report = Vec::new();
        format_layout(&mut destination, None, &mut layout_report).unwrap();
        assert_eq!(layout_report, b"1048576 537919488\n");
        assert_eq!(
            std::fs::read(&disk.path).unwrap(),
            b"replacement must survive"
        );
        let (offset, len) = volume_region(&mut destination.file, 512, DISK / 512).unwrap();
        write_at(&mut destination.file, offset, &[0xa5; 512]).unwrap();
        write_at(&mut destination.file, offset + len - 512, &[0x5a; 512]).unwrap();

        let mut volume_report = Vec::new();
        format_volume(prepared, &mut destination, &mut volume_report).unwrap();
        assert_eq!(volume_report, format!("{offset} {len} 0\n").as_bytes());
        assert_eq!(
            std::fs::read(&disk.path).unwrap(),
            b"replacement must survive"
        );
        assert_eq!(
            read_at(&mut destination.file, offset, 512).unwrap(),
            vec![0; 512]
        );
        assert_eq!(
            read_at(&mut destination.file, offset + len - 512, 512).unwrap(),
            vec![0; 512]
        );
        assert_eq!(
            volume_region(&mut destination.file, 512, DISK / 512).unwrap(),
            (offset, len)
        );
        assert_eq!(std::fs::metadata(&retained.path).unwrap().len(), DISK);
        drop(destination);
    }

    /// The volume region comes off the TABLE, so it is the partition the disk
    /// describes and not a recomputation that could have drifted from it.
    #[test]
    fn the_volume_region_is_the_partition_the_table_describes() {
        let scratch = Scratch::disk(DISK);
        run_layout(&scratch.path, &mut Vec::new()).unwrap();
        let plan = plan(512, DISK).unwrap();
        let mut file = File::open(&scratch.path).unwrap();
        let (offset, len) = volume_region(&mut file, 512, DISK / 512).unwrap();
        assert_eq!(offset, plan.volume_start * 512);
        assert_eq!(len, (plan.volume_end - plan.volume_start + 1) * 512);
        // ...and it ends on the last usable sector, so the partition the
        // filesystem is sized for is the whole of what the table set aside.
        assert_eq!(offset + len, (plan.volume_end + 1) * 512);
    }

    /// A table that is INTERNALLY consistent but describes a different disk is
    /// refused.
    ///
    /// `gpt::parse` is handed two byte slices and never learns where they came
    /// from, so it cannot catch this: every checksum is over the bytes, and a
    /// backup written somewhere other than the LBA its own header names is
    /// still a backup that verifies. Built here exactly that way — a table for
    /// a disk twice this size, with its backup placed where THIS disk's backup
    /// goes — because the consequence is a `td-volume` past the end of the
    /// destination, which a regular file silently EXTENDS to fit.
    #[test]
    fn a_table_describing_a_different_disk_is_refused() {
        let scratch = Scratch::disk(DISK);
        let claimed = (DISK * 2) / 512;
        let layout = gpt::Layout {
            sector_size: 512,
            disk_sectors: claimed,
            disk_guid: gpt::Guid::parse("12345678-1234-4234-8234-123456789abc").unwrap(),
            align_sectors: protocol::PARTITION_ALIGN_BYTES / 512,
            partitions: Vec::new(),
        };
        let forged = gpt::build(&layout).unwrap();
        {
            let mut file = OpenOptions::new().write(true).open(&scratch.path).unwrap();
            write_at(&mut file, forged.primary_offset, &forged.primary).unwrap();
            // ...at THIS disk's backup position, not the one the header names.
            let [_, backup] = table_ranges(512, DISK / 512).unwrap();
            write_at(&mut file, backup.0, &forged.backup).unwrap();
        }
        let mut file = File::open(&scratch.path).unwrap();
        let error = volume_region(&mut file, 512, DISK / 512).unwrap_err();
        assert!(
            format!("{error}").contains("-sector disk, not the"),
            "a table for another disk must be refused: {error}"
        );
    }

    /// The volume mkfs is ASKED for is the partition's own size.
    ///
    /// Nothing downstream can see this. `--byte-count` half the real length
    /// still leaves the superblock at 64 KiB, the label beside it and a mirror
    /// at 64 MiB, so every offset check in this crate and in the recipe passes
    /// while the filesystem reports less space than the partition it lives in.
    /// The argv is the only place the request itself is visible.
    #[test]
    fn mkfs_is_asked_for_the_partitions_own_size() {
        let scratch = Scratch::disk(DISK);
        run_layout(&scratch.path, &mut Vec::new()).unwrap();
        let dir = fake_mkfs(RECORDING_MKFS);
        run_volume(
            VolumeSettings::default(),
            None,
            &scratch.path,
            &dir.join("mkfs.btrfs"),
            &dir,
            None,
            &mut Vec::new(),
        )
        .unwrap();
        let argv = std::fs::read_to_string(dir.join("argv")).unwrap();
        let words: Vec<&str> = argv.lines().collect();
        let plan = plan(512, DISK).unwrap();
        let expected = (plan.volume_end - plan.volume_start + 1) * 512;
        let after = |flag: &str| {
            words
                .iter()
                .position(|w| *w == flag)
                .and_then(|i| words.get(i + 1))
                .map(|s| s.to_string())
        };
        assert_eq!(
            after("--byte-count"),
            Some(expected.to_string()),
            "mkfs was sized for something other than the partition: {argv:?}"
        );
        assert_eq!(
            after("--label"),
            Some(protocol::VOLUME_LABEL.to_string()),
            "the label is the one td-boot looks for: {argv:?}"
        );
        assert_eq!(
            after("--subvol"),
            Some(format!("rw:{}", protocol::VOLUME_SUBVOL)),
            "the subvolume is read-write and named: {argv:?}"
        );
        // The UUID is per-install and random, so what is pinned is that one was
        // asked for and that it PARSES — a malformed one mkfs would reject.
        let uuid = after("--uuid").unwrap_or_default();
        assert!(
            gpt::Guid::parse(&uuid).is_ok(),
            "the uuid is not a GUID: {uuid:?}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Two installs draw different volume UUIDs, so two td disks are not one
    /// filesystem as far as anything resolving by UUID is concerned.
    #[test]
    fn each_volume_gets_its_own_uuid() {
        let mut seen = Vec::new();
        for _ in 0..2 {
            let scratch = Scratch::disk(DISK);
            run_layout(&scratch.path, &mut Vec::new()).unwrap();
            let dir = fake_mkfs(RECORDING_MKFS);
            run_volume(
                VolumeSettings::default(),
                None,
                &scratch.path,
                &dir.join("mkfs.btrfs"),
                &dir,
                None,
                &mut Vec::new(),
            )
            .unwrap();
            let argv = std::fs::read_to_string(dir.join("argv")).unwrap();
            let words: Vec<&str> = argv.lines().collect();
            let uuid = words
                .iter()
                .position(|w| *w == "--uuid")
                .and_then(|i| words.get(i + 1))
                .map(|s| s.to_string())
                .unwrap();
            seen.push(uuid);
            std::fs::remove_dir_all(&dir).unwrap();
        }
        assert_ne!(
            seen.first(),
            seen.get(1),
            "two installs shared a volume UUID"
        );
    }

    /// A table with TWO partitions of the volume's name is refused rather than
    /// resolved by position: GPT does not make names unique, and picking the
    /// first formats whichever partition happens to be listed earlier.
    ///
    /// A partition of the right name and the WRONG TYPE is not a match at all —
    /// a name is 36 characters anyone can write, and the type GUID is what says
    /// what the partition is for.
    #[test]
    fn an_ambiguous_or_wrongly_typed_volume_is_refused() {
        let make = |parts: Vec<gpt::Partition>| {
            let scratch = Scratch::disk(DISK);
            let layout = gpt::Layout {
                sector_size: 512,
                disk_sectors: DISK / 512,
                disk_guid: gpt::Guid::parse("12345678-1234-4234-8234-123456789abc").unwrap(),
                align_sectors: protocol::PARTITION_ALIGN_BYTES / 512,
                partitions: parts,
            };
            let image = gpt::build(&layout).unwrap();
            {
                let mut file = OpenOptions::new().write(true).open(&scratch.path).unwrap();
                write_at(&mut file, image.primary_offset, &image.primary).unwrap();
                write_at(&mut file, image.backup_offset, &image.backup).unwrap();
            }
            let mut file = File::open(&scratch.path).unwrap();
            let error = volume_region(&mut file, 512, DISK / 512).unwrap_err();
            format!("{error}")
        };
        // Distinct unique GUIDs, because `gpt::build` refuses two of one — the
        // ambiguity under test here is of the NAME, which nothing refuses.
        let volume = |start: u64, end: u64, type_guid: gpt::Guid, tag: u8| gpt::Partition {
            type_guid,
            unique_guid: gpt::Guid::parse(&format!("00000000-0000-4000-8000-00000000000{tag}"))
                .unwrap(),
            start_lba: start,
            end_lba: end,
            attributes: 0,
            name: protocol::VOLUME_PARTITION_NAME.to_string(),
        };
        let two = make(vec![
            volume(2048, 4095, gpt::TYPE_LINUX_FS, 1),
            volume(4096, 8191, gpt::TYPE_LINUX_FS, 2),
        ]);
        assert!(
            two.contains("has 2 partitions named"),
            "two of the name must be refused: {two}"
        );
        let wrong_type = make(vec![volume(2048, 4095, gpt::TYPE_ESP, 3)]);
        assert!(
            wrong_type.contains("no td-volume partition on this disk"),
            "the name alone is not a match: {wrong_type}"
        );
    }

    /// The ENGINE refuses to place a partition over the table, which is the
    /// first of the two answers to a volume that would overwrite one.
    ///
    /// The second is `volume_region`'s own `overlaps a partition table` bound,
    /// and NO TEST HERE REACHES IT — deliberately recorded rather than left to
    /// be discovered. `gpt::parse` bounds partitions by the header's own
    /// `first_usable`, a field in the same table, so a hand-forged table
    /// declaring a usable range over its own entry array would be
    /// self-consistent and would reach it. Building one means re-sealing two
    /// headers and an entry-array CRC by hand, which is `gpt.rs`'s job
    /// reimplemented in a test that would then pass while the real sealing
    /// changed underneath it. So the bound stays as what it is — a check on the
    /// last value before a raw write to somebody's disk — and this test pins
    /// the reachable half.
    #[test]
    fn a_volume_over_the_table_cannot_even_be_built() {
        let layout = gpt::Layout {
            sector_size: 512,
            disk_sectors: DISK / 512,
            disk_guid: gpt::Guid::parse("12345678-1234-4234-8234-123456789abc").unwrap(),
            align_sectors: protocol::PARTITION_ALIGN_BYTES / 512,
            partitions: vec![gpt::Partition {
                type_guid: gpt::TYPE_LINUX_FS,
                unique_guid: gpt::Guid::parse("00000000-0000-4000-8000-000000000002").unwrap(),
                // LBA 2 is inside the primary entry array at any sector size.
                start_lba: 2,
                end_lba: 2047,
                attributes: 0,
                name: protocol::VOLUME_PARTITION_NAME.to_string(),
            }],
        };
        let error = gpt::build(&layout).unwrap_err();
        assert!(
            error.contains("the table itself occupies through"),
            "the engine must refuse a partition over its own table: {error}"
        );
    }

    /// A scratch image that is a SYMLINK is replaced, not followed — otherwise
    /// `File::create` truncates whatever it points at and grows it to the
    /// partition's size, under an installer that is usually root.
    #[test]
    fn a_symlinked_scratch_image_is_replaced_rather_than_followed() {
        let dir = fake_mkfs(RECORDING_MKFS);
        let scratch = Scratch::disk(DISK);
        run_layout(&scratch.path, &mut Vec::new()).unwrap();
        let bystander = dir.join("someone-elses-file");
        std::fs::write(&bystander, b"do not truncate me").unwrap();
        std::os::unix::fs::symlink(&bystander, dir.join("td-volume.img")).unwrap();
        run_volume(
            VolumeSettings::default(),
            None,
            &scratch.path,
            &dir.join("mkfs.btrfs"),
            &dir,
            None,
            &mut Vec::new(),
        )
        .unwrap();
        assert_eq!(
            std::fs::read(&bystander).unwrap(),
            b"do not truncate me",
            "the symlink's target was written through"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A destination with no table at all is refused, and SO IS one with a
    /// valid table that has no `td-volume` in it — a different branch, and the
    /// one the diagnostic is written for.
    #[test]
    fn a_volume_needs_a_layout_first() {
        let scratch = Scratch::disk(DISK);
        let mut file = File::open(&scratch.path).unwrap();
        let error = volume_region(&mut file, 512, DISK / 512).unwrap_err();
        assert!(
            format!("{error}").contains("gpt:"),
            "an unlaid-out disk is refused by the parser: {error}"
        );

        // A well-formed table for this disk, carrying no partitions.
        let layout = gpt::Layout {
            sector_size: 512,
            disk_sectors: DISK / 512,
            disk_guid: gpt::Guid::parse("12345678-1234-4234-8234-123456789abc").unwrap(),
            align_sectors: protocol::PARTITION_ALIGN_BYTES / 512,
            partitions: Vec::new(),
        };
        let empty = gpt::build(&layout).unwrap();
        {
            let mut file = OpenOptions::new().write(true).open(&scratch.path).unwrap();
            write_at(&mut file, empty.primary_offset, &empty.primary).unwrap();
            write_at(&mut file, empty.backup_offset, &empty.backup).unwrap();
        }
        let mut file = File::open(&scratch.path).unwrap();
        let error = volume_region(&mut file, 512, DISK / 512).unwrap_err();
        assert!(
            format!("{error}").contains("no td-volume partition on this disk"),
            "a table without the volume must say so: {error}"
        );
    }

    /// The line `run_volume` reports is the volume's own geometry, and a
    /// `mkfs` that wrote nothing copies nothing.
    ///
    /// The stand-in is a shell script rather than the real thing: no host is
    /// required to have `mkfs.btrfs`, and what is under test here is the
    /// arithmetic and the reporting, not the filesystem. It leaves the image
    /// all zeros, so `written` is 0 — which is pinned too, since a copy that
    /// wrote something out of an empty image would be inventing it.
    ///
    /// What this canNOT cover is the child's stdout staying out of the
    /// process's own, which is what the parent commit's recipe check caught:
    /// `out` here is a `Vec`, so a child inheriting fd 1 is invisible to it.
    /// `tests/stdout_is_a_data_channel.rs` runs the real binary for that.
    #[test]
    fn the_reported_line_is_the_volumes_geometry() {
        let scratch = Scratch::disk(DISK);
        run_layout(&scratch.path, &mut Vec::new()).unwrap();
        let dir = fake_mkfs("#!/bin/sh\necho 'btrfs-progs v7.0'\n");
        let mut out = Vec::new();
        run_volume(
            VolumeSettings::default(),
            None,
            &scratch.path,
            &dir.join("mkfs.btrfs"),
            &dir,
            None,
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        let fields: Vec<&str> = text.split_whitespace().collect();
        assert_eq!(
            fields.len(),
            3,
            "the line is <off> <len> <written>: {text:?}"
        );
        let plan = plan(512, DISK).unwrap();
        assert_eq!(
            fields.first().map(|f| f.parse::<u64>().unwrap()),
            Some(plan.volume_start * 512)
        );
        assert_eq!(
            fields.get(1).map(|f| f.parse::<u64>().unwrap()),
            Some((plan.volume_end - plan.volume_start + 1) * 512)
        );
        assert_eq!(fields.get(2).map(|f| f.parse::<u64>().unwrap()), Some(0));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A destination whose PATH has a space in it does not shift the fields a
    /// caller reads by position. Nothing but numbers goes on that channel, so
    /// this holds by construction rather than by escaping — which is the point:
    /// escaping is a rule every future field has to remember.
    #[test]
    fn a_destination_with_a_space_in_its_name_does_not_shift_the_fields() {
        let dir = scratch::path("space");
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("a disk.img");
        File::create(&path).unwrap().set_len(DISK).unwrap();
        let mut out = Vec::new();
        run_layout(&path, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert_eq!(text.lines().count(), 1, "one line: {text:?}");
        let fields: Vec<&str> = text.split_whitespace().collect();
        assert_eq!(fields.len(), 2, "the line is <esp> <volume>: {text:?}");
        for field in &fields {
            assert!(field.parse::<u64>().is_ok(), "{field:?} is not a number");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_preselected_uuid_is_forwarded_exactly_to_the_formatter() {
        let uuid = "12345678-90ab-cdef-1234-567890abcdef";
        let scratch = Scratch::disk(DISK);
        run_layout(&scratch.path, &mut Vec::new()).unwrap();
        let dir = fake_mkfs(RECORDING_MKFS);
        let mode = parse_args(args(&[
            "volume",
            "--uuid",
            uuid,
            scratch.path.to_str().unwrap(),
            dir.join("mkfs.btrfs").to_str().unwrap(),
            dir.to_str().unwrap(),
        ]))
        .unwrap();
        let Mode::Volume {
            boot: None,
            uuid: Some(identity),
            timezone: None,
            hostname: None,
            username: None,
            destination,
            mkfs,
            scratch: staging_scratch,
            seed,
        } = mode
        else {
            panic!("preselected UUID was not retained");
        };
        run_volume(
            VolumeSettings::default(),
            Some(&identity),
            &destination,
            &mkfs,
            &staging_scratch,
            seed.as_ref(),
            &mut Vec::new(),
        )
        .unwrap();
        let argv = std::fs::read_to_string(dir.join("argv")).unwrap();
        let words: Vec<_> = argv.lines().collect();
        assert_eq!(
            words
                .windows(2)
                .filter(|pair| pair[0] == "--uuid")
                .collect::<Vec<_>>(),
            vec![&["--uuid", uuid][..]]
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn uuid_options_require_canonical_bytes_and_exact_placement() {
        let uuid = "12345678-90ab-cdef-1234-567890abcdef";
        for bad in [
            "",
            "00000000-0000-0000-0000-000000000000",
            "12345678-90AB-cdef-1234-567890abcdef",
            "1234567890abcdef1234567890abcdef",
            "12345678-90ab-cdef-1234-567890abcdeg",
            "12345678-90ab-cdef-1234-567890abcdef\n",
        ] {
            assert!(
                parse_args(args(&["volume", "--uuid", bad, "disk", "/mkfs", "scratch"])).is_err()
            );
        }
        for bad in [
            vec!["volume", "--uuid"],
            vec!["volume", "--uuid", uuid],
            vec![
                "volume", "--uuid", uuid, "--uuid", uuid, "disk", "/mkfs", "scratch",
            ],
            vec!["volume", "disk", "/mkfs", "scratch", "--uuid", uuid],
            vec![
                "volume", "--uuid", uuid, "--uuid", uuid, "disk", "/mkfs", "scratch", "key",
            ],
            vec!["layout", "--uuid", uuid, "disk"],
        ] {
            assert!(parse_args(args(&bad)).is_err(), "{bad:?}");
        }
        use std::os::unix::ffi::OsStringExt;
        let mut non_utf8 = args(&["volume", "--uuid"]).collect::<Vec<_>>();
        non_utf8.push(OsString::from_vec(vec![0xff; 36]));
        non_utf8.extend(args(&["disk", "/mkfs", "scratch"]));
        assert!(parse_args(non_utf8.into_iter()).is_err());
        assert!(parse_args(args(&[
            "volume", "--uuid", uuid, "disk", "/mkfs", "scratch", "/td-boot", "source", "key"
        ]))
        .is_ok());
    }

    /// The publish arguments are three or none, never a defaulted subset.
    #[test]
    fn a_publish_is_all_three_arguments_or_none() {
        assert_eq!(
            parse_args(args(&[
                "volume",
                "/dev/sda",
                "/bin/mkfs.btrfs",
                "/tmp",
                "/bin/td-boot",
                "/media/deployment",
                "/media/key.pub",
            ]))
            .unwrap(),
            Mode::Volume {
                boot: None,
                uuid: None,
                timezone: None,
                hostname: None,
                username: None,
                destination: PathBuf::from("/dev/sda"),
                mkfs: PathBuf::from("/bin/mkfs.btrfs"),
                scratch: PathBuf::from("/tmp"),
                seed: Some(VolumeSeed::Publish(Publish {
                    td_boot: PathBuf::from("/bin/td-boot"),
                    deployment: PathBuf::from("/media/deployment"),
                    trusted_key: PathBuf::from("/media/key.pub"),
                })),
            }
        );
        // Four and five arguments are a caller who asked for something this
        // cannot do. Defaulting the missing one is how a fail-open gets in —
        // the key most of all, whose absence is what td-boot reads as "publish
        // without checking".
        for short in [
            vec![
                "volume",
                "/dev/sda",
                "/bin/mkfs.btrfs",
                "/tmp",
                "/bin/td-boot",
            ],
            vec![
                "volume",
                "/dev/sda",
                "/bin/mkfs.btrfs",
                "/tmp",
                "/bin/td-boot",
                "/media/deployment",
            ],
        ] {
            assert!(parse_args(args(&short)).is_err(), "{short:?} is incomplete");
        }
        assert!(
            parse_args(args(&[
                "volume",
                "/dev/sda",
                "/bin/mkfs.btrfs",
                "/tmp",
                "/bin/td-boot",
                "/media/deployment",
                "/media/key.pub",
                "extra",
            ]))
            .is_err(),
            "a seventh argument is not silently ignored"
        );
    }

    #[test]
    fn trust_only_arguments_are_explicit_and_cannot_mix_with_publish() {
        assert_eq!(
            parse_args(args(&[
                "volume",
                "disk",
                "/mkfs",
                "scratch",
                "--trusted-key",
                "key"
            ]))
            .unwrap(),
            Mode::Volume {
                boot: None,
                uuid: None,
                timezone: None,
                hostname: None,
                username: None,
                destination: PathBuf::from("disk"),
                mkfs: PathBuf::from("/mkfs"),
                scratch: PathBuf::from("scratch"),
                seed: Some(VolumeSeed::Trust(PathBuf::from("key"))),
            }
        );
        for bad in [
            vec!["volume", "disk", "/mkfs", "scratch", "--trusted-key"],
            vec![
                "volume",
                "disk",
                "/mkfs",
                "scratch",
                "--trusted-key",
                "key",
                "extra",
            ],
            vec![
                "volume",
                "disk",
                "/mkfs",
                "scratch",
                "--trusted-key",
                "--trusted-key",
            ],
            vec!["volume", "--trusted-key", "key", "disk", "/mkfs", "scratch"],
            vec!["layout", "disk", "--trusted-key", "key"],
            vec![
                "volume",
                "disk",
                "/mkfs",
                "scratch",
                "/td-boot",
                "/deploy",
                "--trusted-key",
                "key",
            ],
            vec![
                "volume",
                "disk",
                "/mkfs",
                "scratch",
                "/td-boot",
                "/deploy",
                "key1",
                "--trusted-key",
                "key2",
            ],
        ] {
            assert!(parse_args(args(&bad)).is_err(), "accepted {bad:?}");
        }
    }

    #[test]
    fn trust_only_arguments_retain_the_preselected_uuid() {
        let uuid = "12345678-1234-4234-8234-123456789abc";
        assert_eq!(
            parse_args(args(&[
                "volume",
                "--uuid",
                uuid,
                "disk",
                "/mkfs",
                "scratch",
                "--trusted-key",
                "key"
            ]))
            .unwrap(),
            Mode::Volume {
                boot: None,
                uuid: Some(VolumeUuid(uuid.into())),
                timezone: None,
                hostname: None,
                username: None,
                destination: PathBuf::from("disk"),
                mkfs: PathBuf::from("/mkfs"),
                scratch: PathBuf::from("scratch"),
                seed: Some(VolumeSeed::Trust(PathBuf::from("key"))),
            }
        );
    }

    #[test]
    fn trust_only_formatting_prepares_an_empty_publication_root() {
        use std::os::unix::fs::PermissionsExt;
        let scratch = Scratch::disk(DISK);
        run_layout(&scratch.path, &mut Vec::new()).unwrap();
        let dir = fake_mkfs(RECORDING_MKFS);
        let key = key_file(&dir);
        let mut out = Vec::new();
        run_volume(
            VolumeSettings::default(),
            None,
            &scratch.path,
            &dir.join("mkfs.btrfs"),
            &dir,
            Some(&VolumeSeed::Trust(key.clone())),
            &mut out,
        )
        .unwrap();
        let root = dir.join("td-volume-root");
        assert!(root.join("@var").is_dir());
        for path in ["td", "td/boot", "td/deployments", "td/incoming"] {
            let path = root.join(path);
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o755
            );
            if path != root.join("td") {
                assert_eq!(std::fs::read_dir(&path).unwrap().count(), 0);
            }
        }
        let mut entries: Vec<_> = std::fs::read_dir(root.join("td"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        entries.sort();
        assert_eq!(
            entries,
            ["boot", "deployments", "incoming", "trusted.pub"].map(OsString::from)
        );
        let carried = root.join("td/trusted.pub");
        assert_eq!(
            std::fs::read(&carried).unwrap(),
            std::fs::read(&key).unwrap()
        );
        assert_eq!(
            std::fs::metadata(&carried).unwrap().permissions().mode() & 0o777,
            0o644
        );
        assert!(!dir.join("td-install-key/td-trusted.pub").exists());
        assert_eq!(
            String::from_utf8(out).unwrap().split_whitespace().count(),
            3
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn invalid_trust_only_key_preserves_destination_and_scratch() {
        let scratch = Scratch::disk(4096);
        let before = std::fs::read(&scratch.path).unwrap();
        let dir = fake_mkfs(RECORDING_MKFS);
        let staging = dir.join("td-volume-root");
        std::fs::create_dir(&staging).unwrap();
        std::fs::write(staging.join("keep"), b"untouched").unwrap();
        let missing = dir.join("missing");
        let link = dir.join("symlink");
        std::os::unix::fs::symlink(key_file(&dir), &link).unwrap();
        let oversized = dir.join("oversized");
        std::fs::write(
            &oversized,
            vec![b'x'; protocol::MAX_PUBLIC_KEY_BYTES as usize + 1],
        )
        .unwrap();
        for key in [missing, link, oversized] {
            let mut out = Vec::new();
            let error = run_volume(
                VolumeSettings::default(),
                None,
                &scratch.path,
                &dir.join("mkfs.btrfs"),
                &dir,
                Some(&VolumeSeed::Trust(key)),
                &mut out,
            )
            .unwrap_err();
            assert!(
                error.to_string().contains("trusted deployment key"),
                "{error}"
            );
            assert!(out.is_empty());
            assert_eq!(std::fs::read(&scratch.path).unwrap(), before);
            assert_eq!(std::fs::read(staging.join("keep")).unwrap(), b"untouched");
            assert!(!dir.join("argv").exists());
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// 64 lowercase hex, because that is what a deployment id is: `td-install`
    /// checks the shape before joining it onto a path, so a stand-in printing
    /// anything else is exercising the refusal.
    const STAND_IN_ID: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    /// A trusted key on disk. Real bytes and a real file, because the key is
    /// carried onto the volume and so has to be readable — a stand-in td-boot
    /// ignores it, but `seed_into` does not.
    fn key_file(dir: &Path) -> PathBuf {
        let path = dir.join("key.pub");
        std::fs::write(&path, format!("{}\n", "ab".repeat(32))).unwrap();
        path
    }

    /// A `td-boot` stand-in that publishes: runs `body`, then does the two
    /// things `seed_into` reads back — creates `td/deployments/<id>` and
    /// prints that id.
    fn publishing_td_boot(path: &Path, body: &str) {
        scratch::executable(
            path,
            &format!(
                "#!/bin/sh\n{body}mkdir -p \"$2/{}/{STAND_IN_ID}\"\necho {STAND_IN_ID}\n",
                protocol::DEPLOYMENTS_DIR
            ),
        )
        .unwrap();
    }

    /// The publish runs BEFORE mkfs, into the tree `--rootdir` bakes in, and
    /// hands td-boot exactly what it was told to — the three directories the
    /// one writer requires already made.
    #[test]
    fn the_publish_reaches_td_boot_before_the_filesystem_is_made() {
        let scratch = Scratch::disk(DISK);
        run_layout(&scratch.path, &mut Vec::new()).unwrap();
        // Both stand-ins APPEND their name to one file, so the order is read off
        // that file rather than inferred. Nothing else here can see the order:
        // a publish that ran after mkfs would still see the directories made
        // and still write its witness, which is how this test passed the
        // mutation it is named for until the log existed.
        let dir = fake_mkfs(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$(dirname \"$0\")/argv\"\n\
             echo mkfs >> \"$(dirname \"$0\")/order\"\n",
        );
        // A td-boot stand-in that records its argv and proves the staging tree
        // was ready when it ran: it writes into the deployments directory,
        // which only exists if `seed_into` made it first. It also DOES what
        // a publish does — makes `td/deployments/<id>` and names it on stdout —
        // because that is now the contract, and a stand-in that did less would
        // be testing the refusal rather than the path.
        let td_boot = dir.join("td-boot");
        publishing_td_boot(
            &td_boot,
            "printf '%s\\n' \"$@\" > \"$(dirname \"$0\")/publish-argv\"\n\
             echo publish >> \"$(dirname \"$0\")/order\"\n",
        );
        let key = key_file(&dir);
        let publish = Publish {
            td_boot: td_boot.clone(),
            deployment: PathBuf::from("/media/deployment"),
            trusted_key: key.clone(),
        };
        let mut out = Vec::new();
        run_volume(
            VolumeSettings::default(),
            None,
            &scratch.path,
            &dir.join("mkfs.btrfs"),
            &dir,
            Some(&VolumeSeed::Publish(publish)),
            &mut out,
        )
        .unwrap();

        let argv = std::fs::read_to_string(dir.join("publish-argv")).unwrap();
        let words: Vec<&str> = argv.lines().collect();
        // Canonical on BOTH sides: the argument is resolved, so a host whose
        // temporary directory is itself a symlink would otherwise fail this on
        // the spelling rather than on anything it is about.
        let staging = std::fs::canonicalize(dir.join("td-volume-root")).unwrap();
        // The key argument is the SNAPSHOT, not the path this program was
        // given: handing td-boot the caller's path would be a second read of
        // it, which is the race the snapshot exists to close. Asserted as the
        // exact path rather than merely "not the original", so a snapshot
        // written somewhere unexpected is a failure too.
        let snapshot = std::fs::canonicalize(&dir)
            .unwrap()
            .join("td-install-key")
            .join("td-trusted.pub");
        assert_ne!(snapshot, key, "the snapshot and the given key are one file");
        assert_eq!(
            words,
            vec![
                "publish",
                staging.to_str().unwrap(),
                "/media/deployment",
                snapshot.to_str().unwrap(),
            ],
            "td-boot was not asked to publish into the staging tree: {argv:?}"
        );
        // ...and it is BESIDE the volume root, never inside it: td-boot must
        // not read its trust root out of the tree it is writing.
        assert!(
            !snapshot.starts_with(&staging),
            "the trust root snapshot is inside the volume root"
        );
        // The snapshot is gone once promoted — it was RENAMED, so the volume's
        // key is the very file td-boot authenticated under.
        assert!(
            !snapshot.exists(),
            "the snapshot was copied rather than renamed"
        );
        assert!(
            staging
                .join(protocol::DEPLOYMENTS_DIR)
                .join(STAND_IN_ID)
                .is_dir(),
            "the deployments directory was not there when td-boot ran"
        );
        let order = std::fs::read_to_string(dir.join("order")).unwrap();
        assert_eq!(
            order.lines().collect::<Vec<_>>(),
            vec!["publish", "mkfs"],
            "the deployment must be in the tree before `--rootdir` reads it"
        );
        // The staging tree still carries @var, so the publish did not displace
        // what the volume already needed.
        assert!(staging.join(protocol::VOLUME_SUBVOL).is_dir());
        // The reported line is still three fields. That the id did not reach fd
        // 1 is NOT checkable here — `out` is a `Vec`, so a child handed
        // `Stdio::inherit()` writes past it and this stays green; that mutation
        // is caught by the subprocess test instead.
        let text = String::from_utf8(out).unwrap();
        assert_eq!(text.split_whitespace().count(), 3, "{text:?}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The volume keeps the trust root, so the installed machine can
    /// authenticate its own updates — and keeps it only once the publish that
    /// used those bytes has succeeded.
    #[test]
    fn the_volume_carries_the_key_that_authenticated_it() {
        let scratch = Scratch::disk(DISK);
        run_layout(&scratch.path, &mut Vec::new()).unwrap();
        let dir = fake_mkfs(RECORDING_MKFS);
        let td_boot = dir.join("td-boot");
        publishing_td_boot(&td_boot, "");
        let key = key_file(&dir);
        let publish = Publish {
            td_boot,
            deployment: PathBuf::from("/media/deployment"),
            trusted_key: key.clone(),
        };
        run_volume(
            VolumeSettings::default(),
            None,
            &scratch.path,
            &dir.join("mkfs.btrfs"),
            &dir,
            Some(&VolumeSeed::Publish(publish)),
            &mut Vec::new(),
        )
        .unwrap();

        let staging = dir.join("td-volume-root");
        let carried = staging.join(protocol::VOLUME_TRUSTED_KEY);
        assert_eq!(
            std::fs::read(&carried).unwrap(),
            std::fs::read(&key).unwrap(),
            "the volume's key is not the one the install was given"
        );
        // Where that path is, and that it is relative, are properties of the
        // constant and are pinned in their own test rather than asserted here
        // against themselves.
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&carried).unwrap().permissions().mode() & 0o777,
            0o644
        );

        // The update CHANNEL, empty and 0755. `td-boot update` treats a missing
        // channel as a configuration fault rather than as nothing to do, so a
        // machine installed without one fails every timer tick — and the qemu
        // harness stages its own, so no oracle would ever show it. Asserted on
        // the staging tree because that is what `--rootdir` reads, and empty
        // directories do survive into the image.
        let channel = staging.join(protocol::VOLUME_CHANNEL_DIR);
        assert!(
            channel.is_dir(),
            "the volume must carry an update channel: {}",
            channel.display()
        );
        assert_eq!(
            std::fs::read_dir(&channel).unwrap().count(),
            0,
            "a freshly installed channel has been offered nothing"
        );
        assert_eq!(
            std::fs::metadata(&channel).unwrap().permissions().mode() & 0o777,
            0o755
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The snapshot must not LAUNDER a key past td-boot's own reader.
    ///
    /// td-boot refuses a symlink, a non-regular file, and anything over
    /// `MAX_PUBLIC_KEY_BYTES`. Handing it a copy would turn every one of those
    /// refusals into a successful install, because the copy is a small regular
    /// file whatever the original was — so the same rule is applied here, and
    /// each refusal is checked by its REASON rather than by failing at all: a
    /// non-zero result is satisfied by any error, including one from a later
    /// step that would mean the rule never ran.
    #[test]
    fn a_key_td_boot_would_refuse_is_refused_here_too() {
        let dir = fake_mkfs(RECORDING_MKFS);
        let real = key_file(&dir);

        let link = dir.join("link.pub");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let refused = read_trusted_key(&link).unwrap_err().to_string();
        assert!(refused.contains("must be a real regular file"), "{refused}");

        let big = dir.join("big.pub");
        std::fs::write(
            &big,
            vec![b'a'; protocol::MAX_PUBLIC_KEY_BYTES as usize + 1],
        )
        .unwrap();
        let refused = read_trusted_key(&big).unwrap_err().to_string();
        // The whole phrase: the message carries the path, which holds a pid,
        // and a pid containing the bound would satisfy a bare digit check.
        assert!(
            refused.contains(&format!(
                "trusted deployment key exceeds {} bytes",
                protocol::MAX_PUBLIC_KEY_BYTES
            )),
            "{refused}"
        );

        let refused = read_trusted_key(&dir).unwrap_err().to_string();
        assert!(refused.contains("must be a real regular file"), "{refused}");

        // A FIFO is the case the type check must catch BEFORE the open rather
        // than after it: `File::open` on one blocks until a writer appears, so
        // a version that opened first hung here with no diagnostic instead of
        // failing. Nothing writes to this one, so a regression does not fail
        // this test — it hangs it, which is the honest signal.
        let fifo = dir.join("fifo.pub");
        let made = std::process::Command::new("mkfifo").arg(&fifo).status();
        if made.map(|status| status.success()).unwrap_or(false) {
            let refused = read_trusted_key(&fifo).unwrap_err().to_string();
            assert!(refused.contains("must be a real regular file"), "{refused}");
        }

        // The bound is applied to what is READ and not only to what `stat`
        // claimed, since the two can disagree — a file that grew between them
        // would otherwise be cut to a valid length and laundered past td-boot's
        // refusal. `/proc/self/status` is that disagreement without a race to
        // arrange: a regular file whose reported length is 0 and whose
        // contents are not.
        let proc_status = Path::new("/proc/self/status");
        assert_eq!(std::fs::metadata(proc_status).unwrap().len(), 0);
        let refused = read_trusted_key(proc_status).unwrap_err().to_string();
        assert!(refused.contains("changed while reading"), "{refused}");

        // ...and the bound is not so tight that a key AT it is refused, nor so
        // loose that a real key trips it.
        let edge = dir.join("edge.pub");
        std::fs::write(&edge, vec![b'a'; protocol::MAX_PUBLIC_KEY_BYTES as usize]).unwrap();
        assert_eq!(
            read_trusted_key(&edge).unwrap().len(),
            protocol::MAX_PUBLIC_KEY_BYTES as usize
        );
        assert_eq!(
            read_trusted_key(&real).unwrap(),
            std::fs::read(&real).unwrap()
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A path this program cannot open NAMES ITSELF.
    ///
    /// `io::Error` carries an errno and nothing else, so the destination — the
    /// one argument an operator is most likely to mistype, and the one that is
    /// a device node on a real install — refused with a bare `No such file or
    /// directory` on a command line naming up to five paths. The key already
    /// named itself, through `realfile`; this is the rest of them.
    ///
    /// Both verbs, because they open the destination independently and only
    /// `layout` was ever driven with a bad one.
    #[test]
    fn a_path_that_cannot_be_opened_names_itself() {
        let dir = scratch::path("named");
        std::fs::create_dir(&dir).unwrap();

        let absent = dir.join("no-such-disk");
        let refused = run_layout(&absent, &mut Vec::new())
            .unwrap_err()
            .to_string();
        assert!(
            refused.contains(&absent.display().to_string()),
            "layout must name the destination it could not open, got {refused:?}"
        );

        let refused = run_volume(
            VolumeSettings::default(),
            None,
            &absent,
            &dir.join("mkfs.btrfs"),
            &dir,
            None,
            &mut Vec::new(),
        )
        .unwrap_err()
        .to_string();
        assert!(
            refused.contains(&absent.display().to_string()),
            "volume must name the destination it could not open, got {refused:?}"
        );

        // A DIRECTORY is the other shape a mistyped destination takes, and it
        // fails at a different call than an absent one.
        let refused = run_layout(&dir, &mut Vec::new()).unwrap_err().to_string();
        assert!(
            refused.contains(&dir.display().to_string()),
            "layout must name a destination that is not a file, got {refused:?}"
        );

        // The key's own refusal is `realfile`'s and predates this; asserted
        // here so the property is stated over every path the verbs take.
        let refused = read_trusted_key(&absent).unwrap_err().to_string();
        assert!(
            refused.contains(&absent.display().to_string()),
            "the trusted key must name itself, got {refused:?}"
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// NOTHING OUTSIDE THE CHOKE POINTS TOUCHES THE FILESYSTEM, AND THE
    /// COMPILER IS WHAT SAYS SO.
    ///
    /// `clippy.toml` disallows every path-taking entry point into the
    /// filesystem and `Cargo.toml` denies the lint, so such a call outside
    /// the choke points is a BUILD failure and not a test failure. That
    /// pair IS the roster. What is left for a test is the attribute that
    /// opens a hole in it, and an attribute is text.
    ///
    /// This replaced a source scan over six files, and the scan is the reason
    /// the roster moved. Nine rounds of review walked out of it, every time
    /// through a spelling it did not model: an import, an alias, a turbofish,
    /// a qualified path, a raw identifier, a macro-assembled callee, a line
    /// break between two tokens, a string holding `fn `. Clippy resolves a
    /// PATH, so every one of those is the same call to it. The last round
    /// found the one that ended the argument rather than extending it: a
    /// wrapper taking `sidecar: &str` opens a file, because `File::open`
    /// takes `impl AsRef<Path>` — no text can tell that a parameter is a path
    /// when its own type does not say so, and a compiler never had to ask.
    #[test]
    fn the_lint_is_allowed_only_at_the_choke_points() {
        for (label, source, expected) in compiled_files() {
            // Every ATTRIBUTE that suppresses ANY lint, rather than every
            // mention of this one. Review reached the shipped code through
            // five spellings that never name it: `#[allow(clippy::all)]`,
            // `#[allow(clippy::style)]` — the group it is IN —
            // `#[expect(clippy::style)]`, a crate-level `#![allow(clippy::all)]`
            // and `#[allow(clippy :: disallowed_methods)]`, which rustc accepts
            // and a search for the name walks past. So the roster is the
            // ATTRIBUTES this crate permits, spaces removed, and every other
            // one is refused whatever lint it names.
            //
            // Over the UNCOMMENTED source, or the likeliest author of the next
            // false red is a td-boot developer explaining the allow this crate
            // put in their file.
            let text = uncommented(source);
            let lines: Vec<&str> = text.lines().collect();
            let mut seen = Vec::new();
            for (unit, item) in units(&lines) {
                if !suppresses_a_lint(&unit) {
                    continue;
                }
                assert!(
                    unit == unspaced(ALLOW),
                    "{label} before line {item}: a lint suppression this crate \
                     does not permit: {unit}"
                );
                seen.push(lines.get(item).copied().unwrap_or_default().trim());
            }
            assert_eq!(
                seen, expected,
                "{label}: the allow sits on an item this test does not know — \
                 either an allow moved, or a choke point's signature changed \
                 under MAIN_CHOKE/REALFILE_CHOKE/SHA256_CHOKE"
            );
        }
    }

    /// Whether `squeezed` — one line with its whitespace removed — is an
    /// ATTRIBUTE that turns a lint off.
    ///
    /// The source as ATTRIBUTE UNITS, whitespace removed, each paired with the
    /// index of the line AFTER it — the item it sits on.
    ///
    /// A line that opens an attribute and does not close it takes the lines
    /// that finish it. Review spread a suppression over four:
    ///
    /// ```ignore
    /// #[cfg_attr(
    ///     all(),
    ///     allow(clippy::all)
    /// )]
    /// ```
    ///
    /// where the opener carries no `allow(` and the `allow(` carries no
    /// opener, so a line-at-a-time scan saw neither half.
    fn units(lines: &[&str]) -> Vec<(String, usize)> {
        // Brackets OUTSIDE strings, or a `reason = "…[…"` joins the rest of
        // the file to itself.
        let unbalanced = |text: &str| {
            let plain = unstringed(text);
            plain.matches('[').count() > plain.matches(']').count()
        };
        let mut out = Vec::new();
        let mut index = 0usize;
        while let Some(line) = lines.get(index) {
            let mut unit = (*line).to_string();
            let mut end = index;
            // A line that BEGINS one, not one that merely mentions `#[` — this
            // scan reads its own source, where `"#["` appears in a string, and
            // joining from there swallowed the file. A multiline attribute
            // sharing a line with code is therefore not joined; that is one
            // coincidence past what review demonstrated and is a known limit.
            // UNSPACED, since `# [cfg_attr(` is an attribute to rustc and was
            // not one to this — the same raw-versus-unspaced split the scan
            // below had already been taught, one layer up. A line merely
            // MENTIONING `#[` in a string still does not open one, because
            // its unspaced form does not start with it.
            let opener = unspaced(&unit);
            if opener.starts_with("#[") || opener.starts_with("#![") {
                while unbalanced(&unit) {
                    end = end.saturating_add(1);
                    match lines.get(end) {
                        None => break,
                        Some(next) => unit.push_str(next),
                    }
                }
            }
            index = end.saturating_add(1);
            // The ITEM an attribute sits on is the next line with anything on
            // it. A blank line or a comment between the two is legal Rust and
            // rustc applies the attribute across either, so reporting the
            // line immediately below reds a file that is correct — and
            // `uncommented` has already blanked the comments, which makes the
            // two cases one skip.
            let mut item = index;
            while lines.get(item).is_some_and(|line| line.trim().is_empty()) {
                item = item.saturating_add(1);
            }
            out.push((unspaced(&unit), item));
        }
        out
    }

    /// The three words that lower a lint's level. `warn(` is here because
    /// `#[warn(clippy::disallowed_methods)]` turns the deny into a warning and
    /// the preflight passes no `-D warnings` — review found it, an eighth
    /// spelling after the seven the scan was built for.
    const LOWERS: [&str; 3] = ["allow(", "expect(", "warn("];

    /// The four spellings that can reach THIS lint: itself, the group it is
    /// in, the group that holds every clippy lint, and rustc's group that
    /// holds every lint at all. A suppression naming none of them cannot
    /// touch it.
    ///
    /// Named rather than refused wholesale, because the wholesale rule was a
    /// cross-crate landmine: `#[allow(clippy::too_many_arguments)]` in
    /// `engine/src/gpt.rs` reds td-install, and its author is an engine
    /// developer with no reason to know td-install compiles that file.
    ///
    /// The first is the SINGULAR, which is a prefix of the plural and so
    /// covers both in one entry. `clippy::disallowed_method` is the lint's
    /// pre-1.55 name and still a RENAME ALIAS: rustc resolves it and the
    /// allow takes effect, which review measured — with the rename note
    /// itself removable on the same line by
    /// `#[allow(renamed_and_removed_lints, …)]`, leaving no diagnostic at
    /// all. So an alias reaches this lint today, and the residual risk is
    /// not only the clippy release that moves it into another group.
    const REACHES: [&str; 4] = [
        concat!("clippy::disallowed", "_method"),
        "clippy::style",
        "clippy::all",
        "warnings",
    ];

    /// An attribute OPENER anywhere in `squeezed`, a word that LOWERS a level,
    /// and a name that REACHES this lint. All three, because any two of them
    /// is a rule about something else: an opener and a word is every
    /// `#[allow(dead_code)]` in these files, and a word alone is every
    /// `.expect(` call in the test half.
    ///
    /// Anywhere rather than at the start, because an attribute may sit
    /// mid-line beside code — review wrote `let x = 1; #[cfg_attr(all(),
    /// allow(…))] File::open(p)`, which begins with neither `#` nor the
    /// attribute's own opener. The opener is `#[` or `#![` and not the whole
    /// `#[allow(`, so a suppression NESTED in a `cfg_attr` is caught by the
    /// same rule rather than by an entry naming it.
    fn suppresses_a_lint(squeezed: &str) -> bool {
        let attribute =
            squeezed.starts_with('#') || squeezed.contains("#[") || squeezed.contains("#![");
        attribute
            && LOWERS.iter().any(|word| squeezed.contains(word))
            && REACHES.iter().any(|name| squeezed.contains(name))
    }

    /// THE OTHER TWO PARTS OF THE MECHANISM, which live outside this file and
    /// would each stop it dead without a word.
    ///
    /// `Cargo.toml` is what turns the roster into an error; without the deny
    /// every entry in `clippy.toml` is advice. And the roster itself is the
    /// roster — its length is pinned because a deleted line refuses nothing
    /// and looks like nothing.
    #[test]
    fn the_roster_and_the_deny_are_both_still_there() {
        // The deny must be a real ENTRY and nothing may countermand it, which
        // are two separate ways the same line stops being in force. A
        // `manifest.contains` is satisfied by a COMMENT, so the key is looked
        // for among a table's entries; and review added `all = { level =
        // "allow", priority = 1 }` beside it, which leaves every assertion
        // here green and silences the deny for the whole crate, a higher
        // priority outranking a plain entry whatever it says. BOTH tables,
        // since `[lints.rust]` can allow the `warnings` group the same way
        // and reaches clippy's lints too. Every entry in either is a bare
        // deny or forbid today, so the check is that their shape has not
        // changed rather than a rule about which levels are permitted.
        let manifest = include_str!("../Cargo.toml");
        // A `#` starts a TOML comment wherever it sits, so it is cut from
        // every line rather than only skipped when it opens one: review wrote
        // `all = { level = "allow", priority = 1 } # = "deny"`, which ends
        // with the deny's own text and passed the shape check below while
        // silencing the lint crate-wide.
        //
        // And a HEADER is a line of its own, not the first text that spells
        // one. `split_once` took a header out of a COMMENT, so a decoy
        // section of denies read as the lint table while the real one below
        // it allowed the group — review's, and green.
        let table = |name: &str| -> Vec<&str> {
            let mut rows = Vec::new();
            let mut inside = false;
            for line in manifest.lines() {
                let bare = line.split('#').next().unwrap_or_default().trim();
                if bare.starts_with('[') {
                    inside = bare == name;
                } else if inside && !bare.is_empty() {
                    rows.push(bare);
                }
            }
            rows
        };
        let clippy = table("[lints.clippy]");
        let rust = table("[lints.rust]");
        assert!(!clippy.is_empty(), "Cargo.toml has no [lints.clippy] table");
        assert!(!rust.is_empty(), "Cargo.toml has no [lints.rust] table");
        assert!(
            clippy.contains(&concat!("disallowed", "_methods = \"deny\"")),
            "Cargo.toml no longer denies the lint, so the roster is advice"
        );
        for line in clippy.iter().chain(rust.iter()) {
            assert!(
                line.ends_with("= \"deny\"") || line.ends_with("= \"forbid\""),
                "Cargo.toml's lint tables gained an entry that is not a plain \
                 deny or forbid, which can outrank the deny above: {line}"
            );
        }
        let roster = include_str!("../clippy.toml");
        assert_eq!(
            roster.matches("{ path = ").count(),
            53,
            "the disallowed-path roster is not the length it was"
        );
        // The ones that are not `std::fs` at all are the easiest to lose to a
        // tidy-up, since they do not look like filesystem calls — and the two
        // `DirEntry` methods are easier still, because nothing in this crate
        // calls them yet and a roster entry for an absent call looks dead.
        for entry in [
            "std::env::set_current_dir",
            "std::path::absolute",
            "std::os::unix::net::UnixListener::bind",
            "std::os::unix::net::UnixDatagram::connect",
            "std::os::unix::net::SocketAddr::from_pathname",
            "std::os::unix::net::UnixDatagram::send_to_addr",
            "std::path::Path::try_exists",
            "std::fs::DirEntry::metadata",
            "std::fs::DirEntry::file_type",
        ] {
            assert!(roster.contains(entry), "the roster no longer holds {entry}");
        }
    }

    /// The one attribute that may open a hole in the roster.
    ///
    /// Spelled in two pieces so this file does not match itself:
    /// `include_str!("main.rs")` reads the whole of it, test half included.
    /// The split is at `#[all`/`ow(` rather than inside the lint name,
    /// because that is the half the scan below keys on.
    const ALLOW: &str = concat!("#[all", "ow(clippy::disallowed_methods)]");

    /// The twelve files this binary compiles, with the item each allow in
    /// them must sit on. Shared pure modules and test-only scratch have none;
    /// inventory uses paths; timezones uses the regular-file reader.
    type Compiled = (&'static str, &'static str, &'static [&'static str]);

    fn compiled_files() -> [Compiled; 18] {
        [
            ("main.rs", include_str!("main.rs"), MAIN_CHOKE.as_slice()),
            (
                "realfile.rs",
                include_str!("../../td-boot/src/realfile.rs"),
                REALFILE_CHOKE.as_slice(),
            ),
            (
                "gpt.rs",
                include_str!("../../engine/src/gpt.rs"),
                [].as_slice(),
            ),
            (
                "fat.rs",
                include_str!("../../engine/src/fat.rs"),
                [].as_slice(),
            ),
            (
                "cpio.rs",
                include_str!("../../engine/src/cpio.rs"),
                [].as_slice(),
            ),
            (
                "crc32.rs",
                include_str!("../../engine/src/crc32.rs"),
                [].as_slice(),
            ),
            (
                "sha256.rs",
                include_str!("../../engine/src/sha256.rs"),
                SHA256_CHOKE.as_slice(),
            ),
            (
                "protocol.rs",
                include_str!("../../td-boot/src/protocol.rs"),
                [].as_slice(),
            ),
            ("scratch.rs", include_str!("scratch.rs"), [].as_slice()),
            ("inventory.rs", include_str!("inventory.rs"), [].as_slice()),
            (
                "installation_plan.rs",
                include_str!("installation_plan.rs"),
                [].as_slice(),
            ),
            (
                "installation_protocol.rs",
                include_str!("installation_protocol.rs"),
                [].as_slice(),
            ),
            (
                "installation_service.rs",
                include_str!("installation_service.rs"),
                [].as_slice(),
            ),
            (
                "installation_consent.rs",
                include_str!("installation_consent.rs"),
                [].as_slice(),
            ),
            ("timezones.rs", include_str!("timezones.rs"), [].as_slice()),
            (
                "hostname.rs",
                include_str!("../../td-firstboot/src/hostname.rs"),
                [].as_slice(),
            ),
            ("loop_sys.rs", include_str!("loop_sys.rs"), [].as_slice()),
            (
                "loop_device.rs",
                include_str!("loop_device.rs"),
                [].as_slice(),
            ),
        ]
    }

    /// THE LIST ABOVE IS HAND-KEPT, and an additional file compiled into this
    /// binary would be read by neither guard — silently, since both count only
    /// what they were handed. Nothing but this relates it to the `#[path]`
    /// declarations it mirrors. The marker is split so this file does not
    /// match itself, as `ALLOW` is: the scan reads its own source.
    ///
    /// Over the UNCOMMENTED source, for the reason the allow scan is: a
    /// comment explaining a `#[path]` declaration is prose, and reading one as
    /// a declaration reds a file that compiles exactly fourteen.
    #[test]
    fn every_compiled_file_is_one_the_guards_read() {
        // WHITESPACE-INSENSITIVE from the marker on: `#[path="x.rs"]` with no
        // spaces is the same attribute to rustc and was invisible to a search
        // for the spelling this file happens to use.
        const MARKER: &str = concat!("#[pa", "th");
        // The marker scan is the one view that keeps STRINGS: it reads the
        // attribute's own path out of one. Everything below reads code
        // instead, and reads it off `plain_source`, which drops both.
        let text = uncommented(include_str!("main.rs"));
        let source = unspaced(&text);
        let source = source.as_str();
        let labels: Vec<&str> = compiled_files()
            .iter()
            .map(|(label, _, _)| *label)
            .collect();
        // The TABLE's own body, because `include_str!` below has to bind a
        // declaration to the file the guards read and a `contains` over the
        // whole source binds it to any TEXT: review spelled the missing
        // include inside a `stringify!`, which satisfied the search while
        // `compiled_files` went on reading the original.
        let table_body = {
            const HEAD: &str = "fn compiled_files() -> [Compiled; 18] {";
            let Some(at) = index_of(&text, HEAD) else {
                panic!("the compiled-file table is not where this scan looks for it")
            };
            let tail = text.get(at..).unwrap_or_default();
            let mut depth = 0usize;
            let mut end = tail.len();
            for (n, c) in tail.char_indices() {
                match c {
                    '{' => depth = depth.saturating_add(1),
                    '}' => {
                        depth = depth.saturating_sub(1);
                        if depth == 0 {
                            end = n;
                            break;
                        }
                    }
                    _ => {}
                }
            }
            unspaced(tail.get(..end).unwrap_or_default())
        };
        let mut declared = 0usize;
        let mut from = 0usize;
        while let Some(at) = source.get(from..).and_then(|rest| index_of(rest, MARKER)) {
            let start = from.saturating_add(at).saturating_add(MARKER.len());
            let rest = source.get(start..).unwrap_or_default();
            // Advanced BEFORE the shape test below, which may skip this
            // occurrence — a `continue` past the cursor is a loop that never
            // ends, and this one hung the suite until it was moved up.
            from = start;
            // The attribute's own shape, not merely its name: unspaced, a
            // declaration is `#[path="…"]` and this test's own prose about
            // `#[path]` is not. Written as a prefix test rather than by
            // trimming, or the prose matches and the path read is garbage —
            // which is how this was caught.
            let Some(quoted) = rest.strip_prefix("=\"") else {
                continue;
            };
            let path = quoted.split('"').next().unwrap_or_default();
            let name = path.rsplit('/').next().unwrap_or_default();
            assert!(
                labels.contains(&name),
                "{path} is compiled into this binary and scanned by neither guard"
            );
            // …and the guard reads THAT file rather than one with the same
            // basename. `compiled_files` names each by its `include_str!`
            // argument, so requiring the declared path to appear as one is
            // what ties the two together: review pointed out that moving a
            // declaration to `../../alternate/gpt.rs` still matches the
            // `gpt.rs` label while the guards go on reading the original.
            assert!(
                table_body.contains(&format!("include_str!(\"{path}\")")),
                "{path} is compiled in, but the guards read a different file \
                 of that name"
            );
            declared = declared.saturating_add(1);
        }
        // …and the list holds nothing ELSE, or a file could be dropped from
        // the crate while its entry went on standing in for it.
        assert_eq!(
            declared.saturating_add(1),
            labels.len(),
            "{declared} `#[path]` declarations against {} scanned files",
            labels.len()
        );
        // …and no file reaches this binary any OTHER way. A `#[path]` is not
        // how Rust normally names a second file: `mod escape;` compiles
        // `src/escape.rs` with no attribute to count, and `include!` splices
        // one into this file outright. Either is a ninth compiled source
        // carrying a crate-level `#![allow]` and any filesystem call it
        // likes, with both guards passing — review's, and the reason the loop
        // below counts DECLARATIONS rather than trusting the attribute.
        // EVERY compiled file, not this one alone. An eighth file arrives
        // through whichever compiled file declares it, and until review measured
        // it the three checks below ran over `main.rs` only: an
        // `include!("spliced.rs")` in `gpt.rs` — which resolves to the same
        // file for both crates, so it is the realistic shape rather than a
        // contrivance — spliced in a crate-level allow and a raw
        // `File::open`, clippy exit 0 and all four guards green. `pub mod
        // extra;` there did the same. The comment this replaces claimed "no
        // file reaches this binary any OTHER way"; it did.
        for (label, text, _) in compiled_files() {
            // ONE pass, and one that refuses what it cannot lex. A raw string
            // holding an odd quote desynchronises a composed strip for the
            // rest of the file, and review measured the silent pass that
            // follows: `mod escape;` below one is invisible, the module count
            // still agrees, and the module compiles with an allow of its own.
            // A BLOCK comment is the other refusal, and it is a twelfth
            // suppression rather than a lexing nicety —
            // `#[allow(clippy::/*x*/all)]` resolves for rustc, does not match
            // the roster, and was measured green over a raw `File::open`.
            let Some(plain_file) = plain_source(text) else {
                panic!(
                    "{label} holds a raw string or a block comment, either of \
                     which can spell a declaration or a lint path the guards \
                     below cannot read"
                )
            };
            let unspaced_file = unspaced(&plain_file);
            // Split like `MARKER` and `ALLOW`: written whole, the assertion is
            // an instance of what it refuses, since the scan reads its own
            // source. The bare macro NAME, not one delimiter of it: Rust takes
            // `(`, `[` and `{` alike, and a check naming the first walks past
            // the other two. `include_str!` does not match, its `!` sitting
            // elsewhere.
            assert!(
                !unspaced_file.contains(concat!("inclu", "de!")),
                "{label} splices a file into itself, where neither guard can \
                 tell it apart from that file's own source"
            );
            // A MACRO can assemble an attribute out of pieces no unit holds: a
            // `#[$attr]` in a definition and `allow(clippy::all)` at the call
            // site are two texts, and the scan reads text. Nothing here
            // defines one, and these eight files are compiled with no external
            // crate to import one from, so refusing the DEFINITION closes it.
            //
            // Over the UNSPACED, UNSTRINGED file rather than at a line start:
            // `macro_rules ! wrapped` compiles, defines the macro, and matched
            // neither the line-start test nor the keyword roster — as did the
            // same name anywhere but a line start. Dropping the strings is
            // what keeps the roster naming it above from matching itself, the
            // line start having been doing that job.
            assert!(
                !unspaced_file.contains("macro_rules!"),
                "{label} defines a macro, which can assemble a lint \
                 suppression the allow scan cannot read"
            );
            // …and no module of its own. `main.rs` declares the seven; any
            // other file declaring one compiles another source.
            let mods = declarations(&plain_file)
                .into_iter()
                .filter(|(keyword, head)| *keyword == "mod" && head.trim_end().ends_with(';'))
                .count();
            let want = if label == "main.rs" { declared } else { 0 };
            assert_eq!(
                mods, want,
                "{label} declares {mods} file modules against {want} `#[path]` \
                 declarations — a module without one compiles a file neither \
                 guard reads"
            );
        }
    }

    /// `sha256_file` is admitted as a choke point because the file is
    /// compiled, not because this crate opens through it: the kernel check
    /// hashes the descriptor it pinned, and a path-taking hash would reopen.
    #[test]
    fn the_engine_path_hash_is_never_called() {
        let needle = concat!("sha256", "_file");
        for (label, source, _) in compiled_files() {
            if label == "sha256.rs" {
                continue;
            }
            let text = uncommented(source);
            let shipped = text.split("#[cfg(test)]\nmod tests").next().unwrap();
            assert!(!shipped.contains(needle), "{label} calls {needle}");
        }
    }

    /// `main.rs`'s one choke point, as the line under its allow reads.
    const MAIN_CHOKE: [&str; 1] = ["mod paths {"];

    /// The engine hash's one, a path helper this crate compiles and never
    /// calls; it names its path like every other choke wrapper.
    const SHA256_CHOKE: [&str; 1] = ["pub fn sha256_file(p: &Path) -> std::io::Result<String> {"];

    /// `realfile.rs`'s two, which are functions rather than a module: td-boot
    /// compiles that file too and has no `clippy.toml`, so the allow there is
    /// inert for it and load-bearing here.
    const REALFILE_CHOKE: [&str; 2] = [
        "pub fn open_real_file(path: &Path, label: &str) -> io::Result<(File, Metadata)> {",
        "fn open_checked(path: &Path, expected: &Metadata, label: &str) -> io::Result<(File, Metadata)> {",
    ];

    /// EVERY WRAPPER IN A CHOKE POINT NAMES EVERY PATH IT TAKES.
    ///
    /// The half the compiler cannot check. Clippy says WHERE a call may be;
    /// whether the wrapper holding it puts the path in the message is a
    /// question about the wrapper's own text, and the wrappers are short
    /// enough for text to answer it.
    ///
    /// It earns its place: review wrote a `rename` that named only `from`,
    /// and the destination is the argument an operator is likelier to have
    /// got wrong.
    #[test]
    fn every_choke_wrapper_names_every_path_it_takes() {
        let mut checked = 0;
        for (label, source, markers) in compiled_files() {
            for marker in markers {
                let body = uncommented(region(label, source, marker));
                // Plainness FIRST: the two below read the region with its
                // strings dropped, and that strip is only sound once the
                // constructs it cannot lex have been refused.
                reads_as_plain_code(label, marker, &body);
                hands_out_no_capability(label, marker, &body);
                checked += names_every_path(label, marker, &body);
            }
        }
        // A naming test that found nothing to check would pass whatever the
        // wrappers did.
        assert_eq!(checked, 25, "{checked} wrappers were checked");
    }

    /// The text of the item opened at `marker`, up to the next line that is a
    /// lone `}`.
    ///
    /// A brace WALK would have to know which braces are inside a string, and
    /// that lexer is what this commit deleted. Every item here closes at
    /// column 0 — `mod paths`, both `realfile.rs` functions and `sha256_file`
    /// are top-level — which is a property of the file's layout that a reader
    /// can check and `rustfmt` keeps.
    fn region<'a>(label: &str, source: &'a str, marker: &str) -> &'a str {
        let Some(at) = index_of(source, marker) else {
            panic!("{label}: the choke point {marker} is not there any more");
        };
        let tail = source.get(at..).unwrap_or_default();
        let Some(end) = index_of(tail, "\n}") else {
            panic!("{label}: the choke point {marker} never closes");
        };
        tail.get(..end).unwrap_or_default()
    }

    /// `code` without its line comments, which discuss `.at(path)` and
    /// `File::open` in prose and would otherwise answer for the code.
    fn uncommented(code: &str) -> String {
        let mut out = String::with_capacity(code.len());
        for line in code.lines() {
            match comment_at(line) {
                Some(at) => out.push_str(line.get(..at).unwrap_or_default()),
                None => out.push_str(line),
            }
            out.push('\n');
        }
        out
    }

    /// Where `line`'s comment starts, if it has one — the first `//` OUTSIDE a
    /// string literal.
    ///
    /// Outside, because review hid an attribute behind one: in
    /// `const _: &str = "//"; #[allow(clippy::all)]` the slashes are data, and
    /// a strip that cut there deleted a suppression rustc sees. The toggle is
    /// per LINE, so a quote char literal earlier on the same line still fools
    /// it; that is two deliberate coincidences deep and is a known limit
    /// rather than a closed one.
    fn comment_at(line: &str) -> Option<usize> {
        let mut in_str = false;
        let mut esc = false;
        let mut slash = false;
        for (at, ch) in line.char_indices() {
            if in_str {
                match ch {
                    _ if esc => esc = false,
                    '\\' => esc = true,
                    '"' => in_str = false,
                    _ => {}
                }
                slash = false;
                continue;
            }
            if ch == '"' {
                in_str = true;
                slash = false;
                continue;
            }
            if ch == '/' && slash {
                return at.checked_sub(1);
            }
            slash = ch == '/';
        }
        None
    }

    /// A CHOKE POINT MAY NOT HAND ITS CAPABILITY OUT.
    ///
    /// The allow makes one region able to open any path. Nothing stopped that
    /// region EXPORTING the ability, and review wrote both spellings green:
    ///
    /// ```ignore
    /// pub fn opener() -> impl Fn(&Path) -> io::Result<File> { |p| File::open(p) }
    /// pub const OPENER: fn(&Path) -> io::Result<File> = |p| File::open(p);
    /// ```
    ///
    /// Either lets any call site in the crate open any path with the bare
    /// `io::Error` this whole mechanism exists to replace, and neither
    /// declares a `Path` PARAMETER — so the naming loop below skips both and
    /// the wrapper count stays right. A closure can only be NAMED with `Fn`,
    /// and a function pointer is the only other way to carry one, so refusing
    /// those spellings refuses the shape rather than two examples of it.
    ///
    /// Over the region with its WHITESPACE REMOVED, and the visibility read
    /// the same way. `fn (&Path)` is the same type as `fn(&Path)`, and
    /// `pub(crate)` is `pub ` with no space after it — review wrote both and
    /// walked past a check reading raw text. Every export here is therefore a
    /// plain `pub fn`, which also refuses `pub(crate)`, `pub unsafe fn` and
    /// `pub static`: these short wrappers need none of them, and a shape
    /// that arrives is a decision rather than an accident.
    fn hands_out_no_capability(label: &str, region: &str, body: &str) {
        // With STRINGS dropped, so a message naming a shape is data rather
        // than an export — and read as DECLARATIONS rather than as lines. A
        // line is not the unit: review put a newline after `impl`, a tab
        // after it, and an attribute before it on the same line, and all
        // three walked past a rule anchored at a line start. `rustfmt` writes
        // the same shape by itself once a signature outgrows the width.
        let plain = unstringed(body);
        let squeezed = unspaced(&plain);
        for shape in ["Fn(", "Fn<", "FnMut(", "FnOnce(", "fn("] {
            assert!(
                !squeezed.contains(shape),
                "{label}: {region} names `{shape}`, so it can hand the \
                 capability out"
            );
        }
        // A head ends at the `{` or `;` that CLOSES it, not at the first one
        // in it. `[u8; 4]` supplies a `;` inside brackets, and cutting there
        // handed the return-type read half a signature — a false red naming
        // the wrong problem, which review measured on a shape this crate has
        // every reason to write.
        // …and a parenthesised parameter TYPE does not end the list, for the
        // same reason and with a worse consequence: every parameter after it
        // went unread, so a wrapper could take a path it was never asked to
        // name.
        assert_eq!(
            path_parameters("fn f(pair: (u32, u32), path: &Path) -> io::Result<()> {"),
            vec!["path".to_string()],
            "a parenthesised parameter type cuts the argument list short"
        );
        let arrayed = "pub fn read_first(path: &Path, buf: &mut [u8; 4]) -> io::Result<()> {";
        assert!(
            declarations(arrayed)
                .iter()
                .any(|(keyword, head)| *keyword == "pub" && *head == arrayed),
            "an array type in a parameter cuts a signature short"
        );
        for (keyword, head) in declarations(&plain) {
            let head = unspaced(head);
            // An ITEM a caller outside can reach carries a capability whether
            // or not it spells `Fn`, which is what the shapes above missed:
            // review wrote `impl crate::OpenAnything for crate::AnyOpener`
            // into the region, with the trait and the type declared outside
            // it, and every rule here walked past — a trait-impl method has
            // no visibility of its own, and `&self` plus a `&str` is no
            // `Path` parameter to check. So the item kinds that can carry one
            // are pinned WHOLE, and a region declares exactly what it
            // declares today.
            if keyword != "pub" {
                assert!(
                    PINNED_ITEMS.iter().any(|item| unspaced(item) == head),
                    "{label}: {region} declares an item this test does not \
                     know, which may carry a capability with no `Fn` in it: \
                     {head}"
                );
                continue;
            }
            assert!(
                head.starts_with("pubfn"),
                "{label}: {region} exports something that is not a plain \
                 `pub fn`: {head}"
            );
            // …and every one takes a PATH. `File::open` accepts an
            // `impl AsRef<Path>`, so review's `pub fn open_named(name: &str)`
            // is the whole capability with the bare error, declares no path
            // to name, and is a plain `pub fn` — three checks walked past it.
            // A wrapper with no `Path` parameter could not name one anyway,
            // which is why refusing it costs nothing.
            assert!(
                head.contains("&Path"),
                "{label}: {region} exports a wrapper that takes no `&Path`, \
                 so it has no path to name: {head}"
            );
            // …and hands back one of two shapes. An allowlist rather than
            // more refused spellings, because a capability can be named by
            // an ALIAS declared outside the region — `type Cap = fn(&Path)
            // -> io::Result<File>` and a wrapper returning `Cap` spells no
            // shape above — and no text in the region can tell what a name
            // means. Read after the LAST `->`, which is the return arrow: a
            // parameter cannot carry one, since the types that do are the
            // `Fn`/`fn(` shapes refused above. Over the whole head it was a
            // check a PARAMETER could satisfy — review wrote `_seed:
            // io::Result<()>` beside a return type naming an alias. Matched
            // as a SUFFIX of the type rather than straight after the arrow,
            // or `-> std::io::Result<File>` reds. What this cannot close is
            // the same capability smuggled INSIDE an allowed return type;
            // that is the `AsRef<Path>` limit again, and the compiler is the
            // thing that could tell.
            let returns = index_of_last(&head, "->")
                .and_then(|at| head.get(at..))
                .unwrap_or_default();
            assert!(
                returns.contains("io::Result<") || returns.contains("Option<"),
                "{label}: {region} exports a wrapper returning something \
                 other than `io::Result` or `Option`: {head}"
            );
            // A `ReadDir` handed back is the one wrapper shape DESIGN.md's
            // D10 forbids in prose: reading a directory fails once per
            // ENTRY, and the roster cannot hold `ReadDir::next` because that
            // path does not resolve. So the rule lands here — a wrapper must
            // consume the iterator and name the directory itself.
            assert!(
                !returns.contains("ReadDir"),
                "{label}: {region} hands back a `ReadDir`, whose per-entry \
                 errors name nothing: {head}"
            );
        }
    }

    /// The DECLARATIONS `plain` opens: each keyword at an identifier
    /// boundary, paired with its head — the text through to the `{` or `;`
    /// that ends it.
    ///
    /// A boundary on BOTH sides, since `mod` is a prefix of `mode`, `impl` of
    /// `implementation` and `pub` of `public`; and a head rather than a line,
    /// since a declaration may be written across as many as it likes.
    fn declarations(plain: &str) -> Vec<(&'static str, &str)> {
        const KEYWORDS: [&str; 5] = ["impl", "trait", "mod", "macro_rules!", "pub"];
        let ident = |c: char| c.is_alphanumeric() || c == '_';
        let mut out = Vec::new();
        for keyword in KEYWORDS {
            let mut from = 0usize;
            while let Some(at) = plain.get(from..).and_then(|rest| index_of(rest, keyword)) {
                let start = from.saturating_add(at);
                from = start.saturating_add(keyword.len());
                let before = plain.get(..start).and_then(|t| t.chars().next_back());
                let after = plain.get(from..).and_then(|t| t.chars().next());
                if before.is_some_and(ident) || after.is_some_and(ident) {
                    continue;
                }
                let tail = plain.get(start..).unwrap_or_default();
                // Walked rather than searched: this file is staged into a
                // recipe, and the recipe scan refuses the bare token an
                // iterator search would spell — which is why `index_of` and
                // `index_of_last` exist here at all.
                // At BRACKET DEPTH ZERO, or an array type ends the head: the
                // `;` in `buf: &mut [u8; 4]` cut a signature in half and the
                // truncated text then failed the return-type read, reporting
                // a wrapper as exporting the wrong thing. A false red with
                // the wrong diagnosis, which review measured.
                let mut end = tail.len();
                let mut depth = 0usize;
                for (at, c) in tail.char_indices() {
                    match c {
                        '[' | '(' => depth = depth.saturating_add(1),
                        ']' | ')' => depth = depth.saturating_sub(1),
                        '{' | ';' if depth == 0 => {
                            end = at.saturating_add(c.len_utf8());
                            break;
                        }
                        _ => {}
                    }
                }
                out.push((keyword, tail.get(..end).unwrap_or_default()));
            }
        }
        out
    }

    /// The three declarations `main.rs`'s choke point opens, pinned whole. A
    /// fourth is a decision someone makes on purpose.
    ///
    /// A `use` is not among the keywords that reach this, because `pub use`
    /// is already refused as a non-`pub fn` export and a private one
    /// re-exports nothing.
    const PINNED_ITEMS: [&str; 3] = [
        "mod paths {",
        "trait NamePath<T> {",
        "impl<T> NamePath<T> for io::Result<T> {",
    ];

    /// The char literals a choke point may not hold, written as escapes so
    /// this file is not itself an instance of what it refuses.
    ///
    /// Each would desynchronise a COUNTER this test runs on the region: the
    /// two quotes are a delimiter to `unstringed`'s toggle, and the two braces
    /// are an item boundary to the brace count below. A sibling scan in
    /// `builder` NEUTRALISES the same literals instead, because it reads whole
    /// files and cannot refuse one; these short wrappers can afford the
    /// refusal, and refusing is the smaller thing to be right about.
    const REFUSED_CHARS: [&str; 4] = ["'\u{22}'", "'\\\u{22}'", "'{'", "'}'"];

    /// `unstringed`'S TOGGLE IS ONLY RIGHT IF THE REGION IS PLAIN, AND THE
    /// REGION IS ONLY THE ITEM IF NOTHING CUT IT SHORT.
    ///
    /// Four constructs break one or the other, and each is refused rather than
    /// lexed. A RAW string holding a quote ends the literal early, so its
    /// content is read as code — at ANY hash count, which review found the
    /// two-spelling check missing: `r##"\"##;` desynchronises the toggle for
    /// the rest of the region. A `'"'` char literal opens a string that
    /// swallows the rest of the function. A BLOCK comment can hold a line that
    /// begins `}`, which is where `region` stops. And a string SPANNING LINES
    /// can hold one too — that is the truncation, and it is why every line
    /// must close the strings it opens.
    ///
    /// The brace count is what proves the region is the whole item, and on its
    /// own it does NOT: review appended
    ///
    /// ```ignore
    /// const NOTE: &str = "usage:
    /// }";
    /// pub const OPENER: fn(&Path) -> io::Result<File> = |p| File::open(p);
    /// ```
    ///
    /// at MODULE level, where every wrapper before it is balanced, so the
    /// module's own brace is still the only one left open and the count read
    /// 1 — with the handout sitting after the cut, scanned by nothing. It
    /// takes all four refusals for the count to mean anything: with no
    /// line-spanning string, no block comment and no brace char literal, a
    /// `}` at column 0 is code closing the one brace the count says is open,
    /// which is the item's own.
    fn reads_as_plain_code(label: &str, region: &str, body: &str) {
        // Over the STRING-STRIPPED body, or every message ending in the
        // letter `r` is a raw string to this: review found `concat!("cannot
        // open dir")` refused, and `"for"`, `"error"`, `"other"` and `"/usr"`
        // with it. The strip is sound for the question — a real raw string
        // still shows its `r` and its opening quote, since the strip keeps
        // both delimiters and only drops what is between them.
        assert!(
            !opens_a_raw_string(&unstringed(body)),
            "{label}: {region} holds a raw string, which the string strip \
             cannot lex"
        );
        assert!(
            !body.contains("/*"),
            "{label}: {region} holds a block comment, which can hold the line \
             that ends the item"
        );
        for spelling in REFUSED_CHARS {
            assert!(
                !body.contains(spelling),
                "{label}: {region} holds the char literal {spelling}, which \
                 the string strip or the brace count reads as punctuation"
            );
        }
        // A string that does not close on its own line is what cuts a region
        // short, and it is visible from the CUT side: the truncated body ends
        // inside the literal. `unstringed` keeps both delimiters and drops the
        // content, so an odd count of them is an unclosed one.
        for (n, line) in body.lines().enumerate() {
            assert_eq!(
                unstringed(line).matches('"').count() % 2,
                0,
                "{label}: {region} line {} holds a string literal that does \
                 not close on its own line: {line}",
                n.saturating_add(1)
            );
        }
        // …and the region is the WHOLE item. `region` cuts at the first line
        // that is a lone `}`; counting braces outside strings is what notices
        // a cut anywhere inside a wrapper, and the refusals above are what
        // make the count sound at module level too.
        let plain = unstringed(body);
        let opened = plain.matches('{').count();
        let closed = plain.matches('}').count();
        assert_eq!(
            opened.saturating_sub(closed),
            1,
            "{label}: {region} was cut short — {opened} `{{` and {closed} `}}` \
             where the item's own opening brace should be the only one left"
        );
    }

    /// Whether `body` opens a RAW string, at any hash count.
    ///
    /// `r"` and `r#"` were checked by name, and `r##"` — which
    /// `builder`'s own gate bodies use — walked past both.
    /// `text` with its comments gone and the CONTENT of its strings and char
    /// literals blanked, in ONE pass — or `None` where it meets a construct it
    /// does not lex.
    ///
    /// Two passes cannot do this soundly and this file had two: stripping
    /// comments first reads a `//` INSIDE a string as a comment, and stripping
    /// strings first reads a `"` inside a COMMENT as a string. Both are live
    /// here — main.rs continues a string across lines — so the composed view
    /// really was desynchronised, and review measured what that buys: a `mod
    /// escape;` below the desynchronisation is invisible to the module count,
    /// which still agrees, while the module compiles with a lint suppression
    /// of its own.
    ///
    /// The region scans still REFUSE rather than lex, and that stays the rule
    /// for a region: these short wrappers can afford it. A whole FILE
    /// cannot — main.rs holds sixteen char literals a counter would
    /// desynchronise on, and refusing them would refuse this file.
    ///
    /// What it does not lex is a raw string and a block comment, and neither
    /// is a caveat: it refuses both, none of the eight compiled files holds
    /// either (measured), and the refusal is what the raw-string check this
    /// replaces could not state soundly.
    fn plain_source(text: &str) -> Option<String> {
        let chars: Vec<char> = text.chars().collect();
        let mut out = String::with_capacity(text.len());
        let mut i = 0usize;
        while let Some(&c) = chars.get(i) {
            let next = chars.get(i.saturating_add(1)).copied();
            if c == '/' && next == Some('/') {
                while chars.get(i).is_some_and(|ch| *ch != '\n') {
                    i = i.saturating_add(1);
                }
                continue;
            }
            if c == '/' && next == Some('*') {
                return None;
            }
            // A raw string, at any hash count and under any prefix. The `r`
            // must open one rather than end an identifier, or `for r"` — an
            // ordinary string after a variable named `r` — refuses the file.
            if c == 'r' || (matches!(c, 'b' | 'c') && next == Some('r')) {
                let at = if c == 'r' { i } else { i.saturating_add(1) };
                let ident = |ch: char| ch.is_alphanumeric() || ch == '_';
                let opens = !at
                    .checked_sub(1)
                    .and_then(|p| chars.get(p))
                    .is_some_and(|ch| ident(*ch));
                let mut j = at.saturating_add(1);
                while chars.get(j) == Some(&'#') {
                    j = j.saturating_add(1);
                }
                if opens && chars.get(j) == Some(&'"') {
                    return None;
                }
            }
            if c == '\'' {
                if let Some(len) = char_literal_len(&chars, i) {
                    out.push_str("'x'");
                    i = i.saturating_add(len);
                    continue;
                }
            }
            if c == '"' {
                out.push('"');
                i = i.saturating_add(1);
                let mut esc = false;
                while let Some(&ch) = chars.get(i) {
                    i = i.saturating_add(1);
                    if esc {
                        esc = false;
                    } else if ch == '\\' {
                        esc = true;
                    } else if ch == '"' {
                        out.push('"');
                        break;
                    }
                    // Newlines survive so what is left has the line structure
                    // the file had, a continued string included.
                    if ch == '\n' {
                        out.push('\n');
                    }
                }
                continue;
            }
            out.push(c);
            i = i.saturating_add(1);
        }
        Some(out)
    }

    /// The length in CHARS of the char literal at `at`, or `None` where that
    /// `'` opens a LIFETIME or a label.
    ///
    /// Decided by LOOKAHEAD, as rustc decides it: a literal is one character
    /// or one escape between two quotes, and a lifetime has no closing quote.
    /// `builder`'s `lex` carries the same function for the same reason —
    /// neither crate may depend on the other, both being their own workspace.
    fn char_literal_len(chars: &[char], at: usize) -> Option<usize> {
        let get = |n: usize| chars.get(at.saturating_add(n)).copied();
        let hex = |n: usize| get(n).is_some_and(|c| c.is_ascii_hexdigit());
        if get(0) != Some('\'') {
            return None;
        }
        let close = match get(1)? {
            '\\' => match get(2)? {
                'x' if hex(3) && hex(4) => 5,
                'x' => return None,
                'u' => {
                    if get(3) != Some('{') {
                        return None;
                    }
                    let digits = (4..10).take_while(|n| hex(*n)).count();
                    let end = 4usize.saturating_add(digits);
                    if get(end) != Some('}') {
                        return None;
                    }
                    end.saturating_add(1)
                }
                _ => 3,
            },
            '\'' | '\n' | '\r' | '\t' => return None,
            _ => 2,
        };
        (get(close) == Some('\'')).then(|| close.saturating_add(1))
    }

    fn opens_a_raw_string(body: &str) -> bool {
        let chars: Vec<char> = body.chars().collect();
        chars.iter().enumerate().any(|(at, c)| {
            if *c != 'r' {
                return false;
            }
            let mut j = at.saturating_add(1);
            while chars.get(j) == Some(&'#') {
                j = j.saturating_add(1);
            }
            chars.get(j) == Some(&'"')
        })
    }

    /// How many wrappers were checked, so the caller can refuse a run that
    /// checked none.
    ///
    /// A marker's PRESENCE is what this reads, not that the error carries it.
    /// `let _ = path.display(); File::open(path)` names the path and returns
    /// the bare error, and review wrote it green. Closing that needs the
    /// naming tied to the value returned, which is a question about
    /// expressions rather than about text; the limit is recorded rather than
    /// papered over.
    fn names_every_path(label: &str, region: &str, body: &str) -> usize {
        let pieces = functions(body);
        // EVERY `fn` in the region is one of them. Without this the
        // enumeration counts only what it could parse, so a spelling it stops
        // at is a wrapper that vanishes rather than one that reds — the same
        // "count only what they were handed" failure the compiled-file list
        // has, one layer down, and the shape three separate rounds of review
        // walked out through (`fn  x`, a name on the next line, `x (`,
        // `x<T>(`). Pinned against the KEYWORDS rather than against a number,
        // so it holds however many wrappers there come to be.
        let keywords = {
            let plain = unstringed(body);
            let mut count = 0usize;
            let mut from = 0usize;
            while let Some(hit) = plain.get(from..).and_then(|rest| index_of(rest, "fn")) {
                let at = from.saturating_add(hit);
                from = at.saturating_add("fn".len());
                let before = plain.get(..at).and_then(|t| t.chars().next_back());
                let after = plain.get(from..).and_then(|t| t.chars().next());
                let ident = |c: char| c.is_alphanumeric() || c == '_';
                if !before.is_some_and(ident) && !after.is_some_and(ident) {
                    count = count.saturating_add(1);
                }
            }
            count
        };
        assert_eq!(
            pieces.len(),
            keywords,
            "{label}: {region} declares {keywords} functions and this read \
             {} — a spelling this enumeration stops at is a wrapper nothing \
             below ever asks to name its path",
            pieces.len()
        );
        let mut checked = 0;
        for piece in pieces {
            // A trait's method SIGNATURE has no body to name anything in.
            // Its piece runs on to the next declaration, so holding a `{` does
            // not tell the two apart — which comes FIRST does.
            let opens = index_of(piece, "{");
            let ends = index_of(piece, ";");
            if opens.is_none() || (ends.is_some() && ends < opens) {
                continue;
            }
            // A path parameter is spelled as a `Path` here, so the signature
            // says which arguments are ones. Taken GENERICALLY it does not:
            // review wrote `open_any(path: impl AsRef<Path>)` naming
            // `/not-the-file` for every failure, and the loop below found no
            // parameter to require a naming for.
            assert!(
                !piece.contains("AsRef<") && !piece.contains("Into<"),
                "{label}: {region} takes a path generically, where this cannot \
                 read it off the signature: {piece}"
            );
            let names = path_parameters(piece);
            if names.is_empty() {
                continue;
            }
            checked += 1;
            let body = unspaced(&unstringed(piece));
            // The one wrapper whose error never becomes a message is exempt
            // BY NAME, because a shape was tried twice and broken twice: a
            // bare `.ok()` anywhere excused every path in the function, and
            // narrowing that to `(path).ok()` still let `let _ =
            // std::fs::metadata(path).ok();` sit beside an unnamed
            // `File::open(path)`. The `(` is part of the name, or
            // `metadata_if_present_and_open` inherits the exemption.
            //
            // And the RETURN TYPE, which is what actually makes it safe:
            // nothing that hands back an `Option` can propagate an error at
            // all, so there is none to leave unnamed. A wrapper of this name
            // that started returning `io::Result` would keep the exemption
            // without it and could then propagate one raw.
            let discarded = piece.trim_start().starts_with(DISCARDS_ITS_ERROR)
                && piece.contains("-> Option<")
                && names
                    .iter()
                    .any(|name| body.contains(&format!("({name}).ok()")));
            for name in names {
                // The three ways a name reaches a message, spelled out rather
                // than counted. Counting is what review broke: `p` occurring
                // twice in `File::open(p).at(Path::new("/wrong"))` is once as
                // the argument and once inside the word `open`, so a wrapper
                // reporting an unrelated path passed. A wrapper that names its
                // path some fourth way reds here and adds its spelling.
                let spellings = [
                    format!(".at({name})"),
                    format!("named(error,{name})"),
                    format!("{name}.display()"),
                ];
                assert!(
                    spellings.iter().any(|spelling| body.contains(spelling)) || discarded,
                    "{label}: {region} takes `{name}` and does not name it: {piece}"
                );
            }
        }
        checked
    }

    /// The functions `body` declares, cut at each `fn NAME(` rather than at
    /// each `fn `, so a string or a name ending in `fn` opens nothing.
    ///
    /// A RAW identifier is a name too. `pub fn r#open_raw(path: &Path)` is a
    /// wrapper the enumeration stopped reading at the `#`, so it declared no
    /// path, was never counted, and could return the bare error while the
    /// pinned count stayed right — review's, and the same class as every
    /// spelling that drove the roster to clippy.
    fn functions(body: &str) -> Vec<&str> {
        let mut heads = Vec::new();
        let mut from = 0;
        while let Some(hit) = body.get(from..).and_then(|rest| index_of(rest, "fn")) {
            let at = from.saturating_add(hit);
            let after = at.saturating_add("fn".len());
            from = after;
            // `fn` as a WORD, and whatever whitespace follows it. The literal
            // `fn ` was the keyword plus exactly one space, so `pub fn  name`
            // and a `fn` with its name on the next line — both valid, and the
            // second is what `rustfmt` writes for a long one — declared a
            // wrapper this enumeration never saw: no name, no path, never
            // counted, and the pinned count still right. Review's.
            let before = body.get(..at).and_then(|t| t.chars().next_back());
            if before.is_some_and(|c| c.is_alphanumeric() || c == '_') {
                continue;
            }
            let spaced = body.get(after..).unwrap_or_default();
            if !spaced.starts_with(char::is_whitespace) {
                continue;
            }
            let tail = spaced.trim_start();
            // The piece begins at the NAME, as it did when the keyword and
            // its one space were matched together.
            let name_at = after.saturating_add(spaced.len().saturating_sub(tail.len()));
            let tail = tail.strip_prefix("r#").unwrap_or(tail);
            let name: String = tail
                .chars()
                .take_while(|ch| ch.is_alphanumeric() || *ch == '_')
                .collect();
            // WHITESPACE and a GENERIC LIST both sit between a name and its
            // arguments, and requiring the `(` to touch the name stopped this
            // enumeration at either: `open_spaced (path: &Path)` and
            // `open_generic<T>(path: &Path, _t: T)` are wrappers it never
            // saw, so neither declared a path, neither was counted, and both
            // could hand back the bare error with the pinned count still
            // right. Review's, and the mirror of the two spellings the round
            // before — which is why the count below is now pinned to the
            // `fn`s in the region rather than to what this returns.
            let rest = tail.get(name.len()..).unwrap_or_default().trim_start();
            let rest = match rest.strip_prefix('<') {
                Some(generics) => {
                    let mut depth = 1usize;
                    let mut end = generics.len();
                    let mut prev = '<';
                    for (i, c) in generics.char_indices() {
                        match c {
                            '<' => depth = depth.saturating_add(1),
                            // Not the `>` of a `->`, which closes nothing.
                            '>' if prev != '-' => {
                                depth = depth.saturating_sub(1);
                                if depth == 0 {
                                    end = i.saturating_add(1);
                                    break;
                                }
                            }
                            _ => {}
                        }
                        prev = c;
                    }
                    generics.get(end..).unwrap_or_default().trim_start()
                }
                None => rest,
            };
            if !name.is_empty() && rest.starts_with('(') {
                heads.push(name_at);
            }
        }
        let mut out = Vec::new();
        for (index, start) in heads.iter().enumerate() {
            let end = heads.get(index + 1).copied().unwrap_or(body.len());
            out.push(body.get(*start..end).unwrap_or_default());
        }
        out
    }

    /// `code` with every space taken out, so the spellings above match a
    /// wrapper however it is wrapped across lines.
    fn unspaced(code: &str) -> String {
        code.chars().filter(|ch| !ch.is_whitespace()).collect()
    }

    /// `code` with the CONTENTS of its string literals removed.
    ///
    /// A marker inside a MESSAGE answers for the code otherwise. Review wrote
    /// `format!("open failed, see .at(path) below: {error}")` into a wrapper
    /// that named nothing and watched it pass: `uncommented` takes comments
    /// and nothing took strings, so the prose satisfied a check the code did
    /// not. Every naming this looks for is CODE — `.at(path)`,
    /// `named(error, path)`, `path.display()` — so dropping string content can
    /// only lose a fake one.
    ///
    /// A plain quote TOGGLE, which is sound only because
    /// `reads_as_plain_code` has refused the two constructs that break one.
    fn unstringed(code: &str) -> String {
        let mut out = String::with_capacity(code.len());
        let mut in_str = false;
        let mut esc = false;
        for ch in code.chars() {
            if in_str {
                match ch {
                    _ if esc => esc = false,
                    '\\' => esc = true,
                    '"' => {
                        in_str = false;
                        out.push(ch);
                    }
                    _ => {}
                }
                continue;
            }
            if ch == '"' {
                in_str = true;
            }
            out.push(ch);
        }
        out
    }

    /// The path parameters `piece` declares, read off its signature.
    ///
    /// By the TYPE naming `Path`, which is what a wrapper here writes and not
    /// what every path-taking parameter must be: `File::open` accepts an
    /// `impl AsRef<Path>`, so a `&str` — or an `&OsStr`, the same class and
    /// the same blind spot — is a path too and this cannot tell. The generic
    /// form is refused above; the concrete one is a limit, and a small one now
    /// that it applies to these short functions the compiler has already
    /// fenced rather than to a whole crate.
    fn path_parameters(piece: &str) -> Vec<String> {
        let Some((_, after)) = piece.split_once('(') else {
            return Vec::new();
        };
        // To the paren that CLOSES the list, not the first one inside it. A
        // parenthesised parameter type — a tuple, or a function type — closes
        // one of its own, and cutting there dropped every parameter after it,
        // the `&Path` a wrapper must name included. Review measured a wrapper
        // that took a path and was never asked to name it.
        let mut depth = 0usize;
        let mut end = after.len();
        for (at, c) in after.char_indices() {
            match c {
                '(' => depth = depth.saturating_add(1),
                ')' if depth == 0 => {
                    end = at;
                    break;
                }
                ')' => depth = depth.saturating_sub(1),
                _ => {}
            }
        }
        let arguments = after.get(..end).unwrap_or_default();
        arguments
            .split(',')
            .filter(|argument| argument.contains("Path"))
            .filter_map(|argument| argument.split(':').next())
            .map(|name| name.trim().trim_start_matches("mut ").trim().to_string())
            .filter(|name| !name.is_empty())
            .collect()
    }

    /// The ONE wrapper whose error never becomes a message, named rather than
    /// recognised.
    const DISCARDS_ITS_ERROR: &str = "metadata_if_present(";

    /// The first index of `needle`, spelled without the method that names it.
    ///
    /// This file is `include_str!`'d into its recipe, and the catalog's
    /// command-surface scan reds on the bare token that method would leave in
    /// the text — the same reason a comment here cannot spell it either.
    fn index_of(haystack: &str, needle: &str) -> Option<usize> {
        haystack
            .match_indices(needle)
            .next()
            .map(|(index, _)| index)
    }

    /// The LAST index of `needle`, spelled without its method for the same
    /// reason — the reverse one leaves a token the catalog scan reds on too.
    ///
    /// Walked rather than reversed: `MatchIndices` is only double-ended for a
    /// pattern whose searcher can run backwards, which a `&str`'s cannot.
    fn index_of_last(haystack: &str, needle: &str) -> Option<usize> {
        let mut last = None;
        for (index, _) in haystack.match_indices(needle) {
            last = Some(index);
        }
        last
    }

    /// A KEY THAT IS NOT THERE COSTS NOTHING.
    ///
    /// The refusal has to come before the staging tree is emptied, or a
    /// mistyped fifth argument destroys a directory before saying it did not
    /// like the fifth argument. That is the same failure `run_volume`'s own
    /// comment records for the fourth, which is why the read sits beside those
    /// checks rather than in `seed_into` where it started: there it ran
    /// after `remove_dir_all`, and this test reds.
    #[test]
    fn a_key_that_is_not_there_does_not_cost_the_staging_tree() {
        let scratch = Scratch::disk(DISK);
        run_layout(&scratch.path, &mut Vec::new()).unwrap();
        let dir = fake_mkfs(RECORDING_MKFS);
        let td_boot = dir.join("td-boot");
        publishing_td_boot(&td_boot, "");
        // Something in the staging tree that a premature refusal would take.
        let keep = dir.join("td-volume-root").join("keepme");
        std::fs::create_dir_all(&keep).unwrap();
        std::fs::write(keep.join("data"), b"not yours to remove").unwrap();
        let publish = Publish {
            td_boot,
            deployment: PathBuf::from("/media/deployment"),
            trusted_key: dir.join("no-such-key.pub"),
        };
        let error = run_volume(
            VolumeSettings::default(),
            None,
            &scratch.path,
            &dir.join("mkfs.btrfs"),
            &dir,
            Some(&VolumeSeed::Publish(publish)),
            &mut Vec::new(),
        )
        .unwrap_err();
        assert!(
            format!("{error}").contains("trusted deployment key"),
            "the refusal must name the key: {error}"
        );
        assert_eq!(
            std::fs::read(keep.join("data")).unwrap(),
            b"not yours to remove",
            "a missing key emptied the caller's staging tree"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The trust root's path is RELATIVE and outside the deployments
    /// directory.
    ///
    /// Both are properties of the constant rather than of any run, and both
    /// are pinned here because `staging.join(…)` is where they bite: an
    /// absolute constant would discard the staging tree silently and put a
    /// machine's trust root at `/td/trusted.pub` on the INSTALLER, and one
    /// under `td/deployments` would be replaced by the first update. The
    /// literal is pinned too, as `TRUSTED_KEY_PATH` is, since a rename is a
    /// key the reader never finds.
    #[test]
    fn the_trust_roots_path_is_relative_and_outside_the_deployments() {
        assert_eq!(protocol::VOLUME_TRUSTED_KEY, "td/trusted.pub");
        let path = Path::new(protocol::VOLUME_TRUSTED_KEY);
        assert!(
            path.is_relative(),
            "an absolute key path discards the volume"
        );
        assert!(!path.starts_with(protocol::DEPLOYMENTS_DIR));
        assert_eq!(
            Path::new("/vol").join(protocol::VOLUME_TRUSTED_KEY),
            Path::new("/vol/td/trusted.pub")
        );
    }

    /// Nothing pre-placed where the snapshot goes is ever WRITTEN THROUGH.
    ///
    /// The scratch directory is not private and both names here are guessable,
    /// so each is tried as a symlink pointing at a file that must survive:
    /// following either would truncate it under an installer that is usually
    /// root. Whether the install then succeeds or refuses is not the property
    /// — the victim is — so both outcomes are accepted and only the bytes are
    /// asserted.
    #[test]
    fn nothing_pre_placed_at_the_snapshot_is_written_through() {
        for name in ["td-install-key", "td-install-key/td-trusted.pub"] {
            let scratch = Scratch::disk(DISK);
            run_layout(&scratch.path, &mut Vec::new()).unwrap();
            let dir = fake_mkfs(RECORDING_MKFS);
            let td_boot = dir.join("td-boot");
            publishing_td_boot(&td_boot, "");
            let victim = dir.join("victim");
            std::fs::write(&victim, b"do not truncate me").unwrap();
            let planted = dir.join(name);
            if let Some(parent) = planted.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::os::unix::fs::symlink(&victim, &planted).unwrap();
            let publish = Publish {
                td_boot,
                deployment: PathBuf::from("/media/deployment"),
                trusted_key: key_file(&dir),
            };
            let _ = run_volume(
                VolumeSettings::default(),
                None,
                &scratch.path,
                &dir.join("mkfs.btrfs"),
                &dir,
                Some(&VolumeSeed::Publish(publish)),
                &mut Vec::new(),
            );
            assert_eq!(
                std::fs::read(&victim).unwrap(),
                b"do not truncate me",
                "a symlink at {name} was followed and its target truncated"
            );
            std::fs::remove_dir_all(&dir).unwrap();
        }
    }

    /// Bare formatting remains unprovisioned unless a seed is explicit.
    /// This guards a future unconditional seed; it cannot red a mutation
    /// inside seed_into because bare formatting does not call that function.
    #[test]
    fn a_volume_without_an_explicit_seed_carries_no_key() {
        let scratch = Scratch::disk(DISK);
        run_layout(&scratch.path, &mut Vec::new()).unwrap();
        let dir = fake_mkfs(RECORDING_MKFS);
        run_volume(
            VolumeSettings::default(),
            None,
            &scratch.path,
            &dir.join("mkfs.btrfs"),
            &dir,
            None,
            &mut Vec::new(),
        )
        .unwrap();
        assert!(
            !dir.join("td-volume-root")
                .join(protocol::VOLUME_TRUSTED_KEY)
                .exists(),
            "a volume with no deployment carries a trust root"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A SUCCESSFUL EXIT IS NOT A PUBLISH.
    ///
    /// The shape this exists for: a td-boot that exits 0 having written
    /// nothing produced a complete, correct, mountable volume with an empty
    /// `td/deployments`, and `volume` printed its byte offsets and returned
    /// success. The disk installs and cannot boot, and the first thing to
    /// notice is the machine. Each of the three answers is refused separately,
    /// because they fail for different reasons: nothing printed, a malformed
    /// id, and — the one that matters — a well-formed id naming a directory
    /// that is not there.
    #[test]
    fn a_publish_that_writes_nothing_is_not_a_publish() {
        for (name, body) in [
            ("silent", "#!/bin/sh\nexit 0\n"),
            ("malformed", "#!/bin/sh\necho not-a-digest\n"),
            // The id is REAL and the directory is not: this is the only one of
            // the three a check on the child's output alone would pass.
            (
                "absent",
                "#!/bin/sh\necho 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\n",
            ),
        ] {
            let scratch = Scratch::disk(DISK);
            run_layout(&scratch.path, &mut Vec::new()).unwrap();
            let dir = fake_mkfs(RECORDING_MKFS);
            let td_boot = dir.join("td-boot");
            scratch::executable(&td_boot, body).unwrap();
            let publish = Publish {
                td_boot,
                deployment: PathBuf::from("/media/deployment"),
                trusted_key: key_file(&dir),
            };
            let error = run_volume(
                VolumeSettings::default(),
                None,
                &scratch.path,
                &dir.join("mkfs.btrfs"),
                &dir,
                Some(&VolumeSeed::Publish(publish)),
                &mut Vec::new(),
            )
            .unwrap_err();
            let text = format!("{error}");
            assert!(
                text.contains("published no deployment id") || text.contains("is not there"),
                "the {name} publish was taken for a real one: {text}"
            );
            assert!(
                !dir.join("argv").exists(),
                "the {name} publish still made a filesystem"
            );
            std::fs::remove_dir_all(&dir).unwrap();
        }
    }

    /// A DEPLOYMENTS DIRECTORY THAT CANNOT BE READ is not a publish that
    /// wrote nothing, and says so.
    ///
    /// This is the one behaviour `paths::is_dir` changed rather than merely
    /// named. `Path::is_dir` answers `false` for every failure it meets, so a
    /// `td/deployments` that is not a directory at all — or that this process
    /// may not traverse — read as an id td-boot never wrote, and the operator
    /// was told the program that had just done the work published nothing.
    /// The errno is the whole difference between "look at td-boot" and "look
    /// at the disk", so it is the errno that has to reach the message.
    #[test]
    fn a_deployments_directory_that_cannot_be_read_is_not_a_missing_publish() {
        let scratch = Scratch::disk(DISK);
        run_layout(&scratch.path, &mut Vec::new()).unwrap();
        let dir = fake_mkfs(RECORDING_MKFS);
        let td_boot = dir.join("td-boot");
        // The staging root is `$2`. td-install created `td/deployments` as
        // a directory before this ran; putting a FILE there is ENOTDIR on
        // the join below, which needs no ownership games to arrange and so
        // behaves the same for a test run as root.
        scratch::executable(
            &td_boot,
            "#!/bin/sh\nrmdir \"$2/td/deployments\"\n: > \"$2/td/deployments\"\n\
             echo 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\n",
        )
        .unwrap();
        let publish = Publish {
            td_boot,
            deployment: PathBuf::from("/media/deployment"),
            trusted_key: key_file(&dir),
        };
        let error = run_volume(
            VolumeSettings::default(),
            None,
            &scratch.path,
            &dir.join("mkfs.btrfs"),
            &dir,
            Some(&VolumeSeed::Publish(publish)),
            &mut Vec::new(),
        )
        .unwrap_err();
        let text = format!("{error}");
        assert!(
            !text.contains("is not there"),
            "an unreadable deployments directory was reported as a publish \
             that never happened: {text}"
        );
        assert!(
            text.contains("td/deployments"),
            "the refusal does not name the path it could not read: {text}"
        );
        assert!(
            !dir.join("argv").exists(),
            "the refusal still made a filesystem"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// An id that is well-formed but points OUT of the staging tree is refused
    /// on its shape, before it is joined onto a path.
    #[test]
    fn a_traversing_deployment_id_is_refused_before_it_is_joined() {
        let scratch = Scratch::disk(DISK);
        run_layout(&scratch.path, &mut Vec::new()).unwrap();
        let dir = fake_mkfs(RECORDING_MKFS);
        let td_boot = dir.join("td-boot");
        // `..` resolves to the staging tree itself, which IS a directory —
        // so a readback that joined first and asked afterwards would accept
        // this and report a deployment that was never written.
        scratch::executable(&td_boot, "#!/bin/sh\necho ../..\n").unwrap();
        let publish = Publish {
            td_boot,
            deployment: PathBuf::from("/media/deployment"),
            trusted_key: key_file(&dir),
        };
        let error = run_volume(
            VolumeSettings::default(),
            None,
            &scratch.path,
            &dir.join("mkfs.btrfs"),
            &dir,
            Some(&VolumeSeed::Publish(publish)),
            &mut Vec::new(),
        )
        .unwrap_err();
        assert!(
            format!("{error}").contains("published no deployment id"),
            "a traversing id must be refused on its shape: {error}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A td-boot that FAILS fails the install, rather than making a volume
    /// with no deployment in it and reporting success.
    #[test]
    fn a_failing_publish_fails_the_volume() {
        let scratch = Scratch::disk(DISK);
        run_layout(&scratch.path, &mut Vec::new()).unwrap();
        let dir = fake_mkfs(RECORDING_MKFS);
        let td_boot = dir.join("td-boot");
        scratch::executable(&td_boot, "#!/bin/sh\nexit 7\n").unwrap();
        let publish = Publish {
            td_boot,
            deployment: PathBuf::from("/media/deployment"),
            trusted_key: key_file(&dir),
        };
        let error = run_volume(
            VolumeSettings::default(),
            None,
            &scratch.path,
            &dir.join("mkfs.btrfs"),
            &dir,
            Some(&VolumeSeed::Publish(publish)),
            &mut Vec::new(),
        )
        .unwrap_err();
        assert!(
            format!("{error}").contains("publish failed"),
            "a failing publish must be reported: {error}"
        );
        // ...and mkfs never ran, so no volume was made to hold nothing.
        assert!(
            !dir.join("argv").exists(),
            "the filesystem was made despite the publish failing"
        );
        // Nor is the trust root there. The key is READ before the publish and
        // written after it, so a staging tree that failed carries no key — one
        // written first would outlive the publish it belonged to and be the
        // root a later, unrelated publish into the same scratch inherited.
        assert!(
            !dir.join("td-volume-root")
                .join(protocol::VOLUME_TRUSTED_KEY)
                .exists(),
            "a failed publish left its trust root behind"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A relative `td-boot` is refused rather than resolved through `PATH`,
    /// for the reason the mkfs one is.
    #[test]
    fn a_relative_td_boot_is_refused() {
        let scratch = Scratch::disk(DISK);
        run_layout(&scratch.path, &mut Vec::new()).unwrap();
        let dir = fake_mkfs(RECORDING_MKFS);
        let publish = Publish {
            td_boot: PathBuf::from("td-boot"),
            deployment: PathBuf::from("/media/deployment"),
            trusted_key: key_file(&dir),
        };
        let error = run_volume(
            VolumeSettings::default(),
            None,
            &scratch.path,
            &dir.join("mkfs.btrfs"),
            &dir,
            Some(&VolumeSeed::Publish(publish)),
            &mut Vec::new(),
        )
        .unwrap_err();
        // Named, not just refused: the mkfs refusal emits the same sentence, so
        // an assertion on it alone would pass whichever of the two fired.
        let text = format!("{error}");
        assert!(
            text.contains("resolves through PATH") && text.contains("td-boot"),
            "a bare td-boot name must be refused, and said to be td-boot's: {text}"
        );
        // ...and it costs nothing: the caller's staging tree is untouched,
        // which is what moving this check ahead of the wipe bought.
        assert!(
            !dir.join("td-volume-root").exists(),
            "an argv mistake destroyed the staging tree before reporting itself"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A relative `mkfs` is refused rather than resolved through `PATH`.
    #[test]
    fn only_main_writes_the_program_name_into_a_diagnostic() {
        // The subprocess guard can only reach diagnostics some invocation
        // produces, and most of these are overflow arms, block-device-only
        // paths and post-mkfs failures nothing drives — so a fifty-first
        // message written with the prefix would be invisible to it. This is a
        // scan over the crate's own SOURCE, the shape the confinement tests
        // elsewhere in td use for exactly this reason.
        //
        // The needle is COMPOSED rather than written as a literal, or this
        // assertion would count itself and be off by one forever.
        let needle = format!("{}td-install: ", '"');
        let hits: Vec<&str> = include_str!("main.rs")
            .lines()
            .filter(|line| line.contains(&needle))
            .collect();
        assert_eq!(
            hits.len(),
            2,
            "the program's name belongs at the one place that PRINTS an \
             error, not in the message: {hits:#?}"
        );
        for line in &hits {
            assert!(
                line.contains("writeln!(io::stderr()"),
                "this names the program somewhere other than main's printer: {line}"
            );
        }
    }

    #[test]
    fn a_relative_mkfs_is_refused_before_it_can_be_searched_for() {
        let scratch = Scratch::disk(DISK);
        run_layout(&scratch.path, &mut Vec::new()).unwrap();
        // Its own directory, though the refusal precedes every filesystem
        // operation: were that ordering ever lost, the image would land here
        // and not beside every other fixture.
        let dir = scratch::path("relative");
        std::fs::create_dir(&dir).unwrap();
        let error = run_volume(
            VolumeSettings::default(),
            None,
            &scratch.path,
            Path::new("mkfs.btrfs"),
            &dir,
            None,
            &mut Vec::new(),
        )
        .unwrap_err();
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(
            format!("{error}").contains("resolves through PATH"),
            "a bare name must be refused: {error}"
        );
    }

    /// A scratch directory that would put the image ON the destination is
    /// refused, rather than truncating the disk whose table was just parsed.
    #[test]
    fn a_scratch_image_that_is_the_destination_is_refused() {
        let dir = scratch::path("alias");
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("td-volume.img");
        File::create(&path).unwrap().set_len(DISK).unwrap();
        run_layout(&path, &mut Vec::new()).unwrap();
        let fake = fake_mkfs("#!/bin/sh\nexit 0\n");
        let error = run_volume(
            VolumeSettings::default(),
            None,
            &path,
            &fake.join("mkfs.btrfs"),
            &dir,
            None,
            &mut Vec::new(),
        )
        .unwrap_err();
        assert!(
            format!("{error}").contains("is the destination itself"),
            "the alias must be refused: {error}"
        );
        // ...and the disk it would have truncated is still the size it was.
        assert_eq!(std::fs::metadata(&path).unwrap().len(), DISK);
        std::fs::remove_dir_all(&dir).unwrap();
        std::fs::remove_dir_all(&fake).unwrap();
    }

    /// Both ENDS of the region are cleared before the copy, because a signature
    /// the sparse copy would skip can be at either.
    #[test]
    fn the_copy_clears_both_ends_of_the_region_first() {
        const MIB: u64 = 1024 * 1024;
        let dest = Scratch::disk(8 * MIB);
        let (offset, len) = (MIB, 6 * MIB);
        {
            let mut file = OpenOptions::new().write(true).open(&dest.path).unwrap();
            // A signature at each end of the region, and one in its middle that
            // the copy is expected to leave alone.
            write_at(&mut file, offset, &[0xaa; 512]).unwrap();
            write_at(&mut file, offset + len - 512, &[0xbb; 512]).unwrap();
            write_at(&mut file, offset + 3 * MIB, &[0xcc; 512]).unwrap();
        }
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&dest.path)
            .unwrap();
        zero_edges(&mut file, offset, len).unwrap();
        assert_eq!(dest.read_at(offset, 4), [0; 4], "the head was cleared");
        assert_eq!(
            dest.read_at(offset + len - 512, 4),
            [0; 4],
            "the tail was cleared — MD RAID and ZFS put metadata there"
        );
        assert_eq!(
            dest.read_at(offset + 3 * MIB, 4),
            [0xcc; 4],
            "the middle is the copy's to write, not this function's"
        );
    }

    /// The copy honours its RANGE, which is what lets `run_volume` order it.
    ///
    /// The ordering itself — everything but the first chunk, a barrier, then the
    /// first chunk — cannot be observed from the final state, and an interrupted
    /// install is not something a test can stage. What can be pinned is the
    /// mechanism the ordering rests on: a range that starts past a live chunk
    /// must leave that chunk's destination untouched, or "deferred" would mean
    /// "written twice" and the barrier would guarantee nothing.
    #[test]
    fn the_copy_writes_only_the_range_it_is_given() {
        const CHUNK: u64 = 1024 * 1024;
        let source = Scratch::disk(3 * CHUNK);
        let dest = Scratch::disk(4 * CHUNK);
        {
            let mut file = OpenOptions::new().write(true).open(&source.path).unwrap();
            write_at(&mut file, 0, &[0xab; 4096]).unwrap();
            write_at(&mut file, 2 * CHUNK, &[0xcd; 4096]).unwrap();
        }
        {
            let mut file = OpenOptions::new().write(true).open(&dest.path).unwrap();
            write_at(&mut file, CHUNK, &[0xee; 512]).unwrap();
        }
        let mut image = File::open(&source.path).unwrap();
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&dest.path)
            .unwrap();
        // Everything BUT the first chunk, exactly as `run_volume` does it.
        let written = copy_sparse(&mut image, &mut file, CHUNK, CHUNK, 3 * CHUNK).unwrap();
        assert_eq!(written, CHUNK, "only the one live chunk in range");
        assert_eq!(
            dest.read_at(CHUNK, 4),
            [0xee; 4],
            "the deferred chunk's destination was not touched"
        );
        assert_eq!(
            dest.read_at(3 * CHUNK, 4),
            [0xcd; 4],
            "the in-range chunk landed"
        );
        // ...and the deferred pass then lands it, at its own offset.
        let first = copy_sparse(&mut image, &mut file, CHUNK, 0, CHUNK).unwrap();
        assert_eq!(first, CHUNK);
        assert_eq!(dest.read_at(CHUNK, 4), [0xab; 4]);
    }

    /// The staging tree is EMPTIED, not merely ensured: `--rootdir` copies what
    /// is under it into the filesystem, so anything left there by a previous run
    /// would land on a machine's /var.
    #[test]
    fn a_stale_staging_tree_does_not_reach_the_volume() {
        let scratch = Scratch::disk(DISK);
        run_layout(&scratch.path, &mut Vec::new()).unwrap();
        let dir = fake_mkfs(RECORDING_MKFS);
        let stale = dir.join("td-volume-root").join("junk");
        std::fs::create_dir_all(stale.parent().unwrap()).unwrap();
        std::fs::write(&stale, b"not mine").unwrap();
        run_volume(
            VolumeSettings::default(),
            None,
            &scratch.path,
            &dir.join("mkfs.btrfs"),
            &dir,
            None,
            &mut Vec::new(),
        )
        .unwrap();
        assert!(
            !stale.exists(),
            "a stale staging file survived into the volume"
        );
        assert!(
            dir.join("td-volume-root")
                .join(protocol::VOLUME_SUBVOL)
                .is_dir(),
            "the subvolume directory is still staged"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A region too small for two disjoint edges is cleared once, whole,
    /// rather than twice over its own middle.
    #[test]
    fn a_region_smaller_than_two_edges_is_cleared_once_and_entirely() {
        const MIB: u64 = 1024 * 1024;
        let dest = Scratch::disk(4 * MIB);
        {
            let mut file = OpenOptions::new().write(true).open(&dest.path).unwrap();
            write_at(&mut file, MIB, &vec![0xff; MIB as usize]).unwrap();
        }
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&dest.path)
            .unwrap();
        zero_edges(&mut file, MIB, MIB).unwrap();
        assert_eq!(dest.read_at(MIB, 4), [0; 4]);
        assert_eq!(dest.read_at(2 * MIB - 4, 4), [0; 4], "to its very end");
    }

    #[test]
    fn format_cli_reuses_volume_options_after_the_boot_pair() {
        for tail in [
            vec![],
            vec!["--trusted-key", "/key"],
            vec!["/td-boot", "/bundle", "/key"],
        ] {
            let mut values = vec![
                "format",
                "/kernel",
                "/initrd",
                "--uuid",
                "12345678-1234-4234-8234-123456789abc",
                "--timezone",
                "Etc/UTC",
                "--hostname",
                "my-td",
                "--username",
                "alice",
                "/verified",
                "/firstboot",
                "disk",
                "/mkfs",
                "scratch",
            ];
            values.extend(tail);
            let parsed = parse_args(args(&values)).unwrap();
            assert!(matches!(parsed, Mode::Volume { boot: Some(ref boot),
                uuid: Some(_), timezone: Some(_), hostname: Some(_), username: Some(_), .. }
                if boot == &BootFiles { kernel: "/kernel".into(), initramfs: "/initrd".into() }));
        }
        for values in [
            vec!["format"],
            vec!["format", "/kernel"],
            vec!["format", "/kernel", "/initrd", "disk", "/mkfs"],
            vec![
                "format", "/kernel", "/initrd", "disk", "/mkfs", "scratch", "extra",
            ],
            vec![
                "format", "/kernel", "/initrd", "--uuid", "invalid", "disk", "/mkfs", "scratch",
            ],
            vec![
                "format",
                "/kernel",
                "/initrd",
                "--timezone",
                "Etc/UTC",
                "--uuid",
                "12345678-1234-4234-8234-123456789abc",
                "disk",
                "/mkfs",
                "scratch",
            ],
        ] {
            assert!(parse_args(args(&values)).is_err(), "{values:?}");
        }
    }

    #[test]
    fn format_cli_explains_options_before_boot_files() {
        for pair in [
            ["--uuid", "12345678-1234-4234-8234-123456789abc"],
            ["/kernel", "--timezone"],
            ["-kernel", "/initrd"],
        ] {
            let error = parse_args(args(&[
                "format", pair[0], pair[1], "disk", "/mkfs", "scratch",
            ]))
            .unwrap_err();
            assert!(
                error.to_string().contains("paths before volume options"),
                "{error}"
            );
        }
        assert!(parse_args(args(&[
            "format",
            "./-kernel",
            "/initrd",
            "disk",
            "/mkfs",
            "scratch"
        ]))
        .is_ok());
    }

    fn combined_fixture(body: &str) -> (Scratch, ScratchDirectory, BootFiles) {
        let disk = Scratch::disk(DISK);
        run_layout(&disk.path, &mut Vec::new()).unwrap();
        let mut file = OpenOptions::new().write(true).open(&disk.path).unwrap();
        let offset = plan(512, DISK).unwrap().volume_start * 512;
        for at in [MIB, offset, DISK - 2 * MIB] {
            write_at(&mut file, at, &[0xa5; 4096]).unwrap();
        }
        let dir = ScratchDirectory(fake_mkfs(body));
        let boot = BootFiles {
            kernel: dir.0.join("kernel"),
            initramfs: dir.0.join("initrd"),
        };
        std::fs::write(&boot.kernel, b"retained kernel").unwrap();
        std::fs::write(&boot.initramfs, b"retained initrd").unwrap();
        (disk, dir, boot)
    }

    #[test]
    fn combined_format_preparation_failures_preserve_existing_layout_and_volume() {
        for (body, reason) in [
            ("#!/bin/sh\nexit 3\n", "failed on the scratch image"),
            (
                "#!/bin/sh\nfor image do :; done\nrm -- \"$image\"\n",
                "prepared Btrfs image",
            ),
            (
                "#!/bin/sh\nfor image do :; done\n: > \"$image\"\n",
                "bytes, not the required",
            ),
            (
                "#!/bin/sh\nfor image do :; done\nprintf x >> \"$image\"\n",
                "bytes, not the required",
            ),
        ] {
            let (disk, dir, boot) = combined_fixture(body);
            let before = volume_write_snapshot(&disk);
            let mkfs = dir.0.join("mkfs.btrfs");
            let prepared =
                prepare_volume(VolumeSettings::default(), None, &mkfs, &dir.0, None).unwrap();
            let mut output = Vec::new();
            let error = run_format(prepared, &disk.path, &boot, &mut output).unwrap_err();
            assert!(error.to_string().contains(reason), "{error}");
            assert!(
                before == volume_write_snapshot(&disk),
                "{reason} changed the disk"
            );
            assert_eq!(std::fs::metadata(&disk.path).unwrap().len(), DISK);
            assert!(output.is_empty());
        }
    }

    #[test]
    fn combined_format_admits_boot_files_before_touching_scratch() {
        let (disk, dir, mut boot) = combined_fixture(RECORDING_MKFS);
        boot.initramfs = dir.0.join("missing");
        let mkfs = dir.0.join("mkfs.btrfs");
        let before = volume_write_snapshot(&disk);
        let prepared =
            prepare_volume(VolumeSettings::default(), None, &mkfs, &dir.0, None).unwrap();
        let error = run_format(prepared, &disk.path, &boot, &mut Vec::new()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert!(!dir.0.join("argv").exists());
        assert!(!dir.0.join("td-volume-root").exists());
        assert!(before == volume_write_snapshot(&disk));
    }

    #[test]
    fn combined_format_refuses_unusable_scratch_before_layout() {
        let (disk, dir, boot) = combined_fixture(RECORDING_MKFS);
        let scratch = dir.0.join("not-a-directory");
        std::fs::write(&scratch, b"keep").unwrap();
        let before = volume_write_snapshot(&disk);
        let mkfs = dir.0.join("mkfs.btrfs");
        let prepared =
            prepare_volume(VolumeSettings::default(), None, &mkfs, &scratch, None).unwrap();
        assert!(run_format(prepared, &disk.path, &boot, &mut Vec::new()).is_err());
        assert!(!dir.0.join("argv").exists());
        assert_eq!(std::fs::read(scratch).unwrap(), b"keep");
        assert!(before == volume_write_snapshot(&disk));
    }

    #[test]
    fn combined_format_rechecks_boot_lengths_after_filesystem_preparation() {
        let (disk, dir, boot) =
            combined_fixture("#!/bin/sh\nset -eu\nbase=$(dirname \"$0\")\n: > \"$base/kernel\"\n");
        let before = volume_write_snapshot(&disk);
        let mkfs = dir.0.join("mkfs.btrfs");
        let prepared =
            prepare_volume(VolumeSettings::default(), None, &mkfs, &dir.0, None).unwrap();
        let error = run_format(prepared, &disk.path, &boot, &mut Vec::new()).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("EFI input changed size before layout"),
            "{error}"
        );
        assert!(before == volume_write_snapshot(&disk));
    }

    #[test]
    fn combined_format_retains_destination_and_boot_inputs_across_preparation() {
        let body = concat!("#!/bin/sh\nset -eu\nbase=$(dirname \"$0\")\n",
            "mv -- \"$base/disk\" \"$base/retained\"\nprintf replacement > \"$base/disk\"\n",
            "mv -- \"$base/kernel\" \"$base/kernel.old\"\nprintf replacement > \"$base/kernel\"\n",
            "for image do :; done\nprintf prepared | dd of=\"$image\" bs=1 seek=65536 conv=notrunc 2>/dev/null\n");
        let (disk, dir, boot) = combined_fixture(body);
        let path = dir.0.join("disk");
        std::fs::rename(&disk.path, &path).unwrap();
        let mkfs = dir.0.join("mkfs.btrfs");
        let prepared =
            prepare_volume(VolumeSettings::default(), None, &mkfs, &dir.0, None).unwrap();
        let mut output = Vec::new();
        run_format(prepared, &path, &boot, &mut output).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"replacement");
        assert_eq!(std::fs::read(&boot.kernel).unwrap(), b"replacement");
        let mut original = File::open(dir.0.join("retained")).unwrap();
        let (offset, len) = destination_volume(&mut original).unwrap();
        assert_eq!(
            read_at(&mut original, offset + 65536, 8).unwrap(),
            b"prepared"
        );
        assert_eq!(output, format!("{offset} {len} {MIB}\n").as_bytes());
        let esp_bytes = read_at(&mut original, MIB, 16 * MIB).unwrap();
        assert!(esp_bytes
            .windows(b"retained kernel".len())
            .any(|bytes| bytes == b"retained kernel"));
        assert!(esp_bytes
            .windows(b"retained initrd".len())
            .any(|bytes| bytes == b"retained initrd"));
        assert!(!esp_bytes
            .windows(b"replacement".len())
            .any(|bytes| bytes == b"replacement"));
    }

    #[test]
    fn combined_format_succeeds_without_an_existing_gpt() {
        let disk = Scratch::disk(DISK);
        let dir = ScratchDirectory(fake_mkfs(RECORDING_MKFS));
        let boot = BootFiles {
            kernel: dir.0.join("kernel"),
            initramfs: dir.0.join("initrd"),
        };
        std::fs::write(&boot.kernel, b"kernel").unwrap();
        std::fs::write(&boot.initramfs, b"initrd").unwrap();
        assert!(destination_volume(&mut File::open(&disk.path).unwrap()).is_err());
        let mkfs = dir.0.join("mkfs.btrfs");
        let prepared =
            prepare_volume(VolumeSettings::default(), None, &mkfs, &dir.0, None).unwrap();
        let mut output = Vec::new();
        run_format(prepared, &disk.path, &boot, &mut output).unwrap();
        let (offset, len) = destination_volume(&mut File::open(&disk.path).unwrap()).unwrap();
        assert_eq!(offset, 537919488);
        assert_eq!(output, format!("{offset} {len} 0\n").as_bytes());
        assert_eq!(std::fs::metadata(&disk.path).unwrap().len(), DISK);
    }

    /// What a format wrote, with the digests of the combined fixture's
    /// kernel and initramfs.
    fn formatted_fixture() -> (
        Scratch,
        ScratchDirectory,
        FormatDestination,
        Formatted,
        [[u8; 32]; 2],
    ) {
        let (disk, dir, boot) = combined_fixture(RECORDING_MKFS);
        let mkfs = dir.0.join("mkfs.btrfs");
        let prepared =
            prepare_volume(VolumeSettings::default(), None, &mkfs, &dir.0, None).unwrap();
        let mut destination = FormatDestination::open(&disk.path).unwrap();
        let digests = [
            sha256::hex_digest(b"retained kernel"),
            sha256::hex_digest(b"retained initrd"),
        ]
        .map(|hex| digest_bytes(&hex).unwrap());
        let formatted = format_held(
            prepared,
            &mut destination,
            &boot,
            Some(&digests),
            &mut io::sink(),
            &mut |_| {},
        )
        .unwrap();
        (disk, dir, destination, formatted, digests)
    }

    /// After an install the boot artifacts are read back off the disk
    /// against what was written: both tables, the ESP's metadata, and each
    /// file and its cluster's zeroed rest. A byte of difference anywhere in
    /// them refuses.
    #[test]
    fn installed_boot_artifacts_are_read_back() {
        let (_disk, _dir, mut destination, formatted, digests) = formatted_fixture();
        let file = &mut destination.file;
        assert!(formatted.published.is_none());
        verify_boot(file, &formatted, &digests).unwrap();
        let [kernel, initramfs] = digests;
        assert!(verify_boot(file, &formatted, &[initramfs, kernel]).is_err());
        let (metadata_at, metadata) = &formatted.metadata;
        // The first and last bytes of every region the read-back covers, and
        // the last written metadata byte, in \EFI\BOOT's entries.
        let entries = metadata.iter().rposition(|byte| *byte != 0).unwrap() as u64;
        let mut flipped = vec![
            formatted.table.primary_offset,
            formatted.table.primary_offset + formatted.table.primary.len() as u64 - 1,
            formatted.table.backup_offset,
            formatted.table.backup_offset + formatted.table.backup.len() as u64 - 1,
            *metadata_at,
            metadata_at + entries,
            metadata_at + metadata.len() as u64 - 1,
        ];
        for (offset, len, padding) in formatted.boot {
            assert!(padding > 0, "the fixture's files end mid-cluster");
            flipped.extend([
                offset,
                offset + len - 1,
                offset + len,
                offset + len + padding - 1,
            ]);
        }
        for at in flipped {
            let byte = read_at(file, at, 1).unwrap();
            write_at(file, at, &[byte[0] ^ 1]).unwrap();
            assert!(
                verify_boot(file, &formatted, &digests).is_err(),
                "byte {at}"
            );
            write_at(file, at, &byte).unwrap();
        }
        verify_boot(file, &formatted, &digests).unwrap();
        // A volume the tables do not place where the layout did.
        let moved = Formatted {
            volume: (formatted.volume.0 + 512, formatted.volume.1),
            table: formatted.table.clone(),
            metadata: formatted.metadata.clone(),
            boot: formatted.boot,
            published: None,
        };
        assert!(verify_boot(file, &moved, &digests).is_err());
    }

    /// Verifying boot passes only for the plan's published id and the boot
    /// artifacts as written; any other outcome leaves the disk without a
    /// table, so firmware does not try it.
    #[test]
    fn an_installation_that_does_not_verify_is_withdrawn() {
        let id = "ab".repeat(32);
        let published = |formatted: Formatted, stdout: &[u8]| Formatted {
            published: Some(stdout.to_vec()),
            ..formatted
        };
        let (_disk, _dir, mut destination, formatted, digests) = formatted_fixture();
        let formatted = published(formatted, format!("{id}\n").as_bytes());
        finish_installation(&mut destination.file, &formatted, &id, &digests).unwrap();
        destination_volume(&mut destination.file).unwrap();
        // Both copies, since firmware falls back to a valid backup.
        let withdrawn = |destination: &mut FormatDestination, table: &gpt::Image| {
            for (offset, len) in [
                (table.primary_offset, table.primary.len()),
                (table.backup_offset, table.backup.len()),
            ] {
                let bytes = read_at(&mut destination.file, offset, len as u64).unwrap();
                assert!(bytes.iter().all(|byte| *byte == 0));
            }
            assert!(destination_volume(&mut destination.file).is_err());
        };
        // Nothing published, another id, or output that is no id.
        for stdout in [
            None,
            Some(format!("{}\n", "cd".repeat(32))),
            Some(id.clone()),
        ] {
            let (_disk, _dir, mut destination, formatted, digests) = formatted_fixture();
            let formatted = Formatted {
                published: stdout.map(String::into_bytes),
                ..formatted
            };
            assert!(finish_installation(&mut destination.file, &formatted, &id, &digests).is_err());
            withdrawn(&mut destination, &formatted.table);
        }
        // The plan's id, but a boot artifact that does not read back, or the
        // digests in the other order.
        let (_disk, _dir, mut destination, formatted, digests) = formatted_fixture();
        let formatted = published(formatted, format!("{id}\n").as_bytes());
        let (kernel_at, _, _) = formatted.boot[0];
        write_at(&mut destination.file, kernel_at, b"X").unwrap();
        assert!(finish_installation(&mut destination.file, &formatted, &id, &digests).is_err());
        withdrawn(&mut destination, &formatted.table);
        let (_disk, _dir, mut destination, formatted, [kernel, initramfs]) = formatted_fixture();
        let formatted = published(formatted, format!("{id}\n").as_bytes());
        assert!(
            finish_installation(&mut destination.file, &formatted, &id, &[initramfs, kernel])
                .is_err()
        );
        withdrawn(&mut destination, &formatted.table);
    }

    /// The selector, like the kernel, is checked through the descriptor the
    /// copy reads before anything is written.
    #[test]
    fn either_boot_file_off_its_digest_refuses_before_writing() {
        let (disk, _dir, boot) = combined_fixture(RECORDING_MKFS);
        let digests = [
            sha256::hex_digest(b"retained kernel"),
            sha256::hex_digest(b"retained initrd"),
        ]
        .map(|hex| digest_bytes(&hex).unwrap());
        let esp = plan(512, DISK).unwrap().esp_offset().unwrap();
        let snapshot = |disk: &Scratch| {
            let mut file = File::open(&disk.path).unwrap();
            let mut regions = volume_write_snapshot(disk);
            regions.push(read_at(&mut file, 0, MIB).unwrap());
            regions.push(read_at(&mut file, esp, MIB).unwrap());
            regions
        };
        let before = snapshot(&disk);
        let mut destination = FormatDestination::open(&disk.path).unwrap();
        prepare_layout(&mut destination, Some(&boot), Some(&digests)).unwrap();
        let [kernel, initramfs] = digests;
        for wrong in [[initramfs, initramfs], [kernel, kernel]] {
            let error = prepare_layout(&mut destination, Some(&boot), Some(&wrong))
                .err()
                .unwrap();
            assert!(error.to_string().contains("is not the"), "{error}");
        }
        assert!(snapshot(&disk) == before);
    }

    #[test]
    fn prepared_layout_refuses_changed_geometry_without_writes() {
        let (disk, _dir, boot) = combined_fixture(RECORDING_MKFS);
        let mut destination = FormatDestination::open(&disk.path).unwrap();
        let layout = prepare_layout(&mut destination, Some(&boot), None).unwrap();
        destination.file.set_len(DISK + MIB).unwrap();
        let before = volume_write_snapshot(&disk);
        let error = layout
            .write_to(&mut destination, &mut Vec::new())
            .unwrap_err();
        assert!(error.to_string().contains("geometry changed"), "{error}");
        assert!(before == volume_write_snapshot(&disk));
        assert_eq!(destination.file.metadata().unwrap().len(), DISK + MIB);
    }

    fn volume_write_snapshot(disk: &Scratch) -> Vec<Vec<u8>> {
        let plan = plan(512, DISK).unwrap();
        let offset = plan.volume_start * 512;
        let len = (plan.volume_end - plan.volume_start + 1) * 512;
        [
            (0, 2 * MIB),
            (offset, MIB),
            (offset + len / 2, 4096),
            (offset + len - MIB, MIB),
            (DISK - 65536, 65536),
        ]
        .into_iter()
        .map(|(at, len)| disk.read_at(at, len as usize))
        .collect()
    }

    fn unlaid_volume_image(
        len: u64,
    ) -> (
        Scratch,
        ScratchDirectory,
        FormatDestination,
        PreparedVolumeImage,
    ) {
        let disk = Scratch::disk(DISK);
        let mut destination = FormatDestination::open(&disk.path).unwrap();
        let plan = plan(512, DISK).unwrap();
        let offset = plan.volume_start * 512;
        let volume_len = (plan.volume_end - plan.volume_start + 1) * 512;
        for (at, count) in [
            (0, 4096),
            (MIB, 4096),
            (DISK - 4096, 4096),
            (offset, MIB),
            (offset + volume_len / 2, 4096),
            (offset + volume_len - MIB, MIB),
        ] {
            write_at(&mut destination.file, at, &vec![0xa5; count as usize]).unwrap();
        }
        let before = volume_write_snapshot(&disk);
        let dir = ScratchDirectory(fake_mkfs(RECORDING_MKFS));
        let mkfs = dir.0.join("mkfs.btrfs");
        let prepared =
            prepare_volume(VolumeSettings::default(), None, &mkfs, &dir.0, None).unwrap();
        let image = prepare_volume_image(prepared, &destination, len).unwrap();
        assert!(
            before == volume_write_snapshot(&disk),
            "preparation wrote the destination"
        );
        assert_eq!(std::fs::metadata(&disk.path).unwrap().len(), DISK);
        assert!(
            destination_volume(&mut destination.file).is_err(),
            "preparation laid out a disk"
        );
        (disk, dir, destination, image)
    }

    #[test]
    fn staged_volume_before_layout_copies_the_held_image_after_path_replacement() {
        let plan = plan(512, DISK).unwrap();
        let len = (plan.volume_end - plan.volume_start + 1) * 512;
        let (disk, dir, mut destination, image) = unlaid_volume_image(len);
        let image_path = dir.0.join("td-volume.img");
        // Finish caller-owned fixture bytes before copying, then replace only
        // the name. The admitted descriptor must still supply the original.
        let mut source = OpenOptions::new().write(true).open(&image_path).unwrap();
        write_at(&mut source, 65536, b"original prepared bytes").unwrap();
        drop(source);
        std::fs::rename(&image_path, dir.0.join("original.img")).unwrap();
        let mut replacement = File::create(&image_path).unwrap();
        replacement.set_len(len).unwrap();
        write_at(&mut replacement, 65536, b"replacement must survive").unwrap();
        drop(replacement);

        format_layout(&mut destination, None, &mut Vec::new()).unwrap();
        let mut output = Vec::new();
        image.write_to(&mut destination, &mut output).unwrap();
        let offset = plan.volume_start * 512;
        assert_eq!(disk.read_at(offset + 65536, 23), b"original prepared bytes");
        assert_eq!(output, format!("{offset} {len} {MIB}\n").as_bytes());
        let mut replacement = File::open(&image_path).unwrap();
        assert_eq!(
            read_at(&mut replacement, 65536, 24).unwrap(),
            b"replacement must survive"
        );
        assert_eq!(replacement.metadata().unwrap().len(), len);
        assert_eq!(
            destination_volume(&mut destination.file).unwrap(),
            (offset, len)
        );
        assert_eq!(std::fs::metadata(&disk.path).unwrap().len(), DISK);
    }

    #[test]
    fn staged_volume_refuses_its_own_inode_as_the_copy_destination() {
        let plan = plan(512, DISK).unwrap();
        let len = (plan.volume_end - plan.volume_start + 1) * 512;
        let (disk, dir, _destination, image) = unlaid_volume_image(len);
        let mut alias = FormatDestination::open(&dir.0.join("td-volume.img")).unwrap();
        write_at(&mut alias.file, 0, &[0x5a; 4096]).unwrap();
        let before = volume_write_snapshot(&disk);
        let mut output = Vec::new();
        let error = image.write_to(&mut alias, &mut output).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(
            error.to_string(),
            "prepared Btrfs image is the destination itself"
        );
        assert_eq!(read_at(&mut alias.file, 0, 4096).unwrap(), vec![0x5a; 4096]);
        assert_eq!(alias.file.metadata().unwrap().len(), len);
        assert!(before == volume_write_snapshot(&disk));
        assert!(output.is_empty());
    }

    #[test]
    fn staged_volume_refuses_a_different_destination_extent_before_writes() {
        let plan = plan(512, DISK).unwrap();
        let len = (plan.volume_end - plan.volume_start + 1) * 512;
        for prepared_len in [len - 512, len + 512] {
            let (disk, _dir, mut destination, image) = unlaid_volume_image(prepared_len);
            format_layout(&mut destination, None, &mut Vec::new()).unwrap();
            let before = volume_write_snapshot(&disk);
            let mut output = Vec::new();
            let result = image.write_to(&mut destination, &mut output);
            assert!(
                before == volume_write_snapshot(&disk),
                "extent refusal wrote the destination"
            );
            let error = result.unwrap_err();
            assert!(
                error.to_string().contains("but the destination volume has"),
                "{error}"
            );
            assert_eq!(std::fs::metadata(&disk.path).unwrap().len(), DISK);
            assert!(output.is_empty());
        }
    }

    #[test]
    fn staged_volume_refuses_a_resized_source_before_writes() {
        let plan = plan(512, DISK).unwrap();
        let len = (plan.volume_end - plan.volume_start + 1) * 512;
        for changed_len in [len - 512, len + 512] {
            let (disk, dir, mut destination, image) = unlaid_volume_image(len);
            format_layout(&mut destination, None, &mut Vec::new()).unwrap();
            OpenOptions::new()
                .write(true)
                .open(dir.0.join("td-volume.img"))
                .unwrap()
                .set_len(changed_len)
                .unwrap();
            let before = volume_write_snapshot(&disk);
            let mut output = Vec::new();
            let result = image.write_to(&mut destination, &mut output);
            assert!(
                before == volume_write_snapshot(&disk),
                "source refusal wrote the destination"
            );
            let error = result.unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("prepared Btrfs image changed size"),
                "{error}"
            );
            assert_eq!(std::fs::metadata(&disk.path).unwrap().len(), DISK);
            assert!(output.is_empty());
        }
    }

    #[derive(Clone, Copy, Debug)]
    enum PreparedImageFault {
        Missing,
        Truncated,
        Grown,
        Replaced,
        Aliased,
    }

    /// Observe the parent's held inode from the child, then force admission
    /// failures where a late refusal would erase the destination's canaries.
    fn refused_prepared_image_preserves_destination(fault: PreparedImageFault) {
        let disk = Scratch::disk(DISK);
        run_layout(&disk.path, &mut Vec::new()).unwrap();
        let plan = plan(512, DISK).unwrap();
        let offset = plan.volume_start * 512;
        let len = (plan.volume_end - plan.volume_start + 1) * 512;
        let ranges = [
            (0, 64 * 1024),
            (offset, MIB),
            (offset + len / 2, 4096),
            (offset + len - MIB, MIB),
            (DISK - 64 * 1024, 64 * 1024),
        ];
        let mut file = OpenOptions::new().write(true).open(&disk.path).unwrap();
        for (at, count) in ranges.iter().skip(1).take(3) {
            write_at(&mut file, *at, &vec![0xa5; *count as usize]).unwrap();
        }
        drop(file);
        let snapshot = || {
            ranges
                .iter()
                .map(|(at, count)| disk.read_at(*at, *count as usize))
                .collect::<Vec<_>>()
        };
        let before = snapshot();
        let (action, expected_kind, reason) = match fault {
            PreparedImageFault::Missing => (
                "rm -- \"$image\"",
                io::ErrorKind::NotFound,
                "prepared Btrfs image",
            ),
            PreparedImageFault::Truncated => (
                ": > \"$image\"",
                io::ErrorKind::InvalidData,
                "bytes, not the required",
            ),
            PreparedImageFault::Grown => (
                "printf x >> \"$image\"",
                io::ErrorKind::InvalidData,
                "bytes, not the required",
            ),
            PreparedImageFault::Replaced => (
                "mv -- \"$image.replacement\" \"$image\"",
                io::ErrorKind::InvalidData,
                "prepared Btrfs image was replaced",
            ),
            PreparedImageFault::Aliased => (
                "mv -- \"$image.replacement\" \"$image\"",
                io::ErrorKind::InvalidData,
                "prepared Btrfs image must be a real regular file",
            ),
        };
        let observe_retention = concat!(
            "#!/bin/sh\nset -eu\nfor image in \"$@\"; do :; done\n",
            "held=no\nfor fd in /proc/\"$PPID\"/fd/*; do\n",
            "  if [ \"$fd\" -ef \"$image\" ]; then held=yes; break; fi\n",
            "done\n[ \"$held\" = yes ] || exit 9\n",
        );
        let dir = fake_mkfs(&format!(
            "{observe_retention}{action}\nprintf done > \"$image.done\"\n"
        ));
        let alternate = dir.join("td-volume.img.replacement");
        match fault {
            PreparedImageFault::Replaced => File::create(&alternate).unwrap().set_len(len).unwrap(),
            PreparedImageFault::Aliased => {
                std::os::unix::fs::symlink(&disk.path, &alternate).unwrap()
            }
            _ => {}
        }
        let mut output = Vec::new();
        let result = run_volume(
            VolumeSettings::default(),
            None,
            &disk.path,
            &dir.join("mkfs.btrfs"),
            &dir,
            None,
            &mut output,
        );
        let completed = dir.join("td-volume.img.done").is_file();
        let after = snapshot();
        let disk_len = std::fs::metadata(&disk.path).unwrap().len();
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(
            completed,
            "formatter did not observe the held inode and complete its mutation: {result:?}"
        );
        let error = result.expect_err("invalid prepared image was accepted");
        assert_eq!(
            error.kind(),
            expected_kind,
            "wrong refusal for {fault:?}: {error}"
        );
        assert!(
            error.to_string().contains(reason),
            "wrong refusal for {fault:?}: {error}"
        );
        assert!(output.is_empty(), "a refused volume reported success");
        assert!(
            before == after,
            "prepared-image refusal changed destination bytes"
        );
        assert_eq!(disk_len, DISK);
    }

    #[test]
    fn missing_prepared_image_preserves_destination() {
        refused_prepared_image_preserves_destination(PreparedImageFault::Missing);
    }

    #[test]
    fn truncated_prepared_image_preserves_destination() {
        refused_prepared_image_preserves_destination(PreparedImageFault::Truncated);
    }

    #[test]
    fn grown_prepared_image_preserves_destination() {
        refused_prepared_image_preserves_destination(PreparedImageFault::Grown);
    }

    #[test]
    fn replaced_prepared_image_preserves_destination() {
        refused_prepared_image_preserves_destination(PreparedImageFault::Replaced);
    }

    #[test]
    fn aliased_prepared_image_preserves_destination() {
        refused_prepared_image_preserves_destination(PreparedImageFault::Aliased);
    }

    /// A `mkfs` that FAILS fails the install, rather than leaving a partition
    /// with whatever was in it before and a zero exit status.
    #[test]
    fn a_failing_mkfs_fails_the_volume() {
        let scratch = Scratch::disk(DISK);
        run_layout(&scratch.path, &mut Vec::new()).unwrap();
        let dir = fake_mkfs("#!/bin/sh\nexit 3\n");
        let error = run_volume(
            VolumeSettings::default(),
            None,
            &scratch.path,
            &dir.join("mkfs.btrfs"),
            &dir,
            None,
            &mut Vec::new(),
        )
        .unwrap_err();
        assert!(
            format!("{error}").contains("failed on the scratch image"),
            "a failing mkfs must be reported: {error}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The sparse copy writes the chunks that hold something and skips the
    /// holes — and the skip is not free, which is the half worth pinning: a
    /// byte under a hole SURVIVES. That is why `run_volume` zeroes the region's
    /// first megabyte before copying, and this is the test that would notice if
    /// the skip ever silently became a full copy (`written` would jump).
    #[test]
    fn the_sparse_copy_skips_holes_and_keeps_what_is_under_them() {
        const CHUNK: u64 = 1024 * 1024;
        let source = Scratch::disk(3 * CHUNK);
        let dest = Scratch::disk(4 * CHUNK);
        {
            let mut file = OpenOptions::new().write(true).open(&source.path).unwrap();
            // First chunk holds data, second is a hole, third holds data.
            write_at(&mut file, 0, &[0xab; 4096]).unwrap();
            write_at(&mut file, 2 * CHUNK, &[0xcd; 4096]).unwrap();
        }
        {
            // Something already on the destination, under what will be the hole.
            let mut file = OpenOptions::new().write(true).open(&dest.path).unwrap();
            write_at(&mut file, CHUNK + CHUNK, &[0xee; 512]).unwrap();
        }
        let mut image = File::open(&source.path).unwrap();
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&dest.path)
            .unwrap();
        let written = copy_sparse(&mut image, &mut file, CHUNK, 0, 3 * CHUNK).unwrap();
        assert_eq!(written, 2 * CHUNK, "only the two live chunks are written");
        assert_eq!(dest.read_at(CHUNK, 4), [0xab; 4], "the first chunk arrived");
        assert_eq!(
            dest.read_at(3 * CHUNK, 4),
            [0xcd; 4],
            "the third chunk arrived at its own offset, not packed after the first"
        );
        assert_eq!(
            dest.read_at(2 * CHUNK, 4),
            [0xee; 4],
            "the skipped hole left the destination's own bytes in place"
        );
    }

    #[test]
    fn a_plan_puts_both_partitions_where_gpt_allows_and_alignment_requires() {
        let p = plan(512, DISK).unwrap();
        let align = protocol::PARTITION_ALIGN_BYTES / 512;

        assert_eq!(p.esp_start % align, 0, "ESP start is 1 MiB aligned");
        assert_eq!(p.volume_start % align, 0, "volume start is 1 MiB aligned");
        assert!(p.esp_start >= gpt::first_usable_lba(512).unwrap());
        assert_eq!(
            p.volume_end,
            gpt::last_usable_lba(512, DISK / 512).unwrap(),
            "the volume takes the REMAINDER of the disk; stopping short is \
             capacity lost with nothing reporting it"
        );
        assert!(p.esp_end < p.volume_start, "the partitions do not overlap");
        assert_eq!(
            p.volume_start,
            align_up(p.esp_end + 1, align).unwrap(),
            "the gap after the ESP is the alignment and nothing more"
        );
        assert_eq!(
            (p.esp_end - p.esp_start + 1) * 512,
            protocol::ESP_BYTES,
            "the ESP is exactly its declared size"
        );
    }

    /// The end LBA is INCLUSIVE. An exclusive end is an off-by-one that no
    /// reader can detect and that overlaps the next partition by one sector.
    /// Both ends are INCLUSIVE, which is how GPT stores them: an exclusive end
    /// is an off-by-one no reader detects and one sector of overlap with the
    /// next partition.
    #[test]
    fn partition_ends_are_inclusive() {
        let p = plan(512, DISK).unwrap();
        // The ESP spans esp_end - esp_start + 1 sectors, so its declared size
        // is only exact if the end is the LAST sector rather than one past it.
        assert_eq!(p.esp_sectors().unwrap(), protocol::ESP_BYTES / 512);
        // Same for the volume, whose end is the last usable LBA and not the
        // first unusable one — a disk's final sector is addressable.
        let last = gpt::last_usable_lba(512, DISK / 512).unwrap();
        assert_eq!(p.volume_end, last);
        assert!(
            p.volume_end < p.disk_sectors,
            "the end is an LBA, not a count"
        );
    }

    #[test]
    fn a_disk_too_small_for_retention_and_an_update_is_refused_by_name() {
        let error = plan(512, protocol::ESP_BYTES + 64 * MIB).unwrap_err();
        assert!(error.contains("td volume"), "{error}");
        // Smaller than the ESP itself: the volume never starts.
        let error = plan(512, 16 * MIB).unwrap_err();
        assert!(error.contains("too small"), "{error}");
        // Smaller than a GPT, which is refused before any partition is placed.
        let error = plan(512, 8 * 512).unwrap_err();
        assert!(error.contains("GPT alone"), "{error}");
    }

    #[test]
    fn a_size_that_is_not_whole_sectors_is_refused() {
        let error = plan(512, 8 * GIB + 1).unwrap_err();
        assert!(error.contains("whole number"), "{error}");
    }

    /// 4Kn disks are laid out in their OWN sectors, not in 512-byte ones.
    #[test]
    fn a_4kn_disk_plans_in_its_own_sectors() {
        let p = plan(4096, DISK).unwrap();
        assert_eq!(p.sector_size, 4096);
        assert_eq!(p.disk_sectors, DISK / 4096);
        assert_eq!((p.esp_end - p.esp_start + 1) * 4096, protocol::ESP_BYTES);
        assert_eq!(p.esp_start % (protocol::PARTITION_ALIGN_BYTES / 4096), 0);
    }

    #[test]
    fn align_up_rounds_and_refuses_a_zero_alignment() {
        assert_eq!(align_up(0, 2048), Some(0));
        assert_eq!(align_up(1, 2048), Some(2048));
        assert_eq!(align_up(2048, 2048), Some(2048));
        assert_eq!(align_up(2049, 2048), Some(4096));
        assert_eq!(align_up(1, 0), None, "a zero alignment is not a no-op");
        assert_eq!(align_up(u64::MAX, 2048), None, "overflow is not wrapped");
    }

    #[test]
    fn a_laid_out_disk_carries_a_table_gpt_reads_back() {
        let scratch = Scratch::disk(DISK);
        run_layout(&scratch.path, &mut Vec::new()).unwrap();
        let table = scratch.table(DISK);

        let p = plan(512, DISK).unwrap();
        assert_eq!(table.partitions.len(), 2);
        let esp = table.partitions.first().unwrap();
        let volume = table.partitions.get(1).unwrap();
        assert_eq!(esp.type_guid, gpt::TYPE_ESP);
        assert_eq!(esp.name, protocol::ESP_PARTITION_NAME);
        assert_eq!(volume.type_guid, gpt::TYPE_LINUX_FS);
        assert_eq!(volume.name, protocol::VOLUME_PARTITION_NAME);
        // Against the PLAN, which is the divergence this crate can have: the
        // FAT geometry is derived from `plan` and the GPT entries from
        // `layout`, so a table written for a different layout than the ESP was
        // formatted against is well-formed and wrong.
        assert_eq!(table.disk_sectors, p.disk_sectors);
        assert_eq!((esp.start_lba, esp.end_lba), (p.esp_start, p.esp_end));
        assert_eq!(
            (volume.start_lba, volume.end_lba),
            (p.volume_start, p.volume_end)
        );
        assert_ne!(esp.unique_guid, volume.unique_guid, "distinct partitions");
    }

    /// The ESP is a FAT32 volume firmware will mount: signature, the label, and
    /// a `total_sectors` that matches the partition the table declares.
    #[test]
    fn the_esp_is_a_fat32_volume_of_the_partitions_size() {
        let scratch = Scratch::disk(DISK);
        run_layout(&scratch.path, &mut Vec::new()).unwrap();
        let p = plan(512, DISK).unwrap();

        let boot = scratch.read_at(p.esp_offset().unwrap(), 512);
        assert_eq!(boot.get(510..512).unwrap(), &[0x55, 0xaa], "boot signature");
        assert_eq!(
            boot.get(0x47..0x52).unwrap(),
            b"TD-ESP     ",
            "the label is space padded to 11"
        );
        let total = u32::from_le_bytes(boot.get(0x20..0x24).unwrap().try_into().unwrap());
        assert_eq!(
            u64::from(total),
            p.esp_sectors().unwrap(),
            "the volume fills its partition"
        );
        let hidden = u32::from_le_bytes(boot.get(0x1c..0x20).unwrap().try_into().unwrap());
        assert_eq!(u64::from(hidden), p.esp_start, "HiddSec is the start LBA");
    }

    /// `fat.rs` requires zeroed space and cannot check it. A destination whose
    /// ESP already holds a filesystem must come out as though it never did:
    /// stale bytes in the FAT read as ALLOCATED clusters, which is a volume
    /// whose free count disagrees with its own table.
    #[test]
    fn a_destination_with_a_stale_filesystem_is_zeroed_before_it_is_formatted() {
        let scratch = Scratch::disk(DISK);
        let p = plan(512, DISK).unwrap();
        let at = p.esp_offset().unwrap();
        {
            let mut file = OpenOptions::new().write(true).open(&scratch.path).unwrap();
            // Every byte of the metadata region set, which is the worst case:
            // every FAT slot reads as a cluster chain that goes nowhere.
            file.seek(SeekFrom::Start(at)).unwrap();
            file.write_all(&vec![0xffu8; 8 * MIB as usize]).unwrap();
        }
        run_layout(&scratch.path, &mut Vec::new()).unwrap();

        let esp = fat::build(&fat::Volume {
            bytes_per_sector: 512,
            total_sectors: p.esp_sectors().unwrap(),
            hidden_sectors: p.esp_start as u32,
            volume_id: 0,
            label: protocol::ESP_VOLUME_LABEL.to_string(),
            sectors_per_cluster: None,
            root: Vec::new(),
        })
        .unwrap();
        let fat_bytes = u64::from(esp.sectors_per_fat) * 512;
        let first_fat = at + u64::from(fat::RESERVED_SECTORS) * 512;

        // BOTH copies, each past its own live prefix — an empty volume's FAT is
        // three entries long (media, end-of-chain, the root's), and every slot
        // after them must read as FREE. Checking one copy would miss the second,
        // which is the one a repair tool falls back to.
        // The live prefix is whatever `fat::build` emitted for this FAT, not a
        // literal: everything past it must read as free.
        for copy in 0..u64::from(fat::NUM_FATS) {
            let start = first_fat + copy * fat_bytes;
            let rel = start - at;
            let live = esp
                .extents
                .iter()
                .filter(|e| e.offset == rel)
                .map(|e| e.bytes.len() as u64)
                .max()
                .unwrap();
            let tail = scratch.read_at(start + live, usize::try_from(fat_bytes - live).unwrap());
            assert!(
                tail.iter().all(|byte| *byte == 0),
                "FAT copy {copy} still holds stale bytes past its live prefix"
            );
        }
        // The root directory's cluster is WRITTEN whole rather than merely
        // zeroed — an empty labelled volume still has one entry, the label — so
        // what it must equal is the extent, not zero. A stale entry surviving
        // here would be a file firmware lists on a volume that never had one.
        let root = first_fat + u64::from(fat::NUM_FATS) * fat_bytes;
        let expected = esp
            .extents
            .iter()
            .filter(|e| e.offset + at == root)
            .map(|e| e.bytes.as_ref())
            .next()
            .unwrap();
        assert_eq!(
            scratch.read_at(root, expected.len()),
            expected,
            "the root directory cluster is not what fat::build laid down"
        );
    }

    /// A REINSTALL replaces the table rather than layering on it, and the old
    /// one is gone before the ESP under it is touched.
    ///
    /// The second half is what the ordering is for and cannot be observed from
    /// the finished disk, so it is checked where it IS observable: after
    /// invalidation the destination carries no table at all, which is what a
    /// failed install would leave behind. A disk carrying a stale table over a
    /// half-written filesystem is worse, because firmware tries it.
    #[test]
    fn a_reinstall_clears_the_old_table_before_the_esp_beneath_it() {
        let scratch = Scratch::disk(DISK);
        run_layout(&scratch.path, &mut Vec::new()).unwrap();
        let first = scratch.table(DISK);

        {
            let mut file = OpenOptions::new().write(true).open(&scratch.path).unwrap();
            let p = plan(512, DISK).unwrap();
            let image = gpt::build(&gpt::Layout {
                sector_size: 512,
                disk_sectors: p.disk_sectors,
                disk_guid: random_guid().unwrap(),
                align_sectors: 2048,
                partitions: Vec::new(),
            })
            .unwrap();
            invalidate_table(&mut file, &image).unwrap();
        }
        let primary = scratch.read_at(0, 34 * 512);
        assert!(
            primary.iter().all(|byte| *byte == 0),
            "an invalidated disk still carries a header or a protective MBR"
        );
        assert!(
            gpt::parse(&primary, &scratch.read_at(DISK - 33 * 512, 33 * 512), 512).is_err(),
            "an invalidated disk must not parse as partitioned"
        );

        run_layout(&scratch.path, &mut Vec::new()).unwrap();
        let second = scratch.table(DISK);
        assert_eq!(second.partitions.len(), 2, "the reinstall wrote a table");
        assert_ne!(
            first.disk_guid, second.disk_guid,
            "a reinstall is a new disk identity, not the old one recovered"
        );
    }

    /// Two installs of the same size differ in their GUIDs. A fixed one would
    /// have every td disk claim the same identity, which udev and firmware each
    /// resolve by picking one.
    #[test]
    fn each_install_gets_its_own_guids() {
        let first = Scratch::disk(DISK);
        let second = Scratch::disk(DISK);
        run_layout(&first.path, &mut Vec::new()).unwrap();
        run_layout(&second.path, &mut Vec::new()).unwrap();

        let (a, b) = (first.table(DISK), second.table(DISK));
        assert_ne!(a.disk_guid, b.disk_guid);
        assert_ne!(
            a.partitions.first().unwrap().unique_guid,
            b.partitions.first().unwrap().unique_guid
        );
    }

    /// A GUID from `/dev/urandom` still has to BE a GUID: RFC 4122 version 4
    /// and the variant bits, which is what a reader uses to tell a random one
    /// from a structured one.
    #[test]
    fn a_generated_guid_carries_version_four_and_the_variant() {
        for _ in 0..8 {
            let guid = random_guid().unwrap();
            assert_eq!(guid.0.get(7).unwrap() & 0xf0, 0x40, "version 4");
            assert_eq!(guid.0.get(8).unwrap() & 0xc0, 0x80, "RFC 4122 variant");
        }
    }

    /// A regular file has no logical block size to ask for, and must not be
    /// asked: the sysfs path for a regular file's device number is the
    /// FILESYSTEM's, whose block size is not the sector size of anything.
    /// The interleaved device encoding, against glibc's own masks.
    ///
    /// The extended-major cases are the point. Shifting the extended minor down
    /// puts the extended major directly above it, so a mask that only clears
    /// the low bits carries it along — which is what this did, and what stays
    /// invisible while every block major is under 4096.
    #[test]
    fn device_numbers_match_the_sysmacros_encoding() {
        /// `makedev`: major over bits 8..=19 and 44..=63, minor over 0..=7 and
        /// 20..=43. Written out here rather than reusing the decoder, so the
        /// test does not agree with the code by construction.
        fn makedev(major: u64, minor: u64) -> u64 {
            ((major & 0xfff) << 8) | ((major >> 12) << 44) | (minor & 0xff) | ((minor >> 8) << 20)
        }

        for (major, minor) in [
            (8, 0),                // /dev/sda
            (8, 1),                // /dev/sda1
            (259, 5),              // an NVMe namespace, major past one byte
            (0xfff, 0xff_ffff),    // every bit of both low fields
            (0x1000, 3),           // the first EXTENDED major
            (0xf_ffff, 0xff_ffff), // every bit of both
            (0x1234, 0x9_abcd),
        ] {
            let rdev = makedev(major, minor);
            assert_eq!(
                device_numbers(rdev),
                (major, minor),
                "rdev {rdev:#x} decodes wrong"
            );
        }
    }

    #[test]
    fn a_regular_file_takes_the_default_sector_size() {
        let scratch = Scratch::disk(DISK);
        let file = File::open(&scratch.path).unwrap();
        assert_eq!(logical_sector_size(&file).unwrap(), FILE_SECTOR_BYTES);
    }

    #[test]
    fn a_destinations_size_is_asked_of_the_destination() {
        let scratch = Scratch::disk(3 * GIB);
        let mut file = File::open(&scratch.path).unwrap();
        assert_eq!(destination_bytes(&mut file).unwrap(), 3 * GIB);
        assert_eq!(
            file.stream_position().unwrap(),
            0,
            "the descriptor is left where it was found"
        );
    }

    /// The metadata region is what the zeroing covers, and it must be the
    /// reserved sectors plus BOTH FATs plus the root cluster — a region short
    /// by one FAT leaves the second copy holding whatever was there.
    #[test]
    fn the_metadata_region_covers_both_fats_and_the_root_cluster() {
        let p = plan(512, DISK).unwrap();
        let esp = fat::build(&fat::Volume {
            bytes_per_sector: 512,
            total_sectors: p.esp_sectors().unwrap(),
            hidden_sectors: p.esp_start as u32,
            volume_id: 0,
            label: protocol::ESP_VOLUME_LABEL.to_string(),
            sectors_per_cluster: None,
            root: Vec::new(),
        })
        .unwrap();

        let expected = (u64::from(fat::RESERVED_SECTORS)
            + 2 * u64::from(esp.sectors_per_fat)
            + u64::from(esp.sectors_per_cluster))
            * 512;
        assert_eq!(metadata_bytes(&esp).unwrap(), expected);
        assert_eq!(u64::from(fat::NUM_FATS), 2, "two FATs is what that 2 is");
    }

    #[test]
    fn loop_publication_follows_the_trusted_key_in_format_only() {
        let publish = |tail: &[&str]| {
            let mut all = vec![
                "format",
                "kernel",
                "initrd",
                "disk",
                "/mkfs",
                "scratch",
                "--trusted-key",
                "key",
            ];
            all.extend_from_slice(tail);
            parse_args(args(&all))
        };
        assert_eq!(
            publish(&["--publish", "/td-boot", "/source", "/volume"]).unwrap(),
            Mode::Volume {
                boot: Some(BootFiles {
                    kernel: PathBuf::from("kernel"),
                    initramfs: PathBuf::from("initrd"),
                }),
                uuid: None,
                timezone: None,
                hostname: None,
                username: None,
                destination: PathBuf::from("disk"),
                mkfs: PathBuf::from("/mkfs"),
                scratch: PathBuf::from("scratch"),
                seed: Some(VolumeSeed::Loop(
                    PathBuf::from("key"),
                    LoopPublish {
                        td_boot: PathBuf::from("/td-boot"),
                        deployment: PathBuf::from("/source"),
                        mountpoint: PathBuf::from("/volume"),
                    }
                )),
            }
        );
        for tail in [
            &["--publish", "/td-boot", "/source"][..],
            &["--publish", "/td-boot", "/source", "/volume", "extra"],
            &["--publish", "td-boot", "/source", "/volume"],
            &["--publish", "/td-boot", "source", "/volume"],
            &["--publish", "/td-boot", "/source", "volume"],
            &["--publish", "/td-boot", "/source", "--publish"],
        ] {
            assert!(publish(tail).is_err(), "accepted {tail:?}");
        }
        for bad in [
            vec![
                "volume",
                "disk",
                "/mkfs",
                "scratch",
                "--trusted-key",
                "key",
                "--publish",
                "/td-boot",
                "/source",
                "/volume",
            ],
            vec![
                "format",
                "kernel",
                "initrd",
                "disk",
                "/mkfs",
                "scratch",
                "--publish",
                "/td-boot",
                "/source",
                "/volume",
            ],
            vec![
                "format",
                "kernel",
                "initrd",
                "disk",
                "/mkfs",
                "scratch",
                "--publish",
                "/td-boot",
                "/source",
                "/volume",
                "--trusted-key",
                "key",
            ],
        ] {
            assert!(parse_args(args(&bad)).is_err(), "accepted {bad:?}");
        }
    }

    /// The report says the disk is published, so a publication that fails
    /// writes none, whether the loop cannot be bound (no privilege here) or
    /// td-boot refuses on it.
    #[test]
    fn loop_publication_reports_nothing_until_it_publishes() {
        let disk = Scratch::disk(DISK);
        let dir = ScratchDirectory(fake_mkfs(RECORDING_MKFS));
        let boot = BootFiles {
            kernel: dir.0.join("kernel"),
            initramfs: dir.0.join("initrd"),
        };
        std::fs::write(&boot.kernel, b"kernel").unwrap();
        std::fs::write(&boot.initramfs, b"initrd").unwrap();
        let td_boot = dir.0.join("td-boot");
        std::fs::write(&td_boot, "#!/bin/sh\nexit 1\n").unwrap();
        std::fs::set_permissions(
            &td_boot,
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();
        let mountpoint = dir.0.join("volume");
        std::fs::create_dir(&mountpoint).unwrap();
        let seed = VolumeSeed::Loop(
            key_file(&dir.0),
            LoopPublish {
                td_boot,
                deployment: dir.0.clone(),
                mountpoint,
            },
        );
        let mkfs = dir.0.join("mkfs.btrfs");
        let prepared =
            prepare_volume(VolumeSettings::default(), None, &mkfs, &dir.0, Some(&seed)).unwrap();
        let mut output = Vec::new();
        let error = run_format(prepared, &disk.path, &boot, &mut output).unwrap_err();
        assert!(
            error.to_string().contains("loop over the volume")
                || error.to_string().contains("install on /dev/loop"),
            "{error}"
        );
        assert!(output.is_empty(), "reported before publication");
        // The volume itself was written before publication was attempted.
        assert!(destination_volume(&mut File::open(&disk.path).unwrap()).is_ok());
    }

    /// td-boot takes the deployment and mountpoint in the other order, so a
    /// swap is refused before the destination is opened, not after layout.
    #[test]
    fn loop_publication_refuses_a_misplaced_operand_before_any_write() {
        let dir = ScratchDirectory(fake_mkfs(RECORDING_MKFS));
        let empty = dir.0.join("volume");
        std::fs::create_dir(&empty).unwrap();
        let file = dir.0.join("file");
        std::fs::write(&file, b"x").unwrap();
        let mkfs = dir.0.join("mkfs.btrfs");
        for (deployment, mountpoint, refusal) in [
            (&file, &empty, "is not a directory"),
            (&dir.0, &dir.0, "is not an empty directory"),
            (&dir.0, &file, "is not an empty directory"),
            (&dir.0, &dir.0.join("absent"), "is not an empty directory"),
        ] {
            let seed = VolumeSeed::Loop(
                key_file(&dir.0),
                LoopPublish {
                    td_boot: PathBuf::from("/td-boot"),
                    deployment: deployment.clone(),
                    mountpoint: mountpoint.clone(),
                },
            );
            let error = prepare_volume(VolumeSettings::default(), None, &mkfs, &dir.0, Some(&seed))
                .err()
                .unwrap();
            assert!(error.to_string().contains(refusal), "{error}");
        }
    }

    // ── The consented installation's execution ────────────────────────────

    /// Everything a `LiveExecution` reads, on a sparse disk.
    struct ExecutionFixture {
        _dir: ScratchDirectory,
        disk: Scratch,
        execution: LiveExecution,
        plan: installation_plan::Plan,
    }

    impl ExecutionFixture {
        /// `kernel` is what the source holds; the manifest names `named`.
        fn new(kernel: &[u8], named: &[u8]) -> Self {
            Self::reviewed_at(kernel, named, DISK)
        }

        /// The same, with the review having observed `capacity` bytes.
        fn reviewed_at(kernel: &[u8], named: &[u8], capacity: u64) -> Self {
            let dir = ScratchDirectory(fake_mkfs(RECORDING_MKFS));
            let root = dir.0.join("root");
            std::fs::create_dir_all(root.join("bin")).unwrap();
            std::fs::rename(dir.0.join("mkfs.btrfs"), root.join("bin/mkfs.btrfs")).unwrap();
            let template = root.join(protocol::SELECTOR_TEMPLATE_PATH);
            std::fs::create_dir_all(template.parent().unwrap()).unwrap();
            std::fs::write(&template, b"stock selector").unwrap();
            let firstboot = dir.0.join("td-firstboot");
            scratch::executable(&firstboot, "#!/bin/sh\nexit 0\n").unwrap();
            let td_boot = dir.0.join("td-boot");
            scratch::executable(&td_boot, "#!/bin/sh\nexit 1\n").unwrap();
            let zones = dir.0.join("zoneinfo");
            std::fs::create_dir_all(zones.join("Etc")).unwrap();
            std::fs::write(zones.join("iso3166.tab"), b"GB\tBritain\n").unwrap();
            std::fs::write(
                zones.join("zone1970.tab"),
                b"GB\t+5130-00007\tEurope/London\n",
            )
            .unwrap();
            let mut header = vec![0; 44];
            header[..5].copy_from_slice(b"TZif2");
            std::fs::create_dir(zones.join("Europe")).unwrap();
            for id in ["Europe/London", "Etc/UTC"] {
                std::fs::write(zones.join(id), &header).unwrap();
            }
            let source = dir.0.join("source");
            std::fs::create_dir(&source).unwrap();
            std::fs::write(source.join("bzImage"), kernel).unwrap();
            std::fs::write(source.join("initramfs.cpio"), b"initramfs").unwrap();
            std::fs::write(source.join("root.erofs"), b"root").unwrap();
            let manifest = format!(
                "td-deployment-v1\n{}  bzImage\n{}  initramfs.cpio\n{}  root.erofs\n",
                sha256::hex_digest(named),
                "11".repeat(32),
                "22".repeat(32)
            );
            std::fs::write(source.join("manifest"), &manifest).unwrap();
            let mut deployment = [0; 32];
            let mut hasher = sha256::Sha256::new();
            hasher.update(manifest.as_bytes());
            deployment.copy_from_slice(&hasher.finalize());
            let booted = dir.0.join("td-deployment");
            std::fs::write(&booted, format!("{}\n", sha256::to_base16(&deployment))).unwrap();
            let run = dir.0.join("run");
            std::fs::create_dir(&run).unwrap();
            let execution = LiveExecution {
                td_boot,
                source,
                trusted_key: key_file(&dir.0),
                root,
                firstboot,
                timezones: zones,
                booted,
                run,
            };
            let destination =
                installation_plan::Destination::new(installation_plan::DestinationObservation {
                    name: "vda",
                    major: 253,
                    minor: 0,
                    sequence: 1,
                    capacity,
                    sector: 512,
                    removable: false,
                    model: None,
                    serial: None,
                    wwid: None,
                })
                .unwrap();
            let settings =
                installation_plan::Settings::new("tester", "td", "us", "Etc/UTC").unwrap();
            let mut uuid = [0x5a; 16];
            uuid[6] = 0x40;
            uuid[8] = 0x80;
            let plan =
                installation_plan::Plan::new([7; 32], destination, deployment, uuid, settings)
                    .unwrap();
            Self {
                _dir: dir,
                disk: Scratch::disk(DISK),
                execution,
                plan,
            }
        }

        /// The outcome and every phase reported, run on the held disk.
        fn run(
            &self,
        ) -> (
            Result<(), installation_protocol::Failure>,
            Vec<installation_protocol::Phase>,
        ) {
            use installation_service::Execute;
            let claim = OpenOptions::new()
                .read(true)
                .write(true)
                .open(&self.disk.path)
                .unwrap();
            let mut phases = Vec::new();
            let outcome = self
                .execution
                .execute(&self.plan, claim, &mut |phase| phases.push(phase));
            (outcome, phases)
        }

        /// Nothing left in the run directory, whatever the outcome.
        fn assert_workspace_gone(&self) {
            assert_eq!(
                std::fs::read_dir(&self.execution.run).unwrap().count(),
                0,
                "the workspace outlived its execution"
            );
        }

        /// Stopped with `failure` before any phase began and any byte of the
        /// disk was written.
        fn assert_untouched(self, failure: installation_protocol::Failure) {
            let (outcome, phases) = self.run();
            assert_eq!(outcome, Err(failure));
            assert!(phases.is_empty(), "{phases:?}");
            assert!(self.disk.read_at(0, 1 << 20).iter().all(|b| *b == 0));
            assert!(self
                .disk
                .read_at(DISK - (1 << 20), 1 << 20)
                .iter()
                .all(|b| *b == 0));
            self.assert_workspace_gone();
        }
    }

    /// The source is checked before the destination is touched: a root that
    /// is not the planned deployment, a manifest that is not the planned one,
    /// and a kernel the manifest does not name each fail verification with
    /// no phase begun and every byte of the disk still zero.
    #[test]
    fn execution_verifies_its_source_before_any_write() {
        use installation_protocol::Failure;
        let another_root = ExecutionFixture::new(b"kernel", b"kernel");
        std::fs::write(
            &another_root.execution.booted,
            format!("{}\n", "cd".repeat(32)),
        )
        .unwrap();
        another_root.assert_untouched(Failure::VerificationFailed);
        // Well formed and naming this kernel, but not the plan's manifest:
        // only its digest tells it apart.
        let another_manifest = ExecutionFixture::new(b"kernel", b"kernel");
        std::fs::write(
            another_manifest.execution.source.join("manifest"),
            format!(
                "td-deployment-v1\n{}  bzImage\n{}  initramfs.cpio\n{}  root.erofs\n",
                sha256::hex_digest(b"kernel"),
                "33".repeat(32),
                "22".repeat(32)
            ),
        )
        .unwrap();
        another_manifest.assert_untouched(Failure::VerificationFailed);
        // The manifest is the plan's; the kernel beside it is not its kernel.
        ExecutionFixture::new(b"another kernel", b"kernel")
            .assert_untouched(Failure::VerificationFailed);
        // td-boot's source naming: no kernel, a link, or two files.
        let none = ExecutionFixture::new(b"kernel", b"kernel");
        std::fs::remove_file(none.execution.source.join("bzImage")).unwrap();
        none.assert_untouched(Failure::VerificationFailed);
        let linked = ExecutionFixture::new(b"kernel", b"kernel");
        let source = linked.execution.source.clone();
        std::fs::rename(source.join("bzImage"), source.join("real")).unwrap();
        std::os::unix::fs::symlink(source.join("real"), source.join("bzImage")).unwrap();
        linked.assert_untouched(Failure::VerificationFailed);
        let both = ExecutionFixture::new(b"kernel", b"kernel");
        std::fs::write(both.execution.source.join("bzimage"), b"kernel").unwrap();
        both.assert_untouched(Failure::VerificationFailed);
    }

    /// The other refusals before any write, each by its own failure.
    #[test]
    fn execution_maps_each_refusal_before_writing() {
        use installation_protocol::Failure;
        // The plan's zone is gone from the catalog it is loaded from.
        let zone = ExecutionFixture::new(b"kernel", b"kernel");
        std::fs::remove_file(zone.execution.timezones.join("Etc/UTC")).unwrap();
        zone.assert_untouched(Failure::SettingsFailed);
        // A disk the layout cannot hold.
        ExecutionFixture::reviewed_at(b"kernel", b"kernel", 1 << 20)
            .assert_untouched(Failure::InsufficientSpace);
        // A deployment grown since the review, which the volume cannot hold.
        let grown = ExecutionFixture::new(b"kernel", b"kernel");
        File::create(grown.execution.source.join("root.erofs"))
            .unwrap()
            .set_len(5 * GIB)
            .unwrap();
        grown.assert_untouched(Failure::InsufficientSpace);
        // A workspace of the plan's name is already there, and is not this
        // execution's to remove.
        let taken = ExecutionFixture::new(b"kernel", b"kernel");
        let name = taken.execution.run.join("td-install-0707070707070707");
        std::fs::create_dir(&name).unwrap();
        let (outcome, phases) = taken.run();
        assert_eq!(outcome, Err(Failure::InsufficientSpace));
        assert!(phases.is_empty());
        assert!(name.is_dir());
        assert!(taken.disk.read_at(0, 1 << 20).iter().all(|b| *b == 0));
    }

    /// A source that verifies is written, with each phase reported as it
    /// begins, and publication through the loop then fails here (unprivileged,
    /// or a td-boot stand-in that exits 1): a write failure, the disk laid
    /// out, and the workspace gone.
    #[test]
    fn execution_reports_each_phase_and_a_failed_publication() {
        use installation_protocol::{Failure, Phase};
        // Under either spelling td-boot reads a source's kernel by.
        for spelling in ["bzImage", "bzimage"] {
            let fixture = ExecutionFixture::new(b"kernel", b"kernel");
            let source = &fixture.execution.source;
            std::fs::rename(source.join("bzImage"), source.join(spelling)).unwrap();
            let (outcome, phases) = fixture.run();
            assert_eq!(outcome, Err(Failure::WriteFailed));
            assert_eq!(
                phases,
                [Phase::WritingFilesystems, Phase::PublishingDeployment]
            );
            let table = fixture.disk.table(DISK);
            assert_eq!(table.partitions.len(), 2);
            fixture.assert_workspace_gone();
        }
    }

    #[test]
    fn a_workspace_is_made_fresh_and_removed_whole() {
        let dir = ScratchDirectory(scratch::path("workspace"));
        std::fs::create_dir(&dir.0).unwrap();
        let workspace = Workspace::create(&dir.0, &[0xa5; 32]).unwrap();
        assert_eq!(workspace.dir, dir.0.join("td-install-a5a5a5a5a5a5a5a5"));
        assert_eq!(workspace.kernel, workspace.dir.join("bzImage"));
        use std::os::unix::fs::PermissionsExt;
        for path in [&workspace.dir, &workspace.scratch, &workspace.mountpoint] {
            let mode = std::fs::metadata(path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700, "{}", path.display());
        }
        assert!(Workspace::create(&dir.0, &[0xa5; 32]).is_err());
        std::fs::write(&workspace.kernel, b"kernel").unwrap();
        std::fs::write(&workspace.selector, b"selector").unwrap();
        std::fs::write(workspace.scratch.join("image"), b"image").unwrap();
        workspace.remove().unwrap();
        assert_eq!(std::fs::read_dir(&dir.0).unwrap().count(), 0);
        // Dropped without `remove`, as an unwinding panic drops it.
        let workspace = Workspace::create(&dir.0, &[0x11; 32]).unwrap();
        std::fs::write(workspace.scratch.join("image"), b"image").unwrap();
        drop(workspace);
        assert_eq!(std::fs::read_dir(&dir.0).unwrap().count(), 0);
        // A mountpoint that is not empty is a volume left mounted: refused,
        // never walked.
        let workspace = Workspace::create(&dir.0, &[0x5a; 32]).unwrap();
        std::fs::write(workspace.mountpoint.join("installed"), b"x").unwrap();
        let mountpoint = workspace.mountpoint.clone();
        let scratch = workspace.scratch.clone();
        assert!(workspace.remove().is_err());
        assert!(mountpoint.join("installed").exists());
        // ...and the parts beside it are still removed.
        assert!(!scratch.exists());
        // The first part refused does not stop the ones after it.
        let workspace = Workspace::create(&dir.0, &[0x3c; 32]).unwrap();
        std::fs::create_dir(&workspace.kernel).unwrap();
        std::fs::write(workspace.kernel.join("held"), b"x").unwrap();
        std::fs::write(&workspace.selector, b"selector").unwrap();
        let (selector, scratch) = (workspace.selector.clone(), workspace.scratch.clone());
        assert!(workspace.remove().is_err());
        assert!(!selector.exists() && !scratch.exists());
    }

    /// The disk a review presents must hold what installing writes: the
    /// kernel and selector in the ESP, one copy of the deployment and a GiB
    /// in the volume. The source's sizes are its payloads' own, under
    /// td-boot's naming; what no disk could hold is the source's fault.
    #[test]
    fn a_review_fits_its_deployment_to_its_disk() {
        use installation_protocol::Refusal;
        use installation_service::Host;
        let fixture = ExecutionFixture::new(b"kernel", b"kernel");
        let source = fixture.execution.source.clone();
        let sized = |path: &Path, bytes: u64| File::create(path).unwrap().set_len(bytes).unwrap();
        // The stock deployment's sizes, on the stock 6 GiB disk.
        sized(&source.join("bzImage"), 6_337_536);
        sized(&source.join("initramfs.cpio"), 9_629_184);
        sized(&source.join("root.erofs"), 3_005_456_384);
        let execution = &fixture.execution;
        let mut host = LiveHost {
            td_boot: execution.td_boot.clone(),
            source: source.clone(),
            trusted_key: execution.trusted_key.clone(),
            root: execution.root.clone(),
            firstboot: execution.firstboot.clone(),
            timezones: execution.timezones.clone(),
            catalog: None,
            booted: execution.booted.clone(),
        };
        let fit = |host: &mut LiveHost| host.check_fit(&fixture.plan);
        assert_eq!(fit(&mut host), Ok(()));
        // The medium's spelling of the kernel is the same payload; both
        // spellings as two files are refused.
        std::fs::rename(source.join("bzImage"), source.join("bzimage")).unwrap();
        assert_eq!(fit(&mut host), Ok(()));
        sized(&source.join("bzImage"), 6_337_536);
        assert_eq!(fit(&mut host), Err(Refusal::SourceUnavailable));
        std::fs::remove_file(source.join("bzimage")).unwrap();
        // The volume: about 5.5 GiB holds 3 GB and a GiB, not 5 GiB.
        sized(&source.join("root.erofs"), 5 * GIB);
        assert_eq!(fit(&mut host), Err(Refusal::InsufficientSpace));
        sized(&source.join("root.erofs"), 3_005_456_384);
        // The initramfs is a volume payload, not the ESP's: the selector is.
        sized(&source.join("initramfs.cpio"), 500 << 20);
        assert_eq!(fit(&mut host), Ok(()));
        sized(&source.join("initramfs.cpio"), 9_629_184);
        // The kernel's bound is the execution's, either side of it.
        sized(&source.join("bzImage"), MAX_BOOT_FILE);
        assert_eq!(fit(&mut host), Ok(()));
        for bytes in [0, MAX_BOOT_FILE + 1] {
            sized(&source.join("bzImage"), bytes);
            assert_eq!(fit(&mut host), Err(Refusal::SourceUnavailable), "{bytes}");
        }
        // A kernel and selector each in bound but together more than the
        // fixed ESP holds: no disk is bigger there.
        sized(&source.join("bzImage"), MAX_BOOT_FILE);
        let template = execution.root.join(protocol::SELECTOR_TEMPLATE_PATH);
        let original = std::fs::read(&template).unwrap();
        sized(&template, MAX_BOOT_FILE - 4096);
        assert_eq!(fit(&mut host), Err(Refusal::SourceUnavailable));
        std::fs::write(&template, &original).unwrap();
        sized(&source.join("bzImage"), 6_337_536);
        assert_eq!(fit(&mut host), Ok(()));
        // A payload that is missing, or not a regular file, is the source's
        // fault, not the disk's.
        std::fs::remove_file(source.join("initramfs.cpio")).unwrap();
        assert_eq!(fit(&mut host), Err(Refusal::SourceUnavailable));
        std::fs::create_dir(source.join("initramfs.cpio")).unwrap();
        assert_eq!(fit(&mut host), Err(Refusal::SourceUnavailable));
        std::fs::remove_dir(source.join("initramfs.cpio")).unwrap();
        sized(&source.join("initramfs.cpio"), 9_629_184);
        // So is a key or selector template the root does not hold.
        let key = std::fs::read(&execution.trusted_key).unwrap();
        std::fs::remove_file(&execution.trusted_key).unwrap();
        assert_eq!(fit(&mut host), Err(Refusal::SourceUnavailable));
        std::fs::write(&execution.trusted_key, &key).unwrap();
        std::fs::remove_file(&template).unwrap();
        assert_eq!(fit(&mut host), Err(Refusal::SourceUnavailable));
    }

    /// The fit's arithmetic at its edges: the volume needs exactly one copy
    /// and a GiB, and the selector's length is the one prepared.
    #[test]
    fn the_fit_is_one_copy_and_a_gib_and_the_prepared_selector() {
        let volume = plan(512, DISK).unwrap().volume_bytes().unwrap();
        let most = volume - GIB;
        assert_eq!(volume_fit(512, DISK, most), Ok(()));
        assert!(volume_fit(512, DISK, most + 1).is_err());
        assert!(volume_fit(512, DISK, u64::MAX).is_err());
        // A disk the layout refuses is refused as such.
        assert!(volume_fit(512, 1 << 20, 1).is_err());
        assert_eq!(esp_fit(512, DISK, 1 << 20, 1 << 20), Ok(()));
        assert!(esp_fit(512, DISK, protocol::ESP_BYTES, 1).is_err());
        assert!(esp_fit(512, DISK, 1, protocol::ESP_BYTES).is_err());

        let fixture = ExecutionFixture::new(b"kernel", b"kernel");
        let execution = &fixture.execution;
        let template = execution.root.join(protocol::SELECTOR_TEMPLATE_PATH);
        let uuid = VolumeUuid::parse(&canonical_uuid(fixture.plan.volume_uuid())).unwrap();
        let total = selector_parts(&template, &execution.trusted_key, &uuid)
            .unwrap()
            .total()
            .unwrap();
        let output = execution.run.join("selector.cpio");
        prepare_selector(&template, &execution.trusted_key, &uuid, &output).unwrap();
        assert_eq!(std::fs::metadata(&output).unwrap().len(), total);
    }

    /// Proposal and execution both require the running root to be the
    /// deployment they install, by the record init wrote.
    #[test]
    fn the_booted_record_names_exactly_one_deployment() {
        let dir = ScratchDirectory(scratch::path("booted"));
        std::fs::create_dir(&dir.0).unwrap();
        let record = dir.0.join("td-deployment");
        let id = "ab".repeat(32);
        assert!(booted_as(&record, &id).is_err());
        std::fs::write(&record, format!("{id}\n")).unwrap();
        booted_as(&record, &id).unwrap();
        assert!(booted_as(&record, &"cd".repeat(32)).is_err());
        for written in [id.clone(), format!("{id}\n\n"), format!("{id} \n")] {
            std::fs::write(&record, written.as_bytes()).unwrap();
            assert!(booted_as(&record, &id).is_err(), "{written:?}");
        }
    }

    /// A source td-boot authenticates is admitted only as the deployment
    /// the running root is.
    #[test]
    fn a_source_is_admitted_only_as_the_booted_deployment() {
        use installation_protocol::Refusal;
        use installation_service::Host;
        let fixture = ExecutionFixture::new(b"kernel", b"kernel");
        let execution = &fixture.execution;
        let id = sha256::to_base16(fixture.plan.deployment());
        let validator = execution.run.join("td-boot-validator");
        scratch::executable(&validator, &format!("#!/bin/sh\necho {id}\n")).unwrap();
        let mut host = LiveHost {
            td_boot: validator,
            source: execution.source.clone(),
            trusted_key: execution.trusted_key.clone(),
            root: execution.root.clone(),
            firstboot: execution.firstboot.clone(),
            timezones: execution.timezones.clone(),
            catalog: None,
            booted: execution.booted.clone(),
        };
        assert_eq!(host.authenticate_source(), Ok(*fixture.plan.deployment()));
        std::fs::write(&execution.booted, format!("{}\n", "cd".repeat(32))).unwrap();
        assert_eq!(host.authenticate_source(), Err(Refusal::SourceUnavailable));
        std::fs::remove_file(&execution.booted).unwrap();
        assert_eq!(host.authenticate_source(), Err(Refusal::SourceUnavailable));
    }

    /// A source the kernel's block inventory cannot place, as a scratch
    /// directory on tmpfs is, refuses discovery rather than listing every
    /// disk. Scratch may be disk-backed, and unprivileged the claim probe
    /// refuses discovery too, so this pins only the outcome; `disk_of`'s
    /// test pins the resolution.
    #[test]
    fn an_unresolvable_source_refuses_discovery() {
        use installation_protocol::Refusal;
        use installation_service::Host;
        let fixture = ExecutionFixture::new(b"kernel", b"kernel");
        let execution = &fixture.execution;
        let mut host = LiveHost {
            td_boot: execution.td_boot.clone(),
            source: execution.source.clone(),
            trusted_key: execution.trusted_key.clone(),
            root: execution.root.clone(),
            firstboot: execution.firstboot.clone(),
            timezones: execution.timezones.clone(),
            catalog: None,
            booted: execution.booted.clone(),
        };
        assert_eq!(host.candidates(), Err(Refusal::DiscoveryFailed));
    }

    /// The source's disk leaves the candidates and nothing else does.
    #[test]
    fn the_source_disk_is_never_a_candidate() {
        let disk = |name: &'static str, minor: u32| {
            installation_plan::Destination::new(installation_plan::DestinationObservation {
                name,
                major: 8,
                minor,
                sequence: u64::from(minor) + 1,
                capacity: DISK,
                sector: 512,
                removable: false,
                model: None,
                serial: None,
                wwid: None,
            })
            .unwrap()
        };
        let both =
            || installation_plan::Candidates::new(vec![disk("sda", 0), disk("sdb", 16)]).unwrap();
        assert_eq!(
            excluding(both(), "sdb").unwrap().as_slice(),
            [disk("sda", 0)]
        );
        assert_eq!(
            excluding(both(), "sr0").unwrap().as_slice(),
            both().as_slice()
        );
    }

    #[test]
    fn a_plan_uuid_and_a_published_id_read_canonically() {
        let mut bytes = [0u8; 16];
        for (at, byte) in bytes.iter_mut().enumerate() {
            *byte = at as u8 * 17;
        }
        assert_eq!(
            canonical_uuid(&bytes),
            "00112233-4455-6677-8899-aabbccddeeff"
        );
        let id = "ab".repeat(32);
        assert_eq!(published_id(format!("{id}\n").as_bytes()), Some(id.clone()));
        for wrong in [
            id.clone(),
            format!("{id}\n\n"),
            format!("{}\n", "AB".repeat(32)),
            format!("{}\n", "ab".repeat(31)),
            String::new(),
        ] {
            assert_eq!(published_id(wrong.as_bytes()), None, "{wrong:?}");
        }
    }

    // ── The unsafe surface (UNSAFE.md §21) ─────────────────────────────────
    //
    // The compiler checks that `unsafe` appears only where an allow permits
    // it; it cannot check that there is ONE allow, that the assembly body and
    // the two requests are the reviewed ones, or that only `loop_device.rs`
    // reaches them. These do, over every file this binary compiles, which
    // `every_compiled_file_is_one_the_guards_read` keeps complete. Names are
    // assembled with `concat!` so this file does not match itself; the scans
    // read code with its strings emptied.

    /// A compiled file's code: comments and string contents gone.
    fn plain_of(label: &str) -> String {
        let Some((_, text, _)) = compiled_files()
            .into_iter()
            .find(|(name, _, _)| *name == label)
        else {
            panic!("{label} is not compiled into this binary")
        };
        let Some(plain) = plain_source(text) else {
            panic!("{label} cannot be read as plain code")
        };
        plain
    }

    /// The same, whitespace gone too.
    fn code_of(label: &str) -> String {
        unspaced(&plain_of(label))
    }

    /// Occurrences of `word` as a whole identifier.
    fn words(code: &str, word: &str) -> usize {
        let ident = |ch: char| ch.is_alphanumeric() || ch == '_';
        code.match_indices(word)
            .filter(|(at, _)| {
                let before = code.get(..*at).and_then(|head| head.chars().next_back());
                let after = code
                    .get(at.saturating_add(word.len())..)
                    .and_then(|tail| tail.chars().next());
                !before.is_some_and(ident) && !after.is_some_and(ident)
            })
            .count()
    }

    const UNSAFE: &str = concat!("un", "safe");
    const LINT: &str = concat!("un", "safe_code");
    const RAW: &str = concat!("sys", "call3");
    const SURFACE: &str = concat!("loop", "_sys");

    #[test]
    fn the_one_unsafe_block_sits_under_the_one_scoped_allow() {
        for (label, _, _) in compiled_files() {
            let code = code_of(label);
            let plain = plain_of(label);
            let (blocks, lints) = match label {
                "loop_sys.rs" => (1, 1),
                "main.rs" => (0, 1),
                _ => (0, 0),
            };
            assert_eq!(
                words(&plain, UNSAFE),
                blocks,
                "{label}: the unsafe keyword appears outside the one block"
            );
            assert_eq!(
                words(&plain, LINT),
                lints,
                "{label}: the unsafe lint is named outside the crate deny and \
                 the one scoped allow"
            );
            for form in [
                concat!("global_a", "sm!"),
                concat!("naked_a", "sm!"),
                concat!("a", "sm!"),
            ] {
                assert_eq!(
                    code.matches(form).count(),
                    usize::from(label == "loop_sys.rs" && form == concat!("a", "sm!")),
                    "{label}: inline assembly outside the one pinned body: {form}"
                );
            }
        }
        let main = code_of("main.rs");
        assert_eq!(main.matches(&format!("#![deny({LINT})]")).count(), 1);
        let sys = code_of("loop_sys.rs");
        assert_eq!(
            sys.matches(&format!("#[inline]#[allow({LINT})]fn{RAW}("))
                .count(),
            1,
            "the allow must sit on the raw entry point and nothing else"
        );
        assert_eq!(
            sys.matches(&format!("{UNSAFE}{{")).count(),
            1,
            "the one unsafe item must be a block"
        );
        // The library compiles none of this and keeps the stronger lint.
        assert!(unspaced(include_str!("lib.rs")).contains(&format!("#![forbid({LINT})]")));
    }

    /// The raw entry point is pinned WHOLE, from its attributes to its last
    /// token: a second instruction in the same `asm!`, `in("rsi")` swapped
    /// with `in("rdx")`, or a rebinding such as `let a2 = a2 ^ 1;` before the
    /// block changes no count above. Read with its strings, which name the
    /// registers. `options(nomem)` is absent by design: the kernel reads the
    /// configuration through the pointer. Both wrappers are pinned whole
    /// too, so nothing between building the configuration and the call can
    /// change a byte of it, such as setting a flag at a literal offset.
    #[test]
    fn the_raw_entry_point_is_pinned_whole() {
        const WRAPPERS: [&str; 2] = [
            concat!(
                "pubfnfree_loop(control:&File)->io::Result<usize>{",
                "constLOOP_CTL_GET_FREE:usize=0x4c82;check(sys",
                "call3(SYS_IOCTL,control.as_raw_fd()asusize,LOOP_CTL_GET_FREE,0,))}"
            ),
            concat!(
                "pubfnconfigure(device:&File,backing:&File,offset:u64,len:u64,",
                "block_size:u32,)->io::Result<()>{",
                "constLOOP_CONFIGURE:usize=0x4c0a;",
                "letconfig=config(backing,offset,len,block_size)?;check(sys",
                "call3(SYS_IOCTL,device.as_raw_fd()asusize,LOOP_CONFIGURE,",
                "config.as_ptr()asusize,)).map(drop)}"
            ),
        ];
        let wrapped = unspaced(&uncommented(include_str!("loop_sys.rs")));
        for wrapper in WRAPPERS {
            assert_eq!(wrapped.matches(wrapper).count(), 1, "re-audit {wrapper}");
        }
        const FUNCTION: &str = concat!(
            "#[inline]#[allow(un",
            "safe_code)]fnsys",
            "call3(n:usize,a1:usize,a2:usize,a3:usize)->isize{letret:isize;",
            "un",
            "safe{core::arch::a",
            "sm!(\"syscall\",inlateout(\"rax\")nasisize=>ret,",
            "in(\"rdi\")a1,in(\"rsi\")a2,in(\"rdx\")a3,",
            "out(\"rcx\")_,out(\"r11\")_,options(nostack),);}ret}"
        );
        let sys = unspaced(&uncommented(include_str!("loop_sys.rs")));
        assert_eq!(sys.matches(FUNCTION).count(), 1, "re-audit the function");
    }

    /// One syscall, two requests and the configuration's layout and flags,
    /// by VALUE: `ioctl(2)`'s number says nothing about what it does, the
    /// request does, and a field offset or flag is what the kernel is told.
    /// Each constant is declared once and used only where counted, so none
    /// can be shadowed or recomputed at its use.
    #[test]
    fn the_syscall_requests_and_layout_are_value_pinned() {
        let sys = code_of("loop_sys.rs");
        let plain = plain_of("loop_sys.rs");
        let pinned = [
            (concat!("SYS", "_IOCTL"), "usize=16", 3),
            (concat!("LOOP_CTL", "_GET_FREE"), "usize=0x4c82", 2),
            (concat!("LOOP", "_CONFIGURE"), "usize=0x4c0a", 2),
            (concat!("LOOP", "_CONFIG_LEN"), "usize=304", 3),
            (concat!("CONFIG", "_FD"), "usize=0", 2),
            (concat!("CONFIG", "_BLOCK_SIZE"), "usize=4", 2),
            (concat!("INFO", "_OFFSET"), "usize=8+24", 2),
            (concat!("INFO", "_SIZELIMIT"), "usize=8+32", 2),
            (concat!("INFO", "_FLAGS"), "usize=8+52", 2),
            (concat!("LO_FLAGS", "_AUTOCLEAR"), "u32=4", 2),
        ];
        for (name, value, mentions) in pinned {
            assert_eq!(
                sys.matches(&format!("const{name}:{value};")).count(),
                1,
                "{name} must be declared once as {value}"
            );
            assert_eq!(words(&plain, name), mentions, "{name} is named elsewhere");
        }
        assert_eq!(
            words(&plain, "const"),
            pinned.len(),
            "an undeclared constant"
        );
        for (label, _, _) in compiled_files() {
            if label != "loop_sys.rs" {
                assert!(
                    !code_of(label).contains(concat!("const", "SYS_")),
                    "{label} declares a syscall number"
                );
            }
        }
    }

    /// Each call site whole, every argument in its register's place, and the
    /// entry point named nowhere but its definition and those two calls.
    #[test]
    fn every_call_site_is_pinned_whole() {
        let sys = unspaced(&uncommented(include_str!("loop_sys.rs")));
        for arguments in [
            "(SYS_IOCTL,control.as_raw_fd()asusize,LOOP_CTL_GET_FREE,0,)",
            "(SYS_IOCTL,device.as_raw_fd()asusize,LOOP_CONFIGURE,config.as_ptr()asusize,)",
        ] {
            assert_eq!(
                sys.matches(&format!("{RAW}{arguments}")).count(),
                1,
                "not the pinned call: {arguments}"
            );
        }
        for (label, _, _) in compiled_files() {
            let expected = if label == "loop_sys.rs" { 3 } else { 0 };
            assert_eq!(
                words(&plain_of(label), RAW),
                expected,
                "{label}: the raw entry point is named outside its definition \
                 and two calls"
            );
        }
        assert_eq!(
            code_of("loop_sys.rs")
                .matches(&format!("pubfn{RAW}("))
                .count(),
            0,
            "module privacy is the entry point's confinement"
        );
    }

    /// Only `loop_device.rs` reaches the wrappers, by one plain import, and
    /// each wrapper is named, as a whole identifier, only at its definition,
    /// its one call there and its test: an alias such as
    /// `let call = free_loop;` is a mention without a parenthesis.
    #[test]
    fn only_the_loop_module_reaches_the_wrappers() {
        for (label, _, _) in compiled_files() {
            let plain = plain_of(label);
            let expected = match label {
                "main.rs" => 1,
                "loop_device.rs" => 3,
                _ => 0,
            };
            assert_eq!(
                words(&plain, SURFACE),
                expected,
                "{label}: the syscall module is named where it should not be"
            );
        }
        assert_eq!(
            code_of("main.rs")
                .matches(&format!("mod{SURFACE};"))
                .count(),
            1
        );
        let caller = code_of("loop_device.rs");
        for form in [
            format!("usecrate::{SURFACE};"),
            format!("{SURFACE}::free_loop(&control)"),
            format!("{SURFACE}::configure(&device,backing,offset,len,blocks)"),
        ] {
            assert_eq!(caller.matches(&form).count(), 1, "{form}");
        }
        for wrapper in [concat!("free", "_loop"), concat!("config", "ure")] {
            for (label, _, _) in compiled_files() {
                let expected = match label {
                    "loop_sys.rs" => 2,
                    "loop_device.rs" => 1,
                    _ => 0,
                };
                assert_eq!(
                    words(&plain_of(label), wrapper),
                    expected,
                    "{label} names {wrapper} beyond its definition, call and test"
                );
            }
            // ...and of loop_sys.rs's two, the production one is the
            // definition.
            let sys = plain_of("loop_sys.rs");
            let production = sys
                .split(concat!("#[cfg(", "test)]"))
                .next()
                .unwrap_or_default();
            assert_eq!(words(production, wrapper), 1, "{wrapper} in production");
        }
        // The configuration is built in one place, from the wrapper's own
        // arguments, with the flags fixed there.
        assert_eq!(
            code_of("loop_sys.rs")
                .matches("letconfig=config(backing,offset,len,block_size)?;")
                .count(),
            1
        );
    }
}

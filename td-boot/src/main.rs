//! td-boot selects a deployment on the persistent volume, preferring `current`
//! and falling back to `previous`, then invokes the confined kexec helper.
//! Every reader checks a deployment's payloads against its manifest, whose
//! hash is the deployment id. Nothing signs a deployment: who may publish
//! one is whoever can write the volume (td-install/DESIGN.md §6, "No
//! deployment signatures").
#![forbid(unsafe_code)]

// The protected tier's admission record, shared with td-authd by its path:
// td-boot writes it, td-authd reads and judges it.
#[path = "admission.rs"]
#[allow(dead_code, reason = "the record's reader and judge are td-authd's")]
mod admission;
// The deployment initramfs's `admit` verb.
#[path = "admit.rs"]
mod admit;
// The staged selector update's digests and prediction
// (td-install/ENCRYPTION.md "Prediction"), which its pre-seal takes.
#[path = "authenticode.rs"]
mod authenticode;
#[path = "cap.rs"]
mod cap;
// engine's newc writer, for the selector appendix below.
#[path = "../../engine/src/boot_info.rs"]
#[allow(dead_code, reason = "the selector reads deployment metadata")]
mod boot_info;
#[path = "../../engine/src/cpio.rs"]
#[allow(dead_code, reason = "the appendix uses its writer alone")]
mod cpio;
#[path = "menu.rs"]
mod menu;
// engine's GPT reader and its CRC-32, through which the staged update
// finds the ESP as td-install does (`gpt::locate`).
#[path = "../../engine/src/crc32.rs"]
#[allow(dead_code, reason = "the GPT reader uses its checksum alone")]
mod crc32;
#[path = "../../engine/src/gpt.rs"]
#[allow(dead_code, reason = "the selector reads a table and writes none")]
mod gpt;
// Which terminal `/dev/console` is, td-init's, shared with its secret-line
// applet: it lives outside this crate, so the `#[path]` is required, and the
// recipe stages it at `{src}/td-init/src/console.rs`.
#[path = "../../td-init/src/console.rs"]
mod console;
#[path = "eventlog.rs"]
mod eventlog;
// The selector's FIDO2 client and its `fido-worker` verb.
#[path = "fido.rs"]
mod fido;
#[path = "measurement.rs"]
mod measurement;
#[path = "mirror.rs"]
mod mirror;
mod protocol;
// The archive preparing a selector appends, shared with td-install by its
// path; the staged-update commit rebuilds it to check `INITRD.NEW`.
#[path = "selector_appendix.rs"]
mod selector_appendix;
#[path = "selector_release.rs"]
mod selector_release;
// A staged selector update's pre-seal and commit.
#[path = "selector_update.rs"]
mod selector_update;
#[path = "unlock.rs"]
mod unlock;
#[path = "volume.rs"]
mod volume;
// The real-regular-bounded file rule, td-fs's, shared with `td-install` for
// `protocol.rs`'s reason. It lives outside this crate, so the `#[path]` is
// required, and the recipe stages it at `{src}/td-fs/src/real_file.rs`: it
// is in the target binary.
#[path = "../../td-fs/src/real_file.rs"]
mod td_fs;
// td-fs's fixture guard, for this crate's tests only.
#[cfg(test)]
#[path = "../../td-fs/src/temp_dir.rs"]
mod test_temp;
// The committed fixture deployment, in its own file because the `td-install`
// recipe check stages the SAME one: a manifest and the payloads it hashes.
// `cfg(test)` keeps it out of the target binary, which is why td-boot's
// recipe does not stage it.
//
// Written with a redundant `#[path]` — the file is where the module name says
// — so that `affected.rs`'s staging guard SEES it: that guard reads `#[path]`
// attributes and is blind to a plain `mod`. Gated, it is skipped; ungated, the
// guard demands td-boot's recipe stage it. So dropping the `cfg(test)` to
// reuse this outside tests reds `ready` instead of failing the target build an
// hour later inside `recipe-checks`, which is the loop that guard exists to
// close. This is the first file under a target-static crate's `src/` that is
// deliberately not staged.
#[cfg(test)]
#[path = "fixture.rs"]
mod fixture;
// td's FAT32 writer, which builds the selector update tests' ESPs.
#[cfg(test)]
#[path = "../../engine/src/fat.rs"]
#[allow(dead_code, reason = "the tests build images with its writer alone")]
mod fat;
#[path = "../../engine/src/sha256.rs"]
#[allow(dead_code)]
mod sha256;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, Metadata, OpenOptions, TryLockError};
use std::io::{self, Seek, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{
    symlink, DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt,
};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use td_protector::luks::{KeyFile, Luks};
use td_protector::release::DeviceTpm;

// td-boot reaches td-owned programs, including D6's static td-luks,
// which the deployment initramfs's unlock runs (unlock.rs). `losetup` was the
// last other one: the ability to BOOT rested on busybox existing at an
// absolute path and parsing `-r <device> <file>` as expected, with nothing
// tying the two together, so dropping that applet from the image would have
// stopped every boot at the root loop with no build-time complaint. It is a
// td-init applet beside mount/umount, under one pinned `LOOP_SET_FD` request.
const TD_MOUNT: &str = "/bin/mount";
const TD_UMOUNT: &str = "/bin/umount";
const TD_LOSETUP: &str = "/bin/losetup";
const TD_KEXEC: &str = "/bin/td-kexec";
// root-loop requires procfs so losetup reopens the verified inode, not its path.
const STDIN_PATH: &str = "/proc/self/fd/0";
const ATTEMPT_HEADER: &[u8] = b"td-boot-attempt-v1";
const MAX_ATTEMPT_BYTES: u64 = 64;
const MAX_CMDLINE_BYTES: usize = 2048;
const MAX_MOUNTINFO_BYTES: u64 = 1024 * 1024;
const UPDATE_LOCK_DIR: &str = "/run/td-boot-locks";

enum Mode {
    Initramfs,
    /// One FIDO2 session's HID worker: td-fido's root worker for the node
    /// the session admitted, started by `fido::Client` (run by nothing yet).
    FidoWorker {
        index: String,
        inode: String,
        rdev: String,
    },
    OnVolume {
        operation: Box<Mode>,
    },
    /// The deployment initramfs moves the admission record to the
    /// system's `/run` (td-install/ENCRYPTION.md "Verified account handoff").
    Admit {
        run: PathBuf,
    },
    MountVolume {
        device: PathBuf,
        mountpoint: PathBuf,
        var: bool,
    },
    Volume {
        uuid: Option<volume::Uuid>,
    },
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
    /// Publish into a volume root that is ALREADY writable — the inner half of
    /// `install`, which mounts and then does exactly this.
    ///
    /// It exists because `td-install` cannot mount (its DESIGN's D8 leaves it
    /// no syscall surface) and a regular-file destination has no partition
    /// device to mount anyway (D9). So the installer stages a deployment into
    /// the tree `mkfs.btrfs --rootdir` bakes into the volume, and the publish
    /// happens before the filesystem does. D1 is unharmed: this is not a second
    /// implementation but the same `install_deployment`, reached without the
    /// mount.
    Reinstall {
        device: PathBuf,
        mountpoint: PathBuf,
        source: PathBuf,
    },
    Publish {
        root: PathBuf,
        source: PathBuf,
    },
    /// The unattended entry point: take whatever the channel is offering, if
    /// anything.
    Update {
        device: PathBuf,
        mountpoint: PathBuf,
        /// The volume's READ-ONLY view, which `update` requires to be a real
        /// mount point (DESIGN §10 item 10f). It is named rather than derived
        /// because only the caller knows which directory ought to carry a
        /// filesystem: nothing distinguishes a mounted volume from an ordinary
        /// directory of the same name except being told which to expect.
        volume: PathBuf,
        channel: PathBuf,
    },
    /// Moves `current` back to `previous`, but only while the two
    /// selectors still name the pair the caller approved (td-install
    /// DESIGN, "Full-system volume consumers").
    Rollback {
        device: PathBuf,
        mountpoint: PathBuf,
        current: String,
        previous: String,
    },
    Success {
        device: PathBuf,
        mountpoint: PathBuf,
        deployment_id: String,
    },
    /// Read-only payload-integrity preflight of a source directory, which
    /// prints its deployment id; no publication.
    ValidateSource {
        directory: PathBuf,
    },
    /// The live selector: verify the deployment on the attached install
    /// medium against its manifest and kexec it with a fresh volatile volume
    /// identity (td-install/MEDIA.md "Live boot").
    LiveBoot {
        mountpoint: PathBuf,
        cmdline: OsString,
    },
    /// The live deployment's root: bind the medium's `root.erofs` to a
    /// read-only loop after checking it against the handed-off deployment id,
    /// leaving the medium mounted.
    LiveRoot {
        mountpoint: PathBuf,
        deployment_id: String,
        loop_device: PathBuf,
    },
    /// Stage the live volume's contents for `mkfs.btrfs --rootdir` from the
    /// medium `live-root` left mounted.
    LiveSeed {
        mountpoint: PathBuf,
        deployment_id: String,
        seed: PathBuf,
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
    // The manifest's digest `initramfs` verified against, which td-kexec's
    // key handoff checks its copy against.
    initramfs_digest: String,
}

struct Selection {
    slot: &'static str,
    deployment: Deployment,
    current_error: Option<String>,
}

struct BootDecision {
    slot: &'static str,
    deployment_id: String,
    current_error: Option<String>,
    exhausted_deployment: Option<String>,
    fallback_error: Option<String>,
    bookkeeping_error: Option<String>,
    remaining_attempts: Option<u8>,
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
        "usage: td-boot initramfs /init\n       td-boot on-volume <boot|install|update|rollback|success|mount-root|mount-var> <arguments without device>\n       td-boot volume [UUID]\n       td-boot verify <volume-root>\n       td-boot root-loop <volume-root> <deployment-id> <loop-device>\n       td-boot boot <device> <mountpoint> <cmdline>\n       td-boot install <device> <mountpoint> <deployment-directory>\n       td-boot update <device> <mountpoint> <volume> <channel>\n       td-boot reinstall <device> <mountpoint> <deployment-directory>\n       td-boot publish <volume-root> <deployment-directory>\n       td-boot rollback <device> <mountpoint> <current-id> <previous-id>\n       td-boot success <device> <mountpoint> <deployment-id>\n       td-boot validate-source <deployment-directory>\n       td-boot live-boot <mountpoint> <cmdline>\n       td-boot live-root <mountpoint> <deployment-id> <loop-device>\n       td-boot live-seed <mountpoint> <deployment-id> <seed-directory>\n       td-boot admit <run>\n       td-boot fido-worker <index> <inode> <rdev>",
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
        Some(mode) if mode == "initramfs" => {
            if args.next().as_deref() != Some(OsStr::new("/init")) || args.next().is_some() {
                return Err(usage_error());
            }
            Ok(Mode::Initramfs)
        }
        Some(mode) if mode == OsStr::new(fido::VERB) => {
            let mut text = || {
                args.next()
                    .ok_or_else(usage_error)?
                    .into_string()
                    .map_err(|_| invalid("non-UTF-8 FIDO worker argument"))
            };
            let (index, inode, rdev) = (text()?, text()?, text()?);
            if args.next().is_some() {
                return Err(usage_error());
            }
            Ok(Mode::FidoWorker { index, inode, rdev })
        }
        Some(mode) if mode == OsStr::new("on-volume") => {
            let verb = args.next().ok_or_else(usage_error)?;
            if !matches!(
                verb.to_str(),
                Some(
                    "boot"
                        | "install"
                        | "update"
                        | "rollback"
                        | "success"
                        | "mount-root"
                        | "mount-var"
                )
            ) {
                return Err(usage_error());
            }
            let mut inner = vec![verb, OsString::from("/volume-device")];
            inner.extend(args);
            Ok(Mode::OnVolume {
                operation: Box::new(parse_args(inner.into_iter())?),
            })
        }
        Some(mode) if mode == OsStr::new("admit") => {
            let run = PathBuf::from(args.next().ok_or_else(usage_error)?);
            if args.next().is_some() {
                return Err(usage_error());
            }
            Ok(Mode::Admit { run })
        }
        Some(mode) if mode == OsStr::new("mount-root") || mode == OsStr::new("mount-var") => {
            let device = PathBuf::from(args.next().ok_or_else(usage_error)?);
            let mountpoint = PathBuf::from(args.next().ok_or_else(usage_error)?);
            if args.next().is_some() {
                return Err(usage_error());
            }
            Ok(Mode::MountVolume {
                device,
                mountpoint,
                var: mode == OsStr::new("mount-var"),
            })
        }
        Some(mode) if mode == OsStr::new("volume") => {
            let uuid = args
                .next()
                .map(|value| {
                    let text = value
                        .to_str()
                        .ok_or_else(|| invalid("non-ASCII volume UUID"))?;
                    volume::Uuid::parse(text)
                })
                .transpose()?;
            if args.next().is_some() {
                return Err(usage_error());
            }
            Ok(Mode::Volume { uuid })
        }
        Some(mode) if mode == OsStr::new("verify") => {
            let root = args.next().ok_or_else(usage_error)?;
            if args.next().is_some() {
                return Err(usage_error());
            }
            Ok(Mode::Verify {
                root: PathBuf::from(root),
            })
        }
        Some(mode) if mode == OsStr::new("validate-source") => {
            let directory = args.next().ok_or_else(usage_error)?;
            if args.next().is_some() {
                return Err(usage_error());
            }
            Ok(Mode::ValidateSource {
                directory: PathBuf::from(directory),
            })
        }
        Some(mode) if mode == OsStr::new("live-boot") => {
            let mountpoint = args.next().ok_or_else(usage_error)?;
            let cmdline = args.next().ok_or_else(usage_error)?;
            if args.next().is_some() {
                return Err(usage_error());
            }
            Ok(Mode::LiveBoot {
                mountpoint: PathBuf::from(mountpoint),
                cmdline,
            })
        }
        Some(mode) if mode == OsStr::new("live-root") => {
            let mountpoint = args.next().ok_or_else(usage_error)?;
            let deployment_id = parse_deployment_id(args.next().ok_or_else(usage_error)?)?;
            let loop_device = args.next().ok_or_else(usage_error)?;
            if args.next().is_some() {
                return Err(usage_error());
            }
            Ok(Mode::LiveRoot {
                mountpoint: PathBuf::from(mountpoint),
                deployment_id,
                loop_device: PathBuf::from(loop_device),
            })
        }
        Some(mode) if mode == OsStr::new("live-seed") => {
            let mountpoint = args.next().ok_or_else(usage_error)?;
            let deployment_id = parse_deployment_id(args.next().ok_or_else(usage_error)?)?;
            let seed = args.next().ok_or_else(usage_error)?;
            if args.next().is_some() {
                return Err(usage_error());
            }
            Ok(Mode::LiveSeed {
                mountpoint: PathBuf::from(mountpoint),
                deployment_id,
                seed: PathBuf::from(seed),
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
        Some(mode) if mode == OsStr::new("reinstall") => {
            let device = args.next().ok_or_else(usage_error)?;
            let mountpoint = args.next().ok_or_else(usage_error)?;
            let source = args.next().ok_or_else(usage_error)?;
            if args.next().is_some() {
                return Err(usage_error());
            }
            Ok(Mode::Reinstall {
                device: device.into(),
                mountpoint: mountpoint.into(),
                source: source.into(),
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
        Some(mode) if mode == OsStr::new(protocol::PUBLISH_VERB) => {
            let root = args.next().ok_or_else(usage_error)?;
            let source = args.next().ok_or_else(usage_error)?;
            if args.next().is_some() {
                return Err(usage_error());
            }
            Ok(Mode::Publish {
                root: PathBuf::from(root),
                source: PathBuf::from(source),
            })
        }
        Some(mode) if mode == OsStr::new(protocol::UPDATE_VERB) => {
            let device = args.next().ok_or_else(usage_error)?;
            let mountpoint = args.next().ok_or_else(usage_error)?;
            let volume = args.next().ok_or_else(usage_error)?;
            let channel = args.next().ok_or_else(usage_error)?;
            if args.next().is_some() {
                return Err(usage_error());
            }
            Ok(Mode::Update {
                device: PathBuf::from(device),
                mountpoint: PathBuf::from(mountpoint),
                volume: PathBuf::from(volume),
                channel: PathBuf::from(channel),
            })
        }
        Some(mode) if mode == OsStr::new("rollback") => {
            let device = args.next().ok_or_else(usage_error)?;
            let mountpoint = args.next().ok_or_else(usage_error)?;
            let current = parse_deployment_id(args.next().ok_or_else(usage_error)?)?;
            let previous = parse_deployment_id(args.next().ok_or_else(usage_error)?)?;
            if args.next().is_some() {
                return Err(usage_error());
            }
            if current == previous {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "a rollback names two different deployments",
                ));
            }
            Ok(Mode::Rollback {
                device: PathBuf::from(device),
                mountpoint: PathBuf::from(mountpoint),
                current,
                previous,
            })
        }
        Some(mode) if mode == OsStr::new("success") => {
            let device = args.next().ok_or_else(usage_error)?;
            let mountpoint = args.next().ok_or_else(usage_error)?;
            let deployment_id = parse_deployment_id(args.next().ok_or_else(usage_error)?)?;
            if args.next().is_some() {
                return Err(usage_error());
            }
            Ok(Mode::Success {
                device: PathBuf::from(device),
                mountpoint: PathBuf::from(mountpoint),
                deployment_id,
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

use td_fs::{open_real_file, read_bounded_real_file};

use protocol::valid_digest;

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
    if header != protocol::MANIFEST_HEADER || !terminator.is_empty() || lines.next().is_some() {
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

/// The bundle in `directory`: its manifest, read once and parsed from the
/// bytes in hand, and each payload checked against it. Its id is the
/// manifest's hash; nothing about it says who produced it.
///
/// Only a source may be a mounted medium (`names`), while a published
/// deployment is held to the exact names the boot path opens.
fn open_bundle(directory: &Path, names: Names) -> io::Result<VerifiedBundle> {
    require_absolute(directory, "deployment directory")?;
    require_real_directory(directory, "deployment directory")?;
    let manifest_path = directory.join(protocol::MANIFEST_NAME);
    let manifest = read_bounded_real_file(
        &manifest_path,
        "deployment manifest",
        protocol::MAX_MANIFEST_BYTES,
    )?;
    let parsed = parse_manifest(&manifest)?;
    let id = sha256::hex_digest(&manifest);
    let kernel = verify_payload(
        directory,
        &bundle_name(directory, "bzImage", names)?,
        &parsed.kernel,
    )?;
    let initramfs = verify_payload(
        directory,
        &bundle_name(directory, "initramfs.cpio", names)?,
        &parsed.initramfs,
    )?;
    let root = verify_payload(
        directory,
        &bundle_name(directory, "root.erofs", names)?,
        &parsed.root,
    )?;
    Ok(VerifiedBundle {
        id,
        manifest,
        kernel,
        initramfs,
        root,
    })
}

/// Which payload names a bundle directory may use.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Names {
    /// A deployment td published: exactly the names the boot path opens.
    Published,
    /// An install or validation source, which may be a mounted medium.
    Source,
}

/// A payload's name in a bundle directory: its deployment spelling, or, for a
/// source, the install medium's, whose ISO 9660 mount shows `bzImage` as
/// `bzimage` (MEDIA.md), so a mounted medium is a source directory as it
/// stands. Publication writes the deployment spelling either way.
///
/// Either is safe to accept because the caller verifies the bytes against the
/// manifest's digest; a directory holding both as different files is refused
/// rather than chosen between, so one bundle has one spelling. One file under
/// both, as a case-insensitive filesystem shows it, is that file.
fn bundle_name(directory: &Path, name: &str, names: Names) -> io::Result<String> {
    let medium = media_name(name)?;
    if names == Names::Published || medium == name {
        return Ok(name.to_string());
    }
    let identity = |name: &str| match fs::symlink_metadata(directory.join(name)) {
        Ok(metadata) => Ok(Some((metadata.dev(), metadata.ino()))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    };
    match (identity(name)?, identity(&medium)?) {
        (Some(deployment), Some(source)) if deployment != source => Err(invalid(format!(
            "source directory {} holds both {name} and {medium}",
            directory.display()
        ))),
        (None, Some(_)) => Ok(medium),
        // Absent, the open below names the deployment spelling.
        _ => Ok(name.to_string()),
    }
}

fn sync_directory(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

fn uid_is_root(uid: u32) -> bool {
    uid == 0
}

fn state_owner_allowed(file_uid: u32, fixture_uid: Option<u32>) -> bool {
    uid_is_root(file_uid) || fixture_uid == Some(file_uid)
}

#[cfg(test)]
fn fixture_uid() -> Option<u32> {
    fs::metadata("/proc/self").ok().map(|process| process.uid())
}

#[cfg(not(test))]
fn fixture_uid() -> Option<u32> {
    None
}

fn root_owned(metadata: &Metadata) -> bool {
    state_owner_allowed(metadata.uid(), fixture_uid())
}

fn validate_attempts_directory(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_dir()
        || !root_owned(&metadata)
        || metadata.permissions().mode() & 0o777 != 0o700
    {
        return Err(invalid(format!(
            "boot attempts directory must be a root-owned real directory with mode 0700: {}",
            path.display()
        )));
    }
    Ok(())
}

fn attempts_directory(root: &Path, create: bool) -> io::Result<Option<PathBuf>> {
    let path = root.join(protocol::ATTEMPTS_DIR);
    match fs::symlink_metadata(&path) {
        Ok(_) => {
            validate_attempts_directory(&path)?;
            Ok(Some(path))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound && !create => Ok(None),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let mut builder = fs::DirBuilder::new();
            builder.mode(0o700);
            builder.create(&path)?;
            validate_attempts_directory(&path)?;
            sync_directory(&root.join(protocol::BOOT_DIR))?;
            Ok(Some(path))
        }
        Err(error) => Err(error),
    }
}

fn parse_attempt_state(bytes: &[u8]) -> io::Result<u8> {
    let mut lines = bytes.split(|byte| *byte == b'\n');
    let header = lines
        .next()
        .ok_or_else(|| invalid("boot attempt state is empty"))?;
    let remaining = lines
        .next()
        .ok_or_else(|| invalid("boot attempt state has no remaining count"))?;
    let terminator = lines
        .next()
        .ok_or_else(|| invalid("boot attempt state lacks a trailing newline"))?;
    if header != ATTEMPT_HEADER || !terminator.is_empty() || lines.next().is_some() {
        return Err(invalid(
            "boot attempt state must have the exact td-boot-attempt-v1 two-line form",
        ));
    }
    let value = remaining
        .strip_prefix(b"remaining ")
        .ok_or_else(|| invalid("boot attempt state has a malformed remaining count"))?;
    if value.is_empty()
        || !value.iter().all(u8::is_ascii_digit)
        || (value.len() > 1 && value.first() == Some(&b'0'))
    {
        return Err(invalid(
            "boot attempt remaining count must be canonical decimal",
        ));
    }
    let remaining = std::str::from_utf8(value)
        .ok()
        .and_then(|text| text.parse::<u8>().ok())
        .ok_or_else(|| invalid("boot attempt remaining count is out of range"))?;
    if remaining > protocol::ATTEMPT_V1_MAX_REMAINING {
        return Err(invalid(format!(
            "boot attempt remaining count exceeds {}",
            protocol::ATTEMPT_V1_MAX_REMAINING
        )));
    }
    Ok(remaining)
}

fn read_attempt_state(root: &Path, id: &str) -> io::Result<Option<u8>> {
    if !valid_digest(id.as_bytes()) {
        return Err(invalid("invalid boot attempt deployment id"));
    }
    let Some(directory) = attempts_directory(root, false)? else {
        return Ok(None);
    };
    let path = directory.join(id);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if !metadata.file_type().is_file()
        || !root_owned(&metadata)
        || metadata.permissions().mode() & 0o777 != 0o600
    {
        return Err(invalid(format!(
            "boot attempt state must be a root-owned real file with mode 0600: {}",
            path.display()
        )));
    }
    let bytes = read_bounded_real_file(&path, "boot attempt state", MAX_ATTEMPT_BYTES)?;
    parse_attempt_state(&bytes).map(Some)
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

fn reap_attempt_temporaries(attempts: &Path) -> io::Result<()> {
    let mut changed = false;
    for entry in fs::read_dir(attempts)? {
        let entry = entry?;
        let name = entry.file_name();
        if temporary_owner_pid(name.as_bytes(), b".attempt-", true).is_none() {
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
        sync_directory(attempts)?;
    }
    Ok(())
}

fn create_attempt_file(directory: &Path, id: &str) -> io::Result<(PathBuf, File)> {
    for attempt in 0..1024u32 {
        let path = directory.join(format!(".attempt-{id}-{}-{attempt}", std::process::id()));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!(
            "could not create a unique boot attempt file below {}",
            directory.display()
        ),
    ))
}

fn write_attempt_state(root: &Path, id: &str, remaining: u8) -> io::Result<()> {
    if !valid_digest(id.as_bytes()) || remaining > protocol::ATTEMPT_V1_MAX_REMAINING {
        return Err(invalid("invalid boot attempt state"));
    }
    let directory = attempts_directory(root, true)?
        .ok_or_else(|| invalid("boot attempts directory was not created"))?;
    reap_attempt_temporaries(&directory)?;
    let (temporary, mut file) = create_attempt_file(&directory, id)?;
    let mut cleanup = RemoveFile {
        path: Some(temporary.clone()),
    };
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file()
        || !root_owned(&metadata)
        || metadata.permissions().mode() & 0o777 != 0o600
    {
        return Err(invalid(format!(
            "new boot attempt state must be a root-owned real file with mode 0600: {}",
            temporary.display()
        )));
    }
    file.write_all(ATTEMPT_HEADER)?;
    writeln!(file, "\nremaining {remaining}")?;
    file.sync_all()?;
    fs::rename(&temporary, directory.join(id))?;
    cleanup.path = None;
    sync_directory(&directory)
}

fn mark_attempt_successful(root: &Path, id: &str) -> io::Result<()> {
    if read_attempt_state(root, id)?.is_none() {
        return Ok(());
    }
    let directory = attempts_directory(root, false)?
        .ok_or_else(|| invalid("boot attempts directory disappeared"))?;
    fs::remove_file(directory.join(id))?;
    sync_directory(&directory)
}

enum AttemptDecision {
    Successful,
    Consumed(u8),
    Exhausted,
}

fn consume_boot_attempt(root: &Path, id: &str) -> io::Result<AttemptDecision> {
    match read_attempt_state(root, id)? {
        None => Ok(AttemptDecision::Successful),
        Some(0) => Ok(AttemptDecision::Exhausted),
        Some(remaining) => {
            let next = remaining.saturating_sub(1);
            write_attempt_state(root, id, next)?;
            Ok(AttemptDecision::Consumed(next))
        }
    }
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

/// Whether the already-published deployment at `directory` verifies as the
/// one being installed. The id is the hash of the manifest that names the
/// payload digests, so one that verifies under the same id is the same
/// deployment.
fn existing_is_same(directory: &Path, want: &VerifiedBundle) -> io::Result<bool> {
    Ok(open_bundle(directory, Names::Published)?.id == want.id)
}

fn publish_bundle(root: &Path, mut bundle: VerifiedBundle) -> io::Result<String> {
    let deployments = root.join(protocol::DEPLOYMENTS_DIR);
    require_real_directory(&deployments, "deployments directory")?;
    let destination = deployments.join(&bundle.id);
    match fs::symlink_metadata(&destination) {
        Ok(_) => {
            if !existing_is_same(&destination, &bundle)? {
                return Err(invalid(format!(
                    "existing deployment {} does not verify as {}",
                    destination.display(),
                    bundle.id
                )));
            }
            sync_directory(&deployments)?;
            return Ok(bundle.id);
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
    write_synced_file(&staging.join(protocol::MANIFEST_NAME), &bundle.manifest)?;
    sync_directory(&staging)?;

    let staged = open_bundle(&staging, Names::Published)?;
    if staged.id != bundle.id {
        return Err(invalid(format!(
            "staged deployment id {} changed from verified source {}",
            staged.id, bundle.id
        )));
    }

    match fs::rename(&staging, &destination) {
        Ok(()) => cleanup.path = None,
        Err(rename_error) => {
            // A concurrent publisher won the rename. Its bundle is the same
            // deployment if it verifies under the same id.
            match existing_is_same(&destination, &bundle) {
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
    if current != previous {
        write_attempt_state(root, current, protocol::DEFAULT_BOOT_ATTEMPTS)?;
    }
    replace_selector(root, "current", current)
}

fn install_deployment(root: &Path, source: &Path) -> io::Result<String> {
    install_deployment_mode(root, source, false)
}

fn install_deployment_mode(root: &Path, source: &Path, reset: bool) -> io::Result<String> {
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

    let bundle = open_bundle(source, Names::Source)?;
    let candidate = bundle.id.clone();
    let selection = if reset {
        None
    } else {
        match select_deployment(root) {
            Ok(selection) => Some(selection),
            Err(_) if selectors_are_absent(root)? => None,
            Err(error) => {
                return Err(invalid(format!(
                    "refusing activation without a verified existing deployment: {error}"
                )));
            }
        }
    };
    // A recovery must replace even a damaged copy with this same manifest ID.
    // Keep removal in the publisher's existing temporary-directory namespace;
    // a crash is reclaimed on the next transaction, never by walking @var.
    let _replaced = if reset {
        let deployments = root.join(protocol::DEPLOYMENTS_DIR);
        let destination = deployments.join(&candidate);
        match fs::symlink_metadata(&destination) {
            Ok(_) => {
                require_real_directory(&destination, "replaced deployment")?;
                let temporary = create_install_directory(&deployments, &candidate)?;
                let cleanup = RemoveDirectory {
                    path: Some(temporary.clone()),
                };
                fs::rename(&destination, &temporary)?;
                sync_directory(&deployments)?;
                Some(cleanup)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        }
    } else {
        None
    };
    let installed = publish_bundle(root, bundle)?;
    if reset {
        // Recovery replaces even malformed retry bookkeeping for this ID.
        if let Some(directory) = attempts_directory(root, false)? {
            match fs::remove_file(directory.join(&installed)) {
                Ok(()) => sync_directory(&directory)?,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        activate_install(root, &installed, &installed)?;
        return Ok(installed);
    }
    if selection
        .as_ref()
        .is_some_and(|selected| selected.slot == "current" && selected.deployment.id == installed)
    {
        match verify_slot(root, "previous") {
            Ok(previous) if read_attempt_state(root, &previous.id)?.is_none() => {}
            Ok(previous) => {
                return Err(invalid(format!(
                    "refusing to retain pending deployment {} as the fallback",
                    previous.id
                )));
            }
            Err(_) => {
                if read_attempt_state(root, &installed)?.is_some() {
                    return Err(invalid(
                        "refusing to replace an invalid fallback with a pending deployment",
                    ));
                }
                replace_selector(root, "previous", &installed)?;
            }
        }
        return Ok(installed);
    }
    let known_good = match &selection {
        Some(selected) if read_attempt_state(root, &selected.deployment.id)?.is_some() => {
            let previous = verify_slot(root, "previous").map_err(|error| {
                invalid(format!(
                    "refusing activation while current deployment {} is pending: \
                     no verified fallback ({error})",
                    selected.deployment.id
                ))
            })?;
            if read_attempt_state(root, &previous.id)?.is_some() {
                return Err(invalid(
                    "refusing activation without a successful fallback deployment",
                ));
            }
            previous.id
        }
        Some(selected) => selected.deployment.id.clone(),
        None => candidate.clone(),
    };
    activate_install(root, &known_good, &installed)?;
    Ok(installed)
}

/// Moves `current` to `previous` only while the selectors name exactly
/// the approved pair: the caller holds the transaction lock, and the check
/// comes before any change, so an update or rollback published since the
/// pair was read is refused rather than undone.
fn rollback_deployment(root: &Path, current: &str, previous: &str) -> io::Result<String> {
    require_absolute(root, "volume root")?;
    require_real_directory(root, "volume root")?;
    require_real_directory(&root.join(protocol::BOOT_DIR), "boot selector directory")?;
    for (slot, expected) in [("current", current), ("previous", previous)] {
        let named = read_selector(root, slot)?;
        if named != expected {
            return Err(invalid(format!(
                "refusing rollback: {slot} names {named}, not the approved {expected}"
            )));
        }
    }
    reap_selector_temporaries(&root.join(protocol::BOOT_DIR))?;
    let previous = verify_slot(root, "previous")?;
    let previous_id = previous.id;
    if read_attempt_state(root, &previous_id)?.is_some() {
        return Err(invalid(format!(
            "previous deployment {previous_id} is not marked successful"
        )));
    }
    replace_selector(root, "current", &previous_id)?;
    Ok(previous_id)
}

fn current_attempt_state(root: &Path, deployment_id: &str) -> io::Result<Option<u8>> {
    require_absolute(root, "volume root")?;
    require_real_directory(root, "volume root")?;
    require_real_directory(&root.join(protocol::BOOT_DIR), "boot selector directory")?;
    let current_id = read_selector(root, "current")?;
    if current_id != deployment_id {
        return Err(invalid(format!(
            "running deployment {deployment_id} is not current {}",
            current_id
        )));
    }
    read_attempt_state(root, deployment_id)
}

fn mark_deployment_successful(root: &Path, deployment_id: &str) -> io::Result<String> {
    if current_attempt_state(root, deployment_id)?.is_none() {
        return Ok(deployment_id.to_string());
    }
    verify_deployment(root, deployment_id)?;
    mark_attempt_successful(root, deployment_id)?;
    Ok(deployment_id.to_string())
}

/// A deployment's manifest bytes, read ONCE and bound to its id by hash, so
/// the payload digests are parsed from the bytes that hashed to the id.
fn manifest_bytes(root: &Path, id: &str) -> io::Result<(PathBuf, Vec<u8>)> {
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
    let manifest_path = directory.join(protocol::MANIFEST_NAME);
    let bytes = read_bounded_real_file(
        &manifest_path,
        "deployment manifest",
        protocol::MAX_MANIFEST_BYTES,
    )?;
    let manifest_id = sha256::hex_digest(&bytes);
    if manifest_id.as_str() != id {
        return Err(invalid(format!(
            "deployment id {id} does not match manifest hash {manifest_id}"
        )));
    }
    Ok((directory, bytes))
}

fn verified_manifest(root: &Path, id: &str) -> io::Result<(PathBuf, Manifest)> {
    let (directory, bytes) = manifest_bytes(root, id)?;
    let manifest = parse_manifest(&bytes)?;
    Ok((directory, manifest))
}

fn verify_slot(root: &Path, slot: &str) -> io::Result<Deployment> {
    let id = read_selector(root, slot)?;
    verify_deployment(root, &id)
}

fn verify_deployment(root: &Path, id: &str) -> io::Result<Deployment> {
    let (directory, manifest) = verified_manifest(root, id)?;
    verify_deployment_payloads(&directory, id, &manifest)
}

/// The payload half, given a manifest a caller has already established.
fn verify_deployment_payloads(
    directory: &Path,
    id: &str,
    manifest: &Manifest,
) -> io::Result<Deployment> {
    let kernel = verify_payload(directory, "bzImage", &manifest.kernel)?;
    let initramfs = verify_payload(directory, "initramfs.cpio", &manifest.initramfs)?;
    // Verify root here so corruption selects previous; root-loop repeats the hash
    // after kexec to bind the verified inode at the actual mount boundary.
    verify_payload(directory, "root.erofs", &manifest.root)?;

    Ok(Deployment {
        id: id.to_string(),
        kernel,
        initramfs,
        initramfs_digest: manifest.initramfs.clone(),
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

/// The fallback decision, given an already-read `previous`.
fn previous_decision(
    root: &Path,
    previous: Deployment,
    current_error: Option<String>,
    exhausted_deployment: Option<String>,
) -> io::Result<BootDecision> {
    if read_attempt_state(root, &previous.id)?.is_some() {
        return Err(invalid(format!(
            "previous deployment {} is not marked successful",
            previous.id
        )));
    }
    Ok(BootDecision {
        slot: "previous",
        deployment_id: previous.id,
        current_error,
        exhausted_deployment,
        fallback_error: None,
        bookkeeping_error: None,
        remaining_attempts: None,
    })
}

fn verified_previous_decision(
    root: &Path,
    current_error: Option<String>,
    exhausted_deployment: Option<String>,
) -> io::Result<BootDecision> {
    let previous = verify_slot(root, "previous")?;
    previous_decision(root, previous, current_error, exhausted_deployment)
}

fn select_boot_deployment(root: &Path) -> io::Result<BootDecision> {
    require_absolute(root, "volume root")?;
    require_real_directory(root, "volume root")?;
    require_real_directory(&root.join("td"), "td directory")?;
    require_real_directory(&root.join(protocol::BOOT_DIR), "boot selector directory")?;
    require_real_directory(
        &root.join(protocol::DEPLOYMENTS_DIR),
        "deployments directory",
    )?;
    // Invalid attempt metadata cannot authorize a write, but must not prevent
    // conservative verified-previous recovery.
    let attempts = attempts_directory(root, false)
        .map_err(|error| io::Error::other(format!("boot attempt bookkeeping rejected: {error}")))?;
    if let Some(attempts) = attempts {
        reap_attempt_temporaries(&attempts)?;
    }

    let current = match verify_slot(root, "current") {
        Ok(current) => current,
        Err(error) => {
            let decision = verified_previous_decision(root, Some(error.to_string()), None)
                .map_err(|previous| {
                    invalid(format!(
                        "no verified deployment: current rejected ({error}); \
                         previous rejected ({previous})"
                    ))
                })?;
            replace_selector(root, "current", &decision.deployment_id)?;
            return Ok(decision);
        }
    };
    match consume_boot_attempt(root, &current.id) {
        Ok(AttemptDecision::Successful) => Ok(BootDecision {
            slot: "current",
            deployment_id: current.id,
            current_error: None,
            exhausted_deployment: None,
            fallback_error: None,
            bookkeeping_error: None,
            remaining_attempts: None,
        }),
        Ok(AttemptDecision::Consumed(remaining)) => Ok(BootDecision {
            slot: "current",
            deployment_id: current.id,
            current_error: None,
            exhausted_deployment: None,
            fallback_error: None,
            bookkeeping_error: None,
            remaining_attempts: Some(remaining),
        }),
        Ok(AttemptDecision::Exhausted) => {
            let exhausted = current.id;
            match verified_previous_decision(root, None, Some(exhausted.clone())) {
                Ok(decision) => {
                    replace_selector(root, "current", &decision.deployment_id)?;
                    Ok(decision)
                }
                Err(error) => Ok(BootDecision {
                    slot: "current",
                    deployment_id: exhausted.clone(),
                    current_error: None,
                    exhausted_deployment: Some(exhausted),
                    fallback_error: Some(error.to_string()),
                    bookkeeping_error: None,
                    remaining_attempts: None,
                }),
            }
        }
        Err(error) => match verified_previous_decision(root, Some(error.to_string()), None) {
            Ok(decision) => {
                replace_selector(root, "current", &decision.deployment_id)?;
                Ok(decision)
            }
            Err(previous) => Ok(BootDecision {
                slot: "current",
                deployment_id: current.id,
                current_error: None,
                exhausted_deployment: None,
                fallback_error: None,
                bookkeeping_error: Some(format!(
                    "current attempt state rejected ({error}); \
                         previous unavailable ({previous})"
                )),
                remaining_attempts: None,
            }),
        },
    }
}

fn cmdline_has_token(cmdline: &[u8], token: &[u8]) -> bool {
    cmdline
        .split(|byte| byte.is_ascii_whitespace())
        .any(|field| field == token)
}

#[derive(Debug, Eq, PartialEq)]
enum SuccessDisposition {
    Record,
    ConfirmRecovery,
    RejectRecovery,
}

fn success_disposition(cmdline: &[u8], deployment_id: &str) -> SuccessDisposition {
    if !cmdline_has_token(
        cmdline,
        protocol::BOOKKEEPING_UNAVAILABLE_CMDLINE_TOKEN.as_bytes(),
    ) {
        return SuccessDisposition::Record;
    }
    let deployment = format!("td.deployment={deployment_id}");
    if cmdline_has_token(cmdline, deployment.as_bytes()) {
        SuccessDisposition::ConfirmRecovery
    } else {
        SuccessDisposition::RejectRecovery
    }
}

fn kernel_cmdline(
    base: &OsStr,
    deployment_id: &str,
    bookkeeping_unavailable: bool,
) -> io::Result<OsString> {
    handoff_cmdline(base, deployment_id, bookkeeping_unavailable, "")
}

/// `kernel_cmdline` plus tokens only td-boot may add (`extra`, for the live
/// handoff). The live tokens are reserved in every base line, so neither an
/// installed selector's command line nor firmware can request the live branch.
fn handoff_cmdline(
    base: &OsStr,
    deployment_id: &str,
    bookkeeping_unavailable: bool,
    extra: &str,
) -> io::Result<OsString> {
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
    for reserved in [
        SELECTOR,
        protocol::BOOKKEEPING_UNAVAILABLE_CMDLINE_TOKEN.as_bytes(),
        protocol::LIVE_CMDLINE_PREFIX.as_bytes(),
        protocol::RAM_DISK_SIZE_PREFIX.as_bytes(),
    ] {
        if bytes
            .windows(reserved.len())
            .any(|window| window == reserved)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "kernel cmdline already contains reserved token {}",
                    String::from_utf8_lossy(reserved)
                ),
            ));
        }
    }

    // After a bare `--` the kernel hands every word to init, so a live RAM
    // disk size appended past one would never reach brd.
    if !extra.is_empty() && bytes.split(|byte| *byte == b' ').any(|word| word == b"--") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "kernel cmdline ends kernel parameters with --; the live handoff cannot follow it",
        ));
    }
    let mut token = format!("td.deployment={deployment_id}");
    if bookkeeping_unavailable {
        token.push(' ');
        token.push_str(protocol::BOOKKEEPING_UNAVAILABLE_CMDLINE_TOKEN);
    }
    if !extra.is_empty() {
        token.push(' ');
        token.push_str(extra);
    }
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
    let mut command = Command::new(TD_MOUNT);
    command.args([
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
    let mut command = Command::new(TD_UMOUNT);
    command.arg(mountpoint.as_os_str());
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

/// `--fds-key`: as `--fds`, with the initramfs digest td-boot verified and
/// the volume key on a pipe this process holds, named `/proc/PID/fd/N`
/// (td-install/ENCRYPTION.md "Boot and authority boundaries");
/// `--fds-key-admit` with the admission record on a second such pipe
/// ("Verified account handoff").
fn key_kexec_command(
    kernel: File,
    initramfs: File,
    digest: &str,
    key_pipe: &Path,
    record_pipe: Option<&Path>,
    cmdline: &OsStr,
) -> Command {
    let mut command = Command::new(TD_KEXEC);
    match record_pipe {
        None => command.arg("--fds-key").arg(digest).arg(key_pipe),
        Some(record) => command
            .arg("--fds-key-admit")
            .arg(digest)
            .arg(key_pipe)
            .arg(record),
    };
    command
        .arg(cmdline)
        .stdin(Stdio::from(kernel))
        .stdout(Stdio::from(initramfs));
    command
}

fn loop_command(root: File, loop_device: &Path) -> Command {
    let mut command = Command::new(TD_LOSETUP);
    command
        .args([
            OsStr::new("-r"),
            loop_device.as_os_str(),
            OsStr::new(STDIN_PATH),
        ])
        .stdin(Stdio::from(root));
    command
}

fn diagnostic(out: &mut dyn Write, message: std::fmt::Arguments<'_>) -> io::Result<()> {
    // Formatting directly to stderr can split a diagnostic across writes.
    let complete = format!("{message}\n");
    out.write_all(complete.as_bytes())
}

fn report_volume_binding(
    out: &mut dyn Write,
    uuid: &volume::Uuid,
    device: &Path,
) -> io::Result<()> {
    diagnostic(
        out,
        format_args!("TD-BOOT-VOLUME {uuid} {}", device.display()),
    )
}

fn report_fallback(out: &mut dyn Write, selection: &Selection) -> io::Result<()> {
    if let Some(error) = &selection.current_error {
        // Rejection is boot protocol, not best-effort diagnostics: an unobservable
        // fallback must not masquerade as a healthy primary selection.
        diagnostic(
            out,
            format_args!(
                "{}: current rejected ({error}); using previous {}",
                protocol::CURRENT_REJECTED_MARKER,
                selection.deployment.id
            ),
        )?;
    }
    Ok(())
}

fn report_boot_decision(out: &mut dyn Write, decision: &BootDecision) -> io::Result<()> {
    if let Some(error) = &decision.bookkeeping_error {
        diagnostic(
            out,
            format_args!("{}: {error}", protocol::BOOKKEEPING_UNAVAILABLE_MARKER),
        )?;
    }
    if let Some(error) = &decision.current_error {
        diagnostic(
            out,
            format_args!(
                "{}: current rejected ({error}); using previous {}",
                protocol::CURRENT_REJECTED_MARKER,
                decision.deployment_id
            ),
        )?;
    }
    if let Some(exhausted) = &decision.exhausted_deployment {
        if let Some(error) = &decision.fallback_error {
            diagnostic(
                out,
                format_args!(
                    "{} {exhausted}: previous unavailable ({error}); retrying {}",
                    protocol::ATTEMPTS_EXHAUSTED_MARKER,
                    decision.deployment_id
                ),
            )?;
        } else {
            diagnostic(
                out,
                format_args!(
                    "{} {exhausted} -> {}",
                    protocol::ATTEMPTS_EXHAUSTED_MARKER,
                    decision.deployment_id
                ),
            )?;
        }
    }
    if let Some(remaining) = decision.remaining_attempts {
        diagnostic(
            out,
            format_args!(
                "{} {} remaining={remaining}",
                protocol::ATTEMPT_CONSUMED_MARKER,
                decision.deployment_id
            ),
        )?;
    }
    let marker = match decision.slot {
        "current" => protocol::SELECTED_CURRENT_MARKER,
        "previous" => protocol::SELECTED_PREVIOUS_MARKER,
        _ => {
            return Err(invalid(format!(
                "internal: unknown boot decision slot {}",
                decision.slot
            )));
        }
    };
    diagnostic(out, format_args!("{marker} {}", decision.deployment_id))
}

fn attempt_status(state: Option<u8>) -> String {
    match state {
        None => "successful".to_string(),
        Some(0) => "exhausted".to_string(),
        Some(remaining) => format!("pending remaining={remaining}"),
    }
}

fn run_verify(root: &Path) -> io::Result<()> {
    let selection = select_deployment(root)?;
    report_fallback(&mut mirror::stderr(), &selection)?;
    let state = read_attempt_state(root, &selection.deployment.id)?;
    if selection.slot == "current" && state == Some(0) {
        if let Ok(previous) = verify_slot(root, "previous").and_then(|previous| {
            previous_decision(root, previous, None, Some(selection.deployment.id.clone()))
        }) {
            return writeln!(
                io::stdout(),
                "previous {} successful current-exhausted={}",
                previous.deployment_id,
                selection.deployment.id
            );
        }
    }
    let status = attempt_status(state);
    writeln!(
        io::stdout(),
        "{} {} {status}",
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
                mirror::stderr(),
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

fn mounted_btrfs_source(mountinfo: &[u8], mountpoint: &Path) -> io::Result<Option<OsString>> {
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
            if !options
                .split(|byte| *byte == b',')
                .any(|option| option == required)
            {
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

// A Btrfs mount can retain a now-closed /proc/PID/fd/N source name. Bind
// recovery to the filesystem itself, not to a reusable pid or descriptor name.
fn same_mounted_volume(device: &Path, mounted_dev: u64) -> io::Result<bool> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let base = update_lock_directory()?;
    let mut probe = None;
    for _ in 0..16 {
        let path = base.join(format!(
            "volume-probe-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        match fs::DirBuilder::new().mode(0o700).create(&path) {
            Ok(()) => {
                probe = Some(path);
                break;
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    let path = probe.ok_or_else(|| invalid("no private volume probe directory available"))?;
    let result = (|| {
        run_command(
            &mut mount_command(device, &path),
            "read-only volume identity mount",
        )?;
        let result = fs::metadata(&path).map(|metadata| metadata.dev() == mounted_dev);
        let unmounted = run_command(&mut unmount_command(&path), "volume identity unmount");
        match (result, unmounted) {
            (Ok(same), Ok(())) => Ok(same),
            (Err(error), _) | (_, Err(error)) => Err(error),
        }
    })();
    // Never recursively remove this path: a failed unmount may still expose a volume.
    let removed = fs::remove_dir(&path);
    match (result, removed) {
        (Ok(same), Ok(())) => Ok(same),
        (Err(error), _) | (_, Err(error)) => Err(error),
    }
}

fn prepare_update_mountpoint(mountpoint: &Path, device: &Path, device_id: u64) -> io::Result<()> {
    let current = fs::metadata(device)?;
    if !current.file_type().is_block_device() || current.rdev() != device_id {
        return Err(invalid("locked volume device changed"));
    }
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
        mounted_btrfs_source(&mountinfo, mountpoint)?.ok_or_else(|| {
            invalid(format!(
                "update mountpoint is mounted but absent from mountinfo: {}",
                mountpoint.display()
            ))
        })?;
        if !same_mounted_volume(device, metadata.dev())? {
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

enum WritableVolumeFailure<T> {
    Transaction(io::Error),
    Committed { value: T, unmount_error: io::Error },
    Mounted(io::Error),
}

impl<T> WritableVolumeFailure<T> {
    fn into_io(self) -> io::Error {
        match self {
            WritableVolumeFailure::Transaction(error) => error,
            WritableVolumeFailure::Committed {
                value: _,
                unmount_error,
            } => io::Error::new(
                unmount_error.kind(),
                format!("deployment transaction committed, but {unmount_error}"),
            ),
            WritableVolumeFailure::Mounted(error) => error,
        }
    }
}

fn finish_writable_operation<T>(
    result: io::Result<T>,
    unmounted: io::Result<()>,
) -> Result<T, WritableVolumeFailure<T>> {
    match (result, unmounted) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), Ok(())) => Err(WritableVolumeFailure::Transaction(error)),
        (Ok(value), Err(unmount_error)) => Err(WritableVolumeFailure::Committed {
            value,
            unmount_error,
        }),
        (Err(operation_error), Err(unmount_error)) => {
            Err(WritableVolumeFailure::Mounted(io::Error::new(
                operation_error.kind(),
                format!("{operation_error}; additionally {unmount_error}"),
            )))
        }
    }
}

fn run_on_prelocked_writable_volume<T>(
    device: &Path,
    mountpoint: &Path,
    device_id: u64,
    operation: impl FnOnce(&Path) -> io::Result<T>,
) -> Result<T, WritableVolumeFailure<T>> {
    prepare_update_mountpoint(mountpoint, device, device_id)
        .map_err(WritableVolumeFailure::Transaction)?;
    run_command(
        &mut writable_mount_command(device, mountpoint),
        "read-write Btrfs mount",
    )
    .map_err(WritableVolumeFailure::Transaction)?;

    let result = operation(mountpoint);
    let unmounted = run_command(&mut unmount_command(mountpoint), "Btrfs unmount");
    finish_writable_operation(result, unmounted)
}

fn run_on_writable_volume<T>(
    device: &Path,
    mountpoint: &Path,
    operation: impl FnOnce(&Path) -> io::Result<T>,
) -> io::Result<T> {
    require_absolute(device, "volume device")?;
    require_absolute(mountpoint, "mountpoint")?;
    let (_transaction_lock, _device_lock, device_id) = acquire_update_locks(device)?;
    run_on_prelocked_writable_volume(device, mountpoint, device_id, operation)
        .map_err(WritableVolumeFailure::into_io)
}

fn read_only_current(root: &Path) -> Option<(BootDecision, Deployment)> {
    let id = read_selector(root, "current").ok()?;
    if read_attempt_state(root, &id).ok()?.is_some() {
        return None;
    }
    let deployment = verify_deployment(root, &id).ok()?;
    Some((
        BootDecision {
            slot: "current",
            deployment_id: id,
            current_error: None,
            exhausted_deployment: None,
            fallback_error: None,
            bookkeeping_error: None,
            remaining_attempts: None,
        },
        deployment,
    ))
}

fn read_only_recovery(
    root: &Path,
    transaction_error: &io::Error,
) -> io::Result<(BootDecision, Deployment)> {
    let bookkeeping_error = Some(transaction_error.to_string());
    let current = verify_slot(root, "current");
    let previous = verify_slot(root, "previous");
    // The writable transaction owns attempt-state authority; recovery can only trust
    // verified payloads and the invariant that activation retains previous as fallback.
    match (current, previous) {
        (Ok(current), Ok(previous)) if previous.id != current.id => Ok((
            BootDecision {
                slot: "previous",
                deployment_id: previous.id.clone(),
                current_error: None,
                exhausted_deployment: None,
                fallback_error: None,
                bookkeeping_error,
                remaining_attempts: None,
            },
            previous,
        )),
        (Ok(current), _) => Ok((
            BootDecision {
                slot: "current",
                deployment_id: current.id.clone(),
                current_error: None,
                exhausted_deployment: None,
                fallback_error: None,
                bookkeeping_error,
                remaining_attempts: None,
            },
            current,
        )),
        (Err(current_error), Ok(previous)) => Ok((
            BootDecision {
                slot: "previous",
                deployment_id: previous.id.clone(),
                current_error: Some(current_error.to_string()),
                exhausted_deployment: None,
                fallback_error: None,
                bookkeeping_error,
                remaining_attempts: None,
            },
            previous,
        )),
        (Err(current_error), Err(previous_error)) => Err(invalid(format!(
            "boot state transaction failed ({transaction_error}); no read-only recovery: \
             current rejected ({current_error}); previous rejected ({previous_error})"
        ))),
    }
}

/// What the installed selector hands the deployment beside the kernel and
/// initramfs on an encrypted volume: the volume key, and after a protected
/// release the method its admission record carries.
#[derive(Clone, Copy)]
struct Handoff<'a> {
    key: &'a unlock::VolumeKey,
    admission: Option<admission::Method>,
}

/// The admission record for the selected deployment `id`: the primary
/// account, `method` once the PCR 11 measurement succeeded and no
/// admission otherwise (td-install/ENCRYPTION.md "Verified account
/// handoff").
fn admission_record(
    id: &str,
    method: admission::Method,
    measured: bool,
) -> io::Result<[u8; admission::RECORD_BYTES]> {
    let manifest = admission::manifest_id(id.as_bytes())
        .ok_or_else(|| invalid(format!("deployment id {id:?} is not 64 hex digits")))?;
    Ok(admission::Record {
        uid: admission::PRIMARY_UID,
        method: if measured {
            method
        } else {
            admission::Method::None
        },
        manifest,
    }
    .encode())
}

/// The consoles' line when a protected boot's measurement fails: fixed,
/// so it carries nothing the TPM or the deployment said.
const MEASUREMENT_FAILED: &str =
    "td-boot: TD-BOOT-UNMEASURED PCR 11 measurement failed; this boot admits no session";

/// Measures the selected deployment `id` into PCR 11 when the selector
/// carries a measurement policy, then builds a protected release's
/// admission record with the result. A failure aborts an unencrypted
/// boot. An `encrypted` one goes on with the fixed line: after a
/// protected release (`admission` set) its record carries method 0, since
/// refusing would leave a machine without a working TPM, a FIDO2-primary
/// one included, unbootable, and method 0 admits nothing; a device-bound
/// release, whose selector only phase 1 gives the policy, writes none
/// (td-install/ENCRYPTION.md "Verified account handoff"). That holds when
/// PCR 11 is uncertain too, after an extension whose readback failed.
fn measured_record(
    id: &str,
    measured: bool,
    encrypted: bool,
    admission: Option<admission::Method>,
    measure: impl FnOnce() -> io::Result<[u8; 32]>,
    console: &mut impl Write,
) -> io::Result<Option<[u8; admission::RECORD_BYTES]>> {
    let measured = if !measured {
        false
    } else {
        match measure() {
            Ok(pcr) => {
                writeln!(
                    console,
                    "td-boot: TD-BOOT-MEASURED-PCR11 {}",
                    sha256::to_base16(&pcr)
                )?;
                true
            }
            Err(_) if encrypted || admission.is_some() => {
                let _ = writeln!(console, "{MEASUREMENT_FAILED}");
                false
            }
            Err(error) => return Err(error),
        }
    };
    // Written only now, after the measurement: a release whose deployment
    // was not measured admits nothing, though its record still marks the
    // machine protected.
    admission
        .map(|method| admission_record(id, method, measured))
        .transpose()
}

fn kexec_boot_decision(
    deployment: Deployment,
    decision: &BootDecision,
    base_cmdline: &OsStr,
    measured: bool,
    handoff: Option<Handoff<'_>>,
) -> io::Result<()> {
    report_boot_decision(&mut mirror::stderr(), decision)?;
    let cmdline = kernel_cmdline(
        base_cmdline,
        &decision.deployment_id,
        decision.bookkeeping_error.is_some(),
    )?;
    let record = measured_record(
        &deployment.id,
        measured,
        handoff.is_some(),
        handoff.and_then(|handoff| handoff.admission),
        || measurement::measure(&deployment.id, cmdline.as_bytes()),
        &mut mirror::stderr(),
    )?;
    let Deployment {
        kernel,
        initramfs,
        initramfs_digest,
        ..
    } = deployment;
    // An encrypted volume's key, after the measurement: the whole key and
    // then end of file on a pipe whose write end is closed, which td-kexec
    // opens anew by name and appends to its sealed copy of the initramfs.
    // The record rides a second such pipe.
    let key_pipe = handoff
        .map(|handoff| KeyFile::new(handoff.key.expose()))
        .transpose()?;
    let record_pipe = record.map(|record| KeyFile::new(&record)).transpose()?;
    let mut command = match (&key_pipe, &record_pipe) {
        (None, _) => kexec_command(kernel, initramfs, cmdline.as_os_str()),
        (Some(pipe), None) => key_kexec_command(
            kernel,
            initramfs,
            &initramfs_digest,
            pipe.path(),
            None,
            cmdline.as_os_str(),
        ),
        (Some(pipe), Some(record)) => key_kexec_command(
            kernel,
            initramfs,
            &initramfs_digest,
            pipe.path(),
            Some(record.path()),
            cmdline.as_os_str(),
        ),
    };
    run_command(&mut command, "td-kexec")?;
    Err(io::Error::other(
        "td-kexec returned without booting the verified deployment",
    ))
}

fn run_boot(
    device: &Path,
    mountpoint: &Path,
    base_cmdline: &OsStr,
    handoff: Option<Handoff<'_>>,
    mut commit: Option<&mut dyn FnMut(&Path)>,
) -> io::Result<()> {
    require_absolute(device, "volume device")?;
    require_absolute(mountpoint, "mountpoint")?;
    let measured = measurement::enabled(Path::new("/"))?;
    let (_transaction_lock, _device_lock, device_id) = acquire_update_locks(device)?;
    prepare_update_mountpoint(mountpoint, device, device_id)?;
    run_command(
        &mut mount_command(device, mountpoint),
        "read-only Btrfs mount",
    )?;
    let choice = match menu::choose(mountpoint) {
        Ok(choice) => choice,
        Err(error) => {
            best_effort_unmount(mountpoint);
            return Err(error);
        }
    };
    if let Some(id) = choice {
        if let Some((decision, deployment)) = read_selector(mountpoint, "current")
            .ok()
            .filter(|current| current == &id)
            .and_then(|_| read_only_current(mountpoint))
        {
            if decision.deployment_id == id {
                before_kexec(&mut commit, mountpoint);
                let result =
                    kexec_boot_decision(deployment, &decision, base_cmdline, measured, handoff);
                best_effort_unmount(mountpoint);
                return result;
            }
        }
        let result = run_manual_boot(
            device,
            mountpoint,
            device_id,
            &id,
            base_cmdline,
            measured,
            handoff,
            &mut commit,
        );
        best_effort_unmount(mountpoint);
        return result;
    }
    if let Some((decision, deployment)) = read_only_current(mountpoint) {
        before_kexec(&mut commit, mountpoint);
        let result = kexec_boot_decision(deployment, &decision, base_cmdline, measured, handoff);
        best_effort_unmount(mountpoint);
        return result;
    }
    if let Err(transaction_error) =
        run_command(&mut unmount_command(mountpoint), "read-only Btrfs unmount")
    {
        let result = (|| {
            let (decision, deployment) = read_only_recovery(mountpoint, &transaction_error)?;
            before_kexec(&mut commit, mountpoint);
            kexec_boot_decision(deployment, &decision, base_cmdline, measured, handoff)
        })();
        best_effort_unmount(mountpoint);
        return result;
    }

    let decision = match run_on_prelocked_writable_volume(device, mountpoint, device_id, |root| {
        select_boot_deployment(root)
    }) {
        Ok(decision) => decision,
        Err(WritableVolumeFailure::Committed {
            value: decision,
            unmount_error,
        }) => {
            let result = (|| {
                writeln!(
                    mirror::stderr(),
                    "td-boot: selection committed but Btrfs unmount failed \
                         ({unmount_error}); booting the committed deployment"
                )?;
                let deployment = verify_deployment(mountpoint, &decision.deployment_id)?;
                before_kexec(&mut commit, mountpoint);
                kexec_boot_decision(deployment, &decision, base_cmdline, measured, handoff)
            })();
            best_effort_unmount(mountpoint);
            return result;
        }
        Err(WritableVolumeFailure::Mounted(transaction_error)) => {
            let result = (|| {
                let (decision, deployment) = read_only_recovery(mountpoint, &transaction_error)?;
                before_kexec(&mut commit, mountpoint);
                kexec_boot_decision(deployment, &decision, base_cmdline, measured, handoff)
            })();
            best_effort_unmount(mountpoint);
            return result;
        }
        Err(WritableVolumeFailure::Transaction(transaction_error)) => {
            // Update APIs fail closed on semantic errors; PID 1 still tries the
            // verified payload-only recovery path before giving up.
            best_effort_unmount(mountpoint);
            prepare_update_mountpoint(mountpoint, device, device_id)?;
            run_command(
                &mut mount_command(device, mountpoint),
                "read-only Btrfs recovery mount",
            )?;
            let result = (|| {
                let (decision, deployment) = read_only_recovery(mountpoint, &transaction_error)?;
                before_kexec(&mut commit, mountpoint);
                kexec_boot_decision(deployment, &decision, base_cmdline, measured, handoff)
            })();
            best_effort_unmount(mountpoint);
            return result;
        }
    };

    prepare_update_mountpoint(mountpoint, device, device_id)?;
    run_command(
        &mut mount_command(device, mountpoint),
        "read-only Btrfs mount",
    )?;

    let result = (|| {
        // The writable transaction closed its payload handles before unmounting.
        // Reverify under the mount whose handles are passed to kexec.
        let deployment = verify_deployment(mountpoint, &decision.deployment_id)?;
        before_kexec(&mut commit, mountpoint);
        kexec_boot_decision(deployment, &decision, base_cmdline, measured, handoff)
    })();
    best_effort_unmount(mountpoint);
    result
}

/// Manual selection refuses transaction failure instead of silently booting
/// a different version from the one the operator chose.
#[allow(
    clippy::too_many_arguments,
    reason = "the boot transaction's existing context"
)]
fn run_manual_boot(
    device: &Path,
    mountpoint: &Path,
    device_id: u64,
    id: &str,
    base_cmdline: &OsStr,
    measured: bool,
    handoff: Option<Handoff<'_>>,
    commit: &mut Option<&mut dyn FnMut(&Path)>,
) -> io::Result<()> {
    run_command(
        &mut unmount_command(mountpoint),
        "boot menu read-only unmount",
    )?;
    let (decision, mounted) =
        match run_on_prelocked_writable_volume(device, mountpoint, device_id, |root| {
            menu::select(root, id)
        }) {
            Ok(decision) => (decision, false),
            Err(WritableVolumeFailure::Committed {
                value,
                unmount_error,
            }) => {
                writeln!(
                    mirror::stderr(),
                    "td-boot: manual choice committed; unmount failed ({unmount_error})"
                )?;
                (value, true)
            }
            Err(error) => return Err(error.into_io()),
        };
    if decision.deployment_id != id {
        return Err(invalid("manual boot selected a different deployment"));
    }
    if !mounted {
        prepare_update_mountpoint(mountpoint, device, device_id)?;
        run_command(
            &mut mount_command(device, mountpoint),
            "boot menu read-only mount",
        )?;
    }
    let deployment = verify_deployment(mountpoint, id)?;
    before_kexec(commit, mountpoint);
    kexec_boot_decision(deployment, &decision, base_cmdline, measured, handoff)
}

/// A staged selector update's commit (td-install/ENCRYPTION.md "Selector
/// update"), once, with the volume mounted read-only at `mountpoint` and
/// the deployment to boot already verified: after the release cap,
/// before the kexec, on whichever path `run_boot` took.
fn before_kexec(commit: &mut Option<&mut dyn FnMut(&Path)>, mountpoint: &Path) {
    if let Some(commit) = commit.take() {
        commit(mountpoint);
    }
}

/// Mount the volume and publish `source` into it, printing its id.
fn publish_onto_volume(device: &Path, mountpoint: &Path, source: &Path) -> io::Result<()> {
    require_absolute(source, "deployment source")?;
    let id = run_on_writable_volume(device, mountpoint, |root| install_deployment(root, source))?;
    writeln!(io::stdout(), "{id}")
}

/// Install whatever the channel is offering, if it is offering anything.
///
/// The differences from `install` are all about being run by a TIMER rather
/// than by a person, and each is the same judgement: an updater nobody is
/// watching must not decide for itself that something missing is fine.
///
/// So the CHANNEL must be there and be a directory — pointing this at a typo
/// is a configuration fault, and answering "nothing to do" forever is how that
/// goes unnoticed until an update is urgently needed. The CANDIDATE inside it
/// may be absent, and that alone is the quiet success: it is the ordinary state
/// of a machine that is up to date, and stdout stays empty so a caller can tell
/// "installed" from "nothing offered" without parsing anything.
///
/// The channel is checked BEFORE the candidate is looked for, so a channel
/// that has gone missing or become writable by others is reported on the
/// next timer tick rather than on the first tick that had an update to
/// install. Both paths are required ABSOLUTE, since nothing sets a timer's
/// working directory.
///
/// No signature vouches for a candidate, so who can write it is what does:
/// every directory from the volume root down to the channel, the candidate
/// directory and its four files must be root-owned, and none may be
/// writable by anyone else. A directory above the channel may be if it is
/// sticky, as `/tmp` is; the channel itself may not. The payloads are then
/// hashed as `install` hashes them. Nothing but root can
/// swap or rewrite the candidate between this decision and the publish.
fn run_update(device: &Path, mountpoint: &Path, volume: &Path, channel: &Path) -> io::Result<()> {
    act_on_update(device, mountpoint, update_decision(volume, channel)?)
}

/// What `update` does with its decision: nothing for no candidate,
/// otherwise the publish `install` makes.
fn act_on_update(device: &Path, mountpoint: &Path, decision: Option<PathBuf>) -> io::Result<()> {
    match decision {
        None => Ok(()),
        Some(candidate) => publish_onto_volume(device, mountpoint, &candidate),
    }
}

/// Is `path` inside `volume`?
///
/// A named function for one call, because the rule it encodes is exactly what a
/// plausible edit gets wrong and nothing else could then catch: `starts_with`
/// compares COMPONENTS, and the string-prefix spelling that looks equivalent
/// admits a channel under `/run/tdx` for a volume of `/run/td`. Testing that needs
/// two mount points a test cannot create, so the predicate is tested instead of
/// the path through it.
///
/// It normalizes nothing — `..` is a component like any other — so both
/// arguments must already be resolved.
fn is_within(volume: &Path, path: &Path) -> bool {
    path.starts_with(volume)
}

/// Everything `update` DECIDES, split from what it does so the decision is
/// reachable without a block device.
///
/// The verb cannot be driven end to end in this crate: the transaction lock
/// refuses a character device before any mount, so `/dev/null` never gets far
/// enough to install anything. Review found that this left the whole publish
/// unpinned — replacing it with `Ok(())` was green. What is testable is the
/// decision, and the decision is everything that makes this verb different
/// from `install`.
///
/// `None` is "nothing to do" and means EXACTLY one thing: no candidate. A
/// channel that cannot be read, or one holding something that is not a real
/// directory, is loud. That boundary is the verb.
fn update_decision(volume: &Path, channel: &Path) -> io::Result<Option<PathBuf>> {
    require_absolute(volume, "update volume")?;
    require_absolute(channel, "update channel")?;
    require_real_directory(volume, "update volume")?;
    // BOTH are resolved before either is judged, and the RESOLVED channel is
    // what is read. `starts_with` is lexical — it compares components and
    // normalizes nothing — so `<volume>/../elsewhere` is "under the volume" to
    // it and `/run/elsewhere` to the kernel; an intermediate symlink escapes
    // the same way. Reading the resolved path also stops the check and the
    // read disagreeing.
    let volume = fs::canonicalize(volume).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("update volume {}: {error}", volume.display()),
        )
    })?;
    // DESIGN §10 item 10f. `/init` creates this directory before moving the
    // volume onto it, so a move that never happened leaves an ordinary
    // directory on the `/run` tmpfs — and anything that can write `/run` can
    // then offer a candidate to an unattended updater. A mount point is what
    // tells those apart: a device number that differs from its parent's.
    let mounted = is_mount_point(&volume).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("update volume {}: {error}", volume.display()),
        )
    })?;
    if !mounted {
        return Err(invalid(format!(
            "update volume is not a mount point: {}",
            volume.display()
        )));
    }
    channel_decision(&volume, channel)
}

/// `lstat`, its failure naming the path it was asked about.
fn lstat(path: &Path, label: &str) -> io::Result<Metadata> {
    fs::symlink_metadata(path).map_err(|error| {
        io::Error::new(error.kind(), format!("{label} {}: {error}", path.display()))
    })
}

/// The decision below the mount-point test: `volume` is canonical and
/// mounted, and `channel` is resolved, placed and judged against it.
fn channel_decision(volume: &Path, channel: &Path) -> io::Result<Option<PathBuf>> {
    require_real_directory(channel, "update channel")?;
    let channel = fs::canonicalize(channel).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("update channel {}: {error}", channel.display()),
        )
    })?;
    // ...and the channel must be ON it, or naming a mount point elsewhere
    // would satisfy the check while the candidate came from anywhere at all.
    if !is_within(volume, &channel) {
        return Err(invalid(format!(
            "update channel {} is not on the volume {}",
            channel.display(),
            volume.display()
        )));
    }
    // Every directory from the volume root down, the channel last: a parent
    // others could write lets them swap the whole channel.
    let below = channel.strip_prefix(volume).map_err(|_| {
        invalid(format!(
            "update channel {} is not on the volume",
            channel.display()
        ))
    })?;
    let mut path = volume.to_path_buf();
    for component in std::iter::once(None).chain(below.components().map(Some)) {
        if let Some(component) = component {
            path.push(component);
        }
        let held = lstat(&path, "update channel path")?;
        let mode = held.permissions().mode();
        if !held.file_type().is_dir()
            || !root_owned(&held)
            || (mode & 0o022 != 0 && (path == channel || mode & 0o1000 == 0))
        {
            return Err(invalid(format!(
                "update channel {}: {} must be root-owned and writable by no one else",
                channel.display(),
                path.display()
            )));
        }
    }
    let candidate = channel.join(protocol::CHANNEL_CANDIDATE);
    // ONE lookup rather than an existence test and then a type test: two put the
    // quiet/loud boundary on different instants, so a candidate removed between
    // them reports "no such file" where this verb's rule says absent is quiet.
    // Its owner and mode are judged on the same answer.
    let held = match fs::symlink_metadata(&candidate) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        other => other.map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("update candidate {}: {error}", candidate.display()),
            )
        })?,
    };
    if !held.file_type().is_dir() {
        return Err(invalid(format!(
            "update candidate must be a real directory: {}",
            candidate.display()
        )));
    }
    if !root_owned(&held) || held.permissions().mode() & 0o022 != 0 {
        return Err(invalid(format!(
            "update candidate {} must be root-owned and writable by no one else",
            candidate.display()
        )));
    }
    for name in [
        protocol::MANIFEST_NAME,
        "bzImage",
        "initramfs.cpio",
        "root.erofs",
    ] {
        let path = candidate.join(bundle_name(&candidate, name, Names::Source)?);
        let held = lstat(&path, "update candidate file")?;
        if !held.file_type().is_file()
            || !root_owned(&held)
            || held.permissions().mode() & 0o022 != 0
        {
            return Err(invalid(format!(
                "update candidate file {} must be a root-owned regular file writable by no one else",
                path.display()
            )));
        }
    }
    Ok(Some(candidate))
}

/// Is `path` where a filesystem is mounted?
///
/// `mountpoint(1)`'s test, and for its reason: a mount point's device number
/// differs from its parent's. `/proc` is deliberately not consulted — this runs
/// inside a build sandbox as readily as on a machine, and a check that failed
/// where `/proc/self/mountinfo` is absent would fail exactly where `publish`
/// is meant to be used.
///
/// It misses a BIND mount of a directory on the same filesystem, which shares
/// its parent's device number. That case is not what this guards against: the
/// danger is a live volume with its own filesystem, which always differs.
fn is_mount_point(path: &Path) -> io::Result<bool> {
    use std::os::linux::fs::MetadataExt;
    let here = fs::metadata(path)?;
    let Some(parent) = path.parent() else {
        // No parent means the filesystem root, which is one.
        return Ok(true);
    };
    Ok(fs::metadata(parent)?.st_dev() != here.st_dev())
}

fn run_publish(root: &Path, source: &Path) -> io::Result<()> {
    require_absolute(root, "volume root")?;
    // A MOUNT POINT is refused, and this is what stands in for the transaction
    // locking `install` does. Every other mutating verb goes through
    // `run_on_writable_volume`, which serialises on a lock keyed by the volume's
    // BLOCK DEVICE — a thing this verb does not have and cannot be given, since
    // its whole purpose is a root with no device under it. Rather than publish
    // unserialised onto a volume something else may be mutating, it declines to
    // be pointed at one at all: a mounted root is `install`'s, and what is left
    // is the staging tree this exists for, which by construction has one writer.
    if is_mount_point(root)? {
        return Err(invalid(format!(
            "{} is a mount point; publish is for a staging directory — use `install` \
             for a mounted volume, which takes the update locks",
            root.display()
        )));
    }
    let id = install_deployment(root, source)?;
    writeln!(io::stdout(), "{id}")
}

fn run_rollback(device: &Path, mountpoint: &Path, current: &str, previous: &str) -> io::Result<()> {
    let id = run_on_writable_volume(device, mountpoint, |root| {
        rollback_deployment(root, current, previous)
    })?;
    writeln!(io::stdout(), "{id}")
}

fn run_success(device: &Path, mountpoint: &Path, deployment_id: &str) -> io::Result<()> {
    let cmdline = read_bounded_real_file(
        Path::new("/proc/cmdline"),
        "kernel command line",
        MAX_CMDLINE_BYTES as u64,
    )?;
    match success_disposition(&cmdline, deployment_id) {
        SuccessDisposition::ConfirmRecovery => {
            writeln!(io::stdout(), "{deployment_id}")?;
            return Ok(());
        }
        SuccessDisposition::RejectRecovery => {
            return Err(invalid(format!(
                "recovery command line does not select deployment {deployment_id}"
            )));
        }
        SuccessDisposition::Record => {}
    }
    require_absolute(device, "volume device")?;
    require_absolute(mountpoint, "mountpoint")?;
    let (_transaction_lock, _device_lock, device_id) = acquire_update_locks(device)?;
    prepare_update_mountpoint(mountpoint, device, device_id)?;
    run_command(
        &mut mount_command(device, mountpoint),
        "read-only Btrfs mount",
    )?;
    let state = current_attempt_state(mountpoint, deployment_id);
    let unmounted = run_command(&mut unmount_command(mountpoint), "read-only Btrfs unmount");
    let state = match (state, unmounted) {
        (Ok(state), Ok(())) => state,
        (Err(error), Ok(())) | (Ok(_), Err(error)) => return Err(error),
        (Err(state_error), Err(unmount_error)) => {
            return Err(io::Error::new(
                state_error.kind(),
                format!("{state_error}; additionally {unmount_error}"),
            ));
        }
    };
    if state.is_none() {
        writeln!(io::stdout(), "{deployment_id}")?;
        return Ok(());
    }
    let id = run_on_prelocked_writable_volume(device, mountpoint, device_id, |root| {
        mark_deployment_successful(root, deployment_id)
    })
    .map_err(WritableVolumeFailure::into_io)?;
    writeln!(io::stdout(), "{id}")
}

/// `td-boot validate-source <deployment-directory>`: the source's payloads
/// checked against its manifest, and its id printed.
fn run_validate_source(directory: &Path, out: &mut dyn Write) -> io::Result<()> {
    let bundle = open_bundle(directory, Names::Source)?;
    writeln!(out, "{}", bundle.id)
}

/// The smallest volatile volume a live boot accepts. Btrfs itself needs far
/// less; this leaves room for firstboot state and a session's writes.
const LIVE_MIN_VOLUME_KIB: u64 = 512 * 1024;

/// Half of `MemTotal`, in KiB, for the live volume on /dev/ram0. brd holds
/// only written pages, so this is a ceiling rather than a reservation.
fn live_ram_disk_kib(meminfo: &[u8]) -> io::Result<u64> {
    let text = std::str::from_utf8(meminfo).map_err(|_| invalid("/proc/meminfo is not UTF-8"))?;
    let total = text
        .lines()
        .find_map(|line| line.strip_prefix("MemTotal:"))
        .and_then(|rest| rest.trim().strip_suffix(" kB"))
        .and_then(|kib| kib.trim_end().parse::<u64>().ok())
        .ok_or_else(|| invalid("/proc/meminfo has no MemTotal in kB"))?;
    let half = total / 2;
    if half < LIVE_MIN_VOLUME_KIB {
        return Err(invalid(format!(
            "a live boot needs at least {} MiB of memory, found {} MiB",
            LIVE_MIN_VOLUME_KIB * 2 / 1024,
            total / 1024
        )));
    }
    Ok(half)
}

/// The live handoff: the fresh volume identity, the live token and the RAM
/// disk size, in that order after the selector's own tokens.
fn live_cmdline(
    base: &OsStr,
    uuid: &volume::Uuid,
    ram_disk_kib: u64,
    deployment_id: &str,
) -> io::Result<OsString> {
    let with_volume = volume::command_line(base.as_bytes(), uuid)?;
    handoff_cmdline(
        &with_volume,
        deployment_id,
        false,
        &format!(
            "{} {}{ram_disk_kib} panic=0",
            protocol::LIVE_CMDLINE_TOKEN,
            protocol::RAM_DISK_SIZE_PREFIX
        ),
    )
}

/// The name a deployment file has on a mounted install medium.
fn media_name(deployment_name: &str) -> io::Result<String> {
    protocol::MEDIA_DEPLOYMENT_FILES
        .iter()
        .find(|(_, name)| *name == deployment_name)
        .map(|(iso, _)| iso.to_ascii_lowercase())
        .ok_or_else(|| invalid(format!("{deployment_name} is not an install media file")))
}

fn media_manifest(medium: &Path) -> io::Result<Vec<u8>> {
    read_bounded_real_file(
        &medium.join(media_name(protocol::MANIFEST_NAME)?),
        "install medium manifest",
        protocol::MAX_MANIFEST_BYTES,
    )
}

/// The medium's deployment, its kernel and initramfs checked against its
/// manifest.
///
/// `root.erofs` is not hashed here: `live-root` hashes it after kexec, at the
/// descriptor it binds to the loop. A live boot has no previous deployment to
/// fall back to, so checking it twice would only double the time spent
/// reading the largest file on the medium.
fn verified_media_deployment(medium: &Path) -> io::Result<Deployment> {
    require_real_directory(medium, "install medium")?;
    let manifest = media_manifest(medium)?;
    let parsed = parse_manifest(&manifest)?;
    let kernel = verify_payload(medium, &media_name("bzImage")?, &parsed.kernel)?;
    let initramfs = verify_payload(medium, &media_name("initramfs.cpio")?, &parsed.initramfs)?;
    Ok(Deployment {
        id: sha256::hex_digest(&manifest),
        kernel,
        initramfs,
        initramfs_digest: parsed.initramfs,
    })
}

fn media_mount_command(device: &Path, mountpoint: &Path) -> Command {
    let mut command = Command::new(TD_MOUNT);
    command.args([
        OsStr::new("-t"),
        OsStr::new("iso9660"),
        OsStr::new("-o"),
        OsStr::new("ro,nodev,nosuid,noexec,map=normal"),
        device.as_os_str(),
        mountpoint.as_os_str(),
    ]);
    command
}

/// Find the one install medium and mount it read-only at `mountpoint`.
fn mount_medium(mountpoint: &Path) -> io::Result<()> {
    require_absolute(mountpoint, "mountpoint")?;
    match fs::DirBuilder::new().mode(0o700).create(mountpoint) {
        Err(error) if error.kind() != io::ErrorKind::AlreadyExists => return Err(error),
        _ => require_real_directory(mountpoint, "mountpoint")?,
    }
    let medium = volume::find_medium()?;
    writeln!(
        mirror::stderr(),
        "td-boot: install medium {}",
        medium.device.display()
    )?;
    run_command(
        &mut media_mount_command(&medium.path(), mountpoint),
        "read-only install medium mount",
    )
}

/// The live selector's provisioning, checked.
///
/// A live selector boots whatever deployment a medium carries, so it must be
/// exactly that: the marker, no volume identity (which would name an
/// installed volume it never reads) and no measurement policy (which it never
/// applies). A selector carrying either was built wrong, and says so here
/// rather than booting with a policy silently ignored.
fn check_live_selector(rootfs: &Path) -> io::Result<()> {
    let marker = read_bounded_real_file(
        &rootfs.join(protocol::LIVE_MEDIA_MARKER_PATH),
        "live media marker",
        64,
    )?;
    if marker != protocol::LIVE_MEDIA_MARKER {
        return Err(invalid("live media marker is malformed"));
    }
    for (path, what) in [
        (protocol::VOLUME_UUID_PATH, "an installed volume identity"),
        (
            protocol::MEASUREMENT_POLICY_PATH,
            "a boot measurement policy",
        ),
    ] {
        match fs::symlink_metadata(rootfs.join(path)) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
            Ok(_) => {
                return Err(invalid(format!(
                    "live selector carries {what} ({path}); a live boot applies neither"
                )));
            }
        }
    }
    Ok(())
}

/// Stop on the console until a platform reset. The selector's `/init` execs
/// td-boot, so returning would exit init and panic the kernel, and
/// `panic=-1` would reboot into the same refusal without end.
fn halt(reason: &str) -> ! {
    let _ = writeln!(
        mirror::stderr(),
        "td-boot: {reason}\ntd-boot: halted; only a platform reset leaves this"
    );
    loop {
        std::thread::park();
    }
}

fn run_live_boot(mountpoint: &Path, base_cmdline: &OsStr) -> io::Result<()> {
    check_live_selector(Path::new("/"))?;
    // Before anything reads the medium, so that no live session can release
    // a td disk's protector (MEDIA.md "Live boot" step 0). The console lines
    // are best effort: a failed write must not exit init after a cap. A
    // stuck TPM can hold each command for the kernel's timeout, so the
    // first line says what the console is waiting on.
    let _ = writeln!(mirror::stderr(), "td-boot: capping PCR 12");
    let capped =
        cap::live().unwrap_or_else(|refusal| halt(&format!("{refusal}; live boot refused")));
    let _ = writeln!(mirror::stderr(), "td-boot: {}", capped.describe());
    let ram_disk_kib = live_ram_disk_kib(&read_bounded_real_file(
        Path::new("/proc/meminfo"),
        "memory summary",
        64 * 1024,
    )?)?;
    let _ = writeln!(
        mirror::stderr(),
        "td-boot: live RAM disk {ram_disk_kib} KiB"
    );
    let uuid = volume::Uuid::random()?;
    mount_medium(mountpoint)?;
    let result = (|| {
        let _ = writeln!(
            mirror::stderr(),
            "td-boot: verifying live kernel/initramfs against the manifest"
        );
        let deployment = verified_media_deployment(mountpoint)?;
        let cmdline = live_cmdline(base_cmdline, &uuid, ram_disk_kib, &deployment.id)?;
        writeln!(
            mirror::stderr(),
            "td-boot: live deployment {} volume {uuid} ram-disk-kib {ram_disk_kib}",
            deployment.id
        )?;
        let Deployment {
            kernel, initramfs, ..
        } = deployment;
        run_command(
            &mut kexec_command(kernel, initramfs, cmdline.as_os_str()),
            "td-kexec",
        )?;
        Err(io::Error::other(
            "td-kexec returned without booting the live deployment",
        ))
    })();
    best_effort_unmount(mountpoint);
    result
}

/// The live handoff must name the deployment a live verb was given.
///
/// The deployment init reads its id from the same line, so this is defence in
/// depth: an argv id the selector did not verify is refused.
fn require_live_handoff(cmdline: &[u8], deployment_id: &str) -> io::Result<()> {
    if !cmdline_has_token(cmdline, protocol::LIVE_CMDLINE_TOKEN.as_bytes()) {
        return Err(invalid(format!(
            "the live verbs require the live selector's {} handoff",
            protocol::LIVE_CMDLINE_TOKEN
        )));
    }
    if !cmdline_has_token(cmdline, format!("td.deployment={deployment_id}").as_bytes()) {
        return Err(invalid(format!(
            "a live verb was given deployment {deployment_id}, which the handoff does not name"
        )));
    }
    Ok(())
}

/// The medium's `root.erofs`, open and hashed, if the medium holds exactly
/// the deployment the selector verified.
///
/// The id is the manifest's hash, so a match binds this medium's manifest,
/// and so its root digest, to what was verified before kexec.
fn live_root_payload(medium: &Path, deployment_id: &str) -> io::Result<File> {
    require_real_directory(medium, "install medium")?;
    let manifest = media_manifest(medium)?;
    let id = sha256::hex_digest(&manifest);
    if id != deployment_id {
        return Err(invalid(format!(
            "install medium holds deployment {id}, not the selected {deployment_id}"
        )));
    }
    let parsed = parse_manifest(&manifest)?;
    verify_payload(medium, &media_name("root.erofs")?, &parsed.root)
}

/// Leaves the medium mounted: the loop holds its `root.erofs` open.
fn run_live_root(mountpoint: &Path, deployment_id: &str, loop_device: &Path) -> io::Result<()> {
    require_absolute(loop_device, "loop device")?;
    require_live_handoff(
        &read_bounded_real_file(
            Path::new("/proc/cmdline"),
            "kernel command line",
            MAX_CMDLINE_BYTES as u64,
        )?,
        deployment_id,
    )?;
    mount_medium(mountpoint)?;
    let _ = writeln!(
        mirror::stderr(),
        "td-boot: verifying live root.erofs (this can take time)"
    );
    let result = live_root_payload(mountpoint, deployment_id).and_then(|root| {
        run_command(
            &mut loop_command(root, loop_device),
            "read-only live root loop setup",
        )
    });
    if result.is_err() {
        best_effort_unmount(mountpoint);
    }
    result
}

/// The live volume's contents, staged for `mkfs.btrfs --rootdir`.
///
/// Only what the booted system reads from its volume: `current` naming the
/// running deployment, that deployment's manifest, an empty update channel
/// and the `@var` directory. The payloads stay on the medium, which
/// `live-root` has already bound. Modes are set rather than masked, as
/// td-install's seed does, because `--rootdir` copies them verbatim.
///
/// The medium's manifest must hash to the handed-off id. Everything is read
/// and checked before anything is created, and `/init` has no retry, so a
/// failed write leaving part of a tree is not a case to recover from.
///
/// A server medium's provisioned account is staged too, as an image's
/// volume carries it.
fn live_seed(medium: &Path, deployment_id: &str, seed: &Path) -> io::Result<()> {
    require_absolute(seed, "live seed")?;
    require_real_directory(medium, "install medium")?;
    let manifest = media_manifest(medium)?;
    let id = sha256::hex_digest(&manifest);
    if id != deployment_id {
        return Err(invalid(format!(
            "install medium holds deployment {id}, not the selected {deployment_id}"
        )));
    }
    let provision = media_provision(medium)?;
    let deployment = format!("{}/{id}", protocol::DEPLOYMENTS_DIR);
    // `seed` first and alone may not exist yet: a stale tree would otherwise
    // become the volume's contents.
    for directory in [
        "",
        "td",
        protocol::BOOT_DIR,
        protocol::DEPLOYMENTS_DIR,
        protocol::VOLUME_CHANNEL_DIR,
        &deployment,
        protocol::VOLUME_SUBVOL,
    ] {
        let path = seed.join(directory);
        fs::create_dir(&path)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755))?;
    }
    write_seed_file(
        &seed.join(&deployment).join(protocol::MANIFEST_NAME),
        &manifest,
    )?;
    if let Some(provision) = provision {
        let name = seed.join(protocol::PROVISIONED_NAME_PATH);
        let td = name
            .parent()
            .ok_or_else(|| invalid("the provisioned name has no directory"))?;
        let lib = td
            .parent()
            .ok_or_else(|| invalid("the provisioned name has no directory"))?;
        for directory in [lib, td] {
            fs::create_dir(directory)?;
            fs::set_permissions(directory, fs::Permissions::from_mode(0o755))?;
        }
        write_seed_file(&name, format!("{}\n", provision.name).as_bytes())?;
        write_seed_file(&seed.join(protocol::PROVISIONED_KEYS_PATH), &provision.keys)?;
        writeln!(
            mirror::stderr(),
            "td-boot: live provision for {} ({} key(s))",
            provision.name,
            provision.keys.iter().filter(|byte| **byte == b'\n').count()
        )?;
    }
    symlink(
        format!("{}{id}", protocol::SELECTOR_PREFIX),
        seed.join(protocol::BOOT_DIR).join(protocol::CURRENT_SLOT),
    )
}

fn write_seed_file(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    file.set_permissions(fs::Permissions::from_mode(0o644))
}

/// A provisioned primary account: the name it takes and its key lines.
#[derive(Debug, PartialEq)]
struct Provision {
    name: String,
    keys: Vec<u8>,
}

/// The medium's provision, if it carries one. Like the deployment beside it,
/// nothing signs it: whoever wrote the medium chose both.
fn media_provision(medium: &Path) -> io::Result<Option<Provision>> {
    let path = medium.join(protocol::MEDIA_PROVISION_NAME.to_ascii_lowercase());
    match fs::symlink_metadata(&path) {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(io::Error::new(
                error.kind(),
                format!("{}: {error}", path.display()),
            ))
        }
    }
    let bytes = read_bounded_real_file(
        &path,
        "install medium provision",
        protocol::MAX_PROVISION_BYTES,
    )?;
    parse_provision(&bytes).map(Some)
}

/// `td-provision-v1`, then `user NAME`, then one or more `ssh-ed25519` key
/// lines, every line ending in a newline. The name is held to td-authd's
/// grammar here, so a bad one stops the boot before anything is formatted;
/// td-firstboot also refuses reserved and taken names at boot.
fn parse_provision(bytes: &[u8]) -> io::Result<Provision> {
    let body = std::str::from_utf8(bytes)
        .ok()
        .and_then(|text| text.strip_prefix(protocol::PROVISION_FORMAT))
        .ok_or_else(|| invalid("a provision opens with its format line"))?;
    let mut lines = body
        .strip_suffix('\n')
        .ok_or_else(|| invalid("a provision ends with a newline"))?
        .split('\n');
    let name = lines
        .next()
        .and_then(|line| line.strip_prefix(protocol::PROVISION_USER_PREFIX))
        .filter(|name| {
            (1..=32).contains(&name.len())
                && name
                    .bytes()
                    .next()
                    .is_some_and(|byte| byte.is_ascii_lowercase())
                && name.bytes().all(|byte| {
                    byte.is_ascii_lowercase()
                        || byte.is_ascii_digit()
                        || matches!(byte, b'_' | b'-')
                })
        })
        .ok_or_else(|| invalid("a provision's second line is `user NAME`"))?;
    let mut keys = Vec::new();
    for line in lines {
        let blob = line
            .strip_prefix("ssh-ed25519 ")
            .and_then(|rest| rest.split(' ').next())
            .filter(|blob| {
                !blob.is_empty()
                    && blob
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || b"+/=".contains(&byte))
            });
        if blob.is_none() || !line.bytes().all(|byte| (0x20..0x7f).contains(&byte)) {
            return Err(invalid(
                "a provision's key lines are printable ssh-ed25519 keys",
            ));
        }
        keys.extend_from_slice(line.as_bytes());
        keys.push(b'\n');
    }
    if keys.is_empty() {
        return Err(invalid("a provision names at least one key"));
    }
    Ok(Provision {
        name: name.to_owned(),
        keys,
    })
}

fn run_live_seed(mountpoint: &Path, deployment_id: &str, seed: &Path) -> io::Result<()> {
    let cmdline = read_bounded_real_file(
        Path::new("/proc/cmdline"),
        "kernel command line",
        MAX_CMDLINE_BYTES as u64,
    )?;
    require_live_handoff(&cmdline, deployment_id)?;
    live_seed(mountpoint, deployment_id, seed)
}

fn run() -> io::Result<()> {
    let mode = parse_args(std::env::args_os().skip(1))?;
    if mirrors(&mode) {
        mirror::start(Path::new(console::ACTIVE));
    }
    dispatch(mode)
}

/// The verbs the two initramfs run, whose console lines are mirrored to the
/// VT (td-install/ENCRYPTION.md "Keyboard console"): the selector's
/// `on-volume boot` and `live-boot`, and the deployment initramfs's
/// `on-volume mount-root`, `on-volume mount-var`, `admit`, `root-loop`,
/// `live-root` and `live-seed`. No other run mirrors, so the running system's VT, which
/// the compositor's display covers, gets nothing from td-boot.
fn mirrors(mode: &Mode) -> bool {
    match mode {
        Mode::OnVolume { operation } => {
            matches!(**operation, Mode::Boot { .. } | Mode::MountVolume { .. })
        }
        Mode::LiveBoot { .. }
        | Mode::Admit { .. }
        | Mode::RootLoop { .. }
        | Mode::LiveRoot { .. }
        | Mode::LiveSeed { .. } => true,
        _ => false,
    }
}

fn on_volume(mut operation: Mode) -> io::Result<()> {
    // mount-root takes the selector's handed-off volume key first, removing
    // it and the kernel's initrd.image copy on every path, a refusal's
    // included (td-install/ENCRYPTION.md "Boot and authority boundaries").
    // No other operation reads or removes it.
    let key = match &operation {
        Mode::MountVolume { var: false, .. } => unlock::take_key(Path::new("/"), 0)?,
        _ => None,
    };
    let uuid = if let Mode::Boot { cmdline, .. } = &mut operation {
        let bytes = read_bounded_real_file(
            &Path::new("/").join(protocol::VOLUME_UUID_PATH),
            "selector volume UUID",
            37,
        )?;
        let uuid = volume::configured(&bytes)?;
        *cmdline = volume::command_line(cmdline.as_bytes(), &uuid)?;
        uuid
    } else {
        volume::handoff(&read_bounded_real_file(
            Path::new("/proc/cmdline"),
            "kernel command line",
            MAX_CMDLINE_BYTES as u64,
        )?)?
    };
    let opened = volume::Opened::open(&uuid)?;
    // Only the installed selector's boot of a td LUKS2 volume releases; a
    // Btrfs volume's boot and every other operation bind as before.
    let (mut operation, opened) = match (operation, opened) {
        (
            Mode::Boot {
                mountpoint,
                cmdline,
                ..
            },
            volume::Opened::Luks2 { partition, mapping },
        ) => return boot_encrypted(&uuid, partition, mapping, &mountpoint, &cmdline),
        other => other,
    };
    let binding = bind_volume(&mut operation, opened, key, |partition, key| {
        unlock::unlock(
            &mut unlock::System::new(partition, &uuid)?,
            &partition.path(),
            key,
            &mut mirror::stderr(),
        )
    })?;
    let pinned = match binding {
        Binding::Bound(pinned) => pinned,
        Binding::Halt(reason) => halt(&reason),
    };
    report_volume_binding(&mut mirror::stderr(), &uuid, &pinned.device)?;
    dispatch(operation)
}

/// The installed selector on a td LUKS2 volume (td-install/ENCRYPTION.md
/// "Selector release"). The release order runs on the pinned partition
/// before any mount or deployment selection: td's bounded header read, the
/// TPM wait, then td-protector's release with its cap. The released secret
/// opens the mapping, which is admitted as discovery admits one and held,
/// with the partition, until kexec. Selection, fallback, boot attempts and
/// the PCR 11 measurement are `run_boot`'s, unchanged, over the mapping; the
/// volume key reaches only the handoff. A refusal halts: the selector is
/// init, and exiting would reboot into the same refusal.
fn boot_encrypted(
    uuid: &volume::Uuid,
    partition: volume::Pinned,
    mapping: Option<volume::Mapping>,
    mountpoint: &Path,
    cmdline: &OsStr,
) -> io::Result<()> {
    report_volume_binding(&mut mirror::stderr(), uuid, &partition.device)?;
    if let Some(mapping) = mapping {
        halt(&format!(
            "td volume {uuid} already has an active mapping, {}, before the selector's \
             release; boot refused",
            mapping.describe()
        ));
    }
    // Best effort: a failed console write must not exit init after a cap.
    let mut console = |line: &str| {
        let _ = writeln!(mirror::stderr(), "td-boot: {line}");
    };
    // Step 1, before any C parser.
    let header = td_protector::luks2::read(&mut partition.file());
    // Whatever the header holds: a corrupted or token-stripped header must
    // not skip the wait and leave PCR 12 open to a late-probing TPM.
    if !selector_release::tpm_present() {
        console(&format!(
            "waiting up to {} s for {}",
            selector_release::TPM_WAIT.as_secs(),
            unlock::TPM_DEVICE
        ));
    }
    let present =
        selector_release::wait_for_tpm(&mut selector_release::tpm_present, &mut std::thread::sleep);
    if !present {
        console(&format!(
            "no TPM device ({}): no TPM protector can release and PCR 12 stays open; a TPM \
             the kernel exposes later on this boot finds it open",
            unlock::TPM_DEVICE
        ));
    }
    let mut tpm = present.then_some(DeviceTpm);
    let mut cryptsetup = Luks::new(Path::new("/bin").join(protocol::TD_LUKS));
    let mut line = selector_release::Applet(Path::new("/bin").join(protocol::SECRET_LINE_APPLET));
    // A protected header's security keys, one FIDO2 client per Enter.
    let mut keys = selector_release::SecurityKeys::root();
    // A staged selector update, read and predicted after a TPM PIN release
    // and before the cap (td-install/ENCRYPTION.md "Selector update").
    let mut machine = selector_update::Selector {
        partition: &partition,
        uuid: uuid.to_string(),
    };
    let mut staged = None;
    // The header read again, and an upgrade boot's power supply and
    // resilience, each when the release asks.
    let mut read = || td_protector::luks2::read_upgrade(&mut partition.file());
    let mut on_mains = || selector_update::on_mains(Path::new(POWER_SUPPLY_CLASS));
    let mut resilience = || selector_update::resilience(&partition);
    let mut volume = selector_release::Volume {
        read: &mut read,
        on_mains: &mut on_mains,
        resilience: &mut resilience,
    };
    let (keyslot, key, admission, mut predicted) = match selector_release::unlock(
        &header,
        &partition.path(),
        tpm.as_mut(),
        &mut cryptsetup,
        &mut line,
        &mut keys,
        &mut |console: &mut dyn FnMut(&str)| {
            staged = selector_update::preseal(&mut machine, console);
            staged.as_ref().and_then(selector_update::Staged::predicted)
        },
        Path::new(selector_release::KEY_DIRECTORY),
        &mut volume,
        &mut console,
    ) {
        selector_release::Unlock::Boot {
            keyslot,
            key,
            admission,
            predicted,
        } => (keyslot, key, admission, predicted),
        selector_release::Unlock::Halt(reason) => halt(&reason),
        selector_release::Unlock::Failed(error) => return Err(error),
    };
    let mapping = volume::admit_mapping(&partition, uuid, selector_release::MAPPING_NAME)?;
    console(&format!(
        "volume {uuid} opened with keyslot {keyslot} as {}",
        mapping.describe()
    ));
    // Its commit, once `run_boot` has the volume mounted read-only, and only
    // after the TPM PIN released: a PIN whose keyslot did not open is no
    // owner's.
    let mut update = match staged {
        Some(staged) if admission == Some(admission::Method::TpmPin) => Some(staged),
        Some(_) => {
            console("the staged selector update waits for a TPM PIN boot");
            None
        }
        None => None,
    };
    let mut commit = |root: &Path| {
        let Some(staged) = update.take() else {
            return;
        };
        let mut prompt = selector_release::Lines(&mut line);
        let committed = selector_update::commit(
            staged,
            predicted.take(),
            root,
            &mut machine,
            &mut cryptsetup,
            &partition.path(),
            key.expose(),
            &mut prompt,
            &mut console,
        );
        if committed == selector_update::Outcome::Restart {
            halt(&selector_update::restart());
        }
    };
    run_boot(
        &mapping.path(),
        mountpoint,
        cmdline,
        Some(Handoff {
            key: &key,
            admission,
        }),
        Some(&mut commit),
    )
}

/// The kernel's power supplies, read for the upgrade's mains check.
const POWER_SUPPLY_CLASS: &str = "/sys/class/power_supply";

/// What `bind_volume` decided.
enum Binding {
    /// The operation's device, held.
    Bound(volume::Pinned),
    /// The post-cap check refused the boot: halt, never exit init.
    Halt(String),
}

/// Who consumes the volume `bind_volume` binds.
enum Consumer {
    /// The selector's `boot`.
    Selector,
    /// The deployment initramfs's `mount-root` (false) or `mount-var` (true).
    Mount { var: bool },
    /// The running system's `install`, `update`, `rollback` and `success`.
    Running,
}

/// Gives the operation the pinned volume as its device operand. A Btrfs
/// volume binds as before, and refuses a handed-off key. A td LUKS2
/// volume binds through its mapping: `mount-root` requires the key and no
/// active mapping, and binds the mapping `unlock` opens; `mount-var` and
/// the running system's operations bind the `td-system` mapping
/// `mount-root` opened, by its held descriptor, and refuse without it.
/// The installed selector's boot of one never reaches here
/// (`boot_encrypted`), and is refused if it does.
fn bind_volume(
    operation: &mut Mode,
    opened: volume::Opened,
    key: Option<unlock::VolumeKey>,
    unlock: impl FnOnce(&volume::Pinned, unlock::VolumeKey) -> io::Result<unlock::Unlocked>,
) -> io::Result<Binding> {
    let (device, consumer) = match operation {
        Mode::MountVolume { device, var, .. } => (device, Consumer::Mount { var: *var }),
        Mode::Boot { device, .. } => (device, Consumer::Selector),
        Mode::Install { device, .. }
        | Mode::Update { device, .. }
        | Mode::Rollback { device, .. }
        | Mode::Success { device, .. } => (device, Consumer::Running),
        _ => return Err(usage_error()),
    };
    let pinned = match opened {
        volume::Opened::Btrfs(pinned) => {
            if key.is_some() {
                return Err(invalid(format!(
                    "a volume key was handed off for {}, which is not encrypted",
                    pinned.device.display()
                )));
            }
            pinned
        }
        volume::Opened::Luks2 { partition, mapping } => match consumer {
            Consumer::Mount { var: false } => {
                let Some(key) = key else {
                    return Err(invalid(format!(
                        "{} holds a td LUKS2 volume and no volume key was handed off",
                        partition.device.display()
                    )));
                };
                if let Some(mapping) = mapping {
                    return Err(invalid(format!(
                        "{} already has the active mapping {}; the deployment initramfs \
                         opens the volume itself",
                        partition.device.display(),
                        mapping.name()
                    )));
                }
                match unlock(&partition, key)? {
                    unlock::Unlocked::Mapping(mapping) => {
                        mapping.into_pinned(partition.device.clone())
                    }
                    unlock::Unlocked::Halt(reason) => return Ok(Binding::Halt(reason)),
                }
            }
            Consumer::Mount { var: true } | Consumer::Running => {
                volume::system_mapping(&partition, mapping)?.into_pinned(partition.device.clone())
            }
            Consumer::Selector => {
                return Err(invalid(format!(
                    "{} holds a td LUKS2 volume, which the selector boots only through \
                     its release",
                    partition.device.display()
                )))
            }
        },
    };
    *device = pinned.path();
    Ok(Binding::Bound(pinned))
}

/// `admit RUN`: the deployment init runs it right after mounting the
/// system's `/run` tmpfs at `RUN`; its failure, which writes nothing, is
/// no refusal (the init line ends in `|| :`).
fn run_admit(run: &Path) -> io::Result<()> {
    require_absolute(run, "run directory")?;
    if admit::admit(Path::new("/"), run, admit::Owner::ROOT)? {
        writeln!(
            mirror::stderr(),
            "td-boot: admission record moved to {}",
            run.join(admission::DIRECTORY)
                .join(admission::RECORD)
                .display()
        )?;
    }
    Ok(())
}

fn mount_volume(device: &Path, mountpoint: &Path, var: bool) -> io::Result<()> {
    require_absolute(device, "volume device")?;
    require_absolute(mountpoint, "mountpoint")?;
    let options = if var {
        "rw,nodev,nosuid,subvol=@var"
    } else {
        "ro,nodev,nosuid,noexec"
    };
    run_command(
        &mut btrfs_mount_command(device, mountpoint, options),
        "Btrfs volume mount",
    )
}

/// The verb table, split from `run` so a test can reach it without argv.
fn dispatch(mode: Mode) -> io::Result<()> {
    match mode {
        Mode::Initramfs => run_initramfs(),
        Mode::FidoWorker { index, inode, rdev } => {
            fido::worker(&index, &inode, &rdev).map_err(io::Error::other)
        }
        Mode::OnVolume { operation } => on_volume(*operation),
        Mode::Admit { run } => run_admit(&run),
        Mode::MountVolume {
            device,
            mountpoint,
            var,
        } => mount_volume(&device, &mountpoint, var),
        Mode::Volume { uuid } => {
            let line = volume::resolve(uuid.as_ref())?.describe()?;
            writeln!(io::stdout(), "{line}")
        }
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
        } => run_boot(&device, &mountpoint, &cmdline, None, None),
        Mode::Install {
            device,
            mountpoint,
            source,
        } => publish_onto_volume(&device, &mountpoint, &source),
        Mode::Reinstall {
            device,
            mountpoint,
            source,
        } => {
            // Admit the complete replacement before mounting writable. Recovery
            // replaces both boot selections; it never formats or resets @var.
            let _ = open_bundle(&source, Names::Source)?;
            let id = run_on_writable_volume(&device, &mountpoint, |root| {
                install_deployment_mode(root, &source, true)
            })?;
            writeln!(io::stdout(), "{id}")
        }
        Mode::Publish { root, source } => run_publish(&root, &source),
        Mode::Update {
            device,
            mountpoint,
            volume,
            channel,
        } => run_update(&device, &mountpoint, &volume, &channel),
        Mode::Rollback {
            device,
            mountpoint,
            current,
            previous,
        } => run_rollback(&device, &mountpoint, &current, &previous),
        Mode::Success {
            device,
            mountpoint,
            deployment_id,
        } => run_success(&device, &mountpoint, &deployment_id),
        Mode::ValidateSource { directory } => run_validate_source(&directory, &mut io::stdout()),
        Mode::LiveBoot {
            mountpoint,
            cmdline,
        } => run_live_boot(&mountpoint, &cmdline),
        Mode::LiveRoot {
            mountpoint,
            deployment_id,
            loop_device,
        } => run_live_root(&mountpoint, &deployment_id, &loop_device),
        Mode::LiveSeed {
            mountpoint,
            deployment_id,
            seed,
        } => run_live_seed(&mountpoint, &deployment_id, &seed),
    }
}

fn live_diagnostics(marker_present: bool, cmdline: &[u8]) -> bool {
    marker_present || cmdline_has_token(cmdline, b"td.live=1")
}

/// Probe the cpio console before giving it to the shell and every child.
/// Merely opening a registered serial console does not prove it accepts writes.
fn select_initramfs_output<W: Write>(
    console: io::Result<W>,
    kernel_log: impl FnOnce() -> io::Result<W>,
) -> io::Result<(W, bool)> {
    let error = match console.and_then(|mut output| {
        output.write_all(b"td-boot: initramfs output on /dev/console\n")?;
        output.flush()?;
        Ok(output)
    }) {
        Ok(output) => return Ok((output, false)),
        Err(error) => error,
    };
    let mut output = kernel_log().map_err(|fallback| {
        io::Error::new(
            fallback.kind(),
            format!("initramfs console failed: {error}; open kernel log: {fallback}"),
        )
    })?;
    let notice = format!(
        "td-boot: initramfs /dev/console failed: {error}; stdout and stderr on /dev/kmsg\n"
    );
    output.write_all(notice.as_bytes())?;
    output.flush()?;
    Ok((output, true))
}

/// The /init interpreter mounts the API filesystems before entering the shell.
/// Live boots send shell errors and traces through printk to both consoles.
fn run_initramfs() -> io::Result<()> {
    use std::os::unix::process::CommandExt;

    if std::process::id() != 1 {
        return Err(invalid("initramfs entry requires PID 1"));
    }
    // Retained output must use cpio nodes, never pin devtmpfs at its unmount.
    let kernel_log = OpenOptions::new().write(true).open("/dev/kmsg");
    let mut selected_output = None;
    let mut live = Path::new("/etc/td/live-media").exists();
    let result = (|| {
        let (output, kernel_output) = select_initramfs_output(
            OpenOptions::new()
                .write(true)
                .custom_flags(0o400) // O_NOCTTY: PID 1 must not acquire a terminal.
                .open("/dev/console"),
            || match &kernel_log {
                Ok(log) => log.try_clone(),
                Err(error) => Err(io::Error::new(error.kind(), error.to_string())),
            },
        )?;
        let output = selected_output.insert(output);
        for (kind, source, target) in [
            ("devtmpfs", "dev", "/dev"),
            ("proc", "proc", "/proc"),
            ("sysfs", "sysfs", "/sys"),
        ] {
            run_command(
                Command::new(TD_MOUNT)
                    .args(["-t", kind, source, target])
                    .stdout(Stdio::from(output.try_clone()?))
                    .stderr(Stdio::from(output.try_clone()?)),
                &format!("initramfs mount {target}"),
            )?;
            if target == "/proc" {
                let cmdline = read_bounded_real_file(
                    Path::new("/proc/cmdline"),
                    "kernel command line",
                    MAX_CMDLINE_BYTES as u64,
                )?;
                live = live_diagnostics(live, &cmdline);
                if live {
                    fs::write("/proc/sys/kernel/panic", "0\n")?;
                }
                if live || kernel_output {
                    for (path, value) in [
                        ("/proc/sys/kernel/printk", "7\n"),
                        ("/proc/sys/kernel/printk_devkmsg", "on\n"),
                    ] {
                        fs::write(path, value).map_err(|e| {
                            io::Error::new(
                                e.kind(),
                                format!("configure initramfs diagnostics {path}: {e}"),
                            )
                        })?;
                    }
                }
            }
        }
        let mut shell = Command::new("/bin/sh");
        shell
            .stdout(Stdio::from(output.try_clone()?))
            .stderr(Stdio::from(output.try_clone()?));
        if live {
            let log = kernel_log
                .as_ref()
                .map_err(|e| io::Error::new(e.kind(), format!("open initramfs /dev/kmsg: {e}")))?;
            let mut banner = log;
            writeln!(
                banner,
                "td-init: live initramfs diagnostics; panic reboot disabled; shell trace follows"
            )?;
            shell.arg("-x").stderr(Stdio::from(log.try_clone()?));
        }
        // exec retains PID 1 for both the selector's kexec and switch_root.
        Err(shell.arg("/init").exec())
    })();
    if let (Err(error), Some(output)) = (&result, &mut selected_output) {
        let _ = writeln!(output, "td-boot: initramfs setup failed: {error}");
    }
    if live {
        if let Err(error) = &result {
            if let Ok(mut log) = kernel_log.as_ref() {
                let _ = writeln!(
                    log,
                    "td-boot: initramfs setup failed: {error}; halted; restart manually"
                );
            }
            halt(&format!("initramfs setup failed: {error}"));
        }
    }
    result
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let _ = writeln!(mirror::stderr(), "td-boot: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn initramfs_output_is_probed_and_the_fallback_reaches_children() {
        let dir = test_temp::TempDir::new("td-initramfs-output").unwrap();
        let console_path = dir.path().join("console");
        let log_path = dir.path().join("kernel-log");
        let (output, fallback) =
            select_initramfs_output(File::create(&console_path), || -> io::Result<File> {
                panic!("healthy console must not use kernel log")
            })
            .unwrap();
        assert!(!fallback);
        drop(output);
        assert!(fs::read_to_string(&console_path)
            .unwrap()
            .contains("initramfs output on /dev/console"));

        // /dev/full accepts an open but refuses output, like the absent UART.
        let (output, fallback) =
            select_initramfs_output(OpenOptions::new().write(true).open("/dev/full"), || {
                File::create(&log_path)
            })
            .unwrap();
        assert!(fallback);
        let status = Command::new("/bin/sh")
            .args([
                "-ec",
                "printf 'child stdout\n'; printf 'child stderr\n' >&2",
            ])
            .stdout(Stdio::from(output.try_clone().unwrap()))
            .stderr(Stdio::from(output))
            .status()
            .unwrap();
        assert!(status.success());
        let log = fs::read_to_string(&log_path).unwrap();
        assert!(log.contains("stdout and stderr on /dev/kmsg"));
        assert!(log.contains("child stdout\nchild stderr\n"));
    }

    #[test]
    fn initramfs_output_rejects_open_write_and_flush_failures() {
        struct Sink {
            fails_flush: bool,
        }
        impl Write for Sink {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                if self.fails_flush {
                    Err(io::Error::from_raw_os_error(5))
                } else {
                    Ok(())
                }
            }
        }
        for console in [
            Err(io::Error::from_raw_os_error(6)),
            Ok(Sink { fails_flush: true }),
        ] {
            let (_, fallback) =
                select_initramfs_output(console, || Ok(Sink { fails_flush: false })).unwrap();
            assert!(fallback);
        }
        let error = select_initramfs_output::<Sink>(Err(io::Error::from_raw_os_error(5)), || {
            Err(io::Error::from_raw_os_error(2))
        })
        .err()
        .unwrap();
        assert!(error.to_string().contains("console failed"));
        assert!(error.to_string().contains("open kernel log"));
        assert!(
            select_initramfs_output(Err(io::Error::from_raw_os_error(5)), || Ok(Sink {
                fails_flush: true
            }),)
            .is_err()
        );
    }

    #[test]
    fn initramfs_entry_and_live_diagnostics_are_narrow() {
        assert!(matches!(
            parse_args(["initramfs", "/init"].map(OsString::from).into_iter()).unwrap(),
            Mode::Initramfs
        ));
        for args in [
            vec!["initramfs"],
            vec!["initramfs", "/other"],
            vec!["initramfs", "/init", "extra"],
        ] {
            assert!(parse_args(args.into_iter().map(OsString::from)).is_err());
        }
        assert!(run_initramfs()
            .unwrap_err()
            .to_string()
            .contains("requires PID 1"));
        assert!(live_diagnostics(true, b""));
        assert!(live_diagnostics(false, b"console=ttyS0 td.live=1\n"));
        for line in [
            b"td.live=10".as_slice(),
            b"other=td.live=1",
            b"td.live=0",
            b"",
        ] {
            assert!(!live_diagnostics(false, line));
        }
    }

    #[test]
    fn initramfs_mounts_apis_before_exec_and_preserves_pid_one() {
        let source = include_str!("main.rs");
        let body = source
            .split("fn run_initramfs() -> io::Result<()> {")
            .nth(1)
            .unwrap()
            .split("fn main()")
            .next()
            .unwrap();
        assert!(
            body.find("requires PID 1").unwrap() < body.find("Command::new(TD_MOUNT)").unwrap()
        );
        assert!(
            body.find("(\"devtmpfs\", \"dev\", \"/dev\")").unwrap()
                < body.find("(\"proc\", \"proc\", \"/proc\")").unwrap()
        );
        assert!(body.contains("(\"sysfs\", \"sysfs\", \"/sys\")"));
        assert!(
            body.find("/proc/sys/kernel/panic").unwrap()
                < body.find("shell.arg(\"/init\").exec()").unwrap()
        );
        assert!(body.contains("shell.arg(\"-x\").stderr(Stdio::from(log.try_clone()?))"));
        assert!(
            body.find("select_initramfs_output(").unwrap()
                < body.find("Command::new(TD_MOUNT)").unwrap()
        );
        assert_eq!(
            body.matches(".stdout(Stdio::from(output.try_clone()?))")
                .count(),
            2
        );
        assert_eq!(
            body.matches(".stderr(Stdio::from(output.try_clone()?))")
                .count(),
            2
        );
        assert!(body.contains("if live || kernel_output"));
        assert!(
            body.find("/etc/td/live-media").unwrap()
                < body.find("select_initramfs_output(").unwrap()
        );
        assert!(
            body.find("let result = (|| {").unwrap()
                < body.find("select_initramfs_output(").unwrap()
        );
        assert!(body.contains("halt(&format!(\"initramfs setup failed: {error}\"))"));
        assert!(
            body.find("open(\"/dev/kmsg\")").unwrap()
                < body.find("Command::new(TD_MOUNT)").unwrap()
        );
    }

    #[derive(Default)]
    struct DiagnosticWrites {
        writes: Vec<Vec<u8>>,
        fail_after: Option<usize>,
    }

    impl Write for DiagnosticWrites {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if self.fail_after == Some(self.writes.len()) {
                return Err(io::Error::from(io::ErrorKind::BrokenPipe));
            }
            self.writes.push(bytes.to_vec());
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// The initramfs verbs mirror their console lines to the VT; nothing
    /// the running system runs does.
    #[test]
    fn only_the_initramfs_verbs_mirror() {
        let id = "a".repeat(64);
        for (argv, want) in [
            (vec!["on-volume", "boot", "/volume", "quiet"], true),
            (vec!["on-volume", "mount-root", "/volume"], true),
            (vec!["on-volume", "mount-var", "/sysroot/var"], true),
            (vec!["root-loop", "/volume", &id, "/dev/loop0"], true),
            (vec!["live-boot", "/media", "quiet"], true),
            (vec!["live-root", "/media", &id, "/dev/loop0"], true),
            (vec!["live-seed", "/media", &id, "/live-seed"], true),
            (vec!["on-volume", "success", "/run/td-update", &id], false),
            (
                vec![
                    "on-volume",
                    "rollback",
                    "/run/td-update",
                    &id,
                    &"b".repeat(64),
                ],
                false,
            ),
            (vec!["boot", "/dev/vda", "/volume", "quiet"], false),
            (vec!["mount-root", "/dev/vda", "/volume"], false),
            (vec!["verify", "/volume"], false),
            (vec!["volume"], false),
        ] {
            let mode = parse_args(args(&argv)).unwrap();
            assert_eq!(mirrors(&mode), want, "{argv:?}");
        }
    }

    #[test]
    fn volume_and_selection_diagnostics_offer_complete_records_to_the_writer() {
        let mut output = DiagnosticWrites::default();
        let uuid = volume::Uuid::parse("00112233-4455-6677-8899-aabbccddeeff").unwrap();
        report_volume_binding(&mut output, &uuid, Path::new("/dev/nvme1n1p2")).unwrap();
        let decision = BootDecision {
            slot: "current",
            deployment_id: "deployment".into(),
            current_error: None,
            exhausted_deployment: None,
            fallback_error: None,
            bookkeeping_error: None,
            remaining_attempts: Some(2),
        };
        report_boot_decision(&mut output, &decision).unwrap();
        assert_eq!(
            output.writes,
            [
                format!("TD-BOOT-VOLUME {uuid} /dev/nvme1n1p2\n").into_bytes(),
                b"TD-BOOT-ATTEMPT-CONSUMED deployment remaining=2\n".to_vec(),
                b"TD-BOOT-SELECTED-CURRENT deployment\n".to_vec(),
            ]
        );
        output.writes.clear();
        let decision = BootDecision {
            slot: "previous",
            deployment_id: "deployment".into(),
            current_error: Some("hash mismatch".into()),
            exhausted_deployment: Some("old".into()),
            fallback_error: None,
            bookkeeping_error: Some("read-only state".into()),
            remaining_attempts: None,
        };
        report_boot_decision(&mut output, &decision).unwrap();
        assert_eq!(output.writes, [
            b"TD-BOOT-BOOKKEEPING-UNAVAILABLE: read-only state\n".to_vec(),
            b"TD-BOOT-CURRENT-REJECTED: current rejected (hash mismatch); using previous deployment\n".to_vec(),
            b"TD-BOOT-ATTEMPTS-EXHAUSTED old -> deployment\n".to_vec(),
            b"TD-BOOT-SELECTED-PREVIOUS deployment\n".to_vec(),
        ]);
        output.writes.clear();
        let decision = BootDecision {
            slot: "current",
            deployment_id: "deployment".into(),
            current_error: None,
            exhausted_deployment: Some("old".into()),
            fallback_error: Some("missing".into()),
            bookkeeping_error: None,
            remaining_attempts: None,
        };
        report_boot_decision(&mut output, &decision).unwrap();
        assert_eq!(output.writes, [
            b"TD-BOOT-ATTEMPTS-EXHAUSTED old: previous unavailable (missing); retrying deployment\n".to_vec(),
            b"TD-BOOT-SELECTED-CURRENT deployment\n".to_vec(),
        ]);
    }

    #[test]
    fn verify_fallback_diagnostic_is_complete_and_propagates_failure() {
        let fixture = Fixture::new();
        let id = fixture.valid_deployment();
        fixture.selector("previous", &id);
        let mut selection = select_deployment(&fixture.root).unwrap();
        selection.current_error = Some("hash mismatch".into());
        let mut output = DiagnosticWrites::default();
        report_fallback(&mut output, &selection).unwrap();
        assert_eq!(
            output.writes,
            [format!(
                "TD-BOOT-CURRENT-REJECTED: current rejected (hash mismatch); using previous {id}\n"
            )
            .into_bytes()]
        );
        let mut failed = DiagnosticWrites {
            writes: Vec::new(),
            fail_after: Some(0),
        };
        assert_eq!(
            report_fallback(&mut failed, &selection).unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
        assert!(failed.writes.is_empty());
        selection.current_error = None;
        report_fallback(&mut failed, &selection).unwrap();
        assert!(failed.writes.is_empty());
    }

    #[test]
    fn diagnostic_failures_stop_before_later_selection_evidence() {
        let uuid = volume::Uuid::parse("00112233-4455-6677-8899-aabbccddeeff").unwrap();
        let decision = BootDecision {
            slot: "current",
            deployment_id: "deployment".into(),
            current_error: None,
            exhausted_deployment: None,
            fallback_error: None,
            bookkeeping_error: None,
            remaining_attempts: Some(2),
        };
        for fail_after in [0, 1] {
            let mut output = DiagnosticWrites {
                writes: Vec::new(),
                fail_after: Some(fail_after),
            };
            assert_eq!(
                report_boot_decision(&mut output, &decision)
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::BrokenPipe
            );
            assert_eq!(output.writes.len(), fail_after);
            assert!(!output
                .writes
                .iter()
                .any(|bytes| bytes.starts_with(b"TD-BOOT-SELECTED")));
        }
        let mut output = DiagnosticWrites {
            writes: Vec::new(),
            fail_after: Some(0),
        };
        assert_eq!(
            report_volume_binding(&mut output, &uuid, Path::new("/dev/vda2"))
                .unwrap_err()
                .kind(),
            io::ErrorKind::BrokenPipe
        );
        assert!(output.writes.is_empty());
    }

    #[test]
    fn diagnostic_short_writes_are_retried_until_complete() {
        let uuid = volume::Uuid::parse("00112233-4455-6677-8899-aabbccddeeff").unwrap();
        struct ShortWrites(Vec<u8>);
        impl Write for ShortWrites {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                let taken = bytes.len().min(3);
                self.0.extend_from_slice(bytes.get(..taken).unwrap());
                Ok(taken)
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let mut short = ShortWrites(Vec::new());
        report_volume_binding(&mut short, &uuid, Path::new("/dev/vda2")).unwrap();
        assert_eq!(
            short.0,
            b"TD-BOOT-VOLUME 00112233-4455-6677-8899-aabbccddeeff /dev/vda2\n"
        );
    }

    use crate::fixture;

    /// The three payloads a fixture deployment carries, keyed by tag.
    fn fixture_payloads(tag: &str) -> [(&'static str, String); 3] {
        fixture::payloads(tag)
    }

    /// The manifest those payloads hash to, under THIS crate's SHA-256.
    fn fixture_manifest(tag: &str) -> String {
        fixture::manifest(tag, sha256::hex_digest)
    }

    #[test]
    fn reinstall_repairs_same_id_payloads_and_preserves_home_and_settings() {
        use std::os::unix::fs::MetadataExt;
        let temp = test_temp::TempDir::new("td-reinstall").unwrap();
        let fixture = Fixture {
            root: temp.path().to_path_buf(),
            _guard: temp,
        };
        fs::create_dir_all(fixture.root.join("td/boot")).unwrap();
        fs::create_dir_all(fixture.root.join("td/deployments")).unwrap();
        fs::create_dir_all(fixture.root.join("@var/home/alice")).unwrap();
        fs::create_dir_all(fixture.root.join("@var/lib/td")).unwrap();
        let home = fixture.root.join("@var/home/alice/keep");
        let settings = fixture.root.join("@var/lib/td/username");
        fs::write(&home, b"irreplaceable personal bytes").unwrap();
        fs::write(&settings, b"alice\n").unwrap();
        let inode = fs::metadata(&home).unwrap().ino();
        let id = fixture.valid_deployment();
        fixture.selector("current", &id);
        fixture.selector("previous", &id);
        let (source, same_id) = fixture.source_bundle("replacement", "");
        assert_eq!(id, same_id);
        let payload = fixture
            .root
            .join(protocol::DEPLOYMENTS_DIR)
            .join(&id)
            .join("root.erofs");
        fs::write(&payload, b"damaged system").unwrap();
        // Ordinary updates refuse damaged predecessors; recovery is explicit.
        assert!(install_deployment(&fixture.root, &source).is_err());
        assert_eq!(fs::read(&payload).unwrap(), b"damaged system");
        let attempts = attempts_directory(&fixture.root, true).unwrap().unwrap();
        fs::write(attempts.join(&id), b"damaged attempt state").unwrap();
        for _ in 0..2 {
            assert_eq!(
                install_deployment_mode(&fixture.root, &source, true).unwrap(),
                id
            );
            assert_eq!(verify_slot(&fixture.root, "current").unwrap().id, id);
            assert_eq!(verify_slot(&fixture.root, "previous").unwrap().id, id);
            assert_eq!(fs::metadata(&home).unwrap().ino(), inode);
            assert_eq!(fs::read(&home).unwrap(), b"irreplaceable personal bytes");
            assert_eq!(fs::read(&settings).unwrap(), b"alice\n");
        }
    }

    struct Fixture {
        root: PathBuf,
        _guard: test_temp::TempDir,
    }

    impl Fixture {
        fn new() -> Self {
            let guard = test_temp::TempDir::new("td-boot-test").unwrap();
            let root = guard.path().to_path_buf();
            fs::create_dir_all(root.join("td/boot")).unwrap();
            fs::create_dir_all(root.join("td/deployments")).unwrap();
            Fixture {
                root,
                _guard: guard,
            }
        }

        fn valid_deployment(&self) -> String {
            let manifest = fixture_manifest("");
            let id = sha256::hex_digest(manifest.as_bytes());
            let directory = self.root.join("td/deployments").join(&id);
            fs::create_dir(&directory).unwrap();
            for (label, bytes) in fixture_payloads("") {
                fs::write(directory.join(label), bytes).unwrap();
            }
            fs::write(directory.join("manifest"), manifest).unwrap();
            id
        }

        fn source_bundle(&self, name: &str, tag: &str) -> (PathBuf, String) {
            let directory = self.root.join(name);
            fs::create_dir(&directory).unwrap();
            let manifest = fixture_manifest(tag);
            let id = sha256::hex_digest(manifest.as_bytes());
            for (label, bytes) in fixture_payloads(tag) {
                fs::write(directory.join(label), bytes).unwrap();
            }
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
        let (current, previous) = ("a".repeat(64), "b".repeat(64));
        match parse_args(args(&[
            "rollback",
            "/dev/vda",
            "/run/td-update",
            &current,
            &previous,
        ])) {
            Ok(Mode::Rollback {
                current: named,
                previous: kept,
                ..
            }) => assert_eq!((named, kept), (current.clone(), previous.clone())),
            _ => panic!("rollback must parse and carry the approved pair"),
        }
        assert!(matches!(
            parse_args(args(&[
                "success",
                "/dev/vda",
                "/run/td-update",
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            ])),
            Ok(Mode::Success { .. })
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
        assert!(parse_args(args(&[
            "install",
            "/dev/vda",
            "/volume",
            "/incoming/deployment",
            "extra",
        ]))
        .is_err());
        // A former trusted-key operand is refused, not ignored.
        assert!(parse_args(args(&["update", "/dev/vda", "/run/td-update", "/channel"])).is_err());
        assert!(parse_args(args(&[
            "update",
            "/dev/vda",
            "/run/td-update",
            "/volume",
            "/channel",
            "/key.pub",
        ]))
        .is_err());
        // All three paths by VALUE, not by shape: they are three absolute paths
        // in a row, so a pair swapped at the call site still parses and the
        // wrong one is checked. The volume is the one that arrived last and is
        // easiest to mis-order.
        match parse_args(args(&[
            "update",
            "/dev/vda",
            "/run/td-update",
            "/run/td-volume",
            "/run/td-volume/td/incoming",
        ])) {
            Ok(Mode::Update {
                volume, channel, ..
            }) => {
                assert_eq!(volume, Path::new("/run/td-volume"));
                assert_eq!(channel, Path::new("/run/td-volume/td/incoming"));
            }
            _ => panic!("update must parse and carry its volume and channel"),
        }
        assert!(parse_args(args(&["rollback", "/volume"])).is_err());
        // The pair is required, each a canonical ID, and the two differ.
        let upper = current.to_uppercase();
        for bad in [
            vec!["rollback", "/dev/vda", "/volume"],
            vec!["rollback", "/dev/vda", "/volume", &current],
            vec!["rollback", "/dev/vda", "/volume", &current, &current],
            vec!["rollback", "/dev/vda", "/volume", &upper, &previous],
            vec!["rollback", "/dev/vda", "/volume", &current, "extra"],
            vec![
                "rollback", "/dev/vda", "/volume", &current, &previous, "extra",
            ],
        ] {
            assert!(parse_args(args(&bad)).is_err(), "{bad:?}");
        }
        assert!(parse_args(args(&["success", "/dev/vda", "/volume", "not-a-digest"])).is_err());
        assert!(parse_args(args(&["unknown", "/volume"])).is_err());

        // A former trusted-key operand is refused, not ignored.
        assert!(parse_args(args(&[
            "install",
            "/dev/vda",
            "/run/td-update",
            "/incoming/deployment",
            "/run/td-volume/td/trusted.pub",
        ]))
        .is_err());
        match parse_args(args(&["publish", "/volume", "/incoming/deployment"])) {
            Ok(Mode::Publish { root, source }) => {
                assert_eq!(root, Path::new("/volume"));
                assert_eq!(source, Path::new("/incoming/deployment"));
            }
            _ => panic!("publish must parse and carry its root and source"),
        }
        assert!(parse_args(args(&["publish", "/volume"])).is_err());
        assert!(parse_args(args(&[
            "publish",
            "/volume",
            "/incoming/deployment",
            "/key.pub",
        ]))
        .is_err());
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

    /// `publish` refuses a MOUNT POINT, which is what stands in for the
    /// transaction locking it cannot do: the lock every other mutating verb
    /// takes is keyed by the volume's block device, and this verb has none by
    /// construction. So rather than publish unserialised onto a volume
    /// something else may be mutating, it declines to be pointed at one.
    #[test]
    fn publish_refuses_a_mounted_root() {
        // `/proc` is a mount point on any host that can run this, and `/` is
        // one by definition; the fixture's own directory is not.
        let fixture = Fixture::new();
        assert!(
            !is_mount_point(&fixture.root).unwrap(),
            "a plain directory is not a mount point"
        );
        assert!(
            is_mount_point(Path::new("/")).unwrap(),
            "the filesystem root is a mount point"
        );
        if Path::new("/proc/self").exists() {
            assert!(
                is_mount_point(Path::new("/proc")).unwrap(),
                "/proc is a mount point"
            );
            let error = run_publish(Path::new("/proc"), &fixture.root)
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("is a mount point"),
                "publish must refuse a mounted root: {error}"
            );
        }
    }

    /// A volume the arms below can use, so that they are about what they say
    /// they are about rather than about the mount gate.
    ///
    /// `/` because a test cannot MAKE a mount point — that is the whole
    /// difficulty of this check — and `/` is one on every host while every
    /// absolute path is under it. The gate's own arms take the other side and
    /// use a directory that is NOT one, which a test can always arrange.
    fn any_mount_point() -> &'static Path {
        Path::new("/")
    }

    /// The fixture as the volume root, canonical and 0755, for the channel
    /// half of the decision: only directories a test made are walked, never
    /// the host's own ancestors of its temporary directory.
    fn fixture_volume(fixture: &Fixture) -> PathBuf {
        let volume = fs::canonicalize(&fixture.root).unwrap();
        fs::set_permissions(&volume, fs::Permissions::from_mode(0o755)).unwrap();
        volume
    }

    /// `run_update` below the mount-point gate, on `volume`.
    fn update_on(volume: &Path, mountpoint: &Path, channel: &Path) -> io::Result<()> {
        act_on_update(
            Path::new("/dev/null"),
            mountpoint,
            channel_decision(volume, channel)?,
        )
    }

    /// `update` is the verb a timer runs, so what it does when there is
    /// NOTHING to do is most of its behaviour.
    ///
    /// Three outcomes are pinned here and the difference between them is the
    /// whole design: a channel that is not there is a configuration fault and
    /// must be loud, a candidate that is not there is an up-to-date machine and
    /// must be silent, and a channel others may write must be loud in BOTH
    /// cases — including the idle one, which is the run that can afford to
    /// report it.
    ///
    /// None of these reaches a block device, which is what makes them testable:
    /// `/dev/null` stands in, and an error naming it would mean a check ran in
    /// the wrong order.
    #[test]
    fn update_is_quiet_only_when_the_channel_has_nothing_to_offer() {
        let fixture = Fixture::new();
        let volume = fixture_volume(&fixture);
        let channel = volume.join("channel");
        fs::create_dir(&channel).unwrap();
        fs::set_permissions(&channel, fs::Permissions::from_mode(0o755)).unwrap();
        let mountpoint = volume.join("mountpoint");
        let update = |channel: &Path| update_on(&volume, &mountpoint, channel);

        // An empty channel is the ordinary state of an up-to-date machine.
        assert!(
            update(&channel).is_ok(),
            "an absent candidate is nothing to do, not a failure"
        );

        // A channel that is not there is a typo, and answering "nothing to do"
        // to a typo hides it until an update is urgently needed.
        let error = update(&volume.join("no-such-channel"))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("no-such-channel"),
            "a missing channel must name itself: {error}"
        );

        // A file where the channel should be is the same class of fault.
        let file = volume.join("channel-file");
        fs::write(&file, b"not a channel").unwrap();
        let error = update(&file).unwrap_err().to_string();
        assert!(
            error.contains("must be a real directory"),
            "a channel that is not a directory must be refused: {error}"
        );

        // Nothing sets a timer's working directory, so a relative path is a
        // different directory depending on who started it. The verb's own
        // gate refuses it before any directory is walked.
        let error = run_update(
            Path::new("/dev/null"),
            &mountpoint,
            any_mount_point(),
            Path::new("channel"),
        )
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("must be an absolute path"),
            "a relative channel must be refused: {error}"
        );

        // A channel that cannot be READ is loud too. It is the case that would
        // most plausibly have been folded into "nothing to offer", and it is a
        // fault rather than an absence.
        let sealed = volume.join("sealed");
        fs::create_dir(&sealed).unwrap();
        fs::set_permissions(&sealed, fs::Permissions::from_mode(0o000)).unwrap();
        let denied = update(&sealed);
        fs::set_permissions(&sealed, fs::Permissions::from_mode(0o755)).unwrap();
        // Root ignores the mode, so this asserts only where it can mean anything.
        if !running_as_root() {
            let error = denied.unwrap_err().to_string();
            assert!(
                error.contains("update candidate"),
                "an unreadable channel must be loud: {error}"
            );
        }

        // Nothing signs a candidate, so who may write the channel is what
        // vouches for it: one others may write is refused, and on an idle
        // tick too.
        for mode in [0o775, 0o757, 0o777] {
            fs::set_permissions(&channel, fs::Permissions::from_mode(mode)).unwrap();
            let error = update(&channel).unwrap_err().to_string();
            assert!(
                error.contains("writable by no one else"),
                "a channel with mode {mode:o} must be refused: {error}"
            );
            assert!(
                !error.contains("/dev/null"),
                "the device must not have been reached: {error}"
            );
        }
    }

    /// Is this process root? Root bypasses directory modes, so a test that
    /// arranges "cannot read" has nothing to assert when it is.
    ///
    /// `find_map` rather than the shorter iterator method: this file is
    /// `include_str!`'d into the td-boot recipe, and
    /// `no_bootstrap_step_invokes_host_find_or_xargs` scans a recipe's step text
    /// for the retired findutils tool as a bare token. A Rust method call reads
    /// as one to that scan, which is the right conservatism on its side — and
    /// this comment cannot name it either, for exactly the same reason.
    fn running_as_root() -> bool {
        fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|status| {
                status
                    .lines()
                    .find_map(|line| line.strip_prefix("Uid:"))
                    .and_then(|rest| rest.split_whitespace().next().map(str::to_string))
            })
            .is_some_and(|uid| uid == "0")
    }

    /// A candidate in `channel` with the four files a bundle names, the
    /// owner and modes the update rule requires.
    fn offer(channel: &Path) -> PathBuf {
        let candidate = channel.join(protocol::CHANNEL_CANDIDATE);
        fs::create_dir(&candidate).unwrap();
        fs::set_permissions(&candidate, fs::Permissions::from_mode(0o755)).unwrap();
        for name in ["manifest", "bzImage", "initramfs.cpio", "root.erofs"] {
            fs::write(candidate.join(name), name).unwrap();
            fs::set_permissions(candidate.join(name), fs::Permissions::from_mode(0o644)).unwrap();
        }
        candidate
    }

    /// Who can write the candidate, or any directory above the channel, is
    /// what vouches for an unsigned update, so each must be closed to others.
    #[test]
    fn an_update_others_could_rewrite_is_refused() {
        let fixture = Fixture::new();
        let volume = fixture_volume(&fixture);
        let parent = volume.join("parent");
        fs::create_dir(&parent).unwrap();
        let channel = parent.join("channel");
        fs::create_dir(&channel).unwrap();
        fs::set_permissions(&channel, fs::Permissions::from_mode(0o755)).unwrap();
        let candidate = offer(&channel);
        let refused = |what: &str| {
            let error = channel_decision(&volume, &channel).unwrap_err().to_string();
            assert!(
                error.contains("writable by no one else"),
                "{what} must be refused: {error}"
            );
        };

        // A parent others may write could swap the whole channel; sticky,
        // as `/tmp` is, it cannot.
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o775)).unwrap();
        refused("a group-writable parent");
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o1777)).unwrap();
        assert!(channel_decision(&volume, &channel).unwrap().is_some());
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o755)).unwrap();
        // The channel itself may not lean on the sticky bit.
        fs::set_permissions(&channel, fs::Permissions::from_mode(0o1777)).unwrap();
        refused("a sticky world-writable channel");
        fs::set_permissions(&channel, fs::Permissions::from_mode(0o755)).unwrap();

        fs::set_permissions(&candidate, fs::Permissions::from_mode(0o775)).unwrap();
        refused("a group-writable candidate");
        fs::set_permissions(&candidate, fs::Permissions::from_mode(0o755)).unwrap();
        for name in ["manifest", "bzImage", "initramfs.cpio", "root.erofs"] {
            let file = candidate.join(name);
            fs::set_permissions(&file, fs::Permissions::from_mode(0o664)).unwrap();
            refused(&format!("a group-writable {name}"));
            fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
        }
        // A link would let its target's writers rewrite the payload.
        let manifest = candidate.join("manifest");
        fs::remove_file(&manifest).unwrap();
        symlink(volume.join("elsewhere"), &manifest).unwrap();
        refused("a symlinked manifest");
        fs::remove_file(&manifest).unwrap();
        fs::write(&manifest, b"manifest").unwrap();
        fs::set_permissions(&manifest, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(channel_decision(&volume, &channel).unwrap().is_some());
    }

    /// What the decision IS, which is the half `run_update` can be driven to.
    #[test]
    fn a_candidate_that_is_there_is_decided_on() {
        let fixture = Fixture::new();
        let volume = fixture_volume(&fixture);
        let channel = volume.join("channel");
        fs::create_dir(&channel).unwrap();
        fs::set_permissions(&channel, fs::Permissions::from_mode(0o755)).unwrap();

        assert!(
            channel_decision(&volume, &channel).unwrap().is_none(),
            "an empty channel decides on nothing"
        );

        let candidate = offer(&channel);
        let chosen = channel_decision(&volume, &channel)
            .unwrap()
            .expect("a candidate that is there must be decided on");
        assert_eq!(
            chosen,
            fs::canonicalize(&candidate).unwrap(),
            "the decision names the candidate"
        );
    }

    /// The decision is not merely made, it is ACTED ON.
    ///
    /// With a candidate present the verb must get as far as the device and fail
    /// THERE — `/dev/null` is not a volume. A `run_update` that dropped the
    /// publish and returned `Ok(())` was green before this, which is the whole
    /// point: "did nothing" and "nothing to do" are the two states this verb
    /// must never confuse.
    #[test]
    fn a_decided_update_reaches_the_device_instead_of_returning_quietly() {
        let fixture = Fixture::new();
        let volume = fixture_volume(&fixture);
        let channel = volume.join("channel");
        fs::create_dir(&channel).unwrap();
        fs::set_permissions(&channel, fs::Permissions::from_mode(0o755)).unwrap();
        offer(&channel);

        let error = update_on(&volume, &volume.join("mountpoint"), &channel)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("/dev/null"),
            "an update with a candidate must reach the device: {error}"
        );
    }

    /// `update`'s argv hop, pinned by BEHAVIOUR: the dispatch arm would still
    /// compile with `volume` and `channel`, two adjacent `PathBuf`s, swapped.
    #[test]
    fn dispatch_hands_update_its_volume_and_channel_the_right_way_round() {
        let fixture = Fixture::new();
        // The channel is missing, so the run must fail naming it AS the
        // channel. Swapped, the missing path would be reported as the volume.
        let error = dispatch(Mode::Update {
            device: PathBuf::from("/dev/null"),
            mountpoint: fixture.root.join("mountpoint"),
            volume: any_mount_point().to_path_buf(),
            channel: fixture.root.join("no-such-channel"),
        })
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("update channel") && error.contains("no-such-channel"),
            "the channel must arrive as the channel: {error}"
        );
    }

    /// A volume that is not a MOUNT POINT is refused — DESIGN §10 item 10f.
    ///
    /// The failure this closes: `/init` creates `/run/td-volume` before moving
    /// the volume onto it, so a move that never happened leaves an ordinary
    /// directory on the `/run` tmpfs. Anything that can write `/run` can then
    /// offer a candidate there, and an unattended updater would install it, on
    /// the one verb with nobody watching.
    ///
    /// A directory that is NOT a mount point is the half of this a test can
    /// arrange; `any_mount_point` above is the other half and explains why.
    #[test]
    fn a_volume_that_is_not_a_mount_point_is_refused() {
        let fixture = Fixture::new();
        let channel = fixture.root.join("channel");
        fs::create_dir(&channel).unwrap();
        fs::set_permissions(&channel, fs::Permissions::from_mode(0o755)).unwrap();

        // With a candidate waiting, and again with the channel empty: the check
        // is EAGER, so an up-to-date machine reports its missing volume too
        // rather than only the tick that finally has work.
        let idle = fixture.root.join("idle");
        fs::create_dir(&idle).unwrap();
        fs::create_dir(channel.join(protocol::CHANNEL_CANDIDATE)).unwrap();
        for (offered, what) in [(&channel, "a candidate"), (&idle, "an empty channel")] {
            let error = run_update(
                Path::new("/dev/null"),
                &fixture.root.join("mountpoint"),
                &fixture.root,
                offered,
            )
            .unwrap_err()
            .to_string();
            assert!(
                error.contains("is not a mount point"),
                "a volume that is only a directory must be refused with {what}: {error}"
            );
            assert!(
                !error.contains("/dev/null"),
                "the device must not have been reached: {error}"
            );
        }

        // The volume gets the same two shape checks its siblings get, which
        // review found nothing was asking for. A relative one is a different
        // directory depending on who started the timer; a volume that is a FILE
        // reaches `is_mount_point`, where a device number compares equal to its
        // parent's and the diagnostic would be about mounting rather than about
        // what was named.
        let file = fixture.root.join("volume-file");
        fs::write(&file, b"not a volume").unwrap();
        for (volume, want) in [
            (Path::new("volume"), "must be an absolute path"),
            (file.as_path(), "must be a real directory"),
            // An absent one reports the IO error rather than the type, which is
            // the better diagnostic and is asserted as what it is.
            (&fixture.root.join("no-such-volume"), "no-such-volume"),
        ] {
            let error = run_update(
                Path::new("/dev/null"),
                &fixture.root.join("mountpoint"),
                volume,
                &channel,
            )
            .unwrap_err()
            .to_string();
            assert!(
                error.contains(want),
                "a volume at {} must be refused with {want}: {error}",
                volume.display()
            );
        }
    }

    /// ...and the channel must be ON the volume that was named.
    ///
    /// Without this the gate is satisfied by naming ANY mount point while the
    /// candidate comes from somewhere else entirely — `/proc` is a mount point
    /// on every host, so the check would pass and say nothing about the
    /// channel.
    #[test]
    fn a_channel_that_is_not_on_the_named_volume_is_refused() {
        let fixture = Fixture::new();
        let channel = fixture.root.join("channel");
        fs::create_dir(&channel).unwrap();
        fs::set_permissions(&channel, fs::Permissions::from_mode(0o755)).unwrap();

        // `/proc` stands in for "some mount point that is not the fixture's".
        // Asserted rather than assumed, so a sandbox without it says THAT
        // instead of failing on the message below.
        assert!(
            is_mount_point(Path::new("/proc")).unwrap_or(false),
            "this test needs /proc to be a mount point to have a volume to name"
        );
        let update = |channel: &Path| {
            run_update(
                Path::new("/dev/null"),
                &fixture.root.join("mountpoint"),
                Path::new("/proc"),
                channel,
            )
            .unwrap_err()
            .to_string()
        };
        let error = update(&channel);
        assert!(
            error.contains("is not on the volume"),
            "a channel off the named volume must be refused: {error}"
        );

        // ...and containment is judged on the RESOLVED path, not the spelling.
        // `Path::starts_with` compares components and normalizes nothing, so a
        // `..` walks straight out of the volume while still "starting with" it.
        let escaped = Path::new("/proc/..").join(
            channel
                .strip_prefix("/")
                .expect("the fixture channel is an absolute path"),
        );
        assert!(
            escaped.starts_with("/proc"),
            "the traversal must be lexically inside the volume, or it tests nothing"
        );
        let error = update(&escaped);
        assert!(
            error.contains("is not on the volume"),
            "a channel that walks out of the volume with `..` must be refused: {error}"
        );

        // A SYMLINK is refused outright rather than resolved and admitted.
        let link = fixture.root.join("linked");
        std::os::unix::fs::symlink(&channel, &link).unwrap();
        let error = run_update(
            Path::new("/dev/null"),
            &fixture.root.join("mountpoint"),
            any_mount_point(),
            &link,
        )
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("must be a real directory"),
            "a symlinked channel must be refused: {error}"
        );
    }

    /// Containment is COMPONENT-wise, which a string prefix is not.
    ///
    /// Tested on the predicate rather than through `update`, because the path
    /// through it would need two mount points whose names share a prefix and a
    /// test cannot create either.
    #[test]
    fn a_volume_does_not_contain_its_name_plus_more_letters() {
        assert!(is_within(
            Path::new("/run/td"),
            Path::new("/run/td/key.pub")
        ));
        assert!(is_within(Path::new("/run/td"), Path::new("/run/td")));
        assert!(is_within(Path::new("/"), Path::new("/anything/at/all")));
        assert!(
            !is_within(Path::new("/run/td"), Path::new("/run/tdx/key.pub")),
            "a string prefix is not containment: /run/tdx is not under /run/td"
        );
        assert!(!is_within(Path::new("/run/td"), Path::new("/run/key.pub")));
    }

    /// The usage line and the parser agree about `update`'s arity.
    ///
    /// Nothing else asserts any usage text, so the operator-facing description
    /// of a verb can drift from what it accepts — which is what happened here
    /// the moment the volume argument was added.
    #[test]
    fn the_usage_line_names_every_argument_update_requires() {
        let usage = usage_error().to_string();
        assert!(
            usage.contains(&format!(
                "td-boot {} <device> <mountpoint> <volume> <channel>\n",
                protocol::UPDATE_VERB
            )),
            "the usage line must match the arity the parser enforces: {usage}"
        );
    }

    /// A candidate must be a real directory, `require_real_directory`'s rule
    /// everywhere else in this file. A symlink would make the channel's
    /// contents depend on something outside it, which its owner does not
    /// control.
    #[test]
    fn an_update_candidate_that_is_not_a_real_directory_is_refused() {
        let fixture = Fixture::new();
        let volume = fixture_volume(&fixture);
        let channel = volume.join("channel");
        fs::create_dir(&channel).unwrap();
        fs::set_permissions(&channel, fs::Permissions::from_mode(0o755)).unwrap();
        let elsewhere = volume.join("elsewhere");
        fs::create_dir(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, channel.join(protocol::CHANNEL_CANDIDATE)).unwrap();

        let error = update_on(&volume, &volume.join("mountpoint"), &channel)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("update candidate must be a real directory"),
            "a symlinked candidate must be refused: {error}"
        );
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
        assert_eq!(
            read_attempt_state(&fixture.root, &candidate).unwrap(),
            Some(protocol::DEFAULT_BOOT_ATTEMPTS)
        );
        let entries = fs::read_dir(fixture.root.join("td/deployments"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        assert!(entries
            .iter()
            .all(|name| !name.as_bytes().starts_with(b".install-")));
    }

    /// A legacy `manifest.sig` beside a source's manifest is ignored: the
    /// deployment publishes and boots, and the file never reaches it.
    #[test]
    fn a_legacy_signature_beside_a_source_is_neither_needed_nor_published() {
        let fixture = Fixture::new();
        let initial = fixture.valid_deployment();
        fixture.selector("current", &initial);
        fixture.selector("previous", &initial);
        let (source, candidate) = fixture.source_bundle("candidate", "next");
        fs::write(source.join("manifest.sig"), b"aa11\n").unwrap();

        assert_eq!(
            install_deployment(&fixture.root, &source).unwrap(),
            candidate
        );
        let published = fixture
            .root
            .join(protocol::DEPLOYMENTS_DIR)
            .join(&candidate);
        let mut names = fs::read_dir(&published)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect::<Vec<_>>();
        names.sort();
        assert_eq!(
            names,
            ["bzImage", "initramfs.cpio", "manifest", "root.erofs"],
            "only the manifest and the payloads it names are published"
        );
        assert_eq!(
            select_boot_deployment(&fixture.root).unwrap().deployment_id,
            candidate
        );
    }

    /// Re-installing an IDENTICAL bundle must be a true no-op. Asserted on the
    /// inode, because rewriting the same bytes is invisible in the contents.
    #[test]
    fn re_installing_an_identical_bundle_rewrites_nothing() {
        let fixture = Fixture::new();
        let initial = fixture.valid_deployment();
        fixture.selector("current", &initial);
        fixture.selector("previous", &initial);
        let (source, candidate) = fixture.source_bundle("candidate", "next");
        install_deployment(&fixture.root, &source).unwrap();

        let path = fixture
            .root
            .join(protocol::DEPLOYMENTS_DIR)
            .join(&candidate)
            .join(protocol::MANIFEST_NAME);
        let before = fs::symlink_metadata(&path).unwrap().ino();
        install_deployment(&fixture.root, &source).unwrap();
        assert_eq!(
            fs::symlink_metadata(&path).unwrap().ino(),
            before,
            "an unchanged deployment must not be rewritten"
        );
    }

    #[test]
    fn failed_pending_mark_keeps_the_verified_current_as_previous() {
        let fixture = Fixture::new();
        let current = fixture.valid_deployment();
        fixture.selector("current", &current);
        fixture.selector("previous", &current);
        let (source, previous) = fixture.source_bundle("previous", "previous");
        let bundle = open_bundle(&source, Names::Source).unwrap();
        publish_bundle(&fixture.root, bundle).unwrap();
        replace_selector(&fixture.root, "previous", &previous).unwrap();
        let attempts = attempts_directory(&fixture.root, true).unwrap().unwrap();
        fs::create_dir(attempts.join(&previous)).unwrap();

        assert!(install_deployment(&fixture.root, &source).is_err());
        assert_eq!(read_selector(&fixture.root, "current").unwrap(), current);
        assert_eq!(read_selector(&fixture.root, "previous").unwrap(), current);
    }

    #[test]
    fn repeated_pending_install_preserves_budget_and_rollback_target() {
        let fixture = Fixture::new();
        let initial = fixture.valid_deployment();
        fixture.selector("current", &initial);
        fixture.selector("previous", &initial);
        let (source, candidate) = fixture.source_bundle("candidate", "next");

        install_deployment(&fixture.root, &source).unwrap();
        let first = select_boot_deployment(&fixture.root).unwrap();
        assert_eq!(
            first.remaining_attempts,
            Some(protocol::DEFAULT_BOOT_ATTEMPTS - 1)
        );
        install_deployment(&fixture.root, &source).unwrap();

        assert_eq!(read_selector(&fixture.root, "current").unwrap(), candidate);
        assert_eq!(read_selector(&fixture.root, "previous").unwrap(), initial);
        // Reinstall is idempotent, not an escape from automatic rollback.
        assert_eq!(
            read_attempt_state(&fixture.root, &candidate).unwrap(),
            Some(protocol::DEFAULT_BOOT_ATTEMPTS - 1)
        );
    }

    #[test]
    fn repeated_pending_install_rejects_a_broken_previous_selector() {
        let fixture = Fixture::new();
        let initial = fixture.valid_deployment();
        fixture.selector("current", &initial);
        fixture.selector("previous", &initial);
        let (source, candidate) = fixture.source_bundle("candidate", "next");

        install_deployment(&fixture.root, &source).unwrap();
        fs::remove_dir_all(fixture.root.join("td/deployments").join(initial)).unwrap();
        let error = install_deployment(&fixture.root, &source).unwrap_err();

        assert!(error
            .to_string()
            .contains("invalid fallback with a pending deployment"));
        assert_eq!(read_selector(&fixture.root, "current").unwrap(), candidate);
        assert!(verify_slot(&fixture.root, "previous").is_err());
    }

    #[test]
    fn repeated_install_rejects_a_pending_previous_deployment() {
        let fixture = Fixture::new();
        let initial = fixture.valid_deployment();
        fixture.selector("current", &initial);
        fixture.selector("previous", &initial);
        let (source, candidate) = fixture.source_bundle("candidate", "next");

        install_deployment(&fixture.root, &source).unwrap();
        write_attempt_state(&fixture.root, &initial, 1).unwrap();
        let error = install_deployment(&fixture.root, &source).unwrap_err();

        assert!(error
            .to_string()
            .contains("refusing to retain pending deployment"));
        assert_eq!(read_selector(&fixture.root, "current").unwrap(), candidate);
        assert_eq!(read_selector(&fixture.root, "previous").unwrap(), initial);
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
    fn boot_menu_promotes_previous_and_can_retry_the_displaced_pending_version() {
        let fixture = Fixture::new();
        let initial = fixture.valid_deployment();
        fixture.selector("current", &initial);
        fixture.selector("previous", &initial);
        let (source, candidate) = fixture.source_bundle("candidate", "next");
        install_deployment(&fixture.root, &source).unwrap();
        let decision = menu::select(&fixture.root, &initial).unwrap();
        assert_eq!(decision.deployment_id, initial);
        assert_eq!(read_selector(&fixture.root, "current").unwrap(), initial);
        assert_eq!(read_selector(&fixture.root, "previous").unwrap(), candidate);
        assert_eq!(
            current_attempt_state(&fixture.root, &initial).unwrap(),
            None
        );
        let decision = menu::select(&fixture.root, &candidate).unwrap();
        assert_eq!(decision.deployment_id, candidate);
        assert_eq!(read_selector(&fixture.root, "previous").unwrap(), initial);
        assert_eq!(
            read_attempt_state(&fixture.root, &candidate).unwrap(),
            Some(2)
        );
        write_attempt_state(&fixture.root, &candidate, 0).unwrap();
        assert_eq!(
            menu::select(&fixture.root, &candidate)
                .unwrap()
                .deployment_id,
            candidate
        );
        assert_eq!(
            read_attempt_state(&fixture.root, &candidate).unwrap(),
            Some(2)
        );
    }

    #[test]
    fn boot_menu_refuses_unretained_and_unverified_choices_without_writes() {
        let fixture = Fixture::new();
        let initial = fixture.valid_deployment();
        fixture.selector("current", &initial);
        fixture.selector("previous", &initial);
        assert!(menu::select(&fixture.root, &"a".repeat(64)).is_err());
        assert_eq!(read_selector(&fixture.root, "current").unwrap(), initial);
        assert_eq!(read_selector(&fixture.root, "previous").unwrap(), initial);
        fs::write(
            fixture
                .root
                .join("td/deployments")
                .join(&initial)
                .join("root.erofs"),
            b"damaged",
        )
        .unwrap();
        let error = menu::select(&fixture.root, &initial).err().unwrap();
        assert!(
            error.to_string().contains("root.erofs hash mismatch"),
            "{error}"
        );
        assert_eq!(read_selector(&fixture.root, "current").unwrap(), initial);
    }

    #[test]
    fn rollback_selects_previous_and_retains_displaced_deployment() {
        let fixture = Fixture::new();
        let initial = fixture.valid_deployment();
        fixture.selector("current", &initial);
        fixture.selector("previous", &initial);
        let (source, candidate) = fixture.source_bundle("candidate", "next");
        install_deployment(&fixture.root, &source).unwrap();

        assert_eq!(
            rollback_deployment(&fixture.root, &candidate, &initial).unwrap(),
            initial
        );
        assert_eq!(read_selector(&fixture.root, "current").unwrap(), initial);
        assert_eq!(read_selector(&fixture.root, "previous").unwrap(), initial);
        assert!(fixture.root.join("td/deployments").join(candidate).is_dir());
    }

    /// The approved pair is used up by the rollback that moves it: the same
    /// pair a second time no longer matches `current`, and nothing moves.
    #[test]
    fn a_second_rollback_of_the_same_pair_is_refused() {
        let fixture = Fixture::new();
        let initial = fixture.valid_deployment();
        fixture.selector("current", &initial);
        fixture.selector("previous", &initial);
        let (source, candidate) = fixture.source_bundle("candidate", "next");
        install_deployment(&fixture.root, &source).unwrap();

        rollback_deployment(&fixture.root, &candidate, &initial).unwrap();
        let error = rollback_deployment(&fixture.root, &candidate, &initial).unwrap_err();

        assert!(error.to_string().contains(&format!(
            "refusing rollback: current names {initial}, not the approved {candidate}"
        )));
        assert_eq!(read_selector(&fixture.root, "current").unwrap(), initial);
        assert_eq!(read_selector(&fixture.root, "previous").unwrap(), initial);
        assert!(fixture.root.join("td/deployments").join(candidate).is_dir());
    }

    /// The race td-authd's consent leaves open (td-authd/DESIGN.md,
    /// "Elevation operations"): the person approves rolling pending `B`
    /// back to `A`, and before the helper runs an update publishes `C`.
    /// While `B` was pending, `install_deployment` kept `A` as `previous`,
    /// so only the check of `current` sees it. The rollback is refused
    /// before any change, even a temporary's reaping, and `C` stays current
    /// with its boot budget.
    #[test]
    fn an_update_published_after_consent_is_refused_before_any_change() {
        let fixture = Fixture::new();
        let initial = fixture.valid_deployment();
        fixture.selector("current", &initial);
        fixture.selector("previous", &initial);
        let (source, approved) = fixture.source_bundle("candidate", "next");
        install_deployment(&fixture.root, &source).unwrap();
        assert_eq!(read_selector(&fixture.root, "current").unwrap(), approved);
        assert_eq!(read_selector(&fixture.root, "previous").unwrap(), initial);

        let (later, published) = fixture.source_bundle("later", "first");
        install_deployment(&fixture.root, &later).unwrap();
        assert_eq!(read_selector(&fixture.root, "current").unwrap(), published);
        assert_eq!(read_selector(&fixture.root, "previous").unwrap(), initial);
        let budget = read_attempt_state(&fixture.root, &published).unwrap();
        assert!(budget.is_some());
        let temporary = fixture
            .root
            .join(format!("td/boot/.current-{}-0", std::process::id()));
        fs::write(&temporary, b"").unwrap();

        let error = rollback_deployment(&fixture.root, &approved, &initial).unwrap_err();

        assert!(error.to_string().contains(&format!(
            "refusing rollback: current names {published}, not the approved {approved}"
        )));
        assert_eq!(read_selector(&fixture.root, "current").unwrap(), published);
        assert_eq!(read_selector(&fixture.root, "previous").unwrap(), initial);
        assert_eq!(
            read_attempt_state(&fixture.root, &published).unwrap(),
            budget
        );
        assert!(temporary.exists());
    }

    /// A `previous` moved since consent refuses as well, before any change:
    /// the rollback never lands on a deployment nobody approved.
    #[test]
    fn a_previous_moved_after_consent_is_refused_before_any_change() {
        let fixture = Fixture::new();
        let initial = fixture.valid_deployment();
        fixture.selector("current", &initial);
        fixture.selector("previous", &initial);
        let (source, candidate) = fixture.source_bundle("candidate", "next");
        install_deployment(&fixture.root, &source).unwrap();
        let (other, moved) = fixture.source_bundle("other", "recovery");
        let bundle = open_bundle(&other, Names::Source).unwrap();
        publish_bundle(&fixture.root, bundle).unwrap();
        replace_selector(&fixture.root, "previous", &moved).unwrap();

        let error = rollback_deployment(&fixture.root, &candidate, &initial).unwrap_err();

        assert!(error.to_string().contains(&format!(
            "refusing rollback: previous names {moved}, not the approved {initial}"
        )));
        assert_eq!(read_selector(&fixture.root, "current").unwrap(), candidate);
        assert_eq!(read_selector(&fixture.root, "previous").unwrap(), moved);
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

        assert_eq!(
            rollback_deployment(&fixture.root, &current, &previous).unwrap(),
            previous
        );
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

        let error = rollback_deployment(&fixture.root, &current, &previous).unwrap_err();

        assert!(error.to_string().contains("root.erofs hash mismatch"));
        assert_eq!(read_selector(&fixture.root, "current").unwrap(), current);
        assert_eq!(read_selector(&fixture.root, "previous").unwrap(), previous);
    }

    #[test]
    fn rollback_rejects_a_pending_previous_without_moving_current() {
        let fixture = Fixture::new();
        let initial = fixture.valid_deployment();
        fixture.selector("current", &initial);
        fixture.selector("previous", &initial);
        let (source, candidate) = fixture.source_bundle("candidate", "next");
        install_deployment(&fixture.root, &source).unwrap();
        fs::remove_file(fixture.root.join("td/boot/previous")).unwrap();
        fixture.selector("previous", &candidate);

        let error = rollback_deployment(&fixture.root, &candidate, &candidate).unwrap_err();

        assert!(error.to_string().contains("is not marked successful"));
        assert_eq!(read_selector(&fixture.root, "current").unwrap(), candidate);
        assert_eq!(read_selector(&fixture.root, "previous").unwrap(), candidate);
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
        assert_eq!(read_attempt_state(&fixture.root, &candidate).unwrap(), None);
    }

    #[test]
    fn attempt_state_parser_is_strict() {
        assert_eq!(
            parse_attempt_state(b"td-boot-attempt-v1\nremaining 3\n").unwrap(),
            3
        );
        for malformed in [
            b"".as_slice(),
            b"td-boot-attempt-v1\nremaining 3".as_slice(),
            b"td-boot-attempt-v1\nremaining 4\n".as_slice(),
            b"td-boot-attempt-v1\nremaining 03\n".as_slice(),
            b"td-boot-attempt-v1\nremaining 3\nextra\n".as_slice(),
            b"td-boot-attempt-v2\nremaining 3\n".as_slice(),
        ] {
            assert!(parse_attempt_state(malformed).is_err());
        }
        assert_eq!(attempt_status(None), "successful");
        assert_eq!(attempt_status(Some(0)), "exhausted");
        assert_eq!(attempt_status(Some(2)), "pending remaining=2");
        let fixture = Fixture::new();
        assert!(read_attempt_state(&fixture.root, "../current").is_err());
    }

    #[test]
    fn invalid_attempts_directory_recovers_previous_without_moving_current() {
        let fixture = Fixture::new();
        let initial = fixture.valid_deployment();
        fixture.selector("current", &initial);
        fixture.selector("previous", &initial);
        let (source, candidate) = fixture.source_bundle("candidate", "next");
        install_deployment(&fixture.root, &source).unwrap();
        let attempts = fixture.root.join(protocol::ATTEMPTS_DIR);
        fs::set_permissions(&attempts, fs::Permissions::from_mode(0o755)).unwrap();

        let error = select_boot_deployment(&fixture.root).err().unwrap();
        let (decision, _) = read_only_recovery(&fixture.root, &error).unwrap();

        assert!(error.to_string().contains("mode 0700"));
        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert_eq!(decision.slot, "previous");
        assert_eq!(decision.deployment_id, initial);
        assert_eq!(read_selector(&fixture.root, "current").unwrap(), candidate);
    }

    #[test]
    fn state_owner_policy_separates_production_from_fixtures() {
        assert!(state_owner_allowed(0, None));
        assert!(!state_owner_allowed(1, None));
        assert!(!state_owner_allowed(u32::MAX, None));
        assert!(state_owner_allowed(1000, Some(1000)));
        assert!(!state_owner_allowed(1001, Some(1000)));
    }

    #[test]
    fn pending_boots_consume_budget_then_promote_previous() {
        let fixture = Fixture::new();
        let initial = fixture.valid_deployment();
        fixture.selector("current", &initial);
        fixture.selector("previous", &initial);
        let (source, candidate) = fixture.source_bundle("candidate", "next");
        install_deployment(&fixture.root, &source).unwrap();

        for remaining in [2, 1, 0] {
            let decision = select_boot_deployment(&fixture.root).unwrap();
            assert_eq!(decision.slot, "current");
            assert_eq!(decision.deployment_id, candidate);
            assert_eq!(decision.remaining_attempts, Some(remaining));
            assert!(decision.exhausted_deployment.is_none());
        }

        let decision = select_boot_deployment(&fixture.root).unwrap();
        assert_eq!(decision.slot, "previous");
        assert_eq!(decision.deployment_id, initial);
        assert_eq!(decision.exhausted_deployment, Some(candidate.clone()));
        assert_eq!(read_selector(&fixture.root, "current").unwrap(), initial);
        assert_eq!(
            read_attempt_state(&fixture.root, &candidate).unwrap(),
            Some(0)
        );
    }

    #[test]
    fn exhausted_current_still_boots_when_previous_is_unusable() {
        let fixture = Fixture::new();
        let initial = fixture.valid_deployment();
        fixture.selector("current", &initial);
        fixture.selector("previous", &initial);
        let (source, candidate) = fixture.source_bundle("candidate", "next");
        install_deployment(&fixture.root, &source).unwrap();
        fs::remove_dir_all(fixture.root.join("td/deployments").join(initial)).unwrap();
        for _ in 0..protocol::DEFAULT_BOOT_ATTEMPTS {
            select_boot_deployment(&fixture.root).unwrap();
        }

        let decision = select_boot_deployment(&fixture.root).unwrap();

        assert_eq!(decision.slot, "current");
        assert_eq!(decision.deployment_id, candidate);
        assert_eq!(
            decision.exhausted_deployment,
            Some(decision.deployment_id.clone())
        );
        assert!(decision.fallback_error.is_some());
        assert_eq!(read_selector(&fixture.root, "current").unwrap(), candidate);
    }

    #[test]
    fn successful_boot_clears_attempt_budget() {
        let fixture = Fixture::new();
        let initial = fixture.valid_deployment();
        fixture.selector("current", &initial);
        fixture.selector("previous", &initial);
        let (source, candidate) = fixture.source_bundle("candidate", "next");
        install_deployment(&fixture.root, &source).unwrap();
        select_boot_deployment(&fixture.root).unwrap();
        assert_eq!(
            current_attempt_state(&fixture.root, &candidate).unwrap(),
            Some(protocol::DEFAULT_BOOT_ATTEMPTS - 1)
        );

        assert_eq!(
            mark_deployment_successful(&fixture.root, &candidate).unwrap(),
            candidate
        );
        assert_eq!(read_attempt_state(&fixture.root, &candidate).unwrap(), None);
        assert_eq!(
            current_attempt_state(&fixture.root, &candidate).unwrap(),
            None
        );
        let decision = select_boot_deployment(&fixture.root).unwrap();
        assert_eq!(decision.deployment_id, candidate);
        assert_eq!(decision.remaining_attempts, None);
    }

    #[test]
    fn success_rejects_a_non_current_deployment() {
        let fixture = Fixture::new();
        let initial = fixture.valid_deployment();
        fixture.selector("current", &initial);
        fixture.selector("previous", &initial);
        let (source, candidate) = fixture.source_bundle("candidate", "next");
        install_deployment(&fixture.root, &source).unwrap();

        let error = mark_deployment_successful(&fixture.root, &initial).unwrap_err();

        assert!(error.to_string().contains("is not current"));
        assert_eq!(
            read_attempt_state(&fixture.root, &candidate).unwrap(),
            Some(protocol::DEFAULT_BOOT_ATTEMPTS)
        );
    }

    #[test]
    fn malformed_attempt_state_promotes_verified_previous() {
        let fixture = Fixture::new();
        let initial = fixture.valid_deployment();
        fixture.selector("current", &initial);
        fixture.selector("previous", &initial);
        let (source, candidate) = fixture.source_bundle("candidate", "next");
        install_deployment(&fixture.root, &source).unwrap();
        fs::write(
            fixture.root.join(protocol::ATTEMPTS_DIR).join(&candidate),
            b"malformed\n",
        )
        .unwrap();

        let decision = select_boot_deployment(&fixture.root).unwrap();

        assert_eq!(decision.slot, "previous");
        assert_eq!(decision.deployment_id, initial);
        assert!(decision.current_error.is_some());
        assert_eq!(read_selector(&fixture.root, "current").unwrap(), initial);
    }

    #[test]
    fn malformed_attempt_state_keeps_verified_current_when_previous_is_unusable() {
        let fixture = Fixture::new();
        let initial = fixture.valid_deployment();
        fixture.selector("current", &initial);
        fixture.selector("previous", &initial);
        let (source, candidate) = fixture.source_bundle("candidate", "next");
        install_deployment(&fixture.root, &source).unwrap();
        fs::remove_dir_all(fixture.root.join("td/deployments").join(initial)).unwrap();
        fs::write(
            fixture.root.join(protocol::ATTEMPTS_DIR).join(&candidate),
            b"malformed\n",
        )
        .unwrap();

        let decision = select_boot_deployment(&fixture.root).unwrap();

        assert_eq!(decision.slot, "current");
        assert_eq!(decision.deployment_id, candidate);
        assert!(decision.bookkeeping_error.is_some());
        assert_eq!(read_selector(&fixture.root, "current").unwrap(), candidate);
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

    /// A deployment needs no signature: one whose payloads verify against a
    /// manifest that hashes to its id boots, and a legacy `manifest.sig`
    /// beside it is neither read nor required.
    #[test]
    fn an_unsigned_deployment_boots_and_a_legacy_signature_is_ignored() {
        let fixture = Fixture::new();
        let initial = fixture.valid_deployment();
        fixture.selector("current", &initial);
        fixture.selector("previous", &initial);
        assert_eq!(
            select_boot_deployment(&fixture.root).unwrap().deployment_id,
            initial
        );
        fs::write(
            fixture
                .root
                .join(protocol::DEPLOYMENTS_DIR)
                .join(&initial)
                .join("manifest.sig"),
            b"not a signature\n",
        )
        .unwrap();
        assert_eq!(
            select_boot_deployment(&fixture.root).unwrap().deployment_id,
            initial
        );
    }

    /// The id/manifest binding, which is what makes ONE read enough.
    ///
    /// The selector names a deployment by ID. Requiring the manifest bytes to
    /// hash to the id is the join between the two, and without it a deployment
    /// directory could serve one manifest while its id — the thing handed to
    /// `root-loop` past the kexec — named something else entirely.
    #[test]
    fn a_manifest_that_does_not_hash_to_its_directory_name_is_refused() {
        let fixture = Fixture::new();
        let initial = fixture.valid_deployment();
        fixture.selector("current", &initial);
        fixture.selector("previous", &initial);
        let manifest = fixture
            .root
            .join(protocol::DEPLOYMENTS_DIR)
            .join(&initial)
            .join(protocol::MANIFEST_NAME);
        let mut bytes = fs::read(&manifest).unwrap();
        bytes.extend_from_slice(b"\n");
        fs::write(&manifest, &bytes).unwrap();

        let refused = select_boot_deployment(&fixture.root).err().unwrap();

        assert!(
            refused.to_string().contains("does not match manifest hash"),
            "the id must be bound to the bytes: {refused}"
        );
    }

    /// The slot names in `protocol.rs` are the ones a publish actually creates.
    ///
    /// `td-install`'s recipe check reads the `current` selector back out of an
    /// unmounted image, and it must not spell that name itself — a check
    /// carrying its own copy keeps passing through exactly the rename it exists
    /// to catch. So the constant is the shared statement, and this ties it to
    /// BEHAVIOUR rather than to source text: a publish is done and the entry
    /// the constant names has to be the symlink that appeared.
    #[test]
    fn a_publish_creates_the_selector_protocol_names() {
        // TWO deployments, because one cannot tell the constants apart: a first
        // publish points both slots at the same id, so `current` and `previous`
        // are indistinguishable and swapping the two constants passes. The
        // second publish demotes the first, and only then does each slot have a
        // target that is wrong for the other.
        let fixture = Fixture::new();
        let first = fixture.valid_deployment();
        // Set up with the LITERALS td-boot's own code uses, and assert through
        // the constants — so this reds if the constants stop naming these.
        fixture.selector("current", &first);
        fixture.selector("previous", &first);
        let (source, second) = fixture.source_bundle("incoming", "next");
        install_deployment(&fixture.root, &source).unwrap();
        assert_ne!(
            first, second,
            "the two fixtures must differ to tell slots apart"
        );

        let boot = fixture.root.join(protocol::BOOT_DIR);
        for (slot, want) in [
            (protocol::CURRENT_SLOT, &second),
            (protocol::PREVIOUS_SLOT, &first),
        ] {
            let target = fs::read_link(boot.join(slot)).unwrap();
            assert_eq!(
                target,
                PathBuf::from(format!("{}{want}", protocol::SELECTOR_PREFIX)),
                "the {slot} selector does not point where a publish puts it"
            );
        }
    }

    #[test]
    fn measurement_policy_is_explicit_bounded_and_never_a_symlink() {
        let fixture = Fixture::new();
        let path = fixture.root.join(protocol::MEASUREMENT_POLICY_PATH);
        assert!(!measurement::enabled(&fixture.root).unwrap());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        for bytes in [
            b"".as_slice(),
            b"td-selector-pcr11-v2\n",
            b"td-selector-pcr11-v1\nextra",
        ] {
            fs::write(&path, bytes).unwrap();
            assert!(measurement::enabled(&fixture.root).is_err());
        }
        fs::write(&path, b"td-selector-pcr11-v1\n").unwrap();
        assert!(measurement::enabled(&fixture.root).unwrap());
        fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink("missing", &path).unwrap();
        assert!(measurement::enabled(&fixture.root).is_err());
    }

    #[test]
    fn measurement_stays_between_verified_selection_and_kexec() {
        let source = include_str!("main.rs");
        let boot = source
            .split_once("\nfn run_boot(")
            .unwrap()
            .1
            .split_once("\n}\n")
            .unwrap()
            .0;
        assert!(boot.contains("measurement::enabled(Path::new(\"/\"))?"));
        for call in boot.split("kexec_boot_decision(").skip(1) {
            assert!(call
                .split_once(')')
                .unwrap()
                .0
                .ends_with(", measured, handoff"));
        }
        let handoff = source
            .split_once("\nfn kexec_boot_decision(")
            .unwrap()
            .1
            .split_once("\n}\n")
            .unwrap()
            .0;
        let position = |needle: &str| {
            assert_eq!(handoff.matches(needle).count(), 1, "{needle}");
            handoff.find(needle).unwrap()
        };
        let arguments = position("let cmdline = kernel_cmdline(");
        // The admission record is built only after the measurement, with
        // its result, and from the deployment that was measured.
        let recorded = position("let record = measured_record(\n        &deployment.id,");
        let measure = position("|| measurement::measure(&deployment.id, cmdline.as_bytes()),");
        // The key and the record are written to their pipes only after the
        // measurement, and the pipes live until td-kexec returns.
        let piped = position(".map(|handoff| KeyFile::new(handoff.key.expose()))");
        let record_piped = position("let record_pipe = record.map(|record| KeyFile::new(&record))");
        let plain = position("(None, _) => kexec_command(kernel, initramfs, cmdline.as_os_str()),");
        let keyed = position("(Some(pipe), None) => key_kexec_command(");
        let admitting = position("(Some(pipe), Some(record)) => key_kexec_command(");
        let execute = position("run_command(&mut command, \"td-kexec\")?;");
        assert!(arguments < recorded && recorded < measure && measure < piped);
        assert!(piped < record_piped && record_piped < plain);
        assert!(plain < keyed && keyed < admitting && admitting < execute);
        let record = source
            .split_once("\nfn measured_record(")
            .unwrap()
            .1
            .split_once("\n}\n")
            .unwrap()
            .0;
        let at = |needle: &str| {
            assert_eq!(record.matches(needle).count(), 1, "{needle}");
            record.find(needle).unwrap()
        };
        // Only an encrypted boot survives a failed measurement.
        assert!(at("match measure() {") < at("Err(_) if encrypted || admission.is_some() => {"));
        assert!(
            at("Err(_) if encrypted || admission.is_some() => {")
                < at("Err(error) => return Err(error),")
        );
        assert!(
            at("Err(error) => return Err(error),") < at("admission_record(id, method, measured)")
        );
        assert!(!handoff.contains("drop(key_pipe)"));
        assert!(!handoff.contains("drop(record_pipe)"));
    }

    /// The record names the primary account and the selected deployment,
    /// and carries the release's method only after a measurement.
    #[test]
    fn the_admission_record_follows_the_measurement() {
        let id = "00112233445566778899aabbccddeeff0123456789abcdeffedcba9876543210";
        for method in [
            admission::Method::None,
            admission::Method::TpmPin,
            admission::Method::Fido2Primary,
        ] {
            for measured in [false, true] {
                let record =
                    admission::Record::decode(&admission_record(id, method, measured).unwrap())
                        .unwrap();
                assert_eq!(record.uid, 1000);
                assert_eq!(
                    record.manifest,
                    admission::manifest_id(id.as_bytes()).unwrap()
                );
                let expected = if measured {
                    method
                } else {
                    admission::Method::None
                };
                assert_eq!(record.method, expected, "{method:?} {measured}");
            }
        }
        assert!(admission_record("not an id", admission::Method::TpmPin, true).is_err());
        // The UID td-protector's tokens name is the record's.
        assert_eq!(
            td_protector::protected::ACCOUNT,
            admission::PRIMARY_UID.to_string()
        );
        assert_eq!(admission::RECORD_BYTES, protocol::ADMIT_RECORD_BYTES);
    }

    /// A protected boot whose measurement fails boots with method 0 and a
    /// fixed console line: no TPM under a FIDO2-primary release, a failing
    /// one under a recovery-key release, and a readback after the
    /// extension (PCR 11 uncertain) under a tpm-pin release. A device-bound
    /// one boots with the line and no record; an unencrypted one still
    /// aborts on the failure, and one that succeeds keeps its method.
    #[test]
    fn a_protected_boot_whose_measurement_fails_boots_with_method_0() {
        let id = "00112233445566778899aabbccddeeff0123456789abcdeffedcba9876543210";
        let failures = [
            (
                admission::Method::Fido2Primary,
                io::ErrorKind::NotFound,
                "open /dev/tpmrm0: No such file or directory",
            ),
            (
                admission::Method::None,
                io::ErrorKind::InvalidData,
                "TPM PCR 11 extension failed; cold boot required: TPM_RC_FAILURE",
            ),
            (
                admission::Method::TpmPin,
                io::ErrorKind::InvalidData,
                "read back SHA-256 PCR 11 failed; cold boot required: lost readback",
            ),
        ];
        for (method, kind, why) in failures {
            let mut console = Vec::new();
            let record = measured_record(
                id,
                true,
                true,
                Some(method),
                || Err(io::Error::new(kind, why)),
                &mut console,
            )
            .unwrap()
            .unwrap();
            let record = admission::Record::decode(&record).unwrap();
            assert_eq!(record.method, admission::Method::None, "{method:?}");
            assert_eq!(record.uid, admission::PRIMARY_UID);
            assert_eq!(
                String::from_utf8(console).unwrap(),
                format!("{MEASUREMENT_FAILED}\n")
            );
        }
        // A device-bound release, whose selector phase 1 gave the policy,
        // boots with the line and no record.
        let mut console = Vec::new();
        let record = measured_record(
            id,
            true,
            true,
            None,
            || Err(io::Error::new(io::ErrorKind::NotFound, "no TPM")),
            &mut console,
        )
        .unwrap();
        assert_eq!(record, None);
        assert_eq!(
            String::from_utf8(console).unwrap(),
            format!("{MEASUREMENT_FAILED}\n")
        );
        // An unencrypted boot: the failure aborts, before any record.
        let mut console = Vec::new();
        let error = measured_record(
            id,
            true,
            false,
            None,
            || Err(io::Error::new(io::ErrorKind::NotFound, "no TPM")),
            &mut console,
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "no TPM");
        assert!(console.is_empty());
        // A measurement that succeeds keeps the method; none is taken
        // without a policy.
        let mut console = Vec::new();
        let record = measured_record(
            id,
            true,
            true,
            Some(admission::Method::Fido2Primary),
            || Ok([7; 32]),
            &mut console,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            admission::Record::decode(&record).unwrap().method,
            admission::Method::Fido2Primary
        );
        assert!(String::from_utf8(console)
            .unwrap()
            .starts_with("td-boot: TD-BOOT-MEASURED-PCR11 0707"));
        let record = measured_record(
            id,
            false,
            true,
            Some(admission::Method::TpmPin),
            || panic!("measured without a policy"),
            &mut Vec::new(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            admission::Record::decode(&record).unwrap().method,
            admission::Method::None
        );
        assert_eq!(
            measured_record(
                id,
                false,
                false,
                None,
                || panic!("measured"),
                &mut Vec::new()
            )
            .unwrap(),
            None
        );
    }

    /// The selector names its pipes to td-kexec by the mode that carries
    /// them: `--fds-key` with the key alone, `--fds-key-admit` with both.
    #[test]
    fn the_admitting_handoff_names_both_pipes() {
        let args = |record: Option<&Path>| {
            let command = key_kexec_command(
                File::open("/dev/null").unwrap(),
                File::open("/dev/null").unwrap(),
                "d",
                Path::new("/proc/1/fd/3"),
                record,
                OsStr::new("c"),
            );
            command
                .get_args()
                .map(|arg| arg.to_str().unwrap().to_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(args(None), ["--fds-key", "d", "/proc/1/fd/3", "c"]);
        assert_eq!(
            args(Some(Path::new("/proc/1/fd/4"))),
            ["--fds-key-admit", "d", "/proc/1/fd/3", "/proc/1/fd/4", "c"]
        );
    }

    /// Nothing `run_boot` hands to kexec skipped the id/manifest binding.
    ///
    /// A property of the SOURCE because `run_boot` needs a block device, a
    /// mount and a working kexec, and no unit test has any of the three — so
    /// its exit paths cannot be driven here at all. Each of them ends in
    /// `kexec_boot_decision`, and what must hold is that the `Deployment` each
    /// one passes came from a reader that checks the manifest hashes to the
    /// id and the payloads to the manifest: `read_only_current`,
    /// `read_only_recovery` or `verify_deployment`, re-read under the mount
    /// whose handles kexec receives. A payload read alone binds nothing.
    #[test]
    fn run_boot_reaches_kexec_through_verifying_readers_only() {
        let source = include_str!("main.rs");
        let start = source
            .split_once("\nfn run_boot(")
            .map(|(_, rest)| rest)
            .unwrap();
        let body = start.split_once("\n}\n").map(|(body, _)| body).unwrap();
        assert!(
            body.contains("kexec_boot_decision"),
            "the scan lost run_boot's body"
        );
        for unbound in ["verify_payload(", "open_bundle(", "manifest_bytes("] {
            assert!(
                !body.contains(unbound),
                "run_boot names {unbound}: every deployment it kexecs must come \
                 from a reader that binds the manifest to its id"
            );
        }
        assert_eq!(
            body.matches("verify_deployment(mountpoint, &decision.deployment_id)?")
                .count(),
            2,
            "both post-transaction paths re-verify under the mount kexec reads"
        );
        // The policy comes from the selector's OWN rootfs, never the volume
        // being selected from.
        assert!(!body.contains("measurement::enabled(mountpoint"));
    }

    #[test]
    fn read_only_fast_path_accepts_only_successful_current() {
        let fixture = Fixture::new();
        let initial = fixture.valid_deployment();
        fixture.selector("current", &initial);
        fixture.selector("previous", &initial);
        assert_eq!(
            read_only_current(&fixture.root).map(|(decision, _)| decision.deployment_id),
            Some(initial.clone())
        );

        let (source, candidate) = fixture.source_bundle("candidate", "next");
        install_deployment(&fixture.root, &source).unwrap();

        assert!(read_only_current(&fixture.root).is_none());
        assert_eq!(read_selector(&fixture.root, "current").unwrap(), candidate);
    }

    #[test]
    fn read_only_recovery_prefers_previous_but_keeps_a_verified_current_available() {
        let fixture = Fixture::new();
        let initial = fixture.valid_deployment();
        fixture.selector("current", &initial);
        fixture.selector("previous", &initial);
        let (source, candidate) = fixture.source_bundle("candidate", "next");
        install_deployment(&fixture.root, &source).unwrap();
        let transaction_error = io::Error::other("read-write mount rejected");

        let (decision, _) = read_only_recovery(&fixture.root, &transaction_error).unwrap();

        assert_eq!(decision.slot, "previous");
        assert_eq!(decision.deployment_id, initial);
        assert!(decision.bookkeeping_error.is_some());

        fs::remove_dir_all(
            fixture
                .root
                .join("td/deployments")
                .join(&decision.deployment_id),
        )
        .unwrap();
        let (decision, _) = read_only_recovery(&fixture.root, &transaction_error).unwrap();
        assert_eq!(decision.slot, "current");
        assert_eq!(decision.deployment_id, candidate);
    }

    /// The read-only paths boot what they verify: a slot whose kernel or
    /// manifest no longer matches is declined by the fast path and never
    /// chosen by recovery, which falls to the other slot or refuses.
    #[test]
    fn read_only_paths_never_choose_a_corrupt_slot() {
        for tampered in ["bzImage", "manifest"] {
            let corrupt = |root: &Path, id: &str| {
                let path = root.join("td/deployments").join(id).join(tampered);
                let mut bytes = fs::read(&path).unwrap();
                bytes.extend_from_slice(b"tampered");
                fs::write(&path, bytes).unwrap();
            };
            let error = io::Error::other("read-write mount rejected");

            let fixture = Fixture::new();
            let initial = fixture.valid_deployment();
            fixture.selector("current", &initial);
            fixture.selector("previous", &initial);
            corrupt(&fixture.root, &initial);
            assert!(
                read_only_current(&fixture.root).is_none(),
                "the fast path must decline a corrupt {tampered}"
            );
            assert!(
                read_only_recovery(&fixture.root, &error).is_err(),
                "recovery must refuse when every slot has a corrupt {tampered}"
            );

            // One corrupt slot of two: recovery boots the other.
            for corrupt_previous in [false, true] {
                let fixture = Fixture::new();
                let initial = fixture.valid_deployment();
                fixture.selector("current", &initial);
                fixture.selector("previous", &initial);
                let (source, candidate) = fixture.source_bundle("candidate", "next");
                install_deployment(&fixture.root, &source).unwrap();
                let (bad, good, slot) = if corrupt_previous {
                    (&initial, &candidate, "current")
                } else {
                    (&candidate, &initial, "previous")
                };
                corrupt(&fixture.root, bad);
                let (decision, _) = read_only_recovery(&fixture.root, &error).unwrap();
                assert_eq!(decision.slot, slot, "a corrupt {tampered}");
                assert_eq!(&decision.deployment_id, good, "a corrupt {tampered}");
            }
        }
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
    fn boot_selection_promotes_a_verified_previous_after_current_corruption() {
        let fixture = Fixture::new();
        let initial = fixture.valid_deployment();
        fixture.selector("current", &initial);
        fixture.selector("previous", &initial);
        let (source, candidate) = fixture.source_bundle("candidate", "next");
        install_deployment(&fixture.root, &source).unwrap();
        fs::write(
            fixture
                .root
                .join("td/deployments")
                .join(candidate)
                .join("root.erofs"),
            b"tampered\n",
        )
        .unwrap();

        let selected = select_boot_deployment(&fixture.root).unwrap();

        assert_eq!(selected.slot, "previous");
        assert_eq!(selected.deployment_id, initial);
        assert!(selected.current_error.is_some());
        assert_eq!(
            read_selector(&fixture.root, "current").unwrap(),
            selected.deployment_id
        );
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
            kernel_cmdline(OsStr::new("console=ttyS0 quiet"), &id, false).unwrap(),
            OsString::from(format!("console=ttyS0 quiet td.deployment={id}"))
        );
        assert_eq!(
            kernel_cmdline(OsStr::new("console=ttyS0"), &id, true).unwrap(),
            OsString::from(format!(
                "console=ttyS0 td.deployment={id} {}",
                protocol::BOOKKEEPING_UNAVAILABLE_CMDLINE_TOKEN
            ))
        );
        assert!(kernel_cmdline(OsStr::new("td.deployment=old"), &id, false).is_err());
        assert!(kernel_cmdline(
            OsStr::new(protocol::BOOKKEEPING_UNAVAILABLE_CMDLINE_TOKEN),
            &id,
            false
        )
        .is_err());
        assert!(kernel_cmdline(OsStr::new("\"td.deployment=old\""), &id, false).is_err());
        assert!(kernel_cmdline(OsStr::new("foo=\"unterminated"), &id, false).is_err());
        assert!(kernel_cmdline(OsStr::new("foo\\ bar"), &id, false).is_err());
        assert!(kernel_cmdline(OsStr::from_bytes(b"quiet\nbad"), &id, false).is_err());
    }

    #[test]
    fn cmdline_token_matching_is_field_exact() {
        let token = protocol::BOOKKEEPING_UNAVAILABLE_CMDLINE_TOKEN.as_bytes();
        assert!(cmdline_has_token(
            b"quiet td.boot-bookkeeping-unavailable=1 console=ttyS0\n",
            token
        ));
        assert!(!cmdline_has_token(
            b"quiet x=td.boot-bookkeeping-unavailable=1",
            token
        ));
        assert!(!cmdline_has_token(
            b"td.boot-bookkeeping-unavailable=10",
            token
        ));
        let id = "a".repeat(64);
        let matching = format!(
            "td.deployment={id} {}",
            protocol::BOOKKEEPING_UNAVAILABLE_CMDLINE_TOKEN
        );
        assert_eq!(
            success_disposition(b"quiet console=ttyS0", &id),
            SuccessDisposition::Record
        );
        assert_eq!(
            success_disposition(matching.as_bytes(), &id),
            SuccessDisposition::ConfirmRecovery
        );
        assert_eq!(
            success_disposition(matching.as_bytes(), &"b".repeat(64)),
            SuccessDisposition::RejectRecovery
        );
    }

    #[test]
    fn cmdline_limit_reserves_the_kernel_nul() {
        let id = "c".repeat(64);
        let token_len = format!("td.deployment={id}").len();
        let accepted = "x".repeat(MAX_CMDLINE_BYTES - token_len - 2);
        let output = kernel_cmdline(OsStr::new(&accepted), &id, false).unwrap();
        assert_eq!(output.as_bytes().len() + 1, MAX_CMDLINE_BYTES);

        let rejected = "x".repeat(accepted.len() + 1);
        assert!(kernel_cmdline(OsStr::new(&rejected), &id, false).is_err());
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
        let stacked =
            b"36 25 0:35 / /run/td\\040update rw,nodev,nosuid,noexec - btrfs /dev/vda rw\n\
37 25 0:36 / /run/td\\040update rw,nodev,nosuid,noexec - btrfs /dev/vda rw\n";
        assert!(mounted_btrfs_source(stacked, mountpoint).is_err());
        assert!(decode_mountinfo_field(b"/run/bad\\09x").is_err());
    }

    #[test]
    fn writable_failures_preserve_transaction_outcomes() {
        let operation =
            finish_writable_operation::<()>(Err(invalid("invalid attempt state")), Ok(()));
        assert!(matches!(
            operation,
            Err(WritableVolumeFailure::Transaction(_))
        ));

        let storage_full = finish_writable_operation::<()>(
            Err(io::Error::from(io::ErrorKind::StorageFull)),
            Ok(()),
        );
        assert!(matches!(
            storage_full,
            Err(WritableVolumeFailure::Transaction(_))
        ));

        let io_failure =
            finish_writable_operation::<()>(Err(io::Error::other("I/O error")), Ok(()));
        assert!(matches!(
            io_failure,
            Err(WritableVolumeFailure::Transaction(_))
        ));

        let committed = finish_writable_operation(Ok(()), Err(io::Error::other("busy")));
        assert!(matches!(
            committed,
            Err(WritableVolumeFailure::Committed { .. })
        ));

        let failed_and_busy = finish_writable_operation::<()>(
            Err(invalid("invalid attempt state")),
            Err(io::Error::other("busy")),
        );
        assert!(matches!(
            failed_and_busy,
            Err(WritableVolumeFailure::Mounted(_))
        ));
    }

    #[test]
    fn boot_commands_pin_mount_options_and_fd_handoff() {
        // mount/umount are td-init applets called by their /bin names — NOT
        // `busybox <applet>` — since the mount(2)/umount2(2) amendment. The
        // program IS the applet name here, so pin the two to each other: a path
        // that stopped matching its protocol name would send td-boot to a
        // symlink the image does not pack.
        assert_eq!(TD_MOUNT, format!("/bin/{}", protocol::MOUNT_APPLET));
        assert_eq!(TD_UMOUNT, format!("/bin/{}", protocol::UMOUNT_APPLET));
        assert_eq!(TD_LOSETUP, format!("/bin/{}", protocol::LOSETUP_APPLET));
        let mount = mount_command(Path::new("/dev/vda"), Path::new("/run/td-volume"));
        assert_eq!(mount.get_program(), OsStr::new(TD_MOUNT));
        assert_eq!(
            mount.get_args().collect::<Vec<_>>(),
            vec![
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
                OsStr::new("-t"),
                OsStr::new("btrfs"),
                OsStr::new("-o"),
                OsStr::new("rw,nodev,nosuid,noexec"),
                OsStr::new("/dev/vda"),
                OsStr::new("/run/td-update"),
            ]
        );
        let unmount = unmount_command(Path::new("/run/td-update"));
        assert_eq!(unmount.get_program(), OsStr::new(TD_UMOUNT));
        assert_eq!(
            unmount.get_args().collect::<Vec<_>>(),
            vec![OsStr::new("/run/td-update")]
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
        assert_eq!(
            command.get_program(),
            OsStr::new(TD_LOSETUP),
            "the root loop must be attached by td's own applet, not a third-party multicall"
        );
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            vec![
                OsStr::new("-r"),
                OsStr::new("/dev/loop0"),
                OsStr::new(STDIN_PATH),
            ]
        );
    }

    #[test]
    fn validate_source_reports_the_id_only_after_checking_every_payload() {
        let fixture = Fixture::new();
        let (directory, id) = fixture.source_bundle("source", "");
        let mut output = Vec::new();
        run_validate_source(&directory, &mut output).unwrap();
        assert_eq!(output, format!("{id}\n").as_bytes());

        for (name, original) in fixture_payloads("") {
            let path = directory.join(name);
            assert_eq!(fs::read(&path).unwrap(), original.as_bytes());
            fs::write(&path, b"tampered").unwrap();
            output.clear();
            let error = run_validate_source(&directory, &mut output).unwrap_err();
            assert!(error
                .to_string()
                .contains(&format!("{name} hash mismatch:")));
            assert!(output.is_empty());
            assert_eq!(fs::read(&path).unwrap(), b"tampered");
            fs::write(&path, &original).unwrap();
        }
        assert_eq!(
            fs::read(directory.join("manifest")).unwrap(),
            fixture_manifest("").as_bytes()
        );
    }

    #[test]
    fn validate_source_refuses_missing_and_symlinked_payloads() {
        let fixture = Fixture::new();
        let (directory, _) = fixture.source_bundle("source", "");
        for (name, original) in fixture_payloads("") {
            let path = directory.join(name);
            let saved = directory.join(format!("{name}.saved"));
            fs::rename(&path, &saved).unwrap();
            let mut output = Vec::new();
            let error = run_validate_source(&directory, &mut output).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::NotFound, "{error}");
            assert!(output.is_empty());
            symlink(&saved, &path).unwrap();
            let error = run_validate_source(&directory, &mut output).unwrap_err();
            assert!(
                error.to_string().contains("must be a real regular file"),
                "{error}"
            );
            assert!(output.is_empty());
            assert_eq!(fs::read(&saved).unwrap(), original.as_bytes());
            assert_eq!(fs::read_link(&path).unwrap(), saved);
            fs::remove_file(&path).unwrap();
            fs::rename(&saved, &path).unwrap();
        }
    }

    #[test]
    fn validate_source_refuses_relative_directories_and_special_payloads() {
        let fixture = Fixture::new();
        let (directory, _) = fixture.source_bundle("source", "");
        let mut output = Vec::new();
        let error = run_validate_source(Path::new("relative"), &mut output).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(error
            .to_string()
            .contains("deployment directory must be an absolute path"));
        assert!(output.is_empty());
        for (name, original) in fixture_payloads("") {
            let path = directory.join(name);
            fs::remove_file(&path).unwrap();
            let socket = std::os::unix::net::UnixListener::bind(&path).unwrap();
            let error = run_validate_source(&directory, &mut output).unwrap_err();
            assert!(
                error.to_string().contains("must be a real regular file"),
                "{error}"
            );
            assert!(output.is_empty());
            drop(socket);
            fs::remove_file(&path).unwrap();
            fs::write(&path, original).unwrap();
        }
    }

    #[test]
    fn validate_source_takes_exactly_its_directory() {
        let parsed = parse_args(
            ["validate-source", "/bundle"]
                .map(OsString::from)
                .into_iter(),
        )
        .unwrap();
        let Mode::ValidateSource { directory } = parsed else {
            panic!("validate-source must select its read-only operation");
        };
        assert_eq!(directory, PathBuf::from("/bundle"));
        for args in [
            vec!["validate-source"],
            // A former trusted-key operand is refused, not ignored.
            vec!["validate-source", "/bundle", "/key.pub"],
            vec!["on-volume", "validate-source", "/bundle"],
            // `authenticate` is no longer a verb.
            vec!["authenticate", "/bundle"],
        ] {
            assert!(parse_args(args.into_iter().map(OsString::from)).is_err());
        }
    }

    #[test]
    fn live_verbs_take_exact_operands_and_a_canonical_id() {
        let id = "a".repeat(64);
        assert!(matches!(
            parse_args(args(&["live-boot", "/media", "quiet"])).unwrap(),
            Mode::LiveBoot { .. }
        ));
        assert!(matches!(
            parse_args(args(&["live-root", "/media", &id, "/dev/loop0"])).unwrap(),
            Mode::LiveRoot { .. }
        ));
        for bad in [
            vec!["live-boot", "/media"],
            vec!["live-boot", "/media", "quiet", "extra"],
            vec!["live-root", "/media", &id],
            vec!["live-root", "/media", "not-an-id", "/dev/loop0"],
            vec!["live-root", "/media", &id, "/dev/loop0", "extra"],
            vec!["on-volume", "live-boot", "/media", "quiet"],
        ] {
            assert!(parse_args(args(&bad)).is_err(), "{bad:?}");
        }
        let usage = usage_error().to_string();
        assert!(usage.contains("td-boot live-boot <mountpoint> <cmdline>"));
        assert!(usage.contains("td-boot live-root <mountpoint> <deployment-id> <loop-device>"));
    }

    #[test]
    fn the_live_ram_disk_is_half_of_memory_with_a_floor() {
        let meminfo = |total: &str| format!("MemTotal:       {total} kB\nMemFree:  1 kB\n");
        assert_eq!(
            live_ram_disk_kib(meminfo("4194304").as_bytes()).unwrap(),
            2097152
        );
        assert_eq!(
            live_ram_disk_kib(meminfo("1048576").as_bytes()).unwrap(),
            LIVE_MIN_VOLUME_KIB
        );
        assert!(live_ram_disk_kib(meminfo("1048574").as_bytes()).is_err());
        for bad in [
            "",
            "MemFree: 4194304 kB\n",
            "MemTotal: 4194304 MB\n",
            "MemTotal: -1 kB\n",
        ] {
            assert!(live_ram_disk_kib(bad.as_bytes()).is_err(), "{bad}");
        }
        assert!(live_ram_disk_kib(b"MemTotal: \xff kB\n").is_err());
    }

    #[test]
    fn the_live_handoff_adds_its_tokens_once_and_no_base_line_can_carry_them() {
        let id = "b".repeat(64);
        let uuid = volume::Uuid::parse("12345678-90ab-4def-8234-567890abcdef").unwrap();
        let line =
            live_cmdline(OsStr::new("console=ttyS0 rdinit=/init"), &uuid, 2048, &id).unwrap();
        assert_eq!(
            line.to_str().unwrap(),
            format!(
                "console=ttyS0 rdinit=/init td.volume={uuid} td.deployment={id} \
                 td.live=1 brd.rd_size=2048 panic=0"
            )
        );
        assert_eq!(volume::handoff(line.as_bytes()).unwrap(), uuid);
        assert!(cmdline_has_token(line.as_bytes(), b"td.live=1"));
        assert!(line.as_bytes().ends_with(b"panic=0"));
        for reserved in [
            "td.live=1",
            "td.live=0",
            "brd.rd_size=1",
            "td.deployment=x",
            "td.volume=12345678-90ab-4def-8234-567890abcdef",
        ] {
            let base = format!("quiet {reserved}");
            assert!(
                live_cmdline(OsStr::new(&base), &uuid, 2048, &id).is_err(),
                "{reserved}"
            );
        }
        // An installed selector's line is refused the same live tokens.
        for reserved in ["td.live=1", "brd.rd_size=4096"] {
            let base = format!("quiet {reserved}");
            assert!(
                kernel_cmdline(OsStr::new(&base), &id, false).is_err(),
                "{reserved}"
            );
        }
    }

    #[test]
    fn media_names_are_the_lowercased_iso_names_of_every_deployment_file() {
        assert_eq!(media_name("bzImage").unwrap(), "bzimage");
        for name in [protocol::MANIFEST_NAME, "initramfs.cpio", "root.erofs"] {
            assert_eq!(media_name(name).unwrap(), name);
        }
        assert!(media_name("manifest.sig").is_err());
        for (iso, name) in protocol::MEDIA_DEPLOYMENT_FILES {
            assert_eq!(media_name(name).unwrap(), iso.to_ascii_lowercase());
        }
        for bad in ["bzimage", "BZIMAGE", "selector.cpio", ""] {
            assert!(media_name(bad).is_err(), "{bad}");
        }
    }

    /// A fixture deployment laid out the way a mounted install medium shows it.
    fn media_bundle(fixture: &Fixture) -> (PathBuf, String) {
        let (directory, id) = fixture.source_bundle("medium", "");
        fs::rename(directory.join("bzImage"), directory.join("bzimage")).unwrap();
        (directory, id)
    }

    #[test]
    fn a_live_medium_is_verified_before_its_boot_payloads_are_trusted() {
        let fixture = Fixture::new();
        let (medium, id) = media_bundle(&fixture);
        let deployment = verified_media_deployment(&medium).unwrap();
        assert_eq!(deployment.id, id);

        for name in ["bzimage", "initramfs.cpio"] {
            let path = medium.join(name);
            let original = fs::read(&path).unwrap();
            fs::write(&path, b"tampered").unwrap();
            let error = verified_media_deployment(&medium).err().unwrap();
            assert!(
                error.to_string().contains("hash mismatch"),
                "{name}: {error}"
            );
            fs::write(&path, original).unwrap();
        }
        // The root is hashed by live-root, at the descriptor it binds.
        fs::write(medium.join("root.erofs"), b"checked after kexec").unwrap();
        assert!(verified_media_deployment(&medium).is_ok());
    }

    #[test]
    fn a_mounted_medium_is_a_source_directory_as_it_stands() {
        let fixture = Fixture::new();
        let (medium, id) = media_bundle(&fixture);
        let bundle = open_bundle(&medium, Names::Source).unwrap();
        assert_eq!(bundle.id, id);

        // The medium's spelling is held to the manifest like the other.
        let kernel = medium.join("bzimage");
        let original = fs::read(&kernel).unwrap();
        fs::write(&kernel, b"tampered").unwrap();
        let error = open_bundle(&medium, Names::Source).err().unwrap();
        assert!(
            error.to_string().contains("bzimage hash mismatch"),
            "{error}"
        );
        fs::write(&kernel, &original).unwrap();

        // Both spellings as two files are refused, even both intact; one file
        // under both names, as a case-insensitive filesystem shows it, is not.
        fs::hard_link(&kernel, medium.join("bzImage")).unwrap();
        assert_eq!(open_bundle(&medium, Names::Source).unwrap().id, id);
        fs::remove_file(medium.join("bzImage")).unwrap();
        fs::write(medium.join("bzImage"), &original).unwrap();
        let error = open_bundle(&medium, Names::Source).err().unwrap();
        assert!(error.to_string().contains("holds both"), "{error}");
        fs::remove_file(&kernel).unwrap();
        assert_eq!(open_bundle(&medium, Names::Source).unwrap().id, id);

        // Neither: the refusal names the deployment spelling.
        fs::remove_file(medium.join("bzImage")).unwrap();
        let error = open_bundle(&medium, Names::Source).err().unwrap();
        assert!(error.to_string().contains("bzImage"), "{error}");
    }

    #[test]
    fn a_medium_source_publishes_under_the_names_the_boot_path_opens() {
        let fixture = Fixture::new();
        let (medium, id) = media_bundle(&fixture);
        assert_eq!(install_deployment(&fixture.root, &medium).unwrap(), id);
        let published = fixture.root.join(protocol::DEPLOYMENTS_DIR).join(&id);
        assert!(published.join("bzImage").is_file());
        assert!(!published.join("bzimage").exists());
        assert_eq!(verify_deployment(&fixture.root, &id).unwrap().id, id);

        // A published deployment is held to the boot path's names: one that
        // holds only the medium's spelling is not the deployment to keep.
        fs::rename(published.join("bzImage"), published.join("bzimage")).unwrap();
        assert!(verify_deployment(&fixture.root, &id).is_err());
        // Pinned at the classifier: as `current` it is refused earlier, but an
        // inactive one would otherwise read as already published and be kept.
        let want = open_bundle(&medium, Names::Source).unwrap();
        assert!(existing_is_same(&published, &want).is_err());
        assert!(install_deployment(&fixture.root, &medium).is_err());
    }

    #[test]
    fn a_kernel_under_its_deployment_spelling_is_not_the_media_kernel() {
        let fixture = Fixture::new();
        let (directory, _) = fixture.source_bundle("unmapped", "");
        let error = verified_media_deployment(&directory).err().unwrap();
        assert!(error.to_string().contains("bzimage"), "{error}");
    }

    #[test]
    fn a_malformed_media_manifest_is_refused() {
        let fixture = Fixture::new();
        let (medium, _) = media_bundle(&fixture);
        fs::write(medium.join("manifest"), b"not a manifest\n").unwrap();
        assert!(verified_media_deployment(&medium).is_err());
    }

    #[test]
    fn live_root_binds_only_the_selected_medium_and_an_intact_root() {
        let fixture = Fixture::new();
        let (medium, id) = media_bundle(&fixture);
        assert!(live_root_payload(&medium, &id).is_ok());

        let error = live_root_payload(&medium, &"c".repeat(64)).err().unwrap();
        assert!(error.to_string().contains("not the selected"), "{error}");

        let (other, other_id) = fixture.source_bundle("other-medium", "next");
        assert_ne!(other_id, id);
        let error = live_root_payload(&other, &id).err().unwrap();
        assert!(
            error.to_string().contains("not the selected"),
            "a medium holding another deployment: {error}"
        );

        fs::write(medium.join("root.erofs"), b"tampered").unwrap();
        let error = live_root_payload(&medium, &id).err().unwrap();
        assert!(error.to_string().contains("hash mismatch"), "{error}");
    }

    #[test]
    fn live_root_requires_the_live_handoff_naming_its_deployment() {
        let id = "d".repeat(64);
        let line = format!("quiet td.deployment={id} td.live=1 brd.rd_size=4");
        assert!(require_live_handoff(line.as_bytes(), &id).is_ok());
        for bad in [
            format!("quiet td.deployment={id}"),
            format!("quiet td.deployment={id} td.live=0"),
            format!("quiet td.deployment={id} td.live=1x"),
            format!("quiet td.deployment={} td.live=1", "e".repeat(64)),
            "quiet td.live=1".to_string(),
        ] {
            assert!(require_live_handoff(bad.as_bytes(), &id).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_live_handoff_refuses_a_line_that_ends_kernel_parameters() {
        let id = "b".repeat(64);
        let uuid = volume::Uuid::parse("12345678-90ab-4def-8234-567890abcdef").unwrap();
        assert!(live_cmdline(OsStr::new("quiet -- single"), &uuid, 2048, &id).is_err());
        assert!(live_cmdline(OsStr::new("quiet --"), &uuid, 2048, &id).is_err());
        assert!(live_cmdline(OsStr::new("quiet a--b --x"), &uuid, 2048, &id).is_ok());
        // An installed handoff appends nothing brd must see, so it is unchanged.
        assert!(kernel_cmdline(OsStr::new("quiet -- single"), &id, false).is_ok());
    }

    #[test]
    fn the_live_handoff_is_bounded_by_the_kernel_command_line() {
        let id = "b".repeat(64);
        let uuid = volume::Uuid::parse("12345678-90ab-4def-8234-567890abcdef").unwrap();
        let fits = |base: &str| live_cmdline(OsStr::new(base), &uuid, 2048, &id);
        let appended = fits("x").unwrap().len() - 1;
        let longest = "x".repeat(MAX_CMDLINE_BYTES - appended - 1);
        assert_eq!(fits(&longest).unwrap().len() + 1, MAX_CMDLINE_BYTES);
        assert!(fits(&format!("{longest}x")).is_err());
    }

    #[test]
    fn a_live_selector_is_provisioned_as_exactly_that() {
        let fixture = Fixture::new();
        let rootfs = &fixture.root;
        let marker = rootfs.join(protocol::LIVE_MEDIA_MARKER_PATH);
        fs::create_dir_all(marker.parent().unwrap()).unwrap();

        assert!(check_live_selector(rootfs).is_err(), "no marker");
        fs::write(&marker, protocol::LIVE_MEDIA_MARKER).unwrap();
        check_live_selector(rootfs).unwrap();

        for wrong in [&b"td-live-media-v1"[..], b"td-live-media-v2\n", b""] {
            fs::write(&marker, wrong).unwrap();
            assert!(check_live_selector(rootfs).is_err(), "{wrong:?}");
        }
        // A symlink to the exact bytes is still refused: the marker is part of
        // the selector's own rootfs, never a pointer elsewhere.
        let elsewhere = fixture.root.join("marker-elsewhere");
        fs::write(&elsewhere, protocol::LIVE_MEDIA_MARKER).unwrap();
        fs::remove_file(&marker).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &marker).unwrap();
        assert!(check_live_selector(rootfs).is_err(), "a symlinked marker");
        fs::remove_file(&marker).unwrap();
        fs::write(&marker, protocol::LIVE_MEDIA_MARKER).unwrap();

        for path in [
            protocol::VOLUME_UUID_PATH,
            protocol::MEASUREMENT_POLICY_PATH,
        ] {
            let extra = rootfs.join(path);
            fs::write(&extra, b"x").unwrap();
            let error = check_live_selector(rootfs).err().unwrap();
            assert!(error.to_string().contains(path), "{error}");
            fs::remove_file(&extra).unwrap();
        }
        check_live_selector(rootfs).unwrap();
    }

    /// What production passes, stated as a literal; the parameter is for the
    /// test above.
    #[test]
    fn the_live_selector_is_checked_on_its_own_rootfs() {
        let source = include_str!("main.rs");
        let body = source
            .split_once("\nfn run_live_boot(")
            .and_then(|(_, rest)| rest.split_once("\n}\n"))
            .map(|(body, _)| body)
            .unwrap();
        assert!(body.contains("check_live_selector(Path::new(\"/\"))?"));
    }

    /// The deployment initramfs's order: mount-root takes the key before the
    /// volume is found, so every refusal after it leaves no key; the unlock
    /// (its post-cap check, then cryptsetup) runs inside the binding, a halt
    /// halts rather than returning, and nothing is mounted or reported
    /// before the binding succeeds.
    #[test]
    fn mount_root_takes_the_key_before_discovery_and_unlocks_before_mounting() {
        let source = include_str!("main.rs");
        let body = source
            .split_once("\nfn on_volume(")
            .and_then(|(_, rest)| rest.split_once("\n}\n"))
            .map(|(body, _)| body)
            .unwrap();
        let position = |needle: &str| {
            assert_eq!(body.matches(needle).count(), 1, "{needle}");
            body.find(needle).unwrap()
        };
        let taken = position(
            "Mode::MountVolume { var: false, .. } => unlock::take_key(Path::new(\"/\"), 0)?,",
        );
        let order = [
            taken,
            position("volume::handoff("),
            position("volume::Opened::open(&uuid)?"),
            position("bind_volume(&mut operation, opened, key, |partition, key| {"),
            position("unlock::unlock("),
            position("Binding::Halt(reason) => halt(&reason),"),
            position("report_volume_binding("),
            position("dispatch(operation)"),
        ];
        assert!(order.windows(2).all(|pair| pair[0] < pair[1]), "{body}");
        let unlock = include_str!("unlock.rs");
        let body = unlock
            .split_once("\npub(crate) fn unlock<U: Unlocker>(")
            .and_then(|(_, rest)| rest.split_once("\n}\n"))
            .map(|(body, _)| body)
            .unwrap();
        let checked = body.find("check_cap(unlocker, out)").unwrap();
        let opened = body.find("open_by_volume_key_args(").unwrap();
        let admitted = body.find("unlocker.admit()").unwrap();
        assert!(checked < opened && opened < admitted);
    }

    /// The cap runs once, after the selector's own provisioning check and
    /// before anything else, the medium's discovery and mount included; a
    /// refusal halts rather than returning, which would exit init.
    #[test]
    fn the_live_selector_caps_before_it_reads_the_medium() {
        let source = include_str!("main.rs");
        let body = source
            .split_once("\nfn run_live_boot(")
            .and_then(|(_, rest)| rest.split_once("\n}\n"))
            .map(|(body, _)| body)
            .unwrap();
        let position = |needle: &str| {
            assert_eq!(body.matches(needle).count(), 1, "{needle}");
            body.find(needle).unwrap()
        };
        let checked = position("check_live_selector(Path::new(\"/\"))?;");
        let capped = position(
            "cap::live().unwrap_or_else(|refusal| halt(&format!(\"{refusal}; live boot refused\")));",
        );
        let announced =
            position("let _ = writeln!(mirror::stderr(), \"td-boot: capping PCR 12\");");
        assert!(checked < announced && announced < capped, "{body}");
        assert!(
            capped
                < position(
                    "let _ = writeln!(mirror::stderr(), \"td-boot: {}\", capped.describe());"
                )
        );
        for later in [
            "live_ram_disk_kib(",
            "volume::Uuid::random()",
            "mount_medium(mountpoint)",
            "verified_media_deployment(",
            "kexec_command(",
        ] {
            assert!(capped < position(later), "{later} precedes the cap");
        }
        // Only run_live_boot caps through cap.rs, and it never releases.
        let (production, _) = source.split_once("\n#[cfg(test)]\nmod tests {").unwrap();
        assert_eq!(production.matches("cap::live").count(), 1);
        for release in ["selector_release::", "boot_encrypted(", "Luks::new("] {
            assert!(!body.contains(release), "the live selector names {release}");
        }
        // Halts: the live selector's cap, the installed one's release
        // (`boot_encrypted`, two calls) and a selector update's failed
        // restart there, and the deployment initramfs's unlock
        // (`on_volume`), and live early-init setup.
        assert_eq!(
            production.matches("halt(").count(),
            7,
            "its definition and the six calls"
        );
        let encrypted = source
            .split_once("\nfn boot_encrypted(")
            .and_then(|(_, rest)| rest.split_once("\n}\n"))
            .map(|(body, _)| body)
            .unwrap();
        assert_eq!(body.matches("halt(").count(), 1);
        assert_eq!(encrypted.matches("halt(").count(), 3);
        assert!(encrypted.contains("halt(&selector_update::restart());"));

        let halt = source
            .split_once("\nfn halt(reason: &str) -> ! {\n")
            .and_then(|(_, rest)| rest.split_once("\n}\n"))
            .map(|(body, _)| body)
            .unwrap();
        assert!(halt.contains("loop {\n        std::thread::park();\n    }"));
        for forbidden in ["exit", "return", "abort", "break", "?"] {
            assert!(!halt.contains(forbidden), "halt contains {forbidden}");
        }
    }

    #[test]
    fn a_live_seed_holds_only_what_the_booted_system_reads_from_its_volume() {
        use std::os::unix::fs::PermissionsExt;
        let fixture = Fixture::new();
        let (medium, id) = media_bundle(&fixture);
        let seed = fixture.root.join("seed");
        live_seed(&medium, &id, &seed).unwrap();

        let mut found = Vec::new();
        let mut pending = vec![seed.clone()];
        while let Some(directory) = pending.pop() {
            for entry in fs::read_dir(&directory).unwrap() {
                let path = entry.unwrap().path();
                let metadata = fs::symlink_metadata(&path).unwrap();
                let relative = path.strip_prefix(&seed).unwrap().display().to_string();
                if metadata.is_dir() {
                    pending.push(path);
                }
                found.push((relative, metadata.permissions().mode() & 0o7777));
            }
        }
        found.sort();
        let deployment = format!("td/deployments/{id}");
        let mut expected = vec![
            ("@var".to_string(), 0o755),
            ("td".to_string(), 0o755),
            ("td/boot".to_string(), 0o755),
            ("td/boot/current".to_string(), 0o777),
            ("td/deployments".to_string(), 0o755),
            (deployment.clone(), 0o755),
            (format!("{deployment}/manifest"), 0o644),
            ("td/incoming".to_string(), 0o755),
        ];
        expected.sort();
        assert_eq!(found, expected);
        assert_eq!(read_selector(&seed, "current").unwrap(), id);
        assert_eq!(
            fs::read(seed.join(&deployment).join("manifest")).unwrap(),
            fs::read(medium.join("manifest")).unwrap()
        );
        // A stale tree is never reused as a volume's contents.
        assert!(live_seed(&medium, &id, &seed).is_err());
        let other = fixture.root.join("other-seed");
        let error = live_seed(&medium, &"c".repeat(64), &other).err().unwrap();
        assert!(error.to_string().contains("not the selected"), "{error}");
        assert!(!other.exists(), "a refused seed left a tree behind");
        assert!(live_seed(&medium, &id, Path::new("relative")).is_err());
        // A legacy signature on the medium is not staged.
        fs::write(medium.join("manifest.sig"), b"aa11\n").unwrap();
        live_seed(&medium, &id, &other).unwrap();
        assert!(!other.join(&deployment).join("manifest.sig").exists());
    }

    const PROVISION_KEY_LINE: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl fixture";
    const PROVISION_FIXTURE: &str = "td-provision-v1\nuser ada\nssh-ed25519 \
        AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl fixture\n";

    /// The default medium, carrying a provision.
    fn provisioned_medium(fixture: &Fixture) -> (PathBuf, String) {
        let (medium, id) = media_bundle(fixture);
        fs::write(medium.join("provision"), PROVISION_FIXTURE).unwrap();
        (medium, id)
    }

    #[test]
    fn a_live_seed_stages_the_media_provision() {
        use std::os::unix::fs::PermissionsExt;
        let fixture = Fixture::new();
        let (medium, id) = provisioned_medium(&fixture);
        let seed = fixture.root.join("seed");
        live_seed(&medium, &id, &seed).unwrap();
        let mode = |relative: &str| {
            fs::symlink_metadata(seed.join(relative))
                .unwrap()
                .permissions()
                .mode()
                & 0o7777
        };
        assert_eq!(mode("@var/lib"), 0o755);
        assert_eq!(mode("@var/lib/td"), 0o755);
        for path in [
            protocol::PROVISIONED_NAME_PATH,
            protocol::PROVISIONED_KEYS_PATH,
        ] {
            assert_eq!(mode(path), 0o644, "{path}");
        }
        assert_eq!(
            fs::read_to_string(seed.join(protocol::PROVISIONED_NAME_PATH)).unwrap(),
            "ada\n"
        );
        assert_eq!(
            fs::read_to_string(seed.join(protocol::PROVISIONED_KEYS_PATH)).unwrap(),
            format!("{PROVISION_KEY_LINE}\n")
        );

        // A deployment's manifest is not a provision: the format line
        // separates the two, and the refusal comes before anything is staged.
        let other = fixture.root.join("other-seed");
        let provision = medium.join("provision");
        fs::copy(medium.join("manifest"), &provision).unwrap();
        let error = live_seed(&medium, &id, &other).err().unwrap();
        assert!(error.to_string().contains("format line"), "{error}");
        assert!(!other.exists());
        // A provision that is not a regular file is refused, not followed.
        fs::remove_file(&provision).unwrap();
        fs::write(medium.join("elsewhere"), PROVISION_FIXTURE).unwrap();
        std::os::unix::fs::symlink("elsewhere", &provision).unwrap();
        assert!(live_seed(&medium, &id, &other).is_err());
        assert!(!other.exists());
    }

    #[test]
    fn a_provision_is_named_on_the_medium_as_the_writer_names_it() {
        // Linux mounts ISO-9660 with `map=normal`, which lowercases names.
        assert_eq!(
            protocol::MEDIA_PROVISION_NAME.to_ascii_lowercase(),
            "provision"
        );
    }

    #[test]
    fn a_provision_parses_only_its_own_format() {
        assert_eq!(
            parse_provision(PROVISION_FIXTURE.as_bytes()).unwrap(),
            Provision {
                name: "ada".into(),
                keys: format!("{PROVISION_KEY_LINE}\n").into_bytes(),
            }
        );
        let two =
            format!("td-provision-v1\nuser a_b-9\n{PROVISION_KEY_LINE}\n{PROVISION_KEY_LINE}\n");
        assert_eq!(
            parse_provision(two.as_bytes()).unwrap().keys.len(),
            2 * (PROVISION_KEY_LINE.len() + 1)
        );
        let long = "a".repeat(33);
        for bad in [
            String::new(),
            format!("td-deployment-v1\nuser ada\n{PROVISION_KEY_LINE}\n"),
            format!("td-provision-v1\nuser ada\n{PROVISION_KEY_LINE}"),
            format!("td-provision-v1\nuser ada\r\n{PROVISION_KEY_LINE}\n"),
            format!("td-provision-v1\nuser ada\n{PROVISION_KEY_LINE}\r\n"),
            format!("td-provision-v1\nuser Ada\n{PROVISION_KEY_LINE}\n"),
            format!("td-provision-v1\nuser a/b\n{PROVISION_KEY_LINE}\n"),
            format!("td-provision-v1\nuser \n{PROVISION_KEY_LINE}\n"),
            format!("td-provision-v1\nuser {long}\n{PROVISION_KEY_LINE}\n"),
            format!("td-provision-v1\nname ada\n{PROVISION_KEY_LINE}\n"),
            "td-provision-v1\nuser ada\n".to_string(),
            "td-provision-v1\nuser ada\nssh-rsa AAAA x\n".to_string(),
            format!("td-provision-v1\nuser ada\n\n{PROVISION_KEY_LINE}\n"),
            format!("td-provision-v1\nuser ada\n{PROVISION_KEY_LINE}\u{7}\n"),
            format!("td-provision-v1\nuser ada\0\n{PROVISION_KEY_LINE}\n"),
            format!("td-provision-v1\nuser 9ops\n{PROVISION_KEY_LINE}\n"),
            format!("td-provision-v1\nuser -x\n{PROVISION_KEY_LINE}\n"),
            format!("td-provision-v1\nuser _\n{PROVISION_KEY_LINE}\n"),
            "td-provision-v1\nuser ada\nssh-ed25519 \n".to_string(),
            "td-provision-v1\nuser ada\nssh-ed25519 not*base64 c\n".to_string(),
        ] {
            assert!(parse_provision(bad.as_bytes()).is_err(), "{bad:?}");
        }
        assert!(parse_provision(b"td-provision-v1\nuser ada\n\xff\n").is_err());
    }

    #[test]
    fn live_seed_verb_takes_exact_operands() {
        let id = "a".repeat(64);
        assert!(matches!(
            parse_args(args(&["live-seed", "/media", &id, "/live-seed"])).unwrap(),
            Mode::LiveSeed { .. }
        ));
        for bad in [
            vec!["live-seed", "/media", &id],
            vec!["live-seed", "/media", "not-an-id", "/live-seed"],
            vec!["live-seed", "/media", &id, "/live-seed", "extra"],
        ] {
            assert!(parse_args(args(&bad)).is_err(), "{bad:?}");
        }
        assert!(usage_error()
            .to_string()
            .contains("td-boot live-seed <mountpoint> <deployment-id> <seed-directory>"));
    }

    /// The body of production function `name`, up to its closing brace.
    fn production_body(name: &str) -> &'static str {
        include_str!("main.rs")
            .split_once(&format!("\nfn {name}("))
            .and_then(|(_, rest)| rest.split_once("\n}\n"))
            .map(|(body, _)| body)
            .unwrap()
    }

    /// The installed selector's boot of a td LUKS2 volume, and only that,
    /// takes the release path; everything else binds as before.
    #[test]
    fn only_the_selectors_boot_of_a_luks2_volume_releases() {
        let body = production_body("on_volume");
        let opened = body
            .find("let opened = volume::Opened::open(&uuid)?;")
            .unwrap();
        let arm = body
            .find("volume::Opened::Luks2 { partition, mapping },\n        ) => return boot_encrypted(&uuid, partition, mapping, &mountpoint, &cmdline),")
            .unwrap();
        let bound = body
            .find("bind_volume(&mut operation, opened, key, ")
            .unwrap();
        assert!(opened < arm && arm < bound, "{body}");
        assert!(body[..arm].contains("Mode::Boot {"));
        assert_eq!(body.matches("boot_encrypted(").count(), 1);
        let source = include_str!("main.rs");
        let (production, _) = source.split_once("\n#[cfg(test)]\nmod tests {").unwrap();
        assert_eq!(production.matches("boot_encrypted(").count(), 2);
        assert_eq!(production.matches("selector_release::unlock(").count(), 1);
        // bind_volume refuses a selector boot of an encrypted volume that
        // reached it some other way.
        assert!(production_body("bind_volume").contains(
            "            Consumer::Selector => {\n                return Err(invalid(format!("
        ));
    }

    /// The release order and the cap, through `selector_release::unlock`, run before
    /// any mount or deployment selection, which are `run_boot`'s alone; the
    /// mapping is admitted only after the release opened it, and it and the
    /// partition are held until `run_boot` returns, which is after
    /// `kexec_file_load` returned. Nothing in td-boot closes a mapping.
    #[test]
    fn the_installed_selector_releases_before_it_mounts_or_selects() {
        let body = production_body("boot_encrypted");
        let position = |needle: &str| {
            assert_eq!(body.matches(needle).count(), 1, "{needle}");
            body.find(needle).unwrap()
        };
        let pre_active = position("if let Some(mapping) = mapping {");
        let header = position("let header = td_protector::luks2::read(&mut partition.file());");
        let waited = position("selector_release::wait_for_tpm(&mut selector_release::tpm_present, &mut std::thread::sleep);");
        // The wait never depends on what the header holds.
        assert!(!body.contains("if wait") && !body.contains("tokens"));
        let device = position("let mut tpm = present.then_some(DeviceTpm);");
        let program = position(
            "let mut cryptsetup = Luks::new(Path::new(\"/bin\").join(protocol::TD_LUKS));",
        );
        // The recovery flow's console reader, td-init's applet by its /bin
        // name; the release ending in recovery runs it, and only then.
        let applet = position(
            "let mut line = selector_release::Applet(Path::new(\"/bin\").join(protocol::SECRET_LINE_APPLET));",
        );
        let keys = position("let mut keys = selector_release::SecurityKeys::root();");
        let released = position(
            "let (keyslot, key, admission, mut predicted) = match selector_release::unlock(",
        );
        assert!(position("&mut line,") > released);
        assert!(applet < keys && keys < released && position("&mut keys,") > released);
        assert_eq!(protocol::SECRET_LINE_APPLET, "secret-line");
        let halted = position("selector_release::Unlock::Halt(reason) => halt(&reason),");
        let failed = position("selector_release::Unlock::Failed(error) => return Err(error),");
        let admitted = position(
            "let mapping = volume::admit_mapping(&partition, uuid, selector_release::MAPPING_NAME)?;",
        );
        // The release's own admission, and nothing else, reaches the handoff.
        let boot = "run_boot(\n        &mapping.path(),\n        mountpoint,\n        cmdline,\n        \
                    Some(Handoff {\n            key: &key,\n            admission,\n        }),\n        \
                    Some(&mut commit),\n    )";
        let booted = position(boot);
        assert!(pre_active < header && header < waited);
        assert!(waited < device && device < program && program < applet && applet < released);
        assert!(released < halted && halted < failed && failed < admitted);
        assert!(admitted < booted);
        assert!(body.trim_end().ends_with(boot));
        // Beside it, the staged update's commit asks only whether the TPM
        // PIN released.
        assert_eq!(body.matches("admission").count(), 6);
        assert!(body.contains(
            "Some(staged) if admission == Some(admission::Method::TpmPin) => Some(staged),"
        ));
        for later in [
            "mount_command(",
            "select_boot_deployment(",
            "read_only_current(",
            "verify_deployment(",
            "measurement::",
            "kexec",
            "drop(",
            "close",
        ] {
            assert!(!body.contains(later), "boot_encrypted names {later}");
        }
        let unlock = include_str!("selector_release.rs");
        let (production, _) = unlock.split_once("\n#[cfg(test)]").unwrap();
        for verb in ["close_args", "\"close\"", "remove_args", "kill_slot"] {
            assert!(
                !production.contains(verb),
                "selector_release.rs names {verb}"
            );
            let main = include_str!("main.rs");
            let (main, _) = main.split_once("\n#[cfg(test)]\nmod tests {").unwrap();
            assert!(!main.contains(verb), "main.rs names {verb}");
        }
        // The selection run_boot makes and its PCR 11 measurement are the
        // unencrypted boot's, over the mapping.
        assert!(production_body("run_boot").contains("select_boot_deployment(root)"));
    }

    /// An unencrypted volume's boot is today's: `run_boot` contacts no TPM
    /// but its configured PCR 11 measurement, caps nothing and runs no
    /// cryptsetup, and dispatch hands it no key.
    #[test]
    fn an_unencrypted_boot_makes_no_tpm_contact_and_no_cap() {
        let (signature, body) = production_body("run_boot")
            .split_once(") -> io::Result<()> {")
            .unwrap();
        assert!(signature.ends_with(
            "handoff: Option<Handoff<'_>>,\n    mut commit: Option<&mut dyn FnMut(&Path)>,\n"
        ));
        for absent in [
            "selector_release::",
            "cap::",
            "DeviceTpm",
            "Luks",
            "td_protector",
            "td_tpm",
            "luks2",
            "SECRET_LINE",
            "secret-line",
            "Applet",
        ] {
            assert!(!body.contains(absent), "run_boot names {absent}");
        }
        // Only the encrypted selector boot reaches the recovery flow's
        // console reader.
        let source = include_str!("main.rs");
        let (production, _) = source.split_once("\n#[cfg(test)]\nmod tests {").unwrap();
        assert_eq!(production.matches("SECRET_LINE_APPLET").count(), 1);
        assert!(production_body("boot_encrypted").contains("SECRET_LINE_APPLET"));
        assert!(production_body("dispatch")
            .contains("} => run_boot(&device, &mountpoint, &cmdline, None, None),"));
        let decision = production_body("kexec_boot_decision");
        assert!(decision.contains("let key_pipe = handoff"));
        assert!(decision
            .contains("(None, _) => kexec_command(kernel, initramfs, cmdline.as_os_str()),"));
    }

    /// The handoff's command is td-kexec's `--fds-key` with the manifest's
    /// initramfs digest, and its pipe holds exactly the key then end of
    /// file, read as td-kexec reads it: opened anew by name without
    /// blocking.
    #[test]
    fn the_key_handoff_is_td_kexecs_fds_key_over_a_closed_pipe() {
        let fixture = Fixture::new();
        let id = fixture.valid_deployment();
        fixture.selector("current", &id);
        let deployment = select_deployment(&fixture.root).unwrap().deployment;
        let (_, manifest) = verified_manifest(&fixture.root, &id).unwrap();
        assert_eq!(deployment.initramfs_digest, manifest.initramfs);
        assert!(valid_digest(deployment.initramfs_digest.as_bytes()));

        let key = [0x3c; protocol::VOLUME_KEY_BYTES];
        let pipe = KeyFile::new(&key).unwrap();
        let Deployment {
            kernel,
            initramfs,
            initramfs_digest,
            ..
        } = deployment;
        let command = key_kexec_command(
            kernel,
            initramfs,
            &initramfs_digest,
            pipe.path(),
            None,
            OsStr::new("quiet td.deployment=test"),
        );
        assert_eq!(command.get_program(), OsStr::new(TD_KEXEC));
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            vec![
                OsStr::new("--fds-key"),
                OsStr::new(&initramfs_digest),
                pipe.path().as_os_str(),
                OsStr::new("quiet td.deployment=test"),
            ]
        );
        let name = pipe.path().to_str().unwrap();
        let prefix = format!("/proc/{}/fd/", std::process::id());
        assert!(name.strip_prefix(&prefix).unwrap().parse::<u32>().is_ok());

        let mut reader = OpenOptions::new()
            .read(true)
            .custom_flags(0o4000)
            .open(pipe.path())
            .unwrap();
        assert!(reader.metadata().unwrap().file_type().is_fifo());
        let mut read = [0u8; protocol::VOLUME_KEY_BYTES];
        io::Read::read_exact(&mut reader, &mut read).unwrap();
        assert_eq!(read, key);
        assert_eq!(io::Read::read(&mut reader, &mut [0u8; 1]).unwrap(), 0);
    }
}

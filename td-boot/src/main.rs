//! td-boot selects a deployment on the persistent volume, preferring `current`
//! and falling back to `previous`, then invokes the confined kexec helper.
//! Hashes detect corruption and cannot answer who produced a deployment, so
//! the BOOT decision also requires an ed25519 signature over the manifest,
//! under a trust root read from the rootfs td-boot is running in. `install`
//! and `verify` run where there is no such rootfs and check hashes alone —
//! `td-install/DESIGN.md` §6 is why.
#![forbid(unsafe_code)]

mod protocol;
// The real-regular-bounded file rule, shared with `td-install` for
// `protocol.rs`'s reason — and with the same redundant `#[path]` the fixture
// carries below, so the staging guard sees it. Unlike the fixture this one IS
// staged: it is in the target binary.
#[path = "realfile.rs"]
mod realfile;
// The committed fixture deployment, in its own file because the `td-install`
// recipe check stages the SAME one and the signatures are over the manifest
// its payloads hash to. `cfg(test)` keeps it out of the target binary, which
// is why td-boot's recipe does not stage it.
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
#[path = "../../engine/src/sha256.rs"]
#[allow(dead_code)]
mod sha256;
// The authenticity half, which hashes cannot give: ed25519 VERIFICATION.
// Declared as a PAIR at the crate root because `ed25519.rs` reaches its hash as
// `crate::sha512` — the spelling that resolves identically inside the engine
// lib, inside td-net's test build, and here.
//
// `ed25519_sign.rs` is deliberately absent and must stay so: this binary
// verifies and never signs, since a signer here would be a crypto surface
// serving no boot-time purpose. That is not left to a comment —
// `builder/src/affected.rs` refuses any file under `td-boot/src` that names it.
#[path = "../../engine/src/sha512.rs"]
#[allow(dead_code)]
mod sha512;
#[path = "../../engine/src/ed25519.rs"]
#[allow(dead_code)]
mod ed25519;

use std::ffi::{OsStr, OsString};
use std::fs::{self, File, Metadata, OpenOptions, TryLockError};
use std::io::{self, Seek, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{
    DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt, symlink,
};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

// td-boot reaches no third-party program at all now. `losetup` was the last
// one: the ability to BOOT rested on busybox existing at an absolute path and
// parsing `-r <device> <file>` as expected, with nothing tying the two
// together, so dropping that applet from the image would have stopped every
// boot at the root loop with no build-time complaint. It is a td-init applet
// beside mount/umount, under one pinned `LOOP_SET_FD` request.
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
        /// `None` means "look where a td rootfs keeps one" — which on a booted
        /// deployment finds nothing (§6), so an updater on a running machine
        /// must say WHICH key. See `install_trust_root`.
        trusted_key: Option<PathBuf>,
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
    Publish {
        root: PathBuf,
        source: PathBuf,
        /// `None` means "look where a td rootfs keeps one"; see
        /// `install_trust_root` for why absence and unreadability differ.
        trusted_key: Option<PathBuf>,
    },
    /// The unattended entry point: take whatever the channel is offering, if
    /// anything, under a trust root the caller must name.
    Update {
        device: PathBuf,
        mountpoint: PathBuf,
        channel: PathBuf,
        /// REQUIRED, unlike `install`'s. An updater nobody is watching has no
        /// business deciding that a missing key means "publish anyway", and
        /// there is no configuration in which it should.
        trusted_key: PathBuf,
    },
    Rollback {
        device: PathBuf,
        mountpoint: PathBuf,
    },
    Success {
        device: PathBuf,
        mountpoint: PathBuf,
        deployment_id: String,
    },
    /// Check a staged deployment's detached signature against a trusted key,
    /// and report ONLY that. Deliberately separate from `verify`, which is
    /// about integrity and selection: this verb decides authenticity and
    /// nothing else, so a caller — an operator, or the system oracle checking
    /// that what it signed is what a machine would accept — gets one answer to
    /// one question. When the boot path itself refuses unsigned deployments
    /// this becomes the same check reached a second way, which is why it takes
    /// a directory rather than a volume root and slot.
    Authenticate {
        directory: PathBuf,
        trusted_key: PathBuf,
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
    // Detached and OPTIONAL here, because nothing verifies it yet. What this
    // landing owes it is only that publishing carries it: `publish_bundle` used
    // to copy four literal names, so a signature beside a manifest was dropped
    // without a word and no machine could ever have one.
    signature: Option<Vec<u8>>,
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
        "usage: td-boot verify <volume-root>\n       td-boot root-loop <volume-root> <deployment-id> <loop-device>\n       td-boot boot <device> <mountpoint> <cmdline>\n       td-boot install <device> <mountpoint> <deployment-directory> [trusted-key]\n       td-boot update <device> <mountpoint> <channel> <trusted-key>\n       td-boot publish <volume-root> <deployment-directory> [trusted-key]\n       td-boot rollback <device> <mountpoint>\n       td-boot success <device> <mountpoint> <deployment-id>\n       td-boot authenticate <deployment-directory> [trusted-key]",
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
        Some(mode) if mode == OsStr::new("authenticate") => {
            let directory = args.next().ok_or_else(usage_error)?;
            // The key argument is optional, and its default is the one place a
            // booted td-boot ever reads a trust root from: `TRUSTED_KEY_PATH`
            // in its own rootfs, which the harness writes into the selector
            // initramfs. Spelling it here rather than only in the harness is
            // what makes that constant a shared contract instead of one side's
            // preference — a disagreement would be a key placed where nothing
            // looks, silent on both sides.
            let trusted_key = args.next().map_or_else(
                || Path::new("/").join(protocol::TRUSTED_KEY_PATH),
                PathBuf::from,
            );
            if args.next().is_some() {
                return Err(usage_error());
            }
            Ok(Mode::Authenticate {
                directory: PathBuf::from(directory),
                trusted_key,
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
            // `publish`'s optional key, for `publish`'s reason: absence is a
            // real configuration rather than a mistake, and only
            // `install_trust_root` may decide what it means.
            let trusted_key = args.next().map(PathBuf::from);
            if args.next().is_some() {
                return Err(usage_error());
            }
            Ok(Mode::Install {
                device: PathBuf::from(device),
                mountpoint: PathBuf::from(mountpoint),
                source: PathBuf::from(source),
                trusted_key,
            })
        }
        Some(mode) if mode == OsStr::new(protocol::PUBLISH_VERB) => {
            let root = args.next().ok_or_else(usage_error)?;
            let source = args.next().ok_or_else(usage_error)?;
            // `Option`, not `authenticate`'s defaulted path: there the key is
            // the whole question, so a missing one is a failure; here its
            // absence is a real configuration — a rootfs with no trust root
            // publishes as it always has — and only `install_trust_root` may
            // decide that, from whether the file is there.
            let trusted_key = args.next().map(PathBuf::from);
            if args.next().is_some() {
                return Err(usage_error());
            }
            Ok(Mode::Publish {
                root: PathBuf::from(root),
                source: PathBuf::from(source),
                trusted_key,
            })
        }
        Some(mode) if mode == OsStr::new(protocol::UPDATE_VERB) => {
            let device = args.next().ok_or_else(usage_error)?;
            let mountpoint = args.next().ok_or_else(usage_error)?;
            let channel = args.next().ok_or_else(usage_error)?;
            let trusted_key = args.next().ok_or_else(usage_error)?;
            if args.next().is_some() {
                return Err(usage_error());
            }
            Ok(Mode::Update {
                device: PathBuf::from(device),
                mountpoint: PathBuf::from(mountpoint),
                channel: PathBuf::from(channel),
                trusted_key: PathBuf::from(trusted_key),
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

use realfile::{open_real_file, read_bounded_real_file};

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

/// `trust` is the key a PUBLISH runs under, and `Some` makes this fail-closed.
///
/// Where it is given, the manifest is authenticated between being READ and
/// being PARSED, which is the only point that satisfies both halves of DESIGN
/// §10 item 6. Before the parse, because a deployment nobody vouched for must
/// be refused without its contents ever being interpreted — the boot path's
/// order, and an install source is exactly as untrusted as a slot, item 10's
/// update channel being what fetches these. And over the bytes already in hand,
/// because re-reading the manifest to parse it would leave a window in which a
/// writer could serve a signed manifest to the check and a different one to the
/// parse.
///
/// `None` is every OTHER caller: `existing_bundle` and the staged readback ask
/// about deployments td itself already published, which is a question about
/// integrity rather than authenticity.
fn open_bundle(directory: &Path, trust: Option<&TrustRoot>) -> io::Result<VerifiedBundle> {
    require_absolute(directory, "deployment directory")?;
    require_real_directory(directory, "deployment directory")?;
    let manifest_path = directory.join(protocol::MANIFEST_NAME);
    let manifest =
        read_bounded_real_file(&manifest_path, "deployment manifest", protocol::MAX_MANIFEST_BYTES)?;
    let signature = read_optional_signature(directory)?;
    if let Some(key) = trust {
        // An UNSIGNED bundle is refused rather than treated as unverifiable,
        // or the check would be escapable by deleting `manifest.sig`.
        let bytes = signature.as_deref().ok_or_else(|| {
            invalid(
                "deployment carries no signature and this publish has a trust root".to_string(),
            )
        })?;
        authenticate_manifest(&manifest, bytes, key)?;
    }
    let parsed = parse_manifest(&manifest)?;
    let id = sha256::hex_digest(&manifest);
    let kernel = verify_payload(directory, "bzImage", &parsed.kernel)?;
    let initramfs = verify_payload(directory, "initramfs.cpio", &parsed.initramfs)?;
    let root = verify_payload(directory, "root.erofs", &parsed.root)?;
    Ok(VerifiedBundle {
        id,
        manifest,
        signature,
        kernel,
        initramfs,
        root,
    })
}

/// The detached signature beside the manifest, if the bundle carries one.
///
/// Absent is `None` rather than an error: nothing verifies it yet, so requiring
/// it would make every existing bundle uninstallable for no gain. Every OTHER
/// failure is still an error — a signature that exists but is a symlink, a
/// directory, or too large is a bundle to refuse rather than one to treat as
/// unsigned, since silently downgrading to "no signature" is exactly the
/// fail-open the verifying half must not inherit.
fn read_optional_signature(directory: &Path) -> io::Result<Option<Vec<u8>>> {
    let path = directory.join(protocol::MANIFEST_SIG_NAME);
    match fs::symlink_metadata(&path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(io::Error::new(
            error.kind(),
            format!("deployment signature {}: {error}", path.display()),
        )),
        Ok(_) => read_bounded_real_file(
            &path,
            "deployment signature",
            protocol::MAX_SIGNATURE_BYTES,
        )
        .map(Some),
    }
}

/// Decode exactly `N` bytes of hex, tolerating surrounding ASCII whitespace —
/// `td-deploy` writes a trailing newline after both the signature and the key,
/// and `tests/td-subst.pub` established that shape for every committed public
/// half.
///
/// Over BYTES rather than `&str`, and pairs taken by pattern rather than by
/// slicing, because this parses files an attacker supplies: `manifest.sig`
/// comes off a volume anyone who can write the disk can write. The same decoder
/// written over `&str` with `&s[i..i + 2]` panicked on a multi-byte character
/// in exactly this position — that was a live remote-reachable panic in
/// `net/src/sig.rs`, reached from a fetched narinfo's `Sig:` field BEFORE
/// anything was verified, and it is not going to be reintroduced here.
fn decode_hex<const N: usize>(text: &[u8], label: &str) -> io::Result<[u8; N]> {
    let trimmed = text.trim_ascii();
    // `checked_mul`, not `saturating_mul`: saturation would leave the expected
    // length at `usize::MAX`, which is ODD, and an odd expected length is one
    // `chunks_exact` below would silently drop a byte from. Unreachable for the
    // two callers (N is 32 and 64), and refused rather than reasoned about.
    let Some(expected) = N.checked_mul(2) else {
        return Err(invalid(format!("{label}: absurd expected length")));
    };
    if trimmed.len() != expected {
        return Err(invalid(format!(
            "{label} must be exactly {expected} hexadecimal characters, got {}",
            trimmed.len()
        )));
    }
    let nibble = |c: u8| -> io::Result<u8> {
        match c {
            b'0'..=b'9' => Ok(c - b'0'),
            b'a'..=b'f' => Ok(c - b'a' + 10),
            b'A'..=b'F' => Ok(c - b'A' + 10),
            _ => Err(invalid(format!("{label} is not hexadecimal"))),
        }
    };
    // `as_chunks` so each pair arrives as `[u8; 2]` and destructures without a
    // fallible branch. `td-netd/src/ip_list` avoids it on the grounds that the
    // pinned bootstrap toolchain may not have it — that caution does not apply
    // here and is checkable rather than arguable: `engine/src/sha256.rs` calls
    // `as_chunks::<64>()` and has been staged by THIS recipe since the boot shim
    // first landed, and `ed25519.rs` — staged beside it — calls it six times,
    // once inside `verify` itself. A toolchain that could not compile it could
    // not build td-boot at all.
    //
    // The remainder is provably empty after the length check above and is
    // checked anyway, since "provably" is doing the work of a bounds check.
    let (pairs, rest) = trimmed.as_chunks::<2>();
    if !rest.is_empty() {
        return Err(invalid(format!("{label} is not hexadecimal")));
    }
    let mut out = [0u8; N];
    for (slot, [high, low]) in out.iter_mut().zip(pairs) {
        *slot = (nibble(*high)? << 4) | nibble(*low)?;
    }
    Ok(out)
}

/// The trusted deployment key, read from a file holding its 32 bytes as hex.
///
/// Read at RUNTIME rather than compiled in, which DESIGN.md §6 argues at
/// length and which is forced rather than chosen: a recipe embeds its sources
/// with `include_str!` resolved when td-recipe-eval itself compiles, and
/// recipes are content-addressed with no per-run parameter — so a build-time
/// pin could only carry a key that existed before the build, and since no
/// private key is committed, nothing at test time would hold the half needed to
/// sign for it. `tests/td-subst.pub` is the same shape for the same reason.
fn read_trusted_key(path: &Path) -> io::Result<[u8; ed25519::PUBLIC_KEY_LEN]> {
    let text = read_bounded_real_file(
        path,
        "trusted deployment key",
        protocol::MAX_PUBLIC_KEY_BYTES,
    )?;
    decode_hex(&text, "trusted deployment key")
}

/// Does `signature` — the bytes of `manifest.sig`, hex — authenticate
/// `manifest` under `key`?
///
/// `Ok(())` is the ONLY way this reports authenticity, and every other outcome
/// is an `Err` naming which one it was. That shape is deliberate: D2 is
/// fail-closed, so a caller must not be able to reach a "not really a failure"
/// branch, and an error is what `?` refuses on. The reasons stay distinct
/// because an operator reading a refused boot needs to tell a truncated file
/// from a wrong key.
fn authenticate_manifest(
    manifest: &[u8],
    signature: &[u8],
    key: &[u8; ed25519::PUBLIC_KEY_LEN],
) -> io::Result<()> {
    let signature = decode_hex::<{ ed25519::SIGNATURE_LEN }>(signature, "deployment signature")?;
    if ed25519::verify(key, manifest, &signature) {
        return Ok(());
    }
    // Phrased so it does not blame the signature, because at this point td-boot
    // CANNOT tell which side is at fault: `verify` folds "wrong signature",
    // "signature over other bytes" and "trusted key is well-formed hex but not a
    // valid curve point" into one `false`. Naming the signature would send an
    // operator to the wrong file. Distinguishing them properly needs a key check
    // that duplicates `verify`'s own subgroup policy, which is a worse trade
    // than a precise sentence.
    Err(invalid(format!(
        "{}: the signature, the manifest bytes and the trusted key do not agree",
        protocol::MANIFEST_UNAUTHENTICATED
    )))
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

/// What an already-published deployment at the destination is, relative to the
/// one being installed.
///
/// The id alone used to answer this, and that made key rotation unreachable:
/// re-signing deliberately does NOT change the id (D3), so a bundle carrying a
/// fresh signature looked identical to the installed one and the publish
/// early-returned without writing it. The id is still what says the PAYLOADS
/// agree — it is the hash of the manifest that names their digests — so the
/// signature is the only thing left to compare.
enum Existing {
    /// Same id, same signature: publishing is a no-op, as it always was.
    Same,
    /// Same id, different signature. The payloads are already verified
    /// identical, so only the signature file needs replacing.
    Resigned(Vec<u8>),
    /// Same id, but the source dropped a signature the destination has.
    /// Refused rather than performed: removing a signature is a downgrade, and
    /// doing nothing would silently ignore what the caller asked for.
    SignatureWithdrawn,
    /// A different deployment entirely.
    Different,
}

fn existing_bundle(directory: &Path, want: &VerifiedBundle) -> io::Result<Existing> {
    let found = open_bundle(directory, None)?;
    if found.id != want.id {
        return Ok(Existing::Different);
    }
    // Spelled exhaustively rather than with a `_` arm: this classifies for a
    // fail-CLOSED caller, and a catch-all would send any future fifth state to
    // the no-op by default instead of to a refusal.
    Ok(match (&want.signature, found.signature) {
        (Some(new), Some(old)) if *new == old => Existing::Same,
        (Some(new), Some(_)) => Existing::Resigned(new.clone()),
        (Some(new), None) => Existing::Resigned(new.clone()),
        (None, Some(_)) => Existing::SignatureWithdrawn,
        (None, None) => Existing::Same,
    })
}

/// Replace a published deployment's detached signature in place.
///
/// Sound only because the caller has established the ids match, which means the
/// manifest bytes match, which means the payload digests it names — and so the
/// payloads `open_bundle` just verified — are the same file contents. Nothing
/// about the deployment changes but which key vouches for it.
/// Stage the new signature under a name no other writer can be using.
///
/// A FIXED temporary name was wrong twice over, because `write_synced_file`
/// creates with `create_new`. A write that failed partway — or a crash between
/// the create and the rename — left the file behind, and since nothing reaps
/// inside a deployment directory (`reap_install_temporaries` sweeps `.install-`
/// entries in the deployments directory only), every later rotation then failed
/// `AlreadyExists`: signing was wedged permanently by one interrupted write.
/// And two publishers rotating the same deployment at once collided on it, so
/// one failed for a reason that has nothing to do with what it was asked to do.
/// The pid-and-attempt shape is the one `create_install_directory` already uses.
fn write_signature_temporary(
    directory: &Path,
    signature: &[u8],
) -> io::Result<(PathBuf, RemoveFile)> {
    for attempt in 0..1024u32 {
        let path = directory.join(format!(
            ".{}-{}-{attempt}",
            protocol::MANIFEST_SIG_NAME,
            std::process::id()
        ));
        // Armed BEFORE the write, so a write that fails after `create_new`
        // succeeded removes what it made rather than leaving the wedge above.
        let mut cleanup = RemoveFile {
            path: Some(path.clone()),
        };
        match write_synced_file(&path, signature) {
            Ok(()) => return Ok((path, cleanup)),
            // Someone else's file — a dead process with this pid, or a thread
            // racing us. Take the next name, and do NOT delete what is not ours.
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => cleanup.path = None,
            Err(error) => {
                return Err(io::Error::new(
                    error.kind(),
                    format!("stage deployment signature {}: {error}", path.display()),
                ));
            }
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!(
            "could not create a unique signature temporary below {}",
            directory.display()
        ),
    ))
}

fn replace_signature(directory: &Path, signature: &[u8]) -> io::Result<()> {
    let destination = directory.join(protocol::MANIFEST_SIG_NAME);
    // A crash must leave the old signature or the new one, never a half-written
    // file: a truncated signature verifies as nothing and would strand a
    // machine on a deployment it can no longer authenticate.
    let (staging, mut cleanup) = write_signature_temporary(directory, signature)?;
    match fs::rename(&staging, &destination) {
        Ok(()) => {
            cleanup.path = None;
            sync_directory(directory)
        }
        Err(error) => Err(io::Error::new(
            error.kind(),
            format!(
                "publish signature {} -> {}: {error}",
                staging.display(),
                destination.display()
            ),
        )),
    }
}

/// Which boot slots currently select this deployment, in slot order.
///
/// A slot that cannot be read is not one that selects it: a missing `previous`
/// is the ordinary state of a freshly installed volume, not a reason to fail an
/// install.
fn slots_pointing_at(root: &Path, id: &str) -> Vec<&'static str> {
    let mut live = Vec::new();
    for slot in ["current", "previous"] {
        if read_selector(root, slot).is_ok_and(|selected| selected == id) {
            live.push(slot);
        }
    }
    live
}

/// Say so when a re-sign replaced the signature of a slot that boots.
///
/// D3 wants re-signing a LIVE deployment to work — that is the whole point of
/// the signature being detached and outside the id, so a key rotation reaches a
/// machine that already has the deployment. What the boot flip added is that
/// the replacement decides whether that deployment still boots, and `install`
/// cannot check it: it runs on the real root, which per §6 has no trust root.
/// So an operator who re-signs with the wrong key gets a machine that stops
/// booting, and if `current` and `previous` are the same deployment there is
/// nothing to fall back to.
///
/// A warning rather than a refusal because refusing is D3's feature, and rather
/// than silence because the next report is a machine that does not come back.
///
/// The real answer was for the installing rootfs to have a trust root, and it
/// now can: where `install_trust_root` finds one, the re-signed manifest is
/// authenticated before anything is written and a wrong key is a refusal
/// instead of this. So this path is what remains when there is NO trust root —
/// the real root §6 describes — rather than the only answer there is.
fn warn_unverifiable_resign(root: &Path, id: &str) -> io::Result<()> {
    let live = slots_pointing_at(root, id);
    if live.is_empty() {
        return Ok(());
    }
    writeln!(
        io::stderr(),
        "td-boot: warning: replaced the signature of deployment {id}, which {} \
         point{} at; nothing here can check it — the next boot will, and will \
         refuse the deployment if it does not verify",
        live.join(" and "),
        if live.len() == 1 { "s" } else { "" }
    )
}

fn publish_bundle(
    root: &Path,
    mut bundle: VerifiedBundle,
    authenticated: bool,
) -> io::Result<String> {
    let deployments = root.join(protocol::DEPLOYMENTS_DIR);
    require_real_directory(&deployments, "deployments directory")?;
    let destination = deployments.join(&bundle.id);
    match fs::symlink_metadata(&destination) {
        Ok(_) => {
            match existing_bundle(&destination, &bundle)? {
                Existing::Same => {}
                Existing::Resigned(signature) => {
                    replace_signature(&destination, &signature)?;
                    // Only where nothing checked it. Under a trust root the
                    // replacement was authenticated before anything was
                    // written, so the warning would be false — and a false one
                    // here is worse than none, since its whole job is to make
                    // an operator look at a machine that may not come back.
                    //
                    // `let _`, not `?`: the signature is already replaced, so
                    // failing on a diagnostic write would report a publish that
                    // in fact happened, and the caller would retry a thing that
                    // is done.
                    if !authenticated {
                        let _ = warn_unverifiable_resign(root, &bundle.id);
                    }
                }
                Existing::SignatureWithdrawn => {
                    return Err(invalid(format!(
                        "deployment {} is already published WITH a signature and the source \
                         carries none; refusing to remove it",
                        bundle.id
                    )));
                }
                Existing::Different => {
                    return Err(invalid(format!(
                        "existing deployment {} does not verify as {}",
                        destination.display(),
                        bundle.id
                    )));
                }
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
    if let Some(signature) = &bundle.signature {
        write_synced_file(&staging.join(protocol::MANIFEST_SIG_NAME), signature)?;
    }
    sync_directory(&staging)?;

    let staged = open_bundle(&staging, None)?;
    if staged.id != bundle.id {
        return Err(invalid(format!(
            "staged deployment id {} changed from verified source {}",
            staged.id, bundle.id
        )));
    }
    // The id is the manifest's hash, so it says nothing about the DETACHED
    // signature: a staging directory missing it reads as a perfectly good
    // deployment. Read it back for the reason the rest of this tree does —
    // nothing observable distinguishes a signature that was never written from
    // one the source never had.
    if staged.signature != bundle.signature {
        return Err(invalid(format!(
            "staged deployment {} did not carry the source's signature",
            bundle.id
        )));
    }

    match fs::rename(&staging, &destination) {
        Ok(()) => cleanup.path = None,
        Err(rename_error) => {
            // A concurrent publisher won the rename. Its bundle is the same
            // deployment if the id agrees; a signature difference is still
            // ours to apply, since the two publishers may hold different keys
            // and the later one is the one the caller asked for.
            match existing_bundle(&destination, &bundle) {
                Ok(Existing::Same) => {
                    sync_directory(&deployments)?;
                    return Ok(bundle.id);
                }
                Ok(Existing::Resigned(signature)) => {
                    // Compose both, as the sibling arm below does: on its own
                    // a signature error here reads as a rotation failure and
                    // says nothing about the rename that led to it.
                    replace_signature(&destination, &signature).map_err(|signature_error| {
                        io::Error::new(
                            signature_error.kind(),
                            format!(
                                "publish deployment {} -> {}: {rename_error}; concurrent \
                                 destination re-signing failed: {signature_error}",
                                staging.display(),
                                destination.display()
                            ),
                        )
                    })?;
                    sync_directory(&deployments)?;
                    return Ok(bundle.id);
                }
                Ok(Existing::SignatureWithdrawn) => {
                    return Err(invalid(format!(
                        "deployment {} was published concurrently WITH a signature and this \
                         source carries none; refusing to remove it",
                        bundle.id
                    )));
                }
                Ok(Existing::Different) => {}
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

/// The trust root a PUBLISH runs under, which is not the boot path's.
///
/// §6 gives the booted selector's rootfs the only trust root a running machine
/// has, and the real root none — so `install` could not check what it wrote and
/// said so (`warn_unverifiable_resign`). An INSTALLER is the case that
/// distinction was waiting for: it runs from media td built, which can carry a
/// key exactly as the selector initramfs does.
///
/// Three outcomes, and which one a caller gets is never a matter of how the
/// read went. An explicit path is fail-closed — asking for a key and getting an
/// unreadable one is a refusal, not a fallback. With no argument the key is
/// looked for where a td rootfs keeps one, and its ABSENCE is the only thing
/// that means "no trust root": present-but-unusable refuses, because a
/// truncated or mistyped key is a provisioning fault and publishing anyway
/// would make it invisible until the machine failed to boot.
/// `rootfs` is a parameter for `read_boot_trust_root`'s reason: the branch that
/// takes no explicit key is the one PRODUCTION uses, and hardcoding `/` would
/// leave it reachable only by writing to the host's own root. What production
/// passes is pinned as a literal by its own test instead.
fn install_trust_root(explicit: Option<&Path>, rootfs: &Path) -> io::Result<Option<TrustRoot>> {
    if let Some(path) = explicit {
        return read_trusted_key(path).map(Some);
    }
    // `TRUSTED_KEY_PATH` must stay RELATIVE for this join to mean anything —
    // an absolute one would discard `rootfs` silently, which is the shape of
    // the worst single edit DESIGN §10 item 6 names. It is pinned as a literal
    // by `authenticate_parses_its_arguments_exactly`.
    let path = rootfs.join(protocol::TRUSTED_KEY_PATH);
    match fs::symlink_metadata(&path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        // Wrapped with the path, as every other read in this file is: this one
        // REFUSES an install, and `td-boot: Permission denied` names no file.
        Err(error) => Err(io::Error::new(
            error.kind(),
            format!("trust root {}: {error}", path.display()),
        )),
        Ok(_) => read_trusted_key(&path).map(Some),
    }
}

fn install_deployment(root: &Path, source: &Path, trust: Option<&TrustRoot>) -> io::Result<String> {
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

    // Authenticity is decided INSIDE `open_bundle`, between the manifest being
    // read and its being parsed — see there. Passing the key down rather than
    // checking around the call is what satisfies both halves of DESIGN §10 item
    // 6 at once: refuse before anything interprets the bundle, and read the
    // manifest exactly ONCE.
    let bundle = open_bundle(source, trust)?;
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
    let installed = publish_bundle(root, bundle, trust.is_some())?;
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

fn rollback_deployment(root: &Path) -> io::Result<String> {
    require_absolute(root, "volume root")?;
    require_real_directory(root, "volume root")?;
    require_real_directory(&root.join(protocol::BOOT_DIR), "boot selector directory")?;
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

/// A deployment's manifest bytes, read ONCE and bound to its id by hash.
///
/// Returned as bytes rather than parsed because the authenticated path needs
/// exactly these bytes to check a signature over, and a second read to get
/// them would be a window: check the signature against one revision, take the
/// payload digests from another. There is one read here, and both callers work
/// from what it returned.
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

/// The rootfs a booting td-boot reads its trust root out of: the selector
/// initramfs it is itself running in. Named rather than spelled at the call
/// site so a test can state what production passes; the parameter exists so
/// the tests can point the read somewhere writable, and has exactly one
/// non-test caller.
const BOOT_ROOTFS: &str = "/";

/// The machine's trust root, read once per `boot` and threaded from there.
///
/// A distinct name because it is not interchangeable with the key argument the
/// `authenticate` verb takes from its caller: this one comes from the rootfs
/// td-boot is running in and nowhere else.
type TrustRoot = [u8; ed25519::PUBLIC_KEY_LEN];

/// Read the trusted key from this rootfs, where the harness put it.
///
/// A machine with no readable trust root cannot authenticate ANY slot, so this
/// is a hard failure of the whole selection rather than something to fall back
/// from — falling back to `previous` would just fail the same way, one slot
/// later and with a message about the wrong thing. It is also what makes a
/// mis-provisioned selector loud: per DESIGN.md §6 the kernel says nothing at
/// all about an appendix it could not parse, so this refusal is the only
/// report that the key never arrived.
fn read_boot_trust_root(rootfs: &Path) -> io::Result<TrustRoot> {
    let path = rootfs.join(protocol::TRUSTED_KEY_PATH);
    read_trusted_key(&path).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "no usable trust root at {}: {error} — this initramfs was built \
                 without one, or its key archive did not land",
                path.display()
            ),
        )
    })
}

/// Authenticate a deployment's manifest under the machine's trust root.
///
/// Reached from the BOOT DECISION and nowhere downstream of it, which §6 of
/// `td-install/DESIGN.md` forces rather than prefers: only the selector's
/// rootfs holds the key. `verify_slot` below is the unauthenticated question
/// `install` and `verify` ask from the real root, where there is none.
///
/// Returns the manifest rather than a verdict so the signature is checked over
/// the same bytes the payload digests are parsed from. A second read to parse
/// would be a window: signed manifest to the check, another to the parse.
fn authenticated_manifest(
    root: &Path,
    id: &str,
    key: &TrustRoot,
) -> io::Result<(PathBuf, Manifest)> {
    let (directory, bytes) = manifest_bytes(root, id)?;
    let signature = read_optional_signature(&directory)?.ok_or_else(|| {
        invalid(format!(
            "deployment {id} carries no {} — an unsigned deployment is refused, \
             not trusted",
            protocol::MANIFEST_SIG_NAME
        ))
    })?;
    // Before `parse_manifest`, so a deployment nobody vouched for is refused
    // without its contents ever being interpreted.
    authenticate_manifest(&bytes, &signature, key)?;
    let manifest = parse_manifest(&bytes)?;
    Ok((directory, manifest))
}

fn verify_slot(root: &Path, slot: &str) -> io::Result<Deployment> {
    let id = read_selector(root, slot)?;
    verify_deployment(root, &id)
}

/// `verify_slot` for the boot decision: authentic FIRST, then whole.
///
/// The order is the point. A slot is refused for being unsigned before any of
/// its payload digests are read, so the hashes that decide what to kexec are
/// only ever hashes out of bytes the trust root vouched for.
fn authenticated_slot(root: &Path, slot: &str, key: &TrustRoot) -> io::Result<Deployment> {
    let id = read_selector(root, slot)?;
    authenticated_deployment(root, id.as_str(), key)
}

/// `verify_deployment` for the boot decision, by id rather than by slot.
///
/// `run_boot` re-verifies the chosen deployment under the mount whose handles
/// are passed to kexec, and does it through this rather than through
/// `verify_deployment`. The id it re-verifies came from an authenticated
/// selection, so the hashes alone would already bind it — the manifest must
/// hash to that id, and the payloads to that manifest. Using this instead
/// makes "everything handed to kexec was authenticated" a property of the call
/// rather than of a trace back to where the id came from, which is the half a
/// later edit can break silently.
fn authenticated_deployment(root: &Path, id: &str, key: &TrustRoot) -> io::Result<Deployment> {
    let (directory, manifest) = authenticated_manifest(root, id, key)?;
    verify_deployment_payloads(&directory, id, &manifest)
}

fn verify_deployment(root: &Path, id: &str) -> io::Result<Deployment> {
    let (directory, manifest) = verified_manifest(root, id)?;
    verify_deployment_payloads(&directory, id, &manifest)
}

/// The payload half, given a manifest a caller has already established.
///
/// Split so the authenticated path can hand over the manifest it checked a
/// signature against rather than causing it to be read again — see
/// `authenticated_manifest`.
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
///
/// Split from its callers because the two read that slot differently and must:
/// `boot` authenticates (it is choosing what to run), `verify` cannot, because
/// it runs on the operator's real root where §6's trust root does not exist.
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
    key: &TrustRoot,
) -> io::Result<BootDecision> {
    let previous = authenticated_slot(root, "previous", key)?;
    previous_decision(root, previous, current_error, exhausted_deployment)
}

fn select_boot_deployment(root: &Path, key: &TrustRoot) -> io::Result<BootDecision> {
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
    let attempts = attempts_directory(root, false).map_err(|error| {
        io::Error::other(format!("boot attempt bookkeeping rejected: {error}"))
    })?;
    if let Some(attempts) = attempts {
        reap_attempt_temporaries(&attempts)?;
    }

    let current = match authenticated_slot(root, "current", key) {
        Ok(current) => current,
        Err(error) => {
            let decision = verified_previous_decision(root, Some(error.to_string()), None, key)
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
            match verified_previous_decision(root, None, Some(exhausted.clone()), key) {
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
        Err(error) => {
            match verified_previous_decision(root, Some(error.to_string()), None, key) {
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
            }
        }
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

    let mut token = format!("td.deployment={deployment_id}");
    if bookkeeping_unavailable {
        token.push(' ');
        token.push_str(protocol::BOOKKEEPING_UNAVAILABLE_CMDLINE_TOKEN);
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

fn report_boot_decision(decision: &BootDecision) -> io::Result<()> {
    if let Some(error) = &decision.bookkeeping_error {
        writeln!(
            io::stderr(),
            "{}: {error}",
            protocol::BOOKKEEPING_UNAVAILABLE_MARKER
        )?;
    }
    if let Some(error) = &decision.current_error {
        writeln!(
            io::stderr(),
            "{}: current rejected ({error}); using previous {}",
            protocol::CURRENT_REJECTED_MARKER,
            decision.deployment_id
        )?;
    }
    if let Some(exhausted) = &decision.exhausted_deployment {
        if let Some(error) = &decision.fallback_error {
            writeln!(
                io::stderr(),
                "{} {exhausted}: previous unavailable ({error}); retrying {}",
                protocol::ATTEMPTS_EXHAUSTED_MARKER,
                decision.deployment_id
            )?;
        } else {
            writeln!(
                io::stderr(),
                "{} {exhausted} -> {}",
                protocol::ATTEMPTS_EXHAUSTED_MARKER,
                decision.deployment_id
            )?;
        }
    }
    if let Some(remaining) = decision.remaining_attempts {
        writeln!(
            io::stderr(),
            "{} {} remaining={remaining}",
            protocol::ATTEMPT_CONSUMED_MARKER,
            decision.deployment_id
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
    writeln!(io::stderr(), "{marker} {}", decision.deployment_id)
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
    report_fallback(&selection)?;
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
        selection.slot, selection.deployment.id
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
    prepare_update_mountpoint(mountpoint, device_id).map_err(WritableVolumeFailure::Transaction)?;
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

fn read_only_current(root: &Path, key: &TrustRoot) -> Option<(BootDecision, Deployment)> {
    let id = read_selector(root, "current").ok()?;
    if read_attempt_state(root, &id).ok()?.is_some() {
        return None;
    }
    let deployment = authenticated_deployment(root, &id, key).ok()?;
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
    key: &TrustRoot,
) -> io::Result<(BootDecision, Deployment)> {
    let bookkeeping_error = Some(transaction_error.to_string());
    let current = authenticated_slot(root, "current", key);
    let previous = authenticated_slot(root, "previous", key);
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

fn kexec_boot_decision(
    deployment: Deployment,
    decision: &BootDecision,
    base_cmdline: &OsStr,
) -> io::Result<()> {
    report_boot_decision(decision)?;
    let cmdline = kernel_cmdline(
        base_cmdline,
        &decision.deployment_id,
        decision.bookkeeping_error.is_some(),
    )?;
    let Deployment {
        kernel, initramfs, ..
    } = deployment;
    run_command(
        &mut kexec_command(kernel, initramfs, cmdline.as_os_str()),
        "td-kexec",
    )?;
    Err(io::Error::other(
        "td-kexec returned without booting the verified deployment",
    ))
}

fn run_boot(device: &Path, mountpoint: &Path, base_cmdline: &OsStr) -> io::Result<()> {
    require_absolute(device, "volume device")?;
    require_absolute(mountpoint, "mountpoint")?;
    // Read before the volume is touched: with no trust root nothing on it can
    // be chosen, so the failure should name the missing key rather than
    // whatever the first slot happens to be wrong about.
    let key = read_boot_trust_root(Path::new(BOOT_ROOTFS))?;
    let (_transaction_lock, _device_lock, device_id) = acquire_update_locks(device)?;
    prepare_update_mountpoint(mountpoint, device_id)?;
    run_command(
        &mut mount_command(device, mountpoint),
        "read-only Btrfs mount",
    )?;
    if let Some((decision, deployment)) = read_only_current(mountpoint, &key) {
        let result = kexec_boot_decision(deployment, &decision, base_cmdline);
        best_effort_unmount(mountpoint);
        return result;
    }
    if let Err(transaction_error) =
        run_command(&mut unmount_command(mountpoint), "read-only Btrfs unmount")
    {
        let result = (|| {
            let (decision, deployment) = read_only_recovery(mountpoint, &transaction_error, &key)?;
            kexec_boot_decision(deployment, &decision, base_cmdline)
        })();
        best_effort_unmount(mountpoint);
        return result;
    }

    let decision =
        match run_on_prelocked_writable_volume(device, mountpoint, device_id, |root| {
            select_boot_deployment(root, &key)
        }) {
            Ok(decision) => decision,
            Err(WritableVolumeFailure::Committed {
                value: decision,
                unmount_error,
            }) => {
                let result = (|| {
                    writeln!(
                        io::stderr(),
                        "td-boot: selection committed but Btrfs unmount failed \
                         ({unmount_error}); booting the committed deployment"
                    )?;
                    let deployment =
                        authenticated_deployment(mountpoint, &decision.deployment_id, &key)?;
                    kexec_boot_decision(deployment, &decision, base_cmdline)
                })();
                best_effort_unmount(mountpoint);
                return result;
            }
            Err(WritableVolumeFailure::Mounted(transaction_error)) => {
                let result = (|| {
                    let (decision, deployment) =
                        read_only_recovery(mountpoint, &transaction_error, &key)?;
                    kexec_boot_decision(deployment, &decision, base_cmdline)
                })();
                best_effort_unmount(mountpoint);
                return result;
            }
            Err(WritableVolumeFailure::Transaction(transaction_error)) => {
                // Update APIs fail closed on semantic errors; PID 1 still tries the
                // verified payload-only recovery path before giving up.
                best_effort_unmount(mountpoint);
                prepare_update_mountpoint(mountpoint, device_id)?;
                run_command(
                    &mut mount_command(device, mountpoint),
                    "read-only Btrfs recovery mount",
                )?;
                let result = (|| {
                    let (decision, deployment) =
                        read_only_recovery(mountpoint, &transaction_error, &key)?;
                    kexec_boot_decision(deployment, &decision, base_cmdline)
                })();
                best_effort_unmount(mountpoint);
                return result;
            }
        };

    prepare_update_mountpoint(mountpoint, device_id)?;
    run_command(
        &mut mount_command(device, mountpoint),
        "read-only Btrfs mount",
    )?;

    let result = (|| {
        // The writable transaction closed its payload handles before unmounting.
        // Reverify under the mount whose handles are passed to kexec.
        let deployment = authenticated_deployment(mountpoint, &decision.deployment_id, &key)?;
        kexec_boot_decision(deployment, &decision, base_cmdline)
    })();
    best_effort_unmount(mountpoint);
    result
}

/// Mount the volume and publish into it, under an optional named trust root.
///
/// The key is a PARAMETER because §6 leaves a booted deployment without one:
/// `install_trust_root`'s probe answers for the selector initramfs and finds
/// nothing on the real root, so before this an update running on a live machine
/// published whatever it was handed and only warned. The volume carries a trust
/// root now (item 10a), and this is the argument that lets it be used.
///
/// A key ON the volume being installed into is not circular, but only because
/// of an ordering that must not be reversed: the key is read while the volume
/// is still mounted READ-ONLY where the running system already has it, before
/// this verb mounts the same device writable. What is checked is therefore the
/// trust root as it stood before the transaction, which is the one the operator
/// provisioned — not one the bundle could have arranged to be there.
fn run_install(
    device: &Path,
    mountpoint: &Path,
    source: &Path,
    trusted_key: Option<&Path>,
) -> io::Result<()> {
    require_absolute(source, "deployment source")?;
    // Resolved BEFORE the mount, so a missing or unusable key fails without
    // having touched the volume: an install refused for want of a trust root
    // should leave the disk exactly as it found it. That ordering is what a
    // key ON the volume relies on — see the verb's own note below.
    let trust = install_trust_root(trusted_key, Path::new("/"))?;
    publish_onto_volume(device, mountpoint, source, trust.as_ref())
}

/// The half `install` and `update` share, after each has decided what it is
/// installing and under whose key.
fn publish_onto_volume(
    device: &Path,
    mountpoint: &Path,
    source: &Path,
    trust: Option<&TrustRoot>,
) -> io::Result<()> {
    let id = run_on_writable_volume(device, mountpoint, |root| {
        install_deployment(root, source, trust)
    })?;
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
/// The trust root is read BEFORE the candidate is looked for, which costs a read
/// on every idle run and is the point: a key that has gone missing or unreadable
/// is reported on the next timer tick rather than on the first tick that had an
/// update to install, which is the tick least able to afford it. Both paths are
/// required ABSOLUTE, since nothing sets a timer's working directory.
///
/// The candidate is refused unless it is a real directory. That rule holds
/// against a producer that got it wrong and NOT against one racing this call:
/// the path is looked up again under the mount, so a writer with the channel can
/// swap it in between. What bounds that is authentication rather than the
/// lookup — whatever is read still has to verify under the trust root — so it is
/// recorded in DESIGN as a residual instead of being closed with the directory
/// handles it would really need.
fn run_update(
    device: &Path,
    mountpoint: &Path,
    channel: &Path,
    trusted_key: &Path,
) -> io::Result<()> {
    match update_decision(channel, trusted_key)? {
        None => Ok(()),
        Some((candidate, trust)) => publish_authenticated(device, mountpoint, &candidate, &trust),
    }
}

/// Everything `update` DECIDES, split from what it does so the decision is
/// reachable without a block device.
///
/// The verb cannot be driven end to end in this crate: the transaction lock
/// refuses a character device before any mount, so `/dev/null` never gets far
/// enough to install anything. Review found that this left the whole publish
/// unpinned — replacing it with `Ok(())` was green, and so was publishing
/// UNAUTHENTICATED. What is testable is the decision, and the decision is
/// everything that makes this verb different from `install`.
///
/// `None` is "nothing to do" and means EXACTLY one thing: no candidate. A
/// channel that cannot be read, or one holding something that is not a real
/// directory, is loud. That boundary is the verb.
fn update_decision(channel: &Path, trusted_key: &Path) -> io::Result<Option<(PathBuf, TrustRoot)>> {
    require_absolute(channel, "update channel")?;
    require_absolute(trusted_key, "update trust root")?;
    require_real_directory(channel, "update channel")?;
    // Read before the candidate is looked for, so an idle tick still reports a
    // trust root that has gone missing. Unwrapped to a `TrustRoot`, not left an
    // `Option`: `install_trust_root` answers `None` only for an absent DEFAULT,
    // and this key is explicit, so `None` here is unreachable rather than a
    // configuration — and carrying it would put the fail-open back in the type.
    let trust = install_trust_root(Some(trusted_key), Path::new("/"))?.ok_or_else(|| {
        invalid(format!(
            "update trust root {}: an explicit key cannot be absent",
            trusted_key.display()
        ))
    })?;
    let candidate = channel.join(protocol::CHANNEL_CANDIDATE);
    // ONE lookup rather than an existence test and then a type test: two put the
    // quiet/loud boundary on different instants, so a candidate removed between
    // them reports "no such file" where this verb's rule says absent is quiet.
    match require_real_directory(&candidate, "update candidate") {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        other => other?,
    }
    Ok(Some((candidate, trust)))
}

/// `publish_onto_volume` for a caller that must NEVER publish unauthenticated.
///
/// A `&TrustRoot` rather than an `Option`, so `update`'s central safety property
/// is not left to a test to catch: the fail-open does not typecheck.
fn publish_authenticated(
    device: &Path,
    mountpoint: &Path,
    source: &Path,
    trust: &TrustRoot,
) -> io::Result<()> {
    publish_onto_volume(device, mountpoint, source, Some(trust))
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

fn run_publish(root: &Path, source: &Path, trusted_key: Option<&Path>) -> io::Result<()> {
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
    let trust = install_trust_root(trusted_key, Path::new("/"))?;
    let id = install_deployment(root, source, trust.as_ref())?;
    writeln!(io::stdout(), "{id}")
}

fn run_rollback(device: &Path, mountpoint: &Path) -> io::Result<()> {
    let id = run_on_writable_volume(device, mountpoint, rollback_deployment)?;
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
    prepare_update_mountpoint(mountpoint, device_id)?;
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

/// `td-boot authenticate <deployment-directory> [trusted-key]`, the key
/// defaulting to `/` + `protocol::TRUSTED_KEY_PATH`.
///
/// Prints the deployment id — `sha256(manifest)`, the same identity every other
/// verb uses — on success, and refuses with a reason otherwise. An ABSENT
/// signature is a refusal here, not a `None`: this verb's entire question is
/// authenticity, so "there is nothing to check" is the answer no. That differs
/// from `read_optional_signature`'s tolerance on the publish path, where an
/// unsigned bundle is still installable, and the difference is the point.
fn run_authenticate(directory: &Path, trusted_key: &Path) -> io::Result<()> {
    require_real_directory(directory, "deployment")?;
    let key = read_trusted_key(trusted_key)?;
    let manifest = read_bounded_real_file(
        &directory.join(protocol::MANIFEST_NAME),
        "deployment manifest",
        protocol::MAX_MANIFEST_BYTES,
    )?;
    let signature = read_optional_signature(directory)?
        .ok_or_else(|| invalid("deployment carries no signature to authenticate"))?;
    authenticate_manifest(&manifest, &signature, &key)?;
    // Parsed only AFTER the signature holds, so nothing about an unauthenticated
    // manifest's shape can be reported: the id is the one fact worth printing,
    // and it is a fact about bytes that were signed.
    parse_manifest(&manifest)?;
    writeln!(io::stdout(), "{}", sha256::hex_digest(&manifest))
}

fn run() -> io::Result<()> {
    dispatch(parse_args(std::env::args_os().skip(1))?)
}

/// The verb table, split from `run` so a test can reach it without argv.
///
/// Each arm is a hop nothing else pins: an arm that drops a field compiles, and
/// for `Install`'s trust root that is the fail-open this whole verb exists to
/// prevent. `a_named_trust_root_is_resolved_before_the_device_is_touched`
/// drives this function rather than `run_install` for exactly that reason.
fn dispatch(mode: Mode) -> io::Result<()> {
    match mode {
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
            trusted_key,
        } => run_install(&device, &mountpoint, &source, trusted_key.as_deref()),
        Mode::Publish {
            root,
            source,
            trusted_key,
        } => run_publish(&root, &source, trusted_key.as_deref()),
        Mode::Update {
            device,
            mountpoint,
            channel,
            trusted_key,
        } => run_update(&device, &mountpoint, &channel, &trusted_key),
        Mode::Rollback { device, mountpoint } => run_rollback(&device, &mountpoint),
        Mode::Success {
            device,
            mountpoint,
            deployment_id,
        } => run_success(&device, &mountpoint, &deployment_id),
        Mode::Authenticate {
            directory,
            trusted_key,
        } => run_authenticate(&directory, &trusted_key),
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

    use crate::fixture;

    /// The one public key every fixture deployment is signed under.
    ///
    /// A committed VECTOR because td-boot cannot sign: `ed25519_sign.rs` is
    /// deliberately not in this binary and `affected.rs` refuses any file under
    /// `td-boot/src` that names it. Only public halves and signatures are
    /// written down, the practice `td-boot/tests/README` records — whose own
    /// fixtures cannot serve here, since a deployment that BOOTS needs payloads
    /// that hash to what its manifest says.
    ///
    /// The regeneration procedure is `fixture.rs`'s, beside the vectors it
    /// regenerates.
    const FIXTURE_PUBLIC_KEY: &str = fixture::PUBLIC_KEY;

    /// A second, UNRELATED public key — see `fixture.rs` for why it is a real
    /// generated one rather than this key with a bit flipped.
    const FIXTURE_OTHER_PUBLIC_KEY: &str = fixture::OTHER_PUBLIC_KEY;

    /// Keyed by `source_bundle`'s tag; `""` is `valid_deployment`'s manifest.
    const FIXTURE_SIGNATURES: [(&str, &str); 8] = fixture::SIGNATURES;

    /// The three payloads a fixture deployment carries, keyed by tag.
    fn fixture_payloads(tag: &str) -> [(&'static str, String); 3] {
        fixture::payloads(tag)
    }

    /// The manifest those payloads hash to, under THIS crate's SHA-256.
    ///
    /// That is what makes the shared definition a shared FACT rather than a
    /// shared string: the recipe generator builds the same manifest from the
    /// same payloads with its own copy of the same digest code, so the two
    /// agreeing is checked by the signature rather than assumed.
    fn fixture_manifest(tag: &str) -> String {
        fixture::manifest(tag, sha256::hex_digest)
    }

    /// The committed signature for a fixture manifest, as the hexadecimal TEXT
    /// a real `manifest.sig` holds — so what the fixture writes goes through
    /// the shipped parser on the way back in, rather than around it.
    fn fixture_signature(tag: &str) -> &'static str {
        match fixture::signature(tag) {
            Some(hex) => hex,
            None => panic!("no committed signature for fixture tag {tag:?}"),
        }
    }

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
            let manifest = fixture_manifest("");
            let id = sha256::hex_digest(manifest.as_bytes());
            let directory = self.root.join("td/deployments").join(&id);
            fs::create_dir(&directory).unwrap();
            for (label, bytes) in fixture_payloads("") {
                fs::write(directory.join(label), bytes).unwrap();
            }
            fs::write(directory.join("manifest"), manifest).unwrap();
            fs::write(
                directory.join(protocol::MANIFEST_SIG_NAME),
                format!("{}\n", fixture_signature("")),
            )
            .unwrap();
            id
        }

        /// The trust root these deployments verify under.
        fn key(&self) -> TrustRoot {
            decode_hex(FIXTURE_PUBLIC_KEY.as_bytes(), "fixture key").unwrap()
        }

        /// A key nobody signed under: the negative control for authentication.
        ///
        /// A whole valid key rather than a corruption of the real one, so a
        /// refusal cannot be about the key being malformed, the wrong length,
        /// or off the curve — only about the signature not being under THIS
        /// key. See `FIXTURE_OTHER_PUBLIC_KEY`.
        fn wrong_key(&self) -> TrustRoot {
            decode_hex(FIXTURE_OTHER_PUBLIC_KEY.as_bytes(), "other fixture key").unwrap()
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
            self.sign_source_verifiably(&directory, tag);
            (directory, id)
        }

        /// A source bundle carrying NO signature, for the two tests that are
        /// about publication of an unsigned one.
        ///
        /// The default is signed, and deliberately: an unsigned candidate is
        /// refused by the boot decision for that reason alone, which silently
        /// takes over from whatever a test was actually about. Three tests were
        /// defeated that way before this — one of them the only cover for
        /// `read_only_current`'s attempt-state guard, so the whole
        /// attempt/rollback mechanism went untested while staying green.
        fn unsigned_source_bundle(&self, name: &str, tag: &str) -> (PathBuf, String) {
            let (directory, id) = self.source_bundle(name, tag);
            fs::remove_file(directory.join(protocol::MANIFEST_SIG_NAME)).unwrap();
            (directory, id)
        }

        /// Put a detached signature beside a source bundle's manifest.
        ///
        /// Opaque bytes on purpose: nothing verifies them in this landing, and
        /// the property under test is that publishing CARRIES the file rather
        /// than anything about what it contains.
        fn sign_source(&self, directory: &Path, signature: &str) {
            fs::write(
                directory.join(protocol::MANIFEST_SIG_NAME),
                format!("{signature}\n"),
            )
            .unwrap();
        }

        /// Sign a source bundle so the deployment it publishes AUTHENTICATES.
        ///
        /// Distinct from `sign_source`, which writes opaque bytes for the tests
        /// that are only about publication carrying the file across: those
        /// would pass just as well with garbage, and this one cannot.
        fn sign_source_verifiably(&self, directory: &Path, tag: &str) {
            self.sign_source(directory, fixture_signature(tag));
        }

        fn published_signature(&self, id: &str) -> Option<Vec<u8>> {
            let path = self
                .root
                .join(protocol::DEPLOYMENTS_DIR)
                .join(id)
                .join(protocol::MANIFEST_SIG_NAME);
            fs::read(path).ok()
        }

        /// `selector` over a slot that already exists.
        fn selector_replace(&self, slot: &str, id: &str) {
            let path = self.root.join(protocol::BOOT_DIR).join(slot);
            if fs::symlink_metadata(&path).is_ok() {
                fs::remove_file(&path).unwrap();
            }
            self.selector(slot, id);
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
            Ok(Mode::Install {
                trusted_key: None,
                ..
            })
        ));
        assert!(matches!(
            parse_args(args(&["rollback", "/dev/vda", "/run/td-update"])),
            Ok(Mode::Rollback { .. })
        ));
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
            "/key.pub",
            "extra",
        ]))
        .is_err());
        // `update`'s key is REQUIRED, so the three-argument form — which is a
        // valid `install` — must not parse as an update that probes.
        assert!(parse_args(args(&["update", "/dev/vda", "/run/td-update", "/channel"])).is_err());
        assert!(parse_args(args(&[
            "update",
            "/dev/vda",
            "/run/td-update",
            "/channel",
            "/key.pub",
            "extra",
        ]))
        .is_err());
        match parse_args(args(&[
            "update",
            "/dev/vda",
            "/run/td-update",
            "/run/td-volume/td/incoming",
            "/run/td-volume/td/trusted.pub",
        ])) {
            Ok(Mode::Update {
                channel,
                trusted_key,
                ..
            }) => {
                assert_eq!(channel, Path::new("/run/td-volume/td/incoming"));
                assert_eq!(trusted_key, Path::new("/run/td-volume/td/trusted.pub"));
            }
            _ => panic!("update must parse and carry its channel and key"),
        }
        assert!(parse_args(args(&["rollback", "/volume"])).is_err());
        assert!(parse_args(args(&["rollback", "/dev/vda", "/volume", "extra"])).is_err());
        assert!(parse_args(args(&["success", "/dev/vda", "/volume", "not-a-digest"])).is_err());
        assert!(parse_args(args(&["unknown", "/volume"])).is_err());

        // `install` and `publish` are the file's two VARIABLE arities, so the
        // optional argument is checked by value rather than by shape: dropping
        // it on the floor is a fail-open, since `None` means "probe the default
        // and maybe publish unverified" while the caller believed it had named
        // a key.
        match parse_args(args(&[
            "install",
            "/dev/vda",
            "/run/td-update",
            "/incoming/deployment",
            "/run/td-volume/td/trusted.pub",
        ])) {
            Ok(Mode::Install { trusted_key, .. }) => {
                assert_eq!(
                    trusted_key.as_deref(),
                    Some(Path::new("/run/td-volume/td/trusted.pub"))
                );
            }
            _ => panic!("install must parse and carry its trusted key"),
        }
        assert!(matches!(
            parse_args(args(&["publish", "/volume", "/incoming/deployment"])),
            Ok(Mode::Publish {
                trusted_key: None,
                ..
            })
        ));
        match parse_args(args(&["publish", "/volume", "/incoming/deployment", "/key.pub"])) {
            Ok(Mode::Publish { trusted_key, .. }) => {
                assert_eq!(trusted_key.as_deref(), Some(Path::new("/key.pub")));
            }
            _ => panic!("publish must parse and carry its trusted key"),
        }
        assert!(parse_args(args(&["publish", "/volume"])).is_err());
        assert!(parse_args(args(&[
            "publish",
            "/volume",
            "/incoming/deployment",
            "/key.pub",
            "extra"
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

    /// A publish under a trust root REFUSES a bundle that does not verify.
    ///
    /// This is the half of §10 item 7c that is about security rather than
    /// plumbing. Before it, the first thing on the path to check a bundle was
    /// the next BOOT — so a deployment signed with the wrong key produced a
    /// machine that did not come back, discovered by whoever was standing in
    /// front of it. The wrong key is a whole valid one, so the refusal cannot
    /// be about a malformed key.
    #[test]
    fn a_publish_under_a_trust_root_refuses_what_does_not_verify() {
        let fixture = Fixture::new();
        let (source, id) = fixture.source_bundle("candidate", "next");

        let error = install_deployment(&fixture.root, &source, Some(&fixture.wrong_key()))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains(protocol::MANIFEST_UNAUTHENTICATED),
            "a bundle signed under another key must be refused: {error}"
        );
        assert!(
            !fixture
                .root
                .join(protocol::DEPLOYMENTS_DIR)
                .join(&id)
                .exists(),
            "the refused deployment must not have been published"
        );

        // ...and the SAME bundle under the right key publishes, so the refusal
        // above is about the key and not about the bundle.
        assert_eq!(
            install_deployment(&fixture.root, &source, Some(&fixture.key())).unwrap(),
            id
        );
        // What landed carries the signature that was authenticated, rather than
        // a deployment published beside a signature nothing checked.
        let published = fixture
            .root
            .join(protocol::DEPLOYMENTS_DIR)
            .join(&id)
            .join(protocol::MANIFEST_SIG_NAME);
        assert_eq!(
            fs::read_to_string(&published).unwrap().trim(),
            fixture_signature("next"),
            "the published signature is the one that authenticated"
        );
    }

    /// A bundle nobody vouched for is refused WITHOUT its contents being
    /// interpreted — the order DESIGN §10 item 6 states as policy for the boot
    /// path, and an install source is exactly as untrusted as a slot.
    ///
    /// Staged so the two orders give different answers: the payloads are
    /// removed, so anything that reached `verify_payload` would fail about a
    /// missing `bzImage`. Getting the signature error instead is what says the
    /// check ran first. A manifest that does not even PARSE is the sharper half
    /// — under the wrong order its parse error would arrive before any question
    /// of authenticity.
    #[test]
    fn a_bundle_nobody_vouched_for_is_refused_before_it_is_interpreted() {
        let fixture = Fixture::new();
        let (source, _) = fixture.source_bundle("candidate", "next");
        for (label, _) in fixture_payloads("next") {
            fs::remove_file(source.join(label)).unwrap();
        }

        // `VerifiedBundle` holds open files and is deliberately not `Debug`.
        let refusal = |r: io::Result<VerifiedBundle>| r.map(|_| ()).unwrap_err().to_string();

        let error = refusal(open_bundle(&source, Some(&fixture.wrong_key())));
        assert!(
            error.contains(protocol::MANIFEST_UNAUTHENTICATED),
            "authenticity must be decided before the payloads are read: {error}"
        );

        // The same source with no trust root gets as far as the payloads, which
        // is what proves the assertion above is about ORDER and not about the
        // payloads being unreachable anyway.
        let untrusted = refusal(open_bundle(&source, None));
        assert!(
            untrusted.contains("bzImage"),
            "without a trust root the payloads are what fails: {untrusted}"
        );
    }

    /// An UNSIGNED bundle is refused under a trust root, rather than published
    /// as an unverifiable one. Without this the check is trivially escapable:
    /// deleting `manifest.sig` would turn a refusal into a successful install.
    #[test]
    fn a_publish_under_a_trust_root_refuses_an_unsigned_bundle() {
        let fixture = Fixture::new();
        let (source, id) = fixture.unsigned_source_bundle("candidate", "next");

        let error = install_deployment(&fixture.root, &source, Some(&fixture.key()))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("carries no signature"),
            "an unsigned bundle must be refused under a trust root: {error}"
        );

        // With NO trust root the same bundle publishes, which is what keeps a
        // rootfs that has no key behaving exactly as it did.
        assert_eq!(
            install_deployment(&fixture.root, &source, None).unwrap(),
            id
        );
    }

    /// Absence of a trust root and UNREADABILITY of one are different answers.
    ///
    /// The distinction is the whole of `install_trust_root`: a rootfs with no
    /// key publishes as it always has, and a key that is present but truncated
    /// or mistyped is a provisioning fault that must not be published through —
    /// publishing anyway would hide it until the machine failed to boot.
    #[test]
    fn an_explicit_trust_root_is_fail_closed() {
        let fixture = Fixture::new();
        let good = fixture.root.join("good.pub");
        fs::write(&good, format!("{FIXTURE_PUBLIC_KEY}\n")).unwrap();
        assert_eq!(
            install_trust_root(Some(&good), &fixture.root).unwrap(),
            Some(fixture.key())
        );

        let truncated = fixture.root.join("truncated.pub");
        fs::write(&truncated, "9ae001c4\n").unwrap();
        assert!(
            install_trust_root(Some(&truncated), &fixture.root).is_err(),
            "a truncated key must refuse rather than read as absent"
        );

        // A path that does not exist, asked for EXPLICITLY, is an error rather
        // than an absence: only the unasked-for default may answer "no trust
        // root", or a mistyped argument would silently publish unverified.
        let missing = fixture.root.join("nothing-here.pub");
        assert!(
            install_trust_root(Some(&missing), &fixture.root).is_err(),
            "an explicit key that is not there must refuse"
        );
    }

    /// The DEFAULT branch, which is the one production takes.
    ///
    /// It is the branch worth testing hardest and the one a hardcoded `/` would
    /// have made unreachable — hence the `rootfs` parameter, as
    /// `read_boot_trust_root` has for the same reason. Three states, three
    /// answers: absent is the only "no trust root", and present-but-unusable
    /// refuses, because a truncated or mistyped key is a provisioning fault and
    /// publishing through it hides that fault until the machine fails to boot.
    #[test]
    fn only_an_absent_default_trust_root_means_there_is_none() {
        let fixture = Fixture::new();
        let path = fixture.root.join(protocol::TRUSTED_KEY_PATH);
        let parent = path.parent().unwrap().to_path_buf();
        fs::create_dir_all(&parent).unwrap();

        assert_eq!(
            install_trust_root(None, &fixture.root).unwrap(),
            None,
            "no key file at all is the one absence"
        );

        fs::write(&path, "9ae001c4\n").unwrap();
        assert!(
            install_trust_root(None, &fixture.root).is_err(),
            "a truncated default key must refuse, not read as absent"
        );

        fs::write(&path, format!("{FIXTURE_PUBLIC_KEY}\n")).unwrap();
        assert_eq!(
            install_trust_root(None, &fixture.root).unwrap(),
            Some(fixture.key()),
            "a usable default key is found without being asked for"
        );
    }

    /// What production passes for that rootfs, stated as a literal — the
    /// parameter exists for the test above, and this is what pins the real one.
    ///
    /// What it rules out is a BOOT trust root read off the volume, which is
    /// §6's surface: it would authorise the deployments sitting beside it, so
    /// whoever writes a forged one writes the matching key too. The key item
    /// 10 puts on the volume is a different role — it checks an incoming
    /// bundle, and the boot path still authenticates under the initramfs's
    /// key, so it can admit a deployment to the volume and never to a boot.
    /// §6 carries that reconciliation; this pin is only the probe half of it,
    /// and a probe is what would make ABSENCE mean "publish without checking".
    ///
    /// `update` must never publish unauthenticated, and the type only gets that
    /// most of the way: `publish_authenticated` takes a `&TrustRoot`, so no
    /// caller can hand it `None` — but its own one-line body could still pass
    /// one on, and that mutation was green with everything else in place.
    ///
    /// Pinned as TEXT because the behavioural route needs a block device: the
    /// transaction lock refuses a character device before any mount, so nothing
    /// in this crate can drive a publish to completion.
    #[test]
    fn the_authenticated_publish_passes_the_trust_root_it_was_given() {
        let source = include_str!("main.rs");
        let body = source
            .split("fn publish_authenticated(")
            .nth(1)
            .and_then(|rest| rest.split("\n}").next())
            .unwrap_or("");
        assert!(
            body.contains("Some(trust)"),
            "publish_authenticated must pass its trust root on: {body}"
        );
        assert!(
            !body.contains("None"),
            "publish_authenticated must not be able to publish unverified: {body}"
        );
    }

    /// Every shipped call must also PASS ITS PARAMETER as its FIRST argument:
    /// a `None` there compiles and would silently ignore the key the caller
    /// named.
    #[test]
    fn the_shipped_trust_root_calls_probe_the_rootfs_and_honour_their_argument() {
        let source = include_str!("main.rs");
        // The FIRST ARGUMENT, not the line: `update` passes `Some(trusted_key)`
        // where the other two pass the `Option` they were handed, so the needle
        // cannot be one spelling of the call — and a needle that merely wants
        // `trusted_key` SOMEWHERE on the line is satisfied by
        // `{ let _ = trusted_key; … (None, …) }`, which is the fail-open itself.
        let call = concat!("install_trust_root", "(");
        let mut calls = 0;
        for line in source.lines() {
            if line.contains("install_trust_root(") && !line.contains("fn install_trust_root") {
                // A comment is not a call, and this file's prose names both
                // arguments the assertions below look for.
                if line.trim_start().starts_with("//") {
                    continue;
                }
                if line.contains("&fixture.root") {
                    continue;
                }
                calls += 1;
                assert!(
                    line.contains("Path::new(\"/\")"),
                    "a shipped install_trust_root call must name the running rootfs: {line}"
                );
                let first = line
                    .split(call)
                    .nth(1)
                    .and_then(|rest| rest.split(',').next())
                    .unwrap_or("");
                assert!(
                    first.contains("trusted_key"),
                    "a shipped trust-root call must pass the key it was given: {line}"
                );
            }
        }
        assert_eq!(
            calls, 3,
            "run_install, run_publish and run_update are its shipped callers"
        );
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
            let error = run_publish(Path::new("/proc"), &fixture.root, None)
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("is a mount point"),
                "publish must refuse a mounted root: {error}"
            );
        }
    }

    /// The argv hop the source scan cannot see, pinned by BEHAVIOUR.
    ///
    /// `dispatch`'s `Install` arm can drop `trusted_key` and still compile —
    /// that is the fail-open the verb exists to prevent, and it passed every
    /// other test in this crate. So this drives `dispatch` with a key that is
    /// not there and a device that is not a volume, and requires the error to
    /// name THE KEY: the trust root is resolved before the device is touched,
    /// so whichever complaint comes back says which of the two ran first.
    /// Dropping the argument anywhere between argv and `install_trust_root`
    /// leaves `None`, which probes instead and gets as far as the device.
    ///
    /// It needs no root and no block device precisely because it never reaches
    /// one.
    #[test]
    fn a_named_trust_root_is_resolved_before_the_device_is_touched() {
        let fixture = Fixture::new();
        let missing = fixture.root.join("no-such-key.pub");
        let error = dispatch(Mode::Install {
            device: PathBuf::from("/dev/null"),
            mountpoint: fixture.root.join("mountpoint"),
            source: fixture.root.join("bundle"),
            trusted_key: Some(missing.clone()),
        })
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("no-such-key.pub"),
            "a named trust root must be read before the device is looked at: {error}"
        );
        assert!(
            !error.contains("/dev/null"),
            "the device must not have been reached: {error}"
        );
    }

    /// `update` is the verb a timer runs, so what it does when there is
    /// NOTHING to do is most of its behaviour.
    ///
    /// Three outcomes are pinned here and the difference between them is the
    /// whole design: a channel that is not there is a configuration fault and
    /// must be loud, a candidate that is not there is an up-to-date machine and
    /// must be silent, and a trust root that is not readable must be loud in
    /// BOTH cases — including the idle one, which is the run that can afford to
    /// report it.
    ///
    /// None of these reaches a block device, which is what makes them testable:
    /// `/dev/null` stands in, and an error naming it would mean a check ran in
    /// the wrong order.
    #[test]
    fn update_is_quiet_only_when_the_channel_has_nothing_to_offer() {
        let fixture = Fixture::new();
        let channel = fixture.root.join("channel");
        fs::create_dir(&channel).unwrap();
        let key = fixture.root.join("trusted.pub");
        fs::write(&key, format!("{FIXTURE_PUBLIC_KEY}\n")).unwrap();

        // An empty channel is the ordinary state of an up-to-date machine.
        assert!(
            run_update(
                Path::new("/dev/null"),
                &fixture.root.join("mountpoint"),
                &channel,
                &key,
            )
            .is_ok(),
            "an absent candidate is nothing to do, not a failure"
        );

        // A channel that is not there is a typo, and answering "nothing to do"
        // to a typo hides it until an update is urgently needed.
        let error = run_update(
            Path::new("/dev/null"),
            &fixture.root.join("mountpoint"),
            &fixture.root.join("no-such-channel"),
            &key,
        )
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("no-such-channel"),
            "a missing channel must name itself: {error}"
        );

        // A file where the channel should be is the same class of fault.
        let file = fixture.root.join("channel-file");
        fs::write(&file, b"not a channel").unwrap();
        let error = run_update(
            Path::new("/dev/null"),
            &fixture.root.join("mountpoint"),
            &file,
            &key,
        )
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("must be a real directory"),
            "a channel that is not a directory must be refused: {error}"
        );

        // Nothing sets a timer's working directory, so a relative path is a
        // different file depending on who started it. Both arguments, because
        // the channel's own check runs before the mount and `install_deployment`
        // would otherwise catch it only inside the transaction.
        for relative in [Path::new("channel"), Path::new("trusted.pub")] {
            let error = run_update(
                Path::new("/dev/null"),
                &fixture.root.join("mountpoint"),
                if relative == Path::new("channel") {
                    relative
                } else {
                    &channel
                },
                if relative == Path::new("channel") {
                    &key
                } else {
                    relative
                },
            )
            .unwrap_err()
            .to_string();
            assert!(
                error.contains("must be an absolute path"),
                "a relative {} must be refused: {error}",
                relative.display()
            );
        }

        // A channel that cannot be READ is loud too. It is the case that would
        // most plausibly have been folded into "nothing to offer", and it is a
        // fault rather than an absence.
        let sealed = fixture.root.join("sealed");
        fs::create_dir(&sealed).unwrap();
        fs::set_permissions(&sealed, fs::Permissions::from_mode(0o000)).unwrap();
        let denied = run_update(
            Path::new("/dev/null"),
            &fixture.root.join("mountpoint"),
            &sealed,
            &key,
        );
        fs::set_permissions(&sealed, fs::Permissions::from_mode(0o755)).unwrap();
        // Root ignores the mode, so this asserts only where it can mean anything.
        if !running_as_root() {
            let error = denied.unwrap_err().to_string();
            assert!(
                error.contains("update candidate"),
                "an unreadable channel must be loud: {error}"
            );
        }

        // The idle run still reads the key, so a broken trust root surfaces on
        // a tick with nothing to install rather than on the tick that has one.
        let error = run_update(
            Path::new("/dev/null"),
            &fixture.root.join("mountpoint"),
            &channel,
            &fixture.root.join("no-such-key.pub"),
        )
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("no-such-key.pub"),
            "an idle update must still check its trust root: {error}"
        );
        assert!(
            !error.contains("/dev/null"),
            "the device must not have been reached: {error}"
        );
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

    /// What the decision IS, which is the half `run_update` can be driven to.
    ///
    /// A candidate that is there must come back WITH a trust root — the
    /// alternative, publishing unauthenticated, is what review found green
    /// before `publish_authenticated` made it untypeable, and this is the other
    /// half of that: the decision must actually carry the key it read.
    #[test]
    fn a_candidate_that_is_there_is_decided_on_with_its_trust_root() {
        let fixture = Fixture::new();
        let channel = fixture.root.join("channel");
        fs::create_dir(&channel).unwrap();
        let key = fixture.root.join("trusted.pub");
        fs::write(&key, format!("{FIXTURE_PUBLIC_KEY}\n")).unwrap();

        assert!(
            update_decision(&channel, &key).unwrap().is_none(),
            "an empty channel decides on nothing"
        );

        let candidate = channel.join(protocol::CHANNEL_CANDIDATE);
        fs::create_dir(&candidate).unwrap();
        let (chosen, trust) = update_decision(&channel, &key)
            .unwrap()
            .expect("a candidate that is there must be decided on");
        assert_eq!(chosen, candidate, "the decision names the candidate");
        assert_eq!(
            trust,
            read_trusted_key(&key).unwrap(),
            "the decision carries the key it was told to check under"
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
        let channel = fixture.root.join("channel");
        fs::create_dir(&channel).unwrap();
        fs::create_dir(channel.join(protocol::CHANNEL_CANDIDATE)).unwrap();
        let key = fixture.root.join("trusted.pub");
        fs::write(&key, format!("{FIXTURE_PUBLIC_KEY}\n")).unwrap();

        let error = run_update(
            Path::new("/dev/null"),
            &fixture.root.join("mountpoint"),
            &channel,
            &key,
        )
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("/dev/null"),
            "an update with a candidate must reach the device: {error}"
        );
    }

    /// `update`'s argv hop, pinned by BEHAVIOUR for `install`'s reason: the
    /// dispatch arm can drop `trusted_key` and still compile, and for this verb
    /// there is no `Option` to fall back to — it would not compile as `None`,
    /// but it WOULD compile pointing at the wrong one of its four paths, and
    /// `channel` and `trusted_key` are adjacent `PathBuf`s of the same type.
    #[test]
    fn dispatch_hands_update_its_channel_and_its_key_the_right_way_round() {
        let fixture = Fixture::new();
        let channel = fixture.root.join("channel");
        fs::create_dir(&channel).unwrap();

        // The key is missing, so the run must fail NAMING it. Swapped, the
        // channel would be the unreadable key and the key the missing channel,
        // and each mistake names the other path.
        let error = dispatch(Mode::Update {
            device: PathBuf::from("/dev/null"),
            mountpoint: fixture.root.join("mountpoint"),
            channel: channel.clone(),
            trusted_key: fixture.root.join("no-such-key.pub"),
        })
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("no-such-key.pub") && !error.contains("channel"),
            "the key must arrive as the key: {error}"
        );
    }

    /// A candidate must be a real directory, `require_real_directory`'s rule
    /// everywhere else in this file. A symlink would make the channel's
    /// contents depend on something outside it that nothing here
    /// authenticates the path of.
    #[test]
    fn an_update_candidate_that_is_not_a_real_directory_is_refused() {
        let fixture = Fixture::new();
        let channel = fixture.root.join("channel");
        fs::create_dir(&channel).unwrap();
        let key = fixture.root.join("trusted.pub");
        fs::write(&key, format!("{FIXTURE_PUBLIC_KEY}\n")).unwrap();
        let elsewhere = fixture.root.join("elsewhere");
        fs::create_dir(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, channel.join(protocol::CHANNEL_CANDIDATE)).unwrap();

        let error = run_update(
            Path::new("/dev/null"),
            &fixture.root.join("mountpoint"),
            &channel,
            &key,
        )
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
            install_deployment(&fixture.root, &source, None).unwrap(),
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

    /// The gap this landing exists for: `publish_bundle` copied four literal
    /// names, so a detached signature beside the manifest was dropped without a
    /// word and no machine could ever have one to verify.
    #[test]
    fn publishing_carries_the_detached_signature() {
        let fixture = Fixture::new();
        let initial = fixture.valid_deployment();
        fixture.selector("current", &initial);
        fixture.selector("previous", &initial);
        let (source, candidate) = fixture.source_bundle("candidate", "next");
        fixture.sign_source(&source, "aa11");

        assert_eq!(
            install_deployment(&fixture.root, &source, None).unwrap(),
            candidate
        );
        assert_eq!(
            fixture.published_signature(&candidate).as_deref(),
            Some(&b"aa11\n"[..]),
            "the signature must reach the deployment directory"
        );
    }

    /// An unsigned bundle still installs, because nothing verifies a signature
    /// yet and requiring one would make every existing bundle uninstallable.
    #[test]
    fn an_unsigned_bundle_still_publishes() {
        let fixture = Fixture::new();
        let initial = fixture.valid_deployment();
        fixture.selector("current", &initial);
        fixture.selector("previous", &initial);
        let (source, candidate) = fixture.unsigned_source_bundle("candidate", "next");

        assert_eq!(
            install_deployment(&fixture.root, &source, None).unwrap(),
            candidate
        );
        assert_eq!(fixture.published_signature(&candidate), None);
    }

    /// D3's whole point, made reachable: re-signing does not change the id, so
    /// an id-only comparison made the publish a no-op and a rotated key could
    /// never reach a machine. The payloads are already verified identical, so
    /// only the signature is replaced.
    #[test]
    fn re_signing_an_installed_deployment_updates_the_signature_in_place() {
        let fixture = Fixture::new();
        let initial = fixture.valid_deployment();
        fixture.selector("current", &initial);
        fixture.selector("previous", &initial);
        let (source, candidate) = fixture.source_bundle("candidate", "next");
        fixture.sign_source(&source, "aa11");
        install_deployment(&fixture.root, &source, None).unwrap();

        // Same bundle, new key. The id is unchanged by construction.
        fixture.sign_source(&source, "bb22");
        assert_eq!(
            install_deployment(&fixture.root, &source, None).unwrap(),
            candidate,
            "the deployment keeps its id across a re-signing"
        );
        assert_eq!(
            fixture.published_signature(&candidate).as_deref(),
            Some(&b"bb22\n"[..]),
            "the installed signature must be the one just supplied"
        );
        // And nothing is left over from the in-place replacement.
        let entries = fs::read_dir(fixture.root.join(protocol::DEPLOYMENTS_DIR).join(&candidate))
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        assert!(
            entries.iter().all(|name| !name.as_bytes().starts_with(b".")),
            "no staging temporary survives: {entries:?}"
        );
    }

    /// The property the fixed temporary name did not have. `write_synced_file`
    /// creates with `create_new`, so ONE name means a second staging — a
    /// concurrent publisher, or a rotation after a crash left the name taken —
    /// fails `AlreadyExists`. Nothing reaps inside a deployment directory
    /// (`reap_install_temporaries` sweeps `.install-` entries in the
    /// deployments directory alone), so that failure was permanent.
    ///
    /// Asserted on the staging function directly rather than through two
    /// threads: the overlap a thread pair needs to demonstrate this is timing,
    /// and a boot-critical gate should not carry a test that depends on it.
    #[test]
    fn a_taken_signature_temporary_does_not_block_the_next_one() {
        let fixture = Fixture::new();
        let directory = fixture.root.join("staging");
        fs::create_dir(&directory).unwrap();

        let (first, keep_first) = write_signature_temporary(&directory, b"aa11\n").unwrap();
        // Held, so the first name is still taken when the second is staged.
        let (second, keep_second) = write_signature_temporary(&directory, b"bb22\n").unwrap();
        assert_ne!(first, second, "a second staging must not reuse the name");
        assert_eq!(fs::read(&first).unwrap(), b"aa11\n", "and must not clobber it");
        assert_eq!(fs::read(&second).unwrap(), b"bb22\n");

        // Both are still this function's own to remove.
        drop(keep_first);
        drop(keep_second);
        assert!(!first.exists() && !second.exists(), "the guards remove both");
    }

    /// A rotation that cannot rename must not leave its temporary behind, or
    /// the failure it reports becomes permanent for every later attempt.
    #[test]
    fn a_failed_rotation_leaves_no_temporary() {
        let fixture = Fixture::new();
        let initial = fixture.valid_deployment();
        fixture.selector("current", &initial);
        fixture.selector("previous", &initial);
        let (source, candidate) = fixture.source_bundle("candidate", "next");
        fixture.sign_source(&source, "aa11");
        install_deployment(&fixture.root, &source, None).unwrap();

        // A directory at the destination makes the rename fail with the
        // temporary already written.
        let published = fixture.root.join(protocol::DEPLOYMENTS_DIR).join(&candidate);
        fs::remove_file(published.join(protocol::MANIFEST_SIG_NAME)).unwrap();
        fs::create_dir(published.join(protocol::MANIFEST_SIG_NAME)).unwrap();
        assert!(replace_signature(&published, b"bb22\n").is_err());

        let leftovers = fs::read_dir(&published)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .filter(|name| name.as_bytes().starts_with(b".manifest.sig"))
            .collect::<Vec<_>>();
        assert!(leftovers.is_empty(), "a failed rotation left {leftovers:?}");
    }

    /// Re-installing an IDENTICAL signed bundle must be a true no-op. Asserted
    /// on the inode, because rewriting the same bytes is invisible in the
    /// contents: a classifier that sent `(Some(x), Some(x))` down the resign
    /// path would rename a fresh file over the old one and stay green on every
    /// other assertion here.
    #[test]
    fn re_installing_an_identical_signed_bundle_rewrites_nothing() {
        let fixture = Fixture::new();
        let initial = fixture.valid_deployment();
        fixture.selector("current", &initial);
        fixture.selector("previous", &initial);
        let (source, candidate) = fixture.source_bundle("candidate", "next");
        fixture.sign_source(&source, "aa11");
        install_deployment(&fixture.root, &source, None).unwrap();

        let path = fixture
            .root
            .join(protocol::DEPLOYMENTS_DIR)
            .join(&candidate)
            .join(protocol::MANIFEST_SIG_NAME);
        let before = fs::symlink_metadata(&path).unwrap().ino();
        install_deployment(&fixture.root, &source, None).unwrap();
        assert_eq!(
            fs::symlink_metadata(&path).unwrap().ino(),
            before,
            "an unchanged signature must not be rewritten"
        );
    }

    /// The bound is a read limit, not a taste: exactly `MAX_SIGNATURE_BYTES` is
    /// accepted, and only one byte more is refused.
    #[test]
    fn a_signature_of_exactly_the_bound_is_accepted() {
        let fixture = Fixture::new();
        let (source, _) = fixture.source_bundle("candidate", "next");
        let path = source.join(protocol::MANIFEST_SIG_NAME);

        // Pinned by LITERAL, not by the constant: a fixture sized from the
        // constant shrinks with it, so lowering the bound would leave this
        // green while refusing signatures td-deploy actually writes.
        assert_eq!(
            protocol::MAX_SIGNATURE_BYTES,
            160,
            "128 hex characters and a newline, with slack for a trailing CRLF"
        );
        fs::write(&path, vec![b'a'; 160]).unwrap();
        let bundle = open_bundle(&source, None).unwrap();
        assert_eq!(
            bundle.signature.as_deref(),
            Some(&vec![b'a'; 160][..]),
            "exactly at the bound must be read, not refused"
        );
        // And one byte more is not.
        fs::write(&path, vec![b'a'; 161]).unwrap();
        assert!(open_bundle(&source, None).map(|_| ()).is_err(), "one past the bound");
    }

    /// Upgrading an already-installed unsigned deployment to a signed one is
    /// the same path, and is what a first signed release does to a machine
    /// running an unsigned bundle.
    #[test]
    fn signing_an_already_installed_unsigned_deployment_adds_the_signature() {
        let fixture = Fixture::new();
        let initial = fixture.valid_deployment();
        fixture.selector("current", &initial);
        fixture.selector("previous", &initial);
        let (source, candidate) = fixture.unsigned_source_bundle("candidate", "next");
        install_deployment(&fixture.root, &source, None).unwrap();
        assert_eq!(fixture.published_signature(&candidate), None);

        fixture.sign_source(&source, "cc33");
        install_deployment(&fixture.root, &source, None).unwrap();
        assert_eq!(
            fixture.published_signature(&candidate).as_deref(),
            Some(&b"cc33\n"[..])
        );
    }

    /// The one direction that is refused. Removing a signature is a downgrade,
    /// and doing nothing would silently ignore what the caller asked for — so
    /// neither is done, and the installed signature is left exactly as it was.
    #[test]
    fn withdrawing_a_signature_is_refused_rather_than_performed() {
        let fixture = Fixture::new();
        let initial = fixture.valid_deployment();
        fixture.selector("current", &initial);
        fixture.selector("previous", &initial);
        let (source, candidate) = fixture.source_bundle("candidate", "next");
        fixture.sign_source(&source, "dd44");
        install_deployment(&fixture.root, &source, None).unwrap();

        fs::remove_file(source.join(protocol::MANIFEST_SIG_NAME)).unwrap();
        let error = install_deployment(&fixture.root, &source, None).unwrap_err();
        assert!(
            error.to_string().contains("refusing to remove it"),
            "refused, but for {error}"
        );
        assert_eq!(
            fixture.published_signature(&candidate).as_deref(),
            Some(&b"dd44\n"[..]),
            "the installed signature must survive the refusal"
        );
    }

    /// A signature that EXISTS but is not a readable regular file of bounded
    /// size is a bundle to refuse, not one to treat as unsigned — silently
    /// downgrading to "no signature" is the fail-open the verifying half must
    /// not inherit.
    #[test]
    fn a_malformed_signature_is_refused_rather_than_read_as_absent() {
        let fixture = Fixture::new();
        let (source, _) = fixture.source_bundle("candidate", "next");
        let path = source.join(protocol::MANIFEST_SIG_NAME);
        // `VerifiedBundle` holds open files and is deliberately not `Debug`.
        let refusal = |r: io::Result<VerifiedBundle>| r.map(|_| ()).unwrap_err();

        fs::write(&path, vec![b'a'; (protocol::MAX_SIGNATURE_BYTES + 1) as usize]).unwrap();
        let error = refusal(open_bundle(&source, None));
        assert!(error.to_string().contains("deployment signature"), "{error}");

        fs::remove_file(&path).unwrap();
        symlink("manifest", &path).unwrap();
        let error = refusal(open_bundle(&source, None));
        assert!(
            error.to_string().contains("must be a real regular file"),
            "a symlinked signature must be refused, got {error}"
        );

        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        let error = refusal(open_bundle(&source, None));
        assert!(
            error.to_string().contains("must be a real regular file"),
            "a directory must be refused, got {error}"
        );
    }

    #[test]
    fn failed_pending_mark_keeps_the_verified_current_as_previous() {
        let fixture = Fixture::new();
        let current = fixture.valid_deployment();
        fixture.selector("current", &current);
        fixture.selector("previous", &current);
        let (source, previous) = fixture.source_bundle("previous", "previous");
        let bundle = open_bundle(&source, None).unwrap();
        publish_bundle(&fixture.root, bundle, false).unwrap();
        replace_selector(&fixture.root, "previous", &previous).unwrap();
        let attempts = attempts_directory(&fixture.root, true).unwrap().unwrap();
        fs::create_dir(attempts.join(&previous)).unwrap();

        assert!(install_deployment(&fixture.root, &source, None).is_err());
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

        install_deployment(&fixture.root, &source, None).unwrap();
        let first = select_boot_deployment(&fixture.root, &fixture.key()).unwrap();
        assert_eq!(
            first.remaining_attempts,
            Some(protocol::DEFAULT_BOOT_ATTEMPTS - 1)
        );
        install_deployment(&fixture.root, &source, None).unwrap();

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

        install_deployment(&fixture.root, &source, None).unwrap();
        fs::remove_dir_all(fixture.root.join("td/deployments").join(initial)).unwrap();
        let error = install_deployment(&fixture.root, &source, None).unwrap_err();

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

        install_deployment(&fixture.root, &source, None).unwrap();
        write_attempt_state(&fixture.root, &initial, 1).unwrap();
        let error = install_deployment(&fixture.root, &source, None).unwrap_err();

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

        let error = install_deployment(&fixture.root, &source, None).unwrap_err();

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

        install_deployment(&fixture.root, &source, None).unwrap();

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

        let error = install_deployment(&fixture.root, &source, None).unwrap_err();
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

        let error = install_deployment(&fixture.root, &source, None).unwrap_err();

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
        install_deployment(&fixture.root, &source, None).unwrap();

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
        install_deployment(&fixture.root, &source, None).unwrap();

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

        let error = install_deployment(&fixture.root, &source, None).unwrap_err();
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
    fn rollback_rejects_a_pending_previous_without_moving_current() {
        let fixture = Fixture::new();
        let initial = fixture.valid_deployment();
        fixture.selector("current", &initial);
        fixture.selector("previous", &initial);
        let (source, candidate) = fixture.source_bundle("candidate", "next");
        install_deployment(&fixture.root, &source, None).unwrap();
        fs::remove_file(fixture.root.join("td/boot/previous")).unwrap();
        fixture.selector("previous", &candidate);

        let error = rollback_deployment(&fixture.root).unwrap_err();

        assert!(error.to_string().contains("is not marked successful"));
        assert_eq!(read_selector(&fixture.root, "current").unwrap(), candidate);
        assert_eq!(read_selector(&fixture.root, "previous").unwrap(), candidate);
    }

    #[test]
    fn first_install_bootstraps_both_selectors() {
        let fixture = Fixture::new();
        let (source, candidate) = fixture.source_bundle("candidate", "first");

        assert_eq!(
            install_deployment(&fixture.root, &source, None).unwrap(),
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
        install_deployment(&fixture.root, &source, None).unwrap();
        let attempts = fixture.root.join(protocol::ATTEMPTS_DIR);
        fs::set_permissions(&attempts, fs::Permissions::from_mode(0o755)).unwrap();

        let error = select_boot_deployment(&fixture.root, &fixture.key()).err().unwrap();
        let (decision, _) = read_only_recovery(&fixture.root, &error, &fixture.key()).unwrap();

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
        install_deployment(&fixture.root, &source, None).unwrap();

        for remaining in [2, 1, 0] {
            let decision = select_boot_deployment(&fixture.root, &fixture.key()).unwrap();
            assert_eq!(decision.slot, "current");
            assert_eq!(decision.deployment_id, candidate);
            assert_eq!(decision.remaining_attempts, Some(remaining));
            assert!(decision.exhausted_deployment.is_none());
        }

        let decision = select_boot_deployment(&fixture.root, &fixture.key()).unwrap();
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
        install_deployment(&fixture.root, &source, None).unwrap();
        fs::remove_dir_all(fixture.root.join("td/deployments").join(initial)).unwrap();
        for _ in 0..protocol::DEFAULT_BOOT_ATTEMPTS {
            select_boot_deployment(&fixture.root, &fixture.key()).unwrap();
        }

        let decision = select_boot_deployment(&fixture.root, &fixture.key()).unwrap();

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
        install_deployment(&fixture.root, &source, None).unwrap();
        select_boot_deployment(&fixture.root, &fixture.key()).unwrap();
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
        let decision = select_boot_deployment(&fixture.root, &fixture.key()).unwrap();
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
        install_deployment(&fixture.root, &source, None).unwrap();

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
        install_deployment(&fixture.root, &source, None).unwrap();
        fs::write(
            fixture.root.join(protocol::ATTEMPTS_DIR).join(&candidate),
            b"malformed\n",
        )
        .unwrap();

        let decision = select_boot_deployment(&fixture.root, &fixture.key()).unwrap();

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
        install_deployment(&fixture.root, &source, None).unwrap();
        fs::remove_dir_all(fixture.root.join("td/deployments").join(initial)).unwrap();
        fs::write(
            fixture.root.join(protocol::ATTEMPTS_DIR).join(&candidate),
            b"malformed\n",
        )
        .unwrap();

        let decision = select_boot_deployment(&fixture.root, &fixture.key()).unwrap();

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

    /// The fail-closed flip: a deployment nobody vouched for does not boot.
    ///
    /// Every other assertion in this file is about a deployment being WHOLE.
    /// This one is about it being VOUCHED FOR, which hashes cannot answer: the
    /// payloads here verify perfectly and the manifest hashes to its own id, so
    /// nothing but the signature distinguishes it from the deployments the
    /// tests above boot.
    #[test]
    fn a_deployment_signed_under_another_key_does_not_boot() {
        let fixture = Fixture::new();
        let initial = fixture.valid_deployment();
        fixture.selector("current", &initial);
        fixture.selector("previous", &initial);

        let accepted = select_boot_deployment(&fixture.root, &fixture.key()).unwrap();
        let refused = select_boot_deployment(&fixture.root, &fixture.wrong_key())
            .err()
            .unwrap();

        // Same volume, same bytes, one byte different in the key.
        assert_eq!(accepted.deployment_id, initial);
        assert_eq!(refused.kind(), io::ErrorKind::InvalidData);
        assert!(
            refused.to_string().contains("signature"),
            "the refusal must name what was wrong: {refused}"
        );
    }

    /// An unsigned deployment is refused rather than trusted, which is the
    /// direction that matters: the absent file must not read as consent.
    #[test]
    fn an_unsigned_deployment_does_not_boot() {
        let fixture = Fixture::new();
        let initial = fixture.valid_deployment();
        fixture.selector("current", &initial);
        fixture.selector("previous", &initial);
        fs::remove_file(
            fixture
                .root
                .join(protocol::DEPLOYMENTS_DIR)
                .join(&initial)
                .join(protocol::MANIFEST_SIG_NAME),
        )
        .unwrap();

        let refused = select_boot_deployment(&fixture.root, &fixture.key())
            .err()
            .unwrap();

        assert!(
            refused.to_string().contains("carries no manifest.sig"),
            "an unsigned deployment must be refused by name: {refused}"
        );
    }

    /// The read-only fast path authenticates too, and this is the one that
    /// would have been easy to miss: it returns `Option`, so an unauthenticated
    /// current does not error there — it declines, and the writable path
    /// refuses it properly afterwards. What must not happen is a kexec.
    #[test]
    fn the_read_only_fast_path_declines_a_deployment_it_cannot_authenticate() {
        let fixture = Fixture::new();
        let initial = fixture.valid_deployment();
        fixture.selector("current", &initial);

        assert!(read_only_current(&fixture.root, &fixture.key()).is_some());
        assert!(read_only_current(&fixture.root, &fixture.wrong_key()).is_none());
    }

    /// Read-only RECOVERY authenticates both slots. It runs when the writable
    /// transaction failed, so it is the path with the least state to reason
    /// about and the most temptation to accept whatever verifies.
    #[test]
    fn read_only_recovery_refuses_slots_it_cannot_authenticate() {
        let fixture = Fixture::new();
        let initial = fixture.valid_deployment();
        fixture.selector("current", &initial);
        fixture.selector("previous", &initial);
        let transaction_error = io::Error::other("read-write mount rejected");

        assert!(read_only_recovery(&fixture.root, &transaction_error, &fixture.key()).is_ok());
        let refused = read_only_recovery(&fixture.root, &transaction_error, &fixture.wrong_key())
            .err()
            .unwrap();

        assert!(
            refused.to_string().contains("signature"),
            "recovery must refuse an unauthenticated slot: {refused}"
        );
    }

    /// Authenticity is decided BEFORE any payload digest is read, so the
    /// hashes that choose what to kexec are only ever hashes out of vouched-for
    /// bytes. Asserted by making BOTH wrong and reading which one is reported:
    /// a corrupt payload under a valid signature names the payload, and the
    /// same corruption unsigned names the signature instead.
    #[test]
    fn authenticity_is_decided_before_a_payload_is_hashed() {
        let fixture = Fixture::new();
        let initial = fixture.valid_deployment();
        fixture.selector("current", &initial);
        fixture.selector("previous", &initial);
        let directory = fixture.root.join(protocol::DEPLOYMENTS_DIR).join(&initial);
        fs::write(directory.join("bzImage"), b"tampered\n").unwrap();

        let signed = select_boot_deployment(&fixture.root, &fixture.key())
            .err()
            .unwrap()
            .to_string();
        fs::remove_file(directory.join(protocol::MANIFEST_SIG_NAME)).unwrap();
        let unsigned = select_boot_deployment(&fixture.root, &fixture.key())
            .err()
            .unwrap()
            .to_string();

        assert!(signed.contains("bzImage"), "payload failure: {signed}");
        assert!(
            !signed.contains("manifest.sig"),
            "a signed deployment must not be reported as unsigned: {signed}"
        );
        assert!(
            unsigned.contains("carries no manifest.sig") && !unsigned.contains("bzImage"),
            "the signature must be decided first: {unsigned}"
        );
    }

    /// The id/manifest binding, which is what makes ONE read enough.
    ///
    /// A signature vouches for manifest BYTES; the selector names a deployment
    /// by ID. Requiring the bytes to hash to the id is the join between the
    /// two, and without it a deployment directory could serve a validly signed
    /// manifest while its id — the thing handed to `root-loop` past the kexec —
    /// named something else entirely.
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

        let refused = select_boot_deployment(&fixture.root, &fixture.key())
            .err()
            .unwrap();

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
        install_deployment(&fixture.root, &source, None).unwrap();
        assert_ne!(first, second, "the two fixtures must differ to tell slots apart");

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

    /// EVERY committed signature verifies over the manifest the fixture builds
    /// for its tag.
    ///
    /// The doc on `fixture::SIGNATURES` says a changed payload constant is
    /// caught because no signature verifies afterwards. That is only true of
    /// the tags some test happens to use — the rest would go stale in the tree
    /// with nothing red, which is the maintenance trap the doc claims not to
    /// be. So the table is checked as a whole, and against the SAME
    /// `fixture_manifest` the fixture writes, so the two cannot drift.
    ///
    /// td-boot cannot sign, but it can verify, so this needs nothing new.
    #[test]
    fn every_committed_fixture_signature_verifies_over_its_own_manifest() {
        let key: TrustRoot = decode_hex(FIXTURE_PUBLIC_KEY.as_bytes(), "fixture key").unwrap();
        let other: TrustRoot =
            decode_hex(FIXTURE_OTHER_PUBLIC_KEY.as_bytes(), "other key").unwrap();
        assert_ne!(key, other, "the negative control must be a different key");

        for (tag, hex) in FIXTURE_SIGNATURES {
            let manifest = fixture_manifest(tag);
            let signature: [u8; ed25519::SIGNATURE_LEN] =
                decode_hex(hex.as_bytes(), "fixture signature").unwrap();
            assert!(
                ed25519::verify(&key, manifest.as_bytes(), &signature),
                "the committed signature for {tag:?} does not verify — regenerate \
                 the whole set, see td-boot/src/fixture.rs"
            );
            assert!(
                !ed25519::verify(&other, manifest.as_bytes(), &signature),
                "{tag:?} verifies under the wrong key too, so it proves nothing"
            );
        }
    }

    /// Re-signing a LIVE deployment with a signature nobody can check here
    /// really does take its bootability away — pinned as a test because the
    /// boot flip is what created the hazard, and a hazard nothing exercises is
    /// one the next change can quietly widen.
    ///
    /// D3 wants the re-sign itself to succeed (a key rotation must reach a
    /// machine that already has the deployment) and `install` cannot check the
    /// new signature, since per §6 the real root has no trust root. So the
    /// install is allowed, `warn_unverifiable_resign` reports it, and this is
    /// what the operator was warned about. The real fix is a trust root on the
    /// installing rootfs, which is §10 item 7's.
    #[test]
    fn resigning_a_live_deployment_with_an_uncheckable_signature_costs_it_the_boot() {
        let fixture = Fixture::new();
        let initial = fixture.valid_deployment();
        fixture.selector("current", &initial);
        fixture.selector("previous", &initial);
        let (source, candidate) = fixture.source_bundle("candidate", "next");
        install_deployment(&fixture.root, &source, None).unwrap();
        assert_eq!(
            select_boot_deployment(&fixture.root, &fixture.key())
                .unwrap()
                .deployment_id,
            candidate
        );

        // A well-formed signature under nobody's key: 128 hex characters, so it
        // is the SIGNATURE that fails rather than the file being malformed.
        fixture.sign_source(&source, &"ab".repeat(ed25519::SIGNATURE_LEN));
        install_deployment(&fixture.root, &source, None).unwrap();

        assert_eq!(
            slots_pointing_at(&fixture.root, &candidate),
            vec!["current"],
            "the warning names what it selects"
        );
        // ROLLBACK absorbs it while `previous` is a different, still-authentic
        // deployment — the fail-closed refusal of `current` is exactly what the
        // fallback exists for, so the machine still boots.
        let decision = select_boot_deployment(&fixture.root, &fixture.key()).unwrap();
        assert_eq!(decision.slot, "previous");
        assert_eq!(decision.deployment_id, initial);
        assert!(
            decision
                .current_error
                .as_deref()
                .is_some_and(|error| error.contains("signature")),
            "the rollback must say the signature is why: {:?}",
            decision.current_error
        );

        // What has no fallback is the case where both slots are the SAME
        // deployment, which is the state a freshly installed machine is in.
        // Then the re-sign really does cost the machine its boot, and nothing
        // before the next power-on says so.
        fs::remove_file(fixture.root.join(protocol::BOOT_DIR).join("previous")).unwrap();
        fixture.selector("previous", &candidate);
        fixture.selector_replace("current", &candidate);
        let refused = select_boot_deployment(&fixture.root, &fixture.key())
            .err()
            .unwrap();
        assert!(
            refused.to_string().contains("signature"),
            "with no distinct fallback the machine stops booting: {refused}"
        );
    }

    /// The warning fires on exactly the slots that select the deployment, and
    /// a volume with no `previous` is not an error — that is a freshly
    /// installed one.
    #[test]
    fn a_resign_names_every_slot_that_selects_the_deployment() {
        let fixture = Fixture::new();
        let initial = fixture.valid_deployment();

        assert!(slots_pointing_at(&fixture.root, &initial).is_empty());
        fixture.selector("current", &initial);
        assert_eq!(slots_pointing_at(&fixture.root, &initial), vec!["current"]);
        fixture.selector("previous", &initial);
        assert_eq!(
            slots_pointing_at(&fixture.root, &initial),
            vec!["current", "previous"]
        );
        assert!(
            slots_pointing_at(&fixture.root, "not-a-deployment").is_empty(),
            "a slot selecting something else is not this deployment"
        );
        assert!(warn_unverifiable_resign(&fixture.root, &initial).is_ok());
    }

    /// A re-sign under a TRUST ROOT does not warn that nothing can check it,
    /// because something did.
    ///
    /// The warning's whole job is to make an operator look at a machine that
    /// may not come back; a false one spends that. Asserted against the source
    /// as well as run, because the branch is a `writeln!` to stderr that a test
    /// in this process cannot observe — and asserted BOTH ways because the
    /// source check alone would pass on a body that never reached the guard,
    /// while the run alone would pass on one that warned every time.
    #[test]
    fn an_authenticated_resign_does_not_warn_that_nothing_checked_it() {
        let fixture = Fixture::new();
        let initial = fixture.valid_deployment();
        fixture.selector("current", &initial);
        // The same tag `valid_deployment` uses, so this really is the live
        // deployment being published again rather than a different one.
        let (source, id) = fixture.source_bundle("resigned", "");
        assert_eq!(id, initial, "the re-sign is of the LIVE deployment");

        // Publishing the same deployment again, authenticated, is the resign
        // path and must succeed.
        assert_eq!(
            install_deployment(&fixture.root, &source, Some(&fixture.key())).unwrap(),
            initial
        );

        let body = include_str!("main.rs");
        let guarded = body
            .split("Existing::Resigned(signature) =>")
            .nth(1)
            .unwrap_or_default();
        let arm = guarded.split("Existing::SignatureWithdrawn").next().unwrap_or_default();
        assert!(
            arm.contains("if !authenticated"),
            "the resign warning must be guarded by whether anything checked it"
        );
        assert!(
            arm.contains("let _ = warn_unverifiable_resign"),
            "the warning must not fail a publish whose signature is already replaced"
        );
    }

    /// Nothing `run_boot` hands to kexec was read by an unauthenticated
    /// reader.
    ///
    /// A property of the SOURCE because `run_boot` needs a block device, a
    /// mount and a working kexec, and no unit test has any of the three — so
    /// its four exit paths cannot be driven here at all. Each of them ends in
    /// `kexec_boot_decision`, and what must hold is that the `Deployment` each
    /// one passes came from an authenticating reader: `read_only_current`,
    /// `read_only_recovery` or `authenticated_deployment`. The unauthenticated
    /// `verify_slot`/`verify_deployment` pair exists for `install` and
    /// `verify`, which run on the real root and have no trust root to read,
    /// and naming either inside this function would be the fail-open D2
    /// forbids.
    #[test]
    fn run_boot_reaches_kexec_through_authenticating_readers_only() {
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
        for unauthenticated in ["verify_slot(", "verify_deployment(", "verified_manifest("] {
            assert!(
                !body.contains(unauthenticated),
                "run_boot names {unauthenticated}: every deployment it kexecs must \
                 come from an authenticating reader"
            );
        }
        // The WHOLE call, not just the name. `read_boot_trust_root(mountpoint)`
        // reads the trust root off the Btrfs volume — the surface §6's first
        // bullet rules out by name, since anyone who can write a forged
        // deployment can write the matching public key beside it — and it
        // satisfies every other assertion in this file, this one included if it
        // only looked for the function's name.
        assert!(
            body.contains("read_boot_trust_root(Path::new(BOOT_ROOTFS))"),
            "run_boot must read the trust root from its OWN rootfs"
        );
        assert!(
            !body.contains("read_boot_trust_root(mountpoint"),
            "the trust root must not come from the volume being selected from"
        );
    }

    /// A missing trust root is a hard failure of the whole selection rather
    /// than something to fall back from — see `read_boot_trust_root`. It is
    /// also the ONLY report that a mis-provisioned key never arrived, because
    /// per DESIGN.md §6 the kernel says nothing about an appendix it could not
    /// parse.
    #[test]
    fn a_rootfs_without_a_trust_root_cannot_boot_anything() {
        let fixture = Fixture::new();
        let refused = read_boot_trust_root(&fixture.root).err().unwrap();

        assert!(
            refused.to_string().contains("no usable trust root"),
            "the failure must name the missing key: {refused}"
        );

        let key_path = fixture.root.join(protocol::TRUSTED_KEY_PATH);
        fs::create_dir_all(key_path.parent().unwrap()).unwrap();
        fs::write(&key_path, format!("{FIXTURE_PUBLIC_KEY}\n")).unwrap();
        assert_eq!(read_boot_trust_root(&fixture.root).unwrap(), fixture.key());
    }

    /// What production passes for that rootfs, stated as a literal. The
    /// parameter exists for the test above; this is what pins the real one.
    #[test]
    fn the_boot_trust_root_is_read_from_the_running_rootfs() {
        assert_eq!(BOOT_ROOTFS, "/");
        assert_eq!(
            Path::new(BOOT_ROOTFS).join(protocol::TRUSTED_KEY_PATH),
            Path::new("/etc/td/deployment.pub")
        );
    }

    #[test]
    fn read_only_fast_path_accepts_only_successful_current() {
        let fixture = Fixture::new();
        let initial = fixture.valid_deployment();
        fixture.selector("current", &initial);
        fixture.selector("previous", &initial);
        assert_eq!(
            read_only_current(&fixture.root, &fixture.key()).map(|(decision, _)| decision.deployment_id),
            Some(initial.clone())
        );

        let (source, candidate) = fixture.source_bundle("candidate", "next");
        install_deployment(&fixture.root, &source, None).unwrap();

        assert!(read_only_current(&fixture.root, &fixture.key()).is_none());
        assert_eq!(read_selector(&fixture.root, "current").unwrap(), candidate);
    }

    #[test]
    fn read_only_recovery_prefers_previous_but_keeps_a_verified_current_available() {
        let fixture = Fixture::new();
        let initial = fixture.valid_deployment();
        fixture.selector("current", &initial);
        fixture.selector("previous", &initial);
        let (source, candidate) = fixture.source_bundle("candidate", "next");
        install_deployment(&fixture.root, &source, None).unwrap();
        let transaction_error = io::Error::other("read-write mount rejected");

        let (decision, _) = read_only_recovery(&fixture.root, &transaction_error, &fixture.key()).unwrap();

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
        let (decision, _) = read_only_recovery(&fixture.root, &transaction_error, &fixture.key()).unwrap();
        assert_eq!(decision.slot, "current");
        assert_eq!(decision.deployment_id, candidate);
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
        install_deployment(&fixture.root, &source, None).unwrap();
        fs::write(
            fixture
                .root
                .join("td/deployments")
                .join(candidate)
                .join("root.erofs"),
            b"tampered\n",
        )
        .unwrap();

        let selected = select_boot_deployment(&fixture.root, &fixture.key()).unwrap();

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
        let stacked = b"36 25 0:35 / /run/td\\040update rw,nodev,nosuid,noexec - btrfs /dev/vda rw\n\
37 25 0:36 / /run/td\\040update rw,nodev,nosuid,noexec - btrfs /dev/vda rw\n";
        assert!(mounted_btrfs_source(stacked, mountpoint).is_err());
        assert!(decode_mountinfo_field(b"/run/bad\\09x").is_err());
    }

    #[test]
    fn writable_failures_preserve_transaction_outcomes() {
        let operation = finish_writable_operation::<()>(
            Err(invalid("invalid attempt state")),
            Ok(()),
        );
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

    // ---- authenticity: the ed25519 half ----
    //
    // td-boot cannot SIGN — it has the verifier and deliberately not the signer
    // — so the positive case is a committed triple generated once by
    // `td-deploy`: a manifest, its detached signature, and the public half of
    // the key that made it. The private half was never committed and does not
    // exist any more, which is exactly why the triple is committed rather than
    // regenerated. See `td-boot/tests/README`.
    //
    // The negatives need no signer at all, and they are the ones that matter:
    // D2 is fail-closed, so what has to be true is that everything which is not
    // a good signature is refused.

    const FIXTURE_MANIFEST: &[u8] = include_bytes!("../tests/deployment.manifest");
    const FIXTURE_SIGNATURE: &[u8] = include_bytes!("../tests/deployment.manifest.sig");
    const FIXTURE_KEY: &[u8] = include_bytes!("../tests/deployment.pub");
    const FIXTURE_OTHER_KEY: &[u8] = include_bytes!("../tests/deployment-other.pub");

    fn fixture_key() -> [u8; ed25519::PUBLIC_KEY_LEN] {
        decode_hex(FIXTURE_KEY, "fixture key").expect("the committed key is 64 hex characters")
    }

    #[test]
    fn the_committed_signature_authenticates_its_manifest() {
        assert!(
            authenticate_manifest(FIXTURE_MANIFEST, FIXTURE_SIGNATURE, &fixture_key()).is_ok(),
            "the committed triple must agree, or every negative below proves nothing"
        );
        // And the triple is a DEPLOYMENT, not just bytes: the manifest carries
        // the header the protocol requires and parses, so this is the same
        // shape a machine would be asked to boot.
        assert!(FIXTURE_MANIFEST.starts_with(protocol::MANIFEST_HEADER));
        assert!(parse_manifest(FIXTURE_MANIFEST).is_ok());
    }

    #[test]
    fn a_signature_under_another_key_is_refused() {
        let other: [u8; ed25519::PUBLIC_KEY_LEN] =
            decode_hex(FIXTURE_OTHER_KEY, "other key").expect("64 hex characters");
        assert_ne!(other, fixture_key(), "the two fixture keys must differ");
        assert!(authenticate_manifest(FIXTURE_MANIFEST, FIXTURE_SIGNATURE, &other).is_err());
    }

    #[test]
    fn a_tampered_manifest_is_refused() {
        // Every byte in turn, so this cannot pass by tampering somewhere the
        // signature does not cover — there is no such place, and that is the
        // claim being made.
        for i in 0..FIXTURE_MANIFEST.len() {
            let mut tampered = FIXTURE_MANIFEST.to_vec();
            if let Some(byte) = tampered.get_mut(i) {
                *byte ^= 1;
            }
            assert!(
                authenticate_manifest(&tampered, FIXTURE_SIGNATURE, &fixture_key()).is_err(),
                "manifest byte {i} must not be free to move"
            );
        }
    }

    #[test]
    fn every_bit_of_the_signature_matters() {
        let good: [u8; ed25519::SIGNATURE_LEN] =
            decode_hex(FIXTURE_SIGNATURE, "signature").expect("128 hex characters");
        for byte in 0..ed25519::SIGNATURE_LEN {
            for bit in 0..8u32 {
                let mut broken = good;
                if let Some(slot) = broken.get_mut(byte) {
                    *slot ^= 1u8 << bit;
                }
                let hex = broken.iter().fold(String::new(), |mut s, b| {
                    s.push_str(&format!("{b:02x}"));
                    s
                });
                assert!(
                    authenticate_manifest(FIXTURE_MANIFEST, hex.as_bytes(), &fixture_key())
                        .is_err(),
                    "signature byte {byte} bit {bit} must not be free to move"
                );
            }
        }
    }

    /// A signature file is read off a volume anyone who can write the disk can
    /// write, so every malformed shape must be a refusal and NONE of them may
    /// be fatal. The multi-byte cases are the ones that matter: the same
    /// decoder written over `&str` panicked on exactly these.
    #[test]
    fn a_malformed_signature_is_refused_and_never_fatal() {
        let key = fixture_key();
        let good = String::from_utf8_lossy(FIXTURE_SIGNATURE).trim().to_string();
        let mut cases: Vec<Vec<u8>> = vec![
            Vec::new(),
            b"\n".to_vec(),
            b"zz".to_vec(),
            b"+f".to_vec(),
            good.as_bytes().get(..127).unwrap_or_default().to_vec(),
            format!("{good}00").into_bytes(),
            format!("{good}x").into_bytes(),
            "\u{20ac}".repeat(64).into_bytes(),
            format!("{}\u{00e9}", good.get(..126).unwrap_or_default()).into_bytes(),
            // Exactly 128 BYTES with a two-byte character STRADDLING an even
            // offset (1 + 2 + 125), so it survives the length check and lands
            // mid-character in the pair loop. That is the only shape which
            // actually reaches the panic a `&str`-slicing decoder had — the
            // two cases above are refused for their length before they get
            // there, which a verify-red probe caught. Without this the "never
            // fatal" in this test's name would be untested for signatures.
            format!("a\u{00e9}{}", "0".repeat(125)).into_bytes(),
        ];
        // A signature of the right LENGTH but entirely absent structure.
        cases.push(vec![b'0'; 128]);
        for case in cases {
            assert!(
                authenticate_manifest(FIXTURE_MANIFEST, &case, &key).is_err(),
                "malformed signature {:?} must be refused",
                String::from_utf8_lossy(&case)
            );
        }
    }

    #[test]
    fn a_signature_may_be_written_with_or_without_a_trailing_newline() {
        let key = fixture_key();
        let trimmed = String::from_utf8_lossy(FIXTURE_SIGNATURE).trim().to_string();
        for spelling in [
            trimmed.clone(),
            format!("{trimmed}\n"),
            format!("{trimmed}\r\n"),
            format!("  {trimmed}  "),
            trimmed.to_uppercase(),
        ] {
            assert!(
                authenticate_manifest(FIXTURE_MANIFEST, spelling.as_bytes(), &key).is_ok(),
                "{spelling:?} is the same signature"
            );
        }
    }

    #[test]
    fn the_trusted_key_is_read_from_a_file_and_malformed_ones_are_refused() {
        let fixture = Fixture::new();
        let path = fixture.root.join("deployment.pub");

        fs::write(&path, FIXTURE_KEY).unwrap();
        assert_eq!(read_trusted_key(&path).unwrap(), fixture_key());

        // Absent is an error, never a default key and never "unsigned": §6
        // names the fail-open branch as the tempting one, so it is the one
        // pinned here.
        let missing = fixture.root.join("absent.pub");
        assert!(read_trusted_key(&missing).is_err());

        for bad in [
            &b""[..],
            &b"not hex at all, not even close\n"[..],
            &b"00\n"[..],
            &[0xffu8; 64][..],
        ] {
            fs::write(&path, bad).unwrap();
            assert!(
                read_trusted_key(&path).is_err(),
                "{:?} is not a key",
                String::from_utf8_lossy(bad)
            );
        }

        // And a file too large to be a key is refused BY THE BOUND rather than
        // read, which is what stops a "key" being a payload. The error text is
        // what distinguishes the two: a huge file is refused for its length by
        // the decoder as well, so an `is_err()` here passes with the bound
        // removed entirely — which a verify-red probe caught it doing.
        fs::write(&path, vec![b'a'; 4096]).unwrap();
        let refused = read_trusted_key(&path).unwrap_err().to_string();
        // The whole phrase, not `contains("96")` beside it: the message embeds
        // the fixture path, which carries a pid, and a pid containing 96 would
        // satisfy a bare digit check whatever the bound was.
        assert!(
            refused.contains(&format!("exceeds {} bytes", protocol::MAX_PUBLIC_KEY_BYTES)),
            "a key must be refused by its bound before it is read, got {refused:?}"
        );
        assert_eq!(protocol::MAX_PUBLIC_KEY_BYTES, 96, "the bound this pins");
    }

    /// A directory or a symlink where the key should be is a refusal, not a
    /// read — the same rule every other file td-boot opens off a volume obeys.
    #[test]
    fn a_trusted_key_that_is_not_a_real_file_is_refused() {
        let fixture = Fixture::new();
        let directory = fixture.root.join("key-dir");
        fs::create_dir(&directory).unwrap();
        assert!(read_trusted_key(&directory).is_err());

        let target = fixture.root.join("real.pub");
        fs::write(&target, FIXTURE_KEY).unwrap();
        let link = fixture.root.join("link.pub");
        symlink(&target, &link).unwrap();
        assert!(read_trusted_key(&link).is_err(), "a symlinked key is refused");
    }

    /// The verb exists so the verifier is REACHABLE, which is not a stylistic
    /// point: rustc drops dead code, so declaring the modules and never calling
    /// them compiles ed25519 and SHA-512 and then discards them. Measured on the
    /// recipe's own layout, the shipped binary was 24576 bytes SMALLER with the
    /// functions unreferenced — so "the verifier is in the target binary" was
    /// false until something called it.
    #[test]
    fn authenticate_accepts_a_signed_deployment_and_refuses_everything_else() {
        let fixture = Fixture::new();
        let directory = fixture.root.join("bundle");
        fs::create_dir(&directory).unwrap();
        fs::write(directory.join(protocol::MANIFEST_NAME), FIXTURE_MANIFEST).unwrap();
        fs::write(
            directory.join(protocol::MANIFEST_SIG_NAME),
            FIXTURE_SIGNATURE,
        )
        .unwrap();
        let key = fixture.root.join("trusted.pub");
        fs::write(&key, FIXTURE_KEY).unwrap();

        assert!(run_authenticate(&directory, &key).is_ok());
        // The id it reports is the deployment id every other verb uses.
        assert_eq!(
            sha256::hex_digest(FIXTURE_MANIFEST),
            "93f336004695ce2d19e1dbb793e2784f8842dd74c29a5cd30e2d782fb18fde3f"
        );

        // The wrong key.
        let other = fixture.root.join("other.pub");
        fs::write(&other, FIXTURE_OTHER_KEY).unwrap();
        assert!(run_authenticate(&directory, &other).is_err());

        // An ABSENT signature is a refusal here, unlike on the publish path.
        fs::remove_file(directory.join(protocol::MANIFEST_SIG_NAME)).unwrap();
        assert!(run_authenticate(&directory, &key).is_err());

        // A signature over a DIFFERENT manifest: the bytes are a real signature,
        // just not of these bytes, which is the substitution the check exists for.
        fs::write(
            directory.join(protocol::MANIFEST_SIG_NAME),
            FIXTURE_SIGNATURE,
        )
        .unwrap();
        let mut tampered = FIXTURE_MANIFEST.to_vec();
        if let Some(byte) = tampered.last_mut() {
            *byte = b'\n';
        }
        if let Some(byte) = tampered.get_mut(20) {
            *byte ^= 1;
        }
        fs::write(directory.join(protocol::MANIFEST_NAME), &tampered).unwrap();
        assert!(run_authenticate(&directory, &key).is_err());

        // And a directory that is not a deployment at all.
        assert!(run_authenticate(&fixture.root.join("absent"), &key).is_err());
    }

    #[test]
    fn authenticate_parses_its_arguments_exactly() {
        let parsed = parse_args(
            [
                OsString::from("authenticate"),
                OsString::from("/bundle"),
                OsString::from("/key.pub"),
            ]
            .into_iter(),
        );
        assert!(matches!(parsed, Ok(Mode::Authenticate { .. })));
        // An explicit key is taken verbatim.
        let Ok(Mode::Authenticate { trusted_key, .. }) = parsed else {
            panic!("authenticate with an explicit key must parse");
        };
        assert_eq!(trusted_key, PathBuf::from("/key.pub"));

        // Omitted, it defaults to the one place a booted td-boot finds a trust
        // root — the absolute form of the constant the harness writes into the
        // selector initramfs. Spelled as a LITERAL: comparing it to the
        // constant would agree with itself however wrong the constant was, and
        // wrong here is a key nothing ever reads.
        let defaulted = parse_args(
            [OsString::from("authenticate"), OsString::from("/bundle")].into_iter(),
        );
        let Ok(Mode::Authenticate { trusted_key, .. }) = defaulted else {
            panic!("authenticate must parse with the key omitted");
        };
        assert_eq!(trusted_key, PathBuf::from("/etc/td/deployment.pub"));
        assert_eq!(protocol::TRUSTED_KEY_PATH, "etc/td/deployment.pub");
        // `CHANNEL_CANDIDATE` has the same hazard and needs the same pin: it is
        // `join`ed onto the channel, and a leading slash would silently discard
        // the channel and point the updater at the filesystem root.
        assert_eq!(protocol::CHANNEL_CANDIDATE, "candidate");
        assert_eq!(protocol::VOLUME_CHANNEL_DIR, "td/incoming");

        // Too few and too many are both usage errors, as every other verb's are.
        assert!(parse_args([OsString::from("authenticate")].into_iter()).is_err());
        assert!(parse_args(
            [
                OsString::from("authenticate"),
                OsString::from("/bundle"),
                OsString::from("/key.pub"),
                OsString::from("extra"),
            ]
            .into_iter()
        )
        .is_err());
    }

    #[test]
    fn decode_hex_is_exact_about_length_and_alphabet() {
        assert_eq!(decode_hex::<2>(b"00ff", "x").unwrap(), [0x00, 0xff]);
        assert_eq!(decode_hex::<2>(b"00FF\n", "x").unwrap(), [0x00, 0xff]);
        assert!(decode_hex::<2>(b"00f", "x").is_err(), "odd length");
        assert!(decode_hex::<2>(b"00ff00", "x").is_err(), "too long");
        assert!(decode_hex::<2>(b"00 ff", "x").is_err(), "inner space");
        assert!(decode_hex::<2>(b"00g0", "x").is_err(), "not hex");
        // `from_str_radix` accepted a sign; a key parser must not.
        assert!(decode_hex::<1>(b"+f", "x").is_err(), "a signed nibble is not hex");
    }
}

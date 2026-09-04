//! Build a self-contained, redistributable td demo VM.
//!
//! `td-recipe-eval run` builds the distro and boots it immediately, which is
//! the right tool for whoever has the checkout — and useless to everyone else,
//! because the build climbs the whole stage0-posix ladder first. This produces
//! the same boot as a DIRECTORY of prebuilt files plus a POSIX-sh launcher, so
//! somebody with qemu and no td checkout can try the system in the time it
//! takes to download it.
//!
//! Sibling of `checks/run.rs`: identical build, identical verification,
//! identical trust root, and — through `checks/vm_profile.rs` — literally the
//! same qemu argv. The difference is only where the boot happens. `run` boots
//! private copies it deletes on exit; this writes the images somewhere durable
//! and renders the launcher instead of spawning qemu at all.
//!
//! What it deliberately is NOT: a distribution channel. Nothing in td's recipe
//! graph names a bundle, no build fetches one, and a bundle that is never
//! published changes nothing about how td is built or updated (AGENTS.md
//! principle 5 — updates are a git pull, not a package download). It is a
//! convenience artifact for people evaluating td, and the shipped README says
//! so in those words.

use std::fs;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::check_runner::RecipeCheckRunner;
use crate::checks::qemu_boot::{
    build_btrfs_tools, create_persistent_volume, find_qemu_tool, provision_selector,
    verify_deployment, verify_selector, RunTrust, VolumePurpose, SYSTEM_GUEST_MEMORY_MIB,
};
use crate::checks::vm_profile::{
    self, Compression, DiskFormat, CHECKSUMS_NAME, INITRD_NAME, KERNEL_NAME, LAUNCHER_NAME,
    README_NAME,
};

/// The distro image recipe a bundle ships; its closure pulls in the kernel.
const SYSTEM: &str = "system-x86-64";

/// Where a bundle goes when the operator does not say. Repo-relative and
/// gitignored: a bundle is host-local build output, like a `target/` tree.
pub(crate) const DEFAULT_OUT: &str = "dist/td-vm-x86-64";

/// Proof that a directory is a bundle THIS tool wrote, and the only thing that
/// licenses `--force` to delete anything in it.
///
/// Recognising a bundle by its file names alone is not safe: `README.md` and
/// `start` are bundle names AND ordinary repository files, so `--force --out .`
/// in a checkout would have deleted the tracked copies of both. A marker the
/// operator could not have written by accident makes replacement provable
/// rather than guessed.
const MARKER_NAME: &str = ".td-bundle";
const MARKER_FORMAT: &str = "td-bundle-v1";

/// What the operator asked for.
#[derive(Debug, Clone)]
pub(crate) struct BundleOptions {
    pub(crate) out: PathBuf,
    /// Skip the qcow2 conversion and ship the 5 GiB raw volume. For a host
    /// with no `qemu-img`, and for anyone who would rather hand out a raw
    /// image than depend on qcow2 at all.
    pub(crate) raw: bool,
    /// Compress with zlib instead of zstd: about 14% larger, readable by any
    /// qemu that reads qcow2. For handing the bundle to someone whose qemu
    /// predates 5.1.
    pub(crate) zlib: bool,
    /// Overwrite a bundle already in `out`.
    pub(crate) force: bool,
}

/// Every file a bundle owns, newest first in the order they are written.
///
/// `--force` deletes exactly these and the volume that is not being written,
/// and NOTHING else: `out` is an operator-supplied path, and a recursive
/// delete of one is a way to lose a home directory to a typo.
fn bundle_files() -> Vec<&'static str> {
    vec![
        KERNEL_NAME,
        INITRD_NAME,
        DiskFormat::Qcow2(Compression::Zstd).file_name(),
        DiskFormat::Raw.file_name(),
        LAUNCHER_NAME,
        README_NAME,
        CHECKSUMS_NAME,
        MARKER_NAME,
    ]
}

/// `lock` is the ladder lock, held across the build and the volume creation —
/// the mkfs.btrfs it execs lives in the ladder store, so releasing earlier
/// would let a concurrent `clear-store` delete the tool mid-run. It is dropped
/// before the qcow2 conversion, which reads only the bundle directory.
pub(crate) fn run(
    runner: &RecipeCheckRunner,
    lock: std::fs::File,
    options: &BundleOptions,
) -> Result<(), String> {
    // Settled before the build: a destination that can never receive a bundle
    // is refused now rather than after the climb. This may CREATE `out` —
    // that is how an uncreatable path is detected — but removes nothing; the
    // clearing waits for `open_out_dir` below.
    ensure_out_dir(runner, options)?;

    println!(
        "   [bundle] building the td distro ({SYSTEM}); its closure pulls in the kernel.\n            \
         An unchanged tree is reused whole and returns at once; a cold tree climbs the\n            \
         whole ladder from stage0 and can take a long time. Per-rung progress streams below.\n"
    );
    let trees = runner.build_and_stage(SYSTEM, &[SYSTEM])?;
    let system_tree = trees
        .first()
        .cloned()
        .ok_or_else(|| format!("distro build did not stage the {SYSTEM} output"))?;
    let deployment = system_tree.join("deployment");

    // Same order as run.rs: verify every payload against its own manifest
    // before anything is copied out of the store.
    let (bzimage, _, _) = verify_deployment(&deployment)?;
    let selector = verify_selector(&system_tree.join("boot"))?;
    let (mkfs, btrfs) = build_btrfs_tools(runner)?;

    // The deployment id is the sha256 of the manifest bytes — the content
    // identity td-boot recomputes at every verify. It is what a release page
    // should name, because it is a claim about the bytes rather than a tag
    // somebody chose.
    let deployment_id = crate::sha256::sha256_file(&deployment.join("manifest"))
        .map_err(|e| format!("hash the deployment manifest: {e}"))?;

    // Everything is BUILT in a private scratch directory and only the finished
    // files are moved into `out`.
    //
    // Not tidiness: `create_persistent_volume` and `provision_selector` create
    // and unconditionally remove `persistent-volume-seed` and
    // `selector-initramfs-trusted.cpio` beside the volume they are given.
    // Pointed at `out`, they would recursively delete anything already at those
    // names — which are not bundle-owned, so `--force` never even had a say.
    // A scratch directory this run owns outright cannot collide with an
    // operator's data.
    let scratch = Scratch::new(runner.ladder_work_dir())?;

    println!("   [bundle] staging boot payloads");
    let staged_kernel = scratch.dir.join(KERNEL_NAME);
    copy_into(&bzimage, &staged_kernel)?;

    // This bundle's trust root. The private half is generated here, used to
    // sign the deployment that goes into the volume, and dropped when this
    // function returns; only the public half ships, inside the selector.
    let trust = RunTrust::generate()?;
    // `provision_selector` names its output for the boot fixture that first
    // needed it. The bundle publishes it under the name its launcher passes to
    // `-initrd`, so rename rather than teach every reader two names for one
    // file. Same directory, so this cannot cross a filesystem.
    let provisioned = provision_selector(&selector, &scratch.dir, &trust)?;
    let staged_initrd = scratch.dir.join(INITRD_NAME);
    fs::rename(&provisioned, &staged_initrd).map_err(|e| {
        format!(
            "name the staged selector {} -> {}: {e}",
            provisioned.display(),
            staged_initrd.display()
        )
    })?;

    println!("   [bundle] creating the persistent Btrfs volume (this writes several GiB)");
    let raw_volume = scratch.dir.join(DiskFormat::Raw.file_name());
    create_persistent_volume(
        &deployment,
        &mkfs,
        &btrfs,
        &raw_volume,
        &trust,
        // Handed to strangers: no oracle scaffolding, nothing about this host.
        VolumePurpose::Published,
    )?;

    // The ladder is no longer needed: everything below reads only the scratch.
    drop(lock);

    let format = if options.raw {
        DiskFormat::Raw
    } else {
        let preferred = if options.zlib {
            Compression::Zlib
        } else {
            Compression::Zstd
        };
        compress_volume(&raw_volume, &scratch.dir, preferred)?
    };

    let staged_disk = scratch.dir.join(format.file_name());
    write_executable(
        &scratch.dir.join(LAUNCHER_NAME),
        vm_profile::launcher_script(format).as_bytes(),
    )?;
    fs::write(scratch.dir.join(README_NAME), readme(&deployment_id, format))
        .map_err(|e| format!("write the bundle README: {e}"))?;
    // Before `write_checksums` because `verify_staged` requires it, NOT because
    // it is checksummed — `checksum_targets` deliberately excludes it, so that
    // `sha256sum -c` succeeds for a downloader who fetched the files the README
    // documents.
    fs::write(
        scratch.dir.join(MARKER_NAME),
        format!("{MARKER_FORMAT}\ndeployment={deployment_id}\n"),
    )
    .map_err(|e| format!("write the bundle marker: {e}"))?;
    verify_staged(&scratch.dir, format)?;
    write_checksums(&scratch.dir, format)?;

    // `out` was created before the build, so that an uncreatable one failed
    // fast. Only now, with every file finished, is it CLEARED and filled —
    // and re-created first if something removed it while the build ran.
    let out = open_out_dir(&options.out, options.force, runner.ladder_work_dir())?;
    for name in published_names(format) {
        move_into_place(&scratch.dir.join(name), &out.join(name))?;
    }
    let _ = (staged_kernel, staged_initrd, staged_disk);

    report(&out, format, &deployment_id)
}

/// Settle the destination: refuse one that cannot receive a bundle, and create
/// it if it does not exist yet. REMOVES NOTHING.
///
/// The split from `open_out_dir` is about deletion, not about touching the
/// filesystem at all. `bundle_cli` calls this before the warm so a bad `--out`
/// costs microseconds instead of a fetch and a full climb, and answering that
/// question honestly means attempting the creation: a path under a file, or
/// under a parent that does not exist and cannot be made, is only knowable by
/// trying. It is not a full precondition check — an `--out` whose parent is
/// writable succeeds here and can still fail later on space.
/// What must NOT happen early is the clearing — that would destroy a perfectly
/// good previous bundle and then spend an hour discovering the replacement
/// cannot be built, leaving the operator with neither. So: create here, clear
/// in `open_out_dir`, immediately before the finished files are moved in. The
/// cost is an empty directory left behind by a later failure.
pub(crate) fn ensure_out_dir(
    runner: &RecipeCheckRunner,
    options: &BundleOptions,
) -> Result<(), String> {
    ensure_out_dir_under(&options.out, options.force, runner.ladder_work_dir())
}

/// `ensure_out_dir` with the ladder path passed in rather than read off a
/// runner, so the guards are reachable from a test.
fn ensure_out_dir_under(
    out: &Path,
    force: bool,
    ladder_work_dir: &Path,
) -> Result<(), String> {
    // A bundle inside the ladder work tree is deleted by the next
    // `clear-store`, which is a surprising way to lose a finished release
    // artifact. The same guard `run.rs` puts on its private image dir, for the
    // same reason and with a longer-lived victim.
    // Both operands are spelled out rather than pattern-matched away. The
    // earlier `if let (Ok, Ok)` skipped the guard in silence whenever EITHER
    // side failed, and fixing only `canonical_parent` left the same hole on
    // the ladder side. That hole is not reachable today — `RecipeCheckRunner::
    // new` creates the ladder tree before this runs — but "unreachable" is a
    // property of a caller, not of this function, and a silent skip is the
    // wrong shape for a guard regardless. An absent ladder cannot contain
    // anything, so it is a pass, not a skip; a ladder that exists and cannot
    // be resolved is a refusal.
    let candidate = canonical_parent(out)?;
    let ladder = match ladder_work_dir.canonicalize() {
        Ok(ladder) => Some(ladder),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(format!(
                "resolve the ladder work dir {}: {error}",
                ladder_work_dir.display()
            ))
        }
    };
    if let Some(ladder) = ladder {
        if candidate == ladder || candidate.starts_with(&ladder) {
            return Err(format!(
                "{} is inside the ladder work tree ({}); a `clear-store` would delete the \
                 bundle. Choose an --out outside it.",
                out.display(),
                ladder.display()
            ));
        }
    }

    if out.is_dir() {
        // `symlink_metadata`, not `exists`: a BROKEN symlink at a bundle name
        // is invisible to `exists()`, so it was never listed, never cleared,
        // and `move_into_place`'s copy fallback would then follow it and write
        // the bundle's bytes to whatever it pointed at, outside `out`.
        let existing: Vec<&str> = bundle_files()
            .into_iter()
            .filter(|name| out.join(name).symlink_metadata().is_ok())
            .collect();
        if !existing.is_empty() {
            // A directory is replaceable only if THIS tool wrote it. Bundle
            // file names are ordinary names — `README.md` and `start` are both
            // repository files — so name collisions alone must never license a
            // delete: `--force --out .` in a checkout would otherwise remove
            // the tracked copies of both.
            if !is_bundle_directory(out) {
                return Err(format!(
                    "{} holds files a bundle would overwrite ({}) but no {MARKER_NAME} \
                     marker, so it was not written by `bundle` and will not be \
                     replaced — even with --force. Choose an empty --out DIR.",
                    out.display(),
                    existing.join(", ")
                ));
            }
            if !force {
                return Err(format!(
                    "{} already holds a bundle; pass --force to replace it, or \
                     --out DIR to write elsewhere",
                    out.display()
                ));
            }
        }
    } else if out.exists() {
        return Err(format!(
            "{} exists and is not a directory",
            out.display()
        ));
    } else {
        // CREATED here, not in `open_out_dir`. Only the clearing had to move
        // late; creating is not destructive, and it is what makes this check
        // worth running early. Without it, `--out /mnt/usb/td-vm` with nothing
        // mounted, or a directory the operator cannot write, returned Ok here
        // and failed at `create_dir_all` after the ladder climb and the
        // several-GiB volume build — the precise failure this ordering exists
        // to prevent. An empty directory left behind by a later failure is a
        // far smaller cost than the hours.
        fs::create_dir_all(out)
            .map_err(|e| format!("create bundle directory {}: {e}", out.display()))?;
    }
    Ok(())
}

/// Re-check the destination, then create it and clear the bundle files it
/// already holds, returning its canonical path.
///
/// Called once, at publication time, so the window between "the old bundle is
/// gone" and "the new one is complete" is the move loop rather than the whole
/// build. Re-checks rather than trusting the earlier `ensure_out_dir`, because
/// the destination is operator-supplied and an hour has passed.
fn open_out_dir(
    out: &Path,
    force: bool,
    ladder_work_dir: &Path,
) -> Result<PathBuf, String> {
    // Re-settles rather than trusting the earlier call: the destination is
    // operator-supplied and the build took hours, so it may have become a
    // file, filled with somebody else's data, or been removed — the last of
    // which this re-creates. NOT because the ladder guard needed `setup()`
    // first: `RecipeCheckRunner::new` creates the ladder tree before the
    // earlier call, so that guard was already live.
    ensure_out_dir_under(out, force, ladder_work_dir)?;
    for name in bundle_files() {
        let path = out.join(name);
        // `remove_file` unlinks the link itself rather than its target, which
        // is what a symlink planted at a bundle name deserves.
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("replace {}: {error}", path.display())),
        }
    }
    out.canonicalize()
        .map_err(|e| format!("resolve bundle directory {}: {e}", out.display()))
}

/// Whether `directory` carries this tool's marker, and so may be replaced.
///
/// Read rather than merely stat'd: an empty or foreign file at that name is not
/// this tool's marker, and the whole point is that an operator cannot produce
/// one by accident.
fn is_bundle_directory(directory: &Path) -> bool {
    fs::read_to_string(directory.join(MARKER_NAME))
        .is_ok_and(|text| text.lines().next() == Some(MARKER_FORMAT))
}

/// A private directory this run owns outright, removed when it drops.
///
/// Modelled on `run.rs`'s `TempImages`, and created the same way and for the
/// same reasons: `DirBuilder::mode(0o700).create` establishes owner-only
/// permissions in the `mkdir` syscall itself, so no umask can widen it and no
/// create-then-chmod window exists; `create` fails rather than reusing an
/// existing path, so a local attacker cannot pre-plant a directory or symlink
/// and have image bytes copied through it.
struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    fn new(ladder_work_dir: &Path) -> Result<Self, String> {
        let base = std::env::temp_dir();
        // The same fail-closed guard `run.rs` puts on its staging directory: a
        // TMPDIR inside the ladder tree would be wiped by a concurrent
        // `clear-store` mid-build.
        //
        // Spelled out rather than pattern-matched for the reason given in
        // `ensure_out_dir_under`: an `if let (Ok, Ok)` skips the guard in
        // silence when either side fails, and one such skip in this file was
        // already a finding.
        let canonical_base = base.canonicalize().map_err(|e| {
            format!("resolve the system temp dir {}: {e}", base.display())
        })?;
        let ladder = match ladder_work_dir.canonicalize() {
            Ok(ladder) => Some(ladder),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(format!(
                    "resolve the ladder work dir {}: {error}",
                    ladder_work_dir.display()
                ))
            }
        };
        if let Some(ladder) = ladder {
            if canonical_base == ladder || canonical_base.starts_with(&ladder) {
                return Err(format!(
                    "the system temp dir ({}) is inside the ladder work tree ({}); a \
                     concurrent ladder wipe could delete the bundle mid-build. Set TMPDIR \
                     to a directory outside the ladder and retry.",
                    base.display(),
                    ladder.display()
                ));
            }
        }
        let pid = std::process::id();
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        for attempt in 0..1024u32 {
            let dir = base.join(format!("td-bundle-{pid}-{seed}-{attempt}"));
            match fs::DirBuilder::new().mode(0o700).create(&dir) {
                Ok(()) => return Ok(Self { dir }),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(format!("create bundle scratch {}: {e}", dir.display())),
            }
        }
        Err("could not create a private bundle scratch directory after 1024 attempts".to_string())
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// Move a finished file from the scratch into the bundle.
///
/// `rename` first, then copy-and-remove: the scratch is under `TMPDIR` and the
/// destination is wherever the operator asked for, and those are routinely
/// different filesystems, where `rename` is `EXDEV` rather than an error worth
/// reporting.
fn move_into_place(from: &Path, to: &Path) -> Result<(), String> {
    /// The rename-across-filesystems refusal — the same number on Linux and
    /// macOS, and the reason this function has a copy fallback at all: the
    /// scratch lives under `TMPDIR` and `--out` can be anywhere.
    const EXDEV: i32 = 18;

    match fs::rename(from, to) {
        Ok(()) => return Ok(()),
        Err(error) if error.raw_os_error() == Some(EXDEV) => {}
        Err(error) => {
            return Err(format!(
                "move {} -> {}: {error}",
                from.display(),
                to.display()
            ))
        }
    }
    // Unlink `to` first: `fs::copy` FOLLOWS a symlink at the destination, so a
    // link planted at a bundle name would take the image bytes outside `out`.
    // `open_out_dir` already cleared these names, so this is the second lock on
    // the same door rather than the only one — but the copy path is the common
    // one here and the cost is a syscall.
    match fs::remove_file(to) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("clear {}: {error}", to.display())),
    }
    fs::copy(from, to)
        .map_err(|e| format!("copy {} -> {}: {e}", from.display(), to.display()))?;
    fs::remove_file(from).map_err(|e| format!("remove staged {}: {e}", from.display()))
}

/// The nearest existing ancestor of `path`, canonicalised.
///
/// `path` itself usually does not exist yet, and `canonicalize` on a missing
/// path fails — which would silently skip the containment check above.
///
/// Made ABSOLUTE first, and that is the whole correctness of it. A relative
/// path walks to a bare first component whose `parent()` is `Some("")` rather
/// than `None`, so the loop used to bottom out on the empty path and return an
/// error. `ensure_out_dir_under` reads that error as "cannot compare" and proceeds,
/// so the ladder guard was skipped for every relative `--out` whose leading
/// component did not exist yet — including `DEFAULT_OUT` on the very first run,
/// which is the common case rather than a corner. Joining onto the working
/// directory makes the walk terminate at `/`, which always canonicalises.
fn canonical_parent(path: &Path) -> Result<PathBuf, String> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|e| format!("current dir: {e}"))?
            .join(path)
    };
    let mut at = absolute.as_path();
    loop {
        if let Ok(resolved) = at.canonicalize() {
            return Ok(resolved);
        }
        at = at
            .parent()
            .ok_or_else(|| format!("no existing ancestor of {}", path.display()))?;
    }
}

/// Convert the raw volume to qcow2 and remove the raw one, returning the
/// format the launcher must be rendered for.
///
/// A missing `qemu-img` is NOT an error: it is a qemu install without one
/// optional tool, and a raw bundle boots identically — just larger. Report the
/// fallback rather than refusing, because the operator asked for a bundle and
/// this still is one.
fn compress_volume(
    raw: &Path,
    out: &Path,
    preferred: Compression,
) -> Result<DiskFormat, String> {
    let Some(qemu_img) = find_qemu_tool("qemu-img") else {
        println!(
            "   [bundle] qemu-img not found; shipping the raw {} volume. It boots the same \
             and compresses well in transit, but the download is much larger.",
            DiskFormat::Raw.file_name()
        );
        return Ok(DiskFormat::Raw);
    };

    let compression = match convert_once(&qemu_img, raw, out, preferred) {
        Ok(()) => preferred,
        // `--zlib` asked for zlib explicitly: there is nothing to fall back to,
        // so the error is the answer.
        Err(error) if preferred != Compression::Zstd => return Err(error),
        // Retried, not diagnosed. The failure this fallback exists for is a
        // qemu-img too old to WRITE zstd, and falling back keeps a bundle
        // possible on such a host instead of ending the run after the whole
        // build. But a full disk, an unreadable raw volume and an out-of-memory
        // kill arrive here identically, and naming the old-qemu-img cause would
        // send an operator to check a version that is fine. So report what
        // actually happened, say what is being tried, and if zlib fails too,
        // return BOTH errors — the zstd one is usually the informative half,
        // and it was previously discarded.
        Err(zstd_error) => {
            println!(
                "   [bundle] {qemu_img} could not write a zstd-compressed qcow2: \
                 {zstd_error}\n            \
                 Retrying with zlib, which every qemu reads — a qemu-img older than \
                 {} cannot write zstd, and that is the usual reason. The bundle will \
                 be roughly 14% larger. Any other cause will fail the retry too.",
                DiskFormat::Qcow2(Compression::Zstd).minimum_qemu()
            );
            convert_once(&qemu_img, raw, out, Compression::Zlib).map_err(|zlib_error| {
                format!(
                    "compress the volume: zstd failed ({zstd_error}) and the zlib \
                     fallback failed too ({zlib_error}). Pass --raw to ship the \
                     uncompressed volume instead."
                )
            })?;
            Compression::Zlib
        }
    };
    fs::remove_file(raw)
        .map_err(|e| format!("remove the raw volume {}: {e}", raw.display()))?;
    Ok(DiskFormat::Qcow2(compression))
}

/// One `qemu-img convert` into the bundle's qcow2 path.
///
/// The destination is removed first: a failed convert leaves a partial file
/// behind, and `qemu-img` would refuse the retry rather than overwrite it.
fn convert_once(
    qemu_img: &str,
    raw: &Path,
    out: &Path,
    compression: Compression,
) -> Result<(), String> {
    let qcow2 = out.join(DiskFormat::Qcow2(compression).file_name());
    match fs::remove_file(&qcow2) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(format!(
                "remove partial volume {}: {error}",
                qcow2.display()
            ))
        }
    }
    println!(
        "   [bundle] converting the volume to {} qcow2 with {qemu_img}",
        match compression {
            Compression::Zstd => "zstd-compressed",
            Compression::Zlib => "zlib-compressed",
        }
    );
    // `-c` compresses each cluster. The guest never writes to the shipped image
    // — the launcher boots it with `snapshot=on` — so a read-mostly compressed
    // image costs nothing at run time and cuts the download by two thirds.
    let status = Command::new(qemu_img)
        .args(["convert", "-c", "-O", "qcow2", "-o"])
        .arg(compression.qemu_img_options())
        .arg(raw)
        .arg(&qcow2)
        .status()
        .map_err(|e| format!("spawn {qemu_img}: {e}"))?;
    if status.success() {
        return Ok(());
    }
    // A failed convert leaves a partial image behind. The retry path removes it
    // on the way in, but a HARD failure would otherwise leave a truncated
    // `td-system.qcow2` in the bundle directory — which the next run then
    // refuses without `--force`, and which an operator could mistake for a
    // finished artifact.
    let _ = fs::remove_file(&qcow2);
    Err(format!(
        "{qemu_img} convert exited {status}; re-run with --zlib for maximum \
         compatibility, or --raw to ship the raw volume"
    ))
}

/// Copy a verified store payload into the staging directory.
///
/// Store outputs are mode 0444 and `fs::copy` carries that across, which would
/// leave a bundle nobody can delete without chmod first. Widen to 0644 — these
/// are ordinary distributable files, not store contents.
fn copy_into(source: &Path, destination: &Path) -> Result<(), String> {
    fs::copy(source, destination).map_err(|e| {
        format!(
            "copy {} -> {}: {e}",
            source.display(),
            destination.display()
        )
    })?;
    fs::set_permissions(destination, fs::Permissions::from_mode(0o644))
        .map_err(|e| format!("set mode on {}: {e}", destination.display()))
}

/// Write `contents` to `path` as an executable file.
fn write_executable(path: &Path, contents: &[u8]) -> Result<(), String> {
    let mut file = fs::File::create(path)
        .map_err(|e| format!("create {}: {e}", path.display()))?;
    file.write_all(contents)
        .map_err(|e| format!("write {}: {e}", path.display()))?;
    // Mode set AFTER the write, so a partially written launcher is never
    // executable, and explicitly rather than by umask: a `start` that arrives
    // without +x is the first thing a new user hits.
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))
        .map_err(|e| format!("make {} executable: {e}", path.display()))
}

/// Every file a finished bundle consists of, in publication order.
///
/// ONE list, because it was previously three: the staging steps, the checksum
/// targets, and the publish loop each spelled the names out, and two of them
/// disagreed with what was actually on disk. `MARKER_NAME` goes FIRST so an
/// interrupted publish leaves a directory that still identifies itself as this
/// tool's and can be replaced with `--force`; `CHECKSUMS_NAME` goes last
/// because it is computed from every name before it.
fn published_names(format: DiskFormat) -> Vec<&'static str> {
    vec![
        MARKER_NAME,
        KERNEL_NAME,
        INITRD_NAME,
        format.file_name(),
        LAUNCHER_NAME,
        README_NAME,
        CHECKSUMS_NAME,
    ]
}

/// What `SHA256SUMS` covers: the payload a downloader actually fetches.
///
/// Not itself, and NOT the marker. `.td-bundle` is this tool's private claim
/// on the directory, not something anyone downloads: the README documents five
/// files and tells the reader to run `sha256sum -c SHA256SUMS`, so listing a
/// sixth, undocumented, dot-prefixed name there fails that command with
/// `WARNING: 1 listed file could not be read` for everyone who fetched exactly
/// what the README described. That reads as a corrupt download.
fn checksum_targets(format: DiskFormat) -> Vec<&'static str> {
    published_names(format)
        .into_iter()
        .filter(|name| *name != CHECKSUMS_NAME && *name != MARKER_NAME)
        .collect()
}

/// Prove every published file exists in the scratch before checksums are taken.
///
/// The staging steps name their own outputs — `provision_selector` picks its
/// own file name, `compress_volume` picks the format — so a name that drifts
/// from `published_names` used to surface as an ENOENT from sha256 after the
/// entire build, or not at all. This turns that into one error that names the
/// missing file, at the moment the set is supposed to be complete.
fn verify_staged(dir: &Path, format: DiskFormat) -> Result<(), String> {
    let missing: Vec<&str> = published_names(format)
        .into_iter()
        // Written by `write_checksums` itself, immediately after this.
        .filter(|name| *name != CHECKSUMS_NAME && !dir.join(name).is_file())
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    Err(format!(
        "the bundle staged in {} is missing {}; a staging step and \
         `published_names` disagree about the file name",
        dir.display(),
        missing.join(", ")
    ))
}

/// Write `SHA256SUMS` in the exact format `sha256sum -c` reads: the lowercase
/// digest, two spaces, and the file name.
fn write_checksums(out: &Path, format: DiskFormat) -> Result<(), String> {
    let mut text = String::new();
    for name in checksum_targets(format) {
        let path = out.join(name);
        let digest = crate::sha256::sha256_file(&path)
            .map_err(|e| format!("hash {}: {e}", path.display()))?;
        text.push_str(&digest);
        text.push_str("  ");
        text.push_str(name);
        text.push('\n');
    }
    fs::write(out.join(CHECKSUMS_NAME), text)
        .map_err(|e| format!("write {CHECKSUMS_NAME}: {e}"))
}

/// Bytes as a short human figure for the summary. Nothing branches on it.
///
/// Three units, because a bundle spans five orders of magnitude: a 5 KiB
/// launcher beside a 600 MiB disk. Rounding the small files up to `1 MiB` — as
/// a MiB-only version of this did — describes a shell script as a thousand
/// times its size, right where an operator is checking that the download looks
/// sane.
fn human_bytes(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    const GIB: u64 = 1024 * MIB;
    if bytes >= GIB {
        format!("{:.1} GiB", bytes as f64 / GIB as f64)
    } else if bytes >= MIB {
        format!("{} MiB", bytes.div_euclid(MIB))
    } else {
        // Never `0 KiB` for a file that exists: a zero beside a name reads as a
        // failure to write it.
        format!("{} KiB", bytes.div_euclid(KIB).max(1))
    }
}

/// The README that ships beside the images.
fn readme(deployment_id: &str, format: DiskFormat) -> String {
    let disk = format.file_name();
    let minimum_qemu = format.minimum_qemu();
    // Only the compressed formats pay a write-amplification cost, and the two
    // of them pay different ones. A raw image is written in place, so claiming
    // a cluster rewrite there would be a warning about nothing.
    let persist_cost = match format {
        DiskFormat::Qcow2(compression) => format!(
            " It is also slower — the image is compressed in {} clusters, so \
             each guest write decompresses and rewrites a whole cluster \
             uncompressed, and the file grows much faster than the data \
             written to it.",
            compression.cluster_size_label()
        ),
        DiskFormat::Raw => String::new(),
    };
    format!(
        "# td — bootable demo system (x86-64)\n\
         \n\
         A prebuilt td system and a launcher, so you can try td under QEMU without\n\
         building it. Building td from source climbs the entire stage0-posix\n\
         bootstrap ladder — a tiny auditable seed, Mes/MesCC, TinyCC, a GCC/glibc\n\
         ladder, then Rust — and takes hours on a fast machine. This takes a\n\
         download.\n\
         \n\
         Deployment id: `{deployment_id}`\n\
         \n\
         That is the SHA-256 of the deployment manifest, which is td's content\n\
         identity for a system: the boot path recomputes it every time it selects\n\
         this deployment.\n\
         \n\
         ## What you need\n\
         \n\
         - QEMU {minimum_qemu} or newer (`qemu-system-x86_64`). Debian/Ubuntu:\n\
         \x20 `qemu-system-x86`. Fedora: `qemu-system-x86`. Arch: `qemu-full`.\n\
         \x20 macOS: `brew install qemu`. Check yours with `qemu-system-x86_64 --version`.\n\
         - A couple of GiB free in your temporary directory for the throwaway\n\
         \x20 overlay the guest writes to.\n\
         - Optional but worth having: read/write access to `/dev/kvm` on an x86-64\n\
         \x20 Linux host, usually membership in the `kvm` group. Without it QEMU\n\
         \x20 emulates every instruction and the boot takes several times longer.\n\
         \n\
         ## Run it\n\
         \n\
         ```\n\
         ./start\n\
         ```\n\
         \n\
         The guest's serial console is wired to your terminal, so the shell you get\n\
         is right there. If your host has a display, QEMU also opens a window on the\n\
         graphical framebuffer.\n\
         \n\
         To shut down: type `exit` (or Ctrl-D) at the shell. The session wrapper\n\
         tears the system down and QEMU exits. To force-quit QEMU at any moment:\n\
         Ctrl-A then X.\n\
         \n\
         `./start --help` lists the few options. `TD_QEMU_ACCEL=tcg` forces software\n\
         emulation even where hardware acceleration is available; `TD_VM_MEMORY=4096`\n\
         raises the guest's RAM from its {SYSTEM_GUEST_MEMORY_MIB} MiB default.\n\
         \n\
         ## Your changes are thrown away by default\n\
         \n\
         `./start` boots with `snapshot=on`, so everything the guest writes lands in\n\
         a temporary overlay QEMU deletes when it exits. `{disk}` is never\n\
         modified, every boot starts from the same state, and the checksum below\n\
         keeps matching.\n\
         \n\
         `./start --persist` writes into `{disk}` instead. That is a one-way\n\
         door: the file no longer matches `{CHECKSUMS_NAME}`, and getting the\n\
         pristine image back means downloading it again.{persist_cost} For\n\
         anything beyond a look around, install td to a real disk rather than\n\
         persisting into the demo image.\n\
         \n\
         ## What is in this directory\n\
         \n\
         | file | what it is |\n\
         | --- | --- |\n\
         | `{LAUNCHER_NAME}` | POSIX-sh launcher: finds QEMU, picks an accelerator, boots the system |\n\
         | `{KERNEL_NAME}` | the Linux kernel, built from source by td |\n\
         | `{INITRD_NAME}` | td's boot selector, carrying this bundle's public key |\n\
         | `{disk}` | a Btrfs volume: the signed deployment (td's userland, plus Firefox and its runtime) and an empty `@var` |\n\
         | `{README_NAME}` | this file |\n\
         | `{CHECKSUMS_NAME}` | SHA-256 of every file above |\n\
         \n\
         Verify what you downloaded:\n\
         \n\
         ```\n\
         sha256sum -c {CHECKSUMS_NAME}\n\
         ```\n\
         \n\
         ## What actually happens when you boot it\n\
         \n\
         QEMU direct-boots the kernel with the selector initramfs. The selector\n\
         verifies the deployment on the virtual disk against its manifest and this\n\
         bundle's key, kexecs it, and the deployment's own initramfs loop-mounts a\n\
         read-only EROFS root, mounts the persistent `@var` subvolume plus a\n\
         volatile `/run` and `/tmp`, and `switch_root`s into the real system.\n\
         \n\
         That is the same path a td installation takes on real hardware, minus the\n\
         bootloader — there is no ESP here, because QEMU is loading the kernel\n\
         directly.\n\
         \n\
         ## What you are running\n\
         \n\
         This is a demo image, configured for trying td rather than for keeping\n\
         anything in. Specifically:\n\
         \n\
         - **`root` and `tester` have empty passwords, and the console auto-logs\n\
         \x20 in as `tester`.** td's stated goal is passwordless-but-authorized\n\
         \x20 login backed by a hardware token; that is not built yet, and this\n\
         \x20 image is the honest current state. The guest gets NAT-only networking\n\
         \x20 with no forwarded ports, so nothing outside your machine can reach it\n\
         \x20 — but do not put anything private in it and do not expose it.\n\
         - **It ships Firefox**, as a confined third-party application: a marked\n\
         \x20 foreign payload that no part of td is built from, running behind td's\n\
         \x20 namespace, seccomp, D-Bus, portal and Wayland boundaries. Everything\n\
         \x20 else in the image is built from source.\n\
         - **Two bundles built from the same tree are not byte-identical.** Each is\n\
         \x20 signed with a freshly generated throwaway key, so the volume differs\n\
         \x20 every time. `{CHECKSUMS_NAME}` verifies THIS download; the deployment\n\
         \x20 id above is the reproducible identity of what is inside it.\n\
         \n\
         ## About the signature\n\
         \n\
         The deployment inside the volume is signed with an Ed25519 key generated\n\
         when this bundle was made. The public half is the only trust root inside\n\
         `{INITRD_NAME}`; the private half was never written to disk and no\n\
         longer exists. So the pairing proves the volume holds the deployment this\n\
         bundle shipped, and nothing else — it is not a project release identity.\n\
         Check the download against `{CHECKSUMS_NAME}` and the checksums published\n\
         beside it.\n\
         \n\
         ## This is a convenience artifact, not a distribution channel\n\
         \n\
         Nothing in td's build graph references this bundle. No recipe fetches it,\n\
         no build depends on it, and deleting it changes nothing about how td is\n\
         built. td's recipe graph is the distribution and updates are a `git pull`\n\
         and a rebuild, not a package download. This exists so that evaluating td\n\
         does not require first spending hours building it.\n\
         \n\
         ## Building it yourself\n\
         \n\
         ```\n\
         git clone <the td repository>\n\
         cd td\n\
         ./start\n\
         ```\n\
         \n\
         `./start` builds the distribution from source and boots exactly what this\n\
         bundle boots — the launcher here is rendered from the same QEMU profile\n\
         that command uses.\n"
    )
}

/// The closing summary: what was written, how big it is, and the one command
/// that puts it on a release page.
fn report(out: &Path, format: DiskFormat, deployment_id: &str) -> Result<(), String> {
    let mut total = 0u64;
    println!("\n   [bundle] wrote {}", out.display());
    for name in checksum_targets(format)
        .into_iter()
        .chain([CHECKSUMS_NAME])
    {
        let path = out.join(name);
        let size = fs::metadata(&path)
            .map_err(|e| format!("stat {}: {e}", path.display()))?
            .len();
        total = total.saturating_add(size);
        println!("            {:>10}  {name}", human_bytes(size));
    }
    println!("            {:>10}  total", human_bytes(total));
    println!("\n   [bundle] deployment id {deployment_id}");
    // The archive NAMES its members rather than taking `.`: `out` may be a
    // directory the operator also keeps other things in, and `tar .` would
    // publish whatever else is sitting there — unlisted in SHA256SUMS and
    // possibly private.
    let members = checksum_targets(format)
        .into_iter()
        .chain([CHECKSUMS_NAME])
        .collect::<Vec<_>>()
        .join(" ");
    println!(
        "\n   [bundle] try it:      {}/{LAUNCHER_NAME}\n            \
         publish it:  tar -C {} -czf td-vm-x86-64.tar.gz {members}\n            \
         \x20            gh release create <tag> td-vm-x86-64.tar.gz\n            \
         or upload those files individually as release assets.\n            \
         \n            \
         Before publishing: the volume contains Firefox, a third-party binary\n            \
         td redistributes rather than builds. Putting it on a release page is a\n            \
         different act from booting it locally, and the licensing and trademark\n            \
         question is yours to answer. Nothing here has published anything.\n",
        out.display(),
        out.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "td-bundle-{tag}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        fs::create_dir_all(&base).expect("scratch");
        base
    }

    /// Checking a destination may CREATE it, but must never remove anything.
    ///
    /// The two halves are not the same act, and the difference is the whole
    /// point of the split. `bundle_cli` asks before the warm so a bad `--out`
    /// costs microseconds rather than a whole climb — which requires actually
    /// attempting the creation, or `--out` on an unmounted path returns Ok here
    /// and fails hours later. But if that early call also CLEARED, `--force`
    /// would delete a good published bundle in the first milliseconds and then
    /// spend the build discovering the replacement cannot be made, leaving the
    /// operator with neither. Create early, clear late.
    #[test]
    fn checking_a_destination_creates_it_but_removes_nothing() {
        let base = scratch("check-pure");
        let ladder = base.join("ladder");
        fs::create_dir_all(&ladder).expect("ladder");

        // A destination that does not exist yet is created, so that a path
        // which CANNOT be created is diagnosed now rather than after the build.
        let fresh = base.join("fresh");
        ensure_out_dir_under(&fresh, false, &ladder).expect("fresh");
        assert!(fresh.is_dir(), "checking did not create the destination");

        // And an uncreatable destination is refused at once.
        let blocked = base.join("a-file").join("under-a-file");
        fs::write(base.join("a-file"), b"not a directory\n").expect("file");
        let error = ensure_out_dir_under(&blocked, false, &ladder)
            .expect_err("a destination that cannot be created must be refused early");
        // Named, so the refusal has to come from the creation attempt. A bare
        // is_err() would stay green if some earlier guard started rejecting it
        // for an unrelated reason, which is how this case got missed before.
        assert!(error.contains("create bundle directory"), "{error}");

        // A previous bundle survives every check, including the --force one
        // that later authorises its replacement.
        let out = base.join("bundle");
        fs::create_dir_all(&out).expect("out");
        fs::write(
            out.join(MARKER_NAME),
            format!("{MARKER_FORMAT}\ndeployment=abc\n"),
        )
        .expect("marker");
        fs::write(out.join(README_NAME), b"# old bundle\n").expect("readme");

        assert!(
            ensure_out_dir_under(&out, false, &ladder).is_err(),
            "an existing bundle must need --force"
        );
        ensure_out_dir_under(&out, true, &ladder).expect("forced check");
        assert!(
            out.join(README_NAME).is_file(),
            "checking with --force destroyed the previous bundle"
        );

        // Only opening it clears, and only then.
        let opened = open_out_dir(&out, true, &ladder).expect("open");
        assert_eq!(opened, out.canonicalize().expect("canonical"));
        assert!(!out.join(README_NAME).exists(), "opening did not clear");
        assert!(open_out_dir(&fresh, false, &ladder).is_ok());
        assert!(fresh.is_dir());

        let _ = fs::remove_dir_all(&base);
    }

    /// The destination checks are cheap and knowable before anything is
    /// fetched or built. These are the two that used to surface only after the
    /// climb: `--out` naming a file, and `--out` sitting inside the ladder work
    /// tree, where the next `clear-store` would delete the finished bundle.
    #[test]
    fn a_destination_that_can_never_work_is_rejected_and_never_created() {
        let base = scratch("early");
        let ladder = base.join("ladder");
        fs::create_dir_all(&ladder).expect("ladder");

        let file = base.join("not-a-directory");
        fs::write(&file, b"not a directory\n").expect("file");
        let error = ensure_out_dir_under(&file, true, &ladder).expect_err("file");
        assert!(error.contains("exists and is not a directory"), "{error}");

        let inside = ladder.join("dist");
        let error = ensure_out_dir_under(&inside, false, &ladder).expect_err("inside");
        assert!(error.contains("ladder work tree"), "{error}");
        assert!(
            !inside.exists(),
            "a rejected destination must not have been created"
        );

        // A ladder directory that does not exist yet is a PASS, not a silent
        // skip. No production caller reaches this — `RecipeCheckRunner::new`
        // creates the ladder tree first — but a guard that skips itself in
        // silence is the wrong shape whether or not it fires, and an absent
        // ladder genuinely cannot contain anything.
        let absent = base.join("no-ladder-here");
        ensure_out_dir_under(&base.join("elsewhere"), false, &absent).expect("absent ladder");

        let _ = fs::remove_dir_all(&base);
    }

    /// A bundle is one set of names, spelled once.
    ///
    /// Scope, stated honestly: this pins the LIST's shape and proves
    /// `verify_staged` names what is missing. It does NOT prove `run()` stages
    /// under these names — it populates its fixture from `published_names`
    /// itself, so that half is circular. What actually catches a staging step
    /// that names its output something else is the runtime
    /// `verify_staged(&scratch.dir, format)` call, which fires during a real
    /// bundle run. Nothing cheap reaches `run()`'s staging, because reaching it
    /// means climbing the ladder.
    ///
    /// The two defects behind this: the selector was staged as
    /// `selector-initramfs-trusted.cpio` while everything downstream looked for
    /// `selector-initramfs.cpio`, and the marker was hashed before it was
    /// written. Each failed EVERY bundle, on the happy path, after the entire
    /// build — and every test agreed with every other test while all of them
    /// were wrong about the disk.
    #[test]
    fn the_published_set_is_one_list_and_the_scratch_must_match_it() {
        for format in [
            DiskFormat::Qcow2(Compression::Zstd),
            DiskFormat::Qcow2(Compression::Zlib),
            DiskFormat::Raw,
        ] {
            let names = published_names(format);
            assert_eq!(
                names.first().copied(),
                Some(MARKER_NAME),
                "the marker must be published first, so an interrupted publish \
                 leaves a directory --force can still replace"
            );
            assert_eq!(
                names.last().copied(),
                Some(CHECKSUMS_NAME),
                "SHA256SUMS is computed from the names before it"
            );

            // Everything published is something --force knows how to clear.
            for name in &names {
                assert!(
                    bundle_files().contains(name),
                    "{name} is published but bundle_files() cannot clear it"
                );
            }

            // What a downloader verifies is exactly what the README documents:
            // the marker is this tool's private business and listing it would
            // fail `sha256sum -c` for anyone who fetched the documented files.
            let checksums = checksum_targets(format);
            assert!(!checksums.contains(&MARKER_NAME));
            assert!(!checksums.contains(&CHECKSUMS_NAME));
            assert_eq!(checksums.len(), names.len() - 2);

            // And the staging invariant itself: a missing file is named, here,
            // rather than surfacing as an ENOENT from sha256 after the climb.
            let base = scratch("staged");
            let error = verify_staged(&base, format).expect_err("empty scratch");
            for name in &names {
                if *name == CHECKSUMS_NAME {
                    continue;
                }
                assert!(error.contains(name), "{error} does not name {name}");
                fs::write(base.join(name), b"x").expect("stage");
            }
            verify_staged(&base, format).expect("fully staged");
            let _ = fs::remove_dir_all(&base);
        }
    }

    /// The marker is what makes a replacement provable rather than guessed.
    /// Without it, `--force --out .` in this very repository would delete the
    /// tracked `README.md` and `start`, because both are bundle file names.
    #[test]
    fn a_directory_is_replaceable_only_with_this_tools_marker() {
        let base = std::env::temp_dir().join(format!(
            "td-bundle-marker-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        fs::create_dir_all(&base).expect("scratch");

        // A directory that merely looks like one — the exact shape of a
        // repository checkout, which carries both of these names.
        fs::write(base.join(README_NAME), b"# some other project\n").expect("readme");
        fs::write(base.join(LAUNCHER_NAME), b"#!/bin/sh\n").expect("start");
        assert!(
            !is_bundle_directory(&base),
            "bundle file names alone must never license a delete"
        );

        // A marker with the wrong content is not this tool's marker.
        fs::write(base.join(MARKER_NAME), b"something else\n").expect("marker");
        assert!(!is_bundle_directory(&base));

        fs::write(
            base.join(MARKER_NAME),
            format!("{MARKER_FORMAT}\ndeployment=abc\n"),
        )
        .expect("marker");
        assert!(is_bundle_directory(&base));

        let _ = fs::remove_dir_all(&base);
    }

    /// The marker has to be a file the bundle actually writes and owns,
    /// otherwise the check above can never pass for a real bundle — and it
    /// must NOT be one a downloader is asked to verify. The README documents
    /// five files and says to run `sha256sum -c SHA256SUMS`; a sixth,
    /// undocumented, dot-prefixed entry makes that command report a missing
    /// file and exit non-zero for everyone who fetched exactly what they were
    /// told to, which reads as a corrupt download.
    #[test]
    fn the_marker_is_owned_by_the_bundle_but_not_checksummed() {
        assert!(bundle_files().contains(&MARKER_NAME));
        assert!(published_names(DiskFormat::Raw).contains(&MARKER_NAME));
        assert!(!checksum_targets(DiskFormat::Raw).contains(&MARKER_NAME));
    }

    /// `--force` may delete only files a bundle owns. `out` is whatever the
    /// operator typed, and this list is the whole of what stands between a
    /// mistyped `--out` and someone's home directory.
    #[test]
    fn force_only_ever_removes_files_a_bundle_wrote() {
        let owned = bundle_files();
        for name in [
            KERNEL_NAME,
            INITRD_NAME,
            LAUNCHER_NAME,
            README_NAME,
            CHECKSUMS_NAME,
            "td-system.qcow2",
            "td-system.img",
        ] {
            assert!(owned.contains(&name), "{name} is written but never replaced");
        }
        assert!(owned.contains(&MARKER_NAME));
        assert_eq!(owned.len(), 8, "a name was added without a decision about --force");
    }

    /// Both volume names are removable regardless of which one is being
    /// written: a `--raw` bundle over a qcow2 one that left its old disk
    /// behind would ship two multi-hundred-MiB files and a `SHA256SUMS` that
    /// names one of them.
    #[test]
    fn replacing_a_bundle_clears_the_other_disk_format() {
        let owned = bundle_files();
        assert!(owned.contains(&DiskFormat::Qcow2(Compression::Zstd).file_name()));
        assert!(owned.contains(&DiskFormat::Raw.file_name()));
    }

    /// Every file a downloader fetches is checksummed, except the checksum
    /// file itself (which cannot contain its own digest) and the marker (which
    /// a downloader neither fetches nor is told about).
    #[test]
    fn the_checksum_list_covers_the_whole_bundle() {
        for format in [
            DiskFormat::Qcow2(Compression::Zstd),
            DiskFormat::Qcow2(Compression::Zlib),
            DiskFormat::Raw,
        ] {
            let listed = checksum_targets(format);
            assert!(!listed.contains(&CHECKSUMS_NAME));
            assert!(!listed.contains(&MARKER_NAME));
            for name in [KERNEL_NAME, INITRD_NAME, LAUNCHER_NAME, README_NAME] {
                assert!(listed.contains(&name), "{name} is shipped unchecksummed");
            }
            assert!(listed.contains(&format.file_name()));
            // ...and only the disk that was actually written.
            assert_eq!(listed.len(), 5, "{format:?}");

            // What is checksummed is exactly what the README's table lists, so
            // the verify command it prints succeeds on the files it describes.
            let text = readme("abc123", format);
            for name in &listed {
                // The whole table ROW, not just a backticked name anywhere in
                // the prose. A bare `contains` is satisfied by "start" inside
                // "starts from the same state"; a backtick alone is satisfied
                // by the initramfs being mentioned under "About the
                // signature". Only the row proves the table lists the file.
                assert!(
                    text.contains(&format!("| `{name}` |")),
                    "{name} is checksummed but the README's table never lists it"
                );
            }
        }
    }

    /// The README names the disk the launcher will look for. A README that
    /// tells the reader to check a file that is not there is a support ticket.
    #[test]
    fn the_readme_names_the_disk_that_shipped() {
        for format in [DiskFormat::Qcow2(Compression::Zstd), DiskFormat::Raw] {
            let text = readme("abc123", format);
            assert!(text.contains(format.file_name()), "{format:?}");
            let other = match format {
                DiskFormat::Qcow2(_) => DiskFormat::Raw,
                DiskFormat::Raw => DiskFormat::Qcow2(Compression::Zstd),
            };
            assert!(
                !text.contains(other.file_name()),
                "{format:?} README mentions the format that did not ship"
            );
            assert!(text.contains("abc123"), "the deployment id is the release's name");
        }
    }

    /// The two claims the README makes that a reader would act on: writes are
    /// discarded by default, and this is not something td's build depends on.
    #[test]
    fn the_readme_states_the_load_bearing_promises() {
        let text = readme("abc123", DiskFormat::Qcow2(Compression::Zstd));
        assert!(text.contains("snapshot=on"));
        assert!(text.contains("--persist"));
        assert!(text.contains("not a distribution channel"));
        assert!(text.contains("git pull"));
    }


    /// The README's stated qemu floor must match the image beside it. A bundle
    /// that ships zstd and asks for 2.4 sends the reader to a qemu that opens
    /// nothing, with an error about a compression type rather than a version.
    #[test]
    fn the_readme_states_the_floor_its_own_image_needs() {
        let zstd = readme("abc123", DiskFormat::Qcow2(Compression::Zstd));
        assert!(zstd.contains("QEMU 5.1 or newer"), "zstd README: {zstd}");

        // Not 2.4: every launcher emits `-audiodev`, which is qemu 4.0, so the
        // codec's own floor is not the floor a reader needs.
        let zlib = readme("abc123", DiskFormat::Qcow2(Compression::Zlib));
        assert!(zlib.contains("QEMU 4.0 or newer"), "zlib README: {zlib}");
        assert!(!zlib.contains("5.1"));
        assert!(!zlib.contains("2.4"));

        let raw = readme("abc123", DiskFormat::Raw);
        assert!(raw.contains("QEMU 4.0 or newer"));
    }

    #[test]
    fn sizes_read_as_whole_units() {
        assert_eq!(human_bytes(5 * 1024 * 1024 * 1024), "5.0 GiB");
        assert_eq!(human_bytes(250 * 1024 * 1024), "250 MiB");
        // The launcher and the checksum file live down here. Reporting them in
        // MiB rounded them up to `1 MiB`, overstating a 5 KiB script by three
        // orders of magnitude in the one place an operator eyeballs the bundle.
        assert_eq!(human_bytes(5445), "5 KiB");
        assert_eq!(human_bytes(394), "1 KiB");
        // Nothing in a bundle is genuinely zero-sized, and a `0` beside a name
        // reads as a file that failed to write.
        assert_eq!(human_bytes(1), "1 KiB");
        // The unit boundaries themselves, where an off-by-one shows up.
        assert_eq!(human_bytes(1024 * 1024 - 1), "1023 KiB");
        assert_eq!(human_bytes(1024 * 1024), "1 MiB");
    }

    #[test]
    fn the_nearest_existing_ancestor_is_what_gets_compared() {
        let existing = std::env::temp_dir();
        let missing = existing.join("td-bundle-does-not-exist/nor/does/this");
        let resolved = canonical_parent(&missing).unwrap();
        assert_eq!(
            resolved,
            existing.canonicalize().unwrap(),
            "a missing --out must still resolve to something the ladder check can compare"
        );
    }

    /// A RELATIVE `--out` has to resolve too, and this is the case that was
    /// wrong: `Path::parent()` of a bare component is `Some("")`, not `None`,
    /// so the walk bottomed out on the empty path and returned an error.
    /// `ensure_out_dir_under` treats that error as "cannot compare" and carries on,
    /// so the ladder-containment guard was silently skipped — for `DEFAULT_OUT`
    /// itself on any run where `dist/` did not exist yet, which is the first
    /// run every operator makes.
    #[test]
    fn a_relative_out_still_resolves_so_the_guard_runs() {
        let here = std::env::current_dir().unwrap().canonicalize().unwrap();
        for relative in [
            "td-bundle-nonexistent",
            DEFAULT_OUT,
            "td-bundle-nonexistent/deeper/still",
        ] {
            let resolved = canonical_parent(Path::new(relative))
                .unwrap_or_else(|e| panic!("{relative}: {e}"));
            assert!(
                resolved.starts_with(&here) || resolved == here,
                "{relative} resolved to {}, outside {}",
                resolved.display(),
                here.display()
            );
        }
    }

    /// An absolute path whose every component is missing must walk to `/`
    /// rather than erroring: the guard has to have something to compare.
    #[test]
    fn an_entirely_missing_absolute_path_walks_to_the_root() {
        let resolved =
            canonical_parent(Path::new("/td-bundle-nonexistent/a/b/c")).unwrap();
        assert_eq!(resolved, Path::new("/"));
    }
}

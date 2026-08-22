//! The S3 build sandbox: execute a parsed `.drv` in a fresh user namespace,
//! replicating the pinned daemon's guest-visible contract (read off
//! nix/libstore/build.cc):
//!   - namespaces: NEWUSER|NEWNS|NEWPID|NEWNET|NEWIPC|NEWUTS. NEWNET makes the
//!     build offline by construction; NEWPID (in the same unshare as NEWUSER, so
//!     the PID ns is owned by the new user ns) forks the builder to PID 1 of its
//!     own pid namespace with a FRESH procfs — the build sees only its own process
//!     tree, not the host's (the daemon, other concurrent builds, their
//!     /proc/<pid>/environ), full parity with host_shell / `guix shell -C`;
//!   - uid/gid: guest 30001/30000 mapped over the invoking user, setgroups
//!     denied (build.cc defaultGuestUID/GID, initializeUserNamespace);
//!   - chroot: the build pivot_roots into a MINIMAL fresh-tmpfs root holding only
//!     the staged /gnu/store, a writable /tmp, /dev rbind'd from the invoking
//!     namespace, a fresh /proc, and a minimal /etc — nothing else of the host
//!     filesystem. So `build` is SELF-hermetic, not dependent on the outer
//!     host-sandbox to hide /etc, /home, /usr, /var/guix … from the builder
//!     (own-builder-daemon: self-hermetic build sandbox);
//!   - store: every closure item bind-mounted into a staged directory which
//!     is then rbind-mounted over the new root's /gnu/store, so the builder sees
//!     real store paths while writes land in the scratch directory (the rootless
//!     rung's mechanics) and the bound inputs stay protected by their host-root
//!     ownership;
//!   - build dir: private disk-backed scratch bind-mounted at `/tmp`, with
//!     `/tmp/guix-build-<drvname>-0` (0700, <drvname> keeps the .drv suffix) as
//!     cwd. Mesboot steps are materialized there and consumed before recipe
//!     execution;
//!   - env: cleared, then PATH/HOME/NIX_STORE/NIX_BUILD_CORES, the drv's
//!     env, then NIX_BUILD_TOP/TMPDIR/TEMPDIR/TMP/TEMP/PWD — build.cc's exact
//!     set and override order (the TMPDIR group wins over drv env). The trusted
//!     mesboot runner receives the hashed TD_STEPS data path through TD_STEPS_FILE.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
#![allow(unsafe_code)] // confined raw-syscall / low-level layer (UNSAFE.md)

use std::collections::BTreeSet;
use std::ffi::{CString, OsStr};
use std::fs;
use std::io;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, ExitStatus};

use crate::drv::Derivation;
use crate::sys;

/// The store prefix WITH a trailing slash (`/td/store/` by default, or a
/// `TD_STORE_DIR`-selected prefix). Every store-path operation strips/joins this
/// so a build targeting `/td/store` stages its inputs and writes its outputs there
/// NATIVELY — no post-hoc `/gnu/store -> /td/store` byte rewrite. The prefix is part of
/// the content hash (`store::make_store_path_in`), so a `/td/store` build is a distinct,
/// guix-independent store, not a relabel of a `/gnu/store` one.
fn store_prefix() -> String {
    format!("{}/", crate::store::store_dir())
}
const GUEST_UID: u32 = 30001;
const GUEST_GID: u32 = 30000;

fn err(what: String) -> io::Error {
    io::Error::new(io::ErrorKind::Other, what)
}

fn trusted_recipe_builder(builder: &str) -> bool {
    builder == format!("{}/bin/td-builder", crate::store::builder_identity_path())
}

fn forward_trusted_check_policy(
    command: &mut Command,
    builder: &str,
    inherited: Option<&OsStr>,
) {
    if trusted_recipe_builder(builder) {
        if let Some(value) = inherited {
            command.env(crate::check_memory::JOB_BUDGET_ENV, value);
        }
    }
}

/// Replace the trusted mesboot runner's hashed `TD_STEPS` data with the
/// sandbox-local file path that `run_mesboot` consumes.
fn configure_builder_env(
    command: &mut Command,
    builder: &str,
    args: &[String],
    env: &[(String, String)],
    mesboot_steps_file: &Path,
) -> io::Result<Option<String>> {
    if env
        .iter()
        .any(|(key, _)| key == crate::check_memory::JOB_BUDGET_ENV)
    {
        return Err(err(format!(
            "derivation environment may not set reserved policy key {}",
            crate::check_memory::JOB_BUDGET_ENV
        )));
    }
    let mesboot = args.len() == 1
        && args.first().is_some_and(|arg| arg == "mesboot-build")
        && builder.ends_with("/bin/td-builder");
    if mesboot && !trusted_recipe_builder(builder) {
        return Err(err(format!(
            "mesboot derivation builder {builder} is not the stable td-builder identity"
        )));
    }
    let application_policy = application_manifest_policy_for(builder, args);
    let application_manifest = if trusted_recipe_builder(builder) {
        unique_env(env, "TD_APPLICATION_MANIFEST")?
    } else {
        None
    };
    let application_spec = if trusted_recipe_builder(builder) {
        unique_env(env, "TD_APPLICATION_SPEC")?
    } else {
        None
    };
    let application_launcher = if trusted_recipe_builder(builder) {
        unique_env(env, "TD_APPLICATION_LAUNCHER")?
    } else {
        None
    };
    if (application_manifest.is_some()
        || application_spec.is_some()
        || application_launcher.is_some())
        && application_policy.is_none()
    {
        return Err(err(
            "trusted td-builder invocation with application metadata has no metadata policy"
                .to_string(),
        ));
    }
    if application_manifest.is_some() != application_spec.is_some()
        || application_manifest.is_some() != application_launcher.is_some()
    {
        return Err(err(
            "application metadata requires manifest, compiled spec and launcher export".to_string(),
        ));
    }
    if (application_manifest.is_some()
        || application_spec.is_some()
        || application_launcher.is_some())
        && application_policy == Some(ApplicationManifestPolicy::Reserve)
    {
        return Err(err(
            "non-application phase received application metadata".to_string(),
        ));
    }
    let mut steps = None;
    for (key, value) in env {
        if mesboot && key == "TD_STEPS" {
            steps = Some(value.clone());
        } else if !mesboot && application_policy.is_some() && key == "TD_PAYLOAD_MAP" {
            // A non-mesboot application payload is consumed only by the outer
            // spec compiler. Keep its bind noexec through the drv contract, but
            // do not hand the package process the name-to-path map.
            continue;
        } else if application_policy.is_some()
            && matches!(
                key.as_str(),
                "TD_APPLICATION_MANIFEST" | "TD_APPLICATION_SPEC" | "TD_APPLICATION_LAUNCHER"
            )
        {
            continue;
        } else {
            command.env(key, value);
        }
    }
    if mesboot && steps.is_none() {
        return Err(err(
            "mesboot derivation is missing its TD_STEPS file payload".to_string(),
        ));
    }
    if mesboot {
        command.env(crate::build::MESBOOT_STEPS_FILE_ENV, mesboot_steps_file);
    }
    Ok(steps)
}

fn write_mesboot_steps_file(path: &Path, steps: Option<&str>) -> io::Result<()> {
    if let Some(steps) = steps {
        fs::write(path, steps)?;
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ApplicationManifestPolicy {
    Materialize,
    Reserve,
}

fn application_manifest_policy_for(
    builder: &str,
    args: &[String],
) -> Option<ApplicationManifestPolicy> {
    if !trusted_recipe_builder(builder) || args.len() != 1 {
        return None;
    }
    let runner = args.first()?.as_str();
    if crate::APPLICATION_PHASE_RUNNERS.contains(&runner) {
        Some(ApplicationManifestPolicy::Materialize)
    } else if crate::NON_APPLICATION_PHASE_RUNNERS.contains(&runner) {
        Some(ApplicationManifestPolicy::Reserve)
    } else {
        None
    }
}

fn application_manifest_policy(drv: &Derivation) -> Option<ApplicationManifestPolicy> {
    application_manifest_policy_for(&drv.builder, &drv.args)
}

#[cfg(test)]
pub(crate) fn has_application_manifest_policy(drv: &Derivation) -> bool {
    application_manifest_policy(drv).is_some()
}

fn unique_env<'a>(env: &'a [(String, String)], key: &str) -> io::Result<Option<&'a str>> {
    let mut found = None;
    for (name, value) in env {
        if name == key {
            if found.is_some() {
                return Err(err(format!(
                    "derivation carries duplicate environment key {key:?}"
                )));
            }
            found = Some(value.as_str());
        }
    }
    Ok(found)
}

fn unique_drv_env<'a>(drv: &'a Derivation, key: &str) -> io::Result<Option<&'a str>> {
    unique_env(&drv.env, key)
}

fn finalize_application_output(
    drv: &Derivation,
    outputs: &[(String, PathBuf)],
) -> io::Result<()> {
    let Some(policy) = application_manifest_policy(drv) else {
        if trusted_recipe_builder(&drv.builder)
            && (unique_drv_env(drv, "TD_APPLICATION_MANIFEST")?.is_some()
                || unique_drv_env(drv, "TD_APPLICATION_SPEC")?.is_some()
                || unique_drv_env(drv, "TD_APPLICATION_LAUNCHER")?.is_some())
        {
            return Err(err(
                "trusted td-builder invocation with application metadata has no metadata policy"
                    .to_string(),
            ));
        }
        return Ok(());
    };
    let manifest = unique_drv_env(drv, "TD_APPLICATION_MANIFEST")?;
    let spec = unique_drv_env(drv, "TD_APPLICATION_SPEC")?;
    let launcher = unique_drv_env(drv, "TD_APPLICATION_LAUNCHER")?;
    if manifest.is_some() != spec.is_some() || manifest.is_some() != launcher.is_some() {
        return Err(err(
            "application metadata requires manifest, compiled spec and launcher export".to_string(),
        ));
    }
    if policy == ApplicationManifestPolicy::Reserve
        && (manifest.is_some() || spec.is_some() || launcher.is_some())
    {
        return Err(err(
            "non-application phase received application metadata".to_string(),
        ));
    }
    let mut output_names = BTreeSet::new();
    let mut output_paths = BTreeSet::new();
    for (name, path) in outputs {
        if !output_names.insert(name.as_str()) {
            return Err(err(format!(
                "application-manifest phase has duplicate output name {name:?}"
            )));
        }
        if !output_paths.insert(path.as_path()) {
            return Err(err(format!(
                "application-manifest phase has duplicate output path {}",
                path.display()
            )));
        }
    }
    let out = outputs
        .iter()
        .find(|(name, _)| name == "out")
        .map(|(_, path)| path);
    for (name, path) in outputs {
        if name != "out" {
            crate::build::materialize_application_metadata_at(path, None, None, None)
                .map_err(|error| err(format!("application metadata finalization: {error}")))?;
        }
    }
    let (out_manifest, out_spec, out_launcher) = if policy == ApplicationManifestPolicy::Materialize
    {
        (manifest, spec, launcher)
    } else {
        (None, None, None)
    };
    match (out, out_manifest, out_spec, out_launcher) {
        (Some(path), manifest, spec, launcher) => {
            crate::build::materialize_application_metadata_at(path, manifest, spec, launcher)
                .map_err(|error| err(format!("application metadata finalization: {error}")))
        }
        (None, Some(_), Some(_), Some(_)) => Err(err(
            "application declaration requires an `out' output".to_string()
        )),
        (None, None, None, None) => Ok(()),
        _ => Err(err("application metadata is incomplete".to_string())),
    }
}

/// Map exactly one uid/gid pair into a user namespace already entered via
/// `unshare(2)`/`CLONE_NEWUSER` (a separate call — its flags and failure handling differ
/// per caller, so this covers only the part that's IDENTICAL everywhere: the ordering-
/// sensitive id-mapping triplet). Order matters: `setgroups` MUST be denied BEFORE the
/// `gid_map` write — the kernel refuses an unprivileged gid_map write otherwise
/// (CVE-2014-8989). `host_uid`/`host_gid` are the real ids as seen from OUTSIDE the
/// namespace (the map's second column); `uid_target`/`gid_target` are what the process
/// appears as INSIDE it (the map's first column).
pub fn map_userns_id(host_uid: u32, host_gid: u32, uid_target: u32, gid_target: u32) -> io::Result<()> {
    fs::write("/proc/self/setgroups", "deny")?;
    fs::write("/proc/self/uid_map", format!("{uid_target} {host_uid} 1"))?;
    fs::write("/proc/self/gid_map", format!("{gid_target} {host_gid} 1"))?;
    Ok(())
}

/// A closure entry is either a bare CANONICAL store path or `CANONICAL\tON-DISK`.
/// The canonical half is the `/gnu/store/<base>` path the build must SEE; the
/// on-disk half is where the tree physically lives on the host to bind FROM. They
/// differ only for a td-interned item (e.g. a source td restored into its OWN store
/// dir, never registered with the daemon) — every daemon-resident item is a bare
/// path, so on-disk defaults to canonical. This keeps a td-owned store reachable by
/// the sandbox with no extra argument, the encoding riding through `closure.txt`.
pub fn split_closure_entry(entry: &str) -> (&str, &str) {
    match entry.split_once('\t') {
        Some((canonical, on_disk)) => (canonical, on_disk),
        None => (entry, entry),
    }
}

/// build.cc storePathToName: strip the store dir and the 32-char base32
/// hash + dash. For a drv path the result KEEPS the .drv suffix.
/// Pure core: `prefix` is the store dir WITH trailing slash, so this is testable for
/// any store (`/gnu/store/` or `/td/store/`) without touching process env.
fn store_path_name_in<'a>(prefix: &str, path: &'a str) -> io::Result<&'a str> {
    let base = path
        .strip_prefix(prefix)
        .ok_or_else(|| err(format!("{path}: not a store path")))?;
    if base.len() > 33 && base.as_bytes()[32] == b'-' && !base.contains('/') {
        Ok(&base[33..])
    } else {
        Err(err(format!("{path}: malformed store path basename")))
    }
}

/// Strip the active store dir + hash, yielding the path name (`store::store_dir()`-aware).
pub fn store_path_name(path: &str) -> io::Result<&str> {
    store_path_name_in(&store_prefix(), path)
}

fn mountinfo_path(field: &str) -> Option<PathBuf> {
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0usize;
    while at < bytes.len() {
        if bytes.get(at) == Some(&b'\\') {
            let digits = bytes.get(at.saturating_add(1)..at.saturating_add(4))?;
            if digits.len() != 3 || !digits.iter().all(|byte| matches!(byte, b'0'..=b'7')) {
                return None;
            }
            let first = *digits.first()?;
            let second = *digits.get(1)?;
            let third = *digits.get(2)?;
            let value = u16::from(first - b'0') * 64
                + u16::from(second - b'0') * 8
                + u16::from(third - b'0');
            let value = u8::try_from(value).ok()?;
            if value == 0 {
                return None;
            }
            out.push(value);
            at = at.saturating_add(4);
        } else {
            out.push(*bytes.get(at)?);
            at = at.saturating_add(1);
        }
    }
    Some(PathBuf::from(std::ffi::OsString::from_vec(out)))
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct MountBacking {
    fs_type: String,
    overlay_upper: Option<PathBuf>,
}

fn mount_backing_from(mountinfo: &str, path: &Path) -> Option<MountBacking> {
    let mut best: Option<(usize, MountBacking)> = None;
    for line in mountinfo.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let Some(split) = fields.iter().position(|field| *field == "-") else {
            continue;
        };
        let Some(mount) = fields.get(4).and_then(|field| mountinfo_path(field)) else {
            continue;
        };
        let Some(fs_type) = fields.get(split.saturating_add(1)) else {
            continue;
        };
        let overlay_upper = if *fs_type == "overlay" {
            fields
                .get(split.saturating_add(3))
                .and_then(|options| {
                    options
                        .split(',')
                        .find_map(|option| option.strip_prefix("upperdir="))
                })
                .and_then(mountinfo_path)
        } else {
            None
        };
        if !path.starts_with(&mount) {
            continue;
        }
        let depth = mount.components().count();
        if best.as_ref().is_none_or(|(old, _)| depth >= *old) {
            best = Some((
                depth,
                MountBacking {
                    fs_type: (*fs_type).to_string(),
                    overlay_upper,
                },
            ));
        }
    }
    best.map(|(_, backing)| backing)
}

fn memory_backed_fs(fs_type: &str) -> bool {
    matches!(
        fs_type,
        "tmpfs" | "ramfs" | "hugetlbfs" | "devtmpfs"
    )
}

fn require_disk_backed_from(
    mountinfo: &str,
    path: &Path,
    depth: usize,
    allow_hidden_overlay_upper: bool,
) -> io::Result<()> {
    if depth > 8 {
        return Err(err(format!(
            "cannot resolve overlay backing filesystem for build scratch {}",
            path.display()
        )));
    }
    let backing = mount_backing_from(mountinfo, path).ok_or_else(|| {
        err(format!(
            "cannot identify the filesystem backing build scratch {}",
            path.display()
        ))
    })?;
    if memory_backed_fs(&backing.fs_type) {
        return Err(err(format!(
            "build scratch {} is on memory-backed {}; choose disk-backed scratch",
            path.display(),
            backing.fs_type
        )));
    }
    if backing.fs_type == "overlay" {
        let upper = backing.overlay_upper.ok_or_else(|| {
            err(format!(
                "cannot prove that overlay scratch {} has a disk-backed writable layer",
                path.display()
            ))
        })?;
        if !upper.is_absolute() || upper == path {
            return Err(err(format!(
                "cannot prove that overlay scratch {} has a distinct disk-backed writable layer",
                path.display()
            )));
        }
        // A detached sandbox and an ordinary rootless container can expose an
        // overlay bind while hiding the host's upperdir pathname behind their
        // own root. Reject a visible memory-backed upper, but do not reject an
        // otherwise usable disk overlay merely because that host path cannot
        // be resolved in this namespace. This is cooperative OOM avoidance,
        // not an authenticated storage boundary.
        if upper.exists() || !allow_hidden_overlay_upper {
            require_disk_backed_from(
                mountinfo,
                &upper,
                depth.saturating_add(1),
                allow_hidden_overlay_upper,
            )?;
        }
    }
    Ok(())
}

pub(crate) fn require_disk_backed(path: &Path) -> io::Result<()> {
    let canonical = fs::canonicalize(path)?;
    let mountinfo = fs::read_to_string("/proc/self/mountinfo")?;
    require_disk_backed_from(&mountinfo, &canonical, 0, true)
}

struct ScratchCleanup {
    path: PathBuf,
    _lease: fs::File,
}

fn make_dir_accessible(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() {
        return Ok(());
    }
    let mode = metadata.permissions().mode();
    if mode & 0o700 != 0o700 {
        fs::set_permissions(path, fs::Permissions::from_mode(mode | 0o700))?;
    }
    Ok(())
}

/// Delete a recipe-controlled directory without recursion and without retaining
/// one pathname per sibling or depth. Each pass removes files until it finds a
/// child directory, descends using one mutable PathBuf, then rescans the parent
/// after the child is gone. That trades some directory scans for a fixed memory
/// footprint and cannot overflow a daemon worker's call stack.
fn remove_directory_tree(path: &Path) -> io::Result<()> {
    let root = path.to_path_buf();
    let mut current = root.clone();
    loop {
        let metadata = match fs::symlink_metadata(&current) {
            Ok(metadata) => metadata,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                if current == root {
                    return Ok(());
                }
                if !current.pop() {
                    return Err(err("scratch cleanup lost its root".to_string()));
                }
                continue;
            }
            Err(e) => return Err(e),
        };
        // A child swapped from a directory to a symlink or file is removed,
        // never followed outside the scratch tree.
        if !metadata.is_dir() {
            fs::remove_file(&current)?;
            if current == root {
                return Ok(());
            }
            if !current.pop() {
                return Err(err("scratch cleanup lost its root".to_string()));
            }
            continue;
        }
        make_dir_accessible(&current)?;

        let mut descended = false;
        for entry in fs::read_dir(&current)? {
            let entry = match entry {
                Ok(entry) => entry,
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e),
            };
            let child = entry.path();
            let child_metadata = match fs::symlink_metadata(&child) {
                Ok(metadata) => metadata,
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e),
            };
            if child_metadata.is_dir() {
                current = child;
                descended = true;
                break;
            }
            match fs::remove_file(&child) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                // A concurrent replacement became a directory; rescan it on
                // the next pass rather than treating the race as admission.
                Err(e) if e.kind() == io::ErrorKind::IsADirectory => {
                    current = child;
                    descended = true;
                    break;
                }
                Err(e) => return Err(e),
            }
        }
        if descended {
            continue;
        }
        match fs::remove_dir(&current) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) if e.kind() == io::ErrorKind::DirectoryNotEmpty => continue,
            Err(e) => return Err(e),
        }
        if current == root {
            return Ok(());
        }
        if !current.pop() {
            return Err(err("scratch cleanup lost its root".to_string()));
        }
    }
}

pub(crate) fn remove_scratch_tree(path: &Path) -> io::Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    if metadata.is_dir() {
        remove_directory_tree(path)
    } else {
        fs::remove_file(path)
    }
}

impl Drop for ScratchCleanup {
    fn drop(&mut self) {
        let _ = remove_scratch_tree(&self.path);
    }
}

/// Reclaim disk-backed build trees whose builder no longer holds the sibling
/// lease. The walk follows only the daemon scratch layout and never descends
/// into output `newstore` trees. A missing lease is preserved for compatibility
/// with an older concurrently running builder; only scratch created by this
/// version is eligible for automatic crash recovery.
pub(crate) fn sweep_abandoned_build_temps(root: &Path) -> io::Result<()> {
    struct Frame {
        depth: usize,
        entries: fs::ReadDir,
    }

    fn reclaim_one(dir: &Path) -> io::Result<()> {
        let build_tmp = dir.join("build-tmp");
        if fs::symlink_metadata(&build_tmp)
            .map(|metadata| metadata.is_dir())
            .unwrap_or(false)
        {
            let lease_path = dir.join(".build-tmp.lock");
            if let Ok(lease) = fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&lease_path)
            {
                // Crash recovery is best-effort and must never become global
                // daemon admission. A pathological tree may remain on disk,
                // but it cannot poison unrelated future requests.
                if lease.try_lock().is_ok() {
                    let _ = remove_scratch_tree(&build_tmp);
                }
            }
        }
        Ok(())
    }

    reclaim_one(root)?;
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(_) => return Ok(()),
    };
    // The daemon layout is at most two levels below `root`. Retain only those
    // three live iterators, so both memory and file-descriptor use stay constant
    // with directory width while every entry is eventually visited.
    let mut stack = vec![Frame { depth: 0, entries }];
    while let Some(frame) = stack.last_mut() {
        let Some(entry) = frame.entries.next() else {
            stack.pop();
            continue;
        };
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        let name = entry.file_name();
        if matches!(name.to_str(), Some("newstore" | "buildroot" | "build-tmp")) {
            continue;
        }
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(_) => continue,
        };
        if !file_type.is_dir() {
            continue;
        }
        let depth = frame.depth.saturating_add(1);
        let path = entry.path();
        reclaim_one(&path)?;
        if depth < 2 {
            if let Ok(entries) = fs::read_dir(&path) {
                stack.push(Frame { depth, entries });
            }
        }
    }
    Ok(())
}

/// Cap `cmd`'s child — and everything it forks/execs — at `bytes` of data
/// segment via a pre_exec setrlimit(RLIMIT_DATA). td's own prlimit(1)
/// replacement for the gate-runner per-process memory backstop (#319): no
/// host util-linux binary, so it works inside the loop sandbox. The requested
/// cap is clamped to the ambient HARD limit (raising a hard limit is EPERM
/// for an unprivileged process, and a host whose hard limit sits below the
/// default cap would otherwise red every gate with an opaque spawn error —
/// review finding; the tighter-than-requested cap is still fail-closed).
/// A refused setrlimit still fails the spawn (the gate reds) rather than
/// running the body uncapped.
pub fn cap_child_data_rlimit(cmd: &mut Command, bytes: u64) {
    let bytes = match sys::get_rlimit(sys::RLIMIT_DATA) {
        Ok((_, hard)) => bytes.min(hard),
        Err(_) => bytes,
    };
    // Post-fork safe: set_rlimit is one raw syscall; its error path is
    // io::Error::from_raw_os_error (no allocation).
    unsafe {
        cmd.pre_exec(move || sys::set_rlimit(sys::RLIMIT_DATA, bytes, bytes));
    }
}

/// Arm `cmd`'s child to die with the process that spawned it:
/// PR_SET_PDEATHSIG(SIGKILL) between fork and exec. A gate runner that is
/// killed rather than exiting — a cancelled agent session, Ctrl-C, an OOM —
/// otherwise leaves its gate reparented to init, where it keeps running with
/// nobody waiting on it; eight such test binaries once spun for three days.
///
/// This reaches the DIRECT child only: the flag is cleared across fork(2).
/// Check-host and gate commands therefore pair it with `contain_pid_namespace`;
/// killing namespace PID 1 makes the kernel reap the entire descendant tree.
/// Other sandbox call sites use their own PID-1 parent-liveness handshake.
pub fn die_with_parent(cmd: &mut Command) {
    // Captured HERE, in the parent, rather than read once the fork has happened:
    // a getppid taken in the child cannot tell "my parent" from "the reaper that
    // already adopted me", so a parent dying anywhere in the fork→pre_exec path
    // would leave both reads agreeing and PDEATHSIG armed against the reaper —
    // the child running on orphaned, which is the case this exists to prevent.
    let parent = i64::from(std::process::id());
    // Post-fork safe: set_pdeathsig/getppid are raw syscalls, and
    // from_raw_os_error packs an errno without allocating.
    unsafe {
        cmd.pre_exec(move || {
            sys::set_pdeathsig(sys::SIGKILL)?;
            if sys::getppid() != parent {
                // Fail the SPAWN rather than exiting 0: std reads a clean child
                // exit here as "exec succeeded", so the gate would wait(0),
                // report Passed and be JOURNALLED GREEN for --resume having
                // never run its body.
                return Err(io::Error::from_raw_os_error(ESRCH));
            }
            Ok(())
        });
    }
}

/// Make `cmd` PID 1 of a fresh rootless PID namespace while preserving the
/// caller-visible child and exit status.
///
/// The `Command` child first creates the namespaces, then forks once. Its
/// child becomes PID 1 and execs the requested program; the outer half only
/// waits and mirrors the status. Linux tears down every other namespace member
/// when PID 1 exits, including double-forked descendants that changed process
/// group or session. This turns a check-host or gate permit into a real process
/// lifetime boundary without a privileged cgroup.
pub fn contain_pid_namespace(cmd: &mut Command) -> io::Result<()> {
    let host_uid = sys::getuid();
    let host_gid = sys::getgid();
    let root = CString::new("/").map_err(io::Error::other)?;
    let proc_path = CString::new("/proc").map_err(io::Error::other)?;
    let proc_type = CString::new("proc").map_err(io::Error::other)?;
    unsafe {
        cmd.pre_exec(move || {
            sys::unshare(sys::CLONE_NEWUSER | sys::CLONE_NEWNS | sys::CLONE_NEWPID)
                .map_err(|e| {
                    sys::warn(b"td-builder check: FAILED creating the PID lifetime namespace\n");
                    e
                })?;
            map_userns_id(host_uid, host_gid, host_uid, host_gid).map_err(|e| {
                sys::warn(b"td-builder check: FAILED mapping the PID namespace identity\n");
                e
            })?;
            sys::mount(None, &root, None, sys::MS_REC | sys::MS_PRIVATE, None).map_err(|e| {
                sys::warn(b"td-builder check: FAILED privatizing PID namespace mounts\n");
                e
            })?;
            let (live_r, live_w) = sys::pipe_liveness()?;
            let pid = sys::fork()?;
            if pid != 0 {
                let _ = sys::close(live_r);
                let status = sys::waitpid(pid)?;
                let code = if status & 0x7f == 0 {
                    (status >> 8) & 0xff
                } else {
                    128 + (status & 0x7f)
                };
                sys::exit_group(code);
            }
            sys::set_pdeathsig(sys::SIGKILL)?;
            pid1_confirm_parent(live_r, live_w)?;
            sys::mount(
                Some(&proc_type),
                &proc_path,
                Some(&proc_type),
                0,
                None,
            )
            .map_err(|e| {
                sys::warn(b"td-builder check: FAILED mounting the PID namespace procfs\n");
                e
            })
        });
    }
    Ok(())
}

/// Executed only by the hidden `check-pidns-run` wrapper (or its unit-test
/// surrogate). Keeping the namespace setup in a process that has already
/// exec'd is important: its caller can start RSS/deadline monitoring as soon as
/// this wrapper exists, while the wrapper waits for namespace PID 1 below.
pub fn pid_namespace_status(args: &[String]) -> io::Result<ExitStatus> {
    let program = args
        .first()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "PID namespace has no command"))?;
    let mut command = Command::new(program);
    command.args(args.get(1..).unwrap_or_default());
    die_with_parent(&mut command);
    contain_pid_namespace(&mut command)?;
    command.status()
}

pub fn pid_namespace_cli(args: &[String]) -> ExitCode {
    match pid_namespace_status(args) {
        Ok(status) => {
            use std::os::unix::process::ExitStatusExt as _;
            let code = status.code().unwrap_or_else(|| {
                status
                    .signal()
                    .map(|signal| 128i32.saturating_add(signal))
                    .unwrap_or(i32::from(u8::MAX))
            });
            ExitCode::from(u8::try_from(code).unwrap_or(u8::MAX))
        }
        Err(e) => {
            eprintln!("td-builder: cannot run check PID namespace: {e}");
            ExitCode::FAILURE
        }
    }
}

/// ESRCH, "no such process" — what every bail on a vanished parent reports.
/// Spelled here rather than taken from libc: this workspace carries zero
/// dependencies.
const ESRCH: i32 = 3;

/// PID 1's half of the parent-liveness handshake, run just after it re-arms
/// PR_SET_PDEATHSIG. Arming happens after a fork, so a parent killed while the
/// child was still unscheduled is never signalled; PID 1 cannot fall back on
/// `getppid` either, because its parent is in an ancestor PID namespace where
/// the kernel reports 0. So it drops its own copy of the write end and reads
/// the pipe: EOF means every other holder is gone. Arm-then-read leaves no
/// window — a death before the read is EOF, one after is a signal.
///
/// Post-fork safe: three raw syscalls, and `from_raw_os_error` packs an errno
/// without allocating.
fn pid1_confirm_parent(live_r: i32, live_w: i32) -> io::Result<()> {
    // Our own copy first, or the read below can never see EOF: this process
    // would be the writer it is waiting to outlive. Load-bearing enough to
    // report — a write end left open answers "alive" forever, which is the
    // check silently becoming a no-op.
    sys::close(live_w)
        .map_err(|e| { sys::warn(b"td-builder sandbox: FAILED closing the liveness write end\n"); e })?;
    // An unreadable channel is NOT a dead parent, so its errno propagates
    // rather than taking the bail below.
    if sys::pipe_peer_open(live_r)
        .map_err(|e| { sys::warn(b"td-builder sandbox: FAILED reading the parent-liveness pipe\n"); e })?
    {
        return Ok(());
    }
    sys::warn(b"td-builder sandbox: parent died before pid 1 armed PR_SET_PDEATHSIG\n");
    Err(io::Error::from_raw_os_error(ESRCH))
}

/// WHO issued the authority for a staged input's hash — its provenance CLASS
/// (re #469). Integrity and provenance are distinct: integrity is "the bytes
/// match a recorded hash"; provenance is "the authority that recorded that
/// hash is allowed to do so". An `InputOrigin` is constructed ONLY at the
/// planner's typed db-intake sites — the plan's seed db, a prior step's
/// td.db, a td-interned source/vendor placement db, the stage0 builder
/// placement db — each declared with its class in code where the planner
/// hands it over. A raw path, environment variable, database row, or cache
/// file can locate bytes, but no production function turns one directly into
/// an `InputOrigin`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InputOrigin {
    /// A pinned, hash-verified seed registration: the interned seed db whose
    /// entries the compiled seed-digest table gated at synthesis, or a
    /// td-interned source/vendored-crate placement db (a declared
    /// fixed-output fetch td restored itself).
    AuditedSeed,
    /// A prior td recipe output: a build-plan step's td.db row, written by
    /// the engine after that step's own verified build (deriver recorded).
    RecipeOutput,
    /// The control-plane td-builder staged as the drv's builder — the stage0
    /// placement db `store-add-builder` wrote for the binary driving this
    /// build.
    ControlPlaneBuilder,
}

impl InputOrigin {
    /// The audit token (`provenance.manifest`'s origin column).
    pub fn as_str(self) -> &'static str {
        match self {
            InputOrigin::AuditedSeed => "audited-seed",
            InputOrigin::RecipeOutput => "recipe-output",
            InputOrigin::ControlPlaneBuilder => "control-plane-builder",
        }
    }
}

/// One staged input's authority record: the expected NAR hash
/// (`sha256:<hex>`, the `ValidPaths.hash` wire format) plus WHO issued it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct StagedInput {
    pub nar_hash: String,
    pub origin: InputOrigin,
}

/// The staged-input provenance manifest (re #469): canonical store path →
/// (expected NAR hash, origin class), assembled by the planner from TYPED
/// td-owned store DBs ONLY (interned-seed registrations, prior build-plan
/// steps' td.dbs, the source/builder placement dbs). EVERY build carries one —
/// there is no non-strict mode: each closure item must have a record and its
/// on-disk bytes must hash to it, or it refuses to stage. A caller-supplied
/// store DIRECTORY is thereby a byte source, never an authority: the bytes
/// bind only if a td-owned registration vouches for them.
///
/// Cost, decided deliberately: verification re-hashes every closure item at
/// EVERY strict step — O(closure bytes) per step, not amortized. The bootstrap
/// rungs are small, a streaming SHA-256 is cheap next to the build it guards,
/// and trusting a prior step's verification is exactly the assume-the-cache
/// hole this manifest exists to close.
pub type StageManifest = std::collections::BTreeMap<String, StagedInput>;

/// Stream-hash a tree/file in NAR form — `sha256:<hex>`, the `ValidPaths.hash`
/// wire format every td store registration records. `pub(crate)` because the
/// loop-userland cache (check_loop.rs) verifies its durable items with the
/// same hash before mounting them.
pub(crate) fn nar_hash_of(path: &Path) -> io::Result<String> {
    struct W(crate::sha256::Sha256);
    impl io::Write for W {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.update(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut w = W(crate::sha256::Sha256::new());
    crate::nar::write_nar(&mut w, path)?;
    Ok(format!("sha256:{}", crate::sha256::to_base16(&w.0.finalize())))
}

/// Verify ONE closure item against the provenance manifest — split out of
/// `build` so the rejection paths unit-test without a namespace. Refuses (a)
/// an item no td-owned db vouches for and (b) on-disk bytes that do not hash
/// to the recorded NAR hash.
pub fn verify_staged_item(
    manifest: &StageManifest,
    canonical: &str,
    on_disk: &str,
) -> io::Result<()> {
    let Some(want) = manifest.get(canonical) else {
        return Err(err(format!(
            "provenance rejected: closure item {canonical} has no td-owned store-db record — refusing to stage it (re #469)"
        )));
    };
    let got = nar_hash_of(Path::new(on_disk))?;
    if got != want.nar_hash {
        return Err(err(format!(
            "provenance rejected: closure item {canonical} (on disk {on_disk}) hashes {got} but its td-owned registration ({}) records {} — refusing to stage tampered bytes (re #469)",
            want.origin.as_str(),
            want.nar_hash
        )));
    }
    Ok(())
}

/// Plan (and materialise the newstore skeleton for) ONE verified closure item's
/// bind mounts — the `(on-disk source, in-newstore target)` pairs the child will
/// `mount --bind`, basename-keyed under `newstore/<store-hash>`. Every item — a
/// declared input, a source, or the builder tree — binds WHOLE. Split out of
/// `build` so the layout unit-tests without a namespace.
fn plan_staged_item(
    newstore: &Path,
    canonical: &str,
    on_disk: &str,
    meta: &fs::Metadata,
) -> io::Result<Vec<(String, PathBuf)>> {
    // BASENAME-keyed: a closure can span MULTIPLE store prefixes (/gnu/store deps +
    // /td/store td-built deps, e.g. a chained toolchain — brick 8). Each item is staged
    // flat under newstore/<base> (store hashes are unique); newstore is then mounted at
    // EVERY prefix the closure spans below, so /gnu/store/<b> and /td/store/<b> both
    // resolve to their item. For a single-prefix closure this is exactly the old layout.
    let base = canonical
        .rsplit('/')
        .next()
        .filter(|b| !b.is_empty())
        .ok_or_else(|| err(format!("closure item {canonical}: not a store path")))?;
    let target = newstore.join(base);
    if meta.is_dir() {
        fs::create_dir_all(&target)?;
    } else if meta.is_file() {
        fs::File::create(&target)?;
    } else {
        // A symlink cannot be bind-mounted; no pinned-channel closure
        // has top-level symlink store items — refuse rather than guess.
        return Err(err(format!("closure item {canonical}: unsupported file type")));
    }
    Ok(vec![(on_disk.to_string(), target)])
}

/// The store paths a drv declared through the DATA channel (APPLICATIONS.md §B.8),
/// read out of `TD_PAYLOAD_MAP` in the drv's own env.
///
/// ABSENT is none; PRESENT AND MALFORMED is an ERROR. The two are
/// indistinguishable in the result, and reading a corrupt map as "no payloads"
/// would mount a payload executable on a JSON slip.
///
/// Only the DECLARED payloads, not their closure: §B.8 rests on a payload being
/// self-contained, and one that dragged td-built dependencies in would leave those
/// binds executable.
fn payload_paths(drv: &Derivation) -> io::Result<Vec<String>> {
    let Some((_, map)) = drv.env.iter().find(|(k, _)| k == "TD_PAYLOAD_MAP") else {
        return Ok(Vec::new());
    };
    let parsed = crate::json::parse(map)
        .map_err(|e| err(format!("TD_PAYLOAD_MAP is not JSON ({e}): {map}")))?;
    let crate::json::Json::Obj(kvs) = parsed else {
        return Err(err(format!("TD_PAYLOAD_MAP is not a JSON object: {map}")));
    };
    let mut paths = Vec::with_capacity(kvs.len());
    for (name, value) in &kvs {
        let path = value
            .as_str()
            .ok_or_else(|| err(format!("TD_PAYLOAD_MAP entry `{name}' is not a string")))?;
        paths.push(path.to_string());
    }
    Ok(paths)
}

/// The remount flags an item takes BEYOND the read-only lock every closure item
/// gets. Keyed on CANONICAL — the path the build sees, and the one the payload map
/// names — never on the on-disk path, which differs for a td-interned item and
/// would silently match nothing.
fn extra_bind_flags(payloads: &[String], canonical: &str) -> usize {
    if payloads.iter().any(|p| p == canonical) {
        sys::MS_NOEXEC
    } else {
        0
    }
}

/// The whole flag PLAN for a closure: one `(canonical, on_disk, extra)` per entry,
/// plus the two refusals that need the whole closure in view.
///
/// Takes the DERIVATION rather than a payload list because the staging loop needs
/// a namespace and so cannot be tested: a payload argument there is one an
/// untestable caller could pass empty. Pure otherwise, so the flag a payload gets
/// is a value an assertion can hold.
fn plan_bind_flags<'a>(
    drv: &Derivation,
    closure: &'a [String],
) -> io::Result<Vec<(&'a str, &'a str, usize)>> {
    // The DATA channel's paths, read off the drv's own env — hashed drv data, so
    // the set cannot change without changing the derivation.
    let payloads = &payload_paths(drv)?;
    let mut plan = Vec::with_capacity(closure.len());
    for entry in closure {
        let (canonical, on_disk) = split_closure_entry(entry);
        plan.push((canonical, on_disk, extra_bind_flags(payloads, canonical)));
    }
    // Items are staged flat under `newstore/<basename>`, so two entries sharing a
    // basename bind onto ONE target and the second stacks over the first. That
    // shadowing predates the payload channel; what is new is that it can drop a
    // restriction, so the case where the two disagree about flags is refused.
    for (i, (canonical, _, extra)) in plan.iter().enumerate() {
        let base = canonical.rsplit('/').next().unwrap_or(canonical);
        for (other, _, other_extra) in plan.iter().skip(i + 1) {
            if other.rsplit('/').next().unwrap_or(other) == base && other_extra != extra {
                return Err(err(format!(
                    "closure items {canonical} and {other} stage onto the same name but \
                     take different mount flags — the later bind would shadow the \
                     earlier one's restriction (APPLICATIONS.md section B.8)"
                )));
            }
        }
    }
    // A payload the closure never offered is a restriction that matched NOTHING,
    // and nothing observable would distinguish that from one that applied: the
    // build runs with every bind executable and exits 0. Same argument as
    // `losetup`'s read-only readback. The two sets really can diverge — the maps
    // are built from the parsed lock while the closure comes from the drv's
    // input-srcs, which `substitute_gcc_toolchain` rewrites after that parse.
    for path in payloads {
        if !plan.iter().any(|(canonical, _, _)| canonical == path) {
            return Err(err(format!(
                "TD_PAYLOAD_MAP names {path}, which is not in this build's input \
                 closure — the noexec restriction would apply to nothing and the \
                 build would look identical (APPLICATIONS.md section B.8)"
            )));
        }
    }
    Ok(plan)
}

/// The flag word the child's SECOND mount issues — the remount that locks a staged
/// item. A function so a test can hold it: re-composing the same `|` chain in an
/// assertion observes nothing. The creating BIND deliberately does not take it,
/// since bind creation ignores the flag word entirely.
fn remount_flags(extra: usize) -> usize {
    sys::MS_REMOUNT | sys::MS_BIND | sys::MS_RDONLY | extra
}

/// Run the drv's builder inside the namespace sandbox. `closure` lists every
/// store path the build may see (the staged store's contents); `scratch` is
/// a writable host directory. `manifest` is the #469 staging gate — REQUIRED,
/// not optional: no engine path may stage inputs a td-owned db does not vouch
/// for. Every closure item is provenance-verified (`verify_staged_item`)
/// BEFORE its bind target is staged; the item binds are then locked
/// READ-ONLY in the child, so the verified bytes cannot be rewritten through
/// a live bind for the build's duration (the hash runs in the parent and the
/// mount in the child, so the lock — not the hash — is what holds after
/// staging). On success returns (output name, host-side path under
/// scratch/newstore) for every drv output, each verified to exist.
pub fn build(
    drv: &Derivation,
    drv_path: &str,
    closure: &[String],
    scratch: &Path,
    manifest: &StageManifest,
) -> io::Result<Vec<(String, PathBuf)>> {
    if drv.platform != "x86_64-linux" {
        return Err(err(format!(
            "platform `{}' is not x86_64-linux — refusing to build",
            drv.platform
        )));
    }
    fs::create_dir_all(scratch)?;
    require_disk_backed(scratch)?;

    // The active store dir (default /td/store; overridden by TD_STORE_DIR). Every
    // closure path the build SEES is under this prefix, the new root mounts its store
    // here, and NIX_STORE points at it — so a /td/store build is native, not rewritten.
    let store_dir_str = crate::store::store_dir();
    let store_prefix = format!("{store_dir_str}/");

    // Stage the bind targets in the parent (plain file ops on our scratch);
    // the mounts themselves happen in the child's namespace.
    let newstore = scratch.join("newstore");
    fs::create_dir_all(&newstore)?;
    let plan = plan_bind_flags(drv, closure)?;
    let mut binds: Vec<(CString, CString, usize)> = Vec::with_capacity(closure.len());
    // CANONICAL is the store path the build SEES; ON-DISK is where to bind FROM
    // (== canonical for daemon-resident items, a td store dir for td-interned ones).
    for (canonical, on_disk, extra) in plan {
        verify_staged_item(manifest, canonical, on_disk)?;
        let meta = fs::symlink_metadata(on_disk)
            .map_err(|e| err(format!("closure item {canonical} (on disk {on_disk}): {e}")))?;
        for (src, dst) in plan_staged_item(&newstore, canonical, on_disk, &meta)? {
            binds.push((
                CString::new(src.as_str()).map_err(|_| err(format!("{src}: NUL in path")))?,
                CString::new(dst.as_os_str().as_encoded_bytes())
                    .map_err(|_| err(format!("{}: NUL in path", dst.display())))?,
                extra,
            ));
        }
    }

    // The build dir is `guix-build-<drvName>-0`. For a store-path drv that is
    // storePathToName(drvPath). For an emitted `.drv` handed in from outside the
    // store (td-drv-build builds the file td WROTE), derive the same name from the
    // first output's store name + ".drv" (drvName == outName + ".drv" for these
    // single-output subjects). Store-path inputs (the td-builder rung) are
    // unaffected — the first branch still wins.
    let drv_name = match store_path_name(drv_path) {
        Ok(n) => n.to_string(),
        Err(_) => {
            let out0 = drv
                .outputs
                .first()
                .ok_or_else(|| err("derivation has no outputs".into()))?;
            format!("{}.drv", store_path_name(&out0.path)?)
        }
    };
    let build_dir = format!("/tmp/guix-build-{}-0", drv_name);
    let host_uid = sys::getuid();
    let host_gid = sys::getgid();

    // A fresh tmpfs becomes the build's MINIMAL root: the staged /gnu/store, a
    // writable /tmp, a minimal /dev, a fresh /proc and a minimal /etc — and
    // NOTHING ELSE of the host filesystem. Without this pivot the build inherited
    // the invoking root (only /gnu/store + /tmp overlaid), so /etc, /home, /usr …
    // leaked in and the build was hermetic ONLY when wrapped in the outer
    // host-sandbox. Pivoting here makes `build` SELF-hermetic (own-builder-daemon
    // track). The build now also unshares NEWPID and forks the builder to PID 1 of
    // its own pid namespace; the /proc mounted below is a FRESH procfs reflecting
    // that namespace, not the invoking one.
    let newroot = scratch.join("buildroot");
    fs::create_dir_all(&newroot)?;
    let cstr = |p: &Path| CString::new(p.as_os_str().as_encoded_bytes()).unwrap();
    let newstore_c = cstr(&newstore);
    let root_c = CString::new("/").unwrap();
    let tmpfs_c = CString::new("tmpfs").unwrap();
    let procfs_c = CString::new("proc").unwrap();
    let newroot_c = cstr(&newroot);
    // The store dir INSIDE the new root, e.g. <newroot>/gnu/store or <newroot>/td/store
    // (store_dir_str is absolute; strip the leading '/' to make it root-relative).
    let store_dir = newroot.join(store_dir_str.trim_start_matches('/'));
    let store_dir_c = cstr(&store_dir);
    let tmp_dir = newroot.join("tmp");
    let tmp_dir_c = cstr(&tmp_dir);
    let build_tmp = scratch.join("build-tmp");
    let build_tmp_lease_path = scratch.join(".build-tmp.lock");
    let build_tmp_lease = fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&build_tmp_lease_path)?;
    build_tmp_lease.lock()?;
    remove_scratch_tree(&build_tmp)?;
    fs::create_dir_all(&build_tmp)?;
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&build_tmp, fs::Permissions::from_mode(0o1777))?;
    }
    let build_tmp_cleanup = ScratchCleanup {
        path: build_tmp.clone(),
        _lease: build_tmp_lease,
    };
    let build_tmp_c = cstr(&build_tmp);
    let dev_dir = newroot.join("dev");
    let dev_dir_c = cstr(&dev_dir);
    let proc_dir = newroot.join("proc");
    let proc_dir_c = cstr(&proc_dir);
    let etc_dir = newroot.join("etc");
    let etc_passwd = etc_dir.join("passwd");
    let etc_group = etc_dir.join("group");
    let oldroot_rel = newroot.join("oldroot");
    let oldroot_rel_c = cstr(&oldroot_rel);
    let oldroot_abs_c = CString::new("/oldroot").unwrap();
    // EXTRA store prefixes the closure spans beyond the active one (e.g. /td/store toolchain
    // inputs in a /gnu/store-native corpus build — brick 8). newstore is rbind'd at each of
    // these too, so those inputs are visible at their canonical prefix. Empty for the common
    // single-store build → the mount sequence below is unchanged.
    let mut extra_prefixes: Vec<String> = closure
        .iter()
        .map(|e| split_closure_entry(e).0)
        .filter_map(|c| c.rsplit_once('/').map(|(d, _)| d.to_string()))
        .filter(|d| *d != store_dir_str)
        .collect();
    extra_prefixes.sort();
    extra_prefixes.dedup();
    let extra_store_dirs: Vec<PathBuf> = extra_prefixes
        .iter()
        .map(|p| newroot.join(p.trim_start_matches('/')))
        .collect();
    let extra_store_cs: Vec<CString> = extra_store_dirs.iter().map(|d| cstr(d)).collect();
    // /dev is rbind'd whole from the invoking namespace rather than rebuilt node by
    // node: re-binding a device node onto a fresh unprivileged-userns tmpfs strips
    // device access (the re-bound /dev/null returns EACCES on write), whereas an
    // rbind preserves the source mount's working device binds. In the loop the
    // source is host_shell's ALREADY-minimal /dev (null/zero/…/shm/pts, no host
    // device tree); a future standalone daemon would reuse that minimal-/dev builder.
    let dev_src_c = CString::new("/dev").unwrap();
    // Minimal /etc (daemon build-chroot parity): passwd + group so getpwuid/getgrgid
    // resolve the build user, with NO host /etc reachable.
    let passwd_body = format!(
        "root:x:0:0:System administrator:/:/noshell\n\
         nixbld:x:{GUEST_UID}:{GUEST_GID}:Build user:/build-top:/noshell\n\
         nobody:x:65534:65534:Nobody:/:/noshell\n"
    );
    let group_body = format!("root:x:0:\nnixbld:x:{GUEST_GID}:\nnogroup:x:65534:\n");
    let build_dir_owned = build_dir.clone();
    let mesboot_steps_file =
        PathBuf::from(&build_dir).join(crate::build::MESBOOT_STEPS_FILE);

    let inherited_job_budget = crate::check_memory::request_job_budget()
        .map(|bytes| std::ffi::OsString::from(bytes.to_string()));
    let mut cmd = Command::new(&drv.builder);
    cmd.args(&drv.args);
    cmd.env_clear();
    // build.cc's exact assembly order; Command's env map gives the same
    // override semantics (later set wins).
    cmd.env("PATH", "/path-not-set");
    cmd.env("HOME", "/homeless-shelter");
    cmd.env("NIX_STORE", &store_dir_str);
    cmd.env(
        "NIX_BUILD_CORES",
        crate::check_memory::build_jobs().to_string(),
    );
    let mesboot_steps = configure_builder_env(
        &mut cmd,
        &drv.builder,
        &drv.args,
        &drv.env,
        &mesboot_steps_file,
    )?;
    forward_trusted_check_policy(
        &mut cmd,
        &drv.builder,
        inherited_job_budget.as_deref(),
    );
    for k in ["NIX_BUILD_TOP", "TMPDIR", "TEMPDIR", "TMP", "TEMP", "PWD"] {
        cmd.env(k, &build_dir);
    }

    // Captured in the PARENT: a `getppid` taken in the CHILD cannot tell "my
    // parent" from "the reaper that already adopted me", so both reads agree
    // however early the parent died.
    let outer_parent = i64::from(std::process::id());

    unsafe {
        cmd.pre_exec(move || {
            // Arm parent-death reaping before anything else: if the outer
            // td-builder dies during setup, this process is SIGKILLed rather than
            // left running. (Still in the outer PID namespace here, so getppid is
            // meaningful; the re-check closes the parent-died-mid-setup race.)
            sys::set_pdeathsig(sys::SIGKILL)?;
            if sys::getppid() != outer_parent {
                // Fail the SPAWN rather than exiting 0: std reads a clean child
                // exit here as "exec succeeded", so the caller would take an
                // orphan-avoidance bail for a build that ran.
                return Err(io::Error::from_raw_os_error(ESRCH));
            }
            // New USER + PID + mount + net + IPC + UTS namespaces. NEWPID rides in
            // the SAME unshare as NEWUSER so the new PID namespace is owned by the
            // new user namespace; the fork below then lands the builder at PID 1 of
            // that namespace, where a fresh /proc reflects only the build's own
            // process tree — the host's processes (the daemon, other concurrent
            // builds, their /proc/<pid>/environ) are no longer visible or signalable.
            sys::unshare(
                sys::CLONE_NEWUSER
                    | sys::CLONE_NEWNS
                    | sys::CLONE_NEWPID
                    | sys::CLONE_NEWNET
                    | sys::CLONE_NEWIPC
                    | sys::CLONE_NEWUTS,
            )?;
            // Map the guest ids before touching anything else so file
            // creation below happens as 30001/30000, not the overflow id.
            map_userns_id(host_uid, host_gid, GUEST_UID, GUEST_GID)?;
            // Fork: the child is PID 1 of the new PID namespace and does the mount
            // setup + (via std) exec of the builder; THIS process (the PID-ns
            // parent, still in the outer PID ns) only waits for it and propagates
            // its exit. It must NOT fall through to std's exec path — the builder is
            // exec'd exactly once, as PID 1. Stdio is inherited, so output streams.
            // Created BEFORE the fork so both ends are inherited; this process
            // then holds the write end for the rest of its life, and its death
            // — however abrupt — closes it. See `pid1_confirm_parent`.
            let (live_r, live_w) = sys::pipe_liveness()?;
            let pid = sys::fork()?;
            if pid != 0 {
                // Only the read end is surplus here; the write end is the thing
                // PID 1 watches, and must stay open for this process's life.
                let _ = sys::close(live_r);
                let status = sys::waitpid(pid)?;
                let code = if status & 0x7f == 0 {
                    (status >> 8) & 0xff
                } else {
                    128 + (status & 0x7f)
                };
                sys::exit_group(code);
            }
            // --- PID 1 of the new PID namespace from here on ---
            // Re-arm parent-death reaping (fork cleared it): if the PID-ns parent
            // waiting above dies, PID 1 is SIGKILLed and the kernel tears down the
            // whole namespace, reaping the build. PDEATHSIG survives the execve.
            sys::set_pdeathsig(sys::SIGKILL)?;
            // Then check the parent outlived the fork: arming after one leaves a
            // window where nothing would reap this process, and an orphan here is
            // a whole build tree.
            pid1_confirm_parent(live_r, live_w)?;
            // Keep every mount below private to this namespace.
            sys::mount(None, &root_c, None, sys::MS_REC | sys::MS_PRIVATE, None)?;
            // Stage each closure item into newstore (host scratch, OUTSIDE the new
            // root), then rbind newstore over the new root's /gnu/store below.
            // Each INPUT bind is locked read-only immediately (remount of the
            // bind just created — load-bearing, so a failure is fatal): the
            // builder runs as the mapped owner uid and could otherwise write
            // straight through the live bind into the on-disk store the
            // manifest verified at staging, so the verify-then-bind boundary
            // would hold only for an instant (re #469). newstore itself stays
            // writable — outputs land as NEW entries beside the binds, never
            // through one — and the /gnu/store rbind below carries each
            // child's ro flag along.
            for (src, dst, extra) in &binds {
                sys::mount(Some(src), dst, None, sys::MS_BIND, None)?;
                sys::mount(None, dst, None, remount_flags(*extra), None)?;
            }
            // The fresh minimal root, then its skeleton dirs.
            sys::mount(Some(&tmpfs_c), &newroot_c, Some(&tmpfs_c), 0, None)?;
            fs::create_dir_all(&store_dir)?;
            fs::create_dir_all(&tmp_dir)?;
            fs::create_dir_all(&dev_dir)?;
            fs::create_dir_all(&proc_dir)?;
            fs::create_dir_all(&etc_dir)?;
            fs::create_dir_all(&oldroot_rel)?;
            // Staged store → /gnu/store (rbind carries the per-item binds); outputs
            // the build writes under /gnu/store land in newstore on the host.
            sys::mount(Some(&newstore_c), &store_dir_c, None, sys::MS_BIND | sys::MS_REC, None)?;
            // … and at every EXTRA prefix the closure spans (e.g. /td/store toolchain inputs):
            // the SAME newstore (basename-keyed) rbind'd there too, so those canonical paths
            // resolve. Empty for a single-store build, so this is a no-op in the common case.
            for (i, dst) in extra_store_cs.iter().enumerate() {
                fs::create_dir_all(&extra_store_dirs[i])?;
                sys::mount(Some(&newstore_c), dst, None, sys::MS_BIND | sys::MS_REC, None)?;
            }
            // Keep large source and object trees on private disk-backed scratch,
            // rather than charging them as tmpfs memory.
            sys::mount(
                Some(&build_tmp_c),
                &tmp_dir_c,
                None,
                sys::MS_BIND | sys::MS_REC,
                None,
            )?;
            // /dev rbind'd whole (preserves working device binds; see note above).
            sys::mount(Some(&dev_src_c), &dev_dir_c, None, sys::MS_BIND | sys::MS_REC, None)?;
            // A FRESH procfs reflecting the build's OWN pid namespace (we are PID 1),
            // not the invoking namespace's /proc.
            sys::mount(Some(&procfs_c), &proc_dir_c, Some(&procfs_c), 0, None)?;
            // Minimal /etc.
            fs::write(&etc_passwd, &passwd_body)?;
            fs::write(&etc_group, &group_body)?;
            // Pivot into the minimal root and drop the host root entirely.
            sys::pivot_root(&newroot_c, &oldroot_rel_c)?;
            std::env::set_current_dir("/")?;
            sys::umount2(&oldroot_abs_c, sys::MNT_DETACH)?;
            let _ = fs::remove_dir("/oldroot");
            // The build dir lives on the private disk-backed /tmp bind.
            fs::DirBuilder::new().mode(0o700).create(&build_dir_owned)?;
            write_mesboot_steps_file(&mesboot_steps_file, mesboot_steps.as_deref())?;
            std::env::set_current_dir(&build_dir_owned)?;
            Ok(())
        });
    }

    let status = cmd.status();
    drop(build_tmp_cleanup);
    let status = status.map_err(|e| err(format!("spawning builder {}: {e}", drv.builder)))?;
    if !status.success() {
        return Err(err(format!(
            "builder for {drv_path} failed: {status}"
        )));
    }

    let mut outputs = Vec::with_capacity(drv.outputs.len());
    for o in &drv.outputs {
        let host = newstore.join(
            o.path
                .strip_prefix(&store_prefix)
                .ok_or_else(|| err(format!("output {}: not a store path", o.path)))?,
        );
        fs::symlink_metadata(&host).map_err(|_| {
            err(format!(
                "builder exited 0 but did not produce output `{}' ({})",
                o.name, o.path
            ))
        })?;
        outputs.push((o.name.clone(), host));
    }
    // `cmd` is the outer trampoline that waits for PID 1. Namespace teardown
    // prevents package code from running before the trampoline returns here.
    finalize_application_output(drv, &outputs)?;
    Ok(outputs)
}

/// A host path to expose inside the loop sandbox (rbind-mounted at the same
/// path in the new root). `src` may be a directory or a regular file — the
/// mountpoint is created to match (a file store item, e.g. a pinned `.crate`,
/// binds onto a created empty file). `readonly` remounts it read-only after
/// binding.
pub struct Bind {
    pub src: String,
    /// Mount `src` at this absolute path inside the new root instead of at `src`
    /// (None ⇒ same path). Lets the user store at a host path (e.g. `~/.td/store`)
    /// appear at td's store prefix (`/td/store`) inside the sandbox — the
    /// own-root/store-ns case, breaking from guix's `/gnu/store`.
    pub dest: Option<String>,
    pub readonly: bool,
    /// When `readonly`, tolerate a FAILED read-only remount by DETACHING the bind
    /// (fail closed — no host-owned subtree left writable in the sandbox) instead
    /// of erroring. Set ONLY for defense-in-depth ro binds the kernel may forbid
    /// remounting in a child user namespace because the mount is owned by the
    /// host user namespace.
    /// NEVER for binds whose read-only is load-bearing (the store): those still
    /// error on a failed remount.
    pub ro_optional: bool,
}

/// The loop-sandbox DEV-SHELL (vs. the build jail above): pivot into a fresh
/// tmpfs root that exposes ONLY `binds` (rbind, the same path inside), a
/// writable tmpfs at each of `tmpfs_dirs`, and a minimal synthetic `/dev` (the
/// standard char devices + shm + a private devpts + fd symlinks, matching
/// `guix shell -C` — NOT the host device tree); the host filesystem is otherwise
/// gone. `path_env` is the full PATH; an empty value stays empty. `home` is HOME;
/// `workdir` is the cwd to enter after pivot (empty → `/`); `extra_env` is
/// caller-preserved env (e.g. the `TD_SUBST_*`/`TD_DAEMON_*` knobs). Runs `cmd args` and
/// returns its exit status. Unshares
/// NEWUSER|NEWNS|NEWPID|NEWNET|NEWIPC|NEWUTS and runs the command as PID 1 of the
/// new PID namespace with a private /proc mounted by that PID-1 process — full
/// `guix shell -C` parity, so nested containers (the loop-sandbox/loop-rung
/// equivalence oracle, the rootless rung) can create their own PID ns + /proc.
/// uid/gid use the IDENTITY map (host uid → itself) so the host daemon's
/// peer-cred check still sees the real host uid, and its own network namespace
/// (loopback brought up) matches `guix shell -C`'s offline posture.
///
/// `ro_dirs`: absolute in-sandbox directories to lock READ-ONLY (a recursive
/// self-bind, then a non-recursive ro remount of the top mount) after all
/// binds land — the tmpfs dirs that HOLD per-item bind mountpoints (e.g. the
/// seed store dir holding `--store-item` mounts). The items' own bind mounts
/// ride along visible and keep their own ro state; the parent dir itself
/// rejects entry creation afterwards, so an ACCIDENTAL write can't plant a
/// sibling next to the declared inputs. Not a security boundary against a
/// hostile gate: the gate body owns the sandbox's user/mount namespaces
/// (CAP_SYS_ADMIN inside) and can over-mount the parent — same trust model as
/// every mount in this sandbox. A listed dir that no bind created is skipped.
#[allow(clippy::too_many_arguments)]
pub fn host_shell(
    cmd: &str,
    args: &[String],
    binds: &[Bind],
    tmpfs_dirs: &[String],
    path_env: &str,
    home: &str,
    workdir: &str,
    extra_env: &[(String, String)],
    ro_dirs: &[String],
    scratch: &Path,
) -> io::Result<std::process::ExitStatus> {
    let newroot = scratch.join("root");
    fs::create_dir_all(&newroot)?;
    let host_uid = sys::getuid();
    let host_gid = sys::getgid();

    // Precompute every CString in the parent (the child's pre_exec only does
    // syscalls + fs ops, mirroring `build` above).
    // tmpfs root/dirs are owned by uid 0 of the new userns by default; with the
    // identity uid map (below) that is unmapped, so set the owner explicitly to
    // the host uid/gid via the tmpfs `uid=/gid=` mount data — keeps the dirs
    // writable while the process stays the (non-root) host uid.
    let tmpfs_data = CString::new(format!("uid={host_uid},gid={host_gid}")).unwrap();
    let newroot_c = CString::new(newroot.as_os_str().as_encoded_bytes()).unwrap();
    let root_c = CString::new("/").unwrap();
    let tmpfs_c = CString::new("tmpfs").unwrap();
    // A FRESH procfs is mounted at <newroot>/proc by the PID-1 child (below), so
    // /proc reflects the sandbox's OWN PID namespace, not the host's. The host
    // /proc is no longer bound in (main.rs drops it from the exposure set).
    let proc_c = CString::new("proc").unwrap();
    let proc_target_dir = newroot.join("proc");
    let proc_target_c = CString::new(proc_target_dir.as_os_str().as_encoded_bytes()).unwrap();
    let oldroot_rel = newroot.join("oldroot");
    let oldroot_rel_c = CString::new(oldroot_rel.as_os_str().as_encoded_bytes()).unwrap();
    let oldroot_abs_c = CString::new("/oldroot").unwrap();

    // Everything the child's pre_exec needs per bind, precomputed in the
    // parent: the C paths, whether the source is a directory (the mountpoint
    // is created to MATCH — a regular-file source, e.g. a pinned `.crate`
    // store item, binds onto a created empty file, mirroring the mount
    // applet), and a NAMED failure line (the child can only sys::warn a
    // preformatted byte string — a bare "FAILED bind-mounting" with no path
    // made failures undiagnosable).
    struct BindSpec {
        src: CString,
        target_dir: PathBuf,
        target: CString,
        readonly: bool,
        ro_optional: bool,
        src_is_dir: bool,
        fail_msg: Vec<u8>,
    }
    let mut bind_specs: Vec<BindSpec> = Vec::with_capacity(binds.len());
    for b in binds {
        // Bind `src` at `dest` inside the new root (dest defaults to src).
        let inside = b.dest.as_deref().unwrap_or(&b.src);
        let target = newroot.join(inside.strip_prefix('/').unwrap_or(inside));
        bind_specs.push(BindSpec {
            src: CString::new(b.src.as_str())
                .map_err(|_| err(format!("{}: NUL in path", b.src)))?,
            target_dir: target.clone(),
            target: CString::new(target.as_os_str().as_encoded_bytes())
                .map_err(|_| err(format!("{}: NUL in path", target.display())))?,
            readonly: b.readonly,
            ro_optional: b.ro_optional,
            // An unreadable source keeps the directory default; the mount then
            // fails exactly as before, now with the path named.
            src_is_dir: fs::metadata(&b.src).map(|m| m.is_dir()).unwrap_or(true),
            fail_msg: format!(
                "td-builder host-sandbox: FAILED bind-mounting {} -> {inside}\n",
                b.src
            )
            .into_bytes(),
        });
    }
    // Read-only parent-dir remounts (see the doc comment): precomputed like the
    // binds — the in-sandbox path, its C string, and a named failure line.
    struct RoDirSpec {
        target_dir: PathBuf,
        target: CString,
        fail_msg: Vec<u8>,
    }
    let mut ro_dir_specs: Vec<RoDirSpec> = Vec::with_capacity(ro_dirs.len());
    for d in ro_dirs {
        let target = newroot.join(d.strip_prefix('/').unwrap_or(d));
        ro_dir_specs.push(RoDirSpec {
            target_dir: target.clone(),
            target: CString::new(target.as_os_str().as_encoded_bytes())
                .map_err(|_| err(format!("{}: NUL in path", target.display())))?,
            fail_msg: format!(
                "td-builder host-sandbox: FAILED ro-remounting the bind parent dir {d}\n"
            )
            .into_bytes(),
        });
    }
    // (target_dir, target_c) for each writable tmpfs mount.
    let mut tmpfs_specs: Vec<(PathBuf, CString)> = Vec::with_capacity(tmpfs_dirs.len());
    for d in tmpfs_dirs {
        let target = newroot.join(d.strip_prefix('/').unwrap_or(d));
        tmpfs_specs.push((
            target.clone(),
            CString::new(target.as_os_str().as_encoded_bytes())
                .map_err(|_| err(format!("{}: NUL in path", target.display())))?,
        ));
    }

    // Minimal /dev, precomputed. The old exposure rbind-mounted the WHOLE host
    // /dev read-write, leaking /dev/kmsg (kernel log), /dev/kvm, raw disks, input
    // devices and GPUs into the "hermetic" sandbox. Instead build a fresh tmpfs
    // populated with ONLY the device set `guix shell -C` exposes: the standard
    // char devices (BIND-mounted from the host — a child userns cannot mknod, so
    // only these named nodes are reachable), /dev/shm, a private devpts, and the
    // fd symlinks.
    let dev_dir = newroot.join("dev");
    let dev_dir_c = CString::new(dev_dir.as_os_str().as_encoded_bytes()).unwrap();
    let dev_data = CString::new(format!("uid={host_uid},gid={host_gid},mode=0755")).unwrap();
    let mut dev_node_specs: Vec<(CString, PathBuf, CString)> = Vec::new();
    for n in ["null", "zero", "full", "random", "urandom", "tty"] {
        let src = format!("/dev/{n}");
        if Path::new(&src).exists() {
            let target = dev_dir.join(n);
            dev_node_specs.push((
                CString::new(src).unwrap(),
                target.clone(),
                CString::new(target.as_os_str().as_encoded_bytes()).unwrap(),
            ));
        }
    }
    let dev_shm_dir = dev_dir.join("shm");
    let dev_shm_c = CString::new(dev_shm_dir.as_os_str().as_encoded_bytes()).unwrap();
    let dev_shm_data = CString::new(format!("uid={host_uid},gid={host_gid},mode=1777")).unwrap();
    let dev_pts_dir = dev_dir.join("pts");
    let dev_pts_c = CString::new(dev_pts_dir.as_os_str().as_encoded_bytes()).unwrap();
    let devpts_c = CString::new("devpts").unwrap();
    let devpts_data =
        CString::new(format!("newinstance,ptmxmode=0666,mode=0620,gid={host_gid}")).unwrap();
    // (symlink path under <newroot>/dev, its target). /dev/ptmx → the private pts
    // instance; the std-stream links point into the private /proc mounted below.
    let dev_symlinks: Vec<(PathBuf, &str)> = vec![
        (dev_dir.join("ptmx"), "pts/ptmx"),
        (dev_dir.join("fd"), "/proc/self/fd"),
        (dev_dir.join("stdin"), "/proc/self/fd/0"),
        (dev_dir.join("stdout"), "/proc/self/fd/1"),
        (dev_dir.join("stderr"), "/proc/self/fd/2"),
    ];

    let workdir = if workdir.is_empty() { "/" } else { workdir };
    let workdir_owned = workdir.to_string();

    let mut command = Command::new(cmd);
    command.args(args);
    command.env_clear();
    command.env("PATH", path_env);
    command.env("HOME", home);
    command.env("TMPDIR", "/tmp");
    command.env("TD_HOST_SANDBOX", "1");
    // Caller-preserved env (e.g. the TD_SUBST_*/TD_DAEMON_* knobs).
    for (k, v) in extra_env {
        command.env(k, v);
    }
    // Generic terminal/identity env the gate bodies may read (TERM for terminal
    // output; USER/LOGNAME for any per-user path). Harmless, and keeps behaviour
    // identical to the outer shell.
    for k in ["TERM", "USER", "LOGNAME"] {
        if let Ok(v) = std::env::var(k) {
            command.env(k, v);
        }
    }

    // Captured in the PARENT: a `getppid` taken in the CHILD cannot tell "my
    // parent" from "the reaper that already adopted me", so both reads agree
    // however early the parent died.
    let outer_parent = i64::from(std::process::id());

    unsafe {
        command.pre_exec(move || {
            // Arm parent-death reaping BEFORE anything else: if the outer
            // td-builder is killed (CI cancellation, a timeout, Ctrl-C) during or
            // after setup, this process is SIGKILLed instead of left running.
            // Re-checked just after against the pid the PARENT recorded, closing
            // the race where it died between the fork and the prctl. (This level
            // is still in the outer PID namespace, so getppid is meaningful.)
            sys::set_pdeathsig(sys::SIGKILL)
                .map_err(|e| { sys::warn(b"td-builder host-sandbox: FAILED arming PR_SET_PDEATHSIG\n"); e })?;
            if sys::getppid() != outer_parent {
                // Fail the SPAWN rather than exiting 0: std reads a clean child
                // exit here as "exec succeeded", so the caller would take an
                // orphan-avoidance bail for a sandbox that ran.
                sys::warn(b"td-builder host-sandbox: parent died before PR_SET_PDEATHSIG armed\n");
                return Err(io::Error::from_raw_os_error(ESRCH));
            }
            // New USER + PID + mount + net + IPC + UTS namespaces. NEWPID is in
            // the SAME unshare as NEWUSER so the new PID namespace is OWNED by the
            // new user namespace (the kernel applies NEWUSER first); the fork
            // below then lands the command at PID 1 of that PID namespace, where a
            // private /proc reflects it — full parity with `guix shell -C`, so
            // nested containers (the loop-sandbox/loop-rung equivalence oracle and
            // the rootless rung) can create their own PID ns + /proc instead of
            // tripping over the host's root-owned PID 1.
            sys::unshare(
                sys::CLONE_NEWUSER
                    | sys::CLONE_NEWNS
                    | sys::CLONE_NEWPID
                    | sys::CLONE_NEWNET
                    | sys::CLONE_NEWIPC
                    | sys::CLONE_NEWUTS,
            )
            .map_err(|e| {
                sys::warn(b"td-builder host-sandbox: FAILED at unshare(NEWUSER|NEWNS|NEWPID|NEWNET|NEWIPC|NEWUTS)\n");
                e
            })?;
            // IDENTITY map (host uid/gid → itself), exactly like `guix shell -C`:
            // the process stays the NON-root host uid inside, so file-permission
            // checks (e.g. sqlite's access(W_OK) on the root-owned store DB)
            // behave as on the host — a uid-0 map would make root bypass them and
            // then fail on the real write. tmpfs ownership is handled via the
            // `uid=/gid=` mount data instead. The daemon's SO_PEERCRED sees the
            // real host uid either way.
            map_userns_id(host_uid, host_gid, host_uid, host_gid)
                .map_err(|e| { sys::warn(b"td-builder host-sandbox: FAILED mapping the identity uid/gid\n"); e })?;
            // Own network namespace (offline by construction, like `guix shell
            // -C`); bring its loopback up to match that posture.
            sys::bring_loopback_up()
                .map_err(|e| { sys::warn(b"td-builder host-sandbox: FAILED bringing loopback up\n"); e })?;
            // Fork: the child is PID 1 of the new PID namespace and goes on to set
            // up the mounts + exec the command; THIS process (the PID-ns parent,
            // still in the outer PID ns) only waits for it and propagates its exit
            // via exit_group. It must NOT fall through to std's exec path — the
            // command is exec'd exactly once, as PID 1. Stdio is inherited
            // directly, so output still streams; only the exit status flows here.
            // Created BEFORE the fork so both ends are inherited; this process
            // then holds the write end for the rest of its life, and its death
            // — however abrupt — closes it. See `pid1_confirm_parent`.
            let (live_r, live_w) = sys::pipe_liveness()
                .map_err(|e| { sys::warn(b"td-builder host-sandbox: FAILED at pipe2\n"); e })?;
            let pid = sys::fork()
                .map_err(|e| { sys::warn(b"td-builder host-sandbox: FAILED at fork\n"); e })?;
            if pid != 0 {
                // Only the read end is surplus here; the write end is the thing
                // PID 1 is watching and must stay open for this process's life.
                let _ = sys::close(live_r);
                let status = sys::waitpid(pid)?;
                let code = if status & 0x7f == 0 {
                    (status >> 8) & 0xff
                } else {
                    128 + (status & 0x7f)
                };
                sys::exit_group(code);
            }
            // --- PID 1 of the new PID namespace, from here on ---
            // Re-arm parent-death reaping FIRST (fork cleared it): if the
            // PID-namespace parent — the process waitpid-ing us just above — dies,
            // we (PID 1) are SIGKILLed, and the kernel then tears down the whole
            // PID namespace, reaping every descendant build/mount. PDEATHSIG
            // survives the upcoming execve, so the exec'd command stays covered.
            sys::set_pdeathsig(sys::SIGKILL)
                .map_err(|e| { sys::warn(b"td-builder host-sandbox: FAILED re-arming PR_SET_PDEATHSIG in pid 1\n"); e })?;
            // Then check that the parent survived long enough for that arming
            // to mean anything: under load a forked child can sit unscheduled
            // while its parent is killed, and this process IS pid 1 of its own
            // namespace, so nothing would reap it — it would finish setup, exec
            // and leave the whole tree running.
            pid1_confirm_parent(live_r, live_w)?;
            // Everything below private to this namespace.
            sys::mount(None, &root_c, None, sys::MS_REC | sys::MS_PRIVATE, None)
                .map_err(|e| { sys::warn(b"td-builder host-sandbox: FAILED at mount(/, MS_REC|MS_PRIVATE)\n"); e })?;
            // A fresh tmpfs is the new root (also makes it a mount point, which
            // pivot_root requires), owned by the host uid/gid.
            sys::mount(Some(&tmpfs_c), &newroot_c, Some(&tmpfs_c), 0, Some(&tmpfs_data))
                .map_err(|e| { sys::warn(b"td-builder host-sandbox: FAILED mounting the tmpfs root\n"); e })?;
            // Expose each requested host path (rbind), read-only where asked.
            // The mountpoint matches the source kind: dir → dir, file → file.
            for spec in &bind_specs {
                if spec.src_is_dir {
                    fs::create_dir_all(&spec.target_dir)?;
                } else {
                    if let Some(parent) = spec.target_dir.parent() {
                        fs::create_dir_all(parent)?;
                    }
                    if !spec.target_dir.exists() {
                        fs::File::create(&spec.target_dir)?;
                    }
                }
                sys::mount(Some(&spec.src), &spec.target, None, sys::MS_BIND | sys::MS_REC, None)
                    .map_err(|e| { sys::warn(&spec.fail_msg); e })?;
                if spec.readonly {
                    let ro = sys::mount(
                        None,
                        &spec.target,
                        None,
                        sys::MS_REMOUNT | sys::MS_BIND | sys::MS_REC | sys::MS_RDONLY,
                        None,
                    );
                    // A child userns cannot always remount read-only a mount
                    // owned by the host userns. For ro_optional binds that
                    // failure detaches the bind; every load-bearing read-only
                    // bind (notably the store) remains fatal.
                    if let Err(e) = ro {
                        if spec.ro_optional {
                            // Rather than leave the host subtree writable inside
                            // the hermetic sandbox, detach it. The leftover empty
                            // mountpoint is harmless.
                            sys::warn(b"td-builder host-sandbox: ro-remount not permitted for an ro_optional bind; detached (fail-closed, no host exposure)\n");
                            let _ = sys::umount2(&spec.target, sys::MNT_DETACH);
                        } else {
                            sys::warn(&spec.fail_msg);
                            sys::warn(b"td-builder host-sandbox: (FAILED ro-remounting the exposed path above)\n");
                            return Err(e);
                        }
                    }
                }
            }
            // Lock each bind-holding parent dir read-only, AFTER every bind has
            // landed: a RECURSIVE self-bind (making the plain tmpfs dir its own
            // vfsmount) then a NON-recursive ro-remount of just that top mount —
            // the per-item child mounts ride along visible (already ro from
            // their own remounts), but creating a sibling entry in the dir
            // itself now fails EROFS. MS_REC is load-bearing: a NON-recursive
            // self-bind would clone only the top mount, SHADOWING every item
            // bind under it with the empty mountpoint dirs (review finding —
            // every store item would read as an empty dir). These dirs are
            // sandbox-owned tmpfs, so the remount is never the host-owned-EPERM
            // case: a failure here is fatal (the read-only is load-bearing,
            // exactly like the item binds').
            for spec in &ro_dir_specs {
                if !spec.target_dir.is_dir() {
                    continue; // no bind created it — nothing to lock
                }
                sys::mount(
                    Some(&spec.target),
                    &spec.target,
                    None,
                    sys::MS_BIND | sys::MS_REC,
                    None,
                )
                .map_err(|e| { sys::warn(&spec.fail_msg); e })?;
                sys::mount(
                    None,
                    &spec.target,
                    None,
                    sys::MS_REMOUNT | sys::MS_BIND | sys::MS_RDONLY,
                    None,
                )
                .map_err(|e| { sys::warn(&spec.fail_msg); e })?;
            }
            // Minimal /dev (replaces the dropped blanket host /dev bind): a fresh
            // tmpfs with only the standard char devices (bind-mounted from the
            // host), /dev/shm, a best-effort private devpts, and the fd symlinks.
            // Nothing else from the host /dev is reachable.
            fs::create_dir_all(&dev_dir)?;
            sys::mount(Some(&tmpfs_c), &dev_dir_c, Some(&tmpfs_c), 0, Some(&dev_data))
                .map_err(|e| { sys::warn(b"td-builder host-sandbox: FAILED mounting /dev tmpfs\n"); e })?;
            for (src_c, target, target_c) in &dev_node_specs {
                fs::File::create(target)?;
                sys::mount(Some(src_c), target_c, None, sys::MS_BIND, None)
                    .map_err(|e| { sys::warn(b"td-builder host-sandbox: FAILED binding a /dev node\n"); e })?;
            }
            fs::create_dir_all(&dev_shm_dir)?;
            sys::mount(Some(&tmpfs_c), &dev_shm_c, Some(&tmpfs_c), 0, Some(&dev_shm_data))
                .map_err(|e| { sys::warn(b"td-builder host-sandbox: FAILED mounting /dev/shm\n"); e })?;
            // /dev/pts + /dev/ptmx are best-effort: a new devpts instance needs an
            // unprivileged-mountable devpts (most kernels allow it; some restrict).
            // Nothing in the loop needs a real pty, so a missing /dev/pts only
            // affects a direct interactive user of this sandbox.
            fs::create_dir_all(&dev_pts_dir)?;
            if sys::mount(Some(&devpts_c), &dev_pts_c, Some(&devpts_c), 0, Some(&devpts_data))
                .is_err()
            {
                sys::warn(b"td-builder host-sandbox: devpts unavailable; /dev/pts left empty\n");
            }
            for (link, dest) in &dev_symlinks {
                let _ = std::os::unix::fs::symlink(dest, link);
            }
            // A FRESH procfs reflecting THIS PID namespace (we are its PID 1) —
            // NOT the host /proc. Nested containers write /proc/<pid>/setgroups
            // and friends against this; the host /proc (root-owned PID 1) refused
            // those writes from the non-root sandbox uid.
            fs::create_dir_all(&proc_target_dir)?;
            sys::mount(Some(&proc_c), &proc_target_c, Some(&proc_c), 0, None)
                .map_err(|e| { sys::warn(b"td-builder host-sandbox: FAILED mounting a fresh /proc\n"); e })?;
            // Writable scratch tmpfs mounts (/tmp, HOME), owned by the host uid.
            for (target_dir, target_c) in &tmpfs_specs {
                fs::create_dir_all(target_dir)?;
                sys::mount(Some(&tmpfs_c), target_c, Some(&tmpfs_c), 0, Some(&tmpfs_data))
                    .map_err(|e| { sys::warn(b"td-builder host-sandbox: FAILED mounting a scratch tmpfs\n"); e })?;
            }
            // pivot into the new root and drop the old one entirely.
            fs::create_dir_all(&oldroot_rel)?;
            sys::pivot_root(&newroot_c, &oldroot_rel_c)
                .map_err(|e| { sys::warn(b"td-builder host-sandbox: FAILED at pivot_root\n"); e })?;
            std::env::set_current_dir("/")?;
            sys::umount2(&oldroot_abs_c, sys::MNT_DETACH)
                .map_err(|e| { sys::warn(b"td-builder host-sandbox: FAILED unmounting oldroot\n"); e })?;
            let _ = fs::remove_dir("/oldroot");
            // Enter the requested working directory (e.g. the exposed worktree).
            std::env::set_current_dir(&workdir_owned)?;
            Ok(())
        });
    }

    command
        .status()
        .map_err(|e| err(format!("spawning {cmd} in host-sandbox: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    // The #469 staging gate, exercised at the unit level (no namespace needed):
    // an item no td-owned db vouches for refuses to stage, tampered bytes refuse
    // to stage, and vouched bytes pass. Verified red against the pre-manifest
    // boundary, which bind-mounted any existing on-disk path a closure named.
    #[test]
    fn staging_rejects_unmanifested_and_tampered_items() {
        let dir = std::env::temp_dir().join(format!("td-stage-verify-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let item = dir.join("aaa-tool-1.0");
        fs::write(&item, b"trusted bytes").unwrap();
        let on_disk = item.to_str().unwrap();
        let canonical = "/td/store/aaa-tool-1.0";
        let good_hash = nar_hash_of(&item).unwrap();

        // No record at all → refused before any hashing.
        let empty = StageManifest::new();
        let err = verify_staged_item(&empty, canonical, on_disk).unwrap_err();
        assert!(err.to_string().contains("no td-owned store-db record"), "{err}");

        // A record whose hash the on-disk bytes do not match → refused.
        let mut tampered = StageManifest::new();
        tampered.insert(
            canonical.to_string(),
            StagedInput { nar_hash: "sha256:0000".to_string(), origin: InputOrigin::AuditedSeed },
        );
        let err = verify_staged_item(&tampered, canonical, on_disk).unwrap_err();
        assert!(err.to_string().contains("refusing to stage tampered bytes"), "{err}");

        // The vouched bytes pass.
        let mut vouched = StageManifest::new();
        vouched.insert(
            canonical.to_string(),
            StagedInput { nar_hash: good_hash, origin: InputOrigin::AuditedSeed },
        );
        verify_staged_item(&vouched, canonical, on_disk).unwrap();

        // …and stop passing the moment the bytes change under the same record.
        fs::write(&item, b"tampered bytes").unwrap();
        let err = verify_staged_item(&vouched, canonical, on_disk).unwrap_err();
        assert!(err.to_string().contains("refusing to stage tampered bytes"), "{err}");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn mesboot_steps_are_a_file_payload_not_an_exec_environment_value() {
        let builder_store = crate::store::builder_identity_path();
        let builder = format!("{builder_store}/bin/td-builder");
        let args = vec!["mesboot-build".to_string()];
        let large = "x".repeat(300 * 1024);
        let value = "x".repeat(4000);
        let manifest = td_engine::application::ApplicationDeclaration::new(
            "empty-runtime",
            "/app/bin/application",
        )
        .unwrap()
        .with_environment("TOKEN", "{root}")
        .unwrap()
        .with_environment("BIG_A", &value)
        .unwrap()
        .with_environment("BIG_B", &value)
        .unwrap()
        .with_environment("BIG_C", &value)
        .unwrap()
        .manifest(
            "application",
            "1",
            td_engine::application::ApplicationProvenance::Source,
        )
        .unwrap();
        let application_spec = td_engine::application_spec::ApplicationSpec::compile(
            &manifest,
            "/td/store/0123456789abcdfghijklmnpqrsvwxyz-empty-runtime-1",
            td_engine::permissions::PermissionPolicy::new(),
        )
        .unwrap()
        .to_keyfile();
        let application = crate::json::Json::Str(manifest.to_keyfile()).to_canonical();
        let application_spec = crate::json::Json::Str(application_spec).to_canonical();
        let application_launcher =
            crate::json::Json::Str("application\tApplication\tapplication fixture\n".into())
                .to_canonical();
        assert!(application.len() > 12_000);
        let env = vec![
            ("TD_INPUT_MAP".to_string(), "{}".to_string()),
            ("TD_APPLICATION_MANIFEST".to_string(), application.clone()),
            ("TD_APPLICATION_SPEC".to_string(), application_spec.clone()),
            (
                "TD_APPLICATION_LAUNCHER".to_string(),
                application_launcher.clone(),
            ),
            ("TD_STEPS".to_string(), large.clone()),
        ];
        let mut command = Command::new(&builder);
        command.env_clear();
        let steps_file = Path::new("/tmp/build/.td-steps.json");
        let steps = configure_builder_env(
            &mut command,
            &builder,
            &args,
            &env,
            steps_file,
        )
        .unwrap();
        assert_eq!(steps.as_deref(), Some(large.as_str()));
        assert!(
            command
                .get_envs()
                .all(|(key, _)| key != std::ffi::OsStr::new("TD_STEPS")),
            "TD_STEPS must not cross execve in the environment"
        );
        assert!(
            command
                .get_envs()
                .any(|(key, value)| key == std::ffi::OsStr::new("TD_INPUT_MAP")
                    && value == Some(std::ffi::OsStr::new("{}"))),
            "ordinary derivation environment entries remain environment entries"
        );
        assert!(
            command
                .get_envs()
                .all(|(key, _)| key != std::ffi::OsStr::new("TD_APPLICATION_MANIFEST")),
            "the sandbox parent, not the package PID namespace, owns application metadata"
        );
        assert!(
            command
                .get_envs()
                .all(|(key, _)| key != std::ffi::OsStr::new("TD_APPLICATION_SPEC")),
            "the compiled spec must stay in the outer sandbox too"
        );
        assert!(
            command
                .get_envs()
                .all(|(key, _)| key != std::ffi::OsStr::new("TD_APPLICATION_LAUNCHER")),
            "the launcher export must stay in the outer sandbox too"
        );
        assert!(
            command.get_envs().any(|(key, value)| {
                key == std::ffi::OsStr::new(crate::build::MESBOOT_STEPS_FILE_ENV)
                    && value == Some(steps_file.as_os_str())
            }),
            "the builder receives the absolute steps-file contract"
        );
        let mut missing = Command::new("/td/store/builder/bin/td-builder");
        missing.env_clear();
        assert!(
            configure_builder_env(
                &mut missing,
                &builder,
                &args,
                &[],
                steps_file,
            )
            .unwrap_err()
            .to_string()
            .contains("missing its TD_STEPS file payload")
        );
        let mut other_builder = Command::new("/td/store/other/bin/builder");
        other_builder.env_clear();
        assert!(
            configure_builder_env(
                &mut other_builder,
                "/td/store/other/bin/builder",
                &args,
                &env,
                steps_file,
            )
            .unwrap()
            .is_none()
        );
        assert!(
            other_builder
                .get_envs()
                .any(|(key, value)| key == std::ffi::OsStr::new("TD_STEPS")
                    && value == Some(std::ffi::OsStr::new(&large))),
            "another builder's environment ABI must remain unchanged"
        );
        assert!(
            other_builder.get_envs().any(|(key, value)| {
                key == std::ffi::OsStr::new("TD_APPLICATION_MANIFEST")
                    && value == Some(std::ffi::OsStr::new(application.as_str()))
            }),
            "the filter applies only to trusted recipe runners"
        );
        assert!(other_builder.get_envs().any(|(key, value)| {
            key == std::ffi::OsStr::new("TD_APPLICATION_SPEC")
                && value == Some(std::ffi::OsStr::new(application_spec.as_str()))
        }));
        assert!(other_builder.get_envs().any(|(key, value)| {
            key == std::ffi::OsStr::new("TD_APPLICATION_LAUNCHER")
                && value == Some(std::ffi::OsStr::new(application_launcher.as_str()))
        }));
        let mut untrusted_td_builder = Command::new("/td/store/other/bin/td-builder");
        untrusted_td_builder.env_clear();
        assert!(
            configure_builder_env(
                &mut untrusted_td_builder,
                "/td/store/other/bin/td-builder",
                &args,
                &env,
                steps_file,
            )
            .unwrap_err()
            .to_string()
            .contains("not the stable td-builder identity"),
            "a td-builder-shaped mesboot invocation must fail before execve"
        );
        let mut other_command = Command::new(&builder);
        other_command.env_clear();
        assert!(
            configure_builder_env(
                &mut other_command,
                &builder,
                &["autotools-build".to_string()],
                &env,
                steps_file,
            )
            .unwrap()
            .is_none()
        );
        assert!(
            other_command
                .get_envs()
                .any(|(key, _)| key == std::ffi::OsStr::new("TD_STEPS")),
            "only the exact mesboot invocation uses the file handoff"
        );
        let mut malformed_phase = Command::new(&builder);
        malformed_phase.env_clear();
        let error = configure_builder_env(
            &mut malformed_phase,
            &builder,
            &["mesboot-build".to_string(), "extra".to_string()],
            &env,
            steps_file,
        )
        .unwrap_err();
        assert!(error.to_string().contains("has no metadata policy"), "{error}");
        let mut manifest_free_phase = Command::new(&builder);
        manifest_free_phase.env_clear();
        let manifest_free_env = vec![("TD_STEPS".to_string(), large.clone())];
        assert!(
            configure_builder_env(
                &mut manifest_free_phase,
                &builder,
                &["mesboot-build".to_string(), "extra".to_string()],
                &manifest_free_env,
                steps_file,
            )
            .unwrap()
            .is_none()
        );
        assert!(manifest_free_phase.get_envs().any(|(key, value)| {
            key == std::ffi::OsStr::new("TD_STEPS")
                && value == Some(std::ffi::OsStr::new(&large))
        }));
        let mut policy_collision = Command::new(&builder);
        policy_collision.env_clear();
        let collision = vec![(
            crate::check_memory::JOB_BUDGET_ENV.to_string(),
            u64::MAX.to_string(),
        )];
        let error = configure_builder_env(
            &mut policy_collision,
            &builder,
            &["autotools-build".to_string()],
            &collision,
            steps_file,
        )
        .unwrap_err();
        assert!(error.to_string().contains("reserved policy key"), "{error}");

        let inherited = std::ffi::OsStr::new("4294967296");
        let mut trusted = Command::new(&builder);
        trusted.env_clear();
        forward_trusted_check_policy(&mut trusted, &builder, Some(inherited));
        assert!(trusted.get_envs().any(|(key, value)| {
            key == std::ffi::OsStr::new(crate::check_memory::JOB_BUDGET_ENV)
                && value == Some(inherited)
        }));
        let mut untrusted = Command::new("/td/store/other/bin/td-builder");
        untrusted.env_clear();
        forward_trusted_check_policy(
            &mut untrusted,
            "/td/store/other/bin/td-builder",
            Some(inherited),
        );
        assert!(untrusted.get_envs().all(|(key, _)| {
            key != std::ffi::OsStr::new(crate::check_memory::JOB_BUDGET_ENV)
        }));

        let mut bootstrap_application = Command::new(&builder);
        bootstrap_application.env_clear();
        let error = configure_builder_env(
            &mut bootstrap_application,
            &builder,
            &["stage0-build".to_string()],
            &env,
            steps_file,
        )
        .unwrap_err();
        assert!(error.to_string().contains("non-application phase"), "{error}");
        let dir =
            std::env::temp_dir().join(format!("td-mesboot-steps-file-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(crate::build::MESBOOT_STEPS_FILE);
        write_mesboot_steps_file(&path, steps.as_deref()).unwrap();
        assert_eq!(
            crate::build::consume_mesboot_steps_file(&path).unwrap(),
            large
        );
        assert!(!path.exists(), "the composed handoff consumes its input file");
        fs::remove_dir_all(&dir).ok();
    }

    fn phase_drv(runner: &str, env: Vec<(String, String)>) -> Derivation {
        Derivation {
            outputs: Vec::new(),
            input_drvs: Vec::new(),
            input_srcs: Vec::new(),
            platform: "x86_64-linux".into(),
            builder: format!(
                "{}/bin/td-builder",
                crate::store::builder_identity_path()
            ),
            args: vec![runner.to_string()],
            env,
        }
    }

    #[test]
    fn every_recipe_runner_has_one_outer_manifest_policy() {
        assert!(
            crate::store::BUILDER_ABI >= 2,
            "manifest reservation must not reuse outputs built before the policy existed"
        );
        assert!(
            crate::store::BUILDER_ABI >= 3,
            "spec reservation must not reuse outputs built before the policy existed"
        );
        for runner in crate::APPLICATION_PHASE_RUNNERS {
            assert_eq!(
                application_manifest_policy(&phase_drv(runner, Vec::new())),
                Some(ApplicationManifestPolicy::Materialize)
            );
        }
        for runner in crate::NON_APPLICATION_PHASE_RUNNERS {
            assert_eq!(
                application_manifest_policy(&phase_drv(runner, Vec::new())),
                Some(ApplicationManifestPolicy::Reserve)
            );
        }
        let mut untrusted = phase_drv("autotools-build", Vec::new());
        untrusted.builder = "/td/store/untrusted/bin/td-builder".into();
        assert_eq!(application_manifest_policy(&untrusted), None);
    }

    #[test]
    fn application_finalization_uses_the_drv_contract_after_namespace_teardown() {
        let source = include_str!("sandbox.rs");
        let production = source
            .split_once("\nmod tests {")
            .map(|(before, _)| before)
            .unwrap_or(source);
        let waited = production
            .find("let status = cmd.status();")
            .expect("sandbox build must wait for its PID namespace");
        let finalized = production
            .find("finalize_application_output(drv, &outputs)?;")
            .expect("sandbox parent must finalize application metadata");
        assert!(
            waited < finalized,
            "a detached output mutator must be unable to run before finalization"
        );

        let build = include_str!("build.rs");
        let build_production = build
            .split_once("\nmod tests {")
            .map(|(before, _)| before)
            .unwrap_or(build);
        assert_eq!(
            build_production
                .matches("materialize_application_metadata_at(")
                .count(),
            1,
            "only the outer sandbox may call the materializer"
        );
        assert_eq!(
            build_production
                .matches("decode_application_manifest(")
                .count(),
            2,
            "the manifest decoder may only be defined and called by the materializer"
        );
        assert_eq!(
            build_production.matches("decode_application_spec(").count(),
            2,
            "the spec decoder may only be defined and called by the materializer"
        );
        assert_eq!(
            build_production
                .matches("write_application_metadata_file(")
                .count(),
            4,
            "only the materializer may call the fixed metadata writer"
        );
    }

    #[test]
    fn outer_finalizer_reads_only_the_hashed_drv_value() {
        let directory =
            std::env::temp_dir().join(format!("td-outer-app-finalizer-{}", std::process::id()));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(&directory).unwrap();
        let manifest = td_engine::application::ApplicationDeclaration::new(
            "empty-runtime",
            "/app/bin/application",
        )
        .unwrap()
        .manifest(
            "application",
            "1",
            td_engine::application::ApplicationProvenance::Source,
        )
        .unwrap();
        let text = manifest.to_keyfile();
        let spec_text = td_engine::application_spec::ApplicationSpec::compile(
            &manifest,
            "/td/store/0123456789abcdfghijklmnpqrsvwxyz-empty-runtime-1",
            td_engine::permissions::PermissionPolicy::new(),
        )
        .unwrap()
        .to_keyfile();
        let encoded = crate::json::Json::Str(text.clone()).to_canonical();
        let encoded_spec = crate::json::Json::Str(spec_text.clone()).to_canonical();
        let launcher_text = "application\tApplication\tapplication fixture\n";
        let encoded_launcher = crate::json::Json::Str(launcher_text.into()).to_canonical();
        let drv = phase_drv(
            "autotools-build",
            vec![
                ("TD_APPLICATION_MANIFEST".into(), encoded),
                ("TD_APPLICATION_SPEC".into(), encoded_spec),
                ("TD_APPLICATION_LAUNCHER".into(), encoded_launcher),
            ],
        );
        finalize_application_output(&drv, &[("out".into(), directory.clone())]).unwrap();
        assert_eq!(fs::read_to_string(directory.join("manifest")).unwrap(), text);
        assert_eq!(
            fs::read_to_string(directory.join("spec")).unwrap(),
            spec_text
        );
        assert_eq!(
            fs::read_to_string(directory.join("exports/launcher.tsv")).unwrap(),
            launcher_text
        );

        let duplicate = phase_drv(
            "autotools-build",
            vec![
                ("TD_APPLICATION_MANIFEST".into(), "one".into()),
                ("TD_APPLICATION_MANIFEST".into(), "two".into()),
                ("TD_APPLICATION_SPEC".into(), "spec".into()),
                ("TD_APPLICATION_LAUNCHER".into(), "launcher".into()),
            ],
        );
        let other = directory.join("duplicate");
        fs::create_dir_all(&other).unwrap();
        let error = finalize_application_output(&duplicate, &[("out".into(), other)]).unwrap_err();
        assert!(error.to_string().contains("duplicate environment key"), "{error}");

        let primary = directory.join("multi-out");
        let secondary = directory.join("multi-dev");
        fs::create_dir_all(&primary).unwrap();
        fs::create_dir_all(&secondary).unwrap();
        fs::write(secondary.join("manifest"), "package-authored").unwrap();
        let error = finalize_application_output(
            &drv,
            &[("out".into(), primary.clone()), ("dev".into(), secondary)],
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("reserved for builder-authenticated metadata"),
            "{error}"
        );
        assert!(
            !primary.join("manifest").exists(),
            "secondary outputs are reserved before the authenticated file is written"
        );

        let named_output = directory.join("named-output");
        fs::create_dir_all(&named_output).unwrap();
        let plain = phase_drv("autotools-build", Vec::new());
        finalize_application_output(&plain, &[("bin".into(), named_output.clone())]).unwrap();
        assert!(!named_output.join("manifest").exists());
        let missing_out = directory.join("missing-out");
        fs::create_dir_all(&missing_out).unwrap();
        let error = finalize_application_output(&drv, &[("bin".into(), missing_out)]).unwrap_err();
        assert!(error.to_string().contains("requires an `out' output"), "{error}");

        let duplicate_out_a = directory.join("duplicate-out-a");
        let duplicate_out_b = directory.join("duplicate-out-b");
        fs::create_dir_all(&duplicate_out_a).unwrap();
        fs::create_dir_all(&duplicate_out_b).unwrap();
        let error = finalize_application_output(
            &drv,
            &[
                ("out".into(), duplicate_out_a.clone()),
                ("out".into(), duplicate_out_b.clone()),
            ],
        )
        .unwrap_err();
        assert!(error.to_string().contains("duplicate output name"), "{error}");
        assert!(!duplicate_out_a.join("manifest").exists());
        assert!(!duplicate_out_b.join("manifest").exists());

        let duplicate_path = directory.join("duplicate-path");
        fs::create_dir_all(&duplicate_path).unwrap();
        let error = finalize_application_output(
            &plain,
            &[
                ("bin".into(), duplicate_path.clone()),
                ("dev".into(), duplicate_path),
            ],
        )
        .unwrap_err();
        assert!(error.to_string().contains("duplicate output path"), "{error}");
        fs::remove_dir_all(&directory).ok();
    }

    // A closure item binds WHOLE: `plan_staged_item` returns one `(on-disk, target)`
    // pair keyed by the store basename, whatever runnable programs the tree ships.
    /// The DATA channel's mount half (APPLICATIONS.md §B.8). Three properties, and
    /// the third is the one a reader should check first: `MS_NOEXEC` is pinned by
    /// VALUE, because it reaches the kernel as a bit in a flag word and a wrong
    /// constant is a mount that succeeds having promised something else.
    #[test]
    fn a_payload_binds_noexec_and_nothing_else_does() {
        assert_eq!(sys::MS_NOEXEC, 0x8, "MS_NOEXEC is the kernel's, not ours");
        let payloads = vec!["/td/store/def-firefox".to_string()];
        assert_eq!(
            extra_bind_flags(&payloads, "/td/store/def-firefox"),
            sys::MS_NOEXEC
        );
        // An ordinary input keeps exactly the flags it always had — this landing
        // must not quietly make the whole store non-executable, which would break
        // every build rather than confine one payload.
        assert_eq!(extra_bind_flags(&payloads, "/td/store/abc-gcc"), 0);
        assert_eq!(extra_bind_flags(&[], "/td/store/def-firefox"), 0);
        // The word the child ACTUALLY issues, through the same function the call
        // site calls. Re-composing the `|` chain here instead would assert
        // nothing — `(A|B|C|X) & X == X` for any values — and deleting `extra`
        // from the production remount would leave it green.
        assert_eq!(
            remount_flags(extra_bind_flags(&payloads, "/td/store/def-firefox")),
            sys::MS_REMOUNT | sys::MS_BIND | sys::MS_RDONLY | sys::MS_NOEXEC
        );
        assert_eq!(
            remount_flags(extra_bind_flags(&payloads, "/td/store/abc-gcc")),
            sys::MS_REMOUNT | sys::MS_BIND | sys::MS_RDONLY,
            "an ordinary item must gain nothing"
        );
    }

    /// A drv declaring `paths` through the payload channel and nothing else.
    fn drv_with_payloads(paths: &[&str]) -> Derivation {
        let map = paths
            .iter()
            .map(|p| {
                let name = p.rsplit('-').next().unwrap_or("p");
                format!("\"{name}\":\"{p}\"")
            })
            .collect::<Vec<_>>()
            .join(",");
        Derivation {
            outputs: Vec::new(),
            input_drvs: Vec::new(),
            input_srcs: Vec::new(),
            platform: "x86_64-linux".into(),
            builder: "/b".into(),
            args: Vec::new(),
            env: if paths.is_empty() {
                Vec::new()
            } else {
                vec![("TD_PAYLOAD_MAP".into(), format!("{{{map}}}"))]
            },
        }
    }

    /// The assertions above reach the LEAVES; this one reaches the SEAM, which is
    /// where the flag is actually derived. Replacing the staging loop's per-item
    /// `extra` with a literal `0` left every leaf assertion green, because none
    /// of them observed what that loop was handed — so the plan is a value now,
    /// derived from the drv rather than from an argument the untestable caller
    /// supplies, and this holds it.
    #[test]
    fn the_bind_plan_carries_noexec_to_exactly_the_declared_payloads() {
        let closure = vec![
            "/td/store/abc-gcc".to_string(),
            "/td/store/def-firefox".to_string(),
            // A td-interned item: canonical is what the build sees, on-disk is
            // where the bytes are. The flag must key on the FORMER.
            "/gnu/store/ghi-bash\t/td/store/ghi-bash".to_string(),
        ];
        let plan = plan_bind_flags(&drv_with_payloads(&["/td/store/def-firefox"]), &closure).unwrap();
        assert_eq!(
            plan,
            vec![
                ("/td/store/abc-gcc", "/td/store/abc-gcc", 0),
                ("/td/store/def-firefox", "/td/store/def-firefox", sys::MS_NOEXEC),
                ("/gnu/store/ghi-bash", "/td/store/ghi-bash", 0),
            ]
        );
        // A drv declaring no payload is exactly what every build is today.
        assert!(plan_bind_flags(&drv_with_payloads(&[]), &closure)
            .unwrap()
            .iter()
            .all(|(_, _, extra)| *extra == 0));
    }

    /// A declared payload the closure never offers is a restriction that applied
    /// to NOTHING, which no observation distinguishes from one that applied: the
    /// build runs with every bind executable and exits 0.
    #[test]
    fn a_payload_missing_from_the_closure_is_an_error_not_a_no_op() {
        let closure = vec!["/td/store/abc-gcc".to_string()];
        let e = plan_bind_flags(&drv_with_payloads(&["/td/store/def-firefox"]), &closure)
            .expect_err("a payload outside the closure must refuse");
        assert!(e.to_string().contains("def-firefox"), "{e}");
        assert!(e.to_string().contains("apply to nothing"), "{e}");
        // The on-disk half of an interned entry is NOT what the map names, so
        // matching against it would let the restriction pass while applying to
        // a path the build never sees.
        let interned = vec!["/gnu/store/def-firefox\t/td/store/def-firefox".to_string()];
        assert!(plan_bind_flags(&drv_with_payloads(&["/td/store/def-firefox"]), &interned).is_err());
        assert!(plan_bind_flags(&drv_with_payloads(&["/gnu/store/def-firefox"]), &interned).is_ok());
    }

    /// Items stage flat under `newstore/<basename>`, so two entries sharing one
    /// basename bind onto a single target and the second stacks over the first.
    /// That shadowing predates this channel; dropping a restriction through it
    /// does not, so a disagreement about flags is refused.
    #[test]
    fn a_basename_collision_may_not_silently_drop_a_restriction() {
        let collide = vec![
            "/gnu/store/abc-x".to_string(),
            "/td/store/abc-x".to_string(),
        ];
        let e = plan_bind_flags(&drv_with_payloads(&["/td/store/abc-x"]), &collide)
            .expect_err("one payload and one ordinary item on one target must refuse");
        assert!(e.to_string().contains("shadow"), "{e}");
        // Agreeing entries are the pre-existing case and are left as they were.
        assert!(plan_bind_flags(&drv_with_payloads(&[]), &collide).is_ok());
    }

    /// `payload_paths` is how a plan learns what is restricted, so a SECOND
    /// caller would be a second place the answer could be obtained — and one
    /// that could then be handed to something that ignores it. One production
    /// caller, and it is the planner.
    #[test]
    fn the_payload_set_is_read_in_exactly_one_place() {
        let src = include_str!("sandbox.rs");
        let shipped = match src.find("\nmod tests {") {
            Some(at) => src.get(..at).unwrap_or(src),
            None => src,
        };
        let calls: Vec<&str> = shipped
            .lines()
            .map(str::trim)
            .filter(|l| l.contains("payload_paths(drv)") || l.contains("plan_bind_flags("))
            .collect();
        assert_eq!(
            calls,
            vec![
                "let payloads = &payload_paths(drv)?;",
                "let plan = plan_bind_flags(drv, closure)?;",
            ],
            "one reader of the payload set, one planner, and the planner is what \
             the staging loop consumes (APPLICATIONS.md section B.8)"
        );
    }

    /// The remount is the ONLY place `extra` may reach the kernel, asserted over
    /// this file's own source because no runtime test in this crate enters a
    /// namespace.
    ///
    /// Two failures it exists for, both of which leave every other test green:
    /// dropping `extra` from the remount, and moving it to the `MS_BIND` call that
    /// CREATES the mount — where the kernel ignores the flag word entirely, so a
    /// payload would bind executable while the call site reads correctly.
    #[test]
    fn the_payload_flag_reaches_the_kernel_only_through_the_remount() {
        let src = include_str!("sandbox.rs");
        let shipped = match src.find("\nmod tests {") {
            Some(at) => src.get(..at).unwrap_or(src),
            None => src,
        };
        let calls: Vec<&str> = shipped
            .lines()
            .map(str::trim)
            .filter(|l| l.contains("sys::mount(") && l.contains("extra"))
            .collect();
        assert_eq!(
            calls,
            ["sys::mount(None, dst, None, remount_flags(*extra), None)?;"],
            "the payload flag must reach the kernel through the remount and nowhere \
             else (APPLICATIONS.md section B.8)"
        );
        // The creating bind must stay a bare MS_BIND: a flag word there applies
        // nothing, so the mistake is invisible at the call site.
        assert!(
            shipped.contains("sys::mount(Some(src), dst, None, sys::MS_BIND, None)?;"),
            "the creating bind must carry MS_BIND alone"
        );
        // ...and the scan must be able to SEE the loop, or it passes for nothing.
        assert!(
            shipped.contains("fn remount_flags") && shipped.len() < src.len(),
            "the shipped-half split stopped working"
        );
        // The last seam a type cannot hold: the staging loop copies the plan's
        // flag word into the bind it pushes, and writing a literal `0` there
        // leaves every value assertion in this file green — the plan is still
        // right, it is just no longer what the child is handed. So the push is
        // pinned in text, since nothing here can enter a namespace to observe it.
        let pushes: Vec<&str> = shipped
            .lines()
            .map(str::trim)
            .filter(|l| l.contains("binds.push(("))
            .collect();
        assert_eq!(pushes.len(), 1, "one bind push: {pushes:?}");
        let tail = shipped
            .split_once("binds.push((")
            .map(|(_, rest)| rest)
            .unwrap_or_default();
        let flag_arg = tail
            .lines()
            .map(str::trim)
            .take_while(|l| !l.starts_with("));"))
            .last();
        assert_eq!(
            flag_arg,
            Some("extra,"),
            "the bind must carry the PLAN's flag word, not a literal"
        );
    }

    /// The payload set is read off the drv's OWN env, so it is hashed drv data and
    /// cannot be changed without changing the derivation. ABSENT is none — the
    /// ordinary case — and MALFORMED is an error, because reading a corrupt map as
    /// "no payloads" would mount a payload executable on a JSON slip.
    #[test]
    fn payload_paths_come_from_the_drvs_own_env() {
        let drv = |env: Vec<(String, String)>| Derivation {
            outputs: Vec::new(),
            input_drvs: Vec::new(),
            input_srcs: Vec::new(),
            platform: "x86_64-linux".into(),
            builder: "/b".into(),
            args: Vec::new(),
            env,
        };
        assert!(
            payload_paths(&drv(Vec::new())).unwrap().is_empty(),
            "absent is none"
        );
        assert_eq!(
            payload_paths(&drv(vec![(
                "TD_PAYLOAD_MAP".into(),
                r#"{"firefox":"/td/store/def-firefox"}"#.into()
            )]))
            .unwrap(),
            vec!["/td/store/def-firefox".to_string()]
        );
        // PRESENT AND MALFORMED is an error, never "no payloads". Reading a corrupt
        // map as absent would mount a payload EXECUTABLE on the strength of a JSON
        // slip — a restriction that fails open when its input is damaged.
        for bad in [
            "not json",
            "[\"/td/store/def-firefox\"]",
            r#"{"firefox":7}"#,
        ] {
            let e = payload_paths(&drv(vec![("TD_PAYLOAD_MAP".into(), bad.into())]))
                .expect_err("a malformed payload map must refuse, not fail open");
            assert!(
                e.to_string().contains("TD_PAYLOAD_MAP"),
                "the refusal must name the variable: {e}"
            );
        }
        // A neighbouring variable must not be mistaken for it.
        assert!(
            payload_paths(&drv(vec![(
                "TD_INPUT_MAP".into(),
                r#"{"gcc":"/td/store/abc-gcc"}"#.into()
            )]))
            .unwrap()
            .is_empty(),
            "the tool channel is not the data one"
        );
    }

    #[test]
    fn plan_staged_item_stages_a_dir_whole() {
        let root = std::env::temp_dir().join(format!("td-stage-whole-{}", std::process::id()));
        fs::remove_dir_all(&root).ok();
        let newstore = root.join("newstore");
        fs::create_dir_all(&newstore).unwrap();
        let tree = root.join("wzxy-td-tool-1.0");
        fs::create_dir_all(tree.join("bin")).unwrap();
        fs::write(tree.join("bin/td-tool"), b"\x7fELF a program").unwrap();
        let canonical = "/gnu/store/wzxy-td-tool-1.0";
        let on_disk = tree.to_str().unwrap();
        let meta = fs::symlink_metadata(&tree).unwrap();
        let binds = plan_staged_item(&newstore, canonical, on_disk, &meta).unwrap();
        assert_eq!(binds.len(), 1, "a dir stages as one whole-tree bind");
        assert_eq!(binds[0].0.as_str(), on_disk, "the whole tree is the bind source");
        assert_eq!(binds[0].1, newstore.join("wzxy-td-tool-1.0"), "keyed by store basename");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn mountinfo_selects_the_deepest_backing_mount_and_decodes_paths() {
        let info = "malformed row that must not hide later mounts\n\
                    1 0 8:1 / / rw - ext4 /dev/root rw\n\
                    2 1 0:2 / /work rw - tmpfs tmpfs rw\n\
                    3 1 8:2 / /work/disk\\040cache rw - xfs /dev/sdb rw\n\
                    4 1 0:3 / /work rw - overlay overlay rw,lowerdir=/lower,upperdir=/memory/upper,workdir=/memory/work\n\
                    5 1 0:4 / /memory rw - tmpfs tmpfs rw\n";
        let overlay = mount_backing_from(info, Path::new("/work/tree")).unwrap();
        assert_eq!(
            overlay,
            MountBacking {
                fs_type: "overlay".to_string(),
                overlay_upper: Some(PathBuf::from("/memory/upper")),
            }
        );
        assert_eq!(
            mount_backing_from(info, Path::new("/work/disk cache/build"))
                .map(|backing| backing.fs_type),
            Some("xfs".to_string()),
        );
        assert!(require_disk_backed_from(info, Path::new("/work/tree"), 0, false).is_err());
        assert!(
            require_disk_backed_from(info, Path::new("/work/disk cache/build"), 0, false).is_ok()
        );
        assert!(memory_backed_fs("tmpfs"));
        assert!(!memory_backed_fs("xfs"));
    }

    #[test]
    fn hidden_overlay_upper_is_usable_in_a_detached_sandbox() {
        let hidden = format!("/td-hidden-overlay-upper-{}", std::process::id());
        assert!(!Path::new(&hidden).exists());
        let info = format!(
            "1 0 0:1 / / rw - tmpfs tmpfs rw\n\
             2 1 0:2 / /tmp rw - overlay overlay rw,lowerdir=/lower,upperdir={hidden},workdir=/work\n"
        );

        assert!(require_disk_backed_from(&info, Path::new("/tmp/logs"), 0, true).is_ok());
        assert!(require_disk_backed_from(&info, Path::new("/tmp/logs"), 0, false).is_err());
    }

    #[test]
    fn scratch_cleanup_recovers_read_only_build_directories() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!(
            "td-readonly-scratch-{}",
            std::process::id()
        ));
        let nested = root.join("readonly/nested");
        fs::create_dir_all(&nested).unwrap();
        fs::write(nested.join("artifact"), b"bytes").unwrap();
        fs::set_permissions(&nested, fs::Permissions::from_mode(0o500)).unwrap();
        fs::set_permissions(root.join("readonly"), fs::Permissions::from_mode(0o500)).unwrap();
        remove_scratch_tree(&root).unwrap();
        assert!(!root.exists());
    }

    #[test]
    fn build_scratch_sweep_reclaims_only_unleased_trees() {
        // Nest this test fixture below the production walk's depth bound. A
        // daemon-request exercised concurrently by another test may sweep its
        // own broad scratch root; that must not make this lock-lifetime test
        // race an unrelated sweeper.
        let fixture_root = std::env::temp_dir()
            .join(format!("td-build-scratch-sweep-{}", std::process::id()));
        let fixture = fixture_root.join("fixture/a");
        let root = fixture.join("root");
        let _ = fs::remove_dir_all(&fixture_root);
        let stale = root.join("stale");
        let active = root.join("active");
        for dir in [&stale, &active] {
            fs::create_dir_all(dir.join("build-tmp/objects")).unwrap();
            fs::write(dir.join("build-tmp/objects/file"), b"large tree").unwrap();
            fs::File::create(dir.join(".build-tmp.lock")).unwrap();
        }
        let active_lease = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(active.join(".build-tmp.lock"))
            .unwrap();
        active_lease.lock().unwrap();

        sweep_abandoned_build_temps(&root).unwrap();

        assert!(!stale.join("build-tmp").exists());
        assert!(active.join("build-tmp").exists());
        drop(active_lease);
        sweep_abandoned_build_temps(&root).unwrap();
        assert!(!active.join("build-tmp").exists());
        let _ = fs::remove_dir_all(fixture_root);
    }

    #[test]
    fn overdeep_scratch_cannot_poison_unrelated_recovery() {
        use std::os::unix::fs::PermissionsExt;

        let root = std::env::temp_dir().join(format!(
            "td-build-scratch-depth-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let poison = root.join("poison");
        let mut deep = poison.join("build-tmp");
        let mut created = Vec::new();
        for _ in 0..(128 + 2) {
            deep = deep.join("d");
            created.push(deep.clone());
        }
        fs::create_dir_all(&deep).unwrap();
        fs::write(deep.join("artifact"), b"bytes").unwrap();
        fs::File::create(poison.join(".build-tmp.lock")).unwrap();
        if let Some(blocked) = created.last() {
            fs::set_permissions(blocked, fs::Permissions::from_mode(0o000)).unwrap();
        }
        let healthy = root.join("healthy");
        fs::create_dir_all(healthy.join("build-tmp")).unwrap();
        fs::File::create(healthy.join(".build-tmp.lock")).unwrap();
        let opaque = root.join("opaque");
        fs::create_dir_all(opaque.join("nested/build-tmp")).unwrap();
        fs::set_permissions(&opaque, fs::Permissions::from_mode(0o000)).unwrap();

        sweep_abandoned_build_temps(&root).unwrap();

        assert!(!healthy.join("build-tmp").exists());
        assert!(!poison.join("build-tmp").exists());
        for dir in created.iter().rev() {
            let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o700));
        }
        let _ = fs::set_permissions(&opaque, fs::Permissions::from_mode(0o700));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn store_path_name_strips_hash() {
        // Feeds the ACTIVE store prefix (now the native `/td/store` default); the
        // sibling `store_path_name_honors_the_active_prefix` covers the explicit-prefix
        // core and proves a `/gnu/store` path is rejected under a `/td/store` active dir.
        assert_eq!(
            store_path_name("/td/store/xiwgysq1h8dd2k5mkb94ky8vrgcp10dz-td-builder-0.1.0.drv")
                .unwrap(),
            "td-builder-0.1.0.drv"
        );
        assert!(store_path_name("/tmp/x").is_err());
        assert!(store_path_name("/td/store/tooshort-x").is_err());
        // A slash after the hash means a path INSIDE an item, not an item.
        assert!(store_path_name(
            "/td/store/xiwgysq1h8dd2k5mkb94ky8vrgcp10dz-td-builder-0.1.0/bin/td-builder"
        )
        .is_err());
    }

    #[test]
    fn store_path_name_honors_the_active_prefix() {
        // The pure core strips whichever store dir is active — proving a /td/store build
        // recognises its OWN paths natively (no /gnu/store assumption baked in).
        assert_eq!(
            store_path_name_in(
                "/td/store/",
                "/td/store/xiwgysq1h8dd2k5mkb94ky8vrgcp10dz-hello-2.12.1"
            )
            .unwrap(),
            "hello-2.12.1"
        );
        // A /gnu/store path is NOT a /td/store path — the prefix is load-bearing.
        assert!(store_path_name_in(
            "/td/store/",
            "/gnu/store/xiwgysq1h8dd2k5mkb94ky8vrgcp10dz-hello-2.12.1"
        )
        .is_err());
    }

    #[test]
    fn closure_entry_splits_canonical_from_on_disk() {
        // A bare entry binds from its canonical path (the daemon-resident case).
        let bare = "/gnu/store/xiwgysq1h8dd2k5mkb94ky8vrgcp10dz-td-builder-src";
        assert_eq!(split_closure_entry(bare), (bare, bare));
        // A `CANONICAL\tON-DISK` entry binds from the td store dir but the build
        // still SEES the canonical path (the td-interned source case).
        let canonical = "/gnu/store/xiwgysq1h8dd2k5mkb94ky8vrgcp10dz-td-builder-src";
        let on_disk = "/scratch/srcstore/xiwgysq1h8dd2k5mkb94ky8vrgcp10dz-td-builder-src";
        let entry = format!("{canonical}\t{on_disk}");
        assert_eq!(split_closure_entry(&entry), (canonical, on_disk));
    }
}

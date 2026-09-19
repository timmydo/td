//! Where the test suites put their fixtures.
//!
//! Compiled into the unit suite as a test-only module and into the
//! integration test by `#[path]`: a binary crate has no library an
//! integration test could import, and both suites must make one choice for
//! the same reasons.
//!
//! An install reads every byte of a volume that is five gigabytes at its
//! smallest — the scratch image mkfs formatted, which lives here — and fsyncs
//! the destination seven times. On a disk-backed filesystem those two are the
//! whole cost of the suites: 32 s and 14 s here against 1.5 s and 1 s on
//! tmpfs, where a hole is a zero page and an fsync is nothing. Nothing a test
//! observes depends on which filesystem holds the files.
//!
//! So the fixtures go under `/dev/shm` when the host mounts one that will do,
//! and under the temp dir otherwise — or whenever `TMPDIR` is set, since an
//! explicit choice is an answer, not a default to improve on. "Will do" is
//! probed, not assumed. The mkfs and td-boot stand-ins are scripts and live
//! here too, and a hardened host mounts its tmpfs `noexec`. And a tmpfs
//! bounded at the 64 MiB a container runtime gives by default cannot hold a
//! run: the images are sparse, but a formatted volume's tables and the edges
//! an install zeroes are real pages, and the harness holds a dozen fixtures
//! at once.
//!
//! Everything lives in one private directory created exclusively for this
//! process. `/dev/shm` and `/tmp` are world-writable, and a fixture at a
//! predictable name in one is a name a planted symlink could already hold;
//! `mkdir` does not follow one, so a taken name fails instead of being
//! entered. A held file lock protects each root across PID namespaces. Only
//! unlocked roots with the lease marker are swept; legacy unmarked roots are
//! left for manual cleanup. The chosen filesystem must support advisory file
//! locks. The lease belongs to the test process, not orphaned children.

use std::fs::{self, File};
use std::io::{self, ErrorKind, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::OnceLock;

/// What the probe writes and frees before choosing the tmpfs: room for the
/// fixtures a full-width run holds at once, with margin.
const RESERVE: u64 = 256 * 1024 * 1024;

// Old PID-based sweepers must not recognize the leased roots.
const PREFIX: &str = "td-install-leased-scratch";
const LEASE: &str = ".lease";

struct ScratchRoot {
    path: PathBuf,
    _lease: File,
}

static NEXT: AtomicUsize = AtomicUsize::new(0);

/// A fresh, unique path under this process's private scratch root. Nothing is
/// created at it; the caller makes the file or directory it needs.
pub fn path(tag: &str) -> PathBuf {
    root().join(format!("{tag}-{}", NEXT.fetch_add(1, Ordering::Relaxed)))
}

fn root() -> &'static Path {
    static ROOT: OnceLock<ScratchRoot> = OnceLock::new();
    ROOT.get_or_init(|| {
        let explicit = std::env::var_os("TMPDIR").is_some_and(|v| !v.is_empty());
        let shm = PathBuf::from("/dev/shm");
        let base = if !explicit && suits(&shm) {
            shm
        } else {
            std::env::temp_dir()
        };
        match private_dir(&base) {
            Ok(dir) => {
                if let Ok(metadata) = dir._lease.metadata() {
                    sweep(&base, metadata.uid());
                }
                dir
            }
            Err(e) => panic!("no scratch root under {}: {e}", base.display()),
        }
    })
    .path
    .as_path()
}

/// A private directory named with a PID, clock tick and serial.
/// Mode 0700 and exclusive: an entry already at the name, a symlink included, fails
/// the creation rather than being entered, and the next name is tried.
fn private_dir(base: &Path) -> io::Result<ScratchRoot> {
    use std::os::unix::fs::DirBuilderExt;
    let pid = std::process::id();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_nanos();
    let mut taken = None;
    for _ in 0..64 {
        let dir = base.join(format!(
            "{PREFIX}-{pid}-{nonce}-{}",
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
            Ok(()) => {
                let result = claim_root(&dir);
                if result.is_err() {
                    let _ = fs::remove_dir_all(&dir);
                }
                return result;
            }
            Err(e) if e.kind() == ErrorKind::AlreadyExists => taken = Some(e),
            Err(e) => return Err(e),
        }
    }
    Err(taken.unwrap_or_else(|| io::Error::other("every scratch name is taken")))
}

fn claim_root(dir: &Path) -> io::Result<ScratchRoot> {
    let pending = dir.join(".lease-new");
    let lease = File::options()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&pending)?;
    lease
        .try_lock()
        .map_err(|error| io::Error::other(format!("lock scratch lease: {error}")))?;
    // Publish only after locking, so another process cannot reclaim a new root.
    fs::rename(&pending, dir.join(LEASE))?;
    Ok(ScratchRoot {
        path: dir.to_path_buf(),
        _lease: lease,
    })
}

/// Whether `base` can hold a run's fixtures and run a script from them.
fn suits(base: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let Ok(dir) = private_dir(base) else {
        return false;
    };
    let script = dir.path.join("probe.sh");
    let ok = std::fs::write(&script, "#!/bin/sh\nexit 0\n").is_ok()
        && std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).is_ok()
        && std::process::Command::new(&script)
            .status()
            .is_ok_and(|status| status.success())
        && fills(&dir.path.join("reserve"), RESERVE);
    let _ = std::fs::remove_dir_all(&dir.path);
    ok
}

/// Whether `bytes` of real, not sparse, data can be written at `path`. A
/// tmpfs page is allocated by the write, so a bounded one runs out here
/// rather than under a fixture.
fn fills(path: &Path, bytes: u64) -> bool {
    let Ok(mut file) = std::fs::File::create(path) else {
        return false;
    };
    let chunk = vec![0u8; 1 << 20];
    let mut left = bytes;
    while left > 0 {
        let take = usize::try_from(left).map_or(chunk.len(), |l| l.min(chunk.len()));
        let Some(piece) = chunk.get(..take) else {
            return false;
        };
        if file.write_all(piece).is_err() {
            return false;
        }
        left -= take as u64;
    }
    true
}

/// Reclaim only private, same-owner roots whose published lease is unlocked.
/// PID visibility is not evidence of liveness across namespaces.
fn sweep(base: &Path, owner: u32) {
    let Ok(entries) = fs::read_dir(base) else {
        return;
    };
    let prefix = format!("{PREFIX}-");
    for entry in entries.flatten() {
        if !entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.starts_with(&prefix))
        {
            continue;
        }
        let Ok(root) = fs::symlink_metadata(entry.path()) else {
            continue;
        };
        if !root.is_dir() || root.uid() != owner || root.mode() & 0o7777 != 0o700 {
            continue;
        }
        let path = entry.path().join(LEASE);
        let Ok(marker) = fs::symlink_metadata(&path) else {
            continue;
        };
        if !marker.is_file()
            || marker.uid() != owner
            || marker.nlink() != 1
            || marker.mode() & 0o7777 != 0o600
        {
            continue;
        }
        let Ok(lease) = File::options().read(true).write(true).open(&path) else {
            continue;
        };
        let Ok(opened) = lease.metadata() else {
            continue;
        };
        if opened.dev() != marker.dev() || opened.ino() != marker.ino() || lease.try_lock().is_err()
        {
            continue;
        }
        // Keep the lock until reclamation finishes.
        let _ = reclaim_root(&entry.path());
    }
}

fn reclaim_root(root: &Path) -> io::Result<()> {
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if entry.file_name() == LEASE {
            continue;
        }
        if entry.file_type()?.is_dir() {
            fs::remove_dir_all(entry.path())?;
        } else {
            fs::remove_file(entry.path())?;
        }
    }
    // A failed content removal retains the lease marker for the next sweep.
    // Interruption after this unlink can strand only an empty directory.
    fs::remove_file(root.join(LEASE))?;
    fs::remove_dir(root)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::DirBuilderExt;

    #[test]
    fn a_live_lease_survives_an_invisible_pid_and_released_roots_are_swept() -> io::Result<()> {
        let base = private_dir(&std::env::temp_dir())?;
        let owner = fs::metadata(&base.path)?.uid();
        let hidden = base.path.join(format!("{PREFIX}-0-0"));
        fs::DirBuilder::new().mode(0o700).create(&hidden)?;
        let held = claim_root(&hidden)?;
        fs::write(hidden.join("evidence"), b"live fixture")?;
        sweep(&base.path, owner);
        assert_eq!(fs::read(hidden.join("evidence"))?, b"live fixture");
        drop(held);
        sweep(&base.path, owner);
        assert!(!hidden.exists());
        fs::remove_dir_all(&base.path)
    }

    #[test]
    fn incomplete_and_aliased_roots_are_not_reclaimed() -> io::Result<()> {
        let base = private_dir(&std::env::temp_dir())?;
        let owner = fs::metadata(&base.path)?.uid();
        let incomplete = base.path.join(format!("{PREFIX}-0-0"));
        fs::DirBuilder::new().mode(0o700).create(&incomplete)?;
        fs::write(incomplete.join("evidence"), b"not published")?;
        let alias = base.path.join(format!("{PREFIX}-0-1"));
        std::os::unix::fs::symlink(&incomplete, &alias)?;
        sweep(&base.path, owner);
        assert_eq!(fs::read(incomplete.join("evidence"))?, b"not published");
        assert!(fs::symlink_metadata(alias)?.is_symlink());
        fs::remove_dir_all(&base.path)
    }
}

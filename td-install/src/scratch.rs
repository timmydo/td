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
//! entered. The base is swept of the roots of earlier runs whose process is
//! gone, which is what a killed run leaves behind.

use std::io::{self, ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::OnceLock;

/// What the probe writes and frees before choosing the tmpfs: room for the
/// fixtures a full-width run holds at once, with margin.
const RESERVE: u64 = 256 * 1024 * 1024;

const PREFIX: &str = "td-install-scratch";

static NEXT: AtomicUsize = AtomicUsize::new(0);

/// A fresh, unique path under this process's private scratch root. Nothing is
/// created at it; the caller makes the file or directory it needs.
pub fn path(tag: &str) -> PathBuf {
    root().join(format!("{tag}-{}", NEXT.fetch_add(1, Ordering::Relaxed)))
}

fn root() -> &'static Path {
    static ROOT: OnceLock<PathBuf> = OnceLock::new();
    ROOT.get_or_init(|| {
        let explicit = std::env::var_os("TMPDIR").is_some_and(|v| !v.is_empty());
        let shm = PathBuf::from("/dev/shm");
        let base = if !explicit && suits(&shm) {
            shm
        } else {
            std::env::temp_dir()
        };
        sweep(&base);
        match private_dir(&base) {
            Ok(dir) => dir,
            Err(e) => panic!("no scratch root under {}: {e}", base.display()),
        }
    })
}

/// A directory `base/td-install-scratch-<pid>-<n>` that did not exist, mode
/// 0700. Exclusive: an entry already at the name, a symlink included, fails
/// the creation rather than being entered, and the next name is tried.
fn private_dir(base: &Path) -> io::Result<PathBuf> {
    use std::os::unix::fs::DirBuilderExt;
    let pid = std::process::id();
    let mut taken = None;
    for _ in 0..8 {
        let dir = base.join(format!("{PREFIX}-{pid}-{}", NEXT.fetch_add(1, Ordering::Relaxed)));
        match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
            Ok(()) => return Ok(dir),
            Err(e) if e.kind() == ErrorKind::AlreadyExists => taken = Some(e),
            Err(e) => return Err(e),
        }
    }
    Err(taken.unwrap_or_else(|| io::Error::other("every scratch name is taken")))
}

/// Whether `base` can hold a run's fixtures and run a script from them.
fn suits(base: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let Ok(dir) = private_dir(base) else {
        return false;
    };
    let script = dir.join("probe.sh");
    let ok = std::fs::write(&script, "#!/bin/sh\nexit 0\n").is_ok()
        && std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).is_ok()
        && std::process::Command::new(&script)
            .status()
            .is_ok_and(|status| status.success())
        && fills(&dir.join("reserve"), RESERVE);
    let _ = std::fs::remove_dir_all(&dir);
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

/// Remove the scratch roots under `base` of earlier runs whose process is
/// gone. Liveness is `/proc/<pid>`; without a procfs nothing is known and
/// nothing is removed. Only a root this user made can be removed — anything
/// else fails quietly — and a symlink at such a name is removed as a link.
fn sweep(base: &Path) {
    if !Path::new("/proc/self").exists() {
        return;
    }
    let Ok(entries) = std::fs::read_dir(base) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name
            .to_str()
            .and_then(|n| n.strip_prefix(PREFIX))
            .and_then(|rest| rest.strip_prefix('-'))
            .and_then(|rest| rest.split('-').next())
            .and_then(|pid| pid.parse::<u32>().ok())
        else {
            continue;
        };
        if pid == std::process::id() || Path::new("/proc").join(pid.to_string()).exists() {
            continue;
        }
        let _ = std::fs::remove_dir_all(entry.path());
    }
}

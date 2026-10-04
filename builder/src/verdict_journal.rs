//! The pass journals `--resume` reads: a directory holding one file per
//! content key, each a line per gate or preflight command that passed on
//! that content. Host-only (engine_set.rs): `gate-run` and `affected-checks`
//! keep them, and no build reads them.
//!
//! A line is a proof only until it is asked again: whatever runs is first
//! forgotten under every key, so a rerun that fails or is stopped leaves
//! nothing to resume. Losing another key's line costs a rerun, never a
//! wrong skip. Every access holds a lock, within the process for parallel
//! gates and across processes for two runs in one worktree, since a forget
//! is a read, filter and rename that a concurrent one could undo; a run
//! holds a second lock for its whole cycle (`hold_run`).

use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

static IN_PROCESS: Mutex<()> = Mutex::new(());

const LOCK: &str = ".lock";
const RUN: &str = ".run";

pub(crate) struct Journal {
    dir: PathBuf,
}

impl Journal {
    pub(crate) fn new(dir: PathBuf) -> Self {
        Journal { dir }
    }

    /// An exclusive lock on `name` in the journal directory, made if need
    /// be. None when either cannot be had.
    fn flock(&self, name: &str) -> Option<File> {
        std::fs::create_dir_all(&self.dir).ok()?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.dir.join(name))
            .ok()?;
        file.lock().ok().map(|()| file)
    }

    /// Run `f` holding both locks, telling it whether it does. A caller
    /// without the file lock must fail safe: read nothing, record nothing,
    /// and forget by removing whole files, since a rewrite from a stale read
    /// could put back a line a concurrent forget removed.
    fn locked<T>(&self, f: impl FnOnce(bool) -> T) -> T {
        let _held = IN_PROCESS.lock().unwrap_or_else(PoisonError::into_inner);
        let flock = self.flock(LOCK);
        f(flock.is_some())
    }

    /// Hold the journal for a whole run's forget, run and record cycle: a
    /// second run in the same worktree waits here, so it cannot forget a
    /// line between another's forget and record and have the other's pass
    /// outlive its own failure. None when the lock cannot be taken; the run
    /// then neither reuses nor records.
    pub(crate) fn hold_run(&self) -> Option<File> {
        std::fs::create_dir_all(&self.dir).ok()?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.dir.join(RUN))
            .ok()?;
        match file.try_lock() {
            Ok(()) => return Some(file),
            Err(std::fs::TryLockError::WouldBlock) => eprintln!(
                "waiting for another run in this worktree to release {}",
                self.dir.display()
            ),
            Err(std::fs::TryLockError::Error(_)) => return None,
        }
        file.lock().ok().map(|()| file)
    }

    fn path(&self, key: &str) -> PathBuf {
        self.dir.join(key)
    }

    /// What passed under `key`.
    pub(crate) fn read(&self, key: &str) -> HashSet<String> {
        self.locked(|held| {
            if !held {
                return HashSet::new();
            }
            std::fs::read_to_string(self.path(key))
                .map(|t| t.lines().map(str::to_string).collect())
                .unwrap_or_default()
        })
    }

    /// Record that `line` passed under `key` (best-effort: journaling never
    /// changes a verdict). A line that is empty or spans lines is not
    /// recorded, since it would read back as something else.
    pub(crate) fn record(&self, key: &str, line: &str) {
        if line.is_empty() || line.contains('\n') {
            return;
        }
        self.locked(|held| {
            if !held {
                return;
            }
            if let Ok(mut f) = OpenOptions::new()
                .create(true)
                .append(true)
                .open(self.path(key))
            {
                let _ = writeln!(f, "{line}");
            }
        });
    }

    /// Remove `line` under every key, before it runs. A journal that cannot
    /// be rewritten, or any holding the line when the lock could not be
    /// taken, is removed whole, so a stale pass cannot outlive a forget.
    pub(crate) fn forget(&self, line: &str) {
        self.locked(|held| {
            let Ok(entries) = std::fs::read_dir(&self.dir) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if name == LOCK || name == RUN || name.ends_with(".tmp") {
                    continue;
                }
                forget_in(&path, line, held);
            }
        });
    }
}

fn forget_in(path: &Path, line: &str, held: bool) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    if !text.lines().any(|l| l == line) {
        return;
    }
    if !held {
        let _ = std::fs::remove_file(path);
        return;
    }
    let kept: String = text
        .lines()
        .filter(|l| *l != line)
        .map(|l| format!("{l}\n"))
        .collect();
    let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
    let written = File::create(&tmp)
        .and_then(|mut f| f.write_all(kept.as_bytes()))
        .and_then(|()| std::fs::rename(&tmp, path));
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("td-verdict-journal-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn a_forget_reaches_every_key_and_keeps_the_rest() {
        let d = dir("forget");
        let j = Journal::new(d.clone());
        assert!(j.read("k1").is_empty(), "nothing before the directory");
        j.record("k1", "a");
        j.record("k1", "b");
        j.record("k2", "a");
        j.record("k2", "two\nlines");
        j.record("k2", "");
        assert_eq!(j.read("k2").len(), 1, "only whole single lines record");
        j.forget("a");
        assert_eq!(j.read("k1"), HashSet::from(["b".to_string()]));
        assert!(j.read("k2").is_empty());
        j.forget("absent");
        assert_eq!(j.read("k1").len(), 1);
        let names: Vec<String> = std::fs::read_dir(&d)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(names.iter().all(|n| !n.ends_with(".tmp")), "{names:?}");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Without the file lock a forget cannot trust its read, so it removes
    /// any journal holding the line rather than rewriting it.
    #[test]
    fn an_unlocked_forget_removes_the_whole_journal() {
        let d = dir("unlocked");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("k1"), "a\nb\n").unwrap();
        std::fs::write(d.join("k2"), "b\n").unwrap();
        forget_in(&d.join("k1"), "a", false);
        forget_in(&d.join("k2"), "a", false);
        assert!(!d.join("k1").exists(), "held the line: removed");
        assert!(d.join("k2").exists(), "did not: kept");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A second run's hold waits for the first's to drop.
    #[test]
    fn a_run_hold_is_exclusive_across_handles() {
        let d = dir("hold");
        let j = Journal::new(d.clone());
        let first = j.hold_run().unwrap();
        let probe = OpenOptions::new().write(true).open(d.join(RUN)).unwrap();
        assert!(matches!(
            probe.try_lock(),
            Err(std::fs::TryLockError::WouldBlock)
        ));
        drop(first);
        assert!(probe.try_lock().is_ok());
        let _ = std::fs::remove_dir_all(&d);
    }
}

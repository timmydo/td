//! Test-only helpers that used to come from the `tempfile` crate: a directory
//! under the system temporary directory that is removed on drop.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);

/// A fresh directory, removed with everything in it when dropped.
pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Make a directory unique to this process and call; the name carries the pid
/// and a counter so parallel tests never share one.
pub fn tempdir() -> std::io::Result<TempDir> {
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "{}-{}-{n}",
        env!("CARGO_PKG_NAME"),
        std::process::id()
    ));
    std::fs::create_dir_all(&path)?;
    Ok(TempDir { path })
}

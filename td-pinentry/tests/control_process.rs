//! td-pinentry under the real headless td-compositor. `ready` supplies
//! the disposable compositor through `TD_TEST_COMPOSITOR`; the cases
//! start td-pinentry as gpg-agent or ssh would, type into its window
//! through the compositor's seat, and read the answer it writes.
//!
//! The file is named `control_process` and mounts `native_compositor` so
//! the gate's native runner (`builder/src/native_tests.rs`), which runs
//! `--test control_process` filtered to `native_compositor::`, finds it.
#![forbid(unsafe_code)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use std::io::{self, Read, Write};
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

const TIMEOUT: Duration = Duration::from_secs(10);
static NEXT: AtomicU64 = AtomicU64::new(0);

#[path = "support/native_compositor.rs"]
mod native_compositor;

/// A short-lived private directory for a compositor session or a
/// client's runtime, at a Linux-socket-length path independent of
/// TMPDIR.
struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        let path = Path::new("/tmp").join(format!(
            "td-pinentry-process-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .unwrap();
        Self(path)
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn remaining(deadline: Instant) -> io::Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|duration| !duration.is_zero())
        .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "native I/O deadline"))
}

fn write_until(stream: &mut UnixStream, mut bytes: &[u8], deadline: Instant) -> io::Result<()> {
    while !bytes.is_empty() {
        stream.set_write_timeout(Some(remaining(deadline)?))?;
        match stream.write(bytes) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(n) => bytes = &bytes[n..],
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

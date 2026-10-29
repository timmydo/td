//! Deadline-bounded Unix connects with a finite number of pending helpers.
//!
//! A full Unix listen backlog can block `UnixStream::connect` before a stream
//! exists on which to set a timeout. A timed-out caller relinquishes its result,
//! but its helper retains admission until the connect actually returns.

use std::io;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::{mpsc, Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

pub const MAX_CONNECTORS: usize = 8;
pub const STACK_BYTES: usize = 256 * 1024;
static CONNECTORS: OnceLock<Arc<Pool>> = OnceLock::new();

/// Connect to a caller-authorized path within one absolute deadline, including
/// admission. This bounds resources, not filesystem authority: the caller must
/// validate the endpoint and its trusted namespace separately.
pub fn connect_until(path: &Path, deadline: Instant) -> io::Result<UnixStream> {
    let path = path.to_owned();
    connect_by(deadline, "td-unix-connect", move || {
        UnixStream::connect(path)
    })
}

/// Shared implementation for adapters that already own their dialing operation.
/// At most eight connect helpers are pending in this process, including helpers
/// whose callers timed out. A late result is closed, never reused by a new call.
pub fn connect_by<F>(deadline: Instant, name: &str, connect: F) -> io::Result<UnixStream>
where
    F: FnOnce() -> io::Result<UnixStream> + Send + 'static,
{
    CONNECTORS
        .get_or_init(|| Arc::new(Pool::new(MAX_CONNECTORS)))
        .run(deadline, name, connect)
}

fn remaining(deadline: Instant) -> io::Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|left| !left.is_zero())
        .ok_or_else(timeout)
}

fn timeout() -> io::Error {
    io::Error::new(
        io::ErrorKind::TimedOut,
        "Unix socket connect deadline expired",
    )
}

fn poisoned() -> io::Error {
    io::Error::other("Unix socket connect admission unavailable")
}

struct Pool {
    active: Mutex<usize>,
    changed: Condvar,
    maximum: usize,
}
impl Pool {
    const fn new(maximum: usize) -> Self {
        Self {
            active: Mutex::new(0),
            changed: Condvar::new(),
            maximum,
        }
    }

    fn acquire(self: &Arc<Self>, deadline: Instant) -> io::Result<Permit> {
        let mut active = self.active.lock().map_err(|_| poisoned())?;
        loop {
            let left = remaining(deadline)?;
            if *active < self.maximum {
                *active += 1;
                return Ok(Permit(self.clone()));
            }
            let (next, _) = self
                .changed
                .wait_timeout(active, left)
                .map_err(|_| poisoned())?;
            active = next;
        }
    }

    fn run<F>(self: &Arc<Self>, deadline: Instant, name: &str, connect: F) -> io::Result<UnixStream>
    where
        F: FnOnce() -> io::Result<UnixStream> + Send + 'static,
    {
        let permit = self.acquire(deadline)?;
        remaining(deadline)?;
        let receiver = start(permit, name, connect)?;
        let result = receiver.recv_timeout(remaining(deadline)?);
        remaining(deadline)?;
        match result {
            Ok(result) => result,
            Err(mpsc::RecvTimeoutError::Timeout) => Err(timeout()),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(io::Error::other(
                "Unix socket connect helper ended without a result",
            )),
        }
    }
}
fn start<F>(
    permit: Permit,
    name: &str,
    connect: F,
) -> io::Result<mpsc::Receiver<io::Result<UnixStream>>>
where
    F: FnOnce() -> io::Result<UnixStream> + Send + 'static,
{
    let (sender, receiver) = mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name(name.to_owned())
        .stack_size(STACK_BYTES)
        .spawn(move || {
            let _permit = permit;
            let _ = sender.send(connect());
        })?;
    Ok(receiver)
}
struct Permit(Arc<Pool>);
impl Drop for Permit {
    fn drop(&mut self) {
        if let Ok(mut active) = self.0.active.lock() {
            *active = active.saturating_sub(1);
            self.0.changed.notify_one();
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::io::Read;
    fn deadline() -> Instant {
        Instant::now() + Duration::from_secs(30)
    }

    #[test]
    fn expired_admission_never_starts_a_connect() {
        let pool = Arc::new(Pool::new(1));
        let result = pool.run(Instant::now(), "expired-connect", || {
            panic!("expired call must not dial")
        });
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::TimedOut);
        assert_eq!(*pool.active.lock().unwrap(), 0);
    }

    #[test]
    fn admission_and_result_share_one_deadline() {
        let pool = Arc::new(Pool::new(1));
        let permit = pool.acquire(deadline()).unwrap();
        let result = pool.run(Instant::now(), "full-connect", || {
            panic!("full pool must not dial")
        });
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::TimedOut);
        assert_eq!(*pool.active.lock().unwrap(), 1);
        drop(permit);
        let (stream, _peer) = UnixStream::pair().unwrap();
        assert!(pool
            .run(deadline(), "released-connect", move || Ok(stream))
            .is_ok());
    }

    #[test]
    fn a_timed_out_helper_retains_its_permit_and_closes_its_late_stream() {
        let pool = Arc::new(Pool::new(1));
        let (stream, mut peer) = UnixStream::pair().unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap();
        let (started, wait_started) = mpsc::sync_channel(1);
        let (release, wait_release) = mpsc::sync_channel(1);
        let receiver = start(
            pool.acquire(deadline()).unwrap(),
            "blocked-connect",
            move || {
                started.send(()).unwrap();
                wait_release.recv().unwrap();
                Ok(stream)
            },
        )
        .unwrap();
        wait_started.recv_timeout(Duration::from_secs(30)).unwrap();
        // A timeout drops this receiver. Exercise that exact cancellation
        // without depending on how quickly the host schedules the helper.
        drop(receiver);
        assert_eq!(*pool.active.lock().unwrap(), 1);
        let result = pool.run(Instant::now(), "refused-connect", || panic!("no admission"));
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::TimedOut);
        release.send(()).unwrap();
        assert_eq!(peer.read(&mut [0]).unwrap(), 0);
        let permit = pool.acquire(deadline()).unwrap();
        drop(permit);
    }

    #[test]
    fn a_failed_or_panicking_helper_releases_admission() {
        let pool = Arc::new(Pool::new(1));
        let result = pool.run(deadline(), "failed-connect", || {
            Err(io::ErrorKind::NotFound.into())
        });
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::NotFound);
        let result = pool.run(deadline(), "panicking-connect", || panic!("fixture"));
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::Other);
        drop(pool.acquire(deadline()).unwrap());
    }
}

//! td's own persistent BUILD daemon (own-builder-daemon track): a long-running
//! td-builder that realizes derivations served over a Unix socket — the loop's
//! builder instead of guix-daemon. `serve` is the accept loop; `request` is the
//! in-process client (so a caller needs no nc/socat).
//!
//! The daemon is the loop's shared per-user build limiter: it realizes drvs
//! CONCURRENTLY but only up to a `budget` of simultaneous builds (a counting
//! semaphore), queueing the rest. Because ONE shared daemon serves every
//! worktree/agent for this user, N agents submitting at once can never exceed
//! the worker budget. Memory-heavy check submissions separately hold grants
//! from the lazy per-user check host. Each build runs in a SEPARATE child
//! `td-builder` process (Command::spawn — the safe fork+exec), never an in-process fork on a daemon
//! thread (`sandbox::build` mutates the process CWD and forks with heavy pre-exec work,
//! which is unsound in a multithreaded process); process isolation also gives each build
//! its own CWD/namespaces. Content-addressed dedup + repro (`daemon-build`/`daemon-check`)
//! live in the spawned child; the daemon adds persistence, the socket front end, and the
//! budget. Line protocol (one request per connection):
//!   request  = "<drv-path>\n"          build (realize) the drv
//!            | "CHECK <drv-path>\n"     reproducibility check: rebuild once + compare
//!                                       against the build already realized (two
//!                                       independent builds; falls back to two fresh
//!                                       builds if none was realized yet)
//!            | "SHUTDOWN\n"             clean stop
//!   response = "OK <payload>\n" | "ERR <msg>\n"

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;

const MAX_REQUEST_BYTES: usize = 64 * 1024;
const MAX_RESPONSE_BYTES: usize = 64 * 1024;
const WORKER_STACK_BYTES: usize = 512 * 1024;
const DEFAULT_READ_TIMEOUT_MS: u64 = 30_000;
const MAX_READ_TIMEOUT_MS: u64 = 60_000;
const DISCONNECT_POLL: std::time::Duration = std::time::Duration::from_millis(50);

/// A counting semaphore (std has none): `budget` permits; `acquire` blocks until one is
/// free and releases on guard drop. This bounds worker threads in this daemon.
struct Semaphore {
    count: Mutex<usize>,
    cv: Condvar,
}

impl Semaphore {
    fn new(n: usize) -> Arc<Semaphore> {
        Arc::new(Semaphore {
            count: Mutex::new(n),
            cv: Condvar::new(),
        })
    }
    fn acquire(self: &Arc<Self>) -> Permit {
        let mut n = self.count.lock().unwrap();
        while *n == 0 {
            n = self.cv.wait(n).unwrap();
        }
        *n -= 1;
        Permit(self.clone())
    }
}

/// Releases its permit back to the semaphore on drop (even on a build panic).
struct Permit(Arc<Semaphore>);
impl Drop for Permit {
    fn drop(&mut self) {
        let mut n = self.0.count.lock().unwrap();
        *n += 1;
        self.0.cv.notify_one();
    }
}

struct CountGuard(Arc<AtomicUsize>);

impl CountGuard {
    fn enter(counter: Arc<AtomicUsize>) -> (Self, usize) {
        let count = counter.fetch_add(1, Ordering::SeqCst) + 1;
        (Self(counter), count)
    }
}

impl Drop for CountGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

struct WatchDone(Arc<std::sync::atomic::AtomicBool>);

impl Drop for WatchDone {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

fn watch_requester(
    mut stream: UnixStream,
    done: Arc<std::sync::atomic::AtomicBool>,
    cancelled: Arc<std::sync::atomic::AtomicBool>,
) {
    let _ = stream.set_read_timeout(Some(DISCONNECT_POLL));
    let mut byte = [0u8; 1];
    while !done.load(Ordering::Relaxed) {
        match stream.read(&mut byte) {
            Ok(0) => {
                cancelled.store(true, Ordering::Relaxed);
                return;
            }
            Ok(_) => {}
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::TimedOut
                        | std::io::ErrorKind::Interrupted
                ) => {}
            Err(_) => {
                cancelled.store(true, Ordering::Relaxed);
                return;
            }
        }
    }
}

fn request_read_timeout(raw: Option<&str>) -> Result<std::time::Duration, String> {
    let millis = match raw {
        Some(value) => value
            .parse::<u64>()
            .map_err(|_| "TD_DAEMON_READ_TIMEOUT_MS must be an integer".to_string())?,
        None => DEFAULT_READ_TIMEOUT_MS,
    };
    if !(1..=MAX_READ_TIMEOUT_MS).contains(&millis) {
        return Err(format!(
            "TD_DAEMON_READ_TIMEOUT_MS must be between 1 and {MAX_READ_TIMEOUT_MS}"
        ));
    }
    Ok(std::time::Duration::from_millis(millis))
}

fn read_request_line(
    conn: &UnixStream,
    timeout: std::time::Duration,
) -> Result<Option<String>, String> {
    let deadline = std::time::Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| "daemon request deadline overflowed Instant".to_string())?;
    let mut line = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let remaining = deadline
            .checked_duration_since(std::time::Instant::now())
            .ok_or_else(|| {
                "daemon request did not finish before its absolute deadline".to_string()
            })?;
        conn.set_read_timeout(Some(remaining))
            .map_err(|e| format!("set bounded daemon request timeout: {e}"))?;
        let mut stream = conn;
        let count = stream
            .read(&mut chunk)
            .map_err(|e| format!("read daemon request before its absolute deadline: {e}"))?;
        if count == 0 {
            return if line.is_empty() {
                Ok(None)
            } else {
                Err("daemon request ended before its newline".to_string())
            };
        }
        let bytes = chunk
            .get(..count)
            .ok_or_else(|| "daemon request read exceeded its fixed chunk".to_string())?;
        let newline = bytes.iter().position(|byte| *byte == b'\n');
        let take = newline.unwrap_or(bytes.len());
        let total = line
            .len()
            .checked_add(take)
            .ok_or_else(|| "daemon request length overflowed usize".to_string())?;
        if total > MAX_REQUEST_BYTES {
            return Err(format!(
                "request is too large: limit is {MAX_REQUEST_BYTES} bytes"
            ));
        }
        let body = bytes
            .get(..take)
            .ok_or_else(|| "daemon request slice exceeded its fixed chunk".to_string())?;
        line.extend_from_slice(body);
        if newline.is_some() {
            let text = String::from_utf8(line)
                .map_err(|_| "daemon request is not valid UTF-8".to_string())?;
            return Ok(Some(text.trim().to_string()));
        }
    }
}

fn response_line(kind: &str, payload: &str) -> String {
    let clean = payload.replace(['\n', '\r'], " ");
    let room = MAX_RESPONSE_BYTES.saturating_sub(kind.len().saturating_add(2));
    let mut end = clean.len().min(room);
    while end > 0 && !clean.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    format!("{kind} {}\n", clean.get(..end).unwrap_or(""))
}

fn read_response_line(stream: &UnixStream) -> Result<String, String> {
    let limit = u64::try_from(MAX_RESPONSE_BYTES.saturating_add(1)).unwrap_or(u64::MAX);
    let reader = BufReader::new(stream);
    let mut limited = reader.take(limit);
    let mut bytes = Vec::with_capacity(MAX_RESPONSE_BYTES.min(4096));
    limited
        .read_until(b'\n', &mut bytes)
        .map_err(|e| format!("read daemon response: {e}"))?;
    if bytes.is_empty() {
        return Err("daemon closed without a response".to_string());
    }
    if bytes.len() > MAX_RESPONSE_BYTES || bytes.last() != Some(&b'\n') {
        return Err(format!(
            "daemon response exceeds {MAX_RESPONSE_BYTES} bytes or has no newline"
        ));
    }
    let _ = bytes.pop();
    if bytes.last() == Some(&b'\r') {
        let _ = bytes.pop();
    }
    String::from_utf8(bytes).map_err(|_| "daemon response is not valid UTF-8".to_string())
}

/// Accept-loop over a Unix socket at `socket`. Reads one request line per connection
/// (cheaply, in the accept loop) and dispatches the build to a worker thread that runs
/// `handle` only while holding one of `budget` permits — so at most `budget` builds run
/// at once across all submitters to this user's daemon. `handle(req, cancelled)` gets the
/// raw request line (a drv path, or "CHECK <drv>") plus a flag set when the requester
/// disconnects, and returns the OK payload (or an Err rendered as "ERR …"). Serves until
/// a "SHUTDOWN" line (or the socket errors), then joins outstanding builds.
pub fn serve(
    socket: &str,
    budget: usize,
    handle: impl Fn(&str, &std::sync::atomic::AtomicBool) -> Result<String, String>
        + Send
        + Sync
        + 'static,
) -> Result<(), String> {
    // A stale socket from a prior run would make bind fail with EADDRINUSE.
    let _ = std::fs::remove_file(socket);
    let listener = UnixListener::bind(socket).map_err(|e| format!("bind {socket}: {e}"))?;
    let budget = budget.max(1);
    eprintln!(
        "td-builder: build daemon listening on {socket} (budget {budget} concurrent builds; memory admitted by the per-user check host)"
    );
    let sem = Semaphore::new(budget);
    let handle = Arc::new(handle);
    // Live concurrent-build count, logged on each START so a gate can assert the observed
    // PEAK never exceeds the budget (the cap actually holds) and does reach it (it is not
    // secretly serial).
    let active = Arc::new(AtomicUsize::new(0));
    // Accepted-but-not-finished requests. Workers are DETACHED (no JoinHandle kept) — the
    // daemon is persistent and effectively never shuts down, so a per-request Vec of handles
    // would grow without bound and leave zombie threads. SHUTDOWN instead drains via this
    // counter so no in-flight build is abandoned.
    let inflight = Arc::new(AtomicUsize::new(0));
    // The request read below runs ON the accept thread (cheap by design), so a client
    // that connects and never delivers a full line would otherwise wedge the WHOLE
    // shared daemon (head-of-line): observed live when a gate's kill-cascade test
    // (sandbox-hardening) SIGKILLs process trees while build-recipes' submitters are
    // connecting — a dying client can leave a byte-less connection open via an inherited
    // fd. Bound the read; a connection that times out, errors, or EOFs is DROPPED (with
    // a log line) and the daemon serves the next one. Only an explicit "SHUTDOWN" line
    // stops the daemon — an empty read is a dead client, never a shutdown request.
    let timeout_env = match std::env::var("TD_DAEMON_READ_TIMEOUT_MS") {
        Ok(value) => Some(value),
        Err(std::env::VarError::NotPresent) => None,
        Err(e) => return Err(format!("read TD_DAEMON_READ_TIMEOUT_MS: {e}")),
    };
    let read_timeout = request_read_timeout(timeout_env.as_deref())?;
    for conn in listener.incoming() {
        let conn = match conn {
            Ok(c) => c,
            Err(e) => {
                eprintln!("td-builder: daemon: accept error (serving on): {e}");
                continue;
            }
        };
        let req = match read_request_line(&conn, read_timeout) {
            Ok(Some(line)) => line,
            Ok(None) => {
                eprintln!(
                    "td-builder: daemon: dropped an empty connection (client gone before sending a request)"
                );
                continue;
            }
            Err(e) => {
                eprintln!("td-builder: daemon: dropped an unbounded request: {e}");
                let _ = (&conn).write_all(format!("ERR {e}\n").as_bytes());
                continue;
            }
        };
        // The reply is written by the worker thread when the build ends — that can be
        // minutes/hours later, long past any read deadline; the timeout was only for
        // the request read.
        let _ = conn.set_read_timeout(None);
        if req.is_empty() {
            eprintln!("td-builder: daemon: dropped a blank request line");
            continue;
        }
        if req == "SHUTDOWN" {
            // Drain in-flight builds before exiting so none is killed mid-realize.
            while inflight.load(Ordering::SeqCst) > 0 {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            let _ = (&conn).write_all(b"OK shutdown\n");
            break;
        }
        // Claim before spawning so queued clients do not allocate unbounded
        // thread stacks in the persistent process.
        let permit = sem.acquire();
        let (inflight_guard, _) = CountGuard::enter(inflight.clone());
        let handle = handle.clone();
        let active = active.clone();
        let worker = thread::Builder::new()
            .name("td-build-daemon-worker".to_string())
            .stack_size(WORKER_STACK_BYTES)
            .spawn(move || {
                let _inflight = inflight_guard;
                let _permit = permit;
                let (_active, n) = CountGuard::enter(active);
                eprintln!("td-builder: daemon build START ({n}/{budget} active): {req}");
                let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
                let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
                let watcher = conn.try_clone().and_then(|peer| {
                    let done = done.clone();
                    let cancelled = cancelled.clone();
                    thread::Builder::new()
                        .name("td-build-daemon-peer".to_string())
                        .stack_size(WORKER_STACK_BYTES)
                        .spawn(move || watch_requester(peer, done, cancelled))
                        .map_err(std::io::Error::other)
                });
                let resp = match watcher {
                    Ok(watcher) => {
                        let done_guard = WatchDone(done);
                        if let Ok(ms) = std::env::var("TD_DAEMON_TEST_SLEEP_MS") {
                            if let Ok(ms) = ms.parse::<u64>() {
                                std::thread::sleep(std::time::Duration::from_millis(ms));
                            }
                        }
                        let result = handle(&req, &cancelled);
                        drop(done_guard);
                        let _ = watcher.join();
                        match result {
                            Ok(payload) => response_line("OK", &payload),
                            Err(e) => response_line("ERR", &e),
                        }
                    }
                    Err(e) => response_line(
                        "ERR",
                        &format!("cannot monitor daemon requester lifetime: {e}"),
                    ),
                };
                eprintln!("td-builder: daemon build DONE: {req}");
                let _ = (&conn).write_all(resp.as_bytes());
            });
        if let Err(e) = worker {
            eprintln!("td-builder: daemon: spawn bounded worker: {e}");
        }
    }
    Ok(())
}

/// Connect to the daemon at `socket`, send `req` (a drv path, "CHECK <drv>", or
/// "SHUTDOWN"), and return its single-line response ("OK …" or "ERR …").
pub fn request(socket: &str, req: &str, job_budget: Option<u64>) -> Result<String, String> {
    let stream = UnixStream::connect(socket).map_err(|e| format!("connect {socket}: {e}"))?;
    let granted = job_budget.filter(|_| req != "SHUTDOWN");
    let prefix_len = granted
        .map(|bytes| {
            "BUDGET "
                .len()
                .saturating_add(bytes.to_string().len())
                .saturating_add(1)
        })
        .unwrap_or(0);
    if prefix_len.saturating_add(req.len()) > MAX_REQUEST_BYTES {
        return Err(format!("daemon request exceeds {MAX_REQUEST_BYTES} bytes"));
    }
    if let Some(bytes) = granted {
        writeln!(&stream, "BUDGET {bytes} {req}").map_err(|e| e.to_string())?;
    } else {
        writeln!(&stream, "{req}").map_err(|e| e.to_string())?;
    }
    read_response_line(&stream)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn daemon_response_is_bounded_before_allocation() {
        let (server, client) = UnixStream::pair().unwrap();
        let writer = thread::spawn(move || {
            let line = response_line("ERR", &"é".repeat(MAX_RESPONSE_BYTES));
            assert!(line.len() <= MAX_RESPONSE_BYTES);
            (&server).write_all(line.as_bytes()).unwrap();
        });
        let response = read_response_line(&client).unwrap();
        assert!(response.starts_with("ERR "));
        writer.join().unwrap();

        let (server, client) = UnixStream::pair().unwrap();
        let writer = thread::spawn(move || {
            (&server)
                .write_all(&vec![b'x'; MAX_RESPONSE_BYTES.saturating_add(1)])
                .unwrap();
        });
        assert!(read_response_line(&client).is_err());
        writer.join().unwrap();
    }

    #[test]
    fn daemon_request_deadline_is_absolute_across_trickled_bytes() {
        let (server, mut client) = UnixStream::pair().unwrap();
        let writer = thread::spawn(move || {
            for _ in 0..20 {
                if client.write_all(b"x").is_err() {
                    return;
                }
                thread::sleep(Duration::from_millis(10));
            }
        });
        let started = std::time::Instant::now();
        let err = read_request_line(&server, Duration::from_millis(50)).unwrap_err();
        assert!(err.contains("deadline"), "got: {err}");
        assert!(started.elapsed() < Duration::from_millis(500));
        drop(server);
        writer.join().unwrap();
    }

    #[test]
    fn daemon_request_timeout_is_finite_and_bounded() {
        assert_eq!(
            request_read_timeout(None).unwrap(),
            Duration::from_millis(DEFAULT_READ_TIMEOUT_MS)
        );
        assert!(request_read_timeout(Some("0")).is_err());
        assert!(request_read_timeout(Some("60001")).is_err());
        assert!(request_read_timeout(Some("not-a-number")).is_err());
    }

    /// The core budget property, hermetically (no build machinery): with budget K and M>K
    /// concurrent submitters, at most K handlers run at once AND the peak reaches K — i.e.
    /// the per-daemon cap holds and the daemon is not secretly serial. Verified-red: make
    /// `serve` serial (drop the semaphore / budget=1) → peak=1≠K; make it unbounded → peak
    /// can exceed K. This is the same property gate `daemon-budget` asserts end to end.
    #[test]
    fn budget_caps_concurrent_builds_across_submitters() {
        let dir = std::env::temp_dir().join(format!("td-daemon-budget-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let socket = dir.join("sock");
        let socket_s = socket.to_string_lossy().into_owned();
        let budget = 2usize;

        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let (a, p) = (active.clone(), peak.clone());
        let handle = move |
            _req: &str,
            _cancelled: &std::sync::atomic::AtomicBool,
        | -> Result<String, String> {
            let now = a.fetch_add(1, Ordering::SeqCst) + 1;
            p.fetch_max(now, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(80)); // hold the slot so builds overlap
            a.fetch_sub(1, Ordering::SeqCst);
            Ok("done".to_string())
        };
        let sock_for_serve = socket_s.clone();
        let server = thread::spawn(move || serve(&sock_for_serve, budget, handle).unwrap());

        for _ in 0..200 {
            if socket.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let mut clients = Vec::new();
        for _ in 0..6 {
            let s = socket_s.clone();
            clients.push(thread::spawn(move || {
                let r = request(&s, "/fake.drv", None).unwrap();
                assert!(r.starts_with("OK "), "unexpected response: {r}");
            }));
        }
        for c in clients {
            c.join().unwrap();
        }
        let _ = request(&socket_s, "SHUTDOWN", None);
        server.join().unwrap();

        assert_eq!(
            peak.load(Ordering::SeqCst),
            budget,
            "peak concurrency must reach exactly the budget — not exceed it, not stay serial"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn requester_disconnect_cancels_the_bounded_handler() {
        let dir = std::env::temp_dir().join(format!(
            "td-daemon-disconnect-{}",
            std::process::id()
        ));
        let _ = std::fs::create_dir_all(&dir);
        let socket = dir.join("sock");
        let socket_s = socket.to_string_lossy().into_owned();
        let observed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let handler_observed = observed.clone();
        let handle = move |
            _req: &str,
            cancelled: &std::sync::atomic::AtomicBool,
        | -> Result<String, String> {
            while !cancelled.load(Ordering::Relaxed) {
                thread::sleep(Duration::from_millis(5));
            }
            handler_observed.store(true, Ordering::Relaxed);
            Err("requester disconnected".to_string())
        };
        let sock_for_serve = socket_s.clone();
        let server = thread::spawn(move || serve(&sock_for_serve, 1, handle).unwrap());
        for _ in 0..200 {
            if socket.exists() {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        let stream = UnixStream::connect(&socket).unwrap();
        writeln!(&stream, "/fake.drv").unwrap();
        drop(stream);

        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !observed.load(Ordering::Relaxed) {
            assert!(
                std::time::Instant::now() < deadline,
                "daemon handler did not observe requester disconnect"
            );
            thread::sleep(Duration::from_millis(10));
        }
        let _ = request(&socket_s, "SHUTDOWN", None);
        server.join().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Dead and silent connections must not stop or wedge the daemon (the per-user
    /// serializer): (a) a client that connects and dies without sending a request (EOF)
    /// used to be treated as SHUTDOWN — the daemon exited; (b) a client that connects and
    /// stays SILENT used to wedge the accept thread forever (blocking read, no timeout —
    /// observed live when sandbox-hardening's kill-cascade test SIGKILLed process trees
    /// while build-recipes' submitters were connecting). With the fix, both connections
    /// are dropped and a real request still succeeds. Verified-red: revert the accept-loop
    /// fix — (a) makes the post-EOF request fail (daemon gone), (b) hangs this test.
    #[test]
    fn daemon_survives_dead_and_silent_connections() {
        std::env::set_var("TD_DAEMON_READ_TIMEOUT_MS", "100");
        let dir = std::env::temp_dir().join(format!("td-daemon-hol-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let socket = dir.join("sock");
        let socket_s = socket.to_string_lossy().into_owned();

        let handle = move |
            _req: &str,
            _cancelled: &std::sync::atomic::AtomicBool,
        | -> Result<String, String> { Ok("done".to_string()) };
        let sock_for_serve = socket_s.clone();
        let server = thread::spawn(move || serve(&sock_for_serve, 2, handle).unwrap());
        for _ in 0..200 {
            if socket.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }

        // (a) connect-and-die: EOF must drop the CONNECTION, not the daemon.
        drop(std::os::unix::net::UnixStream::connect(&socket).unwrap());
        // (b) connect-and-stall: held open with no bytes; must not wedge the accept loop.
        let _silent = std::os::unix::net::UnixStream::connect(&socket).unwrap();

        // A real request must still succeed (bounded wait so a regression fails loudly
        // instead of hanging the test runner).
        let s = socket_s.clone();
        let real = thread::spawn(move || request(&s, "/fake.drv", None).unwrap());
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !real.is_finished() {
            assert!(
                std::time::Instant::now() < deadline,
                "daemon wedged: a dead/silent connection blocked a real request (head-of-line)"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        let r = real.join().unwrap();
        assert!(r.starts_with("OK "), "unexpected response: {r}");

        let _ = request(&socket_s, "SHUTDOWN", None);
        server.join().unwrap();
        std::env::remove_var("TD_DAEMON_READ_TIMEOUT_MS");
        let _ = std::fs::remove_dir_all(&dir);
    }

}

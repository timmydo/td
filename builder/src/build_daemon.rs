//! td's own persistent BUILD daemon (own-builder-daemon track): a long-running
//! td-builder that realizes derivations served over a Unix socket — the loop's
//! builder instead of guix-daemon. `serve` is the accept loop; `request` is the
//! in-process client (so a caller needs no nc/socat).
//!
//! The daemon is the loop's SINGLE machine-wide build limiter: it realizes drvs
//! CONCURRENTLY but only up to a global `budget` of simultaneous builds (a counting
//! semaphore), queueing the rest. Because ONE shared daemon serves every worktree
//! /agent (tools/build-daemon-ensure.sh starts one per host), N agents submitting at
//! once can never exceed the budget — bounding CPU and (budget × per-build RSS) memory
//! no matter how many checks run. Each build runs in a SEPARATE child `td-builder`
//! process (Command::spawn — the safe fork+exec), never an in-process fork on a daemon
//! thread (`sandbox::build` mutates the process CWD and forks with heavy pre-exec work,
//! which is unsound in a multithreaded process); process isolation also gives each build
//! its own CWD/namespaces. Content-addressed dedup + repro (`daemon-build`/`daemon-check`)
//! live in the spawned child; the daemon adds persistence, the socket front end, and the
//! budget. Line protocol (one request per connection):
//!   request  = "<drv-path>\n"          build (realize) the drv
//!            | "CHECK <drv-path>\n"     reproducibility double-build + compare
//!            | "SHUTDOWN\n"             clean stop
//!   response = "OK <payload>\n" | "ERR <msg>\n"

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;

/// A counting semaphore (std has none): `budget` permits; `acquire` blocks until one is
/// free and releases on guard drop. This is the machine-wide concurrent-build cap.
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

/// MemAvailable from /proc/meminfo, in GiB (None if unreadable).
fn mem_available_gib() -> Option<f64> {
    let s = std::fs::read_to_string("/proc/meminfo").ok()?;
    for line in s.lines() {
        if let Some(rest) = line.strip_prefix("MemAvailable:") {
            let kb: f64 = rest.trim().trim_end_matches("kB").trim().parse().ok()?;
            return Some(kb / 1024.0 / 1024.0);
        }
    }
    None
}

/// Global memory admission: atomically claim a build slot (incrementing `active`), blocking
/// while free memory is below `min_free_gib`. Returns the new `active` count (for the START
/// log). /proc/meminfo is the shared signal EVERY daemon reads, so this bounds machine-wide
/// memory even when per-binary daemons fragment the per-daemon concurrency budget — the guard
/// against OOM on a swapless host. Always admits when none is running (`active == 0`): only a
/// running build frees memory, so blocking the last one would deadlock. The claim is a CAS so
/// concurrent workers can't all pass the idle check at once. `min_free_gib <= 0` disables the
/// memory check (admit immediately). The caller must `active.fetch_sub(1)` when the build ends.
fn admit(active: &AtomicUsize, min_free_gib: f64) -> usize {
    loop {
        let cur = active.load(Ordering::SeqCst);
        let ok = cur == 0
            || min_free_gib <= 0.0
            || mem_available_gib().map(|g| g >= min_free_gib).unwrap_or(true);
        if !ok {
            std::thread::sleep(std::time::Duration::from_millis(500));
            continue;
        }
        if active
            .compare_exchange(cur, cur + 1, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            return cur + 1;
        }
        // Lost the race to another worker; re-evaluate against the new count.
    }
}

/// Accept-loop over a Unix socket at `socket`. Reads one request line per connection
/// (cheaply, in the accept loop) and dispatches the build to a worker thread that runs
/// `handle` only while holding one of `budget` permits — so at most `budget` builds run
/// at once across ALL submitters. `handle(req)` gets the raw request line (a drv path, or
/// "CHECK <drv>") and returns the OK payload (or an Err rendered as "ERR …"). Serves until
/// a "SHUTDOWN" line (or the socket errors), then joins outstanding builds.
pub fn serve(
    socket: &str,
    budget: usize,
    min_free_gib: f64,
    handle: impl Fn(&str) -> Result<String, String> + Send + Sync + 'static,
) -> Result<(), String> {
    // A stale socket from a prior run would make bind fail with EADDRINUSE.
    let _ = std::fs::remove_file(socket);
    let listener = UnixListener::bind(socket).map_err(|e| format!("bind {socket}: {e}"))?;
    let budget = budget.max(1);
    // `min_free_gib` reserves free memory before admitting a build (global OOM guard; see
    // mem_admission_wait). The daemon arm derives it from TD_MIN_FREE_GIB.
    eprintln!(
        "td-builder: build daemon listening on {socket} (budget {budget} concurrent builds, reserve {min_free_gib:.0} GiB free)"
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
    for conn in listener.incoming() {
        let conn = conn.map_err(|e| format!("accept: {e}"))?;
        // Read the request line; scope the reader's borrow before moving `conn`.
        let req = {
            let mut line = String::new();
            BufReader::new(&conn)
                .read_line(&mut line)
                .map_err(|e| e.to_string())?;
            line.trim().to_string()
        };
        if req.is_empty() || req == "SHUTDOWN" {
            // Drain in-flight builds before exiting so none is killed mid-realize.
            while inflight.load(Ordering::SeqCst) > 0 {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            let _ = (&conn).write_all(b"OK shutdown\n");
            break;
        }
        inflight.fetch_add(1, Ordering::SeqCst);
        let sem = sem.clone();
        let handle = handle.clone();
        let active = active.clone();
        let inflight = inflight.clone();
        thread::spawn(move || {
            let _permit = sem.acquire(); // blocks here when the budget is full (the queue)
            // Atomically claim a slot, blocking while free memory is below the reserve.
            let n = admit(&active, min_free_gib);
            eprintln!("td-builder: daemon build START ({n}/{budget} active): {req}");
            // Test-only slot-occupancy hold (never set in production): lets the daemon-budget
            // gate force overlap and measure the concurrency ceiling deterministically without
            // slow real builds. Held under the permit, so it counts against the budget.
            if let Ok(ms) = std::env::var("TD_DAEMON_TEST_SLEEP_MS") {
                if let Ok(ms) = ms.parse::<u64>() {
                    std::thread::sleep(std::time::Duration::from_millis(ms));
                }
            }
            let resp = match handle(&req) {
                Ok(payload) => format!("OK {payload}\n"),
                // Keep the response a single line — a build error can be multi-line.
                Err(e) => format!("ERR {}\n", e.replace('\n', " ")),
            };
            active.fetch_sub(1, Ordering::SeqCst);
            eprintln!("td-builder: daemon build DONE: {req}");
            let _ = (&conn).write_all(resp.as_bytes());
            inflight.fetch_sub(1, Ordering::SeqCst);
        });
    }
    Ok(())
}

/// Connect to the daemon at `socket`, send `req` (a drv path, "CHECK <drv>", or
/// "SHUTDOWN"), and return its single-line response ("OK …" or "ERR …").
pub fn request(socket: &str, req: &str) -> Result<String, String> {
    let stream = UnixStream::connect(socket).map_err(|e| format!("connect {socket}: {e}"))?;
    writeln!(&stream, "{req}").map_err(|e| e.to_string())?;
    let mut resp = String::new();
    BufReader::new(&stream)
        .read_line(&mut resp)
        .map_err(|e| e.to_string())?;
    Ok(resp.trim_end().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// The core budget property, hermetically (no build machinery): with budget K and M>K
    /// concurrent submitters, at most K handlers run at once AND the peak reaches K — i.e.
    /// the machine-wide cap holds and the daemon is not secretly serial. Verified-red: make
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
        let handle = move |_req: &str| -> Result<String, String> {
            let now = a.fetch_add(1, Ordering::SeqCst) + 1;
            p.fetch_max(now, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(80)); // hold the slot so builds overlap
            a.fetch_sub(1, Ordering::SeqCst);
            Ok("done".to_string())
        };
        let sock_for_serve = socket_s.clone();
        // min_free_gib = 0.0 → memory guard disabled; this test isolates the budget cap.
        let server = thread::spawn(move || serve(&sock_for_serve, budget, 0.0, handle).unwrap());

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
                let r = request(&s, "/fake.drv").unwrap();
                assert!(r.starts_with("OK "), "unexpected response: {r}");
            }));
        }
        for c in clients {
            c.join().unwrap();
        }
        let _ = request(&socket_s, "SHUTDOWN");
        server.join().unwrap();

        assert_eq!(
            peak.load(Ordering::SeqCst),
            budget,
            "peak concurrency must reach exactly the budget — not exceed it, not stay serial"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The memory admission guard, hermetically: with an impossibly-high free-memory reserve,
    /// the daemon admits only ONE build at a time (the active==0 progress guarantee) even
    /// though the budget is high — so machine-wide memory is bounded regardless of the budget
    /// or how many daemons run. Verified-red: drop mem_admission_wait → peak reaches the
    /// budget (4), not 1. This is the OOM guard on a swapless host.
    #[test]
    fn memory_guard_serializes_when_free_memory_is_below_the_reserve() {
        let dir = std::env::temp_dir().join(format!("td-daemon-mem-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let socket = dir.join("sock");
        let socket_s = socket.to_string_lossy().into_owned();
        let budget = 4usize; // high budget — the memory guard, not the budget, must cap here

        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let (a, p) = (active.clone(), peak.clone());
        let handle = move |_req: &str| -> Result<String, String> {
            let now = a.fetch_add(1, Ordering::SeqCst) + 1;
            p.fetch_max(now, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(120));
            a.fetch_sub(1, Ordering::SeqCst);
            Ok("done".to_string())
        };
        let sock_for_serve = socket_s.clone();
        // An impossible reserve (1e9 GiB) → free memory is always "below" it → the guard
        // admits a second build only when none is running.
        let server = thread::spawn(move || serve(&sock_for_serve, budget, 1e9, handle).unwrap());

        for _ in 0..200 {
            if socket.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let mut clients = Vec::new();
        for _ in 0..4 {
            let s = socket_s.clone();
            clients.push(thread::spawn(move || {
                let _ = request(&s, "/fake.drv");
            }));
        }
        for c in clients {
            c.join().unwrap();
        }
        let _ = request(&socket_s, "SHUTDOWN");
        server.join().unwrap();

        assert_eq!(
            peak.load(Ordering::SeqCst),
            1,
            "the memory guard must admit only one build at a time when free memory is below the reserve"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

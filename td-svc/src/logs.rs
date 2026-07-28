//! Capturing a service's output (DESIGN.md §7).
//!
//! The shape is forced by one rule: **a drain must never block the service.**
//! Writing straight through from the reading thread looks simpler and is the
//! trap — a stalled write backs up into the pipe, the pipe fills, and the
//! service blocks in `write(2)` before any error has surfaced anywhere. A
//! service that stops making progress because its LOG is slow is a worse
//! failure than losing the log.
//!
//! So the reader and the writer are separated by a bounded queue. Two drain
//! threads (stdout and stderr) push lines into it; one writer thread empties
//! it. When the queue is full, lines are **dropped and counted** rather than
//! blocking the drain, and the count is reported into the log as
//! `... N lines dropped` once there is room again — a gap that says so beats
//! both a silent gap and a wedged service.
//!
//! ## One writer per service, for the life of the supervisor
//!
//! The writer outlives the instance. A restarting daemon would otherwise get a
//! second writer with its own handle on the same path, and two of them rotating
//! the same file race: both rename, one reopens a file the other has already
//! moved. Instead the queue and its writer are created once per service and
//! each new instance's drains push into the queue that is already there.
//!
//! Drains are per-instance and end at EOF. They are NOT joined on the exit
//! path: a descendant that inherited the pipe write end holds it open after the
//! leader is gone, so joining would hang the supervisor on a process it does
//! not supervise.
//!
//! ## Rotation
//!
//! By size, `MAX_BYTES` × `GENERATIONS`. `/var` is a persistent Btrfs volume
//! and an unbounded log is a way to fill it — at which point every service that
//! writes anywhere stops working, which is a far bigger outage than a truncated
//! log.
//!
//! ## Shutdown
//!
//! Every handle under `/var` must be CLOSED before `/etc/shutdown` runs:
//! `umount /var` fails EBUSY against an open file, `/etc/shutdown` withholds
//! its marker on a failed unmount, and the boot oracle greps for that marker —
//! so one stray descriptor presents as a mount bug. `close_all` asks the
//! writers to finish and waits, but only up to a deadline: a writer wedged in
//! `write(2)` on a stalled filesystem cannot be recovered, and blocking the
//! shutdown on it would trade a lost log for a machine that never powers off.
//! What is abandoned still holds its fd, which is what the marker tripwire is
//! for.

use std::collections::VecDeque;
use std::fs;
use std::io::{self, BufRead, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// Rotate at this size, keeping this many older generations beside the live
/// file — a ceiling of `MAX_BYTES * (GENERATIONS + 1)` per service.
pub const MAX_BYTES: u64 = 256 * 1024;
pub const GENERATIONS: u32 = 4;

/// How many lines may be waiting before new ones are dropped.
///
/// Bounded because the queue is the whole point: an unbounded one moves the
/// failure from "the log lost lines" to "td-svc grew until the machine died",
/// and td-svc is PID 1's only child that must not do that.
pub const CAPACITY: usize = 1024;

/// …and how many BYTES, which is the bound that actually holds.
///
/// A line count is not a memory bound: `next_line` returns up to `MAX_LINE`
/// plus whatever the reader had buffered, and a lossy conversion of non-UTF-8
/// input can treble that, so 1024 lines is tens of megabytes per service in
/// the worst case — reachable by any service printing binary faster than a
/// slow `/var` drains. The claim this module makes is that td-svc does not
/// grow until the machine dies, and only a byte budget makes it true.
pub const CAPACITY_BYTES: usize = 1024 * 1024;

/// Longest line written as one line. A service that emits megabytes with no
/// newline is split rather than buffered — the alternative is a drain that
/// allocates without bound on output nobody framed.
const MAX_LINE: usize = 8 * 1024;

/// How often an idle writer wakes to re-check for a stop it may have missed.
const WRITER_TICK: Duration = Duration::from_millis(200);

/// Log files can carry anything a service prints, including material that was
/// only ever meant for root. td-svc inherits PID 1's umask, which PID 1 never
/// sets, so the mode has to be stated rather than left to it.
const FILE_MODE: u32 = 0o600;
/// Same reasoning for the directory `log=` names: one td-svc creates is not a
/// place anyone else may put a file for it to append to.
const DIR_MODE: u32 = 0o700;

/// `O_NOFOLLOW` — the log path's final component must be a real file, not a
/// symlink somebody else planted.
///
/// The mode above protects only what td-svc CREATES. A `log=` naming a file in
/// a directory that already exists and is writable by others is a way to have
/// root-owned service output appended to a file of the attacker's choosing;
/// this refuses to follow the link rather than reasoning about which
/// directories are safe.
const O_NOFOLLOW: i32 = 0o400000;

/// The queue between a service's drains and its writer.
#[derive(Default)]
struct Queue {
    lines: VecDeque<String>,
    /// Sum of `lines`' lengths, kept rather than recomputed: this is read on
    /// every push, which is the one path that must not become O(queue).
    bytes: usize,
    /// Lines refused because the queue was full, not yet reported.
    dropped: u64,
    /// Set at shutdown: finish what is queued, then close and stop.
    stopping: bool,
}

/// A service's capture: the queue, the signal, and whether the writer has
/// finished with its file.
pub struct Capture {
    queue: Mutex<Queue>,
    wake: Condvar,
    /// The writer has closed its file and exited. Read by `close_all`, which
    /// cannot join the thread — see the module header.
    closed: AtomicBool,
    name: String,
}

impl Capture {
    /// Queue one line, or count it as dropped.
    ///
    /// Never blocks and never fails: the caller is a drain thread, and the
    /// entire design exists so that this call cannot back up into the service.
    pub fn push(&self, line: String) {
        let mut queue = lock(&self.queue);
        if queue.stopping {
            // The file is closing or closed. Counting these would produce a
            // "lines dropped" marker nothing will ever write.
            return;
        }
        if queue.lines.len() >= CAPACITY
            || queue.bytes.saturating_add(line.len()) > CAPACITY_BYTES
        {
            queue.dropped = queue.dropped.saturating_add(1);
            return;
        }
        queue.bytes = queue.bytes.saturating_add(line.len());
        queue.lines.push_back(line);
        drop(queue);
        self.wake.notify_all();
    }

    /// Ask the writer to finish and close.
    fn stop(&self) {
        lock(&self.queue).stopping = true;
        self.wake.notify_all();
    }

    /// Has the writer closed its file and exited?
    ///
    /// Public because a caller that RETIRED a capture has no other way to see
    /// that the retirement took: `close_all` stops the writer itself, so a
    /// test that asked through it could not tell a retired writer from one it
    /// had just stopped.
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }
}

/// Stop one writer without waiting for it.
///
/// The shutdown path waits (`close_all`); a reload that drops a service must
/// not, because the main loop is answering a control request and a wedged
/// filesystem would hold the whole supervisor there.
pub fn retire(capture: &Arc<Capture>) {
    capture.stop();
}

/// A poisoned lock is not a reason to abort PID 1's supervisor.
///
/// Poisoning means some thread panicked while holding this, and td-svc's rules
/// forbid panicking — but `unwrap` here would turn a bug anywhere in a drain
/// into a dead supervisor, which is strictly worse than carrying on with a
/// queue whose invariants are simply "a `VecDeque` and two counters".
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Where a service's captured output goes, and the rotation state for it.
pub struct Sink {
    path: String,
    max_bytes: u64,
    generations: u32,
    file: Option<fs::File>,
    size: u64,
    /// `console=yes`: a copy of every line, prefixed with the service name so a
    /// console carrying several services can be read.
    console: Option<fs::File>,
    name: String,
    /// Set once the sink has said it cannot write. See `complain`.
    complained: bool,
}

impl Sink {
    pub fn new(name: &str, path: &str, console: Option<fs::File>) -> Sink {
        Sink {
            path: path.to_string(),
            max_bytes: MAX_BYTES,
            generations: GENERATIONS,
            file: None,
            size: 0,
            console,
            name: name.to_string(),
            complained: false,
        }
    }

    /// Open on first use rather than at spawn.
    ///
    /// A service that never prints leaves no file, and — more usefully — a
    /// `/var` that is not mounted yet does not turn into an empty log at the
    /// mount point that the real `/var` then hides.
    fn ensure_open(&mut self) -> io::Result<&mut fs::File> {
        if self.file.is_none() {
            use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
            if let Some(dir) = std::path::Path::new(&self.path).parent() {
                // NOT bare `create_dir_all`: that is `0777 & ~umask`, and
                // td-svc inherits PID 1's umask, which PID 1 never sets. A
                // world-writable log directory lets anyone replace the file
                // this then appends to. Only components it CREATES take this
                // mode, so an existing /var/log keeps its own.
                match fs::DirBuilder::new().recursive(true).mode(DIR_MODE).create(dir) {
                    Ok(()) => {}
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                    Err(e) => return Err(e),
                }
            }
            let file = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .mode(FILE_MODE)
                .custom_flags(O_NOFOLLOW)
                .open(&self.path)?;
            self.size = file.metadata().map(|m| m.len()).unwrap_or(0);
            self.file = Some(file);
        }
        self.file
            .as_mut()
            .ok_or_else(|| io::Error::other("log file vanished between open and use"))
    }

    /// `path.1` … `path.N`, oldest discarded.
    ///
    /// Best effort throughout: a rename that fails leaves the generation it was
    /// moving where it is, and the next write continues into a file that is
    /// merely bigger than intended. Refusing to log because a rotation failed
    /// would lose the output that explains why it failed.
    fn rotate(&mut self) {
        // Closed FIRST: writing through a handle whose name has moved appends
        // to the rotated generation, so the live file would stay empty and the
        // cap would never be enforced again.
        self.file = None;
        self.size = 0;
        if self.generations == 0 {
            let _ = fs::remove_file(&self.path);
            return;
        }
        let _ = fs::remove_file(format!("{}.{}", self.path, self.generations));
        for generation in (1..self.generations).rev() {
            let _ = fs::rename(
                format!("{}.{generation}", self.path),
                format!("{}.{}", self.path, generation.saturating_add(1)),
            );
        }
        let _ = fs::rename(&self.path, format!("{}.1", self.path));
    }

    /// Append one line, rotating first if it would not fit.
    fn write_line(&mut self, line: &str) {
        if let Some(console) = self.console.as_mut() {
            // ONE `write_all` of one buffer. `writeln!` issues a `write(2)` per
            // format fragment, and two `console=yes` services would interleave
            // mid-line — defeating the very prefix that makes a shared console
            // readable.
            let mut copy = String::with_capacity(self.name.len().saturating_add(line.len() + 3));
            copy.push_str(&self.name);
            copy.push_str(": ");
            copy.push_str(line);
            copy.push('\n');
            let _ = console.write_all(copy.as_bytes());
        }
        let wanted = line.len().saturating_add(1) as u64;
        // Opened FIRST, because `size` is only known once the existing file has
        // been stat'd: a supervisor that restarts onto an already-full log
        // would otherwise append one line to it before the first rotation, and
        // if that were the only line it ever wrote the file would stay over the
        // ceiling forever.
        if self.ensure_open().is_err() {
            self.complain();
            return;
        }
        // `self.size > 0` so a single line longer than the whole budget is
        // written to an empty file instead of rotating forever without ever
        // recording it.
        if self.size.saturating_add(wanted) > self.max_bytes && self.size > 0 {
            self.rotate();
        }
        let Ok(file) = self.ensure_open() else {
            self.complain();
            return;
        };
        if writeln!(file, "{line}").is_ok() {
            self.size = self.size.saturating_add(wanted);
        } else {
            self.complain();
        }
    }

    /// Say once that this log is not being written.
    ///
    /// Latched rather than rate-limited: a read-only or full `/var` fails on
    /// EVERY line, and a complaint per line would scroll the console with the
    /// message that explains it. Once is what the reader needs — the log's own
    /// absence is the rest of the evidence.
    fn complain(&mut self) {
        if self.complained {
            return;
        }
        self.complained = true;
        crate::supervise::log(&format!(
            "{}: cannot write {}; its output is being discarded",
            self.name, self.path
        ));
    }

    /// Release the file. Called before `/etc/shutdown` runs, and the whole
    /// reason `close_all` exists.
    fn close(&mut self) {
        if let Some(mut file) = self.file.take() {
            let _ = file.flush();
        }
        self.console = None;
    }
}

/// Start a service's writer thread and return the handle its drains push to.
///
/// `None` if the thread could not be started: the caller then runs the service
/// WITHOUT capture rather than not at all, because a service that does not
/// start is a worse answer than a service whose output is not recorded.
pub fn start(name: &str, sink: Sink) -> Option<Arc<Capture>> {
    let capture = Arc::new(Capture {
        queue: Mutex::new(Queue::default()),
        wake: Condvar::new(),
        closed: AtomicBool::new(false),
        name: name.to_string(),
    });
    let mine = Arc::clone(&capture);
    let label = format!("{name}-log");
    let started = crate::supervise::spawn_thread(&label, move || writer(&mine, sink));
    started.then_some(capture)
}

/// Empty the queue into the sink until asked to stop.
fn writer(capture: &Arc<Capture>, mut sink: Sink) {
    let mut batch: Vec<String> = Vec::with_capacity(CAPACITY);
    loop {
        let mut dropped: u64 = 0;
        let stopping;
        {
            let mut queue = lock(&capture.queue);
            while queue.lines.is_empty() && !queue.stopping {
                let (next, _) = capture.wake.wait_timeout(queue, WRITER_TICK).unwrap_or_else(
                    |poisoned| {
                        let (guard, timeout) = poisoned.into_inner();
                        (guard, timeout)
                    },
                );
                queue = next;
            }
            batch.extend(queue.lines.drain(..));
            queue.bytes = 0;
            dropped = dropped.saturating_add(queue.dropped);
            queue.dropped = 0;
            stopping = queue.stopping;
        }
        for line in batch.drain(..) {
            sink.write_line(&line);
        }
        if dropped > 0 {
            // AFTER the batch, so the marker sits where the gap actually is.
            sink.write_line(&format!("... {dropped} lines dropped"));
        }
        if stopping {
            break;
        }
    }
    sink.close();
    // Release/Acquire, not Relaxed: this flag is what tells the shutdown the
    // file is closed, so the `close(2)` above must be visible to whoever sees it.
    capture.closed.store(true, Ordering::Release);
}

/// Read one line, without trusting the producer to frame it.
///
/// `read_line` is not usable here for two reasons: it fails on output that is
/// not UTF-8 (a service printing a binary blob would end its own capture), and
/// it has no length bound (a service printing without newlines would allocate
/// until the machine died). Invalid bytes are replaced and overlong lines are
/// split.
fn next_line(reader: &mut impl BufRead, buf: &mut Vec<u8>) -> io::Result<bool> {
    buf.clear();
    loop {
        let available = match reader.fill_buf() {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        if available.is_empty() {
            return Ok(!buf.is_empty());
        }
        match available.iter().position(|byte| *byte == b'\n') {
            Some(at) => {
                buf.extend_from_slice(available.get(..at).unwrap_or(&[]));
                reader.consume(at.saturating_add(1));
                return Ok(true);
            }
            None => {
                let taken = available.len();
                buf.extend_from_slice(available);
                reader.consume(taken);
                if buf.len() >= MAX_LINE {
                    return Ok(true);
                }
            }
        }
    }
}

/// Drain one stream into a capture until EOF.
///
/// Not joined by anyone: a descendant holding the write end keeps this reader
/// blocked after the service itself has exited, and the supervisor must not
/// wait on a process it does not supervise.
pub fn drain(name: &str, stream: impl io::Read + Send + 'static, capture: Arc<Capture>) {
    let label = format!("{name}-drain");
    // A drain that cannot START drops the stream it was handed, closing the
    // read end — so the service's next write takes SIGPIPE and dies, rather
    // than blocking forever on a pipe nobody empties. That is the better of
    // the two failures still available once the pipe exists; the case where
    // NO writer could be started never gets a pipe at all.
    let _ = crate::supervise::spawn_thread(&label, move || {
        let mut reader = io::BufReader::new(stream);
        let mut buf = Vec::with_capacity(MAX_LINE);
        loop {
            match next_line(&mut reader, &mut buf) {
                Ok(true) => capture.push(String::from_utf8_lossy(&buf).into_owned()),
                Ok(false) => return,
                Err(_) => return,
            }
        }
    });
}

/// Close every capture's file, waiting up to `within` for the writers.
///
/// Returns the names still holding a file when the deadline passed, which is
/// exactly the set that can make `umount /var` fail — worth naming on the
/// console, because the symptom otherwise appears much later and looks like a
/// mount bug rather than a log one.
pub fn close_all(captures: &[Arc<Capture>], within: Duration) -> Vec<String> {
    for capture in captures {
        capture.stop();
    }
    if let Some(deadline) = Instant::now().checked_add(within) {
        while Instant::now() < deadline {
            if captures.iter().all(|capture| capture.is_closed()) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    captures
        .iter()
        .filter(|capture| !capture.is_closed())
        .map(|capture| capture.name.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn scratch(tag: &str) -> String {
        let dir = format!(
            "{}/td-svc-logs-{}-{tag}",
            std::env::temp_dir().display(),
            std::process::id()
        );
        let _ = fs::create_dir_all(&dir);
        dir
    }

    fn read(path: &str) -> String {
        fs::read_to_string(path).unwrap_or_default()
    }

    /// Lines reach the file, in order.
    #[test]
    fn captured_lines_reach_the_log() {
        let dir = scratch("basic");
        let path = format!("{dir}/a.log");
        let capture = start("a", Sink::new("a", &path, None)).unwrap();
        capture.push("first".to_string());
        capture.push("second".to_string());
        let left = close_all(&[Arc::clone(&capture)], Duration::from_secs(5));
        assert!(left.is_empty(), "the writer did not close: {left:?}");
        assert_eq!(read(&path), "first\nsecond\n");
        let _ = fs::remove_dir_all(&dir);
    }

    /// A full queue drops lines and SAYS so. A silent gap is indistinguishable
    /// from a service that went quiet, which is the fact the log is being read
    /// to establish.
    #[test]
    fn a_full_queue_drops_lines_and_reports_how_many() {
        let dir = scratch("drop");
        // Built by hand, with no writer running, so the queue is provably full
        // rather than merely raced into being full.
        let capture = Arc::new(Capture {
            queue: Mutex::new(Queue::default()),
            wake: Condvar::new(),
            closed: AtomicBool::new(false),
            name: "a".to_string(),
        });
        for i in 0..CAPACITY + 25 {
            capture.push(format!("line {i}"));
        }
        {
            let queue = lock(&capture.queue);
            assert_eq!(queue.lines.len(), CAPACITY, "the queue grew past its bound");
            assert_eq!(queue.dropped, 25, "the overflow was not counted");
        }
        let _ = fs::remove_dir_all(&dir);
    }

    /// The queue is bounded in BYTES as well as lines.
    ///
    /// A line count is not a memory bound: a service printing long lines
    /// faster than a slow `/var` drains would otherwise hold tens of megabytes
    /// per service, and "td-svc grew until the machine died" is the outcome
    /// this bound claims to prevent.
    #[test]
    fn a_queue_full_of_long_lines_is_bounded_by_bytes_not_by_count() {
        let capture = Arc::new(Capture {
            queue: Mutex::new(Queue::default()),
            wake: Condvar::new(),
            closed: AtomicBool::new(false),
            name: "a".to_string(),
        });
        let long = "x".repeat(64 * 1024);
        for _ in 0..64 {
            capture.push(long.clone());
        }
        let queue = lock(&capture.queue);
        assert!(
            queue.lines.len() < CAPACITY,
            "the line count alone would have admitted all of these"
        );
        assert!(
            queue.bytes <= CAPACITY_BYTES,
            "the queue holds {} bytes, past its {CAPACITY_BYTES} budget",
            queue.bytes
        );
        assert!(queue.dropped > 0, "nothing was reported as dropped");
    }

    /// The drop marker is written into the log itself.
    #[test]
    fn the_drop_marker_lands_in_the_log() {
        let dir = scratch("marker");
        let path = format!("{dir}/a.log");
        let capture = start("a", Sink::new("a", &path, None)).unwrap();
        {
            let mut queue = lock(&capture.queue);
            queue.dropped = 7;
        }
        capture.push("after".to_string());
        let left = close_all(&[Arc::clone(&capture)], Duration::from_secs(5));
        assert!(left.is_empty(), "{left:?}");
        let text = read(&path);
        assert!(
            text.contains("... 7 lines dropped"),
            "the gap was silent: {text:?}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// Rotation caps the live file and keeps N generations, oldest discarded.
    #[test]
    fn the_log_rotates_and_keeps_a_bounded_number_of_generations() {
        let dir = scratch("rotate");
        let path = format!("{dir}/a.log");
        let mut sink = Sink::new("a", &path, None);
        sink.max_bytes = 16;
        sink.generations = 2;
        for i in 0..12 {
            sink.write_line(&format!("line{i}"));
        }
        sink.close();

        assert!(
            fs::metadata(&path).is_ok_and(|m| m.len() <= 16),
            "the live file is missing or outgrew its cap"
        );
        assert!(
            std::path::Path::new(&format!("{path}.1")).exists(),
            "nothing was rotated"
        );
        assert!(
            std::path::Path::new(&format!("{path}.2")).exists(),
            "the second generation is missing"
        );
        assert!(
            !std::path::Path::new(&format!("{path}.3")).exists(),
            "generations grew past the bound; /var fills up again"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// A supervisor restarting onto an ALREADY-FULL log rotates it, rather
    /// than appending one more line to a file that is already over the cap.
    ///
    /// The size is only known once the file has been stat'd, so a check that
    /// runs before the open reads `size == 0` and skips the rotation. If that
    /// line were the only one the new supervisor ever wrote, the file would
    /// stay over the ceiling for the life of the boot.
    #[test]
    fn a_log_that_is_already_full_rotates_on_the_first_line_after_a_restart() {
        let dir = scratch("persisted");
        let path = format!("{dir}/a.log");
        let _ = fs::write(&path, "x".repeat(64));

        // A fresh sink, as a restarted supervisor would build.
        let mut sink = Sink::new("a", &path, None);
        sink.max_bytes = 16;
        sink.generations = 2;
        sink.write_line("new");
        sink.close();

        assert!(
            std::path::Path::new(&format!("{path}.1")).exists(),
            "the persisted log was not rotated; it stays over the ceiling"
        );
        assert_eq!(
            read(&path).trim_end(),
            "new",
            "the new line did not land in a fresh live file"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// A line longer than the whole budget is still recorded rather than
    /// rotating the file away forever trying to make room for it.
    #[test]
    fn a_line_bigger_than_the_budget_is_still_written() {
        let dir = scratch("huge");
        let path = format!("{dir}/a.log");
        let mut sink = Sink::new("a", &path, None);
        sink.max_bytes = 8;
        sink.generations = 1;
        sink.write_line(&"x".repeat(64));
        sink.close();
        assert_eq!(read(&path).trim_end().len(), 64, "the long line was lost");
        let _ = fs::remove_dir_all(&dir);
    }

    /// Output that is not UTF-8 does not end the capture, and an unframed
    /// stream is split rather than buffered without bound.
    #[test]
    fn a_drain_survives_binary_output_and_bounds_an_unframed_line() {
        let mut reader = io::BufReader::new(&b"ok\n\xff\xfe\n"[..]);
        let mut buf = Vec::new();
        assert!(next_line(&mut reader, &mut buf).unwrap());
        assert_eq!(String::from_utf8_lossy(&buf), "ok");
        assert!(next_line(&mut reader, &mut buf).unwrap());
        assert_eq!(String::from_utf8_lossy(&buf), "\u{fffd}\u{fffd}");
        assert!(!next_line(&mut reader, &mut buf).unwrap(), "EOF not reported");

        let unframed = vec![b'z'; MAX_LINE * 2];
        let mut reader = io::BufReader::new(&unframed[..]);
        assert!(next_line(&mut reader, &mut buf).unwrap());
        assert_eq!(
            buf.len(),
            MAX_LINE,
            "an unframed stream was buffered past the bound"
        );
        let _ = &reader;
    }

    /// Does this process still hold a descriptor on that path?
    ///
    /// The EBUSY condition itself, read the way the kernel sees it, rather than
    /// a proxy for it.
    fn holds_open(path: &str) -> bool {
        let Ok(entries) = fs::read_dir("/proc/self/fd") else {
            return false;
        };
        entries.flatten().any(|entry| {
            fs::read_link(entry.path()).is_ok_and(|target| target == std::path::Path::new(path))
        })
    }

    /// A log path that is a symlink is refused, not followed.
    ///
    /// `DIR_MODE` protects only directories td-svc created, so a `log=` inside
    /// a pre-existing writable directory is otherwise a way to have root-owned
    /// service output appended to a file of somebody else's choosing.
    #[test]
    fn a_log_path_that_is_a_symlink_is_not_followed() {
        let dir = scratch("symlink");
        let target = format!("{dir}/target");
        let link = format!("{dir}/a.log");
        let _ = fs::write(&target, "");
        let _ = fs::remove_file(&link);
        if std::os::unix::fs::symlink(&target, &link).is_err() {
            eprintln!("note: cannot create a symlink here; skipping");
            let _ = fs::remove_dir_all(&dir);
            return;
        }
        let mut sink = Sink::new("a", &link, None);
        sink.write_line("secret");
        sink.close();
        assert_eq!(
            read(&target),
            "",
            "output was appended through a symlink to a file td-svc does not own"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// The shutdown close really releases the file.
    ///
    /// This is the property `umount /var` depends on: `/etc/shutdown` withholds
    /// its marker on a failed unmount and the boot oracle greps for it, so a
    /// descriptor left open here presents much later as a mount bug. Asserting
    /// on `close_all`'s return value alone would prove only that the writer
    /// SAID it was done.
    #[test]
    fn closing_releases_the_descriptor_umount_would_trip_on() {
        let dir = scratch("fd");
        let path = format!("{dir}/a.log");
        let capture = start("a", Sink::new("a", &path, None)).unwrap();
        capture.push("something".to_string());
        // Waited for, so the assertion below is about the CLOSE rather than
        // racing the open.
        let mut opened = false;
        for _ in 0..500 {
            if holds_open(&path) {
                opened = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        if !opened {
            // Only a /proc we cannot READ is a skip. `holds_open` answers
            // `false` both for that and for "never opened", so a regression
            // that stopped `ensure_open` from opening would otherwise turn the
            // one test of the actual EBUSY property into a silent no-op.
            assert!(
                fs::read_dir("/proc/self/fd").is_err(),
                "the writer never opened the log at all"
            );
            eprintln!("note: cannot see /proc/self/fd here; skipping");
            let _ = fs::remove_dir_all(&dir);
            return;
        }
        let left = close_all(&[Arc::clone(&capture)], Duration::from_secs(5));
        assert!(left.is_empty(), "the writer did not close: {left:?}");
        assert!(
            !holds_open(&path),
            "the log descriptor outlived the close; umount /var would fail EBUSY"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// `close_all` names what it could not close, because that set is exactly
    /// what makes `umount /var` fail.
    #[test]
    fn close_all_reports_a_writer_it_could_not_close() {
        let stuck = Arc::new(Capture {
            queue: Mutex::new(Queue::default()),
            wake: Condvar::new(),
            closed: AtomicBool::new(false),
            name: "stuck".to_string(),
        });
        let left = close_all(&[Arc::clone(&stuck)], Duration::from_millis(50));
        assert_eq!(
            left,
            vec!["stuck".to_string()],
            "a writer that never closed was reported as closed"
        );
    }

    /// A push after the stop is not counted as dropped: it would produce a
    /// marker no writer is left to write.
    #[test]
    fn a_line_arriving_after_the_close_is_not_counted_as_dropped() {
        let capture = Arc::new(Capture {
            queue: Mutex::new(Queue::default()),
            wake: Condvar::new(),
            closed: AtomicBool::new(false),
            name: "a".to_string(),
        });
        capture.stop();
        capture.push("late".to_string());
        let queue = lock(&capture.queue);
        assert!(queue.lines.is_empty(), "a line was queued after the close");
        assert_eq!(queue.dropped, 0, "a marker was armed with no writer left");
    }
}

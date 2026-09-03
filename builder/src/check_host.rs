//! Lazy, rootless per-user host for memory-heavy check entry points.

use std::ffi::{OsStr, OsString};
use std::fs::OpenOptions;
use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::check_memory::{self, HostBudget, TokenPool};

const SOCKET_NAME: &str = "check-host-v2.sock";
const START_LOCK_NAME: &str = "check-host-v2.start.lock";
const HOST_LOCK_NAME: &str = "check-host-v2.host.lock";
const LOG_NAME: &str = "check-host-v2.log";
const PREV_LOG_NAME: &str = "check-host-v2.log.prev";
/// Past this, a log whose rotation keeps failing is discarded rather than
/// grown: the runtime dir is usually a tmpfs.
const MAX_LOG_BYTES: u64 = 4 * 1024 * 1024;
/// How long `accept` may fail CONTINUOUSLY before the host gives up and says
/// so, rather than reporting the same failure into its log forever.
///
/// A duration rather than a count: the thing being detected is a failure that
/// never clears, and a count cannot tell that apart from one that clears in a
/// few seconds. Under the two-concurrent-gate load this host exists for, a
/// transient EMFILE tearing the host down would start a restart storm that
/// burns both log generations.
const MAX_ACCEPT_FAILURE: Duration = Duration::from_secs(60);
/// How long a hand-off to the worker pool waits between attempts.
const HANDOFF_POLL: Duration = Duration::from_millis(100);
/// How long `check-host-stop` waits for its answer before reporting that it
/// did not get one. A stop needs a free worker to dequeue it, so a wedged pool
/// would otherwise hang the command with nothing said anywhere.
const STOP_TIMEOUT: Duration = Duration::from_secs(10);
/// How long one connection may wait for a worker before it is dropped.
///
/// `stop` is set only by a worker that has DEQUEUED a stop request, so a pool
/// with no worker able to dequeue anything can never be asked to stop. Waiting
/// on that flag would be waiting on something the wedged case cannot produce,
/// so the wait is bounded by the clock instead and the loop goes back to
/// accepting. The client whose connection is dropped reads an ending and is
/// pointed at this log, which says what happened to it.
const HANDOFF_TIMEOUT: Duration = Duration::from_secs(30);
const MAGIC: &[u8; 8] = b"TDCHV2\0\n";
const REQUEST_RUN: u8 = 1;
const REQUEST_PING: u8 = 2;
const REQUEST_STOP: u8 = 3;
const FRAME_STDOUT: u8 = 1;
const FRAME_STDERR: u8 = 2;
const FRAME_EXIT: u8 = 3;
const FRAME_PONG: u8 = 4;
const MAX_FIELD_BYTES: usize = 64 * 1024;
const MAX_REQUEST_BYTES: usize = 1024 * 1024;
const MAX_ARGS: usize = 512;
const MAX_ENVS: usize = 4096;
const IO_CHUNK_BYTES: usize = 64 * 1024;
const WORKER_THREADS: usize = 32;
const WORKER_STACK_BYTES: usize = 512 * 1024;
const START_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const OUTPUT_TIMEOUT: Duration = Duration::from_secs(5);
/// How long one write may make NO progress before its client is treated as
/// stuck rather than slow.
///
/// `OUTPUT_TIMEOUT` is how often the write wakes up to look, NOT the verdict.
/// Reading a 5s socket timeout as "the client is gone" ends a multi-minute
/// hosted check because a log reader paused, which is a far worse outcome than
/// waiting. What this still protects against — a client wedged forever holding
/// a worker and its memory permit — needs minutes, not seconds, and real
/// memory pressure is cancelled by the emergency reserve check instead, which
/// is the mechanism actually aimed at that.
const STALL_BUDGET: Duration = Duration::from_secs(300);
const POLL_INTERVAL: Duration = Duration::from_millis(100);
const IDLE_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const FEED_NO_DAEMON_ENV: &str = "TD_FEED_NO_DAEMON";
/// What a client sees when the host stops mid-request.
///
/// A short read at a frame boundary is not a protocol error: the host
/// closed the connection having never sent an exit frame. Naming that is
/// the difference between a diagnosable death and `failed to fill whole
/// buffer`.
const HOST_CLOSED: &str = "the connection ended without an exit frame";

// The socket name is the coordinator ABI. Bump it with any incompatible token
// or wire-policy change; hosted requests always execute their own exact binary,
// so ordinary worktree changes do not make the coordinator a stale builder.

#[derive(Debug)]
struct RunRequest {
    exe: OsString,
    cwd: OsString,
    args: Vec<OsString>,
    env: Vec<(OsString, OsString)>,
}

enum Request {
    Run(RunRequest),
    Ping,
    Stop,
}

fn supervisor_executable(request: &RunRequest) -> &OsStr {
    &request.exe
}

struct FrameWriter {
    stream: Mutex<UnixStream>,
    disconnected: AtomicBool,
    /// Set when this request is being cancelled. Patience is for a client that
    /// might still be served; once the check is dead there is nothing left to
    /// deliver, and a pump parked on a stalled socket would otherwise hold its
    /// worker and its memory permit for the rest of the budget.
    abandoned: AtomicBool,
    /// How long one write may make NO progress before the client is called
    /// stuck. Held per writer so a test can shorten it; production uses
    /// `STALL_BUDGET`.
    stall_budget: Duration,
}

impl FrameWriter {
    fn new(stream: UnixStream) -> io::Result<Arc<Self>> {
        Self::with_budget(stream, STALL_BUDGET)
    }

    fn with_budget(stream: UnixStream, stall_budget: Duration) -> io::Result<Arc<Self>> {
        stream.set_write_timeout(Some(OUTPUT_TIMEOUT))?;
        Ok(Arc::new(Self {
            stream: Mutex::new(stream),
            disconnected: AtomicBool::new(false),
            abandoned: AtomicBool::new(false),
            stall_budget,
        }))
    }

    /// Stop waiting for this client: the request it belonged to is over.
    fn abandon(&self) {
        self.abandoned.store(true, Ordering::Relaxed);
    }

    /// Frame only if no pump holds the stream, and only briefly.
    ///
    /// For cancellation's own last word to the client. `try_lock` because the
    /// pump it would queue behind is precisely the one parked on the stalled
    /// socket, so blocking here would reintroduce the delay cancellation
    /// exists to end. Best effort by design: the host log records the cause
    /// either way, and the client is told to read it.
    fn frame_if_free(&self, kind: u8, bytes: &[u8], budget: Duration) -> io::Result<()> {
        let stream = self
            .stream
            .try_lock()
            .map_err(|_| io::Error::other("check-host output is busy"))?;
        self.frame_locked(stream, kind, bytes, budget, false)
    }

    fn frame(&self, kind: u8, bytes: &[u8]) -> io::Result<()> {
        let stream = self
            .stream
            .lock()
            .map_err(|_| io::Error::other("check-host output lock poisoned"))?;
        self.frame_locked(stream, kind, bytes, self.stall_budget, true)
    }

    fn frame_locked(
        &self,
        mut stream: std::sync::MutexGuard<'_, UnixStream>,
        kind: u8,
        bytes: &[u8],
        budget: Duration,
        heed_abandon: bool,
    ) -> io::Result<()> {
        let len = u32::try_from(bytes.len())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "oversized host frame"))?;
        // Checked under the lock, not before it. A write that gives up may
        // have committed part of a frame, leaving no boundary to resume from,
        // and the stdout and stderr pumps share this writer: a second pump
        // writing its header after that splices it into the first one's body,
        // and the client reports a framing bug that never happened — a
        // manufactured sibling of the very message this file exists to make
        // trustworthy. One writer giving up ends the stream for all of them.
        // A timeout alone no longer truncates anything: only budget
        // exhaustion, cancellation, or a hard error can.
        if self.disconnected.load(Ordering::Relaxed) {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "check-host response stream already gave up",
            ));
        }
        // Destructured rather than indexed: total, panic-free, and it cannot
        // silently produce a zero-length header the way a fallible write into
        // a fixed array could. A malformed header is a desynchronised stream,
        // which is the class of lie this file exists to stop telling.
        let [a, b, c, d] = len.to_be_bytes();
        let header = [kind, a, b, c, d];
        let abandoned = heed_abandon.then_some(&self.abandoned);
        let result = write_patiently(&mut stream, &header, budget, abandoned)
            .and_then(|()| write_patiently(&mut stream, bytes, budget, abandoned));
        if result.is_err() {
            self.disconnected.store(true, Ordering::Relaxed);
            // Give the client a clean EOF to report rather than half a frame
            // to misparse. That is the `Closed` case, which says the host may
            // be gone — which, for this request, it is.
            let _ = stream.shutdown(Shutdown::Write);
        }
        result
    }
}

/// Write every byte, waiting out a client that is slow without burying it.
///
/// Two things go wrong with `write_all` on this socket. It reports a timeout
/// without saying how much it committed, so a partially written frame leaves
/// no boundary to resume from; and it cannot distinguish a client that has
/// died from one whose own stdout blocked for five seconds. The first
/// manufactures framing errors, and the second kills hosted checks that were
/// doing nothing wrong.
///
/// Tracking progress fixes both: the offset keeps the boundary, and the budget
/// — reset by any progress at all — is what separates slow from stuck. A peer
/// that is genuinely gone reports EPIPE or ECONNRESET rather than timing out,
/// so it still fails at once.
fn write_patiently(
    stream: &mut UnixStream,
    buf: &[u8],
    budget: Duration,
    abandoned: Option<&AtomicBool>,
) -> io::Result<()> {
    let mut written = 0usize;
    let mut since_progress = Instant::now();
    while written < buf.len() {
        let rest = buf
            .get(written..)
            .ok_or_else(|| io::Error::other("check-host frame write ran past its buffer"))?;
        match stream.write(rest) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "check-host client accepted no bytes",
                ))
            }
            Ok(n) => {
                written = written.saturating_add(n);
                since_progress = Instant::now();
            }
            // A signal is not progress. Retrying is right — the offset is
            // intact — but exempting it from the budget would let a stream of
            // interruptions hold a worker and its memory permit forever, which
            // is the bound this function advertises.
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::Interrupted
                        | io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                ) =>
            {
                // The socket wakes at least every `OUTPUT_TIMEOUT`, so this
                // is where a cancelled request gets to stop waiting.
                if abandoned.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
                    return Err(io::Error::new(
                        io::ErrorKind::Interrupted,
                        "check-host request was cancelled while writing",
                    ));
                }
                if since_progress.elapsed() >= budget {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        // `{:?}`, not `as_secs()`: a sub-second budget (the
                        // tests use one) would truncate to "0s" and read as
                        // though no time had been given at all.
                        // Not "the client accepted nothing": an EINTR storm
                        // reaches here too, and that is the host's own signal
                        // traffic, not the client's fault.
                        format!("no progress writing to the check-host client for {budget:?}"),
                    ));
                }
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

pub fn should_forward(args: &[String]) -> bool {
    should_forward_with_host_state(
        args,
        std::env::var_os(check_memory::HOST_CHILD_ENV).is_some(),
    )
}

fn should_forward_with_host_state(args: &[String], already_hosted: bool) -> bool {
    if already_hosted {
        return false;
    }
    match args.first().map(String::as_str) {
        Some("check") => !args
            .iter()
            .any(|arg| matches!(arg.as_str(), "-h" | "--help")),
        Some("ready") => !args
            .iter()
            .any(|arg| matches!(arg.as_str(), "-h" | "--help" | "--record-only")),
        Some("affected-checks") => {
            args.iter().any(|arg| arg == "--run")
                && !args
                    .iter()
                    .any(|arg| matches!(arg.as_str(), "-h" | "--help" | "--self-test"))
        }
        Some("gate-run") => !args.iter().any(|arg| {
            matches!(
                arg.as_str(),
                "-h" | "--help" | "--list" | "list-gates" | "gate-timing-report"
            )
        }),
        Some("daemon-request") => {
            args.len() == 3 && args.get(2).is_some_and(|request| request != "SHUTDOWN")
        }
        _ => false,
    }
}

pub fn forward(args: &[String]) -> ExitCode {
    match forward_inner(args) {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            eprintln!("td-builder: check host: {e}");
            ExitCode::FAILURE
        }
    }
}

fn forward_inner(args: &[String]) -> Result<u8, String> {
    let runtime = runtime_dir()?;
    ensure_private_dir(&runtime)?;
    let socket = runtime.join(SOCKET_NAME);
    ensure_server(&runtime, &socket)?;
    // Pin the executable inode before submitting. A request can wait for
    // memory tokens while cargo atomically replaces the worktree binary; a
    // pathname would then select different bytes (or disappear) by the time
    // the host spawned it. The client keeps this descriptor open through the
    // response, and the same-UID host executes it through procfs.
    let pinned_exe = std::fs::File::open("/proc/self/exe")
        .map_err(|e| format!("pin current td-builder executable: {e}"))?;
    let pinned_exe_path = OsString::from(format!(
        "/proc/{}/fd/{}",
        std::process::id(),
        pinned_exe.as_raw_fd()
    ));
    let mut stream = UnixStream::connect(&socket)
        .map_err(|e| format!("connect to {}: {e}", socket.display()))?;
    let request = RunRequest {
        exe: pinned_exe_path,
        cwd: std::env::current_dir()
            .map_err(|e| format!("resolve current directory: {e}"))?
            .into_os_string(),
        args: args.iter().map(OsString::from).collect(),
        env: std::env::vars_os().collect(),
    };
    if let Err(e) = write_run_request(&mut stream, &request) {
        // Submitting fails the same way reading does — the host can die
        // between the connect and the request, and a gate-run issues many
        // requests down fresh connections — but this path used to surface a
        // bare errno with no log named and nothing to read. Ask the socket
        // which it was, with a bound so a live host cannot park us here.
        let _ = stream.set_read_timeout(Some(Duration::from_millis(50)));
        let failure = if peer_gone(&mut stream) {
            ResponseError::Closed
        } else {
            // A live peer means the request never landed: a field or count
            // over this client's own bound, or a write that failed for a
            // reason the host's log cannot explain. Sending the reader to that
            // log would be the same misattribution this file refuses in the
            // other direction for an oversized RECEIVED frame.
            //
            // Note this splits on peer LIVENESS, not on the nature of the
            // error, so a bound violation that races a real host death is
            // still reported as an ending. Splitting on cause would mean
            // checking this client's own bounds before connecting.
            ResponseError::Local(format!("send check-host request: {e}"))
        };
        return Err(describe_response_error(failure, &runtime));
    }
    let result = read_run_frames(&mut stream);
    drop(pinned_exe);
    // A host-side failure is worth a trip to the host's log, and a local one
    // is not. Only generations with something in them are named, and the
    // current one is named as movable: nothing has rotated it yet at the
    // moment this prints, but this death is what makes the next client start
    // the replacement that will.
    // "Dropped" rather than "died": a panicking worker drops the stream with
    // the host still serving, and that is one of the things this can mean.
    result.map_err(|e| describe_response_error(e, &runtime))
}

pub fn serve_cli(args: &[String]) -> ExitCode {
    let Some(runtime) = args.first().map(PathBuf::from) else {
        eprintln!("usage: td-builder check-host-serve RUNTIME");
        return ExitCode::from(2);
    };
    if args.get(1).is_some() {
        eprintln!("usage: td-builder check-host-serve RUNTIME");
        return ExitCode::from(2);
    }
    match serve(&runtime) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("td-builder: check-host-serve: {e}");
            ExitCode::FAILURE
        }
    }
}

pub fn stop_cli() -> ExitCode {
    // Annotated like a run failure. A stop that reports a bare ending is the
    // symptom this commit's own record says to expect a recurrence of, and
    // sending the reader to the host's log is the entire remedy on offer.
    let outcome = runtime_dir().and_then(|runtime| {
        stop(&runtime.join(SOCKET_NAME)).map_err(|e| describe_response_error(e, &runtime))
    });
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("td-builder: check-host-stop: {e}");
            ExitCode::FAILURE
        }
    }
}

fn runtime_dir() -> Result<PathBuf, String> {
    let uid = crate::sys::getuid();
    // The location is a UID property, not a caller-environment property. Using
    // XDG_RUNTIME_DIR directly lets two shells for the same UID select disjoint
    // token pools merely because one omitted or overrode the variable.
    let run_user = PathBuf::from(format!("/run/user/{uid}"));
    if let Ok(metadata) = std::fs::symlink_metadata(&run_user) {
        if metadata.file_type().is_dir() && metadata.uid() == uid {
            return Ok(run_user.join("td-builder"));
        }
    }
    let shm = Path::new("/dev/shm");
    if shm.is_dir() {
        return Ok(shm.join(format!("td-builder-{uid}")));
    }
    // Keep the final fallback a UID property as well. `std::env::temp_dir()`
    // consults TMPDIR, so two shells for one UID could otherwise create
    // independent coordinators and over-admit the same machine.
    Ok(Path::new("/tmp").join(format!("td-builder-{uid}")))
}

fn ensure_private_dir(path: &Path) -> Result<(), String> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => validate_private_dir(path, &metadata),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let mut builder = std::fs::DirBuilder::new();
            builder.mode(0o700);
            if let Err(err) = builder.create(path) {
                if err.kind() != io::ErrorKind::AlreadyExists {
                    return Err(format!(
                        "create check-host runtime {}: {err}",
                        path.display()
                    ));
                }
            }
            let metadata = std::fs::symlink_metadata(path)
                .map_err(|err| format!("inspect check-host runtime {}: {err}", path.display()))?;
            validate_private_dir(path, &metadata)
        }
        Err(e) => Err(format!(
            "inspect check-host runtime {}: {e}",
            path.display()
        )),
    }
}

fn validate_private_dir(path: &Path, metadata: &std::fs::Metadata) -> Result<(), String> {
    if !metadata.file_type().is_dir() || metadata.uid() != crate::sys::getuid() {
        return Err(format!(
            "check-host runtime {} must be a directory owned by uid {}",
            path.display(),
            crate::sys::getuid()
        ));
    }
    if metadata.mode() & 0o077 != 0 {
        return Err(format!(
            "check-host runtime {} must not be accessible by group or other users",
            path.display()
        ));
    }
    Ok(())
}

fn ensure_server(runtime: &Path, socket: &Path) -> Result<(), String> {
    if ping(socket).is_ok() {
        return Ok(());
    }
    let lock_path = runtime.join(START_LOCK_NAME);
    let lock = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(&lock_path)
        .map_err(|e| format!("open host start lock {}: {e}", lock_path.display()))?;
    crate::sys::flock_exclusive(lock.as_raw_fd())
        .map_err(|e| format!("lock host startup {}: {e}", lock_path.display()))?;
    if ping(socket).is_ok() {
        return Ok(());
    }
    // A health request can queue behind all request workers while the host is
    // waiting for memory. Its lifetime flock, rather than ping latency, is the
    // authoritative one-host test. The startup lock closes the probe-to-spawn
    // race between concurrent first callers.
    let host_lock_path = runtime.join(HOST_LOCK_NAME);
    let host_probe = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(&host_lock_path)
        .map_err(|e| format!("open host lifetime lock {}: {e}", host_lock_path.display()))?;
    match crate::sys::flock_try_exclusive(host_probe.as_raw_fd()) {
        Ok(false) if path_is_socket(socket) => return Ok(()),
        Ok(false) => {
            let deadline = Instant::now() + START_TIMEOUT;
            while Instant::now() < deadline {
                if path_is_socket(socket) {
                    return Ok(());
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            // Two shapes reach here and they read very differently. One is a
            // host still starting. The other is a host SHUTTING DOWN: it
            // withdraws the socket name as soon as its accept loop ends but
            // holds the lifetime lock until its last worker finishes, so this
            // window is as long as the longest check still in flight. Calling
            // that a failure to publish sends the reader hunting a startup
            // fault that is really an orderly exit, so name both and point at
            // the log that tells them apart.
            return Err(match host_logs(runtime) {
                Some(logs) => format!(
                    "a host holds the lifetime lock but has not published {} \
                     within {}s: it is starting, or shutting down and still \
                     finishing a check. Its log is {logs}",
                    socket.display(),
                    START_TIMEOUT.as_secs()
                ),
                None => format!(
                    "a host holds the lifetime lock but has not published {} \
                     within {}s, and nothing under {} says whether it is \
                     starting or shutting down",
                    socket.display(),
                    START_TIMEOUT.as_secs(),
                    runtime.display()
                ),
            });
        }
        Ok(true) => drop(host_probe),
        Err(e) => {
            return Err(format!(
                "probe host lifetime lock {}: {e}",
                host_lock_path.display()
            ))
        }
    }

    let (log_path, _) = log_paths(runtime);
    let log = open_fresh_log(runtime)?;
    let log_err = log
        .try_clone()
        .map_err(|e| format!("clone check-host log: {e}"))?;
    // This path pins the caller's current inode across an atomic cargo relink,
    // just as request descriptors below pin the exact submitting binary.
    let mut child = Command::new("/proc/self/exe")
        .arg("check-host-serve")
        .arg(runtime)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err))
        .process_group(0)
        .spawn()
        .map_err(|e| format!("start /proc/self/exe check-host-serve: {e}"))?;

    let deadline = Instant::now() + START_TIMEOUT;
    while Instant::now() < deadline {
        if ping(socket).is_ok() {
            return Ok(());
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                return Err(format!(
                    "check host exited during startup ({status}); see {}",
                    log_path.display()
                ))
            }
            Ok(None) => {}
            Err(e) => return Err(format!("wait for check-host startup: {e}")),
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    // Killing it here leaves a banner and no exit line, which is the
    // signature of a host that died unaccountably. Write into its own log
    // that td did this, so the next reader is not chasing a ghost.
    //
    // Only into a log that already says something, though. A host killed
    // before it could write its banner leaves an empty one, and the empty-log
    // rule is what stops the next start from spending its single generation
    // on it. Appending here would defeat that rule using the one case it was
    // written for, and push a real account out of reach to make room for a
    // line about a host that never spoke. That case goes to the starter's own
    // stderr, which is where its `Err` is about to say the same thing.
    let host_spoke = has_content(&log_path);
    let notice = format!(
        "td-builder: check host {} killed by its starter at {}s: \
         not ready within {}s",
        child.id(),
        epoch_seconds(),
        START_TIMEOUT.as_secs()
    );
    let logged = host_spoke
        && OpenOptions::new()
            .append(true)
            .open(&log_path)
            .map(|mut log| writeln!(log, "{notice}").is_ok())
            .unwrap_or(false);
    if !logged {
        eprintln!("{notice}");
    }
    terminate_tree(child.id());
    let _ = child.wait();
    Err(format!(
        "check host did not become ready within {}s; see {}",
        START_TIMEOUT.as_secs(),
        log_path.display()
    ))
}

/// This host's log, and the one it displaced.
fn log_paths(runtime: &Path) -> (PathBuf, PathBuf) {
    (runtime.join(LOG_NAME), runtime.join(PREV_LOG_NAME))
}

/// The host logs with something in them, for pointing a reader at.
///
/// Existence is not the test: this rotation deliberately leaves 0-byte logs
/// behind a host that was killed before it could speak, and naming one first
/// would send the reader to an empty file while the account it wants sits in
/// the other. They are listed rather than related, because after that case the
/// older one is not the host this one replaced.
fn host_logs(runtime: &Path) -> Option<String> {
    let (log_path, previous) = log_paths(runtime);
    match (has_content(&log_path), has_content(&previous)) {
        (false, false) => None,
        (false, true) => Some(previous.display().to_string()),
        // The current generation is not a stable address. The death being
        // reported is exactly what makes the next client start a replacement,
        // and that replacement rotates this file into `.prev` — usually before
        // a human reads the CI output naming it. Say where it is and where it
        // will have gone, or the pointer is stale on arrival.
        (true, false) => Some(format!(
            "{} (or {}, if a replacement host has since rotated it)",
            log_path.display(),
            previous.display()
        )),
        // Same reasoning as above, and here rotation also DESTROYS: the
        // replacement's rename overwrites `.prev`, so by the time this is
        // read the older account may be gone rather than merely moved.
        (true, true) => Some(format!(
            "{} and {} (a replacement host rotating the first would displace \
             the second)",
            log_path.display(),
            previous.display()
        )),
    }
}

/// Whether a log says anything at all.
///
/// One definition, three callers: which generation is worth naming, whether a
/// generation is worth keeping, and whether the starter may append its kill
/// notice. They have to agree — a log that is "empty" for rotation but "has
/// content" for the notice would rotate away a real account to keep a line
/// about a host that never spoke.
fn has_content(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|metadata| metadata.len() > 0)
        .unwrap_or(false)
}

/// Open a fresh log without destroying the outgoing host's.
///
/// A host that dies is replaced by the very next client, so truncating here
/// would make the recovery erase the only record of the death — the client is
/// left with an unexplained EOF and the reason is already gone. Exactly one
/// generation is retained, which is exactly the restart that follows a death.
/// Rotation is best effort: a first start has nothing to move, and a rename
/// that fails must not keep a host from starting.
fn open_fresh_log(runtime: &Path) -> Result<std::fs::File, String> {
    let (log_path, previous) = log_paths(runtime);
    let len = std::fs::metadata(&log_path)
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    // Only a log that says something is worth a generation. A host killed
    // before it could write its banner leaves an empty one, and rotating that
    // would push an earlier host's account of its death out of reach — which
    // is the case this rotation exists for, and the case a loaded machine
    // produces twice in a row.
    let mut keep = len > 0;
    let mut rotate_failure = None;
    if keep {
        match std::fs::rename(&log_path, &previous) {
            // Discarding here would destroy exactly the record this function
            // exists to keep, so it is kept and appended to. A rotation that
            // cannot happen is an environment fault, not a detail.
            Err(e) => rotate_failure = Some(e),
            Ok(()) => keep = false,
        }
    }
    // Rotation has been failing long enough to matter. The runtime dir is
    // usually a tmpfs, so an unbounded log is a worse failure than a lost one.
    let over_cap = keep && len > MAX_LOG_BYTES;
    if over_cap {
        keep = false;
    }
    // Reported once, and only after the cap has had its say: announcing
    // "appending rather than discarding" and then discarding two lines later
    // would make the first sentence false in the one run that most needs to be
    // believed.
    if let Some(e) = rotate_failure {
        eprintln!(
            "td-builder: could not rotate {} ({e}); {}",
            log_path.display(),
            if over_cap {
                "and it is past the size cap"
            } else {
                "appending to it rather than discarding the previous host's log"
            }
        );
    }
    // O_APPEND, always. The starter writes into this file too (when it kills a
    // host for missing its deadline), through a different file description
    // with its own offset — without O_APPEND the host's next write lands at
    // its stale offset and overwrites what the starter just said.
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(&log_path)
        .map_err(|e| format!("open check-host log {}: {e}", log_path.display()))?;
    if !keep {
        // TRUNCATED IN PLACE, never unlinked. The start flock is held across
        // spawn and the readiness wait, so on the ordinary path no second
        // client reaches here while a host runs. The window is narrower than
        // that: a starter that dies mid-startup releases the flock before its
        // host has taken the lifetime lock, and a host between `serve`
        // returning and the process exiting still writes to this log with the
        // lock already released. In either, unlinking leaves a live writer
        // holding an inode with no name, so its account exists nowhere a
        // reader can reach — the disappearance this function exists to
        // prevent, caused by the function itself. `ftruncate` on an O_APPEND
        // descriptor keeps the file, and any writer resumes at the new end.
        //
        // Reported after the fact, never before: whatever stopped the rename
        // usually stops this too. Announcing the discard first would claim a
        // cap that is not being enforced.
        match log.set_len(0) {
            Ok(()) if over_cap => eprintln!(
                "td-builder: discarded {} ({len} bytes, over the \
                 {MAX_LOG_BYTES}-byte cap and unrotatable)",
                log_path.display()
            ),
            Ok(()) => {}
            Err(e) => eprintln!(
                "td-builder: could not clear {} ({e}); appending to it instead",
                log_path.display()
            ),
        }
    }
    Ok(log)
}

fn epoch_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or(0)
}

fn path_is_socket(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .map(|metadata| metadata.file_type().is_socket())
        .unwrap_or(false)
}

fn ping(socket: &Path) -> Result<(), String> {
    let mut stream = UnixStream::connect(socket).map_err(|e| e.to_string())?;
    stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .map_err(|e| e.to_string())?;
    write_control_request(&mut stream, REQUEST_PING)?;
    let (kind, bytes) = read_frame(&mut stream).map_err(FrameError::message)?;
    if kind == FRAME_PONG && bytes.is_empty() {
        Ok(())
    } else {
        Err("check host returned an invalid ping response".to_string())
    }
}

fn stop(socket: &Path) -> Result<(), ResponseError> {
    let mut stream = UnixStream::connect(socket)
        .map_err(|e| ResponseError::Local(format!("connect to {}: {e}", socket.display())))?;
    // Bounded, like `ping`. A stop queued behind 32 busy workers is never
    // dequeued, and without this the CLI simply hangs with no output and
    // nothing in the log to explain the silence.
    stream
        .set_read_timeout(Some(STOP_TIMEOUT))
        .map_err(|e| ResponseError::Local(format!("set check-host stop timeout: {e}")))?;
    write_control_request(&mut stream, REQUEST_STOP).map_err(ResponseError::Local)?;
    let (kind, bytes) = read_frame(&mut stream).map_err(|e| match e {
        FrameError::Closed => ResponseError::Closed,
        // Local, not Host: the host's log cannot explain a request no worker
        // ever dequeued, and a bare EAGAIN here would be indistinguishable
        // from the failure this commit exists to make legible. Say what the
        // silence means and what to do about it instead.
        FrameError::TimedOut => ResponseError::Local(format!(
            "the check host did not answer a stop request within {}s; a stop \
             needs a free worker to dequeue it, so a pool whose workers are \
             all wedged cannot be stopped this way — kill the host instead",
            STOP_TIMEOUT.as_secs()
        )),
        FrameError::Other(message) => ResponseError::Host(message),
    })?;
    if kind == FRAME_EXIT && bytes == 0u32.to_be_bytes() {
        Ok(())
    } else {
        Err(ResponseError::Host(
            "check host returned an invalid stop response".to_string(),
        ))
    }
}

fn serve(runtime: &Path) -> Result<(), String> {
    ensure_private_dir(runtime)?;
    let host_lock_path = runtime.join(HOST_LOCK_NAME);
    let host_lock = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(&host_lock_path)
        .map_err(|e| format!("open host lifetime lock {}: {e}", host_lock_path.display()))?;
    if !crate::sys::flock_try_exclusive(host_lock.as_raw_fd())
        .map_err(|e| format!("lock host lifetime {}: {e}", host_lock_path.display()))?
    {
        return Err("another per-user check host is already running".to_string());
    }
    let budget = HostBudget::discover()?;
    let token_dir = runtime.join("memory-tokens-v1");
    let pool = TokenPool::create(&token_dir, &budget)?;
    let socket = runtime.join(SOCKET_NAME);
    let _ = std::fs::remove_file(&socket);
    let listener = UnixListener::bind(&socket)
        .map_err(|e| format!("bind check-host socket {}: {e}", socket.display()))?;
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| format!("chmod check-host socket {}: {e}", socket.display()))?;
    listener
        .set_nonblocking(true)
        .map_err(|e| format!("make check-host listener nonblocking: {e}"))?;
    eprintln!(
        "td-builder: per-user check host {} listening on {} at {}s \
         ({} GiB work, {} GiB reserve, {} tokens)",
        std::process::id(),
        socket.display(),
        epoch_seconds(),
        budget.work_bytes / check_memory::GIB,
        budget.reserve_bytes / check_memory::GIB,
        budget.token_count
    );

    let (send, recv) = std::sync::mpsc::sync_channel::<UnixStream>(WORKER_THREADS);
    let recv = Arc::new(Mutex::new(recv));
    let stop = Arc::new(AtomicBool::new(false));
    let active = Arc::new(AtomicUsize::new(0));
    let mut workers = Vec::with_capacity(WORKER_THREADS);
    for index in 0..WORKER_THREADS {
        let recv = recv.clone();
        let stop = stop.clone();
        let active = active.clone();
        let pool = pool.clone();
        let budget = budget.clone();
        let runtime = runtime.to_path_buf();
        let token_dir = token_dir.clone();
        let worker = std::thread::Builder::new()
            .name(format!("td-check-host-{index}"))
            .stack_size(WORKER_STACK_BYTES)
            .spawn(move || worker_loop(recv, stop, active, pool, budget, runtime, token_dir))
            .map_err(|e| format!("spawn check-host worker {index}: {e}"))?;
        workers.push(worker);
    }
    // The workers are now the only owners. Holding a receiver here too would
    // make the `send` below unable to ever fail, so a pool emptied by panics
    // would fill the channel and block this loop forever, socket still bound
    // and lock still held — a host that answers nothing and says nothing.
    drop(recv);

    let mut idle_since = Instant::now();
    // An ORDERLY exit has to be distinguishable from a death. Without this the
    // log of a host that is gone looks the same either way, and a client's EOF
    // cannot be attributed.
    let mut reason = "stop request";
    // An accept error that never clears (EMFILE, ENOBUFS) would otherwise be
    // reported forever into the one file that is supposed to be the record,
    // never reaching the idle check, holding the lock and the socket.
    let mut failing_since: Option<Instant> = None;
    'accept: while !stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _)) => {
                failing_since = None;
                idle_since = Instant::now();
                // Workers that are alive but wedged — parked on a memory token
                // that never frees, or polling a child that never exits for a
                // client that never leaves — fill the channel without ever
                // dropping their receivers. A blocking `send` would then park
                // here for good: `stop` never rechecked, lifetime lock held,
                // socket still bound, which is the same mute hang the receiver
                // fix removed, reached from the other side.
                let mut pending = stream;
                let waiting_since = Instant::now();
                loop {
                    match send.try_send(pending) {
                        Ok(()) => break,
                        Err(std::sync::mpsc::TrySendError::Full(returned)) => {
                            if stop.load(Ordering::Relaxed) {
                                break 'accept;
                            }
                            if waiting_since.elapsed() >= HANDOFF_TIMEOUT {
                                // Dropped, and said so. Every worker is busy
                                // and none has freed in 30s, so holding this
                                // connection only adds a second mute client to
                                // a host that already cannot explain itself.
                                // What is known is that nobody dequeued for
                                // 30s and the queue is full. How many workers
                                // are alive to be busy is NOT known, because a
                                // panicking worker shrinks the pool, so this
                                // says the evidence and not the inference.
                                eprintln!(
                                    "td-builder: check host dropped a connection after {}s: \
                                     no worker took it and the {WORKER_THREADS}-slot queue \
                                     is full at {}s",
                                    HANDOFF_TIMEOUT.as_secs(),
                                    epoch_seconds()
                                );
                                drop(returned);
                                break;
                            }
                            pending = returned;
                            std::thread::sleep(HANDOFF_POLL);
                        }
                        Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                            reason = "no worker left to accept requests";
                            break 'accept;
                        }
                    }
                }
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                // EAGAIN with nothing pending is the benign case, not a run.
                failing_since = None;
                if active.load(Ordering::Relaxed) == 0 && idle_since.elapsed() >= IDLE_TIMEOUT {
                    reason = "idle timeout";
                    break 'accept;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                eprintln!("td-builder: check host accept error: {e}");
                let started = *failing_since.get_or_insert_with(Instant::now);
                if started.elapsed() >= MAX_ACCEPT_FAILURE {
                    reason = "accept kept failing";
                    break 'accept;
                }
                // The same pause the WouldBlock arm takes, so a transient
                // error does not spin between here and the cap.
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
    // Unlinked FIRST, before the joins. The listener is bound until `serve`
    // returns, so between the loop ending and the process exiting a client can
    // still connect, land in a backlog nobody will ever accept, and read an
    // ending from a host that is doing exactly what it was asked to do — a
    // phantom death reported by the very command that caused the shutdown.
    // ENOENT is the honest answer in that window: there is nothing to talk to.
    // Established connections are unaffected; only the name goes.
    let _ = std::fs::remove_file(&socket);
    drop(send);
    for worker in workers {
        let _ = worker.join();
    }
    eprintln!(
        "td-builder: check host {} exiting on {reason} at {}s",
        std::process::id(),
        epoch_seconds()
    );
    Ok(())
}

/// Holds the active-request count for as long as a request is in flight.
///
/// The count is what stops the accept loop idling out under a live request, so
/// a request that unwinds past a bare decrement would leak one forever and the
/// host could never exit again. A guard cannot be skipped by an unwind.
struct ActiveGuard(Arc<AtomicUsize>);

impl ActiveGuard {
    fn new(active: &Arc<AtomicUsize>) -> Self {
        active.fetch_add(1, Ordering::Relaxed);
        Self(active.clone())
    }
}

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

fn worker_loop(
    recv: Arc<Mutex<std::sync::mpsc::Receiver<UnixStream>>>,
    stop: Arc<AtomicBool>,
    active: Arc<AtomicUsize>,
    pool: TokenPool,
    budget: HostBudget,
    runtime: PathBuf,
    token_dir: PathBuf,
) {
    loop {
        let next = match recv.lock() {
            Ok(receiver) => receiver.recv(),
            Err(_) => return,
        };
        let mut stream = match next {
            Ok(stream) => stream,
            Err(_) => return,
        };
        let deadline = Instant::now()
            .checked_add(REQUEST_TIMEOUT)
            .unwrap_or_else(Instant::now);
        match read_request(&mut stream, deadline) {
            Ok(Request::Ping) => {
                // Same reasoning as the stop arm: a ping that goes
                // unanswered reads at the client as a host that vanished.
                match FrameWriter::new(stream).and_then(|writer| writer.frame(FRAME_PONG, &[])) {
                    Ok(()) => {}
                    Err(e) => {
                        eprintln!("td-builder: check host could not answer a ping: {e}")
                    }
                }
            }
            Ok(Request::Stop) => {
                // Answer first, then arm the shutdown. `serve` joins its
                // workers, so this ordering is not what keeps the reply alive
                // today; it is what stops a future teardown that does not join
                // from turning an orderly exit into the EOF a dead host makes.
                // A reply that cannot be sent still stops the host.
                //
                // Reported, never dropped: an unanswered stop leaves the
                // stopper reading that same EOF and reporting a host that may
                // have died, while this one exits perfectly normally. That is
                // the exact confusion this commit exists to remove, so the
                // one path that can cause it silently is made to speak.
                match FrameWriter::new(stream)
                    .and_then(|writer| writer.frame(FRAME_EXIT, &0u32.to_be_bytes()))
                {
                    Ok(()) => {}
                    Err(e) => {
                        eprintln!("td-builder: check host could not answer the stop request: {e}")
                    }
                }
                stop.store(true, Ordering::Relaxed);
            }
            Ok(Request::Run(request)) => {
                let _active = ActiveGuard::new(&active);
                // ONE writer for this connection, built here and shared with
                // the error path. A second `FrameWriter` over a clone carries
                // its own `disconnected` flag and its own mutex, so a stream
                // one of them had given up on would still be written to by the
                // other — which is the splice `frame` refuses to make, put
                // back by the error path that reports it.
                let bound = stream
                    .try_clone()
                    .map_err(|e| format!("clone check-host client socket: {e}"))
                    .and_then(|clone| {
                        FrameWriter::new(clone)
                            .map_err(|e| format!("bound check-host client output: {e}"))
                    });
                match bound {
                    Ok(writer) => {
                        if let Err(e) = run_request(
                            stream, request, &pool, &budget, &runtime, &token_dir, &writer,
                        ) {
                            eprintln!("td-builder: check host request failed: {e}");
                            let line = format!("td-builder: check host: {e}\n");
                            let _ = writer.frame(FRAME_STDERR, line.as_bytes());
                            let _ = writer.frame(FRAME_EXIT, &1u32.to_be_bytes());
                        }
                    }
                    Err(e) => {
                        eprintln!("td-builder: check host could not bind a client's output: {e}")
                    }
                }
            }
            Err(e) => eprintln!("td-builder: check host dropped malformed request: {e}"),
        }
    }
}

fn run_request(
    mut stream: UnixStream,
    request: RunRequest,
    pool: &TokenPool,
    budget: &HostBudget,
    runtime: &Path,
    token_dir: &Path,
    writer: &Arc<FrameWriter>,
) -> Result<(), String> {
    stream
        .set_read_timeout(Some(Duration::from_millis(20)))
        .map_err(|e| format!("set check-host client poll timeout: {e}"))?;
    let disconnected = AtomicBool::new(false);
    let admission_stream = Mutex::new(
        stream
            .try_clone()
            .map_err(|e| format!("clone check-host admission socket: {e}"))?,
    );
    let permit = check_memory::base_permit(pool, budget, &|| {
        if disconnected.load(Ordering::Relaxed) {
            return true;
        }
        let gone = admission_stream
            .lock()
            .map(|mut socket| peer_gone(&mut socket))
            .unwrap_or(true);
        if gone {
            disconnected.store(true, Ordering::Relaxed);
        }
        gone
    })?;
    stream
        .set_read_timeout(Some(Duration::from_millis(20)))
        .map_err(|e| format!("reset check-host client poll timeout: {e}"))?;
    // Each request supplies a procfs descriptor to its pinned executable.
    // Use it for the supervisor, then have that exact process re-exec itself
    // after installing the PID namespace. A client that disconnects closes
    // the descriptor and cancellation wins before its token can be reused.
    let mut command = Command::new(supervisor_executable(&request));
    command
        .arg("check-pidns-run")
        .arg("/proc/self/exe")
        .args(&request.args)
        .current_dir(PathBuf::from(&request.cwd))
        .env_clear();
    for (key, value) in request.env {
        if !reserved_policy_key(&key) {
            command.env(key, value);
        }
    }
    let base_jobs = check_memory::jobs_for_budget(
        permit.bytes(),
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1),
    );
    command
        .env(check_memory::HOST_CHILD_ENV, "1")
        .env(check_memory::HOST_RUNTIME_ENV, runtime)
        .env(check_memory::TOKEN_DIR_ENV, token_dir)
        .env(
            check_memory::TOKEN_COUNT_ENV,
            budget.token_count.to_string(),
        )
        .env(
            check_memory::BASE_TOKENS_ENV,
            budget.base_tokens.to_string(),
        )
        .env(
            check_memory::GATE_TOKENS_ENV,
            budget.gate_tokens.to_string(),
        )
        .env(
            check_memory::RESERVE_BYTES_ENV,
            budget.reserve_bytes.to_string(),
        )
        .env(check_memory::JOB_BUDGET_ENV, permit.bytes().to_string())
        // A daemon created beneath this request's private PID namespace would
        // be killed with the request and could corrupt the cross-worktree
        // feed.pid/feed.addr state. Hosted checks use the direct streaming
        // warm path unless the caller supplied an already-running feed base.
        .env(FEED_NO_DAEMON_ENV, "1")
        .env("CARGO_BUILD_JOBS", base_jobs.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    // The worker that spawns this child also waits for it below. The child is a
    // tiny PID-namespace supervisor: if the host itself is killed, PDEATHSIG
    // kills that supervisor, which kills namespace PID 1, and the kernel tears
    // down every provisioning, warm, gate, and build descendant before the
    // token flocks can be reused.
    crate::sandbox::die_with_parent(&mut command);
    let mut child = crate::spawn::past_a_busy_program(|| command.spawn()).map_err(|e| {
        format!(
            "spawn hosted command {}: {e}",
            Path::new(&request.exe).display()
        )
    })?;
    let pid = child.id();
    let Some(stdout) = child.stdout.take() else {
        terminate_tree(pid);
        let _ = child.wait();
        return Err("hosted command has no stdout pipe".to_string());
    };
    let Some(stderr) = child.stderr.take() else {
        terminate_tree(pid);
        let _ = child.wait();
        return Err("hosted command has no stderr pipe".to_string());
    };
    let out_writer = writer.clone();
    let out_thread = match spawn_output_pump("stdout", stdout, FRAME_STDOUT, out_writer) {
        Ok(thread) => thread,
        Err(e) => {
            terminate_tree(pid);
            let _ = child.wait();
            return Err(e);
        }
    };
    let err_writer = writer.clone();
    let err_thread = match spawn_output_pump("stderr", stderr, FRAME_STDERR, err_writer) {
        Ok(thread) => thread,
        Err(e) => {
            terminate_tree(pid);
            let _ = child.wait();
            // Same reason as the cancel path: a pump parked on a stalled
            // client would otherwise hold this worker for the whole budget.
            writer.abandon();
            let _ = out_thread.join();
            return Err(e);
        }
    };

    let emergency = budget.reserve_bytes / 2;
    let mut cancelled = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(e) => {
                // Joined before returning: the pumps are still draining this
                // child, and the caller is about to write its own frames down
                // the same stream. Leaving them running splices one into the
                // other and manufactures a framing error.
                terminate_tree(pid);
                writer.abandon();
                let _ = out_thread.join();
                let _ = err_thread.join();
                return Err(format!("wait for hosted command {pid}: {e}"));
            }
        }
        let pressure = check_memory::emergency_memory_available(emergency)
            .map(|available| !available)
            .unwrap_or(true);
        let disconnected = writer.disconnected.load(Ordering::Relaxed);
        let peer_left = peer_gone(&mut stream);
        if disconnected || peer_left || pressure {
            cancelled = true;
            // Cancelling drops the client with no exit frame, which is
            // indistinguishable at the client from the host having died, so
            // the host is the only place that can say which it was. Every
            // cause that held is named: they overlap, and picking one would
            // mean picking a precedence the condition above does not have.
            let mut causes = Vec::new();
            if disconnected {
                causes.push("response writes stopped being accepted");
            }
            if peer_left {
                causes.push("client went away");
            }
            if pressure {
                causes.push("emergency memory reserve crossed");
            }
            eprintln!(
                "td-builder: check host cancelling request {pid} ({}) at {}s",
                causes.join(", "),
                epoch_seconds()
            );
            // Kill FIRST, explain second. The reserve being crossed is the
            // one moment nothing may wait, and this diagnostic goes to the
            // client whose stalled reading may be why the write would wait at
            // all — so writing it before the kill can keep the memory-heavy
            // child alive for the whole stall budget, precisely when that is
            // most expensive. The explanation is worth a few seconds, not
            // minutes, so it also carries its own short deadline.
            terminate_tree(pid);
            if pressure {
                let _ = writer.frame_if_free(
                    FRAME_STDERR,
                    b"td-builder: check host: emergency memory reserve crossed; cancelling hosted check\n",
                    OUTPUT_TIMEOUT,
                );
            }
            // Release any pump already parked on this client. Without this the
            // kill frees the child's memory but the worker and its permit stay
            // held for the rest of the budget, so a few stalled clients shrink
            // the pool for minutes.
            writer.abandon();
            match child.wait() {
                Ok(status) => break status,
                Err(e) => {
                    // Same reason as above: never return past a live pump.
                    writer.abandon();
                    let _ = out_thread.join();
                    let _ = err_thread.join();
                    return Err(format!("wait after cancelling hosted command {pid}: {e}"));
                }
            }
        }
        std::thread::sleep(POLL_INTERVAL);
    };
    let _ = out_thread.join();
    let _ = err_thread.join();
    let mut code = status
        .code()
        .or_else(|| status.signal().map(|signal| 128 + signal))
        .unwrap_or(1)
        .clamp(0, 255) as u32;
    if cancelled && code == 0 {
        code = 1;
    }
    if let Err(e) = writer.frame(FRAME_EXIT, &code.to_be_bytes()) {
        // The client is about to read EOF and report a host that vanished.
        // This is the only record of what actually happened to its exit code.
        eprintln!("td-builder: check host could not send exit {code} for {pid}: {e}");
    }
    Ok(())
}

fn spawn_output_pump<R: Read + Send + 'static>(
    name: &str,
    mut reader: R,
    frame: u8,
    writer: Arc<FrameWriter>,
) -> Result<std::thread::JoinHandle<()>, String> {
    std::thread::Builder::new()
        .name(format!("td-check-host-{name}"))
        .stack_size(WORKER_STACK_BYTES)
        .spawn(move || {
            let mut buffer = vec![0u8; IO_CHUNK_BYTES];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(n) => {
                        if writer.frame(frame, buffer.get(..n).unwrap_or(&[])).is_err() {
                            break;
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
        })
        .map_err(|e| format!("spawn hosted {name} output pump: {e}"))
}

fn peer_gone(stream: &mut UnixStream) -> bool {
    let mut byte = [0u8; 1];
    match stream.read(&mut byte) {
        Ok(0) => true,
        Ok(_) => false,
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted
            ) =>
        {
            false
        }
        Err(_) => true,
    }
}

fn terminate_tree(root: u32) {
    // Snapshot before killing the root. If TERM lets the root exit first, an
    // escaped descendant is reparented and vanishes from a second tree walk.
    // Cancellation is a containment path, so use uncatchable SIGKILL for the
    // captured descendants and the ordinary process group in one pass.
    let mut descendants = descendant_pids(root);
    descendants.sort_unstable_by(|a, b| b.cmp(a));
    let _ = crate::sys::kill_process_group(root, crate::sys::SIGKILL);
    let _ = crate::sys::kill_pid(i64::from(root), crate::sys::SIGKILL);
    for pid in descendants {
        let _ = crate::sys::kill_pid(i64::from(pid), crate::sys::SIGKILL);
    }
}

fn descendant_pids(root: u32) -> Vec<u32> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut pairs = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name
            .to_str()
            .filter(|value| value.bytes().all(|byte| byte.is_ascii_digit()))
            .and_then(|value| value.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        let Some(after) = stat.rsplit_once(") ").map(|(_, after)| after) else {
            continue;
        };
        let parent = after
            .split_whitespace()
            .nth(1)
            .and_then(|value| value.parse::<u32>().ok());
        if let Some(parent) = parent {
            pairs.push((pid, parent));
        }
    }
    let mut out = Vec::new();
    let mut frontier = vec![root];
    while let Some(parent) = frontier.pop() {
        for (pid, ppid) in &pairs {
            if *ppid == parent && !out.contains(pid) {
                out.push(*pid);
                frontier.push(*pid);
            }
        }
    }
    out
}

fn reserved_policy_key(key: &OsStr) -> bool {
    let bytes = key.as_bytes();
    bytes.starts_with(b"TD_CHECK_HOST_")
        || bytes == check_memory::JOB_BUDGET_ENV.as_bytes()
        || bytes == FEED_NO_DAEMON_ENV.as_bytes()
        || matches!(bytes, b"CARGO_BUILD_JOBS" | b"NIX_BUILD_CORES")
}

fn write_control_request(stream: &mut UnixStream, kind: u8) -> Result<(), String> {
    stream.write_all(MAGIC).map_err(|e| e.to_string())?;
    stream.write_all(&[kind]).map_err(|e| e.to_string())
}

fn write_run_request(stream: &mut UnixStream, request: &RunRequest) -> Result<(), String> {
    write_control_request(stream, REQUEST_RUN)?;
    write_field(stream, request.exe.as_bytes())?;
    write_field(stream, request.cwd.as_bytes())?;
    write_count(stream, request.args.len(), MAX_ARGS, "argument")?;
    for arg in &request.args {
        write_field(stream, arg.as_bytes())?;
    }
    write_count(stream, request.env.len(), MAX_ENVS, "environment")?;
    for (key, value) in &request.env {
        write_field(stream, key.as_bytes())?;
        write_field(stream, value.as_bytes())?;
    }
    Ok(())
}

fn write_count(
    stream: &mut UnixStream,
    count: usize,
    max: usize,
    what: &str,
) -> Result<(), String> {
    if count > max {
        return Err(format!(
            "check-host request has too many {what} fields ({count} > {max})"
        ));
    }
    let count = u32::try_from(count).map_err(|_| format!("{what} count does not fit u32"))?;
    stream
        .write_all(&count.to_be_bytes())
        .map_err(|e| e.to_string())
}

fn write_field(stream: &mut UnixStream, bytes: &[u8]) -> Result<(), String> {
    if bytes.len() > MAX_FIELD_BYTES {
        return Err(format!(
            "check-host request field is {} bytes (limit {MAX_FIELD_BYTES})",
            bytes.len()
        ));
    }
    let len =
        u32::try_from(bytes.len()).map_err(|_| "field length does not fit u32".to_string())?;
    stream
        .write_all(&len.to_be_bytes())
        .map_err(|e| e.to_string())?;
    stream.write_all(bytes).map_err(|e| e.to_string())
}

fn read_request(stream: &mut UnixStream, deadline: Instant) -> Result<Request, String> {
    let mut budget = MAX_REQUEST_BYTES;
    let mut magic = [0u8; MAGIC.len()];
    read_exact(stream, &mut magic, &mut budget, deadline)?;
    if &magic != MAGIC {
        return Err("bad check-host protocol magic".to_string());
    }
    let mut kind = [0u8; 1];
    read_exact(stream, &mut kind, &mut budget, deadline)?;
    match kind[0] {
        REQUEST_PING => Ok(Request::Ping),
        REQUEST_STOP => Ok(Request::Stop),
        REQUEST_RUN => {
            let exe = OsString::from_vec(read_field(stream, &mut budget, deadline)?);
            let cwd = OsString::from_vec(read_field(stream, &mut budget, deadline)?);
            let argc = read_count(stream, &mut budget, MAX_ARGS, "argument", deadline)?;
            let mut args = Vec::with_capacity(argc);
            for _ in 0..argc {
                args.push(OsString::from_vec(read_field(
                    stream,
                    &mut budget,
                    deadline,
                )?));
            }
            let envc = read_count(stream, &mut budget, MAX_ENVS, "environment", deadline)?;
            let mut env = Vec::with_capacity(envc);
            for _ in 0..envc {
                let key = OsString::from_vec(read_field(stream, &mut budget, deadline)?);
                let value = OsString::from_vec(read_field(stream, &mut budget, deadline)?);
                if key.as_bytes().is_empty() || key.as_bytes().contains(&b'=') {
                    return Err("check-host request has an invalid environment key".to_string());
                }
                env.push((key, value));
            }
            Ok(Request::Run(RunRequest {
                exe,
                cwd,
                args,
                env,
            }))
        }
        other => Err(format!("unknown check-host request type {other}")),
    }
}

fn read_count(
    stream: &mut UnixStream,
    budget: &mut usize,
    max: usize,
    what: &str,
    deadline: Instant,
) -> Result<usize, String> {
    let mut raw = [0u8; 4];
    read_exact(stream, &mut raw, budget, deadline)?;
    let count = usize::try_from(u32::from_be_bytes(raw)).unwrap_or(usize::MAX);
    if count > max {
        Err(format!("check-host request has too many {what} fields"))
    } else {
        Ok(count)
    }
}

fn read_field(
    stream: &mut UnixStream,
    budget: &mut usize,
    deadline: Instant,
) -> Result<Vec<u8>, String> {
    let mut raw = [0u8; 4];
    read_exact(stream, &mut raw, budget, deadline)?;
    let len = usize::try_from(u32::from_be_bytes(raw)).unwrap_or(usize::MAX);
    if len > MAX_FIELD_BYTES || len > *budget {
        return Err(format!(
            "check-host request field exceeds {MAX_FIELD_BYTES} bytes"
        ));
    }
    let mut bytes = vec![0u8; len];
    read_exact(stream, &mut bytes, budget, deadline)?;
    Ok(bytes)
}

fn read_exact(
    stream: &mut UnixStream,
    bytes: &mut [u8],
    budget: &mut usize,
    deadline: Instant,
) -> Result<(), String> {
    if bytes.len() > *budget {
        return Err("check-host request exceeds its total byte limit".to_string());
    }
    let mut filled = 0usize;
    while filled < bytes.len() {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| "check-host request exceeded its absolute deadline".to_string())?;
        stream
            .set_read_timeout(Some(remaining))
            .map_err(|e| format!("set check-host request deadline: {e}"))?;
        let tail = bytes
            .get_mut(filled..)
            .ok_or_else(|| "check-host request read offset escaped its field".to_string())?;
        match stream.read(tail) {
            Ok(0) => return Err("check-host request ended early".to_string()),
            Ok(count) => filled = filled.saturating_add(count),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(format!("read check-host request: {e}")),
        }
    }
    *budget = budget.saturating_sub(bytes.len());
    Ok(())
}

/// Say what a response failure was, and where to read about it.
fn describe_response_error(error: ResponseError, runtime: &Path) -> String {
    match error {
        ResponseError::Local(message) => message,
        // Never "said which in no host log under ...": a sentence asserting
        // the host explained itself, in a file it states does not exist.
        ResponseError::Host(message) => match host_logs(runtime) {
            Some(logs) => format!("{message}; the host's side of this is in {logs}"),
            None => format!(
                "{message}; nothing survived under {} to say more",
                runtime.display()
            ),
        },
        ResponseError::Closed => match host_logs(runtime) {
            Some(logs) => format!(
                "{HOST_CLOSED}; the host exited, was killed, or dropped this \
                 request, and said which in {logs}"
            ),
            None => format!(
                "{HOST_CLOSED}; the host exited, was killed, or dropped this \
                 request, and nothing survived under {} to say which",
                runtime.display()
            ),
        },
    }
}

/// Which side a response failure belongs to, and how much it implies.
///
/// Only the first two are a reason to read the host's log, and only `Closed`
/// says the host may be gone. Annotating a broken local stdout with the
/// host's log would send the reader to a file with nothing to say about it,
/// and telling them a live host died because it sent an oversized frame would
/// send them to one with nothing to say about that either.
#[derive(Debug)]
enum ResponseError {
    Closed,
    Host(String),
    Local(String),
}

fn read_run_frames(stream: &mut UnixStream) -> Result<u8, ResponseError> {
    read_run_frames_into(stream, &mut std::io::stdout(), &mut std::io::stderr())
}

/// The response loop, with its two sinks named so a test can fail one.
fn read_run_frames_into(
    stream: &mut UnixStream,
    out: &mut impl Write,
    err: &mut impl Write,
) -> Result<u8, ResponseError> {
    loop {
        let (kind, bytes) = read_frame(stream).map_err(|e| match e {
            FrameError::Closed => ResponseError::Closed,
            // This client sets no read deadline on the response, so the host
            // going silent without closing is a host-side condition and the
            // log is the right place to send the reader.
            FrameError::TimedOut => ResponseError::Host(
                "the host went silent without closing the connection".to_string(),
            ),
            FrameError::Other(message) => ResponseError::Host(message),
        })?;
        match kind {
            FRAME_STDOUT => out
                .write_all(&bytes)
                .map_err(|e| ResponseError::Local(format!("write hosted stdout: {e}")))?,
            FRAME_STDERR => err
                .write_all(&bytes)
                .map_err(|e| ResponseError::Local(format!("write hosted stderr: {e}")))?,
            FRAME_EXIT if bytes.len() == 4 => {
                let raw: [u8; 4] = bytes.as_slice().try_into().map_err(|_| {
                    ResponseError::Host("invalid check-host exit frame".to_string())
                })?;
                return Ok(u32::from_be_bytes(raw).min(255) as u8);
            }
            _ => {
                return Err(ResponseError::Host(format!(
                    "invalid check-host response frame {kind}"
                )))
            }
        }
    }
}

/// A frame that could not be read, and what kind of silence it was.
enum FrameError {
    Closed,
    /// The read deadline expired: nothing arrived, and the connection is still
    /// open. Distinct from `Other` because the bare `EAGAIN` this produces is
    /// the same shape as the message this whole file exists to abolish.
    TimedOut,
    Other(String),
}

impl FrameError {
    fn message(self) -> String {
        match self {
            FrameError::Closed => HOST_CLOSED.to_string(),
            FrameError::TimedOut => "the check host did not answer in time".to_string(),
            FrameError::Other(message) => message,
        }
    }
}

/// Whether a failure to read a frame TYPE means the connection simply ended.
///
/// Kept separate because it cannot be provoked from a socket pair: RST arrives
/// only from a real peer killed with output still queued, which is exactly the
/// case a unit test cannot stage but a `kill -9` produces routinely.
fn ended_at_boundary(kind: io::ErrorKind) -> bool {
    matches!(
        kind,
        io::ErrorKind::UnexpectedEof | io::ErrorKind::ConnectionReset
    )
}

fn read_frame(stream: &mut UnixStream) -> Result<(u8, Vec<u8>), FrameError> {
    let mut kind = [0u8; 1];
    if let Err(e) = stream.read_exact(&mut kind) {
        // At a frame BOUNDARY these two mean one thing: no more frames are
        // coming and no exit frame arrived. A clean shutdown gives EOF; a host
        // SIGKILLed with output still queued gives RST instead, which is the
        // same event and deserves the same explanation rather than a bare
        // errno. Mid-frame (length, body) they stay `Other`: that is
        // truncation, which is a different diagnosis.
        if ended_at_boundary(e.kind()) {
            return Err(FrameError::Closed);
        }
        if matches!(
            e.kind(),
            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
        ) {
            return Err(FrameError::TimedOut);
        }
        return Err(FrameError::Other(format!(
            "read check-host frame type: {e}"
        )));
    }
    let mut raw = [0u8; 4];
    stream
        .read_exact(&mut raw)
        .map_err(|e| FrameError::Other(format!("read check-host frame length: {e}")))?;
    let len = usize::try_from(u32::from_be_bytes(raw)).unwrap_or(usize::MAX);
    if len > IO_CHUNK_BYTES {
        return Err(FrameError::Other(format!(
            "check-host frame exceeds {IO_CHUNK_BYTES} bytes"
        )));
    }
    let mut bytes = vec![0u8; len];
    stream
        .read_exact(&mut bytes)
        .map_err(|e| FrameError::Other(format!("read check-host frame body: {e}")))?;
    Ok((kind[0], bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A private directory for one test, named so two cannot collide.
    fn scratch_runtime(name: &str) -> PathBuf {
        let runtime = std::env::temp_dir().join(format!(
            "td-check-host-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&runtime);
        std::fs::create_dir_all(&runtime).unwrap();
        runtime
    }

    /// The replacement must not erase the record of what it replaced.
    ///
    /// A dead host is replaced by the next client within milliseconds, so a
    /// truncating start would make recovery destroy the only account of the
    /// death. One generation back is exactly the restart that follows one.
    #[test]
    fn a_replacement_host_keeps_the_dead_hosts_log() {
        let runtime = scratch_runtime("rotate");
        let (log_path, previous) = log_paths(&runtime);

        let mut first = open_fresh_log(&runtime).unwrap();
        first.write_all(b"first host died here\n").unwrap();
        drop(first);

        let second = open_fresh_log(&runtime).unwrap();
        drop(second);
        assert_eq!(
            std::fs::read_to_string(&previous).unwrap(),
            "first host died here\n",
            "the dead host's log must survive the restart that replaces it"
        );
        assert_eq!(std::fs::read_to_string(&log_path).unwrap(), "");

        // Only one generation is kept, so the record cannot grow without bound.
        let mut second = OpenOptions::new().append(true).open(&log_path).unwrap();
        second.write_all(b"second\n").unwrap();
        drop(second);
        drop(open_fresh_log(&runtime).unwrap());
        assert_eq!(std::fs::read_to_string(&previous).unwrap(), "second\n");

        let _ = std::fs::remove_dir_all(&runtime);
    }

    /// A failure is annotated with the host's log only when it is the host's.
    ///
    /// The reader's time is the thing being spent here: a broken local stdout
    /// sent to the host's log is a wasted trip to a file with nothing to say,
    /// and a `.prev` named on a first start is a trip to a file that does not
    /// exist at all.
    #[test]
    fn only_a_host_side_failure_is_worth_the_host_log() {
        let runtime = scratch_runtime("attribute");
        let (log_path, previous) = log_paths(&runtime);

        let local = describe_response_error(
            ResponseError::Local("write hosted stdout: Broken pipe".to_string()),
            &runtime,
        );
        assert_eq!(local, "write hosted stdout: Broken pipe");

        // Only a closed connection may claim the host might be gone. A frame
        // this client refused says nothing about the host's health.
        let refused = describe_response_error(
            ResponseError::Host("check-host frame exceeds 65536 bytes".to_string()),
            &runtime,
        );
        assert!(
            !refused.contains("exited, was killed"),
            "a refused frame is not a dead host: {refused}"
        );

        // No log yet: say so rather than name a file nobody can read.
        let closed = describe_response_error(ResponseError::Closed, &runtime);
        assert!(closed.starts_with(HOST_CLOSED), "{closed}");
        assert!(closed.contains("exited, was killed"), "{closed}");
        assert!(closed.contains("nothing survived under"), "{closed}");

        // An empty log is not worth a trip: this rotation leaves them behind a
        // host that was killed before it could speak.
        std::fs::write(&log_path, b"").unwrap();
        assert_eq!(host_logs(&runtime), None);
        let closed = describe_response_error(ResponseError::Closed, &runtime);
        assert!(closed.contains("nothing survived under"), "{closed}");

        std::fs::write(&log_path, b"banner\n").unwrap();
        assert_eq!(
            host_logs(&runtime),
            Some(format!(
                "{} (or {}, if a replacement host has since rotated it)",
                log_path.display(),
                previous.display()
            ))
        );
        let closed = describe_response_error(ResponseError::Closed, &runtime);
        assert!(closed.contains(&log_path.display().to_string()), "{closed}");
        // The empty generation is named only as where this file is about to
        // go — the replacement this death provokes rotates it — and never as
        // an account to go and read now.
        assert!(
            closed.contains("if a replacement host has since rotated it"),
            "the current log must be given as movable: {closed}"
        );

        std::fs::write(&previous, b"older banner\n").unwrap();
        assert_eq!(
            host_logs(&runtime),
            Some(format!(
                "{} and {} (a replacement host rotating the first would \
                 displace the second)",
                log_path.display(),
                previous.display()
            ))
        );
        let closed = describe_response_error(ResponseError::Closed, &runtime);
        assert!(closed.contains(&previous.display().to_string()), "{closed}");
        assert!(
            closed.contains("would displace"),
            "two live generations are just as movable as one: {closed}"
        );

        // The shape a killed host actually leaves once its replacement has
        // started: an empty current generation, the account in the previous
        // one. Naming the empty file here would send the reader nowhere, and
        // hedging about a rotation that already happened would be noise.
        std::fs::write(&log_path, b"").unwrap();
        // Compared exactly, not by `contains`: `...log.prev` has `...log` as a
        // prefix, so a substring test here can never fail and would pin
        // nothing at all.
        assert_eq!(
            host_logs(&runtime),
            Some(previous.display().to_string()),
            "an empty current generation must not be named at all"
        );
        let closed = describe_response_error(ResponseError::Closed, &runtime);
        assert!(closed.contains(&previous.display().to_string()), "{closed}");
        assert!(
            !closed.contains("if a replacement host has since rotated it"),
            "the rotation already happened; do not hedge about it: {closed}"
        );

        let _ = std::fs::remove_dir_all(&runtime);
    }

    /// A sink that refuses everything, standing in for a broken local stdout.
    struct Refuses;

    impl Write for Refuses {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::from(io::ErrorKind::BrokenPipe))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// The response loop must decide whose failure it was, not just report it.
    ///
    /// `describe_response_error` can only be as right as the classification it
    /// is handed, so the classification is pinned where it is made: a local
    /// write that fails is the client's problem, and must not be dressed up as
    /// a host that stopped.
    #[test]
    fn a_broken_local_sink_is_not_the_hosts_failure() {
        let (mut client, mut host) = UnixStream::pair().unwrap();
        let payload = b"hosted output";
        host.write_all(&[FRAME_STDOUT]).unwrap();
        host.write_all(&(payload.len() as u32).to_be_bytes())
            .unwrap();
        host.write_all(payload).unwrap();

        let failure = read_run_frames_into(&mut client, &mut Refuses, &mut Vec::new()).unwrap_err();
        assert!(
            matches!(&failure, ResponseError::Local(message)
                if message.starts_with("write hosted stdout:")),
            "a local sink that refuses is a local failure"
        );

        // And the same frame with a sink that accepts it runs on to the exit.
        host.write_all(&[FRAME_EXIT]).unwrap();
        host.write_all(&4u32.to_be_bytes()).unwrap();
        host.write_all(&7u32.to_be_bytes()).unwrap();
        drop(host);
        let mut client = client;
        let code = read_run_frames_into(&mut client, &mut Vec::new(), &mut Vec::new()).unwrap();
        assert_eq!(code, 7);
    }

    /// A stream that failed mid-frame is finished, for every writer on it.
    ///
    /// `write_all` can fail with bytes already committed, and there is no way
    /// to find the next frame boundary after that. The stdout and stderr pumps
    /// share one `FrameWriter`, so nothing but this refusal stops the second
    /// from writing its header into the first one's body and handing the
    /// client a framing error that describes nothing that happened.
    #[test]
    fn a_writer_that_failed_does_not_let_the_next_one_resume() {
        let (client, host) = UnixStream::pair().unwrap();
        let writer = FrameWriter::new(host).unwrap();
        assert!(writer.frame(FRAME_STDOUT, b"delivered").is_ok());

        // The client is gone: the next write cannot land.
        drop(client);
        assert!(
            writer.frame(FRAME_STDOUT, b"lost").is_err(),
            "a write to a departed client must fail"
        );
        assert!(writer.disconnected.load(Ordering::Relaxed));

        // The other pump now tries to use the same stream.
        let after = writer.frame(FRAME_STDERR, b"would be spliced").unwrap_err();
        assert_eq!(after.kind(), io::ErrorKind::BrokenPipe);
        assert!(
            after.to_string().contains("already gave up"),
            "the refusal must name itself, not look like a fresh write failure: {after}"
        );
    }

    /// A killed host and a closed one end the same way, and read the same.
    ///
    /// A host SIGKILLed with output still queued resets the connection instead
    /// of closing it. Reporting that as a bare `Connection reset by peer` says
    /// nothing about which of the three things happened, which is the whole
    /// complaint this commit answers.
    #[test]
    fn a_reset_at_a_frame_boundary_is_a_connection_that_ended() {
        assert!(ended_at_boundary(io::ErrorKind::UnexpectedEof));
        assert!(ended_at_boundary(io::ErrorKind::ConnectionReset));
        // Not everything is: these are real read failures on a live host, and
        // calling them "ended" would assert a death that did not happen.
        assert!(!ended_at_boundary(io::ErrorKind::TimedOut));
        assert!(!ended_at_boundary(io::ErrorKind::Interrupted));
        assert!(!ended_at_boundary(io::ErrorKind::InvalidData));
    }

    /// A client that is merely SLOW must not be buried.
    ///
    /// This is the flake, at the only level it can be staged: the response
    /// socket's 5s `SO_SNDTIMEO` used to be read as "the client is gone", so a
    /// hosted check was cancelled and SIGKILLed because a log reader paused.
    /// The client then saw an ending at a frame boundary and reported a host
    /// that had died, while the host was healthy throughout. Nothing here is
    /// wrong except the reader's timing, so nothing may be killed.
    #[test]
    fn a_slow_client_is_waited_for_rather_than_buried() {
        let (mut client, host) = UnixStream::pair().unwrap();
        let mut clone = host.try_clone().unwrap();
        let writer = FrameWriter::new(host).unwrap();
        // Wake often, so the timeout path is taken many times over the pause
        // below without the test having to wait five seconds for each.
        clone
            .set_write_timeout(Some(Duration::from_millis(20)))
            .unwrap();

        // Position-dependent, so a write that resumes from the wrong offset
        // or repeats a slice cannot go unnoticed the way a run of one byte
        // would. Far larger than any plausible socket buffer, so the write
        // cannot simply be swallowed whole.
        const PAYLOAD: usize = 8 * 1024 * 1024;
        let payload: Vec<u8> = (0..PAYLOAD).map(|i| (i % 251) as u8).collect();
        let mut expected = Vec::with_capacity(PAYLOAD + 5);
        expected.push(FRAME_STDOUT);
        expected.extend_from_slice(&(PAYLOAD as u32).to_be_bytes());
        expected.extend_from_slice(&payload);

        // The reader stalls, then comes back — a slow consumer, not a dead one.
        let stall = Duration::from_millis(300);
        // Bounded, so a mutation that writes too few bytes fails this test
        // instead of parking the suite on a read that will never complete.
        client
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let drained = std::thread::spawn(move || {
            std::thread::sleep(stall);
            let mut sink = Vec::new();
            let mut buf = vec![0u8; 64 * 1024];
            while sink.len() < PAYLOAD + 5 {
                match client.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => sink.extend_from_slice(buf.get(..n).unwrap_or(&[])),
                }
            }
            sink
        });

        let started = Instant::now();
        let sent = writer.frame(FRAME_STDOUT, &payload);
        let waited = started.elapsed();
        assert!(
            sent.is_ok(),
            "a client that paused and came back must be waited for: {sent:?}"
        );
        assert!(
            !writer.disconnected.load(Ordering::Relaxed),
            "a slow client must not be marked gone; that is what kills the check"
        );
        // Without this the test could pass having never blocked at all — if
        // the buffer swallowed the frame, restoring "a timeout is fatal" would
        // stay green and this would pin nothing.
        assert!(
            waited >= stall,
            "the write must actually have waited on the reader, waited {waited:?}"
        );

        // Exact bytes, header included: this is what pins the offset
        // arithmetic and the five-byte header the resumed writes must not
        // disturb. Read below the protocol layer deliberately — the frame is
        // larger than `IO_CHUNK_BYTES`, so a real client would refuse it, but
        // the contract under test is the writer's, not the reader's.
        let read_back = drained.join().unwrap_or_default();
        assert_eq!(
            read_back.len(),
            expected.len(),
            "the frame must arrive whole once the reader returns"
        );
        assert!(
            read_back == expected,
            "the frame must arrive byte-for-byte across the resumed writes"
        );
    }

    /// Cancelling reaches a write that is ALREADY parked on a stalled client.
    ///
    /// Patience is for a client that might still be served. Once the check is
    /// dead there is nothing left to deliver, and a pump still waiting out its
    /// budget holds a worker and a memory permit that the host needs back —
    /// so a handful of stalled clients would shrink the pool for minutes.
    #[test]
    fn cancelling_releases_a_write_already_parked_on_a_stalled_client() {
        let (client, host) = UnixStream::pair().unwrap();
        let mut clone = host.try_clone().unwrap();
        // The production budget: nothing here may depend on it expiring.
        let writer = FrameWriter::with_budget(host, STALL_BUDGET).unwrap();
        clone
            .set_write_timeout(Some(Duration::from_millis(20)))
            .unwrap();

        let writing = Arc::clone(&writer);
        let parked = std::thread::spawn(move || {
            let payload = vec![b'x'; 8 * 1024 * 1024];
            writing.frame(FRAME_STDOUT, &payload)
        });

        // Let it fill the buffer and settle into waiting. `client` is never
        // read and never dropped: the peer is alive, just not listening.
        std::thread::sleep(Duration::from_millis(200));
        let started = Instant::now();
        writer.abandon();
        let outcome = parked.join();
        let waited = started.elapsed();

        assert!(outcome.is_ok(), "the writing thread must not have panicked");
        let result = outcome.unwrap_or(Ok(()));
        assert!(
            result.is_err(),
            "a cancelled write must stop rather than deliver"
        );
        assert_eq!(
            result.map_err(|e| e.kind()).unwrap_err(),
            io::ErrorKind::Interrupted,
            "cancellation is not the client's failure"
        );
        assert!(
            waited < Duration::from_secs(5),
            "cancelling must not wait out the budget, waited {waited:?}"
        );

        drop(client);
    }

    /// A failed frame shuts the write half for EVERY handle on the socket.
    ///
    /// `frame`'s refusal only covers writers sharing one `FrameWriter`, and a
    /// clone of the socket carries no flag at all — `run_request` hands the
    /// pumps one handle while its caller holds another. The `shutdown` is the
    /// part that reaches those, so it is pinned rather than assumed.
    ///
    /// Staged with a live peer whose budget has run out, which is the case
    /// that matters: a dead peer would fail the clone's write anyway and prove
    /// nothing about whether the shutdown happened.
    #[test]
    fn a_failed_frame_shuts_the_write_half_for_every_handle() {
        let (mut client, host) = UnixStream::pair().unwrap();
        let mut clone = host.try_clone().unwrap();
        // A budget this short makes "stuck" reachable in a test. Production
        // waits `STALL_BUDGET`; the verdict is the same, only later.
        let writer = FrameWriter::with_budget(host, Duration::from_millis(100)).unwrap();
        // SO_SNDTIMEO is a socket option, so this reaches the writer's handle
        // too: it is how often the write wakes to look, not the verdict.
        clone
            .set_write_timeout(Some(Duration::from_millis(20)))
            .unwrap();

        // Fill the buffer against a client that never reads, until the budget
        // is spent and the writer gives up on it.
        let payload = vec![b'x'; 64 * 1024];
        let mut gave_up = false;
        for _ in 0..64 {
            if writer.frame(FRAME_STDOUT, &payload).is_err() {
                gave_up = true;
                break;
            }
        }
        assert!(
            gave_up,
            "the socket buffer must fill for this to test anything"
        );
        assert!(
            writer.disconnected.load(Ordering::Relaxed),
            "a client that never accepts a byte is eventually stuck, not slow"
        );

        // Drain it, so a write would succeed again if nothing had shut the
        // half down. Without the shutdown this clone would append to a stream
        // that is stranded mid-frame.
        client
            .set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        let mut sink = vec![0u8; 256 * 1024];
        while client.read(&mut sink).map(|n| n > 0).unwrap_or(false) {}

        assert!(
            clone.write_all(b"would be spliced").is_err(),
            "a failed frame must shut the write half for every handle on it"
        );
    }

    /// A stop that cannot be answered is classified, not just printed.
    ///
    /// `stop_cli` annotates whatever this returns with the host's log, so the
    /// operator chasing the recurrence this commit predicts is sent somewhere.
    /// A bare string could not be annotated at all.
    #[test]
    fn a_stop_whose_host_vanishes_is_a_closed_connection() {
        let runtime = scratch_runtime("stopclosed");
        let socket = runtime.join(SOCKET_NAME);
        let listener = UnixListener::bind(&socket).unwrap();
        let accepted = std::thread::spawn(move || {
            // Accept, take the whole request, and leave without answering:
            // the host that died between reading and replying. Reading first
            // is what makes this deterministic — dropping the stream the
            // instant it is accepted races the client's write, and a write
            // that loses gets EPIPE and a Local error instead of the Closed
            // this is pinning. A commit about a CI flake must not add one.
            if let Ok((mut accepted, _)) = listener.accept() {
                let mut request = vec![0u8; MAGIC.len() + 1];
                let _ = accepted.read_exact(&mut request);
            }
        });

        let failure = stop(&socket).unwrap_err();
        assert!(
            matches!(&failure, ResponseError::Closed),
            "an unanswered stop is a connection that ended: {failure:?}"
        );

        // And it acquires the log pointer on the way out.
        let (log_path, _) = log_paths(&runtime);
        std::fs::write(&log_path, b"banner\n").unwrap();
        let described = describe_response_error(ResponseError::Closed, &runtime);
        assert!(
            described.contains(&log_path.display().to_string()),
            "{described}"
        );

        let _ = accepted.join();
        let _ = std::fs::remove_dir_all(&runtime);
    }

    /// The other half of that split, pinned where it is DECIDED.
    ///
    /// A frame this client refuses is the host's doing and not a closed
    /// connection, and the difference is made in the `FrameError` mapping, not
    /// in the formatter. Handing `describe_response_error` a `Host` built by
    /// the test would pin nothing: mapping `Other` to `Closed` has to red
    /// here, or the classification is only asserted where it is printed.
    #[test]
    fn a_frame_this_client_refuses_is_not_a_closed_connection() {
        let (mut client, mut host) = UnixStream::pair().unwrap();
        let oversized = u32::try_from(IO_CHUNK_BYTES).unwrap().saturating_add(1);
        host.write_all(&[FRAME_STDOUT]).unwrap();
        host.write_all(&oversized.to_be_bytes()).unwrap();

        let refused =
            read_run_frames_into(&mut client, &mut Vec::new(), &mut Vec::new()).unwrap_err();
        assert!(
            matches!(&refused, ResponseError::Host(message) if message.contains("exceeds")),
            "an oversized frame is the host's doing, not a connection that closed: {refused:?}"
        );

        // A connection that simply ends is the other classification, from the
        // same reader, so neither can stand in for the other.
        let (mut client, host) = UnixStream::pair().unwrap();
        drop(host);
        let ended =
            read_run_frames_into(&mut client, &mut Vec::new(), &mut Vec::new()).unwrap_err();
        assert!(
            matches!(&ended, ResponseError::Closed),
            "an ended connection is Closed: {ended:?}"
        );
    }

    /// Past the cap and unrotatable, the log is discarded — in place.
    ///
    /// This is the only path where the truncate does any work: an empty log is
    /// a no-op and a successful rename leaves nothing behind. The runtime dir
    /// is usually a tmpfs, so a log that can never be rotated has to stop
    /// growing, and it has to stop growing without unlinking the file a
    /// running host is writing to.
    #[test]
    fn a_log_past_the_cap_that_cannot_rotate_is_discarded() {
        let runtime = scratch_runtime("cap");
        let (log_path, previous) = log_paths(&runtime);

        let mut log = open_fresh_log(&runtime).unwrap();
        let oversized = usize::try_from(MAX_LOG_BYTES).unwrap().saturating_add(1);
        log.write_all(&vec![b'x'; oversized]).unwrap();
        drop(log);

        // A `.prev` that cannot be replaced, so rotation keeps failing and the
        // cap is the only thing bounding the file.
        std::fs::create_dir(&previous).unwrap();
        std::fs::write(previous.join("occupied"), b"x").unwrap();

        let before = std::fs::metadata(&log_path).unwrap().ino();
        drop(open_fresh_log(&runtime).unwrap());

        let after = std::fs::metadata(&log_path).unwrap();
        assert_eq!(
            after.len(),
            0,
            "a log past the cap that cannot rotate must stop growing"
        );
        assert_eq!(
            after.ino(),
            before,
            "even discarding clears in place; unlinking loses a live host's stderr"
        );

        let _ = std::fs::remove_dir_all(&runtime);
    }

    /// Clearing the log must not unlink the file a live host is writing to.
    ///
    /// A host's stderr IS this file, held open for its whole life, and the
    /// start flock does not cover every moment a host is writing to it: a
    /// starter that dies mid-startup releases the flock before its host takes
    /// the lifetime lock, and an exiting host writes its last lines after
    /// releasing it. Unlinking in those windows leaves a live writer holding
    /// an inode with no name, so its account survives where no reader can
    /// reach it: the exact disappearance this function exists to prevent,
    /// caused by the function itself.
    #[test]
    fn clearing_the_log_keeps_the_file_a_host_holds_open() {
        let runtime = scratch_runtime("inode");
        let (log_path, _) = log_paths(&runtime);

        let held = open_fresh_log(&runtime).unwrap();
        let before = std::fs::metadata(&log_path).unwrap().ino();

        // Empty, so no generation is spent and the log is cleared in place.
        let replacement = open_fresh_log(&runtime).unwrap();
        let after = std::fs::metadata(&log_path).unwrap().ino();
        assert_eq!(
            before, after,
            "the log was replaced rather than cleared; a host holding the old \
             one now writes where nobody can read"
        );

        // And the descriptor the "running host" holds still reaches the file
        // a reader would open by name.
        let mut held = held;
        held.write_all(b"still reachable\n").unwrap();
        drop(held);
        drop(replacement);
        assert_eq!(
            std::fs::read_to_string(&log_path).unwrap(),
            "still reachable\n"
        );

        let _ = std::fs::remove_dir_all(&runtime);
    }

    /// A rotation that cannot happen must not cost the record either.
    ///
    /// This is the branch where `rename` fails for a real reason rather than
    /// for want of a first generation. Discarding the log there would destroy
    /// exactly what this function exists to keep, so it is appended to.
    #[test]
    fn a_log_that_cannot_rotate_is_appended_to_rather_than_lost() {
        let runtime = scratch_runtime("norotate");
        let (log_path, previous) = log_paths(&runtime);

        let mut first = open_fresh_log(&runtime).unwrap();
        first.write_all(b"the account\n").unwrap();
        drop(first);

        // A `.prev` that cannot be replaced by a rename.
        std::fs::create_dir(&previous).unwrap();
        std::fs::write(previous.join("occupied"), b"x").unwrap();

        let mut second = open_fresh_log(&runtime).unwrap();
        second.write_all(b"the next host\n").unwrap();
        drop(second);

        assert_eq!(
            std::fs::read_to_string(&log_path).unwrap(),
            "the account\nthe next host\n",
            "a rotation that cannot happen must not cost the record"
        );

        let _ = std::fs::remove_dir_all(&runtime);
    }

    /// A host killed before it could speak must not cost a generation.
    ///
    /// Under the load that kills hosts, a replacement can fail to come up too.
    /// If its empty log rotated, the second failure would push the first
    /// host's account of its death out of reach — losing exactly the record
    /// this rotation exists to keep, in exactly the conditions that need it.
    #[test]
    fn an_empty_log_does_not_spend_a_generation() {
        let runtime = scratch_runtime("empty");
        let (log_path, previous) = log_paths(&runtime);

        let mut first = open_fresh_log(&runtime).unwrap();
        first.write_all(b"the death that matters\n").unwrap();
        drop(first);

        // A replacement starts, rotating the account above out to `.prev`, and
        // is killed before it writes its banner.
        drop(open_fresh_log(&runtime).unwrap());
        assert_eq!(
            std::fs::read_to_string(&previous).unwrap(),
            "the death that matters\n"
        );
        assert_eq!(std::fs::read_to_string(&log_path).unwrap(), "");

        // Its successor must not rotate that silence over the account.
        drop(open_fresh_log(&runtime).unwrap());
        assert_eq!(
            std::fs::read_to_string(&previous).unwrap(),
            "the death that matters\n",
            "an empty log must not displace a host's account of its death"
        );

        let _ = std::fs::remove_dir_all(&runtime);
    }

    /// A host that stops mid-request is named as such, not as a short read.
    #[test]
    fn a_response_that_stops_early_names_the_host_not_the_buffer() {
        let (client, host) = UnixStream::pair().unwrap();
        drop(host);
        let mut client = client;
        let closed = read_frame(&mut client).unwrap_err();
        assert!(
            matches!(closed, FrameError::Closed),
            "a connection that ended at a frame boundary is a gone host"
        );
        assert_eq!(closed.message(), HOST_CLOSED);

        // A frame that starts and then stops is still a truncated frame: only
        // the boundary case means "the host is gone".
        let (mut client, mut host) = UnixStream::pair().unwrap();
        host.write_all(&[FRAME_STDOUT]).unwrap();
        drop(host);
        let truncated = read_frame(&mut client).unwrap_err();
        assert!(
            matches!(truncated, FrameError::Other(_)),
            "a frame that starts and stops is a truncated frame, not a gone host"
        );
        let truncated = truncated.message();
        assert!(
            truncated.starts_with("read check-host frame length:"),
            "{truncated}"
        );
    }

    /// The idle exit is gated on this count, so an unwind must not leak one.
    #[test]
    fn an_active_request_is_uncounted_however_it_leaves() {
        let active = Arc::new(AtomicUsize::new(0));
        {
            let _guard = ActiveGuard::new(&active);
            assert_eq!(active.load(Ordering::Relaxed), 1);
        }
        assert_eq!(active.load(Ordering::Relaxed), 0);

        let unwound = std::panic::catch_unwind({
            let active = active.clone();
            move || {
                let _guard = ActiveGuard::new(&active);
                panic!("a hosted request failed mid-flight");
            }
        });
        assert!(unwound.is_err());
        assert_eq!(
            active.load(Ordering::Relaxed),
            0,
            "a request that unwinds must not leave the host permanently busy"
        );
    }

    #[test]
    fn routing_only_wraps_work_executing_entry_points() {
        let words = |items: &[&str]| {
            items
                .iter()
                .map(|item| item.to_string())
                .collect::<Vec<_>>()
        };
        let forwards = |items: &[&str]| should_forward_with_host_state(&words(items), false);
        assert!(forwards(&["check", "check-engine"]));
        assert!(!forwards(&["check", "--help"]));
        assert!(forwards(&["ready"]));
        assert!(forwards(&["affected-checks", "--run"]));
        assert!(forwards(&["gate-run", "check-engine"]));
        assert!(!forwards(&["gate-run", "--list"]));
        assert!(forwards(&[
            "daemon-request",
            "/tmp/daemon.sock",
            "/td/store/build.drv"
        ]));
        assert!(!forwards(&[
            "daemon-request",
            "/tmp/daemon.sock",
            "SHUTDOWN"
        ]));
        assert!(!forwards(&["ready", "--record-only"]));
        assert!(!forwards(&["affected-checks"]));
        assert!(!forwards(&["build", "x"]));
        assert!(!should_forward_with_host_state(
            &words(&["check", "check-engine"]),
            true
        ));
    }

    #[test]
    fn request_codec_round_trips_non_utf8_fields() {
        let dir = std::env::temp_dir().join(format!("td-check-host-codec-{}", std::process::id()));
        let _ = std::fs::remove_file(&dir);
        let listener = UnixListener::bind(&dir).unwrap();
        let client = std::thread::spawn({
            let dir = dir.clone();
            move || {
                let mut stream = UnixStream::connect(dir).unwrap();
                let request = RunRequest {
                    exe: OsString::from_vec(vec![b'/', 0xff]),
                    cwd: OsString::from("/tmp"),
                    args: vec![OsString::from("check")],
                    env: vec![(OsString::from("K"), OsString::from_vec(vec![0xfe]))],
                };
                write_run_request(&mut stream, &request).unwrap();
            }
        });
        let (mut stream, _) = listener.accept().unwrap();
        let Request::Run(decoded) =
            read_request(&mut stream, Instant::now() + REQUEST_TIMEOUT).unwrap()
        else {
            panic!("expected a run request");
        };
        assert_eq!(decoded.exe.as_bytes(), [b'/', 0xff]);
        assert_eq!(decoded.env.first().unwrap().1.as_bytes(), [0xfe]);
        client.join().unwrap();
        let _ = std::fs::remove_file(&dir);
    }

    #[test]
    fn policy_keys_cannot_arrive_from_a_client_environment() {
        assert!(reserved_policy_key(OsStr::new("TD_CHECK_HOST_TOKEN_COUNT")));
        assert!(reserved_policy_key(OsStr::new(
            check_memory::JOB_BUDGET_ENV
        )));
        assert!(reserved_policy_key(OsStr::new("CARGO_BUILD_JOBS")));
        assert!(reserved_policy_key(OsStr::new(FEED_NO_DAEMON_ENV)));
        assert!(!reserved_policy_key(OsStr::new("TD_CHECK_DISABLE")));
    }

    #[test]
    fn rebuilt_clients_supply_the_namespace_supervisor_executable() {
        let request = RunRequest {
            exe: OsString::from("/worktree/target/release/td-builder"),
            cwd: OsString::from("/worktree"),
            args: Vec::new(),
            env: Vec::new(),
        };
        assert_eq!(
            supervisor_executable(&request),
            OsStr::new("/worktree/target/release/td-builder")
        );
    }
}

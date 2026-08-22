//! Lazy, rootless per-user host for memory-heavy check entry points.

use std::ffi::{OsStr, OsString};
use std::fs::OpenOptions;
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::check_memory::{self, HostBudget, TokenPool};

const SOCKET_NAME: &str = "check-host-v2.sock";
const START_LOCK_NAME: &str = "check-host-v2.start.lock";
const HOST_LOCK_NAME: &str = "check-host-v2.host.lock";
const LOG_NAME: &str = "check-host-v2.log";
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
const POLL_INTERVAL: Duration = Duration::from_millis(100);
const IDLE_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const FEED_NO_DAEMON_ENV: &str = "TD_FEED_NO_DAEMON";

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
}

impl FrameWriter {
    fn new(stream: UnixStream) -> io::Result<Arc<Self>> {
        stream.set_write_timeout(Some(OUTPUT_TIMEOUT))?;
        Ok(Arc::new(Self {
            stream: Mutex::new(stream),
            disconnected: AtomicBool::new(false),
        }))
    }

    fn frame(&self, kind: u8, bytes: &[u8]) -> io::Result<()> {
        let len = u32::try_from(bytes.len())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "oversized host frame"))?;
        let mut stream = self
            .stream
            .lock()
            .map_err(|_| io::Error::other("check-host output lock poisoned"))?;
        let result = stream
            .write_all(&[kind])
            .and_then(|()| stream.write_all(&len.to_be_bytes()))
            .and_then(|()| stream.write_all(bytes));
        if result.is_err() {
            self.disconnected.store(true, Ordering::Relaxed);
        }
        result
    }
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
    write_run_request(&mut stream, &request)?;
    let result = read_run_frames(&mut stream);
    drop(pinned_exe);
    result
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
    match runtime_dir().and_then(|runtime| stop(&runtime.join(SOCKET_NAME))) {
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
            return Err(format!(
                "live check host did not publish {} within {}s",
                socket.display(),
                START_TIMEOUT.as_secs()
            ));
        }
        Ok(true) => drop(host_probe),
        Err(e) => {
            return Err(format!(
                "probe host lifetime lock {}: {e}",
                host_lock_path.display()
            ))
        }
    }

    let log_path = runtime.join(LOG_NAME);
    let log = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(&log_path)
        .map_err(|e| format!("open check-host log {}: {e}", log_path.display()))?;
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
    terminate_tree(child.id());
    let _ = child.wait();
    Err(format!(
        "check host did not become ready within {}s; see {}",
        START_TIMEOUT.as_secs(),
        log_path.display()
    ))
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
    let (kind, bytes) = read_frame(&mut stream)?;
    if kind == FRAME_PONG && bytes.is_empty() {
        Ok(())
    } else {
        Err("check host returned an invalid ping response".to_string())
    }
}

fn stop(socket: &Path) -> Result<(), String> {
    let mut stream =
        UnixStream::connect(socket).map_err(|e| format!("connect to {}: {e}", socket.display()))?;
    write_control_request(&mut stream, REQUEST_STOP)?;
    let (kind, bytes) = read_frame(&mut stream)?;
    if kind == FRAME_EXIT && bytes == 0u32.to_be_bytes() {
        Ok(())
    } else {
        Err("check host returned an invalid stop response".to_string())
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
        "td-builder: per-user check host listening on {} ({} GiB work, {} GiB reserve, {} tokens)",
        socket.display(),
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

    let mut idle_since = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _)) => {
                idle_since = Instant::now();
                if send.send(stream).is_err() {
                    break;
                }
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                if active.load(Ordering::Relaxed) == 0 && idle_since.elapsed() >= IDLE_TIMEOUT {
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => eprintln!("td-builder: check host accept error: {e}"),
        }
    }
    drop(send);
    for worker in workers {
        let _ = worker.join();
    }
    let _ = std::fs::remove_file(&socket);
    Ok(())
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
                if let Ok(writer) = FrameWriter::new(stream) {
                    let _ = writer.frame(FRAME_PONG, &[]);
                }
            }
            Ok(Request::Stop) => {
                stop.store(true, Ordering::Relaxed);
                if let Ok(writer) = FrameWriter::new(stream) {
                    let _ = writer.frame(FRAME_EXIT, &0u32.to_be_bytes());
                }
            }
            Ok(Request::Run(request)) => {
                active.fetch_add(1, Ordering::Relaxed);
                let error_stream = stream.try_clone().ok();
                if let Err(e) = run_request(stream, request, &pool, &budget, &runtime, &token_dir) {
                    eprintln!("td-builder: check host request failed: {e}");
                    if let Some(error_stream) = error_stream {
                        if let Ok(writer) = FrameWriter::new(error_stream) {
                            let line = format!("td-builder: check host: {e}\n");
                            let _ = writer.frame(FRAME_STDERR, line.as_bytes());
                            let _ = writer.frame(FRAME_EXIT, &1u32.to_be_bytes());
                        }
                    }
                }
                active.fetch_sub(1, Ordering::Relaxed);
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
    let writer_stream = stream
        .try_clone()
        .map_err(|e| format!("clone check-host client socket: {e}"))?;
    let writer = FrameWriter::new(writer_stream)
        .map_err(|e| format!("bound check-host client output: {e}"))?;

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
                terminate_tree(pid);
                return Err(format!("wait for hosted command {pid}: {e}"));
            }
        }
        let pressure = check_memory::emergency_memory_available(emergency)
            .map(|available| !available)
            .unwrap_or(true);
        if writer.disconnected.load(Ordering::Relaxed) || peer_gone(&mut stream) || pressure {
            cancelled = true;
            if pressure {
                let _ = writer.frame(
                    FRAME_STDERR,
                    b"td-builder: check host: emergency memory reserve crossed; cancelling hosted check\n",
                );
            }
            terminate_tree(pid);
            break child
                .wait()
                .map_err(|e| format!("wait after cancelling hosted command {pid}: {e}"))?;
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
    let _ = writer.frame(FRAME_EXIT, &code.to_be_bytes());
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

fn read_run_frames(stream: &mut UnixStream) -> Result<u8, String> {
    loop {
        let (kind, bytes) = read_frame(stream)?;
        match kind {
            FRAME_STDOUT => std::io::stdout()
                .write_all(&bytes)
                .map_err(|e| format!("write hosted stdout: {e}"))?,
            FRAME_STDERR => std::io::stderr()
                .write_all(&bytes)
                .map_err(|e| format!("write hosted stderr: {e}"))?,
            FRAME_EXIT if bytes.len() == 4 => {
                let raw: [u8; 4] = bytes
                    .as_slice()
                    .try_into()
                    .map_err(|_| "invalid check-host exit frame".to_string())?;
                return Ok(u32::from_be_bytes(raw).min(255) as u8);
            }
            _ => return Err(format!("invalid check-host response frame {kind}")),
        }
    }
}

fn read_frame(stream: &mut UnixStream) -> Result<(u8, Vec<u8>), String> {
    let mut kind = [0u8; 1];
    stream
        .read_exact(&mut kind)
        .map_err(|e| format!("read check-host frame type: {e}"))?;
    let mut raw = [0u8; 4];
    stream
        .read_exact(&mut raw)
        .map_err(|e| format!("read check-host frame length: {e}"))?;
    let len = usize::try_from(u32::from_be_bytes(raw)).unwrap_or(usize::MAX);
    if len > IO_CHUNK_BYTES {
        return Err(format!("check-host frame exceeds {IO_CHUNK_BYTES} bytes"));
    }
    let mut bytes = vec![0u8; len];
    stream
        .read_exact(&mut bytes)
        .map_err(|e| format!("read check-host frame body: {e}"))?;
    Ok((kind[0], bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

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

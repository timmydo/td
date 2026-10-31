//! Restricted remote administration and durable trusted-main update jobs.

use crate::{io, Result, O_NOFOLLOW, O_NONBLOCK};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{chown, DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub(crate) const ADMIN_UID: u32 = 986;
pub(crate) const BUILD_UID: u32 = 985;
const STATE: &str = "/var/lib/td-admin";
const BUILD_HOME: &str = "/var/lib/td-admin-build";
const SOCKET: &str = "/run/td-admin/control";
const OPERATION_GROUP: &str = "/sys/fs/cgroup/system/td-admin/operation";
const GIT_GROUP: &str = "/sys/fs/cgroup/system/td-admin/git";
const BUILD_GROUP: &str = "/sys/fs/cgroup/system/td-admin/build";
const UPSTREAM: &str = "https://github.com/timmydo/td.git";
const HELP: &str = "td-admin update [--no-reboot] | rollback [--no-reboot] | status [JOB] | logs [JOB] | cancel | reboot\nSSH: ssh td-admin@HOST update [--no-reboot]\nUpdates build trusted upstream main and reboot by default. Jobs survive disconnects.\nInitial setup: td-admin enroll-key PUBLIC_KEY_FILE, then approve H on secure attention.\n";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Request {
    Update { reboot: bool },
    Rollback { reboot: bool },
    Status(Option<u64>),
    Logs(Option<u64>),
    Cancel,
    Reboot,
}

fn parse(words: &[&str]) -> Result<Request> {
    match words {
        ["update"] => Ok(Request::Update { reboot: true }),
        ["update", "--no-reboot"] => Ok(Request::Update { reboot: false }),
        ["rollback"] => Ok(Request::Rollback { reboot: true }),
        ["rollback", "--no-reboot"] => Ok(Request::Rollback { reboot: false }),
        ["status"] => Ok(Request::Status(None)),
        ["status", job] => Ok(Request::Status(Some(job_id(job)?))),
        ["logs"] => Ok(Request::Logs(None)),
        ["logs", job] => Ok(Request::Logs(Some(job_id(job)?))),
        ["cancel"] => Ok(Request::Cancel),
        ["reboot"] => Ok(Request::Reboot),
        _ => Err(HELP.into()),
    }
}

fn job_id(text: &str) -> Result<u64> {
    if text.is_empty() || text.starts_with('0') || !text.bytes().all(|b| b.is_ascii_digit()) {
        return Err("invalid job ID".into());
    }
    text.parse().map_err(|_| "invalid job ID".into())
}

fn root() -> Result<()> {
    if io(fs::metadata("/proc/self"), "read admin identity")?.uid() != 0 {
        return Err("this internal operation requires root".into());
    }
    Ok(())
}

fn directory(path: &Path, uid: u32, gid: u32, mode: u32) -> Result<()> {
    for parent in path.ancestors().skip(1) {
        let metadata = io(
            fs::symlink_metadata(parent),
            "inspect admin directory ancestor",
        )?;
        if !metadata.is_dir() || metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
            return Err("admin directory ancestors must be protected root directories".into());
        }
    }
    match fs::DirBuilder::new().mode(mode).create(path) {
        Ok(()) => {
            io(chown(path, Some(uid), Some(gid)), "assign admin directory")?;
            io(
                fs::set_permissions(path, fs::Permissions::from_mode(mode)),
                "protect admin directory",
            )?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(format!("create {}: {e}", path.display())),
    }
    let m = io(fs::symlink_metadata(path), "inspect admin directory")?;
    if !m.is_dir() || m.uid() != uid || m.gid() != gid || m.mode() & 0o7777 != mode {
        return Err(format!(
            "{} has an unexpected type, owner or mode",
            path.display()
        ));
    }
    Ok(())
}

fn read(path: &Path, limit: u64) -> Result<String> {
    let file = io(
        OpenOptions::new()
            .read(true)
            .custom_flags(O_NOFOLLOW | O_NONBLOCK)
            .open(path),
        "open admin record",
    )?;
    let m = io(file.metadata(), "inspect admin record")?;
    if !m.is_file() || m.len() > limit {
        return Err("admin record is not a bounded regular file".into());
    }
    let mut text = String::new();
    io(
        file.take(limit + 1).read_to_string(&mut text),
        "read admin record",
    )?;
    if text.len() as u64 > limit {
        return Err("admin record grew beyond its bound".into());
    }
    Ok(text)
}

fn publish(path: &Path, text: &str) -> Result<()> {
    let parent = path.parent().ok_or("admin record has no parent")?;
    let next = path.with_extension("next");
    match fs::remove_file(&next) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("remove interrupted admin record: {e}")),
    }
    let mut file = io(
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&next),
        "create admin record",
    )?;
    io(
        file.write_all(text.as_bytes())
            .and_then(|()| file.sync_all()),
        "persist admin record",
    )?;
    io(fs::rename(next, path), "publish admin record")?;
    io(
        File::open(parent).and_then(|f| f.sync_all()),
        "sync admin directory",
    )
}

#[derive(Default)]
struct Job {
    active: bool,
    cancel: bool,
    installing: bool,
    id: u64,
    text: String,
}

type Shared = Arc<Mutex<Job>>;

fn record(shared: &Shared, phase: &str, detail: &str) -> Result<()> {
    let mut job = shared.lock().map_err(|_| "admin state lock poisoned")?;
    let detail: String = detail
        .chars()
        .scan(0usize, |length, ch| {
            *length += ch.len_utf8();
            (*length <= 12000).then_some(ch)
        })
        .collect();
    let text = format!("job {}: {phase}\n{}\n", job.id, detail);
    publish(
        &Path::new(STATE)
            .join("jobs")
            .join(format!("{}.status", job.id)),
        &text,
    )?;
    publish(&Path::new(STATE).join("status"), &text)?;
    job.text = text;
    Ok(())
}

fn cancelled(shared: &Shared) -> Result<()> {
    if shared
        .lock()
        .map_err(|_| "admin state lock poisoned")?
        .cancel
    {
        return Err("cancelled before installation".into());
    }
    Ok(())
}

fn fixed(program: &str) -> Command {
    let mut cmd = Command::new(program);
    cmd.env_clear()
        .env("PATH", "/bin")
        .current_dir("/")
        .stdin(Stdio::null());
    cmd
}

fn spawn_contained(command: &mut Command, group: &str) -> Result<Child> {
    let mut child = io(command.stdin(Stdio::piped()).spawn(), "start gated helper")?;
    let admitted = (|| {
        io(
            fs::write(
                Path::new(group).join("cgroup.procs"),
                format!("{}\n", child.id()),
            ),
            "contain helper",
        )?;
        let mut gate = child.stdin.take().ok_or("missing helper gate")?;
        io(gate.write_all(b"G"), "release contained helper")
    })();
    if let Err(error) = admitted {
        let _ = child.kill();
        let _ = child.wait();
        drain_group(group)?;
        return Err(error);
    }
    Ok(child)
}

fn wait_helper(child: &mut Child, group: &str, shared: Option<&Shared>) -> Result<ExitStatus> {
    let result = loop {
        if let Some(shared) = shared {
            if let Err(error) = cancelled(shared) {
                break Err(error);
            }
        }
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) => std::thread::sleep(Duration::from_millis(100)),
            Err(error) => break Err(format!("wait for helper: {error}")),
        }
    };
    // Stop the direct child before the final drain: it cannot join or fork later.
    let _ = child.kill();
    let _ = child.wait();
    drain_group(group)?;
    result
}

fn git(args: &[&str], log: &File, shared: &Shared) -> Result<String> {
    let mut command = fixed("/bin/td-admin");
    command
        .args(["git-child", "--git-dir"])
        .arg(Path::new(STATE).join("repository.git"));
    capture(command.args(args), log, shared)
}

fn capture(command: &mut Command, log: &File, shared: &Shared) -> Result<String> {
    drain_group(GIT_GROUP)?;
    cancelled(shared)?;
    command
        .stdout(Stdio::piped())
        .stderr(Stdio::from(io(log.try_clone(), "retain Git log")?));
    let mut child = spawn_contained(command, GIT_GROUP)?;
    let stdout = child.stdout.take().ok_or("missing Git output")?;
    // Continue draining after the bound, so an oversized output cannot stall Git.
    let reader = std::thread::Builder::new()
        .name("admin-git-output".into())
        .spawn(move || {
            let mut bytes = Vec::new();
            let mut stdout = stdout;
            io(
                (&mut stdout).take(4097).read_to_end(&mut bytes),
                "read Git output",
            )?;
            io(
                std::io::copy(&mut stdout, &mut std::io::sink()),
                "drain Git output",
            )?;
            if bytes.len() > 4096 {
                return Err("Git output exceeded its bound".into());
            }
            String::from_utf8(bytes).map_err(|_| "Git output is not UTF-8".into())
        });
    let reader = match reader {
        Ok(reader) => reader,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            drain_group(GIT_GROUP)?;
            return Err(format!("start Git output reader: {error}"));
        }
    };
    let status = wait_helper(&mut child, GIT_GROUP, Some(shared))?;
    let output = reader.join().map_err(|_| "Git output reader failed")?;
    if !status.success() {
        return Err("admin Git failed; see td-admin logs".into());
    }
    output
}

fn operation(args: &[&str]) -> Result<()> {
    drain_group(OPERATION_GROUP)?;
    let mut child = spawn_contained(fixed("/bin/td-admin").args(args), OPERATION_GROUP)?;
    if wait_helper(&mut child, OPERATION_GROUP, None)?.success() {
        Ok(())
    } else {
        Err("admin operation failed; see service log".into())
    }
}

fn expose_repository(path: &Path, budget: &mut usize) -> Result<()> {
    if *budget == 0 {
        return Err("admin repository exceeds its file bound".into());
    }
    *budget -= 1;
    let metadata = io(fs::symlink_metadata(path), "inspect private repository")?;
    if metadata.uid() != 0 || (!metadata.is_dir() && !metadata.is_file()) {
        return Err("admin repository contains a foreign owner or special file".into());
    }
    io(
        fs::set_permissions(
            path,
            fs::Permissions::from_mode(if metadata.is_dir() { 0o755 } else { 0o644 }),
        ),
        "make source readable to builder",
    )?;
    if metadata.is_dir() {
        for entry in io(fs::read_dir(path), "enumerate admin repository")? {
            expose_repository(&io(entry, "read repository entry")?.path(), budget)?;
        }
    }
    Ok(())
}

fn commit_id(text: &str) -> Result<&str> {
    let id = text.strip_suffix('\n').unwrap_or(text);
    if !matches!(id.len(), 40 | 64)
        || !id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err("invalid upstream commit ID".into());
    }
    Ok(id)
}

fn select(log: &File, shared: &Shared) -> Result<String> {
    let repository = Path::new(STATE).join("repository.git");
    if !crate::present(&repository)? {
        capture(
            fixed("/bin/td-admin")
                .args(["git-child", "init", "--bare"])
                .arg(&repository),
            log,
            shared,
        )?;
    }
    git(
        &[
            "-c",
            "protocol.file.allow=never",
            "-c",
            "http.lowSpeedLimit=1",
            "-c",
            "http.lowSpeedTime=30",
            "fetch",
            "--no-tags",
            "--no-recurse-submodules",
            UPSTREAM,
            "+refs/heads/main:refs/heads/main",
        ],
        log,
        shared,
    )?;
    let revision = git(
        &["rev-parse", "--verify", "refs/heads/main^{commit}"],
        log,
        shared,
    )?;
    let revision = commit_id(&revision)?.to_string();
    let previous = Path::new(STATE).join("accepted-commit");
    if crate::present(&previous)? {
        let old = read(&previous, 65)?;
        git(
            &["merge-base", "--is-ancestor", commit_id(&old)?, &revision],
            log,
            shared,
        )
        .map_err(|e| {
            format!("trusted main is not a fast-forward from the last accepted commit: {e}")
        })?;
    }
    expose_repository(&repository, &mut 1_000_000)?;
    publish(&previous, &format!("{revision}\n"))?;
    Ok(revision)
}

fn drain_group(group: &str) -> Result<()> {
    io(
        fs::write(Path::new(group).join("cgroup.kill"), b"1\n"),
        "stop worker descendants",
    )?;
    for _ in 0..200 {
        let events = read(&Path::new(group).join("cgroup.events"), 4096)?;
        if events.lines().any(|line| line == "populated 0") {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Err("worker descendants did not stop; refusing work".into())
}

fn build_job(shared: &Shared, revision: &str, log: &File) -> Result<PathBuf> {
    drain_group(BUILD_GROUP)?;
    let receipt = Path::new(BUILD_HOME).join("result");
    if crate::present(&receipt)? {
        io(fs::remove_file(&receipt), "remove previous build receipt")?;
    }
    let mut child = spawn_contained(
        fixed("/bin/td-admin")
            .args(["build-child", revision])
            .stdout(Stdio::from(io(log.try_clone(), "retain build log")?))
            .stderr(Stdio::from(io(log.try_clone(), "retain build log")?)),
        BUILD_GROUP,
    )?;
    if !wait_helper(&mut child, BUILD_GROUP, Some(shared))?.success() {
        return Err("build failed; see td-admin logs".into());
    }
    let result = read(&receipt, 4096)?;
    let path = PathBuf::from(result.trim_end_matches('\n'));
    let base = Path::new(BUILD_HOME).join("source/.td-build-cache/update-deployments");
    if path.parent() != Some(base.as_path())
        || !path
            .file_name()
            .and_then(|s| s.to_str())
            .is_some_and(|s| crate::protocol::valid_digest(s.as_bytes()))
    {
        return Err("build returned an invalid deployment receipt".into());
    }
    Ok(path)
}

fn update(shared: &Shared, reboot: bool, log: &File) -> Result<()> {
    record(shared, "fetching main", UPSTREAM)?;
    let revision = select(log, shared)?;
    cancelled(shared)?;
    record(
        shared,
        "building",
        &format!("commit {revision}\nreboot {reboot}"),
    )?;
    let source = build_job(shared, &revision, log)?;
    let manifest = read(
        &source.join("manifest"),
        crate::protocol::MAX_MANIFEST_BYTES,
    )?;
    let deployment = crate::sha256::hex_digest(manifest.as_bytes());
    {
        let mut job = shared.lock().map_err(|_| "admin state lock poisoned")?;
        if job.cancel {
            return Err("cancelled before installation".into());
        }
        job.installing = true;
    }
    let detail = format!("commit {revision}\ndeployment {deployment}\nreboot {reboot}");
    record(shared, "installing", &detail)?;
    operation(&[
        "install-child",
        source.to_str().ok_or("deployment path is not UTF-8")?,
        &deployment,
    ])?;
    record(shared, "installed", &detail)?;
    if reboot {
        record(shared, "reboot requested", &detail)?;
        operation(&["reboot-child"])?;
    }
    Ok(())
}

fn selector(slot: &str) -> Result<String> {
    let target = io(
        fs::read_link(Path::new("/run/td-volume/boot").join(slot)),
        "read deployment selector",
    )?;
    let id = target
        .to_str()
        .and_then(|s| s.strip_prefix("../deployments/"))
        .filter(|id| crate::protocol::valid_digest(id.as_bytes()))
        .ok_or("invalid deployment selector")?;
    Ok(id.into())
}

fn rollback(shared: &Shared, reboot: bool) -> Result<()> {
    let current = selector("current")?;
    let previous = selector("previous")?;
    if current == previous {
        return Err("no previous deployment to roll back to".into());
    }
    {
        let mut job = shared.lock().map_err(|_| "admin state lock poisoned")?;
        if job.cancel {
            return Err("rollback cancelled".into());
        }
        job.installing = true;
    }
    let detail = format!("current {current}\nprevious {previous}\nreboot {reboot}");
    record(shared, "rolling back", &detail)?;
    operation(&["rollback-child", &current, &previous])?;
    record(shared, "rolled back", &detail)?;
    if reboot {
        record(shared, "reboot requested", &detail)?;
        operation(&["reboot-child"])?;
    }
    Ok(())
}

fn request(shared: &Shared, request: Request) -> Result<String> {
    let mut job = shared.lock().map_err(|_| "admin state lock poisoned")?;
    match request {
        Request::Status(id) => {
            return if id.is_none() || id == Some(job.id) {
                Ok(job.text.clone())
            } else {
                read(
                    &Path::new(STATE)
                        .join("jobs")
                        .join(format!("{}.status", id.ok_or("missing job ID")?)),
                    16384,
                )
            }
        }
        Request::Logs(id) => {
            let path = Path::new(STATE)
                .join("jobs")
                .join(format!("{}.log", id.unwrap_or(job.id)));
            if !crate::present(&path)? {
                return Ok("No build log yet.\n".into());
            }
            let mut file = io(File::open(path), "open build log")?;
            let length = io(file.metadata(), "inspect build log")?.len();
            io(
                file.seek(SeekFrom::Start(length.saturating_sub(65536))),
                "seek recent build log",
            )?;
            let mut bytes = Vec::new();
            io(
                file.take(65536).read_to_end(&mut bytes),
                "read recent build log",
            )?;
            return Ok(String::from_utf8_lossy(&bytes).into_owned());
        }
        Request::Cancel => {
            if !job.active {
                return Ok(job.text.clone());
            }
            if job.installing {
                return Err("installation has started; it must finish".into());
            }
            job.cancel = true;
            return Ok("Cancellation requested. Use status to observe completion.\n".into());
        }
        Request::Reboot if job.active => return Err("an update is active; use status".into()),
        Request::Reboot => {
            operation(&["reboot-child"])?;
            return Ok("Reboot requested.\n".into());
        }
        Request::Update { .. } | Request::Rollback { .. } if job.active => {
            return Ok(job.text.clone())
        }
        Request::Update { .. } | Request::Rollback { .. } => {}
    }
    let (reboot, operation) = match request {
        Request::Update { reboot } => (reboot, "update"),
        Request::Rollback { reboot } => (reboot, "rollback"),
        _ => return Err("invalid job request".into()),
    };
    let (id, log) = allocate_log(&Path::new(STATE).join("jobs"), job.id)?;
    publish(&Path::new(STATE).join("sequence"), &format!("{id}\n"))?;
    let text = format!("job {id}: queued {operation}\nreboot {reboot}\n");
    publish(
        &Path::new(STATE).join("jobs").join(format!("{id}.status")),
        &text,
    )?;
    publish(&Path::new(STATE).join("status"), &text)?;
    *job = Job {
        active: true,
        id,
        text: text.clone(),
        ..Job::default()
    };
    let worker = Arc::clone(shared);
    if let Err(error) = std::thread::Builder::new()
        .name("admin-update".into())
        .spawn(move || {
            let result = if operation == "update" {
                update(&worker, reboot, &log)
            } else {
                rollback(&worker, reboot)
            };
            if let Err(error) = result {
                let prior = worker
                    .lock()
                    .map(|job| job.text.clone())
                    .unwrap_or_default();
                let _ = record(&worker, "failed", &format!("{error}\nPrior state: {prior}"));
                eprintln!("td-admin: {error}");
            }
            if let Ok(mut job) = worker.lock() {
                job.active = false;
            }
        })
    {
        job.active = false;
        drop(job);
        let message = format!("start update worker: {error}");
        record(shared, "failed", &message)?;
        return Err(message);
    }
    Ok(text)
}

fn allocate_log(directory: &Path, previous: u64) -> Result<(u64, File)> {
    let mut id = previous;
    for _ in 0..1024 {
        id = id.checked_add(1).ok_or("admin job counter exhausted")?;
        match OpenOptions::new()
            .append(true)
            .create_new(true)
            .mode(0o600)
            .open(directory.join(format!("{id}.log")))
        {
            Ok(file) => return Ok((id, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(format!("create update log: {error}")),
        }
    }
    Err("too many interrupted job allocations".into())
}

fn recovered_status(id: u64, previous: &str) -> String {
    if previous
        .lines()
        .next()
        .is_some_and(|line| line == format!("job {id}: not running after service restart"))
    {
        return previous.into();
    }
    format!("job {id}: not running after service restart\nInspect the previous report before retrying.\n{previous}")
}

fn serve() -> Result<()> {
    root()?;
    directory(Path::new(STATE), 0, 0, 0o755)?;
    directory(&Path::new(STATE).join("jobs"), 0, 0, 0o700)?;
    directory(Path::new(BUILD_HOME), BUILD_UID, BUILD_UID, 0o700)?;
    directory(Path::new("/run/td-admin"), 0, ADMIN_UID, 0o710)?;
    let lease = io(
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(O_NOFOLLOW)
            .open(Path::new(STATE).join("service.lock")),
        "open admin lease",
    )?;
    lease
        .try_lock()
        .map_err(|e| format!("admin service already running: {e}"))?;
    io(
        fs::create_dir_all(BUILD_GROUP),
        "create contained build group",
    )?;
    io(fs::create_dir_all(GIT_GROUP), "create contained Git group")?;
    io(
        fs::create_dir_all(OPERATION_GROUP),
        "create contained operation group",
    )?;
    drain_group(GIT_GROUP)?;
    drain_group(OPERATION_GROUP)?;
    drain_group(BUILD_GROUP)?;
    let sequence = Path::new(STATE).join("sequence");
    let id = if crate::present(&sequence)? {
        read(&sequence, 32)?
            .trim()
            .parse::<u64>()
            .map_err(|_| "invalid job sequence")?
    } else {
        0
    };
    let status = Path::new(STATE).join("status");
    let text = if crate::present(&status)? {
        let text = recovered_status(id, &read(&status, 16384)?);
        publish(
            &Path::new(STATE).join("jobs").join(format!("{id}.status")),
            &text,
        )?;
        publish(&status, &text)?;
        text
    } else {
        "No update has been requested.\n".into()
    };
    let shared = Arc::new(Mutex::new(Job {
        id,
        text,
        ..Job::default()
    }));
    if crate::present(Path::new(SOCKET))? {
        io(fs::remove_file(SOCKET), "remove old admin socket")?;
    }
    let listener = io(UnixListener::bind(SOCKET), "bind admin socket")?;
    io(
        chown(SOCKET, Some(0), Some(ADMIN_UID)),
        "assign admin socket",
    )?;
    io(
        fs::set_permissions(SOCKET, fs::Permissions::from_mode(0o660)),
        "protect admin socket",
    )?;
    for stream in listener.incoming() {
        let mut stream = io(stream, "accept admin connection")?;
        let result = (|| {
            io(
                stream.set_read_timeout(Some(Duration::from_secs(2))),
                "bound admin request",
            )?;
            io(
                stream.set_write_timeout(Some(Duration::from_secs(2))),
                "bound admin response",
            )?;
            let mut bytes = Vec::new();
            io(
                (&mut stream).take(257).read_to_end(&mut bytes),
                "read admin request",
            )?;
            if bytes.len() > 256 {
                return Err("admin command is too long".into());
            }
            let text = std::str::from_utf8(&bytes).map_err(|_| "admin command is not UTF-8")?;
            request(
                &shared,
                parse(&text.split_whitespace().collect::<Vec<_>>())?,
            )
        })();
        let reply = match result {
            Ok(text) => format!("OK\n{text}"),
            Err(error) => format!("ERR\n{error}\n"),
        };
        let _ = stream.write_all(reply.as_bytes());
    }
    Err("admin listener ended".into())
}

fn client(words: &[&str]) -> Result<()> {
    parse(words)?;
    let mut stream = io(
        UnixStream::connect(SOCKET),
        "connect to admin service (use the dedicated admin SSH key)",
    )?;
    io(
        stream.set_read_timeout(Some(Duration::from_secs(10))),
        "bound admin response",
    )?;
    io(
        stream.set_write_timeout(Some(Duration::from_secs(2))),
        "bound admin request",
    )?;
    io(
        stream.write_all(words.join(" ").as_bytes()),
        "send admin command",
    )?;
    io(
        stream.shutdown(std::net::Shutdown::Write),
        "finish admin command",
    )?;
    let mut response = String::new();
    io(
        stream.take(70000).read_to_string(&mut response),
        "read admin response",
    )?;
    if let Some(text) = response.strip_prefix("OK\n") {
        io(
            std::io::stdout().write_all(text.as_bytes()),
            "show admin result",
        )
    } else {
        Err(response
            .strip_prefix("ERR\n")
            .unwrap_or("incomplete admin response")
            .into())
    }
}

fn await_gate(mut reader: impl Read) -> Result<()> {
    let mut gate = [0];
    io(reader.read_exact(&mut gate), "wait for helper containment")?;
    if gate != [b'G'] {
        return Err("invalid helper start gate".into());
    }
    Ok(())
}

fn join_group(group: &str) -> Result<()> {
    root()?;
    await_gate(std::io::stdin())?;
    let membership = read(Path::new("/proc/self/cgroup"), 4096)?;
    let relative = group
        .strip_prefix("/sys/fs/cgroup")
        .ok_or("invalid worker group")?;
    if membership.trim() != format!("0::{relative}") {
        return Err("worker cgroup membership did not stick".into());
    }
    Ok(())
}

fn git_child(args: &[&str]) -> Result<()> {
    join_group(GIT_GROUP)?;
    Err(format!(
        "start contained Git: {}",
        crate::git(Path::new("/bin/git"), Path::new(STATE))
            .args(args)
            .exec()
    ))
}

fn build_child(revision: &str) -> Result<()> {
    commit_id(revision)?;
    join_group(BUILD_GROUP)?;
    Err(format!(
        "start build identity: {}",
        fixed("/bin/td-login")
            .args([
                "exec-service-as",
                "td-admin-build",
                "--",
                "/bin/td-admin",
                "build-worker",
                revision
            ])
            .exec()
    ))
}

fn build_worker(revision: &str) -> Result<()> {
    commit_id(revision)?;
    if io(fs::metadata("/proc/self"), "read build identity")?.uid() != BUILD_UID {
        return Err("build worker requires its private identity".into());
    }
    let home = Path::new(BUILD_HOME);
    let source = home.join("source");
    if crate::present(&source)? {
        io(fs::remove_dir_all(&source), "remove prior source checkout")?;
    }
    let lease = crate::source_lock(home)?;
    crate::run_git(
        crate::git(Path::new("/bin/git"), home)
            .args([
                "-c",
                "safe.directory=/var/lib/td-admin/repository.git",
                "clone",
                "--no-checkout",
                "--no-hardlinks",
                "--",
            ])
            .arg(Path::new(STATE).join("repository.git"))
            .arg(&source),
        &lease,
        "clone admitted source",
    )?;
    crate::run_git(
        crate::git(Path::new("/bin/git"), &source).args(["checkout", "--detach", revision]),
        &lease,
        "check out admitted commit",
    )?;
    let actual = crate::run_git(
        crate::git(Path::new("/bin/git"), &source).args(["rev-parse", "HEAD"]),
        &lease,
        "verify admitted source",
    )?;
    if commit_id(&actual)? != revision {
        return Err("build checkout changed commit".into());
    }
    let output = crate::build(&source, home)?;
    publish(&home.join("result"), &format!("{}\n", output.display()))
}

fn key_line(text: &str) -> Result<String> {
    let mut fields = text.split_whitespace();
    if fields.next() != Some("ssh-ed25519") || text.trim().contains(['\n', '\r']) {
        return Err("supply one plain Ed25519 public key, without authorized_keys options".into());
    }
    let blob = fields.next().ok_or("missing public key blob")?;
    if blob.len() != 68
        || !blob
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/')
    {
        return Err("invalid Ed25519 public key encoding".into());
    }
    Ok(format!("ssh-ed25519 {blob}\n"))
}

fn accept_key(blob: &str) -> Result<()> {
    root()?;
    directory(Path::new(STATE), 0, 0, 0o755)?;
    let key = key_line(&format!("ssh-ed25519 {blob}"))?;
    publish_key(Path::new(STATE), &key, |next| {
        let validation = io(
            fixed("/bin/ssh-keygen")
                .args(["-l", "-f"])
                .arg(next)
                .output(),
            "validate admin public key",
        )?;
        if validation.status.success() {
            Ok(())
        } else {
            Err("ssh-keygen refused the admin public key".into())
        }
    })
}

struct EnrollmentLease(File);
impl Drop for EnrollmentLease {
    fn drop(&mut self) {
        // A concurrent spawn can briefly retain the open description pre-exec.
        let _ = self.0.unlock();
    }
}

fn enrollment_lease(state: &Path) -> Result<EnrollmentLease> {
    let lease = io(
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(O_NOFOLLOW)
            .open(state.join("key.lock")),
        "open key enrollment lock",
    )?;
    lease
        .try_lock()
        .map_err(|e| format!("key enrollment is busy: {e}"))?;
    Ok(EnrollmentLease(lease))
}

fn publish_key(state: &Path, key: &str, validate: impl FnOnce(&Path) -> Result<()>) -> Result<()> {
    let _lease = enrollment_lease(state)?;
    let destination = state.join("authorized_keys");
    if crate::present(&destination)? {
        return Err(
            "an administrator key is already enrolled; initial enrollment cannot replace it".into(),
        );
    }
    let next = state.join("key-candidate");
    publish(&next, key)?;
    if let Err(error) = validate(&next) {
        let _ = fs::remove_file(next);
        return Err(error);
    }
    io(
        fs::set_permissions(&next, fs::Permissions::from_mode(0o644)),
        "make public key readable to sshd",
    )?;
    io(
        File::open(&next).and_then(|file| file.sync_all()),
        "persist public key",
    )?;
    io(
        fs::hard_link(&next, &destination),
        "publish first admin SSH key",
    )?;
    io(fs::remove_file(&next), "remove key candidate")?;
    io(
        File::open(state).and_then(|file| file.sync_all()),
        "persist admin SSH key",
    )
}

pub(crate) fn run(args: &[String]) -> Result<()> {
    let words: Vec<_> = args.iter().map(String::as_str).collect();
    match words.as_slice() {
        ["serve"] => serve(),
        ["accept-key", blob] => accept_key(blob),
        ["enroll-key", source] => {
            let key = key_line(&read(Path::new(source), 4096)?)?;
            let blob = key.split_whitespace().nth(1).ok_or("missing public key")?;
            crate::success(
                fixed("/bin/td-authd").args(["request-hostname", &format!("admin-key {blob}")]),
                "request admin key enrollment",
            )
        }
        ["install-child", source, deployment] => {
            join_group(OPERATION_GROUP)?;
            crate::apply::admin(Path::new(source), deployment, BUILD_UID)
        }
        ["rollback-child", current, previous] => {
            join_group(OPERATION_GROUP)?;
            crate::success(
                fixed("/bin/td-boot").args([
                    "on-volume",
                    "rollback",
                    "/run/td-update",
                    current,
                    previous,
                ]),
                "roll back deployment",
            )
        }
        ["reboot-child"] => {
            join_group(OPERATION_GROUP)?;
            crate::success(fixed("/bin/td-svc").arg("reboot"), "request orderly reboot")
        }
        ["git-child", args @ ..] => git_child(args),
        ["build-child", revision] => build_child(revision),
        ["build-worker", revision] => build_worker(revision),
        ["--help" | "-h"] | [] => io(
            std::io::stdout().write_all(HELP.as_bytes()),
            "write admin help",
        ),
        ["-c", "/bin/td-admin ssh"] => {
            let original =
                std::env::var("SSH_ORIGINAL_COMMAND").map_err(|_| "SSH command is missing")?;
            client(&original.split_whitespace().collect::<Vec<_>>())
        }
        _ => client(&words),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    #[test]
    fn update_reboots_by_default_and_only_accepts_the_fixed_interface() {
        assert_eq!(parse(&["update"]), Ok(Request::Update { reboot: true }));
        assert_eq!(
            parse(&["update", "--no-reboot"]),
            Ok(Request::Update { reboot: false })
        );
        for words in [
            vec!["update", "--repo", "/tmp/repo"],
            vec!["update", "main"],
            vec!["update;reboot"],
            vec!["sh"],
            vec!["serve"],
            vec!["update", "--no-reboot", "--no-reboot"],
        ] {
            assert!(parse(&words).is_err());
        }
    }
    #[test]
    fn upstream_revisions_are_exact_object_ids() {
        assert!(commit_id(&"a".repeat(40)).is_ok());
        for text in [
            "main",
            "HEAD",
            "-x",
            "abc\nreboot",
            "",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaG",
        ] {
            assert!(commit_id(text).is_err());
        }
    }
    const KEY: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIGmTjqOMdHoPzoqbE+wEE5Q4aNmEGC0tgJbQOkxqdgUi";

    #[test]
    fn enrollment_unlocks_even_if_a_spawn_retains_its_open_description() {
        let scratch = crate::temp_dir::TempDir::new("admin-enroll-lock").unwrap();
        let lease = enrollment_lease(scratch.path()).unwrap();
        let inherited = lease.0.try_clone().unwrap();
        assert!(enrollment_lease(scratch.path()).is_err());
        drop(lease);
        let next = enrollment_lease(scratch.path()).unwrap();
        drop(inherited);
        drop(next);
    }

    #[test]
    fn enrollment_validates_before_publication_and_never_replaces_a_key() {
        let scratch = crate::temp_dir::TempDir::new("admin-enroll").unwrap();
        let state = scratch.path();
        let destination = state.join("authorized_keys");
        assert!(publish_key(state, KEY, |_| Err("invalid key".into())).is_err());
        assert!(!destination.exists());
        publish_key(state, KEY, |candidate| {
            assert_eq!(read(candidate, 4096).unwrap(), KEY);
            Ok(())
        })
        .unwrap();
        assert_eq!(read(&destination, 4096).unwrap(), KEY);
        assert_eq!(fs::metadata(&destination).unwrap().mode() & 0o777, 0o644);
        assert!(publish_key(state, "replacement", |_| Ok(())).is_err());
        assert_eq!(read(&destination, 4096).unwrap(), KEY);
    }

    #[test]
    fn a_racing_key_publication_is_not_overwritten() {
        let scratch = crate::temp_dir::TempDir::new("admin-key-race").unwrap();
        let destination = scratch.path().join("authorized_keys");
        assert!(publish_key(scratch.path(), KEY, |_| {
            fs::write(&destination, "previous").unwrap();
            Ok(())
        })
        .is_err());
        assert_eq!(fs::read_to_string(destination).unwrap(), "previous");
    }

    #[test]
    fn enrollment_accepts_one_plain_public_key_and_discards_only_the_comment() {
        assert_eq!(
            key_line(&format!("{KEY} laptop\n")).unwrap(),
            format!("{KEY}\n")
        );
        for text in [
            format!("restrict {KEY}"),
            format!("{KEY}\n{KEY}"),
            "ssh-rsa AAAA".into(),
            "ssh-ed25519 AAAA".into(),
        ] {
            assert!(key_line(&text).is_err());
        }
    }

    #[test]
    fn duplicate_submissions_observe_the_job_and_cancellation_stops_at_installation() {
        let shared = Arc::new(Mutex::new(Job {
            active: true,
            id: 7,
            text: "job 7: building\nreboot true\n".into(),
            ..Job::default()
        }));
        let before = request(&shared, Request::Status(None)).unwrap();
        assert_eq!(
            request(&shared, Request::Update { reboot: false }).unwrap(),
            before
        );
        assert!(request(&shared, Request::Reboot).is_err());
        assert!(request(&shared, Request::Cancel).is_ok());
        assert!(cancelled(&shared).is_err());
        {
            let mut job = shared.lock().unwrap();
            job.cancel = false;
            job.installing = true;
        }
        assert!(request(&shared, Request::Cancel).is_err());
        assert!(cancelled(&shared).is_ok());
        assert_eq!(request(&shared, Request::Status(Some(7))).unwrap(), before);
    }

    #[test]
    fn gated_helper_fixture() {
        let Some(group) = std::env::var_os("TD_ADMIN_TEST_GROUP") else {
            return;
        };
        let group = Path::new(&group);
        if await_gate(std::io::stdin()).is_err() {
            return;
        }
        assert_eq!(
            fs::read_to_string(group.join("cgroup.procs")).unwrap(),
            format!("{}\n", std::process::id())
        );
        fs::write(group.join("started"), "yes").unwrap();
        if std::env::var_os("TD_ADMIN_TEST_STALL").is_some() {
            loop {
                std::thread::sleep(Duration::from_secs(1));
            }
        }
    }

    fn fixture_command(group: &Path) -> Command {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "admin::tests::gated_helper_fixture",
                "--nocapture",
            ])
            .env("TD_ADMIN_TEST_GROUP", group)
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command
    }

    #[test]
    fn helpers_require_placement_before_release_and_eof_refuses_execution() {
        let scratch = crate::temp_dir::TempDir::new("admin-start-gate").unwrap();
        let group = scratch.path();
        let mut eof = fixture_command(group).stdin(Stdio::null()).spawn().unwrap();
        assert!(eof.wait().unwrap().success());
        assert!(!group.join("started").exists());
        let mut child =
            spawn_contained(&mut fixture_command(group), group.to_str().unwrap()).unwrap();
        assert!(child.wait().unwrap().success());
        assert!(group.join("started").exists());
        fs::remove_file(group.join("started")).unwrap();
        fs::remove_file(group.join("cgroup.procs")).unwrap();
        fs::create_dir(group.join("cgroup.procs")).unwrap();
        fs::write(group.join("cgroup.events"), "populated 0\n").unwrap();
        assert!(spawn_contained(&mut fixture_command(group), group.to_str().unwrap()).is_err());
        assert!(!group.join("started").exists());
        assert_eq!(
            fs::read_to_string(group.join("cgroup.kill")).unwrap(),
            "1\n"
        );
    }

    #[test]
    fn cancelling_a_stalled_helper_reaps_it_before_returning() {
        let scratch = crate::temp_dir::TempDir::new("admin-cancel-helper").unwrap();
        let group = scratch.path();
        fs::write(group.join("cgroup.events"), "populated 0\n").unwrap();
        let mut command = fixture_command(group);
        command.env("TD_ADMIN_TEST_STALL", "1");
        let mut child = spawn_contained(&mut command, group.to_str().unwrap()).unwrap();
        let shared = Arc::new(Mutex::new(Job {
            cancel: true,
            ..Job::default()
        }));
        assert!(wait_helper(&mut child, group.to_str().unwrap(), Some(&shared)).is_err());
        assert!(child.try_wait().unwrap().is_some());
        assert_eq!(
            fs::read_to_string(group.join("cgroup.kill")).unwrap(),
            "1\n"
        );
    }

    #[test]
    fn interrupted_allocations_and_restarts_preserve_job_history() {
        let scratch = crate::temp_dir::TempDir::new("admin-restart").unwrap();
        let (first, _) = allocate_log(scratch.path(), 0).unwrap();
        let (next, _) = allocate_log(scratch.path(), 0).unwrap();
        assert_eq!((first, next), (1, 2));
        let recovered = recovered_status(2, "job 2: building\n");
        assert!(recovered.contains("not running after service restart"));
        assert!(recovered.contains("job 2: building"));
        assert_eq!(recovered_status(2, &recovered), recovered);
    }

    #[test]
    fn durable_records_replace_the_name_without_following_symlinks() {
        let scratch = crate::temp_dir::TempDir::new("admin-record").unwrap();
        let status = scratch.path().join("status");
        let unrelated = scratch.path().join("unrelated");
        fs::write(&unrelated, "preserve").unwrap();
        std::os::unix::fs::symlink(&unrelated, &status).unwrap();
        assert!(read(&status, 100).is_err());
        publish(&status, "queued\n").unwrap();
        publish(&status, "installed\n").unwrap();
        assert_eq!(read(&status, 100).unwrap(), "installed\n");
        assert_eq!(read(&unrelated, 100).unwrap(), "preserve");
        assert!(read(&status, 3).is_err());
        for id in ["0", "01", "../1", "-1", "18446744073709551616"] {
            assert!(parse(&["status", id]).is_err());
        }
    }
}

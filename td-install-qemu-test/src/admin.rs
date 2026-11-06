//! Guest-side SSH client and faulty loopback repository for qemu-admin-update.
//! This executable is staged only on the disposable oracle volume.
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

type Result<T> = std::result::Result<T, String>;
const FIXTURE: &str = "/run/td-volume/td/admin-fixture";
const WORK: &str = "/var/home/tester/.td-admin-oracle";
const ADDRESS: &str = "127.0.0.1:28180";

fn io<T>(result: std::io::Result<T>) -> Result<T> {
    result.map_err(|error| error.to_string())
}

struct Server {
    phase: Arc<AtomicUsize>,
    objects: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<Result<()>>>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn route(target: &str, phase: usize) -> Option<PathBuf> {
    let label = match phase {
        0 => "expired",
        1 | 2 => "valid",
        3 => "rotated",
        4 => "valid",
        5 => "bad",
        _ => return None,
    };
    if target == "/heads/system-x86-64" {
        return Some(Path::new(FIXTURE).join(format!("heads/{label}")));
    }
    if target == "/roots/2" && phase >= 3 && phase != 4 {
        return Some(Path::new(FIXTURE).join("repo/roots/2"));
    }
    let (kind, digest) = target.strip_prefix('/')?.split_once('/')?;
    if matches!(kind, "objects" | "releases")
        && digest.len() == 64
        && digest
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Some(Path::new(FIXTURE).join("repo").join(kind).join(digest));
    }
    None
}

fn response(stream: &mut TcpStream, phase: usize, objects: &AtomicUsize) -> Result<()> {
    io(stream.set_read_timeout(Some(Duration::from_secs(5))))?;
    io(stream.set_write_timeout(Some(Duration::from_secs(5))))?;
    let mut header = Vec::new();
    let mut byte = [0];
    while !header.ends_with(b"\r\n\r\n") {
        if header.len() == 8192 || io(stream.read(&mut byte))? == 0 {
            return Err("incomplete HTTP fixture request".into());
        }
        header.extend_from_slice(&byte);
    }
    let text = std::str::from_utf8(&header).map_err(|e| e.to_string())?;
    let mut words = text
        .lines()
        .next()
        .ok_or("HTTP request line")?
        .split_whitespace();
    if words.next() != Some("GET") {
        return Err("fixture only supports GET".into());
    }
    let target = words.next().ok_or("HTTP target")?;
    let Some(path) = route(target, phase) else {
        return io(stream.write_all(
            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        ));
    };
    let bytes = super::read(&path, 16 * 1024 * 1024)?;
    io(write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        bytes.len()
    ))?;
    let object = target.starts_with("/objects/");
    if object {
        objects.fetch_add(1, Ordering::SeqCst);
    }
    let truncate = object && phase == 1;
    let body = if truncate {
        bytes.get(..bytes.len() / 2).ok_or("truncation bound")?
    } else {
        &bytes
    };
    io(stream.write_all(body))
}

impl Server {
    fn start(phase: usize) -> Result<Self> {
        let listener = io(TcpListener::bind(ADDRESS))?;
        io(listener.set_nonblocking(true))?;
        let phase = Arc::new(AtomicUsize::new(phase));
        let objects = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let (p, o, s) = (phase.clone(), objects.clone(), stop.clone());
        let worker = thread::spawn(move || {
            while !s.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => response(&mut stream, p.load(Ordering::SeqCst), &o)?,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2))
                    }
                    Err(e) => return Err(e.to_string()),
                }
            }
            Ok(())
        });
        Ok(Self {
            phase,
            objects,
            stop,
            worker: Some(worker),
        })
    }

    fn select(&self, phase: usize) {
        self.phase.store(phase, Ordering::SeqCst);
        self.objects.store(0, Ordering::SeqCst);
    }
}

fn ssh(command: &str) -> Result<String> {
    let out = Path::new(WORK).join("stdout");
    let err = Path::new(WORK).join("stderr");
    let mut child = Command::new("/bin/ssh")
        .env_clear()
        .env("HOME", WORK)
        .args([
            "-F",
            "/dev/null",
            "-T",
            "-o",
            "BatchMode=yes",
            "-o",
            "IdentitiesOnly=yes",
            "-o",
            "StrictHostKeyChecking=accept-new",
            "-o",
            "GlobalKnownHostsFile=/dev/null",
            "-o",
            "UserKnownHostsFile=/var/home/tester/.td-admin-oracle/known_hosts",
            "-o",
            "ConnectTimeout=10",
            "-i",
            "/var/home/tester/.td-admin-oracle/key",
            "td-admin@127.0.0.1",
            command,
        ])
        .stdin(Stdio::null())
        .stdout(io(File::create(&out))?)
        .stderr(io(File::create(&err))?)
        .spawn()
        .map_err(|e| e.to_string())?;
    let deadline = Instant::now() + Duration::from_secs(60);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
            result => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("SSH fixture timeout/wait failure: {result:?}"));
            }
        }
    };
    let output = String::from_utf8(super::read(&out, 65536)?).map_err(|e| e.to_string())?;
    if !status.success() {
        return Err(format!(
            "SSH {command}: {status}: {}",
            String::from_utf8_lossy(&super::read(&err, 65536)?)
        ));
    }
    Ok(output)
}

fn job(verb: &str, succeeds: bool, previous: &mut u64) -> Result<String> {
    let submission_deadline = Instant::now() + Duration::from_secs(60);
    let reply = loop {
        let reply = ssh(verb)?;
        let id = reply
            .lines()
            .next()
            .and_then(|line| line.strip_prefix("job "))
            .and_then(|line| line.split_once(':'))
            .and_then(|(id, _)| id.parse::<u64>().ok())
            .ok_or("missing SSH job receipt")?;
        if id > *previous {
            *previous = id;
            break reply;
        }
        if Instant::now() >= submission_deadline {
            return Err("previous admin job remained active".into());
        }
        thread::sleep(Duration::from_millis(200));
    };
    let id = reply
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("job "))
        .and_then(|line| line.split_once(':'))
        .map(|(id, _)| id)
        .filter(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()))
        .ok_or("missing SSH job receipt")?;
    let end = Instant::now() + Duration::from_secs(600);
    loop {
        let status = ssh(&format!("status {id}"))?;
        let first = status.lines().next().unwrap_or_default();
        let failed = first == format!("job {id}: failed");
        let installed = first == format!("job {id}: installed");
        if failed || installed {
            if installed != succeeds {
                return Err(format!(
                    "unexpected job outcome: {status}\n{}",
                    ssh(&format!("logs {id}"))?
                ));
            }
            if failed && !status.contains("update-from-source --no-reboot") {
                return Err("failure omitted explicit source alternative".into());
            }
            return ssh(&format!("logs {id}"));
        }
        if Instant::now() >= end {
            return Err(format!("admin job timed out: {status}"));
        }
        thread::sleep(Duration::from_millis(200));
    }
}

fn selectors() -> Result<(PathBuf, PathBuf)> {
    Ok((
        io(fs::read_link("/run/td-volume/td/boot/current"))?,
        io(fs::read_link("/run/td-volume/td/boot/previous"))?,
    ))
}

pub(super) fn run(phase: &OsStr) -> Result<()> {
    if io(fs::metadata("/proc/self"))?.uid() != 1000 {
        return Err("admin oracle client must be the ordinary primary account".into());
    }
    io(fs::create_dir_all(WORK))?;
    io(fs::set_permissions(WORK, fs::Permissions::from_mode(0o700)))?;
    io(fs::copy(
        Path::new(FIXTURE).join("ssh-key"),
        Path::new(WORK).join("key"),
    ))?;
    io(fs::set_permissions(
        Path::new(WORK).join("key"),
        fs::Permissions::from_mode(0o600),
    ))?;
    if phase == "exercise" {
        let expired = String::from_utf8(super::read(
            &Path::new(FIXTURE).join("heads/expired"),
            16384,
        )?)
        .map_err(|e| e.to_string())?;
        let expiry = expired
            .lines()
            .nth(4)
            .ok_or("fixture expiry")?
            .parse::<u64>()
            .map_err(|e| e.to_string())?;
        let deadline = Instant::now() + Duration::from_secs(180);
        while SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| e.to_string())?
            .as_secs()
            <= expiry
        {
            if Instant::now() >= deadline {
                return Err("fixture expiry wait exceeded three minutes".into());
            }
            thread::sleep(Duration::from_millis(100));
        }
        let server = Server::start(0)?;
        let before = selectors()?;
        let mut previous = 0;
        let logs = job("update --no-reboot", false, &mut previous)?;
        if !logs.contains("release metadata is expired") {
            return Err(format!("expiry was not the refusal reason: {logs}"));
        }
        if selectors()? != before {
            return Err("expired release changed selectors".into());
        }
        println!("TD-ADMIN-EXPIRED-REFUSED");
        server.select(1);
        job("update --no-reboot", false, &mut previous)?;
        if server.objects.load(Ordering::SeqCst) == 0 || selectors()? != before {
            return Err("truncated transfer did not preserve selectors".into());
        }
        println!("TD-ADMIN-INTERRUPTED");
        server.select(2);
        let logs = job("update --no-reboot", true, &mut previous)?;
        if !logs.contains("reused output bytes")
            || logs.contains(", 0 reused output bytes")
            || selectors()?.0 == before.0
        {
            return Err(format!(
                "resume did not reuse verified objects and install: {logs}"
            ));
        }
        println!("TD-ADMIN-RESUMED-INSTALLED");
        server.select(3);
        job("update --no-reboot", true, &mut previous)?;
        println!("TD-ADMIN-ROTATED");
        let installed = selectors()?;
        server.select(4);
        let logs = job("update --no-reboot", false, &mut previous)?;
        if !logs.contains("root rollback") {
            return Err(format!("old root was not the refusal reason: {logs}"));
        }
        if selectors()? != installed {
            return Err("old-root release changed selectors".into());
        }
        println!("TD-ADMIN-ROOT-ROLLBACK-REFUSED");
        println!("TD-ADMIN-EXERCISE-OK");
    } else if phase == "bad" {
        let _server = Server::start(5)?;
        let reply = ssh("update")?;
        if !reply.contains("reboot true") {
            return Err("update did not default to reboot".into());
        }
        println!("TD-ADMIN-DEFAULT-REBOOT-SUBMITTED");
        loop {
            thread::sleep(Duration::from_secs(1));
        }
    } else if phase == "reboot" {
        ssh("reboot")?;
    } else {
        return Err("unknown admin fixture phase".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn repository_routes_are_fixed_or_digest_named() {
        for target in [
            "/../ssh-key",
            "/objects/../ssh-key",
            "/roots/1",
            "/roots/3",
            "/heads/server-x86-64",
        ] {
            assert!(route(target, 2).is_none());
        }
        assert!(route("/roots/2", 2).is_none());
        assert!(route("/roots/2", 3).is_some());
        assert!(route(&format!("/objects/{}", "a".repeat(64)), 2).is_some());
    }
}

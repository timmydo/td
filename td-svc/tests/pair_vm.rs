//! Standalone static VM fixture and host runner. --run-vm builds a temporary
//! initramfs and boots it with explicit kernel, supervisor and busybox inputs.
//! Two scenarios: `pair` (the default) and `leaf`, a `stop=leaf` unit.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::fs;
use std::io::{Read, Write};
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn park() -> ! {
    loop {
        std::thread::sleep(Duration::from_secs(1));
    }
}

/// Live and not a zombie: nothing in the guest reaps reparented processes.
fn alive(pid: u32) -> bool {
    match fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => {
            let (_, fields) = stat.rsplit_once(") ").unwrap();
            !matches!(fields.split_whitespace().next(), Some("Z" | "X"))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => panic!("liveness of {pid} unknown: {e}"),
    }
}

/// `/proc/PID/stat` field `index` after the command, counted from state = 0.
fn stat_field(pid: &str, index: usize) -> String {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    let (_, fields) = stat.rsplit_once(") ").unwrap();
    fields.split_whitespace().nth(index).unwrap().to_string()
}

fn first() {
    let mut socket = UnixStream::from(std::io::stdin().as_fd().try_clone_to_owned().unwrap());
    socket
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    if let Ok(text) = fs::read_to_string("/run/old-grandchild") {
        let pid: u32 = text.parse().unwrap();
        let live = alive(pid);
        fs::write(
            "/run/pair-result",
            if live {
                "FAIL: old grandchild survived restart"
            } else {
                "PASS: old grandchild gone before next generation"
            },
        )
        .unwrap();
        park();
    }
    // Deliberately retain fd 0 in a grandchild: a direct-child-only kill leaks it.
    let mut child = Command::new("/pair-probe")
        .arg("descendant")
        .stdin(Stdio::inherit())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    fs::write("/run/old-grandchild", child.id().to_string()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while fs::read_to_string("/run/grandchild-running")
        .ok()
        .as_deref()
        != Some(&child.id().to_string())
    {
        assert!(
            child.try_wait().unwrap().is_none(),
            "grandchild exited before the probe"
        );
        assert!(Instant::now() < deadline, "grandchild did not start");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        fs::read_link("/proc/self/fd/0").unwrap(),
        fs::read_link(format!("/proc/{}/fd/0", child.id())).unwrap()
    );
    socket.write_all(b"first").unwrap();
    let mut reply = [0; 6];
    socket.read_exact(&mut reply).unwrap();
    assert_eq!(&reply, b"second");
    socket.write_all(b"ack").unwrap();
    let _ = child.wait();
    park();
}

fn second() {
    let mut socket = UnixStream::from(std::io::stdin().as_fd().try_clone_to_owned().unwrap());
    socket
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut request = [0; 5];
    socket.read_exact(&mut request).unwrap();
    assert_eq!(&request, b"first");
    socket.write_all(b"second").unwrap();
    let mut ack = [0; 3];
    socket.read_exact(&mut ack).unwrap();
    assert_eq!(&ack, b"ack");
    // A clean peer exit is still a paired-service failure.
}

fn leaf_result(text: &str) -> ! {
    fs::write("/run/leaf-result", text).unwrap();
    park();
}

/// One leader generation of the `stop=leaf` unit. The first starts a session
/// child at once, before anything else, as a daemon that forks before td-svc
/// could place it would; the child leaves the leader's process group and
/// session as an OpenSSH session does, and the leader then crashes. The
/// start gate must have put both in the leaf. The second, restarted in place,
/// must find that session alive in its own leaf and asks for a restart. The
/// third, started only once the leaf is empty, must find it gone.
fn leader() {
    let first = !Path::new("/run/leaf-session").exists();
    let early = first.then(|| {
        Command::new("/bin/busybox")
            .args(["setsid", "/pair-probe", "session"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    });
    // The start gate (DESIGN.md I7): a `stop=leaf` leader runs only once
    // td-svc has placed it, so it is in its leaf from its first instruction.
    let own = fs::read_to_string("/proc/self/cgroup").unwrap();
    if own.trim() != "0::/system/leafd" {
        leaf_result("FAIL: the leader ran before it was placed in its leaf");
    }
    let Some(mut child) = early else {
        let pid: u32 = fs::read_to_string("/run/leaf-session")
            .unwrap()
            .parse()
            .unwrap();
        if Path::new("/run/leaf-crashed").exists() {
            if alive(pid) {
                leaf_result("FAIL: the session survived a requested restart");
            }
            leaf_result("PASS: the session survived a crash and ended before the next generation");
        }
        if !alive(pid) {
            leaf_result("FAIL: a leader crash ended the session");
        }
        if fs::read_to_string(format!("/proc/{pid}/cgroup")).unwrap() != own {
            leaf_result("FAIL: the restarted leader is not in the session's leaf");
        }
        fs::write("/run/leaf-crashed", "").unwrap();
        fs::write("/run/restart-wanted", "").unwrap();
        park();
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    let pid = loop {
        if let Ok(pid) = fs::read_to_string("/run/session-running") {
            break pid;
        }
        assert!(
            child.try_wait().unwrap().is_none(),
            "session exited before it started"
        );
        assert!(Instant::now() < deadline, "session did not start");
        std::thread::sleep(Duration::from_millis(10));
    };
    let mine = std::process::id().to_string();
    // Fields 2 and 3 after the state: process group and session.
    if stat_field(&pid, 2) == stat_field(&mine, 2) || stat_field(&pid, 3) == stat_field(&mine, 3) {
        leaf_result("FAIL: the session child did not leave the leader's group");
    }
    if fs::read_to_string(format!("/proc/{pid}/cgroup")).unwrap() != own {
        leaf_result("FAIL: the early session started outside the leaf");
    }
    fs::write("/run/leaf-session", pid).unwrap();
    // A crash, not a stop: the restart policy brings the next leader back.
    std::process::exit(1);
}

fn mount_pseudo_filesystems() {
    for path in ["/proc", "/sys", "/run", "/etc"] {
        fs::create_dir_all(path).unwrap();
    }
    for (kind, destination) in [("proc", "/proc"), ("sysfs", "/sys")] {
        assert!(Command::new("/bin/busybox")
            .args(["mount", "-t", kind, kind, destination])
            .status()
            .unwrap()
            .success());
    }
    fs::create_dir_all("/sys/fs/cgroup").unwrap();
    assert!(Command::new("/bin/busybox")
        .args(["mount", "-t", "cgroup2", "none", "/sys/fs/cgroup"])
        .status()
        .unwrap()
        .success());
}

/// Supervise the `stop=leaf` unit, and restart it once its second leader
/// asks: the requested restart is what must end the session.
fn init_leaf() -> ! {
    fs::write(
        "/etc/leaf.conf",
        "[leafd]\ntype=daemon\nexec=/pair-probe leader\nrestart=always\nstop=leaf\nstop-timeout=1\n",
    )
    .unwrap();
    let mut supervisor = Command::new("/bin/td-svc")
        .args(["run", "-f", "/etc/leaf.conf"])
        .stdin(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut restarted = false;
    loop {
        if let Ok(result) = fs::read_to_string("/run/leaf-result") {
            println!("TD-PAIR-VM: {result}");
            park();
        }
        if !restarted && Path::new("/run/restart-wanted").exists() {
            restarted = true;
            assert!(Command::new("/bin/td-svc")
                .args(["restart", "leafd"])
                .status()
                .unwrap()
                .success());
        }
        if supervisor.try_wait().unwrap().is_some() || Instant::now() >= deadline {
            println!("TD-PAIR-VM: FAIL: supervisor exited or the leaf scenario timed out");
            park();
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn init() {
    assert_eq!(std::process::id(), 1, "VM init only");
    mount_pseudo_filesystems();
    if fs::read_to_string("/td-vm-scenario").unwrap() == "leaf" {
        init_leaf();
    }
    fs::write("/etc/pair.conf", "[paired]\ntype=daemon\nexec=/pair-probe first\npair-exec=/pair-probe second\nready=/pair-probe ready\nrestart=on-failure\nstop-timeout=1\n").unwrap();
    let mut supervisor = Command::new("/bin/td-svc")
        .args(["run", "-f", "/etc/pair.conf"])
        .stdin(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Ok(result) = fs::read_to_string("/run/pair-result") {
            println!("TD-PAIR-VM: {result}");
            if result.starts_with("PASS:") {
                assert!(Command::new("/bin/td-svc")
                    .args(["stop", "paired"])
                    .status()
                    .unwrap()
                    .success());
            }
            park();
        }
        if supervisor.try_wait().unwrap().is_some() || Instant::now() >= deadline {
            println!("TD-PAIR-VM: FAIL: supervisor exited or restart timed out");
            park();
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn main() {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    if arguments.first().map(String::as_str) == Some("--run-vm") {
        match run_vm(&arguments) {
            Ok(()) => return,
            Err(why) => {
                eprintln!("td-pair-vm: {why}");
                std::process::exit(1);
            }
        }
    }
    assert!(
        std::path::Path::new("/td-pair-test-vm").is_file(),
        "VM fixture only"
    );
    match std::env::args().nth(1).as_deref() {
        Some("first") => first(),
        Some("second") => second(),
        Some("descendant") => {
            fs::write("/run/grandchild-running", std::process::id().to_string()).unwrap();
            park();
        }
        Some("ready") => (),
        Some("leader") => leader(),
        Some("session") => {
            fs::write("/run/session-running", std::process::id().to_string()).unwrap();
            park();
        }
        None => init(),
        Some(other) => panic!("unknown VM fixture mode: {other}"),
    }
}

struct Scratch(PathBuf);
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct Guest(std::process::Child);
impl Drop for Guest {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn entry(
    archive: &mut Vec<u8>,
    inode: u32,
    name: &str,
    mode: u32,
    data: &[u8],
    major: u32,
    minor: u32,
) {
    archive.extend_from_slice(b"070701");
    for value in [
        inode as u64,
        mode as u64,
        0,
        0,
        1,
        1,
        data.len() as u64,
        0,
        0,
        major as u64,
        minor as u64,
        name.len() as u64 + 1,
        0,
    ] {
        archive.extend_from_slice(format!("{value:08x}").as_bytes());
    }
    archive.extend_from_slice(name.as_bytes());
    archive.push(0);
    archive.resize((archive.len() + 3) & !3, 0);
    archive.extend_from_slice(data);
    archive.resize((archive.len() + 3) & !3, 0);
}

fn artifact(path: &Path) -> std::io::Result<Vec<u8>> {
    let file = fs::File::open(path)?;
    let mut bytes = Vec::new();
    file.take(128 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > 128 * 1024 * 1024 {
        return Err(std::io::Error::other("VM artifact exceeds 128 MiB"));
    }
    Ok(bytes)
}

fn run_vm(arguments: &[String]) -> std::io::Result<()> {
    let (kernel, supervisor, busybox, log_path, scenario) = match arguments {
        [_verb, kernel, supervisor, busybox, log_path] => {
            (kernel, supervisor, busybox, log_path, "pair")
        }
        [_verb, kernel, supervisor, busybox, log_path, scenario]
            if scenario == "pair" || scenario == "leaf" =>
        {
            (kernel, supervisor, busybox, log_path, scenario.as_str())
        }
        _ => {
            return Err(std::io::Error::other(
                "usage: pair-vm --run-vm KERNEL TD-SVC BUSYBOX NEW-LOG [pair|leaf]",
            ))
        }
    };
    let passed = match scenario {
        "leaf" => {
            "TD-PAIR-VM: PASS: the session survived a crash and ended before the next generation"
        }
        _ => "TD-PAIR-VM: PASS: old grandchild gone before next generation",
    };
    // TCG unless the caller asks for KVM; nothing falls back between them.
    let accel = match std::env::var("TD_QEMU_ACCEL").as_deref() {
        Ok("kvm") => "kvm",
        Ok("tcg") | Err(std::env::VarError::NotPresent) => "tcg",
        _ => return Err(std::io::Error::other("TD_QEMU_ACCEL must be kvm or tcg")),
    };
    for input in [kernel, supervisor, busybox, log_path] {
        if !Path::new(input).is_absolute() {
            return Err(std::io::Error::other(
                "VM artifact and log paths must be absolute",
            ));
        }
    }
    // Refuse overwriting a prior proof or an arbitrary caller file.
    let mut saved = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(log_path)?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(std::io::Error::other)?
        .as_nanos();
    let scratch =
        Scratch(std::env::temp_dir().join(format!("td-pair-vm-{}-{stamp}", std::process::id())));
    fs::create_dir(&scratch.0)?;
    let mut archive = Vec::new();
    let mut inode = 1;
    for name in ["bin", "dev", "proc", "sys", "run", "etc", "tmp"] {
        entry(&mut archive, inode, name, 0o40755, &[], 0, 0);
        inode += 1;
    }
    for (name, mode, data, major, minor) in [
        ("dev/console", 0o20600, Vec::new(), 5, 1),
        ("dev/null", 0o20666, Vec::new(), 1, 3),
        ("td-pair-test-vm", 0o100600, Vec::new(), 0, 0),
        (
            "td-vm-scenario",
            0o100600,
            scenario.as_bytes().to_vec(),
            0,
            0,
        ),
        (
            "pair-probe",
            0o100755,
            artifact(&std::env::current_exe()?)?,
            0,
            0,
        ),
        ("init", 0o120777, b"/pair-probe".to_vec(), 0, 0),
        (
            "bin/td-svc",
            0o100755,
            artifact(Path::new(supervisor))?,
            0,
            0,
        ),
        ("bin/busybox", 0o100755, artifact(Path::new(busybox))?, 0, 0),
        ("TRAILER!!!", 0, Vec::new(), 0, 0),
    ] {
        entry(&mut archive, inode, name, mode, &data, major, minor);
        inode += 1;
    }
    let initramfs = scratch.0.join("initramfs.cpio");
    fs::write(&initramfs, archive)?;
    let output_path = scratch.0.join("serial");
    let output = fs::File::create(&output_path)?;
    let mut guest = Guest(
        Command::new("qemu-system-x86_64")
            .args([
                "-machine",
                "q35",
                "-accel",
                accel,
                "-m",
                "512",
                "-smp",
                "2",
                "-nodefaults",
                "-display",
                "none",
                "-serial",
                "stdio",
                "-monitor",
                "none",
                "-nic",
                "none",
                "-no-reboot",
                "-kernel",
                kernel,
                "-initrd",
            ])
            .arg(&initramfs)
            .args(["-append", "console=ttyS0 rdinit=/init panic=-1"])
            .stdin(Stdio::null())
            .stdout(output.try_clone()?)
            .stderr(output)
            .spawn()?,
    );
    let mut serial = fs::File::open(output_path)?;
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut report = Vec::new();
    let mut bytes = [0u8; 8192];
    let verdict = loop {
        let count = serial.read(&mut bytes)?;
        let chunk = bytes
            .get(..count)
            .ok_or_else(|| std::io::Error::other("invalid serial count"))?;
        saved.write_all(chunk)?;
        report.extend_from_slice(chunk);
        let text = String::from_utf8_lossy(&report);
        if text.contains(passed) {
            break Ok(());
        }
        let complete = text.get(..text.rfind('\n').unwrap_or(0)).unwrap_or("");
        if complete
            .lines()
            .any(|line| line.starts_with("TD-PAIR-VM: FAIL:"))
        {
            break Err(std::io::Error::other("VM descendant cleanup failed"));
        }
        if report.len() > 2 * 1024 * 1024 || Instant::now() >= deadline {
            break Err(std::io::Error::other("VM output or time limit exceeded"));
        }
        if count == 0 {
            if guest.0.try_wait()?.is_some() {
                break Err(std::io::Error::other("VM exited before its proof marker"));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    };
    drop(guest);
    saved.sync_all()?;
    verdict?;
    println!("TD-PAIR-VM: PASS; serial proof saved to {log_path}");
    Ok(())
}

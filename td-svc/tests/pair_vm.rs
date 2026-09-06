//! Standalone static VM fixture and host runner. --run-vm builds a temporary
//! initramfs and boots it with explicit kernel, supervisor and busybox inputs.
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

fn first() {
    let mut socket = UnixStream::from(std::io::stdin().as_fd().try_clone_to_owned().unwrap());
    socket
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    if let Ok(text) = fs::read_to_string("/run/old-grandchild") {
        let pid: u32 = text.parse().unwrap();
        let live = match fs::read_to_string(format!("/proc/{pid}/stat")) {
            Ok(stat) => {
                let (_, fields) = stat.rsplit_once(") ").unwrap();
                !matches!(fields.split_whitespace().next(), Some("Z" | "X"))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
            Err(e) => panic!("grandchild liveness unknown: {e}"),
        };
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

fn init() {
    assert_eq!(std::process::id(), 1, "VM init only");
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
    let [_verb, kernel, supervisor, busybox, log_path] = arguments else {
        return Err(std::io::Error::other(
            "usage: pair-vm --run-vm KERNEL TD-SVC BUSYBOX NEW-LOG",
        ));
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
                "tcg",
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
        if text.contains("TD-PAIR-VM: PASS: old grandchild gone before next generation") {
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

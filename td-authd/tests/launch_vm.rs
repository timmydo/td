#![deny(unsafe_code)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
//! Standalone host VM fixture; compile with host rustc for an installed static
//! target. --run-vm takes kernel, authd, firstboot, login, busybox and a new log.
//! Fixture code is never a target recipe input or shipped distribution artifact.
#[path = "../src/channel.rs"]
mod channel;
#[path = "../src/sys.rs"]
mod sys;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

fn park() -> ! {
    loop {
        std::thread::sleep(Duration::from_secs(1));
    }
}

fn terminal() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    assert_eq!(args.len(), 5);
    assert_eq!(
        &args[..4],
        [
            "run",
            "--socket",
            "/run/td-compositor/1000/wayland-0",
            "--ready-socket"
        ]
    );
    assert!(args[4].starts_with("/run/user/1000/td-auth-terminal-"));
    assert!(args[4].ends_with("-1.ready"));
    assert_eq!(
        fs::read_to_string("/proc/self/cgroup").unwrap(),
        "0::/td-user-1000/session\n"
    );
    let stat = fs::read_to_string("/proc/self/stat").unwrap();
    let (_, fields) = stat.rsplit_once(") ").unwrap();
    assert_eq!(
        fields.split_whitespace().nth(2).unwrap(),
        std::process::id().to_string()
    );
    assert_eq!(fields.split_whitespace().nth(4).unwrap(), "0");
    let status = fs::read_to_string("/proc/self/status").unwrap();
    for key in ["Uid:", "Gid:"] {
        assert_eq!(
            status
                .lines()
                .find_map(|l| l.strip_prefix(key))
                .unwrap()
                .split_whitespace()
                .collect::<Vec<_>>(),
            ["1000"; 4]
        );
    }
    for key in ["CapPrm:", "CapEff:", "CapAmb:"] {
        assert_eq!(
            status
                .lines()
                .find_map(|l| l.strip_prefix(key))
                .unwrap()
                .trim(),
            "0000000000000000"
        );
    }
    for fd in [0, 1, 2] {
        let metadata = fs::metadata(format!("/proc/self/fd/{fd}")).unwrap();
        assert!(metadata.file_type().is_char_device());
        assert_eq!(metadata.rdev(), 0x103);
    }
    let mut fds: Vec<u32> = fs::read_dir("/proc/self/fd")
        .unwrap()
        .map(|e| e.unwrap().file_name().to_str().unwrap().parse().unwrap())
        .collect();
    fds.sort();
    assert_eq!(fds, [0, 1, 2, 3]);
    let mut keys: Vec<_> = std::env::vars().map(|(k, _)| k).collect();
    keys.sort();
    assert_eq!(
        keys,
        [
            "HOME",
            "LOGNAME",
            "PATH",
            "SHELL",
            "TD_CONTROL_SOCKET",
            "USER"
        ]
    );
    assert_eq!(
        std::env::var("TD_CONTROL_SOCKET").unwrap(),
        "/run/td-compositor/1000/td-control"
    );
    fs::write(
        "/run/user/1000/terminal-evidence",
        "uid 1000; null stdio; no authority descriptor\n",
    )
    .unwrap();
}

fn peer(denied: bool, placement_denied: bool) {
    let connection = channel::Channel::from_stdin(0);
    if denied {
        if let Ok(mut connection) = connection {
            if let Ok(version) = connection.receive() {
                assert_eq!(version, b"TDLA001\n");
                let _ = connection.send(b"TDLA001\n");
                assert!(connection.receive().is_err());
            }
        }
        return;
    }
    let mut connection = connection.unwrap();
    assert_eq!(connection.receive().unwrap(), b"TDLA001\n");
    connection.send(b"TDLA001\n").unwrap();
    assert_eq!(connection.receive().unwrap(), [0x80]);
    connection.send(&[3]).unwrap();
    assert_eq!(connection.receive().unwrap(), [0x83]);
    connection.send(&[1]).unwrap();
    let started = connection.receive().unwrap();
    assert_eq!(started, [0x81, 0, 0, 0, 0, 0, 0, 0, 1]);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        connection.send(&[2, 0, 0, 0, 0, 0, 0, 0, 1]).unwrap();
        let status = connection.receive().unwrap();
        if status == [0x82, if placement_denied { 2 } else { 1 }] {
            break;
        }
        assert_eq!(status, [0x82, 0], "terminal failed: {status:?}");
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    // Refusal can kill the peer before send's final liveness check.
    let _ = connection.send(&[2, 0, 0, 0, 0, 0, 0, 0, 1]);
    assert!(connection.receive().is_err());
}

fn wait(child: &mut Child) -> ExitStatus {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            panic!("child wait timed out");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn attempt(denied: bool, wrong_sender: bool, placement_denied: bool) {
    let (first, second) = UnixStream::pair().unwrap();
    let mut authority = Command::new("/bin/td-authd")
        .args([
            "terminal-serve",
            "--user",
            "tester",
            "--uid",
            "1000",
            "--peer-uid",
            "993",
        ])
        .stdin(Stdio::from(std::os::fd::OwnedFd::from(first)))
        .spawn()
        .unwrap();
    let mut client = Command::new("/bin/td-login")
        .args([
            if wrong_sender {
                "exec-as"
            } else {
                "exec-service-as"
            },
            if wrong_sender { "tester" } else { "tdc1000" },
            "--",
            "/pair-probe",
            if denied {
                "--peer-denied"
            } else if placement_denied {
                "--peer-placement-denied"
            } else {
                "--peer"
            },
        ])
        .stdin(Stdio::from(std::os::fd::OwnedFd::from(second)))
        .spawn()
        .unwrap();
    assert!(
        wait(&mut client).success(),
        "terminal diagnostic: {:?}",
        fs::read_to_string("/run/user/1000/terminal-failure")
    );
    assert!(!wait(&mut authority).success());
}

fn init() {
    assert!(Command::new("/bin/busybox")
        .args(["mount", "-t", "proc", "proc", "/proc"])
        .status()
        .unwrap()
        .success());

    fs::create_dir_all("/sys/fs/cgroup").unwrap();
    assert!(Command::new("/bin/busybox")
        .args(["mount", "-t", "cgroup2", "none", "/sys/fs/cgroup"])
        .status()
        .unwrap()
        .success());
    fs::create_dir_all("/sys/fs/cgroup/td-user-1000/session").unwrap();
    for path in ["/var/lib/td", "/run/user/1000", "/home/tester"] {
        fs::create_dir_all(path).unwrap();
    }
    for path in ["/run/user/1000", "/home/tester"] {
        std::os::unix::fs::chown(path, Some(1000), Some(1000)).unwrap();
    }
    let table = "td-principals-v1\nsession\t1000\t993\t992\t991\napplication\t1000\tmail\t65537\n";
    for (path,text,mode) in [
        ("/etc/td-principals.tsv",table,0o444),
        ("/var/lib/td/principals.tsv",table,0o600),
        ("/etc/passwd","root:x:0:0:root:/root:/bin/false\ntester:x:1000:1000:Test:/home/tester:/bin/false\ntdc1000:x:993:993:Compositor:/run:/bin/false\n",0o644),
        ("/etc/group","root:x:0:\ntester:x:1000:\ntdc1000:x:993:\n",0o644),
        ("/etc/shadow","root::1:0:99999:7:::\ntester::1:0:99999:7:::\ntdc1000:!td-service:1:0:99999:7:::\n",0o600),
    ] { fs::write(path,text).unwrap();fs::set_permissions(path,fs::Permissions::from_mode(mode)).unwrap(); }
    assert!(Command::new("/bin/td-firstboot")
        .args(["check-launch-session", "tester", "1000", "993"])
        .status()
        .unwrap()
        .success());
    attempt(false, false, false);
    assert_eq!(
        fs::read_to_string("/run/user/1000/terminal-evidence").unwrap(),
        "uid 1000; null stdio; no authority descriptor\n"
    );
    fs::remove_file("/run/user/1000/terminal-evidence").unwrap();
    attempt(true, true, false);
    assert!(!Path::new("/run/user/1000/terminal-evidence").exists());
    fs::remove_dir("/sys/fs/cgroup/td-user-1000/session").unwrap();
    attempt(false, false, true);
    assert!(!Path::new("/run/user/1000/terminal-evidence").exists());
    fs::remove_file("/var/lib/td/principals.tsv").unwrap();
    attempt(true, false, false);
    assert!(!Path::new("/run/user/1000/terminal-evidence").exists());
    assert!(!Path::new("/var/lib/td/principals.tsv").exists());
    fs::remove_dir("/var/lib/td").unwrap();
    attempt(true, false, false);
    assert!(!Path::new("/var/lib/td").exists());
    assert!(!Path::new("/run/user/1000/terminal-evidence").exists());
    println!("TD-TERMINAL-VM: PASS");
    park();
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).is_some_and(|a| a == "--run-vm") {
        run_vm(&args[1..]).unwrap();
        return;
    }
    assert!(
        Path::new("/td-terminal-test-vm").is_file(),
        "VM fixture only"
    );
    std::panic::set_hook(Box::new(|info| {
        eprintln!("TD-TERMINAL-VM: FAIL: {info}");
        let _ = fs::write("/run/user/1000/terminal-failure", info.to_string());
    }));
    if args.first().is_some_and(|a| {
        Path::new(a)
            .file_name()
            .is_some_and(|name| name == "td-term")
    }) {
        terminal();
        return;
    }
    match args.get(1).map(String::as_str) {
        Some("--peer") => peer(false, false),
        Some("--peer-denied") => peer(true, false),
        Some("--peer-placement-denied") => peer(false, true),
        None if std::process::id() == 1 => init(),
        _ => panic!("VM fixture only"),
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
    let [_verb, kernel, authority, firstboot, login, busybox, log_path] = arguments else {
        return Err(std::io::Error::other(
            "usage: terminal-vm --run-vm KERNEL AUTHD FIRSTBOOT LOGIN BUSYBOX NEW-LOG",
        ));
    };
    for input in [kernel, authority, firstboot, login, busybox, log_path] {
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
    let scratch = Scratch(
        std::env::temp_dir().join(format!("td-terminal-vm-{}-{stamp}", std::process::id())),
    );
    fs::create_dir(&scratch.0)?;
    let mut archive = Vec::new();
    let mut inode = 1;
    for name in [".", "bin", "dev", "proc", "sys", "run", "etc", "tmp"] {
        entry(&mut archive, inode, name, 0o40755, &[], 0, 0);
        inode += 1;
    }
    for (name, mode, data, major, minor) in [
        ("dev/console", 0o20600, Vec::new(), 5, 1),
        ("dev/null", 0o20666, Vec::new(), 1, 3),
        ("dev/urandom", 0o20666, Vec::new(), 1, 9),
        ("td-terminal-test-vm", 0o100600, Vec::new(), 0, 0),
        (
            "pair-probe",
            0o100755,
            artifact(&std::env::current_exe()?)?,
            0,
            0,
        ),
        ("init", 0o120777, b"/pair-probe".to_vec(), 0, 0),
        (
            "bin/td-authd",
            0o100755,
            artifact(Path::new(authority))?,
            0,
            0,
        ),
        ("bin/busybox", 0o100755, artifact(Path::new(busybox))?, 0, 0),
        (
            "bin/td-firstboot",
            0o100755,
            artifact(Path::new(firstboot))?,
            0,
            0,
        ),
        ("bin/td-login", 0o100755, artifact(Path::new(login))?, 0, 0),
        ("bin/td-term", 0o120777, b"/pair-probe".to_vec(), 0, 0),
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
        match proof_verdict(&text) {
            Some(true) => break Ok(()),
            Some(false) => break Err(std::io::Error::other("VM terminal authority check failed")),
            None => {}
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
    println!("TD-TERMINAL-VM: PASS; serial proof saved to {log_path}");
    Ok(())
}

fn proof_verdict(text: &str) -> Option<bool> {
    let complete = text
        .rfind('\n')
        .and_then(|end| text.get(..=end))
        .unwrap_or("");
    if complete
        .lines()
        .any(|line| line.starts_with("TD-TERMINAL-VM: FAIL:"))
    {
        Some(false)
    } else if complete.lines().any(|line| line == "TD-TERMINAL-VM: PASS") {
        Some(true)
    } else {
        None
    }
}

#[test]
fn streamed_proof_requires_the_complete_exact_verdict_line() {
    for ending in ["\n", "\r\n"] {
        let good = format!("booting\nTD-TERMINAL-VM: PASS{ending}");
        for end in 0..good.len() {
            assert_eq!(proof_verdict(&good[..end]), None, "prefix {end}");
        }
        assert_eq!(proof_verdict(&good), Some(true));
        let bad = format!("booting\nTD-TERMINAL-VM: PASS-BROKEN{ending}");
        for end in 0..=bad.len() {
            assert_eq!(proof_verdict(&bad[..end]), None, "false prefix {end}");
        }
        assert_eq!(
            proof_verdict(&format!("{good}TD-TERMINAL-VM: FAIL: bad{ending}")),
            Some(false)
        );
    }
}

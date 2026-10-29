//! Live namespace qualification uses only this td-owned executable.
#![forbid(unsafe_code)]
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn call(home: &Path, jail: &Path, args: &[&str]) -> io::Result<Output> {
    Command::new(env!("CARGO_BIN_EXE_td-sandbox"))
        .arg("--jail")
        .arg(jail)
        .args(args)
        .env("HOME", home)
        .output()
}
fn passed(output: Output) -> io::Result<String> {
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[test]
#[ignore = "requires built TD_SANDBOX_TEST_JAIL, user namespaces, TMPDIR under worktree scratch"]
fn private_home_shares_and_executable_replacement() -> io::Result<()> {
    let root = td_fs::TempDir::new("sandbox-live")?;
    let jail = PathBuf::from(
        std::env::var_os("TD_SANDBOX_TEST_JAIL")
            .ok_or_else(|| io::Error::other("TD_SANDBOX_TEST_JAIL missing"))?,
    );
    passed(call(&root, &jail, &["create", "test"])?)?;
    let home = root.join(".local/share/td-sandbox/test/home");
    let probe = home.join("probe");
    fs::copy(std::env::current_exe()?, &probe)?;
    let read = root.join("read");
    let write = root.join("write");
    fs::create_dir(&read)?;
    fs::create_dir(&write)?;
    fs::write(read.join("original"), "protected")?;
    fs::write(
        home.join("paths"),
        format!(
            "{}\n{}\n{}",
            read.display(),
            write.display(),
            root.join("secret").display()
        ),
    )?;
    fs::write(root.join("secret"), "ungranted")?;
    let path = |p: &Path| {
        p.to_str()
            .map(str::to_owned)
            .ok_or_else(|| io::Error::other("fixture path"))
    };
    passed(call(
        &root,
        &jail,
        &["share", "test", &path(&read)?, "--read"],
    )?)?;
    passed(call(
        &root,
        &jail,
        &["share", "test", &path(&write)?, "--write"],
    )?)?;
    let run = [
        "run",
        "test",
        "--",
        &path(&probe)?,
        "--exact",
        "inside_fixture",
        "--ignored",
        "--nocapture",
    ];
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    fs::write(
        home.join("network"),
        format!("{}\noff", listener.local_addr()?),
    )?;
    let output = passed(call(&root, &jail, &run)?)?;
    assert!(output.contains("private-home-probe-ok"), "{output}");
    passed(call(&root, &jail, &["network", "test", "host"])?)?;
    fs::write(
        home.join("network"),
        format!("{}\nhost", listener.local_addr()?),
    )?;
    let output = passed(call(&root, &jail, &run)?)?;
    assert!(output.contains("private-home-probe-ok"), "{output}");
    assert_eq!(fs::read_to_string(write.join("changed"))?, "written");
    assert_eq!(fs::read_to_string(read.join("original"))?, "protected");
    fs::remove_dir_all(&read)?;
    passed(call(&root, &jail, &["unshare", "test", &path(&read)?])?)?;
    let inspection = passed(call(&root, &jail, &["inspect", "test"])?)?;
    assert!(!inspection.contains(&path(&read)?));
    Ok(())
}

#[test]
#[ignore = "executed only inside the live sandbox"]
fn inside_fixture() -> io::Result<()> {
    let home =
        PathBuf::from(std::env::var_os("HOME").ok_or_else(|| io::Error::other("HOME missing"))?);
    let paths = fs::read_to_string(home.join("paths"))?;
    let mut paths = paths.lines();
    let read = PathBuf::from(
        paths
            .next()
            .ok_or_else(|| io::Error::other("read missing"))?,
    );
    let write = PathBuf::from(
        paths
            .next()
            .ok_or_else(|| io::Error::other("write missing"))?,
    );
    let secret = PathBuf::from(
        paths
            .next()
            .ok_or_else(|| io::Error::other("secret missing"))?,
    );
    assert!(!secret.exists());
    let network = fs::read_to_string(home.join("network"))?;
    let (address, mode) = network
        .split_once('\n')
        .ok_or_else(|| io::Error::other("network fixture missing"))?;
    let address = address.parse().map_err(io::Error::other)?;
    assert_eq!(
        std::net::TcpStream::connect_timeout(&address, std::time::Duration::from_millis(250))
            .is_ok(),
        mode == "host"
    );
    assert!(fs::write(read.join("original"), "changed").is_err());
    fs::write(write.join("changed"), "written")?;
    let next = home.join("next-version");
    fs::copy(std::env::current_exe()?, &next)?;
    let status = Command::new(next)
        .args(["--exact", "restarted_fixture", "--ignored", "--nocapture"])
        .status()?;
    assert!(status.success());
    println!("private-home-probe-ok");
    Ok(())
}

#[test]
#[ignore = "executed only as the private-home replacement executable"]
fn restarted_fixture() -> io::Result<()> {
    assert!(std::env::current_exe()?.ends_with("next-version"));
    println!("replacement-executable-ok");
    Ok(())
}

#[test]
#[ignore = "requires built TD_SANDBOX_TEST_JAIL, user namespaces, TMPDIR under worktree scratch"]
fn terminal_resize_input_and_stop_restore_the_host_terminal() -> io::Result<()> {
    terminal_case(false)
}

#[test]
#[ignore = "requires built TD_SANDBOX_TEST_JAIL, user namespaces, TMPDIR under worktree scratch"]
fn terminal_stop_survives_saturated_input() -> io::Result<()> {
    terminal_case(true)
}

fn terminal_case(saturate: bool) -> io::Result<()> {
    use std::io::{Read, Write};
    use std::time::{Duration, Instant};
    use td_ui::pty::{ChildCommand, Pty};
    let root = td_fs::TempDir::new("sandbox-terminal")?;
    let jail = PathBuf::from(
        std::env::var_os("TD_SANDBOX_TEST_JAIL")
            .ok_or_else(|| io::Error::other("TD_SANDBOX_TEST_JAIL missing"))?,
    );
    passed(call(&root, &jail, &["create", "test"])?)?;
    let home = root.join(".local/share/td-sandbox/test/home");
    let probe = home.join("probe");
    fs::copy(std::env::current_exe()?, &probe)?;
    let pty = Pty::open().map_err(io::Error::other)?;
    pty.resize(37, 101).map_err(io::Error::other)?;
    let slave = pty.peer().map_err(io::Error::other)?;
    let _retained_slave = slave.try_clone()?;
    let environment: Vec<_> = std::env::vars_os()
        .filter(|(k, _)| k != "HOME" && k != "TERM")
        .chain([
            ("HOME".into(), root.as_os_str().to_owned()),
            ("TERM".into(), "xterm-256color".into()),
        ])
        .collect();
    let mut child = td_ui::pty::spawn(
        &ChildCommand {
            program: env!("CARGO_BIN_EXE_td-sandbox").into(),
            arguments: vec![
                "--jail".into(),
                jail.into_os_string(),
                "run".into(),
                "test".into(),
                "--".into(),
                probe.into_os_string(),
                "--exact".into(),
                "terminal_fixture".into(),
                "--ignored".into(),
                "--nocapture".into(),
            ],
            leads_session: true,
        },
        &environment,
        &root,
        slave,
    )
    .map_err(io::Error::other)?;
    struct ChildGuard<'a>(&'a mut std::process::Child);
    impl Drop for ChildGuard<'_> {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let guard = ChildGuard(&mut child);
    let (tx, rx) = std::sync::mpsc::channel();
    let mut reader = pty.master().try_clone()?;
    std::thread::spawn(move || {
        let mut buf = [0; 4096];
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 {
                break;
            }
            if tx.send(buf.get(..n).unwrap_or_default().to_vec()).is_err() {
                break;
            }
        }
    });
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut output = Vec::new();
    let mut sent = false;
    while Instant::now() < deadline {
        if let Ok(bytes) = rx.recv_timeout(Duration::from_millis(50)) {
            output.extend(bytes);
        }
        if !sent && String::from_utf8_lossy(&output).contains("terminal-ready") {
            pty.master().write_all(b"probe-input\n")?;
            sent = true;
        }
        if String::from_utf8_lossy(&output).contains("terminal-input-ok") {
            break;
        }
        if guard.0.try_wait()?.is_some() {
            break;
        }
    }
    assert!(
        String::from_utf8_lossy(&output).contains("terminal-input-ok"),
        "{}",
        String::from_utf8_lossy(&output)
    );
    if saturate {
        let mut writer = pty.master().try_clone()?;
        std::thread::spawn(move || {
            let bytes = b"unread-input-line\n".repeat(1024);
            for _ in 0..2048 {
                if writer.write_all(&bytes).is_err() {
                    break;
                }
            }
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            let _ = rx.recv_timeout(Duration::from_millis(20));
        }
    }
    passed(call(&root, &PathBuf::from("unused"), &["stop", "test"])?)?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = guard.0.try_wait()? {
            assert_eq!(status.code(), Some(143));
            break;
        }
        assert!(Instant::now() < deadline, "stop did not end the session");
        std::thread::sleep(Duration::from_millis(20));
    }
    if saturate {
        return Ok(());
    }
    while rx.try_recv().is_ok() {}
    pty.master().write_all(b"restored-echo\n")?;
    let echo = rx
        .recv_timeout(Duration::from_secs(2))
        .map_err(io::Error::other)?;
    assert!(String::from_utf8_lossy(&echo).contains("restored-echo"));
    Ok(())
}

#[test]
#[ignore = "executed only on the sandbox's inner PTY"]
fn terminal_fixture() -> io::Result<()> {
    use std::io::{BufRead, Write};
    use std::time::{Duration, Instant};
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let size = td_ui::pty::window_size(&io::stdin()).map_err(io::Error::other)?;
        if size.rows == 37 && size.columns == 101 {
            break;
        }
        assert!(Instant::now() < deadline, "resize did not arrive");
        std::thread::sleep(Duration::from_millis(20));
    }
    // The generic controlling-terminal node resolves the inner session.
    let _tty = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")?;
    println!("terminal-ready");
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin().lock().read_line(&mut line)?;
    assert_eq!(line, "probe-input\n");
    println!("terminal-input-ok");
    io::stdout().flush()?;
    std::thread::sleep(Duration::from_secs(60));
    Ok(())
}

#[test]
#[ignore = "requires built TD_SANDBOX_TEST_JAIL, user namespaces, TMPDIR under worktree scratch"]
fn pipe_backpressure_preserves_stop_and_descendant_cleanup() -> io::Result<()> {
    use std::io::Write;
    use std::process::Stdio;
    use std::time::{Duration, Instant};
    let root = td_fs::TempDir::new("sandbox-pipe-pressure")?;
    let jail = PathBuf::from(
        std::env::var_os("TD_SANDBOX_TEST_JAIL")
            .ok_or_else(|| io::Error::other("TD_SANDBOX_TEST_JAIL missing"))?,
    );
    passed(call(&root, &jail, &["create", "test"])?)?;
    let home = root.join(".local/share/td-sandbox/test/home");
    let probe = home.join("probe");
    fs::copy(std::env::current_exe()?, &probe)?;
    for (fixture, expected) in [
        ("output_pressure_fixture", 143),
        ("inherited_stdin_fixture", 0),
    ] {
        let mut child = Command::new(env!("CARGO_BIN_EXE_td-sandbox"))
            .arg("--jail")
            .arg(&jail)
            .args(["run", "test", "--"])
            .arg(&probe)
            .args(["--exact", fixture, "--ignored", "--nocapture"])
            .env("HOME", &*root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        struct Guard<'a>(&'a mut std::process::Child);
        impl Drop for Guard<'_> {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let guard = Guard(&mut child);
        if fixture == "inherited_stdin_fixture" {
            let mut input = guard
                .0
                .stdin
                .take()
                .ok_or_else(|| io::Error::other("missing stdin"))?;
            std::thread::spawn(move || {
                let _ = input.write_all(&vec![b'x'; 16 * 1024 * 1024]);
            });
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        while !home.join(fixture).exists() {
            assert!(Instant::now() < deadline, "fixture never started");
            std::thread::sleep(Duration::from_millis(20));
        }
        if fixture == "output_pressure_fixture" {
            std::thread::sleep(Duration::from_secs(1));
            passed(call(&root, &jail, &["stop", "test"])?)?;
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = guard.0.try_wait()? {
                assert_eq!(status.code(), Some(expected));
                break;
            }
            assert!(
                Instant::now() < deadline,
                "blocked I/O delayed lifecycle completion"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    Ok(())
}

#[test]
#[ignore = "executed only inside the live sandbox"]
fn output_pressure_fixture() -> io::Result<()> {
    use std::io::Write;
    fs::write("output_pressure_fixture", "ready")?;
    io::stdout().write_all(&vec![b'x'; 16 * 1024 * 1024])
}

#[test]
#[ignore = "executed only inside the live sandbox"]
fn inherited_stdin_fixture() -> io::Result<()> {
    use std::process::Stdio;
    let _descendant = Command::new(std::env::current_exe()?)
        .args([
            "--exact",
            "sleeping_descendant_fixture",
            "--ignored",
            "--nocapture",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    fs::write("inherited_stdin_fixture", "ready")?;
    std::thread::sleep(std::time::Duration::from_millis(300));
    Ok(())
}

#[test]
#[ignore = "executed only as the live sandbox's descendant"]
fn sleeping_descendant_fixture() {
    std::thread::sleep(std::time::Duration::from_secs(60));
}

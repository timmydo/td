//! A real `td-jail --workspace` launch. Ignored by default: it needs
//! unprivileged user namespaces and a host `/bin/sh`, which the build
//! sandbox does not promise. Run it with `cargo test -- --ignored`.

use std::error::Error;
use std::fs;
use std::io::{Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixDatagram, UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const PROBE: &str = r#"#!/bin/sh
read -r request
echo "request=$request"
echo "uid=$(id -u) owner=$(stat -c %u .)"
echo "cwd=$(pwd)"
echo "home=$HOME path=$PATH"
touch built && echo "worktree=writable"
touch "$HOME/h" && echo "home=writable"
touch "$SHARED/x" 2>/dev/null || echo "shared=read-only"
touch /usr/x 2>/dev/null || echo "usr=read-only"
printf '#!/bin/sh\necho ran\n' > "$TMPDIR/s" && chmod +x "$TMPDIR/s" && "$TMPDIR/s" | sed 's/^/tmp=/'
grep -q '^Seccomp:[[:space:]]*2' /proc/self/status && echo "seccomp=filtered"
grep -q '^CapEff:[[:space:]]*0*$' /proc/self/status && echo "capabilities=none"
ls /proc | grep -c '^[0-9]' | sed 's/^/processes=/'
ls -A /etc | tr '\n' ' ' | sed 's/^/etc=/'; echo
ls / | tr '\n' ' ' | sed 's/^/root=/'; echo
"#;

/// A scratch workspace under `name` with the shell probe as its entry;
/// returns its directory and its spec.
fn prepare(name: &str) -> Result<(PathBuf, PathBuf), Box<dyn Error>> {
    prepare_with(name, None)
}

/// As `prepare`, with `entry` as the entry when given.
fn prepare_with(name: &str, entry: Option<&Path>) -> Result<(PathBuf, PathBuf), Box<dyn Error>> {
    let base = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = fs::remove_dir_all(&base);
    for name in ["home", "tree", "shared", "bin"] {
        fs::create_dir_all(base.join(name))?;
    }
    let base = fs::canonicalize(&base)?;
    fs::set_permissions(base.join("home"), fs::Permissions::from_mode(0o700))?;
    let probe = base.join("bin/probe");
    let entry = entry.map_or(probe.clone(), Path::to_path_buf);
    let shared = base.join("shared");
    fs::write(
        &probe,
        PROBE.replace(
            "$SHARED",
            shared.to_str().ok_or("scratch path is not UTF-8")?,
        ),
    )?;
    fs::set_permissions(&probe, fs::Permissions::from_mode(0o755))?;
    let spec = base.join("spec");
    fs::write(
        &spec,
        format!(
            "format=1\nentry={}\nhome={}\nworktree={}\nread={}\n",
            entry.display(),
            base.join("home").display(),
            base.join("tree").display(),
            shared.display()
        ),
    )?;
    fs::set_permissions(&spec, fs::Permissions::from_mode(0o600))?;
    Ok((base, spec))
}

#[test]
#[ignore = "needs unprivileged user namespaces and a host /bin/sh"]
fn a_workspace_launch_is_confined_and_speaks_on_its_channel() -> Result<(), Box<dyn Error>> {
    let (base, spec) = prepare("workspace-launch")?;
    let (mut ours, theirs) = UnixStream::pair()?;
    let child = Command::new(env!("CARGO_BIN_EXE_td-jail"))
        .arg("--workspace")
        .arg(std::process::id().to_string())
        .arg(&spec)
        .stdin(Stdio::from(OwnedFd::from(theirs.try_clone()?)))
        .stdout(Stdio::from(OwnedFd::from(theirs)))
        .stderr(Stdio::piped())
        .spawn()?;
    ours.write_all(b"hello\n")?;
    let mut reply = String::new();
    ours.read_to_string(&mut reply)?;
    let output = child.wait_with_output()?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{reply}\n{stderr}");

    let uid = fs::metadata(&base)?.uid();
    for line in [
        "request=hello".to_string(),
        format!("uid={uid} owner={uid}"),
        format!("cwd={}", base.join("tree").display()),
        "worktree=writable".into(),
        "home=writable".into(),
        "shared=read-only".into(),
        "usr=read-only".into(),
        "tmp=ran".into(),
        "seccomp=filtered".into(),
        "capabilities=none".into(),
        "etc=".into(),
    ] {
        assert!(reply.contains(&line), "{line} absent from:\n{reply}");
    }
    assert!(base.join("tree/built").exists());
    let etc = reply
        .lines()
        .find(|line| line.starts_with("etc="))
        .ok_or("no etc line")?;
    for absent in ["shadow", "sudoers", "ssh"] {
        assert!(!etc.split(' ').any(|name| name == absent), "{etc}");
    }
    let root = reply
        .lines()
        .find(|line| line.starts_with("root="))
        .ok_or("no root line")?;
    assert!(
        !root.split(' ').any(|name| name == "root" || name == "boot"),
        "{root}"
    );
    fs::remove_dir_all(&base)?;
    Ok(())
}

/// A datagram pair is no channel: it could address any socket on the host,
/// and the filter leaves `sendto` alone.
#[test]
#[ignore = "needs unprivileged user namespaces and a host /bin/sh"]
fn a_datagram_channel_is_refused() -> Result<(), Box<dyn Error>> {
    let (base, spec) = prepare("workspace-datagram")?;
    let (_ours, theirs) = UnixDatagram::pair()?;
    let output = Command::new(env!("CARGO_BIN_EXE_td-jail"))
        .arg("--workspace")
        .arg(std::process::id().to_string())
        .arg(&spec)
        .stdin(Stdio::from(OwnedFd::from(theirs.try_clone()?)))
        .stdout(Stdio::from(OwnedFd::from(theirs)))
        .stderr(Stdio::piped())
        .output()?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{stderr}");
    assert!(
        stderr.contains("not an unnamed, connected Unix stream socket"),
        "{stderr}"
    );
    fs::remove_dir_all(&base)?;
    Ok(())
}

/// The workspace program as the kernel applies it, seen from a Rust
/// program inside: no Unix socket of its own, a stream pair but no
/// datagram one, a child spawned on std's fork path (which makes a
/// sequenced-packet pair), and loopback alone. The entry is this
/// test binary, running `rust_probe_inside`.
#[test]
#[ignore = "needs unprivileged user namespaces and a host /bin/sh"]
fn the_workspace_filter_holds_for_a_rust_program() -> Result<(), Box<dyn Error>> {
    let me = fs::canonicalize(std::env::current_exe()?)?;
    let (base, spec) = prepare_with("workspace-rust", Some(&me))?;
    let (mut ours, theirs) = UnixStream::pair()?;
    let child = Command::new(env!("CARGO_BIN_EXE_td-jail"))
        .arg("--workspace")
        .arg(std::process::id().to_string())
        .arg(&spec)
        .args([
            "--exact",
            "rust_probe_inside",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .stdin(Stdio::from(OwnedFd::from(theirs.try_clone()?)))
        .stdout(Stdio::from(OwnedFd::from(theirs)))
        .stderr(Stdio::piped())
        .spawn()?;
    let mut reply = String::new();
    ours.read_to_string(&mut reply)?;
    let output = child.wait_with_output()?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{reply}\n{stderr}");
    for line in [
        "unix-socket=refused",
        "stream-pair=made",
        "datagram-pair=refused",
        "fork-path-spawn=ran",
        "interfaces=lo",
    ] {
        // Whole lines: `interfaces=lo,eth0` is no isolated network.
        assert!(
            reply.lines().any(|got| got == line),
            "{line} absent from:\n{reply}"
        );
    }
    fs::remove_dir_all(&base)?;
    Ok(())
}

/// Run inside the jail by the test above; outside it does nothing.
#[test]
#[ignore = "run inside a workspace instance"]
fn rust_probe_inside() -> Result<(), Box<dyn Error>> {
    if !std::env::current_exe()?.starts_with("/opt/workspace/bin") {
        return Ok(());
    }
    let refused = |result: std::io::Result<()>| match result {
        Err(error) if error.raw_os_error() == Some(1) => "refused",
        Err(_) => "failed-otherwise",
        Ok(()) => "made",
    };
    let home = std::env::var("HOME")?;
    // Off libtest's own `test ... ` line, so each report is a whole line.
    println!();
    println!(
        "unix-socket={}",
        refused(UnixListener::bind(Path::new(&home).join("s")).map(drop))
    );
    println!("stream-pair={}", refused(UnixStream::pair().map(drop)));
    println!("datagram-pair={}", refused(UnixDatagram::pair().map(drop)));
    // A bare name with a changed PATH takes std's fork path, which makes a
    // sequenced-packet pair first: this fails with EPERM without it.
    let ran = Command::new("sh")
        .env("PATH", "/usr/bin:/bin:/run/current-system/profile/bin")
        .args(["-c", "exit 0"])
        .status()
        .is_ok_and(|status| status.success());
    println!("fork-path-spawn={}", if ran { "ran" } else { "failed" });
    let interfaces: Vec<String> = fs::read_to_string("/proc/net/dev")?
        .lines()
        .skip(2)
        .filter_map(|line| line.split(':').next().map(|name| name.trim().to_string()))
        .collect();
    println!("interfaces={}", interfaces.join(","));
    Ok(())
}

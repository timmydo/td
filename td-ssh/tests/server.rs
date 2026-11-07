//! Host integration (DESIGN.md section 9): `td-ssh` against `td-sshd
//! --single-user` on loopback, unprivileged, with the jail's seccomp filter on.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use std::ffi::OsStr;
use std::fs;
use std::io::{PipeWriter, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::Once;
use std::time::{Duration, Instant};
use td_fs::TempDir;
use td_ssh::client::signed_auth_data;
use td_ssh::conn;
use td_ssh::kex::{self, Kex, KexEnvironment, RandomSource, Role, DEFAULT_KEX};
use td_ssh::keyfile::{PrivateKey, PrivateKeyFile, PublicKey};
use td_ssh::msg;
use td_ssh::packet::{exchange_versions, PacketReader, PacketWriter, IDENTIFICATION};
use td_ssh::wire::{Reader, Writer};

const SSH: &str = env!("CARGO_BIN_EXE_td-ssh");
const SSHD: &str = env!("CARGO_BIN_EXE_td-sshd");
const KEYGEN: &str = env!("CARGO_BIN_EXE_td-ssh-keygen");
const USER: &str = "tester";
const DEADLINE: Duration = Duration::from_secs(20);
/// Far past what this binary takes. A child that hangs must fail the suite, not
/// hold it for ever.
const WATCHDOG: Duration = Duration::from_secs(600);

fn start_watchdog() {
    static STARTED: Once = Once::new();
    STARTED.call_once(|| {
        std::thread::spawn(|| {
            std::thread::sleep(WATCHDOG);
            eprintln!("tests/server.rs: still running after {WATCHDOG:?}; a child has hung");
            std::process::exit(101);
        });
    });
}

/// Every child goes through here: the watchdog is running, and stdin is nothing it
/// could wait on, unless its test says otherwise.
fn test_command(program: impl AsRef<OsStr>) -> Command {
    start_watchdog();
    let mut command = std::process::Command::new(program);
    command.stdin(Stdio::null());
    command
}

struct Server {
    _dir: TempDir,
    root: PathBuf,
    home: PathBuf,
    port: u16,
    log: PathBuf,
    child: Child,
    /// Waits on `death_pipe`. The test's death, however it comes, closes that pipe
    /// and this ends the daemon. A test cannot have the kernel do it, which takes
    /// `PR_SET_PDEATHSIG` before an exec and so `unsafe`. Needs a `kill` on the host:
    /// where there is none, the sandbox's end is what ends the daemon.
    warden: Child,
    death_pipe: Option<PipeWriter>,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.death_pipe = None;
        let _ = self.warden.wait();
    }
}

/// What ends `daemon` when this process has gone.
fn warden_of(daemon: u32) -> (Child, PipeWriter) {
    let (reader, writer) = std::io::pipe().unwrap();
    let warden = test_command("/bin/sh")
        .args(["-c", "read _; kill \"$1\" 2>/dev/null", "warden"])
        .arg(daemon.to_string())
        .stdin(reader)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    (warden, writer)
}

fn keygen(path: &Path) {
    let output = test_command(KEYGEN)
        .args(["-q", "-t", "ed25519", "-N", "", "-f"])
        .arg(path)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
}

struct Options<'a> {
    listen: &'a str,
    /// Started by a shell that clears its umask, as nothing stops a parent doing.
    umask_zero: bool,
    /// Appended to the policy, with `{root}` naming the test's directory.
    extra: &'a str,
    login_grace: u32,
}

const SHORT_GRACE: u32 = 2;

fn start(extra_config: &str) -> Server {
    start_with(&Options {
        listen: "127.0.0.1",
        umask_zero: false,
        extra: extra_config,
        login_grace: SHORT_GRACE,
    })
}

fn start_with_grace(login_grace: u32) -> Server {
    start_with(&Options {
        listen: "127.0.0.1",
        umask_zero: false,
        extra: "",
        login_grace,
    })
}

fn start_with(options: &Options<'_>) -> Server {
    let dir = TempDir::new("td-sshd-it").unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
    let home = root.join("home");
    let ssh_dir = home.join(".ssh");
    fs::create_dir_all(&ssh_dir).unwrap();
    keygen(&root.join("host"));
    keygen(&root.join("client"));
    keygen(&root.join("stranger"));
    fs::copy(root.join("client.pub"), ssh_dir.join("authorized_keys")).unwrap();
    fs::set_permissions(
        ssh_dir.join("authorized_keys"),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    let config = root.join("sshd_config");
    fs::write(
        &config,
        format!(
            "Port 0\nListenAddress {}\nHostKey {}\nAuthorizedKeysFile .ssh/authorized_keys\nSubsystem sftp internal-sftp\nPermitRootLogin no\nLoginGraceTime {}\nMaxAuthTries 3\nMaxSessions 4\n{}",
            options.listen,
            root.join("host").display(),
            options.login_grace,
            options.extra.replace("{root}", root.to_str().unwrap()),
        ),
    )
    .unwrap();
    let log = root.join("log");
    let mut command = if options.umask_zero {
        let mut wrapper = test_command("/bin/sh");
        wrapper.args(["-c", "umask 0; exec \"$0\" \"$@\"", SSHD]);
        wrapper
    } else {
        test_command(SSHD)
    };
    let child = command
        .arg("-f")
        .arg(&config)
        .args(["--single-user", USER])
        .env("HOME", &home)
        .env("SHELL", "/bin/sh")
        .stderr(fs::File::create(&log).unwrap())
        .spawn()
        .unwrap();
    // Before anything else can fail: the daemon must not outlive this.
    let (warden, death_pipe) = warden_of(child.id());
    let start = Instant::now();
    let port = loop {
        let text = fs::read_to_string(&log).unwrap_or_default();
        if let Some(rest) = text.split("listening on port ").nth(1) {
            break rest.split_whitespace().next().unwrap().parse().unwrap();
        }
        assert!(start.elapsed() < DEADLINE, "no port in {text:?}");
        std::thread::sleep(Duration::from_millis(10));
    };
    Server {
        _dir: dir,
        root,
        home,
        port,
        log,
        child,
        warden,
        death_pipe: Some(death_pipe),
    }
}

impl Server {
    fn ssh(&self, key: &str) -> Command {
        let mut command = test_command(SSH);
        command
            .args(["-F", "/dev/null", "-p"])
            .arg(self.port.to_string())
            .arg("-i")
            .arg(self.root.join(key))
            .args([
                "-o",
                "BatchMode=yes",
                "-o",
                "StrictHostKeyChecking=accept-new",
            ])
            .arg("-o")
            .arg(format!(
                "UserKnownHostsFile={}",
                self.root.join("known_hosts").display()
            ));
        command
    }

    fn run(&self, command_line: &str) -> Output {
        self.ssh("client")
            .arg(format!("{USER}@127.0.0.1"))
            .arg(command_line)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    fn log(&self) -> String {
        fs::read_to_string(&self.log).unwrap_or_default()
    }

    fn set_authorized_line(&self, prefix: &str) {
        let key = fs::read_to_string(self.root.join("client.pub")).unwrap();
        fs::write(
            self.home.join(".ssh/authorized_keys"),
            format!("{prefix}{key}"),
        )
        .unwrap();
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// The child's `PATH` is `/bin`, which on a build host may hold no utilities, so
/// these commands name nothing but the shell's built-ins.
#[test]
fn exec_status_stderr_and_environment() {
    let server = start("");
    let output = server.run("echo hello; echo oops >&2; exit 42");
    assert_eq!(stdout(&output), "hello\n");
    // The first connection also says it recorded the host key.
    assert!(stderr(&output).ends_with("oops\n"), "{}", stderr(&output));
    assert_eq!(output.status.code(), Some(42));

    // Not TERM: a shell that finds it unset may invent one for itself.
    let output = server.run("echo \"$HOME|$USER|$LOGNAME|$SHELL|$PATH|$SSH_TTY|$PWD\"");
    assert_eq!(
        stdout(&output),
        format!(
            "{home}|{USER}|{USER}|/bin/sh|/bin||{home}\n",
            home = server.home.display()
        ),
        "{}",
        stderr(&output)
    );
    let output = server.run("echo \"$SSH_CONNECTION\"");
    let connection = stdout(&output);
    let fields: Vec<&str> = connection.split_whitespace().collect();
    assert_eq!(fields.len(), 4, "{connection}");
    assert_eq!(fields[0], "127.0.0.1");
    assert_eq!(fields[3], server.port.to_string());
}

#[test]
fn a_killed_child_is_a_failure_at_the_client() {
    let server = start("");
    let output = server.run("kill -TERM $$");
    assert_ne!(output.status.code(), Some(0));
}

#[test]
fn the_socket_and_signer_never_reach_a_child() {
    let server = start("");
    // Only the shell's own redirection of a directory listing would add anything,
    // so walk /proc/self/fd with a glob.
    let output = server.run("cd /proc/self/fd && echo *");
    let fds = stdout(&output);
    let listed: Vec<&str> = fds.split_whitespace().collect();
    assert!(
        listed.iter().all(|fd| matches!(*fd, "0" | "1" | "2" | "3")),
        "{fds}"
    );
    for (name, wanted) in [("0", "pipe:"), ("1", "pipe:"), ("2", "pipe:")] {
        let output = server.run(&format!(
            "cd /proc/self/fd; if test -p {name}; then echo {wanted}; else echo other; fi"
        ));
        assert_eq!(stdout(&output).trim(), wanted, "{name}");
    }
}

#[test]
fn stdin_reaches_the_child_and_eof_is_delivered() {
    let server = start("");
    let mut child = server
        .ssh("client")
        .arg(format!("{USER}@127.0.0.1"))
        .arg("while read -r line; do echo \"got $line\"; done; echo done")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"a\nb\n").unwrap();
    let output = child.wait_with_output().unwrap();
    assert_eq!(stdout(&output), "got a\ngot b\ndone\n");
}

/// Rekeys many times in a transfer through the client's byte limit, with data
/// moving both ways, which exercises the signer's rate limit too.
#[test]
fn a_large_transfer_survives_forced_rekeys() {
    let server = start("");
    let payload: Vec<u8> = (0..4 * 1024 * 1024u32)
        .map(|index| (index.wrapping_mul(2_654_435_761) >> 24) as u8)
        .collect();
    let source = server.root.join("payload");
    fs::write(&source, &payload).unwrap();

    // Up: the shell has only its built-ins to read with, so it discards the bytes
    // and the point is that the transport carried every one of them.
    let output = server
        .ssh("client")
        .args(["-v", "-o", "RekeyLimit=256K"])
        .arg(format!("{USER}@127.0.0.1"))
        .arg("while read -r line; do :; done; echo up-done")
        .stdin(fs::File::open(&source).unwrap())
        .output()
        .unwrap();
    assert_eq!(stdout(&output), "up-done\n", "{}", stderr(&output));
    assert!(rekeys_reported(&output) >= 2, "{}", stderr(&output));

    // Down: a shell loop writing about 2 MiB under a lower limit.
    let line = "x".repeat(1024);
    let output = server
        .ssh("client")
        .args(["-v", "-o", "RekeyLimit=128K"])
        .arg(format!("{USER}@127.0.0.1"))
        .arg(format!(
            "i=0; while test $i -lt 2048; do echo {line}; i=$((i+1)); done"
        ))
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(output.stdout.len(), 2048 * 1025);
    assert!(output
        .stdout
        .chunks(1025)
        .all(|row| row.last() == Some(&b'\n')));
    // This fits inside the signer's one-second limit, so a receive-side rekey
    // completing at all is what this shows: 128K alone triggered it.
    assert!(rekeys_reported(&output) >= 1, "{}", stderr(&output));
}

/// What `-v` says at the end of a connection.
fn rekeys_reported(output: &Output) -> u32 {
    stderr(output)
        .lines()
        .find_map(|line| {
            line.strip_prefix("debug1: rekeyed ")?
                .strip_suffix(" times")?
                .parse()
                .ok()
        })
        .unwrap_or(0)
}

#[test]
fn a_pty_session_gets_a_controlling_terminal() {
    let server = start("");
    let output = server
        .ssh("client")
        .arg("-tt")
        .arg(format!("{USER}@127.0.0.1"))
        .arg("echo \"[$SSH_TTY][$TERM]\"; test -t 0 && test -t 1 && test -t 2 && echo all-tty")
        .stdin(Stdio::null())
        .env("TERM", "vt100")
        .output()
        .unwrap();
    let text = stdout(&output);
    assert!(text.contains("[/dev/pts/"), "{text}{}", stderr(&output));
    assert!(text.contains("][vt100]"), "{text}");
    assert!(text.contains("all-tty"), "{text}");
}

#[test]
fn a_refused_tty_is_only_a_lack_of_one() {
    let server = start("Match User tester\n\tPermitTTY no\n");
    let output = server
        .ssh("client")
        .arg("-tt")
        .arg(format!("{USER}@127.0.0.1"))
        .arg("test -t 0 && echo has-tty || echo no-tty")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(stdout(&output).contains("no-tty"), "{}", stderr(&output));
}

#[test]
fn another_key_or_account_is_permission_denied() {
    let server = start("");
    let output = server
        .ssh("stranger")
        .arg(format!("{USER}@127.0.0.1"))
        .arg("true")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(255));
    assert!(
        stderr(&output).contains("Permission denied (publickey)."),
        "{}",
        stderr(&output)
    );
    let output = server
        .ssh("client")
        .arg("someone-else@127.0.0.1")
        .arg("true")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(255));
    assert!(stderr(&output).contains("Permission denied (publickey)."));
}

#[test]
fn authorized_keys_options_are_honoured_and_unknown_ones_refuse() {
    let server = start("");
    server.set_authorized_line("command=\"echo forced:$SSH_ORIGINAL_COMMAND\" ");
    assert_eq!(stdout(&server.run("asked")), "forced:asked\n");

    server.set_authorized_line("environment=\"A=b\" ");
    assert_eq!(server.run("true").status.code(), Some(255));

    server.set_authorized_line("from=\"10.9.9.9\" ");
    assert_eq!(server.run("true").status.code(), Some(255));

    server.set_authorized_line("from=\"127.0.0.*\" ");
    assert_eq!(server.run("true").status.code(), Some(0));

    server.set_authorized_line("restrict ");
    let output = server
        .ssh("client")
        .arg("-tt")
        .arg(format!("{USER}@127.0.0.1"))
        .arg("test -t 0 && echo has-tty || echo no-tty")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(stdout(&output).contains("no-tty"));
}

#[test]
fn force_command_overrides_a_key_command_and_a_shell() {
    let server = start("Match User tester\n\tForceCommand echo admin:$SSH_ORIGINAL_COMMAND\n");
    server.set_authorized_line("command=\"echo key\" ");
    assert_eq!(stdout(&server.run("asked")), "admin:asked\n");
}

#[test]
fn strict_modes_refuse_a_writable_key_file_and_log_why() {
    let server = start("");
    let path = server.home.join(".ssh/authorized_keys");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o660)).unwrap();
    assert_eq!(server.run("true").status.code(), Some(255));
    assert!(
        server.log().contains("bad ownership or modes"),
        "{}",
        server.log()
    );
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    fs::set_permissions(&server.home, fs::Permissions::from_mode(0o777)).unwrap();
    assert_eq!(server.run("true").status.code(), Some(255));
    fs::set_permissions(&server.home, fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(server.run("true").status.code(), Some(0));
}

#[test]
fn acceptance_is_logged_with_the_fingerprint() {
    let server = start("");
    assert!(server.run("true").status.success());
    let printed = test_command(KEYGEN)
        .arg("-l")
        .arg("-f")
        .arg(server.root.join("client.pub"))
        .output()
        .unwrap();
    let line = String::from_utf8_lossy(&printed.stdout).into_owned();
    let fingerprint = line.split_whitespace().nth(1).unwrap().to_string();
    let log = server.log();
    assert!(
        log.contains(&format!(
            "Accepted publickey for {USER} from 127.0.0.1 port "
        )) && log.contains(&format!("ssh2: ED25519 {fingerprint}")),
        "{log}"
    );
}

fn read_banner(stream: &mut TcpStream) -> String {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut banner = Vec::new();
    let mut byte = [0u8; 1];
    while banner.last() != Some(&b'\n') {
        if stream.read(&mut byte).unwrap() == 0 {
            break;
        }
        banner.push(byte[0]);
    }
    String::from_utf8_lossy(&banner).into_owned()
}

fn status_of(pid: &str) -> Option<String> {
    fs::read_to_string(format!("/proc/{pid}/status")).ok()
}

fn field<'a>(status: &'a str, name: &str) -> Option<&'a str> {
    status
        .lines()
        .find_map(|line| line.strip_prefix(name)?.strip_prefix(':'))
        .map(str::trim)
}

fn children_of(parent: u32) -> Vec<String> {
    let mut found = Vec::new();
    for entry in fs::read_dir("/proc").unwrap().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.bytes().all(|byte| byte.is_ascii_digit()) {
            continue;
        }
        if let Some(status) = status_of(&name) {
            if field(&status, "PPid") == Some(parent.to_string().as_str()) {
                found.push(name);
            }
        }
    }
    found
}

/// The worker holding a pre-auth connection is really inside the filter, which
/// also proves the raw `prctl` and `seccomp` layer against a real kernel.
#[test]
fn the_pre_auth_worker_runs_under_seccomp_and_no_new_privs() {
    let server = start("");
    let mut stream = TcpStream::connect(("127.0.0.1", server.port)).unwrap();
    assert!(read_banner(&mut stream).starts_with("SSH-2.0-tdssh"));
    let monitor = children_of(server.child.id());
    assert_eq!(monitor.len(), 1, "{monitor:?}");
    let worker = children_of(monitor[0].parse().unwrap());
    assert_eq!(worker.len(), 1, "{worker:?}");
    let status = status_of(&worker[0]).unwrap();
    assert_eq!(field(&status, "Seccomp"), Some("2"), "{status}");
    assert_eq!(field(&status, "NoNewPrivs"), Some("1"), "{status}");
    assert_eq!(field(&status, "Threads"), Some("1"), "{status}");
    // Standard three, the socket's two clones, the monitor's clone, urandom.
    let fds = fs::read_dir(format!("/proc/{}/fd", worker[0]))
        .unwrap()
        .count();
    assert_eq!(
        fds, 7,
        "the worker holds only what it opened before the jail"
    );
}

/// The signer takes the worker's jail; only the session is outside it.
#[test]
fn after_authentication_the_signer_is_jailed_and_no_worker_remains() {
    let server = start("");
    let mut child = server
        .ssh("client")
        .arg(format!("{USER}@127.0.0.1"))
        .arg("read -r line; echo $line")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let start = Instant::now();
    let signer = loop {
        let log = server.log();
        let stage = children_of(server.child.id());
        if log.contains("Accepted publickey") && stage.len() == 1 {
            let status = status_of(&stage[0]).unwrap_or_default();
            if field(&status, "Seccomp") == Some("2") {
                break stage[0].clone();
            }
        }
        assert!(start.elapsed() < DEADLINE, "no jailed signer: {log}");
        std::thread::sleep(Duration::from_millis(10));
    };
    let status = status_of(&signer).unwrap();
    assert_eq!(field(&status, "NoNewPrivs"), Some("1"));
    let session = children_of(signer.parse().unwrap());
    assert_eq!(session.len(), 1, "{session:?}");
    let session_status = status_of(&session[0]).unwrap();
    assert_eq!(field(&session_status, "Seccomp"), Some("0"));
    child.stdin.take().unwrap().write_all(b"bye\n").unwrap();
    let output = child.wait_with_output().unwrap();
    assert_eq!(stdout(&output), "bye\n");
}

#[test]
fn login_grace_time_is_absolute_and_ends_the_connection() {
    let server = start("");
    let mut stream = TcpStream::connect(("127.0.0.1", server.port)).unwrap();
    assert!(read_banner(&mut stream).starts_with("SSH-2.0-tdssh"));
    stream.write_all(b"SSH-2.0-silent\r\n").unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let start = Instant::now();
    let mut sink = [0u8; 4096];
    loop {
        match stream.read(&mut sink) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        assert!(start.elapsed() < Duration::from_secs(9), "still open");
    }
    assert!(
        start.elapsed() < Duration::from_secs(9),
        "grace of 2 seconds took {:?}",
        start.elapsed()
    );
}

#[test]
fn the_per_source_cap_closes_excess_connections_before_any_byte() {
    let server = start_with_grace(10);
    let mut held = Vec::new();
    for _ in 0..8 {
        let mut stream = TcpStream::connect(("127.0.0.1", server.port)).unwrap();
        assert!(read_banner(&mut stream).starts_with("SSH-2.0-tdssh"));
        held.push(stream);
    }
    let mut excess = TcpStream::connect(("127.0.0.1", server.port)).unwrap();
    excess
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut byte = [0u8; 1];
    assert!(matches!(excess.read(&mut byte), Ok(0) | Err(_)));
    // Releasing one reopens a slot.
    drop(held.pop());
    let start = Instant::now();
    loop {
        let mut stream = TcpStream::connect(("127.0.0.1", server.port)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_millis(500)))
            .unwrap();
        if matches!(stream.read(&mut byte), Ok(1)) {
            break;
        }
        assert!(start.elapsed() < DEADLINE, "the slot never reopened");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn the_config_test_and_print_modes() {
    let server = start("");
    let config = server.root.join("sshd_config");
    let tested = test_command(SSHD)
        .args(["-t", "-f"])
        .arg(&config)
        .args(["--single-user", USER])
        .output()
        .unwrap();
    assert!(tested.status.success(), "{tested:?}");
    let printed = test_command(SSHD)
        .args(["-T", "-f"])
        .arg(&config)
        .args(["--single-user", USER, "-C", "user=tester,addr=127.0.0.1"])
        .output()
        .unwrap();
    let text = stdout(&printed);
    for line in [
        "permitrootlogin no",
        "passwordauthentication no",
        "subsystem sftp internal-sftp",
    ] {
        assert!(text.lines().any(|seen| seen == line), "{line}: {text}");
    }
    fs::write(&config, "Port 22\nNotAKeyword x\n").unwrap();
    let refused = test_command(SSHD)
        .args(["-t", "-f"])
        .arg(&config)
        .args(["--single-user", USER])
        .output()
        .unwrap();
    assert!(!refused.status.success());
    assert!(stderr(&refused).contains("notakeyword"), "{refused:?}");
}

#[test]
fn a_loose_host_key_stops_the_server_starting() {
    let server = start("");
    let config = server.root.join("sshd_config");
    fs::set_permissions(server.root.join("host"), fs::Permissions::from_mode(0o640)).unwrap();
    let refused = test_command(SSHD)
        .arg("-f")
        .arg(&config)
        .args(["--single-user", USER])
        .output()
        .unwrap();
    assert!(!refused.status.success());
    assert!(stderr(&refused).contains("host key"), "{refused:?}");
}

#[test]
fn single_user_is_refused_as_root() {
    let uid = fs::metadata("/proc/self").unwrap().uid();
    if uid != 0 {
        return;
    }
    let refused = test_command(SSHD)
        .args(["-t", "-f", "/dev/null", "--single-user", "x"])
        .output()
        .unwrap();
    assert!(!refused.status.success());
    assert!(stderr(&refused).contains("refused as root"));
}

/// Every removed client feature is refused, naming the feature.
#[test]
fn removed_client_features_exit_255() {
    for flag in ["-A", "-X", "-Y", "-f", "-M", "-C"] {
        let output = test_command(SSH)
            .args(["-F", "/dev/null", flag, "x@127.0.0.1", "true"])
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(255), "{flag}");
    }
    for flag in ["-L", "-R", "-D", "-J", "-W"] {
        let output = test_command(SSH)
            .args(["-F", "/dev/null", flag, "1:2:3", "x@127.0.0.1", "true"])
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(255), "{flag}");
    }
}

struct ClientEnv(RandomSource<fs::File>);

impl KexEnvironment for ClientEnv {
    fn random(&mut self, out: &mut [u8]) -> td_ssh::error::Result<()> {
        self.0.fill(out)
    }

    fn host_key_blob(&mut self) -> td_ssh::error::Result<Vec<u8>> {
        Err(td_ssh::error::Error::protocol("the client has no host key"))
    }

    fn sign(&mut self, _hash: &[u8; 32]) -> td_ssh::error::Result<Vec<u8>> {
        Err(td_ssh::error::Error::protocol("the client signs no hash"))
    }

    fn check_host_key(&mut self, _blob: &[u8]) -> td_ssh::error::Result<()> {
        Ok(())
    }
}

/// A client that sends exactly the packets its test names, past key exchange.
struct RawClient {
    reader: PacketReader<TcpStream>,
    writer: PacketWriter<TcpStream>,
    session_id: [u8; 32],
    server_version: String,
}

impl RawClient {
    fn connect(port: u16) -> Self {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream.set_read_timeout(Some(DEADLINE)).unwrap();
        let server_version = exchange_versions(&mut stream, true).unwrap();
        let mut engine = Kex::new(
            Role::Client,
            IDENTIFICATION,
            &server_version,
            DEFAULT_KEX.to_vec(),
        );
        let writer_half = stream.try_clone().unwrap();
        let mut reader = PacketReader::new(stream);
        let mut writer = PacketWriter::new(writer_half);
        let mut env = ClientEnv(RandomSource(fs::File::open("/dev/urandom").unwrap()));
        let session_id =
            conn::initial_kex(&mut reader, &mut writer, &mut engine, &mut env).unwrap();
        Self {
            reader,
            writer,
            session_id,
            server_version,
        }
    }

    fn send(&mut self, message: &Writer) {
        self.writer.write_packet(message.as_slice()).unwrap();
    }

    fn receive(&mut self) -> Vec<u8> {
        loop {
            let packet = self.reader.read_packet().unwrap();
            if !matches!(packet.first(), Some(&msg::IGNORE | &msg::DEBUG)) {
                return packet;
            }
        }
    }

    /// The next packet that is not `kind`-independent window traffic.
    fn receive_skipping_window(&mut self) -> Vec<u8> {
        loop {
            let packet = self.receive();
            if packet[0] != msg::CHANNEL_WINDOW_ADJUST {
                return packet;
            }
        }
    }

    fn start_auth(&mut self) {
        let mut request = Writer::message(msg::SERVICE_REQUEST);
        request.str("ssh-userauth");
        self.send(&request);
        assert_eq!(self.receive()[0], msg::SERVICE_ACCEPT);
    }

    fn offer(&mut self, key: &PrivateKey) -> Vec<u8> {
        let blob = kex::ed25519_public_blob(&key.public().key);
        let mut request = Writer::message(msg::USERAUTH_REQUEST);
        request
            .str(USER)
            .str("ssh-connection")
            .str("publickey")
            .bool(false)
            .str("ssh-ed25519")
            .string(&blob);
        self.send(&request);
        self.receive()
    }

    fn login(&mut self, key: &PrivateKey) {
        self.start_auth();
        let blob = kex::ed25519_public_blob(&key.public().key);
        let signature = key
            .sign(&signed_auth_data(&self.session_id, USER, &blob))
            .unwrap();
        let mut request = Writer::message(msg::USERAUTH_REQUEST);
        request
            .str(USER)
            .str("ssh-connection")
            .str("publickey")
            .bool(true)
            .str("ssh-ed25519")
            .string(&blob)
            .string(&kex::ed25519_signature_blob(&signature));
        self.send(&request);
        assert_eq!(self.receive()[0], msg::USERAUTH_SUCCESS);
    }

    /// A session channel running SFTP; the server's channel number.
    fn start_sftp(&mut self) -> u32 {
        let mut open = Writer::message(msg::CHANNEL_OPEN);
        open.str("session").u32(0).u32(1 << 21).u32(1 << 15);
        self.send(&open);
        let confirmation = self.receive_skipping_window();
        assert_eq!(confirmation[0], msg::CHANNEL_OPEN_CONFIRMATION);
        let mut reader = Reader::new(&confirmation[1..]);
        reader.u32().unwrap();
        let channel = reader.u32().unwrap();
        let mut request = Writer::message(msg::CHANNEL_REQUEST);
        request.u32(channel).str("subsystem").bool(true).str("sftp");
        self.send(&request);
        assert_eq!(self.receive_skipping_window()[0], msg::CHANNEL_SUCCESS);
        channel
    }

    /// One SFTP request and the first byte of its reply, which is the reply's type.
    fn sftp_call(&mut self, channel: u32, body: &Writer) -> u8 {
        let mut framed = Writer::new();
        framed.string(body.as_slice());
        let mut data = Writer::message(msg::CHANNEL_DATA);
        data.u32(channel).string(framed.as_slice());
        self.send(&data);
        // The length and the body arrive as separate channel data.
        let mut received = Vec::new();
        while received.len() < 5 {
            let reply = self.receive_skipping_window();
            assert_eq!(reply[0], msg::CHANNEL_DATA);
            let mut reader = Reader::new(&reply[1..]);
            reader.u32().unwrap();
            received.extend_from_slice(reader.string().unwrap());
        }
        received[4]
    }
}

fn load_key(path: &Path) -> PrivateKey {
    let text = fs::read_to_string(path).unwrap();
    PrivateKeyFile::parse(&text).unwrap().decrypt(None).unwrap()
}

fn wait_for(what: &str, done: impl Fn() -> bool) {
    let start = Instant::now();
    while !done() {
        assert!(start.elapsed() < DEADLINE, "never happened: {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// A grace time far past anything this test waits for, so only the connection
/// ending can be what gives the slot back.
const LONG_GRACE: u32 = 120;

#[test]
fn a_closed_connection_ends_its_monitor_well_inside_the_grace_time() {
    let server = start_with_grace(LONG_GRACE);
    let mut stream = TcpStream::connect(("127.0.0.1", server.port)).unwrap();
    assert!(read_banner(&mut stream).starts_with("SSH-2.0-tdssh"));
    assert_eq!(children_of(server.child.id()).len(), 1);
    drop(stream);
    let start = Instant::now();
    wait_for("the monitor to end", || {
        children_of(server.child.id()).is_empty()
    });
    assert!(
        start.elapsed() < Duration::from_secs(10),
        "{:?}",
        start.elapsed()
    );
}

#[test]
fn eight_failed_logins_do_not_lock_out_the_source() {
    let server = start_with_grace(LONG_GRACE);
    for _ in 0..8 {
        let output = server
            .ssh("stranger")
            .arg(format!("{USER}@127.0.0.1"))
            .arg("true")
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(255), "{}", stderr(&output));
    }
    // Their monitors end just after their clients do.
    wait_for("the right key to log in", || {
        stdout(&server.run("echo in")) == "in\n"
    });
}

#[test]
fn too_many_failures_end_the_connection_with_the_clients_message() {
    let server = start_with_grace(LONG_GRACE);
    let stranger = load_key(&server.root.join("stranger"));
    let mut client = RawClient::connect(server.port);
    client.start_auth();
    assert_eq!(client.offer(&stranger)[0], msg::USERAUTH_FAILURE);
    assert_eq!(client.offer(&stranger)[0], msg::USERAUTH_FAILURE);
    let bye = client.offer(&stranger);
    assert_eq!(bye[0], msg::DISCONNECT);
    let mut reader = Reader::new(&bye[1..]);
    reader.u32().unwrap();
    assert_eq!(reader.str().unwrap(), "Too many authentication failures");
    assert!(client.reader.read_packet().is_err());
    wait_for("the monitor to end", || {
        children_of(server.child.id()).is_empty()
    });
}

/// Each would admit everyone today if the line were left to match on its own.
#[test]
fn authorized_keys_lines_that_could_widen_access_are_refused_whole() {
    let server = start("");
    for prefix in [
        "from=\"!127.0.0.0/8,*\" ",
        "from=\"!localhost,*\" ",
        "from=\"10.9.9.9\",from=\"*\" ",
        "from=\"*\",from=\"10.9.9.9\" ",
    ] {
        server.set_authorized_line(prefix);
        assert_eq!(server.run("true").status.code(), Some(255), "{prefix}");
    }
    server.set_authorized_line("from=\"*,!10.9.9.9\" ");
    assert_eq!(server.run("true").status.code(), Some(0));
}

/// A dual-stack listener reports an IPv4 peer as `::ffff:127.0.0.1`.
#[test]
fn a_mapped_peer_is_judged_and_reported_as_its_ipv4_address() {
    if TcpListener::bind("[::]:0").is_err() {
        return;
    }
    let server = start_with(&Options {
        listen: "::",
        umask_zero: false,
        extra: "",
        login_grace: SHORT_GRACE,
    });
    if TcpStream::connect(("127.0.0.1", server.port)).is_err() {
        return;
    }
    server.set_authorized_line("from=\"127.0.0.1\" ");
    let output = server.run("echo \"$SSH_CONNECTION\"");
    assert!(
        stdout(&output).starts_with("127.0.0.1 "),
        "{}{}",
        stdout(&output),
        server.log()
    );
}

#[test]
fn a_line_that_is_not_utf8_leaves_the_key_file_usable() {
    let server = start("");
    let mut bytes = b"# caf\xe9 is latin-1\nssh-ed25519 \xff\xfe bad\n".to_vec();
    bytes.extend_from_slice(&fs::read(server.root.join("client.pub")).unwrap());
    fs::write(server.home.join(".ssh/authorized_keys"), bytes).unwrap();
    assert_eq!(
        server.run("true").status.code(),
        Some(0),
        "{}",
        server.log()
    );
}

#[test]
fn a_missing_key_file_is_ordinary_and_not_a_strict_modes_refusal() {
    let server = start("");
    fs::remove_file(server.home.join(".ssh/authorized_keys")).unwrap();
    assert_eq!(server.run("true").status.code(), Some(255));
    let log = server.log();
    assert!(!log.contains("authorized keys for"), "{log}");
}

const FORGED: &str = "Accepted publickey for root from 6.6.6.6 port 1 ssh2: ED25519 SHA256:forged";

fn forged_line_in(log: &str) -> bool {
    log.lines()
        .any(|line| line.starts_with("Accepted publickey for root"))
}

/// An account's symlink target is what the refusal's message names.
#[test]
fn a_key_files_path_cannot_forge_a_log_line() {
    let server = start("Match User tester\n\tAuthorizedKeysFile {root}/link\n");
    let target = server.root.join(format!("x\n{FORGED}"));
    fs::write(&target, b"").unwrap();
    fs::set_permissions(&target, fs::Permissions::from_mode(0o666)).unwrap();
    symlink(&target, server.root.join("link")).unwrap();
    assert_eq!(server.run("true").status.code(), Some(255));
    let log = server.log();
    assert!(log.contains("bad ownership or modes"), "{log}");
    assert!(!forged_line_in(&log), "{log}");
}

/// Unauthenticated, in clear, before anything else has been said.
#[test]
fn a_kexinit_cannot_forge_a_log_line() {
    let server = start("");
    let mut stream = TcpStream::connect(("127.0.0.1", server.port)).unwrap();
    assert!(read_banner(&mut stream).starts_with("SSH-2.0-tdssh"));
    stream.write_all(b"SSH-2.0-test\r\n").unwrap();
    let mut message = Writer::message(msg::KEXINIT);
    message.raw(&[0; 16]);
    message.str(&format!("nothing\n{FORGED},kex-strict-c-v00@openssh.com"));
    for list in [
        "ssh-ed25519",
        "chacha20-poly1305@openssh.com",
        "chacha20-poly1305@openssh.com",
        "none",
        "none",
        "none",
        "none",
        "",
        "",
    ] {
        message.str(list);
    }
    message.bool(false).u32(0);
    let mut writer = PacketWriter::new(stream.try_clone().unwrap());
    writer.write_packet(message.as_slice()).unwrap();
    let start = Instant::now();
    while !server.log().contains("no matching key exchange method") {
        assert!(start.elapsed() < DEADLINE, "{}", server.log());
        std::thread::sleep(Duration::from_millis(10));
    }
    let log = server.log();
    assert!(!forged_line_in(&log), "{log}");
}

#[test]
fn an_unknown_message_before_authentication_is_unimplemented_and_not_fatal() {
    let server = start("");
    let mut client = RawClient::connect(server.port);
    client.send(&Writer::message(200));
    let seq = client.writer.counters().seq.wrapping_sub(1);
    // Under strict KEX the sequence restarted at NEWKEYS, so it is a small number.
    assert!(seq < 4, "{seq}");
    let reply = client.receive();
    assert_eq!(reply[0], msg::UNIMPLEMENTED);
    assert_eq!(Reader::new(&reply[1..]).u32().unwrap(), seq);
    // And the connection goes on to a login.
    let key = load_key(&server.root.join("client"));
    client.login(&key);
}

/// Exactly what truncated when the drain clock ran through a wait for credit: the
/// window's 2 MiB and 10000 bytes more, with the client not reading.
#[test]
fn a_stalled_reader_on_a_pty_loses_nothing_after_the_child_exits() {
    const PIECE: usize = 14_633;
    const PIECES: usize = 144;
    let server = start("");
    let child = server
        .ssh("client")
        .arg("-tt")
        .arg(format!("{USER}@127.0.0.1"))
        .arg(format!(
            "s={}; i=0; while test $i -lt {PIECES}; do printf %s \"$s\"; i=$((i+1)); done",
            "x".repeat(PIECE)
        ))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Long past the drain, with the child gone and the source waiting for credit.
    std::thread::sleep(Duration::from_millis(1500));
    let output = child.wait_with_output().unwrap();
    assert_eq!(
        output.stdout.len(),
        PIECE * PIECES,
        "{}{}",
        stderr(&output),
        server.log()
    );
}

/// The file SFTP and a command each make with no mode of their own.
#[test]
fn nothing_created_is_group_or_other_writable_under_an_inherited_umask_of_zero() {
    let server = start_with(&Options {
        listen: "127.0.0.1",
        umask_zero: true,
        extra: "",
        login_grace: SHORT_GRACE,
    });
    assert_eq!(
        server.run("echo x > from-command").status.code(),
        Some(0),
        "{}",
        server.log()
    );
    let mode = fs::metadata(server.home.join("from-command"))
        .unwrap()
        .mode();
    assert_eq!(mode & 0o022, 0, "{mode:o}");

    let mut client = RawClient::connect(server.port);
    client.login(&load_key(&server.root.join("client")));
    let channel = client.start_sftp();
    let mut init = Writer::message(1);
    init.u32(3);
    assert_eq!(client.sftp_call(channel, &init), 2);
    // FX_OPEN, PF_WRITE | PF_CREAT, an ATTRS with no flags: FX_HANDLE back.
    let mut open = Writer::message(3);
    open.u32(1).str("from-sftp").u32(2 | 8).u32(0);
    assert_eq!(client.sftp_call(channel, &open), 102);
    let mode = fs::metadata(server.home.join("from-sftp")).unwrap().mode();
    assert_eq!(mode & 0o022, 0, "{mode:o}");
    // A mode that asks for all of it gets what the floor leaves, by the one way the
    // floor is asked: FX_OPEN and FX_MKDIR with SSH_FILEXFER_ATTR_PERMISSIONS.
    const ATTR_PERMISSIONS: u32 = 4;
    let mut open = Writer::message(3);
    open.u32(2)
        .str("wide-open-sftp")
        .u32(2 | 8)
        .u32(ATTR_PERMISSIONS)
        .u32(0o777);
    assert_eq!(client.sftp_call(channel, &open), 102);
    let mode = fs::metadata(server.home.join("wide-open-sftp"))
        .unwrap()
        .mode();
    assert_eq!(mode & 0o777, 0o755, "{mode:o}");
    let mut mkdir = Writer::message(14);
    mkdir
        .u32(3)
        .str("wide-open-dir")
        .u32(ATTR_PERMISSIONS)
        .u32(0o777);
    // FX_STATUS, whose code the file's being there shows.
    assert_eq!(client.sftp_call(channel, &mkdir), 101);
    let mode = fs::metadata(server.home.join("wide-open-dir"))
        .unwrap()
        .mode();
    assert_eq!(mode & 0o777, 0o755, "{mode:o}");
    // With no attributes the same.
    let mut mkdir = Writer::message(14);
    mkdir.u32(4).str("plain-dir").u32(0);
    assert_eq!(client.sftp_call(channel, &mkdir), 101);
    let mode = fs::metadata(server.home.join("plain-dir")).unwrap().mode();
    assert_eq!(mode & 0o777, 0o755, "{mode:o}");
}

/// Enough, at a program writing as fast as a shell can, to pass what a rekey used
/// to be allowed to hold back.
const BIG_OUTPUT: &str =
    "s=x; i=0; while test $i -lt 16; do s=$s$s; i=$((i+1)); done; while :; do echo $s; done";

impl RawClient {
    fn open_session(&mut self, window: u32) -> u32 {
        let mut open = Writer::message(msg::CHANNEL_OPEN);
        open.str("session").u32(0).u32(window).u32(1 << 15);
        self.send(&open);
        let confirmation = self.receive_skipping_window();
        assert_eq!(confirmation[0], msg::CHANNEL_OPEN_CONFIRMATION);
        let mut reader = Reader::new(&confirmation[1..]);
        reader.u32().unwrap();
        reader.u32().unwrap()
    }

    fn exec(&mut self, channel: u32, command: &str, want_reply: bool) {
        let mut request = Writer::message(msg::CHANNEL_REQUEST);
        request
            .u32(channel)
            .str("exec")
            .bool(want_reply)
            .str(command);
        self.send(&request);
    }
}

/// The server fills the core's socket, which this never reads, and then gets every
/// IGNORE it will take. Before the budget that was 4096 of them, a quarter of a GiB.
#[test]
fn a_flood_behind_a_blocked_core_is_held_to_the_budget() {
    let server = start_with_grace(LONG_GRACE);
    let mut client = RawClient::connect(server.port);
    client.login(&load_key(&server.root.join("client")));
    let channel = client.open_session(u32::MAX);
    client.exec(channel, BIG_OUTPUT, false);
    std::thread::sleep(Duration::from_millis(500));
    client
        .writer
        .get_mut()
        .set_write_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    const REFUSED_BY: usize = 48 * 1024 * 1024;
    let mut flood = Writer::message(msg::IGNORE);
    flood.string(&[0; 60_000]);
    let mut sent = 0;
    while sent < REFUSED_BY && client.writer.write_packet(flood.as_slice()).is_ok() {
        sent += flood.as_slice().len();
    }
    assert!(sent < REFUSED_BY, "{sent} bytes taken while blocked");
}

/// A connection with far more outstanding than a rekey may hold back loses nothing
/// when a rekey takes its time, and goes on when it is over.
#[test]
fn a_slow_rekey_does_not_cut_a_busy_connection() {
    let server = start_with_grace(LONG_GRACE);
    let mut client = RawClient::connect(server.port);
    client.login(&load_key(&server.root.join("client")));
    let channel = client.open_session(u32::MAX);
    client.exec(channel, BIG_OUTPUT, false);
    let is_data = |packet: &[u8]| packet[0] == msg::CHANNEL_DATA;
    assert!(is_data(&client.receive_skipping_window()));
    let mut engine = Kex::resume(
        Role::Client,
        IDENTIFICATION,
        &client.server_version,
        client.session_id,
    );
    let mut env = ClientEnv(RandomSource(fs::File::open("/dev/urandom").unwrap()));
    let init = engine.start(&mut env).unwrap();
    client.writer.write_packet(&init).unwrap();
    // The source has every reason to go on, and time to pass the old bound.
    let held = Instant::now();
    let mut peer_init = None;
    while held.elapsed() < Duration::from_secs(2) {
        if peer_init.is_none() {
            let packet = client.reader.read_packet().unwrap();
            assert_ne!(packet[0], msg::DISCONNECT, "{}", server.log());
            if packet[0] == msg::KEXINIT {
                peer_init = Some(packet);
            }
        } else {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    let mut pending = peer_init.expect("the server joined the rekey");
    while !conn::kex_step(
        &mut client.reader,
        &mut client.writer,
        &mut engine,
        &mut env,
        &pending,
    )
    .unwrap()
    {
        pending = client.reader.read_packet().unwrap();
        assert_ne!(pending[0], msg::DISCONNECT, "{}", server.log());
    }
    // Everything that was held goes out first, and then the source goes on.
    let mut bytes = 0;
    while bytes < 32 * 1024 * 1024 {
        let packet = client.reader.read_packet().unwrap();
        assert_ne!(packet[0], msg::DISCONNECT, "{}", server.log());
        if is_data(&packet) {
            bytes += packet.len();
        }
    }
}

/// What a failed exploit looks like has to be visible, and the signals the monitor
/// sends itself or a peer's orderly goodbye do not.
#[test]
fn a_worker_dying_by_a_signal_is_logged() {
    if !test_command("/bin/sh")
        .args(["-c", "command -v kill"])
        .stdout(Stdio::null())
        .status()
        .unwrap()
        .success()
    {
        eprintln!("skipped: this host has no kill for a shell to run");
        return;
    }
    let server = start_with_grace(LONG_GRACE);
    let mut stream = TcpStream::connect(("127.0.0.1", server.port)).unwrap();
    read_banner(&mut stream);
    let monitor = children_of(server.child.id());
    assert_eq!(monitor.len(), 1, "{monitor:?}");
    let worker = children_of(monitor[0].parse().unwrap());
    assert_eq!(worker.len(), 1, "{worker:?}");
    assert!(test_command("/bin/sh")
        .args(["-c", "kill -SEGV \"$1\"", "killer", &worker[0]])
        .status()
        .unwrap()
        .success());
    // std's handler looks at a SIGSEGV first, and may meet the filter doing so.
    wait_for("the log's line", || server.log().contains("died by signal"));
}

#[test]
fn a_peers_orderly_goodbye_before_authentication_is_no_error_line() {
    let server = start_with_grace(LONG_GRACE);
    let mut client = RawClient::connect(server.port);
    let mut goodbye = Writer::message(msg::DISCONNECT);
    goodbye.u32(11).str("bye").str("");
    client.send(&goodbye);
    let monitor = children_of(server.child.id());
    wait_for("the connection's end", || {
        children_of(server.child.id()).is_empty()
    });
    assert!(
        !server.log().contains("pre-auth:"),
        "{monitor:?} {}",
        server.log()
    );
    assert!(!server.log().contains("died by"), "{}", server.log());
}

/// `ssh h yes | head -1`: the client says eow, and the program gets its SIGPIPE.
#[test]
fn a_client_whose_stdout_closes_ends_the_program_writing_to_it() {
    let server = start("");
    let mut child = server
        .ssh("client")
        .arg(format!("{USER}@127.0.0.1"))
        .arg("while :; do echo a line of output; done")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let mut first = [0u8; 16];
    stdout.read_exact(&mut first).unwrap();
    drop(stdout);
    let start = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if start.elapsed() > DEADLINE {
            let _ = child.kill();
            panic!("it never ended: {}", server.log());
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    // The program died of the signal, which reads as a failure, as OpenSSH's does.
    assert_eq!(status.code(), Some(255));
}

/// Eow stops stdout only. The program goes on, what it writes to stderr still comes,
/// and then how it ended: a window shut on the dead output would have held both.
#[test]
fn stderr_written_after_eow_arrives_and_then_the_status() {
    let server = start("");
    let mut client = RawClient::connect(server.port);
    client.login(&load_key(&server.root.join("client")));
    let channel = client.open_session(1 << 20);
    client.exec(
        channel,
        "trap '' PIPE; while echo a line of output; do :; done; echo said late >&2; exit 3",
        false,
    );
    while client.receive_skipping_window()[0] != msg::CHANNEL_DATA {}
    let mut eow = Writer::message(msg::CHANNEL_REQUEST);
    eow.u32(channel).str("eow@openssh.com").bool(false);
    client.send(&eow);
    let mut stderr = Vec::new();
    let status = loop {
        let packet = client.receive_skipping_window();
        let mut reader = Reader::new(&packet[1..]);
        reader.u32().unwrap();
        match packet[0] {
            msg::CHANNEL_DATA => {}
            msg::CHANNEL_EXTENDED_DATA => {
                reader.u32().unwrap();
                stderr.extend_from_slice(reader.string().unwrap());
            }
            msg::CHANNEL_REQUEST => {
                assert_eq!(reader.str().unwrap(), "exit-status");
                reader.bool().unwrap();
                break reader.u32().unwrap();
            }
            msg::CHANNEL_EOF => {}
            other => panic!("message {other}: {}", server.log()),
        }
    };
    assert_eq!(status, 3, "{}", server.log());
    assert!(stderr.ends_with(b"said late\n"), "{stderr:?}");
}

/// The core is blocked writing output to a client that reads none, and it half closes.
/// The reader's end of connection would otherwise wait behind that write for its limit.
#[test]
fn a_client_gone_ends_a_core_blocked_in_a_write() {
    let server = start_with_grace(LONG_GRACE);
    let mut client = RawClient::connect(server.port);
    client.login(&load_key(&server.root.join("client")));
    let channel = client.open_session(u32::MAX);
    client.exec(channel, BIG_OUTPUT, false);
    std::thread::sleep(Duration::from_millis(500));
    assert!(!children_of(server.child.id()).is_empty());
    client
        .writer
        .get_mut()
        .shutdown(std::net::Shutdown::Write)
        .unwrap();
    wait_for("the session's end", || {
        children_of(server.child.id()).is_empty()
    });
}

/// A key of small order in `authorized_keys` would take `(R = identity, S = 0)` on
/// any session. The worker turns it away before the monitor hears of it.
#[test]
fn a_small_order_key_in_authorized_keys_admits_no_one() {
    let server = start("");
    let mut identity = [0u8; 32];
    identity[0] = 1;
    let public = PublicKey {
        key: identity,
        comment: String::new(),
    };
    fs::write(server.home.join(".ssh/authorized_keys"), public.to_line()).unwrap();
    let mut universal = [0u8; 64];
    universal[0] = 1;
    let mut client = RawClient::connect(server.port);
    client.start_auth();
    let mut request = Writer::message(msg::USERAUTH_REQUEST);
    request
        .str(USER)
        .str("ssh-connection")
        .str("publickey")
        .bool(true)
        .str("ssh-ed25519")
        .string(&public.blob())
        .string(&kex::ed25519_signature_blob(&universal));
    client.send(&request);
    assert_eq!(client.receive()[0], msg::USERAUTH_FAILURE);
}

#[test]
fn an_argument_that_is_not_utf8_is_refused_with_status_one() {
    let not_text = OsStr::from_bytes(b"\xff");
    for args in [&[not_text][..], &[OsStr::new("-f"), not_text]] {
        let output = test_command(SSHD).args(args).output().unwrap();
        assert_eq!(output.status.code(), Some(1), "{output:?}");
        let text = String::from_utf8_lossy(&output.stderr);
        assert!(text.contains("not UTF-8"), "{text}");
    }
}

/// With no `-f` the path is the program's own choice, so its directory is too.
#[test]
fn keygen_makes_a_private_dot_ssh_for_the_default_path_alone() {
    let dir = TempDir::new("td-ssh-keygen-home").unwrap();
    let home = dir.path().join("home");
    fs::create_dir(&home).unwrap();
    let output = test_command(KEYGEN)
        .args(["-q", "-t", "ed25519", "-N", ""])
        .env("HOME", &home)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let made = fs::metadata(home.join(".ssh")).unwrap();
    assert_eq!(made.mode() & 0o777, 0o700);
    assert!(home.join(".ssh/id_ed25519").exists());
    // A directory that is there is left as it is.
    fs::remove_file(home.join(".ssh/id_ed25519")).unwrap();
    fs::remove_file(home.join(".ssh/id_ed25519.pub")).unwrap();
    fs::set_permissions(home.join(".ssh"), fs::Permissions::from_mode(0o750)).unwrap();
    let output = test_command(KEYGEN)
        .args(["-q", "-t", "ed25519", "-N", ""])
        .env("HOME", &home)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let kept = fs::metadata(home.join(".ssh")).unwrap();
    assert_eq!(kept.mode() & 0o777, 0o750);
    // One the caller named is theirs to have prepared.
    let output = test_command(KEYGEN)
        .args(["-q", "-t", "ed25519", "-N", "", "-f"])
        .arg(dir.path().join("absent/key"))
        .output()
        .unwrap();
    assert!(!output.status.success(), "{output:?}");
    assert!(!dir.path().join("absent").exists());
}

#[test]
fn keygen_writes_a_key_at_a_path_that_is_not_utf8() {
    let dir = TempDir::new("td-ssh-keygen-bytes").unwrap();
    let mut name = dir.path().as_os_str().as_bytes().to_vec();
    name.extend_from_slice(b"/k\xff");
    let path = PathBuf::from(OsStr::from_bytes(&name));
    let status = test_command(KEYGEN)
        .args(["-q", "-t", "ed25519", "-N", "", "-f"])
        .arg(&path)
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(0));
    assert!(path.exists());
}

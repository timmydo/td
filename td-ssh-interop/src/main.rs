//! Real logins between td-ssh and stock OpenSSH (td-ssh/DESIGN.md section 9). It
//! runs as one process because each `store-ns` is its own network namespace: both
//! ends of a connection must start from here. A wrong KEX or AEAD cannot pass this.
//! It also hands the real td-sshd every form of the image's rendered policy, since
//! that daemon refuses a whole file for one keyword it does not know.

#[path = "../../td-firstboot/src/ssh_policy.rs"]
mod ssh_policy;

use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const MARKER: &str = "TD-SSH-INTEROP-OK";
const KEX: &[&str] = &["mlkem768x25519-sha256", "curve25519-sha256"];
const PASSPHRASE: &str = "correct horse";
const ASKPASS: &str = "askpass";
const FALLBACK_USER: &str = "tester";
const WATCHDOG: Duration = Duration::from_secs(300);
const START_DEADLINE: Duration = Duration::from_secs(20);
const ALLOWED_SIGNER: &str = "signer@td";
const NAMESPACE: &str = "file";
/// Past several of the rekey limits below, in both directions.
const TRANSFER_BYTES: usize = 4 * 1024 * 1024;
const DIGEST_FLAG: &str = "--digest";
const EMIT_FLAG: &str = "--emit";
const FOREVER_FLAG: &str = "--forever";
/// What each multiplexed session sends, and how many there are: together more
/// outstanding than a rekey may hold back, on a connection that keeps rekeying.
const MUX_BYTES: usize = 8 * 1024 * 1024;
const MUX_SESSIONS: usize = 6;
/// A program whose output nobody reads any more ends this soon, or the leg fails.
const EARLY_CLOSE_DEADLINE: Duration = Duration::from_secs(20);
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
/// Rekeys at this limit each take a signature from td-sshd's rate-limited signer:
/// 256K here spent 127 s of a 131 s phase waiting for it.
const UPLOAD_REKEY_LIMIT: &str = "RekeyLimit=1M";
const DOWNLOAD_REKEY_LIMIT: &str = "RekeyLimit=128K";
/// What each client's `-v` says of a key exchange and of its rekeys.
const KEX_LINE: &str = "kex: algorithm: ";
const TD_REKEYED: &str = "rekeyed ";
const STOCK_NEW_KEYS: &str = "SSH2_MSG_NEWKEYS sent";
/// The primary account the image's policy is rendered for.
const POLICY_PRIMARY: &str = "alice";

/// Bytes that are not their own neighbours, so a dropped or repeated block shows.
fn pattern(length: usize) -> Vec<u8> {
    let mut state = FNV_OFFSET;
    (0..length)
        .map(|_| {
            state = state.wrapping_mul(FNV_PRIME) ^ (state >> 29);
            state.to_le_bytes().first().copied().unwrap_or(0)
        })
        .collect()
}

fn fnv(state: u64, bytes: &[u8]) -> u64 {
    bytes.iter().fold(state, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(FNV_PRIME)
    })
}

fn digest_line(bytes: &[u8]) -> String {
    format!("{} {:016x}", bytes.len(), fnv(FNV_OFFSET, bytes))
}

/// The far end of a transfer, run through the login under test.
fn far_end(mode: &str, argument: Option<&str>) -> Result<(), String> {
    if mode == FOREVER_FLAG {
        let piece = pattern(32 * 1024);
        let mut stdout = std::io::stdout().lock();
        loop {
            stdout
                .write_all(&piece)
                .map_err(|error| error.to_string())?;
        }
    }
    if mode == EMIT_FLAG {
        let length: usize = argument
            .and_then(|value| value.parse().ok())
            .ok_or("--emit needs a length")?;
        return std::io::stdout()
            .write_all(&pattern(length))
            .map_err(|error| error.to_string());
    }
    let mut bytes = Vec::new();
    std::io::stdin()
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    println!("{}", digest_line(&bytes));
    Ok(())
}

struct Tools {
    ssh: PathBuf,
    sshd: PathBuf,
    ssh_keygen: PathBuf,
    scp: PathBuf,
    sftp: PathBuf,
    sshd_session: PathBuf,
    sshd_auth: PathBuf,
    td_ssh: PathBuf,
    td_sshd: PathBuf,
    td_keygen: PathBuf,
    shell: PathBuf,
}

/// Shared with the watchdog, which has the last word on a run that hangs.
#[derive(Clone)]
struct Report {
    passed: Arc<AtomicU32>,
    failed: Arc<Mutex<Vec<String>>>,
}

impl Report {
    fn new() -> Self {
        Self {
            passed: Arc::new(AtomicU32::new(0)),
            failed: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn check(&mut self, name: &str, passed: bool, detail: impl FnOnce() -> String) {
        if passed {
            self.passed.fetch_add(1, Ordering::SeqCst);
            println!("ok   {name}");
        } else {
            println!("FAIL {name}");
            let line = format!("{name}: {}", detail());
            if let Ok(mut failed) = self.failed.lock() {
                failed.push(line);
            }
        }
    }

    fn passed(&self) -> u32 {
        self.passed.load(Ordering::SeqCst)
    }

    fn failures(&self) -> Vec<String> {
        self.failed.lock().map_or_else(
            |_| vec!["the failures could not be read".to_string()],
            |failed| failed.clone(),
        )
    }
}

/// Every key exchange the client's `-v` said it ran, in order.
fn negotiated(stderr: &str) -> Vec<&str> {
    stderr
        .lines()
        .filter_map(|line| line.split(KEX_LINE).nth(1))
        .map(|rest| rest.split_whitespace().next().unwrap_or(""))
        .collect()
}

/// Rekeys the client's `-v` says it completed: td-ssh counts them at its end, and
/// stock says each `NEWKEYS` it sends, the first of them for the initial exchange.
fn rekeys(stderr: &str) -> u32 {
    let stock = stderr
        .lines()
        .filter(|line| line.contains(STOCK_NEW_KEYS))
        .count();
    let td = stderr.lines().find_map(|line| {
        line.split(TD_REKEYED)
            .nth(1)?
            .split_whitespace()
            .next()?
            .parse::<u32>()
            .ok()
    });
    td.unwrap_or_else(|| u32::try_from(stock.saturating_sub(1)).unwrap_or(u32::MAX))
}

fn saying(output: &Output, wanted: &str) -> bool {
    text(&output.stderr).contains(wanted)
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn describe(output: &Output) -> String {
    format!(
        "status {:?}, stdout {:?}, stderr {:?}",
        output.status.code(),
        text(&output.stdout),
        text(&output.stderr)
    )
}

fn run(command: &mut Command) -> Result<Output, String> {
    command
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("spawn {command:?}: {error}"))
}

fn run_with_input(command: &mut Command, input: &[u8]) -> Result<Output, String> {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("spawn {command:?}: {error}"))?;
    let mut stdin = child.stdin.take().ok_or("no stdin")?;
    let owned = input.to_vec();
    let writer = std::thread::spawn(move || {
        // The peer may rightly stop reading: its exit status decides.
        let _ = stdin.write_all(&owned);
    });
    let output = child
        .wait_with_output()
        .map_err(|error| error.to_string())?;
    let _ = writer.join();
    Ok(output)
}

fn must(what: &str, output: Output) -> Result<Output, String> {
    if output.status.success() {
        Ok(output)
    } else {
        Err(format!("{what}: {}", describe(&output)))
    }
}

/// The caller's account, added to this namespace's `/etc/passwd` if the bare root
/// has none: stock `ssh` and `sshd` both refuse an unknown uid.
fn account(home: &Path, shell: &Path) -> Result<String, String> {
    let uid = fs::metadata("/proc/self")
        .map_err(|error| format!("/proc/self: {error}"))?
        .uid();
    let gid = fs::metadata("/proc/self")
        .map_err(|error| format!("/proc/self: {error}"))?
        .gid();
    if let Ok(passwd) = fs::read_to_string("/etc/passwd") {
        for line in passwd.lines() {
            let mut fields = line.split(':');
            let name = fields.next();
            if fields.nth(1).and_then(|field| field.parse::<u32>().ok()) == Some(uid) {
                if let Some(name) = name {
                    return Ok(name.to_string());
                }
            }
        }
    }
    fs::create_dir_all("/etc").map_err(|error| format!("mkdir /etc: {error}"))?;
    let mut passwd = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("/etc/passwd")
        .map_err(|error| format!("open /etc/passwd: {error}"))?;
    writeln!(
        passwd,
        "{FALLBACK_USER}:x:{uid}:{gid}::{}:{}",
        home.display(),
        shell.display()
    )
    .map_err(|error| format!("write /etc/passwd: {error}"))?;
    Ok(FALLBACK_USER.to_string())
}

/// `[stock keygen]` and `[td keygen]` produce keys the other reads, with the same
/// fingerprint, and each verifies the other's SSHSIG. td reads a stock encrypted key
/// too, and writes none.
fn crossed_key_tools(
    tools: &Tools,
    work: &Path,
    report: &mut Report,
) -> Result<(PathBuf, PathBuf), String> {
    let stock_key = work.join("stock_key");
    let stock_encrypted = work.join("stock_encrypted");
    let td_key = work.join("td_key");
    for (generator, path, passphrase) in [
        // Plain, so each client can load and sign with it: a refusal then says the
        // server looked at the key, not that the client could not use it.
        (&tools.ssh_keygen, &work.join(STRANGER_KEY), ""),
        (&tools.ssh_keygen, &stock_key, ""),
        (&tools.ssh_keygen, &stock_encrypted, PASSPHRASE),
        (&tools.td_keygen, &td_key, ""),
    ] {
        must(
            "generate a key",
            run(Command::new(generator)
                .args([
                    "-q", "-t", "ed25519", "-N", passphrase, "-C", "interop", "-f",
                ])
                .arg(path))?,
        )?;
    }
    let public = |path: &Path| -> Result<String, String> {
        let mut name = path.as_os_str().to_os_string();
        name.push(".pub");
        fs::read_to_string(&name).map_err(|error| format!("read {name:?}: {error}"))
    };
    let first_two = |line: &str| {
        line.split_whitespace()
            .take(2)
            .collect::<Vec<_>>()
            .join(" ")
    };
    let askpass = write_askpass(tools, work)?;
    for (name, reader, path) in [
        ("td reads a stock key", &tools.td_keygen, &stock_key),
        (
            "td reads a stock encrypted key",
            &tools.td_keygen,
            &stock_encrypted,
        ),
        ("stock reads a td key", &tools.ssh_keygen, &td_key),
    ] {
        let output = run(Command::new(reader)
            .args(["-y", "-f"])
            .arg(path)
            .env("SSH_ASKPASS", &askpass)
            .env("SSH_ASKPASS_REQUIRE", "force")
            .stdin(Stdio::null()))?;
        let wanted = first_two(&public(path)?);
        report.check(
            name,
            output.status.success() && first_two(&text(&output.stdout)) == wanted,
            || format!("wanted {wanted}: {}", describe(&output)),
        );
    }
    let fingerprint = |keygen: &Path, path: &Path| -> Result<String, String> {
        let output = must(
            "fingerprint",
            run(Command::new(keygen).args(["-l", "-f"]).arg(path))?,
        )?;
        Ok(first_two(&text(&output.stdout)))
    };
    let protected = work.join("td_protected");
    let output = run(Command::new(&tools.td_keygen)
        .args(["-q", "-t", "ed25519", "-N", PASSPHRASE, "-f"])
        .arg(&protected))?;
    report.check(
        "td keygen refuses to write a protected key",
        !output.status.success()
            && saying(&output, "only unprotected keys")
            && fs::symlink_metadata(&protected).is_err(),
        || describe(&output),
    );
    let stock_print = fingerprint(&tools.ssh_keygen, &td_key)?;
    let td_print = fingerprint(&tools.td_keygen, &td_key)?;
    report.check(
        "both tools print one fingerprint",
        stock_print == td_print,
        || format!("{stock_print} against {td_print}"),
    );

    let message = work.join("message");
    fs::write(&message, b"signed by each, verified by the other\n")
        .map_err(|error| error.to_string())?;
    let signature = work.join("message.sig");
    for (name, signer, key, verifier) in [
        (
            "stock verifies td's SSHSIG",
            &tools.td_keygen,
            &td_key,
            &tools.ssh_keygen,
        ),
        (
            "td verifies stock's SSHSIG",
            &tools.ssh_keygen,
            &stock_key,
            &tools.td_keygen,
        ),
    ] {
        let _ = fs::remove_file(&signature);
        must(
            name,
            run(Command::new(signer)
                .args(["-q", "-Y", "sign", "-n", NAMESPACE, "-f"])
                .arg(key)
                .arg(&message))?,
        )?;
        let allowed = work.join("allowed_signers");
        fs::write(&allowed, format!("{ALLOWED_SIGNER} {}", public(key)?))
            .map_err(|error| error.to_string())?;
        let verify = |input: &[u8]| {
            run_with_input(
                Command::new(verifier)
                    .args([
                        "-q",
                        "-Y",
                        "verify",
                        "-n",
                        NAMESPACE,
                        "-I",
                        ALLOWED_SIGNER,
                        "-f",
                    ])
                    .arg(&allowed)
                    .arg("-s")
                    .arg(&signature),
                input,
            )
        };
        let content = fs::read(&message).map_err(|error| error.to_string())?;
        let good = verify(&content)?;
        report.check(name, good.status.success(), || describe(&good));
        let bad = verify(b"not the message\n")?;
        report.check(
            &format!("{name}, refusing another message"),
            !bad.status.success(),
            || describe(&bad),
        );
    }
    Ok((stock_key, td_key))
}

/// A plain key nothing authorizes.
const STRANGER_KEY: &str = "stranger_key";

struct Daemon {
    child: Child,
    port: u16,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn start_td_sshd(
    tools: &Tools,
    work: &Path,
    home: &Path,
    user: &str,
    client_public: &str,
) -> Result<Daemon, String> {
    let dot_ssh = home.join(".ssh");
    fs::create_dir_all(&dot_ssh).map_err(|error| error.to_string())?;
    let authorized = dot_ssh.join("authorized_keys");
    fs::write(&authorized, client_public).map_err(|error| error.to_string())?;
    fs::set_permissions(&authorized, fs::Permissions::from_mode(0o600))
        .map_err(|error| error.to_string())?;
    // An existing OpenSSH-format host key stays valid: this one is OpenSSH's.
    let host_key = work.join("td_sshd_host");
    must(
        "generate td-sshd's host key",
        run(Command::new(&tools.ssh_keygen)
            .args(["-q", "-t", "ed25519", "-N", "", "-f"])
            .arg(&host_key))?,
    )?;
    let config = format!(
        "Port 0\nListenAddress 127.0.0.1\nHostKey {}\nAuthorizedKeysFile .ssh/authorized_keys\nSubsystem sftp internal-sftp\nPermitRootLogin no\nLoginGraceTime 30\nMaxAuthTries 3\nMaxSessions {}\n",
        host_key.display(),
        MUX_SESSIONS + 2,
    );
    spawn_td_sshd(tools, work, "td_sshd", &config, home, user)
}

/// `name` keeps one daemon's files from another's.
fn spawn_td_sshd(
    tools: &Tools,
    work: &Path,
    name: &str,
    config: &str,
    home: &Path,
    user: &str,
) -> Result<Daemon, String> {
    let file = work.join(format!("{name}_config"));
    fs::write(&file, config).map_err(|error| error.to_string())?;
    let log = work.join(format!("{name}.log"));
    let child = Command::new(&tools.td_sshd)
        .arg("-f")
        .arg(&file)
        .args(["--single-user", user])
        .env("HOME", home)
        .env("SHELL", &tools.shell)
        .stdin(Stdio::null())
        .stderr(fs::File::create(&log).map_err(|error| error.to_string())?)
        .spawn()
        .map_err(|error| format!("spawn td-sshd: {error}"))?;
    let mut daemon = Daemon { child, port: 0 };
    let start = Instant::now();
    loop {
        let seen = fs::read_to_string(&log).unwrap_or_default();
        if let Some(rest) = seen.split("listening on port ").nth(1) {
            daemon.port = rest
                .split_whitespace()
                .next()
                .and_then(|port| port.parse().ok())
                .ok_or_else(|| format!("bad port in {seen:?}"))?;
            return Ok(daemon);
        }
        if start.elapsed() > START_DEADLINE {
            return Err(format!("td-sshd announced no port: {seen:?}"));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn free_port() -> Result<u16, String> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).map_err(|error| error.to_string())?;
    let port = listener
        .local_addr()
        .map_err(|error| error.to_string())?
        .port();
    Ok(port)
}

/// `name` keeps one daemon's files from another's, and `extra` ends its policy.
fn start_stock_sshd(
    tools: &Tools,
    work: &Path,
    client_public: &str,
    name: &str,
    extra: &str,
) -> Result<Daemon, String> {
    let authorized = work.join(format!("{name}_authorized_keys"));
    fs::write(&authorized, client_public).map_err(|error| error.to_string())?;
    fs::set_permissions(&authorized, fs::Permissions::from_mode(0o600))
        .map_err(|error| error.to_string())?;
    let host_key = work.join(format!("{name}_host"));
    must(
        "generate stock sshd's host key",
        run(Command::new(&tools.td_keygen)
            .args(["-q", "-t", "ed25519", "-N", "", "-f"])
            .arg(&host_key))?,
    )?;
    let port = free_port()?;
    let config = work.join(format!("{name}_config"));
    fs::write(
        &config,
        format!(
            "Port {port}\nListenAddress 127.0.0.1\nHostKey {}\nAuthorizedKeysFile {}\nPidFile none\nStrictModes no\nSshdSessionPath {}\nSshdAuthPath {}\n{extra}",
            host_key.display(),
            authorized.display(),
            tools.sshd_session.display(),
            tools.sshd_auth.display(),
        ),
    )
    .map_err(|error| error.to_string())?;
    let log = work.join(format!("{name}.log"));
    let child = Command::new(&tools.sshd)
        .args(["-D", "-e", "-f"])
        .arg(&config)
        .stdin(Stdio::null())
        .stderr(fs::File::create(&log).map_err(|error| error.to_string())?)
        .spawn()
        .map_err(|error| format!("spawn sshd: {error}"))?;
    let start = Instant::now();
    while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
        if start.elapsed() > START_DEADLINE {
            return Err(format!(
                "stock sshd did not listen: {:?}",
                fs::read_to_string(&log).unwrap_or_default()
            ));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Ok(Daemon { child, port })
}

/// A program answering a key's passphrase, in `work`.
fn write_askpass(tools: &Tools, work: &Path) -> Result<PathBuf, String> {
    let askpass = work.join(ASKPASS);
    fs::write(
        &askpass,
        format!("#!{}\necho {PASSPHRASE}\n", tools.shell.display()),
    )
    .map_err(|error| error.to_string())?;
    fs::set_permissions(&askpass, fs::Permissions::from_mode(0o700))
        .map_err(|error| error.to_string())?;
    Ok(askpass)
}

/// One client binary pointed at one daemon.
struct Client<'a> {
    program: &'a Path,
    port: u16,
    key: &'a Path,
    known_hosts: PathBuf,
    target: String,
}

impl Client<'_> {
    fn command(&self) -> Command {
        let mut command = Command::new(self.program);
        command
            .args(["-F", "/dev/null", "-p"])
            .arg(self.port.to_string())
            .arg("-i")
            .arg(self.key)
            .args([
                "-o",
                "BatchMode=yes",
                "-o",
                "StrictHostKeyChecking=accept-new",
            ])
            .arg("-o")
            .arg(format!("UserKnownHostsFile={}", self.known_hosts.display()))
            .args(["-o", "GlobalKnownHostsFile=/dev/null"]);
        command
    }

    fn exec(&self, options: &[&str], remote: &str) -> Result<Output, String> {
        run(self.command().args(options).arg(&self.target).arg(remote))
    }
}

/// `ssh h program | head`: the client hears its stdout has gone and passes it on,
/// the program writing to it stops, and the whole thing ends rather than going on
/// for ever with its output discarded. Not stock `ssh` to td-sshd: stock sends
/// `eow@openssh.com` only to a server whose banner says OpenSSH (DESIGN.md section 6).
fn stdout_closing_early(
    label: &str,
    client: &Client<'_>,
    report: &mut Report,
) -> Result<(), String> {
    let helper = std::env::current_exe().map_err(|error| error.to_string())?;
    let mut child = client
        .command()
        .arg(&client.target)
        .arg(format!("{} {FOREVER_FLAG}", helper.display()))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| error.to_string())?;
    let mut stdout = child.stdout.take().ok_or("no stdout")?;
    let mut first = [0u8; 1024];
    stdout
        .read_exact(&mut first)
        .map_err(|error| format!("{label}: reading the start: {error}"))?;
    drop(stdout);
    let start = Instant::now();
    let ended = loop {
        match child.try_wait().map_err(|error| error.to_string())? {
            Some(status) => break Some(status),
            None if start.elapsed() > EARLY_CLOSE_DEADLINE => break None,
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    };
    let _ = child.kill();
    let _ = child.wait();
    // The far program is this driver, whose write fails and which exits 1, and the
    // client carries that back. A client that gave up by itself would exit 255, and
    // one killed by its own broken pipe would have no code, the program still running.
    report.check(
        &format!("{label}: a closed stdout ends the program writing to it"),
        ended.is_some_and(|status| status.code() == Some(1)),
        || format!("ended {ended:?}, against a deadline of {EARLY_CLOSE_DEADLINE:?}"),
    );
    Ok(())
}

const PTY_REFUSED: &str = "PTY allocation request failed on channel 0";

/// A refused terminal against stock: told, and the command runs. Without a tty of
/// its own here, this holds what a test can: not that the local terminal stayed cooked.
fn td_ssh_goes_on_without_a_terminal(
    client: &Client<'_>,
    report: &mut Report,
) -> Result<(), String> {
    let output = client.exec(&["-tt"], "echo ran; exit 3")?;
    report.check(
        "td-ssh to sshd: a refused terminal is said, and the command still runs",
        output.status.code() == Some(3)
            && output.stdout == b"ran\n"
            && text(&output.stderr).contains(PTY_REFUSED),
        || describe(&output),
    );
    Ok(())
}

/// Stock `ssh` as `ControlMaster`, rekeying, with more outstanding across its
/// sessions than a rekey may hold back: once `too much traffic held back`.
fn multiplexed_sessions_across_rekeys(
    tools: &Tools,
    work: &Path,
    client: &Client<'_>,
    report: &mut Report,
) -> Result<(), String> {
    // Relative, with both ends started in `work`: a socket's path has to fit in 108
    // bytes, and a temporary directory's need not leave room.
    let socket = work.join("control");
    let control_path = "ControlPath=control";
    let master_log = work.join("control_master.log");
    let mut master = Daemon {
        child: client
            .command()
            .args(["-o", "ControlMaster=yes", "-o", "RekeyLimit=4M", "-o"])
            .arg(control_path)
            .arg("-N")
            .arg(&client.target)
            .current_dir(work)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(fs::File::create(&master_log).map_err(|error| error.to_string())?)
            .spawn()
            .map_err(|error| format!("spawn the control master: {error}"))?,
        port: 0,
    };
    let start = Instant::now();
    while !socket.exists() {
        if start.elapsed() > START_DEADLINE || matches!(master.child.try_wait(), Ok(Some(_))) {
            return Err(format!(
                "no control socket: {:?}",
                fs::read_to_string(&master_log).unwrap_or_default()
            ));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let helper = std::env::current_exe().map_err(|error| error.to_string())?;
    let wanted = digest_line(&pattern(MUX_BYTES));
    let mut sessions = Vec::with_capacity(MUX_SESSIONS);
    for _ in 0..MUX_SESSIONS {
        sessions.push(
            Command::new(&tools.ssh)
                .args(["-F", "/dev/null", "-p"])
                .arg(client.port.to_string())
                .args(["-o", "ControlMaster=no", "-o"])
                .arg(control_path)
                .arg(&client.target)
                .arg(format!("{} {EMIT_FLAG} {MUX_BYTES}", helper.display()))
                .current_dir(work)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .map_err(|error| format!("spawn a session: {error}"))?,
        );
    }
    for (index, session) in sessions.into_iter().enumerate() {
        let output = session
            .wait_with_output()
            .map_err(|error| error.to_string())?;
        let got = digest_line(&output.stdout);
        report.check(
            &format!("ssh to td-sshd: multiplexed session {index} across rekeys, by digest"),
            output.status.success() && got == wanted,
            || {
                format!(
                    "wanted {wanted}, got {got}, stderr {:?}",
                    text(&output.stderr)
                )
            },
        );
    }
    drop(master);
    Ok(())
}

/// `no-pty` on the key, which td-sshd refuses as stock `sshd` does. Stock `ssh` takes
/// a refusal of `-tt`, an explicit request, as fatal: it ends at once, in its own words.
fn no_pty_key_against_td_sshd(
    home: &Path,
    client: &Client<'_>,
    report: &mut Report,
) -> Result<(), String> {
    let authorized = home.join(".ssh/authorized_keys");
    let original = fs::read_to_string(&authorized).map_err(|error| error.to_string())?;
    fs::write(&authorized, format!("no-pty {original}")).map_err(|error| error.to_string())?;
    let outcome = client.exec(&["-tt"], "echo ran");
    fs::write(&authorized, original).map_err(|error| error.to_string())?;
    let output = outcome?;
    report.check(
        "ssh -tt to td-sshd: no-pty on the key is a refusal stock understands",
        output.status.code() == Some(255) && text(&output.stderr).contains(PTY_REFUSED),
        || describe(&output),
    );
    Ok(())
}

/// A negative leg can pass against a daemon that has died, so the last thing against
/// each of them is a login.
fn still_answering(label: &str, client: &Client<'_>, report: &mut Report) -> Result<(), String> {
    let output = client.exec(&[], "echo alive")?;
    report.check(
        &format!("{label}: still answering after its refusals"),
        output.status.success() && output.stdout == b"alive\n",
        || describe(&output),
    );
    Ok(())
}

/// The client under test with another key, which the server must refuse by looking.
fn another_key_refused(
    label: &str,
    client: &Client<'_>,
    stranger: &Path,
    report: &mut Report,
) -> Result<(), String> {
    let wrong = Client {
        program: client.program,
        port: client.port,
        key: stranger,
        known_hosts: client.known_hosts.clone(),
        target: client.target.clone(),
    };
    let output = wrong.exec(&["-o", "IdentitiesOnly=yes"], "true")?;
    report.check(
        &format!("{label}: a key nothing authorizes is Permission denied"),
        output.status.code() == Some(255) && saying(&output, "Permission denied (publickey)"),
        || describe(&output),
    );
    Ok(())
}

/// What any client must do against any server.
fn common_scenarios(label: &str, client: &Client<'_>, report: &mut Report) -> Result<(), String> {
    for kex in KEX {
        let option = format!("KexAlgorithms={kex}");
        let output = client.exec(&["-v", "-o", &option], "echo hello")?;
        let said = text(&output.stderr);
        report.check(
            &format!("{label}: {kex}, and -v says that is what ran"),
            output.status.success()
                && output.stdout == b"hello\n"
                && negotiated(&said).first() == Some(kex),
            || describe(&output),
        );
    }
    let output = client.exec(&[], "exit 42")?;
    report.check(
        &format!("{label}: exit status"),
        output.status.code() == Some(42),
        || describe(&output),
    );
    let output = client.exec(&[], "echo oops >&2")?;
    report.check(
        &format!("{label}: stderr"),
        text(&output.stderr).trim_end().ends_with("oops") && output.stdout.is_empty(),
        || describe(&output),
    );
    let output = run_with_input(
        client
            .command()
            .arg(&client.target)
            .arg("while read -r line; do echo \"got $line\"; done; echo end")
            .stdout(Stdio::piped()),
        b"one\ntwo\n",
    )?;
    report.check(
        &format!("{label}: stdin and its EOF"),
        output.status.success() && output.stdout == b"got one\ngot two\nend\n",
        || describe(&output),
    );

    // This program is the far end too, so a wrong byte shows and not only a
    // wrong count, and with no tool beyond the shell in the store.
    let helper = std::env::current_exe().map_err(|error| error.to_string())?;
    let helper = helper.display();
    let payload = pattern(TRANSFER_BYTES);
    let output = run_with_input(
        client
            .command()
            .args(["-v", "-o", UPLOAD_REKEY_LIMIT])
            .arg(&client.target)
            .arg(format!("{helper} {DIGEST_FLAG}"))
            .stdout(Stdio::piped()),
        &payload,
    )?;
    let wanted = format!("{}\n", digest_line(&payload));
    report.check(
        &format!("{label}: upload across rekeys"),
        output.status.success()
            && text(&output.stdout) == wanted
            && rekeys(&text(&output.stderr)) > 0,
        || format!("wanted {wanted:?}: {}", describe(&output)),
    );
    download_across_rekeys(label, client, &[DOWNLOAD_REKEY_LIMIT], report)
}

/// The far end's `--emit`, by content, with at least one rekey said by `-v`.
fn download_across_rekeys(
    label: &str,
    client: &Client<'_>,
    options: &[&str],
    report: &mut Report,
) -> Result<(), String> {
    let helper = std::env::current_exe().map_err(|error| error.to_string())?;
    let mut all = vec!["-v"];
    for option in options {
        all.extend(["-o", option]);
    }
    let output = client.exec(
        &all,
        &format!("{} {EMIT_FLAG} {TRANSFER_BYTES}", helper.display()),
    )?;
    report.check(
        &format!("{label}: download across rekeys"),
        output.status.success()
            && output.stdout == pattern(TRANSFER_BYTES)
            && rekeys(&text(&output.stderr)) > 0,
        || {
            format!(
                "status {:?}, {} bytes, stderr {:?}",
                output.status.code(),
                output.stdout.len(),
                text(&output.stderr)
            )
        },
    );
    Ok(())
}

/// Each exchange with the server allowing nothing else, where a client that offered
/// only what it preferred would have nothing to agree on, and a server that starts
/// its own rekeys, each of which the client has to answer.
fn td_client_against_stock_settings(
    tools: &Tools,
    work: &Path,
    client: &Client<'_>,
    authorized: &str,
    report: &mut Report,
) -> Result<(), String> {
    for (index, kex) in KEX.iter().enumerate() {
        let sshd = start_stock_sshd(
            tools,
            work,
            authorized,
            &format!("stock_kex_{index}"),
            &format!("KexAlgorithms {kex}\n"),
        )?;
        let output = Client {
            program: client.program,
            port: sshd.port,
            key: client.key,
            known_hosts: work.join(format!("known_hosts_td_kex_{index}")),
            target: client.target.clone(),
        }
        .exec(&["-v"], "echo hello")?;
        let said = text(&output.stderr);
        report.check(
            &format!("td-ssh to sshd allowing only {kex}"),
            output.status.success()
                && output.stdout == b"hello\n"
                && negotiated(&said).first() == Some(kex),
            || describe(&output),
        );
    }
    let sshd = start_stock_sshd(
        tools,
        work,
        authorized,
        "stock_rekeying_sshd",
        "RekeyLimit 1M\n",
    )?;
    let rekeying = Client {
        program: client.program,
        port: sshd.port,
        key: client.key,
        known_hosts: work.join("known_hosts_td_rekeying"),
        target: client.target.clone(),
    };
    download_across_rekeys(
        "td-ssh to sshd, the server starting each rekey",
        &rekeying,
        &[],
        report,
    )
}

fn stock_client_against_td_sshd(
    tools: &Tools,
    work: &Path,
    home: &Path,
    client: &Client<'_>,
    stranger: &Path,
    report: &mut Report,
) -> Result<(), String> {
    common_scenarios("ssh to td-sshd", client, report)?;

    for (name, options, words) in [
        (
            "a remote forward",
            &[
                "-o",
                "ExitOnForwardFailure=yes",
                "-R",
                "0:127.0.0.1:1",
                "-N",
            ][..],
            "remote port forwarding failed",
        ),
        (
            "a direct-tcpip channel",
            &["-W", "127.0.0.1:1"][..],
            "open failed: unknown channel type",
        ),
    ] {
        let output = run(client.command().args(options).arg(&client.target))?;
        report.check(
            &format!("ssh to td-sshd: {name} is refused"),
            output.status.code() == Some(255) && saying(&output, words),
            || describe(&output),
        );
    }

    another_key_refused("ssh to td-sshd", client, stranger, report)?;
    // A cipher every OpenSSH has: this peer builds without OpenSSL, and one it
    // does not know ends on the command line, before td-sshd is ever asked.
    let output = client.exec(&["-o", "Ciphers=aes256-ctr"], "true")?;
    report.check(
        "ssh to td-sshd: an algorithm outside the profile fails",
        output.status.code() == Some(255) && saying(&output, "no matching cipher"),
        || describe(&output),
    );
    let td_client = Client {
        program: &tools.td_ssh,
        port: client.port,
        key: client.key,
        known_hosts: work.join("known_hosts_td_to_td"),
        target: client.target.clone(),
    };
    another_key_refused("td-ssh to td-sshd", &td_client, stranger, report)?;
    stdout_closing_early("td-ssh to td-sshd", &td_client, report)?;
    multiplexed_sessions_across_rekeys(tools, work, client, report)?;
    no_pty_key_against_td_sshd(home, client, report)?;
    still_answering("ssh to td-sshd", client, report)
}

fn td_client_against_stock_sshd(
    tools: &Tools,
    work: &Path,
    client: &Client<'_>,
    encrypted: &Path,
    stranger: &Path,
    report: &mut Report,
) -> Result<(), String> {
    common_scenarios("td-ssh to sshd", client, report)?;
    another_key_refused("td-ssh to sshd", client, stranger, report)?;

    // A passphrase through askpass, against a key stock ssh-keygen wrote.
    let askpass = work.join(ASKPASS);
    let mut command = Command::new(client.program);
    command
        .args(["-F", "/dev/null", "-p"])
        .arg(client.port.to_string())
        .arg("-i")
        .arg(encrypted)
        .args(["-o", "StrictHostKeyChecking=accept-new"])
        .arg("-o")
        .arg(format!(
            "UserKnownHostsFile={}",
            client.known_hosts.display()
        ))
        .env("SSH_ASKPASS", &askpass)
        .env("SSH_ASKPASS_REQUIRE", "force")
        .arg(&client.target)
        .arg("echo unlocked");
    let output = run(&mut command)?;
    report.check(
        "td-ssh to sshd: an encrypted stock key through askpass",
        output.status.success() && output.stdout == b"unlocked\n",
        || describe(&output),
    );

    // The recorded key is held to.
    let other_hosts = work.join("other_known_hosts");
    let foreign = work.join("foreign_host");
    must(
        "generate a foreign key",
        run(Command::new(&tools.td_keygen)
            .args(["-q", "-t", "ed25519", "-N", "", "-f"])
            .arg(&foreign))?,
    )?;
    let mut foreign_public = foreign.as_os_str().to_os_string();
    foreign_public.push(".pub");
    let public = fs::read_to_string(&foreign_public).map_err(|error| error.to_string())?;
    fs::write(
        &other_hosts,
        format!("[127.0.0.1]:{} {public}", client.port),
    )
    .map_err(|error| error.to_string())?;
    let changed = Client {
        program: client.program,
        port: client.port,
        key: client.key,
        known_hosts: other_hosts,
        target: client.target.clone(),
    };
    let output = changed.exec(&[], "true")?;
    report.check(
        "td-ssh to sshd: a changed host key fails",
        output.status.code() == Some(255)
            && text(&output.stderr).contains("Host key verification failed."),
        || describe(&output),
    );
    stdout_closing_early("td-ssh to sshd", client, report)?;
    still_answering("td-ssh to sshd", client, report)
}

fn file_digest(path: &Path) -> Result<String, String> {
    let bytes = fs::read(path).map_err(|error| format!("read {}: {error}", path.display()))?;
    Ok(digest_line(&bytes))
}

/// Stock `sftp` and modern `scp`, which speak SFTP to the subsystem through the
/// stock `ssh` under test, each byte held by digest on the far side too.
fn file_transfer_against_td_sshd(
    tools: &Tools,
    work: &Path,
    home: &Path,
    client: &Client<'_>,
    report: &mut Report,
) -> Result<(), String> {
    let payload = pattern(TRANSFER_BYTES);
    let wanted = digest_line(&payload);
    let local = work.join("transfer_local");
    fs::write(&local, &payload).map_err(|error| error.to_string())?;
    let known_hosts = format!("UserKnownHostsFile={}", client.known_hosts.display());
    let options = |command: &mut Command| {
        command
            .args(["-F", "/dev/null", "-i"])
            .arg(client.key)
            .arg("-S")
            .arg(client.program)
            .args(["-o", "StrictHostKeyChecking=accept-new", "-o"])
            .arg(&known_hosts)
            .args(["-o", "GlobalKnownHostsFile=/dev/null", "-P"])
            .arg(client.port.to_string());
    };
    let batch = |name: &str, script: String| -> Result<Output, String> {
        let file = work.join(name);
        fs::write(&file, script).map_err(|error| error.to_string())?;
        let mut command = Command::new(&tools.sftp);
        options(&mut command);
        run(command.arg("-b").arg(&file).arg(&client.target))
    };

    let back = work.join("sftp_back");
    let output = batch(
        "sftp_batch",
        format!(
            "mkdir incoming\nput {} incoming/first\nrename incoming/first incoming/moved\nls -l incoming\nget incoming/moved {}\n",
            local.display(),
            back.display()
        ),
    )?;
    let listing = text(&output.stdout);
    report.check(
        "sftp to td-sshd: a batch of mkdir, put, rename, ls and get",
        output.status.success(),
        || describe(&output),
    );
    let incoming = fs::metadata(home.join("incoming")).map_err(|error| error.to_string())?;
    report.check(
        "sftp to td-sshd: mkdir is under the umask floor",
        incoming.mode() & 0o022 == 0,
        || format!("{:o}", incoming.mode()),
    );
    let moved = home.join("incoming/moved");
    let far = file_digest(&moved).unwrap_or_else(|error| error);
    report.check("sftp to td-sshd: put, by digest", far == wanted, || {
        format!("wanted {wanted}, the far side holds {far}")
    });
    let near = file_digest(&back).unwrap_or_else(|error| error);
    report.check("sftp to td-sshd: get, by digest", near == wanted, || {
        format!("wanted {wanted}, got {near}")
    });
    report.check(
        "sftp to td-sshd: rename took the old name, ls shows the new",
        !home.join("incoming/first").exists() && listing.contains("incoming/moved"),
        || listing.clone(),
    );
    let output = batch(
        "sftp_cleanup",
        "rm incoming/moved\nrmdir incoming\n".to_string(),
    )?;
    report.check(
        "sftp to td-sshd: rm and rmdir",
        output.status.success() && !home.join("incoming").exists(),
        || describe(&output),
    );

    let scp = |source: &std::ffi::OsStr, destination: &std::ffi::OsStr| -> Result<Output, String> {
        let mut command = Command::new(&tools.scp);
        options(&mut command);
        run(command.arg("-q").arg(source).arg(destination))
    };
    let remote = |path: &str| std::ffi::OsString::from(format!("{}:{path}", client.target));
    let absolute = work.join("scp_absolute");
    for (name, path, landed) in [
        (
            "scp to td-sshd: upload to an absolute path",
            absolute.display().to_string(),
            absolute.clone(),
        ),
        (
            "scp to td-sshd: upload to a home-relative path",
            "scp_relative".to_string(),
            home.join("scp_relative"),
        ),
        (
            "scp to td-sshd: upload to a tilde path",
            "~/scp_tilde".to_string(),
            home.join("scp_tilde"),
        ),
    ] {
        let output = scp(local.as_os_str(), &remote(&path))?;
        let far = file_digest(&landed).unwrap_or_else(|error| error);
        report.check(name, output.status.success() && far == wanted, || {
            format!(
                "wanted {wanted}, the far side holds {far}: {}",
                describe(&output)
            )
        });
    }
    // scp -r makes each directory, then chmods it to the local one's mode: a request
    // for that exact mode, which td-sshd honours as sftp-server does. So a 0777 tree
    // arrives 0777 from either, and it is `MKDIR` that the floor is for, above. What
    // this holds is the ordinary tree arriving as it was.
    let tree = work.join("scp_tree");
    let inner = tree.join("inner");
    fs::create_dir_all(&inner).map_err(|error| error.to_string())?;
    fs::write(inner.join("file"), b"in a tree\n").map_err(|error| error.to_string())?;
    for directory in [&tree, &inner] {
        fs::set_permissions(directory, fs::Permissions::from_mode(0o755))
            .map_err(|error| error.to_string())?;
    }
    let mut command = Command::new(&tools.scp);
    options(&mut command);
    let output = run(command
        .args(["-q", "-r"])
        .arg(&tree)
        .arg(remote("scp_tree")))?;
    let modes: Vec<u32> = [home.join("scp_tree"), home.join("scp_tree/inner")]
        .iter()
        .map(|directory| fs::metadata(directory).map_or(u32::MAX, |found| found.mode()))
        .collect();
    report.check(
        "scp -r to td-sshd: directories under the umask floor",
        output.status.success()
            && modes
                .iter()
                .all(|mode| *mode != u32::MAX && mode & 0o777 == 0o755),
        || format!("{modes:?}: {}", describe(&output)),
    );
    let download = work.join("scp_back");
    let output = scp(&remote("scp_relative"), download.as_os_str())?;
    let near = file_digest(&download).unwrap_or_else(|error| error);
    report.check(
        "scp to td-sshd: download from a home-relative path",
        output.status.success() && near == wanted,
        || format!("wanted {wanted}, got {near}: {}", describe(&output)),
    );
    Ok(())
}

/// What `-T` must say of `user` under one form of the image's policy.
fn wanted_policy_lines(
    form: ssh_policy::Form,
    provisioned: ssh_policy::Provisioned,
    user: &str,
) -> Vec<String> {
    use ssh_policy::{Form, Provisioned};
    let desktop = provisioned == Provisioned::No;
    let keys = if user == POLICY_PRIMARY {
        let mut keys = ssh_policy::SSHD_SELFTEST_AUTHORIZED_KEYS.to_string();
        if !desktop {
            keys.push_str(&format!(
                " {} {}",
                ssh_policy::SSHD_PROVISIONED_AUTHORIZED_KEYS,
                ssh_policy::SSHD_METADATA_AUTHORIZED_KEYS
            ));
        }
        if form == Form::Ordinary {
            keys.push_str(&format!(" {}", ssh_policy::SSHD_USER_AUTHORIZED_KEYS));
        }
        keys
    } else if user == "td-admin" && desktop {
        "/var/lib/td-admin/authorized_keys".to_string()
    } else {
        ssh_policy::SSHD_AUTHORIZED_KEYS.to_string()
    };
    let mut lines = vec![
        format!("hostkey {}", ssh_policy::SSHD_HOST_KEY),
        "permitrootlogin no".to_string(),
        format!("authorizedkeysfile {keys}"),
        "subsystem sftp internal-sftp".to_string(),
    ];
    match (form, desktop) {
        (Form::Ordinary, _) => lines.push("allowusers none".to_string()),
        (Form::Enforced, true) => {
            lines.push(format!("allowusers {POLICY_PRIMARY}"));
            lines.push("allowusers td-admin".to_string());
        }
        (Form::Enforced, false) => lines.push(format!("allowusers {POLICY_PRIMARY}")),
    }
    if user == "td-admin" && desktop {
        lines.push("forcecommand /bin/td-admin ssh".to_string());
        lines.push("permittty no".to_string());
        lines.push("maxsessions 1".to_string());
    }
    lines
}

/// Every form of the policy the image renders, in the real daemon: `-T` parses the
/// file as it is and says what a `Match` for each name selects, not whether that
/// account would be admitted, which it never decides, and `-t` reads it with the
/// test's host key standing for the machine's, which is all that differs.
fn image_policy_in_td_sshd(
    tools: &Tools,
    work: &Path,
    user: &str,
    report: &mut Report,
) -> Result<(), String> {
    use ssh_policy::{Form, Provisioned};
    let host_key = work.join("td_sshd_host");
    for (name, form, provisioned) in [
        ("desktop ordinary", Form::Ordinary, Provisioned::No),
        ("desktop enforced", Form::Enforced, Provisioned::No),
        ("server ordinary", Form::Ordinary, Provisioned::Server),
        ("server enforced", Form::Enforced, Provisioned::Server),
    ] {
        let policy = ssh_policy::config(POLICY_PRIMARY, form, provisioned);
        let file = work.join("image_policy");
        fs::write(&file, &policy).map_err(|error| error.to_string())?;
        for account in [POLICY_PRIMARY, "tester", "root", "td-admin"] {
            let output = run(Command::new(&tools.td_sshd)
                .arg("-T")
                .arg("-f")
                .arg(&file)
                .arg("-C")
                .arg(format!("user={account},host=localhost,addr=127.0.0.1")))?;
            let said = text(&output.stdout);
            let wanted = wanted_policy_lines(form, provisioned, account);
            let missing: Vec<&String> = wanted
                .iter()
                .filter(|line| !said.lines().any(|said| said == line.as_str()))
                .collect();
            let admitted = said
                .lines()
                .filter(|line| line.starts_with("allowusers "))
                .count();
            let allowed = wanted
                .iter()
                .filter(|line| line.starts_with("allowusers "))
                .count();
            report.check(
                &format!("the image's {name} policy selected for {account}, by td-sshd -T"),
                output.status.success() && missing.is_empty() && admitted == allowed,
                || format!("missing {missing:?}: {}", describe(&output)),
            );
        }
        let with_key = policy.replace(
            &format!("HostKey {}\n", ssh_policy::SSHD_HOST_KEY),
            &format!("HostKey {}\n", host_key.display()),
        );
        fs::write(&file, with_key).map_err(|error| error.to_string())?;
        let output = run(Command::new(&tools.td_sshd)
            .arg("-t")
            .arg("-f")
            .arg(&file)
            .args(["--single-user", user]))?;
        report.check(
            &format!("the image's {name} policy, by td-sshd -t"),
            output.status.success(),
            || describe(&output),
        );
    }
    Ok(())
}

/// The enforced form's whole point, by real logins: it withdraws the account's own
/// `~/.ssh/authorized_keys`, which the ordinary form reads. The root-owned self-test
/// file both forms name is the test's, for the login that shows this daemon is up.
fn enforced_form_logins(
    tools: &Tools,
    work: &Path,
    user: &str,
    in_the_account: &Path,
    self_test: &Path,
    report: &mut Report,
) -> Result<(), String> {
    use ssh_policy::{Form, Provisioned};
    let public_of = |path: &Path| -> Result<String, String> {
        let mut name = path.as_os_str().to_os_string();
        name.push(".pub");
        fs::read_to_string(name).map_err(|error| error.to_string())
    };
    let home = work.join("policy_home");
    let dot_ssh = home.join(".ssh");
    fs::create_dir_all(&dot_ssh).map_err(|error| error.to_string())?;
    let own = dot_ssh.join("authorized_keys");
    fs::write(&own, public_of(in_the_account)?).map_err(|error| error.to_string())?;
    fs::set_permissions(&own, fs::Permissions::from_mode(0o600))
        .map_err(|error| error.to_string())?;
    let listed = work.join("policy_self_test_keys");
    fs::write(&listed, public_of(self_test)?).map_err(|error| error.to_string())?;
    fs::set_permissions(&listed, fs::Permissions::from_mode(0o600))
        .map_err(|error| error.to_string())?;
    let render = |form: Form| {
        ssh_policy::config(user, form, Provisioned::No)
            .replace(
                "Port 22\nListenAddress 0.0.0.0\n",
                "Port 0\nListenAddress 127.0.0.1\n",
            )
            .replace(
                &format!("HostKey {}\n", ssh_policy::SSHD_HOST_KEY),
                &format!("HostKey {}\n", work.join("td_sshd_host").display()),
            )
            .replace(
                ssh_policy::SSHD_SELFTEST_AUTHORIZED_KEYS,
                &listed.display().to_string(),
            )
    };
    let target = format!("{user}@127.0.0.1");
    let login = |port: u16, key: &Path, program: &Path, hosts: &str| -> Result<Output, String> {
        Client {
            program,
            port,
            key,
            known_hosts: work.join(hosts),
            target: target.clone(),
        }
        .exec(&["-o", "IdentitiesOnly=yes"], "echo in")
    };

    let enforced = spawn_td_sshd(
        tools,
        work,
        "td_sshd_enforced",
        &render(Form::Enforced),
        &home,
        user,
    )?;
    let output = login(
        enforced.port,
        in_the_account,
        &tools.ssh,
        "known_hosts_enforced",
    )?;
    report.check(
        "the enforced form: a key only in the account's own file is refused",
        output.status.code() == Some(255) && saying(&output, "Permission denied (publickey)"),
        || describe(&output),
    );
    let output = login(
        enforced.port,
        self_test,
        &tools.td_ssh,
        "known_hosts_enforced_td",
    )?;
    report.check(
        "the enforced form: a key in the file it names logs in",
        output.status.success() && output.stdout == b"in\n",
        || describe(&output),
    );
    drop(enforced);

    let ordinary = spawn_td_sshd(
        tools,
        work,
        "td_sshd_ordinary",
        &render(Form::Ordinary),
        &home,
        user,
    )?;
    let output = login(
        ordinary.port,
        in_the_account,
        &tools.ssh,
        "known_hosts_ordinary",
    )?;
    report.check(
        "the ordinary form: the same key logs in",
        output.status.success() && output.stdout == b"in\n",
        || describe(&output),
    );
    Ok(())
}

fn parse_arguments() -> Result<HashMap<String, String>, String> {
    let mut parsed = HashMap::new();
    let mut args = std::env::args().skip(1);
    while let Some(name) = args.next() {
        let value = args.next().ok_or_else(|| format!("{name} needs a value"))?;
        parsed.insert(name, value);
    }
    Ok(parsed)
}

fn tools(arguments: &HashMap<String, String>) -> Result<Tools, String> {
    let directory = |name: &str| -> Result<PathBuf, String> {
        arguments
            .get(name)
            .map(PathBuf::from)
            .ok_or_else(|| format!("missing {name}"))
    };
    let openssh = directory("--openssh")?;
    let td_ssh = directory("--td-ssh")?;
    Ok(Tools {
        ssh: openssh.join("bin/ssh"),
        sshd: openssh.join("bin/sshd"),
        ssh_keygen: openssh.join("bin/ssh-keygen"),
        scp: openssh.join("bin/scp"),
        sftp: openssh.join("bin/sftp"),
        sshd_session: openssh.join("libexec/sshd-session"),
        sshd_auth: openssh.join("libexec/sshd-auth"),
        td_ssh: td_ssh.join("bin/td-ssh"),
        td_sshd: td_ssh.join("bin/td-sshd"),
        td_keygen: td_ssh.join("bin/td-ssh-keygen"),
        shell: directory("--shell")?,
    })
}

/// Every log a daemon or control master left, whichever ones it got as far as.
fn logs(work: &Path) -> String {
    let mut names: Vec<PathBuf> = fs::read_dir(work)
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| path.extension().is_some_and(|tail| tail == "log"))
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    let mut out = String::new();
    for name in names {
        let content = fs::read_to_string(&name).unwrap_or_default();
        out.push_str(&format!("--- {}\n{content}\n", name.display()));
    }
    out
}

fn exercise(tools: &Tools, work: &Path, report: &mut Report) -> Result<(), String> {
    let home = work.join("home");
    fs::create_dir_all(&home).map_err(|error| error.to_string())?;
    let user = account(&home, &tools.shell)?;
    let (stock_key, td_key) = crossed_key_tools(tools, work, report)?;
    let public_of = |path: &Path| -> Result<String, String> {
        let mut name = path.as_os_str().to_os_string();
        name.push(".pub");
        fs::read_to_string(name).map_err(|error| error.to_string())
    };
    let target = format!("{user}@127.0.0.1");

    // Each side authenticates with the key the other's tool wrote.
    let td_sshd = start_td_sshd(tools, work, &home, &user, &public_of(&stock_key)?)?;
    image_policy_in_td_sshd(tools, work, &user, report)?;
    let stock_client = Client {
        program: &tools.ssh,
        port: td_sshd.port,
        key: &stock_key,
        known_hosts: work.join("known_hosts_stock"),
        target: target.clone(),
    };
    let stranger = work.join(STRANGER_KEY);
    stock_client_against_td_sshd(tools, work, &home, &stock_client, &stranger, report)?;
    file_transfer_against_td_sshd(tools, work, &home, &stock_client, report)?;
    drop(td_sshd);
    enforced_form_logins(tools, work, &user, &stock_key, &td_key, report)?;

    let authorized = format!(
        "{}{}",
        public_of(&td_key)?,
        public_of(&work.join("stock_encrypted"))?
    );
    let sshd = start_stock_sshd(tools, work, &authorized, "stock_sshd", "")?;
    let no_tty_sshd = start_stock_sshd(
        tools,
        work,
        &authorized,
        "stock_no_tty_sshd",
        "PermitTTY no\n",
    )?;
    td_ssh_goes_on_without_a_terminal(
        &Client {
            program: &tools.td_ssh,
            port: no_tty_sshd.port,
            key: &td_key,
            known_hosts: work.join("known_hosts_td_no_tty"),
            target: target.clone(),
        },
        report,
    )?;
    drop(no_tty_sshd);
    let td_client = Client {
        program: &tools.td_ssh,
        port: sshd.port,
        key: &td_key,
        known_hosts: work.join("known_hosts_td"),
        target,
    };
    td_client_against_stock_sshd(
        tools,
        work,
        &td_client,
        &work.join("stock_encrypted"),
        &stranger,
        report,
    )?;
    td_client_against_stock_settings(tools, work, &td_client, &authorized, report)
}

fn main() {
    let mut arguments = std::env::args().skip(1);
    let first = arguments.next();
    if let Some(mode) = first
        .as_deref()
        .filter(|mode| [DIGEST_FLAG, EMIT_FLAG, FOREVER_FLAG].contains(mode))
    {
        if let Err(error) = far_end(mode, arguments.next().as_deref()) {
            eprintln!("td-ssh-interop: {error}");
            std::process::exit(1);
        }
        return;
    }
    let mut report = Report::new();
    let work = std::env::temp_dir().join(format!("td-ssh-interop-{}", std::process::id()));
    {
        let report = report.clone();
        let work = work.clone();
        std::thread::spawn(move || {
            std::thread::sleep(WATCHDOG);
            eprintln!(
                "td-ssh-interop: gave up after {WATCHDOG:?}, with {} passed: {}\n{}",
                report.passed(),
                report.failures().join("\n"),
                logs(&work)
            );
            std::process::exit(2);
        });
    }
    let outcome = parse_arguments()
        .and_then(|arguments| tools(&arguments))
        .and_then(|tools| {
            fs::create_dir_all(&work).map_err(|error| error.to_string())?;
            fs::set_permissions(&work, fs::Permissions::from_mode(0o700))
                .map_err(|error| error.to_string())?;
            exercise(&tools, &work, &mut report)
        });
    let logs = logs(&work);
    let _ = fs::remove_dir_all(&work);
    if let Err(error) = outcome {
        eprintln!("td-ssh-interop: {error}\n{logs}");
        std::process::exit(1);
    }
    let failed = report.failures();
    if !failed.is_empty() {
        eprintln!("td-ssh-interop: {}\n{logs}", failed.join("\n"));
        std::process::exit(1);
    }
    println!("{MARKER} {}", report.passed());
}

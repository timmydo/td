// qemu-login-system (td-secret/DESIGN.md, "Login system guest"): the
// production system image's supervisor, firstboot, serial greeter, sshd,
// boot health, compositor and td-authd over a record this worker
// enrolled, through a UHID keyboard and UHID keys, with no TPM. One test,
// booted once per phase on one disposable volume (`PHASES`, which
// recipes/src/fixtures/secret_vm.rs pins); the host checks the display at
// each screen the guest names, as login-desktop's does, and wakes the
// guest from the suspend to RAM it asks for.

use super::*;
use std::os::unix::fs::chown;
use std::process::{Output, Stdio};

/// The boots, in order. `seed` enrolls two keys on an unenrolled machine;
/// `locked` is the first enrolled boot; each `damaged-` boot shows its
/// damage, which the boot before it made, and repairs it, and the
/// `repaired-` boot after it is an ordinary enrolled one again, which
/// makes the next damage.
const PHASES: &[&str] = &[
    "seed",
    "locked",
    "damaged-directory-mode",
    "repaired-directory-mode",
    "damaged-directory-owner",
    "repaired-directory-owner",
    "damaged-directory-file",
    "repaired-directory-file",
    "damaged-record-mode",
    "repaired-record-mode",
    "damaged-record-links",
    "repaired-record-links",
    "damaged-record-truncated",
    "repaired-record-truncated",
    "damaged-record-version",
    "repaired-record-version",
];
/// The stock image's hostname, which the lock surface shows.
const HOSTNAME: &str = "td";
/// The fixture unit's readiness, which the stock seat setup waits for.
const INPUT_READY: &str = "/run/td-secret-system-input-ready";
/// Root's fixture state on `@var`, outside the login directory: the
/// persistent keys and the intact record a repair restores.
const STATE: &str = "/var/lib/td-fixture/login-system";
const SYSTEM_KEYS: &[&str] = &["a", "b"];
/// A boot's own scratch, and the record's second name for the link damage.
const WORK: &str = "/run/td-login-system";
const SECOND_LINK: &str = "/var/lib/td/login-second-link";
/// The keyboard's left GUI modifier, and the usages of L and I.
const SUPER: u8 = 0x08;
const KEY_L: u8 = 0x0f;
const KEY_I: u8 = 0x0c;
const SSH_REPLY: &str = "TD-LOGIN-SYSTEM-SSH";
const UPDATE_READY: &str = " is ready. Press Ctrl+Alt+Escape, then I to review it.";
const UPDATE_BACKING_OFF: &str = "the installation queue is backing off after unapproved requests";
/// td-authd's backoff file; its update row counts each admitted request.
const BACKOFF: &str = "/var/lib/td/authd/backoff";
/// The cutover record firstboot and td-authd publish, and td-authd's
/// reboot guard (td-authd/DESIGN.md, amendment 7).
const CUTOVER: &str = "/run/td-login-cutover";
const REBOOT_GUARD: &str = "/var/lib/td/login/cutover-reboot";
/// The open SSH session's remote command's operand, which names it.
const SESSION_SLEEP: &str = "4321";

fn phase() -> &'static str {
    let cmdline = fs::read_to_string("/proc/cmdline").unwrap();
    let named: Vec<&str> = cmdline
        .split_ascii_whitespace()
        .filter_map(|token| token.strip_prefix("td.login-system="))
        .collect();
    assert_eq!(named.len(), 1, "{cmdline}");
    PHASES
        .iter()
        .copied()
        .find(|phase| *phase == named[0])
        .expect("a listed login system phase")
}

/// The damage this boot leaves for the next, if the next is a damaged one.
fn next_damage(phase: &str) -> Option<&'static str> {
    let at = PHASES.iter().position(|listed| *listed == phase)?;
    PHASES.get(at + 1)?.strip_prefix("damaged-")
}

fn system_wait(label: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(180);
    while !done() {
        assert!(Instant::now() < deadline, "login system timed out: {label}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn service(args: &[&str]) -> String {
    let output = Command::new("/bin/td-svc").args(args).output().unwrap();
    assert!(
        output.status.success(),
        "td-svc {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

fn ready(names: &[&str]) {
    system_wait("stock services ready", || {
        let status = service(&["status"]);
        names.iter().all(|name| {
            status
                .lines()
                .any(|line| line.starts_with(&format!("{name} ready ")))
        })
    });
}

struct Proc {
    pid: u32,
    parent: u32,
    argv: Vec<String>,
}

fn processes() -> Vec<Proc> {
    fs::read_dir("/proc")
        .unwrap()
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .filter_map(|name| name.parse::<u32>().ok())
        .filter_map(|pid| {
            let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
            let (_, fields) = stat.rsplit_once(')')?;
            let mut fields = fields.split_whitespace();
            if fields.next()? == "Z" {
                return None;
            }
            let parent = fields.next()?.parse().ok()?;
            let argv = fs::read(format!("/proc/{pid}/cmdline"))
                .ok()?
                .split(|byte| *byte == 0)
                .filter(|word| !word.is_empty())
                .map(|word| String::from_utf8_lossy(word).into_owned())
                .collect();
            Some(Proc { pid, parent, argv })
        })
        .collect()
}

fn runs(process: &Proc, program: &str, verb: &str) -> bool {
    process
        .argv
        .first()
        .is_some_and(|name| name.ends_with(program))
        && process.argv.get(1).is_some_and(|word| word == verb)
}

/// The paired authority: the wayland unit's `td-authd terminal-serve
/// --primary`, once there is exactly one.
fn authority() -> u32 {
    let find = || -> Vec<u32> {
        processes()
            .iter()
            .filter(|process| {
                runs(process, "td-authd", "terminal-serve")
                    && process.argv.iter().any(|word| word == "--primary")
            })
            .map(|process| process.pid)
            .collect()
    };
    system_wait("one paired authority", || find().len() == 1);
    find()[0]
}

fn compositor() -> Option<u32> {
    let found: Vec<u32> = processes()
        .iter()
        .filter(|process| runs(process, "td-compositor", "run"))
        .map(|process| process.pid)
        .collect();
    match found[..] {
        [pid] => Some(pid),
        _ => None,
    }
}

/// The authority's live login workers.
fn workers(authority: u32) -> Vec<u32> {
    processes()
        .iter()
        .filter(|process| {
            process.parent == authority && process.argv.iter().any(|word| word == "login-operation")
        })
        .map(|process| process.pid)
        .collect()
}

/// For a second after the chord or Escape, no login worker lives: no
/// `1b` reached the authority.
fn no_unlock(authority: u32) {
    let until = Instant::now() + Duration::from_secs(1);
    while Instant::now() < until {
        assert_eq!(workers(authority), [] as [u32; 0], "a login worker started");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The serial greeter's td-login, parked on ttyS0 after its refusal. Its
/// getty flushed the line's input before it ran, so the host's answers
/// are typed only after this.
fn greeter() -> Option<u32> {
    processes()
        .iter()
        .find(|process| {
            fs::read_link(format!("/proc/{}/exe", process.pid))
                .is_ok_and(|exe| exe.to_string_lossy().ends_with("/td-login"))
                && fs::read_link(format!("/proc/{}/fd/0", process.pid))
                    .is_ok_and(|line| line == Path::new("/dev/ttyS0"))
        })
        .map(|process| process.pid)
}

/// The program of a process reading ttyS0, if one does.
fn serial() -> Option<String> {
    processes().iter().find_map(|process| {
        fs::read_link(format!("/proc/{}/fd/0", process.pid))
            .is_ok_and(|line| line == Path::new("/dev/ttyS0"))
            .then(|| fs::read_link(format!("/proc/{}/exe", process.pid)).ok())?
            .map(|exe| exe.to_string_lossy().into_owned())
    })
}

/// The serial greeter's login shell once its profile has finished: a
/// login shell (`-` before its name) on ttyS0 with no child.
fn login_shell() -> Option<u32> {
    let all = processes();
    all.iter()
        .find(|process| {
            process
                .argv
                .first()
                .is_some_and(|name| name.starts_with('-'))
                && fs::read_link(format!("/proc/{}/fd/0", process.pid))
                    .is_ok_and(|line| line == Path::new("/dev/ttyS0"))
        })
        .filter(|shell| !all.iter().any(|process| process.parent == shell.pid))
        .map(|shell| shell.pid)
}

/// Waits for the serial greeter's login shell to stay idle for two
/// seconds, and returns it.
fn idle_login_shell() -> u32 {
    let mut idle = None;
    system_wait("the serial greeter's idle login shell", || {
        let shell = login_shell();
        match (shell, idle) {
            (Some(pid), Some((seen, since))) if pid == seen => {
                Instant::now().duration_since(since) >= Duration::from_secs(2)
            }
            (Some(pid), _) => {
                idle = Some((pid, Instant::now()));
                false
            }
            (None, _) => {
                idle = None;
                false
            }
        }
    });
    idle.unwrap().0
}

fn parked_greeter() -> u32 {
    system_wait("the serial greeter's td-login", || greeter().is_some());
    let pid = greeter().unwrap();
    std::thread::sleep(Duration::from_secs(1));
    assert_eq!(greeter(), Some(pid), "the serial greeter did not stay");
    pid
}

/// The boot-success health transaction completed, which follows the
/// profiler's evidence and so may take minutes.
fn boot_healthy() {
    // After the profiler's evidence, so within its service bound.
    let deadline = Instant::now() + Duration::from_secs(900);
    while fs::read_to_string("/run/td-boot-success-ok")
        .map_or(true, |status| status != "td-boot-success-v1\n")
    {
        assert!(
            Instant::now() < deadline,
            "login system timed out: boot health"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn key_file(name: &str) -> PathBuf {
    Path::new(STATE).join(name)
}

fn record_path() -> PathBuf {
    directory().join(UID.to_string())
}

fn backup() -> PathBuf {
    Path::new(STATE).join("record")
}

fn restored() -> Vec<Virtual> {
    SYSTEM_KEYS
        .iter()
        .map(|name| {
            Virtual::restore(Config::default(), &key_file(name))
                .unwrap_or_else(|damage| panic!("virtual key {name}: {damage}"))
        })
        .collect()
}

/// A key's enrolled credential's fingerprint, as the PIN step shows it.
fn print(key: &Virtual) -> String {
    hex(&fingerprint(&key.state().credentials.last().unwrap().id))
}

fn enrolled() -> Vec<Vec<u8>> {
    let ids = stored().expect("an enrolled record");
    assert_eq!(ids.len(), 2);
    ids
}

/// The kernel console lets go of the framebuffer, which is then blanked,
/// before the stock seat starts the pair; the host sees the blank first.
fn start(host: &mut Host) {
    release_console();
    blank_screen();
    host.screen("blank", &[]);
    fs::write(INPUT_READY, b"ready").unwrap();
}

/// The chord's unlock with `key`: its PIN step empty and with four masks,
/// Enter, the touch request with exactly one login worker alive and the
/// key waiting for its touch, the touch request again with that worker
/// still alive and still waiting, and once more as `release`. The host
/// holds only lock pixels from the first touch request through
/// `release`, and only then does the guest complete the touch, so the
/// unlock cannot commit inside the held window; then the client's window
/// and the desktop.
fn unlock(host: &mut Host, keyboard: &mut Keyboard, key: &Virtual) {
    let print = print(key);
    key.script(Script {
        presence: Presence::Held,
        ..Script::default()
    });
    let plugged = Plugged::insert(key, NAME);
    let authority = authority();
    chord(keyboard);
    host.screen("pin", &[&print, "8", "0"]);
    type_pin(keyboard, PIN);
    host.screen("pin", &[&print, "8", "4"]);
    keyboard.key(ENTER);
    host.screen("touch", &[&print, "8"]);
    let worker = workers(authority);
    assert_eq!(worker.len(), 1, "the authority's login workers");
    // The screen can precede the assertion's arrival at the key.
    system_wait("the key waiting for its touch", || key.touching());
    host.screen("touch", &[&print, "8"]);
    assert_eq!(
        workers(authority),
        worker,
        "the worker ended within the touch"
    );
    assert!(key.touching(), "the touch ended before its release");
    host.screen("release", &[&print, "8"]);
    let touched = key.touched();
    key.release_touch();
    system_wait("the released touch", || {
        !key.touching() && key.touched() > touched
    });
    host.screen("unlocked", &[]);
    host.screen("desktop", &[]);
    system_wait("the worker reaped", || workers(authority).is_empty());
    assert_eq!(key.state().retries, 8);
    assert_eq!(key.saved(), Ok(()));
    // The unlock's identify and assertion sessions.
    assert_eq!(plugged.remove().channels, 2);
}

/// `ssh` to `user` on loopback with `identity`, as root or as the primary
/// account, `-v` when `verbose`: the remote command still to add.
fn ssh_command(as_primary: bool, user: &str, identity: &str, verbose: bool) -> Command {
    let mut command = if as_primary {
        let mut command = Command::new("/bin/td-login");
        command.args(["exec-as", "tester", "--", "/bin/ssh"]);
        command
    } else {
        Command::new("/bin/ssh")
    };
    command.args(["-F", "/dev/null"]);
    if verbose {
        command.arg("-v");
    }
    command
        .args(["-i", identity])
        .args(["-o", "BatchMode=yes", "-o", "IdentitiesOnly=yes"])
        .args(["-o", "StrictHostKeyChecking=yes"])
        .args(["-o", "UserKnownHostsFile=/run/td-ssh-known-hosts"])
        .args([
            "-o",
            "GlobalKnownHostsFile=/dev/null",
            "-o",
            "ConnectTimeout=10",
        ])
        .arg(format!("{user}@127.0.0.1"))
        .env_clear()
        .stdin(Stdio::null());
    command
}

/// `ssh_command`'s run of `echo`.
fn ssh_output(as_primary: bool, user: &str, identity: &str, verbose: bool) -> Output {
    let output = ssh_command(as_primary, user, identity, verbose)
        .args(["/bin/echo", SSH_REPLY])
        .output()
        .unwrap();
    eprintln!(
        "ssh as {user}: {} {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    output
}

/// Whether the command ran.
fn ssh(as_primary: bool, user: &str, identity: &str) -> bool {
    let output = ssh_output(as_primary, user, identity, false);
    output.status.success() && output.stdout == format!("{SSH_REPLY}\n").as_bytes()
}

/// Whether the daemon refused `identity` for `user` after the client offered
/// it: ssh's own 255, no command output, and the offer before the publickey
/// denial. A connection that failed offered nothing, so it is not a refusal.
fn ssh_refused(user: &str, identity: &str) -> bool {
    let output = ssh_output(false, user, identity, true);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let offered = stderr.find(&format!("Offering public key: {identity} ED25519 "));
    let denied = stderr.rfind("Permission denied (publickey)");
    output.status.code() == Some(255)
        && output.stdout.is_empty()
        && matches!((offered, denied), (Some(o), Some(d)) if o < d)
}

/// The boot's sshd form: both refuse a loopback root key in the persistent
/// file (APPLICATIONS.md §L.1, L7) and admit the primary account's, which
/// boot health made; only the enforced one names `AllowUsers`.
fn sshd(enforced: bool) {
    let config = fs::read_to_string("/run/td-sshd.conf").unwrap();
    assert!(!config.contains("prohibit-password"), "{config}");
    if enforced {
        assert!(
            config.contains("PermitRootLogin no\nAllowUsers tester\n"),
            "{config}"
        );
    } else {
        assert!(config.contains("PermitRootLogin no\n"), "{config}");
        assert!(!config.contains("AllowUsers"), "{config}");
    }
    let identity = Path::new(WORK).join("root-key");
    let _ = fs::remove_file(&identity);
    let _ = fs::remove_file(identity.with_extension("pub"));
    assert!(Command::new("/bin/ssh-keygen")
        .args([
            "-q",
            "-t",
            "ed25519",
            "-N",
            "",
            "-C",
            "td-login-system",
            "-f"
        ])
        .arg(&identity)
        .status()
        .unwrap()
        .success());
    let public = fs::read_to_string(identity.with_extension("pub")).unwrap();
    let mut authorized = OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .open("/etc/ssh/authorized_keys")
        .unwrap();
    write!(authorized, "restrict,from=\"127.0.0.1\" {public}").unwrap();
    authorized.sync_all().unwrap();
    let identity = identity.to_str().unwrap();
    assert!(ssh_refused("root", identity), "root over SSH");
    assert!(
        ssh(true, "tester", "/run/td-ssh-selftest"),
        "the primary over SSH"
    );
}

/// While this is mounted over `/bin/td-secret`, every run of it, which is
/// how td-authd starts its login worker and its state helper, appends
/// its verb to a log before running the real binary from a copy; so a
/// worker however short-lived is recorded. A failing one also fails
/// `inspect-login`.
struct Recorder(PathBuf);

impl Recorder {
    fn mount(failing: bool) -> Self {
        let real = fs::canonicalize("/bin/td-secret").unwrap();
        let copy = Path::new(WORK).join("td-secret-real");
        fs::copy(&real, &copy).unwrap();
        fs::set_permissions(&copy, Permissions::from_mode(0o755)).unwrap();
        let log = Path::new(WORK).join("verbs");
        fs::write(&log, b"").unwrap();
        let wrapper = Path::new(WORK).join("td-secret-recording");
        let fail = if failing {
            "[ \"$1\" = inspect-login ] && exit 1\n"
        } else {
            ""
        };
        fs::write(
            &wrapper,
            format!(
                "#!/bin/sh\necho \"$1\" >> {} || exit 1\n{fail}exec {} \"$@\"\n",
                log.display(),
                copy.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&wrapper, Permissions::from_mode(0o755)).unwrap();
        assert!(Command::new("/bin/mount")
            .args(["-o", "bind"])
            .arg(&wrapper)
            .arg(&real)
            .status()
            .unwrap()
            .success());
        let status = Command::new("/bin/td-secret")
            .args(["inspect-login", "--uid", "1000"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(!failing || !status.success(), "the helper still answers");
        let recorder = Self(real);
        // td-authd's own helper may have run beside the check.
        let verbs = recorder.verbs();
        assert!(
            verbs.iter().any(|verb| verb == "inspect-login") && recorder.workers() == 0,
            "the recorder: {verbs:?}"
        );
        recorder
    }

    /// Every verb run since the mount, the mount's own check among them.
    fn verbs(&self) -> Vec<String> {
        fs::read_to_string(Path::new(WORK).join("verbs"))
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// How many login workers td-authd has started since the mount.
    fn workers(&self) -> usize {
        self.verbs()
            .iter()
            .filter(|verb| *verb == "login-operation")
            .count()
    }

    fn unmount(self) {
        assert!(Command::new("/bin/umount")
            .arg(&self.0)
            .status()
            .unwrap()
            .success());
    }
}

/// The record a worker enrolled, and the volume's marked deployments
/// read as production reads them: its files root's, so the current one's
/// marker gives this build's version, which a reader requiring another
/// owner does not see.
fn seed_record() {
    let volume = Volume {
        path: Path::new(login_tier::VOLUME),
        owner: 0,
    };
    let production = Context {
        retained: &volume,
        ..context()
    };
    assert_eq!(version(&production), Ok(VERSION));
    let foreign = Volume {
        path: Path::new(login_tier::VOLUME),
        owner: UID,
    };
    assert_eq!(
        version(&Context {
            retained: &foreign,
            ..context()
        }),
        Err(Failure::Version)
    );
    fs::DirBuilder::new()
        .mode(0o700)
        .recursive(true)
        .create(STATE)
        .unwrap();
    for (name, seed) in SYSTEM_KEYS.iter().zip(["system-a", "system-b"]) {
        Virtual::persistent(Config::default(), Some(PIN), seed, &key_file(name)).unwrap();
    }
    let keys = restored();
    let [first, second] = &keys[..] else {
        panic!("two keys");
    };
    let plugged = Plugged::insert(first, NAME);
    let (result, _) = operated(
        begins(enrolling(1, 1, LoginStep::Connect)),
        &Polls::default(),
        &production,
    );
    assert_eq!(result, Ok(()));
    // The second key added by the first, across a person's swap.
    let port: Port = Arc::new(Mutex::new(Some(plugged)));
    let changed = Swapped::default();
    let polls = Polls::default();
    let connect = login(adding(1, LoginStep::Connect));
    let plan = Plan {
        operation: Some(adding(1, LoginStep::Identify)),
        pin: Some(pin_frame(PIN)),
        ..swap(&port, &changed, &polls, connect, second)
    };
    let (result, _) = operated(plan, &polls, &production);
    assert_eq!(result, Ok(()));
    swapped(&changed);
    port.lock().unwrap().take().unwrap().remove();
    let ids = enrolled();
    for key in &keys {
        assert_eq!(key.saved(), Ok(()));
        assert!(ids.contains(&key.state().credentials.last().unwrap().id));
    }
    let State::Enrolled(record) = state() else {
        panic!("not enrolled");
    };
    assert_eq!(record.version(), VERSION);
    let mut copy = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(backup())
        .unwrap();
    copy.write_all(&fs::read(record_path()).unwrap()).unwrap();
    copy.sync_all().unwrap();
}

/// The cutover record for this boot naming `form`.
fn cutover_record(form: &str) -> String {
    let boot = fs::read_to_string("/proc/sys/kernel/random/boot_id").unwrap();
    format!("td-login-cutover-v1\n{boot}{form}\n")
}

/// The open SSH session's remote command, while it runs.
fn remote_session() -> Option<u32> {
    processes()
        .iter()
        .find(|process| runs(process, "sleep", SESSION_SLEEP))
        .map(|process| process.pid)
}

/// The seed's cutover (TOKEN-LOGIN.md increment 5's A3): with the serial
/// greeter's session logged in again and an SSH session open as the
/// primary account, the record is enrolled and the pair restarted. Its
/// authority's first `1a` finds the reduced state enforced and the boot's
/// record unenrolled, so it renders the enforced form, hands the line
/// back, restarts `sshd` and `greeter` and records the form. Both sessions
/// end, the greeter that replaces the serial one refuses, and the lock
/// surface shows no failure.
fn cutover(host: &mut Host) {
    service(&["start", "greeter"]);
    let shell = idle_login_shell();
    let mut session = ssh_command(true, "tester", "/run/td-ssh-selftest", false)
        .args(["/bin/sleep", SESSION_SLEEP])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    system_wait("the open SSH session", || {
        assert!(
            session.try_wait().unwrap().is_none(),
            "the SSH session ended"
        );
        remote_session().is_some()
    });
    seed_record();
    assert_eq!(
        fs::read_to_string(CUTOVER).unwrap(),
        cutover_record("unenrolled")
    );
    service(&["restart", "wayland"]);
    system_wait("the serial session ended", || {
        !Path::new(&format!("/proc/{shell}")).exists()
    });
    system_wait("the SSH session ended", || {
        remote_session().is_none() && session.try_wait().unwrap().is_some()
    });
    system_wait("the enforced record", || {
        fs::read_to_string(CUTOVER).ok() == Some(cutover_record("enforced"))
    });
    assert!(!Path::new(REBOOT_GUARD).exists(), "a failed revocation");
    let line = fs::metadata("/dev/ttyS0").unwrap();
    assert_eq!((line.uid(), line.gid()), (0, 0), "ttyS0 handed back");
    parked_greeter();
    assert_eq!(login_shell(), None, "a serial shell after the cutover");
    // The serial session's leader exiting hung up every descriptor open
    // on its controlling terminal, the host's among them: open it again.
    *host = Host::open();
    host.screen("locked", &[]);
    sshd(true);
}

/// Leaves `damage` for the next boot, as other code on the machine could.
fn damage(damage: &str) {
    let record = record_path();
    match damage {
        "directory-mode" => {
            fs::set_permissions(directory(), Permissions::from_mode(0o755)).unwrap()
        }
        "directory-owner" => chown(directory(), Some(UID), Some(UID)).unwrap(),
        "directory-file" => {
            fs::remove_dir_all(directory()).unwrap();
            fs::write(directory(), b"").unwrap();
        }
        "record-mode" => fs::set_permissions(&record, Permissions::from_mode(0o644)).unwrap(),
        "record-links" => fs::hard_link(&record, SECOND_LINK).unwrap(),
        "record-truncated" => {
            let file = OpenOptions::new().write(true).open(&record).unwrap();
            let length = file.metadata().unwrap().len();
            file.set_len(length - 1).unwrap();
            file.sync_all().unwrap();
        }
        "record-version" => {
            let mut bytes = fs::read(&record).unwrap();
            bytes[8] = 0xff;
            fs::write(&record, bytes).unwrap();
        }
        other => panic!("no damage {other}"),
    }
    assert!(matches!(state(), State::Unavailable(_)), "{damage}");
    assert!(Command::new("/bin/sync").status().unwrap().success());
}

/// TOKEN-LOGIN.md's recovery for `damage`: the directory restored as
/// root:root mode 0700, the extra link removed, or the record replaced by
/// its intact copy.
fn repair(damage: &str) {
    let record = record_path();
    let intact = fs::read(backup()).unwrap();
    match damage {
        "directory-mode" => {
            fs::set_permissions(directory(), Permissions::from_mode(0o700)).unwrap()
        }
        "directory-owner" => chown(directory(), Some(0), Some(0)).unwrap(),
        "directory-file" => {
            fs::remove_file(directory()).unwrap();
            fs::DirBuilder::new()
                .mode(0o700)
                .create(directory())
                .unwrap();
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&record)
                .unwrap();
            file.write_all(&intact).unwrap();
            file.sync_all().unwrap();
        }
        "record-mode" => fs::set_permissions(&record, Permissions::from_mode(0o600)).unwrap(),
        "record-links" => fs::remove_file(SECOND_LINK).unwrap(),
        "record-truncated" | "record-version" => fs::write(&record, &intact).unwrap(),
        other => panic!("no damage {other}"),
    }
    assert_eq!(fs::read(&record).unwrap(), intact);
    enrolled();
    assert!(Command::new("/bin/sync").status().unwrap().success());
}

/// A queued update whose source is `bundle`, as the primary account, and
/// the attention menu's `I` on it: `refused`, or its prompt for `id`
/// cancelled. Either way Escape returns to the desktop and the request
/// ends unfinished, and it counted: the next request is refused while
/// the queue backs off, until root's fixture removes the backoff rather
/// than wait out its 210 seconds.
fn update(host: &mut Host, keyboard: &mut Keyboard, bundle: &Path, id: &str, refused: bool) {
    for path in fs::read_dir(bundle).unwrap() {
        chown(path.unwrap().path(), Some(UID), Some(UID)).unwrap();
    }
    chown(bundle, Some(UID), Some(UID)).unwrap();
    let log = bundle.with_extension("log");
    let output = File::create(&log).unwrap();
    let mut client = Command::new("/bin/td-login")
        .args(["exec-as", "tester", "--", "/bin/td-authd", "request-update"])
        .arg(bundle)
        .arg(id)
        .env_clear()
        .stdin(Stdio::null())
        .stdout(output.try_clone().unwrap())
        .stderr(output)
        .spawn()
        .unwrap();
    system_wait("the update admitted to the queue", || {
        assert!(
            client.try_wait().unwrap().is_none(),
            "{}",
            fs::read_to_string(&log).unwrap()
        );
        fs::read_to_string(&log)
            .unwrap()
            .contains(&format!("Update {id}{UPDATE_READY}"))
    });
    chord(keyboard);
    host.screen("menu", &[]);
    keyboard.key(KEY_I);
    if refused {
        host.screen("update-refused", &[]);
    } else {
        host.screen("install", &[id]);
    }
    keyboard.key(ESCAPE);
    host.screen("desktop", &[]);
    let mut status = None;
    system_wait("the update request ended", || {
        status = client.try_wait().unwrap();
        status.is_some()
    });
    assert!(
        !status.unwrap().success(),
        "{}",
        fs::read_to_string(&log).unwrap()
    );
    let backoff = fs::read_to_string(BACKOFF).unwrap();
    assert!(backoff.contains("\nupdate\t"), "{backoff}");
    let output = Command::new("/bin/td-login")
        .args(["exec-as", "tester", "--", "/bin/td-authd", "request-update"])
        .arg(bundle)
        .arg(id)
        .env_clear()
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let said = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{said}");
    assert!(said.contains(UPDATE_BACKING_OFF), "{said}");
    fs::remove_file(BACKOFF).unwrap();
}

/// The three updates: a deployment with no marker and one whose marker
/// lists another version are refused; a copy of this marked deployment
/// is admitted to its prompt.
fn updates(host: &mut Host, keyboard: &mut Keyboard) {
    use crate::login_tier::tests::{bundle, initramfs, marker};
    for (name, archive) in [
        ("unmarked", initramfs(None)),
        ("other-version", initramfs(Some(&marker(&[VERSION + 1])))),
    ] {
        let directory = Path::new(WORK).join(name);
        fs::create_dir(&directory).unwrap();
        let id = bundle(&directory, &archive);
        update(host, keyboard, &directory, &id, true);
    }
    let current = fs::read_link(Path::new(login_tier::VOLUME).join("boot/current")).unwrap();
    let id = current
        .to_str()
        .unwrap()
        .strip_prefix("../deployments/")
        .unwrap()
        .to_string();
    let source = Path::new(login_tier::VOLUME).join("deployments").join(&id);
    let directory = Path::new(WORK).join("marked");
    fs::create_dir(&directory).unwrap();
    for name in ["manifest", "initramfs.cpio"] {
        fs::copy(source.join(name), directory.join(name)).unwrap();
    }
    assert_eq!(
        login_tier::read(
            &File::open(&directory).unwrap(),
            &id,
            0,
            Duration::from_secs(10)
        ),
        Ok(READS.to_vec())
    );
    update(host, keyboard, &directory, &id, false);
}

fn uptime() -> Duration {
    let text = fs::read_to_string("/proc/uptime").unwrap();
    Duration::from_secs_f64(text.split_whitespace().next().unwrap().parse().unwrap())
}

fn suspensions() -> u64 {
    fs::read_to_string("/sys/power/suspend_stats/success")
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}

/// Suspend to RAM from the desktop with `key` plugged, the boot's last
/// step: the host sees the desktop, waits for QEMU to report the guest
/// suspended, holds it and wakes it. This kernel's virtio-gpu has no
/// restore, so the card stays reset; the host then admits only lock
/// pixels or QEMU's inactive output, which it must see. The chord's
/// first report is the keyboard's first input after the wake, and it
/// starts the boot's one login worker since before the suspend, which
/// the chord does only on a locked session, under the same compositor
/// and authority: the session was locked when the first post-wake input
/// was routed, in the same generation.
fn suspend(host: &mut Host, keyboard: &mut Keyboard, key: &Virtual) {
    // The unlock's close discards the keyboard's next fresh report: spend
    // it now, so that the chord's first report is routed as it is sent.
    keyboard.key(CAPS_LOCK);
    let plugged = Plugged::insert(key, NAME);
    let (asleep_authority, asleep_compositor) = (authority(), compositor());
    assert!(asleep_compositor.is_some(), "one compositor");
    let recorder = Recorder::mount(false);
    assert!(fs::read_to_string("/sys/power/state")
        .unwrap()
        .split_whitespace()
        .any(|state| state == "mem"));
    fs::write("/sys/power/mem_sleep", b"deep").unwrap();
    assert!(fs::read_to_string("/sys/power/mem_sleep")
        .unwrap()
        .contains("[deep]"));
    let before = suspensions();
    host.screen("asleep", &[]);
    assert!(workers(asleep_authority).is_empty(), "a live login worker");
    assert_eq!(recorder.workers(), 0, "a login worker before the suspend");
    let (up, now) = (uptime(), Instant::now());
    fs::write("/sys/power/state", b"mem").unwrap();
    // The whole chord in one report, so one evdev frame and one batch:
    // hid-input reports the modifier usages before the key array, so
    // Ctrl and Alt are held when Escape is read.
    keyboard.report(5, ESCAPE);
    keyboard.report(0, 0);
    let slept = (uptime() - up).saturating_sub(now.elapsed());
    assert_eq!(suspensions(), before + 1);
    assert!(slept >= Duration::from_secs(8), "slept {slept:?}");
    eprintln!("system suspended for {slept:?}");
    system_wait("the unlock's worker after the wake", || {
        recorder.workers() == 1 && workers(asleep_authority).len() == 1
    });
    keyboard.key(ESCAPE);
    system_wait("the worker reaped", || workers(asleep_authority).is_empty());
    assert_eq!(recorder.workers(), 1, "login workers since the suspend");
    assert_eq!(compositor(), asleep_compositor, "the compositor");
    assert_eq!(authority(), asleep_authority, "the authority");
    recorder.unmount();
    plugged.remove();
}

/// The compositor killed: from its frame's end the host holds lock
/// pixels or QEMU's inactive output until its successor's generation
/// shows the lock surface.
fn killed(host: &mut Host) {
    let old = compositor().expect("one compositor");
    assert!(Command::new("/bin/kill")
        .args(["-KILL", &old.to_string()])
        .status()
        .unwrap()
        .success());
    host.screen("killed", &[]);
    system_wait("a new compositor", || {
        compositor().is_some_and(|pid| pid != old)
    });
    host.screen("locked", &[]);
    ready(&["wayland"]);
}

/// The pair restarted with the state helper failing: locked with that
/// cause, the chord shows it and starts no worker, live or reaped; once
/// the helper answers again, the compositor's polling finds the record.
fn unreadable(host: &mut Host, keyboard: &mut Keyboard, key: &Virtual) {
    let failing = Recorder::mount(true);
    service(&["restart", "wayland"]);
    host.screen("locked-unreadable", &[]);
    let plugged = Plugged::insert(key, NAME);
    let authority = authority();
    chord(keyboard);
    host.screen("unreadable", &[]);
    no_unlock(authority);
    keyboard.key(ESCAPE);
    host.screen("locked-unreadable", &[]);
    no_unlock(authority);
    assert_eq!(failing.workers(), 0, "a login worker while unreadable");
    // The record shows td-authd's children: the polled helper.
    let helpers = failing
        .verbs()
        .iter()
        .filter(|verb| *verb == "inspect-login")
        .count();
    assert!(helpers > 1, "the polled helper ran {helpers} times");
    failing.unmount();
    host.screen("locked", &[]);
    plugged.remove();
    // The restart skipped the terminal, whose window the next unlock shows.
    service(&["start", "terminal"]);
    ready(&["wayland", "terminal"]);
}

/// A damaged boot: locked with the cause, whose chord shows it and starts
/// no worker, with a key plugged that a worker would find.
fn damaged_boot(host: &mut Host, keyboard: &mut Keyboard, keys: &[Virtual], record: bool) {
    let (surface, notice) = if record {
        ("locked-record", "record")
    } else {
        ("locked-damaged", "damaged")
    };
    host.screen(surface, &[]);
    let plugged = Plugged::insert(&keys[0], NAME);
    let authority = authority();
    let recorder = Recorder::mount(false);
    chord(keyboard);
    host.screen(notice, &[]);
    no_unlock(authority);
    keyboard.key(ESCAPE);
    host.screen(surface, &[]);
    no_unlock(authority);
    assert_eq!(recorder.workers(), 0, "a login worker while damaged");
    recorder.unmount();
    plugged.remove();
}

struct SystemDiagnostics;

impl Drop for SystemDiagnostics {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            return;
        }
        if let Ok(output) = Command::new("/bin/td-svc").arg("status").output() {
            eprintln!("td-svc status: {}", String::from_utf8_lossy(&output.stdout));
        }
        for name in ["unmarked.log", "other-version.log", "marked.log"] {
            if let Ok(text) = fs::read_to_string(Path::new(WORK).join(name)) {
                eprintln!("{name}: {text}");
            }
        }
    }
}

#[test]
#[ignore = "requires qemu-login-system's full system image, volume and UHID"]
fn qemu_login_system_locks_unlocks_and_refuses_on_a_full_system() {
    guard("fido-system");
    let _diagnostics = SystemDiagnostics;
    let phase = phase();
    eprintln!("login system phase {phase}");
    fs::DirBuilder::new().mode(0o755).create(WORK).unwrap();
    assert_eq!(
        fs::read_to_string("/proc/sys/kernel/hostname")
            .unwrap()
            .trim(),
        HOSTNAME
    );
    assert!(Device::discover().unwrap().is_empty());
    if phase == "seed" {
        // Unenrolled, with the state helper failing: root reads no record,
        // so td-authd runs no helper and the session starts unlocked. The
        // boot's record names that state, so root's check changes nothing.
        assert!(matches!(state(), State::Unenrolled));
        assert_eq!(
            fs::read_to_string(CUTOVER).unwrap(),
            cutover_record("unenrolled")
        );
        let failing = Recorder::mount(true);
        // The serial greeter logged in, and its shell would read the
        // host's answers: once it has, the greeter is stopped until the
        // cutover.
        idle_login_shell();
        service(&["stop", "greeter"]);
        system_wait("ttyS0 released", || serial().is_none());
        let mut host = Host::open();
        release_console();
        blank_screen();
        host.screen("blank-unlocked", &[]);
        fs::write(INPUT_READY, b"ready").unwrap();
        ready(&["wayland", "terminal"]);
        host.screen("desktop", &[]);
        boot_healthy();
        sshd(false);
        // Only the mount's own check ran the helper.
        let helpers = failing
            .verbs()
            .iter()
            .filter(|verb| *verb == "inspect-login")
            .count();
        assert_eq!(helpers, 1, "td-authd ran the state helper unenrolled");
        failing.unmount();
        cutover(&mut host);
        return;
    }
    let ids = enrolled_or_damaged(phase);
    let keys = restored();
    if !ids.is_empty() {
        for key in &keys {
            assert!(ids.contains(&key.state().credentials.last().unwrap().id));
        }
    }
    let mut keyboard = Keyboard::new();
    let parked = parked_greeter();
    let mut host = Host::open();
    start(&mut host);
    match phase.split_once('-') {
        None => {
            host.screen("locked", &[]);
            ready(&["wayland", "terminal"]);
            host.screen("locked", &[]);
            boot_healthy();
            sshd(true);
            unlock(&mut host, &mut keyboard, &keys[0]);
            // The unlock's close cut off the keyboard's first fresh
            // report, which Caps Lock spends as the chord does.
            keyboard.key(CAPS_LOCK);
            keyboard.report(SUPER, 0);
            keyboard.report(SUPER, KEY_L);
            keyboard.report(0, 0);
            host.screen("locked", &[]);
            unlock(&mut host, &mut keyboard, &keys[1]);
            chord(&mut keyboard);
            host.screen("menu", &[]);
            keyboard.key(KEY_L);
            host.screen("locked", &[]);
            unlock(&mut host, &mut keyboard, &keys[0]);
            updates(&mut host, &mut keyboard);
            killed(&mut host);
            unreadable(&mut host, &mut keyboard, &keys[0]);
            unlock(&mut host, &mut keyboard, &keys[1]);
            suspend(&mut host, &mut keyboard, &keys[1]);
        }
        Some(("damaged", damage)) => {
            damaged_boot(
                &mut host,
                &mut keyboard,
                &keys,
                damage.starts_with("record-"),
            );
            boot_healthy();
            sshd(true);
            repair(damage);
        }
        Some(("repaired", _)) => {
            host.screen("locked", &[]);
            boot_healthy();
            sshd(true);
        }
        _ => panic!("no phase {phase}"),
    }
    assert_eq!(greeter(), Some(parked), "the serial greeter left its park");
    if let Some(next) = next_damage(phase) {
        damage(next);
    }
    for key in &keys {
        assert_eq!(key.saved(), Ok(()));
        assert_eq!(key.state().retries, 8);
    }
    assert!(Device::discover().unwrap().is_empty());
}

/// The record's credentials on an enrolled boot, and none on a damaged
/// one, whose state must be its damage's cause.
fn enrolled_or_damaged(phase: &str) -> Vec<Vec<u8>> {
    match phase.strip_prefix("damaged-") {
        Some(damage) => {
            let cause = if damage.starts_with("record-") {
                Cause::RecordDamaged
            } else {
                Cause::DirectoryDamaged
            };
            assert!(
                matches!(state(), State::Unavailable(found) if found == cause),
                "{damage}"
            );
            Vec::new()
        }
        None => {
            let ids = enrolled();
            assert_eq!(
                fs::read(record_path()).unwrap(),
                fs::read(backup()).unwrap()
            );
            ids
        }
    }
}

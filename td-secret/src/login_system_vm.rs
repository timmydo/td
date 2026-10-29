// qemu-login-system (td-secret/DESIGN.md, "Login system guest"): the
// production system image's supervisor, firstboot, serial greeter, sshd,
// boot health, compositor, td-authd and its login worker, through a UHID
// keyboard and UHID keys, with no TPM. One test, booted once per phase on
// one disposable volume (`PHASES`, which recipes/src/fixtures/secret_vm.rs
// pins); the host checks the display at each screen the guest names, as
// login-desktop's does, the guest approving each disclosure with Enter,
// wakes the guest from the suspend to RAM it asks for
// and cuts its power where it says.

use super::*;
use std::os::unix::fs::{chown, symlink};
use std::process::{Child, Output, Stdio};

/// The boots, in order. `seed` enrolls two keys through the attention
/// screen of an unenrolled machine; `locked` is the first enrolled boot;
/// each `damaged-` boot shows its damage, which the boot before it made,
/// and repairs it, and the `repaired-` boot after it is an ordinary
/// enrolled one again, which makes the next damage. `added` adds keys to
/// eight, `removed` removes all but one and then the last, `unlocked`
/// boots unenrolled and enrolls one key across a killed authority, `cut`
/// removes that key, enrolls another and is cut right after its
/// publication, and `after-cut` is the boot after the cut.
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
    "added",
    "removed",
    "unlocked",
    "cut",
    "after-cut",
];
/// The stock image's hostname, which the lock surface shows.
const HOSTNAME: &str = "td";
/// The fixture unit's readiness, which the stock seat setup waits for.
const INPUT_READY: &str = "/run/td-secret-system-input-ready";
/// Root's fixture state on `@var`, outside the login directory: the
/// persistent keys and the intact record a repair restores.
const STATE: &str = "/var/lib/td-fixture/login-system";
const SYSTEM_KEYS: &[&str] = &["a", "b"];
/// The keys `added` adds to those two, to eight.
const ADDED_KEYS: &[&str] = &["c", "d", "e", "f", "g", "h"];
/// `unlocked`'s one-key enrollment's key, and `cut`'s.
const SINGLE_KEY: &str = "i";
const CUT_KEY: &str = "j";
/// A boot's own scratch, and the record's second name for the link damage.
const WORK: &str = "/run/td-login-system";
const SECOND_LINK: &str = "/var/lib/td/login-second-link";
/// The keyboard's left GUI modifier, and the usages of L, I, K, A, D, 1
/// and 2.
const SUPER: u8 = 0x08;
const KEY_L: u8 = 0x0f;
const KEY_I: u8 = 0x0c;
const KEY_K: u8 = 0x0e;
const KEY_A: u8 = 0x04;
const KEY_D: u8 = 0x07;
const KEY_1: u8 = 0x1e;
const KEY_2: u8 = 0x1f;
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
/// While this exists the account's profile holds a serial login: its
/// login shell runs `sleep HOLD_SLEEP` in the foreground with TERM
/// ignored, so the session reads nothing the host answers on ttyS0 and
/// outlives a TERM.
const SERIAL_HOLD: &str = "/run/td-login-system/serial-hold";
const HOLD_SLEEP: &str = "7777";
const PROFILE: &str = "/home/tester/.profile";
/// The word on which the host cuts the power (the fixture's
/// `LOGIN_SYSTEM_CUT`).
const CUT: &str = "TD-LOGIN-SYSTEM-CUT";

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
    /// The controlling terminal, as `/proc/PID/stat` encodes it, and the
    /// start in clock ticks since boot.
    tty: u64,
    start: u64,
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
            let fields: Vec<&str> = fields.split_whitespace().collect();
            if *fields.first()? == "Z" {
                return None;
            }
            let parent = fields.get(1)?.parse().ok()?;
            let tty = fields.get(4)?.parse().ok()?;
            let start = fields.get(19)?.parse().ok()?;
            let argv = fs::read(format!("/proc/{pid}/cmdline"))
                .ok()?
                .split(|byte| *byte == 0)
                .filter(|word| !word.is_empty())
                .map(|word| String::from_utf8_lossy(word).into_owned())
                .collect();
            Some(Proc {
                pid,
                parent,
                tty,
                start,
                argv,
            })
        })
        .collect()
}

/// A process by its pid and its start, so a reused pid is not it.
type Member = (u32, u64);

/// ttyS0's number as `/proc/PID/stat` gives a controlling terminal.
fn serial_tty() -> u64 {
    let rdev = fs::metadata("/dev/ttyS0").unwrap().rdev();
    let (major, minor) = ((rdev >> 8) & 0xfff, (rdev & 0xff) | ((rdev >> 12) & !0xff));
    (minor & 0xff) | (major << 8) | ((minor & !0xff) << 12)
}

/// The processes whose controlling terminal is ttyS0: the greeter's
/// `tty=` containment, a serial session's every process.
fn serial_members() -> Vec<Member> {
    let tty = serial_tty();
    processes()
        .iter()
        .filter(|process| process.tty == tty)
        .map(|process| (process.pid, process.start))
        .collect()
}

/// The members of the cgroup leaf `pid` runs in: for `sshd`'s leader its
/// unit's whole service leaf, every OpenSSH session in it.
fn leaf_members(pid: u32) -> Vec<Member> {
    let cgroup = fs::read_to_string(format!("/proc/{pid}/cgroup")).unwrap();
    let path = cgroup
        .lines()
        .find_map(|line| line.strip_prefix("0::"))
        .unwrap();
    let procs = fs::read_to_string(format!("/sys/fs/cgroup{path}/cgroup.procs")).unwrap();
    let all = processes();
    procs
        .lines()
        .filter_map(|line| line.parse::<u32>().ok())
        .filter_map(|pid| all.iter().find(|process| process.pid == pid))
        .map(|process| (process.pid, process.start))
        .collect()
}

/// Whether none of `members` lives.
fn gone(members: &[Member]) -> bool {
    let all = processes();
    !members.iter().any(|(pid, start)| {
        all.iter()
            .any(|process| process.pid == *pid && process.start == *start)
    })
}

/// Whether every one of `members` lives.
fn alive(members: &[Member]) -> bool {
    let all = processes();
    members.iter().all(|(pid, start)| {
        all.iter()
            .any(|process| process.pid == *pid && process.start == *start)
    })
}

/// `pid`'s start, in clock ticks since boot.
fn started(pid: u32) -> u64 {
    processes()
        .iter()
        .find(|process| process.pid == pid)
        .map(|process| process.start)
        .unwrap_or_else(|| panic!("no process {pid}"))
}

/// Whether `pid` ignores SIGTERM: bit 14 of its ignored mask.
fn ignores_term(pid: u32) -> bool {
    let status = fs::read_to_string(format!("/proc/{pid}/status")).unwrap();
    let mask = status
        .lines()
        .find_map(|line| line.strip_prefix("SigIgn:"))
        .unwrap();
    u64::from_str_radix(mask.trim(), 16).unwrap() & (1 << 14) != 0
}

/// `unit`'s leader, as td-svc reports it.
fn leader(unit: &str) -> u32 {
    let status = service(&["status", unit]);
    status
        .lines()
        .filter_map(|line| line.strip_prefix(&format!("{unit} ")))
        .find_map(|rest| {
            rest.split_whitespace()
                .find_map(|word| word.strip_prefix("pid="))?
                .parse()
                .ok()
        })
        .unwrap_or_else(|| panic!("{unit}'s leader: {status}"))
}

fn runs(process: &Proc, program: &str, verb: &str) -> bool {
    process
        .argv
        .first()
        .is_some_and(|name| name.ends_with(program))
        && process.argv.get(1).is_some_and(|word| word == verb)
}

/// The paired authorities: the wayland unit's `td-authd terminal-serve
/// --primary`.
fn authorities() -> Vec<u32> {
    processes()
        .iter()
        .filter(|process| {
            runs(process, "td-authd", "terminal-serve")
                && process.argv.iter().any(|word| word == "--primary")
        })
        .map(|process| process.pid)
        .collect()
}

/// The paired authority, once there is exactly one.
fn authority() -> u32 {
    system_wait("one paired authority", || authorities().len() == 1);
    authorities()[0]
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

/// The persistent key `name`, as this boot finds it.
fn restore(name: &str) -> Virtual {
    Virtual::restore(Config::default(), &key_file(name))
        .unwrap_or_else(|damage| panic!("virtual key {name}: {damage}"))
}

fn restored() -> Vec<Virtual> {
    SYSTEM_KEYS.iter().map(|name| restore(name)).collect()
}

/// A new persistent key `name` on `@var`, with the PIN set.
fn new_key(name: &str) -> Virtual {
    fs::DirBuilder::new()
        .mode(0o700)
        .recursive(true)
        .create(STATE)
        .unwrap();
    Virtual::persistent(
        Config::default(),
        Some(PIN),
        &format!("system-{name}"),
        &key_file(name),
    )
    .unwrap()
}

/// A key's newest credential, the one it enrolled last.
fn credential(key: &Virtual) -> Vec<u8> {
    key.state().credentials.last().unwrap().id.clone()
}

/// A key's enrolled credential's fingerprint, as a step names it.
fn enrolled_print(key: &Virtual) -> Fingerprint {
    fingerprint(&credential(key))
}

/// A key's enrolled credential's fingerprint, as the PIN step shows it.
fn print(key: &Virtual) -> String {
    hex(&enrolled_print(key))
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

/// The worker's production read of the retained deployments: the
/// volume's files are root's, so the current one's marker gives this
/// build's version, which a reader requiring another owner does not see.
fn retained_reads() {
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

/// An SSH session as the primary account, its remote command running.
fn open_ssh() -> Child {
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
    session
}

/// From now on in this boot, a serial login is held (`SERIAL_HOLD`).
fn hold_serial() {
    let profile =
        format!("if [ -e {SERIAL_HOLD} ]; then trap '' TERM; /bin/sleep {HOLD_SLEEP}; fi\n");
    fs::write(PROFILE, profile).unwrap();
    fs::set_permissions(PROFILE, Permissions::from_mode(0o644)).unwrap();
    chown(PROFILE, Some(UID), Some(UID)).unwrap();
    fs::write(SERIAL_HOLD, b"").unwrap();
}

/// The held serial login: its login shell on ttyS0 and that shell's
/// `sleep`.
fn held_session() -> Option<(u32, u32)> {
    let all = processes();
    let tty = serial_tty();
    let shell = all.iter().find(|process| {
        process.tty == tty
            && process
                .argv
                .first()
                .is_some_and(|name| name.starts_with('-'))
    })?;
    let sleep = all
        .iter()
        .find(|process| process.parent == shell.pid && runs(process, "sleep", HOLD_SLEEP))?;
    Some((shell.pid, sleep.pid))
}

/// Waits for the serial greeter's held login, whose `sleep` ignores TERM,
/// and returns its session's containment.
fn held_login() -> Vec<Member> {
    let mut held = None;
    system_wait("the held serial login", || {
        held = held_session();
        held.is_some()
    });
    let (_, sleep) = held.unwrap();
    assert!(ignores_term(sleep), "the held login's sleep takes TERM");
    let members = serial_members();
    assert!(
        members.len() >= 2,
        "the held login's processes: {members:?}"
    );
    members
}

/// A step's rows as root describes it, consent's own lines, named to the
/// host with each space as `_`.
fn rows(operation: Operation) -> Vec<String> {
    Request::new([1; 32], UID, operation)
        .unwrap()
        .lines()
        .into_iter()
        .map(|row| row.replace(' ', "_"))
        .collect()
}

fn words(rows: &[String]) -> Vec<&str> {
    rows.iter().map(String::as_str).collect()
}

/// Types decimal digits on the number row: removal positions.
fn type_digits(keyboard: &mut Keyboard, text: &str) {
    for usage in digits(text.as_bytes()) {
        keyboard.key(usage);
    }
}

/// From the desktop: the attention menu, its `K` screen and `choice` on it.
fn manage(host: &mut Host, keyboard: &mut Keyboard, choice: u8) {
    chord(keyboard);
    host.screen("menu", &[]);
    keyboard.key(KEY_K);
    host.screen("keys", &[]);
    keyboard.key(choice);
}

/// A disclosure step's prompt, checked by the host, then Enter once the
/// compositor's settle period after presentation has passed, which is
/// that step's receipt.
fn approve(host: &mut Host, keyboard: &mut Keyboard, operation: Operation) {
    host.screen("disclosure", &words(&rows(operation)));
    std::thread::sleep(Duration::from_secs(1));
    keyboard.key(ENTER);
}

/// A PIN step of `operation`: its prompt with the field empty, the PIN
/// typed on the keyboard and Enter, the touch request while `key` waits
/// for its touch, and the touch.
fn pin_step(host: &mut Host, keyboard: &mut Keyboard, key: &Virtual, operation: Operation) {
    let rows = rows(operation);
    let mut field = vec!["0"];
    field.extend(words(&rows));
    host.screen("step-pin", &field);
    type_pin(keyboard, PIN);
    keyboard.key(ENTER);
    system_wait("the key waiting for its touch", || key.touching());
    host.screen("step-touch", &words(&rows));
    let touched = key.touched();
    key.release_touch();
    system_wait("the touch", || !key.touching() && key.touched() > touched);
}

/// A step that asks no PIN but a touch, `operation`'s prompt: the probe's
/// no-PIN assertion. Its prompt while `key` waits for the touch, and the
/// touch.
fn touch_step(host: &mut Host, key: &Virtual, operation: Operation) {
    system_wait("the key waiting for its touch", || key.touching());
    host.screen("step", &words(&rows(operation)));
    let touched = key.touched();
    key.release_touch();
    system_wait("the touch", || !key.touching() && key.touched() > touched);
}

/// A new key's create, prove and repeat after its connect step, each a PIN
/// step and its touch, its PIN retries at eight, then the probe, whose
/// no-PIN assertion asks for a touch. The new credential's fingerprint.
fn ceremony(
    host: &mut Host,
    keyboard: &mut Keyboard,
    key: &Virtual,
    step: impl Fn(LoginStep) -> Operation,
) -> Fingerprint {
    let made = key.state().credentials.len();
    pin_step(host, keyboard, key, step(LoginStep::Create { retries: 8 }));
    system_wait("the created credential", || {
        key.state().credentials.len() == made + 1
    });
    let new = enrolled_print(key);
    for step in [
        step(LoginStep::Prove {
            key: new,
            retries: 8,
        }),
        step(LoginStep::Repeat {
            key: new,
            retries: 8,
        }),
    ] {
        pin_step(host, keyboard, key, step);
    }
    touch_step(host, key, step(LoginStep::Probe { key: new }));
    new
}

/// Every key's touch is held until the guest has seen its request.
fn held(keys: &[&Virtual]) {
    for key in keys {
        key.script(Script {
            presence: Presence::Held,
            ..Script::default()
        });
    }
}

/// A first enrollment of `keys` from the desktop: `1` or `2`, the
/// disclosure on the first key's connect step approved by Enter, then
/// each key connected alone, the next once the previous key is removed,
/// and its ceremony. Returns after the last key's last touch, that key
/// still connected: the commit, publication and cutover follow on their
/// own.
fn enroll(host: &mut Host, keyboard: &mut Keyboard, keys: &[&Virtual]) -> Plugged {
    let after = u8::try_from(keys.len()).unwrap();
    let enrolling = |key, step| Operation::LoginEnroll {
        account: UID,
        before: 0,
        after,
        key,
        step,
        proof: None,
    };
    held(keys);
    let mut plugged = Plugged::insert(keys[0], NAME);
    manage(host, keyboard, if after == 1 { KEY_1 } else { KEY_2 });
    approve(host, keyboard, enrolling(1, LoginStep::Connect));
    for (index, key) in keys.iter().enumerate() {
        let ordinal = u8::try_from(index + 1).unwrap();
        if ordinal > 1 {
            host.screen(
                "step",
                &words(&rows(enrolling(ordinal, LoginStep::Connect))),
            );
            plugged.remove();
            plugged = Plugged::insert(key, NAME);
        }
        ceremony(host, keyboard, key, |step| enrolling(ordinal, step));
    }
    plugged
}

/// An addition to `before` keys authorized by `by`: `A`, the authorize
/// step's PIN and touch, the connect step, `by` swapped for `new`, its
/// ceremony and `LOGIN KEY ADDED`; Escape returns to the desktop.
fn add(host: &mut Host, keyboard: &mut Keyboard, by: &Virtual, new: &Virtual, before: u8) {
    let adding = |step| Operation::LoginAdd {
        account: UID,
        before,
        after: before + 1,
        step,
        proof: None,
    };
    held(&[by, new]);
    let plugged = Plugged::insert(by, NAME);
    manage(host, keyboard, KEY_A);
    let authorize = LoginStep::Authorize {
        key: enrolled_print(by),
        retries: 8,
    };
    pin_step(host, keyboard, by, adding(authorize));
    host.screen("step", &words(&rows(adding(LoginStep::Connect))));
    plugged.remove();
    let plugged = Plugged::insert(new, NAME);
    ceremony(host, keyboard, new, adding);
    host.screen("added", &[]);
    keyboard.key(ESCAPE);
    host.screen("desktop", &[]);
    plugged.remove();
}

/// The record's slots that hold `keys`' credentials, by position.
fn slots_of(keys: &[&Virtual]) -> Vec<Slot> {
    let ids = stored().expect("an enrolled record");
    let mut slots: Vec<Slot> = keys
        .iter()
        .map(|key| {
            let id = credential(key);
            let at = ids.iter().position(|stored| *stored == id).unwrap();
            Slot {
                position: u8::try_from(at + 1).unwrap(),
                key: fingerprint(&id),
            }
        })
        .collect();
    slots.sort_by_key(|slot| slot.position);
    slots
}

/// A removal of `removed` from `before` keys, authorized by `by`: `D`,
/// the positions typed and Enter, the disclosure approved by Enter,
/// and the authorize step's PIN and touch. Returns with `by` connected:
/// the commit follows on its own.
fn remove(
    host: &mut Host,
    keyboard: &mut Keyboard,
    by: &Virtual,
    removed: &[Slot],
    before: u8,
) -> Plugged {
    let after = before - u8::try_from(removed.len()).unwrap();
    held(&[by]);
    let plugged = Plugged::insert(by, NAME);
    manage(host, keyboard, KEY_D);
    let count = before.to_string();
    host.screen("removing", &[&count]);
    let positions: Vec<String> = removed
        .iter()
        .map(|slot| slot.position.to_string())
        .collect();
    type_digits(keyboard, &positions.concat());
    let mut chosen = vec![count.as_str()];
    chosen.extend(words(&positions));
    host.screen("removing", &chosen);
    keyboard.key(ENTER);
    let removing = |step| Operation::LoginRemove {
        account: UID,
        before,
        after,
        removed: removed.to_vec(),
        step,
        proof: None,
    };
    approve(host, keyboard, removing(LoginStep::Identify));
    let authorize = LoginStep::Authorize {
        key: enrolled_print(by),
        retries: 8,
    };
    pin_step(host, keyboard, by, removing(authorize));
    plugged
}

/// After an enrollment's last touch with a serial session and an SSH
/// session open: the record published, then the cutover (TOKEN-LOGIN.md,
/// "Cutover"): every process of both containments gone, the SSH client
/// ended, the boot's record naming the enforced form, no reboot guard,
/// ttyS0 root's and a parked greeter refusing, with no shell.
fn revoked(serial: &[Member], ssh: &[Member], client: &mut Child) {
    system_wait("the published record", || {
        matches!(state(), State::Enrolled(_))
    });
    system_wait("the serial session's containment emptied", || gone(serial));
    system_wait("the SSH session's containment emptied", || {
        gone(ssh) && client.try_wait().unwrap().is_some()
    });
    assert!(remote_session().is_none(), "the SSH session's command");
    system_wait("the enforced record", || {
        fs::read_to_string(CUTOVER).ok() == Some(cutover_record("enforced"))
    });
    assert!(!Path::new(REBOOT_GUARD).exists(), "a failed revocation");
    let line = fs::metadata("/dev/ttyS0").unwrap();
    assert_eq!((line.uid(), line.gid()), (0, 0), "ttyS0 handed back");
    parked_greeter();
    assert_eq!(login_shell(), None, "a serial shell after the cutover");
    assert_eq!(held_session(), None, "the held login after the cutover");
}

/// After the last key's removal, `by` still connected: the record
/// unlinked, then the cutover back: the serial greeter logs in at once,
/// held; the boot's record names the unenrolled form, no reboot guard;
/// then `LOGIN KEYS REMOVED` on the session, which stayed unlocked, and
/// Escape; and the ordinary SSH form, which still refuses root.
fn unenrolled_again(host: &mut Host, keyboard: &mut Keyboard, by: Plugged) {
    system_wait("the removed record", || {
        matches!(state(), State::Unenrolled)
    });
    held_login();
    system_wait("the unenrolled record", || {
        fs::read_to_string(CUTOVER).ok() == Some(cutover_record("unenrolled"))
    });
    assert!(!Path::new(REBOOT_GUARD).exists(), "a failed revocation");
    // The parked greeter's exit hung up the host's descriptor too.
    *host = Host::open();
    host.screen("removed", &[]);
    keyboard.key(ESCAPE);
    host.screen("desktop", &[]);
    by.remove();
    sshd(false);
}

/// With `previous` naming a deployment whose initramfs carries no tier
/// marker, behind a bind of a fixture volume whose `current` is the booted
/// deployment: a first enrollment's `2` is refused with `A RETAINED
/// SYSTEM CANNOT READ KEYS` before any presentation or token request, and
/// the machine stays unenrolled.
fn unmarked_previous(host: &mut Host, keyboard: &mut Keyboard, key: &Virtual) {
    use crate::login_tier::tests::{bundle, initramfs};
    let volume = Path::new(login_tier::VOLUME);
    let current = fs::read_link(volume.join("boot/current")).unwrap();
    let current = current
        .to_str()
        .unwrap()
        .strip_prefix("../deployments/")
        .unwrap()
        .to_string();
    let fixture = Path::new(WORK).join("volume");
    let deployments = fixture.join("deployments");
    fs::create_dir_all(fixture.join("boot")).unwrap();
    fs::create_dir_all(&deployments).unwrap();
    let unmarked = Path::new(WORK).join("unmarked-deployment");
    fs::create_dir(&unmarked).unwrap();
    let id = bundle(&unmarked, &initramfs(None));
    fs::rename(&unmarked, deployments.join(&id)).unwrap();
    let kept = deployments.join(&current);
    fs::create_dir(&kept).unwrap();
    symlink(
        format!("../deployments/{current}"),
        fixture.join("boot/current"),
    )
    .unwrap();
    symlink(
        format!("../deployments/{id}"),
        fixture.join("boot/previous"),
    )
    .unwrap();
    let bind = |from: &Path, to: &Path| {
        assert!(Command::new("/bin/mount")
            .args(["-o", "bind"])
            .arg(from)
            .arg(to)
            .status()
            .unwrap()
            .success());
    };
    // The booted deployment is bound aside first, since the fixture then
    // hides it, and into the fixture's `current` last: a bind carries no
    // mount beneath its source.
    let real = Path::new(WORK).join("current-deployment");
    fs::create_dir(&real).unwrap();
    bind(&volume.join("deployments").join(&current), &real);
    bind(&fixture, volume);
    let shown = volume.join("deployments").join(&current);
    bind(&real, &shown);
    let give_up = Duration::from_secs(10);
    assert_eq!(
        login_tier::retained(volume, login_tier::CURRENT, 0, give_up),
        Ok(READS.to_vec())
    );
    assert!(login_tier::retained(volume, login_tier::PREVIOUS, 0, give_up).is_err());
    let plugged = Plugged::insert(key, NAME);
    manage(host, keyboard, KEY_2);
    host.screen("version-refused", &[]);
    keyboard.key(ESCAPE);
    host.screen("desktop", &[]);
    assert_eq!(plugged.remove().requests, 0, "the refusal reached the key");
    assert!(matches!(state(), State::Unenrolled));
    for target in [shown.as_path(), volume, real.as_path()] {
        assert!(Command::new("/bin/umount")
            .arg(target)
            .status()
            .unwrap()
            .success());
    }
}

/// While mounted over `/bin/td-firstboot`, a `render-ssh-policy` run,
/// which is how td-authd's cutover renders, marks that it began and waits
/// for the release before running the real binary from a copy, so the
/// cutover holds between its record's removal and its restarts. td-authd
/// kills a render at ten seconds. Every other verb runs at once.
struct HeldRender(PathBuf);

impl HeldRender {
    fn mount() -> Self {
        let real = fs::canonicalize("/bin/td-firstboot").unwrap();
        let copy = Path::new(WORK).join("td-firstboot-real");
        fs::copy(&real, &copy).unwrap();
        fs::set_permissions(&copy, Permissions::from_mode(0o755)).unwrap();
        let wrapper = Path::new(WORK).join("td-firstboot-held");
        fs::write(
            &wrapper,
            format!(
                "#!/bin/sh\nif [ \"$1\" = render-ssh-policy ]; then\n    : > {began} || exit 1\n    while [ ! -e {release} ]; do /bin/sleep 0.05; done\nfi\nexec {copy} \"$@\"\n",
                began = Self::began_path().display(),
                release = Self::release_path().display(),
                copy = copy.display(),
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
        Self(real)
    }

    fn began_path() -> PathBuf {
        Path::new(WORK).join("render-began")
    }

    fn release_path() -> PathBuf {
        Path::new(WORK).join("render-release")
    }

    fn began(&self) -> bool {
        Self::began_path().exists()
    }

    fn release(&self) {
        fs::write(Self::release_path(), b"").unwrap();
    }

    fn unmount(self) {
        assert!(Command::new("/bin/umount")
            .arg(&self.0)
            .status()
            .unwrap()
            .success());
    }
}

/// An unenrolled boot's start: the serial greeter logged in, then stopped,
/// so no shell reads the host's answers on ttyS0; the keyboard; the blank,
/// then the unlocked generation's first frame, holding no lock pixel, and
/// the desktop; boot health and the ordinary SSH form, which refuses root.
fn unenrolled_start() -> (Host, Keyboard) {
    idle_login_shell();
    service(&["stop", "greeter"]);
    system_wait("ttyS0 released", || serial().is_none());
    let keyboard = Keyboard::new();
    let mut host = Host::open();
    release_console();
    blank_screen();
    host.screen("blank-unlocked", &[]);
    fs::write(INPUT_READY, b"ready").unwrap();
    ready(&["wayland", "terminal"]);
    host.screen("desktop", &[]);
    boot_healthy();
    sshd(false);
    (host, keyboard)
}

/// `seed`: unenrolled, with the state helper failing: root reads no
/// record, so td-authd runs no helper and the session starts unlocked, and
/// the boot's record names that state, so root's check changes nothing.
/// An enrollment is refused while `previous` lacks the tier marker; then,
/// with the serial greeter logged in again and held by a process that
/// ignores TERM and an SSH session open as the primary account, both keys
/// are enrolled through the attention screen and the cutover ends both
/// sessions. Root's SSH key is refused before and after.
fn seed() -> Vec<Virtual> {
    assert!(matches!(state(), State::Unenrolled));
    assert_eq!(
        fs::read_to_string(CUTOVER).unwrap(),
        cutover_record("unenrolled")
    );
    let failing = Recorder::mount(true);
    let (mut host, mut keyboard) = unenrolled_start();
    // Only the mount's own check ran the helper.
    let helpers = failing
        .verbs()
        .iter()
        .filter(|verb| *verb == "inspect-login")
        .count();
    assert_eq!(helpers, 1, "td-authd ran the state helper unenrolled");
    failing.unmount();
    retained_reads();
    let keys: Vec<Virtual> = SYSTEM_KEYS.iter().map(|name| new_key(name)).collect();
    unmarked_previous(&mut host, &mut keyboard, &keys[0]);
    hold_serial();
    service(&["start", "greeter"]);
    let serial = held_login();
    let mut client = open_ssh();
    let ssh = leaf_members(leader("sshd"));
    // The greeter's start may have hung up the host's descriptor.
    host = Host::open();
    let render = HeldRender::mount();
    let plugged = enroll(&mut host, &mut keyboard, &[&keys[0], &keys[1]]);
    // Held at its render, the cutover has removed the boot's record and
    // ended nothing yet: both sessions outlived the enrollment itself.
    system_wait("the cutover's render", || render.began());
    assert!(!Path::new(CUTOVER).exists(), "the cutover kept its record");
    assert!(
        alive(&serial) && alive(&ssh),
        "a session ended before the cutover"
    );
    render.release();
    revoked(&serial, &ssh, &mut client);
    render.unmount();
    // The serial session's leader exiting hung up every descriptor open on
    // its controlling terminal, the host's among them.
    host = Host::open();
    host.screen("enrolled", &[]);
    keyboard.key(ESCAPE);
    host.screen("desktop", &[]);
    plugged.remove();
    let ids = enrolled();
    for key in &keys {
        assert!(ids.contains(&credential(key)));
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
    sshd(true);
    keys
}

/// `unlocked`: the boot after every key's removal starts unlocked with the
/// ordinary SSH form. Then a one-key enrollment across a killed authority:
/// with a serial login held and an SSH session open, the cutover's render
/// after the publication is held, td-authd is killed while it waits,
/// before either restart, and the next generation's first check completes
/// the revocation; that generation starts locked.
fn unlocked() -> Vec<Virtual> {
    assert!(matches!(state(), State::Unenrolled));
    assert_eq!(
        fs::read_to_string(CUTOVER).unwrap(),
        cutover_record("unenrolled")
    );
    let (_, mut keyboard) = unenrolled_start();
    let key = new_key(SINGLE_KEY);
    hold_serial();
    service(&["start", "greeter"]);
    let serial = held_login();
    let mut client = open_ssh();
    let (sshd_leader, greeter_leader) = (leader("sshd"), leader("greeter"));
    let ssh = leaf_members(sshd_leader);
    // The greeter's start may have hung up the host's descriptor.
    let mut host = Host::open();
    let render = HeldRender::mount();
    let old = authority();
    let plugged = enroll(&mut host, &mut keyboard, &[&key]);
    system_wait("the cutover's render", || render.began());
    // Published, its record removed, and neither unit restarted.
    assert!(matches!(state(), State::Enrolled(_)));
    assert!(!Path::new(CUTOVER).exists(), "the cutover kept its record");
    assert_eq!(
        (leader("sshd"), leader("greeter")),
        (sshd_leader, greeter_leader)
    );
    assert!(
        alive(&serial) && alive(&ssh),
        "a session ended before the kill"
    );
    assert!(Command::new("/bin/kill")
        .args(["-KILL", &old.to_string()])
        .status()
        .unwrap()
        .success());
    render.release();
    system_wait(
        "the next authority",
        || matches!(authorities()[..], [new] if new != old),
    );
    revoked(&serial, &ssh, &mut client);
    render.unmount();
    host = Host::open();
    host.screen("locked", &[]);
    plugged.remove();
    assert_eq!(stored(), Some(vec![credential(&key)]));
    sshd(true);
    vec![key]
}

/// An enrolled boot's first frames: locked from the first frame and still
/// locked once the terminal is ready; boot health and the enforced SSH
/// form, which refuses root and admits the primary.
fn locked_start(host: &mut Host) {
    host.screen("locked", &[]);
    ready(&["wayland", "terminal"]);
    boot_healthy();
    sshd(true);
}

/// `added`: from the two keys, an addition of each of six more,
/// authorized by the first, to eight; a ninth is refused before any
/// token.
fn added(host: &mut Host, keyboard: &mut Keyboard) -> Vec<Virtual> {
    locked_start(host);
    let mut keys = restored();
    assert_eq!(stored().unwrap().len(), 2);
    unlock(host, keyboard, &keys[0]);
    for name in ADDED_KEYS {
        let new = new_key(name);
        let before = u8::try_from(keys.len()).unwrap();
        add(host, keyboard, &keys[0], &new, before);
        keys.push(new);
        assert_eq!(stored().unwrap().len(), keys.len());
    }
    let ids = stored().unwrap();
    for key in &keys {
        assert!(ids.contains(&credential(key)));
    }
    // A key is plugged, so the refusal is root's at the baseline, not a
    // missing key: it reaches no token.
    let plugged = Plugged::insert(&keys[0], NAME);
    manage(host, keyboard, KEY_A);
    host.screen("eight", &[]);
    keyboard.key(ESCAPE);
    host.screen("desktop", &[]);
    assert_eq!(plugged.remove().requests, 0, "the refusal reached the key");
    assert_eq!(stored(), Some(ids));
    keys
}

/// `removed`: from eight keys, seven removed, leaving the first: the
/// one-key disclosure, authorized by the second, which is among them.
/// Then the last, behind the remove-every-key disclosure, and the cutover
/// back, with the serial greeter logging in at once.
fn removed(host: &mut Host, keyboard: &mut Keyboard) -> Vec<Virtual> {
    locked_start(host);
    let keys: Vec<Virtual> = SYSTEM_KEYS
        .iter()
        .chain(ADDED_KEYS)
        .map(|name| restore(name))
        .collect();
    assert_eq!(stored().unwrap().len(), 8);
    unlock(host, keyboard, &keys[0]);
    let others: Vec<&Virtual> = keys[1..].iter().collect();
    let plugged = remove(host, keyboard, &keys[1], &slots_of(&others), 8);
    host.screen("removed", &[]);
    keyboard.key(ESCAPE);
    host.screen("desktop", &[]);
    plugged.remove();
    assert_eq!(stored(), Some(vec![credential(&keys[0])]));
    hold_serial();
    let plugged = remove(host, keyboard, &keys[0], &slots_of(&[&keys[0]]), 1);
    unenrolled_again(host, keyboard, plugged);
    keys
}

/// `cut`: the one key removed, behind its disclosure, back to unenrolled;
/// then a one-key enrollment whose cutover's render is held, and the
/// power cut as soon as its record is published. It never returns.
fn cut(host: &mut Host, keyboard: &mut Keyboard) -> ! {
    locked_start(host);
    let single = restore(SINGLE_KEY);
    assert_eq!(stored(), Some(vec![credential(&single)]));
    unlock(host, keyboard, &single);
    hold_serial();
    let plugged = remove(host, keyboard, &single, &slots_of(&[&single]), 1);
    unenrolled_again(host, keyboard, plugged);
    let last = new_key(CUT_KEY);
    let (sshd_leader, greeter_leader) = (leader("sshd"), leader("greeter"));
    let render = HeldRender::mount();
    let plugged = enroll(host, keyboard, &[&last]);
    // The cutover starts only after the worker's successful end, so once
    // its render begins the record's rename and directory sync are done;
    // it has removed the boot's record and restarted neither unit.
    system_wait("the cutover's render", || render.began());
    assert!(matches!(state(), State::Enrolled(_)));
    assert!(!Path::new(CUTOVER).exists(), "the cutover kept its record");
    assert_eq!(
        (leader("sshd"), leader("greeter")),
        (sshd_leader, greeter_leader)
    );
    let mut console = OpenOptions::new().write(true).open("/dev/console").unwrap();
    console
        .write_all(format!("{CUT} cut\n").as_bytes())
        .unwrap();
    // The host kills QEMU on that word; nothing more runs.
    let _held = (render, plugged);
    loop {
        std::thread::sleep(Duration::from_secs(1));
    }
}

/// `after-cut`: the power cut came right after the publication, before
/// the cutover's restarts. This boot's firstboot rendered the enforced
/// form and recorded it in stage 1, before `sshd` and the greeter
/// started; the authority's check found that record and restarted
/// neither, so both still run under firstboot's form. The record is the
/// whole new one, and its key unlocks.
fn after_cut(host: &mut Host, keyboard: &mut Keyboard, record: fs::Metadata) -> Vec<Virtual> {
    let last = restore(CUT_KEY);
    assert_eq!(stored(), Some(vec![credential(&last)]));
    assert_eq!(
        fs::read_to_string(CUTOVER).unwrap(),
        cutover_record("enforced")
    );
    assert!(!Path::new(REBOOT_GUARD).exists(), "a failed revocation");
    // A cutover would restart both units after the authority started,
    // and publish another record than the one `record` saw before it.
    locked_start(host);
    let authority = started(authority());
    for unit in ["sshd", "greeter"] {
        assert!(
            started(leader(unit)) <= authority,
            "{unit} after the authority"
        );
    }
    let unchanged = fs::metadata(CUTOVER).unwrap();
    assert_eq!(
        (unchanged.ino(), unchanged.mtime(), unchanged.mtime_nsec()),
        (record.ino(), record.mtime(), record.mtime_nsec()),
        "the authority rewrote the boot's record"
    );
    unlock(host, keyboard, &last);
    vec![last]
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
    quiet_kernel_console();
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
    let keys = match phase {
        "seed" => seed(),
        "unlocked" => unlocked(),
        _ => enrolled_boot(phase),
    };
    if let Some(next) = next_damage(phase) {
        damage(next);
    }
    for key in &keys {
        assert_eq!(key.saved(), Ok(()));
        assert_eq!(key.state().retries, 8);
    }
    assert!(Device::discover().unwrap().is_empty());
}

/// A boot that starts enrolled or unavailable: its greeter parked
/// refusing before the host is asked anything, since its getty flushes
/// the line's input first, and its first frame the lock surface's. A
/// greeter that a cutover back restarts is no longer parked.
fn enrolled_boot(phase: &str) -> Vec<Virtual> {
    // Before the input is ready, so before the seat and the authority.
    assert!(authorities().is_empty(), "an authority before the seat");
    let record = fs::metadata(CUTOVER);
    let mut keyboard = Keyboard::new();
    let parked = parked_greeter();
    let mut host = Host::open();
    start(&mut host);
    let (keys, parks) = match phase {
        "locked" => (locked(&mut host, &mut keyboard), true),
        "added" => (added(&mut host, &mut keyboard), true),
        "removed" => (removed(&mut host, &mut keyboard), false),
        "cut" => cut(&mut host, &mut keyboard),
        "after-cut" => (after_cut(&mut host, &mut keyboard, record.unwrap()), true),
        _ => (damaged_or_repaired(phase, &mut host, &mut keyboard), true),
    };
    if parks {
        assert_eq!(greeter(), Some(parked), "the serial greeter left its park");
    }
    keys
}

/// The two keys, each in the record unless the phase damaged it.
fn system_keys(phase: &str) -> Vec<Virtual> {
    let ids = enrolled_or_damaged(phase);
    let keys = restored();
    if !ids.is_empty() {
        for key in &keys {
            assert!(ids.contains(&credential(key)));
        }
    }
    keys
}

/// `locked`: the first enrolled boot's unlocks with each key, relocks,
/// updates, killed compositor, unreadable state and, last, suspend.
fn locked(host: &mut Host, keyboard: &mut Keyboard) -> Vec<Virtual> {
    let keys = system_keys("locked");
    host.screen("locked", &[]);
    ready(&["wayland", "terminal"]);
    host.screen("locked", &[]);
    boot_healthy();
    sshd(true);
    unlock(host, keyboard, &keys[0]);
    // The unlock's close cut off the keyboard's first fresh report, which
    // Caps Lock spends as the chord does.
    keyboard.key(CAPS_LOCK);
    keyboard.report(SUPER, 0);
    keyboard.report(SUPER, KEY_L);
    keyboard.report(0, 0);
    host.screen("locked", &[]);
    unlock(host, keyboard, &keys[1]);
    chord(keyboard);
    host.screen("menu", &[]);
    keyboard.key(KEY_L);
    host.screen("locked", &[]);
    unlock(host, keyboard, &keys[0]);
    updates(host, keyboard);
    killed(host);
    unreadable(host, keyboard, &keys[0]);
    unlock(host, keyboard, &keys[1]);
    suspend(host, keyboard, &keys[1]);
    keys
}

/// A `damaged-` boot, which shows its cause and repairs it, or a
/// `repaired-` one, which is enrolled again.
fn damaged_or_repaired(phase: &str, host: &mut Host, keyboard: &mut Keyboard) -> Vec<Virtual> {
    let keys = system_keys(phase);
    match phase.split_once('-') {
        Some(("damaged", damage)) => {
            damaged_boot(host, keyboard, &keys, damage.starts_with("record-"));
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
    keys
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

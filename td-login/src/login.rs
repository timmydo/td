//! `login` — start a session for a named user.
//!
//! On a td image this is what getty execs: `/etc/autologin` runs
//! `td-login login-primary`, which resolves the primary account and enters
//! `login -f <user>`, which is how the machine reaches its greeter at all. The
//! `-f` (already-authenticated) path is therefore the one the boot proves on
//! every start; the interactive path is the one the policy table of
//! THREAT-MODEL.md §3 governs. Both console logins, the interactive one and
//! `login-primary`, first pass `guard_console`.

use crate::creds::Credentials;
use crate::db::{self, Account, Denied};
use crate::login_state::{self, Owner, State};
use crate::session::{self, Env, Session};
use crate::status::Status;
use crate::{emit, emit_err, primary_account};
use std::path::Path;

/// THREAT-MODEL.md §3's one line, written whole to standard error when the
/// console refuses.
pub(crate) const CONSOLE_REFUSED: &str =
    "td-login: login keys enrolled or unavailable; console login refused\n";

/// The root the console gate reads the login state under: the real one.
const REAL_ROOT: &str = "/";

/// How many user names an interactive login will accept before giving up, the
/// same bound `login(1)` uses. Unbounded retries against a getty that respawns
/// is a busy loop with no operator on the other end.
const MAX_ATTEMPTS: usize = 3;

/// Longest user name accepted from a terminal. Anything longer is not a name in
/// any database td generates, and bounding it keeps a paste of arbitrary length
/// out of the lookup path.
const MAX_NAME: usize = 32;

/// What `login`'s argv asked for.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Options {
    /// `-p`: keep the caller's environment instead of starting fresh.
    pub preserve: bool,
    /// `-h HOST`: accepted and IGNORED. td writes no utmp/wtmp/lastlog — there
    /// is nothing on the image that reads them — so the remote host has nowhere
    /// to be recorded. Rejecting the flag instead would break a caller that
    /// passes it out of habit for no security benefit.
    pub host: Option<String>,
    /// `-f USER`: the caller asserts USER is already authenticated.
    pub forced: Option<String>,
    /// A bare user name.
    pub user: Option<String>,
}

impl Options {
    /// The user this invocation is about, and whether authentication is being
    /// bypassed. `None` means "ask".
    fn target(&self) -> (Option<&str>, bool) {
        match (&self.forced, &self.user) {
            (Some(name), _) => (Some(name.as_str()), true),
            (None, Some(name)) => (Some(name.as_str()), false),
            (None, None) => (None, false),
        }
    }
}

pub fn parse(args: &[String]) -> Result<Options, String> {
    let mut opts = Options::default();
    let mut rest_are_operands = false;
    let mut i = 0usize;
    while let Some(arg) = args.get(i) {
        i += 1;
        if rest_are_operands || !arg.starts_with('-') || arg == "-" {
            if opts.user.is_some() {
                return Err(format!("unexpected extra operand {arg:?}"));
            }
            opts.user = Some(arg.clone());
            continue;
        }
        match arg.as_str() {
            "--" => rest_are_operands = true,
            "-p" => opts.preserve = true,
            "-h" | "-f" => {
                let Some(value) = args.get(i) else {
                    return Err(format!("{arg} needs an argument"));
                };
                i += 1;
                if arg == "-h" {
                    opts.host = Some(value.clone());
                } else {
                    if opts.forced.is_some() {
                        return Err("-f given more than once".into());
                    }
                    opts.forced = Some(value.clone());
                }
            }
            other => return Err(format!("unrecognised argument {other:?}")),
        }
    }
    if opts.forced.is_some() && opts.user.is_some() {
        return Err("-f USER and a bare user name name two different sessions".into());
    }
    Ok(opts)
}

pub fn run(args: &[String]) -> Result<u8, String> {
    let opts =
        parse(args).map_err(|e| format!("{e}\nusage: login [-p] [-h HOST] [-f USER] [USER]"))?;
    let status = Status::read()?;
    let (target, forced) = opts.target();
    // `-f` bypasses the account's own secret, so it is root's to use. Without
    // this the refusal would still come — from `creds::apply`, after the
    // database said yes — and the diagnostic would name the wrong step.
    if forced && !status.is_root() {
        return Err("only root may use -f (it starts a session without authenticating)".into());
    }
    if !forced {
        guard_console(&status);
    }
    let mode = if opts.preserve {
        Env::Preserve
    } else {
        Env::Fresh
    };
    match target {
        Some(name) => start(name, forced, mode, &status),
        None => ask(mode, &status),
    }
}

/// The console gate (THREAT-MODEL.md §3): returns only on a verifiably
/// unenrolled machine, and otherwise refuses for good.
///
/// It concerns a caller root in some uid column. One root in none can switch
/// to nobody but itself (`creds::may_switch`, §4), so it reaches no session it
/// lacked, and it cannot read the root-only directory either: gating it would
/// refuse every unprivileged `login` on an unenrolled machine for nothing.
pub(crate) fn guard_console(status: &Status) {
    guard_console_at(status, Path::new(REAL_ROOT), Owner::ROOT);
}

fn guard_console_at(status: &Status, root: &Path, owner: Owner) {
    if status.uid.contains(&0) && !admits(console_state(root, owner)) {
        refuse_console(status)
    }
}

/// The shared predicate for the primary account's record under `root`.
fn console_state(root: &Path, owner: Owner) -> State {
    login_state::state_as(root, owner, primary_account::UID)
}

/// Only unenrolled opens the console; enrolled and every unavailable cause
/// keep it shut.
fn admits(state: State) -> bool {
    state == State::Unenrolled
}

/// Hands the terminal back to root, says the one line and parks. It never
/// exits: the greeter's wrapper reboots when its session chain succeeds and
/// td-svc restarts the unit whenever it ends, so an exit would be a reboot or
/// a respawn loop.
fn refuse_console(status: &Status) -> ! {
    if status.is_root() {
        if let Err(why) = crate::tty::reclaim() {
            emit_err(&format!(
                "login: not returning the terminal to root: {why}\n"
            ));
        }
    }
    emit_err(CONSOLE_REFUSED);
    park()
}

/// Blocks until a signal ends the process. A wakeup without an unpark, which
/// nothing here issues, parks again rather than spinning.
fn park() -> ! {
    loop {
        std::thread::park();
    }
}

/// Prompt for a user name until one is authorized or the attempts run out.
///
/// The loop retries the AUTHORIZATION only, never the commit. Once `commit` runs
/// it has chowned a terminal and dropped privilege, so a failure past that point
/// is not something to try again as a different user — and looping would re-enter
/// the prompt as the account we just became, with `creds::apply` refusing every
/// further attempt for a reason that has nothing to do with what was typed.
///
/// Denials are reported with ONE generic message, whatever the reason. The
/// specific reasons — no such user, locked, has a password this build cannot
/// verify — each answer a question an unauthenticated caller at a console
/// should not get to ask. `-f` (root) keeps the precise diagnostics, because
/// there the caller is already privileged and the message is for an operator
/// debugging a boot.
fn ask(mode: Env, status: &Status) -> Result<u8, String> {
    for _ in 0..MAX_ATTEMPTS {
        emit("\nlogin: ")?;
        let mut line = String::new();
        match std::io::stdin().read_line(&mut line) {
            Ok(0) => return Err("end of input".into()),
            Ok(_) => {}
            Err(e) => return Err(format!("cannot read the user name: {e}")),
        }
        let name = line.trim();
        if name.is_empty() {
            continue;
        }
        match authorize(name, false) {
            Ok(account) => return commit(&account, mode, status),
            Err(_) => emit_err("Login incorrect\n"),
        }
    }
    Err("too many login attempts".into())
}

/// Names this will even look up. The database lookup is exact-match and the
/// parsers are strict, so this is a bound rather than a defence — but a name
/// with a colon or a newline in it cannot exist in any file td generates, and
/// refusing it before the lookup keeps the terminal's input out of the parser.
fn plausible_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

fn start(name: &str, forced: bool, mode: Env, status: &Status) -> Result<u8, String> {
    let account = authorize(name, forced)?;
    commit(&account, mode, status)
}

/// The DECISION half: resolve the account and apply the policy. Nothing here
/// changes any state, which is what makes it safe for the prompt loop to retry.
///
/// `pub(crate)` because it is the crate's ONE authentication decision and every
/// front end reaches it: `login` through `start` and the prompt loop, `exec-as`
/// and `exec-primary` with `forced`. A front end with its own copy of these
/// five steps would be one policy in two places, with the compiler checking
/// only the `match`; `the_session_policy_is_decided_in_one_place` stops one
/// appearing.
pub(crate) fn authorize(name: &str, forced: bool) -> Result<Account, String> {
    if !plausible_name(name) {
        return Err(format!("{name:?} is not a plausible user name"));
    }
    let account = db::account(name)?;
    let secret = db::secret(name)?;
    db::may_start_session(secret, forced).map_err(|denial| match denial {
        Denied::Locked => format!("account {name:?} is locked"),
        Denied::ServiceOnly => format!("account {name:?} is service-only"),
        Denied::NeedsPassword => format!(
            "account {name:?} has a password and this build verifies no hash scheme \
             (see td-login/THREAT-MODEL.md section 3)"
        ),
        Denied::NotService => format!("account {name:?} is not a service account"),
    })?;
    Ok(account)
}

/// Resolve the service-specific account class for `exec-service-as`.
///
/// Kept beside `authorize` so every front end still reaches one account and
/// secret lookup boundary. The separate policy call is load-bearing: ordinary
/// forced sessions continue to reject this account class.
pub(crate) fn authorize_service(name: &str) -> Result<Account, String> {
    if !plausible_name(name) {
        return Err(format!("{name:?} is not a plausible user name"));
    }
    let account = db::account(name)?;
    let secret = db::secret(name)?;
    db::may_start_service(secret).map_err(|denial| match denial {
        Denied::NotService => format!("account {name:?} is not a service account"),
        Denied::Locked => format!("account {name:?} is locked"),
        Denied::ServiceOnly => format!("account {name:?} is service-only"),
        Denied::NeedsPassword => format!("account {name:?} needs a password"),
    })?;
    Ok(account)
}

/// The COMMITTING half: everything that needs root happens here, in order, and
/// then `session::enter` drops privilege exactly once and execs. It runs at most
/// once per invocation — a failure after this point is reported as itself, never
/// retried as another login attempt.
fn commit(account: &Account, mode: Env, status: &Status) -> Result<u8, String> {
    let groups = db::supplementary(&account.name)?;
    let creds = Credentials::new(account.uid, account.gid, &groups);

    // The terminal hand-over needs root and must therefore precede the switch.
    // A refusal is a warning, not a failed login: see THREAT-MODEL.md section 6.
    if status.is_root() {
        if let Err(why) = crate::tty::hand_over(account.uid, account.gid) {
            emit_err(&format!("login: not claiming the terminal: {why}\n"));
        }
    }

    let cwd = workdir(account, |path| std::path::Path::new(path).is_dir());
    if cwd != account.home {
        emit_err(&format!(
            "login: home {} is not a directory; starting in {cwd}\n",
            account.home
        ));
    }
    let session = Session {
        creds,
        program: account.shell.clone(),
        arg0: session::login_arg0(&account.shell),
        args: Vec::new(),
        env: session::environment(account, mode, &session::inherited()),
        cwd,
    };
    session::enter(&session)
}

/// The session's working directory: the account's home when it is one, `/`
/// otherwise. `is_dir` is injected so the decision is testable without a
/// filesystem.
fn workdir(account: &Account, is_dir: impl Fn(&str) -> bool) -> String {
    if is_dir(&account.home) {
        account.home.clone()
    } else {
        session::ROOTDIR.to_string()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;

    fn argv(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|s| (*s).to_string()).collect()
    }

    /// The forms the image actually uses, plus the ones an operator types.
    #[test]
    fn the_argv_forms_parse() {
        // /etc/autologin's exact invocation.
        let o = parse(&argv(&["-f", "tester"])).unwrap();
        assert_eq!(o.target(), (Some("tester"), true));
        assert!(!o.preserve);

        let o = parse(&argv(&["tester"])).unwrap();
        assert_eq!(o.target(), (Some("tester"), false));

        let o = parse(&argv(&["-p", "-h", "10.0.2.2", "-f", "root"])).unwrap();
        assert_eq!(o.target(), (Some("root"), true));
        assert!(o.preserve);
        assert_eq!(o.host.as_deref(), Some("10.0.2.2"));

        assert_eq!(parse(&argv(&[])).unwrap().target(), (None, false));
        // `--` ends option parsing, so a user name that looks like a flag is
        // still a user name.
        assert_eq!(
            parse(&argv(&["--", "-p"])).unwrap().target(),
            (Some("-p"), false)
        );
    }

    /// Every refusal is a refusal, not a default. The one that matters is the
    /// last: `-f root tester` reading as "force root, ignore tester" would start
    /// a different session than the caller wrote.
    #[test]
    fn ambiguous_or_unknown_argv_is_refused() {
        assert!(parse(&argv(&["-f"])).is_err());
        assert!(parse(&argv(&["-h"])).is_err());
        assert!(parse(&argv(&["-x"])).is_err());
        assert!(parse(&argv(&["--nope"])).is_err());
        assert!(parse(&argv(&["a", "b"])).is_err());
        assert!(parse(&argv(&["-f", "a", "-f", "b"])).is_err());
        assert!(parse(&argv(&["-f", "root", "tester"])).is_err());
    }

    #[test]
    fn only_plausible_names_reach_the_database() {
        for good in ["root", "tester", "td.user", "a_b-c", "u1"] {
            assert!(plausible_name(good), "{good} should be looked up");
        }
        for bad in [
            "",
            "root:x:0:0",
            "te ster",
            "root\n",
            "../etc/passwd",
            "verylongnameverylongnameverylongnameverylong",
        ] {
            assert!(!plausible_name(bad), "{bad:?} must not be looked up");
        }
    }

    /// The decision half must reject an implausible name BEFORE touching the
    /// database, so the prompt loop's retry never reaches a parser with a
    /// terminal's raw input. It is also the half the loop is allowed to repeat:
    /// nothing it does is observable, which is what makes retrying it safe.
    #[test]
    fn authorize_rejects_an_implausible_name_before_reading_any_file() {
        for bad in ["root:x:0:0", "te ster", "../etc/passwd", ""] {
            let err = authorize(bad, true).unwrap_err();
            assert!(
                err.contains("not a plausible user name"),
                "{bad:?} must be refused before the lookup, got: {err}"
            );
            let err = authorize_service(bad).unwrap_err();
            assert!(
                err.contains("not a plausible user name"),
                "service path must refuse {bad:?} before the lookup, got: {err}"
            );
        }
    }

    #[test]
    fn a_missing_home_falls_back_to_the_root_directory() {
        let account = Account {
            name: "tester".into(),
            uid: 1000,
            gid: 1000,
            gecos: String::new(),
            home: "/home/tester".into(),
            shell: "/bin/sh".into(),
        };
        assert_eq!(workdir(&account, |_| true), "/home/tester");
        assert_eq!(workdir(&account, |_| false), "/");
    }

    use crate::login_state::Cause;

    use std::os::unix::fs::{symlink, DirBuilderExt, MetadataExt};
    use std::path::PathBuf;
    use std::process::{Child, Command, Stdio};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    /// The child harness's caller uid columns, root, and the owner it
    /// requires as `UID:GID`.
    const CHILD_CALLER: &str = "TD_LOGIN_TEST_CONSOLE_CALLER";
    const CHILD_ROOT: &str = "TD_LOGIN_TEST_CONSOLE_ROOT";
    const CHILD_OWNER: &str = "TD_LOGIN_TEST_CONSOLE_OWNER";
    const CHILD_TEST: &str = "login::tests::console_gate_child";
    /// Printed by the child when the gate returned.
    const PROCEEDED: &str = "td-login-test: the console gate returned";

    /// A temporary root whose `var/lib/td/login` is a valid 0700 directory,
    /// owned by the test's own IDs, which `owner` stands in for root's.
    struct Root {
        root: PathBuf,
        owner: Owner,
    }

    impl Root {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let root = std::env::temp_dir().join(format!(
                "td-login-console-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(root.join("var/lib/td")).unwrap();
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(login_state::directory(&root))
                .unwrap();
            let meta = std::fs::metadata(login_state::directory(&root)).unwrap();
            Self {
                root,
                owner: Owner {
                    uid: meta.uid(),
                    gid: meta.gid(),
                },
            }
        }

        fn login(&self) -> PathBuf {
            login_state::directory(&self.root)
        }

        fn state(&self) -> State {
            console_state(&self.root, self.owner)
        }

        fn enrol(&self) {
            std::fs::write(self.login().join("1000"), b"not parsed").unwrap();
        }
    }

    impl Drop for Root {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn status(uid: [u32; 4]) -> Status {
        Status {
            uid,
            gid: [0; 4],
            groups: vec![0],
            threads: 1,
            cap_prm: 0,
            cap_eff: 0,
            cap_amb: 0,
            cap_inh: 0,
        }
    }

    #[test]
    fn the_refusal_line_is_the_threat_models_exactly() {
        assert_eq!(
            CONSOLE_REFUSED,
            "td-login: login keys enrolled or unavailable; console login refused\n"
        );
        let model =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/THREAT-MODEL.md"))
                .unwrap();
        let line = CONSOLE_REFUSED.trim_end();
        assert_eq!(model.matches(&format!("`{line}`")).count(), 1);
        assert_eq!(REAL_ROOT, "/");
        assert_eq!(primary_account::UID, 1000);
    }

    /// Only unenrolled opens the console; every other state, each cause of
    /// unavailable included, keeps it shut.
    #[test]
    fn only_an_unenrolled_state_admits_the_console() {
        assert!(admits(State::Unenrolled));
        for shut in [
            State::Enrolled,
            State::Unavailable(Cause::DirectoryDamaged),
            State::Unavailable(Cause::RecordDamaged),
            State::Unavailable(Cause::Unreadable),
        ] {
            assert!(!admits(shut), "{shut:?}");
        }
    }

    /// The gate reads the shared predicate for the primary account's record,
    /// under the root it is given.
    #[test]
    fn the_console_reads_the_primary_record_name_under_its_root() {
        let damaged = State::Unavailable(Cause::DirectoryDamaged);
        let root = Root::new();
        assert_eq!(root.state(), State::Unenrolled);
        // Another account's name, or a temporary, is not the record.
        for name in ["1001", "tmp-1000", "10000"] {
            std::fs::write(root.login().join(name), b"").unwrap();
        }
        assert_eq!(root.state(), State::Unenrolled);
        root.enrol();
        assert_eq!(root.state(), State::Enrolled);
        std::fs::remove_file(root.login().join("1000")).unwrap();
        assert_eq!(root.state(), State::Unenrolled);
        // Another owner, a missing directory, a file and a link in its place.
        let other = Owner {
            uid: root.owner.uid ^ 1,
            ..root.owner
        };
        assert_eq!(console_state(&root.root, other), damaged);
        let held = root.root.join("held");
        std::fs::rename(root.login(), &held).unwrap();
        assert_eq!(root.state(), damaged);
        std::fs::write(root.login(), b"").unwrap();
        assert_eq!(root.state(), damaged);
        std::fs::remove_file(root.login()).unwrap();
        symlink(&held, root.login()).unwrap();
        assert_eq!(root.state(), damaged);
        std::fs::remove_file(root.login()).unwrap();
        std::fs::rename(&held, root.login()).unwrap();
        assert_eq!(root.state(), State::Unenrolled);
    }

    /// The child half of `the_gate_parks_a_refusal_and_returns_otherwise`:
    /// runs the gate for the caller, root and owner it is handed, and says so
    /// only if the gate returned. Without them it does nothing.
    #[test]
    #[ignore = "spawned by the_gate_parks_a_refusal_and_returns_otherwise"]
    fn console_gate_child() {
        let (Some(caller), Some(root), Some(owner)) = (
            std::env::var(CHILD_CALLER).ok(),
            std::env::var_os(CHILD_ROOT),
            std::env::var(CHILD_OWNER).ok(),
        ) else {
            return;
        };
        let columns: Vec<u32> = caller.split(',').map(|id| id.parse().unwrap()).collect();
        let (uid, gid) = owner.split_once(':').unwrap();
        let owner = Owner {
            uid: uid.parse().unwrap(),
            gid: gid.parse().unwrap(),
        };
        guard_console_at(
            &status(columns.try_into().unwrap()),
            Path::new(&root),
            owner,
        );
        emit_err(&format!("{PROCEEDED}\n"));
    }

    /// A child the test still holds, and the file its stderr goes to. A
    /// file rather than a pipe, so the test can read what the child has said
    /// so far without blocking on a child that never ends. Dropping it kills
    /// the child, so a failed assertion leaves no parked process behind.
    struct Held {
        child: Child,
        log: PathBuf,
    }

    impl Held {
        fn said(&self) -> String {
            std::fs::read_to_string(&self.log).unwrap_or_default()
        }
    }

    impl Drop for Held {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
            let _ = std::fs::remove_file(&self.log);
        }
    }

    /// The gate in a child of its own: refused, it never returns, and only
    /// another process can watch that without parking itself.
    fn spawn_gate(caller: [u32; 4], root: &Path, owner: Owner) -> Held {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let log = std::env::temp_dir().join(format!(
            "td-login-gate-{}-{}.log",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let stderr = std::fs::File::create(&log).unwrap();
        let caller: Vec<String> = caller.iter().map(u32::to_string).collect();
        let child = Command::new(std::env::current_exe().unwrap())
            .args([CHILD_TEST, "--exact", "--ignored", "--nocapture"])
            .env(CHILD_CALLER, caller.join(","))
            .env(CHILD_ROOT, root)
            .env(CHILD_OWNER, format!("{}:{}", owner.uid, owner.gid))
            // Never a terminal: the root caller's hand-back then finds
            // /dev/null, which is no controlling terminal, and changes
            // nothing.
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(stderr)
            .spawn()
            .unwrap();
        Held { child, log }
    }

    const ROOT_CALLER: [u32; 4] = [0; 4];
    const USER_CALLER: [u32; 4] = [1000; 4];
    /// The shape a setuid-root exec would leave: root in some columns.
    const MIXED_CALLER: [u32; 4] = [1000, 0, 0, 0];
    /// How long a loaded machine may take to start a child and reach the
    /// gate. Generous: only a broken gate waits it out.
    const DEADLINE: Duration = Duration::from_secs(30);
    /// After the line, how long a refused gate must stay alive.
    const GRACE: Duration = Duration::from_millis(500);
    const POLL: Duration = Duration::from_millis(20);

    /// Refused, the gate never returns: for a caller root in some column,
    /// each state that is not unenrolled writes the one line last and is
    /// still alive after a grace period past it. It returns for a root
    /// caller on an unenrolled machine, and for a caller root in no column
    /// whatever the state: that caller can switch to nobody but itself, and
    /// the directory is root's to read. Every wait is bounded, so a broken
    /// gate reds rather than hangs.
    #[test]
    fn the_gate_parks_a_refusal_and_returns_otherwise() {
        let unenrolled = Root::new();
        let enrolled = Root::new();
        enrolled.enrol();
        let missing = Root::new();
        std::fs::remove_dir(missing.login()).unwrap();
        let not_a_directory = Root::new();
        std::fs::remove_dir(not_a_directory.login()).unwrap();
        std::fs::write(not_a_directory.login(), b"").unwrap();
        let foreign = Root::new();
        let foreign_owner = Owner {
            uid: foreign.owner.uid ^ 1,
            ..foreign.owner
        };

        let mut refused = [
            (
                "enrolled",
                spawn_gate(ROOT_CALLER, &enrolled.root, enrolled.owner),
            ),
            (
                "missing",
                spawn_gate(ROOT_CALLER, &missing.root, missing.owner),
            ),
            (
                "not a directory",
                spawn_gate(ROOT_CALLER, &not_a_directory.root, not_a_directory.owner),
            ),
            (
                "foreign owner",
                spawn_gate(ROOT_CALLER, &foreign.root, foreign_owner),
            ),
            (
                "enrolled, mixed caller",
                spawn_gate(MIXED_CALLER, &enrolled.root, enrolled.owner),
            ),
        ];
        let mut returned = [
            (
                "unenrolled",
                spawn_gate(ROOT_CALLER, &unenrolled.root, unenrolled.owner),
            ),
            (
                "unenrolled, mixed caller",
                spawn_gate(MIXED_CALLER, &unenrolled.root, unenrolled.owner),
            ),
            (
                "enrolled, user caller",
                spawn_gate(USER_CALLER, &enrolled.root, enrolled.owner),
            ),
            (
                "missing, user caller",
                spawn_gate(USER_CALLER, &missing.root, missing.owner),
            ),
        ];

        let deadline = std::time::Instant::now() + DEADLINE;
        for (case, held) in &mut refused {
            while !held.said().contains(CONSOLE_REFUSED) {
                assert!(
                    held.child.try_wait().unwrap().is_none(),
                    "{case}: the gate exited without the line: {:?}",
                    held.said()
                );
                assert!(
                    std::time::Instant::now() < deadline,
                    "{case}: no line by the deadline: {:?}",
                    held.said()
                );
                std::thread::sleep(POLL);
            }
        }
        std::thread::sleep(GRACE);
        for (case, held) in &mut refused {
            assert!(
                held.child.try_wait().unwrap().is_none(),
                "{case}: the refused gate exited"
            );
            held.child.kill().unwrap();
            held.child.wait().unwrap();
            let said = held.said();
            assert!(said.ends_with(CONSOLE_REFUSED), "{case}: {said:?}");
            assert_eq!(said.matches(CONSOLE_REFUSED).count(), 1, "{case}");
            assert!(!said.contains(PROCEEDED), "{case}: {said:?}");
        }
        for (case, held) in &mut returned {
            let exit = loop {
                if let Some(exit) = held.child.try_wait().unwrap() {
                    break exit;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "{case}: the gate did not return"
                );
                std::thread::sleep(POLL);
            };
            let said = held.said();
            assert!(exit.success(), "{case}: {exit:?}: {said}");
            assert!(said.contains(PROCEEDED), "{case}: {said:?}");
            assert!(!said.contains(CONSOLE_REFUSED), "{case}: {said:?}");
        }
    }
}

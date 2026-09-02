//! `login` — start a session for a named user.
//!
//! On a td image this is what getty execs: `/etc/autologin` runs
//! `login -f <user>`, which is how the machine reaches its greeter at all. The
//! `-f` (already-authenticated) path is therefore the one the boot proves on
//! every start; the interactive path is the one the policy table of
//! THREAT-MODEL.md §3 governs.

use crate::creds::Credentials;
use crate::db::{self, Account, Denied};
use crate::session::{self, Env, Session};
use crate::status::Status;
use crate::{emit, emit_err};

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
    let opts = parse(args).map_err(|e| format!("{e}\nusage: login [-p] [-h HOST] [-f USER] [USER]"))?;
    let status = Status::read()?;
    let (target, forced) = opts.target();
    // `-f` bypasses the account's own secret, so it is root's to use. Without
    // this the refusal would still come — from `creds::apply`, after the
    // database said yes — and the diagnostic would name the wrong step.
    if forced && !status.is_root() {
        return Err("only root may use -f (it starts a session without authenticating)".into());
    }
    let mode = if opts.preserve { Env::Preserve } else { Env::Fresh };
    match target {
        Some(name) => start(name, forced, mode, &status),
        None => ask(mode, &status),
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
/// front end reaches it: `login` through `start` and the prompt loop, `su` and
/// `exec-as` with `forced`. `su` used to carry its own copy of four of these
/// five steps — one policy in two places, with the compiler checking only the
/// `match` — and `the_session_policy_is_decided_in_one_place` is what stops a
/// third appearing.
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
        env: session::environment(account, &account.shell, mode, &session::inherited()),
        cwd: Some(cwd),
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
}

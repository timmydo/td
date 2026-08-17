//! What a session is, and the one path into one.
//!
//! Everything that needs root — the terminal hand-over, reading the account
//! database, deciding the working directory — happens in the applet BEFORE a
//! `Session` is built. `enter` then drops privilege and execs, in that order and
//! nothing between: it is the only caller of `creds::apply` in the crate, which
//! `main.rs`'s confinement test asserts, so there is exactly one place where
//! td-login stops being root.

use std::os::unix::process::CommandExt;
use std::process::Command;

use crate::creds::{self, Credentials};
use crate::db::Account;

/// The one `PATH` a td session gets. td images have no `/usr` and no `/sbin`;
/// `/bin` is a pure symlink farm into `/td/store`. Inheriting the caller's
/// `PATH` into a session is how a relative entry reaches a root shell, so it is
/// set in both modes rather than preserved (THREAT-MODEL.md §5).
pub const PATH: &str = "/bin";

/// Fallback working directory when the account's home is unusable. `/` always
/// exists and is always traversable; a session that cannot `chdir` at all is one
/// that never starts.
pub const ROOTDIR: &str = "/";

/// How much of the caller's environment survives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Env {
    /// A login session: discard the caller's environment except `TERM`.
    Fresh,
    /// `su` without `-`, or `login -p`: keep it, but still restate the five
    /// variables that describe WHO the session belongs to.
    Preserve,
    /// `exec-as`: discard ALL of it, `TERM` included, leaving exactly the five
    /// identity variables.
    ///
    /// A mode rather than "`Fresh` over an empty list", which computes the same
    /// environment, because the decision is then a TYPE the caller states and a
    /// test can hand a populated environment to. Written the other way, nothing
    /// distinguishes this from `Fresh` — or from `Preserve` — once the list is
    /// empty, so a regression that restored inheritance would be invisible to
    /// any test whose own environment happened to be bare.
    ///
    /// `TERM` is what separates it from `Fresh`, and the difference is not
    /// stylistic: a login session keeps it because a terminal type is a
    /// property of the terminal rather than of the caller, and a supervised
    /// daemon has no terminal — so there it is the SUPERVISOR's value, and one
    /// that quietly changes a program's output between one boot and the next.
    Service,
}

/// A ready-to-enter session. Built entirely while privileged; `enter` consumes
/// it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub creds: Credentials,
    /// Absolute path to exec. `db::account` rejects a relative shell, and no
    /// `PATH` search happens here.
    pub program: String,
    /// argv[0]. A leading `-` is what tells the shell it is a login shell, and
    /// is why `/etc/profile` gets sourced at all.
    pub arg0: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    /// `None` INHERITS the caller's directory. That is not the same as `/`: a
    /// plain `su` must leave the caller where it was, and chdir'ing into a
    /// directory the target user cannot traverse would fail the exec for a
    /// reason unrelated to the credentials.
    pub cwd: Option<String>,
}

/// The environment a session starts with.
///
/// `shell` is passed separately from `account` because `su -s SHELL` runs a
/// program the account does not name, and `SHELL` must describe the program this
/// session is actually running — a shell that reports one interpreter while
/// running another is a lie every re-exec inside the session then acts on.
/// `inherited` is passed in rather than read from the process so the decision is
/// testable.
pub fn environment(
    account: &Account,
    shell: &str,
    mode: Env,
    inherited: &[(String, String)],
) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = Vec::with_capacity(inherited.len() + 5);
    for (key, value) in inherited {
        // A service keeps nothing at all; see `Env::Service`.
        if mode == Env::Service {
            continue;
        }
        // TERM survives a reset because the terminal type is a property of the
        // terminal, not of whoever called us — a fresh session on a vt100 that
        // thinks it is on a dumb terminal is unusable.
        if mode == Env::Fresh && key != "TERM" {
            continue;
        }
        // A name may appear TWICE: `environ` is a plain array and `execve` never
        // deduplicates it. Collapsing to the FIRST is what `getenv(3)` answers
        // with, so the session sees what it would have seen — and leaving both
        // would make the answer depend on WHO asks, since the map `Command::env`
        // builds keeps the LAST. That is also what makes the five overrides below
        // final rather than advisory.
        if env.iter().any(|(seen, _): &(String, String)| seen == key) {
            continue;
        }
        env.push((key.clone(), value.clone()));
    }
    for (key, value) in [
        ("HOME", account.home.as_str()),
        ("SHELL", shell),
        ("USER", account.name.as_str()),
        ("LOGNAME", account.name.as_str()),
        ("PATH", PATH),
    ] {
        set(&mut env, key, value);
    }
    env
}

/// Remove EVERY entry for `key`, then append one. Not "overwrite the first
/// match": an environment may legitimately carry the same name twice (`environ`
/// is a plain array and `execve` never deduplicates it), and overwriting only
/// the first leaves the second in place — `Command::env` then collapses them
/// keeping the LAST, which is the caller's value, not ours. That turns each of
/// these five overrides into a suggestion.
fn set(env: &mut Vec<(String, String)>, key: &str, value: &str) {
    env.retain(|(k, _)| k != key);
    env.push((key.to_string(), value.to_string()));
}

/// The environment this process was given, as pairs — verbatim, duplicates and
/// all. `environment` above is what collapses them, so the rule lives in the one
/// function that decides what a session gets rather than in the collector a test
/// can bypass.
///
/// Non-UTF-8 entries ARE dropped here: a variable whose bytes we could not
/// reproduce exactly would be passed on wrong, and every variable a td session
/// needs is ASCII.
pub fn inherited() -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    for (key, value) in std::env::vars_os() {
        if let (Some(k), Some(v)) = (key.to_str(), value.to_str()) {
            pairs.push((k.to_string(), v.to_string()));
        }
    }
    pairs
}

/// argv[0] for a login shell: the shell's basename with a `-` in front, which is
/// how every Bourne shell learns to source the login profile.
pub fn login_arg0(shell: &str) -> String {
    format!("-{}", crate::basename(shell))
}

/// Drop privilege, then exec. Never returns on success.
///
/// `creds::apply` verifies the switch against `/proc/self/status` before this
/// returns, so an `exec` below always runs with credentials the kernel has
/// confirmed. Nothing that needs root may be added between these two statements
/// — that is the whole ordering contract of THREAT-MODEL.md §2 expressed as
/// control flow.
pub fn enter(session: &Session) -> Result<u8, String> {
    creds::apply(&session.creds)?;

    let mut command = Command::new(&session.program);
    command.arg0(&session.arg0);
    command.args(&session.args);
    command.env_clear();
    for (key, value) in &session.env {
        command.env(key, value);
    }
    if let Some(cwd) = &session.cwd {
        command.current_dir(cwd);
    }
    let failure = command.exec();
    Err(format!(
        "cannot exec {} as {}: {failure}",
        session.program, session.arg0
    ))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;

    fn account() -> Account {
        Account {
            name: "tester".into(),
            uid: 1000,
            gid: 1000,
            gecos: "Test User".into(),
            home: "/home/tester".into(),
            shell: "/bin/sh".into(),
        }
    }

    fn pairs(xs: &[(&str, &str)]) -> Vec<(String, String)> {
        xs.iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    fn value<'a>(env: &'a [(String, String)], key: &str) -> Option<&'a str> {
        for (k, v) in env {
            if k == key {
                return Some(v.as_str());
            }
        }
        None
    }

    /// A login session keeps TERM and nothing else of the caller's. The two
    /// entries that must NOT survive are the interesting ones: an inherited
    /// PATH steers every unqualified command the session runs, and an inherited
    /// HOME sends its dotfile reads somewhere the caller chose.
    #[test]
    fn a_fresh_environment_keeps_only_term() {
        let caller = pairs(&[
            ("TERM", "vt100"),
            ("PATH", "/tmp/evil"),
            ("HOME", "/tmp"),
            ("LD_PRELOAD", "/tmp/x.so"),
            ("SSH_AUTH_SOCK", "/tmp/agent"),
        ]);
        let env = environment(&account(), "/bin/sh", Env::Fresh, &caller);
        assert_eq!(value(&env, "TERM"), Some("vt100"));
        assert_eq!(value(&env, "PATH"), Some("/bin"));
        assert_eq!(value(&env, "HOME"), Some("/home/tester"));
        assert_eq!(value(&env, "SHELL"), Some("/bin/sh"));
        assert_eq!(value(&env, "USER"), Some("tester"));
        assert_eq!(value(&env, "LOGNAME"), Some("tester"));
        assert_eq!(value(&env, "LD_PRELOAD"), None);
        assert_eq!(value(&env, "SSH_AUTH_SOCK"), None);
        assert_eq!(env.len(), 6);
    }

    /// A preserved environment still describes the TARGET account: the five
    /// identity variables are overwritten in place, never appended alongside a
    /// stale copy that a consumer might read instead.
    #[test]
    fn a_preserved_environment_still_restates_who_the_session_belongs_to() {
        let caller = pairs(&[
            ("HOME", "/root"),
            ("USER", "root"),
            ("LOGNAME", "root"),
            ("PATH", "/tmp/evil"),
            ("EDITOR", "vi"),
        ]);
        let env = environment(&account(), "/bin/sh", Env::Preserve, &caller);
        assert_eq!(value(&env, "HOME"), Some("/home/tester"));
        assert_eq!(value(&env, "USER"), Some("tester"));
        assert_eq!(value(&env, "LOGNAME"), Some("tester"));
        assert_eq!(value(&env, "PATH"), Some("/bin"));
        assert_eq!(value(&env, "EDITOR"), Some("vi"));
        assert_eq!(env.iter().filter(|(k, _)| k == "HOME").count(), 1);
        assert_eq!(env.len(), 6);
    }

    /// No TERM to carry means no TERM in the session, not an empty one — an
    /// empty TERM is a value programs act on, and absence is what they expect.
    #[test]
    fn an_absent_term_is_not_invented() {
        let env = environment(&account(), "/bin/sh", Env::Fresh, &pairs(&[("PATH", "/x")]));
        assert_eq!(value(&env, "TERM"), None);
        assert_eq!(env.len(), 5);
    }

    /// A service session keeps NOTHING of the caller's, `TERM` included, and is
    /// the only mode that does. Asserted against a populated environment
    /// rather than an empty one, which is the whole point of it being a mode:
    /// over an empty list every mode agrees, so a test that passed one would
    /// green whatever the mode did.
    #[test]
    fn a_service_environment_keeps_nothing_of_the_callers() {
        let caller = pairs(&[
            ("TERM", "vt100"),
            ("PATH", "/tmp/evil"),
            ("HOME", "/tmp"),
            ("XDG_RUNTIME_DIR", "/run/user/0"),
            ("LD_PRELOAD", "/tmp/x.so"),
        ]);
        let env = environment(&account(), "/bin/sh", Env::Service, &caller);
        assert_eq!(value(&env, "TERM"), None, "a daemon has no terminal");
        assert_eq!(value(&env, "XDG_RUNTIME_DIR"), None);
        assert_eq!(value(&env, "LD_PRELOAD"), None);
        // Exactly the five identity variables, all describing the account.
        assert_eq!(value(&env, "PATH"), Some(PATH));
        assert_eq!(value(&env, "HOME"), Some("/home/tester"));
        assert_eq!(value(&env, "SHELL"), Some("/bin/sh"));
        assert_eq!(value(&env, "USER"), Some("tester"));
        assert_eq!(value(&env, "LOGNAME"), Some("tester"));
        assert_eq!(env.len(), 5, "unexpected environment: {env:?}");
    }

    /// A name may appear TWICE in an environment — `environ` is an array, not a
    /// map — and an override that patched only the first occurrence would leave
    /// the second for `Command::env` to keep. That is a full bypass of all five
    /// overrides, `PATH` included, so both modes are checked.
    #[test]
    fn a_duplicated_name_cannot_survive_an_override() {
        let caller = pairs(&[
            ("PATH", "/bin"),
            ("PATH", "/tmp/evil"),
            ("HOME", "/root"),
            ("HOME", "/tmp/evil"),
            ("TERM", "vt100"),
            ("TERM", "evil"),
        ]);
        for mode in [Env::Fresh, Env::Preserve] {
            let env = environment(&account(), "/bin/sh", mode, &caller);
            for key in ["PATH", "HOME", "SHELL", "USER", "LOGNAME"] {
                assert_eq!(
                    env.iter().filter(|(k, _)| k == key).count(),
                    1,
                    "{key} survives twice under {mode:?}, so the caller's copy wins"
                );
            }
            assert_eq!(value(&env, "PATH"), Some("/bin"));
            assert_eq!(value(&env, "HOME"), Some("/home/tester"));
        }
        // ...and a duplicate we do NOT override collapses to the one `getenv`
        // would have answered with, rather than being carried through twice.
        let env = environment(&account(), "/bin/sh", Env::Preserve, &caller);
        assert_eq!(env.iter().filter(|(k, _)| k == "TERM").count(), 1);
    }

    /// `su -s SHELL` runs a program the account does not name, and the session's
    /// `SHELL` must be that program. Reporting the account's instead would have
    /// every re-exec inside the session reach for an interpreter it is not using.
    #[test]
    fn the_shell_variable_names_the_program_actually_run() {
        let env = environment(&account(), "/bin/td-sh", Env::Fresh, &[]);
        assert_eq!(value(&env, "SHELL"), Some("/bin/td-sh"));
        assert_eq!(value(&env, "HOME"), Some("/home/tester"));
    }

    #[test]
    fn a_login_shell_gets_a_dashed_argv0() {
        assert_eq!(login_arg0("/bin/sh"), "-sh");
        assert_eq!(login_arg0("/td/store/abc-td-sh/bin/td-sh"), "-td-sh");
        assert_eq!(login_arg0("sh"), "-sh");
    }
}

//! `exec-as` — run one named program as another user, with no shell anywhere.
//!
//! This exists because td-svc has no `user=` field, so every unit that must run
//! unprivileged spells its credential switch as
//! `exec=/bin/su -s /bin/sh {user} -c '…'`. That hands a STRING to a shell which
//! then word-splits it, which is both more shell than directive 3 wants and a
//! parser between the supervisor and the program it supervises: a store path
//! containing a space, a glob character in an argument, or an `$` anywhere is
//! re-interpreted by `sh` before the daemon ever starts. `exec-as` takes a
//! literal argv instead and execs it directly — the words the unit wrote are the
//! words the program receives.
//!
//! It is the same operation `su` performs, minus the shell: resolve the account,
//! refuse a locked one, build the session, and hand it to `session::enter`, which
//! is the crate's ONE credential switch and carries the `/proc/self/status`
//! readback with it. So this applet adds no syscall and no second way to become
//! somebody — `main.rs`'s confinement test asserts `session::enter` stays the
//! only caller of `creds::apply`, and this file is a new caller of `enter`, not
//! of `apply`.

use crate::creds::Credentials;
use crate::db;
use crate::login;
use crate::session::{self, Env, Session};

/// What an `exec-as` argv asked for.
#[derive(Debug, PartialEq, Eq)]
pub struct Options {
    pub user: String,
    /// Absolute path to exec. No `PATH` search happens, as in `login` and `su -s`.
    pub program: String,
    pub args: Vec<String>,
}

/// `exec-as USER -- PROGRAM [ARG…]`.
///
/// The `--` is REQUIRED rather than optional, and that is the whole of the
/// parser. `su` permutes its options around the user name because the image's
/// own scripts depend on it; this form has no options at all, so a separator that
/// must be there removes the only ambiguity available — whether a leading-dash
/// word after the user is an option for `exec-as` or the first argument of the
/// program. It is also what keeps this parser from growing: an option added later
/// cannot be confused with an argument that already works.
pub fn parse(args: &[String]) -> Result<Options, String> {
    let Some(user) = args.first() else {
        return Err("no user named".into());
    };
    if user.starts_with('-') {
        // `exec-as` has no options, so a leading dash is a mistyped invocation
        // rather than a user named `-l`. Refusing beats resolving an account
        // nobody meant.
        return Err(format!("{user:?} is not a user name; exec-as takes no options"));
    }
    match args.get(1).map(String::as_str) {
        Some("--") => {}
        Some(other) => return Err(format!("expected `--` after the user name, got {other:?}")),
        None => return Err("expected `--` after the user name".into()),
    }
    let Some(program) = args.get(2) else {
        return Err("no program named after `--`".into());
    };
    if !program.starts_with('/') {
        return Err(format!("{program:?} is not an absolute path"));
    }
    if program.ends_with('/') {
        // Its basename is `""`, and every program these units start dispatches
        // ON argv[0]. The exec fails anyway; this says why.
        return Err(format!("{program:?} ends in a slash, so it names no program"));
    }
    Ok(Options {
        user: user.clone(),
        program: program.clone(),
        args: args.get(3..).unwrap_or(&[]).to_vec(),
    })
}

/// Build the session `run` will enter. Pure — every input is passed in — so the
/// four decisions this applet makes beyond `su`'s are assertable without an
/// `exec`: the environment mode, what `SHELL` names, `argv[0]`, and the working
/// directory. `run` is then only the database lookups and the switch.
fn session_for(
    account: &db::Account,
    groups: &[u32],
    opts: Options,
    inherited: &[(String, String)],
) -> Session {
    // The ACCOUNT's shell, not the program: `su -s` names the program because
    // there it IS the session's interpreter, and a daemon is not one —
    // `SHELL=/bin/td-busd` re-execs the daemon for anything that spawns $SHELL.
    let env = session::environment(account, &account.shell, Env::Service, inherited);
    Session {
        creds: Credentials::new(account.uid, account.gid, groups),
        // Plain basename: a leading `-` is what makes a shell source a login
        // profile, and this is a service.
        arg0: crate::basename(&opts.program).to_string(),
        program: opts.program,
        args: opts.args,
        env,
        // `/`, not the caller's. A daemon holding the supervisor's directory
        // pins that mount for as long as it runs.
        cwd: Some(session::ROOTDIR.to_string()),
    }
}

pub fn run(args: &[String]) -> Result<u8, String> {
    let opts = parse(args).map_err(|e| format!("{e}\nusage: exec-as USER -- PROGRAM [ARG…]"))?;
    // FORCED, exactly as `su` is — not because reaching a switch needs root,
    // which is false: `creds::apply` returns early when the credentials already
    // match, so `exec-as tester` run BY tester is a no-op. What needs root is a
    // switch that CHANGES something, and `creds::may_switch` refuses that over
    // all four uid columns. THREAT-MODEL.md §3.
    let account = login::authorize(&opts.user, true)?;
    let groups = db::supplementary(&opts.user)?;
    let session = session_for(&account, &groups, opts, &session::inherited());
    session::enter(&session)
}

/// The service-only sibling of `run`.
///
/// The argv grammar and credential application are identical. Only the
/// account authorization differs: this path accepts the exact service marker
/// and rejects every interactive, hashed, or ordinarily locked account.
pub fn run_service(args: &[String]) -> Result<u8, String> {
    let opts = parse(args)
        .map_err(|e| format!("{e}\nusage: exec-service-as USER -- PROGRAM [ARG…]"))?;
    let account = login::authorize_service(&opts.user)?;
    let groups = db::supplementary(&opts.user)?;
    let session = session_for(&account, &groups, opts, &session::inherited());
    session::enter(&session)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;

    fn argv(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|s| (*s).to_string()).collect()
    }

    /// The form APPLICATIONS.md's units are written in.
    #[test]
    fn the_designs_own_unit_line_parses() {
        let o = parse(&argv(&[
            "tester",
            "--",
            "/bin/td-busd",
            "run",
            "--socket",
            "/run/user/1000/bus",
        ]))
        .unwrap();
        assert_eq!(o.user, "tester");
        assert_eq!(o.program, "/bin/td-busd");
        assert_eq!(o.args, argv(&["run", "--socket", "/run/user/1000/bus"]));
    }

    /// A program with NO arguments is the ordinary case, not an edge one.
    #[test]
    fn a_bare_program_parses() {
        let o = parse(&argv(&["tester", "--", "/bin/true"])).unwrap();
        assert!(o.args.is_empty());
    }

    /// Everything after `--` belongs to the program, INCLUDING words that look
    /// like options for exec-as itself. This is the property the mandatory `--`
    /// exists to give, so it is asserted rather than assumed: without it, a
    /// daemon whose first argument is `-l` could not be spelled at all.
    #[test]
    fn the_programs_own_options_are_not_read_as_ours() {
        let o = parse(&argv(&["tester", "--", "/bin/td-busd", "-l", "--", "-c"])).unwrap();
        assert_eq!(o.args, argv(&["-l", "--", "-c"]));
    }

    fn account() -> db::Account {
        db::Account {
            name: "tester".into(),
            uid: 1000,
            gid: 1000,
            gecos: "Test User".into(),
            home: "/home/tester".into(),
            shell: "/bin/sh".into(),
        }
    }

    fn value<'a>(env: &'a [(String, String)], key: &str) -> Option<&'a str> {
        for (k, v) in env {
            if k == key {
                return Some(v.as_str());
            }
        }
        None
    }

    /// The four decisions this applet makes beyond `su`'s, pinned together
    /// because each is a silent failure rather than a loud one.
    #[test]
    fn the_session_a_daemon_gets() {
        let opts = parse(&argv(&["tester", "--", "/bin/td-busd", "run"])).unwrap();
        // A POPULATED caller environment, so "nothing survives" is a property
        // this test can observe rather than one an empty list would fake.
        let caller = vec![
            ("TERM".to_string(), "vt100".to_string()),
            ("XDG_RUNTIME_DIR".to_string(), "/run/user/0".to_string()),
            ("PATH".to_string(), "/tmp/evil".to_string()),
        ];
        let s = session_for(&account(), &[1000, 27], opts, &caller);

        // argv[0] is the plain basename: a dashed one would make a shell source
        // a login profile, and nothing here is a login.
        assert_eq!(s.arg0, "td-busd");
        assert_eq!(s.program, "/bin/td-busd");
        assert_eq!(s.args, argv(&["run"]));

        // SHELL is the account's, NOT the program: a daemon pointed at itself
        // re-execs the daemon for anything that spawns $SHELL.
        assert_eq!(value(&s.env, "SHELL"), Some("/bin/sh"));

        // NOTHING of the caller's environment reaches the daemon, so what it
        // sees is not a function of the boot path. `TERM` is the one this has
        // to state rather than assume: `Env::Fresh` alone would keep it, which
        // is right for a login session and wrong for a process with no
        // terminal — and it is exactly the sort of value that changes a
        // program's output between one boot and the next.
        assert_eq!(value(&s.env, "TERM"), None);
        assert_eq!(value(&s.env, "XDG_RUNTIME_DIR"), None);
        assert_eq!(value(&s.env, "PATH"), Some(session::PATH));
        assert_eq!(value(&s.env, "HOME"), Some("/home/tester"));
        assert_eq!(value(&s.env, "USER"), Some("tester"));
        assert_eq!(value(&s.env, "LOGNAME"), Some("tester"));
        // Exactly the five identity variables and nothing else.
        assert_eq!(s.env.len(), 5, "unexpected environment: {:?}", s.env);

        // `/`, not the caller's directory: the supervisor's cwd is the one
        // piece of its state that would otherwise reach the daemon.
        assert_eq!(s.cwd.as_deref(), Some(session::ROOTDIR));
    }

    #[test]
    fn ambiguous_or_unknown_argv_is_refused() {
        assert!(parse(&argv(&[])).is_err(), "no user");
        assert!(parse(&argv(&["tester"])).is_err(), "no --");
        assert!(parse(&argv(&["tester", "--"])).is_err(), "no program");
        assert!(
            parse(&argv(&["tester", "/bin/true"])).is_err(),
            "the -- is required, so a missing one is refused rather than guessed"
        );
        assert!(
            parse(&argv(&["-l", "--", "/bin/true"])).is_err(),
            "exec-as has no options, so a leading dash is a mistake"
        );
        assert!(
            parse(&argv(&["tester", "--", "td-busd"])).is_err(),
            "no PATH search, so a relative program is refused rather than resolved"
        );
        assert!(
            parse(&argv(&["tester", "--", "/bin/td-busd/"])).is_err(),
            "a trailing slash leaves an empty basename, so argv[0] would be empty \
             — and every program these units start dispatches on argv[0]"
        );
    }
}

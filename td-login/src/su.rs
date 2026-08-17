//! `su` — run a shell, or one command, as another user.
//!
//! td-login is never installed setuid-root (THREAT-MODEL.md §4), so this can
//! only ever DROP privilege: root becomes somebody else, and nobody else becomes
//! anybody. That is what makes `-s SHELL` and `-c CMD` — an escalation surface in
//! a setuid `su` — inert here.
//!
//! The image depends on this: `/etc/rootcheck` and `/etc/bootsuccess` run every
//! unprivileged health leg through `su -s /bin/sh <user> -c '…'`, so a boot that
//! reaches its markers has exercised this applet several times over.

use crate::creds::Credentials;
use crate::db;
use crate::login;
use crate::session::{self, Env, Session};
use crate::{emit_err, ROOT};

/// What `su`'s argv asked for.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Options {
    /// `-` or `-l`: a login session (fresh environment, home directory, dashed
    /// argv[0]) rather than "the same session as another user".
    pub login: bool,
    /// `-c CMD`.
    pub command: Option<String>,
    /// `-s SHELL`, overriding the account's own.
    pub shell: Option<String>,
    /// The target account; `None` means root, as everywhere else.
    pub user: Option<String>,
    /// Operands after the user name, passed on to the shell.
    pub args: Vec<String>,
}

/// Options may appear BEFORE or AFTER the user name — `su -s /bin/sh tester -c
/// '…'` is the form the image's own scripts use, and busybox's getopt permutes
/// exactly that way. The first bare word is the user; any further bare word is
/// an argument for the shell.
pub fn parse(args: &[String]) -> Result<Options, String> {
    let mut opts = Options::default();
    let mut rest_are_operands = false;
    let mut i = 0usize;
    while let Some(arg) = args.get(i) {
        i += 1;
        if rest_are_operands || !arg.starts_with('-') {
            if opts.user.is_none() {
                opts.user = Some(arg.clone());
            } else {
                opts.args.push(arg.clone());
            }
            continue;
        }
        match arg.as_str() {
            "--" => rest_are_operands = true,
            // A bare `-` is the oldest spelling of `-l` and is an OPTION, not a
            // user named "-".
            "-" | "-l" => opts.login = true,
            // `-m`/`-p` mean "keep the environment", which is what a non-login
            // su already does. Accepted so a caller that spells it out is not
            // refused; it cannot turn a login session back into a preserving one.
            "-m" | "-p" => {}
            "-c" | "-s" => {
                let Some(value) = args.get(i) else {
                    return Err(format!("{arg} needs an argument"));
                };
                i += 1;
                let slot = if arg == "-c" {
                    &mut opts.command
                } else {
                    &mut opts.shell
                };
                if slot.is_some() {
                    return Err(format!("{arg} given more than once"));
                }
                *slot = Some(value.clone());
            }
            other => return Err(format!("unrecognised argument {other:?}")),
        }
    }
    if let Some(shell) = &opts.shell {
        // `login` execs by absolute path with no PATH search, and so does this.
        if !shell.starts_with('/') {
            return Err(format!("-s {shell:?} is not an absolute path"));
        }
    }
    Ok(opts)
}

pub fn run(args: &[String]) -> Result<u8, String> {
    let opts = parse(args)
        .map_err(|e| format!("{e}\nusage: su [-] [-l] [-m|-p] [-s SHELL] [-c CMD] [USER [ARG…]]"))?;
    let name = opts.user.as_deref().unwrap_or(ROOT);
    // FORCED, and through the crate's one decision rather than a copy of it:
    // `su` had its own version of four of `authorize`'s five steps, which is a
    // policy that has to be changed in two places with the compiler checking
    // neither. A LOCKED account is still refused — THREAT-MODEL.md section 3.
    let account = login::authorize(name, true)?;
    let groups = db::supplementary(name)?;
    let creds = Credentials::new(account.uid, account.gid, &groups);

    let program = opts.shell.clone().unwrap_or_else(|| account.shell.clone());
    let mut argv: Vec<String> = Vec::new();
    if let Some(command) = &opts.command {
        argv.push("-c".into());
        argv.push(command.clone());
    }
    argv.extend(opts.args.iter().cloned());

    let mode = if opts.login { Env::Fresh } else { Env::Preserve };
    // A login su moves to the account's home; a plain one stays where it is.
    // `None` is INHERIT, not `/`: chdir'ing into a directory the target user
    // cannot traverse would fail the exec for a reason that has nothing to do
    // with the credentials.
    let cwd = if opts.login {
        let home = std::path::Path::new(&account.home);
        if home.is_dir() {
            Some(account.home.clone())
        } else {
            emit_err(&format!(
                "su: home {} is not a directory; starting in {}\n",
                account.home,
                session::ROOTDIR
            ));
            Some(session::ROOTDIR.to_string())
        }
    } else {
        None
    };
    let arg0 = if opts.login {
        session::login_arg0(&program)
    } else {
        crate::basename(&program).to_string()
    };
    let env = session::environment(&account, &program, mode, &session::inherited());
    let session = Session {
        creds,
        program,
        arg0,
        args: argv,
        env,
        cwd,
    };
    session::enter(&session)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;

    fn argv(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|s| (*s).to_string()).collect()
    }

    /// The exact form `/etc/rootcheck` and `/etc/bootsuccess` use. Options
    /// AFTER the user name is the whole reason this parser permutes; reading
    /// `-c` as an operand would pass the health command to the shell as `$1`
    /// and run an interactive shell on a serial console instead.
    #[test]
    fn the_images_own_invocation_parses() {
        let o = parse(&argv(&["-s", "/bin/sh", "tester", "-c", "/bin/cat /etc/os-release"])).unwrap();
        assert_eq!(o.user.as_deref(), Some("tester"));
        assert_eq!(o.shell.as_deref(), Some("/bin/sh"));
        assert_eq!(o.command.as_deref(), Some("/bin/cat /etc/os-release"));
        assert!(!o.login);
        assert!(o.args.is_empty());
    }

    #[test]
    fn the_other_ordinary_forms_parse() {
        assert_eq!(parse(&argv(&[])).unwrap(), Options::default());
        let o = parse(&argv(&["-", "tester"])).unwrap();
        assert!(o.login);
        assert_eq!(o.user.as_deref(), Some("tester"));
        let o = parse(&argv(&["-l", "tester", "one", "two"])).unwrap();
        assert!(o.login);
        assert_eq!(o.args, argv(&["one", "two"]));
        // -m/-p are accepted and cannot undo -l.
        let o = parse(&argv(&["-l", "-m", "root"])).unwrap();
        assert!(o.login);
        // `--` ends option parsing.
        let o = parse(&argv(&["--", "-l"])).unwrap();
        assert!(!o.login);
        assert_eq!(o.user.as_deref(), Some("-l"));
    }

    #[test]
    fn ambiguous_or_unknown_argv_is_refused() {
        assert!(parse(&argv(&["-c"])).is_err());
        assert!(parse(&argv(&["-s"])).is_err());
        assert!(parse(&argv(&["-z"])).is_err());
        assert!(parse(&argv(&["-c", "a", "-c", "b"])).is_err());
        assert!(parse(&argv(&["-s", "sh"])).is_err(), "-s must be absolute");
    }
}

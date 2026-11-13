//! Named development sandboxes, each run by root's workspace launch intake
//! as a ledger account of its own (td-sandbox/DESIGN.md). td-sandbox keeps
//! the human's record of each and relays its terminal; root keeps its
//! account, grants and network, and confinement belongs to the jail root
//! starts.
#![forbid(unsafe_code)]
#[allow(
    dead_code,
    reason = "the runtime declarations td-app and the jail share"
)]
#[path = "../../engine/src/flathub_app.rs"]
mod flathub_app;
mod import;
mod intake;
mod relay;
mod state;
mod wire;
#[path = "../../engine/src/workspace_intake.rs"]
mod workspace_intake;
// The workload account ledger's name grammar, which a sandbox's name
// meets, its intake socket and its account homes.
#[allow(dead_code, reason = "the ledger's root half is td-authd's")]
#[path = "../../engine/src/workload_accounts.rs"]
mod workload_accounts;

use intake::Intake;
use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

const HELP: &str = "td-sandbox COMMAND\n\
  Each sandbox runs as an account of its own (tdwUID), through root's\n\
  workspace intake; it sees only the directories of your home you share.\n\
  init-default                import older sandboxes once; seed default with\n\
                              writable ~/sandbox\n\
  create NAME                 create a sandbox with a fresh home (network off);\n\
                              refused over a sandbox root holds with files\n\
                              or grants, which adopt or delete settle\n\
  adopt NAME                  record a sandbox root holds that has no record\n\
  list                        list sandboxes with their accounts and networks\n\
  inspect NAME                show account, home, network, runtime and grants\n\
  share NAME PATH --read|--write  share a directory below your home\n\
  unshare NAME PATH\n\
  network NAME off|host\n\
  runtime NAME RUNTIME|native bind a declared Flathub runtime at /usr\n\
  shell NAME                  start an interactive shell\n\
  run NAME -- COMMAND [ARG...] run a command in the sandbox\n\
  stop NAME                   terminate the active session\n\
  delete NAME                 delete an inactive sandbox, its grants and\n\
                              its home's contents; its account is kept\n\
  Sandboxes run only on td: every command but help, runtime and stop asks\n\
  root's workspace intake.\n";

/// How long `init-default` waits for root's intake at boot.
const BOOT_PATIENCE: Duration = Duration::from_secs(10);

fn error(message: impl ToString) -> io::Error {
    io::Error::other(message.to_string())
}

/// A refused allocation (td-alloc/DESIGN.md): `OutOfMemory`, an error that
/// allocates nothing.
pub(crate) fn refused<E>(_: E) -> io::Error {
    io::ErrorKind::OutOfMemory.into()
}

/// Text formatted into fallibly grown storage.
pub(crate) fn text(args: fmt::Arguments<'_>) -> io::Result<String> {
    td_alloc::format(args).map_err(refused)
}

/// `value` copied into fallibly grown storage.
pub(crate) fn owned(value: &str) -> io::Result<String> {
    td_alloc::string(value).map_err(refused)
}

/// An error saying what `args` formats, or, its text refused, the
/// refusal.
pub(crate) fn failure(args: fmt::Arguments<'_>) -> io::Error {
    text(args).map_or_else(|refusal| refusal, io::Error::other)
}
fn word(arg: Option<OsString>) -> io::Result<String> {
    arg.and_then(|v| v.into_string().ok())
        .ok_or_else(|| error(HELP))
}
fn end(args: &mut impl Iterator<Item = OsString>) -> io::Result<()> {
    match args.next() {
        Some(_) => Err(error(HELP)),
        None => Ok(()),
    }
}

/// `$HOME` as given, and canonical: grants are relative to the latter.
fn homes() -> io::Result<(PathBuf, PathBuf)> {
    let home = PathBuf::from(std::env::var_os("HOME").ok_or_else(|| error("HOME is unset"))?);
    let canonical = fs::canonicalize(&home)?;
    Ok((home, canonical))
}

/// A directory to share, as a grant row holds it: relative to the
/// canonical home, below it.
fn grant_path(home: &Path, path: &Path) -> io::Result<String> {
    let path = fs::canonicalize(path)?;
    match intake::relative(home, &path) {
        Some(relative) => owned(relative),
        None => Err(failure(format_args!(
            "{} is not below your home {}: only a directory below it can be shared",
            path.display(),
            home.display()
        ))),
    }
}

/// A share to remove, which may be gone: its canonical path when it is
/// there, else the path as given, below either form of the home; or the
/// row's own home-relative form as `inspect` shows it, after `~/`.
fn unshare_path(home: &Path, given_home: &Path, path: &Path) -> io::Result<String> {
    if let Some(row) = path.to_str().and_then(|p| p.strip_prefix("~/")) {
        return owned(row);
    }
    let resolved = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    match intake::relative(home, &resolved).or_else(|| intake::relative(given_home, path)) {
        Some(relative) => owned(relative),
        None => Err(failure(format_args!(
            "{} is not below your home",
            path.display()
        ))),
    }
}

/// What root answered a creation: the account, or that a sandbox of the
/// name already holds files in its home or grant rows, which root never
/// creates over.
enum Created {
    New(u32),
    Exists,
}

fn ask_create(name: &str, intake: &impl Intake) -> io::Result<Created> {
    match intake.ask(&intake::simple("create", name)?)? {
        intake::Answer::Ok(rest) => intake::created(&rest).map(Created::New),
        intake::Answer::Refused(reason) if reason == intake::EXISTS => Ok(Created::Exists),
        intake::Answer::Refused(reason) => Err(error(intake::shown(&reason))),
    }
}

/// Root's refusals by their words, told as what the human can do.
fn explain(name: &str, e: io::Error) -> io::Error {
    match e.to_string().as_str() {
        intake::EXISTS => failure(format_args!(
            "root already holds a sandbox named {name}, with files in its home or grants: \
             `td-sandbox adopt {name}` records it here, `td-sandbox delete {name}` discards it"
        )),
        intake::DELETED => failure(format_args!(
            "sandbox {name} was deleted: `td-sandbox create {name}` makes it again. Should \
             root refuse that as existing, a grant was made while it was being deleted, and \
             `td-sandbox delete {name}` clears it, and its record here"
        )),
        _ => e,
    }
}

/// Creates `name` through root, its record first, which holds the name
/// while root works and is removed again when root refuses. A sandbox
/// root already holds is never created over: the human adopts or deletes
/// it.
fn create(profiles: &Path, name: &str, intake: &impl Intake) -> io::Result<(state::Profile, u32)> {
    let profile = state::Profile::create(profiles, name, None, None)?;
    match ask_create(name, intake) {
        Ok(Created::New(uid)) => Ok((profile, uid)),
        Ok(Created::Exists) => {
            let _ = profile.remove();
            Err(explain(name, error(intake::EXISTS)))
        }
        Err(e) => {
            let _ = profile.remove();
            Err(e)
        }
    }
}

/// Records the sandbox `name` that root holds with files or grants and
/// that has no record here: one whose record was lost or never written.
fn adopt(profiles: &Path, name: &str, intake: &impl Intake) -> io::Result<u32> {
    let status = intake
        .ask(&intake::simple("status", name)?)?
        .accepted()
        .and_then(|rest| intake::status(&rest))?;
    match status {
        intake::Status::Held(uid) => {
            drop(state::Profile::create(profiles, name, None, None)?);
            Ok(uid)
        }
        intake::Status::Free(_) => Err(failure(format_args!(
            "sandbox {name} holds no file and no grant, so there is nothing to adopt: \
             `td-sandbox create {name}` makes it"
        ))),
        intake::Status::Absent => Err(failure(format_args!("root holds no sandbox named {name}"))),
    }
}

/// Imports the old profiles once, then seeds `default` unless a readable
/// record of it exists: an unreadable one is removed and made again. A
/// new default is granted a private `~/sandbox` writable, its record
/// written last, so a run cut short is completed by the next; one root
/// already holds is adopted as it is, its grants its own.
fn init_default(home: &Path, intake: &impl Intake) -> io::Result<()> {
    let canonical = fs::canonicalize(home)?;
    let base = state::base(home)?;
    import::once(&base, &canonical, intake)?;
    let profiles = state::profiles(&base)?;
    if state::Profile::read(&profiles, "default").is_ok() {
        return Ok(());
    }
    if let Some(broken) = state::removal(&profiles, "default")? {
        broken.remove()?;
    }
    let shared = home.join("sandbox");
    td_fs::private_dir(&shared)?;
    if fs::canonicalize(&shared)? != canonical.join("sandbox") {
        return Err(failure(format_args!(
            "{} must be a private directory of your home, not reached through a link",
            shared.display()
        )));
    }
    if let Created::New(_) = ask_create("default", intake)? {
        intake
            .ask(&intake::grant("default", true, "sandbox")?)?
            .accepted()?;
    }
    drop(state::Profile::create(&profiles, "default", None, None)?);
    Ok(())
}

/// One sandbox's grants, as root lists them.
fn grants(intake: &impl Intake, name: &str) -> io::Result<intake::Grants> {
    intake
        .ask(&intake::simple("grants", name)?)
        .and_then(intake::Answer::accepted)
        .and_then(|rest| intake::grants(&rest))
}

fn network_text(host: bool) -> &'static str {
    if host {
        "host (includes localhost and LAN)"
    } else {
        "off"
    }
}

fn inspect(profile: &state::Profile, grants: &intake::Grants) -> io::Result<String> {
    let mut shown = text(format_args!(
        "Account: {}{}\nHome: {}\nNetwork: {}\nRuntime: {}\n",
        workload_accounts::WORKLOAD_ACCOUNT_PREFIX,
        grants.uid,
        workload_accounts::Kind::Sandbox.home(grants.uid).display(),
        network_text(grants.host),
        profile.runtime.as_deref().unwrap_or("native")
    ))?;
    for (path, write) in &grants.directories {
        let line = text(format_args!(
            "{}: ~/{path}\n",
            if *write { "Write" } else { "Read" }
        ))?;
        td_alloc::push_str(&mut shown, &line).map_err(refused)?;
    }
    if let Some(old) = &profile.old_home {
        let line = text(format_args!(
            "Old home: {} (from before sandbox accounts; left as it was)\n",
            old.display()
        ))?;
        td_alloc::push_str(&mut shown, &line).map_err(refused)?;
    }
    Ok(shown)
}

/// Runs a session: the record locked for its length, a stop socket beside
/// it, root's answer, then the relay over the same connection.
fn session(profile: &state::Profile, name: &str, line: &str, terminal: bool) -> io::Result<u8> {
    let control = profile.root.join("control");
    if fs::symlink_metadata(&control).is_ok() {
        fs::remove_file(&control)?;
    }
    let directory = fs::File::open(&profile.root)?;
    let stop = UnixListener::bind(text(format_args!(
        "/proc/self/fd/{}/control",
        directory.as_raw_fd()
    ))?)?;
    stop.set_nonblocking(true)?;
    let result = intake::Root::new()
        .launch(line)
        .map_err(|e| failure(format_args!("sandbox {name}: {}", explain(name, e))))
        .and_then(|channel| relay::outside(channel, &stop, terminal));
    drop(stop);
    let _ = fs::remove_file(&control);
    result
}

fn run() -> io::Result<u8> {
    let mut args = std::env::args_os().skip(1).peekable();
    if args.peek().is_some_and(|v| v == "--inside") {
        args.next();
        return relay::inside(args.collect());
    }
    let command = word(args.next())?;
    if matches!(command.as_str(), "help" | "--help" | "-h") {
        print!("{HELP}");
        return Ok(0);
    }
    let (home, canonical) = homes()?;
    let root = intake::Root::new();
    if command == "init-default" {
        end(&mut args)?;
        let patient = intake::Root {
            patience: BOOT_PATIENCE,
            ..intake::Root::new()
        };
        init_default(&home, &patient)?;
        return Ok(0);
    }
    let base = state::base(&home)?;
    let profiles = state::profiles(&base)?;
    if command == "list" {
        end(&mut args)?;
        for name in state::list(&profiles)? {
            match grants(&root, &name) {
                Ok(grants) => println!(
                    "{name}\t{}{}\tnetwork {}",
                    workload_accounts::WORKLOAD_ACCOUNT_PREFIX,
                    grants.uid,
                    if grants.host { "host" } else { "off" }
                ),
                Err(e) => println!("{name}\t-\t{e}"),
            }
        }
        return Ok(0);
    }
    let name = word(args.next())?;
    state::name(&name)?;
    match command.as_str() {
        "stop" => {
            end(&mut args)?;
            let profile = state::Profile::read(&profiles, &name)?;
            let directory = fs::File::open(&profile.root)?;
            let _ = UnixStream::connect(text(format_args!(
                "/proc/self/fd/{}/control",
                directory.as_raw_fd()
            ))?)?;
        }
        "create" => {
            end(&mut args)?;
            let (_, uid) = create(&profiles, &name, &root)?;
            println!(
                "Created {name} as {}{uid}; network off",
                workload_accounts::WORKLOAD_ACCOUNT_PREFIX
            );
        }
        "adopt" => {
            end(&mut args)?;
            let uid = adopt(&profiles, &name, &root)?;
            println!(
                "Adopted {name} as {}{uid}",
                workload_accounts::WORKLOAD_ACCOUNT_PREFIX
            );
        }
        "inspect" => {
            end(&mut args)?;
            let profile = state::Profile::read(&profiles, &name)?;
            print!("{}", inspect(&profile, &grants(&root, &name)?)?);
        }
        "network" => {
            let host = match word(args.next())?.as_str() {
                "host" => true,
                "off" => false,
                _ => return Err(error(HELP)),
            };
            end(&mut args)?;
            state::Profile::read(&profiles, &name)?;
            root.ask(&intake::network(&name, host)?)?
                .accepted()
                .map_err(|e| explain(&name, e))?;
        }
        "runtime" => {
            let choice = word(args.next())?;
            end(&mut args)?;
            let mut profile = state::Profile::open(&profiles, &name)?;
            profile.runtime = match choice.as_str() {
                "native" => None,
                runtime => Some(owned(state::runtime(runtime)?)?),
            };
            profile.save()?;
        }
        "share" => {
            let path = PathBuf::from(args.next().ok_or_else(|| error(HELP))?);
            let write = match word(args.next())?.as_str() {
                "--read" => false,
                "--write" => true,
                _ => return Err(error(HELP)),
            };
            end(&mut args)?;
            state::Profile::read(&profiles, &name)?;
            let relative = grant_path(&canonical, &path)?;
            root.ask(&intake::grant(&name, write, &relative)?)?
                .accepted()
                .map_err(|e| explain(&name, e))?;
        }
        "unshare" => {
            let path = PathBuf::from(args.next().ok_or_else(|| error(HELP))?);
            end(&mut args)?;
            state::Profile::read(&profiles, &name)?;
            let relative = unshare_path(&canonical, &home, &path)?;
            match root
                .ask(&intake::revoke(&name, &relative)?)?
                .accepted()
                .map_err(|e| explain(&name, e))?
                .as_str()
            {
                "revoked\n" => {}
                _ => return Err(error("folder was not shared")),
            }
        }
        "delete" => {
            end(&mut args)?;
            // Readable or not, or absent, as for a sandbox root holds
            // whose record was lost: root deletes it all the same.
            let record = state::removal(&profiles, &name)?;
            root.ask(&intake::simple("delete", &name)?)?.accepted()?;
            if let Some(record) = record {
                record.remove()?;
            }
        }
        "shell" | "run" => {
            let terminal = relay::is_terminal();
            let profile = state::Profile::open(&profiles, &name)?;
            let runtime = profile.runtime.as_deref();
            let term = if terminal {
                std::env::var("TERM").unwrap_or_else(|_| "xterm-256color".into())
            } else {
                "dumb".into()
            };
            let line = if command == "shell" {
                if args.next().is_some() || !terminal {
                    return Err(error(
                        "shell requires an interactive terminal; use run for pipes",
                    ));
                }
                intake::start(&name, runtime, &term)?
            } else {
                if args.next().as_deref() != Some(std::ffi::OsStr::new("--")) {
                    return Err(error(HELP));
                }
                let argv = args
                    .map(|argument| {
                        argument
                            .into_string()
                            .map_err(|_| error("a command's arguments must be UTF-8"))
                    })
                    .collect::<io::Result<Vec<_>>>()?;
                if argv.is_empty() {
                    return Err(error(HELP));
                }
                intake::run(&name, runtime, terminal, &term, &argv)?
            };
            return session(&profile, &name, &line, terminal);
        }
        _ => return Err(error(HELP)),
    }
    Ok(0)
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            let _ = writeln!(io::stderr(), "td-sandbox: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// Root as the commands see it: each request recorded, its answer
    /// chosen by its verb.
    #[derive(Default)]
    struct Fake {
        asked: RefCell<Vec<String>>,
        refuse: Option<&'static str>,
        /// Root's state of every name: held when set.
        held: bool,
    }

    impl Intake for Fake {
        fn ask(&self, line: &str) -> io::Result<intake::Answer> {
            self.asked.borrow_mut().push(line.to_string());
            if self.refuse.is_some_and(|verb| line.starts_with(verb)) {
                return Ok(intake::Answer::Refused("refused here".into()));
            }
            if self.held && line.starts_with("create ") {
                return Ok(intake::Answer::Refused(intake::EXISTS.into()));
            }
            Ok(intake::Answer::Ok(if line.starts_with("create ") {
                "98304\n".into()
            } else if line.starts_with("status ") {
                if self.held {
                    "held 98304\n"
                } else {
                    "free 98304\n"
                }
                .into()
            } else {
                "added\n".into()
            }))
        }
    }

    /// init-default imports first, then creates default through root with
    /// a private ~/sandbox shared writable; a second run asks nothing.
    #[test]
    fn init_default_imports_then_seeds_default_through_root() -> io::Result<()> {
        let home = td_fs::TempDir::new("sandbox-init-default")?;
        let fake = Fake::default();
        init_default(&home, &fake)?;
        assert_eq!(
            *fake.asked.borrow(),
            ["create default\n", "grant default rw sandbox\n"]
        );
        let base = state::base(&home)?;
        assert!(base.join(import::MARKER).exists());
        let profiles = state::profiles(&base)?;
        assert!(state::exists(&profiles, "default")?);
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(home.join("sandbox"))?.permissions().mode() & 0o777,
            0o700
        );
        let again = Fake::default();
        init_default(&home, &again)?;
        assert!(again.asked.borrow().is_empty());
        Ok(())
    }

    /// A seed whose share root refuses writes no record, so the next run
    /// tries again, and that run's creation of the sandbox the first left
    /// free completes it; a creation root refuses leaves no record.
    #[test]
    fn a_seed_cut_short_is_completed_and_a_refused_creation_leaves_no_record() -> io::Result<()> {
        let home = td_fs::TempDir::new("sandbox-init-refused")?;
        let fake = Fake {
            refuse: Some("grant "),
            ..Fake::default()
        };
        assert!(init_default(&home, &fake).is_err());
        assert_eq!(
            *fake.asked.borrow(),
            ["create default\n", "grant default rw sandbox\n"]
        );
        let profiles = state::profiles(&state::base(&home)?)?;
        assert!(!state::exists(&profiles, "default")?);
        let again = Fake::default();
        init_default(&home, &again)?;
        assert_eq!(
            *again.asked.borrow(),
            ["create default\n", "grant default rw sandbox\n"]
        );
        assert!(state::exists(&profiles, "default")?);
        let refused = Fake {
            refuse: Some("create "),
            ..Fake::default()
        };
        assert!(create(&profiles, "tools", &refused).is_err());
        assert!(!state::exists(&profiles, "tools")?);
        let (_, uid) = create(&profiles, "tools", &Fake::default())?;
        assert_eq!(uid, 98304);
        assert!(state::exists(&profiles, "tools")?);
        Ok(())
    }

    /// A sandbox root already holds is never created over: `create`
    /// leaves no record and says to adopt or delete; `adopt` records it,
    /// and only a held one; init-default adopts a held default as it is,
    /// granting nothing, and takes an unreadable record of it as none.
    #[test]
    fn a_held_sandbox_is_adopted_never_created_over() -> io::Result<()> {
        let home = td_fs::TempDir::new("sandbox-held")?;
        let profiles = state::profiles(&state::base(&home)?)?;
        let held = Fake {
            held: true,
            ..Fake::default()
        };
        let refused = create(&profiles, "tools", &held).err();
        let refused = refused.map(|e| e.to_string()).unwrap_or_default();
        assert!(refused.contains("td-sandbox adopt tools"), "{refused}");
        assert!(!state::exists(&profiles, "tools")?);
        assert_eq!(adopt(&profiles, "tools", &held)?, 98304);
        assert!(state::Profile::read(&profiles, "tools").is_ok());
        assert!(adopt(&profiles, "tools", &held).is_err());
        assert!(adopt(&profiles, "other", &Fake::default()).is_err());
        assert!(!state::exists(&profiles, "other")?);
        init_default(&home, &held)?;
        assert_eq!(
            *held.asked.borrow(),
            [
                "create tools\n",
                "status tools\n",
                "status tools\n",
                "create default\n"
            ]
        );
        assert!(state::Profile::read(&profiles, "default").is_ok());
        // An unreadable record of default is none: made again.
        fs::remove_file(profiles.join("default/record"))?;
        let again = Fake {
            held: true,
            ..Fake::default()
        };
        init_default(&home, &again)?;
        assert_eq!(*again.asked.borrow(), ["create default\n"]);
        assert!(state::Profile::read(&profiles, "default").is_ok());
        Ok(())
    }

    /// init-default shares only a private directory of the home reached
    /// as itself: a link at ~/sandbox, to a directory in the home or out
    /// of it, is refused before root is asked anything.
    #[test]
    fn init_default_refuses_a_linked_sandbox_directory() -> io::Result<()> {
        let home = td_fs::TempDir::new("sandbox-init-link")?;
        let elsewhere = td_fs::TempDir::new("sandbox-init-elsewhere")?;
        std::os::unix::fs::symlink(elsewhere.path(), home.join("sandbox"))?;
        let fake = Fake::default();
        assert!(init_default(&home, &fake).is_err());
        assert!(fake.asked.borrow().is_empty());
        fs::remove_file(home.join("sandbox"))?;
        fs::create_dir(home.join("inner"))?;
        std::os::unix::fs::symlink(home.join("inner"), home.join("sandbox"))?;
        assert!(init_default(&home, &fake).is_err());
        assert!(fake.asked.borrow().is_empty());
        Ok(())
    }

    /// Only a directory below the home is shared, by its home-relative
    /// path; one to unshare may be gone, or given as `inspect` shows it.
    #[test]
    fn shares_are_home_relative() -> io::Result<()> {
        let home = td_fs::TempDir::new("sandbox-share")?;
        let canonical = fs::canonicalize(home.path())?;
        fs::create_dir_all(canonical.join("src/my project"))?;
        assert_eq!(
            grant_path(&canonical, &canonical.join("src/my project"))?,
            "src/my project"
        );
        assert!(grant_path(&canonical, &canonical).is_err());
        assert!(grant_path(&canonical, Path::new("/")).is_err());
        assert!(grant_path(&canonical, &canonical.join("absent")).is_err());
        assert_eq!(
            unshare_path(&canonical, home.path(), &canonical.join("gone"))?,
            "gone"
        );
        assert_eq!(
            unshare_path(&canonical, home.path(), Path::new("~/src/my project"))?,
            "src/my project"
        );
        assert!(unshare_path(&canonical, home.path(), Path::new("/srv")).is_err());
        Ok(())
    }

    #[test]
    fn inspect_names_the_account_its_grants_and_the_old_home() -> io::Result<()> {
        let base = td_fs::TempDir::new("sandbox-inspect")?;
        let profiles = state::profiles(&base)?;
        drop(state::Profile::create(
            &profiles,
            "default",
            None,
            Some(PathBuf::from("/h/.local/share/td-sandbox/default/home")),
        )?);
        let profile = state::Profile::read(&profiles, "default")?;
        let grants = intake::grants("98304 off 2\nro\tMy Pictures\nrw\tsandbox\n")?;
        assert_eq!(
            inspect(&profile, &grants)?,
            "Account: tdw98304\nHome: /var/lib/td/workloads/98304\nNetwork: off\n\
             Runtime: native\nRead: ~/My Pictures\nWrite: ~/sandbox\n\
             Old home: /h/.local/share/td-sandbox/default/home (from before sandbox \
             accounts; left as it was)\n"
        );
        Ok(())
    }

    /// td-sandbox runs nothing of its own outside the jail: every session
    /// goes through root's intake, and only the relay inside the jail
    /// starts a program.
    #[test]
    fn td_sandbox_never_launches_the_jail_itself() {
        for (file, source) in [
            ("main.rs", include_str!("main.rs")),
            ("relay.rs", include_str!("relay.rs")),
            ("intake.rs", include_str!("intake.rs")),
            ("import.rs", include_str!("import.rs")),
            ("state.rs", include_str!("state.rs")),
        ] {
            let production = source.split("#[cfg(test)]").next().unwrap_or_default();
            for absent in [
                "td-jail",
                "--workspace",
                "launch-spec",
                "host_path",
                "allocate-workload",
                "/var/lib/td/grants.tsv",
            ] {
                assert!(!production.contains(absent), "{file}: {absent}");
            }
            let spawns = production.matches("Command::new(").count();
            assert_eq!(
                spawns,
                usize::from(file == "relay.rs"),
                "{file} spawns a program"
            );
        }
        let relay = include_str!("relay.rs");
        let inside = relay.split("pub fn inside(").nth(1).unwrap_or_default();
        assert!(inside.contains("Command::new(program)"));
        assert!(inside.contains("\"/opt/workspace/bin/td-sandbox\""));
    }
}

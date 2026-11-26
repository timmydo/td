//! Named development sandboxes, each run by root's workspace launch intake
//! as a ledger account of its own (td-sandbox/DESIGN.md). td-sandbox keeps
//! the human's record of each and relays its terminal; root keeps its
//! account, grants and network, and confinement belongs to the jail root
//! starts.
#![forbid(unsafe_code)]
pub mod client;
#[allow(
    dead_code,
    reason = "the runtime declarations td-app and the jail share"
)]
#[path = "../../engine/src/flathub_app.rs"]
mod flathub_app;
mod import;
mod intake;
mod relay;
mod sessions;
use td_alloc as allocation;
#[path = "../../engine/src/sandbox_layout.rs"]
mod sandbox_layout;
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
use std::io::{self, IsTerminal, Write};
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
  runtimes                    list supported runtimes to install in td-software\n\
  inspect NAME                show account, home, network, runtime and grants\n\
  share NAME PATH --read|--write [--at DEST]\n\
                              mount in the sandbox home, or at absolute DEST\n\
  unshare NAME PATH\n\
  home NAME PATH|private      place the private home at your home path or its account path\n\
  network NAME off|host\n\
  device NAME kvm on|off      allow KVM in new sessions\n\
  runtime NAME RUNTIME|native-glibc|native\n\
                              bind a declared Flathub runtime at /usr, or\n\
                              td's glibc at /lib and /lib64 for programs\n\
                              built for another glibc system\n\
  shell NAME                  start an interactive shell\n\
  run NAME -- COMMAND [ARG...] run a command in the sandbox\n\
  sessions NAME               list active CLI session IDs\n\
  stop NAME [ID]              stop one session (ID required when several run)\n\
  delete NAME                 delete an inactive sandbox, its grants and\n\
                              its home's contents; its account is kept\n\
  Sandboxes run on td. Grant changes and launches use root's workspace intake.\n";

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

fn validate_mounts(
    added: Option<&str>,
    layout: &sandbox_layout::Layout,
    grants: &intake::Grants,
    human: &Path,
    personal: bool,
) -> io::Result<()> {
    let account_home = workload_accounts::Kind::Sandbox.home(grants.uid);
    let home = if personal { human } else { &account_home };
    let mut targets = Vec::new();
    for path in grants
        .directories
        .iter()
        .map(|(p, _)| p.as_str())
        .filter(|p| Some(*p) != added)
        .chain(added)
    {
        let (target, original) = layout.targets(path, home, human)?;
        for mount in std::iter::once(target).chain(original) {
            if targets
                .iter()
                .any(|prior: &PathBuf| sandbox_layout::overlaps(prior, &mount))
            {
                return Err(failure(format_args!(
                    "sandbox mount destination {} overlaps another mount",
                    mount.display()
                )));
            }
            td_alloc::push(&mut targets, mount).map_err(refused)?;
        }
    }
    Ok(())
}

fn inspect(
    profile: &state::Profile,
    layout: &sandbox_layout::Layout,
    grants: &intake::Grants,
    human: &Path,
) -> io::Result<String> {
    let account_home = workload_accounts::Kind::Sandbox.home(grants.uid);
    let home = if profile.personal_home_path {
        human
    } else {
        &account_home
    };
    let mut shown = text(format_args!(
        "Account: {}{}\nHome: {}\nNetwork: {}\nRuntime: {}\n",
        workload_accounts::WORKLOAD_ACCOUNT_PREFIX,
        grants.uid,
        home.display(),
        network_text(grants.host),
        profile.runtime.as_deref().unwrap_or("native")
    ))?;
    for (path, write) in &grants.directories {
        let line = text(format_args!(
            "{}: ~/{path}\n",
            if *write { "Write" } else { "Read" }
        ))?;
        if let Some(target) = layout.get(path) {
            let prefix = if target.starts_with('/') { "" } else { "~/" };
            let mapped = text(format_args!(
                "{} -> {prefix}{target}\n",
                line.trim_end_matches('\n')
            ))?;
            td_alloc::push_str(&mut shown, &mapped).map_err(refused)?;
            continue;
        }
        td_alloc::push_str(&mut shown, &line).map_err(refused)?;
    }
    if profile.kvm()? {
        td_alloc::push_str(&mut shown, "KVM: on\n").map_err(refused)?;
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

/// Each CLI launch holds its own stop marker and intake connection.
fn session(
    profile: &state::Profile,
    name: &str,
    line: &str,
    terminal: bool,
    shell: bool,
) -> io::Result<u8> {
    let session = sessions::Session::register(profile)?;
    let (guard, layout) = profile.layout()?;
    let channel = intake::Root::new()
        .launch(&intake::launch_request(line, &layout, profile.kvm()?)?)
        .map_err(|e| failure(format_args!("sandbox {name}: {}", explain(name, e))))?;
    let banner = if shell {
        // The terminal relay disables output newline translation.
        let eol = if io::stderr().is_terminal() {
            "\r\n"
        } else {
            "\n"
        };
        Some(text(format_args!(
            "Sandbox {name}, session {:016x}{eol}",
            session.id
        ))?)
    } else {
        None
    };
    relay::outside(channel, &session.marker, terminal, banner, guard)
}

/// The compiled choices, not a claim that their payloads are installed.
fn runtime_catalog(output: &mut impl Write) -> io::Result<()> {
    writeln!(output, "NAME\tSOURCE\nnative\ttd (no download)")?;
    writeln!(
        output,
        "{}\ttd, with td's glibc at /lib and /lib64 (no download)",
        flathub_app::NATIVE_GLIBC
    )?;
    for runtime in flathub_app::FLATHUB_RUNTIMES {
        writeln!(output, "{}\t{}", runtime.name, runtime.exact_ref)?;
    }
    writeln!(
        output,
        "Install Flathub runtimes in td-software before using them (td only)."
    )
}

fn launcher_ids(
    home: &Path,
    ledger: impl FnOnce() -> Result<workload_accounts::Ledger, String>,
    out: &mut impl Write,
) -> io::Result<()> {
    let profiles = PathBuf::from(text(format_args!(
        "{}/.local/share/td-sandbox/.profiles",
        state::text(home)?
    ))?);
    let base = profiles.parent().ok_or(io::ErrorKind::InvalidInput)?;
    for directory in [base, profiles.as_path()] {
        match td_fs::check_private_dir(directory) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            result => result?,
        }
    }
    let ledger = ledger().map_err(io::Error::other)?;
    for name in state::list(&profiles)? {
        if let Some(uid) = ledger.uid_of(workload_accounts::Kind::Sandbox, &name) {
            if state::Profile::read(&profiles, &name).is_ok() {
                let offset = u16::try_from(uid - workload_accounts::WORKLOAD_UIDS.start())
                    .map_err(io::Error::other)?;
                out.write_all(&offset.to_be_bytes())?;
            }
        }
    }
    out.flush()
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
    if command == "runtimes" {
        end(&mut args)?;
        runtime_catalog(&mut io::stdout().lock())?;
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
    if command == "launcher-ids" {
        end(&mut args)?;
        launcher_ids(
            &canonical,
            workload_accounts::load,
            &mut io::stdout().lock(),
        )?;
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
        "sessions" => {
            end(&mut args)?;
            let profile = state::Profile::open(&profiles, &name, state::Access::Session)?;
            let ids = profile.sessions()?.active()?;
            let mut out = io::stdout().lock();
            for id in ids {
                writeln!(out, "{id:016x}")?;
            }
        }
        "stop" => {
            let id = args.next().map(|value| word(Some(value))).transpose()?;
            end(&mut args)?;
            let profile = state::Profile::open(&profiles, &name, state::Access::Session)?;
            profile.sessions()?.stop(&name, id.as_deref())?;
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
            let (_guard, layout) = profile.layout()?;
            print!(
                "{}",
                inspect(&profile, &layout, &grants(&root, &name)?, &canonical)?
            );
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
        "device" => {
            if word(args.next())? != "kvm" {
                return Err(error(HELP));
            }
            let enabled = match word(args.next())?.as_str() {
                "on" => true,
                "off" => false,
                _ => return Err(error(HELP)),
            };
            end(&mut args)?;
            let profile = state::Profile::open(&profiles, &name, state::Access::Session)?;
            profile.set_kvm(enabled)?;
        }
        "home" => {
            let choice = word(args.next())?;
            end(&mut args)?;
            let personal = match choice.as_str() {
                "private" => false,
                path if Path::new(path) == canonical || Path::new(path) == home => true,
                _ => {
                    return Err(failure(format_args!(
                        "choose {} or private",
                        canonical.display()
                    )))
                }
            };
            let mut profile = state::Profile::open(&profiles, &name, state::Access::Change)?;
            let (_lock, layout) = profile.layout()?;
            if layout.mounts().next().is_some() {
                validate_mounts(None, &layout, &grants(&root, &name)?, &canonical, personal)?;
            }
            profile.personal_home_path = personal;
            profile.save()?;
        }
        "runtime" => {
            let choice = word(args.next())?;
            end(&mut args)?;
            let mut profile = state::Profile::open(&profiles, &name, state::Access::Change)?;
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
            let target = match args.next() {
                None => None,
                Some(flag) if flag == "--at" => Some(word(args.next())?),
                _ => return Err(error(HELP)),
            };
            end(&mut args)?;
            let profile = state::Profile::open(&profiles, &name, state::Access::Session)?;
            let relative = grant_path(&canonical, &path)?;
            let target = target
                .as_deref()
                .map(sandbox_layout::destination)
                .transpose()?;
            profile
                .change_mount(
                    &relative,
                    |layout| {
                        let current = grants(&root, &name)?;
                        layout.retain(|src| {
                            src != relative
                                && current.directories.iter().any(|(path, _)| path == src)
                        });
                        if let Some(target) = target {
                            layout.add(&relative, target)?;
                        }

                        validate_mounts(
                            Some(&relative),
                            layout,
                            &current,
                            &canonical,
                            profile.personal_home_path,
                        )
                    },
                    |_| root.ask(&intake::grant(&name, write, &relative)?),
                )
                .map_err(|e| explain(&name, e))?;
        }
        "unshare" => {
            let path = PathBuf::from(args.next().ok_or_else(|| error(HELP))?);
            end(&mut args)?;
            let profile = state::Profile::open(&profiles, &name, state::Access::Session)?;
            let relative = unshare_path(&canonical, &home, &path)?;
            profile
                .change_mount(
                    &relative,
                    |layout| {
                        layout.retain(|src| src != relative);
                        Ok(())
                    },
                    |retry| {
                        if retry
                            && !grants(&root, &name)?
                                .directories
                                .iter()
                                .any(|(source, _)| source == &relative)
                        {
                            return Ok(intake::Answer::Ok(String::new()));
                        }
                        match root.ask(&intake::revoke(&name, &relative)?)? {
                            intake::Answer::Ok(answer) if answer != "revoked\n" => {
                                Err(error("unexpected revoke reply"))
                            }
                            answer => Ok(answer),
                        }
                    },
                )
                .map_err(|e| explain(&name, e))?;
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
            let profile = state::Profile::open(&profiles, &name, state::Access::Session)?;
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
                intake::start(&name, runtime, &term, profile.personal_home_path)?
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
                intake::run(
                    &name,
                    runtime,
                    terminal,
                    &term,
                    &argv,
                    profile.personal_home_path,
                )?
            };
            return session(&profile, &name, &line, terminal, command == "shell");
        }
        _ => return Err(error(HELP)),
    }
    Ok(0)
}

pub fn main() -> ExitCode {
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

    #[test]
    fn changed_home_and_absolute_mounts_must_leave_separate_destinations() -> io::Result<()> {
        let human = Path::new("/var/home/alice");
        let grants = intake::grants("98304 off 2\nrw\tsrc/td\nro\tbooks\n")?;
        let mut layout = sandbox_layout::Layout::default();
        layout.add("books", "/var/home/alice")?;
        assert!(validate_mounts(None, &layout, &grants, human, true).is_err());
        assert!(sandbox_layout::destination("/oldroot/nested").is_err());
        assert!(sandbox_layout::destination(&"a".repeat(256)).is_err());
        assert!(sandbox_layout::destination("~/~/double").is_err());
        assert!(sandbox_layout::destination("/root-write-probe").is_err());
        Ok(())
    }

    #[test]
    fn launcher_lists_readable_profiles_in_private_directories() -> io::Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let home = td_fs::TempDir::new("launcher-profiles")?;
        let ledger = || {
            workload_accounts::Ledger::parse(
                "td-accounts-v1\nsandbox\tdev\t98310\nsandbox\tgone\t98311\n",
            )
        };
        let mut answer = Vec::new();
        launcher_ids(&home, ledger, &mut answer)?;
        assert!(answer.is_empty());
        let base = state::base(&home)?;
        let profiles = state::profiles(&base)?;
        drop(state::Profile::create(&profiles, "dev", None, None)?);
        launcher_ids(&home, ledger, &mut answer)?;
        assert_eq!(answer, [0, 6]);
        fs::set_permissions(&profiles, fs::Permissions::from_mode(0o755))?;
        assert!(launcher_ids(&home, ledger, &mut Vec::new()).is_err());
        Ok(())
    }

    #[test]
    fn runtimes_lists_the_compiled_choices_without_an_intake_or_home() -> io::Result<()> {
        let mut output = Vec::new();
        runtime_catalog(&mut output)?;
        let listing = String::from_utf8(output).map_err(error)?;
        assert!(listing.contains("native\ttd (no download)\n"));
        assert!(listing.contains("\nnative-glibc\ttd, with td's glibc at /lib and /lib64"));
        assert!(state::runtime("native-glibc").is_ok());
        assert!(state::runtime("native").is_err());
        for runtime in flathub_app::FLATHUB_RUNTIMES {
            assert!(listing.contains(&format!("{}\t{}\n", runtime.name, runtime.exact_ref)));
        }
        assert!(listing.contains("Install Flathub runtimes in td-software"));
        Ok(())
    }

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
            inspect(
                &profile,
                &Default::default(),
                &grants,
                Path::new("/var/home/alice")
            )?,
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
            ("lib.rs", include_str!("lib.rs")),
            ("client.rs", include_str!("client.rs")),
            ("relay.rs", include_str!("relay.rs")),
            ("intake.rs", include_str!("intake.rs")),
            ("import.rs", include_str!("import.rs")),
            ("state.rs", include_str!("state.rs")),
            ("sessions.rs", include_str!("sessions.rs")),
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

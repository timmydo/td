//! Named private-home development environments. Confinement belongs to td-jail.
#![forbid(unsafe_code)]
#[path = "../../engine/src/host_path.rs"]
mod host_path;
mod relay;
mod state;
mod wire;

use std::ffi::OsString;
use std::fs;
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

const HELP: &str = "td-sandbox [--jail PATH] COMMAND\n\
  init-default                seed default with writable ~/sandbox once\n\
  create NAME                 create a private home (network off)\n\
  list                        list environments\n\
  inspect NAME                show home, network and folder grants\n\
  share NAME PATH --read|--write\n\
  unshare NAME PATH\n\
  network NAME off|host\n\
  shell NAME                  start an interactive shell\n\
  run NAME -- COMMAND [ARG...] run a command in the private home\n\
  stop NAME                   terminate the active session\n\
  delete NAME                 delete an inactive environment and its home\n";

fn error(message: impl ToString) -> io::Error {
    io::Error::other(message.to_string())
}
fn word(arg: Option<OsString>) -> io::Result<String> {
    arg.and_then(|v| v.into_string().ok())
        .ok_or_else(|| error(HELP))
}
fn system_path(path: &Path) -> bool {
    [
        "/usr", "/bin", "/sbin", "/lib", "/lib64", "/gnu", "/nix", "/td",
    ]
    .iter()
    .any(|p| path.starts_with(p))
}
fn command_path(name: &str) -> io::Result<PathBuf> {
    td_fs::executable_on_path(name)
        .ok_or_else(|| error(format!("{name} is not on PATH")))
        .and_then(fs::canonicalize)
}

fn run() -> io::Result<u8> {
    let mut args = std::env::args_os().skip(1).peekable();
    if args.peek().is_some_and(|v| v == "--inside") {
        args.next();
        return relay::inside(args.collect());
    }
    let jail = if args.peek().is_some_and(|v| v == "--jail") {
        args.next();
        Some(PathBuf::from(args.next().ok_or_else(|| error(HELP))?))
    } else {
        None
    };
    let command = word(args.next())?;
    if matches!(command.as_str(), "help" | "--help" | "-h") {
        print!("{HELP}");
        return Ok(0);
    }
    let base = state::base()?;
    if command == "init-default" {
        if args.next().is_some() {
            return Err(error(HELP));
        }
        let home = std::env::var_os("HOME").ok_or_else(|| error("HOME is unset"))?;
        state::State::init_default(&base, Path::new(&home))?;
        return Ok(0);
    }
    if command == "list" {
        if args.next().is_some() {
            return Err(error(HELP));
        }
        let mut names = Vec::new();
        for entry in fs::read_dir(base)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                if let Some(n) = entry
                    .file_name()
                    .to_str()
                    .filter(|n| state::name(n).is_ok())
                {
                    names.push(n.to_owned());
                }
            }
        }
        names.sort();
        for name in names {
            println!("{name}");
        }
        return Ok(0);
    }
    let name = word(args.next())?;
    state::name(&name)?;
    if command == "stop" {
        if args.next().is_some() {
            return Err(error(HELP));
        }
        td_fs::check_private_dir(&base.join(&name))?;
        let directory = fs::File::open(base.join(&name))?;
        let _ = UnixStream::connect(format!("/proc/self/fd/{}/control", directory.as_raw_fd()))?;
        return Ok(0);
    }
    if command == "create" {
        if args.next().is_some() {
            return Err(error(HELP));
        }
        state::State::create(&base, &name)?;
        println!("Created {name}; network off");
        return Ok(0);
    }
    let mut state = state::State::open(&base, &name)?;
    match command.as_str() {
        "inspect" if args.peek().is_none() => {
            println!(
                "Home: {}\nNetwork: {}",
                state.home.display(),
                if state.network {
                    "host (includes localhost and LAN)"
                } else {
                    "off"
                }
            );
            for (path, write) in &state.shares {
                println!(
                    "{}: {}",
                    if *write { "Write" } else { "Read" },
                    path.display()
                );
            }
        }
        "network" => {
            state.network = match word(args.next())?.as_str() {
                "host" => true,
                "off" => false,
                _ => return Err(error(HELP)),
            };
            if args.next().is_some() {
                return Err(error(HELP));
            }
            state.save()?;
        }
        "share" => {
            let path = PathBuf::from(args.next().ok_or_else(|| error(HELP))?);
            let write = match word(args.next())?.as_str() {
                "--read" => false,
                "--write" => true,
                _ => return Err(error(HELP)),
            };
            if args.next().is_some() {
                return Err(error(HELP));
            }
            state.share(&base, &path, write)?;
        }
        "unshare" => {
            let supplied = PathBuf::from(args.next().ok_or_else(|| error(HELP))?);
            // A removed grant must remain removable after its directory disappears.
            let path = if state.shares.iter().any(|(p, _)| p == &supplied) {
                supplied
            } else {
                fs::canonicalize(supplied)?
            };
            if args.next().is_some() {
                return Err(error(HELP));
            }
            let before = state.shares.len();
            state.shares.retain(|(p, _)| *p != path);
            if state.shares.len() == before {
                return Err(error("folder was not shared"));
            }
            state.save()?;
        }
        "delete" if args.peek().is_none() => {
            fs::remove_dir_all(&state.root)?;
        }
        "shell" | "run" => {
            let terminal = relay::is_terminal();
            let argv = if command == "shell" {
                if args.next().is_some() || !terminal {
                    return Err(error(
                        "shell requires an interactive terminal; use run for pipes",
                    ));
                }
                let shell = std::env::var_os("SHELL")
                    .and_then(|p| fs::canonicalize(p).ok())
                    .filter(|p| system_path(p))
                    .or_else(|| command_path("sh").ok())
                    .ok_or_else(|| error("no admitted shell"))?;
                vec![shell.into_os_string(), "-i".into()]
            } else {
                if args.next().as_deref() != Some(std::ffi::OsStr::new("--")) {
                    return Err(error(HELP));
                }
                let argv: Vec<_> = args.collect();
                if argv.is_empty() {
                    return Err(error(HELP));
                }
                argv
            };
            return launch(&state, jail, argv, terminal);
        }
        _ => return Err(error(HELP)),
    }
    Ok(0)
}

struct LaunchFiles {
    spec: PathBuf,
    control: PathBuf,
}
impl Drop for LaunchFiles {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.spec);
        let _ = fs::remove_file(&self.control);
    }
}

fn launch(
    state: &state::State,
    jail: Option<PathBuf>,
    argv: Vec<OsString>,
    terminal: bool,
) -> io::Result<u8> {
    let executable = fs::canonicalize(std::env::current_exe()?)?;
    let jail = match jail {
        Some(path) => fs::canonicalize(path)?,
        None => {
            let sibling = executable
                .parent()
                .ok_or_else(|| error("executable has no parent"))?
                .join("td-jail");
            let companion = executable
                .parent()
                .ok_or_else(|| error("executable has no parent"))?
                .join("../lib/td/td-jail");
            if sibling.is_file() {
                sibling
            } else if companion.is_file() {
                fs::canonicalize(companion)?
            } else {
                command_path("td-jail")?
            }
        }
    };
    let control = state.root.join("control");
    if fs::symlink_metadata(&control).is_ok() {
        fs::remove_file(&control)?;
    }
    let directory = fs::File::open(&state.root)?;
    let stop = UnixListener::bind(format!("/proc/self/fd/{}/control", directory.as_raw_fd()))?;
    stop.set_nonblocking(true)?;
    let files = LaunchFiles {
        spec: state.root.join("launch-spec"),
        control,
    };
    let mut spec = format!(
        "format=1\nprofile={}\nentry={}\n",
        if state.network {
            "development"
        } else {
            "development-offline"
        },
        state::text(&executable)?
    );
    let parent_path = std::env::var_os("PATH");
    for alias in host_path::aliases(parent_path.as_deref(), system_path) {
        spec.push_str(&format!(
            "alias={} {}\n",
            alias.name,
            state::text(&alias.source)?
        ));
    }
    let mut paths = Vec::new();
    for path in parent_path
        .as_deref()
        .map(std::env::split_paths)
        .into_iter()
        .flatten()
    {
        if let Ok(path) = fs::canonicalize(path) {
            if system_path(&path) && path.is_dir() && !paths.contains(&path) && paths.len() < 16 {
                state::text(&path)?;
                paths.push(path);
            }
        }
    }
    for path in paths {
        spec.push_str(&format!("path={}\n", state::text(&path)?));
    }
    spec.push_str(&format!("home={}\n", state::text(&state.home)?));
    for write in [false, true] {
        for (path, mode) in &state.shares {
            if *mode == write {
                spec.push_str(&format!(
                    "{}={}\n",
                    if write { "write" } else { "read" },
                    state::text(path)?
                ));
            }
        }
    }
    td_fs::replace(&files.spec, spec.as_bytes(), 0o600)?;
    let (channel, entry) = UnixStream::pair()?;
    let term = if terminal {
        std::env::var_os("TERM").unwrap_or_else(|| "xterm-256color".into())
    } else {
        "dumb".into()
    };
    let mut command = Command::new(jail);
    command
        .args([
            OsString::from("--workspace"),
            std::process::id().to_string().into(),
            files.spec.as_os_str().to_owned(),
            "--inside".into(),
            if terminal {
                "pty".into()
            } else {
                "pipes".into()
            },
            term,
        ])
        .args(argv)
        .env_clear()
        .stdin(Stdio::from(std::os::fd::OwnedFd::from(entry.try_clone()?)))
        .stdout(Stdio::from(std::os::fd::OwnedFd::from(entry)))
        .stderr(Stdio::inherit());
    let mut child = relay::Running(command.spawn()?);
    drop(command);
    let result = relay::outside(channel, &mut child, &stop, terminal);
    drop(child);
    drop(stop);
    drop(files);
    result
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

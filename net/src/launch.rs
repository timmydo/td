// td-launch — run one application with a fetch service of its own, for a
// host that has no session fetch service (APPLICATIONS.md §X.7). td-news,
// td-mail and td-agent hold no network client: on td the jail binds them
// td-fetchd's socket. Run elsewhere, this applet serves one for the launch:
//
//   td-net launch PROGRAM [ARG...]
//
// PROGRAM is a path, never looked up on PATH. The launch makes a runtime
// directory of its own under the session's, `$XDG_RUNTIME_DIR/td-launch/PID`
// (mode 0700), starts this binary's `fetchd` at `td-fetch/socket` there,
// waits for its probe, and then becomes PROGRAM, by exec, with that
// directory as its `XDG_RUNTIME_DIR` and the session's Wayland display made
// absolute. PROGRAM keeps the launch's pid, its terminal and its exit
// status, so a shell's Ctrl-C and wait reach it directly. The session's own
// `td-fetch/socket` is never touched.
//
// The service ends with PROGRAM however PROGRAM ends: it is started with
// `--exit-with-parent PID`, the launch's pid, in a process group of its own
// so a terminal's signals pass it by, and it removes its socket and exits
// once its parent is no longer that pid. What it leaves is an empty
// directory named for a pid no longer running, which the next launch
// sweeps, judging by its own pid namespace.
//
// Invoked by an application's name (`td-news`, `td-mail`, `td-agent`)
// through a link to this binary, it launches the program of that name
// beside the binary the link resolves to, refusing one that is this binary.
// For td-agent it also names the td-jail and td-txt beside it, when
// `./install-apps` put them there, in `TD_AGENT_JAIL` and `TD_AGENT_TXT`:
// its workspace jail and the td-txt its tools run. One that is not there
// is removed from the environment the program inherits, so a variable
// left in the caller's shell never pairs it with another build's. For
// td-agent it serves the egress relay too, td-egressd, at
// `td-egress/socket` in the same runtime directory: the one way its
// workspaces' proxied connections leave (td-agent/DESIGN.md §10).
use std::ffi::OsStr;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// The applications a link to this binary may name: those that fetch
/// through the service.
pub(crate) const LAUNCHED: &[&str] = &["td-agent", "td-mail", "td-news"];

/// The programs an application runs beside it, each found beside this
/// binary and named to the application in an environment variable:
/// td-agent's workspace jail and the td-txt its tools run
/// (td-agent/src/jail.rs). Without them td-agent refuses its tools.
fn companions(name: &str) -> &'static [(&'static str, &'static str)] {
    match name {
        "td-agent" => &[("td-jail", "TD_AGENT_JAIL"), ("td-txt", "TD_AGENT_TXT")],
        _ => &[],
    }
}

/// Whether `name` is given the egress relay beside its fetch service.
fn relays(name: &str) -> bool {
    name == "td-agent"
}

/// Every launched program's companions, each once, in order: what
/// `./install-apps` builds and installs beside this binary.
fn all_companions() -> Vec<&'static str> {
    let mut all: Vec<&'static str> = Vec::new();
    for name in LAUNCHED {
        for (companion, _) in companions(name) {
            if !all.contains(companion) {
                all.push(companion);
            }
        }
    }
    all
}

/// Writes `names` one per line, for `./install-apps`.
fn list(flag: &str, names: &[&str]) -> i32 {
    let mut out = std::io::stdout().lock();
    let written = names
        .iter()
        .try_for_each(|name| writeln!(out, "{name}"))
        .and_then(|()| out.flush());
    match written {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("td-launch: {flag}: {e}");
            1
        }
    }
}

/// How long a freshly started service may take to answer its probe.
const SERVICE_START: Duration = Duration::from_secs(5);
/// How long one probe may wait for the service's reply.
const PROBE_BUDGET: Duration = Duration::from_secs(2);
const PROBE_PACE: Duration = Duration::from_millis(50);
/// This binary as the kernel holds it, which a rebuild or reinstall that
/// replaces the file on disk does not change.
pub(crate) const SELF: &str = "/proc/self/exe";

/// `td-launch PROGRAM [ARG...]`, `args[0]` being the applet's name.
pub fn run(args: &[String]) {
    let code = match args.get(1).map(String::as_str) {
        // The names a link to this binary launches, one per line, for
        // `./install-apps` to link.
        Some("--names") if args.len() == 2 => list("--names", LAUNCHED),
        // The companions those programs are given, for `./install-apps`
        // to build and install beside this binary.
        Some("--companions") if args.len() == 2 => list("--companions", &all_companions()),
        Some(program) if program.contains('/') => {
            let rest = args.get(2..).unwrap_or(&[]);
            say(launch_here(Path::new(program), rest, &[], false, true))
        }
        _ => {
            eprintln!(
                "usage: td-net launch PROGRAM [ARG...]  (PROGRAM a path)
       td-net launch --names
       td-net launch --companions"
            );
            2
        }
    };
    std::process::exit(code);
}

/// The launch an application's link to this binary asks for: `name`
/// beside the binary, with `args` (the link's own arguments).
pub fn run_named(name: &str, args: &[String]) {
    let launched = std::env::current_exe()
        .map_err(|e| format!("cannot find this program: {e}"))
        .and_then(|exe| {
            let program = beside(&exe, name)?;
            let named = companions_beside(&exe, name)?;
            launch_here(&program, args, &named, relays(name), false)
        });
    std::process::exit(say(launched));
}

/// Each of `name`'s companions with the variable it is named in: its path
/// when it is beside `exe`, or `None` when it is not, for the launch to
/// remove the variable and the application to refuse what needs it. One
/// that is `exe` itself by another name, is not a file, or is a link to
/// nothing is refused, by the companion's name.
fn companions_beside(
    exe: &Path,
    name: &str,
) -> Result<Vec<(&'static str, Option<PathBuf>)>, String> {
    let dir = exe
        .parent()
        .ok_or_else(|| format!("{} has no directory", exe.display()))?;
    let launcher = identity(exe)?;
    let mut named = Vec::new();
    for (companion, var) in companions(name) {
        let path = dir.join(companion);
        let found = match std::fs::metadata(&path) {
            Ok(meta) if (meta.dev(), meta.ino()) == launcher => {
                return Err(format!(
                    "{} is this launcher, not {name}'s {companion}",
                    path.display()
                ));
            }
            Ok(meta) if !meta.is_file() => {
                return Err(format!(
                    "{name}'s {companion} {} is not a file",
                    path.display()
                ));
            }
            Ok(_) => Some(path),
            // A link to nothing is a fault to name, not an absence.
            Err(e)
                if e.kind() == std::io::ErrorKind::NotFound
                    && std::fs::symlink_metadata(&path).is_err() =>
            {
                None
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(format!(
                    "{name}'s {companion} {} is a link to nothing",
                    path.display()
                ));
            }
            Err(e) => return Err(format!("{name}'s {companion} {}: {e}", path.display())),
        };
        named.push((*var, found));
    }
    Ok(named)
}

/// A file's device and inode, following links.
fn identity(path: &Path) -> Result<(u64, u64), String> {
    std::fs::metadata(path)
        .map(|meta| (meta.dev(), meta.ino()))
        .map_err(|e| format!("{}: {e}", path.display()))
}

/// A launch that returned did not become its program: say why.
fn say(launched: Result<(), String>) -> i32 {
    match launched {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("td-launch: {e}");
            1
        }
    }
}

/// `name` in the directory of `exe`, which is not `exe` itself, by link
/// or by file: a program that is this binary would launch itself.
fn beside(exe: &Path, name: &str) -> Result<PathBuf, String> {
    let dir = exe
        .parent()
        .ok_or_else(|| format!("{} has no directory", exe.display()))?;
    let program = dir.join(name);
    if identity(&program)? == identity(exe)? {
        return Err(format!(
            "{} is this launcher, not the application",
            program.display()
        ));
    }
    Ok(program)
}

/// The launch from this process's environment. `loud` says where the
/// socket is served, for a launch asked for by path.
fn launch_here(
    program: &Path,
    args: &[String],
    named: &[(&str, Option<PathBuf>)],
    egress: bool,
    loud: bool,
) -> Result<(), String> {
    let (runtime, display) = session(
        nonempty("XDG_RUNTIME_DIR").as_deref(),
        nonempty("WAYLAND_SOCKET").as_deref(),
        nonempty("WAYLAND_DISPLAY").as_deref(),
    )?;
    launch(
        program,
        args,
        named,
        &runtime,
        display.as_deref(),
        egress,
        loud,
    )
}

/// A nonempty environment value, an empty one reading as unset.
fn nonempty(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

/// What the session must provide: `XDG_RUNTIME_DIR`, which the launch's
/// own runtime directory goes under. The display the application is given
/// is the session's, made absolute, since the application's runtime
/// directory is not where the compositor's socket is: a relative
/// `WAYLAND_DISPLAY`, or the toolkit's default `wayland-0` when none is
/// set, is joined to the session's directory; an absolute one is kept; and
/// an inherited `WAYLAND_SOCKET` needs no display at all. A display that
/// is not there is the application's to refuse, by its path, and only
/// when it opens a window.
fn session(
    runtime: Option<&str>,
    socket: Option<&str>,
    display: Option<&str>,
) -> Result<(PathBuf, Option<PathBuf>), String> {
    let runtime = runtime.ok_or_else(|| {
        "XDG_RUNTIME_DIR is not set: the fetch socket and the Wayland display \
         live under it (a session manager such as elogind sets it)"
            .to_string()
    })?;
    let runtime = PathBuf::from(runtime);
    if !runtime.is_absolute() {
        return Err(format!(
            "XDG_RUNTIME_DIR={} is not an absolute path",
            runtime.display()
        ));
    }
    let display = if socket.is_some() {
        None
    } else {
        let display = PathBuf::from(display.unwrap_or("wayland-0"));
        Some(if display.is_absolute() {
            display
        } else {
            runtime.join(display)
        })
    };
    Ok((runtime, display))
}

/// The socket the applications look for under their runtime directory:
/// `td-fetch/socket`, the same path td-jail binds into a jail.
fn socket_under(runtime: &Path) -> PathBuf {
    runtime.join("td-fetch").join("socket")
}

/// The launch's own runtime directory under the session's, mode 0700,
/// named by this process, whose pid its program keeps; those of launches
/// no longer running are swept first, when `/proc` is there to say which
/// are running.
fn private_runtime(runtime: &Path, proc: &Path) -> Result<PathBuf, String> {
    let base = runtime.join("td-launch");
    std::fs::DirBuilder::new()
        .mode(0o700)
        .recursive(true)
        .create(&base)
        .map_err(|e| format!("create {}: {e}", base.display()))?;
    sweep(&base, proc);
    let dir = base.join(std::process::id().to_string());
    if dir.exists() {
        std::fs::remove_dir_all(&dir).map_err(|e| format!("clear {}: {e}", dir.display()))?;
    }
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&dir)
        .map_err(|e| format!("create {}: {e}", dir.display()))?;
    Ok(dir)
}

fn sweep(base: &Path, proc: &Path) {
    if !proc.join("self").is_dir() {
        return;
    }
    let Ok(entries) = std::fs::read_dir(base) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str().filter(|n| n.parse::<u32>().is_ok()) else {
            continue;
        };
        if !proc.join(pid).is_dir() {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// A service this binary serves for a launch: its applet's name and how
/// it is probed.
struct Service {
    applet: &'static str,
    probe: fn(&Path, Duration) -> Result<(), String>,
}

const FETCHD: Service = Service {
    applet: "td-fetchd",
    probe: crate::fetchd::probe_within,
};
const EGRESSD: Service = Service {
    applet: "td-egressd",
    probe: crate::egress::probe_within,
};

/// Starts this binary's `service` at `socket`, to end when this process's
/// pid is no longer its parent, and waits for its probe; the child is
/// killed when it does not answer.
fn start_service(service: &Service, socket: &Path) -> Result<Child, String> {
    let applet = service.applet;
    let mut child = Command::new(SELF)
        .arg0(applet)
        .args(["run", "--exit-with-parent"])
        .arg(std::process::id().to_string())
        .arg("--socket")
        .arg(socket)
        .stdin(Stdio::null())
        .process_group(0)
        .spawn()
        .map_err(|e| format!("cannot start {applet}: {e}"))?;
    let deadline = Instant::now() + SERVICE_START;
    loop {
        let said = match (service.probe)(socket, PROBE_BUDGET) {
            Ok(()) => return Ok(child),
            Err(said) => said,
        };
        let failed = match child.try_wait() {
            Ok(Some(status)) => Some(format!("{applet} exited before serving ({status})")),
            Ok(None) if Instant::now() >= deadline => Some(format!(
                "{applet} did not answer its probe within {} s: {said}",
                SERVICE_START.as_secs()
            )),
            Ok(None) => None,
            Err(e) => Some(format!("waiting for {applet}: {e}")),
        };
        if let Some(failed) = failed {
            let _ = child.kill();
            let _ = child.wait();
            return Err(failed);
        }
        std::thread::sleep(PROBE_PACE);
    }
}

/// Serve the fetch socket, and the egress relay's when `egress` says so,
/// in a runtime directory of this launch's under `runtime` and become
/// `program` with `args`; returns only when it could not, the services
/// stopped and the directory removed.
fn launch(
    program: &Path,
    args: &[String],
    named: &[(&str, Option<PathBuf>)],
    runtime: &Path,
    display: Option<&Path>,
    egress: bool,
    loud: bool,
) -> Result<(), String> {
    let private = private_runtime(runtime, Path::new("/proc"))?;
    let socket = socket_under(&private);
    let mut services = Vec::new();
    let mut wanted = vec![(FETCHD, socket.clone())];
    if egress {
        wanted.push((EGRESSD, private.join("td-egress").join("socket")));
    }
    for (service, at) in &wanted {
        match start_service(service, at) {
            Ok(child) => services.push(child),
            Err(e) => {
                for mut child in services {
                    let _ = child.kill();
                    let _ = child.wait();
                }
                let _ = std::fs::remove_dir_all(&private);
                return Err(e);
            }
        }
    }
    if loud {
        eprintln!(
            "td-launch: serving the fetch socket at {}",
            socket.display()
        );
    }
    let mut command = Command::new(program);
    command.args(args).env("XDG_RUNTIME_DIR", &private);
    for (var, path) in named {
        match path {
            Some(path) => command.env(var, path),
            None => command.env_remove(var),
        };
    }
    if let Some(display) = display {
        command.env("WAYLAND_DISPLAY", display);
    }
    let e = command.exec();
    for mut child in services {
        let _ = child.kill();
        let _ = child.wait();
    }
    let _ = std::fs::remove_dir_all(&private);
    Err(format!("cannot run {}: {e}", program.display()))
}

/// Whether `base`, a link's name, is one of the applications this binary
/// launches.
pub(crate) fn launches(base: &OsStr) -> Option<&'static str> {
    LAUNCHED
        .iter()
        .copied()
        .find(|name| OsStr::new(name) == base)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("td-launch-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_names_are_the_three_fetching_applications() {
        assert_eq!(launches(OsStr::new("td-news")), Some("td-news"));
        assert_eq!(launches(OsStr::new("td-mail")), Some("td-mail"));
        assert_eq!(launches(OsStr::new("td-agent")), Some("td-agent"));
        assert_eq!(launches(OsStr::new("td-editor")), None);
        assert_eq!(launches(OsStr::new("td-net")), None);
    }

    #[test]
    fn the_socket_is_where_the_applications_look() {
        assert_eq!(
            socket_under(Path::new("/run/user/1000/td-launch/7")),
            Path::new("/run/user/1000/td-launch/7/td-fetch/socket")
        );
    }

    #[test]
    fn the_session_is_the_runtime_directory_and_the_display_made_absolute() {
        let (runtime, display) = session(Some("/run/user/1"), None, None).unwrap();
        assert_eq!(runtime, Path::new("/run/user/1"));
        assert_eq!(display.unwrap(), Path::new("/run/user/1/wayland-0"));
        let (_, display) = session(Some("/run/user/1"), None, Some("wayland-5")).unwrap();
        assert_eq!(display.unwrap(), Path::new("/run/user/1/wayland-5"));
        let (_, display) = session(Some("/run/user/1"), None, Some("/x/w")).unwrap();
        assert_eq!(display.unwrap(), Path::new("/x/w"));
        let (_, display) = session(Some("/run/user/1"), Some("3"), Some("w")).unwrap();
        assert!(display.is_none());
        assert!(session(None, None, None)
            .unwrap_err()
            .contains("XDG_RUNTIME_DIR"));
        assert!(session(Some("rel"), None, None)
            .unwrap_err()
            .contains("absolute"));
    }

    #[test]
    fn the_private_runtime_directory_is_this_launchs_and_dead_ones_are_swept() {
        let base = scratch("runtime");
        let proc = base.join("proc");
        std::fs::create_dir_all(proc.join("self")).unwrap();
        std::fs::create_dir_all(proc.join("41")).unwrap();
        let runtime = base.join("run");
        for pid in ["41", "42", "x"] {
            std::fs::create_dir_all(runtime.join("td-launch").join(pid)).unwrap();
        }
        let dir = private_runtime(&runtime, &proc).unwrap();
        assert_eq!(
            dir,
            runtime
                .join("td-launch")
                .join(std::process::id().to_string())
        );
        let mode = std::os::unix::fs::PermissionsExt::mode(
            &std::fs::metadata(&dir).unwrap().permissions(),
        );
        assert_eq!(mode & 0o777, 0o700);
        assert!(
            runtime.join("td-launch/41").is_dir(),
            "a running one is kept"
        );
        assert!(
            !runtime.join("td-launch/42").exists(),
            "a dead one is swept"
        );
        assert!(runtime.join("td-launch/x").is_dir(), "not a pid: kept");
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn the_installer_is_told_every_companion_once() {
        assert_eq!(all_companions(), ["td-jail", "td-txt"]);
    }

    #[test]
    fn the_agent_is_given_the_jail_and_td_txt_beside_it_when_they_are_there() {
        assert_eq!(
            companions("td-agent"),
            [("td-jail", "TD_AGENT_JAIL"), ("td-txt", "TD_AGENT_TXT")]
        );
        assert!(companions("td-news").is_empty());
        assert!(companions("td-mail").is_empty());
        let base = scratch("companions");
        let lib = base.join("lib");
        std::fs::create_dir_all(&lib).unwrap();
        std::fs::write(lib.join("td-net"), b"").unwrap();
        let exe = lib.join("td-net");
        // Absent ones are listed unnamed, for the launch to remove.
        assert_eq!(
            companions_beside(&exe, "td-agent").unwrap(),
            [("TD_AGENT_JAIL", None), ("TD_AGENT_TXT", None)]
        );
        std::fs::write(lib.join("td-jail"), b"jail").unwrap();
        assert_eq!(
            companions_beside(&exe, "td-agent").unwrap(),
            [
                ("TD_AGENT_JAIL", Some(lib.join("td-jail"))),
                ("TD_AGENT_TXT", None)
            ]
        );
        std::fs::write(lib.join("td-txt"), b"txt").unwrap();
        assert_eq!(
            companions_beside(&exe, "td-agent").unwrap(),
            [
                ("TD_AGENT_JAIL", Some(lib.join("td-jail"))),
                ("TD_AGENT_TXT", Some(lib.join("td-txt")))
            ]
        );
        assert!(companions_beside(&exe, "td-news").unwrap().is_empty());
        // A companion that is this binary by another name is refused, by
        // its own name.
        std::fs::remove_file(lib.join("td-txt")).unwrap();
        std::fs::hard_link(&exe, lib.join("td-txt")).unwrap();
        let refused = companions_beside(&exe, "td-agent").unwrap_err();
        assert!(
            refused.contains("is this launcher, not td-agent's td-txt"),
            "{refused}"
        );
        // So are a directory and a link to nothing, by name.
        std::fs::remove_file(lib.join("td-txt")).unwrap();
        std::fs::create_dir(lib.join("td-txt")).unwrap();
        let refused = companions_beside(&exe, "td-agent").unwrap_err();
        assert!(refused.contains("td-agent's td-txt"), "{refused}");
        assert!(refused.contains("is not a file"), "{refused}");
        std::fs::remove_dir(lib.join("td-txt")).unwrap();
        std::os::unix::fs::symlink(lib.join("gone"), lib.join("td-txt")).unwrap();
        let refused = companions_beside(&exe, "td-agent").unwrap_err();
        assert!(
            refused.contains("td-agent's td-txt") && refused.contains("a link to nothing"),
            "{refused}"
        );
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn a_link_launches_the_program_beside_the_binary_and_never_itself() {
        let base = scratch("beside");
        let lib = base.join("lib");
        std::fs::create_dir_all(&lib).unwrap();
        std::fs::write(lib.join("td-net"), b"").unwrap();
        std::fs::write(lib.join("td-news"), b"").unwrap();
        assert_eq!(
            beside(&lib.join("td-net"), "td-news").unwrap(),
            lib.join("td-news")
        );
        assert!(beside(&lib.join("td-net"), "td-mail").is_err(), "missing");
        // A td-mail beside the binary that is a link back to it.
        std::os::unix::fs::symlink(lib.join("td-net"), lib.join("td-mail")).unwrap();
        assert!(beside(&lib.join("td-net"), "td-mail")
            .unwrap_err()
            .contains("this launcher"));
        // Or the same file by another name.
        std::fs::remove_file(lib.join("td-mail")).unwrap();
        std::fs::hard_link(lib.join("td-net"), lib.join("td-mail")).unwrap();
        assert!(beside(&lib.join("td-net"), "td-mail")
            .unwrap_err()
            .contains("this launcher"));
        std::fs::remove_dir_all(&base).unwrap();
    }
}

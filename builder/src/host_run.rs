//! `td-builder host-run NAME [ARG...]` — the checkout's td-news or td-mail on
//! this host, unjailed, with the fetch service it needs served for it. The
//! repository-root `./news` and `./mail` entry scripts exec this.
//!
//! A development fixture, not host mode's jail (APPLICATIONS.md §X.7): the
//! application runs as the caller under the session's Wayland display, and
//! nothing built here enters a build, so the host's own cargo and C compiler
//! serve, with no static or musl requirement, and a missing piece is one
//! named line.
//!
//! The one thing the applications cannot do themselves is fetch: they hold no
//! TLS, resolver or network and ask `td-fetchd` at
//! `$XDG_RUNTIME_DIR/td-fetch/socket`. The launch serves that itself, in a
//! runtime directory of its own under the session's, which the application is
//! given as `XDG_RUNTIME_DIR` with the display's path made absolute: the
//! checkout's td-net multicall is built and its `fetchd` applet started there
//! as a child, and stopped, its directory removed, when the application exits.
//! Two launches side by side are two services, neither the other's to take
//! away; the session's own `td-fetch/socket` is never touched, so a direct
//! `cargo run` still gets the application's named refusal; and each child is
//! armed to die with this process, so a launch that is killed rather than
//! ending leaves no service behind.

use std::io::{self, Read};
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, ExitStatus, Stdio};
use std::time::{Duration, Instant};

/// How long a freshly started fetch service may take to answer its probe.
const SERVICE_START: Duration = Duration::from_secs(5);
/// How long one probe may take before it is killed as unanswered.
const PROBE_BUDGET: Duration = Duration::from_secs(2);
const PROBE_PACE: Duration = Duration::from_millis(100);

/// The application crate a launcher name stands for; the crate is its
/// binary's name too.
fn crate_of(name: &str) -> Option<&'static str> {
    match name {
        "news" => Some("td-news"),
        "mail" => Some("td-mail"),
        _ => None,
    }
}

/// The socket the applications look for under their runtime directory:
/// `td-fetch/socket`, the same path td-jail binds into a jail.
fn socket_under(runtime: &Path) -> PathBuf {
    runtime.join("td-fetch").join("socket")
}

/// A nonempty environment value, an empty one reading as unset.
fn nonempty(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

/// The checkout, which is the working directory: the entry scripts change
/// to it, and a verb run from anywhere else is told so before anything is
/// built.
fn checkout(root: &Path, crate_dir: &str) -> Result<(), String> {
    for dir in ["net", crate_dir] {
        if !root.join(dir).join("Cargo.toml").is_file() {
            return Err(format!(
                "{} is not the td checkout ({dir}/Cargo.toml is not under it): run ./news or \
                 ./mail from the repository root",
                root.display()
            ));
        }
    }
    Ok(())
}

/// What the session must provide: `XDG_RUNTIME_DIR`, which the launch's
/// own runtime directory goes under. The display the application is given
/// is the session's, made absolute, since the application's runtime
/// directory is not where the compositor's socket is: a relative
/// `WAYLAND_DISPLAY`, or the toolkit's default `wayland-0` when none is
/// set, is joined to the session's directory; an absolute one is kept; and
/// an inherited `WAYLAND_SOCKET` needs no display at all. A display that
/// is not there is the application's to refuse, by its path, and only
/// when it opens a window: its `--cli`, `--help` and fetch-and-quit modes
/// open none.
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

/// The launch's own runtime directory under the session's, mode 0700,
/// named by this process: the fetch socket's home, removed when the launch
/// ends. A launch that was killed left its directory, and its service died
/// with it, so directories of launches no longer running are swept first,
/// when `/proc` is there to say which are running; it says so for this
/// process's pid namespace, which is the session's unless a launch runs
/// in a container that shares the runtime directory and not the
/// namespace.
fn private_runtime(runtime: &Path) -> Result<PathBuf, String> {
    let base = runtime.join("td-host-run");
    std::fs::DirBuilder::new()
        .mode(0o700)
        .recursive(true)
        .create(&base)
        .map_err(|e| format!("create {}: {e}", base.display()))?;
    sweep(&base);
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

fn sweep(base: &Path) {
    if !Path::new("/proc/self").is_dir() {
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
        if !Path::new("/proc").join(pid).is_dir() {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// The host's own build tools: cargo and rustc on PATH, the C compiler as
/// the seed provisioning resolves it (`TD_CC_HOME`, else `cc` or `gcc` on
/// PATH), which also links, and the archiver beside it when there is one,
/// since a provided toolchain's `ar` may be the only one.
struct Tools {
    cargo: PathBuf,
    rustc: PathBuf,
    cc: PathBuf,
    ar: Option<PathBuf>,
    linker_var: String,
}

fn tools(root: &Path) -> Result<Tools, String> {
    let penv = crate::stage0::ProvisionEnv::from_env(root);
    let cargo = crate::stage0::find_in_path(&penv.search_path, "cargo")
        .ok_or_else(|| "no cargo on PATH: install Rust (cargo and rustc)".to_string())?;
    let rustc = crate::stage0::find_in_path(&penv.search_path, "rustc")
        .ok_or_else(|| "no rustc on PATH beside cargo".to_string())?;
    let ccpath = match crate::stage0::provision_cc(&penv) {
        Ok(p) => p,
        Err(crate::stage0::ProvisionErr::Unavailable(m))
        | Err(crate::stage0::ProvisionErr::Broken(m)) => return Err(m),
    };
    let under = |names: &[&str]| {
        ccpath.split(':').filter(|d| !d.is_empty()).find_map(|d| {
            names
                .iter()
                .map(|name| Path::new(d).join(name))
                .find(|p| crate::stage0::is_exec(p))
        })
    };
    let cc = under(&["cc", "gcc"])
        .ok_or_else(|| format!("no cc or gcc under the provisioned C toolchain ({ccpath})"))?;
    let ar = under(&["ar"]);
    let triple = crate::stage0::rustc_host_triple(&rustc)?;
    Ok(Tools {
        cargo,
        rustc,
        cc,
        ar,
        linker_var: crate::stage0::target_linker_var(&triple),
    })
}

/// `cargo build --release --locked` for one checkout crate, the C compiler
/// pinned as compiler and linker (a host without `cc` on PATH, Guix among
/// them, has `gcc`) and rustc as found; the binary where cargo reports it,
/// which a configured target or target directory may have moved.
fn build(root: &Path, tools: &Tools, dir: &str, bin: &str) -> Result<PathBuf, String> {
    eprintln!("host-run: building {dir}");
    let mut command = Command::new(&tools.cargo);
    command
        .args([
            "build",
            "--release",
            "--locked",
            "--quiet",
            "--message-format=json-render-diagnostics",
        ])
        .arg("--manifest-path")
        .arg(root.join(dir).join("Cargo.toml"))
        .env("CC", &tools.cc)
        .env("HOST_CC", &tools.cc)
        .env(&tools.linker_var, &tools.cc)
        .env("RUSTC", &tools.rustc)
        .env_remove("CARGO_BUILD_TARGET");
    if let Some(ar) = &tools.ar {
        command.env("AR", ar);
    }
    let output = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .output()
        .map_err(|e| format!("cannot run cargo for {dir}: {e}"))?;
    if !output.status.success() {
        return Err(format!("cargo build for {dir} failed ({})", output.status));
    }
    let path = executable(&String::from_utf8_lossy(&output.stdout), bin)
        .ok_or_else(|| format!("cargo built {dir} but reported no executable named {bin}"))?;
    if !path.is_file() {
        return Err(format!(
            "cargo built {dir} but {} is not there",
            path.display()
        ));
    }
    Ok(path)
}

/// The executable cargo's `compiler-artifact` message for the binary `bin`
/// names, the last when there are several: cargo's own word on where the
/// binary is. Every line is one JSON object; the target's name and the
/// executable are read as JSON strings, the escapes it may carry decoded.
fn executable(messages: &str, bin: &str) -> Option<PathBuf> {
    messages
        .lines()
        .filter(|line| line.contains("\"reason\":\"compiler-artifact\""))
        .filter(|line| json_string_after(line, "\"name\":\"").as_deref() == Some(bin))
        .filter_map(|line| json_string_after(line, "\"executable\":\""))
        .next_back()
        .map(PathBuf::from)
}

/// The JSON string that follows the first `key` in `line`, decoded; none
/// when the key is absent, the string unterminated or an escape not one
/// JSON allows.
fn json_string_after(line: &str, key: &str) -> Option<String> {
    let start = line.find(key)?.checked_add(key.len())?;
    let mut chars = line.get(start..)?.chars();
    let mut out = String::new();
    loop {
        match chars.next()? {
            '"' => return Some(out),
            '\\' => out.push(match chars.next()? {
                '"' => '"',
                '\\' => '\\',
                '/' => '/',
                'b' => '\u{8}',
                'f' => '\u{c}',
                'n' => '\n',
                'r' => '\r',
                't' => '\t',
                'u' => {
                    let hex: String = chars.by_ref().take(4).collect();
                    if hex.len() != 4 {
                        return None;
                    }
                    char::from_u32(u32::from_str_radix(&hex, 16).ok()?)?
                }
                _ => return None,
            }),
            c => out.push(c),
        }
    }
}

/// What a waited-for child said: answered (it ran and succeeded), refused
/// (it ran and failed, or could not be waited for), or nothing yet.
fn answered(waited: io::Result<Option<ExitStatus>>) -> Option<bool> {
    match waited {
        Ok(Some(status)) => Some(status.success()),
        Ok(None) => None,
        Err(_) => Some(false),
    }
}

/// The applet's own probe of `socket`, bounded: a probe that has not
/// answered within its budget is killed and counts as nothing answering.
/// The error is the probe's own words, for the launch to say when the
/// service never answers.
fn probe(td_net: &Path, socket: &Path) -> Result<(), String> {
    let mut child = Command::new(td_net)
        .args(["fetchd", "probe"])
        .arg(socket)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot run the fetch probe: {e}"))?;
    let deadline = Instant::now() + PROBE_BUDGET;
    loop {
        match answered(child.try_wait()) {
            Some(true) => return Ok(()),
            Some(false) => {
                let mut said = String::new();
                if let Some(mut stderr) = child.stderr.take() {
                    let _ = stderr.read_to_string(&mut said);
                }
                let _ = child.wait();
                return Err(said.trim().to_string());
            }
            None => {}
        }
        if Instant::now() >= deadline {
            let _ = crate::sys::kill_child_recorded(
                &mut child,
                "host-run: the fetch probe did not answer",
            );
            let _ = child.wait();
            return Err(format!(
                "the probe did not answer within {} s",
                PROBE_BUDGET.as_secs()
            ));
        }
        std::thread::sleep(PROBE_PACE);
    }
}

/// Starts the fetch service at `socket` and waits for its probe; the child
/// is the caller's to stop, and dies with this process if it is not.
fn start_service(td_net: &Path, socket: &Path) -> Result<Child, String> {
    eprintln!("host-run: serving the fetch socket at {}", socket.display());
    let mut command = Command::new(td_net);
    command
        .args(["fetchd", "run", "--socket"])
        .arg(socket)
        .stdin(Stdio::null());
    crate::sandbox::die_with_parent(&mut command);
    let mut child = command
        .spawn()
        .map_err(|e| format!("cannot start td-fetchd: {e}"))?;
    let deadline = Instant::now() + SERVICE_START;
    loop {
        let said = match probe(td_net, socket) {
            Ok(()) => return Ok(child),
            Err(said) => said,
        };
        match child.try_wait() {
            Ok(Some(status)) => return Err(format!("td-fetchd exited before serving ({status})")),
            Ok(None) => {}
            Err(e) => {
                let _ = crate::sys::kill_child_recorded(
                    &mut child,
                    "host-run: td-fetchd could not be waited for",
                );
                let _ = child.wait();
                return Err(format!("waiting for td-fetchd: {e}"));
            }
        }
        if Instant::now() >= deadline {
            let _ = crate::sys::kill_child_recorded(
                &mut child,
                "host-run: td-fetchd did not answer its probe",
            );
            let _ = child.wait();
            return Err(format!(
                "td-fetchd did not answer its probe within {} s: {said}",
                SERVICE_START.as_secs()
            ));
        }
        std::thread::sleep(PROBE_PACE);
    }
}

fn stop_service(child: &mut Child, reason: &str) {
    let _ = crate::sys::kill_recorded(
        crate::sys::KillTarget::Pid(i64::from(child.id())),
        crate::sys::SIGTERM,
        reason,
    );
    let _ = child.wait();
}

/// The application's exit as a shell reports it: its code, or 128 plus
/// the signal that ended it.
fn exit_code(status: ExitStatus) -> ExitCode {
    match (status.code(), status.signal()) {
        (Some(code), _) => ExitCode::from(code.clamp(0, 255) as u8),
        (None, Some(signal)) => ExitCode::from(128u8.saturating_add(signal.clamp(0, 127) as u8)),
        (None, None) => ExitCode::FAILURE,
    }
}

fn launch(root: &Path, name: &str, args: &[String]) -> Result<ExitCode, String> {
    let crate_dir = crate_of(name).ok_or_else(|| format!("no application named {name}"))?;
    checkout(root, crate_dir)?;
    let (runtime, display) = session(
        nonempty("XDG_RUNTIME_DIR").as_deref(),
        nonempty("WAYLAND_SOCKET").as_deref(),
        nonempty("WAYLAND_DISPLAY").as_deref(),
    )?;
    let tools = tools(root)?;
    let td_net = build(root, &tools, "net", "td-net")?;
    // Built before the service is started: a build that fails has nothing
    // to stop.
    let app = build(root, &tools, crate_dir, crate_dir)?;
    let private = private_runtime(&runtime)?;
    let outcome = start_service(&td_net, &socket_under(&private)).and_then(|mut service| {
        eprintln!("host-run: running {}", app.display());
        let mut command = Command::new(&app);
        command
            .args(args)
            .env("XDG_RUNTIME_DIR", &private)
            .stdin(Stdio::inherit());
        if let Some(display) = &display {
            command.env("WAYLAND_DISPLAY", display);
        }
        crate::sandbox::die_with_parent(&mut command);
        let status = command
            .status()
            .map_err(|e| format!("cannot run {}: {e}", app.display()));
        let reason = if status.is_ok() {
            "host-run: the application exited"
        } else {
            "host-run: the application could not be run"
        };
        stop_service(&mut service, reason);
        status
    });
    let _ = std::fs::remove_dir_all(&private);
    Ok(exit_code(outcome?))
}

/// `args` are the verb's own: the name, then the application's arguments.
pub(crate) fn run(args: &[String]) -> ExitCode {
    let Some(name) = args.first().map(String::as_str) else {
        eprintln!("usage: td-builder host-run news|mail [ARG...]");
        return ExitCode::from(2);
    };
    if crate_of(name).is_none() {
        eprintln!("usage: td-builder host-run news|mail [ARG...]");
        return ExitCode::from(2);
    }
    let root = match std::env::current_dir() {
        Ok(root) => root,
        Err(e) => {
            eprintln!("td-builder: host-run: getcwd: {e}");
            return ExitCode::FAILURE;
        }
    };
    match launch(&root, name, args.get(1..).unwrap_or(&[])) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("td-builder: host-run: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_names_are_the_two_applications() {
        assert_eq!(crate_of("news"), Some("td-news"));
        assert_eq!(crate_of("mail"), Some("td-mail"));
        assert_eq!(crate_of("td-news"), None);
        assert_eq!(crate_of(""), None);
    }

    #[test]
    fn the_socket_is_where_the_applications_look() {
        assert_eq!(
            socket_under(Path::new("/run/user/1000/td-host-run/42")),
            PathBuf::from("/run/user/1000/td-host-run/42/td-fetch/socket")
        );
    }

    #[test]
    fn the_checkout_is_named_when_the_directory_is_not_it() {
        let e = checkout(Path::new("/nonexistent"), "td-news").unwrap_err();
        assert!(e.contains("net/Cargo.toml"), "{e}");
        assert!(e.ends_with("from the repository root"), "{e}");
    }

    #[test]
    fn the_session_is_the_runtime_directory_and_the_display_made_absolute() {
        let e = session(None, None, Some("wayland-1")).unwrap_err();
        assert!(e.starts_with("XDG_RUNTIME_DIR is not set"), "{e}");
        let e = session(Some("run/user/1000"), None, None).unwrap_err();
        assert!(e.contains("not an absolute path"), "{e}");
        let runtime = PathBuf::from("/run/user/1000");
        // A relative display joins the session's directory, the default
        // when none is set; an absolute one is kept; an inherited socket
        // needs none.
        assert_eq!(
            session(Some("/run/user/1000"), None, Some("wayland-1")).unwrap(),
            (runtime.clone(), Some(runtime.join("wayland-1")))
        );
        assert_eq!(
            session(Some("/run/user/1000"), None, None).unwrap(),
            (runtime.clone(), Some(runtime.join("wayland-0")))
        );
        assert_eq!(
            session(Some("/run/user/1000"), None, Some("/tmp/wl")).unwrap(),
            (runtime.clone(), Some(PathBuf::from("/tmp/wl")))
        );
        assert_eq!(
            session(Some("/run/user/1000"), Some("5"), Some("wayland-1")).unwrap(),
            (runtime, None)
        );
    }

    #[test]
    fn the_executable_is_the_named_binarys_from_cargos_report() {
        let report = concat!(
            "{\"reason\":\"compiler-artifact\",\"target\":{\"kind\":[\"lib\"],\"name\":\"td-ui\"},",
            "\"executable\":null}\n",
            "{\"reason\":\"build-script-executed\",\"package_id\":\"x\"}\n",
            "{\"reason\":\"compiler-artifact\",\"target\":{\"kind\":[\"bin\"],\"name\":\"td-net\"},",
            "\"executable\":\"/x/target/release/td-net\"}\n",
            "{\"reason\":\"compiler-artifact\",\"target\":{\"kind\":[\"bin\"],\"name\":\"td-news\"},",
            "\"executable\":\"/x/a \\\"q\\\"\\\\b\\u00e9/td-news\"}\n",
            "{\"reason\":\"build-finished\",\"success\":true}\n",
        );
        assert_eq!(
            executable(report, "td-net"),
            Some(PathBuf::from("/x/target/release/td-net"))
        );
        assert_eq!(
            executable(report, "td-news"),
            Some(PathBuf::from("/x/a \"q\"\\bé/td-news"))
        );
        assert_eq!(executable(report, "td-mail"), None);
        assert_eq!(executable("", "td-net"), None);
        // An unterminated string or an escape JSON has not is no path.
        assert_eq!(
            json_string_after("\"executable\":\"/x/y", "\"executable\":\""),
            None
        );
        assert_eq!(
            json_string_after("\"executable\":\"/x\\q\"", "\"executable\":\""),
            None
        );
        assert_eq!(
            json_string_after("\"executable\":\"/x\\u12\"", "\"executable\":\""),
            None
        );
    }

    #[test]
    fn only_a_probe_that_ran_and_succeeded_answered() {
        assert_eq!(answered(Err(io::Error::other("cannot wait"))), Some(false));
        // A wait status: the exit code in the high byte.
        assert_eq!(
            answered(Ok(Some(ExitStatus::from_raw(1 << 8)))),
            Some(false)
        );
        assert_eq!(answered(Ok(Some(ExitStatus::from_raw(0)))), Some(true));
        assert_eq!(answered(Ok(None)), None);
    }

    #[test]
    fn a_probe_of_nothing_is_not_served() {
        // A td-net that is not there cannot probe: the answer is "not
        // served", never a panic or a spurious service.
        let e = probe(
            Path::new("/nonexistent/td-net"),
            Path::new("/nonexistent/socket"),
        )
        .unwrap_err();
        assert!(e.starts_with("cannot run the fetch probe"), "{e}");
    }

    #[test]
    fn the_exit_is_the_code_or_128_plus_the_signal() {
        assert_eq!(exit_code(ExitStatus::from_raw(0)), ExitCode::from(0));
        assert_eq!(exit_code(ExitStatus::from_raw(7 << 8)), ExitCode::from(7));
        // Ended by SIGTERM (15) and by SIGKILL (9): the wait status's low
        // bits.
        assert_eq!(exit_code(ExitStatus::from_raw(15)), ExitCode::from(143));
        assert_eq!(exit_code(ExitStatus::from_raw(9)), ExitCode::from(137));
    }

    #[test]
    fn the_private_runtime_directory_is_this_launchs_and_dead_ones_are_swept() {
        let base = std::env::temp_dir().join(format!(
            "td-host-run-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&base).unwrap();
        // A launch that is gone (no such pid) left its directory; a name
        // that is not a pid is not a launch's and stays.
        let dead = base.join("td-host-run").join("4294967295");
        let other = base.join("td-host-run").join("keep");
        std::fs::create_dir_all(dead.join("td-fetch")).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        let dir = private_runtime(&base).unwrap();
        assert_eq!(
            dir,
            base.join("td-host-run")
                .join(std::process::id().to_string())
        );
        assert!(dir.is_dir());
        assert!(!dead.exists(), "the dead launch's directory is swept");
        assert!(other.is_dir());
        let _ = std::fs::remove_dir_all(&base);
    }
}

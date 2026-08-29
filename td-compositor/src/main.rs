#![deny(unsafe_code)]

mod bar;
mod client;
mod client_resources;
mod configure;
mod conn;
mod font;
mod font_data;
mod framebuffer;
mod help;
mod input;
mod keyboard;
mod keys;
mod launcher;
mod layout;
mod pointer;
mod positioner;
mod pty;
mod ready;
mod render;
mod runtime;
mod scene;
mod server;
mod socket;
mod sys;
mod term;
mod term_client;
mod terminfo;
mod ui;
mod wire;

use framebuffer::Framebuffer;
use runtime::Runtime;
use std::env;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process;
use std::sync::{Arc, Mutex};

const MAX_UI_DIMENSION: usize = 16_384;
const MAX_UI_FRAME_BYTES: usize = 32 * 1024 * 1024;
/// The most keys a `wl_keyboard.enter` may report as already held. One
/// spelling for every client, for the reason `verify_keymap` is one function:
/// a bound copied is a bound that stays agreed until exactly one copy moves.
/// td's own server cannot legitimately reach it — `input.rs` caps its pressed
/// set at `MAX_XKB_EVDEV_KEY` — so it bounds what another compositor claims.
const MAX_HELD_KEYS: usize = 256;

fn usage() -> String {
    "usage: td-compositor run --framebuffer PATH --input DIR --socket PATH \
     (--launcher-client PATH | --launcher-application NAME \
     --application-ready-socket PATH --application-app-id ID \
     --application-content-rgb-a RGB --application-content-rgb-b RGB) \
     --terminal-client PATH | \
     td-compositor probe SOCKET | \
     td-compositor probe-application SOCKET ID RGB_A RGB_B [--quiet] | \
     td-compositor terminfo PATH | \
     td-compositor selftest"
        .into()
}

fn client_usage() -> String {
    "usage: td-ui-demo run --socket PATH --ready-socket PATH | \
     td-ui-demo probe READY_SOCKET | td-ui-demo selftest [--shared-network]"
        .into()
}

fn term_usage() -> String {
    "usage: td-term run --socket PATH --ready-socket PATH \
| td-term probe READY_SOCKET | td-term selftest"
        .into()
}

/// Which program this binary was invoked as. Three installed names share one
/// artifact; the fixture keeps the compositor basename, so its authenticated
/// identity and exact `/app` entry jointly select the demo personality.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Personality {
    Compositor,
    Demo,
    Term,
}

impl Personality {
    fn of(executable: &str, jail_fixture: bool) -> Personality {
        if jail_fixture && executable == client::JAIL_FIXTURE_ENTRY {
            return Personality::Demo;
        }
        let name = Path::new(executable)
            .file_name()
            .and_then(|name| name.to_str());
        match name {
            Some("td-ui-demo") => Personality::Demo,
            Some("td-term") => Personality::Term,
            _ => Personality::Compositor,
        }
    }

    fn program(self) -> &'static str {
        match self {
            Personality::Compositor => "td-compositor",
            Personality::Demo => "td-ui-demo",
            Personality::Term => "td-term",
        }
    }
}

/// The terminal's four self-checks and the marker that says all four ran.
/// Where it writes is a parameter so the marker is a tested string rather than
/// one nobody reads.
fn term_selftest(out: &mut impl std::io::Write) -> Result<(), String> {
    term::selftest()?;
    pty::selftest()?;
    ready::selftest()?;
    term_client::selftest()?;
    writeln!(out, "TD-TERM-SELFTEST-OK").map_err(|e| format!("write selftest marker: {e}"))
}

/// The terminal's own entry point. `run` is the Wayland client; the other two
/// need no surface — the readiness probe td-svc calls, and the packaged
/// binary's self-check.
fn run_term(args: &[String]) -> Result<(), String> {
    let command = args.first().ok_or_else(term_usage)?;
    match command.as_str() {
        "run" => {
            let args = args.get(1..).ok_or_else(term_usage)?;
            let (socket, ready_socket) = parse_run_flags(args)?;
            term_client::run(&term_client::Options {
                socket,
                ready_socket,
            })
        }
        "probe" => {
            let socket = args.get(1).ok_or_else(term_usage)?;
            if args.get(2).is_some() {
                return Err(term_usage());
            }
            ready::probe(Path::new(socket))
        }
        "selftest" => {
            if args.get(1).is_some() {
                return Err(term_usage());
            }
            term_selftest(&mut std::io::stdout())
        }
        _ => Err(term_usage()),
    }
}

struct RunOptions {
    framebuffer: PathBuf,
    input: PathBuf,
    socket: PathBuf,
    launcher_client: Option<PathBuf>,
    launcher_application: Option<String>,
    terminal_client: PathBuf,
    application_ready_socket: Option<PathBuf>,
    application_app_id: Option<String>,
    application_content_rgb_a: Option<String>,
    application_content_rgb_b: Option<String>,
}

fn parse_run(args: &[String]) -> Result<RunOptions, String> {
    let mut framebuffer = None;
    let mut input = None;
    let mut socket = None;
    let mut launcher_client = None;
    let mut launcher_application = None;
    let mut terminal_client = None;
    let mut application_ready_socket = None;
    let mut application_app_id = None;
    let mut application_content_rgb_a = None;
    let mut application_content_rgb_b = None;
    let mut index = 0;
    while index < args.len() {
        let flag = args
            .get(index)
            .ok_or_else(|| "missing compositor flag".to_string())?;
        let value = args
            .get(index + 1)
            .ok_or_else(|| format!("{flag} requires a value"))?;
        match flag.as_str() {
            "--framebuffer" if framebuffer.is_none() => framebuffer = Some(PathBuf::from(value)),
            "--input" if input.is_none() => input = Some(PathBuf::from(value)),
            "--socket" if socket.is_none() => socket = Some(PathBuf::from(value)),
            "--launcher-client" if launcher_client.is_none() => {
                launcher_client = Some(PathBuf::from(value))
            }
            "--launcher-application" if launcher_application.is_none() => {
                launcher_application = Some(value.clone())
            }
            "--terminal-client" if terminal_client.is_none() => {
                terminal_client = Some(PathBuf::from(value))
            }
            "--application-ready-socket" if application_ready_socket.is_none() => {
                application_ready_socket = Some(PathBuf::from(value))
            }
            "--application-app-id" if application_app_id.is_none() => {
                application_app_id = Some(value.clone())
            }
            "--application-content-rgb-a" if application_content_rgb_a.is_none() => {
                application_content_rgb_a = Some(value.clone())
            }
            "--application-content-rgb-b" if application_content_rgb_b.is_none() => {
                application_content_rgb_b = Some(value.clone())
            }
            "--framebuffer" | "--input" | "--socket" | "--launcher-client"
            | "--launcher-application" | "--terminal-client"
            | "--application-ready-socket" | "--application-app-id"
            | "--application-content-rgb-a" | "--application-content-rgb-b" => {
                return Err(format!("duplicate flag '{flag}'"));
            }
            _ => return Err(format!("unrecognised argument '{flag}'")),
        }
        index += 2;
    }
    if launcher_client.is_some() == launcher_application.is_some() {
        return Err("exactly one --launcher-client or --launcher-application is required".into());
    }
    if application_ready_socket.is_some() != application_app_id.is_some()
        || application_ready_socket.is_some() != application_content_rgb_a.is_some()
        || application_ready_socket.is_some() != application_content_rgb_b.is_some()
    {
        return Err(
            "--application-ready-socket, --application-app-id, and both \
             --application-content-rgb arguments must be supplied together"
                .into()
        );
    }
    if application_ready_socket.is_some() != launcher_application.is_some() {
        return Err(
            "launcher applications require application-window readiness and vice versa".into(),
        );
    }
    let mut socket = socket.ok_or_else(|| "--socket is required".to_string())?;
    if let Some(ready) = application_ready_socket.as_mut() {
        let resolved_wayland = resolve_socket_endpoint(&socket, "Wayland")?;
        let resolved_ready = resolve_socket_endpoint(ready, "application-window readiness")?;
        if resolved_ready == resolved_wayland {
            return Err("application-window readiness must not alias the Wayland socket".into());
        }
        // Retain the resolved endpoints, not a mutable symlink spelling that
        // could name a different parent between this check and either bind.
        socket = resolved_wayland;
        *ready = resolved_ready;
    }
    Ok(RunOptions {
        framebuffer: framebuffer.ok_or_else(|| "--framebuffer is required".to_string())?,
        input: input.ok_or_else(|| "--input is required".to_string())?,
        socket,
        launcher_client,
        launcher_application,
        // Required rather than defaulted: the compositor cannot know the store
        // path the terminal landed at, and a launcher entry that spawns nothing
        // is worse than one that never appeared.
        terminal_client: terminal_client
            .ok_or_else(|| "--terminal-client is required".to_string())?,
        application_ready_socket,
        application_app_id,
        application_content_rgb_a,
        application_content_rgb_b,
    })
}

fn resolve_socket_endpoint(path: &Path, label: &str) -> Result<PathBuf, String> {
    if !path.is_absolute() {
        return Err(format!("{label} socket path must be absolute"));
    }
    let name = path
        .file_name()
        .ok_or_else(|| format!("{label} socket path has no final name"))?;
    let parent = path
        .parent()
        .ok_or_else(|| format!("{label} socket path has no parent directory"))?;
    let parent = std::fs::canonicalize(parent)
        .map_err(|error| format!("resolve {label} socket parent {}: {error}", parent.display()))?;
    Ok(parent.join(name))
}

fn run_compositor(options: RunOptions) -> Result<(), String> {
    let framebuffer = Framebuffer::open(&options.framebuffer)?;
    let geometry = (framebuffer.width, framebuffer.height, framebuffer.stride);
    let mut runtime = Runtime::new(framebuffer);
    runtime.set_launcher_application(options.launcher_application.as_deref());
    if let Some((((path, app_id), content_rgb_a), content_rgb_b)) = options
        .application_ready_socket
        .as_deref()
        .zip(options.application_app_id.as_deref())
        .zip(options.application_content_rgb_a.as_deref())
        .zip(options.application_content_rgb_b.as_deref())
    {
        let observer = server::watch_application(path, app_id, content_rgb_a, content_rgb_b)?;
        runtime.watch_application_with_cursor(
            app_id.to_string(),
            observer.content_rgbs,
            observer.content_wake,
            observer.cursor_wake,
        )?;
    }
    let runtime = Arc::new(Mutex::new(runtime));
    runtime
        .lock()
        .map_err(|_| "runtime lock poisoned".to_string())?
        .repaint()?;
    let inputs = input::start(
        &options.input,
        Arc::clone(&runtime),
        launcher::LaunchOptions {
            socket: options.socket.clone(),
            client: options.launcher_client,
            terminal: options.terminal_client,
            application: options
                .launcher_application
                .map(|name| launcher::ApplicationLaunch { name }),
        },
    )?;
    // Reported, never fatal: a compositor without a clock is worth more
    // than no compositor.
    if let Err(error) = bar::start(
        Arc::clone(&runtime),
        PathBuf::from("/proc"),
        PathBuf::from("/sys"),
    ) {
        eprintln!("td-compositor: {error}");
    }
    eprintln!(
        "td-compositor: software output {}x{} stride={} inputs={inputs}",
        geometry.0, geometry.1, geometry.2
    );
    server::serve(&options.socket, runtime)
}

fn selftest() -> Result<(), String> {
    term::selftest()?;
    keys::selftest()?;
    pty::selftest()?;
    render::selftest()?;
    terminfo::selftest()?;

    let mut payload = wire::Builder::new();
    payload.u32(7);
    let mut encoded = payload.message(1, 0)?;
    let message =
        wire::take(&mut encoded)?.ok_or_else(|| "wire selftest emitted no message".to_string())?;
    let mut cursor = wire::Cursor::new(&message.payload);
    if message.object != 1 || message.opcode != 0 || cursor.u32()? != 7 {
        return Err("wire selftest did not round-trip".into());
    }
    cursor.finish()?;

    let mut scene = scene::Scene::new();
    scene.commit(
        scene::SurfaceKey {
            client: 1,
            object: 2,
        },
        scene::Surface {
            width: 1,
            height: 1,
            pixels: vec![1, 2, 3, 0],
            format: scene::SHM_XRGB8888,
        },
    )?;
    // Tall enough to have a tiling area beneath the status bar AND a client
    // area beneath the tile's title band: a frame the size of either is all
    // decoration and no client, and this is the check that says so.
    let height = scene::least_output_height(4);
    let mut frame = vec![0; 4 * height * 4];
    scene.render(&mut frame, 4, height, 4 * 4);
    if !frame.as_chunks::<4>().0.contains(&[1, 2, 3, 0]) {
        return Err("renderer selftest did not copy its surface".into());
    }
    let mut out = std::io::stdout().lock();
    writeln!(out, "TD-COMPOSITOR-SELFTEST-OK").map_err(|e| format!("write compositor selftest marker: {e}"))?;
    Ok(())
}

fn run(args: &[String]) -> Result<(), String> {
    let command = args.first().ok_or_else(usage)?;
    match command.as_str() {
        "run" => run_compositor(parse_run(args.get(1..).ok_or_else(usage)?)?),
        "probe" => {
            let socket = args.get(1).ok_or_else(usage)?;
            if args.get(2).is_some() {
                return Err(usage());
            }
            server::probe(Path::new(socket))
        }
        "probe-application" => {
            let socket = args.get(1).ok_or_else(usage)?;
            let app_id = args.get(2).ok_or_else(usage)?;
            let content_rgb_a = args.get(3).ok_or_else(usage)?;
            let content_rgb_b = args.get(4).ok_or_else(usage)?;
            let quiet = match args.get(5).map(String::as_str) {
                None => false,
                Some("--quiet") => true,
                Some(_) => return Err(usage()),
            };
            if args.get(6).is_some() {
                return Err(usage());
            }
            let line =
                server::probe_application(Path::new(socket), app_id, content_rgb_a, content_rgb_b)?;
            if quiet {
                return Ok(());
            }
            let mut out = std::io::stdout().lock();
            out.write_all(line.as_bytes())
                .map_err(|error| format!("write application evidence: {error}"))?;
            out.flush()
                .map_err(|error| format!("flush application evidence: {error}"))
        }
        // The build step that installs the entry; the shipped binary is the
        // only encoder, so nothing can compile a second, divergent copy.
        "terminfo" => {
            let path = args.get(1).ok_or_else(usage)?;
            if args.get(2).is_some() {
                return Err(usage());
            }
            // ncurses finds an entry only under its own first letter, and a
            // path that spells it differently is a terminal with no
            // description and nothing to say so.
            if !path.ends_with(terminfo::INSTALL_PATH) {
                return Err(format!(
                    "{path} does not end with {}",
                    terminfo::INSTALL_PATH
                ));
            }
            let bytes = terminfo::entry()?;
            std::fs::write(path, bytes).map_err(|error| format!("write {path}: {error}"))
        }
        "selftest" => {
            if args.get(1).is_some() {
                return Err(usage());
            }
            selftest()
        }
        _ => Err(usage()),
    }
}

/// The two run flags both clients take. They are spelled the same way because
/// the terminal is meant to replace the demo service, not to be started
/// differently from it; what differs is only what each builds from them.
fn parse_run_flags(args: &[String]) -> Result<(PathBuf, PathBuf), String> {
    let mut socket = None;
    let mut ready_socket = None;
    let mut index = 0;
    while index < args.len() {
        let flag = args
            .get(index)
            .ok_or_else(|| "missing run flag".to_string())?;
        let value = args
            .get(index + 1)
            .ok_or_else(|| format!("{flag} requires a value"))?;
        match flag.as_str() {
            "--socket" if socket.is_none() => socket = Some(PathBuf::from(value)),
            "--ready-socket" if ready_socket.is_none() => ready_socket = Some(PathBuf::from(value)),
            "--socket" | "--ready-socket" => return Err(format!("duplicate flag '{flag}'")),
            _ => return Err(format!("unrecognised argument '{flag}'")),
        }
        index += 2;
    }
    Ok((
        socket.ok_or_else(|| "--socket is required".to_string())?,
        ready_socket.ok_or_else(|| "--ready-socket is required".to_string())?,
    ))
}

fn parse_client_run(args: &[String]) -> Result<client::Options, String> {
    let (socket, ready_socket) = parse_run_flags(args)?;
    Ok(client::Options {
        socket,
        ready_socket,
    })
}

fn run_client(args: &[String]) -> Result<(), client::ClientRunFailure> {
    let command = args.first().ok_or_else(client_usage)?;
    match command.as_str() {
        "run" => client::run(&parse_client_run(args.get(1..).ok_or_else(client_usage)?)?),
        "probe" => {
            let socket = args.get(1).ok_or_else(client_usage)?;
            if args.get(2).is_some() {
                return Err(client_usage().into());
            }
            client::probe(Path::new(socket)).map_err(Into::into)
        }
        "selftest" => {
            let shared_network = match (args.get(1).map(String::as_str), args.get(2)) {
                (None, None) => false,
                (Some("--shared-network"), None) => true,
                _ => return Err(client_usage().into()),
            };
            client::selftest(shared_network)
        }
        _ => Err(client_usage().into()),
    }
}

fn main() {
    let mut argv = env::args();
    let executable = argv.next().unwrap_or_default();
    let args: Vec<String> = argv.collect();
    let jail_fixture =
        env::var_os("FLATPAK_ID").as_deref() == Some(client::JAIL_FIXTURE_ID.as_ref());
    let personality = Personality::of(&executable, jail_fixture);
    let result = match personality {
        Personality::Compositor => run(&args),
        Personality::Demo => match run_client(&args) {
            Ok(()) => Ok(()),
            Err(error) => {
                eprintln!("{}: {}", personality.program(), error.message());
                process::exit(error.exit_code());
            }
        },
        Personality::Term => run_term(&args),
    };
    if let Err(error) = result {
        eprintln!("{}: {error}", personality.program());
        process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_three_names_pick_the_three_programs() {
        // Installed symlinks select by basename; only the exact fixture entry
        // also uses its authenticated application identity.
        assert!(Personality::of("/bin/td-term", false) == Personality::Term);
        assert!(Personality::of("td-term", false) == Personality::Term);
        assert!(Personality::of("/td/store/x-td-compositor/bin/td-term", false) == Personality::Term);
        assert!(Personality::of("/bin/td-ui-demo", false) == Personality::Demo);
        assert!(Personality::of("/bin/td-compositor", false) == Personality::Compositor);
        assert!(Personality::of(client::JAIL_FIXTURE_ENTRY, true) == Personality::Demo);
        assert!(Personality::of("/bin/td-compositor", true) == Personality::Compositor);
        assert!(Personality::of("/bin/td-term", true) == Personality::Term);
        assert!(Personality::of("/bin/td-ui-demo", true) == Personality::Demo);
        // An unknown name is the compositor, as it was before td-term existed.
        assert!(Personality::of("/bin/something-else", false) == Personality::Compositor);
        assert!(Personality::of("", false) == Personality::Compositor);
        // A name that merely CONTAINS one is not that one.
        assert!(Personality::of("/bin/td-terminal", false) == Personality::Compositor);
        assert!(Personality::of("/bin/td-term/", false) == Personality::Term);
        for personality in [
            Personality::Compositor,
            Personality::Demo,
            Personality::Term,
        ] {
            assert_eq!(Personality::of(personality.program(), false), personality);
        }
    }

    #[test]
    fn the_terminal_serves_only_what_it_can_serve_yet() {
        // `run` lands with the Wayland client; until then it is not a command,
        // and neither is anything else that is not spelled out.
        assert!(run_term(&[]).is_err());
        assert!(run_term(&["run".into()]).is_err());
        assert!(run_term(&["probe".into()]).is_err());
        assert!(run_term(&["probe".into(), "/nonexistent".into()]).is_err());
        assert!(run_term(&["probe".into(), "/a".into(), "/b".into()]).is_err());
        assert!(run_term(&["selftest".into(), "extra".into()]).is_err());
        run_term(&["selftest".into()]).unwrap();
        // The marker is the terminal's own, not the compositor's.
        let mut printed = Vec::new();
        term_selftest(&mut printed).unwrap();
        assert_eq!(printed, b"TD-TERM-SELFTEST-OK\n");
    }

    #[test]
    fn native_launcher_commands_round_trip_through_the_client_parser() {
        let launch = launcher::LaunchOptions {
            socket: PathBuf::from("/run/user/1000/wayland-0"),
            client: Some(PathBuf::from("/bin/td-ui-demo")),
            terminal: PathBuf::from("/bin/td-term"),
            application: None,
        };
        let (program, arguments, ready_socket) =
            launcher::launch_command(&launch, launcher::LaunchRequest::UiDemo, 7).unwrap();
        let arguments: Vec<String> = arguments
            .into_iter()
            .map(|argument| argument.into_string().unwrap())
            .collect();
        assert_eq!(Some(program), launch.client);
        assert_eq!(arguments.first().map(String::as_str), Some("run"));
        let parsed = parse_client_run(arguments.get(1..).unwrap()).unwrap();
        assert_eq!(parsed.socket, launch.socket);
        assert_eq!(
            parsed.ready_socket.parent(),
            Some(Path::new("/run/user/1000"))
        );
        assert_eq!(
            ready_socket.parent(),
            Some(Path::new("/run/user/1000"))
        );
        let ready_name = parsed.ready_socket.file_name().unwrap().to_string_lossy();
        assert!(ready_name.starts_with("td-launcher-"));
        assert!(ready_name.ends_with("-7.ready"));

        // The terminal rides the same `parse_run_flags`, reached here through
        // the demo's wrapper — the shared parser is what makes one set of run
        // flags a property rather than a coincidence.
        let (program, arguments, ready_socket) =
            launcher::launch_command(&launch, launcher::LaunchRequest::Terminal, 8).unwrap();
        assert_eq!(program, launch.terminal);
        let arguments: Vec<String> = arguments
            .into_iter()
            .map(|argument| argument.into_string().unwrap())
            .collect();
        assert_eq!(arguments.first().map(String::as_str), Some("run"));
        let parsed = parse_client_run(arguments.get(1..).unwrap()).unwrap();
        assert_eq!(parsed.socket, launch.socket);
        assert_eq!(parsed.ready_socket, ready_socket);
        // The two usage strings are hand-written and the parser is not, so the
        // thing that can drift is what each TELLS an operator. Both must spell
        // the shared flags identically, or one personality documents a
        // spelling `parse_run_flags` would refuse.
        let flags = "run --socket PATH --ready-socket PATH";
        assert!(client_usage().contains(flags), "{}", client_usage());
        assert!(term_usage().contains(flags), "{}", term_usage());
    }
}

#[cfg(test)]
mod confinement {
    const IMPORTERS: &[(&str, &str)] = &[
        ("import-libvterm.rs", include_str!("../tools/import-libvterm.rs")),
        ("import-unifont.rs", include_str!("../tools/import-unifont.rs")),
    ];
    const MAIN: &str = include_str!("main.rs");
    const SHARED_SHA256: &str = include_str!("../../engine/src/sha256.rs");
    const SYS: &str = include_str!("sys.rs");
    const OTHER: &[(&str, &str)] = &[
        ("bar.rs", include_str!("bar.rs")),
        ("client.rs", include_str!("client.rs")),
        ("client_resources.rs", include_str!("client_resources.rs")),
        ("configure.rs", include_str!("configure.rs")),
        ("conn.rs", include_str!("conn.rs")),
        ("font.rs", include_str!("font.rs")),
        ("font_data.rs", include_str!("font_data.rs")),
        ("framebuffer.rs", include_str!("framebuffer.rs")),
        ("help.rs", include_str!("help.rs")),
        ("input.rs", include_str!("input.rs")),
        ("keyboard.rs", include_str!("keyboard.rs")),
        ("keys.rs", include_str!("keys.rs")),
        ("launcher.rs", include_str!("launcher.rs")),
        ("layout.rs", include_str!("layout.rs")),
        ("pointer.rs", include_str!("pointer.rs")),
        ("positioner.rs", include_str!("positioner.rs")),
        ("pty.rs", include_str!("pty.rs")),
        ("ready.rs", include_str!("ready.rs")),
        ("render.rs", include_str!("render.rs")),
        ("runtime.rs", include_str!("runtime.rs")),
        ("scene.rs", include_str!("scene.rs")),
        ("server.rs", include_str!("server.rs")),
        ("socket.rs", include_str!("socket.rs")),
        ("term.rs", include_str!("term.rs")),
        ("term_client.rs", include_str!("term_client.rs")),
        ("terminfo.rs", include_str!("terminfo.rs")),
        ("ui.rs", include_str!("ui.rs")),
        ("wire.rs", include_str!("wire.rs")),
    ];
    const TEST_ONLY: &[(&str, &str)] = &[
        ("render_spec.rs", include_str!("render_spec.rs")),
        ("term_spec.rs", include_str!("term_spec.rs")),
    ];

    /// A module's source with its test module removed. Anchored on the test
    /// MODULE and not on `#[cfg(test)]` alone, since a crate gates individual
    /// constants, imports and helpers that way too and the first of those
    /// would truncate a scan to nothing.
    ///
    /// A missing anchor PANICS rather than falling back to the whole file:
    /// every caller below counts something its own test module also spells,
    /// so a silent fallback would not loosen these scans — it would invert
    /// them, and they would pass by seeing the test that proves the opposite.
    fn production(source: &str) -> &str {
        let Some((production, _)) = source.split_once("\n#[cfg(test)]\nmod tests {") else {
            panic!("a scanned module has no test module to cut at")
        };
        production
    }

    fn occurrences(source: &str, needle: &str) -> usize {
        source.match_indices(needle).count()
    }

    /// Braces and commas go with the whitespace, because `use crate::{sys as
    /// raw};` squeezes to something no ungrouped form matches — and the alias
    /// it introduces is then invisible to the caller scan as well.
    fn squeezed(source: &str) -> String {
        source
            .chars()
            .filter(|c| !c.is_whitespace() && !matches!(c, '{' | '}' | ','))
            .collect()
    }

    /// The terminal's selftest is a composition, and its marker says all four
    /// ran. Nothing observable distinguishes three from four — each returns
    /// `Ok(())` — so the composition is pinned against the source, the way
    /// this crate pins everything else the compiler cannot see.
    #[test]
    fn the_terminals_selftest_covers_all_four_of_its_layers() {
        let body = MAIN
            .split("fn term_selftest")
            .nth(1)
            .and_then(|rest| rest.split("\nfn ").next())
            .expect("term_selftest body");
        for layer in [
            "term::selftest()?",
            "pty::selftest()?",
            "ready::selftest()?",
            "term_client::selftest()?",
        ] {
            assert!(body.contains(layer), "the terminal selftest skips {layer}");
        }
    }

    #[test]
    fn confinement_inventory_covers_every_module() {
        let mut declared: Vec<&str> = MAIN
            .lines()
            .filter_map(|line| {
                line.strip_prefix("pub ")
                    .unwrap_or(line)
                    .strip_prefix("mod ")?
                    .strip_suffix(';')
            })
            .collect();
        let mut inventoried: Vec<&str> = OTHER
            .iter()
            .filter_map(|(name, _)| name.strip_suffix(".rs"))
            .chain(std::iter::once("sys"))
            .collect();
        declared.sort_unstable();
        inventoried.sort_unstable();
        assert_eq!(inventoried, declared);
    }

    #[test]
    fn importer_is_a_safe_standalone_crate_root() {
        for (name, importer) in IMPORTERS {
            assert!(
                importer.starts_with("#![deny(unsafe_code)]"),
                "{name} does not deny unsafe"
            );
            assert_eq!(occurrences(importer, "unsafe"), 1, "{name}");
            assert_eq!(occurrences(importer, "#[allow(unsafe_code)]"), 0, "{name}");
            assert_eq!(occurrences(importer, "core::arch::asm!"), 0, "{name}");
        }
        assert_eq!(occurrences(SHARED_SHA256, "unsafe"), 0);
        assert_eq!(occurrences(SHARED_SHA256, "core::arch::asm!"), 0);
    }

    #[test]
    fn two_scoped_unsafe_bodies_are_the_syscalls_and_exact_fd_adoption() {
        let syscall_body = r#"#[allow(unsafe_code)]
fn syscall3(number: usize, a1: usize, a2: usize, a3: usize) -> isize {
    let result: isize;
    unsafe {
        core::arch::asm!(
            "syscall",
            inlateout("rax") number as isize => result,
            in("rdi") a1,
            in("rsi") a2,
            in("rdx") a3,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack, preserves_flags),
        );
    }
    result
}"#;
        assert!(MAIN.contains("#![deny(unsafe_code)]"));
        let adoption_body = r#"    #[allow(unsafe_code)]
    pub fn into_file(self) -> File {
        let owned = ManuallyDrop::new(self);
        // SAFETY: `ReceivedFd::adopt` accepted one live SCM_RIGHTS descriptor,
        // this consumes its sole owner, and `ManuallyDrop` prevents the raw
        // close path from running after `File` assumes that ownership.
        unsafe { File::from_raw_fd(owned.fd) }
    }"#;
        assert_eq!(occurrences(SYS, "#[allow(unsafe_code)]"), 2);
        assert_eq!(occurrences(SYS, "unsafe {"), 2);
        assert_eq!(occurrences(SYS, "core::arch::asm!"), 1);
        assert_eq!(occurrences(SYS, syscall_body), 1);
        assert_eq!(occurrences(SYS, adoption_body), 1);
        assert_eq!(occurrences(SYS, "File::from_raw_fd("), 1);
        for syscall in [
            "const SYS_CLOSE: usize = 3;",
            "const SYS_IOCTL: usize = 16;",
            "const SYS_SENDMSG: usize = 46;",
            "const SYS_RECVMSG: usize = 47;",
            "const SYS_FCNTL: usize = 72;",
        ] {
            assert!(SYS.contains(syscall), "{syscall}");
        }
        assert_eq!(occurrences(SYS, "const SYS_"), 5);
        for (name, source) in OTHER.iter().chain(TEST_ONLY) {
            assert!(
                !source.contains("unsafe"),
                "{name} introduced a second unsafe surface"
            );
            assert!(
                !source.contains("core::arch::asm!"),
                "{name} introduced another raw syscall body"
            );
        }
    }

    #[test]
    fn the_fcntl_surface_is_two_pinned_file_status_commands() {
        for command in [
            "const F_GETFL: usize = 3;",
            "const F_SETFL: usize = 4;",
            "const O_NONBLOCK: usize = 0o4000;",
        ] {
            assert!(SYS.contains(command), "{command}");
        }
        let guard = "    if !matches!(command, F_GETFL | F_SETFL) {";
        let entry =
            "    errno_result(syscall3(SYS_FCNTL, fd as usize, command, argument), operation)";
        assert_eq!(occurrences(SYS, guard), 1);
        assert_eq!(occurrences(SYS, entry), 1);
        assert_eq!(occurrences(SYS, "fn fcntl("), 1);
        assert_eq!(occurrences(production(SYS), "fcntl(file.as_raw_fd(), F_GETFL, 0"), 1);
        assert_eq!(
            occurrences(
                production(SYS),
                "        F_SETFL,\n        flags | O_NONBLOCK,"
            ),
            1
        );
        assert_eq!(
            occurrences(
                production(SYS),
                "fcntl(file.as_raw_fd(), F_SETFL, flags, \"restore F_SETFL\")?"
            ),
            1
        );
        for wrapper in [
            "pub fn make_nonblocking(",
            "pub fn restore_status_flags(",
        ] {
            assert_eq!(occurrences(SYS, wrapper), 1, "{wrapper}");
        }
    }

    #[test]
    fn test_only_module_inventory_matches_path_declarations() {
        for (name, _) in TEST_ONLY {
            let declaration = format!("#[path = \"{name}\"]");
            assert!(
                OTHER
                    .iter()
                    .any(|(_, source)| source.contains(&declaration)),
                "{name} is not declared from an inventoried module"
            );
        }
    }

    #[test]
    fn confinement_inventory_covers_every_source_file() {
        let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut actual = std::fs::read_dir(directory)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().and_then(|value| value.to_str()) == Some("rs"))
            .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        actual.sort();
        let mut inventoried = vec!["main.rs".to_string(), "sys.rs".to_string()];
        inventoried.extend(
            OTHER
                .iter()
                .chain(TEST_ONLY)
                .map(|(name, _)| (*name).to_string()),
        );
        inventoried.sort();
        assert_eq!(inventoried, actual);
    }

    /// `ioctl(2)`'s request number chooses the operation, so the roster is the
    /// confinement: these six values, one entry point, and five callers.
    #[test]
    fn the_ioctl_surface_is_six_pinned_requests_and_five_wrappers() {
        for request in [
            "const TIOCSPTLCK: usize = 0x4004_5431;",
            "const TIOCGPTPEER: usize = 0x5441;",
            "const TIOCSWINSZ: usize = 0x5414;",
            "const TIOCGWINSZ: usize = 0x5413;",
            "const EVIOCGABS_X: usize = 0x8018_4540;",
            "const EVIOCGABS_Y: usize = 0x8018_4541;",
        ] {
            assert!(SYS.contains(request), "{request}");
        }
        assert_eq!(occurrences(SYS, "const TIOC"), 4);
        assert_eq!(occurrences(SYS, "const EVIOCGABS"), 2);
        // The two evdev requests differ in ONE nibble, and the size field of
        // each has to be the 24 bytes `ABSINFO_WORDS` declares — so the axis
        // is chosen by an enum ARM rather than by a number at a call site.
        // Counted over the shipped half alone, as the roster scans below are:
        // the crate's own tests name these numbers to assert things about
        // them, and a pin that moved whenever a test did would pin nothing.
        // Three each: the declaration, the guard, and the arm that picks it.
        assert_eq!(occurrences(SYS, "const ABSINFO_WORDS: usize = 6;"), 1);
        assert_eq!(occurrences(production(SYS), "EVIOCGABS_X"), 3);
        assert_eq!(occurrences(production(SYS), "EVIOCGABS_Y"), 3);
        // The guard names each request once more; every other mention is a
        // wrapper passing it to the one entry point.
        let guard = r#"    if !matches!(
        request,
        TIOCSPTLCK | TIOCGPTPEER | TIOCSWINSZ | TIOCGWINSZ | EVIOCGABS_X | EVIOCGABS_Y
    ) {"#;
        assert_eq!(occurrences(SYS, guard), 1);
        let entry = r#"    errno_result(syscall3(SYS_IOCTL, fd as usize, request, argument), operation)"#;
        assert_eq!(occurrences(SYS, entry), 1);
        assert_eq!(occurrences(SYS, "fn ioctl("), 1);
        // One definition plus exactly five call sites: a SIXTH wrapper reusing
        // a pinned request would satisfy every other assertion here.
        let production_sys = production(SYS);
        let mentions = occurrences(production_sys, "ioctl(");
        let prose = occurrences(production_sys, "ioctl(2)");
        assert_eq!(mentions - prose, 6);
        // The four wrappers, each reaching that entry point exactly once with
        // its own request and its own operand.
        for (wrapper, call) in [
            (
                "pub fn unlock_pty(",
                "        master.as_raw_fd(),\n        TIOCSPTLCK,\n        (&unlocked as *const i32) as usize,\n        \"TIOCSPTLCK\",",
            ),
            (
                "pub fn pty_peer(",
                "        master.as_raw_fd(),\n        TIOCGPTPEER,\n        PTY_PEER_FLAGS,\n        \"TIOCGPTPEER\",",
            ),
            (
                "pub fn set_window_size(",
                "        terminal.as_raw_fd(),\n        TIOCSWINSZ,\n        (&words as *const [u16; 4]) as usize,\n        \"TIOCSWINSZ\",",
            ),
            (
                "pub fn window_size(",
                "        terminal.as_raw_fd(),\n        TIOCGWINSZ,\n        (&mut words as *mut [u16; 4]) as usize,\n        \"TIOCGWINSZ\",",
            ),
        ] {
            assert!(SYS.contains(wrapper), "{wrapper}");
            assert_eq!(occurrences(SYS, call), 1, "{wrapper}");
        }
        // The fifth takes its request from an AXIS rather than being handed
        // one, so the two evdev numbers are unreachable from any call site.
        assert!(SYS.contains("pub fn absolute_info(device: &impl AsRawFd, axis: AbsAxis)"));
        assert_eq!(
            occurrences(
                SYS,
                "        device.as_raw_fd(),\n        request,\n        (&mut words as *mut [i32; ABSINFO_WORDS]) as usize,\n        name,"
            ),
            1
        );
        // The peer's open flags are pinned rather than chosen by a caller: the
        // slave belongs to the child, so O_NOCTTY cannot be forgotten.
        assert!(SYS.contains("const PTY_PEER_FLAGS: usize = 0o2 | 0o400 | 0o2_000_000;"));
        // The eight bytes the kernel reads are an array whose layout the
        // language guarantees, not an attribute nobody can observe. Same for
        // the absinfo's twenty-four, where the three leading words are the
        // same type and a slipped index is a well-formed wrong range.
        assert!(SYS.contains("fn winsize_words(size: WindowSize) -> [u16; 4] {"));
        assert_eq!(occurrences(SYS, "as *const [u16; 4]"), 1);
        assert_eq!(occurrences(SYS, "as *mut [u16; 4]"), 1);
        assert_eq!(occurrences(SYS, "as *mut [i32; ABSINFO_WORDS]"), 1);
        assert!(SYS.contains("fn absinfo(words: [i32; ABSINFO_WORDS]) -> AbsInfo {"));
    }

    #[test]
    fn syscall_wrapper_is_called_only_by_the_five_reviewed_operations() {
        let close = r#"errno_result(syscall3(SYS_CLOSE, fd as usize, 0, 0), "close")?"#;
        let receive = r#"syscall3(
            SYS_RECVMSG,
            stream.as_raw_fd() as usize,
            (&mut message as *mut MsgHdr) as usize,
            MSG_CMSG_CLOEXEC as usize,
        )"#;
        let send = r#"syscall3(
            SYS_SENDMSG,
            stream.as_raw_fd() as usize,
            (&message as *const MsgHdr) as usize,
            0,
        )"#;
        assert_eq!(occurrences(SYS, "syscall3("), 6);
        assert_eq!(occurrences(SYS, "SYS_CLOSE"), 2);
        assert_eq!(occurrences(SYS, "SYS_FCNTL"), 2);
        assert_eq!(occurrences(SYS, "SYS_IOCTL"), 2);
        assert_eq!(occurrences(SYS, "SYS_SENDMSG"), 2);
        assert_eq!(occurrences(SYS, "SYS_RECVMSG"), 2);
        assert_eq!(occurrences(SYS, close), 1);
        assert_eq!(occurrences(SYS, receive), 1);
        assert_eq!(occurrences(SYS, send), 1);
        for operation in [
            "fn close_raw(",
            "fn fcntl(",
            "pub fn recv_with_fds(",
            "pub fn send_with_fd(",
        ] {
            assert!(SYS.contains(operation), "{operation}");
        }
    }

    /// Three disjoint reviewed surfaces live behind one syscall body, so each
    /// is pinned to its own module: descriptor transport to the protocol
    /// endpoints, terminal control to the PTY adapter, an absolute device's
    /// axis range to the evdev reader. Nothing else names `sys` at all — an
    /// alias elsewhere would give an audited call a name none of these scans
    /// looks for.
    #[test]
    fn each_confined_operation_is_reachable_only_from_its_own_module() {
        const TRANSPORT: &[&str] = &[
            "sys::send_with_fd(",
            "sys::recv_with_fds(",
            "sys::duplicate_received(",
            "sys::discard_received(",
            "sys::ReceivedFd::adopt(",
            "sys::ReceivedFd::into_file(",
            "sys::make_nonblocking(",
            "sys::restore_status_flags(",
        ];
        const TERMINAL: &[&str] = &[
            "sys::unlock_pty(",
            "sys::pty_peer(",
            "sys::set_window_size(",
            "sys::window_size(",
        ];
        // The third surface, and the reason the module list below grew: an
        // absolute pointer's range is asked for where the device file is
        // opened, and again only at a recovery.
        const ABSOLUTE: &[&str] = &["sys::absolute_info("];
        // Each axis asked once and asked FOR ITS OWN COORDINATE, neither of
        // which a runtime test on this gate can see: a pair that asks X twice,
        // or that crosses the two, is a well-formed pair of ranges that maps
        // every report to the wrong part of the screen, and the gate has no
        // absolute device to notice with. The counts alone catch only the
        // first — crossing the two leaves both at one — so the call sites are
        // pinned whole, as `sys.rs`'s wrappers are.
        let input = production(include_str!("input.rs"));
        assert_eq!(occurrences(input, "sys::AbsAxis::X"), 1);
        assert_eq!(occurrences(input, "sys::AbsAxis::Y"), 1);
        for call in [
            "let x = sys::absolute_info(device, sys::AbsAxis::X).ok()?;",
            "let y = sys::absolute_info(device, sys::AbsAxis::Y).ok()?;",
        ] {
            assert!(input.contains(call), "input.rs no longer spells `{call}`");
        }
        let production_main = production(MAIN);
        for (name, source) in std::iter::once(("main.rs", production_main))
            .chain(OTHER.iter().copied())
            .chain(TEST_ONLY.iter().copied())
        {
            if matches!(
                name,
                "client.rs" | "conn.rs" | "server.rs" | "pty.rs" | "input.rs"
            ) {
                continue;
            }
            assert!(
                !source.contains("sys::"),
                "{name} reached a confined syscall outside its reviewed module"
            );
        }
        // A NAMED or ALIASED import defeats the scan above the way a glob does:
        // `use crate::sys as raw;` then `raw::set_window_size(...)` matches
        // nothing it looks for, and terminal control would be reachable from a
        // module that never resizes anything it verified. Every reach must be
        // spelled `sys::<wrapper>` where the caller scan can see it — including
        // inside the three permitted modules, so the module list stays the
        // whole answer to "who can call this".
        for (name, source) in std::iter::once(("main.rs", production_main))
            .chain(OTHER.iter().copied())
            .chain(TEST_ONLY.iter().copied())
        {
            for form in [
                concat!("use", "crate::sys::"),
                concat!("use", "super::sys::"),
                concat!("sys", "as"),
                concat!("sys::", "*"),
            ] {
                assert_eq!(
                    squeezed(source).matches(form).count(),
                    0,
                    "{name} imports out of the syscall module ('{form}')"
                );
            }
        }
        // Extracting the connection made the transport CRATE-VISIBLE: a module
        // that holds a `Connection` reaches `sendmsg`/`recvmsg` through
        // `send_with_fd`/`next`/`take_fd` without ever spelling `sys::`, which
        // is all the scan above looks for. So who may NAME the transport is a
        // roster on the same footing as who may call the syscall, and the
        // terminal's client joins it by amendment rather than by importing.
        const TRANSPORT_USERS: &[&str] = &["client.rs", "conn.rs", "term_client.rs"];
        for (name, source) in std::iter::once(("main.rs", production_main))
            .chain(OTHER.iter().copied())
            .chain(TEST_ONLY.iter().copied())
        {
            if TRANSPORT_USERS.contains(&name) {
                continue;
            }
            for form in [concat!("conn", "::"), concat!("conn", "as")] {
                assert_eq!(
                    squeezed(source).matches(form).count(),
                    0,
                    "{name} reached the Wayland transport ('{form}')"
                );
            }
        }
        let client = include_str!("client.rs");
        let conn = include_str!("conn.rs");
        let server = include_str!("server.rs");
        let pty = include_str!("pty.rs");
        assert_eq!(
            occurrences(production(server), "sys::ReceivedFd::adopt("),
            1,
            "server selection descriptor ownership must have one adoption site"
        );
        assert_eq!(
            occurrences(production(conn), "sys::ReceivedFd::adopt("),
            1,
            "client selection descriptor ownership must have one adoption site"
        );
        assert_eq!(
            occurrences(production(conn), "sys::ReceivedFd::into_file("),
            1,
            "client selection endpoints must have one exact conversion site"
        );
        for source in [production(client), production(server)] {
            assert_eq!(
                occurrences(source, "sys::ReceivedFd::into_file("),
                0,
                "exact endpoint conversion escaped conn.rs"
            );
        }
        assert_eq!(
            occurrences(
                production(conn),
                "let flags = sys::make_nonblocking(file)?;"
            ),
            1,
            "client selection writes must capture the kernel's status word"
        );
        assert_eq!(
            occurrences(
                production(conn),
                "let restored = sys::restore_status_flags(file, flags);"
            ),
            1,
            "client selection writes must restore the captured status word"
        );
        for source in [production(client), production(server)] {
            assert_eq!(occurrences(source, "sys::make_nonblocking("), 0);
            assert_eq!(occurrences(source, "sys::restore_status_flags("), 0);
        }
        for operation in TRANSPORT {
            assert!(
                client.contains(operation) || conn.contains(operation) || server.contains(operation),
                "{operation}"
            );
            assert!(!pty.contains(operation), "pty.rs reached {operation}");
        }
        let input = include_str!("input.rs");
        for operation in TERMINAL {
            assert!(pty.contains(operation), "{operation}");
            assert!(
                !client.contains(operation)
                    && !conn.contains(operation)
                    && !server.contains(operation)
                    && !input.contains(operation),
                "a module outside the terminal reached {operation}"
            );
        }
        // The three surfaces are DISJOINT, which is what makes the roster a
        // statement about each rather than about their union: the input reader
        // may ask a device for its range and nothing else, and no module that
        // speaks the protocol or drives a terminal may ask at all.
        for operation in ABSOLUTE {
            assert!(input.contains(operation), "{operation}");
            assert!(
                !client.contains(operation)
                    && !conn.contains(operation)
                    && !server.contains(operation)
                    && !pty.contains(operation),
                "a module outside the input reader reached {operation}"
            );
        }
        for operation in TRANSPORT.iter().chain(TERMINAL) {
            assert!(!input.contains(operation), "input.rs reached {operation}");
        }
        // Both wrappers are generic over `AsRawFd`, so inside pty.rs they
        // would type-check against ANY terminal — including an operator's.
        let production_pty = production(pty);
        // The IDENTIFIER count is the load-bearing one, and it is what the
        // spelling assertions below are not: a call written with a space, or
        // taken as a function pointer and called through that, satisfies
        // every `sys::window_size(&self.master)` match while reaching the
        // kernel with a descriptor nobody checked. It bounds every reach
        // because the glob-and-alias scan above already requires each to be
        // spelled `sys::<wrapper>`. Three: one in `set_window_size`, two in
        // `window_size` — the setter's caller is `Pty::resize`, which
        // verifies what it published, and the getter's two are that same
        // readback and the accessor for something that did NOT set the size.
        assert_eq!(occurrences(production_pty, "window_size"), 3);
        assert_eq!(occurrences(production_pty, "sys::set_window_size("), 1);
        assert!(production_pty.contains("sys::set_window_size(&self.master, requested)?;"));
        assert_eq!(occurrences(production_pty, "sys::window_size("), 2);
        assert_eq!(
            occurrences(production_pty, "sys::window_size(&self.master)"),
            2
        );
        assert!(production_pty.contains("let observed = sys::window_size(&self.master)?;"));
    }

    /// The binary's own `selftest` subcommand, which is what the td-ui-test
    /// RECIPE runs — and which nothing in `cargo test` reached before. A
    /// geometry change that made it fail was invisible here until the recipe
    /// gate said so, twenty minutes later.
    #[test]
    fn the_selftest_subcommand_passes() {
        super::selftest().unwrap();
    }

    #[test]
    fn run_parser_requires_every_device_boundary() {
        use std::os::unix::fs::symlink;
        use std::path::Path;
        use std::time::{SystemTime, UNIX_EPOCH};

        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "td-compositor-parser-{}-{nonce}",
            std::process::id()
        ));
        let actual = root.join("actual");
        let alias = root.join("alias");
        std::fs::create_dir_all(&actual).unwrap();
        symlink(&actual, &alias).unwrap();
        let wayland = actual.join("wayland-0");
        let ready = actual.join("firefox-window-ready");
        let valid = |socket: &Path, ready: &Path| {
            vec![
                "--framebuffer".into(),
                "/dev/fb0".into(),
                "--input".into(),
                "/dev/input".into(),
                "--socket".into(),
                socket.to_string_lossy().into_owned(),
                "--launcher-application".into(),
                "td-jail-fixture".into(),
                "--terminal-client".into(),
                "/bin/td-term".into(),
                "--application-ready-socket".into(),
                ready.to_string_lossy().into_owned(),
                "--application-app-id".into(),
                "org.mozilla.firefox".into(),
                "--application-content-rgb-a".into(),
                "ff00ff".into(),
                "--application-content-rgb-b".into(),
                "00ff00".into(),
            ]
        };
        let options = super::parse_run(&valid(&wayland, &ready)).unwrap();
        let resolved_parent = std::fs::canonicalize(&actual).unwrap();
        assert_eq!(options.framebuffer, std::path::PathBuf::from("/dev/fb0"));
        assert_eq!(options.launcher_client, None);
        assert_eq!(options.launcher_application.as_deref(), Some("td-jail-fixture"));
        assert_eq!(
            options.terminal_client,
            std::path::PathBuf::from("/bin/td-term")
        );
        assert_eq!(options.socket, resolved_parent.join("wayland-0"));
        assert_eq!(
            options.application_ready_socket,
            Some(resolved_parent.join("firefox-window-ready"))
        );
        assert_eq!(
            options.application_content_rgb_a.as_deref(),
            Some("ff00ff")
        );
        assert_eq!(
            options.application_content_rgb_b.as_deref(),
            Some("00ff00")
        );
        let mut activation_without_observer = valid(&wayland, &ready);
        activation_without_observer.truncate(activation_without_observer.len() - 8);
        assert!(super::parse_run(&activation_without_observer).is_err());
        let mut one_content_color = valid(&wayland, &ready);
        one_content_color.truncate(one_content_color.len() - 2);
        assert!(super::parse_run(&one_content_color).is_err());
        assert_eq!(
            options.application_app_id.as_deref(),
            Some("org.mozilla.firefox")
        );
        // Each device boundary and exactly one launcher mode are required, so
        // dropping one is refused rather than defaulted.
        assert!(super::parse_run(&[
            "--framebuffer".into(),
            "/dev/fb0".into(),
            "--input".into(),
            "/dev/input".into(),
            "--socket".into(),
            "/run/user/1000/wayland-0".into(),
            "--launcher-client".into(),
            "/bin/td-ui-demo".into(),
        ])
        .is_err());
        assert!(super::parse_run(&valid(
            &wayland,
            &actual.join(".").join("wayland-0")
        ))
        .is_err());
        assert!(super::parse_run(&valid(&wayland, &alias.join("wayland-0"))).is_err());
        // The observer is meaningful only as one exact argument set around a
        // configured application launch.
        assert!(super::parse_run(&[
            "--framebuffer".into(),
            "/dev/fb0".into(),
            "--input".into(),
            "/dev/input".into(),
            "--socket".into(),
            "/run/user/1000/wayland-0".into(),
            "--launcher-application".into(),
            "td-jail-fixture".into(),
            "--terminal-client".into(),
            "/bin/td-term".into(),
            "--application-ready-socket".into(),
            "/run/user/1000/application-ready".into(),
        ])
        .is_err());
        assert!(super::parse_run(&[
            "--framebuffer".into(),
            "/dev/fb0".into(),
            "--input".into(),
            "/dev/input".into(),
            "--socket".into(),
            "/run/user/1000/wayland-0".into(),
            "--launcher-client".into(),
            "/bin/td-ui-demo".into(),
            "--terminal-client".into(),
            "/bin/td-term".into(),
            "--application-ready-socket".into(),
            "/run/user/1000/application-ready".into(),
            "--application-app-id".into(),
            "org.td.demo".into(),
        ])
        .is_err());
        assert!(super::parse_run(&[
            "--framebuffer".into(),
            "/dev/fb0".into(),
            "--input".into(),
            "/dev/input".into(),
            "--socket".into(),
            "/run/user/1000/wayland-0".into(),
            "--launcher-application".into(),
            "td-jail-fixture".into(),
            "--terminal-client".into(),
            "/bin/td-term".into(),
        ])
        .is_err());
        // The activation-only application mode cannot also retain a dead
        // direct launcher client.
        let mut both_modes = valid(&wayland, &ready);
        both_modes.extend([
            "--launcher-client".into(),
            "/bin/td-ui-demo".into(),
        ]);
        assert!(super::parse_run(&both_modes).is_err());
        assert!(super::parse_run(&[
            "--framebuffer".into(),
            "/dev/fb0".into(),
            "--input".into(),
            "/dev/input".into(),
            "--socket".into(),
            "/run/user/1000/wayland-0".into(),
            "--terminal-client".into(),
            "/bin/td-term".into(),
        ])
        .is_err());
        // A repeated flag is refused rather than last-one-wins.
        assert!(super::parse_run(&[
            "--framebuffer".into(),
            "/dev/fb0".into(),
            "--input".into(),
            "/dev/input".into(),
            "--socket".into(),
            "/run/user/1000/wayland-0".into(),
            "--launcher-client".into(),
            "/bin/td-ui-demo".into(),
            "--terminal-client".into(),
            "/bin/td-term".into(),
            "--terminal-client".into(),
            "/bin/elsewhere".into(),
        ])
        .is_err());
        assert!(super::parse_run(&[
            "--framebuffer".into(),
            "/dev/fb0".into(),
            "--input".into(),
            "/dev/input".into(),
            "--socket".into(),
            "/run/user/1000/wayland-0".into(),
        ])
        .is_err());
        assert!(super::parse_run(&[
            "--framebuffer".into(),
            "/dev/fb0".into(),
            "--socket".into(),
            "/run/user/1000/wayland-0".into(),
        ])
        .is_err());

        std::fs::remove_dir_all(root).unwrap();
    }
}

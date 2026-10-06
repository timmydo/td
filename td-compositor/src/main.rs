#![deny(unsafe_code)]

#[allow(dead_code, reason = "shared immutable application policy")]
#[cfg_attr(
    not(feature = "target-recipe"),
    path = "../../td-busd/src/app_policy.rs"
)]
mod app_policy;

// td-ui's outline face, mounted as td-ui mounts this crate's font and wire:
// the chrome's text draws through it (text.rs).
#[allow(dead_code, reason = "shared td-ui outline face")]
#[cfg_attr(not(feature = "target-recipe"), path = "../../td-ui/src/atlas.rs")]
mod atlas;
#[allow(dead_code, reason = "shared td-ui outline face")]
#[cfg_attr(not(feature = "target-recipe"), path = "../../td-ui/src/coverage.rs")]
mod coverage;
#[allow(dead_code, reason = "shared td-ui outline face")]
#[cfg_attr(not(feature = "target-recipe"), path = "../../td-ui/src/face.rs")]
mod face;
#[allow(dead_code, reason = "shared td-ui outline face")]
#[cfg_attr(not(feature = "target-recipe"), path = "../../td-ui/src/face_file.rs")]
mod face_file;
#[allow(dead_code, reason = "shared td-ui outline face")]
#[cfg_attr(not(feature = "target-recipe"), path = "../../td-ui/src/sfnt.rs")]
mod sfnt;

mod attention;
mod authority;
mod bar;
mod buffer;
mod client;
mod client_resources;
mod clock;
mod configure;
mod conn;
mod control;
mod drm;
mod filter;
mod font;
mod font_data;
mod framebuffer;
mod headless;
mod help;
mod input;
mod keyboard;
mod launcher;
mod layout;
mod output;
mod pointer;
mod positioner;
mod proc_status;
mod reportable;
mod runtime;
mod scene;
mod secret_client;
mod server;
mod session;
mod socket;
mod sys;
mod text;
#[cfg_attr(not(feature = "target-recipe"), path = "../../td-civil/src/tzif.rs")]
mod timezone;
mod ui;
mod vm_bridge;
mod vm_wire;
mod wire;

use framebuffer::Framebuffer;
use output::OutputBackend;
use runtime::Runtime;
use std::env;
use std::ffi::OsString;
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
    "usage: td-compositor run (--card PATH [--framebuffer PATH] | --framebuffer PATH) \
     --input DIR --socket PATH \
     --portal-socket PATH [--control-socket PATH] \
     (--launcher-client PATH | --launcher-application NAME \
     --application-ready-socket PATH --application-app-id ID \
     --application-content-rgb-a RGB --application-content-rgb-b RGB) \
     (--terminal-client PATH | --terminal-authority stdin) | \
     td-compositor headless --session-dir NEW_ABSOLUTE_PATH --width N --height N \
     [--input-control enabled] [--capture-control enabled] [--clipboard-control enabled] | \
     td-compositor probe-terminal-authority | \
     td-compositor probe SOCKET | \
     td-compositor probe-application SOCKET ID RGB_A RGB_B [--quiet] | \
     td-compositor selftest\n\
     --terminal-authority requires --launcher-application. \
     --framebuffer beside --card is driven only when the card does not exist."
        .into()
}

fn client_usage() -> String {
    "usage: td-ui-demo run --socket PATH --ready-socket PATH | \
     td-ui-demo probe READY_SOCKET | td-ui-demo selftest [--shared-network]"
        .into()
}

/// td's own flags are text; only a child's argv is bytes. Refusing is what
/// `env::args()` did by panicking, said in words.
fn utf8_args(args: &[OsString]) -> Result<Vec<String>, String> {
    args.iter()
        .map(|argument| {
            argument
                .clone()
                .into_string()
                .map_err(|raw| format!("argument '{}' is not UTF-8", raw.to_string_lossy()))
        })
        .collect()
}

/// Which program this binary was invoked as. Three installed names share one
/// artifact; the fixture keeps the compositor basename, so its authenticated
/// identity and exact `/app` entry jointly select the demo personality.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Personality {
    Compositor,
    Demo,
    /// The control client: not a Wayland client at all, which is why it is a
    /// name of this artifact rather than a program of its own — the request
    /// vocabulary and the compositor that answers it are one module, and two
    /// binaries built from it could not disagree about the wire.
    Control,
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
            Some("td-ctl") => Personality::Control,
            _ => Personality::Compositor,
        }
    }

    fn program(self) -> &'static str {
        match self {
            Personality::Compositor => "td-compositor",
            Personality::Demo => "td-ui-demo",
            Personality::Control => "td-ctl",
        }
    }
}

/// Where the compositor draws. `Framebuffer` is fbdev; `Card` is a DRM
/// primary node the KMS backend takes mastership of and flips on, with the
/// fbdev node to drive instead when the machine has no such card.
#[derive(Debug, Eq, PartialEq)]
enum OutputDevice {
    Framebuffer(PathBuf),
    Card {
        card: PathBuf,
        fallback: Option<PathBuf>,
    },
}

/// The backend `run` opens, once the card's existence is known.
#[derive(Debug, Eq, PartialEq)]
enum Backend<'a> {
    Framebuffer(&'a Path),
    Card(&'a Path),
}

/// Choose the backend. `inspect` answers whether the card node exists, and is
/// asked only when there is a fallback to take.
///
/// The fallback is for a machine with NO card, and nothing else: only a
/// `NotFound` takes it. A card that exists but cannot be inspected, opened or
/// modeset is an error, never masked by quietly drawing somewhere else.
fn select_backend(
    output: &OutputDevice,
    inspect: impl FnOnce(&Path) -> std::io::Result<()>,
) -> Result<Backend<'_>, String> {
    match output {
        OutputDevice::Framebuffer(path) => Ok(Backend::Framebuffer(path)),
        OutputDevice::Card {
            card,
            fallback: None,
        } => Ok(Backend::Card(card)),
        OutputDevice::Card {
            card,
            fallback: Some(framebuffer),
        } => match inspect(card) {
            Ok(()) => Ok(Backend::Card(card)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok(Backend::Framebuffer(framebuffer))
            }
            Err(error) => Err(format!("inspect DRM device {}: {error}", card.display())),
        },
    }
}

struct RunOptions {
    output: OutputDevice,
    input: PathBuf,
    socket: PathBuf,
    portal_socket: PathBuf,
    launcher_client: Option<PathBuf>,
    launcher_application: Option<String>,
    terminal_client: Option<PathBuf>,
    terminal_authority: bool,
    /// Absent by DEFAULT, and absent means there is no control socket to
    /// reach: a session that was not asked for one exposes none, so the way
    /// to turn the surface off is not to configure it.
    control_socket: Option<PathBuf>,
    application_ready_socket: Option<PathBuf>,
    application_app_id: Option<String>,
    application_content_rgb_a: Option<String>,
    application_content_rgb_b: Option<String>,
}

fn parse_run(args: &[String]) -> Result<RunOptions, String> {
    let mut framebuffer = None;
    let mut card = None;
    let mut input = None;
    let mut socket = None;
    let mut portal_socket = None;
    let mut launcher_client = None;
    let mut launcher_application = None;
    let mut terminal_client = None;
    let mut terminal_authority = false;
    let mut control_socket = None;
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
            "--card" if card.is_none() => card = Some(PathBuf::from(value)),
            "--input" if input.is_none() => input = Some(PathBuf::from(value)),
            "--socket" if socket.is_none() => socket = Some(PathBuf::from(value)),
            "--portal-socket" if portal_socket.is_none() => {
                portal_socket = Some(PathBuf::from(value))
            }
            "--launcher-client" if launcher_client.is_none() => {
                launcher_client = Some(PathBuf::from(value))
            }
            "--launcher-application" if launcher_application.is_none() => {
                launcher_application = Some(value.clone())
            }
            "--terminal-client" if terminal_client.is_none() => {
                terminal_client = Some(PathBuf::from(value))
            }
            "--terminal-authority" if !terminal_authority => {
                if value != "stdin" {
                    return Err("--terminal-authority requires stdin".into());
                }
                terminal_authority = true;
            }
            "--control-socket" if control_socket.is_none() => {
                control_socket = Some(PathBuf::from(value))
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
            "--framebuffer"
            | "--card"
            | "--input"
            | "--socket"
            | "--portal-socket"
            | "--launcher-client"
            | "--launcher-application"
            | "--terminal-client"
            | "--terminal-authority"
            | "--control-socket"
            | "--application-ready-socket"
            | "--application-app-id"
            | "--application-content-rgb-a"
            | "--application-content-rgb-b" => {
                return Err(format!("duplicate flag '{flag}'"));
            }
            _ => return Err(format!("unrecognised argument '{flag}'")),
        }
        index += 2;
    }
    let output = match (framebuffer, card) {
        (Some(path), None) => OutputDevice::Framebuffer(path),
        (fallback, Some(card)) => OutputDevice::Card { card, fallback },
        (None, None) => return Err("--card or --framebuffer is required".into()),
    };
    if terminal_client.is_some() == terminal_authority {
        return Err("exactly one --terminal-client or --terminal-authority is required".into());
    }
    if terminal_authority && launcher_application.is_none() {
        return Err("the authority launcher requires a supervised application card".into());
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
                .into(),
        );
    }
    if application_ready_socket.is_some() != launcher_application.is_some() {
        return Err(
            "launcher applications require application-window readiness and vice versa".into(),
        );
    }
    let socket = resolve_socket_endpoint(
        &socket.ok_or_else(|| "--socket is required".to_string())?,
        "Wayland",
    )?;
    let portal_socket = resolve_socket_endpoint(
        &portal_socket.ok_or_else(|| "--portal-socket is required".to_string())?,
        "private portal Wayland",
    )?;
    // Retain the RESOLVED endpoints, not a symlink spelling that could name a
    // different parent between this check and the bind.
    let control_socket = control_socket
        .as_deref()
        .map(|path| resolve_socket_endpoint(path, "control"))
        .transpose()?;
    let application_ready_socket = application_ready_socket
        .as_deref()
        .map(|path| resolve_socket_endpoint(path, "application-window readiness"))
        .transpose()?;
    // Every endpoint against every other. Written as one walk rather than a
    // comparison per pair, because four endpoints are six pairs and the pair
    // nobody remembers to add is two services quietly sharing one socket —
    // where the failure is not a refusal but a compositor answering the
    // wrong protocol on a path something else believes it owns.
    let endpoints: [(&str, Option<&Path>); 4] = [
        ("the Wayland socket", Some(socket.as_path())),
        ("private portal Wayland", Some(portal_socket.as_path())),
        ("the control socket", control_socket.as_deref()),
        (
            "application-window readiness",
            application_ready_socket.as_deref(),
        ),
    ];
    for (index, (label, endpoint)) in endpoints.iter().enumerate() {
        let Some(endpoint) = endpoint else {
            continue;
        };
        for (other_label, other) in endpoints.iter().skip(index.saturating_add(1)) {
            if *other == Some(*endpoint) {
                return Err(format!("{other_label} must not alias {label}"));
            }
        }
    }
    Ok(RunOptions {
        output,
        input: input.ok_or_else(|| "--input is required".to_string())?,
        socket,
        portal_socket,
        launcher_client,
        launcher_application,
        terminal_client,
        terminal_authority,
        control_socket,
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
    let parent = std::fs::canonicalize(parent).map_err(|error| {
        format!(
            "resolve {label} socket parent {}: {error}",
            parent.display()
        )
    })?;
    Ok(parent.join(name))
}

fn run_compositor(options: RunOptions) -> Result<(), String> {
    let socket_policy = session::SocketPolicy::for_authority(options.terminal_authority);
    let display_policy = if options.terminal_authority {
        session::application_policy()?;
        session::SocketPolicy::ApplicationSession
    } else {
        session::SocketPolicy::Private
    };
    // Pin the root sender before framebuffer, listener, or input workers exist.
    let launches = if options.terminal_authority {
        launcher::LaunchBackend::Authority(authority::Launcher::connect()?)
    } else {
        launcher::LaunchBackend::Direct(launcher::LaunchProcesses::new(launcher::LaunchOptions {
            socket: options.socket.clone(),
            client: options.launcher_client.clone(),
            terminal: options
                .terminal_client
                .clone()
                .ok_or("missing direct terminal client")?,
            application: options
                .launcher_application
                .clone()
                .map(|name| launcher::ApplicationLaunch { name }),
        })?)
    };
    let task_launcher = launches.task_launcher();
    let backend = select_backend(&options.output, |card| std::fs::metadata(card).map(|_| ()))?;
    if let (OutputDevice::Card { card, .. }, Backend::Framebuffer(path)) =
        (&options.output, &backend)
    {
        eprintln!(
            "td-compositor: no DRM device at {}; driving {} instead",
            card.display(),
            path.display()
        );
    }
    let (mut runtime, flips, described) = match backend {
        Backend::Framebuffer(path) => {
            let framebuffer = Framebuffer::open(path)?;
            let described = describe_output("software", &framebuffer);
            (Runtime::new(framebuffer), None, described)
        }
        Backend::Card(path) => {
            let (kms, flips, lit) = drm::open_kms(path)?;
            // Logged rather than fatal, as the flip marker is: the display
            // works whether or not the console took the line.
            if let Err(error) = announce_kms_ready(&mut std::io::stdout().lock(), &lit, &kms) {
                eprintln!("td-compositor: {error}");
            }
            let described = format!("{} {lit}", describe_output("kms", &kms));
            (Runtime::new(kms), Some(flips), described)
        }
    };
    runtime.enable_attention(options.terminal_authority);
    runtime.set_text(text::load(
        env::var_os(face_file::SETTING).as_deref(),
        options.terminal_authority,
    ));
    runtime.set_launcher_application(options.launcher_application.as_deref());
    runtime.set_launcher_authority(options.terminal_authority);
    if let Some((((path, app_id), content_rgb_a), content_rgb_b)) = options
        .application_ready_socket
        .as_deref()
        .zip(options.application_app_id.as_deref())
        .zip(options.application_content_rgb_a.as_deref())
        .zip(options.application_content_rgb_b.as_deref())
    {
        let observer =
            server::watch_application(path, app_id, content_rgb_a, content_rgb_b, socket_policy)?;
        runtime.watch_application_with_cursor(
            app_id.to_string(),
            observer.content_rgbs,
            observer.content_wake,
            observer.cursor_wake,
            observer.connection_live,
        )?;
    }
    let runtime = Arc::new(Mutex::new(runtime));
    // Before the first paint, so the flip it queues has a reader: a
    // completion that arrived first would wait in the card's queue, but a
    // reader that failed to start must stop the compositor before it draws.
    if let Some(flips) = flips {
        start_flip_threads(flips, &runtime)?;
    }
    runtime
        .lock()
        .map_err(|_| "runtime lock poisoned".to_string())?
        .repaint()?;
    let inputs = input::start(&options.input, Arc::clone(&runtime), launches)?;
    // FATAL, like the Wayland listeners and unlike the status bar below. An
    // earlier draft reasoned from the bar — a session you can see beats one
    // that refused to start — and it was the wrong analogy: the bar is a
    // decoration, and this is an endpoint whose NAME the session hands to
    // every program it starts. `remove_stale` already clears a socket nobody
    // is answering, so the only way past it is a path something LIVE owns,
    // and carrying on there would advertise that path while somebody else
    // answered on it. A caller was then told, with authority, whatever the
    // incumbent said.
    if let Some(path) = options.control_socket.as_deref() {
        control::serve(path, Arc::clone(&runtime), socket_policy)?;
    }
    if let Err(error) = vm_bridge::start(Arc::clone(&runtime), task_launcher) {
        eprintln!("td-compositor: VM bridge unavailable: {error}");
    }
    // Reported, never fatal: a compositor without a clock is worth more
    // than no compositor.
    if let Err(error) = bar::start(
        Arc::clone(&runtime),
        PathBuf::from("/proc"),
        PathBuf::from("/sys"),
    ) {
        eprintln!("td-compositor: {error}");
    }
    eprintln!("td-compositor: {described} inputs={inputs}");
    server::serve(
        &options.socket,
        &options.portal_socket,
        runtime,
        display_policy,
    )
}

/// One line for the startup diagnostic: the backend's kind, geometry, and
/// what it can put on glass as DRM fourccs. The formats are a property of the
/// BACKEND rather than of td, so fbdev and KMS on one machine may differ, and
/// the list is the first thing to look at when a format is refused.
fn describe_output(kind: &str, backend: &dyn OutputBackend) -> String {
    let size = backend.dimensions();
    let scanout: Vec<String> = backend
        .supported_formats()
        .iter()
        .map(|format| {
            let code = format.code();
            // A fourcc that is not four printable characters is still a
            // number worth reading, and hex is the form every DRM header
            // writes it in.
            std::str::from_utf8(&code)
                .map(String::from)
                .unwrap_or_else(|_| format!("{:#010x}", u32::from_le_bytes(code)))
        })
        .collect();
    format!(
        "{kind} output {}x{} stride={} scanout={}",
        size.width,
        size.height,
        backend.target_stride(),
        scanout.join(",")
    )
}

/// The KMS backend's boot evidence, printed once the modeset `open_kms` made
/// has answered: what it lit, and the geometry the runtime renders at.
fn announce_kms_ready(
    out: &mut impl Write,
    lit: &str,
    backend: &dyn OutputBackend,
) -> Result<(), String> {
    let size = backend.dimensions();
    let line = format!(
        "\nTD-COMPOSITOR-KMS-READY {lit} output={}x{} stride={}\n",
        size.width,
        size.height,
        backend.target_stride()
    );
    write_marker(out, &line)
}

/// One write that begins a line of its own. The console is shared with
/// unbuffered stderr, this process's and others', whose fragments need not
/// end a line, and the boot check reads a marker only at a line's start.
fn write_marker(out: &mut impl Write, line: &str) -> Result<(), String> {
    out.write_all(line.as_bytes())
        .and_then(|()| out.flush())
        .map_err(|error| format!("write boot marker: {error}"))
}

/// The first page flip the runtime accepted as on glass, printed once. The
/// cookie is the completion's own `user_data`, carried back by the kernel.
fn announce_first_flip(out: &mut impl Write, frame: output::FrameId) -> Result<(), String> {
    let line = format!(
        "\nTD-COMPOSITOR-FLIP-OK cookie={:#x} flip=ok\n",
        frame.cookie()
    );
    write_marker(out, &line)
}

/// How often the watchdog looks at the frame in flight. A fifth of the
/// deadline, so a stalled flip is recovered within 1.2 deadlines at worst.
const FLIP_WATCHDOG_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

/// The KMS backend's two threads: one blocks on the card for completions
/// and delivers each under the runtime lock, the other bounds the wait for
/// a completion that never comes.
///
/// An error from either ends the process. The reader's are a card it cannot
/// read and a completion the runtime cannot place; the watchdog's is a
/// recovery that failed several ticks in a row. Each means the compositor
/// can no longer learn when a frame reached the screen, so it can never
/// queue another and would stand frozen while looking alive. Exiting hands
/// the display back to the supervisor, and the kernel releases mastership
/// with the descriptors. A failed PAINT is not among them: it stays owed.
///
/// The reader also prints the boot's flip marker, once, after the first
/// completion the runtime accepts, and outside the runtime lock. A marker it
/// cannot write is logged, not fatal: the display is working.
fn start_flip_threads(
    mut flips: drm::FlipEvents,
    runtime: &Arc<Mutex<Runtime>>,
) -> Result<(), String> {
    let completions = Arc::clone(runtime);
    std::thread::Builder::new()
        .name("td-kms-flips".into())
        .spawn(move || {
            let mut announced = false;
            loop {
                let delivered = flips.next().and_then(|frame| {
                    completions
                        .lock()
                        .map_err(|_| "runtime lock poisoned".to_string())?
                        .output_event(output::OutputEvent::Presented(frame))?;
                    Ok(frame)
                });
                match delivered {
                    Ok(frame) if !announced => {
                        announced = true;
                        if let Err(error) =
                            announce_first_flip(&mut std::io::stdout().lock(), frame)
                        {
                            eprintln!("td-compositor: {error}");
                        }
                    }
                    Ok(_) => {}
                    Err(error) => {
                        eprintln!("td-compositor: page-flip completion: {error}");
                        process::exit(1);
                    }
                }
            }
        })
        .map_err(|error| format!("start the page-flip reader: {error}"))?;
    let watched = Arc::clone(runtime);
    std::thread::Builder::new()
        .name("td-kms-watchdog".into())
        .spawn(move || loop {
            std::thread::sleep(FLIP_WATCHDOG_INTERVAL);
            let checked = watched
                .lock()
                .map_err(|_| "runtime lock poisoned".to_string())
                .and_then(|mut runtime| runtime.output_watchdog(std::time::Instant::now()));
            if let Err(error) = checked {
                eprintln!("td-compositor: page-flip watchdog: {error}");
                process::exit(1);
            }
        })
        .map_err(|error| format!("start the page-flip watchdog: {error}"))?;
    Ok(())
}

fn selftest() -> Result<(), String> {
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
        buffer::Surface::from_shm_pixels(1, 1, vec![1, 2, 3, 0], scene::SHM_XRGB8888)?,
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
    writeln!(out, "TD-COMPOSITOR-SELFTEST-OK")
        .map_err(|e| format!("write compositor selftest marker: {e}"))?;
    Ok(())
}

fn run(args: &[String]) -> Result<(), String> {
    let command = args.first().ok_or_else(usage)?;
    match command.as_str() {
        "probe-terminal-authority" => {
            if args.len() != 1 {
                return Err(usage());
            }
            session::probe_terminal_authority()
        }
        "run" => run_compositor(parse_run(args.get(1..).ok_or_else(usage)?)?),
        "headless" => headless::run(args.get(1..).ok_or_else(usage)?, std::io::stdin()),
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
        "selftest" => {
            if args.get(1).is_some() {
                return Err(usage());
            }
            selftest()
        }
        _ => Err(usage()),
    }
}

/// The demo client's two run flags. td-term spells the same two the same
/// way, so a launcher builds either command from one pair of paths.
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

/// Where `td-ctl` looks for the session it is driving. An environment
/// variable rather than a fixed path, for the reason `WAYLAND_DISPLAY` is
/// one: the socket belongs to a session, and a machine can have a session
/// whose runtime directory is not the one a constant would name.
const CONTROL_SOCKET_ENV: &str = "TD_CONTROL_SOCKET";

/// `td-ctl`'s outcome for one argument vector. An argument that is not
/// UTF-8 is the caller's mistake, so it is a refusal (exit 2) like any other
/// wrong request, not the "session unavailable" status (exit 1) that the
/// shared handler in `main` gives the other personalities' failures.
fn control_outcome(args: &[OsString]) -> Result<(), control::ControlFailure> {
    let args = utf8_args(args).map_err(control::ControlFailure::Refused)?;
    run_control(&args)
}

/// `td-ctl`: one request, one answer, exit.
///
/// The request is parsed HERE as well as by the compositor. Not a duplicate
/// check — it is the same parser — but it is what lets a typo be answered
/// without a session to answer it, which is the difference between `td-ctl`
/// being usable over ssh into a machine whose compositor has died and it
/// being a program that reports a socket error for a misspelt verb.
fn run_control(args: &[String]) -> Result<(), control::ControlFailure> {
    let mut request_words = args;
    let mut socket = None;
    if args.first().is_some_and(|flag| flag == "--socket") {
        let value = args.get(1).ok_or_else(|| {
            control::ControlFailure::Refused("--socket requires a value".to_string())
        })?;
        socket = Some(PathBuf::from(value));
        request_words = args.get(2..).unwrap_or_default();
    }
    let line = request_words.join(" ");
    if line.is_empty() || line == "help" || line == "--help" {
        return say(&control::help());
    }
    // Refused rather than unreachable: the request is wrong wherever it was
    // read, and a script branching on the status wants "fix the command"
    // separated from "there is no compositor".
    let request = control::Request::parse(&line).map_err(control::ControlFailure::Refused)?;
    let socket = match socket {
        Some(socket) => socket,
        None => env::var_os(CONTROL_SOCKET_ENV)
            .map(PathBuf::from)
            .ok_or_else(|| {
                control::ControlFailure::Unreachable(format!(
                    "no control socket: set {CONTROL_SOCKET_ENV} or pass --socket"
                ))
            })?,
    };
    if request == control::Request::Capture {
        return say_bytes(&control::ask_capture(&socket)?);
    }
    if request == control::Request::Observe {
        return say(&control::ask_observe(&socket)?);
    }
    let body = control::ask(&socket, request)?;
    say(&body)
}

/// Write to stdout without panicking. `print!` panics when the write fails and
/// this crate is `panic = "abort"` in release, so `td-ctl layout | head -1`
/// would abort rather than report — and a control client that dies on a closed
/// pipe is worse than one that says it could not write. The failure is
/// UNREACHABLE rather than a refusal: the compositor answered, and what went
/// wrong is on this side of it.
fn say(text: &str) -> Result<(), control::ControlFailure> {
    say_bytes(text.as_bytes())
}

fn say_bytes(bytes: &[u8]) -> Result<(), control::ControlFailure> {
    let mut out = std::io::stdout().lock();
    out.write_all(bytes)
        .and_then(|()| out.flush())
        .map_err(|error| {
            control::ControlFailure::Unreachable(format!("write control output: {error}"))
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
    // Bytes, not text: `env::args()` panics on a non-UTF-8 argument. Each
    // personality decides what must be UTF-8.
    let mut argv = env::args_os();
    let executable = argv
        .next()
        .map(|word| word.to_string_lossy().into_owned())
        .unwrap_or_default();
    let args: Vec<OsString> = argv.collect();
    let jail_fixture =
        env::var_os("FLATPAK_ID").as_deref() == Some(client::JAIL_FIXTURE_ID.as_ref());
    let personality = Personality::of(&executable, jail_fixture);
    let result = match personality {
        Personality::Compositor => utf8_args(&args).and_then(|args| run(&args)),
        Personality::Demo => match utf8_args(&args) {
            Err(error) => Err(error),
            Ok(args) => match run_client(&args) {
                Ok(()) => Ok(()),
                Err(error) => {
                    eprintln!("{}: {}", personality.program(), error.message());
                    process::exit(error.exit_code());
                }
            },
        },
        // td-ctl's vocabulary is text; `control_outcome` decides that for
        // this personality, and a word that cannot be a request is refused
        // the way any wrong request is.
        Personality::Control => match control_outcome(&args) {
            Ok(()) => Ok(()),
            Err(error) => {
                eprintln!("{}: {}", personality.program(), error.message());
                process::exit(error.exit_code());
            }
        },
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
    fn terminal_authority_probe_belongs_to_the_compositor_personality() {
        let result = run(&["probe-terminal-authority".into()]);
        assert_ne!(
            result,
            Err(usage()),
            "the image command must reach the probe"
        );
        assert_eq!(
            run(&["probe-terminal-authority".into(), "extra".into()]),
            Err(usage())
        );
    }

    /// The terminal is its own program now; its installer subcommand is not
    /// a word the compositor answers.
    #[test]
    fn the_compositor_no_longer_serves_the_terminal() {
        assert_eq!(run(&["terminfo".into(), "/x".into()]), Err(usage()));
        assert!(!usage().contains("terminfo"));
        assert!(Personality::of("/bin/td-term", false) == Personality::Compositor);
    }

    #[test]
    fn a_non_utf8_control_argument_is_a_refusal_not_an_unavailable_session() {
        use std::os::unix::ffi::OsStringExt;
        let raw = OsString::from_vec(vec![b'w', b'i', b'n', 0xff]);
        let error = control_outcome(&[raw]).expect_err("a non-UTF-8 word is no request");
        assert_eq!(error.exit_code(), 2, "{}", error.message());
        assert!(error.message().contains("not UTF-8"), "{}", error.message());
    }

    #[test]
    fn the_three_names_pick_the_three_programs() {
        // Installed symlinks select by basename; only the exact fixture entry
        // also uses its authenticated application identity.
        for (name, personality) in [
            ("td-compositor", Personality::Compositor),
            ("td-ui-demo", Personality::Demo),
            ("td-ctl", Personality::Control),
        ] {
            // The name a personality PRINTS itself as has to be the name that
            // selects it, or a diagnostic names a program the reader cannot
            // run. Every one of the three, since the loop is the check.
            assert_eq!(personality.program(), name);
            assert!(Personality::of(&format!("/bin/{name}"), false) == personality);
        }
        assert!(Personality::of("/bin/td-ctl", false) == Personality::Control);
        assert!(Personality::of("td-ctl", false) == Personality::Control);
        assert!(
            Personality::of("/td/store/x-td-compositor/bin/td-ctl", false) == Personality::Control
        );
        assert!(Personality::of("/bin/td-ctl", true) == Personality::Control);
        // A name that merely contains one is not that one, here as elsewhere:
        // the control client must not be selected by `td-ctlx` or by a
        // `td-ctl` that is only the tail of a longer name.
        assert!(Personality::of("/bin/td-ctlx", false) == Personality::Compositor);
        assert!(Personality::of("/bin/xtd-ctl", false) == Personality::Compositor);
        assert!(Personality::of("/bin/td-ui-demo", false) == Personality::Demo);
        assert!(Personality::of("/bin/td-compositor", false) == Personality::Compositor);
        assert!(Personality::of(client::JAIL_FIXTURE_ENTRY, true) == Personality::Demo);
        assert!(Personality::of("/bin/td-compositor", true) == Personality::Compositor);
        assert!(Personality::of("/bin/td-ui-demo", true) == Personality::Demo);
        // An unknown name is the compositor.
        assert!(Personality::of("/bin/something-else", false) == Personality::Compositor);
        assert!(Personality::of("", false) == Personality::Compositor);
        // A name that merely CONTAINS one is not that one.
        assert!(Personality::of("/bin/td-ui-demox", false) == Personality::Compositor);
        assert!(Personality::of("/bin/td-ui-demo/", false) == Personality::Demo);
        for personality in [
            Personality::Compositor,
            Personality::Demo,
            Personality::Control,
        ] {
            assert_eq!(Personality::of(personality.program(), false), personality);
        }
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
        assert_eq!(ready_socket.parent(), Some(Path::new("/run/user/1000")));
        let ready_name = parsed.ready_socket.file_name().unwrap().to_string_lossy();
        assert!(ready_name.starts_with("td-launcher-"));
        assert!(ready_name.ends_with("-7.ready"));

        // An ordinary terminal keeps the two common flags, and the task
        // terminal adds one terminal-only absolute directory. td-term is its
        // own program, so its argv is pinned here as the words td-term's
        // `run` parses.
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
        // The usage string is hand-written and the parser is not, so the
        // thing that can drift is what it TELLS an operator.
        let flags = "run --socket PATH --ready-socket PATH";
        assert!(client_usage().contains(flags), "{}", client_usage());
    }
}

#[cfg(test)]
#[cfg(not(feature = "target-recipe"))]
mod confinement {
    const IMPORTERS: &[(&str, &str)] = &[(
        "import-unifont.rs",
        include_str!("../tools/import-unifont.rs"),
    )];
    const MAIN: &str = include_str!("main.rs");
    const SHARED_SHA256: &str = include_str!("../../engine/src/sha256.rs");
    const SYS: &str = include_str!("sys.rs");
    const DRM: &str = include_str!("drm.rs");
    const AUTHORITY_FINGERPRINT: u64 = 0x7b2d03c877bb41a2;
    const AUTH_SYS_FINGERPRINT: u64 = 0x42363c39df98214d;
    const AUTH_CHANNEL_FINGERPRINT: u64 = 0xdf20e4130b2d96e2;
    const AUTHORITY: &str = include_str!("authority.rs");
    const AUTH_CHANNEL: &str = include_str!("../../td-authd/src/channel.rs");
    const AUTH_SYS: &str = include_str!("../../td-authd/src/sys.rs");

    /// Sources another crate owns, mounted by path rather than kept in src.
    const MOUNTED: &[&str] = &[
        "app_policy.rs",
        "atlas.rs",
        "coverage.rs",
        "face.rs",
        "face_file.rs",
        "sfnt.rs",
        "timezone.rs",
    ];

    const OTHER: &[(&str, &str)] = &[
        (
            "app_policy.rs",
            include_str!("../../td-busd/src/app_policy.rs"),
        ),
        ("atlas.rs", include_str!("../../td-ui/src/atlas.rs")),
        ("attention.rs", include_str!("attention.rs")),
        ("authority.rs", AUTHORITY),
        ("bar.rs", include_str!("bar.rs")),
        ("buffer.rs", include_str!("buffer.rs")),
        ("client.rs", include_str!("client.rs")),
        ("client_resources.rs", include_str!("client_resources.rs")),
        ("clock.rs", include_str!("clock.rs")),
        ("configure.rs", include_str!("configure.rs")),
        ("conn.rs", include_str!("conn.rs")),
        ("control.rs", include_str!("control.rs")),
        ("coverage.rs", include_str!("../../td-ui/src/coverage.rs")),
        ("drm.rs", include_str!("drm.rs")),
        ("face.rs", include_str!("../../td-ui/src/face.rs")),
        ("face_file.rs", include_str!("../../td-ui/src/face_file.rs")),
        ("filter.rs", include_str!("filter.rs")),
        ("font.rs", include_str!("font.rs")),
        ("font_data.rs", include_str!("font_data.rs")),
        ("framebuffer.rs", include_str!("framebuffer.rs")),
        ("headless.rs", include_str!("headless.rs")),
        ("help.rs", include_str!("help.rs")),
        ("input.rs", include_str!("input.rs")),
        ("keyboard.rs", include_str!("keyboard.rs")),
        ("launcher.rs", include_str!("launcher.rs")),
        ("layout.rs", include_str!("layout.rs")),
        ("output.rs", include_str!("output.rs")),
        ("pointer.rs", include_str!("pointer.rs")),
        ("positioner.rs", include_str!("positioner.rs")),
        ("proc_status.rs", include_str!("proc_status.rs")),
        ("reportable.rs", include_str!("reportable.rs")),
        ("runtime.rs", include_str!("runtime.rs")),
        ("scene.rs", include_str!("scene.rs")),
        ("secret_client.rs", include_str!("secret_client.rs")),
        ("server.rs", include_str!("server.rs")),
        ("session.rs", include_str!("session.rs")),
        ("sfnt.rs", include_str!("../../td-ui/src/sfnt.rs")),
        ("socket.rs", include_str!("socket.rs")),
        ("text.rs", include_str!("text.rs")),
        ("timezone.rs", include_str!("../../td-civil/src/tzif.rs")),
        ("ui.rs", include_str!("ui.rs")),
        ("vm_bridge.rs", include_str!("vm_bridge.rs")),
        ("vm_wire.rs", include_str!("vm_wire.rs")),
        ("wire.rs", include_str!("wire.rs")),
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

    /// td-term names its own window's app id; the authority probe recognises
    /// the terminal by it, so the compositor's copy is pinned to it.
    #[test]
    fn the_authority_probe_names_the_app_id_td_term_sets() {
        let app = include_str!("../../td-term/src/app.rs");
        let app_id = format!(
            "const APP_ID: &str = \"{}\";",
            crate::session::TERMINAL_APP_ID
        );
        assert!(app.contains(&app_id), "td-term no longer spells {app_id}");
    }

    #[test]
    fn private_authority_startup_and_shared_transport_have_one_owner() {
        let fingerprint = |source: &str| {
            source.bytes().fold(0xcbf29ce484222325u64, |hash, byte| {
                (hash ^ byte as u64).wrapping_mul(0x100000001b3)
            })
        };
        assert_eq!(
            fingerprint(include_str!("../../td-authd/src/consent.rs")),
            0x0c9abd729126791a,
            "shared consent changed: reconcile td-secret/src/lib.rs, td-authd/tests/confinement.rs and this pin"
        );
        assert_eq!(fingerprint(AUTHORITY), AUTHORITY_FINGERPRINT);
        assert_eq!(
            fingerprint(AUTH_CHANNEL),
            AUTH_CHANNEL_FINGERPRINT,
            "shared channel changed: reconcile td-authd/tests/confinement.rs and this pin"
        );
        assert_eq!(
            fingerprint(AUTH_SYS),
            AUTH_SYS_FINGERPRINT,
            "shared sys changed: reconcile td-authd/tests/confinement.rs and this pin"
        );
        assert!(AUTHORITY.starts_with("#![deny(unsafe_code)]"));
        assert_eq!(AUTHORITY.matches("mod channel;").count(), 1);
        assert_eq!(AUTHORITY.matches("mod sys;").count(), 1);
        assert_eq!(AUTHORITY.matches("path =").count(), 6);
        assert_eq!(AUTHORITY.matches("mod consent;").count(), 1);
        assert_eq!(AUTH_SYS.matches("unsafe").count(), 4);
        assert_eq!(AUTH_SYS.matches("#[allow(unsafe_code)]").count(), 2);
        assert_eq!(AUTH_SYS.matches("const SYS_").count(), 4);
        for (name, source) in OTHER {
            // Negative scan: modules without tests are scanned in full.
            let source = source
                .split_once("\n#[cfg(test)]\nmod tests {")
                .map_or(*source, |(body, _)| body);
            for forbidden in ["io::stdin(", "fd/0", "from_raw_fd(0"] {
                assert!(
                    !source.contains(forbidden),
                    "{name}: inherited authority stdin access"
                );
            }
        }
        // Only the disjoint headless personality receives an ordinary stdin
        // reader. No module may independently acquire the authority endpoint.
        assert_eq!(production(MAIN).matches("std::io::stdin()").count(), 1);
        assert!(production(MAIN).contains(
            "\"headless\" => headless::run(args.get(1..).ok_or_else(usage)?, std::io::stdin()),"
        ));
        let client = production(AUTHORITY);
        for absent in [
            "::Command",
            "pre_exec",
            ".exec(",
            "println!",
            "eprintln!",
            "sys::",
        ] {
            assert!(!client.contains(absent), "{absent}");
        }
        assert_eq!(client.matches(".spawn(").count(), 1);
        assert_eq!(client.matches("Channel::from_stdin(0)").count(), 1);
        assert_eq!(client.matches("std::process::exit(1)").count(), 1);
        let greeting = client.find("Channel::from_stdin(0)").unwrap();
        assert!(greeting < client.find("std::thread::Builder").unwrap());
        let run = MAIN
            .split_once("fn run_compositor(")
            .unwrap()
            .1
            .split_once("fn selftest(")
            .unwrap()
            .0;
        assert!(
            run.find("authority::Launcher::connect()").unwrap()
                < run.find("Framebuffer::open").unwrap()
        );
        assert!(
            run.find("authority::Launcher::connect()").unwrap()
                < run.find("drm::open_kms").unwrap()
        );
        assert!(
            run.find("authority::Launcher::connect()").unwrap() < run.find("input::start").unwrap()
        );
        // The prepared session binds a live boot's setup intake, so the
        // wizard, started once the display probe answers, finds it.
        assert!(
            run.find("authority::Launcher::connect()").unwrap()
                < run.find("server::serve(").unwrap()
        );
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

    /// Four scoped `unsafe` bodies plus the mapping's `Send`, and the fourth
    /// body is a different CLASS from the other three.
    ///
    /// `syscall5`, `syscall6` and the descriptor adoption are instruction
    /// one-shots: the unsafe begins at an instruction and ends when it returns,
    /// and what returns is a number that safe code then owns. `bytes_mut` is
    /// not. It lends a slice over kernel-owned memory which outlives the call
    /// and crosses a module boundary, because the renderer writes pixels INTO
    /// the mapping — the lifetime-carrying class `UNSAFE.md` §6 budgeted before
    /// any of it existed. Pinning each body by its exact text is what stops a
    /// fifth from arriving quietly beside them.
    ///
    /// The fifth ALLOWANCE is not a body. The KMS backend lives in the
    /// runtime, which crosses threads behind its lock, so the region that
    /// owns a mapping must be `Send`; a raw pointer field makes it `!Send`
    /// by default. The impl is pinned whole, and `Sync` is pinned absent:
    /// a shared reference lends nothing, so nothing needs it.
    #[test]
    fn scoped_unsafe_is_the_syscalls_the_adoption_the_mapping_and_its_send() {
        let syscall_body = r#"#[allow(unsafe_code)]
fn syscall5(number: usize, a1: usize, a2: usize, a3: usize, a4: usize, a5: usize) -> isize {
    let result: isize;
    unsafe {
        core::arch::asm!(
            "syscall",
            inlateout("rax") number as isize => result,
            in("rdi") a1,
            in("rsi") a2,
            in("rdx") a3,
            in("r10") a4,
            in("r8") a5,
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
        let syscall6_body = r#"#[allow(unsafe_code)]
fn syscall6(
    number: usize,
    a1: usize,
    a2: usize,
    a3: usize,
    a4: usize,
    a5: usize,
    a6: usize,
) -> isize {
    let result: isize;
    unsafe {
        core::arch::asm!(
            "syscall",
            inlateout("rax") number as isize => result,
            in("rdi") a1,
            in("rsi") a2,
            in("rdx") a3,
            in("r10") a4,
            in("r8") a5,
            in("r9") a6,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack, preserves_flags),
        );
    }
    result
}"#;
        let mapping_body = r#"    #[allow(unsafe_code)]
    pub fn bytes_mut(&mut self) -> &mut [u8] {
        // SAFETY: `drm_map_dumb` is the sole constructor and returns only after
        // `mmap` answered a live mapping of exactly `length` bytes at
        // `address`. `Drop` is the only unmap and cannot have run while this
        // `&mut self` exists, and the returned borrow cannot outlive it.
        unsafe { core::slice::from_raw_parts_mut(self.address, self.length) }
    }"#;
        let send_impl = r#"#[allow(unsafe_code)]
unsafe impl Send for MappedRegion {}"#;
        assert_eq!(occurrences(SYS, "#[allow(unsafe_code)]"), 5);
        assert_eq!(occurrences(SYS, "unsafe {"), 4);
        assert_eq!(occurrences(SYS, "unsafe impl"), 1);
        assert_eq!(occurrences(SYS, send_impl), 1);
        assert!(!SYS.contains("impl Sync"));
        assert_eq!(occurrences(SYS, "core::arch::asm!"), 2);
        assert_eq!(occurrences(SYS, syscall_body), 1);
        assert_eq!(occurrences(SYS, syscall6_body), 1);
        assert_eq!(occurrences(SYS, adoption_body), 1);
        assert_eq!(occurrences(SYS, mapping_body), 1);
        assert_eq!(occurrences(SYS, "File::from_raw_fd("), 1);
        // The ONE way a pointer becomes a slice, and it is inside the mapping
        // body above. A second would be a second lifetime-carrying region with
        // no type owning its unmap.
        assert_eq!(occurrences(SYS, "core::slice::from_raw_parts_mut("), 1);
        for syscall in [
            "const SYS_CLOSE: usize = 3;",
            "const SYS_MMAP: usize = 9;",
            "const SYS_MUNMAP: usize = 11;",
            "const SYS_IOCTL: usize = 16;",
            "const SYS_SENDMSG: usize = 46;",
            "const SYS_RECVMSG: usize = 47;",
            "const SYS_GETSOCKOPT: usize = 55;",
            "const SYS_FCNTL: usize = 72;",
            "const SYS_CLOCK_GETTIME: usize = 228;",
        ] {
            assert!(SYS.contains(syscall), "{syscall}");
        }
        assert_eq!(occurrences(SYS, "const SYS_"), 9);
        for (name, source) in OTHER {
            assert_eq!(
                source.matches("unsafe").count(),
                usize::from(*name == "authority.rs"),
                "{name} changed its permitted denial-only keyword count"
            );
            assert!(
                !source.contains("core::arch::asm!"),
                "{name} introduced another raw syscall body"
            );
        }
    }

    #[test]
    fn received_files_share_the_existing_exact_adoption_surface() {
        assert!(production(SYS).contains(
            r#"pub fn take_received(fd: RawFd) -> Result<File, String> {
    ReceivedFd::adopt(fd).map(ReceivedFd::into_file)
}"#
        ));
        assert_eq!(production(SYS).matches("ReceivedFd::adopt(fd)").count(), 1);
        assert_eq!(production(SYS).matches("ReceivedFd::into_file").count(), 1);
        for (name, source) in OTHER {
            let expected = usize::from(matches!(*name, "client.rs" | "conn.rs" | "server.rs"));
            assert_eq!(
                source
                    .split_once("\n#[cfg(test)]\nmod tests {")
                    .map_or(*source, |(body, _)| body)
                    .matches("sys::take_received(")
                    .count(),
                expected,
                "{name} changed the received-file adoption roster"
            );
        }
        let client = production(include_str!("client.rs"));
        assert!(client.contains(
            "let fd = received\n        .fds\n        .pop()\n        .ok_or_else(|| \"demo descriptor transport returned no fd\".to_string())?;\n    let file = sys::take_received(fd)?;"
        ));
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
        let entry = r#"    errno_result(
        syscall5(SYS_FCNTL, fd as usize, command, argument, 0, 0),
        operation,
    )"#;
        assert_eq!(occurrences(SYS, guard), 1);
        assert_eq!(occurrences(SYS, entry), 1);
        assert_eq!(occurrences(SYS, "fn fcntl("), 1);
        assert_eq!(
            occurrences(production(SYS), "fcntl(file.as_raw_fd(), F_GETFL, 0"),
            1
        );
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
        for wrapper in ["pub fn make_nonblocking(", "pub fn restore_status_flags("] {
            assert_eq!(occurrences(SYS, wrapper), 1, "{wrapper}");
        }
    }

    #[test]
    fn the_peer_credential_surface_is_one_exact_uid_query() {
        for declaration in [
            "const SOL_SOCKET: i32 = 1;",
            "const SO_PEERCRED: i32 = 17;",
            "let mut credentials = [0u32; 3];",
            "if length != expected {",
            ".get(1)\n        .copied()",
        ] {
            assert!(SYS.contains(declaration), "{declaration}");
        }
        assert_eq!(occurrences(SYS, "pub fn peer_uid("), 1);
        assert_eq!(occurrences(production(SYS), "SO_PEERCRED"), 7);
        assert_eq!(occurrences(production(SYS), "SOL_SOCKET as usize"), 1);
        assert_eq!(occurrences(production(SYS), "as *mut [u32; 3]"), 1);
        assert_eq!(occurrences(production(SYS), "as *mut u32"), 1);
    }

    /// The person's account is the installed one, whatever its name: no
    /// source names the stock VM's home. Whole files, since production
    /// modules may follow a test module (vm_wire.rs), and the needle is
    /// built so this test does not name it.
    #[test]
    fn no_source_names_the_stock_account_home() {
        let home = ["/home/", "tester"].concat();
        for (name, source) in [("main.rs", MAIN), ("sys.rs", SYS)]
            .iter()
            .chain(OTHER.iter())
            .chain(IMPORTERS.iter())
        {
            assert!(!source.contains(&home), "{name}");
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
                .filter(|(name, _)| !MOUNTED.contains(name))
                .map(|(name, _)| (*name).to_string()),
        );
        inventoried.sort();
        assert_eq!(inventoried, actual);
    }

    /// `ioctl(2)`'s request number chooses the operation, so the roster is the
    /// confinement: these seventeen values, one allow-list, and sixteen
    /// wrapper functions.
    ///
    /// Four DRM numbers READ a card. `DROP_MASTER` releases authority that
    /// opening a primary node granted without being asked, and `SET_MASTER`
    /// takes it back deliberately, for the KMS backend's life. The dumb trio
    /// allocates, maps and frees. The modeset four register a framebuffer,
    /// read a CRTC, drive one, and unregister.
    ///
    /// `MODE_SETPLANE` and `MODE_ATOMIC` remain deliberately absent, and the
    /// stand-in has moved AGAIN: this increment rosters `MODE_PAGE_FLIP`, which
    /// the last one named here as absent. That movement is the test working
    /// rather than the test churning — a request named absent after it is
    /// admitted asserts nothing while still passing, so each landing that grows
    /// the roster must hand the role to something it genuinely does not issue.
    ///
    /// The two that inherit it are the next two real steps and not arbitrary:
    /// `SETPLANE` puts a second buffer on an overlay plane, which is what
    /// direct scanout of a dmabuf needs (§M row 2), and `ATOMIC` commits a
    /// whole display state at once, which is what replaces this legacy
    /// modeset-and-flip pair when more than one plane is in play.
    #[test]
    fn the_ioctl_surface_is_seventeen_pinned_requests_and_sixteen_wrappers() {
        for request in [
            "const EVIOCGABS_X: usize = 0x8018_4540;",
            "const EVIOCGABS_Y: usize = 0x8018_4541;",
            "const EVIOCSCLOCKID: usize = 0x4004_45a0;",
            "const DRM_IOCTL_VERSION: usize = 0xc040_6400;",
            "const DRM_IOCTL_MODE_GETRESOURCES: usize = 0xc040_64a0;",
            "const DRM_IOCTL_MODE_GETENCODER: usize = 0xc014_64a6;",
            "const DRM_IOCTL_MODE_GETCONNECTOR: usize = 0xc050_64a7;",
            "const DRM_IOCTL_DROP_MASTER: usize = 0x0000_641f;",
            "const DRM_IOCTL_MODE_CREATE_DUMB: usize = 0xc020_64b2;",
            "const DRM_IOCTL_MODE_MAP_DUMB: usize = 0xc010_64b3;",
            "const DRM_IOCTL_MODE_DESTROY_DUMB: usize = 0xc004_64b4;",
            "const DRM_IOCTL_SET_MASTER: usize = 0x0000_641e;",
            "const DRM_IOCTL_MODE_GETCRTC: usize = 0xc068_64a1;",
            "const DRM_IOCTL_MODE_SETCRTC: usize = 0xc068_64a2;",
            "const DRM_IOCTL_MODE_RMFB: usize = 0xc004_64af;",
            "const DRM_IOCTL_MODE_ADDFB2: usize = 0xc068_64b8;",
            "const DRM_IOCTL_MODE_PAGE_FLIP: usize = 0xc018_64b0;",
        ] {
            assert!(SYS.contains(request), "{request}");
        }
        // The terminal's PTY requests left with td-term, which owns them in
        // td-ui's surface; the compositor owns no terminal.
        assert_eq!(occurrences(SYS, "const TIOC"), 0);
        for number in ["0x4004_5431", "0x5441", "0x5414", "0x5413"] {
            assert!(!production(SYS).contains(number), "{number}");
        }
        assert_eq!(occurrences(SYS, "const EVIOCGABS"), 2);
        assert_eq!(occurrences(SYS, "const DRM_IOCTL_"), 14);
        // No write-side DRM request is DECLARED, and none of their numbers
        // appears at all. The declaration is the thing to forbid rather than
        // the name: `DrmModeInfo`'s own doc says a mode is what `SETCRTC` takes
        // back, which is why it carries the whole 68-byte layout instead of the
        // two fields read today, and a scan that tripped over saying so would
        // be a reason to stop explaining rather than a reason to stop.
        //
        // Listed individually rather than by prefix: the claim is that each of
        // these is absent, and a prefix count of zero would also pass if the
        // naming convention changed. Over the SHIPPED half, because `sys.rs`'s
        // own test issues `SETCRTC` to prove the allow-list refuses it — a pin
        // that moved whenever a test did would pin nothing.
        for absent in [
            "const DRM_IOCTL_MODE_SETPLANE",
            "const DRM_IOCTL_MODE_ATOMIC",
            "0xc030_64b7",
            "0xc038_64bc",
        ] {
            assert!(
                !production(SYS).contains(absent),
                "{absent} is not this increment's"
            );
        }
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
        EVIOCGABS_X
            | EVIOCGABS_Y
            | EVIOCSCLOCKID
            | DRM_IOCTL_VERSION
            | DRM_IOCTL_MODE_GETRESOURCES
            | DRM_IOCTL_MODE_GETENCODER
            | DRM_IOCTL_MODE_GETCONNECTOR
            | DRM_IOCTL_DROP_MASTER
            | DRM_IOCTL_MODE_CREATE_DUMB
            | DRM_IOCTL_MODE_MAP_DUMB
            | DRM_IOCTL_MODE_DESTROY_DUMB
            | DRM_IOCTL_SET_MASTER
            | DRM_IOCTL_MODE_GETCRTC
            | DRM_IOCTL_MODE_SETCRTC
            | DRM_IOCTL_MODE_RMFB
            | DRM_IOCTL_MODE_ADDFB2
            | DRM_IOCTL_MODE_PAGE_FLIP
    ) {"#;
        assert_eq!(occurrences(SYS, guard), 1);
        let entry = r#"    Ok(syscall5(SYS_IOCTL, fd as usize, request, argument, 0, 0))"#;
        assert_eq!(occurrences(SYS, entry), 1);
        assert_eq!(occurrences(SYS, "fn ioctl("), 1);
        assert_eq!(occurrences(SYS, "fn ioctl_checked("), 1);
        assert_eq!(occurrences(SYS, "fn drm_ioctl("), 1);
        let production_sys = production(SYS);
        let prose = occurrences(production_sys, "ioctl(2)");
        let every = occurrences(production_sys, "ioctl(") - prose;
        let drm = occurrences(production_sys, "drm_ioctl(");
        // One definition plus exactly two call sites for the evdev entry
        // point: a third wrapper reusing a pinned request would satisfy every
        // other assertion here.
        assert_eq!(every - drm, 3);
        // One definition plus the seventeen requests the fourteen DRM wrappers
        // issue: two each for the three that ask a count before they ask for
        // data, one for the encoder, whose answer is a fixed-size struct, one
        // each to take and give back mastership, one each to create, map and
        // destroy a dumb buffer, one each to read a CRTC, drive one, register a
        // framebuffer and unregister it, and one to queue a page flip.
        assert_eq!(drm, 18);
        // Both entry points reach the SAME allow-list, and it is defined once.
        // This is the assertion that stops a second `syscall5(SYS_IOCTL, ..)`
        // growing beside the roster instead of behind it.
        assert_eq!(occurrences(production_sys, "ioctl_checked("), 3);
        assert_eq!(occurrences(production_sys, "syscall5(SYS_IOCTL"), 1);
        // The absolute-axis wrapper takes its request from an AXIS rather than
        // being handed one, so the two evdev numbers are unreachable from any
        // call site.
        assert!(SYS.contains("pub fn absolute_info(device: &impl AsRawFd, axis: AbsAxis)"));
        assert_eq!(
            occurrences(
                SYS,
                "        device.as_raw_fd(),\n        request,\n        (&mut words as *mut [i32; ABSINFO_WORDS]) as usize,\n        name,"
            ),
            1
        );
        // The absinfo's twenty-four bytes are an array whose layout the
        // language guarantees, not an attribute nobody can observe: the three
        // leading words are the same type and a slipped index is a
        // well-formed wrong range.
        assert_eq!(occurrences(SYS, "as *mut [i32; ABSINFO_WORDS]"), 1);
        assert!(SYS.contains("fn absinfo(words: [i32; ABSINFO_WORDS]) -> AbsInfo {"));
        // The DRM wrappers that ask a card about itself, each reaching its
        // own request.
        for (wrapper, request) in [
            ("pub fn drm_driver_name(", "DRM_IOCTL_VERSION"),
            ("pub fn drm_resources(", "DRM_IOCTL_MODE_GETRESOURCES"),
            ("pub fn drm_connector(", "DRM_IOCTL_MODE_GETCONNECTOR"),
            ("pub fn drm_encoder(", "DRM_IOCTL_MODE_GETENCODER"),
            ("pub fn drm_drop_master(", "DRM_IOCTL_DROP_MASTER"),
        ] {
            assert!(SYS.contains(wrapper), "{wrapper}");
            // The declaration, the guard arm, and the call that issues it.
            // Counted over the shipped half, as the rosters above are.
            assert!(
                occurrences(production(SYS), request) >= 3,
                "{request} is not reached from {wrapper}"
            );
        }
        // Mastership is RELEASED on the way in, on the one path that opens a
        // card, so the one place it is TAKEN is `open_kms`'s explicit
        // `SET_MASTER`, two syscalls later. An earlier revision asserted the ABSENCE of `DROP_MASTER` as proof of
        // not being master; that had it backwards -- `drm_master_open` grants
        // mastership on the open itself, so the absence pinned that the code
        // could not give back what it had already taken.
        assert!(production(DRM).contains("sys::drm_drop_master(&card)"));
        // The COUNTING call must not be the kernel's force-probe request.
        // `count_modes == 0` re-reads EDID and, in the UAPI header's own words,
        // "can be slow, might cause flickering and the ioctl will block"; the
        // documented counting form is one mode with room for one mode.
        assert!(production(SYS).contains("probe.count_modes = 1;"));
        assert!(production(SYS).contains("probe.modes_ptr = address_of(&mut one_mode);"));
        assert_eq!(occurrences(production(DRM), "OpenOptions::new()"), 1);
        // And the policy module DECLARES no request of its own: the ABI lives
        // in `sys.rs`, so the only requests `drm.rs` can cause are the
        // fourteen DRM requests the allow-list admits, reached through the
        // calls pinned above. It may still NAME one in prose, so the claim is
        // about a declaration.
        assert!(!production(DRM).contains("const DRM_IOCTL"));
        // No raw syscall of its own: named as the two forms that would be one,
        // rather than as the word, which this module's own prose uses to say
        // how briefly it holds mastership.
        assert!(!production(DRM).contains("syscall5("));
        assert!(!production(DRM).contains("syscall6("));
        assert!(!production(DRM).contains("asm!"));
        // Nor does the policy module map anything itself. `ScanoutBuffer` OWNS
        // a `MappedRegion` and lends its bytes on, which is the whole point of
        // the region type: the mapping pair is created and destroyed in one
        // module, and `drm.rs` never names either half.
        assert!(!production(DRM).contains("from_raw_parts"));
        assert!(!production(DRM).contains("SYS_MMAP"));
        assert!(!production(DRM).contains("SYS_MUNMAP"));
    }

    /// The backend unwinds in the one order that works, and that order is
    /// field declaration order.
    ///
    /// The restore is a `SETCRTC`, which is the ONE teardown step that
    /// carries `DRM_MASTER` in the kernel's ioctl table -- `GETCRTC`, `ADDFB2`
    /// and `RMFB` carry no flag (`drm_ioctl.c:674`, `675`, `691`, `692`). So
    /// mastership has to outlive the restore, and is released after it.
    ///
    /// `RMFB` on a framebuffer still being scanned out is not refused:
    /// `drm_framebuffer_remove` disables the CRTCs using it, because "drm ABI
    /// mandates that we remove any deleted framebuffers from active usage".
    /// Unregistering first therefore blanks the CRTC and leaves the restore
    /// to light it again, which is a flicker and not a failure. The CRTC is
    /// declared before the buffers so that does not happen.
    #[test]
    fn the_backend_unwinds_crtc_then_mastership_then_buffers() {
        let drm = production(DRM);
        // Each release is issued from exactly one place: `Registration` for
        // the owned buffers. Mastership is TAKEN once, by `open_kms`, which
        // keeps it for the backend's life.
        assert_eq!(occurrences(drm, "sys::drm_rm_fb("), 1);
        assert_eq!(occurrences(drm, "sys::drm_set_master("), 1);
        // The chain declares its CRTC before its buffers, so the restore runs
        // before any framebuffer it may be scanning out is unregistered; and
        // the CRTC's own drop restores before it gives mastership back.
        let chain = drm
            .find("pub struct SwapChain<C, M> {")
            .and_then(|start| drm.get(start..))
            .expect("drm.rs no longer declares SwapChain");
        let chain = chain
            .get(..chain.find("\n}").unwrap_or(chain.len()))
            .unwrap_or_default();
        let crtc_field = chain
            .find("    crtc: C,")
            .expect("SwapChain lost its CRTC field");
        let buffers_field = chain
            .find("    buffers: [Slot<M>; 2],")
            .expect("SwapChain lost its buffers field");
        assert!(
            crtc_field < buffers_field,
            "SwapChain declares its buffers before its CRTC, so the framebuffers are \
             unregistered while the CRTC still scans one out"
        );
        assert!(!drm.contains("Drop for SwapChain"));
        let teardown = drm
            .find("impl Drop for CardCrtc {")
            .and_then(|start| drm.get(start..))
            .expect("CardCrtc no longer releases in a Drop");
        let teardown = teardown
            .get(..teardown.find("\n}").unwrap_or(teardown.len()))
            .unwrap_or_default();
        let restore_call = teardown
            .find("sys::drm_set_crtc(&*self.card, &self.saved, &mut self.routed)")
            .expect("CardCrtc no longer restores its saved state");
        let master_call = teardown
            .find("sys::drm_drop_master(&*self.card)")
            .expect("CardCrtc no longer gives mastership back");
        assert!(
            restore_call < master_call,
            "CardCrtc gives mastership back before the restore that needs it"
        );
        // The restore's connector set is READ rather than substituted. Pinned
        // as the CALL, not merely as the function's existence: reverting this
        // one line to the selected connector leaves `connectors_on_crtc`
        // defined, leaves every sys call site spelled, and silently restores
        // the bug two reviewers caught -- a second lit sink left dark.
        assert!(
            drm.contains("let routed = connectors_on_crtc(&card, scanout.crtc_id)?;"),
            "open_kms no longer reads the CRTC's real connector routing, so its \
             restore is guessing again"
        );
    }

    /// A scanout buffer is unmapped BEFORE its handle is released, and that
    /// ordering is the compiler's rather than a comment's.
    ///
    /// Rust drops struct fields in declaration order, so `region` before
    /// `registration` means munmap before `RMFB` and `DESTROY_DUMB`. An
    /// `impl Drop for ScanoutBuffer` would invert it: a type's own `drop` runs
    /// BEFORE its fields are dropped. Both halves are pinned because either
    /// one alone permits the other order.
    ///
    /// What is pinned is a PREFERENCE, not a safety property. The kernel
    /// tolerates either order, since a GEM mapping holds its own reference to
    /// the object; `UNSAFE.md` §6 carries the citation and the correction of an
    /// earlier claim that it was required.
    #[test]
    fn a_scanout_buffer_unmaps_before_it_releases_the_handle() {
        let drm = production(DRM);
        assert!(drm.contains("impl Drop for Registration"));
        assert_eq!(occurrences(drm, "sys::drm_destroy_dumb("), 1);
        assert!(
            !drm.contains("impl Drop for ScanoutBuffer"),
            "ScanoutBuffer grew a destructor, which runs before its fields drop"
        );
        let owned = drm
            .find("pub struct ScanoutBuffer {")
            .and_then(|start| drm.get(start..))
            .expect("drm.rs no longer declares ScanoutBuffer");
        let owned = owned
            .get(..owned.find('}').unwrap_or(owned.len()))
            .unwrap_or_default();
        let owned_region = owned
            .find("region: sys::MappedRegion,")
            .expect("ScanoutBuffer no longer holds its mapping by that name");
        let owned_registration = owned
            .find("registration: Registration,")
            .expect("ScanoutBuffer no longer holds its registration by that name");
        assert!(
            owned_region < owned_registration,
            "ScanoutBuffer declares its registration before its mapping"
        );
        let released = drm
            .find("impl Drop for Registration {")
            .and_then(|start| drm.get(start..))
            .expect("Registration no longer releases in a Drop");
        let released = released
            .get(..released.find("\n}").unwrap_or(released.len()))
            .unwrap_or_default();
        let unregister = released
            .find("sys::drm_rm_fb(&*self.card, self.fb_id)")
            .expect("Registration no longer unregisters its framebuffer");
        let destroy = released
            .find("sys::drm_destroy_dumb(&*self.card, self.handle)")
            .expect("Registration no longer frees its handle");
        assert!(
            unregister < destroy,
            "Registration frees the handle before unregistering the framebuffer on it"
        );
    }

    #[test]
    fn syscall_wrapper_is_called_only_by_the_eight_reviewed_operations() {
        let close = r#"errno_result(syscall5(SYS_CLOSE, fd as usize, 0, 0, 0, 0), "close")?"#;
        let receive = r#"syscall5(
            SYS_RECVMSG,
            stream.as_raw_fd() as usize,
            (&mut message as *mut MsgHdr) as usize,
            MSG_CMSG_CLOEXEC as usize,
            0,
            0,
        )"#;
        let send = r#"syscall5(
            SYS_SENDMSG,
            stream.as_raw_fd() as usize,
            (&message as *const MsgHdr) as usize,
            0,
            0,
            0,
        )"#;
        let peer = r#"syscall5(
            SYS_GETSOCKOPT,
            stream.as_raw_fd() as usize,
            SOL_SOCKET as usize,
            SO_PEERCRED as usize,
            (&mut credentials as *mut [u32; 3]) as usize,
            (&mut length as *mut u32) as usize,
        )"#;
        let unmap =
            r#"        let _ = syscall5(SYS_MUNMAP, self.address as usize, self.length, 0, 0, 0);"#;
        let map = r#"            SYS_MMAP,
            0,
            length,
            PROT_READ | PROT_WRITE,
            MAP_SHARED,
            fd as usize,
            offset,"#;
        assert_eq!(occurrences(SYS, "syscall5("), 9);
        // The definition and the ONE call. `mmap` is the only six-argument
        // syscall this crate makes, and a second caller of `syscall6` would be
        // a second mapping with no region type owning its unmap.
        assert_eq!(occurrences(SYS, "syscall6("), 2);
        assert_eq!(occurrences(SYS, "SYS_MMAP"), 2);
        assert_eq!(occurrences(SYS, "SYS_MUNMAP"), 2);
        assert_eq!(occurrences(SYS, unmap), 1);
        assert_eq!(occurrences(SYS, map), 1);
        // The mapping is pinned to a SHARED read/write mapping of one owned
        // card descriptor, which is `UNSAFE.md` §6's pinning for §11's reason:
        // a caller-supplied protection or visibility would make this a general
        // mapping facility rather than the one scanout target it is.
        assert_eq!(occurrences(SYS, "const PROT_READ: usize = 0x1;"), 1);
        assert_eq!(occurrences(SYS, "const PROT_WRITE: usize = 0x2;"), 1);
        assert_eq!(occurrences(SYS, "const MAP_SHARED: usize = 0x1;"), 1);
        // The region is CONSTRUCTED once, and the length it stores is the
        // length `mmap` was called with. `UNSAFE.md` §6 claims this is pinned;
        // a review found that it was not, and that `length: length + 4096`
        // passed the whole suite while handing out a slice 4096 bytes past the
        // mapping. Pinning the literal is what makes the section's claim true:
        // the two `length`s can no longer drift apart silently.
        let construction = r#"    let region = MappedRegion {
        address: address as *mut u8,
        length,
    };"#;
        assert_eq!(occurrences(SYS, construction), 1);
        // `= MappedRegion {` is the CONSTRUCTION, and there is one. The bare
        // name also spells the declaration and two `impl` headers, so counting
        // that would pin the wrong thing.
        assert_eq!(occurrences(production(SYS), "= MappedRegion {"), 1);
        // And it is not clonable. `Copy` the compiler already refuses for a
        // type with `Drop`; `Clone` it would ACCEPT, because `*mut u8` is
        // `Copy`, and a derive would give two owners one mapping and two
        // unmaps. Pinned as the doc line immediately preceding the
        // declaration, so an attribute cannot be inserted between them.
        let declaration = r#"///   and the borrow cannot outlive the guard.
pub struct MappedRegion {
    address: *mut u8,
    length: usize,
}"#;
        assert_eq!(occurrences(SYS, declaration), 1);
        assert_eq!(occurrences(SYS, "SYS_CLOSE"), 2);
        assert_eq!(occurrences(SYS, "SYS_FCNTL"), 2);
        assert_eq!(occurrences(SYS, "SYS_IOCTL"), 2);
        assert_eq!(occurrences(SYS, "SYS_SENDMSG"), 2);
        assert_eq!(occurrences(SYS, "SYS_RECVMSG"), 2);
        assert_eq!(occurrences(SYS, "SYS_GETSOCKOPT"), 2);
        assert_eq!(occurrences(SYS, close), 1);
        assert_eq!(occurrences(SYS, receive), 1);
        assert_eq!(occurrences(SYS, send), 1);
        assert_eq!(occurrences(SYS, peer), 1);
        for operation in [
            "fn close_raw(",
            "fn fcntl(",
            "pub fn peer_uid(",
            "pub fn recv_with_fds(",
            "pub fn send_with_fd(",
            "pub fn send_prefix_with_fd(",
        ] {
            assert!(SYS.contains(operation), "{operation}");
        }
    }

    #[test]
    fn attention_clocks_pin_the_same_clock_and_native_operand_widths() {
        let source = production(SYS);
        for fragment in [
            "const CLOCK_MONOTONIC: i32 = 1;",
            "let clock: i32 = CLOCK_MONOTONIC;",
            "(&clock as *const i32) as usize",
            "let mut words = [0_i64; 2];",
            "(&mut words as *mut [i64; 2]) as usize",
            "let [seconds, nanos] = words;",
        ] {
            assert!(source.contains(fragment), "{fragment}");
        }
        assert_eq!(occurrences(source, "SYS_CLOCK_GETTIME"), 2);
        assert_eq!(occurrences(source, "EVIOCSCLOCKID"), 4);
        let call = "SYS_CLOCK_GETTIME,\n            CLOCK_MONOTONIC as usize,\n            (&mut words as *mut [i64; 2]) as usize,\n            0,\n            0,\n            0,";
        assert!(source.contains(call));
        for (name, module) in OTHER {
            // The transition test also samples the clock after its write.
            let module = if *name == "runtime.rs" {
                production(module)
            } else {
                module
            };
            assert_eq!(
                occurrences(module, "sys::monotonic_time"),
                2 * usize::from(*name == "runtime.rs"),
                "{name}"
            );
            assert_eq!(
                occurrences(module, "sys::input_monotonic_clock"),
                usize::from(*name == "input.rs"),
                "{name}"
            );
        }
        let first = crate::sys::monotonic_time().unwrap();
        assert!(crate::sys::monotonic_time().unwrap() >= first);
        assert!(
            crate::sys::input_monotonic_clock(&std::fs::File::open("/dev/null").unwrap()).is_err()
        );
    }

    /// Pin each syscall family to its reviewed callers: transport endpoints,
    /// evdev, peer admission, DRM, and the shared clock.
    /// Aliases must not hide an additional caller from these scans.
    #[test]
    fn each_confined_operation_is_reachable_only_from_its_own_module() {
        const TRANSPORT: &[&str] = &[
            "sys::send_with_fd(",
            "sys::send_prefix_with_fd(",
            "sys::recv_with_fds(",
            "sys::take_received(",
            "sys::discard_received(",
            "sys::ReceivedFd::adopt(",
            "sys::ReceivedFd::into_file(",
        ];
        assert!(production(MAIN).contains("runtime.enable_attention(options.terminal_authority)"));
        assert_eq!(
            occurrences(
                production(include_str!("runtime.rs")),
                "crate::sys::monotonic_time()"
            ),
            2
        );
        assert_eq!(
            occurrences(
                production(include_str!("input.rs")),
                "sys::input_monotonic_clock(&file)?"
            ),
            1
        );
        assert!(production(include_str!("input.rs")).contains(
            "if attention_enabled {\n            sys::input_monotonic_clock(&file)?;\n        }"
        ));
        assert_eq!(
            occurrences(production(include_str!("runtime.rs")), "sys::"),
            2
        );
        const PEER_AUTH: &[&str] = &["sys::peer_uid("];
        assert!(production(MAIN)
            .contains("session::SocketPolicy::for_authority(options.terminal_authority)"));
        let session = production(include_str!("session.rs"));
        assert_eq!(occurrences(session, "sys::"), 1);
        assert_eq!(occurrences(session, "sys::peer_uid(stream)?"), 1);
        // An absolute pointer's range is asked for where the device file is
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
        // The card is asked four questions and no others, each through the
        // wrapper that owns its request. Pinned as call sites and not merely as
        // a count, for `absolute_info`'s reason one module over: `drm_connector`
        // and `drm_encoder` both take a bare `u32` id, so handing a connector's
        // id to the encoder wrapper type-checks and asks the kernel about
        // whichever object happens to hold that number.
        let drm = production(DRM);
        for call in [
            "let driver = sys::drm_driver_name(card)?;",
            "let resources =\n        sys::drm_resources(card).map_err(|error| format!(\"{error} (driver {driver})\"))?;",
            "let Ok(connector) = sys::drm_connector(card, *connector_id) else {",
            "if let Ok(encoder) = sys::drm_encoder(card, connector.encoder_id) {",
            "let Ok(encoder) = sys::drm_encoder(card, *encoder_id) else {",
            "sys::drm_drop_master(&card)",
            "let listed = sys::drm_resources(card)?;",
            "let Ok(connector) = sys::drm_connector(card, *id) else {",
            "let Ok(encoder) = sys::drm_encoder(card, connector.encoder_id) else {",
            // The backend's twelve.
            "sys::drm_set_master(&card).map_err(|error| {",
            "let saved = sys::drm_get_crtc(&card, scanout.crtc_id)?;",
            "let buffer = sys::drm_create_dumb(&**card, width, height)?;",
            "let region = sys::drm_map_dumb(&**card, &buffer)?;",
            "sys::drm_add_fb(&**card, width, height, buffer.pitch, buffer.handle)?;",
            "let _ = sys::drm_rm_fb(&*self.card, self.fb_id);",
            "let _ = sys::drm_destroy_dumb(&*self.card, self.handle);",
            "sys::drm_page_flip(&*self.card, self.crtc_id, fb_id, cookie)",
            "sys::drm_set_crtc(&*self.card, &wanted, &mut connectors)?;",
            "let live = sys::drm_get_crtc(&*self.card, self.crtc_id)?;",
            "if let Err(error) = sys::drm_set_crtc(&*self.card, &self.saved, &mut self.routed) {",
            "let _ = sys::drm_drop_master(&*self.card);",
        ] {
            assert!(drm.contains(call), "drm.rs no longer spells `{call}`");
        }
        // Twenty-one calls and no twenty-second. Nine are discovery's and
        // the open's: `drop_master` once in `open_card`; `drm_resources`,
        // `drm_connector` and `drm_encoder` read twice for two different
        // questions -- "what could be driven" for discovery and "what is being
        // driven right now" for the restore -- and `drm_encoder` a third time
        // inside `crtc_for`'s fallback; and the driver's name.
        //
        // The backend adds twelve, one per step of its life: take mastership
        // and save the CRTC; allocate, map and register each buffer; flip,
        // set and read back; and restore, give back, unregister and free on
        // the way out. Each of the four release calls is reached from a
        // `Drop` and from nowhere else, which is what makes the unwind a
        // property of field order rather than of a path some early return
        // can miss.
        assert_eq!(occurrences(drm, "sys::drm_"), 21);
        // `drm_add_fb` is pinned by COUNT as well as by spelling, because it
        // is the worst of these to get wrong: four bare `u32`s in a row, so
        // swapping width for height, or pitch for handle, type-checks, passes
        // this whole suite, and fails only on a real card. The backend
        // registers each buffer once, at allocation, and flips between the
        // two registrations for its life.
        assert_eq!(occurrences(drm, "sys::drm_add_fb("), 1);
        assert_eq!(occurrences(drm, "sys::drm_resources("), 2);
        // One queue: the backend's CRTC.
        assert_eq!(occurrences(drm, "sys::drm_page_flip("), 1);
        let production_main = production(MAIN);
        for (name, source) in
            std::iter::once(("main.rs", production_main)).chain(OTHER.iter().copied())
        {
            if matches!(
                name,
                "client.rs"
                    | "conn.rs"
                    | "server.rs"
                    | "input.rs"
                    | "drm.rs"
                    | "session.rs"
                    | "runtime.rs"
            ) {
                continue;
            }
            assert!(
                !source.contains("sys::"),
                "{name} reached a confined syscall outside its reviewed module"
            );
        }
        // A NAMED or ALIASED import defeats the scan above the way a glob does:
        // `use crate::sys as raw;` then `raw::drm_set_crtc(...)` matches
        // nothing it looks for, and a modeset would be reachable from a
        // module that never verified the card it drives. Every reach must be
        // spelled `sys::<wrapper>` where the caller scan can see it — including
        // inside every permitted module, so the module list stays the
        // whole answer to "who can call this".
        for (name, source) in
            std::iter::once(("main.rs", production_main)).chain(OTHER.iter().copied())
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
        // roster on the same footing as who may call the syscall, and a new
        // client joins it by amendment rather than by importing.
        const TRANSPORT_USERS: &[&str] = &["client.rs", "conn.rs"];
        for (name, source) in
            std::iter::once(("main.rs", production_main)).chain(OTHER.iter().copied())
        {
            if TRANSPORT_USERS.contains(&name) {
                continue;
            }
            let mut source = squeezed(source);
            if name == "vm_bridge.rs" {
                assert_eq!(
                    source.matches("conn::write_clipboard(").count(),
                    1,
                    "VM clipboard writes must have one bounded transport entry"
                );
                source = source.replace("conn::write_clipboard(", "");
            }
            for form in [concat!("conn", "::"), concat!("conn", "as")] {
                assert_eq!(
                    source.matches(form).count(),
                    0,
                    "{name} reached the Wayland transport ('{form}')"
                );
            }
        }
        let client = include_str!("client.rs");
        let conn = include_str!("conn.rs");
        let server = include_str!("server.rs");
        // The control listener is STARTED, and by the compositor's own run
        // path. Nothing else can see this: `td-ctl help` needs no session, the
        // recipe compares an `exec=` string, and the crate's socket tests
        // build their own listener — so deleting this call left the whole
        // feature dead in the image with every test green. Pinned in the
        // source, as this crate pins what the compiler cannot see.
        assert!(
            production(MAIN).contains("control::serve(path, Arc::clone(&runtime), socket_policy)?"),
            "run_compositor no longer starts the control listener"
        );
        assert_eq!(
            occurrences(production(server), "sys::peer_uid(&stream)"),
            1,
            "private listener peer authentication must have one kernel query"
        );
        assert!(production(server).contains("static NEXT_CLIENT: AtomicU64 = AtomicU64::new(1)"));
        for (name, source) in
            std::iter::once(("main.rs", production_main)).chain(OTHER.iter().copied())
        {
            // This roster includes generated modules without test sections.
            let source = source.split("\n#[cfg(test)]\nmod tests {").next().unwrap();
            assert_eq!(
                occurrences(source, "TransferEndpoint::from_file("),
                usize::from(name == "vm_bridge.rs"),
                "endpoint construction escaped {name}"
            );
            assert_eq!(
                occurrences(source, ".into_file("),
                usize::from(name == "runtime.rs"),
                "endpoint extraction escaped {name}"
            );
        }
        let accept = production(server)
            .split("fn accept_clients(")
            .nth(1)
            .and_then(|source| source.split("fn serve_client(").next())
            .unwrap();
        assert!(
            accept.find("prepare_client_stream").unwrap()
                < accept.find("ClientPermit::acquire").unwrap(),
            "private peer authentication must precede slot admission"
        );
        assert_eq!(
            occurrences(production(server), "sys::ReceivedFd::adopt("),
            1,
            "server selection descriptor ownership must have one adoption site"
        );
        // The client that received selection endpoints was td-term's, and it
        // left with td-term: the compositor's own clients adopt none.
        assert_eq!(
            occurrences(production(conn), "sys::ReceivedFd::adopt("),
            0,
            "client selection descriptor adoption returned to the transport"
        );
        assert_eq!(
            occurrences(production(conn), "sys::ReceivedFd::into_file("),
            0,
            "client selection endpoint conversion returned to the transport"
        );
        assert_eq!(
            occurrences(production(client), "sys::ReceivedFd::into_file("),
            0,
            "exact endpoint conversion escaped the client transport module"
        );
        assert_eq!(
            occurrences(production(server), "sys::ReceivedFd::into_file("),
            1,
            "server clipboard endpoints must have one exact conversion site"
        );
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
        // Scan every family against every module, including other admitted
        // syscall users. The union of the module roster is not confinement.
        const FAMILIES: &[(&[&str], &[&str])] = &[
            (TRANSPORT, &["client.rs", "conn.rs", "server.rs"]),
            (PEER_AUTH, &["server.rs", "session.rs"]),
            (ABSOLUTE, &["input.rs"]),
            (&["sys::drm_", "sys::parse_drm_event"], &["drm.rs"]),
            (
                &["sys::make_nonblocking", "sys::restore_status_flags"],
                &["conn.rs"],
            ),
            (&["sys::input_monotonic_clock"], &["input.rs"]),
            (&["sys::monotonic_time"], &["runtime.rs"]),
        ];
        for (name, source) in
            std::iter::once(("main.rs", production_main)).chain(OTHER.iter().copied())
        {
            let source = squeezed(source);
            for (operations, permitted) in FAMILIES {
                if permitted.contains(&name) {
                    continue;
                }
                for operation in *operations {
                    let identifier = operation.trim_end_matches('(');
                    assert!(
                        !source.contains(identifier),
                        "{name} reached {identifier} outside its reviewed family"
                    );
                }
            }
        }
    }

    /// The binary's own `selftest` subcommand, which is what the td-ui-test
    /// RECIPE runs — and which nothing in `cargo test` reached before. A
    /// geometry change that made it fail was invisible here until the recipe
    /// gate said so, twenty minutes later.
    #[test]
    fn the_selftest_subcommand_passes() {
        super::selftest().unwrap();
    }

    /// The fallback is taken for a MISSING card and nothing else: a card that
    /// exists is driven, and one that cannot even be inspected is an error
    /// rather than a quiet switch to fbdev. The existence check is asked only
    /// when there is somewhere to fall back to.
    #[test]
    fn the_framebuffer_is_driven_only_when_the_card_is_missing() {
        use super::{select_backend, Backend, OutputDevice};
        use std::io::{Error, ErrorKind};
        use std::path::{Path, PathBuf};

        let card = Path::new("/dev/dri/card0");
        let fb = Path::new("/dev/fb0");
        let with_fallback = OutputDevice::Card {
            card: card.into(),
            fallback: Some(fb.into()),
        };
        assert_eq!(
            select_backend(&with_fallback, |_| Ok(())),
            Ok(Backend::Card(card))
        );
        assert_eq!(
            select_backend(&with_fallback, |_| Err(Error::from(ErrorKind::NotFound))),
            Ok(Backend::Framebuffer(fb))
        );
        let refused = select_backend(&with_fallback, |_| {
            Err(Error::from(ErrorKind::PermissionDenied))
        })
        .unwrap_err();
        assert!(refused.contains("/dev/dri/card0"), "{refused}");
        let asked = |_: &Path| -> std::io::Result<()> { panic!("no fallback, nothing to ask") };
        assert_eq!(
            select_backend(
                &OutputDevice::Card {
                    card: card.into(),
                    fallback: None,
                },
                asked
            ),
            Ok(Backend::Card(card))
        );
        assert_eq!(
            select_backend(&OutputDevice::Framebuffer(PathBuf::from(fb)), asked),
            Ok(Backend::Framebuffer(fb))
        );
    }

    /// The two KMS boot markers, byte for byte: the image's check parses
    /// these lines, so their spelling is an interface.
    #[test]
    fn the_kms_boot_markers_are_one_flushed_line_each() {
        let (chain, _) = crate::drm::testing::chain(4, 3);
        let lit = "driver=virtio_gpu connector=Virtual-1#36 status=connected crtc=35 \
                   encoder=37 mode=4x3@60 name=4x3 preferred=true mm=0x0 fb=40,41 modeset=ok";
        let mut out = Vec::new();
        super::announce_kms_ready(&mut out, lit, &chain).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            format!("\nTD-COMPOSITOR-KMS-READY {lit} output=4x3 stride=16\n")
        );
        let mut out = Vec::new();
        super::announce_first_flip(&mut out, crate::output::FrameId::FIRST.next()).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "\nTD-COMPOSITOR-FLIP-OK cookie=0x2 flip=ok\n"
        );
    }

    #[test]
    fn the_control_socket_is_a_boundary_like_the_other_three() {
        // The flag and the alias walk, neither of which anything else reaches:
        // `td-ctl help` needs no session, the recipe compares an `exec=`
        // string, and the crate's socket tests build their own listener. With
        // the endpoint dropped from the walk, or the flag storing a path of
        // its own, every other test in this crate stayed green.
        use std::os::unix::fs::symlink;
        use std::path::Path;
        use std::time::{SystemTime, UNIX_EPOCH};

        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "td-compositor-control-parser-{}-{nonce}",
            std::process::id()
        ));
        let actual = root.join("actual");
        let alias = root.join("alias");
        std::fs::create_dir_all(&actual).unwrap();
        symlink(&actual, &alias).unwrap();
        let wayland = actual.join("wayland-0");
        let portal = actual.join("td-portal-wayland-0");
        let ready = actual.join("firefox-window-ready");
        let control = actual.join("td-control");
        let valid = |control: &Path| {
            vec![
                "--framebuffer".into(),
                "/dev/fb0".into(),
                "--input".into(),
                "/dev/input".into(),
                "--socket".into(),
                wayland.to_string_lossy().into_owned(),
                "--portal-socket".into(),
                portal.to_string_lossy().into_owned(),
                "--control-socket".into(),
                control.to_string_lossy().into_owned(),
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

        // The flag's value reaches the option, resolved — a compositor that
        // bound somewhere else would advertise a path nothing answers on.
        let options = super::parse_run(&valid(&control)).unwrap();
        let resolved_parent = std::fs::canonicalize(&actual).unwrap();
        assert_eq!(
            options.control_socket,
            Some(resolved_parent.join("td-control"))
        );
        // Absent means absent: no socket bound and nothing to reach.
        let mut without = valid(&control);
        let at = without
            .iter()
            .position(|argument| argument == "--control-socket")
            .expect("the flag this test is about");
        without.drain(at..at.saturating_add(2));
        assert_eq!(super::parse_run(&without).unwrap().control_socket, None);

        // All three of its pairs. Four endpoints are six pairs and these are
        // the three the fourth added.
        assert!(super::parse_run(&valid(&wayland)).is_err());
        assert!(super::parse_run(&valid(&portal)).is_err());
        assert!(super::parse_run(&valid(&ready)).is_err());
        // Including under a spelling that resolves onto one of them, which is
        // why the walk compares resolved endpoints rather than argv strings.
        assert!(super::parse_run(&valid(&alias.join("wayland-0"))).is_err());
        assert!(super::parse_run(&valid(&actual.join(".").join("wayland-0"))).is_err());

        // And named twice is a mistake, not a last-one-wins.
        let mut twice = valid(&control);
        twice.push("--control-socket".into());
        twice.push(control.to_string_lossy().into_owned());
        assert!(super::parse_run(&twice).is_err());

        std::fs::remove_dir_all(&root).ok();
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
        let portal = actual.join("td-portal-wayland-0");
        let ready = actual.join("firefox-window-ready");
        let valid = |socket: &Path, portal: &Path, ready: &Path| {
            vec![
                "--framebuffer".into(),
                "/dev/fb0".into(),
                "--input".into(),
                "/dev/input".into(),
                "--socket".into(),
                socket.to_string_lossy().into_owned(),
                "--portal-socket".into(),
                portal.to_string_lossy().into_owned(),
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
        let options = super::parse_run(&valid(&wayland, &portal, &ready)).unwrap();
        let mut authority = valid(&wayland, &portal, &ready);
        let at = authority
            .iter()
            .position(|word| word == "--terminal-client")
            .unwrap();
        authority[at] = "--terminal-authority".into();
        authority[at + 1] = "stdin".into();
        let remote = super::parse_run(&authority).unwrap();
        assert!(remote.terminal_authority);
        assert!(remote.terminal_client.is_none());
        for value in ["0", "/run/authority", "stdout", ""] {
            let mut invalid = authority.clone();
            invalid[at + 1] = value.into();
            assert!(super::parse_run(&invalid).is_err());
        }
        let mut both = authority.clone();
        both.extend(["--terminal-client".into(), "/bin/td-term".into()]);
        assert!(super::parse_run(&both).is_err());
        let mut duplicated = authority.clone();
        duplicated.extend(["--terminal-authority".into(), "stdin".into()]);
        assert!(super::parse_run(&duplicated).is_err());
        let mut demo = authority;
        let app = demo
            .iter()
            .position(|word| word == "--launcher-application")
            .unwrap();
        demo[app] = "--launcher-client".into();
        demo[app + 1] = "/bin/td-ui-demo".into();
        assert!(super::parse_run(&demo).is_err());

        let resolved_parent = std::fs::canonicalize(&actual).unwrap();
        assert_eq!(
            options.output,
            super::OutputDevice::Framebuffer(std::path::PathBuf::from("/dev/fb0"))
        );
        // The card instead of fbdev; the card with fbdev as its fallback; and
        // never neither.
        let fresh = valid(&wayland, &portal, &ready);
        let at = fresh
            .iter()
            .position(|word| word == "--framebuffer")
            .unwrap();
        let mut card = fresh.clone();
        card[at] = "--card".into();
        card[at + 1] = "/dev/dri/card0".into();
        assert_eq!(
            super::parse_run(&card).unwrap().output,
            super::OutputDevice::Card {
                card: std::path::PathBuf::from("/dev/dri/card0"),
                fallback: None,
            }
        );
        let mut both_outputs = fresh.clone();
        both_outputs.extend(["--card".into(), "/dev/dri/card0".into()]);
        assert_eq!(
            super::parse_run(&both_outputs).unwrap().output,
            super::OutputDevice::Card {
                card: std::path::PathBuf::from("/dev/dri/card0"),
                fallback: Some(std::path::PathBuf::from("/dev/fb0")),
            }
        );
        let mut twice = both_outputs.clone();
        twice.extend(["--card".into(), "/dev/dri/card1".into()]);
        assert!(super::parse_run(&twice).is_err());
        let mut neither = fresh;
        neither.drain(at..at + 2);
        assert!(super::parse_run(&neither)
            .err()
            .is_some_and(|error| error.contains("--card or --framebuffer is required")));
        assert_eq!(options.launcher_client, None);
        assert_eq!(
            options.launcher_application.as_deref(),
            Some("td-jail-fixture")
        );
        assert_eq!(
            options.terminal_client,
            Some(std::path::PathBuf::from("/bin/td-term"))
        );
        assert_eq!(options.socket, resolved_parent.join("wayland-0"));
        assert_eq!(
            options.portal_socket,
            resolved_parent.join("td-portal-wayland-0")
        );
        assert_eq!(
            options.application_ready_socket,
            Some(resolved_parent.join("firefox-window-ready"))
        );
        assert_eq!(options.application_content_rgb_a.as_deref(), Some("ff00ff"));
        assert_eq!(options.application_content_rgb_b.as_deref(), Some("00ff00"));
        let mut activation_without_observer = valid(&wayland, &portal, &ready);
        activation_without_observer.truncate(activation_without_observer.len() - 8);
        assert!(super::parse_run(&activation_without_observer).is_err());
        let mut one_content_color = valid(&wayland, &portal, &ready);
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
            &portal,
            &actual.join(".").join("wayland-0")
        ))
        .is_err());
        assert!(super::parse_run(&valid(&wayland, &portal, &alias.join("wayland-0"))).is_err());
        assert!(super::parse_run(&valid(&wayland, &wayland, &ready)).is_err());
        assert!(super::parse_run(&valid(&wayland, &ready, &ready)).is_err());
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
        let mut both_modes = valid(&wayland, &portal, &ready);
        both_modes.extend(["--launcher-client".into(), "/bin/td-ui-demo".into()]);
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

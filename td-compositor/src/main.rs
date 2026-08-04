#![deny(unsafe_code)]

mod client;
mod configure;
mod font;
mod font_data;
mod framebuffer;
mod input;
mod keyboard;
mod keys;
mod launcher;
mod layout;
mod pointer;
mod pty;
mod render;
mod runtime;
mod scene;
mod server;
mod socket;
mod sys;
mod term;
mod terminfo;
mod ui;
mod wire;

use framebuffer::Framebuffer;
use runtime::Runtime;
use std::env;
use std::path::{Path, PathBuf};
use std::process;
use std::sync::{Arc, Mutex};

const MAX_UI_DIMENSION: usize = 16_384;
const MAX_UI_FRAME_BYTES: usize = 32 * 1024 * 1024;

fn usage() -> String {
    "usage: td-compositor run --framebuffer PATH --input DIR --socket PATH \
     --launcher-client PATH | td-compositor probe SOCKET | td-compositor terminfo PATH | \
     td-compositor selftest"
        .into()
}

fn client_usage() -> String {
    "usage: td-ui-demo run --socket PATH --ready-socket PATH | \
     td-ui-demo probe READY_SOCKET | td-ui-demo selftest"
        .into()
}

struct RunOptions {
    framebuffer: PathBuf,
    input: PathBuf,
    socket: PathBuf,
    launcher_client: PathBuf,
}

fn parse_run(args: &[String]) -> Result<RunOptions, String> {
    let mut framebuffer = None;
    let mut input = None;
    let mut socket = None;
    let mut launcher_client = None;
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
            "--framebuffer" | "--input" | "--socket" | "--launcher-client" => {
                return Err(format!("duplicate flag '{flag}'"))
            }
            _ => return Err(format!("unrecognised argument '{flag}'")),
        }
        index += 2;
    }
    Ok(RunOptions {
        framebuffer: framebuffer.ok_or_else(|| "--framebuffer is required".to_string())?,
        input: input.ok_or_else(|| "--input is required".to_string())?,
        socket: socket.ok_or_else(|| "--socket is required".to_string())?,
        launcher_client: launcher_client
            .ok_or_else(|| "--launcher-client is required".to_string())?,
    })
}

fn run_compositor(options: RunOptions) -> Result<(), String> {
    let framebuffer = Framebuffer::open(&options.framebuffer)?;
    let geometry = (framebuffer.width, framebuffer.height, framebuffer.stride);
    let runtime = Arc::new(Mutex::new(Runtime::new(framebuffer)));
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
        },
    )?;
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
    let mut frame = vec![0; 4 * 4 * 4];
    scene.render(&mut frame, 4, 4, 4 * 4);
    if !frame.as_chunks::<4>().0.contains(&[1, 2, 3, 0]) {
        return Err("renderer selftest did not copy its surface".into());
    }
    println!("TD-COMPOSITOR-SELFTEST-OK");
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

fn parse_client_run(args: &[String]) -> Result<client::Options, String> {
    let mut socket = None;
    let mut ready_socket = None;
    let mut index = 0;
    while index < args.len() {
        let flag = args
            .get(index)
            .ok_or_else(|| "missing UI client flag".to_string())?;
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
    Ok(client::Options {
        socket: socket.ok_or_else(|| "--socket is required".to_string())?,
        ready_socket: ready_socket.ok_or_else(|| "--ready-socket is required".to_string())?,
    })
}

fn run_client(args: &[String]) -> Result<(), String> {
    let command = args.first().ok_or_else(client_usage)?;
    match command.as_str() {
        "run" => client::run(&parse_client_run(args.get(1..).ok_or_else(client_usage)?)?),
        "probe" => {
            let socket = args.get(1).ok_or_else(client_usage)?;
            if args.get(2).is_some() {
                return Err(client_usage());
            }
            client::probe(Path::new(socket))
        }
        "selftest" => {
            if args.get(1).is_some() {
                return Err(client_usage());
            }
            client::selftest()
        }
        _ => Err(client_usage()),
    }
}

fn main() {
    let mut argv = env::args();
    let executable = argv.next().unwrap_or_default();
    let args: Vec<String> = argv.collect();
    let is_client = Path::new(&executable)
        .file_name()
        .and_then(|name| name.to_str())
        == Some("td-ui-demo");
    let result = if is_client {
        run_client(&args)
    } else {
        run(&args)
    };
    if let Err(error) = result {
        let program = if is_client {
            "td-ui-demo"
        } else {
            "td-compositor"
        };
        eprintln!("{program}: {error}");
        process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launcher_command_round_trips_through_the_client_parser() {
        let launch = launcher::LaunchOptions {
            socket: PathBuf::from("/run/user/1000/wayland-0"),
            client: PathBuf::from("/bin/td-ui-demo"),
        };
        let (program, arguments, ready_socket) =
            launcher::launch_command(&launch, launcher::LaunchRequest::UiDemo, 7).unwrap();
        let arguments: Vec<String> = arguments
            .into_iter()
            .map(|argument| argument.into_string().unwrap())
            .collect();
        assert_eq!(program, launch.client);
        assert_eq!(arguments.first().map(String::as_str), Some("run"));
        let parsed = parse_client_run(arguments.get(1..).unwrap()).unwrap();
        assert_eq!(parsed.socket, launch.socket);
        assert_eq!(parsed.ready_socket, ready_socket);
        assert_eq!(
            parsed.ready_socket.parent(),
            Some(Path::new("/run/user/1000"))
        );
        let ready_name = parsed.ready_socket.file_name().unwrap().to_string_lossy();
        assert!(ready_name.starts_with("td-launcher-"));
        assert!(ready_name.ends_with("-7.ready"));
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
        ("client.rs", include_str!("client.rs")),
        ("configure.rs", include_str!("configure.rs")),
        ("font.rs", include_str!("font.rs")),
        ("font_data.rs", include_str!("font_data.rs")),
        ("framebuffer.rs", include_str!("framebuffer.rs")),
        ("input.rs", include_str!("input.rs")),
        ("keyboard.rs", include_str!("keyboard.rs")),
        ("keys.rs", include_str!("keys.rs")),
        ("launcher.rs", include_str!("launcher.rs")),
        ("layout.rs", include_str!("layout.rs")),
        ("pointer.rs", include_str!("pointer.rs")),
        ("pty.rs", include_str!("pty.rs")),
        ("render.rs", include_str!("render.rs")),
        ("runtime.rs", include_str!("runtime.rs")),
        ("scene.rs", include_str!("scene.rs")),
        ("server.rs", include_str!("server.rs")),
        ("socket.rs", include_str!("socket.rs")),
        ("term.rs", include_str!("term.rs")),
        ("terminfo.rs", include_str!("terminfo.rs")),
        ("ui.rs", include_str!("ui.rs")),
        ("wire.rs", include_str!("wire.rs")),
    ];
    const TEST_ONLY: &[(&str, &str)] = &[
        ("render_spec.rs", include_str!("render_spec.rs")),
        ("term_spec.rs", include_str!("term_spec.rs")),
    ];

    fn occurrences(source: &str, needle: &str) -> usize {
        source.match_indices(needle).count()
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
    fn one_scoped_unsafe_body_carries_only_the_reviewed_syscalls() {
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
        assert_eq!(occurrences(SYS, "#[allow(unsafe_code)]"), 1);
        assert_eq!(occurrences(SYS, "unsafe {"), 1);
        assert_eq!(occurrences(SYS, "core::arch::asm!"), 1);
        assert_eq!(occurrences(SYS, syscall_body), 1);
        for syscall in [
            "const SYS_CLOSE: usize = 3;",
            "const SYS_IOCTL: usize = 16;",
            "const SYS_SENDMSG: usize = 46;",
            "const SYS_RECVMSG: usize = 47;",
        ] {
            assert!(SYS.contains(syscall), "{syscall}");
        }
        assert_eq!(occurrences(SYS, "const SYS_"), 4);
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
    /// confinement: these four values, one entry point, and four callers.
    #[test]
    fn the_ioctl_surface_is_four_pinned_requests_and_four_wrappers() {
        for request in [
            "const TIOCSPTLCK: usize = 0x4004_5431;",
            "const TIOCGPTPEER: usize = 0x5441;",
            "const TIOCSWINSZ: usize = 0x5414;",
            "const TIOCGWINSZ: usize = 0x5413;",
        ] {
            assert!(SYS.contains(request), "{request}");
        }
        assert_eq!(occurrences(SYS, "const TIOC"), 4);
        // The guard names each request once more; every other mention is a
        // wrapper passing it to the one entry point.
        let guard = r#"    if !matches!(
        request,
        TIOCSPTLCK | TIOCGPTPEER | TIOCSWINSZ | TIOCGWINSZ
    ) {"#;
        assert_eq!(occurrences(SYS, guard), 1);
        let entry = r#"    errno_result(syscall3(SYS_IOCTL, fd as usize, request, argument), operation)"#;
        assert_eq!(occurrences(SYS, entry), 1);
        assert_eq!(occurrences(SYS, "fn ioctl("), 1);
        // One definition plus exactly four call sites: a FIFTH wrapper reusing
        // a pinned request would satisfy every other assertion here. Split at
        // the test MODULE, since `sys.rs` puts `#[cfg(test)]` on individual
        // constants too and the first one would truncate this to nothing.
        let production_sys = SYS
            .split_once("\n#[cfg(test)]\nmod tests {")
            .map_or(SYS, |(source, _)| source);
        let mentions = occurrences(production_sys, "ioctl(");
        let prose = occurrences(production_sys, "ioctl(2)");
        assert_eq!(mentions - prose, 5);
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
        // The peer's open flags are pinned rather than chosen by a caller: the
        // slave belongs to the child, so O_NOCTTY cannot be forgotten.
        assert!(SYS.contains("const PTY_PEER_FLAGS: usize = 0o2 | 0o400 | 0o2_000_000;"));
        // The eight bytes the kernel reads are an array whose layout the
        // language guarantees, not an attribute nobody can observe.
        assert!(SYS.contains("fn winsize_words(size: WindowSize) -> [u16; 4] {"));
        assert_eq!(occurrences(SYS, "as *const [u16; 4]"), 1);
        assert_eq!(occurrences(SYS, "as *mut [u16; 4]"), 1);
    }

    #[test]
    fn syscall_wrapper_is_called_only_by_the_four_reviewed_operations() {
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
        assert_eq!(occurrences(SYS, "syscall3("), 5);
        assert_eq!(occurrences(SYS, "SYS_CLOSE"), 2);
        assert_eq!(occurrences(SYS, "SYS_IOCTL"), 2);
        assert_eq!(occurrences(SYS, "SYS_SENDMSG"), 2);
        assert_eq!(occurrences(SYS, "SYS_RECVMSG"), 2);
        assert_eq!(occurrences(SYS, close), 1);
        assert_eq!(occurrences(SYS, receive), 1);
        assert_eq!(occurrences(SYS, send), 1);
        for operation in [
            "fn close_raw(",
            "pub fn recv_with_fds(",
            "pub fn send_with_fd(",
        ] {
            assert!(SYS.contains(operation), "{operation}");
        }
    }

    /// Two disjoint reviewed surfaces live behind one syscall body, so each is
    /// pinned to its own module: descriptor transport to the protocol
    /// endpoints, terminal control to the PTY adapter. Nothing else names
    /// `sys` at all — an alias elsewhere would give an audited call a name
    /// neither of these scans looks for.
    #[test]
    fn each_confined_operation_is_reachable_only_from_its_own_module() {
        const TRANSPORT: &[&str] = &[
            "sys::send_with_fd(",
            "sys::recv_with_fds(",
            "sys::duplicate_received(",
            "sys::discard_received(",
        ];
        const TERMINAL: &[&str] = &[
            "sys::unlock_pty(",
            "sys::pty_peer(",
            "sys::set_window_size(",
            "sys::window_size(",
        ];
        let production_main = MAIN
            .split_once("\n#[cfg(test)]")
            .map_or(MAIN, |(source, _)| source);
        for (name, source) in std::iter::once(("main.rs", production_main))
            .chain(OTHER.iter().copied())
            .chain(TEST_ONLY.iter().copied())
        {
            if matches!(name, "client.rs" | "server.rs" | "pty.rs") {
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
            let squeezed: String = source.chars().filter(|c| !c.is_whitespace()).collect();
            for form in [
                concat!("use", "crate::sys::"),
                concat!("use", "super::sys::"),
                concat!("use", "crate::sysas"),
                concat!("use", "super::sysas"),
                concat!("sys::", "*"),
            ] {
                assert_eq!(
                    squeezed.matches(form).count(),
                    0,
                    "{name} imports out of the syscall module ('{form}')"
                );
            }
        }
        let client = include_str!("client.rs");
        let server = include_str!("server.rs");
        let pty = include_str!("pty.rs");
        for operation in TRANSPORT {
            assert!(
                client.contains(operation) || server.contains(operation),
                "{operation}"
            );
            assert!(!pty.contains(operation), "pty.rs reached {operation}");
        }
        for operation in TERMINAL {
            assert!(pty.contains(operation), "{operation}");
            assert!(
                !client.contains(operation) && !server.contains(operation),
                "a protocol endpoint reached {operation}"
            );
        }
        // `set_window_size` is generic over `AsRawFd`, so inside pty.rs it
        // would type-check against any terminal — including an operator's.
        // `Pty::resize` is the only caller, and it is the one that verifies
        // what it published.
        let production_pty = pty
            .split_once("\n#[cfg(test)]")
            .map_or(pty, |(source, _)| source);
        assert_eq!(occurrences(production_pty, "sys::set_window_size("), 1);
        assert_eq!(occurrences(production_pty, "sys::window_size("), 1);
        assert!(production_pty.contains("sys::set_window_size(&self.master, requested)?;"));
        assert!(production_pty.contains("let observed = sys::window_size(&self.master)?;"));
    }

    #[test]
    fn run_parser_requires_every_device_boundary() {
        let options = super::parse_run(&[
            "--framebuffer".into(),
            "/dev/fb0".into(),
            "--input".into(),
            "/dev/input".into(),
            "--socket".into(),
            "/run/user/1000/wayland-0".into(),
            "--launcher-client".into(),
            "/bin/td-ui-demo".into(),
        ])
        .unwrap();
        assert_eq!(options.framebuffer, std::path::PathBuf::from("/dev/fb0"));
        assert_eq!(
            options.launcher_client,
            std::path::PathBuf::from("/bin/td-ui-demo")
        );
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
    }
}

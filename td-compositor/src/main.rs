#![deny(unsafe_code)]

mod framebuffer;
mod input;
mod runtime;
mod scene;
mod server;
mod sys;
mod wire;

use framebuffer::Framebuffer;
use runtime::Runtime;
use std::env;
use std::path::{Path, PathBuf};
use std::process;
use std::sync::{Arc, Mutex};

fn usage() -> String {
    "usage: td-compositor run --framebuffer PATH --input DIR --socket PATH | \
     td-compositor probe SOCKET | td-compositor selftest"
        .into()
}

struct RunOptions {
    framebuffer: PathBuf,
    input: PathBuf,
    socket: PathBuf,
}

fn parse_run(args: &[String]) -> Result<RunOptions, String> {
    let mut framebuffer = None;
    let mut input = None;
    let mut socket = None;
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
            "--framebuffer" | "--input" | "--socket" => {
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
    let inputs = input::start(&options.input, Arc::clone(&runtime))?;
    eprintln!(
        "td-compositor: software output {}x{} stride={} inputs={inputs}",
        geometry.0, geometry.1, geometry.2
    );
    server::serve(&options.socket, runtime)
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
        "selftest" => {
            if args.get(1).is_some() {
                return Err(usage());
            }
            selftest()
        }
        _ => Err(usage()),
    }
}

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    if let Err(error) = run(&args) {
        eprintln!("td-compositor: {error}");
        process::exit(1);
    }
}

#[cfg(test)]
mod confinement {
    const MAIN: &str = include_str!("main.rs");
    const SYS: &str = include_str!("sys.rs");
    const OTHER: &[(&str, &str)] = &[
        ("framebuffer.rs", include_str!("framebuffer.rs")),
        ("input.rs", include_str!("input.rs")),
        ("runtime.rs", include_str!("runtime.rs")),
        ("scene.rs", include_str!("scene.rs")),
        ("server.rs", include_str!("server.rs")),
        ("wire.rs", include_str!("wire.rs")),
    ];

    fn occurrences(source: &str, needle: &str) -> usize {
        source.match_indices(needle).count()
    }

    #[test]
    fn one_scoped_unsafe_body_carries_only_wayland_fd_transport() {
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
            "const SYS_SENDMSG: usize = 46;",
            "const SYS_RECVMSG: usize = 47;",
        ] {
            assert!(SYS.contains(syscall), "{syscall}");
        }
        assert_eq!(occurrences(SYS, "const SYS_"), 3);
        for (name, source) in OTHER {
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
    fn syscall_wrapper_is_called_only_by_the_three_reviewed_operations() {
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
        assert_eq!(occurrences(SYS, "syscall3("), 4);
        assert_eq!(occurrences(SYS, "SYS_CLOSE"), 2);
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

    #[test]
    fn run_parser_requires_every_device_boundary() {
        let options = super::parse_run(&[
            "--framebuffer".into(),
            "/dev/fb0".into(),
            "--input".into(),
            "/dev/input".into(),
            "--socket".into(),
            "/run/user/1000/wayland-0".into(),
        ])
        .unwrap();
        assert_eq!(options.framebuffer, std::path::PathBuf::from("/dev/fb0"));
        assert!(super::parse_run(&[
            "--framebuffer".into(),
            "/dev/fb0".into(),
            "--socket".into(),
            "/run/user/1000/wayland-0".into(),
        ])
        .is_err());
    }
}

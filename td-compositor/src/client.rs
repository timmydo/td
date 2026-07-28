use crate::{socket, sys, wire};
use std::fs::{self, File, OpenOptions, Permissions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

const DISPLAY: u32 = 1;
const REGISTRY: u32 = 2;
const SYNC_CALLBACK: u32 = 3;
const COMPOSITOR: u32 = 4;
const SHM: u32 = 5;
const XDG_WM_BASE: u32 = 6;
const SURFACE: u32 = 7;
const XDG_SURFACE: u32 = 8;
const XDG_TOPLEVEL: u32 = 9;
const SHM_POOL: u32 = 10;
const BUFFER: u32 = 11;
const FRAME_CALLBACK: u32 = 12;

const WIDTH: usize = 512;
const HEIGHT: usize = 320;
const BYTES_PER_PIXEL: usize = 4;
const SHM_XRGB8888: u32 = 1;
const CONNECT_ATTEMPTS: usize = 300;
const MAX_EVENT_BUFFER: usize = 128 * 1024;
const PRESENT_TIMEOUT: Duration = Duration::from_secs(20);

pub struct Options {
    pub socket: PathBuf,
    pub ready_socket: PathBuf,
}

#[derive(Clone, Copy)]
struct Global {
    name: u32,
    version: u32,
}

#[derive(Default)]
struct Globals {
    compositor: Option<Global>,
    shm: Option<Global>,
    xdg_wm_base: Option<Global>,
}

impl Globals {
    fn record(&mut self, name: u32, interface: &str, version: u32) {
        let global = Global { name, version };
        match interface {
            "wl_compositor"
                if self
                    .compositor
                    .is_none_or(|current| version > current.version) =>
            {
                self.compositor = Some(global)
            }
            "wl_shm" if self.shm.is_none_or(|current| version > current.version) => {
                self.shm = Some(global)
            }
            "xdg_wm_base"
                if self
                    .xdg_wm_base
                    .is_none_or(|current| version > current.version) =>
            {
                self.xdg_wm_base = Some(global)
            }
            _ => {}
        }
    }

    fn require(
        global: Option<Global>,
        interface: &str,
        minimum: u32,
        maximum: u32,
    ) -> Result<(u32, u32), String> {
        let global = global.ok_or_else(|| format!("compositor did not advertise {interface}"))?;
        if global.version < minimum {
            return Err(format!(
                "{interface} version {} is below required version {minimum}",
                global.version
            ));
        }
        Ok((global.name, global.version.min(maximum)))
    }
}

struct Connection {
    stream: UnixStream,
    buffered: Vec<u8>,
    deadline: Option<Instant>,
}

impl Connection {
    fn connect(path: &Path, deadline: Instant) -> Result<Connection, String> {
        let mut last = None;
        for attempt in 0..CONNECT_ATTEMPTS {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .filter(|duration| !duration.is_zero())
                .ok_or_else(|| "Wayland presentation handshake timed out".to_string())?;
            match UnixStream::connect(path) {
                Ok(stream) => {
                    return Ok(Connection {
                        stream,
                        buffered: Vec::with_capacity(16 * 1024),
                        deadline: Some(deadline),
                    });
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                    ) =>
                {
                    last = Some(error);
                    if attempt + 1 < CONNECT_ATTEMPTS {
                        thread::sleep(remaining.min(Duration::from_millis(100)));
                    }
                }
                Err(error) => {
                    return Err(format!(
                        "connect Wayland socket {}: {error}",
                        path.display()
                    ));
                }
            }
        }
        Err(format!(
            "connect Wayland socket {} after {CONNECT_ATTEMPTS} attempts: {}",
            path.display(),
            last.map_or_else(|| "unknown error".to_string(), |error| error.to_string())
        ))
    }

    fn remaining(&self) -> Result<Option<Duration>, String> {
        self.deadline
            .map(|deadline| {
                deadline
                    .checked_duration_since(Instant::now())
                    .filter(|duration| !duration.is_zero())
                    .ok_or_else(|| "Wayland presentation handshake timed out".to_string())
            })
            .transpose()
    }

    fn finish_handshake(&mut self) -> Result<(), String> {
        self.stream
            .set_read_timeout(None)
            .map_err(|e| format!("clear Wayland handshake timeout: {e}"))?;
        self.deadline = None;
        Ok(())
    }

    fn send(&mut self, object: u32, opcode: u16, builder: wire::Builder) -> Result<(), String> {
        let bytes = builder.message(object, opcode)?;
        self.stream
            .write_all(&bytes)
            .map_err(|e| format!("write Wayland request object={object} opcode={opcode}: {e}"))
    }

    fn send_with_fd(
        &mut self,
        object: u32,
        opcode: u16,
        builder: wire::Builder,
        file: &File,
    ) -> Result<(), String> {
        let bytes = builder.message(object, opcode)?;
        sys::send_with_fd(&self.stream, &bytes, file.as_raw_fd())
    }

    fn next(&mut self) -> Result<wire::Message, String> {
        loop {
            let remaining = self.remaining()?;
            if let Some(message) = wire::take(&mut self.buffered)? {
                return Ok(message);
            }
            if self.buffered.len() > MAX_EVENT_BUFFER {
                return Err(format!(
                    "Wayland event buffer exceeded {MAX_EVENT_BUFFER} bytes"
                ));
            }
            let mut incoming = [0u8; 16 * 1024];
            if let Some(remaining) = remaining {
                self.stream
                    .set_read_timeout(Some(remaining))
                    .map_err(|e| format!("set Wayland handshake timeout: {e}"))?;
            }
            let count = self.stream.read(&mut incoming).map_err(|e| {
                if self.deadline.is_some()
                    && matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    )
                {
                    "Wayland presentation handshake timed out".to_string()
                } else {
                    format!("read Wayland event: {e}")
                }
            })?;
            if count == 0 {
                return Err("Wayland compositor closed the connection".into());
            }
            let bytes = incoming
                .get(..count)
                .ok_or_else(|| "Wayland read count escaped input buffer".to_string())?;
            self.buffered.extend_from_slice(bytes);
        }
    }

    fn handle_common(&mut self, message: &wire::Message) -> Result<bool, String> {
        if message.object == DISPLAY && message.opcode == 0 {
            let mut args = wire::Cursor::new(&message.payload);
            let object = args.u32()?;
            let code = args.u32()?;
            let text = args.string()?;
            args.finish()?;
            return Err(format!(
                "Wayland protocol error on object {object}, code {code}: {text}"
            ));
        }
        if message.object == XDG_WM_BASE && message.opcode == 0 {
            let mut args = wire::Cursor::new(&message.payload);
            let serial = args.u32()?;
            args.finish()?;
            let mut pong = wire::Builder::new();
            pong.u32(serial);
            self.send(XDG_WM_BASE, 3, pong)?;
            return Ok(true);
        }
        Ok(false)
    }
}

struct ReadySocket {
    path: PathBuf,
}

impl Drop for ReadySocket {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn start_ready_socket(path: &Path) -> Result<ReadySocket, String> {
    socket::remove_stale(path, "readiness")?;
    let listener = UnixListener::bind(path)
        .map_err(|e| format!("bind readiness socket {}: {e}", path.display()))?;
    if let Err(error) = fs::set_permissions(path, Permissions::from_mode(0o600)) {
        let cleanup = fs::remove_file(path);
        return match cleanup {
            Ok(()) => Err(format!(
                "chmod readiness socket {}: {error}",
                path.display()
            )),
            Err(cleanup) => Err(format!(
                "chmod readiness socket {}: {error}; remove it: {cleanup}",
                path.display()
            )),
        };
    }
    let owned_path = path.to_path_buf();
    if let Err(error) = thread::Builder::new()
        .name("ui-demo-ready".into())
        .spawn(move || {
            for connection in listener.incoming() {
                if connection.is_err() {
                    break;
                }
            }
        })
    {
        let _ = fs::remove_file(path);
        return Err(format!("start readiness listener: {error}"));
    }
    Ok(ReadySocket { path: owned_path })
}

fn bind(
    connection: &mut Connection,
    name: u32,
    interface: &str,
    version: u32,
    id: u32,
) -> Result<(), String> {
    let mut request = wire::Builder::new();
    request.u32(name);
    request.string(interface)?;
    request.u32(version);
    request.u32(id);
    connection.send(REGISTRY, 0, request)
}

fn discover_globals(connection: &mut Connection) -> Result<Globals, String> {
    let mut registry = wire::Builder::new();
    registry.u32(REGISTRY);
    connection.send(DISPLAY, 1, registry)?;

    let mut sync = wire::Builder::new();
    sync.u32(SYNC_CALLBACK);
    connection.send(DISPLAY, 0, sync)?;

    let mut globals = Globals::default();
    loop {
        let message = connection.next()?;
        if connection.handle_common(&message)? {
            continue;
        }
        if message.object == REGISTRY && message.opcode == 0 {
            let mut args = wire::Cursor::new(&message.payload);
            let name = args.u32()?;
            let interface = args.string()?;
            let version = args.u32()?;
            args.finish()?;
            globals.record(name, &interface, version);
        } else if message.object == SYNC_CALLBACK && message.opcode == 0 {
            let mut args = wire::Cursor::new(&message.payload);
            args.u32()?;
            args.finish()?;
            return Ok(globals);
        }
    }
}

fn build_pixels() -> Result<Vec<u8>, String> {
    let count = WIDTH
        .checked_mul(HEIGHT)
        .and_then(|pixels| pixels.checked_mul(BYTES_PER_PIXEL))
        .ok_or_else(|| "demo surface size overflow".to_string())?;
    let mut pixels = vec![0u8; count];
    for (index, pixel) in pixels.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        let x = index % WIDTH;
        let y = index / WIDTH;
        let card = (32..WIDTH - 32).contains(&x) && (28..HEIGHT - 28).contains(&y);
        let header = card && y < 82;
        let footer = card && y >= HEIGHT - 72;
        let column = x.saturating_sub(56) / 136;
        let stripe = card && (96..HEIGHT - 88).contains(&y) && (56..WIDTH - 48).contains(&x);
        let gradient =
            u8::try_from((x + y) % 48).map_err(|_| "demo gradient escaped u8".to_string())?;
        let color = if header {
            [0x78, 0x46, 0xe8, 0]
        } else if footer {
            [0x3c, 0xc8, 0xa0, 0]
        } else if stripe {
            match column {
                0 => [0xd8, 0x78, 0x40, 0],
                1 => [0x68, 0xb8, 0xf0, 0],
                _ => [0xb8, 0x70, 0xe8, 0],
            }
        } else if card {
            [0x32, 0x2a, 0x38, 0]
        } else {
            [
                0x20u8.saturating_add(gradient / 3),
                0x18u8.saturating_add(gradient / 4),
                0x24u8.saturating_add(gradient / 2),
                0,
            ]
        };
        pixel.copy_from_slice(&color);
    }
    Ok(pixels)
}

fn backing_file(directory: &Path, pixels: &[u8]) -> Result<File, String> {
    let pid = std::process::id();
    for attempt in 0..64u32 {
        let path = directory.join(format!("td-ui-demo-{pid}-{attempt}.shm"));
        let mut file = match OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(format!(
                    "create wl_shm backing file {}: {error}",
                    path.display()
                ));
            }
        };
        let write = file
            .write_all(pixels)
            .map_err(|e| format!("write wl_shm backing file {}: {e}", path.display()));
        let remove = fs::remove_file(&path)
            .map_err(|e| format!("unlink wl_shm file {}: {e}", path.display()));
        // Unlink before propagating a write failure so every created path is cleaned up.
        write?;
        remove?;
        return Ok(file);
    }
    Err(format!(
        "could not create a unique wl_shm file in {}",
        directory.display()
    ))
}

fn configure_surface(connection: &mut Connection) -> Result<(), String> {
    let mut surface = wire::Builder::new();
    surface.u32(SURFACE);
    connection.send(COMPOSITOR, 0, surface)?;

    let mut xdg_surface = wire::Builder::new();
    xdg_surface.u32(XDG_SURFACE);
    xdg_surface.u32(SURFACE);
    connection.send(XDG_WM_BASE, 2, xdg_surface)?;

    let mut toplevel = wire::Builder::new();
    toplevel.u32(XDG_TOPLEVEL);
    connection.send(XDG_SURFACE, 1, toplevel)?;

    let mut title = wire::Builder::new();
    title.string("td software Wayland demo")?;
    connection.send(XDG_TOPLEVEL, 2, title)?;
    connection.send(SURFACE, 6, wire::Builder::new())?;

    loop {
        let message = connection.next()?;
        if connection.handle_common(&message)? {
            continue;
        }
        if message.object == XDG_SURFACE && message.opcode == 0 {
            let mut args = wire::Cursor::new(&message.payload);
            let serial = args.u32()?;
            args.finish()?;
            let mut ack = wire::Builder::new();
            ack.u32(serial);
            connection.send(XDG_SURFACE, 4, ack)?;
            return Ok(());
        }
    }
}

fn commit_first_frame(connection: &mut Connection, runtime_directory: &Path) -> Result<(), String> {
    let pixels = build_pixels()?;
    let file = backing_file(runtime_directory, &pixels)?;
    let size =
        i32::try_from(pixels.len()).map_err(|_| "demo wl_shm pool exceeds i32".to_string())?;
    let width = i32::try_from(WIDTH).map_err(|_| "demo width exceeds i32".to_string())?;
    let height = i32::try_from(HEIGHT).map_err(|_| "demo height exceeds i32".to_string())?;
    let stride = WIDTH
        .checked_mul(BYTES_PER_PIXEL)
        .and_then(|value| i32::try_from(value).ok())
        .ok_or_else(|| "demo stride exceeds i32".to_string())?;

    let mut pool = wire::Builder::new();
    pool.u32(SHM_POOL);
    pool.i32(size);
    connection.send_with_fd(SHM, 0, pool, &file)?;

    let mut buffer = wire::Builder::new();
    buffer.u32(BUFFER);
    buffer.i32(0);
    buffer.i32(width);
    buffer.i32(height);
    buffer.i32(stride);
    buffer.u32(SHM_XRGB8888);
    connection.send(SHM_POOL, 0, buffer)?;

    let mut attach = wire::Builder::new();
    attach.u32(BUFFER);
    attach.i32(0);
    attach.i32(0);
    connection.send(SURFACE, 1, attach)?;

    let mut damage = wire::Builder::new();
    damage.i32(0);
    damage.i32(0);
    damage.i32(width);
    damage.i32(height);
    connection.send(SURFACE, 9, damage)?;

    let mut frame = wire::Builder::new();
    frame.u32(FRAME_CALLBACK);
    connection.send(SURFACE, 3, frame)?;
    connection.send(SURFACE, 6, wire::Builder::new())?;

    let mut released = false;
    let mut presented = false;
    while !released || !presented {
        let message = connection.next()?;
        if connection.handle_common(&message)? {
            continue;
        }
        if message.object == BUFFER && message.opcode == 0 {
            wire::Cursor::new(&message.payload).finish()?;
            released = true;
        } else if message.object == FRAME_CALLBACK && message.opcode == 0 {
            let mut args = wire::Cursor::new(&message.payload);
            args.u32()?;
            args.finish()?;
            presented = true;
        }
    }
    Ok(())
}

fn present(mut connection: Connection, runtime_directory: &Path) -> Result<Connection, String> {
    let globals = discover_globals(&mut connection)?;
    let (compositor_name, compositor_version) =
        Globals::require(globals.compositor, "wl_compositor", 4, 4)?;
    let (shm_name, shm_version) = Globals::require(globals.shm, "wl_shm", 1, 1)?;
    let (xdg_name, xdg_version) = Globals::require(globals.xdg_wm_base, "xdg_wm_base", 1, 1)?;
    bind(
        &mut connection,
        compositor_name,
        "wl_compositor",
        compositor_version,
        COMPOSITOR,
    )?;
    bind(&mut connection, shm_name, "wl_shm", shm_version, SHM)?;
    bind(
        &mut connection,
        xdg_name,
        "xdg_wm_base",
        xdg_version,
        XDG_WM_BASE,
    )?;
    configure_surface(&mut connection)?;
    commit_first_frame(&mut connection, runtime_directory)?;
    Ok(connection)
}

pub fn run(options: &Options) -> Result<(), String> {
    let runtime_directory = options
        .socket
        .parent()
        .ok_or_else(|| format!("Wayland socket {} has no parent", options.socket.display()))?;
    let deadline = Instant::now()
        .checked_add(PRESENT_TIMEOUT)
        .ok_or_else(|| "could not bound the Wayland presentation handshake".to_string())?;
    let connection = Connection::connect(&options.socket, deadline)?;
    let mut connection = present(connection, runtime_directory)?;
    connection.finish_handshake()?;

    let _ready = start_ready_socket(&options.ready_socket)?;
    println!("TD-UI-CLIENT-READY surface={}x{}", WIDTH, HEIGHT);
    std::io::stdout()
        .flush()
        .map_err(|e| format!("flush UI client ready marker: {e}"))?;

    loop {
        let message = connection.next()?;
        if connection.handle_common(&message)? {
            continue;
        }
        if message.object == XDG_TOPLEVEL && message.opcode == 1 {
            return Err("compositor requested that the demo close".into());
        }
    }
}

#[cfg(test)]
pub fn present_for_test(
    stream: UnixStream,
    runtime_directory: &Path,
) -> Result<UnixStream, String> {
    let mut connection = Connection {
        stream,
        buffered: Vec::with_capacity(16 * 1024),
        deadline: None,
    };
    connection = present(connection, runtime_directory)?;
    Ok(connection.stream)
}

pub fn probe(path: &Path) -> Result<(), String> {
    UnixStream::connect(path)
        .map(|_| ())
        .map_err(|e| format!("connect UI readiness socket {}: {e}", path.display()))
}

pub fn selftest() -> Result<(), String> {
    let pixels = build_pixels()?;
    let first = pixels
        .get(..4)
        .ok_or_else(|| "demo pattern has no first pixel".to_string())?;
    let last = pixels
        .get(pixels.len().saturating_sub(4)..)
        .ok_or_else(|| "demo pattern has no last pixel".to_string())?;
    if first == last {
        return Err("demo pattern did not vary across the surface".into());
    }

    let (sender, receiver) =
        UnixStream::pair().map_err(|e| format!("create descriptor test socket: {e}"))?;
    let file = backing_file(&std::env::temp_dir(), first)?;
    sys::send_with_fd(&sender, b"demo", file.as_raw_fd())?;
    let mut bytes = [0u8; 16];
    let received = sys::recv_with_fds(&receiver, &mut bytes)?;
    if received.count != 4 || bytes.get(..4) != Some(b"demo") || received.fds.len() != 1 {
        sys::discard_received(&received.fds);
        return Err("demo descriptor transport did not preserve its message".into());
    }
    let fd = received
        .fds
        .first()
        .copied()
        .ok_or_else(|| "demo descriptor transport returned no fd".to_string())?;
    let mut duplicate = sys::duplicate_received(fd)?;
    let mut content = Vec::new();
    duplicate
        .read_to_end(&mut content)
        .map_err(|e| format!("read duplicated demo descriptor: {e}"))?;
    if content != first {
        return Err("demo descriptor transport did not preserve pixels".into());
    }
    println!("TD-UI-DEMO-SELFTEST-OK");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pattern_is_exactly_one_xrgb_surface() {
        let pixels = build_pixels().unwrap();
        assert_eq!(pixels.len(), WIDTH * HEIGHT * BYTES_PER_PIXEL);
        assert!(pixels.as_chunks::<4>().0.iter().all(|pixel| pixel[3] == 0));
    }

    #[test]
    fn globals_choose_highest_supported_advertisement() {
        let mut globals = Globals::default();
        globals.record(8, "wl_compositor", 3);
        globals.record(9, "wl_compositor", 4);
        assert_eq!(
            Globals::require(globals.compositor, "wl_compositor", 4, 4).unwrap(),
            (9, 4)
        );

        let mut globals = Globals::default();
        globals.record(9, "wl_compositor", 7);
        assert_eq!(
            Globals::require(globals.compositor, "wl_compositor", 4, 4).unwrap(),
            (9, 4)
        );
    }

    #[test]
    fn expired_presentation_deadline_does_not_block_on_a_stalled_peer() {
        let (stream, _peer) = UnixStream::pair().unwrap();
        let mut connection = Connection {
            stream,
            buffered: Vec::new(),
            deadline: Some(Instant::now()),
        };
        assert_eq!(
            connection.next().unwrap_err(),
            "Wayland presentation handshake timed out"
        );
    }
}

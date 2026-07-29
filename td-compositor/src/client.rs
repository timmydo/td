use crate::{socket, sys, wire, MAX_UI_DIMENSION, MAX_UI_FRAME_BYTES};
use std::collections::BTreeSet;
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
const FIRST_DYNAMIC_ID: u32 = 10;

const DEFAULT_WIDTH: usize = 512;
const DEFAULT_HEIGHT: usize = 320;
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
    next_id: u32,
    free_ids: BTreeSet<u32>,
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
                        next_id: FIRST_DYNAMIC_ID,
                        free_ids: BTreeSet::new(),
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
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) {
                    if self.deadline.is_some() {
                        "Wayland presentation handshake timed out".to_string()
                    } else {
                        "Wayland event wait timed out".to_string()
                    }
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
        if message.object == DISPLAY && message.opcode == 1 {
            let mut args = wire::Cursor::new(&message.payload);
            let id = args.u32()?;
            args.finish()?;
            if id >= FIRST_DYNAMIC_ID {
                if id >= self.next_id {
                    return Err(format!("compositor deleted unallocated object {id}"));
                }
                if !self.free_ids.insert(id) {
                    return Err(format!("compositor deleted object {id} twice"));
                }
            }
            return Ok(true);
        }
        Ok(false)
    }

    fn allocate_id(&mut self) -> Result<u32, String> {
        if let Some(id) = self.free_ids.pop_first() {
            return Ok(id);
        }
        let id = self.next_id;
        self.next_id = self
            .next_id
            .checked_add(1)
            .filter(|next| *next != 0)
            .ok_or_else(|| "Wayland object id space exhausted".to_string())?;
        Ok(id)
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

fn build_pixels(width: usize, height: usize) -> Result<Vec<u8>, String> {
    if width == 0 || height == 0 {
        return Err("demo surface dimensions must be positive".into());
    }
    if width > MAX_UI_DIMENSION || height > MAX_UI_DIMENSION {
        return Err(format!(
            "demo surface {width}x{height} exceeds the dimension limit"
        ));
    }
    let count = width
        .checked_mul(height)
        .and_then(|pixels| pixels.checked_mul(BYTES_PER_PIXEL))
        .ok_or_else(|| "demo surface size overflow".to_string())?;
    if count > MAX_UI_FRAME_BYTES {
        return Err(format!(
            "demo surface needs {count} bytes, exceeding {MAX_UI_FRAME_BYTES}"
        ));
    }
    let inset_x = width / 16;
    let inset_y = height / 12;
    let card_right = width.saturating_sub(inset_x);
    let card_bottom = height.saturating_sub(inset_y);
    let header_end = inset_y.saturating_add(height / 6);
    let footer_start = card_bottom.saturating_sub(height / 5);
    let stripe_top = header_end.saturating_add(height / 24);
    let stripe_bottom = footer_start.saturating_sub(height / 24);
    let stripe_left = inset_x.saturating_add(width / 20);
    let stripe_right = card_right.saturating_sub(width / 20);
    let stripe_width = stripe_right.saturating_sub(stripe_left).max(1);
    let mut pixels = vec![0u8; count];
    for (index, pixel) in pixels.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        let x = index % width;
        let y = index / width;
        let card = (inset_x..card_right).contains(&x) && (inset_y..card_bottom).contains(&y);
        let header = card && y < header_end;
        let footer = card && y >= footer_start;
        let stripe = card
            && (stripe_top..stripe_bottom).contains(&y)
            && (stripe_left..stripe_right).contains(&x);
        let gradient =
            u8::try_from((x + y) % 48).map_err(|_| "demo gradient escaped u8".to_string())?;
        let color = if header {
            [0x78, 0x46, 0xe8, 0]
        } else if footer {
            [0x3c, 0xc8, 0xa0, 0]
        } else if stripe {
            let column = x.saturating_sub(stripe_left).saturating_mul(3) / stripe_width;
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

fn create_surface(connection: &mut Connection) -> Result<(), String> {
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
    connection.send(SURFACE, 6, wire::Builder::new())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Size {
    width: usize,
    height: usize,
}

#[derive(Clone, Copy)]
struct PendingConfigure {
    width: Option<usize>,
    height: Option<usize>,
    activated: bool,
    fullscreen: bool,
}

impl PendingConfigure {
    fn resolve_size(self, current: Option<Size>) -> Size {
        let fallback = current.unwrap_or(Size {
            width: DEFAULT_WIDTH,
            height: DEFAULT_HEIGHT,
        });
        Size {
            width: self.width.unwrap_or(fallback.width),
            height: self.height.unwrap_or(fallback.height),
        }
    }

    fn selects_layout_size(self) -> bool {
        self.width.is_some() || self.height.is_some()
    }
}

struct TargetFrame {
    buffer: u32,
    callback: u32,
    released: bool,
    presented: bool,
}

struct Demo {
    current_size: Option<Size>,
    pending_configure: Option<PendingConfigure>,
    target: Option<TargetFrame>,
    live_buffers: BTreeSet<u32>,
    live_callbacks: BTreeSet<u32>,
    layout_configured: bool,
    xrgb_advertised: bool,
    activated: bool,
    fullscreen: bool,
}

impl Demo {
    fn new() -> Demo {
        Demo {
            current_size: None,
            pending_configure: None,
            target: None,
            live_buffers: BTreeSet::new(),
            live_callbacks: BTreeSet::new(),
            layout_configured: false,
            xrgb_advertised: false,
            activated: false,
            fullscreen: false,
        }
    }

    fn ready(&self) -> bool {
        self.layout_configured
            && self
                .target
                .as_ref()
                .is_some_and(|frame| frame.released && frame.presented)
    }

    fn configure_toplevel(&mut self, message: &wire::Message) -> Result<(), String> {
        let mut args = wire::Cursor::new(&message.payload);
        let width = args.i32()?;
        let height = args.i32()?;
        if width < 0 || height < 0 {
            return Err(format!(
                "compositor configured a negative demo size {width}x{height}"
            ));
        }
        let state_bytes = usize::try_from(args.u32()?)
            .map_err(|_| "XDG state array length overflow".to_string())?;
        if !state_bytes.is_multiple_of(4) {
            return Err(format!("XDG state array has invalid length {state_bytes}"));
        }
        let mut activated = false;
        let mut fullscreen = false;
        for _ in 0..state_bytes / 4 {
            match args.u32()? {
                2 => fullscreen = true,
                4 => activated = true,
                _ => {}
            }
        }
        args.finish()?;
        let width =
            usize::try_from(width).map_err(|_| "configured width escaped usize".to_string())?;
        let height =
            usize::try_from(height).map_err(|_| "configured height escaped usize".to_string())?;
        let width = (width != 0).then_some(width);
        let height = (height != 0).then_some(height);
        self.pending_configure = Some(PendingConfigure {
            width,
            height,
            activated,
            fullscreen,
        });
        Ok(())
    }

    fn acknowledge(
        &mut self,
        connection: &mut Connection,
        message: &wire::Message,
        runtime_directory: &Path,
    ) -> Result<(), String> {
        let mut args = wire::Cursor::new(&message.payload);
        let serial = args.u32()?;
        args.finish()?;
        let configure = self.take_configure();
        let mut ack = wire::Builder::new();
        ack.u32(serial);
        connection.send(XDG_SURFACE, 4, ack)?;

        let size = configure.resolve_size(self.current_size);
        if configure.selects_layout_size() {
            self.layout_configured = true;
        }
        self.activated = configure.activated;
        self.fullscreen = configure.fullscreen;
        if self.current_size == Some(size) {
            connection.send(SURFACE, 6, wire::Builder::new())?;
        } else {
            self.commit_frame(connection, runtime_directory, size)?;
        }
        Ok(())
    }

    fn take_configure(&mut self) -> PendingConfigure {
        self.pending_configure.take().unwrap_or(PendingConfigure {
            width: None,
            height: None,
            activated: self.activated,
            fullscreen: self.fullscreen,
        })
    }

    fn commit_frame(
        &mut self,
        connection: &mut Connection,
        runtime_directory: &Path,
        size: Size,
    ) -> Result<(), String> {
        if !self.xrgb_advertised {
            return Err("compositor did not advertise wl_shm XRGB8888".into());
        }
        let pixels = build_pixels(size.width, size.height)?;
        let file = backing_file(runtime_directory, &pixels)?;
        let pool_id = connection.allocate_id()?;
        let buffer_id = connection.allocate_id()?;
        let callback_id = connection.allocate_id()?;
        let bytes =
            i32::try_from(pixels.len()).map_err(|_| "demo wl_shm pool exceeds i32".to_string())?;
        let width = i32::try_from(size.width).map_err(|_| "demo width exceeds i32".to_string())?;
        let height =
            i32::try_from(size.height).map_err(|_| "demo height exceeds i32".to_string())?;
        let stride = size
            .width
            .checked_mul(BYTES_PER_PIXEL)
            .and_then(|value| i32::try_from(value).ok())
            .ok_or_else(|| "demo stride exceeds i32".to_string())?;

        let mut pool = wire::Builder::new();
        pool.u32(pool_id);
        pool.i32(bytes);
        connection.send_with_fd(SHM, 0, pool, &file)?;

        let mut buffer = wire::Builder::new();
        buffer.u32(buffer_id);
        buffer.i32(0);
        buffer.i32(width);
        buffer.i32(height);
        buffer.i32(stride);
        buffer.u32(SHM_XRGB8888);
        connection.send(pool_id, 0, buffer)?;
        connection.send(pool_id, 1, wire::Builder::new())?;

        let mut attach = wire::Builder::new();
        attach.u32(buffer_id);
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
        frame.u32(callback_id);
        connection.send(SURFACE, 3, frame)?;
        connection.send(SURFACE, 6, wire::Builder::new())?;

        if !self.live_buffers.insert(buffer_id) {
            return Err(format!(
                "demo buffer object {buffer_id} was reused while live"
            ));
        }
        if !self.live_callbacks.insert(callback_id) {
            return Err(format!(
                "demo callback object {callback_id} was reused while live"
            ));
        }
        self.current_size = Some(size);
        self.target = Some(TargetFrame {
            buffer: buffer_id,
            callback: callback_id,
            released: false,
            presented: false,
        });
        Ok(())
    }

    fn release_buffer(
        &mut self,
        connection: &mut Connection,
        message: &wire::Message,
    ) -> Result<(), String> {
        wire::Cursor::new(&message.payload).finish()?;
        if !self.live_buffers.remove(&message.object) {
            return Err(format!(
                "compositor released unknown demo buffer {}",
                message.object
            ));
        }
        if let Some(target) = self.target.as_mut() {
            if target.buffer == message.object {
                target.released = true;
            }
        }
        connection.send(message.object, 0, wire::Builder::new())
    }

    fn finish_callback(&mut self, message: &wire::Message) -> Result<(), String> {
        let mut args = wire::Cursor::new(&message.payload);
        args.u32()?;
        args.finish()?;
        if !self.live_callbacks.remove(&message.object) {
            return Err(format!(
                "compositor completed unknown demo callback {}",
                message.object
            ));
        }
        if let Some(target) = self.target.as_mut() {
            if target.callback == message.object {
                target.presented = true;
            }
        }
        Ok(())
    }

    fn dispatch(
        &mut self,
        connection: &mut Connection,
        message: &wire::Message,
        runtime_directory: &Path,
    ) -> Result<(), String> {
        if message.object == XDG_TOPLEVEL && message.opcode == 0 {
            return self.configure_toplevel(message);
        }
        if message.object == SHM && message.opcode == 0 {
            let mut args = wire::Cursor::new(&message.payload);
            if args.u32()? == SHM_XRGB8888 {
                self.xrgb_advertised = true;
            }
            return args.finish();
        }
        if message.object == XDG_SURFACE && message.opcode == 0 {
            return self.acknowledge(connection, message, runtime_directory);
        }
        if self.live_buffers.contains(&message.object) && message.opcode == 0 {
            return self.release_buffer(connection, message);
        }
        if self.live_callbacks.contains(&message.object) && message.opcode == 0 {
            return self.finish_callback(message);
        }
        if message.object == XDG_TOPLEVEL && message.opcode == 1 {
            return Err("compositor requested that the demo close".into());
        }
        Err(format!(
            "unexpected Wayland event object={} opcode={}",
            message.object, message.opcode
        ))
    }
}

fn present(
    mut connection: Connection,
    runtime_directory: &Path,
) -> Result<(Connection, Demo), String> {
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
    create_surface(&mut connection)?;

    let mut demo = Demo::new();
    while !demo.ready() {
        let message = connection.next()?;
        if connection.handle_common(&message)? {
            continue;
        }
        demo.dispatch(&mut connection, &message, runtime_directory)?;
    }
    Ok((connection, demo))
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
    let (mut connection, mut demo) = present(connection, runtime_directory)?;
    let size = demo
        .current_size
        .ok_or_else(|| "demo became ready without a configured size".to_string())?;
    connection.finish_handshake()?;

    let _ready = start_ready_socket(&options.ready_socket)?;
    println!("TD-UI-CLIENT-READY surface={}x{}", size.width, size.height);
    std::io::stdout()
        .flush()
        .map_err(|e| format!("flush UI client ready marker: {e}"))?;

    loop {
        let message = connection.next()?;
        if connection.handle_common(&message)? {
            continue;
        }
        demo.dispatch(&mut connection, &message, runtime_directory)?;
    }
}

#[cfg(test)]
pub struct TestPresentation {
    connection: Connection,
    demo: Demo,
    runtime_directory: PathBuf,
}

#[cfg(test)]
impl TestPresentation {
    pub fn wait_for(
        &mut self,
        size: (usize, usize),
        activated: bool,
        fullscreen: bool,
    ) -> Result<(), String> {
        loop {
            let settled = self
                .demo
                .target
                .as_ref()
                .is_some_and(|frame| frame.released && frame.presented);
            if self.demo.current_size
                == Some(Size {
                    width: size.0,
                    height: size.1,
                })
                && self.demo.activated == activated
                && self.demo.fullscreen == fullscreen
                && settled
            {
                return Ok(());
            }
            let message = self.connection.next()?;
            if self.connection.handle_common(&message)? {
                continue;
            }
            self.demo
                .dispatch(&mut self.connection, &message, &self.runtime_directory)?;
        }
    }
}

#[cfg(test)]
pub fn present_for_test(
    stream: UnixStream,
    runtime_directory: &Path,
) -> Result<TestPresentation, String> {
    let connection = Connection {
        stream,
        buffered: Vec::with_capacity(16 * 1024),
        deadline: Instant::now().checked_add(Duration::from_secs(5)),
        next_id: FIRST_DYNAMIC_ID,
        free_ids: BTreeSet::new(),
    };
    let (mut connection, demo) = present(connection, runtime_directory)?;
    connection.finish_handshake()?;
    connection
        .stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| format!("set Wayland test event timeout: {e}"))?;
    Ok(TestPresentation {
        connection,
        demo,
        runtime_directory: runtime_directory.to_path_buf(),
    })
}

pub fn probe(path: &Path) -> Result<(), String> {
    UnixStream::connect(path)
        .map(|_| ())
        .map_err(|e| format!("connect UI readiness socket {}: {e}", path.display()))
}

pub fn selftest() -> Result<(), String> {
    let pixels = build_pixels(DEFAULT_WIDTH, DEFAULT_HEIGHT)?;
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
    let received = sys::recv_with_fds(&receiver, &mut bytes).map_err(|error| error.to_string())?;
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
        let pixels = build_pixels(DEFAULT_WIDTH, DEFAULT_HEIGHT).unwrap();
        assert_eq!(
            pixels.len(),
            DEFAULT_WIDTH * DEFAULT_HEIGHT * BYTES_PER_PIXEL
        );
        assert!(pixels.as_chunks::<4>().0.iter().all(|pixel| pixel[3] == 0));
        assert_eq!(build_pixels(1, 1).unwrap().len(), BYTES_PER_PIXEL);
        assert!(build_pixels(0, 1).is_err());
        assert!(build_pixels(MAX_UI_DIMENSION + 1, 1).is_err());
        assert!(build_pixels(MAX_UI_DIMENSION, MAX_UI_DIMENSION).is_err());
    }

    #[test]
    fn dynamic_object_ids_are_reused_only_after_delete_id() {
        let (stream, _peer) = UnixStream::pair().unwrap();
        let mut connection = Connection {
            stream,
            buffered: Vec::new(),
            deadline: None,
            next_id: FIRST_DYNAMIC_ID,
            free_ids: BTreeSet::new(),
        };
        assert_eq!(connection.allocate_id().unwrap(), 10);
        assert_eq!(connection.allocate_id().unwrap(), 11);

        let mut deleted = wire::Builder::new();
        deleted.u32(10);
        let mut bytes = deleted.message(DISPLAY, 1).unwrap();
        let event = wire::take(&mut bytes).unwrap().unwrap();
        assert!(connection.handle_common(&event).unwrap());
        assert_eq!(connection.allocate_id().unwrap(), 10);

        let mut duplicate = wire::Builder::new();
        duplicate.u32(10);
        let mut bytes = duplicate.message(DISPLAY, 1).unwrap();
        let event = wire::take(&mut bytes).unwrap().unwrap();
        assert!(connection.handle_common(&event).unwrap());
        let mut bytes = {
            let mut duplicate = wire::Builder::new();
            duplicate.u32(10);
            duplicate.message(DISPLAY, 1).unwrap()
        };
        let event = wire::take(&mut bytes).unwrap().unwrap();
        assert!(connection.handle_common(&event).is_err());

        let mut future = wire::Builder::new();
        future.u32(99);
        let mut bytes = future.message(DISPLAY, 1).unwrap();
        let event = wire::take(&mut bytes).unwrap().unwrap();
        assert!(connection.handle_common(&event).is_err());
    }

    #[test]
    fn toplevel_configure_parses_states_and_rejects_invalid_dimensions() {
        let mut states = Vec::new();
        states.extend_from_slice(&2u32.to_ne_bytes());
        states.extend_from_slice(&4u32.to_ne_bytes());
        let mut configured = wire::Builder::new();
        configured.i32(320);
        configured.i32(200);
        configured.array(&states).unwrap();
        let mut bytes = configured.message(XDG_TOPLEVEL, 0).unwrap();
        let event = wire::take(&mut bytes).unwrap().unwrap();
        let mut demo = Demo::new();
        demo.configure_toplevel(&event).unwrap();
        let pending = demo.pending_configure.unwrap();
        assert_eq!(
            pending.resolve_size(None),
            Size {
                width: 320,
                height: 200
            }
        );
        assert_eq!(pending.width, Some(320));
        assert_eq!(pending.height, Some(200));
        assert!(pending.activated);
        assert!(pending.fullscreen);

        for (width, height) in [(-1, 10), (10, -1)] {
            let mut invalid = wire::Builder::new();
            invalid.i32(width);
            invalid.i32(height);
            invalid.array(&[]).unwrap();
            let mut bytes = invalid.message(XDG_TOPLEVEL, 0).unwrap();
            let event = wire::take(&mut bytes).unwrap().unwrap();
            assert!(demo.configure_toplevel(&event).is_err());
        }

        for (width, height, expected) in [
            (
                0,
                10,
                Size {
                    width: 20,
                    height: 10,
                },
            ),
            (
                10,
                0,
                Size {
                    width: 10,
                    height: 30,
                },
            ),
        ] {
            let mut partial = wire::Builder::new();
            partial.i32(width);
            partial.i32(height);
            partial.array(&[]).unwrap();
            let mut bytes = partial.message(XDG_TOPLEVEL, 0).unwrap();
            let event = wire::take(&mut bytes).unwrap().unwrap();
            demo.configure_toplevel(&event).unwrap();
            assert_eq!(
                demo.pending_configure.unwrap().resolve_size(Some(Size {
                    width: 20,
                    height: 30
                })),
                expected
            );
        }
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
            next_id: FIRST_DYNAMIC_ID,
            free_ids: BTreeSet::new(),
        };
        assert_eq!(
            connection.next().unwrap_err(),
            "Wayland presentation handshake timed out"
        );
    }

    #[test]
    fn test_event_timeout_is_distinct_from_the_handshake_deadline() {
        let (stream, _peer) = UnixStream::pair().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_millis(1)))
            .unwrap();
        let mut connection = Connection {
            stream,
            buffered: Vec::new(),
            deadline: None,
            next_id: FIRST_DYNAMIC_ID,
            free_ids: BTreeSet::new(),
        };
        assert_eq!(
            connection.next().unwrap_err(),
            "Wayland event wait timed out"
        );
    }

    #[test]
    fn bare_surface_configure_reuses_current_toplevel_state() {
        let mut demo = Demo::new();
        demo.current_size = Some(Size {
            width: 320,
            height: 200,
        });
        demo.activated = true;
        demo.fullscreen = true;
        let configure = demo.take_configure();
        assert_eq!(
            configure.resolve_size(demo.current_size),
            Size {
                width: 320,
                height: 200,
            }
        );
        assert!(configure.activated);
        assert!(configure.fullscreen);
        assert!(!configure.selects_layout_size());
    }
}

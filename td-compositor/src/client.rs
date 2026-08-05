use crate::conn::{
    self, Connection, Globals, COMPOSITOR, KEYBOARD, POINTER, SEAT, SHM, SURFACE, XDG_SURFACE,
    XDG_TOPLEVEL, XDG_WM_BASE,
};

/// One past the last fixed id the DEMO creates. It binds a seat and creates a
/// keyboard and pointer, so its dynamic range starts higher than the
/// terminal's; see `conn`'s note on why this is per-client and must be dense.
const FIRST_DYNAMIC_ID: u32 = POINTER + 1;
use crate::keyboard::XKB_KEYMAP;
use crate::render::BYTES_PER_PIXEL;
use crate::scene::SHM_XRGB8888;
use crate::pointer::MAX_POINTER_FRAME_EVENTS;
use crate::ui::{KeyboardUpdate, PointerUpdate, UiKeyState, UiModel, UiModifiers};
use crate::{socket, sys, wire, MAX_UI_DIMENSION, MAX_UI_FRAME_BYTES};
use std::collections::BTreeSet;
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const DEFAULT_WIDTH: usize = 512;
const DEFAULT_HEIGHT: usize = 320;
const PRESENT_TIMEOUT: Duration = Duration::from_secs(20);

pub struct Options {
    pub socket: PathBuf,
    pub ready_socket: PathBuf,
}

fn build_pixels(width: usize, height: usize, ui: &UiModel) -> Result<Vec<u8>, String> {
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
    ui.paint(&mut pixels, width, height)?;
    Ok(pixels)
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
    seat_capabilities: Option<u32>,
    seat_devices_requested: bool,
    keymap_verified: bool,
    repeat: Option<(i32, i32)>,
    ui: UiModel,
    pending_pointer: Vec<PointerUpdate>,
    rendered_revision: u64,
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
            seat_capabilities: None,
            seat_devices_requested: false,
            keymap_verified: false,
            repeat: None,
            ui: UiModel::default(),
            pending_pointer: Vec::new(),
            rendered_revision: 0,
            activated: false,
            fullscreen: false,
        }
    }

    fn ready(&self) -> bool {
        self.layout_configured
            && self
                .seat_capabilities
                .is_some_and(|capabilities| capabilities & 3 == 3)
            && self.keymap_verified
            && self
                .target
                .as_ref()
                .is_some_and(|frame| frame.released && frame.presented)
    }

    fn target_settled(&self) -> bool {
        self.target
            .as_ref()
            .is_none_or(|frame| frame.released && frame.presented)
    }

    fn input_pending(&self) -> bool {
        self.rendered_revision != self.ui.revision()
    }

    fn maybe_commit_input(
        &mut self,
        connection: &mut Connection,
        runtime_directory: &Path,
    ) -> Result<(), String> {
        if !self.input_pending() || !self.target_settled() {
            return Ok(());
        }
        let Some(size) = self.current_size else {
            return Ok(());
        };
        self.commit_frame(connection, runtime_directory, size)
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
        let pixels = build_pixels(size.width, size.height, &self.ui)?;
        let (buffer_id, callback_id) = conn::attach_frame(
            connection,
            runtime_directory,
            "td-ui-demo",
            &pixels,
            size.width,
            size.height,
        )?;

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
        self.rendered_revision = self.ui.revision();
        self.target = Some(TargetFrame {
            buffer: buffer_id,
            callback: callback_id,
            released: false,
            presented: false,
        });
        Ok(())
    }

    fn seat_event(
        &mut self,
        connection: &mut Connection,
        message: &wire::Message,
    ) -> Result<(), String> {
        let mut args = wire::Cursor::new(&message.payload);
        match message.opcode {
            0 => {
                let capabilities = args.u32()?;
                args.finish()?;
                self.seat_capabilities = Some(capabilities);
                if capabilities & 3 == 3 && !self.seat_devices_requested {
                    let mut keyboard = wire::Builder::new();
                    keyboard.u32(KEYBOARD);
                    connection.send(SEAT, 1, keyboard)?;
                    let mut pointer = wire::Builder::new();
                    pointer.u32(POINTER);
                    connection.send(SEAT, 0, pointer)?;
                    self.seat_devices_requested = true;
                }
                Ok(())
            }
            1 => {
                args.string()?;
                args.finish()
            }
            _ => Err(format!(
                "unexpected wl_seat event opcode={}",
                message.opcode
            )),
        }
    }

    fn keyboard_event(
        &mut self,
        connection: &mut Connection,
        message: &wire::Message,
    ) -> Result<(), String> {
        let mut args = wire::Cursor::new(&message.payload);
        match message.opcode {
            0 => {
                let format = args.u32()?;
                let size = args.u32()?;
                args.finish()?;
                let mut file = connection.take_fd("wl_keyboard.keymap")?;
                if format != 1 {
                    return Err(format!("unsupported wl_keyboard keymap format {format}"));
                }
                let expected_size = XKB_KEYMAP
                    .len()
                    .checked_add(1)
                    .ok_or_else(|| "expected XKB keymap size overflow".to_string())?;
                let announced_size = usize::try_from(size)
                    .map_err(|_| "wl_keyboard keymap size escaped usize".to_string())?;
                if announced_size != expected_size {
                    return Err(format!(
                        "wl_keyboard keymap has size {announced_size}, expected {expected_size}"
                    ));
                }
                let metadata_size = usize::try_from(
                    file.metadata()
                        .map_err(|e| format!("stat wl_keyboard keymap: {e}"))?
                        .len(),
                )
                .map_err(|_| "wl_keyboard keymap file size escaped usize".to_string())?;
                if metadata_size != expected_size {
                    return Err(format!(
                        "wl_keyboard keymap file has size {metadata_size}, expected {expected_size}"
                    ));
                }
                let bytes = read_keymap_bytes(&mut file, expected_size)?;
                let body = bytes
                    .get(..XKB_KEYMAP.len())
                    .ok_or_else(|| "wl_keyboard keymap is truncated".to_string())?;
                if body != XKB_KEYMAP.as_bytes() || bytes.last().copied() != Some(0) {
                    return Err("wl_keyboard keymap differs from td's pinned keymap".into());
                }
                self.keymap_verified = true;
                Ok(())
            }
            1 => {
                args.u32()?;
                let surface = args.u32()?;
                if surface != SURFACE {
                    return Err(format!("wl_keyboard entered unexpected surface {surface}"));
                }
                let byte_count = usize::try_from(args.u32()?)
                    .map_err(|_| "wl_keyboard key array size escaped usize".to_string())?;
                if !byte_count.is_multiple_of(4) || byte_count / 4 > 256 {
                    return Err(format!(
                        "wl_keyboard key array has invalid length {byte_count}"
                    ));
                }
                let mut keys = BTreeSet::new();
                for _ in 0..byte_count / 4 {
                    keys.insert(args.u32()?);
                }
                args.finish()?;
                self.ui.keyboard(KeyboardUpdate::Enter { keys })?;
                Ok(())
            }
            2 => {
                args.u32()?;
                let surface = args.u32()?;
                args.finish()?;
                if surface != SURFACE {
                    return Err(format!("wl_keyboard left unexpected surface {surface}"));
                }
                self.ui.keyboard(KeyboardUpdate::Leave)?;
                Ok(())
            }
            3 => {
                args.u32()?;
                args.u32()?;
                let key = args.u32()?;
                let state = key_state(args.u32()?, "wl_keyboard key")?;
                args.finish()?;
                self.ui.keyboard(KeyboardUpdate::Key { key, state })?;
                Ok(())
            }
            4 => {
                args.u32()?;
                let modifiers = UiModifiers {
                    depressed: args.u32()?,
                    latched: args.u32()?,
                    locked: args.u32()?,
                    group: args.u32()?,
                };
                args.finish()?;
                self.ui.keyboard(KeyboardUpdate::Modifiers(modifiers))?;
                Ok(())
            }
            5 => {
                let rate = args.i32()?;
                let delay = args.i32()?;
                args.finish()?;
                if rate < 0 || delay < 0 {
                    return Err(format!(
                        "wl_keyboard supplied invalid repeat rate={rate} delay={delay}"
                    ));
                }
                self.repeat = Some((rate, delay));
                Ok(())
            }
            _ => Err(format!(
                "unexpected wl_keyboard event opcode={}",
                message.opcode
            )),
        }
    }

    fn pointer_event(&mut self, message: &wire::Message) -> Result<(), String> {
        let mut args = wire::Cursor::new(&message.payload);
        let update = match message.opcode {
            0 => {
                args.u32()?;
                let surface = args.u32()?;
                if surface != SURFACE {
                    return Err(format!("wl_pointer entered unexpected surface {surface}"));
                }
                PointerUpdate::Enter {
                    x: args.i32()?,
                    y: args.i32()?,
                }
            }
            1 => {
                args.u32()?;
                let surface = args.u32()?;
                if surface != SURFACE {
                    return Err(format!("wl_pointer left unexpected surface {surface}"));
                }
                PointerUpdate::Leave
            }
            2 => {
                args.u32()?;
                PointerUpdate::Motion {
                    x: args.i32()?,
                    y: args.i32()?,
                }
            }
            3 => {
                args.u32()?;
                args.u32()?;
                let button = args.u32()?;
                let state = key_state(args.u32()?, "wl_pointer button")?;
                PointerUpdate::Button { button, state }
            }
            5 => {
                args.finish()?;
                let mut pending = std::mem::take(&mut self.pending_pointer);
                let result = self.ui.pointer_frame(&pending);
                pending.clear();
                self.pending_pointer = pending;
                result?;
                return Ok(());
            }
            4 => {
                args.u32()?;
                pointer_axis(args.u32()?)?;
                args.i32()?;
                args.finish()?;
                return Ok(());
            }
            6 => {
                let source = args.u32()?;
                args.finish()?;
                if source > 3 {
                    return Err(format!("wl_pointer supplied invalid axis source {source}"));
                }
                return Ok(());
            }
            7 => {
                args.u32()?;
                pointer_axis(args.u32()?)?;
                args.finish()?;
                return Ok(());
            }
            8 | 9 => {
                pointer_axis(args.u32()?)?;
                args.i32()?;
                args.finish()?;
                return Ok(());
            }
            10 => {
                pointer_axis(args.u32()?)?;
                let direction = args.u32()?;
                args.finish()?;
                if direction > 1 {
                    return Err(format!(
                        "wl_pointer supplied invalid axis relative direction {direction}"
                    ));
                }
                return Ok(());
            }
            _ => {
                return Err(format!(
                    "unexpected wl_pointer event opcode={}",
                    message.opcode
                ))
            }
        };
        args.finish()?;
        if self.pending_pointer.len() >= MAX_POINTER_FRAME_EVENTS {
            return Err(format!(
                "wl_pointer frame exceeds {MAX_POINTER_FRAME_EVENTS} events"
            ));
        }
        self.pending_pointer.push(update);
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
        if message.object == SEAT {
            return self.seat_event(connection, message);
        }
        if message.object == KEYBOARD {
            return self.keyboard_event(connection, message);
        }
        if message.object == POINTER {
            return self.pointer_event(message);
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

    fn dispatch_and_render(
        &mut self,
        connection: &mut Connection,
        message: &wire::Message,
        runtime_directory: &Path,
    ) -> Result<(), String> {
        self.dispatch(connection, message, runtime_directory)?;
        self.maybe_commit_input(connection, runtime_directory)
    }
}

fn key_state(value: u32, event: &str) -> Result<UiKeyState, String> {
    match value {
        0 => Ok(UiKeyState::Released),
        1 => Ok(UiKeyState::Pressed),
        _ => Err(format!("{event} has invalid state {value}")),
    }
}

fn pointer_axis(axis: u32) -> Result<(), String> {
    if axis <= 1 {
        Ok(())
    } else {
        Err(format!("wl_pointer supplied invalid axis {axis}"))
    }
}

fn read_keymap_bytes(file: &mut File, expected_size: usize) -> Result<Vec<u8>, String> {
    let read_bound = expected_size
        .checked_add(1)
        .and_then(|value| u64::try_from(value).ok())
        .ok_or_else(|| "wl_keyboard keymap read bound overflow".to_string())?;
    let mut bytes = Vec::with_capacity(expected_size);
    Read::by_ref(file)
        .take(read_bound)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("read wl_keyboard keymap: {e}"))?;
    if bytes.len() != expected_size {
        return Err(format!(
            "wl_keyboard keymap read {} bytes, expected {expected_size}",
            bytes.len()
        ));
    }
    Ok(bytes)
}

fn present(
    mut connection: Connection,
    runtime_directory: &Path,
) -> Result<(Connection, Demo), String> {
    let globals = conn::discover_globals(&mut connection)?;
    let (compositor_name, compositor_version) =
        Globals::require(globals.compositor(), "wl_compositor", 4, 4)?;
    let (shm_name, shm_version) = Globals::require(globals.shm(), "wl_shm", 1, 1)?;
    let (xdg_name, xdg_version) = Globals::require(globals.xdg_wm_base(), "xdg_wm_base", 1, 1)?;
    let (seat_name, seat_version) = Globals::require(globals.seat(), "wl_seat", 5, 7)?;
    conn::bind(
        &mut connection,
        compositor_name,
        "wl_compositor",
        compositor_version,
        COMPOSITOR,
    )?;
    conn::bind(&mut connection, shm_name, "wl_shm", shm_version, SHM)?;
    conn::bind(
        &mut connection,
        xdg_name,
        "xdg_wm_base",
        xdg_version,
        XDG_WM_BASE,
    )?;
    conn::bind(&mut connection, seat_name, "wl_seat", seat_version, SEAT)?;
    conn::create_surface(&mut connection, "td software Wayland demo")?;

    let mut demo = Demo::new();
    while !demo.ready() {
        let message = connection.next()?;
        if connection.handle_common(&message)? {
            continue;
        }
        demo.dispatch_and_render(&mut connection, &message, runtime_directory)?;
    }
    let unclaimed = connection.pending_fd_count();
    if unclaimed != 0 {
        return Err(format!(
            "Wayland presentation retained {unclaimed} unexpected descriptors"
        ));
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
    let connection = Connection::connect(&options.socket, deadline, FIRST_DYNAMIC_ID)?;
    let (mut connection, mut demo) = present(connection, runtime_directory)?;
    let size = demo
        .current_size
        .ok_or_else(|| "demo became ready without a configured size".to_string())?;
    connection.finish_handshake()?;

    let _ready = socket::publish(&options.ready_socket, "ui-demo-ready", Vec::new())?;
    // NOTE: `println!` panics on a write failure, the hazard the terminal's
    // own marker avoids. It is left alone here because the recipe pins this
    // exact spelling as the demo's boot oracle, so changing it is that
    // oracle's landing rather than this one.
    println!("TD-UI-CLIENT-READY surface={}x{}", size.width, size.height);
    std::io::stdout()
        .flush()
        .map_err(|e| format!("flush UI client ready marker: {e}"))?;

    loop {
        let message = connection.next()?;
        if connection.handle_common(&message)? {
            continue;
        }
        demo.dispatch_and_render(&mut connection, &message, runtime_directory)?;
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
            self.demo.dispatch_and_render(
                &mut self.connection,
                &message,
                &self.runtime_directory,
            )?;
        }
    }

    pub fn wait_for_pointer(&mut self, pointer: (i32, i32), button: u32) -> Result<(), String> {
        loop {
            if self
                .demo
                .ui
                .pointer_has_button(pointer.0, pointer.1, button)
                && self.demo.rendered_revision == self.demo.ui.revision()
                && self.demo.target_settled()
            {
                return Ok(());
            }
            let message = self.connection.next()?;
            if self.connection.handle_common(&message)? {
                continue;
            }
            self.demo.dispatch_and_render(
                &mut self.connection,
                &message,
                &self.runtime_directory,
            )?;
        }
    }

    pub fn wait_for_key(&mut self, key: u32, state: UiKeyState) -> Result<(), String> {
        loop {
            if self.demo.ui.last_key() == Some((key, state))
                && self.demo.rendered_revision == self.demo.ui.revision()
                && self.demo.target_settled()
            {
                return Ok(());
            }
            let message = self.connection.next()?;
            if self.connection.handle_common(&message)? {
                continue;
            }
            self.demo.dispatch_and_render(
                &mut self.connection,
                &message,
                &self.runtime_directory,
            )?;
        }
    }
}

#[cfg(test)]
pub fn present_for_test(
    stream: UnixStream,
    runtime_directory: &Path,
) -> Result<TestPresentation, String> {
    let connection = Connection::over(
        stream,
        Instant::now().checked_add(Duration::from_secs(5)),
        FIRST_DYNAMIC_ID,
    );
    let (mut connection, demo) = present(connection, runtime_directory)?;
    connection.finish_handshake()?;
    connection.set_read_timeout(Some(Duration::from_secs(5)))?;
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
    let pixels = build_pixels(DEFAULT_WIDTH, DEFAULT_HEIGHT, &UiModel::default())?;
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
    let file = conn::backing_file(&std::env::temp_dir(), "td-ui-demo", first)?;
    sys::send_with_fd(&sender, b"demo", file.as_raw_fd())
        .map_err(|e| format!("send descriptor self-test: {e}"))?;
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
    use crate::conn::{DISPLAY, MAX_PENDING_FDS};
    use std::os::fd::IntoRawFd;

    fn test_connection(stream: UnixStream) -> Connection {
        Connection::over(stream, None, FIRST_DYNAMIC_ID)
    }

    fn event(object: u32, opcode: u16, builder: wire::Builder) -> wire::Message {
        let mut bytes = builder.message(object, opcode).unwrap();
        wire::take(&mut bytes).unwrap().unwrap()
    }

    fn assert_socket_eof(mut stream: UnixStream) {
        stream
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let mut byte = [0u8; 1];
        assert_eq!(stream.read(&mut byte).unwrap(), 0);
    }

    #[test]
    fn pattern_is_exactly_one_xrgb_surface() {
        let pixels = build_pixels(DEFAULT_WIDTH, DEFAULT_HEIGHT, &UiModel::default()).unwrap();
        assert_eq!(
            pixels.len(),
            DEFAULT_WIDTH * DEFAULT_HEIGHT * BYTES_PER_PIXEL
        );
        assert!(pixels.as_chunks::<4>().0.iter().all(|pixel| pixel[3] == 0));
        assert_eq!(
            build_pixels(1, 1, &UiModel::default()).unwrap().len(),
            BYTES_PER_PIXEL
        );
        assert!(build_pixels(0, 1, &UiModel::default()).is_err());
        assert!(build_pixels(MAX_UI_DIMENSION + 1, 1, &UiModel::default()).is_err());
        assert!(build_pixels(MAX_UI_DIMENSION, MAX_UI_DIMENSION, &UiModel::default()).is_err());
    }

    #[test]
    fn dynamic_object_ids_are_reused_only_after_delete_id() {
        let (stream, _peer) = UnixStream::pair().unwrap();
        let mut connection = Connection::over(stream, None, FIRST_DYNAMIC_ID);
        assert_eq!(connection.allocate_id().unwrap(), 13);
        assert_eq!(connection.allocate_id().unwrap(), 14);

        let mut deleted = wire::Builder::new();
        deleted.u32(13);
        let mut bytes = deleted.message(DISPLAY, 1).unwrap();
        let event = wire::take(&mut bytes).unwrap().unwrap();
        assert!(connection.handle_common(&event).unwrap());
        assert_eq!(connection.allocate_id().unwrap(), 13);

        let mut duplicate = wire::Builder::new();
        duplicate.u32(13);
        let mut bytes = duplicate.message(DISPLAY, 1).unwrap();
        let event = wire::take(&mut bytes).unwrap().unwrap();
        assert!(connection.handle_common(&event).unwrap());
        let mut bytes = {
            let mut duplicate = wire::Builder::new();
            duplicate.u32(13);
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
            Globals::require(globals.compositor(), "wl_compositor", 4, 4).unwrap(),
            (9, 4)
        );

        let mut globals = Globals::default();
        globals.record(9, "wl_compositor", 7);
        assert_eq!(
            Globals::require(globals.compositor(), "wl_compositor", 4, 4).unwrap(),
            (9, 4)
        );

        let mut globals = Globals::default();
        globals.record(10, "wl_seat", 4);
        assert!(Globals::require(globals.seat(), "wl_seat", 5, 7).is_err());
        globals.record(11, "wl_seat", 9);
        assert_eq!(
            Globals::require(globals.seat(), "wl_seat", 5, 7).unwrap(),
            (11, 7)
        );
    }

    #[test]
    fn readiness_requires_layout_seat_keymap_release_and_presentation() {
        let mut demo = Demo::new();
        demo.target = Some(TargetFrame {
            buffer: 20,
            callback: 21,
            released: false,
            presented: false,
        });
        assert!(!demo.ready());
        demo.layout_configured = true;
        assert!(!demo.ready());
        demo.seat_capabilities = Some(3);
        assert!(!demo.ready());
        demo.keymap_verified = true;
        assert!(!demo.ready());
        demo.target.as_mut().unwrap().released = true;
        assert!(!demo.ready());
        demo.target.as_mut().unwrap().presented = true;
        assert!(demo.ready());
        demo.seat_capabilities = Some(2);
        assert!(!demo.ready());
    }

    #[test]
    fn seat_events_validate_capabilities_name_and_opcode() {
        let (stream, mut peer) = UnixStream::pair().unwrap();
        let mut connection = test_connection(stream);
        let mut demo = Demo::new();
        let mut keyboard_only = wire::Builder::new();
        keyboard_only.u32(2);
        demo.seat_event(&mut connection, &event(SEAT, 0, keyboard_only))
            .unwrap();
        assert_eq!(demo.seat_capabilities, Some(2));
        assert!(!demo.seat_devices_requested);
        peer.set_nonblocking(true).unwrap();
        let mut unexpected = [0; 1];
        assert_eq!(
            peer.read(&mut unexpected).unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        peer.set_nonblocking(false).unwrap();

        let mut capabilities = wire::Builder::new();
        capabilities.u32(3);
        demo.seat_event(&mut connection, &event(SEAT, 0, capabilities))
            .unwrap();
        assert_eq!(demo.seat_capabilities, Some(3));
        assert!(demo.seat_devices_requested);

        let mut repeated = wire::Builder::new();
        repeated.u32(3);
        demo.seat_event(&mut connection, &event(SEAT, 0, repeated))
            .unwrap();
        let mut requests = vec![0; 24];
        peer.read_exact(&mut requests).unwrap();
        let keyboard = wire::take(&mut requests).unwrap().unwrap();
        let pointer = wire::take(&mut requests).unwrap().unwrap();
        assert_eq!((keyboard.object, keyboard.opcode), (SEAT, 1));
        assert_eq!((pointer.object, pointer.opcode), (SEAT, 0));
        assert!(requests.is_empty());
        let mut keyboard_args = wire::Cursor::new(&keyboard.payload);
        assert_eq!(keyboard_args.u32().unwrap(), KEYBOARD);
        keyboard_args.finish().unwrap();
        let mut pointer_args = wire::Cursor::new(&pointer.payload);
        assert_eq!(pointer_args.u32().unwrap(), POINTER);
        pointer_args.finish().unwrap();
        peer.set_nonblocking(true).unwrap();
        let mut unexpected = [0; 1];
        assert_eq!(
            peer.read(&mut unexpected).unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );

        let mut name = wire::Builder::new();
        name.string("seat0").unwrap();
        demo.seat_event(&mut connection, &event(SEAT, 1, name))
            .unwrap();

        let mut missing_pointer = wire::Builder::new();
        missing_pointer.u32(2);
        demo.seat_event(&mut connection, &event(SEAT, 0, missing_pointer))
            .unwrap();
        assert_eq!(demo.seat_capabilities, Some(2));
        assert!(demo.seat_devices_requested);
        assert!(demo
            .seat_event(&mut connection, &event(SEAT, 9, wire::Builder::new()))
            .unwrap_err()
            .contains("unexpected"));
    }

    #[test]
    fn pinned_keymap_descriptor_is_required_and_verified_exactly() {
        let expected = {
            let mut bytes = XKB_KEYMAP.as_bytes().to_vec();
            bytes.push(0);
            bytes
        };
        let (stream, peer) = UnixStream::pair().unwrap();
        let mut connection = test_connection(stream);
        let file = conn::backing_file(&std::env::temp_dir(), "td-ui-demo-test", &expected).unwrap();
        let mut keymap = wire::Builder::new();
        keymap.u32(1);
        keymap.u32(u32::try_from(expected.len()).unwrap());
        let bytes = keymap.message(KEYBOARD, 0).unwrap();
        sys::send_with_fd(&peer, &bytes, file.as_raw_fd()).unwrap();
        let message = connection.next().unwrap();
        let mut demo = Demo::new();
        demo.keyboard_event(&mut connection, &message).unwrap();
        assert!(demo.keymap_verified);
        assert_eq!(connection.pending_fd_count(), 0);

        let (stream, peer) = UnixStream::pair().unwrap();
        let mut connection = test_connection(stream);
        let mut changed = expected.clone();
        changed[0] ^= 1;
        let file = conn::backing_file(&std::env::temp_dir(), "td-ui-demo-test", &changed).unwrap();
        let mut keymap = wire::Builder::new();
        keymap.u32(1);
        keymap.u32(u32::try_from(changed.len()).unwrap());
        let bytes = keymap.message(KEYBOARD, 0).unwrap();
        sys::send_with_fd(&peer, &bytes, file.as_raw_fd()).unwrap();
        let message = connection.next().unwrap();
        assert!(Demo::new()
            .keyboard_event(&mut connection, &message)
            .is_err());
        assert_eq!(connection.pending_fd_count(), 0);

        let (stream, _peer) = UnixStream::pair().unwrap();
        let mut connection = test_connection(stream);
        let mut keymap = wire::Builder::new();
        keymap.u32(1);
        keymap.u32(u32::try_from(expected.len()).unwrap());
        let message = event(KEYBOARD, 0, keymap);
        assert!(Demo::new()
            .keyboard_event(&mut connection, &message)
            .is_err());
    }

    #[test]
    fn keymap_rejects_format_and_announced_size_after_consuming_fd() {
        let mut expected = XKB_KEYMAP.as_bytes().to_vec();
        expected.push(0);
        for (format, size, expected_error) in [
            (
                0,
                u32::try_from(expected.len()).unwrap(),
                "unsupported wl_keyboard keymap format",
            ),
            (1, 1, "wl_keyboard keymap has size"),
        ] {
            let (stream, peer) = UnixStream::pair().unwrap();
            let mut connection = test_connection(stream);
            let file = conn::backing_file(&std::env::temp_dir(), "td-ui-demo-test", &expected).unwrap();
            let mut keymap = wire::Builder::new();
            keymap.u32(format);
            keymap.u32(size);
            let bytes = keymap.message(KEYBOARD, 0).unwrap();
            sys::send_with_fd(&peer, &bytes, file.as_raw_fd()).unwrap();
            let message = connection.next().unwrap();
            let error = Demo::new()
                .keyboard_event(&mut connection, &message)
                .unwrap_err();
            assert!(error.contains(expected_error));
            assert_eq!(connection.pending_fd_count(), 0);
        }
    }

    #[test]
    fn keymap_read_is_bounded_against_growth_after_metadata() {
        let bytes = vec![7u8; 18];
        let file = conn::backing_file(&std::env::temp_dir(), "td-ui-demo-test", &bytes).unwrap();
        let mut file = sys::duplicate_received(file.into_raw_fd()).unwrap();
        let error = read_keymap_bytes(&mut file, 16).unwrap_err();
        assert!(error.contains("read 17 bytes, expected 16"));
    }

    #[test]
    fn framed_reads_keep_descriptors_with_their_event() {
        let (stream, mut peer) = UnixStream::pair().unwrap();
        let mut connection = test_connection(stream);
        let first = wire::Builder::new().message(SEAT, 1).unwrap();
        let second = wire::Builder::new().message(SEAT, 1).unwrap();
        let mut ordinary = first.clone();
        ordinary.extend_from_slice(&second);
        peer.write_all(ordinary.get(..3).unwrap()).unwrap();
        peer.write_all(ordinary.get(3..).unwrap()).unwrap();

        let descriptor_bytes = b"descriptor-order";
        let file = conn::backing_file(&std::env::temp_dir(), "td-ui-demo-test", descriptor_bytes).unwrap();
        let mut descriptor_event = wire::Builder::new();
        descriptor_event.u32(1);
        descriptor_event.u32(u32::try_from(descriptor_bytes.len()).unwrap());
        let descriptor_event = descriptor_event.message(KEYBOARD, 0).unwrap();
        sys::send_with_fd(&peer, &descriptor_event, file.as_raw_fd()).unwrap();

        for _ in 0..2 {
            let message = connection.next().unwrap();
            assert_eq!((message.object, message.opcode), (SEAT, 1));
            assert_eq!(connection.pending_fd_count(), 0);
        }
        let message = connection.next().unwrap();
        assert_eq!((message.object, message.opcode), (KEYBOARD, 0));
        let mut received = connection.take_fd("test descriptor").unwrap();
        let mut bytes = Vec::new();
        received.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, descriptor_bytes);
        assert_eq!(connection.pending_fd_count(), 0);
    }

    #[test]
    fn descriptor_overflow_closes_every_received_descriptor() {
        let (stream, peer) = UnixStream::pair().unwrap();
        let mut connection = test_connection(stream);
        let mut event = wire::Builder::new();
        event.u32(0);
        let bytes = event.message(KEYBOARD, 0).unwrap();
        let mut sources = Vec::new();
        let mut observers = Vec::new();
        for byte in bytes.iter().take(MAX_PENDING_FDS + 1) {
            let (source, observer) = UnixStream::pair().unwrap();
            sys::send_with_fd(&peer, std::slice::from_ref(byte), source.as_raw_fd()).unwrap();
            sources.push(source);
            observers.push(observer);
        }
        drop(sources);

        let error = connection.next().unwrap_err();
        assert!(error.contains("queued more than"));
        assert_eq!(connection.pending_fd_count(), 0);
        for observer in observers {
            assert_socket_eof(observer);
        }
    }

    #[test]
    fn a_coalesced_descriptor_waits_for_its_following_event() {
        let (stream, peer) = UnixStream::pair().unwrap();
        let mut connection = test_connection(stream);
        let first = wire::Builder::new().message(SEAT, 1).unwrap();
        let second = wire::Builder::new().message(KEYBOARD, 0).unwrap();
        let mut events = first;
        events.extend_from_slice(&second);
        let source = conn::backing_file(&std::env::temp_dir(), "td-ui-demo-test", b"coalesced-fd").unwrap();
        sys::send_with_fd(&peer, &events, source.as_raw_fd()).unwrap();
        drop(source);

        let message = connection.next().unwrap();
        assert_eq!((message.object, message.opcode), (SEAT, 1));
        assert_eq!(connection.pending_fd_count(), 1);
        let message = connection.next().unwrap();
        assert_eq!((message.object, message.opcode), (KEYBOARD, 0));
        assert_eq!(connection.pending_fd_count(), 1);
        let mut received = connection.take_fd("coalesced test").unwrap();
        let mut contents = Vec::new();
        received.read_to_end(&mut contents).unwrap();
        assert_eq!(contents, b"coalesced-fd");
        assert_eq!(connection.pending_fd_count(), 0);
    }

    #[test]
    fn dropping_a_connection_closes_an_unclaimed_descriptor() {
        let (stream, _peer) = UnixStream::pair().unwrap();
        let mut connection = test_connection(stream);
        let (owned, observer) = UnixStream::pair().unwrap();
        connection.queue_fd_for_test(owned.into_raw_fd());
        drop(connection);
        assert_socket_eof(observer);
    }

    #[test]
    fn keyboard_events_validate_focus_state_and_repeat_metadata() {
        let (stream, _peer) = UnixStream::pair().unwrap();
        let mut connection = test_connection(stream);
        let mut demo = Demo::new();
        let mut held = Vec::new();
        held.extend_from_slice(&30u32.to_ne_bytes());
        held.extend_from_slice(&31u32.to_ne_bytes());
        let mut enter = wire::Builder::new();
        enter.u32(1);
        enter.u32(SURFACE);
        enter.array(&held).unwrap();
        demo.keyboard_event(&mut connection, &event(KEYBOARD, 1, enter))
            .unwrap();
        assert_eq!(demo.ui.revision(), 1);

        let mut key = wire::Builder::new();
        key.u32(2);
        key.u32(10);
        key.u32(30);
        key.u32(0);
        demo.keyboard_event(&mut connection, &event(KEYBOARD, 3, key))
            .unwrap();
        assert_eq!(demo.ui.last_key(), Some((30, UiKeyState::Released)));

        let mut modifiers = wire::Builder::new();
        modifiers.u32(3);
        modifiers.u32(4);
        modifiers.u32(5);
        modifiers.u32(6);
        modifiers.u32(7);
        demo.keyboard_event(&mut connection, &event(KEYBOARD, 4, modifiers))
            .unwrap();

        let mut repeat = wire::Builder::new();
        repeat.i32(25);
        repeat.i32(600);
        demo.keyboard_event(&mut connection, &event(KEYBOARD, 5, repeat))
            .unwrap();
        assert_eq!(demo.repeat, Some((25, 600)));

        let mut invalid = wire::Builder::new();
        invalid.u32(4);
        invalid.u32(11);
        invalid.u32(31);
        invalid.u32(2);
        let revision = demo.ui.revision();
        assert!(demo
            .keyboard_event(&mut connection, &event(KEYBOARD, 3, invalid))
            .is_err());
        assert_eq!(demo.ui.revision(), revision);
    }

    #[test]
    fn pointer_updates_are_bounded_and_atomic_at_frame() {
        let mut demo = Demo::new();
        let mut enter = wire::Builder::new();
        enter.u32(1);
        enter.u32(SURFACE);
        enter.i32(20 * 256);
        enter.i32(30 * 256);
        demo.pointer_event(&event(POINTER, 0, enter)).unwrap();

        let mut motion = wire::Builder::new();
        motion.u32(2);
        motion.i32(24 * 256);
        motion.i32(34 * 256);
        demo.pointer_event(&event(POINTER, 2, motion)).unwrap();
        assert_eq!(demo.ui.revision(), 0);
        assert_eq!(demo.pending_pointer.len(), 2);
        let capacity = demo.pending_pointer.capacity();

        demo.pointer_event(&event(POINTER, 5, wire::Builder::new()))
            .unwrap();
        assert_eq!(demo.ui.revision(), 1);
        assert!(demo.pending_pointer.is_empty());
        assert!(demo.pending_pointer.capacity() >= capacity);

        let mut axis = wire::Builder::new();
        axis.u32(3);
        axis.u32(0);
        axis.i32(-256);
        demo.pointer_event(&event(POINTER, 4, axis)).unwrap();
        let mut source = wire::Builder::new();
        source.u32(0);
        demo.pointer_event(&event(POINTER, 6, source)).unwrap();
        let mut stop = wire::Builder::new();
        stop.u32(4);
        stop.u32(1);
        demo.pointer_event(&event(POINTER, 7, stop)).unwrap();
        let mut discrete = wire::Builder::new();
        discrete.u32(1);
        discrete.i32(-1);
        demo.pointer_event(&event(POINTER, 8, discrete)).unwrap();
        assert_eq!(demo.ui.revision(), 1);
        assert!(demo.pending_pointer.is_empty());

        let mut invalid = wire::Builder::new();
        invalid.u32(3);
        invalid.u32(4);
        invalid.u32(0x110);
        invalid.u32(2);
        assert!(demo.pointer_event(&event(POINTER, 3, invalid)).is_err());
        assert!(demo.pending_pointer.is_empty());

        demo.pending_pointer =
            vec![PointerUpdate::Leave; MAX_POINTER_FRAME_EVENTS.saturating_sub(1)];
        let mut leave = wire::Builder::new();
        leave.u32(5);
        leave.u32(SURFACE);
        demo.pointer_event(&event(POINTER, 1, leave)).unwrap();
        assert_eq!(demo.pending_pointer.len(), MAX_POINTER_FRAME_EVENTS);
        let mut overflow = wire::Builder::new();
        overflow.u32(6);
        overflow.u32(SURFACE);
        assert!(demo.pointer_event(&event(POINTER, 1, overflow)).is_err());
        assert_eq!(demo.pending_pointer.len(), MAX_POINTER_FRAME_EVENTS);

        let mut invalid_axis = wire::Builder::new();
        invalid_axis.u32(5);
        invalid_axis.u32(2);
        invalid_axis.i32(1);
        assert!(demo
            .pointer_event(&event(POINTER, 4, invalid_axis))
            .is_err());
    }

    #[test]
    fn input_rendering_coalesces_while_a_frame_is_in_flight() {
        let (stream, _peer) = UnixStream::pair().unwrap();
        let mut connection = test_connection(stream);
        connection.set_next_id_for_test(20);
        let mut demo = Demo::new();
        demo.xrgb_advertised = true;
        demo.current_size = Some(Size {
            width: 96,
            height: 64,
        });
        demo.target = Some(TargetFrame {
            buffer: 18,
            callback: 19,
            released: false,
            presented: false,
        });
        demo.ui
            .keyboard(KeyboardUpdate::Enter {
                keys: BTreeSet::new(),
            })
            .unwrap();
        demo.maybe_commit_input(&mut connection, &std::env::temp_dir())
            .unwrap();
        assert_eq!(connection.next_id_for_test(), 20);
        assert_eq!(demo.rendered_revision, 0);

        let target = demo.target.as_mut().unwrap();
        target.released = true;
        target.presented = true;
        demo.maybe_commit_input(&mut connection, &std::env::temp_dir())
            .unwrap();
        assert_eq!(connection.next_id_for_test(), 23);
        assert_eq!(demo.rendered_revision, demo.ui.revision());

        demo.ui
            .keyboard(KeyboardUpdate::Key {
                key: 30,
                state: UiKeyState::Pressed,
            })
            .unwrap();
        demo.ui
            .keyboard(KeyboardUpdate::Key {
                key: 31,
                state: UiKeyState::Pressed,
            })
            .unwrap();
        demo.maybe_commit_input(&mut connection, &std::env::temp_dir())
            .unwrap();
        assert_eq!(connection.next_id_for_test(), 23);
        assert!(demo.input_pending());
    }

    #[test]
    fn expired_presentation_deadline_does_not_block_on_a_stalled_peer() {
        let (stream, _peer) = UnixStream::pair().unwrap();
        let mut connection = Connection::over(stream, Some(Instant::now()), FIRST_DYNAMIC_ID);
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
        let mut connection = Connection::over(stream, None, FIRST_DYNAMIC_ID);
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

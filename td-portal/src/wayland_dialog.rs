//! One bounded private-Wayland FileChooser dialog.

use crate::file_chooser::{self, Action, Chooser, FileFilter, Mode, Outcome};
use crate::keyboard::{MOD_ALT, MOD_CAPS, MOD_CONTROL, MOD_LOGO, MOD_SHIFT, XKB_KEYMAP};
use crate::wayland_channel::EXPECTED_GLOBALS;
use crate::{sys, wayland_wire as wire};
use std::collections::{BTreeSet, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
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
const SEAT: u32 = 10;
const KEYBOARD: u32 = 11;
const PORTAL_MANAGER: u32 = 12;
const FIRST_DYNAMIC_ID: u32 = 13;

const SEAT_KEYBOARD: u32 = 2;
const KEY_RELEASED: u32 = 0;
const KEY_PRESSED: u32 = 1;
const SHM_XRGB8888: u32 = 1;
const PORTAL_DIALOG_DISMISSED: u32 = 2;
const MAX_PENDING_FDS: usize = 8;
const MAX_HELD_KEYS: usize = 256;
const RECEIVE_BUFFER_BYTES: usize = 16 * 1024;
const MAX_BUFFERED_BYTES: usize = 256 * 1024;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(20);
const IDLE_PULSE: Duration = Duration::from_secs(10);
const CONNECT_ATTEMPTS: usize = 200;

#[derive(Debug)]
pub struct DialogConfig {
    pub socket: PathBuf,
    pub runtime_directory: PathBuf,
    pub title: String,
    pub parent_handle: String,
    pub app_id: String,
    pub host_root: PathBuf,
    pub guest_root: PathBuf,
    pub mode: Mode,
    pub accept_label: Option<String>,
    pub filter: Option<FileFilter>,
    pub connector: Arc<AtomicBool>,
}

#[derive(Debug)]
pub enum Notice {
    Connected(UnixStream),
    Presented {
        width: usize,
        height: usize,
        checksum: u64,
    },
    Completed(Result<Outcome, String>),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Global {
    name: u32,
    version: u32,
}

#[derive(Debug)]
struct Globals {
    ordered: Vec<(u32, String, u32)>,
    compositor: Option<Global>,
    shm: Option<Global>,
    xdg_wm_base: Option<Global>,
    seat: Option<Global>,
    portal_manager: Option<Global>,
}

struct ConnectLease(Arc<AtomicBool>);

impl ConnectLease {
    fn acquire(active: Arc<AtomicBool>) -> Result<Self, String> {
        active
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| "the private Wayland connector is already occupied".to_string())?;
        Ok(Self(active))
    }
}

impl Drop for ConnectLease {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

impl Globals {
    fn new() -> Self {
        Self {
            ordered: Vec::with_capacity(EXPECTED_GLOBALS.len()),
            compositor: None,
            shm: None,
            xdg_wm_base: None,
            seat: None,
            portal_manager: None,
        }
    }

    fn record(&mut self, name: u32, interface: String, version: u32) -> Result<(), String> {
        if name == 0 || self.ordered.iter().any(|(seen, _, _)| *seen == name) {
            return Err(format!(
                "private portal registry repeated invalid global name {name}"
            ));
        }
        if self.ordered.len() >= EXPECTED_GLOBALS.len() {
            return Err(format!(
                "private portal registry advertised more than {} globals",
                EXPECTED_GLOBALS.len()
            ));
        }
        let global = Global { name, version };
        match interface.as_str() {
            "wl_compositor" => self.compositor = Some(global),
            "wl_shm" => self.shm = Some(global),
            "xdg_wm_base" => self.xdg_wm_base = Some(global),
            "wl_seat" => self.seat = Some(global),
            "td_portal_manager_v1" => self.portal_manager = Some(global),
            _ => {}
        }
        self.ordered.push((name, interface, version));
        Ok(())
    }

    fn finish(&self) -> Result<(), String> {
        if self.ordered.len() != EXPECTED_GLOBALS.len() {
            return Err(format!(
                "private portal registry advertised {} globals, expected {}",
                self.ordered.len(),
                EXPECTED_GLOBALS.len()
            ));
        }
        for ((_, actual_interface, actual_version), (interface, version)) in
            self.ordered.iter().zip(EXPECTED_GLOBALS)
        {
            if actual_interface != interface || *actual_version != version {
                return Err(format!(
                    "private portal registry advertised {actual_interface} v{actual_version}, expected {interface} v{version}"
                ));
            }
        }
        Ok(())
    }

    fn require(global: Option<Global>, interface: &str, version: u32) -> Result<Global, String> {
        let global = global.ok_or_else(|| format!("private compositor omitted {interface}"))?;
        if global.version < version {
            return Err(format!(
                "private compositor advertised {interface} v{}, need v{version}",
                global.version
            ));
        }
        Ok(global)
    }
}

struct Connection {
    stream: UnixStream,
    buffered: Vec<u8>,
    pending_fds: VecDeque<RawFd>,
    incoming: [u8; RECEIVE_BUFFER_BYTES],
    deadline: Option<Instant>,
    last_write: Instant,
    next_id: u32,
    free_ids: BTreeSet<u32>,
    keepalive_callbacks: BTreeSet<u32>,
    retired_keepalive: Option<u32>,
}

impl Drop for Connection {
    fn drop(&mut self) {
        while let Some(fd) = self.pending_fds.pop_front() {
            sys::discard_received(&[fd]);
        }
    }
}

impl Connection {
    fn connect(path: &Path, connector: Arc<AtomicBool>) -> Result<Self, String> {
        let deadline = Instant::now()
            .checked_add(HANDSHAKE_TIMEOUT)
            .ok_or_else(|| "private Wayland handshake deadline overflowed".to_string())?;
        let path = path.to_path_buf();
        let stream = bounded_connect(connector, deadline, move || {
            connect_blocking_until(&path, deadline)
        })?;
        Ok(Self {
            stream,
            buffered: Vec::with_capacity(RECEIVE_BUFFER_BYTES),
            pending_fds: VecDeque::new(),
            incoming: [0; RECEIVE_BUFFER_BYTES],
            deadline: Some(deadline),
            last_write: Instant::now(),
            next_id: FIRST_DYNAMIC_ID,
            free_ids: BTreeSet::new(),
            keepalive_callbacks: BTreeSet::new(),
            retired_keepalive: None,
        })
    }

    fn canceller(&self) -> Result<UnixStream, String> {
        self.stream
            .try_clone()
            .map_err(|error| format!("clone private portal Wayland cancellation handle: {error}"))
    }

    fn finish_handshake(&mut self) -> Result<(), String> {
        self.stream
            .set_write_timeout(Some(IDLE_PULSE))
            .map_err(|error| format!("set private Wayland write bound: {error}"))?;
        self.deadline = None;
        Ok(())
    }

    fn begin_exchange(&mut self) -> Result<(), String> {
        let deadline = Instant::now()
            .checked_add(HANDSHAKE_TIMEOUT)
            .ok_or_else(|| "private Wayland exchange deadline overflowed".to_string())?;
        self.deadline = Some(deadline);
        self.stream
            .set_read_timeout(Some(remaining(deadline)?))
            .map_err(|error| format!("set private Wayland exchange read timeout: {error}"))?;
        self.stream
            .set_write_timeout(Some(remaining(deadline)?))
            .map_err(|error| format!("set private Wayland exchange write timeout: {error}"))
    }

    fn pulse(&mut self) -> Result<(), String> {
        if !self.keepalive_callbacks.is_empty() || self.retired_keepalive.is_some() {
            return Err("private compositor did not retire its idle pulse".into());
        }
        let callback = self.allocate_id()?;
        if !self.keepalive_callbacks.insert(callback) {
            return Err("private Wayland idle callback collided".into());
        }
        let mut sync = wire::Builder::new();
        sync.u32(callback);
        self.send(DISPLAY, 0, sync)
    }

    fn send(&mut self, object: u32, opcode: u16, payload: wire::Builder) -> Result<(), String> {
        let bytes = payload.message(object, opcode)?;
        self.write_timeout()?;
        self.stream
            .write_all(&bytes)
            .map_err(|error| format!("send private Wayland request: {error}"))?;
        self.last_write = Instant::now();
        Ok(())
    }

    fn send_with_fd(
        &mut self,
        object: u32,
        opcode: u16,
        payload: wire::Builder,
        file: &File,
    ) -> Result<(), String> {
        let bytes = payload.message(object, opcode)?;
        self.write_timeout()?;
        sys::send_with_fd(&self.stream, &bytes, file.as_raw_fd())
            .map_err(|error| format!("send private Wayland descriptor: {error}"))?;
        self.last_write = Instant::now();
        Ok(())
    }

    fn write_timeout(&self) -> Result<(), String> {
        if let Some(deadline) = self.deadline {
            self.stream
                .set_write_timeout(Some(remaining(deadline)?))
                .map_err(|error| format!("set private Wayland write timeout: {error}"))?;
        }
        Ok(())
    }

    fn next(&mut self) -> Result<wire::Message, String> {
        loop {
            if let Some(message) = wire::take(&mut self.buffered)? {
                return Ok(message);
            }
            if self.buffered.len() >= MAX_BUFFERED_BYTES {
                return Err(format!(
                    "private Wayland message exceeds {MAX_BUFFERED_BYTES} bytes"
                ));
            }
            let wanted = match wire::header(&self.buffered)? {
                Some((_, _, size)) => size.saturating_sub(self.buffered.len()),
                None => wire::HEADER_SIZE.saturating_sub(self.buffered.len()),
            };
            if wanted == 0 {
                return Err("private Wayland parser made no progress".into());
            }
            let timeout = if let Some(deadline) = self.deadline {
                remaining(deadline)?
            } else {
                let idle = IDLE_PULSE.saturating_sub(self.last_write.elapsed());
                if idle.is_zero() {
                    self.pulse()?;
                    continue;
                }
                idle
            };
            self.stream
                .set_read_timeout(Some(timeout))
                .map_err(|error| format!("set private Wayland read timeout: {error}"))?;
            let capacity = wanted.min(self.incoming.len());
            let input = self
                .incoming
                .get_mut(..capacity)
                .ok_or_else(|| "private Wayland receive bound escaped".to_string())?;
            let received = match sys::recv_with_fds(&self.stream, input) {
                Ok(received) => received,
                Err(sys::ReceiveError::Disconnected) => {
                    return Err("private Wayland compositor disconnected".into());
                }
                Err(sys::ReceiveError::TimedOut) if self.deadline.is_none() => {
                    self.pulse()?;
                    continue;
                }
                Err(error) => return Err(format!("receive private Wayland event: {error}")),
            };
            if received.count == 0 {
                sys::discard_received(&received.fds);
                return Err("private Wayland compositor disconnected".into());
            }
            if self.pending_fds.len().saturating_add(received.fds.len()) > MAX_PENDING_FDS {
                sys::discard_received(&received.fds);
                while let Some(fd) = self.pending_fds.pop_front() {
                    sys::discard_received(&[fd]);
                }
                return Err(format!(
                    "private Wayland connection queued more than {MAX_PENDING_FDS} descriptors"
                ));
            }
            let bytes = input
                .get(..received.count)
                .ok_or_else(|| "private Wayland receive count escaped".to_string())?;
            self.buffered.extend_from_slice(bytes);
            self.pending_fds.extend(received.fds);
        }
    }

    fn handle_common(&mut self, message: &wire::Message) -> Result<bool, String> {
        if message.opcode == 0 && self.keepalive_callbacks.remove(&message.object) {
            let mut args = wire::Cursor::new(&message.payload);
            args.u32()?;
            args.finish()?;
            if self.retired_keepalive.replace(message.object).is_some() {
                return Err("private compositor overlapped idle callbacks".into());
            }
            return Ok(true);
        }
        if message.object == DISPLAY && message.opcode == 0 {
            let mut args = wire::Cursor::new(&message.payload);
            let object = args.u32()?;
            let code = args.u32()?;
            let text = args.string()?;
            args.finish()?;
            return Err(format!(
                "private Wayland protocol error on object {object}, code {code}: {text}"
            ));
        }
        if message.object == DISPLAY && message.opcode == 1 {
            let mut args = wire::Cursor::new(&message.payload);
            let id = args.u32()?;
            args.finish()?;
            if self.keepalive_callbacks.contains(&id) {
                return Err(format!(
                    "private compositor deleted live idle callback {id}"
                ));
            }
            if self.retired_keepalive == Some(id) {
                self.retired_keepalive = None;
            }
            if id >= FIRST_DYNAMIC_ID && (id >= self.next_id || !self.free_ids.insert(id)) {
                return Err(format!("private compositor deleted invalid object {id}"));
            }
            return Ok(true);
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

    fn allocate_id(&mut self) -> Result<u32, String> {
        if let Some(id) = self.free_ids.pop_first() {
            return Ok(id);
        }
        let id = self.next_id;
        self.next_id = self
            .next_id
            .checked_add(1)
            .filter(|next| *next != 0)
            .ok_or_else(|| "private Wayland object id space exhausted".to_string())?;
        Ok(id)
    }

    fn take_file(&mut self, purpose: &str) -> Result<File, String> {
        let fd = self
            .pending_fds
            .pop_front()
            .ok_or_else(|| format!("{purpose} event arrived without a descriptor"))?;
        sys::duplicate_received(fd)
    }
}

fn bounded_connect<F>(
    connector: Arc<AtomicBool>,
    deadline: Instant,
    operation: F,
) -> Result<UnixStream, String>
where
    F: FnOnce() -> Result<UnixStream, String> + Send + 'static,
{
    let lease = ConnectLease::acquire(connector)?;
    let (sender, receiver) = mpsc::sync_channel(1);
    thread::Builder::new()
        .name("td-portal-wayland-connect".into())
        .spawn(move || {
            let result = operation();
            drop(lease);
            let _ = sender.send(result);
        })
        .map_err(|error| format!("spawn the private Wayland connector: {error}"))?;
    let wait = remaining(deadline)?;
    receiver.recv_timeout(wait).map_err(|error| match error {
        mpsc::RecvTimeoutError::Timeout => {
            "private portal Wayland connect exceeded its 20-second deadline".to_string()
        }
        mpsc::RecvTimeoutError::Disconnected => {
            "private portal Wayland connector exited without a result".to_string()
        }
    })?
}

fn connect_blocking_until(path: &Path, deadline: Instant) -> Result<UnixStream, String> {
    let mut last = None;
    for attempt in 0..CONNECT_ATTEMPTS {
        match UnixStream::connect(path) {
            Ok(stream) => return Ok(stream),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                ) =>
            {
                last = Some(error);
                if attempt + 1 < CONNECT_ATTEMPTS {
                    let Some(wait) = deadline
                        .checked_duration_since(Instant::now())
                        .filter(|wait| !wait.is_zero())
                    else {
                        break;
                    };
                    thread::sleep(wait.min(Duration::from_millis(100)));
                }
            }
            Err(error) => {
                return Err(format!(
                    "connect private portal Wayland socket {}: {error}",
                    path.display()
                ));
            }
        }
    }
    Err(format!(
        "connect private portal Wayland socket {} after {CONNECT_ATTEMPTS} attempts: {}",
        path.display(),
        last.map_or_else(|| "unknown error".to_string(), |error| error.to_string())
    ))
}

#[derive(Debug)]
struct Frame {
    buffer: u32,
    callback: u32,
    released: bool,
    presented: bool,
    width: usize,
    height: usize,
    checksum: u64,
}

struct Dialog {
    chooser: Chooser,
    window_title: String,
    parent_handle: String,
    runtime_directory: PathBuf,
    globals: Globals,
    xrgb: bool,
    keyboard_requested: bool,
    keymap_verified: bool,
    keyboard_focused: bool,
    modifiers: u32,
    group: u32,
    proposed: Option<(usize, usize)>,
    current: Option<(usize, usize)>,
    dirty: bool,
    frame: Option<Frame>,
    portal_state: Option<u32>,
    presented_once: bool,
    dismissing: bool,
}

impl Dialog {
    fn new(config: DialogConfig) -> Result<Self, String> {
        let window_title = if config.title.is_empty() {
            format!("{} — Open file", config.app_id)
        } else {
            format!("{} — {}", config.app_id, config.title)
        };
        let chooser = Chooser::open_with_options(
            &window_title,
            &config.host_root,
            &config.guest_root,
            config.mode,
            config.accept_label,
            config.filter,
        )?;
        Ok(Self {
            chooser,
            window_title,
            parent_handle: config.parent_handle,
            runtime_directory: config.runtime_directory,
            globals: Globals::new(),
            xrgb: false,
            keyboard_requested: false,
            keymap_verified: false,
            keyboard_focused: false,
            modifiers: 0,
            group: 0,
            proposed: None,
            current: None,
            dirty: false,
            frame: None,
            portal_state: None,
            presented_once: false,
            dismissing: false,
        })
    }

    fn discover(&mut self, connection: &mut Connection) -> Result<(), String> {
        let mut registry = wire::Builder::new();
        registry.u32(REGISTRY);
        connection.send(DISPLAY, 1, registry)?;
        let mut sync = wire::Builder::new();
        sync.u32(SYNC_CALLBACK);
        connection.send(DISPLAY, 0, sync)?;
        loop {
            let message = connection.next()?;
            if connection.handle_common(&message)? {
                continue;
            }
            match (message.object, message.opcode) {
                (REGISTRY, 0) => {
                    let mut args = wire::Cursor::new(&message.payload);
                    let name = args.u32()?;
                    let interface = args.string()?;
                    let version = args.u32()?;
                    args.finish()?;
                    self.globals.record(name, interface, version)?;
                }
                (SYNC_CALLBACK, 0) => {
                    let mut args = wire::Cursor::new(&message.payload);
                    args.u32()?;
                    args.finish()?;
                    self.globals.finish()?;
                    return Ok(());
                }
                _ => {
                    return Err(format!(
                        "unexpected private registry event object={} opcode={}",
                        message.object, message.opcode
                    ));
                }
            }
        }
    }

    fn bind_and_create(&mut self, connection: &mut Connection) -> Result<(), String> {
        let compositor = Globals::require(self.globals.compositor, "wl_compositor", 4)?;
        let shm = Globals::require(self.globals.shm, "wl_shm", 1)?;
        let wm = Globals::require(self.globals.xdg_wm_base, "xdg_wm_base", 1)?;
        let seat = Globals::require(self.globals.seat, "wl_seat", 1)?;
        let manager = Globals::require(self.globals.portal_manager, "td_portal_manager_v1", 1)?;
        for (global, interface, version, object) in [
            (compositor, "wl_compositor", 4, COMPOSITOR),
            (shm, "wl_shm", 1, SHM),
            (wm, "xdg_wm_base", 1, XDG_WM_BASE),
            (seat, "wl_seat", 7, SEAT),
            (manager, "td_portal_manager_v1", 1, PORTAL_MANAGER),
        ] {
            bind(connection, global.name, interface, version, object)?;
        }

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
        title.string(&self.window_title)?;
        connection.send(XDG_TOPLEVEL, 2, title)?;
        let mut dialog = wire::Builder::new();
        dialog.u32(SURFACE);
        dialog.string(&self.parent_handle)?;
        dialog.u32(0);
        connection.send(PORTAL_MANAGER, 0, dialog)?;
        connection.send(SURFACE, 6, wire::Builder::new())
    }

    fn dispatch(
        &mut self,
        connection: &mut Connection,
        message: &wire::Message,
        notice: &impl Fn(Notice) -> Result<(), String>,
    ) -> Result<Option<Outcome>, String> {
        if connection.handle_common(message)? {
            return Ok(None);
        }
        match (message.object, message.opcode) {
            (REGISTRY, 0 | 1) => self.registry_update(message)?,
            (SHM, 0) => {
                let mut args = wire::Cursor::new(&message.payload);
                if args.u32()? == SHM_XRGB8888 {
                    self.xrgb = true;
                }
                args.finish()?;
            }
            (XDG_TOPLEVEL, 0) => self.toplevel_configure(message)?,
            (XDG_TOPLEVEL, 1) => return Ok(Some(Outcome::Cancelled)),
            (XDG_SURFACE, 0) => self.surface_configure(connection, message)?,
            (SEAT, 0 | 1) => self.seat_event(connection, message)?,
            (KEYBOARD, 0..=5) => {
                if let Some(outcome) = self.keyboard_event(connection, message)? {
                    return Ok(Some(outcome));
                }
            }
            (PORTAL_MANAGER, 0) => self.portal_event(message)?,
            (object, 0)
                if self
                    .frame
                    .as_ref()
                    .is_some_and(|frame| frame.callback == object) =>
            {
                let mut args = wire::Cursor::new(&message.payload);
                args.u32()?;
                args.finish()?;
                let Some(frame) = self.frame.as_mut() else {
                    return Err("private frame callback lost its state".into());
                };
                frame.presented = true;
            }
            (object, 0)
                if self
                    .frame
                    .as_ref()
                    .is_some_and(|frame| frame.buffer == object) =>
            {
                let args = wire::Cursor::new(&message.payload);
                args.finish()?;
                connection.send(object, 0, wire::Builder::new())?;
                let Some(frame) = self.frame.as_mut() else {
                    return Err("private buffer release lost its state".into());
                };
                frame.released = true;
            }
            _ => {
                return Err(format!(
                    "unexpected private dialog event object={} opcode={}",
                    message.object, message.opcode
                ));
            }
        }
        self.settle(connection, notice)?;
        Ok(None)
    }

    fn registry_update(&self, message: &wire::Message) -> Result<(), String> {
        let mut args = wire::Cursor::new(&message.payload);
        let name = args.u32()?;
        if message.opcode == 0 {
            args.string()?;
            args.u32()?;
        }
        args.finish()?;
        if message.opcode == 1
            && self
                .globals
                .ordered
                .iter()
                .any(|(bound, _, _)| *bound == name)
        {
            return Err(format!(
                "private compositor withdrew bound global name {name}"
            ));
        }
        Ok(())
    }

    fn toplevel_configure(&mut self, message: &wire::Message) -> Result<(), String> {
        let mut args = wire::Cursor::new(&message.payload);
        let width = args.i32()?;
        let height = args.i32()?;
        let states = usize::try_from(args.u32()?)
            .map_err(|_| "private toplevel state length escaped usize".to_string())?;
        if !states.is_multiple_of(4) || states > 64 {
            return Err(format!("private toplevel state array has {states} bytes"));
        }
        for _ in 0..states / 4 {
            args.u32()?;
        }
        args.finish()?;
        if width < 0 || height < 0 {
            return Err(format!(
                "private compositor proposed invalid dialog size {width}x{height}"
            ));
        }
        let fallback = self
            .current
            .unwrap_or((file_chooser::WIDTH, file_chooser::HEIGHT));
        let width = if width == 0 {
            fallback.0
        } else {
            usize::try_from(width).map_err(|_| "private dialog width escaped usize".to_string())?
        };
        let height = if height == 0 {
            fallback.1
        } else {
            usize::try_from(height)
                .map_err(|_| "private dialog height escaped usize".to_string())?
        };
        self.chooser.set_viewport(width, height)?;
        self.proposed = Some((width, height));
        Ok(())
    }

    fn surface_configure(
        &mut self,
        connection: &mut Connection,
        message: &wire::Message,
    ) -> Result<(), String> {
        let mut args = wire::Cursor::new(&message.payload);
        let serial = args.u32()?;
        args.finish()?;
        let mut ack = wire::Builder::new();
        ack.u32(serial);
        connection.send(XDG_SURFACE, 4, ack)?;
        self.current = Some(
            self.proposed
                .take()
                .or(self.current)
                .ok_or_else(|| "private surface configure had no dialog size".to_string())?,
        );
        self.dirty = true;
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
                if capabilities & SEAT_KEYBOARD != 0 && !self.keyboard_requested {
                    let mut keyboard = wire::Builder::new();
                    keyboard.u32(KEYBOARD);
                    connection.send(SEAT, 1, keyboard)?;
                    self.keyboard_requested = true;
                }
            }
            1 => {
                args.string()?;
                args.finish()?;
            }
            _ => return Err("private seat sent an unsupported event".into()),
        }
        Ok(())
    }

    fn keyboard_event(
        &mut self,
        connection: &mut Connection,
        message: &wire::Message,
    ) -> Result<Option<Outcome>, String> {
        let mut args = wire::Cursor::new(&message.payload);
        match message.opcode {
            0 => {
                let format = args.u32()?;
                let size = args.u32()?;
                args.finish()?;
                let file = connection.take_file("private wl_keyboard.keymap")?;
                verify_keymap(&file, format, size)?;
                self.keymap_verified = true;
            }
            1 => {
                args.u32()?;
                let surface = args.u32()?;
                let bytes = usize::try_from(args.u32()?)
                    .map_err(|_| "private held-key array escaped usize".to_string())?;
                if surface != SURFACE || !bytes.is_multiple_of(4) || bytes / 4 > MAX_HELD_KEYS {
                    return Err(format!(
                        "private keyboard entered surface {surface} with {bytes} key bytes"
                    ));
                }
                for _ in 0..bytes / 4 {
                    args.u32()?;
                }
                args.finish()?;
                if !self.keymap_verified {
                    return Err("private keyboard entered before its keymap".into());
                }
                self.keyboard_focused = true;
            }
            2 => {
                args.u32()?;
                let surface = args.u32()?;
                args.finish()?;
                if surface != SURFACE {
                    return Err(format!("private keyboard left surface {surface}"));
                }
                self.keyboard_focused = false;
                self.modifiers = 0;
                self.group = 0;
            }
            3 => {
                args.u32()?;
                args.u32()?;
                let key = args.u32()?;
                let state = args.u32()?;
                args.finish()?;
                if !matches!(state, KEY_RELEASED | KEY_PRESSED) {
                    return Err(format!("private keyboard sent invalid key state {state}"));
                }
                if state == KEY_PRESSED {
                    if !self.keymap_verified || !self.keyboard_focused {
                        return Err(
                            "private keyboard sent input before its keymap and enter".into()
                        );
                    }
                    if let Some(action) = key_action(key, self.modifiers, self.group) {
                        let outcome = self.chooser.apply(action)?;
                        if outcome != Outcome::Pending {
                            return Ok(Some(outcome));
                        }
                        self.dirty = true;
                    }
                }
            }
            4 => {
                args.u32()?;
                let depressed = args.u32()?;
                let latched = args.u32()?;
                let locked = args.u32()?;
                self.group = args.u32()?;
                args.finish()?;
                self.modifiers = depressed | latched | locked;
            }
            5 => {
                args.i32()?;
                args.i32()?;
                args.finish()?;
            }
            _ => return Err("private keyboard sent an unsupported event".into()),
        }
        Ok(None)
    }

    fn portal_event(&mut self, message: &wire::Message) -> Result<(), String> {
        let mut args = wire::Cursor::new(&message.payload);
        let surface = args.u32()?;
        let state = args.u32()?;
        args.finish()?;
        if surface != SURFACE || state > PORTAL_DIALOG_DISMISSED {
            return Err(format!(
                "private portal manager answered surface {surface} with state {state}"
            ));
        }
        if state == PORTAL_DIALOG_DISMISSED && !self.dismissing {
            return Err("private portal dialog was dismissed without a request".into());
        }
        self.portal_state = Some(state);
        Ok(())
    }

    fn settle(
        &mut self,
        connection: &mut Connection,
        notice: &impl Fn(Notice) -> Result<(), String>,
    ) -> Result<(), String> {
        if let Some(frame) = self.frame.as_ref() {
            if !(frame.released && frame.presented) {
                return Ok(());
            }
            if !self.presented_once {
                if !matches!(self.portal_state, Some(0 | 1)) || !self.keyboard_focused {
                    return Ok(());
                }
                notice(Notice::Presented {
                    width: frame.width,
                    height: frame.height,
                    checksum: frame.checksum,
                })?;
                self.presented_once = true;
                if !self.dismissing {
                    connection.finish_handshake()?;
                }
            }
            self.frame = None;
        }
        if !self.dirty || self.dismissing {
            return Ok(());
        }
        if !self.xrgb {
            return Ok(());
        }
        let (width, height) = self
            .current
            .ok_or_else(|| "private dialog has no configured size".to_string())?;
        let pixels = self.chooser.render_sized(width, height)?;
        let checksum = pixel_checksum(&pixels);
        let (buffer, callback) =
            attach_frame(connection, &self.runtime_directory, &pixels, width, height)?;
        self.frame = Some(Frame {
            buffer,
            callback,
            released: false,
            presented: false,
            width,
            height,
            checksum,
        });
        self.dirty = false;
        Ok(())
    }

    fn dismiss(&mut self, connection: &mut Connection) -> Result<(), String> {
        self.dismissing = true;
        connection.begin_exchange()?;
        let mut dismiss = wire::Builder::new();
        dismiss.u32(SURFACE);
        connection.send(PORTAL_MANAGER, 1, dismiss)
    }
}

pub fn spawn<F>(config: DialogConfig, notice: F) -> Result<(), String>
where
    F: Fn(Notice) -> Result<(), String> + Send + 'static,
{
    thread::Builder::new()
        .name("td-portal-file-chooser".into())
        .spawn(move || {
            let result = run(config, &notice);
            let _ = notice(Notice::Completed(result));
        })
        .map(|_| ())
        .map_err(|error| format!("spawn FileChooser dialog worker: {error}"))
}

fn run(
    config: DialogConfig,
    notice: &impl Fn(Notice) -> Result<(), String>,
) -> Result<Outcome, String> {
    let mut connection = Connection::connect(&config.socket, config.connector.clone())?;
    notice(Notice::Connected(connection.canceller()?))?;
    let mut dialog = Dialog::new(config)?;
    dialog.discover(&mut connection)?;
    dialog.bind_and_create(&mut connection)?;
    loop {
        let message = connection.next()?;
        if let Some(outcome) = dialog.dispatch(&mut connection, &message, notice)? {
            dialog.dismiss(&mut connection)?;
            loop {
                let message = connection.next()?;
                dialog.dispatch(&mut connection, &message, notice)?;
                if dialog.portal_state == Some(PORTAL_DIALOG_DISMISSED) {
                    return Ok(outcome);
                }
            }
        }
    }
}

fn remaining(deadline: Instant) -> Result<Duration, String> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|left| !left.is_zero())
        .ok_or_else(|| "private Wayland handshake timed out".to_string())
}

fn bind(
    connection: &mut Connection,
    name: u32,
    interface: &str,
    version: u32,
    object: u32,
) -> Result<(), String> {
    let mut request = wire::Builder::new();
    request.u32(name);
    request.string(interface)?;
    request.u32(version);
    request.u32(object);
    connection.send(REGISTRY, 0, request)
}

fn key_action(key: u32, modifiers: u32, group: u32) -> Option<Action> {
    if group != 0 || modifiers & (MOD_ALT | MOD_LOGO) != 0 {
        return None;
    }
    if matches!(key, 28 | 96) && modifiers & MOD_CONTROL != 0 {
        return Some(Action::Accept);
    }
    if modifiers & MOD_CONTROL != 0 {
        return None;
    }
    match key {
        1 => Some(Action::Cancel),
        14 => Some(Action::Backspace),
        28 | 96 | 106 => Some(Action::Activate),
        57 => Some(Action::Toggle),
        103 => Some(Action::Previous),
        105 => Some(Action::Parent),
        108 => Some(Action::Next),
        _ => key_character(key, modifiers).map(Action::Insert),
    }
}

fn key_character(key: u32, modifiers: u32) -> Option<char> {
    let letter = match key {
        16 => 'q',
        17 => 'w',
        18 => 'e',
        19 => 'r',
        20 => 't',
        21 => 'y',
        22 => 'u',
        23 => 'i',
        24 => 'o',
        25 => 'p',
        30 => 'a',
        31 => 's',
        32 => 'd',
        33 => 'f',
        34 => 'g',
        35 => 'h',
        36 => 'j',
        37 => 'k',
        38 => 'l',
        44 => 'z',
        45 => 'x',
        46 => 'c',
        47 => 'v',
        48 => 'b',
        49 => 'n',
        50 => 'm',
        _ => return None,
    };
    let uppercase = (modifiers & MOD_SHIFT != 0) ^ (modifiers & MOD_CAPS != 0);
    Some(if uppercase {
        letter.to_ascii_uppercase()
    } else {
        letter
    })
}

fn verify_keymap(file: &File, format: u32, size: u32) -> Result<(), String> {
    if format != 1 {
        return Err(format!(
            "unsupported private wl_keyboard keymap format {format}"
        ));
    }
    let expected = XKB_KEYMAP
        .len()
        .checked_add(1)
        .ok_or_else(|| "private keymap size overflow".to_string())?;
    if usize::try_from(size).ok() != Some(expected)
        || usize::try_from(
            file.metadata()
                .map_err(|error| format!("stat keymap: {error}"))?
                .len(),
        )
        .ok()
            != Some(expected)
    {
        return Err(format!(
            "private wl_keyboard keymap size differs from {expected}"
        ));
    }
    let mut bytes = vec![0u8; expected];
    let mut filled = 0usize;
    while filled < expected {
        let rest = bytes
            .get_mut(filled..)
            .ok_or_else(|| "private keymap read escaped its buffer".to_string())?;
        let offset =
            u64::try_from(filled).map_err(|_| "private keymap offset escaped u64".to_string())?;
        match file.read_at(rest, offset) {
            Ok(0) => return Err("private wl_keyboard keymap was truncated".into()),
            Ok(count) => filled = filled.saturating_add(count),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(format!("read private wl_keyboard keymap: {error}")),
        }
    }
    if bytes.get(..XKB_KEYMAP.len()) != Some(XKB_KEYMAP.as_bytes())
        || bytes.last().copied() != Some(0)
    {
        return Err("private wl_keyboard keymap differs from td's pinned keymap".into());
    }
    Ok(())
}

fn backing_file(directory: &Path, pixels: &[u8]) -> Result<File, String> {
    for attempt in 0..64u32 {
        let path = directory.join(format!(
            "td-file-chooser-{}-{attempt}.shm",
            std::process::id()
        ));
        let mut file = match OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(format!("create chooser shm {}: {error}", path.display())),
        };
        let write = file
            .write_all(pixels)
            .map_err(|error| format!("write chooser shm {}: {error}", path.display()));
        let remove = fs::remove_file(&path)
            .map_err(|error| format!("unlink chooser shm {}: {error}", path.display()));
        write?;
        remove?;
        return Ok(file);
    }
    Err("file chooser exhausted its 64 shm names".into())
}

fn attach_frame(
    connection: &mut Connection,
    directory: &Path,
    pixels: &[u8],
    width: usize,
    height: usize,
) -> Result<(u32, u32), String> {
    let file = backing_file(directory, pixels)?;
    let pool = connection.allocate_id()?;
    let buffer = connection.allocate_id()?;
    let callback = connection.allocate_id()?;
    let bytes = i32::try_from(pixels.len()).map_err(|_| "chooser shm exceeds i32".to_string())?;
    let width_i32 = i32::try_from(width).map_err(|_| "chooser width exceeds i32".to_string())?;
    let height_i32 = i32::try_from(height).map_err(|_| "chooser height exceeds i32".to_string())?;
    let stride = width
        .checked_mul(file_chooser::BYTES_PER_PIXEL)
        .and_then(|value| i32::try_from(value).ok())
        .ok_or_else(|| "chooser stride exceeds i32".to_string())?;
    let mut create_pool = wire::Builder::new();
    create_pool.u32(pool);
    create_pool.i32(bytes);
    connection.send_with_fd(SHM, 0, create_pool, &file)?;
    let mut create_buffer = wire::Builder::new();
    create_buffer.u32(buffer);
    create_buffer.i32(0);
    create_buffer.i32(width_i32);
    create_buffer.i32(height_i32);
    create_buffer.i32(stride);
    create_buffer.u32(SHM_XRGB8888);
    connection.send(pool, 0, create_buffer)?;
    connection.send(pool, 1, wire::Builder::new())?;
    let mut attach = wire::Builder::new();
    attach.u32(buffer);
    attach.i32(0);
    attach.i32(0);
    connection.send(SURFACE, 1, attach)?;
    let mut damage = wire::Builder::new();
    damage.i32(0);
    damage.i32(0);
    damage.i32(width_i32);
    damage.i32(height_i32);
    connection.send(SURFACE, 9, damage)?;
    let mut frame = wire::Builder::new();
    frame.u32(callback);
    connection.send(SURFACE, 3, frame)?;
    connection.send(SURFACE, 6, wire::Builder::new())?;
    Ok((buffer, callback))
}

fn pixel_checksum(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x100_0000_01b3)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::io::Read;
    use std::os::unix::net::UnixListener;
    use std::sync::mpsc;

    struct Temp(PathBuf);

    impl Temp {
        fn new(name: &str) -> Self {
            for attempt in 0..32u8 {
                let path = std::env::temp_dir().join(format!(
                    "td-portal-wayland-{name}-{}-{attempt}",
                    std::process::id()
                ));
                match fs::create_dir(&path) {
                    Ok(()) => return Self(path),
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(error) => panic!("create fake Wayland directory: {error}"),
                }
            }
            panic!("exhausted fake Wayland directory names");
        }
    }

    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    struct Peer {
        stream: UnixStream,
        buffered: Vec<u8>,
        fds: VecDeque<RawFd>,
        incoming: [u8; RECEIVE_BUFFER_BYTES],
    }

    impl Drop for Peer {
        fn drop(&mut self) {
            while let Some(fd) = self.fds.pop_front() {
                sys::discard_received(&[fd]);
            }
        }
    }

    impl Peer {
        fn new(stream: UnixStream) -> Self {
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            Self {
                stream,
                buffered: Vec::new(),
                fds: VecDeque::new(),
                incoming: [0; RECEIVE_BUFFER_BYTES],
            }
        }

        fn next(&mut self) -> wire::Message {
            loop {
                if let Some(message) = wire::take(&mut self.buffered).unwrap() {
                    return message;
                }
                let received = sys::recv_with_fds(&self.stream, &mut self.incoming).unwrap();
                assert!(received.count > 0);
                self.buffered
                    .extend_from_slice(&self.incoming[..received.count]);
                self.fds.extend(received.fds);
            }
        }

        fn expect(&mut self, object: u32, opcode: u16) -> wire::Message {
            let message = self.next();
            assert_eq!((message.object, message.opcode), (object, opcode));
            message
        }

        fn send(&mut self, object: u32, opcode: u16, payload: wire::Builder) {
            self.stream
                .write_all(&payload.message(object, opcode).unwrap())
                .unwrap();
        }

        fn send_fd(&mut self, object: u32, opcode: u16, payload: wire::Builder, file: &File) {
            let message = payload.message(object, opcode).unwrap();
            sys::send_with_fd(&self.stream, &message, file.as_raw_fd()).unwrap();
        }

        fn expect_frame(&mut self, width: usize, height: usize) -> (u32, u32, u64) {
            let create_pool = self.expect(SHM, 0);
            let mut args = wire::Cursor::new(&create_pool.payload);
            let pool = args.u32().unwrap();
            let byte_count = usize::try_from(args.i32().unwrap()).unwrap();
            args.finish().unwrap();
            assert_eq!(byte_count, width * height * file_chooser::BYTES_PER_PIXEL);
            let descriptor = self.fds.pop_front().expect("wl_shm descriptor");
            let mut pixels = sys::duplicate_received(descriptor).unwrap();
            let mut pixel_bytes = Vec::new();
            pixels.read_to_end(&mut pixel_bytes).unwrap();
            assert_eq!(pixel_bytes.len(), byte_count);
            let checksum = pixel_checksum(&pixel_bytes);

            let create_buffer = self.expect(pool, 0);
            let mut args = wire::Cursor::new(&create_buffer.payload);
            let buffer = args.u32().unwrap();
            assert_eq!(args.i32().unwrap(), 0);
            assert_eq!(args.i32().unwrap(), i32::try_from(width).unwrap());
            assert_eq!(args.i32().unwrap(), i32::try_from(height).unwrap());
            assert_eq!(args.i32().unwrap(), i32::try_from(width * 4).unwrap());
            assert_eq!(args.u32().unwrap(), SHM_XRGB8888);
            args.finish().unwrap();
            self.expect(pool, 1);
            let attach = self.expect(SURFACE, 1);
            let mut args = wire::Cursor::new(&attach.payload);
            assert_eq!(args.u32().unwrap(), buffer);
            assert_eq!(args.i32().unwrap(), 0);
            assert_eq!(args.i32().unwrap(), 0);
            args.finish().unwrap();
            self.expect(SURFACE, 9);
            let frame = self.expect(SURFACE, 3);
            let mut args = wire::Cursor::new(&frame.payload);
            let callback = args.u32().unwrap();
            args.finish().unwrap();
            self.expect(SURFACE, 6);
            assert!(self.fds.is_empty());
            (buffer, callback, checksum)
        }
    }

    #[test]
    fn physical_keys_map_to_closed_chooser_actions() {
        assert_eq!(key_action(108, 0, 0), Some(Action::Next));
        assert_eq!(key_action(103, 0, 0), Some(Action::Previous));
        assert_eq!(key_action(106, 0, 0), Some(Action::Activate));
        assert_eq!(key_action(28, 0, 0), Some(Action::Activate));
        assert_eq!(key_action(28, MOD_CONTROL, 0), Some(Action::Accept));
        assert_eq!(key_action(96, MOD_CONTROL, 0), Some(Action::Accept));
        assert_eq!(key_action(19, 0, 0), Some(Action::Insert('r')));
        assert_eq!(key_action(19, MOD_SHIFT, 0), Some(Action::Insert('R')));
        assert_eq!(key_action(19, MOD_ALT, 0), None);
        assert_eq!(key_action(19, 0, 1), None);
    }

    #[test]
    fn registry_is_exact_and_ordered() {
        let mut globals = Globals::new();
        for (index, (interface, version)) in EXPECTED_GLOBALS.iter().enumerate() {
            globals
                .record(
                    u32::try_from(index + 1).unwrap(),
                    (*interface).into(),
                    *version,
                )
                .unwrap();
        }
        globals.finish().unwrap();
        assert!(globals.record(99, "extra".into(), 1).is_err());
        assert_eq!(globals.ordered.len(), EXPECTED_GLOBALS.len());
        globals.ordered.swap(0, 1);
        assert!(globals.finish().is_err());
    }

    #[test]
    fn pixel_checksum_is_stable_and_order_sensitive() {
        assert_eq!(pixel_checksum(b"pixels"), pixel_checksum(b"pixels"));
        assert_ne!(pixel_checksum(b"pixels"), pixel_checksum(b"pixelS"));
    }

    #[test]
    fn idle_pulse_is_one_sync_until_done_and_delete_id() {
        let (client, server) = UnixStream::pair().unwrap();
        let mut connection = Connection {
            stream: client,
            buffered: Vec::new(),
            pending_fds: VecDeque::new(),
            incoming: [0; RECEIVE_BUFFER_BYTES],
            deadline: None,
            last_write: Instant::now(),
            next_id: FIRST_DYNAMIC_ID,
            free_ids: BTreeSet::new(),
            keepalive_callbacks: BTreeSet::new(),
            retired_keepalive: None,
        };
        let mut peer = Peer::new(server);
        connection.pulse().unwrap();
        assert!(connection.pulse().is_err());
        let sync = peer.expect(DISPLAY, 0);
        let mut args = wire::Cursor::new(&sync.payload);
        let callback = args.u32().unwrap();
        args.finish().unwrap();
        let mut done = wire::Builder::new();
        done.u32(9);
        peer.send(callback, 0, done);
        let mut delete = wire::Builder::new();
        delete.u32(callback);
        peer.send(DISPLAY, 1, delete);
        let message = connection.next().unwrap();
        assert!(connection.handle_common(&message).unwrap());
        assert!(connection.pulse().is_err());
        let message = connection.next().unwrap();
        assert!(connection.handle_common(&message).unwrap());
        connection.pulse().unwrap();
        assert_eq!(connection.keepalive_callbacks.len(), 1);
    }

    #[test]
    fn incoming_events_do_not_suppress_a_write_idle_pulse() {
        let (client, server) = UnixStream::pair().unwrap();
        let last_write = Instant::now().checked_sub(IDLE_PULSE).unwrap();
        let mut connection = Connection {
            stream: client,
            buffered: Vec::new(),
            pending_fds: VecDeque::new(),
            incoming: [0; RECEIVE_BUFFER_BYTES],
            deadline: None,
            last_write,
            next_id: FIRST_DYNAMIC_ID,
            free_ids: BTreeSet::new(),
            keepalive_callbacks: BTreeSet::new(),
            retired_keepalive: None,
        };
        let mut peer = Peer::new(server);
        let mut incoming = wire::Builder::new();
        incoming.u32(91);
        peer.send(REGISTRY, 1, incoming);
        assert_eq!(connection.next().unwrap().object, REGISTRY);
        let sync = peer.expect(DISPLAY, 0);
        let mut args = wire::Cursor::new(&sync.payload);
        let callback = args.u32().unwrap();
        args.finish().unwrap();
        assert!(connection.keepalive_callbacks.contains(&callback));
    }

    #[test]
    fn first_presentation_does_not_clear_a_dismissal_deadline() {
        let temp = Temp::new("dismiss-deadline");
        let root = temp.0.join("Downloads");
        fs::create_dir(&root).unwrap();
        let mut dialog = Dialog::new(DialogConfig {
            socket: temp.0.join("wayland-0"),
            runtime_directory: temp.0.clone(),
            title: String::new(),
            parent_handle: "0123456789abcdef".into(),
            app_id: "firefox".into(),
            host_root: root,
            guest_root: PathBuf::from("/home/td/Downloads"),
            mode: Mode::OpenFile { multiple: false },
            accept_label: None,
            filter: None,
            connector: Arc::new(AtomicBool::new(false)),
        })
        .unwrap();
        assert_eq!(dialog.window_title, "firefox — Open file");
        dialog.dismissing = true;
        dialog.portal_state = Some(1);
        dialog.keyboard_focused = true;
        dialog.frame = Some(Frame {
            buffer: 20,
            callback: 21,
            released: true,
            presented: true,
            width: 640,
            height: 432,
            checksum: 9,
        });
        let (client, _server) = UnixStream::pair().unwrap();
        let deadline = Instant::now().checked_add(HANDSHAKE_TIMEOUT).unwrap();
        let mut connection = Connection {
            stream: client,
            buffered: Vec::new(),
            pending_fds: VecDeque::new(),
            incoming: [0; RECEIVE_BUFFER_BYTES],
            deadline: Some(deadline),
            last_write: Instant::now(),
            next_id: FIRST_DYNAMIC_ID,
            free_ids: BTreeSet::new(),
            keepalive_callbacks: BTreeSet::new(),
            retired_keepalive: None,
        };
        let notices = std::cell::RefCell::new(Vec::new());
        dialog
            .settle(&mut connection, &|notice| {
                notices.borrow_mut().push(notice);
                Ok(())
            })
            .unwrap();
        assert_eq!(connection.deadline, Some(deadline));
        assert!(matches!(
            notices.borrow().as_slice(),
            [Notice::Presented { .. }]
        ));
    }

    #[test]
    fn maximum_caller_title_keeps_the_authenticated_prefix() {
        let temp = Temp::new("maximum-title");
        let root = temp.0.join("Downloads");
        fs::create_dir(&root).unwrap();
        let dialog = Dialog::new(DialogConfig {
            socket: temp.0.join("wayland-0"),
            runtime_directory: temp.0.clone(),
            title: "a".repeat(256),
            parent_handle: String::new(),
            app_id: "firefox".into(),
            host_root: root,
            guest_root: PathBuf::from("/home/td/Downloads"),
            mode: Mode::OpenFile { multiple: false },
            accept_label: None,
            filter: None,
            connector: Arc::new(AtomicBool::new(false)),
        })
        .unwrap();
        assert!(dialog.window_title.starts_with("firefox — "));
        assert_eq!(dialog.window_title.len(), 268);
        assert!(dialog.window_title.len() <= file_chooser::MAX_RENDERED_TITLE_BYTES);
    }

    #[test]
    fn stalled_connect_has_one_bounded_worker_lane() {
        let connector = Arc::new(AtomicBool::new(false));
        let (release, blocked) = mpsc::sync_channel(1);
        let deadline = Instant::now()
            .checked_add(Duration::from_millis(50))
            .unwrap();
        let error = bounded_connect(connector.clone(), deadline, move || {
            blocked.recv().unwrap();
            Err("released stalled connector".into())
        })
        .unwrap_err();
        assert!(error.contains("20-second deadline"));

        let invoked = Arc::new(AtomicBool::new(false));
        let invoked_by_worker = invoked.clone();
        let deadline = Instant::now().checked_add(Duration::from_secs(1)).unwrap();
        let error = bounded_connect(connector.clone(), deadline, move || {
            invoked_by_worker.store(true, Ordering::Release);
            Ok(UnixStream::pair().unwrap().0)
        })
        .unwrap_err();
        assert!(error.contains("connector is already occupied"));
        assert!(!invoked.load(Ordering::Acquire));

        release.send(()).unwrap();
        for _ in 0..100 {
            if !connector.load(Ordering::Acquire) {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(!connector.load(Ordering::Acquire));
        let deadline = Instant::now().checked_add(Duration::from_secs(1)).unwrap();
        assert!(bounded_connect(connector, deadline, || {
            Ok(UnixStream::pair().unwrap().0)
        })
        .is_ok());
    }

    #[test]
    fn presentation_waits_for_portal_admission_and_keyboard_focus() {
        let temp = Temp::new("portal-admission");
        let root = temp.0.join("Downloads");
        fs::create_dir(&root).unwrap();
        let mut dialog = Dialog::new(DialogConfig {
            socket: temp.0.join("wayland-0"),
            runtime_directory: temp.0.clone(),
            title: "Choose a report".into(),
            parent_handle: "0123456789abcdef".into(),
            app_id: "firefox".into(),
            host_root: root,
            guest_root: PathBuf::from("/home/td/Downloads"),
            mode: Mode::OpenFile { multiple: false },
            accept_label: None,
            filter: None,
            connector: Arc::new(AtomicBool::new(false)),
        })
        .unwrap();
        dialog.frame = Some(Frame {
            buffer: 20,
            callback: 21,
            released: true,
            presented: true,
            width: 640,
            height: 432,
            checksum: 9,
        });
        let (client, _server) = UnixStream::pair().unwrap();
        let deadline = Instant::now().checked_add(HANDSHAKE_TIMEOUT).unwrap();
        let mut connection = Connection {
            stream: client,
            buffered: Vec::new(),
            pending_fds: VecDeque::new(),
            incoming: [0; RECEIVE_BUFFER_BYTES],
            deadline: Some(deadline),
            last_write: Instant::now(),
            next_id: FIRST_DYNAMIC_ID,
            free_ids: BTreeSet::new(),
            keepalive_callbacks: BTreeSet::new(),
            retired_keepalive: None,
        };
        let notices = std::cell::RefCell::new(Vec::new());
        let record = |notice| {
            notices.borrow_mut().push(notice);
            Ok(())
        };
        dialog.settle(&mut connection, &record).unwrap();
        assert!(notices.borrow().is_empty());
        assert!(dialog.frame.is_some());
        assert_eq!(connection.deadline, Some(deadline));

        dialog.portal_state = Some(1);
        dialog.settle(&mut connection, &record).unwrap();
        assert!(notices.borrow().is_empty());
        assert!(dialog.frame.is_some());
        assert_eq!(connection.deadline, Some(deadline));

        dialog.keyboard_focused = true;
        dialog.settle(&mut connection, &record).unwrap();
        assert!(matches!(
            notices.borrow().as_slice(),
            [Notice::Presented { .. }]
        ));
        assert!(dialog.frame.is_none());
        assert_eq!(connection.deadline, None);
    }

    #[test]
    fn fake_compositor_observes_pixels_and_physical_acceptance() {
        let temp = Temp::new("round-trip");
        let socket = temp.0.join("wayland-0");
        let root = temp.0.join("Downloads");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("report.txt"), b"authenticated fixture").unwrap();
        let listener = UnixListener::bind(&socket).unwrap();
        let (notice_tx, notice_rx) = mpsc::channel();
        let server_root = temp.0.clone();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut peer = Peer::new(stream);
            let registry = peer.expect(DISPLAY, 1);
            let mut args = wire::Cursor::new(&registry.payload);
            assert_eq!(args.u32().unwrap(), REGISTRY);
            args.finish().unwrap();
            let sync = peer.expect(DISPLAY, 0);
            let mut args = wire::Cursor::new(&sync.payload);
            assert_eq!(args.u32().unwrap(), SYNC_CALLBACK);
            args.finish().unwrap();

            for (index, (interface, version)) in EXPECTED_GLOBALS.iter().enumerate() {
                let mut global = wire::Builder::new();
                global.u32(u32::try_from(index + 101).unwrap());
                global.string(interface).unwrap();
                global.u32(*version);
                peer.send(REGISTRY, 0, global);
            }
            let mut done = wire::Builder::new();
            done.u32(1);
            peer.send(SYNC_CALLBACK, 0, done);

            for (object, opcode) in [
                (REGISTRY, 0),
                (REGISTRY, 0),
                (REGISTRY, 0),
                (REGISTRY, 0),
                (REGISTRY, 0),
                (COMPOSITOR, 0),
                (XDG_WM_BASE, 2),
                (XDG_SURFACE, 1),
            ] {
                peer.expect(object, opcode);
            }
            let title = peer.expect(XDG_TOPLEVEL, 2);
            let mut args = wire::Cursor::new(&title.payload);
            assert_eq!(args.string().unwrap(), "firefox — Choose a report");
            args.finish().unwrap();
            let dialog = peer.expect(PORTAL_MANAGER, 0);
            let mut args = wire::Cursor::new(&dialog.payload);
            assert_eq!(args.u32().unwrap(), SURFACE);
            assert_eq!(args.string().unwrap(), "0123456789abcdef");
            assert_eq!(args.u32().unwrap(), 0);
            args.finish().unwrap();
            peer.expect(SURFACE, 6);

            let mut format = wire::Builder::new();
            format.u32(SHM_XRGB8888);
            peer.send(SHM, 0, format);
            let mut capabilities = wire::Builder::new();
            capabilities.u32(SEAT_KEYBOARD);
            peer.send(SEAT, 0, capabilities);
            let keyboard = peer.expect(SEAT, 1);
            let mut args = wire::Cursor::new(&keyboard.payload);
            assert_eq!(args.u32().unwrap(), KEYBOARD);
            args.finish().unwrap();

            let mut keymap_bytes = XKB_KEYMAP.as_bytes().to_vec();
            keymap_bytes.push(0);
            let keymap = backing_file(&server_root, &keymap_bytes).unwrap();
            let mut keymap_event = wire::Builder::new();
            keymap_event.u32(1);
            keymap_event.u32(u32::try_from(keymap_bytes.len()).unwrap());
            peer.send_fd(KEYBOARD, 0, keymap_event, &keymap);
            let mut enter = wire::Builder::new();
            enter.u32(29);
            enter.u32(SURFACE);
            enter.array(&[]).unwrap();
            peer.send(KEYBOARD, 1, enter);
            let mut portal_state = wire::Builder::new();
            portal_state.u32(SURFACE);
            portal_state.u32(1);
            peer.send(PORTAL_MANAGER, 0, portal_state);
            let mut toplevel = wire::Builder::new();
            toplevel.i32(0);
            toplevel.i32(0);
            toplevel.array(&[]).unwrap();
            peer.send(XDG_TOPLEVEL, 0, toplevel);
            let mut configure = wire::Builder::new();
            configure.u32(23);
            peer.send(XDG_SURFACE, 0, configure);

            let ack = peer.expect(XDG_SURFACE, 4);
            let mut args = wire::Cursor::new(&ack.payload);
            assert_eq!(args.u32().unwrap(), 23);
            args.finish().unwrap();
            let (buffer, callback, rendered_checksum) = peer.expect_frame(640, 432);
            let mut toplevel = wire::Builder::new();
            toplevel.i32(600);
            toplevel.i32(402);
            toplevel.array(&[]).unwrap();
            peer.send(XDG_TOPLEVEL, 0, toplevel);
            let mut configure = wire::Builder::new();
            configure.u32(24);
            peer.send(XDG_SURFACE, 0, configure);
            let ack = peer.expect(XDG_SURFACE, 4);
            let mut args = wire::Cursor::new(&ack.payload);
            assert_eq!(args.u32().unwrap(), 24);
            args.finish().unwrap();
            peer.send(buffer, 0, wire::Builder::new());
            let mut frame_done = wire::Builder::new();
            frame_done.u32(99);
            peer.send(callback, 0, frame_done);
            peer.expect(buffer, 0);
            let (buffer, callback, resized_checksum) = peer.expect_frame(600, 402);
            assert_ne!(resized_checksum, rendered_checksum);
            peer.send(buffer, 0, wire::Builder::new());
            let mut frame_done = wire::Builder::new();
            frame_done.u32(100);
            peer.send(callback, 0, frame_done);
            let mut key = wire::Builder::new();
            key.u32(31);
            key.u32(1);
            key.u32(28);
            key.u32(KEY_PRESSED);
            peer.send(KEYBOARD, 3, key);

            loop {
                let request = peer.next();
                if (request.object, request.opcode) == (PORTAL_MANAGER, 1) {
                    let mut args = wire::Cursor::new(&request.payload);
                    assert_eq!(args.u32().unwrap(), SURFACE);
                    args.finish().unwrap();
                    break;
                }
            }
            let mut dismissed = wire::Builder::new();
            dismissed.u32(SURFACE);
            dismissed.u32(PORTAL_DIALOG_DISMISSED);
            peer.send(PORTAL_MANAGER, 0, dismissed);
            rendered_checksum
        });

        spawn(
            DialogConfig {
                socket,
                runtime_directory: temp.0.clone(),
                title: "Choose a report".into(),
                parent_handle: "0123456789abcdef".into(),
                app_id: "firefox".into(),
                host_root: root,
                guest_root: PathBuf::from("/home/td/Downloads"),
                mode: Mode::OpenFile { multiple: false },
                accept_label: None,
                filter: None,
                connector: Arc::new(AtomicBool::new(false)),
            },
            move |notice| {
                notice_tx
                    .send(notice)
                    .map_err(|error| format!("record dialog notice: {error}"))
            },
        )
        .unwrap();

        assert!(matches!(
            notice_rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            Notice::Connected(_)
        ));
        let presented_checksum = match notice_rx.recv_timeout(Duration::from_secs(5)).unwrap() {
            Notice::Presented {
                width,
                height,
                checksum,
            } => {
                assert_eq!((width, height), (640, 432));
                checksum
            }
            other => panic!("expected presented notice, got {other:?}"),
        };
        match notice_rx.recv_timeout(Duration::from_secs(5)).unwrap() {
            Notice::Completed(Ok(Outcome::Accepted(uris))) => assert_eq!(
                uris,
                vec!["file:///home/td/Downloads/report.txt".to_string()]
            ),
            other => panic!("expected accepted completion, got {other:?}"),
        }
        assert_eq!(server.join().unwrap(), presented_checksum);
    }
}

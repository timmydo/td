use crate::runtime::Runtime;
use crate::scene::{Surface, SurfaceKey, SHM_ARGB8888, SHM_XRGB8888};
use crate::{socket, sys, wire};
use std::collections::{BTreeMap, VecDeque};
use std::fs::{self, File, Permissions};
use std::io::Write;
use std::os::fd::RawFd;
use std::os::unix::fs::{FileExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

const GLOBAL_COMPOSITOR: u32 = 1;
const GLOBAL_SHM: u32 = 2;
const GLOBAL_OUTPUT: u32 = 3;
const GLOBAL_XDG_WM_BASE: u32 = 4;
const MAX_POOL_BYTES: usize = 64 * 1024 * 1024;
const MAX_CLIENT_BUFFER: usize = 256 * 1024;
const MAX_PENDING_FDS: usize = 64;
const MAX_SURFACE_DIMENSION: usize = 16_384;
const MAX_OBJECTS: usize = 512;
const MAX_CLIENT_SURFACE_BYTES: usize = 32 * 1024 * 1024;
const MAX_CLIENTS: usize = 32;

static NEXT_CLIENT: AtomicU64 = AtomicU64::new(1);
static NEXT_SERIAL: AtomicU64 = AtomicU64::new(1);
static NEXT_BUFFER_SERIAL: AtomicU64 = AtomicU64::new(1);
static ACTIVE_CLIENTS: AtomicUsize = AtomicUsize::new(0);

#[derive(Clone)]
struct Pool {
    file: Arc<File>,
    size: usize,
}

#[derive(Clone)]
struct Buffer {
    serial: u64,
    file: Arc<File>,
    offset: usize,
    width: usize,
    height: usize,
    stride: usize,
    format: u32,
}

#[derive(Clone)]
enum PendingBuffer {
    Detach,
    Buffer { object: u32, buffer: Buffer },
}

#[derive(Clone, Default)]
struct SurfaceState {
    pending_buffer: Option<PendingBuffer>,
    frame_callbacks: Vec<u32>,
    role: Option<u32>,
}

#[derive(Clone)]
enum Object {
    Display,
    Registry,
    Compositor,
    Region,
    Shm,
    Pool(Pool),
    Buffer(Buffer),
    Surface(SurfaceState),
    Callback,
    Output {
        version: u32,
    },
    XdgWmBase,
    XdgSurface {
        surface: u32,
        toplevel: Option<u32>,
        configure_serial: Option<u32>,
        configured: bool,
    },
    XdgToplevel {
        xdg_surface: u32,
    },
}

struct Client {
    id: u64,
    stream: UnixStream,
    disconnected: bool,
    objects: BTreeMap<u32, Object>,
    runtime: Arc<Mutex<Runtime>>,
    mapped_bytes: BTreeMap<u32, usize>,
    mapped_total: usize,
}

struct ClientPermit;

impl ClientPermit {
    fn acquire() -> Result<ClientPermit, String> {
        let mut current = ACTIVE_CLIENTS.load(Ordering::Acquire);
        loop {
            if current >= MAX_CLIENTS {
                return Err(format!(
                    "refusing Wayland client: {current} connections already active"
                ));
            }
            match ACTIVE_CLIENTS.compare_exchange_weak(
                current,
                current.saturating_add(1),
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => return Ok(ClientPermit),
                Err(observed) => current = observed,
            }
        }
    }
}

impl Drop for ClientPermit {
    fn drop(&mut self) {
        ACTIVE_CLIENTS.fetch_sub(1, Ordering::AcqRel);
    }
}

fn next_serial() -> u32 {
    let value = NEXT_SERIAL.fetch_add(1, Ordering::Relaxed);
    let folded = value % u64::from(u32::MAX);
    u32::try_from(folded).unwrap_or(1).max(1)
}

fn client_surface_total(current: usize, prior: usize, proposed: usize) -> Result<usize, String> {
    let retained = current
        .checked_sub(prior)
        .ok_or_else(|| "client surface byte accounting underflow".to_string())?;
    let next = retained
        .checked_add(proposed)
        .ok_or_else(|| "client surface byte accounting overflow".to_string())?;
    if next > MAX_CLIENT_SURFACE_BYTES {
        return Err(format!(
            "client surfaces need {next} bytes, exceeding {MAX_CLIENT_SURFACE_BYTES}"
        ));
    }
    Ok(next)
}

#[cfg(test)]
fn request(object: u32, opcode: u16, builder: wire::Builder) -> Result<wire::Message, String> {
    let mut encoded = builder.message(object, opcode)?;
    wire::take(&mut encoded)?.ok_or_else(|| "request builder emitted no message".to_string())
}

impl Client {
    fn new(id: u64, stream: UnixStream, runtime: Arc<Mutex<Runtime>>) -> Client {
        let mut objects = BTreeMap::new();
        objects.insert(1, Object::Display);
        Client {
            id,
            stream,
            disconnected: false,
            objects,
            runtime,
            mapped_bytes: BTreeMap::new(),
            mapped_total: 0,
        }
    }

    fn clear_surface_bytes(&mut self, surface: u32) {
        if let Some(bytes) = self.mapped_bytes.remove(&surface) {
            self.mapped_total = self.mapped_total.saturating_sub(bytes);
        }
    }

    fn unmap_surface(&mut self, surface: u32) -> Result<(), String> {
        self.clear_surface_bytes(surface);
        self.runtime
            .lock()
            .map_err(|_| "runtime lock poisoned".to_string())?
            .unmap(SurfaceKey {
                client: self.id,
                object: surface,
            })
    }

    fn remove_surface(&mut self, surface: u32) -> Result<(), String> {
        self.clear_surface_bytes(surface);
        self.runtime
            .lock()
            .map_err(|_| "runtime lock poisoned".to_string())?
            .remove(SurfaceKey {
                client: self.id,
                object: surface,
            })
    }

    fn send(&mut self, object: u32, opcode: u16, builder: wire::Builder) -> Result<(), String> {
        let message = builder.message(object, opcode)?;
        if self.disconnected {
            return Ok(());
        }
        match self.stream.write_all(&message) {
            Ok(()) => Ok(()),
            Err(error) if sys::write_peer_disconnected(&error) => {
                self.disconnected = true;
                Ok(())
            }
            Err(error) => Err(format!("write Wayland event: {error}")),
        }
    }

    fn delete_id(&mut self, id: u32) -> Result<(), String> {
        let mut event = wire::Builder::new();
        event.u32(id);
        self.send(1, 1, event)
    }

    fn protocol_error(&mut self, object: u32, error: &str) {
        let mut event = wire::Builder::new();
        event.u32(object);
        event.u32(3);
        if event.string(error).is_ok() {
            let _ = self.send(1, 0, event);
        }
    }

    fn remove_object(&mut self, id: u32) -> Result<(), String> {
        if id <= 1 {
            return Err(format!("refusing to delete reserved object {id}"));
        }
        if self.objects.remove(&id).is_none() {
            return Err(format!("object {id} does not exist"));
        }
        self.delete_id(id)
    }

    fn insert(&mut self, id: u32, object: Object) -> Result<(), String> {
        if id <= 1 {
            return Err(format!("new object id {id} is reserved"));
        }
        if self.objects.contains_key(&id) {
            return Err(format!("object id {id} was reused before delete_id"));
        }
        if self.objects.len() >= MAX_OBJECTS {
            return Err(format!("client exceeded the {MAX_OBJECTS}-object limit"));
        }
        self.objects.insert(id, object);
        Ok(())
    }

    fn global(
        &mut self,
        registry: u32,
        name: u32,
        interface: &str,
        version: u32,
    ) -> Result<(), String> {
        let mut event = wire::Builder::new();
        event.u32(name);
        event.string(interface)?;
        event.u32(version);
        self.send(registry, 0, event)
    }

    fn advertise_globals(&mut self, registry: u32) -> Result<(), String> {
        self.global(registry, GLOBAL_COMPOSITOR, "wl_compositor", 4)?;
        self.global(registry, GLOBAL_SHM, "wl_shm", 1)?;
        self.global(registry, GLOBAL_OUTPUT, "wl_output", 4)?;
        self.global(registry, GLOBAL_XDG_WM_BASE, "xdg_wm_base", 1)
    }

    fn bind_global(
        &mut self,
        name: u32,
        interface: &str,
        version: u32,
        id: u32,
    ) -> Result<(), String> {
        match (name, interface) {
            (GLOBAL_COMPOSITOR, "wl_compositor") if (1..=4).contains(&version) => {
                self.insert(id, Object::Compositor)
            }
            (GLOBAL_SHM, "wl_shm") if version == 1 => {
                self.insert(id, Object::Shm)?;
                for format in [SHM_ARGB8888, SHM_XRGB8888] {
                    let mut event = wire::Builder::new();
                    event.u32(format);
                    self.send(id, 0, event)?;
                }
                Ok(())
            }
            (GLOBAL_OUTPUT, "wl_output") if (1..=4).contains(&version) => {
                self.insert(id, Object::Output { version })?;
                self.send_output(id, version)
            }
            (GLOBAL_XDG_WM_BASE, "xdg_wm_base") if version == 1 => {
                self.insert(id, Object::XdgWmBase)
            }
            _ => Err(format!(
                "global {name} does not provide {interface} version {version}"
            )),
        }
    }

    fn send_output(&mut self, id: u32, version: u32) -> Result<(), String> {
        let (width, height) = {
            let runtime = self
                .runtime
                .lock()
                .map_err(|_| "runtime lock poisoned".to_string())?;
            (runtime.width(), runtime.height())
        };
        let width = i32::try_from(width).map_err(|_| "output width exceeds i32".to_string())?;
        let height = i32::try_from(height).map_err(|_| "output height exceeds i32".to_string())?;

        let mut geometry = wire::Builder::new();
        geometry.i32(0);
        geometry.i32(0);
        geometry.i32(270);
        geometry.i32(170);
        geometry.i32(0);
        geometry.string("td")?;
        geometry.string("software framebuffer")?;
        geometry.i32(0);
        self.send(id, 0, geometry)?;

        let mut mode = wire::Builder::new();
        mode.u32(3);
        mode.i32(width);
        mode.i32(height);
        mode.i32(60_000);
        self.send(id, 1, mode)?;

        if version >= 2 {
            let mut scale = wire::Builder::new();
            scale.i32(1);
            self.send(id, 3, scale)?;
        }
        if version >= 4 {
            let mut name = wire::Builder::new();
            name.string("TD-1")?;
            self.send(id, 4, name)?;
            let mut description = wire::Builder::new();
            description.string("td software framebuffer")?;
            self.send(id, 5, description)?;
        }
        if version >= 2 {
            self.send(id, 2, wire::Builder::new())?;
        }
        Ok(())
    }

    fn create_pool(
        &mut self,
        id: u32,
        declared_size: i32,
        fds: &mut VecDeque<RawFd>,
    ) -> Result<(), String> {
        if declared_size <= 0 {
            return Err(format!("wl_shm pool size {declared_size} is not positive"));
        }
        let size =
            usize::try_from(declared_size).map_err(|_| "wl_shm pool size overflow".to_string())?;
        if size > MAX_POOL_BYTES {
            return Err(format!("wl_shm pool size {size} exceeds {MAX_POOL_BYTES}"));
        }
        let fd = fds
            .pop_front()
            .ok_or_else(|| "wl_shm.create_pool arrived without a descriptor".to_string())?;
        let file = sys::duplicate_received(fd)?;
        let actual = usize::try_from(
            file.metadata()
                .map_err(|e| format!("stat wl_shm pool: {e}"))?
                .len(),
        )
        .map_err(|_| "wl_shm backing file is too large".to_string())?;
        if size > actual {
            return Err(format!(
                "wl_shm declared {size} bytes but backing file has {actual}"
            ));
        }
        self.insert(
            id,
            Object::Pool(Pool {
                file: Arc::new(file),
                size,
            }),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn create_buffer(
        &mut self,
        pool: Pool,
        id: u32,
        offset: i32,
        width: i32,
        height: i32,
        stride: i32,
        format: u32,
    ) -> Result<(), String> {
        if offset < 0 || width <= 0 || height <= 0 || stride <= 0 {
            return Err(format!(
                "invalid wl_shm buffer offset={offset} width={width} height={height} stride={stride}"
            ));
        }
        let offset = usize::try_from(offset).map_err(|_| "buffer offset overflow".to_string())?;
        let width = usize::try_from(width).map_err(|_| "buffer width overflow".to_string())?;
        let height = usize::try_from(height).map_err(|_| "buffer height overflow".to_string())?;
        let stride = usize::try_from(stride).map_err(|_| "buffer stride overflow".to_string())?;
        if width > MAX_SURFACE_DIMENSION || height > MAX_SURFACE_DIMENSION {
            return Err(format!(
                "wl_shm buffer {width}x{height} exceeds the dimension limit"
            ));
        }
        if !matches!(format, SHM_ARGB8888 | SHM_XRGB8888) {
            return Err(format!("unsupported wl_shm format {format}"));
        }
        let row = width
            .checked_mul(4)
            .ok_or_else(|| "wl_shm row size overflow".to_string())?;
        if stride < row {
            return Err(format!(
                "wl_shm stride {stride} is smaller than the {row}-byte row"
            ));
        }
        let final_row = height
            .saturating_sub(1)
            .checked_mul(stride)
            .and_then(|value| value.checked_add(row))
            .and_then(|value| value.checked_add(offset))
            .ok_or_else(|| "wl_shm buffer extent overflow".to_string())?;
        if final_row > pool.size {
            return Err(format!(
                "wl_shm buffer ends at {final_row}, beyond pool size {}",
                pool.size
            ));
        }
        let tight = row
            .checked_mul(height)
            .ok_or_else(|| "wl_shm copied surface size overflow".to_string())?;
        if tight > MAX_POOL_BYTES {
            return Err(format!(
                "wl_shm copied surface exceeds {MAX_POOL_BYTES} bytes"
            ));
        }
        self.insert(
            id,
            Object::Buffer(Buffer {
                serial: NEXT_BUFFER_SERIAL.fetch_add(1, Ordering::Relaxed),
                file: pool.file,
                offset,
                width,
                height,
                stride,
                format,
            }),
        )
    }

    fn copy_buffer(buffer: &Buffer) -> Result<Surface, String> {
        let row = buffer
            .width
            .checked_mul(4)
            .ok_or_else(|| "surface row overflow".to_string())?;
        let total = row
            .checked_mul(buffer.height)
            .ok_or_else(|| "surface size overflow".to_string())?;
        let mut pixels = vec![0; total];
        for source_y in 0..buffer.height {
            let source = buffer
                .offset
                .checked_add(
                    source_y
                        .checked_mul(buffer.stride)
                        .ok_or_else(|| "source row overflow".to_string())?,
                )
                .ok_or_else(|| "source offset overflow".to_string())?;
            let target = source_y
                .checked_mul(row)
                .ok_or_else(|| "target row overflow".to_string())?;
            let target_end = target
                .checked_add(row)
                .ok_or_else(|| "target row end overflow".to_string())?;
            let destination = pixels
                .get_mut(target..target_end)
                .ok_or_else(|| "target row escaped surface".to_string())?;
            buffer
                .file
                .read_exact_at(
                    destination,
                    u64::try_from(source).map_err(|_| "source offset exceeds u64".to_string())?,
                )
                .map_err(|e| format!("read wl_shm row {source_y}: {e}"))?;
        }
        Ok(Surface {
            width: buffer.width,
            height: buffer.height,
            pixels,
            format: buffer.format,
        })
    }

    fn commit_surface(&mut self, id: u32, state: SurfaceState) -> Result<(), String> {
        let attaching_buffer = matches!(state.pending_buffer, Some(PendingBuffer::Buffer { .. }));
        if let Some(role) = state.role {
            let xdg = self
                .objects
                .get(&role)
                .cloned()
                .ok_or_else(|| format!("wl_surface {id} has a destroyed xdg_surface role"))?;
            let Object::XdgSurface {
                toplevel,
                configure_serial,
                configured,
                ..
            } = xdg
            else {
                return Err(format!("wl_surface {id} has a non-XDG role object"));
            };
            let toplevel =
                toplevel.ok_or_else(|| format!("xdg_surface {role} has no role object"))?;
            if configure_serial.is_none() {
                if attaching_buffer {
                    return Err(format!(
                        "xdg_surface {role} attached a buffer before its initial configure"
                    ));
                }
                let serial = self.configure_xdg(role, toplevel)?;
                self.objects.insert(
                    role,
                    Object::XdgSurface {
                        surface: id,
                        toplevel: Some(toplevel),
                        configure_serial: Some(serial),
                        configured: false,
                    },
                );
            } else if attaching_buffer && !configured {
                return Err(format!(
                    "xdg_surface {role} attached a buffer before acknowledging configure"
                ));
            }
        } else if attaching_buffer {
            return Err(format!("wl_surface {id} attached a buffer without a role"));
        }
        if let Some(pending) = state.pending_buffer {
            let key = SurfaceKey {
                client: self.id,
                object: id,
            };
            match pending {
                PendingBuffer::Detach => {
                    self.unmap_surface(id)?;
                }
                PendingBuffer::Buffer { object, buffer } => {
                    let surface_bytes = buffer
                        .width
                        .checked_mul(buffer.height)
                        .and_then(|pixels| pixels.checked_mul(4))
                        .ok_or_else(|| "client surface byte count overflow".to_string())?;
                    let prior = self.mapped_bytes.get(&id).copied().unwrap_or(0);
                    let next = client_surface_total(self.mapped_total, prior, surface_bytes)?;
                    let surface = Self::copy_buffer(&buffer)?;
                    self.runtime
                        .lock()
                        .map_err(|_| "runtime lock poisoned".to_string())?
                        .commit(key, surface)?;
                    self.mapped_bytes.insert(id, surface_bytes);
                    self.mapped_total = next;
                    if matches!(
                        self.objects.get(&object),
                        Some(Object::Buffer(current)) if current.serial == buffer.serial
                    ) {
                        self.send(object, 0, wire::Builder::new())?;
                    }
                }
            }
        }
        for callback in state.frame_callbacks {
            let mut done = wire::Builder::new();
            done.u32(next_serial());
            self.send(callback, 0, done)?;
            self.objects.remove(&callback);
            self.delete_id(callback)?;
        }
        if let Some(Object::Surface(current)) = self.objects.get_mut(&id) {
            current.pending_buffer = None;
            current.frame_callbacks.clear();
        }
        Ok(())
    }

    fn configure_xdg(&mut self, xdg_surface: u32, toplevel: u32) -> Result<u32, String> {
        let mut toplevel_configure = wire::Builder::new();
        toplevel_configure.i32(0);
        toplevel_configure.i32(0);
        toplevel_configure.array(&[])?;
        self.send(toplevel, 0, toplevel_configure)?;

        let serial = next_serial();
        let mut surface_configure = wire::Builder::new();
        surface_configure.u32(serial);
        self.send(xdg_surface, 0, surface_configure)?;
        Ok(serial)
    }

    fn dispatch(
        &mut self,
        message: wire::Message,
        fds: &mut VecDeque<RawFd>,
    ) -> Result<(), String> {
        let object = self
            .objects
            .get(&message.object)
            .cloned()
            .ok_or_else(|| format!("request for unknown object {}", message.object))?;
        let mut args = wire::Cursor::new(&message.payload);
        match object {
            Object::Display => match message.opcode {
                0 => {
                    let callback = args.u32()?;
                    args.finish()?;
                    self.insert(callback, Object::Callback)?;
                    let mut done = wire::Builder::new();
                    done.u32(next_serial());
                    self.send(callback, 0, done)?;
                    self.objects.remove(&callback);
                    self.delete_id(callback)
                }
                1 => {
                    let registry = args.u32()?;
                    args.finish()?;
                    self.insert(registry, Object::Registry)?;
                    self.advertise_globals(registry)
                }
                _ => Err(format!("unsupported wl_display request {}", message.opcode)),
            },
            Object::Registry => match message.opcode {
                0 => {
                    let name = args.u32()?;
                    let interface = args.string()?;
                    let version = args.u32()?;
                    let id = args.u32()?;
                    args.finish()?;
                    self.bind_global(name, &interface, version, id)
                }
                _ => Err(format!(
                    "unsupported wl_registry request {}",
                    message.opcode
                )),
            },
            Object::Compositor => match message.opcode {
                0 => {
                    let surface = args.u32()?;
                    args.finish()?;
                    self.insert(surface, Object::Surface(SurfaceState::default()))
                }
                1 => {
                    let region = args.u32()?;
                    args.finish()?;
                    self.insert(region, Object::Region)
                }
                _ => Err(format!(
                    "unsupported wl_compositor request {}",
                    message.opcode
                )),
            },
            Object::Region => match message.opcode {
                0 => {
                    args.finish()?;
                    self.remove_object(message.object)
                }
                1 | 2 => {
                    for _ in 0..4 {
                        args.i32()?;
                    }
                    args.finish()
                }
                _ => Err(format!("unsupported wl_region request {}", message.opcode)),
            },
            Object::Shm => match message.opcode {
                0 => {
                    let id = args.u32()?;
                    let size = args.i32()?;
                    args.finish()?;
                    self.create_pool(id, size, fds)
                }
                _ => Err(format!("unsupported wl_shm request {}", message.opcode)),
            },
            Object::Pool(pool) => match message.opcode {
                0 => {
                    let id = args.u32()?;
                    let offset = args.i32()?;
                    let width = args.i32()?;
                    let height = args.i32()?;
                    let stride = args.i32()?;
                    let format = args.u32()?;
                    args.finish()?;
                    self.create_buffer(pool, id, offset, width, height, stride, format)
                }
                1 => {
                    args.finish()?;
                    self.remove_object(message.object)
                }
                2 => {
                    let size = args.i32()?;
                    args.finish()?;
                    if size <= 0 {
                        return Err(format!("invalid wl_shm pool resize {size}"));
                    }
                    let size = usize::try_from(size)
                        .map_err(|_| "wl_shm pool resize overflow".to_string())?;
                    let actual = usize::try_from(
                        pool.file
                            .metadata()
                            .map_err(|e| format!("stat resized wl_shm pool: {e}"))?
                            .len(),
                    )
                    .map_err(|_| "resized wl_shm file is too large".to_string())?;
                    if size < pool.size || size > actual || size > MAX_POOL_BYTES {
                        return Err(format!(
                            "wl_shm pool resize {size} is outside {}..={}",
                            pool.size,
                            actual.min(MAX_POOL_BYTES)
                        ));
                    }
                    self.objects.insert(
                        message.object,
                        Object::Pool(Pool {
                            file: pool.file,
                            size,
                        }),
                    );
                    Ok(())
                }
                _ => Err(format!(
                    "unsupported wl_shm_pool request {}",
                    message.opcode
                )),
            },
            Object::Buffer(_) => match message.opcode {
                0 => {
                    args.finish()?;
                    self.remove_object(message.object)
                }
                _ => Err(format!("unsupported wl_buffer request {}", message.opcode)),
            },
            Object::Surface(mut state) => match message.opcode {
                0 => {
                    args.finish()?;
                    if state
                        .role
                        .is_some_and(|role| self.objects.contains_key(&role))
                    {
                        return Err(format!(
                            "wl_surface {} was destroyed before its role object",
                            message.object
                        ));
                    }
                    self.remove_surface(message.object)?;
                    self.remove_object(message.object)
                }
                1 => {
                    let buffer = args.u32()?;
                    args.i32()?;
                    args.i32()?;
                    args.finish()?;
                    state.pending_buffer = if buffer == 0 {
                        Some(PendingBuffer::Detach)
                    } else {
                        let buffer_state = match self.objects.get(&buffer).cloned() {
                            Some(Object::Buffer(buffer_state)) => buffer_state,
                            _ => {
                                return Err(format!(
                                    "surface attach references non-buffer {buffer}"
                                ))
                            }
                        };
                        Some(PendingBuffer::Buffer {
                            object: buffer,
                            buffer: buffer_state,
                        })
                    };
                    self.objects.insert(message.object, Object::Surface(state));
                    Ok(())
                }
                2 | 9 => {
                    for _ in 0..4 {
                        args.i32()?;
                    }
                    args.finish()
                }
                3 => {
                    let callback = args.u32()?;
                    args.finish()?;
                    self.insert(callback, Object::Callback)?;
                    state.frame_callbacks.push(callback);
                    self.objects.insert(message.object, Object::Surface(state));
                    Ok(())
                }
                4 | 5 => {
                    let region = args.u32()?;
                    args.finish()?;
                    if region != 0 && !matches!(self.objects.get(&region), Some(Object::Region)) {
                        return Err(format!("surface references non-region {region}"));
                    }
                    Ok(())
                }
                6 => {
                    args.finish()?;
                    self.commit_surface(message.object, state)
                }
                7 => {
                    let transform = args.i32()?;
                    args.finish()?;
                    if transform != 0 {
                        return Err(format!("unsupported buffer transform {transform}"));
                    }
                    Ok(())
                }
                8 => {
                    let scale = args.i32()?;
                    args.finish()?;
                    if scale != 1 {
                        return Err(format!("unsupported buffer scale {scale}"));
                    }
                    Ok(())
                }
                _ => Err(format!("unsupported wl_surface request {}", message.opcode)),
            },
            Object::Callback => Err(format!(
                "wl_callback object {} accepts no requests",
                message.object
            )),
            Object::Output { version } => match message.opcode {
                0 if version >= 3 => {
                    args.finish()?;
                    self.remove_object(message.object)
                }
                _ => Err(format!("unsupported wl_output request {}", message.opcode)),
            },
            Object::XdgWmBase => match message.opcode {
                0 => {
                    args.finish()?;
                    self.remove_object(message.object)
                }
                2 => {
                    let id = args.u32()?;
                    let surface = args.u32()?;
                    args.finish()?;
                    let mut state = match self.objects.get(&surface).cloned() {
                        Some(Object::Surface(state)) => state,
                        _ => {
                            return Err(format!(
                                "xdg_surface role references non-surface {surface}"
                            ))
                        }
                    };
                    if state.role.is_some() {
                        return Err(format!("wl_surface {surface} already has a role"));
                    }
                    self.insert(
                        id,
                        Object::XdgSurface {
                            surface,
                            toplevel: None,
                            configure_serial: None,
                            configured: false,
                        },
                    )?;
                    state.role = Some(id);
                    self.objects.insert(surface, Object::Surface(state));
                    Ok(())
                }
                3 => {
                    args.u32()?;
                    args.finish()
                }
                1 => Err("xdg_positioner is not supported".into()),
                _ => Err(format!(
                    "unsupported xdg_wm_base request {}",
                    message.opcode
                )),
            },
            Object::XdgSurface {
                surface,
                toplevel,
                configure_serial,
                configured,
            } => match message.opcode {
                0 => {
                    args.finish()?;
                    if toplevel.is_some() {
                        return Err(format!(
                            "xdg_surface {} was destroyed before its xdg_toplevel",
                            message.object
                        ));
                    }
                    self.remove_object(message.object)
                }
                1 => {
                    let new_toplevel = args.u32()?;
                    args.finish()?;
                    if toplevel.is_some() {
                        return Err(format!(
                            "xdg_surface {} already has a role object",
                            message.object
                        ));
                    }
                    if !matches!(self.objects.get(&surface), Some(Object::Surface(_))) {
                        return Err(format!(
                            "xdg_surface refers to missing wl_surface {surface}"
                        ));
                    }
                    self.insert(
                        new_toplevel,
                        Object::XdgToplevel {
                            xdg_surface: message.object,
                        },
                    )?;
                    self.objects.insert(
                        message.object,
                        Object::XdgSurface {
                            surface,
                            toplevel: Some(new_toplevel),
                            configure_serial,
                            configured,
                        },
                    );
                    Ok(())
                }
                3 => {
                    for _ in 0..4 {
                        args.i32()?;
                    }
                    args.finish()
                }
                4 => {
                    let serial = args.u32()?;
                    args.finish()?;
                    if configure_serial != Some(serial) {
                        return Err(format!(
                            "xdg_surface {} acknowledged unknown configure {serial}",
                            message.object
                        ));
                    }
                    self.objects.insert(
                        message.object,
                        Object::XdgSurface {
                            surface,
                            toplevel,
                            configure_serial,
                            configured: true,
                        },
                    );
                    Ok(())
                }
                2 => Err("xdg_popup is not supported".into()),
                _ => Err(format!(
                    "unsupported xdg_surface request {}",
                    message.opcode
                )),
            },
            Object::XdgToplevel { xdg_surface } => match message.opcode {
                0 => {
                    args.finish()?;
                    self.remove_object(message.object)?;
                    let Some(Object::XdgSurface { surface, .. }) =
                        self.objects.get(&xdg_surface).cloned()
                    else {
                        return Err(format!(
                            "xdg_toplevel {} lost xdg_surface {xdg_surface}",
                            message.object
                        ));
                    };
                    self.unmap_surface(surface)?;
                    self.objects.insert(
                        xdg_surface,
                        Object::XdgSurface {
                            surface,
                            toplevel: None,
                            configure_serial: None,
                            configured: false,
                        },
                    );
                    Ok(())
                }
                1 => {
                    args.u32()?;
                    args.finish()
                }
                2 | 3 => {
                    args.string()?;
                    args.finish()
                }
                7 | 8 => {
                    args.i32()?;
                    args.i32()?;
                    args.finish()
                }
                9 | 10 | 12 | 13 => args.finish(),
                4 | 5 | 6 | 11 => Err(format!(
                    "interactive xdg_toplevel request {} is not supported",
                    message.opcode
                )),
                _ => Err(format!(
                    "unsupported xdg_toplevel request {}",
                    message.opcode
                )),
            },
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
enum DispatchOutcome {
    NeedInput,
    Disconnected,
}

fn dispatch_buffered(
    client: &mut Client,
    bytes: &mut Vec<u8>,
    fds: &mut VecDeque<RawFd>,
) -> Result<DispatchOutcome, String> {
    loop {
        let message = match wire::take(bytes) {
            Ok(Some(message)) => message,
            Ok(None) => return Ok(DispatchOutcome::NeedInput),
            Err(error) => {
                client.protocol_error(1, &error);
                return Err(error);
            }
        };
        let object = message.object;
        if let Err(error) = client.dispatch(message, fds) {
            client.protocol_error(object, &error);
            return Err(error);
        }
        if client.disconnected {
            return Ok(DispatchOutcome::Disconnected);
        }
    }
}

fn serve_client(stream: UnixStream, id: u64, runtime: Arc<Mutex<Runtime>>) -> Result<(), String> {
    let mut client = Client::new(id, stream, Arc::clone(&runtime));
    let mut bytes = Vec::with_capacity(64 * 1024);
    let mut incoming = [0u8; 64 * 1024];
    let mut fds: VecDeque<RawFd> = VecDeque::new();
    let outcome = loop {
        match dispatch_buffered(&mut client, &mut bytes, &mut fds) {
            Ok(DispatchOutcome::NeedInput) => {}
            Ok(DispatchOutcome::Disconnected) => break Ok(()),
            Err(error) => break Err(error),
        }
        if bytes.len() > MAX_CLIENT_BUFFER {
            break Err(format!(
                "client receive buffer exceeded {MAX_CLIENT_BUFFER} bytes"
            ));
        }
        let received = match sys::recv_with_fds(&client.stream, &mut incoming) {
            Ok(value) => value,
            Err(sys::ReceiveError::Disconnected) => break Ok(()),
            Err(sys::ReceiveError::Failure(error)) => break Err(error),
        };
        if received.count == 0 {
            break Ok(());
        }
        let Some(received_bytes) = incoming.get(..received.count) else {
            break Err("recvmsg byte count escaped input buffer".to_string());
        };
        bytes.extend_from_slice(received_bytes);
        fds.extend(received.fds);
        if fds.len() > MAX_PENDING_FDS {
            break Err(format!(
                "client queued more than {MAX_PENDING_FDS} descriptors"
            ));
        }
    };
    let raw: Vec<RawFd> = fds.into_iter().collect();
    sys::discard_received(&raw);
    let cleanup = runtime
        .lock()
        .map_err(|_| "runtime lock poisoned".to_string())?
        .remove_client(id);
    match (outcome, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Err(error), Err(cleanup)) => Err(format!("{error}; client cleanup failed: {cleanup}")),
    }
}

pub fn serve(path: &Path, runtime: Arc<Mutex<Runtime>>) -> Result<(), String> {
    socket::remove_stale(path, "Wayland")?;
    let listener = UnixListener::bind(path)
        .map_err(|e| format!("bind Wayland socket {}: {e}", path.display()))?;
    fs::set_permissions(path, Permissions::from_mode(0o600))
        .map_err(|e| format!("chmod Wayland socket {}: {e}", path.display()))?;
    println!("TD-WAYLAND-READY socket={}", path.display());
    std::io::stdout()
        .flush()
        .map_err(|e| format!("flush Wayland ready marker: {e}"))?;
    for connection in listener.incoming() {
        let stream = connection.map_err(|e| format!("accept Wayland client: {e}"))?;
        let permit = match ClientPermit::acquire() {
            Ok(permit) => permit,
            Err(error) => {
                eprintln!("td-compositor: {error}");
                continue;
            }
        };
        let id = NEXT_CLIENT.fetch_add(1, Ordering::Relaxed);
        let runtime = Arc::clone(&runtime);
        thread::Builder::new()
            .name(format!("wayland-client-{id}"))
            .spawn(move || {
                let _permit = permit;
                if let Err(error) = serve_client(stream, id, runtime) {
                    eprintln!("td-compositor: client {id}: {error}");
                }
            })
            .map_err(|e| format!("spawn Wayland client {id}: {e}"))?;
    }
    Ok(())
}

pub fn probe(path: &Path) -> Result<(), String> {
    UnixStream::connect(path)
        .map(|_| ())
        .map_err(|e| format!("connect Wayland socket {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::framebuffer::Framebuffer;
    use crate::layout::Command;
    use std::io::Read;
    use std::os::fd::AsRawFd;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    static TEST_SEQ: AtomicU64 = AtomicU64::new(0);

    fn send(stream: &mut UnixStream, object: u32, opcode: u16, builder: wire::Builder) {
        stream
            .write_all(&builder.message(object, opcode).unwrap())
            .unwrap();
    }

    #[test]
    fn shm_commit_and_transient_remap_preserve_pixels_and_workspace() {
        let stem = format!(
            "td-wayland-test-{}-{}",
            std::process::id(),
            TEST_SEQ.fetch_add(1, Ordering::Relaxed)
        );
        let framebuffer_path = std::env::temp_dir().join(format!("{stem}.fb"));
        let pool_path = std::env::temp_dir().join(format!("{stem}.pool"));
        let framebuffer = Framebuffer::test_file(&framebuffer_path, 64, 64, 64 * 4).unwrap();
        let runtime = Arc::new(Mutex::new(Runtime::new(framebuffer)));
        runtime.lock().unwrap().repaint().unwrap();
        let (server, mut client) = UnixStream::pair().unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let thread_runtime = Arc::clone(&runtime);
        let worker = thread::spawn(move || serve_client(server, 7, thread_runtime));

        let mut get_registry = wire::Builder::new();
        get_registry.u32(2);
        send(&mut client, 1, 1, get_registry);

        let mut bind_compositor = wire::Builder::new();
        bind_compositor.u32(GLOBAL_COMPOSITOR);
        bind_compositor.string("wl_compositor").unwrap();
        bind_compositor.u32(4);
        bind_compositor.u32(3);
        send(&mut client, 2, 0, bind_compositor);

        let mut bind_shm = wire::Builder::new();
        bind_shm.u32(GLOBAL_SHM);
        bind_shm.string("wl_shm").unwrap();
        bind_shm.u32(1);
        bind_shm.u32(4);
        send(&mut client, 2, 0, bind_shm);

        let mut create_surface = wire::Builder::new();
        create_surface.u32(5);
        send(&mut client, 3, 0, create_surface);

        let mut bind_xdg = wire::Builder::new();
        bind_xdg.u32(GLOBAL_XDG_WM_BASE);
        bind_xdg.string("xdg_wm_base").unwrap();
        bind_xdg.u32(1);
        bind_xdg.u32(8);
        send(&mut client, 2, 0, bind_xdg);

        let mut get_xdg_surface = wire::Builder::new();
        get_xdg_surface.u32(9);
        get_xdg_surface.u32(5);
        send(&mut client, 8, 2, get_xdg_surface);

        let mut get_toplevel = wire::Builder::new();
        get_toplevel.u32(10);
        send(&mut client, 9, 1, get_toplevel);
        send(&mut client, 5, 6, wire::Builder::new());

        let mut received = Vec::new();
        let mut scratch = [0u8; 4096];
        let configure = loop {
            let count = client.read(&mut scratch).unwrap();
            assert!(count > 0);
            received.extend_from_slice(scratch.get(..count).unwrap());
            let mut serial = None;
            while let Some(message) = wire::take(&mut received).unwrap() {
                if message.object == 9 && message.opcode == 0 {
                    let mut args = wire::Cursor::new(&message.payload);
                    serial = Some(args.u32().unwrap());
                    args.finish().unwrap();
                }
            }
            if let Some(serial) = serial {
                break serial;
            }
        };
        let mut ack = wire::Builder::new();
        ack.u32(configure);
        send(&mut client, 9, 4, ack);

        let mut pixels = Vec::new();
        for _ in 0..16 * 16 {
            pixels.extend_from_slice(&[0x11u8, 0x22, 0x33, 0]);
        }
        fs::write(&pool_path, &pixels).unwrap();
        let pool = File::open(&pool_path).unwrap();
        let mut create_pool = wire::Builder::new();
        create_pool.u32(6);
        create_pool.i32(i32::try_from(pixels.len()).unwrap());
        let create_pool = create_pool.message(4, 0).unwrap();
        sys::send_with_fd(&client, &create_pool, pool.as_raw_fd()).unwrap();

        let mut create_buffer = wire::Builder::new();
        create_buffer.u32(7);
        create_buffer.i32(0);
        create_buffer.i32(16);
        create_buffer.i32(16);
        create_buffer.i32(16 * 4);
        create_buffer.u32(SHM_XRGB8888);
        send(&mut client, 6, 0, create_buffer);

        let mut attach = wire::Builder::new();
        attach.u32(7);
        attach.i32(0);
        attach.i32(0);
        send(&mut client, 5, 1, attach);
        send(&mut client, 5, 6, wire::Builder::new());

        let mut saw_release = false;
        while !saw_release {
            let count = client.read(&mut scratch).unwrap();
            assert!(count > 0);
            received.extend_from_slice(scratch.get(..count).unwrap());
            while let Some(message) = wire::take(&mut received).unwrap() {
                if message.object == 7 && message.opcode == 0 {
                    saw_release = true;
                }
            }
        }
        let frame = fs::read(&framebuffer_path).unwrap();
        assert!(frame.as_chunks::<4>().0.contains(&[0x11, 0x22, 0x33, 0]));

        runtime
            .lock()
            .unwrap()
            .command(Command::MoveToWorkspace(2))
            .unwrap();
        let mut detach = wire::Builder::new();
        detach.u32(0);
        detach.i32(0);
        detach.i32(0);
        send(&mut client, 5, 1, detach);
        send(&mut client, 5, 6, wire::Builder::new());

        let mut attach = wire::Builder::new();
        attach.u32(7);
        attach.i32(0);
        attach.i32(0);
        send(&mut client, 5, 1, attach);
        send(&mut client, 5, 6, wire::Builder::new());
        let mut saw_second_release = false;
        while !saw_second_release {
            let count = client.read(&mut scratch).unwrap();
            assert!(count > 0);
            received.extend_from_slice(scratch.get(..count).unwrap());
            while let Some(message) = wire::take(&mut received).unwrap() {
                if message.object == 7 && message.opcode == 0 {
                    saw_second_release = true;
                }
            }
        }
        let inactive_frame = fs::read(&framebuffer_path).unwrap();
        assert!(!inactive_frame
            .as_chunks::<4>()
            .0
            .contains(&[0x11, 0x22, 0x33, 0]));
        runtime
            .lock()
            .unwrap()
            .command(Command::SwitchWorkspace(2))
            .unwrap();
        let restored_frame = fs::read(&framebuffer_path).unwrap();
        assert!(restored_frame
            .as_chunks::<4>()
            .0
            .contains(&[0x11, 0x22, 0x33, 0]));

        drop(client);
        worker.join().unwrap().unwrap();
        fs::remove_file(framebuffer_path).unwrap();
        fs::remove_file(pool_path).unwrap();
    }

    #[test]
    fn td_ui_demo_completes_the_real_server_handshake_and_frame() {
        let stem = format!(
            "td-ui-demo-integration-{}-{}",
            std::process::id(),
            TEST_SEQ.fetch_add(1, Ordering::Relaxed)
        );
        let framebuffer_path = std::env::temp_dir().join(format!("{stem}.fb"));
        let framebuffer = Framebuffer::test_file(&framebuffer_path, 640, 400, 640 * 4).unwrap();
        let runtime = Arc::new(Mutex::new(Runtime::new(framebuffer)));
        runtime.lock().unwrap().repaint().unwrap();
        let (server, client) = UnixStream::pair().unwrap();
        let thread_runtime = Arc::clone(&runtime);
        let worker = thread::spawn(move || serve_client(server, 77, thread_runtime));

        let connected = crate::client::present_for_test(client, &std::env::temp_dir()).unwrap();
        let frame = fs::read(&framebuffer_path).unwrap();
        assert!(frame.as_chunks::<4>().0.contains(&[0x78, 0x46, 0xe8, 0]));

        drop(connected);
        worker.join().unwrap().unwrap();
        fs::remove_file(framebuffer_path).unwrap();
    }

    #[test]
    fn event_write_after_peer_departure_is_clean_disconnect() {
        let framebuffer_path = std::env::temp_dir().join(format!(
            "td-wayland-disconnect-{}-{}",
            std::process::id(),
            TEST_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let framebuffer = Framebuffer::test_file(&framebuffer_path, 4, 4, 16).unwrap();
        let runtime = Arc::new(Mutex::new(Runtime::new(framebuffer)));
        let (server, peer) = UnixStream::pair().unwrap();
        let mut client = Client::new(1, server, runtime);
        drop(peer);

        let mut first = wire::Builder::new();
        first.u32(2);
        client.send(1, 1, first).unwrap();
        assert!(client.disconnected);
        let mut second = wire::Builder::new();
        second.u32(3);
        client.send(1, 1, second).unwrap();
        assert!(client.send(0, 1, wire::Builder::new()).is_err());

        fs::remove_file(framebuffer_path).unwrap();
    }

    #[test]
    fn disconnected_client_suppresses_later_writes() {
        let framebuffer_path = std::env::temp_dir().join(format!(
            "td-wayland-suppress-write-{}-{}",
            std::process::id(),
            TEST_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let framebuffer = Framebuffer::test_file(&framebuffer_path, 4, 4, 16).unwrap();
        let runtime = Arc::new(Mutex::new(Runtime::new(framebuffer)));
        let (server, mut peer) = UnixStream::pair().unwrap();
        peer.set_nonblocking(true).unwrap();
        let mut client = Client::new(1, server, runtime);
        client.disconnected = true;
        let mut event = wire::Builder::new();
        event.u32(2);

        client.send(1, 1, event).unwrap();

        let mut received = [0u8; 16];
        let error = peer.read(&mut received).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
        fs::remove_file(framebuffer_path).unwrap();
    }

    #[test]
    fn unread_event_makes_peer_reset_a_clean_server_exit() {
        let framebuffer_path = std::env::temp_dir().join(format!(
            "td-wayland-recv-reset-{}-{}",
            std::process::id(),
            TEST_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let framebuffer = Framebuffer::test_file(&framebuffer_path, 4, 4, 16).unwrap();
        let runtime = Arc::new(Mutex::new(Runtime::new(framebuffer)));
        let (mut server, peer) = UnixStream::pair().unwrap();
        server.write_all(b"unread event").unwrap();
        drop(peer);

        serve_client(server, 1, runtime).unwrap();

        fs::remove_file(framebuffer_path).unwrap();
    }

    #[test]
    fn queued_request_stops_dispatch_when_its_reply_finds_closed_peer() {
        let framebuffer_path = std::env::temp_dir().join(format!(
            "td-wayland-dispatch-disconnect-{}-{}",
            std::process::id(),
            TEST_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let framebuffer = Framebuffer::test_file(&framebuffer_path, 4, 4, 16).unwrap();
        let runtime = Arc::new(Mutex::new(Runtime::new(framebuffer)));
        let (server, peer) = UnixStream::pair().unwrap();
        let mut client = Client::new(1, server, runtime);
        let mut get_registry = wire::Builder::new();
        get_registry.u32(2);
        let mut bytes = get_registry.message(1, 1).unwrap();
        let mut fds = VecDeque::new();
        drop(peer);

        assert_eq!(
            dispatch_buffered(&mut client, &mut bytes, &mut fds).unwrap(),
            DispatchOutcome::Disconnected
        );
        assert!(bytes.is_empty());
        assert!(client.disconnected);

        fs::remove_file(framebuffer_path).unwrap();
    }

    #[test]
    fn queued_request_with_closed_peer_makes_clean_server_exit() {
        let framebuffer_path = std::env::temp_dir().join(format!(
            "td-wayland-server-disconnect-{}-{}",
            std::process::id(),
            TEST_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let framebuffer = Framebuffer::test_file(&framebuffer_path, 4, 4, 16).unwrap();
        let runtime = Arc::new(Mutex::new(Runtime::new(framebuffer)));
        let (server, mut peer) = UnixStream::pair().unwrap();
        let mut get_registry = wire::Builder::new();
        get_registry.u32(2);
        peer.write_all(&get_registry.message(1, 1).unwrap())
            .unwrap();
        drop(peer);

        serve_client(server, 1, runtime).unwrap();

        fs::remove_file(framebuffer_path).unwrap();
    }

    #[test]
    fn pool_and_buffer_bounds_fail_closed() {
        let path = std::env::temp_dir().join(format!(
            "td-wayland-bounds-{}-{}",
            std::process::id(),
            TEST_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        fs::write(&path, vec![0u8; 64]).unwrap();
        let pool = Pool {
            file: Arc::new(File::open(&path).unwrap()),
            size: 64,
        };
        let framebuffer_path = path.with_extension("fb");
        let framebuffer = Framebuffer::test_file(&framebuffer_path, 4, 4, 16).unwrap();
        let (server, _peer) = UnixStream::pair().unwrap();
        let runtime = Arc::new(Mutex::new(Runtime::new(framebuffer)));
        let mut client = Client::new(1, server, runtime);
        assert!(client
            .create_buffer(pool.clone(), 2, 0, 4, 4, 15, SHM_XRGB8888)
            .is_err());
        assert!(client
            .create_buffer(pool, 2, 0, 4, 5, 16, SHM_XRGB8888)
            .is_err());
        fs::remove_file(path).unwrap();
        fs::remove_file(framebuffer_path).unwrap();
    }

    #[test]
    fn attached_buffer_survives_client_side_object_destruction() {
        let stem = format!(
            "td-wayland-buffer-life-{}-{}",
            std::process::id(),
            TEST_SEQ.fetch_add(1, Ordering::Relaxed)
        );
        let framebuffer_path = std::env::temp_dir().join(format!("{stem}.fb"));
        let pool_path = std::env::temp_dir().join(format!("{stem}.pool"));
        let pixels = [0x21u8, 0x43, 0x65, 0];
        fs::write(&pool_path, pixels).unwrap();
        let framebuffer = Framebuffer::test_file(&framebuffer_path, 8, 8, 32).unwrap();
        let (server, _peer) = UnixStream::pair().unwrap();
        let runtime = Arc::new(Mutex::new(Runtime::new(framebuffer)));
        let mut client = Client::new(2, server, runtime);
        client
            .insert(
                5,
                Object::Surface(SurfaceState {
                    role: Some(9),
                    ..SurfaceState::default()
                }),
            )
            .unwrap();
        client
            .insert(
                9,
                Object::XdgSurface {
                    surface: 5,
                    toplevel: Some(10),
                    configure_serial: Some(44),
                    configured: true,
                },
            )
            .unwrap();
        client
            .insert(10, Object::XdgToplevel { xdg_surface: 9 })
            .unwrap();
        client
            .insert(
                7,
                Object::Buffer(Buffer {
                    serial: 99,
                    file: Arc::new(File::open(&pool_path).unwrap()),
                    offset: 0,
                    width: 1,
                    height: 1,
                    stride: 4,
                    format: SHM_XRGB8888,
                }),
            )
            .unwrap();

        let mut attach = wire::Builder::new();
        attach.u32(7);
        attach.i32(0);
        attach.i32(0);
        client
            .dispatch(request(5, 1, attach).unwrap(), &mut VecDeque::new())
            .unwrap();
        client
            .dispatch(
                request(7, 0, wire::Builder::new()).unwrap(),
                &mut VecDeque::new(),
            )
            .unwrap();
        client
            .dispatch(
                request(5, 6, wire::Builder::new()).unwrap(),
                &mut VecDeque::new(),
            )
            .unwrap();

        let frame = fs::read(&framebuffer_path).unwrap();
        assert!(frame.as_chunks::<4>().0.contains(&pixels));

        client
            .dispatch(
                request(10, 0, wire::Builder::new()).unwrap(),
                &mut VecDeque::new(),
            )
            .unwrap();
        let frame = fs::read(&framebuffer_path).unwrap();
        assert!(!frame.as_chunks::<4>().0.contains(&pixels));
        assert!(matches!(
            client.objects.get(&9),
            Some(Object::XdgSurface {
                toplevel: None,
                configure_serial: None,
                configured: false,
                ..
            })
        ));

        let mut new_toplevel = wire::Builder::new();
        new_toplevel.u32(11);
        client
            .dispatch(request(9, 1, new_toplevel).unwrap(), &mut VecDeque::new())
            .unwrap();
        let mut geometry = wire::Builder::new();
        for value in [0, 0, 1, 1] {
            geometry.i32(value);
        }
        client
            .dispatch(request(9, 3, geometry).unwrap(), &mut VecDeque::new())
            .unwrap();
        client
            .dispatch(
                request(5, 6, wire::Builder::new()).unwrap(),
                &mut VecDeque::new(),
            )
            .unwrap();
        assert!(matches!(
            client.objects.get(&9),
            Some(Object::XdgSurface {
                toplevel: Some(11),
                configure_serial: Some(_),
                configured: false,
                ..
            })
        ));
        fs::remove_file(framebuffer_path).unwrap();
        fs::remove_file(pool_path).unwrap();
    }

    #[test]
    fn malformed_wire_input_still_removes_the_clients_scene() {
        let stem = format!(
            "td-wayland-cleanup-{}-{}",
            std::process::id(),
            TEST_SEQ.fetch_add(1, Ordering::Relaxed)
        );
        let framebuffer_path = std::env::temp_dir().join(format!("{stem}.fb"));
        let framebuffer = Framebuffer::test_file(&framebuffer_path, 8, 8, 32).unwrap();
        let runtime = Arc::new(Mutex::new(Runtime::new(framebuffer)));
        runtime
            .lock()
            .unwrap()
            .commit(
                SurfaceKey {
                    client: 77,
                    object: 5,
                },
                Surface {
                    width: 1,
                    height: 1,
                    pixels: vec![1, 2, 3, 0],
                    format: SHM_XRGB8888,
                },
            )
            .unwrap();
        let (server, mut peer) = UnixStream::pair().unwrap();
        let thread_runtime = Arc::clone(&runtime);
        let worker = thread::spawn(move || serve_client(server, 77, thread_runtime));
        let mut malformed = Vec::new();
        malformed.extend_from_slice(&1u32.to_ne_bytes());
        malformed.extend_from_slice(&(7u32 << 16).to_ne_bytes());
        peer.write_all(&malformed).unwrap();
        drop(peer);
        assert!(worker.join().unwrap().is_err());
        let frame = fs::read(&framebuffer_path).unwrap();
        assert!(!frame.as_chunks::<4>().0.contains(&[1, 2, 3, 0]));
        fs::remove_file(framebuffer_path).unwrap();
    }

    #[test]
    fn client_memory_and_connection_limits_fail_closed() {
        assert_eq!(
            client_surface_total(MAX_CLIENT_SURFACE_BYTES, 4096, 4096).unwrap(),
            MAX_CLIENT_SURFACE_BYTES
        );
        assert!(client_surface_total(MAX_CLIENT_SURFACE_BYTES, 0, 1).is_err());
        let mut permits = Vec::new();
        for _ in 0..MAX_CLIENTS {
            permits.push(ClientPermit::acquire().unwrap());
        }
        assert!(ClientPermit::acquire().is_err());
        drop(permits);
        assert_eq!(ACTIVE_CLIENTS.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn message_builder_helper_is_not_vacuous() {
        let mut builder = wire::Builder::new();
        builder.u32(2);
        let message = request(1, 1, builder).unwrap();
        assert_eq!(message.object, 1);
        assert_eq!(message.opcode, 1);
    }
}

//! Real editor process and control/file workers; the display is a wire fixture,
//! not a compositor or pixel oracle. Ordinary transport tests inspect SHM bytes.
#![forbid(unsafe_code)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

type Result<T> = std::result::Result<T, String>;
const TIMEOUT: Duration = Duration::from_secs(10);
static NEXT: AtomicU64 = AtomicU64::new(0);

#[path = "support/native_compositor.rs"]
mod native_compositor;

// Linux evdev key codes used by the owned Weston seat, never ASCII.
const KEY_ESCAPE: u32 = 1;
const KEY_LEFT_CTRL: u32 = 29;
const KEY_A: u32 = 30;
const KEY_LEFT_SHIFT: u32 = 42;
const KEY_Z: u32 = 44;
const KEY_X: u32 = 45;
const KEY_V: u32 = 47;
const KEY_B: u32 = 48;
const KEY_SLASH: u32 = 53;

struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        // Linux pathname sockets need a short path, independent of TMPDIR.
        let path = Path::new("/tmp").join(format!(
            "td-editor-process-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        Self(path)
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn words(values: &[u32]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_ne_bytes()).collect()
}
fn word(bytes: &[u8], at: usize) -> Result<u32> {
    Ok(u32::from_ne_bytes(
        bytes
            .get(at..at + 4)
            .ok_or("short display word")?
            .try_into()
            .map_err(|_| "display word")?,
    ))
}
fn remaining(deadline: Instant) -> io::Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|duration| !duration.is_zero())
        .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "fixture I/O deadline"))
}
fn read_until(stream: &mut UnixStream, mut bytes: &mut [u8], deadline: Instant) -> io::Result<()> {
    while !bytes.is_empty() {
        stream.set_read_timeout(Some(remaining(deadline)?))?;
        match stream.read(bytes) {
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => bytes = &mut bytes[n..],
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}
fn write_until(stream: &mut UnixStream, mut bytes: &[u8], deadline: Instant) -> io::Result<()> {
    while !bytes.is_empty() {
        stream.set_write_timeout(Some(remaining(deadline)?))?;
        match stream.write(bytes) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(n) => bytes = &bytes[n..],
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}
fn transport(error: io::Error) -> String {
    format!("transport: {error}")
}
fn field<'a>(state: &'a str, name: &str) -> Option<&'a str> {
    state.split('\t').find_map(|field| {
        let (key, value) = field.split_once('=')?;
        (key == name).then_some(value)
    })
}
fn quit_reply_valid(response: &Result<String>) -> bool {
    match response {
        Ok(reply) => reply == "ok\tclosed",
        Err(error) => error.starts_with("transport: "),
    }
}

#[test]
fn quit_reply_requires_closed_or_transport_loss() {
    assert!(quit_reply_valid(&Ok("ok\tclosed".into())));
    assert!(quit_reply_valid(&Err("transport: unexpected EOF".into())));
    for reply in ["ok\t", "ok\tdialog\t1", "error\tunavailable\t-"] {
        assert!(!quit_reply_valid(&Ok(reply.into())));
    }
    assert!(!quit_reply_valid(&Err("response identity".into())));
}
fn send(stream: &mut UnixStream, object: u32, opcode: u16, payload: &[u8]) -> Result<()> {
    let size = u32::try_from(payload.len() + 8).map_err(|_| "display size")?;
    if size > u16::MAX as u32 || !size.is_multiple_of(4) {
        return Err("display size".into());
    }
    let mut message = words(&[object, (size << 16) | u32::from(opcode)]);
    message.extend_from_slice(payload);
    write_until(stream, &message, Instant::now() + Duration::from_secs(2)).map_err(transport)
}
fn global(
    stream: &mut UnixStream,
    registry: u32,
    name: u32,
    interface: &str,
    version: u32,
) -> Result<()> {
    let mut body = words(&[
        name,
        u32::try_from(interface.len() + 1).map_err(|_| "interface size")?,
    ]);
    body.extend_from_slice(interface.as_bytes());
    body.push(0);
    while !body.len().is_multiple_of(4) {
        body.push(0);
    }
    body.extend(words(&[version]));
    send(stream, registry, 0, &body)
}

#[derive(Clone, Copy)]
enum Object {
    Display,
    Registry,
    Compositor,
    Shm,
    Seat,
    Pointer,
    Wm,
    Surface,
    XdgSurface,
    Toplevel,
    Pool,
    Buffer,
    Callback,
}
#[derive(Default)]
struct DisplayStats {
    commits: usize,
    callbacks: usize,
    buffers: usize,
}
impl DisplayStats {
    fn assert_rendered(&self) {
        assert!(self.commits > 0, "no buffered commit");
        assert!(self.callbacks > 0, "no frame callback");
        assert!(self.buffers > 0, "no created buffer");
    }
}
struct Display {
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<Result<DisplayStats>>>,
}
impl Display {
    fn start(path: &Path, pointer: bool) -> Self {
        let listener = UnixListener::bind(path).unwrap();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let signal = stop.clone();
        let join = std::thread::spawn(move || {
            let deadline = Instant::now() + TIMEOUT;
            let mut stream = loop {
                if signal.load(Ordering::Relaxed) {
                    return Ok(DisplayStats::default());
                }
                if Instant::now() >= deadline {
                    return Err("display accept timeout".into());
                }
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2))
                    }
                    Err(e) => return Err(e.to_string()),
                }
            };
            stream
                .set_read_timeout(Some(Duration::from_millis(20)))
                .map_err(|e| e.to_string())?;
            let lifetime = Instant::now() + Duration::from_secs(30);
            let mut objects = BTreeMap::from([(1, Object::Display)]);
            let mut creation_frontier = 2;
            let mut pending = Vec::new();
            let mut input = [0; 4096];
            let mut stats = DisplayStats::default();
            let mut configured = false;
            let mut surface = 0;
            let mut xdg = 0;
            let mut toplevel = 0;
            let mut attached = None;
            let mut current_buffer = 0;
            let mut callback = None;
            let mut pointer_id = None;
            let mut entered = false;
            while !signal.load(Ordering::Relaxed) {
                remaining(lifetime).map_err(transport)?;
                match stream.read(&mut input) {
                    Ok(0) => return Ok(stats),
                    Ok(n) => pending.extend_from_slice(&input[..n]),
                    Err(e)
                        if matches!(
                            e.kind(),
                            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                        ) =>
                    {
                        continue
                    }
                    Err(e) => return Err(e.to_string()),
                }
                if pending.len() > 128 * 1024 {
                    return Err("display pending bound".into());
                }
                while pending.len() >= 8 {
                    if signal.load(Ordering::Relaxed) {
                        return Ok(stats);
                    }
                    remaining(lifetime).map_err(transport)?;
                    let object = word(&pending, 0)?;
                    let header = word(&pending, 4)?;
                    let size = (header >> 16) as usize;
                    if size < 8 || !size.is_multiple_of(4) {
                        return Err("invalid display request".into());
                    }
                    if pending.len() < size {
                        break;
                    }
                    let payload = pending[8..size].to_vec();
                    pending.drain(..size);
                    let opcode = (header & 0xffff) as u16;
                    let kind = objects
                        .get(&object)
                        .copied()
                        .ok_or("unknown display object")?;
                    // Only the fixture's advertised/supported interfaces.
                    // Unknown requests fail below; grow both tables together.
                    let constructor = match (kind, opcode) {
                        (Object::Registry, 0) => Some(word(
                            &payload,
                            payload.len().checked_sub(4).ok_or("bind size")?,
                        )?),
                        (Object::Display, 0 | 1)
                        | (Object::Compositor, 0)
                        | (Object::Seat, 0)
                        | (Object::Wm, 2)
                        | (Object::XdgSurface, 1)
                        | (Object::Shm, 0)
                        | (Object::Pool, 0)
                        | (Object::Surface, 3) => Some(word(&payload, 0)?),
                        _ => None,
                    };
                    if let Some(id) = constructor {
                        // Match libwayland's fresh-ID frontier, including callbacks.
                        // Lower IDs may be reused after their delete_id event.
                        if id == 0 || id > creation_frontier || objects.contains_key(&id) {
                            return Err(format!(
                                "invalid new object {id}; frontier {creation_frontier}"
                            ));
                        }
                        if id == creation_frontier {
                            creation_frontier += 1;
                        }
                    }
                    match (kind, opcode) {
                        (Object::Display, 1) => {
                            let registry = word(&payload, 0)?;
                            objects.insert(registry, Object::Registry);
                            for (id, interface, version) in [
                                (1, "wl_compositor", 4),
                                (2, "wl_shm", 1),
                                (3, "xdg_wm_base", 1),
                                (4, "wl_seat", 5),
                            ] {
                                global(&mut stream, registry, id, interface, version)?;
                            }
                        }
                        (Object::Display, 0) => {
                            let sync = word(&payload, 0)?;
                            objects.insert(sync, Object::Callback);
                            send(&mut stream, sync, 0, &words(&[0]))?;
                            send(&mut stream, 1, 1, &words(&[sync]))?;
                            objects.remove(&sync);
                        }
                        (Object::Registry, 0) => {
                            let kind = match word(&payload, 0)? {
                                1 => Object::Compositor,
                                2 => Object::Shm,
                                3 => Object::Wm,
                                4 => Object::Seat,
                                _ => return Err("unexpected global bind".into()),
                            };
                            let id = constructor.ok_or("missing bind constructor")?;
                            objects.insert(id, kind);
                            if matches!(kind, Object::Shm) {
                                send(&mut stream, id, 0, &words(&[1]))?;
                            }
                            if matches!(kind, Object::Seat) {
                                send(&mut stream, id, 0, &words(&[u32::from(pointer)]))?;
                            }
                        }
                        (Object::Compositor, 0) => {
                            if surface != 0 {
                                return Err("fixture supports one main surface only".into());
                            }
                            surface = word(&payload, 0)?;
                            objects.insert(surface, Object::Surface);
                        }
                        (Object::Seat, 0) if pointer => {
                            let id = word(&payload, 0)?;
                            objects.insert(id, Object::Pointer);
                            pointer_id = Some(id);
                        }
                        (Object::Wm, 2) => {
                            xdg = word(&payload, 0)?;
                            objects.insert(xdg, Object::XdgSurface);
                        }
                        (Object::XdgSurface, 1) => {
                            toplevel = word(&payload, 0)?;
                            objects.insert(toplevel, Object::Toplevel);
                        }
                        (Object::Shm, 0) => {
                            // Plain read intentionally discards SCM_RIGHTS. This fixture
                            // checks lifecycle, while separate tests inspect mapped pixels.
                            objects.insert(word(&payload, 0)?, Object::Pool);
                        }
                        (Object::Pool, 0) => {
                            if word(&payload, 8)? != 640
                                || word(&payload, 12)? != 480
                                || word(&payload, 16)? != 640 * 4
                                || word(&payload, 20)? != 1
                            {
                                return Err(
                                    "expected 640x480 XRGB buffer with packed stride".into()
                                );
                            }
                            objects.insert(word(&payload, 0)?, Object::Buffer);
                            stats.buffers += 1;
                        }
                        (Object::Pool, 1) | (Object::Buffer, 0) => {
                            objects.remove(&object);
                            send(&mut stream, 1, 1, &words(&[object]))?;
                        }
                        (Object::Surface, 1) => {
                            attached = Some(word(&payload, 0)?);
                        }
                        (Object::Surface, 3) => {
                            if callback.is_some() {
                                return Err("fixture supports one pending frame callback".into());
                            }
                            callback = Some(word(&payload, 0)?);
                            objects.insert(word(&payload, 0)?, Object::Callback);
                        }
                        (Object::Surface, 6) if !configured => {
                            if toplevel == 0 || xdg == 0 {
                                return Err("commit before role".into());
                            }
                            configured = true;
                            send(&mut stream, toplevel, 0, &words(&[640, 480, 0]))?;
                            send(&mut stream, xdg, 0, &words(&[1]))?;
                        }
                        (Object::Surface, 6) => {
                            let newly_attached = attached.take();
                            if let Some(buffer) = newly_attached {
                                if buffer != 0
                                    && !matches!(objects.get(&buffer), Some(Object::Buffer))
                                {
                                    return Err("uncreated buffer".into());
                                }
                                current_buffer = buffer;
                            }
                            if current_buffer != 0 {
                                stats.commits += 1;
                            }
                            if let Some(id) = callback.take() {
                                send(&mut stream, id, 0, &words(&[1]))?;
                                send(&mut stream, 1, 1, &words(&[id]))?;
                                objects.remove(&id);
                                stats.callbacks += 1;
                            }
                            if let Some(buffer) = newly_attached.filter(|buffer| *buffer != 0) {
                                send(&mut stream, buffer, 0, &[])?;
                            }
                        }
                        (Object::Surface, 2 | 7 | 9)
                        | (Object::XdgSurface, 3 | 4)
                        | (Object::Toplevel, 2 | 3 | 8) => {}
                        _ => return Err(format!("unexpected display request {object}:{opcode}")),
                    }
                    if configured && !entered {
                        if let Some(id) = pointer_id {
                            send(&mut stream, id, 0, &words(&[1, surface, 0, 0]))?;
                            entered = true;
                        }
                    }
                }
            }
            Ok(stats)
        });
        Self {
            stop,
            join: Some(join),
        }
    }
    fn finish(&mut self) -> DisplayStats {
        self.stop.store(true, Ordering::Relaxed);
        self.join.take().unwrap().join().unwrap().unwrap()
    }
}
impl Drop for Display {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(join) = self.join.take() {
            if let Ok(Err(detail)) = join.join() {
                eprintln!("display fixture: {detail}");
            }
        }
    }
}

struct EditorProcess {
    child: Child,
    socket: PathBuf,
    log: PathBuf,
    next: u64,
}
impl EditorProcess {
    fn wait_field(&mut self, request: &str, name: &str, expected: &str) {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let state = self.ok(request);
            if field(&state, name) == Some(expected) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "waiting for {request} {name}={expected}: {state}"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    fn wait_keyboard(&mut self, profile: &str) -> String {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let state = self.ok("state");
            assert_eq!(field(&state, "keys"), Some(profile), "{state}");
            if field(&state, "key-ready") == Some("1") {
                return state;
            }
            assert!(
                Instant::now() < deadline,
                "keyboard readiness: {state}; {}",
                self.ok("prompt-state")
            );
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    fn wait_tab(&mut self, revision: u64, text: &str) {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let state = self.ok("state");
            let row = field(&state, "tab").unwrap();
            assert_eq!(row.split(',').next(), Some("1"), "{state}");
            let actual = row.split(',').nth(1).unwrap().parse::<u64>().unwrap();
            assert!(actual <= revision, "unexpected extra edit: {state}");
            if actual == revision {
                assert_eq!(
                    self.ok(&format!("text\t1\t{revision}\t0\t100")),
                    format!(
                        "{}\t{}",
                        text.len(),
                        td_editor::control::hex(text.as_bytes())
                    ),
                    "{state}"
                );
                return;
            }
            assert!(Instant::now() < deadline, "input delivery timeout: {state}");
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    fn start(directory: &Directory, display: &Path, file: &Path, dictionary: &Path) -> Self {
        Self::start_with_profile(directory, display, file, dictionary, "windows")
    }
    fn start_with_profile(
        directory: &Directory,
        display: &Path,
        file: &Path,
        dictionary: &Path,
        profile: &str,
    ) -> Self {
        let socket = directory.0.join("control");
        let log = directory.0.join("stderr");
        let child = Command::new(env!("CARGO_BIN_EXE_td-editor"))
            .args(["--window", "--control-socket"])
            .arg(&socket)
            .arg("--dictionary")
            .arg(dictionary)
            .arg(format!("--keys={profile}"))
            .arg("--")
            .arg(file)
            .env_clear()
            .env("WAYLAND_DISPLAY", display)
            .env("XDG_RUNTIME_DIR", &directory.0)
            .env("TMPDIR", &directory.0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(&log).unwrap())
            .spawn()
            .unwrap();
        Self {
            child,
            socket,
            log,
            next: 0,
        }
    }
    fn request(&mut self, tail: &str) -> Result<String> {
        let deadline = Instant::now() + TIMEOUT;
        let mut stream = loop {
            match UnixStream::connect(&self.socket) {
                Ok(stream) => break stream,
                Err(_) if Instant::now() < deadline => {
                    if let Some(status) = self.child.try_wait().map_err(|e| e.to_string())? {
                        return Err(format!(
                            "editor exited {status}: {}",
                            std::fs::read_to_string(&self.log).unwrap_or_default()
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(e) => return Err(e.to_string()),
            }
        };
        self.next += 1;
        let payload = format!("1\t{}\t{tail}", self.next);
        let frame = td_editor::control::frame(payload.as_bytes()).map_err(|e| e.to_string())?;
        write_until(&mut stream, &frame, deadline).map_err(transport)?;
        let mut header = [0; 4];
        read_until(&mut stream, &mut header, deadline).map_err(transport)?;
        let length = u32::from_be_bytes(header) as usize;
        if length == 0 || length > td_editor::control::MAX_FRAME {
            return Err("control response bound".into());
        }
        let mut response = vec![0; length];
        read_until(&mut stream, &mut response, deadline).map_err(transport)?;
        let response = String::from_utf8(response).map_err(|e| e.to_string())?;
        let prefix = format!("1\t{}\t", self.next);
        Ok(response
            .strip_prefix(&prefix)
            .ok_or("response identity")?
            .to_owned())
    }
    fn ok(&mut self, tail: &str) -> String {
        let response = self.request(tail);
        assert!(
            response.is_ok(),
            "{tail}: {response:?}; status: {:?}; stderr: {}",
            self.child.try_wait(),
            std::fs::read_to_string(&self.log).unwrap_or_default()
        );
        let response = response.unwrap();
        response.strip_prefix("ok\t").expect(&response).to_owned()
    }
    fn wait_job(&mut self, id: &str) -> String {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let state = self.ok("state");
            let prefix = format!("job={id},");
            let job = state
                .split('\t')
                .find(|field| field.starts_with(&prefix))
                .expect(&state);
            if !job.contains(",pending,") {
                assert!(job.ends_with(",complete,-"), "{job}");
                return job.to_owned();
            }
            assert!(Instant::now() < deadline, "job deadline: {state}");
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    fn job(&mut self, tail: &str) -> String {
        let response = self.request(tail).unwrap();
        let id = response.strip_prefix("pending\t").expect(&response);
        self.wait_job(id)
    }
    fn press(&mut self, tab: u64, revision: u64, x: i64, y: i64) {
        let deadline = Instant::now() + TIMEOUT;
        let state = loop {
            let state = self.ok("state");
            if field(&state, "pointer-ready") == Some("1") {
                break state;
            }
            assert!(Instant::now() < deadline, "pointer readiness: {state}");
            std::thread::sleep(Duration::from_millis(2));
        };
        assert_eq!(field(&state, "key-ready"), Some("0"));
        let token = field(&state, "input-generation").unwrap();
        self.ok(&format!(
            "pointer\t{tab}\t{revision}\t{token}\tpress\t{x}\t{y}\t0"
        ));
    }
    fn menu(&mut self, tab: u64, revision: u64, group: usize, row: usize) {
        let geometry =
            td_editor::render::Geometry::new(640, 480, td_editor::render::Scale::new(1).unwrap())
                .unwrap();
        let header = geometry.menu(group).unwrap();
        self.press(tab, revision, header.x + 4, 8);
        // At width 640 the 320px panel clamps x to 640 - 320. Single
        // presses activate 24px item rows; a trailing release is separate.
        self.press(
            tab,
            revision,
            header.x.min(320) + 4,
            24 + row as i64 * 24 + 12,
        );
    }
    fn answer(&mut self, tab: u64, revision: u64, kind: &str, action: &str, key_ready: bool) {
        let snapshot = self.ok("prompt-state");
        // Input replies follow synchronous prompt dispatch: polling here
        // would hide a wrong transition rather than wait for queued work.
        assert_eq!(field(&snapshot, "prompt"), Some(kind), "{snapshot}");
        assert_eq!(
            field(&snapshot, "target"),
            Some(format!("{tab},{revision}").as_str())
        );
        assert_eq!(
            field(&snapshot, "key-ready"),
            Some(if key_ready { "1" } else { "0" })
        );
        let token = field(&snapshot, "input-generation").unwrap();
        self.ok(&format!(
            "prompt-answer\t{tab}\t{revision}\t{token}\t{kind}\t{action}"
        ));
    }
    fn rendered(&mut self) {
        self.rendered_at(640, 480);
    }
    fn rendered_at(&mut self, width: u32, height: u32) {
        let state = self.ok("state");
        let generation = field(&state, "window-generation").unwrap();
        let frame = self.ok(&format!("wait-frame\t{generation}"));
        assert!(frame.ends_with(&format!(",{width},{height},1")), "{frame}");
        assert!(
            frame.split(',').next().unwrap().parse::<u64>().unwrap()
                >= generation.parse::<u64>().unwrap()
        );
    }
    fn quit(&mut self) {
        // Shutdown may lose the last transport reply, but a received
        // protocol refusal must never be mistaken for successful quit.
        let reply = self.request("quit");
        assert!(quit_reply_valid(&reply), "{reply:?}");
        assert!(
            self.exit().success(),
            "{}",
            std::fs::read_to_string(&self.log).unwrap()
        );
        assert!(!self.socket.exists());
    }
    fn exit(&mut self) -> std::process::ExitStatus {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            assert!(Instant::now() < deadline, "editor exit timeout");
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}

impl Drop for EditorProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct WestonProcess {
    child: Child,
    directory: PathBuf,
}
impl WestonProcess {
    fn assert_serving(&mut self) {
        display_roundtrip(UnixStream::connect(self.directory.join("wayland")).unwrap());
        assert!(
            self.child.try_wait().unwrap().is_none(),
            "Editor quit must not stop Weston: {}",
            self.diagnostics()
        );
    }
    fn diagnostics(&self) -> String {
        format!(
            "Weston log: {}\nWeston stderr: {}",
            std::fs::read_to_string(self.directory.join("weston.log")).unwrap_or_default(),
            std::fs::read_to_string(self.directory.join("weston-stderr")).unwrap_or_default()
        )
    }
}
impl Drop for WestonProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if std::thread::panicking() {
            eprintln!("{}", self.diagnostics());
        }
    }
}
impl WestonProcess {
    fn start(directory: &Directory) -> Self {
        let executable = PathBuf::from(
            std::env::var_os("TD_EDITOR_TEST_WESTON").expect("explicit Weston executable"),
        );
        let module = PathBuf::from(
            std::env::var_os("TD_EDITOR_TEST_WESTON_MODULE")
                .expect("explicit matching upstream Weston test-plugin"),
        );
        assert!(
            executable.is_absolute(),
            "absolute Weston executable required"
        );
        assert!(
            module.is_absolute(),
            "absolute matching Weston test module required"
        );
        let display = directory.0.join("wayland");
        let log = directory.0.join("weston.log");
        let mut module_arg = std::ffi::OsString::from("--modules=");
        module_arg.push(&module);
        let mut log_arg = std::ffi::OsString::from("--log=");
        log_arg.push(&log);
        let mut weston = WestonProcess {
            directory: directory.0.clone(),
            child: Command::new(executable)
                .env_clear()
                .env("XDG_RUNTIME_DIR", &directory.0)
                .args([
                    "--backend=headless-backend.so",
                    "--use-pixman",
                    "--shell=kiosk-shell.so",
                    "--no-config",
                    "--width=1024",
                    "--height=768",
                    "--idle-time=0",
                    "--socket=wayland",
                ])
                .arg(module_arg)
                .arg(log_arg)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(std::fs::File::create(directory.0.join("weston-stderr")).unwrap())
                .spawn()
                .unwrap(),
        };
        let deadline = Instant::now() + TIMEOUT;
        let ready = loop {
            if let Ok(stream) = UnixStream::connect(&display) {
                break stream;
            }
            assert!(
                weston.child.try_wait().unwrap().is_none(),
                "{}",
                weston.diagnostics()
            );
            assert!(
                Instant::now() < deadline,
                "Weston startup timeout: {}",
                weston.diagnostics()
            );
            std::thread::sleep(Duration::from_millis(2));
        };
        display_roundtrip(ready);
        weston
    }
}

fn display_roundtrip(mut stream: UnixStream) {
    let deadline = Instant::now() + TIMEOUT;
    send(&mut stream, 1, 0, &words(&[2])).unwrap();
    let mut reply = [0; 12];
    read_until(&mut stream, &mut reply, deadline).unwrap();
    assert_eq!(word(&reply, 0).unwrap(), 2, "display sync callback");
    assert_eq!(word(&reply, 4).unwrap(), 12 << 16, "display sync schema");
}

#[test]
#[ignore = "requires explicit Weston executable and matching upstream test-plugin; see README"]
fn disposable_weston_runs_the_production_editor_and_control_workers() {
    let directory = Directory::new();
    let display = directory.0.join("wayland");
    let mut weston = WestonProcess::start(&directory);
    let file = directory.0.join("-draft with spaces.txt");
    let dictionary = directory.0.join("dictionary");
    std::fs::write(&file, b"\xef\xbb\xbfone\r\nwrng\r\n").unwrap();
    std::fs::write(&dictionary, b"one\nwrong\n").unwrap();
    let mut editor = EditorProcess::start(&directory, &display, &file, &dictionary);
    editor.rendered_at(1024, 768);
    let state = editor.wait_keyboard("windows");
    let token = field(&state, "input-generation").unwrap();
    editor.ok(&format!("key\t1\t0\t{token}\t432d66")); // Native Ctrl+F.
    let prompt = editor.ok("prompt-state");
    assert_eq!(field(&prompt, "prompt"), Some("find-forward"));
    let token = field(&prompt, "input-generation").unwrap();
    editor.ok(&format!(
        "prompt-answer\t1\t0\t{token}\tfind-forward\tcancel"
    ));
    assert_eq!(field(&editor.ok("prompt-state"), "prompt"), Some("none"));
    editor.ok("replace\t1\t0\t77726e67\t77726f6e67");
    editor.ok("undo\t1\t1");
    assert!(editor
        .ok("text\t1\t2\t0\t100")
        .contains(&td_editor::control::hex(b"one\nwrng\n")));
    editor.ok("redo\t1\t2");
    assert!(editor
        .ok("text\t1\t3\t0\t100")
        .ends_with(&td_editor::control::hex(b"one\nwrong\n")));
    let spelling = editor.job("check-spelling\t1\t3");
    let scan = spelling.split(',').nth(4).unwrap();
    assert_eq!(
        editor.ok(&format!("spelling-results\t1\t3\t{scan}\t0\t10")),
        format!("1\t3\t{scan}\tcomplete\t0\t0\t2\t0\t0\t0\t-")
    );
    editor.rendered_at(1024, 768);
    assert!(editor.job("save\t1\t3").contains(",save,1,3,"));
    assert_eq!(
        std::fs::read(&file).unwrap(),
        b"\xef\xbb\xbfone\r\nwrong\r\n"
    );
    assert!(
        editor.child.try_wait().unwrap().is_none(),
        "Save must not exit the editor"
    );
    editor.quit();
    weston.assert_serving();
}

// The upstream Weston 10 test protocol is used only on our owned private
// compositor. No editor-private object ID or logical key command is injected.
struct WestonInput {
    stream: UnixStream,
    sync: u32,
    ticks: u32,
}
fn protocol_string(payload: &[u8], at: usize) -> Result<(&str, usize)> {
    let length = word(payload, at)? as usize;
    let start = at.checked_add(4).ok_or("test protocol string size")?;
    let end = start
        .checked_add(length)
        .ok_or("test protocol string size")?;
    let padded = end.checked_add(3).ok_or("test protocol string size")? & !3;
    let bytes = payload
        .get(start..end)
        .and_then(|bytes| bytes.strip_suffix(&[0]))
        .ok_or("test protocol string terminator/bound")?;
    if bytes.contains(&0) || padded > payload.len() {
        return Err("test protocol string bound".into());
    }
    Ok((
        std::str::from_utf8(bytes).map_err(|_| "test protocol string UTF-8")?,
        padded,
    ))
}
fn registry_global(payload: &[u8]) -> Result<(u32, &str, u32)> {
    let name = word(payload, 0)?;
    let (interface, end) = protocol_string(payload, 4)?;
    let version = word(payload, end)?;
    if name == 0 || version == 0 || end + 4 != payload.len() {
        return Err("test registry global schema".into());
    }
    Ok((name, interface, version))
}
impl WestonInput {
    fn over(stream: UnixStream) -> Self {
        // IDs 1..4 are display, registry, first sync and weston_test.
        Self {
            stream,
            sync: 5,
            ticks: 0,
        }
    }
    fn read(&mut self, deadline: Instant) -> Result<(u32, u16, Vec<u8>)> {
        let mut header = [0; 8];
        read_until(&mut self.stream, &mut header, deadline).map_err(transport)?;
        let object = word(&header, 0)?;
        let code = word(&header, 4)?;
        let size = (code >> 16) as usize;
        if size < 8 || !size.is_multiple_of(4) {
            return Err("test protocol frame size".into());
        }
        let mut payload = vec![0; size - 8];
        read_until(&mut self.stream, &mut payload, deadline).map_err(transport)?;
        if object == 1 && code as u16 == 0 {
            let object = word(&payload, 0)?;
            let error = word(&payload, 4)?;
            let (detail, end) = protocol_string(&payload, 8)?;
            if end != payload.len() {
                return Err("test display error schema".into());
            }
            return Err(format!(
                "Weston protocol error on object {object}, code {error}: {detail}"
            ));
        }
        Ok((object, code as u16, payload))
    }
    fn connect(path: &Path) -> Self {
        let mut input = Self::over(UnixStream::connect(path).unwrap());
        send(&mut input.stream, 1, 1, &words(&[2])).unwrap();
        send(&mut input.stream, 1, 0, &words(&[3])).unwrap();
        let deadline = Instant::now() + TIMEOUT;
        let mut name = None;
        let mut done = false;
        for _ in 0..128 {
            let (object, opcode, payload) = input.read(deadline).unwrap();
            if object == 3 && opcode == 0 {
                done = true;
                break;
            }
            if object == 2 && opcode == 0 {
                let (global, interface, _) =
                    registry_global(&payload).expect("valid registry global");
                if interface == "weston_test" {
                    assert!(name.is_none());
                    name = Some(global);
                }
            }
        }
        assert!(done, "registry sync missing within 128 events");
        let mut bind = words(&[name.expect("Weston test global"), 12]);
        bind.extend_from_slice(b"weston_test\0");
        bind.extend(words(&[1, 4]));
        send(&mut input.stream, 2, 0, &bind).unwrap();
        input.barrier();
        input
    }
    fn barrier(&mut self) {
        let id = self.sync;
        self.sync += 1;
        send(&mut self.stream, 1, 0, &words(&[id])).unwrap();
        let deadline = Instant::now() + TIMEOUT;
        let mut done = false;
        for _ in 0..128 {
            let (object, opcode, _) = self.read(deadline).unwrap();
            if object == id && opcode == 0 {
                done = true;
                break;
            }
        }
        assert!(done, "compositor sync missing within 128 events");
    }
    fn key(&mut self, key: u32, pressed: bool) {
        self.timed(5, key, u32::from(pressed));
    }
    fn timed(&mut self, opcode: u16, first: u32, second: u32) {
        self.ticks += 1;
        send(
            &mut self.stream,
            4,
            opcode,
            &words(&[
                0,
                1 + self.ticks / 1000,
                (self.ticks % 1000) * 1_000_000,
                first,
                second,
            ]),
        )
        .unwrap();
    }
    fn motion(&mut self, x: i32, y: i32) {
        // Unlike wl_pointer coordinates, weston_test.move_pointer uses ints.
        self.timed(1, x as u32, y as u32);
        self.barrier();
    }
    fn left_button(&mut self, pressed: bool) {
        self.timed(2, 0x110, u32::from(pressed)); // Linux BTN_LEFT.
        self.barrier();
    }
    fn click(&mut self, x: i32, y: i32) {
        self.motion(x, y);
        self.left_button(true);
        self.left_button(false);
    }
    fn chord(&mut self, modifier: Option<u32>, key: u32) {
        if let Some(modifier) = modifier {
            self.key(modifier, true);
        }
        self.key(key, true);
        self.key(key, false);
        if let Some(modifier) = modifier {
            self.key(modifier, false);
        }
        self.barrier();
    }
}

#[test]
fn weston_test_event_reader_refuses_short_unaligned_and_error_frames() {
    for header in [(1, 4u32 << 16), (2, 9u32 << 16)] {
        let (stream, mut peer) = UnixStream::pair().unwrap();
        peer.write_all(&words(&[header.0, header.1])).unwrap();
        // A removed alignment guard must fail immediately, not via timeout.
        peer.write_all(&[0]).unwrap();
        let mut input = WestonInput::over(stream);
        assert_eq!(
            input.read(Instant::now() + TIMEOUT).unwrap_err(),
            "test protocol frame size"
        );
    }
    let (stream, mut peer) = UnixStream::pair().unwrap();
    let mut error = words(&[4, 7, 5]);
    error.extend_from_slice(b"oops\0\0\0\0");
    send(&mut peer, 1, 0, &error).unwrap();
    let mut input = WestonInput::over(stream);
    assert_eq!(
        input.read(Instant::now() + TIMEOUT).unwrap_err(),
        "Weston protocol error on object 4, code 7: oops"
    );
    send(&mut peer, 3, 0, &words(&[42])).unwrap();
    assert_eq!(
        input.read(Instant::now() + TIMEOUT).unwrap(),
        (3, 0, words(&[42]))
    );
}

#[test]
fn weston_test_global_decoder_checks_strings_versions_and_payload_bounds() {
    let mut global = words(&[77, 12]);
    global.extend_from_slice(b"weston_test\0");
    global.extend(words(&[1]));
    assert_eq!(registry_global(&global).unwrap(), (77, "weston_test", 1));
    for length in 0..global.len() {
        assert!(registry_global(&global[..length]).is_err());
    }
    let mut bad = global.clone();
    bad[19] = b'x';
    assert!(registry_global(&bad).is_err());
    let mut bad = global.clone();
    bad[20..24].fill(0);
    assert_eq!(
        registry_global(&bad).unwrap_err(),
        "test registry global schema"
    );
    let mut bad = global.clone();
    bad[4..8].copy_from_slice(&u32::MAX.to_ne_bytes());
    assert!(registry_global(&bad).is_err());
    global.extend(words(&[0]));
    assert_eq!(
        registry_global(&global).unwrap_err(),
        "test registry global schema"
    );
}

fn weston_keyboard_profile(profile: &str) {
    let directory = Directory::new();
    let display = directory.0.join("wayland");
    let mut weston = WestonProcess::start(&directory);
    let file = directory.0.join("draft");
    let dictionary = directory.0.join("dictionary");
    std::fs::write(&file, b"one\n").unwrap();
    std::fs::write(&dictionary, b"one\n").unwrap();
    let mut editor =
        EditorProcess::start_with_profile(&directory, &display, &file, &dictionary, profile);
    editor.rendered_at(1024, 768);
    editor.wait_keyboard(profile);
    let mut input = WestonInput::connect(&display);
    // Linux evdev codes, not ASCII or already decoded editor chords.
    input.chord(Some(KEY_LEFT_SHIFT), KEY_A);
    editor.wait_tab(1, "Aone\n");
    input.chord(None, KEY_B); // Lowercase b after releasing Shift.
    editor.wait_tab(2, "Abone\n");
    input.chord(
        Some(KEY_LEFT_CTRL),
        if profile == "windows" {
            KEY_Z
        } else {
            KEY_SLASH
        },
    );
    editor.wait_tab(3, "Aone\n");
    editor.job("save\t1\t3");
    assert_eq!(std::fs::read(&file).unwrap(), b"Aone\n");
    editor.rendered_at(1024, 768);
    editor.quit();
    weston.assert_serving();
}

#[test]
#[ignore = "requires explicit Weston executable and matching upstream test-plugin; see README"]
fn disposable_weston_delivers_windows_keyboard_events() {
    weston_keyboard_profile("windows");
}

#[test]
#[ignore = "requires explicit Weston executable and matching upstream test-plugin; see README"]
fn disposable_weston_delivers_emacs_keyboard_events() {
    weston_keyboard_profile("emacs");
}

#[test]
fn weston_test_timed_requests_use_integer_pixels_and_nanosecond_timestamps() {
    let (client, mut peer) = UnixStream::pair().unwrap();
    let mut input = WestonInput::over(client);
    input.ticks = 998;
    std::thread::scope(|scope| {
        let oracle = scope.spawn(move || {
            for (index, expected) in [
                [4u32, (28 << 16) | 1, 0, 1, 999_000_000, 0xfffffff7, 56],
                [4, (28 << 16) | 2, 0, 2, 0, 0x110, 1],
                [4, (28 << 16) | 1, 0, 2, 1_000_000, 12, 34],
                [4, (28 << 16) | 2, 0, 2, 2_000_000, 0x110, 1],
                [4, (28 << 16) | 2, 0, 2, 3_000_000, 0x110, 0],
                [4, (28 << 16) | 5, 0, 2, 4_000_000, 48, 0],
            ]
            .into_iter()
            .enumerate()
            {
                let mut actual = [0; 28];
                read_until(&mut peer, &mut actual, Instant::now() + TIMEOUT).unwrap();
                for (bytes, value) in actual.as_chunks::<4>().0.iter().zip(expected) {
                    assert_eq!(u32::from_ne_bytes(*bytes), value);
                }
                if index < 5 {
                    let mut sync = [0; 12];
                    read_until(&mut peer, &mut sync, Instant::now() + TIMEOUT).unwrap();
                    let id = 5 + index as u32;
                    assert_eq!(sync.as_slice(), words(&[1, 12 << 16, id]));
                    send(&mut peer, id, 0, &words(&[0])).unwrap();
                }
            }
            peer.set_read_timeout(Some(TIMEOUT)).unwrap();
            assert_eq!(peer.read(&mut [0]).unwrap(), 0, "extra test requests");
        });
        // Exercise the actual helpers, including signed-coordinate wire bits,
        // button choice, click ordering and their compositor sync requests.
        input.motion(-9, 56);
        input.left_button(true);
        input.click(12, 34);
        input.key(KEY_B, false);
        drop(input);
        oracle.join().unwrap();
    });
}

#[test]
#[ignore = "requires explicit Weston executable and matching upstream test-plugin; see README"]
fn disposable_weston_delivers_pointer_selection_and_menu_events() {
    const EDIT_X: i32 = 68;
    const PANEL_TOP: i32 = 24;
    const MENU_ROW_HEIGHT: i32 = 24;
    const FIND_ROW: i32 = 8;
    let directory = Directory::new();
    let display = directory.0.join("wayland");
    let mut weston = WestonProcess::start(&directory);
    let file = directory.0.join("draft");
    let dictionary = directory.0.join("dictionary");
    std::fs::write(&file, b"one two\n").unwrap();
    std::fs::write(&dictionary, b"one\ntwo\n").unwrap();
    let mut editor = EditorProcess::start(&directory, &display, &file, &dictionary);
    editor.rendered_at(1024, 768);
    editor.wait_keyboard("windows");
    let mut input = WestonInput::connect(&display);
    // Kiosk fills the output; scale-one text starts at (8,48), 8px cells.
    // Use explicit pixel expectations, independent of the hit-test code.
    input.motion(9, 56);
    editor.wait_field("state", "pointer-ready", "1");
    input.left_button(true);
    input.motion(33, 56);
    editor.wait_field("state", "tab", "1,0,0,8,0,3,0,72,0,lf");
    input.left_button(false);
    // A later unheld motion must not extend the selected range.
    input.motion(65, 56);
    input.chord(None, KEY_B); // Native lowercase b replaces exactly "one".
    editor.wait_tab(1, "b two\n");
    input.chord(Some(KEY_LEFT_CTRL), KEY_Z); // Native Windows undo.
    editor.wait_tab(2, "one two\n");
    input.click(EDIT_X, 8); // Edit header.
    editor.wait_field("state", "modal", "0,0,0,0,1,0,0,0,0");
    input.click(
        EDIT_X,
        PANEL_TOP + FIND_ROW * MENU_ROW_HEIGHT + MENU_ROW_HEIGHT / 2,
    ); // Find, pinned to zero-based row eight.
    editor.wait_field("prompt-state", "prompt", "find-forward");
    input.chord(None, KEY_ESCAPE); // Escape cancels without changing text.
    editor.wait_field("prompt-state", "prompt", "none");
    editor.wait_tab(2, "one two\n");
    editor.job("save\t1\t2");
    assert_eq!(std::fs::read(&file).unwrap(), b"one two\n");
    editor.rendered_at(1024, 768);
    editor.quit();
    weston.assert_serving();
}

#[test]
#[ignore = "requires explicit Weston executable and matching upstream test-plugin; see README"]
fn disposable_weston_transfers_clipboard_between_editor_processes() {
    let compositor = Directory::new();
    let display = compositor.0.join("wayland");
    let mut weston = WestonProcess::start(&compositor);
    let source_dir = Directory::new();
    let source_path = source_dir.0.join("source");
    let dictionary = source_dir.0.join("dictionary");
    let text = "café e\u{301} 🦀\nsecond line\n";
    std::fs::write(&source_path, text).unwrap();
    std::fs::write(&dictionary, b"line\nsecond\n").unwrap();
    let mut source = EditorProcess::start(&source_dir, &display, &source_path, &dictionary);
    source.rendered_at(1024, 768);
    source.wait_keyboard("windows");
    let mut input = WestonInput::connect(&display);
    input.chord(Some(KEY_LEFT_CTRL), KEY_A);
    input.chord(Some(KEY_LEFT_CTRL), KEY_X);
    // The Cut edit proves the editor accepted native input. Weston 10 does
    // not fully validate the first selection owner's activation serial.
    source.wait_tab(1, "");
    input.chord(None, KEY_B);
    source.wait_tab(2, "b");
    source.job("save\t1\t2");
    assert_eq!(std::fs::read(&source_path).unwrap(), b"b");
    source.rendered_at(1024, 768);

    let destination_dir = Directory::new();
    let destination_path = destination_dir.0.join("destination");
    let destination_dictionary = destination_dir.0.join("dictionary");
    std::fs::write(&destination_path, b"").unwrap();
    std::fs::write(&destination_dictionary, b"line\nsecond\n").unwrap();
    let mut destination = EditorProcess::start(
        &destination_dir,
        &display,
        &destination_path,
        &destination_dictionary,
    );
    destination.rendered_at(1024, 768);
    destination.wait_keyboard("windows");
    source.wait_field("state", "focus", "0");
    destination.wait_tab(0, ""); // Receiving an offer must not insert text.
    input.chord(Some(KEY_LEFT_CTRL), KEY_V);
    // Clipboard must retain the pre-cut snapshot, not the edited source.
    destination.wait_tab(1, text);
    destination.job("save\t1\t1");
    assert_eq!(std::fs::read(&destination_path).unwrap(), text.as_bytes());
    source.wait_tab(2, "b");
    // Source is now occluded; its saved frame was fenced before mapping here.
    destination.rendered_at(1024, 768);
    destination.quit();
    source.quit();
    weston.assert_serving();
}

#[test]
fn production_process_roundtrips_remote_edit_jobs_frames_and_close_dialogs() {
    let directory = Directory::new();
    let display_path = directory.0.join("wayland");
    let mut display = Display::start(&display_path, false);
    let file = directory.0.join("-draft");
    let dictionary = directory.0.join("dictionary");
    std::fs::write(&file, b"\xef\xbb\xbfone\r\nwrng\r\n").unwrap();
    std::fs::write(&dictionary, b"one\nwarm\nwrong\n").unwrap();
    let mut editor = EditorProcess::start(&directory, &display_path, &file, &dictionary);
    assert_eq!(field(&editor.ok("state"), "key-ready"), Some("0"));
    control_edit_jobs_and_close_dialogs(&mut editor, &file);
    editor.rendered();
    editor.quit();
    display.finish().assert_rendered();
}

fn control_edit_jobs_and_close_dialogs(editor: &mut EditorProcess, file: &Path) {
    let spelling = editor.job("check-spelling\t1\t0");
    let scan = spelling.split(',').nth(4).unwrap();
    assert_eq!(
        editor.ok(&format!("spelling-results\t1\t0\t{scan}\t0\t10")),
        format!("1\t0\t{scan}\tcomplete\t1\t1\t2\t1\t0\t0\t4,8")
    );
    editor.ok("insert\t1\t0\t0\t0\t7761726d20");
    assert!(editor
        .request(&format!("spelling-results\t1\t1\t{scan}\t0\t10"))
        .unwrap()
        .starts_with("error\tstale-revision\t"));
    assert!(editor
        .request("insert\t1\t0\t0\t0\t78")
        .unwrap()
        .starts_with("error\tstale-revision\t"));
    editor.ok("replace\t1\t1\t77726e67\t77726f6e67");
    editor.ok("undo\t1\t2");
    assert!(editor
        .ok("text\t1\t3\t0\t100")
        .contains(&td_editor::control::hex(b"warm one\nwrng\n")));
    editor.ok("redo\t1\t3");
    assert!(editor
        .ok("text\t1\t4\t0\t100")
        .contains(&td_editor::control::hex(b"warm one\nwrong\n")));
    let spelling = editor.job("check-spelling\t1\t4");
    let scan = spelling.split(',').nth(4).unwrap();
    assert_eq!(
        editor.ok(&format!("spelling-results\t1\t4\t{scan}\t0\t10")),
        format!("1\t4\t{scan}\tcomplete\t0\t0\t3\t0\t0\t0\t-")
    );
    assert!(editor.job("save\t1\t4").contains(",save,1,4,"));
    assert_eq!(
        std::fs::read(file).unwrap(),
        b"\xef\xbb\xbfwarm one\r\nwrong\r\n"
    );
    assert_eq!(editor.ok("new"), "2");
    let unicode = "e\u{301}🦀";
    editor.ok(&format!(
        "insert\t2\t0\t0\t0\t{}",
        td_editor::control::hex(unicode.as_bytes())
    ));
    assert!(editor
        .ok("text\t2\t1\t0\t100")
        .contains(&td_editor::control::hex(unicode.as_bytes())));
    let first = editor.ok("close-tab\t2\t1");
    let first = first.strip_prefix("dialog\t").unwrap();
    editor.ok(&format!("dialog-answer\t{first}\t2\t1\tcancel"));
    let second = editor.ok("close-tab\t2\t1");
    let second = second.strip_prefix("dialog\t").unwrap();
    assert_ne!(first, second);
    assert!(editor
        .request(&format!("dialog-answer\t{first}\t2\t1\tdiscard"))
        .unwrap()
        .starts_with("error\tinvalid-argument\t"));
    editor.ok(&format!("dialog-answer\t{second}\t2\t1\tdiscard"));
    assert_eq!(field(&editor.ok("prompt-state"), "prompt"), Some("none"));
    editor.wait_field("state", "active", "1");
    editor.wait_field("state", "tab", "1,4,0,15,15,15,0,72,1,crlf");
    editor.wait_tab(4, "warm one\nwrong\n");
    assert_eq!(
        editor.ok("state").split('\t').filter(|field| field.starts_with("tab=")).count(),
        1
    );
}

#[test]
fn production_pointer_menu_and_prompt_answers_work_without_keyboard_focus() {
    let directory = Directory::new();
    let display_path = directory.0.join("wayland");
    let mut display = Display::start(&display_path, true);
    let file = directory.0.join("draft");
    let dictionary = directory.0.join("dictionary");
    std::fs::write(&file, b"one wrng\n").unwrap();
    std::fs::write(&dictionary, b"one\nwrong\n").unwrap();
    let mut editor = EditorProcess::start(&directory, &display_path, &file, &dictionary);
    control_menu_prompts_and_fill(&mut editor, &file, false, |editor, revision, group, row| {
        editor.menu(1, revision, group, row);
    });
    editor.rendered();
    editor.quit();
    display.finish().assert_rendered();
}

fn control_menu_prompts_and_fill(
    editor: &mut EditorProcess,
    file: &Path,
    key_ready: bool,
    mut menu: impl FnMut(&mut EditorProcess, u64, usize, usize),
) {
    menu(editor, 0, 1, 8); // Edit > Find.
    editor.answer(1, 0, "find-forward", "entry\t77726e67", key_ready);
    editor.answer(1, 0, "find-forward", "submit", key_ready);
    menu(editor, 0, 1, 11); // Edit > Replace; selected match seeds Find.
    editor.answer(1, 0, "replace", "next-field", key_ready);
    editor.answer(1, 0, "replace", "entry\t77726f6e67", key_ready);
    editor.answer(1, 0, "replace", "replace-one", key_ready);
    editor.answer(1, 1, "replace", "cancel", key_ready);
    assert!(editor
        .ok("text\t1\t1\t0\t100")
        .contains(&td_editor::control::hex(b"one wrong\n")));
    menu(editor, 1, 3, 1); // Help > Command.
    editor.answer(1, 1, "command", "entry\t73", key_ready);
    editor.answer(1, 1, "command", "complete", key_ready);
    editor.answer(1, 1, "command", "submit", key_ready);
    editor.answer(1, 1, "fill-column", "entry\t3830", key_ready);
    editor.answer(1, 1, "fill-column", "submit", key_ready);
    assert_eq!(field(&editor.ok("prompt-state"), "prompt"), Some("none"));
    editor.job("save\t1\t1");
    assert_eq!(std::fs::read(file).unwrap(), b"one wrong\n");
    // Exercise the stored fill setting: 80 keeps sixteen words on line one;
    // the default 72 would keep only fourteen.
    let paragraph = format!("{}word", "word ".repeat(16));
    let filled = format!("{}word\nword", "word ".repeat(15));
    editor.ok("select-range\t1\t1\t0\t10");
    editor.ok(&format!(
        "insert\t1\t1\t0\t10\t{}",
        td_editor::control::hex(paragraph.as_bytes())
    ));
    editor.ok("select-range\t1\t2\t0\t0");
    editor.ok("fill-paragraph\t1\t2\t0\t0");
    assert!(editor
        .ok("text\t1\t3\t0\t100")
        .contains(&td_editor::control::hex(filled.as_bytes())));
    editor.job("save\t1\t3");
    assert_eq!(std::fs::read(file).unwrap(), filled.as_bytes());
}

#[test]
fn production_display_loss_exits_nonzero_without_saving_dirty_text() {
    let directory = Directory::new();
    let display_path = directory.0.join("wayland");
    let mut display = Display::start(&display_path, false);
    let file = directory.0.join("draft");
    let dictionary = directory.0.join("dictionary");
    std::fs::write(&file, b"original\n").unwrap();
    std::fs::write(&dictionary, b"original\n").unwrap();
    let mut editor = EditorProcess::start(&directory, &display_path, &file, &dictionary);
    editor.ok("insert\t1\t0\t0\t0\t78");
    editor.rendered();
    display.finish().assert_rendered();
    assert_eq!(editor.exit().code(), Some(1));
    let diagnostic = std::fs::read_to_string(&editor.log).unwrap();
    assert!(
        diagnostic.contains("Wayland compositor disconnected")
            || diagnostic.contains("Wayland receive:"),
        "{diagnostic}"
    );
    assert_eq!(std::fs::read(&file).unwrap(), b"original\n");
    assert!(!editor.socket.exists());
}

#[test]
fn fixture_io_deadlines_bound_partial_and_expired_io() {
    let (mut reader, mut writer) = UnixStream::pair().unwrap();
    writer.write_all(b"x").unwrap();
    let error = read_until(
        &mut reader,
        &mut [0; 2],
        Instant::now() + Duration::from_millis(20),
    )
    .unwrap_err();
    assert!(matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    ));
    writer.write_all(b"y").unwrap();
    assert_eq!(
        read_until(&mut reader, &mut [0], Instant::now())
            .unwrap_err()
            .kind(),
        io::ErrorKind::TimedOut
    );
    assert_eq!(
        write_until(&mut writer, b"z", Instant::now())
            .unwrap_err()
            .kind(),
        io::ErrorKind::TimedOut
    );
    let mut byte = [0];
    read_until(&mut reader, &mut byte, Instant::now() + TIMEOUT).unwrap();
    assert_eq!(byte, *b"y");
}

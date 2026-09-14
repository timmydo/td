//! The window: a `td_ui::client::App` over the `Session` the replay drives,
//! so a key, a press and a request over the control socket reach the one
//! dispatcher. It paints the scene through the toolkit's raster, blits the
//! thumbnails its pool made into the grid's boxes and the flag badges over
//! them, serves the seam's vocabulary on `--control-socket` and holds
//! `wait-idle` until nothing is outstanding and the frame the compositor
//! acknowledged is the model's; `--preview` is the same frame without a
//! display. Nothing here opens a file: the thumbnail rule and its cache are
//! `main`'s, called from the pool's threads.

use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::{OsStr, OsString};
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Duration;

use td_ui::client::{run, App, Client, Handled, KeyboardEvent, Tag};
use td_ui::control::{self, Refusal};
use td_ui::control_socket::Socket;
use td_ui::control_worker::{Job, Worker, CONNECTIONS};
use td_ui::driven::{self, Input, Outcome, Payload, PointerPhase};
use td_ui::font::Font;
use td_ui::pointer::{self, Wheel};
use td_ui::raster::{Raster, Scale, Surface, MAX_FRAME_BYTES};
use td_ui::wayland::{connect, endpoint};
use td_ui::wire::Message;

use td_photo::develop;
use td_photo::image::Rgb8;
use td_photo::ui::{
    self, Action, BINDINGS, CONTROL_JOBS_PER_TURN, MAX_WAIT_MS, THUMB_CACHE_BYTES, THUMB_HEIGHT,
    THUMB_WIDTH,
};

use crate::{make_thumbnail, note, threads, Session};

type Result<T> = std::result::Result<T, String>;

fn error(value: impl std::fmt::Display) -> String {
    value.to_string()
}

/// `open [ROLL] [--control-socket PATH]`: the window on the display, on
/// `ROLL` if given, serving the seam on `PATH` if given, each at most once.
/// The socket is bound before the display is connected, as td-editor's is,
/// so a bad path fails before a window appears.
pub fn open(rest: &[OsString]) -> Result<()> {
    let mut roll: Option<PathBuf> = None;
    let mut socket: Option<PathBuf> = None;
    let mut args = rest.iter();
    while let Some(arg) = args.next() {
        if arg == "--control-socket" {
            if socket.is_some() {
                return Err("--control-socket may be given only once".to_string());
            }
            let path = args
                .next()
                .ok_or("--control-socket needs an absolute path")?;
            let bytes = path.as_bytes();
            if !Path::new(path).is_absolute() || bytes.len() > 107 || bytes.contains(&0) {
                return Err(
                    "control socket path must be absolute, without NUL and at most 107 bytes"
                        .to_string(),
                );
            }
            socket = Some(PathBuf::from(path));
        } else if roll.is_none() && !arg.as_bytes().starts_with(b"--") {
            roll = Some(PathBuf::from(arg));
        } else {
            return Err(format!("unrecognized argument {arg:?}; see --help"));
        }
    }
    let socket = socket
        .as_deref()
        .map(Socket::bind)
        .transpose()
        .map_err(error)?;
    let session = session(ui::DEFAULT_WIDTH, ui::DEFAULT_HEIGHT, roll.as_deref())?;
    let endpoint = endpoint(
        std::env::var_os("WAYLAND_SOCKET"),
        std::env::var_os("WAYLAND_DISPLAY"),
        std::env::var_os("XDG_RUNTIME_DIR"),
    )?;
    let stream = connect(endpoint)?;
    let control = socket.map(Worker::start).transpose().map_err(error)?;
    let mut window = Window::new(stream, std::env::temp_dir(), session, control)?;
    let result = run(&mut window);
    window.finish(result)
}

/// `--preview WxH [ROLL]`: the frame the window would show for `ROLL` on a
/// `W` by `H` surface once every thumbnail it wants is in, as a binary PPM
/// on stdout: the scene as the seam paints it and the thumbnails blitted as
/// the window blits them, made here on the calling thread.
pub fn preview(rest: &[OsString]) -> Result<()> {
    let [size, rest @ ..] = rest else {
        return Err("--preview needs WxH; see --help".to_string());
    };
    let roll = match rest {
        [] => None,
        [roll] => Some(PathBuf::from(roll)),
        _ => return Err("unrecognized arguments; see --help".to_string()),
    };
    let (width, height) = size
        .to_str()
        .and_then(|text| text.split_once('x'))
        .and_then(|(w, h)| Some((w.parse::<usize>().ok()?, h.parse::<usize>().ok()?)))
        .ok_or_else(|| format!("--preview {size:?} is not WxH"))?;
    let session = session(width, height, roll.as_deref())?;
    let font = td_ui::font::pinned()?;
    let surface = session.ui.surface();
    let stride = surface.width * 4;
    let bytes = stride
        .checked_mul(surface.height)
        .filter(|bytes| *bytes <= MAX_FRAME_BYTES)
        .ok_or("--preview: the frame is over the raster's ceiling")?;
    let mut pixels = vec![0u8; bytes];
    Raster::new(&mut pixels, &font, surface, stride)
        .map_err(error)?
        .paint(&session.ui.scene(), surface.bounds())
        .map_err(error)?;
    if let Some(roll) = &roll {
        let area = session.ui.layout().area;
        for (index, r#box) in session.ui.visible() {
            let Some(photo) = session.ui.photos().get(index) else {
                continue;
            };
            if let Some(image) =
                thumbnail(&roll.join(&photo.name), surface.scale.value(), threads())
            {
                ui::blit(&mut pixels, surface, stride, area, r#box, &image).map_err(error)?;
            }
        }
        // The badges again, over the thumbnails that covered them, within
        // the area as the blits were: the status band covers a badge that
        // runs under it.
        Raster::new(&mut pixels, &font, surface, stride)
            .map_err(error)?
            .paint(&session.ui.badges(), area)
            .map_err(error)?;
    }
    let rgb = td_ui::raster::rgb(&pixels, surface, stride).map_err(error)?;
    io::stdout()
        .lock()
        .write_all(&td_ui::raster::ppm(surface, &rgb))
        .map_err(error)
}

/// A session on a `width` by `height` surface, with `roll` opened.
fn session(width: usize, height: usize, roll: Option<&Path>) -> Result<Session> {
    let surface =
        Surface::new(width, height, Scale::default()).map_err(|e| format!("size: {e}"))?;
    let mut session = Session::new(surface);
    if let Some(roll) = roll {
        session
            .open(roll.as_os_str().as_bytes())
            .map_err(|e| format!("{}: {e}", roll.display()))?;
    }
    Ok(session)
}

/// One thumbnail as the window shows it: the rule `thumb --cache` applies,
/// at the box's width, so the cache is shared with the verb exactly, then
/// shrunk to the box for a shape taller than it. A thumbnail that cannot
/// be made is a note on stderr and `None`; the box keeps its placeholder.
fn thumbnail(path: &Path, scale: usize, threads: usize) -> Option<Rgb8> {
    let (width, height) = (THUMB_WIDTH * scale, THUMB_HEIGHT * scale);
    let made = make_thumbnail(path, width, true, threads).and_then(|made| {
        develop::shrink(made.image, width, height, threads)
            .map_err(|e| format!("{}: {e}", path.display()))
    });
    match made {
        Ok(image) => Some(image),
        Err(why) => {
            note(&why);
            None
        }
    }
}

// ------------------------------------------------------------------- pool

/// What a thumbnail is asked for by: the roll, the name in it and the
/// scale, so another roll's file of the same name, or the same file at
/// another scale, is another thumbnail.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct Key {
    roll: PathBuf,
    name: String,
    scale: usize,
}

impl Key {
    fn path(&self) -> PathBuf {
        self.roll.join(&self.name)
    }
}

/// What a worker made for a key, or that it could not (said on stderr).
struct Done {
    key: Key,
    image: Option<Rgb8>,
}

#[derive(Default)]
struct Queue {
    pending: VecDeque<Key>,
    /// Taken by a worker and not yet collected by the window.
    running: HashSet<Key>,
    closing: bool,
}

impl Queue {
    /// Replaces what is pending with `wanted`, less what is running, and
    /// says how many are outstanding.
    fn replace(&mut self, wanted: Vec<Key>) -> usize {
        let running = &self.running;
        self.pending.clear();
        self.pending
            .extend(wanted.into_iter().filter(|key| !running.contains(key)));
        self.pending.len() + self.running.len()
    }
}

/// The thumbnail pool: `threads()` workers over one queue, started at
/// window open and joined at close. The queue is replaced whenever the
/// wants move, so a request the model no longer wants is dropped before it
/// starts; a result stays in the running set until the window collects it,
/// so the count outstanding never reads zero with a thumbnail made and not
/// yet held. A finished thumbnail is kept whichever wants asked for it,
/// since it is the file's.
struct Pool {
    queue: Arc<(Mutex<Queue>, Condvar)>,
    done: Receiver<Done>,
    threads: Vec<JoinHandle<()>>,
}

impl Pool {
    fn start(count: usize) -> Result<Pool> {
        let queue = Arc::new((Mutex::new(Queue::default()), Condvar::new()));
        let (send, done) = mpsc::channel();
        let count = count.max(1);
        let mut threads = Vec::with_capacity(count);
        for _ in 0..count {
            let queue = Arc::clone(&queue);
            let send = send.clone();
            let thread = std::thread::Builder::new()
                .name("td-photo-thumb".to_string())
                .spawn(move || work(&queue, &send))
                .map_err(error)?;
            threads.push(thread);
        }
        Ok(Pool {
            queue,
            done,
            threads,
        })
    }

    fn lock(&self) -> Result<MutexGuard<'_, Queue>> {
        self.queue
            .0
            .lock()
            .map_err(|_| "thumbnail queue poisoned".to_string())
    }

    /// Replaces what is pending with `wanted`, less what is running, and
    /// says how many are outstanding.
    fn want(&self, wanted: Vec<Key>) -> Result<usize> {
        let outstanding = self.lock()?.replace(wanted);
        self.queue.1.notify_all();
        Ok(outstanding)
    }

    fn outstanding(&self) -> Result<usize> {
        let queue = self.lock()?;
        Ok(queue.pending.len() + queue.running.len())
    }

    /// Takes what the workers made, each leaving the running set as it is
    /// taken, under the one lock, so what `outstanding` counts next is
    /// exactly what the window does not hold.
    fn collect(&self) -> Result<Vec<Done>> {
        let mut queue = self.lock()?;
        let done: Vec<Done> = std::iter::from_fn(|| self.done.try_recv().ok()).collect();
        for item in &done {
            queue.running.remove(&item.key);
        }
        Ok(done)
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        if let Ok(mut queue) = self.queue.0.lock() {
            queue.closing = true;
            queue.pending.clear();
        }
        self.queue.1.notify_all();
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

fn work(queue: &(Mutex<Queue>, Condvar), done: &Sender<Done>) {
    loop {
        let key = {
            let Ok(mut guard) = queue.0.lock() else {
                return;
            };
            let key = loop {
                if guard.closing {
                    return;
                }
                match guard.pending.pop_front() {
                    Some(key) => break key,
                    None => {
                        guard = match queue.1.wait(guard) {
                            Ok(guard) => guard,
                            Err(_) => return,
                        };
                    }
                }
            };
            guard.running.insert(key.clone());
            key
        };
        let image = thumbnail(&key.path(), key.scale, 1);
        // The window takes the key out of the running set as it collects.
        if done.send(Done { key, image }).is_err() {
            return;
        }
    }
}

// ----------------------------------------------------------------- window

/// The window owns no Wayland objects of its own.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Object {}

impl Tag for Object {
    fn retired(self) -> bool {
        match self {}
    }
}

/// A thumbnail held for a name, or that none could be made, what it is
/// charged to the budget and the turn it was last blitted, for eviction.
struct Thumb {
    image: Option<Rgb8>,
    charge: usize,
    shown: u64,
}

/// What a held entry costs beside its pixels, for the map's slot and the
/// name: a thumbnail that could not be made is charged too, so the map is
/// bounded as the pixels are.
const THUMB_OVERHEAD: usize = 64;

fn charge(key: &Key, image: Option<&Rgb8>) -> usize {
    image
        .map_or(0, |image| image.data.len())
        .saturating_add(key.name.len())
        .saturating_add(THUMB_OVERHEAD)
}

struct Window {
    client: Client<Object>,
    font: Font,
    session: Session,
    pool: Pool,
    /// The roll and the scale the held thumbnails are for: another roll's,
    /// or the roll's at another scale, are let go when it changes.
    held: Option<(PathBuf, usize)>,
    /// The held thumbnails by name, within `held`.
    thumbs: HashMap<String, Thumb>,
    /// What the held thumbnails are charged between them, under
    /// `THUMB_CACHE_BYTES`.
    thumb_bytes: usize,
    /// The pointer's last position on the surface, in the protocol's 24.8
    /// fixed point as the toolkit decodes it.
    pointer: (i32, i32),
    wheel: Wheel,
    control: Option<Worker<Payload>>,
    /// `wait-idle` requests held, each with its deadline on the turn clock.
    waiters: VecDeque<(Job<Payload>, u64)>,
    control_error: Option<String>,
    /// The generation of the frame last submitted; none before the first.
    submitted: Option<u64>,
    /// The generation of the frame the compositor acknowledged with its
    /// frame callback: the one on screen, as far as the client can know.
    presented: Option<u64>,
    /// The generation the pool's wants were last computed for.
    wanted_at: Option<u64>,
    /// The turn clock: milliseconds since the loop began.
    clock: u64,
    /// When a `quit` answered over the socket closes the window: a grace
    /// after its reply, which the toolkit's worker would drop unwritten if
    /// the window closed on the same turn.
    closing: Option<u64>,
}

/// Turn-clock milliseconds between a socket's `quit` reply and the close:
/// enough for the largest reply the seam has, a `frame-page` of
/// `driven::PAGE_BYTES` in hex, at the pace the toolkit's worker writes.
const QUIT_GRACE_MS: u64 = 500;

/// `wait-idle` requests held at most: one of the worker's connections
/// stays free, so the next request is admitted and, if it is a wait,
/// answered `limit` at once rather than left in the listener's backlog.
const MAX_WAITERS: usize = CONNECTIONS - 1;

/// `wait-idle MS` as the window holds it: the milliseconds, when the
/// request is that verb with one well-formed field under the ceiling.
/// Anything else is answered at once, the router judging it.
fn wait_ms(bytes: &[u8]) -> Option<u64> {
    let text = std::str::from_utf8(bytes).ok()?;
    let mut fields = text.split('\t').skip(2);
    if fields.next()? != "wait-idle" {
        return None;
    }
    let ms = control::decimal(fields.next()?).ok()?;
    if fields.next().is_some() || ms > MAX_WAIT_MS {
        return None;
    }
    Some(ms)
}

/// The request's ID, which the worker validated with the envelope.
fn envelope_id(bytes: &[u8]) -> u64 {
    std::str::from_utf8(bytes)
        .ok()
        .and_then(|text| text.split('\t').nth(1))
        .and_then(|id| control::decimal(id).ok())
        .unwrap_or(0)
}

impl Window {
    fn new(
        stream: UnixStream,
        temporary: PathBuf,
        session: Session,
        control: Option<Worker<Payload>>,
    ) -> Result<Self> {
        Ok(Window {
            client: Client::new(stream, temporary)?,
            font: td_ui::font::pinned()?,
            session,
            pool: Pool::start(threads())?,
            held: None,
            thumbs: HashMap::new(),
            thumb_bytes: 0,
            pointer: (0, 0),
            wheel: Wheel::default(),
            control,
            waiters: VecDeque::new(),
            control_error: None,
            submitted: None,
            presented: None,
            wanted_at: None,
            clock: 0,
            closing: None,
        })
    }

    fn initialize(&mut self) -> Result<()> {
        self.client.set_title("td-photo")?;
        self.client.set_app_id("td-photo")?;
        self.client.commit()
    }

    /// One input through the dispatcher. A refusal (a flag on a sidecar
    /// that cannot be read, say) was said on stderr and changes nothing the
    /// file did not (a sidecar changed meanwhile is settled, as the replay
    /// settles it); `quit` closes the window.
    fn input(&mut self, input: Input<'_>) -> Outcome {
        let outcome =
            driven::Controller::input(&mut self.session, input).unwrap_or(Outcome::Ignored);
        if outcome == Outcome::Quit {
            self.client.close();
        }
        outcome
    }

    fn event(&mut self, message: Message) -> Result<()> {
        match self.client.handle(&message, self.clock)? {
            Handled::Done => Ok(()),
            // The compositor took the frame last submitted: it is on screen.
            Handled::FrameDone => {
                self.presented = self.submitted;
                Ok(())
            }
            Handled::Bound => self.initialize(),
            Handled::Configure { size, serial } => self.configure(size, serial),
            Handled::CloseRequested => {
                self.client.close();
                Ok(())
            }
            Handled::Keyboard(event) => {
                self.keyboard(event);
                Ok(())
            }
            Handled::Pointer(event) => self.pointer(event),
            Handled::GlobalRemoved { required: true, .. } => {
                Err("required Wayland global was removed".to_string())
            }
            // The clipboard is not used, and the client released what it
            // held for a seat or a device that went; a removed optional
            // global needs nothing of the window's.
            Handled::GlobalRemoved { .. }
            | Handled::Capabilities { .. }
            | Handled::SeatRemoved
            | Handled::Clipboard(_) => Ok(()),
            Handled::Unhandled => Err(format!(
                "unexpected Wayland event {}:{}",
                message.object, message.opcode
            )),
        }
    }

    /// The compositor's size, through the model: a zero axis keeps the
    /// current one, a negative one is protocol-illegal and treated the
    /// same, and a size the model refuses (past the raster's ceilings)
    /// keeps the frame at the size it has.
    fn configure(&mut self, size: Option<(i32, i32)>, serial: u32) -> Result<()> {
        if let Some((width, height)) = size {
            let current = self.session.ui.surface();
            let width = usize::try_from(width)
                .ok()
                .filter(|w| *w > 0)
                .unwrap_or(current.width);
            let height = usize::try_from(height)
                .ok()
                .filter(|h| *h > 0)
                .unwrap_or(current.height);
            let resize = Input::Resize {
                width,
                height,
                scale: 1,
            };
            if let Err(why) = driven::Controller::input(&mut self.session, resize) {
                note(&format!("configure {width}x{height} refused: {why}"));
            }
        }
        self.client.acknowledge(serial)
    }

    fn keyboard(&mut self, event: KeyboardEvent) {
        match event {
            KeyboardEvent::Key { key, stroke, .. } => {
                let outcome = self.input(Input::Key {
                    chord: &stroke.chord,
                });
                // A held arrow walks the grid; a flag, a filter, a view or
                // quit fires once.
                let repeats = driven::bound(&BINDINGS, &stroke.chord)
                    .and_then(|binding| Action::parse(binding.name))
                    .is_some_and(Action::repeats);
                if stroke.repeat && repeats && outcome != Outcome::Quit {
                    self.client.arm(key, self.clock);
                }
            }
            KeyboardEvent::Focus(focused) => {
                self.input(Input::Focus(focused));
            }
            KeyboardEvent::Keymap(Err(why)) | KeyboardEvent::Refused(why) => note(&why),
            KeyboardEvent::Keymap(Ok(())) | KeyboardEvent::Ready => {}
        }
    }

    fn pointer(&mut self, event: pointer::Event) -> Result<()> {
        use pointer::Event as P;
        match event {
            P::Enter { x, y, .. } | P::Motion(x, y) => self.pointer = (x, y),
            P::Button {
                button: 0x110,
                pressed: true,
                ..
            } => {
                // The toolkit hands the protocol's 24.8 fixed point; the
                // model takes pixels.
                let (x, y) = self.pointer;
                let pixel = |fixed: i32| u32::try_from(i64::from(fixed).div_euclid(256)).ok();
                if let (Some(x), Some(y)) = (pixel(x), pixel(y)) {
                    self.input(Input::Pointer {
                        phase: PointerPhase::Press,
                        x,
                        y,
                    });
                }
            }
            P::Leave(_) | P::Button { .. } => {}
            P::Axis(..) | P::Source(_) | P::Stop(_) | P::Discrete(..) => {
                self.wheel.update(event)?;
            }
            P::Frame => {
                let (rows, columns) = self.wheel.frame();
                let clamp = |value: isize| {
                    i32::try_from(value).unwrap_or(if value < 0 { i32::MIN } else { i32::MAX })
                };
                if rows != 0 || columns != 0 {
                    self.input(Input::Wheel {
                        rows: clamp(rows),
                        columns: clamp(columns),
                    });
                }
            }
        }
        Ok(())
    }

    fn end_turn(&mut self, now: u64, idle: bool) -> Result<()> {
        self.clock = now;
        if self.closing.is_some_and(|deadline| now >= deadline) {
            self.client.close();
            return Ok(());
        }
        if idle {
            match self.client.repeat(now) {
                Ok(Some(stroke)) => {
                    let outcome = self.input(Input::Key {
                        chord: &stroke.chord,
                    });
                    if outcome == Outcome::Ignored {
                        self.client.cancel_repeat();
                    }
                }
                Ok(None) => {}
                Err(why) => {
                    self.client.cancel_repeat();
                    note(&why);
                }
            }
        }
        // The socket is served before the wants are recomputed, so a roll
        // or a scale a request changed is what the wants, and the frame
        // drawn after this turn, are for.
        self.collect()?;
        self.control_tick(now);
        let jobs = self.want()?;
        self.session.ui.set_jobs(jobs);
        // The wait: the repeat's due time, at most a frame while work is
        // outstanding, at most a control poll while the socket is served.
        let mut wait = Duration::from_millis(self.client.wait_ms(now));
        if jobs > 0 {
            wait = wait.min(Duration::from_millis(16));
        }
        if self.control.is_some() || !self.waiters.is_empty() {
            wait = wait.min(Duration::from_millis(10));
        }
        let connection = self.client.connection();
        connection.set_wait(connection.wait().min(wait));
        Ok(())
    }

    /// Takes what the pool made. A result for a roll or a scale no longer
    /// held is dropped; a thumbnail for a photo on screen is a new
    /// generation; past the memory budget, the least recently shown that is
    /// not on screen goes first.
    fn collect(&mut self) -> Result<()> {
        let done = self.pool.collect()?;
        if done.is_empty() {
            return Ok(());
        }
        let Some((roll, scale)) = &self.held else {
            return Ok(());
        };
        let ui = &self.session.ui;
        let on_screen: HashSet<&str> = ui
            .visible()
            .into_iter()
            .filter_map(|(index, _)| ui.photos().get(index))
            .map(|photo| photo.name.as_str())
            .collect();
        let mut touched = false;
        for Done { key, image } in done {
            if key.roll != *roll || key.scale != *scale {
                continue;
            }
            touched |= on_screen.contains(key.name.as_str());
            let thumb = Thumb {
                charge: charge(&key, image.as_ref()),
                image,
                shown: self.clock,
            };
            self.thumb_bytes = self.thumb_bytes.saturating_add(thumb.charge);
            if let Some(old) = self.thumbs.insert(key.name, thumb) {
                self.thumb_bytes = self.thumb_bytes.saturating_sub(old.charge);
            }
        }
        while self.thumb_bytes > THUMB_CACHE_BYTES {
            let victim = self
                .thumbs
                .iter()
                .filter(|(name, _)| !on_screen.contains(name.as_str()))
                .min_by_key(|(_, thumb)| thumb.shown)
                .map(|(name, _)| name.clone());
            let Some(gone) = victim.and_then(|name| self.thumbs.remove(&name)) else {
                break;
            };
            self.thumb_bytes = self.thumb_bytes.saturating_sub(gone.charge);
            // What went may still be wanted off screen: the wants are
            // recomputed.
            self.wanted_at = None;
        }
        if touched {
            self.session.ui.touch();
        }
        Ok(())
    }

    /// The wants for the model as it stands, when it moved, and the count
    /// outstanding either way: the thumbnails not yet held, in the order
    /// `Controller::wanted` gives them. What is held is the open roll's at
    /// the surface's scale; when either changes the rest is let go.
    fn want(&mut self) -> Result<usize> {
        let generation = self.session.ui.generation();
        if self.wanted_at == Some(generation) {
            return self.pool.outstanding();
        }
        self.wanted_at = Some(generation);
        let ui = &self.session.ui;
        let scale = ui.surface().scale.value();
        let held = ui
            .roll()
            .map(|roll| (PathBuf::from(OsStr::from_bytes(roll)), scale));
        if self.held != held {
            self.thumbs.clear();
            self.thumb_bytes = 0;
            self.held = held;
        }
        let keys = match &self.held {
            None => Vec::new(),
            Some((roll, scale)) => ui
                .wanted()
                .into_iter()
                .filter_map(|index| ui.photos().get(index))
                .filter(|photo| !self.thumbs.contains_key(&photo.name))
                .map(|photo| Key {
                    roll: roll.clone(),
                    name: photo.name.clone(),
                    scale: *scale,
                })
                .collect(),
        };
        self.pool.want(keys)
    }

    /// Nothing outstanding, the wants computed for the model as it stands
    /// and the frame on screen the model's: what `wait-idle` waits for.
    fn is_idle(&self) -> bool {
        let generation = self.session.ui.generation();
        self.presented == Some(generation)
            && self.wanted_at == Some(generation)
            && self.session.ui.jobs() == 0
    }

    fn control_tick(&mut self, now: u64) {
        if self.client.closed() {
            return;
        }
        let closing = self.closing.is_some();
        let mut budget = CONTROL_JOBS_PER_TURN;
        // Each held wait is looked at once; a reply spends the turn's
        // budget. A window closing answers them all, so their replies leave
        // within the grace.
        for _ in 0..self.waiters.len() {
            let Some((job, deadline)) = self.waiters.pop_front() else {
                break;
            };
            if !job.is_live() {
                continue;
            }
            let idle = self.is_idle();
            if (idle || closing || now >= deadline) && budget > 0 {
                budget -= 1;
                self.answer(job, idle);
            } else {
                self.waiters.push_back((job, deadline));
            }
        }
        // A window closing admits nothing more: a reply admitted now might
        // not leave before the close.
        if closing {
            return;
        }
        // One outer turn, not every decoded Wayland event, budgets the work.
        for _ in 0..CONTROL_JOBS_PER_TURN {
            if budget == 0 {
                break;
            }
            let Some(worker) = self.control.as_ref() else {
                break;
            };
            let job = match worker.try_request() {
                Ok(Some(job)) => job,
                Ok(None) => break,
                Err(why) => {
                    self.disable_control(&why.to_string());
                    break;
                }
            };
            budget -= 1;
            let idle = self.is_idle();
            match wait_ms(job.request().bytes()) {
                // A wait with time on it is held; `wait-idle 0` asks and is
                // answered at once.
                Some(ms) if !idle && ms > 0 => {
                    if self.waiters.len() < MAX_WAITERS {
                        self.waiters.push_back((job, now.saturating_add(ms)));
                    } else {
                        self.refuse_limit(job);
                    }
                }
                _ => self.answer(job, idle),
            }
            if self.closing.is_some() {
                break;
            }
        }
    }

    /// One request through the seam's router, `wait-idle` told what to say;
    /// `quit` closes the window.
    fn answer(&mut self, job: Job<Payload>, idle: bool) {
        self.session.idle = idle;
        let session = &mut self.session;
        if let Err(why) = job.respond_with(|payload| driven::request(session, payload.bytes())) {
            note(&format!("control response refused: {why}"));
        }
        if self.session.quit && self.closing.is_none() {
            self.closing = Some(self.clock.saturating_add(QUIT_GRACE_MS));
        }
    }

    /// A wait past the `MAX_WAITERS` held: the transport's `limit`, as
    /// td-editor answers one.
    fn refuse_limit(&self, job: Job<Payload>) {
        let refusal: Refusal<ui::Error> = Refusal {
            id: envelope_id(job.request().bytes()),
            error: control::Error::Limit.into(),
        };
        if let Err(why) = job.respond(refusal.response().as_bytes()) {
            note(&format!("control response refused: {why}"));
        }
    }

    fn stop_control(&mut self) {
        self.waiters.clear();
        if let Some(worker) = self.control.take() {
            if let Err(why) = worker.close() {
                self.control_error = Some(why.to_string());
            }
        }
    }

    fn disable_control(&mut self, why: &str) {
        note(&format!("control disabled: {why}"));
        self.stop_control();
    }

    /// Closes the socket after the loop and reports a shutdown that failed
    /// beside the loop's own result, as td-editor does. The pool is joined
    /// when the window is dropped.
    fn finish(mut self, result: Result<()>) -> Result<()> {
        self.stop_control();
        match self.control_error.take() {
            Some(detail) => Err(match result {
                Ok(()) => format!("control shutdown failed: {detail}"),
                Err(previous) => format!("{previous}; control shutdown failed: {detail}"),
            }),
            None => result,
        }
    }

    /// Presents the model's generation when the frame last submitted is not
    /// it: the scene through the raster, then the thumbnails held for the
    /// photos on screen, centred in their boxes and clipped to the grid's
    /// area, then the flag badges again over them, within the area too.
    fn draw(&mut self) -> Result<()> {
        let generation = self.session.ui.generation();
        if self.submitted == Some(generation) || !self.client.can_present() {
            return Ok(());
        }
        let surface = self.session.ui.surface();
        let stride = surface.width * 4;
        let Window {
            client,
            font,
            session,
            thumbs,
            clock,
            ..
        } = self;
        let ui = &session.ui;
        let area = ui.layout().area;
        let visible = ui.visible();
        let scene = ui.scene();
        let badges = ui.badges();
        let photos = ui.photos();
        let submitted = client.present(surface.width, surface.height, &mut |pixels| {
            Raster::new(pixels, font, surface, stride)
                .map_err(error)?
                .paint(&scene, surface.bounds())
                .map_err(error)?;
            for (index, r#box) in &visible {
                let Some(thumb) = photos
                    .get(*index)
                    .and_then(|photo| thumbs.get_mut(&photo.name))
                else {
                    continue;
                };
                thumb.shown = *clock;
                if let Some(image) = &thumb.image {
                    ui::blit(pixels, surface, stride, area, *r#box, image).map_err(error)?;
                }
            }
            // Within the area as the blits were: the status band covers a
            // badge that runs under it.
            Raster::new(pixels, font, surface, stride)
                .map_err(error)?
                .paint(&badges, area)
                .map_err(error)
        })?;
        if submitted {
            self.submitted = Some(generation);
        }
        Ok(())
    }
}

impl App for Window {
    type Tag = Object;

    fn client(&mut self) -> &mut Client<Object> {
        &mut self.client
    }

    fn needs_descriptor(&self, _: &Message) -> Result<bool> {
        Ok(false)
    }

    fn descriptor_wait(&mut self) {}

    fn tick(&mut self, now: u64) -> Result<()> {
        self.clock = now;
        Ok(())
    }

    fn event(&mut self, message: Message) -> Result<()> {
        Window::event(self, message)
    }

    fn end_turn(&mut self, now: u64, idle: bool) -> Result<()> {
        Window::end_turn(self, now, idle)
    }

    fn draw(&mut self) -> Result<()> {
        Window::draw(self)
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
mod tests {
    use super::*;

    fn key(name: &str) -> Key {
        Key {
            roll: PathBuf::from("/td-photo/no-such-roll"),
            name: name.to_string(),
            scale: 1,
        }
    }

    #[test]
    fn a_wait_is_the_verb_with_one_field_under_the_ceiling() {
        assert_eq!(wait_ms(b"1\t7\twait-idle\t100"), Some(100));
        assert_eq!(wait_ms(b"1\t7\twait-idle\t0"), Some(0));
        assert_eq!(wait_ms(b"1\t7\twait-idle\t4000"), Some(MAX_WAIT_MS));
        for other in [
            &b"1\t7\twait-idle\t4001"[..],
            b"1\t7\twait-idle",
            b"1\t7\twait-idle\t1\t2",
            b"1\t7\twait-idle\tsoon",
            b"1\t7\tstate",
            b"1\t7",
            b"\xff\t7\twait-idle\t1",
        ] {
            assert_eq!(wait_ms(other), None, "{}", String::from_utf8_lossy(other));
        }
        assert_eq!(envelope_id(b"1\t42\tstate"), 42);
        assert_eq!(envelope_id(b"1\tx\tstate"), 0);
        assert_eq!(envelope_id(b"\xff"), 0);
    }

    #[test]
    fn the_queue_skips_what_runs_and_counts_both() {
        let mut queue = Queue::default();
        assert_eq!(queue.replace(vec![key("a"), key("b")]), 2);
        let taken = queue.pending.pop_front().unwrap();
        queue.running.insert(taken);
        assert_eq!(queue.replace(vec![key("a"), key("c")]), 2);
        let pending: Vec<&str> = queue.pending.iter().map(|key| key.name.as_str()).collect();
        assert_eq!(pending, ["c"]);
        assert_eq!(queue.replace(Vec::new()), 1);
        assert!(queue.pending.is_empty());
        // Another roll's file of the same name is another key.
        let other = Key {
            roll: PathBuf::from("/td-photo/another"),
            ..key("a")
        };
        assert_eq!(queue.replace(vec![other]), 2);
    }

    #[test]
    fn a_held_entry_is_charged_with_its_name_made_or_not() {
        let image = Rgb8 {
            width: 2,
            height: 1,
            data: vec![0; 6],
        };
        assert_eq!(charge(&key("a"), None), 1 + THUMB_OVERHEAD);
        assert_eq!(charge(&key("abc"), Some(&image)), 6 + 3 + THUMB_OVERHEAD);
    }

    #[test]
    fn the_pool_keeps_a_result_outstanding_until_it_is_collected() {
        let pool = Pool::start(1).unwrap();
        assert_eq!(pool.outstanding().unwrap(), 0);
        assert_eq!(pool.want(vec![key("a.NEF"), key("b.NEF")]).unwrap(), 2);
        // Made or not, nothing leaves the count until `collect` takes it.
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(pool.outstanding().unwrap(), 2);
        let mut done = Vec::new();
        for _ in 0..2000 {
            done.extend(pool.collect().unwrap());
            if done.len() == 2 {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let mut names: Vec<&str> = done.iter().map(|done| done.key.name.as_str()).collect();
        names.sort_unstable();
        assert_eq!(names, ["a.NEF", "b.NEF"]);
        // The roll is not there, so neither could be made; each was said.
        assert!(done.iter().all(|done| done.image.is_none()));
        assert_eq!(pool.outstanding().unwrap(), 0);
    }
}

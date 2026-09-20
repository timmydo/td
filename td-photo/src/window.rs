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
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};
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
use td_ui::wayland::{connect, endpoint, Endpoint};
use td_ui::wire::Message;

use td_photo::develop;
use td_photo::image::Rgb8;
use td_photo::library;
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
    // Named as what was resolved, since a bare `td-photo` lands here: the
    // path the display and runtime directory made, or the inherited
    // descriptor, never a display the endpoint did not consult. Without a
    // compositor the verbs are the way in, and the message says so.
    let endpoint = endpoint(
        std::env::var_os("WAYLAND_SOCKET"),
        std::env::var_os("WAYLAND_DISPLAY"),
        std::env::var_os("XDG_RUNTIME_DIR"),
    )
    .map_err(|why| format!("Wayland: {why}; see --help"))?;
    let name = match &endpoint {
        Endpoint::Path(path) => format!("display {}", path.display()),
        Endpoint::Inherited(fd) => format!("socket descriptor {fd}"),
    };
    let stream = connect(endpoint).map_err(|why| format!("Wayland {name}: {why}; see --help"))?;
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
    let mut roll: Option<PathBuf> = None;
    let mut develop: Option<usize> = None;
    let mut args = rest.iter();
    while let Some(arg) = args.next() {
        if arg == "--develop" {
            if develop.is_some() {
                return Err("--develop may be given only once".to_string());
            }
            // A position follows when the next argument is a decimal;
            // otherwise the cursor's photo, which is the first at open.
            let position = match args.clone().next() {
                Some(next)
                    if next.to_str().is_some_and(|text| {
                        !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit())
                    }) =>
                {
                    args.next();
                    next.to_str()
                        .and_then(|text| text.parse::<usize>().ok())
                        .ok_or("--develop POSITION is out of range")?
                }
                _ => 0,
            };
            develop = Some(position);
        } else if roll.is_none() && !arg.as_bytes().starts_with(b"--") {
            roll = Some(PathBuf::from(arg));
        } else {
            return Err(format!("unrecognized argument {arg:?}; see --help"));
        }
    }
    if develop.is_some() && roll.is_none() {
        return Err("--develop needs a ROLL to develop; see --help".to_string());
    }
    let (width, height) = size
        .to_str()
        .and_then(|text| text.split_once('x'))
        .and_then(|(w, h)| Some((w.parse::<usize>().ok()?, h.parse::<usize>().ok()?)))
        .ok_or_else(|| format!("--preview {size:?} is not WxH"))?;
    let mut session = session(width, height, roll.as_deref())?;
    if let Some(position) = develop {
        enter_develop(&mut session, position)?;
    }
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
        // The developed preview, when developing: the cursor photo blitted
        // into the develop box, the same frame the window shows there, made
        // here on the calling thread. The grid loop above is inert in
        // develop mode, where nothing is `visible`.
        if let Some((r#box, image)) = developed(&session, roll) {
            ui::blit(&mut pixels, surface, stride, area, r#box, &image).map_err(error)?;
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

/// Puts a `--preview` session into the develop view of the photo at
/// `position`, so the frame is the developed preview the window shows there.
/// Neither action writes a sidecar, so there is nothing to carry out.
fn enter_develop(session: &mut Session, position: usize) -> Result<()> {
    session
        .ui
        .action("select", &[position.to_string().as_str()])
        .map_err(|e| format!("--develop {position}: {e}"))?;
    session
        .ui
        .action("develop", &[])
        .map_err(|e| format!("--develop: {e}"))?;
    Ok(())
}

/// The developed preview for a `--preview --develop` session: the develop
/// box and the cursor photo developed to fit it, at the sidecar's exposure
/// and look, made on the calling thread; `None` when not developing or the
/// develop cannot be made, leaving the box its placeholder.
fn developed(session: &Session, roll: &Path) -> Option<(td_ui::raster::Rect, Rgb8)> {
    let r#box = session.ui.develop_box()?;
    let index = session.ui.cursor()?;
    let photo = session.ui.photos().get(index)?;
    let exposure = photo
        .sidecar
        .as_ref()
        .and_then(|sidecar| sidecar.exposure())
        .unwrap_or(0);
    let look = photo.sidecar.as_ref().and_then(|sidecar| sidecar.look());
    let crop = photo.sidecar.as_ref().and_then(|sidecar| sidecar.crop());
    let image = crate::develop_preview(
        roll,
        &photo.name,
        r#box.width as usize,
        r#box.height as usize,
        exposure,
        look,
        crop,
        threads(),
    )?;
    Some((r#box, image))
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

/// What a worker develops the preview by: the roll, the photo in it, the
/// box it fits, the sidecar's crop, exposure and look, so the same photo at
/// another box, crop, exposure or look is another develop. There is no
/// generation: two requests with the same fields yield the same pixels.
#[derive(Clone, Debug, Eq, PartialEq)]
struct Preview {
    roll: PathBuf,
    name: String,
    box_w: usize,
    box_h: usize,
    crop: Option<library::Crop>,
    exposure: i32,
    look: Option<String>,
}

/// The level a develop starts from, and the cached levels it reuses: an
/// exposure or look edit starts at `Level3` (level 2 reused), a resize at
/// `Level2` (level 1 reused), a photo whose level 0 is cached at `Level1`,
/// and a new photo at `Decode`. The window plans it from what its memo
/// holds; the worker runs from here forward.
enum Start {
    Decode,
    Level1 {
        raw: Arc<crate::RawFrame>,
    },
    Level2 {
        meta: crate::Meta,
        level1: Arc<develop::Level1>,
    },
    Level3 {
        meta: crate::Meta,
        level2: Arc<develop::Level2>,
    },
}

/// Which level a develop began at: what the memo tests read from a plan.
#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Stage {
    Decode,
    Level1,
    Level2,
    Level3,
}

#[cfg(test)]
impl Start {
    fn stage(&self) -> Stage {
        match self {
            Start::Decode => Stage::Decode,
            Start::Level1 { .. } => Stage::Level1,
            Start::Level2 { .. } => Stage::Level2,
            Start::Level3 { .. } => Stage::Level3,
        }
    }
}

/// What a develop produced, to merge into the window's memo and show: each
/// variant carries only the levels recomputed, since the window already
/// holds the earlier ones. `None` when it could not be made (said on
/// stderr), which redraws the box's placeholder.
enum Made {
    Decoded {
        raw: Arc<crate::RawFrame>,
        meta: crate::Meta,
        level1: Arc<develop::Level1>,
        long_edge: usize,
        crop: Option<library::Crop>,
        level2: Arc<develop::Level2>,
        image: Rgb8,
    },
    Level1 {
        meta: crate::Meta,
        level1: Arc<develop::Level1>,
        long_edge: usize,
        crop: Option<library::Crop>,
        level2: Arc<develop::Level2>,
        image: Rgb8,
    },
    Level2 {
        long_edge: usize,
        crop: Option<library::Crop>,
        level2: Arc<develop::Level2>,
        image: Rgb8,
    },
    Level3 {
        image: Rgb8,
    },
    None,
}

/// What a worker finished: a thumbnail for a key (`None` when it could not
/// be made, said on stderr, which leaves the box its placeholder), the
/// developed preview for a request, carrying what it made, or an export,
/// carrying the JPEG's path and the frame it decoded (for the raw cache)
/// or why it failed (said on stderr).
enum Done {
    Thumb {
        key: Key,
        image: Option<Rgb8>,
    },
    Develop {
        preview: Preview,
        made: Made,
    },
    Export {
        request: crate::ExportRequest,
        result: std::result::Result<(PathBuf, Option<crate::RawFrame>), String>,
    },
}

/// What a worker takes off the queue: an export carries the cached level 0
/// when the window held it at submission, so a photo just developed exports
/// without the codec; weakly, so the queue keeps no frame the raw cache has
/// let go, and one evicted while queued is decoded again.
enum Task {
    Thumb(Key),
    Develop(Preview, Start),
    Export(crate::ExportRequest, Option<Weak<crate::RawFrame>>),
}

#[derive(Default)]
struct Queue {
    pending: VecDeque<Key>,
    /// Thumbnails taken by a worker and not yet collected by the window.
    running: HashSet<Key>,
    /// The develop the window wants and no worker has taken yet, with the
    /// level it should start from.
    preview: Option<(Preview, Start)>,
    /// The develop a worker has taken and the window has not collected yet;
    /// at most one, so at most one raw is decoded at a time.
    developing: Option<Preview>,
    /// Exports asked for and not yet taken, in order: never dropped by a
    /// replacement of the wants, since each is a file the user asked for,
    /// and drained before the pool closes.
    exports: VecDeque<(crate::ExportRequest, Option<Weak<crate::RawFrame>>)>,
    /// The export a worker has taken and not yet sent the result of; at
    /// most one, so exports run in order and one raw decode of theirs at a
    /// time, beside the develop's. The worker leaves it, under the lock, as
    /// it sends, so a closing pool drains its exports with no window to
    /// collect them.
    exporting: Option<crate::ExportRequest>,
    closing: bool,
}

impl Queue {
    /// Replaces the thumbnail wants with `thumbs` (less what is running) and
    /// the develop want with `preview`, but never re-queues the develop a
    /// worker already holds, and says how many jobs are outstanding.
    fn replace(&mut self, thumbs: Vec<Key>, preview: Option<(Preview, Start)>) -> usize {
        let running = &self.running;
        self.pending.clear();
        self.pending
            .extend(thumbs.into_iter().filter(|key| !running.contains(key)));
        self.preview = match preview {
            // The one in flight is not queued again; it is being made.
            Some((want, _)) if self.developing.as_ref() == Some(&want) => None,
            other => other,
        };
        self.outstanding()
    }

    fn outstanding(&self) -> usize {
        self.pending.len()
            + self.running.len()
            + usize::from(self.preview.is_some())
            + usize::from(self.developing.is_some())
            + self.exports.len()
            + usize::from(self.exporting.is_some())
    }

    /// The next task for a worker, or `None` to wait: a thumbnail first,
    /// then the develop when no develop is already in flight, so the one
    /// raw decode is never run twice at once, then an export when none is
    /// in flight. A closing queue hands out exports alone, so what was
    /// asked for is written before the pool is joined.
    fn take(&mut self) -> Option<Task> {
        if !self.closing {
            if let Some(key) = self.pending.pop_front() {
                self.running.insert(key.clone());
                return Some(Task::Thumb(key));
            }
            if self.developing.is_none() {
                if let Some((preview, start)) = self.preview.take() {
                    self.developing = Some(preview.clone());
                    return Some(Task::Develop(preview, start));
                }
            }
        }
        if self.exporting.is_none() {
            if let Some((request, raw)) = self.exports.pop_front() {
                self.exporting = Some(request.clone());
                return Some(Task::Export(request, raw));
            }
        }
        None
    }

    /// Whether a closing worker may leave: nothing to export and none in
    /// flight.
    fn drained(&self) -> bool {
        self.exports.is_empty() && self.exporting.is_none()
    }

    /// A develop finished: it leaves the in-flight slot, and any queued plan
    /// is dropped too, so no worker takes a plan made against the old memo in
    /// the gap before the turn loop merges the result and replans. `want`
    /// re-submits from the fresh memo in the same turn, so nothing wanted is
    /// lost.
    fn develop_done(&mut self) {
        self.developing = None;
        self.preview = None;
    }
}

/// The pool: `threads()` workers over one queue, started at window open and
/// joined at close. The queue is replaced whenever the wants move, so a
/// request the model no longer wants is dropped before it starts; a
/// thumbnail or develop result stays outstanding (in the running set, or as
/// the develop in flight) until the window collects it, and an export until
/// the worker sends its result, so the count never reads zero with a result
/// made and not yet held or sent. Thumbnails run many at once; the develop
/// and the exports run one at a time. A finished thumbnail is kept whichever
/// wants asked for it, since it is the file's.
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
                .name("td-photo-pool".to_string())
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
            .map_err(|_| "pool queue poisoned".to_string())
    }

    /// Replaces the thumbnail wants and the develop want, and says how many
    /// jobs are outstanding.
    fn want(&self, thumbs: Vec<Key>, preview: Option<(Preview, Start)>) -> Result<usize> {
        let outstanding = self.lock()?.replace(thumbs, preview);
        self.queue.1.notify_all();
        Ok(outstanding)
    }

    fn outstanding(&self) -> Result<usize> {
        Ok(self.lock()?.outstanding())
    }

    /// Queues an export behind those already asked for, with the cached
    /// level 0 when the window holds it, and says how many jobs are
    /// outstanding.
    fn export(
        &self,
        request: crate::ExportRequest,
        raw: Option<Weak<crate::RawFrame>>,
    ) -> Result<usize> {
        let outstanding = {
            let mut queue = self.lock()?;
            queue.exports.push_back((request, raw));
            queue.outstanding()
        };
        self.queue.1.notify_all();
        Ok(outstanding)
    }

    /// Takes what the workers made, each leaving the running set or the
    /// develop-in-flight slot as it is taken, under the one lock, so what
    /// `outstanding` counts next is exactly what the window does not hold.
    /// An export left its slot as the worker sent it, under the same lock.
    fn collect(&self) -> Result<Vec<Done>> {
        let mut queue = self.lock()?;
        let done: Vec<Done> = std::iter::from_fn(|| self.done.try_recv().ok()).collect();
        for item in &done {
            match item {
                Done::Thumb { key, .. } => {
                    queue.running.remove(key);
                }
                Done::Develop { .. } => queue.develop_done(),
                Done::Export { .. } => {}
            }
        }
        Ok(done)
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        if let Ok(mut queue) = self.queue.0.lock() {
            queue.closing = true;
            queue.pending.clear();
            queue.preview = None;
        }
        self.queue.1.notify_all();
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

fn work(queue: &(Mutex<Queue>, Condvar), done: &Sender<Done>) {
    loop {
        let task = {
            let Ok(mut guard) = queue.0.lock() else {
                return;
            };
            loop {
                // A closing queue is left once its exports are drained; the
                // thumbnails and the develop it dropped are not waited for.
                if guard.closing && guard.drained() {
                    return;
                }
                match guard.take() {
                    Some(task) => break task,
                    None => {
                        guard = match queue.1.wait(guard) {
                            Ok(guard) => guard,
                            Err(_) => return,
                        };
                    }
                }
            }
        };
        // The window takes the key or the develop out of the count as it
        // collects the result.
        let result = match task {
            Task::Thumb(key) => {
                let image = thumbnail(&key.path(), key.scale, 1);
                Done::Thumb { key, image }
            }
            Task::Develop(preview, start) => {
                let made = develop_task(&preview, start);
                Done::Develop { preview, made }
            }
            Task::Export(request, raw) => {
                // A frame the raw cache let go while this waited is decoded
                // again; the export is what was asked for either way.
                let raw = raw.as_ref().and_then(Weak::upgrade);
                let result = crate::export_file(&request, raw.as_deref(), threads())
                    .map(|exported| (exported.out, exported.raw));
                // Why it failed is noted here, so an export that fails after
                // the window closed is reported too.
                if let Err(why) = &result {
                    note(why);
                }
                Done::Export { request, result }
            }
        };
        if matches!(result, Done::Export { .. }) {
            // Sent and the slot left under the one lock: the window never
            // counts an export it holds the result of, nor misses one whose
            // result is not yet sent. The worker leaves the slot, not the
            // window's collect, so a closing pool drains its exports with no
            // window to collect them, and the waiting workers are woken to
            // take the next.
            let Ok(mut guard) = queue.0.lock() else {
                return;
            };
            let sent = done.send(result).is_ok();
            guard.exporting = None;
            drop(guard);
            queue.1.notify_all();
            if !sent {
                return;
            }
        } else if done.send(result).is_err() {
            return;
        }
    }
}

/// Runs a develop from `start` forward on a pool worker, off the turn
/// thread: the look stem resolved here, then the levels the start does not
/// already carry, each cheaper than the last. `Made::None`, with a note,
/// when any level cannot be made, so the box redraws its placeholder.
fn develop_task(preview: &Preview, start: Start) -> Made {
    match develop_try(preview, start) {
        Ok(made) => made,
        Err(why) => {
            note(&why);
            Made::None
        }
    }
}

fn develop_try(preview: &Preview, start: Start) -> Result<Made> {
    let threads = threads();
    let long_edge = preview.box_w.max(preview.box_h);
    let crop = preview.crop.map(crate::crop_fractions);
    let stops = preview.exposure as f32 / 100.0;
    let look = match &preview.look {
        Some(stem) => Some(crate::find_look(stem)?),
        None => None,
    };
    // Level 2 to the shown frame, at this box, exposure and look.
    let frame = |level2: &develop::Level2, meta: &crate::Meta| {
        crate::level2_frame(
            level2,
            meta,
            preview.box_w,
            preview.box_h,
            stops,
            look.as_ref(),
            threads,
        )
    };
    // Level 1 to its level 2 and frame, shared by the decode and cached-raw
    // starts.
    let from_level1 = |level1: &develop::Level1, meta: &crate::Meta| {
        let level2 = crate::level1_level2(level1, meta, crop, long_edge, threads)?;
        let image = frame(&level2, meta)?;
        Ok::<_, String>((level2, image))
    };
    Ok(match start {
        Start::Level3 { meta, level2 } => Made::Level3 {
            image: frame(&level2, &meta)?,
        },
        Start::Level2 { meta, level1 } => {
            let (level2, image) = from_level1(&level1, &meta)?;
            Made::Level2 {
                long_edge,
                crop: preview.crop,
                level2: Arc::new(level2),
                image,
            }
        }
        Start::Level1 { raw } => {
            let meta = raw.meta();
            let level1 = crate::raw_level1(&raw, threads)?;
            let (level2, image) = from_level1(&level1, &meta)?;
            Made::Level1 {
                meta,
                level1: Arc::new(level1),
                long_edge,
                crop: preview.crop,
                level2: Arc::new(level2),
                image,
            }
        }
        Start::Decode => {
            let (raw, _info) = crate::decode_raw(&preview.roll.join(&preview.name))?;
            let meta = raw.meta();
            let level1 = crate::raw_level1(&raw, threads)?;
            let (level2, image) = from_level1(&level1, &meta)?;
            Made::Decoded {
                raw: Arc::new(raw),
                meta,
                level1: Arc::new(level1),
                long_edge,
                crop: preview.crop,
                level2: Arc::new(level2),
                image,
            }
        }
    })
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

/// A photo the memo and the raw cache key by: the roll and the name in it,
/// so another roll's file of the same name is another photo. Scale-free,
/// since level 0 and level 1 do not depend on the scale.
#[derive(Clone, Eq, PartialEq)]
struct PhotoKey {
    roll: PathBuf,
    name: String,
}

impl PhotoKey {
    /// Whether this is the photo the preview develops.
    fn is(&self, preview: &Preview) -> bool {
        self.roll == preview.roll && self.name == preview.name
    }
}

/// The photo an export is of, keyed as the memo keys it: the request's
/// path is the roll's directory and the name in it.
fn export_key(request: &crate::ExportRequest) -> Option<PhotoKey> {
    Some(PhotoKey {
        roll: request.path.parent()?.to_path_buf(),
        name: request.path.file_name()?.to_str()?.to_string(),
    })
}

/// The current photo's levels above 0: level 1 (the demosaic, reused across
/// resizes) and the level 2 for the box's current long edge and crop (reused
/// across exposure and look edits), with the metadata level 3 applies. Let go
/// with the thumbnails when the roll or scale changes.
struct Current {
    key: PhotoKey,
    meta: crate::Meta,
    level1: Arc<develop::Level1>,
    long_edge: usize,
    crop: Option<library::Crop>,
    level2: Arc<develop::Level2>,
}

/// A level-0 frame the raw cache holds, with its byte charge and the turn
/// clock when it was last the current develop; eviction picks the smallest
/// `shown`, so the entries' order in the deque carries no meaning.
struct Cached {
    key: PhotoKey,
    frame: Arc<crate::RawFrame>,
    bytes: usize,
    shown: u64,
}

/// What a raw-cache entry costs beside its samples and key: the slot alone,
/// so the cache is bounded as the frames are.
const RAW_OVERHEAD: usize = 128;

/// The develop memo the window holds for the open roll: the current photo's
/// levels above 0 and the level-0 raw cache. It plans each develop from what
/// it holds, so only the levels an edit invalidates rerun, and merges each
/// result back. Held apart from the window so its planning and eviction are
/// tested on their own.
#[derive(Default)]
struct Memo {
    /// The current photo's level 1 and its level 2 for the box's current
    /// long edge, with the metadata level 3 applies.
    current: Option<Current>,
    /// Level-0 frames held in memory under `develop::RAW_CACHE_BYTES`,
    /// evicted least-recently-shown (by each entry's `shown`, not position).
    raw: VecDeque<Cached>,
    /// What the raw cache holds between its entries, for eviction.
    raw_bytes: usize,
}

impl Memo {
    /// Lets go of every level, as when the roll or scale changes.
    fn clear(&mut self) {
        self.current = None;
        self.raw.clear();
        self.raw_bytes = 0;
    }

    /// The level a develop for `preview` starts from, given what is held:
    /// level 3 when its level 2 is current for the box and crop, level 2 when
    /// only level 1 is (a resize or crop edit), level 1 when the level 0 is
    /// cached, else a decode. Each start carries the cached levels the worker
    /// reuses.
    fn plan(&self, preview: &Preview) -> Start {
        let long_edge = preview.box_w.max(preview.box_h);
        if let Some(current) = &self.current {
            if current.key.is(preview) {
                if current.long_edge == long_edge && current.crop == preview.crop {
                    return Start::Level3 {
                        meta: current.meta,
                        level2: Arc::clone(&current.level2),
                    };
                }
                return Start::Level2 {
                    meta: current.meta,
                    level1: Arc::clone(&current.level1),
                };
            }
        }
        if let Some(cached) = self.raw.iter().find(|cached| cached.key.is(preview)) {
            return Start::Level1 {
                raw: Arc::clone(&cached.frame),
            };
        }
        Start::Decode
    }

    /// Merges a develop's result and returns the frame to show. Only the
    /// levels the develop recomputed are stored; the earlier ones are already
    /// held. `on_photo` says whether the result's photo is still the one the
    /// model wants now: a `Decoded` or `Level1` result becomes the current
    /// levels only then, so a develop that finished after a switch does not
    /// evict the photo the model moved to (its level 0 is cached either way).
    /// A `Made::Level2` for a photo no longer current is stale, so it is
    /// dropped and returns no frame; `Made::None` leaves the memo untouched.
    /// `shown` is the turn clock, for the raw cache's eviction order.
    fn merge(&mut self, preview: &Preview, made: Made, shown: u64, on_photo: bool) -> Option<Rgb8> {
        let key = PhotoKey {
            roll: preview.roll.clone(),
            name: preview.name.clone(),
        };
        match made {
            Made::Decoded {
                raw,
                meta,
                level1,
                long_edge,
                crop,
                level2,
                image,
            } => {
                self.cache_raw(key.clone(), raw, shown);
                if on_photo {
                    self.current = Some(Current {
                        key,
                        meta,
                        level1,
                        long_edge,
                        crop,
                        level2,
                    });
                }
                Some(image)
            }
            Made::Level1 {
                meta,
                level1,
                long_edge,
                crop,
                level2,
                image,
            } => {
                // The level 0 it ran from is already cached; keep it recent.
                self.touch_raw(&key, shown);
                if on_photo {
                    self.current = Some(Current {
                        key,
                        meta,
                        level1,
                        long_edge,
                        crop,
                        level2,
                    });
                }
                Some(image)
            }
            Made::Level2 {
                long_edge,
                crop,
                level2,
                image,
            } => match self.current.as_mut() {
                Some(current) if current.key == key => {
                    current.long_edge = long_edge;
                    current.crop = crop;
                    current.level2 = level2;
                    Some(image)
                }
                // The photo is no longer current: the level 2 is stale, so it
                // is dropped, neither memoized nor shown.
                _ => None,
            },
            Made::Level3 { image } => Some(image),
            Made::None => None,
        }
    }

    /// Adds a level-0 frame, replacing any entry for the same photo, and
    /// evicts the least recently shown while over `develop::RAW_CACHE_BYTES`,
    /// keeping at least the one just added. The charge counts the samples and
    /// the key's own heap (the roll path and the name), as the thumbnail
    /// charge does, so the byte budget bounds the whole cache.
    fn cache_raw(&mut self, key: PhotoKey, frame: Arc<crate::RawFrame>, shown: u64) {
        let bytes = frame
            .bytes()
            .saturating_add(key.roll.as_os_str().len())
            .saturating_add(key.name.len())
            .saturating_add(RAW_OVERHEAD);
        self.drop_raw(&key);
        self.raw.push_back(Cached {
            key,
            frame,
            bytes,
            shown,
        });
        self.raw_bytes = self.raw_bytes.saturating_add(bytes);
        while self.raw_bytes > develop::RAW_CACHE_BYTES && self.raw.len() > 1 {
            let victim = self
                .raw
                .iter()
                .enumerate()
                .min_by_key(|(_, cached)| cached.shown)
                .map(|(index, _)| index);
            let Some(gone) = victim.and_then(|index| self.raw.remove(index)) else {
                break;
            };
            self.raw_bytes = self.raw_bytes.saturating_sub(gone.bytes);
        }
    }

    /// The cached level 0 of a photo, if held: what an export starts from.
    fn raw_frame(&self, key: &PhotoKey) -> Option<Arc<crate::RawFrame>> {
        self.raw
            .iter()
            .find(|cached| cached.key == *key)
            .map(|cached| Arc::clone(&cached.frame))
    }

    /// Drops the raw-cache entry for a photo, if any.
    fn drop_raw(&mut self, key: &PhotoKey) {
        if let Some(index) = self.raw.iter().position(|cached| cached.key == *key) {
            if let Some(gone) = self.raw.remove(index) {
                self.raw_bytes = self.raw_bytes.saturating_sub(gone.bytes);
            }
        }
    }

    /// Marks a raw-cache entry the most recently shown, so a photo returned
    /// to is not the first evicted.
    fn touch_raw(&mut self, key: &PhotoKey, shown: u64) {
        if let Some(cached) = self.raw.iter_mut().find(|cached| cached.key == *key) {
            cached.shown = shown;
        }
    }
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
    /// The last developed preview and the request it was for (with no image
    /// when it could not be made), shown in the develop box until the next
    /// one lands. Let go with the thumbnails when the roll or scale changes.
    developed: Option<(Preview, Option<Rgb8>)>,
    /// The develop memo: the current photo's cached levels above 0 (so an
    /// exposure or look edit reruns level 3 alone and a resize reruns level
    /// 2) and the level-0 raw cache (so returning to a photo reruns level 1
    /// rather than the codec). Let go with the thumbnails when the roll or
    /// scale changes.
    memo: Memo,
    /// The pointer's last position on the surface, in the protocol's 24.8
    /// fixed point as the toolkit decodes it.
    pointer: (i32, i32),
    /// Whether the left button is down: a motion while it is drives a crop
    /// drag's move, and its release the drag's release.
    pressed: bool,
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
        mut session: Session,
        control: Option<Worker<Payload>>,
    ) -> Result<Self> {
        // The session queues exports for the pool rather than running them
        // on the turn thread; `submit_exports` drains it each turn.
        session.exports = Some(Vec::new());
        Ok(Window {
            client: Client::new(stream, temporary)?,
            font: td_ui::font::pinned()?,
            session,
            pool: Pool::start(threads())?,
            held: None,
            thumbs: HashMap::new(),
            thumb_bytes: 0,
            developed: None,
            memo: Memo::default(),
            pointer: (0, 0),
            pressed: false,
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
            // The pointer went with the seat or its capability: end any drag it
            // was running so it does not resume on the next device.
            Handled::SeatRemoved | Handled::Capabilities { pointer: false, .. } => {
                self.abort_pointer_grab();
                Ok(())
            }
            // The clipboard is not used, and the client released what it
            // held for a seat or a device that went; a removed optional
            // global, or a capability change that keeps the pointer, needs
            // nothing of the window's.
            Handled::GlobalRemoved { .. }
            | Handled::Capabilities { .. }
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
                // quit fires once. The chooser, when open, has its own rule.
                let repeats = if self.session.ui.chooser().is_some() {
                    self.session.ui.chooser_repeats(&stroke.chord)
                } else {
                    driven::bound(&BINDINGS, &stroke.chord)
                        .and_then(|binding| Action::parse(binding.name))
                        .is_some_and(Action::repeats)
                };
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
        // The toolkit hands the protocol's 24.8 fixed point; the model takes
        // pixels.
        let pixel = |fixed: i32| i64::from(fixed).div_euclid(256);
        // A move or release under the implicit button grab a drag runs under
        // may leave the surface, reading negative above or left of it; it
        // clamps to zero, which the controller clamps into the canvas, so a
        // drag past the top-left edge tracks the edge and the release still
        // reaches the controller (which clears the drag) rather than being
        // dropped.
        let send_clamped = |this: &mut Self, phase| {
            let (x, y) = this.pointer;
            this.input(Input::Pointer {
                phase,
                x: u32::try_from(pixel(x)).unwrap_or(0),
                y: u32::try_from(pixel(y)).unwrap_or(0),
            });
        };
        match event {
            P::Enter { x, y, .. } => self.pointer = (x, y),
            P::Motion(x, y) => {
                self.pointer = (x, y);
                // A motion while the button is down carries a drag along.
                if self.pressed {
                    send_clamped(self, PointerPhase::Move);
                }
            }
            P::Button {
                button: 0x110,
                pressed: true,
                ..
            } => {
                self.pressed = true;
                // A press is a real on-surface location: one off the top-left
                // (a left press while another button holds a cross-button grab)
                // is rejected, not clamped, so it cannot land a spurious hit at
                // a clamped zero (a strip's button, a cell).
                let (x, y) = self.pointer;
                if let (Ok(x), Ok(y)) = (u32::try_from(pixel(x)), u32::try_from(pixel(y))) {
                    self.input(Input::Pointer {
                        phase: PointerPhase::Press,
                        x,
                        y,
                    });
                }
            }
            P::Button {
                button: 0x110,
                pressed: false,
                ..
            } => {
                if self.pressed {
                    self.pressed = false;
                    send_clamped(self, PointerPhase::Release);
                }
            }
            P::Leave(_) => self.abort_pointer_grab(),
            P::Button { .. } => {}
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

    /// Ends an in-progress drag when the pointer goes away without a button
    /// release -- a grab the compositor revoked (leave), the pointer capability
    /// withdrawn, or the seat removed -- by releasing at the last point, so the
    /// controller clears the drag rather than leaving it armed to rubber-band on
    /// the next hover. A no-op when no button is held.
    fn abort_pointer_grab(&mut self) {
        if !self.pressed {
            return;
        }
        self.pressed = false;
        let (x, y) = self.pointer;
        let pixel = |fixed: i32| i64::from(fixed).div_euclid(256);
        self.input(Input::Pointer {
            phase: PointerPhase::Release,
            x: u32::try_from(pixel(x)).unwrap_or(0),
            y: u32::try_from(pixel(y)).unwrap_or(0),
        });
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
                    // A repeat the chooser no longer wants (Backspace once
                    // the filter it was editing is empty) stops here, or a
                    // held key would run up the tree.
                    let ui = &self.session.ui;
                    if ui.chooser().is_some() && !ui.chooser_repeats(&stroke.chord) {
                        self.client.cancel_repeat();
                    } else {
                        let outcome = self.input(Input::Key {
                            chord: &stroke.chord,
                        });
                        if outcome == Outcome::Ignored {
                            self.client.cancel_repeat();
                        }
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
        // drawn after this turn, are for; the exports a request or a key
        // asked for go to the pool in the same turn.
        self.collect()?;
        self.report_preview_fit();
        self.control_tick(now);
        self.submit_exports()?;
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

    /// Hands the exports the session queued this turn to the pool, each with
    /// the photo's cached level 0 when the memo holds it, so a photo just
    /// developed exports without the codec.
    fn submit_exports(&mut self) -> Result<()> {
        let requests: Vec<crate::ExportRequest> = self
            .session
            .exports
            .as_mut()
            .map(std::mem::take)
            .unwrap_or_default();
        for request in requests {
            // Keyed by the request's own path, so a roll opened in the same
            // turn never lends its like-named photo's frame.
            let raw = export_key(&request)
                .and_then(|key| self.memo.raw_frame(&key))
                .map(|frame| Arc::downgrade(&frame));
            self.pool.export(request, raw)?;
        }
        Ok(())
    }

    /// Reports the developed image's fitted rectangle to the model as the
    /// crop drag's canvas: a fact, so it does not bump the generation.
    fn report_preview_fit(&mut self) {
        let fit = self.preview_fit();
        self.session.ui.set_preview_fit(fit);
    }

    /// The develop box rectangle the cursor photo's held developed image
    /// fills, centred as `ui::blit` centres it, or `None` when not developing
    /// or no image is held for the cursor photo yet.
    fn preview_fit(&self) -> Option<td_ui::raster::Rect> {
        let ui = &self.session.ui;
        let r#box = ui.develop_box()?;
        let name = &ui.photos().get(ui.cursor()?)?.name;
        let image = self
            .developed
            .as_ref()
            .filter(|(preview, _)| preview.name == *name)
            .and_then(|(_, image)| image.as_ref())?;
        let width = i64::try_from(image.width).ok()?;
        let height = i64::try_from(image.height).ok()?;
        Some(td_ui::raster::Rect {
            x: r#box.x + (i64::from(r#box.width) - width) / 2,
            y: r#box.y + (i64::from(r#box.height) - height) / 2,
            width: u32::try_from(image.width).ok()?,
            height: u32::try_from(image.height).ok()?,
        })
    }

    /// Takes what the pool made. The developed preview replaces the one held
    /// and, when it is the one the model wants and an image was made, is a
    /// new generation. A thumbnail for a roll or a scale no longer held is
    /// dropped; one for a photo on screen is a new generation; past the
    /// memory budget, the least recently shown that is not on screen goes
    /// first.
    fn collect(&mut self) -> Result<()> {
        let done = self.pool.collect()?;
        if done.is_empty() {
            return Ok(());
        }
        let mut touched = false;
        let mut thumbs = Vec::new();
        let mut developed = None;
        for item in done {
            match item {
                Done::Thumb { key, image } => thumbs.push((key, image)),
                // Only the newest develop matters; earlier ones are stale.
                Done::Develop { preview, made } => developed = Some((preview, made)),
                Done::Export { request, result } => self.exported(request, result),
            }
        }
        if let Some((preview, made)) = developed {
            // A develop for a roll no longer held (one from a previous roll
            // completing after a switch) is dropped, not shown or memoized;
            // either way the wants are recomputed, so the current roll's
            // develop is asked for.
            if self.held.as_ref().map(|(roll, _)| roll) == Some(&preview.roll) {
                // The develop the model wants now, read once: `on_photo` is
                // whether its photo is still the cursor's (so its levels
                // become current), and the exact match drives the redraw.
                let wanted = self.wanted_preview();
                let on_photo = wanted
                    .as_ref()
                    .is_some_and(|w| w.roll == preview.roll && w.name == preview.name);
                let image = self.memo.merge(&preview, made, self.clock, on_photo);
                // A new generation whenever this is the develop the model
                // wants now, an image or not: a made one to show it, a failed
                // one to redraw the placeholder over any image a previous
                // develop left in the box and to settle idle honestly.
                touched |= wanted.as_ref() == Some(&preview);
                self.developed = Some((preview, image));
            }
            self.wanted_at = None;
        }
        if let Some((roll, scale)) = self.held.clone() {
            let ui = &self.session.ui;
            let on_screen: HashSet<String> = ui
                .visible()
                .into_iter()
                .filter_map(|(index, _)| ui.photos().get(index))
                .map(|photo| photo.name.clone())
                .collect();
            for (key, image) in thumbs {
                if key.roll != roll || key.scale != scale {
                    continue;
                }
                touched |= on_screen.contains(&key.name);
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
        }
        if touched {
            self.session.ui.touch();
        }
        Ok(())
    }

    /// An export finished: the status row's note says which name it took or
    /// that it failed (its reason went to stderr on the worker), a new
    /// generation either way, and the frame it decoded joins the raw cache
    /// when the roll is still the held one, so the photo develops from it.
    fn exported(
        &mut self,
        request: crate::ExportRequest,
        result: std::result::Result<(PathBuf, Option<crate::RawFrame>), String>,
    ) {
        let key = export_key(&request);
        // The export's roll is the one held, or the note is stderr's: the
        // row's note is the open roll's, and a like-named photo there is
        // another photo.
        let held = self
            .held
            .as_ref()
            .zip(key.as_ref())
            .is_some_and(|((roll, _), key)| *roll == key.roll);
        let name = key.as_ref().map_or("", |key| key.name.as_str());
        let text = match result {
            Ok((out, raw)) => {
                if let (Some(raw), true, Some(key)) = (raw, held, key.clone()) {
                    self.memo.cache_raw(key, Arc::new(raw), self.clock);
                }
                crate::export_note(&out)
            }
            // The worker noted why.
            Err(_) => format!("export of {name} failed"),
        };
        if held {
            self.session.ui.set_export(Some(text));
        } else {
            note(&text);
        }
    }

    /// The develop the model wants now: the cursor photo fitted to the
    /// develop box at its sidecar's crop, exposure and look, or `None`
    /// outside develop mode or before a roll.
    fn wanted_preview(&self) -> Option<Preview> {
        let ui = &self.session.ui;
        let r#box = ui.develop_box()?;
        let index = ui.cursor()?;
        let photo = ui.photos().get(index)?;
        let roll = ui.roll()?;
        Some(Preview {
            roll: PathBuf::from(OsStr::from_bytes(roll)),
            name: photo.name.clone(),
            box_w: r#box.width as usize,
            box_h: r#box.height as usize,
            // Crop-adjust shows the uncropped image so the crop can be grown;
            // its handles overlay the whole frame. Elsewhere the preview is the
            // cropped result.
            crop: if ui.adjusting() {
                None
            } else {
                photo.sidecar.as_ref().and_then(|sidecar| sidecar.crop())
            },
            exposure: photo
                .sidecar
                .as_ref()
                .and_then(|sidecar| sidecar.exposure())
                .unwrap_or(0),
            look: photo
                .sidecar
                .as_ref()
                .and_then(|sidecar| sidecar.look())
                .map(str::to_string),
        })
    }

    /// The wants for the model as it stands, when it moved, and the count
    /// outstanding either way: the thumbnails not yet held, in the order
    /// `Controller::wanted` gives them, and the develop it wants unless the
    /// frame already holds it. What is held is the open roll's at the
    /// surface's scale; when either changes the rest is let go.
    fn want(&mut self) -> Result<usize> {
        let generation = self.session.ui.generation();
        if self.wanted_at == Some(generation) {
            return self.pool.outstanding();
        }
        self.wanted_at = Some(generation);
        let scale = self.session.ui.surface().scale.value();
        let held = self
            .session
            .ui
            .roll()
            .map(|roll| (PathBuf::from(OsStr::from_bytes(roll)), scale));
        if self.held != held {
            self.thumbs.clear();
            self.thumb_bytes = 0;
            self.developed = None;
            self.memo.clear();
            self.held = held;
        }
        let ui = &self.session.ui;
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
        // The develop, asked for once and not again while the frame holds it,
        // so a settled develop is not re-run every turn; when it is asked
        // for, planned from the memo so only the invalidated levels rerun.
        let preview = self.wanted_preview();
        let submit = match (&preview, &self.developed) {
            (Some(want), Some((have, _))) if want == have => None,
            (Some(want), _) => Some((want.clone(), self.memo.plan(want))),
            (None, _) => None,
        };
        self.pool.want(keys, submit)
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
        // The exports a key or a request asked for in the closing turn
        // reach the pool before it is joined, so a `quit` waits for them
        // too, whichever way the window closed.
        let result = match (result, self.submit_exports()) {
            (Ok(()), Err(why)) => Err(format!("exports not queued at close: {why}")),
            (result, _) => result,
        };
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
    /// photos on screen or, in develop mode, the developed preview in the
    /// develop box, each centred and clipped to the grid's area, then the
    /// flag badges again over the thumbnails, within the area too.
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
            developed,
            clock,
            ..
        } = self;
        let ui = &session.ui;
        let area = ui.layout().area;
        let visible = ui.visible();
        let scene = ui.scene();
        let badges = ui.badges();
        let marquee = ui.marquee();
        let photos = ui.photos();
        // The develop box and the image to fill it: the held develop of the
        // cursor's photo, so an exposure or look edit shows the last frame of
        // that photo rather than a placeholder while the new one is made, but
        // a move to another photo shows the placeholder until its own develop
        // lands, not the previous photo's pixels.
        let develop = ui.develop_box().and_then(|r#box| {
            let name = &photos.get(ui.cursor()?)?.name;
            developed
                .as_ref()
                .filter(|(preview, _)| preview.name == *name)
                .and_then(|(_, image)| image.as_ref())
                .map(|image| (r#box, image))
        });
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
            if let Some((r#box, image)) = develop {
                ui::blit(pixels, surface, stride, area, r#box, image).map_err(error)?;
            }
            // Within the area as the blits were: the status band covers a
            // badge that runs under it.
            Raster::new(pixels, font, surface, stride)
                .map_err(error)?
                .paint(&badges, area)
                .map_err(error)?;
            // The crop marquee over the develop image, as the badges are
            // painted over the thumbnails.
            Raster::new(pixels, font, surface, stride)
                .map_err(error)?
                .paint(&marquee, area)
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

    fn preview(name: &str) -> Preview {
        Preview {
            roll: PathBuf::from("/td-photo/no-such-roll"),
            name: name.to_string(),
            box_w: 300,
            box_h: 200,
            crop: None,
            exposure: 0,
            look: None,
        }
    }

    #[test]
    fn the_queue_skips_what_runs_and_counts_both() {
        let mut queue = Queue::default();
        assert_eq!(queue.replace(vec![key("a"), key("b")], None), 2);
        let taken = queue.pending.pop_front().unwrap();
        queue.running.insert(taken);
        assert_eq!(queue.replace(vec![key("a"), key("c")], None), 2);
        let pending: Vec<&str> = queue.pending.iter().map(|key| key.name.as_str()).collect();
        assert_eq!(pending, ["c"]);
        assert_eq!(queue.replace(Vec::new(), None), 1);
        assert!(queue.pending.is_empty());
        // Another roll's file of the same name is another key.
        let other = Key {
            roll: PathBuf::from("/td-photo/another"),
            ..key("a")
        };
        assert_eq!(queue.replace(vec![other], None), 2);
    }

    #[test]
    fn the_queue_runs_one_develop_and_keeps_it_outstanding() {
        let mut queue = Queue::default();
        // One develop wanted; taking it moves it into the in-flight slot,
        // where it stays outstanding until the window collects it.
        assert_eq!(
            queue.replace(Vec::new(), Some((preview("a"), Start::Decode))),
            1
        );
        match queue.take() {
            Some(Task::Develop(request, _)) => assert_eq!(request.name, "a"),
            _ => panic!("expected the develop"),
        }
        assert!(queue.developing.is_some());
        assert_eq!(queue.outstanding(), 1);
        // No second develop while one is in flight, and wanting the same one
        // again does not re-queue it.
        assert!(queue.take().is_none());
        assert_eq!(
            queue.replace(Vec::new(), Some((preview("a"), Start::Decode))),
            1
        );
        assert!(queue.preview.is_none());
        // A different develop queues behind the one in flight; only once the
        // in-flight one is collected does a worker take it.
        assert_eq!(
            queue.replace(Vec::new(), Some((preview("b"), Start::Decode))),
            2
        );
        assert!(queue.take().is_none());
        queue.developing = None;
        match queue.take() {
            Some(Task::Develop(request, _)) => assert_eq!(request.name, "b"),
            _ => panic!("expected the queued develop"),
        }
        // A thumbnail is taken before the develop.
        let mut queue = Queue::default();
        assert_eq!(
            queue.replace(vec![key("t")], Some((preview("a"), Start::Decode))),
            2
        );
        match queue.take() {
            Some(Task::Thumb(taken)) => assert_eq!(taken.name, "t"),
            _ => panic!("expected the thumbnail first"),
        }
    }

    #[test]
    fn a_finished_develop_drops_the_queued_plan() {
        // A develop is in flight and another is queued behind it (an edit
        // made while it ran). When it finishes, the in-flight slot empties
        // and the queued plan is dropped, so no worker takes a plan made
        // against the pre-merge memo; the turn loop replans in the same turn.
        let mut queue = Queue {
            developing: Some(preview("a")),
            preview: Some((preview("b"), Start::Decode)),
            ..Queue::default()
        };
        queue.develop_done();
        assert!(queue.developing.is_none());
        assert!(queue.preview.is_none());
        assert_eq!(queue.outstanding(), 0);
    }

    fn export(name: &str) -> crate::ExportRequest {
        crate::ExportRequest {
            path: PathBuf::from("/td-photo/none").join(name),
            exposure: 0,
            crop: None,
            look: None,
        }
    }

    #[test]
    fn exports_survive_the_wants_run_in_order_one_at_a_time_and_drain_at_close() {
        let mut queue = Queue::default();
        queue.exports.push_back((export("a.NEF"), None));
        queue.exports.push_back((export("b.NEF"), None));
        // A replacement of the wants leaves the exports where they are, and
        // they count as outstanding.
        assert_eq!(
            queue.replace(vec![key("t")], Some((preview("p"), Start::Decode))),
            4
        );
        // A thumbnail and the develop go first; then one export, and not the
        // second while it is in flight.
        assert!(matches!(queue.take(), Some(Task::Thumb(_))));
        assert!(matches!(queue.take(), Some(Task::Develop(..))));
        match queue.take() {
            Some(Task::Export(request, None)) => {
                assert_eq!(request.path.file_name().unwrap(), "a.NEF")
            }
            _ => panic!("expected the first export"),
        }
        assert!(queue.take().is_none());
        assert_eq!(queue.outstanding(), 4);
        // Collected, the next runs; a closing queue hands out exports alone
        // and is drained once none is left to take.
        queue.exporting = None;
        queue.closing = true;
        queue.pending.push_back(key("u"));
        queue.preview = Some((preview("q"), Start::Decode));
        assert!(!queue.drained());
        match queue.take() {
            Some(Task::Export(request, _)) => {
                assert_eq!(request.path.file_name().unwrap(), "b.NEF")
            }
            _ => panic!("expected the second export"),
        }
        // Not drained while the second is in flight; the worker's leaving
        // the slot drains it.
        assert!(!queue.drained());
        assert!(queue.take().is_none());
        queue.exporting = None;
        assert!(queue.drained());
    }

    #[test]
    fn the_pool_runs_an_export_and_reports_what_came_of_it() {
        let pool = Pool::start(1).unwrap();
        // A file that is not there fails on the worker; the request comes
        // back with why. It leaves the count as its result is sent, so the
        // count reads zero with the result still in the channel, never the
        // other way round.
        assert_eq!(pool.export(export("a.NEF"), None).unwrap(), 1);
        let mut sent = false;
        for _ in 0..2000 {
            if pool.outstanding().unwrap() == 0 {
                sent = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(sent, "the export never left the count");
        let done = pool.collect().unwrap();
        match done.as_slice() {
            [Done::Export { request, result }] => {
                assert_eq!(request.path.file_name().unwrap(), "a.NEF");
                assert!(result.is_err());
            }
            _ => panic!("expected one export result"),
        }
        assert_eq!(pool.outstanding().unwrap(), 0);
    }

    #[test]
    fn an_export_is_keyed_by_its_own_path() {
        let key = export_key(&export("a.NEF")).unwrap();
        assert_eq!(key.roll, PathBuf::from("/td-photo/none"));
        assert_eq!(key.name, "a.NEF");
        let mut memo = Memo::default();
        let frame = Arc::new(crate::RawFrame::synth(4));
        memo.cache_raw(key.clone(), Arc::clone(&frame), 1);
        assert!(memo.raw_frame(&key).is_some());
        // The same name in another roll is another photo.
        assert!(memo.raw_frame(&photo_key("a.NEF")).is_none());
    }

    #[test]
    fn the_pool_runs_queued_exports_one_after_another_without_the_wants() {
        let pool = Pool::start(2).unwrap();
        assert_eq!(pool.export(export("a.NEF"), None).unwrap(), 1);
        // One or two outstanding: the first may already have failed and
        // left the count before the second is queued.
        assert!((1..=2).contains(&pool.export(export("b.NEF"), None).unwrap()));
        // Nothing but collection: the second runs once the first has left
        // the slot, with no want to wake the workers.
        let mut names = Vec::new();
        for _ in 0..2000 {
            for done in pool.collect().unwrap() {
                if let Done::Export { request, result } = done {
                    assert!(result.is_err());
                    names.push(request.path.file_name().unwrap().to_owned());
                }
            }
            if names.len() == 2 {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(names, ["a.NEF", "b.NEF"]);
        assert_eq!(pool.outstanding().unwrap(), 0);
    }

    #[test]
    fn a_closing_pool_drains_the_exports_queued_without_the_window() {
        // The window collects nothing once it closes: the workers alone must
        // hand the exports on, or the join never returns.
        let (tx, rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("closing-pool".to_string())
            .spawn(move || {
                let pool = Pool::start(2).unwrap();
                for name in ["a.NEF", "b.NEF", "c.NEF"] {
                    pool.export(export(name), None).unwrap();
                }
                drop(pool);
                let _ = tx.send(());
            })
            .unwrap();
        assert!(
            rx.recv_timeout(Duration::from_secs(10)).is_ok(),
            "the pool did not join with exports queued"
        );
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
        assert_eq!(
            pool.want(vec![key("a.NEF"), key("b.NEF")], None).unwrap(),
            2
        );
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
        let mut names: Vec<String> = done
            .iter()
            .filter_map(|done| match done {
                Done::Thumb { key, .. } => Some(key.name.clone()),
                Done::Develop { .. } | Done::Export { .. } => None,
            })
            .collect();
        names.sort_unstable();
        assert_eq!(names, ["a.NEF", "b.NEF"]);
        // The roll is not there, so neither could be made; each was said.
        assert!(done
            .iter()
            .all(|done| matches!(done, Done::Thumb { image: None, .. })));
        assert_eq!(pool.outstanding().unwrap(), 0);
    }

    #[test]
    fn the_pool_runs_the_develop_and_keeps_it_outstanding() {
        let pool = Pool::start(2).unwrap();
        assert_eq!(
            pool.want(Vec::new(), Some((preview("x.NEF"), Start::Decode)))
                .unwrap(),
            1
        );
        let mut got = false;
        for _ in 0..2000 {
            for done in pool.collect().unwrap() {
                if let Done::Develop { preview, made } = done {
                    assert_eq!(preview.name, "x.NEF");
                    // The roll is not there, so it could not be made.
                    assert!(matches!(made, Made::None));
                    got = true;
                }
            }
            if got {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(got, "no develop result");
        assert_eq!(pool.outstanding().unwrap(), 0);
    }

    fn photo_key(name: &str) -> PhotoKey {
        PhotoKey {
            roll: PathBuf::from("/td-photo/no-such-roll"),
            name: name.to_string(),
        }
    }

    fn made_decoded(samples: usize, long_edge: usize) -> Made {
        let raw = Arc::new(crate::RawFrame::synth(samples));
        let meta = raw.meta();
        Made::Decoded {
            raw,
            meta,
            level1: Arc::new(develop::Level1 {
                width: 1,
                height: 1,
                rgb: vec![0, 0, 0],
            }),
            long_edge,
            crop: None,
            level2: Arc::new(develop::Level2 {
                width: 1,
                height: 1,
                rgb: vec![0.0, 0.0, 0.0],
            }),
            image: Rgb8 {
                width: 1,
                height: 1,
                data: vec![0, 0, 0],
            },
        }
    }

    #[test]
    fn the_memo_plans_the_start_from_what_it_holds() {
        let mut memo = Memo::default();
        let a = preview("a"); // box 300x200, so long edge 300
                              // Nothing held: a decode.
        assert_eq!(memo.plan(&a).stage(), Stage::Decode);
        // Develop it on the cursor: level 0 cached, level 1 and 2 current.
        assert!(memo.merge(&a, made_decoded(4, 300), 1, true).is_some());
        // Same photo, same box, another exposure or look: level 3 alone.
        assert_eq!(memo.plan(&a).stage(), Stage::Level3);
        let a_edited = Preview {
            exposure: 150,
            look: Some("mono".to_string()),
            ..a.clone()
        };
        assert_eq!(memo.plan(&a_edited).stage(), Stage::Level3);
        // Same photo, a larger box: level 2 from the cached level 1.
        let a_big = Preview {
            box_w: 600,
            box_h: 400,
            ..a.clone()
        };
        assert_eq!(memo.plan(&a_big).stage(), Stage::Level2);
        // Same photo and box, a crop edit: level 2 reruns from the cached
        // level 1, since the crop is applied there.
        let a_cropped = Preview {
            crop: Some(library::Crop::new(2500, 2500, 5000, 5000).unwrap()),
            ..a.clone()
        };
        assert_eq!(memo.plan(&a_cropped).stage(), Stage::Level2);
        // Another photo, not cached: a decode.
        let b = preview("b");
        assert_eq!(memo.plan(&b).stage(), Stage::Decode);
        // Develop b on the cursor, then return to a: a's level 0 is still
        // cached, so its level 1 reruns rather than the codec.
        assert!(memo.merge(&b, made_decoded(4, 300), 2, true).is_some());
        assert_eq!(memo.plan(&b).stage(), Stage::Level3);
        assert_eq!(memo.plan(&a).stage(), Stage::Level1);
        // Cleared, everything is a decode again.
        memo.clear();
        assert_eq!(memo.plan(&a).stage(), Stage::Decode);
    }

    #[test]
    fn a_failed_develop_leaves_the_memo_untouched() {
        let mut memo = Memo::default();
        let a = preview("a");
        assert!(memo.merge(&a, made_decoded(4, 300), 1, true).is_some());
        // A develop that could not be made returns no frame and changes
        // nothing: the current photo's levels still plan a level-3 rerun.
        assert!(memo.merge(&a, Made::None, 2, true).is_none());
        assert_eq!(memo.plan(&a).stage(), Stage::Level3);
    }

    #[test]
    fn a_develop_finishing_off_the_cursor_caches_but_is_not_current() {
        let mut memo = Memo::default();
        let a = preview("a");
        let b = preview("b");
        // Develop a on the cursor: it becomes current.
        assert!(memo.merge(&a, made_decoded(4, 300), 1, true).is_some());
        assert_eq!(memo.plan(&a).stage(), Stage::Level3);
        // A develop for b lands while the cursor is still on a (on_photo
        // false, as after a switch back to a): b's level 0 is cached, but a
        // stays current, so an a edit still reruns level 3 alone and a return
        // to b reruns level 1 from the cache, not a decode.
        assert!(memo.merge(&b, made_decoded(4, 300), 2, false).is_some());
        assert_eq!(memo.plan(&a).stage(), Stage::Level3);
        assert_eq!(memo.plan(&b).stage(), Stage::Level1);
    }

    #[test]
    fn the_raw_cache_evicts_least_recently_shown_under_the_budget() {
        let mut memo = Memo::default();
        // Each frame is a bit over a third of the budget (bytes are two a
        // sample), so two fit and a third pushes the least recently shown
        // out. The buffers are zero-allocated, hence lazily faulted, so this
        // costs address space, not resident memory.
        let big = develop::RAW_CACHE_BYTES / 6 + 2_000_000;
        memo.cache_raw(photo_key("a"), Arc::new(crate::RawFrame::synth(big)), 1);
        memo.cache_raw(photo_key("b"), Arc::new(crate::RawFrame::synth(big)), 2);
        assert_eq!(memo.raw.len(), 2);
        assert!(memo.raw_bytes <= develop::RAW_CACHE_BYTES);
        // Show a again, so b is now the oldest; the third frame evicts b.
        memo.touch_raw(&photo_key("a"), 3);
        memo.cache_raw(photo_key("c"), Arc::new(crate::RawFrame::synth(big)), 4);
        assert!(memo.raw_bytes <= develop::RAW_CACHE_BYTES);
        let held: Vec<&str> = memo.raw.iter().map(|c| c.key.name.as_str()).collect();
        assert_eq!(held, ["a", "c"]);
    }
}

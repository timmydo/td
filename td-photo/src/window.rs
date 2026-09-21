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
use td_ui::keyboard::Held;
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
/// the window blits them, made here on the calling thread. `--single` in
/// place of `--develop` is the cull single view of the photo; `--zoom`
/// after `--develop` shows the develop box at 100% around the image's
/// centre, as `zoom-100` does.
pub fn preview(rest: &[OsString]) -> Result<()> {
    let [size, rest @ ..] = rest else {
        return Err("--preview needs WxH; see --help".to_string());
    };
    let mut roll: Option<PathBuf> = None;
    let mut develop: Option<usize> = None;
    let mut single = false;
    let mut zoom = false;
    let mut args = rest.iter();
    while let Some(arg) = args.next() {
        if arg == "--zoom" {
            if develop.is_none() || single {
                return Err("--zoom needs --develop before it; see --help".to_string());
            }
            zoom = true;
        } else if arg == "--develop" || arg == "--single" {
            if develop.is_some() {
                return Err("--develop or --single may be given only once".to_string());
            }
            single = arg == "--single";
            // A position follows when the next argument is a decimal;
            // otherwise the cursor's photo, which is the first at open.
            let flag = if single { "--single" } else { "--develop" };
            let position = match args.clone().next() {
                Some(next)
                    if next.to_str().is_some_and(|text| {
                        !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit())
                    }) =>
                {
                    args.next();
                    next.to_str()
                        .and_then(|text| text.parse::<usize>().ok())
                        .ok_or_else(|| format!("{flag} POSITION is out of range"))?
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
        return Err("--develop or --single needs a ROLL; see --help".to_string());
    }
    let (width, height) = size
        .to_str()
        .and_then(|text| text.split_once('x'))
        .and_then(|(w, h)| Some((w.parse::<usize>().ok()?, h.parse::<usize>().ok()?)))
        .ok_or_else(|| format!("--preview {size:?} is not WxH"))?;
    let mut session = session(width, height, roll.as_deref())?;
    if let Some(position) = develop {
        enter_view(&mut session, position, single)?;
        if zoom {
            session
                .ui
                .action("zoom-100", &[])
                .map_err(|e| format!("--zoom: {e}"))?;
        }
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
        // The developed preview, in develop or the single view: the cursor
        // photo blitted into its box, the same frame the window shows
        // there, made here on the calling thread. The loop above blitted
        // the grid's thumbnails in cull and the filmstrip's in develop.
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

/// Puts a `--preview` session into the develop view, or with `single`
/// the cull single view, of the photo at
/// `position`, so the frame is the developed preview the window shows there.
/// Neither action writes a sidecar, so there is nothing to carry out.
fn enter_view(session: &mut Session, position: usize, single: bool) -> Result<()> {
    let flag = if single { "--single" } else { "--develop" };
    session
        .ui
        .action("select", &[position.to_string().as_str()])
        .map_err(|e| format!("{flag} {position}: {e}"))?;
    session
        .ui
        .action(if single { "view" } else { "develop" }, &[])
        .map_err(|e| format!("{flag}: {e}"))?;
    Ok(())
}

/// The developed preview for a `--preview --develop` or `--single`
/// session: the develop box (the single view's) and the cursor photo
/// developed to fit it, or to the model's zoom, at the sidecar's exposure
/// and look, made on the calling thread; `None` without a box or when the
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
        session.ui.zoom(),
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
/// box it fits, the sidecar's crop, exposure and look, and the zoom and
/// its centre when the box does not fit the image, so the same photo at
/// another box, crop, exposure, look or zoom is another develop. There is
/// no generation: two requests with the same fields yield the same pixels.
#[derive(Clone, Debug, Eq, PartialEq)]
struct Preview {
    roll: PathBuf,
    name: String,
    box_w: usize,
    box_h: usize,
    crop: Option<library::Crop>,
    exposure: i32,
    look: Option<String>,
    zoom: Option<(u32, (u32, u32))>,
}

impl Preview {
    /// The zoom as the pipeline takes it, with the box.
    fn zoom(&self) -> Option<develop::Zoom> {
        self.zoom.map(|(percent, centre)| develop::Zoom {
            percent,
            centre,
            box_w: self.box_w,
            box_h: self.box_h,
        })
    }
}

/// The level a develop starts from, and the cached levels it reuses: an
/// exposure or look edit starts at `Level3` (level 2 reused), a resize at
/// `Level2` (level 1 reused), a photo whose level 0 is cached at `Level1`,
/// and a new photo at `Decode`; a zoomed develop of the current photo
/// starts at `Zoom` (level 1 reused, and the cached level 0 with it past
/// `HALF_ZOOM`). The window plans it from what its memo holds; the worker
/// runs from here forward.
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
    Zoom {
        raw: Option<Arc<crate::RawFrame>>,
        meta: crate::Meta,
        level1: Arc<develop::Level1>,
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
    /// A zoom start, and whether it carries the level-0 frame.
    Zoom(bool),
}

#[cfg(test)]
impl Start {
    fn stage(&self) -> Stage {
        match self {
            Start::Decode => Stage::Decode,
            Start::Level1 { .. } => Stage::Level1,
            Start::Level2 { .. } => Stage::Level2,
            Start::Level3 { .. } => Stage::Level3,
            Start::Zoom { raw, .. } => Stage::Zoom(raw.is_some()),
        }
    }
}

/// What a develop produced, to merge into the window's memo and show: each
/// variant carries only the levels recomputed, since the window already
/// holds the earlier ones; a zoomed develop's fit level 2 is kept as the
/// current one so leaving the zoom reruns level 3 alone, while its zoomed
/// level 2 is transient, remade on the next zoom or pan. `None` when it
/// could not be made (said on stderr), which redraws the box's placeholder.
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
    Zoom {
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
    /// A neighbour's level 0 decoded ahead of its develop, for the raw
    /// cache; `None` when it could not be (said on stderr).
    Prefetched {
        key: PhotoKey,
        frame: Option<crate::RawFrame>,
    },
}

/// What a worker takes off the queue: an export carries the cached level 0
/// when the window held it at submission, so a photo just developed exports
/// without the codec; weakly, so the queue keeps no frame the raw cache has
/// let go, and one evicted while queued is decoded again. A prefetch is a
/// neighbour's decode alone.
enum Task {
    Thumb(Key),
    Develop(Preview, Start),
    Export(crate::ExportRequest, Option<Weak<crate::RawFrame>>),
    Prefetch(PhotoKey),
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
    /// The neighbours whose level 0 the window would have decoded ahead, in
    /// order, and the one a worker is decoding or the window has not yet
    /// collected: the lowest class of work, taken only when nothing wanted
    /// is pending and none is in flight, so a prefetch never delays what is
    /// asked for, and one at a time beside the develop's decode. Not a job:
    /// `outstanding` leaves it out, since nothing waits for it and its
    /// result only fills the cache. The slot is held until the window
    /// collects the frame, so no develop decodes what is on its way.
    prefetch: VecDeque<PhotoKey>,
    prefetching: Option<PhotoKey>,
    /// A collected prefetch dropped a develop that waited on it: no
    /// prefetch is handed out until the window has replaced the wants,
    /// so that develop, replanned, is taken first.
    replan: bool,
    closing: bool,
}

impl Queue {
    /// Replaces the thumbnail wants with `thumbs` (less what is running) and
    /// the develop want with `preview`, but never re-queues the develop a
    /// worker already holds, and says how many jobs are outstanding.
    fn replace(&mut self, thumbs: Vec<Key>, preview: Option<(Preview, Start)>) -> usize {
        let running = &self.running;
        self.replan = false;
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

    /// Replaces the prefetch wants with `keys`, less the one in flight.
    fn replace_prefetch(&mut self, keys: Vec<PhotoKey>) {
        let prefetching = &self.prefetching;
        self.prefetch.clear();
        self.prefetch.extend(
            keys.into_iter()
                .filter(|key| prefetching.as_ref() != Some(key)),
        );
    }

    /// Whether the pending develop would decode the photo a prefetch is
    /// decoding: it waits for that, so one frame is not decoded twice.
    fn develop_waits(&self) -> bool {
        match (&self.preview, &self.prefetching) {
            (Some((preview, Start::Decode)), Some(key)) => key.is(preview),
            _ => false,
        }
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
    /// raw decode is never run twice at once (and not while a prefetch
    /// decodes its photo: that frame arrives and the develop is replanned
    /// from it), then an export when none is in flight, then, with nothing
    /// wanted pending and no prefetch in flight, a prefetch. A closing
    /// queue hands out exports alone, so what was asked for is written
    /// before the pool is joined.
    fn take(&mut self) -> Option<Task> {
        if !self.closing {
            if let Some(key) = self.pending.pop_front() {
                self.running.insert(key.clone());
                return Some(Task::Thumb(key));
            }
            if self.developing.is_none() && !self.develop_waits() {
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
        if !self.closing
            && !self.replan
            && self.prefetching.is_none()
            && self.pending.is_empty()
            && self.preview.is_none()
            && self.exports.is_empty()
        {
            while let Some(key) = self.prefetch.pop_front() {
                // The develop or the export in flight may be decoding this
                // very photo; its frame will be cached as it lands, so the
                // prefetch is dropped rather than decoded twice at once.
                if self.decoding(&key) {
                    continue;
                }
                self.prefetching = Some(key.clone());
                return Some(Task::Prefetch(key));
            }
        }
        None
    }

    /// Whether the develop or the export in flight is of `key`'s photo.
    fn decoding(&self, key: &PhotoKey) -> bool {
        self.developing.as_ref().is_some_and(|p| key.is(p))
            || self
                .exporting
                .as_ref()
                .and_then(export_key)
                .is_some_and(|exporting| exporting == *key)
    }

    /// A prefetch was collected: it leaves its slot, and a pending develop
    /// that waited to decode the same photo is dropped, no prefetch handed
    /// out until the window replaces the wants, so it replans that develop
    /// from the merged cache and a worker takes it first.
    fn prefetch_done(&mut self) {
        if self.develop_waits() {
            self.preview = None;
            self.replan = true;
        }
        self.prefetching = None;
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

    /// Replaces the thumbnail wants, the develop want and the prefetch
    /// wants, and says how many jobs are outstanding.
    fn want(
        &self,
        thumbs: Vec<Key>,
        preview: Option<(Preview, Start)>,
        prefetch: Vec<PhotoKey>,
    ) -> Result<usize> {
        let outstanding = {
            let mut queue = self.lock()?;
            queue.replace_prefetch(prefetch);
            queue.replace(thumbs, preview)
        };
        self.queue.1.notify_all();
        Ok(outstanding)
    }

    fn outstanding(&self) -> Result<usize> {
        Ok(self.lock()?.outstanding())
    }

    /// Whether a prefetch is queued, in flight or uncollected: not a job,
    /// but the turn loop polls for its frame as it does for a job's, so the
    /// next neighbour follows it without waiting for an event.
    fn prefetching(&self) -> Result<bool> {
        let queue = self.lock()?;
        Ok(queue.prefetching.is_some() || !queue.prefetch.is_empty())
    }

    /// The workers started.
    fn workers(&self) -> usize {
        self.threads.len()
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

    /// Takes what the workers made, each leaving the running set, the
    /// develop-in-flight slot or the prefetch slot as it is taken, under
    /// the one lock, so what `outstanding` counts next is exactly what the
    /// window does not hold and no develop is planned to decode a frame
    /// on its way. An export left its slot as the worker sent it, under
    /// the same lock.
    fn collect(&self) -> Result<Vec<Done>> {
        let mut queue = self.lock()?;
        let done: Vec<Done> = std::iter::from_fn(|| self.done.try_recv().ok()).collect();
        for item in &done {
            match item {
                Done::Thumb { key, .. } => {
                    queue.running.remove(key);
                }
                Done::Develop { .. } => queue.develop_done(),
                Done::Prefetched { .. } => queue.prefetch_done(),
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
            queue.prefetch.clear();
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
            Task::Prefetch(key) => {
                let frame = match crate::decode_raw(&key.roll.join(&key.name)) {
                    Ok((raw, _)) => Some(raw),
                    Err(why) => {
                        note(&why);
                        None
                    }
                };
                Done::Prefetched { key, frame }
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
    // The zoomed frame from level 1, and the level 0 past half zoom: the
    // window's zoomed level 2 is transient, so it goes straight to the
    // frame.
    let zoomed = |level1: &develop::Level1, raw: Option<&crate::RawFrame>, meta: &crate::Meta| {
        let zoom = preview.zoom().ok_or_else(|| "no zoom".to_string())?;
        let level2 = crate::zoom_level2(level1, raw, meta, crop, zoom, threads)?;
        frame(&level2, meta)
    };
    // Level 1 to its fit level 2 and the frame -- the zoomed one when the
    // preview zooms, the fit level 2 kept either way -- shared by the
    // decode and cached-raw starts, which have the frame for a zoom past
    // half.
    let from_level1 = |level1: &develop::Level1, raw: &crate::RawFrame, meta: &crate::Meta| {
        let level2 = crate::level1_level2(level1, meta, crop, long_edge, threads)?;
        let image = if preview.zoom.is_some() {
            zoomed(level1, Some(raw), meta)?
        } else {
            frame(&level2, meta)?
        };
        Ok::<_, String>((level2, image))
    };
    Ok(match start {
        Start::Level3 { meta, level2 } => Made::Level3 {
            image: frame(&level2, &meta)?,
        },
        Start::Zoom { raw, meta, level1 } => Made::Zoom {
            image: zoomed(&level1, raw.as_deref(), &meta)?,
        },
        Start::Level2 { meta, level1 } => {
            let level2 = crate::level1_level2(&level1, &meta, crop, long_edge, threads)?;
            let image = frame(&level2, &meta)?;
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
            let (level2, image) = from_level1(&level1, &raw, &meta)?;
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
            let (level2, image) = from_level1(&level1, &raw, &meta)?;
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

/// How far a window developed around `from` lies from one wanted around
/// `want`, in surface pixels at `zoom` percent over `extent` in a box:
/// units of the axis to photosites, then to pixels, the held centre
/// against the wanted one, so an image blitted by it is where its content
/// is. Both centres are held inside the image first (`ui::clamped_centre`,
/// the rule the viewport clamps by), so a centre the model normalized
/// after the develop was asked for -- a photo switch landing before its
/// extent is known -- names the same window and shifts nothing.
fn centre_shift(
    from: (u32, u32),
    want: (u32, u32),
    zoom: u32,
    extent: (usize, usize),
    r#box: (usize, usize),
) -> (i64, i64) {
    let (box_w, box_h) = (
        u32::try_from(r#box.0).unwrap_or(u32::MAX),
        u32::try_from(r#box.1).unwrap_or(u32::MAX),
    );
    let from = ui::clamped_centre(from, zoom, extent, box_w, box_h);
    let want = ui::clamped_centre(want, zoom, extent, box_w, box_h);
    let shift = |held: u32, want: u32, extent: usize| {
        (i64::from(held) - i64::from(want))
            .saturating_mul(i64::try_from(extent).unwrap_or(i64::MAX))
            .saturating_mul(i64::from(zoom))
            / (i64::from(develop::CENTRE_UNIT) * 100)
    };
    (
        shift(from.0, want.0, extent.0),
        shift(from.1, want.1, extent.1),
    )
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

/// The workers a pool needs before it prefetches: one to decode ahead and
/// one free for what is asked for.
const MIN_PREFETCH_WORKERS: usize = 2;

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
    /// The neighbours a prefetch could not decode (`None`: never asked for
    /// again while the memo lives) or the cache had no room for (the bytes
    /// it needed: asked for again once that much is free).
    refused: Vec<(PhotoKey, Option<usize>)>,
}

impl Memo {
    /// Lets go of every level, as when the roll or scale changes.
    fn clear(&mut self) {
        self.current = None;
        self.raw.clear();
        self.raw_bytes = 0;
        self.refused.clear();
    }

    /// The level a develop for `preview` starts from, given what is held:
    /// level 3 when its level 2 is current for the box and crop, level 2 when
    /// only level 1 is (a resize or crop edit), level 1 when the level 0 is
    /// cached, else a decode; a zoomed develop of the current photo starts
    /// at its level 1 with the cached level 0 (a zoom past half needs it:
    /// evicted, the decode starts over, caching it again). Each start
    /// carries the cached levels the worker reuses.
    fn plan(&self, preview: &Preview) -> Start {
        let long_edge = preview.box_w.max(preview.box_h);
        let raw = self
            .raw
            .iter()
            .find(|cached| cached.key.is(preview))
            .map(|cached| Arc::clone(&cached.frame));
        if let Some(current) = &self.current {
            if current.key.is(preview) {
                if let Some((zoom, _)) = preview.zoom {
                    if zoom <= develop::HALF_ZOOM || raw.is_some() {
                        return Start::Zoom {
                            raw,
                            meta: current.meta,
                            level1: Arc::clone(&current.level1),
                        };
                    }
                } else if current.long_edge == long_edge && current.crop == preview.crop {
                    return Start::Level3 {
                        meta: current.meta,
                        level2: Arc::clone(&current.level2),
                    };
                }
                if preview.zoom.is_none() {
                    return Start::Level2 {
                        meta: current.meta,
                        level1: Arc::clone(&current.level1),
                    };
                }
            }
        }
        if let Some(raw) = raw {
            return Start::Level1 { raw };
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
            Made::Zoom { image } => {
                // The level 0 it may have run from stays recent.
                self.touch_raw(&key, shown);
                Some(image)
            }
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

    /// Takes a prefetch's result: the level-0 frame joins the cache when
    /// there is room for it under `develop::RAW_CACHE_BYTES` without an
    /// eviction (a prefetch never costs a frame shown), as the least
    /// recently shown, never having been; one already held is left as it
    /// is. A frame that could not be decoded, or had no room, is refused
    /// (`prefetchable`). Says whether the frame is held.
    fn prefetched(&mut self, key: PhotoKey, frame: Option<Arc<crate::RawFrame>>) -> bool {
        // One entry a photo: a refusal replaces an earlier one.
        self.refused.retain(|(refused, _)| *refused != key);
        let Some(frame) = frame else {
            self.refused.push((key, None));
            return false;
        };
        if self.raw_frame(&key).is_some() {
            return true;
        }
        let bytes = frame
            .bytes()
            .saturating_add(key.roll.as_os_str().len())
            .saturating_add(key.name.len())
            .saturating_add(RAW_OVERHEAD);
        if self.raw_bytes.saturating_add(bytes) > develop::RAW_CACHE_BYTES {
            self.refused.push((key, Some(bytes)));
            return false;
        }
        self.raw.push_back(Cached {
            key,
            frame,
            bytes,
            shown: 0,
        });
        self.raw_bytes = self.raw_bytes.saturating_add(bytes);
        true
    }

    /// Whether a neighbour is worth a prefetch: not held, not refused for
    /// good, and not refused for room that is still not there.
    fn prefetchable(&self, key: &PhotoKey) -> bool {
        self.raw_frame(key).is_none()
            && !self.refused.iter().any(|(refused, needed)| {
                refused == key
                    && needed.is_none_or(|bytes| {
                        self.raw_bytes.saturating_add(bytes) > develop::RAW_CACHE_BYTES
                    })
            })
    }

    /// Whether a frame the size of the largest held would fit beside them
    /// without an eviction: what a prefetch is asked for on. An empty cache
    /// has room for a first.
    fn room_for_another(&self) -> bool {
        let largest = self
            .raw
            .iter()
            .map(|cached| cached.bytes)
            .max()
            .unwrap_or(0);
        self.raw_bytes.saturating_add(largest) <= develop::RAW_CACHE_BYTES
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
            KeyboardEvent::Held(held) => {
                self.input(Input::Held(held));
            }
            // A new map clears the roles held without a report: the hints
            // go with them, and the next change brings them back.
            KeyboardEvent::Keymap(result) => {
                if let Err(why) = result {
                    note(&why);
                }
                self.input(Input::Held(Held::default()));
            }
            KeyboardEvent::Refused(why) => note(&why),
            KeyboardEvent::Ready => {}
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
        // outstanding or a prefetch is under way, at most a control poll
        // while the socket is served.
        let mut wait = Duration::from_millis(self.client.wait_ms(now));
        if jobs > 0 || self.pool.prefetching()? {
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
    /// crop drag's canvas, and the cursor photo's extent as the zoom's
    /// measure: facts, so they do not bump the generation.
    fn report_preview_fit(&mut self) {
        let fit = self.preview_fit();
        self.session.ui.set_preview_fit(fit);
        let extent = self.zoom_extent();
        self.session.ui.set_zoom_extent(extent);
    }

    /// Whether the held develop is the wanted one up to a centre the model
    /// normalized after it was asked for: a photo switch lands the zoomed
    /// develop before its extent is known, then `clamp_centre` moves the
    /// centre into the room, naming the window the develop already clamped
    /// to, so it is not asked for again.
    fn normalized(&self, want: &Preview, have: &Preview) -> bool {
        let (Some((zoom, wanted)), Some((held_zoom, held))) = (want.zoom, have.zoom) else {
            return false;
        };
        let (Some(extent), Ok(box_w), Ok(box_h)) = (
            self.zoom_extent(),
            u32::try_from(want.box_w),
            u32::try_from(want.box_h),
        ) else {
            return false;
        };
        zoom == held_zoom
            && Preview {
                zoom: want.zoom,
                ..have.clone()
            } == *want
            && ui::clamped_centre(held, zoom, extent, box_w, box_h)
                == ui::clamped_centre(wanted, zoom, extent, box_w, box_h)
    }

    /// How far the held zoomed image's window lies from the one the model
    /// wants, in surface pixels, while the develop at a moved centre is on
    /// its way: the held image is blitted shifted by it, so a pan's release
    /// leaves the image where the pointer left it until the new frame
    /// lands, rather than snapping back and then jumping. Zero unless the
    /// held develop differs from the wanted one only by its centre (an
    /// exposure or look edit racing the release shifts the same), and zero
    /// for a centre the model only normalized (`centre_shift`).
    fn held_shift(&self) -> (i64, i64) {
        let none = (0, 0);
        let Some(wanted) = self.wanted_preview() else {
            return none;
        };
        let Some((held, Some(_))) = self.developed.as_ref() else {
            return none;
        };
        let (Some((zoom, want)), Some((held_zoom, from))) = (wanted.zoom, held.zoom) else {
            return none;
        };
        let same = held_zoom == zoom
            && held.name == wanted.name
            && held.crop == wanted.crop
            && (held.box_w, held.box_h) == (wanted.box_w, wanted.box_h);
        let (Some(extent), true) = (self.zoom_extent(), same) else {
            return none;
        };
        centre_shift(from, want, zoom, extent, (wanted.box_w, wanted.box_h))
    }

    /// The cursor photo's oriented, cropped extent at full resolution, at
    /// the crop the mode wants, from the level 1 the memo holds current for
    /// it; `None` before a develop of the photo lands.
    fn zoom_extent(&self) -> Option<(usize, usize)> {
        let wanted = self.wanted_preview()?;
        let current = self
            .memo
            .current
            .as_ref()
            .filter(|current| current.key.is(&wanted))?;
        crate::level1_extent(
            &current.level1,
            &current.meta,
            wanted.crop.map(crate::crop_fractions),
        )
    }

    /// The develop box rectangle the cursor photo's held developed image
    /// fills, centred as `ui::blit` centres it, or `None` without a box or
    /// when no image is held for the cursor photo yet at the crop and zoom the
    /// mode wants: in crop-adjust the uncropped frame, else the sidecar's
    /// crop. Zoomed, the fit is unused (the box is no crop canvas and the
    /// pan needs none), so a held develop `normalized` treats as the wanted
    /// one need not be one here.
    /// The held frame of the other crop is still shown while the wanted one
    /// is made, but it is no canvas: a crop drawn or dragged against it
    /// would map onto content the frame does not show.
    fn preview_fit(&self) -> Option<td_ui::raster::Rect> {
        let ui = &self.session.ui;
        let r#box = ui.develop_box()?;
        let wanted = self.wanted_preview()?;
        let image = self
            .developed
            .as_ref()
            .filter(|(preview, _)| {
                preview.name == wanted.name
                    && preview.crop == wanted.crop
                    && preview.zoom == wanted.zoom
                    && (preview.box_w, preview.box_h) == (wanted.box_w, wanted.box_h)
            })
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
                Done::Prefetched { key, frame } => {
                    // The frame joins the cache when its roll is still held
                    // and there is room; either way the wants are recomputed,
                    // so the next neighbour is asked for and a develop that
                    // waited on this decode is planned from it.
                    if self
                        .held
                        .as_ref()
                        .is_some_and(|(roll, _)| *roll == key.roll)
                    {
                        self.memo.prefetched(key, frame.map(Arc::new));
                    }
                    self.wanted_at = None;
                }
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
    /// box (develop's or the single view's) at its sidecar's crop, exposure
    /// and look, or `None` when the model shows no box.
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
            zoom: ui.zoom(),
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
            (Some(want), Some((have, _))) if want == have || self.normalized(want, have) => None,
            (Some(want), _) => Some((want.clone(), self.memo.plan(want))),
            (None, _) => None,
        };
        // The neighbours' level 0, decoded ahead while the cache has room
        // for another frame without an eviction and the pool has a worker
        // to spare (on one worker a prefetch would hold the develop up):
        // the ones not yet cached, nearest first.
        let spare = self.pool.workers() >= MIN_PREFETCH_WORKERS;
        let prefetch = match &self.held {
            Some((roll, _)) if spare && self.memo.room_for_another() => ui
                .neighbours()
                .into_iter()
                .filter_map(|index| ui.photos().get(index))
                .map(|photo| PhotoKey {
                    roll: roll.clone(),
                    name: photo.name.clone(),
                })
                .filter(|key| self.memo.prefetchable(key))
                .collect(),
            _ => Vec::new(),
        };
        self.pool.want(keys, submit, prefetch)
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
    /// photos on screen (the grid's cells, or develop's filmstrip) and, in
    /// develop or the single view, the developed preview in its box, each
    /// centred and clipped to the grid's area, then the flag badges again
    /// over the thumbnails, within the area too.
    fn draw(&mut self) -> Result<()> {
        let generation = self.session.ui.generation();
        if self.submitted == Some(generation) || !self.client.can_present() {
            return Ok(());
        }
        let surface = self.session.ui.surface();
        let stride = surface.width * 4;
        let held_shift = self.held_shift();
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
                // A pan in progress shifts the held image by the pointer's
                // travel, within the box, and once released by how far the
                // held window lies from the one asked for (`held_shift`),
                // until the develop at the moved centre lands.
                let (dx, dy) = ui.pan_shift();
                let shifted = td_ui::raster::Rect {
                    x: r#box.x.saturating_add(dx).saturating_add(held_shift.0),
                    y: r#box.y.saturating_add(dy).saturating_add(held_shift.1),
                    ..r#box
                };
                ui::blit(pixels, surface, stride, r#box, shifted, image).map_err(error)?;
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
            zoom: None,
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
            pool.want(vec![key("a.NEF"), key("b.NEF")], None, Vec::new())
                .unwrap(),
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
                Done::Develop { .. } | Done::Export { .. } | Done::Prefetched { .. } => None,
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
    fn the_pool_runs_a_prefetch_outside_the_count_and_reports_a_failed_one() {
        let pool = Pool::start(2).unwrap();
        assert_eq!(
            pool.want(Vec::new(), None, vec![photo_key("missing.NEF")])
                .unwrap(),
            0
        );
        assert!(pool.prefetching().unwrap());
        let mut done = Vec::new();
        for _ in 0..2000 {
            assert_eq!(pool.outstanding().unwrap(), 0);
            done.extend(pool.collect().unwrap());
            if !done.is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        // The roll is not there, so the frame could not be made; the slot
        // was held until the collect and is free now.
        assert!(matches!(
            &done[..],
            [Done::Prefetched { key, frame: None }] if key.name == "missing.NEF"
        ));
        assert!(!pool.prefetching().unwrap());
    }

    #[test]
    fn the_pool_runs_the_develop_and_keeps_it_outstanding() {
        let pool = Pool::start(2).unwrap();
        assert_eq!(
            pool.want(
                Vec::new(),
                Some((preview("x.NEF"), Start::Decode)),
                Vec::new()
            )
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
    fn a_zoomed_develop_starts_at_level_1_with_the_frame_past_half() {
        let mut memo = Memo::default();
        let a = preview("a");
        let half = Preview {
            zoom: Some((develop::HALF_ZOOM, (5000, 5000))),
            ..a.clone()
        };
        let full = Preview {
            zoom: Some((100, (5000, 5000))),
            ..a.clone()
        };
        // Nothing held: a decode, which makes the fit levels and the
        // zoomed frame together.
        assert_eq!(memo.plan(&half).stage(), Stage::Decode);
        assert!(memo.merge(&a, made_decoded(4, 300), 1, true).is_some());
        // The photo current and its level 0 cached: a zoom start either
        // way, carrying the frame.
        assert_eq!(memo.plan(&half).stage(), Stage::Zoom(true));
        assert_eq!(memo.plan(&full).stage(), Stage::Zoom(true));
        // A zoom at another box or crop is still a zoom start: the fit
        // level 2 is not what it draws from.
        let wide = Preview {
            box_w: 600,
            ..half.clone()
        };
        assert_eq!(memo.plan(&wide).stage(), Stage::Zoom(true));
        // A zoomed result leaves the fit levels current: back to the fit
        // is level 3 alone.
        assert!(memo
            .merge(
                &full,
                Made::Zoom {
                    image: Rgb8 {
                        width: 1,
                        height: 1,
                        data: vec![0, 0, 0],
                    },
                },
                2,
                true
            )
            .is_some());
        assert_eq!(memo.plan(&a).stage(), Stage::Level3);
        // The level 0 evicted: through half the zoom runs from level 1
        // alone; past it the decode starts over.
        memo.raw.clear();
        memo.raw_bytes = 0;
        assert_eq!(memo.plan(&half).stage(), Stage::Zoom(false));
        assert_eq!(memo.plan(&full).stage(), Stage::Decode);
        // Another photo whose level 0 is cached but is not current: level
        // 1, which makes the fit levels and the zoomed frame from the
        // frame.
        let b = preview("b");
        assert!(memo.merge(&b, made_decoded(4, 300), 3, false).is_some());
        let b_full = Preview {
            zoom: Some((100, (0, 0))),
            ..b
        };
        assert_eq!(memo.plan(&b_full).stage(), Stage::Level1);
    }

    #[test]
    fn the_held_shift_is_the_centres_difference_as_pixels_and_zero_once_normalized() {
        // At 100 over 6000x4000 in a 600x400 box: a release that moved the
        // centre by a hundredth of each axis, right and up, shifts the held
        // image sixty right and forty up, the travel that made it.
        let extent = (6000, 4000);
        let r#box = (600, 400);
        assert_eq!(
            centre_shift((5000, 5000), (4900, 5100), 100, extent, r#box),
            (60, -40)
        );
        assert_eq!(
            centre_shift((4900, 5100), (5000, 5000), 100, extent, r#box),
            (-60, 40)
        );
        // At 50 a unit is half the pixels.
        assert_eq!(
            centre_shift((5000, 5000), (4900, 5100), 50, extent, r#box),
            (30, -20)
        );
        // A held centre outside the room (a photo switch landing before the
        // extent was known) names the clamped window: against the centre
        // the model then normalized it to, nothing shifts.
        let margin = 600 * 5000 / 6000;
        assert_eq!(
            ui::clamped_centre((9900, 5000), 100, extent, 600, 400),
            (10_000 - margin, 5000)
        );
        assert_eq!(
            centre_shift((9900, 5000), (10_000 - margin, 5000), 100, extent, r#box),
            (0, 0)
        );
        assert_eq!(centre_shift((0, 0), (0, 0), 100, extent, r#box), (0, 0));
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
    fn a_prefetch_is_taken_last_one_at_a_time_and_a_develop_waits_on_its_decode() {
        let mut queue = Queue::default();
        queue.replace_prefetch(vec![photo_key("b"), photo_key("c")]);
        // Not a job: nothing waits for it.
        assert_eq!(queue.outstanding(), 0);
        // Wanted work goes first, whatever is queued to prefetch: a
        // thumbnail, the develop, an export; only then a prefetch, and not
        // a second while one is in flight.
        queue.exports.push_back((export("e.NEF"), None));
        assert_eq!(
            queue.replace(vec![key("t")], Some((preview("a"), Start::Decode))),
            3
        );
        assert!(matches!(queue.take(), Some(Task::Thumb(_))));
        assert!(matches!(queue.take(), Some(Task::Develop(..))));
        assert!(matches!(queue.take(), Some(Task::Export(..))));
        match queue.take() {
            Some(Task::Prefetch(key)) => assert_eq!(key.name, "b"),
            _ => panic!("expected the prefetch"),
        }
        assert!(queue.take().is_none());
        assert_eq!(queue.outstanding(), 3);
        // The wants replaced keep the one in flight out of the queue.
        queue.replace_prefetch(vec![photo_key("b"), photo_key("c")]);
        let queued: Vec<&str> = queue.prefetch.iter().map(|k| k.name.as_str()).collect();
        assert_eq!(queued, ["c"]);
        // A pending develop that would decode the photo being prefetched
        // waits for it (a develop of a photo with cached levels does not);
        // the prefetch collected drops that plan and holds the next
        // prefetch back until the wants are replaced, so the window replans
        // the develop from the frame and a worker takes it first.
        queue.developing = None;
        queue.replace(Vec::new(), Some((preview("b"), Start::Decode)));
        assert!(queue.develop_waits());
        assert!(queue.take().is_none());
        // The thumbnail running, the export in flight and the develop
        // waiting count; the prefetch does not.
        assert_eq!(queue.outstanding(), 3);
        queue.prefetch_done();
        assert!(queue.preview.is_none() && queue.prefetching.is_none());
        assert!(queue.replan && queue.take().is_none());
        queue.replace(
            Vec::new(),
            Some((
                preview("b"),
                Start::Level1 {
                    raw: Arc::new(crate::RawFrame::synth(8)),
                },
            )),
        );
        assert!(!queue.replan && !queue.develop_waits());
        assert!(matches!(queue.take(), Some(Task::Develop(..))));
        match queue.take() {
            Some(Task::Prefetch(key)) => assert_eq!(key.name, "c"),
            _ => panic!("expected the next prefetch"),
        }
        // A prefetch collected with no develop waiting frees the slot alone.
        queue.prefetch_done();
        assert!(!queue.replan && queue.prefetching.is_none());
        assert!(queue.developing.is_some());
        // The photo the develop in flight is decoding is not prefetched
        // beside it: dropped, the next taken; the export's likewise.
        queue.replace_prefetch(vec![photo_key("b"), photo_key("d")]);
        match queue.take() {
            Some(Task::Prefetch(key)) => assert_eq!(key.name, "d"),
            _ => panic!("expected the prefetch past the develop's photo"),
        }
        queue.prefetch_done();
        queue.exporting = Some(crate::ExportRequest {
            path: PathBuf::from("/td-photo/no-such-roll").join("d"),
            exposure: 0,
            crop: None,
            look: None,
        });
        queue.replace_prefetch(vec![photo_key("d")]);
        assert!(queue.take().is_none() && queue.prefetch.is_empty());
        queue.exporting = None;
        // Nothing to prefetch while a thumbnail is pending, and none from a
        // closing queue.
        queue.replace_prefetch(vec![photo_key("d")]);
        queue.pending.push_back(key("v"));
        assert!(matches!(queue.take(), Some(Task::Thumb(_))));
        queue.closing = true;
        assert!(queue.take().is_none());
    }

    #[test]
    fn a_prefetched_frame_joins_the_cache_only_with_room_and_never_evicts() {
        let mut memo = Memo::default();
        let big = develop::RAW_CACHE_BYTES / 6 + 2_000_000;
        let frame = |bytes| Some(Arc::new(crate::RawFrame::synth(bytes)));
        assert!(memo.room_for_another() && memo.prefetchable(&photo_key("a")));
        assert!(memo.prefetched(photo_key("a"), frame(big)));
        assert!(!memo.prefetchable(&photo_key("a")), "held");
        assert!(memo.room_for_another());
        assert!(memo.prefetched(photo_key("b"), frame(big)));
        // Two of three fit; a third the size of the largest would not, so
        // none is asked for, and one that arrives anyway is dropped, the
        // held frames untouched, and not asked for again while the room it
        // needs is not there.
        assert!(!memo.room_for_another());
        assert!(!memo.prefetched(photo_key("c"), frame(big)));
        let held: Vec<&str> = memo.raw.iter().map(|c| c.key.name.as_str()).collect();
        assert_eq!(held, ["a", "b"]);
        assert!(!memo.prefetchable(&photo_key("c")));
        // A smaller one that would fit is still asked for; one that could
        // not decode never is.
        assert!(memo.prefetchable(&photo_key("d")));
        assert!(!memo.prefetched(photo_key("d"), None));
        assert!(!memo.prefetchable(&photo_key("d")));
        // One already held is left as it is, not added twice.
        assert!(memo.prefetched(photo_key("a"), frame(8)));
        assert_eq!(memo.raw.len(), 2);
        // A prefetched frame was never shown: a develop's decode evicts it
        // before one that was, and the room it frees lets the refused one
        // be asked for again.
        memo.touch_raw(&photo_key("a"), 5);
        memo.cache_raw(photo_key("e"), Arc::new(crate::RawFrame::synth(big)), 6);
        let held: Vec<&str> = memo.raw.iter().map(|c| c.key.name.as_str()).collect();
        assert_eq!(held, ["a", "e"]);
        assert!(!memo.prefetchable(&photo_key("c")));
        memo.drop_raw(&photo_key("e"));
        assert!(memo.prefetchable(&photo_key("c")));
        // Cleared with the memo.
        memo.clear();
        assert!(memo.prefetchable(&photo_key("d")));
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

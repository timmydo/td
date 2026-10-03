//! The analyzer in td-ui's widget window: the window drives the `App`
//! with its inputs and paints its frame; each turn hands the app's jobs to
//! the worker and the worker's replies back to the app.

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::time::Instant;

use td_ui::raster::{Raster, Scale, Surface};
use td_ui::window::{Clipboard, Flow, Handler, Input};

use td_dua::app::App;
use td_dua::worker::Worker;

/// How long a turn waits while a job is out, so the progress moves.
const BUSY_MS: u64 = 100;
/// How long an idle turn waits; input wakes the loop sooner.
const IDLE_MS: u64 = 1000;

struct Session {
    app: App,
    worker: Worker,
    /// The app's clock, read at every input as well as every poll, so a
    /// double click is timed when it happens rather than at the last poll.
    start: Instant,
}

impl Session {
    fn tick(&mut self) {
        let now = u64::try_from(self.start.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.app.tick(now);
    }

    fn flush(&mut self) -> Flow {
        for job in self.app.take_jobs() {
            if let Err(error) = self.worker.send(job) {
                eprintln!("td-dua: {error}");
                return Flow::Quit;
            }
        }
        if self.app.quitting() {
            Flow::Quit
        } else {
            Flow::Continue
        }
    }
}

impl Handler for Session {
    fn app_id(&self) -> &str {
        "td-dua"
    }

    fn title(&self) -> &str {
        self.app.title()
    }

    fn input(&mut self, input: Input<'_>, _clipboard: &mut dyn Clipboard) -> Flow {
        self.tick();
        self.app.input(input);
        self.flush()
    }

    fn poll(&mut self, _now: u64) -> Flow {
        self.tick();
        while let Some(reply) = self.worker.try_recv() {
            self.app.reply(reply);
        }
        let progress = self.worker.progress();
        self.app.progress(
            progress.entries.load(Ordering::Relaxed),
            progress.bytes.load(Ordering::Relaxed),
        );
        self.flush()
    }

    fn wait_ms(&self, _now: u64) -> u64 {
        if self.app.busy() {
            BUSY_MS
        } else {
            IDLE_MS
        }
    }

    fn needs_redraw(&self) -> bool {
        self.app.needs_redraw()
    }

    fn paint(&mut self, raster: &mut Raster<'_, '_>, surface: Surface) -> Result<(), String> {
        self.app.paint(raster, surface)
    }

    fn notice(&mut self, message: &str) {
        eprintln!("td-dua: window: {message}");
    }

    fn keys(&self) -> Vec<td_ui::keys::Section> {
        self.app.key_list()
    }
}

/// Runs the analyzer over `root` on the compositor the environment names
/// until its window closes.
pub fn run(root: PathBuf) -> Result<(), String> {
    let endpoint = td_ui::wayland::endpoint(
        std::env::var_os("WAYLAND_SOCKET"),
        std::env::var_os("WAYLAND_DISPLAY"),
        std::env::var_os("XDG_RUNTIME_DIR"),
    )?;
    let stream = td_ui::wayland::connect(endpoint)?;
    let surface = Surface::new(
        td_ui::window::DEFAULT_WIDTH,
        td_ui::window::DEFAULT_HEIGHT,
        Scale::default(),
    )
    .map_err(|error| error.to_string())?;
    let mut session = Session {
        app: App::new(root, surface),
        worker: Worker::start()?,
        start: Instant::now(),
    };
    session.flush();
    let typeface = td_ui::pinned_face::load_or_note(
        "td-dua",
        std::env::var_os(td_ui::pinned_face::SETTING).as_deref(),
    );
    td_ui::window::run(&mut session, stream, std::env::temp_dir(), typeface)
}

/// Scans `root` on this thread and writes the window's first frame at
/// `width` by `height` as a binary PPM, in the bitmap face, with the
/// entries along the path `select` (relative to `root`) opened and the
/// last of them selected.
pub fn preview(
    root: PathBuf,
    width: usize,
    height: usize,
    select: Option<PathBuf>,
) -> Result<Vec<u8>, String> {
    let surface =
        Surface::new(width, height, Scale::default()).map_err(|error| error.to_string())?;
    let mut app = App::new(root, surface);
    let progress = td_dua::scan::Progress::default();
    loop {
        let jobs = app.take_jobs();
        if jobs.is_empty() {
            break;
        }
        for job in jobs {
            app.reply(td_dua::worker::run(job, &progress));
        }
    }
    if app.tree().is_none() {
        return Err(app.message().to_owned());
    }
    if let Some(select) = select {
        if let Some(id) = find(&app, &select) {
            app.reveal(id);
        }
    }
    app.prepare();
    let frame = td_ui::driven::paint(&app.frame()).map_err(|error| error.to_string())?;
    Ok(frame.ppm())
}

/// The node at a path relative to the scanned root.
fn find(app: &App, relative: &std::path::Path) -> Option<td_dua::tree::NodeId> {
    let tree = app.tree()?;
    let mut at = td_dua::tree::ROOT;
    for part in relative
        .components()
        .filter(|part| !matches!(part, std::path::Component::CurDir))
    {
        let node = tree.get(at)?;
        at = *node.children.iter().find(|child| {
            tree.get(**child)
                .is_some_and(|c| c.name == part.as_os_str())
        })?;
    }
    Some(at)
}

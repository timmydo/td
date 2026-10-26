//! The manager in td-ui's widget window: the window drives the `App` with
//! its inputs and paints its frame; each turn hands the app's requests to
//! the worker and the worker's answers back to the app.

use std::path::PathBuf;
use std::time::Instant;

use td_ui::raster::{Raster, Scale, Surface};
use td_ui::window::{Clipboard, Flow, Handler, Input};

use td_software::app::App;
use td_software::registry::{self, Application};
use td_software::worker::Worker;

/// The environment variable naming the compositor's control socket.
const CONTROL_SOCKET: &str = "TD_CONTROL_SOCKET";

struct Session {
    app: App,
    worker: Worker,
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
                eprintln!("td-software: {error}");
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
        "td-software"
    }

    fn title(&self) -> &str {
        self.app.title()
    }

    fn input(&mut self, input: Input<'_>, _clipboard: &mut dyn Clipboard) -> Flow {
        self.app.input(input);
        self.flush()
    }

    fn poll(&mut self, _now: u64) -> Flow {
        loop {
            match self.worker.try_recv() {
                Ok(Some(reply)) => self.app.reply(reply),
                Ok(None) => break,
                Err(error) => {
                    eprintln!("td-software: {error}");
                    return Flow::Quit;
                }
            }
        }
        self.tick();
        self.flush()
    }

    fn wait_ms(&self, _now: u64) -> u64 {
        self.app.wait_ms()
    }

    fn needs_redraw(&self) -> bool {
        self.app.needs_redraw()
    }

    fn paint(&mut self, raster: &mut Raster<'_, '_>, surface: Surface) -> Result<(), String> {
        self.app.paint(raster, surface)
    }

    fn notice(&mut self, message: &str) {
        eprintln!("td-software: window: {message}");
    }

    fn keys(&self) -> Vec<td_ui::keys::Section> {
        self.app.key_list()
    }

    fn take_show_keys(&mut self) -> bool {
        self.app.take_show_keys()
    }
}

/// At most `MAX_TABLE_BYTES` of an image table, as text.
fn table(path: &str) -> Result<String, String> {
    let limit = u64::try_from(registry::MAX_TABLE_BYTES).unwrap_or(u64::MAX);
    let bytes = td_fs::read_bounded(std::path::Path::new(path), limit)
        .map_err(|error| format!("cannot read {error}"))?;
    String::from_utf8(bytes).map_err(|_| format!("{path} is not text"))
}

/// The image's applications, or why there are none to show.
fn applications() -> (Vec<Application>, Option<String>) {
    // Display names only: without them the names stand.
    let launcher = match table(registry::LAUNCHER_TABLE) {
        Ok(text) => Some(text),
        Err(error) => {
            eprintln!("td-software: {error}");
            None
        }
    };
    match table(registry::REGISTRY)
        .and_then(|text| registry::applications(&text, launcher.as_deref()))
    {
        Ok(applications) => (applications, None),
        Err(error) => (Vec::new(), Some(error)),
    }
}

fn surface(width: usize, height: usize) -> Result<Surface, String> {
    Surface::new(width, height, Scale::default()).map_err(|error| error.to_string())
}

/// Runs the manager on the compositor the environment names until its
/// window closes.
pub fn run() -> Result<(), String> {
    let endpoint = td_ui::wayland::endpoint(
        std::env::var_os("WAYLAND_SOCKET"),
        std::env::var_os("WAYLAND_DISPLAY"),
        std::env::var_os("XDG_RUNTIME_DIR"),
    )?;
    let stream = td_ui::wayland::connect(endpoint)?;
    let socket = std::env::var_os(CONTROL_SOCKET)
        .filter(|path| !path.is_empty())
        .map(PathBuf::from);
    let (applications, problem) = applications();
    let mut session = Session {
        app: App::new(
            applications,
            problem,
            surface(td_ui::window::DEFAULT_WIDTH, td_ui::window::DEFAULT_HEIGHT)?,
        ),
        worker: Worker::start(socket)?,
        start: Instant::now(),
    };
    session.tick();
    session.flush();
    let typeface = td_ui::pinned_face::load_or_note(
        "td-software",
        std::env::var_os(td_ui::pinned_face::SETTING).as_deref(),
    );
    td_ui::window::run(&mut session, stream, std::env::temp_dir(), typeface)
}

/// The window's first frame at `width` by `height` as a binary PPM, in
/// the bitmap face, before any status is read.
pub fn preview(width: usize, height: usize) -> Result<Vec<u8>, String> {
    let (applications, problem) = applications();
    let app = App::new(applications, problem, surface(width, height)?);
    let frame = td_ui::driven::paint(&app.frame()).map_err(|error| error.to_string())?;
    Ok(frame.ppm())
}

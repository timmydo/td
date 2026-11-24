//! The panel in td-ui's widget window: the window drives the `App` with
//! its inputs and paints its frame; each poll hands the app's actions to
//! the worker and the worker's replies back to the app.

use std::io::Write;
use std::path::Path;

use td_mixer::app::App;
use td_mixer::worker::Worker;
use td_ui::raster::{Raster, Scale, Surface};
use td_ui::window::{Clipboard, Flow, Handler, Input};

struct Session {
    app: App,
    worker: Worker,
}

impl Session {
    fn complain(&self, error: &dyn std::fmt::Display) {
        let _ = writeln!(std::io::stderr(), "td-mixer: {error}");
    }

    fn flush(&mut self) -> Flow {
        let worker = &self.worker;
        if let Err(gone) = self.app.flush(|job| worker.send(job)) {
            self.complain(&gone);
            return Flow::Quit;
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
        "td-mixer"
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
                Err(gone) => {
                    self.complain(&gone);
                    return Flow::Quit;
                }
            }
        }
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
        let _ = writeln!(std::io::stderr(), "td-mixer: window: {message}");
    }

    fn keys(&self) -> Vec<td_ui::keys::Section> {
        self.app.key_list()
    }

    fn take_show_keys(&mut self) -> bool {
        self.app.take_show_keys()
    }
}

fn surface(width: usize, height: usize) -> Result<Surface, String> {
    Surface::new(width, height, Scale::default())
        .map_err(|error| td_alloc::format(format_args!("{error}")).unwrap_or_default())
}

/// Runs the panel on the compositor the environment names, against
/// td-audio's socket, until its window closes.
pub fn run() -> Result<(), String> {
    let endpoint = td_ui::wayland::endpoint(
        std::env::var_os("WAYLAND_SOCKET"),
        std::env::var_os("WAYLAND_DISPLAY"),
        std::env::var_os("XDG_RUNTIME_DIR"),
    )?;
    let stream = td_ui::wayland::connect(endpoint)?;
    let worker = Worker::start(Path::new(td_mixer::SOCKET))
        .map_err(|error| td_alloc::format(format_args!("{error}")).unwrap_or_default())?;
    let mut session = Session {
        app: App::new(surface(WIDTH, HEIGHT)?),
        worker,
    };
    let typeface = td_ui::pinned_face::load_or_note(
        "td-mixer",
        std::env::var_os(td_ui::pinned_face::SETTING).as_deref(),
    );
    td_ui::window::run(&mut session, stream, std::env::temp_dir(), typeface)
}

/// The panel's first size: narrower and shorter than a document window.
pub const WIDTH: usize = 560;
pub const HEIGHT: usize = 420;

/// The window's first frame at `width` by `height` as a binary PPM, in
/// the bitmap face, before the daemon is asked anything.
pub fn preview(width: usize, height: usize) -> Result<Vec<u8>, String> {
    let app = App::new(surface(width, height)?);
    let frame = td_ui::driven::paint(&app.frame())
        .map_err(|error| td_alloc::format(format_args!("{error}")).unwrap_or_default())?;
    Ok(frame.ppm())
}

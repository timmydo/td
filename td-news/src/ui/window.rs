//! The reader in a window: td-ui's widget window drives the `App` with
//! its inputs, polls the backend's channel each turn and presents the
//! frame the app paints. The window owns the Wayland connection; the app
//! owns every view and the document pane.

use std::sync::mpsc;

use td_ui::raster::{Raster, Surface};
use td_ui::window::{Clipboard, Flow, Handler, Input};

use super::App;
use crate::backend::{BackendCommand, BackendResponse};
use crate::cache::Cache;
use crate::config::Config;

/// The pace the terminal loop read its channel at, kept as the longest a
/// turn waits for the backend's answer; the window's own idle wait is
/// shorter still.
const POLL_MS: u64 = 120;

struct Session<'a> {
    app: App,
    cache: &'a Cache,
    cmd_tx: &'a mpsc::Sender<BackendCommand>,
    resp_rx: &'a mpsc::Receiver<BackendResponse>,
}

impl Handler for Session<'_> {
    fn app_id(&self) -> &str {
        "td-news"
    }

    fn title(&self) -> &str {
        "Timmy's News"
    }

    fn input(&mut self, input: Input<'_>, _clipboard: &mut dyn Clipboard) -> Flow {
        if self.app.input(input, self.cache, self.cmd_tx) {
            Flow::Quit
        } else {
            Flow::Continue
        }
    }

    fn poll(&mut self, now: u64) -> Flow {
        // A backend that is gone answers nothing more; the reader stays up
        // on what it holds, as the terminal loop did, until the user quits.
        while let Ok(response) = self.resp_rx.try_recv() {
            self.app.handle_backend(response, self.cache);
        }
        self.app.tick(now);
        if self.app.quitting {
            Flow::Quit
        } else {
            Flow::Continue
        }
    }

    fn wait_ms(&self, _now: u64) -> u64 {
        POLL_MS
    }

    fn needs_redraw(&self) -> bool {
        self.app.pending_redraw
    }

    fn paint(&mut self, raster: &mut Raster<'_, '_>, surface: Surface) -> Result<(), String> {
        self.app.surface = surface;
        self.app.paint(raster)
    }

    fn notice(&mut self, message: &str) {
        crate::log::error(format!("window: {message}"));
    }
}

/// Runs the reader in a window on the compositor the environment names,
/// until the window closes or the app quits.
pub fn run(
    config: &Config,
    cache: &Cache,
    cmd_tx: &mpsc::Sender<BackendCommand>,
    resp_rx: &mpsc::Receiver<BackendResponse>,
    offline: bool,
) -> Result<(), String> {
    let endpoint = td_ui::wayland::endpoint(
        std::env::var_os("WAYLAND_SOCKET"),
        std::env::var_os("WAYLAND_DISPLAY"),
        std::env::var_os("XDG_RUNTIME_DIR"),
    )?;
    let stream = td_ui::wayland::connect(endpoint)?;
    let mut session = Session {
        app: App::new(config, cache, offline)?,
        cache,
        cmd_tx,
        resp_rx,
    };
    td_ui::window::run(&mut session, stream, std::env::temp_dir())
}

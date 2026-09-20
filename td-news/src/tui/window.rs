//! The reader in a window: td-ui's screen window drives the `App` with
//! the inputs it translates, polls the backend's channel each turn and
//! presents the frame the app draws. The window owns the Wayland
//! connection; the app owns every view.

use std::sync::mpsc;

use td_ui::screen::{Input, Screen, Style};
use td_ui::screen_app::{Flow, Handler};

use super::input::{self, InputEvent};
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
    /// Reused per input so a wheel frame allocates nothing.
    events: Vec<InputEvent>,
}

impl Handler for Session<'_> {
    fn title(&self) -> &str {
        "Timmy's News"
    }

    fn app_id(&self) -> &str {
        "td-news"
    }

    fn ground(&self) -> Style {
        self.app.theme.base
    }

    fn input(&mut self, input: Input) -> Flow {
        match input {
            Input::Close => return Flow::Quit,
            Input::Resize { .. } => {
                self.app.pending_redraw = true;
                return Flow::Continue;
            }
            _ => {}
        }
        self.events.clear();
        input::translate(input, self.app.mouse_config, &mut self.events);
        let events = std::mem::take(&mut self.events);
        let mut flow = Flow::Continue;
        for event in &events {
            if self.app.handle_input(*event, self.cache, self.cmd_tx) {
                flow = Flow::Quit;
                break;
            }
        }
        self.events = events;
        flow
    }

    fn poll(&mut self, _now: u64) -> Flow {
        // A backend that is gone answers nothing more; the reader stays up
        // on what it holds, as the terminal loop did, until the user quits.
        while let Ok(response) = self.resp_rx.try_recv() {
            self.app.handle_backend(response, self.cache);
        }
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

    fn render(&mut self, screen: &mut Screen) {
        self.app.draw(screen);
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
        app: App::new(config, cache, offline),
        cache,
        cmd_tx,
        resp_rx,
        events: Vec::new(),
    };
    td_ui::screen_app::run(&mut session, stream, std::env::temp_dir())
}

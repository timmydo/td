//! The notebook in td-ui's widget window: the window drives the `App`
//! with its inputs and paints its frame; each turn hands the app's
//! commands to the vault's thread and its answers back to the app.

use std::path::PathBuf;

use td_ui::raster::{Raster, Surface};
use td_ui::window::{Clipboard, Flow, Handler, Input};

use crate::app::{App, Out};
use crate::backend::Client;

/// The longest a turn waits for the vault's answer; the caret's blink is
/// paced by the same turns.
const POLL_MS: u64 = 50;

struct Session {
    app: App,
    client: Client,
}

impl Session {
    fn flush(&mut self) -> Flow {
        for out in self.app.take_out() {
            match out {
                Out::Send(command) => self.client.send(command),
                Out::Answer(op, answer) => self.client.answer(op, answer),
                Out::Cancel => self.client.cancel(),
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
        "td-pass"
    }

    // The window's title names no entry: the compositor and every task
    // list show it, locked or not.
    fn title(&self) -> &str {
        "td-pass"
    }

    fn input(&mut self, input: Input<'_>, clipboard: &mut dyn Clipboard) -> Flow {
        self.app.input(input, clipboard);
        self.flush()
    }

    fn poll(&mut self, now: u64) -> Flow {
        while let Some(reply) = self.client.try_recv() {
            self.app.reply(reply);
        }
        self.app.tick(now);
        self.flush()
    }

    fn wait_ms(&self, _now: u64) -> u64 {
        POLL_MS
    }

    fn needs_redraw(&self) -> bool {
        self.app.needs_redraw()
    }

    fn paint(&mut self, raster: &mut Raster<'_, '_>, surface: Surface) -> Result<(), String> {
        self.app.paint(raster, surface)
    }

    fn take_withdrawal(&mut self) -> bool {
        self.app.take_withdrawal()
    }

    fn take_scrub(&mut self) -> bool {
        self.app.take_scrub()
    }

    fn notice(&mut self, message: &str) {
        eprintln!("td-pass: window: {message}");
    }
}

/// The memory-backed directory the window's frames are kept in: the
/// session's runtime directory, else `/dev/shm`. Neither on tmpfs or
/// ramfs refuses the window rather than draw secrets into a disk file.
fn frame_directory() -> Result<PathBuf, String> {
    let mountinfo = std::fs::read_to_string("/proc/self/mountinfo")
        .map_err(|error| format!("td-pass cannot read the mount table: {error}"))?;
    let candidates = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .into_iter()
        .chain(std::iter::once(PathBuf::from("/dev/shm")));
    crate::frames::choose(
        &mountinfo,
        candidates.filter_map(|path| std::fs::canonicalize(path).ok()),
    )
    .ok_or_else(|| {
        "td-pass draws only into memory: neither XDG_RUNTIME_DIR nor /dev/shm is on tmpfs"
            .to_owned()
    })
}

/// Runs the notebook on the compositor the environment names until its
/// window closes.
pub fn run() -> Result<(), String> {
    let frames = frame_directory()?;
    let endpoint = td_ui::wayland::endpoint(
        std::env::var_os("WAYLAND_SOCKET"),
        std::env::var_os("WAYLAND_DISPLAY"),
        std::env::var_os("XDG_RUNTIME_DIR"),
    )?;
    let stream = td_ui::wayland::connect(endpoint)?;
    let mut session = Session {
        app: App::new()?,
        client: crate::backend::start()?,
    };
    let typeface = td_ui::pinned_face::load_or_note(
        "td-pass",
        std::env::var_os(td_ui::pinned_face::SETTING).as_deref(),
    );
    let result = td_ui::window::run(&mut session, stream, frames, typeface);
    // Dropping the client ends the vault's thread and the vault with it.
    drop(session);
    result
}

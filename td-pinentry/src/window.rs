//! One prompt in td-ui's widget window on the session's compositor: the
//! window drives the dialog with its inputs and clock, watches the
//! pinentry caller for hanging up, and closes once the prompt is
//! answered.

use std::path::PathBuf;

use td_ui::raster::{Raster, Scale, Surface};
use td_ui::window::{Clipboard, Flow, Handler, Input, DEFAULT_HEIGHT, DEFAULT_WIDTH};

use crate::app::Dialog;
use crate::assuan::Inbox;
use crate::request::{Answer, Request};

struct Session<'a> {
    dialog: Dialog,
    /// The pinentry caller's lines; askpass has no lines to watch.
    inbox: Option<&'a mut Inbox>,
    /// The process that started this one, as it was when the window
    /// opened: a caller killed meanwhile leaves this process to another
    /// parent, and its answer to nobody.
    parent: u32,
}

impl Handler for Session<'_> {
    fn app_id(&self) -> &str {
        "td-pinentry"
    }

    fn title(&self) -> &str {
        self.dialog.title()
    }

    fn input(&mut self, input: Input<'_>, clipboard: &mut dyn Clipboard) -> Flow {
        self.dialog.input(input, clipboard)
    }

    fn poll(&mut self, now: u64) -> Flow {
        if self.inbox.as_mut().is_some_and(|inbox| inbox.hung_up()) {
            return self.dialog.hang_up();
        }
        if std::os::unix::process::parent_id() != self.parent {
            return self.dialog.hang_up();
        }
        self.dialog.tick(now)
    }

    fn wait_ms(&self, now: u64) -> u64 {
        self.dialog.wait_ms(now)
    }

    fn needs_redraw(&self) -> bool {
        self.dialog.needs_redraw()
    }

    fn paint(&mut self, raster: &mut Raster<'_, '_>, surface: Surface) -> Result<(), String> {
        self.dialog.paint(raster, surface)
    }

    fn take_scrub(&mut self) -> bool {
        self.dialog.take_scrub()
    }

    fn keys(&self) -> Vec<td_ui::keys::Section> {
        self.dialog.keys()
    }

    fn notice(&mut self, message: &str) {
        let logged = self.dialog.notice(message);
        eprintln!("td-pinentry: window: {logged}");
    }
}

/// Shows `request` and waits for its answer; a window that cannot open
/// is `Answer::Failed`, saying why.
pub fn ask(request: Request, inbox: Option<&mut Inbox>) -> Answer {
    open(request, inbox).unwrap_or_else(Answer::Failed)
}

fn open(request: Request, inbox: Option<&mut Inbox>) -> Result<Answer, String> {
    let frames = frame_directory()?;
    // A compositor connection handed down as WAYLAND_SOCKET was its
    // receiver's, and an agent or a client starts this program, so only
    // the display is read; each prompt connects anew.
    let endpoint = td_ui::wayland::endpoint(
        None,
        std::env::var_os("WAYLAND_DISPLAY"),
        std::env::var_os("XDG_RUNTIME_DIR"),
    )
    .map_err(|error| format!("no Wayland display to ask on: {error}"))?;
    let stream = td_ui::wayland::connect(endpoint)
        .map_err(|error| format!("cannot reach the Wayland display: {error}"))?;
    let surface = Surface::new(DEFAULT_WIDTH, DEFAULT_HEIGHT, Scale::default())
        .map_err(|error| format!("the window's first surface: {error}"))?;
    let mut session = Session {
        dialog: Dialog::new(request, surface)?,
        inbox,
        parent: std::os::unix::process::parent_id(),
    };
    let typeface = td_ui::pinned_face::load_or_note(
        "td-pinentry",
        std::env::var_os(td_ui::pinned_face::SETTING).as_deref(),
    );
    td_ui::window::run(&mut session, stream, frames, typeface)?;
    Ok(session.dialog.take_answer().unwrap_or(Answer::Cancelled))
}

/// Where the window's frames are kept: the session's runtime directory,
/// else `/dev/shm`. A frame shows a mask glyph for each character of a
/// secret, never the character, so it holds the secret's length alone.
fn frame_directory() -> Result<PathBuf, String> {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute() && path.is_dir())
        .or_else(|| Some(PathBuf::from("/dev/shm")).filter(|path| path.is_dir()))
        .ok_or_else(|| "neither XDG_RUNTIME_DIR nor /dev/shm is a directory".to_owned())
}

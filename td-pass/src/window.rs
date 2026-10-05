//! The notebook in td-ui's widget window: the window drives the `App`
//! with its inputs and paints its frame; each turn hands the app's
//! commands to the vault's thread and its answers back to the app.

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};

use td_ui::finder;
use td_ui::raster::{Raster, Surface};
use td_ui::window::{Clipboard, Flow, Handler, Input};

use crate::app::{App, Out};
use crate::backend::{Client, Host};

/// The longest a turn waits for the vault's answer; the caret's blink is
/// paced by the same turns.
const POLL_MS: u64 = 50;

struct Session {
    app: App,
    client: Client,
    host: Host,
    lister: Sender<Ask>,
    listings: Receiver<Listed>,
}

/// A folder the finder asked for.
struct Ask {
    chooser: u64,
    folder: Option<PathBuf>,
    select: Option<String>,
    files: bool,
    store: bool,
}

/// The answer: the folder listed, its listing or why not, and the entry
/// to select.
struct Listed {
    chooser: u64,
    folder: PathBuf,
    listing: Result<finder::Listing, String>,
    select: Option<String>,
}

/// Lists folders for the finder on a thread of its own, so a folder slow
/// to read (a network or automounted one) never holds the window: it can
/// still lock. The thread ends when the window's end is dropped; one
/// stuck in a read is left to the process's exit.
fn start_lister() -> Result<(Sender<Ask>, Receiver<Listed>), String> {
    let (asks, ask_rx) = mpsc::channel::<Ask>();
    let (listed, listings) = mpsc::channel();
    std::thread::Builder::new()
        .name("lister".to_owned())
        .spawn(move || {
            while let Ok(ask) = ask_rx.recv() {
                let folder = ask.folder.unwrap_or_else(|| {
                    if ask.store {
                        crate::files::store_folder()
                    } else {
                        crate::files::start_folder()
                    }
                });
                let ceiling = ask.files.then_some(crate::protocol::MAX_COPY as u64);
                let listing = crate::files::list_folder(&folder, ceiling);
                let answer = Listed {
                    chooser: ask.chooser,
                    folder,
                    listing,
                    select: ask.select,
                };
                if listed.send(answer).is_err() {
                    break;
                }
            }
        })
        .map_err(|error| format!("td-pass cannot start its folder lister: {error}"))?;
    Ok((asks, listings))
}

impl Session {
    fn flush(&mut self) -> Flow {
        for out in self.app.take_out() {
            match out {
                Out::Send(command) => self.client.send(command),
                Out::Answer(op, answer) => self.client.answer(op, answer),
                Out::Cancel => self.client.cancel(),
                Out::List {
                    chooser,
                    folder,
                    select,
                    files,
                    store,
                } => {
                    // A lister that is gone answers nothing; the finder
                    // shows what it has.
                    let _ = self.lister.send(Ask {
                        chooser,
                        folder,
                        select,
                        files,
                        store,
                    });
                }
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
        while let Some(event) = self.host.try_next() {
            self.app.host(event);
        }
        while let Ok(listed) = self.listings.try_recv() {
            self.app.listed(
                listed.chooser,
                listed.folder,
                listed.listing,
                listed.select.as_deref(),
            );
        }
        self.app.tick(now);
        let flow = self.flush();
        // Sleep waits until the vault's thread has dropped the vault.
        if self.app.settled() {
            self.host.release();
        }
        flow
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

    fn keys(&self) -> Vec<td_ui::keys::Section> {
        self.app.key_list()
    }

    // td-pass has no control socket: every input is the live pointer's
    // or the physical keyboard's.
    fn take_show_keys(&mut self) -> bool {
        self.app.take_key_list_asked()
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
    let (lister, listings) = start_lister()?;
    let mut session = Session {
        app: App::new()?,
        client: crate::backend::start()?,
        host: crate::backend::watch_host(),
        lister,
        listings,
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

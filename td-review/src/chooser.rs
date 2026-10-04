//! `--choose-repo`: a window that picks the repository before the review
//! window opens, for a start with no work tree to begin in, as from a
//! desktop launcher. It lists the saved repositories (`saved.rs`) and a
//! Browse row into td-ui's shared directory finder, from `~/src`. A folder
//! the opener accepts closes the chooser; anything else stays, with the
//! reason on the status row.
//!
//! The chooser itself runs nothing: a folder is marked a repository when it
//! holds `.git`. Browsing, Return only ever enters a folder, since the mark
//! is the folder's own state, which an application under `~/src` can set;
//! a repository opens on Ctrl+Return in it, or from the saved list. What a
//! folder chosen runs is the opener's (`main.rs`), which bounds git's
//! discovery to that folder.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Instant;

use td_ui::chrome::{Status, ROW};
use td_ui::finder::{self, Choose, Entry, Kind, Listing};
use td_ui::pointer::DoubleClick;
use td_ui::raster::{
    text_run, Composition, Draw, GlyphStyle, Primitive, Raster, Rect, Surface, BORDER, CHROME, INK,
    LINE_NUMBER,
};
use td_ui::window::{Clipboard, Flow, Handler, Input, PointerPhase};
use td_ui::{CELL_HEIGHT, CELL_WIDTH};

use crate::view;

/// Band tops in font pixels: the title over a hairline rule, a hint line,
/// the finder from `FINDER_TOP` to the legend's row at the foot.
const INSET_X: usize = CELL_WIDTH;
const TITLE_Y: usize = 4;
const RULE_Y: usize = ROW;
const HINT_Y: usize = ROW + 4;
const FINDER_TOP: usize = 2 * ROW;
/// The smallest surface the finder lays out on, in font pixels; a smaller
/// one is painted clipped from one this size.
const MIN_WIDTH: usize = (finder::MIN_COLUMNS + 2) * CELL_WIDTH;
const MIN_HEIGHT: usize = FINDER_TOP + 5 * ROW;

/// The most bytes of names one browsed folder lists, the most folders, and
/// the most entries of any kind read to find them.
const LISTED_BYTES: usize = finder::LISTING_BYTES / 2;
const LISTED: usize = finder::ENTRIES;
const SCANNED: usize = 16 * finder::ENTRIES;

/// The scrollbar gutter at the right of td-ui's `chrome::List`, in font
/// pixels: inside the list's rectangle, but no row's.
const LIST_GUTTER: usize = 16;

const SAVED_TITLE: &str = "Saved repositories";
const BROWSE: &str = "Browse…";
const REPOSITORY: &str = "git";

/// What an input asks of the window.
#[derive(Debug, PartialEq, Eq)]
pub enum Step {
    /// Nothing beyond a repaint.
    Continue,
    /// Open this folder, if the opener accepts it; `refuse` says why not.
    Open(PathBuf),
    /// This saved repository was dropped from the list.
    Forget(PathBuf),
    Quit,
}

#[derive(Debug, PartialEq, Eq)]
enum Mode {
    Saved,
    Browse(PathBuf),
}

/// The chooser's state: the saved list, which listing the finder shows,
/// and the finder itself.
pub struct Chooser {
    saved: Vec<PathBuf>,
    home: Option<PathBuf>,
    /// Where browsing begins.
    start: PathBuf,
    mode: Mode,
    /// Whether the folder browsed holds `.git`, for the hint.
    here: bool,
    finder: finder::Controller,
    surface: Surface,
    /// Paired by listing index; cancelled with every new listing.
    clicks: DoubleClick<usize>,
}

/// A path as a row shows it: under the home as `~/…`, a control scalar as
/// `?`, cut to `limit` bytes on a scalar boundary.
fn shown(path: &Path, home: Option<&Path>, limit: usize) -> String {
    let relative = home
        .filter(|home| home.parent().is_some())
        .and_then(|home| path.strip_prefix(home).ok());
    let text = match relative {
        Some(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Some(rest) => format!("~/{}", rest.to_string_lossy()),
        None => path.to_string_lossy().into_owned(),
    };
    let mut out = String::with_capacity(text.len().min(limit));
    for c in text.chars().map(|c| if c.is_control() { '?' } else { c }) {
        if out.len() + c.len_utf8() > limit {
            break;
        }
        out.push(c);
    }
    out
}

/// The surface the finder lays out on: `surface`, or the minimum where it
/// is under it, at its scale; none where that cannot be a surface.
fn padded(surface: Surface) -> Option<Surface> {
    let s = surface.scale.value();
    Surface::new(
        surface.width.max(MIN_WIDTH * s),
        surface.height.max(MIN_HEIGHT * s),
        surface.scale,
    )
    .ok()
}

/// The finder's rectangle: inside the inset, from `FINDER_TOP` to the
/// legend.
fn finder_rect(surface: Surface) -> Rect {
    let s = surface.scale.value();
    Rect {
        x: (INSET_X * s) as i64,
        y: (FINDER_TOP * s) as i64,
        width: surface.width.saturating_sub(2 * INSET_X * s) as u32,
        height: surface.height.saturating_sub((FINDER_TOP + ROW) * s) as u32,
    }
}

/// The saved list as the finder lists it, then the Browse row. A saved
/// folder that is gone is listed disabled, so it can still be forgotten.
fn saved_listing(saved: &[PathBuf], home: Option<&Path>) -> Result<Listing, finder::Error> {
    let mut entries = Vec::with_capacity(saved.len() + 1);
    for path in saved {
        let present = path.is_dir();
        let meta = if present { "saved" } else { "missing" };
        let name = shown(path, home, finder::NAME_BYTES);
        entries.push(Entry::new(&name, meta, Kind::Folder, present)?);
    }
    entries.push(Entry::new(BROWSE, "", Kind::Folder, true)?);
    Listing::new(SAVED_TITLE, entries, false)
}

/// The folders in `dir`, by name ignoring case, dot folders, names that
/// cannot be shown and entries that cannot be read left out, each marked
/// when it holds `.git`. Past the bounds the rest is left unlisted and the
/// listing says it was cut short.
fn browse_listing(dir: &Path, home: Option<&Path>) -> io::Result<Listing> {
    let mut folders: Vec<(String, bool)> = Vec::new();
    let mut bytes = 0usize;
    let mut truncated = false;
    for (scanned, entry) in fs::read_dir(dir)?.enumerate() {
        if scanned == SCANNED {
            truncated = true;
            break;
        }
        let Ok(entry) = entry else {
            continue;
        };
        // The kind readdir gave, so a plain file costs no stat; a link is
        // followed, as `cd` would.
        let folder = match entry.file_type() {
            Ok(kind) if kind.is_dir() => true,
            Ok(kind) if kind.is_symlink() => entry.path().is_dir(),
            _ => false,
        };
        if !folder {
            continue;
        }
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if name.starts_with('.')
            || name.len() > finder::NAME_BYTES
            || name.chars().any(char::is_control)
        {
            continue;
        }
        if folders.len() == LISTED || bytes + name.len() > LISTED_BYTES {
            truncated = true;
            break;
        }
        bytes += name.len();
        let repository = entry.path().join(".git").symlink_metadata().is_ok();
        folders.push((name, repository));
    }
    folders.sort_by_cached_key(|(name, _)| name.to_lowercase());
    let mut entries = Vec::with_capacity(folders.len());
    for (name, repository) in &folders {
        let meta = if *repository { REPOSITORY } else { "" };
        entries.push(Entry::new(name, meta, Kind::Folder, true).map_err(io::Error::other)?);
    }
    let label = shown(dir, home, finder::PATH_BYTES);
    Listing::new(&label, entries, truncated).map_err(io::Error::other)
}

/// Whether `dir` holds `.git`.
fn holds_git(dir: &Path) -> bool {
    dir.join(".git").symlink_metadata().is_ok()
}

impl Chooser {
    /// The chooser on `surface`: the saved list when there is one, else
    /// browsing from `start`; a `start` that cannot be read leaves the
    /// Browse row alone, saying why.
    pub fn new(
        saved: Vec<PathBuf>,
        home: Option<PathBuf>,
        start: PathBuf,
        surface: Surface,
    ) -> Result<Self, String> {
        let laid = padded(surface).unwrap_or(surface);
        let mut refused = None;
        let (mode, listing, here) = match saved.is_empty() {
            true => match browse_listing(&start, home.as_deref()) {
                Ok(listing) => (Mode::Browse(start.clone()), listing, holds_git(&start)),
                Err(e) => {
                    refused = Some(format!("{}: {e}", start.display()));
                    let listing =
                        saved_listing(&saved, home.as_deref()).map_err(|e| e.to_string())?;
                    (Mode::Saved, listing, false)
                }
            },
            false => {
                let listing = saved_listing(&saved, home.as_deref()).map_err(|e| e.to_string())?;
                (Mode::Saved, listing, false)
            }
        };
        let finder =
            finder::Controller::new(listing, Choose::Folder, laid, finder_rect(laid), None)
                .map_err(|e| format!("repository finder: {e}"))?;
        let mut chooser = Self {
            saved,
            home,
            start,
            mode,
            here,
            finder,
            surface,
            clicks: DoubleClick::default(),
        };
        if let Some(why) = refused {
            chooser.note(&why);
        }
        Ok(chooser)
    }

    /// Says on the status row why the folder asked for was not opened.
    pub fn refuse(&mut self, why: &str) {
        self.note(why);
    }

    fn note(&mut self, text: &str) {
        let line = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
        let _ = self
            .finder
            .set_note(&shown(Path::new(line.trim()), None, finder::NOTE_BYTES));
    }

    /// The hint over the finder.
    fn hint(&self) -> &'static str {
        match self.mode {
            Mode::Saved => "Open a saved repository, or browse for another.",
            Mode::Browse(_) if self.here => "This folder is a repository: Ctrl+Return opens it.",
            Mode::Browse(_) => {
                "Folders marked git are repositories: enter one, Ctrl+Return opens it."
            }
        }
    }

    /// The legend on the bottom row.
    fn legend(&self) -> &'static str {
        match self.mode {
            Mode::Saved => "MOVE  FILTER  ENTER OPEN  DEL FORGET  ESC QUIT",
            Mode::Browse(_) if self.saved.is_empty() => {
                "MOVE  FILTER  ENTER INTO  LEFT UP  C-ENTER OPEN THIS FOLDER  ESC QUIT"
            }
            Mode::Browse(_) => {
                "MOVE  FILTER  ENTER INTO  LEFT UP  C-ENTER OPEN THIS FOLDER  ESC BACK"
            }
        }
    }

    /// Lists the saved repositories, selecting the row named `select`.
    fn show_saved(&mut self, select: Option<&str>) -> Step {
        match saved_listing(&self.saved, self.home.as_deref()) {
            Ok(listing) => {
                if self.finder.set_listing(listing, select).is_ok() {
                    self.mode = Mode::Saved;
                    self.here = false;
                    self.clicks.cancel();
                }
            }
            Err(e) => self.note(&e.to_string()),
        }
        Step::Continue
    }

    /// Lists `dir`, selecting the folder named `select`; a folder that
    /// cannot be read leaves the listing as it was, saying why, and false.
    fn browse(&mut self, dir: PathBuf, select: Option<&str>) -> bool {
        match browse_listing(&dir, self.home.as_deref()) {
            Ok(listing) => {
                if self.finder.set_listing(listing, select).is_err() {
                    return false;
                }
                self.here = holds_git(&dir);
                self.mode = Mode::Browse(dir);
                self.clicks.cancel();
                true
            }
            Err(e) => {
                self.note(&format!("{}: {e}", shown(&dir, self.home.as_deref(), 512)));
                false
            }
        }
    }

    /// The folder at listing `index` of the browsed `dir`.
    fn child(&self, index: usize) -> Option<PathBuf> {
        let Mode::Browse(dir) = &self.mode else {
            return None;
        };
        let entry = self.finder.listing().entries().get(index)?;
        Some(dir.join(entry.name()))
    }

    /// Return on the row at listing `index`: a saved repository opens,
    /// Browse and any browsed folder, repository or not, is entered, and a
    /// disabled row does nothing, however it was reached.
    fn activate(&mut self, index: usize) -> Step {
        let enabled = self
            .finder
            .listing()
            .entries()
            .get(index)
            .is_some_and(Entry::enabled);
        if !enabled {
            return Step::Continue;
        }
        match self.mode {
            Mode::Saved => match self.saved.get(index) {
                Some(path) => Step::Open(path.clone()),
                None => {
                    self.browse(self.start.clone(), None);
                    Step::Continue
                }
            },
            Mode::Browse(_) => {
                if let Some(path) = self.child(index) {
                    self.browse(path, None);
                }
                Step::Continue
            }
        }
    }

    fn ascend(&mut self) -> Step {
        let Mode::Browse(dir) = &self.mode else {
            return Step::Continue;
        };
        let (Some(parent), Some(name)) = (dir.parent(), dir.file_name()) else {
            return Step::Continue;
        };
        let name = name.to_string_lossy().into_owned();
        self.browse(parent.to_path_buf(), Some(&name));
        Step::Continue
    }

    fn outcome(&mut self, outcome: finder::Outcome) -> Step {
        match outcome {
            finder::Outcome::Descend(index) => self.activate(index),
            finder::Outcome::Ascend => self.ascend(),
            // Accept and Escape are kept from the finder, so it never
            // closes; a resize it cannot lay out is not handed it.
            finder::Outcome::Closed(_) => Step::Quit,
            finder::Outcome::Ignored | finder::Outcome::Consumed | finder::Outcome::Changed => {
                Step::Continue
            }
        }
    }

    /// A key as td-ui's keymap spells it.
    pub fn key(&mut self, chord: &str, repeat: bool) -> Step {
        // Text for the filter, held or not.
        let mut scalars = chord.chars();
        if let (Some(c), None) = (scalars.next(), scalars.next()) {
            return self.insert(c);
        }
        let held = |key| (key, repeat);
        let (key, repeated) = match chord {
            "Up" => held(finder::Key::Up),
            "Down" => held(finder::Key::Down),
            "PageUp" => held(finder::Key::PageUp),
            "PageDown" => held(finder::Key::PageDown),
            "Home" => held(finder::Key::Home),
            "End" => held(finder::Key::End),
            "Backspace" => held(finder::Key::Backspace),
            _ if repeat => return Step::Continue,
            "Return" => (finder::Key::Activate, false),
            "Left" => (finder::Key::Parent, false),
            "Right" => {
                return match (&self.mode, self.finder.selected()) {
                    (Mode::Browse(_), Some(index)) => {
                        if let Some(path) = self.child(index) {
                            self.browse(path, None);
                        }
                        Step::Continue
                    }
                    // Into Browse; a saved repository opens on Return only.
                    (Mode::Saved, Some(index)) if index == self.saved.len() => self.activate(index),
                    _ => Step::Continue,
                };
            }
            "C-Return" => {
                return match (&self.mode, self.finder.selected()) {
                    (Mode::Browse(dir), _) => Step::Open(dir.clone()),
                    (Mode::Saved, Some(index)) => self.activate(index),
                    (Mode::Saved, None) => Step::Continue,
                };
            }
            "Escape" => return self.escape(),
            "Delete" => return self.forget(),
            _ => return Step::Continue,
        };
        let outcome = self.finder.event(finder::Event::Key { key, repeated });
        self.outcome(outcome)
    }

    /// A character for the filter; a control character is none.
    fn insert(&mut self, c: char) -> Step {
        if c.is_control() {
            return Step::Continue;
        }
        let outcome = self.finder.event(finder::Event::Insert(c));
        self.outcome(outcome)
    }

    /// Pasted text for the filter, up to its first line.
    pub fn paste(&mut self, text: &str) -> Step {
        for c in text.chars().take_while(|c| *c != '\n') {
            self.insert(c);
        }
        Step::Continue
    }

    /// Escape clears a filter, then leaves browsing for the saved list,
    /// then quits.
    fn escape(&mut self) -> Step {
        if !self.finder.query().is_empty() {
            while !self.finder.query().is_empty() {
                let backspace = finder::Event::Key {
                    key: finder::Key::Backspace,
                    repeated: true,
                };
                if self.finder.event(backspace) != finder::Outcome::Changed {
                    break;
                }
            }
            return Step::Continue;
        }
        match self.mode {
            Mode::Browse(_) if !self.saved.is_empty() => self.show_saved(None),
            _ => Step::Quit,
        }
    }

    /// Delete on a saved repository drops it from the list; with the last
    /// one gone the chooser browses, or keeps the Browse row alone.
    fn forget(&mut self) -> Step {
        if self.mode != Mode::Saved {
            return Step::Continue;
        }
        let Some(index) = self.finder.selected().filter(|i| *i < self.saved.len()) else {
            return Step::Continue;
        };
        let path = self.saved.remove(index);
        if self.saved.is_empty() && self.browse(self.start.clone(), None) {
            return Step::Forget(path);
        }
        // The row that took its place stays selected, else Browse.
        let next = match self.saved.get(index) {
            Some(next) => shown(next, self.home.as_deref(), finder::NAME_BYTES),
            None => BROWSE.to_string(),
        };
        self.show_saved(Some(&next));
        Step::Forget(path)
    }

    /// The listing index of the shown row under (`x`, `y`), if any.
    fn row_at(&self, x: i64, y: i64) -> Option<usize> {
        let list = self.finder.list_rect();
        let s = self.surface.scale.value();
        let rows = Rect {
            width: list.width.saturating_sub((LIST_GUTTER * s) as u32),
            ..list
        };
        if !rows.contains(x, y) {
            return None;
        }
        let height = (ROW * s) as i64;
        let row = usize::try_from((y - list.y) / height).ok()?;
        let position = self.finder.first().checked_add(row)?;
        self.finder.shown().get(position).copied()
    }

    /// The left button: a press selects the row under it, and a second
    /// press on that row of the same listing soon after is Return on it.
    pub fn pointer(&mut self, phase: PointerPhase, x: i64, y: i64, now_ns: u64) -> Step {
        if phase != PointerPhase::Press {
            return Step::Continue;
        }
        let row = self.row_at(x, y);
        let outcome = self.finder.event(finder::Event::Press { x, y });
        // Only a press that selected its row counts: the list's scrollbar
        // gutter is inside its rectangle and selects nothing.
        match row.filter(|index| self.finder.selected() == Some(*index)) {
            Some(index) if self.clicks.completed(index, now_ns, x, y) => self.activate(index),
            Some(_) => Step::Continue,
            None => {
                self.clicks.cancel();
                self.outcome(outcome)
            }
        }
    }

    /// Wheel travel scrolls the list, wherever the pointer is.
    pub fn wheel(&mut self, rows: isize) -> Step {
        self.clicks.cancel();
        let list = self.finder.list_rect();
        let outcome = self.finder.event(finder::Event::Wheel {
            x: list.x,
            y: list.y,
            rows,
        });
        self.outcome(outcome)
    }

    pub fn resize(&mut self, surface: Surface) -> Step {
        self.surface = surface;
        self.clicks.cancel();
        let Some(laid) = padded(surface) else {
            return Step::Continue;
        };
        let outcome = self.finder.event(finder::Event::Resize {
            surface: laid,
            rect: finder_rect(laid),
        });
        self.outcome(outcome)
    }
}

fn fill(rect: Rect, color: u32, damage: Rect, sink: &mut dyn FnMut(Draw)) {
    if let Some(clip) = rect.intersection(damage) {
        sink(Draw {
            clip,
            primitive: Primitive::Fill { rect, color },
        });
    }
}

impl Composition for Chooser {
    fn surface(&self) -> Surface {
        self.surface
    }

    fn emit(&self, damage: Rect, sink: &mut dyn FnMut(Draw)) {
        let Some(damage) = damage.intersection(self.surface.bounds()) else {
            return;
        };
        let scale = self.surface.scale;
        let s = scale.value();
        let inset = (INSET_X * s) as i64;
        let content = self.surface.width.saturating_sub(2 * INSET_X * s) as u32;
        let band = |top: usize| Rect {
            x: inset,
            y: (top * s) as i64,
            width: content,
            height: (CELL_HEIGHT * s) as u32,
        };
        fill(self.surface.bounds(), CHROME, damage, sink);
        let title = band(TITLE_Y);
        text_run(
            scale,
            "Choose a repository to review".chars(),
            (title.x, title.y),
            title,
            GlyphStyle::medium(INK, CHROME),
            damage,
            sink,
        );
        fill(
            Rect {
                x: inset,
                y: (RULE_Y * s) as i64,
                width: content,
                height: s as u32,
            },
            BORDER,
            damage,
            sink,
        );
        let hint = band(HINT_Y);
        text_run(
            scale,
            self.hint().chars(),
            (hint.x, hint.y),
            hint,
            GlyphStyle::medium(LINE_NUMBER, CHROME),
            damage,
            sink,
        );
        self.finder.emit(damage, sink);
        Status::new(self.surface).emit(self.legend().chars(), damage, sink);
    }
}

/// What a folder chosen opens as, or why it does not.
pub type Opener<T> = Box<dyn FnMut(&Path) -> Result<T, String>>;

/// The chooser in a window, and what it chose.
struct Window<T> {
    chooser: Chooser,
    open: Opener<T>,
    /// Where a forgotten repository is dropped from.
    file: Option<PathBuf>,
    chosen: Option<T>,
    began: Instant,
    dirty: bool,
}

impl<T> Window<T> {
    fn step(&mut self, step: Step) -> Flow {
        self.dirty = true;
        match step {
            Step::Continue => Flow::Continue,
            Step::Quit => Flow::Quit,
            Step::Forget(path) => {
                if let Some(file) = &self.file {
                    if let Err(e) = crate::saved::forget(file, &path) {
                        self.chooser.note(&format!("saving the list: {e}"));
                    }
                }
                Flow::Continue
            }
            Step::Open(path) => match (self.open)(&path) {
                Ok(chosen) => {
                    self.chosen = Some(chosen);
                    Flow::Quit
                }
                Err(why) => {
                    self.chooser.refuse(&why);
                    Flow::Continue
                }
            },
        }
    }
}

impl<T> Handler for Window<T> {
    fn app_id(&self) -> &str {
        "td-review"
    }

    fn title(&self) -> &str {
        "td-review: choose a repository"
    }

    fn input(&mut self, input: Input<'_>, clipboard: &mut dyn Clipboard) -> Flow {
        let step = match input {
            Input::Key {
                chord: "C-v",
                repeat: false,
            } => {
                if let Err(e) = clipboard.paste() {
                    self.chooser.note(&e.to_string());
                }
                Step::Continue
            }
            Input::Key { chord, repeat } => self.chooser.key(chord, repeat),
            Input::Paste(text) => self.chooser.paste(text),
            Input::Pointer {
                phase: PointerPhase::Press,
                x,
                y,
                ..
            } => {
                let now = u64::try_from(self.began.elapsed().as_nanos()).unwrap_or(u64::MAX);
                self.chooser.pointer(PointerPhase::Press, x, y, now)
            }
            Input::Wheel { rows, .. } if rows != 0 => self.chooser.wheel(rows),
            Input::Resize(surface) => self.chooser.resize(surface),
            Input::Close => Step::Quit,
            // A drag or a release changes nothing here, so repaints nothing.
            // A pending double click does not outlive the pointer or focus.
            Input::CancelPointer | Input::Focus(_) => {
                self.chooser.clicks.cancel();
                return Flow::Continue;
            }
            Input::Pointer { .. }
            | Input::Wheel { .. }
            | Input::Hover(_)
            | Input::Context { .. } => return Flow::Continue,
        };
        self.step(step)
    }

    fn poll(&mut self, _now: u64) -> Flow {
        Flow::Continue
    }

    fn wait_ms(&self, _now: u64) -> u64 {
        u64::try_from(td_ui::wayland::IDLE_WAIT.as_millis()).unwrap_or(u64::MAX)
    }

    fn needs_redraw(&self) -> bool {
        self.dirty
    }

    fn paint(&mut self, raster: &mut Raster<'_, '_>, surface: Surface) -> Result<(), String> {
        if surface != self.chooser.surface {
            self.chooser.resize(surface);
        }
        raster
            .paint(&self.chooser, surface.bounds())
            .map_err(|e| e.to_string())?;
        self.dirty = false;
        Ok(())
    }

    fn notice(&mut self, message: &str) {
        eprintln!("td-review: window: {}", view::scrub(message));
    }
}

/// Opens the chooser on the compositor the environment names and returns
/// what `open` made of the folder chosen, or none when the window was
/// closed without one. `file` is the saved list, for a Delete; `warning`
/// is shown on the status row from the start.
pub fn run<T>(
    saved: Vec<PathBuf>,
    warning: Option<String>,
    file: Option<PathBuf>,
    home: Option<PathBuf>,
    start: PathBuf,
    open: Opener<T>,
) -> io::Result<Option<T>> {
    // A connection of its own, by the display's path: an inherited
    // WAYLAND_SOCKET is one connection, and the review window that follows
    // needs it fresh.
    let display = std::env::var_os("WAYLAND_DISPLAY");
    if display.is_none() && std::env::var_os("WAYLAND_SOCKET").is_some() {
        return Err(io::Error::other(
            "--choose-repo needs WAYLAND_DISPLAY: the one connection WAYLAND_SOCKET names is the review window's",
        ));
    }
    let endpoint = td_ui::wayland::endpoint(None, display, std::env::var_os("XDG_RUNTIME_DIR"))
        .map_err(io::Error::other)?;
    let stream = td_ui::wayland::connect(endpoint).map_err(io::Error::other)?;
    let scale = td_ui::raster::Scale::new(1).map_err(|e| io::Error::other(e.to_string()))?;
    let surface = Surface::new(
        td_ui::window::DEFAULT_WIDTH,
        td_ui::window::DEFAULT_HEIGHT,
        scale,
    )
    .map_err(|e| io::Error::other(e.to_string()))?;
    let mut chooser = Chooser::new(saved, home, start, surface).map_err(io::Error::other)?;
    if let Some(warning) = warning {
        chooser.note(&warning);
    }
    let mut window = Window {
        chooser,
        open,
        file,
        chosen: None,
        began: Instant::now(),
        dirty: true,
    };
    let typeface = td_ui::pinned_face::load_or_note(
        "td-review",
        std::env::var_os(td_ui::pinned_face::SETTING).as_deref(),
    );
    let shown = td_ui::window::run(&mut window, stream, std::env::temp_dir(), typeface);
    // A choice made stands, whatever the window met closing.
    match (window.chosen, shown) {
        (Some(chosen), _) => Ok(Some(chosen)),
        (None, shown) => shown.map(|()| None).map_err(io::Error::other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use td_ui::raster::Scale;
    use td_ui::window::NoClipboard;

    fn surface(width: usize, height: usize, scale: u8) -> Surface {
        Surface::new(width, height, Scale::new(scale).unwrap()).unwrap()
    }

    /// A home holding `src` with a repository `td`, a plain folder `notes`
    /// holding a repository `deep`, a dot folder and a file.
    fn home(tag: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("td-review-chooser-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("src/td/.git")).unwrap();
        fs::create_dir_all(root.join("src/notes/deep")).unwrap();
        fs::write(root.join("src/notes/deep/.git"), "gitdir: elsewhere\n").unwrap();
        fs::create_dir_all(root.join("src/.hidden")).unwrap();
        fs::write(root.join("src/README"), "").unwrap();
        root
    }

    fn names(chooser: &Chooser) -> Vec<(String, String, bool)> {
        chooser
            .finder
            .listing()
            .entries()
            .iter()
            .map(|e| (e.name().to_string(), e.meta().to_string(), e.enabled()))
            .collect()
    }

    fn browsing(tag: &str, saved: Vec<PathBuf>) -> (PathBuf, Chooser) {
        let root = home(tag);
        let chooser = Chooser::new(
            saved,
            Some(root.clone()),
            root.join("src"),
            surface(800, 600, 1),
        )
        .unwrap();
        (root, chooser)
    }

    /// The point inside the list's `row`th shown row.
    fn row_point(chooser: &Chooser, row: usize) -> (i64, i64) {
        let list = chooser.finder.list_rect();
        (list.x + 4, list.y + (row * ROW) as i64 + 4)
    }

    #[test]
    fn with_nothing_saved_it_browses_the_folders_marking_repositories() {
        let (root, chooser) = browsing("browse", Vec::new());
        assert_eq!(chooser.finder.listing().path(), "~/src");
        assert_eq!(
            names(&chooser),
            [
                ("notes".to_string(), String::new(), true),
                ("td".to_string(), "git".to_string(), true),
            ]
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn return_enters_every_folder_and_ctrl_return_opens_the_one_in_view() {
        let (root, mut chooser) = browsing("return", Vec::new());
        assert_eq!(
            chooser.hint(),
            "Folders marked git are repositories: enter one, Ctrl+Return opens it."
        );
        // A repository's mark is its own folder's state, which an application
        // can set: Return enters it rather than opening it.
        assert_eq!(chooser.key("Down", false), Step::Continue);
        assert_eq!(chooser.key("Return", false), Step::Continue);
        assert_eq!(chooser.finder.listing().path(), "~/src/td");
        assert_eq!(
            chooser.hint(),
            "This folder is a repository: Ctrl+Return opens it."
        );
        assert_eq!(
            chooser.key("C-Return", false),
            Step::Open(root.join("src/td"))
        );
        assert_eq!(chooser.key("Left", false), Step::Continue);
        assert_eq!(chooser.key("Up", false), Step::Continue);
        assert_eq!(chooser.key("Return", false), Step::Continue);
        assert_eq!(chooser.finder.listing().path(), "~/src/notes");
        // A `.git` file, as a linked worktree has, marks one too.
        assert_eq!(names(&chooser), [("deep".into(), "git".into(), true)]);
        // Right enters, as Return does.
        assert_eq!(chooser.key("Right", false), Step::Continue);
        assert_eq!(chooser.finder.listing().path(), "~/src/notes/deep");
        // Ctrl+Return opens the folder in view.
        assert_eq!(
            chooser.key("C-Return", false),
            Step::Open(root.join("src/notes/deep"))
        );
        // Left goes back up, to the folder it came from.
        assert_eq!(chooser.key("Left", false), Step::Continue);
        assert_eq!(chooser.finder.listing().path(), "~/src/notes");
        assert_eq!(chooser.key("Backspace", false), Step::Continue);
        assert_eq!(chooser.finder.listing().path(), "~/src");
        assert_eq!(
            chooser.finder.selected_entry().map(Entry::name),
            Some("notes")
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn typing_filters_escape_clears_it_and_a_held_return_does_nothing() {
        let (root, mut chooser) = browsing("filter", Vec::new());
        for c in ["t", "d"] {
            assert_eq!(chooser.key(c, false), Step::Continue);
        }
        assert_eq!(chooser.finder.shown().len(), 1);
        assert_eq!(chooser.key("Return", true), Step::Continue);
        // A held letter types on.
        assert_eq!(chooser.key("d", true), Step::Continue);
        assert_eq!(chooser.finder.query(), "tdd");
        chooser.key("Backspace", false);
        assert_eq!(chooser.finder.query(), "td");
        // Escape takes the filter away before it would quit.
        assert_eq!(chooser.key("Escape", false), Step::Continue);
        assert_eq!(chooser.finder.query(), "");
        assert_eq!(chooser.finder.shown().len(), 2);
        for c in ["t", "d"] {
            assert_eq!(chooser.key(c, false), Step::Continue);
        }
        assert_eq!(chooser.key("Return", false), Step::Continue);
        assert_eq!(chooser.finder.listing().path(), "~/src/td");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn the_saved_list_comes_first_and_escape_steps_back_out() {
        let root = home("saved");
        let saved = vec![root.join("src/td"), root.join("gone")];
        let mut chooser = Chooser::new(
            saved,
            Some(root.clone()),
            root.join("src"),
            surface(800, 600, 1),
        )
        .unwrap();
        assert_eq!(chooser.finder.listing().path(), SAVED_TITLE);
        assert_eq!(
            names(&chooser),
            [
                ("~/src/td".into(), "saved".into(), true),
                ("~/gone".into(), "missing".into(), false),
                (BROWSE.into(), String::new(), true),
            ]
        );
        assert_eq!(
            chooser.key("Return", false),
            Step::Open(root.join("src/td"))
        );
        // Right opens nothing on the list; it is Browse's way in.
        assert_eq!(chooser.key("Right", false), Step::Continue);
        assert_eq!(chooser.finder.listing().path(), SAVED_TITLE);
        // Browse, then Escape back to the list, then Escape out.
        chooser.key("End", false);
        assert_eq!(chooser.key("Return", false), Step::Continue);
        assert_eq!(chooser.finder.listing().path(), "~/src");
        assert_eq!(chooser.key("Escape", false), Step::Continue);
        assert_eq!(chooser.finder.listing().path(), SAVED_TITLE);
        assert_eq!(chooser.key("Escape", false), Step::Quit);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_missing_saved_repository_opens_by_no_route() {
        let root = home("missing");
        let saved = vec![root.join("gone"), root.join("src/td")];
        let mut chooser = Chooser::new(
            saved,
            Some(root.clone()),
            root.join("src"),
            surface(800, 600, 1),
        )
        .unwrap();
        assert_eq!(chooser.key("Return", false), Step::Continue);
        assert_eq!(chooser.key("C-Return", false), Step::Continue);
        let (x, y) = row_point(&chooser, 0);
        assert_eq!(
            chooser.pointer(PointerPhase::Press, x, y, 0),
            Step::Continue
        );
        assert_eq!(
            chooser.pointer(PointerPhase::Press, x, y, 100_000_000),
            Step::Continue
        );
        assert_eq!(chooser.finder.listing().path(), SAVED_TITLE);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn delete_forgets_a_saved_repository_and_not_the_browse_row() {
        let root = home("forget");
        let saved = vec![root.join("src/td"), root.join("gone")];
        let mut chooser = Chooser::new(
            saved,
            Some(root.clone()),
            root.join("src"),
            surface(800, 600, 1),
        )
        .unwrap();
        chooser.key("Down", false);
        assert_eq!(
            chooser.key("Delete", false),
            Step::Forget(root.join("gone"))
        );
        assert_eq!(chooser.saved, [root.join("src/td")]);
        // The row that took its place is selected: here, Browse.
        assert_eq!(
            chooser.finder.selected_entry().map(Entry::name),
            Some(BROWSE)
        );
        chooser.key("End", false);
        assert_eq!(chooser.key("Delete", false), Step::Continue);
        chooser.key("Home", false);
        assert_eq!(
            chooser.key("Delete", false),
            Step::Forget(root.join("src/td"))
        );
        // The last one gone, the chooser browses, and Escape quits.
        assert_eq!(chooser.finder.listing().path(), "~/src");
        assert_eq!(chooser.key("Escape", false), Step::Quit);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn an_unreadable_start_leaves_the_browse_row_saying_why() {
        let root = home("unreadable");
        let saved = vec![root.join("src/td")];
        let mut chooser = Chooser::new(
            saved,
            Some(root.clone()),
            root.join("nowhere"),
            surface(800, 600, 1),
        )
        .unwrap();
        // Forgetting the last one cannot browse, so the list stays, Browse
        // alone on it.
        assert_eq!(
            chooser.key("Delete", false),
            Step::Forget(root.join("src/td"))
        );
        assert_eq!(chooser.finder.listing().path(), SAVED_TITLE);
        assert_eq!(names(&chooser), [(BROWSE.into(), String::new(), true)]);
        // And one started with nothing saved begins there, the note saying
        // why.
        let empty = Chooser::new(
            Vec::new(),
            Some(root.clone()),
            root.join("nowhere"),
            surface(800, 600, 1),
        )
        .unwrap();
        assert_eq!(empty.finder.listing().path(), SAVED_TITLE);
        assert!(
            empty.finder.note().contains("nowhere"),
            "{}",
            empty.finder.note()
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_refusal_is_noted_on_one_line() {
        let (root, mut chooser) = browsing("refuse", Vec::new());
        chooser.refuse("\nfatal: not a git repository\nmore\n");
        assert_eq!(chooser.finder.note(), "fatal: not a git repository");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_double_click_on_a_row_is_return_on_it() {
        let (root, mut chooser) = browsing("click", Vec::new());
        let (x, y) = row_point(&chooser, 1);
        // Two presses far apart in time are two selections.
        assert_eq!(
            chooser.pointer(PointerPhase::Press, x, y, 0),
            Step::Continue
        );
        assert_eq!(chooser.finder.selected_entry().map(Entry::name), Some("td"));
        assert_eq!(
            chooser.pointer(PointerPhase::Press, x, y, 2_000_000_000),
            Step::Continue
        );
        assert_eq!(chooser.finder.listing().path(), "~/src");
        // Two close together enter the folder, a repository too.
        assert_eq!(
            chooser.pointer(PointerPhase::Press, x, y, 2_100_000_000),
            Step::Continue
        );
        assert_eq!(chooser.finder.listing().path(), "~/src/td");
        // On the saved list, a double press opens.
        let saved = vec![root.join("src/td")];
        let mut chooser = Chooser::new(
            saved,
            Some(root.clone()),
            root.join("src"),
            surface(800, 600, 1),
        )
        .unwrap();
        let (x, y) = row_point(&chooser, 0);
        chooser.pointer(PointerPhase::Press, x, y, 0);
        assert_eq!(
            chooser.pointer(PointerPhase::Press, x, y, 100_000_000),
            Step::Open(root.join("src/td"))
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn two_presses_in_the_scrollbar_gutter_open_nothing() {
        let (root, mut chooser) = browsing("gutter", Vec::new());
        let list = chooser.finder.list_rect();
        let x = list.x + i64::from(list.width) - 2;
        // Beside the row already selected, then beside another.
        let (_, y) = row_point(&chooser, 0);
        assert_eq!(
            chooser.pointer(PointerPhase::Press, x, y, 0),
            Step::Continue
        );
        assert_eq!(
            chooser.pointer(PointerPhase::Press, x, y, 100_000_000),
            Step::Continue
        );
        let (_, y) = row_point(&chooser, 1);
        assert_eq!(
            chooser.pointer(PointerPhase::Press, x, y, 0),
            Step::Continue
        );
        assert_eq!(
            chooser.pointer(PointerPhase::Press, x, y, 100_000_000),
            Step::Continue
        );
        assert_eq!(chooser.finder.listing().path(), "~/src");
        assert_eq!(
            chooser.finder.selected_entry().map(Entry::name),
            Some("notes")
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_double_click_needs_one_row_of_one_listing() {
        let (root, mut chooser) = browsing("pairs", Vec::new());
        // A press on `notes`, then one on the blank list below the rows.
        let (x, y) = row_point(&chooser, 0);
        chooser.pointer(PointerPhase::Press, x, y, 0);
        let (bx, by) = row_point(&chooser, 5);
        assert_eq!(
            chooser.pointer(PointerPhase::Press, bx, by, 1_000),
            Step::Continue
        );
        assert_eq!(chooser.finder.listing().path(), "~/src");
        // A press on `notes`, Right into it, and a press at the same point:
        // the first row of a new listing, not a double click.
        chooser.pointer(PointerPhase::Press, x, y, 2_000_000_000);
        assert_eq!(chooser.key("Right", false), Step::Continue);
        assert_eq!(chooser.finder.listing().path(), "~/src/notes");
        assert_eq!(
            chooser.pointer(PointerPhase::Press, x, y, 2_100_000_000),
            Step::Continue
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn the_window_opens_what_the_opener_accepts_and_notes_a_refusal() {
        let (root, chooser) = browsing("window", Vec::new());
        let file = root.join("config/repositories");
        crate::saved::store(&file, &[root.join("src/td"), root.join("kept")]).unwrap();
        let mut window = Window {
            chooser,
            open: Box::new(|path: &Path| match path.ends_with("td") {
                true => Ok(path.to_path_buf()),
                false => Err("refused\nsecond line".to_string()),
            }),
            file: Some(file.clone()),
            chosen: None,
            began: Instant::now(),
            dirty: false,
        };
        let mut clipboard = NoClipboard;
        let key = |chord| Input::Key {
            chord,
            repeat: false,
        };
        // C-v asks the clipboard, whose refusal is noted.
        assert_eq!(window.input(key("C-v"), &mut clipboard), Flow::Continue);
        assert!(!window.chooser.finder.note().is_empty());
        // Pasted text filters, up to its first line.
        assert_eq!(
            window.input(Input::Paste("no\nmore"), &mut clipboard),
            Flow::Continue
        );
        assert_eq!(window.chooser.finder.query(), "no");
        window.input(key("Escape"), &mut clipboard);
        assert_eq!(window.chooser.finder.query(), "");
        window.dirty = false;
        // A release repaints nothing.
        let release = Input::Pointer {
            phase: PointerPhase::Release,
            x: 1,
            y: 1,
            extend: false,
            follow: false,
        };
        assert_eq!(window.input(release, &mut clipboard), Flow::Continue);
        assert!(!window.needs_redraw());
        assert_eq!(
            window.input(key("C-Return"), &mut clipboard),
            Flow::Continue
        );
        assert_eq!(window.chooser.finder.note(), "refused");
        assert!(window.needs_redraw());
        // A Forget drops that one path from the file as it now is.
        assert_eq!(
            window.step(Step::Forget(root.join("src/td"))),
            Flow::Continue
        );
        assert_eq!(crate::saved::load(&file).unwrap(), [root.join("kept")]);
        window.input(key("Down"), &mut clipboard);
        assert_eq!(window.input(key("Return"), &mut clipboard), Flow::Continue);
        assert_eq!(window.input(key("C-Return"), &mut clipboard), Flow::Quit);
        assert_eq!(window.chosen, Some(root.join("src/td")));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn it_paints_the_rows_on_any_surface_down_to_a_sliver() {
        let (root, mut chooser) = browsing("paint", Vec::new());
        let font = td_ui::font::pinned().unwrap();
        // The whole window shows the selected row's ground; a sliver too
        // small for the list is painted, clipped, without error.
        for (s, whole) in [
            (surface(800, 600, 1), true),
            (surface(1600, 1200, 2), true),
            (surface(40, 30, 1), false),
        ] {
            assert_eq!(chooser.resize(s), Step::Continue);
            let mut pixels = vec![0u8; s.width * s.height * 4];
            let mut raster = Raster::new(&mut pixels, &font, s, s.width * 4).unwrap();
            raster.paint(&chooser, s.bounds()).unwrap();
            let [_, red, green, blue] = td_ui::chrome::SELECTED_ROW.to_be_bytes();
            let selected = pixels
                .as_chunks::<4>()
                .0
                .iter()
                .any(|p| p[..3] == [blue, green, red]);
            assert_eq!(selected, whole, "{s:?}");
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rows_show_paths_under_the_home_with_a_tilde() {
        let home = Path::new("/home/h");
        assert_eq!(
            shown(Path::new("/home/h/src/td"), Some(home), 64),
            "~/src/td"
        );
        assert_eq!(shown(Path::new("/home/h"), Some(home), 64), "~");
        assert_eq!(shown(Path::new("/srv/x"), Some(home), 64), "/srv/x");
        assert_eq!(shown(Path::new("/a\u{7}b"), None, 64), "/a?b");
        assert_eq!(shown(Path::new("/ééé"), None, 4), "/é");
        // A home of `/` is no home to abbreviate.
        assert_eq!(shown(Path::new("/srv"), Some(Path::new("/")), 64), "/srv");
    }
}

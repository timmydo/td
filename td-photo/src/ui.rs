//! The cull controller: the roll as the window shows it, a grid of photos
//! or one of them, the closed action table an agent reads, and the one
//! dispatcher the keyboard, the pointer, the replay and the socket all go
//! through. Pure: it holds names and sidecars, returns the effects an
//! adapter carries out (open this folder, write that sidecar) and lays out
//! a scene the toolkit paints; `main` owns the files.

use std::fmt;

use td_ui::chrome::{Bar, Block, Status, DISABLED, ROW};
use td_ui::control::{self, decimal, hex, ErrorCode};
use td_ui::driven::{self, Binding, Input, Outcome, PointerPhase};
use td_ui::raster::{
    text_run, Composition, Draw, GlyphStyle, Primitive, Rect, Scale, Surface, CHROME, INK,
    MISSPELLED, PAPER, SELECTED,
};
// One toolkit name per line: tests/confinement.rs reads the name after the
// crate's path.
use td_ui::CELL_HEIGHT;
use td_ui::CELL_WIDTH;

use crate::image::Rgb8;
use crate::library::{Filter, Flag, Key, Sidecar};

/// The surface a session starts on until it is resized.
pub const DEFAULT_WIDTH: usize = 800;
pub const DEFAULT_HEIGHT: usize = 600;
/// A grid cell in reference pixels: the thumbnail box, padding around it
/// and one text row for the name under it; the surface's scale multiplies.
pub const THUMB_WIDTH: usize = 160;
pub const THUMB_HEIGHT: usize = 120;
pub const CELL_PAD: usize = 8;
pub const CELL_W: usize = THUMB_WIDTH + 2 * CELL_PAD;
pub const CELL_H: usize = CELL_PAD + THUMB_HEIGHT + CELL_PAD / 2 + CELL_HEIGHT + CELL_PAD / 2;
/// The most photos a roll may hold in the model, the same ceiling the
/// verbs put on a folder, and the most sidecar text they may hold between
/// them; a roll past either is refused rather than held.
pub const MAX_PHOTOS: usize = 100_000;
pub const MAX_SIDECAR_TOTAL: usize = 64 << 20;
/// The reason a photo is shown as refused when its sidecar would take the
/// roll past `MAX_SIDECAR_TOTAL`.
pub const OVER_BUDGET: &str = "over the roll's sidecar budget";
/// What the window holds of thumbnails in memory between them; past it the
/// least recently shown that is not on screen goes first.
pub const THUMB_CACHE_BYTES: usize = 256 << 20;
/// The longest `wait-idle`, in milliseconds: under the transport's deadline
/// per request (five seconds), so a held wait's reply is written before its
/// connection expires.
pub const MAX_WAIT_MS: u64 = 4_000;
/// Control requests the window admits per turn, td-editor's figure.
pub const CONTROL_JOBS_PER_TURN: usize = 8;
/// Where a thumbnail goes until it is painted: a neutral ground.
pub const PLACEHOLDER: u32 = 0xd6d1c7;
/// The mode word `state` leads with; develop mode is a later increment.
pub const MODE: &str = "cull";

/// Why an action or an input is refused; the codes travel on the wire.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    /// The action needs an open roll.
    NoRoll,
    /// The action needs a photo under the cursor, or names one that is
    /// not shown.
    NoPhoto,
    /// A field is outside the action's grammar.
    BadArgument,
    /// The model would not take it: a refused sidecar, a write that failed.
    Refused,
    Transport(control::Error),
}

impl ErrorCode for Error {
    fn code(&self) -> &'static str {
        match self {
            Self::NoRoll => "no-roll",
            Self::NoPhoto => "no-photo",
            Self::BadArgument => "bad-argument",
            Self::Refused => "refused",
            Self::Transport(error) => error.code(),
        }
    }
}

impl From<control::Error> for Error {
    fn from(error: control::Error) -> Self {
        Self::Transport(error)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code())
    }
}

impl std::error::Error for Error {}

/// One photo as the model holds it: its name in the roll and its sidecar,
/// absent for the camera's defaults, or the reason the sidecar was refused.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Photo {
    pub name: String,
    pub sidecar: Option<Sidecar>,
    pub error: Option<String>,
}

impl Photo {
    pub fn flag(&self) -> Option<Flag> {
        self.sidecar.as_ref().and_then(Sidecar::flag)
    }

    /// A known key's value as written, `-` when absent.
    pub fn value(&self, key: Key) -> &str {
        self.sidecar
            .as_ref()
            .and_then(|sidecar| sidecar.value(key))
            .unwrap_or("-")
    }

    /// The bytes the sidecar takes as text, what the roll's budget counts.
    pub fn bytes(&self) -> usize {
        self.sidecar.as_ref().map_or(0, sidecar_bytes)
    }

    /// The sidecar's state as `list` words it.
    pub fn status(&self) -> &'static str {
        match (&self.sidecar, &self.error) {
            (_, Some(_)) => "error",
            (Some(_), None) => "ok",
            (None, None) => "none",
        }
    }
}

/// The bytes a sidecar takes as text, what the roll's budget counts.
pub fn sidecar_bytes(sidecar: &Sidecar) -> usize {
    sidecar
        .entries()
        .map(|(key, value)| key.len() + value.len() + 2)
        .sum()
}

/// What an adapter carries out after a dispatch: the file work the model
/// asked for, in order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Effect {
    /// Open the roll at this path (bytes, as the request carried them).
    Open(Vec<u8>),
    /// Set or clear this photo's flag in its sidecar, on the file as it is
    /// then, and settle the model from what was written or found.
    Flag {
        index: usize,
        name: String,
        flag: Option<Flag>,
    },
}

/// The closed set of things the window does. The table below is its
/// public face; `ALL` and `BINDINGS` are aligned by index and a test pins
/// that.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Action {
    Open,
    Next,
    Previous,
    Down,
    Up,
    First,
    Last,
    PageDown,
    PageUp,
    Select,
    Pick,
    Reject,
    Unflag,
    All,
    Picks,
    Rejects,
    Unflagged,
    View,
    Grid,
    Scroll,
    Quit,
}

impl Action {
    pub const ALL: [Action; 21] = [
        Action::Open,
        Action::Next,
        Action::Previous,
        Action::Down,
        Action::Up,
        Action::First,
        Action::Last,
        Action::PageDown,
        Action::PageUp,
        Action::Select,
        Action::Pick,
        Action::Reject,
        Action::Unflag,
        Action::All,
        Action::Picks,
        Action::Rejects,
        Action::Unflagged,
        Action::View,
        Action::Grid,
        Action::Scroll,
        Action::Quit,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Next => "next",
            Self::Previous => "previous",
            Self::Down => "down",
            Self::Up => "up",
            Self::First => "first",
            Self::Last => "last",
            Self::PageDown => "page-down",
            Self::PageUp => "page-up",
            Self::Select => "select",
            Self::Pick => "pick",
            Self::Reject => "reject",
            Self::Unflag => "unflag",
            Self::All => "all",
            Self::Picks => "picks",
            Self::Rejects => "rejects",
            Self::Unflagged => "unflagged",
            Self::View => "view",
            Self::Grid => "grid",
            Self::Scroll => "scroll",
            Self::Quit => "quit",
        }
    }

    pub fn parse(name: &str) -> Option<Action> {
        Self::ALL.into_iter().find(|action| action.name() == name)
    }

    /// Whether holding the key repeats the action: the cursor moves and the
    /// pages do, so a held arrow walks the grid; a flag, a filter, a view
    /// or quit fires once.
    pub fn repeats(self) -> bool {
        matches!(
            self,
            Self::Next | Self::Previous | Self::Down | Self::Up | Self::PageDown | Self::PageUp
        )
    }
}

/// The action table: the name `action` takes, the chord the keyboard
/// binds, the argument shape and the help line. Actions without a chord
/// take an argument or are the agent's (`open`); the pointer reaches
/// `select` by pressing a cell and `scroll` by the wheel.
pub const BINDINGS: [Binding; 21] = [
    Binding {
        name: "open",
        chord: None,
        arguments: "HEX_PATH",
        help: "Open the roll at the path, given as hex bytes.",
    },
    Binding {
        name: "next",
        chord: Some("Right"),
        arguments: "",
        help: "Move the cursor to the next shown photo.",
    },
    Binding {
        name: "previous",
        chord: Some("Left"),
        arguments: "",
        help: "Move the cursor to the previous shown photo.",
    },
    Binding {
        name: "down",
        chord: Some("Down"),
        arguments: "",
        help: "Move the cursor one grid row down.",
    },
    Binding {
        name: "up",
        chord: Some("Up"),
        arguments: "",
        help: "Move the cursor one grid row up.",
    },
    Binding {
        name: "first",
        chord: Some("Home"),
        arguments: "",
        help: "Move the cursor to the first shown photo.",
    },
    Binding {
        name: "last",
        chord: Some("End"),
        arguments: "",
        help: "Move the cursor to the last shown photo.",
    },
    Binding {
        name: "page-down",
        chord: Some("PageDown"),
        arguments: "",
        help: "Move the cursor a screen of rows down.",
    },
    Binding {
        name: "page-up",
        chord: Some("PageUp"),
        arguments: "",
        help: "Move the cursor a screen of rows up.",
    },
    Binding {
        name: "select",
        chord: None,
        arguments: "N",
        help: "Move the cursor to the Nth shown photo, counting from 0.",
    },
    Binding {
        name: "pick",
        chord: Some("p"),
        arguments: "",
        help: "Flag the photo under the cursor a pick.",
    },
    Binding {
        name: "reject",
        chord: Some("x"),
        arguments: "",
        help: "Flag the photo under the cursor a reject.",
    },
    Binding {
        name: "unflag",
        chord: Some("u"),
        arguments: "",
        help: "Clear the flag of the photo under the cursor.",
    },
    Binding {
        name: "all",
        chord: Some("1"),
        arguments: "",
        help: "Show every photo.",
    },
    Binding {
        name: "picks",
        chord: Some("2"),
        arguments: "",
        help: "Show the picks only.",
    },
    Binding {
        name: "rejects",
        chord: Some("3"),
        arguments: "",
        help: "Show the rejects only.",
    },
    Binding {
        name: "unflagged",
        chord: Some("4"),
        arguments: "",
        help: "Show the unflagged photos only.",
    },
    Binding {
        name: "view",
        chord: Some("Return"),
        arguments: "",
        help: "Toggle between the grid and the single photo under the cursor.",
    },
    Binding {
        name: "grid",
        chord: Some("Escape"),
        arguments: "",
        help: "Back to the grid.",
    },
    Binding {
        name: "scroll",
        chord: None,
        arguments: "ROWS",
        help: "Scroll the grid by signed rows without moving the cursor.",
    },
    Binding {
        name: "quit",
        chord: Some("q"),
        arguments: "",
        help: "Close the window.",
    },
];

/// The filters in the order the bar shows them.
pub const FILTERS: [(Filter, &str); 4] = [
    (Filter::All, "All"),
    (Filter::Picks, "Picks"),
    (Filter::Rejects, "Rejects"),
    (Filter::Unflagged, "Unflagged"),
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum View {
    Grid,
    Single,
}

impl View {
    pub fn word(self) -> &'static str {
        match self {
            Self::Grid => "grid",
            Self::Single => "single",
        }
    }
}

struct Roll {
    label: String,
    path: Vec<u8>,
}

/// The grid's geometry on a surface: the bar above, the status row below,
/// and whole cells in the area between.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Layout {
    pub surface: Surface,
    pub area: Rect,
    pub columns: usize,
    pub rows: usize,
    pub cell_width: usize,
    pub cell_height: usize,
}

impl Layout {
    pub fn new(surface: Surface) -> Layout {
        let s = surface.scale.value();
        let band = ROW * s;
        let height = surface.height.saturating_sub(2 * band);
        let area = Rect {
            x: 0,
            y: band as i64,
            width: surface.width as u32,
            height: height as u32,
        };
        let cell_width = CELL_W * s;
        let cell_height = CELL_H * s;
        Layout {
            surface,
            area,
            columns: (surface.width / cell_width).max(1),
            rows: (height / cell_height).max(1),
            cell_width,
            cell_height,
        }
    }

    /// The cell of the photo at `position` among the shown, with the grid
    /// scrolled to `first_row`; `None` when it is off the area.
    pub fn cell(&self, position: usize, first_row: usize) -> Option<Rect> {
        let row = (position / self.columns).checked_sub(first_row)?;
        if row >= self.rows {
            return None;
        }
        let column = position % self.columns;
        Some(Rect {
            x: self.area.x + (column * self.cell_width) as i64,
            y: self.area.y + (row * self.cell_height) as i64,
            width: self.cell_width as u32,
            height: self.cell_height as u32,
        })
    }

    /// The thumbnail box inside a cell: `THUMB_WIDTH` by `THUMB_HEIGHT` at
    /// the scale, under `CELL_PAD` of padding.
    pub fn thumb(&self, cell: Rect) -> Rect {
        let s = self.surface.scale.value();
        let pad = (CELL_PAD * s) as i64;
        Rect {
            x: cell.x + pad,
            y: cell.y + pad,
            width: (THUMB_WIDTH * s) as u32,
            height: (THUMB_HEIGHT * s) as u32,
        }
    }

    /// The position under a point, scrolled to `first_row`, whether or not
    /// a photo is there.
    pub fn position_at(&self, x: i64, y: i64, first_row: usize) -> Option<usize> {
        if !self.area.contains(x, y) {
            return None;
        }
        let column = usize::try_from(x - self.area.x).ok()? / self.cell_width;
        let row = usize::try_from(y - self.area.y).ok()? / self.cell_height;
        if column >= self.columns || row >= self.rows {
            return None;
        }
        Some((first_row + row) * self.columns + column)
    }
}

/// The cull model and its dispatcher.
pub struct Controller {
    surface: Surface,
    roll: Option<Roll>,
    photos: Vec<Photo>,
    cursor: Option<usize>,
    filter: Filter,
    view: View,
    first_row: usize,
    generation: u64,
    /// The indices the filter admits, in roll order, refreshed when the
    /// photos or the filter change, so a step scans nothing.
    shown: Vec<usize>,
    /// What the photos' sidecars take between them, held under
    /// `MAX_SIDECAR_TOTAL` at open and at every settle.
    bytes: usize,
    /// The jobs the adapter has outstanding, as it last said.
    jobs: usize,
}

impl Controller {
    pub fn new(surface: Surface) -> Controller {
        Controller {
            surface,
            roll: None,
            photos: Vec::new(),
            cursor: None,
            filter: Filter::All,
            view: View::Grid,
            first_row: 0,
            generation: 0,
            shown: Vec::new(),
            bytes: 0,
            jobs: 0,
        }
    }

    /// The roll an adapter read: its label for the status row, its path
    /// as the request carried it, and its photos in listing order.
    pub fn open(&mut self, label: &str, path: &[u8], photos: Vec<Photo>) -> Result<(), Error> {
        let bytes = photos
            .iter()
            .try_fold(0usize, |total, photo| total.checked_add(photo.bytes()))
            .filter(|bytes| *bytes <= MAX_SIDECAR_TOTAL)
            .ok_or(Error::Refused)?;
        if photos.len() > MAX_PHOTOS {
            return Err(Error::Refused);
        }
        self.bytes = bytes;
        self.roll = Some(Roll {
            label: label.to_string(),
            path: path.to_vec(),
        });
        self.photos = photos;
        self.filter = Filter::All;
        self.view = View::Grid;
        self.first_row = 0;
        self.refresh_shown();
        self.cursor = self.shown.first().copied();
        self.bump();
        Ok(())
    }

    /// Whether a sidecar of `bytes` in place of the photo at `index` keeps
    /// the roll under its budget: what the adapter asks before it writes.
    pub fn fits(&self, index: usize, bytes: usize) -> bool {
        self.photos.get(index).is_some_and(|old| {
            self.bytes.saturating_sub(old.bytes()).saturating_add(bytes) <= MAX_SIDECAR_TOTAL
        })
    }

    /// One photo as the adapter found it after carrying out an effect for
    /// it: what it wrote, or what the file holds when it could not. A flag
    /// changes the model here and nowhere else, so a photo that is what the
    /// model already holds (a write the file refused) is no change and
    /// leaves the generation; one that differs takes the filter, which may
    /// hide it and move the cursor on. The budget holds here as at open: a
    /// sidecar that would take the roll past it is not held, and the photo
    /// is shown as refused for `OVER_BUDGET`. Whether the model changed.
    pub fn settle(&mut self, index: usize, photo: Photo) -> bool {
        let photo = if self.fits(index, photo.bytes()) {
            photo
        } else {
            Photo {
                name: photo.name,
                sidecar: None,
                error: Some(OVER_BUDGET.to_string()),
            }
        };
        let Some(slot) = self.photos.get_mut(index) else {
            return false;
        };
        if *slot == photo {
            return false;
        }
        self.bytes = self
            .bytes
            .saturating_sub(slot.bytes())
            .saturating_add(photo.bytes());
        *slot = photo;
        self.refresh_shown();
        self.keep_cursor_shown();
        self.bump();
        true
    }

    pub fn surface(&self) -> Surface {
        self.surface
    }

    /// The open roll's path, as the request that opened it carried it.
    pub fn roll(&self) -> Option<&[u8]> {
        self.roll.as_ref().map(|roll| roll.path.as_slice())
    }

    pub fn photos(&self) -> &[Photo] {
        &self.photos
    }

    pub fn cursor(&self) -> Option<usize> {
        self.cursor
    }

    pub fn filter(&self) -> Filter {
        self.filter
    }

    pub fn view(&self) -> View {
        self.view
    }

    pub fn first_row(&self) -> usize {
        self.first_row
    }

    /// Bumped on every change, so a frame can be told from the last.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The outstanding job count the adapter last reported.
    pub fn jobs(&self) -> usize {
        self.jobs
    }

    /// The adapter's outstanding job count: a fact `state` reports, not a
    /// change to the frame, so the generation stays.
    pub fn set_jobs(&mut self, jobs: usize) {
        self.jobs = jobs;
    }

    /// Something the adapter paints into the frame changed (a thumbnail
    /// arrived for a photo on screen): a new generation.
    pub fn touch(&mut self) {
        self.bump();
    }

    /// The indices the filter admits, in roll order.
    pub fn shown(&self) -> &[usize] {
        &self.shown
    }

    fn refresh_shown(&mut self) {
        let filter = self.filter;
        self.shown.clear();
        self.shown.extend(
            self.photos
                .iter()
                .enumerate()
                .filter(|(_, photo)| filter.admits(photo.flag()))
                .map(|(index, _)| index),
        );
    }

    pub fn layout(&self) -> Layout {
        Layout::new(self.surface)
    }

    /// The shown photos on screen and the boxes their thumbnails go in, in
    /// the grid; nothing in the single view, whose box is the develop
    /// increment's preview, and nothing before a roll.
    pub fn visible(&self) -> Vec<(usize, Rect)> {
        if self.roll.is_none() || self.view != View::Grid {
            return Vec::new();
        }
        let layout = self.layout();
        let first = self.first_row * layout.columns;
        self.shown
            .iter()
            .skip(first)
            .take(layout.rows * layout.columns)
            .enumerate()
            .filter_map(|(offset, index)| {
                let cell = layout.cell(first + offset, self.first_row)?;
                Some((*index, layout.thumb(cell)))
            })
            .collect()
    }

    /// The photos whose thumbnails the window wants, in the order it wants
    /// them: the grid's screen, then the screen below it, then the one
    /// above, whichever view is showing, so the grid is ready when the
    /// single view returns to it.
    pub fn wanted(&self) -> Vec<usize> {
        if self.roll.is_none() {
            return Vec::new();
        }
        let layout = self.layout();
        let screen = layout.rows * layout.columns;
        let first = self.first_row * layout.columns;
        let mut wanted: Vec<usize> = self
            .shown
            .iter()
            .skip(first)
            .take(2 * screen)
            .copied()
            .collect();
        let above = first.saturating_sub(screen);
        wanted.extend(self.shown.iter().skip(above).take(first - above));
        wanted
    }

    /// One named action with the fields the request carried.
    pub fn action(
        &mut self,
        name: &str,
        arguments: &[&str],
    ) -> Result<(Outcome, Vec<Effect>), Error> {
        let action = Action::parse(name).ok_or(control::Error::Protocol)?;
        self.dispatch(action, arguments)
    }

    /// One input through the key and pointer paths the window uses.
    pub fn input(&mut self, input: Input<'_>) -> Result<(Outcome, Vec<Effect>), Error> {
        match input {
            Input::Key { chord } => match driven::bound(&BINDINGS, chord) {
                Some(binding) => {
                    let action = Action::parse(binding.name).ok_or(Error::BadArgument)?;
                    self.dispatch(action, &[])
                }
                None => Ok((Outcome::Ignored, Vec::new())),
            },
            Input::Pointer { phase, x, y } => self.pointer(phase, i64::from(x), i64::from(y)),
            Input::Wheel { rows, .. } => {
                let outcome = self.scroll(i64::from(rows));
                Ok((self.finish(outcome), Vec::new()))
            }
            Input::Resize {
                width,
                height,
                scale,
            } => {
                let scale = Scale::new(scale).map_err(|_| Error::BadArgument)?;
                let surface = Surface::new(width, height, scale).map_err(|_| Error::BadArgument)?;
                if surface == self.surface {
                    return Ok((Outcome::Ignored, Vec::new()));
                }
                self.surface = surface;
                self.reveal();
                self.bump();
                Ok((Outcome::Changed, Vec::new()))
            }
            Input::Focus(_) | Input::Tick(_) => Ok((Outcome::Ignored, Vec::new())),
        }
    }

    /// The tab-separated facts: the mode, the roll's path in hex, how many
    /// photos and how many shown, the cursor's position among the shown,
    /// the filter, the view, then the photo under the cursor (its name in
    /// hex, flag, exposure, crop, look, sidecar state), the outstanding job
    /// count and the generation. Absent values are `-`.
    pub fn state(&self) -> String {
        let position = self.position();
        let photo = self.cursor.and_then(|index| self.photos.get(index));
        let dash = || "-".to_string();
        [
            MODE.to_string(),
            self.roll.as_ref().map_or_else(dash, |roll| hex(&roll.path)),
            self.photos.len().to_string(),
            self.shown.len().to_string(),
            position.map_or_else(dash, |p| p.to_string()),
            self.filter.word().to_string(),
            self.view.word().to_string(),
            photo.map_or_else(dash, |p| hex(p.name.as_bytes())),
            photo.map_or_else(dash, |p| p.value(Key::Flag).to_string()),
            photo.map_or_else(dash, |p| p.value(Key::Exposure).to_string()),
            photo.map_or_else(dash, |p| p.value(Key::Crop).to_string()),
            photo.map_or_else(dash, |p| p.value(Key::Look).to_string()),
            photo.map_or_else(dash, |p| p.status().to_string()),
            self.jobs.to_string(),
            self.generation.to_string(),
        ]
        .join("\t")
    }

    /// The `photo N` body: the Nth shown photo's name in hex, flag,
    /// exposure, crop, look, sidecar state and, for a refused sidecar, the
    /// reason in hex (`-` otherwise).
    pub fn photo(&self, position: usize) -> Result<String, Error> {
        if self.roll.is_none() {
            return Err(Error::NoRoll);
        }
        let index = *self.shown.get(position).ok_or(Error::NoPhoto)?;
        let photo = self.photos.get(index).ok_or(Error::NoPhoto)?;
        let reason = photo
            .error
            .as_ref()
            .map_or_else(|| "-".to_string(), |error| hex(error.as_bytes()));
        let name = hex(photo.name.as_bytes());
        Ok([
            name.as_str(),
            photo.value(Key::Flag),
            photo.value(Key::Exposure),
            photo.value(Key::Crop),
            photo.value(Key::Look),
            photo.status(),
            reason.as_str(),
        ]
        .join("\t"))
    }

    /// What the window shows now, borrowing the model.
    pub fn scene(&self) -> Scene<'_> {
        Scene { model: self }
    }

    /// The flag badges of the photos on screen, alone: what the window
    /// paints again after the thumbnails, which cover the corner the scene
    /// painted them in.
    pub fn badges(&self) -> Badges<'_> {
        Badges { model: self }
    }

    fn bump(&mut self) {
        self.generation = self.generation.wrapping_add(1);
    }

    /// A change is a new generation; anything else leaves it.
    fn finish(&mut self, outcome: Outcome) -> Outcome {
        if outcome == Outcome::Changed {
            self.bump();
        }
        outcome
    }

    /// The cursor's position among the shown; the list is ascending.
    fn position(&self) -> Option<usize> {
        self.shown.binary_search(&self.cursor?).ok()
    }

    fn dispatch(
        &mut self,
        action: Action,
        arguments: &[&str],
    ) -> Result<(Outcome, Vec<Effect>), Error> {
        let mut effects = Vec::new();
        let outcome = match (action, arguments) {
            (Action::Open, [path]) => {
                // The change is the adapter's to make: `open` bumps when the
                // roll is installed, and a folder that cannot be read leaves
                // the generation alone.
                effects.push(Effect::Open(control::unhex(path)?));
                return Ok((Outcome::Changed, effects));
            }
            (Action::Quit, []) => Outcome::Quit,
            (Action::Scroll, [rows]) => self.scroll(signed(rows)?),
            (Action::All, []) => self.set_filter(Filter::All),
            (Action::Picks, []) => self.set_filter(Filter::Picks),
            (Action::Rejects, []) => self.set_filter(Filter::Rejects),
            (Action::Unflagged, []) => self.set_filter(Filter::Unflagged),
            (Action::Grid, []) => self.set_view(View::Grid),
            (Action::Next, []) => self.step(1)?,
            (Action::Previous, []) => self.step(-1)?,
            (Action::Down, []) => self.step(self.layout().columns as i64)?,
            (Action::Up, []) => self.step(-(self.layout().columns as i64))?,
            (Action::PageDown, []) => {
                let layout = self.layout();
                self.step((layout.rows * layout.columns) as i64)?
            }
            (Action::PageUp, []) => {
                let layout = self.layout();
                self.step(-((layout.rows * layout.columns) as i64))?
            }
            (Action::First, []) => self.step(i64::MIN)?,
            (Action::Last, []) => self.step(i64::MAX)?,
            (Action::Select, [position]) => {
                let position =
                    usize::try_from(decimal(position)?).map_err(|_| Error::BadArgument)?;
                self.select(position)?
            }
            (Action::View, []) => {
                self.need_photo()?;
                let view = match self.view {
                    View::Grid => View::Single,
                    View::Single => View::Grid,
                };
                self.set_view(view)
            }
            (Action::Pick, []) => return self.flag(Some(Flag::Pick), effects),
            (Action::Reject, []) => return self.flag(Some(Flag::Reject), effects),
            (Action::Unflag, []) => return self.flag(None, effects),
            _ => return Err(control::Error::Protocol.into()),
        };
        Ok((self.finish(outcome), effects))
    }

    fn pointer(
        &mut self,
        phase: PointerPhase,
        x: i64,
        y: i64,
    ) -> Result<(Outcome, Vec<Effect>), Error> {
        // Only a press, and only on the surface: a header the width does
        // not show is not a target. The bands are tested last painted
        // first, so on a surface too short for both the status row covers
        // the bar's headers as it covers their pixels.
        if phase != PointerPhase::Press
            || !self.surface.bounds().contains(x, y)
            || Status::new(self.surface).rect().contains(x, y)
        {
            return Ok((Outcome::Ignored, Vec::new()));
        }
        let labels = labels(self.filter);
        let names = names(&labels);
        if let Some(index) = Bar::new(self.surface, &names).hit(x, y) {
            let Some((filter, _)) = FILTERS.get(index) else {
                return Ok((Outcome::Ignored, Vec::new()));
            };
            let outcome = self.set_filter(*filter);
            return Ok((self.finish(outcome), Vec::new()));
        }
        if self.roll.is_none() {
            return Ok((Outcome::Ignored, Vec::new()));
        }
        let outcome = match self.view {
            View::Single if self.layout().area.contains(x, y) => self.set_view(View::Grid),
            View::Single => Outcome::Ignored,
            View::Grid => match self.layout().position_at(x, y, self.first_row) {
                Some(position) if position < self.shown.len() => self.select(position)?,
                _ => Outcome::Ignored,
            },
        };
        Ok((self.finish(outcome), Vec::new()))
    }

    fn need_roll(&self) -> Result<(), Error> {
        if self.roll.is_none() {
            return Err(Error::NoRoll);
        }
        Ok(())
    }

    fn need_photo(&self) -> Result<usize, Error> {
        self.need_roll()?;
        self.cursor.ok_or(Error::NoPhoto)
    }

    fn set_filter(&mut self, filter: Filter) -> Outcome {
        if self.filter == filter {
            return Outcome::Ignored;
        }
        self.filter = filter;
        self.refresh_shown();
        self.keep_cursor_shown();
        Outcome::Changed
    }

    fn set_view(&mut self, view: View) -> Outcome {
        if self.view == view {
            return Outcome::Ignored;
        }
        self.view = view;
        Outcome::Changed
    }

    /// The cursor stays where it is when the filter still shows it, else
    /// goes to the first shown photo, else nowhere, and the single view
    /// ends with it, since it is a view of the cursor's photo.
    fn keep_cursor_shown(&mut self) {
        if self.position().is_none() {
            self.cursor = self.shown.first().copied();
        }
        if self.cursor.is_none() {
            self.view = View::Grid;
        }
        self.reveal();
    }

    /// Moves the cursor by `delta` positions among the shown, clamped to the
    /// ends; `Ignored` at an end it is already at.
    fn step(&mut self, delta: i64) -> Result<Outcome, Error> {
        self.need_photo()?;
        let Some(position) = self.position() else {
            return Err(Error::NoPhoto);
        };
        let last = self.shown.len().saturating_sub(1) as i64;
        let target = (position as i64).saturating_add(delta).clamp(0, last);
        self.select(usize::try_from(target).map_err(|_| Error::NoPhoto)?)
    }

    fn select(&mut self, position: usize) -> Result<Outcome, Error> {
        self.need_roll()?;
        let index = *self.shown.get(position).ok_or(Error::NoPhoto)?;
        if self.cursor == Some(index) {
            return Ok(Outcome::Ignored);
        }
        self.cursor = Some(index);
        self.reveal();
        Ok(Outcome::Changed)
    }

    /// Scrolls the grid by whole rows, clamped to the roll; the cursor
    /// stays where it is.
    fn scroll(&mut self, rows: i64) -> Outcome {
        let layout = self.layout();
        let total = self.shown.len().div_ceil(layout.columns);
        let last = total.saturating_sub(layout.rows) as i64;
        let first = (self.first_row as i64)
            .saturating_add(rows)
            .clamp(0, last.max(0));
        let first = usize::try_from(first).unwrap_or(0);
        if first == self.first_row {
            return Outcome::Ignored;
        }
        self.first_row = first;
        Outcome::Changed
    }

    /// Keeps the cursor's row on the area and the scroll inside the roll.
    fn reveal(&mut self) {
        let layout = self.layout();
        let total = self.shown.len().div_ceil(layout.columns);
        self.first_row = self.first_row.min(total.saturating_sub(layout.rows));
        if let Some(position) = self.position() {
            let row = position / layout.columns;
            if row < self.first_row {
                self.first_row = row;
            } else if row >= self.first_row + layout.rows {
                self.first_row = row + 1 - layout.rows;
            }
        }
    }

    /// Asks the adapter to set or clear the cursor photo's flag on its
    /// sidecar. Like `open`, the change is the adapter's to make: the model
    /// is untouched until `settle`, so a flag the file refuses leaves the
    /// model, the cursor and the generation as they were. The file may
    /// differ from the model's copy, so the dispatch asks even for the flag
    /// the model thinks the photo has and for a photo whose sidecar it
    /// holds as refused: the adapter calls the flag the file already holds
    /// `Ignored` and refuses a sidecar it cannot read, and the model takes
    /// the file's word either way.
    fn flag(
        &mut self,
        flag: Option<Flag>,
        mut effects: Vec<Effect>,
    ) -> Result<(Outcome, Vec<Effect>), Error> {
        let index = self.need_photo()?;
        let photo = self.photos.get(index).ok_or(Error::NoPhoto)?;
        effects.push(Effect::Flag {
            index,
            name: photo.name.clone(),
            flag,
        });
        Ok((Outcome::Changed, effects))
    }
}

/// The bar's labels: the active filter in brackets, the others padded to
/// the same width, so the headers keep their places whichever is active.
fn labels(active: Filter) -> [String; 4] {
    let label = |(filter, name): &(Filter, &str)| {
        if *filter == active {
            format!("[{name}]")
        } else {
            format!(" {name} ")
        }
    };
    [
        label(&FILTERS[0]),
        label(&FILTERS[1]),
        label(&FILTERS[2]),
        label(&FILTERS[3]),
    ]
}

fn names(labels: &[String; 4]) -> [&str; 4] {
    [
        labels[0].as_str(),
        labels[1].as_str(),
        labels[2].as_str(),
        labels[3].as_str(),
    ]
}

fn signed(value: &str) -> Result<i64, Error> {
    let digits = value.strip_prefix('-').unwrap_or(value);
    let magnitude = i64::try_from(decimal(digits)?).map_err(|_| Error::BadArgument)?;
    if magnitude > i64::from(driven::WHEEL_LIMIT) {
        return Err(Error::BadArgument);
    }
    Ok(if digits.len() == value.len() {
        magnitude
    } else {
        -magnitude
    })
}

fn fill(rect: Rect, color: u32, damage: Rect, sink: &mut dyn FnMut(Draw)) {
    if let Some(clip) = rect.intersection(damage) {
        sink(Draw {
            clip,
            primitive: Primitive::Fill { rect, color },
        });
    }
}

/// What the window shows: the filter bar, the grid or the single photo,
/// and the status row, laid out for the model's surface.
pub struct Scene<'a> {
    model: &'a Controller,
}

impl Scene<'_> {
    /// The status row's line.
    pub fn status_line(&self) -> String {
        let model = self.model;
        let Some(roll) = &model.roll else {
            return "No roll open".to_string();
        };
        let shown = model.shown();
        let mut line = format!(
            "{} | {} photos, {} shown | {}",
            roll.label,
            model.photos.len(),
            shown.len(),
            model.filter.word()
        );
        if let Some(position) = model.position() {
            if let Some(photo) = model.cursor.and_then(|index| model.photos.get(index)) {
                let flag = photo.flag().map_or("unflagged", Flag::word);
                line.push_str(&format!(
                    " | {}/{} {} {flag}",
                    position + 1,
                    shown.len(),
                    photo.name
                ));
                if photo.error.is_some() {
                    line.push_str(" (sidecar refused)");
                }
            }
        }
        if model.view == View::Single {
            line.push_str(" | single");
        }
        line
    }

    fn cell(
        &self,
        layout: &Layout,
        rect: Rect,
        photo: &Photo,
        selected: bool,
        damage: Rect,
        sink: &mut dyn FnMut(Draw),
    ) {
        let s = layout.surface.scale.value();
        let scale = layout.surface.scale;
        fill(rect, CHROME, damage, sink);
        if selected {
            let edge = (2 * s) as u32;
            fill(
                Rect {
                    height: edge,
                    ..rect
                },
                SELECTED,
                damage,
                sink,
            );
            fill(
                Rect {
                    y: rect.y + i64::from(rect.height) - i64::from(edge),
                    height: edge,
                    ..rect
                },
                SELECTED,
                damage,
                sink,
            );
            fill(
                Rect {
                    width: edge,
                    ..rect
                },
                SELECTED,
                damage,
                sink,
            );
            fill(
                Rect {
                    x: rect.x + i64::from(rect.width) - i64::from(edge),
                    width: edge,
                    ..rect
                },
                SELECTED,
                damage,
                sink,
            );
        }
        let pad = (CELL_PAD * s) as i64;
        let thumb = layout.thumb(rect);
        fill(thumb, PLACEHOLDER, damage, sink);
        badge(layout, thumb, photo, damage, sink);
        let ink = if photo.flag() == Some(Flag::Reject) {
            DISABLED & 0x00ff_ffff
        } else {
            INK
        };
        let name = Rect {
            x: thumb.x,
            y: thumb.y + i64::from(thumb.height) + pad / 2,
            width: thumb.width,
            height: (CELL_HEIGHT * s) as u32,
        };
        text_run(
            scale,
            photo.name.chars(),
            (name.x, name.y),
            name,
            GlyphStyle::medium(ink, CHROME),
            damage,
            sink,
        );
    }

    fn grid(&self, layout: &Layout, damage: Rect, sink: &mut dyn FnMut(Draw)) {
        let model = self.model;
        let shown = model.shown();
        fill(layout.area, PAPER, damage, sink);
        if shown.is_empty() {
            let text = if model.photos.is_empty() {
                "The roll holds no photos."
            } else {
                "No photo passes the filter."
            };
            if let Some(block) = Block::new(layout.surface, layout.area.y, 1) {
                block.emit(text, damage, sink);
            }
            return;
        }
        let first = model.first_row * layout.columns;
        for (offset, index) in shown
            .iter()
            .skip(first)
            .take(layout.rows * layout.columns)
            .enumerate()
        {
            let Some(photo) = model.photos.get(*index) else {
                continue;
            };
            let Some(rect) = layout.cell(first + offset, model.first_row) else {
                continue;
            };
            self.cell(
                layout,
                rect,
                photo,
                model.cursor == Some(*index),
                damage,
                sink,
            );
        }
    }

    fn single(&self, layout: &Layout, photo: &Photo, damage: Rect, sink: &mut dyn FnMut(Draw)) {
        let s = layout.surface.scale.value();
        let scale = layout.surface.scale;
        fill(layout.area, PAPER, damage, sink);
        let pad = (CELL_PAD * s) as i64;
        let row = (CELL_HEIGHT * s) as i64;
        let text = Rect {
            x: layout.area.x + pad,
            y: layout.area.y + pad / 2,
            width: (layout.area.width as i64 - 2 * pad).max(0) as u32,
            height: row as u32,
        };
        let style = GlyphStyle::medium(INK, PAPER);
        text_run(
            scale,
            photo.name.chars(),
            (text.x, text.y),
            text,
            style,
            damage,
            sink,
        );
        let facts = format!(
            "{} | exposure {} | crop {} | look {} | sidecar {}",
            photo.flag().map_or("unflagged", Flag::word),
            photo.value(Key::Exposure),
            photo.value(Key::Crop),
            photo.value(Key::Look),
            photo.status()
        );
        let second = Rect {
            y: text.y + row,
            ..text
        };
        text_run(
            scale,
            facts.chars(),
            (second.x, second.y),
            second,
            style,
            damage,
            sink,
        );
        // The preview's place: the largest 3:2 box under the two rows.
        let top = second.y + row + pad / 2;
        let bottom = layout.area.y + i64::from(layout.area.height) - pad;
        let available_h = (bottom - top).max(0);
        let available_w = i64::from(text.width);
        let width = available_w.min(available_h * 3 / 2);
        let height = width * 2 / 3;
        if width > 0 && height > 0 {
            fill(
                Rect {
                    x: text.x + (available_w - width) / 2,
                    y: top,
                    width: width as u32,
                    height: height as u32,
                },
                PLACEHOLDER,
                damage,
                sink,
            );
        }
    }
}

impl Composition for Scene<'_> {
    fn surface(&self) -> Surface {
        self.model.surface
    }

    fn emit(&self, damage: Rect, sink: &mut dyn FnMut(Draw)) {
        let model = self.model;
        let layout = model.layout();
        let labels = labels(model.filter);
        let names = names(&labels);
        Bar::new(layout.surface, &names).emit(damage, sink);
        match (&model.roll, model.view) {
            (None, _) => {
                fill(layout.area, PAPER, damage, sink);
                if let Some(block) = Block::new(layout.surface, layout.area.y, 2) {
                    block.emit(
                        "No roll open.\naction open HEX_PATH opens one; --replay ROLL opens it at start.",
                        damage,
                        sink,
                    );
                }
            }
            (Some(_), View::Grid) => self.grid(&layout, damage, sink),
            (Some(_), View::Single) => {
                match model.cursor.and_then(|index| model.photos.get(index)) {
                    Some(photo) => self.single(&layout, photo, damage, sink),
                    None => self.grid(&layout, damage, sink),
                }
            }
        }
        Status::new(layout.surface).emit(self.status_line().chars(), damage, sink);
    }
}

/// A flagged photo's badge at its box's corner: `P` for a pick, `X` for a
/// reject, in a glyph cell of the flag's colour.
fn badge(layout: &Layout, thumb: Rect, photo: &Photo, damage: Rect, sink: &mut dyn FnMut(Draw)) {
    let Some(flag) = photo.flag() else {
        return;
    };
    let s = layout.surface.scale.value();
    let (mark, color) = match flag {
        Flag::Pick => ('P', SELECTED),
        Flag::Reject => ('X', MISSPELLED),
    };
    let badge = Rect {
        x: thumb.x,
        y: thumb.y,
        width: ((CELL_WIDTH + 2) * s) as u32,
        height: (CELL_HEIGHT * s) as u32,
    };
    fill(badge, color, damage, sink);
    text_run(
        layout.surface.scale,
        std::iter::once(mark),
        (badge.x + s as i64, badge.y),
        badge,
        GlyphStyle::medium(PAPER, color),
        damage,
        sink,
    );
}

/// The badges of the photos on screen and nothing else, a composition the
/// window paints over the thumbnails it blitted, with the grid's area as
/// its damage as the blits are clipped to it, since the scene's status
/// band covers a badge that runs under it. So painted, over the scene's
/// own frame it changes nothing: the draws are the scene's, at the same
/// places.
pub struct Badges<'a> {
    model: &'a Controller,
}

impl Composition for Badges<'_> {
    fn surface(&self) -> Surface {
        self.model.surface
    }

    fn emit(&self, damage: Rect, sink: &mut dyn FnMut(Draw)) {
        let layout = self.model.layout();
        for (index, thumb) in self.model.visible() {
            if let Some(photo) = self.model.photos.get(index) {
                badge(&layout, thumb, photo, damage, sink);
            }
        }
    }
}

/// Paints `image` centred in `r#box`, clipped to `clip`, the box and the
/// surface, into an XRGB frame of `stride` bytes per row: the one place
/// photo pixels reach a frame, since the toolkit's raster paints fills and
/// glyphs. An image larger than the box shows its middle. A frame or stride
/// too small for the surface, or an image whose buffer is not its size, is
/// refused.
pub fn blit(
    pixels: &mut [u8],
    surface: Surface,
    stride: usize,
    clip: Rect,
    r#box: Rect,
    image: &Rgb8,
) -> Result<(), Error> {
    let bad = |_| Error::BadArgument;
    let needed = stride
        .checked_mul(surface.height)
        .ok_or(Error::BadArgument)?;
    let samples = image
        .width
        .checked_mul(image.height)
        .and_then(|n| n.checked_mul(3))
        .ok_or(Error::BadArgument)?;
    if stride < surface.width.saturating_mul(4)
        || pixels.len() < needed
        || image.data.len() != samples
    {
        return Err(Error::BadArgument);
    }
    // Nothing of the box on the surface within the clip is nothing to
    // paint, and spares the centring a box at an axis's end.
    let Some(clip) = clip
        .intersection(surface.bounds())
        .and_then(|clip| clip.intersection(r#box))
    else {
        return Ok(());
    };
    let (width, height) = (
        i64::try_from(image.width).map_err(bad)?,
        i64::try_from(image.height).map_err(bad)?,
    );
    let x = r#box
        .x
        .checked_add((i64::from(r#box.width) - width) / 2)
        .ok_or(Error::BadArgument)?;
    let y = r#box
        .y
        .checked_add((i64::from(r#box.height) - height) / 2)
        .ok_or(Error::BadArgument)?;
    let target = Rect {
        x,
        y,
        width: u32::try_from(image.width).map_err(bad)?,
        height: u32::try_from(image.height).map_err(bad)?,
    };
    let Some(clip) = clip.intersection(target) else {
        return Ok(());
    };
    let columns = clip.width as usize;
    let source_x = usize::try_from(clip.x - x).map_err(bad)?;
    let source_y = usize::try_from(clip.y - y).map_err(bad)?;
    let target_x = usize::try_from(clip.x).map_err(bad)?;
    let target_y = usize::try_from(clip.y).map_err(bad)?;
    for row in 0..clip.height as usize {
        let from = ((source_y + row) * image.width + source_x) * 3;
        let to = (target_y + row) * stride + target_x * 4;
        let source = image
            .data
            .get(from..from + columns * 3)
            .ok_or(Error::BadArgument)?;
        let target = pixels
            .get_mut(to..to + columns * 4)
            .ok_or(Error::BadArgument)?;
        let (targets, _) = target.as_chunks_mut::<4>();
        let (sources, _) = source.as_chunks::<3>();
        for ([b, g, r, pad], [red, green, blue]) in targets.iter_mut().zip(sources) {
            *b = *blue;
            *g = *green;
            *r = *red;
            *pad = 0;
        }
    }
    Ok(())
}

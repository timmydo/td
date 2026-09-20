//! The cull controller: the roll as the window shows it, a grid of photos
//! or one of them, the closed action table an agent reads, and the one
//! dispatcher the keyboard, the pointer, the replay and the socket all go
//! through. Pure: it holds names and sidecars, returns the effects an
//! adapter carries out (open this folder, write that sidecar) and lays out
//! a scene the toolkit paints; `main` owns the files.

use std::fmt;

use td_ui::chrome::{
    Block, Button, Buttons, Item, List, Slider, Status, BUTTON_MARGIN, DISABLED, ROW,
};
use td_ui::control::{self, decimal, hex, ErrorCode};
use td_ui::driven::{self, Binding, Input, Outcome, PointerPhase};
use td_ui::finder;
use td_ui::raster::{
    text_run, Composition, Draw, GlyphStyle, Primitive, Rect, Scale, Surface, CHROME, INK,
    MISSPELLED, PAPER, SELECTED,
};
// One toolkit name per line: tests/confinement.rs reads the name after the
// crate's path.
use td_ui::CELL_HEIGHT;
use td_ui::CELL_WIDTH;

use crate::image::Rgb8;
use crate::library::{self, Crop, Filter, Flag, Key, Sidecar, Step};

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
/// The coarse exposure step, a third of a stop in hundredths.
pub const EXPOSURE_STEP: i32 = 33;
/// The fine exposure step, a tenth of a stop in hundredths.
pub const EXPOSURE_FINE: i32 = 10;

/// The mode `state` leads with: culling the roll as a grid, or developing
/// one photo. The develop edits act on the cursor's photo; the cull view
/// (grid or single) is where the mode returns.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Mode {
    Cull,
    Develop,
}

impl Mode {
    pub fn word(self) -> &'static str {
        match self {
            Self::Cull => "cull",
            Self::Develop => "develop",
        }
    }
}

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
    sidecar.bytes()
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
    /// Set or clear one develop key (crop or look) in this photo's
    /// sidecar, on the file as it is then, and settle the model.
    Edit {
        index: usize,
        name: String,
        key: Key,
        value: Option<String>,
    },
    /// Add `delta` hundredths of a stop to this photo's exposure on the
    /// file as it is then, clamped to the exposure range, and settle the
    /// model: a delta, not an absolute, so a value changed meanwhile is
    /// added to.
    Expose {
        index: usize,
        name: String,
        delta: i32,
    },
    /// Reset this photo's develop keys to camera defaults, keeping the
    /// flag, on the file as it is then, and settle the model.
    Reset { index: usize, name: String },
    /// Take the last step of this photo's develop history back, on the
    /// file as it is then, and settle the model; a history with no step
    /// is left as it is, which is `ignored`.
    Undo { index: usize, name: String },
    /// Turn step `step` (from 0) of this photo's develop history off or
    /// back on, on the file as it is then, and settle the model; a step
    /// the file does not hold is `ignored`.
    StepToggle {
        index: usize,
        name: String,
        step: usize,
    },
    /// Delete step `step` (from 0) of this photo's develop history, the
    /// later ones closing up, on the file as it is then, and settle the
    /// model; a step the file does not hold is `ignored`.
    StepDelete {
        index: usize,
        name: String,
        step: usize,
    },
    /// Export this photo at full resolution into the roll's `exported/`
    /// through its sidecar as the file holds it then: the replay runs the
    /// verb on the request, the window hands it to its pool; either says
    /// what came of it through `set_export`, the status row's note.
    Export { index: usize, name: String },
    /// Move the roll's rejects, as the files flag them then, with their
    /// sidecars into `rejected/`, and take the moved ones out of the model
    /// through `remove`.
    DeleteRejected,
    /// List `folder` (`None`: the adapter's working directory) for the
    /// roll chooser, or with `parent` its parent with `folder` itself
    /// selected (the folder an ascent leaves, the open roll at first; the
    /// root's parent is the root): its subfolders and originals, handed
    /// back through `set_listing`, the folder's own name chosen in its
    /// parent, or the refusal noted through `note_listing`.
    List {
        folder: Option<Vec<u8>>,
        parent: bool,
    },
}

/// The closed set of things the window does. The table below is its
/// public face; `ALL` and `BINDINGS` are aligned by index and a test pins
/// that.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Action {
    Open,
    Choose,
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
    Develop,
    ExposeIn,
    ExposeOut,
    ExposeInFine,
    ExposeOutFine,
    Look,
    Crop,
    AdjustCrop,
    Aspect,
    Looks,
    Reset,
    Undo,
    StepToggle,
    StepDelete,
    Uncrop,
    Exposure,
    /// The Nth (from 1) of the available looks, `F1`..`F9`.
    LookAt(u8),
    Export,
    DeleteRejected,
    Scroll,
    Quit,
}

impl Action {
    pub const ALL: [Action; 49] = [
        Action::Open,
        Action::Choose,
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
        Action::Develop,
        Action::ExposeIn,
        Action::ExposeOut,
        Action::ExposeInFine,
        Action::ExposeOutFine,
        Action::Look,
        Action::Crop,
        Action::AdjustCrop,
        Action::Aspect,
        Action::Looks,
        Action::Reset,
        Action::Undo,
        Action::StepToggle,
        Action::StepDelete,
        Action::Uncrop,
        Action::Exposure,
        Action::LookAt(1),
        Action::LookAt(2),
        Action::LookAt(3),
        Action::LookAt(4),
        Action::LookAt(5),
        Action::LookAt(6),
        Action::LookAt(7),
        Action::LookAt(8),
        Action::LookAt(9),
        Action::Export,
        Action::DeleteRejected,
        Action::Scroll,
        Action::Quit,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Choose => "choose",
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
            Self::Develop => "develop",
            Self::ExposeIn => "expose-in",
            Self::ExposeOut => "expose-out",
            Self::ExposeInFine => "expose-in-fine",
            Self::ExposeOutFine => "expose-out-fine",
            Self::Look => "look",
            Self::Crop => "crop",
            Self::AdjustCrop => "adjust-crop",
            Self::Aspect => "aspect",
            Self::Looks => "looks",
            Self::Reset => "reset",
            Self::Undo => "undo",
            Self::StepToggle => "step-toggle",
            Self::StepDelete => "step-delete",
            Self::Uncrop => "uncrop",
            Self::Exposure => "exposure",
            Self::LookAt(1) => "look-1",
            Self::LookAt(2) => "look-2",
            Self::LookAt(3) => "look-3",
            Self::LookAt(4) => "look-4",
            Self::LookAt(5) => "look-5",
            Self::LookAt(6) => "look-6",
            Self::LookAt(7) => "look-7",
            Self::LookAt(8) => "look-8",
            Self::LookAt(9) => "look-9",
            // Not an action: `parse` refuses it and dispatch ignores it.
            Self::LookAt(_) => "look-0",
            Self::Export => "export",
            Self::DeleteRejected => "delete-rejected",
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
pub const BINDINGS: [Binding; 49] = [
    Binding {
        name: "open",
        chord: None,
        arguments: "HEX_PATH",
        help: "Open the roll at the path, given as hex bytes.",
    },
    Binding {
        name: "choose",
        chord: Some("o"),
        arguments: "",
        help: "Open the roll chooser, a finder over the folders; the prompt names its keys. The action closes an open one.",
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
        help: "Move the cursor one grid row down; in develop mode, the history selection down.",
    },
    Binding {
        name: "up",
        chord: Some("Up"),
        arguments: "",
        help: "Move the cursor one grid row up; in develop mode, the history selection up.",
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
        help: "Back to the grid, leaving develop mode.",
    },
    Binding {
        name: "develop",
        chord: Some("d"),
        arguments: "",
        help: "Develop the photo under the cursor.",
    },
    Binding {
        name: "expose-in",
        chord: Some("="),
        arguments: "",
        help: "Raise exposure a third of a stop (develop mode).",
    },
    Binding {
        name: "expose-out",
        chord: Some("-"),
        arguments: "",
        help: "Lower exposure a third of a stop (develop mode).",
    },
    Binding {
        name: "expose-in-fine",
        chord: Some("+"),
        arguments: "",
        help: "Raise exposure a tenth of a stop (develop mode).",
    },
    Binding {
        name: "expose-out-fine",
        chord: Some("_"),
        arguments: "",
        help: "Lower exposure a tenth of a stop (develop mode).",
    },
    Binding {
        name: "look",
        chord: None,
        arguments: "STEM",
        help: "Set the develop look to STEM, or - to clear it.",
    },
    Binding {
        name: "crop",
        chord: None,
        arguments: "X Y W H",
        help: "Set the develop crop to the fractions X Y W H.",
    },
    Binding {
        name: "adjust-crop",
        chord: Some("c"),
        arguments: "",
        help: "Toggle crop adjust: drag the crop's edges and corners (develop mode).",
    },
    Binding {
        name: "aspect",
        chord: None,
        arguments: "RATIO",
        help: "Lock the crop drag to a ratio: free, 3:2, 4:3, 1:1 or 16:9 (develop mode).",
    },
    Binding {
        name: "looks",
        chord: Some("l"),
        arguments: "",
        help: "Toggle the look palette: the available looks, the current marked (develop mode).",
    },
    Binding {
        name: "reset",
        chord: Some("0"),
        arguments: "",
        help: "Reset exposure, crop and look to camera defaults, clearing the history (develop mode).",
    },
    Binding {
        name: "undo",
        chord: Some("z"),
        arguments: "",
        help: "Take the last step of the history back (develop mode).",
    },
    Binding {
        name: "step-toggle",
        chord: Some("t"),
        arguments: "",
        help: "Turn the selected history step off, or back on (develop mode).",
    },
    Binding {
        name: "step-delete",
        chord: Some("Backspace"),
        arguments: "",
        help: "Delete the selected history step (develop mode).",
    },
    Binding {
        name: "uncrop",
        chord: Some("C"),
        arguments: "",
        help: "Clear the crop, back to the whole frame (develop mode).",
    },
    Binding {
        name: "exposure",
        chord: None,
        arguments: "STOPS",
        help: "Set the exposure to STOPS, two decimals in -5.00..5.00 (develop mode); the slider commits one on release.",
    },
    Binding {
        name: "look-1",
        chord: Some("F1"),
        arguments: "",
        help: "Set the look to the first of the available looks, the look strip's order (develop mode).",
    },
    Binding {
        name: "look-2",
        chord: Some("F2"),
        arguments: "",
        help: "Set the look to the second available look (develop mode).",
    },
    Binding {
        name: "look-3",
        chord: Some("F3"),
        arguments: "",
        help: "Set the look to the third available look (develop mode).",
    },
    Binding {
        name: "look-4",
        chord: Some("F4"),
        arguments: "",
        help: "Set the look to the fourth available look (develop mode).",
    },
    Binding {
        name: "look-5",
        chord: Some("F5"),
        arguments: "",
        help: "Set the look to the fifth available look (develop mode).",
    },
    Binding {
        name: "look-6",
        chord: Some("F6"),
        arguments: "",
        help: "Set the look to the sixth available look (develop mode).",
    },
    Binding {
        name: "look-7",
        chord: Some("F7"),
        arguments: "",
        help: "Set the look to the seventh available look (develop mode).",
    },
    Binding {
        name: "look-8",
        chord: Some("F8"),
        arguments: "",
        help: "Set the look to the eighth available look (develop mode).",
    },
    Binding {
        name: "look-9",
        chord: Some("F9"),
        arguments: "",
        help: "Set the look to the ninth available look (develop mode).",
    },
    Binding {
        name: "export",
        chord: Some("e"),
        arguments: "",
        help: "Export the photo under the cursor at full resolution into exported/, with its sidecar's edits.",
    },
    Binding {
        name: "delete-rejected",
        chord: Some("Delete"),
        arguments: "",
        help: "Move the rejects and their sidecars into rejected/ under the roll; no name there is replaced.",
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

/// The filters in the order the filter strip shows them.
pub const FILTERS: [(Filter, &str); 4] = [
    (Filter::All, "All"),
    (Filter::Picks, "Picks"),
    (Filter::Rejects, "Rejects"),
    (Filter::Unflagged, "Unflagged"),
];

/// The filter strip's labels, `FILTERS` in order.
const FILTER_NAMES: [&str; 4] = [FILTERS[0].1, FILTERS[1].1, FILTERS[2].1, FILTERS[3].1];

/// The mode strip's labels: the roll chooser, the cull grid and develop,
/// in the order `mode_states` reports them.
const MODES: [&str; 3] = ["Roll Selection", "Culling", "Develop"];

/// The develop history pane's width in reference pixels: 27 cells, room
/// for an exposure or look step's row (a crop's shows in whole percents,
/// `step_label`) and for the three buttons under the list.
pub const PANE_W: usize = 216;

/// The filmstrip band's height in reference pixels: a thumbnail and its
/// padding, half above and half below.
pub const FILM_H: usize = THUMB_HEIGHT + CELL_PAD;

/// A history step as the pane lists it: `KEY VALUE`, `-` a clear, a crop
/// as `x,y wxh` in whole percents so it fits the pane's row.
pub fn step_label(step: &Step) -> String {
    let value = match (step.key, step.value.as_deref()) {
        (_, None) => "-".to_string(),
        (Key::Crop, Some(text)) => match Crop::parse(text) {
            Ok(crop) => {
                let pct = |n: u32| (n + library::CROP_UNIT / 200) / (library::CROP_UNIT / 100);
                format!(
                    "{},{} {}x{}",
                    pct(crop.x),
                    pct(crop.y),
                    pct(crop.width),
                    pct(crop.height)
                )
            }
            Err(_) => text.to_string(),
        },
        (_, Some(text)) => text.to_string(),
    };
    format!("{} {value}", step.key.name())
}

/// The buttons under the history pane, in order: the selected step off or
/// on, the selected step deleted, the last step taken back.
pub const HISTORY_BUTTONS: [&str; 3] = ["Toggle", "Delete", "Undo"];

/// The tool band's buttons, in order, before the exposure slider: the
/// crop-adjust sub-mode (selected while it is on), the crop cleared, the
/// last step back, the history cleared, and the exposure a third of a
/// stop down or up.
pub const TOOL_BUTTONS: [&str; 6] = ["Crop", "Uncrop", "Undo", "Reset", "-", "+"];
// The layout, the states and the paint zip the six by place.
const _: () = assert!(TOOL_BUTTONS.len() == 6);

/// The look band's first button: no look, the camera's rendering.
pub const NO_LOOK: &str = "None";

/// The exposure slider's steps: a tenth of a stop each over the range,
/// so value 0 is `-MAX_EXPOSURE`, 50 is zero and 100 is `MAX_EXPOSURE`.
pub const EXPOSURE_STEPS: usize = 100;
const _: () = assert!(library::MAX_EXPOSURE == 5 * EXPOSURE_STEPS as i32);

/// The slider's value for an exposure in hundredths of a stop.
pub fn exposure_value(hundredths: i32) -> usize {
    let offset = (hundredths.clamp(-library::MAX_EXPOSURE, library::MAX_EXPOSURE)
        + library::MAX_EXPOSURE) as usize;
    (offset + 5) / 10
}

/// The exposure, in hundredths of a stop, at a slider value.
pub fn exposure_at(value: usize) -> i32 {
    (value.min(EXPOSURE_STEPS) as i32) * 10 - library::MAX_EXPOSURE
}

/// Where a single view paints: its region, the preview box under the
/// text rows, and whether the facts row is among them.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SingleView {
    region: Rect,
    r#box: Option<Rect>,
    facts: bool,
}

/// The tool band's controls: its buttons, `TOOL_BUTTONS` in order, and
/// the exposure slider after them, each `None` where the band cannot hold
/// it whole.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Tools {
    pub buttons: [Option<Button>; 6],
    pub slider: Option<Slider>,
}

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

/// The grid's geometry on a surface: the mode and filter strips above,
/// the status row below, and whole cells in the area between.
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
        let height = surface.height.saturating_sub(3 * band);
        let area = Rect {
            x: 0,
            y: (2 * band) as i64,
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

    /// The single and develop view's preview box: the largest 3:2 rectangle
    /// under the name and facts rows, centred in the area's width, or `None`
    /// when the area leaves no room for one. The scene fills it with a
    /// placeholder; the window and `--preview` blit the developed image into
    /// it in develop mode, so all three agree on where the pixels go.
    pub fn preview_box(&self) -> Option<Rect> {
        self.box_in(self.area, 2)
    }

    /// The develop view's box: under the name row in the area right of
    /// the history pane, below the two bands and above the filmstrip.
    pub fn develop_box(&self) -> Option<Rect> {
        self.box_in(self.develop_view(), 1)
    }

    /// The tool band: the develop region's first `ROW`.
    pub fn tool_band(&self) -> Rect {
        let region = self.develop_region();
        Rect {
            height: region.height.min((ROW * self.surface.scale.value()) as u32),
            ..region
        }
    }

    /// The look band: the `ROW` under the tool band.
    pub fn look_band(&self) -> Rect {
        let region = self.develop_region();
        let band = (ROW * self.surface.scale.value()) as u32;
        Rect {
            y: region.y + i64::from(band),
            height: region.height.saturating_sub(band).min(band),
            ..region
        }
    }

    /// The develop region under its two bands.
    fn below_bands(&self) -> Rect {
        let region = self.develop_region();
        let bands = 2 * (ROW * self.surface.scale.value()) as u32;
        Rect {
            y: region.y + i64::from(bands),
            height: region.height.saturating_sub(bands),
            ..region
        }
    }

    /// The filmstrip band: `FILM_H` at the foot of the region under the
    /// bands, laid only when it can hold a box whole and the view above it
    /// keeps a name row and a box at least a thumbnail tall; `None`
    /// otherwise, and the view keeps the foot.
    pub fn film_band(&self) -> Option<Rect> {
        let s = self.surface.scale.value();
        let below = self.below_bands();
        let film = (FILM_H * s) as u32;
        let keep = ((ROW + FILM_H) * s) as u32;
        let one = ((2 * CELL_WIDTH + THUMB_WIDTH) * s) as u32;
        if below.height < film.saturating_add(keep) || below.width < one {
            return None;
        }
        Some(Rect {
            y: below.y + i64::from(below.height - film),
            height: film,
            ..below
        })
    }

    /// The filmstrip's boxes with `shown` photos and the cursor at
    /// `position` among them: `THUMB_WIDTH` by `THUMB_HEIGHT` from a cell
    /// in, a cell between, as many as the band holds whole, the cursor's
    /// kept centred as the ends allow; each with the shown position it
    /// holds, ascending.
    pub fn film_boxes(&self, shown: usize, position: usize) -> Vec<(usize, Rect)> {
        let Some(band) = self.film_band() else {
            return Vec::new();
        };
        let s = self.surface.scale.value();
        let cell = CELL_WIDTH * s;
        let (width, height) = (THUMB_WIDTH * s, THUMB_HEIGHT * s);
        let count = ((band.width as usize).saturating_sub(cell) / (width + cell)).min(shown);
        if count == 0 {
            return Vec::new();
        }
        let first = position.saturating_sub(count / 2).min(shown - count);
        (0..count)
            .map(|offset| {
                (
                    first + offset,
                    Rect {
                        x: band.x + (cell + offset * (width + cell)) as i64,
                        y: band.y + ((CELL_PAD / 2) * s) as i64,
                        width: width as u32,
                        height: height as u32,
                    },
                )
            })
            .collect()
    }

    /// The develop region under its two bands and above the filmstrip:
    /// the name row and the box.
    pub fn develop_view(&self) -> Rect {
        let below = self.below_bands();
        match self.film_band() {
            Some(film) => Rect {
                height: below.height.saturating_sub(film.height),
                ..below
            },
            None => below,
        }
    }

    /// Buttons on `band` from a cell in, each its label's cells and a cell
    /// each side, a cell between, inset as a strip's; `None` from the first
    /// the band cannot hold whole. The x after the last laid, for what
    /// follows them.
    fn lay_buttons<'a>(
        &self,
        band: Rect,
        labels: impl Iterator<Item = &'a str>,
        out: &mut Vec<Option<Button>>,
    ) -> i64 {
        let s = self.surface.scale.value();
        let cell = (CELL_WIDTH * s) as i64;
        let margin = (BUTTON_MARGIN * s) as i64;
        let right = band.x + i64::from(band.width);
        let whole = band.height as usize >= ROW * s;
        let mut x = band.x + cell;
        for label in labels {
            let width = cell * (label.chars().count() as i64 + 2);
            let rect = Rect {
                x,
                y: band.y + margin,
                width: width as u32,
                height: ((ROW - 2 * BUTTON_MARGIN) * s) as u32,
            };
            let fits = whole && x + width <= right;
            out.push(fits.then(|| Button::new(self.surface, rect)).flatten());
            x += width + cell;
        }
        x
    }

    /// The tool band's buttons and, after them to a cell short of the
    /// band's right, the exposure slider.
    pub fn tools(&self) -> Tools {
        let band = self.tool_band();
        let mut laid = Vec::with_capacity(TOOL_BUTTONS.len());
        let x = self.lay_buttons(band, TOOL_BUTTONS.into_iter(), &mut laid);
        let mut buttons = [None; 6];
        for (slot, button) in buttons.iter_mut().zip(laid) {
            *slot = button;
        }
        let s = self.surface.scale.value();
        let cell = (CELL_WIDTH * s) as i64;
        let right = band.x + i64::from(band.width) - cell;
        // Every step needs its own column, or a press on the knob's own
        // centre would read as another step (td-ui's `travel` contract).
        let slider = (buttons.iter().all(Option::is_some) && right > x)
            .then(|| {
                Slider::new(
                    self.surface,
                    Rect {
                        x,
                        y: band.y,
                        width: (right - x) as u32,
                        height: band.height,
                    },
                )
            })
            .flatten()
            .filter(|slider| slider.travel() as usize >= EXPOSURE_STEPS);
        Tools { buttons, slider }
    }

    /// The look band's buttons: `NO_LOOK`, then a button per available
    /// look in `stems` order, as many as the band holds whole.
    pub fn look_buttons(&self, stems: &[String]) -> Vec<Option<Button>> {
        let mut laid = Vec::with_capacity(stems.len() + 1);
        self.lay_buttons(
            self.look_band(),
            std::iter::once(NO_LOOK).chain(stems.iter().map(String::as_str)),
            &mut laid,
        );
        laid
    }

    /// The area right of the history pane: where develop mode shows the
    /// photo's name, its facts and the preview.
    pub fn develop_region(&self) -> Rect {
        let pane = (PANE_W * self.surface.scale.value()) as u32;
        Rect {
            x: self.area.x + i64::from(pane),
            width: self.area.width.saturating_sub(pane),
            ..self.area
        }
    }

    /// The history pane's list: the pane's width at the area's left, above
    /// one band for its buttons; `None` when the area cannot hold a row.
    pub fn history(&self) -> Option<List> {
        let band = (ROW * self.surface.scale.value()) as u32;
        let rect = Rect {
            width: (PANE_W * self.surface.scale.value()) as u32,
            height: self.area.height.saturating_sub(band),
            ..self.area
        };
        List::new(self.surface, rect)
    }

    /// The pane's buttons on the band under its list, `HISTORY_BUTTONS` in
    /// order from a cell in, each its label's cells and a cell each side,
    /// a cell between, inset as a strip's are; `None` where the surface
    /// cannot hold one whole.
    pub fn history_buttons(&self) -> [Option<Button>; 3] {
        let s = self.surface.scale.value();
        let band = (ROW * s) as i64;
        let y = self.area.y + i64::from(self.area.height) - band + (BUTTON_MARGIN * s) as i64;
        let height = (ROW - 2 * BUTTON_MARGIN) * s;
        let cell = (CELL_WIDTH * s) as i64;
        let mut x = self.area.x + cell;
        let mut buttons = [None; 3];
        for (slot, label) in buttons.iter_mut().zip(HISTORY_BUTTONS) {
            let width = cell * (label.len() as i64 + 2);
            let rect = Rect {
                x,
                y,
                width: width as u32,
                height: height as u32,
            };
            *slot = self.history().and_then(|_| Button::new(self.surface, rect));
            x += width + cell;
        }
        buttons
    }

    /// The largest 3:2 box under `rows` text rows in `region`, the same
    /// geometry the scene, the window and `--preview` share, so the
    /// placeholder and the image land in one place.
    fn box_in(&self, region: Rect, rows: i64) -> Option<Rect> {
        let s = self.surface.scale.value();
        let pad = (CELL_PAD * s) as i64;
        let row = (CELL_HEIGHT * s) as i64;
        let text_x = region.x + pad;
        let text_w = (i64::from(region.width) - 2 * pad).max(0);
        // The text rows sit at `pad/2`, then `row` each; the box opens
        // `pad/2` below them and closes `pad` above the region's foot.
        let top = region.y + pad / 2 + rows * row + pad / 2;
        let bottom = region.y + i64::from(region.height) - pad;
        let available_h = (bottom - top).max(0);
        let width = text_w.min(available_h * 3 / 2);
        let height = width * 2 / 3;
        if width <= 0 || height <= 0 {
            return None;
        }
        Some(Rect {
            x: text_x + (text_w - width) / 2,
            y: top,
            width: width as u32,
            height: height as u32,
        })
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
    mode: Mode,
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
    /// The developed image's rectangle within the develop box, as the
    /// adapter last reported it: the crop drag's canvas, since the model is
    /// pixel-blind and cannot know the fitted image's size. `None` before a
    /// develop lands, when the box itself is the fallback canvas.
    preview_fit: Option<Rect>,
    /// The crop drag in progress, in surface pixels, or `None`: set on a
    /// press inside the preview, moved, and taken on release. Held only in
    /// develop mode and dropped when the photo, mode or surface changes.
    drag: Option<Drag>,
    /// The crop-adjust sub-mode of develop: the box shows the uncropped
    /// develop with the crop outlined and its edge, corner and move handles,
    /// so the crop can be grown as well as tightened. Off outside develop and
    /// dropped when the photo or mode changes; the surface keeps it.
    adjusting: bool,
    /// The aspect the crop drag is locked to: a transient crop-tool setting,
    /// not a `state` field, dropped on a photo or mode change but kept across a
    /// surface resize and the crop-adjust toggle, so a ratio picked in one
    /// governs the next drag either way.
    aspect: Aspect,
    /// The available look stems (built-in and user) the adapter last reported
    /// with `set_looks`: what the look palette lists. A fact like the job
    /// count -- sorted and deduped by the adapter -- so it never bumps the
    /// generation and is absent from `state`.
    looks: Vec<String>,
    /// The look-palette sub-mode of develop: the box lists the available looks
    /// with the current one marked, and a press on a name picks it (the `look`
    /// action also sets one). Off outside develop, dropped when the photo or
    /// mode changes, kept across a surface resize, and mutually exclusive with
    /// crop-adjust.
    look_list: bool,
    /// The status row's export note, as the adapter last set it: what the
    /// last export asked for came to (`exporting NAME`, `exported NAME.jpg`,
    /// `export of NAME failed`). A fact the frame witnesses: setting a
    /// different note bumps the generation, since the row repaints; absent
    /// from `state`, and cleared when a roll opens.
    export: Option<String>,
    /// The roll chooser while it is open (`choose`, `o`); see `Chooser`.
    chooser: Option<Chooser>,
    /// The history pane's selected step, an index into the cursor photo's
    /// history, in develop mode with a step to select; the newest when a
    /// photo is developed or a step is added, kept (clamped) as steps go.
    history_step: Option<usize>,
    /// The first history step the pane shows, moved as little as possible
    /// to keep the selection in view.
    history_first: usize,
    /// The exposure slider's drag: the value under the pointer since the
    /// press, painted as the knob, committed on release. Dropped with the
    /// crop drag when the photo, mode or surface changes.
    slider: Option<usize>,
}

/// The roll chooser: the toolkit's finder over the folder the adapter
/// listed last, and that folder's path as the request carried it (what a
/// descent joins a name to, what `Here` opens). Open until a choice, a
/// cancel, a roll opening or a surface the finder cannot fit; while it is
/// open every key, press and wheel is the finder's, the area shows it in
/// place of the grid, and the boxes, the develop preview and the overlays
/// are withheld from the window, so nothing is blitted over it.
#[derive(Debug)]
struct Chooser {
    finder: finder::Controller,
    folder: Vec<u8>,
}

/// A crop drag: the canvas it maps against (the develop preview's fitted
/// rectangle, captured at the press so a fit reported mid-drag cannot re-map
/// the gesture), the press anchor and the pointer's current point (both in
/// surface pixels clamped into the canvas), the aspect it is locked to (also
/// frozen at the press), and what the drag grips. The marquee (a tighten drag)
/// is the bounding box of anchor and current, snapped to the aspect.
struct Drag {
    canvas: Rect,
    anchor: (i64, i64),
    current: (i64, i64),
    aspect: Aspect,
    grip: Grip,
}

/// What a crop drag moves. `Marquee` draws a fresh rectangle that tightens the
/// current crop (5(e)-ii); `Handle` grabs an edge, corner or the interior of
/// the crop rectangle shown over the uncropped image in crop-adjust, resizing
/// or moving `crop`, the cursor photo's crop frozen at the press, in its own
/// ten-thousandths so the edges the drag does not move keep their exact value.
enum Grip {
    Marquee,
    Handle { zone: Zone, crop: Crop },
}

/// The part of the crop rectangle a handle drag grabs: a corner, an edge, or
/// the interior (which moves the whole rectangle).
#[derive(Clone, Copy, Eq, PartialEq)]
enum Zone {
    Move,
    N,
    S,
    E,
    W,
    Ne,
    Nw,
    Se,
    Sw,
}

/// The aspect a crop drag is locked to: `Free` is any shape, the rest a fixed
/// pixel ratio the drag holds. A transient crop-tool setting, not a `state`
/// field and not saved to the sidecar -- the crop's own fractions record the
/// achieved shape; the lock only shapes the next drag.
#[derive(Clone, Copy, Eq, PartialEq)]
enum Aspect {
    Free,
    R3_2,
    R4_3,
    R1_1,
    R16_9,
}

impl Aspect {
    /// The tokens the `aspect` action takes, each with its pixel ratio; `Free`
    /// has none.
    const ALL: [(Aspect, &'static str, u32, u32); 5] = [
        (Aspect::Free, "free", 0, 0),
        (Aspect::R3_2, "3:2", 3, 2),
        (Aspect::R4_3, "4:3", 4, 3),
        (Aspect::R1_1, "1:1", 1, 1),
        (Aspect::R16_9, "16:9", 16, 9),
    ];

    fn parse(token: &str) -> Option<Aspect> {
        Self::ALL
            .into_iter()
            .find(|(_, name, _, _)| *name == token)
            .map(|(aspect, _, _, _)| aspect)
    }

    /// The pixel ratio `(rw, rh)`, or `None` for `Free`.
    fn ratio(self) -> Option<(u32, u32)> {
        Self::ALL
            .into_iter()
            .find(|(aspect, _, _, _)| *aspect == self)
            .filter(|(_, _, rw, rh)| *rw > 0 && *rh > 0)
            .map(|(_, _, rw, rh)| (rw, rh))
    }
}

/// The marquee a drag spans: the bounding box of its anchor and current.
fn marquee_rect(drag: &Drag) -> Rect {
    let (ax, ay) = drag.anchor;
    let (cx, cy) = drag.current;
    let x = ax.min(cx);
    let y = ay.min(cy);
    Rect {
        x,
        y,
        width: (ax.max(cx) - x) as u32,
        height: (ay.max(cy) - y) as u32,
    }
}

/// The marquee a drag paints and commits: the plain bounding box when the drag
/// is `Free`, else snapped to its locked aspect.
fn marquee_now(drag: &Drag) -> Rect {
    match drag.aspect.ratio() {
        None => marquee_rect(drag),
        Some((rw, rh)) => marquee_snapped(drag, rw, rh),
    }
}

/// The marquee snapped to the pixel ratio `rw:rh`: the largest ratio box that
/// fits inside the raw bounding box of anchor and current, anchored at the press
/// corner and extending toward the pointer. Canvas pixels throughout, so the
/// ratio is the displayed image's pixel ratio; a zero raw edge stays zero (a
/// click, which `drag_crop` rejects).
fn marquee_snapped(drag: &Drag, rw: u32, rh: u32) -> Rect {
    let (ax, ay) = drag.anchor;
    let (cx, cy) = drag.current;
    let raw_w = (cx - ax).abs();
    let raw_h = (cy - ay).abs();
    let (rw, rh) = (i64::from(rw), i64::from(rh));
    // Reduce whichever dimension overshoots the ratio.
    let (w, h) = if raw_w * rh > raw_h * rw {
        (raw_h * rw / rh, raw_h)
    } else {
        (raw_w, raw_w * rh / rw)
    };
    Rect {
        x: if cx >= ax { ax } else { ax - w },
        y: if cy >= ay { ay } else { ay - h },
        width: w.max(0) as u32,
        height: h.max(0) as u32,
    }
}

/// A point clamped into `rect`, its far edges inclusive so a drag can end on
/// the far edge though `Rect::contains` excludes it, keeping the marquee within
/// the canvas.
fn clamp_into(rect: Rect, x: i64, y: i64) -> (i64, i64) {
    let far_x = rect.x.saturating_add(i64::from(rect.width));
    let far_y = rect.y.saturating_add(i64::from(rect.height));
    (x.clamp(rect.x, far_x), y.clamp(rect.y, far_y))
}

/// The whole oriented image: the crop a photo with no sidecar crop is under.
const FULL_CROP: Crop = Crop {
    x: 0,
    y: 0,
    width: library::CROP_UNIT,
    height: library::CROP_UNIT,
};

/// A crop mapped from its ten-thousandths of the oriented image onto `canvas`
/// surface pixels: where its rectangle is drawn over the uncropped develop.
fn crop_on_canvas(canvas: Rect, crop: Crop) -> Rect {
    let unit = u64::from(library::CROP_UNIT);
    let (cw, ch) = (u64::from(canvas.width), u64::from(canvas.height));
    let px = |frac: u32, span: u64| (u64::from(frac) * span / unit) as i64;
    Rect {
        x: canvas.x + px(crop.x, cw),
        y: canvas.y + px(crop.y, ch),
        width: px(crop.width, cw) as u32,
        height: px(crop.height, ch) as u32,
    }
}

/// The crop after a handle drag of `(dx, dy)` canvas pixels from `start`, the
/// crop frozen at the press. The delta is mapped into the crop's own
/// ten-thousandths and applied only to the edges the `zone` moves, so the edges
/// it does not move keep their exact value (no pixel round-trip): a corner
/// moves both its edges, an edge one, and `Move` translates the box. Each moved
/// edge is confined to the unit square and kept at least `MIN_CROP_EDGE` from
/// its opposite, so `Crop::new` (the safety net) accepts a valid `start`'s
/// result; a full-frame result is the whole image (`FULL_CROP`).
fn commit_crop(
    canvas: Rect,
    start: Crop,
    zone: Zone,
    dx: i64,
    dy: i64,
    aspect: Aspect,
) -> Option<Crop> {
    let unit = i64::from(library::CROP_UNIT);
    let min = i64::from(library::MIN_CROP_EDGE);
    // A canvas-pixel delta mapped to ten-thousandths, rounded to the nearest
    // and sign-aware, so a drag left or up maps to a negative fraction.
    let frac = |px: i64, span: u32| -> i64 {
        let span = i64::from(span);
        if span == 0 {
            return 0;
        }
        let scaled = px * unit;
        if scaled >= 0 {
            (scaled + span / 2) / span
        } else {
            -((-scaled + span / 2) / span)
        }
    };
    let fdx = frac(dx, canvas.width);
    let fdy = frac(dy, canvas.height);
    // A locked aspect reshapes an edge or corner drag to hold the ratio, but
    // only once the drag actually moves: a zero-delta grab keeps the free path
    // so it paints the crop already shown rather than reshaping on the press
    // (which a stationary release would then discard). `Move` is a translation,
    // ratio-independent, and a degenerate canvas is guarded against division by
    // zero -- both take the free path too.
    if !matches!(zone, Zone::Move) && (fdx != 0 || fdy != 0) {
        if let Some((rw, rh)) = aspect.ratio() {
            if canvas.width > 0 && canvas.height > 0 {
                return commit_locked(canvas, start, zone, fdx, fdy, rw, rh);
            }
        }
    }
    let mut x0 = i64::from(start.x);
    let mut y0 = i64::from(start.y);
    let mut x1 = i64::from(start.x) + i64::from(start.width);
    let mut y1 = i64::from(start.y) + i64::from(start.height);
    if let Zone::Move = zone {
        let tx = confine(fdx, -x0, unit - x1);
        let ty = confine(fdy, -y0, unit - y1);
        x0 += tx;
        x1 += tx;
        y0 += ty;
        y1 += ty;
    } else {
        if matches!(zone, Zone::W | Zone::Nw | Zone::Sw) {
            x0 = confine(x0 + fdx, 0, x1 - min);
        }
        if matches!(zone, Zone::E | Zone::Ne | Zone::Se) {
            x1 = confine(x1 + fdx, x0 + min, unit);
        }
        if matches!(zone, Zone::N | Zone::Nw | Zone::Ne) {
            y0 = confine(y0 + fdy, 0, y1 - min);
        }
        if matches!(zone, Zone::S | Zone::Sw | Zone::Se) {
            y1 = confine(y1 + fdy, y0 + min, unit);
        }
    }
    Crop::new(x0 as u32, y0 as u32, (x1 - x0) as u32, (y1 - y0) as u32).ok()
}

/// `a / b` rounded to the nearest, for `b > 0` and any sign of `a`.
fn round_div(a: i128, b: i128) -> i128 {
    if a >= 0 {
        (a + b / 2) / b
    } else {
        -((-a + b / 2) / b)
    }
}

/// The crop after a locked-aspect edge or corner drag: like `commit_crop`'s
/// free path but holding the pixel ratio `rw:rh`. Since the ratio is in canvas
/// pixels, in the crop's ten-thousandths it is `width : height =
/// rw*canvas.height : rh*canvas.width`; `h_of_w`/`w_of_h` map one edge to the
/// other. A corner drives the larger ratio box that reaches the pointer, capped
/// to the space from its fixed opposite corner; an edge sets its own axis and
/// centres the derived dimension on the crop's midline. The box is floored to a
/// ratio-preserving `MIN_CROP_EDGE`, and is `None` when even that will not fit;
/// `Crop::new` is the final safety net. `Move` never reaches here.
fn commit_locked(
    canvas: Rect,
    start: Crop,
    zone: Zone,
    fdx: i64,
    fdy: i64,
    rw: u32,
    rh: u32,
) -> Option<Crop> {
    let unit = i128::from(library::CROP_UNIT);
    let min = i128::from(library::MIN_CROP_EDGE);
    let cw = i128::from(canvas.width);
    let ch = i128::from(canvas.height);
    let rw = i128::from(rw);
    let rh = i128::from(rh);
    let h_of_w = |w: i128| round_div(w * cw * rh, ch * rw);
    let w_of_h = |h: i128| round_div(h * ch * rw, cw * rh);
    let (fdx, fdy) = (i128::from(fdx), i128::from(fdy));
    let x0 = i128::from(start.x);
    let y0 = i128::from(start.y);
    let x1 = x0 + i128::from(start.width);
    let y1 = y0 + i128::from(start.height);

    // Reconcile a raw corner box to the ratio (the larger box that reaches the
    // pointer), cap it to the space from the fixed opposite corner, then floor
    // to a ratio-preserving minimum; `None` when even the minimum will not fit.
    let corner = |raw_w: i128, raw_h: i128, aw: i128, ah: i128| -> Option<(i128, i128)> {
        let (raw_w, raw_h) = (raw_w.max(0), raw_h.max(0));
        let (mut w, mut h) = if raw_w >= w_of_h(raw_h) {
            (raw_w, h_of_w(raw_w))
        } else {
            (w_of_h(raw_h), raw_h)
        };
        if w > aw {
            w = aw;
            h = h_of_w(aw);
        }
        if h > ah {
            h = ah;
            w = w_of_h(ah);
        }
        if w < min {
            w = min;
            h = h_of_w(min);
        }
        if h < min {
            h = min;
            w = w_of_h(min);
        }
        (w >= min && h >= min && w <= aw && h <= ah).then_some((w, h))
    };

    match zone {
        Zone::Se => {
            let (w, h) = corner(x1 + fdx - x0, y1 + fdy - y0, unit - x0, unit - y0)?;
            Crop::new(x0 as u32, y0 as u32, w as u32, h as u32).ok()
        }
        Zone::Sw => {
            let (w, h) = corner(x1 - (x0 + fdx), y1 + fdy - y0, x1, unit - y0)?;
            Crop::new((x1 - w) as u32, y0 as u32, w as u32, h as u32).ok()
        }
        Zone::Ne => {
            let (w, h) = corner(x1 + fdx - x0, y1 - (y0 + fdy), unit - x0, y1)?;
            Crop::new(x0 as u32, (y1 - h) as u32, w as u32, h as u32).ok()
        }
        Zone::Nw => {
            let (w, h) = corner(x1 - (x0 + fdx), y1 - (y0 + fdy), x1, y1)?;
            Crop::new((x1 - w) as u32, (y1 - h) as u32, w as u32, h as u32).ok()
        }
        Zone::E | Zone::W => {
            // The dragged edge sets the width; the height is derived and centred
            // on the crop's horizontal midline (kept doubled to avoid a rounding
            // bias). The far vertical edges cap the centred height.
            let aw = if matches!(zone, Zone::E) {
                unit - x0
            } else {
                x1
            };
            let raw_w = if matches!(zone, Zone::E) {
                x1 + fdx - x0
            } else {
                x1 - (x0 + fdx)
            };
            let mid2 = y0 + y1;
            let ah = mid2.min(2 * unit - mid2);
            let mut w = raw_w.max(0).min(aw);
            let mut h = h_of_w(w);
            if h > ah {
                h = ah;
                w = w_of_h(ah);
            }
            if w < min {
                w = min;
                h = h_of_w(min);
            }
            if h < min {
                h = min;
                w = w_of_h(min);
            }
            if w < min || h < min || w > aw || h > ah {
                return None;
            }
            let top = (mid2 - h) / 2;
            let x = if matches!(zone, Zone::E) { x0 } else { x1 - w };
            Crop::new(x as u32, top as u32, w as u32, h as u32).ok()
        }
        Zone::N | Zone::S => {
            // Symmetric: the dragged edge sets the height, the width derived and
            // centred on the crop's vertical midline.
            let ah = if matches!(zone, Zone::S) {
                unit - y0
            } else {
                y1
            };
            let raw_h = if matches!(zone, Zone::S) {
                y1 + fdy - y0
            } else {
                y1 - (y0 + fdy)
            };
            let mid2 = x0 + x1;
            let aw = mid2.min(2 * unit - mid2);
            let mut h = raw_h.max(0).min(ah);
            let mut w = w_of_h(h);
            if w > aw {
                w = aw;
                h = h_of_w(aw);
            }
            if h < min {
                h = min;
                w = w_of_h(min);
            }
            if w < min {
                w = min;
                h = h_of_w(min);
            }
            if w < min || h < min || w > aw || h > ah {
                return None;
            }
            let left = (mid2 - w) / 2;
            let y = if matches!(zone, Zone::S) { y0 } else { y1 - h };
            Crop::new(left as u32, y as u32, w as u32, h as u32).ok()
        }
        Zone::Move => None,
    }
}

/// A value confined to `[lo, hi]` without the `clamp` panic when `hi < lo`
/// (a rectangle already at the minimum edge): the range collapses to `lo`.
fn confine(value: i64, lo: i64, hi: i64) -> i64 {
    value.max(lo).min(hi.max(lo))
}

/// The zone of the crop rectangle a press grabs, or `None` when the press is
/// off the canvas or in no zone: a corner within `grip` of two adjacent edges,
/// then a single edge along its span, then the strict interior (which moves
/// the whole rectangle). `grip` is the handle reach in surface pixels.
fn classify(rect: Rect, canvas: Rect, x: i64, y: i64, grip: i64) -> Option<Zone> {
    if !canvas.contains(x, y) {
        return None;
    }
    let left = rect.x;
    let right = rect.x + i64::from(rect.width);
    let top = rect.y;
    let bottom = rect.y + i64::from(rect.height);
    if x < left - grip || x > right + grip || y < top - grip || y > bottom + grip {
        return None;
    }
    let near_l = (x - left).abs() <= grip;
    let near_r = (x - right).abs() <= grip;
    let near_t = (y - top).abs() <= grip;
    let near_b = (y - bottom).abs() <= grip;
    let zone = match (near_l, near_r, near_t, near_b) {
        (true, _, true, _) => Zone::Nw,
        (_, true, true, _) => Zone::Ne,
        (true, _, _, true) => Zone::Sw,
        (_, true, _, true) => Zone::Se,
        (true, _, _, _) => Zone::W,
        (_, true, _, _) => Zone::E,
        (_, _, true, _) => Zone::N,
        (_, _, _, true) => Zone::S,
        _ if x > left && x < right && y > top && y < bottom => Zone::Move,
        _ => return None,
    };
    Some(zone)
}

/// The overlay the scene paints over the develop box, as its identity for the
/// generation: the crop-adjust rectangle or tighten marquee (the kind matters,
/// so a marquee and a crop-adjust rectangle of the same shape are different
/// frames), or the look palette. Mutually exclusive; `None` when nothing is
/// painted. The generation follows this, so it moves exactly when the overlay
/// does.
#[derive(Clone, Copy, Eq, PartialEq)]
enum Painted {
    /// `true` the crop-adjust rectangle (drawn with handle marks), `false` a
    /// plain tighten marquee outline; a zero-edge rectangle is never this.
    Crop(bool, Rect),
    /// The look palette (a non-empty list of look names); its rows and the
    /// current-look mark are derived, so its identity is just that it shows.
    Looks,
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
            mode: Mode::Cull,
            first_row: 0,
            generation: 0,
            shown: Vec::new(),
            bytes: 0,
            jobs: 0,
            preview_fit: None,
            drag: None,
            adjusting: false,
            aspect: Aspect::Free,
            looks: Vec::new(),
            look_list: false,
            export: None,
            chooser: None,
            history_step: None,
            history_first: 0,
            slider: None,
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
        self.mode = Mode::Cull;
        self.first_row = 0;
        self.drag = None;
        self.slider = None;
        self.adjusting = false;
        self.aspect = Aspect::Free;
        self.look_list = false;
        self.preview_fit = None;
        self.export = None;
        self.chooser = None;
        self.refresh_shown();
        self.cursor = self.shown.first().copied();
        self.sync_step(0);
        self.bump();
        Ok(())
    }

    /// The folder the adapter listed for the chooser, as its path bytes
    /// and the finder's listing: installed in the open chooser, or opening
    /// one over the area (refused when the area cannot hold a finder);
    /// `select` names the entry to land on. A change either way.
    pub fn set_listing(
        &mut self,
        folder: Vec<u8>,
        listing: finder::Listing,
        select: Option<&str>,
    ) -> Result<(), Error> {
        match self.chooser.as_mut() {
            Some(chooser) => {
                chooser
                    .finder
                    .set_listing(listing, select)
                    .map_err(|_| Error::Refused)?;
                chooser.folder = folder;
            }
            None => {
                let finder = finder::Controller::new(
                    listing,
                    finder::Choose::Folder,
                    self.surface,
                    self.layout().area,
                    select,
                )
                .map_err(|_| Error::Refused)?;
                // The pointer is the finder's now; a drag cannot go on.
                self.drag = None;
                self.slider = None;
                self.chooser = Some(Chooser { finder, folder });
            }
        }
        self.bump();
        Ok(())
    }

    /// Why the adapter could not list a folder, shown in the open
    /// chooser's status row until the next listing; nothing without one.
    /// The note is fitted to the finder's bound, its tail kept since the
    /// reason follows the path, and its control characters blanked; the
    /// same note again is no change.
    pub fn note_listing(&mut self, note: &str) {
        let Some(chooser) = self.chooser.as_mut() else {
            return;
        };
        let mut fitted: String = note
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect();
        if fitted.len() > finder::NOTE_BYTES {
            let keep = finder::NOTE_BYTES - '\u{2026}'.len_utf8();
            let mut start = fitted.len() - keep;
            while !fitted.is_char_boundary(start) {
                start += 1;
            }
            fitted = format!("\u{2026}{}", fitted.get(start..).unwrap_or(""));
        }
        if chooser.finder.note() == fitted {
            return;
        }
        if chooser.finder.set_note(&fitted).is_ok() {
            self.bump();
        }
    }

    /// Whether holding `chord` repeats while the chooser is open: the moves,
    /// a typed character but the caret, and Backspace while there is a
    /// filter to edit (a held one on an empty filter would run up the
    /// tree, as a held M-Up or ^ would); Return, C-Return and Escape fire
    /// once.
    pub fn chooser_repeats(&self, chord: &str) -> bool {
        let Some(chooser) = self.chooser.as_ref() else {
            return false;
        };
        match chord {
            "Up" | "Down" | "PageUp" | "PageDown" => true,
            "Backspace" => !chooser.finder.query().is_empty(),
            // An ascent held would run up the tree.
            "M-Up" | "^" => false,
            _ => {
                let mut chars = chord.chars();
                matches!((chars.next(), chars.next()), (Some(c), None) if !c.is_control())
            }
        }
    }

    /// The open chooser: the listed folder's path and the finder.
    pub fn chooser(&self) -> Option<(&[u8], &finder::Controller)> {
        self.chooser
            .as_ref()
            .map(|chooser| (chooser.folder.as_slice(), &chooser.finder))
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
        let before = self.steps().len();
        let cursor = self.cursor;
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
        // A settle that moved the cursor (a flag that hid its photo) shows
        // another photo's history, its newest step selected.
        self.sync_step(if self.cursor == cursor { before } else { 0 });
        self.bump();
        true
    }

    /// Takes the photos named out of the model, as the adapter moved them
    /// out of the roll: the shown list is recomputed, the cursor keeps its
    /// photo when that stays and otherwise its position among the shown,
    /// clamped to the end, or leaves when none is shown, ending the drag
    /// (a crop's or the slider's), crop-adjust, aspect lock and look
    /// palette that were the moved photo's, as a filter that hides it
    /// does. Whether the model changed:
    /// a name it does not hold changes nothing.
    pub fn remove(&mut self, names: &[String]) -> bool {
        if names.is_empty() || !self.photos.iter().any(|photo| names.contains(&photo.name)) {
            return false;
        }
        let before = self.steps().len();
        let position = self.position();
        let kept = self
            .cursor
            .and_then(|index| self.photos.get(index))
            .filter(|photo| !names.contains(&photo.name))
            .map(|photo| photo.name.clone());
        let same = kept.is_some();
        self.photos.retain(|photo| !names.contains(&photo.name));
        self.bytes = self
            .photos
            .iter()
            .fold(0usize, |total, photo| total.saturating_add(photo.bytes()));
        self.refresh_shown();
        self.cursor = match kept {
            Some(name) => self.photos.iter().position(|photo| photo.name == name),
            None => {
                self.drag = None;
                self.slider = None;
                self.adjusting = false;
                self.aspect = Aspect::Free;
                self.look_list = false;
                let last = self.shown.len().checked_sub(1);
                position
                    .zip(last)
                    .and_then(|(position, last)| self.shown.get(position.min(last)).copied())
            }
        };
        self.keep_cursor_shown();
        // The cursor's photo kept its history and its selection; another
        // photo's shows with its newest step selected.
        self.sync_step(if same { before } else { 0 });
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

    pub fn mode(&self) -> Mode {
        self.mode
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

    /// The developed image's fitted rectangle within the develop box, as the
    /// adapter computed it (the box centring `blit` uses): the crop drag's
    /// canvas. A fact like the job count, so it never bumps the generation;
    /// the drag rectangle the model derives from it does.
    pub fn set_preview_fit(&mut self, fit: Option<Rect>) {
        self.preview_fit = fit;
    }

    /// The look stems the palette and the look band list (built-in and
    /// user), as the adapter enumerated them, sorted and deduped by it: a
    /// fact absent from `state`, but one the band paints, so a list that
    /// differs from the one held bumps the generation in develop, as an
    /// export note does. Session-static in practice, reported once when
    /// the roll opens.
    pub fn set_looks(&mut self, looks: Vec<String>) {
        if self.looks != looks {
            self.looks = looks;
            // The look band lists them, so a list that differs repaints
            // where the band is: develop's, and entering develop bumps.
            if self.mode == Mode::Develop {
                self.bump();
            }
        }
    }

    /// The status row's export note, as last set.
    pub fn export_note(&self) -> Option<&str> {
        self.export.as_deref()
    }

    /// What the last export asked for came to, for the status row: the
    /// adapter's report, a fact, but one the row shows, so a note that
    /// differs from the one held is a new generation; the same note again
    /// is not.
    pub fn set_export(&mut self, note: Option<String>) {
        if self.export != note {
            self.export = note;
            self.bump();
        }
    }

    /// The develop history the pane shows: the cursor photo's steps in
    /// develop mode, oldest first; empty in cull or before a photo.
    pub fn steps(&self) -> &[Step] {
        if self.mode != Mode::Develop {
            return &[];
        }
        self.cursor
            .and_then(|index| self.photos.get(index))
            .and_then(|photo| photo.sidecar.as_ref())
            .map_or(&[], |sidecar| sidecar.steps())
    }

    /// The first step the pane shows, with the selection in view.
    pub fn step_first(&self) -> usize {
        let total = self.steps().len();
        match (self.layout().history(), self.history_step) {
            (Some(list), Some(step)) => list.reveal(total, step, self.history_first),
            _ => 0,
        }
    }

    /// Keeps the selection on the history as it stands: none without a
    /// step or outside develop, the newest when the history grew past
    /// `before` (a step just added, or a photo just developed), else the
    /// one selected, clamped to the steps that remain.
    fn sync_step(&mut self, before: usize) {
        let total = self.steps().len();
        self.history_step = if total == 0 {
            None
        } else if total > before {
            Some(total - 1)
        } else {
            Some(
                self.history_step
                    .map_or(total - 1, |step| step.min(total - 1)),
            )
        };
        self.history_first = self.step_first();
    }

    /// Moves the history selection by `delta` steps, clamped to the ends;
    /// `Ignored` at an end it is already at, or with no step.
    fn move_step(&mut self, delta: i64) -> Outcome {
        let total = self.steps().len();
        let Some(step) = self.history_step.filter(|_| total > 0) else {
            return Outcome::Ignored;
        };
        let last = (total - 1) as i64;
        let target = (step as i64).saturating_add(delta).clamp(0, last) as usize;
        self.select_step(target)
    }

    /// Selects history step `target`, a change when it was not selected.
    fn select_step(&mut self, target: usize) -> Outcome {
        if target >= self.steps().len() || self.history_step == Some(target) {
            return Outcome::Ignored;
        }
        self.history_step = Some(target);
        self.history_first = self.step_first();
        Outcome::Changed
    }

    /// The look palette for the scene, when it is open over a non-empty list:
    /// the available look stems and the index of the cursor photo's current
    /// look among them (the row to mark), or `None`. Read by the scene; a fact
    /// and a frame-witnessed sub-mode, not a `state` field.
    pub fn look_palette(&self) -> Option<(&[String], Option<usize>)> {
        if !self.look_list || self.looks.is_empty() || self.chooser.is_some() {
            return None;
        }
        let active = self.cursor.and_then(|index| {
            let stem = self.current_look(index)?;
            self.looks.iter().position(|look| look == stem)
        });
        Some((&self.looks, active))
    }

    /// The crop drag's canvas: the developed image's fitted rectangle when
    /// the adapter has reported one, else the develop box itself (exact when
    /// the developed image fills the box, as an uncropped 3:2 frame does).
    fn canvas(&self) -> Option<Rect> {
        self.preview_fit.or_else(|| self.develop_box())
    }

    /// Whether the crop-adjust sub-mode is on: the box shows the uncropped
    /// develop with the crop's handles. A fact for the window (which then
    /// develops the uncropped frame) and the scene; not a `state` field.
    pub fn adjusting(&self) -> bool {
        self.adjusting
    }

    /// The crop drag's marquee, in surface pixels, or `None` when no tighten
    /// drag is in progress: what the scene outlines over the cropped preview.
    pub fn crop_drag(&self) -> Option<Rect> {
        match self.drag.as_ref() {
            Some(drag) if matches!(drag.grip, Grip::Marquee) => Some(marquee_now(drag)),
            _ => None,
        }
    }

    /// The crop rectangle drawn over the uncropped develop in crop-adjust, in
    /// surface pixels, or `None` when not adjusting: the live handle rectangle
    /// while a handle is dragged, else the current crop mapped onto the canvas.
    /// What the scene outlines and marks with handles.
    pub fn crop_adjust_rect(&self) -> Option<Rect> {
        if !self.adjusting {
            return None;
        }
        if let Some(drag) = self.drag.as_ref() {
            if let Grip::Handle { zone, crop } = drag.grip {
                let dx = drag.current.0 - drag.anchor.0;
                let dy = drag.current.1 - drag.anchor.1;
                // Against the canvas frozen at the press, and via the same
                // `commit_crop` the release uses, so the overlay is exactly the
                // crop that will commit -- no snap and no mid-drag re-mapping if
                // a fresh fit arrives.
                let crop = commit_crop(drag.canvas, crop, zone, dx, dy, drag.aspect)?;
                return Some(crop_on_canvas(drag.canvas, crop));
            }
        }
        let canvas = self.canvas()?;
        Some(crop_on_canvas(canvas, self.current_crop(self.cursor?)))
    }

    /// The cursor photo's crop, or the full unit square when it has none.
    fn current_crop(&self, index: usize) -> Crop {
        self.photos
            .get(index)
            .and_then(|photo| photo.sidecar.as_ref())
            .and_then(Sidecar::crop)
            .unwrap_or(FULL_CROP)
    }

    /// The cursor photo's look stem, or `None` when it has none: the row the
    /// palette marks as current.
    fn current_look(&self, index: usize) -> Option<&str> {
        self.photos
            .get(index)
            .and_then(|photo| photo.sidecar.as_ref())
            .and_then(Sidecar::look)
    }

    /// The look row a pointer at `(x, y)` in surface pixels falls on, when the
    /// palette is open over a non-empty list with a develop box to paint into:
    /// the same rows `paint_looks` draws, so a press picks exactly the name
    /// under it. The top and side padding is not a row, and a row past the box
    /// bottom is not shown and so not hittable.
    fn look_row_at(&self, x: i64, y: i64) -> Option<usize> {
        let (looks, _) = self.look_palette()?;
        let r#box = self.develop_box()?;
        let s = self.surface.scale.value();
        let pad = (CELL_PAD * s) as i64;
        let row = (CELL_HEIGHT * s) as i64;
        if row <= 0 {
            return None;
        }
        let left = r#box.x + pad;
        let width = (i64::from(r#box.width) - 2 * pad).max(0);
        if x < left || x >= left + width {
            return None;
        }
        let top = r#box.y + pad;
        if y < top {
            return None;
        }
        let index = ((y - top) / row) as usize;
        if index >= looks.len() {
            return None;
        }
        let bottom = r#box.y + i64::from(r#box.height);
        if top + row * (index as i64 + 1) > bottom {
            return None;
        }
        Some(index)
    }

    /// The overlay the scene actually paints over the develop box, or `None`
    /// when nothing is: the look palette when it is open over a non-empty list
    /// with a box to paint into, else the crop-adjust rectangle while adjusting
    /// or the tighten marquee. The kind is part of the identity, so a toggle
    /// between two overlays of the same rectangle is still a change. A zero-edge
    /// crop rectangle, an empty look list, and a surface too small for a develop
    /// box are invisible, so their change is not a frame change. The generation
    /// follows this, so it moves exactly when the overlay does.
    fn painted(&self) -> Option<Painted> {
        if self.chooser.is_some() {
            return None;
        }
        if self.look_list && !self.looks.is_empty() && self.develop_box().is_some() {
            return Some(Painted::Looks);
        }
        let rect = if self.adjusting {
            self.crop_adjust_rect()
        } else {
            self.drag.as_ref().map(marquee_now)
        };
        rect.filter(|rect| rect.width > 0 && rect.height > 0)
            .map(|rect| Painted::Crop(self.adjusting, rect))
    }

    /// The outcome of a pointer step or a mode toggle that may have changed the
    /// painted overlay: `Changed` with one generation bump when it differs from
    /// `before`, else `Ignored` with no bump.
    fn outline_changed(&mut self, before: Option<Painted>) -> (Outcome, Vec<Effect>) {
        if self.painted() == before {
            (Outcome::Ignored, Vec::new())
        } else {
            self.bump();
            (Outcome::Changed, Vec::new())
        }
    }

    /// Something the adapter paints into the frame changed (a thumbnail arrived
    /// for a photo on screen, or the develop image landed): a new generation --
    /// unless the look palette is painted over the develop box, an opaque panel
    /// that hides whatever landed behind it. Closing the palette bumps the
    /// generation, so the image the adapter held meanwhile is shown then.
    pub fn touch(&mut self) {
        if self.painted() == Some(Painted::Looks) {
            return;
        }
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

    /// The mode strip, the first band.
    fn mode_strip(&self) -> Buttons<'static> {
        Buttons::new(self.surface, 0, &MODES)
    }

    /// The filter strip, the band under the mode strip.
    fn filter_strip(&self) -> Buttons<'static> {
        Buttons::new(
            self.surface,
            (ROW * self.surface.scale.value()) as i64,
            &FILTER_NAMES,
        )
    }

    /// The mode strip's buttons as `(selected, enabled)`, in `MODES`
    /// order: the one in view is selected (the chooser while it is open,
    /// else the mode; nothing before a roll, when no mode is in view),
    /// Culling can be pressed once a roll is open and Develop once there
    /// is a photo under the cursor.
    pub fn mode_states(&self) -> [(bool, bool); 3] {
        let roll = self.roll.is_some();
        let in_view = if self.chooser.is_some() {
            0
        } else if !roll {
            3
        } else if self.mode == Mode::Develop {
            2
        } else {
            1
        };
        [
            (in_view == 0, true),
            (in_view == 1, roll),
            (in_view == 2, roll && self.cursor.is_some()),
        ]
    }

    /// The filter strip's buttons as `(selected, enabled)`, in `FILTERS`
    /// order: the active filter selected, all of them disabled while the
    /// filters are not the mode's (develop, or the chooser open).
    pub fn filter_states(&self) -> [(bool, bool); 4] {
        let enabled = self.mode == Mode::Cull && self.chooser.is_none();
        let state = |(filter, _): &(Filter, &str)| (*filter == self.filter, enabled);
        [
            state(&FILTERS[0]),
            state(&FILTERS[1]),
            state(&FILTERS[2]),
            state(&FILTERS[3]),
        ]
    }

    /// A press on the mode strip: the roll chooser, the cull grid or
    /// develop, from whatever is in view. The mode in view is `ignored`,
    /// as is one whose button is disabled: culling before a roll, develop
    /// before a photo. Culling leaves develop whole (its palette, its
    /// crop-adjust and any drag with it) and closes the chooser; develop
    /// closes the chooser too.
    fn press_mode(&mut self, index: usize) -> Result<(Outcome, Vec<Effect>), Error> {
        let roll = self.roll.is_some();
        let photo = roll && self.cursor.is_some();
        let chooser = self.chooser.is_some();
        match index {
            0 if !chooser => self.choose(Vec::new()),
            1 if roll && (chooser || self.mode == Mode::Develop) => {
                self.chooser = None;
                if self.mode == Mode::Develop {
                    self.leave_develop();
                }
                Ok((self.finish(Outcome::Changed), Vec::new()))
            }
            2 if photo && (chooser || self.mode == Mode::Cull) => {
                self.chooser = None;
                self.enter_develop()?;
                Ok((self.finish(Outcome::Changed), Vec::new()))
            }
            _ => Ok((Outcome::Ignored, Vec::new())),
        }
    }

    /// Develop to the cull grid, its sub-modes and drag dropped: what
    /// `grid` does once the palette and crop-adjust are down.
    fn leave_develop(&mut self) {
        self.mode = Mode::Cull;
        self.view = View::Grid;
        self.drag = None;
        self.slider = None;
        self.adjusting = false;
        self.aspect = Aspect::Free;
        self.look_list = false;
        self.history_step = None;
    }

    /// The develop preview's box: the layout's preview box when developing a
    /// photo under the cursor, else `None`. The window and `--preview` blit
    /// the developed image here; the cull single view keeps the placeholder,
    /// so this is `Some` only in develop mode.
    pub fn develop_box(&self) -> Option<Rect> {
        if self.mode != Mode::Develop || self.cursor.is_none() || self.chooser.is_some() {
            return None;
        }
        self.layout().develop_box()
    }

    /// The filmstrip's boxes and the photos in them: the shown photos
    /// around the cursor in develop, `Layout::film_boxes`; nothing in cull,
    /// under the chooser or without a cursor.
    pub fn film(&self) -> Vec<(usize, Rect)> {
        if self.mode != Mode::Develop || self.chooser.is_some() {
            return Vec::new();
        }
        let Some(position) = self.position() else {
            return Vec::new();
        };
        self.layout()
            .film_boxes(self.shown.len(), position)
            .into_iter()
            .filter_map(|(position, rect)| Some((*self.shown.get(position)?, rect)))
            .collect()
    }

    /// The shown photos on screen and the boxes their thumbnails go in: the
    /// cull grid's cells, or develop's filmstrip; nothing in the single
    /// view, under the chooser, or before a roll.
    pub fn visible(&self) -> Vec<(usize, Rect)> {
        if self.roll.is_none() || self.chooser.is_some() {
            return Vec::new();
        }
        if self.mode == Mode::Develop {
            return self.film();
        }
        if self.view != View::Grid {
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
    /// them: the cull grid's screen, then the screen below it, then the one
    /// above, whichever cull view is showing, so the grid is ready when the
    /// single view returns to it; in develop the filmstrip's boxes, then
    /// as many shown after them, then before, so a cursor move finds its
    /// neighbours made. Develop without a strip wants none.
    pub fn wanted(&self) -> Vec<usize> {
        if self.roll.is_none() {
            return Vec::new();
        }
        let layout = self.layout();
        let (screen, first) = if self.mode == Mode::Develop {
            let Some(position) = self.position() else {
                return Vec::new();
            };
            let boxes = layout.film_boxes(self.shown.len(), position);
            let Some((first, _)) = boxes.first() else {
                return Vec::new();
            };
            (boxes.len(), *first)
        } else {
            (
                layout.rows * layout.columns,
                self.first_row * layout.columns,
            )
        };
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
            // The chooser owns the keyboard while it is open: a chord is the
            // finder's key or a typed filter character, and no binding is
            // looked up under it.
            Input::Key { chord } if self.chooser.is_some() => self.chooser_key(chord),
            Input::Key { chord } => match driven::bound(&BINDINGS, chord) {
                Some(binding) => {
                    let action = Action::parse(binding.name).ok_or(Error::BadArgument)?;
                    self.dispatch(action, &[])
                }
                None => Ok((Outcome::Ignored, Vec::new())),
            },
            Input::Pointer { phase, x, y } => self.pointer(phase, i64::from(x), i64::from(y)),
            Input::Wheel { rows, .. } if self.chooser.is_some() => {
                self.chooser_wheel(i64::from(rows))
            }
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
                // The canvas moved under any drag; its pixels are stale.
                self.drag = None;
                self.slider = None;
                self.reveal();
                // The chooser is laid out again over the new area; one it
                // cannot fit closes it.
                let area = self.layout().area;
                if let Some(chooser) = self.chooser.as_mut() {
                    if let finder::Outcome::Closed(_) =
                        chooser.finder.event(finder::Event::Resize {
                            surface,
                            rect: area,
                        })
                    {
                        self.chooser = None;
                    }
                }
                self.bump();
                Ok((Outcome::Changed, Vec::new()))
            }
            Input::Focus(_) | Input::Tick(_) => Ok((Outcome::Ignored, Vec::new())),
        }
    }

    /// The tab-separated facts: the mode (`cull` or `develop`), the roll's
    /// path in hex, how many photos and how many shown, the cursor's
    /// position among the shown, the filter, the cull view (`grid` or
    /// `single`, where develop mode returns), then the photo under the
    /// cursor (its name in hex, flag, exposure, crop, look, sidecar state),
    /// the outstanding job count, the generation and the chooser's listed
    /// folder in hex. Absent values are `-`.
    pub fn state(&self) -> String {
        let position = self.position();
        let photo = self.cursor.and_then(|index| self.photos.get(index));
        let dash = || "-".to_string();
        [
            self.mode.word().to_string(),
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
            self.chooser
                .as_ref()
                .map_or_else(dash, |chooser| hex(&chooser.folder)),
            self.steps().len().to_string(),
            self.history_step.map_or_else(dash, |step| step.to_string()),
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

    /// The crop marquee alone: what the window paints again over the develop
    /// image it blitted, as it repaints the badges over the thumbnails.
    pub fn marquee(&self) -> Marquee<'_> {
        Marquee { model: self }
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
        // While the chooser is open the window's actions are behind it:
        // only `choose` (closing it), `open`, `quit` and `scroll` (the
        // finder's wheel) reach through, the rest is `ignored`, as the
        // keyboard cannot reach them either; the mode strip's Culling and
        // Develop buttons reach through by the pointer (`press_mode`).
        if self.chooser.is_some()
            && !matches!(
                action,
                Action::Choose | Action::Open | Action::Quit | Action::Scroll
            )
        {
            return Ok((Outcome::Ignored, effects));
        }
        let outcome = match (action, arguments) {
            (Action::Choose, []) => return self.choose(effects),
            (Action::Scroll, [rows]) if self.chooser.is_some() => {
                return self.chooser_wheel(signed(rows)?)
            }
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
            (Action::Grid, []) => self.back_to_grid(),
            (Action::Next, []) => self.step(1)?,
            (Action::Previous, []) => self.step(-1)?,
            // In develop the rows are the history's: Down and Up move its
            // selection, Left and Right still the cursor.
            (Action::Down, []) if self.mode == Mode::Develop => self.move_step(1),
            (Action::Up, []) if self.mode == Mode::Develop => self.move_step(-1),
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
            (Action::View, []) if self.mode == Mode::Cull => {
                self.need_photo()?;
                let view = match self.view {
                    View::Grid => View::Single,
                    View::Single => View::Grid,
                };
                self.set_view(view)
            }
            // Return is the cull grid/single toggle; in develop mode Escape
            // is the way out.
            (Action::View, []) => Outcome::Ignored,
            (Action::Pick, []) => return self.flag(Some(Flag::Pick), effects),
            (Action::Reject, []) => return self.flag(Some(Flag::Reject), effects),
            (Action::Unflag, []) => return self.flag(None, effects),
            (Action::Develop, []) => self.enter_develop()?,
            (Action::ExposeIn, []) => return self.expose(EXPOSURE_STEP, effects),
            (Action::ExposeOut, []) => return self.expose(-EXPOSURE_STEP, effects),
            (Action::ExposeInFine, []) => return self.expose(EXPOSURE_FINE, effects),
            (Action::ExposeOutFine, []) => return self.expose(-EXPOSURE_FINE, effects),
            (Action::Look, [stem]) => return self.set_look(stem, effects),
            (Action::Crop, [x, y, w, h]) => return self.set_crop(x, y, w, h, effects),
            (Action::AdjustCrop, []) => self.toggle_adjust()?,
            (Action::Aspect, [ratio]) => return self.set_aspect(ratio, effects),
            (Action::Looks, []) => self.toggle_looks()?,
            (Action::Reset, []) => return self.reset_develop(effects),
            (Action::Undo, []) => return self.undo(effects),
            (Action::StepToggle, []) => return self.step_effect(false, effects),
            (Action::StepDelete, []) => return self.step_effect(true, effects),
            (Action::Uncrop, []) => return self.uncrop(effects),
            (Action::Exposure, [stops]) => return self.set_exposure(stops, effects),
            (Action::LookAt(n), []) => return self.look_at(usize::from(n), effects),
            (Action::Export, []) => return self.export(effects),
            (Action::DeleteRejected, []) => return self.delete_rejected(effects),
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
        // The mode strip is the target every mode shares: a press on it
        // changes the mode whatever is in view, the chooser included. The
        // status row covers it on a surface too short for the bands, as
        // it covers its pixels.
        if phase == PointerPhase::Press
            && self.surface.bounds().contains(x, y)
            && !Status::new(self.surface).rect().contains(x, y)
        {
            if let Some(index) = self.mode_strip().hit(x, y) {
                return self.press_mode(index);
            }
        }
        // The chooser owns the pointer while it is open: a press picks a
        // row, and nothing under it (the filter strip, a cell) is a target.
        if self.chooser.is_some() {
            return self.chooser_event(match phase {
                PointerPhase::Press => finder::Event::Press { x, y },
                PointerPhase::Move => finder::Event::Move { x, y },
                PointerPhase::Release => finder::Event::Release { x, y },
            });
        }
        // Develop mode: a crop or slider drag in progress owns the pointer
        // wherever it goes; otherwise the history pane, the two bands and
        // the filmstrip take it over their own pixels, and the preview
        // starts a crop drag on press. A press off all of them (the strips,
        // the status row, the margins) starts no drag, as a filter press is
        // inert here.
        if self.mode == Mode::Develop {
            // The history pane: a press on a step selects it, on a button
            // asks for what the button says; the pane's other pixels, and
            // a move or release over it, are inert. The pane is left of the
            // develop box, so nothing here starts or ends a crop drag.
            if let Some(result) = self.pane_pointer(phase, x, y) {
                return result;
            }
            // The tool and look bands above the preview: the slider's drag
            // owns the pointer once pressed; a press on a button asks what
            // it says; the bands' other pixels, and a move or release over
            // them, are inert.
            if let Some(result) = self.tool_pointer(phase, x, y) {
                return result;
            }
            // The filmstrip under the preview: a press on a box selects its
            // photo as the keys do (closing an open palette, as a photo
            // switch does); the band's other pixels, and a move or release
            // over it, are inert.
            if let Some(result) = self.film_pointer(phase, x, y) {
                return result;
            }
            // The look palette owns the develop box while it is open: a press on
            // a name picks that look and no crop drag starts under it; a move or
            // release, or a press off the names, is inert. The pick is set
            // through the same `Effect::Edit` the `look` action makes, so the
            // current-look mark rides the edit's settle; the palette stays open.
            if self.look_list {
                if phase == PointerPhase::Press {
                    if let Some(row) = self.look_row_at(x, y) {
                        let active = self.look_palette().and_then(|(_, active)| active);
                        if active != Some(row) {
                            // The picked name is always set, never the `look`
                            // action's `-` clear sentinel: the palette lists
                            // real looks, so a press sets the one under it.
                            if let (Some(index), Some(stem)) =
                                (self.develop_photo()?, self.looks.get(row).cloned())
                            {
                                return self.develop_effect(
                                    index,
                                    move |index, name| Effect::Edit {
                                        index,
                                        name,
                                        key: Key::Look,
                                        value: Some(stem),
                                    },
                                    Vec::new(),
                                );
                            }
                        }
                    }
                }
                return Ok((Outcome::Ignored, Vec::new()));
            }
            return self.crop_pointer(phase, x, y);
        }
        // Only a press, and only on the surface: a button the width does
        // not show whole is not a target. The bands are tested last
        // painted first, so on a surface too short for them the status row
        // covers the filter strip's buttons as it covers their pixels.
        if phase != PointerPhase::Press
            || !self.surface.bounds().contains(x, y)
            || Status::new(self.surface).rect().contains(x, y)
        {
            return Ok((Outcome::Ignored, Vec::new()));
        }
        if let Some(index) = self.filter_strip().hit(x, y) {
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

    /// The pointer over the history pane, `None` when it is not over it.
    /// A press on a shown step selects it, on Toggle or Delete asks for
    /// the selected step's, on Undo for the last step back; a press on
    /// the pane's chrome, and a move or release over it, are `Ignored`.
    fn pane_pointer(
        &mut self,
        phase: PointerPhase,
        x: i64,
        y: i64,
    ) -> Option<Result<(Outcome, Vec<Effect>), Error>> {
        // A crop or slider drag in progress owns the pointer wherever it
        // goes, so its release over the pane still ends it.
        if self.drag.is_some() || self.slider.is_some() {
            return None;
        }
        let layout = self.layout();
        let list = layout.history()?;
        let on_button = layout
            .history_buttons()
            .iter()
            .position(|button| button.is_some_and(|button| button.hit(x, y)));
        // The pane is the list and the band under it, the area's height.
        let pane = Rect {
            height: layout.area.height,
            ..list.rect()
        };
        if !pane.contains(x, y) {
            return None;
        }
        if phase != PointerPhase::Press {
            return Some(Ok((Outcome::Ignored, Vec::new())));
        }
        if let Some(row) = list.hit(x, y) {
            let outcome = self.select_step(self.step_first().saturating_add(row));
            return Some(Ok((self.finish(outcome), Vec::new())));
        }
        Some(match on_button {
            Some(0) => self.step_effect(false, Vec::new()),
            Some(1) => self.step_effect(true, Vec::new()),
            Some(2) => self.undo(Vec::new()),
            _ => Ok((Outcome::Ignored, Vec::new())),
        })
    }

    /// The pointer over the filmstrip's band, `None` when it is elsewhere
    /// or a crop drag is on. A press on a box selects its photo as the
    /// `select` action does; the band's other pixels, and a move or
    /// release over it, are inert.
    fn film_pointer(
        &mut self,
        phase: PointerPhase,
        x: i64,
        y: i64,
    ) -> Option<Result<(Outcome, Vec<Effect>), Error>> {
        // A crop drag in progress owns the pointer wherever it goes (the
        // slider's took its turn above).
        if self.drag.is_some() {
            return None;
        }
        let layout = self.layout();
        if !layout.film_band()?.contains(x, y) {
            return None;
        }
        if phase != PointerPhase::Press {
            return Some(Ok((Outcome::Ignored, Vec::new())));
        }
        let Some(position) = self.position() else {
            return Some(Ok((Outcome::Ignored, Vec::new())));
        };
        let hit = layout
            .film_boxes(self.shown.len(), position)
            .into_iter()
            .find(|(_, r#box)| r#box.contains(x, y));
        Some(match hit {
            Some((position, _)) => self
                .select(position)
                .map(|outcome| (self.finish(outcome), Vec::new())),
            None => Ok((Outcome::Ignored, Vec::new())),
        })
    }

    /// The pointer over the tool and look bands, `None` when it is over
    /// neither and no slider drag is on. A press on the slider starts a
    /// drag at the value under the pointer, a move takes it along, and the
    /// release commits the exposure through the `Edit` the `exposure`
    /// action makes, unless it is the value in force. A press on an
    /// enabled button asks what the button says; on the look band's, the
    /// look it names.
    fn tool_pointer(
        &mut self,
        phase: PointerPhase,
        x: i64,
        y: i64,
    ) -> Option<Result<(Outcome, Vec<Effect>), Error>> {
        if self.drag.is_some() {
            return None;
        }
        let layout = self.layout();
        let tools = layout.tools();
        if let Some(value) = self.slider {
            let Some(slider) = tools.slider else {
                self.slider = None;
                return Some(Ok((self.finish(Outcome::Changed), Vec::new())));
            };
            let at = slider.value_at(x, EXPOSURE_STEPS);
            return Some(match phase {
                PointerPhase::Press | PointerPhase::Move => {
                    if at == value {
                        Ok((Outcome::Ignored, Vec::new()))
                    } else {
                        self.slider = Some(at);
                        Ok((self.finish(Outcome::Changed), Vec::new()))
                    }
                }
                PointerPhase::Release => {
                    self.slider = None;
                    let held = self
                        .develop_photo()
                        .ok()
                        .flatten()
                        .map(|index| exposure_value(self.current_exposure(index)));
                    if held == Some(at) {
                        // Back where the knob was: a frame change only if the
                        // drag had moved it.
                        Ok((
                            self.finish(if value == at {
                                Outcome::Ignored
                            } else {
                                Outcome::Changed
                            }),
                            Vec::new(),
                        ))
                    } else {
                        // The knob paints the held value again from here,
                        // whatever becomes of the write (a refused edit
                        // settles to no change), so the frame moves now.
                        if held != Some(value) {
                            self.bump();
                        }
                        self.set_exposure(&library::exposure_text(exposure_at(at)), Vec::new())
                    }
                }
            });
        }
        let on_band = layout.tool_band().contains(x, y) || layout.look_band().contains(x, y);
        if !on_band {
            return None;
        }
        if phase != PointerPhase::Press {
            return Some(Ok((Outcome::Ignored, Vec::new())));
        }
        if let Some(slider) = tools.slider.filter(|slider| slider.hit(x, y)) {
            let at = slider.value_at(x, EXPOSURE_STEPS);
            self.slider = Some(at);
            let held = self
                .develop_photo()
                .ok()
                .flatten()
                .map(|index| exposure_value(self.current_exposure(index)));
            let outcome = if held == Some(at) {
                Outcome::Ignored
            } else {
                Outcome::Changed
            };
            return Some(Ok((self.finish(outcome), Vec::new())));
        }
        let ignored = Some(Ok((Outcome::Ignored, Vec::new())));
        let Some(index) = self.develop_photo().ok().flatten() else {
            return ignored;
        };
        if let Some(which) = tools
            .buttons
            .iter()
            .position(|button| button.is_some_and(|button| button.hit(x, y)))
        {
            if !self.tool_states(index).get(which).copied().unwrap_or(false) {
                return ignored;
            }
            return Some(match which {
                0 => self
                    .toggle_adjust()
                    .map(|outcome| (self.finish(outcome), Vec::new())),
                1 => self.uncrop(Vec::new()),
                2 => self.undo(Vec::new()),
                3 => self.reset_develop(Vec::new()),
                4 => self.expose(-EXPOSURE_STEP, Vec::new()),
                _ => self.expose(EXPOSURE_STEP, Vec::new()),
            });
        }
        let looks = layout.look_buttons(&self.looks);
        if let Some(which) = looks
            .iter()
            .position(|button| button.is_some_and(|button| button.hit(x, y)))
        {
            return Some(match which {
                0 => self.set_look("-", Vec::new()),
                n => match self.looks.get(n - 1).cloned() {
                    Some(stem) => self.set_look(&stem, Vec::new()),
                    None => Ok((Outcome::Ignored, Vec::new())),
                },
            });
        }
        ignored
    }

    /// The cursor photo's exposure in hundredths, zero without one.
    fn current_exposure(&self, index: usize) -> i32 {
        self.photos
            .get(index)
            .and_then(|photo| photo.sidecar.as_ref())
            .and_then(Sidecar::exposure)
            .unwrap_or(0)
    }

    /// Whether each tool button is enabled, `TOOL_BUTTONS` in order: Crop
    /// and the exposure steps always, Uncrop with a crop set, Undo and
    /// Reset with a step in the history.
    pub fn tool_states(&self, index: usize) -> [bool; 6] {
        let sidecar = self
            .photos
            .get(index)
            .and_then(|photo| photo.sidecar.as_ref());
        let cropped = sidecar.and_then(Sidecar::crop).is_some();
        let steps = sidecar.is_some_and(|sidecar| !sidecar.steps().is_empty());
        [true, cropped, steps, steps, true, true]
    }

    /// The exposure slider's value as painted: the drag's while one is
    /// on, else the cursor photo's exposure.
    pub fn slider_value(&self) -> usize {
        self.slider.unwrap_or_else(|| {
            self.cursor.map_or(exposure_value(0), |index| {
                exposure_value(self.current_exposure(index))
            })
        })
    }

    /// Asks for the cursor photo's crop cleared, the whole frame again:
    /// the `Edit` the `crop` action makes, with no value.
    fn uncrop(&mut self, effects: Vec<Effect>) -> Result<(Outcome, Vec<Effect>), Error> {
        let Some(index) = self.develop_photo()? else {
            return Ok((Outcome::Ignored, effects));
        };
        self.develop_effect(
            index,
            |index, name| Effect::Edit {
                index,
                name,
                key: Key::Crop,
                value: None,
            },
            effects,
        )
    }

    /// Sets the cursor photo's exposure to `stops`, the sidecar's spelling
    /// (two decimals in the range), else `BadArgument`; an absolute, unlike
    /// the nudges.
    fn set_exposure(
        &mut self,
        stops: &str,
        effects: Vec<Effect>,
    ) -> Result<(Outcome, Vec<Effect>), Error> {
        let Some(index) = self.develop_photo()? else {
            return Ok((Outcome::Ignored, effects));
        };
        let hundredths = library::exposure(stops).map_err(|_| Error::BadArgument)?;
        let value = library::exposure_text(hundredths);
        self.develop_effect(
            index,
            |index, name| Effect::Edit {
                index,
                name,
                key: Key::Exposure,
                value: Some(value),
            },
            effects,
        )
    }

    /// Sets the look to the `n`th (from 1) of the available looks, the
    /// look strip's order; `Ignored` when there is no such look.
    fn look_at(&mut self, n: usize, effects: Vec<Effect>) -> Result<(Outcome, Vec<Effect>), Error> {
        if self.develop_photo()?.is_none() || !(1..=9).contains(&n) {
            return Ok((Outcome::Ignored, effects));
        }
        match n.checked_sub(1).and_then(|i| self.looks.get(i)).cloned() {
            Some(stem) => self.set_look(&stem, effects),
            None => Ok((Outcome::Ignored, effects)),
        }
    }

    /// Asks for the last step of the cursor photo's history back: the
    /// adapter takes it off the file as it stands, `ignored` when the file
    /// holds none.
    fn undo(&mut self, effects: Vec<Effect>) -> Result<(Outcome, Vec<Effect>), Error> {
        let Some(index) = self.develop_photo()? else {
            return Ok((Outcome::Ignored, effects));
        };
        self.develop_effect(index, |index, name| Effect::Undo { index, name }, effects)
    }

    /// Asks for the selected history step deleted, or turned off or back
    /// on; `Ignored` with no step selected.
    fn step_effect(
        &mut self,
        delete: bool,
        effects: Vec<Effect>,
    ) -> Result<(Outcome, Vec<Effect>), Error> {
        let Some(index) = self.develop_photo()? else {
            return Ok((Outcome::Ignored, effects));
        };
        let Some(step) = self.history_step else {
            return Ok((Outcome::Ignored, effects));
        };
        self.develop_effect(
            index,
            move |index, name| {
                if delete {
                    Effect::StepDelete { index, name, step }
                } else {
                    Effect::StepToggle { index, name, step }
                }
            },
            effects,
        )
    }

    /// The pointer over the develop preview: the crop-adjust handles when the
    /// sub-mode is on, else the tighten marquee. Nothing else in develop uses
    /// the pointer. Both are witnessed by the frame, not `state`, so each step
    /// reports `Changed` with one generation bump exactly when the painted
    /// outline changes (`outline_changed`).
    fn crop_pointer(
        &mut self,
        phase: PointerPhase,
        x: i64,
        y: i64,
    ) -> Result<(Outcome, Vec<Effect>), Error> {
        if self.adjusting {
            self.adjust_pointer(phase, x, y)
        } else {
            self.marquee_pointer(phase, x, y)
        }
    }

    /// The tighten marquee: a press inside the canvas anchors a rectangle, a
    /// move rubber-bands it, a release commits the selected sub-region as the
    /// new crop (composed with the current crop, so always a subset of it)
    /// through the same `Effect::Edit` the `crop` action makes. A degenerate
    /// marquee or a plain click commits nothing. Arming or moving to a
    /// zero-edge, invisible marquee changes nothing; a release or a fresh press
    /// that removes a painted marquee is a change.
    fn marquee_pointer(
        &mut self,
        phase: PointerPhase,
        x: i64,
        y: i64,
    ) -> Result<(Outcome, Vec<Effect>), Error> {
        let ignored = Ok((Outcome::Ignored, Vec::new()));
        match phase {
            PointerPhase::Press => {
                let Some(canvas) = self.canvas() else {
                    return ignored;
                };
                if !canvas.contains(x, y) {
                    return ignored;
                }
                // Capture the canvas with the drag: a fit the adapter reports
                // mid-gesture cannot re-map an anchor taken against the old one.
                let before = self.painted();
                let point = (x, y);
                self.drag = Some(Drag {
                    canvas,
                    anchor: point,
                    current: point,
                    aspect: self.aspect,
                    grip: Grip::Marquee,
                });
                Ok(self.outline_changed(before))
            }
            PointerPhase::Move => {
                let Some(canvas) = self.drag.as_ref().map(|drag| drag.canvas) else {
                    return ignored;
                };
                let point = clamp_into(canvas, x, y);
                let before = self.painted();
                if let Some(drag) = self.drag.as_mut() {
                    drag.current = point;
                }
                Ok(self.outline_changed(before))
            }
            PointerPhase::Release => {
                let before = self.painted();
                let Some(drag) = self.drag.take() else {
                    return ignored;
                };
                let canvas = drag.canvas;
                // The release point ends the gesture as a move to it would, so
                // a press then release with no move between still selects.
                let current = clamp_into(canvas, x, y);
                let marquee = marquee_now(&Drag {
                    canvas,
                    anchor: drag.anchor,
                    current,
                    aspect: drag.aspect,
                    grip: Grip::Marquee,
                });
                // The marquee leaves the frame on release; if it was painted,
                // that is a frame change whatever the commit does -- a crop
                // equal to the current one settles to no change and would not
                // otherwise repaint, leaving the outline behind.
                if before.is_some() {
                    self.bump();
                }
                // The cursor is the develop photo; a drag only exists here.
                let commit = self.cursor.and_then(|index| {
                    self.drag_crop(index, canvas, marquee)
                        .map(|crop| (index, crop))
                });
                match commit {
                    Some((index, crop)) => self.develop_effect(
                        index,
                        |index, name| Effect::Edit {
                            index,
                            name,
                            key: Key::Crop,
                            value: Some(crop.text()),
                        },
                        Vec::new(),
                    ),
                    None if before.is_some() => Ok((Outcome::Changed, Vec::new())),
                    None => ignored,
                }
            }
        }
    }

    /// The crop-adjust handles over the uncropped preview: a press grabs the
    /// crop rectangle's edge, corner or interior (`classify`), a move resizes
    /// or moves it (`commit_crop`, kept inside the unit square at the minimum
    /// edge), and a release commits the result as an *absolute* crop (it can
    /// grow the crop, unlike the tighten marquee) through the same `Effect::Edit`
    /// the `crop` action makes, clearing the crop when it covers the whole image.
    fn adjust_pointer(
        &mut self,
        phase: PointerPhase,
        x: i64,
        y: i64,
    ) -> Result<(Outcome, Vec<Effect>), Error> {
        let ignored = Ok((Outcome::Ignored, Vec::new()));
        match phase {
            PointerPhase::Press => {
                let (Some(canvas), Some(index)) = (self.canvas(), self.cursor) else {
                    return ignored;
                };
                let crop = self.current_crop(index);
                let rect = crop_on_canvas(canvas, crop);
                let grip = (8 * self.surface.scale.value()).max(1) as i64;
                let Some(zone) = classify(rect, canvas, x, y, grip) else {
                    return ignored;
                };
                let before = self.painted();
                let point = (x, y);
                self.drag = Some(Drag {
                    canvas,
                    anchor: point,
                    current: point,
                    aspect: self.aspect,
                    grip: Grip::Handle { zone, crop },
                });
                // Grabbing a handle paints the same rectangle already shown, so
                // no frame change yet.
                Ok(self.outline_changed(before))
            }
            PointerPhase::Move => {
                let Some(canvas) = self.drag.as_ref().map(|drag| drag.canvas) else {
                    return ignored;
                };
                let point = clamp_into(canvas, x, y);
                let before = self.painted();
                if let Some(drag) = self.drag.as_mut() {
                    drag.current = point;
                }
                Ok(self.outline_changed(before))
            }
            PointerPhase::Release => {
                let before = self.painted();
                let Some(drag) = self.drag.take() else {
                    return ignored;
                };
                let canvas = drag.canvas;
                let Grip::Handle { zone, crop: start } = drag.grip else {
                    return ignored;
                };
                let current = clamp_into(canvas, x, y);
                let (dx, dy) = (current.0 - drag.anchor.0, current.1 - drag.anchor.1);
                // A grab released without moving commits nothing, like a marquee
                // click; the overlay stays the crop it already shows.
                if dx == 0 && dy == 0 {
                    return Ok(self.outline_changed(before));
                }
                let commit = self.cursor.and_then(|index| {
                    commit_crop(canvas, start, zone, dx, dy, drag.aspect).map(|crop| (index, crop))
                });
                match commit {
                    Some((index, crop)) => {
                        // A full-frame result clears the crop; else the absolute
                        // crop. The overlay snaps from the live rectangle to the
                        // crop the settle records; witness that snap here and let
                        // the settle bump for the value (both this turn, one
                        // frame). `commit_crop` also drove the live overlay, so
                        // the snap is usually nothing and only the settle bumps.
                        let settled = crop_on_canvas(canvas, crop);
                        let settled = (settled.width > 0 && settled.height > 0)
                            .then_some(Painted::Crop(true, settled));
                        if settled != before {
                            self.bump();
                        }
                        let value = (crop != FULL_CROP).then(|| crop.text());
                        self.develop_effect(
                            index,
                            |index, name| Effect::Edit {
                                index,
                                name,
                                key: Key::Crop,
                                value,
                            },
                            Vec::new(),
                        )
                    }
                    // An invalid result commits nothing; the overlay snaps back
                    // to the crop rectangle, a change only if the drag moved it.
                    None => Ok(self.outline_changed(before)),
                }
            }
        }
    }

    /// The crop a marquee selects: its fractions of the `canvas` composed
    /// with the cursor photo's current crop, so the result is a sub-region
    /// of it. `None` when the marquee is degenerate or the composed box is
    /// under the minimum edge (`Crop::new`); floor division keeps it inside
    /// the current crop, so it is always inside the image.
    fn drag_crop(&self, index: usize, canvas: Rect, marquee: Rect) -> Option<Crop> {
        if marquee.width == 0 || marquee.height == 0 {
            return None;
        }
        let (w, h) = (u64::from(canvas.width), u64::from(canvas.height));
        if w == 0 || h == 0 {
            return None;
        }
        self.photos.get(index)?;
        let current = self.current_crop(index);
        let dx = (marquee.x - canvas.x).max(0) as u64;
        let dy = (marquee.y - canvas.y).max(0) as u64;
        let (dw, dh) = (u64::from(marquee.width), u64::from(marquee.height));
        let (cw, ch) = (u64::from(current.width), u64::from(current.height));
        let nx = u64::from(current.x) + dx * cw / w;
        let ny = u64::from(current.y) + dy * ch / h;
        let nw = dw * cw / w;
        let nh = dh * ch / h;
        Crop::new(nx as u32, ny as u32, nw as u32, nh as u32).ok()
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

    /// A filter is the cull grid's; in develop mode it is ignored, since
    /// develop is scoped to the one photo. The strips are still painted, so the
    /// keys `1`-`4` and a press on it are inert here, not absent.
    fn set_filter(&mut self, filter: Filter) -> Outcome {
        if self.mode == Mode::Develop || self.filter == filter {
            return Outcome::Ignored;
        }
        self.filter = filter;
        self.refresh_shown();
        self.keep_cursor_shown();
        Outcome::Changed
    }

    /// `choose`: opens the roll chooser, asking the adapter to list the
    /// folder beside the open roll with the roll selected (its working
    /// directory when none is open), or closes an open one. The opening
    /// is the adapter's change to make through `set_listing`; a folder
    /// that cannot be read opens nothing.
    fn choose(&mut self, mut effects: Vec<Effect>) -> Result<(Outcome, Vec<Effect>), Error> {
        if self.chooser.is_some() {
            self.chooser = None;
            return Ok((self.finish(Outcome::Changed), effects));
        }
        effects.push(match self.roll.as_ref() {
            Some(roll) => Effect::List {
                folder: Some(roll.path.clone()),
                parent: true,
            },
            None => Effect::List {
                folder: None,
                parent: false,
            },
        });
        Ok((Outcome::Changed, effects))
    }

    /// A chord while the chooser is open: the finder's keys by their
    /// names, `C-Return` its accept, `M-Up` and `^` its parent (so a caret
    /// in a name cannot be filtered by; the rest of the name can), and a
    /// single printable character the filter's; any other chord is
    /// `ignored`.
    fn chooser_key(&mut self, chord: &str) -> Result<(Outcome, Vec<Effect>), Error> {
        let key = |key| finder::Event::Key {
            key,
            repeated: false,
        };
        let event = match chord {
            "Up" => key(finder::Key::Up),
            "Down" => key(finder::Key::Down),
            "PageUp" => key(finder::Key::PageUp),
            "PageDown" => key(finder::Key::PageDown),
            "Home" => key(finder::Key::Home),
            "End" => key(finder::Key::End),
            "Return" => key(finder::Key::Activate),
            "C-Return" => key(finder::Key::Accept),
            "Backspace" => key(finder::Key::Backspace),
            "M-Up" | "^" => key(finder::Key::Parent),
            "Escape" => key(finder::Key::Escape),
            _ => {
                let mut chars = chord.chars();
                match (chars.next(), chars.next()) {
                    (Some(c), None) if !c.is_control() => finder::Event::Insert(c),
                    _ => return Ok((Outcome::Ignored, Vec::new())),
                }
            }
        };
        self.chooser_event(event)
    }

    /// The wheel while the chooser is open scrolls its list.
    fn chooser_wheel(&mut self, rows: i64) -> Result<(Outcome, Vec<Effect>), Error> {
        let Some(list) = self.chooser.as_ref().map(|c| c.finder.list_rect()) else {
            return Ok((Outcome::Ignored, Vec::new()));
        };
        let rows = isize::try_from(rows).unwrap_or(if rows < 0 { isize::MIN } else { isize::MAX });
        self.chooser_event(finder::Event::Wheel {
            x: list.x,
            y: list.y,
            rows,
        })
    }

    /// One finder event and what it asks of the adapter: a descent lists
    /// the folder under the cursor, an ascent the parent with this folder
    /// selected (nothing above the root), `Here` opens the listed folder as
    /// the roll, and a close of any kind takes the chooser down. A descent
    /// or an ascent is `changed` without moving the generation, as `open`
    /// is: the frame changes when the adapter installs the listing or
    /// notes the refusal.
    fn chooser_event(&mut self, event: finder::Event) -> Result<(Outcome, Vec<Effect>), Error> {
        let Some(chooser) = self.chooser.as_mut() else {
            return Ok((Outcome::Ignored, Vec::new()));
        };
        let mut effects = Vec::new();
        let outcome = match chooser.finder.event(event) {
            finder::Outcome::Changed => Outcome::Changed,
            finder::Outcome::Ignored | finder::Outcome::Consumed => Outcome::Ignored,
            finder::Outcome::Descend(index) => {
                return Ok(match chooser.finder.listing().entries().get(index) {
                    Some(entry) => (
                        Outcome::Changed,
                        vec![Effect::List {
                            folder: Some(join_folder(&chooser.folder, entry.name())),
                            parent: false,
                        }],
                    ),
                    None => (Outcome::Ignored, effects),
                });
            }
            finder::Outcome::Ascend => {
                return Ok(if chooser.folder.iter().all(|byte| *byte == b'/') {
                    (Outcome::Ignored, effects)
                } else {
                    (
                        Outcome::Changed,
                        vec![Effect::List {
                            folder: Some(chooser.folder.clone()),
                            parent: true,
                        }],
                    )
                });
            }
            finder::Outcome::Closed(choice) => {
                let folder = std::mem::take(&mut chooser.folder);
                self.chooser = None;
                if choice == finder::Choice::Here {
                    effects.push(Effect::Open(folder));
                }
                Outcome::Changed
            }
        };
        Ok((self.finish(outcome), effects))
    }

    /// `grid`/Escape backs out one level: from crop-adjust to plain develop,
    /// then from develop to the cull grid; from the single view, back to the
    /// grid; from the grid, nothing.
    fn back_to_grid(&mut self) -> Outcome {
        if self.mode == Mode::Develop && self.look_list {
            // Escape closes the palette first, then leaves develop; the
            // status row names the sub-mode, so closing it is a frame
            // change with or without a box the list painted over.
            self.look_list = false;
            return Outcome::Changed;
        }
        if self.mode == Mode::Develop && self.adjusting {
            self.adjusting = false;
            self.drag = None;
            return Outcome::Changed;
        }
        if self.mode == Mode::Develop {
            self.leave_develop();
            return Outcome::Changed;
        }
        self.set_view(View::Grid)
    }

    /// Toggles the crop-adjust sub-mode for the cursor's photo; only in develop
    /// mode (`Ignored` in cull, as the other develop edits are). The display
    /// changes -- the crop overlay appears or the tighten preview returns -- so
    /// it is a frame change witnessed by the painted outline; the settle-bump
    /// is `finish`'s, on the `Changed` this returns.
    fn toggle_adjust(&mut self) -> Result<Outcome, Error> {
        if self.develop_photo()?.is_none() {
            return Ok(Outcome::Ignored);
        }
        self.adjusting = !self.adjusting;
        self.drag = None;
        // Crop-adjust and the look palette are mutually exclusive overlays.
        if self.adjusting {
            self.look_list = false;
        }
        // The status row names the sub-mode and the Crop button shows it,
        // so the toggle is a frame change with or without a box.
        Ok(Outcome::Changed)
    }

    /// Toggles the look palette for the cursor's photo; only in
    /// develop mode (`Ignored` in cull, as the other develop edits are).
    /// Opening it closes crop-adjust, the two being mutually exclusive
    /// overlays; opening over an empty list paints nothing, so it stays closed
    /// and is `Ignored`. The palette is a frame change witnessed by the painted
    /// overlay; the settle-bump is `finish`'s, on the `Changed` this returns.
    fn toggle_looks(&mut self) -> Result<Outcome, Error> {
        if self.develop_photo()?.is_none() {
            return Ok(Outcome::Ignored);
        }
        if self.look_list {
            self.look_list = false;
        } else {
            self.look_list = !self.looks.is_empty();
            if self.look_list {
                self.adjusting = false;
                self.drag = None;
            } else {
                return Ok(Outcome::Ignored);
            }
        }
        // The status row names the sub-mode, so the toggle is a frame
        // change with or without a box to list the looks over.
        Ok(Outcome::Changed)
    }

    /// Enters develop mode for the cursor's photo. A roll with a cursor is
    /// needed, as the single view is; already developing is `Ignored`.
    fn enter_develop(&mut self) -> Result<Outcome, Error> {
        self.need_photo()?;
        if self.mode == Mode::Develop {
            return Ok(Outcome::Ignored);
        }
        self.mode = Mode::Develop;
        // Develop leaves for the cull grid, so the reported view is the
        // grid throughout, not a stale `single` it will not return to.
        self.view = View::Grid;
        self.drag = None;
        self.slider = None;
        self.adjusting = false;
        self.aspect = Aspect::Free;
        self.look_list = false;
        self.sync_step(0);
        Ok(Outcome::Changed)
    }

    /// The photo a develop edit acts on: the cursor, only in develop mode.
    /// Not developing, the edit is not this mode's and is `Ignored`; the
    /// cursor is always set in develop mode, so its absence is `NoPhoto`.
    fn develop_photo(&self) -> Result<Option<usize>, Error> {
        if self.mode != Mode::Develop {
            return Ok(None);
        }
        Ok(Some(self.need_photo()?))
    }

    fn develop_effect(
        &self,
        index: usize,
        effect: impl FnOnce(usize, String) -> Effect,
        mut effects: Vec<Effect>,
    ) -> Result<(Outcome, Vec<Effect>), Error> {
        let name = self.photos.get(index).ok_or(Error::NoPhoto)?.name.clone();
        effects.push(effect(index, name));
        Ok((Outcome::Changed, effects))
    }

    /// Adjusts the cursor photo's exposure by `delta` hundredths of a stop.
    /// The delta is applied to the file's exposure at the adapter, not the
    /// model's copy, so a value changed meanwhile is added to, and a delta
    /// that clamps to no change settles as `Ignored`.
    fn expose(
        &mut self,
        delta: i32,
        effects: Vec<Effect>,
    ) -> Result<(Outcome, Vec<Effect>), Error> {
        let Some(index) = self.develop_photo()? else {
            return Ok((Outcome::Ignored, effects));
        };
        self.develop_effect(
            index,
            |index, name| Effect::Expose { index, name, delta },
            effects,
        )
    }

    /// Sets the cursor photo's look to `stem`, or clears it with `-`. A
    /// value that is not a look stem is `BadArgument`, judged here so it is
    /// the wire's `bad-argument`, not a refused write.
    fn set_look(
        &mut self,
        stem: &str,
        effects: Vec<Effect>,
    ) -> Result<(Outcome, Vec<Effect>), Error> {
        let Some(index) = self.develop_photo()? else {
            return Ok((Outcome::Ignored, effects));
        };
        let value = if stem == "-" {
            None
        } else if library::valid_look(stem) {
            Some(stem.to_string())
        } else {
            return Err(Error::BadArgument);
        };
        self.develop_effect(
            index,
            |index, name| Effect::Edit {
                index,
                name,
                key: Key::Look,
                value,
            },
            effects,
        )
    }

    /// Sets the cursor photo's crop to the fractions `x y w h`. A box that
    /// is not four fractions, or is under the minimum edge or outside the
    /// image, is `BadArgument`.
    fn set_crop(
        &mut self,
        x: &str,
        y: &str,
        w: &str,
        h: &str,
        effects: Vec<Effect>,
    ) -> Result<(Outcome, Vec<Effect>), Error> {
        let Some(index) = self.develop_photo()? else {
            return Ok((Outcome::Ignored, effects));
        };
        let value = Crop::parse(&format!("{x} {y} {w} {h}"))
            .map_err(|_| Error::BadArgument)?
            .text();
        self.develop_effect(
            index,
            |index, name| Effect::Edit {
                index,
                name,
                key: Key::Crop,
                value: Some(value),
            },
            effects,
        )
    }

    /// Locks the crop drag to `ratio` (`free`, `3:2`, `4:3`, `1:1`, `16:9`), an
    /// unknown token being `BadArgument`. Picking a ratio only arms the lock for
    /// the next drag; it never reshapes the current crop, so it is always
    /// `Ignored` (no `Edit`, no generation bump). The lock is transient
    /// controller state, not a `state` field or sidecar key.
    fn set_aspect(
        &mut self,
        ratio: &str,
        effects: Vec<Effect>,
    ) -> Result<(Outcome, Vec<Effect>), Error> {
        if self.develop_photo()?.is_none() {
            return Ok((Outcome::Ignored, effects));
        }
        // Arm the lock for the next drag; picking a ratio never reshapes the
        // current crop on its own. An immediate snap would run against the
        // canvas last reported for the *cropped* preview: crop-adjust develops
        // the frame uncropped only asynchronously, so a snap issued before that
        // fit lands would read the cropped preview's aspect as the whole
        // image's and silently commit a wrong-ratio crop, with no visible drag
        // to reveal it. Reshaping instead flows through a drag -- which the eye
        // follows and which self-corrects once the uncropped fit arrives -- and
        // the immediate snap-on-pick waits for the slice that reports the
        // displayed crop alongside the fit.
        self.aspect = Aspect::parse(ratio).ok_or(Error::BadArgument)?;
        Ok((Outcome::Ignored, effects))
    }

    /// Asks for the cursor photo's export, in either mode: the export is not
    /// a develop edit but the roll's, so the cull grid exports too. The
    /// adapter carries it out and reports through `set_export`.
    fn export(&mut self, effects: Vec<Effect>) -> Result<(Outcome, Vec<Effect>), Error> {
        let index = self.need_photo()?;
        self.develop_effect(index, |index, name| Effect::Export { index, name }, effects)
    }

    /// Asks for the roll's rejects to be moved into `rejected/`: the cull
    /// grid's action, as the filters are, so develop mode ignores it. The
    /// files say which photos are rejects, not the model's copy, so the
    /// dispatch asks whenever a roll is open; the adapter moves them and
    /// takes them out of the model through `remove`, which is the change.
    fn delete_rejected(
        &mut self,
        mut effects: Vec<Effect>,
    ) -> Result<(Outcome, Vec<Effect>), Error> {
        self.need_roll()?;
        if self.mode == Mode::Develop {
            return Ok((Outcome::Ignored, effects));
        }
        effects.push(Effect::DeleteRejected);
        Ok((Outcome::Changed, effects))
    }

    /// Resets the cursor photo's develop keys to camera defaults, keeping
    /// the flag.
    fn reset_develop(&mut self, effects: Vec<Effect>) -> Result<(Outcome, Vec<Effect>), Error> {
        let Some(index) = self.develop_photo()? else {
            return Ok((Outcome::Ignored, effects));
        };
        self.develop_effect(index, |index, name| Effect::Reset { index, name }, effects)
    }

    fn set_view(&mut self, view: View) -> Outcome {
        if self.view == view {
            return Outcome::Ignored;
        }
        self.view = view;
        Outcome::Changed
    }

    /// The cursor stays where it is when the filter still shows it, else
    /// goes to the first shown photo, else nowhere, and the single and
    /// develop views end with it, since each is a view of the cursor's
    /// photo.
    fn keep_cursor_shown(&mut self) {
        if self.position().is_none() {
            self.cursor = self.shown.first().copied();
            // The cursor left the photo the drag, crop-adjust, aspect lock and
            // look palette were for; end them.
            self.drag = None;
            self.slider = None;
            self.adjusting = false;
            self.aspect = Aspect::Free;
            self.look_list = false;
        }
        if self.cursor.is_none() {
            self.view = View::Grid;
            self.mode = Mode::Cull;
            self.drag = None;
            self.slider = None;
            self.adjusting = false;
            self.aspect = Aspect::Free;
            self.look_list = false;
            self.history_step = None;
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
        // The cursor moved off the photo the drag, crop-adjust, aspect lock
        // and look palette were for; end them.
        self.drag = None;
        self.slider = None;
        self.adjusting = false;
        self.aspect = Aspect::Free;
        self.look_list = false;
        self.reveal();
        // The history is the new photo's; its newest step is selected.
        self.sync_step(0);
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

/// `folder/name`, one separator between them.
fn join_folder(folder: &[u8], name: &str) -> Vec<u8> {
    let mut joined = Vec::with_capacity(folder.len() + 1 + name.len());
    joined.extend_from_slice(folder);
    if !folder.ends_with(b"/") {
        joined.push(b'/');
    }
    joined.extend_from_slice(name.as_bytes());
    joined
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

/// What the window shows: the mode and filter strips, the grid or the single photo,
/// and the status row, laid out for the model's surface.
pub struct Scene<'a> {
    model: &'a Controller,
}

impl Scene<'_> {
    /// The status row's line.
    pub fn status_line(&self) -> String {
        let model = self.model;
        if model.chooser.is_some() {
            return "Choose a roll: Return enter, Backspace or ^ up, C-Return open here, \
                    Escape cancel; type to filter"
                .to_string();
        }
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
        match model.mode {
            Mode::Develop => {
                line.push_str(" | develop");
                if model.look_palette().is_some() {
                    line.push_str(" looks");
                } else if model.adjusting {
                    line.push_str(" crop-adjust");
                }
            }
            Mode::Cull if model.view == View::Single => line.push_str(" | single"),
            Mode::Cull => {}
        }
        if let Some(note) = &model.export {
            line.push_str(" | ");
            line.push_str(note);
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
            outline(rect, (2 * s) as u32, SELECTED, damage, sink);
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

    /// The history pane at the area's left: the cursor photo's steps as a
    /// list, a step that is off dimmed, the selected one highlighted, and
    /// the three buttons under it, Toggle and Delete enabled with a
    /// selection, Undo with a step.
    fn history(&self, layout: &Layout, damage: Rect, sink: &mut dyn FnMut(Draw)) {
        let model = self.model;
        let steps = model.steps();
        // The pane's strip is chrome first, whatever the area holds of it,
        // so a surface too short for a row, or narrower than the pane, is
        // painted whole.
        let pane = (PANE_W * layout.surface.scale.value()) as u32;
        fill(
            Rect {
                width: layout.area.width.min(pane),
                ..layout.area
            },
            CHROME,
            damage,
            sink,
        );
        if let Some(list) = layout.history() {
            let first = model.step_first();
            let labels: Vec<String> = steps
                .iter()
                .skip(first)
                .take(list.rows())
                .map(step_label)
                .collect();
            let items = labels
                .iter()
                .zip(steps.iter().skip(first))
                .map(|(label, step)| Item {
                    label,
                    meta: "",
                    enabled: step.on,
                    marked: false,
                });
            list.emit(
                items,
                first,
                model.history_step.unwrap_or(usize::MAX),
                steps.len(),
                damage,
                sink,
            );
        }
        let selected = model.history_step.is_some();
        let enabled = [selected, selected, !steps.is_empty()];
        for ((button, label), enabled) in layout
            .history_buttons()
            .into_iter()
            .zip(HISTORY_BUTTONS)
            .zip(enabled)
        {
            if let Some(button) = button {
                button.emit(label, false, enabled, damage, sink);
            }
        }
    }

    /// The filmstrip: its band chrome, a placeholder and badge per box, and
    /// the cursor's box outlined in its padding, as the grid's cell is.
    fn film(&self, layout: &Layout, damage: Rect, sink: &mut dyn FnMut(Draw)) {
        let model = self.model;
        let Some(band) = layout.film_band() else {
            return;
        };
        fill(band, CHROME, damage, sink);
        let s = layout.surface.scale.value();
        let half = ((CELL_PAD / 2) * s) as i64;
        for (index, thumb) in model.film() {
            let Some(photo) = model.photos.get(index) else {
                continue;
            };
            if model.cursor == Some(index) {
                let around = Rect {
                    x: thumb.x - half,
                    y: thumb.y - half,
                    width: thumb.width + 2 * half as u32,
                    height: thumb.height + 2 * half as u32,
                };
                outline(around, (2 * s) as u32, SELECTED, damage, sink);
            }
            fill(thumb, PLACEHOLDER, damage, sink);
            badge(layout, thumb, photo, damage, sink);
        }
    }

    /// The tool band and the look band over the develop view: chrome, the
    /// tool buttons with the crop-adjust one selected while it is on and
    /// each enabled as `tool_states` says, the exposure slider at the
    /// value in force or under the drag, then the look buttons with the
    /// current look selected (`None` without one).
    fn bands(&self, layout: &Layout, index: usize, damage: Rect, sink: &mut dyn FnMut(Draw)) {
        let model = self.model;
        fill(layout.tool_band(), CHROME, damage, sink);
        fill(layout.look_band(), CHROME, damage, sink);
        let tools = layout.tools();
        let states = model.tool_states(index);
        let buttons = tools.buttons.into_iter().zip(TOOL_BUTTONS).zip(states);
        for (which, ((button, label), enabled)) in buttons.enumerate() {
            if let Some(button) = button {
                button.emit(label, which == 0 && model.adjusting, enabled, damage, sink);
            }
        }
        if let Some(slider) = tools.slider {
            slider.emit(model.slider_value(), EXPOSURE_STEPS, true, damage, sink);
        }
        let current = model.current_look(index);
        let labels = std::iter::once(NO_LOOK).chain(model.looks.iter().map(String::as_str));
        let buttons = layout.look_buttons(&model.looks).into_iter().zip(labels);
        for (which, (button, label)) in buttons.enumerate() {
            if let Some(button) = button {
                // The first is the clear, by place: a look named as it is
                // a look.
                let selected = if which == 0 {
                    current.is_none()
                } else {
                    current == Some(label)
                };
                button.emit(label, selected, true, damage, sink);
            }
        }
    }

    /// The single view over `view.region`: the name, the facts when
    /// `view.facts` (the cull view's; develop's bands and pane say them)
    /// and the preview `view.r#box` under them (the cull view's
    /// `preview_box`, develop's `develop_box`; `None` when the region
    /// cannot hold one).
    fn single(
        &self,
        layout: &Layout,
        view: SingleView,
        photo: &Photo,
        damage: Rect,
        sink: &mut dyn FnMut(Draw),
    ) {
        let SingleView {
            region,
            r#box,
            facts,
        } = view;
        let s = layout.surface.scale.value();
        let scale = layout.surface.scale;
        fill(region, PAPER, damage, sink);
        let pad = (CELL_PAD * s) as i64;
        let row = (CELL_HEIGHT * s) as i64;
        let text = Rect {
            x: region.x + pad,
            y: region.y + pad / 2,
            width: (region.width as i64 - 2 * pad).max(0) as u32,
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
        if facts {
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
        }
        // The preview's place: the largest 3:2 box under the text rows, the
        // same rectangle the window and `--preview` blit the developed image
        // into, so the placeholder and the image share one geometry.
        if let Some(r#box) = r#box {
            fill(r#box, PLACEHOLDER, damage, sink);
            // The develop overlay over the preview -- the crop overlay, or the
            // look palette when it is open -- clipped to the box so it cannot
            // stray past it; drawn here so it is in the scene frame, the
            // `--preview` PPM and the replay `text` oracle, and repainted over
            // the blitted image on the live window as the badges are.
            paint_develop_overlay(self.model, r#box, scale, damage, sink);
        }
    }
}

/// The crop overlay over the develop box: in crop-adjust the crop rectangle
/// with its edge, corner and move handles; otherwise the tighten marquee. It
/// is chrome (frame fills), clipped to the box, so photo pixels still reach the
/// frame through `blit` alone. Painted by the scene (so the replay `frame` and
/// `--preview` witness it) and again by the window over the blitted image.
fn paint_crop(
    model: &Controller,
    r#box: Rect,
    scale: usize,
    damage: Rect,
    sink: &mut dyn FnMut(Draw),
) {
    let Some(clip) = r#box.intersection(damage) else {
        return;
    };
    let edge = (2 * scale) as u32;
    if model.adjusting() {
        let Some(rect) = model.crop_adjust_rect() else {
            return;
        };
        if rect.width == 0 || rect.height == 0 {
            return;
        }
        outline(rect, edge, SELECTED, clip, sink);
        let size = (6 * scale).max(1) as i64;
        for (cx, cy) in handle_marks(rect) {
            let mark = Rect {
                x: cx - size / 2,
                y: cy - size / 2,
                width: size as u32,
                height: size as u32,
            };
            fill(mark, SELECTED, clip, sink);
        }
    } else if let Some(rect) = model.crop_drag() {
        if rect.width > 0 && rect.height > 0 {
            outline(rect, edge, SELECTED, clip, sink);
        }
    }
}

/// The centres of a crop rectangle's eight handles: its four corners and its
/// four edge midpoints.
fn handle_marks(rect: Rect) -> [(i64, i64); 8] {
    let (l, t) = (rect.x, rect.y);
    let r = rect.x + i64::from(rect.width);
    let b = rect.y + i64::from(rect.height);
    let mx = rect.x + i64::from(rect.width) / 2;
    let my = rect.y + i64::from(rect.height) / 2;
    [
        (l, t),
        (r, t),
        (l, b),
        (r, b),
        (mx, t),
        (mx, b),
        (l, my),
        (r, my),
    ]
}

/// The develop box's overlay: the look palette when it is open, else the crop
/// overlay. Mutually exclusive; painted by the scene and again by the window
/// over the blitted image.
fn paint_develop_overlay(
    model: &Controller,
    r#box: Rect,
    scale: Scale,
    damage: Rect,
    sink: &mut dyn FnMut(Draw),
) {
    if model.look_palette().is_some() {
        paint_looks(model, r#box, scale, damage, sink);
    } else {
        paint_crop(model, r#box, scale.value(), damage, sink);
    }
}

/// The look palette over the develop box: an opaque panel listing the
/// available look stems, the current one marked, a press picking the one under
/// it. Chrome (frame fills and glyphs), clipped to the box, so photo pixels
/// reach the frame only through `blit` when the palette is closed. Rows past
/// the box are not shown (scroll is a later slice).
fn paint_looks(
    model: &Controller,
    r#box: Rect,
    scale: Scale,
    damage: Rect,
    sink: &mut dyn FnMut(Draw),
) {
    let Some((looks, active)) = model.look_palette() else {
        return;
    };
    let Some(clip) = r#box.intersection(damage) else {
        return;
    };
    let s = scale.value();
    // An opaque panel so the names read over the develop image.
    fill(r#box, CHROME, clip, sink);
    let pad = (CELL_PAD * s) as i64;
    let row = (CELL_HEIGHT * s) as i64;
    let bottom = r#box.y + i64::from(r#box.height);
    let width = (i64::from(r#box.width) - 2 * pad).max(0) as u32;
    for (index, look) in looks.iter().enumerate() {
        let y = r#box.y + pad + row * index as i64;
        if y + row > bottom {
            break;
        }
        let line = Rect {
            x: r#box.x + pad,
            y,
            width,
            height: row as u32,
        };
        let current = active == Some(index);
        let background = if current {
            fill(line, SELECTED, clip, sink);
            SELECTED
        } else {
            CHROME
        };
        let ink = if current { PAPER } else { INK };
        text_run(
            scale,
            look.chars(),
            (line.x, line.y),
            line,
            GlyphStyle::medium(ink, background),
            clip,
            sink,
        );
    }
}

impl Composition for Scene<'_> {
    fn surface(&self) -> Surface {
        self.model.surface
    }

    fn emit(&self, damage: Rect, sink: &mut dyn FnMut(Draw)) {
        let model = self.model;
        let layout = model.layout();
        model.mode_strip().emit(model.mode_states(), damage, sink);
        model
            .filter_strip()
            .emit(model.filter_states(), damage, sink);
        if let Some(chooser) = &model.chooser {
            // The finder stands in for the area, whatever is open behind it.
            chooser.finder.emit(damage, sink);
            Status::new(layout.surface).emit(self.status_line().chars(), damage, sink);
            return;
        }
        let cursor_photo = model.cursor.and_then(|index| model.photos.get(index));
        match (&model.roll, model.mode, model.view) {
            (None, _, _) => {
                fill(layout.area, PAPER, damage, sink);
                if let Some(block) = Block::new(layout.surface, layout.area.y, 2) {
                    block.emit(
                        "No roll open.\no chooses one; action open HEX_PATH opens one; --replay ROLL opens it at start.",
                        damage,
                        sink,
                    );
                }
            }
            // The develop view shows the cursor photo's facts and a preview
            // box, the single view's layout until the raw render lands (a
            // later increment); the status row names the mode.
            (Some(_), Mode::Develop, _) => match cursor_photo {
                Some(photo) => {
                    self.history(&layout, damage, sink);
                    if let Some(index) = model.cursor {
                        self.bands(&layout, index, damage, sink);
                    }
                    self.film(&layout, damage, sink);
                    self.single(
                        &layout,
                        SingleView {
                            region: layout.develop_view(),
                            r#box: layout.develop_box(),
                            facts: false,
                        },
                        photo,
                        damage,
                        sink,
                    )
                }
                None => self.grid(&layout, damage, sink),
            },
            (Some(_), Mode::Cull, View::Single) => match cursor_photo {
                Some(photo) => self.single(
                    &layout,
                    SingleView {
                        region: layout.area,
                        r#box: layout.preview_box(),
                        facts: true,
                    },
                    photo,
                    damage,
                    sink,
                ),
                None => self.grid(&layout, damage, sink),
            },
            (Some(_), Mode::Cull, View::Grid) => self.grid(&layout, damage, sink),
        }
        Status::new(layout.surface).emit(self.status_line().chars(), damage, sink);
    }
}

/// An `edge`-thick frame just inside `rect` in `color`: the four sides as
/// fills, clipped to `damage`. The selection border and the crop marquee.
fn outline(rect: Rect, edge: u32, color: u32, damage: Rect, sink: &mut dyn FnMut(Draw)) {
    fill(
        Rect {
            height: edge,
            ..rect
        },
        color,
        damage,
        sink,
    );
    fill(
        Rect {
            y: rect.y + i64::from(rect.height) - i64::from(edge),
            height: edge,
            ..rect
        },
        color,
        damage,
        sink,
    );
    fill(
        Rect {
            width: edge,
            ..rect
        },
        color,
        damage,
        sink,
    );
    fill(
        Rect {
            x: rect.x + i64::from(rect.width) - i64::from(edge),
            width: edge,
            ..rect
        },
        color,
        damage,
        sink,
    );
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

/// The develop overlay and nothing else -- the crop overlay, or the look
/// palette when it is open -- a composition the window paints over the develop
/// image it blitted, clipped to the box. So painted, over the scene's own frame
/// it changes nothing: it is the scene's overlay at the same place.
pub struct Marquee<'a> {
    model: &'a Controller,
}

impl Composition for Marquee<'_> {
    fn surface(&self) -> Surface {
        self.model.surface
    }

    fn emit(&self, damage: Rect, sink: &mut dyn FnMut(Draw)) {
        let Some(r#box) = self.model.develop_box() else {
            return;
        };
        paint_develop_overlay(self.model, r#box, self.model.surface.scale, damage, sink);
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

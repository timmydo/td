//! The driven UI's semantic seam: the half a consumer implements once so an
//! agent or a test can operate it without a protocol of its own. A consumer
//! gives a `Controller` over its own closed action type, a table of
//! `Binding`s naming those actions, its `state` body and its current
//! `Composition`; the toolkit routes the generic verbs (`state`, `actions`,
//! `action`, `key`, `pointer`, `wheel`, `resize`, `focus`, `tick`, `text`,
//! `frame`, `frame-page`) over `control`'s envelope, reads text back from
//! the draw stream and paints the frame for inspection. Whether a verb reads
//! or acts stays the consumer's: the router borrows for the reading verbs
//! and passes acting ones through the consumer's own dispatch.

use crate::control::{
    self, decimal, envelope, hex, ok, size, unhex, Envelope, ErrorCode, Parse, Refusal,
};
use crate::font::Font;
use crate::raster::{self, Composition, Primitive, Raster, Rect, Scale, Surface};
use crate::{CELL_HEIGHT, CELL_WIDTH};
use std::sync::OnceLock;

/// A key chord on the wire: bytes after hex decoding.
pub const KEY_BYTES: usize = 32;
/// Fields after the verb in one request.
pub const ARGUMENTS: usize = 8;
/// Frame bytes in one `frame-page` reply, hex-encoded to half a frame.
pub const PAGE_BYTES: usize = 256 * 1024;
/// Wheel deltas in whole rows or columns per request.
pub const WHEEL_LIMIT: i32 = 16_777_216;
/// The generic verbs. One of these with fields it does not take is
/// `protocol` at the router; only other names reach a consumer's
/// `request`.
pub const VERBS: [&str; 12] = [
    "state",
    "actions",
    "action",
    "key",
    "pointer",
    "wheel",
    "resize",
    "focus",
    "tick",
    "text",
    "frame",
    "frame-page",
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PointerPhase {
    Press,
    Move,
    Release,
}

/// One semantic input, as the keyboard, pointer, compositor or clock would
/// deliver it; the consumer's own key table turns a chord into an action.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Input<'a> {
    Key {
        chord: &'a str,
    },
    Pointer {
        phase: PointerPhase,
        x: u32,
        y: u32,
    },
    Wheel {
        rows: i32,
        columns: i32,
    },
    Resize {
        width: usize,
        height: usize,
        scale: u8,
    },
    Focus(bool),
    Tick(u64),
}

/// What a dispatch did, as the reply words it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Outcome {
    Changed,
    Ignored,
    Quit,
}

impl Outcome {
    pub fn word(self) -> &'static str {
        match self {
            Self::Changed => "changed",
            Self::Ignored => "ignored",
            Self::Quit => "quit",
        }
    }
}

/// One row of a consumer's action table: the name `action` takes, the
/// chord its keyboard binds by default, the argument shape and the help
/// line, so an agent reads the table instead of guessing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Binding {
    pub name: &'static str,
    pub chord: Option<&'static str>,
    pub arguments: &'static str,
    pub help: &'static str,
}

/// A consumer's driven surface. The reading verbs borrow; `action` and
/// `input` are the consumer's own admission, the same its keyboard uses.
pub trait Controller {
    type Error: ErrorCode + From<control::Error>;
    /// The closed action table; `check` holds it to the grammar.
    fn bindings(&self) -> &'static [Binding];
    /// Apply one named action with the fields the request carried.
    fn action(&mut self, name: &str, arguments: &[&str]) -> Result<Outcome, Self::Error>;
    /// Deliver one input through the consumer's own key and pointer paths.
    fn input(&mut self, input: Input<'_>) -> Result<Outcome, Self::Error>;
    /// The body of `state`: the consumer's tab-separated facts.
    fn state(&self) -> Result<String, Self::Error>;
    /// One reading of what the window shows now, for `text`, `frame` and
    /// `frame-page`: `view` runs once over the composition. A consumer
    /// whose scene borrows its model builds the scene here, per request,
    /// and may fail with its own error.
    fn compose<R>(&self, view: impl FnOnce(&dyn Composition) -> R) -> Result<R, Self::Error>;
    /// A verb outside the generic set; unknown verbs are `protocol`.
    fn request(&mut self, name: &str, arguments: &[&str]) -> Result<String, Self::Error> {
        let _ = (name, arguments);
        Err(control::Error::Protocol.into())
    }
}

/// The table's grammar, held once in a consumer's tests: names in the code
/// grammar and unique, chords unique and within the key bound, argument
/// shapes and help lines printable ASCII without tabs, help present.
pub fn check(bindings: &[Binding]) -> Result<(), String> {
    for (index, binding) in bindings.iter().enumerate() {
        if !control::valid_code(binding.name) {
            return Err(format!(
                "action name `{}` is outside the code grammar",
                binding.name
            ));
        }
        if binding.help.is_empty() {
            return Err(format!("action `{}` has no help line", binding.name));
        }
        for (what, text) in [("arguments", binding.arguments), ("help", binding.help)] {
            if !text.bytes().all(|b| (b' '..=b'~').contains(&b)) {
                return Err(format!(
                    "action `{}` {what} is not printable ASCII",
                    binding.name
                ));
            }
        }
        if let Some(chord) = binding.chord {
            if chord.is_empty() || chord.len() > KEY_BYTES || chord.chars().any(char::is_control) {
                return Err(format!(
                    "action `{}` chord `{chord}` is outside the key bound",
                    binding.name
                ));
            }
        }
        for earlier in bindings.get(..index).unwrap_or(&[]) {
            if earlier.name == binding.name {
                return Err(format!("action name `{}` is bound twice", binding.name));
            }
            if binding.chord.is_some() && earlier.chord == binding.chord {
                return Err(format!(
                    "chord `{}` binds both `{}` and `{}`",
                    binding.chord.unwrap_or(""),
                    earlier.name,
                    binding.name
                ));
            }
        }
    }
    Ok(())
}

/// The action a chord is bound to, if any.
pub fn bound<'a>(bindings: &'a [Binding], chord: &str) -> Option<&'a Binding> {
    bindings.iter().find(|binding| binding.chord == Some(chord))
}

/// The table as `--help actions` prints it: one aligned row per action.
pub fn help(bindings: &[Binding]) -> String {
    let name_width = bindings.iter().map(|b| b.name.len()).max().unwrap_or(0);
    let chord_width = bindings
        .iter()
        .map(|b| b.chord.unwrap_or("-").chars().count())
        .max()
        .unwrap_or(0);
    let argument_width = bindings
        .iter()
        .map(|b| {
            if b.arguments.is_empty() {
                1
            } else {
                b.arguments.len()
            }
        })
        .max()
        .unwrap_or(0);
    let mut out = String::new();
    for binding in bindings {
        let arguments = if binding.arguments.is_empty() {
            "-"
        } else {
            binding.arguments
        };
        out.push_str(&format!(
            "{:name_width$}  {:chord_width$}  {:argument_width$}  {}\n",
            binding.name,
            binding.chord.unwrap_or("-"),
            arguments,
            binding.help
        ));
    }
    out
}

/// One request over the generic verbs, answered as `control` frames it.
/// A consumer's replay and socket both hand their payloads here.
pub fn request<C: Controller>(controller: &mut C, input: &[u8]) -> String {
    let Envelope { id, name, args } = match envelope::<C::Error>(input) {
        Ok(envelope) => envelope,
        Err(refusal) => return refusal.response(),
    };
    // Never collect an unbounded number of fields from an untrusted frame.
    let arguments: Vec<&str> = args.take(ARGUMENTS + 1).collect();
    let result = if arguments.len() > ARGUMENTS {
        Err(control::Error::Protocol.into())
    } else {
        dispatch(controller, name, &arguments)
    };
    match result {
        Ok(body) => ok(id, &body),
        Err(error) => Refusal { id, error }.response(),
    }
}

/// A request as the bounded worker hands it to a consumer's turn. The
/// envelope and the field count are judged on the worker's thread, so a
/// malformed frame is refused there without the consumer, as the
/// transport promises; the bytes reach the turn, where `request` judges
/// the verb and its fields against the consumer. `Worker<Payload>` is the
/// socket wiring of a driven consumer.
pub struct Payload {
    bytes: Vec<u8>,
}

impl Payload {
    /// The validated request line, for `request`.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

impl Parse for Payload {
    type Error = control::Error;
    fn parse(payload: &[u8]) -> Result<Self, Refusal<control::Error>> {
        let Envelope { id, args, .. } = envelope::<control::Error>(payload)?;
        if args.take(ARGUMENTS + 1).count() > ARGUMENTS {
            return Err(Refusal {
                id,
                error: control::Error::Protocol,
            });
        }
        Ok(Self {
            bytes: payload.to_vec(),
        })
    }
}

fn dispatch<C: Controller>(
    controller: &mut C,
    name: &str,
    arguments: &[&str],
) -> Result<String, C::Error> {
    use control::Error::Protocol;
    Ok(match (name, arguments) {
        ("state", []) => controller.state()?,
        ("actions", []) => actions(controller.bindings()),
        ("action", [action, rest @ ..]) => {
            if !controller.bindings().iter().any(|b| b.name == *action) {
                return Err(Protocol.into());
            }
            controller.action(action, rest)?.word().into()
        }
        ("key", [chord]) => {
            // The bound is checked on the hex too, so a bad chord decodes
            // at most 32 bytes rather than half a frame.
            if chord.len() > KEY_BYTES * 2 {
                return Err(Protocol.into());
            }
            let chord = String::from_utf8(unhex(chord)?).map_err(|_| Protocol)?;
            if chord.is_empty() || chord.len() > KEY_BYTES || chord.chars().any(char::is_control) {
                return Err(Protocol.into());
            }
            controller
                .input(Input::Key { chord: &chord })?
                .word()
                .into()
        }
        ("pointer", [phase, x, y]) => {
            let phase = match *phase {
                "press" => PointerPhase::Press,
                "move" => PointerPhase::Move,
                "release" => PointerPhase::Release,
                _ => return Err(Protocol.into()),
            };
            let x = u32::try_from(decimal(x)?).map_err(|_| Protocol)?;
            let y = u32::try_from(decimal(y)?).map_err(|_| Protocol)?;
            controller
                .input(Input::Pointer { phase, x, y })?
                .word()
                .into()
        }
        ("wheel", [rows, columns]) => controller
            .input(Input::Wheel {
                rows: signed(rows)?,
                columns: signed(columns)?,
            })?
            .word()
            .into(),
        ("resize", [width, height, scale]) => {
            let width = size(width)?;
            let height = size(height)?;
            let scale = u8::try_from(decimal(scale)?).map_err(|_| Protocol)?;
            // The raster's ceilings, before the consumer lays anything out.
            Surface::new(width, height, Scale::new(scale).map_err(wire)?).map_err(wire)?;
            controller
                .input(Input::Resize {
                    width,
                    height,
                    scale,
                })?
                .word()
                .into()
        }
        ("focus", [focused]) => controller
            .input(Input::Focus(control::boolean(focused)?))?
            .word()
            .into(),
        ("tick", [now]) => controller.input(Input::Tick(decimal(now)?))?.word().into(),
        ("text", []) => {
            let (rows, columns, text) = controller.compose(text)?.map_err(wire)?;
            format!("{rows}\t{columns}\t{}", hex(text.as_bytes()))
        }
        ("frame", []) => {
            let frame = controller.compose(paint)?.map_err(wire)?;
            format!(
                "{}\t{}\t{}\t{:016x}",
                frame.surface.width,
                frame.surface.height,
                frame.surface.scale.value(),
                fnv1a64(&frame.rgb)
            )
        }
        ("frame-page", [offset, limit]) => {
            let offset = size(offset)?;
            let limit = size(limit)?;
            if limit == 0 {
                return Err(Protocol.into());
            }
            if limit > PAGE_BYTES {
                return Err(control::Error::Limit.into());
            }
            let frame = controller.compose(paint)?.map_err(wire)?;
            let end = offset.saturating_add(limit).min(frame.rgb.len());
            let page = frame.rgb.get(offset..end).ok_or(Protocol)?;
            format!(
                "{}\t{}\t{offset}\t{}",
                frame.surface.width,
                frame.surface.height,
                hex(page)
            )
        }
        (verb, _) if VERBS.contains(&verb) => return Err(Protocol.into()),
        _ => controller.request(name, arguments)?,
    })
}

/// `N` then, per action, its name and, each in hex with `-` for none, its
/// chord, argument shape and help line. Hex keeps a literal `-` chord
/// apart from absence and whatever a table holds off the ASCII line;
/// `check` is the consumer's pin on the name.
fn actions(bindings: &[Binding]) -> String {
    let mut out = bindings.len().to_string();
    for binding in bindings {
        out.push('\t');
        out.push_str(binding.name);
        out.push('\t');
        out.push_str(
            &binding
                .chord
                .map_or("-".into(), |chord| hex(chord.as_bytes())),
        );
        out.push('\t');
        out.push_str(&hex(binding.arguments.as_bytes()));
        out.push('\t');
        out.push_str(&hex(binding.help.as_bytes()));
    }
    out
}

fn signed(value: &str) -> control::Result<i32> {
    let digits = value.strip_prefix('-').unwrap_or(value);
    let magnitude = i32::try_from(decimal(digits)?).map_err(|_| control::Error::Protocol)?;
    if magnitude > WHEEL_LIMIT {
        return Err(control::Error::Protocol);
    }
    Ok(if digits.len() != value.len() {
        -magnitude
    } else {
        magnitude
    })
}

/// The raster's refusals on the wire: an argument outside its contract is
/// `protocol`, a size past a ceiling `limit`.
fn wire(error: raster::Error) -> control::Error {
    match error {
        raster::Error::InvalidArgument => control::Error::Protocol,
        raster::Error::Limit => control::Error::Limit,
    }
}

/// The text a composition shows, read from its draw stream onto the
/// surface's cell grid at its scale: every glyph lands in the cell its
/// origin names (an origin off the grid, a glyph straddling the top or
/// left edge included, names none), later draws over earlier ones, a fill
/// clears every cell it wholly covers, wholly clipped glyphs are absent,
/// rows are trimmed on the right and trailing blank rows dropped. Returns
/// the grid's rows and columns and the text, of at most that many lines.
pub fn text(composition: &dyn Composition) -> Result<(usize, usize, String), raster::Error> {
    let surface = composition.surface();
    surface.check()?;
    let scale = surface.scale.value();
    let cell_width = CELL_WIDTH.saturating_mul(scale);
    let cell_height = CELL_HEIGHT.saturating_mul(scale);
    let columns = surface.width / cell_width;
    let rows = surface.height / cell_height;
    let (Ok(width), Ok(height)) = (u32::try_from(cell_width), u32::try_from(cell_height)) else {
        return Err(raster::Error::Limit);
    };
    let mut grid = vec![' '; rows.saturating_mul(columns)];
    composition.emit(surface.bounds(), &mut |draw| {
        let (x, y, scalar) = match draw.primitive {
            Primitive::Glyph { x, y, scalar, .. } => (x, y, scalar),
            Primitive::Fill { rect, .. } => {
                // Opaque paint: whatever it wholly covers is no longer shown.
                let Some(covered) = draw.clip.intersection(rect) else {
                    return;
                };
                let span = covered_cells(covered.x, covered.width, cell_width, columns);
                for row in covered_cells(covered.y, covered.height, cell_height, rows) {
                    for column in span.clone() {
                        let index = row.saturating_mul(columns).saturating_add(column);
                        if let Some(slot) = grid.get_mut(index) {
                            *slot = ' ';
                        }
                    }
                }
                return;
            }
        };
        let cell = Rect {
            x,
            y,
            width,
            height,
        };
        if draw.clip.intersection(cell).is_none() {
            return;
        }
        let (Ok(x), Ok(y)) = (usize::try_from(x), usize::try_from(y)) else {
            return;
        };
        let (column, row) = (x / cell_width, y / cell_height);
        if column < columns && row < rows {
            if let Some(slot) = grid.get_mut(row.saturating_mul(columns).saturating_add(column)) {
                *slot = scalar;
            }
        }
    });
    let mut lines: Vec<String> = grid
        .chunks(columns.max(1))
        .take(rows)
        .map(|row| row.iter().collect::<String>().trim_end().to_string())
        .collect();
    while lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    Ok((rows, columns, lines.join("\n")))
}

/// The cells along one axis that a span from `start` for `length` pixels
/// wholly covers, on a grid of `count` cells each `cell` pixels long.
fn covered_cells(start: i64, length: u32, cell: usize, count: usize) -> std::ops::Range<usize> {
    let cell = i64::try_from(cell).unwrap_or(i64::MAX).max(1);
    let end = start.saturating_add(i64::from(length)).max(0);
    let first = start.max(0).saturating_add(cell - 1) / cell;
    let first = usize::try_from(first).unwrap_or(usize::MAX).min(count);
    let last = usize::try_from(end / cell).unwrap_or(usize::MAX).min(count);
    first..last.max(first)
}

/// One painted frame: its surface and its pixels as tight RGB rows.
pub struct Frame {
    pub surface: Surface,
    pub rgb: Vec<u8>,
}

impl Frame {
    /// The frame as a binary PPM.
    pub fn ppm(&self) -> Vec<u8> {
        raster::ppm(self.surface, &self.rgb)
    }
}

/// The embedded face, parsed once per process, for every frame the seam
/// paints.
fn face() -> Result<&'static Font, raster::Error> {
    static FACE: OnceLock<Option<Font>> = OnceLock::new();
    FACE.get_or_init(|| crate::font::pinned().ok())
        .as_ref()
        // The face is a compile-time constant. A build whose face does not
        // parse cannot paint at all, which is the seam's limit, not a fault
        // of the request.
        .ok_or(raster::Error::Limit)
}

/// Paints the composition into a fresh buffer through the pinned face.
pub fn paint(composition: &dyn Composition) -> Result<Frame, raster::Error> {
    let surface = composition.surface();
    surface.check()?;
    let font = face()?;
    let stride = surface.width.saturating_mul(4);
    let mut pixels = vec![0u8; stride.saturating_mul(surface.height)];
    Raster::new(&mut pixels, font, surface, stride)?.paint(composition, surface.bounds())?;
    Ok(Frame {
        surface,
        rgb: raster::rgb(&pixels, surface, stride)?,
    })
}

/// FNV-1a over bytes: a cheap equality witness for frames, not a hash for
/// anything adversarial.
pub fn fnv1a64(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
    })
}

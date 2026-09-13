//! The installer's first page. It is a `Composition` over a `Surface`,
//! built from `td_ui::chrome` and the shared raster: a chrome ground, a
//! heading over a hairline rule, the wrapped disclosure prose in a `Block`,
//! and a `Status` footer naming the step. It holds no state and reads
//! nothing but its surface; the wizard drives it.
//!
//! INSTALLER.md requires the welcome screen to disclose that storage is
//! unencrypted and that the account signs in automatically, and forbids a
//! PIN or password field here. The prose below states those facts and the
//! page draws no text entry, so a reader is told before any disk is
//! touched.

use td_ui::chrome::{Block, Status, ROW};
use td_ui::raster::{
    text_run, Composition, Draw, GlyphStyle, Primitive, Rect, Surface, BORDER, CHROME, INK,
};
use td_ui::{CELL_HEIGHT, CELL_WIDTH};

/// The heading at the top of the page.
pub const HEADING: &str = "Install td";

/// The disclosure paragraphs, wrapped to the surface at construction. They
/// state the two facts INSTALLER.md requires the welcome screen to
/// disclose: storage is not encrypted, and the account signs in
/// automatically with no password or PIN.
pub const BODY: [&str; 3] = [
    "This installs td onto one whole disk that you choose. Everything now on \
     that disk is erased.",
    "Storage on this system is not encrypted: anyone who has the disk can \
     read its files. The account you create signs in automatically when the \
     machine starts, with no password or PIN.",
    "Next you choose the disk, then a username, hostname, keyboard layout \
     and time zone. Networking is set up after the installed system starts. \
     No password is needed to install.",
];

/// The status-row footer naming this step of the wizard. INSTALLER.md's
/// sequence is welcome, destination disk, account and regional settings,
/// review, installation progress, completion: six steps.
pub const FOOTER: &str = "Welcome \u{b7} step 1 of 6";

/// The left and right text margin, in reference pixels: one cell, matching
/// the chrome bands' inset.
const INSET_X: usize = CELL_WIDTH;
/// The heading's top, the rule beneath it, and the body's top, each a
/// chrome row below the last.
const HEADING_Y: usize = ROW;
const RULE_Y: usize = 2 * ROW;
const BODY_Y: usize = 3 * ROW;

/// Word-wraps `paragraphs` to `columns` cells, one blank line between
/// paragraphs, so the result drops straight into a `Block` without the
/// block re-wrapping. A word longer than `columns` is hard-broken rather
/// than overrun. Every line is at most `columns` characters.
pub fn wrap(paragraphs: &[&str], columns: usize) -> Vec<String> {
    let columns = columns.max(1);
    let mut lines = Vec::new();
    for (index, paragraph) in paragraphs.iter().enumerate() {
        if index > 0 {
            lines.push(String::new());
        }
        let start = lines.len();
        wrap_paragraph(paragraph, columns, &mut lines);
        if lines.len() == start {
            lines.push(String::new());
        }
    }
    lines
}

fn wrap_paragraph(paragraph: &str, columns: usize, lines: &mut Vec<String>) {
    let mut line = String::new();
    let mut length = 0;
    for word in paragraph.split_whitespace() {
        let word_length = word.chars().count();
        if word_length > columns {
            if length > 0 {
                lines.push(std::mem::take(&mut line));
                length = 0;
            }
            for chunk in chunks(word, columns) {
                lines.push(chunk);
            }
            if let Some(last) = lines.pop() {
                length = last.chars().count();
                line = last;
            }
            continue;
        }
        let separator = usize::from(length > 0);
        if length + separator + word_length > columns {
            lines.push(std::mem::take(&mut line));
            line.push_str(word);
            length = word_length;
        } else {
            if separator == 1 {
                line.push(' ');
            }
            line.push_str(word);
            length += separator + word_length;
        }
    }
    if length > 0 {
        lines.push(line);
    }
}

/// Splits `word` into pieces of at most `columns` characters.
fn chunks(word: &str, columns: usize) -> Vec<String> {
    let columns = columns.max(1);
    let mut pieces = Vec::new();
    let mut piece = String::new();
    let mut length = 0;
    for character in word.chars() {
        if length == columns {
            pieces.push(std::mem::take(&mut piece));
            length = 0;
        }
        piece.push(character);
        length += 1;
    }
    if length > 0 {
        pieces.push(piece);
    }
    pieces
}

/// The installer's welcome page over one surface: the wrapped disclosure
/// prose, the block that paints it and the status footer, all sized for the
/// surface at construction.
pub struct Welcome {
    surface: Surface,
    body: String,
    block: Block,
    footer: Status,
}

impl Welcome {
    /// Builds the page for `surface`, or `None` when the surface cannot
    /// hold the page: too narrow, so the prose wraps past a chrome block's
    /// rows, or too short, so the block would overlap the footer. A wider
    /// surface wraps the prose shorter, so the binding limit at the narrow
    /// end is the block's row budget, not a separate column floor.
    pub fn new(surface: Surface) -> Option<Self> {
        // A literal `Surface` can carry any field, so validate its axes as
        // `Raster::new` will, before deriving geometry that would truncate.
        surface.check().ok()?;
        let scale = surface.scale.value();
        let columns = surface.width.checked_sub(2 * CELL_WIDTH * scale)? / (CELL_WIDTH * scale);
        let lines = wrap(&BODY, columns);
        let block = Block::new(surface, (BODY_Y * scale) as i64, lines.len())?;
        let body_bottom = (BODY_Y + lines.len() * CELL_HEIGHT) * scale;
        let footer = Status::new(surface);
        if body_bottom as i64 > footer.rect().y {
            return None;
        }
        Some(Self {
            surface,
            body: lines.join("\n"),
            block,
            footer,
        })
    }

    /// The content width between the margins, for the heading's slots.
    fn content(&self) -> Rect {
        let scale = self.surface.scale.value();
        Rect {
            x: (INSET_X * scale) as i64,
            y: (HEADING_Y * scale) as i64,
            width: self.surface.width.saturating_sub(2 * INSET_X * scale) as u32,
            height: (CELL_HEIGHT * scale) as u32,
        }
    }
}

fn fill(rect: Rect, color: u32, damage: Rect, sink: &mut dyn FnMut(Draw)) {
    if let Some(area) = rect.intersection(damage) {
        sink(Draw {
            clip: area,
            primitive: Primitive::Fill { rect: area, color },
        });
    }
}

impl Composition for Welcome {
    fn surface(&self) -> Surface {
        self.surface
    }

    fn emit(&self, damage: Rect, sink: &mut dyn FnMut(Draw)) {
        let scale = self.surface.scale;
        let s = scale.value();
        // The chrome ground under everything, so no pixel is left unpainted.
        fill(self.surface.bounds(), CHROME, damage, sink);
        let heading = self.content();
        text_run(
            scale,
            HEADING.chars(),
            (heading.x, heading.y),
            heading,
            GlyphStyle::medium(INK, CHROME),
            damage,
            sink,
        );
        // A hairline rule separates the heading from the disclosures.
        fill(
            Rect {
                x: (INSET_X * s) as i64,
                y: (RULE_Y * s) as i64,
                width: self.surface.width.saturating_sub(2 * INSET_X * s) as u32,
                height: s as u32,
            },
            BORDER,
            damage,
            sink,
        );
        self.block.emit(&self.body, damage, sink);
        self.footer.emit(FOOTER.chars(), damage, sink);
    }
}

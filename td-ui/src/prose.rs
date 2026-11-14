//! Prose: text in proportional faces, each glyph its own width, laid
//! into lines by pixel width (td-ui/DESIGN.md, "Proportional faces").
//! `Faces` holds the proportional families' style bytes, and the
//! monospace face's when a program has it; `Metrics` measures with them
//! at a surface's scale, and answers the cell without them; `wrap` lays
//! styled runs into lines and `draw` streams a line's glyphs. A raster
//! given the same `Faces` (`Raster::with_prose`) draws what `Metrics`
//! measured, from the face `Metrics::source` names. Nothing here reads
//! the environment, a clock, a descriptor or the filesystem: the
//! consumer hands over the bytes (`pinned_face::prose_or_note` reads the
//! pinned ones).

use td_alloc::Budget;
mod cache;
mod items;
mod wrap;
pub use items::Items;
use std::ops::Range;
use std::sync::Arc;
pub use wrap::{wrap, wrap_at_most, Line, Piece, Placed};

use crate::atlas::Style;
use crate::face::Face;
use crate::font::stand_in;
use crate::raster::{Draw, GlyphStyle, Primitive, Rect, Scale, TextFamily};
use crate::sfnt::{Error, Font};
use crate::typeface::Typeface;
use crate::{CELL_HEIGHT, CELL_WIDTH};

/// The prose size in pixels per em at scale one; the device size is the
/// scale times it.
pub const SIZE: u16 = 16;
/// The bitmap face's baseline below its cell's top: Unifont's 16 rows hold
/// 14 above it and 2 below.
pub const CELL_BASELINE: usize = 14;
/// Subpixels per pixel in which advances are summed.
pub const UNIT: u32 = 64;
/// How far left of its pen a glyph's ink may hang and still be drawn by
/// `draw`: one em at the largest scale.
pub const OVERHANG: u32 = SIZE as u32 * MAX_SCALE as u32;
/// Measurements `Metrics` holds before it forgets them all and starts
/// again, so its memory is bounded whatever text it measures.
pub const MAX_MEASURES: usize = 8192;
/// The largest scale `Scale` admits, at which `Faces::new` checks a face
/// still sizes.
const MAX_SCALE: u8 = 4;

/// Which face draws a scalar: the cell (no prose faces, or a `Cell`
/// glyph), the prose family in its style or regular, the monospace face
/// (its stand-in included), or the bitmap face.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Source {
    Cell,
    Family,
    Mono,
    Bitmap,
}

/// One family's style bytes: the regular, then bold, italic and bold
/// italic as it has them.
#[derive(Clone, Debug)]
struct Styles {
    regular: Arc<[u8]>,
    others: [Option<Arc<[u8]>>; 3],
}

impl Styles {
    /// Every style parsed, and the face sized at the smallest and largest
    /// prose sizes, so a face that exists sizes at every scale.
    fn checked(regular: Arc<[u8]>, others: [Option<Arc<[u8]>>; 3]) -> Result<Self, (Style, Error)> {
        let styles = Self { regular, others };
        for style in [
            Style::Regular,
            Style::Bold,
            Style::Italic,
            Style::BoldItalic,
        ] {
            let bytes = match style {
                Style::Regular => Some(&styles.regular),
                _ => styles.bytes(style),
            };
            if let Some(bytes) = bytes {
                Font::parse(bytes).map_err(|why| (style, why))?;
            }
        }
        for scale in [1, MAX_SCALE] {
            let scale = Scale::new(scale).map_err(|_| (Style::Regular, Error::Limit("scale")))?;
            styles
                .face(size(scale))
                .map_err(|why| (Style::Regular, why))?;
        }
        Ok(styles)
    }

    /// A style's bytes other than the regular one's.
    fn bytes(&self, style: Style) -> Option<&Arc<[u8]>> {
        let [bold, italic, bold_italic] = &self.others;
        match style {
            Style::Regular => None,
            Style::Bold => bold.as_ref(),
            Style::Italic => italic.as_ref(),
            Style::BoldItalic => bold_italic.as_ref(),
        }
    }

    /// The style a glyph is drawn in, as `Face::style` resolves it.
    fn resolve(&self, style: Style) -> Style {
        let bold = matches!(style, Style::Bold | Style::BoldItalic);
        let italic = matches!(style, Style::Italic | Style::BoldItalic);
        [
            (bold && italic, Style::BoldItalic),
            (italic, Style::Italic),
            (bold, Style::Bold),
        ]
        .into_iter()
        .find(|&(asked, style)| asked && self.bytes(style).is_some())
        .map_or(Style::Regular, |(_, style)| style)
    }

    /// The fonts a glyph in `style` is looked for in, in order, as
    /// `Face::glyph` looks: the resolved style, then the regular.
    fn fonts(&self, style: Style) -> impl Iterator<Item = Font<'_>> {
        [self.bytes(self.resolve(style)), Some(&self.regular)]
            .into_iter()
            .flatten()
            .filter_map(|bytes| Font::parse(bytes).ok())
    }

    /// Every style as one face at `size` pixels per em.
    fn face(&self, size: f32) -> Result<Face, Error> {
        let [bold, italic, bold_italic] = &self.others;
        Face::sized(self.regular.clone(), bold.clone(), size)?
            .with_slant(italic.clone(), bold_italic.clone())
    }
}

/// The prose size at `scale`, in pixels per em.
fn size(scale: Scale) -> f32 {
    f32::from(SIZE) * scale.value() as f32
}

#[derive(Clone, Debug)]
struct Bytes {
    sans: Styles,
    serif: Styles,
    mono: Option<Styles>,
}

/// A family's style `Faces::new` refused, and why.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Refused {
    pub family: TextFamily,
    pub style: Style,
    pub error: Error,
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?} {:?}: {}", self.family, self.style, self.error)
    }
}

impl std::error::Error for Refused {}

/// The prose faces: Sans and Serif in four styles each, and the monospace
/// face's styles when a program has them (`with_mono`), held once and
/// shared. A raster draws through the faces sized at its scale, made on
/// first use and remade when the scale changes; a clone shares the bytes
/// and sizes its own.
#[derive(Debug)]
pub struct Faces {
    bytes: Bytes,
    /// The faces at the scale last asked for, or their refusal.
    at: Option<(usize, Option<AtSize>)>,
}

impl Clone for Faces {
    fn clone(&self) -> Self {
        Self {
            bytes: self.bytes.clone(),
            at: None,
        }
    }
}

impl Faces {
    /// Sans and Serif, each its regular, bold, italic and bold italic
    /// styles. A style that does not parse, or a family that does not
    /// size at the prose size at every scale, is refused by name.
    pub fn new(sans: [Vec<u8>; 4], serif: [Vec<u8>; 4]) -> Result<Self, Refused> {
        let family = |family: TextFamily, [regular, bold, italic, bold_italic]: [Vec<u8>; 4]| {
            Styles::checked(
                regular.into(),
                [
                    Some(bold.into()),
                    Some(italic.into()),
                    Some(bold_italic.into()),
                ],
            )
            .map_err(|(style, error)| Refused {
                family,
                style,
                error,
            })
        };
        Ok(Self {
            bytes: Bytes {
                sans: family(TextFamily::Sans, sans)?,
                serif: family(TextFamily::Serif, serif)?,
                mono: None,
            },
            at: None,
        })
    }

    /// These faces with the monospace face's styles, shared with
    /// `typeface`: the `Mono` family's outlines, and the first fallback of
    /// a scalar Sans or Serif lacks. A style that does not parse, or a face
    /// that does not size at the prose size at every scale, is refused
    /// with the reader's error.
    pub fn with_mono(mut self, typeface: &Typeface) -> Result<Self, Error> {
        let (regular, others) = typeface.styles();
        self.bytes.mono = Some(Styles::checked(regular, others).map_err(|(_, why)| why)?);
        self.at = None;
        Ok(self)
    }

    /// Whether the monospace face is among them.
    pub fn has_mono(&self) -> bool {
        self.bytes.mono.is_some()
    }

    /// The faces sized at `scale`, made when the scale changes; none only
    /// if one is refused there, which `new`'s checks rule out.
    pub(crate) fn at(&mut self, scale: Scale) -> Option<&mut AtSize> {
        let value = scale.value();
        if self.at.as_ref().map(|(at, _)| *at) != Some(value) {
            self.at = Some((value, AtSize::new(&self.bytes, scale).ok()));
        }
        self.at.as_mut().and_then(|(_, at)| at.as_mut())
    }
}

/// The faces at one prose size, each with its own atlas page, and the
/// metrics that pick which draws a scalar.
#[derive(Debug)]
pub(crate) struct AtSize {
    pub(crate) metrics: Metrics,
    sans: Face,
    serif: Face,
    mono: Option<Face>,
}

impl AtSize {
    fn new(bytes: &Bytes, scale: Scale) -> Result<Self, Error> {
        let size = size(scale);
        Ok(Self {
            metrics: Metrics::of(Some(bytes.clone()), scale),
            sans: bytes.sans.face(size)?,
            serif: bytes.serif.face(size)?,
            mono: bytes
                .mono
                .as_ref()
                .map(|mono| mono.face(size))
                .transpose()?,
        })
    }

    /// The face `source` names for a glyph of `family`.
    pub(crate) fn face(&mut self, source: Source, family: TextFamily) -> Option<&mut Face> {
        match (source, family) {
            (Source::Family, TextFamily::Sans) => Some(&mut self.sans),
            (Source::Family, TextFamily::Serif) => Some(&mut self.serif),
            (Source::Mono, _) => self.mono.as_mut(),
            _ => None,
        }
    }
}

/// A prose line's box: its height and its baseline below its top, in
/// pixels.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LineBox {
    pub height: u32,
    pub baseline: u32,
}

/// What a prose consumer measures with at one scale: each scalar's
/// advance in `UNIT`s of a pixel, from the face that draws it, and the
/// line box. Built from the faces a raster draws with, so measuring and
/// drawing agree; without them it answers the cell.
#[derive(Clone, Debug)]
pub struct Metrics {
    scale: Scale,
    bytes: Option<Bytes>,
    line: LineBox,
    /// The monospace face's one advance at the prose size, in `UNIT`s.
    mono_advance: u32,
    measures: cache::Cache,
}

impl Metrics {
    /// The metrics of `faces` at `scale`, or of the cell without them.
    pub fn new(faces: Option<&Faces>, scale: Scale) -> Self {
        Self::of(faces.map(|faces| faces.bytes.clone()), scale)
    }

    /// Measures using a caller's shared budget for the optional cache.
    /// Cache admission failure falls back to the same uncached measurements.
    pub fn new_in(faces: Option<&Faces>, scale: Scale, budget: &Budget) -> Self {
        let mut out = Self::new(faces, scale);
        out.measures = cache::Cache::new(budget);
        out
    }

    fn of(bytes: Option<Bytes>, scale: Scale) -> Self {
        let size = i64::from(SIZE) * scale.value() as i64;
        let cell = LineBox {
            height: (CELL_HEIGHT * scale.value()) as u32,
            baseline: (CELL_BASELINE * scale.value()) as u32,
        };
        // The line holds both families: from the higher of their regular
        // styles' ascenders to the lower of their descenders, each in
        // `UNIT`s, rounded once; Mono takes the line's.
        let line = bytes.as_ref().and_then(|bytes| {
            [&bytes.sans, &bytes.serif]
                .into_iter()
                .filter_map(|styles| Font::parse(&styles.regular).ok())
                .map(|font| {
                    let upem = i64::from(font.units_per_em());
                    let units = |metric: i16| {
                        round_div(i64::from(metric) * size * i64::from(UNIT), upem).max(0)
                    };
                    (
                        units(font.ascender()),
                        units(font.descender().saturating_neg()),
                    )
                })
                .reduce(|(a, d), (b, e)| (a.max(b), d.max(e)))
                .map(|(ascent, descent)| {
                    let pixels = |units: i64| pixels(u64::try_from(units).unwrap_or(0));
                    LineBox {
                        height: pixels(ascent.saturating_add(descent)).max(1),
                        baseline: pixels(ascent),
                    }
                })
        });
        let mono_advance = bytes
            .as_ref()
            .and_then(|bytes| bytes.mono.as_ref())
            .and_then(|mono| Font::parse(&mono.regular).ok())
            .and_then(|font| {
                ['0', 'M'].into_iter().find_map(|scalar| {
                    let units = font.advance(font.glyph(scalar)?)?;
                    Some(units_at(units, font.units_per_em(), size))
                })
            })
            .unwrap_or(0);
        Self {
            scale,
            bytes: line.and(bytes),
            line: line.unwrap_or(cell),
            mono_advance,
            measures: cache::Cache::default(),
        }
    }

    pub fn scale(&self) -> Scale {
        self.scale
    }

    /// Whether it measures proportional faces rather than the cell.
    pub fn proportional(&self) -> bool {
        self.bytes.is_some()
    }

    /// The line box: the cell's, `CELL_HEIGHT` at the scale with the
    /// bitmap face's baseline, without proportional faces.
    pub fn line(&self) -> LineBox {
        self.line
    }

    /// `scalar`'s advance in `family` and `style`, in `UNIT`s of a pixel:
    /// that of the face that draws it, `CELL_WIDTH` at the scale for the
    /// cell and the bitmap face.
    pub fn advance(&mut self, family: TextFamily, style: Style, scalar: char) -> u32 {
        self.measure(family, style, scalar).1
    }

    /// Which face draws `scalar` in `family` and `style`: the family's
    /// style, else its regular; else the monospace face in its style or
    /// regular, or its stand-in; else the bitmap face. `Mono` starts at
    /// the monospace face; a `Cell` glyph, or any without proportional
    /// faces, is the cell's.
    pub fn source(&mut self, family: TextFamily, style: Style, scalar: char) -> Source {
        self.measure(family, style, scalar).0
    }

    /// The width in pixels of `text` set in one run from a pen at zero:
    /// its advances summed in `UNIT`s, then rounded.
    pub fn width(&mut self, family: TextFamily, style: Style, text: &str) -> u32 {
        let total = text.chars().fold(0u64, |total, scalar| {
            total.saturating_add(u64::from(self.advance(family, style, shown(scalar))))
        });
        pixels(total)
    }

    fn measure(&mut self, family: TextFamily, style: Style, scalar: char) -> (Source, u32) {
        let cell = (CELL_WIDTH * self.scale.value()) as u32 * UNIT;
        let Some(bytes) = &self.bytes else {
            return (Source::Cell, cell);
        };
        let styles = match family {
            TextFamily::Cell => return (Source::Cell, cell),
            TextFamily::Sans => Some(&bytes.sans),
            TextFamily::Serif => Some(&bytes.serif),
            TextFamily::Mono => None,
        };
        let key = (family, style, scalar);
        if let Some(measured) = self.measures.get(key) {
            return measured;
        }
        let size = i64::from(SIZE) * self.scale.value() as i64;
        let in_family = styles.and_then(|styles| {
            styles.fonts(style).find_map(|font| {
                let units = font.advance(font.glyph(scalar)?)?;
                Some(units_at(units, font.units_per_em(), size))
            })
        });
        let in_mono = || {
            bytes.mono.as_ref().is_some_and(|mono| {
                [Some(scalar), stand_in(scalar)]
                    .into_iter()
                    .flatten()
                    .any(|wanted| mono.fonts(style).any(|font| font.glyph(wanted).is_some()))
            })
        };
        let measured = match in_family {
            Some(advance) => (Source::Family, advance),
            None if in_mono() => (Source::Mono, self.mono_advance),
            None => (Source::Bitmap, cell),
        };
        self.measures.insert(key, measured);
        measured
    }
}

/// `numerator / denominator` rounded half up, for a positive denominator.
fn round_div(numerator: i64, denominator: i64) -> i64 {
    numerator
        .saturating_mul(2)
        .saturating_add(denominator)
        .div_euclid(denominator.max(1).saturating_mul(2))
}

/// `units` of a face with `upem` units per em at `size` pixels per em, in
/// `UNIT`s of a pixel, rounded.
fn units_at(units: u16, upem: u16, size: i64) -> u32 {
    let scaled = i64::from(units) * size * i64::from(UNIT);
    round_div(scaled, i64::from(upem)).clamp(0, i64::from(u32::MAX)) as u32
}

/// `UNIT`s rounded to the nearest pixel.
fn pixels(units: u64) -> u32 {
    u32::try_from(units.saturating_add(u64::from(UNIT / 2)) / u64::from(UNIT)).unwrap_or(u32::MAX)
}

/// The scalar prose shows for `scalar`: a tab is a space, another control
/// the replacement character, as a text run draws one.
pub fn shown(scalar: char) -> char {
    match scalar {
        '\t' => ' ',
        scalar if scalar.is_control() => '\u{fffd}',
        scalar => scalar,
    }
}

/// A piece of styled text handed to `wrap`: its glyphs' style, family
/// included, and a tag of the caller's that comes back on every piece of
/// it (a link's index, say).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Run<'a> {
    pub text: &'a str,
    pub style: GlyphStyle,
    pub tag: usize,
}

/// Streams `line`'s glyphs with its left at `x` and its top at `y`, each
/// in its piece's style, inside `bounds` and clipped to `damage`; nothing
/// when the two do not meet, and no glyph whose pen is `OVERHANG` or more
/// past the clip's right, so a glyph whose ink hangs left of its pen
/// still draws its part inside the clip and a repaint of part of a line
/// equals the whole line's inside its damage.
pub fn draw(
    line: &Line,
    (x, y): (i64, i64),
    bounds: Rect,
    damage: Rect,
    sink: &mut dyn FnMut(Draw),
) {
    let Some(clip) = bounds.intersection(damage) else {
        return;
    };
    let right = clip
        .x
        .saturating_add(i64::from(clip.width))
        .saturating_add(i64::from(OVERHANG));
    for piece in &line.pieces {
        for placed in line.glyphs.get(piece.glyphs.clone()).unwrap_or(&[]) {
            let pen = x.saturating_add(i64::from(placed.x));
            if pen >= right {
                return;
            }
            sink(Draw {
                clip,
                primitive: Primitive::Glyph {
                    x: pen,
                    y,
                    scalar: placed.scalar,
                    style: piece.style,
                },
            });
        }
    }
}

//! Looks: a text format of at most `MAX_OPERATIONS` operations applied to
//! linear sRGB after the pipeline's matrix (DESIGN.md, Looks), the built-in
//! set, and the per-pixel application. A curve is tabulated once, over a
//! log-spaced domain, so a pixel costs table loads and a few multiplies.
//! Pure: bytes in, a `Look` out; `main` reads the files.

use std::fmt;

use crate::color::{self, Matrix, LUMA, MIDDLE_GREY};

// The refusal messages below name these by value (a `&'static str` cannot
// be formatted): change the messages with the numbers.
/// A look file's ceiling.
pub const MAX_LOOK_BYTES: usize = 4096;
/// Operations in one look at most; `name` is not one.
pub const MAX_OPERATIONS: usize = 16;
/// Points in one `curve` at most.
pub const MAX_CURVE_POINTS: usize = 16;
/// A name's length at most, in characters.
pub const MAX_NAME: usize = 64;
/// The first line of every look.
pub const HEADER: &str = "td-photo look 1";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// Over `MAX_LOOK_BYTES`.
    Size,
    /// The first line is not `HEADER`.
    Header,
    /// The line, one-based, and why.
    Line(usize, &'static str),
}

const NOT_TEXT: &str = "not text (a control character or a byte that is not UTF-8)";

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Size => write!(f, "over {MAX_LOOK_BYTES} bytes"),
            Error::Header => write!(f, "first line is not `{HEADER}`"),
            Error::Line(number, why) => write!(f, "line {number}: {why}"),
        }
    }
}

impl std::error::Error for Error {}

/// What a `curve` applies to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Channel {
    Red,
    Green,
    Blue,
    /// Each of the three, the same way: what `tone` does.
    All,
    /// Rec. 709 luminance, the pixel scaled to the curve's answer.
    Luma,
}

impl Channel {
    fn parse(word: &str) -> Option<Channel> {
        Some(match word {
            "r" => Channel::Red,
            "g" => Channel::Green,
            "b" => Channel::Blue,
            "luma" => Channel::Luma,
            _ => return None,
        })
    }
}

/// A curve tabulated over a log-spaced domain: the float's exponent and
/// `MANTISSA_BITS` of its mantissa key `NODES` nodes from `2^-EXPONENTS`
/// to 1, one more at 1, linear between them and from 0 to the first.
#[derive(Clone, Debug, PartialEq)]
struct Table {
    zero: f32,
    nodes: Vec<f32>,
}

const EXPONENTS: u32 = 16;
const MANTISSA_BITS: u32 = 7;
const NODES: u32 = EXPONENTS << MANTISSA_BITS;
const MANTISSA_MASK: u32 = (1 << MANTISSA_BITS) - 1;
const FRACTION_BITS: u32 = 23 - MANTISSA_BITS;
const FRACTION_MASK: u32 = (1 << FRACTION_BITS) - 1;
const FIRST_EXPONENT: u32 = 127 - EXPONENTS;

/// The node's input: `2^(e - EXPONENTS) * (1 + m / 2^MANTISSA_BITS)`.
fn node_x(key: u32) -> f32 {
    let octave = i32::try_from(key >> MANTISSA_BITS).unwrap_or(0);
    let exponent = octave - i32::try_from(EXPONENTS).unwrap_or(0);
    let mantissa = (key & MANTISSA_MASK) as f32 / (1u32 << MANTISSA_BITS) as f32;
    2f32.powi(exponent) * (1.0 + mantissa)
}

/// One over the first node's input, `2^EXPONENTS`: the multiply that
/// puts an input under the first node on the ramp from zero to it.
const PER_FIRST_NODE: f32 = (1u32 << EXPONENTS) as f32;

impl Table {
    /// Tabulates `f`, or `None` when a node is not finite.
    fn build(f: impl Fn(f64) -> f64) -> Option<Table> {
        let nodes: Vec<f32> = (0..=NODES)
            .map(|key| f(f64::from(node_x(key))) as f32)
            .collect();
        let zero = f(0.0) as f32;
        (zero.is_finite() && nodes.iter().all(|v| v.is_finite())).then_some(Table { zero, nodes })
    }

    fn node(&self, key: u32) -> f32 {
        self.nodes.get(key as usize).copied().unwrap_or(self.zero)
    }

    /// The curve at `x`, clamped into `0..=1` first.
    fn at(&self, x: f32) -> f32 {
        let x = if x.is_nan() { 0.0 } else { x.clamp(0.0, 1.0) };
        if x >= 1.0 {
            return self.node(NODES);
        }
        let bits = x.to_bits();
        let exponent = (bits >> 23) & 0xff;
        if exponent < FIRST_EXPONENT {
            // Under the first node, which includes zero and subnormals:
            // linear from `zero`, one multiply, no call.
            let first = self.node(0);
            return self.zero + (first - self.zero) * (x * PER_FIRST_NODE);
        }
        let key = ((exponent - FIRST_EXPONENT) << MANTISSA_BITS)
            | ((bits >> FRACTION_BITS) & MANTISSA_MASK);
        let fraction = (bits & FRACTION_MASK) as f32 / (1u32 << FRACTION_BITS) as f32;
        let a = self.node(key);
        let b = self.node(key + 1);
        a + (b - a) * fraction
    }
}

#[derive(Clone, Debug, PartialEq)]
enum Op {
    Primaries(Matrix),
    Curve { channel: Channel, table: Table },
    Saturation(f32),
    Monochrome([f32; 3]),
}

/// A parsed look: its name, if it has one, and its operations in file
/// order.
#[derive(Clone, Debug, PartialEq)]
pub struct Look {
    name: Option<String>,
    ops: Vec<Op>,
}

/// `-?D+(.D+)?`, at most 24 characters, finite.
fn decimal(text: &str) -> Option<f32> {
    let digits = text.strip_prefix('-').unwrap_or(text);
    let (whole, part) = digits.split_once('.').unwrap_or((digits, "0"));
    let all_digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if text.len() > 24 || !all_digits(whole) || !all_digits(part) {
        return None;
    }
    text.parse::<f32>().ok().filter(|v| v.is_finite())
}

fn numbers<'a>(words: impl Iterator<Item = &'a str>) -> Option<Vec<f32>> {
    words.map(decimal).collect()
}

fn luma(rgb: [f32; 3]) -> f32 {
    LUMA[0] * rgb[0] + LUMA[1] * rgb[1] + LUMA[2] * rgb[2]
}

/// The tone sigmoid `y = (1 + k) x^c / (x^c + k)`, `k = g^c (1 - g) / (g -
/// g^c)` so that middle grey `g` and white are both fixed (a `c` of exactly
/// 1 is the identity, where `k` is undefined: the branch is `|g - g^c| <=
/// 1e-9`, which no other f32 contrast reaches; for `c` below 1 `k` is
/// negative and `x^c + k` stays negative over `0..=1`, since `-k > 1`
/// whenever `g^c < 1`, so the quotient is finite and increasing there
/// too); then the toe below grey,
/// `g (y / g)^p`, and the shoulder above it, `1 - (1 - g) u^q` with
/// `u = (1 - y) / (1 - g)`, the exponents easing quadratically from 1 at
/// grey to `2^toe` at black and `2^shoulder` at white (`1 + (2^t - 1) v^2`
/// for `v` the distance from grey as a share of the half). So a positive toe
/// deepens the shadows and is flat at black, a positive shoulder lifts the
/// highlights into white and is flat there, a negative one does the
/// reverse, all leave grey, black and white where they are, the join at
/// grey is smooth (slope 1 on both sides, whatever the two are), and each
/// half is monotone for the admitted range: the slope's sign is that of
/// `e / y + e' ln(y / g)`, and the exponent `e` is at least 0.5 while `v
/// (1 - v) |ln(1 - v)|` is at most 0.26.
fn tone(contrast: f32, toe: f32, shoulder: f32) -> impl Fn(f64) -> f64 {
    let g = f64::from(MIDDLE_GREY);
    let contrast = f64::from(contrast);
    let grey_c = g.powf(contrast);
    let k = ((g - grey_c).abs() > 1e-9).then(|| grey_c * (1.0 - g) / (g - grey_c));
    let toe_power = 2f64.powf(f64::from(toe));
    let shoulder_power = 2f64.powf(f64::from(shoulder));
    move |x: f64| {
        let x = x.clamp(0.0, 1.0);
        let y = match k {
            None => x,
            Some(k) => {
                let xc = x.powf(contrast);
                (1.0 + k) * xc / (xc + k)
            }
        };
        let y = y.clamp(0.0, 1.0);
        if y < g {
            let v = 1.0 - y / g;
            g * (y / g).powf(1.0 + (toe_power - 1.0) * v * v)
        } else {
            let u = (1.0 - y) / (1.0 - g);
            let w = 1.0 - u;
            1.0 - (1.0 - g) * u.powf(1.0 + (shoulder_power - 1.0) * w * w)
        }
    }
}

/// A monotone cubic through `points` (x strictly increasing), Fritsch and
/// Carlson's tangents, holding the end values outside the first and last.
/// In f64: the admitted knots can put a secant near 1e22 beside one near
/// 1e-22, whose tangent ratio is past f32 but within f64, where every
/// product below stays finite.
fn monotone_cubic(points: &[(f32, f32)]) -> impl Fn(f64) -> f64 {
    let points: Vec<(f64, f64)> = points
        .iter()
        .map(|&(x, y)| (f64::from(x), f64::from(y)))
        .collect();
    let n = points.len();
    let secant = |i: usize| -> f64 {
        match (points.get(i), points.get(i + 1)) {
            (Some(&(x0, y0)), Some(&(x1, y1))) if x1 > x0 => (y1 - y0) / (x1 - x0),
            _ => 0.0,
        }
    };
    let mut tangents: Vec<f64> = (0..n)
        .map(|i| {
            if i == 0 {
                secant(0)
            } else if i + 1 == n {
                secant(n.saturating_sub(2))
            } else {
                let (a, b) = (secant(i - 1), secant(i));
                if a * b <= 0.0 {
                    0.0
                } else {
                    (a + b) / 2.0
                }
            }
        })
        .collect();
    for i in 0..n.saturating_sub(1) {
        let d = secant(i);
        if d == 0.0 {
            for t in tangents.iter_mut().skip(i).take(2) {
                *t = 0.0;
            }
            continue;
        }
        let (alpha, beta) = (
            tangents.get(i).copied().unwrap_or(0.0) / d,
            tangents.get(i + 1).copied().unwrap_or(0.0) / d,
        );
        let norm = alpha * alpha + beta * beta;
        if norm > 9.0 {
            let tau = 3.0 / norm.sqrt();
            if let Some(t) = tangents.get_mut(i) {
                *t = tau * alpha * d;
            }
            if let Some(t) = tangents.get_mut(i + 1) {
                *t = tau * beta * d;
            }
        }
    }
    move |x: f64| {
        let (Some(&(x_first, y_first)), Some(&(x_last, y_last))) = (points.first(), points.last())
        else {
            return x;
        };
        if x <= x_first {
            return y_first;
        }
        if x >= x_last {
            return y_last;
        }
        // The segment holding x: the last point at or before it.
        let i = points.partition_point(|&(px, _)| px <= x).saturating_sub(1);
        let (Some(&(x0, y0)), Some(&(x1, y1)), Some(&m0), Some(&m1)) = (
            points.get(i),
            points.get(i + 1),
            tangents.get(i),
            tangents.get(i + 1),
        ) else {
            return y_last;
        };
        let h = x1 - x0;
        let t = (x - x0) / h;
        let (t2, t3) = (t * t, t * t * t);
        let h00 = 2.0 * t3 - 3.0 * t2 + 1.0;
        let h10 = t3 - 2.0 * t2 + t;
        let h01 = -2.0 * t3 + 3.0 * t2;
        let h11 = t3 - t2;
        (h00 * y0 + h10 * h * m0 + h01 * y1 + h11 * h * m1).clamp(0.0, 1.0)
    }
}

fn line_error<T>(number: usize, why: &'static str) -> Result<T, Error> {
    Err(Error::Line(number, why))
}

/// The one-based line the byte at `at` is on.
fn line_of(bytes: &[u8], at: usize) -> usize {
    bytes
        .get(..at)
        .map_or(0, |head| head.iter().filter(|b| **b == b'\n').count())
        + 1
}

impl Look {
    /// Parses a look file. Every refusal names its line.
    pub fn parse(bytes: &[u8]) -> Result<Look, Error> {
        if bytes.len() > MAX_LOOK_BYTES {
            return Err(Error::Size);
        }
        let text = match std::str::from_utf8(bytes) {
            Ok(text) => text,
            Err(e) => return line_error(line_of(bytes, e.valid_up_to()), NOT_TEXT),
        };
        if let Some((at, _)) = text
            .char_indices()
            .find(|(_, c)| c.is_control() && *c != '\n')
        {
            return line_error(line_of(bytes, at), NOT_TEXT);
        }
        let mut lines = text.strip_suffix('\n').unwrap_or(text).split('\n');
        if lines.next() != Some(HEADER) {
            return Err(Error::Header);
        }
        let mut look = Look {
            name: None,
            ops: Vec::new(),
        };
        for (index, line) in lines.enumerate() {
            let number = index + 2;
            let (word, rest) = line.split_once(' ').unwrap_or((line, ""));
            let op = match word {
                "" => return line_error(number, "blank or indented line"),
                "name" => {
                    let name = rest.trim();
                    if name.is_empty() || name.chars().count() > MAX_NAME {
                        return line_error(number, "name is 1 to 64 characters");
                    }
                    if look.name.is_some() {
                        return line_error(number, "a second name");
                    }
                    look.name = Some(name.to_string());
                    continue;
                }
                "primaries" => Self::primaries(number, rest)?,
                "tone" => Self::tone(number, rest)?,
                "curve" => Self::curve(number, rest)?,
                "saturation" => {
                    let Some(&[s]) = numbers(rest.split_whitespace()).as_deref() else {
                        return line_error(number, "saturation takes one number");
                    };
                    if !(0.0..=2.0).contains(&s) {
                        return line_error(number, "saturation is 0 to 2");
                    }
                    Op::Saturation(s)
                }
                "monochrome" => {
                    let Some(&[r, g, b]) = numbers(rest.split_whitespace()).as_deref() else {
                        return line_error(number, "monochrome takes three weights");
                    };
                    let sum = r + g + b;
                    if r < 0.0 || g < 0.0 || b < 0.0 || sum <= 0.0 {
                        return line_error(
                            number,
                            "monochrome weights are not negative and sum above zero",
                        );
                    }
                    Op::Monochrome([r / sum, g / sum, b / sum])
                }
                _ => return line_error(number, "unknown operation"),
            };
            if look.ops.len() == MAX_OPERATIONS {
                return line_error(number, "over 16 operations");
            }
            look.ops.push(op);
        }
        Ok(look)
    }

    fn primaries(number: usize, rest: &str) -> Result<Op, Error> {
        let Some(values) = numbers(rest.split_whitespace()) else {
            return line_error(number, "primaries takes nine numbers");
        };
        let &[a, b, c, d, e, f, g, h, i] = values.as_slice() else {
            return line_error(number, "primaries takes nine numbers");
        };
        let mut matrix: Matrix = [[a, b, c], [d, e, f], [g, h, i]];
        for row in matrix.iter_mut() {
            let sum: f32 = row.iter().sum();
            if sum <= 0.0 {
                return line_error(number, "primaries rows sum above zero");
            }
            for cell in row.iter_mut() {
                *cell /= sum;
            }
            // Checked after the division, so a row's gain on a channel is
            // at most 12 whatever scale it was written at (an overflowed
            // quotient is outside the range too).
            if row.iter().any(|v| !(-4.0..=4.0).contains(v)) {
                return line_error(
                    number,
                    "primaries entries are within -4 to 4 once the row sums to one",
                );
            }
        }
        Ok(Op::Primaries(matrix))
    }

    fn tone(number: usize, rest: &str) -> Result<Op, Error> {
        let words: Vec<&str> = rest.split_whitespace().collect();
        let (Some(&"contrast"), Some(&"toe"), Some(&"shoulder"), Some(c), Some(t), Some(s), None) = (
            words.first(),
            words.get(2),
            words.get(4),
            words.get(1).and_then(|w| decimal(w)),
            words.get(3).and_then(|w| decimal(w)),
            words.get(5).and_then(|w| decimal(w)),
            words.get(6),
        ) else {
            return line_error(number, "tone takes contrast C toe T shoulder S");
        };
        if !(0.5..=3.0).contains(&c) {
            return line_error(number, "contrast is 0.5 to 3");
        }
        if !(-1.0..=1.0).contains(&t) || !(-1.0..=1.0).contains(&s) {
            return line_error(number, "toe and shoulder are -1 to 1");
        }
        let Some(table) = Table::build(tone(c, t, s)) else {
            return line_error(number, "tone is not finite");
        };
        Ok(Op::Curve {
            channel: Channel::All,
            table,
        })
    }

    fn curve(number: usize, rest: &str) -> Result<Op, Error> {
        let mut words = rest.split_whitespace();
        let Some(channel) = words.next().and_then(Channel::parse) else {
            return line_error(number, "curve takes r, g, b or luma");
        };
        let Some(values) = numbers(words) else {
            return line_error(number, "curve takes pairs of numbers");
        };
        if values.len() % 2 != 0 || values.len() < 4 || values.len() > 2 * MAX_CURVE_POINTS {
            return line_error(number, "curve takes 2 to 16 points");
        }
        let points: Vec<(f32, f32)> = values
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&[x, y]| (x, y))
            .collect();
        if points
            .iter()
            .any(|&(x, y)| !(0.0..=1.0).contains(&x) || !(0.0..=1.0).contains(&y))
        {
            return line_error(number, "curve points are within 0 to 1");
        }
        if points.windows(2).any(|pair| match pair {
            [(x0, _), (x1, _)] => x1 <= x0,
            _ => false,
        }) {
            return line_error(number, "curve points ascend in x");
        }
        let Some(table) = Table::build(monotone_cubic(&points)) else {
            return line_error(number, "curve is not finite");
        };
        Ok(Op::Curve { channel, table })
    }

    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    pub fn operations(&self) -> usize {
        self.ops.len()
    }

    /// One linear sRGB pixel through the operations in file order.
    pub fn apply(&self, mut rgb: [f32; 3]) -> [f32; 3] {
        for op in &self.ops {
            rgb = match op {
                Op::Primaries(matrix) => color::apply(matrix, rgb),
                Op::Curve { channel, table } => match channel {
                    Channel::Red => [table.at(rgb[0]), rgb[1], rgb[2]],
                    Channel::Green => [rgb[0], table.at(rgb[1]), rgb[2]],
                    Channel::Blue => [rgb[0], rgb[1], table.at(rgb[2])],
                    Channel::All => rgb.map(|v| table.at(v)),
                    Channel::Luma => {
                        let y = luma(rgb);
                        let target = table.at(y);
                        // Chroma scaled by the ratio when the pixel darkens,
                        // kept when it brightens: a highlight keeps its hue
                        // as it rolls off, and a near-black hue is not
                        // amplified into a vivid colour by a lifted black.
                        // No case at zero: the factor is 1 there, which is
                        // its limit from above too unless a negative
                        // channel leaves chroma at zero luminance; bounded
                        // either way.
                        let factor = if y > 0.0 { (target / y).min(1.0) } else { 1.0 };
                        rgb.map(|v| target + (v - y) * factor)
                    }
                },
                Op::Saturation(s) => {
                    let y = luma(rgb);
                    rgb.map(|v| y + s * (v - y))
                }
                Op::Monochrome(w) => {
                    let y = w[0] * rgb[0] + w[1] * rgb[1] + w[2] * rgb[2];
                    [y; 3]
                }
            };
        }
        rgb
    }
}

/// The built-in set, by stem, each authored by hand in this format. The
/// Fujifilm-inspired family names what it is inspired by and claims no
/// reproduction of it.
pub const BUILTIN: [(&str, &str); 10] = [
    (
        "contrast-boost",
        "td-photo look 1\nname Contrast boost\ntone contrast 1.35 toe 0.0 shoulder 0.0\n",
    ),
    (
        "contrast-soft",
        "td-photo look 1\nname Contrast soft\ntone contrast 0.8 toe 0.0 shoulder 0.0\n",
    ),
    (
        "mono",
        "td-photo look 1\nname Monochrome\nmonochrome 0.2126 0.7152 0.0722\n",
    ),
    (
        "provia-like",
        "td-photo look 1\nname Provia-like\ntone contrast 1.15 toe 0.0 shoulder 0.1\nsaturation 1.05\n",
    ),
    (
        "velvia-like",
        concat!(
            "td-photo look 1\nname Velvia-like\n",
            "primaries 1.06 -0.04 -0.02  -0.03 1.06 -0.03  -0.02 -0.04 1.06\n",
            "tone contrast 1.3 toe 0.15 shoulder 0.0\nsaturation 1.3\n",
        ),
    ),
    (
        "astia-like",
        concat!(
            "td-photo look 1\nname Astia-like\n",
            "tone contrast 1.05 toe -0.15 shoulder 0.25\n",
            "curve luma 0 0  0.5 0.52  1 1\n",
        ),
    ),
    (
        "classic-chrome-like",
        concat!(
            "td-photo look 1\nname Classic Chrome-like\n",
            "primaries 0.92 0.06 0.02  0.03 0.94 0.03  0.02 0.06 0.92\n",
            "tone contrast 1.35 toe 0.0 shoulder 0.0\n",
            "curve luma 0 0  0.25 0.22  0.75 0.78  1 1\nsaturation 0.85\n",
        ),
    ),
    (
        "classic-neg-like",
        concat!(
            "td-photo look 1\nname Classic Neg-like\n",
            "primaries 0.94 0.04 0.02  0.06 0.90 0.04  0.02 0.08 0.90\n",
            "tone contrast 1.4 toe 0.3 shoulder -0.2\nsaturation 0.9\n",
        ),
    ),
    (
        "eterna-like",
        "td-photo look 1\nname Eterna-like\ntone contrast 0.85 toe -0.3 shoulder 0.35\nsaturation 0.75\n",
    ),
    (
        "acros-like",
        concat!(
            "td-photo look 1\nname Acros-like\n",
            "tone contrast 1.25 toe 0.1 shoulder 0.1\nmonochrome 0.30 0.55 0.15\n",
        ),
    ),
];

/// The built-in look of that stem, as text.
pub fn builtin(stem: &str) -> Option<&'static str> {
    BUILTIN
        .iter()
        .find(|(name, _)| *name == stem)
        .map(|(_, text)| *text)
}

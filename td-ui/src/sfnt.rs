//! A bounded reader of TrueType fonts: the table directory, the header,
//! the horizontal metrics, the Unicode character map and the quadratic
//! outlines of one `glyf` face. Hinting programs are never run, and CFF
//! outlines, collections, variations and colour tables are not read. Every
//! offset is checked against the bytes it names, and each outline is held
//! to the budgets below, its composite components included. Nothing here
//! reads the environment, a clock, a descriptor or the filesystem: a
//! consumer hands over the font's bytes.

pub const MAX_FONT_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_TABLES: usize = 64;
/// Points and contours in one glyph's outline, components included.
pub const MAX_POINTS: usize = 8192;
pub const MAX_CONTOURS: usize = 1024;
/// Components visited expanding one glyph, at every depth together, so a
/// composite fan-out cannot multiply through the depth bound.
pub const MAX_COMPONENTS: usize = 256;
pub const MAX_DEPTH: usize = 8;

/// What the reader refuses, naming the item.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    /// A range past the end of the bytes or table that holds it.
    Truncated(&'static str),
    Missing(&'static str),
    Unsupported(&'static str),
    Malformed(&'static str),
    Limit(&'static str),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (kind, item) = match self {
            Self::Truncated(item) => ("truncated", item),
            Self::Missing(item) => ("missing", item),
            Self::Unsupported(item) => ("unsupported", item),
            Self::Malformed(item) => ("malformed", item),
            Self::Limit(item) => ("limit", item),
        };
        write!(f, "sfnt: {kind} {item}")
    }
}

impl std::error::Error for Error {}

/// An outline point in font units, y up, on or off the curve.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Point {
    pub x: f32,
    pub y: f32,
    pub on: bool,
}

/// One glyph's closed contours, reused across glyphs so a caller's steady
/// state allocates nothing.
#[derive(Clone, Debug, Default)]
pub struct Outline {
    points: Vec<Point>,
    ends: Vec<usize>,
    flags: Vec<u8>,
    overlap: bool,
}

impl Outline {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn clear(&mut self) {
        self.points.clear();
        self.ends.clear();
        self.overlap = false;
    }

    /// Whether the font marked the outline's contours as overlapping
    /// (`OVERLAP_SIMPLE` or `OVERLAP_COMPOUND`), which coverage resolves
    /// on a finer grid for masks up to `coverage::MAX_OVERSAMPLED_AXIS`.
    pub fn overlap(&self) -> bool {
        self.overlap
    }

    /// Marks a hand-built outline's contours as overlapping.
    pub fn set_overlap(&mut self) {
        self.overlap = true;
    }

    pub fn is_empty(&self) -> bool {
        self.points.is_empty()
    }

    pub fn points(&self) -> &[Point] {
        &self.points
    }

    /// Each contour's points in order; the last joins the first.
    pub fn contours(&self) -> impl Iterator<Item = &[Point]> {
        let mut start = 0;
        self.ends.iter().filter_map(move |&end| {
            let contour = self.points.get(start..end);
            start = end;
            contour
        })
    }

    /// Appends one contour under the same budgets a font's glyph meets; an
    /// empty one is refused.
    pub fn push_contour(&mut self, points: &[Point]) -> Result<(), Error> {
        if points.is_empty() {
            return Err(Error::Malformed("empty contour"));
        }
        self.reserve(1, points.len())?;
        self.points.extend_from_slice(points);
        self.ends.push(self.points.len());
        Ok(())
    }

    fn reserve(&self, contours: usize, points: usize) -> Result<(), Error> {
        if self.ends.len().saturating_add(contours) > MAX_CONTOURS {
            return Err(Error::Limit("contours"));
        }
        if self.points.len().saturating_add(points) > MAX_POINTS {
            return Err(Error::Limit("points"));
        }
        Ok(())
    }
}

/// A parsed face over the caller's bytes: tables are located and their
/// fixed parts checked once, glyphs are decoded on request.
#[derive(Clone, Copy, Debug)]
pub struct Font<'a> {
    units_per_em: u16,
    ascender: i16,
    descender: i16,
    line_gap: i16,
    glyphs: u16,
    long_metrics: u16,
    long_offsets: bool,
    hmtx: &'a [u8],
    loca: &'a [u8],
    glyf: &'a [u8],
    cmap: Cmap<'a>,
}

#[derive(Clone, Copy, Debug)]
enum Cmap<'a> {
    /// Format 4: the Basic Multilingual Plane in segments.
    Segments(&'a [u8]),
    /// Format 12: every plane in sequential groups.
    Groups(&'a [u8]),
}

impl<'a> Font<'a> {
    pub fn parse(data: &'a [u8]) -> Result<Self, Error> {
        if data.len() > MAX_FONT_BYTES {
            return Err(Error::Limit("font bytes"));
        }
        const DIRECTORY: &str = "table directory";
        match be_u32(data, 0).ok_or(Error::Truncated(DIRECTORY))? {
            0x0001_0000 | 0x7472_7565 => {}
            0x4f54_544f => return Err(Error::Unsupported("CFF outlines")),
            0x7474_6366 => return Err(Error::Unsupported("font collection")),
            _ => return Err(Error::Malformed("sfnt version")),
        }
        let count = usize::from(be_u16(data, 4).ok_or(Error::Truncated(DIRECTORY))?);
        if count > MAX_TABLES {
            return Err(Error::Limit("tables"));
        }
        let mut tables: Vec<([u8; 4], &[u8])> = Vec::with_capacity(count);
        for index in 0..count {
            let record = 12 + 16 * index;
            let tag: [u8; 4] = data
                .get(record..record + 4)
                .and_then(|tag| tag.try_into().ok())
                .ok_or(Error::Truncated(DIRECTORY))?;
            let offset = be_u32(data, record + 8).ok_or(Error::Truncated(DIRECTORY))?;
            let length = be_u32(data, record + 12).ok_or(Error::Truncated(DIRECTORY))?;
            let table = usize::try_from(offset)
                .ok()
                .zip(usize::try_from(length).ok())
                .and_then(|(offset, length)| data.get(offset..offset.checked_add(length)?))
                .ok_or(Error::Truncated("table record"))?;
            if tables.iter().any(|(seen, _)| *seen == tag) {
                return Err(Error::Malformed("duplicate table"));
            }
            tables.push((tag, table));
        }
        let find = |tag: &[u8; 4]| {
            tables
                .iter()
                .find(|(seen, _)| seen == tag)
                .map(|(_, table)| *table)
        };
        let table = |tag: &[u8; 4], name| find(tag).ok_or(Error::Missing(name));

        let head = table(b"head", "head")?;
        if head.len() < 54 {
            return Err(Error::Truncated("head"));
        }
        if be_u32(head, 12) != Some(0x5f0f_3cf5) {
            return Err(Error::Malformed("head magic"));
        }
        let units_per_em = be_u16(head, 18).ok_or(Error::Truncated("head"))?;
        if !(16..=16384).contains(&units_per_em) {
            return Err(Error::Malformed("units per em"));
        }
        let long_offsets = match be_u16(head, 50) {
            Some(0) => false,
            Some(1) => true,
            _ => return Err(Error::Malformed("index to location format")),
        };

        let maxp = table(b"maxp", "maxp")?;
        let glyphs = be_u16(maxp, 4).ok_or(Error::Truncated("maxp"))?;
        if glyphs == 0 {
            return Err(Error::Malformed("glyph count"));
        }

        let hhea = table(b"hhea", "hhea")?;
        if hhea.len() < 36 {
            return Err(Error::Truncated("hhea"));
        }
        let metric = |at| {
            be_u16(hhea, at)
                .map(|v| v as i16)
                .ok_or(Error::Truncated("hhea"))
        };
        let (ascender, descender, line_gap) = (metric(4)?, metric(6)?, metric(8)?);
        let long_metrics = be_u16(hhea, 34).ok_or(Error::Truncated("hhea"))?;
        if long_metrics == 0 || long_metrics > glyphs {
            return Err(Error::Malformed("horizontal metric count"));
        }

        let hmtx = table(b"hmtx", "hmtx")?;
        let needed = 4 * usize::from(long_metrics) + 2 * usize::from(glyphs - long_metrics);
        if hmtx.len() < needed {
            return Err(Error::Truncated("hmtx"));
        }

        let Some(glyf) = find(b"glyf") else {
            if find(b"CFF ").is_some() || find(b"CFF2").is_some() {
                return Err(Error::Unsupported("CFF outlines"));
            }
            return Err(Error::Missing("glyf"));
        };
        let loca = table(b"loca", "loca")?;
        let entry = if long_offsets { 4 } else { 2 };
        if loca.len() < entry * (usize::from(glyphs) + 1) {
            return Err(Error::Truncated("loca"));
        }

        let cmap = select_cmap(table(b"cmap", "cmap")?)?;
        Ok(Self {
            units_per_em,
            ascender,
            descender,
            line_gap,
            glyphs,
            long_metrics,
            long_offsets,
            hmtx,
            loca,
            glyf,
            cmap,
        })
    }

    pub fn units_per_em(&self) -> u16 {
        self.units_per_em
    }

    /// The `hhea` line metrics in font units: ascender above the baseline
    /// (positive), descender below it (negative) and the gap between lines.
    pub fn ascender(&self) -> i16 {
        self.ascender
    }

    pub fn descender(&self) -> i16 {
        self.descender
    }

    pub fn line_gap(&self) -> i16 {
        self.line_gap
    }

    pub fn glyph_count(&self) -> u16 {
        self.glyphs
    }

    /// The glyph the character map gives a scalar; unmapped, the missing
    /// glyph or an index past the face is none.
    pub fn glyph(&self, scalar: char) -> Option<u16> {
        let code = u32::from(scalar);
        let glyph = match self.cmap {
            Cmap::Segments(table) => segment_glyph(table, code),
            Cmap::Groups(table) => group_glyph(table, code),
        }?;
        (glyph != 0 && glyph < self.glyphs).then_some(glyph)
    }

    /// The glyph's advance width in font units; glyphs past the long
    /// metrics share the last one's, as the format has it.
    pub fn advance(&self, glyph: u16) -> Option<u16> {
        if glyph >= self.glyphs {
            return None;
        }
        let index = usize::from(glyph.min(self.long_metrics - 1));
        be_u16(self.hmtx, 4 * index)
    }

    /// Decodes the glyph's outline into `into`, replacing what it held.
    /// An empty glyph (a space) leaves it empty.
    pub fn outline(&self, glyph: u16, into: &mut Outline) -> Result<(), Error> {
        into.clear();
        let mut components = 0;
        let result = self.append(glyph, Transform::IDENTITY, 0, into, &mut components);
        if result.is_err() {
            into.clear();
        }
        result
    }

    fn glyph_data(&self, glyph: u16) -> Result<&'a [u8], Error> {
        if glyph >= self.glyphs {
            return Err(Error::Malformed("glyph index"));
        }
        let index = usize::from(glyph);
        let (start, end) = if self.long_offsets {
            let at = |i: usize| be_u32(self.loca, 4 * i).and_then(|v| usize::try_from(v).ok());
            (at(index), at(index + 1))
        } else {
            let at = |i: usize| be_u16(self.loca, 2 * i).map(|v| 2 * usize::from(v));
            (at(index), at(index + 1))
        };
        let (start, end) = start.zip(end).ok_or(Error::Truncated("loca"))?;
        if start > end {
            return Err(Error::Malformed("loca"));
        }
        self.glyf.get(start..end).ok_or(Error::Truncated("glyf"))
    }

    fn append(
        &self,
        glyph: u16,
        transform: Transform,
        depth: usize,
        out: &mut Outline,
        components: &mut usize,
    ) -> Result<(), Error> {
        if depth > MAX_DEPTH {
            return Err(Error::Limit("composite depth"));
        }
        let data = self.glyph_data(glyph)?;
        if data.is_empty() {
            return Ok(());
        }
        if data.len() < 10 {
            return Err(Error::Truncated("glyph header"));
        }
        let contours = Reader::new(data, 0).i16("glyph header")?;
        match contours {
            0.. => simple(data, contours as usize, transform, out),
            -1 => self.composite(data, transform, depth, out, components),
            _ => Err(Error::Malformed("contour count")),
        }
    }

    fn composite(
        &self,
        data: &[u8],
        transform: Transform,
        depth: usize,
        out: &mut Outline,
        components: &mut usize,
    ) -> Result<(), Error> {
        const ITEM: &str = "composite glyph";
        const WORDS: u16 = 0x0001;
        const XY_VALUES: u16 = 0x0002;
        const SCALE: u16 = 0x0008;
        const MORE: u16 = 0x0020;
        const XY_SCALE: u16 = 0x0040;
        const TWO_BY_TWO: u16 = 0x0080;
        const SCALED_OFFSET: u16 = 0x0800;
        const OVERLAP_COMPOUND: u16 = 0x0400;
        const UNSCALED_OFFSET: u16 = 0x1000;
        let composite_base = out.points.len();
        let mut reader = Reader::new(data, 10);
        loop {
            *components += 1;
            if *components > MAX_COMPONENTS {
                return Err(Error::Limit("components"));
            }
            let flags = reader.u16(ITEM)?;
            let child = reader.u16(ITEM)?;
            out.overlap |= flags & OVERLAP_COMPOUND != 0;
            let (first, second) = if flags & WORDS != 0 {
                if flags & XY_VALUES != 0 {
                    (i32::from(reader.i16(ITEM)?), i32::from(reader.i16(ITEM)?))
                } else {
                    (i32::from(reader.u16(ITEM)?), i32::from(reader.u16(ITEM)?))
                }
            } else if flags & XY_VALUES != 0 {
                (i32::from(reader.i8(ITEM)?), i32::from(reader.i8(ITEM)?))
            } else {
                (i32::from(reader.u8(ITEM)?), i32::from(reader.u8(ITEM)?))
            };
            let mut local = Transform::IDENTITY;
            if flags & SCALE != 0 {
                let scale = reader.f2dot14(ITEM)?;
                (local.a, local.d) = (scale, scale);
            } else if flags & XY_SCALE != 0 {
                (local.a, local.d) = (reader.f2dot14(ITEM)?, reader.f2dot14(ITEM)?);
            } else if flags & TWO_BY_TWO != 0 {
                local.a = reader.f2dot14(ITEM)?;
                local.b = reader.f2dot14(ITEM)?;
                local.c = reader.f2dot14(ITEM)?;
                local.d = reader.f2dot14(ITEM)?;
            }
            let base = out.points.len();
            if flags & XY_VALUES != 0 {
                let (x, y) = (first as f32, second as f32);
                (local.e, local.f) = if flags & SCALED_OFFSET != 0 && flags & UNSCALED_OFFSET == 0 {
                    (local.a * x + local.c * y, local.b * x + local.d * y)
                } else {
                    (x, y)
                };
                self.append(child, transform.then(local), depth + 1, out, components)?;
            } else {
                // Point matching: the child moves so its point `second`
                // lands on this composite's earlier point `first`.
                self.append(child, transform.then(local), depth + 1, out, components)?;
                let anchor = usize::try_from(first)
                    .ok()
                    .and_then(|i| composite_base.checked_add(i))
                    .filter(|&i| i < base)
                    .and_then(|i| out.points.get(i).copied());
                let moved = usize::try_from(second)
                    .ok()
                    .and_then(|i| base.checked_add(i))
                    .and_then(|i| out.points.get(i).copied());
                let (Some(anchor), Some(moved)) = (anchor, moved) else {
                    return Err(Error::Malformed("matched point"));
                };
                let (dx, dy) = (anchor.x - moved.x, anchor.y - moved.y);
                for point in out.points.get_mut(base..).unwrap_or_default() {
                    point.x += dx;
                    point.y += dy;
                }
            }
            if flags & MORE == 0 {
                return Ok(());
            }
        }
    }
}

fn simple(
    data: &[u8],
    contours: usize,
    transform: Transform,
    out: &mut Outline,
) -> Result<(), Error> {
    const ITEM: &str = "simple glyph";
    const ON_CURVE: u8 = 0x01;
    const OVERLAP_SIMPLE: u8 = 0x40;
    const X_SHORT: u8 = 0x02;
    const Y_SHORT: u8 = 0x04;
    const REPEAT: u8 = 0x08;
    const X_SAME: u8 = 0x10;
    const Y_SAME: u8 = 0x20;
    if contours == 0 {
        return Ok(());
    }
    out.reserve(contours, 0)?;
    let mut reader = Reader::new(data, 10);
    // A refusal part way leaves `out` for the caller to clear, as
    // `Font::outline` does.
    let base = out.points.len();
    let mut previous: Option<usize> = None;
    for _ in 0..contours {
        let end = usize::from(reader.u16(ITEM)?);
        if previous.is_some_and(|previous| end <= previous) {
            return Err(Error::Malformed("contour ends"));
        }
        previous = Some(end);
        out.ends.push(base + end + 1);
    }
    let points = previous.map_or(0, |last| last + 1);
    out.reserve(0, points)?;
    let instructions = usize::from(reader.u16(ITEM)?);
    reader.skip(instructions, ITEM)?;

    out.flags.clear();
    while out.flags.len() < points {
        let first = out.flags.is_empty();
        let flag = reader.u8(ITEM)?;
        out.overlap |= first && flag & OVERLAP_SIMPLE != 0;
        out.flags.push(flag);
        if flag & REPEAT != 0 {
            let repeat = usize::from(reader.u8(ITEM)?);
            if out.flags.len() + repeat > points {
                return Err(Error::Malformed("flag repeat"));
            }
            out.flags.extend(std::iter::repeat_n(flag, repeat));
        }
    }
    let mut x = 0i32;
    for &flag in &out.flags {
        x += if flag & X_SHORT != 0 {
            let delta = i32::from(reader.u8(ITEM)?);
            if flag & X_SAME != 0 {
                delta
            } else {
                -delta
            }
        } else if flag & X_SAME != 0 {
            0
        } else {
            i32::from(reader.i16(ITEM)?)
        };
        out.points.push(Point {
            x: x as f32,
            y: 0.0,
            on: flag & ON_CURVE != 0,
        });
    }
    let mut y = 0i32;
    for (point, &flag) in out
        .points
        .get_mut(base..)
        .unwrap_or_default()
        .iter_mut()
        .zip(&out.flags)
    {
        y += if flag & Y_SHORT != 0 {
            let delta = i32::from(reader.u8(ITEM)?);
            if flag & Y_SAME != 0 {
                delta
            } else {
                -delta
            }
        } else if flag & Y_SAME != 0 {
            0
        } else {
            i32::from(reader.i16(ITEM)?)
        };
        (point.x, point.y) = transform.apply(point.x, y as f32);
    }
    Ok(())
}

/// An affine map `x' = a x + c y + e`, `y' = b x + d y + f`.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Transform {
    a: f32,
    b: f32,
    c: f32,
    d: f32,
    e: f32,
    f: f32,
}

impl Transform {
    const IDENTITY: Self = Self {
        a: 1.0,
        b: 0.0,
        c: 0.0,
        d: 1.0,
        e: 0.0,
        f: 0.0,
    };

    fn apply(self, x: f32, y: f32) -> (f32, f32) {
        (
            self.a * x + self.c * y + self.e,
            self.b * x + self.d * y + self.f,
        )
    }

    /// `inner` first, then `self`.
    fn then(self, inner: Self) -> Self {
        let (e, f) = self.apply(inner.e, inner.f);
        Self {
            a: self.a * inner.a + self.c * inner.b,
            b: self.b * inner.a + self.d * inner.b,
            c: self.a * inner.c + self.c * inner.d,
            d: self.b * inner.c + self.d * inner.d,
            e,
            f,
        }
    }
}

/// The best Unicode subtable: format 12 over format 4, Windows full
/// repertoire over Unicode platform over Windows BMP. A malformed
/// candidate is passed over; the font is refused, with the first
/// candidate's error, only when none is usable.
fn select_cmap(cmap: &[u8]) -> Result<Cmap<'_>, Error> {
    let count = usize::from(be_u16(cmap, 2).ok_or(Error::Truncated("cmap"))?);
    let mut best: Option<(u8, Cmap<'_>)> = None;
    let mut refusal = None;
    for index in 0..count {
        let record = 4 + 8 * index;
        let platform = be_u16(cmap, record).ok_or(Error::Truncated("cmap"))?;
        let encoding = be_u16(cmap, record + 2).ok_or(Error::Truncated("cmap"))?;
        let offset = be_u32(cmap, record + 4)
            .and_then(|offset| usize::try_from(offset).ok())
            .ok_or(Error::Truncated("cmap"))?;
        let unicode = platform == 0 || (platform == 3 && matches!(encoding, 1 | 10));
        if !unicode {
            continue;
        }
        let rank = match (platform, encoding) {
            (3, 10) => 0,
            (0, _) => 1,
            _ => 2,
        };
        let candidate = match subtable(cmap, offset) {
            None => continue,
            Some(Err(error)) => {
                refusal.get_or_insert(error);
                continue;
            }
            Some(Ok(Cmap::Groups(table))) => (rank, Cmap::Groups(table)),
            Some(Ok(Cmap::Segments(table))) => (rank + 3, Cmap::Segments(table)),
        };
        if best.as_ref().is_none_or(|(rank, _)| candidate.0 < *rank) {
            best = Some(candidate);
        }
    }
    match (best, refusal) {
        (Some((_, cmap)), _) => Ok(cmap),
        (None, Some(error)) => Err(error),
        (None, None) => Err(Error::Missing("unicode cmap")),
    }
}

/// The subtable at `offset` with its fixed part checked, or none when its
/// format is neither 4 nor 12. An offset past the table is truncated.
fn subtable(cmap: &[u8], offset: usize) -> Option<Result<Cmap<'_>, Error>> {
    let at = |delta: usize| offset.checked_add(delta);
    let Some(format) = be_u16(cmap, offset) else {
        return Some(Err(Error::Truncated("cmap")));
    };
    match format {
        12 => {
            let table = at(4)
                .and_then(|at| be_u32(cmap, at))
                .and_then(|length| usize::try_from(length).ok())
                .and_then(|length| cmap.get(offset..at(length)?))
                .filter(|table| {
                    be_u32(table, 12)
                        .and_then(|groups| usize::try_from(groups).ok())
                        .and_then(|groups| groups.checked_mul(12)?.checked_add(16))
                        .is_some_and(|needed| needed <= table.len())
                });
            Some(
                table
                    .map(Cmap::Groups)
                    .ok_or(Error::Truncated("cmap format 12")),
            )
        }
        4 => {
            const TRUNCATED: Error = Error::Truncated("cmap format 4");
            let Some(table) = at(2)
                .and_then(|at| be_u16(cmap, at))
                .and_then(|length| cmap.get(offset..at(usize::from(length))?))
            else {
                return Some(Err(TRUNCATED));
            };
            let Some(segments2) = be_u16(table, 6).map(usize::from) else {
                return Some(Err(TRUNCATED));
            };
            if segments2 == 0 || segments2 % 2 != 0 {
                return Some(Err(Error::Malformed("cmap format 4 segments")));
            }
            if 16 + 4 * segments2 > table.len() {
                return Some(Err(TRUNCATED));
            }
            Some(Ok(Cmap::Segments(table)))
        }
        _ => None,
    }
}

fn segment_glyph(table: &[u8], code: u32) -> Option<u16> {
    let code = u16::try_from(code).ok()?;
    let segments2 = usize::from(be_u16(table, 6)?);
    let segments = segments2 / 2;
    let segment = first_at_least(segments, code.into(), |i| {
        be_u16(table, 14 + 2 * i).map(u32::from)
    })?;
    let start = be_u16(table, 16 + segments2 + 2 * segment)?;
    if code < start {
        return None;
    }
    let delta = be_u16(table, 16 + 2 * segments2 + 2 * segment)?;
    let range_at = 16 + 3 * segments2 + 2 * segment;
    let range = usize::from(be_u16(table, range_at)?);
    if range == 0 {
        return Some(code.wrapping_add(delta));
    }
    let glyph = be_u16(table, range_at + range + 2 * usize::from(code - start))?;
    (glyph != 0).then(|| glyph.wrapping_add(delta))
}

fn group_glyph(table: &[u8], code: u32) -> Option<u16> {
    let groups = usize::try_from(be_u32(table, 12)?).ok()?;
    let group = first_at_least(groups, code, |i| be_u32(table, 16 + 12 * i + 4))?;
    let start = be_u32(table, 16 + 12 * group)?;
    if code < start {
        return None;
    }
    let first = be_u32(table, 16 + 12 * group + 8)?;
    u16::try_from(first.checked_add(code - start)?).ok()
}

/// The first of `count` entries, ascending by `end`, whose end is at least
/// `code`; unsorted tables give a wrong answer, never a fault.
fn first_at_least(count: usize, code: u32, end: impl Fn(usize) -> Option<u32>) -> Option<usize> {
    let (mut low, mut high) = (0, count);
    while low < high {
        let middle = low + (high - low) / 2;
        if end(middle)? < code {
            low = middle + 1;
        } else {
            high = middle;
        }
    }
    (low < count).then_some(low)
}

fn be_u16(data: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes(
        data.get(at..at.checked_add(2)?)?.try_into().ok()?,
    ))
}

fn be_u32(data: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(
        data.get(at..at.checked_add(4)?)?.try_into().ok()?,
    ))
}

struct Reader<'a> {
    data: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn new(data: &'a [u8], at: usize) -> Self {
        Self { data, at }
    }

    fn skip(&mut self, bytes: usize, item: &'static str) -> Result<(), Error> {
        let end = self
            .at
            .checked_add(bytes)
            .filter(|&end| end <= self.data.len());
        self.at = end.ok_or(Error::Truncated(item))?;
        Ok(())
    }

    fn u8(&mut self, item: &'static str) -> Result<u8, Error> {
        let value = *self.data.get(self.at).ok_or(Error::Truncated(item))?;
        self.at += 1;
        Ok(value)
    }

    fn i8(&mut self, item: &'static str) -> Result<i8, Error> {
        self.u8(item).map(|v| v as i8)
    }

    fn u16(&mut self, item: &'static str) -> Result<u16, Error> {
        let value = be_u16(self.data, self.at).ok_or(Error::Truncated(item))?;
        self.at += 2;
        Ok(value)
    }

    fn i16(&mut self, item: &'static str) -> Result<i16, Error> {
        self.u16(item).map(|v| v as i16)
    }

    fn f2dot14(&mut self, item: &'static str) -> Result<f32, Error> {
        self.i16(item).map(|v| f32::from(v) / 16384.0)
    }
}

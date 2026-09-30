//! A TrueType encoder for the tests: real sfnt binaries with compact
//! coordinates, flag runs, both loca formats and both character-map
//! formats, so no fetched font participates in any oracle.

#![allow(dead_code)]

#[derive(Clone)]
pub enum Glyph {
    Empty,
    Simple(Vec<Vec<(i32, i32, bool)>>),
    Composite(Vec<Component>),
    Raw(Vec<u8>),
}

#[derive(Clone)]
pub struct Component {
    pub glyph: u16,
    pub flags: u16,
    pub args: (i32, i32),
    pub transform: Vec<i16>,
}

pub fn offset(glyph: u16, dx: i32, dy: i32) -> Component {
    Component {
        glyph,
        flags: 0x0001 | 0x0002,
        args: (dx, dy),
        transform: vec![],
    }
}

pub enum Segment {
    Delta(u16, u16, u16),
    Array(u16, Vec<u16>),
}

pub struct Builder {
    pub glyphs: Vec<Glyph>,
    pub long_metrics: u16,
    pub format4: Vec<Segment>,
    pub format12: Option<Vec<(u32, u32, u32)>>,
    pub omit: Vec<&'static [u8; 4]>,
    pub extra: Vec<([u8; 4], Vec<u8>)>,
    pub units_per_em: u16,
    pub short_offsets: bool,
    pub ascender: i16,
    pub descender: i16,
    /// Every glyph's advance; `None` gives glyph `i` 500 + `i`.
    pub advance: Option<u16>,
}

impl Builder {
    pub fn new(glyphs: Vec<Glyph>) -> Self {
        let count = glyphs.len() as u16;
        Self {
            glyphs,
            long_metrics: count,
            format4: vec![Segment::Delta(0x41, 0x43, 1u16.wrapping_sub(0x41))],
            format12: None,
            omit: vec![],
            extra: vec![],
            units_per_em: 1000,
            short_offsets: false,
            ascender: 800,
            descender: -200,
            advance: None,
        }
    }

    pub fn table(&self, tag: &[u8; 4]) -> Option<Vec<u8>> {
        if self.omit.contains(&tag) {
            return None;
        }
        let count = self.glyphs.len() as u16;
        Some(match tag {
            b"head" => {
                let mut head = vec![0u8; 54];
                head[0..4].copy_from_slice(&0x0001_0000u32.to_be_bytes());
                head[12..16].copy_from_slice(&0x5f0f_3cf5u32.to_be_bytes());
                head[18..20].copy_from_slice(&self.units_per_em.to_be_bytes());
                head[50..52].copy_from_slice(&u16::from(!self.short_offsets).to_be_bytes());
                head
            }
            b"maxp" => {
                let mut maxp = 0x0000_5000u32.to_be_bytes().to_vec();
                maxp.extend_from_slice(&count.to_be_bytes());
                maxp
            }
            b"hhea" => {
                let mut hhea = vec![0u8; 36];
                hhea[4..6].copy_from_slice(&self.ascender.to_be_bytes());
                hhea[6..8].copy_from_slice(&self.descender.to_be_bytes());
                hhea[8..10].copy_from_slice(&90i16.to_be_bytes());
                hhea[34..36].copy_from_slice(&self.long_metrics.to_be_bytes());
                hhea
            }
            b"hmtx" => {
                let mut hmtx = vec![];
                for index in 0..self.glyphs.len() {
                    let advance = self.advance.unwrap_or(500 + index as u16);
                    if index < usize::from(self.long_metrics) {
                        hmtx.extend_from_slice(&advance.to_be_bytes());
                    }
                    hmtx.extend_from_slice(&0i16.to_be_bytes());
                }
                hmtx
            }
            b"glyf" => self.glyf().0,
            b"loca" => {
                let offsets = self.glyf().1;
                let mut loca = vec![];
                for offset in offsets {
                    if self.short_offsets {
                        loca.extend_from_slice(&((offset / 2) as u16).to_be_bytes());
                    } else {
                        loca.extend_from_slice(&offset.to_be_bytes());
                    }
                }
                loca
            }
            b"cmap" => self.cmap(),
            _ => return None,
        })
    }

    pub fn glyf(&self) -> (Vec<u8>, Vec<u32>) {
        let mut glyf = vec![];
        let mut offsets = vec![0];
        for glyph in &self.glyphs {
            glyf.extend(encode(glyph));
            if self.short_offsets && glyf.len() % 2 == 1 {
                glyf.push(0);
            }
            offsets.push(glyf.len() as u32);
        }
        (glyf, offsets)
    }

    pub fn cmap(&self) -> Vec<u8> {
        let mut four = vec![];
        let mut segments: Vec<&Segment> = self.format4.iter().collect();
        let last = Segment::Delta(0xffff, 0xffff, 1);
        segments.push(&last);
        let n = segments.len();
        let ends: Vec<u16> = segments
            .iter()
            .map(|s| match s {
                Segment::Delta(_, end, _) => *end,
                Segment::Array(start, glyphs) => start + glyphs.len() as u16 - 1,
            })
            .collect();
        let mut arrays: Vec<u16> = vec![];
        let mut ranges = vec![];
        for (index, segment) in segments.iter().enumerate() {
            match segment {
                Segment::Delta(..) => ranges.push(0u16),
                Segment::Array(_, glyphs) => {
                    // Bytes from this idRangeOffset entry to its first glyph.
                    ranges.push((2 * (n - index) + 2 * arrays.len()) as u16);
                    arrays.extend(glyphs);
                }
            }
        }
        four.extend_from_slice(&4u16.to_be_bytes());
        four.extend_from_slice(&0u16.to_be_bytes());
        four.extend_from_slice(&0u16.to_be_bytes());
        four.extend_from_slice(&((2 * n) as u16).to_be_bytes());
        four.extend_from_slice(&[0; 6]);
        for end in &ends {
            four.extend_from_slice(&end.to_be_bytes());
        }
        four.extend_from_slice(&[0, 0]);
        for segment in &segments {
            let start = match segment {
                Segment::Delta(start, ..) | Segment::Array(start, _) => *start,
            };
            four.extend_from_slice(&start.to_be_bytes());
        }
        for segment in &segments {
            let delta = match segment {
                Segment::Delta(_, _, delta) => *delta,
                Segment::Array(..) => 0,
            };
            four.extend_from_slice(&delta.to_be_bytes());
        }
        for range in &ranges {
            four.extend_from_slice(&range.to_be_bytes());
        }
        for glyph in &arrays {
            four.extend_from_slice(&glyph.to_be_bytes());
        }
        let length = four.len() as u16;
        four[2..4].copy_from_slice(&length.to_be_bytes());

        let mut subtables = vec![((3u16, 1u16), four)];
        if let Some(groups) = &self.format12 {
            let mut twelve = vec![];
            twelve.extend_from_slice(&12u16.to_be_bytes());
            twelve.extend_from_slice(&0u16.to_be_bytes());
            twelve.extend_from_slice(&((16 + 12 * groups.len()) as u32).to_be_bytes());
            twelve.extend_from_slice(&0u32.to_be_bytes());
            twelve.extend_from_slice(&(groups.len() as u32).to_be_bytes());
            for (start, end, glyph) in groups {
                for value in [start, end, glyph] {
                    twelve.extend_from_slice(&value.to_be_bytes());
                }
            }
            subtables.push(((3, 10), twelve));
        }
        let mut cmap = vec![];
        cmap.extend_from_slice(&0u16.to_be_bytes());
        cmap.extend_from_slice(&(subtables.len() as u16).to_be_bytes());
        let mut offset = 4 + 8 * subtables.len();
        for ((platform, encoding), table) in &subtables {
            cmap.extend_from_slice(&platform.to_be_bytes());
            cmap.extend_from_slice(&encoding.to_be_bytes());
            cmap.extend_from_slice(&(offset as u32).to_be_bytes());
            offset += table.len();
        }
        for (_, table) in &subtables {
            cmap.extend_from_slice(table);
        }
        cmap
    }

    pub fn build(&self) -> Vec<u8> {
        let mut tables: Vec<([u8; 4], Vec<u8>)> = vec![];
        for tag in [
            b"cmap", b"glyf", b"head", b"hhea", b"hmtx", b"loca", b"maxp",
        ] {
            if let Some(table) = self.table(tag) {
                tables.push((*tag, table));
            }
        }
        tables.extend(self.extra.iter().cloned());
        assemble(0x0001_0000, &tables)
    }

    pub fn font(&self) -> Vec<u8> {
        self.build()
    }
}

pub fn assemble(version: u32, tables: &[([u8; 4], Vec<u8>)]) -> Vec<u8> {
    let mut out = version.to_be_bytes().to_vec();
    out.extend_from_slice(&(tables.len() as u16).to_be_bytes());
    out.extend_from_slice(&[0; 6]);
    let mut offset = 12 + 16 * tables.len();
    for (tag, table) in tables {
        out.extend_from_slice(tag);
        out.extend_from_slice(&[0; 4]);
        out.extend_from_slice(&(offset as u32).to_be_bytes());
        out.extend_from_slice(&(table.len() as u32).to_be_bytes());
        offset += table.len().next_multiple_of(4);
    }
    for (_, table) in tables {
        out.extend_from_slice(table);
        out.resize(out.len().next_multiple_of(4), 0);
    }
    out
}

/// Encodes with the compact forms wherever they apply, so decoding meets
/// short vectors of both signs, repeated coordinates and flag runs.
pub fn encode(glyph: &Glyph) -> Vec<u8> {
    let mut out = vec![];
    match glyph {
        Glyph::Empty => {}
        Glyph::Raw(bytes) => out.extend_from_slice(bytes),
        Glyph::Simple(contours) => {
            out.extend_from_slice(&(contours.len() as i16).to_be_bytes());
            out.extend_from_slice(&[0; 8]);
            let mut end = 0usize;
            for contour in contours {
                end += contour.len();
                out.extend_from_slice(&((end - 1) as u16).to_be_bytes());
            }
            out.extend_from_slice(&3u16.to_be_bytes());
            out.extend_from_slice(&[0xb0, 0x01, 0x2f]);
            let points: Vec<(i32, i32, bool)> = contours.iter().flatten().copied().collect();
            let (mut flags, mut xs, mut ys) = (vec![], vec![], vec![]);
            let (mut x, mut y) = (0, 0);
            for &(px, py, on) in &points {
                let mut flag = u8::from(on);
                for (delta, short, same, bytes) in
                    [(px - x, 0x02, 0x10, &mut xs), (py - y, 0x04, 0x20, &mut ys)]
                {
                    if delta == 0 {
                        flag |= same;
                    } else if delta.abs() <= 255 {
                        flag |= short | if delta > 0 { same } else { 0 };
                        bytes.push(delta.unsigned_abs() as u8);
                    } else {
                        bytes.extend_from_slice(&(delta as i16).to_be_bytes());
                    }
                }
                (x, y) = (px, py);
                flags.push(flag);
            }
            let mut index = 0;
            while index < flags.len() {
                let flag = flags[index];
                let run = flags[index..].iter().take_while(|&&f| f == flag).count();
                if run > 1 {
                    out.push(flag | 0x08);
                    out.push((run - 1) as u8);
                } else {
                    out.push(flag);
                }
                index += run;
            }
            out.extend(xs);
            out.extend(ys);
        }
        Glyph::Composite(components) => {
            out.extend_from_slice(&(-1i16).to_be_bytes());
            out.extend_from_slice(&[0; 8]);
            for (index, component) in components.iter().enumerate() {
                let more = if index + 1 < components.len() {
                    0x20
                } else {
                    0
                };
                out.extend_from_slice(&(component.flags | more).to_be_bytes());
                out.extend_from_slice(&component.glyph.to_be_bytes());
                let (a, b) = component.args;
                if component.flags & 0x0001 != 0 {
                    out.extend_from_slice(&(a as i16).to_be_bytes());
                    out.extend_from_slice(&(b as i16).to_be_bytes());
                } else {
                    out.push(a as u8);
                    out.push(b as u8);
                }
                for value in &component.transform {
                    out.extend_from_slice(&value.to_be_bytes());
                }
            }
        }
    }
    out
}

pub fn square(x: i32, y: i32, size: i32) -> Vec<(i32, i32, bool)> {
    vec![
        (x, y, true),
        (x, y + size, true),
        (x + size, y + size, true),
        (x + size, y, true),
    ]
}

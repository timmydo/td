//! The pinned PSF2 face of section 11. Every header field, every table entry,
//! and every pixel offset is checked once at parse time, so the renderer's
//! inner loop can index a glyph without arithmetic that could fail.

use std::collections::BTreeMap;

/// The committed face, decoded from the generated hex module. Hex text rather
/// than a binary beside the sources because the target recipe stages modules
/// through `Step::WriteFile`, whose content is a `String`; an `include_bytes!`
/// of an unstaged asset compiles on the host and fails in the target build.
#[allow(dead_code)]
pub fn pinned() -> Result<Font, String> {
    Font::parse(&decode_hex(crate::font_data::UNIFONT_HEX)?)
}

#[allow(dead_code)]
fn decode_hex(hex: &str) -> Result<Vec<u8>, String> {
    let bytes = hex.as_bytes();
    if !bytes.len().is_multiple_of(2) {
        return Err("font hex has an odd length".to_string());
    }
    let mut out = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.chunks(2) {
        let text = std::str::from_utf8(pair).map_err(|_| "font hex is not ASCII".to_string())?;
        out.push(u8::from_str_radix(text, 16).map_err(|_| format!("font hex '{text}'"))?);
    }
    Ok(out)
}

#[allow(dead_code)]
const MAGIC: [u8; 4] = [0x72, 0xb5, 0x4a, 0x86];
#[allow(dead_code)]
const HEADER_BYTES: usize = 32;
#[allow(dead_code)]
const HAS_UNICODE_TABLE: u32 = 1;
#[allow(dead_code)]
const SEPARATOR: u8 = 0xff;
#[allow(dead_code)]
const SEQUENCE_START: u8 = 0xfe;

/// A cell wider than this cannot be a terminal cell on the supported output.
/// These bound `row_bytes * height`; `charsize` is then pinned equal to it.
#[allow(dead_code)]
const MAX_DIMENSION: usize = 64;
#[allow(dead_code)]
const MAX_GLYPHS: usize = 1 << 20;

/// Drawn when the model holds a scalar the face has no glyph for. U+FFFD if
/// the face carries it, else space: a terminal that silently drew nothing
/// would misreport its own contents.
#[allow(dead_code)]
const REPLACEMENT: char = '\u{fffd}';
#[allow(dead_code)]
const BLANK: char = ' ';

#[derive(Debug)]
#[allow(dead_code)]
pub struct Font {
    width: usize,
    height: usize,
    /// Bytes per glyph. Equal to `height * row_bytes` -- parse refuses any
    /// other value -- and kept as its own field because it is what steps from
    /// one glyph to the next.
    charsize: usize,
    /// Bytes per row within a glyph.
    row_bytes: usize,
    glyphs: Vec<u8>,
    count: usize,
    map: BTreeMap<char, usize>,
    fallback: usize,
}

#[allow(dead_code)]
fn word(bytes: &[u8], at: usize) -> Result<u32, String> {
    let end = at
        .checked_add(4)
        .ok_or_else(|| "psf2 header offset overflow".to_string())?;
    let raw: [u8; 4] = bytes
        .get(at..end)
        .ok_or_else(|| format!("psf2 header is short at {at}"))?
        .try_into()
        .map_err(|_| format!("psf2 header is short at {at}"))?;
    Ok(u32::from_le_bytes(raw))
}

#[allow(dead_code)]
fn size(value: u32, what: &str) -> Result<usize, String> {
    usize::try_from(value).map_err(|_| format!("psf2 {what} {value} does not fit"))
}

impl Font {
    #[allow(dead_code)]
    pub fn parse(bytes: &[u8]) -> Result<Font, String> {
        if bytes.get(..4) != Some(&MAGIC[..]) {
            return Err("not a PSF2 face".to_string());
        }
        if word(bytes, 4)? != 0 {
            return Err("psf2 version is not 0".to_string());
        }
        let header = size(word(bytes, 8)?, "headersize")?;
        if header != HEADER_BYTES {
            return Err(format!("psf2 headersize is {header}, expected {HEADER_BYTES}"));
        }
        let flags = word(bytes, 12)?;
        if flags & HAS_UNICODE_TABLE == 0 {
            return Err("psf2 face carries no Unicode table".to_string());
        }
        let count = size(word(bytes, 16)?, "length")?;
        let charsize = size(word(bytes, 20)?, "charsize")?;
        let height = size(word(bytes, 24)?, "height")?;
        let width = size(word(bytes, 28)?, "width")?;
        if count == 0 || count > MAX_GLYPHS {
            return Err(format!("psf2 declares {count} glyphs"));
        }
        if width == 0 || height == 0 || width > MAX_DIMENSION || height > MAX_DIMENSION {
            return Err(format!("psf2 cell is {width}x{height}"));
        }
        let row_bytes = width.div_ceil(8);
        let needed = row_bytes
            .checked_mul(height)
            .ok_or_else(|| "psf2 glyph size overflow".to_string())?;
        // Exactly, not merely enough: PSF2 defines charsize as
        // height * ceil(width/8) with no padding, and `row()` finds a row at
        // `index * charsize + row * row_bytes`, which only holds when any
        // slack is trailing. Accepting a larger charsize would read
        // row-padded faces at the wrong offsets and draw garbled glyphs from
        // inside the right glyph -- memory-safe, and silent.
        if charsize != needed {
            return Err(format!(
                "psf2 charsize {charsize} is not the {needed} bytes a {width}x{height} cell needs"
            ));
        }
        let bitmap_bytes = charsize
            .checked_mul(count)
            .ok_or_else(|| "psf2 bitmap size overflow".to_string())?;
        let table_at = header
            .checked_add(bitmap_bytes)
            .ok_or_else(|| "psf2 table offset overflow".to_string())?;
        let glyphs = bytes
            .get(header..table_at)
            .ok_or_else(|| format!("psf2 bitmaps need {bitmap_bytes} bytes"))?
            .to_vec();
        let table = bytes
            .get(table_at..)
            .ok_or_else(|| "psf2 has no Unicode table".to_string())?;
        let map = parse_table(table, count)?;
        let fallback = map
            .get(&REPLACEMENT)
            .or_else(|| map.get(&BLANK))
            .copied()
            .ok_or_else(|| "psf2 face has neither U+FFFD nor a space".to_string())?;
        Ok(Font {
            width,
            height,
            charsize,
            row_bytes,
            glyphs,
            count,
            map,
            fallback,
        })
    }

    #[allow(dead_code)]
    pub fn width(&self) -> usize {
        self.width
    }

    #[allow(dead_code)]
    pub fn height(&self) -> usize {
        self.height
    }

    #[allow(dead_code)]
    pub fn glyph_count(&self) -> usize {
        self.count
    }

    #[allow(dead_code)]
    pub fn covers(&self, scalar: char) -> bool {
        self.map.contains_key(&scalar)
    }

    /// Glyph index for a scalar, falling back rather than failing so the
    /// renderer has no error path in its cell loop.
    #[allow(dead_code)]
    pub fn index(&self, scalar: char) -> usize {
        self.map.get(&scalar).copied().unwrap_or(self.fallback)
    }

    /// One row of a glyph's bitmap, already bounded. `row` beyond the cell
    /// yields nothing rather than the next glyph's pixels.
    #[allow(dead_code)]
    pub fn row(&self, index: usize, row: usize) -> Option<&[u8]> {
        if row >= self.height || index >= self.count {
            return None;
        }
        let at = index
            .checked_mul(self.charsize)?
            .checked_add(row.checked_mul(self.row_bytes)?)?;
        self.glyphs.get(at..at.checked_add(self.row_bytes)?)
    }

    /// Whether the pixel at (`column`, `row`) of a glyph is set. Columns are
    /// most-significant-bit first, as PSF2 stores them.
    #[allow(dead_code)]
    pub fn pixel(&self, index: usize, column: usize, row: usize) -> bool {
        if column >= self.width {
            return false;
        }
        let Some(bits) = self.row(index, row) else {
            return false;
        };
        let Some(byte) = bits.get(column / 8) else {
            return false;
        };
        let shift = 7 - (column % 8);
        byte >> shift & 1 == 1
    }
}

/// PSF2's table is one entry per glyph, each a run of UTF-8 scalars ended by
/// 0xFF. 0xFE introduces a multi-scalar sequence, which this profile does not
/// claim: the sequence is skipped, so a face carrying one still loads and its
/// single scalars still resolve.
#[allow(dead_code)]
fn parse_table(table: &[u8], count: usize) -> Result<BTreeMap<char, usize>, String> {
    let mut map = BTreeMap::new();
    let mut index = 0usize;
    let mut at = 0usize;
    let mut start = 0usize;
    let mut in_sequence = false;
    while at < table.len() {
        let Some(byte) = table.get(at).copied() else {
            break;
        };
        if byte == SEPARATOR || byte == SEQUENCE_START {
            let run = table
                .get(start..at)
                .ok_or_else(|| "psf2 table run is out of bounds".to_string())?;
            if index >= count {
                return Err(format!("psf2 table describes more than {count} glyphs"));
            }
            // Sequence runs are not mapped, but they are still validated: the
            // claim is that every table byte was checked, not merely the bytes
            // this profile happens to use.
            let text = std::str::from_utf8(run)
                .map_err(|_| format!("psf2 table entry {index} is not UTF-8"))?;
            if !in_sequence {
                for scalar in text.chars() {
                    map.entry(scalar).or_insert(index);
                }
            }
            if byte == SEPARATOR {
                index = index.saturating_add(1);
                in_sequence = false;
            } else {
                in_sequence = true;
            }
            start = at.saturating_add(1);
        }
        at = at.saturating_add(1);
    }
    if index != count {
        return Err(format!(
            "psf2 table describes {index} glyphs, header declares {count}"
        ));
    }
    // Bytes after the last separator belong to no entry. Ignoring them would
    // accept a face whose table is longer than the glyphs it describes.
    if start != table.len() {
        return Err(format!(
            "psf2 table has {} bytes after its last entry",
            table.len().saturating_sub(start)
        ));
    }
    if map.is_empty() {
        return Err("psf2 table maps no scalars".to_string());
    }
    Ok(map)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unifont() -> Font {
        pinned().unwrap()
    }

    #[test]
    fn the_committed_face_is_the_single_width_unifont_the_asset_record_names() {
        let font = unifont();
        assert_eq!((font.width(), font.height()), (8, 16));
        assert_eq!(font.glyph_count(), 20673);
        let face = decode_hex(crate::font_data::UNIFONT_HEX).unwrap();
        assert_eq!(face.len(), 422_671);
        assert!(decode_hex("0").is_err());
        assert!(decode_hex("zz").is_err());
    }

    #[test]
    fn the_face_covers_what_the_first_profile_renders() {
        let font = unifont();
        for scalar in ' '..='~' {
            assert!(font.covers(scalar), "missing U+{:04X}", scalar as u32);
        }
        // Line drawing and the replacement character the fallback needs.
        for scalar in ['─', '│', '┌', '┐', '└', '┘', '├', '┤', '┬', '┴', '┼', '\u{fffd}'] {
            assert!(font.covers(scalar), "missing U+{:04X}", scalar as u32);
        }
        // Double-width scalars are out of profile, so the face omits them and
        // the fallback is what a model holding one would draw.
        assert!(!font.covers('漢'));
        assert_eq!(font.index('漢'), font.index('\u{fffd}'));
    }

    #[test]
    fn glyph_pixels_are_bounded_on_every_axis() {
        let font = unifont();
        let index = font.index('A');
        assert!(font.row(index, 0).is_some());
        assert!(font.row(index, 15).is_some());
        assert!(font.row(index, 16).is_none(), "past the last row");
        assert!(font.row(font.glyph_count(), 0).is_none(), "past the last glyph");
        assert!(!font.pixel(index, 8, 0), "past the last column");
        assert!(!font.pixel(index, 0, 99), "past the last row");
        // A blank cell is genuinely blank, and a letter is genuinely not.
        let space = font.index(' ');
        assert!((0..16).all(|row| (0..8).all(|col| !font.pixel(space, col, row))));
        assert!((0..16).any(|row| (0..8).any(|col| font.pixel(index, col, row))));
    }

    fn face(header: &[u32; 6], glyphs: &[u8], table: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&MAGIC);
        out.extend_from_slice(&0u32.to_le_bytes());
        for value in header {
            out.extend_from_slice(&value.to_le_bytes());
        }
        out.extend_from_slice(glyphs);
        out.extend_from_slice(table);
        out
    }

    #[test]
    fn every_header_field_is_checked_before_use() {
        let good = [32, HAS_UNICODE_TABLE, 1, 16, 16, 8];
        let glyphs = [0u8; 16];
        let table = b" \xff";
        assert!(Font::parse(&face(&good, &glyphs, table)).is_ok());

        assert!(Font::parse(b"nope").is_err());
        let mut wrong_magic = face(&good, &glyphs, table);
        wrong_magic[0] = 0;
        assert!(Font::parse(&wrong_magic).is_err());

        let no_table = [32, 0, 1, 16, 16, 8];
        assert!(Font::parse(&face(&no_table, &glyphs, table)).is_err());
        let bad_header = [33, HAS_UNICODE_TABLE, 1, 16, 16, 8];
        assert!(Font::parse(&face(&bad_header, &glyphs, table)).is_err());
        let no_glyphs = [32, HAS_UNICODE_TABLE, 0, 16, 16, 8];
        assert!(Font::parse(&face(&no_glyphs, &glyphs, table)).is_err());
        let huge = [32, HAS_UNICODE_TABLE, 1, 16, 16, u32::MAX];
        assert!(Font::parse(&face(&huge, &glyphs, table)).is_err());
        // charsize must be exactly the cell: too small lets a row read run
        // into the next glyph, and too large means the face padded rows that
        // `row()`'s offset arithmetic would then skip past.
        let short = [32, HAS_UNICODE_TABLE, 1, 8, 16, 8];
        assert!(Font::parse(&face(&short, &glyphs, table)).is_err());
        let padded = [32, HAS_UNICODE_TABLE, 1, 32, 16, 8];
        assert!(Font::parse(&face(&padded, &[0u8; 32], table)).is_err());
        // Bitmaps shorter than the header claims.
        assert!(Font::parse(&face(&good, &[0u8; 4], table)).is_err());
        // A table naming fewer or more glyphs than the header.
        assert!(Font::parse(&face(&good, &glyphs, b"")).is_err());
        assert!(Font::parse(&face(&good, &glyphs, b" \xffB\xff")).is_err());
        // A face with no space and no replacement has no fallback to draw.
        assert!(Font::parse(&face(&good, &glyphs, b"B\xff")).is_err());
        // Invalid UTF-8 in the table.
        assert!(Font::parse(&face(&good, &glyphs, b"\xc3\xff")).is_err());
    }

    #[test]
    fn a_table_longer_than_its_entries_is_refused() {
        let header = [32, HAS_UNICODE_TABLE, 1, 16, 16, 8];
        let glyphs = [0u8; 16];
        assert!(Font::parse(&face(&header, &glyphs, b" \xff")).is_ok());
        assert!(Font::parse(&face(&header, &glyphs, b" \xffZ")).is_err());
    }

    #[test]
    fn a_multi_scalar_sequence_is_skipped_without_losing_the_entry() {
        let header = [32, HAS_UNICODE_TABLE, 2, 16, 16, 8];
        let glyphs = [0u8; 32];
        // Glyph 0 is ' ' plus a sequence; glyph 1 is 'B'.
        // PSF2 puts standalone scalars FIRST, then each sequence introduced
        // by 0xFE, through to 0xFF -- so everything after the first 0xFE in an
        // entry is sequence text and must not be mapped as a standalone.
        let table = b" \xfeAB\xfeCD\xffB\xff";
        let font = Font::parse(&face(&header, &glyphs, table)).unwrap();
        assert_eq!(font.glyph_count(), 2);
        assert_eq!(font.index(' '), 0);
        assert_eq!(font.index('B'), 1);
        // 'A', 'C' and 'D' appear only inside sequences, so none is claimed.
        assert!(!font.covers('A'));
        assert!(!font.covers('C'));
        assert!(!font.covers('D'));
        // Skipped does not mean unchecked: invalid UTF-8 inside a sequence is
        // still refused.
        assert!(Font::parse(&face(&header, &glyphs, b" \xfe\xc3\xffB\xff")).is_err());
    }
}

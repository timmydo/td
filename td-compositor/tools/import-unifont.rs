#![deny(unsafe_code)]

//! Converts GNU Unifont's `.hex` release into the single PSF2 face section 11
//! pins. Upstream ships no full-coverage PSF2 -- only an APL-specific PSF1 --
//! so the committed asset is derived here rather than downloaded, and this
//! tool is what makes that derivation reproducible from a hash-pinned input.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

#[allow(dead_code)]
#[path = "../../engine/src/sha256.rs"]
mod sha256;

const RELEASE: &str = "unifont-16.0.04";
const ARCHIVE_URL: &str =
    "https://ftp.gnu.org/gnu/unifont/unifont-16.0.04/unifont_all-16.0.04.hex.gz";
/// SHA-256 of the gzip as published. The importer reads the DECOMPRESSED hex,
/// because a dependency-free gunzip is not this tool's job; `HEX_SHA256` is
/// what it verifies, and both are recorded so the chain back to GNU is whole.
const ARCHIVE_SHA256: &str = "20e8b505f602488697979eefc69857f7f6106bceab702f5ac559f4f84e0e7494";
const HEX_SHA256: &str = "730472c02a75c8c7dd6e6a089d8d10f32c32b99f465f43d9992d8b12c4af640b";
const ASSET_FILE: &str = "unifont-16.0.04-8x16.psf2";

/// Single-width cells only: section 13 makes double-width a deliberate
/// first-profile exclusion, and PSF2 has one fixed cell for every glyph, so a
/// face carrying Unifont's 16x16 glyphs could not describe them anyway.
const GLYPH_WIDTH: usize = 8;
const GLYPH_HEIGHT: usize = 16;
const GLYPH_BYTES: usize = GLYPH_HEIGHT * GLYPH_WIDTH.div_ceil(8);
/// Hex digits in a single-width `.hex` record's bitmap field.
const SINGLE_WIDTH_DIGITS: usize = GLYPH_BYTES * 2;

const PSF2_MAGIC: [u8; 4] = [0x72, 0xb5, 0x4a, 0x86];
const PSF2_HEADER_BYTES: u32 = 32;
const PSF2_HAS_UNICODE_TABLE: u32 = 1;
const PSF2_SEPARATOR: u8 = 0xff;

const MAX_HEX_BYTES: u64 = 64 * 1024 * 1024;
const MAX_CODEPOINT: u32 = 0x10_ffff;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("td-term-import-unifont: {error}");
            ExitCode::FAILURE
        }
    }
}

fn usage() -> String {
    format!(
        "usage: td-term-import-unifont <unifont_all-*.hex> <out-dir>\n  \
         expects the decompressed hex of {ARCHIVE_URL}"
    )
}

fn run() -> Result<(), String> {
    let mut arguments = std::env::args_os().skip(1);
    let Some(hex_path) = arguments.next() else {
        return Err(usage());
    };
    let Some(out_dir) = arguments.next() else {
        return Err(usage());
    };
    if arguments.next().is_some() {
        return Err(usage());
    }
    let hex_path = PathBuf::from(hex_path);
    let out_dir = PathBuf::from(out_dir);
    let bytes = read_input(&hex_path)?;
    let digest = sha256::hex_digest(&bytes);
    if !HEX_SHA256.is_empty() && digest != HEX_SHA256 {
        return Err(format!(
            "{} is not the pinned {RELEASE} hex: expected {HEX_SHA256}, read {digest}",
            hex_path.display()
        ));
    }
    let text = String::from_utf8(bytes)
        .map_err(|_| format!("{} is not UTF-8", hex_path.display()))?;
    let glyphs = parse_hex(&text)?;
    let face = encode_psf2(&glyphs)?;
    let out = out_dir.join(ASSET_FILE);
    write_atomically(&out, &face)?;
    println!(
        "{ASSET_FILE}: {} single-width glyphs, {} bytes, sha256 {}",
        glyphs.len(),
        face.len(),
        sha256::hex_digest(&face)
    );
    println!("  derived from {ARCHIVE_URL}");
    println!("  archive sha256 {ARCHIVE_SHA256}");
    println!("  hex sha256 {digest}");
    Ok(())
}

fn read_input(path: &Path) -> Result<Vec<u8>, String> {
    let metadata =
        std::fs::metadata(path).map_err(|e| format!("stat {}: {e}", path.display()))?;
    if metadata.len() > MAX_HEX_BYTES {
        return Err(format!(
            "{} is {} bytes, above the {MAX_HEX_BYTES}-byte ceiling",
            path.display(),
            metadata.len()
        ));
    }
    std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))
}

/// One `.hex` record is `CODEPOINT:BITMAP`. Double-width records carry twice
/// the digits and are skipped rather than rejected, since the upstream file
/// mixes both and only the single-width half is this face.
fn parse_hex(text: &str) -> Result<BTreeMap<u32, [u8; GLYPH_BYTES]>, String> {
    let mut glyphs = BTreeMap::new();
    for (offset, raw) in text.lines().enumerate() {
        let line = offset.saturating_add(1);
        let record = raw.trim_end_matches(['\r', '\n']);
        if record.is_empty() {
            continue;
        }
        let Some((codepoint, bitmap)) = record.split_once(':') else {
            return Err(format!("hex:{line}: expected CODEPOINT:BITMAP"));
        };
        if bitmap.len() != SINGLE_WIDTH_DIGITS {
            if bitmap.len() % 2 != 0 || bitmap.is_empty() {
                return Err(format!("hex:{line}: bitmap has {} digits", bitmap.len()));
            }
            continue;
        }
        let codepoint = parse_codepoint(codepoint, line)?;
        let mut glyph = [0u8; GLYPH_BYTES];
        for (index, slot) in glyph.iter_mut().enumerate() {
            let at = index.saturating_mul(2);
            let pair = bitmap
                .get(at..at.saturating_add(2))
                .ok_or_else(|| format!("hex:{line}: bitmap truncated"))?;
            *slot = u8::from_str_radix(pair, 16)
                .map_err(|_| format!("hex:{line}: '{pair}' is not a hex byte"))?;
        }
        if glyphs.insert(codepoint, glyph).is_some() {
            return Err(format!("hex:{line}: duplicate codepoint U+{codepoint:04X}"));
        }
    }
    if glyphs.is_empty() {
        return Err("hex carried no single-width glyphs".to_string());
    }
    Ok(glyphs)
}

fn parse_codepoint(text: &str, line: usize) -> Result<u32, String> {
    if text.is_empty() || text.len() > 6 || !text.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!("hex:{line}: '{text}' is not a codepoint"));
    }
    let value = u32::from_str_radix(text, 16)
        .map_err(|_| format!("hex:{line}: '{text}' is not a codepoint"))?;
    if value > MAX_CODEPOINT {
        return Err(format!("hex:{line}: U+{value:04X} is above Unicode"));
    }
    // A surrogate is not a scalar, so it can never be asked for at render time
    // and a table entry for one would be unreachable weight.
    if (0xd800..=0xdfff).contains(&value) {
        return Err(format!("hex:{line}: U+{value:04X} is a surrogate"));
    }
    Ok(value)
}

fn encode_psf2(glyphs: &BTreeMap<u32, [u8; GLYPH_BYTES]>) -> Result<Vec<u8>, String> {
    let count = u32::try_from(glyphs.len()).map_err(|_| "too many glyphs".to_string())?;
    let charsize = u32::try_from(GLYPH_BYTES).map_err(|_| "glyph too large".to_string())?;
    let height = u32::try_from(GLYPH_HEIGHT).map_err(|_| "glyph too tall".to_string())?;
    let width = u32::try_from(GLYPH_WIDTH).map_err(|_| "glyph too wide".to_string())?;
    let mut out = Vec::new();
    out.extend_from_slice(&PSF2_MAGIC);
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&PSF2_HEADER_BYTES.to_le_bytes());
    out.extend_from_slice(&PSF2_HAS_UNICODE_TABLE.to_le_bytes());
    out.extend_from_slice(&count.to_le_bytes());
    out.extend_from_slice(&charsize.to_le_bytes());
    out.extend_from_slice(&height.to_le_bytes());
    out.extend_from_slice(&width.to_le_bytes());
    for glyph in glyphs.values() {
        out.extend_from_slice(glyph);
    }
    // The table is in glyph order and each entry names exactly the one scalar
    // that selects it, so a reader can invert it without ambiguity.
    let mut buffer = [0u8; 4];
    for codepoint in glyphs.keys() {
        let scalar = char::from_u32(*codepoint)
            .ok_or_else(|| format!("U+{codepoint:04X} is not a scalar"))?;
        out.extend_from_slice(scalar.encode_utf8(&mut buffer).as_bytes());
        out.push(PSF2_SEPARATOR);
    }
    Ok(out)
}

fn write_atomically(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("{} has no parent", path.display()))?;
    std::fs::create_dir_all(parent)
        .map_err(|e| format!("create {}: {e}", parent.display()))?;
    let temporary = path.with_extension("psf2.tmp");
    let mut file = std::fs::File::create(&temporary)
        .map_err(|e| format!("create {}: {e}", temporary.display()))?;
    file.write_all(bytes)
        .map_err(|e| format!("write {}: {e}", temporary.display()))?;
    file.sync_all()
        .map_err(|e| format!("sync {}: {e}", temporary.display()))?;
    drop(file);
    std::fs::rename(&temporary, path)
        .map_err(|e| format!("rename into {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "0041:00183C7EFFFF7E3C1800000000000000\n";

    fn single(codepoint: &str, fill: u8) -> String {
        let digits: String = (0..SINGLE_WIDTH_DIGITS)
            .map(|_| char::from(b'0' + fill))
            .collect();
        format!("{codepoint}:{digits}\n")
    }

    #[test]
    fn double_width_records_are_skipped_not_rejected() {
        let wide = format!("4E00:{}\n", "1".repeat(SINGLE_WIDTH_DIGITS * 2));
        let text = format!("{}{wide}", single("0041", 1));
        let glyphs = parse_hex(&text).unwrap();
        assert_eq!(glyphs.len(), 1);
        assert!(glyphs.contains_key(&0x41));
        assert!(!glyphs.contains_key(&0x4e00));
    }

    #[test]
    fn a_malformed_record_reds_rather_than_being_skipped() {
        assert!(parse_hex("0041\n").is_err());
        assert!(parse_hex(&format!("0041:{}\n", "1".repeat(31))).is_err());
        assert!(parse_hex("").is_err());
        assert!(parse_hex(&single("D800", 1)).is_err());
        assert!(parse_hex(&single("110000", 1)).is_err());
        assert!(parse_hex(&single("ZZZZ", 1)).is_err());
        let duplicate = format!("{}{}", single("0041", 1), single("0041", 2));
        assert!(parse_hex(&duplicate).is_err());
    }

    #[test]
    fn the_sample_record_decodes_to_its_bitmap() {
        let glyphs = parse_hex(SAMPLE).unwrap();
        let glyph = glyphs.get(&0x41).unwrap();
        assert_eq!(glyph.len(), GLYPH_BYTES);
        assert_eq!(glyph[0], 0x00);
        assert_eq!(glyph[1], 0x18);
        assert_eq!(glyph[3], 0x7e);
        assert_eq!(glyph[4], 0xff);
        assert_eq!(glyph[15], 0x00);
    }

    #[test]
    fn the_header_and_table_describe_the_glyphs_that_follow() {
        let text = format!("{}{}", single("0041", 1), single("00E9", 2));
        let glyphs = parse_hex(&text).unwrap();
        let face = encode_psf2(&glyphs).unwrap();
        assert_eq!(face.get(..4).unwrap(), PSF2_MAGIC);
        let header = |at: usize| -> u32 {
            let raw: [u8; 4] = face.get(at..at + 4).unwrap().try_into().unwrap();
            u32::from_le_bytes(raw)
        };
        assert_eq!(header(4), 0, "version");
        assert_eq!(header(8), PSF2_HEADER_BYTES);
        assert_eq!(header(12), PSF2_HAS_UNICODE_TABLE);
        assert_eq!(header(16), 2, "glyph count");
        assert_eq!(header(20), GLYPH_BYTES as u32);
        assert_eq!(header(24), GLYPH_HEIGHT as u32);
        assert_eq!(header(28), GLYPH_WIDTH as u32);
        let bitmaps = PSF2_HEADER_BYTES as usize;
        let table = bitmaps + 2 * GLYPH_BYTES;
        // 'A' is one UTF-8 byte plus a separator; 'e-acute' is two plus one.
        assert_eq!(face.len(), table + 2 + 3);
        assert_eq!(face.get(table..table + 2).unwrap(), b"A\xff");
        let mut tail = "é".as_bytes().to_vec();
        tail.push(PSF2_SEPARATOR);
        assert_eq!(face.get(table + 2..).unwrap(), tail.as_slice());
    }
}

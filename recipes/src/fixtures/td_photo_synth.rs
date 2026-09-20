#![deny(unsafe_code)]

//! The synthetic Nikon frame td-photo-test develops in the built td-photo:
//! an uncompressed 14-bit raw sub-image behind the IFD shape the reader
//! expects, with a maker note carrying the white balance, black level and
//! crop, a Z 8 by make and model so the camera table answers, and a bare
//! SOI/EOI embedded preview. The byte layout is the uncompressed writer of
//! td-photo/tests/support/synth_nef.rs restated over literal tags, so the
//! fixture compiles alone on the target toolchain with no crate beside it.
//! It is a fixture of the recipe check, not a shipped program.

use std::env;
use std::fs;
use std::process::ExitCode;

const WIDTH: usize = 64;
const HEIGHT: usize = 48;

// TIFF field kinds and the tags the frame carries, as td-photo's `tiff`
// module spells them.
const BYTE: u16 = 1;
const ASCII: u16 = 2;
const SHORT: u16 = 3;
const LONG: u16 = 4;
const RATIONAL: u16 = 5;
const UNDEFINED: u16 = 7;
const NEW_SUBFILE_TYPE: u16 = 254;
const IMAGE_WIDTH: u16 = 256;
const IMAGE_LENGTH: u16 = 257;
const BITS_PER_SAMPLE: u16 = 258;
const COMPRESSION: u16 = 259;
const PHOTOMETRIC: u16 = 262;
const MAKE: u16 = 271;
const MODEL: u16 = 272;
const STRIP_OFFSETS: u16 = 273;
const ORIENTATION: u16 = 274;
const SAMPLES_PER_PIXEL: u16 = 277;
const ROWS_PER_STRIP: u16 = 278;
const STRIP_BYTE_COUNTS: u16 = 279;
const SUB_IFDS: u16 = 330;
const JPEG_OFFSET: u16 = 513;
const JPEG_LENGTH: u16 = 514;
const CFA_REPEAT_DIM: u16 = 33421;
const CFA_PATTERN: u16 = 33422;
const EXIF_IFD: u16 = 34665;
const MAKER_NOTE: u16 = 37500;
const NIKON_WB_RB_LEVELS: u16 = 0x000c;
const NIKON_BLACK_LEVEL: u16 = 0x003d;
const NIKON_CROP_AREA: u16 = 0x0045;
const COMPRESSION_NONE: u16 = 1;
const PHOTOMETRIC_CFA: u16 = 32803;

enum Value {
    Short(Vec<u16>),
    Long(Vec<u32>),
    Rational(Vec<(u32, u32)>),
    Ascii(&'static str),
    Byte(Vec<u8>),
    Undefined(Vec<u8>),
}

fn count_of(len: usize) -> Result<u32, String> {
    u32::try_from(len).map_err(|_| format!("{len} is beyond a TIFF count"))
}

/// A value's field kind, count and little-endian bytes.
fn encode(value: &Value) -> Result<(u16, u32, Vec<u8>), String> {
    Ok(match value {
        Value::Short(v) => (
            SHORT,
            count_of(v.len())?,
            v.iter().flat_map(|x| x.to_le_bytes()).collect(),
        ),
        Value::Long(v) => (
            LONG,
            count_of(v.len())?,
            v.iter().flat_map(|x| x.to_le_bytes()).collect(),
        ),
        Value::Rational(v) => (
            RATIONAL,
            count_of(v.len())?,
            v.iter()
                .flat_map(|(n, d)| [n.to_le_bytes(), d.to_le_bytes()].concat())
                .collect(),
        ),
        Value::Ascii(s) => {
            let mut bytes = s.as_bytes().to_vec();
            bytes.push(0);
            (ASCII, count_of(bytes.len())?, bytes)
        }
        Value::Byte(v) => (BYTE, count_of(v.len())?, v.clone()),
        Value::Undefined(v) => (UNDEFINED, count_of(v.len())?, v.clone()),
    })
}

/// A little-endian TIFF under construction: blobs and IFDs appended in
/// order, the first-IFD pointer patched at the end.
struct Builder {
    data: Vec<u8>,
}

impl Builder {
    fn new() -> Self {
        Self {
            data: b"II\x2a\x00\x00\x00\x00\x00".to_vec(),
        }
    }

    fn blob(&mut self, bytes: &[u8]) -> Result<u32, String> {
        if self.data.len() % 2 != 0 {
            self.data.push(0);
        }
        let offset = count_of(self.data.len())?;
        self.data.extend_from_slice(bytes);
        Ok(offset)
    }

    /// Writes an IFD from `entries`, given in ascending tag order as TIFF
    /// requires; this builder does not sort them.
    fn ifd(&mut self, entries: &[(u16, Value)]) -> Result<u32, String> {
        let mut fields = Vec::with_capacity(entries.len());
        for (tag, value) in entries {
            let (kind, count, bytes) = encode(value)?;
            let mut field = [0u8; 4];
            if bytes.len() <= 4 {
                for (slot, byte) in field.iter_mut().zip(&bytes) {
                    *slot = *byte;
                }
            } else {
                field = self.blob(&bytes)?.to_le_bytes();
            }
            fields.push((*tag, kind, count, field));
        }
        let mut table = Vec::with_capacity(2 + fields.len() * 12 + 4);
        let entries = u16::try_from(fields.len()).map_err(|_| "too many IFD entries")?;
        table.extend_from_slice(&entries.to_le_bytes());
        for (tag, kind, count, field) in &fields {
            table.extend_from_slice(&tag.to_le_bytes());
            table.extend_from_slice(&kind.to_le_bytes());
            table.extend_from_slice(&count.to_le_bytes());
            table.extend_from_slice(field);
        }
        table.extend_from_slice(&0u32.to_le_bytes());
        self.blob(&table)
    }

    fn set_first(&mut self, offset: u32) -> Result<(), String> {
        let slot = self
            .data
            .get_mut(4..8)
            .ok_or("the header is shorter than its own pointer")?;
        slot.copy_from_slice(&offset.to_le_bytes());
        Ok(())
    }
}

/// The frame: a gradient of samples above the black level, one 14-bit
/// photosite each, little-endian in the strip.
fn frame() -> Result<Vec<u8>, String> {
    let mut strip = Vec::with_capacity(WIDTH * HEIGHT * 2);
    for i in 0..WIDTH * HEIGHT {
        let sample = 1008 + u16::try_from(i % 4000).map_err(|_| "sample beyond 16 bits")?;
        strip.extend_from_slice(&sample.to_le_bytes());
    }
    let width = count_of(WIDTH)?;
    let height = count_of(HEIGHT)?;
    let crop_width = u16::try_from(WIDTH - 4).map_err(|_| "crop width beyond 16 bits")?;
    let crop_height = u16::try_from(HEIGHT - 4).map_err(|_| "crop height beyond 16 bits")?;

    let mut b = Builder::new();
    let strip_offset = b.blob(&strip)?;
    // A bare SOI/EOI: begins with SOI so the reader accepts the entry, but
    // decodes to no image, so the thumbnail rule skips it.
    let preview = b.blob(&[0xff, 0xd8, 0xff, 0xd9])?;

    // The maker note: its own little TIFF behind the ten-byte Nikon prefix.
    let mut inner = Builder::new();
    let wb = [(1019u32, 512u32), (793, 512), (512, 512), (512, 512)];
    let note_ifd = inner.ifd(&[
        (NIKON_WB_RB_LEVELS, Value::Rational(wb.to_vec())),
        (NIKON_BLACK_LEVEL, Value::Short(vec![1008; 4])),
        (
            NIKON_CROP_AREA,
            Value::Short(vec![2, 2, crop_width, crop_height]),
        ),
    ])?;
    inner.set_first(note_ifd)?;
    let mut note = b"Nikon\0\x02\x11\0\0".to_vec();
    note.extend_from_slice(&inner.data);
    let exif = b.ifd(&[(MAKER_NOTE, Value::Undefined(note))])?;

    let raw = b.ifd(&[
        (NEW_SUBFILE_TYPE, Value::Long(vec![0])),
        (IMAGE_WIDTH, Value::Long(vec![width])),
        (IMAGE_LENGTH, Value::Long(vec![height])),
        (BITS_PER_SAMPLE, Value::Short(vec![14])),
        (COMPRESSION, Value::Short(vec![COMPRESSION_NONE])),
        (PHOTOMETRIC, Value::Short(vec![PHOTOMETRIC_CFA])),
        (STRIP_OFFSETS, Value::Long(vec![strip_offset])),
        (SAMPLES_PER_PIXEL, Value::Short(vec![1])),
        (ROWS_PER_STRIP, Value::Long(vec![height])),
        (STRIP_BYTE_COUNTS, Value::Long(vec![count_of(strip.len())?])),
        (CFA_REPEAT_DIM, Value::Short(vec![2, 2])),
        (CFA_PATTERN, Value::Byte(vec![0, 1, 1, 2])),
    ])?;
    let small = b.ifd(&[
        (NEW_SUBFILE_TYPE, Value::Long(vec![1])),
        (JPEG_OFFSET, Value::Long(vec![preview])),
        (JPEG_LENGTH, Value::Long(vec![4])),
    ])?;
    let ifd0 = b.ifd(&[
        (NEW_SUBFILE_TYPE, Value::Long(vec![1])),
        (MAKE, Value::Ascii("NIKON CORPORATION")),
        (MODEL, Value::Ascii("NIKON Z 8")),
        (ORIENTATION, Value::Short(vec![1])),
        (SUB_IFDS, Value::Long(vec![small, raw])),
        (EXIF_IFD, Value::Long(vec![exif])),
    ])?;
    b.set_first(ifd0)?;
    Ok(b.data)
}

fn main() -> ExitCode {
    let Some(out) = env::args_os().nth(1) else {
        eprintln!("usage: td-photo-synth OUT.NEF");
        return ExitCode::FAILURE;
    };
    match frame().and_then(|bytes| fs::write(&out, bytes).map_err(|e| e.to_string())) {
        Ok(()) => ExitCode::SUCCESS,
        Err(why) => {
            eprintln!("td-photo-synth: {why}");
            ExitCode::FAILURE
        }
    }
}

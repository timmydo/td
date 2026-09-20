//! A minimal decodable Nikon NEF written in memory for the develop tests:
//! an uncompressed (`COMPRESSION_NONE`) raw sub-image behind the IFD shape
//! the reader expects, with a maker note carrying the white balance, black
//! level and crop, so a developed frame is fully determined without the
//! Huffman codec the `nef` round-trip tests exercise. It is a td-photo Z 8
//! by make and model, so the camera table answers. The embedded preview is
//! a bare SOI/EOI pair, which the thumbnail rule skips, so a cull grid box
//! keeps its placeholder; only the develop view shows pixels.
//!
//! The byte layout mirrors `tests/nef.rs`'s synthetic writer; this one is
//! the uncompressed subset and its own small TIFF builder, so it needs
//! neither the Huffman encoder nor a linearization table.

use td_photo::tiff::{kind, tag};

const COMPRESSION_NONE: u16 = 1;
const PHOTOMETRIC_CFA: u16 = 32803;

fn u16_bytes(v: u16) -> [u8; 2] {
    v.to_le_bytes()
}

fn u32_bytes(v: u32) -> [u8; 4] {
    v.to_le_bytes()
}

enum Value {
    Short(Vec<u16>),
    Long(Vec<u32>),
    Rational(Vec<(u32, u32)>),
    Ascii(&'static str),
    Byte(Vec<u8>),
    Undefined(Vec<u8>),
}

fn encode_value(value: &Value) -> (u16, u32, Vec<u8>) {
    match value {
        Value::Short(v) => (
            kind::SHORT,
            v.len() as u32,
            v.iter().flat_map(|x| u16_bytes(*x)).collect(),
        ),
        Value::Long(v) => (
            kind::LONG,
            v.len() as u32,
            v.iter().flat_map(|x| u32_bytes(*x)).collect(),
        ),
        Value::Rational(v) => (
            kind::RATIONAL,
            v.len() as u32,
            v.iter()
                .flat_map(|(n, d)| [u32_bytes(*n), u32_bytes(*d)].concat())
                .collect(),
        ),
        Value::Ascii(s) => {
            let mut bytes = s.as_bytes().to_vec();
            bytes.push(0);
            (kind::ASCII, bytes.len() as u32, bytes)
        }
        Value::Byte(v) => (kind::BYTE, v.len() as u32, v.clone()),
        Value::Undefined(v) => (kind::UNDEFINED, v.len() as u32, v.clone()),
    }
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

    fn blob(&mut self, bytes: &[u8]) -> u32 {
        if !self.data.len().is_multiple_of(2) {
            self.data.push(0);
        }
        let offset = self.data.len() as u32;
        self.data.extend_from_slice(bytes);
        offset
    }

    /// Writes an IFD from `entries`, which the caller passes in ascending tag
    /// order, as TIFF requires; this builder does not sort them.
    fn ifd(&mut self, entries: &[(u16, Value)]) -> u32 {
        let mut fields = Vec::new();
        for (tag, value) in entries {
            let (kind, count, bytes) = encode_value(value);
            let field = if bytes.len() <= 4 {
                let mut inline = [0u8; 4];
                inline[..bytes.len()].copy_from_slice(&bytes);
                inline
            } else {
                u32_bytes(self.blob(&bytes))
            };
            fields.push((*tag, kind, count, field));
        }
        let mut table = Vec::new();
        table.extend_from_slice(&u16_bytes(fields.len() as u16));
        for (tag, kind, count, field) in &fields {
            table.extend_from_slice(&u16_bytes(*tag));
            table.extend_from_slice(&u16_bytes(*kind));
            table.extend_from_slice(&u32_bytes(*count));
            table.extend_from_slice(field);
        }
        table.extend_from_slice(&u32_bytes(0));
        self.blob(&table)
    }

    fn set_first(&mut self, offset: u32) {
        self.data[4..8].copy_from_slice(&u32_bytes(offset));
    }

    fn finish(self) -> Vec<u8> {
        self.data
    }
}

/// A decodable uncompressed NEF of `width` by `height` from `samples`
/// (row-major, one 14-bit photosite each, little-endian in the strip).
/// `width` and `height` must be even and at least 8, so the 2x2 CFA and the
/// two-pixel crop margin stay whole and superpixel has rows to work on.
pub fn uncompressed_nef(width: usize, height: usize, samples: &[u16]) -> Vec<u8> {
    // A bare SOI/EOI: begins with SOI so the reader accepts the entry, but
    // decodes to no image, so the thumbnail is skipped and the grid box
    // keeps its placeholder.
    nef_with_preview(width, height, samples, &[0xff, 0xd8, 0xff, 0xd9])
}

/// `uncompressed_nef` with `jpeg` as the embedded preview: a baseline
/// JPEG here is the thumbnail the window shows.
pub fn nef_with_preview(width: usize, height: usize, samples: &[u16], jpeg: &[u8]) -> Vec<u8> {
    // Even and at least 8 so the 2x2 CFA and the crop stay whole, and within
    // u16 so the crop entry below does not truncate.
    assert!(width >= 8 && height >= 8 && width.is_multiple_of(2) && height.is_multiple_of(2));
    assert!(width <= u16::MAX as usize && height <= u16::MAX as usize);
    assert_eq!(samples.len(), width * height, "one sample per photosite");

    let mut b = Builder::new();
    let mut strip_bytes = Vec::with_capacity(samples.len() * 2);
    for sample in samples {
        strip_bytes.extend_from_slice(&u16_bytes(*sample));
    }
    let strip = b.blob(&strip_bytes);
    let preview = b.blob(jpeg);

    // The maker note: its own little TIFF behind the ten-byte Nikon prefix.
    let mut inner = Builder::new();
    let wb = [(1019u32, 512u32), (793, 512), (512, 512), (512, 512)];
    let note_ifd = inner.ifd(&[
        (tag::NIKON_WB_RB_LEVELS, Value::Rational(wb.to_vec())),
        (tag::NIKON_BLACK_LEVEL, Value::Short(vec![1008; 4])),
        (
            tag::NIKON_CROP_AREA,
            Value::Short(vec![2, 2, width as u16 - 4, height as u16 - 4]),
        ),
    ]);
    inner.set_first(note_ifd);
    let mut note = b"Nikon\0\x02\x11\0\0".to_vec();
    note.extend_from_slice(&inner.finish());
    let exif = b.ifd(&[(tag::MAKER_NOTE, Value::Undefined(note))]);

    let raw = b.ifd(&[
        (tag::NEW_SUBFILE_TYPE, Value::Long(vec![0])),
        (tag::IMAGE_WIDTH, Value::Long(vec![width as u32])),
        (tag::IMAGE_LENGTH, Value::Long(vec![height as u32])),
        (tag::BITS_PER_SAMPLE, Value::Short(vec![14])),
        (tag::COMPRESSION, Value::Short(vec![COMPRESSION_NONE])),
        (tag::PHOTOMETRIC, Value::Short(vec![PHOTOMETRIC_CFA])),
        (tag::STRIP_OFFSETS, Value::Long(vec![strip])),
        (tag::SAMPLES_PER_PIXEL, Value::Short(vec![1])),
        (tag::ROWS_PER_STRIP, Value::Long(vec![height as u32])),
        (
            tag::STRIP_BYTE_COUNTS,
            Value::Long(vec![strip_bytes.len() as u32]),
        ),
        (tag::CFA_REPEAT_DIM, Value::Short(vec![2, 2])),
        (tag::CFA_PATTERN, Value::Byte(vec![0, 1, 1, 2])),
    ]);
    let small = b.ifd(&[
        (tag::NEW_SUBFILE_TYPE, Value::Long(vec![1])),
        (tag::JPEG_OFFSET, Value::Long(vec![preview])),
        (tag::JPEG_LENGTH, Value::Long(vec![jpeg.len() as u32])),
    ]);
    let ifd0 = b.ifd(&[
        (tag::NEW_SUBFILE_TYPE, Value::Long(vec![1])),
        (tag::MAKE, Value::Ascii("NIKON CORPORATION")),
        (tag::MODEL, Value::Ascii("NIKON Z 8")),
        (tag::ORIENTATION, Value::Short(vec![1])),
        (tag::SUB_IFDS, Value::Long(vec![small, raw])),
        (tag::EXIF_IFD, Value::Long(vec![exif])),
    ]);
    b.set_first(ifd0);
    b.finish()
}

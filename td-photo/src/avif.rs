//! The AVIF container for the export: one AV1 still picture as a HEIF
//! image item (ISO/IEC 23008-12 with the AV1 ISOBMFF and AVIF
//! bindings), the least file the format asks for and every reader
//! expects. `av1` makes the picture; this wraps its OBUs.

use crate::av1::{self, Geometry};

/// The one item's identifier.
const ITEM: u16 = 1;
/// The last `seq_level_idx` the AVIF Baseline profile (`MA1B`: the
/// main profile at level 5.1 or under) admits. The Advanced profile
/// names the high profile, which this stream is not, so past Baseline
/// no profile brand is claimed: a brand is optional and a claim.
const BASELINE_LEVEL: u32 = 13;
/// The mdat header's bytes: its size and type, before the item's data.
const MDAT_HEADER: usize = 8;

/// The boxes as bytes are put together: big-endian fields inside
/// size-prefixed boxes.
struct Boxes(Vec<u8>);

impl Boxes {
    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }

    fn u16(&mut self, v: u16) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }

    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }

    fn bytes(&mut self, v: &[u8]) {
        self.0.extend_from_slice(v);
    }

    /// A box: its size and type, then what `body` writes.
    fn boxed(&mut self, kind: &[u8; 4], body: impl FnOnce(&mut Boxes)) {
        let start = self.0.len();
        self.u32(0);
        self.bytes(kind);
        body(self);
        let size = u32::try_from(self.0.len() - start).unwrap_or(u32::MAX);
        if let Some(slot) = self.0.get_mut(start..start + 4) {
            slot.copy_from_slice(&size.to_be_bytes());
        }
    }

    /// A full box: its `version` with no flags, then the payload.
    fn full(&mut self, kind: &[u8; 4], version: u8, body: impl FnOnce(&mut Boxes)) {
        self.boxed(kind, |b| {
            b.u32(u32::from(version) << 24);
            body(b);
        });
    }
}

/// The file: `ftyp` (the `avif` brand, `mif1` and `miaf`, and the AVIF
/// profile the level admits, if one), `meta` describing the one `av01`
/// item (its size, its three 8-bit channels, the AV1 configuration and
/// the colour, all as `av1` codes them), then `mdat` holding `obus`,
/// the sequence header and frame OBUs `av1::Encoder::finish` gives.
/// Sizes and offsets are four bytes: a frame is at most `av1::MAX_AXIS`
/// square, whose stream is well under them.
pub fn file(geometry: &Geometry, obus: &[u8]) -> Vec<u8> {
    let mut out = Boxes(Vec::with_capacity(obus.len() + 512));
    out.boxed(b"ftyp", |b| {
        b.bytes(b"avif");
        b.u32(0);
        for brand in [b"avif", b"mif1", b"miaf"] {
            b.bytes(brand);
        }
        if geometry.level() <= BASELINE_LEVEL {
            b.bytes(b"MA1B");
        }
    });
    // The item's offset is where the data follows the meta box and the
    // mdat header; the meta box's length does not depend on it (four
    // bytes either way), so a first write measures the layout and the
    // second carries the offset.
    let meta_at = out.0.len();
    meta(&mut out, geometry, 0, obus.len());
    let data_at = out.0.len() + MDAT_HEADER;
    out.0.truncate(meta_at);
    meta(&mut out, geometry, data_at, obus.len());
    out.boxed(b"mdat", |b| b.bytes(obus));
    out.0
}

fn meta(out: &mut Boxes, geometry: &Geometry, offset: usize, length: usize) {
    let (offset, length) = (
        u32::try_from(offset).unwrap_or(u32::MAX),
        u32::try_from(length).unwrap_or(u32::MAX),
    );
    let (width, height) = (
        u32::try_from(geometry.width()).unwrap_or(u32::MAX),
        u32::try_from(geometry.height()).unwrap_or(u32::MAX),
    );
    out.full(b"meta", 0, |b| {
        b.full(b"hdlr", 0, |b| {
            b.u32(0);
            b.bytes(b"pict");
            b.u32(0);
            b.u32(0);
            b.u32(0);
            b.u8(0);
        });
        b.full(b"pitm", 0, |b| b.u16(ITEM));
        b.full(b"iloc", 0, |b| {
            // offset_size 4 and length_size 4, base_offset_size 0; one
            // item in the file itself, one extent.
            b.u8(0x44);
            b.u8(0);
            b.u16(1);
            b.u16(ITEM);
            b.u16(0);
            b.u16(1);
            b.u32(offset);
            b.u32(length);
        });
        b.full(b"iinf", 0, |b| {
            b.u16(1);
            b.full(b"infe", 2, |b| {
                b.u16(ITEM);
                b.u16(0);
                b.bytes(b"av01");
                b.u8(0);
            });
        });
        b.boxed(b"iprp", |b| {
            b.boxed(b"ipco", |b| {
                b.full(b"ispe", 0, |b| {
                    b.u32(width);
                    b.u32(height);
                });
                b.full(b"pixi", 0, |b| {
                    b.u8(3);
                    b.bytes(&[8, 8, 8]);
                });
                b.boxed(b"av1C", |b| b.bytes(&av1::av1c(geometry)));
                b.boxed(b"colr", |b| {
                    b.bytes(b"nclx");
                    b.u16(u16::from(av1::COLOUR_PRIMARIES));
                    b.u16(u16::from(av1::TRANSFER_CHARACTERISTICS));
                    b.u16(u16::from(av1::MATRIX_COEFFICIENTS));
                    b.u8(if av1::FULL_RANGE { 0x80 } else { 0 });
                });
            });
            b.full(b"ipma", 0, |b| {
                b.u32(1);
                b.u16(ITEM);
                b.u8(4);
                // ispe, pixi, av1C (essential), colr: the properties
                // in ipco order, from one.
                b.bytes(&[1, 2, 0x83, 4]);
            });
        });
    });
}

#[cfg(test)]
mod tests {
    #![allow(clippy::indexing_slicing, clippy::unwrap_used)]

    use super::*;

    /// The boxes of `data`, each as its type and payload, at one level.
    fn boxes(data: &[u8]) -> Vec<(&[u8], &[u8])> {
        let mut out = Vec::new();
        let mut at = 0;
        while at < data.len() {
            let size = u32::from_be_bytes(data[at..at + 4].try_into().unwrap()) as usize;
            assert!(size >= 8 && at + size <= data.len(), "box at {at}: {size}");
            out.push((&data[at + 4..at + 8], &data[at + 8..at + size]));
            at += size;
        }
        out
    }

    #[test]
    fn the_file_is_the_documented_boxes_around_the_obus() {
        let geometry = Geometry::new(300, 70).unwrap();
        let obus = b"OBUS!".to_vec();
        let data = file(&geometry, &obus);
        let top = boxes(&data);
        assert_eq!(
            top.iter().map(|(kind, _)| *kind).collect::<Vec<_>>(),
            [b"ftyp", b"meta", b"mdat"]
        );
        assert_eq!(top[0].1, b"avif\0\0\0\0avifmif1miafMA1B");
        assert_eq!(top[2].1, obus);
        let meta = top[1].1;
        assert_eq!(data.len(), 32 + 8 + meta.len() + 8 + obus.len());
        assert_eq!(&meta[..4], &[0, 0, 0, 0]);
        let inner = boxes(&meta[4..]);
        assert_eq!(
            inner.iter().map(|(kind, _)| *kind).collect::<Vec<_>>(),
            [b"hdlr", b"pitm", b"iloc", b"iinf", b"iprp"]
        );
        assert_eq!(
            inner[0].1,
            b"\0\0\0\0\0\0\0\0pict\0\0\0\0\0\0\0\0\0\0\0\0\0"
        );
        assert_eq!(inner[1].1, &[0, 0, 0, 0, 0, 1]);
        // iloc: the one extent is the mdat payload, by absolute offset.
        let iloc = inner[2].1;
        assert_eq!(&iloc[..14], &[0, 0, 0, 0, 0x44, 0, 0, 1, 0, 1, 0, 0, 0, 1]);
        let offset = u32::from_be_bytes(iloc[14..18].try_into().unwrap()) as usize;
        let length = u32::from_be_bytes(iloc[18..22].try_into().unwrap()) as usize;
        assert_eq!(length, obus.len());
        assert_eq!(&data[offset..offset + length], &obus[..]);
        assert_eq!(offset + length, data.len());
        // iinf: one av01 item, infe version 2.
        assert_eq!(
            inner[3].1,
            b"\0\0\0\0\0\x01\0\0\0\x15infe\x02\0\0\0\0\x01\0\0av01\0"
        );
        let iprp = boxes(inner[4].1);
        assert_eq!(iprp[0].0, b"ipco");
        assert_eq!(iprp[1].0, b"ipma");
        let ipco = boxes(iprp[0].1);
        assert_eq!(
            ipco.iter().map(|(kind, _)| *kind).collect::<Vec<_>>(),
            [b"ispe", b"pixi", b"av1C", b"colr"]
        );
        assert_eq!(ipco[0].1, &[0, 0, 0, 0, 0, 0, 1, 44, 0, 0, 0, 70]);
        assert_eq!(ipco[1].1, &[0, 0, 0, 0, 3, 8, 8, 8]);
        // av1C: marker and version, profile 0 at level 2.0, 8-bit 4:2:0.
        assert_eq!(ipco[2].1, &[0x81, 0x00, 0x0c, 0x00]);
        assert_eq!(ipco[3].1, b"nclx\0\x01\0\x0d\0\x06\x80");
        assert_eq!(iprp[1].1, &[0, 0, 0, 0, 0, 0, 0, 1, 0, 1, 4, 1, 2, 0x83, 4]);
    }

    #[test]
    fn the_level_and_size_follow_the_geometry() {
        // Level 6.0 is past the Baseline profile, as is level 31: no
        // profile brand then.
        let geometry = Geometry::new(6000, 4000).unwrap();
        let data = file(&geometry, &[]);
        assert_eq!(&data[..28], b"\0\0\0\x1cftypavif\0\0\0\0avifmif1miaf");
        assert_eq!(
            &file(&Geometry::new(16384, 16384).unwrap(), &[])[..28],
            b"\0\0\0\x1cftypavif\0\0\0\0avifmif1miaf"
        );
        assert_eq!(
            &file(&Geometry::new(3840, 2160).unwrap(), &[])[..32],
            b"\0\0\0\x20ftypavif\0\0\0\0avifmif1miafMA1B"
        );
        let at = data.windows(4).position(|w| w == b"av1C").unwrap();
        assert_eq!(&data[at + 4..at + 8], &[0x81, 0x10, 0x0c, 0x00]);
        let at = data.windows(4).position(|w| w == b"ispe").unwrap();
        assert_eq!(
            &data[at + 8..at + 16],
            &[0, 0, 0x17, 0x70, 0, 0, 0x0f, 0xa0]
        );
        // The mdat is empty and the extent says so.
        assert_eq!(&data[data.len() - 8..], b"\0\0\0\x08mdat");
        let at = data.windows(4).position(|w| w == b"iloc").unwrap();
        let offset = u32::from_be_bytes(data[at + 18..at + 22].try_into().unwrap()) as usize;
        let length = u32::from_be_bytes(data[at + 22..at + 26].try_into().unwrap());
        assert_eq!((offset, length), (data.len(), 0));
    }
}

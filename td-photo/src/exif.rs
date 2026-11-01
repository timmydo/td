//! The Exif an AVIF export carries: the raw's camera, lens and exposure
//! (`nef::Facts`) written as a fresh big-endian TIFF, IFD0 and an Exif
//! IFD, for the container's `Exif` item (`avif::file`). The raw's own
//! Exif is not copied: its maker note's offsets point into the raw
//! file, and its orientation describes the sensor's frame, where the
//! export's pixels are already turned upright, so `Orientation` is 1.
//! Only what `nef::Facts` holds is written: no GPS and no serial
//! numbers. A text the TIFF ASCII type cannot hold (past `MAX_TEXT`
//! bytes, or not printable 7-bit) is left out, as is a rational with a
//! zero denominator. Nothing here reads a file, the environment or a
//! clock.

use crate::nef::Facts;
use crate::tiff::{kind, tag};

/// The longest text written, in bytes before its NUL.
pub const MAX_TEXT: usize = 255;
/// The `Software` tag's value.
pub const SOFTWARE: &str = "td-photo";
/// Exif 2.32, as the `ExifVersion` tag spells it.
const EXIF_VERSION: &[u8; 4] = b"0232";
/// `ColorSpace` 1: sRGB, which the export's pixels are.
const SRGB: u16 = 1;
/// Exif's fixed values for its mandatory tags: 72 pixels an inch,
/// centred chroma, Y Cb Cr components, Flashpix 1.0.
const RESOLUTION: (u32, u32) = (72, 1);
const INCHES: u16 = 2;
const CENTRED: u16 = 1;
const COMPONENTS: &[u8; 4] = &[1, 2, 3, 0];
const FLASHPIX_VERSION: &[u8; 4] = b"0100";
/// `SensitivityType` for the tag a speed past a SHORT is in: 2 for
/// `RecommendedExposureIndex`, 3 for `ISOSpeed`.
fn sensitivity_type(tag: u16) -> Option<u16> {
    match tag {
        tag::RECOMMENDED_EXPOSURE_INDEX => Some(2),
        tag::ISO_SPEED => Some(3),
        _ => None,
    }
}
/// The TIFF header's bytes: the order, 42, and IFD0's offset.
const HEADER: usize = 8;
/// An IFD's count, then its twelve-byte entries, then its next offset.
const ENTRY: usize = 12;

/// One field: its tag, its kind, its count and its value's bytes.
struct Field {
    tag: u16,
    kind: u16,
    count: u32,
    value: Vec<u8>,
}

impl Field {
    fn short(tag: u16, value: u16) -> Field {
        Field {
            tag,
            kind: kind::SHORT,
            count: 1,
            value: value.to_be_bytes().to_vec(),
        }
    }

    fn undefined(tag: u16, value: &[u8; 4]) -> Field {
        Field {
            tag,
            kind: kind::UNDEFINED,
            count: 4,
            value: value.to_vec(),
        }
    }

    fn long(tag: u16, value: u32) -> Field {
        Field {
            tag,
            kind: kind::LONG,
            count: 1,
            value: value.to_be_bytes().to_vec(),
        }
    }

    fn rational(tag: u16, (n, d): (u32, u32)) -> Option<Field> {
        if d == 0 {
            return None;
        }
        let mut value = n.to_be_bytes().to_vec();
        value.extend_from_slice(&d.to_be_bytes());
        Some(Field {
            tag,
            kind: kind::RATIONAL,
            count: 1,
            value,
        })
    }

    fn srational(tag: u16, (n, d): (i32, i32)) -> Option<Field> {
        if d == 0 {
            return None;
        }
        let mut value = n.to_be_bytes().to_vec();
        value.extend_from_slice(&d.to_be_bytes());
        Some(Field {
            tag,
            kind: kind::SRATIONAL,
            count: 1,
            value,
        })
    }

    /// An ASCII field with its NUL, or none for a text TIFF cannot hold.
    fn ascii(tag: u16, text: &str) -> Option<Field> {
        let bytes = text.as_bytes();
        if bytes.is_empty()
            || bytes.len() > MAX_TEXT
            || !bytes.iter().all(|b| (0x20..=0x7e).contains(b))
        {
            return None;
        }
        let mut value = bytes.to_vec();
        value.push(0);
        Some(Field {
            tag,
            kind: kind::ASCII,
            count: u32::try_from(value.len()).ok()?,
            value,
        })
    }

    /// A SHORT where the value fits one, as Exif types these codes.
    fn code(tag: u16, value: u32) -> Option<Field> {
        u16::try_from(value).ok().map(|v| Field::short(tag, v))
    }
}

/// The Exif TIFF for an export of `width` by `height` (after crop,
/// orientation and shrink) from the photo `facts` describe.
pub fn tiff(facts: &Facts, width: usize, height: usize) -> Vec<u8> {
    let e = &facts.exposure;
    let mut ifd0: Vec<Field> = [
        Field::ascii(tag::MAKE, &facts.make),
        Field::ascii(tag::MODEL, &facts.model),
        Some(Field::short(tag::ORIENTATION, 1)),
        Field::rational(tag::X_RESOLUTION, RESOLUTION),
        Field::rational(tag::Y_RESOLUTION, RESOLUTION),
        Some(Field::short(tag::RESOLUTION_UNIT, INCHES)),
        Field::ascii(tag::SOFTWARE, SOFTWARE),
        Some(Field::short(tag::YCBCR_POSITIONING, CENTRED)),
        facts
            .artist
            .as_deref()
            .and_then(|t| Field::ascii(tag::ARTIST, t)),
        facts
            .copyright
            .as_deref()
            .and_then(|t| Field::ascii(tag::COPYRIGHT, t)),
    ]
    .into_iter()
    .flatten()
    .collect();
    let mut exif: Vec<Field> = [
        e.time.and_then(|v| Field::rational(tag::EXPOSURE_TIME, v)),
        e.aperture.and_then(|v| Field::rational(tag::F_NUMBER, v)),
        e.program
            .and_then(|v| Field::code(tag::EXPOSURE_PROGRAM, v)),
        // Past a SHORT, Exif writes the most it holds and the speed in
        // the tag the camera used, `SensitivityType` naming it; a 65535
        // with no such tag is the camera's own, written as it is.
        e.iso
            .map(|v| Field::short(tag::ISO, u16::try_from(v).unwrap_or(u16::MAX))),
        e.iso_tag
            .and_then(sensitivity_type)
            .map(|t| Field::short(tag::SENSITIVITY_TYPE, t)),
        e.iso_tag
            .filter(|t| sensitivity_type(*t).is_some())
            .zip(e.iso)
            .map(|(t, v)| Field::long(t, v)),
        Some(Field::undefined(tag::EXIF_VERSION, EXIF_VERSION)),
        Some(Field::undefined(tag::COMPONENTS_CONFIGURATION, COMPONENTS)),
        e.taken
            .as_deref()
            .and_then(|t| Field::ascii(tag::DATE_TIME_ORIGINAL, t)),
        e.offset
            .as_deref()
            .and_then(|t| Field::ascii(tag::OFFSET_TIME_ORIGINAL, t)),
        e.bias.and_then(|v| Field::srational(tag::EXPOSURE_BIAS, v)),
        e.metering.and_then(|v| Field::code(tag::METERING_MODE, v)),
        e.flash.and_then(|v| Field::code(tag::FLASH, v)),
        e.focal_length
            .and_then(|v| Field::rational(tag::FOCAL_LENGTH, v)),
        e.subsec
            .as_deref()
            .and_then(|t| Field::ascii(tag::SUB_SEC_TIME_ORIGINAL, t)),
        Some(Field::undefined(tag::FLASHPIX_VERSION, FLASHPIX_VERSION)),
        Some(Field::short(tag::COLOR_SPACE, SRGB)),
        Some(Field::long(
            tag::PIXEL_X_DIMENSION,
            u32::try_from(width).unwrap_or(u32::MAX),
        )),
        Some(Field::long(
            tag::PIXEL_Y_DIMENSION,
            u32::try_from(height).unwrap_or(u32::MAX),
        )),
        e.focal_length_35mm
            .and_then(|v| Field::code(tag::FOCAL_LENGTH_35MM, v)),
        e.lens_make
            .as_deref()
            .and_then(|t| Field::ascii(tag::LENS_MAKE, t)),
        e.lens
            .as_deref()
            .and_then(|t| Field::ascii(tag::LENS_MODEL, t)),
    ]
    .into_iter()
    .flatten()
    .collect();
    // IFD0 points at the Exif IFD, which follows it; the values too long
    // for an entry follow both, each on an even offset.
    let size = |fields: usize| 2 + ENTRY * fields + 4;
    let exif_at = HEADER + size(ifd0.len() + 1);
    ifd0.push(Field::long(tag::EXIF_IFD, offset(exif_at)));
    ifd0.sort_by_key(|f| f.tag);
    exif.sort_by_key(|f| f.tag);
    let data_base = exif_at + size(exif.len());
    let mut out = Vec::with_capacity(data_base + 4 * MAX_TEXT);
    out.extend_from_slice(b"MM\0\x2a");
    out.extend_from_slice(&offset(HEADER).to_be_bytes());
    let mut data = Vec::new();
    for fields in [&ifd0, &exif] {
        out.extend_from_slice(&u16::try_from(fields.len()).unwrap_or(0).to_be_bytes());
        for field in fields {
            out.extend_from_slice(&field.tag.to_be_bytes());
            out.extend_from_slice(&field.kind.to_be_bytes());
            out.extend_from_slice(&field.count.to_be_bytes());
            if field.value.len() <= 4 {
                let mut inline = [0u8; 4];
                for (slot, byte) in inline.iter_mut().zip(&field.value) {
                    *slot = *byte;
                }
                out.extend_from_slice(&inline);
            } else {
                out.extend_from_slice(&offset(data_base + data.len()).to_be_bytes());
                data.extend_from_slice(&field.value);
                if data.len() % 2 == 1 {
                    data.push(0);
                }
            }
        }
        out.extend_from_slice(&0u32.to_be_bytes());
    }
    out.extend_from_slice(&data);
    out
}

/// A position as a TIFF offset: the file is a few hundred bytes.
fn offset(at: usize) -> u32 {
    u32::try_from(at).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::indexing_slicing, clippy::unwrap_used)]

    use super::*;
    use crate::nef::Exposure;
    use crate::tiff::{Endian, Reader, Visited};

    fn facts() -> Facts {
        Facts {
            make: "NIKON CORPORATION".to_string(),
            model: "NIKON Z 8".to_string(),
            artist: Some("A. Photographer".to_string()),
            copyright: Some("(c) A. Photographer".to_string()),
            exposure: Exposure {
                time: Some((10, 20000)),
                aperture: Some((560, 100)),
                iso: Some(2800),
                iso_tag: None,
                focal_length: Some((1800, 10)),
                taken: Some("2026:09:12 17:18:09".to_string()),
                lens: Some("NIKKOR Z 180-600mm f/5.6-6.3 VR".to_string()),
                offset: Some("-07:00".to_string()),
                subsec: Some("45".to_string()),
                lens_make: Some("Nikon".to_string()),
                focal_length_35mm: Some(180),
                bias: Some((-2, 3)),
                program: Some(3),
                metering: Some(5),
                flash: Some(16),
            },
        }
    }

    /// The TIFF read back with the crate's own reader, the one that reads
    /// the raw: IFD0, then the Exif IFD it points at.
    #[test]
    fn the_tiff_reads_back_as_the_facts() {
        let data = tiff(&facts(), 6000, 4000);
        assert_eq!(&data[..8], b"MM\0\x2a\0\0\0\x08");
        let (reader, first) = Reader::new(&data, 0).unwrap();
        assert_eq!(reader.endian(), Endian::Big);
        let chain = reader.chain(first, &mut Visited::new()).unwrap();
        assert_eq!(chain.len(), 1, "IFD0 alone on the chain");
        let ifd0 = &chain[0];
        let text = |ifd: &crate::tiff::Ifd, t: u16| reader.ascii(ifd.find(t).unwrap()).unwrap();
        let int = |ifd: &crate::tiff::Ifd, t: u16| reader.integer(ifd.find(t).unwrap(), 0).unwrap();
        let tags: Vec<u16> = ifd0.entries.iter().map(|e| e.tag).collect();
        assert_eq!(
            tags,
            [271, 272, 274, 282, 283, 296, 305, 315, 531, 33432, 34665]
        );
        assert_eq!(text(ifd0, tag::MAKE), "NIKON CORPORATION");
        assert_eq!(text(ifd0, tag::MODEL), "NIKON Z 8");
        assert_eq!(int(ifd0, tag::ORIENTATION), 1);
        assert_eq!(text(ifd0, tag::SOFTWARE), "td-photo");
        assert_eq!(text(ifd0, tag::ARTIST), "A. Photographer");
        assert_eq!(text(ifd0, tag::COPYRIGHT), "(c) A. Photographer");
        let exif = reader.ifd(int(ifd0, tag::EXIF_IFD)).unwrap();
        let tags: Vec<u16> = exif.entries.iter().map(|e| e.tag).collect();
        assert_eq!(
            tags,
            [
                33434, 33437, 34850, 34855, 36864, 36867, 36881, 37121, 37380, 37383, 37385, 37386,
                37521, 40960, 40961, 40962, 40963, 41989, 42035, 42036
            ]
        );
        let rational = |t: u16| reader.rational(exif.find(t).unwrap(), 0).unwrap();
        assert_eq!(rational(tag::EXPOSURE_TIME), (10, 20000));
        assert_eq!(rational(tag::F_NUMBER), (560, 100));
        assert_eq!(rational(tag::FOCAL_LENGTH), (1800, 10));
        assert_eq!(
            reader
                .srational(exif.find(tag::EXPOSURE_BIAS).unwrap(), 0)
                .unwrap(),
            (-2, 3)
        );
        assert_eq!(int(&exif, tag::ISO), 2800);
        assert_eq!(int(&exif, tag::EXPOSURE_PROGRAM), 3);
        assert_eq!(int(&exif, tag::METERING_MODE), 5);
        assert_eq!(int(&exif, tag::FLASH), 16);
        assert_eq!(int(&exif, tag::FOCAL_LENGTH_35MM), 180);
        assert_eq!(int(&exif, tag::COLOR_SPACE), 1);
        assert_eq!(int(&exif, tag::PIXEL_X_DIMENSION), 6000);
        assert_eq!(int(&exif, tag::PIXEL_Y_DIMENSION), 4000);
        assert_eq!(
            reader.bytes(exif.find(tag::EXIF_VERSION).unwrap()).unwrap(),
            b"0232"
        );
        assert_eq!(text(&exif, tag::DATE_TIME_ORIGINAL), "2026:09:12 17:18:09");
        assert_eq!(text(&exif, tag::OFFSET_TIME_ORIGINAL), "-07:00");
        assert_eq!(text(&exif, tag::SUB_SEC_TIME_ORIGINAL), "45");
        assert_eq!(text(&exif, tag::LENS_MAKE), "Nikon");
        assert_eq!(
            text(&exif, tag::LENS_MODEL),
            "NIKKOR Z 180-600mm f/5.6-6.3 VR"
        );
        // Every value offset is even and inside the data.
        for entry in ifd0.entries.iter().chain(&exif.entries) {
            if entry.len > 4 {
                assert_eq!(entry.offset % 2, 0, "tag {}", entry.tag);
                assert!(entry.offset + entry.len <= data.len());
            }
        }
    }

    /// What the facts lack is not written, nor a text TIFF cannot hold,
    /// a zero denominator, or a code past a SHORT; an ISO past one is
    /// held to its most.
    #[test]
    fn absent_and_unwritable_facts_are_left_out() {
        let bare = Facts {
            make: "NIKON CORPORATION".to_string(),
            model: "NIKON Z 8\u{e9}".to_string(),
            artist: Some("x".repeat(MAX_TEXT + 1)),
            copyright: None,
            exposure: Exposure {
                time: Some((1, 0)),
                iso: Some(102_400),
                iso_tag: Some(tag::RECOMMENDED_EXPOSURE_INDEX),
                flash: Some(70_000),
                lens: Some("tab\there".to_string()),
                ..Exposure::default()
            },
        };
        let data = tiff(&bare, 3, 2);
        let (reader, first) = Reader::new(&data, 0).unwrap();
        let ifd0 = reader.ifd(first).unwrap();
        let tags: Vec<u16> = ifd0.entries.iter().map(|e| e.tag).collect();
        assert_eq!(tags, [271, 274, 282, 283, 296, 305, 531, 34665]);
        let at = reader
            .integer(ifd0.find(tag::EXIF_IFD).unwrap(), 0)
            .unwrap();
        let exif = reader.ifd(at).unwrap();
        let tags: Vec<u16> = exif.entries.iter().map(|e| e.tag).collect();
        assert_eq!(
            tags,
            [34855, 34864, 34866, 36864, 37121, 40960, 40961, 40962, 40963]
        );
        let int = |t: u16| reader.integer(exif.find(t).unwrap(), 0).unwrap();
        assert_eq!(int(tag::ISO), 65535);
        assert_eq!(int(tag::SENSITIVITY_TYPE), 2);
        assert_eq!(int(tag::RECOMMENDED_EXPOSURE_INDEX), 102_400);
    }

    /// A speed read from `ISOSpeed` is written there, type 3; a camera's
    /// 65535 with no speed tag is written alone, no type claimed.
    #[test]
    fn a_fast_speed_keeps_its_tag_and_a_bare_65535_claims_none() {
        let tags = |iso: u32, iso_tag: Option<u16>| {
            let facts = Facts {
                exposure: Exposure {
                    iso: Some(iso),
                    iso_tag,
                    ..Exposure::default()
                },
                ..Facts::default()
            };
            let data = tiff(&facts, 3, 2);
            let (reader, first) = Reader::new(&data, 0).unwrap();
            let ifd0 = reader.ifd(first).unwrap();
            let at = reader
                .integer(ifd0.find(tag::EXIF_IFD).unwrap(), 0)
                .unwrap();
            let exif = reader.ifd(at).unwrap();
            exif.entries
                .iter()
                .filter(|e| (34855..=34867).contains(&e.tag))
                .map(|e| (e.tag, reader.integer(e, 0).unwrap()))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            tags(204_800, Some(tag::ISO_SPEED)),
            [(34855, 65535), (34864, 3), (34867, 204_800)]
        );
        assert_eq!(tags(65535, None), [(34855, 65535)]);
        assert_eq!(tags(800, None), [(34855, 800)]);
    }

    /// The least TIFF with a lens to its bytes, laid out by hand rather
    /// than by the reader that shares the writer's assumptions: IFD0 at
    /// 8 with nine entries (114 bytes) and the Exif IFD at 122 with seven
    /// (90), then from 212 IFD0's long values (the two resolutions and
    /// the software, padded) and the Exif IFD's (the lens, padded).
    #[test]
    fn the_least_tiff_is_its_bytes() {
        let facts = Facts {
            make: "N".to_string(),
            model: "Z".to_string(),
            exposure: Exposure {
                lens: Some("Lens".to_string()),
                ..Exposure::default()
            },
            ..Facts::default()
        };
        let mut want: Vec<u8> = b"MM\0\x2a\0\0\0\x08".to_vec();
        let entry = |want: &mut Vec<u8>, tag: u16, kind: u16, count: u32, value: [u8; 4]| {
            want.extend_from_slice(&tag.to_be_bytes());
            want.extend_from_slice(&kind.to_be_bytes());
            want.extend_from_slice(&count.to_be_bytes());
            want.extend_from_slice(&value);
        };
        want.extend_from_slice(&[0, 9]);
        entry(&mut want, 271, 2, 2, *b"N\0\0\0");
        entry(&mut want, 272, 2, 2, *b"Z\0\0\0");
        entry(&mut want, 274, 3, 1, [0, 1, 0, 0]);
        entry(&mut want, 282, 5, 1, [0, 0, 0, 212]);
        entry(&mut want, 283, 5, 1, [0, 0, 0, 220]);
        entry(&mut want, 296, 3, 1, [0, 2, 0, 0]);
        entry(&mut want, 305, 2, 9, [0, 0, 0, 228]);
        entry(&mut want, 531, 3, 1, [0, 1, 0, 0]);
        entry(&mut want, 34665, 4, 1, [0, 0, 0, 122]);
        want.extend_from_slice(&[0, 0, 0, 0]);
        assert_eq!(want.len(), 122);
        want.extend_from_slice(&[0, 7]);
        entry(&mut want, 36864, 7, 4, *b"0232");
        entry(&mut want, 37121, 7, 4, [1, 2, 3, 0]);
        entry(&mut want, 40960, 7, 4, *b"0100");
        entry(&mut want, 40961, 3, 1, [0, 1, 0, 0]);
        entry(&mut want, 40962, 4, 1, [0, 0, 0, 3]);
        entry(&mut want, 40963, 4, 1, [0, 0, 0, 2]);
        entry(&mut want, 42036, 2, 5, [0, 0, 0, 238]);
        want.extend_from_slice(&[0, 0, 0, 0]);
        assert_eq!(want.len(), 212);
        want.extend_from_slice(&[0, 0, 0, 72, 0, 0, 0, 1]);
        want.extend_from_slice(&[0, 0, 0, 72, 0, 0, 0, 1]);
        want.extend_from_slice(b"td-photo\0\0");
        want.extend_from_slice(b"Lens\0\0");
        assert_eq!(tiff(&facts, 3, 2), want);
    }
}

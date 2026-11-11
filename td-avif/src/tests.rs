#![allow(clippy::indexing_slicing, clippy::unwrap_used)]
use super::*;

fn boxed(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let mut out = ((data.len() + 8) as u32).to_be_bytes().to_vec();
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    out
}
fn full(kind: &[u8; 4], version: u8, data: &[u8]) -> Vec<u8> {
    let mut bytes = vec![version, 0, 0, 0];
    bytes.extend_from_slice(data);
    boxed(kind, &bytes)
}
fn meta(
    offset: usize,
    idat: bool,
    extent_count: usize,
    extra: Option<(&[u8; 4], bool)>,
) -> Vec<u8> {
    let mut out = full(
        b"hdlr",
        0,
        &[
            0, 0, 0, 0, b'p', b'i', b'c', b't', 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ],
    );
    out.extend(full(b"pitm", 0, &[0, 1]));
    let infe = full(b"infe", 2, &[0, 1, 0, 0, b'a', b'v', b'0', b'1', 0]);
    let mut iinf = vec![0, 1];
    iinf.extend(infe);
    out.extend(full(b"iinf", 0, &iinf));
    let mut iloc = vec![0x44, 0, 0, 1, 0, 1];
    if idat {
        iloc.extend([0, 1]);
    }
    iloc.extend([0, 0]);
    iloc.extend((extent_count as u16).to_be_bytes());
    for i in 0..extent_count {
        iloc.extend(((offset + i) as u32).to_be_bytes());
        iloc.extend(1u32.to_be_bytes());
    }
    out.extend(full(b"iloc", u8::from(idat), &iloc));
    let mut props = full(b"ispe", 0, &[0, 0, 0, 3, 0, 0, 0, 2]);
    props.extend(boxed(b"av1C", &[0x81, 0, 0x0c, 0]));
    props.extend(full(b"pixi", 0, &[3, 8, 8, 8]));
    let mut associations = vec![0, 0, 0, 1, 0, 1, 3, 1, 0x82, 3];
    if let Some((kind, essential)) = extra {
        props.extend(boxed(kind, &[0]));
        associations[6] += 1;
        associations.push(4 | if essential { 0x80 } else { 0 });
    }
    let mut iprp = boxed(b"ipco", &props);
    iprp.extend(full(b"ipma", 0, &associations));
    out.extend(boxed(b"iprp", &iprp));
    if idat {
        out.extend(boxed(b"idat", b"ABC"));
    }
    full(b"meta", 0, &out)
}
fn file(idat: bool, extra: Option<(&[u8; 4], bool)>) -> Vec<u8> {
    let mut out = boxed(b"ftyp", b"avif\0\0\0\0mif1avif");
    let m = meta(0, idat, 3, extra);
    let offset = if idat { 0 } else { out.len() + m.len() + 8 };
    out.extend(meta(offset, idat, 3, extra));
    if !idat {
        out.extend(boxed(b"mdat", b"ABC"));
    }
    out
}
fn at(bytes: &[u8], kind: &[u8; 4]) -> usize {
    bytes.windows(4).position(|b| b == kind).unwrap() + 4
}

#[test]
fn primary_item_extent_methods_and_properties() {
    for idat in [false, true] {
        let data = file(idat, None);
        let item = primary_image(&data).unwrap();
        assert_eq!(
            item.header,
            Header {
                width: 3,
                height: 2
            }
        );
        assert_eq!(header(&data).unwrap(), item.header);
        assert_eq!(item.av1_config, [0x81, 0, 0x0c, 0]);
        assert_eq!(item.data, b"ABC");
    }
    assert!(primary_image(&file(false, Some((b"test", false)))).is_ok());
    assert!(matches!(
        primary_image(&file(false, Some((b"test", true)))),
        Err(Error::Unsupported)
    ));
    assert!(matches!(
        primary_image(&file(false, Some((b"irot", false)))),
        Err(Error::Unsupported)
    ));
}

#[test]
fn independent_libavif_container_extracts_its_primary_item() {
    let bytes = include_bytes!("../tests/fixtures/libavif.avif");
    let item = primary_image(bytes).unwrap();
    assert_eq!(
        item.header,
        Header {
            width: 6,
            height: 4
        }
    );
    assert_eq!(item.data.len(), 32);
    assert_eq!(item.av1_config, [0x81, 0, 12, 0]);
}

#[test]
fn admitted_avif_decodes_opaque_rgba_and_refuses_metadata_disagreement() {
    let bytes = include_bytes!("../tests/fixtures/libaom-reduced.avif");
    let image = decode(bytes).unwrap();
    assert_eq!((image.width, image.height), (6, 4));
    assert_eq!(image.data, [232, 134, 109, 255].repeat(24));
    assert!(is_avif(bytes));
    assert!(!is_avif(b"not an AVIF"));
    let mut wrong_size = bytes.to_vec();
    let ispe = at(&wrong_size, b"ispe");
    wrong_size[ispe + 4..ispe + 8].copy_from_slice(&7u32.to_be_bytes());
    assert_eq!(decode(&wrong_size), Err(Error::Container));
    let mut wrong_config = bytes.to_vec();
    let config = at(&wrong_config, b"av1C");
    wrong_config[config + 1] = 4;
    assert_eq!(decode(&wrong_config), Err(Error::Container));
    let mut wrong_colour = bytes.to_vec();
    let colour = at(&wrong_colour, b"nclx");
    wrong_colour[colour + 5] = 1;
    assert_eq!(decode(&wrong_colour), Err(Error::Container));
    let unsupported = include_bytes!("../tests/fixtures/libavif.avif");
    assert_eq!(decode(unsupported), Err(Error::Unsupported));
    for n in 0..bytes.len() {
        assert!(decode(&bytes[..n]).is_err(), "prefix {n}");
    }
}

#[test]
fn colour_conversion_handles_range_and_both_matrices() {
    let mut colour = Colour {
        primaries: 1,
        transfer: 13,
        matrix: 6,
        full_range: true,
    };
    assert_eq!(rgb([0, 128, 128], colour), [0, 0, 0]);
    assert_eq!(rgb([255, 128, 128], colour), [255, 255, 255]);
    assert_eq!(rgb([128, 0, 128], colour), [128, 172, 0]);
    colour.matrix = 1;
    assert_eq!(rgb([128, 0, 128], colour), [128, 152, 0]);
    for matrix in [1, 6] {
        colour.matrix = matrix;
        colour.full_range = false;
        assert_eq!(rgb([16, 128, 128], colour), [0, 0, 0]);
        assert_eq!(rgb([235, 128, 128], colour), [255, 255, 255]);
    }
}

#[test]
fn sequence_signaling_does_not_fall_back_to_a_primary_still() {
    let mut branded = file(false, None);
    branded[8..12].copy_from_slice(b"avis");
    assert_eq!(header(&branded), Err(Error::Unsupported));
    let mut movie = file(false, None);
    movie.extend(boxed(b"moov", &[]));
    assert_eq!(header(&movie), Err(Error::Unsupported));
}

#[test]
fn truncated_boxes_counts_dimensions_and_extents_refuse() {
    let data = file(false, None);
    for n in 0..data.len() {
        assert!(header(&data[..n]).is_err(), "prefix {n}");
    }
    let mut large = data.clone();
    let ispe = at(&large, b"ispe");
    large[ispe + 4..ispe + 8].copy_from_slice(&16385u32.to_be_bytes());
    assert_eq!(header(&large), Err(Error::Axis));
    let mut empty = data.clone();
    empty[ispe + 4..ispe + 8].fill(0);
    assert_eq!(header(&empty), Err(Error::Axis));
    let mut outside = data.clone();
    let iloc = at(&outside, b"iloc");
    outside[iloc + 14..iloc + 18].fill(0xff);
    assert!(header(&outside).is_err());
    let mut box_size = data.clone();
    box_size[..4].copy_from_slice(&4u32.to_be_bytes());
    assert!(header(&box_size).is_err());
    let mut count = data.clone();
    count[iloc + 12..iloc + 14].copy_from_slice(&257u16.to_be_bytes());
    assert_eq!(header(&count), Err(Error::Limit));
    let mut duplicate = data.clone();
    let start = at(&data, b"meta") - 8;
    let size = u32::from_be_bytes(data[start..start + 4].try_into().unwrap()) as usize;
    duplicate.extend_from_slice(&data[start..start + size]);
    assert_eq!(header(&duplicate), Err(Error::Container));
}

#[test]
fn mutation_corpus_is_bounded_and_successful_extractions_agree() {
    for idat in [false, true] {
        let data = file(idat, None);
        let mut seed = 17u64;
        for _ in 0..4000 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            let mut changed = data.clone();
            let n = (seed >> 16) as usize % changed.len();
            changed[n] ^= (seed >> 32) as u8;
            if let Ok(item) = primary_image(&changed) {
                assert_eq!(header(&changed).unwrap(), item.header);
                assert!(item.data.len() <= MAX_INPUT_BYTES);
                assert!(item.header.width * item.header.height <= MAX_IMAGE_PIXELS);
            }
        }
    }
}

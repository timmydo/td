#![allow(clippy::unwrap_used, clippy::indexing_slicing)]
use td_webp::{decode, header};

#[test]
fn libwebp_lossless_oracles() {
    for (encoded, rgba, width, height) in [
        (
            &include_bytes!("fixtures/single.webp")[..],
            &include_bytes!("fixtures/single.rgba")[..],
            1,
            1,
        ),
        (
            &include_bytes!("fixtures/palette.webp")[..],
            &include_bytes!("fixtures/palette.rgba")[..],
            35,
            27,
        ),
        (
            &include_bytes!("fixtures/gradient.webp")[..],
            &include_bytes!("fixtures/gradient.rgba")[..],
            65,
            47,
        ),
        (
            &include_bytes!("fixtures/noise.webp")[..],
            &include_bytes!("fixtures/noise.rgba")[..],
            41,
            29,
        ),
        (
            &include_bytes!("fixtures/vertical.webp")[..],
            &include_bytes!("fixtures/vertical.rgba")[..],
            1,
            127,
        ),
        (
            &include_bytes!("fixtures/horizontal.webp")[..],
            &include_bytes!("fixtures/horizontal.rgba")[..],
            127,
            1,
        ),
    ] {
        let h = header(encoded).unwrap();
        assert_eq!((h.width, h.height), (width, height));
        let image = decode(encoded).unwrap();
        assert_eq!(image.data, rgba, "{width} by {height}");
    }
}

#[test]
fn containers_are_bounded_and_truncation_is_refused() {
    let original = include_bytes!("fixtures/palette.webp");
    for n in 0..original.len() {
        assert!(decode(&original[..n]).is_err());
    }
    let mut bytes = original.to_vec();
    bytes[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
    assert!(decode(&bytes).is_err());
    bytes = original.to_vec();
    bytes[16..20].copy_from_slice(&u32::MAX.to_le_bytes());
    assert!(decode(&bytes).is_err());
    let mut bytes = vec![0; td_webp::MAX_INPUT_BYTES + 1];
    bytes[..original.len()].copy_from_slice(original);
    assert_eq!(header(&bytes), Err(td_webp::Error::Limit));
    assert_eq!(decode(&bytes), Err(td_webp::Error::Limit));
}

#[test]
fn extended_canvas_must_agree_and_duplicate_images_are_refused() {
    let original = include_bytes!("fixtures/palette.webp");
    let mut bytes = b"RIFF\0\0\0\0WEBPVP8X\x0a\0\0\0\x10\0\0\0\x22\0\0\x1a\0\0".to_vec();
    bytes.extend_from_slice(&original[12..]);
    let n = bytes.len() as u32 - 8;
    bytes[4..8].copy_from_slice(&n.to_le_bytes());
    assert_eq!(decode(&bytes), decode(original));
    bytes[24] = 35;
    assert_eq!(decode(&bytes), Err(td_webp::Error::Container));
    bytes[24] = 34;
    bytes.extend_from_slice(&original[12..]);
    let n = bytes.len() as u32 - 8;
    bytes[4..8].copy_from_slice(&n.to_le_bytes());
    assert_eq!(decode(&bytes), Err(td_webp::Error::Container));
}

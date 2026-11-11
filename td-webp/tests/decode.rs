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

#[test]
fn lossy_alpha_matches_libwebp() {
    for (data, alpha) in [
        (
            &include_bytes!("fixtures/gradient-alpha.webp")[..],
            &include_bytes!("fixtures/gradient-alpha.a")[..],
        ),
        (
            &include_bytes!("fixtures/noise-alpha.webp")[..],
            &include_bytes!("fixtures/noise-alpha.a")[..],
        ),
        (
            &include_bytes!("fixtures/palette-alpha.webp")[..],
            &include_bytes!("fixtures/palette-alpha.a")[..],
        ),
    ] {
        let image = decode(data).unwrap();
        let actual: Vec<u8> = image.data.as_chunks::<4>().0.iter().map(|p| p[3]).collect();
        assert_eq!(actual, alpha);
    }
}

fn riff(chunks: &[(&[u8; 4], &[u8])]) -> Vec<u8> {
    let mut bytes = b"RIFF\0\0\0\0WEBP".to_vec();
    for &(name, data) in chunks {
        bytes.extend_from_slice(name);
        bytes.extend_from_slice(&(data.len() as u32).to_le_bytes());
        bytes.extend_from_slice(data);
        if data.len() % 2 != 0 {
            bytes.push(0);
        }
    }
    let n = bytes.len() as u32 - 8;
    bytes[4..8].copy_from_slice(&n.to_le_bytes());
    bytes
}

#[test]
fn first_animation_frame_uses_its_rectangle_and_ignores_later_frames() {
    let still = include_bytes!("fixtures/palette.webp");
    let image = decode(still).unwrap();
    let extended = [0x12, 0, 0, 0, 39, 0, 0, 30, 0, 0];
    let mut frame = vec![1, 0, 0, 1, 0, 0, 34, 0, 0, 26, 0, 0, 100, 0, 0, 2];
    frame.extend_from_slice(&still[12..]);
    let bytes = riff(&[
        (b"VP8X", &extended),
        (b"ANIM", &[255, 0, 0, 255, 0, 0]),
        (b"ANMF", &frame),
        (b"ANMF", b"invalid later frame"),
    ]);
    let actual = decode(&bytes).unwrap();
    assert_eq!((actual.width, actual.height), (40, 31));
    for y in 0..31 {
        for x in 0..40 {
            let p = &actual.data[(y * 40 + x) * 4..(y * 40 + x + 1) * 4];
            if (2..37).contains(&x) && (2..29).contains(&y) {
                let i = ((y - 2) * 35 + x - 2) * 4;
                assert_eq!(p, &image.data[i..i + 4]);
            } else {
                assert_eq!(p, [0, 0, 0, 0]);
            }
        }
    }
    frame[0] = 4;
    assert_eq!(
        decode(&riff(&[
            (b"VP8X", &extended),
            (b"ANIM", &[0; 6]),
            (b"ANMF", &frame)
        ])),
        Err(td_webp::Error::Container)
    );
}

#[test]
fn raw_alpha_filters_reconstruct_edges_and_gradients() {
    let still = include_bytes!("fixtures/gradient-lossy.webp");
    let (width, height) = (65usize, 47usize);
    let alpha: Vec<u8> = (0..width * height)
        .map(|n| ((n % width) * 13 + (n / width) * 19) as u8)
        .collect();
    for filter in 0..4u8 {
        let mut encoded = vec![filter << 2];
        for pos in 0..alpha.len() {
            let (x, y) = (pos % width, pos / width);
            let pred = if filter == 0 || pos == 0 {
                0
            } else if y == 0 {
                alpha[pos - 1]
            } else if x == 0 {
                alpha[pos - width]
            } else {
                match filter {
                    1 => alpha[pos - 1],
                    2 => alpha[pos - width],
                    _ => (i32::from(alpha[pos - 1]) + i32::from(alpha[pos - width])
                        - i32::from(alpha[pos - width - 1]))
                    .clamp(0, 255) as u8,
                }
            };
            encoded.push(alpha[pos].wrapping_sub(pred));
        }
        let bytes = riff(&[
            (b"VP8X", &[0x10, 0, 0, 0, 64, 0, 0, 46, 0, 0]),
            (b"ALPH", &encoded),
            (
                b"VP8 ",
                &still[20..20 + u32::from_le_bytes(still[16..20].try_into().unwrap()) as usize],
            ),
        ]);
        let image = decode(&bytes).unwrap();
        let actual: Vec<u8> = image.data.as_chunks::<4>().0.iter().map(|p| p[3]).collect();
        assert_eq!(actual, alpha);
    }
}

#[test]
fn unused_coefficient_partitions_may_be_empty() {
    let data = include_bytes!("fixtures/empty-partitions.webp");
    let image = decode(data).unwrap();
    assert_eq!((image.width, image.height, image.data.len()), (1, 1, 4));
}

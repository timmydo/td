#![forbid(unsafe_code)]
#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use td_gif::{decode, header, Error};

fn screen(w: u16, h: u16) -> Vec<u8> {
    let mut out = b"GIF89a".to_vec();
    out.extend(w.to_le_bytes());
    out.extend(h.to_le_bytes());
    // Four colours; background green.
    out.extend([0x81, 2, 0, 0, 0, 0, 255, 0, 0, 0, 255, 0, 0, 0, 255]);
    out
}
fn descriptor(out: &mut Vec<u8>, left: u16, top: u16, w: u16, h: u16, flags: u8) {
    out.push(0x2c);
    for word in [left, top, w, h] {
        out.extend(word.to_le_bytes());
    }
    out.push(flags);
}
fn packed(codes: &[u16], min: u8) -> Vec<u8> {
    let clear = 1 << min;
    let mut width = min + 1;
    let mut next = clear + 2;
    let mut old = false;
    let mut acc = 0u32;
    let mut bits = 0;
    let mut data = Vec::new();
    for &code in codes {
        acc |= u32::from(code) << bits;
        bits += width;
        while bits >= 8 {
            data.push(acc as u8);
            acc >>= 8;
            bits -= 8;
        }
        if code == clear {
            width = min + 1;
            next = clear + 2;
            old = false;
        } else if code != clear + 1 {
            if old && next < 4096 {
                next += 1;
                if next == 1 << width && width < 12 {
                    width += 1;
                }
            }
            old = true;
        }
    }
    if bits != 0 {
        data.push(acc as u8);
    }
    data
}
fn data(out: &mut Vec<u8>, min: u8, codes: &[u16], chunk: usize) {
    out.push(min);
    for block in packed(codes, min).chunks(chunk) {
        out.push(block.len() as u8);
        out.extend(block);
    }
    out.push(0);
}
fn simple(w: u16, h: u16, codes: &[u16]) -> Vec<u8> {
    let mut out = screen(w, h);
    descriptor(&mut out, 0, 0, w, h, 0);
    data(&mut out, 2, codes, 255);
    out
}

#[test]
fn independent_imagemagick_fixtures_decode_exact_pixels() {
    for (bytes, w, h, alpha) in [
        (
            include_bytes!("fixtures/palette.gif").as_slice(),
            128,
            128,
            false,
        ),
        (
            include_bytes!("fixtures/interlaced.gif").as_slice(),
            128,
            128,
            false,
        ),
        (
            include_bytes!("fixtures/transparent.gif").as_slice(),
            13,
            11,
            true,
        ),
    ] {
        let image = decode(bytes).unwrap();
        assert_eq!((image.width, image.height), (w, h));
        assert_eq!(header(bytes).unwrap().width, w);
        let mut state = 0x12345678u32;
        for (i, &px) in image.data.as_chunks::<4>().0.iter().enumerate() {
            let (x, y) = (i % w, i / w);
            state = state.wrapping_mul(1664525).wrapping_add(1013904223);
            let value = if alpha {
                ((x + y) % 4 * 85) as u8
            } else {
                (state >> 24) as u8
            };
            let expected = if alpha && (x + 2 * y) % 3 == 0 {
                [0; 4]
            } else {
                [value, value, value, 255]
            };
            assert_eq!(px, expected, "pixel {x},{y}");
        }
    }
}

#[test]
fn lzw_special_code_width_growth_full_dictionary_and_reset() {
    assert_eq!(
        decode(&simple(3, 1, &[4, 1, 6, 5])).unwrap().data,
        [255, 0, 0, 255].repeat(3)
    );
    // Literal stream fills the dictionary, then keeps using it without clear.
    // Arbitrary sub-block boundaries do not reset the bit reservoir.
    let mut codes = vec![4];
    codes.extend((0..12000).map(|i| (i % 4) as u16));
    codes.push(5);
    for chunk in [1, 2, 255] {
        let mut out = screen(12000, 1);
        descriptor(&mut out, 0, 0, 12000, 1, 0);
        data(&mut out, 2, &codes, chunk);
        let image = decode(&out).unwrap();
        assert_eq!(image.data.len(), 48000);
        assert_eq!(&image.data[47996..], &[0, 0, 255, 255]);
    }
    let codes = [4, 1, 6, 4, 2, 6, 5];
    assert_eq!(
        decode(&simple(6, 1, &codes)).unwrap().data,
        [[255, 0, 0, 255].repeat(3), [0, 255, 0, 255].repeat(3)].concat()
    );
    // Once full, references into its frozen dictionary still decode.
    let mut codes = vec![4];
    codes.extend(std::iter::repeat_n(1, 4091));
    codes.extend([6, 4095, 5]);
    let image = decode(&simple(4095, 1, &codes)).unwrap();
    assert_eq!(image.data, [255, 0, 0, 255].repeat(4095));
}

#[test]
fn local_palette_offsets_transparency_and_first_frame_only() {
    let mut out = screen(3, 2);
    out.extend([0x21, 0xfe, 3, b'a', b'b', b'c', 0]);
    descriptor(&mut out, 1, 1, 2, 1, 0x80);
    out.extend([10, 20, 30, 90, 80, 70]);
    data(&mut out, 2, &[4, 0, 1, 5], 255);
    let image = decode(&out).unwrap();
    assert_eq!(&image.data[..16], [0, 255, 0, 255].repeat(4));
    assert_eq!(&image.data[16..], &[10, 20, 30, 255, 90, 80, 70, 255]);
    // Trailer/animation tail is intentionally irrelevant, even malformed.
    out.extend([0x2c, 0xff, 0xff]);
    assert_eq!(decode(&out).unwrap(), image);
    let mut out = screen(3, 2);
    out.extend([0x21, 0xf9, 4, 1, 0, 0, 0, 0]);
    descriptor(&mut out, 1, 1, 2, 1, 0);
    data(&mut out, 2, &[4, 0, 1, 5], 1);
    let image = decode(&out).unwrap();
    assert_eq!(&image.data[..20], &[0; 20]);
    assert_eq!(&image.data[20..], &[255, 0, 0, 255]);
    let mut no_global = b"GIF87a\x01\0\x01\0\0\0\0".to_vec();
    descriptor(&mut no_global, 0, 0, 1, 1, 0x80);
    no_global.extend([2, 3, 4, 5, 6, 7]);
    data(&mut no_global, 2, &[4, 1, 5], 255);
    assert_eq!(decode(&no_global).unwrap().data, [5, 6, 7, 255]);
}

#[test]
fn interlace_all_short_pass_shapes() {
    for height in 1..18u16 {
        let mut codes = vec![4];
        for (start, step) in [(0, 8), (4, 8), (2, 4), (1, 2)] {
            for row in (start..height).step_by(step) {
                codes.extend([4, row % 4]);
            }
        }
        codes.push(5);
        let mut out = screen(1, height);
        descriptor(&mut out, 0, 0, 1, height, 0x40);
        data(&mut out, 2, &codes, 1);
        let image = decode(&out).unwrap();
        for (y, &px) in image.data.as_chunks::<4>().0.iter().enumerate() {
            assert_eq!(
                px,
                [
                    [0, 0, 0, 255],
                    [255, 0, 0, 255],
                    [0, 255, 0, 255],
                    [0, 0, 255, 255]
                ][y % 4]
            );
        }
    }
}

#[test]
fn malformed_streams_and_bounds_are_named() {
    assert_eq!(header(b"notgif......."), Err(Error::NotGif));
    for (w, h) in [(0, 1), (1, 0), (16385, 1), (16384, 16384)] {
        assert_eq!(header(&screen(w, h)), Err(Error::Axis));
    }
    let mut out = screen(1, 1);
    out.push(0x3b);
    assert_eq!(decode(&out), Err(Error::NoImage));
    let mut out = screen(1, 1);
    out.extend([0x21, 1]);
    assert_eq!(decode(&out), Err(Error::PlainText));
    let mut out = screen(1, 1);
    descriptor(&mut out, 1, 0, 1, 1, 0);
    assert_eq!(decode(&out), Err(Error::Frame));
    for codes in [&[1, 5][..], &[4, 7, 5], &[4, 6, 5]] {
        assert_eq!(decode(&simple(1, 1, codes)), Err(Error::Lzw));
    }
    for codes in [&[4, 5][..], &[4, 1, 1, 5], &[4, 1, 6, 5]] {
        assert_eq!(decode(&simple(1, 1, codes)), Err(Error::Pixels));
    }
    let mut out = screen(1, 1);
    descriptor(&mut out, 0, 0, 1, 1, 0);
    data(&mut out, 3, &[8, 7, 9], 255);
    assert_eq!(decode(&out), Err(Error::Palette));
    for flags in [0x20, 0x10] {
        let mut out = screen(1, 1);
        out.extend([0x21, 0xf9, 4, flags, 0, 0, 0, 0]);
        assert_eq!(decode(&out), Err(Error::Extension));
    }
    let mut out = simple(1, 1, &[4, 1, 5]);
    // A complete byte after EOI is not padding.
    let last = out.len() - 1;
    out[last] = 1;
    out.extend([0, 0]);
    assert_eq!(decode(&out), Err(Error::Lzw));
}

#[test]
fn truncations_mutations_and_random_bytes_stay_bounded() {
    let original = simple(7, 1, &[4, 1, 6, 7, 1, 5]);
    assert!(decode(&original).is_ok());
    for n in 0..original.len() {
        assert!(decode(&original[..n]).is_err());
    }
    let mut state = 13u32;
    for len in 0..256 {
        let mut bytes = vec![0; len];
        for b in &mut bytes {
            state = state.wrapping_mul(1664525).wrapping_add(1013904223);
            *b = (state >> 24) as u8;
        }
        assert!(decode(&bytes).is_err());
    }
    let mut mutated = original.clone();
    // Keep logical dimensions small; mutate all other framing and data bytes.
    for pos in (0..6).chain(10..original.len()) {
        for value in [0, 1, 127, 255] {
            mutated[pos] = value;
            if let Ok(image) = decode(&mutated) {
                assert_eq!(image.data.len(), image.width * image.height * 4);
            }
        }
        mutated[pos] = original[pos];
    }
    let fixture = include_bytes!("fixtures/transparent.gif");
    // The returned frame does not require its final GIF trailer.
    for n in 0..fixture.len() - 1 {
        assert!(decode(&fixture[..n]).is_err());
    }
}

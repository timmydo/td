#![forbid(unsafe_code)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

//! The AV1 encoder: its frames' geometry, the streams it writes for a
//! range of shapes and qualities, their reconstruction against the
//! source, and, with `TD_TEST_DAV1D` naming a `dav1d` binary, that a real
//! decoder shows exactly the picture the encoder reconstructed for
//! itself. Without the binary the decoder leg is skipped, not failed.

use std::process::Command;

use td_photo::av1::{qindex, Encoder, Geometry, Reconstruction};
use td_photo::avif;

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("td-photo-av1-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

/// A synthetic photo: a soft gradient with a few edges and some noise.
fn picture(width: usize, height: usize) -> Vec<u8> {
    let mut rgb = Vec::with_capacity(width * height * 3);
    let mut seed = 0x9E3779B97F4A7C15u64;
    for y in 0..height {
        for x in 0..width {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let noise = ((seed >> 60) as i32) - 8;
            let edge = if (x / 23 + y / 17) % 3 == 0 { 40 } else { 0 };
            let r = (x * 255 / width.max(1)) as i32 + noise;
            let g = (y * 255 / height.max(1)) as i32 + edge + noise;
            let b = ((x + y) * 128 / (width + height)) as i32 + 64 - edge / 2;
            rgb.push(r.clamp(0, 255) as u8);
            rgb.push(g.clamp(0, 255) as u8);
            rgb.push(b.clamp(0, 255) as u8);
        }
    }
    rgb
}

/// Encodes a synthetic picture, feeding rows in uneven pieces.
fn encode(width: usize, height: usize, quality: u8, threads: usize) -> (Vec<u8>, Reconstruction) {
    encode_tiled(width, height, quality, threads, 0)
}

/// `encode`, asking for tile rows.
fn encode_tiled(
    width: usize,
    height: usize,
    quality: u8,
    threads: usize,
    rows_log2: u32,
) -> (Vec<u8>, Reconstruction) {
    let rgb = picture(width, height);
    let geometry = Geometry::tiled(width, height, rows_log2).unwrap();
    let mut encoder = Encoder::with_geometry(geometry, quality, threads).unwrap();
    encoder.keep_reconstruction();
    let row = width * 3;
    let mut at = 0;
    let mut piece = 7;
    while at < rgb.len() {
        let end = (at + piece * row).min(rgb.len());
        encoder.encode_rows(&rgb[at..end]).unwrap();
        at = end;
        piece = piece * 3 % 61 + 1;
    }
    let (obus, reconstruction) = encoder.finish_with().unwrap();
    (obus, reconstruction.unwrap())
}

/// Decodes the OBUs with dav1d (a section-5 stream led by a temporal
/// delimiter) to raw 4:2:0 planes, or `None` without the binary.
fn dav1d(obus: &[u8], name: &str) -> Option<Vec<u8>> {
    let dav1d = std::env::var_os("TD_TEST_DAV1D")?;
    let input = scratch(&format!("{name}.obu"));
    let output = scratch(&format!("{name}.yuv"));
    let mut stream = vec![0x12, 0x00];
    stream.extend_from_slice(obus);
    std::fs::write(&input, &stream).unwrap();
    let status = Command::new(dav1d)
        .arg("-q")
        .arg("-i")
        .arg(&input)
        .arg("-o")
        .arg(&output)
        .arg("--muxer")
        .arg("yuv")
        .status()
        .expect("dav1d runs");
    assert!(status.success(), "dav1d refused {name}");
    let decoded = std::fs::read(&output).unwrap();
    // The files are for the decoder; a failure above keeps them to look
    // at.
    std::fs::remove_file(&input).unwrap();
    std::fs::remove_file(&output).unwrap();
    let _ = std::fs::remove_dir(input.parent().unwrap());
    Some(decoded)
}

/// The luma peak signal-to-noise ratio of a reconstruction against the
/// picture it was made from, in decibels.
fn luma_psnr(rgb: &[u8], reconstruction: &Reconstruction) -> f64 {
    let (w, h) = (reconstruction.width, reconstruction.height);
    let mut sse = 0f64;
    for y in 0..h {
        for x in 0..w {
            let at = (y * w + x) * 3;
            let (r, g, b) = (
                i32::from(rgb[at]),
                i32::from(rgb[at + 1]),
                i32::from(rgb[at + 2]),
            );
            let luma = (77 * r + 150 * g + 29 * b + 128) >> 8;
            let d = f64::from(luma - i32::from(reconstruction.planes[0][y * w + x]));
            sse += d * d;
        }
    }
    let mse = sse / (w * h) as f64;
    if mse == 0.0 {
        99.0
    } else {
        10.0 * (255.0 * 255.0 / mse).log10()
    }
}

#[test]
fn the_geometry_tiles_as_the_level_rules_require() {
    // Ten superblocks across split into two columns of five; a frame
    // under eight stays one tile.
    let g = Geometry::new(640, 480).unwrap();
    assert_eq!(g.mi_grid(), (160, 120));
    assert_eq!(g.sb_grid(), (10, 8));
    assert_eq!((g.tile_cols(), g.tile_rows()), (2, 1));
    assert_eq!(g.level(), 4);
    assert_eq!(Geometry::new(640, 400).unwrap().level(), 1);
    let g = Geometry::new(300, 300).unwrap();
    assert_eq!((g.tile_cols(), g.tile_rows()), (1, 1));
    // Wider than 4096 needs two columns at least; 94 superblocks split
    // to sixteen columns of six, which level 6 allows, and the area
    // rule then needs no rows.
    let g = Geometry::new(6000, 4000).unwrap();
    assert_eq!((g.tile_cols(), g.tile_rows()), (16, 1));
    assert_eq!(g.level(), 16);
    // The level the size sets caps the columns: 4096x1024 is level 5
    // (eight columns), though the width would take sixteen.
    let g = Geometry::new(4096, 1024).unwrap();
    assert_eq!((g.tile_cols(), g.tile_rows()), (8, 1));
    assert_eq!(g.level(), 12);
    // Sixty-five superblocks across go in thirteen columns of five,
    // the sixteenth start past the edge, the last column the remainder.
    let g = Geometry::new(4160, 4480).unwrap();
    assert_eq!((g.tile_cols(), g.tile_rows()), (13, 1));
    assert_eq!(g.tile_col_starts()[..3], [0, 5, 10]);
    assert_eq!(g.tile_col_starts()[12..], [60, 65]);
    // At level 3 (the width is past level 2's) the cap is six columns,
    // so the same width goes in four columns of seventeen: eight would
    // pass the cap.
    let g = Geometry::new(4160, 8).unwrap();
    assert_eq!(g.tile_col_starts(), [0, 17, 34, 51, 65]);
    assert_eq!(g.level(), 4);
    // Past level 6 the area rule adds rows, and a column the cap left
    // wide can round up past the tile area: the rows then split again.
    let g = Geometry::new(16384, 16384).unwrap();
    assert_eq!((g.tile_cols(), g.tile_rows()), (16, 2));
    assert_eq!(g.tile_row_starts(), [0, 128, 256]);
    let g = Geometry::new(9217, 14721).unwrap();
    assert_eq!(g.sb_grid(), (145, 231));
    assert_eq!((g.tile_cols(), g.tile_rows()), (15, 2));
    assert_eq!(g.tile_row_starts(), [0, 116, 231]);
    assert_eq!(g.level(), 31);
    // Tile rows on request, no more than the superblock rows allow, and
    // the level accounts for them.
    let g = Geometry::tiled(300, 300, 1).unwrap();
    assert_eq!((g.tile_cols(), g.tile_rows()), (1, 2));
    assert_eq!(g.tile_row_starts(), [0, 3, 5]);
    let g = Geometry::tiled(300, 300, 4).unwrap();
    assert_eq!(g.tile_rows(), 5);
    assert_eq!(g.tile_row_starts(), [0, 1, 2, 3, 4, 5]);
    assert_eq!(g.level(), 0);
    // Twenty tiles are more than level 3 allows.
    assert_eq!(Geometry::tiled(640, 640, 4).unwrap().level(), 8);
    assert_eq!(Geometry::new(16384, 16384).unwrap().level(), 31);
    assert!(Geometry::new(0, 10).is_err());
    assert!(Geometry::new(10, 16385).is_err());
    assert_eq!(qindex(100), 1);
    assert_eq!(qindex(1), 255);
    assert_eq!(qindex(50), 129);
    for q in 2..=100u8 {
        assert!(qindex(q) < qindex(q - 1));
    }
}

#[test]
fn every_shape_codes_and_decodes_to_the_encoders_own_reconstruction() {
    for (width, height, quality, threads, rows_log2) in [
        (64, 64, 80, 1, 0),
        (1, 1, 50, 1, 0),
        (7, 5, 90, 1, 0),
        (128, 64, 80, 1, 0),
        (64, 40, 80, 1, 0),
        (40, 64, 80, 1, 0),
        (65, 33, 30, 1, 0),
        (200, 130, 70, 2, 0),
        (300, 70, 95, 1, 0),
        (520, 40, 60, 2, 0),
        (4160, 8, 50, 1, 0),
        // Tile rows: two, and one per superblock row with two columns.
        (200, 200, 75, 1, 1),
        (520, 150, 60, 2, 2),
    ] {
        let name = format!("p{width}x{height}q{quality}r{rows_log2}");
        let (obus, reconstruction) = encode_tiled(width, height, quality, threads, rows_log2);
        assert!(obus.len() > 10, "{name}");
        assert_eq!(reconstruction.width, width);
        assert_eq!(reconstruction.height, height);
        let (cw, ch) = (width.div_ceil(2), height.div_ceil(2));
        assert_eq!(reconstruction.planes[0].len(), width * height);
        assert_eq!(reconstruction.planes[1].len(), cw * ch);
        if let Some(decoded) = dav1d(&obus, &name) {
            let mut planes = Vec::new();
            for plane in &reconstruction.planes {
                planes.extend_from_slice(plane);
            }
            assert_eq!(decoded.len(), planes.len(), "{name}");
            let mismatch = decoded.iter().zip(&planes).position(|(a, b)| a != b);
            assert_eq!(mismatch, None, "{name}: dav1d differs from the encoder");
        }
    }
}

#[test]
fn quality_buys_fidelity_and_costs_bytes() {
    let rgb = picture(160, 96);
    let mut last_size = 0;
    let mut last_psnr = 0.0;
    for quality in [10, 40, 70, 95] {
        let (obus, reconstruction) = encode(160, 96, quality, 1);
        let psnr = luma_psnr(&rgb, &reconstruction);
        eprintln!("quality {quality}: {} bytes, {psnr:.2} dB", obus.len());
        assert!(obus.len() > last_size, "quality {quality} cost no more");
        assert!(psnr > last_psnr, "quality {quality} lost fidelity");
        last_size = obus.len();
        last_psnr = psnr;
    }
    assert!(last_psnr > 40.0, "{last_psnr}");
}

/// `f(n)` over a payload, most significant bit first.
fn bits(payload: &[u8], at: &mut usize, n: usize) -> u32 {
    let mut value = 0;
    for _ in 0..n {
        let bit = (payload[*at / 8] >> (7 - *at % 8)) & 1;
        value = (value << 1) | u32::from(bit);
        *at += 1;
    }
    value
}

#[test]
fn the_stream_opens_with_the_documented_headers() {
    let (obus, _) = encode(64, 48, 80, 1);
    // A sequence header OBU with a size field, then a frame OBU.
    assert_eq!(obus[0], 0x0A);
    let seq_len = usize::from(obus[1]);
    assert_eq!(obus[2 + seq_len], 0x32);
    let seq = &obus[2..2 + seq_len];
    let mut at = 0;
    assert_eq!(bits(seq, &mut at, 3), 0, "seq_profile");
    assert_eq!(bits(seq, &mut at, 1), 1, "still_picture");
    assert_eq!(bits(seq, &mut at, 1), 1, "reduced_still_picture_header");
    assert_eq!(bits(seq, &mut at, 5), 0, "seq_level_idx");
    assert_eq!(bits(seq, &mut at, 4), 15, "frame_width_bits_minus_1");
    assert_eq!(bits(seq, &mut at, 4), 15, "frame_height_bits_minus_1");
    assert_eq!(bits(seq, &mut at, 16), 63, "max_frame_width_minus_1");
    assert_eq!(bits(seq, &mut at, 16), 47, "max_frame_height_minus_1");
    assert_eq!(
        bits(seq, &mut at, 6),
        0,
        "128x128, filter/edge intra, superres, cdef, restoration"
    );
    assert_eq!(bits(seq, &mut at, 2), 0, "high_bitdepth, mono_chrome");
    assert_eq!(bits(seq, &mut at, 1), 1, "color_description_present_flag");
    assert_eq!(bits(seq, &mut at, 8), 1, "color_primaries BT.709");
    assert_eq!(bits(seq, &mut at, 8), 13, "transfer_characteristics sRGB");
    assert_eq!(bits(seq, &mut at, 8), 6, "matrix_coefficients BT.601");
    assert_eq!(bits(seq, &mut at, 1), 1, "color_range full");
    assert_eq!(bits(seq, &mut at, 2), 0, "chroma_sample_position");
    assert_eq!(
        bits(seq, &mut at, 2),
        0,
        "separate_uv_delta_q, film_grain_params_present"
    );
    assert_eq!(bits(seq, &mut at, 1), 1, "trailing one");
    assert_eq!(seq.len(), at.div_ceil(8));
}

#[test]
fn the_encoder_refuses_rows_that_do_not_fit() {
    let mut encoder = Encoder::new(8, 4, 50, 1).unwrap();
    assert!(encoder.encode_rows(&[0; 8 * 3 + 1]).is_err());
    assert!(encoder.encode_rows(&[0; 8 * 3 * 5]).is_err());
    encoder.encode_rows(&[0; 8 * 3 * 3]).unwrap();
    assert!(encoder.finish().is_err());
    assert!(Encoder::new(0, 4, 50, 1).is_err());
}

#[test]
#[ignore]
fn bench() {
    let (w, h) = (6000, 4000);
    let rgb = picture(w, h);
    for threads in [1, 8] {
        let start = std::time::Instant::now();
        let mut encoder = Encoder::new(w, h, 80, threads).unwrap();
        encoder.encode_rows(&rgb).unwrap();
        let obus = encoder.finish().unwrap();
        eprintln!(
            "{w}x{h} threads {threads}: {} bytes in {:?}",
            obus.len(),
            start.elapsed()
        );
    }
}

/// AVIF against the crate's JPEG encoder over the camera thumbnail
/// fixture, at matched qualities: bytes and luma PSNR.
#[test]
#[ignore]
fn compare_with_jpeg() {
    use td_photo::jpeg;
    let data = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/z8-thumb.jpg"
    ))
    .unwrap();
    let source = jpeg::decode(&data, jpeg::Scale::Full).unwrap();
    let (w, h) = (source.width, source.height);
    eprintln!("source {w}x{h}");
    for quality in [30, 50, 70, 85, 92, 97] {
        let mut encoder = Encoder::new(w, h, quality, 1).unwrap();
        encoder.keep_reconstruction();
        encoder.encode_rows(&source.data).unwrap();
        let (obus, reconstruction) = encoder.finish_with().unwrap();
        let avif_psnr = luma_psnr(&source.data, &reconstruction.unwrap());
        let mut je = jpeg::Encoder::new(w, h, quality, 1).unwrap();
        je.encode_rows(&source.data).unwrap();
        let jpg = je.finish().unwrap();
        let back = jpeg::decode(&jpg, jpeg::Scale::Full).unwrap();
        let mut sse = 0f64;
        for (a, b) in source
            .data
            .as_chunks::<3>()
            .0
            .iter()
            .zip(back.data.as_chunks::<3>().0)
        {
            let la =
                (77 * i32::from(a[0]) + 150 * i32::from(a[1]) + 29 * i32::from(a[2]) + 128) >> 8;
            let lb =
                (77 * i32::from(b[0]) + 150 * i32::from(b[1]) + 29 * i32::from(b[2]) + 128) >> 8;
            sse += f64::from((la - lb) * (la - lb));
        }
        let jpeg_psnr = 10.0 * (255.0 * 255.0 / (sse / (w * h) as f64)).log10();
        eprintln!(
            "q{quality}: avif {} bytes {avif_psnr:.2} dB | jpeg {} bytes {jpeg_psnr:.2} dB",
            obus.len(),
            jpg.len()
        );
    }
}

/// FNV-1a-64 of the bytes.
fn fnv(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

/// The streams are the bytes dav1d decoded once: the hashes were
/// recorded from a run with `TD_TEST_DAV1D` and every decode exact, so a
/// change to any emitted symbol reds here and is verified against the
/// decoder again before the hash moves. The thread count is not in the
/// bytes.
#[test]
fn the_streams_are_the_bytes_dav1d_decoded() {
    for (width, height, quality, rows_log2, hash) in [
        (65, 33, 30, 0, 0xce4a3b13c20c87fdu64),
        (520, 40, 60, 0, 0xb9d5a35ea1e5f2c9),
        (200, 200, 75, 1, 0x2fd0e5e8478c7be5),
    ] {
        let name = format!("g{width}x{height}q{quality}r{rows_log2}");
        let (obus, reconstruction) = encode_tiled(width, height, quality, 1, rows_log2);
        assert_eq!(
            encode_tiled(width, height, quality, 3, rows_log2).0,
            obus,
            "{name}: the thread count changes the bytes"
        );
        if let Some(decoded) = dav1d(&obus, &name) {
            let mut planes = Vec::new();
            for plane in &reconstruction.planes {
                planes.extend_from_slice(plane);
            }
            assert_eq!(decoded, planes, "{name}: dav1d differs from the encoder");
        }
        assert_eq!(fnv(&obus), hash, "{name}: {:#018x}", fnv(&obus));
    }
}

/// The container carries the stream where its `iloc` says, and the
/// stream it carries decodes: the item's extent read back from the file
/// is the OBUs, and dav1d decodes them to the reconstruction.
#[test]
fn the_container_carries_the_stream_where_it_says() {
    let (width, height, quality) = (300, 200, 60);
    let (obus, reconstruction) = encode(width, height, quality, 2);
    let geometry = Geometry::new(width, height).unwrap();
    let data = avif::file(&geometry, &obus);
    assert!(data.starts_with(b"\0\0\0\x20ftypavif"), "{:?}", &data[..16]);
    let at = data.windows(4).position(|w| w == b"iloc").unwrap();
    let offset = u32::from_be_bytes(data[at + 18..at + 22].try_into().unwrap()) as usize;
    let length = u32::from_be_bytes(data[at + 22..at + 26].try_into().unwrap()) as usize;
    assert_eq!(&data[offset..offset + length], &obus[..]);
    assert_eq!(
        &data[offset - 8..offset - 4],
        &(length as u32 + 8).to_be_bytes()
    );
    assert_eq!(&data[offset - 4..offset], b"mdat");
    assert_eq!(offset + length, data.len());
    if let Some(decoded) = dav1d(&data[offset..offset + length], "container") {
        let mut planes = Vec::new();
        for plane in &reconstruction.planes {
            planes.extend_from_slice(plane);
        }
        assert_eq!(decoded, planes, "dav1d differs from the encoder");
    }
}

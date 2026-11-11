#![forbid(unsafe_code)]
#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use td_jpeg::{decode, decode_with, header, Error, Scale};

const PAIRS: &[(&[u8], &[u8], &[u8])] = &[
    (
        include_bytes!("fixtures/progressive/grey-base.jpg"),
        include_bytes!("fixtures/progressive/grey-prog.jpg"),
        include_bytes!("fixtures/progressive/grey-restart-prog.jpg"),
    ),
    (
        include_bytes!("fixtures/progressive/444-base.jpg"),
        include_bytes!("fixtures/progressive/444-prog.jpg"),
        include_bytes!("fixtures/progressive/444-restart-prog.jpg"),
    ),
    (
        include_bytes!("fixtures/progressive/420-base.jpg"),
        include_bytes!("fixtures/progressive/420-prog.jpg"),
        include_bytes!("fixtures/progressive/420-restart-prog.jpg"),
    ),
    (
        include_bytes!("fixtures/progressive/422-base.jpg"),
        include_bytes!("fixtures/progressive/422-prog.jpg"),
        include_bytes!("fixtures/progressive/422-restart-prog.jpg"),
    ),
    (
        include_bytes!("fixtures/progressive/440-base.jpg"),
        include_bytes!("fixtures/progressive/440-prog.jpg"),
        include_bytes!("fixtures/progressive/440-restart-prog.jpg"),
    ),
    (
        include_bytes!("fixtures/progressive/fine-chroma-base.jpg"),
        include_bytes!("fixtures/progressive/fine-chroma-prog.jpg"),
        include_bytes!("fixtures/progressive/fine-chroma-restart-prog.jpg"),
    ),
];

#[test]
fn libjpeg_progression_matches_baseline_at_every_scale_and_sampling() {
    for &(base, progressive, restart) in PAIRS {
        for stream in [progressive, restart] {
            assert_eq!(header(stream).unwrap(), header(base).unwrap());
            for scale in [Scale::Full, Scale::Half, Scale::Quarter, Scale::Eighth] {
                assert_eq!(decode(stream, scale).unwrap(), decode(base, scale).unwrap());
            }
        }
    }
    // No padded dummy blocks in 4:4:4: callback order and every coefficient
    // are exactly those of the independently encoded baseline stream.
    let mut expected = Vec::new();
    decode_with(PAIRS[1].0, Scale::Full, &mut |b| expected.push(*b)).unwrap();
    for stream in [PAIRS[1].1, PAIRS[1].2] {
        let mut actual = Vec::new();
        decode_with(stream, Scale::Full, &mut |b| actual.push(*b)).unwrap();
        assert_eq!(actual, expected);
    }
}

#[test]
fn every_truncation_is_an_error_and_entropy_mutations_never_panic() {
    for (fixture, &(_, stream, restart)) in PAIRS.iter().enumerate() {
        for source in [stream, restart] {
            let stride = if fixture == 0 { 1 } else { 31 };
            for end in (0..source.len()).step_by(stride) {
                assert!(decode(&source[..end], Scale::Eighth).is_err(), "cut {end}");
            }
            // Change entropy bytes only: retain small declared dimensions.
            let mut mutated = source.to_vec();
            for pos in (0..source.len()).step_by(17) {
                if source[pos] == 0xff || source.get(pos.wrapping_sub(1)) == Some(&0xff) {
                    continue;
                }
                if pos < source.windows(2).position(|w| w == [0xff, 0xda]).unwrap() + 14 {
                    continue;
                }
                mutated[pos] ^= 0x55;
                if let Ok(image) = decode(&mutated, Scale::Eighth) {
                    assert!(image.is_consistent());
                }
                mutated[pos] = source[pos];
            }
        }
    }
}

fn segment(out: &mut Vec<u8>, marker: u8, data: &[u8]) {
    out.extend([0xff, marker]);
    out.extend(((data.len() + 2) as u16).to_be_bytes());
    out.extend(data);
}
fn tiny() -> Vec<u8> {
    let mut out = vec![0xff, 0xd8];
    segment(&mut out, 0xc2, &[8, 0, 8, 0, 8, 1, 1, 0x11, 0]);
    let mut quant = vec![0];
    quant.extend([1; 64]);
    segment(&mut out, 0xdb, &quant);
    for class in [0, 0x10] {
        let mut table = vec![class, 1];
        table.extend([0; 15]);
        table.push(0);
        segment(&mut out, 0xc4, &table);
    }
    out
}
fn scan(out: &mut Vec<u8>, ss: u8, se: u8, ah: u8, al: u8, entropy: u8) {
    segment(out, 0xda, &[1, 1, 0, ss, se, ah << 4 | al]);
    out.push(entropy);
}
fn ended(mut out: Vec<u8>) -> Vec<u8> {
    out.extend([0xff, 0xd9]);
    out
}

#[test]
fn progression_validation_and_partial_spectral_images() {
    let mut dc = tiny();
    scan(&mut dc, 0, 0, 0, 0, 0x7f);
    assert!(decode(&ended(dc.clone()), Scale::Full)
        .unwrap()
        .data
        .iter()
        .all(|b| *b == 128));
    for (ss, se, ah, al) in [
        (0, 0, 0, 0),
        (0, 0, 2, 1),
        (1, 63, 1, 0),
        (1, 64, 0, 0),
        (2, 1, 0, 0),
        (0, 1, 0, 0),
        (1, 1, 0, 14),
    ] {
        let mut out = dc.clone();
        scan(&mut out, ss, se, ah, al, 0x7f);
        assert_eq!(decode(&ended(out), Scale::Full), Err(Error::Scan));
    }
    let mut out = tiny();
    scan(&mut out, 1, 63, 0, 0, 0x7f);
    assert_eq!(decode(&ended(out), Scale::Full), Err(Error::Scan));
    let mut out = dc.clone();
    scan(&mut out, 1, 20, 0, 1, 0x7f);
    scan(&mut out, 10, 30, 0, 0, 0x7f);
    assert_eq!(decode(&ended(out), Scale::Full), Err(Error::Scan));
    // An unread whole byte must not be treated as padding.
    dc.extend([0, 0]);
    assert_eq!(decode(&ended(dc), Scale::Full), Err(Error::Scan));
}

#[test]
fn scan_and_metadata_budgets_are_enforced() {
    let mut out = tiny();
    scan(&mut out, 0, 0, 0, 0, 0x7f);
    for k in 1..64 {
        scan(&mut out, k, k, 0, 4, 0x7f);
    }
    for ah in (1..=4).rev() {
        for k in 1..64 {
            scan(&mut out, k, k, ah, ah - 1, 0x7f);
        }
    }
    assert_eq!(decode(&ended(out), Scale::Eighth), Err(Error::Limit));
    let mut out = vec![0xff, 0xd8];
    for _ in 0..td_jpeg::MAX_SEGMENTS {
        segment(&mut out, 0xfe, &[]);
    }
    out.extend(&tiny()[2..]);
    assert_eq!(header(&out), Err(Error::Limit));
}

fn ac_symbol(out: &mut Vec<u8>, symbol: u8) {
    let mut table = vec![0x10, 1];
    table.extend([0; 15]);
    table.push(symbol);
    segment(out, 0xc4, &table);
}

#[test]
fn eob_runs_and_zrl_must_fit_the_scan() {
    for (symbol, end, entropy) in [(0x10, 63, 0x3f), (0xf0, 15, 0x7f)] {
        let mut out = tiny();
        scan(&mut out, 0, 0, 0, 0, 0x7f);
        ac_symbol(&mut out, symbol);
        scan(&mut out, 1, end, 0, 0, entropy);
        assert_eq!(decode(&ended(out), Scale::Full), Err(Error::Run));
    }
    let source = PAIRS[2].2;
    let rst = source.windows(2).position(|w| w == [0xff, 0xd0]).unwrap();
    let mut broken = source.to_vec();
    broken[rst + 1] = 0xd1;
    assert_eq!(decode(&broken, Scale::Full), Err(Error::Restart));
}

#[test]
fn quantiser_is_latched_and_dc_refinement_uses_twos_complement() {
    let mut out = tiny();
    // DC category 1, value -1; initial Al=1 represents -2.
    let mut table = vec![0, 1];
    table.extend([0; 15]);
    table.push(1);
    segment(&mut out, 0xc4, &table);
    scan(&mut out, 0, 0, 0, 1, 0x3f);
    scan(&mut out, 0, 0, 1, 0, 0xff);
    out.push(0);
    let mut quant = vec![0];
    quant.extend([127; 64]);
    segment(&mut out, 0xdb, &quant);
    let mut coefficients = Vec::new();
    decode_with(&ended(out), Scale::Eighth, &mut |b| coefficients.push(*b)).unwrap();
    assert_eq!(coefficients[0][0], -1);
}

#[test]
fn coefficient_budget_is_checked_before_allocating_large_planes() {
    let mut out = vec![0xff, 0xd8];
    // Within the old sample/pixel bounds, beyond the progressive storage
    // cap even at Eighth scale. This needs no large test allocation.
    segment(
        &mut out,
        0xc2,
        &[8, 0x10, 1, 0x20, 0, 3, 1, 0x11, 0, 2, 0x11, 0, 3, 0x11, 0],
    );
    let prefix = tiny();
    let tables = prefix.windows(2).position(|w| w == [0xff, 0xdb]).unwrap();
    out.extend(&prefix[tables..]);
    scan(&mut out, 0, 0, 0, 0, 0x7f);
    let out = ended(out);
    assert!(header(&out).is_ok());
    assert_eq!(decode(&out, Scale::Eighth), Err(Error::Limit));
}

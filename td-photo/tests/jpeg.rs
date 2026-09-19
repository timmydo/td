#![forbid(unsafe_code)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

//! The preview decoder contract: a synthetic baseline JPEG writer drives
//! round trips at every scale, sampling and restart shape against an
//! in-test reference of the same arithmetic; the real Z 8 thumbnail is held
//! to the hashes an independent Python transcription of T.81 produced; and
//! every refusal DESIGN.md names is exercised.

use std::path::Path;

use td_photo::image::{Rgb8, MAX_AXIS};
use td_photo::jpeg::{self, Error, Scale, MAX_PREVIEW_SAMPLES};

const ZIGZAG: [usize; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20,
    13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59,
    52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

fn lcg(seed: &mut u64) -> u32 {
    *seed = seed
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    (*seed >> 33) as u32
}

// ------------------------------------------------------------------ writer

#[derive(Default)]
struct BitWriter {
    out: Vec<u8>,
    acc: u32,
    n: u32,
}

impl BitWriter {
    fn put(&mut self, value: u32, bits: u32) {
        for i in (0..bits).rev() {
            self.acc = (self.acc << 1) | ((value >> i) & 1);
            self.n += 1;
            if self.n == 8 {
                let b = self.acc as u8;
                self.out.push(b);
                if b == 0xFF {
                    self.out.push(0);
                }
                self.acc = 0;
                self.n = 0;
            }
        }
    }

    /// Pads the last byte with one bits, as the standard requires.
    fn flush(&mut self) {
        while self.n != 0 {
            self.put(1, 1);
        }
    }
}

/// A canonical Huffman table: `counts` per length and the symbols in code
/// order, plus each symbol's code.
#[derive(Clone)]
struct Table {
    counts: [u8; 16],
    symbols: Vec<u8>,
    codes: Vec<Option<(u32, u32)>>,
}

fn table(counts: [u8; 16], symbols: Vec<u8>) -> Table {
    let mut codes = vec![None; 256];
    let mut code = 0u32;
    let mut k = 0;
    for (i, count) in counts.iter().enumerate() {
        let len = i as u32 + 1;
        for _ in 0..*count {
            codes[usize::from(symbols[k])] = Some((code, len));
            k += 1;
            code += 1;
        }
        code <<= 1;
    }
    Table {
        counts,
        symbols,
        codes,
    }
}

/// Twelve DC categories, all four bits long.
fn dc_table() -> Table {
    let mut counts = [0u8; 16];
    counts[3] = 12;
    table(counts, (0..=11).collect())
}

/// EOB, ZRL and every run/size pair of baseline: 128 eight-bit codes and
/// 34 nine-bit ones, an incomplete code with no all-ones word.
fn ac_table() -> Table {
    let mut counts = [0u8; 16];
    counts[7] = 128;
    counts[8] = 34;
    let mut symbols = vec![0x00u8, 0xF0];
    for run in 0..16u8 {
        for size in 1..=10u8 {
            symbols.push(run << 4 | size);
        }
    }
    assert_eq!(symbols.len(), 162);
    table(counts, symbols)
}

fn segment(marker: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = vec![0xFF, marker];
    out.extend_from_slice(&((payload.len() + 2) as u16).to_be_bytes());
    out.extend_from_slice(payload);
    out
}

fn dht_payload(class: u8, id: u8, t: &Table) -> Vec<u8> {
    let mut p = vec![class << 4 | id];
    p.extend_from_slice(&t.counts);
    p.extend_from_slice(&t.symbols);
    p
}

fn magnitude(v: i32) -> (u32, u32) {
    let size = 32 - v.unsigned_abs().leading_zeros();
    let bits = if v >= 0 {
        v as u32
    } else {
        (v + (1 << size) - 1) as u32
    };
    (size, bits & ((1u32 << size) - 1))
}

#[derive(Clone)]
struct Comp {
    id: u8,
    h: u8,
    v: u8,
    tq: u8,
    /// Quantised coefficients, natural order, in decode order.
    blocks: Vec<[i32; 64]>,
}

#[derive(Clone)]
struct Synth {
    width: u16,
    height: u16,
    precision: u8,
    sof: u8,
    /// Quantisation tables in zigzag order.
    quant: Vec<[u16; 64]>,
    comps: Vec<Comp>,
    restart: u16,
    /// Overrides the scan's component count.
    scan_count: Option<u8>,
    /// Emit the Huffman tables (a stream without them is refused).
    tables: bool,
    /// Emit an over-full DC table instead of the good one.
    bad_dht: bool,
    /// Tables in place of the standard ones, for streams built by hand.
    dc: Option<Table>,
    ac: Option<Table>,
    /// Replace the entropy-coded data with these bytes.
    raw_scan: Option<Vec<u8>>,
    /// Extra bytes between SOI and the first segment.
    prefix: Vec<u8>,
}

fn flat_quant(value: u16) -> [u16; 64] {
    [value; 64]
}

impl Synth {
    fn mcu_grid(&self) -> (usize, usize) {
        let single = self.comps.len() == 1;
        let hmax = self.comps.iter().map(|c| usize::from(c.h)).max().unwrap();
        let vmax = self.comps.iter().map(|c| usize::from(c.v)).max().unwrap();
        let (w, h) = (usize::from(self.width), usize::from(self.height));
        if single {
            (w.div_ceil(8), h.div_ceil(8))
        } else {
            (w.div_ceil(8 * hmax), h.div_ceil(8 * vmax))
        }
    }

    fn blocks_per_mcu(&self, comp: &Comp) -> (usize, usize) {
        if self.comps.len() == 1 {
            (1, 1)
        } else {
            (usize::from(comp.h), usize::from(comp.v))
        }
    }

    fn encode(&self) -> Vec<u8> {
        let mut out = vec![0xFF, 0xD8];
        out.extend_from_slice(&self.prefix);
        for (id, q) in self.quant.iter().enumerate() {
            let sixteen = q.iter().any(|v| *v > 255);
            let mut p = vec![u8::from(sixteen) << 4 | id as u8];
            for v in q {
                if sixteen {
                    p.extend_from_slice(&v.to_be_bytes());
                } else {
                    p.push(*v as u8);
                }
            }
            out.extend_from_slice(&segment(0xDB, &p));
        }
        let mut sof = vec![self.precision];
        sof.extend_from_slice(&self.height.to_be_bytes());
        sof.extend_from_slice(&self.width.to_be_bytes());
        sof.push(self.comps.len() as u8);
        for c in &self.comps {
            sof.extend_from_slice(&[c.id, c.h << 4 | c.v, c.tq]);
        }
        out.extend_from_slice(&segment(self.sof, &sof));
        let dc = self.dc.clone().unwrap_or_else(dc_table);
        let ac = self.ac.clone().unwrap_or_else(ac_table);
        if self.tables {
            if self.bad_dht {
                let mut counts = [0u8; 16];
                counts[0] = 3;
                let bad = table(counts, vec![0, 1, 2]);
                out.extend_from_slice(&segment(0xC4, &dht_payload(0, 0, &bad)));
            } else {
                out.extend_from_slice(&segment(0xC4, &dht_payload(0, 0, &dc)));
            }
            out.extend_from_slice(&segment(0xC4, &dht_payload(1, 0, &ac)));
        }
        if self.restart != 0 {
            out.extend_from_slice(&segment(0xDD, &self.restart.to_be_bytes()));
        }
        let count = self.scan_count.unwrap_or(self.comps.len() as u8);
        let mut sos = vec![count];
        for c in self.comps.iter().take(usize::from(count)) {
            sos.extend_from_slice(&[c.id, 0x00]);
        }
        sos.extend_from_slice(&[0, 63, 0]);
        out.extend_from_slice(&segment(0xDA, &sos));
        if let Some(raw) = &self.raw_scan {
            out.extend_from_slice(raw);
        } else {
            out.extend_from_slice(&self.scan(&dc, &ac));
        }
        out.extend_from_slice(&[0xFF, 0xD9]);
        out
    }

    fn scan(&self, dc: &Table, ac: &Table) -> Vec<u8> {
        let (mcus_x, mcus_y) = self.mcu_grid();
        let mut w = BitWriter::default();
        let mut preds = vec![0i32; self.comps.len()];
        let mut next = vec![0usize; self.comps.len()];
        let mut mcu = 0usize;
        let mut restarts = 0u8;
        for _ in 0..mcus_y {
            for _ in 0..mcus_x {
                if self.restart != 0 && mcu != 0 && mcu.is_multiple_of(usize::from(self.restart)) {
                    w.flush();
                    w.out.extend_from_slice(&[0xFF, 0xD0 + (restarts % 8)]);
                    restarts = restarts.wrapping_add(1);
                    preds.iter_mut().for_each(|p| *p = 0);
                }
                mcu += 1;
                for (ci, comp) in self.comps.iter().enumerate() {
                    let (bh, bv) = self.blocks_per_mcu(comp);
                    for _ in 0..bh * bv {
                        let block = &comp.blocks[next[ci]];
                        next[ci] += 1;
                        let diff = block[0] - preds[ci];
                        preds[ci] = block[0];
                        let (size, bits) = magnitude(diff);
                        assert!(size <= 11, "DC category {size}");
                        let (code, len) = dc.codes[size as usize].unwrap();
                        w.put(code, len);
                        w.put(bits, size);
                        let mut run = 0u32;
                        for k in 1..64 {
                            let v = block[ZIGZAG[k]];
                            if v == 0 {
                                run += 1;
                                continue;
                            }
                            while run > 15 {
                                let (code, len) = ac.codes[0xF0].unwrap();
                                w.put(code, len);
                                run -= 16;
                            }
                            let (size, bits) = magnitude(v);
                            assert!(size <= 10, "AC category {size}");
                            let (code, len) = ac.codes[(run << 4 | size) as usize].unwrap();
                            w.put(code, len);
                            w.put(bits, size);
                            run = 0;
                        }
                        if run > 0 {
                            let (code, len) = ac.codes[0].unwrap();
                            w.put(code, len);
                        }
                    }
                }
            }
        }
        w.flush();
        w.out
    }
}

/// Random quantised blocks: DC within ±dc, a quarter of the ACs nonzero
/// within ±ac, every eighth block empty but for its last coefficient.
fn random_blocks(count: usize, seed: u64, dc: i32, ac: i32) -> Vec<[i32; 64]> {
    let mut s = seed;
    (0..count)
        .map(|i| {
            let mut b = [0i32; 64];
            b[0] = (lcg(&mut s) % (2 * dc as u32 + 1)) as i32 - dc;
            if i % 8 == 7 {
                b[63] = 3;
                return b;
            }
            for slot in b.iter_mut().skip(1) {
                if lcg(&mut s).is_multiple_of(4) {
                    *slot = (lcg(&mut s) % (2 * ac as u32 + 1)) as i32 - ac;
                }
            }
            b
        })
        .collect()
}

fn grey(width: u16, height: u16, seed: u64) -> Synth {
    let mut s = Synth {
        width,
        height,
        precision: 8,
        sof: 0xC0,
        quant: vec![flat_quant(2)],
        comps: vec![Comp {
            id: 1,
            h: 1,
            v: 1,
            tq: 0,
            blocks: Vec::new(),
        }],
        restart: 0,
        scan_count: None,
        tables: true,
        bad_dht: false,
        dc: None,
        ac: None,
        raw_scan: None,
        prefix: Vec::new(),
    };
    let (mx, my) = s.mcu_grid();
    s.comps[0].blocks = random_blocks(mx * my, seed, 900, 300);
    s
}

fn colour(width: u16, height: u16, sampling: (u8, u8), seed: u64) -> Synth {
    let mut s = Synth {
        width,
        height,
        precision: 8,
        sof: 0xC0,
        quant: vec![flat_quant(3), flat_quant(5)],
        comps: vec![
            Comp {
                id: 1,
                h: sampling.0,
                v: sampling.1,
                tq: 0,
                blocks: Vec::new(),
            },
            Comp {
                id: 2,
                h: 1,
                v: 1,
                tq: 1,
                blocks: Vec::new(),
            },
            Comp {
                id: 3,
                h: 1,
                v: 1,
                tq: 1,
                blocks: Vec::new(),
            },
        ],
        restart: 0,
        scan_count: None,
        tables: true,
        bad_dht: false,
        dc: None,
        ac: None,
        raw_scan: None,
        prefix: Vec::new(),
    };
    let (mx, my) = s.mcu_grid();
    for (i, comp) in s.comps.clone().iter().enumerate() {
        let (bh, bv) = s.blocks_per_mcu(comp);
        s.comps[i].blocks = random_blocks(mx * my * bh * bv, seed + i as u64, 600, 200);
    }
    s
}

// --------------------------------------------------------------- reference

// The decoder's tables, copied rather than imported so that an edit to
// the crate's constants alone moves every round trip; that the values are
// the formula's is the oracle's business, pinned by the fixture's hashes.
const T8: [[f64; 8]; 8] = [
    [
        0.35355339059327373,
        0.4903926402016152,
        0.46193976625564337,
        0.4157348061512726,
        0.3535533905932738,
        0.27778511650980114,
        0.19134171618254492,
        0.09754516100806417,
    ],
    [
        0.35355339059327373,
        0.4157348061512726,
        0.19134171618254492,
        -0.0975451610080641,
        -0.35355339059327373,
        -0.4903926402016152,
        -0.4619397662556434,
        -0.2777851165098011,
    ],
    [
        0.35355339059327373,
        0.27778511650980114,
        -0.19134171618254486,
        -0.4903926402016152,
        -0.35355339059327384,
        0.09754516100806415,
        0.46193976625564326,
        0.41573480615127273,
    ],
    [
        0.35355339059327373,
        0.09754516100806417,
        -0.46193976625564337,
        -0.2777851165098011,
        0.3535533905932737,
        0.41573480615127273,
        -0.19134171618254495,
        -0.4903926402016153,
    ],
    [
        0.35355339059327373,
        -0.0975451610080641,
        -0.4619397662556434,
        0.2777851165098009,
        0.35355339059327384,
        -0.41573480615127256,
        -0.19134171618254528,
        0.4903926402016152,
    ],
    [
        0.35355339059327373,
        -0.277785116509801,
        -0.19134171618254517,
        0.4903926402016152,
        -0.35355339059327334,
        -0.09754516100806401,
        0.46193976625564337,
        -0.4157348061512725,
    ],
    [
        0.35355339059327373,
        -0.4157348061512727,
        0.191341716182545,
        0.09754516100806439,
        -0.35355339059327356,
        0.4903926402016153,
        -0.4619397662556432,
        0.27778511650980076,
    ],
    [
        0.35355339059327373,
        -0.4903926402016152,
        0.46193976625564326,
        -0.41573480615127256,
        0.3535533905932733,
        -0.27778511650980076,
        0.19134171618254478,
        -0.09754516100806429,
    ],
];
const T4: [[f64; 4]; 4] = [
    [
        0.35355339059327373,
        0.46193976625564337,
        0.3535533905932738,
        0.19134171618254492,
    ],
    [
        0.35355339059327373,
        0.19134171618254492,
        -0.35355339059327373,
        -0.4619397662556434,
    ],
    [
        0.35355339059327373,
        -0.19134171618254486,
        -0.35355339059327384,
        0.46193976625564326,
    ],
    [
        0.35355339059327373,
        -0.46193976625564337,
        0.3535533905932737,
        -0.19134171618254495,
    ],
];
const T2: [[f64; 2]; 2] = [
    [0.35355339059327373, 0.3535533905932738],
    [0.35355339059327373, -0.35355339059327373],
];
const T1: [[f64; 1]; 1] = [[0.35355339059327373]];

/// The same arithmetic as the decoder, written the long way over the same
/// literal tables, so every synthetic round trip is exact.
fn reference_block(deq: &[i32; 64], n: usize) -> Vec<u8> {
    let t = |u: usize, x: usize| -> f64 {
        match n {
            8 => T8[x][u],
            4 => T4[x][u],
            2 => T2[x][u],
            _ => T1[x][u],
        }
    };
    let mut tmp = vec![0.0f64; n * n]; // [v][x]
    for v in 0..n {
        for x in 0..n {
            let mut s = 0.0;
            for u in 0..n {
                s += t(u, x) * f64::from(deq[v * 8 + u]);
            }
            tmp[v * n + x] = s;
        }
    }
    let mut out = vec![0u8; n * n];
    for x in 0..n {
        for y in 0..n {
            let mut s = 0.0;
            for v in 0..n {
                s += t(v, y) * tmp[v * n + x];
            }
            out[y * n + x] = (s + 128.0 + 0.5).floor().clamp(0.0, 255.0) as u8;
        }
    }
    out
}

fn reference(synth: &Synth, scale: Scale) -> Rgb8 {
    let n = scale.n();
    let (mcus_x, mcus_y) = synth.mcu_grid();
    let single = synth.comps.len() == 1;
    let hmax = synth.comps.iter().map(|c| c.h).max().unwrap();
    let vmax = synth.comps.iter().map(|c| c.v).max().unwrap();
    struct Plane {
        w: usize,
        h: usize,
        px: Vec<u8>,
        xs: u32,
        ys: u32,
    }
    let mut planes = Vec::new();
    for comp in &synth.comps {
        let (bh, bv) = synth.blocks_per_mcu(comp);
        let (w, h) = (mcus_x * bh * n, mcus_y * bv * n);
        let mut px = vec![0u8; w * h];
        let q = &synth.quant[usize::from(comp.tq)];
        let mut next = 0;
        for my in 0..mcus_y {
            for mx in 0..mcus_x {
                for by in 0..bv {
                    for bx in 0..bh {
                        let block = &comp.blocks[next];
                        next += 1;
                        let mut deq = [0i32; 64];
                        for k in 0..64 {
                            deq[ZIGZAG[k]] = block[ZIGZAG[k]] * i32::from(q[k]);
                        }
                        let samples = reference_block(&deq, n);
                        let (ox, oy) = ((mx * bh + bx) * n, (my * bv + by) * n);
                        for yy in 0..n {
                            for xx in 0..n {
                                px[(oy + yy) * w + ox + xx] = samples[yy * n + xx];
                            }
                        }
                    }
                }
            }
        }
        let (xs, ys) = if single {
            (0, 0)
        } else {
            (u32::from(hmax / comp.h) - 1, u32::from(vmax / comp.v) - 1)
        };
        planes.push(Plane { w, h, px, xs, ys });
    }
    let ow = (usize::from(synth.width) * n).div_ceil(8);
    let oh = (usize::from(synth.height) * n).div_ceil(8);
    let mut image = Rgb8::new(ow, oh).unwrap();
    let at = |p: &Plane, x: usize, y: usize| -> f64 {
        f64::from(p.px[(y >> p.ys).min(p.h - 1) * p.w + (x >> p.xs).min(p.w - 1)])
    };
    for y in 0..oh {
        for x in 0..ow {
            let i = (y * ow + x) * 3;
            if single {
                let v = at(&planes[0], x, y) as u8;
                image.data[i..i + 3].copy_from_slice(&[v, v, v]);
            } else {
                let yy = at(&planes[0], x, y);
                let cb = at(&planes[1], x, y) - 128.0;
                let cr = at(&planes[2], x, y) - 128.0;
                let r = (yy + 1.402 * cr + 0.5).floor().clamp(0.0, 255.0);
                let g = (yy - 0.344136 * cb - 0.714136 * cr + 0.5)
                    .floor()
                    .clamp(0.0, 255.0);
                let b = (yy + 1.772 * cb + 0.5).floor().clamp(0.0, 255.0);
                image.data[i..i + 3].copy_from_slice(&[r as u8, g as u8, b as u8]);
            }
        }
    }
    image
}

/// Rebuilds every component's blocks for the current geometry.
fn regenerate(s: &mut Synth, seed: u64) {
    let (mx, my) = s.mcu_grid();
    for (i, comp) in s.comps.clone().iter().enumerate() {
        let (bh, bv) = s.blocks_per_mcu(comp);
        s.comps[i].blocks = random_blocks(mx * my * bh * bv, seed + i as u64, 600, 200);
    }
}

#[test]
fn chroma_finer_than_luma_round_trips_exactly() {
    for (chroma, w, h) in [
        ((2u8, 1u8), 21u16, 13u16),
        ((1, 2), 17, 30),
        ((2, 2), 33, 9),
    ] {
        let mut s = colour(w, h, (1, 1), 47);
        s.comps[1].h = chroma.0;
        s.comps[1].v = chroma.1;
        regenerate(&mut s, 47);
        let file = s.encode();
        for scale in SCALES {
            assert_close(
                &jpeg::decode(&file, scale).unwrap(),
                &reference(&s, scale),
                0,
                &format!("chroma {chroma:?} {w}x{h} {scale:?}"),
            );
        }
    }
}

fn assert_close(decoded: &Rgb8, expected: &Rgb8, tolerance: i32, what: &str) {
    assert_eq!(
        (decoded.width, decoded.height),
        (expected.width, expected.height),
        "{what}"
    );
    let mut worst = 0;
    for (i, (a, b)) in decoded.data.iter().zip(expected.data.iter()).enumerate() {
        let d = (i32::from(*a) - i32::from(*b)).abs();
        assert!(d <= tolerance, "{what}: byte {i} decoded {a} expected {b}");
        worst = worst.max(d);
    }
    assert!(worst <= tolerance);
}

const SCALES: [Scale; 4] = [Scale::Full, Scale::Half, Scale::Quarter, Scale::Eighth];

// -------------------------------------------------------------- round trips

#[test]
fn a_flat_grey_block_decodes_to_its_level_at_every_scale() {
    let mut s = grey(8, 8, 1);
    s.quant = vec![flat_quant(1)];
    // DC 8 * (200 - 128) = 576 over an identity quantiser: every sample 200.
    s.comps[0].blocks = vec![{
        let mut b = [0i32; 64];
        b[0] = 576;
        b
    }];
    let file = s.encode();
    for scale in SCALES {
        let image = jpeg::decode(&file, scale).unwrap();
        let n = scale.n();
        assert_eq!((image.width, image.height), (n, n), "{scale:?}");
        assert!(
            image.data.iter().all(|v| *v == 200),
            "{scale:?} {:?}",
            image.data
        );
    }
    assert_eq!(
        jpeg::header(&file).unwrap(),
        jpeg::Header {
            width: 8,
            height: 8,
            components: 1,
            sampling: (1, 1)
        }
    );
}

#[test]
fn grey_frames_with_partial_blocks_round_trip_at_every_scale() {
    for (w, h, seed) in [(20u16, 13u16, 3u64), (1, 1, 4), (9, 24, 5), (64, 8, 6)] {
        let s = grey(w, h, seed);
        let file = s.encode();
        for scale in SCALES {
            let decoded = jpeg::decode(&file, scale).unwrap();
            assert_close(
                &decoded,
                &reference(&s, scale),
                0,
                &format!("{w}x{h} {scale:?}"),
            );
        }
    }
}

#[test]
fn colour_frames_of_every_sampling_round_trip_with_restarts() {
    for (sampling, w, h, restart, seed) in [
        ((1u8, 1u8), 37u16, 23u16, 0u16, 10u64),
        ((2, 1), 33, 17, 3, 11),
        ((2, 2), 45, 31, 2, 12),
        ((1, 2), 24, 40, 5, 13),
        ((2, 1), 16, 8, 1, 14),
        ((2, 2), 160, 120, 7, 15),
    ] {
        let mut s = colour(w, h, sampling, seed);
        s.restart = restart;
        if sampling == (2, 2) {
            // A sixteen-bit quantisation table for the chroma.
            s.quant[1] = flat_quant(300);
        }
        let file = s.encode();
        for scale in SCALES {
            let decoded = jpeg::decode(&file, scale).unwrap();
            assert_close(
                &decoded,
                &reference(&s, scale),
                0,
                &format!("{sampling:?} {w}x{h} restart {restart} {scale:?}"),
            );
        }
        let header = jpeg::header(&file).unwrap();
        assert_eq!(
            (header.width, header.height),
            (usize::from(w), usize::from(h))
        );
        assert_eq!(header.components, 3);
        assert_eq!(header.sampling, sampling);
    }
}

#[test]
fn fill_bytes_and_application_segments_are_skipped() {
    let mut s = grey(16, 16, 20);
    // Three fill bytes, an odd count: a reader that skipped them in pairs
    // (as the oracle once did) loses the marker here.
    let mut prefix = vec![0xFF, 0xFF, 0xFF, 0xFF, 0xE0];
    prefix.extend_from_slice(&16u16.to_be_bytes());
    prefix.extend_from_slice(b"JFIF\0\x01\x02\0\0\x01\0\x01\0\0");
    prefix.extend_from_slice(&[0xFF, 0xFE, 0x00, 0x05, b'h', b'i', b'!']);
    s.prefix = prefix;
    let file = s.encode();
    let decoded = jpeg::decode(&file, Scale::Full).unwrap();
    assert_close(&decoded, &reference(&s, Scale::Full), 0, "prefixed");
}

// ---------------------------------------------------------------- the real

fn thumb() -> Vec<u8> {
    std::fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/z8-thumb.jpg"))
        .unwrap()
}

#[test]
fn the_z8_thumbnail_decodes_to_the_oracles_hashes() {
    let data = thumb();
    assert_eq!(data.len(), 13063);
    assert_eq!(
        jpeg::header(&data).unwrap(),
        jpeg::Header {
            width: 160,
            height: 120,
            components: 3,
            sampling: (2, 1)
        }
    );
    // Every dequantised coefficient, integer-exact.
    let mut coefficients = Vec::new();
    let mut blocks = 0usize;
    let image = jpeg::decode_with(&data, Scale::Full, &mut |block: &[i32; 64]| {
        blocks += 1;
        for c in block {
            coefficients.extend_from_slice(&c.to_le_bytes());
        }
    })
    .unwrap();
    assert_eq!(blocks, 600);
    assert_eq!(fnv1a64(&coefficients), 0x4012c2335efb6bfa);
    // The pixels, exact, at every scale.
    assert_eq!((image.width, image.height), (160, 120));
    assert_eq!(fnv1a64(&image.data), 0x15cf1df123ee575d);
    for (scale, size, hash) in [
        (Scale::Half, (80, 60), 0x164a0dbefb89395du64),
        (Scale::Quarter, (40, 30), 0x7c9a6f6b601504ab),
        (Scale::Eighth, (20, 15), 0x8e6c3e050993ae30),
    ] {
        let image = jpeg::decode(&data, scale).unwrap();
        assert_eq!((image.width, image.height), size, "{scale:?}");
        assert_eq!(fnv1a64(&image.data), hash, "{scale:?}");
    }
    // Plausibility: a daylight photograph, not noise.
    let mean = image.data.iter().map(|v| u32::from(*v)).sum::<u32>() / image.data.len() as u32;
    assert!((40..=200).contains(&mean), "mean {mean}");
}

#[test]
fn scales_cover_the_asked_long_edge_from_the_coarsest() {
    assert_eq!(Scale::covering(1620, 1080, 400), Scale::Quarter);
    assert_eq!(Scale::covering(1620, 1080, 406), Scale::Half);
    assert_eq!(Scale::covering(1620, 1080, 811), Scale::Full);
    // 1620 / 8 rounds up to 203, so an eighth still covers 203.
    assert_eq!(Scale::covering(1620, 1080, 204), Scale::Quarter);
    assert_eq!(Scale::covering(1620, 1080, 203), Scale::Eighth);
    assert_eq!(Scale::covering(1620, 1080, 1), Scale::Eighth);
    assert_eq!(Scale::covering(160, 120, 400), Scale::Full);
    assert_eq!(Scale::covering(8256, 5504, 1600), Scale::Quarter);
    assert_eq!(Scale::covering(8256, 5504, 2065), Scale::Half);
    assert_eq!(jpeg::scaled(1620, Scale::Quarter), 405);
    assert_eq!(jpeg::scaled(1080, Scale::Eighth), 135);
    assert_eq!(jpeg::scaled(1, Scale::Eighth), 1);
    assert_eq!(jpeg::scaled(usize::MAX, Scale::Full), 16384);
    assert_eq!(
        Scale::Full.n() + Scale::Half.n() + Scale::Quarter.n() + Scale::Eighth.n(),
        15
    );
}

// ---------------------------------------------------------------- refusals

#[test]
fn every_named_refusal_is_reported() {
    let good = grey(16, 16, 30);
    let file = good.encode();
    assert!(jpeg::decode(&file, Scale::Full).is_ok());

    assert_eq!(jpeg::header(b"").unwrap_err(), Error::NotJpeg);
    assert_eq!(
        jpeg::header(b"\xff\xd8\x12\x34").unwrap_err(),
        Error::NotJpeg
    );
    assert_eq!(jpeg::header(b"P6\n1 1\n255\n").unwrap_err(), Error::NotJpeg);
    assert_eq!(jpeg::header(b"\xff\xd8\xff").unwrap_err(), Error::Truncated);
    assert_eq!(
        jpeg::header(b"\xff\xd8\xff\xd9").unwrap_err(),
        Error::Segment
    );
    assert_eq!(
        jpeg::header(b"\xff\xd8\xff\xdb\x00\x01").unwrap_err(),
        Error::Segment
    );
    assert_eq!(
        jpeg::header(b"\xff\xd8\xff\xdb\x00\x10\x00").unwrap_err(),
        Error::Truncated
    );

    let mut s = good.clone();
    s.sof = 0xC2;
    assert_eq!(
        jpeg::header(&s.encode()).unwrap_err(),
        Error::Unsupported(0xC2)
    );
    let mut s = good.clone();
    s.precision = 12;
    assert_eq!(jpeg::header(&s.encode()).unwrap_err(), Error::Precision(12));
    let mut s = colour(16, 16, (1, 1), 31);
    s.comps.pop();
    s.comps.truncate(2);
    assert_eq!(jpeg::header(&s.encode()).unwrap_err(), Error::Components(2));
    let mut s = colour(16, 16, (1, 1), 31);
    s.comps[2].id = 1;
    assert_eq!(jpeg::header(&s.encode()).unwrap_err(), Error::Components(3));
    let mut s = good.clone();
    s.comps[0].h = 3;
    assert_eq!(jpeg::header(&s.encode()).unwrap_err(), Error::Sampling);
    let mut s = good.clone();
    s.comps[0].v = 0;
    assert_eq!(jpeg::header(&s.encode()).unwrap_err(), Error::Sampling);
    for (w, h) in [(0u16, 8u16), (8, 0), (16385, 8), (8, 16385), (16384, 8193)] {
        let mut s = good.clone();
        s.width = w;
        s.height = h;
        // The header is refused before any scan is read.
        s.raw_scan = Some(Vec::new());
        assert_eq!(
            jpeg::header(&s.encode()).unwrap_err(),
            Error::Axis {
                width: usize::from(w),
                height: usize::from(h)
            }
        );
    }
    // 16384 x 8193 has more pixels than one image buffer may hold
    // (MAX_IMAGE_PIXELS, 64 Mi) while both axes fit. The plane budget is
    // separate and counts padding: 8192 x 8192 with three full-resolution
    // components (192 Mi) is refused, and so is 16384 x 2730 4:4:4, whose
    // 42.7 Mi pixels fit but whose planes pad to 16384 x 2736 x 3 =
    // 134,479,872 samples, just past the 128 Mi (134,217,728) budget that
    // the unpadded 134,184,960 would have met; the Z 8's 8256 x 5504 4:2:2
    // preview (86.7 Mi)
    // is accepted, and a 9000 x 9000 4:2:0 frame within the plane budget
    // but past 64 Mi pixels is refused ahead of the decode.
    assert_eq!(MAX_PREVIEW_SAMPLES, 128 << 20);
    // (Header-only streams: a small frame given large axes, so the test
    // itself allocates no blocks for them.)
    for (w, h) in [(8192u16, 8192u16), (16384, 2730)] {
        let mut s = colour(16, 16, (1, 1), 1);
        s.width = w;
        s.height = h;
        s.raw_scan = Some(Vec::new());
        assert_eq!(
            jpeg::header(&s.encode()).unwrap_err(),
            Error::Axis {
                width: usize::from(w),
                height: usize::from(h)
            }
        );
    }
    let mut s = colour(16, 16, (2, 2), 1);
    s.width = 9000;
    s.height = 9000;
    s.raw_scan = Some(Vec::new());
    assert_eq!(
        jpeg::header(&s.encode()).unwrap_err(),
        Error::Axis {
            width: 9000,
            height: 9000
        }
    );
    let mut s = colour(16, 16, (2, 1), 1);
    s.width = 8256;
    s.height = 5504;
    s.raw_scan = Some(Vec::new());
    let head = jpeg::header(&s.encode()).unwrap();
    assert_eq!(
        (head.width, head.height, head.sampling),
        (8256, 5504, (2, 1))
    );

    // Tables: missing, over-full, a quantiser not declared, a zero entry.
    let mut s = good.clone();
    s.tables = false;
    assert_eq!(
        jpeg::decode(&s.encode(), Scale::Full).unwrap_err(),
        Error::Table
    );
    let mut s = good.clone();
    s.bad_dht = true;
    assert_eq!(
        jpeg::decode(&s.encode(), Scale::Full).unwrap_err(),
        Error::Table
    );
    let mut s = good.clone();
    s.comps[0].tq = 1;
    assert_eq!(
        jpeg::decode(&s.encode(), Scale::Full).unwrap_err(),
        Error::Table
    );
    let mut s = good.clone();
    s.quant[0][10] = 0;
    assert_eq!(
        jpeg::decode(&s.encode(), Scale::Full).unwrap_err(),
        Error::Table
    );

    // A scan that is not the whole frame.
    let mut s = colour(16, 16, (1, 1), 32);
    s.scan_count = Some(1);
    assert_eq!(
        jpeg::decode(&s.encode(), Scale::Full).unwrap_err(),
        Error::Scan
    );

    // Cut inside the scan: the decoder runs to the end on zeros and then
    // says so, at every cut.
    let scan_start = file.len() - 2 - good.scan(&dc_table(), &ac_table()).len();
    for cut in [scan_start + 1, scan_start + 7, file.len() - 3] {
        let short = &file[..cut];
        assert_eq!(
            jpeg::decode(short, Scale::Full).unwrap_err(),
            Error::Truncated,
            "cut at {cut}"
        );
    }
    // Ended by EOI early: the same.
    let mut early = file[..scan_start + 5].to_vec();
    early.extend_from_slice(&[0xFF, 0xD9]);
    assert_eq!(
        jpeg::decode(&early, Scale::Full).unwrap_err(),
        Error::Truncated
    );

    // A restart marker that is not one.
    let mut s = good.clone();
    s.restart = 1;
    let mut file = s.encode();
    let rst = file
        .windows(2)
        .position(|w| w[0] == 0xFF && w[1] == 0xD0)
        .unwrap();
    file[rst + 1] = 0xD9;
    assert_eq!(
        jpeg::decode(&file, Scale::Full).unwrap_err(),
        Error::Restart
    );
    let mut s = good.clone();
    s.restart = 1;
    let mut file = s.encode();
    // Remove the first restart marker entirely.
    file.drain(rst..rst + 2);
    assert_eq!(
        jpeg::decode(&file, Scale::Full).unwrap_err(),
        Error::Restart
    );

    // Bits that begin no code of the DC table (its codes are 0000..1011).
    let mut s = good.clone();
    s.raw_scan = Some(vec![0xFF, 0x00, 0xFF, 0x00, 0xFF, 0x00]);
    assert_eq!(
        jpeg::decode(&s.encode(), Scale::Full).unwrap_err(),
        Error::Huffman
    );

    // A run past the block: DC 0, three ZRLs, then run 15 size 1.
    let ac = ac_table();
    let dc = dc_table();
    let mut w = BitWriter::default();
    let (code, len) = dc.codes[0].unwrap();
    w.put(code, len);
    for _ in 0..3 {
        let (code, len) = ac.codes[0xF0].unwrap();
        w.put(code, len);
    }
    let (code, len) = ac.codes[0xF1].unwrap();
    w.put(code, len);
    w.put(1, 1);
    w.flush();
    let mut s = grey(8, 8, 33);
    s.raw_scan = Some(w.out);
    assert_eq!(
        jpeg::decode(&s.encode(), Scale::Full).unwrap_err(),
        Error::Run
    );

    // A ZRL that does not fit the block: DC 0, then four ZRLs.
    let mut w = BitWriter::default();
    let (code, len) = dc.codes[0].unwrap();
    w.put(code, len);
    for _ in 0..4 {
        let (code, len) = ac.codes[0xF0].unwrap();
        w.put(code, len);
    }
    w.flush();
    let mut s = grey(8, 8, 33);
    s.raw_scan = Some(w.out);
    assert_eq!(
        jpeg::decode(&s.encode(), Scale::Full).unwrap_err(),
        Error::Run
    );

    // An AC symbol with no magnitude that is neither EOB nor ZRL.
    let mut counts = [0u8; 16];
    counts[3] = 4;
    let reserved = table(counts, vec![0x00, 0x10, 0xF0, 0x01]);
    let mut w = BitWriter::default();
    let (code, len) = dc.codes[0].unwrap();
    w.put(code, len);
    let (code, len) = reserved.codes[0x10].unwrap();
    w.put(code, len);
    w.flush();
    let mut s = grey(8, 8, 33);
    s.ac = Some(reserved);
    s.raw_scan = Some(w.out);
    assert_eq!(
        jpeg::decode(&s.encode(), Scale::Full).unwrap_err(),
        Error::Huffman
    );

    // A DC predictor past 16 bits: seventeen blocks of +2047; sixteen fit.
    for (blocks, ok) in [(16u16, true), (17, false)] {
        let mut w = BitWriter::default();
        for _ in 0..blocks {
            let (code, len) = dc.codes[11].unwrap();
            w.put(code, len);
            w.put(0x7FF, 11);
            let (code, len) = ac.codes[0x00].unwrap();
            w.put(code, len);
        }
        w.flush();
        let mut s = grey(8, 8 * blocks, 33);
        s.raw_scan = Some(w.out);
        let decoded = jpeg::decode(&s.encode(), Scale::Full);
        assert_eq!(decoded.is_ok(), ok, "{blocks} blocks");
        if !ok {
            assert_eq!(decoded.unwrap_err(), Error::Huffman);
        }
    }

    // A restart marker out of sequence, and an unread byte before one.
    let mut s = good.clone();
    s.restart = 1;
    let file = s.encode();
    let rst = file
        .windows(2)
        .position(|w| w[0] == 0xFF && w[1] == 0xD0)
        .unwrap();
    let mut wrong = file.clone();
    wrong[rst + 1] = 0xD1;
    assert_eq!(
        jpeg::decode(&wrong, Scale::Full).unwrap_err(),
        Error::Restart
    );
    let mut extra = file.clone();
    extra.insert(rst, 0x12);
    assert_eq!(
        jpeg::decode(&extra, Scale::Full).unwrap_err(),
        Error::Restart
    );

    // After the last MCU: EOI, and nothing but padding before it.
    let file = good.encode();
    let eoi = file.len() - 2;
    let mut cut = file.clone();
    cut.truncate(eoi);
    assert_eq!(
        jpeg::decode(&cut, Scale::Full).unwrap_err(),
        Error::Truncated
    );
    let mut extra = file.clone();
    extra.insert(eoi, 0x12);
    assert_eq!(jpeg::decode(&extra, Scale::Full).unwrap_err(), Error::Scan);
    let mut second = file.clone();
    second.truncate(eoi);
    second.extend_from_slice(&segment(0xDA, &[1, 1, 0, 0, 63, 0]));
    second.extend_from_slice(&[0xFF, 0xD9]);
    assert_eq!(jpeg::decode(&second, Scale::Full).unwrap_err(), Error::Scan);
    // Fill bytes before EOI are allowed, and what follows EOI is not read.
    let mut filled = file.clone();
    filled.insert(eoi, 0xFF);
    filled.extend_from_slice(b"trailing bytes");
    assert_eq!(
        jpeg::decode(&filled, Scale::Full).unwrap().data,
        jpeg::decode(&file, Scale::Full).unwrap().data
    );

    // A table whose last code is all ones (two one-bit codes).
    let mut counts = [0u8; 16];
    counts[0] = 2;
    let mut s = good.clone();
    s.dc = Some(table(counts, vec![0, 1]));
    s.raw_scan = Some(Vec::new());
    assert_eq!(
        jpeg::decode(&s.encode(), Scale::Full).unwrap_err(),
        Error::Table
    );

    // Arithmetic conditioning and the hierarchical markers, a second
    // frame, and more table definitions than the budget.
    for marker in [0xCC, 0xDE, 0xDF] {
        let mut s = good.clone();
        s.prefix = segment(marker, &[0, 0, 0, 0]);
        assert_eq!(
            jpeg::header(&s.encode()).unwrap_err(),
            Error::Unsupported(marker)
        );
    }
    let mut s = good.clone();
    s.prefix = segment(0xC0, &[8, 0, 16, 0, 16, 1, 1, 0x11, 0]);
    assert_eq!(
        jpeg::decode(&s.encode(), Scale::Full).unwrap_err(),
        Error::Segment
    );
    assert_eq!(jpeg::MAX_TABLE_DEFINITIONS, 32);
    let room = jpeg::MAX_TABLE_DEFINITIONS - good.quant.len() - 2;
    for (copies, ok) in [(room, true), (room + 1, false)] {
        let mut s = good.clone();
        s.prefix = segment(0xC4, &dht_payload(0, 1, &dc)).repeat(copies);
        let decoded = jpeg::decode(&s.encode(), Scale::Full);
        assert_eq!(decoded.is_ok(), ok, "{copies} extra tables");
        if !ok {
            assert_eq!(decoded.unwrap_err(), Error::Table);
        }
    }

    // A scan that lists the frame's components out of order.
    let c = colour(16, 16, (1, 1), 31);
    let mut file = c.encode();
    let sos = file
        .windows(2)
        .position(|w| w[0] == 0xFF && w[1] == 0xDA)
        .unwrap();
    file.swap(sos + 5, sos + 7);
    assert_eq!(jpeg::decode(&file, Scale::Full).unwrap_err(), Error::Scan);

    // Accepted as baseline: SOF1 at 8 bits, and a lone component whose
    // sampling factors are not 1x1 (one block per MCU all the same).
    let plain = jpeg::decode(&good.encode(), Scale::Full).unwrap();
    let mut s = good.clone();
    s.sof = 0xC1;
    assert_eq!(
        jpeg::decode(&s.encode(), Scale::Full).unwrap().data,
        plain.data
    );
    let mut s = good.clone();
    s.comps[0].h = 2;
    s.comps[0].v = 2;
    assert_eq!(
        jpeg::decode(&s.encode(), Scale::Full).unwrap().data,
        plain.data
    );

    // Errors name themselves.
    for e in [
        Error::NotJpeg,
        Error::Truncated,
        Error::Unsupported(0xC2),
        Error::Precision(12),
        Error::Components(2),
        Error::Sampling,
        Error::Table,
        Error::Huffman,
        Error::Segment,
        Error::Axis {
            width: 0,
            height: 1,
        },
        Error::Scan,
        Error::Restart,
        Error::Run,
    ] {
        assert!(e.to_string().contains("JPEG"), "{e}");
    }
}

// ------------------------------------------------------------ thumbnails

#[test]
fn the_thumbnail_rule_picks_the_scale_and_resamples_to_the_edge() {
    let data = thumb();
    // An edge the full frame does not exceed decodes at full scale, as is.
    let full = jpeg::thumbnail(&data, 400, 1).unwrap();
    assert_eq!((full.width, full.height), (160, 120));
    assert_eq!(full.data, jpeg::decode(&data, Scale::Full).unwrap().data);
    assert_eq!(fnv1a64(&full.data), 0x15cf1df123ee575d);
    // An edge that a coarser scale meets exactly is that scale, untouched.
    let quarter = jpeg::thumbnail(&data, 40, 1).unwrap();
    assert_eq!((quarter.width, quarter.height), (40, 30));
    assert_eq!(
        quarter.data,
        jpeg::decode(&data, Scale::Quarter).unwrap().data
    );
    assert_eq!(fnv1a64(&quarter.data), 0x7c9a6f6b601504ab);
    // An edge between scales decodes at the coarsest covering scale and is
    // area-resampled down to exactly the edge, the same on any thread count.
    let fifty = jpeg::thumbnail(&data, 50, 1).unwrap();
    assert_eq!((fifty.width, fifty.height), (50, 38));
    assert_eq!(jpeg::thumbnail(&data, 50, 4).unwrap().data, fifty.data);
    let small = jpeg::thumbnail(&data, 16, 1).unwrap();
    assert_eq!((small.width, small.height), (16, 12));
    // The rule is decode-then-resample, so the half decode is the source.
    let half = jpeg::decode(&data, Scale::Half).unwrap();
    let mean = |img: &Rgb8| {
        img.data.iter().map(|v| u64::from(*v)).sum::<u64>() as f64 / img.data.len() as f64
    };
    assert!((mean(&half) - mean(&fifty)).abs() < 2.0);
    // A refusal from the stream is the stream's.
    assert_eq!(jpeg::thumbnail(b"P6", 40, 1).unwrap_err(), Error::NotJpeg);
}

#[test]
fn ppm_round_trips_through_read_and_write_only_in_the_written_shape() {
    use td_photo::image::{read_ppm, write_ppm};
    let image = jpeg::decode(&thumb(), Scale::Eighth).unwrap();
    let mut bytes = Vec::new();
    write_ppm(&image, &mut bytes).unwrap();
    assert!(bytes.starts_with(b"P6\n20 15\n255\n"));
    let back = read_ppm(&bytes).unwrap();
    assert_eq!((back.width, back.height), (20, 15));
    assert_eq!(back.data, image.data);
    // Anything else is a miss, never a panic or a partial image.
    assert!(read_ppm(b"").is_none());
    assert!(read_ppm(b"P6").is_none());
    assert!(read_ppm(b"P5\n20 15\n255\n").is_none());
    assert!(read_ppm(b"P6\n20 15\n255\n").is_none(), "short payload");
    let mut long = bytes.clone();
    long.push(0);
    assert!(read_ppm(&long).is_none(), "long payload");
    let mut short = bytes.clone();
    short.pop();
    assert!(read_ppm(&short).is_none());
    assert!(read_ppm(b"P6\n20  15\n255\n").is_none(), "double space");
    assert!(read_ppm(b"P6\n# c\n20 15\n255\n").is_none(), "comment");
    assert!(read_ppm(b"P6\n20 15\n65535\n").is_none(), "wide samples");
    assert!(read_ppm(b"P6\n0 15\n255\n").is_none(), "zero axis");
    assert!(read_ppm(b"P6\n-20 15\n255\n").is_none());
    assert!(read_ppm(b"P6\n20 15 3\n255\n").is_none());
    // Sizes past the ceilings are refused before any allocation.
    assert!(read_ppm(b"P6\n16385 1\n255\n").is_none());
    assert!(read_ppm(b"P6\n16384 16384\n255\n").is_none());
    assert!(read_ppm(b"P6\n99999999999999999999 1\n255\n").is_none());
}

// ------------------------------------------------------------------ encoder

fn ramp(width: usize, height: usize) -> Rgb8 {
    let mut image = Rgb8::new(width, height).unwrap();
    for y in 0..height {
        for x in 0..width {
            let i = (y * width + x) * 3;
            image.data[i] = (x * 255 / width.max(2).saturating_sub(1).max(1)).min(255) as u8;
            image.data[i + 1] = (y * 255 / height.max(2).saturating_sub(1).max(1)).min(255) as u8;
            image.data[i + 2] = ((x + y) * 127 / (width + height)) as u8;
        }
    }
    image
}

fn noise(width: usize, height: usize, seed: u64) -> Rgb8 {
    let mut image = Rgb8::new(width, height).unwrap();
    let mut s = seed;
    for v in image.data.iter_mut() {
        *v = (lcg(&mut s) >> 8) as u8;
    }
    image
}

fn encode(image: &Rgb8, quality: u8, threads: usize) -> Vec<u8> {
    let mut encoder = jpeg::Encoder::new(image.width, image.height, quality, threads).unwrap();
    encoder.encode_rows(&image.data).unwrap();
    encoder.finish().unwrap()
}

#[test]
fn the_encoder_round_trips_through_the_decoder() {
    for (w, h) in [
        (1usize, 1usize),
        (8, 8),
        (9, 17),
        (37, 29),
        (64, 16),
        (100, 3),
    ] {
        let image = ramp(w, h);
        let bytes = encode(&image, jpeg::QUALITY, 3);
        let head = jpeg::header(&bytes).unwrap();
        assert_eq!((head.width, head.height), (w, h));
        let decoded = jpeg::decode(&bytes, Scale::Full).unwrap();
        assert_close(&decoded, &image, 12, &format!("ramp {w}x{h}"));
        // At the top quality the tables are all ones and the round trip is
        // within the colour transform's rounding, noise included.
        let image = noise(w, h, 0x5eed + w as u64);
        let bytes = encode(&image, 100, 2);
        let decoded = jpeg::decode(&bytes, Scale::Full).unwrap();
        assert_close(&decoded, &image, 4, &format!("noise {w}x{h}"));
    }
    // It compresses: a smooth frame at the export quality is a small
    // fraction of its raw bytes.
    let image = ramp(256, 256);
    let bytes = encode(&image, jpeg::QUALITY, 4);
    assert!(bytes.len() * 8 < image.data.len(), "{} bytes", bytes.len());
    assert!(bytes.starts_with(&[0xff, 0xd8, 0xff, 0xe0]) && bytes.ends_with(&[0xff, 0xd9]));
    // A grey frame stays exactly grey: its chroma is the level and codes
    // to nothing.
    let mut grey = ramp(24, 24);
    for px in grey.data.as_chunks_mut::<3>().0 {
        *px = [px[0]; 3];
    }
    let decoded = jpeg::decode(&encode(&grey, jpeg::QUALITY, 1), Scale::Full).unwrap();
    assert!(decoded
        .data
        .as_chunks::<3>()
        .0
        .iter()
        .all(|p| p[0] == p[1] && p[1] == p[2]));
    assert_close(&decoded, &grey, 4, "grey");
    // A black frame at quality 100: every luma DC is -1024 at a quantiser
    // of 1, the largest baseline DC category (11) through the coder and
    // the decoder both, and the frame decodes to exactly black. (The
    // clamp itself is pinned by `forward`'s unit test; the transform lands
    // on -1024 here without it.)
    let black = Rgb8::new(16, 16).unwrap();
    let decoded = jpeg::decode(&encode(&black, 100, 1), Scale::Full).unwrap();
    assert_eq!(decoded, black);
}

#[test]
fn the_encoder_streams_bands_identically_on_any_thread_count() {
    let image = ramp(50, 45);
    let whole = encode(&image, jpeg::QUALITY, 1);
    for (rows, threads) in [(1usize, 1usize), (7, 4), (8, 2), (64, 16), (45, 3)] {
        let mut encoder = jpeg::Encoder::new(50, 45, jpeg::QUALITY, threads).unwrap();
        let mut bytes = encoder.take();
        assert!(bytes.starts_with(&[0xff, 0xd8]));
        for band in image.data.chunks(rows * 50 * 3) {
            encoder.encode_rows(band).unwrap();
            bytes.extend(encoder.take());
        }
        bytes.extend(encoder.finish().unwrap());
        assert_eq!(bytes, whole, "rows {rows} threads {threads}");
    }
}

#[test]
fn the_encoder_refuses_bad_axes_and_rows() {
    let axis = |w: usize, h: usize| jpeg::Encoder::new(w, h, 92, 1).err().unwrap();
    assert!(matches!(axis(0, 8), jpeg::Error::Axis { .. }));
    assert!(matches!(axis(8, 0), jpeg::Error::Axis { .. }));
    assert!(matches!(axis(MAX_AXIS + 1, 8), jpeg::Error::Axis { .. }));
    assert!(matches!(axis(8, MAX_AXIS + 1), jpeg::Error::Axis { .. }));
    assert!(jpeg::Encoder::new(MAX_AXIS, 1, 92, 1).is_ok());
    let mut encoder = jpeg::Encoder::new(8, 4, 92, 1).unwrap();
    assert_eq!(
        encoder.encode_rows(&[0; 23]).unwrap_err(),
        jpeg::Error::Rows
    );
    assert_eq!(
        encoder.encode_rows(&[0; 8 * 3 * 5]).unwrap_err(),
        jpeg::Error::Rows
    );
    encoder.encode_rows(&[0; 8 * 3 * 3]).unwrap();
    assert_eq!(
        encoder.encode_rows(&[0; 8 * 3 * 2]).unwrap_err(),
        jpeg::Error::Rows
    );
    // Short of the height, it will not finish.
    assert_eq!(encoder.finish().unwrap_err(), jpeg::Error::Rows);
    let mut encoder = jpeg::Encoder::new(8, 4, 92, 1).unwrap();
    encoder.encode_rows(&[0; 8 * 3 * 4]).unwrap();
    assert!(encoder.finish().is_ok());
    assert_eq!(jpeg::QUALITY, 92);
}

#[test]
fn the_encoder_writes_the_standard_headers() {
    // The segments before the scan, by their literal bytes: what a decoder
    // that is not this crate's reads first.
    let bytes = encode(&ramp(9, 5), jpeg::QUALITY, 1);
    let mut at = 2;
    let mut segments = Vec::new();
    while at < bytes.len() {
        assert_eq!(bytes[at], 0xff);
        let marker = bytes[at + 1];
        let len = usize::from(u16::from_be_bytes([bytes[at + 2], bytes[at + 3]]));
        segments.push((marker, bytes[at + 4..at + 2 + len].to_vec()));
        at += 2 + len;
        if marker == 0xda {
            break;
        }
    }
    let markers: Vec<u8> = segments.iter().map(|(m, _)| *m).collect();
    assert_eq!(markers, [0xe0, 0xdb, 0xc0, 0xc4, 0xda]);
    assert_eq!(segments[0].1, b"JFIF\0\x01\x01\x00\x00\x01\x00\x01\x00\x00");
    // Two 8-bit quantisers in zigzag order: the luma's first entries are
    // Annex K's 16, 11, 12, 14, 12, 10, 16, 14 at 92 (scale 16), and the
    // chroma's last is 99 at 16.
    let dqt = &segments[1].1;
    assert_eq!(dqt.len(), 130);
    assert_eq!(dqt[0], 0);
    assert_eq!(&dqt[1..9], &[3, 2, 2, 2, 2, 2, 3, 2]);
    assert_eq!(dqt[65], 1);
    assert_eq!(dqt[66], 3);
    assert_eq!(dqt[129], 16);
    assert_eq!(
        segments[2].1,
        [8, 0, 5, 0, 9, 3, 1, 0x11, 0, 2, 0x11, 1, 3, 0x11, 1]
    );
    // Four Huffman tables with Annex K.3's counts.
    let dht = &segments[3].1;
    assert_eq!(dht.len(), 4 * 17 + 2 * 12 + 2 * 162);
    let mut at = 0;
    for (class_id, bits, values) in [
        (
            0x00u8,
            [0u8, 1, 5, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0],
            12usize,
        ),
        (
            0x10,
            [0, 2, 1, 3, 3, 2, 4, 3, 5, 5, 4, 4, 0, 0, 1, 0x7d],
            162,
        ),
        (0x01, [0, 3, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0], 12),
        (
            0x11,
            [0, 2, 1, 2, 4, 4, 3, 4, 7, 5, 4, 4, 0, 1, 2, 0x77],
            162,
        ),
    ] {
        assert_eq!(dht[at], class_id);
        assert_eq!(&dht[at + 1..at + 17], &bits);
        at += 17 + values;
    }
    assert_eq!(at, dht.len());
    assert_eq!(segments[4].1, [3, 1, 0x00, 2, 0x11, 3, 0x11, 0, 63, 0]);
}

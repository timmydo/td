//! The AEAD corpora again, for unit tests that name a backend: `tests/vectors.rs`
//! goes through the public API, which reaches only the one the CPU selects.

#![allow(clippy::panic, clippy::unwrap_used)]

pub(crate) fn hex(field: &str) -> Vec<u8> {
    if field == "-" {
        return Vec::new();
    }
    field
        .as_bytes()
        .chunks(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}

/// `tests/digest_vectors.py`'s generator.
pub(crate) fn expand(seed: u64, length: usize) -> Vec<u8> {
    let mut state = seed;
    (0..length)
        .map(|_| {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (state >> 56) as u8
        })
        .collect()
}

pub(crate) struct Row {
    pub key: Vec<u8>,
    pub nonce: [u8; 12],
    pub aad: Vec<u8>,
    pub plain: Vec<u8>,
    pub sealed: Vec<u8>,
    pub tag: [u8; 16],
    pub valid: bool,
}

/// A Wycheproof file as `tests/wycheproof/` holds it.
pub(crate) fn rows(text: &str) -> Vec<Row> {
    text.lines()
        .map(|line| {
            let fields: Vec<&str> = line.split(' ').collect();
            let [key, nonce, aad, plain, sealed, tag, verdict] = fields[..] else {
                panic!("malformed row: {line}");
            };
            Row {
                key: hex(key),
                nonce: hex(nonce).try_into().unwrap(),
                aad: hex(aad),
                plain: hex(plain),
                sealed: hex(sealed),
                tag: hex(tag).try_into().unwrap(),
                valid: verdict == "valid",
            }
        })
        .collect()
}

pub(crate) struct LongRow {
    pub key: Vec<u8>,
    pub nonce: [u8; 12],
    pub aad: Vec<u8>,
    pub plain: Vec<u8>,
    pub tag: [u8; 16],
    pub digest: Vec<u8>,
}

/// The rows of `tests/aead_long.txt` for one cipher.
pub(crate) fn long_rows(cipher: &str) -> Vec<LongRow> {
    include_str!("../tests/aead_long.txt")
        .lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split(' ').collect();
            let [name, seed, aad_length, length, tag, digest] = fields[..] else {
                panic!("malformed row: {line}");
            };
            if name != cipher {
                return None;
            }
            let seed: u64 = seed.parse().unwrap();
            let key_len = if name == "aes128" { 16 } else { 32 };
            Some(LongRow {
                key: expand(seed, key_len),
                nonce: expand(seed + 1, 12).try_into().unwrap(),
                aad: expand(seed + 2, aad_length.parse().unwrap()),
                plain: expand(seed + 3, length.parse().unwrap()),
                tag: hex(tag).try_into().unwrap(),
                digest: hex(digest),
            })
        })
        .collect()
}

/// Lengths around each batch and block size either backend works in.
pub(crate) const EDGE_LENGTHS: &[usize] = &[
    0, 1, 15, 16, 17, 31, 32, 33, 63, 64, 65, 113, 127, 128, 129, 143, 144, 145, 191, 192, 193,
    255, 256, 257, 383, 384, 385, 511, 512, 513, 575, 576, 577, 1023, 1024, 1025, 16383, 16384,
];

// `detect` decides which backend every other test reaches, so one that wrongly says
// no would leave a backend untested with nothing to show for it. It lives here so
// that `accel.rs` is all production code, which the confinement test holds whole.
// A sandbox without /proc has nothing to compare; a kernel booted with `clearcpuid=`
// hides a flag CPUID still reports, and this reds on that deliberate mismatch.
#[cfg(target_arch = "x86_64")]
#[test]
fn detection_agrees_with_the_kernel() {
    use crate::accel::{AesClmul, Avx2, ShaNi};

    let Ok(text) = std::fs::read_to_string("/proc/cpuinfo") else {
        return;
    };
    let Some(flags) = text.lines().find(|line| line.starts_with("flags")) else {
        return;
    };
    let all = |wanted: &[&str]| {
        wanted
            .iter()
            .all(|flag| flags.split_whitespace().any(|word| word == *flag))
    };
    assert_eq!(
        AesClmul::detect().is_some(),
        all(&["aes", "pclmulqdq", "ssse3", "sse4_1"])
    );
    assert_eq!(Avx2::detect().is_some(), all(&["avx2"]));
    assert_eq!(
        ShaNi::detect().is_some(),
        all(&["sha_ni", "ssse3", "sse4_1"])
    );
}

/// Each hardware backend and whether this machine has it. Every test that names a
/// backend reaches it through the same `detect`, so one absent here ran nowhere.
#[cfg(target_arch = "x86_64")]
fn hardware_backends() -> Vec<(&'static str, bool)> {
    use crate::accel::{AesClmul, Avx2, ShaNi};
    vec![
        (
            "AES-NI and PCLMULQDQ (AES-GCM)",
            AesClmul::detect().is_some(),
        ),
        ("AVX2 (ChaCha20)", Avx2::detect().is_some()),
        ("SHA-NI (SHA-256)", ShaNi::detect().is_some()),
    ]
}

#[cfg(not(target_arch = "x86_64"))]
fn hardware_backends() -> Vec<(&'static str, bool)> {
    vec![
        ("AES-NI and PCLMULQDQ (AES-GCM)", false),
        ("AVX2 (ChaCha20)", false),
        ("SHA-NI (SHA-256)", false),
    ]
}

/// Says which hardware backends this run's tests exercised; with
/// TD_PRIMITIVES_REQUIRE_ACCEL=1, fails unless all of them were. A run on a CPU
/// without one passes on the software path alone, and only this says so.
#[test]
fn hardware_backends_exercised() {
    let backends = hardware_backends();
    for (name, present) in &backends {
        let state = if *present { "exercised" } else { "absent" };
        eprintln!("td-primitives backend {name}: {state}");
    }
    if std::env::var("TD_PRIMITIVES_REQUIRE_ACCEL").as_deref() == Ok("1") {
        let absent: Vec<&str> = backends
            .iter()
            .filter(|(_, present)| !present)
            .map(|(name, _)| *name)
            .collect();
        assert!(absent.is_empty(), "required but absent: {absent:?}");
    }
}

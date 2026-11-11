//! The on-demand timing-leakage run over this crate's secret paths
//! (DESIGN.md, "Timing measurement"). Ignored, so no gate runs it:
//! `td-builder gate-crates crypto-portable-leakage --artifact DIRECTORY`.

#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use crate::aes_gcm::{self, AesGcm};
use crate::chacha20_poly1305 as chacha;
use crate::curve25519::x25519;
use crate::ec::{p256, p384};
use crate::leakage::{conclude, controls, measure, samples, Outcome, Rng};
use crate::sha::Sha256;
use crate::{ct, hmac};
use std::hint::black_box;

const MESSAGE: &[u8] = b"td-primitives timing-leakage measurement message";
const NONCE: [u8; 12] = [7; 12];
const AAD: &[u8] = b"aad";
const PLAIN: [u8; 64] = [0x5a; 64];

/// The fixed class's value, or a random one `valid` takes.
fn secret<const N: usize>(fixed: bool, rng: &mut Rng, valid: impl Fn(&[u8; N]) -> bool) -> [u8; N] {
    if fixed {
        return [0x42; N];
    }
    loop {
        let candidate = rng.bytes::<N>();
        if valid(&candidate) {
            return candidate;
        }
    }
}

fn key<const N: usize>(fixed: bool, rng: &mut Rng) -> [u8; N] {
    secret(fixed, rng, |_| true)
}

/// AES-GCM on one backend: a key per input, expanded inside the timed call.
fn aes(
    name_seal: &'static str,
    name_open: &'static str,
    build: fn(&[u8]) -> Option<AesGcm>,
    samples: usize,
) -> Vec<Outcome> {
    if build(&[0; 16]).is_none() {
        println!("leakage {name_seal}, {name_open}: this CPU lacks the instructions; not measured");
        return Vec::new();
    }
    let seal = measure(name_seal, samples, 1, 11, key::<16>, |key| {
        let mut data = PLAIN;
        black_box(build(key).unwrap().seal(&NONCE, AAD, &mut data).unwrap());
    });
    let open = measure(
        name_open,
        samples,
        1,
        12,
        |fixed, rng| {
            let key = key::<16>(fixed, rng);
            let mut data = PLAIN;
            let tag = build(&key).unwrap().seal(&NONCE, AAD, &mut data).unwrap();
            (key, data, tag)
        },
        |(key, sealed, tag)| {
            let mut data = *sealed;
            build(key)
                .unwrap()
                .open(&NONCE, AAD, &mut data, tag)
                .unwrap();
            black_box(&data);
        },
    );
    vec![seal, open]
}

#[test]
#[ignore = "on demand: timing is noisy and machine-specific (DESIGN.md)"]
fn timing_leakage() {
    let samples = samples().unwrap();
    let controls = controls(samples);
    let mut paths = Vec::new();
    paths.push(measure(
        "ct::eq, 32 bytes, equal against first byte differing",
        samples,
        8,
        2,
        |fixed, rng| {
            let a = rng.bytes::<32>();
            let mut b = a;
            if !fixed {
                b[0] ^= 1;
            }
            (a, b)
        },
        |(a, b)| {
            black_box(ct::eq(black_box(a), black_box(b)));
        },
    ));
    let poly_key = [9u8; chacha::KEY_LEN];
    let mut sealed = PLAIN;
    let poly_tag = chacha::seal(&poly_key, &NONCE, AAD, &mut sealed).unwrap();
    paths.push(measure(
        "Poly1305 tag check, wrong first byte against wrong last byte",
        samples,
        1,
        3,
        |fixed, _| {
            let mut tag = poly_tag;
            tag[if fixed { 0 } else { chacha::TAG_LEN - 1 }] ^= 1;
            tag
        },
        |tag| {
            let mut data = sealed;
            black_box(chacha::open(&poly_key, &NONCE, AAD, &mut data, tag).is_err());
        },
    ));
    paths.push(measure(
        "ChaCha20-Poly1305 seal, fixed against random key",
        samples,
        1,
        4,
        key::<32>,
        |key| {
            let mut data = PLAIN;
            black_box(chacha::seal(key, &NONCE, AAD, &mut data).unwrap());
        },
    ));
    paths.push(measure(
        "ChaCha20-Poly1305 open, fixed against random key",
        samples,
        1,
        5,
        |fixed, rng| {
            let key = key::<32>(fixed, rng);
            let mut data = PLAIN;
            let tag = chacha::seal(&key, &NONCE, AAD, &mut data).unwrap();
            (key, data, tag)
        },
        |(key, sealed, tag)| {
            let mut data = *sealed;
            chacha::open(key, &NONCE, AAD, &mut data, tag).unwrap();
            black_box(&data);
        },
    ));
    paths.extend(aes(
        "AES-128-GCM seal, AES-NI, fixed against random key",
        "AES-128-GCM open, AES-NI, fixed against random key",
        aes_gcm::tests::hardware,
        samples,
    ));
    paths.extend(aes(
        "AES-128-GCM seal, bitsliced, fixed against random key",
        "AES-128-GCM open, bitsliced, fixed against random key",
        |key| Some(aes_gcm::tests::software(key)),
        samples,
    ));
    paths.push(measure(
        "HMAC-SHA256, fixed against random 32-byte key",
        samples,
        1,
        6,
        key::<32>,
        |key| {
            black_box(hmac::mac::<Sha256>(key, &[MESSAGE]));
        },
    ));
    let p256_peer = p256::SecretKey::from_bytes(&[0x24; 32])
        .unwrap()
        .public_key()
        .unwrap();
    let p256_valid = |bytes: &[u8; 32]| p256::SecretKey::from_bytes(bytes).is_ok();
    paths.push(measure(
        "P-256 ECDSA sign, fixed against random secret scalar",
        samples,
        1,
        7,
        |fixed, rng| secret(fixed, rng, p256_valid),
        |scalar| {
            let key = p256::SecretKey::from_bytes(scalar).unwrap();
            black_box(key.sign_sha256(MESSAGE).unwrap());
        },
    ));
    paths.push(measure(
        "P-256 ECDH, fixed against random secret scalar",
        samples,
        1,
        8,
        |fixed, rng| secret(fixed, rng, p256_valid),
        |scalar| {
            let key = p256::SecretKey::from_bytes(scalar).unwrap();
            black_box(key.agree(&p256_peer).unwrap());
        },
    ));
    let p384_peer = p384::SecretKey::from_bytes(&[0x24; 48])
        .unwrap()
        .public_key()
        .unwrap();
    paths.push(measure(
        "P-384 ECDH, fixed against random secret scalar",
        samples,
        1,
        9,
        |fixed, rng| {
            secret(fixed, rng, |bytes: &[u8; 48]| {
                p384::SecretKey::from_bytes(bytes).is_ok()
            })
        },
        |scalar| {
            let key = p384::SecretKey::from_bytes(scalar).unwrap();
            black_box(key.agree(&p384_peer).unwrap());
        },
    ));
    let x25519_peer = x25519::public_key(&[0x24; 32]);
    paths.push(measure(
        "X25519, fixed against random secret",
        samples,
        1,
        10,
        key::<32>,
        |secret| {
            black_box(x25519::agree(secret, &x25519_peer).unwrap());
        },
    ));
    conclude(&controls, &paths).unwrap();
}

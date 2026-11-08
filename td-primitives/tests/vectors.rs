//! Known answers from sources independent of this crate: Wycheproof, Python's
//! hashlib and hmac, and naive big-integer references written for these tests
//! (DESIGN.md). Each corpus states its row count, so a truncated or filtered file
//! reds rather than passing on less.

#![allow(clippy::indexing_slicing, clippy::unwrap_used, clippy::panic)]

use std::cell::RefCell;
use td_primitives::aes_gcm::AesGcm;
use td_primitives::chacha20_poly1305;
use td_primitives::curve25519::{ed25519, x25519};
use td_primitives::ec::{p256, p384, p521};
use td_primitives::hmac::mac;
use td_primitives::rsa;
use td_primitives::sha::{sha1, Digest, Sha256, Sha384, Sha512};
use td_primitives::Refused;

fn hex(field: &str) -> Vec<u8> {
    if field == "-" {
        return Vec::new();
    }
    assert!(field.len().is_multiple_of(2), "odd hex: {field}");
    (0..field.len() / 2)
        .map(|i| u8::from_str_radix(&field[i * 2..i * 2 + 2], 16).unwrap())
        .collect()
}

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn expand(seed: u64, length: usize) -> Vec<u8> {
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

fn check_digest<D: Digest>(seed: u64, length: usize, want: &str) {
    assert_eq!(
        to_hex(D::digest(&expand(seed, length)).as_ref()),
        want,
        "seed {seed} length {length}"
    );
}

fn check_mac<D: Digest>(seed: u64, key_length: usize, length: usize, want: &str) {
    let key = expand(seed, key_length);
    let message = expand(seed + 1, length);
    assert_eq!(
        to_hex(mac::<D>(&key, &[&message]).as_ref()),
        want,
        "seed {seed} key {key_length} length {length}"
    );
}

#[test]
fn digests_and_macs_agree_with_python() {
    let text = include_str!("digest_vectors.txt");
    let mut rows = 0;
    for line in text.lines() {
        let fields: Vec<&str> = line.split(' ').collect();
        match fields[..] {
            [algorithm, seed, length, want] => {
                let (seed, length) = (seed.parse().unwrap(), length.parse().unwrap());
                match algorithm {
                    "sha1" => assert_eq!(to_hex(&sha1(&expand(seed, length))), want),
                    "sha256" => check_digest::<Sha256>(seed, length, want),
                    "sha384" => check_digest::<Sha384>(seed, length, want),
                    "sha512" => check_digest::<Sha512>(seed, length, want),
                    other => panic!("unknown digest {other}"),
                }
            }
            [algorithm, seed, key_length, length, want] => {
                let seed = seed.parse().unwrap();
                let key_length = key_length.parse().unwrap();
                let length = length.parse().unwrap();
                match algorithm {
                    "hmac-sha256" => check_mac::<Sha256>(seed, key_length, length, want),
                    "hmac-sha384" => check_mac::<Sha384>(seed, key_length, length, want),
                    "hmac-sha512" => check_mac::<Sha512>(seed, key_length, length, want),
                    other => panic!("unknown mac {other}"),
                }
            }
            _ => panic!("malformed row: {line}"),
        }
        rows += 1;
    }
    assert_eq!(rows, 4 * 263 + 3 * 11 * 7);
}

struct AeadRow {
    key: Vec<u8>,
    nonce: [u8; 12],
    aad: Vec<u8>,
    plain: Vec<u8>,
    sealed: Vec<u8>,
    tag: [u8; 16],
    valid: bool,
}

fn aead_rows(text: &str) -> Vec<AeadRow> {
    text.lines()
        .map(|line| {
            let fields: Vec<&str> = line.split(' ').collect();
            let [key, nonce, aad, plain, sealed, tag, verdict] = fields[..] else {
                panic!("malformed row: {line}");
            };
            assert!(verdict == "valid" || verdict == "invalid", "{verdict}");
            AeadRow {
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

fn check_aead(
    row: &AeadRow,
    seal: impl Fn(&mut [u8]) -> [u8; 16],
    open: impl Fn(&mut [u8], &[u8; 16]) -> bool,
) {
    let mut data = row.sealed.clone();
    let opened = open(&mut data, &row.tag);
    assert_eq!(opened, row.valid, "open: {}", to_hex(&row.sealed));
    if row.valid {
        assert_eq!(data, row.plain);
        let mut data = row.plain.clone();
        let tag = seal(&mut data);
        assert_eq!(data, row.sealed);
        assert_eq!(tag, row.tag);
    } else {
        assert_eq!(data, row.sealed, "a refused input stays ciphertext");
    }
}

#[test]
fn aes_gcm_agrees_with_wycheproof() {
    let rows = aead_rows(include_str!("wycheproof/aes_gcm.txt"));
    assert_eq!(rows.len(), AES_GCM_ROWS);
    let mut invalid = 0;
    for row in &rows {
        let aes = match row.key.len() {
            16 => AesGcm::new_128(&row.key.clone().try_into().unwrap()),
            32 => AesGcm::new_256(&row.key.clone().try_into().unwrap()),
            other => panic!("key of {other} bytes"),
        };
        invalid += usize::from(!row.valid);
        check_aead(
            row,
            |data| aes.seal(&row.nonce, &row.aad, data).unwrap(),
            |data, tag| aes.open(&row.nonce, &row.aad, data, tag).is_ok(),
        );
    }
    assert!(invalid > 0);
}

#[test]
fn chacha20_poly1305_agrees_with_wycheproof() {
    let rows = aead_rows(include_str!("wycheproof/chacha20_poly1305.txt"));
    assert_eq!(rows.len(), CHACHA20_POLY1305_ROWS);
    let mut invalid = 0;
    for row in &rows {
        let key: [u8; 32] = row.key.clone().try_into().unwrap();
        invalid += usize::from(!row.valid);
        check_aead(
            row,
            |data| chacha20_poly1305::seal(&key, &row.nonce, &row.aad, data).unwrap(),
            |data, tag| chacha20_poly1305::open(&key, &row.nonce, &row.aad, data, tag).is_ok(),
        );
    }
    assert!(invalid > 0);
}

const AES_GCM_ROWS: usize = 133;
const CHACHA20_POLY1305_ROWS: usize = 316;

#[test]
fn long_records_agree_with_a_naive_reference() {
    let mut rows = 0;
    for line in include_str!("aead_long.txt").lines() {
        let fields: Vec<&str> = line.split(' ').collect();
        let [name, seed, aad_length, length, tag, digest] = fields[..] else {
            panic!("malformed row: {line}");
        };
        let seed: u64 = seed.parse().unwrap();
        let key_len = if name == "aes128" { 16 } else { 32 };
        let key = expand(seed, key_len);
        let nonce: [u8; 12] = expand(seed + 1, 12).try_into().unwrap();
        let aad = expand(seed + 2, aad_length.parse().unwrap());
        let plain = expand(seed + 3, length.parse().unwrap());
        let mut data = plain.clone();
        let made = match name {
            "aes128" => AesGcm::new_128(&key.clone().try_into().unwrap())
                .seal(&nonce, &aad, &mut data)
                .unwrap(),
            "aes256" => AesGcm::new_256(&key.clone().try_into().unwrap())
                .seal(&nonce, &aad, &mut data)
                .unwrap(),
            "chacha" => {
                chacha20_poly1305::seal(&key.clone().try_into().unwrap(), &nonce, &aad, &mut data)
                    .unwrap()
            }
            other => panic!("unknown cipher {other}"),
        };
        assert_eq!(to_hex(&made), tag, "tag: {line}");
        assert_eq!(to_hex(&Sha256::digest(&data)), digest, "ciphertext: {line}");
        let tag: [u8; 16] = made;
        let opened = match name {
            "aes128" => AesGcm::new_128(&key.clone().try_into().unwrap())
                .open(&nonce, &aad, &mut data, &tag),
            "aes256" => AesGcm::new_256(&key.clone().try_into().unwrap())
                .open(&nonce, &aad, &mut data, &tag),
            _ => chacha20_poly1305::open(
                &key.clone().try_into().unwrap(),
                &nonce,
                &aad,
                &mut data,
                &tag,
            ),
        };
        assert_eq!(opened, Ok(()), "{line}");
        assert!(data == plain, "round trip: {line}");
        rows += 1;
    }
    assert_eq!(rows, 12);
}

/// What a corpus's rows did, so a test can pin its treatment of "acceptable".
#[derive(Default, Debug)]
struct Tally {
    valid: usize,
    invalid: usize,
    acceptable_taken: usize,
    acceptable_refused: usize,
}

impl Tally {
    /// Valid must pass and invalid must be refused; acceptable may go either
    /// way, and the caller pins which way it went.
    fn record(&mut self, verdict: &str, passed: bool, row: &str) {
        match verdict {
            "valid" => {
                assert!(passed, "a valid row was refused: {row}");
                self.valid += 1;
            }
            "invalid" => {
                assert!(!passed, "an invalid row was accepted: {row}");
                self.invalid += 1;
            }
            "acceptable" if passed => self.acceptable_taken += 1,
            "acceptable" => self.acceptable_refused += 1,
            other => panic!("unknown verdict {other}"),
        }
    }

    fn counts(&self) -> [usize; 4] {
        [
            self.valid,
            self.invalid,
            self.acceptable_taken,
            self.acceptable_refused,
        ]
    }
}

/// A corpus of K and T rows; `check` sees the key fields, message and signature.
fn signature_corpus(text: &str, check: impl Fn(&[&str], &[u8], &[u8]) -> bool) -> Tally {
    let mut tally = Tally::default();
    let mut key: Vec<&str> = Vec::new();
    for line in text.lines() {
        let fields: Vec<&str> = line.split(' ').collect();
        match fields[..] {
            ["K", ..] => key = fields[1..].to_vec(),
            ["T", message, signature, verdict] => {
                let passed = check(&key, &hex(message), &hex(signature));
                tally.record(verdict, passed, line);
            }
            _ => panic!("malformed row: {line}"),
        }
    }
    tally
}

#[test]
fn x25519_agrees_with_wycheproof() {
    let mut tally = Tally::default();
    for line in include_str!("wycheproof/x25519.txt").lines() {
        let fields: Vec<&str> = line.split(' ').collect();
        let [private, public, shared, verdict] = fields[..] else {
            panic!("malformed row: {line}");
        };
        let private: [u8; 32] = hex(private).try_into().unwrap();
        let public: [u8; 32] = hex(public).try_into().unwrap();
        let outcome = x25519::agree(&private, &public);
        // A row is taken when it produced exactly the stated secret; a refusal
        // is only ever of an all-zero result.
        if let Err(Refused) = outcome {
            assert_eq!(hex(shared), [0u8; 32], "refused a nonzero secret: {line}");
        }
        tally.record(
            verdict,
            outcome.is_ok_and(|s| s.to_vec() == hex(shared)),
            line,
        );
    }
    assert_eq!(tally.counts(), X25519_COUNTS, "{tally:?}");
}

/// A row's scalar, left-padded: it may carry a sign byte or be short.
fn padded_scalar<const N: usize>(field: &str) -> [u8; N] {
    let bytes = hex(field);
    let significant: Vec<u8> = bytes.iter().copied().skip_while(|b| *b == 0).collect();
    assert!(significant.len() <= N, "{field}");
    let mut padded = [0u8; N];
    padded[N - significant.len()..].copy_from_slice(&significant);
    padded
}

fn ecdh_corpus(text: &str, agree: impl Fn(&str, &[u8]) -> Result<Vec<u8>, Refused>) -> Tally {
    let mut tally = Tally::default();
    for line in text.lines() {
        let fields: Vec<&str> = line.split(' ').collect();
        let [private, public, shared, verdict] = fields[..] else {
            panic!("malformed row: {line}");
        };
        let outcome = agree(private, &hex(public));
        // An invalid row names no secret, so any secret at all is the failure.
        let taken = if verdict == "invalid" {
            outcome.is_ok()
        } else {
            outcome.is_ok_and(|s| s == hex(shared))
        };
        tally.record(verdict, taken, line);
    }
    tally
}

#[test]
fn p256_ecdh_agrees_with_wycheproof() {
    let tally = ecdh_corpus(
        include_str!("wycheproof/ecdh_p256.txt"),
        |private, public| {
            let key = p256::SecretKey::from_bytes(&padded_scalar(private)).unwrap();
            key.agree(public).map(|shared| shared.to_vec())
        },
    );
    assert_eq!(tally.counts(), P256_ECDH_COUNTS, "{tally:?}");
}

#[test]
fn p384_ecdh_agrees_with_wycheproof() {
    let tally = ecdh_corpus(
        include_str!("wycheproof/ecdh_p384.txt"),
        |private, public| {
            let key = p384::SecretKey::from_bytes(&padded_scalar(private)).unwrap();
            key.agree(public).map(|shared| shared.to_vec())
        },
    );
    assert_eq!(tally.counts(), P384_ECDH_COUNTS, "{tally:?}");
}

#[test]
fn ed25519_agrees_with_wycheproof() {
    let tally = signature_corpus(
        include_str!("wycheproof/ed25519.txt"),
        |key, message, signature| {
            let public: [u8; 32] = hex(key[1]).try_into().unwrap();
            let Ok(signature) = <[u8; 64]>::try_from(signature) else {
                return false;
            };
            ed25519::verify(&public, message, &signature).is_ok()
        },
    );
    assert_eq!(tally.counts(), ED25519_COUNTS, "{tally:?}");
}

#[test]
fn ecdsa_agrees_with_wycheproof() {
    let tally = signature_corpus(
        include_str!("wycheproof/ecdsa.txt"),
        |key, message, signature| {
            let public = hex(key[2]);
            let digest: Vec<u8> = match key[1] {
                "sha256" => Sha256::digest(message).to_vec(),
                "sha384" => Sha384::digest(message).to_vec(),
                // Longer than P-256's and P-384's orders: those rows pin truncation.
                "sha512" => Sha512::digest(message).to_vec(),
                other => panic!("unknown hash {other}"),
            };
            match key[0] {
                "p256" => p256::verify(&public, &digest, signature).is_ok(),
                "p384" => p384::verify(&public, &digest, signature).is_ok(),
                "p521" => p521::verify(&public, &digest, signature).is_ok(),
                other => panic!("unknown curve {other}"),
            }
        },
    );
    assert_eq!(tally.counts(), ECDSA_COUNTS, "{tally:?}");
}

fn uncompressed_on(curve: &str, encoded: &[u8]) -> Result<Vec<u8>, Refused> {
    match curve {
        "p256" => p256::uncompressed(encoded).map(|point| point.to_vec()),
        "p384" => p384::uncompressed(encoded).map(|point| point.to_vec()),
        "p521" => p521::uncompressed(encoded).map(|point| point.to_vec()),
        other => panic!("unknown curve {other}"),
    }
}

#[test]
fn points_decode_in_either_form_as_wycheproof_states() {
    let mut counts = std::collections::BTreeMap::new();
    for line in include_str!("wycheproof/ec_points.txt").lines() {
        let fields: Vec<&str> = line.split(' ').collect();
        let [curve, encoded, result, verdict] = fields[..] else {
            panic!("malformed row: {line}");
        };
        let outcome = uncompressed_on(curve, &hex(encoded));
        match verdict {
            "valid" => assert_eq!(outcome, Ok(hex(result)), "{line}"),
            "invalid" => assert_eq!(outcome, Err(Refused), "{line}"),
            other => panic!("unknown verdict {other}"),
        }
        *counts.entry((curve, verdict)).or_insert(0usize) += 1;
    }
    assert_eq!(
        counts.into_iter().collect::<Vec<_>>(),
        EC_POINT_COUNTS,
        "rows per curve and verdict"
    );
}

// The one compressed key each ECDH point file takes, which states no
// uncompressed form, checked by the secret it agrees on instead.
#[test]
fn a_decompressed_ecdh_key_agrees_on_wycheproofs_secret() {
    for (text, curve) in [
        (include_str!("wycheproof/ecdh_p256.txt"), "p256"),
        (include_str!("wycheproof/ecdh_p384.txt"), "p384"),
    ] {
        let mut seen = 0;
        for line in text.lines() {
            let fields: Vec<&str> = line.split(' ').collect();
            let [private, public, shared, "acceptable"] = fields[..] else {
                continue;
            };
            assert!(
                public.starts_with("02") || public.starts_with("03"),
                "{line}"
            );
            let point = uncompressed_on(curve, &hex(public)).unwrap();
            let secret = match curve {
                "p256" => p256::SecretKey::from_bytes(&padded_scalar(private))
                    .unwrap()
                    .agree(&point)
                    .map(|s| s.to_vec()),
                _ => p384::SecretKey::from_bytes(&padded_scalar(private))
                    .unwrap()
                    .agree(&point)
                    .map(|s| s.to_vec()),
            };
            assert_eq!(secret, Ok(hex(shared)), "{line}");
            seen += 1;
        }
        assert_eq!(seen, 1, "{curve}");
    }
}

fn der_length(length: usize) -> Vec<u8> {
    match length {
        0..=0x7f => vec![length as u8],
        0x80..=0xff => vec![0x81, length as u8],
        _ => vec![0x82, (length >> 8) as u8, length as u8],
    }
}

fn der(tag: u8, body: &[u8]) -> Vec<u8> {
    let mut out = vec![tag];
    out.extend(der_length(body.len()));
    out.extend_from_slice(body);
    out
}

/// An INTEGER from hex whose leading zero, if any, Wycheproof kept as the sign.
fn der_integer(field: &[u8]) -> Vec<u8> {
    let significant: Vec<u8> = field.iter().copied().skip_while(|b| *b == 0).collect();
    let mut body = Vec::new();
    if significant.first().is_some_and(|high| high & 0x80 != 0) {
        body.push(0);
    }
    body.extend_from_slice(&significant);
    der(2, &body)
}

fn rsa_public_der(modulus: &str, exponent: &str) -> Vec<u8> {
    let mut body = der_integer(&hex(modulus));
    body.extend(der_integer(&hex(exponent)));
    der(0x30, &body)
}

#[test]
fn an_rsa_key_reads_only_as_strict_der() {
    let row = edge_row("rsa2049-key");
    let good = rsa_public_der(row[0], row[1]);
    assert!(rsa::PublicKey::from_der(&good).is_ok());

    let modulus = der_integer(&hex(row[0]));
    let exponent = der_integer(&hex(row[1]));
    let mut trailing = good.clone();
    trailing.push(0);
    assert!(
        rsa::PublicKey::from_der(&trailing).is_err(),
        "byte after it"
    );
    let inside = der(0x30, &[modulus.clone(), exponent.clone(), vec![0]].concat());
    assert!(rsa::PublicKey::from_der(&inside).is_err(), "byte inside it");
    let swapped = der(0x30, &[exponent.clone(), modulus.clone()].concat());
    assert!(rsa::PublicKey::from_der(&swapped).is_err(), "parts swapped");
    assert!(rsa::PublicKey::from_der(&[]).is_err(), "empty");
    // n with a second zero before its sign byte, and a negative n.
    let raw: Vec<u8> = hex(row[0]).into_iter().skip_while(|b| *b == 0).collect();
    let padded = der(2, &[vec![0, 0], raw.clone()].concat());
    let extra = der(0x30, &[padded, exponent.clone()].concat());
    assert!(rsa::PublicKey::from_der(&extra).is_err(), "non-minimal n");
    // This modulus has its top bit set, so without a sign byte it reads as negative.
    let top: Vec<u8> = hex(edge_row("rsa8192-key")[0])
        .into_iter()
        .skip_while(|b| *b == 0)
        .collect();
    assert_ne!(top[0] & 0x80, 0);
    let negative = der(0x30, &[der(2, &top), exponent].concat());
    assert!(rsa::PublicKey::from_der(&negative).is_err(), "negative n");
}

/// The group's key, parsed once: a group shares one and R^2 is not free.
fn rsa_corpus(text: &str, verify: impl Fn(&rsa::PublicKey, &str, &[u8], &[u8]) -> bool) -> Tally {
    let current: RefCell<Option<(String, rsa::PublicKey)>> = RefCell::new(None);
    signature_corpus(text, |key, message, signature| {
        let mut slot = current.borrow_mut();
        if slot.as_ref().is_none_or(|(modulus, _)| modulus != key[1]) {
            // Through the DER a certificate carries, the way rustls reaches RSA.
            let public = rsa::PublicKey::from_der(&rsa_public_der(key[1], key[2])).unwrap();
            *slot = Some((key[1].to_string(), public));
        }
        let (_, public) = slot.as_ref().unwrap();
        verify(public, key[0], message, signature)
    })
}

#[test]
fn rsa_pkcs1_agrees_with_wycheproof() {
    let tally = rsa_corpus(
        include_str!("wycheproof/rsa_pkcs1.txt"),
        |public, hash, message, signature| match hash {
            "sha256" => public.verify_pkcs1::<Sha256>(message, signature).is_ok(),
            "sha384" => public.verify_pkcs1::<Sha384>(message, signature).is_ok(),
            "sha512" => public.verify_pkcs1::<Sha512>(message, signature).is_ok(),
            other => panic!("unknown hash {other}"),
        },
    );
    assert_eq!(tally.counts(), RSA_PKCS1_COUNTS, "{tally:?}");
}

#[test]
fn rsa_pss_agrees_with_wycheproof() {
    let tally = rsa_corpus(
        include_str!("wycheproof/rsa_pss.txt"),
        |public, hash, message, signature| match hash {
            "sha256" => public.verify_pss::<Sha256>(message, signature).is_ok(),
            "sha384" => public.verify_pss::<Sha384>(message, signature).is_ok(),
            "sha512" => public.verify_pss::<Sha512>(message, signature).is_ok(),
            other => panic!("unknown hash {other}"),
        },
    );
    assert_eq!(tally.counts(), RSA_PSS_COUNTS, "{tally:?}");
}

fn edge_row<'a>(name: &str) -> Vec<&'a str> {
    include_str!("edge_vectors.txt")
        .lines()
        .find(|line| line.split(' ').next() == Some(name))
        .unwrap_or_else(|| panic!("no row {name}"))
        .split(' ')
        .skip(1)
        .collect()
}

fn uncompressed(x: &[u8], y: &[u8]) -> Vec<u8> {
    let mut point = vec![4];
    point.extend_from_slice(x);
    point.extend_from_slice(y);
    point
}

// No corpus holds a coordinate that is the right residue but out of range: only
// the range check can tell x from x + p.
#[test]
fn a_p256_coordinate_of_p_or_more_is_refused_though_its_residue_is_on_the_curve() {
    let secret = p256::SecretKey::generate().unwrap();
    let row = edge_row("p256-small-x");
    let (x, y, x_plus_p) = (hex(row[0]), hex(row[1]), hex(row[2]));
    assert!(secret.agree(&uncompressed(&x, &y)).is_ok(), "small x");
    assert!(secret.agree(&uncompressed(&x_plus_p, &y)).is_err());
    let row = edge_row("p256-small-y");
    let (x, y, y_plus_p) = (hex(row[0]), hex(row[1]), hex(row[2]));
    assert!(secret.agree(&uncompressed(&x, &y)).is_ok(), "small y");
    assert!(secret.agree(&uncompressed(&x, &y_plus_p)).is_err());

    let secret = p384::SecretKey::generate().unwrap();
    let row = edge_row("p384-small-x");
    let (x, y, x_plus_p) = (hex(row[0]), hex(row[1]), hex(row[2]));
    assert!(secret.agree(&uncompressed(&x, &y)).is_ok(), "small x");
    assert!(secret.agree(&uncompressed(&x_plus_p, &y)).is_err());
    let row = edge_row("p384-small-y");
    let (x, y, y_plus_p) = (hex(row[0]), hex(row[1]), hex(row[2]));
    assert!(secret.agree(&uncompressed(&x, &y)).is_ok(), "small y");
    assert!(secret.agree(&uncompressed(&x, &y_plus_p)).is_err());
}

// P-384's secret half has no Wycheproof row of its own to generate from: RFC 5903
// section 8.2, whose values td did not compute, pins what goes on the wire.
#[test]
fn p384_keys_give_rfc_5903s_public_values_and_secret() {
    let i = "099f3c7034d4a2c699884d73a375a67f7624ef7c6b3c0f160647b67414dce655\
             e35b538041e649ee3faef896783ab194";
    let gi = "667842d7d180ac2cde6f74f37551f55755c7645c20ef73e31634fe72b4c55ee6\
              de3ac808acb4bdb4c88732aee95f41aa\
              9482ed1fc0eeb9cafc4984625ccfc23f65032149e0e144ada024181535a0f38e\
              eb9fcff3c2c947dae69b4c634573a81c";
    let r = "41cb0779b4bdb85d47846725fbec3c9430fab46cc8dc5060855cc9bda0aa2942\
             e0308312916b8ed2960e4bd55a7448fc";
    let gr = "e558dbef53eecde3d3fccfc1aea08a89a987475d12fd950d83cfa41732bc509d\
              0d1ac43a0336def96fda41d0774a3571\
              dcfbec7aacf3196472169e838430367f66eebe3c6e70c416dd5f0c68759dd1ff\
              f83fa40142209dff5eaad96db9e6386c";
    let shared = "11187331c279962d93d604243fd592cb9d0a926f422e47187521287e7156c5c4\
                  d603135569b9e9d09cf5d4a270f59746";
    let scalar = |text: &str| -> [u8; 48] { hex(text).try_into().unwrap() };
    let i = p384::SecretKey::from_bytes(&scalar(i)).unwrap();
    let r = p384::SecretKey::from_bytes(&scalar(r)).unwrap();
    let public_i = i.public_key().unwrap();
    let public_r = r.public_key().unwrap();
    assert_eq!(public_i[0], 4);
    assert_eq!(public_i[1..].to_vec(), hex(gi));
    assert_eq!(public_r[1..].to_vec(), hex(gr));
    assert_eq!(i.agree(&public_r).unwrap().to_vec(), hex(shared));
    assert_eq!(r.agree(&public_i).unwrap().to_vec(), hex(shared));

    // The same point under every other tag.
    for tag in [0u8, 2, 3, 5, 6, 7] {
        let mut tagged = public_r;
        tagged[0] = tag;
        assert!(i.agree(&tagged).is_err(), "tag {tag}");
    }

    let one = p384::SecretKey::generate().unwrap();
    let other = p384::SecretKey::generate().unwrap();
    let (a, b) = (one.public_key().unwrap(), other.public_key().unwrap());
    assert_ne!(a, b);
    assert_eq!(one.agree(&b).unwrap(), other.agree(&a).unwrap());
}

#[test]
fn a_secret_scalar_must_lie_in_one_to_n() {
    fn check<const N: usize>(n: &str, accepted: impl Fn(&[u8; N]) -> bool) {
        let order: [u8; N] = hex(n).try_into().unwrap();
        let add = |value: [u8; N], by: u8| -> [u8; N] {
            let mut out = value;
            let mut carry = by;
            for byte in out.iter_mut().rev() {
                let (sum, over) = byte.overflowing_add(carry);
                *byte = sum;
                carry = u8::from(over);
            }
            out
        };
        // The order ends in an odd byte above 1, so this needs no borrow.
        let mut below = order;
        below[N - 1] -= 1;
        assert!(accepted(&below), "n - 1");
        let mut one = [0u8; N];
        one[N - 1] = 1;
        assert!(accepted(&one), "1");
        assert!(!accepted(&[0u8; N]), "0");
        assert!(!accepted(&order), "n");
        assert!(!accepted(&add(order, 1)), "n + 1");
        assert!(!accepted(&[0xff; N]), "all ones");
    }
    check::<32>(
        "ffffffff00000000ffffffffffffffffbce6faada7179e84f3b9cac2fc632551",
        |bytes| p256::SecretKey::from_bytes(bytes).is_ok(),
    );
    check::<48>(
        "ffffffffffffffffffffffffffffffffffffffffffffffffc7634d81f4372ddf\
         581a0db248b0a77aecec196accc52973",
        |bytes| p384::SecretKey::from_bytes(bytes).is_ok(),
    );
}

// A forgery for any key exists at e = 0, so a digest of nothing must not count
// as one: with c = 1, r = x(Q) and s = r verify when e = 0.
#[test]
fn a_digest_shorter_than_the_shortest_hash_is_refused() {
    let key = p256::SecretKey::generate().unwrap();
    let public = key.public_key().unwrap();
    let signature = key.sign_sha256(b"message").unwrap();
    let digest = Sha256::digest(b"message");
    assert_eq!(p256::verify(&public, &digest, &signature), Ok(()));
    assert!(p256::verify(&public, &digest[..31], &signature).is_err());
    assert!(p256::verify(&public, &[], &signature).is_err());

    let x = &public[1..33];
    let r_is_x = der(0x30, &[der_integer(x), der_integer(x)].concat());
    assert!(p256::verify(&public, &[], &r_is_x).is_err(), "the forgery");
    assert!(
        p256::verify(&public, &[0; 31], &r_is_x).is_err(),
        "one byte short of a hash"
    );
    assert!(
        p256::verify(&public, &[0; 32], &r_is_x).is_ok(),
        "e = 0 itself is a hash's business"
    );

    // The floor is the same on the longer curve, where 32 bytes are right-aligned.
    let key = p384::SecretKey::generate().unwrap();
    let public = key.public_key().unwrap();
    let x = &public[1..49];
    let r_is_x = der(0x30, &[der_integer(x), der_integer(x)].concat());
    assert!(p384::verify(&public, &[], &r_is_x).is_err());
    assert!(p384::verify(&public, &[0; 31], &r_is_x).is_err());
    assert!(p384::verify(&public, &[0; 32], &r_is_x).is_ok());
    assert!(p384::verify(&public, &[0; 48], &r_is_x).is_ok());
}

// A real pairing in TLS and certificates that no Wycheproof file holds.
#[test]
fn p256_verifies_a_truncated_sha384_digest() {
    let row = edge_row("p256-sha384");
    let (public, message, signature) = (hex(row[0]), hex(row[1]), hex(row[2]));
    let digest = Sha384::digest(&message);
    assert_eq!(p256::verify(&public, &digest, &signature), Ok(()));
    let mut other = digest;
    other[0] ^= 1;
    assert!(
        p256::verify(&public, &other, &signature).is_err(),
        "leftmost"
    );
    // A change past the leftmost 32 bytes is outside what was signed.
    let mut beyond = digest;
    beyond[47] ^= 1;
    assert_eq!(
        p256::verify(&public, &beyond, &signature),
        Ok(()),
        "rightmost"
    );
}

// The range check again, on the coordinate a compressed point carries alone, and
// on P-521, whose 66-byte coordinates leave room above p.
#[test]
fn a_coordinate_of_p_or_more_is_refused_in_either_form() {
    for curve in ["p256", "p384", "p521"] {
        let row = edge_row(&format!("{curve}-small-x"));
        let (x, y, x_plus_p) = (hex(row[0]), hex(row[1]), hex(row[2]));
        let point = uncompressed(&x, &y);
        assert_eq!(uncompressed_on(curve, &point), Ok(point.clone()), "{curve}");
        assert!(uncompressed_on(curve, &uncompressed(&x_plus_p, &y)).is_err());
        let odd = y[y.len() - 1] & 1;
        let tagged = |tag: u8, x: &[u8]| [&[tag][..], x].concat();
        assert_eq!(
            uncompressed_on(curve, &tagged(2 | odd, &x)),
            Ok(point.clone()),
            "{curve} compressed"
        );
        for tag in [2, 3] {
            assert!(uncompressed_on(curve, &tagged(tag, &x_plus_p)).is_err());
        }
        let row = edge_row(&format!("{curve}-small-y"));
        let (x, y, y_plus_p) = (hex(row[0]), hex(row[1]), hex(row[2]));
        let point = uncompressed(&x, &y);
        assert_eq!(uncompressed_on(curve, &point), Ok(point.clone()), "{curve}");
        assert!(uncompressed_on(curve, &uncompressed(&x, &y_plus_p)).is_err());
    }
}

// Pairings td-crypto's certificate policy admits and Wycheproof holds only with
// SHA-512; each digest is shorter than the order, so the whole of it is signed.
#[test]
fn p521_verifies_sha256_and_sha384_digests() {
    let row = edge_row("p521-sha256");
    let (public, message, signature) = (hex(row[0]), hex(row[1]), hex(row[2]));
    let digest = Sha256::digest(&message);
    assert_eq!(p521::verify(&public, &digest, &signature), Ok(()));
    let mut other = digest;
    other[31] ^= 1;
    assert!(p521::verify(&public, &other, &signature).is_err(), "sha256");
    let row = edge_row("p521-sha384");
    let (public, message, signature) = (hex(row[0]), hex(row[1]), hex(row[2]));
    let digest = Sha384::digest(&message);
    assert_eq!(p521::verify(&public, &digest, &signature), Ok(()));
    let mut other = digest;
    other[47] ^= 1;
    assert!(p521::verify(&public, &other, &signature).is_err(), "sha384");
    // The key compressed is refused here: verification takes one form.
    let compressed = [&[2 | (public[132] & 1)][..], &public[1..67]].concat();
    assert_eq!(
        p521::uncompressed(&compressed),
        Ok(public.clone().try_into().unwrap())
    );
    assert!(p521::verify(&compressed, &digest, &signature).is_err());
}

// 66 bytes are 528 bits: only the leftmost 521 were signed.
#[test]
fn p521_keeps_the_leftmost_521_bits_of_a_long_digest() {
    let row = edge_row("p521-digest66");
    let (public, digest, signature) = (hex(row[0]), hex(row[1]), hex(row[2]));
    assert_eq!(digest.len(), 66);
    assert_eq!(p521::verify(&public, &digest, &signature), Ok(()));
    let mut beyond = digest.clone();
    beyond[65] ^= 0x7f;
    assert_eq!(
        p521::verify(&public, &beyond, &signature),
        Ok(()),
        "the low seven bits"
    );
    let mut last = digest.clone();
    last[65] ^= 0x80;
    assert!(p521::verify(&public, &last, &signature).is_err(), "bit 521");
    // A digest one byte longer still keeps the same 521.
    let longer = [&digest[..], &[0xff]].concat();
    assert_eq!(
        p521::verify(&public, &longer, &signature),
        Ok(()),
        "67 bytes"
    );
}

// The width where PSS's encoded message is a byte shorter than the modulus.
#[test]
fn a_2049_bit_pss_signature_needs_a_zero_leading_byte() {
    let row = edge_row("rsa2049-key");
    let strip =
        |field: &str| -> Vec<u8> { hex(field).into_iter().skip_while(|b| *b == 0).collect() };
    let public = rsa::PublicKey::from_parts(&strip(row[0]), &strip(row[1])).unwrap();
    let message = hex(edge_row("rsa2049-message")[0]);
    let signature = |name| hex(edge_row(name)[0]);
    assert_eq!(
        public.verify_pss::<Sha256>(&message, &signature("rsa2049-valid")),
        Ok(())
    );
    for name in [
        "rsa2049-lead-byte",
        "rsa2049-flipped",
        "rsa2049-short-salt",
        "rsa2049-long-salt",
    ] {
        assert!(
            public
                .verify_pss::<Sha256>(&message, &signature(name))
                .is_err(),
            "{name}"
        );
    }
    let mut other = message.clone();
    other[0] ^= 1;
    assert!(public
        .verify_pss::<Sha256>(&other, &signature("rsa2049-valid"))
        .is_err());
}

#[test]
fn the_top_of_the_rsa_range_verifies_and_its_bounds_hold() {
    let row = edge_row("rsa8192-key");
    let strip =
        |field: &str| -> Vec<u8> { hex(field).into_iter().skip_while(|b| *b == 0).collect() };
    let exponent = strip(row[1]);
    let public = rsa::PublicKey::from_parts(&strip(row[0]), &exponent).unwrap();
    let message = hex(edge_row("rsa8192-message")[0]);
    let pkcs1 = hex(edge_row("rsa8192-pkcs1")[0]);
    let pss = hex(edge_row("rsa8192-pss")[0]);
    assert_eq!(public.verify_pkcs1::<Sha256>(&message, &pkcs1), Ok(()));
    assert_eq!(public.verify_pss::<Sha256>(&message, &pss), Ok(()));
    let mut other = message.clone();
    other[0] ^= 1;
    assert!(public.verify_pkcs1::<Sha256>(&other, &pkcs1).is_err());
    assert!(public.verify_pss::<Sha256>(&other, &pss).is_err());

    // Any odd value of the width will do: the bounds are on the width alone.
    let width = |top: u8, bytes: usize| -> Vec<u8> {
        let mut n = vec![0xff; bytes];
        n[0] = top;
        n
    };
    let held = |n: &[u8]| rsa::PublicKey::from_parts(n, &exponent).is_ok();
    assert!(held(&width(0xff, 1024)), "8192 bits");
    assert!(!held(&width(0x01, 1025)), "8193 bits");
    assert!(held(&width(0x80, 256)), "2048 bits");
    assert!(!held(&width(0x7f, 256)), "2047 bits");
    let n = width(0xff, 256);
    let with_e = |e: &[u8]| rsa::PublicKey::from_parts(&n, e).is_ok();
    assert!(with_e(&[3]), "e = 3");
    assert!(!with_e(&[1]), "e = 1");
    assert!(!with_e(&[4]), "even");
    assert!(with_e(&[1, 0xff, 0xff, 0xff, 0xff]), "33 bits");
    assert!(!with_e(&[3, 0xff, 0xff, 0xff, 0xff]), "34 bits");
    // Nine bytes, which an accumulating u64 would read as e = 3.
    assert!(!with_e(&[1, 0, 0, 0, 0, 0, 0, 0, 3]), "nine bytes");
}

// A public key that is the identity makes any message verify with S = 1 and
// R = B, by the verification equation alone, so each spelling of that key can
// be tried with a signature no key had to make. RFC 8032 5.1.3 refuses the two
// non-canonical spellings; only the canonical one reaches the equation.
#[test]
fn only_the_canonical_spelling_of_an_ed25519_key_is_accepted() {
    let mut base = [0x66u8; 32];
    base[0] = 0x58;
    let mut signature = [0u8; 64];
    signature[..32].copy_from_slice(&base);
    signature[32] = 1;

    let mut identity = [0u8; 32];
    identity[0] = 1;
    assert_eq!(ed25519::verify(&identity, b"any", &signature), Ok(()));

    // y = p + 1.
    let mut y_past_p = [0xffu8; 32];
    y_past_p[0] = 0xee;
    y_past_p[31] = 0x7f;
    assert!(ed25519::verify(&y_past_p, b"any", &signature).is_err());

    // x = 0 with its sign bit set.
    let mut negative_zero = identity;
    negative_zero[31] = 0x80;
    assert!(ed25519::verify(&negative_zero, b"any", &signature).is_err());
}

// valid, invalid, acceptable taken, acceptable refused. Each acceptable row's
// treatment is a decision DESIGN.md records, so a change to one reds here:
// X25519 refuses the 31 low-order keys whose secret is zero, as RFC 8446
// requires, and takes the rest; ECDH refuses the compressed point; Ed25519 takes
// small-order and mixed-order keys, since a trusted key is neither; PKCS#1 refuses a
// DigestInfo missing its NULL.
const X25519_COUNTS: [usize; 4] = [264, 0, 223, 31];
const P256_ECDH_COUNTS: [usize; 4] = [330, 24, 0, 1];
const P384_ECDH_COUNTS: [usize; 4] = [771, 18, 0, 1];
const ED25519_COUNTS: [usize; 4] = [117, 64, 29, 0];
const ECDSA_COUNTS: [usize; 4] = [1236, 1862, 0, 0];
const EC_POINT_COUNTS: &[((&str, &str), usize)] = &[
    (("p256", "invalid"), 24),
    (("p256", "valid"), 735),
    (("p384", "invalid"), 18),
    (("p384", "valid"), 1333),
    (("p521", "invalid"), 28),
    (("p521", "valid"), 824),
];
const RSA_PKCS1_COUNTS: [usize; 4] = [39, 1250, 0, 5];
const RSA_PSS_COUNTS: [usize; 4] = [353, 183, 0, 0];

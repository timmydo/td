//! Known answers from sources independent of this crate: Wycheproof, Python's
//! hashlib and hmac, and naive big-integer references written for these tests
//! (DESIGN.md). Each corpus states its row count, so a truncated or filtered file
//! reds rather than passing on less.

#![allow(clippy::indexing_slicing, clippy::unwrap_used, clippy::panic)]

use td_primitives::aes_gcm::AesGcm;
use td_primitives::chacha20_poly1305;
use td_primitives::hmac::mac;
use td_primitives::sha::{sha1, Digest, Sha256, Sha384, Sha512};

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

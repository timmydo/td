//! SHA-256 digest, HMAC-SHA256 (RFC 2104) and one-block HKDF (RFC 5869),
//! the one copy td's std-only crates share (td-fido/DESIGN.md, "Shared
//! HMAC-SHA256"), over the engine's SHA-256 its includer mounts as
//! `super::sha256`: td-fido's `crypto.rs`, and td-secret's store crypto,
//! which compiles this file by path.
//!
//! The key-derived buffers this file owns are zeroed before return: the
//! normalized key, the hashed long key, each pad and the inner hash. The
//! engine's `Sha256` keeps its chaining state and block buffer private and
//! consumes itself in `finalize`, so the key-derived midstates inside it
//! are not cleared; zeroing is best effort in safe Rust either way.

/// The SHA-256 digest of `bytes`.
pub fn digest(bytes: &[u8]) -> [u8; 32] {
    let mut hash = super::sha256::Sha256::new();
    hash.update(bytes);
    hash.finalize()
}

/// HMAC-SHA256 under `key` over the concatenation of `parts`.
pub fn hmac_sha256(key: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    let mut normalized = [0u8; 64];
    if key.len() > 64 {
        let mut hashed = digest(key);
        for (out, byte) in normalized.iter_mut().zip(&hashed) {
            *out = *byte;
        }
        clear(&mut hashed);
    } else {
        for (out, byte) in normalized.iter_mut().zip(key) {
            *out = *byte;
        }
    }
    let mut pad = [0u8; 64];
    for (out, byte) in pad.iter_mut().zip(&normalized) {
        *out = byte ^ 0x36;
    }
    let mut inner = super::sha256::Sha256::new();
    inner.update(&pad);
    for part in parts {
        inner.update(part);
    }
    let mut inner_hash = inner.finalize();
    for (out, byte) in pad.iter_mut().zip(&normalized) {
        *out = byte ^ 0x5c;
    }
    let mut outer = super::sha256::Sha256::new();
    outer.update(&pad);
    outer.update(&inner_hash);
    let result = outer.finalize();
    clear(&mut normalized);
    clear(&mut pad);
    clear(&mut inner_hash);
    result
}

/// HMAC-SHA256 under `key` over `data`.
pub fn hmac(key: &[u8], data: &[u8]) -> [u8; 32] {
    hmac_sha256(key, &[data])
}

/// RFC 5869 HKDF-SHA256 with a single 32-byte output block.
pub fn hkdf(ikm: &[u8], salt: &[u8], info: &[u8]) -> [u8; 32] {
    let mut prk = hmac_sha256(salt, &[ikm]);
    let result = hmac_sha256(&prk, &[info, &[1]]);
    clear(&mut prk);
    result
}

fn clear(bytes: &mut [u8]) {
    bytes.fill(0);
    std::hint::black_box(bytes);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(text: &str) -> Vec<u8> {
        text.as_bytes()
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect()
    }

    /// RFC 4231's HMAC-SHA-256 cases but the truncated fifth, each whole and
    /// split into two parts at every offset.
    #[test]
    fn hmac_sha256_matches_rfc_4231() {
        let long_key = [0xaa; 131];
        let key_four: Vec<u8> = (1..=25).collect();
        let cases: &[(&[u8], &[u8], &str)] = &[
            (
                &[0x0b; 20],
                b"Hi There",
                "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7",
            ),
            (
                b"Jefe",
                b"what do ya want for nothing?",
                "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843",
            ),
            (
                &[0xaa; 20],
                &[0xdd; 50],
                "773ea91e36800e46854db8ebd09181a72959098b3ef8c122d9635514ced565fe",
            ),
            (
                &key_four,
                &[0xcd; 50],
                "82558a389a443c0ea4cc819899f2083a85f0faa3e578f8077a2e3ff46729665b",
            ),
            (
                &long_key,
                b"Test Using Larger Than Block-Size Key - Hash Key First",
                "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54",
            ),
            (
                &long_key,
                b"This is a test using a larger than block-size key and a larger than block-size data. The key needs to be hashed before being used by the HMAC algorithm.",
                "9b09ffa71b942fcb27635fbcd5b0e944bfdc63644f0713938a7f51535c3a35e2",
            ),
        ];
        for (key, data, expected) in cases {
            let expected = hex(expected);
            assert_eq!(hmac_sha256(key, &[data]).to_vec(), expected);
            assert_eq!(hmac(key, data).to_vec(), expected);
            assert_eq!(hmac_sha256(key, &[]), hmac_sha256(key, &[b""]));
            for at in 0..=data.len() {
                let (head, tail) = data.split_at(at);
                assert_eq!(hmac_sha256(key, &[head, tail]).to_vec(), expected);
            }
        }
    }

    /// The block-size boundary: a 64-byte key is used as is and a 65-byte
    /// key is hashed first. Expected values from Python 3's `hmac` module.
    #[test]
    fn hmac_sha256_keys_at_and_past_the_block_size() {
        let at: Vec<u8> = (0..64).collect();
        let past: Vec<u8> = (0..65).collect();
        assert_eq!(hmac_sha256(&at, &[b"td-fido"]).to_vec(), hex(KEY_64));
        assert_eq!(hmac_sha256(&past, &[b"td-fido"]).to_vec(), hex(KEY_65));
        assert_eq!(
            hmac_sha256(&past, &[b"td-fido"]),
            hmac_sha256(&digest(&past), &[b"td-fido"])
        );
    }

    const KEY_64: &str = "0a2a0d24a8c9da5c6161c6095800cea0594ba92c083736ffac0bd0e17878cb33";
    const KEY_65: &str = "2609fde6836c11c08245325112404eb15e73e5eb0263b7775650f146ef5cf079";

    #[test]
    fn hkdf_matches_rfc_5869_case_one() {
        assert_eq!(
            hkdf(
                &[0x0b; 22],
                &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12],
                &[0xf0, 0xf1, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8, 0xf9],
            )
            .to_vec(),
            hex("3cb25f25faacd57a90434f64d0362f2a2d2d0a90cf1a5a4c5db02d56ecc4c5bf")
        );
    }

    #[test]
    fn digest_matches_fips_180_abc() {
        assert_eq!(
            digest(b"abc").to_vec(),
            hex("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
        );
    }
}

//! SHA-256 digest, HMAC-SHA256 (RFC 2104), HKDF (RFC 5869), one block
//! (`hkdf`) or any length up to RFC 5869's bound (`hkdf_expand`), and
//! PBKDF2-HMAC-SHA256 (RFC 8018) for td-luks's LUKS2 keyslots and digests,
//! the one copy td's std-only crates share (td-fido/DESIGN.md, "Shared
//! HMAC-SHA256"), over the engine's SHA-256 its includer mounts as
//! `super::sha256`: td-fido's `crypto.rs`, and td-secret's store crypto,
//! which compiles this file by path.
//!
//! The key-derived buffers this file owns are zeroed before return: the
//! normalized key, the hashed long key, each pad, the inner hash,
//! HKDF's pseudorandom key and output blocks, and PBKDF2's chain links and
//! sums. The
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
    hmac_stream(key, parts.iter().copied())
}

/// HMAC-SHA256 over parts as an iterator yields them, so a caller
/// assembling parts per block allocates nothing.
fn hmac_stream<'a>(key: &[u8], parts: impl Iterator<Item = &'a [u8]>) -> [u8; 32] {
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

/// RFC 5869's longest output, 255 SHA-256 blocks.
pub const HKDF_MAX_OKM: usize = 255 * 32;

/// RFC 5869 HKDF-SHA256 filling `okm`, of at most `HKDF_MAX_OKM` bytes,
/// with `info` the concatenation of `info_parts`. A longer `okm` is
/// refused and left untouched.
pub fn hkdf_expand(
    ikm: &[u8],
    salt: &[u8],
    info_parts: &[&[u8]],
    okm: &mut [u8],
) -> Result<(), &'static str> {
    if okm.len() > HKDF_MAX_OKM {
        return Err("HKDF-SHA256 gives at most 8160 bytes");
    }
    let mut prk = hmac_sha256(salt, &[ikm]);
    let mut block = [0u8; 32];
    for (index, chunk) in okm.chunks_mut(32).enumerate() {
        // At most 255 chunks, so the counter is 1 to 255.
        let counter = [u8::try_from(index + 1).unwrap_or(u8::MAX)];
        let previous: &[u8] = if index == 0 { &[] } else { &block };
        let parts = std::iter::once(previous)
            .chain(info_parts.iter().copied())
            .chain(std::iter::once(&counter[..]));
        let mut next = hmac_stream(&prk, parts);
        block = next;
        clear(&mut next);
        for (out, byte) in chunk.iter_mut().zip(&block) {
            *out = *byte;
        }
    }
    clear(&mut block);
    clear(&mut prk);
    Ok(())
}

/// PBKDF2's most iterations, the bound td-luks reads LUKS2 digests under
/// (td-luks/DESIGN.md "The profile").
pub const PBKDF2_MAX_ITERATIONS: u32 = 4_000_000;
/// PBKDF2's longest output here, 32 SHA-256 blocks.
pub const PBKDF2_MAX_OUTPUT: usize = 1024;

/// PBKDF2-HMAC-SHA256 (RFC 8018 5.2) of `password` and `salt` filling
/// `out`, of 1 to `PBKDF2_MAX_OUTPUT` bytes, after 1 to
/// `PBKDF2_MAX_ITERATIONS` iterations. Anything else is refused and `out`
/// left untouched.
pub fn pbkdf2_sha256(
    password: &[u8],
    salt: &[u8],
    iterations: u32,
    out: &mut [u8],
) -> Result<(), &'static str> {
    if iterations == 0 || iterations > PBKDF2_MAX_ITERATIONS {
        return Err("PBKDF2 takes 1 to 4000000 iterations");
    }
    if out.is_empty() || out.len() > PBKDF2_MAX_OUTPUT {
        return Err("PBKDF2 gives 1 to 1024 bytes here");
    }
    for (index, chunk) in out.chunks_mut(32).enumerate() {
        // At most 32 chunks, so the block index is 1 to 32.
        let block = u32::try_from(index + 1).unwrap_or(u32::MAX).to_be_bytes();
        let mut link = hmac_sha256(password, &[salt, &block]);
        let mut sum = link;
        for _ in 1..iterations {
            let mut next = hmac_sha256(password, &[&link]);
            link = next;
            clear(&mut next);
            for (total, byte) in sum.iter_mut().zip(&link) {
                *total ^= byte;
            }
        }
        for (target, byte) in chunk.iter_mut().zip(&sum) {
            *target = *byte;
        }
        clear(&mut link);
        clear(&mut sum);
    }
    Ok(())
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

    /// RFC 5869's three SHA-256 cases at their whole output lengths, the
    /// info split into parts, one block agreeing with `hkdf`, and the
    /// length bound.
    #[test]
    fn hkdf_expand_matches_rfc_5869() {
        let info: Vec<u8> = (0xb0..=0xff).collect();
        let ikm: Vec<u8> = (0x00..=0x4f).collect();
        let salt: Vec<u8> = (0x60..=0xaf).collect();
        // Input keying material, salt, info and the expected output.
        type Case<'a> = (&'a [u8], &'a [u8], &'a [u8], &'a str);
        let cases: &[Case] = &[
            (
                &[0x0b; 22],
                &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12],
                &[0xf0, 0xf1, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8, 0xf9],
                "3cb25f25faacd57a90434f64d0362f2a2d2d0a90cf1a5a4c5db02d56ecc4c5bf34007208d5b887185865",
            ),
            (
                &ikm,
                &salt,
                &info,
                "b11e398dc80327a1c8e7f78c596a49344f012eda2d4efad8a050cc4c19afa97c59045a99cac7827271cb41c65e590e09da3275600c2f09b8367793a9aca3db71cc30c58179ec3e87c14c01d5c1f3434f1d87",
            ),
            (
                &[0x0b; 22],
                &[],
                &[],
                "8da4e775a563c18f715f802a063c5a31b8a11f5c5ee1879ec3454e5f3c738d2d9d201395faa4b61a96c8",
            ),
        ];
        for (ikm, salt, info, expected) in cases {
            let expected = hex(expected);
            let mut okm = vec![0; expected.len()];
            hkdf_expand(ikm, salt, &[info], &mut okm).unwrap();
            assert_eq!(okm, expected);
            for at in 0..=info.len() {
                let (head, tail) = info.split_at(at);
                let mut split = vec![0; expected.len()];
                hkdf_expand(ikm, salt, &[head, tail], &mut split).unwrap();
                assert_eq!(split, expected);
            }
            let mut one = [0; 32];
            hkdf_expand(ikm, salt, &[info], &mut one).unwrap();
            assert_eq!(one, hkdf(ikm, salt, info));
        }
        let mut most = vec![0; HKDF_MAX_OKM];
        hkdf_expand(b"k", b"", &[], &mut most).unwrap();
        let mut over = vec![7; HKDF_MAX_OKM + 1];
        assert!(hkdf_expand(b"k", b"", &[], &mut over).is_err());
        assert!(over.iter().all(|byte| *byte == 7));
        let mut empty = [0u8; 0];
        hkdf_expand(b"k", b"", &[], &mut empty).unwrap();
    }

    /// RFC 7914 §11's two vectors, then three Python `hashlib.pbkdf2_hmac`
    /// gives, which reproduces both: td-luks's shape (1000 iterations, a
    /// 32-byte salt, a 64-byte key), a password longer than a block with a
    /// length that ends inside a block, and the longest output, pinned by
    /// its SHA-256.
    #[test]
    fn pbkdf2_sha256_matches_rfc_7914_and_hashlib() {
        let cases: &[(&[u8], &[u8], u32, &str)] = &[
            (
                b"passwd",
                b"salt",
                1,
                concat!(
                    "55ac046e56e3089fec1691c22544b605f94185216dde0465e68b9d57c20dacbc",
                    "49ca9cccf179b645991664b39d77ef317c71b845b1e30bd509112041d3a19783",
                ),
            ),
            (
                b"Password",
                b"NaCl",
                80000,
                concat!(
                    "4ddcd8f60b98be21830cee5ef22701f9641a4418d04c0414aeff08876b34ab56",
                    "a1d425a1225833549adb841b51c9b3176a272bdebba1d078478f62b397f33c8d",
                ),
            ),
            (
                b"td-luks",
                &[0; 32],
                1000,
                concat!(
                    "872ddf56eff7f53476771f7d8045d8312a46e65b387d4841e6c7f3c74ae4df71",
                    "660e84971bc0590de1d7c5249d8db507f738d69b8d05a5f080a6e00ef7d88821",
                ),
            ),
            (
                &[b'k'; 100],
                b"salt",
                3,
                "219c68bb88f1010f4a763b7c8fd58639c2280baf6820814c6d404891fcd85c3d19",
            ),
        ];
        for (password, salt, iterations, expected) in cases {
            let expected = hex(expected);
            let mut out = vec![0; expected.len()];
            pbkdf2_sha256(password, salt, *iterations, &mut out).unwrap();
            assert_eq!(out, expected);
        }

        let mut longest = vec![0; PBKDF2_MAX_OUTPUT];
        pbkdf2_sha256(b"td-fido", b"maximum", 2, &mut longest).unwrap();
        assert_eq!(
            digest(&longest).to_vec(),
            hex("9f0876cc7c12a879ccb317c0430edc65f1fd5db6b60f838cec9cb8e952edc782")
        );
    }

    #[test]
    fn pbkdf2_sha256_refuses_and_leaves_output_untouched() {
        let mut out = [7u8; 32];
        for iterations in [0, PBKDF2_MAX_ITERATIONS + 1] {
            assert!(pbkdf2_sha256(b"p", b"s", iterations, &mut out).is_err());
        }
        assert!(pbkdf2_sha256(b"p", b"s", 1, &mut []).is_err());
        let mut over = vec![7; PBKDF2_MAX_OUTPUT + 1];
        assert!(pbkdf2_sha256(b"p", b"s", 1, &mut over).is_err());
        assert!(out.iter().chain(&over).all(|byte| *byte == 7));
        over.pop();
        pbkdf2_sha256(b"p", b"s", 1, &mut over).unwrap();
        // Each block is its own chain: the first equals a one-block call.
        pbkdf2_sha256(b"p", b"s", 1, &mut out).unwrap();
        assert_eq!(over[..32], out);
    }

    #[test]
    fn digest_matches_fips_180_abc() {
        assert_eq!(
            digest(b"abc").to_vec(),
            hex("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
        );
    }
}

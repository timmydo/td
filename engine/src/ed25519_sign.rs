// ed25519 SIGNING, deliberately in its own file.
//
// `ed25519.rs` is verify-only and stays that way: td-boot `#[path]`-includes
// THAT file, so anything added to it lands in the boot binary. That was written
// here in the future tense one commit before it became true, and it is now
// simply true (DESIGN.md §10 item 5). This module is not on that path — the
// boot path's job is to refuse what does not verify, and a signer there would
// be a crypto surface serving no boot-time purpose. It exists for ONE caller:
// the recipe-check oracle, which must sign a manifest whose digests change with
// every build and so cannot use a committed fixture signature.
//
// Neither half of that is enforced by the compiler, so both are asserted
// against the tree in `builder/src/affected.rs`: td-boot may name no path to
// this file, and `ed25519.rs` may declare no signing entry point.
//
// That caller lives in the dependency-free workspace, which is why this is
// hand-rolled rather than `ring`: `recipes` may carry no external crate, and the
// host signer (`td-deploy`, which does use `ring`) is not reachable from a gate
// — no recipe builds td-net and nothing puts it on a check's PATH.
//
// WHAT THIS IS NOT FOR. It signs THROWAWAY per-run test keys. Nothing that
// authorises a real deployment should be signed here; `td-deploy` is the signer
// for that, and it uses an audited implementation. The nonce below is RFC 8032's
// deterministic one, so there is no RNG to get wrong, but this code has none of
// the side-channel hardening a production signer wants and is not claimed to —
// see `ed25519.rs`'s note on `scalar_mul`, which this module drives with a
// SECRET scalar where every other caller drives it with a public one.
//
// Correctness is pinned three ways, because a signer cannot check its own work,
// and WHERE EACH RUNS differs — which matters more than the count:
//   - the RFC 8032 §7.1 vectors (public, and the standard's own) and a
//     round-trip through `ed25519::verify` are `mod tests` below, so the gate's
//     `cargo test --workspace` leg runs them;
//   - the differential against `ring` in `net/src/ed25519_cross.rs` is the
//     strongest of the three and NO GATE RUNS IT, because nothing builds td-net
//     from source. That file's own header states this at length; it is repeated
//     here so the count is not read as three gated checks.

use crate::ed25519::{
    at_least_l, load_words, reduce_wide, store_words, sub_l, Point, BASE_POINT,
};
use crate::sha512;

/// The 32-byte seed an ed25519 private key is. The seed is this module's own;
/// the signature length is RE-EXPORTED from the verifier rather than declared
/// again, since two `= 64` in one crate are two things to change the day either
/// moves and nothing would catch a caller importing the stale one.
pub const SEED_LEN: usize = 32;
pub use crate::ed25519::SIGNATURE_LEN;

/// RFC 8032 §5.1.5: the seed's SHA-512, split into the CLAMPED scalar and the
/// prefix the nonce is derived from.
///
/// Clamping is three bit operations and every one of them matters: clearing the
/// low three bits makes the scalar a multiple of the cofactor (so a small-order
/// component cannot survive), clearing the top bit keeps it below 2^255, and
/// setting bit 254 fixes the leading bit so the scalar ladder runs a constant
/// number of steps.
fn expand(seed: &[u8; SEED_LEN]) -> ([u8; 32], [u8; 32]) {
    let mut hasher = sha512::Sha512::new();
    hasher.update(seed);
    let h = hasher.finalize();
    let (mut scalar, mut prefix) = ([0u8; 32], [0u8; 32]);
    for (slot, byte) in scalar.iter_mut().zip(h.iter()) {
        *slot = *byte;
    }
    for (slot, byte) in prefix.iter_mut().zip(h.iter().skip(32)) {
        *slot = *byte;
    }
    if let Some(first) = scalar.first_mut() {
        *first &= 248;
    }
    if let Some(last) = scalar.last_mut() {
        *last &= 127;
        *last |= 64;
    }
    (scalar, prefix)
}

/// The public half of a seed: `[s]B` compressed.
///
/// `None` carries the same one failure `sign` does; see there.
#[must_use]
pub fn public_key(seed: &[u8; SEED_LEN]) -> Option<[u8; 32]> {
    let (scalar, _) = expand(seed);
    base_mul(&scalar)
}

/// Sign `message` under `seed`, RFC 8032 Ed25519 (not ph, not ctx).
///
/// Returns `None` only if `BASE_POINT` fails to decompress — a constant this
/// crate ships and `the_base_point_decompresses_to_its_known_coordinates`
/// pins, so it is unreachable in any build that runs its own tests. It is an
/// `Option` rather than a panic because this crate may not panic, and it is
/// PROPAGATED rather than absorbed into an all-zero sentinel because both
/// things a sentinel produces are silent: a zeroed public key published as a
/// trust root, and a `sign` that reports success and hands back bytes which
/// then fail to verify on a machine — a signing bug wearing a boot failure's
/// clothes.
#[must_use]
pub fn sign(seed: &[u8; SEED_LEN], message: &[u8]) -> Option<[u8; SIGNATURE_LEN]> {
    let (scalar, prefix) = expand(seed);
    // From the scalar already in hand rather than `public_key(seed)`, which
    // would hash the seed a second time for the same answer.
    let a = base_mul(&scalar)?;

    // r = H(prefix || M) mod L. Deterministic, so there is no RNG here to
    // misuse — and a repeated nonce under a different message is what leaks a
    // private key outright.
    let mut hasher = sha512::Sha512::new();
    hasher.update(&prefix);
    hasher.update(message);
    let r = reduce_wide(&hasher.finalize());
    let r_point = base_mul(&r)?;

    // k = H(R || A || M) mod L, the same challenge `verify` recomputes.
    let mut hasher = sha512::Sha512::new();
    hasher.update(&r_point);
    hasher.update(&a);
    hasher.update(message);
    let k = reduce_wide(&hasher.finalize());

    let s = muladd(&k, &scalar, &r);
    let mut signature = [0u8; SIGNATURE_LEN];
    for (slot, byte) in signature.iter_mut().zip(r_point.iter().chain(s.iter())) {
        *slot = *byte;
    }
    Some(signature)
}

fn base_mul(scalar: &[u8; 32]) -> Option<[u8; 32]> {
    Some(Point::decompress(&BASE_POINT)?.scalar_mul(scalar).compress())
}

/// `(k * s + r) mod L`, the scalar half of a signature.
///
/// Built from an EXACT 512-bit product plus the reduction `verify` already
/// uses, rather than a bespoke modular multiply: `mul_wide` does no reduction
/// at all, so it has no modulus to get wrong, and `reduce_wide` is the same
/// function that reduces every challenge hash on the verifying side. What is
/// left is a single addition of two values already below L, whose sum is below
/// 2L and so needs exactly one conditional subtraction.
fn muladd(k: &[u8; 32], s: &[u8; 32], r: &[u8; 32]) -> [u8; 32] {
    let ks = reduce_wide(&mul_wide(k, s));
    add_mod_l(&ks, r)
}

fn add_mod_l(a: &[u8; 32], b: &[u8; 32]) -> [u8; 32] {
    let (x, y) = (load_words(a), load_words(b));
    let mut sum = [0u64; 4];
    let mut carry = 0u64;
    for ((slot, xi), yi) in sum.iter_mut().zip(x.iter()).zip(y.iter()) {
        let (partial, first) = xi.overflowing_add(*yi);
        let (total, second) = partial.overflowing_add(carry);
        *slot = total;
        carry = u64::from(first) | u64::from(second);
    }
    // Both inputs are below L (< 2^253), so the sum is below 2L and cannot
    // overflow 256 bits; `carry` is therefore always zero here and one
    // conditional subtraction is enough.
    if at_least_l(sum) {
        sum = sub_l(sum);
    }
    store_words(sum)
}

/// The exact 512-bit product of two 256-bit little-endian values.
///
/// Byte-wise schoolbook rather than limb-wise: the per-step maximum is
/// `255 + 255*255 + 255 = 65535`, which is exactly `u16::MAX`, so no
/// intermediate can overflow and the bound is checkable by hand. A limbwise
/// version would need `u128` accumulation with its own carry discipline for no
/// gain — this runs once per signature in a test harness.
fn mul_wide(a: &[u8; 32], b: &[u8; 32]) -> [u8; 64] {
    let mut out = [0u8; 64];
    for (i, ai) in a.iter().enumerate() {
        let mut carry: u16 = 0;
        for (slot, bj) in out.iter_mut().skip(i).zip(b.iter()) {
            let acc = u16::from(*slot) + u16::from(*ai) * u16::from(*bj) + carry;
            let [low, high] = acc.to_le_bytes();
            *slot = low;
            carry = u16::from(high);
        }
        for slot in out.iter_mut().skip(i.saturating_add(32)) {
            if carry == 0 {
                break;
            }
            let acc = u16::from(*slot) + carry;
            let [low, high] = acc.to_le_bytes();
            *slot = low;
            carry = u16::from(high);
        }
        // The loop above exits either on a zero carry or on the end of `out`,
        // and the second would DROP one. It cannot happen — both inputs are
        // below 2^256, so the product is below 2^512 and 64 bytes hold it — but
        // that is a hand proof about a buffer bound, which is the shape of thing
        // worth having the debug build check rather than only a comment.
        debug_assert_eq!(carry, 0, "wide product carried out of 64 bytes");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ed25519;

    /// Whitespace is stripped so a long vector can be written across lines.
    /// The ODD-LENGTH assertion is the point of having one decoder rather than
    /// two: `chunks_exact(2)` drops a trailing nibble in silence, which turns a
    /// mistyped literal into a different message rather than into a failure.
    fn hex_bytes(s: &str) -> Vec<u8> {
        let cleaned: String = s.chars().filter(|c| !c.is_whitespace()).collect();
        let bytes = cleaned.as_bytes();
        assert!(bytes.len() % 2 == 0, "hex literal has an odd length");
        let mut out = vec![0u8; bytes.len() / 2];
        for (slot, pair) in out.iter_mut().zip(bytes.chunks_exact(2)) {
            let nibble = |c: u8| match c {
                b'0'..=b'9' => c - b'0',
                b'a'..=b'f' => c - b'a' + 10,
                _ => panic!("not lowercase hex"),
            };
            *slot = (nibble(pair[0]) << 4) | nibble(pair[1]);
        }
        out
    }

    fn from_hex<const N: usize>(s: &str) -> [u8; N] {
        let bytes = hex_bytes(s);
        assert_eq!(bytes.len(), N, "hex literal is the wrong length");
        let mut out = [0u8; N];
        out.copy_from_slice(&bytes);
        out
    }

    /// RFC 8032 §7.1, ALL FIVE of its pure-Ed25519 vectors. These are the
    /// standard's OWN, which is what makes them worth more than a round-trip: a
    /// signer and verifier that agree with each other can still both be wrong,
    /// and nothing in this crate would say so. Each names its secret, public
    /// key, message and expected signature.
    ///
    /// The count is asserted below rather than left to the array's length,
    /// because "the RFC's vectors" is a claim about the STANDARD and a shortened
    /// list still passes. The fifth is the one an eye skips — §7.1 names four of
    /// them TEST 1/2/3/1024 and the last "TEST SHA(abc)", whose message is
    /// SHA-512("abc") and so exercises a 64-byte message, one whole block.
    #[test]
    fn the_rfc_8032_vectors_are_reproduced_exactly() {
        // (seed, public key, message hex, signature)
        let vectors: [(&str, &str, &str, &str); 5] = [
            (
                "9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60",
                "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a",
                "",
                "e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e065224901555fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b",
            ),
            (
                "4ccd089b28ff96da9db6c346ec114e0f5b8a319f35aba624da8cf6ed4fb8a6fb",
                "3d4017c3e843895a92b70aa74d1b7ebc9c982ccf2ec4968cc0cd55f12af4660c",
                "72",
                "92a009a9f0d4cab8720e820b5f642540a2b27b5416503f8fb3762223ebdb69da085ac1e43e15996e458f3613d0f11d8c387b2eaeb4302aeeb00d291612bb0c00",
            ),
            (
                "c5aa8df43f9f837bedb7442f31dcb7b166d38535076f094b85ce3a2e0b4458f7",
                "fc51cd8e6218a1a38da47ed00230f0580816ed13ba3303ac5deb911548908025",
                "af82",
                "6291d657deec24024827e69c3abe01a30ce548a284743a445e3680d7db5ac3ac18ff9b538d16f290ae67f760984dc6594a7c15e9716ed28dc027beceea1ec40a",
            ),
            (
                "f5e5767cf153319517630f226876b86c8160cc583bc013744c6bf255f5cc0ee5",
                "278117fc144c72340f67d0f2316e8386ceffbf2b2428c9c51fef7c597f1d426e",
                "08b8b2b733424243760fe426a4b54908632110a66c2f6591eabd3345e3e4eb98\
                 fa6e264bf09efe12ee50f8f54e9f77b1e355f6c50544e23fb1433ddf73be84d8\
                 79de7c0046dc4996d9e773f4bc9efe5738829adb26c81b37c93a1b270b20329d\
                 658675fc6ea534e0810a4432826bf58c941efb65d57a338bbd2e26640f89ffbc\
                 1a858efcb8550ee3a5e1998bd177e93a7363c344fe6b199ee5d02e82d522c4fe\
                 ba15452f80288a821a579116ec6dad2b3b310da903401aa62100ab5d1a36553e\
                 06203b33890cc9b832f79ef80560ccb9a39ce767967ed628c6ad573cb116dbef\
                 efd75499da96bd68a8a97b928a8bbc103b6621fcde2beca1231d206be6cd9ec7\
                 aff6f6c94fcd7204ed3455c68c83f4a41da4af2b74ef5c53f1d8ac70bdcb7ed1\
                 85ce81bd84359d44254d95629e9855a94a7c1958d1f8ada5d0532ed8a5aa3fb2\
                 d17ba70eb6248e594e1a2297acbbb39d502f1a8c6eb6f1ce22b3de1a1f40cc24\
                 554119a831a9aad6079cad88425de6bde1a9187ebb6092cf67bf2b13fd65f270\
                 88d78b7e883c8759d2c4f5c65adb7553878ad575f9fad878e80a0c9ba63bcbcc\
                 2732e69485bbc9c90bfbd62481d9089beccf80cfe2df16a2cf65bd92dd597b07\
                 07e0917af48bbb75fed413d238f5555a7a569d80c3414a8d0859dc65a46128ba\
                 b27af87a71314f318c782b23ebfe808b82b0ce26401d2e22f04d83d1255dc51a\
                 ddd3b75a2b1ae0784504df543af8969be3ea7082ff7fc9888c144da2af58429e\
                 c96031dbcad3dad9af0dcbaaaf268cb8fcffead94f3c7ca495e056a9b47acdb7\
                 51fb73e666c6c655ade8297297d07ad1ba5e43f1bca32301651339e22904cc8c\
                 42f58c30c04aafdb038dda0847dd988dcda6f3bfd15c4b4c4525004aa06eeff8\
                 ca61783aacec57fb3d1f92b0fe2fd1a85f6724517b65e614ad6808d6f6ee34df\
                 f7310fdc82aebfd904b01e1dc54b2927094b2db68d6f903b68401adebf5a7e08\
                 d78ff4ef5d63653a65040cf9bfd4aca7984a74d37145986780fc0b16ac451649\
                 de6188a7dbdf191f64b5fc5e2ab47b57f7f7276cd419c17a3ca8e1b939ae49e4\
                 88acba6b965610b5480109c8b17b80e1b7b750dfc7598d5d5011fd2dcc5600a3\
                 2ef5b52a1ecc820e308aa342721aac0943bf6686b64b2579376504ccc493d97e\
                 6aed3fb0f9cd71a43dd497f01f17c0e2cb3797aa2a2f256656168e6c496afc5f\
                 b93246f6b1116398a346f1a641f3b041e989f7914f90cc2c7fff357876e506b5\
                 0d334ba77c225bc307ba537152f3f1610e4eafe595f6d9d90d11faa933a15ef1\
                 369546868a7f3a45a96768d40fd9d03412c091c6315cf4fde7cb68606937380d\
                 b2eaaa707b4c4185c32eddcdd306705e4dc1ffc872eeee475a64dfac86aba41c\
                 0618983f8741c5ef68d3a101e8a3b8cac60c905c15fc910840b94c00a0b9d0",
                "0aab4c900501b3e24d7cdf4663326a3a87df5e4843b2cbdb67cbf6e460fec350\
                 aa5371b1508f9f4528ecea23c436d94b5e8fcd4f681e30a6ac00a9704a188a03",
            ),
            // TEST SHA(abc): the message is SHA-512("abc"), asserted below
            // against this crate's own hash rather than taken on trust.
            (
                "833fe62409237b9d62ec77587520911e9a759cec1d19755b7da901b96dca3d42",
                "ec172b93ad5e563bf4932c70e1245034c35467ef2efd4d64ebf819683467e2bf",
                "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a\
                 2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f",
                "dc2a4459e7369633a52b1bf277839a00201009a3efbf3ecb69bea2186c26b589\
                 09351fc9ac90b3ecfdfbc7c66431e0303dca179c138ac17ad9bef1177331a704",
            ),
        ];
        assert_eq!(vectors.len(), 5, "RFC 8032 §7.1 has five Ed25519 vectors");
        for (i, (seed, pubkey, message, signature)) in vectors.iter().enumerate() {
            let seed: [u8; 32] = from_hex(seed);
            let expected_pub: [u8; 32] = from_hex(pubkey);
            let expected_sig: [u8; 64] = from_hex(signature);
            let msg = hex_bytes(message);
            assert_eq!(
                public_key(&seed).expect("the base point decompresses"),
                expected_pub,
                "vector {i}: public key"
            );
            let produced = sign(&seed, &msg).expect("the base point decompresses");
            assert_eq!(produced, expected_sig, "vector {i}: signature");
            assert!(
                ed25519::verify(&expected_pub, &msg, &produced),
                "vector {i}: own verifier must accept the RFC's signature"
            );
        }

        // The last vector's message names its own derivation, so it is checked
        // rather than copied: a transcription slip there would otherwise be a
        // silently different 64-byte message.
        let mut hasher = sha512::Sha512::new();
        hasher.update(b"abc");
        let digest = hasher.finalize();
        let last = vectors.last().expect("five vectors");
        assert_eq!(hex_bytes(last.2), digest.to_vec(), "TEST SHA(abc)'s message");
    }

    /// The round-trip, over seeds and message lengths the vectors do not cover
    /// — including the boundaries where SHA-512's padding spills into another
    /// block, which is where a length-handling bug would hide.
    #[test]
    fn what_this_signs_the_engine_verifier_accepts() {
        for seed_byte in [0u8, 1, 0x7f, 0x80, 0xff] {
            let seed = [seed_byte; 32];
            let a = public_key(&seed).expect("base point");
            for len in [0usize, 1, 31, 32, 55, 56, 63, 64, 111, 112, 127, 128, 200] {
                let message: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
                let sig = sign(&seed, &message).expect("base point");
                assert!(
                    ed25519::verify(&a, &message, &sig),
                    "seed {seed_byte:#04x}, message length {len}"
                );
            }
        }
    }

    /// The rejection half. An acceptance-only test passes against a verifier
    /// that returns true unconditionally, and against a signer that emits a
    /// constant.
    #[test]
    fn a_signature_stops_verifying_when_anything_moves() {
        let seed = [7u8; 32];
        let (a, message) = (public_key(&seed).expect("base point"), b"td-deployment-v1\n");
        let sig = sign(&seed, message).expect("base point");
        assert!(ed25519::verify(&a, message, &sig));

        let other = public_key(&[8u8; 32]).expect("base point");
        assert!(!ed25519::verify(&other, message, &sig), "a different key");
        assert!(!ed25519::verify(&a, b"td-deployment-v2\n", &sig), "a changed message");
        for i in 0..64 {
            let mut broken = sig;
            if let Some(byte) = broken.get_mut(i) {
                *byte ^= 1;
            }
            assert!(!ed25519::verify(&a, message, &broken), "signature byte {i}");
        }
        for i in 0..32 {
            let mut broken = a;
            if let Some(byte) = broken.get_mut(i) {
                *byte ^= 1;
            }
            assert!(!ed25519::verify(&broken, message, &sig), "public key byte {i}");
        }
    }

    /// Determinism is the whole of RFC 8032's nonce policy: the same seed and
    /// message must give the same signature every time, because a nonce that
    /// varied would be an RNG this code does not have, and one that REPEATED
    /// across different messages leaks the private key outright.
    ///
    /// The reuse half is asserted over `R` ALONE and not over the signature,
    /// which is the difference between testing the property and testing
    /// nothing: `S` covers the message through `k = H(R || A || M)`, so two
    /// signatures differ whenever the messages do — even from a signer whose
    /// nonce is a constant. `R = [r]B` is the only part that moves if and only
    /// if the nonce does.
    #[test]
    fn signing_is_deterministic_and_the_nonce_is_message_dependent() {
        let r_of = |sig: &[u8; SIGNATURE_LEN]| {
            let mut r = [0u8; 32];
            r.copy_from_slice(sig.get(..32).expect("a signature is R || S"));
            r
        };

        let seed = [3u8; 32];
        let first = sign(&seed, b"a").expect("base point");
        assert_eq!(first, sign(&seed, b"a").expect("base point"), "same input");

        let other_message = sign(&seed, b"b").expect("base point");
        assert_ne!(
            r_of(&first),
            r_of(&other_message),
            "a different message must not reuse the nonce"
        );
        // Length alone must move it too, not just content: `r = H(prefix || M)`
        // and a signer that hashed a fixed-width digest of the message instead
        // would pass the check above.
        assert_ne!(
            r_of(&first),
            r_of(&sign(&seed, b"aa").expect("base point")),
            "a longer message must not reuse the nonce"
        );
        assert_ne!(
            r_of(&first),
            r_of(&sign(&[4u8; 32], b"a").expect("base point")),
            "a different seed must not reuse the nonce"
        );
    }

    /// `mul_wide` claims to be an EXACT product, so it is checked against one
    /// computed a different way rather than against itself.
    #[test]
    fn the_wide_product_is_exact() {
        // 1 * x == x, zero-extended.
        let mut one = [0u8; 32];
        if let Some(first) = one.first_mut() {
            *first = 1;
        }
        let x: [u8; 32] = from_hex("0123456789abcdeffedcba98765432100f1e2d3c4b5a69788796a5b4c3d2e1f0");
        let product = mul_wide(&one, &x);
        assert_eq!(product.get(..32), Some(&x[..]), "low half is x");
        assert!(product.get(32..).is_some_and(|hi| hi.iter().all(|b| *b == 0)));

        // The largest inputs: (2^256 - 1)^2 = 2^512 - 2^257 + 1, whose bytes are
        // 01 then thirty-one 00 then fe then thirty-one ff.
        let max = [0xffu8; 32];
        let squared = mul_wide(&max, &max);
        let mut expected = [0u8; 64];
        if let Some(first) = expected.first_mut() {
            *first = 1;
        }
        if let Some(slot) = expected.get_mut(32) {
            *slot = 0xfe;
        }
        for slot in expected.iter_mut().skip(33) {
            *slot = 0xff;
        }
        assert_eq!(squared, expected, "(2^256 - 1)^2");
    }
}

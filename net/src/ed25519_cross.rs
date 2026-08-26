// Cross-implementation agreement between the two halves of td's signing story:
// `ring` SIGNS (host-side, here and in subst.rs's narinfo path) and the
// engine's dependency-free `ed25519::verify` — the one compiled into td-boot,
// where no external crate may go — VERIFIES.
//
// This is the check that a fixed vector cannot make. Keys and messages are
// fresh every run, so agreement is asserted over a distribution rather than
// over one point somebody once recorded; a verifier that mishandles, say, a
// point whose x is zero or a scalar with a rare bit pattern is found here and
// not in production.
//
// WHERE IT RUNS, stated exactly: `cargo test --frozen --workspace` does not
// reach net (it is excluded from the engine workspace), but the affected-checks
// `net-test` preflight names net/Cargo.toml for net and td-engine changes. That
// deliberately puts ring's host C build in this security boundary's check path.
// A developer can run the same suite directly with
//
//     CC=<cc> cargo test --manifest-path net/Cargo.toml
//
// Target gates carry the RESULT too: the signatures a run of this file
// produced are committed as fixtures in ed25519.rs, whose tests do run there.
// That covers the direction which can actually rot — a change to the engine
// verifier — and leaves uncovered only ring changing what it signs, which
// would be a bug in ring.
//
// The rejection half matters as much as the acceptance half: a verifier that
// returns true unconditionally passes every acceptance test ever written.

use ring::rand::SystemRandom;
use ring::signature::{Ed25519KeyPair, KeyPair};

/// L = 2^252 + 27742317777372353535851937790883648493, least-significant word
/// first — the group order, needed to build the malleability case below.
const L_WORDS: [u64; 4] = [
    0x5812_631a_5cf5_d3ed,
    0x14de_f9de_a2f7_9cd6,
    0x0000_0000_0000_0000,
    0x1000_0000_0000_0000,
];

struct Signer {
    key: Ed25519KeyPair,
    public: [u8; 32],
}

impl Signer {
    fn new() -> Signer {
        let rng = SystemRandom::new();
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng).expect("generate ed25519 key");
        let key = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).expect("parse generated key");
        let public: [u8; 32] = key
            .public_key()
            .as_ref()
            .try_into()
            .expect("ed25519 public keys are 32 bytes");
        Signer { key, public }
    }

    fn sign(&self, message: &[u8]) -> [u8; 64] {
        self.key
            .sign(message)
            .as_ref()
            .try_into()
            .expect("ed25519 signatures are 64 bytes")
    }
}

fn message(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

/// s += L, as a 256-bit little-endian integer. S < L, so this cannot carry out
/// of 32 bytes.
fn add_l(scalar: &mut [u8; 32]) {
    let mut carry = 0u128;
    for (chunk, l) in scalar.chunks_mut(8).zip(L_WORDS.iter()) {
        let mut word = [0u8; 8];
        word.copy_from_slice(chunk);
        let sum = u128::from(u64::from_le_bytes(word)) + u128::from(*l) + carry;
        chunk.copy_from_slice(&(sum as u64).to_le_bytes());
        carry = sum >> 64;
    }
}

#[test]
fn the_engine_verifier_accepts_what_ring_signs() {
    // Lengths that bracket SHA-512's block and padding boundaries, since the
    // hash is where a message length can change the answer.
    for len in [0usize, 1, 31, 32, 55, 64, 111, 112, 128, 1000] {
        let signer = Signer::new();
        let msg = message(len);
        let sig = signer.sign(&msg);
        assert!(
            crate::ed25519::verify(&signer.public, &msg, &sig),
            "engine rejected a valid ring signature over {len} bytes"
        );
    }
}

#[test]
fn every_single_bit_of_a_signature_matters() {
    // Not a sample: EVERY bit of R and S is flipped in turn and each must
    // break the signature. This is what catches a verifier that only looks at
    // part of what it was handed.
    let signer = Signer::new();
    let msg = message(64);
    let sig = signer.sign(&msg);
    for byte in 0..64 {
        for bit in 0..8 {
            let mut broken = sig;
            if let Some(slot) = broken.get_mut(byte) {
                *slot ^= 1 << bit;
            }
            assert!(
                !crate::ed25519::verify(&signer.public, &msg, &broken),
                "engine accepted a signature with byte {byte} bit {bit} flipped"
            );
        }
    }
}

#[test]
fn every_single_bit_of_the_public_key_matters() {
    let signer = Signer::new();
    let msg = message(32);
    let sig = signer.sign(&msg);
    for byte in 0..32 {
        for bit in 0..8 {
            let mut other = signer.public;
            if let Some(slot) = other.get_mut(byte) {
                *slot ^= 1 << bit;
            }
            assert!(
                !crate::ed25519::verify(&other, &msg, &sig),
                "engine accepted under a key with byte {byte} bit {bit} flipped"
            );
        }
    }
}

#[test]
fn a_changed_message_breaks_the_signature() {
    let signer = Signer::new();
    let msg = message(96);
    let sig = signer.sign(&msg);
    for index in [0usize, 1, 47, 94, 95] {
        let mut other = msg.clone();
        if let Some(slot) = other.get_mut(index) {
            *slot ^= 0x01;
        }
        assert!(
            !crate::ed25519::verify(&signer.public, &other, &sig),
            "engine accepted a signature over a message changed at {index}"
        );
    }
    // Truncation and extension are the two length changes a byte flip misses.
    assert!(!crate::ed25519::verify(
        &signer.public,
        msg.get(..95).unwrap_or_default(),
        &sig
    ));
    let mut longer = msg.clone();
    longer.push(0);
    assert!(!crate::ed25519::verify(&signer.public, &longer, &sig));
}

#[test]
fn another_signers_key_does_not_verify() {
    let signer = Signer::new();
    let other = Signer::new();
    let msg = message(48);
    let sig = signer.sign(&msg);
    assert!(crate::ed25519::verify(&signer.public, &msg, &sig));
    assert!(!crate::ed25519::verify(&other.public, &msg, &sig));
}

#[test]
fn s_plus_l_is_not_a_second_valid_signature() {
    // The malleability case the S < L check exists for: (S + L) mod L is S, so
    // a verifier that reduces instead of REFUSING accepts a signature nobody
    // with the private key ever made — two distinct valid signatures for one
    // message, which anything using a signature as an identity gets wrong.
    let signer = Signer::new();
    let msg = message(17);
    let sig = signer.sign(&msg);
    assert!(crate::ed25519::verify(&signer.public, &msg, &sig));

    let mut malleable = sig;
    let mut s: [u8; 32] = malleable
        .get(32..64)
        .and_then(|s| s.try_into().ok())
        .expect("a signature has 64 bytes");
    add_l(&mut s);
    if let Some(dst) = malleable.get_mut(32..64) {
        dst.copy_from_slice(&s);
    }
    assert_ne!(malleable, sig, "adding L must change the encoding");
    assert!(
        !crate::ed25519::verify(&signer.public, &msg, &malleable),
        "engine accepted S + L: the canonical-scalar check is not load-bearing"
    );
}

#[test]
fn a_small_order_public_key_is_refused() {
    // y = -1, the order-2 point, and the identity. Keys like these verify
    // signatures nobody needed a private key to produce, so they are refused
    // before the equation is evaluated.
    //
    // This is a smoke check only: the signature here is rejected by the
    // equation as well, so it does not on its own show the refusal is
    // load-bearing. The test that DOES — it builds a signature that verifies
    // for every message under such a key — needs the group internals and so
    // lives in `ed25519.rs` itself, as
    // `a_small_order_key_cannot_sign_for_everything`.
    let mut order_two = [0xffu8; 32];
    if let Some(slot) = order_two.first_mut() {
        *slot = 0xec;
    }
    if let Some(slot) = order_two.last_mut() {
        *slot = 0x7f;
    }
    assert!(!crate::ed25519::verify(&order_two, b"anything", &[0u8; 64]));
    // The all-zero key is the identity point, likewise small order.
    assert!(!crate::ed25519::verify(&[0u8; 32], b"anything", &[0u8; 64]));
}

// ---- the engine's own SIGNER, against ring ----
//
// `engine/src/ed25519_sign.rs` exists for the recipe-check oracle, which must
// sign a per-build manifest and cannot reach `ring` (the check crate carries no
// external dependency) or `td-deploy` (no recipe builds td-net). It is
// hand-rolled, so it is checked against an independent implementation here in
// BOTH directions — the RFC 8032 vectors in the module itself pin it against
// the standard, and these pin it against a real one.
//
// This is the stronger of the two directions to get wrong quietly. A signer
// that agreed with td's own verifier and with nothing else would pass every
// test in the engine and still produce signatures no other implementation
// accepts.

/// The seed `ring` derives a key from is the same 32 bytes the engine signer
/// takes, so the two can be pointed at one key and compared byte for byte.
fn ring_keypair(seed: &[u8; 32]) -> Ed25519KeyPair {
    Ed25519KeyPair::from_seed_unchecked(seed).expect("a 32-byte seed is a valid ed25519 key")
}

/// A random seed makes a failure unreproducible unless the failure says which
/// seed. These are throwaway test keys generated in-process, so printing one is
/// not a disclosure — and without it a red here could not be re-run.
fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Fresh seeds, for this file's stated reason — agreement over a distribution
/// rather than over points somebody once recorded — plus the three extremes a
/// random draw will never produce, since those are where a clamp or a carry
/// goes wrong. The signer is the side with the NEW hand-rolled arithmetic, so
/// it wants the distribution at least as much as the verifier does.
fn cross_seeds() -> Vec<[u8; 32]> {
    use ring::rand::SecureRandom;
    let rng = SystemRandom::new();
    let mut seeds = vec![[0u8; 32], [0xffu8; 32], {
        let mut counting = [0u8; 32];
        for (i, slot) in counting.iter_mut().enumerate() {
            *slot = i as u8;
        }
        counting
    }];
    for _ in 0..8 {
        let mut seed = [0u8; 32];
        rng.fill(&mut seed).expect("system randomness");
        seeds.push(seed);
    }
    seeds
}

#[test]
fn the_engine_signer_produces_exactly_what_ring_produces() {
    // ed25519 signing is deterministic (RFC 8032 derives the nonce from the
    // key and message), so this is byte equality and not merely "both verify".
    for (i, seed) in cross_seeds().iter().enumerate() {
        let seed = *seed;
        let ring_key = ring_keypair(&seed);
        let ring_public: [u8; 32] = ring_key
            .public_key()
            .as_ref()
            .try_into()
            .expect("ed25519 public keys are 32 bytes");
        assert_eq!(
            crate::ed25519_sign::public_key(&seed).expect("the base point decompresses"),
            ring_public,
            "public key for seed {i} ({})", hex(&seed)
        );
        for len in [0usize, 1, 32, 63, 64, 65, 127, 128, 300] {
            let msg = message(len);
            let ring_sig: [u8; 64] = ring_key
                .sign(&msg)
                .as_ref()
                .try_into()
                .expect("ed25519 signatures are 64 bytes");
            let engine_sig =
                crate::ed25519_sign::sign(&seed, &msg).expect("the base point decompresses");
            assert_eq!(
                engine_sig, ring_sig,
                "signature for seed {i} ({}), message length {len}", hex(&seed)
            );
        }
    }
}

#[test]
fn ring_accepts_what_the_engine_signer_produces() {
    use ring::signature::{UnparsedPublicKey, ED25519};
    // The direction byte-equality above already implies, asserted separately
    // because it is the property that actually matters and would survive a
    // future change to either side's encoding conventions.
    for seed in cross_seeds() {
        let public =
            crate::ed25519_sign::public_key(&seed).expect("the base point decompresses");
        for len in [0usize, 17, 64, 129] {
            let msg = message(len);
            let sig = crate::ed25519_sign::sign(&seed, &msg).expect("base point");
            assert!(
                UnparsedPublicKey::new(&ED25519, &public[..])
                    .verify(&msg, &sig[..])
                    .is_ok(),
                "ring must accept the engine signature for seed {} length {len}",
                hex(&seed)
            );
            // And the rejection half, so this cannot pass against a verifier
            // that accepts everything.
            let mut broken = sig;
            if let Some(byte) = broken.first_mut() {
                *byte ^= 1;
            }
            assert!(
                UnparsedPublicKey::new(&ED25519, &public[..])
                    .verify(&msg, &broken[..])
                    .is_err(),
                "ring must reject a mangled engine signature"
            );
        }
    }
}

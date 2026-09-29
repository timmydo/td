#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;
use crate::{Digest, SystemEntropy};

fn hex(text: &str) -> Vec<u8> {
    text.as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}

fn scalar_one() -> [u8; 32] {
    let mut scalar = [0; 32];
    scalar[31] = 1;
    scalar
}

// Existing td-secret P-256 generator coordinates, with private scalar one.
fn generator() -> [u8; 65] {
    hex(concat!(
        "04",
        "6b17d1f2e12c4247f8bce6e563a440f277037d812deb33a0f4a13945d898c296",
        "4fe342e2fe1a7f9b8ee7eb4a7c0f9e162bce33576b315ececbb6406837bf51f5"
    ))
    .try_into()
    .unwrap()
}

fn tlv(tag: u8, bytes: &[u8]) -> Vec<u8> {
    let mut output = vec![tag];
    if bytes.len() >= 128 {
        output.push(0x81);
    }
    output.push(u8::try_from(bytes.len()).unwrap());
    output.extend_from_slice(bytes);
    output
}

fn wrap(inner: &[u8]) -> Vec<u8> {
    let mut info = hex("020100301306072a8648ce3d020106082a8648ce3d030107");
    info.extend(tlv(0x04, inner));
    tlv(0x30, &info)
}

fn fixture(scalar: &[u8], parameters: bool, public: Option<&[u8]>) -> Vec<u8> {
    let mut key = hex("020101");
    key.extend(tlv(0x04, scalar));
    if parameters {
        key.extend(hex("a00a06082a8648ce3d030107"));
    }
    if let Some(public) = public {
        let mut bits = vec![0];
        bits.extend_from_slice(public);
        key.extend(tlv(0xa1, &tlv(0x03, &bits)));
    }
    wrap(&tlv(0x30, &key))
}

fn load_fixture() -> P256Key {
    Provider
        .load_p256(&fixture(&scalar_one(), false, Some(&generator())))
        .unwrap()
}

fn verify(public: &[u8; 65], message: &[u8], signature: &[u8; 64]) -> bool {
    let public = crate::p256_oracle::PublicKey::from_coordinates(
        public[1..33].try_into().unwrap(),
        public[33..].try_into().unwrap(),
    )
    .unwrap();
    let mut hash = crate::sha256_oracle::Sha256::new();
    hash.update(message);
    public
        .verify(
            &hash.finalize(),
            signature[..32].try_into().unwrap(),
            signature[32..].try_into().unwrap(),
        )
        .is_ok()
}

#[test]
fn factory_digest_and_fixed_comparison() {
    let mut digest = Provider.sha256().unwrap();
    digest.update(b"abc").unwrap();
    assert_eq!(
        digest.finish().unwrap().as_slice(),
        hex("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
    );
    for left in [[0; 32], [0xa5; 32], [0xff; 32]] {
        assert!(Provider.equal_digest(&left, &left));
        for offset in 0..32 {
            for bit in 0..8 {
                let mut right = left;
                right[offset] ^= 1 << bit;
                assert!(!Provider.equal_digest(&left, &right));
                assert!(!Provider.equal_digest(&right, &left));
            }
        }
    }
}

#[test]
fn accepted_pkcs8_variants_and_public_point_known_answer() {
    let public = generator();
    for (parameters, included, length) in [
        (false, false, 67),
        (true, false, 79),
        (false, true, 138),
        (true, true, 150),
    ] {
        let bytes = fixture(
            &scalar_one(),
            parameters,
            included.then_some(public.as_slice()),
        );
        assert_eq!(bytes.len(), length);
        let key = Provider.load_p256(&bytes).unwrap();
        let mut output = [0; 65];
        Provider.p256_public(&key, &mut output).unwrap();
        assert_eq!(output, public);
        assert_eq!(format!("{key:?}"), "P256Key(<redacted>)");
        for end in 0..bytes.len() {
            assert!(
                matches!(Provider.load_p256(&bytes[..end]), Err(Error::Invalid)),
                "length={length} end={end}"
            );
        }
        let mut trailing = bytes;
        trailing.push(0);
        assert!(matches!(Provider.load_p256(&trailing), Err(Error::Invalid)));
    }
    // Independent literal encoding of the smallest accepted scalar-one fixture.
    assert_eq!(
        fixture(&scalar_one(), false, None),
        hex(concat!(
            "3041020100301306072a8648ce3d020106082a8648ce3d0301070427",
            "30250201010420",
            "0000000000000000000000000000000000000000000000000000000000000001"
        ))
    );
}

#[test]
fn malformed_der_and_inconsistent_keys_are_refused() {
    let small = fixture(&scalar_one(), false, None);
    let full = fixture(&scalar_one(), true, Some(&generator()));
    let mut refused = vec![vec![], vec![0; P256_PKCS8_CAPACITY + 1]];
    refused.push([small.clone(), small.clone()].concat());
    for (offset, value) in [(0, 0x31), (1, 0x80), (2, 0x03), (4, 1), (23, 8)] {
        let mut bytes = small.clone();
        bytes[offset] = value;
        refused.push(bytes);
    }
    let mut overlong = small.clone();
    overlong.splice(1..2, [0x81, 65]);
    refused.push(overlong);
    let mut overlong = full.clone();
    overlong.splice(1..3, [0x82, 0, 147]);
    refused.push(overlong);
    let mut overlong = small.clone();
    overlong.splice(1..2, [0x82, 0, 65]);
    refused.push(overlong);
    for scalar in [
        vec![],
        vec![1],
        vec![1; 31],
        vec![1; 33],
        [&[0][..], &scalar_one()].concat(),
        vec![0; 32],
        hex("ffffffff00000000ffffffffffffffffbce6faada7179e84f3b9cac2fc632551"),
        vec![0xff; 32],
    ] {
        refused.push(fixture(&scalar, false, None));
    }
    let mut two = scalar_one();
    two[31] = 2;
    refused.push(fixture(&two, false, Some(&generator())));
    let mut off_curve = generator();
    off_curve[64] ^= 1;
    refused.push(fixture(&scalar_one(), false, Some(&off_curve)));
    refused.push(fixture(&scalar_one(), false, Some(&generator()[..33])));
    let mut compressed = generator()[..33].to_vec();
    compressed[0] = 3;
    refused.push(fixture(&scalar_one(), false, Some(&compressed)));
    let mut hybrid = generator();
    hybrid[0] = 7;
    refused.push(fixture(&scalar_one(), false, Some(&hybrid)));
    let mut infinity = [0; 65];
    infinity[0] = 4;
    refused.push(fixture(&scalar_one(), false, Some(&infinity)));
    let mut inner = hex("020101");
    inner.extend(tlv(0x04, &scalar_one()));
    for extra in [
        hex("a000"),
        hex("a00a06082a8648ce3d030108"),
        hex("a00a06082a8648ce3d030107a00a06082a8648ce3d030107"),
        vec![0],
    ] {
        refused.push(wrap(&tlv(0x30, &[inner.clone(), extra].concat())));
    }
    let mut bad_padding = full.clone();
    let at = bad_padding.len() - 66;
    bad_padding[at] = 1;
    refused.push(bad_padding);
    let mut public = vec![0];
    public.extend(generator());
    let public = tlv(0xa1, &tlv(0x03, &public));
    let parameters = hex("a00a06082a8648ce3d030107");
    refused.push(wrap(&tlv(
        0x30,
        &[inner.clone(), public, parameters].concat(),
    )));
    refused.push(wrap(&[tlv(0x30, &inner), vec![0]].concat()));
    // Attributes are outside the accepted PKCS#8 subset.
    let mut attributes = small.clone();
    attributes[1] += 2;
    attributes.extend([0xa0, 0]);
    refused.push(attributes);
    for (case, bytes) in refused.iter().enumerate() {
        assert!(
            matches!(Provider.load_p256(bytes), Err(Error::Invalid)),
            "case={case}"
        );
    }
}

#[test]
fn generated_keys_and_signatures_pass_independent_verification() {
    let _entropy = SystemEntropy::try_new().unwrap();
    for message in [b"".as_slice(), b"abc", &[0xa5; 65], &[0x42; 1024]] {
        let mut encoded = [0xa5; P256_PKCS8_CAPACITY + 8];
        let length = Provider.generate_p256(&mut encoded).unwrap();
        assert_eq!(length, 138);
        assert!(encoded[length..].iter().all(|&byte| byte == 0xa5));
        let key = Provider.load_p256(&encoded[..length]).unwrap();
        let mut public = [0; 65];
        Provider.p256_public(&key, &mut public).unwrap();
        let mut signature = [0; 64];
        Provider.sign_es256(&key, message, &mut signature).unwrap();
        assert!(verify(&public, message, &signature));
        let mut changed = message.to_vec();
        changed.push(0);
        assert!(!verify(&public, &changed, &signature));
        assert!(!verify(&public, message, &[0; 64]));
        let mut hash = crate::sha256_oracle::Sha256::new();
        hash.update(message);
        assert!(!verify(&public, &hash.finalize(), &signature));
    }
    let key = load_fixture();
    let mut signature = [0; 64];
    Provider
        .sign_es256(&key, b"known public key", &mut signature)
        .unwrap();
    assert!(verify(&generator(), b"known public key", &signature));
}

#[test]
fn capacity_failure_and_retired_keys_preserve_caller_output() {
    for length in [0, 1, P256_PKCS8_CAPACITY - 1] {
        let mut output = vec![0xa5; length];
        assert_eq!(Provider.generate_p256(&mut output), Err(Error::Capacity));
        assert_eq!(
            generate(&mut output, || panic!("must refuse before generation")),
            Err(Error::Capacity)
        );
        assert_eq!(output, vec![0xa5; length]);
    }
    for unwind in [false, true] {
        let mut output = [0xa5; P256_PKCS8_CAPACITY];
        assert_eq!(
            generate(&mut output, || {
                if unwind {
                    panic!("fixture generation unwind");
                }
                Err(Error::Crypto)
            }),
            Err(Error::Crypto)
        );
        assert_eq!(output, [0xa5; P256_PKCS8_CAPACITY]);
        let key = load_fixture();
        assert_eq!(
            key.operate::<()>(|_| {
                if unwind {
                    panic!("fixture provider unwind");
                }
                Err(Error::Crypto)
            }),
            Err(Error::Crypto)
        );
        assert!(key.key.lock().unwrap().is_none());
        let mut public = [0xa5; 65];
        let mut signature = [0xa5; 64];
        assert_eq!(Provider.p256_public(&key, &mut public), Err(Error::Crypto));
        assert_eq!(
            Provider.sign_es256(&key, b"retry", &mut signature),
            Err(Error::Crypto)
        );
        assert_eq!(public, [0xa5; 65]);
        assert_eq!(signature, [0xa5; 64]);
    }
    assert_eq!(
        provider::<()>(|| panic!("fixture constructor unwind")),
        Err(Error::Crypto)
    );
    let key = load_fixture();
    let _ = std::panic::catch_unwind(|| {
        let _held = key.key.lock().unwrap();
        panic!("fixture lock poison");
    });
    assert_eq!(key.operate(|_| Ok(())), Err(Error::Crypto));
    assert!(key.key.lock().unwrap_err().into_inner().is_none());
}

#[test]
fn shared_key_serializes_success_and_terminal_failure() {
    let key = load_fixture();
    std::thread::scope(|scope| {
        let mut workers = Vec::new();
        for message in [b"first".as_slice(), b"second", b"third"] {
            let key = &key;
            workers.push(scope.spawn(move || {
                let mut signature = [0; 64];
                Provider.sign_es256(key, message, &mut signature).unwrap();
                assert!(verify(&generator(), message, &signature));
            }));
        }
        for worker in workers {
            worker.join().unwrap();
        }
    });
    key.operate::<()>(|_| Err(Error::Crypto)).unwrap_err();
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                let mut output = [0xa5; 64];
                assert_eq!(
                    Provider.sign_es256(&key, b"after failure", &mut output),
                    Err(Error::Crypto)
                );
                assert_eq!(output, [0xa5; 64]);
            })
            .join()
            .unwrap();
    });
}

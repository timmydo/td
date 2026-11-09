//! Known answers for the td-primitives backend that AWS-LC, the backend it
//! replaced, wrote or checked: persisted P-256 keys in both directions, its
//! signatures under every certificate algorithm it signs, and certificate
//! paths. AWS-LC is outside every closure now, so these fixtures, generated
//! while it was the backend, keep that compatibility pinned
//! (td-mta/IMPLEMENTATION.md F04).
#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use crate::certificate_fixtures::{self as f, oracle_verifies, unhex, SIGNATURE_ANSWERS};
use crate::{Crypto, Provider, P256_PKCS8_CAPACITY};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, SignatureVerificationAlgorithm, UnixTime};
use std::time::Duration;
use td_primitives::ec::p256;
use td_primitives::sha::{Digest, Sha256};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
const NOW: u64 = 1_800_000_000;
// The policy's verifier names, in td-crypto's certificate algorithm order.
const NAMES: &[&str] = &[
    "ECDSA_P256_SHA256",
    "ECDSA_P256_SHA384",
    "ECDSA_P256_SHA512",
    "ECDSA_P384_SHA256",
    "ECDSA_P384_SHA384",
    "ECDSA_P384_SHA512",
    "ECDSA_P521_SHA256",
    "ECDSA_P521_SHA384",
    "ECDSA_P521_SHA512",
    "ED25519",
    "RSA_PSS_SHA256",
    "RSA_PSS_SHA384",
    "RSA_PSS_SHA512",
    "RSA_PKCS1_SHA256",
    "RSA_PKCS1_SHA384",
    "RSA_PKCS1_SHA512",
    "RSA_PKCS1_SHA256_BARE",
    "RSA_PKCS1_SHA384_BARE",
    "RSA_PKCS1_SHA512_BARE",
];
/// Keys AWS-LC generated: PKCS#8, public point, message, AWS-LC's raw r || s
/// signature through this facade, and its DER signature.
const AWS_LC_KEYS: &str = include_str!("fixtures/aws_lc_keys.txt");
/// Keys written in this backend's generated shape, from td-primitives scalars,
/// with td-primitives' DER signatures. AWS-LC loaded each to the same public
/// point and verified each signature when they were written.
const TD_KEYS: &str = include_str!("fixtures/td_keys.txt");
/// AWS-LC's signatures under each certificate algorithm it signs.
const AWS_LC_SIGNATURES: &str = include_str!("fixtures/aws_lc_signatures.txt");

fn policy() -> CryptoProvider {
    crate::tls_policy::provider().unwrap()
}

fn compressed(point: &[u8]) -> Vec<u8> {
    let size = (point.len() - 1) / 2;
    let mut out = vec![2 | (point[point.len() - 1] & 1)];
    out.extend_from_slice(&point[1..=size]);
    out
}

fn rows(text: &str, tag: &str) -> Vec<Vec<Vec<u8>>> {
    text.lines()
        .filter_map(|line| line.strip_prefix(tag))
        .map(|line| line.split(' ').map(unhex).collect())
        .collect()
}

fn fixed_verifies(public: &[u8; 65], message: &[u8], fixed: &[u8; 64]) -> bool {
    let der = f::seq(&[der_integer(&fixed[..32]), der_integer(&fixed[32..])]);
    oracle_verifies(public, message, &der)
}

fn der_integer(bytes: &[u8]) -> Vec<u8> {
    let trimmed: Vec<u8> = bytes.iter().copied().skip_while(|b| *b == 0).collect();
    let mut body = Vec::new();
    if trimmed.first().is_some_and(|b| b & 0x80 != 0) {
        body.push(0);
    }
    body.extend_from_slice(&trimmed);
    f::der(2, &body)
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

/// td-crypto's four accepted shapes: optional inner curve, optional public.
fn shape(scalar: &[u8; 32], parameters: bool, public: Option<&[u8]>) -> Vec<u8> {
    let mut key = unhex("020101");
    key.extend(tlv(0x04, scalar));
    if parameters {
        key.extend(unhex("a00a06082a8648ce3d030107"));
    }
    if let Some(public) = public {
        let mut bits = vec![0];
        bits.extend_from_slice(public);
        key.extend(tlv(0xa1, &tlv(0x03, &bits)));
    }
    let mut info = unhex("020100301306072a8648ce3d020106082a8648ce3d030107");
    info.extend(tlv(0x04, &tlv(0x30, &key)));
    tlv(0x30, &info)
}

fn scalar(document: &[u8]) -> [u8; 32] {
    *crate::pkcs8::parse(document).unwrap().scalar
}

/// Keys AWS-LC wrote load here in all four accepted shapes, to its public
/// point; its signatures verify under td-fido's independent verifier and
/// td-primitives; this backend's signatures with them verify under the oracle.
#[test]
fn keys_aws_lc_wrote_load_and_sign_here() -> Result<()> {
    let keys = rows(AWS_LC_KEYS, "KEY ");
    assert_eq!(keys.len(), 4);
    for row in keys {
        let [document, public, message, fixed, der] = &row[..] else {
            panic!("malformed key row");
        };
        let public: [u8; 65] = public[..].try_into()?;
        let fixed: [u8; 64] = fixed[..].try_into()?;
        assert_eq!(document.len(), 138);
        assert!(fixed_verifies(&public, message, &fixed));
        assert!(oracle_verifies(&public, message, der));
        assert!(p256::verify(&public, &Sha256::digest(message), der).is_ok());
        let secret = scalar(document);
        for (parameters, included, length) in [
            (false, false, 67),
            (true, false, 79),
            (false, true, 138),
            (true, true, 150),
        ] {
            let bytes = shape(&secret, parameters, included.then_some(&public[..]));
            assert_eq!(bytes.len(), length);
            if length == 138 {
                assert_eq!(&bytes, document);
            }
            let key = Provider.load_p256(&bytes)?;
            let mut loaded = [0u8; 65];
            Provider.p256_public(&key, &mut loaded)?;
            assert_eq!(loaded, public, "{length}-byte shape");
            let mut signature = [0u8; 64];
            Provider.sign_es256(&key, message, &mut signature)?;
            assert!(fixed_verifies(&public, message, &signature));
            let mut changed = message.clone();
            changed.push(0);
            assert!(!fixed_verifies(&public, &changed, &signature));
        }
    }
    Ok(())
}

/// The other direction: documents in this backend's generated shape, which
/// AWS-LC loaded to the same points and whose td-primitives signatures it
/// verified, load here; and a document generated now has that same layout
/// around its scalar and point.
#[test]
fn keys_written_here_have_the_shape_aws_lc_loaded() -> Result<()> {
    let keys = rows(TD_KEYS, "KEY ");
    assert_eq!(keys.len(), 4);
    for row in keys {
        let [document, public, message, der] = &row[..] else {
            panic!("malformed key row");
        };
        let public: [u8; 65] = public[..].try_into()?;
        let key = Provider.load_p256(document)?;
        let mut loaded = [0u8; 65];
        Provider.p256_public(&key, &mut loaded)?;
        assert_eq!(loaded, public);
        assert!(oracle_verifies(&public, message, der));
        assert_eq!(
            &shape(&scalar(document), false, Some(&public[..])),
            document
        );
    }
    let mut generated = [0u8; P256_PKCS8_CAPACITY];
    let length = Provider.generate_p256(&mut generated)?;
    let generated = &generated[..length];
    let key = Provider.load_p256(generated)?;
    let mut public = [0u8; 65];
    Provider.p256_public(&key, &mut public)?;
    assert_eq!(
        shape(&scalar(generated), false, Some(&public[..])),
        generated
    );
    // td-tls-provider's rustls key provider loads exactly this shape too.
    let loaded = td_tls_provider::provider()
        .key_provider
        .load_private_key(PrivateKeyDer::Pkcs8(generated.to_vec().into()))?;
    assert!(loaded
        .choose_scheme(&[rustls::SignatureScheme::ECDSA_NISTP256_SHA256])
        .is_some());
    Ok(())
}

/// One signature and the policy's verifiers, by index, that must take it.
struct Signed {
    label: String,
    public: Vec<u8>,
    message: Vec<u8>,
    signature: Vec<u8>,
    own: Vec<usize>,
    /// Under a small-order key, which takes it for any message.
    any_message: bool,
}

fn signatures(text: &str, source: &str) -> Vec<Signed> {
    text.lines()
        .filter(|line| !line.starts_with("PGP_"))
        .map(|line| {
            let fields: Vec<&str> = line.split(' ').collect();
            let [name, public, message, signature] = fields[..] else {
                panic!("malformed row: {line}");
            };
            let algorithm = name.strip_prefix("SMALL_ORDER_").unwrap_or(name);
            let own = (0..NAMES.len())
                .filter(|&i| NAMES[i] == algorithm || NAMES[i] == format!("{algorithm}_BARE"))
                .collect::<Vec<_>>();
            assert_eq!(own.is_empty(), name.starts_with("REFUSED_"), "{name}");
            Signed {
                label: format!("{source} {name}"),
                public: unhex(public),
                message: unhex(message),
                signature: unhex(signature),
                own,
                any_message: algorithm != name,
            }
        })
        .collect()
}

/// AWS-LC's signatures and td-tls-provider's big-integer rows under each of
/// the policy's nineteen verifiers: taken exactly by the signer's algorithm
/// (PKCS#1 also under its identifier without NULL), EC keys uncompressed or
/// compressed, never in another form, and no refused row by any. These are
/// the answers AWS-LC gave for the same inputs when it was the backend.
#[test]
fn policy_verifiers_take_exactly_their_own_signatures() {
    let policy = policy();
    let all = policy.signature_verification_algorithms.all;
    assert_eq!(all.len(), NAMES.len());
    let mut inputs = signatures(AWS_LC_SIGNATURES, "AWS-LC");
    assert_eq!(inputs.len(), 12);
    inputs.extend(signatures(SIGNATURE_ANSWERS, "answer"));
    // ECDSA r or s of 0 or n or more, or not strict DER; RSA moduli outside
    // 2048..8192 bits, exponents of 1, even or over 33 bits; Ed25519 S >= L.
    let refused = inputs.iter().filter(|input| input.own.is_empty()).count();
    assert_eq!(refused, 14);
    let mut exercised = vec![0usize; NAMES.len()];
    for input in &inputs {
        let mut keys = vec![("as signed", input.public.clone(), true)];
        if input.public.first() == Some(&4) {
            let short = compressed(&input.public);
            let mut negated = short.clone();
            negated[0] ^= 1;
            let mut hybrid = input.public.clone();
            hybrid[0] = 6 | (input.public[input.public.len() - 1] & 1);
            keys.push(("compressed", short, true));
            keys.push(("compressed, other y", negated, false));
            keys.push(("hybrid", hybrid, false));
            keys.push(("infinity", vec![0], false));
            let mut truncated = input.public.clone();
            truncated.pop();
            keys.push(("truncated", truncated, false));
        }
        for (index, verifier) in all.iter().enumerate() {
            let own = input.own.contains(&index);
            for (form, key, is_key) in &keys {
                let context = format!("{} under {}, key {form}", input.label, NAMES[index]);
                let taken = verifier
                    .verify_signature(key, &input.message, &input.signature)
                    .is_ok();
                assert_eq!(taken, own && *is_key, "{context}");
                if !(own && *is_key) {
                    continue;
                }
                exercised[index] += 1;
                let mut message = input.message.clone();
                message[0] ^= 1;
                assert_eq!(
                    verifier
                        .verify_signature(key, &message, &input.signature)
                        .is_ok(),
                    input.any_message,
                    "{context}, changed message"
                );
                for at in [0, input.signature.len() / 2, input.signature.len() - 1] {
                    let mut signature = input.signature.clone();
                    signature[at] ^= 0x10;
                    assert!(
                        verifier
                            .verify_signature(key, &input.message, &signature)
                            .is_err(),
                        "{context}, changed signature byte {at}"
                    );
                }
            }
        }
    }
    for (index, count) in exercised.iter().enumerate() {
        let least = if NAMES[index].starts_with("ECDSA_") {
            2
        } else {
            1
        };
        assert!(*count >= least, "{} unexercised", NAMES[index]);
    }
    assert!(CryptoProvider::get_default().is_none());
}

/// AWS-LC's certificate paths, a P-256 leaf under a root for each algorithm
/// it signs with and each EC root's key in both point forms, verify under the
/// policy's certificate algorithms; a changed leaf signature does not.
#[test]
fn committed_certificate_paths_verify_under_the_policy() -> Result<()> {
    let policy = policy();
    let all: &[&'static dyn SignatureVerificationAlgorithm] =
        policy.signature_verification_algorithms.all;
    let now = UnixTime::since_unix_epoch(Duration::from_secs(NOW));
    let mut paths = 0;
    for line in f::CERTIFICATE_PATHS.lines() {
        let fields: Vec<&str> = line.split(' ').collect();
        let ["PATH", name, root, leaf] = fields[..] else {
            continue;
        };
        assert!(NAMES.contains(&name), "{name}");
        let mut store = rustls::RootCertStore::empty();
        store.add(CertificateDer::from(unhex(root)))?;
        let verify = |leaf: &[u8]| {
            let leaf = CertificateDer::from(leaf.to_vec());
            let parsed = rustls::server::ParsedCertificate::try_from(&leaf).unwrap();
            rustls::client::verify_server_cert_signed_by_trust_anchor(
                &parsed,
                &store,
                &[],
                now,
                all,
            )
            .is_ok()
        };
        let leaf = unhex(leaf);
        assert!(verify(&leaf), "{name} path {paths}");
        let mut changed = leaf;
        let last = changed.len() - 1;
        changed[last] ^= 1;
        assert!(!verify(&changed), "changed {name} path {paths}");
        paths += 1;
    }
    // Five EC roots twice, in both point forms, and ten others once.
    assert_eq!(paths, 20);
    Ok(())
}

/// The scalar and embedded-point checks pkcs8::validate leaves to the loader:
/// a scalar outside 1..n-1 and another key's point are refused in every shape
/// that can hold them, as AWS-LC refused them.
#[test]
fn the_loader_checks_scalar_range_and_embedded_point() -> Result<()> {
    let n: [u8; 32] = unhex("ffffffff00000000ffffffffffffffffbce6faada7179e84f3b9cac2fc632551")
        .try_into()
        .unwrap();
    let mut n_minus_1 = n;
    n_minus_1[31] -= 1;
    let mut n_plus_1 = n;
    n_plus_1[31] += 1;
    let mut one = [0u8; 32];
    one[31] = 1;
    let shapes = [(false, false), (true, false), (false, true), (true, true)];
    let generator = p256::SecretKey::from_bytes(&one)?.public_key()?;
    for (secret, admitted) in [
        ([0u8; 32], false),
        (one, true),
        (n_minus_1, true),
        (n, false),
        (n_plus_1, false),
        ([0xff; 32], false),
    ] {
        let point = match p256::SecretKey::from_bytes(&secret) {
            Ok(key) => key.public_key()?,
            Err(_) => generator,
        };
        for (parameters, included) in shapes {
            let document = shape(&secret, parameters, included.then_some(point.as_slice()));
            crate::pkcs8::validate(&document)?;
            let context = format!("{} bytes, {secret:02x?}", document.len());
            match Provider.load_p256(&document) {
                Ok(key) => {
                    assert!(admitted, "{context}");
                    let mut public = [0u8; 65];
                    Provider.p256_public(&key, &mut public)?;
                    assert_eq!(public, point, "{context}");
                }
                Err(error) => {
                    assert!(!admitted, "{context}");
                    assert_eq!(error, crate::Error::Invalid, "{context}");
                }
            }
        }
    }
    let secret = scalar(&f::TestKey::generate_pkcs8()?);
    let other = *f::TestKey::generate().public_key();
    for parameters in [false, true] {
        let document = shape(&secret, parameters, Some(&other));
        crate::pkcs8::validate(&document)?;
        assert_eq!(
            Provider.load_p256(&document).err(),
            Some(crate::Error::Invalid)
        );
    }
    Ok(())
}

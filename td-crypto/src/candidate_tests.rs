//! Test-only qualification of the td-primitives backend, td-tls-provider and
//! td-primitives, against the admitted AWS-LC backend (td-mta/IMPLEMENTATION.md
//! F04). The candidate is assembled here with td-crypto's exact policy; nothing
//! in the library reaches it and no selector exists. Results are compared, never
//! randomized signature bytes.
#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use crate::certificate_fixtures::{self as f, Parameters};
use crate::{Crypto, Provider, P256_PKCS8_CAPACITY};
use aws_lc_rs::encoding::{AsDer, Pkcs8V1Der};
use aws_lc_rs::rand::SystemRandom;
use aws_lc_rs::signature::{self as native, EcdsaKeyPair, Ed25519KeyPair, KeyPair, RsaKeyPair};
use rustls::crypto::{CryptoProvider, WebPkiSupportedAlgorithms};
use rustls::pki_types::{
    CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, SignatureVerificationAlgorithm, UnixTime,
};
use rustls::{CipherSuite as C, NamedGroup as G, SignatureScheme as S, SupportedCipherSuite};
use std::io::{Read, Write};
use std::sync::Arc;
use std::time::Duration;
use td_primitives::ec::p256;
use td_primitives::sha::{Digest, Sha256};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
const NOW: u64 = 1_800_000_000;
// The candidate's verifier names, in td-crypto's certificate algorithm order.
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
const P256_CURVE: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 3, 1, 7];
const P384_CURVE: &[u8] = &[0x2b, 0x81, 4, 0, 0x22];
const P521_CURVE: &[u8] = &[0x2b, 0x81, 4, 0, 0x23];

/// td-crypto's nineteen certificate algorithms in its order over td-tls-provider's
/// exported verifiers, and its ten handshake schemes, each ECDSA scheme's own
/// curve first, since TLS 1.3 takes only the first verifier a scheme maps to.
static SIGNATURE_ALGORITHMS: WebPkiSupportedAlgorithms = {
    use td_tls_provider::verifiers::*;
    WebPkiSupportedAlgorithms {
        all: &[
            ECDSA_P256_SHA256,
            ECDSA_P256_SHA384,
            ECDSA_P256_SHA512,
            ECDSA_P384_SHA256,
            ECDSA_P384_SHA384,
            ECDSA_P384_SHA512,
            ECDSA_P521_SHA256,
            ECDSA_P521_SHA384,
            ECDSA_P521_SHA512,
            ED25519,
            RSA_PSS_SHA256,
            RSA_PSS_SHA384,
            RSA_PSS_SHA512,
            RSA_PKCS1_SHA256,
            RSA_PKCS1_SHA384,
            RSA_PKCS1_SHA512,
            RSA_PKCS1_SHA256_BARE,
            RSA_PKCS1_SHA384_BARE,
            RSA_PKCS1_SHA512_BARE,
        ],
        mapping: &[
            (
                S::ECDSA_NISTP384_SHA384,
                &[ECDSA_P384_SHA384, ECDSA_P256_SHA384, ECDSA_P521_SHA384],
            ),
            (
                S::ECDSA_NISTP256_SHA256,
                &[ECDSA_P256_SHA256, ECDSA_P384_SHA256, ECDSA_P521_SHA256],
            ),
            (
                S::ECDSA_NISTP521_SHA512,
                &[ECDSA_P521_SHA512, ECDSA_P384_SHA512, ECDSA_P256_SHA512],
            ),
            (S::ED25519, &[ED25519]),
            (S::RSA_PSS_SHA512, &[RSA_PSS_SHA512]),
            (S::RSA_PSS_SHA384, &[RSA_PSS_SHA384]),
            (S::RSA_PSS_SHA256, &[RSA_PSS_SHA256]),
            (S::RSA_PKCS1_SHA512, &[RSA_PKCS1_SHA512]),
            (S::RSA_PKCS1_SHA384, &[RSA_PKCS1_SHA384]),
            (S::RSA_PKCS1_SHA256, &[RSA_PKCS1_SHA256]),
        ],
    }
};

/// td-crypto's policy (tls_policy::provider) over the candidate backend.
fn candidate() -> CryptoProvider {
    use td_tls_provider::tls12_ecdsa as x;
    let base = td_tls_provider::provider();
    let suite = |name| {
        *base
            .cipher_suites
            .iter()
            .find(|suite| suite.suite() == name)
            .unwrap()
    };
    let cipher_suites = vec![
        suite(C::TLS13_AES_256_GCM_SHA384),
        suite(C::TLS13_AES_128_GCM_SHA256),
        suite(C::TLS13_CHACHA20_POLY1305_SHA256),
        x::TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384,
        x::TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256,
        x::TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256,
        suite(C::TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384),
        suite(C::TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256),
        suite(C::TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256),
    ];
    CryptoProvider {
        cipher_suites,
        kx_groups: base.kx_groups.clone(),
        signature_verification_algorithms: SIGNATURE_ALGORITHMS,
        ..base
    }
}

fn aws() -> CryptoProvider {
    crate::tls_policy::provider().unwrap()
}

fn hex(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(&text[at..at + 2], 16).unwrap())
        .collect()
}

fn compressed(point: &[u8]) -> Vec<u8> {
    let size = (point.len() - 1) / 2;
    let mut out = vec![2 | (point[point.len() - 1] & 1)];
    out.extend_from_slice(&point[1..=size]);
    out
}

#[test]
fn the_candidate_assembles_td_cryptos_exact_policy() {
    let (candidate, aws) = (candidate(), aws());
    let names = |p: &CryptoProvider| {
        p.cipher_suites
            .iter()
            .map(|s| s.suite())
            .collect::<Vec<_>>()
    };
    assert_eq!(names(&candidate), names(&aws));
    for (ours, theirs) in candidate.cipher_suites.iter().zip(&aws.cipher_suites) {
        let name = ours.suite();
        match (ours, theirs) {
            (SupportedCipherSuite::Tls13(ours), SupportedCipherSuite::Tls13(theirs)) => {
                assert_eq!(
                    ours.common.hash_provider.algorithm(),
                    theirs.common.hash_provider.algorithm()
                );
                assert_eq!(ours.aead_alg.key_len(), theirs.aead_alg.key_len());
                assert_eq!(
                    ours.common.confidentiality_limit, theirs.common.confidentiality_limit,
                    "{name:?}"
                );
            }
            (SupportedCipherSuite::Tls12(ours), SupportedCipherSuite::Tls12(theirs)) => {
                assert_eq!(
                    ours.common.hash_provider.algorithm(),
                    theirs.common.hash_provider.algorithm()
                );
                assert_eq!(ours.sign, theirs.sign, "{name:?}");
                assert!(ours.kx == theirs.kx, "{name:?}");
                assert_eq!(
                    ours.common.confidentiality_limit, theirs.common.confidentiality_limit,
                    "{name:?}"
                );
                let (a, b) = (
                    ours.aead_alg.key_block_shape(),
                    theirs.aead_alg.key_block_shape(),
                );
                assert_eq!(
                    (a.enc_key_len, a.fixed_iv_len, a.explicit_nonce_len),
                    (b.enc_key_len, b.fixed_iv_len, b.explicit_nonce_len),
                    "{name:?}"
                );
            }
            _ => panic!("{name:?} differs in version"),
        }
    }
    let groups = |p: &CryptoProvider| p.kx_groups.iter().map(|g| g.name()).collect::<Vec<_>>();
    assert_eq!(groups(&candidate), groups(&aws));
    assert_eq!(groups(&candidate), [G::X25519, G::secp256r1, G::secp384r1]);
    let (ours, theirs) = (
        candidate.signature_verification_algorithms,
        aws.signature_verification_algorithms,
    );
    assert_eq!(ours.all.len(), theirs.all.len());
    for (index, (a, b)) in ours.all.iter().zip(theirs.all).enumerate() {
        assert_eq!(format!("{a:?}"), NAMES[index]);
        assert_eq!(a.public_key_alg_id(), b.public_key_alg_id(), "{index}");
        assert_eq!(a.signature_alg_id(), b.signature_alg_id(), "{index}");
    }
    assert_eq!(ours.supported_schemes(), theirs.supported_schemes());
    assert_eq!(ours.mapping.len(), theirs.mapping.len());
    for ((scheme, a), (their_scheme, b)) in ours.mapping.iter().zip(theirs.mapping) {
        assert_eq!(scheme, their_scheme);
        assert_eq!(a.len(), b.len(), "{scheme:?}");
        for (a, b) in a.iter().zip(*b) {
            assert_eq!(a.public_key_alg_id(), b.public_key_alg_id(), "{scheme:?}");
            assert_eq!(a.signature_alg_id(), b.signature_alg_id(), "{scheme:?}");
        }
    }
    // No ML-DSA: the candidate has nothing td-crypto's selector excludes.
    assert!(!ours
        .all
        .iter()
        .any(|a| a.public_key_alg_id() == rustls::pki_types::alg_id::ML_DSA_44));
    assert!(CryptoProvider::get_default().is_none());
}

/// One signature and the verifiers, by index into the policy's algorithm list,
/// that must take it: none for a signature both backends must refuse.
struct Signed {
    label: String,
    public: Vec<u8>,
    message: Vec<u8>,
    signature: Vec<u8>,
    own: Vec<usize>,
    /// Under a small-order key, which takes it for any message.
    any_message: bool,
}

fn aws_signatures() -> Result<Vec<Signed>> {
    let rng = SystemRandom::new();
    let mut out = Vec::new();
    for (index, algorithm) in [
        (0, &native::ECDSA_P256_SHA256_ASN1_SIGNING),
        (4, &native::ECDSA_P384_SHA384_ASN1_SIGNING),
        (6, &native::ECDSA_P521_SHA256_ASN1_SIGNING),
        (7, &native::ECDSA_P521_SHA384_ASN1_SIGNING),
        (8, &native::ECDSA_P521_SHA512_ASN1_SIGNING),
    ] {
        let document = EcdsaKeyPair::generate_pkcs8(algorithm, &rng)?;
        let key = EcdsaKeyPair::from_pkcs8(algorithm, document.as_ref())?;
        let message = format!("AWS-LC signs for {}", NAMES[index]).into_bytes();
        out.push(Signed {
            label: format!("AWS-LC {}", NAMES[index]),
            public: key.public_key().as_ref().to_vec(),
            signature: key.sign(&rng, &message)?.as_ref().to_vec(),
            message,
            own: vec![index],
            any_message: false,
        });
    }
    let key = Ed25519KeyPair::generate()?;
    let message = b"AWS-LC signs for ED25519".to_vec();
    out.push(Signed {
        label: "AWS-LC ED25519".into(),
        public: key.public_key().as_ref().to_vec(),
        signature: key.sign(&message).as_ref().to_vec(),
        message,
        own: vec![9],
        any_message: false,
    });
    let key = RsaKeyPair::generate(aws_lc_rs::rsa::KeySize::Rsa2048)?;
    for (own, encoding) in [
        (vec![10], &native::RSA_PSS_SHA256),
        (vec![11], &native::RSA_PSS_SHA384),
        (vec![12], &native::RSA_PSS_SHA512),
        (vec![13, 16], &native::RSA_PKCS1_SHA256),
        (vec![14, 17], &native::RSA_PKCS1_SHA384),
        (vec![15, 18], &native::RSA_PKCS1_SHA512),
    ] {
        let message = format!("AWS-LC signs for {}", NAMES[own[0]]).into_bytes();
        let mut signature = vec![0; key.public_modulus_len()];
        key.sign(encoding, &rng, &message, &mut signature)?;
        out.push(Signed {
            label: format!("AWS-LC {}", NAMES[own[0]]),
            public: key.public_key().as_ref().to_vec(),
            message,
            signature,
            own,
            any_message: false,
        });
    }
    Ok(out)
}

/// td-tls-provider's big-integer answers, among them the four ECDSA curve and
/// hash pairs AWS-LC will not sign with, a signature under a small-order Ed25519
/// key, and the REFUSED_ rows no verifier may take.
fn python_signatures() -> Vec<Signed> {
    include_str!("../../td-tls-provider/tests/signature_answers.txt")
        .lines()
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
                label: format!("answer {name}"),
                public: hex(public),
                message: hex(message),
                signature: hex(signature),
                own,
                any_message: algorithm != name,
            }
        })
        .collect()
}

/// Both verifiers' answer for one input, which must agree.
fn agreed(
    candidate: &dyn SignatureVerificationAlgorithm,
    aws: &dyn SignatureVerificationAlgorithm,
    public: &[u8],
    message: &[u8],
    signature: &[u8],
    context: &str,
) -> bool {
    let ours = candidate
        .verify_signature(public, message, signature)
        .is_ok();
    let theirs = aws.verify_signature(public, message, signature).is_ok();
    assert_eq!(ours, theirs, "candidate {ours}, AWS-LC {theirs}: {context}");
    ours
}

#[test]
fn every_admitted_signature_algorithm_verifies_as_aws_lc_does() -> Result<()> {
    let (candidate, aws) = (candidate(), aws());
    let ours = candidate.signature_verification_algorithms.all;
    let theirs = aws.signature_verification_algorithms.all;
    assert_eq!(ours.len(), NAMES.len());
    let mut inputs = aws_signatures()?;
    inputs.extend(python_signatures());
    // ECDSA r or s of 0 or n or more, or not strict DER; RSA moduli outside
    // 2048..8192 bits, exponents of 1, even or over 33 bits; Ed25519 S >= L.
    let refused = inputs.iter().filter(|input| input.own.is_empty()).count();
    assert_eq!(refused, 14);
    let mut exercised = vec![0usize; NAMES.len()];
    for input in &inputs {
        // Each key form, and whether it is taken as the signer's key. SEC 1's
        // hybrid form (tags 6 and 7) holds the same point; both refuse it.
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
        for index in 0..NAMES.len() {
            let own = input.own.contains(&index);
            for (form, key, is_key) in &keys {
                let context = format!("{} under {}, key {form}", input.label, NAMES[index]);
                let taken = agreed(
                    ours[index],
                    theirs[index],
                    key,
                    &input.message,
                    &input.signature,
                    &context,
                );
                assert_eq!(taken, own && *is_key, "{context}");
                if own && *is_key {
                    exercised[index] += 1;
                    let mut message = input.message.clone();
                    message[0] ^= 1;
                    let taken = agreed(
                        ours[index],
                        theirs[index],
                        key,
                        &message,
                        &input.signature,
                        &format!("{context}, changed message"),
                    );
                    assert_eq!(taken, input.any_message, "{context}, changed message");
                    for at in [0, input.signature.len() / 2, input.signature.len() - 1] {
                        let mut signature = input.signature.clone();
                        signature[at] ^= 0x10;
                        assert!(!agreed(
                            ours[index],
                            theirs[index],
                            key,
                            &input.message,
                            &signature,
                            &format!("{context}, changed signature byte {at}")
                        ));
                    }
                }
            }
        }
    }
    // Every algorithm met at least one signature, EC ones in both key forms.
    for (index, count) in exercised.iter().enumerate() {
        let least = if NAMES[index].starts_with("ECDSA_") {
            2
        } else {
            1
        };
        assert!(*count >= least, "{} unexercised", NAMES[index]);
    }
    assert!(CryptoProvider::get_default().is_none());
    Ok(())
}

fn ec_spki(curve: &[u8], point: &[u8]) -> Vec<u8> {
    let mut bits = vec![0];
    bits.extend_from_slice(point);
    f::seq(&[
        f::seq(&[f::oid(&[0x2a, 0x86, 0x48, 0xce, 0x3d, 2, 1]), f::oid(curve)]),
        f::der(3, &bits),
    ])
}

/// A root key able to sign certificates with one admitted algorithm.
enum Root {
    Ec(EcdsaKeyPair, &'static [u8]),
    Ed25519(Ed25519KeyPair),
    Rsa(RsaKeyPair, &'static dyn native::RsaEncoding),
}

impl Root {
    /// Its SubjectPublicKeyInfo, EC points in either form.
    fn spkis(&self) -> Result<Vec<Vec<u8>>> {
        Ok(match self {
            Self::Ec(key, curve) => {
                let point = key.public_key().as_ref();
                vec![ec_spki(curve, point), ec_spki(curve, &compressed(point))]
            }
            Self::Ed25519(key) => vec![key.public_key().as_der()?.as_ref().to_vec()],
            Self::Rsa(key, _) => vec![key.public_key().as_der()?.as_ref().to_vec()],
        })
    }

    fn sign(&self, body: &[u8]) -> Result<Vec<u8>> {
        let rng = SystemRandom::new();
        Ok(match self {
            Self::Ec(key, _) => key.sign(&rng, body)?.as_ref().to_vec(),
            Self::Ed25519(key) => key.sign(body).as_ref().to_vec(),
            Self::Rsa(key, encoding) => {
                let mut signature = vec![0; key.public_modulus_len()];
                key.sign(*encoding, &rng, body, &mut signature)?;
                signature
            }
        })
    }
}

/// Certificate paths: a leaf signed by its root under each admitted algorithm
/// AWS-LC can sign with, and each EC root's key also compressed, verify alike
/// under both policies' certificate algorithms; a changed signature fails alike.
#[test]
fn certificate_paths_verify_alike_under_each_signable_algorithm() -> Result<()> {
    let (candidate, aws) = (candidate(), aws());
    let ours = candidate.signature_verification_algorithms.all;
    let theirs = aws.signature_verification_algorithms.all;
    let rng = SystemRandom::new();
    let ec = |algorithm, curve| -> Result<Root> {
        let document = EcdsaKeyPair::generate_pkcs8(algorithm, &rng)?;
        Ok(Root::Ec(
            EcdsaKeyPair::from_pkcs8(algorithm, document.as_ref())?,
            curve,
        ))
    };
    let rsa = |encoding| -> Result<Root> {
        Ok(Root::Rsa(
            RsaKeyPair::generate(aws_lc_rs::rsa::KeySize::Rsa2048)?,
            encoding,
        ))
    };
    let roots: Vec<(usize, Root)> = vec![
        (0, ec(&native::ECDSA_P256_SHA256_ASN1_SIGNING, P256_CURVE)?),
        (4, ec(&native::ECDSA_P384_SHA384_ASN1_SIGNING, P384_CURVE)?),
        (6, ec(&native::ECDSA_P521_SHA256_ASN1_SIGNING, P521_CURVE)?),
        (7, ec(&native::ECDSA_P521_SHA384_ASN1_SIGNING, P521_CURVE)?),
        (8, ec(&native::ECDSA_P521_SHA512_ASN1_SIGNING, P521_CURVE)?),
        (9, Root::Ed25519(Ed25519KeyPair::generate()?)),
        (10, rsa(&native::RSA_PSS_SHA256)?),
        (11, rsa(&native::RSA_PSS_SHA384)?),
        (12, rsa(&native::RSA_PSS_SHA512)?),
        (13, rsa(&native::RSA_PKCS1_SHA256)?),
        (14, rsa(&native::RSA_PKCS1_SHA384)?),
        (15, rsa(&native::RSA_PKCS1_SHA512)?),
        // The same PKCS#1 signatures under the identifiers without NULL.
        (16, rsa(&native::RSA_PKCS1_SHA256)?),
        (17, rsa(&native::RSA_PKCS1_SHA384)?),
        (18, rsa(&native::RSA_PKCS1_SHA512)?),
    ];
    let leaf_document =
        EcdsaKeyPair::generate_pkcs8(&native::ECDSA_P256_SHA256_ASN1_SIGNING, &rng)?;
    let leaf_key = EcdsaKeyPair::from_pkcs8(
        &native::ECDSA_P256_SHA256_ASN1_SIGNING,
        leaf_document.as_ref(),
    )?;
    let now = UnixTime::since_unix_epoch(Duration::from_secs(NOW));
    let mut paths = 0;
    for (index, root) in &roots {
        let algorithm = f::der(0x30, theirs[*index].signature_alg_id().as_ref());
        for spki in root.spkis()? {
            let root_cert = f::build(spki, algorithm.clone(), &Parameters::new(true), |body| {
                root.sign(body)
            })?;
            let leaf = f::build(
                f::p256_spki(&leaf_key),
                algorithm.clone(),
                &Parameters::new(false),
                |body| root.sign(body),
            )?;
            let mut store = rustls::RootCertStore::empty();
            store.add(CertificateDer::from(root_cert))?;
            let verify = |leaf: &[u8], all: &[&'static dyn SignatureVerificationAlgorithm]| {
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
            let context = format!("{} path {paths}", NAMES[*index]);
            assert!(verify(&leaf, ours), "candidate refuses {context}");
            assert!(verify(&leaf, theirs), "AWS-LC refuses {context}");
            let mut changed = leaf.clone();
            let last = changed.len() - 1;
            changed[last] ^= 1;
            assert!(!verify(&changed, ours), "candidate takes changed {context}");
            assert!(!verify(&changed, theirs), "AWS-LC takes changed {context}");
            paths += 1;
        }
    }
    // Five EC roots twice, in both point forms, and ten others once.
    assert_eq!(paths, 20);
    Ok(())
}

#[derive(Debug)]
struct Time;
impl rustls::time_provider::TimeProvider for Time {
    fn current_time(&self) -> Option<UnixTime> {
        Some(UnixTime::since_unix_epoch(Duration::from_secs(NOW)))
    }
}

#[derive(Clone, Copy, Debug)]
enum Leaf {
    P256,
    P384,
    P521,
    Ed25519,
    Rsa,
}

/// A local chain under a generated P-256 root and the leaf's PKCS#8, which
/// AWS-LC generated: the candidate's key provider loads it to serve.
struct Identity {
    chain: Vec<CertificateDer<'static>>,
    key: Vec<u8>,
    roots: rustls::RootCertStore,
}

fn identity(leaf: Leaf) -> Result<Identity> {
    let rng = SystemRandom::new();
    let p256 = &native::ECDSA_P256_SHA256_ASN1_SIGNING;
    let root_document = EcdsaKeyPair::generate_pkcs8(p256, &rng)?;
    let root = EcdsaKeyPair::from_pkcs8(p256, root_document.as_ref())?;
    let ec = |algorithm, curve| -> Result<(Vec<u8>, Vec<u8>)> {
        let document = EcdsaKeyPair::generate_pkcs8(algorithm, &rng)?;
        let key = EcdsaKeyPair::from_pkcs8(algorithm, document.as_ref())?;
        Ok((
            ec_spki(curve, key.public_key().as_ref()),
            document.as_ref().to_vec(),
        ))
    };
    let (spki, key) = match leaf {
        Leaf::P256 => ec(p256, P256_CURVE)?,
        Leaf::P384 => ec(&native::ECDSA_P384_SHA384_ASN1_SIGNING, P384_CURVE)?,
        Leaf::P521 => ec(&native::ECDSA_P521_SHA512_ASN1_SIGNING, P521_CURVE)?,
        Leaf::Ed25519 => {
            let document = Ed25519KeyPair::generate_pkcs8(&rng)?;
            let key = Ed25519KeyPair::from_pkcs8(document.as_ref())?;
            (
                key.public_key().as_der()?.as_ref().to_vec(),
                document.as_ref().to_vec(),
            )
        }
        Leaf::Rsa => {
            let key = RsaKeyPair::generate(aws_lc_rs::rsa::KeySize::Rsa2048)?;
            let document: Pkcs8V1Der<'static> = key.as_der()?;
            (
                key.public_key().as_der()?.as_ref().to_vec(),
                document.as_ref().to_vec(),
            )
        }
    };
    let sign = |body: &[u8]| -> f::Result<Vec<u8>> { Ok(root.sign(&rng, body)?.as_ref().to_vec()) };
    let leaf_cert = f::build(
        spki,
        f::signature_algorithm(),
        &Parameters::new(false),
        sign,
    )?;
    let mut roots = rustls::RootCertStore::empty();
    roots.add(CertificateDer::from(f::certificate(&root, &root, true)?))?;
    Ok(Identity {
        chain: vec![CertificateDer::from(leaf_cert)],
        key,
        roots,
    })
}

fn connect(
    client: CryptoProvider,
    server: CryptoProvider,
    version: &'static rustls::SupportedProtocolVersion,
    identity: &Identity,
) -> Result<(rustls::Connection, rustls::Connection)> {
    let server = rustls::ServerConfig::builder_with_details(Arc::new(server), Arc::new(Time))
        .with_protocol_versions(&[version])?
        .with_no_client_auth()
        .with_single_cert(
            identity.chain.clone(),
            PrivatePkcs8KeyDer::from(identity.key.clone()).into(),
        )?;
    let client = rustls::ClientConfig::builder_with_details(Arc::new(client), Arc::new(Time))
        .with_protocol_versions(&[version])?
        .with_root_certificates(identity.roots.clone())
        .with_no_client_auth();
    Ok((
        rustls::Connection::Client(rustls::ClientConnection::new(
            Arc::new(client),
            "localhost".try_into()?,
        )?),
        rustls::Connection::Server(rustls::ServerConnection::new(Arc::new(server))?),
    ))
}

/// Handshake, then more than one record one way and a short reply the other.
fn exchange(client: &mut rustls::Connection, server: &mut rustls::Connection) -> Result<()> {
    crate::tls_smoke::drive(client, server)?;
    let long = vec![0x5a; 20_000];
    client.writer().write_all(&long)?;
    crate::tls_smoke::drive(client, server)?;
    let mut got = vec![0; long.len()];
    server.reader().read_exact(&mut got)?;
    assert!(got == long);
    server.writer().write_all(b"reply")?;
    crate::tls_smoke::drive(client, server)?;
    let mut got = [0; 5];
    client.reader().read_exact(&mut got)?;
    assert_eq!(&got, b"reply");
    Ok(())
}

fn narrowed(provider: CryptoProvider, suite: C, group: G) -> CryptoProvider {
    let suite = *provider
        .cipher_suites
        .iter()
        .find(|s| s.suite() == suite)
        .unwrap();
    let group = *provider
        .kx_groups
        .iter()
        .find(|g| g.name() == group)
        .unwrap();
    CryptoProvider {
        cipher_suites: vec![suite],
        kx_groups: vec![group],
        ..provider
    }
}

/// Every suite the P-256 identity can serve, over every group, under the
/// version it belongs to: candidate client against an AWS-LC server, and AWS-LC
/// client against a candidate server serving AWS-LC's generated key.
#[test]
fn every_suite_and_group_interoperates_in_both_directions() -> Result<()> {
    let identity = identity(Leaf::P256)?;
    let suites = [
        (&rustls::version::TLS13, C::TLS13_AES_256_GCM_SHA384),
        (&rustls::version::TLS13, C::TLS13_AES_128_GCM_SHA256),
        (&rustls::version::TLS13, C::TLS13_CHACHA20_POLY1305_SHA256),
        (
            &rustls::version::TLS12,
            C::TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384,
        ),
        (
            &rustls::version::TLS12,
            C::TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256,
        ),
        (
            &rustls::version::TLS12,
            C::TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256,
        ),
    ];
    let mut connected = 0;
    for (version, suite) in suites {
        for group in [G::X25519, G::secp256r1, G::secp384r1] {
            for candidate_client in [true, false] {
                let (ours, theirs) = (
                    narrowed(candidate(), suite, group),
                    narrowed(aws(), suite, group),
                );
                let (client, server) = if candidate_client {
                    (ours, theirs)
                } else {
                    (theirs, ours)
                };
                let (mut c, mut s) = connect(client, server, version, &identity)?;
                exchange(&mut c, &mut s).map_err(|e| {
                    format!("{suite:?} {group:?} candidate client {candidate_client}: {e}")
                })?;
                for end in [&c, &s] {
                    assert_eq!(end.protocol_version(), Some(version.version));
                    assert_eq!(
                        end.negotiated_cipher_suite().map(|s| s.suite()),
                        Some(suite)
                    );
                    assert_eq!(
                        end.negotiated_key_exchange_group().map(|g| g.name()),
                        Some(group)
                    );
                }
                connected += 1;
            }
        }
    }
    assert_eq!(connected, 36);
    assert!(CryptoProvider::get_default().is_none());
    Ok(())
}

/// The client provider with only these handshake schemes offered.
fn offering(provider: CryptoProvider, schemes: &[S]) -> CryptoProvider {
    let mut algorithms = provider.signature_verification_algorithms;
    let mapping: Vec<_> = algorithms
        .mapping
        .iter()
        .filter(|(scheme, _)| schemes.contains(scheme))
        .copied()
        .collect();
    algorithms.mapping = Box::leak(mapping.into_boxed_slice());
    CryptoProvider {
        signature_verification_algorithms: algorithms,
        ..provider
    }
}

/// AWS-LC servers holding every other admitted key type: the candidate client
/// verifies each handshake signature scheme the policy offers, and its outcome
/// is the AWS-LC client's under the same offer, refusals included.
#[test]
fn handshake_signatures_under_every_scheme_match_the_aws_lc_client() -> Result<()> {
    let tls12 = &rustls::version::TLS12;
    let tls13 = &rustls::version::TLS13;
    let all = &[] as &[S];
    let cases: &[(Leaf, &[S], bool)] = &[
        (Leaf::P256, &[S::ECDSA_NISTP256_SHA256], true),
        (Leaf::P384, &[S::ECDSA_NISTP384_SHA384], true),
        (Leaf::P521, &[S::ECDSA_NISTP521_SHA512], true),
        (Leaf::Ed25519, &[S::ED25519], true),
        (Leaf::Rsa, &[S::RSA_PSS_SHA256], true),
        (Leaf::Rsa, &[S::RSA_PSS_SHA384], true),
        (Leaf::Rsa, &[S::RSA_PSS_SHA512], true),
        // TLS 1.2 only: TLS 1.3 refuses PKCS#1 in the handshake.
        (Leaf::Rsa, &[S::RSA_PKCS1_SHA256], false),
        (Leaf::Rsa, &[S::RSA_PKCS1_SHA384], false),
        (Leaf::Rsa, &[S::RSA_PKCS1_SHA512], false),
        // The whole offer, as configured.
        (Leaf::P384, all, true),
        (Leaf::P521, all, true),
        (Leaf::Ed25519, all, true),
        (Leaf::Rsa, all, true),
        // The server finds no offered scheme its P-384 key signs with and
        // refuses before any signature reaches the client's verifier.
        (Leaf::P384, &[S::ECDSA_NISTP256_SHA256], true),
    ];
    let mut compared = 0;
    for (leaf, schemes, in_tls13) in cases {
        let identity = identity(*leaf)?;
        for version in [tls12, tls13] {
            let outcome = |client: CryptoProvider| -> Result<bool> {
                let client = if schemes.is_empty() {
                    client
                } else {
                    offering(client, schemes)
                };
                let (mut c, mut s) = connect(client, aws(), version, &identity)?;
                Ok(exchange(&mut c, &mut s).is_ok())
            };
            let (ours, theirs) = (outcome(candidate())?, outcome(aws())?);
            let context = format!("{leaf:?} {schemes:?} {:?}", version.version);
            assert_eq!(ours, theirs, "candidate {ours}, AWS-LC {theirs}: {context}");
            let no_common_scheme =
                matches!(leaf, Leaf::P384) && **schemes == [S::ECDSA_NISTP256_SHA256];
            let is_tls12 = version.version == rustls::ProtocolVersion::TLSv1_2;
            let expected = !no_common_scheme && (is_tls12 || *in_tls13);
            assert_eq!(ours, expected, "{context}");
            compared += 1;
        }
    }
    assert_eq!(compared, 30);
    Ok(())
}

/// TLS 1.2 ECDHE-RSA suites over every group, served by AWS-LC with an RSA
/// key: the candidate client's RSA suites and verifiers.
#[test]
fn rsa_suites_interoperate_with_an_aws_lc_server() -> Result<()> {
    let identity = identity(Leaf::Rsa)?;
    let mut connected = 0;
    for suite in [
        C::TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384,
        C::TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256,
        C::TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256,
    ] {
        for group in [G::X25519, G::secp256r1, G::secp384r1] {
            let (mut c, mut s) = connect(
                narrowed(candidate(), suite, group),
                narrowed(aws(), suite, group),
                &rustls::version::TLS12,
                &identity,
            )?;
            exchange(&mut c, &mut s)?;
            assert_eq!(c.negotiated_cipher_suite().map(|s| s.suite()), Some(suite));
            connected += 1;
        }
    }
    assert_eq!(connected, 9);
    Ok(())
}

/// A peer offering only a hybrid group, outside the policy, is refused by the
/// candidate in either role, as by AWS-LC under the same policy. TLS 1.3 only:
/// rustls uses a hybrid group in no other version.
#[test]
fn a_hybrid_only_peer_is_refused_in_both_roles() -> Result<()> {
    let identity = identity(Leaf::P256)?;
    let hybrid = || CryptoProvider {
        kx_groups: vec![rustls::crypto::aws_lc_rs::kx_group::X25519MLKEM768],
        ..rustls::crypto::aws_lc_rs::default_provider()
    };
    let connected = |client, server, version| {
        connect(client, server, version, &identity)
            .and_then(|(mut c, mut s)| exchange(&mut c, &mut s))
            .is_ok()
    };
    let policies: [fn() -> CryptoProvider; 2] = [candidate, aws];
    let tls13 = &rustls::version::TLS13;
    for policy in policies {
        assert!(!connected(policy(), hybrid(), tls13));
        assert!(!connected(hybrid(), policy(), tls13));
    }
    // The hybrid peers themselves connect: the refusal is the policy's.
    assert!(connected(hybrid(), hybrid(), &rustls::version::TLS13));
    Ok(())
}

#[test]
fn constant_time_comparison_agrees() {
    for left in [[0u8; 32], [0xa5; 32], [0xff; 32]] {
        assert!(Provider.equal_digest(&left, &left));
        assert!(td_primitives::ct::eq(&left, &left));
        for offset in 0..32 {
            for bit in 0..8 {
                let mut right = left;
                right[offset] ^= 1 << bit;
                assert_eq!(
                    td_primitives::ct::eq(&left, &right),
                    Provider.equal_digest(&left, &right)
                );
                assert!(!td_primitives::ct::eq(&right, &left));
            }
        }
    }
}

/// The scalar of an accepted P-256 PKCS#8 document (pkcs8::validate's subset).
fn scalar(document: &[u8]) -> [u8; 32] {
    crate::pkcs8::validate(document).unwrap();
    let at = document
        .windows(5)
        .position(|w| w == [0x02, 0x01, 0x01, 0x04, 0x20])
        .unwrap()
        + 5;
    document[at..at + 32].try_into().unwrap()
}

fn spki_point(spki: &[u8]) -> Vec<u8> {
    // A P-256 SPKI ends in the 65-byte point.
    spki[spki.len() - 65..].to_vec()
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

/// r || s as the DER SEQUENCE td-primitives verifies.
fn der_signature(fixed: &[u8; 64]) -> Vec<u8> {
    f::seq(&[der_integer(&fixed[..32]), der_integer(&fixed[32..])])
}

fn candidate_verifies(public: &[u8], message: &[u8], fixed: &[u8; 64]) -> bool {
    p256::verify(
        public,
        Sha256::digest(message).as_ref(),
        &der_signature(fixed),
    )
    .is_ok()
}

fn aws_verifies(public: &[u8], message: &[u8], signature: &[u8], fixed: bool) -> bool {
    let algorithm = if fixed {
        &native::ECDSA_P256_SHA256_FIXED
    } else {
        &native::ECDSA_P256_SHA256_ASN1
    };
    native::UnparsedPublicKey::new(algorithm, public)
        .verify(message, signature)
        .is_ok()
}

/// A candidate-generated key: uniform bytes until one is a scalar, written in
/// td-tls-provider's PKCS#8 shape.
fn candidate_key() -> (Vec<u8>, p256::SecretKey) {
    let mut bytes = [0u8; p256::SECRET_LEN];
    let secret = loop {
        td_primitives::random::fill(&mut bytes).unwrap();
        if let Ok(secret) = p256::SecretKey::from_bytes(&bytes) {
            break secret;
        }
    };
    let document = td_tls_provider::p256_to_pkcs8(&bytes, &secret.public_key().unwrap());
    td_primitives::ct::zero(&mut bytes);
    (document, secret)
}

/// Persisted keys cross both ways: AWS-LC's generated PKCS#8 is byte for byte
/// the shape td-tls-provider writes and loads, and each side's key loads, gives
/// the same public point and signs what the other verifies.
#[test]
fn p256_keys_and_es256_signatures_cross_both_backends() -> Result<()> {
    let messages: &[&[u8]] = &[b"", b"abc", &[0xa5; 65], &[0x42; 1024]];
    for _ in 0..4 {
        let mut document = [0u8; P256_PKCS8_CAPACITY];
        let length = Provider.generate_p256(&mut document)?;
        let document = &document[..length];
        assert_eq!(length, 138);
        let key = Provider.load_p256(document)?;
        let mut public = [0u8; 65];
        Provider.p256_public(&key, &mut public)?;

        // AWS-LC to the candidate: td-primitives from the scalar, and the
        // rustls key provider from the document itself.
        let secret = scalar(document);
        let ours = p256::SecretKey::from_bytes(&secret)?;
        assert_eq!(ours.public_key()?, public);
        assert_eq!(td_tls_provider::p256_to_pkcs8(&secret, &public), document);
        let loaded = td_tls_provider::provider()
            .key_provider
            .load_private_key(PrivateKeyDer::Pkcs8(document.to_vec().into()))?;
        assert_eq!(
            spki_point(loaded.public_key().unwrap().as_ref()),
            public.as_slice()
        );
        let signer = loaded
            .choose_scheme(&[S::ECDSA_NISTP256_SHA256])
            .ok_or("no signer")?;
        for message in messages {
            let mut fixed = [0u8; 64];
            Provider.sign_es256(&key, message, &mut fixed)?;
            assert!(candidate_verifies(&public, message, &fixed));
            assert!(aws_verifies(&public, message, &fixed, true));
            let mut changed = fixed;
            changed[63] ^= 1;
            assert!(!candidate_verifies(&public, message, &changed));
            assert!(!aws_verifies(&public, message, &changed, true));
            for der in [ours.sign_sha256(message)?, signer.sign(message)?] {
                assert!(aws_verifies(&public, message, &der, false));
                let mut changed_message = message.to_vec();
                changed_message.push(0);
                assert!(!aws_verifies(&public, &changed_message, &der, false));
            }
        }

        // The candidate to AWS-LC.
        let (document, ours) = candidate_key();
        assert_eq!(document.len(), 138);
        crate::pkcs8::validate(&document)?;
        let key = Provider.load_p256(&document)?;
        let mut public = [0u8; 65];
        Provider.p256_public(&key, &mut public)?;
        assert_eq!(ours.public_key()?, public);
        for message in messages {
            let der = ours.sign_sha256(message)?;
            assert!(aws_verifies(&public, message, &der, false));
            let mut fixed = [0u8; 64];
            Provider.sign_es256(&key, message, &mut fixed)?;
            assert!(candidate_verifies(&public, message, &fixed));
        }
    }
    Ok(())
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
    let mut info = hex("020100301306072a8648ce3d020106082a8648ce3d030107");
    info.extend(tlv(0x04, &tlv(0x30, &key)));
    tlv(0x30, &info)
}

/// The facade loads four PKCS#8 shapes; td-tls-provider's rustls key provider
/// loads only the 138-byte one both backends generate. td-primitives serves all
/// four from the scalar the facade's own reader finds, so a cutover keeping that
/// reader keeps every persisted shape.
#[test]
fn the_rustls_key_provider_loads_one_of_the_facades_four_shapes() -> Result<()> {
    let mut bytes = [0u8; P256_PKCS8_CAPACITY];
    let length = Provider.generate_p256(&mut bytes)?;
    let secret = scalar(&bytes[..length]);
    let point = p256::SecretKey::from_bytes(&secret)?.public_key()?;
    for (parameters, included, length) in [
        (false, false, 67),
        (true, false, 79),
        (false, true, 138),
        (true, true, 150),
    ] {
        let document = shape(&secret, parameters, included.then_some(point.as_slice()));
        assert_eq!(document.len(), length);
        let key = Provider.load_p256(&document)?;
        let mut aws_public = [0u8; 65];
        Provider.p256_public(&key, &mut aws_public)?;
        assert_eq!(scalar(&document), secret);
        assert_eq!(
            p256::SecretKey::from_bytes(&scalar(&document))?.public_key()?,
            aws_public
        );
        let loaded = td_tls_provider::provider()
            .key_provider
            .load_private_key(PrivateKeyDer::Pkcs8(document.into()));
        assert_eq!(loaded.is_ok(), length == 138, "{length}-byte shape");
    }
    Ok(())
}

/// pkcs8::validate is structural; AWS-LC also refuses a scalar outside 1..n-1
/// and an embedded point that is not the scalar's. td-primitives refuses the
/// same scalars and derives the point from every shape's scalar, so a cutover
/// reader can keep both checks (td-crypto/DESIGN.md).
#[test]
fn a_cutover_reader_can_keep_aws_lcs_scalar_and_point_checks() -> Result<()> {
    let n = hex("ffffffff00000000ffffffffffffffffbce6faada7179e84f3b9cac2fc632551");
    let n: [u8; 32] = n.try_into().unwrap();
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
        let ours = p256::SecretKey::from_bytes(&secret);
        assert_eq!(ours.is_ok(), admitted, "{secret:02x?}");
        // In range, the scalar's own point; otherwise any point, G's here.
        let point = match &ours {
            Ok(key) => key.public_key()?,
            Err(_) => generator,
        };
        for (parameters, included) in shapes {
            let document = shape(&secret, parameters, included.then_some(point.as_slice()));
            crate::pkcs8::validate(&document)?;
            let context = format!("{} bytes, {secret:02x?}", document.len());
            let key = Provider.load_p256(&document);
            assert_eq!(key.is_ok(), admitted, "{context}");
            if let Ok(key) = key {
                let mut public = [0u8; 65];
                Provider.p256_public(&key, &mut public)?;
                assert_eq!(public, point, "{context}");
            }
        }
    }
    // Another key's point embedded: structurally sound, refused by AWS-LC and
    // the rustls key provider, and unequal to the point td-primitives derives.
    let mut bytes = [0u8; P256_PKCS8_CAPACITY];
    let length = Provider.generate_p256(&mut bytes)?;
    let secret = scalar(&bytes[..length]);
    let derived = p256::SecretKey::from_bytes(&secret)?.public_key()?;
    let other = candidate_key().1.public_key()?;
    assert_ne!(derived, other);
    for parameters in [false, true] {
        let document = shape(&secret, parameters, Some(&other));
        crate::pkcs8::validate(&document)?;
        assert_eq!(scalar(&document), secret);
        assert!(Provider.load_p256(&document).is_err());
        assert!(td_tls_provider::provider()
            .key_provider
            .load_private_key(PrivateKeyDer::Pkcs8(document.into()))
            .is_err());
    }
    Ok(())
}

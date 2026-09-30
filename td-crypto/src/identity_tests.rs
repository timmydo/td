#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;
use crate::certificate_fixtures::{self as fixture, der, extension, oid, seq, Parameters};
use crate::pem::tests::pem;
use aws_lc_rs::signature::{EcdsaKeyPair, KeyPair, ECDSA_P256_SHA256_ASN1_SIGNING};

const NOW: u64 = 1_800_000_000;
const START: u64 = 1_735_689_600;
const END: u64 = 2_051_222_400;

fn key() -> (aws_lc_rs::pkcs8::Document, EcdsaKeyPair) {
    let bytes = EcdsaKeyPair::generate_pkcs8(
        &ECDSA_P256_SHA256_ASN1_SIGNING,
        &aws_lc_rs::rand::SystemRandom::new(),
    )
    .unwrap();
    let key = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, bytes.as_ref()).unwrap();
    (bytes, key)
}

struct Fixture {
    root: EcdsaKeyPair,
    leaf: EcdsaKeyPair,
    secret: Vec<u8>,
}

impl Fixture {
    fn new() -> Self {
        let (_, root) = key();
        let (secret, leaf) = key();
        Self {
            root,
            leaf,
            secret: pem("PRIVATE KEY", secret.as_ref()),
        }
    }

    fn chain(&self, leaf: &Parameters, root: &Parameters) -> Vec<Vec<u8>> {
        vec![
            fixture::make(&self.leaf, &self.root, leaf).unwrap(),
            fixture::make(&self.root, &self.root, root).unwrap(),
        ]
    }

    fn admit(
        &self,
        chain: &[Vec<u8>],
        names: &[&str],
        now: Option<u64>,
    ) -> Result<ServerIdentity, TlsError> {
        ServerIdentity::from_pem(&chain_pem(chain), &self.secret, names, now)
    }
}

fn chain_pem(chain: &[Vec<u8>]) -> Vec<u8> {
    chain
        .iter()
        .flat_map(|der| pem("CERTIFICATE", der))
        .collect()
}

#[test]
fn local_identity_owns_material_and_checks_validity() {
    let fixture = Fixture::new();
    let chain = fixture.chain(&Parameters::new(false), &Parameters::new(true));
    let mut text = chain_pem(&chain);
    let mut private = fixture.secret.clone();
    let identity = ServerIdentity::from_pem(&text, &private, &["LOCALHOST"], Some(NOW)).unwrap();
    text.fill(0);
    private.fill(0);
    assert_eq!(identity.certificate_count(), 2);
    assert_eq!(identity.certificate_der(0), Some(chain[0].as_slice()));
    assert_eq!(identity.certificate_der(1), Some(chain[1].as_slice()));
    assert_eq!(identity.certificate_der(usize::MAX), None);
    assert_eq!(identity.name_count(), 1);
    assert_eq!(identity.name(0), Some("localhost"));
    assert_eq!(identity.name(usize::MAX), None);
    assert_eq!(identity.validity(), (START, END));
    for now in [START, NOW, END] {
        assert_eq!(identity.check_validity(Some(now)), Ok(()));
    }
    assert_eq!(identity.check_validity(None), Err(TlsError::Clock));
    assert_eq!(
        identity.check_validity(Some(START - 1)),
        Err(TlsError::Verification(VerificationFailure::NotYetValid))
    );
    assert_eq!(
        identity.check_validity(Some(END + 1)),
        Err(TlsError::Verification(VerificationFailure::Expired))
    );
    assert_eq!(
        format!("{identity:?}"),
        "ServerIdentity { certificates: 2, names: 1, .. }"
    );
    fn send_sync<T: Send + Sync + std::panic::UnwindSafe + std::panic::RefUnwindSafe>() {}
    send_sync::<ServerIdentity>();
    assert!(fixture
        .admit(&chain[..1], &["localhost"], Some(NOW))
        .is_ok());
    let mut self_signed = Parameters::new(false);
    self_signed.issuer = self_signed.subject.clone();
    let leaf = fixture::make(&fixture.leaf, &fixture.leaf, &self_signed).unwrap();
    assert!(fixture.admit(&[leaf], &["localhost"], Some(NOW)).is_ok());
}

#[test]
fn narrowed_identity_shares_material_and_cannot_expand_or_revive_bindings() {
    let fixture = Fixture::new();
    let mut parameters = Parameters::new(false);
    parameters.extensions[2] = extension(
        0x11,
        false,
        seq(&[
            der(0x82, b"localhost"),
            der(0x82, b"mail.example.test"),
            der(0x82, b"unused.example.test"),
        ]),
    );
    let chain = fixture.chain(&parameters, &Parameters::new(true));
    let identity = fixture
        .admit(&chain, &["localhost", "mail.example.test"], Some(NOW))
        .unwrap();
    let narrowed = identity.restrict_names(&["LOCALHOST"]).unwrap();
    assert_eq!(narrowed.name_count(), 1);
    assert_eq!(narrowed.name(0), Some("localhost"));
    assert!(Arc::ptr_eq(&identity.certified, &narrowed.certified));
    assert!(Arc::ptr_eq(&identity.key, &narrowed.key));
    assert_eq!(identity.validity(), narrowed.validity());
    assert_eq!(
        narrowed.check_validity(Some(END + 1)),
        Err(TlsError::Verification(VerificationFailure::Expired))
    );
    for names in [
        &[][..],
        &["localhost", "LOCALHOST"][..],
        &["unused.example.test"][..],
    ] {
        assert_eq!(
            identity.restrict_names(names).err(),
            Some(TlsError::Invalid)
        );
    }
    assert_eq!(
        narrowed.restrict_names(&["mail.example.test"]).err(),
        Some(TlsError::Invalid)
    );
    assert!(identity.retire_for_test().is_err());
    assert_eq!(narrowed.check_validity(Some(NOW)), Err(TlsError::Crypto));
    assert_eq!(
        identity.restrict_names(&["localhost"]).err(),
        Some(TlsError::Crypto)
    );
}

#[test]
fn identity_refuses_wrong_key_name_time_and_order() {
    let fixture = Fixture::new();
    let chain = fixture.chain(&Parameters::new(false), &Parameters::new(true));
    let (wrong, _) = key();
    assert_eq!(
        ServerIdentity::from_pem(
            &chain_pem(&chain),
            &pem("PRIVATE KEY", wrong.as_ref()),
            &["localhost"],
            Some(NOW)
        )
        .err(),
        Some(TlsError::KeyMismatch)
    );
    assert_eq!(
        fixture.admit(&chain, &["other.example"], Some(NOW)).err(),
        Some(TlsError::Verification(VerificationFailure::Name))
    );
    assert_eq!(
        fixture.admit(&chain, &["localhost"], None).err(),
        Some(TlsError::Clock)
    );
    for (now, reason) in [
        (START - 1, VerificationFailure::NotYetValid),
        (END + 1, VerificationFailure::Expired),
    ] {
        assert_eq!(
            fixture.admit(&chain, &["localhost"], Some(now)).err(),
            Some(TlsError::Verification(reason))
        );
    }
    let mut root = Parameters::new(true);
    root.not_after = b"260101000000Z".to_vec();
    assert_eq!(
        fixture
            .admit(
                &fixture.chain(&Parameters::new(false), &root),
                &["localhost"],
                Some(NOW)
            )
            .err(),
        Some(TlsError::Verification(VerificationFailure::Expired))
    );
    assert_eq!(
        fixture
            .admit(
                &[chain[0].clone(), chain[0].clone()],
                &["localhost"],
                Some(NOW)
            )
            .err(),
        Some(TlsError::Invalid)
    );
    // A CA cannot become the signing leaf just by appearing first.
    assert_eq!(
        fixture
            .admit(
                &[chain[1].clone(), chain[0].clone()],
                &["localhost"],
                Some(NOW)
            )
            .err(),
        Some(TlsError::Verification(VerificationFailure::Usage))
    );
    for bad_index in 0..2 {
        let mut corrupt = chain.clone();
        *corrupt[bad_index].last_mut().unwrap() ^= 1;
        assert_eq!(
            fixture.admit(&corrupt, &["localhost"], Some(NOW)).err(),
            Some(TlsError::Verification(VerificationFailure::Signature))
        );
    }
    let mut other_issuer = Parameters::new(false);
    other_issuer.issuer = b"other-root".to_vec();
    assert_eq!(
        fixture
            .admit(
                &fixture.chain(&other_issuer, &Parameters::new(true)),
                &["localhost"],
                Some(NOW)
            )
            .err(),
        Some(TlsError::Invalid)
    );
}

#[test]
fn local_identity_enforces_key_usage_and_extensions() {
    let fixture = Fixture::new();
    let root = Parameters::new(true);
    let mut not_ca = root.clone();
    not_ca.extensions[0] = extension(0x13, true, seq(&[]));
    let mut client_ca = root.clone();
    client_ca.extensions.push(extension(
        0x25,
        false,
        seq(&[oid(&[0x2b, 6, 1, 5, 5, 7, 3, 2])]),
    ));
    for issuer in [not_ca, client_ca] {
        assert_eq!(
            fixture
                .admit(
                    &fixture.chain(&Parameters::new(false), &issuer),
                    &["localhost"],
                    Some(NOW)
                )
                .err(),
            Some(TlsError::Verification(VerificationFailure::Usage))
        );
    }
    let mut leaf = Parameters::new(false);
    leaf.extensions[1] = extension(0x0f, true, der(3, &[5, 0x20]));
    assert_eq!(
        fixture
            .admit(&fixture.chain(&leaf, &root), &["localhost"], Some(NOW))
            .err(),
        Some(TlsError::Verification(VerificationFailure::Usage))
    );
    leaf.extensions[1] = extension(0x0f, true, der(3, &[2, 0x84]));
    assert_eq!(
        fixture
            .admit(&fixture.chain(&leaf, &root), &["localhost"], Some(NOW))
            .err(),
        Some(TlsError::Verification(VerificationFailure::Usage))
    );
    for bits in [&[0, 0x80][..], &[7, 0], &[8, 0], &[0], &[0, 1, 1]] {
        leaf.extensions[1] = extension(0x0f, true, der(3, bits));
        assert_eq!(
            fixture
                .admit(&fixture.chain(&leaf, &root), &["localhost"], Some(NOW))
                .err(),
            Some(TlsError::Invalid)
        );
    }
    let mut leaf = Parameters::new(false);
    leaf.extensions[3] = extension(0x25, false, seq(&[oid(&[0x2b, 6, 1, 5, 5, 7, 3, 2])]));
    assert_eq!(
        fixture
            .admit(&fixture.chain(&leaf, &root), &["localhost"], Some(NOW))
            .err(),
        Some(TlsError::Verification(VerificationFailure::Usage))
    );
    leaf.extensions = vec![leaf.extensions[0].clone(), leaf.extensions[2].clone()];
    assert!(fixture
        .admit(&fixture.chain(&leaf, &root), &["localhost"], Some(NOW))
        .is_ok());
    let mut root = Parameters::new(true);
    root.extensions[1] = extension(0x0f, true, der(3, &[7, 0x80]));
    assert_eq!(
        fixture
            .admit(&fixture.chain(&leaf, &root), &["localhost"], Some(NOW))
            .err(),
        Some(TlsError::Verification(VerificationFailure::Usage))
    );
    let mut leaf = Parameters::new(false);
    leaf.extensions.push(extension(0x7f, false, der(0x05, &[])));
    assert!(fixture
        .admit(
            &fixture.chain(&leaf, &Parameters::new(true)),
            &["localhost"],
            Some(NOW)
        )
        .is_ok());
    leaf.extensions
        .push(leaf.extensions.last().unwrap().clone());
    assert_eq!(
        fixture
            .admit(
                &fixture.chain(&leaf, &Parameters::new(true)),
                &["localhost"],
                Some(NOW)
            )
            .err(),
        Some(TlsError::Invalid)
    );
    leaf.extensions = Parameters::new(false).extensions;
    leaf.extensions.push(extension(0x7f, true, der(0x05, &[])));
    assert_eq!(
        fixture
            .admit(
                &fixture.chain(&leaf, &Parameters::new(true)),
                &["localhost"],
                Some(NOW)
            )
            .err(),
        Some(TlsError::Invalid)
    );
}

#[test]
fn local_identity_name_and_extension_limits() {
    let fixture = Fixture::new();
    let root = Parameters::new(true);
    let basic = fixture.chain(&Parameters::new(false), &root);
    for name in [
        "",
        "localhost.",
        "*.example.com",
        "127.0.0.1",
        "::1",
        "_host.example",
        "-host.example",
        "host-.example",
        "x..example",
        "máil.example",
        "x\0.example",
        "x.123",
    ] {
        assert_eq!(
            fixture.admit(&basic, &[name], Some(NOW)).err(),
            Some(TlsError::Invalid),
            "{name:?}"
        );
    }
    assert_eq!(
        fixture.admit(&basic, &[], Some(NOW)).err(),
        Some(TlsError::Invalid)
    );
    assert_eq!(
        fixture
            .admit(&basic, &["LOCALHOST", "localhost"], Some(NOW))
            .err(),
        Some(TlsError::Invalid)
    );
    let mut leaf = Parameters::new(false);
    leaf.extensions[2] = extension(0x11, false, seq(&[der(0x82, b"*.example.com")]));
    let chain = fixture.chain(&leaf, &root);
    let names: Vec<_> = (0..33).map(|n| format!("n{n}.example.com")).collect();
    let names: Vec<_> = names.iter().map(String::as_str).collect();
    assert_eq!(
        fixture
            .admit(&chain, &names[..32], Some(NOW))
            .unwrap()
            .name_count(),
        32
    );
    assert_eq!(
        fixture.admit(&chain, &names, Some(NOW)).err(),
        Some(TlsError::Invalid)
    );
    assert_eq!(
        fixture
            .admit(&chain, &["deep.name.example.com"], Some(NOW))
            .err(),
        Some(TlsError::Verification(VerificationFailure::Name))
    );
    let exact = [
        "a".repeat(63),
        "b".repeat(63),
        "c".repeat(63),
        "d".repeat(61),
    ]
    .join(".");
    assert_eq!(exact.len(), 253);
    leaf.extensions[2] = extension(0x11, false, seq(&[der(0x82, exact.as_bytes())]));
    let chain = fixture.chain(&leaf, &root);
    assert!(fixture.admit(&chain, &[&exact], Some(NOW)).is_ok());
    assert_eq!(
        fixture.admit(&chain, &[&(exact + "e")], Some(NOW)).err(),
        Some(TlsError::Invalid)
    );

    let mut leaf = Parameters::new(false);
    for n in 0..60 {
        leaf.extensions.push(seq(&[
            oid(&[0x2b, 6, 1, 4, 1, 0x7f, n]),
            der(4, &der(5, &[])),
        ]));
    }
    assert_eq!(leaf.extensions.len(), 64);
    assert!(fixture
        .admit(&fixture.chain(&leaf, &root), &["localhost"], Some(NOW))
        .is_ok());
    leaf.extensions.push(seq(&[
        oid(&[0x2b, 6, 1, 4, 1, 0x7f, 60]),
        der(4, &der(5, &[])),
    ]));
    assert_eq!(
        fixture
            .admit(&fixture.chain(&leaf, &root), &["localhost"], Some(NOW))
            .err(),
        Some(TlsError::Invalid)
    );
}

#[test]
fn local_identity_chain_constraints_and_maximum_depth() {
    let fixture = Fixture::new();
    let (_, replacement) = key();
    let mut rollover = Parameters::new(true);
    rollover.serial = 3;
    let issued = fixture::make(&fixture.leaf, &replacement, &Parameters::new(false)).unwrap();
    let rollover = fixture::make(&replacement, &fixture.root, &rollover).unwrap();
    // Self-issued is not necessarily self-signed. This subset requires the
    // actual issuer of a rollover tail to be supplied.
    assert_eq!(
        fixture
            .admit(
                &[issued.clone(), rollover.clone()],
                &["localhost"],
                Some(NOW)
            )
            .err(),
        Some(TlsError::Verification(VerificationFailure::Signature))
    );
    let root = fixture::certificate(&fixture.root, &fixture.root, true).unwrap();
    assert!(fixture
        .admit(&[issued, rollover, root], &["localhost"], Some(NOW))
        .is_ok());
    // The backend counts a self-issued rollover against a non-anchor limit.
    let (_, outer) = key();
    let mut ca = Parameters::new(true);
    ca.issuer = b"outer-root".to_vec();
    ca.extensions[0] = extension(0x13, true, seq(&[der(1, &[0xff]), der(2, &[0])]));
    let mut outer_parameters = Parameters::new(true);
    outer_parameters.subject = ca.issuer.clone();
    outer_parameters.issuer = ca.issuer.clone();
    let issued = fixture::make(&fixture.leaf, &replacement, &Parameters::new(false)).unwrap();
    let mut rollover_parameters = Parameters::new(true);
    rollover_parameters.serial = 3;
    let rollover = fixture::make(&replacement, &fixture.root, &rollover_parameters).unwrap();
    let mut path = vec![
        issued,
        rollover,
        fixture::make(&fixture.root, &outer, &ca).unwrap(),
        fixture::make(&outer, &outer, &outer_parameters).unwrap(),
    ];
    assert!(matches!(
        fixture.admit(&path, &["localhost"], Some(NOW)).err(),
        Some(TlsError::Verification(
            VerificationFailure::Other | VerificationFailure::Signature
        ))
    ));
    ca.extensions[0] = extension(0x13, true, seq(&[der(1, &[0xff]), der(2, &[1])]));
    path[2] = fixture::make(&fixture.root, &outer, &ca).unwrap();
    assert!(fixture.admit(&path, &["localhost"], Some(NOW)).is_ok());
    let mut partial = Parameters::new(true);
    partial.issuer = b"omitted-root".to_vec();
    partial.not_before = b"260101000000Z".to_vec();
    partial.not_after = b"300101000000Z".to_vec();
    let partial_chain = fixture.chain(&Parameters::new(false), &partial);
    assert_eq!(
        fixture
            .admit(&partial_chain, &["localhost"], Some(NOW))
            .unwrap()
            .validity(),
        (1767225600, 1893456000)
    );
    let mut expired = partial.clone();
    expired.not_after = expired.not_before.clone();
    assert_eq!(
        fixture
            .admit(
                &fixture.chain(&Parameters::new(false), &expired),
                &["localhost"],
                Some(NOW)
            )
            .err(),
        Some(TlsError::Verification(VerificationFailure::Expired))
    );
    partial.extensions.push(extension(
        0x1e,
        true,
        seq(&[der(0xa0, &seq(&[der(0x82, b"example.com")]))]),
    ));
    assert_eq!(
        fixture
            .admit(
                &fixture.chain(&Parameters::new(false), &partial),
                &["localhost"],
                Some(NOW)
            )
            .err(),
        Some(TlsError::Verification(VerificationFailure::Other))
    );

    // A repeated issuer name/key could let path building bypass its constraints.
    let mut constrained = Parameters::new(true);
    constrained.serial = 3;
    constrained.extensions.push(extension(
        0x1e,
        true,
        seq(&[der(0xa0, &seq(&[der(0x82, b"example.com")]))]),
    ));
    let basic = fixture.chain(&Parameters::new(false), &Parameters::new(true));
    let repeated = fixture::make(&fixture.root, &fixture.root, &constrained).unwrap();
    assert_eq!(
        fixture
            .admit(
                &[basic[0].clone(), repeated, basic[1].clone()],
                &["localhost"],
                Some(NOW)
            )
            .err(),
        Some(TlsError::Invalid)
    );

    let point = fixture.root.public_key().as_ref();
    let compressed = [vec![2 | (point[64] & 1)], point[1..33].to_vec()].concat();
    let spki = seq(&[
        der(0x30, certificate_algorithms::P256_KEY),
        der(3, &[vec![0], compressed].concat()),
    ]);
    let plain_root = fixture::build(
        spki.clone(),
        fixture::signature_algorithm(),
        &Parameters::new(true),
        |body| {
            Ok(fixture
                .root
                .sign(&aws_lc_rs::rand::SystemRandom::new(), body)?
                .as_ref()
                .to_vec())
        },
    )
    .unwrap();
    assert!(fixture
        .admit(&[basic[0].clone(), plain_root], &["localhost"], Some(NOW))
        .is_ok());
    let repeated = fixture::build(spki, fixture::signature_algorithm(), &constrained, |body| {
        Ok(fixture
            .root
            .sign(&aws_lc_rs::rand::SystemRandom::new(), body)?
            .as_ref()
            .to_vec())
    })
    .unwrap();
    assert_eq!(
        fixture
            .admit(
                &[basic[0].clone(), repeated, basic[1].clone()],
                &["localhost"],
                Some(NOW)
            )
            .err(),
        Some(TlsError::Invalid)
    );
    let mut chain = Vec::new();
    let mut leaf = Parameters::new(false);
    leaf.issuer = b"issuer-1".to_vec();
    chain.push(fixture::make(&fixture.leaf, &fixture.root, &leaf).unwrap());
    for n in 1..=7 {
        let mut issuer = Parameters::new(true);
        issuer.subject = format!("issuer-{n}").into_bytes();
        issuer.issuer = format!("issuer-{}", (n + 1).min(7)).into_bytes();
        issuer.serial = n + 2;
        chain.push(fixture::make(&fixture.root, &fixture.root, &issuer).unwrap());
    }
    assert_eq!(
        fixture
            .admit(&chain, &["localhost"], Some(NOW))
            .unwrap()
            .certificate_count(),
        8
    );
    let mut wrong_order = chain.clone();
    wrong_order.swap(1, 2);
    assert_eq!(
        fixture.admit(&wrong_order, &["localhost"], Some(NOW)).err(),
        Some(TlsError::Invalid)
    );
    let mut root = Parameters::new(true);
    root.subject = b"issuer-7".to_vec();
    root.issuer = root.subject.clone();
    root.extensions[0] = extension(0x13, true, seq(&[der(1, &[0xff]), der(2, &[5])]));
    chain[7] = fixture::make(&fixture.root, &fixture.root, &root).unwrap();
    assert_eq!(
        fixture.admit(&chain, &["localhost"], Some(NOW)).err(),
        Some(TlsError::Verification(VerificationFailure::Usage))
    );
    root.extensions[0] = extension(0x13, true, seq(&[der(1, &[0xff]), der(2, &[6])]));
    chain[7] = fixture::make(&fixture.root, &fixture.root, &root).unwrap();
    assert!(fixture.admit(&chain, &["localhost"], Some(NOW)).is_ok());
    // A supplied anchor's constraints must still constrain the local leaf.
    let mut root = Parameters::new(true);
    let permitted = der(0xa0, &seq(&[der(0x82, b"example.com")]));
    root.extensions
        .push(extension(0x1e, true, seq(&[permitted])));
    let chain = fixture.chain(&Parameters::new(false), &root);
    assert_eq!(
        fixture.admit(&chain, &["localhost"], Some(NOW)).err(),
        Some(TlsError::Verification(VerificationFailure::Other))
    );
}

#[test]
fn local_identity_accepts_rsa_issuer_and_refuses_unsupported_algorithms() {
    use aws_lc_rs::signature::RSA_PKCS1_SHA256;
    let fixture = Fixture::new();
    let rsa = aws_lc_rs::rsa::KeyPair::generate(aws_lc_rs::rsa::KeySize::Rsa2048).unwrap();
    let rsa_algorithm = seq(&[
        oid(&[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 1, 1, 1]),
        der(5, &[]),
    ]);
    let signature = seq(&[
        oid(&[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 1, 1, 11]),
        der(5, &[]),
    ]);
    let public = [b"\0".as_slice(), rsa.public_key().as_ref()].concat();
    let spki = seq(&[rsa_algorithm, der(3, &public)]);
    let sign = |body: &[u8]| -> fixture::Result<Vec<u8>> {
        let mut signature = vec![0; rsa.public_modulus_len()];
        rsa.sign(
            &RSA_PKCS1_SHA256,
            &aws_lc_rs::rand::SystemRandom::new(),
            body,
            &mut signature,
        )?;
        Ok(signature)
    };
    let root = fixture::build(spki, signature.clone(), &Parameters::new(true), sign).unwrap();
    let leaf = fixture::build(
        fixture::p256_spki(&fixture.leaf),
        signature,
        &Parameters::new(false),
        sign,
    )
    .unwrap();
    assert!(fixture
        .admit(&[leaf, root], &["localhost"], Some(NOW))
        .is_ok());
    let unsupported = seq(&[oid(&[0x2a, 0x86, 0x48, 0xce, 0x3d, 4, 3, 1])]);
    let leaf = fixture::build(
        fixture::p256_spki(&fixture.leaf),
        unsupported,
        &Parameters::new(false),
        |body| {
            Ok(fixture
                .root
                .sign(&aws_lc_rs::rand::SystemRandom::new(), body)?
                .as_ref()
                .to_vec())
        },
    )
    .unwrap();
    let root = fixture::certificate(&fixture.root, &fixture.root, true).unwrap();
    assert_eq!(
        fixture
            .admit(&[leaf, root], &["localhost"], Some(NOW))
            .err(),
        Some(TlsError::Invalid)
    );
}

#[test]
fn identity_admission_unwind_drops_unpublished_state() {
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };
    struct DropFlag(Arc<AtomicBool>);
    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    let fixture = Fixture::new();
    let chain = fixture.chain(&Parameters::new(false), &Parameters::new(true));
    let dropped = Arc::new(AtomicBool::new(false));
    let observe = dropped.clone();
    let result = admission_boundary(|| {
        let _identity = fixture.admit(&chain, &["localhost"], Some(NOW))?;
        let _guard = DropFlag(observe);
        panic!("synthetic identity construction unwind");
    });
    assert_eq!(result.err(), Some(TlsError::Crypto));
    assert!(dropped.load(Ordering::SeqCst));
    // Returned-error redaction and Rust cleanup are not native abort/OOM tests.
    for error in [
        TlsError::Invalid,
        TlsError::KeyMismatch,
        TlsError::Verification(VerificationFailure::Name),
        TlsError::Crypto,
    ] {
        assert!(std::error::Error::source(&error).is_none());
        assert!(!error.to_string().contains("localhost"));
    }
}

#[test]
fn local_identity_metadata_bounds_and_malformed_values() {
    let fixture = Fixture::new();
    let root = Parameters::new(true);
    let mut leaf = Parameters::new(false);
    leaf.not_after = b"20550101000000Z".to_vec();
    assert_eq!(
        fixture
            .admit(&fixture.chain(&leaf, &root), &["localhost"], Some(NOW))
            .unwrap()
            .validity(),
        (START, END)
    );
    for date in [
        b"250229000000Z".as_slice(),
        b"240101000000Z",
        b"500101000000Z",
    ] {
        leaf.not_after = date.to_vec();
        assert_eq!(
            fixture
                .admit(&fixture.chain(&leaf, &root), &["localhost"], Some(NOW))
                .err(),
            Some(TlsError::Invalid)
        );
    }
    for constraint in [seq(&[der(1, &[0])]), seq(&[der(2, &[1])])] {
        let mut leaf = Parameters::new(false);
        leaf.extensions[0] = extension(0x13, true, constraint);
        assert_eq!(
            fixture
                .admit(&fixture.chain(&leaf, &root), &["localhost"], Some(NOW))
                .err(),
            Some(TlsError::Invalid)
        );
    }
    for contents in [
        seq(&[]),
        seq(&[oid(&[])]),
        seq(&[oid(&[0x80, 0])]),
        seq(&[oid(&[0x2b, 0x81])]),
    ] {
        let mut leaf = Parameters::new(false);
        leaf.extensions[3] = extension(0x25, false, contents);
        assert_eq!(
            fixture
                .admit(&fixture.chain(&leaf, &root), &["localhost"], Some(NOW))
                .err(),
            Some(TlsError::Invalid)
        );
    }
    let chain = fixture.chain(&Parameters::new(false), &root);
    for length in 0..chain[0].len() {
        assert_eq!(
            Certificate::parse(&chain[0][..length]).err(),
            Some(TlsError::Invalid)
        );
    }
    for entries in [
        vec![der(0x89, b"bad"), der(0x82, b"localhost")],
        vec![der(0x82, b"localhost"), der(0x89, b"bad")],
        vec![der(0x82, b"localhost"), vec![0x82, 5, b'x']],
        vec![der(0x82, b"localhost"), der(0x82, &[0xff])],
    ] {
        let mut leaf = Parameters::new(false);
        leaf.extensions[2] = extension(0x11, false, seq(&entries));
        assert_eq!(
            fixture
                .admit(&fixture.chain(&leaf, &root), &["localhost"], Some(NOW))
                .err(),
            Some(TlsError::Invalid)
        );
    }
    let public = fixture.leaf.public_key().as_ref();
    let compressed = [vec![2 | (public[64] & 1)], public[1..33].to_vec()].concat();
    for point in [compressed, vec![4; 64], vec![5; 65]] {
        let spki = seq(&[
            der(0x30, certificate_algorithms::P256_KEY),
            der(3, &[vec![0], point].concat()),
        ]);
        let leaf = fixture::build(
            spki,
            fixture::signature_algorithm(),
            &Parameters::new(false),
            |body| {
                Ok(fixture
                    .root
                    .sign(&aws_lc_rs::rand::SystemRandom::new(), body)?
                    .as_ref()
                    .to_vec())
            },
        )
        .unwrap();
        assert_eq!(
            fixture.admit(&[leaf], &["localhost"], Some(NOW)).err(),
            Some(TlsError::Invalid)
        );
    }
    // These are synthetic RSA public encodings: they test bit-size admission,
    // not RSA validity or successful cryptographic verification.
    for (bits, accepted) in [
        (2047usize, false),
        (2048, true),
        (8192, true),
        (8193, false),
    ] {
        let mut modulus = vec![0xff; bits.div_ceil(8)];
        modulus[0] = ((1u16 << ((bits - 1) % 8 + 1)) - 1) as u8;
        if modulus[0] & 0x80 != 0 {
            modulus.insert(0, 0);
        }
        let public = seq(&[der(2, &modulus), der(2, &[1, 0, 1])]);
        let rsa_algorithm = seq(&[
            oid(&[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 1, 1, 1]),
            der(5, &[]),
        ]);
        let spki = seq(&[rsa_algorithm, der(3, &[b"\0".as_slice(), &public].concat())]);
        let certificate = fixture::build(spki, fixture::signature_algorithm(), &root, |body| {
            Ok(fixture
                .root
                .sign(&aws_lc_rs::rand::SystemRandom::new(), body)?
                .as_ref()
                .to_vec())
        })
        .unwrap();
        assert_eq!(
            Certificate::parse(&certificate).unwrap().rsa_size().is_ok(),
            accepted
        );
    }
}
#[test]
fn retained_tls_identity_shares_key_lifecycle() {
    let fixture = Fixture::new();
    let chain = fixture.chain(&Parameters::new(false), &Parameters::new(true));
    let identity = fixture.admit(&chain, &["localhost"], Some(NOW)).unwrap();
    identity.certified.keys_match().unwrap();
    let signer = identity
        .certified
        .key
        .choose_scheme(&[rustls::SignatureScheme::ECDSA_NISTP256_SHA256])
        .unwrap();
    signer.sign(b"identity retained signer").unwrap();
    assert_eq!(
        identity
            .key
            .sign_es256_with::<()>(b"retire", |_| Err(Error::Crypto)),
        Err(Error::Crypto)
    );
    assert_eq!(identity.check_validity(Some(NOW)), Err(TlsError::Crypto));
    assert!(signer.sign(b"after retirement").is_err());
    assert_eq!(identity.certificate_der(0), Some(chain[0].as_slice()));
}

#[test]
fn retained_tls_identity_completes_handshakes_and_survives_remote_refusal() {
    use rustls::{pki_types::UnixTime, SignatureScheme};
    use std::sync::Arc;
    #[derive(Debug)]
    struct Clock;
    impl rustls::time_provider::TimeProvider for Clock {
        fn current_time(&self) -> Option<UnixTime> {
            Some(UnixTime::since_unix_epoch(std::time::Duration::from_secs(
                NOW,
            )))
        }
    }
    let fixture = Fixture::new();
    let chain = fixture.chain(&Parameters::new(false), &Parameters::new(true));
    let root = chain.last().unwrap().clone();
    let identity = fixture.admit(&chain, &["localhost"], Some(NOW)).unwrap();
    let key = identity.key.clone();
    let certified = identity.certified.clone();
    let provider = Arc::new(crate::tls_policy::provider().unwrap());
    let mut configurations = Vec::new();
    for version in [&rustls::version::TLS12, &rustls::version::TLS13] {
        let mut server =
            rustls::ServerConfig::builder_with_details(provider.clone(), Arc::new(Clock))
                .with_protocol_versions(&[version])
                .unwrap()
                .with_no_client_auth()
                .with_cert_resolver(Arc::new(rustls::sign::SingleCertAndKey::from(
                    certified.clone(),
                )));
        server.session_storage = Arc::new(rustls::server::NoServerSessionStorage {});
        server.send_tls13_tickets = 0;
        let server = Arc::new(server);
        let mut roots = rustls::RootCertStore::empty();
        roots.add(root.clone().into()).unwrap();
        let mut client =
            rustls::ClientConfig::builder_with_details(provider.clone(), Arc::new(Clock))
                .with_protocol_versions(&[version])
                .unwrap()
                .with_root_certificates(roots)
                .with_no_client_auth();
        client.resumption = rustls::client::Resumption::disabled();
        let client = Arc::new(client);
        configurations.push((client.clone(), server.clone()));
        for wrong in [true, false] {
            let name = if wrong { "wrong.example" } else { "localhost" };
            let mut client = rustls::Connection::Client(
                rustls::ClientConnection::new(client.clone(), name.try_into().unwrap()).unwrap(),
            );
            let mut server =
                rustls::Connection::Server(rustls::ServerConnection::new(server.clone()).unwrap());
            client.set_buffer_limit(Some(32 * 1024));
            server.set_buffer_limit(Some(32 * 1024));
            let result = crate::tls_smoke::drive(&mut client, &mut server);
            if wrong {
                assert!(matches!(
                    result.unwrap_err().downcast_ref::<rustls::Error>(),
                    Some(rustls::Error::InvalidCertificate(
                        rustls::CertificateError::NotValidForNameContext { .. }
                    ))
                ));
                let mut alert = [0; 32 * 1024];
                let length = client
                    .write_tls(&mut std::io::Cursor::new(alert.as_mut_slice()))
                    .unwrap();
                assert!(length > 0 && !client.wants_write());
                assert_eq!(
                    server
                        .read_tls(&mut std::io::Cursor::new(&alert[..length]))
                        .unwrap(),
                    length
                );
                assert!(matches!(
                    server.process_new_packets(),
                    Err(rustls::Error::AlertReceived(
                        rustls::AlertDescription::BadCertificate
                    ))
                ));
                identity.check_validity(Some(NOW)).unwrap();
            } else {
                result.unwrap();
                assert!(!client.is_handshaking() && !server.is_handshaking());
                assert_eq!(client.protocol_version(), Some(version.version));
            }
        }
        let mut refused = rustls::ServerConnection::new(server).unwrap();
        assert_eq!(
            refused
                .read_tls(&mut std::io::Cursor::new([0xff, 3, 3, 0, 1, 0]))
                .unwrap(),
            6
        );
        assert!(matches!(
            refused.process_new_packets(),
            Err(rustls::Error::InvalidMessage(
                rustls::InvalidMessage::InvalidContentType
            ))
        ));
        let signer = certified
            .key
            .choose_scheme(&[SignatureScheme::ECDSA_NISTP256_SHA256])
            .unwrap();
        signer.sign(b"after remote refusal").unwrap();
    }
    assert_eq!(
        key.sign_es256_with::<()>(b"retire shared signing key", |_| Err(Error::Crypto)),
        Err(Error::Crypto)
    );
    for (client, server) in configurations {
        let mut client = rustls::Connection::Client(
            rustls::ClientConnection::new(client, "localhost".try_into().unwrap()).unwrap(),
        );
        let mut server = rustls::Connection::Server(rustls::ServerConnection::new(server).unwrap());
        client.set_buffer_limit(Some(32 * 1024));
        server.set_buffer_limit(Some(32 * 1024));
        let error = crate::tls_smoke::drive(&mut client, &mut server).unwrap_err();
        match error.downcast_ref::<rustls::Error>().unwrap() {
            rustls::Error::Other(rustls::OtherError(error)) => {
                assert_eq!(error.downcast_ref::<TlsError>(), Some(&TlsError::Crypto))
            }
            _ => panic!("unexpected retired key handshake error"),
        }
    }
}

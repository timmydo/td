//! Generated local certificates shared by backend and admission tests.
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]
use crate::Crypto;
use td_primitives::ec::p256;
pub(super) type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// A P-256 test key over td-primitives, from a PKCS#8 document the provider
/// generated: its public point and DER SHA-256 ECDSA signing.
pub(super) struct TestKey {
    secret: p256::SecretKey,
    public: [u8; 65],
}

impl TestKey {
    pub fn generate() -> Self {
        let mut document = [0u8; crate::P256_PKCS8_CAPACITY];
        let length = crate::Provider.generate_p256(&mut document).unwrap();
        Self::from_pkcs8(document.get(..length).unwrap()).unwrap()
    }

    pub fn generate_pkcs8() -> Result<Vec<u8>> {
        let mut document = [0u8; crate::P256_PKCS8_CAPACITY];
        let length = crate::Provider.generate_p256(&mut document)?;
        Ok(document.get(..length).ok_or("generated length")?.to_vec())
    }

    pub fn from_pkcs8(document: &[u8]) -> Result<Self> {
        let parts = crate::pkcs8::parse(document)?;
        let secret = p256::SecretKey::from_bytes(parts.scalar)?;
        let public = secret.public_key()?;
        Ok(Self { secret, public })
    }

    pub fn public_key(&self) -> &[u8; 65] {
        &self.public
    }

    pub fn sign(&self, body: &[u8]) -> Result<Vec<u8>> {
        Ok(self.secret.sign_sha256(body)?)
    }
}

/// td-tls-provider's big-integer signature rows: name, public key, message and
/// signature, each hex.
pub(super) const SIGNATURE_ANSWERS: &str =
    include_str!("../../td-tls-provider/tests/signature_answers.txt");

pub(super) fn unhex(text: &str) -> Vec<u8> {
    text.as_bytes()
        .chunks(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}

/// The public key of the first signature row named `name`.
pub(super) fn answer_public(name: &str) -> Vec<u8> {
    let line = SIGNATURE_ANSWERS
        .lines()
        .find(|line| line.split(' ').next() == Some(name))
        .unwrap();
    unhex(line.split(' ').nth(1).unwrap())
}

/// Certificate paths AWS-LC wrote while it was the backend: under each
/// algorithm it signs with, a root and the P-256 leaf it signed, whose key is
/// `LEAF_KEY`; EC roots appear twice, their key uncompressed then compressed.
pub(super) const CERTIFICATE_PATHS: &str = include_str!("fixtures/certificate_paths.txt");

/// The root and leaf of the first committed path signed under `name`.
pub(super) fn certificate_path(name: &str) -> (Vec<u8>, Vec<u8>) {
    let line = CERTIFICATE_PATHS
        .lines()
        .find(|line| line.split(' ').nth(1) == Some(name))
        .unwrap();
    let fields: Vec<&str> = line.split(' ').collect();
    (unhex(fields[2]), unhex(fields[3]))
}

/// The committed paths' P-256 leaf key, PKCS#8 as AWS-LC generated it.
pub(super) fn path_leaf_key() -> Vec<u8> {
    let line = CERTIFICATE_PATHS
        .lines()
        .find(|line| line.starts_with("LEAF_KEY "))
        .unwrap();
    unhex(line.split(' ').nth(1).unwrap())
}

/// A DER ECDSA signature's two INTEGERs as fixed-width r || s.
pub(super) fn fixed_signature(der: &[u8]) -> Option<[u8; 64]> {
    let body = der.strip_prefix(&[0x30])?;
    let (&length, mut body) = body.split_first()?;
    if body.len() != usize::from(length) {
        return None;
    }
    let mut out = [0u8; 64];
    for half in out.chunks_mut(32) {
        let (&tag, rest) = body.split_first()?;
        let (&length, rest) = rest.split_first()?;
        let value = rest.get(..usize::from(length))?;
        body = rest.get(usize::from(length)..)?;
        let value = value.strip_prefix(&[0]).unwrap_or(value);
        if tag != 2 || value.len() > 32 {
            return None;
        }
        half.get_mut(32 - value.len()..)?.copy_from_slice(value);
    }
    body.is_empty().then_some(out)
}

/// td-fido's independent P-256 verifier over the engine's SHA-256: the
/// oracle for signatures td-primitives makes.
pub(super) fn oracle_verifies(public: &[u8; 65], message: &[u8], der: &[u8]) -> bool {
    let Some(signature) = fixed_signature(der) else {
        return false;
    };
    let (Some(x), Some(y)) = (public.get(1..33), public.get(33..)) else {
        return false;
    };
    let Ok(public) = crate::p256_oracle::PublicKey::from_coordinates(
        x.try_into().unwrap(),
        y.try_into().unwrap(),
    ) else {
        return false;
    };
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

pub(super) fn der(tag: u8, bytes: &[u8]) -> Vec<u8> {
    let mut out = vec![tag];
    if bytes.len() < 128 {
        out.push(bytes.len() as u8)
    } else {
        let length = bytes.len().to_be_bytes();
        let length = length
            .iter()
            .skip_while(|b| **b == 0)
            .copied()
            .collect::<Vec<_>>();
        out.push(0x80 | length.len() as u8);
        out.extend_from_slice(&length);
    }
    out.extend_from_slice(bytes);
    out
}
pub(super) fn seq(parts: &[Vec<u8>]) -> Vec<u8> {
    der(0x30, &parts.concat())
}
pub(super) fn oid(bytes: &[u8]) -> Vec<u8> {
    der(6, bytes)
}
pub(super) fn name(cn: &[u8]) -> Vec<u8> {
    seq(&[der(0x31, &seq(&[oid(&[0x55, 4, 3]), der(0x0c, cn)]))])
}
pub(super) fn signature_algorithm() -> Vec<u8> {
    seq(&[oid(&[0x2a, 0x86, 0x48, 0xce, 0x3d, 4, 3, 2])])
}
pub(super) fn extension(last: u8, critical: bool, value: Vec<u8>) -> Vec<u8> {
    let mut parts = vec![oid(&[0x55, 0x1d, last])];
    if critical {
        parts.push(der(1, &[0xff]))
    };
    parts.push(der(4, &value));
    seq(&parts)
}

#[derive(Clone)]
pub(super) struct Parameters {
    pub issuer: Vec<u8>,
    pub subject: Vec<u8>,
    pub serial: u8,
    pub not_before: Vec<u8>,
    pub not_after: Vec<u8>,
    pub extensions: Vec<Vec<u8>>,
}
impl Parameters {
    pub fn new(ca: bool) -> Self {
        let mut extensions = vec![
            extension(
                0x13,
                true,
                if ca {
                    seq(&[der(1, &[0xff])])
                } else {
                    seq(&[])
                },
            ),
            extension(0x0f, true, der(3, if ca { &[1, 6] } else { &[7, 0x80] })),
        ];
        if !ca {
            extensions.push(extension(0x11, false, seq(&[der(0x82, b"localhost")])));
            extensions.push(extension(
                0x25,
                false,
                seq(&[oid(&[0x2b, 6, 1, 5, 5, 7, 3, 1])]),
            ));
        }
        Self {
            issuer: b"td-test-root".to_vec(),
            subject: if ca {
                b"td-test-root".to_vec()
            } else {
                b"localhost".to_vec()
            },
            serial: if ca { 1 } else { 2 },
            not_before: b"250101000000Z".to_vec(),
            not_after: b"350101000000Z".to_vec(),
            extensions,
        }
    }
}

pub(super) fn build(
    spki: Vec<u8>,
    signature_algorithm: Vec<u8>,
    parameters: &Parameters,
    sign: impl FnOnce(&[u8]) -> Result<Vec<u8>>,
) -> Result<Vec<u8>> {
    let body = seq(&[
        der(0xa0, &der(2, &[2])),
        der(2, &[parameters.serial]),
        signature_algorithm.clone(),
        name(&parameters.issuer),
        seq(&[
            der(
                if parameters.not_before.len() == 15 {
                    0x18
                } else {
                    0x17
                },
                &parameters.not_before,
            ),
            der(
                if parameters.not_after.len() == 15 {
                    0x18
                } else {
                    0x17
                },
                &parameters.not_after,
            ),
        ]),
        name(&parameters.subject),
        spki,
        der(0xa3, &seq(&parameters.extensions)),
    ]);
    let mut bits = vec![0];
    bits.extend_from_slice(&sign(&body)?);
    Ok(seq(&[body, signature_algorithm, der(3, &bits)]))
}

pub(super) fn p256_spki(key: &TestKey) -> Vec<u8> {
    let mut public = vec![0];
    public.extend_from_slice(key.public_key().as_ref());
    seq(&[
        seq(&[
            oid(&[0x2a, 0x86, 0x48, 0xce, 0x3d, 2, 1]),
            oid(&[0x2a, 0x86, 0x48, 0xce, 0x3d, 3, 1, 7]),
        ]),
        der(3, &public),
    ])
}

pub(super) fn make(key: &TestKey, signer: &TestKey, parameters: &Parameters) -> Result<Vec<u8>> {
    build(p256_spki(key), signature_algorithm(), parameters, |body| {
        signer.sign(body)
    })
}

pub(super) fn certificate(key: &TestKey, signer: &TestKey, ca: bool) -> Result<Vec<u8>> {
    make(key, signer, &Parameters::new(ca))
}

pub(super) fn certificate_with(
    key: &TestKey,
    signer: &TestKey,
    ca: bool,
    usage: u8,
    issuer: &[u8],
    expires: &[u8],
    serial: u8,
) -> Result<Vec<u8>> {
    let mut parameters = Parameters::new(ca);
    parameters.issuer = issuer.to_vec();
    parameters.not_after = expires.to_vec();
    parameters.serial = serial;
    if !ca {
        if let Some(eku) = parameters.extensions.last_mut() {
            *eku = extension(0x25, false, seq(&[oid(&[0x2b, 6, 1, 5, 5, 7, 3, usage])]));
        }
    }
    make(key, signer, &parameters)
}

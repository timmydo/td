//! Generated local certificates shared by backend and admission tests.
use aws_lc_rs::signature::{EcdsaKeyPair, KeyPair};
pub(super) type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
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

pub(super) fn p256_spki(key: &EcdsaKeyPair) -> Vec<u8> {
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

pub(super) fn make(
    key: &EcdsaKeyPair,
    signer: &EcdsaKeyPair,
    parameters: &Parameters,
) -> Result<Vec<u8>> {
    build(p256_spki(key), signature_algorithm(), parameters, |body| {
        Ok(signer
            .sign(&aws_lc_rs::rand::SystemRandom::new(), body)?
            .as_ref()
            .to_vec())
    })
}

pub(super) fn certificate(key: &EcdsaKeyPair, signer: &EcdsaKeyPair, ca: bool) -> Result<Vec<u8>> {
    make(key, signer, &Parameters::new(ca))
}

pub(super) fn certificate_with(
    key: &EcdsaKeyPair,
    signer: &EcdsaKeyPair,
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

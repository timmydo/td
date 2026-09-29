//! Private TLS adapter retaining the owned P-256 key's failure fence.
use crate::{Crypto, Error, P256Key, Provider, TlsError};
use rustls::{
    pki_types::{CertificateDer, SubjectPublicKeyInfoDer},
    sign::{CertifiedKey, Signer, SigningKey},
    SignatureAlgorithm, SignatureScheme,
};
use std::sync::Arc;

struct Key {
    key: Arc<P256Key>,
    spki: [u8; 91],
}
struct Operation(Arc<P256Key>);

pub(super) fn certified_key(
    chain: Vec<Vec<u8>>,
    key: Arc<P256Key>,
) -> Result<CertifiedKey, TlsError> {
    std::panic::catch_unwind(|| {
        let mut point = [0; 65];
        Provider
            .p256_public(&key, &mut point)
            .map_err(|_| TlsError::Crypto)?;
        const PREFIX: [u8; 26] = [
            0x30, 0x59, 0x30, 0x13, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, 0x06,
            0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07, 0x03, 0x42, 0x00,
        ];
        let mut spki = [0; 91];
        spki.get_mut(..PREFIX.len())
            .ok_or(TlsError::Crypto)?
            .copy_from_slice(&PREFIX);
        spki.get_mut(PREFIX.len()..)
            .ok_or(TlsError::Crypto)?
            .copy_from_slice(&point);
        let mut certificates = Vec::new();
        certificates
            .try_reserve_exact(chain.len())
            .map_err(|_| TlsError::Capacity)?;
        for der in chain {
            certificates.push(CertificateDer::from(der));
        }
        let certified = CertifiedKey::new(certificates, Arc::new(Key { key, spki }));
        certified.keys_match().map_err(|_| TlsError::Crypto)?;
        Ok(certified)
    })
    .map_err(|_| TlsError::Crypto)?
}

impl std::fmt::Debug for Key {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TlsSigningKey(<redacted>)")
    }
}
impl std::fmt::Debug for Operation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TlsSigner(<redacted>)")
    }
}
impl SigningKey for Key {
    fn choose_scheme(&self, offered: &[SignatureScheme]) -> Option<Box<dyn Signer>> {
        if offered.contains(&SignatureScheme::ECDSA_NISTP256_SHA256) {
            Some(Box::new(Operation(self.key.clone())))
        } else {
            None
        }
    }
    fn public_key(&self) -> Option<SubjectPublicKeyInfoDer<'_>> {
        Some(SubjectPublicKeyInfoDer::from(self.spki.as_slice()))
    }
    fn algorithm(&self) -> SignatureAlgorithm {
        SignatureAlgorithm::ECDSA
    }
}
impl Signer for Operation {
    fn sign(&self, message: &[u8]) -> Result<Vec<u8>, rustls::Error> {
        self.0
            .sign_es256_with(message, der_signature)
            .map_err(|error| {
                let error = match error {
                    Error::Capacity => TlsError::Capacity,
                    Error::Invalid | Error::Entropy | Error::Crypto => TlsError::Crypto,
                };
                rustls::Error::Other(rustls::OtherError(Arc::new(error)))
            })
    }
    fn scheme(&self) -> SignatureScheme {
        SignatureScheme::ECDSA_NISTP256_SHA256
    }
}

fn der_signature(signature: &[u8; 64]) -> Result<Vec<u8>, Error> {
    let r = signature.get(..32).ok_or(Error::Crypto)?;
    let s = signature.get(32..).ok_or(Error::Crypto)?;
    let scalar = |bytes: &[u8]| -> Result<(usize, bool), Error> {
        let start = bytes
            .iter()
            .position(|byte| *byte != 0)
            .ok_or(Error::Crypto)?;
        let pad = bytes.get(start).ok_or(Error::Crypto)? & 0x80 != 0;
        Ok((start, pad))
    };
    let (r_start, r_pad) = scalar(r)?;
    let (s_start, s_pad) = scalar(s)?;
    let r = r.get(r_start..).ok_or(Error::Crypto)?;
    let s = s.get(s_start..).ok_or(Error::Crypto)?;
    let r_length = u8::try_from(r.len() + usize::from(r_pad)).map_err(|_| Error::Crypto)?;
    let s_length = u8::try_from(s.len() + usize::from(s_pad)).map_err(|_| Error::Crypto)?;
    let length = r_length
        .checked_add(s_length)
        .and_then(|n| n.checked_add(4))
        .ok_or(Error::Crypto)?;
    let mut result = Vec::new();
    result.try_reserve_exact(72).map_err(|_| Error::Capacity)?;
    result.extend_from_slice(&[0x30, length, 0x02, r_length]);
    if r_pad {
        result.push(0);
    }
    result.extend_from_slice(r);
    result.extend_from_slice(&[0x02, s_length]);
    if s_pad {
        result.push(0);
    }
    result.extend_from_slice(s);
    Ok(result)
}

#[cfg(test)]
#[path = "tls_signer_tests.rs"]
mod tests;

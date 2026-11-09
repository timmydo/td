//! Private, explicit TLS algorithms. Configurations must add the role policy.
use crate::{certificate_algorithms, TlsError};
use rustls::{
    crypto::{CryptoProvider, SupportedKxGroup},
    pki_types::SignatureVerificationAlgorithm,
    CipherSuite as Suite, NamedGroup, SignatureScheme as Scheme, SupportedCipherSuite,
};
use td_tls_provider::tls12_ecdsa;

type Mapping = (
    Scheme,
    &'static [&'static dyn SignatureVerificationAlgorithm],
);

const ADMITTED_MAPPINGS: &[(Scheme, &[usize])] = &[
    (Scheme::ECDSA_NISTP384_SHA384, &[4, 1, 7]),
    (Scheme::ECDSA_NISTP256_SHA256, &[0, 3, 6]),
    (Scheme::ECDSA_NISTP521_SHA512, &[8, 5, 2]),
    (Scheme::ED25519, &[9]),
    (Scheme::RSA_PSS_SHA512, &[12]),
    (Scheme::RSA_PSS_SHA384, &[11]),
    (Scheme::RSA_PSS_SHA256, &[10]),
    (Scheme::RSA_PKCS1_SHA512, &[15]),
    (Scheme::RSA_PKCS1_SHA384, &[14]),
    (Scheme::RSA_PKCS1_SHA256, &[13]),
];
const GROUPS: &[NamedGroup] = &[
    NamedGroup::X25519,
    NamedGroup::secp256r1,
    NamedGroup::secp384r1,
];

/// TLS.md's nine suites in preference order over td-tls-provider: its TLS 1.3
/// and ECDHE-RSA suites, and `tls12_ecdsa`'s ECDHE-ECDSA suites, which carry
/// rustls's full server scheme list.
fn suites(base: &CryptoProvider) -> Result<Vec<SupportedCipherSuite>, TlsError> {
    let find = |name| {
        base.cipher_suites
            .iter()
            .find(|suite| suite.suite() == name)
            .copied()
            .ok_or(TlsError::Crypto)
    };
    let suites = [
        find(Suite::TLS13_AES_256_GCM_SHA384)?,
        find(Suite::TLS13_AES_128_GCM_SHA256)?,
        find(Suite::TLS13_CHACHA20_POLY1305_SHA256)?,
        tls12_ecdsa::TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384,
        tls12_ecdsa::TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256,
        tls12_ecdsa::TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256,
        find(Suite::TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384)?,
        find(Suite::TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256)?,
        find(Suite::TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256)?,
    ];
    let mut selected = Vec::new();
    selected
        .try_reserve_exact(suites.len())
        .map_err(|_| TlsError::Capacity)?;
    selected.extend_from_slice(&suites);
    Ok(selected)
}

fn groups(base: &CryptoProvider) -> Result<Vec<&'static dyn SupportedKxGroup>, TlsError> {
    let mut selected = Vec::new();
    selected
        .try_reserve_exact(GROUPS.len())
        .map_err(|_| TlsError::Capacity)?;
    for name in GROUPS {
        let group = base
            .kx_groups
            .iter()
            .find(|group| group.name() == *name)
            .ok_or(TlsError::Crypto)?;
        selected.push(*group);
    }
    Ok(selected)
}

pub(super) fn provider() -> Result<CryptoProvider, TlsError> {
    std::panic::catch_unwind(|| {
        let base = td_tls_provider::provider();
        let mut algorithms = certificate_algorithms::SIGNATURE_ALGORITHMS;
        algorithms.all = certificate_algorithms::select(algorithms.all)?;
        algorithms.mapping = select_mapping(algorithms.all, algorithms.mapping)?;
        Ok(CryptoProvider {
            cipher_suites: suites(&base)?,
            kx_groups: groups(&base)?,
            signature_verification_algorithms: algorithms,
            ..base
        })
    })
    .map_err(|_| TlsError::Crypto)?
}

fn select_mapping<'a>(
    all: &[&'static dyn SignatureVerificationAlgorithm],
    mapping: &'a [Mapping],
) -> Result<&'a [Mapping], TlsError> {
    certificate_algorithms::select(all)?;
    if mapping.len() != ADMITTED_MAPPINGS.len() {
        return Err(TlsError::Crypto);
    }
    for ((scheme, algorithms), (expected_scheme, indexes)) in mapping.iter().zip(ADMITTED_MAPPINGS)
    {
        if scheme != expected_scheme || algorithms.len() != indexes.len() {
            return Err(TlsError::Crypto);
        }
        for (algorithm, index) in algorithms.iter().zip(*indexes) {
            let expected = all.get(*index).ok_or(TlsError::Crypto)?;
            if algorithm.public_key_alg_id() != expected.public_key_alg_id()
                || algorithm.signature_alg_id() != expected.signature_alg_id()
            {
                return Err(TlsError::Crypto);
            }
        }
    }
    Ok(mapping)
}

#[cfg(test)]
#[path = "tls_policy_tests.rs"]
mod tests;

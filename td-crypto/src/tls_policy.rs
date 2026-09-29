//! Private, explicit TLS algorithms. Configurations must add the role policy.
use crate::{certificate_algorithms, TlsError};
use rustls::{
    crypto::{aws_lc_rs as backend, CryptoProvider},
    pki_types::SignatureVerificationAlgorithm,
    SignatureScheme as Scheme,
};

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
const EXCLUDED_MAPPINGS: &[(Scheme, &[usize])] = &[
    (Scheme::ML_DSA_44, &[19]),
    (Scheme::ML_DSA_65, &[20]),
    (Scheme::ML_DSA_87, &[21]),
];

pub(super) fn provider() -> Result<CryptoProvider, TlsError> {
    std::panic::catch_unwind(|| {
        let mut provider = backend::default_provider();
        let algorithms = provider.signature_verification_algorithms;
        provider.signature_verification_algorithms.all =
            certificate_algorithms::select(algorithms.all)?;
        provider.signature_verification_algorithms.mapping =
            select_mapping(algorithms.all, algorithms.mapping)?;
        use backend::cipher_suite as c;
        let suites = [
            c::TLS13_AES_256_GCM_SHA384,
            c::TLS13_AES_128_GCM_SHA256,
            c::TLS13_CHACHA20_POLY1305_SHA256,
            c::TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384,
            c::TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256,
            c::TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256,
            c::TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384,
            c::TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256,
            c::TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256,
        ];
        let mut selected = Vec::new();
        selected
            .try_reserve_exact(suites.len())
            .map_err(|_| TlsError::Capacity)?;
        selected.extend_from_slice(&suites);
        provider.cipher_suites = selected;
        let groups = [
            backend::kx_group::X25519,
            backend::kx_group::SECP256R1,
            backend::kx_group::SECP384R1,
        ];
        let mut selected = Vec::new();
        selected
            .try_reserve_exact(groups.len())
            .map_err(|_| TlsError::Capacity)?;
        selected.extend_from_slice(&groups);
        provider.kx_groups = selected;
        Ok(provider)
    })
    .map_err(|_| TlsError::Crypto)?
}

fn select_mapping<'a>(
    all: &[&'static dyn SignatureVerificationAlgorithm],
    mapping: &'a [Mapping],
) -> Result<&'a [Mapping], TlsError> {
    certificate_algorithms::select(all)?;
    if mapping.len() != ADMITTED_MAPPINGS.len() + EXCLUDED_MAPPINGS.len() {
        return Err(TlsError::Crypto);
    }
    for ((scheme, algorithms), (expected_scheme, indexes)) in mapping
        .iter()
        .zip(ADMITTED_MAPPINGS.iter().chain(EXCLUDED_MAPPINGS))
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
    mapping
        .get(..ADMITTED_MAPPINGS.len())
        .ok_or(TlsError::Crypto)
}

#[cfg(test)]
#[path = "tls_policy_tests.rs"]
mod tests;

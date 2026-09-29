//! Fixed TLS categories. No backend diagnostic or input escapes this boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VerificationFailure {
    Missing,
    Untrusted,
    Expired,
    NotYetValid,
    Name,
    Usage,
    Signature,
    Other,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TlsError {
    Capacity,
    Invalid,
    KeyMismatch,
    Verification(VerificationFailure),
    Protocol,
    Clock,
    Crypto,
}

impl std::fmt::Display for TlsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let text = match self {
            Self::Capacity => "TLS capacity exceeded",
            Self::Invalid => "invalid TLS material or operation",
            Self::KeyMismatch => "TLS key does not match certificate",
            Self::Verification(_) => "TLS certificate verification failed",
            Self::Protocol => "TLS protocol failed",
            Self::Clock => "TLS time unavailable",
            Self::Crypto => "TLS cryptographic operation failed",
        };
        f.write_str(text)
    }
}

impl std::error::Error for TlsError {}

pub(super) fn certificate_error(error: rustls::Error) -> TlsError {
    use rustls::CertificateError as C;
    use VerificationFailure as V;
    match error {
        rustls::Error::InvalidCertificate(error) => match error {
            C::BadEncoding
            | C::UnhandledCriticalExtension
            | C::UnsupportedSignatureAlgorithmContext { .. }
            | C::UnsupportedSignatureAlgorithmForPublicKeyContext { .. } => TlsError::Invalid,
            C::Expired | C::ExpiredContext { .. } => TlsError::Verification(V::Expired),
            C::NotValidYet | C::NotValidYetContext { .. } => TlsError::Verification(V::NotYetValid),
            C::UnknownIssuer => TlsError::Verification(V::Untrusted),
            C::BadSignature => TlsError::Verification(V::Signature),
            C::NotValidForName | C::NotValidForNameContext { .. } => {
                TlsError::Verification(V::Name)
            }
            C::InvalidPurpose | C::InvalidPurposeContext { .. } => TlsError::Verification(V::Usage),
            _ => TlsError::Verification(V::Other),
        },
        _ => TlsError::Crypto,
    }
}

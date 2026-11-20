//! A disposable CA and matching TLS server key and chain for deployment tests.
use crate::R;
use td_crypto::{Crypto, Provider, P256_PKCS8_CAPACITY};

#[path = "../../../../tests/support/tls_certificate_fixture.rs"]
mod cert;

pub(crate) struct ServerPems {
    pub(crate) root: Vec<u8>,
    pub(crate) chain: Vec<u8>,
    pub(crate) key: Vec<u8>,
}

impl ServerPems {
    pub(crate) fn new(host: &str) -> R<Self> {
        let mut bytes = [0; P256_PKCS8_CAPACITY];
        let n = Provider.generate_p256(&mut bytes)?;
        let ca = Provider.load_p256(bytes.get(..n).ok_or("fixture CA key length")?)?;
        let root = cert::pem(
            "CERTIFICATE",
            &cert::certificate_names(&ca, &ca, true, 1, &[host], false),
        );
        let n = Provider.generate_p256(&mut bytes)?;
        let key_der = bytes.get(..n).ok_or("fixture server key length")?;
        let leaf = Provider.load_p256(key_der)?;
        let mut chain = cert::pem(
            "CERTIFICATE",
            &cert::certificate_names(&leaf, &ca, false, 2, &[host], false),
        );
        chain.extend_from_slice(&root);
        Ok(Self {
            root,
            chain,
            key: cert::pem("PRIVATE KEY", key_der),
        })
    }
}

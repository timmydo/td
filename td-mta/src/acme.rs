//! Bounded ACME protocol encoding. Network and durable order ownership are separate.
use td_crypto::{Crypto, Digest, P256Key, Provider};

pub mod challenge;
pub mod http;
pub mod order;
pub mod wire;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Invalid,
    Capacity,
    Crypto,
    Protocol,
    Transport,
    Deadline,
    Database,
    Conflict,
    RetryAt(i64),
}
impl From<td_crypto::Error> for Error {
    fn from(_: td_crypto::Error) -> Self {
        Self::Crypto
    }
}

pub(crate) fn digest(bytes: &[u8]) -> Result<[u8; 32], Error> {
    let mut digest = Provider.sha256()?;
    digest.update(bytes)?;
    Ok(digest.finish()?)
}

/// Exact RFC 7638 member order; no optional JWK members enter the thumbprint.
pub fn jwk(key: &P256Key) -> Result<String, Error> {
    let mut point = [0; 65];
    Provider.p256_public(key, &mut point)?;
    let x = wire::base64(point.get(1..33).ok_or(Error::Invalid)?, true)?;
    let y = wire::base64(point.get(33..65).ok_or(Error::Invalid)?, true)?;
    Ok(format!(
        r#"{{"crv":"P-256","kty":"EC","x":"{x}","y":"{y}"}}"#
    ))
}

pub fn key_authorization(key: &P256Key, token: &str) -> Result<String, Error> {
    wire::token(token, 22, 256)?;
    let thumbprint = wire::base64(&digest(jwk(key)?.as_bytes())?, true)?;
    Ok(format!("{token}.{thumbprint}"))
}

/// Flattened ES256 JWS. `None` uses the JWK for newAccount; otherwise use kid.
/// Empty payload is POST-as-GET, distinct from an empty JSON object.
pub fn jws(
    key: &P256Key,
    url: &str,
    nonce: &str,
    kid: Option<&str>,
    payload: &[u8],
) -> Result<Vec<u8>, Error> {
    crate::config::endpoint::HttpsUri::parse(url).map_err(|_| Error::Invalid)?;
    wire::token(nonce, 1, 512)?;
    if payload.len() > 16 * 1024 {
        return Err(Error::Capacity);
    }
    let url = td_json::Json::Str(url.to_owned()).to_string();
    let identity = match kid {
        Some(kid) => {
            crate::config::endpoint::HttpsUri::parse(kid).map_err(|_| Error::Invalid)?;
            format!("\"kid\":{}", td_json::Json::Str(kid.to_owned()))
        }
        None => format!("\"jwk\":{}", jwk(key)?),
    };
    let protected = format!(r#"{{"alg":"ES256","nonce":"{nonce}","url":{url},{identity}}}"#);
    let protected = wire::base64(protected.as_bytes(), true)?;
    let payload = wire::base64(payload, true)?;
    let signing = format!("{protected}.{payload}");
    let mut signature = [0; 64];
    Provider.sign_es256(key, signing.as_bytes(), &mut signature)?;
    let signature = wire::base64(&signature, true)?;
    Ok(
        format!(r#"{{"protected":"{protected}","payload":"{payload}","signature":"{signature}"}}"#)
            .into_bytes(),
    )
}

/// PKCS#10 with a SAN extensionRequest and ES256 signature; no common-name fallback.
pub fn csr(key: &P256Key, names: &[&str]) -> Result<Vec<u8>, Error> {
    if names.is_empty() || names.len() > 32 {
        return Err(Error::Invalid);
    }
    let mut dns = Vec::new();
    for (index, name) in names.iter().enumerate() {
        crate::config::values::dns_name(name).map_err(|_| Error::Invalid)?;
        if name.bytes().any(|b| b.is_ascii_uppercase())
            || names.get(..index).ok_or(Error::Invalid)?.contains(name)
        {
            return Err(Error::Invalid);
        }
        dns.extend(wire::der(0x82, name.as_bytes())?);
    }
    let names = wire::der(0x30, &dns)?;
    let mut extension = vec![0x06, 0x03, 0x55, 0x1d, 0x11];
    extension.extend(wire::der(0x04, &names)?);
    let extensions = wire::der(0x30, &wire::der(0x30, &extension)?)?;
    let mut attribute = vec![
        0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x09, 0x0e,
    ];
    attribute.extend(wire::der(0x31, &extensions)?);
    let attributes = wire::der(0xa0, &wire::der(0x30, &attribute)?)?;
    let mut point = [0; 65];
    Provider.p256_public(key, &mut point)?;
    let mut bits = vec![0];
    bits.extend_from_slice(&point);
    let mut spki = vec![
        0x30, 0x13, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, 0x06, 0x08, 0x2a, 0x86,
        0x48, 0xce, 0x3d, 0x03, 0x01, 0x07,
    ];
    spki.extend(wire::der(0x03, &bits)?);
    let mut info = vec![0x02, 0x01, 0x00, 0x30, 0x00];
    info.extend(wire::der(0x30, &spki)?);
    info.extend(attributes);
    let info = wire::der(0x30, &info)?;
    let mut signature = [0; 64];
    Provider.sign_es256(key, &info, &mut signature)?;
    let signature = wire::signature_der(&signature)?;
    let mut signed = info;
    signed.extend_from_slice(&[
        0x30, 0x0a, 0x06, 0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03, 0x02,
    ]);
    let mut bits = vec![0];
    bits.extend(signature);
    signed.extend(wire::der(0x03, &bits)?);
    wire::der(0x30, &signed)
}

//! Storage-neutral high-entropy app-password encoding and salted verifiers.
use crate::ids::{AccountId, DeviceId};
use std::io;
use td_crypto::{Crypto, Digest, Entropy, Provider};

pub const DEVICE_CAPACITY: usize = 256;
pub const DEVICE_LABEL_BYTES: usize = 128;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeviceError {
    Invalid,
    Unavailable,
}
impl std::fmt::Display for DeviceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Invalid => "invalid device record",
            Self::Unavailable => "device registry unavailable",
        })
    }
}
impl std::error::Error for DeviceError {}
impl From<io::Error> for DeviceError {
    fn from(_: io::Error) -> Self {
        Self::Unavailable
    }
}
impl From<td_crypto::Error> for DeviceError {
    fn from(_: td_crypto::Error) -> Self {
        Self::Unavailable
    }
}

/// Deliberately neither Debug nor Clone. Explicit exposure is for the local receipt.
pub struct DevicePassword([u8; 97]);
impl DevicePassword {
    pub fn expose(&self) -> &[u8] {
        &self.0
    }
}
impl Drop for DevicePassword {
    fn drop(&mut self) {
        self.0.fill(0);
        std::hint::black_box(&mut self.0);
    }
}
struct Secret([u8; 32]);
impl Drop for Secret {
    fn drop(&mut self) {
        self.0.fill(0);
        std::hint::black_box(&mut self.0);
    }
}
pub(crate) struct Credential {
    pub(crate) id: DeviceId,
    pub(crate) salt: [u8; 32],
    pub(crate) verifier: [u8; 32],
}

pub(crate) fn generate(
    account: AccountId,
    entropy: &mut impl Entropy,
) -> Result<(Credential, DevicePassword), DeviceError> {
    let mut id = [0; 16];
    let mut salt = [0; 32];
    let mut secret = Secret([0; 32]);
    entropy.fill(&mut id)?;
    entropy.fill(&mut salt)?;
    entropy.fill(&mut secret.0)?;
    let id = DeviceId::from_bytes(id);
    let verifier = verify_hash(account, id, &salt, &secret.0)?;
    let mut password = DevicePassword([0; 97]);
    for (out, byte) in password
        .0
        .get_mut(..32)
        .ok_or(DeviceError::Invalid)?
        .as_chunks_mut::<2>()
        .0
        .iter_mut()
        .zip(id.as_bytes())
    {
        out.copy_from_slice(&hex(*byte));
    }
    *password.0.get_mut(32).ok_or(DeviceError::Invalid)? = b'.';
    for (out, byte) in password
        .0
        .get_mut(33..)
        .ok_or(DeviceError::Invalid)?
        .as_chunks_mut::<2>()
        .0
        .iter_mut()
        .zip(&secret.0)
    {
        out.copy_from_slice(&hex(*byte));
    }
    Ok((Credential { id, salt, verifier }, password))
}

pub(crate) struct Presented {
    id: DeviceId,
    secret: Secret,
}
impl Presented {
    pub(crate) fn parse(password: &[u8]) -> Option<Self> {
        if password.len() != 97 || password.get(32) != Some(&b'.') {
            return None;
        }
        let id = password
            .get(..32)
            .and_then(|bytes| std::str::from_utf8(bytes).ok())
            .and_then(|text| DeviceId::parse(text).ok())?;
        let mut secret = Secret([0; 32]);
        for (out, pair) in secret
            .0
            .iter_mut()
            .zip(password.get(33..)?.as_chunks::<2>().0.iter())
        {
            let value = pair
                .first()
                .and_then(|b| unhex(*b))
                .zip(pair.get(1).and_then(|b| unhex(*b)))
                .map(|(hi, lo)| hi * 16 + lo)?;
            *out = value;
        }
        Some(Self { id, secret })
    }
    pub(crate) fn id(&self) -> DeviceId {
        self.id
    }
    pub(crate) fn matches(
        &self,
        account: AccountId,
        salt: &[u8; 32],
        expected: &[u8; 32],
    ) -> Result<bool, DeviceError> {
        let actual = verify_hash(account, self.id, salt, &self.secret.0)?;
        Ok(Provider.equal_digest(&actual, expected))
    }
}

fn hex(byte: u8) -> [u8; 2] {
    fn digit(n: u8) -> u8 {
        if n < 10 {
            b'0' + n
        } else {
            b'a' + n - 10
        }
    }
    [digit(byte >> 4), digit(byte & 15)]
}
fn unhex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}
pub(crate) fn valid_label(label: &str) -> Result<(), DeviceError> {
    if label.is_empty() || label.len() > DEVICE_LABEL_BYTES || label.chars().any(char::is_control) {
        return Err(DeviceError::Invalid);
    }
    Ok(())
}
fn verify_hash(
    account: AccountId,
    id: DeviceId,
    salt: &[u8; 32],
    secret: &[u8; 32],
) -> Result<[u8; 32], DeviceError> {
    let mut digest = Provider.sha256()?;
    for bytes in [
        b"td-mta device verifier v1\0".as_slice(),
        account.as_bytes(),
        id.as_bytes(),
        salt,
        secret,
    ] {
        digest.update(bytes)?;
    }
    Ok(digest.finish()?)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    #[test]
    fn v1_token_and_verifier_are_stable_and_bind_account_identity_and_salt() {
        use super::*;
        struct Random(u8);
        impl Entropy for Random {
            fn fill(&mut self, output: &mut [u8]) -> Result<(), td_crypto::Error> {
                self.0 += 1;
                output.fill(self.0);
                Ok(())
            }
        }
        let account = AccountId::from_bytes([7; 16]);
        let (credential, password) = generate(account, &mut Random(0)).unwrap();
        assert_eq!(
            password.expose(),
            format!("{}.{}", "01".repeat(16), "03".repeat(32)).as_bytes()
        );
        assert_eq!(
            credential.verifier,
            [
                48, 131, 201, 223, 32, 9, 10, 4, 86, 223, 3, 19, 99, 70, 114, 188, 8, 54, 183, 174,
                186, 55, 108, 108, 95, 209, 84, 49, 75, 22, 38, 69
            ]
        );
        let presented = Presented::parse(password.expose()).unwrap();
        assert_eq!(presented.id(), credential.id);
        assert!(presented
            .matches(account, &credential.salt, &credential.verifier)
            .unwrap());
        assert!(!presented
            .matches(
                AccountId::from_bytes([8; 16]),
                &credential.salt,
                &credential.verifier
            )
            .unwrap());
        assert!(!presented
            .matches(account, &[4; 32], &credential.verifier)
            .unwrap());
        let mut other = password.expose().to_vec();
        other[0] = b'2';
        assert!(!Presented::parse(&other)
            .unwrap()
            .matches(account, &credential.salt, &credential.verifier)
            .unwrap());
        other.copy_from_slice(password.expose());
        other[96] = b'4';
        assert!(!Presented::parse(&other)
            .unwrap()
            .matches(account, &credential.salt, &credential.verifier)
            .unwrap());
    }
    #[test]
    fn parsing_is_canonical_and_rejects_incomplete_credentials() {
        use super::*;
        let password = DevicePassword(
            format!("{}.{}", "ab".repeat(16), "cd".repeat(32))
                .into_bytes()
                .try_into()
                .unwrap(),
        );
        for length in 0..97 {
            assert!(Presented::parse(&password.expose()[..length]).is_none());
        }
        let mut other = password.expose().to_vec();
        other.push(b'0');
        assert!(Presented::parse(&other).is_none());
        for offset in [0, 31, 32, 33, 96] {
            let mut other = password.expose().to_vec();
            other[offset] = b'G';
            assert!(Presented::parse(&other).is_none());
        }
        let mut other = password.expose().to_vec();
        other[..32].make_ascii_uppercase();
        assert!(Presented::parse(&other).is_none());
        other.copy_from_slice(password.expose());
        other[33..].make_ascii_uppercase();
        assert!(Presented::parse(&other).is_none());
    }
    #[test]
    fn every_entropy_failure_refuses_a_generated_credential() {
        use super::*;
        struct Broken(usize);
        impl Entropy for Broken {
            fn fill(&mut self, bytes: &mut [u8]) -> Result<(), td_crypto::Error> {
                bytes.fill(42);
                self.0 -= 1;
                if self.0 == 0 {
                    Err(td_crypto::Error::Entropy)
                } else {
                    Ok(())
                }
            }
        }
        for call in 1..=3 {
            assert!(matches!(
                generate(AccountId::from_bytes([7; 16]), &mut Broken(call)),
                Err(DeviceError::Unavailable)
            ));
        }
    }
}

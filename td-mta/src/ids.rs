//! Local object identities. Parsing an ID never grants account authorization.
use std::fmt;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IdError {
    Length,
    NonCanonicalHex,
}

impl fmt::Display for IdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Length => "object ID must contain 32 lowercase hex characters (16 bytes)",
            Self::NonCanonicalHex => "object ID contains noncanonical hex",
        })
    }
}

impl std::error::Error for IdError {}

fn nibble(byte: u8) -> Result<u8, IdError> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        _ => Err(IdError::NonCanonicalHex),
    }
}

fn decode(input: &str) -> Result<[u8; 16], IdError> {
    if input.len() != 32 {
        return Err(IdError::Length);
    }
    let mut bytes = [0; 16];
    let (pairs, _) = input.as_bytes().as_chunks::<2>();
    for (out, &[high, low]) in bytes.iter_mut().zip(pairs) {
        *out = (nibble(high)? << 4) | nibble(low)?;
    }
    Ok(bytes)
}

macro_rules! object_ids {
    ($($name:ident),+ $(,)?) => {$(
        #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name([u8; 16]);

        impl $name {
            /// The entropy adapter and exclusive publication check own generation.
            pub const fn from_bytes(bytes: [u8; 16]) -> Self { Self(bytes) }

            pub const fn as_bytes(&self) -> &[u8; 16] { &self.0 }

            pub fn parse(input: &str) -> Result<Self, IdError> {
                decode(input).map(Self)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                for byte in &self.0 { write!(f, "{byte:02x}")?; }
                Ok(())
            }
        }
    )+};
}

object_ids!(
    AccountId,
    EmailId,
    MailboxId,
    ThreadId,
    BlobId,
    SubmissionId,
    AttemptId,
    IdentityId,
    DeviceId,
    StoreEpoch,
    InstanceId
);

/// Carries scope across internal APIs; callers must still check authorization.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ScopedId<T> {
    pub account: AccountId,
    pub object: T,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_id_has_fixed_bytes_and_display() -> Result<(), IdError> {
        let encoded = "000102030405060708090a0b0c0d0e0f";
        let id = EmailId::parse(encoded)?;
        assert_eq!(
            id.as_bytes(),
            &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15]
        );
        assert_eq!(id.to_string(), encoded);
        Ok(())
    }

    #[test]
    fn invalid_ids_are_rejected_without_path_interpretation() {
        for text in ["", "00", "000102030405060708090a0b0c0d0e0f00"] {
            assert_eq!(BlobId::parse(text), Err(IdError::Length));
        }
        for text in [
            "000102030405060708090A0B0C0D0E0F",
            "../102030405060708090a0b0c0d0e0f",
            "éééééééééééééééé",
            "000102030405060708090a0b0c0d0e0g",
        ] {
            assert!(BlobId::parse(text).is_err());
        }
    }

    #[test]
    fn all_byte_values_survive_canonical_encoding() -> Result<(), IdError> {
        for byte in u8::MIN..=u8::MAX {
            let id = AccountId::from_bytes([byte; 16]);
            assert_eq!(AccountId::parse(&id.to_string())?, id);
        }
        Ok(())
    }
}

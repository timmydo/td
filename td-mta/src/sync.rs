//! Opaque synchronization tokens, not authorization credentials.
use crate::{
    format::{ObjectType, Sequence},
    ids::{AccountId, StoreEpoch},
    wire::{hex, unhex, Error},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DataType {
    Mailbox,
    Thread,
    Email,
    Submission,
}
impl DataType {
    pub const fn object_type(self) -> ObjectType {
        match self {
            Self::Mailbox => ObjectType::Mailbox,
            Self::Thread => ObjectType::Thread,
            Self::Email => ObjectType::Email,
            Self::Submission => ObjectType::EmailSubmission,
        }
    }
    fn from_tag(tag: u8) -> Result<Self, Error> {
        match tag {
            1 => Ok(Self::Mailbox),
            2 => Ok(Self::Thread),
            3 => Ok(Self::Email),
            5 => Ok(Self::Submission),
            _ => Err(Error::Syntax),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DataState {
    pub account: AccountId,
    pub epoch: StoreEpoch,
    pub kind: DataType,
    pub sequence: Sequence,
}
impl DataState {
    pub const WIRE_BYTES: usize = 85;
    pub fn encode(self, output: &mut [u8]) -> Result<&str, Error> {
        let output = output
            .get_mut(..Self::WIRE_BYTES)
            .ok_or(Error::OutputFull)?;
        let (prefix, rest) = output.split_at_mut_checked(3).ok_or(Error::OutputFull)?;
        prefix.copy_from_slice(b"d1_");
        let (kind, rest) = rest.split_at_mut_checked(2).ok_or(Error::OutputFull)?;
        hex(&[self.kind.object_type().tag()], kind)?;
        let (account, rest) = rest.split_at_mut_checked(32).ok_or(Error::OutputFull)?;
        hex(self.account.as_bytes(), account)?;
        let (epoch, sequence) = rest.split_at_mut_checked(32).ok_or(Error::OutputFull)?;
        hex(self.epoch.as_bytes(), epoch)?;
        hex(&self.sequence.number().to_be_bytes(), sequence)?;
        std::str::from_utf8(output).map_err(|_| Error::Syntax)
    }
    pub fn decode(value: &str) -> Result<Self, Error> {
        if value.len() != Self::WIRE_BYTES {
            return Err(Error::Syntax);
        }
        let (prefix, rest) = value.as_bytes().split_at_checked(3).ok_or(Error::Syntax)?;
        if prefix != b"d1_" {
            return Err(Error::Syntax);
        }
        let (kind, rest) = rest.split_at_checked(2).ok_or(Error::Syntax)?;
        let [tag] = unhex(kind)?;
        let (account, rest) = rest.split_at_checked(32).ok_or(Error::Syntax)?;
        let (epoch, sequence) = rest.split_at_checked(32).ok_or(Error::Syntax)?;
        Ok(Self {
            account: AccountId::from_bytes(unhex(account)?),
            epoch: StoreEpoch::from_bytes(unhex(epoch)?),
            kind: DataType::from_tag(tag)?,
            sequence: Sequence::from_u64(u64::from_be_bytes(unhex(sequence)?)),
        })
    }
    /// Range/scoping check for changes; callers must acquire the matching view.
    pub fn is_retained_for(self, current: Self, floor: Sequence) -> bool {
        self.account == current.account
            && self.epoch == current.epoch
            && self.kind == current.kind
            && floor <= self.sequence
            && self.sequence <= current.sequence
    }
}

/// Identity data is atomically selected configuration, outside mail journals.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IdentityState {
    pub account: AccountId,
    pub epoch: StoreEpoch,
    pub digest: [u8; 32],
}
impl IdentityState {
    pub const WIRE_BYTES: usize = 131;
    pub fn encode(self, output: &mut [u8]) -> Result<&str, Error> {
        let output = output
            .get_mut(..Self::WIRE_BYTES)
            .ok_or(Error::OutputFull)?;
        let (prefix, rest) = output.split_at_mut_checked(3).ok_or(Error::OutputFull)?;
        prefix.copy_from_slice(b"c1_");
        let (account, rest) = rest.split_at_mut_checked(32).ok_or(Error::OutputFull)?;
        hex(self.account.as_bytes(), account)?;
        let (epoch, digest) = rest.split_at_mut_checked(32).ok_or(Error::OutputFull)?;
        hex(self.epoch.as_bytes(), epoch)?;
        hex(&self.digest, digest)?;
        std::str::from_utf8(output).map_err(|_| Error::Syntax)
    }
    pub fn decode(value: &str) -> Result<Self, Error> {
        if value.len() != Self::WIRE_BYTES {
            return Err(Error::Syntax);
        }
        let (prefix, rest) = value.as_bytes().split_at_checked(3).ok_or(Error::Syntax)?;
        if prefix != b"c1_" {
            return Err(Error::Syntax);
        }
        let (account, rest) = rest.split_at_checked(32).ok_or(Error::Syntax)?;
        let (epoch, digest) = rest.split_at_checked(32).ok_or(Error::Syntax)?;
        Ok(Self {
            account: AccountId::from_bytes(unhex(account)?),
            epoch: StoreEpoch::from_bytes(unhex(epoch)?),
            digest: unhex(digest)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn data_state_literal_and_scope_boundaries() -> Result<(), Error> {
        let state = DataState {
            account: AccountId::from_bytes([0x11; 16]),
            epoch: StoreEpoch::from_bytes([0x22; 16]),
            kind: DataType::Email,
            sequence: Sequence::from_u64(0x1234),
        };
        let expected =
            "d1_0311111111111111111111111111111111222222222222222222222222222222220000000000001234";
        let mut output = [0; DataState::WIRE_BYTES];
        assert_eq!(state.encode(&mut output)?, expected);
        assert_eq!(DataState::decode(expected)?, state);
        assert!(state.is_retained_for(state, Sequence::from_u64(0x1234)));
        assert!(!state.is_retained_for(state, Sequence::from_u64(0x1235)));
        for current in [
            DataState {
                account: AccountId::from_bytes([3; 16]),
                ..state
            },
            DataState {
                epoch: StoreEpoch::from_bytes([3; 16]),
                ..state
            },
            DataState {
                kind: DataType::Thread,
                ..state
            },
            DataState {
                sequence: Sequence::from_u64(0x1233),
                ..state
            },
        ] {
            assert!(!state.is_retained_for(current, Sequence::default()));
        }
        for kind in [
            DataType::Mailbox,
            DataType::Thread,
            DataType::Email,
            DataType::Submission,
        ] {
            for sequence in [Sequence::default(), Sequence::from_u64(u64::MAX)] {
                let value = DataState {
                    kind,
                    sequence,
                    ..state
                };
                assert_eq!(DataState::decode(value.encode(&mut output)?)?, value);
            }
        }
        for len in 0..DataState::WIRE_BYTES {
            assert_eq!(
                DataState::decode(expected.get(..len).ok_or(Error::Syntax)?),
                Err(Error::Syntax)
            );
            let mut short = vec![0xa5; len];
            assert_eq!(state.encode(&mut short), Err(Error::OutputFull));
            assert!(short.iter().all(|b| *b == 0xa5));
        }
        for invalid in [
            expected.replacen("03", "04", 1),
            expected.replacen("03", "ff", 1),
            expected.replace("d1_", "d2_"),
            format!("{expected}0"),
            expected.replace("1234", "abcD"),
        ] {
            assert_eq!(DataState::decode(&invalid), Err(Error::Syntax));
        }
        Ok(())
    }
    #[test]
    fn identity_state_has_its_own_literal_format() -> Result<(), Error> {
        let state = IdentityState {
            account: AccountId::from_bytes([0x11; 16]),
            epoch: StoreEpoch::from_bytes([0x22; 16]),
            digest: [0xab; 32],
        };
        let expected="c1_1111111111111111111111111111111122222222222222222222222222222222abababababababababababababababababababababababababababababababab";
        let mut output = [0; IdentityState::WIRE_BYTES];
        assert_eq!(state.encode(&mut output)?, expected);
        assert_eq!(IdentityState::decode(expected)?, state);
        assert_eq!(DataState::decode(expected), Err(Error::Syntax));
        for len in 0..IdentityState::WIRE_BYTES {
            assert_eq!(
                IdentityState::decode(expected.get(..len).ok_or(Error::Syntax)?),
                Err(Error::Syntax)
            );
            let mut short = vec![0xa5; len];
            assert_eq!(state.encode(&mut short), Err(Error::OutputFull));
            assert!(short.iter().all(|b| *b == 0xa5));
        }
        assert_eq!(
            IdentityState::decode(&format!("{expected}0")),
            Err(Error::Syntax)
        );
        assert_eq!(
            IdentityState::decode(&expected.replace("ab", "AB")),
            Err(Error::Syntax)
        );
        assert_eq!(
            IdentityState::decode(&expected.replace("c1_", "c2_")),
            Err(Error::Syntax)
        );
        Ok(())
    }
}

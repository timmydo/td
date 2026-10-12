//! Passive query-state syntax; query evaluation and authorization are separate.
use super::{DataState, DataType, DATA_PREFIX};
use crate::{
    format::Sequence,
    ids::{AccountId, StoreEpoch},
    ports::ViewIdentity,
    wire::{hex, unhex, Error},
};
/// POLICY.md owns the semantics associated with each interpretation version.
pub const QUERY_INTERPRETATION_VERSION: u32 = 1;
const QUERY_PREFIX: &[u8; 3] = b"q1_";
const VERSION_HEX: usize = 2 * std::mem::size_of::<u32>();
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QueryType {
    Mailbox,
    Email,
    Submission,
}
impl QueryType {
    fn data_type(self) -> DataType {
        match self {
            Self::Mailbox => DataType::Mailbox,
            Self::Email => DataType::Email,
            Self::Submission => DataType::Submission,
        }
    }
    fn from_data_type(kind: DataType) -> Result<Self, Error> {
        // New data types require an explicit query-support decision.
        match kind {
            DataType::Mailbox => Ok(Self::Mailbox),
            DataType::Email => Ok(Self::Email),
            DataType::Submission => Ok(Self::Submission),
            DataType::Thread => Err(Error::Syntax),
        }
    }
}
/// Passive metadata: equality requires identical query arguments to imply
/// equal results. No authorization, retained view or history lease is held.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct QueryState {
    pub account: AccountId,
    pub epoch: StoreEpoch,
    pub kind: QueryType,
    pub sequence: Sequence,
    pub interpretation: u32,
}
impl QueryState {
    pub const WIRE_BYTES: usize = DataState::WIRE_BYTES + VERSION_HEX;
    /// Capture passive metadata, not a live view or permission to publish.
    pub fn current(identity: ViewIdentity, kind: QueryType) -> Self {
        Self {
            account: identity.account,
            epoch: identity.epoch,
            kind,
            sequence: identity.committed_sequence,
            interpretation: QUERY_INTERPRETATION_VERSION,
        }
    }
    /// Compare every component with trusted current metadata. This does not
    /// compare query arguments, authorize access or establish a live view.
    pub fn matches_current(self, identity: ViewIdentity, kind: QueryType) -> bool {
        self == Self::current(identity, kind)
    }
    /// Too-short output is unchanged; a larger buffer's unused tail is untouched.
    pub fn encode(self, output: &mut [u8]) -> Result<&str, Error> {
        let output = output
            .get_mut(..Self::WIRE_BYTES)
            .ok_or(Error::OutputFull)?;
        let (data, version) = output
            .split_at_mut_checked(DataState::WIRE_BYTES)
            .ok_or(Error::OutputFull)?;
        DataState {
            account: self.account,
            epoch: self.epoch,
            kind: self.kind.data_type(),
            sequence: self.sequence,
        }
        .encode(data)?;
        data.get_mut(..3)
            .ok_or(Error::OutputFull)?
            .copy_from_slice(QUERY_PREFIX);
        hex(&self.interpretation.to_be_bytes(), version)?;
        std::str::from_utf8(output).map_err(|_| Error::Syntax)
    }
    /// Decode syntax including unknown interpretation versions. Callers must
    /// not equate those versions with the currently implemented policy.
    pub fn decode(value: &str) -> Result<Self, Error> {
        if value.len() != Self::WIRE_BYTES {
            return Err(Error::Syntax);
        }
        let (data, version) = value
            .as_bytes()
            .split_at_checked(DataState::WIRE_BYTES)
            .ok_or(Error::Syntax)?;
        if data.get(..3) != Some(QUERY_PREFIX.as_slice()) {
            return Err(Error::Syntax);
        }
        let mut ordinary: [u8; DataState::WIRE_BYTES] =
            data.try_into().map_err(|_| Error::Syntax)?;
        ordinary
            .get_mut(..3)
            .ok_or(Error::Syntax)?
            .copy_from_slice(DATA_PREFIX);
        let data = DataState::decode(std::str::from_utf8(&ordinary).map_err(|_| Error::Syntax)?)?;
        Ok(Self {
            account: data.account,
            epoch: data.epoch,
            kind: QueryType::from_data_type(data.kind)?,
            sequence: data.sequence,
            interpretation: u32::from_be_bytes(unhex(version)?),
        })
    }
}
#[cfg(test)]
#[path = "query_tests.rs"]
mod tests;

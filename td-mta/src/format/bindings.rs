//! Selected metadata bindings, not graph completeness, filesystem trust or durability.
use super::{
    container::{hash, Current, Error, JournalHeader, StoreIdentity},
    journal_stream::Summary as JournalSummary,
    manifest::Manifest,
    table_stream::Summary,
    Error as FormatError, Sequence, Table,
};
use crate::{ids::AccountId, ports::Crypto};

/// A manifest bound to CURRENT, store epoch and the caller's expected account.
/// Every referenced file still requires its own validation and physical EOF checks.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Selection<'a> {
    store: StoreIdentity,
    current: Current,
    manifest: Manifest<'a>,
}
impl<'a> Selection<'a> {
    pub fn decode(
        crypto: &impl Crypto,
        account: AccountId,
        format_bytes: &[u8],
        current_bytes: &[u8],
        manifest_bytes: &'a [u8],
    ) -> Result<Self, Error> {
        let store = StoreIdentity::decode(crypto, format_bytes)?;
        let current = Current::decode(crypto, current_bytes)?;
        if current.account != account || current.epoch != store.epoch {
            return Err(FormatError::InvalidValue.into());
        }
        let manifest = Manifest::decode(crypto, manifest_bytes)?;
        let header = manifest.header();
        if header.account != account
            || header.epoch != store.epoch
            || header.generation != current.generation
        {
            return Err(FormatError::InvalidValue.into());
        }
        if !crypto.equal_digest(&hash(crypto, manifest_bytes)?, &current.manifest_digest) {
            return Err(Error::Checksum);
        }
        Ok(Self {
            store,
            current,
            manifest,
        })
    }
    pub const fn store(self) -> StoreIdentity {
        self.store
    }
    pub const fn current(self) -> Current {
        self.current
    }
    pub const fn manifest(self) -> Manifest<'a> {
        self.manifest
    }

    /// Compare a completed supplied stream with its expected table descriptor.
    /// The expected tag comes from the requested file, not its decoded header.
    pub fn check_table(
        self,
        crypto: &impl Crypto,
        expected: Table,
        actual: Summary,
    ) -> Result<(), Error> {
        let header = actual.header();
        let manifest = self.manifest.header();
        if header.table != expected
            || header.account != manifest.account
            || header.epoch != manifest.epoch
            || header.generation != manifest.generation
            || header.through != manifest.through
        {
            return Err(FormatError::InvalidValue.into());
        }
        let descriptor = self.manifest.table(expected)?;
        if header.record_count != descriptor.record_count
            || header.file_bytes()? != descriptor.file_bytes
        {
            return Err(FormatError::InvalidValue.into());
        }
        if !crypto.equal_digest(&actual.digest(), &descriptor.digest) {
            return Err(Error::Checksum);
        }
        Ok(())
    }
    fn check_journal(
        self,
        crypto: &impl Crypto,
        bytes: &[u8],
        segment: u64,
        base: Sequence,
    ) -> Result<JournalHeader, Error> {
        self.check_journal_identity(JournalHeader::decode(crypto, bytes)?, segment, base)
    }
    fn check_journal_identity(
        self,
        header: JournalHeader,
        segment: u64,
        base: Sequence,
    ) -> Result<JournalHeader, Error> {
        if header.account != self.current.account
            || header.epoch != self.store.epoch
            || header.segment != segment
            || header.base != base
        {
            return Err(FormatError::InvalidValue.into());
        }
        Ok(header)
    }
    /// Header identity only; active frames and committed-prefix recovery remain unchecked.
    pub fn check_active_header(
        self,
        crypto: &impl Crypto,
        bytes: &[u8],
    ) -> Result<JournalHeader, Error> {
        let header = self.manifest.header();
        self.check_journal(crypto, bytes, header.active_segment, header.through)
    }
    /// Header identity only; history frame range, full extent and digest remain unchecked.
    pub fn check_history_header(
        self,
        crypto: &impl Crypto,
        index: usize,
        bytes: &[u8],
    ) -> Result<JournalHeader, Error> {
        let descriptor = self.manifest.history(index)?;
        self.check_journal(crypto, bytes, descriptor.segment, descriptor.base)
    }
    /// Bind a completed supplied prefix to the caller's pinned active offset/sequence.
    /// The caller establishes pin ownership and exact physical prefix consumption.
    pub fn check_active_prefix(
        self,
        through: Sequence,
        file_bytes: u64,
        actual: JournalSummary,
    ) -> Result<(), Error> {
        let selected = self.manifest.header();
        self.check_journal_identity(actual.header(), selected.active_segment, selected.through)?;
        if actual.through() != through
            || u64::try_from(actual.file_bytes()?).map_err(|_| FormatError::Overflow)? != file_bytes
        {
            return Err(FormatError::InvalidValue.into());
        }
        Ok(())
    }
    /// Bind complete supplied history to the indexed descriptor; EOF and pins remain external.
    pub fn check_history_journal(
        self,
        crypto: &impl Crypto,
        index: usize,
        actual: JournalSummary,
    ) -> Result<(), Error> {
        let descriptor = self.manifest.history(index)?;
        self.check_journal_identity(actual.header(), descriptor.segment, descriptor.base)?;
        if actual.through() != descriptor.through
            || u64::try_from(actual.file_bytes()?).map_err(|_| FormatError::Overflow)?
                != descriptor.file_bytes
        {
            return Err(FormatError::InvalidValue.into());
        }
        if !crypto.equal_digest(&actual.digest(), &descriptor.digest) {
            return Err(Error::Checksum);
        }
        Ok(())
    }
}

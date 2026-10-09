//! Bind historical metadata and body reports for one account snapshot.
use crate::{
    format::Table, metadata_sweep::CompleteMetadata, ports::ViewIdentity, store_fs::CompleteBodies,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Identity,
    BlobCount,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "account check reports: {self:?}")
    }
}
impl std::error::Error for Error {}

/// Historical reports only; physical completeness, custody and authority stay external.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CompleteChecks {
    metadata: CompleteMetadata,
    bodies: CompleteBodies,
}
impl CompleteChecks {
    pub const fn identity(&self) -> ViewIdentity {
        self.bodies.identity()
    }
    pub const fn metadata(&self) -> CompleteMetadata {
        self.metadata
    }
    pub const fn bodies(&self) -> CompleteBodies {
        self.bodies
    }
}

/// No I/O or renewed work scope; both inputs must already be complete.
pub fn combine(
    metadata: CompleteMetadata,
    bodies: CompleteBodies,
) -> Result<CompleteChecks, Error> {
    let references = metadata.references();
    if references.identity() != bodies.identity() {
        return Err(Error::Identity);
    }
    if references.table_rows(Table::Blobs) != Some(bodies.blobs()) {
        return Err(Error::BlobCount);
    }
    Ok(CompleteChecks { metadata, bodies })
}

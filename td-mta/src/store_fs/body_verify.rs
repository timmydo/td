//! Complete declared-body verification within one captured account snapshot.
use super::{IndexReadView, MAX_FILE_STEP_BYTES};
use crate::{
    format::{key::Key, row::Row, Table},
    ids::BlobId,
    ports::{Crypto, Error, ReadView, ViewIdentity},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BodyCheckLimits {
    pub blobs: u64,
    pub bytes: u64,
}
/// Historical completion for this identity; neither a body pin nor freshness.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CompleteBodies {
    identity: ViewIdentity,
    blobs: u64,
    bytes: u64,
}
impl CompleteBodies {
    pub const fn identity(&self) -> ViewIdentity {
        self.identity
    }
    pub const fn blobs(&self) -> u64 {
        self.blobs
    }
    pub const fn bytes(&self) -> u64 {
        self.bytes
    }
}
impl IndexReadView<'_, '_> {
    /// Synchronous maintenance; shares the view's original deadline and VM fuel.
    pub fn verify_bodies<C: Crypto>(
        &mut self,
        crypto: &C,
        limits: BodyCheckLimits,
        scratch: &mut [u8; MAX_FILE_STEP_BYTES],
    ) -> Result<CompleteBodies, Error> {
        let identity = self.identity();
        let mut after: Option<BlobId> = None;
        let mut key = [0; 16];
        let mut value = [0; 64];
        let mut blobs = 0u64;
        let mut bytes = 0u64;
        loop {
            let record = self.next(
                Table::Blobs,
                after.as_ref().map(|id| id.as_bytes().as_slice()),
                &mut key,
                &mut value,
            )?;
            let Some(record) = record else {
                return Ok(CompleteBodies {
                    identity,
                    blobs,
                    bytes,
                });
            };
            let (Key::Blob(id), Row::Blob(row)) = (record.key, record.row) else {
                return Err(Error::Corrupt);
            };
            let next_blobs = blobs.checked_add(1).ok_or(Error::Capacity)?;
            let next_bytes = bytes.checked_add(row.length).ok_or(Error::Capacity)?;
            if next_blobs > limits.blobs || next_bytes > limits.bytes {
                return Err(Error::Capacity);
            }
            let mut input = self.open_blob_input(crypto, id, row.length)?;
            while input.read(scratch)? != 0 {}
            drop(input.finish()?);
            blobs = next_blobs;
            bytes = next_bytes;
            after = Some(id);
        }
    }
}

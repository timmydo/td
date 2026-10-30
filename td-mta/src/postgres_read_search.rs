//! Passive search descriptors share the metadata window and its snapshot.
use super::{Error, Snapshot};
use crate::{ids::BlobId, postgres_body, postgres_search};
use tokio_postgres::{types::Type, Client};

#[derive(Clone, Copy)]
pub(super) struct Descriptor {
    pub(super) length: u64,
    pub(super) body_digest: [u8; 32],
    pub(super) size: usize,
    pub(super) expected: [u8; 32],
}

pub(super) struct Window(Vec<(BlobId, Option<Descriptor>)>);
impl Window {
    async fn read(
        client: &Client,
        account: crate::ids::AccountId,
        blobs: &[BlobId],
    ) -> Result<Self, Error> {
        if blobs.len() > postgres_body::MAX_METADATA_IDS {
            return Err(Error::Capacity);
        }
        let mut ids = Vec::new();
        let mut entries = Vec::new();
        ids.try_reserve_exact(blobs.len())
            .map_err(|_| Error::Capacity)?;
        entries
            .try_reserve_exact(blobs.len())
            .map_err(|_| Error::Capacity)?;
        for blob in blobs {
            if entries.iter().any(|(id, _)| id == blob) {
                return Err(Error::Invalid);
            }
            ids.push(blob.as_bytes().as_slice());
            entries.push((*blob, None));
        }
        let rows = client.query_typed("SELECT blob_id,body_length,body_digest,octet_length(fields),fields_digest FROM td_mta.blob_search WHERE account=$1 AND blob_id=ANY($2) AND version=$3 ORDER BY blob_id LIMIT 16", &[(&account.as_bytes().as_slice(),Type::BYTEA),(&ids,Type::BYTEA_ARRAY),(&postgres_search::VERSION,Type::INT4)]).await.map_err(|_| Error::Database)?;
        let mut previous = None;
        for row in rows {
            let bytes: &[u8] = row.try_get(0).map_err(|_| Error::Integrity)?;
            let blob = BlobId::from_bytes(bytes.try_into().map_err(|_| Error::Integrity)?);
            if previous.is_some_and(|old| old >= blob) {
                return Err(Error::Integrity);
            }
            previous = Some(blob);
            let entry = entries
                .iter_mut()
                .find(|(id, _)| *id == blob)
                .ok_or(Error::Integrity)?;
            let length: i64 = row.try_get(1).map_err(|_| Error::Integrity)?;
            let body_digest: &[u8] = row.try_get(2).map_err(|_| Error::Integrity)?;
            let size: i32 = row.try_get(3).map_err(|_| Error::Integrity)?;
            let expected: &[u8] = row.try_get(4).map_err(|_| Error::Integrity)?;
            entry.1 = descriptor(length, body_digest, size, expected);
        }
        Ok(Self(entries))
    }
    fn get(&self, blob: BlobId) -> Option<Descriptor> {
        self.0
            .iter()
            .find(|(id, _)| *id == blob)
            .and_then(|(_, value)| *value)
    }
}

fn descriptor(length: i64, body_digest: &[u8], size: i32, expected: &[u8]) -> Option<Descriptor> {
    let size = usize::try_from(size).ok()?;
    if size > postgres_search::MAX_BYTES {
        return None;
    }
    Some(Descriptor {
        length: u64::try_from(length).ok()?,
        body_digest: body_digest.try_into().ok()?,
        size,
        expected: expected.try_into().ok()?,
    })
}

impl Snapshot<'_> {
    pub(super) fn search_descriptor(&mut self, blob: BlobId) -> Result<Option<Descriptor>, Error> {
        let windowed = self
            .metadata_window
            .iter()
            .flatten()
            .any(|(_, value)| value.is_some_and(|metadata| metadata.blob == blob));
        if !windowed {
            let mut blobs = Vec::new();
            blobs.try_reserve_exact(1).map_err(|_| Error::Capacity)?;
            blobs.push(blob);
            return Ok(self.read_search_descriptors(blobs)?.get(blob));
        }
        if self.search_window.is_none() {
            let mut blobs = Vec::new();
            blobs
                .try_reserve_exact(postgres_body::MAX_METADATA_IDS)
                .map_err(|_| Error::Capacity)?;
            for (_, metadata) in self.metadata_window.iter().flatten() {
                if let Some(metadata) = metadata {
                    if !blobs.contains(&metadata.blob) {
                        blobs.push(metadata.blob);
                    }
                }
            }
            self.search_window = Some(self.read_search_descriptors(blobs)?);
        }
        Ok(self
            .search_window
            .as_ref()
            .and_then(|window| window.get(blob)))
    }
    fn read_search_descriptors(&mut self, blobs: Vec<BlobId>) -> Result<Window, Error> {
        let account = self.identity.account;
        let result = self
            .worker
            .run(|client| Box::pin(async move { Window::read(client, account, &blobs).await }))
            .map_err(Error::Connection)
            .and_then(|result| result);
        self.retain(result)
    }
}

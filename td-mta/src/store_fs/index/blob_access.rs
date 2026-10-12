//! Device-authorized verified blob reads under one pooled snapshot.
use super::*;
use crate::ports::BlobReader;

/// Trusted read policy. The guard carries Access and excludes revocation.
/// An upload permission alone does not implement this separate read contract.
/// Acquisition must obey the deadline and must not call into the coordinator.
pub trait BlobAuthorization {
    type Guard<'a>: UploadGuard
    where
        Self: 'a;
    fn authorize_blob(
        &self,
        account: AccountId,
        device: DeviceId,
        deadline: Deadline,
    ) -> Result<Self::Guard<'_>, ports::Error>;
}

/// Passive bounds; the policy and stored ownership determine actual access.
#[derive(Clone, Copy)]
pub struct BlobReadRequest {
    pub account: AccountId,
    pub device: DeviceId,
    pub id: BlobId,
    pub maximum: u64,
    pub deadline: Deadline,
}

impl<P: BlobAuthorization> StoreCoordinator<'_, '_, P> {
    /// The callback sees only verified bytes and cannot outlive their snapshot.
    /// Authorization is fresh before verification and callback admission. A read
    /// already admitted may finish within its original deadline after revocation.
    /// The callback must bound all its own work and I/O by that same deadline.
    pub fn with_authorized_blob<C: Crypto, T>(
        &self,
        crypto: &C,
        request: BlobReadRequest,
        read: impl FnOnce(&mut dyn BlobReader) -> Result<T, ports::Error>,
    ) -> Result<T, ports::Error> {
        let mut last = self.store.clock.sample()?.monotonic;
        check_time(&self.store, request.deadline, &mut last)?;
        let guard =
            self.authorization
                .authorize_blob(request.account, request.device, request.deadline)?;
        validate_access(guard.access(), request.account, request.device)?;
        drop(guard);
        let mut view = self.store.view(request.account, request.deadline)?;
        let referenced = view.read_snapshot(|native| {
            native.run(|db| {
                db.query_row(
                    "SELECT EXISTS(SELECT 1 FROM emails INDEXED BY emails_blob WHERE account=?1 AND blob_id=?2)
                 OR EXISTS(SELECT 1 FROM submissions INDEXED BY submissions_blob
                     WHERE account=?1 AND transmitted_blob_id=?2)",
                    params![
                        request.account.as_bytes().as_slice(),
                        request.id.as_bytes().as_slice()
                    ],
                    |row| row.get::<_, bool>(0),
                )
                .map_err(sql)
            })
        })?;
        let now = check_time(&self.store, request.deadline, &mut last)?;
        let mut row = [0; 64];
        let expires = if referenced {
            None
        } else {
            let Some((Row::Lease(lease), _)) = view.get(Key::Lease(request.id), &mut row)? else {
                return Err(ports::Error::NotFound);
            };
            if lease.account != request.account
                || lease.device != request.device
                || lease.expires_at <= now.utc_ms
            {
                return Err(ports::Error::NotFound);
            }
            Some(lease.expires_at)
        };
        let mut input = view.open_blob_input(crypto, request.id, request.maximum)?;
        let mut scratch = [0; 16 * 1024];
        while input.position() < input.len() {
            if input.read(&mut scratch)? == 0 {
                return Err(ports::Error::Corrupt);
            }
        }
        let mut body = input.finish()?;
        let guard =
            self.authorization
                .authorize_blob(request.account, request.device, request.deadline)?;
        validate_access(guard.access(), request.account, request.device)?;
        let now = check_time(&self.store, request.deadline, &mut last)?;
        if expires.is_some_and(|expires| expires <= now.utc_ms) {
            return Err(ports::Error::NotFound);
        }
        drop(guard);
        body.check_deadline()?;
        let result = read(&mut body);
        let checked = body.check_deadline();
        match result {
            Ok(value) => checked.map(|()| value),
            Err(error) => Err(error),
        }
    }
}

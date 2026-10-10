//! Explicit upload-lease retirement through the owning store and quota ledger.
use super::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UploadSweepDisposition {
    Retained,
    Retired {
        sequence: Sequence,
        body_removed: bool,
    },
}

#[derive(Debug)]
#[must_use = "advance the sweep cursor only after inspecting a known outcome"]
pub struct UploadCleanup {
    pub id: BlobId,
    pub outcome: Result<UploadSweepDisposition, CommitError>,
    pub admission_stopped: bool,
}

impl<P> UploadCoordinator<'_, '_, P> {
    /// Inspect one lease in account-local ID order. Only an Ok disposition lets
    /// the caller advance to its ID. Reset the cursor after None to start a new
    /// pass, including leases inserted before the previous cursor.
    #[must_use = "inspect the cleanup result before advancing the sweep"]
    pub fn expire_next_upload(
        &mut self,
        crypto: &impl Crypto,
        account: AccountId,
        after: Option<BlobId>,
        deadline: Deadline,
    ) -> Result<Option<UploadCleanup>, UploadError> {
        if self.active {
            return Err(ports::Error::Busy.into());
        }
        if self.stopped {
            return Err(ports::Error::WriterStopped.into());
        }
        let mut last = self.store.clock.sample()?.monotonic;
        let now = check_time(&self.store, deadline, &mut last)?;
        let mut view = self.store.view(account, deadline)?;
        let identity = view.identity();
        // BlobId key; the larger encoded row is BlobRow (48), versus LeaseRow (41).
        let mut key = [0; 16];
        let mut value = [0; 48];
        let Some(record) = view.next(
            Table::Leases,
            after.as_ref().map(|id| id.as_bytes().as_slice()),
            &mut key,
            &mut value,
        )?
        else {
            return Ok(None);
        };
        let (Key::Lease(id), Row::Lease(lease)) = (record.key, record.row) else {
            return Err(ports::Error::Corrupt.into());
        };
        if lease.account != account {
            return Err(ports::Error::Corrupt.into());
        }
        if lease.expires_at > now.utc_ms {
            return Ok(Some(retained(id)));
        }
        let Some((Row::Blob(blob), _)) = view.get(Key::Blob(id), &mut value)? else {
            return Err(ports::Error::Corrupt.into());
        };
        let body_removed = view.read_snapshot(|native| {
            native.run(|db| {
                db.query_row(
                    "SELECT NOT EXISTS(SELECT 1 FROM emails WHERE account=?1 AND blob_id=?2)
                     AND NOT EXISTS(SELECT 1 FROM submissions
                         WHERE account=?1 AND transmitted_blob_id=?2)",
                    params![account.as_bytes().as_slice(), id.as_bytes().as_slice()],
                    |row| row.get::<_, bool>(0),
                )
                .map_err(sql)
            })
        })?;
        drop(view);
        let now = check_time(&self.store, deadline, &mut last)?;
        if lease.expires_at > now.utc_ms {
            return Ok(Some(retained(id)));
        }
        self.ledger
            .check_expired_upload(blob.length, body_removed)?;
        let lease_delete = Operation::delete(Table::Leases, id.as_bytes())?;
        let body_delete = Operation::delete(Table::Blobs, id.as_bytes())?;
        let operations = [lease_delete, body_delete];
        let operations = operations
            .get(..if body_removed { 2 } else { 1 })
            .ok_or(ports::Error::Invalid)?;
        let (physical_lease, mut ticket) = reserve_physical(
            &mut self.ledger,
            &mut self.stopped,
            &mut self.recoverable_files,
            deadline,
            now.monotonic,
        )?;
        let completion = self.store.commit_with_files(
            crypto,
            CommitRequest {
                account,
                epoch: identity.epoch,
                expected: identity.committed_sequence,
                utc_ms: now.utc_ms,
                deadline,
            },
            operations,
            &mut [],
        );
        let files = completion.files();
        let physical_settled = settle_physical(&mut self.ledger, &mut ticket, files);
        let physical_canceled = self.ledger.cancel(physical_lease).is_ok();
        let outcome = completion.outcome();
        let logical_settled = match outcome {
            Ok(_) => self
                .ledger
                .complete_expired_upload(blob.length, body_removed)
                .is_ok(),
            Err(CommitError::Rejected(_)) => true,
            Err(CommitError::Indeterminate(_)) => false,
        };
        self.stopped = !physical_settled
            || !physical_canceled
            || !logical_settled
            || completion.writer_stopped()
            || matches!(files, CommitFileUsage::Unavailable(_));
        self.recoverable_files = matches!(files, CommitFileUsage::Unavailable(_))
            && physical_settled
            && physical_canceled
            && logical_settled
            && !completion.writer_stopped()
            && !matches!(outcome, Err(CommitError::Indeterminate(_)));
        Ok(Some(UploadCleanup {
            id,
            outcome: outcome.map(|sequence| UploadSweepDisposition::Retired {
                sequence,
                body_removed,
            }),
            admission_stopped: self.stopped,
        }))
    }
}

fn retained(id: BlobId) -> UploadCleanup {
    UploadCleanup {
        id,
        outcome: Ok(UploadSweepDisposition::Retained),
        admission_stopped: false,
    }
}

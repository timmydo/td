//! Upload retirement through the owning store and quota ledger.
use super::*;

const RECLAIM_VICTIMS: usize = 64;

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

impl<P> StoreCoordinator<'_, '_, P> {
    /// Inspect one lease in account-local ID order. Only an Ok disposition lets
    /// the caller advance to its ID. Reset the cursor after None to start a new
    /// pass, including leases inserted before the previous cursor.
    #[must_use = "inspect the cleanup result before advancing the sweep"]
    pub fn expire_next_upload(
        &self,
        crypto: &impl Crypto,
        account: AccountId,
        after: Option<BlobId>,
        deadline: Deadline,
    ) -> Result<Option<UploadCleanup>, UploadError> {
        let mut state_guard = self.try_state()?;
        let state = &mut *state_guard;
        if state.stopped {
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
        self.retire_upload(
            crypto,
            state,
            Retirement {
                id,
                length: blob.length,
                body_removed,
                identity,
                now,
                deadline,
            },
        )
        .map(Some)
    }

    fn retire_upload(
        &self,
        crypto: &impl Crypto,
        state: &mut AdmissionState<'_>,
        request: Retirement,
    ) -> Result<UploadCleanup, UploadError> {
        state
            .ledger
            .check_expired_upload(request.length, request.body_removed)?;
        let lease_delete = Operation::delete(Table::Leases, request.id.as_bytes())?;
        let body_delete = Operation::delete(Table::Blobs, request.id.as_bytes())?;
        let operations = [lease_delete, body_delete];
        let operations = operations
            .get(..if request.body_removed { 2 } else { 1 })
            .ok_or(ports::Error::Invalid)?;
        let (physical_lease, mut ticket) = reserve_physical(
            &mut state.ledger,
            &mut state.stopped,
            &mut state.recoverable_files,
            request.deadline,
            request.now.monotonic,
        )?;
        let completion = self.store.commit_with_files(
            crypto,
            CommitRequest {
                account: request.identity.account,
                epoch: request.identity.epoch,
                expected: request.identity.committed_sequence,
                utc_ms: request.now.utc_ms,
                deadline: request.deadline,
            },
            operations,
            &mut [],
        );
        let files = completion.files();
        let physical_settled = settle_physical(&mut state.ledger, &mut ticket, files);
        let physical_canceled = state.ledger.cancel(physical_lease).is_ok();
        let outcome = completion.outcome();
        let logical_settled = match outcome {
            Ok(_) => state
                .ledger
                .complete_expired_upload(request.length, request.body_removed)
                .is_ok(),
            Err(CommitError::Rejected(_)) => true,
            Err(CommitError::Indeterminate(_)) => false,
        };
        state.stopped = !physical_settled
            || !physical_canceled
            || !logical_settled
            || completion.writer_stopped()
            || matches!(files, CommitFileUsage::Unavailable(_));
        state.recoverable_files = matches!(files, CommitFileUsage::Unavailable(_))
            && physical_settled
            && physical_canceled
            && logical_settled
            && !completion.writer_stopped()
            && !matches!(outcome, Err(CommitError::Indeterminate(_)));
        Ok(UploadCleanup {
            id: request.id,
            outcome: outcome.map(|sequence| UploadSweepDisposition::Retired {
                sequence,
                body_removed: request.body_removed,
            }),
            admission_stopped: state.stopped,
        })
    }
}

struct Retirement {
    id: BlobId,
    length: u64,
    body_removed: bool,
    identity: ViewIdentity,
    now: Time,
    deadline: Deadline,
}

fn retained(id: BlobId) -> UploadCleanup {
    UploadCleanup {
        id,
        outcome: Ok(UploadSweepDisposition::Retained),
        admission_stopped: false,
    }
}

impl<'r, 'a, P: UploadAuthorization> StoreCoordinator<'r, 'a, P> {
    /// Reclaim committed, unreferenced uploads oldest first only on quota pressure.
    /// Every attempt retains the original deadline; active ingress is never a victim.
    pub fn reserve_reclaiming<'c, 's, C: Crypto>(
        &'c self,
        crypto: &'c C,
        entropy: &mut impl Entropy,
        spool: &'s IngressSpool<'r>,
        request: UploadRequest,
    ) -> Result<Upload<'c, 's, 'r, 'a, C, P>, UploadError> {
        self.reserve_with_reclaim_limit(crypto, entropy, spool, request, RECLAIM_VICTIMS)
    }
    pub(super) fn reserve_with_reclaim_limit<'c, 's, C: Crypto>(
        &'c self,
        crypto: &'c C,
        entropy: &mut impl Entropy,
        spool: &'s IngressSpool<'r>,
        request: UploadRequest,
        mut victims: usize,
    ) -> Result<Upload<'c, 's, 'r, 'a, C, P>, UploadError> {
        let mut last = self.store.clock.sample()?.monotonic;
        let mut no_progress = false;
        loop {
            check_time(&self.store, request.deadline, &mut last)?;
            match self.reserve_at(crypto, entropy, spool, request, &mut last) {
                Ok(upload) => return Ok(upload),
                Err(
                    error @ UploadError::Ledger(logical::Error::Quota(
                        Kind::BodyBytes | Kind::BlobCount | Kind::UploadBytes,
                    )),
                ) => {
                    if victims == 0 {
                        return Err(ports::Error::Busy.into());
                    }
                    match self.reclaim_for_upload(crypto, request, &mut last)? {
                        Room::Removed => {
                            victims = victims.checked_sub(1).ok_or(ports::Error::Invalid)?;
                            no_progress = false;
                        }
                        Room::Fits if !no_progress => no_progress = true,
                        Room::Fits | Room::Unavailable => return Err(error),
                    }
                }
                Err(error) => return Err(error),
            }
        }
    }

    fn reclaim_for_upload(
        &self,
        crypto: &impl Crypto,
        request: UploadRequest,
        last: &mut Tick,
    ) -> Result<Room, UploadError> {
        let mut state = self.try_state()?;
        if state.stopped {
            return Err(ports::Error::WriterStopped.into());
        }
        check_time(&self.store, request.deadline, last)?;
        let guard =
            self.authorization
                .authorize(request.account, request.device, request.deadline)?;
        validate_access(guard.access(), request.account, request.device)?;
        let needed_bytes = request
            .maximum
            .saturating_sub(state.ledger.remaining_capacity(Kind::BodyBytes)?)
            .max(
                request
                    .maximum
                    .saturating_sub(state.ledger.remaining_capacity(Kind::UploadBytes)?),
            );
        let needed_blobs = 1u64.saturating_sub(state.ledger.remaining_capacity(Kind::BlobCount)?);
        if needed_bytes == 0 && needed_blobs == 0 {
            return Ok(Room::Fits);
        }
        let mut view = self.store.view(request.account, request.deadline)?;
        let identity = view.identity();
        let candidate = view.read_snapshot(|native| {
            native.run(|db| {
                let (bytes, count) = db
                    .query_row(
                        RECLAIMABLE_TOTAL,
                        params![request.account.as_bytes().as_slice()],
                        |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
                    )
                    .map_err(sql)?;
                if u64::try_from(bytes).map_err(|_| ports::Error::Corrupt)? < needed_bytes
                    || u64::try_from(count).map_err(|_| ports::Error::Corrupt)? < needed_blobs
                {
                    return Ok(None);
                }
                let mut statement = db.prepare(OLDEST_RECLAIMABLE).map_err(sql)?;
                let mut rows = statement
                    .query(params![request.account.as_bytes().as_slice()])
                    .map_err(sql)?;
                let Some(row) = rows.next().map_err(sql)? else {
                    return Ok(None);
                };
                let id = row
                    .get_ref(0)
                    .map_err(sql)?
                    .as_blob()
                    .map_err(|_| ports::Error::Corrupt)?;
                let id = BlobId::from_bytes(id.try_into().map_err(|_| ports::Error::Corrupt)?);
                let length = u64::try_from(row.get::<_, i64>(1).map_err(sql)?)
                    .map_err(|_| ports::Error::Corrupt)?;
                Ok(Some((id, length)))
            })
        })?;
        drop(view);
        let Some((id, length)) = candidate else {
            return Ok(Room::Unavailable);
        };
        let now = check_time(&self.store, request.deadline, last)?;
        let cleanup = self.retire_upload(
            crypto,
            &mut state,
            Retirement {
                id,
                length,
                body_removed: true,
                identity,
                now,
                deadline: request.deadline,
            },
        )?;
        // Guard remains held until durable deletion and ledger settlement finish.
        drop(guard);
        if cleanup.admission_stopped {
            return Err(ports::Error::WriterStopped.into());
        }
        cleanup.outcome.map_err(|error| match error {
            CommitError::Rejected(error) => UploadError::Store(error),
            CommitError::Indeterminate(_) => UploadError::Store(ports::Error::WriterStopped),
        })?;
        Ok(Room::Removed)
    }
}

enum Room {
    Fits,
    Removed,
    Unavailable,
}
macro_rules! reclaimable {
    ($select:literal, $tail:literal) => {
        concat!(
            $select,
            " FROM leases l JOIN blobs b ON b.account=l.account AND b.id=l.blob_id
          WHERE l.account=?1
          AND NOT EXISTS(SELECT 1 FROM emails e INDEXED BY emails_blob
              WHERE e.account=b.account AND e.blob_id=b.id)
          AND NOT EXISTS(SELECT 1 FROM submissions s INDEXED BY submissions_blob
              WHERE s.account=b.account AND s.transmitted_blob_id=b.id)",
            $tail,
        )
    };
}
pub(super) const RECLAIMABLE_TOTAL: &str =
    reclaimable!("SELECT COALESCE(SUM(b.length),0),COUNT(*)", "");
pub(super) const OLDEST_RECLAIMABLE: &str = reclaimable!(
    "SELECT b.id,b.length",
    " ORDER BY b.created_at,b.id LIMIT 1"
);

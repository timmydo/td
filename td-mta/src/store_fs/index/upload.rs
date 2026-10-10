//! One reserved device upload, from ingress custody through durable publication.
use super::*;
use crate::{
    admission::{
        logical::{self, Cell as LeaseCell, EffectResult, LeaseId, Leases},
        quota::{Charge, Kind},
        Plan,
    },
    format::row::{LeaseRow, LeaseUse},
    ids::DeviceId,
    ownership::SlotState,
    ports::{Access, Crypto, Digest, Entropy, Principal, Tick, Time},
    store_fs::{IngressSpool, SpoolInput, SpoolWriter},
};

/// Trusted policy adapter. Return a guard only after verifying current upload
/// permission for the account/device and binding the current policy generation.
/// Acquisition must refuse within the supplied deadline; a live guard excludes
/// policy publication and credential revocation.
/// Implementations must not call back into the coordinator while guarded.
pub trait UploadAuthorization {
    type Guard<'a>: UploadGuard
    where
        Self: 'a;
    fn authorize(
        &self,
        account: AccountId,
        device: DeviceId,
        deadline: Deadline,
    ) -> Result<Self::Guard<'_>, ports::Error>;
}
pub trait UploadGuard {
    fn access(&self) -> Access;
}

#[derive(Debug)]
pub enum UploadError {
    Store(ports::Error),
    Ledger(logical::Error),
    Format(format::Error),
}
impl From<ports::Error> for UploadError {
    fn from(value: ports::Error) -> Self {
        Self::Store(value)
    }
}
impl From<logical::Error> for UploadError {
    fn from(value: logical::Error) -> Self {
        Self::Ledger(value)
    }
}
impl From<format::Error> for UploadError {
    fn from(value: format::Error) -> Self {
        Self::Format(value)
    }
}
impl std::fmt::Display for UploadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Store(e) => e.fmt(f),
            Self::Ledger(e) => e.fmt(f),
            Self::Format(e) => e.fmt(f),
        }
    }
}
impl std::error::Error for UploadError {}

/// Passive request bounds; only UploadAuthorization grants permission.
#[derive(Clone, Copy, Debug)]
pub struct UploadRequest {
    pub account: AccountId,
    pub device: DeviceId,
    pub maximum: u64,
    pub deadline: Deadline,
}

pub struct UploadCoordinator<'r, 'a, P> {
    store: IndexStore<'r>,
    ledger: Leases<'a>,
    authorization: P,
    active: bool,
    stopped: bool,
}
impl<'r, 'a, P: UploadAuthorization> UploadCoordinator<'r, 'a, P> {
    /// Cold initialization requires quiescent auxiliary owners and settled effects.
    pub fn new(
        store: IndexStore<'r>,
        plan: &Plan,
        auxiliary: AuxiliaryUsage,
        states: &'a mut [SlotState],
        cells: &'a mut [LeaseCell],
        authorization: P,
        deadline: Deadline,
    ) -> Result<Self, LedgerInitError> {
        if cells.len() < 2 || states.len() < 2 {
            return Err(LedgerInitError::Ledger(logical::Error::Full));
        }
        let ledger = store
            .usage_fence(deadline)
            .map_err(LedgerInitError::Store)?
            .initialize_leases(plan, auxiliary, states, cells)?;
        Ok(Self {
            store,
            ledger,
            authorization,
            active: false,
            stopped: false,
        })
    }
    pub fn admission_stopped(&self) -> bool {
        self.stopped
    }
    pub fn used(&self, kind: Kind) -> Result<u64, logical::Error> {
        self.ledger.used(kind)
    }
    pub fn reserve<'c, 's, C: Crypto>(
        &'c mut self,
        crypto: &'c C,
        entropy: &mut impl Entropy,
        spool: &'s IngressSpool<'r>,
        request: UploadRequest,
    ) -> Result<Upload<'c, 's, 'r, 'a, C, P>, UploadError> {
        let UploadRequest {
            account,
            device,
            maximum,
            deadline,
        } = request;
        if self.stopped {
            return Err(ports::Error::WriterStopped.into());
        }
        if self.active {
            return Err(ports::Error::Busy.into());
        }
        if maximum > spool.capacity().bytes_each {
            return Err(ports::Error::Capacity.into());
        }
        let mut last = self.store.clock.sample()?.monotonic;
        check_time(&self.store, deadline, &mut last)?;
        let access = {
            let guard = self.authorization.authorize(account, device, deadline)?;
            let access = guard.access();
            validate_access(access, account, device)?;
            access
        };
        let identity = self.store.view(account, deadline)?.identity();
        let mut bytes = [0; 16];
        entropy.fill(&mut bytes).map_err(ports::Error::from)?;
        let id = BlobId::from_bytes(bytes);
        let now = check_time(&self.store, deadline, &mut last)?;
        let lease = self.ledger.reserve(
            &[[
                Charge {
                    kind: Kind::BodyBytes,
                    amount: maximum,
                },
                Charge {
                    kind: Kind::BlobCount,
                    amount: 1,
                },
                Charge {
                    kind: Kind::UploadBytes,
                    amount: maximum,
                },
                Charge::ZERO,
            ]],
            deadline,
            now.monotonic,
        )?;
        let writer = match spool.begin(crypto, account, id, deadline) {
            Ok(writer) => writer,
            Err(error) => {
                if self.ledger.cancel(lease).is_err() {
                    self.stopped = true;
                }
                return Err(error.into());
            }
        };
        self.active = true;
        Ok(Upload {
            coordinator: self,
            crypto,
            stage: Stage::Writing(writer),
            lease: Some(lease),
            access,
            identity,
            id,
            maximum,
            length: 0,
            deadline,
            last,
            failed: None,
            replan_allowed: false,
            replanned: false,
        })
    }
}
fn check_time(
    store: &IndexStore<'_>,
    deadline: Deadline,
    last: &mut Tick,
) -> Result<Time, ports::Error> {
    let time = store.clock.sample()?;
    if time.monotonic < *last {
        return Err(ports::Error::Invalid);
    }
    *last = time.monotonic;
    if deadline.expired(time.monotonic) {
        return Err(ports::Error::Deadline);
    }
    Ok(time)
}
fn validate_access(
    access: Access,
    account: AccountId,
    device: DeviceId,
) -> Result<(), ports::Error> {
    if access.account != account || access.principal != Principal::Device(device) {
        return Err(ports::Error::Forbidden);
    }
    Ok(())
}
enum Stage<'s, 'r, D: Digest> {
    Writing(SpoolWriter<'s, 'r, D>),
    Prepared(SpoolInput<'s, 'r>),
    Finished,
}
pub struct Upload<'c, 's, 'r, 'a, C: Crypto, P> {
    coordinator: &'c mut UploadCoordinator<'r, 'a, P>,
    crypto: &'c C,
    stage: Stage<'s, 'r, C::Sha256>,
    lease: Option<LeaseId>,
    access: Access,
    identity: ViewIdentity,
    id: BlobId,
    maximum: u64,
    length: u64,
    deadline: Deadline,
    last: Tick,
    failed: Option<ports::Error>,
    replan_allowed: bool,
    replanned: bool,
}
impl<C: Crypto, P: UploadAuthorization> Upload<'_, '_, '_, '_, C, P> {
    pub fn id(&self) -> BlobId {
        self.id
    }
    pub fn write(&mut self, bytes: &[u8]) -> Result<(), UploadError> {
        check_upload_time(
            &self.coordinator.store,
            self.deadline,
            &mut self.last,
            &mut self.failed,
        )?;
        let next = self
            .length
            .checked_add(bytes.len() as u64)
            .ok_or(ports::Error::Capacity)?;
        if next > self.maximum {
            return Err(ports::Error::Quota.into());
        }
        let Stage::Writing(writer) = &mut self.stage else {
            return Err(ports::Error::Invalid.into());
        };
        writer.write(bytes)?;
        self.length = next;
        Ok(())
    }
    pub fn prepare(&mut self) -> Result<(), UploadError> {
        check_upload_time(
            &self.coordinator.store,
            self.deadline,
            &mut self.last,
            &mut self.failed,
        )?;
        if !matches!(self.stage, Stage::Writing(_)) {
            return Err(ports::Error::Invalid.into());
        }
        let Stage::Writing(writer) = std::mem::replace(&mut self.stage, Stage::Finished) else {
            return Err(ports::Error::Invalid.into());
        };
        self.stage = Stage::Prepared(writer.finish()?);
        Ok(())
    }
}

/// A durable result remains visible even if accounting or spool cleanup stops work.
#[derive(Debug)]
#[must_use = "inspect the durable outcome before acknowledging or retrying"]
pub struct UploadCompletion {
    pub id: BlobId,
    pub length: u64,
    pub digest: [u8; 32],
    pub expires_at: i64,
    pub outcome: Result<Sequence, CommitError>,
    pub admission_stopped: bool,
    pub cleanup_error: Option<ports::Error>,
}
#[derive(Debug)]
#[must_use = "distinguish retained input from a completed commit attempt"]
pub enum UploadAttempt {
    /// Prepared input and unused logical reservation remain owned by this upload.
    SequenceConflict,
    Complete(UploadCompletion),
}
impl<C: Crypto, P: UploadAuthorization> Upload<'_, '_, '_, '_, C, P> {
    pub fn replan(&mut self) -> Result<(), UploadError> {
        if !self.replan_allowed || self.replanned || self.coordinator.stopped {
            return Err(ports::Error::Invalid.into());
        }
        check_upload_time(
            &self.coordinator.store,
            self.deadline,
            &mut self.last,
            &mut self.failed,
        )?;
        let Principal::Device(device) = self.access.principal else {
            return Err(ports::Error::Forbidden.into());
        };
        let guard =
            self.coordinator
                .authorization
                .authorize(self.access.account, device, self.deadline)?;
        if guard.access() != self.access {
            return Err(ports::Error::Forbidden.into());
        }
        let identity = self
            .coordinator
            .store
            .view(self.access.account, self.deadline)?
            .identity();
        if identity.epoch != self.identity.epoch {
            return Err(ports::Error::Conflict.into());
        }
        let Stage::Prepared(input) = &mut self.stage else {
            return Err(ports::Error::Invalid.into());
        };
        input.rewind()?;
        check_upload_time(
            &self.coordinator.store,
            self.deadline,
            &mut self.last,
            &mut self.failed,
        )?;
        self.identity = identity;
        self.replanned = true;
        self.replan_allowed = false;
        Ok(())
    }
    pub fn commit(&mut self) -> Result<UploadAttempt, UploadError> {
        if self.coordinator.stopped {
            return Err(ports::Error::WriterStopped.into());
        }
        if self.replan_allowed {
            return Err(ports::Error::Conflict.into());
        }
        let Stage::Prepared(input) = &mut self.stage else {
            return Err(ports::Error::Invalid.into());
        };
        if input.account() != self.access.account
            || input.id() != self.id
            || input.len() != self.length
            || input.len() > self.maximum
            || self.identity.epoch != self.coordinator.store.epoch()
        {
            return Err(ports::Error::Invalid.into());
        }
        check_upload_time(
            &self.coordinator.store,
            self.deadline,
            &mut self.last,
            &mut self.failed,
        )?;
        let Principal::Device(device) = self.access.principal else {
            return Err(ports::Error::Forbidden.into());
        };
        let guard =
            self.coordinator
                .authorization
                .authorize(self.access.account, device, self.deadline)?;
        if guard.access() != self.access {
            return Err(ports::Error::Forbidden.into());
        }
        let time = check_upload_time(
            &self.coordinator.store,
            self.deadline,
            &mut self.last,
            &mut self.failed,
        )?;
        let expires_at = time
            .utc_ms
            .checked_add(24 * 60 * 60 * 1000)
            .ok_or(ports::Error::Invalid)?;
        let digest = *input.digest();
        let mut blob_bytes = [0; 48];
        let blob_len = Row::Blob(input.row(time.utc_ms)).encode(&mut blob_bytes)?;
        let mut lease_bytes = [0; 41];
        let lease_len = Row::Lease(LeaseRow {
            account: self.access.account,
            device,
            expires_at,
            uses: LeaseUse::Both,
        })
        .encode(&mut lease_bytes)?;
        let operations = [
            Operation::put(
                Table::Blobs,
                self.id.as_bytes(),
                blob_bytes.get(..blob_len).ok_or(ports::Error::Invalid)?,
            )?,
            Operation::put(
                Table::Leases,
                self.id.as_bytes(),
                lease_bytes.get(..lease_len).ok_or(ports::Error::Invalid)?,
            )?,
        ];
        let planned = [self.length, 1, self.length, 0];
        let lease = self.lease.ok_or(ports::Error::Invalid)?;
        let logical_part = self.coordinator.ledger.part(lease, 0)?;
        let database = self
            .coordinator
            .ledger
            .remaining_capacity(Kind::DatabaseBytes)?;
        let wal = self.coordinator.ledger.remaining_capacity(Kind::WalBytes)?;
        let physical_lease = self.coordinator.ledger.reserve(
            &[[
                Charge {
                    kind: Kind::DatabaseBytes,
                    amount: database,
                },
                Charge {
                    kind: Kind::WalBytes,
                    amount: wal,
                },
                Charge::ZERO,
                Charge::ZERO,
            ]],
            self.deadline,
            time.monotonic,
        )?;
        let physical_ticket = self
            .coordinator
            .ledger
            .part(physical_lease, 0)
            .and_then(|part| {
                self.coordinator
                    .ledger
                    .begin_effect(part, [database, wal, 0, 0], time.monotonic)
            });
        let mut physical_ticket = match physical_ticket {
            Ok(ticket) => ticket,
            Err(error) => {
                if self.coordinator.ledger.cancel(physical_lease).is_err() {
                    self.coordinator.stopped = true;
                }
                return Err(error.into());
            }
        };
        let mut logical_ticket =
            match self
                .coordinator
                .ledger
                .begin_effect(logical_part, planned, time.monotonic)
            {
                Ok(ticket) => ticket,
                Err(error) => {
                    if self
                        .coordinator
                        .ledger
                        .complete_effect(&mut physical_ticket, EffectResult::Proven([0; 4]))
                        .is_err()
                        || self.coordinator.ledger.cancel(physical_lease).is_err()
                    {
                        self.coordinator.stopped = true;
                    }
                    return Err(error.into());
                }
            };
        let completion = self.coordinator.store.commit_with_files(
            self.crypto,
            CommitRequest {
                account: self.access.account,
                epoch: self.identity.epoch,
                expected: self.identity.committed_sequence,
                utc_ms: time.utc_ms,
                deadline: self.deadline,
            },
            &operations,
            &mut [BlobSource {
                id: self.id,
                source: input,
            }],
        );
        drop(guard);
        let physical = match completion.files() {
            CommitFileUsage::Unchanged => self
                .coordinator
                .ledger
                .complete_effect(&mut physical_ticket, EffectResult::Proven([0; 4])),
            CommitFileUsage::Measured(files) => self.coordinator.ledger.complete_physical(
                &mut physical_ticket,
                files.database_bytes,
                files.wal_bytes,
            ),
            CommitFileUsage::Unavailable(_) => {
                self.coordinator.stopped = true;
                self.coordinator
                    .ledger
                    .complete_effect(&mut physical_ticket, EffectResult::Uncertain)
            }
        };
        if physical.is_err() {
            self.coordinator.stopped = true;
            let _ = self
                .coordinator
                .ledger
                .complete_effect(&mut physical_ticket, EffectResult::Uncertain);
        }
        if self.coordinator.ledger.cancel(physical_lease).is_err() {
            self.coordinator.stopped = true;
        }
        let outcome = completion.outcome();
        let effect = match outcome {
            Ok(_) => EffectResult::Proven(planned),
            Err(CommitError::Rejected(_)) => EffectResult::Proven([0; 4]),
            Err(CommitError::Indeterminate(_)) => {
                self.coordinator.stopped = true;
                EffectResult::Uncertain
            }
        };
        if self
            .coordinator
            .ledger
            .complete_effect(&mut logical_ticket, effect)
            .is_err()
        {
            self.coordinator.stopped = true;
        }
        self.coordinator.stopped |= completion.writer_stopped();
        if completion.is_sequence_conflict() && !self.coordinator.stopped && !self.replanned {
            self.replan_allowed = true;
            return Ok(UploadAttempt::SequenceConflict);
        }
        if self.coordinator.ledger.cancel(lease).is_err() {
            self.coordinator.stopped = true;
        }
        self.lease = None;
        let cleanup_error =
            discard_stage(std::mem::replace(&mut self.stage, Stage::Finished)).err();
        self.coordinator.active = false;
        Ok(UploadAttempt::Complete(UploadCompletion {
            id: self.id,
            length: self.length,
            digest,
            expires_at,
            outcome,
            admission_stopped: self.coordinator.stopped,
            cleanup_error,
        }))
    }
    pub fn discard(mut self) -> Result<(), UploadError> {
        let cleanup = discard_stage(std::mem::replace(&mut self.stage, Stage::Finished));
        if let Some(lease) = self.lease.take() {
            if let Err(error) = self.coordinator.ledger.cancel(lease) {
                self.coordinator.stopped = true;
                return Err(error.into());
            }
        }
        self.coordinator.active = false;
        cleanup.map_err(UploadError::Store)
    }
}
fn discard_stage<D: Digest>(stage: Stage<'_, '_, D>) -> Result<(), ports::Error> {
    match stage {
        Stage::Writing(writer) => writer.discard(),
        Stage::Prepared(input) => input.discard(),
        Stage::Finished => Ok(()),
    }
}
impl<C: Crypto, P> Drop for Upload<'_, '_, '_, '_, C, P> {
    fn drop(&mut self) {
        let _ = discard_stage(std::mem::replace(&mut self.stage, Stage::Finished));
        if let Some(lease) = self.lease.take() {
            if self.coordinator.ledger.cancel(lease).is_err() {
                self.coordinator.stopped = true;
            }
        }
        self.coordinator.active = false;
    }
}

fn check_upload_time(
    store: &IndexStore<'_>,
    deadline: Deadline,
    last: &mut Tick,
    failed: &mut Option<ports::Error>,
) -> Result<Time, ports::Error> {
    if let Some(error) = *failed {
        return Err(error);
    }
    check_time(store, deadline, last).inspect_err(|error| {
        *failed = Some(*error);
    })
}

#[cfg(test)]
#[path = "upload_tests.rs"]
mod tests;

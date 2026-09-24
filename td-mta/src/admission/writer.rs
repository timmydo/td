//! Scalar writer admission. M05/M08 supply filesystem and publication authority;
//! this state machine alone never authorizes I/O or a checkpoint selection.
use super::space::RoundedGrowth;
use super::{
    logical::{
        self, Cell, Charges, EffectResult, EffectTicket, LeaseId, Leases, PartId, Physical,
        MAX_GROUP_CELLS,
    },
    quota::{Charge, Kind, Usage},
    space, Plan,
};
use crate::{
    format,
    ownership::SlotState,
    ports::{Deadline, Tick},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Logical(logical::Error),
    Arithmetic(super::Error),
    Invalid,
    Closed,
    Busy,
    Stopped,
}
impl From<logical::Error> for Error {
    fn from(e: logical::Error) -> Self {
        Self::Logical(e)
    }
}
impl From<super::Error> for Error {
    fn from(e: super::Error) -> Self {
        Self::Arithmetic(e)
    }
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Logical(e) => e.fmt(f),
            Self::Arithmetic(e) => e.fmt(f),
            Self::Invalid => f.write_str("invalid writer transition"),
            Self::Closed => f.write_str("writer admission closed"),
            Self::Busy => f.write_str("journal append in flight"),
            Self::Stopped => f.write_str("writer stopped for recovery"),
        }
    }
}
impl std::error::Error for Error {}

/// One transaction's conservative frame/operation ceilings, excluding the
/// segment header. The codec still validates actual serialized frame contents.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FrameBudget {
    bytes: u64,
    operations: u64,
}
impl FrameBudget {
    pub fn new(bytes: u64, operations: u64) -> Result<Self, Error> {
        if bytes < super::widen(format::MIN_FRAME_BYTES, "minimum frame")?
            || bytes > super::widen(format::MAX_FRAME_BYTES, "maximum frame")?
            || operations == 0
            || operations > super::widen(format::MAX_FRAME_OPERATIONS, "frame operations")?
        {
            return Err(Error::Invalid);
        }
        let minimum = super::add(
            super::widen(
                format::FRAME_HEADER_BYTES + format::FRAME_FOOTER_BYTES,
                "frame envelope",
            )?,
            super::mul(
                operations,
                super::widen(
                    format::OPERATION_HEADER_BYTES + format::MIN_KEY_BYTES,
                    "minimum operation",
                )?,
                "minimum frame payload",
            )?,
            "minimum frame",
        )?;
        if bytes < minimum {
            return Err(Error::Invalid);
        }
        Ok(Self { bytes, operations })
    }
    pub fn bytes(self) -> u64 {
        self.bytes
    }
    pub fn operations(self) -> u64 {
        self.operations
    }
    fn charges(self) -> Charges {
        [
            Charge {
                kind: Kind::ActiveJournalBytes,
                amount: self.bytes,
            },
            Charge {
                kind: Kind::ActiveJournalOperations,
                amount: self.operations,
            },
            Charge::ZERO,
            Charge::ZERO,
        ]
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CheckpointNeed {
    generation_bytes: u64,
    fresh_journal_bytes: u64,
}
impl CheckpointNeed {
    /// Unrounded bound for the eleven tables, manifest and CURRENT temporary.
    pub fn generation_bytes(self) -> u64 {
        self.generation_bytes
    }
    /// Separate from generation output; closing a journal does not grow it.
    pub fn fresh_journal_bytes(self) -> u64 {
        self.fresh_journal_bytes
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FrameId(PartId);
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[must_use = "retain the grant until cancellation or reconciled completion"]
pub struct Grant {
    lease: LeaseId,
    frame: Option<FrameId>,
}
impl Grant {
    pub fn lease(self) -> LeaseId {
        self.lease
    }
    pub fn frame(self) -> Option<FrameId> {
        self.frame
    }
}
#[derive(Debug)]
#[must_use = "complete the append ticket; dropping it keeps the writer busy"]
pub struct AppendTicket {
    frame: FrameId,
    effect: EffectTicket,
    actual: FrameBudget,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AppendResult {
    /// M08 proves the entire planned frame durably committed.
    Synced,
    /// M08 proves no bytes were appended; a write/sync error is not this proof.
    NotWritten,
    /// Preserve the pending ticket and stop all writer admission for recovery.
    Uncertain,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Phase {
    Open,
    Barrier,
    AwaitingSpace,
    Stopped,
}

pub struct WriterLedger<'a> {
    leases: Leases<'a>,
    plan: &'a Plan,
    selected_tables: u64,
    phase: Phase,
    appending: Option<FrameId>,
}
impl<'a> WriterLedger<'a> {
    /// Selected lengths and used quotas come from the same trusted recovery
    /// snapshot. Active journal use counts frames, not the segment header.
    pub fn new(
        plan: &'a Plan,
        selected_tables: u64,
        used: Usage,
        states: &'a mut [SlotState],
        cells: &'a mut [Cell],
    ) -> Result<Self, Error> {
        if selected_tables
            < super::widen(
                format::TABLE_COUNT * format::TABLE_HEADER_BYTES,
                "empty tables",
            )?
            || selected_tables > plan.disk().live_metadata_bytes
            || (used.get(Kind::ActiveJournalBytes)? == 0)
                != (used.get(Kind::ActiveJournalOperations)? == 0)
        {
            return Err(Error::Invalid);
        }
        let leases = Leases::new(plan, used, states, cells)?;
        let ledger = Self {
            leases,
            plan,
            selected_tables,
            phase: Phase::Open,
            appending: None,
        };
        ledger.checkpoint_need()?;
        Ok(ledger)
    }
    pub(super) fn plan(&self) -> &'a Plan {
        self.plan
    }
    pub(super) fn pristine(&self) -> bool {
        self.phase == Phase::Open && self.appending.is_none() && self.leases.is_empty()
    }
    pub(super) fn physical(&self, part: PartId) -> Result<Physical, Error> {
        Ok(self.leases.physical(part)?)
    }
    pub(super) fn frame_part(frame: FrameId) -> PartId {
        frame.0
    }
    pub(super) fn physical_group(
        &self,
        lease: LeaseId,
    ) -> Result<[Option<Physical>; MAX_GROUP_CELLS], Error> {
        Ok(self.leases.physical_group(lease)?)
    }
    pub(super) fn extend_bound(
        &mut self,
        part: PartId,
        amounts: [u64; 4],
        extra: RoundedGrowth,
        now: Tick,
    ) -> Result<(), Error> {
        self.open()?;
        ordinary(&self.leases.remaining(part)?)?;
        Ok(self.leases.extend_bound(part, amounts, extra, now)?)
    }
    pub(super) fn complete_bound(
        &mut self,
        ticket: &mut EffectTicket,
        result: EffectResult,
        physical: RoundedGrowth,
    ) -> Result<(), Error> {
        Ok(self
            .leases
            .complete_bound(ticket, result, physical, false)?)
    }
    pub fn phase(&self) -> Phase {
        self.phase
    }
    pub fn used(&self, kind: Kind) -> Result<u64, Error> {
        Ok(self.leases.used(kind)?)
    }
    pub fn pending(&self, kind: Kind) -> Result<u64, Error> {
        Ok(self.leases.pending(kind)?)
    }
    pub fn selected_tables(&self) -> u64 {
        self.selected_tables
    }
    fn open(&self) -> Result<(), Error> {
        match self.phase {
            Phase::Open => Ok(()),
            Phase::Stopped => Err(Error::Stopped),
            _ => Err(Error::Closed),
        }
    }
    fn need_from(&self, pending: &Usage) -> Result<CheckpointNeed, Error> {
        let bytes = super::add(
            self.leases.used(Kind::ActiveJournalBytes)?,
            pending.get(Kind::ActiveJournalBytes)?,
            "checkpoint frames",
        )?;
        let operations = super::add(
            self.leases.used(Kind::ActiveJournalOperations)?,
            pending.get(Kind::ActiveJournalOperations)?,
            "checkpoint operations",
        )?;
        Ok(CheckpointNeed {
            generation_bytes: space::checkpoint_bytes(self.selected_tables, bytes, operations)?,
            fresh_journal_bytes: super::widen(format::JOURNAL_HEADER_BYTES, "fresh journal")?,
        })
    }
    pub fn checkpoint_need(&self) -> Result<CheckpointNeed, Error> {
        self.need_from(self.leases.quotas().pending())
    }
    /// Holds the exclusive ledger borrow while the composed coordinator checks
    /// fresh physical observations. Dropping preparation changes no state.
    pub fn prepare<'b>(
        &'b mut self,
        requests: &[Charges],
        frame: Option<FrameBudget>,
        deadline: Deadline,
        now: Tick,
    ) -> Result<Prepared<'b, 'a>, Error> {
        self.open()?;
        let count = requests
            .len()
            .checked_add(usize::from(frame.is_some()))
            .ok_or(Error::Invalid)?;
        if count == 0 || count > MAX_GROUP_CELLS {
            return Err(Error::Invalid);
        }
        if deadline.expired(now) {
            return Err(logical::Error::Expired.into());
        }
        for request in requests {
            ordinary(request)?;
        }
        let mut all = [[Charge::ZERO; 4]; MAX_GROUP_CELLS];
        for (out, request) in all.iter_mut().zip(requests) {
            *out = *request;
        }
        if let Some(frame) = frame {
            *all.get_mut(requests.len()).ok_or(Error::Invalid)? = frame.charges();
        }
        let charges = all.get(..count).ok_or(Error::Invalid)?;
        let projected = self.leases.project(charges)?;
        let need = self.need_from(projected.pending())?;
        Ok(Prepared {
            ledger: self,
            charges: all,
            count,
            has_frame: frame.is_some(),
            deadline,
            need,
        })
    }
    pub fn part(&self, grant: Grant, ordinal: u16) -> Result<PartId, Error> {
        Ok(self.leases.part(grant.lease, ordinal)?)
    }
    pub fn remaining(&self, part: PartId) -> Result<Charges, Error> {
        Ok(self.leases.remaining(part)?)
    }
    pub fn cancel(&mut self, grant: Grant) -> Result<(), Error> {
        Ok(self.leases.cancel(grant.lease)?)
    }
    pub fn expired(&self, now: Tick, output: &mut [Option<LeaseId>]) -> Result<usize, Error> {
        Ok(self.leases.expired(now, output)?)
    }
    pub fn cancel_lease(&mut self, lease: LeaseId) -> Result<(), Error> {
        Ok(self.leases.cancel(lease)?)
    }
    /// Admitted non-journal work may finish while a checkpoint barrier is held.
    pub fn begin_effect(
        &mut self,
        part: PartId,
        amounts: [u64; 4],
        now: Tick,
    ) -> Result<EffectTicket, Error> {
        if self.phase == Phase::Stopped {
            return Err(Error::Stopped);
        }
        ordinary(&self.leases.remaining(part)?)?;
        Ok(self.leases.begin_effect(part, amounts, now)?)
    }
    pub fn complete_effect(
        &mut self,
        ticket: &mut EffectTicket,
        result: EffectResult,
    ) -> Result<(), Error> {
        Ok(self.leases.complete_effect(ticket, result)?)
    }
    pub fn begin_append(
        &mut self,
        frame: FrameId,
        actual: FrameBudget,
        now: Tick,
    ) -> Result<AppendTicket, Error> {
        self.open()?;
        if self.appending.is_some() {
            return Err(Error::Busy);
        }
        let effect =
            self.leases
                .begin_effect(frame.0, [actual.bytes, actual.operations, 0, 0], now)?;
        self.appending = Some(frame);
        Ok(AppendTicket {
            frame,
            effect,
            actual,
        })
    }
    pub fn complete_append(
        &mut self,
        ticket: &mut AppendTicket,
        result: AppendResult,
    ) -> Result<(), Error> {
        self.complete_append_inner(ticket, result, None)
    }
    pub(super) fn complete_append_bound(
        &mut self,
        ticket: &mut AppendTicket,
        result: AppendResult,
        physical: Option<RoundedGrowth>,
    ) -> Result<(), Error> {
        self.complete_append_inner(ticket, result, physical)
    }
    fn complete_append_inner(
        &mut self,
        ticket: &mut AppendTicket,
        result: AppendResult,
        physical: Option<RoundedGrowth>,
    ) -> Result<(), Error> {
        self.open()?;
        if self.appending != Some(ticket.frame) {
            return Err(Error::Invalid);
        }
        self.leases.check_effect(&ticket.effect)?;
        let effect = match result {
            AppendResult::Synced => {
                EffectResult::Proven([ticket.actual.bytes, ticket.actual.operations, 0, 0])
            }
            AppendResult::NotWritten => EffectResult::Proven([0; 4]),
            AppendResult::Uncertain => {
                self.phase = Phase::Stopped;
                return Ok(());
            }
        };
        if let Some(physical) = physical {
            self.leases.complete_bound(
                &mut ticket.effect,
                effect,
                physical,
                result == AppendResult::Synced,
            )?;
        } else if result == AppendResult::Synced {
            self.leases.complete_frame(&mut ticket.effect, effect)?;
        } else {
            self.leases.complete_effect(&mut ticket.effect, effect)?;
        }
        self.appending = None;
        Ok(())
    }
    /// M08 must hold the actual writer/view barrier and prove pin eligibility.
    /// This only closes scalar admission; it grants no building I/O permission.
    pub fn begin_checkpoint(&mut self) -> Result<Checkpoint<'_, 'a>, Error> {
        self.open()?;
        if self.appending.is_some() {
            return Err(Error::Busy);
        }
        self.leases.project_rollover()?;
        let bound = self.checkpoint_need()?;
        let selected_bound = self.need_from(&Usage::default())?.generation_bytes();
        self.phase = Phase::Barrier;
        Ok(Checkpoint {
            ledger: self,
            bound,
            selected_bound,
        })
    }
}
fn ordinary(charges: &Charges) -> Result<(), Error> {
    if charges.iter().any(|c| {
        matches!(
            c.kind,
            Kind::ActiveJournalBytes
                | Kind::ActiveJournalOperations
                | Kind::ClosedJournalBytes
                | Kind::ClosedJournalSegments
                | Kind::CheckpointBytes
                | Kind::LiveMetadataBytes
        )
    }) {
        return Err(Error::Invalid);
    }
    Ok(())
}

#[must_use = "install the checked preparation or drop it without effects"]
pub struct Prepared<'b, 'a> {
    ledger: &'b mut WriterLedger<'a>,
    charges: [Charges; MAX_GROUP_CELLS],
    count: usize,
    has_frame: bool,
    deadline: Deadline,
    need: CheckpointNeed,
}
impl Prepared<'_, '_> {
    pub fn checkpoint_need(&self) -> CheckpointNeed {
        self.need
    }
    /// The caller must first pass the composed physical-space gate. This helper
    /// installs logical state only; CAS refusal leaves all counters unchanged.
    pub fn install(self, now: Tick) -> Result<Grant, Error> {
        self.install_inner(None, now)
    }
    pub(super) fn install_bound(self, physical: &[Physical], now: Tick) -> Result<Grant, Error> {
        self.install_inner(Some(physical), now)
    }
    fn install_inner(self, physical: Option<&[Physical]>, now: Tick) -> Result<Grant, Error> {
        let issued = self.ledger.leases.reserve_group(
            self.charges.get(..self.count).ok_or(Error::Invalid)?,
            physical,
            self.has_frame,
            self.deadline,
            now,
        )?;
        Ok(Grant {
            lease: issued.lease,
            frame: issued.frame.map(FrameId),
        })
    }
}

#[must_use = "finish or abort the checkpoint; dropping it stops the writer"]
pub struct Checkpoint<'b, 'a> {
    ledger: &'b mut WriterLedger<'a>,
    bound: CheckpointNeed,
    selected_bound: u64,
}
impl Drop for Checkpoint<'_, '_> {
    fn drop(&mut self) {
        if self.ledger.phase == Phase::Barrier {
            self.ledger.phase = Phase::Stopped;
        }
    }
}
impl Checkpoint<'_, '_> {
    pub fn checkpoint_need(&self) -> CheckpointNeed {
        self.bound
    }
    pub fn phase(&self) -> Phase {
        self.ledger.phase()
    }
    pub fn used(&self, kind: Kind) -> Result<u64, Error> {
        self.ledger.used(kind)
    }
    pub fn pending(&self, kind: Kind) -> Result<u64, Error> {
        self.ledger.pending(kind)
    }
    pub fn part(&self, grant: Grant, ordinal: u16) -> Result<PartId, Error> {
        self.ledger.part(grant, ordinal)
    }
    pub fn expired(&self, now: Tick, output: &mut [Option<LeaseId>]) -> Result<usize, Error> {
        self.ledger.expired(now, output)
    }
    pub fn cancel_lease(&mut self, lease: LeaseId) -> Result<(), Error> {
        self.ledger.cancel_lease(lease)
    }
    pub fn cancel(&mut self, grant: Grant) -> Result<(), Error> {
        self.ledger.cancel(grant)
    }
    pub fn remaining(&self, part: PartId) -> Result<Charges, Error> {
        self.ledger.remaining(part)
    }
    pub fn begin_effect(
        &mut self,
        part: PartId,
        amounts: [u64; 4],
        now: Tick,
    ) -> Result<EffectTicket, Error> {
        self.ledger.begin_effect(part, amounts, now)
    }
    pub fn complete_effect(
        &mut self,
        ticket: &mut EffectTicket,
        result: EffectResult,
    ) -> Result<(), Error> {
        self.ledger.complete_effect(ticket, result)
    }
    pub fn abort_before_selection(self) {
        self.ledger.phase = Phase::Open;
    }
    /// M08 supplies the selected table length only after durable selection.
    /// The returned ledger stays closed: b3 must protect the next checkpoint
    /// from a fresh physical probe before it can reopen admission.
    pub fn selected(self, selected_tables: u64) -> Result<(), Error> {
        // The adapter reports an already durable selection. Any inconsistency
        // now needs recovery, not another attempt against the old journal.
        self.ledger.phase = Phase::Stopped;
        if selected_tables
            < super::widen(
                format::TABLE_COUNT * format::TABLE_HEADER_BYTES,
                "empty tables",
            )?
            || selected_tables > self.ledger.plan.disk().live_metadata_bytes
            || selected_tables > self.selected_bound
        {
            return Err(Error::Invalid);
        }
        self.ledger.leases.rollover()?;
        self.ledger.selected_tables = selected_tables;
        self.ledger.phase = Phase::AwaitingSpace;
        Ok(())
    }
    /// Uncertain selection cannot safely resume either active journal.
    pub fn uncertain(self) {
        self.ledger.phase = Phase::Stopped;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        admission::{DiskLimits, ViewMode, WorkLimits},
        limits::Limits,
        ownership,
    };
    fn plan() -> Result<Plan, Box<dyn std::error::Error>> {
        Ok(DiskLimits::default().plan(
            &Limits::default().plan()?,
            WorkLimits::default(),
            ViewMode::OnlineBackground,
        )?)
    }
    fn recovered(bytes: u64, operations: u64) -> Result<Usage, Error> {
        let mut used = Usage::default();
        used.add(Kind::ActiveJournalBytes, bytes)?;
        used.add(Kind::ActiveJournalOperations, operations)?;
        used.add(Kind::LiveMetadataBytes, 1232)?;
        Ok(used)
    }
    fn charge(kind: Kind, amount: u64) -> Charges {
        [
            Charge { kind, amount },
            Charge::ZERO,
            Charge::ZERO,
            Charge::ZERO,
        ]
    }
    fn deadline() -> Result<Deadline, crate::ports::Error> {
        Deadline::after(Tick(0), 100)
    }
    fn grant(
        ledger: &mut WriterLedger<'_>,
        requests: &[Charges],
        frame: Option<FrameBudget>,
    ) -> Result<Grant, Error> {
        for _ in 0..1000 {
            let prepared = ledger.prepare(
                requests,
                frame,
                deadline().map_err(|_| Error::Invalid)?,
                Tick(0),
            )?;
            match prepared.install(Tick(1)) {
                Err(Error::Logical(logical::Error::Slot(ownership::Error::Contended))) => {
                    std::thread::yield_now()
                }
                result => return result,
            }
        }
        Err(Error::Busy)
    }
    #[test]
    fn candidate_bound_includes_used_pending_and_new_frames_without_mutating(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let p = plan()?;
        let mut states = [const { SlotState::EMPTY }; 8];
        let mut cells = [const { Cell::EMPTY }; 8];
        let mut ledger = WriterLedger::new(&p, 1232, recovered(1000, 3)?, &mut states, &mut cells)?;
        let first = grant(&mut ledger, &[], Some(FrameBudget::new(2000, 4)?))?;
        let need = ledger.checkpoint_need()?;
        assert_eq!(need.generation_bytes(), 1232 + 3000 + 7 * 36 + 1048576);
        assert_eq!(need.fresh_journal_bytes(), 96);
        {
            let prepared =
                ledger.prepare(&[], Some(FrameBudget::new(3000, 5)?), deadline()?, Tick(0))?;
            assert_eq!(
                prepared.checkpoint_need().generation_bytes(),
                1232 + 6000 + 12 * 36 + 1048576
            );
        }
        assert_eq!(ledger.checkpoint_need()?, need);
        assert_eq!(ledger.pending(Kind::ActiveJournalBytes)?, 2000);
        ledger.cancel(first)?;
        assert_eq!(ledger.pending(Kind::ActiveJournalBytes)?, 0);
        Ok(())
    }
    #[test]
    fn dedicated_append_serializes_and_moves_only_proven_frames(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let p = plan()?;
        let mut states = [const { SlotState::EMPTY }; 8];
        let mut cells = [const { Cell::EMPTY }; 8];
        let mut ledger = WriterLedger::new(&p, 1232, recovered(0, 0)?, &mut states, &mut cells)?;
        let job = grant(&mut ledger, &[], Some(FrameBudget::new(1000, 4)?))?;
        let frame = job.frame().ok_or("frame")?;
        assert!(matches!(
            ledger.begin_effect(frame.0, [1000, 4, 0, 0], Tick(1)),
            Err(Error::Invalid)
        ));
        let before = ledger.checkpoint_need()?;
        let mut ticket = ledger.begin_append(frame, FrameBudget::new(500, 2)?, Tick(1))?;
        assert!(matches!(ledger.begin_checkpoint(), Err(Error::Busy)));
        assert!(matches!(
            ledger.begin_append(frame, FrameBudget::new(500, 2)?, Tick(1)),
            Err(Error::Busy)
        ));
        ledger.complete_append(&mut ticket, AppendResult::NotWritten)?;
        assert_eq!(ledger.used(Kind::ActiveJournalBytes)?, 0);
        assert_eq!(ledger.pending(Kind::ActiveJournalBytes)?, 1000);
        assert_eq!(
            ledger.complete_append(&mut ticket, AppendResult::Synced),
            Err(Error::Invalid)
        );
        let mut ticket = ledger.begin_append(frame, FrameBudget::new(500, 2)?, Tick(1))?;
        ledger.complete_append(&mut ticket, AppendResult::Synced)?;
        assert_eq!(ledger.used(Kind::ActiveJournalBytes)?, 500);
        assert_eq!(ledger.pending(Kind::ActiveJournalBytes)?, 0);
        assert!(ledger.checkpoint_need()?.generation_bytes() < before.generation_bytes());
        assert!(matches!(
            ledger.begin_append(frame, FrameBudget::new(132, 1)?, Tick(2)),
            Err(Error::Logical(logical::Error::Invalid))
        ));
        ledger.cancel(job)?;
        assert_eq!(ledger.used(Kind::ActiveJournalBytes)?, 500);
        assert_eq!(ledger.pending(Kind::ActiveJournalOperations)?, 0);
        assert_eq!(
            ledger.checkpoint_need()?.generation_bytes(),
            1232 + 500 + 72 + 1048576
        );
        Ok(())
    }
    #[test]
    fn uncertain_append_stops_writer_and_preserves_pending_ownership(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let p = plan()?;
        let mut states = [const { SlotState::EMPTY }; 4];
        let mut cells = [const { Cell::EMPTY }; 4];
        let mut ledger = WriterLedger::new(&p, 1232, recovered(0, 0)?, &mut states, &mut cells)?;
        let job = grant(&mut ledger, &[], Some(FrameBudget::new(1000, 4)?))?;
        let mut ticket = ledger.begin_append(
            job.frame().ok_or("frame")?,
            FrameBudget::new(500, 2)?,
            Tick(1),
        )?;
        ledger.complete_append(&mut ticket, AppendResult::Uncertain)?;
        assert_eq!(ledger.phase(), Phase::Stopped);
        assert_eq!(ledger.used(Kind::ActiveJournalBytes)?, 0);
        assert_eq!(ledger.pending(Kind::ActiveJournalBytes)?, 1000);
        assert_eq!(
            ledger.cancel(job),
            Err(Error::Logical(logical::Error::Busy))
        );
        assert_eq!(
            ledger.complete_append(&mut ticket, AppendResult::Synced),
            Err(Error::Stopped)
        );
        assert!(matches!(
            ledger.prepare(&[], Some(FrameBudget::new(500, 1)?), deadline()?, Tick(1)),
            Err(Error::Stopped)
        ));
        assert!(matches!(ledger.begin_checkpoint(), Err(Error::Stopped)));
        Ok(())
    }
    #[test]
    fn rollover_counts_header_preserves_live_frames_and_stays_closed(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let p = plan()?;
        let mut states = [const { SlotState::EMPTY }; 8];
        let mut cells = [const { Cell::EMPTY }; 8];
        let mut used = recovered(1000, 4)?;
        used.add(Kind::ClosedJournalBytes, 5000)?;
        used.add(Kind::ClosedJournalSegments, 2)?;
        let mut ledger = WriterLedger::new(&p, 1232, used, &mut states, &mut cells)?;
        let job = grant(
            &mut ledger,
            &[charge(Kind::BodyBytes, 100)],
            Some(FrameBudget::new(2000, 7)?),
        )?;
        let canceled = grant(&mut ledger, &[], Some(FrameBudget::new(3000, 8)?))?;
        let part = ledger.part(job, 0)?;
        let frame = job.frame().ok_or("frame")?;
        let remaining = ledger.remaining(frame.0)?;
        let mut barrier = ledger.begin_checkpoint()?;
        barrier.cancel(canceled)?;
        let mut raw = barrier.begin_effect(part, [100, 0, 0, 0], Tick(2))?;
        barrier.complete_effect(&mut raw, EffectResult::Proven([40, 0, 0, 0]))?;
        barrier.selected(1500)?;
        assert_eq!(ledger.phase(), Phase::AwaitingSpace);
        let mut remainder = ledger.begin_effect(part, [60, 0, 0, 0], Tick(3))?;
        ledger.complete_effect(&mut remainder, EffectResult::Uncertain)?;
        assert_eq!(ledger.used(Kind::ClosedJournalBytes)?, 6096);
        assert_eq!(ledger.used(Kind::ClosedJournalSegments)?, 3);
        assert_eq!(ledger.used(Kind::ActiveJournalBytes)?, 0);
        assert_eq!(ledger.used(Kind::ActiveJournalOperations)?, 0);
        assert_eq!(ledger.pending(Kind::ActiveJournalBytes)?, 2000);
        assert_eq!(ledger.pending(Kind::ActiveJournalOperations)?, 7);
        assert_eq!(ledger.remaining(frame.0)?, remaining);
        assert_eq!(
            ledger.checkpoint_need()?.generation_bytes(),
            1500 + 2000 + 252 + 1048576
        );
        assert!(matches!(
            ledger.prepare(&[charge(Kind::BodyBytes, 1)], None, deadline()?, Tick(1)),
            Err(Error::Closed)
        ));
        assert!(matches!(
            ledger.begin_append(frame, FrameBudget::new(500, 1)?, Tick(2)),
            Err(Error::Closed)
        ));
        ledger.cancel(job)?;
        assert_eq!(ledger.used(Kind::BodyBytes)?, 100);
        Ok(())
    }
    #[test]
    fn barrier_abort_and_invalid_selection_do_not_alter_counters(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let p = plan()?;
        let mut states = [const { SlotState::EMPTY }; 4];
        let mut cells = [const { Cell::EMPTY }; 4];
        let mut ledger = WriterLedger::new(&p, 1232, recovered(1000, 4)?, &mut states, &mut cells)?;
        let before = ledger.checkpoint_need()?;
        ledger.begin_checkpoint()?.abort_before_selection();
        assert_eq!(ledger.phase(), Phase::Open);
        assert_eq!(ledger.checkpoint_need()?, before);
        let barrier = ledger.begin_checkpoint()?;
        assert_eq!(
            barrier.selected(before.generation_bytes() + 1),
            Err(Error::Invalid)
        );
        assert_eq!(ledger.phase(), Phase::Stopped);
        assert_eq!(ledger.selected_tables(), 1232);
        assert_eq!(ledger.used(Kind::ActiveJournalBytes)?, 1000);
        assert_eq!(ledger.used(Kind::ClosedJournalBytes)?, 0);
        assert!(matches!(
            ledger.prepare(&[charge(Kind::BodyBytes, 1)], None, deadline()?, Tick(1)),
            Err(Error::Stopped)
        ));
        Ok(())
    }
    #[test]
    fn closed_caps_fail_before_barrier_or_counter_changes() -> Result<(), Box<dyn std::error::Error>>
    {
        let p = plan()?;
        for (kind, amount) in [
            (Kind::ClosedJournalBytes, p.closed_journal_bytes() - 1095),
            (Kind::ClosedJournalSegments, p.closed_journal_segments()),
        ] {
            let mut states = [const { SlotState::EMPTY }; 2];
            let mut cells = [const { Cell::EMPTY }; 2];
            let mut used = recovered(1000, 4)?;
            used.add(kind, amount)?;
            let mut ledger = WriterLedger::new(&p, 1232, used, &mut states, &mut cells)?;
            assert!(
                matches!(ledger.begin_checkpoint(), Err(Error::Logical(logical::Error::Quota(k))) if k == kind)
            );
            assert_eq!(ledger.phase(), Phase::Open);
            assert_eq!(ledger.used(kind)?, amount);
            assert_eq!(ledger.used(Kind::ActiveJournalBytes)?, 1000);
        }
        Ok(())
    }
    #[test]
    fn private_quota_routes_and_candidate_caps_refuse_atomically(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let p = plan()?;
        let mut states = [const { SlotState::EMPTY }; 8];
        let mut cells = [const { Cell::EMPTY }; 8];
        let mut ledger = WriterLedger::new(
            &p,
            1232,
            recovered(p.active_journal_bytes() - 1000, 1)?,
            &mut states,
            &mut cells,
        )?;
        for kind in [
            Kind::ActiveJournalBytes,
            Kind::ActiveJournalOperations,
            Kind::ClosedJournalBytes,
            Kind::ClosedJournalSegments,
            Kind::CheckpointBytes,
            Kind::LiveMetadataBytes,
        ] {
            for amount in [0, 1] {
                assert!(matches!(
                    ledger.prepare(&[charge(kind, amount)], None, deadline()?, Tick(0)),
                    Err(Error::Invalid)
                ));
            }
        }
        let first = grant(&mut ledger, &[], Some(FrameBudget::new(1000, 1)?))?;
        assert!(matches!(
            ledger.prepare(&[], Some(FrameBudget::new(132, 1)?), deadline()?, Tick(0)),
            Err(Error::Logical(logical::Error::Quota(
                Kind::ActiveJournalBytes
            )))
        ));
        assert_eq!(ledger.pending(Kind::ActiveJournalBytes)?, 1000);
        ledger.cancel(first)?;
        assert_eq!(ledger.pending(Kind::ActiveJournalBytes)?, 0);
        assert!(matches!(
            ledger.prepare(
                &[charge(Kind::BodyBytes, 1); 8],
                Some(FrameBudget::new(132, 1)?),
                deadline()?,
                Tick(0)
            ),
            Err(Error::Invalid)
        ));
        Ok(())
    }
    #[test]
    fn operation_cap_and_expired_preparation_preserve_existing_frames(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let p = plan()?;
        let mut states = [const { SlotState::EMPTY }; 8];
        let mut cells = [const { Cell::EMPTY }; 8];
        let mut ledger = WriterLedger::new(
            &p,
            1232,
            recovered(1000000, p.active_journal_operations() - 1)?,
            &mut states,
            &mut cells,
        )?;
        let prepared =
            ledger.prepare(&[], Some(FrameBudget::new(1000, 1)?), deadline()?, Tick(0))?;
        assert_eq!(
            prepared.install(Tick(100)),
            Err(Error::Logical(logical::Error::Expired))
        );
        assert_eq!(ledger.pending(Kind::ActiveJournalOperations)?, 0);
        let first = grant(&mut ledger, &[], Some(FrameBudget::new(1000, 1)?))?;
        assert!(matches!(
            ledger.prepare(&[], Some(FrameBudget::new(132, 1)?), deadline()?, Tick(0)),
            Err(Error::Logical(logical::Error::Quota(
                Kind::ActiveJournalOperations
            )))
        ));
        assert_eq!(ledger.pending(Kind::ActiveJournalOperations)?, 1);
        ledger.cancel(first)?;
        Ok(())
    }
    #[test]
    fn foreign_and_canceled_frame_tickets_cannot_append() -> Result<(), Box<dyn std::error::Error>>
    {
        let p = plan()?;
        let mut sa = [const { SlotState::EMPTY }; 4];
        let mut ca = [const { Cell::EMPTY }; 4];
        let mut sb = [const { SlotState::EMPTY }; 4];
        let mut cb = [const { Cell::EMPTY }; 4];
        let mut a = WriterLedger::new(&p, 1232, recovered(0, 0)?, &mut sa, &mut ca)?;
        let mut b = WriterLedger::new(&p, 1232, recovered(0, 0)?, &mut sb, &mut cb)?;
        let request = FrameBudget::new(500, 1)?;
        let ga = grant(&mut a, &[], Some(request))?;
        let gb = grant(&mut b, &[], Some(request))?;
        let fa = ga.frame().ok_or("frame")?;
        assert!(matches!(
            b.begin_append(fa, request, Tick(1)),
            Err(Error::Logical(logical::Error::Stale))
        ));
        let mut ta = a.begin_append(fa, request, Tick(1))?;
        let mut tb = b.begin_append(gb.frame().ok_or("frame")?, request, Tick(1))?;
        assert_eq!(
            b.complete_append(&mut ta, AppendResult::Synced),
            Err(Error::Invalid)
        );
        a.complete_append(&mut ta, AppendResult::Synced)?;
        b.complete_append(&mut tb, AppendResult::NotWritten)?;
        a.cancel(ga)?;
        assert!(matches!(
            a.begin_append(fa, request, Tick(1)),
            Err(Error::Logical(logical::Error::Stale))
        ));
        assert_eq!(b.pending(Kind::ActiveJournalBytes)?, 500);
        b.cancel(gb)?;
        Ok(())
    }
    #[test]
    fn barrier_exposes_expiry_and_parts_without_reopening_admission(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let p = plan()?;
        let mut states = [const { SlotState::EMPTY }; 4];
        let mut cells = [const { Cell::EMPTY }; 4];
        let mut ledger = WriterLedger::new(&p, 1232, recovered(0, 0)?, &mut states, &mut cells)?;
        let job = grant(&mut ledger, &[charge(Kind::BodyBytes, 10)], None)?;
        let mut barrier = ledger.begin_checkpoint()?;
        let part = barrier.part(job, 0)?;
        let mut ticket = barrier.begin_effect(part, [10, 0, 0, 0], Tick(1))?;
        barrier.complete_effect(&mut ticket, EffectResult::Proven([3, 0, 0, 0]))?;
        assert_eq!(barrier.used(Kind::BodyBytes)?, 3);
        assert_eq!(barrier.pending(Kind::BodyBytes)?, 7);
        assert_eq!(barrier.phase(), Phase::Barrier);
        let mut expired = [None; 1];
        assert_eq!(barrier.expired(Tick(100), &mut expired)?, 1);
        barrier.cancel_lease(expired.first().copied().flatten().ok_or("expired")?)?;
        assert_eq!(barrier.pending(Kind::BodyBytes)?, 0);
        barrier.abort_before_selection();
        assert_eq!(ledger.phase(), Phase::Open);
        Ok(())
    }
    #[test]
    fn completed_ticket_cannot_stop_a_new_attempt_on_the_same_frame(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let p = plan()?;
        let mut states = [const { SlotState::EMPTY }; 4];
        let mut cells = [const { Cell::EMPTY }; 4];
        let mut ledger = WriterLedger::new(&p, 1232, recovered(0, 0)?, &mut states, &mut cells)?;
        let job = grant(&mut ledger, &[], Some(FrameBudget::new(1000, 4)?))?;
        let frame = job.frame().ok_or("frame")?;
        let actual = FrameBudget::new(132, 1)?;
        let mut old = ledger.begin_append(frame, actual, Tick(1))?;
        ledger.complete_append(&mut old, AppendResult::NotWritten)?;
        let mut current = ledger.begin_append(frame, actual, Tick(2))?;
        assert_eq!(
            ledger.complete_append(&mut old, AppendResult::Uncertain),
            Err(Error::Logical(logical::Error::InactiveTicket))
        );
        assert_eq!(ledger.phase(), Phase::Open);
        ledger.complete_append(&mut current, AppendResult::Synced)?;
        assert_eq!(ledger.used(Kind::ActiveJournalBytes)?, 132);
        assert_eq!(ledger.pending(Kind::ActiveJournalBytes)?, 0);
        assert_eq!(ledger.pending(Kind::ActiveJournalOperations)?, 0);
        assert!(matches!(
            ledger.begin_append(frame, actual, Tick(3)),
            Err(Error::Logical(logical::Error::Invalid))
        ));
        ledger.cancel(job)?;
        Ok(())
    }
    #[test]
    fn selected_bounds_exclude_uncommitted_frames_and_closed_exact_fit_passes(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let p = plan()?;
        let bound = 1232 + 1000 + 4 * 36 + 1048576;
        for selected in [1231, bound + 1, p.disk().live_metadata_bytes + 1, bound] {
            let mut states = [const { SlotState::EMPTY }; 4];
            let mut cells = [const { Cell::EMPTY }; 4];
            let mut used = recovered(1000, 4)?;
            used.add(Kind::ClosedJournalBytes, p.closed_journal_bytes() - 1096)?;
            let mut ledger = WriterLedger::new(&p, 1232, used, &mut states, &mut cells)?;
            let job = grant(&mut ledger, &[], Some(FrameBudget::new(2000, 8)?))?;
            let barrier = ledger.begin_checkpoint()?;
            assert!(barrier.checkpoint_need().generation_bytes() > bound + 1);
            let result = barrier.selected(selected);
            if selected == bound {
                result?;
                assert_eq!(ledger.phase(), Phase::AwaitingSpace);
                assert_eq!(
                    ledger.used(Kind::ClosedJournalBytes)?,
                    p.closed_journal_bytes()
                );
            } else {
                assert_eq!(result, Err(Error::Invalid));
                assert_eq!(ledger.phase(), Phase::Stopped);
                assert_eq!(ledger.selected_tables(), 1232);
                assert_eq!(ledger.used(Kind::ActiveJournalBytes)?, 1000);
            }
            assert_eq!(ledger.pending(Kind::ActiveJournalBytes)?, 2000);
            ledger.cancel(job)?;
        }
        Ok(())
    }
    #[test]
    fn dropped_or_uncertain_checkpoint_stops_new_effects_but_accepts_completions(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let p = plan()?;
        for uncertain in [false, true] {
            let mut states = [const { SlotState::EMPTY }; 4];
            let mut cells = [const { Cell::EMPTY }; 4];
            let mut ledger =
                WriterLedger::new(&p, 1232, recovered(0, 0)?, &mut states, &mut cells)?;
            assert!(matches!(
                ledger.prepare(&[charge(Kind::BodyBytes, 10)], None, deadline()?, Tick(100)),
                Err(Error::Logical(logical::Error::Expired))
            ));
            let job = grant(&mut ledger, &[charge(Kind::BodyBytes, 10); 2], None)?;
            let part = ledger.part(job, 0)?;
            let mut ticket = ledger.begin_effect(part, [10, 0, 0, 0], Tick(1))?;
            let barrier = ledger.begin_checkpoint()?;
            if uncertain {
                barrier.uncertain();
            } else {
                drop(barrier);
            }
            assert_eq!(ledger.phase(), Phase::Stopped);
            assert!(matches!(
                ledger.begin_effect(ledger.part(job, 1)?, [10, 0, 0, 0], Tick(2)),
                Err(Error::Stopped)
            ));
            ledger.complete_effect(&mut ticket, EffectResult::Proven([3, 0, 0, 0]))?;
            ledger.cancel(job)?;
            assert_eq!(ledger.used(Kind::BodyBytes)?, 3);
        }
        Ok(())
    }
    #[test]
    fn frame_and_recovered_state_bounds_are_checked() -> Result<(), Box<dyn std::error::Error>> {
        assert!(FrameBudget::new(132, 1).is_ok());
        assert!(FrameBudget::new(1048576, 4096).is_ok());
        for (bytes, ops) in [
            (131, 1),
            (1048577, 1),
            (132, 0),
            (132, 4097),
            (132, 4096),
            (u64::MAX, u64::MAX),
        ] {
            assert_eq!(FrameBudget::new(bytes, ops), Err(Error::Invalid));
        }
        let p = plan()?;
        let mut states = [const { SlotState::EMPTY }; 2];
        let mut cells = [const { Cell::EMPTY }; 2];
        assert!(matches!(
            WriterLedger::new(&p, 1231, recovered(0, 0)?, &mut states, &mut cells),
            Err(Error::Invalid)
        ));
        assert!(matches!(
            WriterLedger::new(&p, 1232, recovered(1, 0)?, &mut states, &mut cells),
            Err(Error::Invalid)
        ));
        Ok(())
    }
}

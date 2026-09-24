//! Fixed logical leases. Physical admission and writer barriers are separate
//! coordinator gates; owning one of these tokens never authorizes I/O alone.
use super::{
    filesystems::FilesystemId,
    quota::{Charge, Kind, Quotas, Usage},
    space::RoundedGrowth,
    Error as PlanError, Plan,
};
use crate::{
    ownership::{Error as SlotError, SlotId, SlotPool, SlotState},
    ports::{Deadline, Tick},
};

pub const MAX_LEASE_CELLS: usize = 64;
pub const CHARGES_PER_CELL: usize = 4;
pub const MAX_GROUP_CELLS: usize = 8;
pub type Charges = [Charge; CHARGES_PER_CELL];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Invalid,
    Expired,
    Busy,
    Stale,
    InactiveTicket,
    Full,
    Poisoned,
    Quota(Kind),
    Arithmetic(PlanError),
    Slot(SlotError),
}
impl From<PlanError> for Error {
    fn from(e: PlanError) -> Self {
        Self::Arithmetic(e)
    }
}
impl From<SlotError> for Error {
    fn from(e: SlotError) -> Self {
        Self::Slot(e)
    }
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Quota(kind) => write!(f, "logical quota exhausted: {kind:?}"),
            Self::Arithmetic(e) => e.fmt(f),
            Self::Slot(e) => e.fmt(f),
            _ => f.write_str(match self {
                Self::Invalid => "invalid logical reservation",
                Self::Expired => "reservation deadline expired",
                Self::Busy => "reservation has an in-flight effect",
                Self::Stale => "stale logical reservation",
                Self::InactiveTicket => "effect ticket already consumed",
                Self::Full => "logical reservation cells exhausted",
                Self::Poisoned => "logical reservation bookkeeping stopped",
                _ => "logical reservation error",
            }),
        }
    }
}
impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Arithmetic(e) => Some(e),
            Self::Slot(e) => Some(e),
            _ => None,
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[must_use = "retain the lease until cancellation or reconciled recovery"]
pub struct LeaseId(SlotId);
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PartId(SlotId);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Physical {
    pub filesystem: FilesystemId,
    pub remaining: RoundedGrowth,
}
#[derive(Clone, Copy, Debug)]
pub(super) struct Issued {
    pub lease: LeaseId,
    pub frame: Option<PartId>,
}
#[derive(Clone, Copy, Debug)]
struct Record {
    id: SlotId,
    root: u32,
    ordinal: u16,
    busy: bool,
    deadline: Deadline,
    kinds: [Option<Kind>; CHARGES_PER_CELL],
    amounts: [u64; CHARGES_PER_CELL],
    physical: Option<Physical>,
}
impl Record {
    fn charges(self) -> Charges {
        let mut charges = [Charge::ZERO; CHARGES_PER_CELL];
        for ((charge, kind), amount) in charges.iter_mut().zip(self.kinds).zip(self.amounts) {
            if let Some(kind) = kind {
                *charge = Charge { kind, amount };
            }
        }
        charges
    }
}
#[derive(Debug, Default)]
pub struct Cell {
    record: Option<Record>,
}
impl Cell {
    pub const EMPTY: Self = Self { record: None };
}

/// Linear completion capability. Dropping it leaves the reservation in-flight;
/// recovery must account for effects before that reservation can be released.
#[derive(Debug)]
#[must_use = "complete the ticket; dropping it leaves the lease pinned"]
pub struct EffectTicket {
    part: Option<PartId>,
    planned: [u64; CHARGES_PER_CELL],
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EffectResult {
    /// Exact proven charge, no greater than the plan. The I/O/store adapter owns
    /// that proof; syscall failure alone never establishes zero growth/effects.
    Proven([u64; CHARGES_PER_CELL]),
    /// Full conservative charge remains used until object-ledger reconciliation.
    Uncertain,
}

pub struct Leases<'a> {
    slots: SlotPool<'a>,
    cells: &'a mut [Cell],
    quotas: Quotas,
    poisoned: bool,
}
impl<'a> Leases<'a> {
    /// `used` comes from trusted store reconciliation; reductions below it fail.
    /// Backing state/cells must be empty and have equal lengths, at most 64.
    pub fn new(
        plan: &Plan,
        used: Usage,
        states: &'a mut [SlotState],
        cells: &'a mut [Cell],
    ) -> Result<Self, Error> {
        if states.len() != cells.len()
            || cells.len() > MAX_LEASE_CELLS
            || cells.iter().any(|c| c.record.is_some())
        {
            return Err(Error::Invalid);
        }
        let quotas = Quotas::new(plan, used).map_err(Error::Quota)?;
        Ok(Self {
            slots: SlotPool::new(states)?,
            cells,
            quotas,
            poisoned: false,
        })
    }
    fn healthy(&self) -> Result<(), Error> {
        if self.poisoned {
            Err(Error::Poisoned)
        } else {
            Ok(())
        }
    }
    pub fn used(&self, kind: Kind) -> Result<u64, Error> {
        Ok(self.quotas.used().get(kind)?)
    }
    pub fn pending(&self, kind: Kind) -> Result<u64, Error> {
        Ok(self.quotas.pending().get(kind)?)
    }
    pub fn available_cells(&self) -> usize {
        self.slots.available()
    }

    pub(super) fn quotas(&self) -> &Quotas {
        &self.quotas
    }
    pub(super) fn project(&self, requests: &[Charges]) -> Result<Quotas, Error> {
        self.healthy()?;
        if requests.is_empty() || requests.len() > MAX_GROUP_CELLS {
            return Err(Error::Invalid);
        }
        if requests.len() > self.slots.available() {
            return Err(Error::Full);
        }
        let mut extra = Usage::default();
        for charges in requests {
            for charge in charges {
                extra.add(charge.kind, charge.amount)?;
            }
        }
        self.quotas.with_reservation(extra).map_err(Error::Quota)
    }
    pub(super) fn project_rollover(&self) -> Result<Quotas, Error> {
        self.healthy()?;
        self.quotas.with_rollover().map_err(Error::Quota)
    }
    pub(super) fn rollover(&mut self) -> Result<(), Error> {
        self.quotas = self.project_rollover()?;
        Ok(())
    }

    /// Atomic across up to eight records. Duplicate kinds add across the group.
    /// This reserves logical budgets only; the full coordinator couples it to
    /// fresh physical probes and its derived checkpoint reserve before effects.
    pub fn reserve(
        &mut self,
        requests: &[Charges],
        deadline: Deadline,
        now: Tick,
    ) -> Result<LeaseId, Error> {
        self.reserve_using(requests, deadline, now, |pool, _| pool.acquire())
    }
    fn reserve_using(
        &mut self,
        requests: &[Charges],
        deadline: Deadline,
        now: Tick,
        acquire: impl FnMut(&mut SlotPool<'a>, usize) -> Result<SlotId, SlotError>,
    ) -> Result<LeaseId, Error> {
        self.reserve_inner(requests, None, false, deadline, now, acquire)
            .map(|issued| issued.lease)
    }
    pub(super) fn reserve_group(
        &mut self,
        requests: &[Charges],
        physical: Option<&[Physical]>,
        has_frame: bool,
        deadline: Deadline,
        now: Tick,
    ) -> Result<Issued, Error> {
        self.reserve_inner(requests, physical, has_frame, deadline, now, |pool, _| {
            pool.acquire()
        })
    }
    fn reserve_inner(
        &mut self,
        requests: &[Charges],
        physical: Option<&[Physical]>,
        has_frame: bool,
        deadline: Deadline,
        now: Tick,
        mut acquire: impl FnMut(&mut SlotPool<'a>, usize) -> Result<SlotId, SlotError>,
    ) -> Result<Issued, Error> {
        self.healthy()?;
        if requests.is_empty() || requests.len() > MAX_GROUP_CELLS {
            return Err(Error::Invalid);
        }
        if deadline.expired(now) {
            return Err(Error::Expired);
        }
        if requests.len() > self.slots.available() {
            return Err(Error::Full);
        }
        if physical.is_some_and(|p| p.len() != requests.len()) {
            return Err(Error::Invalid);
        }
        let next = self.project(requests)?;
        let mut issued = [None; MAX_GROUP_CELLS];
        for ordinal in 0..requests.len() {
            let id = match acquire(&mut self.slots, ordinal) {
                Ok(id) => id,
                Err(error) => {
                    self.rollback(&issued)?;
                    return Err(Error::Slot(error));
                }
            };
            let index = match self.slots.resolve(id) {
                Ok(i) => i,
                Err(_) => {
                    self.poisoned = true;
                    return Err(Error::Poisoned);
                }
            };
            let Some(slot) = issued.get_mut(ordinal) else {
                self.poisoned = true;
                return Err(Error::Poisoned);
            };
            *slot = Some((id, index));
        }
        let Some((root_id, root)) = issued.first().copied().flatten() else {
            self.poisoned = true;
            return Err(Error::Poisoned);
        };
        let Ok(root) = u32::try_from(root) else {
            self.poisoned = true;
            return Err(Error::Poisoned);
        };
        let mut records = [None; MAX_GROUP_CELLS];
        let frame = if has_frame {
            let Some((id, _)) = issued.iter().flatten().last() else {
                self.poisoned = true;
                return Err(Error::Poisoned);
            };
            Some(PartId(*id))
        } else {
            None
        };
        for (ordinal, (issued, charges)) in issued.iter().flatten().zip(requests).enumerate() {
            let (id, index) = *issued;
            let binding = physical.and_then(|p| p.get(ordinal)).copied();
            if physical.is_some() && binding.is_none() {
                self.poisoned = true;
                return Err(Error::Poisoned);
            }
            if !self.cells.get(index).is_some_and(|c| c.record.is_none())
                || records.iter().flatten().any(|(other, _)| *other == index)
            {
                self.poisoned = true;
                return Err(Error::Poisoned);
            }
            let Ok(ordinal_value) = u16::try_from(ordinal) else {
                self.poisoned = true;
                return Err(Error::Poisoned);
            };
            let Some(out) = records.get_mut(ordinal) else {
                self.poisoned = true;
                return Err(Error::Poisoned);
            };
            *out = Some((
                index,
                Record {
                    id,
                    root,
                    ordinal: ordinal_value,
                    busy: false,
                    deadline,
                    kinds: charges.map(|c| (c.amount != 0).then_some(c.kind)),
                    amounts: charges.map(|c| c.amount),
                    physical: binding,
                },
            ));
        }
        // Every fallible step precedes publication, including returned tokens.
        for (index, cell) in self.cells.iter_mut().enumerate() {
            if let Some((_, record)) = records.iter().flatten().find(|(i, _)| *i == index) {
                cell.record = Some(*record);
            }
        }
        self.quotas = next;
        Ok(Issued {
            lease: LeaseId(root_id),
            frame,
        })
    }
    fn rollback(&mut self, issued: &[Option<(SlotId, usize)>]) -> Result<(), Error> {
        for (id, _) in issued.iter().flatten() {
            if self.slots.release(*id).is_err() {
                self.poisoned = true;
                return Err(Error::Poisoned);
            }
        }
        Ok(())
    }
    fn record(&self, part: PartId) -> Result<(usize, Record), Error> {
        self.healthy()?;
        let index = self.slots.resolve(part.0).map_err(|_| Error::Stale)?;
        let record = self
            .cells
            .get(index)
            .and_then(|c| c.record)
            .ok_or(Error::Stale)?;
        if record.id != part.0 {
            return Err(Error::Stale);
        }
        Ok((index, record))
    }
    fn root(&self, lease: LeaseId) -> Result<u32, Error> {
        let (index, record) = self.record(PartId(lease.0))?;
        let root = u32::try_from(index).map_err(|_| Error::Stale)?;
        if record.root != root || record.ordinal != 0 {
            return Err(Error::Stale);
        }
        Ok(root)
    }
    pub fn part(&self, lease: LeaseId, ordinal: u16) -> Result<PartId, Error> {
        let root = self.root(lease)?;
        self.cells
            .iter()
            .filter_map(|cell| cell.record)
            .find(|r| r.root == root && r.ordinal == ordinal)
            .map(|r| PartId(r.id))
            .ok_or(Error::Stale)
    }
    pub(super) fn is_empty(&self) -> bool {
        self.cells.iter().all(|c| c.record.is_none())
    }
    pub(super) fn physical(&self, part: PartId) -> Result<Physical, Error> {
        self.record(part)?.1.physical.ok_or(Error::Invalid)
    }
    pub(super) fn physical_group(
        &self,
        lease: LeaseId,
    ) -> Result<[Option<Physical>; MAX_GROUP_CELLS], Error> {
        let root = self.root(lease)?;
        let mut out = [None; MAX_GROUP_CELLS];
        for record in self
            .cells
            .iter()
            .filter_map(|c| c.record)
            .filter(|r| r.root == root)
        {
            if record.busy {
                return Err(Error::Busy);
            }
            let binding = record.physical.ok_or(Error::Invalid)?;
            *out.get_mut(usize::from(record.ordinal))
                .ok_or(Error::Invalid)? = Some(binding);
        }
        Ok(out)
    }
    pub fn remaining(&self, part: PartId) -> Result<Charges, Error> {
        Ok(self.record(part)?.1.charges())
    }

    /// Increase a live part's remaining logical budget. The full coordinator
    /// must pair this with fresh physical admission before enabling more I/O.
    pub fn extend(
        &mut self,
        part: PartId,
        amounts: [u64; CHARGES_PER_CELL],
        now: Tick,
    ) -> Result<(), Error> {
        self.extend_inner(part, amounts, None, now)
    }
    pub(super) fn extend_bound(
        &mut self,
        part: PartId,
        amounts: [u64; 4],
        physical: RoundedGrowth,
        now: Tick,
    ) -> Result<(), Error> {
        self.extend_inner(part, amounts, Some(physical), now)
    }
    fn extend_inner(
        &mut self,
        part: PartId,
        amounts: [u64; 4],
        physical: Option<RoundedGrowth>,
        now: Tick,
    ) -> Result<(), Error> {
        let (index, mut record) = self.record(part)?;
        if record.busy {
            return Err(Error::Busy);
        }
        if record.deadline.expired(now) {
            return Err(Error::Expired);
        }
        let mut extra = Usage::default();
        for ((amount, kind), remaining) in amounts.iter().zip(record.kinds).zip(&mut record.amounts)
        {
            if let Some(kind) = kind {
                extra.add(kind, *amount)?;
            } else if *amount != 0 {
                return Err(Error::Invalid);
            }
            *remaining = super::add(*remaining, *amount, "lease extension")?;
        }
        let next = self.quotas.with_reservation(extra).map_err(Error::Quota)?;
        match (&mut record.physical, physical) {
            (Some(binding), Some(extra)) => {
                binding.remaining = binding.remaining.checked_add(extra)?
            }
            (None, None) => (),
            _ => return Err(Error::Invalid),
        }
        let cell = self.cells.get_mut(index).ok_or(Error::Stale)?;
        cell.record = Some(record);
        self.quotas = next;
        Ok(())
    }

    /// Must be called before effects. The composed coordinator applies physical
    /// and writer gates first. A busy part cannot be reused or canceled.
    pub fn begin_effect(
        &mut self,
        part: PartId,
        amounts: [u64; CHARGES_PER_CELL],
        now: Tick,
    ) -> Result<EffectTicket, Error> {
        let (index, record) = self.record(part)?;
        if record.busy {
            return Err(Error::Busy);
        }
        if record.deadline.expired(now) {
            return Err(Error::Expired);
        }
        if amounts.iter().zip(record.amounts).any(|(a, c)| *a > c) {
            return Err(Error::Invalid);
        }
        let record = self
            .cells
            .get_mut(index)
            .and_then(|c| c.record.as_mut())
            .ok_or(Error::Stale)?;
        record.busy = true;
        Ok(EffectTicket {
            part: Some(part),
            planned: amounts,
        })
    }
    /// Completion can occur after the deadline. Writing and reference publication
    /// use separate tickets/charges; one does not imply that the other occurred.
    /// Invalid completion keeps the live ticket and its reservation pinned.
    pub fn complete_effect(
        &mut self,
        ticket: &mut EffectTicket,
        result: EffectResult,
    ) -> Result<(), Error> {
        self.complete_effect_inner(ticket, result, None, false)
    }
    pub(super) fn check_effect(&self, ticket: &EffectTicket) -> Result<(), Error> {
        let part = ticket.part.ok_or(Error::InactiveTicket)?;
        if !self.record(part)?.1.busy {
            return Err(Error::Invalid);
        }
        Ok(())
    }
    pub(super) fn complete_frame(
        &mut self,
        ticket: &mut EffectTicket,
        result: EffectResult,
    ) -> Result<(), Error> {
        self.complete_effect_inner(ticket, result, None, true)
    }
    pub(super) fn complete_bound(
        &mut self,
        ticket: &mut EffectTicket,
        result: EffectResult,
        physical: RoundedGrowth,
        release_remainder: bool,
    ) -> Result<(), Error> {
        self.complete_effect_inner(ticket, result, Some(physical), release_remainder)
    }
    fn complete_effect_inner(
        &mut self,
        ticket: &mut EffectTicket,
        result: EffectResult,
        physical: Option<RoundedGrowth>,
        release_remainder: bool,
    ) -> Result<(), Error> {
        let part = ticket.part.ok_or(Error::InactiveTicket)?;
        let (index, mut record) = self.record(part)?;
        if !record.busy {
            return Err(Error::Invalid);
        }
        let amounts = match result {
            EffectResult::Proven(v) => v,
            EffectResult::Uncertain => ticket.planned,
        };
        let mut used = Usage::default();
        for (((amount, planned), kind), remaining) in amounts
            .iter()
            .zip(ticket.planned)
            .zip(record.kinds)
            .zip(&mut record.amounts)
        {
            if *amount > planned {
                return Err(Error::Invalid);
            }
            *remaining = remaining.checked_sub(*amount).ok_or(Error::Invalid)?;
            if let Some(kind) = kind {
                used.add(kind, *amount)?;
            } else if *amount != 0 {
                return Err(Error::Invalid);
            }
        }
        let mut next = self.quotas.clone();
        next.complete(used)?;
        if release_remainder {
            let mut unused = Usage::default();
            for charge in record.charges() {
                unused.add(charge.kind, charge.amount)?;
            }
            next.release_unused(unused)?;
            record.amounts.fill(0);
        }
        match (&mut record.physical, physical) {
            (Some(binding), Some(actual)) => {
                binding.remaining = binding.remaining.checked_sub(actual)?;
                if release_remainder {
                    binding.remaining = binding.remaining.zeroed();
                }
            }
            (None, None) => (),
            _ => return Err(Error::Invalid),
        }
        record.busy = false;
        let cell = self.cells.get_mut(index).ok_or(Error::Stale)?;
        cell.record = Some(record);
        self.quotas = next;
        ticket.part = None;
        Ok(())
    }
    /// Enumerate expired roots, including busy groups. Only the returned prefix
    /// is written; a short buffer fails unchanged. Cancellation still checks busy.
    pub fn expired(&self, now: Tick, output: &mut [Option<LeaseId>]) -> Result<usize, Error> {
        self.healthy()?;
        let roots = self
            .cells
            .iter()
            .filter_map(|c| c.record)
            .filter(|r| r.ordinal == 0 && r.deadline.expired(now));
        let count = roots.clone().count();
        if output.len() < count {
            return Err(Error::Full);
        }
        for (out, record) in output.iter_mut().zip(roots) {
            *out = Some(LeaseId(record.id));
        }
        Ok(count)
    }

    /// Release only unspent charges, never used (including orphan) charges.
    pub fn cancel(&mut self, lease: LeaseId) -> Result<(), Error> {
        let root = self.root(lease)?;
        let mut unused = Usage::default();
        for record in self
            .cells
            .iter()
            .filter_map(|c| c.record)
            .filter(|r| r.root == root)
        {
            if record.busy {
                return Err(Error::Busy);
            }
            for charge in record.charges() {
                unused.add(charge.kind, charge.amount)?;
            }
        }
        let mut next = self.quotas.clone();
        next.release_unused(unused)?;
        for cell in self.cells.iter_mut() {
            if let Some(record) = cell.record.filter(|r| r.root == root) {
                if self.slots.release(record.id).is_err() {
                    self.poisoned = true;
                    return Err(Error::Poisoned);
                }
                cell.record = None;
            }
        }
        self.quotas = next;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        admission::{DiskLimits, ViewMode, WorkLimits},
        limits::Limits,
    };
    fn plan(queue: u64) -> Result<Plan, Box<dyn std::error::Error>> {
        let resources = Limits {
            queue_disk_bytes: usize::try_from(queue)?,
            ..Limits::default()
        }
        .plan()?;
        Ok(DiskLimits::default().plan(
            &resources,
            WorkLimits::default(),
            ViewMode::OnlineBackground,
        )?)
    }
    fn charges(kind: Kind, amount: u64) -> Charges {
        [
            Charge { kind, amount },
            Charge::ZERO,
            Charge::ZERO,
            Charge::ZERO,
        ]
    }
    fn reserve(book: &mut Leases<'_>, requests: &[Charges]) -> Result<LeaseId, Error> {
        for _ in 0..1000 {
            match book.reserve(
                requests,
                Deadline::after(Tick(0), 10).map_err(|_| Error::Invalid)?,
                Tick(0),
            ) {
                Err(Error::Slot(SlotError::Contended)) => std::thread::yield_now(),
                result => return result,
            }
        }
        Err(Error::Slot(SlotError::Contended))
    }
    #[test]
    fn group_checks_duplicate_kinds_and_late_quota_failure_atomically(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let p = plan(100)?;
        let mut states = [const { SlotState::EMPTY }; 8];
        let mut cells = [const { Cell::EMPTY }; 8];
        let mut book = Leases::new(&p, Usage::default(), &mut states, &mut cells)?;
        let mut first = charges(Kind::QueueBytes, 40);
        first.get_mut(1).ok_or("cell")?.clone_from(&Charge {
            kind: Kind::QueueBytes,
            amount: 10,
        });
        let lease = reserve(&mut book, &[first, charges(Kind::QueueBytes, 50)])?;
        assert_eq!(book.pending(Kind::QueueBytes)?, 100);
        assert_eq!(book.available_cells(), 6);
        let member = book.part(lease, 1)?;
        assert_eq!(book.cancel(LeaseId(member.0)), Err(Error::Stale));
        let before = book.available_cells();
        assert_eq!(
            reserve(
                &mut book,
                &[charges(Kind::BodyBytes, 3), charges(Kind::QueueBytes, 1)]
            ),
            Err(Error::Quota(Kind::QueueBytes))
        );
        assert_eq!(book.pending(Kind::BodyBytes)?, 0);
        assert_eq!(book.pending(Kind::QueueBytes)?, 100);
        assert_eq!(book.available_cells(), before);
        book.cancel(lease)?;
        assert_eq!(book.pending(Kind::QueueBytes)?, 0);
        assert_eq!(book.available_cells(), 8);
        assert_eq!(book.cancel(lease), Err(Error::Stale));
        Ok(())
    }
    #[test]
    fn written_or_uncertain_charges_survive_cancel_and_publication_is_separate(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let p = plan(100)?;
        let mut states = [const { SlotState::EMPTY }; 4];
        let mut cells = [const { Cell::EMPTY }; 4];
        let mut book = Leases::new(&p, Usage::default(), &mut states, &mut cells)?;
        let body = [
            Charge {
                kind: Kind::BodyBytes,
                amount: 100,
            },
            Charge {
                kind: Kind::BodyFiles,
                amount: 1,
            },
            Charge {
                kind: Kind::UploadBytes,
                amount: 100,
            },
            Charge::ZERO,
        ];
        let lease = reserve(&mut book, &[body])?;
        let part = book.part(lease, 0)?;
        let mut ticket = book.begin_effect(part, [100, 1, 0, 0], Tick(1))?;
        assert_eq!(book.cancel(lease), Err(Error::Busy));
        assert_eq!(
            book.begin_effect(part, [1, 0, 0, 0], Tick(2)).err(),
            Some(Error::Busy)
        );
        book.complete_effect(&mut ticket, EffectResult::Proven([40, 1, 0, 0]))?;
        assert_eq!(book.used(Kind::BodyBytes)?, 40);
        assert_eq!(book.pending(Kind::BodyBytes)?, 60);
        assert_eq!(book.used(Kind::UploadBytes)?, 0);
        book.cancel(lease)?;
        assert_eq!(book.used(Kind::BodyBytes)?, 40);
        assert_eq!(book.used(Kind::BodyFiles)?, 1);
        assert_eq!(book.pending(Kind::UploadBytes)?, 0);
        assert_eq!(
            book.complete_effect(&mut ticket, EffectResult::Uncertain),
            Err(Error::InactiveTicket)
        );
        let lease = reserve(&mut book, &[charges(Kind::BodyBytes, 20)])?;
        let part = book.part(lease, 0)?;
        let mut failed = book.begin_effect(part, [20, 0, 0, 0], Tick(1))?;
        book.complete_effect(&mut failed, EffectResult::Uncertain)?;
        book.cancel(lease)?;
        assert_eq!(book.used(Kind::BodyBytes)?, 60);
        assert_eq!(book.pending(Kind::BodyBytes)?, 0);
        Ok(())
    }
    #[test]
    fn invalid_completion_keeps_ticket_pinned_and_completion_outlives_deadline(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let p = plan(100)?;
        let mut states = [const { SlotState::EMPTY }; 4];
        let mut cells = [const { Cell::EMPTY }; 4];
        let mut book = Leases::new(&p, Usage::default(), &mut states, &mut cells)?;
        let lease = reserve(&mut book, &[charges(Kind::QueueBytes, 100)])?;
        let part = book.part(lease, 0)?;
        let mut ticket = book.begin_effect(part, [40, 0, 0, 0], Tick(9))?;
        assert_eq!(
            book.complete_effect(&mut ticket, EffectResult::Proven([41, 0, 0, 0])),
            Err(Error::Invalid)
        );
        assert_eq!(book.used(Kind::QueueBytes)?, 0);
        assert_eq!(book.pending(Kind::QueueBytes)?, 100);
        assert_eq!(book.cancel(lease), Err(Error::Busy));
        // Completion has no now parameter: expiry cannot undo observed effects.
        book.complete_effect(&mut ticket, EffectResult::Proven([40, 0, 0, 0]))?;
        assert_eq!(
            book.begin_effect(part, [1, 0, 0, 0], Tick(10)).err(),
            Some(Error::Expired)
        );
        assert_eq!(
            book.extend(part, [1, 0, 0, 0], Tick(10)),
            Err(Error::Expired)
        );
        book.cancel(lease)?;
        assert_eq!(book.used(Kind::QueueBytes)?, 40);
        Ok(())
    }
    #[test]
    fn extensions_intersect_used_and_pending_without_growing_cells(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let p = plan(100)?;
        let mut states = [const { SlotState::EMPTY }; 4];
        let mut cells = [const { Cell::EMPTY }; 4];
        let mut used = Usage::default();
        used.add(Kind::QueueBytes, 10)?;
        let mut book = Leases::new(&p, used, &mut states, &mut cells)?;
        let lease = reserve(&mut book, &[charges(Kind::QueueBytes, 20)])?;
        let part = book.part(lease, 0)?;
        book.extend(part, [70, 0, 0, 0], Tick(1))?;
        assert_eq!(book.pending(Kind::QueueBytes)?, 90);
        assert_eq!(book.available_cells(), 3);
        assert_eq!(
            book.extend(part, [1, 0, 0, 0], Tick(2)),
            Err(Error::Quota(Kind::QueueBytes))
        );
        assert_eq!(book.remaining(part)?.first().ok_or("charge")?.amount, 90);
        let mut ticket = book.begin_effect(part, [1, 0, 0, 0], Tick(3))?;
        assert_eq!(book.extend(part, [0; 4], Tick(4)), Err(Error::Busy));
        book.complete_effect(&mut ticket, EffectResult::Proven([0; 4]))?;
        book.cancel(lease)?;
        assert_eq!(book.used(Kind::QueueBytes)?, 10);
        Ok(())
    }
    #[test]
    fn stale_and_foreign_tokens_cannot_modify_a_new_group() -> Result<(), Box<dyn std::error::Error>>
    {
        let p = plan(100)?;
        let mut sa = [const { SlotState::EMPTY }; 2];
        let mut ca = [const { Cell::EMPTY }; 2];
        let mut sb = [const { SlotState::EMPTY }; 2];
        let mut cb = [const { Cell::EMPTY }; 2];
        let mut a = Leases::new(&p, Usage::default(), &mut sa, &mut ca)?;
        let mut b = Leases::new(&p, Usage::default(), &mut sb, &mut cb)?;
        let old = reserve(&mut a, &[charges(Kind::QueueBytes, 1)])?;
        let part = a.part(old, 0)?;
        let other = reserve(&mut b, &[charges(Kind::QueueBytes, 2)])?;
        assert_eq!(b.cancel(old), Err(Error::Stale));
        assert_eq!(a.cancel(other), Err(Error::Stale));
        let mut ticket = a.begin_effect(part, [1, 0, 0, 0], Tick(1))?;
        assert_eq!(
            b.complete_effect(&mut ticket, EffectResult::Uncertain),
            Err(Error::Stale)
        );
        assert_eq!(b.pending(Kind::QueueBytes)?, 2);
        a.complete_effect(&mut ticket, EffectResult::Proven([0; 4]))?;
        a.cancel(old)?;
        let new = reserve(&mut a, &[charges(Kind::QueueBytes, 3)])?;
        assert_eq!(a.cancel(old), Err(Error::Stale));
        assert_eq!(
            a.begin_effect(part, [0; 4], Tick(1)).err(),
            Some(Error::Stale)
        );
        assert_eq!(a.pending(Kind::QueueBytes)?, 3);
        a.cancel(new)?;
        b.cancel(other)?;
        Ok(())
    }
    #[test]
    fn late_ticket_issuance_failure_rolls_back_slots_and_counters(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let p = plan(100)?;
        let mut states = [const { SlotState::EMPTY }; 4];
        let mut cells = [const { Cell::EMPTY }; 4];
        let mut book = Leases::new(&p, Usage::default(), &mut states, &mut cells)?;
        let result = book.reserve_using(
            &[charges(Kind::QueueBytes, 10); 2],
            Deadline::after(Tick(0), 10)?,
            Tick(0),
            |pool, ordinal| {
                if ordinal == 1 {
                    return Err(SlotError::Contended);
                }
                for _ in 0..1000 {
                    match pool.acquire() {
                        Err(SlotError::Contended) => std::thread::yield_now(),
                        result => return result,
                    }
                }
                Err(SlotError::Contended)
            },
        );
        assert_eq!(result, Err(Error::Slot(SlotError::Contended)));
        assert_eq!(book.available_cells(), 4);
        assert_eq!(book.used(Kind::QueueBytes)?, 0);
        assert_eq!(book.pending(Kind::QueueBytes)?, 0);
        let lease = reserve(&mut book, &[charges(Kind::QueueBytes, 100)])?;
        book.cancel(lease)?;
        Ok(())
    }
    #[test]
    fn fixed_capacity_group_bound_and_combined_record_layout(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let p = plan(100)?;
        let mut states = [const { SlotState::EMPTY }; 64];
        let mut cells = [const { Cell::EMPTY }; 64];
        let mut book = Leases::new(&p, Usage::default(), &mut states, &mut cells)?;
        let mut leases = [None; 8];
        for slot in &mut leases {
            *slot = Some(reserve(&mut book, &[[Charge::ZERO; 4]; 8])?);
        }
        assert_eq!(book.available_cells(), 0);
        assert_eq!(reserve(&mut book, &[[Charge::ZERO; 4]]), Err(Error::Full));
        assert_eq!(
            reserve(&mut book, &[[Charge::ZERO; 4]; 9]),
            Err(Error::Invalid)
        );
        for lease in leases.into_iter().flatten() {
            book.cancel(lease)?;
        }
        assert_eq!(book.available_cells(), 64);
        assert!(std::mem::size_of::<Cell>() + std::mem::size_of::<SlotState>() <= 128);
        Ok(())
    }
    #[test]
    fn expiry_enumeration_preserves_busy_members_and_short_output(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let p = plan(100)?;
        let mut states = [const { SlotState::EMPTY }; 4];
        let mut cells = [const { Cell::EMPTY }; 4];
        let mut book = Leases::new(&p, Usage::default(), &mut states, &mut cells)?;
        assert_eq!(
            book.reserve(
                &[charges(Kind::QueueBytes, 1)],
                Deadline::after(Tick(0), 10)?,
                Tick(10)
            ),
            Err(Error::Expired)
        );
        let first = reserve(&mut book, &[charges(Kind::QueueBytes, 10); 2])?;
        let second = reserve(&mut book, &[charges(Kind::QueueBytes, 5)])?;
        let member = book.part(first, 1)?;
        assert_eq!(book.part(first, 2), Err(Error::Stale));
        assert_eq!(
            book.begin_effect(member, [11, 0, 0, 0], Tick(1)).err(),
            Some(Error::Invalid)
        );
        let mut ticket = book.begin_effect(member, [10, 0, 0, 0], Tick(1))?;
        let mut short = [Some(second)];
        assert_eq!(book.expired(Tick(10), &mut short), Err(Error::Full));
        assert_eq!(short, [Some(second)]);
        let mut expired = [None; 4];
        assert_eq!(book.expired(Tick(9), &mut expired)?, 0);
        assert_eq!(book.expired(Tick(10), &mut expired)?, 2);
        assert_eq!(expired, [Some(first), Some(second), None, None]);
        assert_eq!(book.cancel(first), Err(Error::Busy));
        assert_eq!(book.pending(Kind::QueueBytes)?, 25);
        book.cancel(second)?;
        book.complete_effect(&mut ticket, EffectResult::Proven([0; 4]))?;
        book.cancel(first)?;
        assert_eq!(book.pending(Kind::QueueBytes)?, 0);
        assert_eq!(book.available_cells(), 4);
        Ok(())
    }
    #[test]
    fn overflow_and_disabled_extensions_leave_the_book_unchanged(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let p = plan(100)?;
        let mut states = [const { SlotState::EMPTY }; 4];
        let mut cells = [const { Cell::EMPTY }; 4];
        let mut book = Leases::new(&p, Usage::default(), &mut states, &mut cells)?;
        assert!(matches!(
            reserve(
                &mut book,
                &[
                    charges(Kind::QueueBytes, u64::MAX),
                    charges(Kind::QueueBytes, 1)
                ]
            ),
            Err(Error::Arithmetic(_))
        ));
        assert_eq!(book.available_cells(), 4);
        assert_eq!(book.pending(Kind::QueueBytes)?, 0);
        let lease = reserve(&mut book, &[charges(Kind::QueueBytes, 1)])?;
        let part = book.part(lease, 0)?;
        assert!(matches!(
            book.extend(part, [u64::MAX, 0, 0, 0], Tick(1)),
            Err(Error::Arithmetic(_))
        ));
        assert_eq!(
            book.extend(part, [2, 1, 0, 0], Tick(1)),
            Err(Error::Invalid)
        );
        assert_eq!(book.remaining(part)?, charges(Kind::QueueBytes, 1));
        assert_eq!(book.pending(Kind::QueueBytes)?, 1);
        assert_eq!(book.pending(Kind::BodyBytes)?, 0);
        let mut ticket = book.begin_effect(part, [1, 0, 0, 0], Tick(1))?;
        book.complete_effect(&mut ticket, EffectResult::Uncertain)?;
        book.extend(part, [2, 0, 0, 0], Tick(2))?;
        assert_eq!(book.remaining(part)?, charges(Kind::QueueBytes, 2));
        book.cancel(lease)?;
        assert_eq!(book.used(Kind::QueueBytes)?, 1);
        assert_eq!(book.pending(Kind::QueueBytes)?, 0);
        Ok(())
    }
    #[test]
    fn constructor_refuses_usage_over_cap_and_keeps_occupied_cells(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let p = plan(100)?;
        let mut states = [const { SlotState::EMPTY }; 2];
        let mut cells = [const { Cell::EMPTY }; 2];
        let mut used = Usage::default();
        used.add(Kind::QueueBytes, 101)?;
        assert_eq!(
            Leases::new(&p, used, &mut states, &mut cells).err(),
            Some(Error::Quota(Kind::QueueBytes))
        );
        let mut book = Leases::new(&p, Usage::default(), &mut states, &mut cells)?;
        let lease = reserve(&mut book, &[charges(Kind::QueueBytes, 1)])?;
        let _ticket = book.begin_effect(book.part(lease, 0)?, [1, 0, 0, 0], Tick(1))?;
        drop(book);
        assert_eq!(
            Leases::new(&p, Usage::default(), &mut states, &mut cells).err(),
            Some(Error::Invalid)
        );
        Ok(())
    }
}

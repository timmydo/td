//! Serialized physical/logical admission over caller-owned storage. Adapters
//! still own file identity, exact effect proofs and declared logical categories.
use super::{
    filesystems::{
        self, CheckedSample, FilesystemId, Observation, ProbeTicket, Registry, MAX_FILESYSTEMS,
    },
    logical::{Charges, EffectResult, EffectTicket, LeaseId, PartId, Physical, MAX_GROUP_CELLS},
    quota::Kind,
    space::{Counters, FileGrowth, RoundedGrowth},
    writer::{
        self, AppendResult, AppendTicket, CheckpointNeed, FrameBudget, FrameId, Grant, WriterLedger,
    },
};
use crate::{
    format,
    ports::{Deadline, Tick},
};

pub type Samples = [Option<CheckedSample>; MAX_FILESYSTEMS];
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Invalid,
    Closed,
    Stopped,
    Writer(writer::Error),
    Filesystem(filesystems::Error),
    Arithmetic(super::Error),
}
impl From<writer::Error> for Error {
    fn from(e: writer::Error) -> Self {
        Self::Writer(e)
    }
}
impl From<filesystems::Error> for Error {
    fn from(e: filesystems::Error) -> Self {
        Self::Filesystem(e)
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
            Self::Invalid => f.write_str("invalid coupled admission"),
            Self::Closed => f.write_str("physical admission closed"),
            Self::Stopped => f.write_str("physical admission stopped"),
            Self::Writer(e) => e.fmt(f),
            Self::Filesystem(e) => e.fmt(f),
            Self::Arithmetic(e) => e.fmt(f),
        }
    }
}
impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Writer(e) => Some(e),
            Self::Filesystem(e) => Some(e),
            Self::Arithmetic(e) => Some(e),
            _ => None,
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum State {
    Uninitialized,
    Ready,
    Closed,
    Stopped,
}
#[derive(Clone, Copy, Debug)]
pub struct Request {
    pub logical: Charges,
    pub filesystem: FilesystemId,
    pub growth: RoundedGrowth,
}
#[derive(Clone, Copy, Debug)]
pub struct IoPlan {
    pub logical: [u64; 4],
    pub physical: RoundedGrowth,
}
#[derive(Clone, Copy, Debug)]
pub enum IoResult {
    Proven {
        logical: [u64; 4],
        physical: RoundedGrowth,
    },
    Uncertain,
}
#[derive(Debug)]
#[must_use = "complete the effect; dropping it keeps the lease pinned"]
pub struct IoTicket {
    part: PartId,
    effect: EffectTicket,
    planned: RoundedGrowth,
}
#[derive(Debug)]
#[must_use = "complete the append; dropping it keeps the writer pinned"]
pub struct FrameTicket {
    frame: FrameId,
    append: AppendTicket,
    planned: RoundedGrowth,
}
#[derive(Clone, Copy, Debug)]
pub enum FrameResult {
    Synced(RoundedGrowth),
    NotWritten,
    Uncertain,
}

pub struct Coordinator<'a> {
    writer: WriterLedger<'a>,
    filesystems: Registry<'a>,
    generation_fs: FilesystemId,
    journal_fs: FilesystemId,
    initialized: bool,
}
impl<'a> Coordinator<'a> {
    /// Both metadata locations must already be registered against pinned roots.
    /// Existing unbound leases cannot be adopted by this physical coordinator.
    pub fn new(
        writer: WriterLedger<'a>,
        filesystems: Registry<'a>,
        generation_fs: FilesystemId,
        journal_fs: FilesystemId,
    ) -> Result<Self, Error> {
        if !writer.pristine() {
            return Err(Error::Invalid);
        }
        filesystems.allocation_unit(generation_fs)?;
        filesystems.allocation_unit(journal_fs)?;
        Ok(Self {
            writer,
            filesystems,
            generation_fs,
            journal_fs,
            initialized: false,
        })
    }
    pub fn state(&self) -> State {
        if !self.initialized {
            return State::Uninitialized;
        }
        match self.writer.phase() {
            writer::Phase::Open => State::Ready,
            writer::Phase::Stopped => State::Stopped,
            writer::Phase::Barrier | writer::Phase::AwaitingSpace => State::Closed,
        }
    }
    fn ready(&self) -> Result<(), Error> {
        match self.state() {
            State::Ready => Ok(()),
            State::Stopped => Err(Error::Stopped),
            _ => Err(Error::Closed),
        }
    }
    pub fn counters(&self, id: FilesystemId) -> Result<Counters, Error> {
        Ok(self.filesystems.counters(id)?)
    }
    pub fn used(&self, kind: Kind) -> Result<u64, Error> {
        Ok(self.writer.used(kind)?)
    }
    pub fn pending(&self, kind: Kind) -> Result<u64, Error> {
        Ok(self.writer.pending(kind)?)
    }
    pub fn checkpoint_need(&self) -> Result<CheckpointNeed, Error> {
        Ok(self.writer.checkpoint_need()?)
    }
    pub fn begin_probe(
        &self,
        id: FilesystemId,
        enclosing: Deadline,
        now: Tick,
    ) -> Result<ProbeTicket, Error> {
        let milliseconds = super::mul(
            self.writer.plan().work().admission_seconds,
            1000,
            "probe window",
        )?;
        let bounded = Deadline::after(now, milliseconds).map_err(|_| Error::Invalid)?;
        Ok(self
            .filesystems
            .begin_probe(id, enclosing.min(bounded), now)?)
    }
    pub fn consume_observation(
        &self,
        id: FilesystemId,
        observation: &mut Option<Observation>,
        now: Tick,
    ) -> Result<CheckedSample, Error> {
        Ok(self.filesystems.consume(id, observation, now)?)
    }
    /// Installs the baseline completion reserve before any request can write.
    pub fn initialize(&mut self, samples: &mut Samples, now: Tick) -> Result<(), Error> {
        let samples = take_samples(samples);
        if self.initialized {
            return Err(Error::Invalid);
        }
        let plan = self.writer.plan();
        let need = self.writer.checkpoint_need()?;
        let generation =
            generation_growth(need, self.filesystems.allocation_unit(self.generation_fs)?)?;
        let journal = journal_growth(need, self.filesystems.allocation_unit(self.journal_fs)?)?;
        let mut stage = self.filesystems.stage()?;
        protect(
            &mut stage,
            self.generation_fs,
            generation,
            self.journal_fs,
            journal,
        )?;
        stage.assess(plan, samples, now)?;
        stage.publish();
        self.initialized = true;
        Ok(())
    }
    /// Samples cover each distinct requested filesystem plus both metadata
    /// locations. Every provided sample is consumed even when admission fails.
    pub fn grant(
        &mut self,
        requests: &[Request],
        frame: Option<FrameBudget>,
        deadline: Deadline,
        samples: &mut Samples,
        now: Tick,
    ) -> Result<Grant, Error> {
        let samples = take_samples(samples);
        self.ready()?;
        let count = requests
            .len()
            .checked_add(usize::from(frame.is_some()))
            .ok_or(Error::Invalid)?;
        if count == 0 || count > MAX_GROUP_CELLS {
            return Err(Error::Invalid);
        }
        let mut charges = [[super::quota::Charge::ZERO; 4]; MAX_GROUP_CELLS];
        let journal_unit = self.filesystems.allocation_unit(self.journal_fs)?;
        let zero = Physical {
            filesystem: self.journal_fs,
            remaining: RoundedGrowth::from_files(journal_unit, &[], 0)?,
        };
        let mut bindings = [zero; MAX_GROUP_CELLS];
        for ((out, binding), request) in charges.iter_mut().zip(bindings.iter_mut()).zip(requests) {
            *out = request.logical;
            *binding = Physical {
                filesystem: request.filesystem,
                remaining: request.growth,
            };
        }
        if let Some(frame) = frame {
            let growth = RoundedGrowth::from_files(
                journal_unit,
                &[FileGrowth {
                    old_length: 0,
                    new_length: frame.bytes(),
                }],
                0,
            )?;
            *bindings.get_mut(requests.len()).ok_or(Error::Invalid)? = Physical {
                filesystem: self.journal_fs,
                remaining: growth,
            };
        }
        let plan = self.writer.plan();
        let prepared = self.writer.prepare(
            charges.get(..requests.len()).ok_or(Error::Invalid)?,
            frame,
            deadline,
            now,
        )?;
        let need = prepared.checkpoint_need();
        let generation =
            generation_growth(need, self.filesystems.allocation_unit(self.generation_fs)?)?;
        let journal = journal_growth(need, journal_unit)?;
        let bindings = bindings.get(..count).ok_or(Error::Invalid)?;
        let mut stage = self.filesystems.stage()?;
        protect(
            &mut stage,
            self.generation_fs,
            generation,
            self.journal_fs,
            journal,
        )?;
        for binding in bindings {
            stage.add_pending(*binding, true)?;
        }
        stage.assess(plan, samples, now)?;
        let grant = prepared.install_bound(bindings, now)?;
        stage.publish();
        Ok(grant)
    }
    pub fn part(&self, grant: Grant, ordinal: u16) -> Result<PartId, Error> {
        Ok(self.writer.part(grant, ordinal)?)
    }
    pub fn remaining(&self, part: PartId) -> Result<Charges, Error> {
        Ok(self.writer.remaining(part)?)
    }
    pub fn physical_remaining(&self, part: PartId) -> Result<RoundedGrowth, Error> {
        Ok(self.writer.physical(part)?.remaining)
    }
    pub fn expired(&self, now: Tick, output: &mut [Option<LeaseId>]) -> Result<usize, Error> {
        Ok(self.writer.expired(now, output)?)
    }
    pub fn cancel(&mut self, lease: LeaseId) -> Result<(), Error> {
        let group = self.writer.physical_group(lease)?;
        let mut stage = self.filesystems.stage()?;
        for physical in group.into_iter().flatten() {
            stage.release(physical)?;
        }
        self.writer.cancel_lease(lease)?;
        stage.publish();
        Ok(())
    }
    /// Only ordinary parts extend. A frame reserves its full transaction ceiling
    /// at initial admission; a successfully committed frame can never be reused.
    pub fn extend(
        &mut self,
        part: PartId,
        extra: IoPlan,
        samples: &mut Samples,
        now: Tick,
    ) -> Result<(), Error> {
        let samples = take_samples(samples);
        self.ready()?;
        let binding = self.writer.physical(part)?;
        let plan = self.writer.plan();
        let need = self.writer.checkpoint_need()?;
        let generation =
            generation_growth(need, self.filesystems.allocation_unit(self.generation_fs)?)?;
        let journal = journal_growth(need, self.filesystems.allocation_unit(self.journal_fs)?)?;
        let mut stage = self.filesystems.stage()?;
        protect(
            &mut stage,
            self.generation_fs,
            generation,
            self.journal_fs,
            journal,
        )?;
        stage.add_pending(
            Physical {
                filesystem: binding.filesystem,
                remaining: extra.physical,
            },
            false,
        )?;
        stage.assess(plan, samples, now)?;
        self.writer
            .extend_bound(part, extra.logical, extra.physical, now)?;
        stage.publish();
        Ok(())
    }
    pub fn begin_io(&mut self, part: PartId, plan: IoPlan, now: Tick) -> Result<IoTicket, Error> {
        if !self.initialized {
            return Err(Error::Closed);
        }
        let binding = self.writer.physical(part)?;
        binding.remaining.checked_sub(plan.physical)?;
        let effect = self.writer.begin_effect(part, plan.logical, now)?;
        Ok(IoTicket {
            part,
            effect,
            planned: plan.physical,
        })
    }
    /// Invalid proof keeps the live effect and both ledgers unchanged. Completion
    /// can account for admitted effects after a deadline or writer stop.
    pub fn complete_io(&mut self, ticket: &mut IoTicket, result: IoResult) -> Result<(), Error> {
        let binding = self.writer.physical(ticket.part)?;
        let (logical, actual) = match result {
            IoResult::Proven { logical, physical } => (EffectResult::Proven(logical), physical),
            IoResult::Uncertain => (EffectResult::Uncertain, ticket.planned),
        };
        ticket.planned.checked_sub(actual)?;
        let mut stage = self.filesystems.stage()?;
        stage.finish(binding, actual, false)?;
        self.writer
            .complete_bound(&mut ticket.effect, logical, actual)?;
        stage.publish();
        Ok(())
    }
    pub fn begin_append(
        &mut self,
        frame: FrameId,
        actual: FrameBudget,
        now: Tick,
    ) -> Result<FrameTicket, Error> {
        self.ready()?;
        let binding = self.writer.physical(WriterLedger::frame_part(frame))?;
        let planned = RoundedGrowth::from_files(
            binding.remaining.allocation_unit(),
            &[FileGrowth {
                old_length: 0,
                new_length: actual.bytes(),
            }],
            0,
        )?;
        binding.remaining.checked_sub(planned)?;
        let append = self.writer.begin_append(frame, actual, now)?;
        Ok(FrameTicket {
            frame,
            append,
            planned,
        })
    }
    pub fn complete_append(
        &mut self,
        ticket: &mut FrameTicket,
        result: FrameResult,
    ) -> Result<(), Error> {
        if matches!(result, FrameResult::Uncertain) {
            return Ok(self.writer.complete_append_bound(
                &mut ticket.append,
                AppendResult::Uncertain,
                None,
            )?);
        }
        let binding = self
            .writer
            .physical(WriterLedger::frame_part(ticket.frame))?;
        let (outcome, actual, release) = match result {
            FrameResult::Synced(actual) => (AppendResult::Synced, actual, true),
            FrameResult::NotWritten => (AppendResult::NotWritten, ticket.planned.zeroed(), false),
            FrameResult::Uncertain => return Err(Error::Invalid),
        };
        ticket.planned.checked_sub(actual)?;
        let mut stage = self.filesystems.stage()?;
        stage.finish(binding, actual, release)?;
        self.writer
            .complete_append_bound(&mut ticket.append, outcome, Some(actual))?;
        stage.publish();
        Ok(())
    }
}
fn take_samples(samples: &mut Samples) -> Samples {
    std::mem::replace(samples, std::array::from_fn(|_| None))
}
fn generation_growth(need: CheckpointNeed, unit: u64) -> Result<RoundedGrowth, super::Error> {
    let files = super::add(
        super::widen(format::TABLE_COUNT, "checkpoint tables")?,
        2,
        "checkpoint files",
    )?;
    RoundedGrowth::from_total_bound(
        unit,
        need.generation_bytes(),
        files,
        super::add(files, 1, "checkpoint inodes")?,
    )
}
fn journal_growth(need: CheckpointNeed, unit: u64) -> Result<RoundedGrowth, super::Error> {
    RoundedGrowth::from_files(
        unit,
        &[FileGrowth {
            old_length: 0,
            new_length: need.fresh_journal_bytes(),
        }],
        1,
    )
}
fn protect(
    stage: &mut filesystems::Stage<'_, '_>,
    generation_fs: FilesystemId,
    generation: RoundedGrowth,
    journal_fs: FilesystemId,
    journal: RoundedGrowth,
) -> Result<(), Error> {
    if generation_fs == journal_fs {
        stage.protect(generation_fs, generation.checked_add(journal)?)?;
    } else {
        stage.protect(generation_fs, generation)?;
        stage.protect(journal_fs, journal)?;
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::{
        admission::{
            logical,
            quota::{Charge, Usage},
            space::{Growth, Inodes, Sample},
            DiskLimits, Plan, ViewMode, WorkLimits,
        },
        limits::Limits,
        ownership::{self, SlotState},
    };
    type TestResult = Result<(), Box<dyn std::error::Error>>;
    fn plan() -> Result<Plan, super::super::Error> {
        DiskLimits::default().plan(
            &Limits::default()
                .plan()
                .map_err(|_| super::super::Error::Inconsistent("fixture resources"))?,
            WorkLimits::default(),
            ViewMode::OnlineBackground,
        )
    }
    fn deadline() -> Deadline {
        Deadline::after(Tick(0), 100000).unwrap()
    }
    fn growth(unit: u64, bytes: u64, inodes: u64) -> RoundedGrowth {
        RoundedGrowth::from_files(
            unit,
            &[FileGrowth {
                old_length: 0,
                new_length: bytes,
            }],
            inodes,
        )
        .unwrap()
    }
    fn request(id: FilesystemId, unit: u64, logical: u64, bytes: u64, inodes: u64) -> Request {
        Request {
            logical: [
                Charge {
                    kind: Kind::BodyBytes,
                    amount: logical,
                },
                Charge::ZERO,
                Charge::ZERO,
                Charge::ZERO,
            ],
            filesystem: id,
            growth: growth(unit, bytes, inodes),
        }
    }
    fn with_coordinator(
        p: &Plan,
        unit: u64,
        distinct: bool,
        run: impl FnOnce(&mut Coordinator<'_>, &[FilesystemId]) -> TestResult,
    ) -> TestResult {
        let mut ls = [const { SlotState::EMPTY }; 64];
        let mut lc = [const { logical::Cell::EMPTY }; 64];
        let mut fs = [const { SlotState::EMPTY }; 3];
        let mut fc = [const { filesystems::Cell::EMPTY }; 3];
        let mut registry = Registry::new(&mut fs, &mut fc)?;
        let mut ids = Vec::new();
        for key in 0..if distinct { 3 } else { 1 } {
            let mut id = None;
            for _ in 0..1000 {
                match registry.register(filesystems::BackingKey(key), unit) {
                    Err(filesystems::Error::Slot(ownership::Error::Contended)) => {
                        std::thread::yield_now()
                    }
                    result => {
                        id = Some(result?);
                        break;
                    }
                }
            }
            ids.push(id.ok_or("registry contention")?);
        }
        let mut used = Usage::default();
        used.add(Kind::LiveMetadataBytes, 1232)?;
        let writer = WriterLedger::new(p, 1232, used, &mut ls, &mut lc)?;
        let mut coordinator =
            Coordinator::new(writer, registry, ids[0], *ids.get(1).unwrap_or(&ids[0]))?;
        run(&mut coordinator, &ids)
    }
    fn samples(c: &Coordinator<'_>, ids: &[FilesystemId], now: Tick) -> Samples {
        let mut samples: Samples = std::array::from_fn(|_| None);
        for (slot, id) in samples.iter_mut().zip(ids) {
            *slot = Some(sample(c, *id, u64::MAX, Inodes::Available(u64::MAX), now));
        }
        samples
    }
    fn sample(
        c: &Coordinator<'_>,
        id: FilesystemId,
        bytes: u64,
        inodes: Inodes,
        now: Tick,
    ) -> CheckedSample {
        let ticket = c.begin_probe(id, deadline(), now).unwrap();
        let mut observation = Some(
            ticket
                .complete(Ok(Sample {
                    available_bytes: bytes,
                    inodes,
                    allocation_unit: c.filesystems.allocation_unit(id).unwrap(),
                }))
                .unwrap(),
        );
        c.consume_observation(id, &mut observation, now).unwrap()
    }
    fn initialize(c: &mut Coordinator<'_>, ids: &[FilesystemId]) -> TestResult {
        c.initialize(&mut samples(c, &ids[..ids.len().min(2)], Tick(0)), Tick(0))?;
        Ok(())
    }
    fn grant(
        c: &mut Coordinator<'_>,
        ids: &[FilesystemId],
        requests: &[Request],
        frame: Option<FrameBudget>,
    ) -> Result<Grant, Error> {
        for _ in 0..1000 {
            match c.grant(
                requests,
                frame,
                deadline(),
                &mut samples(c, ids, Tick(1)),
                Tick(1),
            ) {
                Err(Error::Writer(writer::Error::Logical(logical::Error::Slot(
                    ownership::Error::Contended,
                )))) => std::thread::yield_now(),
                result => return result,
            }
        }
        Err(Error::Invalid)
    }
    #[test]
    fn cold_start_protects_both_metadata_locations_and_shared_floor_once() -> TestResult {
        let p = plan()?;
        for distinct in [false, true] {
            with_coordinator(&p, 4194304, distinct, |c, ids| {
                assert_eq!(c.state(), State::Uninitialized);
                let mut input = samples(c, &ids[..1], Tick(0));
                assert_eq!(
                    c.grant(
                        &[request(ids[0], 4194304, 1, 1, 1)],
                        None,
                        deadline(),
                        &mut input,
                        Tick(0)
                    ),
                    Err(Error::Closed)
                );
                assert!(input.iter().all(Option::is_none));
                if distinct {
                    assert_eq!(
                        c.initialize(&mut samples(c, &ids[..1], Tick(0)), Tick(0)),
                        Err(Error::Filesystem(filesystems::Error::Missing))
                    );
                    assert_eq!(c.counters(ids[0])?, Counters::default());
                }
                initialize(c, ids)?;
                assert_eq!(c.state(), State::Ready);
                assert_eq!(
                    c.counters(ids[0])?.checkpoint,
                    Growth {
                        bytes: if distinct { 54525952 } else { 58720256 },
                        inodes: if distinct { 14 } else { 15 }
                    }
                );
                if distinct {
                    assert_eq!(
                        c.counters(ids[1])?.checkpoint,
                        Growth {
                            bytes: 4194304,
                            inodes: 1
                        }
                    );
                }
                assert_eq!(
                    c.initialize(&mut samples(c, &ids[..1], Tick(0)), Tick(0)),
                    Err(Error::Invalid)
                );
                Ok(())
            })?;
        }
        with_coordinator(&p, 4096, false, |c, ids| {
            initialize(c, ids)?;
            let cp = c.counters(ids[0])?.checkpoint;
            let mut issued = None;
            for _ in 0..1000 {
                let mut input: Samples = std::array::from_fn(|_| None);
                input[0] = Some(sample(
                    c,
                    ids[0],
                    p.disk().free_bytes + cp.bytes + 8192,
                    Inodes::Available(p.disk().free_inodes + cp.inodes + 2),
                    Tick(1),
                ));
                match c.grant(
                    &[request(ids[0], 4096, 1, 1, 1); 2],
                    None,
                    deadline(),
                    &mut input,
                    Tick(1),
                ) {
                    Err(Error::Writer(writer::Error::Logical(logical::Error::Slot(
                        ownership::Error::Contended,
                    )))) => std::thread::yield_now(),
                    result => {
                        issued = Some(result?);
                        break;
                    }
                }
            }
            let g = issued.ok_or("grant contention")?;
            assert_eq!(
                c.counters(ids[0])?.pending,
                Growth {
                    bytes: 8192,
                    inodes: 2
                }
            );
            c.cancel(g.lease())?;
            Ok(())
        })
    }
    #[test]
    fn failed_multifs_admission_discards_samples_without_installing_any_charge() -> TestResult {
        let p = plan()?;
        with_coordinator(&p, 4096, true, |c, ids| {
            initialize(c, ids)?;
            let before = [
                c.counters(ids[0])?,
                c.counters(ids[1])?,
                c.counters(ids[2])?,
            ];
            let mut unnecessary = samples(c, ids, Tick(1));
            assert_eq!(
                c.grant(
                    &[request(ids[0], 4096, 10, 1, 1)],
                    None,
                    deadline(),
                    &mut unnecessary,
                    Tick(1)
                ),
                Err(Error::Filesystem(filesystems::Error::Invalid))
            );
            assert!(unnecessary.iter().all(Option::is_none));
            assert_eq!(
                [
                    c.counters(ids[0])?,
                    c.counters(ids[1])?,
                    c.counters(ids[2])?
                ],
                before
            );
            assert_eq!(c.pending(Kind::BodyBytes)?, 0);
            let mut input = samples(c, ids, Tick(1));
            input[2] = Some(sample(
                c,
                ids[2],
                p.disk().free_bytes + 4095,
                Inodes::Unsupported,
                Tick(1),
            ));
            assert_eq!(
                c.grant(
                    &[
                        request(ids[0], 4096, 10, 1, 1),
                        request(ids[2], 4096, 20, 1, 1)
                    ],
                    Some(FrameBudget::new(4097, 1)?),
                    deadline(),
                    &mut input,
                    Tick(1)
                ),
                Err(Error::Filesystem(filesystems::Error::Space(
                    super::super::space::Refusal::Bytes
                )))
            );
            assert!(input.iter().all(Option::is_none));
            assert_eq!(
                [
                    c.counters(ids[0])?,
                    c.counters(ids[1])?,
                    c.counters(ids[2])?
                ],
                before
            );
            assert_eq!(c.pending(Kind::BodyBytes)?, 0);
            assert_eq!(c.pending(Kind::ActiveJournalBytes)?, 0);
            let mut expired = [None; 64];
            assert_eq!(c.expired(Tick(100000), &mut expired)?, 0);
            let g = grant(
                c,
                ids,
                &[request(ids[2], 4096, 20, 1, 1)],
                Some(FrameBudget::new(4097, 1)?),
            )?;
            assert_eq!(c.counters(ids[1])?.pending.bytes, 8192);
            assert!(c.counters(ids[0])?.checkpoint.bytes > before[0].checkpoint.bytes);
            c.cancel(g.lease())?;
            Ok(())
        })
    }
    #[test]
    fn completion_and_cancellation_account_for_partial_and_uncertain_effects() -> TestResult {
        let p = plan()?;
        with_coordinator(&p, 4096, true, |c, ids| {
            initialize(c, ids)?;
            let g = grant(c, ids, &[request(ids[2], 4096, 100, 8192, 2)], None)?;
            let part = c.part(g, 0)?;
            let mut ticket = c.begin_io(
                part,
                IoPlan {
                    logical: [60, 0, 0, 0],
                    physical: growth(4096, 4096, 1),
                },
                Tick(2),
            )?;
            let before = c.counters(ids[2])?;
            assert!(c.cancel(g.lease()).is_err());
            assert!(c
                .complete_io(
                    &mut ticket,
                    IoResult::Proven {
                        logical: [61, 0, 0, 0],
                        physical: growth(4096, 4096, 1)
                    }
                )
                .is_err());
            assert_eq!(c.counters(ids[2])?, before);
            assert_eq!(c.pending(Kind::BodyBytes)?, 100);
            assert!(c
                .complete_io(
                    &mut ticket,
                    IoResult::Proven {
                        logical: [40, 0, 0, 0],
                        physical: growth(4096, 8192, 1)
                    }
                )
                .is_err());
            assert_eq!(c.counters(ids[2])?, before);
            c.complete_io(
                &mut ticket,
                IoResult::Proven {
                    logical: [40, 0, 0, 0],
                    physical: growth(4096, 4096, 1),
                },
            )?;
            assert_eq!(c.used(Kind::BodyBytes)?, 40);
            assert_eq!(c.pending(Kind::BodyBytes)?, 60);
            assert_eq!(
                c.counters(ids[2])?.pending,
                Growth {
                    bytes: 4096,
                    inodes: 1
                }
            );
            let before = c.counters(ids[2])?;
            assert!(c.complete_io(&mut ticket, IoResult::Uncertain).is_err());
            assert_eq!(c.counters(ids[2])?, before);
            let mut ticket = c.begin_io(
                part,
                IoPlan {
                    logical: [20, 0, 0, 0],
                    physical: growth(4096, 4096, 1),
                },
                Tick(3),
            )?;
            c.complete_io(&mut ticket, IoResult::Uncertain)?;
            assert_eq!(c.used(Kind::BodyBytes)?, 60);
            assert_eq!(
                c.counters(ids[2])?.completed,
                Growth {
                    bytes: 8192,
                    inodes: 2
                }
            );
            assert_eq!(c.counters(ids[2])?.pending, Growth::default());
            assert!(c.filesystems.unregister(ids[2]).is_err());
            c.cancel(g.lease())?;
            assert_eq!(c.pending(Kind::BodyBytes)?, 0);
            assert_eq!(c.used(Kind::BodyBytes)?, 60);
            assert_eq!(
                c.counters(ids[2])?.completed,
                Growth {
                    bytes: 8192,
                    inodes: 2
                }
            );
            c.filesystems.unregister(ids[2])?;
            Ok(())
        })
    }
    #[test]
    fn final_admission_rechecks_sample_time_completeness_identity_and_concurrent_growth(
    ) -> TestResult {
        let p = plan()?;
        with_coordinator(&p, 4096, false, |c, ids| {
            initialize(c, ids)?;
            let g = grant(c, ids, &[request(ids[0], 4096, 10, 4096, 1)], None)?;
            let part = c.part(g, 0)?;
            let cp = c.counters(ids[0])?.checkpoint;
            let mut input: Samples = std::array::from_fn(|_| None);
            input[0] = Some(sample(
                c,
                ids[0],
                p.disk().free_bytes + cp.bytes + 8191,
                Inodes::Unsupported,
                Tick(1),
            ));
            let mut ticket = c.begin_io(
                part,
                IoPlan {
                    logical: [10, 0, 0, 0],
                    physical: growth(4096, 4096, 1),
                },
                Tick(2),
            )?;
            c.complete_io(&mut ticket, IoResult::Uncertain)?;
            let req = [request(ids[0], 4096, 1, 4096, 1)];
            assert_eq!(
                c.grant(&req, None, deadline(), &mut input, Tick(2)),
                Err(Error::Filesystem(filesystems::Error::Space(
                    super::super::space::Refusal::Bytes
                )))
            );
            assert!(input.iter().all(Option::is_none));
            let mut input = samples(c, ids, Tick(1));
            assert_eq!(
                c.grant(
                    &req,
                    None,
                    deadline(),
                    &mut input,
                    Tick(1 + p.work().admission_seconds * 1000)
                ),
                Err(Error::Filesystem(filesystems::Error::Expired))
            );
            assert!(input.iter().all(Option::is_none));
            let mut input = samples(c, ids, Tick(2));
            assert_eq!(
                c.grant(&req, None, deadline(), &mut input, Tick(1)),
                Err(Error::Filesystem(filesystems::Error::Invalid))
            );
            let mut input = samples(c, ids, Tick(1));
            input[1] = Some(sample(c, ids[0], u64::MAX, Inodes::Unsupported, Tick(1)));
            assert_eq!(
                c.grant(&req, None, deadline(), &mut input, Tick(1)),
                Err(Error::Filesystem(filesystems::Error::Invalid))
            );
            assert_eq!(
                c.grant(
                    &req,
                    None,
                    deadline(),
                    &mut std::array::from_fn(|_| None),
                    Tick(1)
                ),
                Err(Error::Filesystem(filesystems::Error::Missing))
            );
            // A sample minted through Registry can predate coordinator ownership
            // and carry a longer deadline; final admission still caps its age.
            let ticket = c.filesystems.begin_probe(ids[0], deadline(), Tick(1))?;
            let mut observation = Some(ticket.complete(Ok(Sample {
                available_bytes: u64::MAX,
                inodes: Inodes::Unsupported,
                allocation_unit: 4096,
            }))?);
            let now = Tick(1 + p.work().admission_seconds * 1000);
            let mut input: Samples = std::array::from_fn(|_| None);
            input[0] = Some(c.consume_observation(ids[0], &mut observation, now)?);
            assert_eq!(
                c.grant(&req, None, deadline(), &mut input, now),
                Err(Error::Filesystem(filesystems::Error::Expired))
            );
            assert!(input.iter().all(Option::is_none));
            c.cancel(g.lease())?;
            Ok(())
        })
    }
    #[test]
    fn extension_failure_after_physical_staging_preserves_both_ledgers() -> TestResult {
        let p = plan()?;
        with_coordinator(&p, 4096, false, |c, ids| {
            initialize(c, ids)?;
            let g = grant(c, ids, &[request(ids[0], 4096, 100, 4096, 1)], None)?;
            let part = c.part(g, 0)?;
            let before = c.counters(ids[0])?;
            assert_eq!(
                c.extend(
                    part,
                    IoPlan {
                        logical: [p.disk().body_bytes, 0, 0, 0],
                        physical: growth(4096, 4096, 1)
                    },
                    &mut samples(c, ids, Tick(2)),
                    Tick(2)
                ),
                Err(Error::Writer(writer::Error::Logical(
                    logical::Error::Quota(Kind::BodyBytes)
                )))
            );
            assert_eq!(c.counters(ids[0])?, before);
            assert_eq!(c.pending(Kind::BodyBytes)?, 100);
            assert_eq!(
                c.extend(
                    part,
                    IoPlan {
                        logical: [1, 1, 0, 0],
                        physical: growth(4096, 4096, 1)
                    },
                    &mut samples(c, ids, Tick(2)),
                    Tick(2)
                ),
                Err(Error::Writer(writer::Error::Logical(
                    logical::Error::Invalid
                )))
            );
            assert_eq!(c.counters(ids[0])?, before);
            assert_eq!(
                c.extend(
                    part,
                    IoPlan {
                        logical: [1, 0, 0, 0],
                        physical: growth(8192, 8192, 1)
                    },
                    &mut samples(c, ids, Tick(2)),
                    Tick(2)
                ),
                Err(Error::Filesystem(filesystems::Error::UnitChanged))
            );
            assert_eq!(c.counters(ids[0])?, before);
            c.extend(
                part,
                IoPlan {
                    logical: [50, 0, 0, 0],
                    physical: growth(4096, 4096, 1),
                },
                &mut samples(c, ids, Tick(2)),
                Tick(2),
            )?;
            assert_eq!(c.pending(Kind::BodyBytes)?, 150);
            assert_eq!(
                c.physical_remaining(part)?.amount(),
                Growth {
                    bytes: 8192,
                    inodes: 2
                }
            );
            c.cancel(g.lease())?;
            assert_eq!(c.counters(ids[0])?.pending, Growth::default());
            Ok(())
        })
    }
    #[test]
    fn journal_retry_and_one_shot_sync_release_unused_rounded_capacity() -> TestResult {
        let p = plan()?;
        with_coordinator(&p, 4096, false, |c, ids| {
            initialize(c, ids)?;
            let g = grant(c, ids, &[], Some(FrameBudget::new(8193, 10)?))?;
            let frame = g.frame().ok_or("frame")?;
            assert_eq!(c.counters(ids[0])?.pending.bytes, 12288);
            let frame_part = c.part(g, 0)?;
            assert!(c
                .begin_io(
                    frame_part,
                    IoPlan {
                        logical: [100, 1, 0, 0],
                        physical: growth(4096, 4096, 0)
                    },
                    Tick(2)
                )
                .is_err());
            assert_eq!(c.counters(ids[0])?.pending.bytes, 12288);
            let actual = FrameBudget::new(4046, 1)?;
            let mut old = c.begin_append(frame, actual, Tick(2))?;
            c.complete_append(&mut old, FrameResult::NotWritten)?;
            assert_eq!(c.counters(ids[0])?.pending.bytes, 12288);
            assert_eq!(c.counters(ids[0])?.completed.bytes, 0);
            let mut ticket = c.begin_append(frame, actual, Tick(3))?;
            assert_eq!(
                c.complete_append(&mut old, FrameResult::Uncertain),
                Err(Error::Writer(writer::Error::Logical(
                    logical::Error::InactiveTicket
                )))
            );
            assert_eq!(c.state(), State::Ready);
            assert_eq!(
                c.complete_append(&mut ticket, FrameResult::Synced(growth(4096, 8192, 0))),
                Err(Error::Arithmetic(super::super::Error::Inconsistent(
                    "physical byte underflow"
                )))
            );
            assert_eq!(c.counters(ids[0])?.pending.bytes, 12288);
            c.complete_append(&mut ticket, FrameResult::Synced(growth(4096, 4096, 0)))?;
            assert_eq!(c.counters(ids[0])?.pending.bytes, 0);
            assert_eq!(c.counters(ids[0])?.completed.bytes, 4096);
            assert_eq!(c.used(Kind::ActiveJournalBytes)?, 4046);
            assert_eq!(c.pending(Kind::ActiveJournalBytes)?, 0);
            assert!(matches!(
                c.begin_append(frame, FrameBudget::new(132, 1)?, Tick(4)),
                Err(Error::Arithmetic(super::super::Error::Inconsistent(
                    "physical byte underflow"
                )))
            ));
            let part = c.part(g, 0)?;
            assert!(c
                .extend(
                    part,
                    IoPlan {
                        logical: [132, 1, 0, 0],
                        physical: growth(4096, 4096, 0)
                    },
                    &mut samples(c, ids, Tick(4)),
                    Tick(4)
                )
                .is_err());
            c.cancel(g.lease())?;
            assert_eq!(c.used(Kind::ActiveJournalBytes)?, 4046);
            // Old EOF can have a free trailing block; rollover cannot inherit it.
            assert_eq!(
                super::super::space::file_growth(4097, 4097 + 4046, 4096)?,
                0
            );
            let g = grant(c, ids, &[], Some(actual))?;
            assert_eq!(c.counters(ids[0])?.pending.bytes, 4096);
            c.cancel(g.lease())?;
            Ok(())
        })
    }
    #[test]
    fn uncertain_append_pins_capacity_but_previously_admitted_raw_io_can_finish() -> TestResult {
        let p = plan()?;
        with_coordinator(&p, 4096, false, |c, ids| {
            initialize(c, ids)?;
            let g = grant(
                c,
                ids,
                &[request(ids[0], 4096, 100, 4096, 1)],
                Some(FrameBudget::new(1000, 1)?),
            )?;
            let part = c.part(g, 0)?;
            let mut raw = c.begin_io(
                part,
                IoPlan {
                    logical: [100, 0, 0, 0],
                    physical: growth(4096, 4096, 1),
                },
                Tick(2),
            )?;
            let mut append = c.begin_append(
                g.frame().ok_or("frame")?,
                FrameBudget::new(500, 1)?,
                Tick(2),
            )?;
            let before = c.counters(ids[0])?;
            c.complete_append(&mut append, FrameResult::Uncertain)?;
            assert_eq!(c.state(), State::Stopped);
            assert_eq!(c.counters(ids[0])?, before);
            assert!(c.cancel(g.lease()).is_err());
            assert!(c
                .begin_io(
                    part,
                    IoPlan {
                        logical: [1, 0, 0, 0],
                        physical: growth(4096, 0, 0)
                    },
                    Tick(3)
                )
                .is_err());
            c.complete_io(&mut raw, IoResult::Uncertain)?;
            assert_eq!(
                c.counters(ids[0])?.completed,
                Growth {
                    bytes: 4096,
                    inodes: 1
                }
            );
            assert_eq!(
                c.counters(ids[0])?.pending,
                Growth {
                    bytes: 4096,
                    inodes: 0
                }
            );
            assert_eq!(c.pending(Kind::ActiveJournalBytes)?, 1000);
            assert!(c
                .complete_append(&mut append, FrameResult::NotWritten)
                .is_err());
            Ok(())
        })
    }
    #[test]
    fn foreign_effect_and_stale_sample_cannot_change_live_records() -> TestResult {
        let p = plan()?;
        with_coordinator(&p, 4096, true, |a, ia| {
            initialize(a, ia)?;
            let g = grant(a, ia, &[request(ia[2], 4096, 10, 4096, 1)], None)?;
            let mut ticket = a.begin_io(
                a.part(g, 0)?,
                IoPlan {
                    logical: [10, 0, 0, 0],
                    physical: growth(4096, 4096, 1),
                },
                Tick(2),
            )?;
            with_coordinator(&p, 4096, false, |b, ib| {
                initialize(b, ib)?;
                let before = b.counters(ib[0])?;
                assert!(b.complete_io(&mut ticket, IoResult::Uncertain).is_err());
                assert_eq!(b.counters(ib[0])?, before);
                Ok(())
            })?;
            a.complete_io(&mut ticket, IoResult::Uncertain)?;
            a.cancel(g.lease())?;
            let mut input = samples(a, ia, Tick(3));
            a.filesystems.unregister(ia[2])?;
            assert_eq!(
                a.grant(
                    &[request(ia[2], 4096, 10, 4096, 1)],
                    None,
                    deadline(),
                    &mut input,
                    Tick(3)
                ),
                Err(Error::Filesystem(filesystems::Error::Stale))
            );
            assert!(input.iter().all(Option::is_none));
            Ok(())
        })
    }
    #[test]
    fn all_fixed_cells_hold_physical_bindings_without_growing_backing_storage() -> TestResult {
        assert!(std::mem::size_of::<logical::Cell>() + std::mem::size_of::<SlotState>() <= 128);
        let p = plan()?;
        with_coordinator(&p, 4096, false, |c, ids| {
            initialize(c, ids)?;
            let mut grants = Vec::new();
            for _ in 0..8 {
                grants.push(grant(c, ids, &[request(ids[0], 4096, 1, 0, 0); 8], None)?);
            }
            assert_eq!(c.pending(Kind::BodyBytes)?, 64);
            let before = c.counters(ids[0])?;
            assert_eq!(
                grant(c, ids, &[request(ids[0], 4096, 1, 0, 0)], None),
                Err(Error::Writer(writer::Error::Logical(logical::Error::Full)))
            );
            assert_eq!(c.counters(ids[0])?, before);
            for g in grants {
                c.cancel(g.lease())?;
            }
            assert_eq!(c.pending(Kind::BodyBytes)?, 0);
            Ok(())
        })
    }
}

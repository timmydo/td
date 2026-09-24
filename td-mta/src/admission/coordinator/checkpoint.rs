//! Conditional checkpoint accounting; M05/M08 own file proofs and publication.
use super::super::{logical, space::Growth};
use super::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct BuildId {
    filesystem: FilesystemId,
    sequence: u64,
}
#[derive(Clone, Copy)]
pub(super) struct Build {
    id: BuildId,
    deadline: Deadline,
    need: CheckpointNeed,
    original: [RoundedGrowth; 2],
    remaining: [RoundedGrowth; 2],
    logical: u64,
    io_sequence: u64,
    busy: Option<u64>,
    failed: bool,
}
#[derive(Debug)]
#[must_use = "retain the checkpoint token; losing it keeps admission closed"]
pub struct BuildToken {
    id: Option<BuildId>,
}
#[derive(Clone, Copy, Debug)]
pub struct BuildWrite {
    pub generation_bytes: u64,
    pub generation: RoundedGrowth,
    pub journal: RoundedGrowth,
}
#[derive(Clone, Copy, Debug)]
pub enum BuildResult {
    Proven(BuildWrite),
    Uncertain,
}
#[derive(Debug)]
#[must_use = "complete checkpoint I/O before selection or abort"]
pub struct BuildIoTicket {
    id: BuildId,
    sequence: Option<u64>,
    planned: BuildWrite,
}
impl Coordinator<'_> {
    /// The caller first holds the real writer/view barrier and proves pin
    /// eligibility. This acquires capacity, not publication authority.
    pub fn begin_build(&mut self, enclosing: Deadline, now: Tick) -> Result<BuildToken, Error> {
        self.ready()?;
        if self.build.is_some() {
            return Err(Error::Invalid);
        }
        let milliseconds = super::super::mul(
            self.writer.plan().work().checkpoint_seconds,
            1000,
            "checkpoint deadline",
        )?;
        let deadline =
            enclosing.min(Deadline::after(now, milliseconds).map_err(|_| Error::Invalid)?);
        if deadline.expired(now) {
            return Err(Error::Writer(writer::Error::Logical(
                logical::Error::Expired,
            )));
        }
        let sequence = self.build_sequence.checked_add(1).ok_or(Error::Invalid)?;
        let id = BuildId {
            filesystem: self.generation_fs,
            sequence,
        };
        let need = self.writer.build_need()?;
        let generation =
            generation_growth(need, self.filesystems.allocation_unit(self.generation_fs)?)?;
        let journal = journal_growth(need, self.filesystems.allocation_unit(self.journal_fs)?)?;
        let build = Build {
            id,
            deadline,
            need,
            original: [generation, journal],
            remaining: [generation, journal],
            logical: need.generation_bytes(),
            io_sequence: 0,
            busy: None,
            failed: false,
        };
        let mut stage = self.filesystems.stage()?;
        // Shared locations deliberately accumulate in the same staged entry.
        stage.transfer_checkpoint(self.generation_fs, generation, false)?;
        stage.transfer_checkpoint(self.journal_fs, journal, false)?;
        self.writer.start_build()?;
        stage.publish();
        self.build_sequence = sequence;
        self.build = Some(build);
        Ok(BuildToken { id: Some(id) })
    }
    fn matching_build(&self, token: &BuildToken) -> Result<Build, Error> {
        let id = token.id.ok_or(Error::Invalid)?;
        self.build.filter(|b| b.id == id).ok_or(Error::Invalid)
    }
    pub fn build_remaining(&self, token: &BuildToken) -> Result<BuildWrite, Error> {
        let build = self.matching_build(token)?;
        Ok(BuildWrite {
            generation_bytes: build.logical,
            generation: *build.remaining.first().ok_or(Error::Invalid)?,
            journal: *build.remaining.get(1).ok_or(Error::Invalid)?,
        })
    }
    pub fn begin_build_io(
        &mut self,
        token: &BuildToken,
        plan: BuildWrite,
        now: Tick,
    ) -> Result<BuildIoTicket, Error> {
        let mut build = self.matching_build(token)?;
        if self.writer.phase() != writer::Phase::Barrier || build.failed {
            return Err(Error::Closed);
        }
        if build.busy.is_some() {
            return Err(Error::Writer(writer::Error::Busy));
        }
        if build.deadline.expired(now) {
            return Err(Error::Writer(writer::Error::Logical(
                logical::Error::Expired,
            )));
        }
        if plan.generation_bytes > build.logical {
            return Err(Error::Invalid);
        }
        for (remaining, proposed) in build.remaining.iter().zip([plan.generation, plan.journal]) {
            remaining.checked_sub(proposed)?;
        }
        let sequence = build.io_sequence.checked_add(1).ok_or(Error::Invalid)?;
        build.io_sequence = sequence;
        build.busy = Some(sequence);
        self.build = Some(build);
        Ok(BuildIoTicket {
            id: build.id,
            sequence: Some(sequence),
            planned: plan,
        })
    }
    /// Completion is accepted after deadline or stop for already admitted I/O.
    /// Invalid proof retains the live ticket and all pending ownership.
    pub fn complete_build_io(
        &mut self,
        ticket: &mut BuildIoTicket,
        outcome: BuildResult,
    ) -> Result<(), Error> {
        let sequence = ticket.sequence.ok_or(Error::Invalid)?;
        let mut build = self
            .build
            .filter(|b| b.id == ticket.id && b.busy == Some(sequence))
            .ok_or(Error::Invalid)?;
        let (actual, failed) = match outcome {
            BuildResult::Proven(actual) => (actual, false),
            BuildResult::Uncertain => (ticket.planned, true),
        };
        if actual.generation_bytes > ticket.planned.generation_bytes {
            return Err(Error::Invalid);
        }
        build.logical = build
            .logical
            .checked_sub(actual.generation_bytes)
            .ok_or(Error::Invalid)?;
        let mut stage = self.filesystems.stage()?;
        for (((remaining, planned), actual), filesystem) in build
            .remaining
            .iter_mut()
            .zip([ticket.planned.generation, ticket.planned.journal])
            .zip([actual.generation, actual.journal])
            .zip([self.generation_fs, self.journal_fs])
        {
            planned.checked_sub(actual)?;
            stage.finish(
                Physical {
                    filesystem,
                    remaining: *remaining,
                },
                actual,
                false,
            )?;
            *remaining = remaining.checked_sub(actual)?;
        }
        self.writer.complete_build_io(actual.generation_bytes)?;
        stage.publish();
        build.busy = None;
        build.failed |= failed;
        self.build = Some(build);
        ticket.sequence = None;
        Ok(())
    }
    /// Before selection only: all checkpoint workers must have finished. An
    /// affected build leaves orphan charges and requires fresh protection.
    pub fn abandon_build(&mut self, token: &mut BuildToken) -> Result<(), Error> {
        let build = self.matching_build(token)?;
        if build.busy.is_some() {
            return Err(Error::Writer(writer::Error::Busy));
        }
        let untouched = build.remaining == build.original
            && build.logical == build.need.generation_bytes()
            && !build.failed;
        let mut stage = self.filesystems.stage()?;
        for (remaining, filesystem) in build
            .remaining
            .into_iter()
            .zip([self.generation_fs, self.journal_fs])
        {
            if untouched {
                stage.transfer_checkpoint(filesystem, remaining, true)?;
            } else {
                stage.finish(
                    Physical {
                        filesystem,
                        remaining,
                    },
                    remaining.zeroed(),
                    true,
                )?;
            }
        }
        if !untouched {
            stage.invalidate_probes()?;
        }
        self.writer.abandon_build(build.logical, untouched)?;
        stage.publish();
        self.build = None;
        token.id = None;
        Ok(())
    }
    /// M08 reports a durably selected, validated generation and journal. A
    /// matching but invalid report stops admission and retains the build.
    pub fn select_build(
        &mut self,
        token: &mut BuildToken,
        selected_tables: u64,
    ) -> Result<(), Error> {
        let build = self.matching_build(token)?;
        // Any error after a trusted durable-selection report requires recovery.
        if let Err(error) = self.finish_selection(build, selected_tables) {
            self.writer.stop();
            return Err(error);
        }
        self.build = None;
        token.id = None;
        Ok(())
    }
    fn finish_selection(&mut self, build: Build, selected_tables: u64) -> Result<(), Error> {
        if build.busy.is_some() || build.failed {
            return Err(Error::Invalid);
        }
        let written = build
            .need
            .generation_bytes()
            .checked_sub(build.logical)
            .ok_or(Error::Invalid)?;
        let generation_written = build
            .original
            .first()
            .ok_or(Error::Invalid)?
            .checked_sub(*build.remaining.first().ok_or(Error::Invalid)?)?
            .amount();
        let generation_original = build.original.first().ok_or(Error::Invalid)?;
        if selected_tables > written
            || generation_written.inodes != generation_original.amount().inodes
            || generation_written.bytes
                < super::super::space::rounded_bytes(
                    written,
                    generation_original.allocation_unit(),
                )?
            || build.remaining.get(1).ok_or(Error::Invalid)?.amount() != Growth::default()
        {
            return Err(Error::Invalid);
        }
        let mut stage = self.filesystems.stage()?;
        for (remaining, filesystem) in build
            .remaining
            .into_iter()
            .zip([self.generation_fs, self.journal_fs])
        {
            stage.finish(
                Physical {
                    filesystem,
                    remaining,
                },
                remaining.zeroed(),
                true,
            )?;
        }
        stage.invalidate_probes()?;
        self.writer.select_build(
            selected_tables,
            build.need.generation_bytes(),
            build.logical,
        )?;
        stage.publish();
        Ok(())
    }
    /// A torn/uncertain CURRENT transition cannot use the abort-before-selection
    /// path. Existing admitted effects may still report their completed charge.
    pub fn uncertain_selection(&mut self, token: &BuildToken) -> Result<(), Error> {
        self.matching_build(token)?;
        self.writer.stop();
        Ok(())
    }
    pub fn reopen_after_checkpoint(
        &mut self,
        samples: &mut Samples,
        now: Tick,
    ) -> Result<(), Error> {
        let samples = take_samples(samples);
        if self.build.is_some() || self.writer.phase() != writer::Phase::AwaitingSpace {
            return Err(Error::Closed);
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
        self.writer.reopen()?;
        stage.publish();
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::indexing_slicing, clippy::unwrap_used)]
mod tests {
    use super::super::tests::*;
    use super::*;
    use crate::admission::{
        quota::Usage,
        space::{Inodes, Sample},
    };

    fn total(c: Counters) -> Growth {
        Growth {
            bytes: c.pending.bytes + c.checkpoint.bytes,
            inodes: c.pending.inodes + c.checkpoint.inodes,
        }
    }
    fn zero(unit: u64) -> BuildWrite {
        BuildWrite {
            generation_bytes: 0,
            generation: growth(unit, 0, 0),
            journal: growth(unit, 0, 0),
        }
    }
    fn finish(c: &mut Coordinator<'_>, token: &BuildToken, unit: u64) -> TestResult {
        let mut files = [FileGrowth {
            old_length: 0,
            new_length: 112,
        }; 13];
        files[12].new_length = 2752; // Sum 4096; eleven table headers fit 1232.
        let written = BuildWrite {
            generation_bytes: 4096,
            generation: RoundedGrowth::from_files(unit, &files, 14)?,
            journal: growth(unit, 96, 1),
        };
        let mut io = c.begin_build_io(token, written, Tick(1))?;
        c.complete_build_io(&mut io, BuildResult::Proven(written))?;
        Ok(())
    }
    #[test]
    fn checkpoint_transfers_capacity_at_full_client_cell_occupancy_and_aborts_unstarted(
    ) -> TestResult {
        assert!(std::mem::size_of::<Option<Build>>() + std::mem::size_of::<u64>() <= 512);
        let p = plan()?;
        with_coordinator(&p, 4096, false, |c, ids| {
            initialize(c, ids)?;
            let mut grants = Vec::new();
            for _ in 0..8 {
                grants.push(grant(c, ids, &[request(ids[0], 4096, 1, 0, 0); 8], None)?);
            }
            let before = c.counters(ids[0])?;
            let mut token = c.begin_build(deadline(), Tick(1))?;
            assert_eq!(c.state(), State::Closed);
            assert_eq!(c.counters(ids[0])?.pending, before.checkpoint);
            assert_eq!(c.counters(ids[0])?.checkpoint, Growth::default());
            assert_eq!(c.pending(Kind::BodyBytes)?, 64);
            assert_eq!(c.pending(Kind::CheckpointBytes)?, 1049808);
            c.abandon_build(&mut token)?;
            assert_eq!(c.state(), State::Ready);
            assert_eq!(c.counters(ids[0])?, before);
            assert_eq!(c.pending(Kind::CheckpointBytes)?, 0);
            assert_eq!(c.abandon_build(&mut token), Err(Error::Invalid));
            for g in grants {
                c.cancel(g.lease())?;
            }
            Ok(())
        })
    }
    #[test]
    fn checkpoint_quota_overlap_refusal_precedes_physical_publication_and_writer_close(
    ) -> TestResult {
        let p = plan()?;
        let mut used = Usage::default();
        used.add(Kind::LiveMetadataBytes, 1232)?;
        let retained = p.disk().checkpoint_bytes - 1049807;
        used.add(Kind::CheckpointBytes, retained)?;
        with_recovered(&p, 4096, true, used, |c, ids| {
            initialize(c, ids)?;
            let before = [c.counters(ids[0])?, c.counters(ids[1])?];
            assert!(matches!(
                c.begin_build(deadline(), Tick(1)),
                Err(Error::Writer(writer::Error::Logical(
                    logical::Error::Quota(Kind::CheckpointBytes)
                )))
            ));
            assert_eq!(c.state(), State::Ready);
            assert_eq!([c.counters(ids[0])?, c.counters(ids[1])?], before);
            assert_eq!(c.pending(Kind::CheckpointBytes)?, 0);
            assert_eq!(c.used(Kind::CheckpointBytes)?, retained);
            assert!(c.build.is_none());
            Ok(())
        })
    }
    #[test]
    fn closed_build_allows_admitted_raw_effects_and_cancel_but_no_new_mutations() -> TestResult {
        let p = plan()?;
        with_coordinator(&p, 4096, true, |c, ids| {
            initialize(c, ids)?;
            let g = grant(
                c,
                ids,
                &[request(ids[2], 4096, 100, 8192, 2)],
                Some(FrameBudget::new(4046, 1)?),
            )?;
            let part = c.part(g, 0)?;
            let before = [c.counters(ids[0])?, c.counters(ids[1])?];
            let mut build = c.begin_build(deadline(), Tick(1))?;
            for (id, old) in ids[..2].iter().zip(before) {
                assert_eq!(total(c.counters(*id)?), total(old));
            }
            assert_eq!(c.counters(ids[0])?.completed, before[0].completed);
            assert_eq!(
                c.grant(
                    &[request(ids[2], 4096, 1, 1, 1)],
                    None,
                    deadline(),
                    &mut samples(c, ids, Tick(1)),
                    Tick(1)
                ),
                Err(Error::Closed)
            );
            assert_eq!(
                c.extend(
                    part,
                    IoPlan {
                        logical: [1, 0, 0, 0],
                        physical: growth(4096, 1, 0)
                    },
                    &mut samples(c, ids, Tick(1)),
                    Tick(1)
                ),
                Err(Error::Closed)
            );
            assert!(matches!(
                c.begin_append(g.frame().unwrap(), FrameBudget::new(132, 1)?, Tick(1)),
                Err(Error::Closed)
            ));
            let mut io = c.begin_io(
                part,
                IoPlan {
                    logical: [50, 0, 0, 0],
                    physical: growth(4096, 4096, 1),
                },
                Tick(1),
            )?;
            c.complete_io(&mut io, IoResult::Uncertain)?;
            c.cancel(g.lease())?;
            assert_eq!(c.used(Kind::BodyBytes)?, 50);
            assert_eq!(
                c.counters(ids[2])?.completed,
                Growth {
                    bytes: 4096,
                    inodes: 1
                }
            );
            assert_eq!(c.counters(ids[2])?.pending, Growth::default());
            c.abandon_build(&mut build)?;
            assert_eq!(c.state(), State::Ready);
            Ok(())
        })
    }
    #[test]
    fn selection_preserves_frame_leases_and_needs_post_fence_space_on_all_metadata_filesystems(
    ) -> TestResult {
        let p = plan()?;
        for distinct in [false, true] {
            for unit in [4096, 4194304] {
                let mut used = Usage::default();
                used.add(Kind::LiveMetadataBytes, 1232)?;
                used.add(Kind::CheckpointBytes, 512)?;
                used.add(Kind::ActiveJournalBytes, 1000)?;
                used.add(Kind::ActiveJournalOperations, 3)?;
                with_recovered(&p, unit, distinct, used, |c, ids| {
                    initialize(c, ids)?;
                    let metadata = &ids[..ids.len().min(2)];
                    let g = grant(c, metadata, &[], Some(FrameBudget::new(4046, 1)?))?;
                    let frame = g.frame().unwrap();
                    let part = c.part(g, 0)?;
                    let frame_growth = c.physical_remaining(part)?;
                    let before: Vec<_> =
                        metadata.iter().map(|id| c.counters(*id).unwrap()).collect();
                    let mut build = c.begin_build(deadline(), Tick(1))?;
                    for (id, old) in metadata.iter().zip(before) {
                        assert_eq!(total(c.counters(*id)?), total(old));
                    }
                    finish(c, &build, unit)?;
                    let mut old = samples(c, metadata, Tick(1));
                    let unfinished = c.begin_probe(ids[0], deadline(), Tick(1))?;
                    c.select_build(&mut build, 1232)?;
                    assert_eq!(c.state(), State::Closed);
                    assert_eq!(c.used(Kind::CheckpointBytes)?, 4608);
                    assert_eq!(c.pending(Kind::CheckpointBytes)?, 0);
                    assert_eq!(c.used(Kind::ClosedJournalBytes)?, 1096);
                    assert_eq!(c.used(Kind::ClosedJournalSegments)?, 1);
                    assert_eq!(c.used(Kind::ActiveJournalBytes)?, 0);
                    assert_eq!(c.used(Kind::ActiveJournalOperations)?, 0);
                    assert_eq!(c.pending(Kind::ActiveJournalBytes)?, 4046);
                    assert_eq!(c.physical_remaining(part)?, frame_growth);
                    assert_eq!(
                        c.reopen_after_checkpoint(&mut old, Tick(1)),
                        Err(Error::Filesystem(filesystems::Error::Superseded))
                    );
                    assert!(old.iter().all(Option::is_none));
                    let mut old = Some(unfinished.complete(Ok(Sample {
                        available_bytes: u64::MAX,
                        inodes: Inodes::Unsupported,
                        allocation_unit: unit,
                    }))?);
                    assert!(matches!(
                        c.consume_observation(ids[0], &mut old, Tick(1)),
                        Err(Error::Filesystem(filesystems::Error::Superseded))
                    ));
                    let before: Vec<_> =
                        metadata.iter().map(|id| c.counters(*id).unwrap()).collect();
                    let mut low = samples(c, metadata, Tick(1));
                    low[0] = Some(sample(c, ids[0], 0, Inodes::Unsupported, Tick(1)));
                    assert_eq!(
                        c.reopen_after_checkpoint(&mut low, Tick(1)),
                        Err(Error::Filesystem(filesystems::Error::Space(
                            super::super::super::space::Refusal::Bytes
                        )))
                    );
                    assert_eq!(c.state(), State::Closed);
                    for (id, old) in metadata.iter().zip(before) {
                        assert_eq!(c.counters(*id)?, old);
                    }
                    c.reopen_after_checkpoint(&mut samples(c, metadata, Tick(1)), Tick(1))?;
                    assert_eq!(c.state(), State::Ready);
                    let mut append = c.begin_append(frame, FrameBudget::new(500, 1)?, Tick(1))?;
                    c.complete_append(&mut append, FrameResult::Synced(growth(unit, 0, 0)))?;
                    assert_eq!(c.used(Kind::ActiveJournalBytes)?, 500);
                    c.cancel(g.lease())?;
                    Ok(())
                })?;
            }
        }
        Ok(())
    }
    #[test]
    fn partial_or_uncertain_builds_retain_orphans_and_cannot_reopen_on_old_samples() -> TestResult {
        let p = plan()?;
        for uncertain in [false, true] {
            with_coordinator(&p, 4096, false, |c, ids| {
                initialize(c, ids)?;
                let mut token = c.begin_build(deadline(), Tick(1))?;
                let partial = BuildWrite {
                    generation_bytes: 200,
                    generation: growth(4096, 4096, 1),
                    journal: growth(4096, 0, 0),
                };
                let mut io = c.begin_build_io(&token, partial, Tick(1))?;
                assert_eq!(
                    c.abandon_build(&mut token),
                    Err(Error::Writer(writer::Error::Busy))
                );
                c.complete_build_io(
                    &mut io,
                    if uncertain {
                        BuildResult::Uncertain
                    } else {
                        BuildResult::Proven(partial)
                    },
                )?;
                let mut old = samples(c, ids, Tick(1));
                c.abandon_build(&mut token)?;
                assert_eq!(c.state(), State::Closed);
                assert_eq!(c.used(Kind::CheckpointBytes)?, 200);
                assert_eq!(c.pending(Kind::CheckpointBytes)?, 0);
                assert_eq!(
                    c.counters(ids[0])?.completed,
                    Growth {
                        bytes: 4096,
                        inodes: 1
                    }
                );
                assert_eq!(c.counters(ids[0])?.pending, Growth::default());
                assert_eq!(c.used(Kind::ClosedJournalBytes)?, 0);
                assert_eq!(
                    c.reopen_after_checkpoint(&mut old, Tick(1)),
                    Err(Error::Filesystem(filesystems::Error::Superseded))
                );
                c.reopen_after_checkpoint(&mut samples(c, ids, Tick(1)), Tick(1))?;
                assert_eq!(c.state(), State::Ready);
                assert_eq!(c.used(Kind::CheckpointBytes)?, 200);
                Ok(())
            })?;
        }
        Ok(())
    }
    #[test]
    fn invalid_build_completion_preserves_both_ledgers_and_ticket_until_valid_result() -> TestResult
    {
        let p = plan()?;
        with_coordinator(&p, 4096, false, |c, ids| {
            initialize(c, ids)?;
            let mut token = c.begin_build(deadline(), Tick(1))?;
            let written = BuildWrite {
                generation_bytes: 200,
                generation: growth(4096, 4096, 1),
                journal: growth(4096, 0, 0),
            };
            let mut io = c.begin_build_io(&token, written, Tick(1))?;
            let before = c.counters(ids[0])?;
            let pending = c.pending(Kind::CheckpointBytes)?;
            assert_eq!(
                c.complete_build_io(
                    &mut io,
                    BuildResult::Proven(BuildWrite {
                        generation_bytes: 201,
                        ..written
                    })
                ),
                Err(Error::Invalid)
            );
            assert!(c
                .complete_build_io(
                    &mut io,
                    BuildResult::Proven(BuildWrite {
                        generation: growth(4096, 8192, 1),
                        ..written
                    })
                )
                .is_err());
            // The generation projection succeeds before this invalid header
            // amount refuses; neither quota nor physical projection is published.
            assert_eq!(
                c.complete_build_io(
                    &mut io,
                    BuildResult::Proven(BuildWrite {
                        journal: growth(4096, 4096, 1),
                        ..written
                    })
                ),
                Err(Error::Arithmetic(crate::admission::Error::Inconsistent(
                    "physical byte underflow"
                )))
            );
            assert_eq!(c.counters(ids[0])?, before);
            assert_eq!(c.pending(Kind::CheckpointBytes)?, pending);
            assert!(matches!(
                c.begin_build_io(&token, zero(4096), Tick(1)),
                Err(Error::Writer(writer::Error::Busy))
            ));
            c.complete_build_io(&mut io, BuildResult::Proven(written))?;
            let after = c.counters(ids[0])?;
            assert_eq!(
                c.complete_build_io(&mut io, BuildResult::Uncertain),
                Err(Error::Invalid)
            );
            assert_eq!(c.counters(ids[0])?, after);
            c.abandon_build(&mut token)?;
            Ok(())
        })
    }
    #[test]
    fn invalid_or_uncertain_durable_selection_stops_but_inflight_accounting_can_finish(
    ) -> TestResult {
        let p = plan()?;
        for uncertain in [false, true] {
            with_coordinator(&p, 4096, false, |c, ids| {
                initialize(c, ids)?;
                let mut token = c.begin_build(deadline(), Tick(1))?;
                let partial = BuildWrite {
                    generation_bytes: 200,
                    generation: growth(4096, 4096, 1),
                    journal: growth(4096, 0, 0),
                };
                let mut io = c.begin_build_io(&token, partial, Tick(1))?;
                if uncertain {
                    c.uncertain_selection(&token)?;
                } else {
                    assert_eq!(c.select_build(&mut token, 1232), Err(Error::Invalid));
                }
                assert_eq!(c.state(), State::Stopped);
                c.complete_build_io(&mut io, BuildResult::Proven(partial))?;
                assert_eq!(c.used(Kind::CheckpointBytes)?, 200);
                assert_eq!(
                    c.counters(ids[0])?.completed,
                    Growth {
                        bytes: 4096,
                        inodes: 1
                    }
                );
                assert!(c.abandon_build(&mut token).is_err());
                let mut observations = samples(c, ids, Tick(1));
                assert_eq!(
                    c.reopen_after_checkpoint(&mut observations, Tick(1)),
                    Err(Error::Closed)
                );
                assert!(observations.iter().all(Option::is_none));
                Ok(())
            })?;
        }
        Ok(())
    }
    #[test]
    fn foreign_and_lost_build_tokens_do_not_reopen_or_stop_another_coordinator() -> TestResult {
        let p = plan()?;
        with_coordinator(&p, 4096, false, |a, ia| {
            initialize(a, ia)?;
            let mut token = a.begin_build(deadline(), Tick(1))?;
            let mut io = a.begin_build_io(&token, zero(4096), Tick(1))?;
            with_coordinator(&p, 4096, false, |b, ib| {
                initialize(b, ib)?;
                let _lost = b.begin_build(deadline(), Tick(1))?;
                let _lost_io = b.begin_build_io(&_lost, zero(4096), Tick(1))?;
                assert_eq!(b.select_build(&mut token, 1232), Err(Error::Invalid));
                assert_eq!(b.uncertain_selection(&token), Err(Error::Invalid));
                assert_eq!(
                    b.complete_build_io(&mut io, BuildResult::Uncertain),
                    Err(Error::Invalid)
                );
                assert_eq!(b.state(), State::Closed);
                assert!(b.begin_build(deadline(), Tick(1)).is_err());
                Ok(())
            })?;
            a.complete_build_io(&mut io, BuildResult::Proven(zero(4096)))?;
            a.abandon_build(&mut token)?;
            assert_eq!(a.state(), State::Ready);
            Ok(())
        })
    }
    #[test]
    fn build_deadline_and_checked_attempt_sequence_refuse_without_releasing_capacity() -> TestResult
    {
        let p = plan()?;
        with_coordinator(&p, 4096, false, |c, ids| {
            initialize(c, ids)?;
            c.build_sequence = u64::MAX;
            let before = c.counters(ids[0])?;
            assert!(matches!(
                c.begin_build(deadline(), Tick(1)),
                Err(Error::Invalid)
            ));
            assert_eq!(c.counters(ids[0])?, before);
            assert_eq!(c.state(), State::Ready);
            c.build_sequence = 0;
            let mut token = c.begin_build(deadline(), Tick(1))?;
            let mut io = c.begin_build_io(&token, zero(4096), Tick(1))?;
            c.complete_build_io(&mut io, BuildResult::Proven(zero(4096)))?;
            let expired = Tick(1 + p.work().checkpoint_seconds * 1000);
            assert!(matches!(
                c.begin_build_io(&token, zero(4096), expired),
                Err(Error::Writer(writer::Error::Logical(
                    logical::Error::Expired
                )))
            ));
            c.abandon_build(&mut token)?;
            assert_eq!(c.counters(ids[0])?, before);
            Ok(())
        })
    }
    #[test]
    fn selection_report_cannot_use_unwritten_headers_or_uncharged_generation_output() -> TestResult
    {
        let p = plan()?;
        for missing_header in [false, true] {
            with_coordinator(&p, 4096, false, |c, ids| {
                initialize(c, ids)?;
                let mut token = c.begin_build(deadline(), Tick(1))?;
                if missing_header {
                    let mut files = [FileGrowth {
                        old_length: 0,
                        new_length: 112,
                    }; 13];
                    files[12].new_length = 2752;
                    let gen = BuildWrite {
                        generation_bytes: 4096,
                        generation: RoundedGrowth::from_files(4096, &files, 14)?,
                        journal: growth(4096, 0, 0),
                    };
                    let mut io = c.begin_build_io(&token, gen, Tick(1))?;
                    c.complete_build_io(&mut io, BuildResult::Proven(gen))?;
                } else {
                    finish(c, &token, 4096)?;
                }
                let before = c.counters(ids[0])?;
                let pending = c.pending(Kind::CheckpointBytes)?;
                assert_eq!(
                    c.select_build(&mut token, if missing_header { 1232 } else { 5000 }),
                    Err(Error::Invalid)
                );
                assert_eq!(c.state(), State::Stopped);
                assert_eq!(c.counters(ids[0])?, before);
                assert_eq!(c.pending(Kind::CheckpointBytes)?, pending);
                assert_eq!(c.used(Kind::ClosedJournalSegments)?, 0);
                assert_eq!(c.used(Kind::CheckpointBytes)?, 4096);
                Ok(())
            })?;
        }
        Ok(())
    }
    #[test]
    fn selection_independently_requires_inodes_physical_bytes_and_certain_output() -> TestResult {
        let p = plan()?;
        for invalid in 0..3 {
            with_coordinator(&p, 4096, false, |c, ids| {
                initialize(c, ids)?;
                let mut token = c.begin_build(deadline(), Tick(1))?;
                let written = BuildWrite {
                    generation_bytes: 4096,
                    generation: growth(
                        4096,
                        if invalid == 1 { 0 } else { 4096 },
                        if invalid == 0 { 13 } else { 14 },
                    ),
                    journal: growth(4096, 96, 1),
                };
                let mut io = c.begin_build_io(&token, written, Tick(1))?;
                c.complete_build_io(
                    &mut io,
                    if invalid == 2 {
                        BuildResult::Uncertain
                    } else {
                        BuildResult::Proven(written)
                    },
                )?;
                let before = c.counters(ids[0])?;
                let pending = c.pending(Kind::CheckpointBytes)?;
                assert_eq!(c.select_build(&mut token, 1232), Err(Error::Invalid));
                assert_eq!(c.state(), State::Stopped);
                assert_eq!(c.counters(ids[0])?, before);
                assert_eq!(c.pending(Kind::CheckpointBytes)?, pending);
                assert_eq!(c.used(Kind::ClosedJournalSegments)?, 0);
                assert_eq!(c.used(Kind::CheckpointBytes)?, 4096);
                Ok(())
            })?;
        }
        Ok(())
    }
    #[test]
    fn logical_output_without_physical_growth_still_requires_fenced_abort() -> TestResult {
        let p = plan()?;
        with_coordinator(&p, 4096, false, |c, ids| {
            initialize(c, ids)?;
            let mut old_samples = samples(c, ids, Tick(1));
            let mut token = c.begin_build(deadline(), Tick(1))?;
            let written = BuildWrite {
                generation_bytes: 200,
                ..zero(4096)
            };
            let mut io = c.begin_build_io(&token, written, Tick(1))?;
            c.complete_build_io(&mut io, BuildResult::Proven(written))?;
            c.abandon_build(&mut token)?;
            assert_eq!(c.state(), State::Closed);
            assert_eq!(c.writer.phase(), writer::Phase::AwaitingSpace);
            assert_eq!(c.used(Kind::CheckpointBytes)?, 200);
            assert_eq!(c.pending(Kind::CheckpointBytes)?, 0);
            assert_eq!(
                c.reopen_after_checkpoint(&mut old_samples, Tick(1)),
                Err(Error::Filesystem(filesystems::Error::Superseded))
            );
            c.reopen_after_checkpoint(&mut samples(c, ids, Tick(1)), Tick(1))?;
            assert_eq!(c.state(), State::Ready);
            Ok(())
        })
    }
}

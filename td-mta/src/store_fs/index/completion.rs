//! Commit outcome and original-scope file observations under one writer fence.
use super::*;
use std::cell::Cell;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum CommitPhase {
    BeforeSql,
    SqlStarted,
    SequenceConflict,
}
pub(super) struct CommitWork<'a> {
    pub scratch: &'a mut [u8],
    pub phase: &'a Cell<CommitPhase>,
}

/// Observations of this attempt, not authority to alter an arbitrary quota ledger.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommitFileUsage {
    /// No storage-changing SQL began; this attempt changed neither file extent.
    Unchanged,
    /// Validated current lengths, including reusable pages and retained WAL tails.
    Measured(StoreFileUsage),
    /// Work may have changed extents; never substitute zero for this refusal.
    Unavailable(ports::Error),
}

/// Retains a known durable result even when subsequent file accounting refuses.
#[derive(Debug)]
#[must_use = "reconcile file usage without discarding the durable outcome"]
pub struct CommitCompletion {
    outcome: Result<Sequence, CommitError>,
    files: CommitFileUsage,
    sequence_conflict: bool,
    writer_stopped: bool,
}
impl CommitCompletion {
    pub fn outcome(&self) -> Result<Sequence, CommitError> {
        self.outcome
    }
    pub fn files(&self) -> CommitFileUsage {
        self.files
    }
    /// A healthy, accounted endpoint refusal before body reads, not any Conflict.
    pub fn is_sequence_conflict(&self) -> bool {
        self.sequence_conflict
    }
    pub fn writer_stopped(&self) -> bool {
        self.writer_stopped
    }
    fn capture(
        outcome: Result<Sequence, CommitError>,
        phase: CommitPhase,
        writer: Option<&Writer>,
        root: &LockedRoot,
    ) -> Self {
        let writer_stopped = writer.map_or_else(
            || {
                matches!(
                    outcome,
                    Err(CommitError::Rejected(ports::Error::WriterStopped))
                )
            },
            |writer| writer.stopped,
        );
        let files = if phase == CommitPhase::BeforeSql {
            CommitFileUsage::Unchanged
        } else {
            match writer {
                Some(writer) => match StoreFileUsage::capture(root, &writer.native) {
                    Ok(files) => CommitFileUsage::Measured(files),
                    Err(error) => CommitFileUsage::Unavailable(error),
                },
                None => CommitFileUsage::Unavailable(ports::Error::WriterStopped),
            }
        };
        let sequence_conflict = phase == CommitPhase::SequenceConflict
            && outcome == Err(CommitError::Rejected(ports::Error::Conflict))
            && !writer_stopped
            && matches!(files, CommitFileUsage::Measured(_));
        Self {
            outcome,
            files,
            sequence_conflict,
            writer_stopped,
        }
    }
}

impl IndexStore<'_> {
    /// Commit and observe files before releasing the writer, without renewing work.
    /// Callers own authorization, admission, ledger association and reconciliation.
    pub fn commit_with_files<C: ports::Crypto>(
        &self,
        crypto: &C,
        request: CommitRequest,
        operations: &[Operation<'_>],
        sources: &mut [BlobSource<'_>],
    ) -> CommitCompletion {
        self.commit_operations_observed(
            crypto,
            request,
            Operations::Typed(operations),
            sources,
            |result, phase, writer| CommitCompletion::capture(result, phase, writer, self.root),
        )
    }
    /// Encoded commits share the same transaction, outcome and original file scope.
    pub fn commit_batch_with_files<C: ports::Crypto>(
        &self,
        crypto: &C,
        request: CommitRequest,
        batch: &crate::format::batch::Batch<'_, '_>,
        sources: &mut [BlobSource<'_>],
    ) -> CommitCompletion {
        self.commit_operations_observed(
            crypto,
            request,
            Operations::Encoded(batch),
            sources,
            |result, phase, writer| CommitCompletion::capture(result, phase, writer, self.root),
        )
    }
}

#[cfg(test)]
#[path = "completion_tests.rs"]
mod tests;

//! Checked cumulative counters for bounded historical owner audits.
use super::Error;
use crate::metadata_sweep::Limits;

#[derive(Clone, Copy)]
pub(crate) struct AuditBudget {
    pub(super) turns: usize,
    pub(super) input: usize,
    pub(super) output: usize,
    pub(super) frames: usize,
}
impl AuditBudget {
    pub(crate) fn new(metadata: Limits, blobs: u64, bytes: u64) -> Result<Self, Error> {
        // Reference pass: next plus two gets per row; mailbox and recipient
        // passes: one next per row. Parent walks have their own bound.
        // Setup, table-end probes and rollback fit the fixed reserve.
        // Bodies need one manifest and ceil(length / 512 KiB) batch reads;
        // one extra batch per blob covers rounding independently of totals.
        let turns = metadata
            .rows
            .checked_mul(5)
            .and_then(|n| n.checked_add(metadata.parent_reads))
            .and_then(|n| n.checked_add(blobs.checked_mul(2)?))
            .and_then(|n| n.checked_add(bytes.div_ceil(512 * 1024)))
            .and_then(|n| n.checked_add(64))
            .ok_or(Error::Invalid)?;
        let scaled = |factor| {
            turns
                .checked_mul(factor)
                .and_then(|n| usize::try_from(n).ok())
                .ok_or(Error::Invalid)
        };
        Ok(Self {
            turns: usize::try_from(turns).map_err(|_| Error::Invalid)?,
            input: scaled(1024 * 1024)?,
            output: scaled(64 * 1024)?,
            frames: scaled(1088)?,
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    #[test]
    fn audit_counters_cover_independent_body_rounding_and_reject_overflow() {
        let metadata = Limits {
            rows: 10,
            parent_reads: 20,
        };
        let empty = AuditBudget::new(metadata, 0, 0).unwrap();
        assert_eq!(empty.turns, 134);
        let small = AuditBudget::new(metadata, 816, 816).unwrap();
        assert_eq!(small.turns, 134 + 1632 + 1);
        let large = AuditBudget::new(metadata, 1, 1024 * 1024 * 1024).unwrap();
        assert_eq!(large.turns, 134 + 2 + 2048);
        assert_eq!(large.input, large.turns * 1024 * 1024);
        assert_eq!(large.output, large.turns * 64 * 1024);
        assert_eq!(large.frames, large.turns * 1088);
        for (rows, parents, blobs, bytes) in [
            (u64::MAX, 0, 0, 0),
            (0, u64::MAX, 0, 0),
            (0, 0, u64::MAX, 0),
            (0, 0, 0, u64::MAX),
        ] {
            assert!(AuditBudget::new(
                Limits {
                    rows,
                    parent_reads: parents
                },
                blobs,
                bytes
            )
            .is_err());
        }
    }
}

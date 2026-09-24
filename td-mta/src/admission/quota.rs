//! Logical quota state used only under the reservation coordinator.
use super::{add, Error, Plan};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum Kind {
    BodyBytes,
    BodyFiles,
    UploadBytes,
    QueueBytes,
    QueueSubmissions,
    LiveMetadataBytes,
    CheckpointBytes,
    ClosedJournalBytes,
    ClosedJournalSegments,
    ActiveJournalBytes,
    ActiveJournalOperations,
    SortBytes,
    ResponseBytes,
    CacheBytes,
    LogBytes,
    ColdBytes,
}
pub const COUNT: usize = 16;
pub const ALL: [Kind; COUNT] = [
    Kind::BodyBytes,
    Kind::BodyFiles,
    Kind::UploadBytes,
    Kind::QueueBytes,
    Kind::QueueSubmissions,
    Kind::LiveMetadataBytes,
    Kind::CheckpointBytes,
    Kind::ClosedJournalBytes,
    Kind::ClosedJournalSegments,
    Kind::ActiveJournalBytes,
    Kind::ActiveJournalOperations,
    Kind::SortBytes,
    Kind::ResponseBytes,
    Kind::CacheBytes,
    Kind::LogBytes,
    Kind::ColdBytes,
];

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Usage {
    values: [u64; COUNT],
}
impl Usage {
    pub fn get(&self, kind: Kind) -> Result<u64, Error> {
        self.values
            .get(kind as usize)
            .copied()
            .ok_or(Error::Inconsistent("quota registry"))
    }
    pub fn add(&mut self, kind: Kind, amount: u64) -> Result<(), Error> {
        let slot = self
            .values
            .get_mut(kind as usize)
            .ok_or(Error::Inconsistent("quota registry"))?;
        *slot = add(*slot, amount, "quota usage")?;
        Ok(())
    }
    pub fn subtract(&mut self, kind: Kind, amount: u64) -> Result<(), Error> {
        let slot = self
            .values
            .get_mut(kind as usize)
            .ok_or(Error::Inconsistent("quota registry"))?;
        *slot = slot
            .checked_sub(amount)
            .ok_or(Error::Inconsistent("quota charge underflow"))?;
        Ok(())
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Charge {
    pub kind: Kind,
    pub amount: u64,
}
impl Charge {
    pub const ZERO: Self = Self {
        kind: Kind::BodyBytes,
        amount: 0,
    };
}

#[derive(Clone, Debug)]
pub struct Quotas {
    caps: Usage,
    used: Usage,
    pending: Usage,
}
impl Quotas {
    pub fn new(plan: &Plan, used: Usage) -> Result<Self, Kind> {
        let d = plan.disk();
        let caps = Usage {
            values: [
                d.body_bytes,
                d.body_files,
                plan.upload_bytes(),
                plan.queue_bytes(),
                plan.queue_submissions(),
                d.live_metadata_bytes,
                d.checkpoint_bytes,
                plan.closed_journal_bytes(),
                plan.closed_journal_segments(),
                plan.active_journal_bytes(),
                plan.active_journal_operations(),
                plan.sort_bytes(),
                d.response_total_bytes,
                d.cache_bytes,
                plan.log_bytes(),
                d.cold_bytes,
            ],
        };
        for ((cap, used), kind) in caps.values.iter().zip(used.values.iter()).zip(ALL) {
            if used > cap {
                return Err(kind);
            }
        }
        Ok(Self {
            caps,
            used,
            pending: Usage::default(),
        })
    }
    pub fn used(&self) -> &Usage {
        &self.used
    }
    pub fn pending(&self) -> &Usage {
        &self.pending
    }
    pub fn caps(&self) -> &Usage {
        &self.caps
    }
    pub fn with_reservation(&self, extra: Usage) -> Result<Self, Kind> {
        let mut next = self.clone();
        for ((((out, pending), used), cap), kind) in next
            .pending
            .values
            .iter_mut()
            .zip(self.pending.values.iter())
            .zip(self.used.values.iter())
            .zip(self.caps.values.iter())
            .zip(ALL)
        {
            let value = extra.values.get(kind as usize).copied().ok_or(kind)?;
            let pending = pending.checked_add(value).ok_or(kind)?;
            if used.checked_add(pending).ok_or(kind)? > *cap {
                return Err(kind);
            }
            *out = pending;
        }
        Ok(next)
    }
    /// Called only at the writer barrier. Header bytes were already written;
    /// this is a logical category transfer, never new physical growth.
    pub(super) fn with_rollover(&self) -> Result<Self, Kind> {
        let mut next = self.clone();
        let bytes = self
            .used
            .get(Kind::ActiveJournalBytes)
            .map_err(|_| Kind::ActiveJournalBytes)?;
        let operations = self
            .used
            .get(Kind::ActiveJournalOperations)
            .map_err(|_| Kind::ActiveJournalOperations)?;
        let header = super::widen(crate::format::JOURNAL_HEADER_BYTES, "journal header")
            .map_err(|_| Kind::ClosedJournalBytes)?;
        let closed = bytes.checked_add(header).ok_or(Kind::ClosedJournalBytes)?;
        next.used
            .add(Kind::ClosedJournalBytes, closed)
            .map_err(|_| Kind::ClosedJournalBytes)?;
        next.used
            .add(Kind::ClosedJournalSegments, 1)
            .map_err(|_| Kind::ClosedJournalSegments)?;
        next.used
            .subtract(Kind::ActiveJournalBytes, bytes)
            .map_err(|_| Kind::ActiveJournalBytes)?;
        next.used
            .subtract(Kind::ActiveJournalOperations, operations)
            .map_err(|_| Kind::ActiveJournalOperations)?;
        next.with_reservation(Usage::default())
    }
    // Private transitions to be wired to checked live lease entries. No public
    // release-by-kind or public constructed reservation can authorize effects.
    pub(super) fn complete(&mut self, used: Usage) -> Result<(), Error> {
        let mut next = self.clone();
        for kind in ALL {
            let amount = used.get(kind)?;
            next.pending.subtract(kind, amount)?;
            next.used.add(kind, amount)?;
        }
        *self = next;
        Ok(())
    }
    pub(super) fn release_unused(&mut self, unused: Usage) -> Result<(), Error> {
        let mut next = self.pending;
        for kind in ALL {
            next.subtract(kind, unused.get(kind)?)?;
        }
        self.pending = next;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        admission::{DiskLimits, ViewMode, WorkLimits, GIB, MIB},
        limits::Limits,
    };
    #[test]
    fn each_quota_kind_maps_to_its_distinct_configured_cap(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let resources = Limits {
            upload_disk_bytes: 132 * crate::limits::MIB,
            queue_disk_bytes: 260 * crate::limits::MIB,
            queue_submissions: 1003,
            sort_disk_bytes: 68 * crate::limits::MIB,
            log_file_bytes: 9 * crate::limits::MIB,
            retained_logs: 6,
            ..Limits::default()
        }
        .plan()?;
        let plan = DiskLimits {
            body_files: 250003,
            live_metadata_bytes: 258 * MIB,
            checkpoint_bytes: 3 * GIB,
            response_total_bytes: 270 * MIB,
            cache_bytes: 140 * MIB,
            cold_bytes: 18 * MIB,
            ..DiskLimits::default()
        }
        .plan(
            &resources,
            WorkLimits::default(),
            ViewMode::OnlineBackground,
        )?;
        let quotas = Quotas::new(&plan, Usage::default()).map_err(|_| "invalid fixture caps")?;
        let expected = [
            4294967296, 250003, 138412032, 272629760, 1003, 270532608, 3221225472, 415236096, 195,
            4194304, 8192, 71303168, 283115520, 146800640, 66060288, 18874368,
        ];
        for (kind, cap) in ALL.into_iter().zip(expected) {
            assert_eq!(quotas.caps().get(kind)?, cap, "{kind:?}");
        }
        Ok(())
    }
}

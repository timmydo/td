//! Logical quota state used only under the reservation coordinator.
use super::{add, Error, Plan};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum Kind {
    BodyBytes,
    BlobCount,
    UploadBytes,
    QueueBytes,
    QueueSubmissions,
    DatabaseBytes,
    WalBytes,
    SortBytes,
    ResponseBytes,
    CacheBytes,
    LogBytes,
    ColdBytes,
}
pub const COUNT: usize = 12;
pub const ALL: [Kind; COUNT] = [
    Kind::BodyBytes,
    Kind::BlobCount,
    Kind::UploadBytes,
    Kind::QueueBytes,
    Kind::QueueSubmissions,
    Kind::DatabaseBytes,
    Kind::WalBytes,
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
                d.blob_count,
                plan.upload_bytes(),
                plan.queue_bytes(),
                plan.queue_submissions(),
                d.database_bytes,
                d.wal_bytes,
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
        admission::{DiskLimits, ViewMode, WorkLimits, MIB},
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
            blob_count: 250003,
            database_bytes: super::super::DATABASE_BYTES,
            wal_bytes: 20 * super::super::GIB,
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
            4294967296,
            250003,
            138412032,
            272629760,
            1003,
            8589934592,
            21474836480,
            71303168,
            283115520,
            146800640,
            66060288,
            18874368,
        ];
        for (kind, cap) in ALL.into_iter().zip(expected) {
            assert_eq!(quotas.caps().get(kind)?, cap, "{kind:?}");
        }
        Ok(())
    }
}

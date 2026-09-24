//! Checked disk/work configuration. No filesystem admission or probe yet.
use crate::limits::ResourcePlan;

pub const MIB: u64 = 1 << 20;
pub const GIB: u64 = 1 << 30;
pub const HISTORY_BYTES: u64 = 128 * MIB;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Range {
        field: &'static str,
        min: u64,
        max: u64,
    },
    Inconsistent(&'static str),
    Overflow(&'static str),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Range { field, min, max } => write!(f, "{field} must be in {min}..={max}"),
            Self::Inconsistent(rule) => write!(f, "inconsistent admission limits: {rule}"),
            Self::Overflow(field) => write!(f, "admission arithmetic overflow: {field}"),
        }
    }
}
impl std::error::Error for Error {}

macro_rules! settings {
    ($name:ident { $($field:ident: $default:expr, $min:expr, $max:expr;)+ }) => {
        #[derive(Clone, Copy, Debug, Eq, PartialEq)]
        pub struct $name { $(pub $field: u64,)+ }
        impl Default for $name {
            fn default() -> Self { Self { $($field: $default,)+ } }
        }
        impl $name {
            fn validate(&self) -> Result<(), Error> {
                $(if !($min..=$max).contains(&self.$field) {
                    return Err(Error::Range { field: stringify!($field), min: $min, max: $max });
                })+
                Ok(())
            }
        }
    };
}

pub mod space;
pub mod timers;
pub mod work;

settings! { DiskLimits {
    body_bytes: 4 * GIB, 1, 1024 * GIB;
    body_files: 250000, 1, 1000000;
    live_metadata_bytes: 256 * MIB, 1, GIB;
    checkpoint_bytes: 2 * GIB, 1, 8 * GIB;
    response_bytes: 128 * MIB, 1, GIB;
    response_total_bytes: 256 * MIB, 1, 4 * GIB;
    cache_bytes: 128 * MIB, 1024, GIB;
    cold_bytes: 16 * MIB, 1, 64 * MIB;
    free_bytes: 128 * MIB, 128 * MIB, u64::MAX;
    free_inodes: 4096, 4096, u64::MAX;
} }

settings! { WorkLimits {
    foreground_seconds: 120, 120, 16 * 120;
    foreground_io_bytes: 8 * GIB, 8 * GIB, 16 * 8 * GIB;
    foreground_records: 2000000, 2000000, 16 * 2000000;
    changes_seconds: 30, 30, 16 * 30;
    changes_io_bytes: 128 * MIB, 128 * MIB, 16 * 128 * MIB;
    changes_records: 1000000, 1000000, 16 * 1000000;
    request_seconds: 300, 300, 16 * 300;
    commit_seconds: 30, 30, 16 * 30;
    commit_io_bytes: 256 * MIB, 256 * MIB, 16 * 256 * MIB;
    commit_records: 250000, 250000, 16 * 250000;
    checkpoint_seconds: 60, 60, 16 * 60;
    checkpoint_io_bytes: 2 * GIB, 2 * GIB, 16 * 2 * GIB;
    gc_drain_seconds: 30, 30, 16 * 30;
    gc_seconds: 120, 120, 16 * 120;
    gc_io_bytes: 8 * GIB, 8 * GIB, 16 * 8 * GIB;
    gc_records: 128000000, 128000000, 16 * 128000000;
    gc_unlinks: 1000, 1000, 16 * 1000;
    backup_seconds: 900, 900, 16 * 900;
    backup_io_bytes: 16 * GIB, 16 * GIB, 16 * 16 * GIB;
    admission_seconds: 1, 1, 16;
} }

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ViewMode {
    ForegroundOnly,
    OnlineBackground,
}

/// Immutable validated disk/work settings and independent logical quota caps.
/// A future coordinator must still check observed use and physical free space.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Plan {
    disk: DiskLimits,
    work: WorkLimits,
    view_mode: ViewMode,
    closed_journal_bytes: u64,
    closed_journal_segments: u64,
    minimum_response_bytes: u64,
    log_bytes: u64,
    upload_bytes: u64,
    queue_bytes: u64,
    queue_submissions: u64,
    sort_bytes: u64,
}

impl Plan {
    pub fn disk(&self) -> &DiskLimits {
        &self.disk
    }
    pub fn work(&self) -> &WorkLimits {
        &self.work
    }
    pub fn view_mode(&self) -> ViewMode {
        self.view_mode
    }
    pub fn closed_journal_bytes(&self) -> u64 {
        self.closed_journal_bytes
    }
    pub fn closed_journal_segments(&self) -> u64 {
        self.closed_journal_segments
    }
    pub fn minimum_response_bytes(&self) -> u64 {
        self.minimum_response_bytes
    }
    pub fn log_bytes(&self) -> u64 {
        self.log_bytes
    }
    pub fn upload_bytes(&self) -> u64 {
        self.upload_bytes
    }
    pub fn queue_bytes(&self) -> u64 {
        self.queue_bytes
    }
    pub fn queue_submissions(&self) -> u64 {
        self.queue_submissions
    }
    pub fn sort_bytes(&self) -> u64 {
        self.sort_bytes
    }
}

fn add(a: u64, b: u64, field: &'static str) -> Result<u64, Error> {
    a.checked_add(b).ok_or(Error::Overflow(field))
}
fn mul(a: u64, b: u64, field: &'static str) -> Result<u64, Error> {
    a.checked_mul(b).ok_or(Error::Overflow(field))
}
fn widen(value: usize, field: &'static str) -> Result<u64, Error> {
    u64::try_from(value).map_err(|_| Error::Overflow(field))
}
fn require(condition: bool, rule: &'static str) -> Result<(), Error> {
    if condition {
        Ok(())
    } else {
        Err(Error::Inconsistent(rule))
    }
}

impl DiskLimits {
    pub fn plan(
        self,
        resources: &ResourcePlan,
        work: WorkLimits,
        views: ViewMode,
    ) -> Result<Plan, Error> {
        self.validate()?;
        work.validate()?;
        require(
            self.live_metadata_bytes
                >= mul(
                    widen(crate::format::TABLE_COUNT, "table count")?,
                    widen(crate::format::TABLE_HEADER_BYTES, "table header bytes")?,
                    "empty metadata headers",
                )?,
            "live metadata quota must fit empty table headers",
        )?;
        let limits = resources.limits();
        let journal = widen(limits.journal_bytes, "journal bytes")?;
        let operations = widen(limits.journal_operations, "journal operations")?;
        let sort = widen(limits.sort_disk_bytes, "sort bytes")?;
        let generations = add(
            widen(limits.storage_views, "storage views")?,
            1,
            "closed journal generations",
        )?;
        let closed_journal_bytes = mul(
            generations,
            add(HISTORY_BYTES, journal, "closed journal length")?,
            "closed journal cap",
        )?;
        let closed_journal_segments = mul(
            generations,
            add(
                widen(crate::format::MAX_HISTORY_DESCRIPTORS, "history segments")?,
                1,
                "history segments",
            )?,
            "closed journal segments",
        )?;
        let checkpoint = add(
            add(self.live_metadata_bytes, journal, "checkpoint projection")?,
            MIB,
            "checkpoint projection",
        )?;
        let minimum_response_bytes = add(
            add(
                mul(
                    32,
                    widen(limits.json_bytes, "JSON bytes")?,
                    "response framing",
                )?,
                mul(
                    4096,
                    widen(limits.json_methods, "JSON methods")?,
                    "response framing",
                )?,
                "response framing",
            )?,
            64 * 1024,
            "response framing",
        )?;
        let gc_io = add(
            add(
                mul(4, self.live_metadata_bytes, "GC I/O")?,
                mul(16, sort, "GC I/O")?,
                "GC I/O",
            )?,
            add(mul(8, journal, "GC I/O")?, 16 * MIB, "GC I/O")?,
            "GC I/O",
        )?;
        let gc_records = add(
            mul(
                16,
                add(self.live_metadata_bytes / 64, self.body_files, "GC records")?,
                "GC records",
            )?,
            mul(16, operations, "GC records")?,
            "GC records",
        )?;
        for (condition, rule) in [
            (
                self.body_bytes >= widen(limits.message_bytes, "message bytes")?,
                "raw body quota must fit one maximum message",
            ),
            (
                self.response_bytes <= self.response_total_bytes,
                "per-request retention exceeds aggregate retention",
            ),
            (
                self.response_bytes >= minimum_response_bytes,
                "request retention cannot hold mandatory framing",
            ),
            (
                self.checkpoint_bytes >= mul(4, checkpoint, "checkpoint quota")?,
                "checkpoint quota cannot hold generation overlap",
            ),
            (
                work.checkpoint_io_bytes >= mul(2, checkpoint, "checkpoint I/O")?,
                "checkpoint I/O cannot cover configured metadata",
            ),
            (
                work.gc_io_bytes >= gc_io,
                "GC I/O cannot cover configured capacities",
            ),
            (
                work.gc_records >= gc_records,
                "GC records cannot cover configured capacities",
            ),
            (
                work.gc_seconds >= add(work.checkpoint_seconds, work.commit_seconds, "GC time")?,
                "GC time must include checkpoint and commit",
            ),
            (
                views != ViewMode::OnlineBackground || limits.storage_views >= 2,
                "background work requires a foreground read view",
            ),
        ] {
            require(condition, rule)?;
        }
        Ok(Plan {
            disk: self,
            work,
            view_mode: views,
            closed_journal_bytes,
            closed_journal_segments,
            minimum_response_bytes,
            log_bytes: widen(resources.log_disk_bytes(), "logs")?,
            upload_bytes: widen(limits.upload_disk_bytes, "upload quota")?,
            queue_bytes: widen(limits.queue_disk_bytes, "queue quota")?,
            queue_submissions: widen(limits.queue_submissions, "queue count")?,
            sort_bytes: sort,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::limits::Limits;

    #[test]
    fn default_disk_and_work_plan_pins_independent_numeric_oracles(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let resources = Limits::default().plan()?;
        let plan = DiskLimits::default().plan(
            &resources,
            WorkLimits::default(),
            ViewMode::OnlineBackground,
        )?;
        assert_eq!(plan.disk().body_bytes, 4294967296);
        assert_eq!(plan.closed_journal_bytes(), 415236096);
        assert_eq!(plan.closed_journal_segments(), 195);
        assert_eq!(plan.minimum_response_bytes(), 33685504);
        assert_eq!(plan.log_bytes(), 41943040);
        assert_eq!(plan.upload_bytes(), 134217728);
        assert_eq!(plan.queue_bytes(), 268435456);
        assert_eq!(plan.queue_submissions(), 1000);
        assert_eq!(plan.sort_bytes(), 67108864);
        assert_eq!(plan.work().foreground_io_bytes, 8589934592);
        Ok(())
    }

    #[test]
    fn mandatory_response_and_generation_limits_are_exact() -> Result<(), Box<dyn std::error::Error>>
    {
        let resources = Limits::default().plan()?;
        let disk = DiskLimits {
            response_bytes: 33685504,
            checkpoint_bytes: 1094713344,
            ..DiskLimits::default()
        };
        assert!(disk
            .plan(&resources, WorkLimits::default(), ViewMode::ForegroundOnly)
            .is_ok());
        assert!(DiskLimits {
            response_bytes: 33685503,
            ..disk
        }
        .plan(&resources, WorkLimits::default(), ViewMode::ForegroundOnly)
        .is_err());
        assert!(DiskLimits {
            checkpoint_bytes: 1094713343,
            ..disk
        }
        .plan(&resources, WorkLimits::default(), ViewMode::ForegroundOnly)
        .is_err());
        assert!(DiskLimits {
            response_total_bytes: 33685503,
            ..disk
        }
        .plan(&resources, WorkLimits::default(), ViewMode::ForegroundOnly)
        .is_err());
        Ok(())
    }

    #[test]
    fn raised_metadata_requires_raised_maintenance_work() -> Result<(), Box<dyn std::error::Error>>
    {
        let resources = Limits::default().plan()?;
        let disk = DiskLimits {
            live_metadata_bytes: GIB,
            checkpoint_bytes: 8 * GIB,
            ..DiskLimits::default()
        };
        assert!(disk
            .plan(&resources, WorkLimits::default(), ViewMode::ForegroundOnly)
            .is_err());
        let work = WorkLimits {
            checkpoint_io_bytes: 3 * GIB,
            gc_records: 300000000,
            ..WorkLimits::default()
        };
        assert!(disk
            .plan(&resources, work, ViewMode::ForegroundOnly)
            .is_ok());
        assert!(disk
            .plan(
                &resources,
                WorkLimits {
                    gc_records: 128000000,
                    ..work
                },
                ViewMode::ForegroundOnly
            )
            .is_err());
        assert!(disk
            .plan(
                &resources,
                WorkLimits {
                    gc_seconds: 120,
                    checkpoint_seconds: 120,
                    ..work
                },
                ViewMode::ForegroundOnly
            )
            .is_err());
        Ok(())
    }

    #[test]
    fn maintenance_capacity_relations_refuse_one_below_exact_boundaries(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let resources = Limits {
            sort_disk_bytes: 256 * crate::limits::MIB,
            ..Limits::default()
        }
        .plan()?;
        let disk = DiskLimits {
            live_metadata_bytes: GIB,
            checkpoint_bytes: 8 * GIB,
            ..DiskLimits::default()
        };
        let work = WorkLimits {
            checkpoint_io_bytes: 2157969408,
            gc_io_bytes: 8640266240,
            gc_records: 272566528,
            checkpoint_seconds: 90,
            commit_seconds: 60,
            gc_seconds: 150,
            ..WorkLimits::default()
        };
        assert!(disk
            .plan(&resources, work, ViewMode::OnlineBackground)
            .is_ok());
        for insufficient in [
            WorkLimits {
                checkpoint_io_bytes: 2157969407,
                ..work
            },
            WorkLimits {
                gc_io_bytes: 8640266239,
                ..work
            },
            WorkLimits {
                gc_records: 272566527,
                ..work
            },
            WorkLimits {
                gc_seconds: 149,
                ..work
            },
        ] {
            assert!(disk
                .plan(&resources, insufficient, ViewMode::OnlineBackground)
                .is_err());
        }
        Ok(())
    }

    #[test]
    fn logical_subquotas_do_not_reserve_whole_message_capacity(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let limits = Limits {
            upload_disk_bytes: 1,
            queue_disk_bytes: 1,
            storage_views: 1,
            ..Limits::default()
        };
        let resources = limits.plan()?;
        let disk = DiskLimits::default();
        let work = WorkLimits::default();
        let plan = disk.plan(&resources, work, ViewMode::ForegroundOnly)?;
        assert_eq!(plan.upload_bytes(), 1);
        assert_eq!(plan.queue_bytes(), 1);
        assert_eq!(plan.closed_journal_segments(), 130);
        assert_eq!(plan.view_mode(), ViewMode::ForegroundOnly);
        let small_quotas = Limits {
            upload_disk_bytes: 1,
            queue_disk_bytes: 1,
            ..Limits::default()
        }
        .plan()?;
        assert_eq!(
            small_quotas.total_bytes(),
            Limits::default().plan()?.total_bytes()
        );
        assert!(disk
            .plan(&resources, work, ViewMode::OnlineBackground)
            .is_err());
        Ok(())
    }

    #[test]
    fn ranges_zero_floors_and_overflow_refuse() -> Result<(), Box<dyn std::error::Error>> {
        let resources = Limits::default().plan()?;
        for disk in [
            DiskLimits {
                body_bytes: 0,
                ..DiskLimits::default()
            },
            DiskLimits {
                body_files: 1000001,
                ..DiskLimits::default()
            },
            DiskLimits {
                body_bytes: 1024 * GIB + 1,
                ..DiskLimits::default()
            },
            DiskLimits {
                free_bytes: 128 * MIB - 1,
                ..DiskLimits::default()
            },
            DiskLimits {
                free_inodes: 4095,
                ..DiskLimits::default()
            },
            DiskLimits {
                body_bytes: 1,
                ..DiskLimits::default()
            },
        ] {
            assert!(disk
                .plan(&resources, WorkLimits::default(), ViewMode::ForegroundOnly)
                .is_err());
        }
        for work in [
            WorkLimits {
                request_seconds: 299,
                ..WorkLimits::default()
            },
            WorkLimits {
                admission_seconds: 17,
                ..WorkLimits::default()
            },
        ] {
            assert!(DiskLimits::default()
                .plan(&resources, work, ViewMode::ForegroundOnly)
                .is_err());
        }
        assert!(matches!(
            DiskLimits::default().plan(
                &resources,
                WorkLimits {
                    gc_records: u64::MAX,
                    ..WorkLimits::default()
                },
                ViewMode::ForegroundOnly,
            ),
            Err(Error::Range {
                field: "gc_records",
                ..
            })
        ));
        let minimum = DiskLimits {
            live_metadata_bytes: 1232,
            cache_bytes: 1024,
            ..DiskLimits::default()
        };
        assert!(minimum
            .plan(&resources, WorkLimits::default(), ViewMode::ForegroundOnly)
            .is_ok());
        for disk in [
            DiskLimits {
                live_metadata_bytes: 1231,
                ..minimum
            },
            DiskLimits {
                cache_bytes: 1023,
                ..minimum
            },
        ] {
            assert!(disk
                .plan(&resources, WorkLimits::default(), ViewMode::ForegroundOnly)
                .is_err());
        }
        assert_eq!(add(u64::MAX, 1, "test"), Err(Error::Overflow("test")));
        assert_eq!(mul(u64::MAX, 2, "test"), Err(Error::Overflow("test")));
        Ok(())
    }
}

//! Checked logical disk/work configuration. No physical-space probe.
use crate::limits::ResourcePlan;

pub const MIB: u64 = 1 << 20;
pub const GIB: u64 = 1 << 30;
pub const DATABASE_BYTES: u64 = crate::limits::SQLITE_DATABASE_BYTES;
pub const WAL_BYTES: u64 = crate::limits::SQLITE_WAL_BYTES;

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
            pub(crate) const CONFIG_FIELDS: &'static [&'static str] = &[$(stringify!($field),)+];
            // Cold bounded string dispatch keeps declarations in one place.
            pub(crate) fn set_config(&mut self, index: usize, value: u64) -> bool {
                match Self::CONFIG_FIELDS.get(index).copied() {
                    $(Some(stringify!($field)) => { self.$field = value; true },)+
                    _ => false,
                }
            }
            fn validate(&self) -> Result<(), Error> {
                $(if !($min..=$max).contains(&self.$field) {
                    return Err(Error::Range { field: stringify!($field), min: $min, max: $max });
                })+
                Ok(())
            }
        }
    };
}

#[path = "admission/logical.rs"]
pub mod logical;
#[path = "admission/quota.rs"]
pub mod quota;
#[path = "admission/timers.rs"]
pub mod timers;
#[path = "admission/work.rs"]
pub mod work;

settings! { DiskLimits {
    body_bytes: 4 * GIB, 1, 4 * GIB;
    blob_count: 250000, 1, 1000000;
    database_bytes: DATABASE_BYTES, 1, DATABASE_BYTES;
    wal_bytes: WAL_BYTES, 1, 32 * GIB;
    response_bytes: 128 * MIB, 1, GIB;
    response_total_bytes: 256 * MIB, 1, 4 * GIB;
    cache_bytes: 128 * MIB, 1024, GIB;
    cold_bytes: 16 * MIB, 1, 64 * MIB;
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
    checkpoint_io_bytes: DATABASE_BYTES, DATABASE_BYTES, 16 * DATABASE_BYTES;
    gc_drain_seconds: 30, 30, 16 * 30;
    gc_seconds: 120, 120, 16 * 120;
    gc_io_bytes: 8 * GIB, 8 * GIB, 16 * 8 * GIB;
    gc_records: 128000000, 128000000, 16 * 128000000;
    gc_blobs: 1000, 1000, 16 * 1000;
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
/// Runtime admission reconciles actual logical use; I/O can still fail.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Plan {
    disk: DiskLimits,
    work: WorkLimits,
    view_mode: ViewMode,
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
        let limits = resources.limits();
        require(
            self.database_bytes == DATABASE_BYTES,
            "database quota must equal SQLite page ceiling",
        )?;
        require(
            self.wal_bytes >= WAL_BYTES,
            "WAL quota must fit bounded transaction overlap",
        )?;
        require(
            self.body_bytes >= widen(limits.message_bytes, "message bytes")?,
            "raw quota must fit one maximum message",
        )?;
        require(
            self.response_bytes <= self.response_total_bytes,
            "per-request retention exceeds aggregate retention",
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
            65536,
            "response framing",
        )?;
        require(
            self.response_bytes >= minimum_response_bytes,
            "request retention cannot hold mandatory framing",
        )?;
        if views == ViewMode::OnlineBackground {
            require(
                limits.storage_views >= 2,
                "online background work requires another view",
            )?;
        }
        Ok(Plan {
            disk: self,
            work,
            view_mode: views,
            minimum_response_bytes,
            log_bytes: widen(resources.log_disk_bytes(), "log bytes")?,
            upload_bytes: widen(limits.upload_disk_bytes, "upload bytes")?,
            queue_bytes: widen(limits.queue_disk_bytes, "queue bytes")?,
            queue_submissions: widen(limits.queue_submissions, "queue submissions")?,
            sort_bytes: widen(limits.sort_disk_bytes, "sort bytes")?,
        })
    }
}
#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::limits::Limits;
    #[test]
    fn sqlite_page_and_wal_caps_are_independently_admitted() {
        let resources = crate::limits::Limits::default().plan().unwrap();
        assert!(DiskLimits::default()
            .plan(&resources, WorkLimits::default(), ViewMode::ForegroundOnly)
            .is_ok());
        let disk = DiskLimits {
            database_bytes: DATABASE_BYTES - 1,
            ..DiskLimits::default()
        };
        assert!(disk
            .plan(&resources, WorkLimits::default(), ViewMode::ForegroundOnly)
            .is_err());
        let disk = DiskLimits {
            wal_bytes: WAL_BYTES - 1,
            ..DiskLimits::default()
        };
        assert!(disk
            .plan(&resources, WorkLimits::default(), ViewMode::ForegroundOnly)
            .is_err());
    }

    #[test]
    fn retained_response_view_and_sort_admission_boundaries(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let resources = crate::limits::Limits::default().plan()?;
        let disk = DiskLimits {
            response_bytes: 33685504,
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
            response_total_bytes: 33685503,
            ..disk
        }
        .plan(&resources, WorkLimits::default(), ViewMode::ForegroundOnly)
        .is_err());
        let one = crate::limits::Limits {
            storage_views: 1,
            ..crate::limits::Limits::default()
        }
        .plan()?;
        assert!(disk
            .plan(&one, WorkLimits::default(), ViewMode::OnlineBackground)
            .is_err());
        assert!(disk
            .plan(&one, WorkLimits::default(), ViewMode::ForegroundOnly)
            .is_ok());
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
                blob_count: 1000001,
                ..DiskLimits::default()
            },
            DiskLimits {
                body_bytes: 4 * GIB + 1,
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
            database_bytes: DATABASE_BYTES,
            cache_bytes: 1024,
            ..DiskLimits::default()
        };
        assert!(minimum
            .plan(&resources, WorkLimits::default(), ViewMode::ForegroundOnly)
            .is_ok());
        for disk in [
            DiskLimits {
                database_bytes: DATABASE_BYTES - 1,
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

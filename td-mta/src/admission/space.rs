//! Physical-space arithmetic for the future serialized reservation coordinator.
//! Inputs are trusted adapter/accounting snapshots, not client-provided values.
use super::{add, mul, widen, Error, Plan, MIB};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Growth {
    pub bytes: u64,
    pub inodes: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Inodes {
    Available(u64),
    Unsupported,
}

/// A successful probe of space available to an unprivileged writer. The
/// caller rejects failed probes; M05 owns open-descriptor identity checks.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Sample {
    pub available_bytes: u64,
    pub inodes: Inodes,
    pub allocation_unit: u64,
}

/// All namespaces on one backing filesystem share these counters. Completed
/// growth is monotonic; deletion never subtracts from it. A fresh probe, with
/// counters captured before it starts, is the only way to observe freed space.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Counters {
    pub completed: Growth,
    pub pending: Growth,
    /// Candidate reserve for the resulting state, including this request.
    /// Install it atomically with the new pending reservation.
    pub checkpoint: Growth,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Refusal {
    Invalid(Error),
    Bytes,
    Inodes,
    CounterRegression,
}
impl From<Error> for Refusal {
    fn from(e: Error) -> Self {
        Self::Invalid(e)
    }
}
impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(e) => e.fmt(f),
            Self::Bytes => f.write_str("insufficient filesystem bytes"),
            Self::Inodes => f.write_str("insufficient filesystem inodes"),
            Self::CounterRegression => f.write_str("completed growth counter regressed"),
        }
    }
}
impl std::error::Error for Refusal {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Invalid(error) => Some(error),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Headroom {
    pub bytes: u64,
    pub inodes: Inodes,
}

/// Round each file separately. Zero growth does not reserve an allocation unit.
/// This is a conservative length estimate; filesystem metadata has its floor.
pub fn rounded_bytes(bytes: u64, unit: u64) -> Result<u64, Error> {
    let whole = bytes
        .checked_div(unit)
        .ok_or(Error::Inconsistent("zero allocation unit"))?;
    let remainder = bytes
        .checked_rem(unit)
        .ok_or(Error::Inconsistent("zero allocation unit"))?;
    mul(
        add(whole, u64::from(remainder != 0), "allocation units")?,
        unit,
        "rounded growth",
    )
}

/// New allocation from extending one file with previously charged length.
/// A shrinking file gives no admission credit; deletion needs a new probe.
pub fn file_growth(old_length: u64, new_length: u64, unit: u64) -> Result<u64, Error> {
    if new_length < old_length {
        return Err(Error::Inconsistent("growth cannot shrink a file"));
    }
    rounded_bytes(new_length, unit)?
        .checked_sub(rounded_bytes(old_length, unit)?)
        .ok_or(Error::Overflow("file growth"))
}

/// Lengths of one growing file whose old allocation was already charged.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FileGrowth {
    pub old_length: u64,
    pub new_length: u64,
}

/// Per-file rounded demand, bound to the allocation unit used to compute it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RoundedGrowth {
    amount: Growth,
    unit: std::num::NonZeroU64,
}
impl RoundedGrowth {
    pub(super) fn checked_add(self, other: Self) -> Result<Self, Error> {
        if self.unit != other.unit {
            return Err(Error::Inconsistent("allocation unit mismatch"));
        }
        Ok(Self {
            unit: self.unit,
            amount: Growth {
                bytes: add(self.amount.bytes, other.amount.bytes, "physical byte sum")?,
                inodes: add(
                    self.amount.inodes,
                    other.amount.inodes,
                    "physical inode sum",
                )?,
            },
        })
    }
    pub(super) fn checked_sub(self, other: Self) -> Result<Self, Error> {
        if self.unit != other.unit {
            return Err(Error::Inconsistent("allocation unit mismatch"));
        }
        Ok(Self {
            unit: self.unit,
            amount: Growth {
                bytes: self
                    .amount
                    .bytes
                    .checked_sub(other.amount.bytes)
                    .ok_or(Error::Inconsistent("physical byte underflow"))?,
                inodes: self
                    .amount
                    .inodes
                    .checked_sub(other.amount.inodes)
                    .ok_or(Error::Inconsistent("physical inode underflow"))?,
            },
        })
    }
    pub(super) fn zeroed(self) -> Self {
        Self {
            unit: self.unit,
            amount: Growth::default(),
        }
    }
    /// Include new file/directory entries separately in `new_inodes`.
    /// The caller bounds the slice and charges the work of visiting every file.
    pub fn from_files(unit: u64, files: &[FileGrowth], new_inodes: u64) -> Result<Self, Error> {
        let unit =
            std::num::NonZeroU64::new(unit).ok_or(Error::Inconsistent("zero allocation unit"))?;
        let mut bytes = 0;
        for file in files {
            bytes = add(
                bytes,
                file_growth(file.old_length, file.new_length, unit.get())?,
                "file growth sum",
            )?;
        }
        Ok(Self {
            amount: Growth {
                bytes,
                inodes: new_inodes,
            },
            unit,
        })
    }
    /// Conservative growth for NEW files when only their total length bound
    /// is known. At most files-1 extra units cover arbitrary distribution.
    pub fn from_total_bound(
        unit: u64,
        bytes: u64,
        files: u64,
        new_inodes: u64,
    ) -> Result<Self, Error> {
        let unit =
            std::num::NonZeroU64::new(unit).ok_or(Error::Inconsistent("zero allocation unit"))?;
        let extra_files = files
            .checked_sub(1)
            .ok_or(Error::Inconsistent("total bound needs at least one file"))?;
        let bytes = add(
            rounded_bytes(bytes, unit.get())?,
            mul(extra_files, unit.get(), "file rounding slack")?,
            "rounded file total",
        )?;
        Ok(Self {
            amount: Growth {
                bytes,
                inodes: new_inodes,
            },
            unit,
        })
    }
    pub fn amount(self) -> Growth {
        self.amount
    }
    pub fn allocation_unit(self) -> u64 {
        self.unit.get()
    }
}

/// Evaluate one fresh probe under the coordinator lock. `captured` is the
/// completed counter value from before that SAME filesystem probe started.
/// Request bytes carry their rounding unit; directory/temp-file entries are
/// included in its inode count. The checkpoint reserve includes the request
/// being assessed. No counter or reservation is changed.
/// The coordinator must atomically install a passing request before another
/// evaluation, and must reject failed/stale/foreign probes before calling this.
pub fn assess(
    plan: &Plan,
    sample: Sample,
    captured: Growth,
    current: Counters,
    request: RoundedGrowth,
) -> Result<Headroom, Refusal> {
    if sample.allocation_unit == 0 {
        return Err(Error::Inconsistent("zero allocation unit").into());
    }
    if request.allocation_unit() != sample.allocation_unit {
        return Err(Error::Inconsistent("allocation unit changed").into());
    }
    let request = request.amount();
    let concurrent_bytes = current
        .completed
        .bytes
        .checked_sub(captured.bytes)
        .ok_or(Refusal::CounterRegression)?;
    let concurrent_inodes = current
        .completed
        .inodes
        .checked_sub(captured.inodes)
        .ok_or(Refusal::CounterRegression)?;
    // Completion must never overflow after an admitted write has happened.
    total(
        current.completed.bytes,
        current.pending.bytes,
        current.checkpoint.bytes,
        request.bytes,
        0,
        "completed byte counter",
    )?;
    total(
        current.completed.inodes,
        current.pending.inodes,
        current.checkpoint.inodes,
        request.inodes,
        0,
        "completed inode counter",
    )?;
    let need_bytes = total(
        concurrent_bytes,
        current.pending.bytes,
        current.checkpoint.bytes,
        request.bytes,
        plan.disk().free_bytes,
        "byte demand",
    )
    .map_err(|_| Refusal::Bytes)?;
    let bytes = sample
        .available_bytes
        .checked_sub(need_bytes)
        .ok_or(Refusal::Bytes)?;
    let inodes = match sample.inodes {
        Inodes::Unsupported => Inodes::Unsupported,
        Inodes::Available(available) => {
            let need = total(
                concurrent_inodes,
                current.pending.inodes,
                current.checkpoint.inodes,
                request.inodes,
                plan.disk().free_inodes,
                "inode demand",
            )
            .map_err(|_| Refusal::Inodes)?;
            Inodes::Available(available.checked_sub(need).ok_or(Refusal::Inodes)?)
        }
    };
    Ok(Headroom { bytes, inodes })
}

fn total(
    concurrent: u64,
    pending: u64,
    checkpoint: u64,
    request: u64,
    floor: u64,
    field: &'static str,
) -> Result<u64, Error> {
    add(
        add(add(concurrent, pending, field)?, checkpoint, field)?,
        add(request, floor, field)?,
        field,
    )
}

/// Conservative unrounded checkpoint output length before every mutation.
/// Include committed and reserved journal bytes/operations, INCLUDING the
/// candidate request. This is not a rounded physical reservation. Use
/// RoundedGrowth::from_total_bound with all prospective table/control files
/// (including manifest and CURRENT temporary) and new directory/file inodes;
/// rounding this total once without per-file slack under-reserves space.
pub fn checkpoint_bytes(
    selected_tables: u64,
    journal_bytes: u64,
    journal_operations: u64,
) -> Result<u64, Error> {
    let correction = crate::format::RECORD_OVERHEAD_BYTES
        .checked_sub(crate::format::OPERATION_HEADER_BYTES)
        .ok_or(Error::Inconsistent(
            "record overhead below operation header",
        ))?;
    add(
        add(selected_tables, journal_bytes, "checkpoint reserve")?,
        add(
            mul(
                widen(correction, "record overhead correction")?,
                journal_operations,
                "checkpoint reserve",
            )?,
            MIB,
            "checkpoint reserve",
        )?,
        "checkpoint reserve",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        admission::{DiskLimits, ViewMode, WorkLimits},
        limits::Limits,
    };
    fn plan() -> Result<Plan, Box<dyn std::error::Error>> {
        Ok(DiskLimits::default().plan(
            &Limits::default().plan()?,
            WorkLimits::default(),
            ViewMode::OnlineBackground,
        )?)
    }
    fn evaluate(
        plan: &Plan,
        sample: Sample,
        captured: Growth,
        current: Counters,
        request: Growth,
    ) -> Result<Headroom, Refusal> {
        let rounded = RoundedGrowth::from_files(
            sample.allocation_unit,
            &[FileGrowth {
                old_length: 0,
                new_length: request.bytes,
            }],
            request.inodes,
        )?;
        assess(plan, sample, captured, current, rounded)
    }
    #[test]
    fn every_file_rounds_separately_and_overflow_refuses() -> Result<(), Box<dyn std::error::Error>>
    {
        assert_eq!(rounded_bytes(0, 4096)?, 0);
        assert_eq!(rounded_bytes(1, 4096)?, 4096);
        assert_eq!(rounded_bytes(4096, 4096)?, 4096);
        assert_eq!(rounded_bytes(4097, 4096)?, 8192);
        assert_eq!(rounded_bytes(7, 3)?, 9);
        // Two separate one-byte files need two units, not one shared unit.
        let tiny = RoundedGrowth::from_files(
            4096,
            &[FileGrowth {
                old_length: 0,
                new_length: 1,
            }; 2],
            2,
        )?;
        assert_eq!(
            tiny.amount(),
            Growth {
                bytes: 8192,
                inodes: 2
            }
        );
        assert_eq!(tiny.allocation_unit(), 4096);
        assert_eq!(rounded_bytes(u64::MAX - 4095, 4096)?, u64::MAX - 4095);
        assert!(rounded_bytes(u64::MAX - 4094, 4096).is_err());
        assert_eq!(file_growth(1, 4096, 4096)?, 0);
        assert_eq!(file_growth(4096, 4097, 4096)?, 4096);
        assert!(file_growth(2, 1, 4096).is_err());
        assert!(rounded_bytes(0, 0).is_err());
        assert!(rounded_bytes(u64::MAX, 4096).is_err());
        assert_eq!(rounded_bytes(u64::MAX, 1)?, u64::MAX);
        Ok(())
    }
    #[test]
    fn exact_floors_pending_and_checkpoint_capacity_are_preserved(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let p = plan()?;
        let current = Counters {
            pending: Growth {
                bytes: 8192,
                inodes: 2,
            },
            checkpoint: Growth {
                bytes: 16384,
                inodes: 12,
            },
            ..Counters::default()
        };
        let request = Growth {
            bytes: 4096,
            inodes: 1,
        };
        let sample = Sample {
            available_bytes: 134246400,
            inodes: Inodes::Available(4111),
            allocation_unit: 4096,
        };
        assert_eq!(
            evaluate(&p, sample, Growth::default(), current, request)?,
            Headroom {
                bytes: 0,
                inodes: Inodes::Available(0)
            }
        );
        assert_eq!(
            evaluate(
                &p,
                Sample {
                    available_bytes: sample.available_bytes - 1,
                    ..sample
                },
                Growth::default(),
                current,
                request
            ),
            Err(Refusal::Bytes)
        );
        assert_eq!(
            evaluate(
                &p,
                Sample {
                    inodes: Inodes::Available(4110),
                    ..sample
                },
                Growth::default(),
                current,
                request
            ),
            Err(Refusal::Inodes)
        );
        Ok(())
    }
    #[test]
    fn concurrent_completions_cannot_free_pending_space_twice(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let p = plan()?;
        let before = Growth {
            bytes: 4096,
            inodes: 1,
        };
        let sample = Sample {
            available_bytes: 134225920,
            inodes: Inodes::Available(4098),
            allocation_unit: 4096,
        };
        let mut counters = Counters {
            completed: before,
            pending: Growth {
                bytes: 8192,
                inodes: 2,
            },
            ..Counters::default()
        };
        assert_eq!(
            evaluate(
                &p,
                sample,
                before,
                counters,
                Growth {
                    bytes: 1,
                    inodes: 0
                }
            ),
            Err(Refusal::Bytes)
        );
        counters.completed = Growth {
            bytes: 8192,
            inodes: 2,
        };
        counters.pending = Growth {
            bytes: 4096,
            inodes: 1,
        };
        assert_eq!(
            evaluate(
                &p,
                sample,
                before,
                counters,
                Growth {
                    bytes: 1,
                    inodes: 0
                }
            ),
            Err(Refusal::Bytes)
        );
        counters.completed = Growth {
            bytes: 12288,
            inodes: 3,
        };
        counters.pending = Growth::default();
        assert_eq!(
            evaluate(
                &p,
                sample,
                before,
                counters,
                Growth {
                    bytes: 1,
                    inodes: 0
                }
            ),
            Err(Refusal::Bytes)
        );
        // A new probe starts after completion; it no longer deducts old growth.
        let fresh = Sample {
            available_bytes: 134221824,
            inodes: Inodes::Available(4097),
            ..sample
        };
        assert_eq!(
            evaluate(
                &p,
                fresh,
                counters.completed,
                counters,
                Growth {
                    bytes: 4096,
                    inodes: 1
                }
            )?
            .bytes,
            0
        );
        assert_eq!(
            evaluate(
                &p,
                sample,
                Growth {
                    bytes: 12289,
                    inodes: 3
                },
                counters,
                Growth::default()
            ),
            Err(Refusal::CounterRegression)
        );
        Ok(())
    }
    #[test]
    fn unsupported_inodes_leave_byte_checks_active() -> Result<(), Box<dyn std::error::Error>> {
        let p = plan()?;
        let sample = Sample {
            available_bytes: 134217728,
            inodes: Inodes::Unsupported,
            allocation_unit: 4096,
        };
        assert_eq!(
            evaluate(
                &p,
                sample,
                Growth::default(),
                Counters::default(),
                Growth::default()
            )?
            .inodes,
            Inodes::Unsupported
        );
        assert_eq!(
            evaluate(
                &p,
                sample,
                Growth::default(),
                Counters::default(),
                Growth {
                    bytes: 1,
                    inodes: 0
                }
            ),
            Err(Refusal::Bytes)
        );
        assert!(matches!(
            evaluate(
                &p,
                sample,
                Growth {
                    bytes: u64::MAX,
                    inodes: 0
                },
                Counters {
                    completed: Growth {
                        bytes: u64::MAX,
                        inodes: 0
                    },
                    ..Counters::default()
                },
                Growth {
                    bytes: 1,
                    inodes: 0
                }
            ),
            Err(Refusal::Invalid(Error::Overflow(_)))
        ));
        assert!(matches!(
            evaluate(
                &p,
                sample,
                Growth {
                    bytes: 0,
                    inodes: u64::MAX
                },
                Counters {
                    completed: Growth {
                        bytes: 0,
                        inodes: u64::MAX
                    },
                    ..Counters::default()
                },
                Growth {
                    bytes: 0,
                    inodes: 1
                }
            ),
            Err(Refusal::Invalid(Error::Overflow(_)))
        ));
        // A valid counter domain can still exceed available space.
        assert_eq!(
            evaluate(
                &p,
                sample,
                Growth::default(),
                Counters {
                    pending: Growth {
                        bytes: u64::MAX,
                        inodes: 0
                    },
                    ..Counters::default()
                },
                Growth::default()
            ),
            Err(Refusal::Bytes)
        );
        Ok(())
    }
    #[test]
    fn checkpoint_reserve_corrects_every_journal_operation(
    ) -> Result<(), Box<dyn std::error::Error>> {
        assert_eq!(checkpoint_bytes(1232, 4194304, 8192)?, 5539024);
        assert!(checkpoint_bytes(u64::MAX, 1, 0).is_err());
        assert!(checkpoint_bytes(0, 0, u64::MAX).is_err());
        Ok(())
    }
    #[test]
    fn pending_and_checkpoint_growth_need_counter_headroom(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let p = plan()?;
        let sample = Sample {
            available_bytes: u64::MAX,
            inodes: Inodes::Unsupported,
            allocation_unit: 1,
        };
        for checkpoint in [false, true] {
            for inodes in [false, true] {
                let completed = if inodes {
                    Growth {
                        bytes: 0,
                        inodes: u64::MAX - 10,
                    }
                } else {
                    Growth {
                        bytes: u64::MAX - 10,
                        inodes: 0,
                    }
                };
                let extra = if inodes {
                    Growth {
                        bytes: 0,
                        inodes: 11,
                    }
                } else {
                    Growth {
                        bytes: 11,
                        inodes: 0,
                    }
                };
                let current = Counters {
                    completed,
                    pending: if checkpoint { Growth::default() } else { extra },
                    checkpoint: if checkpoint { extra } else { Growth::default() },
                };
                let expected = if inodes {
                    "completed inode counter"
                } else {
                    "completed byte counter"
                };
                assert_eq!(
                    evaluate(&p, sample, completed, current, Growth::default()),
                    Err(Refusal::Invalid(Error::Overflow(expected)))
                );
            }
        }
        Ok(())
    }
    #[test]
    fn extreme_floors_are_shortage_not_counter_faults() -> Result<(), Box<dyn std::error::Error>> {
        let resources = Limits::default().plan()?;
        let sample = Sample {
            available_bytes: u64::MAX,
            inodes: Inodes::Available(u64::MAX),
            allocation_unit: 1,
        };
        let p = DiskLimits {
            free_bytes: u64::MAX,
            ..DiskLimits::default()
        }
        .plan(
            &resources,
            WorkLimits::default(),
            ViewMode::OnlineBackground,
        )?;
        assert_eq!(
            evaluate(
                &p,
                sample,
                Growth::default(),
                Counters::default(),
                Growth {
                    bytes: 1,
                    inodes: 0
                }
            ),
            Err(Refusal::Bytes)
        );
        let p = DiskLimits {
            free_inodes: u64::MAX,
            ..DiskLimits::default()
        }
        .plan(
            &resources,
            WorkLimits::default(),
            ViewMode::OnlineBackground,
        )?;
        assert_eq!(
            evaluate(
                &p,
                sample,
                Growth::default(),
                Counters::default(),
                Growth {
                    bytes: 0,
                    inodes: 1
                }
            ),
            Err(Refusal::Inodes)
        );
        Ok(())
    }
    #[test]
    fn rounded_demands_bind_the_unit_and_cover_unknown_file_distribution(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let p = plan()?;
        let request = RoundedGrowth::from_files(
            4096,
            &[FileGrowth {
                old_length: 0,
                new_length: 1,
            }],
            1,
        )?;
        let sample = Sample {
            available_bytes: u64::MAX,
            inodes: Inodes::Unsupported,
            allocation_unit: 8192,
        };
        assert_eq!(
            assess(&p, sample, Growth::default(), Counters::default(), request),
            Err(Refusal::Invalid(Error::Inconsistent(
                "allocation unit changed"
            )))
        );
        assert_eq!(
            assess(
                &p,
                Sample {
                    allocation_unit: 0,
                    ..sample
                },
                Growth::default(),
                Counters::default(),
                request
            ),
            Err(Refusal::Invalid(Error::Inconsistent(
                "zero allocation unit"
            )))
        );
        assert_eq!(
            RoundedGrowth::from_total_bound(4096, 2, 2, 2)?
                .amount()
                .bytes,
            8192
        );
        let checkpoint = checkpoint_bytes(1232, 4194304, 8192)?;
        // Thirteen files: eleven tables, manifest and CURRENT temporary.
        assert_eq!(
            RoundedGrowth::from_total_bound(4194304, checkpoint, 13, 14)?.amount(),
            Growth {
                bytes: 58720256,
                inodes: 14
            }
        );
        assert!(RoundedGrowth::from_total_bound(4096, 1, 0, 0).is_err());
        assert!(RoundedGrowth::from_total_bound(u64::MAX, 1, 2, 2).is_err());
        Ok(())
    }
}

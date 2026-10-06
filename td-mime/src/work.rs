//! Per-job charged counters; no I/O, cancellation of kernel calls or scheduler.
use crate::time::{Deadline, Tick};
use std::{
    num::NonZeroU64,
    sync::atomic::{AtomicU64, Ordering},
};

static NEXT_OWNER: AtomicU64 = AtomicU64::new(1);

/// A move-stable owner label. It conveys no work or publication authority.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Owner(NonZeroU64);
impl Owner {
    pub fn issue() -> Option<Self> {
        Self::issue_from(&NEXT_OWNER)
    }
    fn issue_from(next: &AtomicU64) -> Option<Self> {
        let mut current = next.load(Ordering::Relaxed);
        loop {
            let following = current.checked_add(1)?;
            match next.compare_exchange_weak(
                current,
                following,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(value) => return NonZeroU64::new(value).map(Self),
                Err(value) => current = value,
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Charge {
    pub io_bytes: u64,
    pub records: u64,
    pub output_bytes: u64,
    pub unlinks: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Stop {
    Deadline,
    IoBytes,
    Records,
    OutputBytes,
    Unlinks,
}

impl std::fmt::Display for Stop {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Deadline => "work deadline expired",
            Self::IoBytes => "work I/O budget exhausted",
            Self::Records => "work record budget exhausted",
            Self::OutputBytes => "work output budget exhausted",
            Self::Unlinks => "work unlink budget exhausted",
        })
    }
}
impl std::error::Error for Stop {}

/// Charge before bounded work, including replay and failed operations.
/// The first refusal is sticky; it cannot be cleared by retrying a smaller
/// charge. A refusal changes no remaining counters. This is not a quota lease
/// or permission to roll back/relabel an already committed store operation.
#[derive(Debug)]
pub struct Meter {
    deadline: Deadline,
    remaining: Charge,
    stopped: Option<Stop>,
    owner: Option<Owner>,
}

impl Meter {
    /// The caller supplies the job's validated caps and enclosing deadline.
    pub fn new(deadline: impl Into<Deadline>, capacity: Charge) -> Self {
        Self {
            deadline: deadline.into(),
            remaining: capacity,
            stopped: None,
            owner: None,
        }
    }
    /// Used by shared admission to bind detached prepaid credit to this job.
    #[doc(hidden)]
    pub fn owner(&mut self) -> Option<Owner> {
        if self.owner.is_none() {
            self.owner = Owner::issue();
        }
        self.owner
    }
    pub fn remaining(&self) -> Charge {
        self.remaining
    }
    pub fn stopped(&self) -> Option<Stop> {
        self.stopped
    }
    pub fn deadline(&self) -> Deadline {
        self.deadline
    }

    pub fn charge(&mut self, now: Tick, requested: Charge) -> Result<(), Stop> {
        if let Some(reason) = self.stopped {
            return Err(reason);
        }
        let result = self.checked_charge(now, requested);
        match result {
            Ok(remaining) => {
                self.remaining = remaining;
                Ok(())
            }
            Err(reason) => {
                self.stopped = Some(reason);
                Err(reason)
            }
        }
    }

    fn checked_charge(&self, now: Tick, requested: Charge) -> Result<Charge, Stop> {
        if self.deadline.expired(now) {
            return Err(Stop::Deadline);
        }
        Ok(Charge {
            io_bytes: self
                .remaining
                .io_bytes
                .checked_sub(requested.io_bytes)
                .ok_or(Stop::IoBytes)?,
            records: self
                .remaining
                .records
                .checked_sub(requested.records)
                .ok_or(Stop::Records)?,
            output_bytes: self
                .remaining
                .output_bytes
                .checked_sub(requested.output_bytes)
                .ok_or(Stop::OutputBytes)?,
            unlinks: self
                .remaining
                .unlinks
                .checked_sub(requested.unlinks)
                .ok_or(Stop::Unlinks)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owner_labels_refuse_counter_wrap() {
        let next = AtomicU64::new(u64::MAX - 1);
        assert!(Owner::issue_from(&next).is_some());
        assert!(Owner::issue_from(&next).is_none());
        assert!(Owner::issue_from(&next).is_none());
        assert_eq!(next.load(Ordering::Relaxed), u64::MAX);
    }

    #[test]
    fn repeated_work_and_exact_capacity_are_charged() -> Result<(), Box<dyn std::error::Error>> {
        let mut meter = Meter::new(
            Deadline::after(Tick(0), 10)?,
            Charge {
                io_bytes: 8,
                records: 2,
                output_bytes: 6,
                unlinks: 2,
            },
        );
        let step = Charge {
            io_bytes: 4,
            records: 1,
            output_bytes: 3,
            unlinks: 1,
        };
        meter.charge(Tick(1), step)?;
        meter.charge(Tick(2), step)?;
        assert_eq!(meter.remaining(), Charge::default());
        meter.charge(Tick(9), Charge::default())?;
        assert_eq!(meter.charge(Tick(9), step), Err(Stop::IoBytes));
        assert_eq!(meter.charge(Tick(9), Charge::default()), Err(Stop::IoBytes));
        Ok(())
    }

    #[test]
    fn every_failure_is_atomic_and_sticky() -> Result<(), Box<dyn std::error::Error>> {
        let capacity = Charge {
            io_bytes: 2,
            records: 3,
            output_bytes: 4,
            unlinks: 2,
        };
        for (requested, reason) in [
            (
                Charge {
                    io_bytes: u64::MAX,
                    ..Charge::default()
                },
                Stop::IoBytes,
            ),
            (
                Charge {
                    io_bytes: 1,
                    records: 4,
                    output_bytes: 1,
                    unlinks: 1,
                },
                Stop::Records,
            ),
            (
                Charge {
                    io_bytes: 1,
                    records: 1,
                    output_bytes: 5,
                    unlinks: 1,
                },
                Stop::OutputBytes,
            ),
            (
                Charge {
                    io_bytes: 1,
                    records: 1,
                    output_bytes: 1,
                    unlinks: 3,
                },
                Stop::Unlinks,
            ),
        ] {
            let mut meter = Meter::new(Deadline::after(Tick(0), 10)?, capacity);
            assert_eq!(meter.charge(Tick(1), requested), Err(reason));
            assert_eq!(meter.remaining(), capacity);
            assert_eq!(meter.charge(Tick(10), Charge::default()), Err(reason));
            assert_eq!(meter.stopped(), Some(reason));
        }
        Ok(())
    }

    #[test]
    fn absolute_deadline_never_moves_with_progress() -> Result<(), Box<dyn std::error::Error>> {
        let deadline = Deadline::after(Tick(100), 10)?;
        let mut meter = Meter::new(
            deadline,
            Charge {
                records: 100,
                ..Charge::default()
            },
        );
        for tick in 100..110 {
            meter.charge(
                Tick(tick),
                Charge {
                    records: 1,
                    ..Charge::default()
                },
            )?;
        }
        assert_eq!(meter.deadline(), deadline);
        assert_eq!(
            meter.charge(Tick(110), Charge::default()),
            Err(Stop::Deadline)
        );
        assert_eq!(meter.remaining().records, 90);
        assert_eq!(
            meter.charge(Tick(109), Charge::default()),
            Err(Stop::Deadline)
        );
        Ok(())
    }
}

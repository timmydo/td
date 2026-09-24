//! Bounded filesystem identity and linear probe matching. No probe syscall or
//! physical grant is implemented here; M05 supplies trusted backing identities.
use super::space::{Counters, Growth, Sample};
use crate::{
    ownership::{self, SlotId, SlotPool, SlotState},
    ports::{self, Deadline, Tick},
};

pub const MAX_FILESYSTEMS: usize = 16;
/// Adapter-assigned identity for one shared available-space budget. Equal keys
/// must mean shared backing capacity, even when paths/mount aliases differ.
/// This is not a pathname, device-number encoding, or authentication credential.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BackingKey(pub u128);
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[must_use = "retain the registered filesystem identity for its configured paths"]
pub struct FilesystemId(SlotId);
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Invalid,
    Full,
    Stale,
    Busy,
    Expired,
    Missing,
    UnitChanged,
    CounterRegression,
    Poisoned,
    Slot(ownership::Error),
    Probe(ports::Error),
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Slot(e) => e.fmt(f),
            Self::Probe(e) => e.fmt(f),
            Self::Invalid => f.write_str("invalid filesystem registration or probe"),
            Self::Full => f.write_str("filesystem registry exhausted"),
            Self::Stale => f.write_str("stale or foreign filesystem identity"),
            Self::Busy => f.write_str("filesystem still owns reservations"),
            Self::Expired => f.write_str("filesystem probe deadline expired"),
            Self::Missing => f.write_str("filesystem observation already consumed or missing"),
            Self::UnitChanged => f.write_str("filesystem allocation unit changed"),
            Self::CounterRegression => f.write_str("filesystem growth counters regressed"),
            Self::Poisoned => f.write_str("filesystem accounting stopped"),
        }
    }
}
impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Slot(e) => Some(e),
            Self::Probe(e) => Some(e),
            _ => None,
        }
    }
}
impl From<ownership::Error> for Error {
    fn from(e: ownership::Error) -> Self {
        Self::Slot(e)
    }
}

#[derive(Clone, Copy, Debug)]
struct Record {
    id: FilesystemId,
    key: BackingKey,
    unit: std::num::NonZeroU64,
    counters: Counters,
    lease_references: u16,
}
#[derive(Debug, Default)]
pub struct Cell {
    record: Option<Record>,
}
impl Cell {
    pub const EMPTY: Self = Self { record: None };
}

pub struct Registry<'a> {
    slots: SlotPool<'a>,
    cells: &'a mut [Cell],
    poisoned: bool,
}
impl<'a> Registry<'a> {
    pub fn new(states: &'a mut [SlotState], cells: &'a mut [Cell]) -> Result<Self, Error> {
        if states.len() != cells.len()
            || cells.len() > MAX_FILESYSTEMS
            || cells.iter().any(|c| c.record.is_some())
        {
            return Err(Error::Invalid);
        }
        Ok(Self {
            slots: SlotPool::new(states)?,
            cells,
            poisoned: false,
        })
    }
    fn healthy(&self) -> Result<(), Error> {
        if self.poisoned {
            Err(Error::Poisoned)
        } else {
            Ok(())
        }
    }
    fn record(&self, id: FilesystemId) -> Result<(usize, Record), Error> {
        self.healthy()?;
        let index = self.slots.resolve(id.0).map_err(|_| Error::Stale)?;
        let record = self
            .cells
            .get(index)
            .and_then(|c| c.record)
            .ok_or(Error::Stale)?;
        if record.id != id {
            return Err(Error::Stale);
        }
        Ok((index, record))
    }
    /// Aliases share the same registration. M05 must deduplicate true backing
    /// capacity before assigning a key; this helper cannot discover that fact.
    pub fn register(
        &mut self,
        key: BackingKey,
        allocation_unit: u64,
    ) -> Result<FilesystemId, Error> {
        self.healthy()?;
        let unit = std::num::NonZeroU64::new(allocation_unit).ok_or(Error::Invalid)?;
        if let Some(record) = self
            .cells
            .iter()
            .filter_map(|c| c.record)
            .find(|r| r.key == key)
        {
            return if record.unit == unit {
                Ok(record.id)
            } else {
                Err(Error::UnitChanged)
            };
        }
        if self.slots.available() == 0 {
            return Err(Error::Full);
        }
        let id = FilesystemId(self.slots.acquire()?);
        let index = match self.slots.resolve(id.0) {
            Ok(i) => i,
            Err(_) => {
                self.poisoned = true;
                return Err(Error::Poisoned);
            }
        };
        let Some(cell) = self.cells.get_mut(index) else {
            self.poisoned = true;
            return Err(Error::Poisoned);
        };
        cell.record = Some(Record {
            id,
            key,
            unit,
            counters: Counters::default(),
            lease_references: 0,
        });
        Ok(id)
    }
    /// The owner first detaches configured paths. Even a zero-growth lease must
    /// retain a reference until release; pending probes own no capacity.
    pub fn unregister(&mut self, id: FilesystemId) -> Result<(), Error> {
        let (index, record) = self.record(id)?;
        if record.lease_references != 0
            || record.counters.pending != Growth::default()
            || record.counters.checkpoint != Growth::default()
        {
            return Err(Error::Busy);
        }
        if self.slots.release(id.0).is_err() {
            self.poisoned = true;
            return Err(Error::Poisoned);
        }
        let Some(cell) = self.cells.get_mut(index) else {
            self.poisoned = true;
            return Err(Error::Poisoned);
        };
        cell.record = None;
        Ok(())
    }
    pub fn counters(&self, id: FilesystemId) -> Result<Counters, Error> {
        Ok(self.record(id)?.1.counters)
    }
    pub fn allocation_unit(&self, id: FilesystemId) -> Result<u64, Error> {
        Ok(self.record(id)?.1.unit.get())
    }
    /// Capture completed growth before the worker starts sampling. Concurrent
    /// probes may finish out of order; there is no latest-only serial fence.
    pub fn begin_probe(
        &self,
        id: FilesystemId,
        deadline: Deadline,
        now: Tick,
    ) -> Result<ProbeTicket, Error> {
        let (_, record) = self.record(id)?;
        if deadline.expired(now) {
            return Err(Error::Expired);
        }
        Ok(ProbeTicket {
            id,
            captured: record.counters.completed,
            started: now,
            deadline,
        })
    }
    /// Consume on every attempt, including refusal. The later physical grant
    /// still samples current counters and time under its exclusive coordinator.
    pub fn consume(
        &self,
        id: FilesystemId,
        observation: &mut Option<Observation>,
        now: Tick,
    ) -> Result<CheckedSample, Error> {
        let observation = observation.take().ok_or(Error::Missing)?;
        let (_, record) = self.record(id)?;
        if observation.ticket.id != id {
            return Err(Error::Stale);
        }
        if now < observation.ticket.started {
            return Err(Error::Invalid);
        }
        if observation.ticket.deadline.expired(now) {
            return Err(Error::Expired);
        }
        if observation.sample.allocation_unit != record.unit.get() {
            return Err(Error::UnitChanged);
        }
        if observation.ticket.captured.bytes > record.counters.completed.bytes
            || observation.ticket.captured.inodes > record.counters.completed.inodes
        {
            return Err(Error::CounterRegression);
        }
        Ok(CheckedSample {
            ticket: observation.ticket,
            sample: observation.sample,
        })
    }
}

#[derive(Debug)]
#[must_use = "complete the probe or discard it without granting capacity"]
pub struct ProbeTicket {
    id: FilesystemId,
    captured: Growth,
    started: Tick,
    deadline: Deadline,
}
impl ProbeTicket {
    /// The trusted adapter probes the pinned filesystem named by this ticket.
    pub fn filesystem(&self) -> FilesystemId {
        self.id
    }
    pub fn complete(self, result: Result<Sample, ports::Error>) -> Result<Observation, Error> {
        let sample = result.map_err(Error::Probe)?;
        if sample.allocation_unit == 0 {
            return Err(Error::Invalid);
        }
        Ok(Observation {
            ticket: self,
            sample,
        })
    }
}
#[derive(Debug)]
#[must_use = "consume the observation once or discard it without admission"]
pub struct Observation {
    ticket: ProbeTicket,
    sample: Sample,
}
/// Matched probe data, not a grant. Its deadline and identity must still be
/// checked at the composed atomic admission boundary; no lease is installed.
#[derive(Debug)]
#[must_use = "assess under the coordinator or discard without admission"]
pub struct CheckedSample {
    ticket: ProbeTicket,
    sample: Sample,
}
impl CheckedSample {
    pub fn filesystem(&self) -> FilesystemId {
        self.ticket.id
    }
    pub fn captured(&self) -> Growth {
        self.ticket.captured
    }
    pub fn sample(&self) -> Sample {
        self.sample
    }
    pub fn started(&self) -> Tick {
        self.ticket.started
    }
    pub fn deadline(&self) -> Deadline {
        self.ticket.deadline
    }
}

#[cfg(test)]
mod tests {
    use super::super::space::Inodes;
    use super::*;
    fn register(registry: &mut Registry<'_>, key: u128) -> Result<FilesystemId, Error> {
        for _ in 0..1000 {
            match registry.register(BackingKey(key), 4096) {
                Err(Error::Slot(ownership::Error::Contended)) => std::thread::yield_now(),
                result => return result,
            }
        }
        Err(Error::Slot(ownership::Error::Contended))
    }
    fn deadline() -> Result<Deadline, ports::Error> {
        Deadline::after(Tick(0), 10)
    }
    fn sample(bytes: u64) -> Sample {
        Sample {
            available_bytes: bytes,
            inodes: Inodes::Available(5000),
            allocation_unit: 4096,
        }
    }
    #[test]
    fn aliases_share_a_fixed_slot_and_capacity_never_grows(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mut states = [const { SlotState::EMPTY }; 16];
        let mut cells = [const { Cell::EMPTY }; 16];
        let mut registry = Registry::new(&mut states, &mut cells)?;
        let first = register(&mut registry, 100)?;
        assert_eq!(register(&mut registry, 100)?, first);
        assert_eq!(
            registry.register(BackingKey(100), 8192),
            Err(Error::UnitChanged)
        );
        for key in 101..116 {
            let _id = register(&mut registry, key)?;
        }
        assert_eq!(registry.register(BackingKey(999), 4096), Err(Error::Full));
        assert_eq!(registry.allocation_unit(first)?, 4096);
        registry.unregister(first)?;
        let replacement = register(&mut registry, 100)?;
        assert_ne!(first, replacement);
        assert_eq!(registry.counters(first), Err(Error::Stale));
        assert!(std::mem::size_of::<Cell>() + std::mem::size_of::<SlotState>() <= 128);
        assert!(
            MAX_FILESYSTEMS * (std::mem::size_of::<Cell>() + std::mem::size_of::<SlotState>())
                <= 2048
        );
        Ok(())
    }
    #[test]
    fn probes_capture_before_sampling_and_can_finish_out_of_order(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mut states = [const { SlotState::EMPTY }; 2];
        let mut cells = [const { Cell::EMPTY }; 2];
        let mut registry = Registry::new(&mut states, &mut cells)?;
        let id = register(&mut registry, 1)?;
        let first = registry.begin_probe(id, deadline()?, Tick(0))?;
        let index = registry.record(id)?.0;
        registry
            .cells
            .get_mut(index)
            .and_then(|c| c.record.as_mut())
            .ok_or("record")?
            .counters
            .completed = Growth {
            bytes: 4096,
            inodes: 1,
        };
        let second = registry.begin_probe(id, deadline()?, Tick(1))?;
        let mut newer = Some(second.complete(Ok(sample(99999)))?);
        let newer = registry.consume(id, &mut newer, Tick(2))?;
        assert_eq!(
            newer.captured(),
            Growth {
                bytes: 4096,
                inodes: 1
            }
        );
        let mut older = Some(first.complete(Ok(sample(100000)))?);
        let checked = registry.consume(id, &mut older, Tick(3))?;
        assert_eq!(checked.captured(), Growth::default());
        assert_eq!(checked.filesystem(), id);
        assert_eq!(checked.sample(), sample(100000));
        assert_eq!(checked.started(), Tick(0));
        assert_eq!(newer.started(), Tick(1));
        assert_eq!(checked.deadline(), deadline()?);
        assert!(matches!(
            registry.consume(id, &mut older, Tick(4)),
            Err(Error::Missing)
        ));
        assert_eq!(
            registry.counters(id)?.completed,
            Growth {
                bytes: 4096,
                inodes: 1
            }
        );
        Ok(())
    }
    #[test]
    fn retired_foreign_and_wrong_filesystem_observations_are_consumed(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mut sa = [const { SlotState::EMPTY }; 3];
        let mut ca = [const { Cell::EMPTY }; 3];
        let mut sb = [const { SlotState::EMPTY }; 1];
        let mut cb = [const { Cell::EMPTY }; 1];
        let mut a = Registry::new(&mut sa, &mut ca)?;
        let mut b = Registry::new(&mut sb, &mut cb)?;
        let old = register(&mut a, 1)?;
        let other = register(&mut a, 2)?;
        let foreign = register(&mut b, 1)?;
        let mut wrong = Some(
            a.begin_probe(old, deadline()?, Tick(0))?
                .complete(Ok(sample(1000)))?,
        );
        assert!(matches!(
            a.consume(other, &mut wrong, Tick(1)),
            Err(Error::Stale)
        ));
        assert!(wrong.is_none());
        let mut stale = Some(
            a.begin_probe(old, deadline()?, Tick(0))?
                .complete(Ok(sample(1000)))?,
        );
        a.unregister(old)?;
        let new = register(&mut a, 1)?;
        assert!(matches!(
            a.consume(new, &mut stale, Tick(1)),
            Err(Error::Stale)
        ));
        assert!(stale.is_none());
        let mut foreign_sample = Some(
            b.begin_probe(foreign, deadline()?, Tick(0))?
                .complete(Ok(sample(1000)))?,
        );
        assert!(matches!(
            a.consume(foreign, &mut foreign_sample, Tick(1)),
            Err(Error::Stale)
        ));
        assert!(foreign_sample.is_none());
        Ok(())
    }
    #[test]
    fn failed_expired_and_changed_unit_probes_never_become_admission_data(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mut states = [const { SlotState::EMPTY }; 2];
        let mut cells = [const { Cell::EMPTY }; 2];
        let mut registry = Registry::new(&mut states, &mut cells)?;
        let id = register(&mut registry, 1)?;
        assert!(matches!(
            registry.begin_probe(id, deadline()?, Tick(10)),
            Err(Error::Expired)
        ));
        assert!(matches!(
            registry
                .begin_probe(id, deadline()?, Tick(0))?
                .complete(Err(ports::Error::Deadline)),
            Err(Error::Probe(ports::Error::Deadline))
        ));
        let mut expired = Some(
            registry
                .begin_probe(id, deadline()?, Tick(0))?
                .complete(Ok(sample(1000)))?,
        );
        assert!(matches!(
            registry.consume(id, &mut expired, Tick(10)),
            Err(Error::Expired)
        ));
        assert!(expired.is_none());
        let mut zero = sample(1000);
        zero.allocation_unit = 0;
        assert!(matches!(
            registry
                .begin_probe(id, deadline()?, Tick(0))?
                .complete(Ok(zero)),
            Err(Error::Invalid)
        ));
        let mut changed = sample(1000);
        changed.allocation_unit = 8192;
        let mut changed = Some(
            registry
                .begin_probe(id, deadline()?, Tick(0))?
                .complete(Ok(changed))?,
        );
        assert!(matches!(
            registry.consume(id, &mut changed, Tick(1)),
            Err(Error::UnitChanged)
        ));
        assert!(changed.is_none());
        assert_eq!(registry.counters(id)?, Counters::default());
        Ok(())
    }
    #[test]
    fn outstanding_capacity_or_zero_growth_lease_references_block_retirement(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mut states = [const { SlotState::EMPTY }; 1];
        let mut cells = [const { Cell::EMPTY }; 1];
        let mut registry = Registry::new(&mut states, &mut cells)?;
        let id = register(&mut registry, 1)?;
        let index = registry.record(id)?.0;
        for (pending, checkpoint, references) in [
            (
                Growth {
                    bytes: 1,
                    inodes: 0,
                },
                Growth::default(),
                0,
            ),
            (
                Growth::default(),
                Growth {
                    bytes: 0,
                    inodes: 1,
                },
                0,
            ),
            (Growth::default(), Growth::default(), 1),
        ] {
            let record = registry
                .cells
                .get_mut(index)
                .and_then(|c| c.record.as_mut())
                .ok_or("record")?;
            record.counters.pending = pending;
            record.counters.checkpoint = checkpoint;
            record.lease_references = references;
            assert_eq!(registry.unregister(id), Err(Error::Busy));
        }
        let record = registry
            .cells
            .get_mut(index)
            .and_then(|c| c.record.as_mut())
            .ok_or("record")?;
        record.lease_references = 0;
        record.counters.completed = Growth {
            bytes: 1000,
            inodes: 1,
        };
        registry.unregister(id)?;
        assert_eq!(registry.counters(id), Err(Error::Stale));
        Ok(())
    }
    #[test]
    fn constructor_bounds_and_poisoning_are_enforced() -> Result<(), Box<dyn std::error::Error>> {
        let mut states = [const { SlotState::EMPTY }; 17];
        let mut cells = [const { Cell::EMPTY }; 17];
        assert!(matches!(
            Registry::new(&mut states, &mut cells),
            Err(Error::Invalid)
        ));
        let mut one = [const { Cell::EMPTY }; 1];
        assert!(matches!(
            Registry::new(&mut states, &mut one),
            Err(Error::Invalid)
        ));
        let mut states = [const { SlotState::EMPTY }; 1];
        let mut one = [const { Cell::EMPTY }; 1];
        let mut pool = SlotPool::new(&mut states)?;
        let mut acquired = false;
        for _ in 0..1000 {
            match pool.acquire() {
                Ok(_) => {
                    acquired = true;
                    break;
                }
                Err(ownership::Error::Contended) => std::thread::yield_now(),
                Err(e) => return Err(e.into()),
            }
        }
        if !acquired {
            return Err("slot issuer contention".into());
        }
        std::mem::forget(pool);
        assert!(matches!(
            Registry::new(&mut states, &mut one),
            Err(Error::Slot(ownership::Error::OccupiedStorage))
        ));
        for state in &mut states {
            *state = SlotState::EMPTY;
        }
        let mut registry = Registry::new(&mut states, &mut one)?;
        let id = register(&mut registry, 1)?;
        let mut observation = Some(
            registry
                .begin_probe(id, deadline()?, Tick(1))?
                .complete(Ok(sample(1000)))?,
        );
        assert!(matches!(
            registry.consume(id, &mut observation, Tick(0)),
            Err(Error::Invalid)
        ));
        let mut observation = Some(
            registry
                .begin_probe(id, deadline()?, Tick(0))?
                .complete(Ok(sample(1000)))?,
        );
        registry.poisoned = true;
        assert_eq!(registry.register(BackingKey(1), 4096), Err(Error::Poisoned));
        assert_eq!(registry.unregister(id), Err(Error::Poisoned));
        assert_eq!(registry.counters(id), Err(Error::Poisoned));
        assert_eq!(registry.allocation_unit(id), Err(Error::Poisoned));
        assert!(matches!(
            registry.begin_probe(id, deadline()?, Tick(0)),
            Err(Error::Poisoned)
        ));
        assert!(matches!(
            registry.consume(id, &mut observation, Tick(1)),
            Err(Error::Poisoned)
        ));
        assert!(observation.is_none());
        Ok(())
    }
    #[test]
    fn counter_regression_and_occupied_backing_fail_closed(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mut states = [const { SlotState::EMPTY }; 1];
        let mut cells = [const { Cell::EMPTY }; 1];
        let mut registry = Registry::new(&mut states, &mut cells)?;
        let id = register(&mut registry, 1)?;
        let index = registry.record(id)?.0;
        for regressed in [
            Growth {
                bytes: 0,
                inodes: 1,
            },
            Growth {
                bytes: 1,
                inodes: 0,
            },
        ] {
            registry
                .cells
                .get_mut(index)
                .and_then(|c| c.record.as_mut())
                .ok_or("record")?
                .counters
                .completed = Growth {
                bytes: 1,
                inodes: 1,
            };
            let mut observation = Some(
                registry
                    .begin_probe(id, deadline()?, Tick(0))?
                    .complete(Ok(sample(1000)))?,
            );
            registry
                .cells
                .get_mut(index)
                .and_then(|c| c.record.as_mut())
                .ok_or("record")?
                .counters
                .completed = regressed;
            assert!(matches!(
                registry.consume(id, &mut observation, Tick(1)),
                Err(Error::CounterRegression)
            ));
            assert!(observation.is_none());
        }
        drop(registry);
        assert!(matches!(
            Registry::new(&mut states, &mut cells),
            Err(Error::Invalid)
        ));
        // No effects exist in this fixture; emulate a reconciled cold reset.
        for cell in &mut cells {
            *cell = Cell::EMPTY;
        }
        let mut rebuilt = Registry::new(&mut states, &mut cells)?;
        let replacement = register(&mut rebuilt, 1)?;
        assert_ne!(replacement, id);
        assert_eq!(rebuilt.counters(id), Err(Error::Stale));
        Ok(())
    }
}

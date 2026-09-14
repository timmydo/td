//! Budgeted, immutable identity versions shared by retained snapshots.
use crate::budget::{Budget, Error as BudgetError, MemoryString, MemoryVec};
use crate::hierarchy::ProcessKey;
use std::sync::Arc;
pub const IDENTITIES: usize = 524288;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IdentityId {
    slot: usize,
    generation: u64,
}
#[derive(Clone, Copy, Debug)]
pub struct Spec<'a> {
    pub key: ProcessKey,
    pub name: &'a str,
}
#[derive(Debug)]
pub struct Identity {
    key: ProcessKey,
    name: MemoryString,
    references: usize,
}
impl Identity {
    pub fn key(&self) -> ProcessKey {
        self.key
    }
    pub fn name(&self) -> &str {
        self.name.as_str()
    }
    pub fn references(&self) -> usize {
        self.references
    }
}
#[derive(Debug)]
struct Slot {
    generation: u64,
    entry: Option<Identity>,
    next_free: Option<usize>,
}
#[derive(Clone, Copy, Debug)]
struct Index {
    key: ProcessKey,
    id: IdentityId,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Invalid,
    Limit,
    Budget(BudgetError),
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid => f.write_str("invalid identity reference or roster"),
            Self::Limit => f.write_str("identity table limit reached"),
            Self::Budget(error) => error.fmt(f),
        }
    }
}
impl std::error::Error for Error {}
#[derive(Debug)]
pub struct Identities {
    budget: Arc<Budget>,
    slots: MemoryVec<Slot>,
    index: MemoryVec<Index>,
    free: Option<usize>,
}
fn grow<T>(values: &mut MemoryVec<T>) -> Result<(), Error> {
    if values.len() < values.capacity() {
        return Ok(());
    }
    if values.len() >= IDENTITIES {
        return Err(Error::Limit);
    }
    values
        .reserve(values.capacity().saturating_mul(2).clamp(16, IDENTITIES))
        .map_err(Error::Budget)
}
impl Identities {
    pub fn new(budget: &Arc<Budget>) -> Result<Self, Error> {
        Ok(Self {
            budget: Arc::clone(budget),
            slots: MemoryVec::new(budget, 0).map_err(Error::Budget)?,
            index: MemoryVec::new(budget, 0).map_err(Error::Budget)?,
            free: None,
        })
    }
    pub fn get(&self, id: IdentityId) -> Option<&Identity> {
        let slot = self.slots.get(id.slot)?;
        if slot.generation != id.generation {
            return None;
        }
        slot.entry.as_ref()
    }
    fn clean_index(&mut self) {
        let slots = &self.slots;
        self.index.retain(|index| {
            slots
                .get(index.id.slot)
                .is_some_and(|slot| slot.generation == index.id.generation && slot.entry.is_some())
        });
        self.index.sort_unstable_by_key(|index| index.key);
    }
    pub fn release(&mut self, id: IdentityId) -> Result<(), Error> {
        let slot = self.slots.get_mut(id.slot).ok_or(Error::Invalid)?;
        if slot.generation != id.generation {
            return Err(Error::Invalid);
        }
        let entry = slot.entry.as_mut().ok_or(Error::Invalid)?;
        entry.references = entry.references.checked_sub(1).ok_or(Error::Invalid)?;
        if entry.references == 0 {
            slot.entry = None;
            slot.next_free = self.free;
            self.free = Some(id.slot);
        }
        Ok(())
    }
    fn intern(&mut self, spec: Spec<'_>, sorted: usize) -> Result<IdentityId, Error> {
        let index = self.index.get(..sorted).ok_or(Error::Invalid)?;
        let first = index.partition_point(|index| index.key < spec.key);
        let existing = index
            .get(first..)
            .ok_or(Error::Invalid)?
            .iter()
            .take_while(|index| index.key == spec.key)
            .find_map(|index| {
                self.get(index.id)
                    .filter(|entry| entry.name() == spec.name)
                    .map(|_| index.id)
            });
        if let Some(id) = existing {
            let entry = self
                .slots
                .get_mut(id.slot)
                .and_then(|slot| slot.entry.as_mut())
                .ok_or(Error::Invalid)?;
            entry.references = entry.references.checked_add(1).ok_or(Error::Limit)?;
            return Ok(id);
        }
        grow(&mut self.index)?;
        if self.free.is_none() {
            grow(&mut self.slots)?;
        }
        let entry = Identity {
            key: spec.key,
            name: MemoryString::new(&self.budget, spec.name).map_err(Error::Budget)?,
            references: 1,
        };
        let id = if let Some(free) = self.free {
            let slot = self.slots.get_mut(free).ok_or(Error::Invalid)?;
            let generation = slot.generation.checked_add(1).ok_or(Error::Limit)?;
            self.free = slot.next_free;
            *slot = Slot {
                generation,
                entry: Some(entry),
                next_free: None,
            };
            IdentityId {
                slot: free,
                generation,
            }
        } else {
            let id = IdentityId {
                slot: self.slots.len(),
                generation: 1,
            };
            self.slots
                .push(Slot {
                    generation: 1,
                    entry: Some(entry),
                    next_free: None,
                })
                .map_err(|_| Error::Limit)?;
            id
        };
        if self.index.push(Index { key: spec.key, id }).is_err() {
            let _ = self.release(id);
            return Err(Error::Limit);
        }
        Ok(id)
    }
    /// One unique, increasing ProcessKey per spec. Each returned ID owns one
    /// reference; release every ID when its snapshot is evicted or abandoned.
    /// Names are already escaped display text. A name change creates a version.
    pub fn intern_batch(&mut self, specs: &[Spec<'_>]) -> Result<MemoryVec<IdentityId>, Error> {
        if specs.len() > crate::hierarchy::ROWS
            || specs.iter().any(|spec| {
                spec.key.pid == 0
                    || spec.name.len() > 4096
                    || spec.name.chars().any(char::is_control)
            })
            || specs.windows(2).any(|pair| {
                pair.first()
                    .zip(pair.get(1))
                    .is_some_and(|(a, b)| a.key >= b.key)
            })
        {
            return Err(Error::Invalid);
        }
        self.clean_index();
        let sorted = self.index.len();
        let mut ids = MemoryVec::new(&self.budget, specs.len()).map_err(Error::Budget)?;
        for spec in specs.iter().copied() {
            match self.intern(spec, sorted) {
                Ok(id) => {
                    if ids.push(id).is_err() {
                        let _ = self.release(id);
                        for id in ids.iter().copied() {
                            let _ = self.release(id);
                        }
                        return Err(Error::Limit);
                    }
                }
                Err(error) => {
                    for id in ids.iter().copied() {
                        let _ = self.release(id);
                    }
                    return Err(error);
                }
            }
        }
        // New identities are sorted once, never inserted by shifting per row.
        self.index.sort_unstable_by_key(|index| index.key);
        Ok(ids)
    }
    pub fn live_entries(&self) -> usize {
        self.slots
            .iter()
            .filter(|slot| slot.entry.is_some())
            .count()
    }
}

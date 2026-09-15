//! One immutable process observation, retaining its shared identity versions.
use crate::budget::{Budget, Charge, Error as BudgetError, MemoryVec};
use crate::hierarchy::{Forest, Input, Node, ProcessKey};
use crate::identities::{Identities, Identity, IdentityId, Spec};
use std::sync::{Arc, Mutex};
#[derive(Clone, Copy, Debug)]
pub struct Observed<'a> {
    pub cpu_time_ms: Option<u64>,
    pub input: Input,
    pub name: &'a str,
    pub uid: Option<u32>,
    pub state: u8,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Process {
    pub cpu_time_ms: Option<u64>,
    pub key: ProcessKey,
    pub cpu: Option<u64>,
    pub rss: Option<u64>,
    pub uid: Option<u32>,
    pub state: u8,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Budget(BudgetError),
    Hierarchy(crate::hierarchy::Error),
    Identity(crate::identities::Error),
    Poisoned,
    InvalidTime,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Budget(error) => error.fmt(f),
            Self::Hierarchy(error) => error.fmt(f),
            Self::Identity(error) => error.fmt(f),
            Self::Poisoned => f.write_str("identity model lock is poisoned"),
            Self::InvalidTime => f.write_str("collection ends before it starts"),
        }
    }
}
impl std::error::Error for Error {}
#[derive(Debug)]
struct Shared {
    budget: Arc<Budget>,
    pool: Mutex<Identities>,
    _charge: Charge,
}
#[derive(Clone, Debug)]
pub struct IdentityStore(Arc<Shared>);
impl IdentityStore {
    pub fn new(budget: &Arc<Budget>) -> Result<Self, Error> {
        // One fixed startup Arc; variable-sized pool storage reserves fallibly.
        let charge = budget
            .charge(std::mem::size_of::<Shared>() + 2 * std::mem::size_of::<usize>())
            .map_err(Error::Budget)?;
        Ok(Self(Arc::new(Shared {
            budget: Arc::clone(budget),
            pool: Mutex::new(Identities::new(budget).map_err(Error::Identity)?),
            _charge: charge,
        })))
    }
    pub fn live_entries(&self) -> Result<usize, Error> {
        Ok(self
            .0
            .pool
            .lock()
            .map_err(|_| Error::Poisoned)?
            .live_entries())
    }
}
pub struct Names<'a> {
    pool: &'a Identities,
    identities: &'a [IdentityId],
}
impl Names<'_> {
    pub fn get(&self, row: usize) -> Option<&Identity> {
        self.identities.get(row).and_then(|id| self.pool.get(*id))
    }
}
#[derive(Debug)]
pub struct Snapshot {
    pub started_ns: u64,
    pub ended_ns: u64,
    pub partial: bool,
    processes: MemoryVec<Process>,
    forest: Forest,
    identities: MemoryVec<IdentityId>,
    store: IdentityStore,
    _charge: Charge,
}
impl Snapshot {
    pub fn new(
        store: &IdentityStore,
        rows: &[Observed<'_>],
        started_ns: u64,
        ended_ns: u64,
        partial: bool,
    ) -> Result<Self, Error> {
        let budget = &store.0.budget;
        if rows.len() > crate::hierarchy::ROWS {
            return Err(Error::Hierarchy(crate::hierarchy::Error::InvalidRows));
        }
        if ended_ns < started_ns {
            return Err(Error::InvalidTime);
        }
        let charge = budget
            .charge(std::mem::size_of::<Self>())
            .map_err(Error::Budget)?;
        let mut inputs = MemoryVec::new(budget, rows.len()).map_err(Error::Budget)?;
        let mut processes = MemoryVec::new(budget, rows.len()).map_err(Error::Budget)?;
        let mut specs = MemoryVec::new(budget, rows.len()).map_err(Error::Budget)?;
        for row in rows {
            inputs
                .push(row.input)
                .map_err(|_| Error::Budget(BudgetError::Limit))?;
            processes
                .push(Process {
                    cpu_time_ms: row.cpu_time_ms,
                    key: row.input.key,
                    cpu: row.input.cpu,
                    rss: row.input.rss,
                    uid: row.uid,
                    state: row.state,
                })
                .map_err(|_| Error::Budget(BudgetError::Limit))?;
            specs
                .push(Spec {
                    key: row.input.key,
                    name: row.name,
                })
                .map_err(|_| Error::Budget(BudgetError::Limit))?;
        }
        let forest = Forest::new(budget, &inputs, partial).map_err(Error::Hierarchy)?;
        // No fallible operation follows acquiring the row identity references.
        let identities = store
            .0
            .pool
            .lock()
            .map_err(|_| Error::Poisoned)?
            .intern_batch(&specs)
            .map_err(Error::Identity)?;
        Ok(Self {
            started_ns,
            ended_ns,
            partial,
            processes,
            forest,
            identities,
            store: store.clone(),
            _charge: charge,
        })
    }
    pub fn processes(&self) -> &[Process] {
        &self.processes
    }
    pub fn ancestry(&self) -> &[Node] {
        self.forest.nodes()
    }
    /// The callback holds the identity lock: no I/O, recursive identity access,
    /// snapshot creation or snapshot drops until it returns.
    pub fn with_identities<T>(&self, read: impl FnOnce(Names<'_>) -> T) -> Result<T, Error> {
        let pool = self.store.0.pool.lock().map_err(|_| Error::Poisoned)?;
        Ok(read(Names {
            pool: &pool,
            identities: &self.identities,
        }))
    }
}
impl Drop for Snapshot {
    fn drop(&mut self) {
        let mut pool = match self.store.0.pool.lock() {
            Ok(pool) => pool,
            Err(poisoned) => poisoned.into_inner(),
        };
        for id in self.identities.iter().copied() {
            // Private snapshot ownership releases each acquired reference once.
            let _ = pool.release(id);
        }
    }
}

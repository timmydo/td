//! Two retained generations per domain; payload validation belongs to its loader.
use crate::ports::Error;
use std::{
    marker::PhantomData,
    num::NonZeroU64,
    sync::{
        atomic::{AtomicU64, AtomicU8, Ordering},
        Arc,
    },
};

static NEXT_GENERATION: AtomicU64 = AtomicU64::new(1);
const TWO_SLOTS: u8 = 3;

/// Process-local identity; never persisted or accepted as peer authority.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GenerationId(NonZeroU64);

impl GenerationId {
    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

struct Budget {
    occupied: AtomicU8,
}

struct Permit {
    budget: Arc<Budget>,
    bit: u8,
}

impl Permit {
    fn reserve(budget: &Arc<Budget>) -> Result<Self, Error> {
        let occupied = budget.occupied.load(Ordering::Acquire);
        let free = !occupied & TWO_SLOTS;
        let bit = 1u8.checked_shl(free.trailing_zeros()).ok_or(Error::Busy)?;
        budget
            .occupied
            .compare_exchange(
                occupied,
                occupied | bit,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map_err(|_| Error::Busy)?;
        Ok(Self {
            budget: Arc::clone(budget),
            bit,
        })
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        // Payload destruction precedes release; a later reservation acquires it.
        self.budget.occupied.fetch_and(!self.bit, Ordering::Release);
    }
}

struct Entry<T> {
    id: GenerationId,
    value: Box<T>,
    // Field drop order keeps capacity charged until value has been destroyed.
    permit: Permit,
}

/// A retained payload. Cloning retains its existing generation, without growing
/// backing storage. The runtime separately bounds the number of live leases.
pub struct GenerationLease<T> {
    entry: Arc<Entry<T>>,
}

impl<T> Clone for GenerationLease<T> {
    fn clone(&self) -> Self {
        Self {
            entry: Arc::clone(&self.entry),
        }
    }
}

impl<T> GenerationLease<T> {
    pub fn id(&self) -> GenerationId {
        self.entry.id
    }

    /// The loader owns payload validity and immutability. Resources extracted
    /// or cloned from the payload must not outlive their generation lease.
    pub fn value(&self) -> &T {
        &self.entry.value
    }
}

impl<T> std::fmt::Debug for GenerationLease<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GenerationLease")
            .field("id", &self.id())
            .finish_non_exhaustive()
    }
}

/// Reserved capacity and captured base, without a borrow of the set. Move this
/// owner to the cold control worker before loading any generation material.
#[must_use = "construct or drop the reserved generation"]
pub struct GenerationConstruction<T> {
    id: GenerationId,
    base: Option<GenerationId>,
    permit: Permit,
    payload: PhantomData<fn() -> T>,
}

impl<T> GenerationConstruction<T> {
    /// The loader builds its boxed payload and temporaries inside this slot.
    /// It must bound its own stack use; this API does not pass T by value.
    /// Returned failure releases capacity; unexpected loader/destructor panic
    /// follows fatal service policy. Success allocates the shared owner cold.
    pub fn construct(
        self,
        load: impl FnOnce() -> Result<Box<T>, Error>,
    ) -> Result<PreparedGeneration<T>, Error> {
        let value = load()?;
        Ok(PreparedGeneration {
            base: self.base,
            retained: GenerationLease {
                entry: Arc::new(Entry {
                    id: self.id,
                    value,
                    permit: self.permit,
                }),
            },
        })
    }
}

impl<T> std::fmt::Debug for GenerationConstruction<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("GenerationConstruction(<redacted>)")
    }
}

/// One unpublished payload, charged against the same two-generation domain.
/// Publication consumes it. Drop discards it and eventually returns capacity.
///
/// ```compile_fail,E0277
/// use td_mta::generations::PreparedGeneration;
/// fn requires_clone<T: Clone>() {}
/// requires_clone::<PreparedGeneration<u8>>();
/// ```
#[must_use = "publish or drop the prepared generation"]
pub struct PreparedGeneration<T> {
    base: Option<GenerationId>,
    retained: GenerationLease<T>,
}

impl<T> std::fmt::Debug for PreparedGeneration<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PreparedGeneration(<redacted>)")
    }
}

/// Publication changed nothing. Send this candidate to the control worker for
/// disposal or recover it with into_prepared; failure does not lose its owner.
#[must_use = "recover or schedule disposal of the refused candidate"]
pub struct PublicationRefusal<T> {
    error: Error,
    prepared: PreparedGeneration<T>,
}

impl<T> PublicationRefusal<T> {
    pub const fn error(&self) -> Error {
        self.error
    }

    pub fn into_prepared(self) -> PreparedGeneration<T> {
        self.prepared
    }
}

impl<T> std::fmt::Debug for PublicationRefusal<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PublicationRefusal")
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}

/// A publication's former active owner, possibly empty on initial publication.
/// The coordinator must send this wrapper to the control worker for disposal;
/// explicit Drop still runs on the calling thread, with no automatic handoff.
///
/// ```compile_fail
/// #![deny(unused_must_use)]
/// use td_mta::generations::{GenerationSet, PreparedGeneration, PublicationRefusal};
/// fn ignored(mut set: GenerationSet<u8>, p: PreparedGeneration<u8>)
///     -> Result<(), PublicationRefusal<u8>> {
///     set.publish(p)?;
///     Ok(())
/// }
/// ```
#[must_use = "send the retired owner to the control worker before disposal"]
pub struct RetiredGeneration<T>(Option<GenerationLease<T>>);

impl<T> RetiredGeneration<T> {
    #[must_use = "retain or explicitly dispose of the former active lease"]
    pub fn into_lease(self) -> Option<GenerationLease<T>> {
        self.0
    }
}

impl<T> std::fmt::Debug for RetiredGeneration<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RetiredGeneration(<redacted>)")
    }
}

/// One startup retention domain. Current, retired, reserved and prepared owners
/// occupy at most two slots. A new set is a separate domain, never a way to
/// replace a live set while bypassing its retained resources.
///
/// T must own a bounded payload with bounded destruction. This counts owners,
/// not bytes: loaders charge raw input, parsed objects and all temporary/native
/// allocations to the existing configuration/certificate ledger. No snapshot
/// validation, TLS authorization, reload deadline or worker scheduling is here.
///
/// ```compile_fail,E0277
/// use td_mta::generations::GenerationSet;
/// fn requires_default<T: Default>() {}
/// requires_default::<GenerationSet<u8>>();
/// ```
pub struct GenerationSet<T> {
    budget: Arc<Budget>,
    current: Option<GenerationLease<T>>,
}

impl<T> GenerationSet<T> {
    /// Cold startup allocation; std allocation failure follows process policy.
    pub fn at_startup() -> Self {
        Self {
            budget: Arc::new(Budget {
                occupied: AtomicU8::new(0),
            }),
            current: None,
        }
    }

    /// Observation only; never permission to construct a candidate.
    pub fn available(&self) -> usize {
        (!self.budget.occupied.load(Ordering::Acquire) & TWO_SLOTS).count_ones() as usize
    }

    pub fn current(&self) -> Option<GenerationLease<T>> {
        self.current.clone()
    }

    /// Bounded reservation and ID issuance, each with one CAS attempt. Busy
    /// includes contention from another domain using the process-wide ID source.
    /// Retry on a later turn; never spin or construct without a reservation.
    pub fn reserve(&self) -> Result<GenerationConstruction<T>, Error> {
        self.reserve_using(&NEXT_GENERATION)
    }

    /// Cold convenience for a control worker that already owns the set. Main
    /// uses reserve and hands off that owner without holding a lock across I/O.
    pub fn prepare(
        &self,
        load: impl FnOnce() -> Result<Box<T>, Error>,
    ) -> Result<PreparedGeneration<T>, Error> {
        self.reserve()?.construct(load)
    }

    fn reserve_using(&self, counter: &AtomicU64) -> Result<GenerationConstruction<T>, Error> {
        let permit = Permit::reserve(&self.budget)?;
        let current = counter.load(Ordering::Relaxed);
        let id = NonZeroU64::new(current).ok_or(Error::Capacity)?;
        let next = current.checked_add(1).ok_or(Error::Capacity)?;
        // Uniqueness only; payload publication uses exclusive set ownership.
        counter
            .compare_exchange(current, next, Ordering::Relaxed, Ordering::Relaxed)
            .map_err(|_| Error::Busy)?;
        Ok(GenerationConstruction {
            id: GenerationId(id),
            base: self.current.as_ref().map(GenerationLease::id),
            permit,
            payload: PhantomData,
        })
    }

    /// Publish under the coordinator's exclusive ownership. A candidate is
    /// valid only in its original domain and against its captured active base.
    /// The coordinator may be main: move the returned retired-owner wrapper to
    /// the control worker before disposing of it. Other leases can still keep
    /// it charged. Publication itself neither clones nor destroys a payload.
    pub fn publish(
        &mut self,
        prepared: PreparedGeneration<T>,
    ) -> Result<RetiredGeneration<T>, PublicationRefusal<T>> {
        let error = if !Arc::ptr_eq(&self.budget, &prepared.retained.entry.permit.budget) {
            Some(Error::Forbidden)
        } else if self.current.as_ref().map(GenerationLease::id) != prepared.base {
            Some(Error::Conflict)
        } else {
            None
        };
        if let Some(error) = error {
            return Err(PublicationRefusal { error, prepared });
        }
        Ok(RetiredGeneration(self.current.replace(prepared.retained)))
    }
}

#[cfg(test)]
#[path = "generations_tests.rs"]
mod tests;

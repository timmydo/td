//! Fixed bookkeeping for observed C allocation boundaries, without pointer access.
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

const EMPTY: usize = 0;
const DELETED: usize = 1;

struct Entry {
    address: AtomicUsize,
    bytes: AtomicUsize,
}
impl Entry {
    const fn new() -> Self {
        Self {
            address: AtomicUsize::new(EMPTY),
            bytes: AtomicUsize::new(0),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    InvalidAddress,
    Duplicate,
    Unknown,
    Full,
    Arithmetic,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Snapshot {
    pub blocks: usize,
    pub bytes: usize,
    pub peak: usize,
    pub invalid: bool,
}

pub struct Registry<const N: usize> {
    entries: [Entry; N],
    locked: AtomicBool,
    blocks: AtomicUsize,
    bytes: AtomicUsize,
    peak: AtomicUsize,
    invalid: AtomicBool,
}

struct Guard<'a, const N: usize>(&'a Registry<N>);
impl<const N: usize> Drop for Guard<'_, N> {
    fn drop(&mut self) {
        self.0.locked.store(false, Ordering::Release);
    }
}

impl<const N: usize> Registry<N> {
    pub const fn new() -> Self {
        Self {
            entries: [const { Entry::new() }; N],
            locked: AtomicBool::new(false),
            blocks: AtomicUsize::new(0),
            bytes: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            invalid: AtomicBool::new(false),
        }
    }

    fn lock(&self) -> Guard<'_, N> {
        while self
            .locked
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            std::hint::spin_loop();
        }
        Guard(self)
    }

    fn fail<T>(&self, error: Error) -> Result<T, Error> {
        self.invalid.store(true, Ordering::Relaxed);
        Err(error)
    }

    // Returns an existing entry or a vacancy; deletion must retain probe chains.
    fn locate(&self, address: usize) -> Result<(Option<usize>, Option<usize>), Error> {
        if address <= DELETED {
            return Err(Error::InvalidAddress);
        }
        if N == 0 {
            return Err(Error::Full);
        }
        let mut index = address.rotate_right(4) % N;
        let mut vacancy = None;
        for _ in 0..N {
            let entry = self.entries.get(index).ok_or(Error::Arithmetic)?;
            match entry.address.load(Ordering::Relaxed) {
                current if current == address => return Ok((Some(index), vacancy)),
                EMPTY => return Ok((None, vacancy.or(Some(index)))),
                DELETED => {
                    vacancy = vacancy.or(Some(index));
                }
                _ => {}
            }
            index = if index == N.saturating_sub(1) {
                0
            } else {
                index.saturating_add(1)
            };
        }
        Ok((None, vacancy))
    }

    pub fn insert(&self, address: usize, size: usize) -> Result<(), Error> {
        let _guard = self.lock();
        let result = (|| {
            let (found, vacant) = self.locate(address)?;
            if found.is_some() {
                return Err(Error::Duplicate);
            }
            let entry = self
                .entries
                .get(vacant.ok_or(Error::Full)?)
                .ok_or(Error::Arithmetic)?;
            let blocks = self
                .blocks
                .load(Ordering::Relaxed)
                .checked_add(1)
                .ok_or(Error::Arithmetic)?;
            let bytes = self
                .bytes
                .load(Ordering::Relaxed)
                .checked_add(size)
                .ok_or(Error::Arithmetic)?;
            entry.bytes.store(size, Ordering::Relaxed);
            entry.address.store(address, Ordering::Relaxed);
            self.blocks.store(blocks, Ordering::Relaxed);
            self.bytes.store(bytes, Ordering::Relaxed);
            self.peak.fetch_max(bytes, Ordering::Relaxed);
            Ok(())
        })();
        result.or_else(|error| self.fail(error))
    }

    // Remove before the real free/realloc, so another thread may reuse the address.
    // Reinsert the old address and returned size if a nonzero realloc fails.
    pub fn remove(&self, address: usize) -> Result<usize, Error> {
        let _guard = self.lock();
        let result = (|| {
            let (found, _) = self.locate(address)?;
            let entry = self
                .entries
                .get(found.ok_or(Error::Unknown)?)
                .ok_or(Error::Arithmetic)?;
            let size = entry.bytes.load(Ordering::Relaxed);
            let blocks = self
                .blocks
                .load(Ordering::Relaxed)
                .checked_sub(1)
                .ok_or(Error::Arithmetic)?;
            let bytes = self
                .bytes
                .load(Ordering::Relaxed)
                .checked_sub(size)
                .ok_or(Error::Arithmetic)?;
            entry.address.store(DELETED, Ordering::Relaxed);
            entry.bytes.store(0, Ordering::Relaxed);
            self.blocks.store(blocks, Ordering::Relaxed);
            self.bytes.store(bytes, Ordering::Relaxed);
            Ok(size)
        })();
        result.or_else(|error| self.fail(error))
    }

    pub fn snapshot(&self) -> Snapshot {
        let _guard = self.lock();
        Snapshot {
            blocks: self.blocks.load(Ordering::Relaxed),
            bytes: self.bytes.load(Ordering::Relaxed),
            peak: self.peak.load(Ordering::Relaxed),
            invalid: self.invalid.load(Ordering::Relaxed),
        }
    }
}

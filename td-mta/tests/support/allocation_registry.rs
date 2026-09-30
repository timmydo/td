//! Fixed bookkeeping for observed C allocation boundaries, without pointer access.
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

const EMPTY: usize = 0;
const RESERVED: usize = 1;

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

    pub fn invalidate(&self) {
        self.invalid.store(true, Ordering::Relaxed);
    }

    fn fail<T>(&self, error: Error) -> Result<T, Error> {
        self.invalidate();
        Err(error)
    }

    // Returns an existing entry or a vacancy; deletion closes gaps in probe chains.
    fn locate(&self, address: usize) -> Result<(Option<usize>, Option<usize>), Error> {
        if address <= RESERVED {
            return Err(Error::InvalidAddress);
        }
        if N == 0 {
            return Err(Error::Full);
        }
        let mut index = address.rotate_right(4) % N;
        for _ in 0..N {
            let entry = self.entries.get(index).ok_or(Error::Arithmetic)?;
            match entry.address.load(Ordering::Relaxed) {
                current if current == address => return Ok((Some(index), None)),
                EMPTY => return Ok((None, Some(index))),
                _ => {}
            }
            index = Self::next(index);
        }
        Ok((None, None))
    }

    fn next(index: usize) -> usize {
        if index == N.saturating_sub(1) {
            0
        } else {
            index.saturating_add(1)
        }
    }

    fn distance(start: usize, end: usize) -> usize {
        if end >= start {
            end - start
        } else {
            N - (start - end)
        }
    }

    // Moving later entries backward leaves a genuine empty slot without breaking
    // searches whose home bucket lies before the removed entry, including wrap.
    fn close_gap(&self, mut hole: usize) {
        if N == 0 {
            self.invalid.store(true, Ordering::Relaxed);
            return;
        }
        // A reachable full table filled a last empty slot that no probe chain
        // crossed; one traversal of the other N-1 slots therefore suffices.
        let mut scan = Self::next(hole);
        for _ in 0..N.saturating_sub(1) {
            let Some(entry) = self.entries.get(scan) else {
                self.invalid.store(true, Ordering::Relaxed);
                return;
            };
            let address = entry.address.load(Ordering::Relaxed);
            if address == EMPTY {
                break;
            }
            let home = address.rotate_right(4) % N;
            if Self::distance(home, hole) < Self::distance(home, scan) {
                let Some(target) = self.entries.get(hole) else {
                    self.invalid.store(true, Ordering::Relaxed);
                    return;
                };
                target.address.store(address, Ordering::Relaxed);
                target
                    .bytes
                    .store(entry.bytes.load(Ordering::Relaxed), Ordering::Relaxed);
                hole = scan;
            }
            scan = Self::next(scan);
        }
        if let Some(entry) = self.entries.get(hole) {
            entry.address.store(EMPTY, Ordering::Relaxed);
            entry.bytes.store(0, Ordering::Relaxed);
        } else {
            self.invalid.store(true, Ordering::Relaxed);
        }
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
            let index = found.ok_or(Error::Unknown)?;
            let entry = self.entries.get(index).ok_or(Error::Arithmetic)?;
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
            self.close_gap(index);
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

#[cfg(test)]
mod tests {
    #[test]
    fn churn_returns_every_slot_to_empty() {
        use super::*;
        let r = Registry::<17>::new();
        for address in 2..4096 {
            assert_eq!(r.insert(address, 13), Ok(()));
            assert_eq!(r.remove(address), Ok(13));
            assert!(r
                .entries
                .iter()
                .all(|entry| entry.address.load(Ordering::Relaxed) == EMPTY));
            assert_eq!(
                r.locate(address),
                Ok((None, Some(address.rotate_right(4) % 17)))
            );
        }
        assert_eq!(
            r.snapshot(),
            Snapshot {
                blocks: 0,
                bytes: 0,
                peak: 13,
                invalid: false
            }
        );
    }
}

//! Requested Rust bytes, excluding allocator overhead and native allocations.
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Snapshot {
    pub alloc: usize,
    pub zeroed: usize,
    pub realloc: usize,
    pub free: usize,
    pub failed: usize,
    pub live: usize,
    pub peak: usize,
    pub invalid: bool,
}

pub struct Counters {
    alloc: AtomicUsize,
    zeroed: AtomicUsize,
    realloc: AtomicUsize,
    free: AtomicUsize,
    failed: AtomicUsize,
    live: AtomicUsize,
    peak: AtomicUsize,
    invalid: AtomicBool,
}

impl Counters {
    pub const fn new() -> Self {
        Self {
            alloc: AtomicUsize::new(0),
            zeroed: AtomicUsize::new(0),
            realloc: AtomicUsize::new(0),
            free: AtomicUsize::new(0),
            failed: AtomicUsize::new(0),
            live: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            invalid: AtomicBool::new(false),
        }
    }

    fn add(&self, field: &AtomicUsize, n: usize) {
        if update(field, |v| v.checked_add(n)).is_err() {
            self.invalid.store(true, Relaxed);
        }
    }

    fn subtract(&self, n: usize) {
        if update(&self.live, |v| v.checked_sub(n)).is_err() {
            self.invalid.store(true, Relaxed);
        }
    }

    fn grow(&self, n: usize) {
        match update(&self.live, |v| v.checked_add(n)) {
            Ok(old) => {
                self.peak.fetch_max(old.saturating_add(n), Relaxed);
            }
            Err(_) => self.invalid.store(true, Relaxed),
        }
    }

    pub fn allocated(&self, size: usize, zeroed: bool, success: bool) {
        self.add(if zeroed { &self.zeroed } else { &self.alloc }, 1);
        if success {
            self.grow(size);
        } else {
            self.add(&self.failed, 1);
        }
    }

    pub fn freed(&self, size: usize) {
        self.add(&self.free, 1);
        self.subtract(size);
    }

    pub fn resized(&self, old: usize, new: usize, success: bool) {
        self.add(&self.realloc, 1);
        if !success {
            self.add(&self.failed, 1);
        } else if let Some(growth) = new.checked_sub(old) {
            self.grow(growth);
        } else if let Some(shrink) = old.checked_sub(new) {
            self.subtract(shrink);
        }
    }

    // Read only at quiescent boundaries; the fields are not one atomic snapshot.
    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            alloc: self.alloc.load(Relaxed),
            zeroed: self.zeroed.load(Relaxed),
            realloc: self.realloc.load(Relaxed),
            free: self.free.load(Relaxed),
            failed: self.failed.load(Relaxed),
            live: self.live.load(Relaxed),
            peak: self.peak.load(Relaxed),
            invalid: self.invalid.load(Relaxed),
        }
    }

    pub fn verify_model() {
        let c = Self::new();
        c.allocated(16, false, true);
        c.allocated(32, true, true);
        c.allocated(999, false, false);
        c.resized(16, 64, true);
        c.resized(64, 1024, false);
        c.resized(64, 8, true);
        c.freed(8);
        c.freed(32);
        assert_eq!(
            c.snapshot(),
            Snapshot {
                alloc: 2,
                zeroed: 1,
                realloc: 3,
                free: 2,
                failed: 2,
                live: 0,
                peak: 96,
                invalid: false
            }
        );
        c.freed(1);
        assert!(c.snapshot().invalid);
        c.allocated(1, false, true);
        assert!(c.snapshot().invalid);
        let c = Self::new();
        c.allocated(usize::MAX, false, true);
        c.allocated(1, false, true);
        assert!(c.snapshot().invalid);
        let c = Self::new();
        c.alloc.store(usize::MAX, Relaxed);
        c.allocated(1, false, false);
        assert!(c.snapshot().invalid);
    }
}

fn update(field: &AtomicUsize, change: impl Fn(usize) -> Option<usize>) -> Result<usize, ()> {
    let mut current = field.load(Relaxed);
    loop {
        let next = change(current).ok_or(())?;
        match field.compare_exchange_weak(current, next, Relaxed, Relaxed) {
            Ok(old) => return Ok(old),
            Err(actual) => current = actual,
        }
    }
}

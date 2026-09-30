#![cfg(test)]
#![forbid(unsafe_code)]
#![allow(clippy::unwrap_used, clippy::panic)]
#[path = "support/allocation_registry.rs"]
mod registry;
use registry::{Error, Registry, Snapshot};

#[test]
fn collision_chains_tombstones_zero_sizes_and_full_table() {
    let r = Registry::<4>::new();
    // All these addresses collide after rotation and modulo four.
    for (address, size) in [(64, 0), (128, 3), (192, 7), (256, 11)] {
        r.insert(address, size).unwrap();
    }
    assert_eq!(r.remove(64), Ok(0));
    assert_eq!(r.remove(192), Ok(7));
    assert_eq!(r.remove(256), Ok(11));
    assert_eq!(r.remove(128), Ok(3));
    assert_eq!(
        r.snapshot(),
        Snapshot {
            blocks: 0,
            bytes: 0,
            peak: 21,
            invalid: false
        }
    );
    for address in [320, 384, 448, 512] {
        r.insert(address, 1).unwrap();
    }
    assert_eq!(r.insert(576, 1), Err(Error::Full));
    assert_eq!(
        r.snapshot(),
        Snapshot {
            blocks: 4,
            bytes: 4,
            peak: 21,
            invalid: true
        }
    );
}

#[test]
fn malformed_operations_retire_evidence_without_corrupting_ownership() {
    for address in [0, 1] {
        let r = Registry::<4>::new();
        assert_eq!(r.insert(address, 1), Err(Error::InvalidAddress));
        assert!(r.snapshot().invalid);
    }
    let r = Registry::<0>::new();
    assert_eq!(r.insert(64, 1), Err(Error::Full));
    assert!(r.snapshot().invalid);
    let r = Registry::<4>::new();
    r.insert(64, 9).unwrap();
    assert_eq!(r.insert(64, 1), Err(Error::Duplicate));
    assert!(r.snapshot().invalid);
    assert_eq!(r.remove(128), Err(Error::Unknown));
    assert_eq!(r.remove(64), Ok(9));
    assert_eq!(r.remove(64), Err(Error::Unknown));
    assert_eq!(
        r.snapshot(),
        Snapshot {
            blocks: 0,
            bytes: 0,
            peak: 9,
            invalid: true
        }
    );
    let r = Registry::<4>::new();
    r.insert(64, usize::MAX).unwrap();
    assert_eq!(r.insert(128, 1), Err(Error::Arithmetic));
    assert_eq!(r.remove(128), Err(Error::Unknown));
    assert_eq!(r.remove(64), Ok(usize::MAX));
    assert_eq!(r.snapshot().blocks, 0);
    assert!(r.snapshot().invalid);
}

#[test]
fn duplicate_beyond_a_tombstone_retires_evidence_without_insertion() {
    let r = Registry::<4>::new();
    r.insert(64, 7).unwrap();
    r.insert(128, 9).unwrap();
    assert_eq!(r.remove(64), Ok(7));
    assert_eq!(r.insert(128, 1), Err(Error::Duplicate));
    assert_eq!(
        r.snapshot(),
        Snapshot {
            blocks: 1,
            bytes: 9,
            peak: 16,
            invalid: true,
        }
    );
    assert_eq!(r.remove(128), Ok(9));
    assert_eq!(r.snapshot().blocks, 0);
}

#[test]
fn replacement_failure_and_reused_old_addresses_keep_distinct_owners() {
    let r = Registry::<4>::new();
    r.insert(64, 8).unwrap();
    let old = r.remove(64).unwrap();
    r.insert(64, old).unwrap(); // simulated failed nonzero realloc
    assert_eq!(r.remove(64), Ok(8));
    r.insert(64, 99).unwrap(); // another allocation reused the freed old address
    r.insert(128, 16).unwrap(); // successful moved realloc returned a new address
    assert_eq!(r.remove(128), Ok(16));
    assert_eq!(r.remove(64), Ok(99));
    assert_eq!(
        r.snapshot(),
        Snapshot {
            blocks: 0,
            bytes: 0,
            peak: 115,
            invalid: false
        }
    );
}

#[test]
fn concurrent_owners_release_all_entries_and_smoke_check_snapshots() {
    let r = Registry::<8>::new();
    std::thread::scope(|scope| {
        for address in [64, 128, 192, 256] {
            let r = &r;
            scope.spawn(move || {
                for _ in 0..2048 {
                    r.insert(address, 13).unwrap();
                    let s = r.snapshot();
                    assert_eq!(s.bytes, s.blocks * 13);
                    assert!(!s.invalid);
                    assert_eq!(r.remove(address), Ok(13));
                }
            });
        }
    });
    let s = r.snapshot();
    assert_eq!(s.blocks, 0);
    assert_eq!(s.bytes, 0);
    assert!((13..=52).contains(&s.peak));
    assert!(!s.invalid);
}

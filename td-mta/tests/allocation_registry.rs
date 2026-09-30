#![cfg(test)]
#![forbid(unsafe_code)]
#![allow(clippy::unwrap_used, clippy::panic)]
#[path = "support/allocation_registry.rs"]
mod registry;
use registry::{Error, Registry, Snapshot};

#[test]
fn collision_chains_closed_gaps_zero_sizes_and_full_table() {
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
fn duplicate_beyond_a_removed_slot_retires_evidence_without_insertion() {
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

#[test]
fn wraparound_cluster_and_home_entries_survive_deletion() {
    let r = Registry::<4>::new();
    // Homes 3, 0, 3, 1: deletion must skip the home-zero entry and move
    // the displaced home-three entry backward across the table boundary.
    for (address, size) in [(48, 11), (64, 13), (112, 17), (16, 19)] {
        r.insert(address, size).unwrap();
    }
    assert_eq!(r.remove(48), Ok(11));
    for (address, size) in [(112, 17), (64, 13), (16, 19)] {
        assert_eq!(r.remove(address), Ok(size));
    }
    assert_eq!(
        r.snapshot(),
        Snapshot {
            blocks: 0,
            bytes: 0,
            peak: 60,
            invalid: false
        }
    );
}

#[test]
fn deterministic_operation_traces_match_an_independent_map() {
    use std::collections::BTreeMap;
    fn exercise<const N: usize>() {
        let r = Registry::<N>::new();
        let mut owners = BTreeMap::<usize, usize>::new();
        let mut state = 0x1234_5678u64;
        let mut peak = 0usize;
        let mut invalid = false;
        for step in 0..100_000usize {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            let address = ((state >> 32) % 23) as usize;
            let address = if address < 2 { address } else { address * 16 };
            let size = if step % 257 == 0 {
                usize::MAX
            } else {
                ((state >> 48) % 32) as usize
            };
            let bytes: usize = owners.values().sum();
            if state >> 63 == 0 {
                let expected = if address <= 1 {
                    Err(Error::InvalidAddress)
                } else if owners.contains_key(&address) {
                    Err(Error::Duplicate)
                } else if owners.len() == N {
                    Err(Error::Full)
                } else if bytes.checked_add(size).is_none() {
                    Err(Error::Arithmetic)
                } else {
                    owners.insert(address, size);
                    peak = peak.max(bytes + size);
                    Ok(())
                };
                invalid |= expected.is_err();
                assert_eq!(
                    r.insert(address, size),
                    expected,
                    "capacity={N}, step={step}"
                );
            } else {
                let expected = if address <= 1 {
                    Err(Error::InvalidAddress)
                } else if N == 0 {
                    Err(Error::Full)
                } else {
                    owners.remove(&address).ok_or(Error::Unknown)
                };
                invalid |= expected.is_err();
                assert_eq!(r.remove(address), expected, "capacity={N}, step={step}");
            }
            assert_eq!(
                r.snapshot(),
                Snapshot {
                    blocks: owners.len(),
                    bytes: owners.values().sum(),
                    peak,
                    invalid
                },
                "capacity={N}, step={step}"
            );
        }
    }
    exercise::<0>();
    exercise::<1>();
    exercise::<3>();
    exercise::<4>();
    exercise::<7>();
}

#[test]
fn valid_operation_traces_never_retire_evidence() {
    use std::collections::BTreeMap;
    fn exercise<const N: usize>() {
        let r = Registry::<N>::new();
        let mut owners = BTreeMap::<usize, usize>::new();
        let mut state = 0x7654_3210u64;
        let mut peak = 0usize;
        for step in 0..50_000usize {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            if owners.is_empty() || (state >> 63 == 0 && owners.len() < N) {
                let mut address = ((state >> 32) as usize % 23 + 2) * 16;
                while owners.contains_key(&address) {
                    address += 16;
                }
                let size = ((state >> 48) % 32) as usize;
                owners.insert(address, size);
                peak = peak.max(owners.values().sum());
                assert_eq!(r.insert(address, size), Ok(()), "capacity={N}, step={step}");
            } else {
                let index = (state >> 32) as usize % owners.len();
                let (&address, &size) = owners.iter().nth(index).unwrap();
                owners.remove(&address);
                assert_eq!(r.remove(address), Ok(size), "capacity={N}, step={step}");
            }
            assert_eq!(
                r.snapshot(),
                Snapshot {
                    blocks: owners.len(),
                    bytes: owners.values().sum(),
                    peak,
                    invalid: false
                },
                "capacity={N}, step={step}"
            );
        }
    }
    exercise::<1>();
    exercise::<3>();
    exercise::<4>();
    exercise::<7>();
}

#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use td_taskmgr::budget::Budget;
use td_taskmgr::hierarchy::ProcessKey;
use td_taskmgr::identities::{Identities, Spec};
fn spec(pid: u32, name: &str) -> Spec<'_> {
    Spec {
        key: ProcessKey {
            generation: 1,
            pid,
            start_ticks: 100,
        },
        name,
    }
}
#[test]
fn identity_versions_share_names_until_last_snapshot_releases_them() {
    let budget = Budget::new(1024 * 1024).unwrap();
    let mut pool = Identities::new(&budget).unwrap();
    let first = pool
        .intern_batch(&[spec(1, "before"), spec(2, "worker")])
        .unwrap();
    let second = pool
        .intern_batch(&[spec(1, "after"), spec(2, "worker")])
        .unwrap();
    assert_ne!(first[0], second[0]);
    assert_eq!(first[1], second[1]);
    assert_eq!(pool.get(first[0]).unwrap().name(), "before");
    assert_eq!(pool.get(second[0]).unwrap().name(), "after");
    assert_eq!(pool.get(first[1]).unwrap().references(), 2);
    for id in first.iter().copied() {
        pool.release(id).unwrap();
    }
    assert_eq!(pool.live_entries(), 2);
    assert!(pool.get(first[0]).is_none());
    for id in second.iter().copied() {
        pool.release(id).unwrap();
    }
    assert_eq!(pool.live_entries(), 0);
}
#[test]
fn reused_slots_do_not_reinterpret_old_ids_or_reused_process_pids() {
    let budget = Budget::new(1024 * 1024).unwrap();
    let mut pool = Identities::new(&budget).unwrap();
    let first = pool.intern_batch(&[spec(1, "worker")]).unwrap()[0];
    pool.release(first).unwrap();
    let mut reused = spec(1, "worker");
    reused.key.start_ticks += 1;
    let second = pool.intern_batch(&[reused]).unwrap()[0];
    assert_ne!(first, second);
    assert!(pool.get(first).is_none());
    assert!(pool.release(first).is_err());
    assert_eq!(pool.get(second).unwrap().key().start_ticks, 101);
}
#[test]
fn partial_batch_failure_releases_new_and_existing_references() {
    let budget = Budget::new(5000).unwrap();
    let mut pool = Identities::new(&budget).unwrap();
    let first = pool.intern_batch(&[spec(1, "held")]).unwrap()[0];
    let text = "x".repeat(4096);
    assert!(pool
        .intern_batch(&[spec(1, "held"), spec(2, &text)])
        .is_err());
    assert_eq!(pool.live_entries(), 1);
    assert_eq!(pool.get(first).unwrap().references(), 1);
    assert!(pool.intern_batch(&[spec(2, "a"), spec(1, "b")]).is_err());
    assert!(budget.peak() <= budget.maximum());
}
#[test]
fn churn_is_bounded_and_slot_storage_is_reused() {
    let budget = Budget::new(16 * 1024 * 1024).unwrap();
    let mut pool = Identities::new(&budget).unwrap();
    for round in 0..4 {
        let specs = (1..=32768)
            .map(|pid| spec(pid + round * 32768, "worker"))
            .collect::<Vec<_>>();
        let ids = pool.intern_batch(&specs).unwrap();
        assert_eq!(pool.live_entries(), 32768);
        for id in ids.iter().copied() {
            pool.release(id).unwrap();
        }
        assert_eq!(pool.live_entries(), 0);
    }
    assert!(budget.peak() <= budget.maximum());
}

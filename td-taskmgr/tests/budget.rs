#![allow(clippy::unwrap_used, clippy::panic)]
use td_taskmgr::budget::{Budget, Error, MemoryString, MemoryVec};
#[test]
fn quota_precedes_growth_and_refusal_keeps_values_and_accounting() {
    let budget = Budget::new(512).unwrap();
    let base = budget.used();
    let mut rows = MemoryVec::new(&budget, 4).unwrap();
    for n in 0..4u64 {
        rows.push(n).unwrap();
    }
    assert_eq!(rows.push(4), Err(4));
    let used = budget.used();
    assert_eq!(rows.reserve(1000), Err(Error::Limit));
    assert_eq!(&*rows, &[0, 1, 2, 3]);
    assert_eq!(budget.used(), used);
    rows.reserve(8).unwrap();
    assert_eq!(&*rows, &[0, 1, 2, 3]);
    assert!(rows.capacity() >= 8);
    assert!(budget.peak() <= budget.maximum());
    drop(rows);
    assert_eq!(budget.used(), base);
}
#[test]
fn strings_and_nested_buffers_release_their_charges_with_their_owners() {
    let budget = Budget::new(4096).unwrap();
    let base = budget.used();
    let mut rows = MemoryVec::new(&budget, 2).unwrap();
    let empty = budget.used();
    rows.push(MemoryString::new(&budget, "worker").unwrap())
        .unwrap();
    rows.push(MemoryString::new(&budget, "café").unwrap())
        .unwrap();
    assert_eq!(rows.first().unwrap().as_str(), "worker");
    assert!(budget.used() > empty);
    let value = rows.pop().unwrap();
    let held = budget.used();
    drop(value);
    assert!(budget.used() < held);
    rows.clear();
    assert_eq!(budget.used(), empty);
    drop(rows);
    assert_eq!(budget.used(), base);
}
#[test]
fn collection_and_history_threads_share_one_limit() {
    use std::sync::{Arc, Barrier};
    let budget = Budget::new(2048).unwrap();
    let base = budget.used();
    let barrier = Arc::new(Barrier::new(4));
    let mut handles = Vec::new();
    for _ in 0..4 {
        let budget = Arc::clone(&budget);
        let barrier = Arc::clone(&barrier);
        handles.push(std::thread::spawn(move || {
            let mut refused = 0;
            for _ in 0..100 {
                barrier.wait();
                let bytes = MemoryVec::<u8>::new(&budget, 1024);
                refused += usize::from(bytes.is_err());
                barrier.wait();
                drop(bytes);
                barrier.wait();
            }
            refused
        }));
    }
    let refused: usize = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .sum();
    assert_eq!(refused, 300);
    assert_eq!(budget.used(), base);
    assert!(budget.peak() <= budget.maximum());
}
#[test]
fn compaction_preserves_values_and_refunds_unused_capacity() {
    let budget = Budget::new(4096).unwrap();
    let mut rows = MemoryVec::new(&budget, 256).unwrap();
    for n in 0..3u64 {
        rows.push(n).unwrap();
    }
    let used = budget.used();
    rows.compact().unwrap();
    assert_eq!(&*rows, &[0, 1, 2]);
    assert_eq!(rows.capacity(), rows.len());
    assert!(budget.used() < used);
    rows.clear();
    rows.compact().unwrap();
    assert_eq!(rows.capacity(), 0);
}

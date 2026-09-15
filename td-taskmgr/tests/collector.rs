#![allow(clippy::unwrap_used, clippy::panic)]
use std::sync::atomic::AtomicBool;
use td_taskmgr::budget::{Budget, LIMIT};
use td_taskmgr::collector::Collector;
use td_taskmgr::snapshot::IdentityStore;
#[test]
fn actual_procfs_collection_observes_self_and_retains_names_with_one_budget() {
    let budget = Budget::new(LIMIT).unwrap();
    let store = IdentityStore::new(&budget).unwrap();
    let mut collector = Collector::new(&budget, 7).unwrap();
    let cancel = AtomicBool::new(false);
    let first = collector.sample(&cancel).unwrap();
    let own = first
        .processes
        .iter()
        .find(|row| row.input.key.pid == std::process::id())
        .unwrap();
    assert_eq!(own.input.key.generation, 7);
    assert_eq!(own.input.cpu, None);
    let first_cpu_time = own.cpu_time_ms.unwrap();
    assert!(own.input.rss.is_some());
    assert!(first.name(own).is_some());
    assert!(!first.cpus.is_empty());
    assert_eq!(first.cpus.capacity(), first.cpus.len());
    assert!(first.memory.unwrap().total.is_some());
    let own_key = own.input.key;
    let snapshot = first.snapshot(&store).unwrap();
    assert_eq!(snapshot.processes().len(), first.processes.len());
    assert_eq!(
        snapshot
            .processes()
            .iter()
            .find(|row| row.key == own_key)
            .unwrap()
            .cpu_time_ms,
        Some(first_cpu_time)
    );
    drop(first);
    std::thread::sleep(std::time::Duration::from_millis(20));
    let second = collector.sample(&cancel).unwrap();
    let own = second
        .processes
        .iter()
        .find(|row| row.input.key == own_key)
        .unwrap();
    assert!(own.input.cpu.is_some());
    assert!(own.cpu_time_ms.unwrap() >= first_cpu_time);
    assert!(second.cpu.is_some());
    drop(second);
    drop(snapshot);
    assert_eq!(store.live_entries().unwrap(), 0);
    assert!(budget.peak() <= budget.maximum());
}
#[test]
fn cancellation_and_insufficient_budget_return_without_starting_a_scan() {
    let budget = Budget::new(LIMIT).unwrap();
    let mut collector = Collector::new(&budget, 1).unwrap();
    assert_eq!(
        collector.sample(&AtomicBool::new(true)).unwrap_err().kind(),
        std::io::ErrorKind::Interrupted
    );
    assert!(Collector::new(&Budget::new(4096).unwrap(), 1).is_err());
}

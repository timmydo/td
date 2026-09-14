#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use td_taskmgr::budget::Budget;
use td_taskmgr::hierarchy::{Input, ProcessKey};
use td_taskmgr::snapshot::{IdentityStore, Observed, Snapshot};
fn row(pid: u32, parent: u32, name: &str) -> Observed<'_> {
    Observed {
        input: Input {
            key: ProcessKey {
                generation: 1,
                pid,
                start_ticks: 100,
            },
            parent_pid: Some(parent),
            cpu: Some(100),
            rss: Some(4096),
        },
        uid: Some(1000),
        state: b'R',
        name,
    }
}
#[test]
fn history_owns_names_and_automatic_eviction_releases_exactly_its_references() {
    let budget = Budget::new(1024 * 1024).unwrap();
    let store = IdentityStore::new(&budget).unwrap();
    let mut history =
        td_taskmgr::history::History::new(&budget, td_taskmgr::history::Interval::Second).unwrap();
    let first = Snapshot::new(
        &store,
        &[row(1, 0, "old"), row(2, 1, "worker")],
        0,
        1,
        false,
    )
    .unwrap();
    let id = history.admit(1, 0, first).unwrap();
    history.inspect(id);
    let second = Snapshot::new(
        &store,
        &[row(1, 0, "new"), row(2, 1, "worker")],
        2,
        3,
        false,
    )
    .unwrap();
    second
        .with_identities(|names| assert_eq!(names.get(1).unwrap().references(), 2))
        .unwrap();
    history.admit(3, 0, second).unwrap();
    assert_eq!(store.live_entries().unwrap(), 3);
    history
        .selected()
        .unwrap()
        .value
        .with_identities(|names| assert_eq!(names.get(0).unwrap().name(), "old"))
        .unwrap();
    drop(history);
    assert_eq!(store.live_entries().unwrap(), 0);
}
#[test]
fn invalid_or_failed_snapshots_never_leak_identity_references() {
    let budget = Budget::new(5000).unwrap();
    let store = IdentityStore::new(&budget).unwrap();
    assert!(Snapshot::new(&store, &[row(1, 0, "bad")], 2, 1, false).is_err());
    assert!(Snapshot::new(&store, &[row(2, 0, "bad"), row(1, 0, "bad")], 0, 1, false).is_err());
    let first = Snapshot::new(&store, &[row(1, 0, "held")], 0, 1, false).unwrap();
    assert!(Snapshot::new(
        &store,
        &[row(1, 0, "held"), row(2, 1, &"x".repeat(4096))],
        2,
        3,
        false
    )
    .is_err());
    assert_eq!(store.live_entries().unwrap(), 1);
    first
        .with_identities(|names| assert_eq!(names.get(0).unwrap().references(), 1))
        .unwrap();
    drop(first);
    assert_eq!(store.live_entries().unwrap(), 0);
}

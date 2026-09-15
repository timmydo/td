#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use td_taskmgr::budget::{Budget, LIMIT};
use td_taskmgr::contributors::{Colors, Contributors, Metric};
use td_taskmgr::hierarchy::{Input, ProcessKey};
use td_taskmgr::snapshot::{IdentityStore, Observed, Snapshot};
fn snapshot(store: &IdentityStore, values: &[(u32, Option<u64>)]) -> Snapshot {
    let rows = values
        .iter()
        .map(|(pid, cpu)| Observed {
            cpu_time_ms: None,
            input: Input {
                key: ProcessKey {
                    generation: 1,
                    pid: *pid,
                    start_ticks: 1,
                },
                parent_pid: Some(0),
                cpu: *cpu,
                rss: Some(4096),
            },
            name: "worker",
            uid: Some(1000),
            state: b'R',
        })
        .collect::<Vec<_>>();
    Snapshot::new(store, &rows, 0, 1, false).unwrap()
}
#[test]
fn peaks_use_whole_view_keep_selection_and_preserve_retained_colors() {
    let budget = Budget::new(LIMIT).unwrap();
    let store = IdentityStore::new(&budget).unwrap();
    let first = snapshot(
        &store,
        &(1..=10)
            .map(|pid| (pid, Some(u64::from(pid) * 100)))
            .collect::<Vec<_>>(),
    );
    let last = snapshot(&store, &[(1, Some(10000)), (2, Some(0)), (3, Some(100))]);
    let selected = first.processes()[1].key;
    let named = Contributors::choose([&first, &last], Metric::Cpu, Some(selected));
    let keys = named.named().map(|item| item.key.pid).collect::<Vec<_>>();
    assert_eq!(keys, vec![1, 2, 5, 6, 7, 8, 9, 10]);
    let mut colors = Colors::default();
    colors.update(&named);
    let old = colors.slot(selected).unwrap();
    let next = Contributors::choose([&first], Metric::Cpu, Some(selected));
    colors.update(&next);
    assert_eq!(colors.slot(selected), Some(old));
    let mut slots = next
        .named()
        .map(|item| colors.slot(item.key).unwrap())
        .collect::<Vec<_>>();
    slots.sort_unstable();
    slots.dedup();
    assert_eq!(slots.len(), 8);
}
#[test]
fn missing_observations_are_gaps_and_other_does_not_invent_unavailable_values() {
    let budget = Budget::new(LIMIT).unwrap();
    let store = IdentityStore::new(&budget).unwrap();
    let first = snapshot(&store, &[(1, Some(20)), (2, Some(30))]);
    let last = snapshot(&store, &[(2, None), (3, Some(40))]);
    let named = Contributors::choose([&first], Metric::Cpu, None);
    assert_eq!(named.values(&first)[0], Some(20));
    assert_eq!(named.values(&first)[1], Some(30));
    assert_eq!(named.values(&last)[0], None);
    assert_eq!(named.values(&last)[1], None);
    assert_eq!(named.values(&last)[8], Some(40));
    let unknown = snapshot(&store, &[(3, None)]);
    assert_eq!(named.values(&unknown)[8], None);
}

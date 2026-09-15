#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use std::sync::Arc;
use td_taskmgr::budget::{Budget, LIMIT};
use td_taskmgr::hierarchy::{Input, ProcessKey};
use td_taskmgr::projection::{Column, Expansion, Key, Projection, Sort};
use td_taskmgr::snapshot::{IdentityStore, Observed, Snapshot};
fn process(pid: u32, parent: u32, name: &'static str, cpu: u64) -> Observed<'static> {
    Observed {
        cpu_time_ms: None,
        input: Input {
            key: ProcessKey {
                generation: 1,
                pid,
                start_ticks: 1,
            },
            parent_pid: Some(parent),
            cpu: Some(cpu),
            rss: Some(cpu * 4096),
        },
        name,
        uid: Some(1000),
        state: b'R',
    }
}
fn make_snapshot(budget: &Arc<Budget>, rows: &[Observed<'_>]) -> Snapshot {
    Snapshot::new(&IdentityStore::new(budget).unwrap(), rows, 0, 1, false).unwrap()
}
fn key(pid: u32) -> Key {
    Key::Process(ProcessKey {
        generation: 1,
        pid,
        start_ticks: 1,
    })
}
#[test]
fn cpu_sort_ranks_all_processes_across_parent_child_relationships() {
    let budget = Budget::new(LIMIT).unwrap();
    let snapshot = make_snapshot(
        &budget,
        &[
            process(1, 0, "root", 0),
            process(2, 1, "zebra", 5),
            process(3, 1, "apple", 10),
            process(4, 2, "grandchild", 99),
        ],
    );
    let expansion = Expansion::new(&budget).unwrap();
    let view = Projection::new(
        &budget,
        &snapshot,
        Sort {
            column: Column::Cpu,
            descending: true,
        },
        "",
        None,
        &expansion,
    )
    .unwrap();
    assert_eq!(
        view.rows().iter().map(|r| r.key).collect::<Vec<_>>(),
        vec![key(4), key(3), key(2), key(1)]
    );
    assert_eq!(
        view.rows().iter().map(|r| r.depth).collect::<Vec<_>>(),
        vec![0, 0, 0, 0]
    );
    assert!(view.rows().iter().all(|row| row.parent.is_none()));
    let tree = Projection::new(
        &budget,
        &snapshot,
        Sort {
            column: Column::Name,
            descending: false,
        },
        "",
        None,
        &expansion,
    )
    .unwrap();
    assert_eq!(
        tree.rows().iter().map(|r| r.key).collect::<Vec<_>>(),
        vec![key(1), key(3), key(2), key(4)]
    );
    let memory = Projection::new(
        &budget,
        &snapshot,
        Sort {
            column: Column::Rss,
            descending: true,
        },
        "",
        None,
        &expansion,
    )
    .unwrap();
    assert_eq!(memory.rows().first().unwrap().key, key(4));
    let filtered = Projection::new(
        &budget,
        &snapshot,
        Sort {
            column: Column::Cpu,
            descending: true,
        },
        "grandchild",
        None,
        &expansion,
    )
    .unwrap();
    assert_eq!(filtered.rows().len(), 1);
    assert!(!filtered.rows()[0].context);
}
#[test]
fn search_keeps_context_and_selected_exception_without_changing_collapses() {
    let budget = Budget::new(LIMIT).unwrap();
    let snapshot = make_snapshot(
        &budget,
        &[
            process(1, 0, "root", 0),
            process(2, 1, "worker", 5),
            process(3, 1, "apple", 10),
            process(4, 2, "target", 99),
        ],
    );
    let mut expansion = Expansion::new(&budget).unwrap();
    expansion.set(key(1), false).unwrap();
    let selected = Some(ProcessKey {
        generation: 1,
        pid: 3,
        start_ticks: 1,
    });
    let view = Projection::new(
        &budget,
        &snapshot,
        Sort::default(),
        "target",
        selected,
        &expansion,
    )
    .unwrap();
    assert_eq!(
        view.rows().iter().map(|r| r.key).collect::<Vec<_>>(),
        vec![key(1), key(2), key(4), key(3)]
    );
    assert!(view.rows()[0].context && !view.rows()[0].exception);
    assert!(view.rows()[1].context && !view.rows()[1].exception);
    assert!(!view.rows()[2].context);
    assert!(view.rows()[3].exception);
    assert!(!expansion.expanded(key(1)));
    let view = Projection::new(&budget, &snapshot, Sort::default(), "", None, &expansion).unwrap();
    assert_eq!(view.rows().len(), 1);
}
#[test]
fn unresolved_roots_are_explicit_and_reveal_expands_them() {
    let budget = Budget::new(LIMIT).unwrap();
    let snapshot = make_snapshot(
        &budget,
        &[process(2, 99, "orphan", 0), process(3, 2, "child", 0)],
    );
    let mut expansion = Expansion::new(&budget).unwrap();
    expansion.set(Key::Unavailable, false).unwrap();
    expansion.set(key(2), false).unwrap();
    let view = Projection::new(&budget, &snapshot, Sort::default(), "", None, &expansion).unwrap();
    assert_eq!(view.rows().len(), 1);
    assert_eq!(view.rows()[0].key, Key::Unavailable);
    assert!(!view.rows()[0].context);
    expansion.reveal(
        &snapshot,
        ProcessKey {
            generation: 1,
            pid: 3,
            start_ticks: 1,
        },
    );
    let view = Projection::new(&budget, &snapshot, Sort::default(), "", None, &expansion).unwrap();
    assert_eq!(
        view.rows().iter().map(|r| r.key).collect::<Vec<_>>(),
        vec![Key::Unavailable, key(2), key(3)]
    );
    assert_eq!(view.rows()[1].parent, Some(Key::Unavailable));
    assert_eq!(view.rows()[2].depth, 2);
}
#[test]
fn pid_uid_and_unicode_text_search_are_bounded_and_literal() {
    let budget = Budget::new(LIMIT).unwrap();
    let snapshot = make_snapshot(
        &budget,
        &[process(20, 0, "Λambda", 0), process(31, 0, "lambda", 0)],
    );
    let expansion = Expansion::new(&budget).unwrap();
    for (query, expected) in [("20", 1), ("1000", 2), ("Λ", 1), ("λ", 0), ("*", 0)] {
        let view =
            Projection::new(&budget, &snapshot, Sort::default(), query, None, &expansion).unwrap();
        assert_eq!(view.rows().len(), expected, "{query}");
    }
    assert!(Projection::new(
        &budget,
        &snapshot,
        Sort::default(),
        &"a".repeat(257),
        None,
        &expansion
    )
    .is_err());
}
#[test]
fn column_click_defaults_and_ties_are_stable() {
    let budget = Budget::new(LIMIT).unwrap();
    let snapshot = make_snapshot(
        &budget,
        &[process(1, 0, "same", 1), process(2, 0, "same", 1)],
    );
    let expansion = Expansion::new(&budget).unwrap();
    let mut sort = Sort::default();
    sort.click(Column::Cpu);
    assert!(sort.descending);
    sort.click(Column::Cpu);
    assert!(!sort.descending);
    sort.click(Column::Name);
    assert!(!sort.descending);
    sort.click(Column::Name);
    assert!(sort.descending);
    let view = Projection::new(&budget, &snapshot, sort, "", None, &expansion).unwrap();
    assert_eq!(
        view.rows().iter().map(|r| r.key).collect::<Vec<_>>(),
        vec![key(1), key(2)]
    );
}
#[test]
fn maximum_roster_plus_synthetic_root_and_depth_remain_visible() {
    let budget = Budget::new(LIMIT).unwrap();
    let rows = (1..=32768)
        .map(|pid| process(pid, 40000, "orphan", 1))
        .collect::<Vec<_>>();
    let snapshot = make_snapshot(&budget, &rows);
    let expansion = Expansion::new(&budget).unwrap();
    let view = Projection::new(&budget, &snapshot, Sort::default(), "", None, &expansion).unwrap();
    assert_eq!(view.rows().len(), 32769);
    assert_eq!(view.rows().last().unwrap().key, key(32768));
    assert!(budget.peak() <= LIMIT);
    let rows = (1..=257)
        .map(|pid| process(pid, if pid == 1 { 999 } else { pid - 1 }, "chain", 1))
        .collect::<Vec<_>>();
    let snapshot = make_snapshot(&budget, &rows);
    let view = Projection::new(&budget, &snapshot, Sort::default(), "", None, &expansion).unwrap();
    assert_eq!(view.rows().len(), 258);
    assert!(view.rows().iter().all(|r| r.depth <= 256));
}

#[test]
fn process_detail_roots_the_subtree_and_excludes_siblings_and_reused_ids() {
    let budget = Budget::new(LIMIT).unwrap();
    let snapshot = make_snapshot(
        &budget,
        &[
            process(1, 0, "root", 0),
            process(2, 1, "child", 50),
            process(3, 2, "grandchild", 80),
            process(4, 1, "sibling", 100),
        ],
    );
    let expansion = Expansion::new(&budget).unwrap();
    let root = snapshot.processes()[1].key;
    let detail = Projection::for_root(
        &budget,
        &snapshot,
        Sort::default(),
        "",
        Some(root),
        &expansion,
        Some(root),
    )
    .unwrap();
    assert_eq!(
        detail.rows().iter().map(|r| r.key).collect::<Vec<_>>(),
        vec![key(2), key(3)]
    );
    assert_eq!(detail.rows()[0].depth, 0);
    assert_eq!(detail.rows()[0].parent, None);
    assert_eq!(detail.rows()[1].parent, Some(key(2)));
    let missing = Projection::for_root(
        &budget,
        &snapshot,
        Sort::default(),
        "",
        None,
        &expansion,
        Some(ProcessKey {
            start_ticks: 2,
            ..root
        }),
    )
    .unwrap();
    assert!(missing.rows().is_empty());
}

#[test]
fn cpu_time_ranks_lifetime_totals_including_collapsed_descendants() {
    let budget = Budget::new(LIMIT).unwrap();
    let mut rows = [
        process(1, 0, "parent", 99),
        process(2, 1, "child", 0),
        process(3, 0, "unknown", 5),
    ];
    rows[0].cpu_time_ms = Some(10);
    rows[1].cpu_time_ms = Some(50_000);
    let snapshot = make_snapshot(&budget, &rows);
    let mut expansion = Expansion::new(&budget).unwrap();
    expansion.set(key(1), false).unwrap();
    let view = Projection::new(
        &budget,
        &snapshot,
        Sort {
            column: Column::CpuTime,
            descending: true,
        },
        "",
        None,
        &expansion,
    )
    .unwrap();
    assert_eq!(
        view.rows().iter().map(|row| row.key).collect::<Vec<_>>(),
        vec![key(2), key(1), key(3)]
    );
    assert!(view.rows().iter().all(|row| row.depth == 0));
}

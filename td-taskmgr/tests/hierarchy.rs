#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use td_taskmgr::budget::Budget;
use td_taskmgr::hierarchy::{Forest, Input, ParentIssue, ProcessKey, DEPTH};
fn row(pid: u32, parent: u32, cpu: Option<u64>) -> Input {
    Input {
        key: ProcessKey {
            generation: 1,
            pid,
            start_ticks: 100,
        },
        parent_pid: Some(parent),
        cpu,
        rss: Some(4096),
    }
}
#[test]
fn missing_parents_and_adoption_resolve_within_the_snapshot() {
    let budget = Budget::new(1024 * 1024).unwrap();
    let mut rows = [
        row(1, 0, Some(1)),
        row(2, 99, None),
        row(3, 1, Some(3)),
        row(4, 3, Some(4)),
    ];
    rows[2].key.start_ticks = 1000;
    let forest = Forest::new(&budget, &rows, false).unwrap();
    assert_eq!(forest.nodes()[1].issue, ParentIssue::Unavailable);
    assert_eq!(forest.nodes()[3].parent, Some(2)); // younger subreaper may adopt
    assert_eq!(forest.nodes()[0].cpu.value(), Some(8));
    assert_eq!(forest.nodes()[0].rss.value(), Some(3 * 4096));
    assert!(!forest.nodes()[0].cpu.partial);
    assert_eq!(forest.nodes()[1].cpu.value(), None);
}
#[test]
fn cycles_are_broken_deterministically_and_incoming_chains_keep_depths() {
    let budget = Budget::new(1024 * 1024).unwrap();
    let rows = [
        row(1, 4, Some(1)),
        row(2, 3, Some(2)),
        row(3, 4, Some(3)),
        row(4, 2, Some(4)),
        row(5, 5, Some(5)),
    ];
    let forest = Forest::new(&budget, &rows, false).unwrap();
    assert_eq!(forest.nodes()[1].parent, None);
    assert_eq!(forest.nodes()[1].issue, ParentIssue::Cycle);
    assert_eq!(forest.nodes()[0].depth, 2);
    assert_eq!(forest.nodes()[1].cpu.value(), Some(10));
    assert_eq!(forest.nodes()[4].issue, ParentIssue::Cycle);
    assert_eq!(forest.nodes()[4].cpu.value(), Some(5));
    for (index, node) in forest.nodes().iter().enumerate() {
        if let Some(parent) = node.parent {
            assert_ne!(index, parent);
            assert_eq!(node.depth, forest.nodes()[parent].depth + 1);
        }
    }
}
#[test]
fn deep_or_large_populations_and_total_overflow_stay_bounded() {
    let budget = Budget::new(16 * 1024 * 1024).unwrap();
    let rows = (1..=32768)
        .map(|pid| row(pid, pid - 1, Some(1)))
        .collect::<Vec<_>>();
    let forest = Forest::new(&budget, &rows, false).unwrap();
    assert!(forest
        .nodes()
        .iter()
        .all(|node| usize::from(node.depth) < DEPTH));
    assert!(forest
        .nodes()
        .iter()
        .any(|node| node.issue == ParentIssue::Depth));
    let sum: u64 = forest
        .nodes()
        .iter()
        .filter(|node| node.parent.is_none())
        .filter_map(|node| node.cpu.value())
        .sum();
    assert_eq!(sum, 32768);
    let rows = [
        row(1, 0, Some(u64::MAX)),
        row(2, 1, Some(1)),
        row(3, 1, Some(2)),
    ];
    let forest = Forest::new(&budget, &rows, false).unwrap();
    assert_eq!(forest.nodes()[0].cpu.value(), None);
    assert!(forest.nodes()[0].cpu.partial);
}
#[test]
fn unavailable_metrics_mark_totals_partial_and_identity_roster_is_validated() {
    let budget = Budget::new(1024 * 1024).unwrap();
    let rows = [row(1, 0, None), row(2, 1, Some(5))];
    let forest = Forest::new(&budget, &rows, false).unwrap();
    assert_eq!(forest.nodes()[0].cpu.value(), Some(5));
    assert!(forest.nodes()[0].cpu.partial);
    assert!(Forest::new(&budget, &[rows[1], rows[0]], false).is_err());
    assert!(Forest::new(&budget, &[rows[0], rows[0]], false).is_err());
    let forest = Forest::new(&budget, &rows, true).unwrap();
    assert!(forest.nodes().iter().all(|node| node.rss.partial));
}

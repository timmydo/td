#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use td_taskmgr::budget::{Budget, MemoryString};
use td_taskmgr::history::{Error, History, Interval, WINDOW_NS};
#[test]
fn cadence_bounds_and_actual_age_hold_without_fabricating_missing_samples() {
    for interval in [
        Interval::HalfSecond,
        Interval::Second,
        Interval::TwoSeconds,
        Interval::FiveSeconds,
    ] {
        let budget = Budget::new(1024 * 1024).unwrap();
        let mut history = History::new(&budget, interval).unwrap();
        for n in 0..500 {
            history.admit(n * interval.nanoseconds(), 0, n).unwrap();
        }
        assert_eq!(history.samples().len(), interval.samples());
        assert_eq!(
            history.retained_duration_ns(),
            WINDOW_NS - interval.nanoseconds()
        );
        let time = 800 * interval.nanoseconds();
        history.admit(time, 300, 800).unwrap();
        assert_eq!(history.samples().len(), 1);
        assert_eq!(history.samples()[0].skipped, 300);
        assert_eq!(history.retained_duration_ns(), 0);
    }
}
#[test]
fn inspection_is_immutable_across_eviction_and_live_releases_old_pin() {
    let budget = Budget::new(1024 * 1024).unwrap();
    let mut history = History::new(&budget, Interval::Second).unwrap();
    let pinned = history.admit(0, 0, "historical exited process").unwrap();
    assert!(history.inspect(pinned));
    for n in 1..400 {
        history
            .admit(n * 1_000_000_000, 0, "current process")
            .unwrap();
    }
    assert_eq!(history.pinned(), Some(pinned));
    assert_eq!(
        history.selected().unwrap().value,
        "historical exited process"
    );
    assert_eq!(history.samples().len(), 120);
    assert_eq!(history.retained_duration_ns(), 118_000_000_000);
    history.set_interval(Interval::FiveSeconds);
    assert_eq!(history.samples().len(), 24);
    assert_eq!(history.pinned(), Some(pinned));
    history.live();
    assert_eq!(history.pinned(), None);
    assert_eq!(history.selected().unwrap().value, "current process");
    assert!(!history.inspect(pinned));
}
#[test]
fn budget_pressure_evicts_unpinned_first_and_never_moves_inspection() {
    let budget = Budget::new(64 * 1024).unwrap();
    let mut history = History::new(&budget, Interval::Second).unwrap();
    let base = budget.used();
    let id = history
        .admit(0, 0, MemoryString::new(&budget, &"x".repeat(8000)).unwrap())
        .unwrap();
    history.inspect(id);
    history
        .admit(1, 0, MemoryString::new(&budget, &"y".repeat(8000)).unwrap())
        .unwrap();
    assert!(history.make_room(budget.maximum() - base - 8100));
    assert_eq!(history.samples().len(), 1);
    assert!(!history.make_room(budget.maximum() - base));
    assert!(history.pressure());
    assert_eq!(history.selected().unwrap().id, id);
    history.live();
    assert!(history.make_room(budget.maximum() - base));
    assert!(history.samples().is_empty());
    assert_eq!(budget.used(), base);
}
#[test]
fn invalid_times_do_not_evict_and_nearest_time_uses_earlier_ties() {
    let budget = Budget::new(64 * 1024).unwrap();
    let mut history = History::new(&budget, Interval::Second).unwrap();
    let first = history.admit(10, 0, 1).unwrap();
    history.admit(20, 0, 2).unwrap();
    assert_eq!(history.admit(20, 0, 3), Err((Error::Time, 3)));
    assert_eq!(history.admit(19, 0, 4), Err((Error::Time, 4)));
    assert_eq!(history.samples().len(), 2);
    assert_eq!(history.at(15).unwrap().id, first);
    assert_eq!(history.at(u64::MAX).unwrap().value, 2);
}

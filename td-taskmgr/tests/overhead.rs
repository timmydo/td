#![allow(clippy::unwrap_used, clippy::panic)]
use td_taskmgr::budget::{Budget, LIMIT};
use td_taskmgr::contributors::{Contributors, Metric};
use td_taskmgr::hierarchy::{Input, ProcessKey};
use td_taskmgr::snapshot::{IdentityStore, Observed, Snapshot};
#[test]
#[ignore = "explicit release-mode measurement of a large synthetic population"]
fn synthetic_population_model_overhead() {
    let budget = Budget::new(LIMIT).unwrap();
    let store = IdentityStore::new(&budget).unwrap();
    let rows = (1..=32768)
        .map(|pid| Observed {
            input: Input {
                key: ProcessKey {
                    generation: 1,
                    pid,
                    start_ticks: 1,
                },
                parent_pid: Some(0),
                cpu: Some(u64::from(pid)),
                rss: Some(4096),
            },
            name: "worker",
            uid: Some(1000),
            state: b'R',
        })
        .collect::<Vec<_>>();
    let units = td_taskmgr::linux_read::auxv(&std::fs::read("/proc/self/auxv").unwrap()).unwrap();
    let ticks = || {
        let bytes = std::fs::read("/proc/self/stat").unwrap();
        let p = td_taskmgr::parsers::process(&bytes).unwrap();
        p.user_ticks.unwrap() + p.system_ticks.unwrap()
    };
    let cpu_before = ticks();
    let start = std::time::Instant::now();
    for _ in 0..10 {
        let first = Snapshot::new(&store, &rows, 0, 1, false).unwrap();
        let second = Snapshot::new(&store, &rows, 2, 3, false).unwrap();
        let contributors = Contributors::choose([&first, &second], Metric::Cpu, None);
        assert_eq!(contributors.named().count(), 8);
        assert_eq!(first.processes().len(), 32768);
    }
    let wall = start.elapsed();
    let cpu = ticks() - cpu_before;
    eprintln!("synthetic: rows=32768 iterations=10 snapshots_per_iteration=2 elapsed_ms={} cpu_ticks={} ticks_per_second={} peak_model_bytes={} retained_pool_bytes={}",wall.as_millis(),cpu,units.ticks_per_second,budget.peak(),budget.used());
    assert!(budget.peak() <= budget.maximum());
    assert_eq!(store.live_entries().unwrap(), 0);
}

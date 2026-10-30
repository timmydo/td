#![allow(clippy::unwrap_used, clippy::indexing_slicing)]
use super::{clock_sys, runtime, state};
use std::fs;
use std::os::unix::fs::symlink;
use td_time::request::parse_utc;

#[test]
fn rtc_calendar_layout_is_utc_with_linux_year_and_month_offsets() {
    assert_eq!(
        clock_sys::rtc_fields(parse_utc("2026-10-30T06:11:04Z").unwrap()).unwrap(),
        [4, 11, 6, 30, 9, 126, 0, 0, 0]
    );
    assert_eq!(
        clock_sys::rtc_fields(parse_utc("2024-02-29T23:59:59Z").unwrap()).unwrap(),
        [59, 59, 23, 29, 1, 124, 0, 0, 0]
    );
    assert!(clock_sys::rtc_fields(0).is_err());
    assert!(clock_sys::rtc_fields(u64::MAX).is_err());
}

#[test]
fn automatic_policy_repairs_seven_hours_but_honors_disable_and_large_error() {
    let now = parse_utc("2026-10-30T06:11:04Z").unwrap() * 1000;
    for offset in [-86_400_000i64, -25_200_000, 0, 25_200_000, 86_400_000] {
        let local = now.checked_add_signed(offset).unwrap();
        assert_eq!(runtime::automatic_time(local, now, true).unwrap(), now);
        assert!(runtime::automatic_time(local, now, false).is_err());
    }
    for offset in [-86_400_001i64, 86_400_001] {
        assert!(
            runtime::automatic_time(now.checked_add_signed(offset).unwrap(), now, true).is_err()
        );
    }
    assert!(runtime::automatic_time(0, 0, true).is_err());
    assert!(runtime::automatic_time(u64::MAX, u64::MAX, true).is_err());
}

#[test]
fn setting_reads_are_bounded_and_publication_never_follows_old_names() {
    let tree = td_fs::TempDir::new("td-time-state").unwrap();
    let root = tree.path();
    let other = root.join("other");
    fs::write(&other, b"unchanged").unwrap();
    symlink(&other, root.join("automatic")).unwrap();
    assert!(state::read(&root.join("automatic"), 64).is_err());
    symlink(&other, root.join(".automatic.td-time-new")).unwrap();
    state::publish(root, "automatic", b"off\n").unwrap();
    assert_eq!(fs::read(&other).unwrap(), b"unchanged");
    assert_eq!(state::read(&root.join("automatic"), 4).unwrap(), "off\n");
    assert!(state::read(&root.join("automatic"), 3).is_err());
    state::publish(root, "automatic", b"on\n").unwrap();
    assert_eq!(state::read(&root.join("automatic"), 4).unwrap(), "on\n");
    assert!(state::publish(root, "../outside", b"no").is_err());
    assert!(!root.join(".automatic.td-time-new").exists());
}

#[test]
fn clock_boundary_has_one_instruction_and_only_the_two_fixed_writes() {
    let raw = include_str!("clock_sys.rs");
    let hash = raw.bytes().fold(0xcbf29ce484222325u64, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x100000001b3)
    });
    assert_eq!(hash, 0xa5995c811049c599); // Filled after formatting; this pins the entire boundary.
    assert_eq!(raw.matches("unsafe").count(), 2);
    assert_eq!(raw.matches("asm!").count(), 1);
    assert!(raw.contains("const CLOCK_SETTIME: usize = 227;"));
    assert!(raw.contains("const CLOCK_REALTIME: usize = 0;"));
    assert!(raw.contains("const IOCTL: usize = 16;"));
    assert!(raw.contains("const RTC_SET_TIME: usize = 0x4024700a;"));
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut inventory: Vec<_> = fs::read_dir(root.join("src"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    inventory.sort();
    assert_eq!(
        inventory,
        [
            "clock_sys.rs",
            "lib.rs",
            "main.rs",
            "ntp.rs",
            "request.rs",
            "runtime.rs",
            "state.rs",
            "tests.rs"
        ]
    );
    for (name, count) in [
        ("lib.rs", 1),
        ("main.rs", 1),
        ("ntp.rs", 0),
        ("request.rs", 0),
        ("runtime.rs", 0),
        ("state.rs", 0),
    ] {
        let source = fs::read_to_string(root.join("src").join(name)).unwrap();
        assert_eq!(source.matches("unsafe").count(), count, "{name}");
        if name != "runtime.rs" && name != "main.rs" {
            assert!(!source.contains("clock_sys"), "{name}");
        }
    }
    let runtime = include_str!("runtime.rs");
    assert_eq!(runtime.matches("clock_sys::realtime(").count(), 1);
    assert_eq!(runtime.matches("clock_sys::rtc(").count(), 1);
    assert!(!raw.contains("nomem"));
    // These tests never invoke the clock-changing wrappers.
}

#[test]
fn slow_queries_do_not_count_as_suspend() {
    use std::time::Duration;
    for query_seconds in [0, 6, 20] {
        let elapsed = Duration::from_secs(query_seconds + 1);
        assert!(!runtime::resumed((query_seconds + 1) * 1000, elapsed));
        assert!(runtime::resumed((query_seconds + 7) * 1000, elapsed));
    }
    assert!(!runtime::resumed(6000, Duration::from_secs(1)));
    assert!(runtime::resumed(6001, Duration::from_secs(1)));
}

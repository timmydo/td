#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;
use crate::ports::{Deadline, Tick, Time};
use std::sync::atomic::{AtomicU64, Ordering};
struct Timer {
    tick: AtomicU64,
    calls: AtomicU64,
    expires: AtomicU64,
}
impl Timer {
    fn new() -> Self {
        Self {
            tick: AtomicU64::new(1),
            calls: AtomicU64::new(0),
            expires: AtomicU64::new(u64::MAX),
        }
    }
    fn after(&self, calls: u64) {
        self.expires.store(
            self.calls.load(Ordering::Relaxed) + calls,
            Ordering::Relaxed,
        );
    }
}
impl Clock for Timer {
    fn sample(&self) -> Result<Time, PolicyError> {
        let count = self.calls.fetch_add(1, Ordering::Relaxed) + 1;
        Ok(Time {
            utc_ms: 0,
            monotonic: Tick(if count >= self.expires.load(Ordering::Relaxed) {
                100
            } else {
                self.tick.load(Ordering::Relaxed)
            }),
        })
    }
}
fn budget(records: u64, output_bytes: u64) -> Meter {
    Meter::new(
        Deadline::after(Tick(0), 100).unwrap(),
        Charge {
            records,
            output_bytes,
            ..Charge::default()
        },
    )
}
fn project(source: &str, last_scalar: bool) -> (String, Summary) {
    let clock = Timer::new();
    let mut work = budget(10000, 10000);
    let pointer = std::ptr::from_mut(&mut work);
    let mut cursor = Cursor::new(&mut work, &clock, Tick(1)).unwrap();
    assert!(cursor.value().is_none());
    let chars: Vec<_> = source.chars().collect();
    let mut pos = 0;
    let mut output = String::new();
    for _ in 0..10000 {
        let input = chars.get(pos).copied();
        let last = pos == chars.len() || (last_scalar && pos + 1 == chars.len());
        let p = cursor.poll(input, last).unwrap();
        if p.consumed {
            pos += 1;
        }
        match p.status {
            Status::Scalar(value) => output.push(value),
            Status::Complete => {
                assert_eq!(pos, chars.len());
                let summary = cursor.value().unwrap();
                let (work, finished) = cursor.finish().unwrap();
                assert_eq!(std::ptr::from_mut(work), pointer);
                assert_eq!(summary, finished);
                assert_eq!(usize::from(summary.scalars), output.chars().count());
                assert_eq!(usize::from(summary.utf8_bytes), output.len());
                assert!(summary.scalars <= MAX_SCALARS);
                assert!(summary.utf8_bytes <= 1024);
                assert_eq!(10000 - work.remaining().output_bytes, output.len() as u64);
                let records = chars.len()
                    + output.chars().filter(|&c| c == ' ').count()
                    + usize::from(chars.is_empty() || !last_scalar);
                assert_eq!(10000 - work.remaining().records, records as u64);
                return (output, summary);
            }
            Status::Yield | Status::NeedInput => {}
        }
    }
    panic!("preview stalled");
}
#[test]
fn whitespace_collapses_without_leading_or_trailing_space() {
    for (source, expected) in [
        ("", ""),
        (" \t\r\n\u{a0}", ""),
        (" \t a\r\n b  \u{a0}c\n", "a b c"),
        ("a\u{b}\u{c}b", "a b"),
        ("<b>x</b>", "<b>x</b>"),
        ("e\u{301}\u{2003}é\0", "e\u{301}\u{2003}é�"),
        ("a\u{ffff}\u{fdd0}z", "a��z"),
        ("a\u{85}\u{2028}\u{2029}b\u{1b}\u{7f}\u{9f}", "a b���"),
    ] {
        for last in [false, true] {
            assert_eq!(project(source, last).0, expected);
        }
    }
}
#[test]
fn cap_counts_scalars_keeps_whole_utf8_and_never_a_lone_generated_separator() {
    for (source, expected) in [
        ("😀".repeat(300), "😀".repeat(256)),
        (format!("{} x rest", "é".repeat(255)), "é".repeat(255)),
        (
            format!("{} x rest", "é".repeat(254)),
            format!("{} x", "é".repeat(254)),
        ),
        (format!("{}\n\u{ffff}", "a".repeat(256)), "a".repeat(256)),
        ("e\u{301}".repeat(200), "e\u{301}".repeat(128)),
        (
            format!("a{}", "e\u{301}".repeat(200)),
            format!("a{}e", "e\u{301}".repeat(127)),
        ),
    ] {
        for last in [false, true] {
            assert_eq!(project(&source, last).0, expected);
        }
    }
}
#[test]
fn incomplete_empty_and_cached_polls_have_fresh_deadlines_and_no_positive_charge() {
    for mode in 0..4 {
        let clock = Timer::new();
        let mut work = budget(100, 100);
        let mut cursor = Cursor::new(&mut work, &clock, Tick(1)).unwrap();
        let before = cursor.work.remaining();
        assert_eq!(
            cursor.poll(None, false).unwrap(),
            Progress {
                consumed: false,
                status: Status::NeedInput
            }
        );
        assert_eq!(cursor.work.remaining(), before);
        if mode == 0 {
            assert_eq!(cursor.finish().err(), Some(Error::State));
            continue;
        }
        assert_eq!(
            cursor.poll(Some('x'), true).unwrap().status,
            Status::Scalar('x')
        );
        let before = cursor.work.remaining();
        assert_eq!(cursor.poll(None, true).unwrap().status, Status::Complete);
        assert_eq!(cursor.work.remaining(), before);
        clock.after(0);
        let error = Error::Work(Stop::Deadline);
        if mode == 1 {
            assert_eq!(cursor.finish().err(), Some(error));
        } else {
            if mode == 2 {
                assert_eq!(cursor.check(), Err(error));
            } else {
                assert_eq!(cursor.poll(None, true), Err(error));
            }
            assert!(cursor.value().is_none());
            assert_eq!(cursor.poll(None, true), Err(error));
            assert_eq!(cursor.finish().err(), Some(error));
        }
    }
}
#[test]
fn quota_regression_and_post_emit_expiry_latch_without_replacing_work() {
    for mode in 0..5 {
        let clock = Timer::new();
        let mut work = budget(
            if mode == 0 || mode == 4 { 0 } else { 10 },
            if mode == 1 { 0 } else { 10 },
        );
        let mut cursor = Cursor::new(&mut work, &clock, Tick(1)).unwrap();
        if mode == 2 {
            clock.tick.store(0, Ordering::Relaxed);
        }
        if mode == 3 {
            clock.after(4);
        }
        if mode == 4 {
            clock.after(3);
        }
        let error = match mode {
            0 | 4 => Error::Work(Stop::Records),
            1 => Error::Work(Stop::OutputBytes),
            2 => Error::Policy(PolicyError::Invalid),
            _ => Error::Work(Stop::Deadline),
        };
        assert_eq!(cursor.poll(Some('a'), true), Err(error));
        if mode == 3 {
            assert_eq!(cursor.summary.scalars, 1);
        }
        assert!(cursor.value().is_none());
        let before = cursor.work.remaining();
        let calls = clock.calls.load(Ordering::Relaxed);
        assert_eq!(cursor.poll(None, true), Err(error));
        assert_eq!(cursor.check(), Err(error));
        assert_eq!(cursor.work.remaining(), before);
        assert_eq!(clock.calls.load(Ordering::Relaxed), calls);
        assert_eq!(cursor.finish().err(), Some(error));
    }
}

#[test]
fn construction_preserves_the_jobs_prior_tick_and_checks_expiry() {
    for expired in [false, true] {
        let clock = Timer::new();
        if expired {
            clock.after(0);
        }
        let mut work = budget(100, 100);
        let before = work.remaining();
        assert_eq!(
            Cursor::new(&mut work, &clock, Tick(2)).err(),
            Some(if expired {
                Error::Work(Stop::Deadline)
            } else {
                Error::Policy(PolicyError::Invalid)
            })
        );
        assert_eq!(work.remaining(), before);
    }
}

#[test]
fn consuming_last_input_permits_fresh_finish_before_complete_poll() {
    for last in [' ', 'x'] {
        let clock = Timer::new();
        let mut work = budget(100, 100);
        let mut cursor = Cursor::new(&mut work, &clock, Tick(1)).unwrap();
        assert_eq!(
            cursor.poll(Some('a'), false).unwrap().status,
            Status::Scalar('a')
        );
        let progress = cursor.poll(Some(last), true).unwrap();
        assert!(progress.consumed);
        assert_eq!(
            progress.status,
            if last == ' ' {
                Status::Yield
            } else {
                Status::Scalar('x')
            }
        );
        let count = if last == ' ' { 1 } else { 2 };
        assert_eq!(
            cursor.finish().unwrap().1,
            Summary {
                scalars: count,
                utf8_bytes: count
            }
        );
    }
}

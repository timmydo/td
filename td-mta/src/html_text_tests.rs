#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;
use crate::ports::{Deadline, Time};
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
#[test]
fn extraction_charges_every_turn_and_emission_under_the_original_owner() {
    for (source, expected, malformed) in [
        ("", "", false),
        (
            "<head>x</head><div>Hello&nbsp;<b>😀</b></div><img alt='&amp;'>",
            " Hello\u{20}😀  & ",
            false,
        ),
        ("a<script>hidden</script><span>b</span>c", "abc", false),
        ("a\u{a0}b", "a b", false),
        ("&copy; &#65; &#0;", "&copy; A �", false),
        ("a<img title='broken", "a broken ", true),
    ] {
        for final_scalar in [false, true] {
            let clock = Timer::new();
            let mut work = budget(10000, 10000);
            let pointer = std::ptr::from_mut(&mut work);
            let mut cursor = Cursor::new(&mut work, &clock, Tick(1)).unwrap();
            let chars: Vec<_> = source.chars().collect();
            let mut offset = 0;
            let mut out = String::new();
            let mut done = false;
            for turn in 1..10000 {
                let last = offset == chars.len() || (final_scalar && offset + 1 == chars.len());
                let p = cursor.poll(chars.get(offset).copied(), last).unwrap();
                if p.consumed {
                    offset += 1;
                }
                if let Some(c) = p.scalar {
                    out.push(c);
                }
                assert_eq!(10000 - cursor.work.remaining().records, turn);
                assert_eq!(
                    10000 - cursor.work.remaining().output_bytes,
                    out.len() as u64
                );
                if p.status == Status::Complete {
                    assert_eq!(offset, chars.len());
                    assert_eq!(out, expected);
                    let before = cursor.work.remaining();
                    assert_eq!(
                        cursor.poll(Some('!'), false).unwrap(),
                        Progress {
                            consumed: false,
                            scalar: None,
                            status: Status::Complete
                        }
                    );
                    assert_eq!(cursor.work.remaining(), before);
                    let expected = Diagnostics {
                        malformed_markup: malformed,
                    };
                    assert_eq!(cursor.value(), Some(expected));
                    let (work, diagnostics) = cursor.finish().unwrap();
                    assert_eq!(std::ptr::from_mut(work), pointer);
                    assert_eq!(diagnostics, expected);
                    done = true;
                    break;
                }
            }
            assert!(done);
        }
    }
}
#[test]
fn source_gaps_are_charged_and_incomplete_finish_refuses() {
    let clock = Timer::new();
    let mut work = budget(5, 5);
    let mut cursor = Cursor::new(&mut work, &clock, Tick(1)).unwrap();
    assert_eq!(
        cursor.poll(None, false).unwrap(),
        Progress {
            consumed: false,
            scalar: None,
            status: Status::NeedInput
        }
    );
    assert_eq!(cursor.work.remaining().records, 4);
    assert!(cursor.value().is_none());
    assert_eq!(cursor.finish().err(), Some(Error::State));
}
#[test]
fn quota_clock_regression_and_post_output_expiry_are_sticky() {
    for mode in 0..4 {
        let clock = Timer::new();
        let mut work = budget(
            if mode == 0 { 0 } else { 10 },
            if mode == 1 { 0 } else { 10 },
        );
        let mut cursor = Cursor::new(&mut work, &clock, Tick(1)).unwrap();
        if mode == 2 {
            clock.tick.store(0, Ordering::Relaxed);
        }
        if mode == 3 {
            clock.after(4);
        }
        let error = match mode {
            0 => Error::Work(Stop::Records),
            1 => Error::Work(Stop::OutputBytes),
            2 => Error::Policy(PolicyError::Invalid),
            _ => Error::Work(Stop::Deadline),
        };
        assert_eq!(cursor.poll(Some('😀'), true), Err(error));
        if mode == 3 {
            assert_eq!(cursor.work.remaining().output_bytes, 6);
        }
        assert!(cursor.value().is_none());
        let calls = clock.calls.load(Ordering::Relaxed);
        assert_eq!(cursor.poll(None, true), Err(error));
        assert_eq!(cursor.check(), Err(error));
        assert_eq!(cursor.finish().err(), Some(error));
        assert_eq!(clock.calls.load(Ordering::Relaxed), calls);
    }
}
#[test]
fn construction_active_cached_and_finish_checks_are_fresh() {
    let clock = Timer::new();
    let mut work = budget(10, 10);
    assert_eq!(
        Cursor::new(&mut work, &clock, Tick(2)).err(),
        Some(Error::Policy(PolicyError::Invalid))
    );
    for mode in 0..4 {
        let clock = Timer::new();
        let mut work = budget(10, 10);
        if mode == 0 {
            clock.after(0);
            assert_eq!(
                Cursor::new(&mut work, &clock, Tick(1)).err(),
                Some(Error::Work(Stop::Deadline))
            );
            continue;
        }
        let mut cursor = Cursor::new(&mut work, &clock, Tick(1)).unwrap();
        if mode != 1 {
            assert_eq!(cursor.poll(None, true).unwrap().status, Status::Complete);
        }
        clock.after(0);
        let error = Error::Work(Stop::Deadline);
        if mode == 3 {
            assert_eq!(cursor.finish().err(), Some(error));
        } else {
            assert_eq!(cursor.poll(None, true), Err(error));
            assert!(cursor.value().is_none());
        }
    }
}

#[test]
fn pending_reference_gaps_are_charged_and_protocol_misuse_retires() {
    let clock = Timer::new();
    let mut work = budget(100, 100);
    let mut cursor = Cursor::new(&mut work, &clock, Tick(1)).unwrap();
    for c in "&amp".chars() {
        assert!(cursor.poll(Some(c), false).unwrap().consumed);
    }
    assert!(!cursor.poll(Some(';'), false).unwrap().consumed);
    assert!(cursor.poll(Some(';'), false).unwrap().consumed);
    let before = cursor.work.remaining();
    let p = cursor.poll(None, false).unwrap();
    assert_eq!(p.scalar, Some('&'));
    assert!(!p.consumed);
    assert_eq!(before.records - cursor.work.remaining().records, 1);
    assert_eq!(
        before.output_bytes - cursor.work.remaining().output_bytes,
        1
    );
    assert_eq!(cursor.poll(None, true).unwrap().status, Status::Complete);
    cursor.finish().unwrap();

    let mut work = budget(100, 100);
    let mut cursor = Cursor::new(&mut work, &clock, Tick(1)).unwrap();
    for c in "&amp".chars() {
        assert!(cursor.poll(Some(c), false).unwrap().consumed);
    }
    assert!(!cursor.poll(Some(';'), false).unwrap().consumed);
    assert_eq!(cursor.poll(None, true), Err(Error::State));
    let calls = clock.calls.load(Ordering::Relaxed);
    assert_eq!(cursor.check(), Err(Error::State));
    assert_eq!(cursor.poll(None, true), Err(Error::State));
    assert_eq!(cursor.finish().err(), Some(Error::State));
    assert_eq!(clock.calls.load(Ordering::Relaxed), calls);
}
#[test]
fn explicit_check_cached_postcheck_and_original_quota_precedence_are_pinned() {
    for mode in 0..3 {
        let clock = Timer::new();
        let mut work = budget(if mode == 2 { 0 } else { 10 }, 10);
        let mut cursor = Cursor::new(&mut work, &clock, Tick(1)).unwrap();
        if mode == 0 {
            clock.after(0);
            assert_eq!(cursor.check(), Err(Error::Work(Stop::Deadline)));
        } else if mode == 1 {
            assert_eq!(cursor.poll(None, true).unwrap().status, Status::Complete);
            clock.after(2);
            assert_eq!(cursor.poll(None, true), Err(Error::Work(Stop::Deadline)));
        } else {
            let before = clock.calls.load(Ordering::Relaxed);
            clock.after(3);
            assert_eq!(
                cursor.poll(Some('x'), false),
                Err(Error::Work(Stop::Records))
            );
            assert_eq!(clock.calls.load(Ordering::Relaxed) - before, 3);
        }
    }
}

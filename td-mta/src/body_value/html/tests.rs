#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;
use crate::ports::{Deadline, Time};
use std::sync::atomic::{AtomicU64, Ordering};

struct TestClock(AtomicU64);
impl TestClock {
    fn new() -> Self {
        Self(AtomicU64::new(1))
    }
    fn set(&self, tick: u64) {
        self.0.store(tick, Ordering::Relaxed);
    }
}
impl Clock for TestClock {
    fn sample(&self) -> Result<Time, PolicyError> {
        Ok(Time {
            utc_ms: 0,
            monotonic: Tick(self.0.load(Ordering::Relaxed)),
        })
    }
}
fn budget(records: u64, bytes: u64) -> Meter {
    Meter::new(
        Deadline::after(Tick(0), 100).unwrap(),
        Charge {
            records,
            output_bytes: bytes,
            ..Charge::default()
        },
    )
}
fn drive(cursor: &mut Cursor<'_, '_>, text: &str, last_scalar: bool) -> Cutoff {
    let chars: Vec<_> = text.chars().collect();
    let mut position = 0;
    for _ in 0..text.len() * 4 + 20 {
        let input = chars.get(position).copied();
        let last = position == chars.len() || (last_scalar && position + 1 == chars.len());
        let step = cursor.poll(input, last).unwrap();
        if step.consumed {
            position += 1;
        }
        if step.status == Status::Complete {
            assert_eq!(position, chars.len());
            return cursor.value().unwrap();
        }
        assert!(cursor.value().is_none());
    }
    panic!("planner did not finish");
}
fn plan(text: &str, cap: u64, last: bool) -> Cutoff {
    let clock = TestClock::new();
    let mut work = budget(100000, 100000);
    let mut cursor = Cursor::new(cap, &mut work, &clock).unwrap();
    let value = drive(&mut cursor, text, last);
    assert_eq!(cursor.finish().unwrap(), value);
    value
}
#[test]
fn literal_cutoffs_cover_quotes_tags_unicode_and_incomplete_eof() {
    for (text, cap, bytes, truncated, problem) in [
        ("", 1, 0, false, false),
        ("ab<b>cd</b>ef", 4, 2, true, false),
        ("ab<b>cd</b>ef", 5, 5, true, false),
        ("ab<b>cd</b>ef", 9, 7, true, false),
        ("ab<b>cd</b>ef", 11, 11, true, false),
        ("x<a title='>'>z", 12, 1, true, false),
        ("x<a title=\">\">z", 12, 1, true, false),
        ("x<a title='\" >'>z", 14, 1, true, false),
        ("x<a title='\\'>yz", 15, 15, true, false),
        ("x&LT;y", 3, 3, true, false),
        ("x<<a>yz", 5, 5, true, false),
        ("x<!-- >yz", 7, 7, true, false),
        ("x<script><y", 10, 9, true, false),
        ("x<unfinished", 12, 12, false, false),
        ("x<unfinished", 11, 1, true, false),
        ("<x", 1, 0, true, false),
        ("a < b and more", 8, 2, true, false),
        ("x<p title=it's data-x='>'>tail", 24, 24, true, false),
        ("x<p title=it's data-x='>'>tail", 25, 25, true, false),
        ("é<x>😀z", 8, 5, true, false),
        ("é<x>😀z", 9, 9, true, false),
        ("a\r\n<b>z", 4, 2, true, false),
        ("a\r<x", 3, 2, true, false),
        ("a<q>\u{10ffff}", 1, 1, true, true),
        ("\0\t'e\u{301}", 0, 6, false, false),
    ] {
        for last in [false, true] {
            assert_eq!(
                plan(text, cap, last),
                Cutoff {
                    prefix: Prefix::new(bytes),
                    is_truncated: truncated,
                    is_encoding_problem: problem
                },
                "{text:?} cap={cap} last={last}"
            );
        }
    }
}
#[test]
fn all_cap_positions_use_literal_safe_boundaries() {
    // Projected bytes: e + combining acute (3), tag (9), emoji (4), close (4).
    let text = "e\u{301}<a x='>'>😀</a>";
    let safe = [0, 1, 3, 12, 16, 20];
    for cap in 0..=21 {
        let expected = if cap == 0 || cap >= 20 {
            20
        } else {
            *safe.iter().filter(|&&n| n <= cap).max().unwrap()
        };
        for last in [false, true] {
            assert_eq!(
                plan(text, cap, last),
                Cutoff {
                    prefix: Prefix::new(expected),
                    is_truncated: cap != 0 && cap < 20,
                    is_encoding_problem: false,
                }
            );
        }
    }
}
#[test]
fn pending_turns_charge_once_and_do_not_consume_replayed_input() {
    let clock = TestClock::new();
    let mut work = budget(20, 20);
    let mut cursor = Cursor::new(1, &mut work, &clock).unwrap();
    assert_eq!(cursor.poll(None, false).unwrap().status, Status::NeedInput);
    let value = drive(&mut cursor, "a\r\nb", false);
    assert_eq!(value.prefix, Prefix::new(1));
    assert!(value.is_truncated);
    cursor.finish().unwrap();
    // Four decoded scalars, three normalized scalars, three projected bytes.
    assert_eq!(work.remaining().records, 13);
    assert_eq!(work.remaining().output_bytes, 17);
}
#[test]
fn full_tail_is_funded_and_failures_hide_the_plan() {
    for (records, bytes, expected) in [
        (0, 10, Stop::Records),
        (1, 10, Stop::Records),
        (10, 0, Stop::OutputBytes),
        (10, 1, Stop::OutputBytes),
    ] {
        let clock = TestClock::new();
        let mut work = budget(records, bytes);
        let mut cursor = Cursor::new(1, &mut work, &clock).unwrap();
        let mut position = 0;
        let chars = ['a', 'b'];
        let error = loop {
            match cursor.poll(chars.get(position).copied(), position == 2) {
                Ok(step) => {
                    assert_ne!(step.status, Status::Complete);
                    if step.consumed {
                        position += 1;
                    }
                }
                Err(error) => break error,
            }
        };
        assert_eq!(error, Error::Work(expected));
        assert!(cursor.value().is_none());
        clock.set(100);
        assert_eq!(cursor.poll(None, true), Err(error));
        assert_eq!(cursor.finish(), Err(error));
    }
    let clock = TestClock::new();
    let mut work = budget(4, 2);
    let mut cursor = Cursor::new(1, &mut work, &clock).unwrap();
    assert_eq!(drive(&mut cursor, "ab", false).prefix, Prefix::new(1));
    cursor.finish().unwrap();
    assert_eq!(work.remaining().records, 0);
    assert_eq!(work.remaining().output_bytes, 0);
}
#[test]
fn freshness_incomplete_finish_and_overflow_are_terminal() {
    let clock = TestClock::new();
    let mut work = budget(100, 100);
    assert_eq!(
        Cursor::new(1, &mut work, &clock).unwrap().finish(),
        Err(Error::State)
    );
    for tick in [0, 100] {
        clock.set(1);
        let mut work = budget(100, 100);
        let mut cursor = Cursor::new(1, &mut work, &clock).unwrap();
        drive(&mut cursor, "ab", false);
        clock.set(tick);
        let error = if tick == 0 {
            Error::Policy(PolicyError::Invalid)
        } else {
            Error::Work(Stop::Deadline)
        };
        assert_eq!(cursor.poll(None, true), Err(error));
        assert!(cursor.value().is_none());
        clock.set(1);
        assert_eq!(cursor.finish(), Err(error));
    }
    let mut work = budget(100, 100);
    let mut cursor = Cursor::new(1, &mut work, &clock).unwrap();
    cursor.total = u64::MAX;
    cursor.pending = Some('a');
    assert_eq!(cursor.poll(None, true), Err(Error::SizeOverflow));
    assert_eq!(cursor.finish(), Err(Error::SizeOverflow));
}
#[test]
fn arbitrarily_long_tags_need_only_constant_owner_storage() {
    let text = format!("a<x title='{}'>z", "q".repeat(16000));
    assert_eq!(plan(&text, 8000, false).prefix, Prefix::new(1));
    assert_eq!(plan(&text, 0, false).prefix, Prefix::new(text.len() as u64));
    let first_tag = format!("<x title='{}'>z", "q".repeat(16000));
    assert_eq!(plan(&first_tag, 8000, false).prefix, Prefix::Empty);
    assert!(std::mem::size_of::<Cursor<'_, '_>>() <= 256);
}

#[test]
fn finish_and_post_turn_admission_are_fresh_and_keep_precedence() {
    let clock = TestClock::new();
    let mut work = budget(100, 100);
    let mut cursor = Cursor::new(1, &mut work, &clock).unwrap();
    drive(&mut cursor, "ab", false);
    clock.set(100);
    assert_eq!(cursor.finish(), Err(Error::Work(Stop::Deadline)));

    let clock = TestClock::new();
    let mut work = budget(100, 100);
    let mut cursor = Cursor::new(1, &mut work, &clock).unwrap();
    let cached = drive(&mut cursor, "ab", false);
    clock.set(100);
    assert_eq!(cursor.value(), Some(cached));
    assert_eq!(cursor.check(), Err(Error::Work(Stop::Deadline)));
    assert!(cursor.value().is_none());
    assert_eq!(cursor.finish(), Err(Error::Work(Stop::Deadline)));

    struct FaultClock(AtomicU64);
    impl Clock for FaultClock {
        fn sample(&self) -> Result<Time, PolicyError> {
            if self.0.fetch_add(1, Ordering::Relaxed) == 3 {
                return Err(PolicyError::Busy);
            }
            Ok(Time {
                utc_ms: 0,
                monotonic: Tick(1),
            })
        }
    }
    // Constructor, poll precheck, step admission, failing postcheck.
    let clock = FaultClock(AtomicU64::new(0));
    let mut work = budget(0, 100);
    let mut cursor = Cursor::new(1, &mut work, &clock).unwrap();
    assert_eq!(
        cursor.poll(Some('x'), true),
        Err(Error::Policy(PolicyError::Busy))
    );
    assert!(cursor.value().is_none());
    assert_eq!(cursor.finish(), Err(Error::Policy(PolicyError::Busy)));
    assert_eq!(clock.0.load(Ordering::Relaxed), 4);
    assert_eq!(work.stopped(), Some(Stop::Records));
}

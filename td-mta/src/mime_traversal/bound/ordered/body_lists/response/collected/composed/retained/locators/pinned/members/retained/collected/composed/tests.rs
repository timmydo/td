#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::super::super::super::super::tests::TestClock;
use super::super::super::super::tests::with_bound;
use super::super::{Cell, Collecting};
use super::*;
use crate::{
    mime_traversal::bound::ordered::tests::SOURCE,
    ports::{BlobReader, Clock, Error as PolicyError, Time},
};
use std::sync::atomic::{AtomicU64, Ordering};
pub(super) const SIMPLE: &[u8] = b"\r\nabc\r\n";
pub(super) fn with_collection<T>(
    source: &[u8],
    base: u64,
    clock: &dyn Clock,
    run: impl FnOnce(Serialized<'_, '_, '_, '_, '_, '_, '_, '_, '_, '_, '_>) -> T,
) -> T {
    with_bound(source, base, clock, |bound| {
        let count = bound.value().unwrap().candidates.len();
        let mut backing = vec![[0; 512]; count];
        let mut cells: Vec<_> = backing.iter_mut().map(|b| Cell::new(b)).collect();
        let mut collecting = Collecting::new(bound, &mut cells, Tick(1)).unwrap();
        for _ in 0..count {
            let mut child = collecting.next(Tick(1)).unwrap();
            let mut complete = false;
            for _ in 0..10000 {
                if child.poll(Tick(1)).unwrap() == super::super::super::RetainStatus::Complete {
                    complete = true;
                    break;
                }
            }
            assert!(complete);
            child.finish(Tick(1)).unwrap();
        }
        run(collecting.finish(Tick(1)).unwrap())
    })
}
pub(super) fn costs(cursor: &Cursor<'_, '_, '_, '_, '_, '_, '_, '_, '_, '_, '_>) -> [u64; 5] {
    let s = &cursor
        .source
        .source
        .original
        .source
        .original
        .source
        .projected
        .structure;
    let w = s.work.remaining();
    [
        s.budget.source_bytes_remaining(),
        s.budget.steps_remaining(),
        w.io_bytes,
        w.records,
        w.output_bytes,
    ]
}
pub(super) fn drain(
    cursor: &mut Cursor<'_, '_, '_, '_, '_, '_, '_, '_, '_, '_, '_>,
    width: usize,
) -> Result<Vec<u8>, Error> {
    let mut bytes = Vec::new();
    for _ in 0..10000 {
        let mut output = [0xa5; 128];
        let before = costs(cursor);
        let p = cursor.poll(Tick(1), &mut output[..width])?;
        let after = costs(cursor);
        assert_eq!(before[0], after[0]);
        assert_eq!(before[2], after[2]);
        assert_eq!(before[1] - after[1], 1);
        assert_eq!(before[4] - after[4], p.written as u64);
        assert!(p.written <= 64);
        assert!(output[p.written..].iter().all(|b| *b == 0xa5));
        bytes.extend_from_slice(&output[..p.written]);
        if p.status == Status::Complete {
            return Ok(bytes);
        }
    }
    panic!("source-bound composition stalled")
}
fn expected(source: &[u8], mode: Mode, fragments: &[Cell<'_>]) -> String {
    let part = |i: usize| {
        format!(
            "{{{},\"subParts\":null}}",
            std::str::from_utf8(fragments[i].value().unwrap().members).unwrap()
        )
    };
    if mode == Mode::Structure {
        if source == SOURCE {
            return format!(
                "\"bodyStructure\":{{{},\"subParts\":[{},{}]}}",
                std::str::from_utf8(fragments[0].value().unwrap().members).unwrap(),
                part(1),
                part(2)
            );
        }
        return format!("\"bodyStructure\":{}", part(0));
    }
    if source == SOURCE {
        format!(
            "\"textBody\":[{}],\"htmlBody\":[{}],\"attachments\":[{}],\"hasAttachment\":true",
            part(2),
            part(2),
            part(1)
        )
    } else {
        format!(
            "\"textBody\":[{}],\"htmlBody\":[{}],\"attachments\":[],\"hasAttachment\":false",
            part(0),
            part(0)
        )
    }
}
pub(super) fn deadline(actual: bool) -> Error {
    if actual {
        Error::Parent(PolicyError::Deadline)
    } else {
        Error::Original(OriginalError::Admission(crate::nfc::Error::Work(
            crate::admission::work::Stop::Deadline,
        )))
    }
}
#[test]
fn literal_tree_lists_keep_whole_members_and_every_original_owner() {
    for source in [SIMPLE, SOURCE] {
        for base in [0, 17, u64::MAX - source.len() as u64] {
            for mode in [Mode::Structure, Mode::Lists] {
                for width in [1, 2, 7, 64, 128] {
                    let clock = TestClock::new();
                    with_collection(source, base, &clock, |serialized| {
                        let value = serialized.value().unwrap();
                        let whole = value.members.as_ptr();
                        let candidates = value.original.candidates.as_ptr();
                        let prior = value.original.original.members.as_ptr();
                        let expected = expected(source, mode, value.members);
                        let mut cursor = Cursor::new(serialized, mode, Tick(1)).unwrap();
                        let before = costs(&cursor);
                        assert_eq!(
                            cursor.poll(Tick(1), &mut []),
                            Ok(Progress {
                                written: 0,
                                status: Status::NeedOutput
                            })
                        );
                        assert_eq!(costs(&cursor), before);
                        let output = drain(&mut cursor, width).unwrap();
                        assert_eq!(output, expected.as_bytes());
                        let after = costs(&cursor);
                        assert_eq!(before[4] - after[4], output.len() as u64);
                        assert_eq!(cursor.value().unwrap().0, mode);
                        assert_eq!(
                            cursor.poll(Tick(100), &mut []),
                            Ok(Progress {
                                written: 0,
                                status: Status::Complete
                            })
                        );
                        assert_eq!(costs(&cursor), after);
                        let mut composed = cursor.finish(Tick(1)).unwrap();
                        composed.check_deadline(Tick(1)).unwrap();
                        let (serialized, actual) = composed.finish(Tick(1)).unwrap();
                        assert_eq!(actual, mode);
                        let value = serialized.value().unwrap();
                        assert_eq!(value.members.as_ptr(), whole);
                        assert_eq!(value.original.candidates.as_ptr(), candidates);
                        assert_eq!(value.original.original.members.as_ptr(), prior);
                        let (bound, members) = serialized.finish(Tick(1)).unwrap();
                        assert_eq!(members.as_ptr(), whole);
                        let (_, mut parent) = bound.finish(Tick(1)).unwrap();
                        assert_eq!(parent.read_at(0, &mut [0; 2]).unwrap(), 2);
                    });
                }
            }
        }
    }
}
#[test]
fn constructor_freshly_checks_both_domains() {
    for actual in [false, true] {
        let clock = TestClock::new();
        with_collection(SIMPLE, 0, &clock, |serialized| {
            let now = if actual {
                clock.expire();
                Tick(1)
            } else {
                Tick(100)
            };
            assert_eq!(
                Cursor::new(serialized, Mode::Lists, now).err(),
                Some(deadline(actual))
            );
        });
    }
}
#[test]
fn selected_funded_prefixes_and_complete_cursor_finish_remain_fresh() {
    for mode in [Mode::Structure, Mode::Lists] {
        let clock = TestClock::new();
        let turns = with_collection(SIMPLE, 0, &clock, |serialized| {
            let mut cursor = Cursor::new(serialized, mode, Tick(1)).unwrap();
            for turn in 1..10000 {
                if cursor.poll(Tick(1), &mut [0; 64]).unwrap().status == Status::Complete {
                    return turn;
                }
            }
            panic!("baseline stalled")
        });
        for prefix in [0, 1, turns / 2, turns - 1, turns] {
            for fault in 0..3 {
                let clock = TestClock::new();
                with_collection(SIMPLE, 0, &clock, |serialized| {
                    let mut cursor = Cursor::new(serialized, mode, Tick(1)).unwrap();
                    for _ in 0..prefix {
                        cursor.poll(Tick(1), &mut [0; 64]).unwrap();
                    }
                    if fault == 0 {
                        if prefix == turns {
                            assert!(cursor.finish(Tick(1)).is_ok());
                        } else {
                            assert_eq!(cursor.finish(Tick(1)).err(), Some(Error::InvalidState));
                        }
                        return;
                    }
                    let actual = fault == 2;
                    let tick = if actual {
                        clock.expire();
                        Tick(1)
                    } else {
                        Tick(100)
                    };
                    let e = deadline(actual);
                    if prefix == turns {
                        assert_eq!(cursor.finish(tick).err(), Some(e));
                    } else {
                        assert_eq!(cursor.check_deadline(tick), Err(e));
                        assert!(cursor.value().is_none());
                        assert_eq!(cursor.poll(Tick(1), &mut [0; 64]), Err(e));
                        assert_eq!(cursor.finish(Tick(1)).err(), Some(e));
                    }
                });
            }
        }
    }
}
#[test]
fn completed_composed_owner_explicit_and_consuming_checks_stay_fresh() {
    for mode in [Mode::Structure, Mode::Lists] {
        for actual in [false, true] {
            for explicit in [false, true] {
                let clock = TestClock::new();
                with_collection(SIMPLE, 0, &clock, |serialized| {
                    let mut cursor = Cursor::new(serialized, mode, Tick(1)).unwrap();
                    drain(&mut cursor, 64).unwrap();
                    let mut composed = cursor.finish(Tick(1)).unwrap();
                    let tick = if actual {
                        clock.expire();
                        Tick(1)
                    } else {
                        Tick(100)
                    };
                    if explicit {
                        assert_eq!(composed.check_deadline(tick), Err(deadline(actual)));
                        assert!(composed.value().is_none());
                        assert_eq!(composed.finish(Tick(1)).err(), Some(deadline(actual)));
                    } else {
                        assert_eq!(composed.finish(tick).err(), Some(deadline(actual)));
                    }
                });
            }
        }
    }
}
#[test]
fn original_step_record_and_wire_refusal_hide_values_without_source_io() {
    for kind in 0..3 {
        let clock = TestClock::new();
        with_collection(SIMPLE, 0, &clock, |serialized| {
            let mut cursor = Cursor::new(serialized, Mode::Structure, Tick(1)).unwrap();
            let s = &mut cursor
                .source
                .source
                .original
                .source
                .original
                .source
                .projected
                .structure;
            let left = s.work.remaining();
            if kind == 2 {
                let steps = s.budget.steps_remaining();
                s.budget.charge(s.work, Tick(1), 0, steps, &mut 0).unwrap();
            } else {
                s.work
                    .charge(
                        Tick(1),
                        Charge {
                            records: if kind == 1 { left.records } else { 0 },
                            output_bytes: if kind == 0 { left.output_bytes } else { 0 },
                            ..Charge::default()
                        },
                    )
                    .unwrap();
            }
            let nfc = if kind == 2 {
                crate::nfc::Error::InterpretationLimit
            } else {
                crate::nfc::Error::Work(if kind == 1 {
                    crate::admission::work::Stop::Records
                } else {
                    crate::admission::work::Stop::OutputBytes
                })
            };
            let e = Error::Original(OriginalError::Admission(nfc));
            let mut output = [0xa5; 64];
            for _ in 0..100 {
                let before = costs(&cursor);
                let result = cursor.poll(Tick(1), &mut output);
                let after = costs(&cursor);
                assert_eq!(before[0], after[0]);
                assert_eq!(before[2], after[2]);
                match result {
                    Ok(p) => assert_eq!(p.written, 0),
                    Err(error) => {
                        assert_eq!(error, e);
                        assert_eq!(output, [0xa5; 64]);
                        assert!(cursor.value().is_none());
                        assert_eq!(cursor.poll(Tick(1), &mut output), Err(e));
                        assert_eq!(cursor.finish(Tick(1)).err(), Some(e));
                        return;
                    }
                }
            }
            panic!("quota did not refuse")
        });
    }
}
struct FenceClock {
    calls: AtomicU64,
    expire_at: AtomicU64,
}
impl Clock for FenceClock {
    fn sample(&self) -> Result<Time, PolicyError> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        Ok(Time {
            utc_ms: 0,
            monotonic: Tick(if n >= self.expire_at.load(Ordering::SeqCst) {
                u64::MAX
            } else {
                1
            }),
        })
    }
}
#[test]
fn actual_post_turn_pin_fence_wins_over_original_funding_refusal() {
    for quota in [false, true] {
        let clock = FenceClock {
            calls: AtomicU64::new(0),
            expire_at: AtomicU64::new(u64::MAX),
        };
        with_collection(SIMPLE, 0, &clock, |serialized| {
            let mut cursor = Cursor::new(serialized, Mode::Structure, Tick(1)).unwrap();
            if quota {
                let s = &mut cursor
                    .source
                    .source
                    .original
                    .source
                    .original
                    .source
                    .projected
                    .structure;
                let left = s.budget.steps_remaining();
                s.budget.charge(s.work, Tick(1), 0, left, &mut 0).unwrap();
            }
            let before = clock.calls.load(Ordering::SeqCst);
            clock.expire_at.store(before + 3, Ordering::SeqCst);
            let mut output = [0xa5; 64];
            let e = Error::Parent(PolicyError::Deadline);
            assert_eq!(cursor.poll(Tick(1), &mut output), Err(e));
            assert_eq!(output, [0xa5; 64]);
            assert!(cursor.value().is_none());
            assert_eq!(clock.calls.load(Ordering::SeqCst), before + 3);
            clock.expire_at.store(u64::MAX, Ordering::SeqCst);
            assert_eq!(cursor.poll(Tick(1), &mut output), Err(e));
            assert_eq!(cursor.finish(Tick(1)).err(), Some(e));
            assert_eq!(clock.calls.load(Ordering::SeqCst), before + 3);
        });
    }
}
pub fn probe(mut snapshot: impl FnMut()) {
    for trial in 0..8 {
        let clock = TestClock::new();
        let mode = if trial == 1 {
            Mode::Lists
        } else {
            Mode::Structure
        };
        let base = if trial == 0 {
            u64::MAX - SOURCE.len() as u64
        } else {
            0
        };
        with_collection(SOURCE, base, &clock, |serialized| {
            if trial == 7 {
                clock.expire();
            }
            snapshot();
            match Cursor::new(serialized, mode, Tick(1)) {
                Err(e) => {
                    assert_eq!(trial, 7);
                    assert_eq!(e, Error::Parent(PolicyError::Deadline));
                }
                Ok(mut cursor) => {
                    let mut output = [0; 64];
                    if trial == 6 {
                        let s = &mut cursor
                            .source
                            .source
                            .original
                            .source
                            .original
                            .source
                            .projected
                            .structure;
                        let left = s.work.remaining().output_bytes;
                        s.work
                            .charge(
                                Tick(1),
                                Charge {
                                    output_bytes: left,
                                    ..Charge::default()
                                },
                            )
                            .unwrap();
                    }
                    if trial == 2 {
                        let mut copied = false;
                        for _ in 0..10000 {
                            if cursor.poll(Tick(1), &mut output).unwrap().written != 0 {
                                copied = true;
                                break;
                            }
                        }
                        assert!(copied);
                        clock.expire();
                        assert_eq!(
                            cursor.poll(Tick(1), &mut output),
                            Err(Error::Parent(PolicyError::Deadline))
                        );
                        drop(cursor);
                    } else {
                        let mut complete = false;
                        let mut refused = false;
                        for _ in 0..10000 {
                            match cursor.poll(Tick(1), &mut output) {
                                Ok(p) if p.status == Status::Complete => {
                                    complete = true;
                                    break;
                                }
                                Ok(_) => {}
                                Err(e) => {
                                    assert_eq!(trial, 6);
                                    assert_eq!(
                                        e,
                                        Error::Original(OriginalError::Admission(
                                            crate::nfc::Error::Work(
                                                crate::admission::work::Stop::OutputBytes
                                            )
                                        ))
                                    );
                                    refused = true;
                                    break;
                                }
                            }
                        }
                        if refused {
                            drop(cursor);
                        } else {
                            assert!(complete);
                            if trial == 3 {
                                clock.expire();
                                assert_eq!(
                                    cursor.finish(Tick(1)).err(),
                                    Some(Error::Parent(PolicyError::Deadline))
                                );
                            } else {
                                let composed = cursor.finish(Tick(1)).unwrap();
                                if trial == 4 {
                                    clock.expire();
                                    assert_eq!(
                                        composed.finish(Tick(1)).err(),
                                        Some(Error::Parent(PolicyError::Deadline))
                                    );
                                } else {
                                    let (mut serialized, actual) =
                                        composed.finish(Tick(1)).unwrap();
                                    assert_eq!(actual, mode);
                                    if trial == 5 {
                                        clock.expire();
                                        assert_eq!(
                                            serialized.check_deadline(Tick(1)),
                                            Err(Error::Parent(PolicyError::Deadline))
                                        );
                                        drop(serialized);
                                    } else {
                                        let (bound, cells) = serialized.finish(Tick(1)).unwrap();
                                        assert!(!cells.is_empty());
                                        drop(bound);
                                    }
                                }
                            }
                        }
                    }
                }
            }
            snapshot();
        });
    }
}

#[test]
fn empty_output_checks_both_deadline_domains_without_charge_or_progress() {
    for actual in [false, true] {
        let clock = TestClock::new();
        with_collection(SIMPLE, 0, &clock, |serialized| {
            let mut cursor = Cursor::new(serialized, Mode::Structure, Tick(1)).unwrap();
            let before = costs(&cursor);
            let tick = if actual {
                clock.expire();
                Tick(1)
            } else {
                Tick(100)
            };
            let e = deadline(actual);
            assert_eq!(cursor.poll(tick, &mut []), Err(e));
            assert_eq!(costs(&cursor), before);
            assert!(cursor.value().is_none());
            assert_eq!(cursor.poll(Tick(1), &mut []), Err(e));
            assert_eq!(cursor.finish(Tick(1)).err(), Some(e));
        });
    }
}
#[test]
fn final_byte_post_copy_pin_refusal_hides_complete_and_stays_remembered() {
    for mode in [Mode::Structure, Mode::Lists] {
        let healthy = TestClock::new();
        let turns = with_collection(SIMPLE, 0, &healthy, |serialized| {
            let mut cursor = Cursor::new(serialized, mode, Tick(1)).unwrap();
            for turn in 1..10000 {
                if cursor.poll(Tick(1), &mut [0; 64]).unwrap().status == Status::Complete {
                    return turn;
                }
            }
            panic!("baseline stalled")
        });
        let clock = FenceClock {
            calls: AtomicU64::new(0),
            expire_at: AtomicU64::new(u64::MAX),
        };
        with_collection(SIMPLE, 0, &clock, |serialized| {
            let mut cursor = Cursor::new(serialized, mode, Tick(1)).unwrap();
            for _ in 1..turns {
                assert_eq!(
                    cursor.poll(Tick(1), &mut [0; 64]).unwrap().status,
                    Status::Yield
                );
            }
            let before = clock.calls.load(Ordering::SeqCst);
            clock.expire_at.store(before + 3, Ordering::SeqCst);
            let mut output = [0xa5; 64];
            let e = Error::Parent(PolicyError::Deadline);
            assert_eq!(cursor.poll(Tick(1), &mut output), Err(e));
            assert!(output.iter().any(|b| *b != 0xa5));
            assert!(output.starts_with(match mode {
                Mode::Structure => b",\"subParts\":null}",
                Mode::Lists => b",\"hasAttachment\":false",
            }));
            assert!(cursor.value().is_none());
            assert_eq!(clock.calls.load(Ordering::SeqCst), before + 3);
            clock.expire_at.store(u64::MAX, Ordering::SeqCst);
            assert_eq!(cursor.poll(Tick(1), &mut [0; 64]), Err(e));
            assert_eq!(cursor.finish(Tick(1)).err(), Some(e));
            assert_eq!(clock.calls.load(Ordering::SeqCst), before + 3);
        });
    }
}

#[test]
fn disagreeing_original_candidate_or_whole_member_ordinal_refuses_stickily() {
    for candidate_fault in [false, true] {
        let clock = TestClock::new();
        with_collection(SIMPLE, 0, &clock, |serialized| {
            let mut candidates = serialized.source.original.candidates.to_vec();
            if candidate_fault {
                candidates[0].ordinal = 2;
            }
            let cells: Vec<_> = serialized
                .cells
                .iter()
                .map(|cell| {
                    let mut member = cell.value().unwrap();
                    if !candidate_fault {
                        member.ordinal = 2;
                    }
                    Cell {
                        output: None,
                        member: Some(member),
                    }
                })
                .collect();
            let bound = super::super::super::super::super::Bound {
                original: super::super::super::super::super::super::Mapped {
                    source: serialized.source.original.source,
                    parent: serialized.source.original.parent,
                    candidates: &candidates,
                },
                parent: serialized.source.parent,
                failure: None,
            };
            let serialized = Serialized {
                source: bound,
                cells: &cells,
            };
            let mut cursor = Cursor::new(serialized, Mode::Structure, Tick(1)).unwrap();
            let mut output = [0xa5; 64];
            for _ in 0..2 {
                assert_eq!(
                    cursor.poll(Tick(1), &mut output).unwrap().status,
                    Status::Yield
                );
            }
            output.fill(0xa5);
            let before = costs(&cursor);
            let error = Error::Original(OriginalError::InvalidState);
            assert_eq!(cursor.poll(Tick(1), &mut output), Err(error));
            let after = costs(&cursor);
            assert_eq!(before[0], after[0]);
            assert_eq!(before[2], after[2]);
            assert_eq!(output, [0xa5; 64]);
            assert!(cursor.value().is_none());
            assert_eq!(cursor.poll(Tick(1), &mut output), Err(error));
            assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
        });
    }
}

#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::super::tests::{mapped, TestClock};
use super::*;
use crate::ports::{BlobReader, Clock, Error as PolicyError, Time};
use std::sync::atomic::{AtomicU64, Ordering};
use td_crypto::Provider;
const SIMPLE: &[u8] = b"\r\nabc\r\n";
const MULTIPART: &[u8] =
    b"Content-Type: multipart/mixed;boundary=x\r\n\r\n--x\r\n\r\nabc\r\n--x--\r\n";
pub(super) fn with_bound<T>(
    source: &[u8],
    base: u64,
    clock: &dyn Clock,
    run: impl FnOnce(Bound<'_, '_, '_, '_, '_, '_, '_, '_, '_>) -> T,
) -> T {
    let mut result = None;
    crate::store_fs::with_pinned_fixture(source, clock, |parent| {
        mapped(source, base, |original| {
            let mut binding =
                super::super::Cursor::new(original, parent, &Provider, Tick(1)).unwrap();
            while binding.poll(Tick(1)).unwrap() != crate::mime_traversal::Status::Complete {}
            result = Some(run(binding.finish(Tick(1)).unwrap()));
        });
    });
    result.unwrap()
}
pub(super) fn exercise<T>(
    source: &[u8],
    base: u64,
    ordinal: u16,
    run: impl FnOnce(Cursor<'_, '_, '_, '_, '_, '_, '_, '_, '_>, &TestClock) -> T,
) -> T {
    let clock = TestClock::new();
    with_bound(source, base, &clock, |bound| {
        run(Cursor::new(bound, ordinal, Tick(1)).unwrap(), &clock)
    })
}
fn costs(cursor: &Cursor<'_, '_, '_, '_, '_, '_, '_, '_, '_>) -> [u64; 5] {
    let structure = &cursor
        .source
        .original
        .source
        .original
        .source
        .projected
        .structure;
    let left = structure.work.remaining();
    [
        structure.budget.source_bytes_remaining(),
        structure.budget.steps_remaining(),
        left.io_bytes,
        left.records,
        left.output_bytes,
    ]
}
fn drain(
    cursor: &mut Cursor<'_, '_, '_, '_, '_, '_, '_, '_, '_>,
    width: usize,
) -> Result<Vec<u8>, Error> {
    let mut bytes = Vec::new();
    for _ in 0..10000 {
        let mut output = [0xa5; 128];
        let before = costs(cursor);
        let credit = cursor.credit.remaining();
        let progress = cursor.poll(Tick(1), &mut output[..width])?;
        let after = costs(cursor);
        assert_eq!(before[0], after[0]);
        assert_eq!(before[2], after[2]);
        assert_eq!(before[1] - after[1], 1);
        assert_eq!(before[3] - after[3], 1 + u64::from(credit == 0));
        assert_eq!(before[4] - after[4], progress.written as u64);
        assert!(progress.written <= 64);
        assert!(output[progress.written..].iter().all(|byte| *byte == 0xa5));
        bytes.extend_from_slice(&output[..progress.written]);
        if progress.status == Status::Complete {
            return Ok(bytes);
        }
    }
    panic!("member did not complete")
}
#[test]
fn literal_leaf_wire_and_container_null_keep_original_fragment_and_owners() {
    for (source, ordinal, suffix) in [
        (
            SIMPLE,
            1,
            ",\"blobId\":\"p1_444444444444444444444444444444440000000000000002000000000000000500\"",
        ),
        (MULTIPART, 1, ",\"blobId\":null"),
        (
            MULTIPART,
            2,
            ",\"blobId\":\"p1_444444444444444444444444444444440000000000000033000000000000000300\"",
        ),
    ] {
        for base in [0, 17, u64::MAX - source.len() as u64] {
            for width in [1, 2, 7, 64, 128] {
                exercise(source, base, ordinal, |mut cursor, _clock| {
                    let original = cursor.source.value().unwrap();
                    let fragment = original.original.original.fragments[usize::from(ordinal) - 1]
                        .value()
                        .unwrap()
                        .fragment;
                    let fragment_ptr = fragment.as_ptr();
                    let candidates = original.candidates.as_ptr();
                    let source_ptr = cursor
                        .source
                        .original
                        .source
                        .original
                        .source
                        .projected
                        .structure
                        .source
                        .as_ptr();
                    let whole_ptr = original.original.members.as_ptr();
                    let expected = [fragment, suffix.as_bytes()].concat();
                    let output = drain(&mut cursor, width).unwrap();
                    assert_eq!(output, expected);
                    assert_eq!(cursor.value(), Some(ordinal));
                    let before = costs(&cursor);
                    assert_eq!(
                        cursor.poll(Tick(100), &mut []),
                        Ok(Progress {
                            written: 0,
                            status: Status::Complete
                        })
                    );
                    assert_eq!(costs(&cursor), before);
                    let member = cursor.finish(Tick(1)).unwrap();
                    assert_eq!(member.value(), Some(ordinal));
                    let (bound, actual) = member.finish(Tick(1)).unwrap();
                    assert_eq!(actual, ordinal);
                    let original = bound.value().unwrap();
                    assert_eq!(
                        original.original.original.fragments[usize::from(ordinal) - 1]
                            .value()
                            .unwrap()
                            .fragment
                            .as_ptr(),
                        fragment_ptr
                    );
                    assert_eq!(original.candidates.as_ptr(), candidates);
                    assert_eq!(original.original.members.as_ptr(), whole_ptr);
                    assert_eq!(
                        bound
                            .original
                            .source
                            .original
                            .source
                            .projected
                            .structure
                            .source
                            .as_ptr(),
                        source_ptr
                    );
                    let (_, mut parent) = bound.finish(Tick(1)).unwrap();
                    assert_eq!(parent.read_at(0, &mut [0; 2]).unwrap(), 2);
                });
            }
        }
    }
}
#[test]
fn fresh_deadlines_cover_every_emission_prefix_and_complete_member() {
    let turns = exercise(SIMPLE, 0, 1, |mut cursor, _| {
        drain(&mut cursor, 64).unwrap().len().div_ceil(64)
    });
    for prefix in 0..=turns {
        for actual in [false, true] {
            exercise(SIMPLE, 0, 1, |mut cursor, clock| {
                for _ in 0..prefix {
                    cursor.poll(Tick(1), &mut [0; 64]).unwrap();
                }
                let tick = if actual {
                    clock.expire();
                    Tick(1)
                } else {
                    Tick(100)
                };
                if prefix == turns {
                    let member = cursor.finish(Tick(1));
                    if actual {
                        assert_eq!(member.err(), Some(Error::Parent(PolicyError::Deadline)));
                    } else {
                        assert_eq!(
                            member.unwrap().finish(tick).err(),
                            Some(Error::Original(super::super::super::Error::Admission(
                                crate::nfc::Error::Work(crate::admission::work::Stop::Deadline)
                            )))
                        );
                    }
                } else {
                    assert!(cursor.check_deadline(tick).is_err());
                    assert!(cursor.value().is_none());
                    let error = cursor.failure.unwrap();
                    assert_eq!(cursor.poll(Tick(1), &mut [0; 64]), Err(error));
                    assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
                }
            });
        }
    }
}
#[test]
fn empty_output_and_exact_final_byte_preserve_funded_progress() {
    exercise(SIMPLE, 0, 1, |mut cursor, _| {
        let before = costs(&cursor);
        assert_eq!(
            cursor.poll(Tick(1), &mut []),
            Ok(Progress {
                written: 0,
                status: Status::NeedOutput
            })
        );
        assert_eq!(costs(&cursor), before);
        assert!(cursor.value().is_none());
        let bytes = drain(&mut cursor, 1).unwrap();
        assert_eq!(cursor.position, bytes.len());
        assert_eq!(cursor.value(), Some(1));
    });
}

#[test]
fn constructor_freshness_precedes_invalid_ordinal_and_only_original_selection_enters() {
    for ordinal in [0, 2, u16::MAX] {
        for expired in [false, true] {
            let clock = TestClock::new();
            with_bound(SIMPLE, 0, &clock, |bound| {
                if expired {
                    clock.expire();
                }
                assert_eq!(
                    Cursor::new(bound, ordinal, Tick(1)).err(),
                    Some(if expired {
                        Error::Parent(PolicyError::Deadline)
                    } else {
                        Error::InvalidState
                    })
                );
            });
        }
    }
}
#[test]
fn quota_cutoffs_refuse_before_copying_and_hide_every_partial_value() {
    for (kind, max) in [(0, 64), (1, 2), (2, 1)] {
        for cut in 0..max {
            exercise(SIMPLE, 0, 1, |mut cursor, _| {
                let structure = &mut cursor
                    .source
                    .original
                    .source
                    .original
                    .source
                    .projected
                    .structure;
                let left = structure.work.remaining();
                if kind == 2 {
                    let steps = structure.budget.steps_remaining();
                    structure
                        .budget
                        .charge(
                            structure.work,
                            Tick(1),
                            0,
                            steps - cut,
                            &mut crate::nfc::Credit::new(),
                        )
                        .unwrap();
                } else {
                    structure
                        .work
                        .charge(
                            Tick(1),
                            Charge {
                                records: if kind == 1 { left.records - cut } else { 0 },
                                output_bytes: if kind == 0 {
                                    left.output_bytes - cut
                                } else {
                                    0
                                },
                                ..Charge::default()
                            },
                        )
                        .unwrap();
                }
                let mut output = [0xa5; 64];
                let error = cursor.poll(Tick(1), &mut output).unwrap_err();
                let expected = if kind == 2 {
                    crate::nfc::Error::InterpretationLimit
                } else {
                    crate::nfc::Error::Work(if kind == 1 {
                        crate::admission::work::Stop::Records
                    } else {
                        crate::admission::work::Stop::OutputBytes
                    })
                };
                assert_eq!(
                    error,
                    Error::Original(super::super::super::Error::Admission(expected))
                );
                assert_eq!(output, [0xa5; 64]);
                assert_eq!(cursor.position, 0);
                assert!(cursor.value().is_none());
                assert_eq!(cursor.poll(Tick(1), &mut output), Err(error));
                assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
            });
        }
    }
}
struct FenceClock {
    samples: AtomicU64,
    expire_at: AtomicU64,
}
impl Clock for FenceClock {
    fn sample(&self) -> Result<Time, PolicyError> {
        let count = self.samples.fetch_add(1, Ordering::SeqCst) + 1;
        Ok(Time {
            utc_ms: 0,
            monotonic: Tick(if count >= self.expire_at.load(Ordering::SeqCst) {
                u64::MAX
            } else {
                1
            }),
        })
    }
}
#[test]
fn actual_clock_after_copy_retires_written_prefix_and_remembers_refusal() {
    let clock = FenceClock {
        samples: AtomicU64::new(0),
        expire_at: AtomicU64::new(u64::MAX),
    };
    with_bound(SIMPLE, 0, &clock, |bound| {
        let mut cursor = Cursor::new(bound, 1, Tick(1)).unwrap();
        clock
            .expire_at
            .store(clock.samples.load(Ordering::SeqCst) + 3, Ordering::SeqCst);
        let before = costs(&cursor);
        let mut output = [0xa5; 64];
        let error = Error::Parent(PolicyError::Deadline);
        assert_eq!(cursor.poll(Tick(1), &mut output), Err(error));
        assert_ne!(output, [0xa5; 64]);
        assert_eq!(before[4] - costs(&cursor)[4], 64);
        assert!(cursor.value().is_none());
        clock.expire_at.store(u64::MAX, Ordering::SeqCst);
        let samples = clock.samples.load(Ordering::SeqCst);
        assert_eq!(cursor.poll(Tick(1), &mut output), Err(error));
        assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
        assert_eq!(clock.samples.load(Ordering::SeqCst), samples);
    });
}

/// Original source matching and file setup precede these measured intervals.
pub fn probe(mut snapshot: impl FnMut()) {
    for trial in 0..8 {
        let source = if trial == 1 { MULTIPART } else { SIMPLE };
        let base = if trial == 0 {
            u64::MAX - source.len() as u64
        } else {
            0
        };
        let clock = TestClock::new();
        with_bound(source, base, &clock, |mut bound| {
            if trial == 5 {
                let structure = &mut bound.original.source.original.source.projected.structure;
                let output = structure.work.remaining().output_bytes;
                structure
                    .work
                    .charge(
                        Tick(1),
                        Charge {
                            output_bytes: output,
                            ..Charge::default()
                        },
                    )
                    .unwrap();
            }
            if trial == 7 {
                clock.expire();
            }
            let mut output = [0xa5; 64];
            snapshot();
            if trial == 6 || trial == 7 {
                assert_eq!(
                    Cursor::new(bound, if trial == 6 { 0 } else { 1 }, Tick(1)).err(),
                    Some(if trial == 6 {
                        Error::InvalidState
                    } else {
                        Error::Parent(PolicyError::Deadline)
                    })
                );
            } else {
                let mut cursor = Cursor::new(bound, 1, Tick(1)).unwrap();
                if trial == 5 {
                    let error = Error::Original(super::super::super::Error::Admission(
                        crate::nfc::Error::Work(crate::admission::work::Stop::OutputBytes),
                    ));
                    assert_eq!(cursor.poll(Tick(1), &mut output), Err(error));
                    assert_eq!(output, [0xa5; 64]);
                    assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
                } else {
                    assert_eq!(
                        cursor.poll(Tick(1), &mut output).unwrap().status,
                        Status::Yield
                    );
                    if trial == 2 {
                        clock.expire();
                        let error = Error::Parent(PolicyError::Deadline);
                        assert_eq!(cursor.poll(Tick(1), &mut output), Err(error));
                        assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
                    } else {
                        while cursor.poll(Tick(1), &mut output).unwrap().status != Status::Complete
                        {
                        }
                        if trial == 3 {
                            clock.expire();
                            assert_eq!(
                                cursor.finish(Tick(1)).err(),
                                Some(Error::Parent(PolicyError::Deadline))
                            );
                        } else {
                            let member = cursor.finish(Tick(1)).unwrap();
                            if trial == 4 {
                                clock.expire();
                                assert_eq!(
                                    member.finish(Tick(1)).err(),
                                    Some(Error::Parent(PolicyError::Deadline))
                                );
                            } else {
                                let (bound, ordinal) = member.finish(Tick(1)).unwrap();
                                assert_eq!(ordinal, 1);
                                drop(bound);
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
fn explicit_completed_member_checks_retire_owner_and_hide_passive_value() {
    for actual in [false, true] {
        exercise(SIMPLE, 0, 1, |mut cursor, clock| {
            drain(&mut cursor, 64).unwrap();
            let mut member = cursor.finish(Tick(1)).unwrap();
            let tick = if actual {
                clock.expire();
                Tick(1)
            } else {
                Tick(100)
            };
            let error = member.check_deadline(tick).unwrap_err();
            assert_eq!(member.failure, Some(error));
            assert!(member.value().is_none());
            assert_eq!(member.check_deadline(Tick(1)), Err(error));
            assert_eq!(member.finish(Tick(1)).err(), Some(error));
        });
    }
}

#[test]
fn post_work_pin_deadline_overrides_original_quota_failure() {
    for steps in [false, true] {
        let clock = FenceClock {
            samples: AtomicU64::new(0),
            expire_at: AtomicU64::new(u64::MAX),
        };
        with_bound(SIMPLE, 0, &clock, |bound| {
            let mut cursor = Cursor::new(bound, 1, Tick(1)).unwrap();
            let structure = &mut cursor
                .source
                .original
                .source
                .original
                .source
                .projected
                .structure;
            if steps {
                let remaining = structure.budget.steps_remaining();
                structure
                    .budget
                    .charge(
                        structure.work,
                        Tick(1),
                        0,
                        remaining,
                        &mut crate::nfc::Credit::new(),
                    )
                    .unwrap();
            } else {
                let remaining = structure.work.remaining().output_bytes;
                structure
                    .work
                    .charge(
                        Tick(1),
                        Charge {
                            output_bytes: remaining,
                            ..Charge::default()
                        },
                    )
                    .unwrap();
            }
            let samples = clock.samples.load(Ordering::SeqCst);
            clock.expire_at.store(samples + 3, Ordering::SeqCst);
            let mut output = [0xa5; 64];
            let error = Error::Parent(PolicyError::Deadline);
            assert_eq!(cursor.poll(Tick(1), &mut output), Err(error));
            assert_eq!(output, [0xa5; 64]);
            assert_eq!(cursor.position, 0);
            assert!(cursor.value().is_none());
            assert_eq!(cursor.failure, Some(error));
            assert_eq!(clock.samples.load(Ordering::SeqCst), samples + 3);
            clock.expire_at.store(u64::MAX, Ordering::SeqCst);
            assert_eq!(cursor.poll(Tick(1), &mut output), Err(error));
            assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
            assert_eq!(clock.samples.load(Ordering::SeqCst), samples + 3);
        });
    }
}

#[test]
fn healthy_incomplete_cursor_cannot_create_member_at_any_prefix() {
    let total = exercise(SIMPLE, 0, 1, |mut cursor, _| {
        drain(&mut cursor, 64).unwrap().len().div_ceil(64)
    });
    for prefix in 0..total {
        exercise(SIMPLE, 0, 1, |mut cursor, _| {
            for _ in 0..prefix {
                cursor.poll(Tick(1), &mut [0; 64]).unwrap();
            }
            assert!(cursor.value().is_none());
            assert_eq!(cursor.finish(Tick(1)).err(), Some(Error::InvalidState));
        });
    }
}
#[test]
fn constructor_and_empty_output_freshly_check_both_deadline_domains() {
    let clock = TestClock::new();
    with_bound(SIMPLE, 0, &clock, |bound| {
        assert_eq!(
            Cursor::new(bound, 1, Tick(100)).err(),
            Some(Error::Original(super::super::super::Error::Admission(
                crate::nfc::Error::Work(crate::admission::work::Stop::Deadline)
            )))
        );
    });
    for actual in [false, true] {
        exercise(SIMPLE, 0, 1, |mut cursor, clock| {
            let before = costs(&cursor);
            let tick = if actual {
                clock.expire();
                Tick(1)
            } else {
                Tick(100)
            };
            let error = cursor.poll(tick, &mut []).unwrap_err();
            let expected = if actual {
                Error::Parent(PolicyError::Deadline)
            } else {
                Error::Original(super::super::super::Error::Admission(
                    crate::nfc::Error::Work(crate::admission::work::Stop::Deadline),
                ))
            };
            assert_eq!(error, expected);
            assert_eq!(costs(&cursor), before);
            assert_eq!(cursor.position, 0);
            assert!(cursor.value().is_none());
            assert_eq!(cursor.poll(Tick(1), &mut []), Err(error));
            assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
        });
    }
}

#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::super::super::tests::TestClock;
use super::super::tests::with_bound;
use super::*;
use crate::{
    admission::work::Charge,
    ports::{BlobReader, Error as PolicyError},
};
const SIMPLE: &[u8] = b"\r\nabc\r\n";
const MULTIPART: &[u8] =
    b"Content-Type: multipart/mixed;boundary=x\r\n\r\n--x\r\n\r\nabc\r\n--x--\r\n";
fn deadline_error(actual: bool) -> Error {
    if actual {
        Error::Parent(PolicyError::Deadline)
    } else {
        Error::Original(super::super::super::super::Error::Admission(
            crate::nfc::Error::Work(crate::admission::work::Stop::Deadline),
        ))
    }
}
fn costs(cursor: &Cursor<'_, '_, '_, '_, '_, '_, '_, '_, '_, '_>) -> [u64; 5] {
    let structure = &cursor
        .emitter
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
fn drain(cursor: &mut Cursor<'_, '_, '_, '_, '_, '_, '_, '_, '_, '_>) -> Result<(), Error> {
    for _ in 0..10000 {
        if cursor.poll(Tick(1))? == RetainStatus::Complete {
            return Ok(());
        }
    }
    panic!("retention did not complete")
}
fn expected(source: &[u8], ordinal: u16) -> (Vec<u8>, [u64; 5]) {
    let clock = TestClock::new();
    with_bound(source, 0, &clock, |bound| {
        let mut cursor = super::super::Cursor::new(bound, ordinal, Tick(1)).unwrap();
        let structure = &cursor
            .source
            .original
            .source
            .original
            .source
            .projected
            .structure;
        let left = structure.work.remaining();
        let before = [
            structure.budget.source_bytes_remaining(),
            structure.budget.steps_remaining(),
            left.io_bytes,
            left.records,
            left.output_bytes,
        ];
        let mut bytes = Vec::new();
        for _ in 0..10000 {
            let mut output = [0; 64];
            let progress = cursor.poll(Tick(1), &mut output).unwrap();
            bytes.extend_from_slice(&output[..progress.written]);
            if progress.status == Status::Complete {
                let structure = &cursor
                    .source
                    .original
                    .source
                    .original
                    .source
                    .projected
                    .structure;
                let left = structure.work.remaining();
                let after = [
                    structure.budget.source_bytes_remaining(),
                    structure.budget.steps_remaining(),
                    left.io_bytes,
                    left.records,
                    left.output_bytes,
                ];
                return (
                    bytes,
                    std::array::from_fn(|index| before[index] - after[index]),
                );
            }
        }
        panic!("bare emission did not complete")
    })
}
#[test]
fn whole_window_matches_bare_emission_and_preserves_all_original_owners() {
    for (source, ordinal) in [(SIMPLE, 1), (MULTIPART, 1), (MULTIPART, 2)] {
        let (bytes, debits) = expected(source, ordinal);
        for base in [0, 17, u64::MAX - source.len() as u64] {
            for extra in [0, 1, 31] {
                let clock = TestClock::new();
                with_bound(source, base, &clock, |bound| {
                    let original = bound.value().unwrap();
                    let candidates = original.candidates.as_ptr();
                    let fragment = original.original.original.fragments[usize::from(ordinal) - 1]
                        .value()
                        .unwrap()
                        .fragment;
                    let reservation = window_bound(fragment.len()).unwrap();
                    if original.candidates[usize::from(ordinal) - 1]
                        .locator()
                        .is_some()
                    {
                        assert_eq!(reservation, bytes.len());
                    } else {
                        assert!(bytes.len() <= reservation);
                    }
                    let mut output = vec![0xa5; bytes.len() + extra];
                    let output_ptr = output.as_ptr();
                    let mut cursor = Cursor::new(bound, ordinal, &mut output, Tick(1)).unwrap();
                    let before = costs(&cursor);
                    drain(&mut cursor).unwrap();
                    let after = costs(&cursor);
                    assert_eq!(
                        std::array::from_fn::<_, 5, _>(|i| before[i] - after[i]),
                        debits
                    );
                    let value = cursor.value().unwrap();
                    assert_eq!(value.ordinal, ordinal);
                    assert_eq!(value.members, bytes);
                    assert_eq!(value.members.as_ptr(), output_ptr);
                    assert_eq!(cursor.poll(Tick(100)), Ok(RetainStatus::Complete));
                    assert_eq!(costs(&cursor), after);
                    let retained = cursor.finish(Tick(1)).unwrap();
                    assert_eq!(retained.value().unwrap().members, bytes);
                    let (bound, actual, members) = retained.finish(Tick(1)).unwrap();
                    assert_eq!(actual, ordinal);
                    assert_eq!(members, bytes);
                    assert_eq!(members.as_ptr(), output_ptr);
                    assert_eq!(bound.value().unwrap().candidates.as_ptr(), candidates);
                    let (_, mut parent) = bound.finish(Tick(1)).unwrap();
                    assert_eq!(parent.read_at(0, &mut [0; 2]).unwrap(), 2);
                    assert!(output[bytes.len()..].iter().all(|byte| *byte == 0xa5));
                });
            }
        }
    }
}
#[test]
fn every_shorter_window_refuses_stickily_without_replacing_proof() {
    let (bytes, _) = expected(SIMPLE, 1);
    for capacity in 0..bytes.len() {
        let clock = TestClock::new();
        with_bound(SIMPLE, 0, &clock, |bound| {
            let mut output = vec![0xa5; capacity];
            let mut cursor = Cursor::new(bound, 1, &mut output, Tick(1)).unwrap();
            let error = Error::Original(super::super::super::super::Error::ResponseCapacity);
            assert_eq!(drain(&mut cursor), Err(error));
            assert!(cursor.value().is_none());
            let after = costs(&cursor);
            assert_eq!(cursor.poll(Tick(1)), Err(error));
            assert_eq!(costs(&cursor), after);
            assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
        });
    }
}
#[test]
fn full_window_expiry_and_completed_retention_are_fresh() {
    let (bytes, _) = expected(SIMPLE, 1);
    for actual in [false, true] {
        for phase in 0..3 {
            let clock = TestClock::new();
            with_bound(SIMPLE, 0, &clock, |bound| {
                let mut output = vec![0xa5; if phase == 0 { 0 } else { bytes.len() }];
                let mut cursor = Cursor::new(bound, 1, &mut output, Tick(1)).unwrap();
                if phase != 0 {
                    drain(&mut cursor).unwrap();
                }
                if phase == 2 {
                    let retained = cursor.finish(Tick(1)).unwrap();
                    let tick = if actual {
                        clock.expire();
                        Tick(1)
                    } else {
                        Tick(100)
                    };
                    assert_eq!(retained.finish(tick).err(), Some(deadline_error(actual)));
                    return;
                }
                let tick = if actual {
                    clock.expire();
                    Tick(1)
                } else {
                    Tick(100)
                };
                if phase == 1 {
                    assert_eq!(cursor.finish(tick).err(), Some(deadline_error(actual)));
                } else {
                    let error = cursor.poll(tick).unwrap_err();
                    assert_eq!(error, deadline_error(actual));
                    assert!(cursor.value().is_none());
                    assert_eq!(cursor.poll(Tick(1)), Err(error));
                    assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
                }
            });
        }
    }
}
#[test]
fn original_wire_record_step_cutoffs_hide_partial_retention() {
    for kind in 0..3 {
        let clock = TestClock::new();
        with_bound(SIMPLE, 0, &clock, |bound| {
            let mut output = [0xa5; 512];
            let mut cursor = Cursor::new(bound, 1, &mut output, Tick(1)).unwrap();
            let structure = &mut cursor
                .emitter
                .source
                .original
                .source
                .original
                .source
                .projected
                .structure;
            let left = structure.work.remaining();
            if kind == 2 {
                let remaining = structure.budget.steps_remaining();
                structure
                    .budget
                    .charge(structure.work, Tick(1), 0, remaining, &mut 0)
                    .unwrap();
            } else {
                structure
                    .work
                    .charge(
                        Tick(1),
                        Charge {
                            output_bytes: if kind == 0 { left.output_bytes } else { 0 },
                            records: if kind == 1 { left.records } else { 0 },
                            ..Charge::default()
                        },
                    )
                    .unwrap();
            }
            let quota = if kind == 2 {
                crate::nfc::Error::InterpretationLimit
            } else {
                crate::nfc::Error::Work(if kind == 1 {
                    crate::admission::work::Stop::Records
                } else {
                    crate::admission::work::Stop::OutputBytes
                })
            };
            let error = Error::Original(super::super::super::super::Error::Admission(quota));
            assert_eq!(cursor.poll(Tick(1)), Err(error));
            assert!(cursor.value().is_none());
            assert_eq!(cursor.emitter.failure, Some(error));
            assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
            assert_eq!(output, [0xa5; 512]);
        });
    }
}

#[test]
fn conservative_window_reservation_checks_its_entire_sum() {
    assert_eq!(window_bound(0), Ok(81));
    assert_eq!(window_bound(256), Ok(337));
    assert_eq!(window_bound(usize::MAX - 81), Ok(usize::MAX));
    assert_eq!(window_bound(usize::MAX - 80), Err(Error::InvalidState));
    assert_eq!(window_bound(usize::MAX), Err(Error::InvalidState));
}

/// Matching, mapping, file setup and complete pin verification stay cold.
pub fn probe(mut snapshot: impl FnMut()) {
    for trial in 0..8 {
        let source = if trial == 1 { MULTIPART } else { SIMPLE };
        let clock = TestClock::new();
        let base = if trial == 0 {
            u64::MAX - source.len() as u64
        } else {
            0
        };
        with_bound(source, base, &clock, |mut bound| {
            let mut output = [0xa5; 512];
            let length = if trial == 5 { 8 } else { 512 };
            if trial == 6 {
                let structure = &mut bound.original.source.original.source.projected.structure;
                let left = structure.work.remaining();
                structure
                    .work
                    .charge(
                        Tick(1),
                        Charge {
                            output_bytes: left.output_bytes,
                            ..Charge::default()
                        },
                    )
                    .unwrap();
            }
            if trial == 7 {
                clock.expire();
            }
            snapshot();
            if trial == 7 {
                assert_eq!(
                    Cursor::new(bound, 1, &mut output[..length], Tick(1)).err(),
                    Some(Error::Parent(PolicyError::Deadline))
                );
            } else {
                let mut cursor = Cursor::new(bound, 1, &mut output[..length], Tick(1)).unwrap();
                if trial == 6 {
                    let error = Error::Original(super::super::super::super::Error::Admission(
                        crate::nfc::Error::Work(crate::admission::work::Stop::OutputBytes),
                    ));
                    assert_eq!(cursor.poll(Tick(1)), Err(error));
                    assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
                } else if trial == 5 {
                    let error =
                        Error::Original(super::super::super::super::Error::ResponseCapacity);
                    assert_eq!(drain(&mut cursor), Err(error));
                    assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
                } else {
                    assert_eq!(cursor.poll(Tick(1)), Ok(RetainStatus::Yield));
                    if trial == 2 {
                        clock.expire();
                        let error = Error::Parent(PolicyError::Deadline);
                        assert_eq!(cursor.poll(Tick(1)), Err(error));
                        assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
                    } else {
                        drain(&mut cursor).unwrap();
                        if trial == 3 {
                            clock.expire();
                            assert_eq!(
                                cursor.finish(Tick(1)).err(),
                                Some(Error::Parent(PolicyError::Deadline))
                            );
                        } else {
                            let retained = cursor.finish(Tick(1)).unwrap();
                            if trial == 4 {
                                clock.expire();
                                assert_eq!(
                                    retained.finish(Tick(1)).err(),
                                    Some(Error::Parent(PolicyError::Deadline))
                                );
                            } else {
                                let (bound, ordinal, members) = retained.finish(Tick(1)).unwrap();
                                assert_eq!(ordinal, 1);
                                assert!(!members.is_empty());
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
fn every_funded_prefix_and_explicit_completed_retention_remain_fresh() {
    let (bytes, _) = expected(SIMPLE, 1);
    let turns = bytes.len().div_ceil(64);
    for prefix in 0..=turns {
        for actual in [false, true] {
            let clock = TestClock::new();
            with_bound(SIMPLE, 0, &clock, |bound| {
                let mut output = [0xa5; 512];
                let mut cursor = Cursor::new(bound, 1, &mut output, Tick(1)).unwrap();
                for _ in 0..prefix {
                    cursor.poll(Tick(1)).unwrap();
                }
                if prefix == turns {
                    let mut retained = cursor.finish(Tick(1)).unwrap();
                    let tick = if actual {
                        clock.expire();
                        Tick(1)
                    } else {
                        Tick(100)
                    };
                    let error = retained.check_deadline(tick).unwrap_err();
                    assert_eq!(error, deadline_error(actual));
                    assert!(retained.value().is_none());
                    assert_eq!(retained.check_deadline(Tick(1)), Err(error));
                    assert_eq!(retained.finish(Tick(1)).err(), Some(error));
                } else {
                    let tick = if actual {
                        clock.expire();
                        Tick(1)
                    } else {
                        Tick(100)
                    };
                    let error = cursor.check_deadline(tick).unwrap_err();
                    assert_eq!(error, deadline_error(actual));
                    assert!(cursor.value().is_none());
                    assert_eq!(cursor.poll(Tick(1)), Err(error));
                    assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
                }
            });
        }
    }
}
#[test]
fn constructor_deadline_precedes_invalid_ordinal_with_empty_window() {
    for actual in [false, true] {
        let clock = TestClock::new();
        with_bound(SIMPLE, 0, &clock, |bound| {
            let tick = if actual {
                clock.expire();
                Tick(1)
            } else {
                Tick(100)
            };
            let error = Cursor::new(bound, 0, &mut [], tick).err().unwrap();
            assert_eq!(error, deadline_error(actual));
        });
    }
}

#[test]
fn healthy_incomplete_prefix_cannot_create_whole_retention() {
    let (bytes, _) = expected(SIMPLE, 1);
    for prefix in 0..bytes.len().div_ceil(64) {
        let clock = TestClock::new();
        with_bound(SIMPLE, 0, &clock, |bound| {
            let mut output = [0xa5; 512];
            let mut cursor = Cursor::new(bound, 1, &mut output, Tick(1)).unwrap();
            for _ in 0..prefix {
                assert_eq!(cursor.poll(Tick(1)), Ok(RetainStatus::Yield));
            }
            assert!(cursor.value().is_none());
            assert_eq!(cursor.finish(Tick(1)).err(), Some(Error::InvalidState));
        });
    }
}

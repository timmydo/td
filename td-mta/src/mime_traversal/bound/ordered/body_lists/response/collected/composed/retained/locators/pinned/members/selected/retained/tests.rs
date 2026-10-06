#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::super::super::super::tests::TestClock;
use super::super::super::tests::with_bound;
use super::super::tests::{costs as bare_costs, properties};
use super::*;
use crate::{
    admission::work::{Charge, Stop},
    ports::{BlobReader, Error as PolicyError},
};
const SIMPLE: &[u8] = b"\r\nabc\r\n";
const MULTIPART: &[u8] =
    b"Content-Type: multipart/mixed;boundary=x\r\n\r\n--x\r\n\r\nabc\r\n--x--\r\n";
fn deadline_error(actual: bool) -> Error {
    if actual {
        Error::Parent(PolicyError::Deadline)
    } else {
        Error::Original(super::super::super::super::super::Error::Admission(
            crate::nfc::Error::Work(Stop::Deadline),
        ))
    }
}
fn drain(cursor: &mut Cursor<'_, '_, '_, '_, '_, '_, '_, '_, '_, '_>) -> Result<(), Error> {
    for _ in 0..10000 {
        if cursor.poll(Tick(1))? == Status::Complete {
            return Ok(());
        }
    }
    panic!("selected whole part did not complete")
}
type Turn = (usize, [u64; 5]);
fn expected(source: &[u8], ordinal: u16, bits: u16) -> (Vec<u8>, Vec<Turn>) {
    let clock = TestClock::new();
    with_bound(source, 0, &clock, |bound| {
        let mut cursor =
            super::super::Cursor::new(bound, ordinal, properties(bits), Tick(1)).unwrap();
        let mut bytes = Vec::new();
        let mut turns = Vec::new();
        for _ in 0..10000 {
            let before = bare_costs(&cursor.inner);
            let mut output = [0; 64];
            let p = cursor.poll(Tick(1), &mut output).unwrap();
            let after = bare_costs(&cursor.inner);
            turns.push((p.written, std::array::from_fn(|i| before[i] - after[i])));
            bytes.extend_from_slice(&output[..p.written]);
            if p.status == super::super::Status::Complete {
                return (bytes, turns);
            }
        }
        panic!("selected bare part did not complete")
    })
}
#[test]
fn whole_exact_and_spare_windows_match_bare_turns_and_original_custody() {
    for (source, ordinal) in [(SIMPLE, 1), (MULTIPART, 1), (MULTIPART, 2)] {
        for bits in [0, 1, 128, 256, 512, 513, 1023] {
            let (bytes, turns) = expected(source, ordinal, bits);
            for base in [0, 17, u64::MAX - source.len() as u64] {
                for extra in [0, 31] {
                    let clock = TestClock::new();
                    with_bound(source, base, &clock, |bound| {
                        let candidates = bound.value().unwrap().candidates.as_ptr();
                        let mut output = vec![0xa5; bytes.len() + extra];
                        let ptr = output.as_ptr();
                        let mut cursor =
                            Cursor::new(bound, ordinal, properties(bits), &mut output, Tick(1))
                                .unwrap();
                        for (index, (written, debits)) in turns.iter().enumerate() {
                            let before = cursor.inner.costs();
                            let status = cursor.poll(Tick(1)).unwrap();
                            let after = cursor.inner.costs();
                            assert_eq!(
                                std::array::from_fn::<_, 5, _>(|i| before[i] - after[i]),
                                *debits
                            );
                            assert_eq!(
                                status,
                                if index + 1 == turns.len() {
                                    Status::Complete
                                } else {
                                    Status::Yield
                                }
                            );
                            assert_eq!(before[4] - after[4], *written as u64);
                        }
                        let value = cursor.value().unwrap();
                        assert_eq!(value.members, bytes);
                        assert_eq!(value.members.as_ptr(), ptr);
                        assert_eq!(value.ordinal, ordinal);
                        assert_eq!(value.properties, properties(bits));
                        let retained = cursor.finish(Tick(1)).unwrap();
                        let value = retained.value().unwrap();
                        assert_eq!((value.ordinal, value.members.as_ptr()), (ordinal, ptr));
                        assert_eq!(value.properties, properties(bits));
                        assert_eq!(value.members, bytes);
                        let (bound, actual, props, members) = retained.finish(Tick(1)).unwrap();
                        assert_eq!((actual, props), (ordinal, properties(bits)));
                        assert_eq!(members, bytes);
                        assert_eq!(members.as_ptr(), ptr);
                        assert_eq!(bound.value().unwrap().candidates.as_ptr(), candidates);
                        let (_, mut parent) = bound.finish(Tick(1)).unwrap();
                        assert_eq!(parent.read_at(0, &mut [0; 2]).unwrap(), 2);
                        assert!(output[bytes.len()..].iter().all(|b| *b == 0xa5));
                    });
                }
            }
        }
    }
}
#[test]
fn thirteen_leaf_selections_retain_complete_bare_output() {
    for bits in [0, 1, 2, 4, 8, 16, 32, 64, 128, 256, 512, 513, 1023] {
        let (bytes, _) = expected(SIMPLE, 1, bits);
        let clock = TestClock::new();
        with_bound(SIMPLE, 0, &clock, |bound| {
            let mut output = vec![0xa5; bytes.len()];
            let mut cursor = Cursor::new(bound, 1, properties(bits), &mut output, Tick(1)).unwrap();
            drain(&mut cursor).unwrap();
            assert_eq!(cursor.value().unwrap().members, bytes);
        });
    }
}
#[test]
fn meaningful_short_windows_refuse_exactly_and_stickily() {
    for bits in [1, 512, 513, 1023] {
        let (bytes, _) = expected(SIMPLE, 1, bits);
        for capacity in [0, 1, 63, 64, 65, bytes.len() / 2, bytes.len() - 1] {
            if capacity >= bytes.len() {
                continue;
            }
            let clock = TestClock::new();
            with_bound(SIMPLE, 0, &clock, |bound| {
                let mut output = vec![0xa5; capacity];
                let mut cursor =
                    Cursor::new(bound, 1, properties(bits), &mut output, Tick(1)).unwrap();
                let error =
                    Error::Original(super::super::super::super::super::Error::ResponseCapacity);
                assert_eq!(drain(&mut cursor), Err(error));
                assert!(cursor.value().is_none());
                let after = cursor.inner.costs();
                assert_eq!(cursor.poll(Tick(1)), Err(error));
                assert_eq!(cursor.inner.costs(), after);
                assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
            });
        }
    }
}
#[test]
fn construction_precedes_capacity_and_invalid_ordinal_in_both_domains() {
    for bits in [0, 512, 1023] {
        for actual in [false, true] {
            let clock = TestClock::new();
            with_bound(SIMPLE, 0, &clock, |bound| {
                let tick = if actual {
                    clock.expire();
                    Tick(1)
                } else {
                    Tick(100)
                };
                assert_eq!(
                    Cursor::new(bound, 0, properties(bits), &mut [], tick).err(),
                    Some(deadline_error(actual))
                );
            });
        }
    }
}
#[test]
fn incomplete_prefixes_and_completed_owners_remain_explicitly_fresh() {
    let (_, turns) = expected(SIMPLE, 1, 512);
    for prefix in [0, 1, 2, turns.len() - 1, turns.len()] {
        for actual in [false, true] {
            let clock = TestClock::new();
            with_bound(SIMPLE, 0, &clock, |bound| {
                let mut output = [0xa5; 512];
                let mut cursor =
                    Cursor::new(bound, 1, properties(512), &mut output, Tick(1)).unwrap();
                for _ in 0..prefix {
                    cursor.poll(Tick(1)).unwrap();
                }
                if prefix == turns.len() {
                    let mut retained = cursor.finish(Tick(1)).unwrap();
                    let tick = if actual {
                        clock.expire();
                        Tick(1)
                    } else {
                        Tick(100)
                    };
                    let error = deadline_error(actual);
                    assert_eq!(retained.check_deadline(tick), Err(error));
                    assert!(retained.value().is_none());
                    assert_eq!(retained.finish(Tick(1)).err(), Some(error));
                } else {
                    let tick = if actual {
                        clock.expire();
                        Tick(1)
                    } else {
                        Tick(100)
                    };
                    let error = deadline_error(actual);
                    assert_eq!(cursor.check_deadline(tick), Err(error));
                    assert!(cursor.value().is_none());
                    assert_eq!(cursor.poll(Tick(1)), Err(error));
                    assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
                }
            });
        }
    }
    for bits in [0, 512, 1023] {
        for actual in [false, true] {
            let clock = TestClock::new();
            with_bound(SIMPLE, 0, &clock, |bound| {
                let mut output = [0xa5; 512];
                let mut cursor =
                    Cursor::new(bound, 1, properties(bits), &mut output, Tick(1)).unwrap();
                drain(&mut cursor).unwrap();
                let tick = if actual {
                    clock.expire();
                    Tick(1)
                } else {
                    Tick(100)
                };
                let error = deadline_error(actual);
                assert_eq!(cursor.check_deadline(tick), Err(error));
                assert!(cursor.value().is_none());
                assert_eq!(cursor.poll(Tick(1)), Err(error));
                assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
            });
        }
    }
    for bits in [0, 512, 1023] {
        for actual in [false, true] {
            for retained_phase in [false, true] {
                let clock = TestClock::new();
                with_bound(SIMPLE, 0, &clock, |bound| {
                    let mut output = [0xa5; 512];
                    let mut cursor =
                        Cursor::new(bound, 1, properties(bits), &mut output, Tick(1)).unwrap();
                    drain(&mut cursor).unwrap();
                    if retained_phase {
                        let retained = cursor.finish(Tick(1)).unwrap();
                        let tick = if actual {
                            clock.expire();
                            Tick(1)
                        } else {
                            Tick(100)
                        };
                        assert_eq!(retained.finish(tick).err(), Some(deadline_error(actual)));
                    } else {
                        let tick = if actual {
                            clock.expire();
                            Tick(1)
                        } else {
                            Tick(100)
                        };
                        assert_eq!(cursor.finish(tick).err(), Some(deadline_error(actual)));
                    }
                });
            }
        }
    }
}
#[test]
fn partial_deadline_precedes_capacity_and_cached_complete_is_inert() {
    for capacity in [0, 64] {
        for actual in [false, true] {
            let clock = TestClock::new();
            with_bound(SIMPLE, 0, &clock, |bound| {
                let mut output = [0xa5; 64];
                let mut cursor =
                    Cursor::new(bound, 1, properties(512), &mut output[..capacity], Tick(1))
                        .unwrap();
                if capacity != 0 {
                    loop {
                        let before = cursor.inner.costs();
                        assert_eq!(cursor.poll(Tick(1)), Ok(Status::Yield));
                        if cursor.inner.costs()[4] < before[4] {
                            break;
                        }
                    }
                    assert!(cursor.value().is_none());
                }
                let tick = if actual {
                    clock.expire();
                    Tick(1)
                } else {
                    Tick(100)
                };
                let error = deadline_error(actual);
                assert_eq!(cursor.poll(tick), Err(error));
                assert!(cursor.value().is_none());
                assert_eq!(cursor.poll(Tick(1)), Err(error));
                assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
            });
        }
    }
    for bits in [0, 512, 1023] {
        let clock = TestClock::new();
        with_bound(SIMPLE, 0, &clock, |bound| {
            let mut output = [0xa5; 512];
            let mut cursor = Cursor::new(bound, 1, properties(bits), &mut output, Tick(1)).unwrap();
            drain(&mut cursor).unwrap();
            let after = cursor.inner.costs();
            clock.expire();
            assert_eq!(cursor.poll(Tick(100)), Ok(Status::Complete));
            assert_eq!(cursor.inner.costs(), after);
            assert_eq!(
                cursor.finish(Tick(1)).err(),
                Some(Error::Parent(PolicyError::Deadline))
            );
        });
    }
}
#[test]
fn every_healthy_incomplete_prefix_cannot_create_whole_retention() {
    let (_, turns) = expected(SIMPLE, 1, 512);
    for prefix in 0..turns.len() {
        let clock = TestClock::new();
        with_bound(SIMPLE, 0, &clock, |bound| {
            let mut output = [0xa5; 512];
            let mut cursor = Cursor::new(bound, 1, properties(512), &mut output, Tick(1)).unwrap();
            for _ in 0..prefix {
                assert_eq!(cursor.poll(Tick(1)), Ok(Status::Yield));
            }
            assert!(cursor.value().is_none());
            assert_eq!(cursor.finish(Tick(1)).err(), Some(Error::InvalidState));
        });
    }
}
#[test]
fn interpretation_record_and_wire_refusal_hide_whole_values() {
    for kind in 0..3 {
        let clock = TestClock::new();
        with_bound(SIMPLE, 0, &clock, |mut bound| {
            let structure = &mut bound.original.source.original.source.projected.structure;
            let expected = if kind == 0 {
                let left = structure.budget.steps_remaining();
                structure
                    .budget
                    .charge(
                        structure.work,
                        Tick(1),
                        0,
                        left,
                        &mut crate::nfc::Credit::new(),
                    )
                    .unwrap();
                crate::nfc::Error::InterpretationLimit
            } else {
                let left = structure.work.remaining();
                structure
                    .work
                    .charge(
                        Tick(1),
                        Charge {
                            records: if kind == 1 { left.records } else { 0 },
                            output_bytes: if kind == 2 { left.output_bytes } else { 0 },
                            ..Charge::default()
                        },
                    )
                    .unwrap();
                crate::nfc::Error::Work(if kind == 1 {
                    Stop::Records
                } else {
                    Stop::OutputBytes
                })
            };
            let mut output = [0xa5; 512];
            let mut cursor = Cursor::new(bound, 1, properties(512), &mut output, Tick(1)).unwrap();
            let error = Error::Original(super::super::super::super::super::Error::Admission(
                expected,
            ));
            assert_eq!(drain(&mut cursor), Err(error));
            assert!(cursor.value().is_none());
            assert_eq!(cursor.poll(Tick(1)), Err(error));
            assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
            assert_eq!(output, [0xa5; 512]);
        });
    }
}

/// Source matching, generated fragments and output preparation stay cold.
pub fn probe(mut snapshot: impl FnMut()) {
    for trial in 0..8 {
        let clock = TestClock::new();
        let base = if trial == 0 {
            u64::MAX - SIMPLE.len() as u64
        } else {
            0
        };
        with_bound(SIMPLE, base, &clock, |bound| {
            let mut output = [0xa5; 512];
            let length = if trial == 1 {
                0
            } else if trial == 6 {
                1
            } else {
                512
            };
            if trial == 7 {
                clock.expire();
            }
            let props = properties(if trial == 1 {
                0
            } else if trial == 0 {
                513
            } else {
                512
            });
            snapshot();
            if trial == 7 {
                assert_eq!(
                    Cursor::new(bound, 1, props, &mut output[..length], Tick(1)).err(),
                    Some(Error::Parent(PolicyError::Deadline))
                );
            } else {
                let mut cursor =
                    Cursor::new(bound, 1, props, &mut output[..length], Tick(1)).unwrap();
                if trial == 6 {
                    let error =
                        Error::Original(super::super::super::super::super::Error::ResponseCapacity);
                    assert_eq!(drain(&mut cursor), Err(error));
                    assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
                    snapshot();
                    return;
                }
                if trial == 1 {
                    assert_eq!(cursor.poll(Tick(100)), Ok(Status::Complete));
                } else {
                    loop {
                        let before = cursor.inner.costs();
                        assert_eq!(cursor.poll(Tick(1)), Ok(Status::Yield));
                        if cursor.inner.costs()[4] < before[4] {
                            break;
                        }
                    }
                    if trial == 2 {
                        clock.expire();
                        let error = Error::Parent(PolicyError::Deadline);
                        assert_eq!(cursor.poll(Tick(1)), Err(error));
                        assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
                        snapshot();
                        return;
                    }
                    drain(&mut cursor).unwrap();
                }
                if trial == 3 {
                    clock.expire();
                    assert_eq!(
                        cursor.finish(Tick(1)).err(),
                        Some(Error::Parent(PolicyError::Deadline))
                    );
                } else {
                    let retained = cursor.finish(Tick(1)).unwrap();
                    assert_eq!(retained.value().unwrap().properties, props);
                    if trial == 4 {
                        clock.expire();
                        assert_eq!(
                            retained.finish(Tick(1)).err(),
                            Some(Error::Parent(PolicyError::Deadline))
                        );
                    } else {
                        let (mut bound, ordinal, actual, members) =
                            retained.finish(Tick(1)).unwrap();
                        assert_eq!((ordinal, actual), (1, props));
                        assert_eq!(members.is_empty(), trial == 1);
                        if trial == 5 {
                            clock.expire();
                            assert_eq!(
                                bound.check_deadline(Tick(1)),
                                Err(Error::Parent(PolicyError::Deadline))
                            );
                        }
                        drop(bound);
                    }
                }
            }
            snapshot();
        });
    }
}

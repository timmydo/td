#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::super::super::super::super::super::super::tests::TestClock;
use super::super::super::tests::{costs, deadline, with_collection, SIMPLE};
use super::*;
use crate::{mime_traversal::bound::ordered::tests::SOURCE, ports::Error as PolicyError};
fn source_costs(serialized: &Serialized<'_, '_, '_, '_, '_, '_, '_, '_, '_, '_, '_>) -> [u64; 5] {
    let structure = &serialized
        .source
        .original
        .source
        .original
        .source
        .projected
        .structure;
    let work = structure.work.remaining();
    [
        structure.budget.source_bytes_remaining(),
        structure.budget.steps_remaining(),
        work.io_bytes,
        work.records,
        work.output_bytes,
    ]
}
fn selection(bits: u8) -> Properties {
    Properties {
        body_structure: bits & 16 != 0,
        text_body: bits & 1 != 0,
        html_body: bits & 2 != 0,
        attachments: bits & 4 != 0,
        has_attachment: bits & 8 != 0,
    }
}
fn emitted(source: &[u8], base: u64, properties: Properties) -> Vec<u8> {
    let clock = TestClock::new();
    with_collection(source, base, &clock, |serialized| {
        let mut composer = super::super::Cursor::new(serialized, properties, Tick(1)).unwrap();
        let mut bytes = Vec::new();
        for _ in 0..10000 {
            let mut output = [0; 64];
            let progress = composer.poll(Tick(1), &mut output).unwrap();
            bytes.extend_from_slice(&output[..progress.written]);
            if progress.status == super::super::super::Status::Complete {
                return bytes;
            }
        }
        panic!("selected emission stalled")
    })
}
fn complete(cursor: &mut Cursor<'_, '_, '_, '_, '_, '_, '_, '_, '_, '_, '_, '_>) {
    for _ in 0..10000 {
        if cursor.poll(Tick(1)).unwrap() == Status::Complete {
            return;
        }
    }
    panic!("retained composition stalled")
}
#[test]
fn exact_and_spare_windows_preserve_original_members_and_descriptor_custody() {
    for source in [SIMPLE, SOURCE] {
        for base in [0, 19, u64::MAX - source.len() as u64] {
            for properties in (0..32)
                .filter(|bits| (source == SIMPLE && base == 0) || [0, 1, 16, 17, 31].contains(bits))
                .map(selection)
            {
                let expected = emitted(source, base, properties);
                for spare in [0, 31] {
                    let clock = TestClock::new();
                    with_collection(source, base, &clock, |serialized| {
                        let cells_ptr = serialized.cells.as_ptr();
                        let mut output = vec![0xa5; expected.len() + spare];
                        let output_ptr = output.as_ptr();
                        let mut cursor =
                            Cursor::new(serialized, properties, &mut output, Tick(1)).unwrap();
                        assert_eq!(cursor.value().is_some(), properties == Properties::NONE);
                        complete(&mut cursor);
                        let view = cursor.value().unwrap();
                        assert_eq!(view.properties, properties);
                        assert_eq!(view.members, expected);
                        assert_eq!(view.members.as_ptr(), output_ptr);
                        let retained = cursor.finish(Tick(1)).unwrap();
                        assert_eq!(retained.value().unwrap().properties, properties);
                        assert_eq!(retained.value().unwrap().members, expected);
                        let (serialized, actual, members) = retained.finish(Tick(1)).unwrap();
                        assert_eq!(actual, properties);
                        assert_eq!(members, expected);
                        assert_eq!(serialized.cells.as_ptr(), cells_ptr);
                        let (mut bound, cells) = serialized.finish(Tick(1)).unwrap();
                        assert_eq!(cells.as_ptr(), cells_ptr);
                        bound.check_deadline(Tick(1)).unwrap();
                        drop(bound);
                        assert!(output[expected.len()..].iter().all(|byte| *byte == 0xa5));
                    });
                }
            }
        }
    }
}
#[test]
fn requested_short_root_windows_refuse_stickily_without_whole_value() {
    for properties in [
        Properties::ALL,
        Properties {
            text_body: true,
            ..Properties::NONE
        },
    ] {
        let expected = emitted(SIMPLE, 0, properties);
        let length = expected.len();
        for capacity in [0, 1, 63, 64, 65, length / 2, length - 1] {
            if capacity >= length {
                continue;
            }
            let clock = TestClock::new();
            with_collection(SIMPLE, 0, &clock, |serialized| {
                let mut output = vec![0xa5; capacity];
                let mut cursor = Cursor::new(serialized, properties, &mut output, Tick(1)).unwrap();
                let before_wire = cursor.inner.costs()[4];
                let mut refused = false;
                for _ in 0..10000 {
                    match cursor.poll(Tick(1)) {
                        Ok(Status::Yield) => assert!(cursor.value().is_none()),
                        Ok(_) => panic!("short window completed"),
                        Err(error) => {
                            assert_eq!(
                                error,
                                Error::Original(
                                    super::super::super::OriginalError::ResponseCapacity
                                )
                            );
                            assert!(cursor.value().is_none());
                            assert_eq!(cursor.poll(Tick(1)), Err(error));
                            assert_eq!(before_wire - cursor.inner.costs()[4], capacity as u64);
                            assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
                            assert_eq!(output, expected[..capacity]);
                            refused = true;
                            break;
                        }
                    }
                }
                assert!(refused);
            });
        }
    }
}
#[test]
fn whole_bytes_and_all_five_costs_match_bare_original_composition() {
    for source in [SIMPLE, SOURCE] {
        for base in [0, 19, u64::MAX - source.len() as u64] {
            for properties in [0, 1, 16, 17, 31].map(selection) {
                let clock = TestClock::new();
                let (expected, bare_costs) = with_collection(source, base, &clock, |serialized| {
                    let mut cursor =
                        super::super::Cursor::new(serialized, properties, Tick(1)).unwrap();
                    let mut bytes = Vec::new();
                    let mut turns = Vec::new();
                    for _ in 0..10000 {
                        let before = costs(&cursor.original);
                        let mut output = [0; 64];
                        let progress = cursor.poll(Tick(1), &mut output).unwrap();
                        let after = costs(&cursor.original);
                        turns.push(std::array::from_fn::<_, 5, _>(|index| {
                            before[index] - after[index]
                        }));
                        bytes.extend_from_slice(&output[..progress.written]);
                        if progress.status == super::super::super::Status::Complete {
                            return (bytes, turns);
                        }
                    }
                    panic!("bare composition stalled")
                });
                with_collection(source, base, &clock, |serialized| {
                    let before_constructor = source_costs(&serialized);
                    let mut output = [0; 4096];
                    let mut cursor =
                        Cursor::new(serialized, properties, &mut output, Tick(1)).unwrap();
                    assert_eq!(cursor.inner.costs(), before_constructor);
                    let mut turns = Vec::new();
                    let mut copied = 0;
                    for _ in 0..10000 {
                        let before = cursor.inner.costs();
                        let status = cursor.poll(Tick(1)).unwrap();
                        let after = cursor.inner.costs();
                        turns.push(std::array::from_fn::<_, 5, _>(|index| {
                            before[index] - after[index]
                        }));
                        assert_eq!(before[0], after[0]);
                        assert_eq!(before[2], after[2]);
                        assert_eq!(
                            before[1] - after[1],
                            if properties == Properties::NONE { 0 } else { 1 }
                        );
                        let written = before[4] - after[4];
                        assert!(written <= 64);
                        copied += written;
                        if status == Status::Complete {
                            assert_eq!(cursor.value().unwrap().members.len() as u64, copied);
                            assert_eq!(cursor.value().unwrap().members, expected);
                            assert_eq!(turns, bare_costs);
                            return;
                        }
                    }
                    panic!("funded retention stalled")
                });
            }
        }
    }
}
#[test]
fn constructor_checks_both_deadline_domains_even_for_empty_window() {
    for properties in [Properties::NONE, Properties::ALL] {
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
                    Cursor::new(serialized, properties, &mut [], now).err(),
                    Some(deadline(actual))
                );
            });
        }
    }
}

#[test]
fn fresh_deadline_precedes_capacity_after_requested_partial_prefixes() {
    for actual in [false, true] {
        for prefix in [0, 1, 2] {
            let clock = TestClock::new();
            with_collection(SIMPLE, 0, &clock, |serialized| {
                let mut output = [0; 1];
                let mut cursor =
                    Cursor::new(serialized, Properties::ALL, &mut output, Tick(1)).unwrap();
                for _ in 0..prefix {
                    assert_eq!(cursor.poll(Tick(1)).unwrap(), Status::Yield);
                }
                let now = if actual {
                    clock.expire();
                    Tick(1)
                } else {
                    Tick(100)
                };
                let error = deadline(actual);
                assert_eq!(cursor.poll(now), Err(error));
                assert!(cursor.value().is_none());
                assert_eq!(cursor.poll(Tick(1)), Err(error));
                assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
            });
        }
    }
}
#[test]
fn premature_finish_refuses_and_cached_complete_has_no_fresh_work() {
    for properties in [
        Properties::NONE,
        Properties::ALL,
        Properties {
            text_body: true,
            ..Properties::NONE
        },
    ] {
        let clock = TestClock::new();
        if properties != Properties::NONE {
            with_collection(SIMPLE, 0, &clock, |serialized| {
                let mut output = [0; 1024];
                let mut cursor = Cursor::new(serialized, properties, &mut output, Tick(1)).unwrap();
                assert_eq!(cursor.poll(Tick(1)).unwrap(), Status::Yield);
                assert_eq!(cursor.finish(Tick(1)).err(), Some(Error::InvalidState));
            });
        }
        with_collection(SIMPLE, 0, &clock, |serialized| {
            let mut output = [0; 1024];
            let mut cursor = Cursor::new(serialized, properties, &mut output, Tick(1)).unwrap();
            complete(&mut cursor);
            let before = cursor.inner.costs();
            clock.expire();
            assert_eq!(cursor.poll(Tick(100)), Ok(Status::Complete));
            assert_eq!(cursor.inner.costs(), before);
            assert!(cursor.value().is_some());
            assert_eq!(
                cursor.finish(Tick(1)).err(),
                Some(Error::Parent(PolicyError::Deadline))
            );
        });
    }
}
#[test]
fn every_complete_owner_explicit_check_and_final_release_is_fresh() {
    for properties in [Properties::NONE, Properties::ALL] {
        for actual in [false, true] {
            for retained_owner in [false, true] {
                for explicit in [false, true] {
                    let clock = TestClock::new();
                    with_collection(SIMPLE, 0, &clock, |serialized| {
                        let mut output = [0; 1024];
                        let mut cursor =
                            Cursor::new(serialized, properties, &mut output, Tick(1)).unwrap();
                        complete(&mut cursor);
                        if retained_owner {
                            let mut retained = cursor.finish(Tick(1)).unwrap();
                            let now = if actual {
                                clock.expire();
                                Tick(1)
                            } else {
                                Tick(100)
                            };
                            let error = deadline(actual);
                            if explicit {
                                assert_eq!(retained.check_deadline(now), Err(error));
                                assert!(retained.value().is_none());
                            }
                            assert_eq!(retained.finish(now).err(), Some(error));
                        } else {
                            let now = if actual {
                                clock.expire();
                                Tick(1)
                            } else {
                                Tick(100)
                            };
                            let error = deadline(actual);
                            if explicit {
                                assert_eq!(cursor.check_deadline(now), Err(error));
                                assert!(cursor.value().is_none());
                            }
                            assert_eq!(cursor.finish(now).err(), Some(error));
                        }
                    });
                }
            }
        }
    }
}

#[test]
fn original_wire_refusal_hides_window_and_remembers_exact_error() {
    let clock = TestClock::new();
    with_collection(SIMPLE, 0, &clock, |mut serialized| {
        let structure = &mut serialized
            .source
            .original
            .source
            .original
            .source
            .projected
            .structure;
        let left = structure.work.remaining().output_bytes;
        structure
            .work
            .charge(
                Tick(1),
                crate::admission::work::Charge {
                    output_bytes: left,
                    ..Default::default()
                },
            )
            .unwrap();
        let mut output = [0xa5; 1024];
        let mut cursor = Cursor::new(serialized, Properties::ALL, &mut output, Tick(1)).unwrap();
        assert_eq!(cursor.poll(Tick(1)).unwrap(), Status::Yield);
        let error = Error::Original(super::super::super::OriginalError::Admission(
            crate::nfc::Error::Work(crate::admission::work::Stop::OutputBytes),
        ));
        assert_eq!(cursor.poll(Tick(1)), Err(error));
        assert!(cursor.value().is_none());
        assert_eq!(cursor.poll(Tick(1)), Err(error));
        assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
        assert_eq!(output, [0xa5; 1024]);
    });
}
pub fn probe(mut snapshot: impl FnMut()) {
    for trial in 0..8 {
        let clock = TestClock::new();
        let properties = if trial == 1 {
            Properties::NONE
        } else {
            Properties {
                body_structure: true,
                text_body: true,
                ..Properties::NONE
            }
        };
        let base = if trial == 0 {
            u64::MAX - SOURCE.len() as u64
        } else {
            0
        };
        with_collection(SOURCE, base, &clock, |serialized| {
            let mut output = [0; 4096];
            let output = if trial == 6 || trial == 1 {
                &mut output[..0]
            } else {
                &mut output[..]
            };
            if trial == 7 {
                clock.expire();
            }
            snapshot();
            match Cursor::new(serialized, properties, output, Tick(1)) {
                Err(error) => {
                    assert_eq!(trial, 7);
                    assert_eq!(error, Error::Parent(PolicyError::Deadline));
                }
                Ok(mut cursor) => {
                    if trial == 6 {
                        assert_eq!(
                            cursor.poll(Tick(1)),
                            Err(Error::Original(
                                super::super::super::OriginalError::ResponseCapacity
                            ))
                        );
                        drop(cursor);
                    } else if trial == 2 {
                        let mut copied = false;
                        for _ in 0..10000 {
                            let before = cursor.inner.costs()[4];
                            cursor.poll(Tick(1)).unwrap();
                            if cursor.inner.costs()[4] < before {
                                copied = true;
                                break;
                            }
                        }
                        assert!(copied);
                        clock.expire();
                        assert_eq!(
                            cursor.poll(Tick(1)),
                            Err(Error::Parent(PolicyError::Deadline))
                        );
                        drop(cursor);
                    } else {
                        complete(&mut cursor);
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
                                let (mut serialized, actual, bytes) =
                                    retained.finish(Tick(1)).unwrap();
                                assert_eq!(actual, properties);
                                assert_eq!(bytes.is_empty(), trial == 1);
                                if trial == 5 {
                                    clock.expire();
                                    assert_eq!(
                                        serialized.check_deadline(Tick(1)),
                                        Err(Error::Parent(PolicyError::Deadline))
                                    );
                                }
                                drop(serialized);
                            }
                        }
                    }
                }
            }
            snapshot();
        });
    }
}

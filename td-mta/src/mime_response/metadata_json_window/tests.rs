#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use crate::mime_response::metadata_json_window::*;
use crate::mime_response::pinned::tests::TestClock;
use {
    crate::mime_response::metadata_json::tests::costs,
    crate::mime_response::metadata_json::tests::deadline,
    crate::mime_response::metadata_json::tests::drain,
    crate::mime_response::metadata_json::tests::with_collection,
    crate::mime_response::metadata_json::tests::SIMPLE,
};
use {crate::mime_response::ordered::tests::SOURCE, crate::ports::Error as PolicyError};
fn emitted(source: &[u8], base: u64, mode: Mode) -> Vec<u8> {
    let clock = TestClock::new();
    with_collection(source, base, &clock, |serialized| {
        let mut composer =
            crate::mime_response::metadata_json::Cursor::new(serialized, mode, Tick(1)).unwrap();
        drain(&mut composer, 64).unwrap()
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
            for mode in [Mode::Structure, Mode::Lists] {
                let expected = emitted(source, base, mode);
                for spare in [0, 31] {
                    let clock = TestClock::new();
                    with_collection(source, base, &clock, |serialized| {
                        let cells_ptr = serialized.cells.as_ptr();
                        let mut output = vec![0xa5; expected.len() + spare];
                        let output_ptr = output.as_ptr();
                        let mut cursor =
                            Cursor::new(serialized, mode, &mut output, Tick(1)).unwrap();
                        assert!(cursor.value().is_none());
                        complete(&mut cursor);
                        let view = cursor.value().unwrap();
                        assert_eq!(view.mode, mode);
                        assert_eq!(view.members, expected);
                        assert_eq!(view.members.as_ptr(), output_ptr);
                        let retained = cursor.finish(Tick(1)).unwrap();
                        assert_eq!(retained.value().unwrap().members, expected);
                        let (serialized, actual, members) = retained.finish(Tick(1)).unwrap();
                        assert_eq!(actual, mode);
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
fn selected_short_root_windows_refuse_stickily_without_whole_value() {
    for mode in [Mode::Structure, Mode::Lists] {
        let length = emitted(SIMPLE, 0, mode).len();
        for capacity in [0, 1, 63, 64, 65, length / 2, length - 1] {
            if capacity >= length {
                continue;
            }
            let clock = TestClock::new();
            with_collection(SIMPLE, 0, &clock, |serialized| {
                let mut output = vec![0xa5; capacity];
                let mut cursor = Cursor::new(serialized, mode, &mut output, Tick(1)).unwrap();
                let mut refused = false;
                for _ in 0..10000 {
                    match cursor.poll(Tick(1)) {
                        Ok(Status::Yield) => assert!(cursor.value().is_none()),
                        Ok(_) => panic!("short window completed"),
                        Err(error) => {
                            assert_eq!(
                                error,
                                Error::Original(
                                    crate::mime_response::bound::Error::ResponseCapacity
                                )
                            );
                            assert!(cursor.value().is_none());
                            assert_eq!(cursor.poll(Tick(1)), Err(error));
                            assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
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
            for mode in [Mode::Structure, Mode::Lists] {
                let clock = TestClock::new();
                let (expected, bare_costs) = with_collection(source, base, &clock, |serialized| {
                    let mut cursor =
                        crate::mime_response::metadata_json::Cursor::new(serialized, mode, Tick(1))
                            .unwrap();
                    let mut bytes = Vec::new();
                    let mut turns = Vec::new();
                    for _ in 0..10000 {
                        let before = costs(&cursor);
                        let mut output = [0; 64];
                        let progress = cursor.poll(Tick(1), &mut output).unwrap();
                        let after = costs(&cursor);
                        turns.push(std::array::from_fn::<_, 5, _>(|index| {
                            before[index] - after[index]
                        }));
                        bytes.extend_from_slice(&output[..progress.written]);
                        if progress.status == crate::mime_response::metadata_json::Status::Complete
                        {
                            return (bytes, turns);
                        }
                    }
                    panic!("bare composition stalled")
                });
                with_collection(source, base, &clock, |serialized| {
                    let mut output = [0; 4096];
                    let mut cursor = Cursor::new(serialized, mode, &mut output, Tick(1)).unwrap();
                    let mut turns = Vec::new();
                    let mut copied = 0;
                    for _ in 0..10000 {
                        let before = costs(&cursor.composer);
                        let status = cursor.poll(Tick(1)).unwrap();
                        let after = costs(&cursor.composer);
                        turns.push(std::array::from_fn::<_, 5, _>(|index| {
                            before[index] - after[index]
                        }));
                        assert_eq!(before[0], after[0]);
                        assert_eq!(before[2], after[2]);
                        assert_eq!(before[1] - after[1], 1);
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
                Cursor::new(serialized, Mode::Lists, &mut [], now).err(),
                Some(deadline(actual))
            );
        });
    }
}
#[test]
fn fresh_deadline_precedes_capacity_after_selected_partial_prefixes() {
    for actual in [false, true] {
        for prefix in [0, 1, 2] {
            let clock = TestClock::new();
            with_collection(SIMPLE, 0, &clock, |serialized| {
                let mut output = [0; 1];
                let mut cursor =
                    Cursor::new(serialized, Mode::Structure, &mut output, Tick(1)).unwrap();
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
    for mode in [Mode::Structure, Mode::Lists] {
        let clock = TestClock::new();
        with_collection(SIMPLE, 0, &clock, |serialized| {
            let mut output = [0; 1024];
            let mut cursor = Cursor::new(serialized, mode, &mut output, Tick(1)).unwrap();
            assert_eq!(cursor.poll(Tick(1)).unwrap(), Status::Yield);
            assert_eq!(cursor.finish(Tick(1)).err(), Some(Error::InvalidState));
        });
        with_collection(SIMPLE, 0, &clock, |serialized| {
            let mut output = [0; 1024];
            let mut cursor = Cursor::new(serialized, mode, &mut output, Tick(1)).unwrap();
            complete(&mut cursor);
            let before = costs(&cursor.composer);
            clock.expire();
            assert_eq!(cursor.poll(Tick(100)), Ok(Status::Complete));
            assert_eq!(costs(&cursor.composer), before);
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
    for actual in [false, true] {
        for retained_owner in [false, true] {
            for explicit in [false, true] {
                let clock = TestClock::new();
                with_collection(SIMPLE, 0, &clock, |serialized| {
                    let mut output = [0; 1024];
                    let mut cursor =
                        Cursor::new(serialized, Mode::Structure, &mut output, Tick(1)).unwrap();
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
        let mut cursor = Cursor::new(serialized, Mode::Structure, &mut output, Tick(1)).unwrap();
        assert_eq!(cursor.poll(Tick(1)).unwrap(), Status::Yield);
        let error = Error::Original(crate::mime_response::bound::Error::Admission(
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
            let mut output = [0; 4096];
            let output = if trial == 6 {
                &mut output[..0]
            } else {
                &mut output[..]
            };
            if trial == 7 {
                clock.expire();
            }
            snapshot();
            match Cursor::new(serialized, mode, output, Tick(1)) {
                Err(error) => {
                    assert_eq!(trial, 7);
                    assert_eq!(error, Error::Parent(PolicyError::Deadline));
                }
                Ok(mut cursor) => {
                    if trial == 6 {
                        assert_eq!(
                            cursor.poll(Tick(1)),
                            Err(Error::Original(
                                crate::mime_response::bound::Error::ResponseCapacity
                            ))
                        );
                        drop(cursor);
                    } else if trial == 2 {
                        let mut copied = false;
                        for _ in 0..10000 {
                            let before = costs(&cursor.composer)[4];
                            cursor.poll(Tick(1)).unwrap();
                            if costs(&cursor.composer)[4] < before {
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
                                assert_eq!(actual, mode);
                                assert!(!bytes.is_empty());
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

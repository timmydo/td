#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::super::super::super::super::super::tests::TestClock;
use super::super::tests::{costs, deadline, with_collection, SIMPLE};
use super::super::Mode;
use super::*;
use crate::{
    admission::work::Charge,
    mime_traversal::bound::ordered::tests::SOURCE,
    ports::{BlobReader, Clock, Error as PolicyError, Time},
};
use std::sync::atomic::{AtomicU64, Ordering};
fn properties(bits: u8) -> Properties {
    Properties {
        body_structure: bits & 16 != 0,
        text_body: bits & 1 != 0,
        html_body: bits & 2 != 0,
        attachments: bits & 4 != 0,
        has_attachment: bits & 8 != 0,
    }
}
fn expected(
    source: &[u8],
    properties: Properties,
    cells: &[super::super::super::Cell<'_>],
) -> String {
    let part = |index: usize| {
        format!(
            "{{{},\"subParts\":null}}",
            std::str::from_utf8(cells[index].value().unwrap().members).unwrap()
        )
    };
    let mut fields = Vec::new();
    if properties.body_structure {
        fields.push(if source == SOURCE {
            format!(
                "\"bodyStructure\":{{{},\"subParts\":[{},{}]}}",
                std::str::from_utf8(cells[0].value().unwrap().members).unwrap(),
                part(1),
                part(2)
            )
        } else {
            format!("\"bodyStructure\":{}", part(0))
        });
    }
    let index = if source == SOURCE { 2 } else { 0 };
    if properties.text_body {
        fields.push(format!("\"textBody\":[{}]", part(index)));
    }
    if properties.html_body {
        fields.push(format!("\"htmlBody\":[{}]", part(index)));
    }
    if properties.attachments {
        fields.push(if source == SOURCE {
            format!("\"attachments\":[{}]", part(1))
        } else {
            "\"attachments\":[]".to_owned()
        });
    }
    if properties.has_attachment {
        fields.push(format!("\"hasAttachment\":{}", source == SOURCE));
    }
    fields.join(",")
}
fn drain(
    cursor: &mut Cursor<'_, '_, '_, '_, '_, '_, '_, '_, '_, '_, '_>,
    width: usize,
) -> (Vec<u8>, Vec<(Progress, [u64; 5])>) {
    let mut bytes = Vec::new();
    let mut turns = Vec::new();
    for _ in 0..10000 {
        let before = costs(&cursor.original);
        let was_complete = cursor.value().is_some();
        let mut output = [0xa5; 128];
        let progress = cursor.poll(Tick(1), &mut output[..width]).unwrap();
        let after = costs(&cursor.original);
        assert_eq!(before[0], after[0]);
        assert_eq!(before[2], after[2]);
        assert_eq!(before[1] - after[1], if was_complete { 0 } else { 1 });
        assert_eq!(
            before[3] - after[3],
            if was_complete {
                0
            } else {
                u64::from(turns.len() % 16 == 0)
            }
        );
        assert_eq!(before[4] - after[4], progress.written as u64);
        assert!(progress.written <= 64);
        assert!(output[progress.written..].iter().all(|byte| *byte == 0xa5));
        bytes.extend_from_slice(&output[..progress.written]);
        turns.push((
            progress,
            std::array::from_fn(|index| before[index] - after[index]),
        ));
        if progress.status == super::super::Status::Complete {
            assert!(was_complete || progress.written != 0);
            return (bytes, turns);
        }
    }
    panic!("requested emission stalled")
}

#[test]
fn all_thirty_two_subsets_emit_literal_requested_keys_and_preserve_original_custody() {
    for source in [SIMPLE, SOURCE] {
        for base in [0, 17, u64::MAX - source.len() as u64] {
            for bits in (0..32).filter(|bits| {
                (source == SIMPLE && base == 0) || [0, 1, 8, 15, 16, 17, 31].contains(bits)
            }) {
                let clock = TestClock::new();
                let props = properties(bits);
                with_collection(source, base, &clock, |serialized| {
                    let pointer = serialized.cells.as_ptr();
                    let expected = expected(source, props, serialized.cells);
                    let mut cursor = Cursor::new(serialized, props, Tick(1)).unwrap();
                    assert_eq!(cursor.value().is_some(), bits == 0);
                    let (bytes, _) = drain(&mut cursor, 64);
                    assert_eq!(bytes, expected.as_bytes());
                    assert_eq!(cursor.value().unwrap().0, props);
                    let composed = cursor.finish(Tick(1)).unwrap();
                    assert_eq!(composed.value().unwrap().0, props);
                    let (serialized, selected) = composed.finish(Tick(1)).unwrap();
                    assert_eq!(selected, props);
                    assert_eq!(serialized.cells.as_ptr(), pointer);
                    let (bound, cells) = serialized.finish(Tick(1)).unwrap();
                    assert_eq!(cells.as_ptr(), pointer);
                    let (_, mut parent) = bound.finish(Tick(1)).unwrap();
                    assert_eq!(parent.read_at(0, &mut [0; 2]).unwrap(), 2);
                });
            }
        }
    }
}
#[test]
fn single_mode_requests_preserve_legacy_bytes_turns_and_funding() {
    for (bits, mode) in [(15, Mode::Lists), (16, Mode::Structure)] {
        for width in [1, 7, 64, 128] {
            let clock = TestClock::new();
            let selected = with_collection(SOURCE, 0, &clock, |serialized| {
                let mut cursor = Cursor::new(serialized, properties(bits), Tick(1)).unwrap();
                drain(&mut cursor, width)
            });
            let original = with_collection(SOURCE, 0, &clock, |serialized| {
                let mut cursor = super::super::Cursor::new(serialized, mode, Tick(1)).unwrap();
                let mut bytes = Vec::new();
                let mut turns = Vec::new();
                for _ in 0..10000 {
                    let before = costs(&cursor);
                    let mut output = [0; 128];
                    let progress = cursor.poll(Tick(1), &mut output[..width]).unwrap();
                    let after = costs(&cursor);
                    bytes.extend_from_slice(&output[..progress.written]);
                    turns.push((
                        progress,
                        std::array::from_fn(|index| before[index] - after[index]),
                    ));
                    if progress.status == super::super::Status::Complete {
                        return (bytes, turns);
                    }
                }
                panic!("original composition stalled")
            });
            assert_eq!(selected, original);
        }
    }
}

#[test]
fn constructors_freshly_admit_both_domains_including_empty_selection() {
    for bits in [0, 15, 16, 31] {
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
                    Cursor::new(serialized, properties(bits), now).err(),
                    Some(deadline(actual))
                );
            });
        }
    }
}
#[test]
fn requested_prefixes_and_complete_finish_keep_both_domains_fresh() {
    for bits in [2, 8, 16, 17, 31] {
        for actual in [false, true] {
            let healthy = TestClock::new();
            let total = with_collection(SIMPLE, 0, &healthy, |serialized| {
                let mut cursor = Cursor::new(serialized, properties(bits), Tick(1)).unwrap();
                drain(&mut cursor, 64).1.len()
            });
            for prefix in [0, 1, total / 2, total - 1, total] {
                let clock = TestClock::new();
                with_collection(SIMPLE, 0, &clock, |serialized| {
                    let mut cursor = Cursor::new(serialized, properties(bits), Tick(1)).unwrap();
                    for _ in 0..prefix {
                        cursor.poll(Tick(1), &mut [0; 64]).unwrap();
                    }
                    let now = if actual {
                        clock.expire();
                        Tick(1)
                    } else {
                        Tick(100)
                    };
                    assert_eq!(cursor.finish(now).err(), Some(deadline(actual)));
                });
            }
        }
    }
}
#[test]
fn requested_wire_record_and_step_refusals_are_exact_sticky_and_source_io_free() {
    for bits in [8, 31] {
        for kind in 0..3 {
            let clock = TestClock::new();
            with_collection(SIMPLE, 0, &clock, |serialized| {
                let mut cursor = Cursor::new(serialized, properties(bits), Tick(1)).unwrap();
                let s = &mut cursor
                    .original
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
                    s.budget
                        .charge(s.work, Tick(1), 0, steps, &mut crate::nfc::Credit::new())
                        .unwrap();
                } else {
                    s.work
                        .charge(
                            Tick(1),
                            Charge {
                                records: if kind == 1 { left.records } else { 0 },
                                output_bytes: if kind == 0 { left.output_bytes } else { 0 },
                                ..Default::default()
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
                let error = Error::Original(super::super::OriginalError::Admission(nfc));
                let mut output = [0xa5; 64];
                for _ in 0..100 {
                    let before = costs(&cursor.original);
                    let result = cursor.poll(Tick(1), &mut output);
                    let after = costs(&cursor.original);
                    assert_eq!(before[0], after[0]);
                    assert_eq!(before[2], after[2]);
                    match result {
                        Ok(p) => assert_eq!(p.written, 0),
                        Err(e) => {
                            assert_eq!(e, error);
                            assert_eq!(output, [0xa5; 64]);
                            assert!(cursor.value().is_none());
                            assert_eq!(cursor.poll(Tick(1), &mut output), Err(e));
                            assert_eq!(cursor.finish(Tick(1)).err(), Some(e));
                            return;
                        }
                    }
                }
                panic!("requested quota did not refuse")
            });
        }
    }
}

#[test]
fn empty_selection_cached_complete_is_inert_but_complete_owner_boundaries_are_fresh() {
    for bits in [0, 2, 16, 31] {
        for actual in [false, true] {
            for composed_owner in [false, true] {
                for explicit in [false, true] {
                    let clock = TestClock::new();
                    with_collection(SIMPLE, 0, &clock, |serialized| {
                        let mut cursor =
                            Cursor::new(serialized, properties(bits), Tick(1)).unwrap();
                        let _ = drain(&mut cursor, 64);
                        if composed_owner {
                            let mut owner = cursor.finish(Tick(1)).unwrap();
                            let now = if actual {
                                clock.expire();
                                Tick(1)
                            } else {
                                Tick(100)
                            };
                            let e = deadline(actual);
                            if explicit {
                                assert_eq!(owner.check_deadline(now), Err(e));
                                assert!(owner.value().is_none());
                            }
                            assert_eq!(owner.finish(now).err(), Some(e));
                        } else {
                            let before = costs(&cursor.original);
                            let now = if actual {
                                clock.expire();
                                Tick(1)
                            } else {
                                Tick(100)
                            };
                            let e = deadline(actual);
                            assert_eq!(
                                cursor.poll(now, &mut []).unwrap(),
                                Progress {
                                    written: 0,
                                    status: super::super::Status::Complete
                                }
                            );
                            assert_eq!(costs(&cursor.original), before);
                            if explicit {
                                assert_eq!(cursor.check_deadline(now), Err(e));
                                assert!(cursor.value().is_none());
                            }
                            assert_eq!(cursor.finish(now).err(), Some(e));
                        }
                    });
                }
            }
        }
    }
}
#[test]
fn unfinished_empty_output_admits_freshly_without_work_or_progress() {
    for actual in [false, true] {
        let clock = TestClock::new();
        with_collection(SIMPLE, 0, &clock, |serialized| {
            let mut cursor = Cursor::new(serialized, properties(2), Tick(1)).unwrap();
            let before = costs(&cursor.original);
            assert_eq!(
                cursor.poll(Tick(1), &mut []).unwrap(),
                Progress {
                    written: 0,
                    status: super::super::Status::NeedOutput
                }
            );
            assert_eq!(costs(&cursor.original), before);
            let now = if actual {
                clock.expire();
                Tick(1)
            } else {
                Tick(100)
            };
            let error = deadline(actual);
            assert_eq!(cursor.poll(now, &mut []), Err(error));
            assert_eq!(costs(&cursor.original), before);
            assert!(cursor.value().is_none());
            assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
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
fn requested_boolean_and_tree_post_copy_expiry_hides_complete_and_remembers_error() {
    for bits in [8, 31] {
        let clock = FenceClock {
            calls: AtomicU64::new(0),
            expire_at: AtomicU64::new(u64::MAX),
        };
        with_collection(SIMPLE, 0, &clock, |serialized| {
            let mut cursor = Cursor::new(serialized, properties(bits), Tick(1)).unwrap();
            assert_eq!(cursor.poll(Tick(1), &mut [0; 64]).unwrap().written, 0);
            let before = clock.calls.load(Ordering::SeqCst);
            clock.expire_at.store(before + 3, Ordering::SeqCst);
            let mut output = [0xa5; 64];
            let error = Error::Parent(PolicyError::Deadline);
            assert_eq!(cursor.poll(Tick(1), &mut output), Err(error));
            assert!(output.starts_with(if bits == 8 {
                b"\"hasAttachment\":false".as_slice()
            } else {
                b"\"bodyStructure\":".as_slice()
            }));
            assert!(cursor.value().is_none());
            assert_eq!(clock.calls.load(Ordering::SeqCst), before + 3);
            clock.expire_at.store(u64::MAX, Ordering::SeqCst);
            assert_eq!(cursor.poll(Tick(1), &mut [0; 64]), Err(error));
            assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
            assert_eq!(clock.calls.load(Ordering::SeqCst), before + 3);
        });
    }
}

pub fn probe(mut snapshot: impl FnMut()) {
    for trial in 0..8 {
        let clock = TestClock::new();
        let props = properties(if trial == 0 {
            17
        } else if trial == 1 {
            0
        } else {
            31
        });
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
            match Cursor::new(serialized, props, Tick(1)) {
                Err(e) => {
                    assert_eq!(trial, 7);
                    assert_eq!(e, Error::Parent(PolicyError::Deadline));
                }
                Ok(mut cursor) => {
                    let mut output = [0; 64];
                    if trial == 6 {
                        let s = &mut cursor
                            .original
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
                                    ..Default::default()
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
                                Ok(p) if p.status == super::super::Status::Complete => {
                                    complete = true;
                                    break;
                                }
                                Ok(_) => {}
                                Err(e) => {
                                    assert_eq!(trial, 6);
                                    assert_eq!(
                                        e,
                                        Error::Original(super::super::OriginalError::Admission(
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
                                    assert_eq!(actual, props);
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
            }
            snapshot();
        });
    }
}

#[test]
fn combined_tree_to_list_transition_is_single_funded_turn_and_refuses_without_bytes() {
    for source in [SIMPLE, SOURCE] {
        for width in [1, 7, 64, 128] {
            let clock = TestClock::new();
            let tree = with_collection(source, 0, &clock, |serialized| {
                let mut cursor = Cursor::new(serialized, properties(16), Tick(1)).unwrap();
                drain(&mut cursor, width)
            });
            let lists = with_collection(source, 0, &clock, |serialized| {
                let mut cursor = Cursor::new(serialized, properties(15), Tick(1)).unwrap();
                drain(&mut cursor, width)
            });
            let combined = with_collection(source, 0, &clock, |serialized| {
                let mut cursor = Cursor::new(serialized, properties(31), Tick(1)).unwrap();
                drain(&mut cursor, width)
            });
            assert_eq!(
                combined.0,
                [tree.0.as_slice(), b",", lists.0.as_slice()].concat()
            );
            let cap = width.min(64);
            let separator_turn = 13usize.div_ceil(cap) - 12usize.div_ceil(cap);
            assert_eq!(
                combined.1.len(),
                tree.1.len() + lists.1.len() + separator_turn
            );
            let transition = combined.1[tree.1.len()];
            assert_eq!(transition.0.written, 0);
            assert_eq!(transition.0.status, super::super::Status::Yield);
            assert_eq!(transition.1[1], 1);
            assert_eq!(transition.1[4], 0);
            assert!(combined.1[tree.1.len() + 1].0.written > 0);
            with_collection(source, 0, &clock, |serialized| {
                let mut cursor = Cursor::new(serialized, properties(31), Tick(1)).unwrap();
                let structure = &mut cursor
                    .original
                    .source
                    .source
                    .original
                    .source
                    .original
                    .source
                    .projected
                    .structure;
                let steps = structure.budget.steps_remaining();
                structure
                    .budget
                    .charge(
                        structure.work,
                        Tick(1),
                        0,
                        steps - tree.1.len() as u64,
                        &mut crate::nfc::Credit::new(),
                    )
                    .unwrap();
                for _ in 0..tree.1.len() {
                    cursor.poll(Tick(1), &mut [0; 128][..width]).unwrap();
                }
                let before = costs(&cursor.original);
                let mut output = [0xa5; 128];
                let error = Error::Original(super::super::OriginalError::Admission(
                    crate::nfc::Error::InterpretationLimit,
                ));
                assert_eq!(cursor.poll(Tick(1), &mut output[..width]), Err(error));
                assert_eq!(output, [0xa5; 128]);
                assert_eq!(costs(&cursor.original), before);
                assert!(cursor.value().is_none());
                assert_eq!(cursor.poll(Tick(1), &mut output[..width]), Err(error));
                assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
            });
        }
    }
}

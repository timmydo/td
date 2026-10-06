#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::super::super::super::super::super::super::tests::TestClock;
use super::super::super::super::super::tests::properties as part_properties;
use super::super::tests::{deadline, expected as mode_expected, with_collection, SIMPLE};
use super::super::Mode;
use super::*;
use crate::{mime_traversal::bound::ordered::tests::SOURCE, ports::BlobReader};
pub(super) fn properties(bits: u8) -> Properties {
    Properties {
        body_structure: bits & 16 != 0,
        text_body: bits & 1 != 0,
        html_body: bits & 2 != 0,
        attachments: bits & 4 != 0,
        has_attachment: bits & 8 != 0,
    }
}
pub(super) fn expected(
    source: &[u8],
    bits: u8,
    cells: &[super::super::super::Cell<'_>],
) -> Vec<u8> {
    let mut fields = Vec::new();
    if bits & 16 != 0 {
        fields.push(String::from_utf8(mode_expected(source, Mode::Structure, cells)).unwrap());
    }
    let leaf = |index: usize| {
        let metadata = std::str::from_utf8(cells[index].value().unwrap().members).unwrap();
        format!(
            "{{{}{}\"subParts\":null}}",
            metadata,
            if metadata.is_empty() { "" } else { "," }
        )
    };
    let text = leaf(if source == SOURCE { 2 } else { 0 });
    if bits & 1 != 0 {
        fields.push(format!("\"textBody\":[{}]", text));
    }
    if bits & 2 != 0 {
        fields.push(format!("\"htmlBody\":[{}]", text));
    }
    if bits & 4 != 0 {
        fields.push(if source == SOURCE {
            format!("\"attachments\":[{}]", leaf(1))
        } else {
            "\"attachments\":[]".to_owned()
        });
    }
    if bits & 8 != 0 {
        fields.push(format!("\"hasAttachment\":{}", source == SOURCE));
    }
    fields.join(",").into_bytes()
}
fn drain(
    cursor: &mut Cursor<'_, '_, '_, '_, '_, '_, '_, '_, '_, '_, '_>,
    width: usize,
    none: bool,
) -> Result<Vec<u8>, Error> {
    let mut bytes = Vec::new();
    for turn in 0..10000 {
        let before = cursor.inner.costs();
        let mut output = [0xa5; 128];
        let p = cursor.poll(Tick(1), &mut output[..width])?;
        let after = cursor.inner.costs();
        assert_eq!((before[0], before[2]), (after[0], after[2]));
        assert_eq!(before[1] - after[1], if none { 0 } else { 1 });
        assert_eq!(before[3] - after[3], u64::from(!none && turn % 16 == 0));
        assert_eq!(before[4] - after[4], p.written as u64);
        assert!(p.written <= 64);
        assert!(output[p.written..].iter().all(|b| *b == 0xa5));
        bytes.extend_from_slice(&output[..p.written]);
        if p.status == Status::Complete {
            return Ok(bytes);
        }
    }
    panic!("requested selected composition stalled")
}
#[test]
fn requested_members_preserve_both_selections_and_every_original_owner() {
    for source in [SIMPLE, SOURCE] {
        for metadata in [0, 1, 513, 1023] {
            for bits in [0, 1, 2, 4, 8, 16, 17, 3, 31] {
                for base in [0, u64::MAX - source.len() as u64] {
                    let clock = TestClock::new();
                    with_collection(source, base, metadata, &clock, |serialized| {
                        let view = serialized.value().unwrap();
                        let bytes = expected(source, bits, view.original.members);
                        let cells = view.original.members.as_ptr();
                        let candidates = view.original.original.candidates.as_ptr();
                        let whole = view.original.original.original.members.as_ptr();
                        let mut cursor =
                            Cursor::new(serialized, properties(bits), Tick(1)).unwrap();
                        assert_eq!(cursor.value().is_some(), bits == 0);
                        let before = cursor.inner.costs();
                        let p = cursor.poll(Tick(1), &mut []).unwrap();
                        assert_eq!(
                            p,
                            Progress {
                                written: 0,
                                status: if bits == 0 {
                                    Status::Complete
                                } else {
                                    Status::NeedOutput
                                }
                            }
                        );
                        assert_eq!(cursor.inner.costs(), before);
                        assert_eq!(drain(&mut cursor, 64, bits == 0).unwrap(), bytes);
                        let after = cursor.inner.costs();
                        assert_eq!(before[4] - after[4], bytes.len() as u64);
                        let (props, view) = cursor.value().unwrap();
                        assert_eq!(props, properties(bits));
                        assert_eq!(view.properties, part_properties(metadata));
                        assert_eq!(view.original.members.as_ptr(), cells);
                        assert_eq!(
                            cursor.poll(Tick(100), &mut []),
                            Ok(Progress {
                                written: 0,
                                status: Status::Complete
                            })
                        );
                        assert_eq!(cursor.inner.costs(), after);
                        let composed = cursor.finish(Tick(1)).unwrap();
                        let (props, view) = composed.value().unwrap();
                        assert_eq!(props, properties(bits));
                        assert_eq!(view.properties, part_properties(metadata));
                        assert_eq!(view.original.original.candidates.as_ptr(), candidates);
                        let (serialized, props) = composed.finish(Tick(1)).unwrap();
                        assert_eq!(props, properties(bits));
                        let (bound, parts, slots) = serialized.finish(Tick(1)).unwrap();
                        assert_eq!(parts, part_properties(metadata));
                        assert_eq!(slots.as_ptr(), cells);
                        assert_eq!(bound.value().unwrap().original.members.as_ptr(), whole);
                        let (_, mut pin) = bound.finish(Tick(1)).unwrap();
                        assert_eq!(pin.read_at(0, &mut [0; 2]).unwrap(), 2);
                    });
                }
            }
        }
    }
}
#[test]
fn combined_structure_then_lists_crosses_bounded_output_widths() {
    for source in [SIMPLE, SOURCE] {
        for width in [1, 7, 63, 64, 65] {
            let clock = TestClock::new();
            with_collection(source, 0, 513, &clock, |serialized| {
                let bytes = expected(source, 31, serialized.value().unwrap().original.members);
                let mut cursor = Cursor::new(serialized, Properties::ALL, Tick(1)).unwrap();
                assert_eq!(drain(&mut cursor, width, false).unwrap(), bytes);
            });
        }
    }
}
#[test]
fn outer_none_is_fresh_zero_work_even_with_selected_metadata() {
    for metadata in [0, 513, 1023] {
        let clock = TestClock::new();
        with_collection(SOURCE, 0, metadata, &clock, |serialized| {
            let mut cursor = Cursor::new(serialized, Properties::NONE, Tick(1)).unwrap();
            let before = cursor.inner.costs();
            let (props, view) = cursor.value().unwrap();
            assert_eq!(props, Properties::NONE);
            assert_eq!(view.properties, part_properties(metadata));
            assert_eq!(
                cursor.poll(Tick(100), &mut []),
                Ok(Progress {
                    written: 0,
                    status: Status::Complete
                })
            );
            assert_eq!(cursor.inner.costs(), before);
            let composed = cursor.finish(Tick(1)).unwrap();
            assert_eq!(composed.value().unwrap().0, Properties::NONE);
            composed.finish(Tick(1)).unwrap();
        });
    }
}
#[test]
fn metadata_all_matches_legacy_requested_bytes_turns_and_five_costs() {
    for source in [SIMPLE, SOURCE] {
        for bits in [0, 1, 16, 17, 31] {
            let clock = TestClock::new();
            let turns = with_collection(source, 0, 1023, &clock, |serialized| {
                let mut cursor =
                    shared::Cursor::new(serialized.inner, properties(bits), Tick(1)).unwrap();
                let mut turns = Vec::new();
                let mut complete = false;
                for _ in 0..10000 {
                    let before = cursor.costs();
                    let mut out = [0; 64];
                    let p = cursor.poll(Tick(1), &mut out).unwrap();
                    let after = cursor.costs();
                    turns.push((
                        p,
                        out[..p.written].to_vec(),
                        std::array::from_fn::<_, 5, _>(|i| before[i] - after[i]),
                    ));
                    if p.status == Status::Complete {
                        complete = true;
                        break;
                    }
                }
                assert!(complete, "legacy requested composition stalled");
                turns
            });
            with_collection(source, 0, 1023, &clock, |serialized| {
                let mut cursor = Cursor::new(serialized, properties(bits), Tick(1)).unwrap();
                for (p, bytes, debits) in turns {
                    let before = cursor.inner.costs();
                    let mut out = [0; 64];
                    assert_eq!(cursor.poll(Tick(1), &mut out), Ok(p));
                    let after = cursor.inner.costs();
                    assert_eq!(
                        std::array::from_fn::<_, 5, _>(|i| before[i] - after[i]),
                        debits
                    );
                    assert_eq!(out[..p.written], bytes);
                }
            });
        }
    }
}
#[test]
fn constructor_and_unfinished_empty_output_keep_both_freshness_domains() {
    for actual in [false, true] {
        for bits in [0, 31] {
            let clock = TestClock::new();
            with_collection(SIMPLE, 0, 513, &clock, |serialized| {
                let tick = if actual {
                    clock.expire();
                    Tick(1)
                } else {
                    Tick(100)
                };
                assert_eq!(
                    Cursor::new(serialized, properties(bits), tick).err(),
                    Some(deadline(actual))
                );
            });
        }
        let clock = TestClock::new();
        with_collection(SIMPLE, 0, 0, &clock, |serialized| {
            let mut cursor = Cursor::new(serialized, Properties::ALL, Tick(1)).unwrap();
            let before = cursor.inner.costs();
            let tick = if actual {
                clock.expire();
                Tick(1)
            } else {
                Tick(100)
            };
            let error = deadline(actual);
            assert_eq!(cursor.poll(tick, &mut []), Err(error));
            assert_eq!(cursor.inner.costs(), before);
            assert!(cursor.value().is_none());
            assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
        });
    }
}
#[test]
fn incomplete_cursor_cannot_finish_and_consuming_completion_is_fresh() {
    let clock = TestClock::new();
    with_collection(SIMPLE, 0, 513, &clock, |serialized| {
        let cursor = Cursor::new(serialized, Properties::ALL, Tick(1)).unwrap();
        assert_eq!(cursor.finish(Tick(1)).err(), Some(Error::InvalidState));
    });
    for actual in [false, true] {
        for bits in [0, 31] {
            let clock = TestClock::new();
            with_collection(SIMPLE, 0, 513, &clock, |serialized| {
                let mut cursor = Cursor::new(serialized, properties(bits), Tick(1)).unwrap();
                drain(&mut cursor, 64, bits == 0).unwrap();
                let tick = if actual {
                    clock.expire();
                    Tick(1)
                } else {
                    Tick(100)
                };
                assert_eq!(cursor.finish(tick).err(), Some(deadline(actual)));
            });
        }
    }
}
#[test]
fn complete_cursor_and_composed_explicit_checks_hide_values_stickily() {
    for actual in [false, true] {
        for bits in [0, 31] {
            for phase in [0, 1, 2] {
                let clock = TestClock::new();
                with_collection(SIMPLE, 0, 0, &clock, |serialized| {
                    let mut cursor = Cursor::new(serialized, properties(bits), Tick(1)).unwrap();
                    drain(&mut cursor, 64, bits == 0).unwrap();
                    if phase == 0 {
                        let tick = if actual {
                            clock.expire();
                            Tick(1)
                        } else {
                            Tick(100)
                        };
                        let error = deadline(actual);
                        assert_eq!(cursor.check_deadline(tick), Err(error));
                        assert!(cursor.value().is_none());
                        assert_eq!(cursor.poll(Tick(1), &mut []), Err(error));
                        assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
                    } else {
                        let mut composed = cursor.finish(Tick(1)).unwrap();
                        let tick = if actual {
                            clock.expire();
                            Tick(1)
                        } else {
                            Tick(100)
                        };
                        let error = deadline(actual);
                        if phase == 1 {
                            assert_eq!(composed.check_deadline(tick), Err(error));
                            assert!(composed.value().is_none());
                            assert_eq!(composed.finish(Tick(1)).err(), Some(error));
                        } else {
                            assert_eq!(composed.finish(tick).err(), Some(error));
                        }
                    }
                });
            }
        }
    }
}
#[test]
fn selected_container_nulls_and_leaf_locators_remain_byte_identical() {
    let clock = TestClock::new();
    with_collection(
        SOURCE,
        u64::MAX - SOURCE.len() as u64,
        513,
        &clock,
        |serialized| {
            let bytes = expected(SOURCE, 17, serialized.value().unwrap().original.members);
            assert!(bytes
                .windows(b"\"blobId\":null".len())
                .any(|w| w == b"\"blobId\":null"));
            let mut cursor = Cursor::new(serialized, properties(17), Tick(1)).unwrap();
            assert_eq!(drain(&mut cursor, 1, false).unwrap(), bytes);
        },
    );
}
/// Original selected collection and all generated buffers are prepared cold.
pub fn probe(mut snapshot: impl FnMut()) {
    for trial in 0..8 {
        let source = if trial == 0 { SOURCE } else { SIMPLE };
        let base = if trial == 0 {
            u64::MAX - source.len() as u64
        } else {
            0
        };
        let clock = TestClock::new();
        with_collection(
            source,
            base,
            if trial == 1 { 0 } else { 513 },
            &clock,
            |serialized| {
                if trial == 7 {
                    clock.expire();
                }
                snapshot();
                if trial == 7 {
                    assert_eq!(
                        Cursor::new(serialized, Properties::ALL, Tick(1)).err(),
                        Some(deadline(true))
                    );
                } else {
                    let mut cursor = Cursor::new(
                        serialized,
                        if trial == 1 {
                            Properties::NONE
                        } else {
                            Properties::ALL
                        },
                        Tick(1),
                    )
                    .unwrap();
                    if trial == 6 {
                        assert_eq!(
                            cursor.poll(Tick(1), &mut []),
                            Ok(Progress {
                                written: 0,
                                status: Status::NeedOutput
                            })
                        );
                        drop(cursor);
                    } else if trial == 2 {
                        let mut copied = false;
                        for _ in 0..10000 {
                            let p = cursor.poll(Tick(1), &mut [0; 64]).unwrap();
                            assert_eq!(p.status, Status::Yield);
                            if p.written != 0 {
                                copied = true;
                                break;
                            }
                        }
                        assert!(copied, "requested composition copied no bytes");
                        clock.expire();
                        assert_eq!(cursor.poll(Tick(1), &mut [0; 64]), Err(deadline(true)));
                        drop(cursor);
                    } else {
                        let mut complete = false;
                        for _ in 0..10000 {
                            if cursor.poll(Tick(1), &mut [0; 64]).unwrap().status
                                == Status::Complete
                            {
                                complete = true;
                                break;
                            }
                        }
                        assert!(complete, "requested composition stalled");
                        if trial == 3 {
                            clock.expire();
                            assert_eq!(cursor.finish(Tick(1)).err(), Some(deadline(true)));
                        } else {
                            let composed = cursor.finish(Tick(1)).unwrap();
                            if trial == 4 {
                                clock.expire();
                                assert_eq!(composed.finish(Tick(1)).err(), Some(deadline(true)));
                            } else {
                                let (serialized, props) = composed.finish(Tick(1)).unwrap();
                                assert_eq!(
                                    props,
                                    if trial == 1 {
                                        Properties::NONE
                                    } else {
                                        Properties::ALL
                                    }
                                );
                                let (mut bound, parts, _) = serialized.finish(Tick(1)).unwrap();
                                assert_eq!(
                                    parts,
                                    part_properties(if trial == 1 { 0 } else { 513 })
                                );
                                if trial == 5 {
                                    clock.expire();
                                    assert_eq!(bound.check_deadline(Tick(1)), Err(deadline(true)));
                                }
                                drop(bound);
                            }
                        }
                    }
                }
                snapshot();
            },
        );
    }
}

#[test]
fn facade_wire_record_and_step_refusals_preserve_exact_sticky_errors() {
    use super::super::super::super::super::super::tests::with_bound;
    use super::super::super::{Cell, Collecting};
    use crate::admission::work::{Charge, Stop};
    for kind in 0..3 {
        let clock = TestClock::new();
        with_bound(SIMPLE, 0, &clock, |mut bound| {
            let structure = &mut bound.original.source.original.source.projected.structure;
            let left = structure.work.remaining();
            if kind == 2 {
                let steps = structure.budget.steps_remaining();
                structure
                    .budget
                    .charge(structure.work, Tick(1), 0, steps - 1, &mut 0)
                    .unwrap();
            } else {
                structure
                    .work
                    .charge(
                        Tick(1),
                        Charge {
                            records: if kind == 1 { left.records - 2 } else { 0 },
                            output_bytes: if kind == 0 { left.output_bytes } else { 0 },
                            ..Charge::default()
                        },
                    )
                    .unwrap();
            }
            let mut cells = [Cell::new(&mut [])];
            let mut collecting =
                Collecting::new(bound, PartProperties::NONE, &mut cells, Tick(1)).unwrap();
            collecting.next(Tick(1)).unwrap().finish(Tick(1)).unwrap();
            let serialized = collecting.finish(Tick(1)).unwrap();
            let mut cursor = Cursor::new(serialized, Properties::ALL, Tick(1)).unwrap();
            assert!(cursor.value().is_none());
            let expected = Error::Original(
                super::super::super::super::super::super::super::super::Error::Admission(
                    if kind == 2 {
                        crate::nfc::Error::InterpretationLimit
                    } else {
                        crate::nfc::Error::Work(if kind == 1 {
                            Stop::Records
                        } else {
                            Stop::OutputBytes
                        })
                    },
                ),
            );
            let mut output = [0xa5; 64];
            let mut refused = false;
            for _ in 0..100 {
                let before = cursor.inner.costs();
                let result = cursor.poll(Tick(1), &mut output);
                let after = cursor.inner.costs();
                assert_eq!((before[0], before[2]), (after[0], after[2]));
                match result {
                    Ok(progress) => assert_eq!(progress.written, 0),
                    Err(error) => {
                        assert_eq!(error, expected);
                        assert!(cursor.value().is_none());
                        assert_eq!(cursor.poll(Tick(100), &mut output), Err(error));
                        assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
                        refused = true;
                        break;
                    }
                }
            }
            assert!(refused);
            assert_eq!(output, [0xa5; 64]);
        });
    }
}

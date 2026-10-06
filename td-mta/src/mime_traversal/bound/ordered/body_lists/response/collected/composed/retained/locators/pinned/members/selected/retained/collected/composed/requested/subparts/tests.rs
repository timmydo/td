#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::super::super::super::super::super::super::super::tests::TestClock;
use super::super::super::super::super::super::tests::properties as metadata;
use super::super::super::tests::{deadline, with_collection, SIMPLE};
use super::super::tests::{expected as legacy_expected, properties};
use super::*;
use crate::{mime_traversal::bound::ordered::tests::SOURCE, ports::BlobReader};
fn selection(bits: u8, sub_parts: bool) -> Selection {
    Selection {
        properties: properties(bits),
        sub_parts,
    }
}
fn expected(source: &[u8], bits: u8, cells: &[super::super::super::super::Cell<'_>]) -> Vec<u8> {
    let leaf = |i: usize| {
        format!(
            "{{{}}}",
            std::str::from_utf8(cells[i].value().unwrap().members).unwrap()
        )
    };
    let mut fields = Vec::new();
    if bits & 16 != 0 {
        fields.push(format!("\"bodyStructure\":{}", leaf(0)));
    }
    if bits & 1 != 0 {
        fields.push(format!(
            "\"textBody\":[{}]",
            leaf(if source == SOURCE { 2 } else { 0 })
        ));
    }
    if bits & 2 != 0 {
        fields.push(format!(
            "\"htmlBody\":[{}]",
            leaf(if source == SOURCE { 2 } else { 0 })
        ));
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
fn drain(cursor: &mut Cursor<'_, '_, '_, '_, '_, '_, '_, '_, '_, '_, '_>, width: usize) -> Vec<u8> {
    let mut bytes = Vec::new();
    for turn in 0..10000 {
        let before = cursor.inner.costs();
        let mut output = [0xa5; 128];
        let p = cursor.poll(Tick(1), &mut output[..width]).unwrap();
        let after = cursor.inner.costs();
        let none = cursor.selection.properties == super::super::Properties::NONE;
        assert_eq!((before[0], before[2]), (after[0], after[2]));
        assert_eq!(before[1] - after[1], u64::from(!none));
        assert_eq!(before[3] - after[3], u64::from(!none && turn % 16 == 0));
        assert_eq!(before[4] - after[4], p.written as u64);
        assert!(p.written <= 64);
        assert!(output[p.written..].iter().all(|b| *b == 0xa5));
        bytes.extend_from_slice(&output[..p.written]);
        if p.status == Status::Complete {
            return bytes;
        }
    }
    panic!("subParts composition stalled")
}
#[test]
fn absent_subparts_closes_root_and_every_leaf_without_descending() {
    for source in [SIMPLE, SOURCE] {
        for mask in [0, 1, 513, 1023] {
            for bits in [0, 1, 2, 4, 8, 16, 17, 31] {
                let clock = TestClock::new();
                with_collection(
                    source,
                    u64::MAX - source.len() as u64,
                    mask,
                    &clock,
                    |serialized| {
                        let view = serialized.value().unwrap();
                        let bytes = expected(source, bits, view.original.members);
                        let cells = view.original.members.as_ptr();
                        let candidates = view.original.original.candidates.as_ptr();
                        let mut cursor =
                            Cursor::new(serialized, selection(bits, false), Tick(1)).unwrap();
                        assert_eq!(cursor.value().is_some(), bits == 0);
                        assert_eq!(drain(&mut cursor, 64), bytes);
                        let (s, view) = cursor.value().unwrap();
                        assert_eq!(s, selection(bits, false));
                        assert_eq!(view.properties, metadata(mask));
                        assert_eq!(view.original.members.as_ptr(), cells);
                        assert_eq!(view.original.original.candidates.as_ptr(), candidates);
                        let composed = cursor.finish(Tick(1)).unwrap();
                        assert_eq!(composed.value().unwrap().0, s);
                        let (serialized, released) = composed.finish(Tick(1)).unwrap();
                        assert_eq!(released, s);
                        let (bound, fields, slots) = serialized.finish(Tick(1)).unwrap();
                        assert_eq!(fields, metadata(mask));
                        assert_eq!(slots.as_ptr(), cells);
                        let (_, mut pin) = bound.finish(Tick(1)).unwrap();
                        assert_eq!(pin.read_at(0, &mut [0; 2]).unwrap(), 2);
                    },
                );
            }
        }
    }
}
#[test]
fn empty_metadata_has_independent_literal_json_oracles() {
    let clock = TestClock::new();
    with_collection(SOURCE, 0, 0, &clock, |serialized| {
        let mut cursor = Cursor::new(serialized, selection(31, false), Tick(1)).unwrap();
        let expected = b"\"bodyStructure\":{},\"textBody\":[{}],\"htmlBody\":[{}],\"attachments\":[{}],\"hasAttachment\":true";
        assert_eq!(drain(&mut cursor, 1), expected);
    });
    with_collection(SIMPLE, 0, 0, &clock, |serialized| {
        let mut cursor = Cursor::new(serialized, selection(31, true), Tick(1)).unwrap();
        let expected = b"\"bodyStructure\":{\"subParts\":null},\"textBody\":[{\"subParts\":null}],\"htmlBody\":[{\"subParts\":null}],\"attachments\":[],\"hasAttachment\":false";
        assert_eq!(drain(&mut cursor, 1), expected);
    });
}
#[test]
fn present_subparts_matches_legacy_turns_bytes_and_five_debits() {
    for source in [SIMPLE, SOURCE] {
        for bits in [0, 1, 16, 17, 31] {
            let clock = TestClock::new();
            let trace = with_collection(source, 0, 513, &clock, |serialized| {
                let bytes =
                    legacy_expected(source, bits, serialized.value().unwrap().original.members);
                let mut cursor =
                    super::super::Cursor::new(serialized, properties(bits), Tick(1)).unwrap();
                let mut trace = Vec::new();
                let mut all = Vec::new();
                let mut complete = false;
                for _ in 0..10000 {
                    let before = cursor.inner.costs();
                    let mut out = [0; 7];
                    let p = cursor.poll(Tick(1), &mut out).unwrap();
                    let after = cursor.inner.costs();
                    all.extend_from_slice(&out[..p.written]);
                    trace.push((
                        p,
                        out,
                        std::array::from_fn::<_, 5, _>(|i| before[i] - after[i]),
                    ));
                    if p.status == Status::Complete {
                        complete = true;
                        break;
                    }
                }
                assert!(complete);
                assert_eq!(all, bytes);
                trace
            });
            with_collection(source, 0, 513, &clock, |serialized| {
                let mut cursor = Cursor::new(serialized, selection(bits, true), Tick(1)).unwrap();
                for (p, bytes, debits) in trace {
                    let before = cursor.inner.costs();
                    let mut out = [0; 7];
                    assert_eq!(cursor.poll(Tick(1), &mut out), Ok(p));
                    let after = cursor.inner.costs();
                    assert_eq!(out, bytes);
                    assert_eq!(
                        std::array::from_fn::<_, 5, _>(|i| before[i] - after[i]),
                        debits
                    );
                }
                assert_eq!(cursor.value().unwrap().0, selection(bits, true));
            });
        }
    }
}
#[test]
fn both_choices_cross_bounded_widths_and_zero_output_is_unpaid() {
    for sub_parts in [false, true] {
        for width in [1, 7, 63, 64, 65] {
            let clock = TestClock::new();
            with_collection(SOURCE, 0, 513, &clock, |serialized| {
                let cells = serialized.value().unwrap().original.members;
                let bytes = if sub_parts {
                    legacy_expected(SOURCE, 31, cells)
                } else {
                    expected(SOURCE, 31, cells)
                };
                let mut cursor =
                    Cursor::new(serialized, selection(31, sub_parts), Tick(1)).unwrap();
                let before = cursor.inner.costs();
                assert_eq!(
                    cursor.poll(Tick(1), &mut []),
                    Ok(Progress {
                        written: 0,
                        status: Status::NeedOutput
                    })
                );
                assert_eq!(cursor.inner.costs(), before);
                assert_eq!(drain(&mut cursor, width), bytes);
            });
        }
    }
}
#[test]
fn every_omitted_tree_turn_refuses_both_deadlines_before_copy() {
    let clock = TestClock::new();
    let turns = with_collection(SOURCE, 0, 513, &clock, |serialized| {
        let mut cursor = Cursor::new(serialized, selection(31, false), Tick(1)).unwrap();
        let mut count = 0;
        loop {
            count += 1;
            if cursor.poll(Tick(1), &mut [0; 64]).unwrap().status == Status::Complete {
                break;
            }
            assert!(count < 10000);
        }
        count
    });
    for actual in [false, true] {
        for cut in 0..turns {
            let clock = TestClock::new();
            with_collection(SOURCE, 0, 513, &clock, |serialized| {
                let mut cursor = Cursor::new(serialized, selection(31, false), Tick(1)).unwrap();
                for _ in 0..cut {
                    assert_eq!(
                        cursor.poll(Tick(1), &mut [0; 64]).unwrap().status,
                        Status::Yield
                    );
                }
                let before = cursor.inner.costs();
                let tick = if actual {
                    clock.expire();
                    Tick(1)
                } else {
                    Tick(100)
                };
                let mut out = [0xa5; 64];
                assert_eq!(cursor.poll(tick, &mut out), Err(deadline(actual)));
                assert_eq!(cursor.inner.costs(), before);
                assert_eq!(out, [0xa5; 64]);
                assert!(cursor.value().is_none());
                assert_eq!(cursor.finish(Tick(1)).err(), Some(deadline(actual)));
            });
        }
    }
}
#[test]
fn omission_and_outer_none_still_require_fresh_construction_and_finish() {
    for sub_parts in [false, true] {
        for bits in [0, 31] {
            for actual in [false, true] {
                let clock = TestClock::new();
                with_collection(SIMPLE, 0, 0, &clock, |serialized| {
                    let tick = if actual {
                        clock.expire();
                        Tick(1)
                    } else {
                        Tick(100)
                    };
                    assert_eq!(
                        Cursor::new(serialized, selection(bits, sub_parts), tick).err(),
                        Some(deadline(actual))
                    );
                });
                let clock = TestClock::new();
                with_collection(SIMPLE, 0, 0, &clock, |serialized| {
                    let mut cursor =
                        Cursor::new(serialized, selection(bits, sub_parts), Tick(1)).unwrap();
                    drain(&mut cursor, 64);
                    let before = cursor.inner.costs();
                    assert_eq!(
                        cursor.poll(Tick(100), &mut []),
                        Ok(Progress {
                            written: 0,
                            status: Status::Complete
                        })
                    );
                    assert_eq!(cursor.inner.costs(), before);
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
}
#[test]
fn explicit_complete_checks_and_composed_release_hide_values_stickily() {
    for sub_parts in [false, true] {
        for bits in [0, 31] {
            for phase in 0..3 {
                for actual in [false, true] {
                    let clock = TestClock::new();
                    with_collection(SIMPLE, 0, 513, &clock, |serialized| {
                        let mut cursor =
                            Cursor::new(serialized, selection(bits, sub_parts), Tick(1)).unwrap();
                        drain(&mut cursor, 64);
                        if phase == 0 {
                            let tick = if actual {
                                clock.expire();
                                Tick(1)
                            } else {
                                Tick(100)
                            };
                            assert_eq!(cursor.check_deadline(tick), Err(deadline(actual)));
                            assert!(cursor.value().is_none());
                            assert_eq!(cursor.poll(Tick(1), &mut []), Err(deadline(actual)));
                        } else {
                            let mut composed = cursor.finish(Tick(1)).unwrap();
                            let tick = if actual {
                                clock.expire();
                                Tick(1)
                            } else {
                                Tick(100)
                            };
                            if phase == 1 {
                                assert_eq!(composed.check_deadline(tick), Err(deadline(actual)));
                                assert!(composed.value().is_none());
                                assert_eq!(composed.finish(Tick(1)).err(), Some(deadline(actual)));
                            } else {
                                assert_eq!(composed.finish(tick).err(), Some(deadline(actual)));
                            }
                        }
                    });
                }
            }
        }
    }
}
#[test]
fn premature_ownership_release_refuses_incomplete_false_selection() {
    let clock = TestClock::new();
    with_collection(SOURCE, 0, 513, &clock, |serialized| {
        let mut cursor = Cursor::new(serialized, selection(31, false), Tick(1)).unwrap();
        assert!(cursor.value().is_none());
        assert_eq!(
            cursor.poll(Tick(1), &mut [0; 1]).unwrap().status,
            Status::Yield
        );
        assert!(cursor.value().is_none());
        assert_eq!(cursor.finish(Tick(1)).err(), Some(Error::InvalidState));
    });
}
/// The collection, file, fragments and refusal state are prepared cold.
pub fn probe(mut snapshot: impl FnMut()) {
    for trial in 0..8 {
        let clock = TestClock::new();
        with_collection(
            if trial == 0 || trial == 2 {
                SOURCE
            } else {
                SIMPLE
            },
            0,
            if trial == 1 { 0 } else { 513 },
            &clock,
            |serialized| {
                if trial == 7 {
                    clock.expire();
                }
                let s = selection(if trial == 1 { 0 } else { 31 }, trial == 2);
                snapshot();
                if trial == 7 {
                    assert_eq!(
                        Cursor::new(serialized, s, Tick(1)).err(),
                        Some(deadline(true))
                    );
                } else {
                    let mut cursor = Cursor::new(serialized, s, Tick(1)).unwrap();
                    if trial == 6 {
                        assert_eq!(
                            cursor.poll(Tick(1), &mut []),
                            Ok(Progress {
                                written: 0,
                                status: Status::NeedOutput
                            })
                        );
                        drop(cursor);
                    } else if trial == 3 {
                        let mut copied = false;
                        for _ in 0..10000 {
                            let p = cursor.poll(Tick(1), &mut [0; 64]).unwrap();
                            if p.written != 0 {
                                copied = true;
                                break;
                            }
                        }
                        assert!(copied);
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
                        assert!(complete);
                        if trial == 4 {
                            clock.expire();
                            assert_eq!(cursor.finish(Tick(1)).err(), Some(deadline(true)));
                        } else {
                            let composed = cursor.finish(Tick(1)).unwrap();
                            if trial == 5 {
                                clock.expire();
                                assert_eq!(composed.finish(Tick(1)).err(), Some(deadline(true)));
                            } else {
                                let (serialized, released) = composed.finish(Tick(1)).unwrap();
                                assert_eq!(released, s);
                                let (bound, fields, _) = serialized.finish(Tick(1)).unwrap();
                                assert_eq!(fields, metadata(if trial == 1 { 0 } else { 513 }));
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
fn nested_tree_omission_stops_at_root_while_selected_children_recurse() {
    const NESTED: &[u8] = b"Content-Type: multipart/mixed;boundary=x\r\n\r\n--x\r\nContent-Type: multipart/mixed;boundary=y\r\n\r\n--y\r\n\r\nabc\r\n--y--\r\n--x--\r\n";
    for sub_parts in [false, true] {
        let clock = TestClock::new();
        with_collection(NESTED, 0, 0, &clock, |serialized| {
            assert_eq!(serialized.value().unwrap().original.members.len(), 3);
            let expected: &[u8] = if sub_parts {
                br#""bodyStructure":{"subParts":[{"subParts":[{"subParts":null}]}]}"#
            } else {
                br#""bodyStructure":{}"#
            };
            let mut cursor = Cursor::new(serialized, selection(16, sub_parts), Tick(1)).unwrap();
            assert_eq!(drain(&mut cursor, 1), expected);
        });
    }
}

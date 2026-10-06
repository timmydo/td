#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::super::super::super::super::super::tests::TestClock;
use super::super::super::super::super::tests::with_bound;
use super::super::super::super::tests::properties;
use super::super::{Cell, Collecting};
use super::*;
use crate::{
    admission::work::Stop,
    mime_traversal::bound::ordered::tests::SOURCE,
    ports::{BlobReader, Clock, Error as PolicyError},
};
pub(super) const SIMPLE: &[u8] = b"\r\nabc\r\n";
const MULTIPART: &[u8] =
    b"Content-Type: multipart/mixed;boundary=x\r\n\r\n--x\r\n\r\nabc\r\n--x--\r\n";
pub(super) fn with_collection<T>(
    source: &[u8],
    base: u64,
    bits: u16,
    clock: &dyn Clock,
    run: impl FnOnce(Serialized<'_, '_, '_, '_, '_, '_, '_, '_, '_, '_, '_>) -> T,
) -> T {
    with_bound(source, base, clock, |bound| {
        let count = bound.value().unwrap().candidates.len();
        let mut backing = vec![[0xa5; 512]; count];
        let mut cells: Vec<_> = backing
            .iter_mut()
            .map(|b| Cell::new(if bits == 0 { &mut b[..0] } else { b }))
            .collect();
        let mut collecting = Collecting::new(bound, properties(bits), &mut cells, Tick(1)).unwrap();
        for _ in 0..count {
            let mut child = collecting.next(Tick(1)).unwrap();
            let mut done = false;
            for _ in 0..10000 {
                if child.poll(Tick(1)).unwrap() == super::super::Status::Complete {
                    done = true;
                    break;
                }
            }
            assert!(done);
            child.finish(Tick(1)).unwrap();
        }
        run(collecting.finish(Tick(1)).unwrap())
    })
}
pub(super) fn expected(source: &[u8], mode: Mode, cells: &[Cell<'_>]) -> Vec<u8> {
    let metadata =
        |index: usize| std::str::from_utf8(cells[index].value().unwrap().members).unwrap();
    let sep = |index: usize| if metadata(index).is_empty() { "" } else { "," };
    let leaf = |index: usize| format!("{{{}{}\"subParts\":null}}", metadata(index), sep(index));
    if mode == Mode::Structure {
        let tree = if source == SOURCE {
            format!(
                "{{{}{}\"subParts\":[{},{}]}}",
                metadata(0),
                sep(0),
                leaf(1),
                leaf(2)
            )
        } else if source == MULTIPART {
            format!("{{{}{}\"subParts\":[{}]}}", metadata(0), sep(0), leaf(1))
        } else {
            leaf(0)
        };
        return format!("\"bodyStructure\":{}", tree).into_bytes();
    }
    if source == SOURCE {
        format!(
            "\"textBody\":[{}],\"htmlBody\":[{}],\"attachments\":[{}],\"hasAttachment\":true",
            leaf(2),
            leaf(2),
            leaf(1)
        )
        .into_bytes()
    } else {
        let index = if source == MULTIPART { 1 } else { 0 };
        format!(
            "\"textBody\":[{}],\"htmlBody\":[{}],\"attachments\":[],\"hasAttachment\":false",
            leaf(index),
            leaf(index)
        )
        .into_bytes()
    }
}
fn drain(
    cursor: &mut Cursor<'_, '_, '_, '_, '_, '_, '_, '_, '_, '_, '_>,
    width: usize,
) -> Result<Vec<u8>, Error> {
    let mut bytes = Vec::new();
    for turn in 0..10000 {
        let before = cursor.inner.costs();
        let mut output = [0xa5; 128];
        let p = cursor.poll(Tick(1), &mut output[..width])?;
        let after = cursor.inner.costs();
        assert_eq!((before[0], before[2]), (after[0], after[2]));
        assert_eq!(before[1] - after[1], 1);
        assert_eq!(before[3] - after[3], u64::from(turn % 16 == 0));
        assert_eq!(before[4] - after[4], p.written as u64);
        assert!(p.written <= 64);
        assert!(output[p.written..].iter().all(|b| *b == 0xa5));
        bytes.extend_from_slice(&output[..p.written]);
        if p.status == Status::Complete {
            return Ok(bytes);
        }
    }
    panic!("selected composition stalled")
}
pub(super) fn deadline(actual: bool) -> Error {
    if actual {
        Error::Parent(PolicyError::Deadline)
    } else {
        Error::Original(
            super::super::super::super::super::super::super::Error::Admission(
                crate::nfc::Error::Work(Stop::Deadline),
            ),
        )
    }
}
#[test]
fn selected_tree_lists_match_literal_framing_and_keep_every_original_owner() {
    for source in [SIMPLE, MULTIPART, SOURCE] {
        for bits in [0, 1, 128, 256, 512, 513, 1023] {
            for base in [0, 17, u64::MAX - source.len() as u64] {
                for mode in [Mode::Structure, Mode::Lists] {
                    let widths: &[usize] = if bits == 513 && base == 0 {
                        &[1, 7, 63, 64, 65]
                    } else {
                        &[64]
                    };
                    for &width in widths {
                        let clock = TestClock::new();
                        with_collection(source, base, bits, &clock, |serialized| {
                            let view = serialized.value().unwrap();
                            let expected = expected(source, mode, view.original.members);
                            let cells = view.original.members.as_ptr();
                            let candidates = view.original.original.candidates.as_ptr();
                            let original = view.original.original.original.members.as_ptr();
                            let mut cursor = Cursor::new(serialized, mode, Tick(1)).unwrap();
                            let before = cursor.inner.costs();
                            assert!(cursor.value().is_none());
                            assert_eq!(
                                cursor.poll(Tick(1), &mut []),
                                Ok(Progress {
                                    written: 0,
                                    status: Status::NeedOutput
                                })
                            );
                            assert_eq!(cursor.inner.costs(), before);
                            assert_eq!(drain(&mut cursor, width).unwrap(), expected);
                            let after = cursor.inner.costs();
                            assert_eq!(before[4] - after[4], expected.len() as u64);
                            let (actual, view) = cursor.value().unwrap();
                            assert_eq!(actual, mode);
                            assert_eq!(view.properties, properties(bits));
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
                            let (actual, view) = composed.value().unwrap();
                            assert_eq!(actual, mode);
                            assert_eq!(view.properties, properties(bits));
                            assert_eq!(view.original.original.candidates.as_ptr(), candidates);
                            let (serialized, actual) = composed.finish(Tick(1)).unwrap();
                            assert_eq!(actual, mode);
                            let (bound, props, slots) = serialized.finish(Tick(1)).unwrap();
                            assert_eq!(props, properties(bits));
                            assert_eq!(slots.as_ptr(), cells);
                            assert_eq!(bound.value().unwrap().original.members.as_ptr(), original);
                            let (_, mut pin) = bound.finish(Tick(1)).unwrap();
                            assert_eq!(pin.read_at(0, &mut [0; 2]).unwrap(), 2);
                        });
                    }
                }
            }
        }
    }
}
#[test]
fn none_emits_objects_without_leading_member_commas() {
    let clock = TestClock::new();
    for mode in [Mode::Structure, Mode::Lists] {
        with_collection(MULTIPART, 0, 0, &clock, |serialized| {
            let mut cursor = Cursor::new(serialized, mode, Tick(1)).unwrap();
            let bytes = drain(&mut cursor, 1).unwrap();
            assert_eq!(
                bytes,
                if mode == Mode::Structure {
                    b"\"bodyStructure\":{\"subParts\":[{\"subParts\":null}]}".as_slice()
                } else {
                    b"\"textBody\":[{\"subParts\":null}],\"htmlBody\":[{\"subParts\":null}],\"attachments\":[],\"hasAttachment\":false".as_slice()
                }
            );
        });
    }
}
#[test]
fn all_preserves_legacy_bytes_turns_and_five_costs() {
    for source in [SIMPLE, SOURCE] {
        for mode in [Mode::Structure, Mode::Lists] {
            for width in [1, 64] {
                let clock = TestClock::new();
                let legacy = with_collection(source, 0, 1023, &clock, |serialized| {
                    let mut cursor = shared::Cursor::new(serialized.inner, mode, Tick(1)).unwrap();
                    let mut turns = Vec::new();
                    let mut complete = false;
                    for _ in 0..10000 {
                        let before = cursor.costs();
                        let mut output = [0; 64];
                        let p = cursor.poll(Tick(1), &mut output[..width]).unwrap();
                        let after = cursor.costs();
                        turns.push((
                            p,
                            output[..p.written].to_vec(),
                            std::array::from_fn::<_, 5, _>(|i| before[i] - after[i]),
                        ));
                        if p.status == Status::Complete {
                            complete = true;
                            break;
                        }
                    }
                    assert!(complete, "legacy composition stalled");
                    turns
                });
                with_collection(source, 0, 1023, &clock, |serialized| {
                    let mut cursor = Cursor::new(serialized, mode, Tick(1)).unwrap();
                    for (p, bytes, debits) in legacy {
                        let before = cursor.inner.costs();
                        let mut output = [0; 64];
                        assert_eq!(cursor.poll(Tick(1), &mut output[..width]), Ok(p));
                        assert_eq!(output[..p.written], bytes);
                        let after = cursor.inner.costs();
                        assert_eq!(
                            std::array::from_fn::<_, 5, _>(|i| before[i] - after[i]),
                            debits
                        );
                    }
                });
            }
        }
    }
}
#[test]
fn constructor_and_empty_output_check_both_freshness_domains() {
    for actual in [false, true] {
        for constructor in [false, true] {
            let clock = TestClock::new();
            with_collection(SIMPLE, 0, 0, &clock, |serialized| {
                if constructor {
                    let tick = if actual {
                        clock.expire();
                        Tick(1)
                    } else {
                        Tick(100)
                    };
                    assert_eq!(
                        Cursor::new(serialized, Mode::Lists, tick).err(),
                        Some(deadline(actual))
                    );
                } else {
                    let mut cursor = Cursor::new(serialized, Mode::Structure, Tick(1)).unwrap();
                    let before = cursor.inner.costs();
                    let tick = if actual {
                        clock.expire();
                        Tick(1)
                    } else {
                        Tick(100)
                    };
                    assert_eq!(cursor.poll(tick, &mut []), Err(deadline(actual)));
                    assert_eq!(cursor.inner.costs(), before);
                    assert!(cursor.value().is_none());
                    assert_eq!(cursor.finish(Tick(1)).err(), Some(deadline(actual)));
                }
            });
        }
    }
}
#[test]
fn incomplete_and_complete_cursor_consumption_stays_fresh() {
    for actual in [false, true] {
        for complete in [false, true] {
            let clock = TestClock::new();
            with_collection(SIMPLE, 0, 512, &clock, |serialized| {
                let mut cursor = Cursor::new(serialized, Mode::Lists, Tick(1)).unwrap();
                if complete {
                    drain(&mut cursor, 64).unwrap();
                } else {
                    assert_eq!(
                        cursor.poll(Tick(1), &mut [0; 1]).unwrap().status,
                        Status::Yield
                    );
                }
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
    let clock = TestClock::new();
    with_collection(SIMPLE, 0, 0, &clock, |serialized| {
        let cursor = Cursor::new(serialized, Mode::Lists, Tick(1)).unwrap();
        assert_eq!(cursor.finish(Tick(1)).err(), Some(Error::InvalidState));
    });
}
#[test]
fn complete_owner_checks_hide_value_and_remember_exact_refusal() {
    for actual in [false, true] {
        for consuming in [false, true] {
            for bits in [0, 513, 1023] {
                let clock = TestClock::new();
                with_collection(SIMPLE, 0, bits, &clock, |serialized| {
                    let mut cursor = Cursor::new(serialized, Mode::Structure, Tick(1)).unwrap();
                    drain(&mut cursor, 64).unwrap();
                    let mut composed = cursor.finish(Tick(1)).unwrap();
                    let tick = if actual {
                        clock.expire();
                        Tick(1)
                    } else {
                        Tick(100)
                    };
                    if consuming {
                        assert_eq!(composed.finish(tick).err(), Some(deadline(actual)));
                    } else {
                        let error = deadline(actual);
                        assert_eq!(composed.check_deadline(tick), Err(error));
                        assert!(composed.value().is_none());
                        assert_eq!(composed.finish(Tick(1)).err(), Some(error));
                    }
                });
            }
        }
    }
}
#[test]
fn complete_cursor_explicit_checks_hide_value_and_stick_in_both_domains() {
    for bits in [0, 513, 1023] {
        for actual in [false, true] {
            let clock = TestClock::new();
            with_collection(SIMPLE, 0, bits, &clock, |serialized| {
                let mut cursor = Cursor::new(serialized, Mode::Lists, Tick(1)).unwrap();
                drain(&mut cursor, 64).unwrap();
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
            });
        }
    }
}
#[test]
fn frame_wire_refusal_prevents_completion_and_remembers_exact_error() {
    let clock = TestClock::new();
    with_bound(SIMPLE, 0, &clock, |bound| {
        let work = &mut *bound
            .original
            .source
            .original
            .source
            .projected
            .structure
            .work;
        let remaining = work.remaining().output_bytes;
        work.charge(
            Tick(1),
            crate::admission::work::Charge {
                output_bytes: remaining,
                ..crate::admission::work::Charge::default()
            },
        )
        .unwrap();
        let mut cells = [Cell::new(&mut [])];
        let mut collecting = Collecting::new(bound, Properties::NONE, &mut cells, Tick(1)).unwrap();
        let child = collecting.next(Tick(1)).unwrap();
        child.finish(Tick(1)).unwrap();
        let serialized = collecting.finish(Tick(1)).unwrap();
        let mut cursor = Cursor::new(serialized, Mode::Structure, Tick(1)).unwrap();
        assert_eq!(
            cursor.poll(Tick(1), &mut [0xa5; 64]).unwrap().status,
            Status::Yield
        );
        let mut output = [0xa5; 64];
        let error = Error::Original(
            super::super::super::super::super::super::super::Error::Admission(
                crate::nfc::Error::Work(Stop::OutputBytes),
            ),
        );
        assert_eq!(cursor.poll(Tick(1), &mut output), Err(error));
        assert_eq!(output, [0xa5; 64]);
        assert!(cursor.value().is_none());
        assert_eq!(cursor.poll(Tick(100), &mut output), Err(error));
        assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
    });
}

/// Matching, generated member windows, cells and filesystem preparation stay cold.
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
                        Cursor::new(serialized, Mode::Structure, Tick(1)).err(),
                        Some(deadline(true))
                    );
                } else {
                    let mut cursor = Cursor::new(
                        serialized,
                        if trial == 1 {
                            Mode::Lists
                        } else {
                            Mode::Structure
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
                            let mut output = [0; 64];
                            let p = cursor.poll(Tick(1), &mut output).unwrap();
                            assert_eq!(p.status, Status::Yield);
                            if p.written != 0 {
                                copied = true;
                                break;
                            }
                        }
                        assert!(copied, "selected composition copied no bytes");
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
                        assert!(complete, "selected composition stalled");
                        if trial == 3 {
                            clock.expire();
                            assert_eq!(cursor.finish(Tick(1)).err(), Some(deadline(true)));
                        } else {
                            let composed = cursor.finish(Tick(1)).unwrap();
                            assert_eq!(
                                composed.value().unwrap().1.properties,
                                properties(if trial == 1 { 0 } else { 513 })
                            );
                            if trial == 4 {
                                clock.expire();
                                assert_eq!(composed.finish(Tick(1)).err(), Some(deadline(true)));
                            } else {
                                let (serialized, _) = composed.finish(Tick(1)).unwrap();
                                let (mut bound, _, _) = serialized.finish(Tick(1)).unwrap();
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

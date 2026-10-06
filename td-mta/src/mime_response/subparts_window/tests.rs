#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use crate::mime_response::bound::Error as OriginalError;
use crate::mime_response::pinned::tests::TestClock;
use crate::mime_response::requested_selected_json::tests::properties;
use crate::mime_response::selected_metadata::tests::properties as metadata;
use crate::mime_response::subparts_window::*;
use {crate::mime_response::ordered::tests::SOURCE, crate::ports::BlobReader};
use {
    crate::mime_response::selected_metadata_json::tests::deadline,
    crate::mime_response::selected_metadata_json::tests::with_collection,
    crate::mime_response::selected_metadata_json::tests::SIMPLE,
};
fn selection(bits: u8, sub_parts: bool) -> Selection {
    Selection {
        properties: properties(bits),
        sub_parts,
    }
}
fn drain(cursor: &mut Cursor<'_, '_, '_, '_, '_, '_, '_, '_, '_, '_, '_, '_>) -> Result<(), Error> {
    for _ in 0..10000 {
        if cursor.poll(Tick(1))? == Status::Complete {
            return Ok(());
        }
        assert!(cursor.value().is_none());
    }
    panic!("whole subParts retention stalled")
}
fn streamed(source: &[u8], mask: u16, s: Selection) -> Vec<u8> {
    let clock = TestClock::new();
    with_collection(source, 0, mask, &clock, |serialized| {
        let mut cursor =
            crate::mime_response::subparts_json::Cursor::new(serialized, s, Tick(1)).unwrap();
        let mut bytes = Vec::new();
        for _ in 0..10000 {
            let mut out = [0; 64];
            let p = cursor.poll(Tick(1), &mut out).unwrap();
            bytes.extend_from_slice(&out[..p.written]);
            if p.status == crate::mime_response::subparts_json::Status::Complete {
                return bytes;
            }
        }
        panic!("streamed subParts oracle stalled")
    })
}
#[test]
fn whole_bytes_preserve_selection_and_every_original_owner() {
    for source in [SIMPLE, SOURCE] {
        for mask in [0, 513, 1023] {
            for bits in [0, 1, 16, 17, 31] {
                for sub_parts in [false, true] {
                    let s = selection(bits, sub_parts);
                    let bytes = streamed(source, mask, s);
                    let mut output = vec![0xa5; bytes.len() + 1];
                    let output_ptr = output.as_ptr();
                    let clock = TestClock::new();
                    with_collection(
                        source,
                        u64::MAX - source.len() as u64,
                        mask,
                        &clock,
                        |serialized| {
                            let view = serialized.value().unwrap();
                            let cells = view.original.members.as_ptr();
                            let candidates = view.original.original.candidates.as_ptr();
                            let mut cursor =
                                Cursor::new(serialized, s, &mut output, Tick(1)).unwrap();
                            let before = cursor.costs();
                            drain(&mut cursor).unwrap();
                            let after = cursor.costs();
                            assert_eq!((before[0], before[2]), (after[0], after[2]));
                            assert_eq!(before[4] - after[4], bytes.len() as u64);
                            let view = cursor.value().unwrap();
                            assert_eq!(view.selection, s);
                            assert_eq!(view.members, bytes);
                            assert_eq!(view.members.as_ptr(), output_ptr);
                            assert_eq!(view.original.properties, metadata(mask));
                            assert_eq!(view.original.original.members.as_ptr(), cells);
                            assert_eq!(
                                view.original.original.original.candidates.as_ptr(),
                                candidates
                            );
                            assert_eq!(cursor.poll(Tick(100)), Ok(Status::Complete));
                            assert_eq!(cursor.costs(), after);
                            let retained = cursor.finish(Tick(1)).unwrap();
                            let view = retained.value().unwrap();
                            assert_eq!(view.selection, s);
                            assert_eq!(view.members, bytes);
                            assert_eq!(view.members.as_ptr(), output_ptr);
                            assert_eq!(view.original.properties, metadata(mask));
                            assert_eq!(view.original.original.members.as_ptr(), cells);
                            assert_eq!(
                                view.original.original.original.candidates.as_ptr(),
                                candidates
                            );
                            let (serialized, released, members) = retained.finish(Tick(1)).unwrap();
                            assert_eq!(released, s);
                            assert_eq!(members, bytes);
                            assert_eq!(members.as_ptr(), output_ptr);
                            let (bound, fields, slots) = serialized.finish(Tick(1)).unwrap();
                            assert_eq!(fields, metadata(mask));
                            assert_eq!(slots.as_ptr(), cells);
                            let (_, mut pin) = bound.finish(Tick(1)).unwrap();
                            assert_eq!(pin.read_at(0, &mut [0; 2]).unwrap(), 2);
                        },
                    );
                    assert_eq!(output[bytes.len()], 0xa5);
                }
            }
        }
    }
}
#[test]
fn root_only_empty_metadata_has_literal_whole_json() {
    let clock = TestClock::new();
    let mut output = [0xa5; 128];
    with_collection(SOURCE, 0, 0, &clock, |serialized| {
        let mut cursor =
            Cursor::new(serialized, selection(31, false), &mut output, Tick(1)).unwrap();
        drain(&mut cursor).unwrap();
        let expected = b"\"bodyStructure\":{},\"textBody\":[{}],\"htmlBody\":[{}],\"attachments\":[{}],\"hasAttachment\":true";
        assert_eq!(
            cursor.finish(Tick(1)).unwrap().value().unwrap().members,
            expected
        );
    });
}
#[test]
fn exact_capacity_completes_and_short_windows_never_expose_whole() {
    for sub_parts in [false, true] {
        let s = selection(31, sub_parts);
        let bytes = streamed(SOURCE, 513, s);
        for size in [0, 1, 63, bytes.len() - 1, bytes.len(), bytes.len() + 1] {
            let mut output = vec![0xa5; size];
            let clock = TestClock::new();
            with_collection(SOURCE, 0, 513, &clock, |serialized| {
                let mut cursor = Cursor::new(serialized, s, &mut output, Tick(1)).unwrap();
                let result = drain(&mut cursor);
                if size < bytes.len() {
                    let error = Error::Original(OriginalError::ResponseCapacity);
                    assert_eq!(result, Err(error));
                    assert!(cursor.value().is_none());
                    assert_eq!(cursor.poll(Tick(1)), Err(error));
                    assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
                } else {
                    result.unwrap();
                    assert_eq!(cursor.value().unwrap().members, bytes);
                    let (original, released, members) =
                        cursor.finish(Tick(1)).unwrap().finish(Tick(1)).unwrap();
                    assert_eq!(released, s);
                    assert_eq!(members, bytes);
                    assert_eq!(original.value().unwrap().properties, metadata(513));
                }
            });
            if size > bytes.len() {
                assert_eq!(output[bytes.len()], 0xa5);
            }
        }
    }
}
#[test]
fn fresh_construction_and_unfinished_poll_precede_window_capacity() {
    for bits in [0, 31] {
        for actual in [false, true] {
            for sub_parts in [false, true] {
                let clock = TestClock::new();
                with_collection(SIMPLE, 0, 0, &clock, |serialized| {
                    let mut output = [];
                    let tick = if actual {
                        clock.expire();
                        Tick(1)
                    } else {
                        Tick(100)
                    };
                    assert_eq!(
                        Cursor::new(serialized, selection(bits, sub_parts), &mut output, tick)
                            .err(),
                        Some(deadline(actual))
                    );
                });
                let clock = TestClock::new();
                with_collection(SIMPLE, 0, 0, &clock, |serialized| {
                    let mut output = [];
                    let mut cursor =
                        Cursor::new(serialized, selection(bits, sub_parts), &mut output, Tick(1))
                            .unwrap();
                    let before = cursor.costs();
                    let tick = if actual {
                        clock.expire();
                        Tick(1)
                    } else {
                        Tick(100)
                    };
                    if bits == 0 {
                        assert_eq!(cursor.poll(tick), Ok(Status::Complete));
                        assert_eq!(cursor.costs(), before);
                        assert!(cursor.value().is_some());
                        assert_eq!(cursor.finish(tick).err(), Some(deadline(actual)));
                    } else {
                        assert_eq!(cursor.poll(tick), Err(deadline(actual)));
                        assert_eq!(cursor.costs(), before);
                        assert!(cursor.value().is_none());
                        assert_eq!(cursor.finish(Tick(1)).err(), Some(deadline(actual)));
                    }
                });
            }
        }
    }
}
#[test]
fn exact_window_prefix_expiry_is_sticky_and_cannot_expose_whole() {
    let s = selection(31, false);
    let bytes = streamed(SOURCE, 513, s);
    for actual in [false, true] {
        let clock = TestClock::new();
        with_collection(SOURCE, 0, 513, &clock, |serialized| {
            let mut output = vec![0xa5; bytes.len()];
            let mut cursor = Cursor::new(serialized, s, &mut output, Tick(1)).unwrap();
            let initial = cursor.costs();
            let mut copied = false;
            for _ in 0..10000 {
                assert_eq!(cursor.poll(Tick(1)).unwrap(), Status::Yield);
                if cursor.costs()[4] < initial[4] {
                    copied = true;
                    break;
                }
            }
            assert!(copied);
            let before = cursor.costs();
            let tick = if actual {
                clock.expire();
                Tick(1)
            } else {
                Tick(100)
            };
            assert_eq!(cursor.poll(tick), Err(deadline(actual)));
            assert_eq!(cursor.costs(), before);
            assert!(cursor.value().is_none());
            assert_eq!(cursor.poll(Tick(1)), Err(deadline(actual)));
            assert_eq!(cursor.finish(Tick(1)).err(), Some(deadline(actual)));
        });
    }
}
#[test]
fn every_completed_owner_still_requires_fresh_consuming_release() {
    for bits in [0, 31] {
        for actual in [false, true] {
            for sub_parts in [false, true] {
                for phase in 0..4 {
                    let clock = TestClock::new();
                    with_collection(SIMPLE, 0, 513, &clock, |serialized| {
                        let mut output = [0; 512];
                        let mut cursor = Cursor::new(
                            serialized,
                            selection(bits, sub_parts),
                            &mut output,
                            Tick(1),
                        )
                        .unwrap();
                        drain(&mut cursor).unwrap();
                        if phase < 2 {
                            let tick = if actual {
                                clock.expire();
                                Tick(1)
                            } else {
                                Tick(100)
                            };
                            let before = cursor.costs();
                            assert_eq!(cursor.poll(tick), Ok(Status::Complete));
                            assert_eq!(cursor.costs(), before);
                            assert!(cursor.value().is_some());
                            if phase == 0 {
                                assert_eq!(cursor.finish(tick).err(), Some(deadline(actual)));
                            } else {
                                assert_eq!(cursor.check_deadline(tick), Err(deadline(actual)));
                                assert!(cursor.value().is_none());
                                assert_eq!(cursor.poll(Tick(1)), Err(deadline(actual)));
                            }
                        } else {
                            let mut retained = cursor.finish(Tick(1)).unwrap();
                            let tick = if actual {
                                clock.expire();
                                Tick(1)
                            } else {
                                Tick(100)
                            };
                            assert!(retained.value().is_some());
                            if phase == 2 {
                                assert_eq!(retained.finish(tick).err(), Some(deadline(actual)));
                            } else {
                                assert_eq!(retained.check_deadline(tick), Err(deadline(actual)));
                                assert!(retained.value().is_none());
                                assert_eq!(retained.finish(Tick(1)).err(), Some(deadline(actual)));
                            }
                        }
                    });
                }
            }
        }
    }
}
#[test]
fn outer_none_completes_in_empty_window_but_retains_all_labels() {
    for sub_parts in [false, true] {
        let clock = TestClock::new();
        with_collection(SOURCE, 0, 1023, &clock, |serialized| {
            let mut output = [];
            let before = serialized.inner.costs();
            let mut cursor =
                Cursor::new(serialized, selection(0, sub_parts), &mut output, Tick(1)).unwrap();
            assert_eq!(cursor.costs(), before);
            let view = cursor.value().unwrap();
            assert!(view.members.is_empty());
            assert_eq!(view.selection, selection(0, sub_parts));
            assert_eq!(view.original.properties, metadata(1023));
            assert_eq!(cursor.poll(Tick(100)), Ok(Status::Complete));
            assert_eq!(cursor.costs(), before);
            let retained = cursor.finish(Tick(1)).unwrap();
            assert_eq!(retained.value().unwrap().selection, selection(0, sub_parts));
            let (_, s, bytes) = retained.finish(Tick(1)).unwrap();
            assert_eq!(s, selection(0, sub_parts));
            assert!(bytes.is_empty());
        });
    }
}
#[test]
fn retained_poll_matches_streamed_progress_and_debits_without_extra_fee() {
    for sub_parts in [false, true] {
        let clock = TestClock::new();
        let s = selection(31, sub_parts);
        let trace = with_collection(SOURCE, 0, 513, &clock, |serialized| {
            let mut cursor =
                crate::mime_response::subparts_json::Cursor::new(serialized, s, Tick(1)).unwrap();
            let mut trace = Vec::new();
            let mut complete = false;
            for _ in 0..10000 {
                let before = cursor.inner.costs();
                let p = cursor.poll(Tick(1), &mut [0; 64]).unwrap();
                let after = cursor.inner.costs();
                trace.push((p, std::array::from_fn::<_, 5, _>(|i| before[i] - after[i])));
                if p.status == crate::mime_response::subparts_json::Status::Complete {
                    complete = true;
                    break;
                }
            }
            assert!(complete);
            trace
        });
        with_collection(SOURCE, 0, 513, &clock, |serialized| {
            let mut output = [0; 2048];
            let mut cursor = Cursor::new(serialized, s, &mut output, Tick(1)).unwrap();
            for (p, debits) in trace {
                let before = cursor.costs();
                assert_eq!(
                    cursor.poll(Tick(1)).unwrap(),
                    if p.status == crate::mime_response::subparts_json::Status::Complete {
                        Status::Complete
                    } else {
                        Status::Yield
                    }
                );
                let after = cursor.costs();
                assert_eq!(
                    std::array::from_fn::<_, 5, _>(|i| before[i] - after[i]),
                    debits
                );
            }
        });
    }
}
/// Original storage and refusal preparation stay cold; expiry transitions count.
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
                let mut output = [0xa5; 2048];
                let capacity = if trial == 1 {
                    0
                } else if trial == 6 {
                    1
                } else {
                    output.len()
                };
                let s = selection(if trial == 1 { 0 } else { 31 }, trial == 2);
                if trial == 7 {
                    clock.expire();
                }
                snapshot();
                if trial == 7 {
                    assert_eq!(
                        Cursor::new(serialized, s, &mut output[..capacity], Tick(1)).err(),
                        Some(deadline(true))
                    );
                } else {
                    let mut cursor =
                        Cursor::new(serialized, s, &mut output[..capacity], Tick(1)).unwrap();
                    if trial == 3 {
                        let initial = cursor.costs();
                        let mut copied = false;
                        for _ in 0..10000 {
                            assert_eq!(cursor.poll(Tick(1)).unwrap(), Status::Yield);
                            if cursor.costs()[4] < initial[4] {
                                copied = true;
                                break;
                            }
                        }
                        assert!(copied);
                        clock.expire();
                        assert_eq!(cursor.poll(Tick(1)), Err(deadline(true)));
                        drop(cursor);
                    } else if trial == 6 {
                        assert_eq!(
                            drain(&mut cursor),
                            Err(Error::Original(OriginalError::ResponseCapacity))
                        );
                        assert!(cursor.value().is_none());
                        drop(cursor);
                    } else {
                        drain(&mut cursor).unwrap();
                        assert_eq!(cursor.value().unwrap().selection, s);
                        if trial == 4 {
                            clock.expire();
                            assert_eq!(cursor.finish(Tick(1)).err(), Some(deadline(true)));
                        } else {
                            let retained = cursor.finish(Tick(1)).unwrap();
                            assert_eq!(retained.value().unwrap().selection, s);
                            if trial == 5 {
                                clock.expire();
                                assert_eq!(retained.finish(Tick(1)).err(), Some(deadline(true)));
                            } else {
                                let (serialized, released, _) = retained.finish(Tick(1)).unwrap();
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

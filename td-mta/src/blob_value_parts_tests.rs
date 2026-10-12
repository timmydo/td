#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;
use crate::blob_body_lists::tests::{self as fixture, Lists, Storage, Timer};
use crate::{blob_part, nfc::Scratch};
fn exercise(
    bytes: &[u8],
    test: impl FnOnce(blob_body_lists::Retained<'_, '_, '_, '_, '_, '_, '_>, &[Fact], &Timer),
) {
    fixture::exercise(bytes, |retained, nodes, timer| {
        let mut retained = retained;
        let mut storage: [Storage; 8] = std::array::from_fn(|_| Storage::new());
        let mut scratch = Scratch::new();
        let mut scratch = &mut scratch;
        let mut facts = Vec::new();
        for (index, storage) in storage.iter_mut().take(nodes.len()).enumerate() {
            let mut part = blob_part::Cursor::new(
                retained,
                (index + 1) as u16,
                storage.backing(),
                scratch,
                Tick(1),
            )
            .unwrap();
            while part.poll(Tick(1)).unwrap() != mime_part_headers::Status::Complete {}
            let completed = part.finish(Tick(1)).unwrap();
            facts.push(Fact {
                end: End {
                    part: completed.metadata.part,
                    node: completed.metadata.node,
                },
                kind: Kind::from_headers(&completed.metadata.metadata.headers),
            });
            retained = completed.structure;
            scratch = completed.scratch;
        }
        let mut lists = Lists::new();
        let mut listing = blob_body_lists::Cursor::new(
            retained,
            nodes,
            fixture::limits(),
            lists.backing(),
            Tick(1),
        )
        .unwrap();
        while listing.poll(Tick(1)).unwrap() != crate::mime_body_lists::Status::Complete {}
        test(listing.finish(Tick(1)).unwrap(), &facts, timer);
    });
}
fn options(mask: u8) -> Options {
    Options {
        fetch_text: mask & 1 != 0,
        fetch_html: mask & 2 != 0,
        fetch_all: mask & 4 != 0,
        max_bytes: 7,
    }
}
fn drain(cursor: &mut Cursor<'_, '_, '_, '_, '_, '_, '_, '_, '_>) -> Result<(), Error> {
    for _ in 0..1000 {
        if cursor.poll(Tick(1))? == Status::Complete {
            return Ok(());
        }
    }
    panic!("selection stalled");
}
const SOURCE: &[u8] = b"Content-Type: multipart/mixed;boundary=x\r\n\r\n--x\r\nContent-Type: text/plain\r\n\r\np\r\n--x\r\nContent-Type: text/html\r\n\r\n<h>\r\n--x\r\nContent-Type: image/png\r\nContent-Disposition: inline\r\n\r\ni\r\n--x\r\nContent-Type: text/calendar\r\nContent-Disposition: attachment\r\n\r\nc\r\n--x--\r\n";
#[test]
fn flags_select_text_union_once_in_preorder_and_keep_original_owners() {
    exercise(SOURCE, |mut original, facts, _| {
        assert_eq!(
            facts.iter().map(|f| f.kind).collect::<Vec<_>>(),
            [Kind::Other, Kind::Text, Kind::Html, Kind::Other, Kind::Text]
        );
        let pointers = (
            std::ptr::from_mut(original.structure.source),
            std::ptr::from_mut(original.structure.work),
            std::ptr::from_mut(original.structure.budget),
            original.nodes.as_ptr(),
        );
        for mask in 0..8 {
            let mut output = [0; 5];
            let mut marks = [0xff; 5];
            let before = original.structure.work.remaining();
            let mut cursor = Cursor::new(
                original,
                facts,
                &mut output,
                &mut marks,
                options(mask),
                Tick(1),
            )
            .unwrap();
            assert!(cursor.value().is_none());
            drain(&mut cursor).unwrap();
            let expected: &[u16] = if mask & 4 != 0 {
                &[2, 3, 5]
            } else if mask & 3 != 0 {
                &[2, 3]
            } else {
                &[]
            };
            assert_eq!(cursor.value(), Some(expected));
            assert_eq!(cursor.poll(Tick(1)).unwrap(), Status::Complete);
            let retained = cursor.finish(Tick(1)).unwrap();
            assert_eq!(retained.options, options(mask));
            assert!(std::ptr::eq(retained.facts, facts));
            assert_eq!(retained.ordinals, expected);
            original = retained.original;
            let after = original.structure.work.remaining();
            assert_eq!(before.io_bytes, after.io_bytes);
            assert_eq!(
                before.output_bytes - after.output_bytes,
                (2 * expected.len()) as u64
            );
        }
        assert_eq!(
            pointers,
            (
                std::ptr::from_mut(original.structure.source),
                std::ptr::from_mut(original.structure.work),
                std::ptr::from_mut(original.structure.budget),
                original.nodes.as_ptr()
            )
        );
    });
}
#[test]
fn capacities_correspondence_and_original_quotas_refuse_stickily() {
    for mode in 0..5 {
        exercise(SOURCE, |original, facts, timer| {
            let mut changed = facts.to_vec();
            if mode == 0 {
                changed[3].end.part.ordinal = 1;
            }
            if mode == 1 {
                changed[0].kind = Kind::Text;
            }
            if mode == 3 || mode == 4 {
                let left = original.structure.work.remaining();
                original
                    .structure
                    .work
                    .charge(
                        Tick(1),
                        Charge {
                            records: if mode == 3 { left.records } else { 0 },
                            output_bytes: if mode == 4 { left.output_bytes } else { 0 },
                            ..Charge::default()
                        },
                    )
                    .unwrap();
            }
            let mut output = [0xa5a5; 5];
            let mut marks = [0xff; 5];
            let mut cursor = Cursor::new(
                original,
                &changed,
                &mut output[..if mode == 2 { 0 } else { 5 }],
                &mut marks,
                options(7),
                Tick(1),
            )
            .unwrap();
            let error = drain(&mut cursor).unwrap_err();
            if mode == 2 {
                assert_eq!(error, bound::Error::ResponseCapacity.into());
            }
            timer.after(0);
            assert!(cursor.value().is_none());
            assert_eq!(cursor.poll(Tick(1)), Err(error));
            assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
            assert_eq!(output, [0xa5a5; 5]);
        });
    }
}
#[test]
fn native_and_job_expiry_withhold_active_and_cached_results() {
    for mode in 0..6 {
        exercise(SOURCE, |original, facts, timer| {
            let mut output = [0; 5];
            let mut marks = [0; 5];
            let mut cursor = Cursor::new(
                original,
                facts,
                &mut output,
                &mut marks,
                options(7),
                Tick(1),
            )
            .unwrap();
            if mode >= 2 {
                drain(&mut cursor).unwrap();
            }
            let tick = if mode % 2 == 0 {
                timer.after(if mode >= 4 { 3 } else { 5 });
                Tick(1)
            } else {
                Tick(100)
            };
            if mode >= 4 {
                let expected = if mode == 4 {
                    Error::Policy(crate::ports::Error::Deadline)
                } else {
                    Error::Interpretation(bound::Error::Admission(crate::nfc::Error::Work(
                        crate::admission::work::Stop::Deadline,
                    )))
                };
                assert_eq!(cursor.finish(tick).err(), Some(expected));
            } else {
                let error = cursor.poll(tick).unwrap_err();
                assert!(cursor.value().is_none());
                assert_eq!(cursor.poll(Tick(1)), Err(error));
                assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
            }
        });
    }
}

#[test]
fn alternatives_obey_independent_flags_and_exact_original_charges() {
    exercise(fixture::MIXED, |mut original, facts, _| {
        for mask in 0..8 {
            let mut output = vec![0; facts.len()];
            let mut marks = vec![0xff; facts.len()];
            let before = original.structure.work.remaining();
            let steps = original.structure.budget.steps_remaining();
            let source = original.structure.budget.source_bytes_remaining();
            let turns = (2 * facts.len()
                + 2
                + if mask & 1 != 0 {
                    original.lists.text.len()
                } else {
                    0
                }
                + if mask & 2 != 0 {
                    original.lists.html.len()
                } else {
                    0
                }) as u64;
            let mut cursor = Cursor::new(
                original,
                facts,
                &mut output,
                &mut marks,
                options(mask),
                Tick(1),
            )
            .unwrap();
            drain(&mut cursor).unwrap();
            let expected: &[u16] = if mask & 4 != 0 || mask & 3 == 3 {
                &[3, 4]
            } else if mask & 1 != 0 {
                &[3]
            } else if mask & 2 != 0 {
                &[4]
            } else {
                &[]
            };
            assert_eq!(cursor.value(), Some(expected));
            original = cursor.finish(Tick(1)).unwrap().original;
            let after = original.structure.work.remaining();
            assert_eq!(before.io_bytes, after.io_bytes);
            assert_eq!(original.structure.budget.source_bytes_remaining(), source);
            assert_eq!(steps - original.structure.budget.steps_remaining(), turns);
            assert_eq!(before.records - after.records, turns + turns.div_ceil(16));
            assert_eq!(
                before.output_bytes - after.output_bytes,
                (2 * expected.len()) as u64
            );
        }
    });
}

#[test]
fn late_native_expiry_hides_written_ordinals_and_overrides_job_refusal() {
    for mode in 0..3 {
        exercise(SOURCE, |original, facts, timer| {
            let mut output = [0xa5a5; 5];
            let mut marks = [0xff; 5];
            let error = Error::Policy(crate::ports::Error::Deadline);
            if mode == 2 {
                timer.after(3);
                assert_eq!(
                    Cursor::new(
                        original,
                        facts,
                        &mut output,
                        &mut marks,
                        options(7),
                        Tick(100)
                    )
                    .err(),
                    Some(error)
                );
                return;
            }
            let mut cursor = Cursor::new(
                original,
                facts,
                &mut output,
                &mut marks,
                options(7),
                Tick(1),
            )
            .unwrap();
            if mode == 0 {
                while !matches!(cursor.phase, Phase::Emit) || cursor.position != 1 {
                    cursor.poll(Tick(1)).unwrap();
                }
                timer.after(5);
                assert_eq!(cursor.poll(Tick(1)), Err(error));
                assert_eq!(cursor.retained.ordinals[0], 2);
            } else {
                timer.after(3);
                assert_eq!(cursor.poll(Tick(100)), Err(error));
            }
            assert!(cursor.value().is_none());
            assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
        });
    }
}

#[test]
fn short_membership_fact_count_and_unfinished_finish_refuse() {
    for mode in 0..4 {
        exercise(SOURCE, |original, facts, _| {
            let mut output = [0; 5];
            let mut marks = [0xff; 5];
            let result = Cursor::new(
                original,
                if mode == 0 { &facts[..4] } else { facts },
                &mut output,
                &mut marks[..if mode == 1 { 4 } else { 5 }],
                options(7),
                Tick(1),
            );
            if mode < 2 {
                assert_eq!(
                    result.err(),
                    Some(
                        if mode == 1 {
                            bound::Error::ResponseCapacity
                        } else {
                            bound::Error::InvalidState
                        }
                        .into()
                    )
                );
            } else {
                let mut cursor = result.unwrap();
                if mode == 3 {
                    cursor.poll(Tick(1)).unwrap();
                }
                assert_eq!(
                    cursor.finish(Tick(1)).err(),
                    Some(bound::Error::InvalidState.into())
                );
            }
        });
    }
}

#[test]
fn attached_messages_are_leaves_and_inconsistent_lists_and_nodes_refuse() {
    const MESSAGE: &[u8] = b"Content-Type: multipart/mixed;boundary=x\r\n\r\n--x\r\nContent-Type: message/rfc822\r\n\r\nContent-Type: text/plain\r\n\r\ninside\r\n--x\r\nContent-Type: text/plain\r\n\r\noutside\r\n--x--\r\n";
    exercise(MESSAGE, |original, facts, _| {
        assert_eq!(facts.len(), 3);
        assert_eq!(facts[1].kind, Kind::Other);
        let mut output = [0; 3];
        let mut marks = [0; 3];
        let mut cursor = Cursor::new(
            original,
            facts,
            &mut output,
            &mut marks,
            options(7),
            Tick(1),
        )
        .unwrap();
        drain(&mut cursor).unwrap();
        assert_eq!(cursor.value(), Some([3].as_slice()));
        cursor.finish(Tick(1)).unwrap();
    });
    for mode in 0..5 {
        exercise(SOURCE, |original, facts, _| {
            let mut changed = facts.to_vec();
            let mut parts = original.structure.parts.to_vec();
            if mode == 0 {
                changed[2].end.node.depth += 1;
            }
            if mode == 1 {
                parts[2].ordinal = 1;
            }
            let invalid = [if mode == 2 { 0 } else { u16::MAX }];
            let duplicate = [2, 2, 3];
            let lists = crate::mime_body_lists::View {
                text: if mode >= 4 {
                    &duplicate
                } else if mode >= 2 {
                    &invalid
                } else {
                    original.lists.text
                },
                ..original.lists
            };
            let original = blob_body_lists::Retained {
                structure: crate::blob_structure::Retained {
                    parts: &parts,
                    ..original.structure
                },
                lists,
                ..original
            };
            let mut output = [0; 5];
            let mut marks = [0; 5];
            let mut cursor = Cursor::new(
                original,
                &changed,
                &mut output,
                &mut marks,
                options(1),
                Tick(1),
            )
            .unwrap();
            if mode == 4 {
                drain(&mut cursor).unwrap();
                assert_eq!(cursor.value(), Some([2, 3].as_slice()));
                cursor.finish(Tick(1)).unwrap();
            } else {
                assert_eq!(drain(&mut cursor), Err(bound::Error::InvalidState.into()));
                assert!(cursor.value().is_none());
            }
        });
    }
}
#[test]
fn kind_uses_interpreted_type_and_rejects_a_fabricated_empty_subtype() {
    let timer = crate::blob_part_json::tests::Timer::new();
    crate::blob_part_json::tests::exercise_part(
        1,
        b"Content-Type: text/\r\n\r\nx",
        &timer,
        |original| {
            let mut headers = original.metadata.metadata.headers;
            assert_eq!(Kind::from_headers(&headers), Kind::Text);
            headers.content_type = b"text/";
            assert_eq!(Kind::from_headers(&headers), Kind::Other);
            headers.content_type = b"TeXt/HtMl";
            assert_eq!(Kind::from_headers(&headers), Kind::Html);
            headers.content_type = b"TEXT/calendar";
            assert_eq!(Kind::from_headers(&headers), Kind::Text);
        },
    );
}

#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;
use crate::blob_value_parts::tests::exercise;
fn drain(cursor: &mut Cursor<'_, '_, '_, '_, '_, '_, '_>) -> Result<(), Error> {
    let turns = cursor.original.structure.parts.len()
        + cursor.original.lists.text.len()
        + cursor.original.lists.html.len()
        + 4;
    for _ in 0..turns {
        if cursor.poll(Tick(1))? == Status::Complete {
            return Ok(());
        }
    }
    panic!("preview selection stalled")
}
const CASES: &[(&[u8], Option<Choice>)] = &[

        (b"\r\nplain", Some(Choice { ordinal: 1, format: Format::Plain })),
        (b"Content-Type: text/html\r\n\r\n<b>html</b>", Some(Choice { ordinal: 1, format: Format::Html })),
        (b"Content-Type: image/png\r\n\r\nx", None),
        (b"Content-Type: text/plain\r\nContent-Disposition: attachment\r\n\r\nx", None),
        (b"Content-Type: multipart/alternative;boundary=x\r\n\r\n--x\r\nContent-Type: text/html\r\n\r\nhtml\r\n--x\r\nContent-Type: text/plain\r\n\r\nplain\r\n--x--\r\n", Some(Choice { ordinal: 3, format: Format::Plain })),
        (b"Content-Type: multipart/mixed;boundary=x\r\n\r\n--x\r\nContent-Type: image/png\r\n\r\nx\r\n--x\r\nContent-Type: text/html\r\n\r\nhtml\r\n--x\r\nContent-Type: text/plain\r\n\r\nplain\r\n--x--\r\n", Some(Choice { ordinal: 3, format: Format::Html })),
    (b"Content-Type: multipart/alternative;boundary=x\r\n\r\n--x\r\nContent-Type: multipart/mixed;boundary=y\r\n\r\n--y\r\nContent-Type: image/png\r\n\r\ni\r\n--y--\r\n--x\r\nContent-Type: text/html\r\n\r\nhtml\r\n--x--\r\n", Some(Choice { ordinal: 4, format: Format::Html })),
];
#[test]
fn literal_plain_html_alternatives_and_inline_media_keep_original_owners() {
    for &(source, choice) in CASES {
        exercise(source, |original, _, _| {
            let pointers = (
                std::ptr::from_mut(original.structure.source),
                std::ptr::from_mut(original.structure.work),
                std::ptr::from_mut(original.structure.budget),
                original.structure.parts.as_ptr(),
                original.structure.headers.as_ptr(),
                original.nodes.as_ptr(),
                original.lists.text.as_ptr(),
                original.lists.html.as_ptr(),
            );
            let expected_turns = original.structure.parts.len()
                + original.lists.text.len()
                + original.lists.html.len()
                + 3;
            let before = original.structure.work.remaining();
            let steps = original.structure.budget.steps_remaining();
            let mut cursor = Cursor::new(original, Tick(1)).unwrap();
            assert!(cursor.value().is_none());
            drain(&mut cursor).unwrap();
            assert_eq!(cursor.value(), Some(choice));
            assert_eq!(cursor.poll(Tick(1)).unwrap(), Status::Complete);
            let retained = cursor.finish(Tick(1)).unwrap();
            assert_eq!(retained.choice, choice);
            let original = retained.original;
            assert_eq!(
                before.records - original.structure.work.remaining().records,
                (expected_turns + expected_turns.div_ceil(16)) as u64
            );
            assert_eq!(
                steps - original.structure.budget.steps_remaining(),
                expected_turns as u64
            );
            assert_eq!(
                before.output_bytes,
                original.structure.work.remaining().output_bytes
            );
            assert_eq!(
                pointers,
                (
                    std::ptr::from_mut(original.structure.source),
                    std::ptr::from_mut(original.structure.work),
                    std::ptr::from_mut(original.structure.budget),
                    original.structure.parts.as_ptr(),
                    original.structure.headers.as_ptr(),
                    original.nodes.as_ptr(),
                    original.lists.text.as_ptr(),
                    original.lists.html.as_ptr(),
                )
            );
        });
    }
}
#[test]
fn fallback_and_bad_list_suffixes_cannot_hide_behind_an_earlier_choice() {
    for mode in 0..5 {
        exercise(b"Content-Type: text/html\r\n\r\nx", |mut original, _, _| {
            let invalid: &'static [u16] = if mode == 1 { &[1, 1] } else { &[1, 2] };
            if mode == 0 {
                original.lists.text = &[];
            }
            if mode == 1 || mode == 2 {
                original.lists.text = invalid;
            }
            if mode == 3 {
                original.lists.html = invalid;
            }
            if mode == 4 {
                original.lists.text = &[0];
            }
            let mut cursor = Cursor::new(original, Tick(1)).unwrap();
            if mode == 0 {
                drain(&mut cursor).unwrap();
                assert_eq!(
                    cursor.finish(Tick(1)).unwrap().choice,
                    Some(Choice {
                        ordinal: 1,
                        format: Format::Html
                    })
                );
            } else {
                let error = Error::Interpretation(bound::Error::InvalidState);
                assert_eq!(drain(&mut cursor), Err(error));
                assert!(cursor.value().is_none());
                assert_eq!(cursor.poll(Tick(1)), Err(error));
                assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
            }
        });
    }
}
#[test]
fn constructor_active_cached_finish_and_original_work_refusals_are_terminal() {
    for mode in 0..7 {
        exercise(b"\r\nx", |original, _, timer| {
            if mode == 0 {
                timer.after(0);
                assert_eq!(
                    Cursor::new(original, Tick(1)).err(),
                    Some(Error::Policy(crate::ports::Error::Deadline))
                );
                return;
            }
            if mode == 5 {
                let records = original.structure.work.remaining().records;
                original
                    .structure
                    .work
                    .charge(
                        Tick(1),
                        Charge {
                            records,
                            ..Charge::default()
                        },
                    )
                    .unwrap();
            }
            if mode == 6 {
                let steps = original.structure.budget.steps_remaining();
                original
                    .structure
                    .budget
                    .charge(
                        original.structure.work,
                        Tick(1),
                        0,
                        steps,
                        &mut Credit::new(),
                    )
                    .unwrap();
            }
            let mut cursor = Cursor::new(original, Tick(1)).unwrap();
            if mode == 3 || mode == 4 {
                drain(&mut cursor).unwrap();
            }
            let error = if mode == 6 {
                Error::Interpretation(bound::Error::Admission(
                    crate::nfc::Error::InterpretationLimit,
                ))
            } else if mode == 5 {
                Error::Interpretation(bound::Error::Admission(crate::nfc::Error::Work(
                    crate::admission::work::Stop::Records,
                )))
            } else {
                timer.after(if mode == 2 { 5 } else { 0 });
                Error::Policy(crate::ports::Error::Deadline)
            };
            if mode == 4 {
                assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
            } else {
                assert_eq!(cursor.poll(Tick(1)), Err(error));
                assert!(cursor.value().is_none());
                assert_eq!(cursor.poll(Tick(1)), Err(error));
                assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
            }
        });
    }
}

#[test]
fn container_descriptors_cannot_be_classified_as_display_leaves() {
    for media in [Media::Plain, Media::Html, Media::InlineMedia] {
        for descriptor in [
            mime_structure::Media::Multipart,
            mime_structure::Media::MessageRfc822,
        ] {
            exercise(b"Content-Type: image/png\r\n\r\nx", |original, _, _| {
                let mut parts = original.structure.parts.to_vec();
                parts[0].media = descriptor;
                let mut nodes = original.nodes.to_vec();
                nodes[0].class.media = media;
                let original = blob_body_lists::Retained {
                    structure: crate::blob_structure::Retained {
                        parts: &parts,
                        ..original.structure
                    },
                    nodes: &nodes,
                    lists: original.lists,
                };
                let mut cursor = Cursor::new(original, Tick(1)).unwrap();
                let error = Error::Interpretation(bound::Error::InvalidState);
                assert_eq!(drain(&mut cursor), Err(error));
                assert!(cursor.value().is_none());
                assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
            });
        }
    }
}

#[test]
fn malformed_handoff_shapes_correspondence_and_backward_lists_refuse() {
    for mode in 0..7 {
        exercise(crate::blob_value_parts::tests::SOURCE, |original, _, _| {
            let mut parts = original.structure.parts.to_vec();
            let mut nodes = original.nodes.to_vec();
            match mode {
                0 => parts.clear(),
                1 => nodes.clear(),
                2 => parts[1].ordinal = 3,
                3 => nodes[1].parent = 0,
                4 => nodes[1].depth = 0,
                5 => nodes[1].class.media = Media::Other,
                _ => {}
            }
            let mut lists = original.lists;
            if mode == 6 {
                lists.text = &[3, 2];
            }
            let original = blob_body_lists::Retained {
                structure: crate::blob_structure::Retained {
                    parts: &parts,
                    ..original.structure
                },
                nodes: &nodes,
                lists,
            };
            let result = Cursor::new(original, Tick(1));
            let error = Error::Interpretation(bound::Error::InvalidState);
            if mode < 2 {
                assert_eq!(result.err(), Some(error));
            } else {
                let mut cursor = result.unwrap();
                assert_eq!(drain(&mut cursor), Err(error));
                assert!(cursor.value().is_none());
                assert_eq!(cursor.poll(Tick(1)), Err(error));
                assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
            }
        });
    }
}

#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;
use crate::blob_body_lists::tests::{self as fixture, Lists, Storage, Timer};
use crate::mime_response::selected_metadata::Properties as PartProperties;
use crate::mime_response::{locators::Candidate, requested_body_json::Properties};
use crate::{
    blob_part, blob_part_json, blob_part_locators, blob_selected_metadata, mime_part_headers,
    nfc::Scratch,
};
pub(crate) fn exercise(
    bytes: &[u8],
    empty: bool,
    test: impl FnOnce(blob_body_lists::Retained<'_, '_, '_, '_, '_, '_, '_>, &[Fragment<'_>], &Timer),
) {
    fixture::exercise(bytes, |retained, nodes, timer| {
        let mut storage: [Storage; 8] = std::array::from_fn(|_| Storage::new());
        let mut scratch = Scratch::new();
        let mut scratch = &mut scratch;
        let mut candidates = vec![Candidate::default(); retained.parts.len()];
        let mut mapping =
            blob_part_locators::Cursor::new(retained, &mut candidates, Tick(1)).unwrap();
        while mapping.poll(Tick(1)).unwrap() != blob_part_locators::Status::Complete {}
        let mapped = mapping.finish(Tick(1)).unwrap();
        let mut retained = mapped.structure;
        let mut encoded = Vec::new();
        let mut ends = Vec::new();
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
            let mut json =
                blob_part_json::Cursor::new(part.finish(Tick(1)).unwrap(), Tick(1)).unwrap();
            let mut fragment = Vec::new();
            loop {
                let mut out = [0; 64];
                let p = json.poll(Tick(1), &mut out).unwrap();
                fragment.extend_from_slice(&out[..p.written]);
                if p.status == Status::Complete {
                    break;
                }
            }
            let completed = json.finish(Tick(1)).unwrap();
            let props = if empty {
                PartProperties::NONE
            } else {
                PartProperties {
                    part_id: true,
                    ..PartProperties::NONE
                }
            };
            let mut selected = blob_selected_metadata::Cursor::new(
                completed,
                &fragment,
                &mapped.candidates[index],
                props,
                Tick(1),
            )
            .unwrap();
            let mut members = Vec::new();
            loop {
                let mut out = [0; 64];
                let p = selected.poll(Tick(1), &mut out).unwrap();
                members.extend_from_slice(&out[..p.written]);
                if p.status == Status::Complete {
                    break;
                }
            }
            let completed = selected.finish(Tick(1)).unwrap().original;
            ends.push(completed.end);
            encoded.push(members);
            retained = completed.structure;
            scratch = completed.scratch;
        }
        let fragments: Vec<_> = ends
            .iter()
            .zip(&encoded)
            .map(|(end, members)| Fragment { end: *end, members })
            .collect();
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
        test(listing.finish(Tick(1)).unwrap(), &fragments, timer);
    });
}
pub(crate) fn drain(
    cursor: &mut Cursor<'_, '_, '_, '_, '_, '_, '_, '_, '_>,
    width: usize,
) -> Result<Vec<u8>, Error> {
    let mut bytes = Vec::new();
    for _ in 0..10000 {
        let mut out = [0xa5; 128];
        let p = cursor.poll(Tick(1), &mut out[..width])?;
        assert!(p.written <= 64);
        assert!(out[p.written..].iter().all(|&b| b == 0xa5));
        bytes.extend_from_slice(&out[..p.written]);
        if p.status == Status::Complete {
            return Ok(bytes);
        }
    }
    panic!("native body JSON did not finish");
}
fn selection(mask: u8, sub_parts: bool) -> Selection {
    Selection {
        properties: Properties {
            body_structure: mask & 1 != 0,
            text_body: mask & 2 != 0,
            html_body: mask & 4 != 0,
            attachments: mask & 8 != 0,
            has_attachment: mask & 16 != 0,
        },
        sub_parts,
    }
}
pub(crate) fn counters(
    original: &blob_body_lists::Retained<'_, '_, '_, '_, '_, '_, '_>,
) -> [u64; 5] {
    let w = original.structure.work.remaining();
    [
        original.structure.budget.source_bytes_remaining(),
        original.structure.budget.steps_remaining(),
        w.io_bytes,
        w.records,
        w.output_bytes,
    ]
}
#[test]
fn every_outer_selection_and_subparts_choice_has_literal_wire_and_owner_oracles() {
    for empty in [false, true] {
        exercise(fixture::MIXED, empty, |mut original, fragments, _| {
            let pointers = (
                std::ptr::from_mut(original.structure.source),
                std::ptr::from_mut(original.structure.work),
                std::ptr::from_mut(original.structure.budget),
                original.nodes.as_ptr(),
                original.lists.text.as_ptr(),
                original.lists.html.as_ptr(),
                original.lists.attachments.as_ptr(),
            );
            for sub_parts in [false, true] {
                for mask in 0..32 {
                    let selected = selection(mask, sub_parts);
                    let before = counters(&original);
                    let mut cursor = Cursor::new(original, fragments, selected, Tick(1)).unwrap();
                    assert_eq!(
                        cursor.poll(Tick(1), &mut []).unwrap().status,
                        Status::NeedOutput
                    );
                    let bytes =
                        drain(&mut cursor, [1, 7, 64, 128][usize::from(mask >> 1) % 4]).unwrap();
                    let leaf = |id: u16| {
                        if empty {
                            if sub_parts {
                                "{\"subParts\":null}".to_owned()
                            } else {
                                "{}".to_owned()
                            }
                        } else if sub_parts {
                            format!("{{\"partId\":\"{id}\",\"subParts\":null}}")
                        } else {
                            format!("{{\"partId\":\"{id}\"}}")
                        }
                    };
                    let tree = if !sub_parts {
                        if empty {
                            "{}".to_owned()
                        } else {
                            "{\"partId\":null}".to_owned()
                        }
                    } else {
                        let root = if empty { "" } else { "\"partId\":null," };
                        let alternative = if empty { "" } else { "\"partId\":null," };
                        format!(
                            "{{{root}\"subParts\":[{{{alternative}\"subParts\":[{},{}]}},{}]}}",
                            leaf(3),
                            leaf(4),
                            leaf(5)
                        )
                    };
                    let members = [
                        format!("\"bodyStructure\":{tree}"),
                        format!("\"textBody\":[{}]", leaf(3)),
                        format!("\"htmlBody\":[{}]", leaf(4)),
                        format!("\"attachments\":[{}]", leaf(5)),
                        "\"hasAttachment\":true".to_owned(),
                    ];
                    let expected = members
                        .iter()
                        .enumerate()
                        .filter_map(|(i, s)| (mask & (1 << i) != 0).then_some(s.as_str()))
                        .collect::<Vec<_>>()
                        .join(",");
                    assert_eq!(
                        bytes,
                        expected.as_bytes(),
                        "mask {mask} subparts {sub_parts} empty {empty}"
                    );
                    assert_eq!(cursor.value(), Some(selected));
                    let retained = cursor.finish(Tick(1)).unwrap();
                    assert!(std::ptr::eq(retained.fragments, fragments));
                    original = retained.original;
                    let after = counters(&original);
                    assert_eq!(before[0], after[0]);
                    assert_eq!(before[2], after[2]);
                    assert_eq!(before[4] - after[4], bytes.len() as u64);
                    if mask == 0 {
                        assert_eq!(before[1], after[1]);
                        assert_eq!(before[3] - after[3], fragments.len() as u64);
                    }
                }
            }
            assert_eq!(
                (
                    std::ptr::from_mut(original.structure.source),
                    std::ptr::from_mut(original.structure.work),
                    std::ptr::from_mut(original.structure.budget),
                    original.nodes.as_ptr(),
                    original.lists.text.as_ptr(),
                    original.lists.html.as_ptr(),
                    original.lists.attachments.as_ptr()
                ),
                pointers
            );
        });
    }
}
#[test]
fn correspondence_counts_later_mismatches_and_unfinished_finish_refuse() {
    for mode in 0..5 {
        exercise(fixture::MIXED, false, |original, fragments, _| {
            let mut changed = fragments.to_vec();
            match mode {
                0 => {
                    changed.pop();
                }
                1 => changed.push(fragments[0]),
                2 => changed[3].end.part.ordinal = 1,
                3 => changed[3].end.node.depth = 0,
                _ => {}
            }
            let result = Cursor::new(original, &changed, selection(31, true), Tick(1));
            let error = Error::Interpretation(bound::Error::InvalidState);
            if mode < 2 {
                assert_eq!(result.err(), Some(error));
                return;
            }
            let mut cursor = result.unwrap();
            for _ in 0..3 {
                assert_eq!(
                    cursor.poll(Tick(1), &mut [0; 64]).unwrap().status,
                    Status::Yield
                );
            }
            if mode == 4 {
                assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
            } else {
                assert_eq!(cursor.poll(Tick(1), &mut [0; 64]), Err(error));
                assert!(cursor.value().is_none());
                assert_eq!(cursor.check_deadline(Tick(1)), Err(error));
            }
        });
    }
}
#[test]
fn native_and_job_expiry_retire_active_and_completed_composition() {
    for native in [false, true] {
        for mode in 0..5 {
            exercise(b"\ntext", false, |original, fragments, timer| {
                let error = if native {
                    Error::Policy(crate::ports::Error::Deadline)
                } else {
                    Error::Interpretation(bound::Error::Admission(crate::nfc::Error::Work(
                        crate::admission::work::Stop::Deadline,
                    )))
                };
                let expired = if native { Tick(1) } else { Tick(100) };
                if mode == 0 {
                    if native {
                        timer.after(3);
                    }
                    assert_eq!(
                        Cursor::new(original, fragments, selection(31, true), expired).err(),
                        Some(error)
                    );
                    return;
                }
                let mut cursor =
                    Cursor::new(original, fragments, selection(31, true), Tick(1)).unwrap();
                if mode >= 2 {
                    drain(&mut cursor, 64).unwrap();
                }
                if native {
                    timer.after(if mode == 4 { 3 } else { 5 });
                }
                if mode == 3 {
                    assert!(matches!(cursor.finish(expired),Err(found)if found==error));
                } else {
                    let result = if mode == 4 {
                        cursor.check_deadline(expired)
                    } else {
                        cursor.poll(expired, &mut [0; 64]).map(|_| ())
                    };
                    assert_eq!(result, Err(error));
                    assert!(cursor.value().is_none());
                    assert_eq!(cursor.check_deadline(Tick(1)), Err(error));
                    if native && mode == 1 {
                        assert_eq!(cursor.checked, 1);
                    }
                }
            });
        }
    }
}

#[test]
fn original_allowances_and_post_write_expiry_withhold_completion() {
    for mode in 0..4 {
        exercise(b"\ntext", false, |original, fragments, timer| {
            if mode == 0 {
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
            } else if mode == 1 {
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
            } else if mode == 2 {
                let output_bytes = original.structure.work.remaining().output_bytes;
                original
                    .structure
                    .work
                    .charge(
                        Tick(1),
                        Charge {
                            output_bytes,
                            ..Charge::default()
                        },
                    )
                    .unwrap();
            }
            let mut cursor =
                Cursor::new(original, fragments, selection(31, true), Tick(1)).unwrap();
            if mode == 3 {
                for _ in 0..=fragments.len() {
                    assert_eq!(
                        cursor.poll(Tick(1), &mut [0; 64]).unwrap().status,
                        Status::Yield
                    );
                }
                timer.after(5);
                let mut output = [0xa5; 64];
                let error = Error::Policy(crate::ports::Error::Deadline);
                assert_eq!(cursor.poll(Tick(1), &mut output), Err(error));
                assert_ne!(output, [0xa5; 64]);
                assert!(cursor.value().is_none());
                assert_eq!(cursor.poll(Tick(1), &mut [0; 64]), Err(error));
            } else {
                let refusal = match mode {
                    0 => crate::nfc::Error::Work(crate::admission::work::Stop::Records),
                    1 => crate::nfc::Error::InterpretationLimit,
                    _ => crate::nfc::Error::Work(crate::admission::work::Stop::OutputBytes),
                };
                let error = Error::Interpretation(bound::Error::Admission(refusal));
                assert_eq!(drain(&mut cursor, 64), Err(error));
                assert!(cursor.value().is_none());
                assert_eq!(cursor.check_deadline(Tick(100)), Err(error));
            }
        });
    }
    exercise(fixture::MIXED, false, |original, fragments, timer| {
        let mut changed = fragments.to_vec();
        changed[3].end.part.ordinal = 1;
        let mut cursor = Cursor::new(original, &changed, selection(31, true), Tick(1)).unwrap();
        for _ in 0..3 {
            cursor.poll(Tick(1), &mut [0; 64]).unwrap();
        }
        timer.after(5);
        assert_eq!(
            cursor.poll(Tick(1), &mut [0; 64]),
            Err(Error::Policy(crate::ports::Error::Deadline))
        );
        assert_eq!(cursor.checked, 3);
        assert!(cursor.value().is_none());
    });
}

#[test]
fn empty_original_short_nodes_and_invalid_list_ordinals_refuse() {
    for mode in 0..3 {
        exercise(fixture::MIXED, false, |mut original, fragments, _| {
            let invalid = [u16::MAX];
            if mode == 0 {
                original.structure.parts = &[];
                original.nodes = &[];
            } else if mode == 1 {
                original.nodes = &original.nodes[..4];
            }
            let lists = crate::mime_body_lists::View {
                text: if mode == 2 {
                    &invalid
                } else {
                    original.lists.text
                },
                ..original.lists
            };
            let original = blob_body_lists::Retained { lists, ..original };
            let error = Error::Interpretation(bound::Error::InvalidState);
            let result = Cursor::new(
                original,
                if mode == 0 { &[] } else { fragments },
                selection(2, false),
                Tick(1),
            );
            if mode < 2 {
                assert_eq!(result.err(), Some(error));
            } else {
                let mut cursor = result.unwrap();
                assert!(drain(&mut cursor, 1).is_err());
                assert!(cursor.value().is_none());
                assert!(cursor.finish(Tick(1)).is_err());
            }
        });
    }
}

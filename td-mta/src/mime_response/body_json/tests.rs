#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use crate::admission::work::Stop;
use crate::mime_response::body_json::*;
use {
    crate::mime_response::lists::tests::ALTERNATIVE, crate::mime_response::ordered::tests::SOURCE,
};
use {
    crate::mime_response::part_collection::tests::drain as retain,
    crate::mime_response::part_collection::tests::exercise as collection,
};
const DISTINCT: &[u8] = b"Content-Type: multipart/alternative; boundary=x\r\n\r\n--x\r\nContent-Type: text/plain\r\n\r\nplain\r\n--x\r\nContent-Type: text/html\r\n\r\n<p>html</p>\r\n--x--\r\n";
const MIXED: &[u8] = b"Content-Type: multipart/mixed;boundary=m\r\n\r\n--m\r\nContent-Type: multipart/alternative;boundary=a\r\n\r\n--a\r\nContent-Type: text/plain\r\n\r\nplain\r\n--a\r\nContent-Type: text/html\r\n\r\nhtml\r\n--a--\r\n--m\r\nContent-Type: application/octet-stream\r\nContent-Disposition: attachment\r\n\r\nfile\r\n--m--\r\n";
const EMPTY: &[u8] = b"Content-Type: multipart/mixed; boundary=x\r\n\r\n--x--\r\n";
fn exercise<T>(
    source: &[u8],
    base: u64,
    mode: Mode,
    f: impl FnOnce(Cursor<'_, '_, '_, '_, '_>, Vec<Vec<u8>>) -> T,
) -> T {
    collection(source, base, |mut owner, storage, scratch| {
        for _ in 0..owner.total().unwrap() {
            let mut child = owner.next(storage.backing(256), scratch, Tick(1)).unwrap();
            retain(&mut child);
            child.finish(Tick(1)).unwrap();
        }
        let serialized = owner.finish(Tick(1)).unwrap();
        let fragments = serialized
            .value()
            .unwrap()
            .fragments
            .iter()
            .map(|cell| cell.value().unwrap().fragment.to_vec())
            .collect();
        f(Cursor::new(serialized, mode, Tick(1)).unwrap(), fragments)
    })
}
fn drain(cursor: &mut Cursor<'_, '_, '_, '_, '_>, width: usize) -> Result<(Vec<u8>, usize), Error> {
    let mut bytes = Vec::new();
    for turn in 1..100000 {
        let mut output = [0xa5; 128];
        let before = costs(cursor);
        let progress = cursor.poll(Tick(1), &mut output[..width])?;
        let after = costs(cursor);
        assert!(progress.written <= 64);
        assert!(progress.written <= width);
        assert!(output[progress.written..].iter().all(|b| *b == 0xa5));
        assert_eq!(before[0] - after[0], 0);
        assert_eq!(before[2] - after[2], 0);
        assert!(before[1] - after[1] <= 1);
        assert!(before[3] - after[3] <= 1);
        assert_eq!(before[4] - after[4], progress.written as u64);
        bytes.extend_from_slice(&output[..progress.written]);
        if progress.status == Status::Complete {
            return Ok((bytes, turn));
        }
    }
    panic!("composer did not complete")
}
fn costs(cursor: &Cursor<'_, '_, '_, '_, '_>) -> [u64; 5] {
    let structure = &cursor.source.projected.structure;
    let left = structure.work.remaining();
    [
        structure.budget.source_bytes_remaining(),
        structure.budget.steps_remaining(),
        left.io_bytes,
        left.records,
        left.output_bytes,
    ]
}
fn leaf(fragment: &[u8]) -> String {
    format!(
        "{{{},\"subParts\":null}}",
        std::str::from_utf8(fragment).unwrap()
    )
}
fn expected(source: &[u8], mode: Mode, fragments: &[Vec<u8>]) -> String {
    if mode == Mode::Structure {
        if source == MIXED {
            return format!(
                "\"bodyStructure\":{{{},\"subParts\":[{{{},\"subParts\":[{},{}]}},{}]}}",
                std::str::from_utf8(&fragments[0]).unwrap(),
                std::str::from_utf8(&fragments[1]).unwrap(),
                leaf(&fragments[2]),
                leaf(&fragments[3]),
                leaf(&fragments[4])
            );
        }
        if source == SOURCE || source == DISTINCT {
            return format!(
                "\"bodyStructure\":{{{},\"subParts\":[{},{}]}}",
                std::str::from_utf8(&fragments[0]).unwrap(),
                leaf(&fragments[1]),
                leaf(&fragments[2])
            );
        }
        if source == ALTERNATIVE {
            return format!(
                "\"bodyStructure\":{{{},\"subParts\":[{{{},\"subParts\":[{},{}]}}]}}",
                std::str::from_utf8(&fragments[0]).unwrap(),
                std::str::from_utf8(&fragments[1]).unwrap(),
                leaf(&fragments[2]),
                leaf(&fragments[3])
            );
        }
        return format!("\"bodyStructure\":{}", leaf(&fragments[0]));
    }
    let (text, html, attachments, has) = if source == SOURCE {
        (vec![3], vec![3], vec![2], true)
    } else if source == ALTERNATIVE {
        (vec![3, 4], vec![3, 4], vec![], false)
    } else if source == MIXED {
        (vec![3], vec![4], vec![5], true)
    } else if source == DISTINCT {
        (vec![2], vec![3], vec![], false)
    } else {
        (vec![1], vec![1], vec![], false)
    };
    let list = |ordinals: &[usize]| {
        ordinals
            .iter()
            .map(|i| leaf(&fragments[i - 1]))
            .collect::<Vec<_>>()
            .join(",")
    };
    format!(
        "\"textBody\":[{}],\"htmlBody\":[{}],\"attachments\":[{}],\"hasAttachment\":{}",
        list(&text),
        list(&html),
        list(&attachments),
        has
    )
}
#[test]
fn literal_tree_and_lists_preserve_original_metadata_selection_and_wire_costs() {
    for source in [SOURCE, ALTERNATIVE, DISTINCT, MIXED, b"\r\nabc".as_slice()] {
        for base in [0, 17, u64::MAX - source.len() as u64] {
            for mode in [Mode::Structure, Mode::Lists] {
                for width in [1, 2, 7, 64, 128] {
                    exercise(source, base, mode, |mut cursor, fragments| {
                        let pointers = (
                            std::ptr::from_mut(cursor.source.projected.structure.work),
                            std::ptr::from_mut(cursor.source.projected.structure.budget),
                        );
                        let before = costs(&cursor);
                        let (bytes, _) = drain(&mut cursor, width).unwrap();
                        assert_eq!(bytes, expected(source, mode, &fragments).as_bytes());
                        let after = costs(&cursor);
                        assert_eq!(before[4] - after[4], bytes.len() as u64);
                        assert_eq!(before[0], after[0]);
                        assert_eq!(before[2], after[2]);
                        assert_eq!(
                            cursor.poll(Tick(100), &mut []),
                            Ok(Progress {
                                written: 0,
                                status: Status::Complete
                            })
                        );
                        assert_eq!(costs(&cursor), after);
                        let composed = cursor.finish(Tick(1)).unwrap();
                        assert_eq!(composed.value().unwrap().0, mode);
                        let ((actual, view), work, budget) = composed.finish(Tick(1)).unwrap();
                        assert_eq!(actual, mode);
                        assert_eq!(view.fragments.len(), fragments.len());
                        assert_eq!(
                            (std::ptr::from_mut(work), std::ptr::from_mut(budget)),
                            pointers
                        );
                    });
                }
            }
        }
    }
}
#[test]
fn exact_final_byte_reports_complete_without_another_output_window() {
    for source in [SOURCE, ALTERNATIVE, DISTINCT, MIXED, b"\r\nabc".as_slice()] {
        for mode in [Mode::Structure, Mode::Lists] {
            exercise(source, 0, mode, |mut cursor, fragments| {
                let expected = expected(source, mode, &fragments);
                let mut output = vec![0xa5; expected.len()];
                let mut used = 0;
                let mut done = false;
                for _ in 0..100000 {
                    assert!(used < output.len());
                    let progress = cursor.poll(Tick(1), &mut output[used..]).unwrap();
                    used += progress.written;
                    if progress.status == Status::Complete {
                        done = true;
                        break;
                    }
                }
                assert!(done);
                assert_eq!(used, output.len());
                assert_eq!(output, expected.as_bytes());
                cursor.finish(Tick(1)).unwrap();
            });
        }
    }
}
#[test]
fn every_composition_prefix_has_fresh_final_admission_and_empty_output_is_inert() {
    for mode in [Mode::Structure, Mode::Lists] {
        exercise(SOURCE, 0, mode, |mut cursor, _| {
            let error = Error::Admission(crate::nfc::Error::Work(Stop::Deadline));
            assert_eq!(cursor.poll(Tick(100), &mut []).err(), Some(error));
            assert!(cursor.value().is_none());
            assert_eq!(cursor.poll(Tick(1), &mut [0]).err(), Some(error));
            assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
        });
        let turns = exercise(SOURCE, 0, mode, |mut cursor, _| {
            drain(&mut cursor, 1).unwrap().1
        });
        for prefix in 0..=turns {
            for fault in 0..3 {
                exercise(SOURCE, 17, mode, |mut cursor, _| {
                    for _ in 0..prefix {
                        cursor.poll(Tick(1), &mut [0]).unwrap();
                    }
                    let before = costs(&cursor);
                    if prefix != turns {
                        assert_eq!(
                            cursor.poll(Tick(1), &mut []),
                            Ok(Progress {
                                written: 0,
                                status: Status::NeedOutput
                            })
                        );
                        assert_eq!(costs(&cursor), before);
                    }
                    if fault == 0 {
                        assert_eq!(
                            cursor.finish(Tick(100)).err(),
                            Some(Error::Admission(crate::nfc::Error::Work(Stop::Deadline)))
                        );
                        return;
                    }
                    if fault == 1 {
                        let error = cursor.check_deadline(Tick(100)).unwrap_err();
                        assert!(cursor.value().is_none());
                        assert_eq!(cursor.poll(Tick(1), &mut [0]).err(), Some(error));
                        assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
                        return;
                    }
                    if prefix == turns {
                        cursor.finish(Tick(1)).unwrap();
                    } else {
                        assert_eq!(cursor.finish(Tick(1)).err(), Some(Error::InvalidState));
                    }
                });
            }
        }
    }
}
#[test]
fn emission_and_completed_owners_both_require_fresh_whole_release() {
    for mode in [Mode::Structure, Mode::Lists] {
        for fault in 0..2 {
            exercise(SOURCE, 0, mode, |mut cursor, _| {
                drain(&mut cursor, 64).unwrap();
                let mut composed = cursor.finish(Tick(1)).unwrap();
                let error = Error::Admission(crate::nfc::Error::Work(Stop::Deadline));
                if fault == 0 {
                    assert_eq!(composed.finish(Tick(100)).err(), Some(error));
                } else {
                    assert_eq!(composed.check_deadline(Tick(100)), Err(error));
                    assert!(composed.value().is_none());
                    assert_eq!(composed.finish(Tick(1)).err(), Some(error));
                }
            });
        }
    }
}

#[test]
fn original_step_record_and_wire_cutoffs_refuse_stickily_without_source_io() {
    for mode in [Mode::Structure, Mode::Lists] {
        let used = exercise(SOURCE, 0, mode, |mut cursor, _| {
            let before = costs(&cursor);
            drain(&mut cursor, 64).unwrap();
            let after = costs(&cursor);
            std::array::from_fn::<_, 5, _>(|i| before[i] - after[i])
        });
        assert_eq!(used[0], 0);
        assert_eq!(used[2], 0);
        for (kind, &used_value) in used.iter().enumerate() {
            for cut in 0..=used_value {
                exercise(SOURCE, 0, mode, |mut cursor, _| {
                    let structure = &mut cursor.source.projected.structure;
                    if kind < 2 {
                        let left = if kind == 0 {
                            structure.budget.source_bytes_remaining()
                        } else {
                            structure.budget.steps_remaining()
                        };
                        structure
                            .budget
                            .charge(
                                structure.work,
                                Tick(1),
                                if kind == 0 { left - cut } else { 0 },
                                if kind == 1 { left - cut } else { 0 },
                                &mut crate::nfc::Credit::new(),
                            )
                            .unwrap();
                    } else {
                        let left = structure.work.remaining();
                        structure
                            .work
                            .charge(
                                Tick(1),
                                Charge {
                                    io_bytes: if kind == 2 { left.io_bytes - cut } else { 0 },
                                    records: if kind == 3 { left.records - cut } else { 0 },
                                    output_bytes: if kind == 4 {
                                        left.output_bytes - cut
                                    } else {
                                        0
                                    },
                                    ..Default::default()
                                },
                            )
                            .unwrap();
                    }
                    let result = drain(&mut cursor, 64);
                    if cut == used_value {
                        result.unwrap();
                        cursor.finish(Tick(1)).unwrap();
                        return;
                    }
                    let error = result.unwrap_err();
                    assert!(matches!(error, Error::Admission(_)));
                    assert!(cursor.value().is_none());
                    let before = costs(&cursor);
                    assert_eq!(cursor.poll(Tick(100), &mut []).err(), Some(error));
                    assert_eq!(cursor.check_deadline(Tick(1)), Err(error));
                    assert_eq!(costs(&cursor), before);
                    assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
                });
            }
        }
    }
}
#[test]
fn constructor_requires_fresh_original_whole_collection() {
    for inherited in [false, true] {
        collection(SOURCE, 0, |mut owner, storage, scratch| {
            for _ in 0..owner.total().unwrap() {
                let mut child = owner.next(storage.backing(256), scratch, Tick(1)).unwrap();
                retain(&mut child);
                child.finish(Tick(1)).unwrap();
            }
            let mut source = owner.finish(Tick(1)).unwrap();
            let error = Error::Admission(crate::nfc::Error::Work(Stop::Deadline));
            if inherited {
                assert_eq!(source.check_deadline(Tick(100)), Err(error));
                assert!(source.value().is_none());
            }
            assert_eq!(
                Cursor::new(
                    source,
                    Mode::Lists,
                    if inherited { Tick(1) } else { Tick(100) }
                )
                .err(),
                Some(error)
            );
        });
    }
}

#[test]
fn maximum_original_depth_closes_each_container_without_recursion() {
    use {
        crate::header_select::SourceEnd, crate::limits::Limits,
        crate::mime_body_lists::Backing as ListBacking, crate::mime_body_lists::Node,
        crate::mime_body_lists::Status as ListStatus,
        crate::mime_response::bound::Cursor as Traversal, crate::mime_response::lists::Selecting,
        crate::mime_response::ordered::tests::meter, crate::mime_response::ordered::tests::Storage,
        crate::mime_response::ordered::Classifying, crate::mime_structure::Part as Descriptor,
        crate::mime_structure::Status as TraversalStatus, crate::nfc::HeaderBudget,
        crate::nfc::Scratch,
    };
    let mut source = String::new();
    for depth in 0..63 {
        source.push_str(&format!(
            "Content-Type: multipart/mixed;boundary=b{depth:02}\r\n\r\n--b{depth:02}\r\n"
        ));
    }
    source.push_str("Content-Type: text/plain\r\n\r\nx");
    for depth in (0..63).rev() {
        source.push_str(&format!("\r\n--b{depth:02}--\r\n"));
    }
    let limits = Limits {
        mime_depth: 64,
        mime_parts: 4096,
        ..Limits::default()
    };
    let mut parts = [Descriptor::default(); 64];
    let mut nodes = [Node::default(); 64];
    let mut work = meter();
    let mut budget = HeaderBudget::new();
    let mut storage = Storage::new();
    let mut scratch = Scratch::new();
    let mut traversal = Traversal::new(
        source.as_bytes(),
        17,
        SourceEnd::Eof,
        &limits,
        &mut parts,
        &mut work,
        &mut budget,
    )
    .unwrap();
    let mut done = false;
    for _ in 0..100000 {
        if traversal.poll(Tick(1)).unwrap() == TraversalStatus::Complete {
            done = true;
            break;
        }
    }
    assert!(done);
    let mut classifying = Classifying::new(traversal.finish(Tick(1)).unwrap(), &mut nodes).unwrap();
    assert_eq!(classifying.total().unwrap(), 64);
    for _ in 0..64 {
        let mut child = classifying
            .next(storage.backing(256), &mut scratch)
            .unwrap();
        done = false;
        for _ in 0..100000 {
            if child.poll(Tick(1)).unwrap() == TraversalStatus::Complete {
                done = true;
                break;
            }
        }
        assert!(done);
        child.finish(Tick(1)).unwrap();
    }
    let mut text = [0; 64];
    let mut html = [0; 64];
    let mut attachments = [0; 64];
    let mut membership = [0; 64];
    let mut selecting = Selecting::new(
        classifying.finish(Tick(1)).unwrap(),
        &limits,
        ListBacking {
            text: &mut text,
            html: &mut html,
            attachments: &mut attachments,
            membership: &mut membership,
        },
        Tick(1),
    )
    .unwrap();
    done = false;
    for _ in 0..100000 {
        if selecting.poll(Tick(1)).unwrap() == ListStatus::Complete {
            done = true;
            break;
        }
    }
    assert!(done);
    let mut output = [[0; 512]; 64];
    let mut cells = output
        .each_mut()
        .map(|bytes| crate::mime_response::part_collection::Cell::new(bytes));
    let mut collecting = crate::mime_response::part_collection::Collecting::new(
        selecting.finish(Tick(1)).unwrap(),
        &mut cells,
        Tick(1),
    )
    .unwrap();
    for _ in 0..64 {
        let mut child = collecting
            .next(storage.backing(256), &mut scratch, Tick(1))
            .unwrap();
        retain(&mut child);
        child.finish(Tick(1)).unwrap();
    }
    let serialized = collecting.finish(Tick(1)).unwrap();
    let view = serialized.value().unwrap();
    let mut expected = String::from("\"bodyStructure\":");
    for (index, cell) in view.fragments.iter().enumerate() {
        expected.push('{');
        expected.push_str(std::str::from_utf8(cell.value().unwrap().fragment).unwrap());
        if index == 63 {
            expected.push_str(",\"subParts\":null}");
        } else {
            expected.push_str(",\"subParts\":[");
        }
    }
    for _ in 0..63 {
        expected.push_str("]}");
    }
    let mut cursor = Cursor::new(serialized, Mode::Structure, Tick(1)).unwrap();
    let (bytes, _) = drain(&mut cursor, 128).unwrap();
    assert_eq!(bytes, expected.as_bytes());
    cursor.finish(Tick(1)).unwrap();
}

#[test]
fn childless_multipart_refuses_before_original_collection_can_exist() {
    use {
        crate::header_select::SourceEnd, crate::limits::Limits,
        crate::mime_response::bound::Cursor as Traversal,
        crate::mime_response::bound::Error as BoundError, crate::mime_structure::Part,
        crate::mime_structure::Status as TraversalStatus, crate::nfc::HeaderBudget,
    };
    let mut parts = [Part::default(); 4];
    let mut work = crate::mime_response::ordered::tests::meter();
    let mut budget = HeaderBudget::new();
    let mut cursor = Traversal::new(
        EMPTY,
        0,
        SourceEnd::Eof,
        &Limits::default(),
        &mut parts,
        &mut work,
        &mut budget,
    )
    .unwrap();
    let expected = BoundError::Traversal(crate::mime_structure::Error::NotParsable);
    let mut first = None;
    for _ in 0..100000 {
        match cursor.poll(Tick(1)) {
            Err(error) => {
                first = Some(error);
                break;
            }
            Ok(TraversalStatus::Complete) => panic!("childless multipart accepted"),
            Ok(_) => {}
        }
    }
    assert_eq!(first, Some(expected));
    assert_eq!(cursor.poll(Tick(1)), Err(expected));
    assert_eq!(cursor.finish(Tick(1)).err(), Some(expected));
}

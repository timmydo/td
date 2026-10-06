#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use crate::mime_response::part_json::*;
use {
    crate::admission::work::Stop, crate::mime_response::ordered::tests::meter,
    crate::mime_response::ordered::tests::Storage, crate::mime_response::ordered::tests::SOURCE,
};
use {crate::mime_response::lists::tests::Lists, crate::mime_response::lists::tests::ALTERNATIVE};
use {
    crate::mime_response::response::tests::selection, crate::mime_response::response::Projecting,
};
fn forget<T>(value: T) {
    std::mem::forget(value);
}
fn exercise<T>(
    source: &[u8],
    base: u64,
    ordinal: usize,
    f: impl FnOnce(Cursor<'_>) -> (T, bool),
) -> T {
    let mut parts = [Descriptor::default(); 8];
    let mut nodes = [Node::default(); 8];
    let mut lists = Lists::new();
    let mut work = meter();
    let mut budget = HeaderBudget::new();
    let mut storage = Storage::new();
    let mut scratch = Scratch::new();
    let selected = selection(
        source,
        base,
        &mut parts,
        &mut nodes,
        &mut lists,
        &mut work,
        &mut budget,
    );
    let mut parent = Projecting::new(selected, Tick(1)).unwrap();
    for n in 1..=ordinal {
        let mut child = parent
            .next(storage.backing(256), &mut scratch, Tick(1))
            .unwrap();
        let mut done = false;
        for _ in 0..100000 {
            if child.poll(Tick(1)).unwrap() == crate::mime_structure::Status::Complete {
                done = true;
                break;
            }
        }
        assert!(done);
        if n == ordinal {
            let cursor = Cursor::new(child, Tick(1)).unwrap();
            let (result, complete) = f(cursor);
            if complete {
                assert_eq!(parent.completed().unwrap(), ordinal);
            } else {
                assert!(parent.completed().is_err());
            }
            return result;
        }
        child.finish(Tick(1)).unwrap();
    }
    panic!("missing ordinal")
}
fn drain(cursor: &mut Cursor<'_>, width: usize) -> Result<(Vec<u8>, usize), Error> {
    let mut bytes = Vec::new();
    for turn in 1..10000 {
        let mut window = [0xa5; 128];
        let before = snapshot(cursor);
        let progress = cursor.poll(Tick(1), &mut window[..width])?;
        let used = delta(before, snapshot(cursor));
        assert!(used[0] <= 5 && used[1] <= 21 && used[2] <= 5 && used[3] <= 2 && used[4] <= 64);
        assert!(progress.written <= 64);
        assert!(window[progress.written..].iter().all(|byte| *byte == 0xa5));
        bytes.extend_from_slice(&window[..progress.written]);
        if progress.status == Status::Complete {
            return Ok((bytes, turn));
        }
    }
    panic!("JSON did not finish")
}
fn snapshot(cursor: &Cursor<'_>) -> [u64; 5] {
    let left = cursor.scalars.work.remaining();
    [
        cursor.scalars.budget.source_bytes_remaining(),
        cursor.scalars.budget.steps_remaining(),
        left.io_bytes,
        left.records,
        left.output_bytes,
    ]
}
fn delta(before: [u64; 5], after: [u64; 5]) -> [u64; 5] {
    std::array::from_fn(|n| before[n] - after[n])
}
#[test]
fn original_fields_short_drains_and_exact_new_wire_costs() {
    for source in [SOURCE, ALTERNATIVE] {
        let count = if source == SOURCE { 3 } else { 4 };
        for ordinal in 1..=count {
            let mut expected = None;
            for base in [0, 17, u64::MAX - source.len() as u64] {
                for width in 1..=8 {
                    let emitted = exercise(source, base, ordinal, |mut cursor| {
                        let end = cursor.end;
                        let pointers = (
                            std::ptr::from_mut(cursor.scalars.work),
                            std::ptr::from_mut(cursor.scalars.budget),
                            std::ptr::from_mut(cursor.scratch),
                        );
                        let before = snapshot(&cursor);
                        for _ in 0..3 {
                            assert_eq!(
                                cursor.poll(Tick(1), &mut []).unwrap(),
                                Progress {
                                    written: 0,
                                    status: Status::NeedOutput
                                }
                            );
                        }
                        assert_eq!(snapshot(&cursor), before);
                        assert_eq!(*cursor.next, ordinal - 1);
                        let (bytes, _) = drain(&mut cursor, width).unwrap();
                        let costs = delta(before, snapshot(&cursor));
                        assert_eq!(costs[4] as usize, bytes.len());
                        assert_eq!(cursor.value(), Some(end));
                        assert_eq!(*cursor.next, ordinal - 1);
                        let cached = snapshot(&cursor);
                        assert_eq!(
                            cursor.poll(Tick(100), &mut []).unwrap().status,
                            Status::Complete
                        );
                        assert_eq!(snapshot(&cursor), cached);
                        let object = format!("{{{}}}", std::str::from_utf8(&bytes).unwrap());
                        let parsed = td_json::parse(&object).unwrap();
                        assert_eq!(
                            parsed.get("size"),
                            Some(&td_json::Json::Num(end.part.size.to_string()))
                        );
                        assert_eq!(
                            parsed.get("partId"),
                            Some(&if end.part.media == Media::Multipart {
                                td_json::Json::Null
                            } else {
                                td_json::Json::Str(ordinal.to_string())
                            })
                        );
                        assert_eq!(
                            parsed.get("type"),
                            Some(&td_json::Json::Str(
                                std::str::from_utf8(cursor.metadata.headers.content_type)
                                    .unwrap()
                                    .to_owned()
                            ))
                        );
                        let (got, work, budget, scratch) = cursor.finish(Tick(1)).unwrap();
                        assert_eq!(got, end);
                        assert_eq!(
                            (
                                std::ptr::from_mut(work),
                                std::ptr::from_mut(budget),
                                std::ptr::from_mut(scratch)
                            ),
                            pointers
                        );
                        (bytes, true)
                    });
                    if let Some(wanted) = &expected {
                        assert_eq!(&emitted, wanted);
                    } else {
                        expected = Some(emitted);
                    }
                }
            }
            let text = std::str::from_utf8(expected.as_ref().unwrap()).unwrap();
            if source == SOURCE && ordinal == 2 {
                assert!(text.ends_with(",\"cid\":\"id@a\",\"language\":[\"fr\"],\"location\":null"));
            }
            if source == SOURCE && ordinal == 3 {
                assert!(text.ends_with(",\"location\":\"../leaf\""));
            }
        }
    }
}
#[test]
fn selected_filename_escaping_charset_and_nulls_match_literal_fragment() {
    let source = concat!(
        "Content-Type: text/plain; charset=UTF-8\r\n",
        "Content-Disposition: attachment; filename*=utf-8''e%CC%81%E4%BE%8B%F0%9F%90%88%22%5Cx\r\n",
        "Content-ID: <i@a>\r\nContent-Language: en, fr\r\nContent-Location: ../x\r\n\r\nabc"
    )
    .as_bytes();
    let expected = "\"partId\":\"1\",\"size\":3,\"type\":\"text/plain\",\"charset\":\"UTF-8\",\"name\":\"é例🐈\\\"\\\\x\",\"disposition\":\"attachment\",\"cid\":\"i@a\",\"language\":[\"en\",\"fr\"],\"location\":\"../x\"";
    for width in [1, 2, 6, 128] {
        exercise(source, 37, 1, |mut cursor| {
            let expected_source = cursor.metadata.headers.content_type.len()
                + cursor.metadata.headers.charset.unwrap().len()
                + cursor.metadata.headers.filename.unwrap().len()
                + cursor.metadata.headers.disposition.unwrap().len();
            let before = snapshot(&cursor);
            let (bytes, _) = drain(&mut cursor, width).unwrap();
            let used = delta(before, snapshot(&cursor));
            assert_eq!(used[0] as usize, expected_source);
            assert_eq!(used[2] as usize, expected_source);
            assert_eq!(used[4] as usize, bytes.len());
            assert_eq!(bytes, expected.as_bytes());
            cursor.finish(Tick(1)).unwrap();
            ((), true)
        });
    }
}
#[test]
fn every_serialization_prefix_requires_fresh_final_admission_and_retires_parent() {
    let turns = exercise(SOURCE, 0, 2, |mut cursor| {
        let (_, turns) = drain(&mut cursor, 1).unwrap();
        cursor.finish(Tick(1)).unwrap();
        (turns, true)
    });
    for cut in 0..=turns {
        for trial in 0..6 {
            if trial >= 4 && cut == turns {
                continue;
            }
            exercise(SOURCE, 0, 2, |mut cursor| {
                for _ in 0..cut {
                    cursor.poll(Tick(1), &mut [0]).unwrap();
                }
                if trial == 0 {
                    assert_eq!(
                        cursor.check_deadline(Tick(100)),
                        Err(Error::Admission(nfc::Error::Work(Stop::Deadline)))
                    );
                    assert!(cursor.value().is_none());
                    let mut output = [0xa5; 8];
                    assert_eq!(
                        cursor.poll(Tick(1), &mut output),
                        Err(Error::Admission(nfc::Error::Work(Stop::Deadline)))
                    );
                    assert_eq!(output, [0xa5; 8]);
                    assert_eq!(
                        cursor.finish(Tick(1)).err(),
                        Some(Error::Admission(nfc::Error::Work(Stop::Deadline)))
                    );
                } else if trial == 1 {
                    assert_eq!(
                        cursor.finish(Tick(100)).err(),
                        Some(Error::Admission(nfc::Error::Work(Stop::Deadline)))
                    );
                } else if trial == 2 {
                    if cut == turns {
                        cursor.finish(Tick(1)).unwrap();
                        return ((), true);
                    }
                    assert_eq!(cursor.finish(Tick(1)).err(), Some(Error::InvalidState));
                } else if trial == 3 {
                    forget(cursor);
                } else {
                    let mut output = [0xa5; 8];
                    let error = cursor
                        .poll(Tick(100), if trial == 4 { &mut output } else { &mut [] })
                        .unwrap_err();
                    assert!(matches!(
                        error,
                        Error::Admission(nfc::Error::Work(Stop::Deadline))
                    ));
                    assert_eq!(output, [0xa5; 8]);
                    assert!(cursor.value().is_none());
                    assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
                }
                ((), false)
            });
        }
    }
}
#[test]
fn original_output_and_interpretation_cutoffs_are_sticky_without_replacement_owners() {
    let used = exercise(SOURCE, 0, 2, |mut cursor| {
        let before = snapshot(&cursor);
        drain(&mut cursor, 1).unwrap();
        let used = delta(before, snapshot(&cursor));
        cursor.finish(Tick(1)).unwrap();
        (used, true)
    });
    for (kind, used_value) in used.iter().copied().enumerate() {
        for cut in 0..=used_value {
            exercise(SOURCE, 0, 2, |mut cursor| {
                if kind < 2 {
                    let left = snapshot(&cursor);
                    cursor
                        .scalars
                        .budget
                        .charge(
                            &mut meter(),
                            Tick(1),
                            if kind == 0 { left[0] - cut } else { 0 },
                            if kind == 1 { left[1] - cut } else { 0 },
                            &mut crate::nfc::Credit::new(),
                        )
                        .unwrap();
                } else {
                    let left = cursor.scalars.work.remaining();
                    cursor
                        .scalars
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
                                ..Charge::default()
                            },
                        )
                        .unwrap();
                }
                let result = drain(&mut cursor, 1);
                if cut == used_value {
                    result.unwrap();
                    cursor.finish(Tick(1)).unwrap();
                    return ((), true);
                }
                let error = result.unwrap_err();
                assert!(matches!(error, Error::Admission(_)));
                assert!(cursor.value().is_none());
                let before = snapshot(&cursor);
                assert_eq!(cursor.check_deadline(Tick(1)), Err(error));
                assert_eq!(cursor.poll(Tick(100), &mut [0]), Err(error));
                assert_eq!(snapshot(&cursor), before);
                assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
                ((), false)
            });
        }
    }
}

#[test]
fn constructor_requires_complete_original_child_and_fresh_admission() {
    for trial in 0..3 {
        let mut parts = [Descriptor::default(); 8];
        let mut nodes = [Node::default(); 8];
        let mut lists = Lists::new();
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut storage = Storage::new();
        let mut scratch = Scratch::new();
        let selected = selection(
            SOURCE,
            17,
            &mut parts,
            &mut nodes,
            &mut lists,
            &mut work,
            &mut budget,
        );
        let mut parent = Projecting::new(selected, Tick(1)).unwrap();
        let mut part = parent
            .next(storage.backing(256), &mut scratch, Tick(1))
            .unwrap();
        if trial != 0 {
            for _ in 0..100000 {
                if part.poll(Tick(1)).unwrap() == crate::mime_structure::Status::Complete {
                    break;
                }
            }
            assert!(part.value().is_some());
        }
        if trial == 2 {
            let error = part.check_deadline(Tick(100)).unwrap_err();
            assert_eq!(Cursor::new(part, Tick(1)).err(), Some(error));
        } else {
            let error = Cursor::new(part, if trial == 1 { Tick(100) } else { Tick(1) })
                .err()
                .unwrap();
            if trial == 0 {
                assert_eq!(error, Error::Metadata(label_json::Error::InvalidState));
            } else {
                assert_eq!(
                    error,
                    Error::Metadata(label_json::Error::Admission(nfc::Error::Work(
                        Stop::Deadline
                    )))
                );
            }
        }
        assert!(parent.completed().is_err());
        assert!(parent.finish(Tick(1)).is_err());
    }
}

#[test]
fn decoded_sizes_and_present_empty_metadata_are_literal_wire_values() {
    for (source, size) in [
        (
            b"Content-Transfer-Encoding: base64\r\n\r\nYWJjZA==".as_slice(),
            4,
        ),
        (
            b"Content-Transfer-Encoding: quoted-printable\r\n\r\na=00b".as_slice(),
            3,
        ),
        (b"\r\n".as_slice(), 0),
    ] {
        exercise(source, 17, 1, |mut cursor| {
            let (bytes, _) = drain(&mut cursor, 1).unwrap();
            assert_eq!(bytes, format!("\"partId\":\"1\",\"size\":{size},\"type\":\"text/plain\",\"charset\":\"us-ascii\",\"name\":null,\"disposition\":null,\"cid\":null,\"language\":null,\"location\":null").as_bytes());
            cursor.finish(Tick(1)).unwrap();
            ((), true)
        });
    }
    exercise(
        b"Content-Disposition: inline;filename=\"\"\r\nContent-Location: \r\n\r\n",
        0,
        1,
        |mut cursor| {
            let (bytes, _) = drain(&mut cursor, 1).unwrap();
            assert_eq!(bytes, b"\"partId\":\"1\",\"size\":0,\"type\":\"text/plain\",\"charset\":\"us-ascii\",\"name\":\"\",\"disposition\":\"inline\",\"cid\":null,\"language\":null,\"location\":\"\"");
            cursor.finish(Tick(1)).unwrap();
            ((), true)
        },
    );
}

#[test]
fn long_location_copy_obeys_64_byte_turns_with_wide_output() {
    let suffix = "a".repeat(160);
    let source = format!("Content-Location: ../{suffix}\r\n\r\nabc");
    exercise(source.as_bytes(), 17, 1, |mut cursor| {
        let before = snapshot(&cursor);
        let prepaid = cursor.metadata.location.value.unwrap().len();
        assert_eq!(prepaid, 165);
        let mut bytes = Vec::new();
        let mut raw_turns = Vec::new();
        let mut done = false;
        for _ in 0..10000 {
            let raw = cursor.field == Field::Location && matches!(cursor.phase, Phase::Raw);
            let remaining = cursor.scalars.work.remaining().output_bytes;
            let mut output = [0xa5; 128];
            let progress = cursor.poll(Tick(1), &mut output).unwrap();
            assert!(progress.written <= 64);
            assert!(output[progress.written..].iter().all(|byte| *byte == 0xa5));
            bytes.extend_from_slice(&output[..progress.written]);
            if raw {
                raw_turns.push(progress.written);
                assert_eq!(
                    remaining - cursor.scalars.work.remaining().output_bytes,
                    progress.written as u64
                );
            }
            if progress.status == Status::Complete {
                done = true;
                break;
            }
        }
        assert!(done);
        assert_eq!(raw_turns, [64, 64, 37]);
        assert_eq!(bytes,format!("\"partId\":\"1\",\"size\":3,\"type\":\"text/plain\",\"charset\":\"us-ascii\",\"name\":null,\"disposition\":null,\"cid\":null,\"language\":null,\"location\":\"../{suffix}\"").as_bytes());
        assert_eq!(delta(before, snapshot(&cursor))[4] as usize, bytes.len());
        cursor.finish(Tick(1)).unwrap();
        ((), true)
    });
}

#[test]
fn final_charset_mapping_preserves_selected_labels_defaults_and_nontext_nulls() {
    for (source, expected) in [
        (b"\r\nx".as_slice(), Some("us-ascii")),
        (
            b"Content-Type: text/calendar\r\n\r\nx".as_slice(),
            Some("us-ascii"),
        ),
        (
            b"Content-Type: text/plain; charset=unknown-label\r\n\r\nx".as_slice(),
            Some("unknown-label"),
        ),
        (
            b"Content-Type: text/plain; charset=\"bad label\"\r\n\r\nx".as_slice(),
            Some("us-ascii"),
        ),
        (
            b"Content-Type: application/octet-stream\r\n\r\nx".as_slice(),
            None,
        ),
        (b"Content-Type: message/rfc822\r\n\r\nx".as_slice(), None),
        (
            b"Content-Type: application/octet-stream; charset=UTF-8\r\n\r\nx".as_slice(),
            Some("UTF-8"),
        ),
    ] {
        exercise(source, 17, 1, |mut cursor| {
            let (bytes, _) = drain(&mut cursor, 128).unwrap();
            let parsed =
                td_json::parse(&format!("{{{}}}", std::str::from_utf8(&bytes).unwrap())).unwrap();
            assert_eq!(
                parsed.get("charset"),
                Some(&expected.map_or(td_json::Json::Null, |s| td_json::Json::Str(s.to_owned())))
            );
            cursor.finish(Tick(1)).unwrap();
            ((), true)
        });
    }
    exercise(SOURCE, 17, 2, |mut cursor| {
        let (bytes, _) = drain(&mut cursor, 128).unwrap();
        let parsed =
            td_json::parse(&format!("{{{}}}", std::str::from_utf8(&bytes).unwrap())).unwrap();
        assert_eq!(
            parsed.get("type"),
            Some(&td_json::Json::Str("message/rfc822".to_owned()))
        );
        assert_eq!(
            parsed.get("charset"),
            Some(&td_json::Json::Str("us-ascii".to_owned()))
        );
        cursor.finish(Tick(1)).unwrap();
        ((), true)
    });
}
#[test]
fn unsigned_size_property_refuses_out_of_range_original_evidence() {
    for size in [0, 9_007_199_254_740_991, 9_007_199_254_740_992, u64::MAX] {
        exercise(b"\r\nx", 17, 1, |mut cursor| {
            cursor.end.part.size = size;
            let result = drain(&mut cursor, 128);
            if size <= 9_007_199_254_740_991 {
                let (bytes, _) = result.unwrap();
                let parsed =
                    td_json::parse(&format!("{{{}}}", std::str::from_utf8(&bytes).unwrap()))
                        .unwrap();
                assert_eq!(
                    parsed.get("size"),
                    Some(&td_json::Json::Num(size.to_string()))
                );
                cursor.finish(Tick(1)).unwrap();
                ((), true)
            } else {
                assert_eq!(result.err(), Some(Error::InvalidState));
                assert!(cursor.value().is_none());
                assert_eq!(cursor.finish(Tick(1)).err(), Some(Error::InvalidState));
                ((), false)
            }
        });
    }
}

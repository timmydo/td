#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;
use crate::{headers, time::Deadline};
fn meter() -> Meter {
    Meter::new(
        Deadline::after(Tick(0), 100).unwrap(),
        Charge {
            io_bytes: 100_000_000,
            records: 10_000_000,
            output_bytes: 10_000_000,
            unlinks: 0,
        },
    )
}
fn prefix(source: &[u8], base: u64) -> (&[u8], SourceEnd) {
    let mut scanner = headers::Scanner::new(base, u64::MAX);
    let mut position = 0;
    let mut work = meter();
    for _ in 0..1_000_000 {
        let p = scanner
            .poll(&source[position..], true, Tick(1), &mut work)
            .unwrap();
        position += p.consumed;
        if matches!(p.status, headers::Status::Complete(_)) {
            let retained = (position + 1).min(source.len());
            return (
                &source[..retained],
                if retained == source.len() {
                    SourceEnd::Eof
                } else {
                    SourceEnd::Prefix
                },
            );
        }
    }
    panic!("header prefix did not complete")
}
fn drive(
    cursor: &mut Cursor<'_, '_>,
    source: &[u8],
    base: u64,
    width: usize,
    work: &mut Meter,
    budget: &mut HeaderBudget,
) -> Result<bool, Error> {
    // Header backing must outlive the cursor; this helper supplies only bodies.
    let mut rewound = false;
    let mut previous: Option<(u64, u64)> = None;
    for _ in 0..1_000_000 {
        let request = cursor.request()?;
        let input = match request {
            Request::Headers { .. } => return Ok(rewound),
            Request::None => Input::None,
            Request::Body { start, end } => {
                rewound |= previous
                    .is_some_and(|(offset, previous_end)| end == previous_end && start < offset);
                previous = Some((start, end));
                let to = end.min(start + width as u64);
                Input::Body {
                    offset: start,
                    bytes: &source[(start - base) as usize..(to - base) as usize],
                }
            }
        };
        if cursor.poll(input, Tick(1), work, budget)? == Status::Complete {
            return Ok(rewound);
        }
        assert!(cursor.parts()?.is_none());
    }
    panic!("fragment traversal failed to finish")
}
fn complete<'a>(
    cursor: &mut Cursor<'a, '_>,
    source: &'a [u8],
    base: u64,
    width: usize,
    work: &mut Meter,
    budget: &mut HeaderBudget,
) -> Result<bool, Error> {
    let mut rewound = false;
    for _ in 0..1_000_000 {
        if let Request::Headers { start, end, .. } = cursor.request()? {
            let entity = &source[(start - base) as usize..(end - base) as usize];
            let (bytes, source_end) = prefix(entity, start);
            cursor.poll(Input::Headers { bytes, source_end }, Tick(1), work, budget)?;
        }
        rewound |= drive(cursor, source, base, width, work, budget)?;
        if cursor.parts()?.is_some() {
            return Ok(rewound);
        }
    }
    panic!("fragmented entity traversal did not finish")
}
#[test]
fn fragmented_headers_bodies_and_qp_rewinds_preserve_literal_tree() {
    const BASE: u64 = 20000;
    let source = concat!(
        "Content-Type: multipart/digest;boundary=x\r\n\r\npre\r\n--x\r\n",
        "\r\nFrom: attached\r\n\r\nraw\r\n--x\r\n",
        "Content-Type: text/plain\r\nContent-Transfer-Encoding: base64\r\n\r\nYWJj!\r\n--x\r\n",
        "Content-Type: text/plain\r\nContent-Transfer-Encoding: quoted-printable\r\n\r\na=       z\r\n--x--\r\nepi\r\n--x\r\nignored"
    ).as_bytes();
    let mut resident_parts = [Part::default(); 8];
    let mut work = meter();
    let mut budget = HeaderBudget::new();
    let mut resident = super::super::Cursor::new(
        source,
        BASE,
        SourceEnd::Eof,
        Limits::default(),
        &mut resident_parts,
        &mut work,
        &mut budget,
    )
    .unwrap();
    while resident.poll(Tick(1)).unwrap() != Status::Complete {}
    let header_total = resident.header_bytes().unwrap();
    let (expected, _, _) = resident.finish(Tick(1)).unwrap();
    assert_eq!(expected.len(), 4);
    assert_eq!(expected[1].media, Media::MessageRfc822);
    assert_eq!(expected[1].size, b"From: attached\r\n\r\nraw".len() as u64);
    assert_eq!(expected[2].size, 3);
    assert_eq!(expected[2].problems(), ENCODING_PROBLEM);
    assert_eq!(expected[3].size, 10);
    assert_eq!(expected[3].problems(), ENCODING_PROBLEM);
    for width in [1, 2, 3, 7, 31, 128, 4096] {
        let mut backing = [Part::default(); 8];
        let mut cursor = Cursor::new(
            BASE..BASE + source.len() as u64,
            Limits::default(),
            &mut backing,
        )
        .unwrap();
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let rewind = complete(&mut cursor, source, BASE, width, &mut work, &mut budget).unwrap();
        if width == 1 {
            assert!(rewind);
        }
        assert_eq!(cursor.parts().unwrap(), Some(expected));
        assert_eq!(cursor.header_bytes(), Some(header_total));
        assert_eq!(
            cursor.finish(Tick(1), &mut work, &mut budget).unwrap(),
            expected
        );
    }
}
#[test]
fn every_original_work_cut_hides_tree_and_remains_sticky() {
    let source = b"Content-Type: multipart/mixed;boundary=x\n\n--x\n\na\n--x--\nepilogue";
    let mut parts = [Part::default(); 4];
    let mut work = meter();
    let initial = work.remaining();
    let mut budget = HeaderBudget::new();
    let mut cursor = Cursor::new(0..source.len() as u64, Limits::default(), &mut parts).unwrap();
    complete(&mut cursor, source, 0, 3, &mut work, &mut budget).unwrap();
    let left = work.remaining();
    let cost = Charge {
        io_bytes: initial.io_bytes - left.io_bytes,
        records: initial.records - left.records,
        output_bytes: initial.output_bytes - left.output_bytes,
        unlinks: 0,
    };
    for (kind, maximum, stop) in [
        (0, cost.io_bytes, Stop::IoBytes),
        (1, cost.records, Stop::Records),
        (2, cost.output_bytes, Stop::OutputBytes),
    ] {
        for cut in 0..=maximum {
            let mut cap = cost;
            match kind {
                0 => cap.io_bytes = cut,
                1 => cap.records = cut,
                _ => cap.output_bytes = cut,
            }
            let mut work = Meter::new(Deadline::after(Tick(0), 100).unwrap(), cap);
            let mut budget = HeaderBudget::new();
            let mut parts = [Part::default(); 4];
            let mut cursor =
                Cursor::new(0..source.len() as u64, Limits::default(), &mut parts).unwrap();
            let result = complete(&mut cursor, source, 0, 3, &mut work, &mut budget);
            if cut == maximum {
                assert!(result.is_ok());
            } else {
                let error = Error::Work(stop);
                assert_eq!(result, Err(error));
                assert_eq!(cursor.parts(), Err(error));
                assert_eq!(cursor.header_bytes(), None);
                assert_eq!(
                    cursor.poll(Input::None, Tick(1), &mut work, &mut budget),
                    Err(error)
                );
                assert_eq!(cursor.finish(Tick(1), &mut work, &mut budget), Err(error));
            }
        }
    }
}
#[test]
fn original_owners_and_cached_completion_are_fresh() {
    for completed in [false, true] {
        for replace_budget in [false, true] {
            let mut parts = [Part::default(); 1];
            let mut work = meter();
            let mut budget = HeaderBudget::new();
            let mut cursor = Cursor::new(0..1, Limits::default(), &mut parts).unwrap();
            cursor
                .poll(
                    Input::Headers {
                        bytes: b"\n",
                        source_end: SourceEnd::Eof,
                    },
                    Tick(1),
                    &mut work,
                    &mut budget,
                )
                .unwrap();
            if completed {
                complete(&mut cursor, b"\n", 0, 1, &mut work, &mut budget).unwrap();
            }
            let mut other_work = meter();
            let mut other_budget = HeaderBudget::new();
            let original_before = work.remaining();
            let before = (
                other_work.remaining(),
                other_budget.source_bytes_remaining(),
                other_budget.steps_remaining(),
            );
            let error = Error::InvalidState;
            let result = if replace_budget {
                cursor.check_deadline(Tick(1), &mut work, &mut other_budget)
            } else {
                cursor.check_deadline(Tick(1), &mut other_work, &mut budget)
            };
            assert_eq!(result, Err(error));
            assert_eq!(work.remaining(), original_before);
            assert_eq!(
                before,
                (
                    other_work.remaining(),
                    other_budget.source_bytes_remaining(),
                    other_budget.steps_remaining()
                )
            );
            assert_eq!(cursor.parts(), Err(error));
            assert_eq!(
                cursor.check_deadline(Tick(1), &mut work, &mut budget),
                Err(error)
            );
        }
    }
    let mut parts = [Part::default(); 1];
    let mut work = meter();
    let mut budget = HeaderBudget::new();
    let mut cursor = Cursor::new(0..1, Limits::default(), &mut parts).unwrap();
    complete(&mut cursor, b"\n", 0, 1, &mut work, &mut budget).unwrap();
    let before = work.remaining();
    assert_eq!(
        cursor.poll(Input::None, Tick(1), &mut work, &mut budget),
        Ok(Status::Complete)
    );
    assert_eq!(work.remaining(), before);
    assert_eq!(
        cursor.poll(Input::None, Tick(100), &mut work, &mut budget),
        Err(Error::Work(Stop::Deadline))
    );
    assert_eq!(cursor.parts(), Err(Error::Work(Stop::Deadline)));
}
#[test]
fn invalid_input_never_fabricates_eof_or_advances_another_extent() {
    for kind in 0..4 {
        let mut parts = [Part::default(); 4];
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut cursor = Cursor::new(10..30, Limits::default(), &mut parts).unwrap();
        let (input, error) = match kind {
            0 => (
                Input::Headers {
                    bytes: b"\n",
                    source_end: SourceEnd::Eof,
                },
                Error::IncompleteSource,
            ),
            1 => (Input::None, Error::InvalidState),
            2 => (
                Input::Headers {
                    bytes: b"Content-Type: multipart/mixed",
                    source_end: SourceEnd::Prefix,
                },
                Error::InvalidRange,
            ),
            _ => (
                Input::Body {
                    offset: 10,
                    bytes: b"",
                },
                Error::InvalidState,
            ),
        };
        assert_eq!(
            cursor.poll(input, Tick(1), &mut work, &mut budget),
            Err(error)
        );
        assert_eq!(cursor.parts(), Err(error));
    }
    let bytes = b"Content-Transfer-Encoding: base64\n\nYQ==";
    for invalid in [false, true] {
        let mut parts = [Part::default(); 1];
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut cursor = Cursor::new(0..bytes.len() as u64, Limits::default(), &mut parts).unwrap();
        cursor
            .poll(
                Input::Headers {
                    bytes,
                    source_end: SourceEnd::Eof,
                },
                Tick(1),
                &mut work,
                &mut budget,
            )
            .unwrap();
        while cursor.request().unwrap() == Request::None {
            cursor
                .poll(Input::None, Tick(1), &mut work, &mut budget)
                .unwrap();
        }
        let Request::Body { start, .. } = cursor.request().unwrap() else {
            panic!("missing body request")
        };
        if invalid {
            assert_eq!(
                cursor.poll(
                    Input::Body {
                        offset: start + 1,
                        bytes: b"Y"
                    },
                    Tick(1),
                    &mut work,
                    &mut budget
                ),
                Err(Error::InvalidRange)
            );
        } else {
            for _ in 0..3 {
                assert_eq!(
                    cursor.poll(
                        Input::Body {
                            offset: start,
                            bytes: b""
                        },
                        Tick(1),
                        &mut work,
                        &mut budget
                    ),
                    Ok(Status::Yield)
                );
                assert!(cursor.parts().unwrap().is_none());
            }
            complete(&mut cursor, bytes, 0, 1, &mut work, &mut budget).unwrap();
            assert_eq!(cursor.parts().unwrap().unwrap()[0].size, 1);
        }
    }
}

#[test]
fn fragmented_missing_close_and_inverted_child_ranges_remain_explicit() {
    let source = b"Content-Type: multipart/mixed;boundary=x\n\n--x\n\nbody";
    for width in [1, 2, 3, 128] {
        let mut parts = [Part::default(); 4];
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut cursor =
            Cursor::new(0..source.len() as u64, Limits::default(), &mut parts).unwrap();
        complete(&mut cursor, source, 0, width, &mut work, &mut budget).unwrap();
        let parts = cursor.finish(Tick(1), &mut work, &mut budget).unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].problems(), MISSING_CLOSE);
        assert_eq!(parts[1].size, 4);
        assert_eq!(parts[1].problems(), 0);
        assert_eq!(parts[1].entity_end, source.len() as u64);
    }
    let mut parts = [Part::default(); 1];
    let mut cursor = Cursor::new(0..20, Limits::default(), &mut parts).unwrap();
    let before = cursor.request().unwrap();
    assert_eq!(
        cursor.start_child(10, 9, 0, false),
        Err(Error::InvalidRange)
    );
    assert_eq!(cursor.request().unwrap(), before);
}

#[test]
fn nested_fragment_edges_missing_close_and_suffixes_match_literal_extents() {
    for missing in [false, true] {
        let source = format!("Content-Type: multipart/mixed;boundary=x\r\n\r\n--x ignored\r\nContent-Type: multipart/mixed;boundary=y\r\n\r\n--y \t\r\n\r\nbody{}\r\n--x-- \t\r\nepilogue", if missing { "" } else { "\r\n--y--\r\ninner-epilogue" });
        for width in [1, 2, 3, 7, 4096] {
            let mut parts = [Part::default(); 4];
            let mut work = meter();
            let mut budget = HeaderBudget::new();
            let mut cursor =
                Cursor::new(0..source.len() as u64, Limits::default(), &mut parts).unwrap();
            complete(
                &mut cursor,
                source.as_bytes(),
                0,
                width,
                &mut work,
                &mut budget,
            )
            .unwrap();
            let parts = cursor.finish(Tick(1), &mut work, &mut budget).unwrap();
            assert_eq!(parts.len(), 3);
            assert_eq!(parts[0].problems(), IGNORED_SUFFIX);
            assert_eq!(parts[1].parent, 1);
            assert_eq!(parts[2].parent, 2);
            assert_eq!(parts[1].problems(), if missing { MISSING_CLOSE } else { 0 });
            assert_eq!(parts[2].size, 4);
            assert_eq!(parts[2].depth, 3);
        }
    }
}
#[test]
fn separate_header_arena_and_exact_aggregate_ceiling_preserve_views() {
    let source = b"Content-Type: multipart/mixed;boundary=x\n\n--x\nX: y\n\nbody\n--x--";
    let expected_total = b"Content-Type: multipart/mixed;boundary=x\nX: y\n".len();
    let mut parts = [Part::default(); 4];
    let mut work = meter();
    let mut budget = HeaderBudget::new();
    let mut arena = [0; 512];
    let mut remaining = Some(arena.as_mut_slice());
    let mut saved = Vec::new();
    let mut views = Vec::new();
    let mut cursor = Cursor::new(0..source.len() as u64, Limits::default(), &mut parts).unwrap();
    for _ in 0..10000 {
        let input = match cursor.request().unwrap() {
            Request::Headers { start, end, .. } => {
                let (bytes, source_end) = prefix(&source[start as usize..end as usize], start);
                let (target, rest) = remaining.take().unwrap().split_at_mut(bytes.len());
                target.copy_from_slice(bytes);
                remaining = Some(rest);
                saved.push(bytes.to_vec());
                views.push(&*target);
                Input::Headers {
                    bytes: target,
                    source_end,
                }
            }
            Request::Body { start, end } => Input::Body {
                offset: start,
                bytes: &source[start as usize..end.min(start + 1) as usize],
            },
            Request::None => Input::None,
        };
        if cursor.poll(input, Tick(1), &mut work, &mut budget).unwrap() == Status::Complete {
            break;
        }
    }
    assert_eq!(cursor.header_bytes(), Some(expected_total as u64));
    assert_eq!(
        cursor
            .finish(Tick(1), &mut work, &mut budget)
            .unwrap()
            .len(),
        2
    );
    remaining.unwrap().fill(0xa5);
    assert_eq!(views.len(), 2);
    for (view, saved) in views.iter().zip(saved) {
        assert_eq!(*view, saved);
    }
    for limit in [expected_total - 1, expected_total] {
        let mut parts = [Part::default(); 4];
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut cursor = Cursor::new(
            0..source.len() as u64,
            Limits {
                header_bytes: limit,
                ..Limits::default()
            },
            &mut parts,
        )
        .unwrap();
        let result = complete(&mut cursor, source, 0, 1, &mut work, &mut budget);
        if limit == expected_total {
            assert!(result.is_ok());
            assert_eq!(cursor.header_bytes(), Some(expected_total as u64));
        } else {
            assert_eq!(result, Err(Error::HeaderLimit));
            assert_eq!(cursor.parts(), Err(Error::HeaderLimit));
        }
    }
}
#[test]
fn qp_only_rewind_empty_inputs_and_delimiter_overruns_are_explicit() {
    for source in [
        b"Content-Transfer-Encoding: quoted-printable\n\na=       z".as_slice(),
        b"Content-Type: multipart/mixed;boundary=x\n\n--x\n\na\n--x--",
    ] {
        for invalid in 0..3 {
            let mut parts = [Part::default(); 4];
            let mut work = meter();
            let mut budget = HeaderBudget::new();
            let mut cursor =
                Cursor::new(0..source.len() as u64, Limits::default(), &mut parts).unwrap();
            cursor
                .poll(
                    Input::Headers {
                        bytes: source,
                        source_end: SourceEnd::Eof,
                    },
                    Tick(1),
                    &mut work,
                    &mut budget,
                )
                .unwrap();
            for _ in 0..10000 {
                if matches!(cursor.request().unwrap(), Request::Body { .. }) {
                    break;
                }
                cursor
                    .poll(Input::None, Tick(1), &mut work, &mut budget)
                    .unwrap();
            }
            let Request::Body { start, end } = cursor.request().unwrap() else {
                panic!("missing body")
            };
            if invalid != 0 {
                let input = if invalid == 1 {
                    Input::Body {
                        offset: start + 1,
                        bytes: b"",
                    }
                } else {
                    Input::Body {
                        offset: start,
                        bytes: source,
                    }
                };
                assert_eq!(
                    cursor.poll(input, Tick(1), &mut work, &mut budget),
                    Err(Error::InvalidRange)
                );
                assert_eq!(cursor.parts(), Err(Error::InvalidRange));
                continue;
            }
            for _ in 0..3 {
                assert_eq!(
                    cursor.poll(
                        Input::Body {
                            offset: start,
                            bytes: b""
                        },
                        Tick(1),
                        &mut work,
                        &mut budget
                    ),
                    Ok(Status::Yield)
                );
                assert_eq!(cursor.request(), Ok(Request::Body { start, end }));
            }
            let rewind = complete(&mut cursor, source, 0, 1, &mut work, &mut budget).unwrap();
            if source.starts_with(b"Content-Transfer-Encoding") {
                assert!(rewind);
            }
        }
    }
    let mut parts = [Part::default(); 1];
    let mut work = meter();
    let mut budget = HeaderBudget::new();
    let mut cursor = Cursor::new(0..1, Limits::default(), &mut parts).unwrap();
    assert_eq!(
        cursor.poll(
            Input::Headers {
                bytes: b"\n",
                source_end: SourceEnd::Prefix
            },
            Tick(1),
            &mut work,
            &mut budget
        ),
        Err(Error::IncompleteSource)
    );
}

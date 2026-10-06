#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;
use crate::ports::Deadline;
fn work() -> Meter {
    Meter::new(
        Deadline::after(Tick(0), 100).unwrap(),
        Charge {
            io_bytes: 10_000_000,
            records: 2_000_000,
            output_bytes: 10_000_000,
            ..Charge::default()
        },
    )
}
fn complete(cursor: &mut Cursor<'_, '_, '_, '_>, meter: &mut Meter) -> Result<(), Error> {
    for _ in 0..200_000 {
        let before = meter.remaining();
        let result = cursor.poll(Tick(1), meter);
        let after = meter.remaining();
        assert!(before.io_bytes - after.io_bytes <= 6);
        assert!(before.records - after.records <= 1);
        assert!(before.output_bytes - after.output_bytes <= 4);
        assert_eq!(before.unlinks, after.unlinks);
        match result? {
            Status::Yield => assert!(cursor.value().is_none()),
            Status::Complete => return Ok(()),
        }
    }
    panic!("body properties JSON stalled")
}
fn decode<R>(
    source: Option<&str>,
    capacity: usize,
    f: impl FnOnce(Argument<'_, '_>) -> R,
) -> Result<R, Error> {
    let mut buffers = std::array::from_fn::<_, 8, _>(|_| String::with_capacity(capacity));
    let mut cells = buffers.each_mut().map(Cell::new);
    let mut keys = [""; 8];
    let mut cursor = Cursor::new(source, &mut cells, &mut keys);
    complete(&mut cursor, &mut work())?;
    Ok(f(cursor.finish()?))
}
#[test]
fn omission_is_default_explicit_empty_is_none_and_null_never_defaults() {
    let mut cells = [];
    let mut keys = [];
    let mut meter = work();
    let before = meter.remaining();
    let mut cursor = Cursor::new(None, &mut cells, &mut keys);
    assert_eq!(cursor.value(), Some(Argument::Default));
    assert_eq!(cursor.poll(Tick(100), &mut meter), Ok(Status::Complete));
    assert_eq!(meter.remaining(), before);
    assert_eq!(cursor.finish().unwrap(), Argument::Default);
    let mut cells = [];
    let mut keys = [];
    let mut cursor = Cursor::new(Some("[]"), &mut cells, &mut keys);
    let mut meter = work();
    let before = meter.remaining();
    complete(&mut cursor, &mut meter).unwrap();
    let after = meter.remaining();
    assert_eq!(before.io_bytes - after.io_bytes, 2);
    assert_eq!(before.records - after.records, 3);
    assert_eq!(before.output_bytes, after.output_bytes);
    assert_eq!(cursor.finish().unwrap(), Argument::Explicit(&[]));
    assert_eq!(decode(Some("null"), 0, |_| ()), Err(Error::Syntax));
}
#[test]
fn literals_all_escapes_and_unicode_pairs_decode_to_independent_values() {
    let source =
        r#"["partId","blob\u0049d","é","𝄞","\u00E9","\uD834\uDD1E","\"\\\/\b\f\n\r\t\u0000"]"#;
    let actual = decode(Some(source), 64, |argument| {
        let Argument::Explicit(keys) = argument else {
            panic!("default");
        };
        keys.iter().map(|key| key.to_string()).collect::<Vec<_>>()
    })
    .unwrap();
    assert_eq!(
        actual,
        [
            "partId",
            "blobId",
            "é",
            "𝄞",
            "é",
            "𝄞",
            "\"\\/\u{8}\u{c}\n\r\t\0"
        ]
    );
}
#[test]
fn exact_scalar_visit_record_and_output_debits_are_independent() {
    for (source, visits, records, output) in [
        (r#"["a"]"#, 5, 6, 1),
        (r#"["é"]"#, 8, 6, 2),
        (r#"["𝄞"]"#, 10, 6, 4),
        (r#"["\u0061"]"#, 10, 11, 1),
        (r#"["\uD834\uDD1E"]"#, 16, 17, 4),
        (r#"["\n"]"#, 6, 7, 1),
    ] {
        let mut buffer = String::with_capacity(8);
        let mut cells = [Cell::new(&mut buffer)];
        let mut keys = [""];
        let mut cursor = Cursor::new(Some(source), &mut cells, &mut keys);
        let mut meter = work();
        let before = meter.remaining();
        complete(&mut cursor, &mut meter).unwrap();
        let after = meter.remaining();
        assert_eq!(
            (
                before.io_bytes - after.io_bytes,
                before.records - after.records,
                before.output_bytes - after.output_bytes
            ),
            (visits, records, output)
        );
        let before = meter.remaining();
        assert_eq!(cursor.poll(Tick(100), &mut meter), Ok(Status::Complete));
        assert_eq!(meter.remaining(), before);
    }
}
#[test]
fn invalid_shapes_truncation_surrogates_controls_and_trailing_data_refuse() {
    for source in [
        "",
        "null",
        "{}",
        "true",
        r#"["a",]"#,
        r#"[null]"#,
        r#"[1]"#,
        r#"["a"]x"#,
        r#"["\q"]"#,
        "[\"raw\nline\"]",
        r#"["\uD800"]"#,
        r#"["\uDC00"]"#,
        r#"["\uD800\u0041"]"#,
        r#"["\uGGGG"]"#,
        r#"[["x"]]"#,
        r#"["unfinished"#,
        r#"["\u123"#,
    ] {
        let mut buffers = std::array::from_fn::<_, 8, _>(|_| String::with_capacity(64));
        let mut cells = buffers.each_mut().map(Cell::new);
        let mut keys = [""; 8];
        let mut cursor = Cursor::new(Some(source), &mut cells, &mut keys);
        let mut meter = work();
        assert_eq!(
            complete(&mut cursor, &mut meter),
            Err(Error::Syntax),
            "{source}"
        );
        assert!(cursor.value().is_none());
        let before = meter.remaining();
        assert_eq!(cursor.poll(Tick(100), &mut meter), Err(Error::Syntax));
        assert_eq!(meter.remaining(), before);
        assert_eq!(cursor.poll(Tick(1), &mut work()), Err(Error::Syntax));
        assert_eq!(cursor.finish().err(), Some(Error::Syntax));
    }
}
#[test]
fn key_and_cell_counts_and_utf8_capacity_are_exact_and_preserve_prefixes() {
    for capacity in [0, 1, 2, 3] {
        let mut buffer = String::with_capacity(capacity);
        let actual_capacity = buffer.capacity();
        let ptr = buffer.as_ptr();
        let mut cells = [Cell::new(&mut buffer)];
        let mut keys = ["sentinel"];
        let slots = keys.as_ptr();
        let mut cursor = Cursor::new(Some(r#"["é"]"#), &mut cells, &mut keys);
        if actual_capacity < 2 {
            assert_eq!(complete(&mut cursor, &mut work()), Err(Error::Capacity));
            assert!(cursor.value().is_none());
            assert_eq!(cursor.finish().err(), Some(Error::Capacity));
            assert_eq!(keys, ["sentinel"]);
            assert!(buffer.is_empty());
        } else {
            complete(&mut cursor, &mut work()).unwrap();
            let Argument::Explicit(value) = cursor.finish().unwrap() else {
                panic!("default");
            };
            assert_eq!(value, ["é"]);
            assert_eq!(value.as_ptr(), slots);
            assert_eq!(value[0].as_ptr(), ptr);
        }
    }
    let mut buffer = String::with_capacity(3);
    let actual_capacity = buffer.capacity();
    let source = format!("[\"{}\"]", "a".repeat(actual_capacity + 1));
    let expected_prefix = "a".repeat(actual_capacity);
    let mut cells = [Cell::new(&mut buffer)];
    let mut keys = ["sentinel"];
    let mut cursor = Cursor::new(Some(&source), &mut cells, &mut keys);
    assert_eq!(complete(&mut cursor, &mut work()), Err(Error::Capacity));
    assert_eq!(cursor.finish().err(), Some(Error::Capacity));
    assert_eq!(keys, ["sentinel"]);
    assert_eq!(buffer, expected_prefix);
    let mut buffers = std::array::from_fn::<_, 2, _>(|_| String::with_capacity(8));
    let mut cells = buffers.each_mut().map(Cell::new);
    let mut keys = ["sentinel"];
    let mut cursor = Cursor::new(Some(r#"["a","b"]"#), &mut cells, &mut keys);
    assert_eq!(complete(&mut cursor, &mut work()), Err(Error::Capacity));
    assert_eq!(cursor.finish().err(), Some(Error::Capacity));
    assert_eq!(keys, ["a"]);
    let mut buffer = String::with_capacity(8);
    let mut cells = [Cell::new(&mut buffer)];
    let mut keys = ["sentinel"; 2];
    let mut cursor = Cursor::new(Some(r#"["a","b"]"#), &mut cells, &mut keys);
    assert_eq!(complete(&mut cursor, &mut work()), Err(Error::Capacity));
    assert_eq!(cursor.finish().err(), Some(Error::Capacity));
    assert_eq!(keys, ["a", "sentinel"]);
}
#[test]
fn every_cut_deadline_exact_work_and_premature_finish_forbid_whole_success() {
    let source = r#"["a","\uD834\uDD1E"]"#;
    let mut buffers = std::array::from_fn::<_, 2, _>(|_| String::with_capacity(8));
    let mut cells = buffers.each_mut().map(Cell::new);
    let mut keys = [""; 2];
    let mut cursor = Cursor::new(Some(source), &mut cells, &mut keys);
    let mut meter = work();
    let before = meter.remaining();
    let mut turns = 0;
    loop {
        turns += 1;
        if cursor.poll(Tick(1), &mut meter).unwrap() == Status::Complete {
            break;
        }
    }
    let after = meter.remaining();
    let visits = before.io_bytes - after.io_bytes;
    let records = before.records - after.records;
    let output = before.output_bytes - after.output_bytes;
    for (io_bytes, records, output_bytes, expected) in [
        (visits, records, output, None),
        (visits - 1, records, output, Some(Stop::IoBytes)),
        (visits, records - 1, output, Some(Stop::Records)),
        (visits, records, output - 1, Some(Stop::OutputBytes)),
    ] {
        let mut buffers = std::array::from_fn::<_, 2, _>(|_| String::with_capacity(8));
        let mut cells = buffers.each_mut().map(Cell::new);
        let mut keys = [""; 2];
        let mut cursor = Cursor::new(Some(source), &mut cells, &mut keys);
        let mut meter = Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes,
                records,
                output_bytes,
                ..Charge::default()
            },
        );
        if let Some(stop) = expected {
            let error = Error::Work(stop);
            assert_eq!(complete(&mut cursor, &mut meter), Err(error));
            assert!(cursor.value().is_none());
            let before = meter.remaining();
            assert_eq!(cursor.poll(Tick(100), &mut meter), Err(error));
            assert_eq!(meter.remaining(), before);
            assert_eq!(cursor.poll(Tick(1), &mut work()), Err(error));
            assert_eq!(cursor.finish().err(), Some(error));
        } else {
            complete(&mut cursor, &mut meter).unwrap();
            assert_eq!(meter.remaining(), Charge::default());
        }
    }
    for cut in 0..turns {
        let mut buffers = std::array::from_fn::<_, 2, _>(|_| String::with_capacity(8));
        let mut cells = buffers.each_mut().map(Cell::new);
        let mut keys = [""; 2];
        let mut cursor = Cursor::new(Some(source), &mut cells, &mut keys);
        let mut meter = work();
        for _ in 0..cut {
            assert_eq!(cursor.poll(Tick(1), &mut meter), Ok(Status::Yield));
        }
        let before = meter.remaining();
        let error = Error::Work(Stop::Deadline);
        assert_eq!(cursor.poll(Tick(100), &mut meter), Err(error));
        assert!(cursor.value().is_none());
        assert_eq!(meter.remaining(), before);
        assert_eq!(cursor.poll(Tick(1), &mut work()), Err(error));
        assert_eq!(cursor.finish().err(), Some(error));
        let mut buffers = std::array::from_fn::<_, 2, _>(|_| String::with_capacity(8));
        let mut cells = buffers.each_mut().map(Cell::new);
        let mut keys = [""; 2];
        let mut cursor = Cursor::new(Some(source), &mut cells, &mut keys);
        let mut meter = work();
        for _ in 0..cut {
            cursor.poll(Tick(1), &mut meter).unwrap();
        }
        assert_eq!(cursor.finish().err(), Some(Error::InvalidState));
    }
}
#[test]
fn decoded_keys_feed_original_body_selection_and_unknown_names_never_default() {
    let source = r#"["\u0070artId","blob\u0049d","headers","subParts","header:X:asText:all","header:X:asText:all"]"#;
    decode(Some(source), 64, |argument| {
        let Argument::Explicit(keys) = argument else {
            panic!("default");
        };
        let mut cells = std::array::from_fn::<_, 2, _>(|_| super::super::Cell::new());
        let mut cursor = super::super::Cursor::new(keys, &mut cells);
        let mut meter = work();
        loop {
            if cursor.poll(Tick(1), &mut meter).unwrap() == super::super::Status::Complete {
                break;
            }
        }
        let view = cursor.finish().unwrap();
        assert_eq!(
            view.fields(),
            super::super::Fields {
                metadata: super::super::Metadata {
                    part_id: true,
                    blob_id: true,
                    ..super::super::Metadata::NONE
                },
                headers: true,
                sub_parts: true
            }
        );
        assert_eq!(view.headers().len(), 1);
        assert_eq!(
            view.headers()[0].property().unwrap().requested(),
            "header:X:asText:all"
        );
        assert_eq!(
            view.headers()[0].property().unwrap().requested().as_ptr(),
            keys[4].as_ptr()
        );
    })
    .unwrap();
    decode(Some(r#"["unknown"]"#), 64, |argument| {
        let Argument::Explicit(keys) = argument else {
            panic!("default");
        };
        let mut cells = [];
        let mut cursor = super::super::Cursor::new(keys, &mut cells);
        let mut meter = work();
        loop {
            match cursor.poll(Tick(1), &mut meter) {
                Ok(super::super::Status::Yield) => {}
                Err(error) => {
                    assert_eq!(error, super::super::Error::InvalidProperty);
                    assert!(cursor.value().is_none());
                    break;
                }
                _ => panic!("unknown selection completed"),
            }
        }
    })
    .unwrap();
}
#[test]
fn long_names_preserve_utf8_in_original_fixed_storage_without_terminal_rescan() {
    let name = format!("header:{}:asText:all", "X".repeat(65536));
    let source = format!(r#"["{name}"]"#);
    let mut buffer = String::with_capacity(name.len());
    let ptr = buffer.as_ptr();
    let mut cells = [Cell::new(&mut buffer)];
    let mut keys = [""];
    let slots = keys.as_ptr();
    let mut cursor = Cursor::new(Some(&source), &mut cells, &mut keys);
    let mut meter = work();
    let before = meter.remaining();
    complete(&mut cursor, &mut meter).unwrap();
    let after = meter.remaining();
    assert_eq!(before.io_bytes - after.io_bytes, source.len() as u64);
    assert_eq!(before.records - after.records, source.len() as u64 + 1);
    assert_eq!(before.output_bytes - after.output_bytes, name.len() as u64);
    let Argument::Explicit(value) = cursor.finish().unwrap() else {
        panic!("default");
    };
    assert_eq!(value, [name.as_str()]);
    assert_eq!(value.as_ptr(), slots);
    assert_eq!(value[0].as_ptr(), ptr);
}
pub fn probe(mut snapshot: impl FnMut()) {
    let long = format!("header:{}:asText:all", "X".repeat(65536));
    let long_source = format!(r#"["{long}","\uD834\uDD1E"]"#);
    for trial in 0..8 {
        let source = match trial {
            0 => None,
            1 => Some("[]"),
            2 => Some(long_source.as_str()),
            3 => Some(r#"["\uD800"]"#),
            4 => Some(r#"["abc"]"#),
            5 => Some(r#"["a"]"#),
            6 => Some(r#"["𝄞"]"#),
            _ => Some(r#"["\uD834\uDD1E"]"#),
        };
        let capacity = if trial == 2 {
            long.len()
        } else if trial == 4 {
            2
        } else {
            64
        };
        let mut buffers = std::array::from_fn::<_, 2, _>(|_| String::with_capacity(capacity));
        let actual_capacity = buffers[0].capacity();
        let capacity_source = format!("[\"{}\"]", "a".repeat(actual_capacity + 1));
        let source = if trial == 4 {
            Some(capacity_source.as_str())
        } else {
            source
        };
        let mut cells = buffers.each_mut().map(Cell::new);
        let mut keys = [""; 2];
        let mut cursor = Cursor::new(source, &mut cells, &mut keys);
        let mut meter = work();
        if trial >= 4 {
            loop {
                let ready = match trial {
                    4 => {
                        matches!(cursor.phase, Phase::String)
                            && cursor.active.as_ref().unwrap().len() == actual_capacity
                    }
                    5 | 6 => matches!(cursor.phase, Phase::String),
                    _ => matches!(cursor.phase, Phase::Unicode { digits: 2, .. }),
                };
                if ready {
                    break;
                }
                assert_eq!(cursor.poll(Tick(1), &mut meter), Ok(Status::Yield));
            }
            if trial == 5 || trial == 6 {
                meter = Meter::new(
                    Deadline::after(Tick(0), 100).unwrap(),
                    Charge {
                        io_bytes: 1000,
                        records: if trial == 5 { 0 } else { 1000 },
                        output_bytes: if trial == 6 { 0 } else { 1000 },
                        ..Charge::default()
                    },
                );
            }
        }
        snapshot();
        match trial {
            0 => {
                let before = meter.remaining();
                assert_eq!(cursor.poll(Tick(100), &mut meter), Ok(Status::Complete));
                assert_eq!(meter.remaining(), before);
                assert_eq!(cursor.finish().unwrap(), Argument::Default);
            }
            1 => {
                complete(&mut cursor, &mut meter).unwrap();
                let before = meter.remaining();
                assert_eq!(cursor.poll(Tick(100), &mut meter), Ok(Status::Complete));
                assert_eq!(meter.remaining(), before);
                assert_eq!(cursor.finish().unwrap(), Argument::Explicit(&[]));
            }
            2 => {
                complete(&mut cursor, &mut meter).unwrap();
                let Argument::Explicit(value) = cursor.finish().unwrap() else {
                    panic!("default");
                };
                assert_eq!(value, [long.as_str(), "𝄞"]);
            }
            3 => assert_eq!(complete(&mut cursor, &mut meter), Err(Error::Syntax)),
            4 => assert_eq!(cursor.poll(Tick(1), &mut meter), Err(Error::Capacity)),
            5 => assert_eq!(
                cursor.poll(Tick(1), &mut meter),
                Err(Error::Work(Stop::Records))
            ),
            6 => assert_eq!(
                cursor.poll(Tick(1), &mut meter),
                Err(Error::Work(Stop::OutputBytes))
            ),
            7 => assert_eq!(
                cursor.poll(Tick(100), &mut meter),
                Err(Error::Work(Stop::Deadline))
            ),
            _ => panic!("unknown JSON trial"),
        }
        snapshot();
    }
}

#[test]
fn all_json_whitespace_positions_lowercase_hex_and_separator_grammar_are_literal() {
    for (source, expected) in [
        (
            "\t\r\n [ \t\r\n \"a\" \t\r\n , \t\r\n \"b\" \t\r\n ] \t\r\n",
            &["a", "b"][..],
        ),
        (" \t\r\n [ \t\r\n ] \t\r\n", &[][..]),
        (
            r#"["\u00a0","\u00b0","\u00c0","\u00d0","\u00e0","\u00f0","\ud834\udd1e"]"#,
            &["\u{a0}", "°", "À", "Ð", "à", "ð", "𝄞"][..],
        ),
    ] {
        let mut buffers = std::array::from_fn::<_, 8, _>(|_| String::with_capacity(64));
        let mut cells = buffers.each_mut().map(Cell::new);
        let mut keys = [""; 8];
        let mut cursor = Cursor::new(Some(source), &mut cells, &mut keys);
        let mut meter = work();
        let before = meter.remaining();
        complete(&mut cursor, &mut meter).unwrap();
        let after = meter.remaining();
        assert_eq!(before.io_bytes - after.io_bytes, source.len() as u64);
        assert_eq!(before.records - after.records, source.len() as u64 + 1);
        assert_eq!(
            before.output_bytes - after.output_bytes,
            expected.iter().map(|key| key.len() as u64).sum::<u64>()
        );
        assert_eq!(cursor.finish().unwrap(), Argument::Explicit(expected));
    }
    for source in [
        "\u{b}[]",
        "[\u{c}]",
        "[\"a\"\u{b}]",
        "[]\u{c}",
        r#"["a""b"]"#,
    ] {
        assert_eq!(decode(Some(source), 64, |_| ()), Err(Error::Syntax));
    }
}

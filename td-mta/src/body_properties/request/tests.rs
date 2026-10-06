#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;
use crate::{
    admission::work::{Charge, Meter, Stop},
    ports::Deadline,
};
fn work() -> Meter {
    Meter::new(
        Deadline::after(Tick(0), 100).unwrap(),
        Charge {
            io_bytes: 10_000_000,
            records: 1_000_000,
            output_bytes: 10_000_000,
            ..Charge::default()
        },
    )
}
fn complete(cursor: &mut Cursor<'_, '_, '_, '_, '_>, meter: &mut Meter) -> Result<(), Error> {
    for _ in 0..200000 {
        let before = meter.remaining();
        let status = cursor.poll(Tick(1), meter)?;
        let after = meter.remaining();
        assert!(before.io_bytes - after.io_bytes <= 184);
        assert!(before.records - after.records <= 32);
        assert!(before.output_bytes - after.output_bytes <= 4);
        assert_eq!(before.unlinks, after.unlinks);
        if status == Status::Complete {
            return Ok(());
        }
        assert!(cursor.value().is_none());
    }
    panic!("request selection stalled")
}
#[test]
fn omitted_empty_and_null_keep_independent_default_semantics() {
    let mut text = [];
    let mut keys = [];
    let mut headers = [];
    let mut cursor = Cursor::new(None, &mut text, &mut keys, &mut headers);
    let expected = Fields {
        metadata: super::super::Metadata {
            part_id: true,
            blob_id: true,
            size: true,
            name: true,
            media_type: true,
            charset: true,
            disposition: true,
            cid: true,
            language: true,
            location: true,
        },
        headers: false,
        sub_parts: false,
    };
    assert_eq!(cursor.value().unwrap().fields(), expected);
    let mut meter = work();
    let before = meter.remaining();
    assert_eq!(cursor.poll(Tick(100), &mut meter), Ok(Status::Complete));
    assert_eq!(meter.remaining(), before);
    let view = cursor.finish().unwrap();
    assert_eq!(view.fields(), expected);
    assert!(view.headers().is_empty());
    let mut cursor = Cursor::new(Some("[]"), &mut text, &mut keys, &mut headers);
    let before = meter.remaining();
    complete(&mut cursor, &mut meter).unwrap();
    let after = meter.remaining();
    assert_eq!(
        (
            before.io_bytes - after.io_bytes,
            before.records - after.records,
            before.output_bytes - after.output_bytes
        ),
        (2, 3, 0)
    );
    assert_eq!(cursor.finish().unwrap().fields(), Fields::NONE);
    let mut cursor = Cursor::new(Some("null"), &mut text, &mut keys, &mut headers);
    assert_eq!(
        complete(&mut cursor, &mut work()),
        Err(Error::Json(json::Error::Syntax))
    );
    assert!(cursor.value().is_none());
}
#[test]
fn escaped_fields_and_duplicate_headers_keep_first_decoded_loan() {
    let source = r#"["part\u0049d","size","headers","sub\u0050arts","header:X","header:X"]"#;
    let mut buffers = std::array::from_fn::<_, 6, _>(|_| String::with_capacity(32));
    let first = buffers[4].as_ptr();
    let mut text = buffers.each_mut().map(json::Cell::new);
    let mut keys = [""; 6];
    let mut headers = [Cell::new(), Cell::new()];
    let mut cursor = Cursor::new(Some(source), &mut text, &mut keys, &mut headers);
    complete(&mut cursor, &mut work()).unwrap();
    let view = cursor.finish().unwrap();
    assert_eq!(
        view.fields(),
        Fields {
            metadata: super::super::Metadata {
                part_id: true,
                size: true,
                ..super::super::Metadata::NONE
            },
            headers: true,
            sub_parts: true
        }
    );
    assert_eq!(view.headers().len(), 1);
    let header = view.headers()[0].property().unwrap();
    assert_eq!(header.requested(), "header:X");
    assert_eq!(header.requested().as_ptr(), first);
}
#[test]
fn forwarding_preserves_each_original_turn_and_all_work_debits() {
    let source = r#"["size","header:XY","header:XY"]"#;
    let mut buffers = std::array::from_fn::<_, 3, _>(|_| String::with_capacity(32));
    let mut text = buffers.each_mut().map(json::Cell::new);
    let mut keys = [""; 3];
    let mut headers = [Cell::new()];
    let mut cursor = Cursor::new(Some(source), &mut text, &mut keys, &mut headers);
    let mut meter = work();
    let mut actual = Vec::new();
    loop {
        let before = meter.remaining();
        let status = cursor.poll(Tick(1), &mut meter).unwrap();
        let after = meter.remaining();
        actual.push((
            before.io_bytes - after.io_bytes,
            before.records - after.records,
            before.output_bytes - after.output_bytes,
        ));
        if status == Status::Complete {
            break;
        }
    }
    let mut other = std::array::from_fn::<_, 3, _>(|_| String::with_capacity(32));
    let mut text = other.each_mut().map(json::Cell::new);
    let mut keys = [""; 3];
    let mut headers = [Cell::new()];
    let mut json = json::Cursor::new(Some(source), &mut text, &mut keys);
    let mut meter = work();
    let mut expected = Vec::new();
    loop {
        let before = meter.remaining();
        let status = json.poll(Tick(1), &mut meter).unwrap();
        let after = meter.remaining();
        expected.push((
            before.io_bytes - after.io_bytes,
            before.records - after.records,
            before.output_bytes - after.output_bytes,
        ));
        if status == json::Status::Complete {
            break;
        }
    }
    let json::Argument::Explicit(keys) = json.finish().unwrap() else {
        panic!("expected explicit")
    };
    let mut selection = super::super::Cursor::new(keys, &mut headers);
    loop {
        let before = meter.remaining();
        let status = selection.poll(Tick(1), &mut meter).unwrap();
        let after = meter.remaining();
        expected.push((
            before.io_bytes - after.io_bytes,
            before.records - after.records,
            before.output_bytes - after.output_bytes,
        ));
        if status == super::super::Status::Complete {
            break;
        }
    }
    assert_eq!(actual, expected);
    assert_eq!(
        cursor.finish().unwrap().headers()[0].property(),
        selection.finish().unwrap().headers()[0].property()
    );
}
#[test]
fn json_errors_and_actual_text_capacity_remain_exact_and_sticky() {
    for (source, capacity, error) in [
        (r#"["x",]"#, 32, json::Error::Syntax),
        (r#"["ab"]"#, 1, json::Error::Capacity),
        (r#"["\uD800"]"#, 32, json::Error::Syntax),
    ] {
        let mut buffer = String::with_capacity(capacity);
        let mut text = [json::Cell::new(&mut buffer)];
        let mut keys = [""];
        let mut headers = [Cell::new()];
        let mut cursor = Cursor::new(Some(source), &mut text, &mut keys, &mut headers);
        let error = Error::Json(error);
        assert_eq!(complete(&mut cursor, &mut work()), Err(error));
        assert!(cursor.value().is_none());
        let mut meter = work();
        let before = meter.remaining();
        assert_eq!(cursor.poll(Tick(100), &mut meter), Err(error));
        assert_eq!(meter.remaining(), before);
        assert_eq!(cursor.poll(Tick(1), &mut work()), Err(error));
        assert_eq!(cursor.finish().err(), Some(error));
    }
}
#[test]
fn semantic_alias_forms_and_header_capacity_refuse_whole_selection() {
    for (source, error) in [
        (
            r#"["size","unknown"]"#,
            super::super::Error::InvalidProperty,
        ),
        (r#"["subject"]"#, super::super::Error::InvalidProperty),
        (
            r#"["header:From:asDate"]"#,
            super::super::Error::Recognition(crate::body_property::Error::ForbiddenForm),
        ),
        (r#"["header:X"]"#, super::super::Error::Capacity),
    ] {
        let mut buffers = std::array::from_fn::<_, 2, _>(|_| String::with_capacity(32));
        let mut text = buffers.each_mut().map(json::Cell::new);
        let mut keys = [""; 2];
        let mut headers = [];
        let mut cursor = Cursor::new(Some(source), &mut text, &mut keys, &mut headers);
        let error = Error::Selection(error);
        assert_eq!(complete(&mut cursor, &mut work()), Err(error));
        assert!(cursor.value().is_none());
        let mut meter = work();
        let before = meter.remaining();
        assert_eq!(cursor.poll(Tick(100), &mut meter), Err(error));
        assert_eq!(meter.remaining(), before);
        assert_eq!(cursor.finish().err(), Some(error));
    }
}
#[test]
fn every_deadline_cut_and_exact_or_one_short_limits_preserve_first_failure() {
    let source = r#"["size","header:X","header:XY","header:XY"]"#;
    let mut buffers = std::array::from_fn::<_, 4, _>(|_| String::with_capacity(32));
    let mut text = buffers.each_mut().map(json::Cell::new);
    let mut keys = [""; 4];
    let mut headers = [Cell::new(), Cell::new()];
    let mut cursor = Cursor::new(Some(source), &mut text, &mut keys, &mut headers);
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
    let costs = (
        before.io_bytes - after.io_bytes,
        before.records - after.records,
        before.output_bytes - after.output_bytes,
    );
    for cut in 0..turns {
        let mut buffers = std::array::from_fn::<_, 4, _>(|_| String::with_capacity(32));
        let mut text = buffers.each_mut().map(json::Cell::new);
        let mut keys = [""; 4];
        let mut headers = [Cell::new(), Cell::new()];
        let mut cursor = Cursor::new(Some(source), &mut text, &mut keys, &mut headers);
        let mut meter = work();
        for _ in 0..cut {
            assert_eq!(cursor.poll(Tick(1), &mut meter), Ok(Status::Yield));
        }
        let error = if cut < source.len() + 1 {
            Error::Json(json::Error::Work(Stop::Deadline))
        } else {
            Error::Selection(super::super::Error::Recognition(
                crate::body_property::Error::Work(Stop::Deadline),
            ))
        };
        let before = meter.remaining();
        assert_eq!(cursor.poll(Tick(100), &mut meter), Err(error));
        assert_eq!(meter.remaining(), before);
        assert!(cursor.value().is_none());
        assert_eq!(cursor.poll(Tick(1), &mut work()), Err(error));
        assert_eq!(cursor.finish().err(), Some(error));
    }
    for (io, records, output, expected) in [
        (costs.0, costs.1, costs.2, None),
        (costs.0 - 1, costs.1, costs.2, Some(Stop::IoBytes)),
        (costs.0, costs.1 - 1, costs.2, Some(Stop::Records)),
        (costs.0, costs.1, costs.2 - 1, Some(Stop::OutputBytes)),
    ] {
        let mut buffers = std::array::from_fn::<_, 4, _>(|_| String::with_capacity(32));
        let mut text = buffers.each_mut().map(json::Cell::new);
        let mut keys = [""; 4];
        let mut headers = [Cell::new(), Cell::new()];
        let mut cursor = Cursor::new(Some(source), &mut text, &mut keys, &mut headers);
        let mut meter = Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: io,
                records,
                output_bytes: output,
                ..Charge::default()
            },
        );
        let result = complete(&mut cursor, &mut meter);
        if let Some(expected) = expected {
            let error = result.unwrap_err();
            assert_eq!(
                error,
                if expected == Stop::OutputBytes {
                    Error::Json(json::Error::Work(expected))
                } else {
                    Error::Selection(super::super::Error::Recognition(
                        crate::body_property::Error::Work(expected),
                    ))
                }
            );
            assert!(cursor.value().is_none());
            assert_eq!(cursor.poll(Tick(1), &mut work()), Err(error));
        } else {
            result.unwrap();
            let after = meter.remaining();
            assert_eq!(
                (after.io_bytes, after.records, after.output_bytes),
                (0, 0, 0)
            );
            assert!(cursor.value().is_some());
        }
    }
}
#[test]
fn premature_finish_and_cached_completion_preserve_passive_result() {
    let source = r#"["header:X"]"#;
    let mut observed_complete = false;
    for cut in 0..128 {
        let mut buffer = String::with_capacity(16);
        let mut text = [json::Cell::new(&mut buffer)];
        let mut keys = [""];
        let mut headers = [Cell::new()];
        let mut cursor = Cursor::new(Some(source), &mut text, &mut keys, &mut headers);
        let mut meter = work();
        let mut done = false;
        for _ in 0..cut {
            if cursor.poll(Tick(1), &mut meter).unwrap() == Status::Complete {
                done = true;
                observed_complete = true;
                break;
            }
        }
        if !done {
            assert!(cursor.value().is_none());
            assert_eq!(cursor.finish().err(), Some(Error::InvalidState));
        } else {
            let before = meter.remaining();
            let ptr = cursor.value().unwrap().headers().as_ptr();
            assert_eq!(cursor.poll(Tick(100), &mut meter), Ok(Status::Complete));
            assert_eq!(meter.remaining(), before);
            assert_eq!(cursor.finish().unwrap().headers().as_ptr(), ptr);
            break;
        }
    }
    assert!(observed_complete);
}
#[test]
fn long_header_keys_borrow_original_text_and_cells_without_copying() {
    let header = format!("header:{}", "X".repeat(65536));
    let source = format!("[\"{header}\"]");
    let mut buffer = String::with_capacity(header.len());
    let ptr = buffer.as_ptr();
    let mut text = [json::Cell::new(&mut buffer)];
    let mut keys = [""];
    let mut headers = [Cell::new()];
    let cells = headers.as_ptr();
    let mut cursor = Cursor::new(Some(&source), &mut text, &mut keys, &mut headers);
    let mut meter = work();
    complete(&mut cursor, &mut meter).unwrap();
    let view = cursor.finish().unwrap();
    assert_eq!(view.headers().as_ptr(), cells);
    assert_eq!(view.headers()[0].property().unwrap().requested(), header);
    assert_eq!(
        view.headers()[0].property().unwrap().requested().as_ptr(),
        ptr
    );
}
pub fn probe(mut snapshot: impl FnMut()) {
    let long = format!("header:{}", "X".repeat(65536));
    let long_source = format!("[\"{long}\",\"{long}\"]");
    for trial in 0..8 {
        let source = match trial {
            0 => None,
            1 => Some(
                r#"["partId","blobId","size","headers","name","type","charset","disposition","cid","language","location","subParts"]"#,
            ),
            2 => Some(long_source.as_str()),
            3 => Some("null"),
            4 => Some(r#"["unknown"]"#),
            5 => Some(r#"["header:X"]"#),
            _ => Some(r#"["header:X","header:X"]"#),
        };
        let capacity = if trial == 2 { long.len() } else { 32 };
        let mut buffers = std::array::from_fn::<_, 12, _>(|_| String::with_capacity(capacity));
        let mut text = buffers.each_mut().map(json::Cell::new);
        let mut keys = [""; 12];
        let mut header_storage = [Cell::new(), Cell::new()];
        let headers = if trial == 5 {
            &mut header_storage[..0]
        } else {
            &mut header_storage[..]
        };
        let mut cursor = Cursor::new(source, &mut text, &mut keys, headers);
        let mut meter = work();
        let mut empty_text = [];
        let mut empty_keys = [];
        let mut empty_headers = [];
        let mut empty = Cursor::new(
            Some("[]"),
            &mut empty_text,
            &mut empty_keys,
            &mut empty_headers,
        );
        if trial >= 6 {
            loop {
                if matches!(&cursor.phase,Some(Phase::Selection(selection)) if trial==7 || (selection.used==1 && matches!(selection.phase,super::super::Phase::Header{..})))
                {
                    break;
                }
                assert_eq!(cursor.poll(Tick(1), &mut meter), Ok(Status::Yield));
            }
            if trial == 6 {
                meter = Meter::new(
                    Deadline::after(Tick(0), 100).unwrap(),
                    Charge {
                        io_bytes: 1000000,
                        records: 0,
                        output_bytes: 1000000,
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
                let view = cursor.finish().unwrap();
                assert_eq!(view.fields(), Fields::DEFAULT);
                assert!(view.headers().is_empty());
                assert_eq!(meter.remaining(), before);
                complete(&mut empty, &mut meter).unwrap();
                assert_eq!(empty.finish().unwrap().fields(), Fields::NONE);
            }
            1 | 2 => {
                complete(&mut cursor, &mut meter).unwrap();
                let view = cursor.finish().unwrap();
                assert_eq!(view.headers().len(), usize::from(trial == 2));
                if trial == 1 {
                    assert_eq!(
                        view.fields(),
                        Fields {
                            metadata: Fields::DEFAULT.metadata,
                            headers: true,
                            sub_parts: true
                        }
                    );
                }
            }
            3 => assert_eq!(
                complete(&mut cursor, &mut meter),
                Err(Error::Json(json::Error::Syntax))
            ),
            4 => assert_eq!(
                complete(&mut cursor, &mut meter),
                Err(Error::Selection(super::super::Error::InvalidProperty))
            ),
            5 => assert_eq!(
                complete(&mut cursor, &mut meter),
                Err(Error::Selection(super::super::Error::Capacity))
            ),
            6 => assert_eq!(
                cursor.poll(Tick(1), &mut meter),
                Err(Error::Selection(super::super::Error::Recognition(
                    crate::body_property::Error::Work(Stop::Records)
                )))
            ),
            _ => assert_eq!(
                cursor.poll(Tick(100), &mut meter),
                Err(Error::Selection(super::super::Error::Recognition(
                    crate::body_property::Error::Work(Stop::Deadline)
                )))
            ),
        }
        snapshot();
    }
}

#[test]
fn omitted_argument_hides_populated_header_tail_without_changing_its_loan() {
    let mut headers = [Cell::new(), Cell::new()];
    {
        let keys = ["header:X"];
        let mut previous = super::super::Cursor::new(&keys, &mut headers);
        let mut meter = work();
        loop {
            if previous.poll(Tick(1), &mut meter).unwrap() == super::super::Status::Complete {
                break;
            }
        }
        assert_eq!(previous.finish().unwrap().headers().len(), 1);
    }
    assert_eq!(headers[0].property().unwrap().requested(), "header:X");
    let first = headers.as_ptr();
    let mut text = [];
    let mut keys = [];
    let cursor = Cursor::new(None, &mut text, &mut keys, &mut headers);
    let view = cursor.value().unwrap();
    assert_eq!(view.fields(), Fields::DEFAULT);
    assert!(view.headers().is_empty());
    assert_eq!(view.headers().as_ptr(), first);
    let view = cursor.finish().unwrap();
    assert_eq!(view.fields(), Fields::DEFAULT);
    assert!(view.headers().is_empty());
    assert_eq!(view.headers().as_ptr(), first);
}
#[test]
fn impossible_default_from_explicit_decoder_refuses_whole_success_stickily() {
    let mut text = [];
    let mut keys = [];
    let mut headers = [];
    let mut cursor = Cursor::new(None, &mut text, &mut keys, &mut headers);
    let mut decoder_text = [];
    let mut decoder_keys = [];
    cursor.phase = Some(Phase::Json(json::Cursor::new(
        None,
        &mut decoder_text,
        &mut decoder_keys,
    )));
    let mut meter = work();
    let before = meter.remaining();
    assert_eq!(cursor.poll(Tick(1), &mut meter), Err(Error::InvalidState));
    assert_eq!(meter.remaining(), before);
    assert!(cursor.value().is_none());
    assert_eq!(
        cursor.poll(Tick(100), &mut work()),
        Err(Error::InvalidState)
    );
    assert_eq!(cursor.finish().err(), Some(Error::InvalidState));
}

#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;
use crate::{admission::work::Charge, email_property::Field, ports::Deadline};
fn work() -> Meter {
    Meter::new(
        Deadline::after(Tick(0), 100).unwrap(),
        Charge {
            io_bytes: 1_000_000,
            records: 1_000_000,
            output_bytes: 1_000_000,
            unlinks: 0,
        },
    )
}
fn select(
    source: Option<&str>,
    count: usize,
) -> Result<(crate::email_properties::Fields, Vec<String>), Error> {
    let mut buffers: Vec<String> = (0..32).map(|_| String::with_capacity(64)).collect();
    let mut text: Vec<_> = buffers.iter_mut().map(TextCell::new).collect();
    let mut keys = [""; 32];
    let mut headers: Vec<_> = (0..count).map(|_| Cell::new()).collect();
    let mut cursor = Cursor::new(source, &mut text, &mut keys, &mut headers);
    assert!(cursor.value().is_none());
    let mut meter = work();
    for _ in 0..100_000 {
        let before = meter.remaining();
        let status = match cursor.poll(Tick(1), &mut meter) {
            Ok(status) => status,
            Err(error) => {
                assert!(cursor.value().is_none());
                assert!(matches!(cursor.finish(), Err(e) if e == error));
                return Err(error);
            }
        };
        let after = meter.remaining();
        assert!(before.io_bytes - after.io_bytes <= 256);
        assert!(before.records - after.records <= 32);
        assert!(before.output_bytes - after.output_bytes <= 4);
        if status == Status::Complete {
            let view = cursor.finish()?;
            return Ok((
                view.fields(),
                view.headers()
                    .iter()
                    .map(|cell| cell.property().unwrap().requested().to_owned())
                    .collect(),
            ));
        }
    }
    panic!("completion");
}
#[test]
fn omitted_and_null_defaults_differ_from_explicit_empty() {
    let omitted = select(None, 11).unwrap();
    assert_eq!(omitted, select(Some("null"), 11).unwrap());
    assert_eq!(omitted, select(Some(" \r\nnull\t "), 11).unwrap());
    assert_eq!(
        omitted.1,
        [
            "messageId",
            "inReplyTo",
            "references",
            "sender",
            "from",
            "to",
            "cc",
            "bcc",
            "replyTo",
            "subject",
            "sentAt"
        ]
    );
    assert!(omitted.0.contains(Field::Preview));
    assert_eq!(
        select(Some(" [] "), 0).unwrap(),
        (crate::email_properties::Fields::id_only(), Vec::new())
    );
    for source in [None, Some("null")] {
        assert_eq!(
            select(source, 10),
            Err(Error::Selection(super::super::Error::Capacity))
        );
    }
}
#[test]
fn escaped_keys_select_and_deduplicate_after_decoding() {
    let (fields, headers) = select(
        Some(r#"["\u0073ubject","subject","header:Subject:asText","size","size"]"#),
        2,
    )
    .unwrap();
    assert_eq!(headers, ["subject", "header:Subject:asText"]);
    assert!(fields.contains(Field::Id));
    assert!(fields.contains(Field::Size));
    assert!(!fields.contains(Field::Preview));
}
#[test]
fn null_and_array_syntax_and_capacity_never_produce_partial_results() {
    for source in [
        "",
        "n",
        "nu",
        "nul",
        "nulx",
        "nullx",
        "null[]",
        "true",
        "{}",
        "[null]",
        "[\"subject\",]",
        "[\"subject\"]x",
        "[\"\\ud800\"]",
    ] {
        assert_eq!(
            select(Some(source), 11),
            Err(Error::Json(json::Error::Syntax)),
            "{source}"
        );
    }
    assert_eq!(
        select(Some(r#"["subject","unknown"]"#), 11),
        Err(Error::Selection(super::super::Error::InvalidProperty))
    );
    assert_eq!(
        select(Some(&format!("[\"{}\"]", "a".repeat(65))), 11),
        Err(Error::Json(json::Error::Capacity))
    );
    let mut text = [];
    let mut keys = [];
    let mut body = crate::body_properties::json::Cursor::new(Some("null"), &mut text, &mut keys);
    assert_eq!(body.poll(Tick(1), &mut work()), Err(json::Error::Syntax));
}
#[test]
fn unfinished_or_stopped_request_cannot_publish_or_renew_work() {
    for source in [None, Some("null"), Some(r#"["subject"]"#)] {
        let mut string = String::with_capacity(32);
        let mut text = [TextCell::new(&mut string)];
        let mut keys = [""];
        let mut headers = std::array::from_fn::<_, 11, _>(|_| Cell::new());
        let cursor = Cursor::new(source, &mut text, &mut keys, &mut headers);
        assert!(matches!(cursor.finish(), Err(Error::InvalidState)));
    }
    for cutoff in [0, 1, 4, 8, 20, 40] {
        let mut string = String::with_capacity(32);
        let mut text = [TextCell::new(&mut string)];
        let mut keys = [""];
        let mut headers = [Cell::new()];
        let mut cursor = Cursor::new(
            Some(r#"["header:Subject:asText"]"#),
            &mut text,
            &mut keys,
            &mut headers,
        );
        let mut meter = work();
        for _ in 0..cutoff {
            assert_eq!(cursor.poll(Tick(1), &mut meter).unwrap(), Status::Yield);
        }
        let error = cursor.poll(Tick(100), &mut meter).unwrap_err();
        assert_eq!(error, Error::Work(Stop::Deadline));
        assert!(cursor.value().is_none());
        let mut replacement = work();
        let before = replacement.remaining();
        assert_eq!(cursor.poll(Tick(1), &mut replacement), Err(error));
        assert_eq!(replacement.remaining(), before);
        assert!(matches!(cursor.finish(), Err(e) if e == error));
    }
}

#[test]
fn null_costs_and_default_handoff_do_not_bypass_admission() {
    let mut text = [];
    let mut keys = [];
    let mut json = json::Cursor::<true>::new(Some("null"), &mut text, &mut keys);
    let mut meter = work();
    for _ in 0..4 {
        let before = meter.remaining();
        assert_eq!(json.poll(Tick(1), &mut meter), Ok(json::Status::Yield));
        let after = meter.remaining();
        assert_eq!(before.io_bytes - after.io_bytes, 1);
        assert_eq!(before.records - after.records, 1);
        assert_eq!(before.output_bytes, after.output_bytes);
    }
    let before = meter.remaining();
    assert_eq!(json.poll(Tick(1), &mut meter), Ok(json::Status::Complete));
    assert_eq!(before.io_bytes, meter.remaining().io_bytes);
    assert_eq!(before.records - meter.remaining().records, 1);
    assert_eq!(json.finish(), Ok(json::Argument::Default));
    for source in [None, Some("null")] {
        for cutoff in 0..=5 {
            if source.is_none() && cutoff > 1 {
                continue;
            }
            let mut text = [];
            let mut keys = [];
            let mut headers = std::array::from_fn::<_, 11, _>(|_| Cell::new());
            let mut cursor = Cursor::new(source, &mut text, &mut keys, &mut headers);
            let mut meter = work();
            for _ in 0..cutoff {
                assert_eq!(cursor.poll(Tick(1), &mut meter), Ok(Status::Yield));
            }
            assert_eq!(
                cursor.poll(Tick(100), &mut meter),
                Err(Error::Work(Stop::Deadline))
            );
            assert!(cursor.value().is_none());
            let mut fresh = work();
            let before = fresh.remaining();
            assert_eq!(
                cursor.poll(Tick(1), &mut fresh),
                Err(Error::Work(Stop::Deadline))
            );
            assert_eq!(fresh.remaining(), before);
            assert!(matches!(cursor.finish(), Err(Error::Work(Stop::Deadline))));
        }
    }
    let mut text = [];
    let mut keys = [];
    let mut headers = std::array::from_fn::<_, 11, _>(|_| Cell::new());
    let mut cursor = Cursor::new(None, &mut text, &mut keys, &mut headers);
    let mut empty = Meter::new(Deadline::after(Tick(0), 100).unwrap(), Charge::default());
    assert_eq!(
        cursor.poll(Tick(1), &mut empty),
        Err(Error::Work(Stop::Records))
    );
}

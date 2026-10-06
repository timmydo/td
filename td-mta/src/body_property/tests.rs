#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;
use crate::{admission::work::Stop, ports::Deadline};
const EXPECTED: &[(&str, Field)] = &[
    ("partId", Field::PartId),
    ("blobId", Field::BlobId),
    ("size", Field::Size),
    ("headers", Field::Headers),
    ("name", Field::Name),
    ("type", Field::MediaType),
    ("charset", Field::Charset),
    ("disposition", Field::Disposition),
    ("cid", Field::Cid),
    ("language", Field::Language),
    ("location", Field::Location),
    ("subParts", Field::SubParts),
];
fn work() -> Meter {
    Meter::new(
        Deadline::after(Tick(0), 100).unwrap(),
        Charge {
            io_bytes: 10_000_000,
            records: 2_000_000,
            ..Charge::default()
        },
    )
}
fn complete<'a>(cursor: &mut Cursor<'a>, meter: &mut Meter) -> Result<Option<Property<'a>>, Error> {
    for _ in 0..10000 {
        let before = meter.remaining();
        let result = cursor.poll(Tick(1), meter);
        let after = meter.remaining();
        assert!(before.io_bytes - after.io_bytes <= 184);
        assert!(before.records - after.records <= 32);
        assert_eq!(
            (before.output_bytes, before.unlinks),
            (after.output_bytes, after.unlinks)
        );
        match result? {
            Status::Yield => {}
            Status::Complete(value) => return Ok(value),
        }
    }
    panic!("body property recognition stalled")
}
fn parse(key: &str) -> Result<Option<Property<'_>>, Error> {
    complete(&mut Cursor::new(key), &mut work())
}
#[test]
fn every_literal_body_field_including_structural_and_header_lists_is_distinct() {
    for &(key, field) in EXPECTED {
        assert_eq!(parse(key), Ok(Some(Property::Field(field))));
    }
}
#[test]
fn unknown_case_variants_and_email_aliases_are_left_to_the_dispatcher() {
    for key in [
        "",
        "partid",
        "BlobId",
        "SIZE",
        "mediaType",
        "subparts",
        "HEADERS",
        "Header:X",
        "subject",
        "from",
        "messageId",
        "bodyStructure",
        "é",
        " name ",
    ] {
        assert_eq!(parse(key), Ok(None), "{key}");
    }
}
#[test]
fn parameterized_headers_keep_borrowed_spelling_forms_and_occurrences() {
    use header_property::{Form, Occurrence};
    for (key, name, form, occurrence) in [
        (
            "header:cOnTeNt-TyPe",
            "cOnTeNt-TyPe",
            Form::Raw,
            Occurrence::Last,
        ),
        (
            "header:Content-Language:asText:all",
            "Content-Language",
            Form::Text,
            Occurrence::All,
        ),
        (
            "header:From:asAddresses",
            "From",
            Form::Addresses,
            Occurrence::Last,
        ),
        (
            "header:Reply-To:asGroupedAddresses:all",
            "Reply-To",
            Form::GroupedAddresses,
            Occurrence::All,
        ),
        (
            "header:References:asMessageIds",
            "References",
            Form::MessageIds,
            Occurrence::Last,
        ),
        (
            "header:Date:asDate:all",
            "Date",
            Form::Date,
            Occurrence::All,
        ),
        (
            "header:List-Post:asURLs",
            "List-Post",
            Form::URLs,
            Occurrence::Last,
        ),
    ] {
        let Some(Property::Header(property)) = parse(key).unwrap() else {
            panic!("header not recognized");
        };
        assert_eq!(property.requested(), key);
        assert_eq!(property.requested().as_ptr(), key.as_ptr());
        assert_eq!(property.name(), name);
        assert_eq!(property.name().as_ptr(), key[7..].as_ptr());
        assert_eq!(property.form(), form);
        assert_eq!(property.occurrence(), occurrence);
        let mut body = Cursor::new(key);
        let mut original = header_property::Cursor::new(key, header_property::Context::BodyPart);
        let mut body_work = work();
        let mut original_work = work();
        let mut done = false;
        for _ in 0..10000 {
            let expected = original.poll(Tick(1), &mut original_work).unwrap();
            let actual = body.poll(Tick(1), &mut body_work).unwrap();
            assert_eq!(body_work.remaining(), original_work.remaining());
            match expected {
                header_property::Status::Yield => assert_eq!(actual, Status::Yield),
                header_property::Status::Complete(value) => {
                    assert_eq!(actual, Status::Complete(value.map(Property::Header)));
                    done = true;
                    break;
                }
            }
        }
        assert!(done);
        let before = body_work.remaining();
        assert_eq!(
            body.poll(Tick(100), &mut body_work),
            Ok(Status::Complete(Some(Property::Header(property))))
        );
        assert_eq!(body_work.remaining(), before);
    }
}
#[test]
fn malformed_and_forbidden_header_forms_retain_exact_parser_errors() {
    for key in [
        "header:",
        "header::all",
        "header:X:asBroken",
        "header:X:all:asRaw",
        "header:é",
        "header:X ",
    ] {
        assert_eq!(parse(key), Err(Error::InvalidProperty), "{key}");
    }
    assert_eq!(parse("header:From:asDate"), Err(Error::ForbiddenForm));
}
#[test]
fn standard_field_turns_have_independent_exact_prepaid_visits_and_records() {
    for (target, &(key, field)) in EXPECTED.iter().enumerate() {
        let mut cursor = Cursor::new(key);
        let mut meter = work();
        let start = meter.remaining();
        assert_eq!(cursor.poll(Tick(1), &mut meter), Ok(Status::Yield));
        assert_eq!(
            start.io_bytes - meter.remaining().io_bytes,
            key.len().min(7) as u64
        );
        assert_eq!(start.records - meter.remaining().records, 1);
        for (row, &(candidate, _)) in EXPECTED.iter().enumerate().take(target + 1) {
            let before = meter.remaining();
            let status = cursor.poll(Tick(1), &mut meter).unwrap();
            let after = meter.remaining();
            assert_eq!(
                before.io_bytes - after.io_bytes,
                if key.len() == candidate.len() {
                    key.len() as u64
                } else {
                    0
                }
            );
            assert_eq!(before.records - after.records, 1);
            assert_eq!(
                status,
                if row == target {
                    Status::Complete(Some(Property::Field(field)))
                } else {
                    Status::Yield
                }
            );
        }
        let before = meter.remaining();
        assert_eq!(
            cursor.poll(Tick(100), &mut meter),
            Ok(Status::Complete(Some(Property::Field(field))))
        );
        assert_eq!(meter.remaining(), before);
    }
}
#[test]
fn byte_record_and_deadline_refusal_remember_the_first_error_without_new_work() {
    for (io_bytes, records, expected) in [
        (18, 3, Ok(Some(Property::Field(Field::BlobId)))),
        (17, 3, Err(Error::Work(Stop::IoBytes))),
        (18, 2, Err(Error::Work(Stop::Records))),
    ] {
        let mut cursor = Cursor::new("blobId");
        let mut meter = Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes,
                records,
                ..Charge::default()
            },
        );
        assert_eq!(complete(&mut cursor, &mut meter), expected);
        if let Err(error) = expected {
            let before = meter.remaining();
            assert_eq!(cursor.poll(Tick(100), &mut meter), Err(error));
            assert_eq!(meter.remaining(), before);
            assert_eq!(cursor.poll(Tick(1), &mut work()), Err(error));
        }
    }
    for (key, prefix) in [
        ("subParts", 0),
        ("subParts", 1),
        ("subParts", 4),
        ("unrecognized", 13),
        ("header:X-Long:asText", 1),
        ("header:From:asDate", 3),
    ] {
        let mut cursor = Cursor::new(key);
        let mut meter = work();
        for _ in 0..prefix {
            assert_eq!(cursor.poll(Tick(1), &mut meter), Ok(Status::Yield));
        }
        let before = meter.remaining();
        assert_eq!(
            cursor.poll(Tick(100), &mut meter),
            Err(Error::Work(Stop::Deadline))
        );
        assert_eq!(meter.remaining(), before);
        assert_eq!(
            cursor.poll(Tick(1), &mut work()),
            Err(Error::Work(Stop::Deadline))
        );
    }
}
#[test]
fn long_header_names_are_bounded_and_preserve_the_original_request_loan() {
    let key = format!("header:{}:asText:all", "X".repeat(65536));
    let Some(Property::Header(property)) = parse(&key).unwrap() else {
        panic!("long header not recognized");
    };
    assert_eq!(property.requested().as_ptr(), key.as_ptr());
    assert_eq!(property.name().as_ptr(), key[7..].as_ptr());
    assert_eq!(property.name().len(), 65536);
    assert_eq!(property.form(), header_property::Form::Text);
    assert_eq!(property.occurrence(), header_property::Occurrence::All);
}
#[test]
fn long_unknown_names_do_not_scan_or_copy_a_second_key() {
    let key = "é".repeat(65536);
    let mut cursor = Cursor::new(&key);
    let mut meter = work();
    let before = meter.remaining();
    assert_eq!(complete(&mut cursor, &mut meter), Ok(None));
    let after = meter.remaining();
    assert_eq!(before.io_bytes - after.io_bytes, 7);
    assert_eq!(before.records - after.records, 14);
    assert_eq!(
        (before.output_bytes, before.unlinks),
        (after.output_bytes, after.unlinks)
    );
}
/// Long decoded keys and meters are prepared outside the measured intervals.
pub fn probe(mut snapshot: impl FnMut()) {
    let long_header = format!("header:{}:asText:all", "X".repeat(65536));
    let long_unknown = "é".repeat(65536);
    for trial in 0..8 {
        let key = match trial {
            1 => long_header.as_str(),
            2 => long_unknown.as_str(),
            3 => "header:X:asBroken",
            4 => "header:From:asDate",
            6 => "header:X",
            _ => "subParts",
        };
        let mut cursor = Cursor::new(key);
        let mut meter = work();
        if trial == 5 {
            meter = Meter::new(
                Deadline::after(Tick(0), 100).unwrap(),
                Charge {
                    io_bytes: 1000,
                    records: 1,
                    ..Charge::default()
                },
            );
        }
        if trial == 6 {
            meter = Meter::new(
                Deadline::after(Tick(0), 100).unwrap(),
                Charge {
                    io_bytes: 7,
                    records: 2,
                    ..Charge::default()
                },
            );
        }
        if trial >= 5 {
            assert_eq!(cursor.poll(Tick(1), &mut meter), Ok(Status::Yield));
        }
        snapshot();
        match trial {
            0 => {
                for &(key, field) in EXPECTED {
                    let mut cursor = Cursor::new(key);
                    let mut meter = work();
                    assert_eq!(
                        complete(&mut cursor, &mut meter),
                        Ok(Some(Property::Field(field)))
                    );
                }
            }
            1 => assert!(matches!(
                complete(&mut cursor, &mut meter),
                Ok(Some(Property::Header(_)))
            )),
            2 => assert_eq!(complete(&mut cursor, &mut meter), Ok(None)),
            3 => assert_eq!(
                complete(&mut cursor, &mut meter),
                Err(Error::InvalidProperty)
            ),
            4 => assert_eq!(complete(&mut cursor, &mut meter), Err(Error::ForbiddenForm)),
            5 => assert_eq!(
                cursor.poll(Tick(1), &mut meter),
                Err(Error::Work(Stop::Records))
            ),
            6 => assert_eq!(
                cursor.poll(Tick(1), &mut meter),
                Err(Error::Work(Stop::IoBytes))
            ),
            7 => assert_eq!(
                cursor.poll(Tick(100), &mut meter),
                Err(Error::Work(Stop::Deadline))
            ),
            _ => panic!("unknown body property trial"),
        }
        snapshot();
    }
}

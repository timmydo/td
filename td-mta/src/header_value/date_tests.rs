#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;
use crate::{header_date, header_property, ports::Deadline};
fn work(output_bytes: u64) -> Meter {
    Meter::new(
        Deadline::after(Tick(0), 100).unwrap(),
        Charge {
            io_bytes: 100_000_000,
            records: 2_000_000,
            output_bytes,
            ..Charge::default()
        },
    )
}
fn property(key: &str) -> Property<'_> {
    let mut cursor = header_property::Cursor::new(key, header_property::Context::Email);
    let mut work = work(1000);
    for _ in 0..1000 {
        if let header_property::Status::Complete(value) = cursor.poll(Tick(1), &mut work).unwrap() {
            return value.unwrap();
        }
    }
    panic!("property did not finish");
}
fn input<'a>(bytes: &'a [u8], key: &'a str) -> Input<'a> {
    Input {
        bytes,
        base: 4096,
        header_limit: bytes.len() as u64,
        property: property(key),
        source_end: header_select::SourceEnd::Eof,
    }
}
fn drain(cursor: &mut Date<'_, '_>, width: usize) -> (Vec<u8>, Result<End, Error>) {
    assert!(std::mem::size_of_val(cursor) <= 896);
    let mut bytes = Vec::new();
    let mut output = [0xa5; 8];
    for _ in 0..100_000 {
        output.fill(0xa5);
        match cursor.poll(Tick(1), &mut output[..width]) {
            Ok(progress) => {
                assert!(progress.written <= 6);
                bytes.extend_from_slice(&output[..progress.written]);
                assert!(output[progress.written..].iter().all(|byte| *byte == 0xa5));
                if let Status::Complete(end) = progress.status {
                    return (bytes, Ok(end));
                }
            }
            Err(error) => {
                assert_eq!(output, [0xa5; 8]);
                assert_eq!(cursor.poll(Tick(1), &mut output), Err(error));
                assert_eq!(cursor.check_deadline(Tick(1)), Err(error));
                return (bytes, Err(error));
            }
        }
    }
    panic!("Date property did not finish");
}
#[test]
fn dates_preserve_last_all_null_and_leap_diagnostics_under_backpressure() {
    let source = b"Date: 1 Jan 2000 00:00 +0000\r\nDate: 31 Dec 2020 23:59:60 +0000\r\nDate: 1 Jan 2000 00:00 -0000\r\n\r\n";
    for width in 1..=8 {
        for (key, expected, unverified) in [
            ("header:Date:asDate", "\"2000-01-01T00:00:00-00:00\"", false),
            (
                "header:dAtE:asDate:all",
                "[\"2000-01-01T00:00:00Z\",null,\"2000-01-01T00:00:00-00:00\"]",
                true,
            ),
            ("header:X-Missing:asDate", "null", false),
            ("header:X-Missing:asDate:all", "[]", false),
        ] {
            let mut work = work(1000);
            let mut budget = HeaderBudget::new();
            let mut cursor = Date::new(input(source, key), &mut work, &mut budget).unwrap();
            let (bytes, result) = drain(&mut cursor, width);
            let end = result.unwrap();
            assert_eq!(bytes, expected.as_bytes());
            assert_eq!(cursor.has_unverified_leap(), unverified);
            assert_eq!(
                cursor.poll(Tick(100), &mut []),
                Ok(Progress {
                    written: 0,
                    status: Status::Complete(end)
                })
            );
            cursor.check_deadline(Tick(1)).unwrap();
            assert_eq!(work.remaining().output_bytes, 1000 - expected.len() as u64);
        }
    }
    for (source, expected, unverified) in [
        (b"Date:\n\n".as_slice(), "null", false),
        (b"Date: 1 Jan 2000 00:00 +0000 (bad\n\n", "null", false),
        (b"Date: 31 Dec 9999 23:59 -9959\n\n", "null", false),
        (
            b"Date: 31 Dec 2016 23:59:60 +0000\n\n",
            "\"2016-12-31T23:59:60Z\"",
            false,
        ),
        (b"Date: 31 Dec 2020 23:59:60 +0000\n\n", "null", true),
        (b"Date: 31 Dec 2016 23:59:61 +0000\n\n", "null", false),
    ] {
        let mut work = work(1000);
        let mut budget = HeaderBudget::new();
        let mut cursor =
            Date::new(input(source, "header:Date:asDate"), &mut work, &mut budget).unwrap();
        let (bytes, result) = drain(&mut cursor, 1);
        result.unwrap();
        assert_eq!(bytes, expected.as_bytes());
        assert_eq!(cursor.has_unverified_leap(), unverified);
    }
}
#[test]
fn composed_date_charges_equal_separate_selection_parsing_and_rendering() {
    let source = b"Date: 1 Jan 2000 00:00 +0000\nDate: 31 Dec 2020 23:59:60 +0000\nDate: bad\n\n";
    let mut reference_work = work(1000);
    let mut reference_budget = HeaderBudget::new();
    let mut selector = header_select::Cursor::new(
        source,
        0,
        source.len() as u64,
        property("header:Date:asDate:all"),
        header_select::SourceEnd::Eof,
    );
    let mut fields = Vec::new();
    for _ in 0..1000 {
        match selector
            .poll_with_budget(Tick(1), &mut reference_work, &mut reference_budget)
            .unwrap()
        {
            header_select::Status::Match(field) => fields.push(field),
            header_select::Status::Complete(_) => break,
            header_select::Status::Yield => {}
        }
    }
    assert_eq!(fields.len(), 3);
    for field in fields {
        let mut cursor = header_date::Budgeted::new(
            &source[field.value_start as usize..field.value_end as usize],
            &mut reference_work,
            &mut reference_budget,
        );
        let date = loop {
            if let header_date::Status::Complete(date) = cursor.poll(Tick(1)).unwrap() {
                break date;
            }
        };
        if let Some(date) = date {
            header_date::project::render_with_budget(
                date,
                &mut [0; 25],
                Tick(1),
                &mut reference_work,
                &mut reference_budget,
            )
            .unwrap();
        }
    }
    // Array punctuation, two quotes, and two null literals.
    reference_work
        .charge(
            Tick(1),
            Charge {
                output_bytes: 14,
                ..Charge::default()
            },
        )
        .unwrap();
    let mut work = work(1000);
    let mut budget = HeaderBudget::new();
    let mut cursor = Date::new(
        input(source, "header:Date:asDate:all"),
        &mut work,
        &mut budget,
    )
    .unwrap();
    let (bytes, result) = drain(&mut cursor, 1);
    result.unwrap();
    assert_eq!(bytes, b"[\"2000-01-01T00:00:00Z\",null,null]");
    assert_eq!(work.remaining(), reference_work.remaining());
    assert_eq!(
        budget.source_bytes_remaining(),
        reference_budget.source_bytes_remaining()
    );
    assert_eq!(budget.steps_remaining(), reference_budget.steps_remaining());
}
#[test]
fn date_output_refusal_never_exposes_a_partial_timestamp() {
    let source = b"Date: 1 Jan 2000 00:00 +0000\n\n";
    for capacity in 0..22 {
        let mut work = work(capacity);
        let mut budget = HeaderBudget::new();
        let mut cursor =
            Date::new(input(source, "header:Date:asDate"), &mut work, &mut budget).unwrap();
        let (bytes, result) = drain(&mut cursor, 1);
        assert!(bytes.is_empty());
        let expected = if capacity < 20 {
            Error::DateProjection(header_date::project::Error::Work(Stop::OutputBytes))
        } else {
            Error::Work(Stop::OutputBytes)
        };
        assert_eq!(result, Err(expected));
    }
    let mut work = work(23);
    let mut budget = HeaderBudget::new();
    let mut cursor = Date::new(
        input(
            b"Date: 1 Jan 2000 00:00 +0000\nDate: bad\n\n",
            "header:Date:asDate:all",
        ),
        &mut work,
        &mut budget,
    )
    .unwrap();
    let (bytes, result) = drain(&mut cursor, 1);
    assert_eq!(bytes, b"[\"2000-01-01T00:00:00Z\"");
    assert_eq!(
        result,
        Err(Error::Date(header_date::Error::Work(Stop::OutputBytes)))
    );
}
#[test]
fn date_late_selection_and_final_deadline_refusals_retire_output() {
    let first = b"Date: 1 Jan 2000 00:00 +0000\n";
    let source = [first.as_slice(), b"Other: long\n\n"].concat();
    let mut work = work(1000);
    let mut budget = HeaderBudget::new();
    let mut spec = input(&source, "header:Date:asDate:all");
    spec.header_limit = first.len() as u64 + 1;
    let mut cursor = Date::new(spec, &mut work, &mut budget).unwrap();
    let (bytes, result) = drain(&mut cursor, 1);
    assert_eq!(bytes, b"[\"2000-01-01T00:00:00Z\"");
    assert_eq!(
        result,
        Err(Error::Selection(header_select::Error::Headers(
            crate::mime_headers::Error::HeaderLimit
        )))
    );
    let mut work = self::work(1000);
    let mut budget = HeaderBudget::new();
    let mut cursor = Date::new(input(first, "header:Date:asDate"), &mut work, &mut budget).unwrap();
    assert_eq!(
        cursor.poll(Tick(1), &mut []),
        Ok(Progress {
            written: 0,
            status: Status::NeedOutput
        })
    );
    drain(&mut cursor, 1).1.unwrap();
    assert_eq!(
        cursor.check_deadline(Tick(100)),
        Err(Error::Work(Stop::Deadline))
    );
    assert_eq!(
        cursor.poll(Tick(1), &mut [0]),
        Err(Error::Work(Stop::Deadline))
    );
    let mut work = self::work(1000);
    let mut budget = HeaderBudget::new();
    assert!(matches!(
        Date::new(input(first, "header:Date"), &mut work, &mut budget),
        Err(Error::UnsupportedForm)
    ));
    let source = format!("Date:{}{}", "(".repeat(33), ")".repeat(33));
    let mut work = self::work(1000);
    let mut budget = HeaderBudget::new();
    let mut cursor = Date::new(
        input(source.as_bytes(), "header:Date:asDate"),
        &mut work,
        &mut budget,
    )
    .unwrap();
    let (bytes, result) = drain(&mut cursor, 1);
    assert!(bytes.is_empty());
    assert_eq!(result, Err(Error::Date(header_date::Error::NestingLimit)));
}
#[test]
fn date_handoffs_refuse_partial_and_failed_owners() {
    let mut work = work(1000);
    let mut budget = HeaderBudget::new();
    let cursor = header_date::Budgeted::new(b"1 Jan 2000 00:00 +0000", &mut work, &mut budget);
    assert!(matches!(
        cursor.finish(),
        Err(header_date::Error::InvalidState)
    ));
    let mut cursor = header_date::Budgeted::new(b"1 Jan 2000 00:00 +0000", &mut work, &mut budget);
    assert_eq!(cursor.poll(Tick(1)), Ok(header_date::Status::Yield));
    assert!(matches!(
        cursor.finish(),
        Err(header_date::Error::InvalidState)
    ));
    let mut cursor = header_date::Budgeted::new(b"", &mut work, &mut budget);
    let mut complete = false;
    for _ in 0..100 {
        if matches!(
            cursor.poll(Tick(1)).unwrap(),
            header_date::Status::Complete(_)
        ) {
            complete = true;
            break;
        }
    }
    assert!(complete);
    assert_eq!(
        cursor.charge_output(Tick(1), 1001),
        Err(header_date::Error::Work(Stop::OutputBytes))
    );
    assert!(matches!(
        cursor.finish(),
        Err(header_date::Error::Work(Stop::OutputBytes))
    ));
    let mut work = self::work(1000);
    let mut source = DateMode::start(b"1 Jan 2000 00:00 +0000", &mut work, &mut budget, ());
    let mut frame = Frame::new();
    let mut complete = false;
    for _ in 0..1000 {
        if DateMode::poll(&mut source, &mut frame, Tick(1), &mut [0; 1])
            .unwrap()
            .status
            == json_string::Status::Complete
        {
            complete = true;
            break;
        }
    }
    assert!(complete);
    assert_eq!(
        DateMode::charge_output(&mut source, Tick(100), 0),
        Err(Error::Work(Stop::Deadline))
    );
    assert!(matches!(
        DateMode::finish(source),
        Err(Error::Work(Stop::Deadline))
    ));
}

#[test]
fn date_aggregate_refusal_never_becomes_successful_null() {
    for source in [
        b"Date: 1 Jan 2000 00:00 +0000\n\n".as_slice(),
        b"Date: 1 Jan 2000 00:00 +0000 (bad\n\n",
        b"Date: 31 Dec 2020 23:59:60 +0000\n\n",
    ] {
        let mut work = work(1000);
        let mut budget = HeaderBudget::new();
        let mut cursor =
            Date::new(input(source, "header:Date:asDate"), &mut work, &mut budget).unwrap();
        let (expected, result) = drain(&mut cursor, 1);
        result.unwrap();
        let steps = 16_000_000 - budget.steps_remaining();
        let mut late_refusal = false;
        for cut in 0..steps {
            let mut work = self::work(1000);
            let mut budget = HeaderBudget::new();
            budget
                .charge(&mut self::work(1000), Tick(1), 0, 16_000_000 - cut, &mut 0)
                .unwrap();
            let mut cursor =
                Date::new(input(source, "header:Date:asDate"), &mut work, &mut budget).unwrap();
            let (bytes, result) = drain(&mut cursor, 1);
            assert!(expected.starts_with(&bytes));
            late_refusal |= bytes == expected;
            assert!(matches!(
                result,
                Err(Error::InterpretationLimit
                    | Error::Selection(header_select::Error::InterpretationLimit)
                    | Error::Date(header_date::Error::InterpretationLimit)
                    | Error::DateProjection(header_date::project::Error::InterpretationLimit))
            ));
            assert_eq!(work.stopped(), None);
        }
        assert!(
            late_refusal,
            "final selection must still admit provisional output"
        );
    }
}

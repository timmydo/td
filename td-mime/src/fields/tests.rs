#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;
use crate::time::Deadline;
fn work() -> Meter {
    Meter::new(
        Deadline::after(Tick(0), 100).unwrap(),
        Charge {
            io_bytes: 100_000_000,
            records: 2_000_000,
            ..Charge::default()
        },
    )
}
fn parsed(source: &[u8], kind: Kind) -> Result<(Head, Vec<Parameter>), Error> {
    let mut cursor = Cursor::new(source, kind);
    assert!(std::mem::size_of_val(&cursor) <= 512);
    let mut work = work();
    let mut head = None;
    let mut parameters = Vec::new();
    for _ in 0..100_000 {
        let before = work.remaining();
        let status = cursor.poll(Tick(1), &mut work)?;
        assert!(before.io_bytes - work.remaining().io_bytes <= 160);
        assert!(before.records - work.remaining().records <= 32);
        match status {
            Status::Yield => {}
            Status::Head(value) => {
                assert!(head.replace(value).is_none());
            }
            Status::Parameter(value) => parameters.push(value),
            Status::Complete => {
                let before = work.remaining();
                assert_eq!(cursor.poll(Tick(100), &mut work), Ok(Status::Complete));
                assert_eq!(work.remaining(), before);
                return Ok((head.unwrap(), parameters));
            }
        }
    }
    panic!("MIME field did not finish")
}
#[test]
fn content_type_parameters_keep_raw_source_extents_and_wire_order() {
    let source = concat!(
        " (note) TeXt (x)/ (y) PLAIN; charset (z) = utf-8; ",
        "name=\"a\\\"b\r\n c\"; filename*1*=b%20; filename*0*=utf-8''a"
    )
    .as_bytes();
    let (head, parameters) = parsed(source, Kind::ContentType).unwrap();
    assert_eq!(
        source.get(head.first.start..head.first.end),
        Some(b"TeXt".as_slice())
    );
    let second = head.second.unwrap();
    assert_eq!(
        source.get(second.start..second.end),
        Some(b"PLAIN".as_slice())
    );
    let expected = [
        (b"charset".as_slice(), b"utf-8".as_slice(), false),
        (b"name", b"\"a\\\"b\r\n c\"", true),
        (b"filename*1*", b"b%20", false),
        (b"filename*0*", b"utf-8''a", false),
    ];
    assert_eq!(parameters.len(), expected.len());
    for (actual, (name, value, quoted)) in parameters.iter().zip(expected) {
        assert_eq!(source.get(actual.name.start..actual.name.end), Some(name));
        assert_eq!(
            source.get(actual.value.start..actual.value.end),
            Some(value)
        );
        assert_eq!(actual.quoted, quoted);
    }
}
#[test]
fn disposition_transfer_tokens_and_empty_quoted_values_are_complete_syntax() {
    for (kind, source, params) in [
        (
            Kind::ContentDisposition,
            b" attachment; filename=\"\"; filename=second".as_slice(),
            2,
        ),
        (Kind::TransferEncoding, b"(note) BaSe64 (tail)", 0),
        (Kind::ContentDisposition, b"x-custom (note)", 0),
    ] {
        let (head, parameters) = parsed(source, kind).unwrap();
        assert!(head.second.is_none());
        assert_eq!(parameters.len(), params);
    }
    parsed("text/plain; name=\"café\"".as_bytes(), Kind::ContentType).unwrap();
}
#[test]
fn incomplete_or_malformed_fields_never_report_complete() {
    for source in [
        b"".as_slice(),
        b"text",
        b"text/",
        b"text/plain junk",
        b"text/plain;",
        b"text/plain; name",
        b"text/plain; name=",
        b"text/plain; name=\"open",
        b"text/plain; name=\"closed\"junk",
        b"text/plain; name=a b",
        b"text/plain; name=\xff",
        b"text/plain; name=\"caf\xe9\"",
        b"text/plain; name=\"caf\\\xe9\"",
        b"text/plain (\xe9)",
        b"text/plain (\\\xe9)",
        b"text/plain (open",
        b"text/plain\rbroken",
        b"text/plain; name=\"a\0b\"",
        b"text/plain; name=\"a\\\"",
        b"text/plain; =x",
    ] {
        assert_eq!(
            parsed(source, Kind::ContentType),
            Err(Error::Malformed),
            "{source:?}"
        );
    }
    assert_eq!(
        parsed(b"base64; x=y", Kind::TransferEncoding),
        Err(Error::Malformed)
    );
}
#[test]
fn comments_enforce_depth_and_late_failures_retire_provisional_events() {
    let source = format!("text/plain {}x{}", "(".repeat(32), ")".repeat(32));
    parsed(source.as_bytes(), Kind::ContentType).unwrap();
    let source = format!("text/plain {}x{}", "(".repeat(33), ")".repeat(33));
    assert_eq!(
        parsed(source.as_bytes(), Kind::ContentType),
        Err(Error::NestingLimit)
    );
    let mut cursor = Cursor::new(b"text/plain; good=one; bad=", Kind::ContentType);
    let mut work = work();
    let mut head = false;
    let mut parameter = false;
    let mut failed = false;
    for _ in 0..1000 {
        match cursor.poll(Tick(1), &mut work) {
            Ok(Status::Head(_)) => head = true,
            Ok(Status::Parameter(_)) => parameter = true,
            Ok(Status::Yield) => {}
            Ok(Status::Complete) => panic!("accepted malformed suffix"),
            Err(error) => {
                assert_eq!(error, Error::Malformed);
                let mut fresh = self::work();
                let before = fresh.remaining();
                assert_eq!(cursor.poll(Tick(1), &mut fresh), Err(error));
                assert_eq!(fresh.remaining(), before);
                failed = true;
                break;
            }
        }
    }
    assert!(head && parameter && failed);
}
#[test]
fn original_aggregate_and_job_admission_retire_the_parser() {
    for job in [true, false] {
        let mut work = if job {
            Meter::new(Deadline::after(Tick(0), 100).unwrap(), Charge::default())
        } else {
            work()
        };
        let mut budget = crate::nfc::HeaderBudget::new();
        if !job {
            budget
                .charge_local(
                    &mut self::work(),
                    Tick(1),
                    budget.source_bytes_remaining(),
                    0,
                    &mut 0,
                )
                .unwrap();
        }
        let mut cursor = Budgeted::new(
            b"text/plain; name=x",
            Kind::ContentType,
            &mut work,
            &mut budget,
        );
        let mut failed = false;
        for _ in 0..100 {
            match cursor.poll(Tick(1)) {
                Ok(Status::Yield) => {}
                Ok(_) => panic!("exhausted parser emitted event"),
                Err(error) => {
                    assert_eq!(
                        error,
                        if job {
                            Error::Work(Stop::Records)
                        } else {
                            Error::InterpretationLimit
                        }
                    );
                    assert_eq!(cursor.poll(Tick(1)), Err(error));
                    failed = true;
                    break;
                }
            }
        }
        assert!(failed);
    }
}

fn limited(bytes: u64, steps: u64) -> crate::nfc::HeaderBudget {
    let mut budget = crate::nfc::HeaderBudget::new();
    budget
        .charge_local(
            &mut work(),
            Tick(1),
            budget.source_bytes_remaining() - bytes,
            budget.steps_remaining() - steps,
            &mut 0,
        )
        .unwrap();
    budget
}
fn drain(cursor: &mut Budgeted<'_, '_>) -> (Vec<Status>, Result<(), Error>) {
    assert!(std::mem::size_of_val(cursor) <= 512);
    let mut events = Vec::new();
    for _ in 0..100_000 {
        let before = cursor.work.remaining();
        let steps = cursor.budget.steps_remaining();
        let status = cursor.poll(Tick(1));
        let after = cursor.work.remaining();
        assert!(before.io_bytes - after.io_bytes <= 160);
        assert!(before.records - after.records <= 13);
        assert!(steps - cursor.budget.steps_remaining() <= 193);
        assert_eq!(before.output_bytes, after.output_bytes);
        match status {
            Ok(Status::Yield) => {}
            Ok(Status::Complete) => {
                assert_eq!(cursor.poll(Tick(100)), Ok(Status::Complete));
                assert_eq!(cursor.work.remaining(), after);
                return (events, Ok(()));
            }
            Ok(event) => events.push(event),
            Err(error) => return (events, Err(error)),
        }
    }
    panic!("budgeted MIME field did not finish")
}
#[test]
fn long_fields_keep_plain_and_budgeted_events_with_charged_visits() {
    let long = format!(
        "({0})text/long{1}; name=\"{0}\"; x={1}",
        "🐈".repeat(4096),
        "a".repeat(32_768)
    );
    for source in [
        b"text/plain".as_slice(),
        b"text/plain; x=y; x*=utf-8''z",
        long.as_bytes(),
        b"text/plain; x=\"bad",
    ] {
        let mut cursor = Cursor::new(source, Kind::ContentType);
        let mut plain_work = work();
        let mut expected = Vec::new();
        let result = loop {
            match cursor.poll(Tick(1), &mut plain_work) {
                Ok(Status::Yield) => {}
                Ok(Status::Complete) => break Ok(()),
                Ok(event) => expected.push(event),
                Err(error) => break Err(error),
            }
        };
        let mut work = work();
        let mut budget = crate::nfc::HeaderBudget::new();
        assert_eq!(
            drain(&mut Budgeted::new(
                source,
                Kind::ContentType,
                &mut work,
                &mut budget
            )),
            (expected, result)
        );
        let visits = 100_000_000 - work.remaining().io_bytes;
        assert_eq!(work.remaining().io_bytes, plain_work.remaining().io_bytes);
        assert_eq!(16 * 1024 * 1024 - budget.source_bytes_remaining(), visits);
        assert_eq!(
            2_000_000 - work.remaining().records,
            (16_000_000 - budget.steps_remaining()).div_ceil(16)
        );
    }
}
#[test]
fn every_partial_aggregate_budget_retires_provisional_parameters() {
    let mut late = false;
    for source in [
        b"text/plain; x=y; z=\"hi\"".as_slice(),
        b"text/plain; x=y; z=",
        "text/plain; x=\"é\"".as_bytes(),
        b"text/plain (bad",
    ] {
        let mut work = work();
        let mut budget = crate::nfc::HeaderBudget::new();
        let expected = drain(&mut Budgeted::new(
            source,
            Kind::ContentType,
            &mut work,
            &mut budget,
        ));
        let visits = 16 * 1024 * 1024 - budget.source_bytes_remaining();
        let steps = 16_000_000 - budget.steps_remaining();
        for (bytes, steps) in (0..visits)
            .map(|cut| (cut, steps))
            .chain((0..steps).map(|cut| (visits, cut)))
        {
            let mut work = self::work();
            let mut budget = limited(bytes, steps);
            let mut cursor = Budgeted::new(source, Kind::ContentType, &mut work, &mut budget);
            let (events, result) = drain(&mut cursor);
            assert_eq!(result, Err(Error::InterpretationLimit));
            assert!(expected.0.starts_with(&events));
            late |= events
                .iter()
                .any(|event| matches!(event, Status::Parameter(_)));
            let before = cursor.work.remaining();
            assert_eq!(cursor.check_deadline(Tick(1)), result);
            assert_eq!(cursor.poll(Tick(1)), result.map(|_| Status::Complete));
            assert_eq!(cursor.work.remaining(), before);
            assert_eq!(work.stopped(), None);
        }
        let mut work = self::work();
        let mut budget = limited(visits, steps);
        assert_eq!(
            drain(&mut Budgeted::new(
                source,
                Kind::ContentType,
                &mut work,
                &mut budget
            )),
            expected
        );
    }
    assert!(late);
}
#[test]
fn clock_and_job_refusals_latch_without_advancing_and_retire_completion() {
    for (io_bytes, records, now, stop) in [
        (0, 100, 1, Stop::IoBytes),
        (100, 0, 1, Stop::Records),
        (100, 100, 100, Stop::Deadline),
    ] {
        let mut work = Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes,
                records,
                ..Charge::default()
            },
        );
        let mut budget = crate::nfc::HeaderBudget::new();
        let mut cursor = Budgeted::new(b"text/plain", Kind::ContentType, &mut work, &mut budget);
        assert_eq!(cursor.poll(Tick(now)), Err(Error::Work(stop)));
        assert_eq!(cursor.cursor.position, 0);
        assert_eq!(cursor.poll(Tick(1)), Err(Error::Work(stop)));
    }
    let mut work = work();
    let mut budget = crate::nfc::HeaderBudget::new();
    let mut cursor = Budgeted::new(b"text/plain", Kind::ContentType, &mut work, &mut budget);
    assert_eq!(drain(&mut cursor).1, Ok(()));
    let before = cursor.work.remaining();
    cursor.check_deadline(Tick(1)).unwrap();
    assert_eq!(cursor.work.remaining(), before);
    assert_eq!(
        cursor.check_deadline(Tick(100)),
        Err(Error::Work(Stop::Deadline))
    );
    assert_eq!(cursor.poll(Tick(1)), Err(Error::Work(Stop::Deadline)));
}

#[test]
fn ascii_token_alphabet_is_exact_and_parameter_encoding_is_opaque() {
    for byte in 0..=255u8 {
        let source = [byte];
        let expected =
            b"!#$%&'*+-.0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ^_`abcdefghijklmnopqrstuvwxyz{|}~"
                .contains(&byte);
        assert_eq!(
            parsed(&source, Kind::TransferEncoding).is_ok(),
            expected,
            "{byte}"
        );
    }
    let source = b"x/custom; unknown*=unsupported'zz'bad%GG; name=\"=?utf-8?Q?name?=\"";
    let (_, values) = parsed(source, Kind::ContentType).unwrap();
    assert_eq!(values.len(), 2);
    assert_eq!(
        source.get(values[0].value.start..values[0].value.end),
        Some(b"unsupported'zz'bad%GG".as_slice())
    );
    assert_eq!(
        source.get(values[1].value.start..values[1].value.end),
        Some(b"\"=?utf-8?Q?name?=\"".as_slice())
    );
}

#[test]
fn first_completion_is_active_even_after_the_final_extent_event() {
    for source in [b"text/plain".as_slice(), b"text/plain; x=y"] {
        let final_parameter = source.contains(&b';');
        let mut work = work();
        let mut cursor = Cursor::new(source, Kind::ContentType);
        loop {
            let event = cursor.poll(Tick(1), &mut work).unwrap();
            if matches!(event, Status::Parameter(_))
                || (!final_parameter && matches!(event, Status::Head(_)))
            {
                break;
            }
        }
        assert_eq!(
            cursor.poll(Tick(100), &mut work),
            Err(Error::Work(Stop::Deadline))
        );
        assert_eq!(
            cursor.poll(Tick(1), &mut self::work()),
            Err(Error::Work(Stop::Deadline))
        );
        let mut work = self::work();
        let mut budget = crate::nfc::HeaderBudget::new();
        let mut cursor = Budgeted::new(source, Kind::ContentType, &mut work, &mut budget);
        loop {
            let event = cursor.poll(Tick(1)).unwrap();
            if matches!(event, Status::Parameter(_))
                || (!final_parameter && matches!(event, Status::Head(_)))
            {
                break;
            }
        }
        assert_eq!(cursor.poll(Tick(100)), Err(Error::Work(Stop::Deadline)));
        assert_eq!(cursor.poll(Tick(1)), Err(Error::Work(Stop::Deadline)));
    }
}

#[test]
fn folded_parameter_gaps_and_kind_specific_refusals_are_pinned() {
    let source = b"text/plain;\r\n\tcharset=x;\n filename=\"y\"";
    let (_, parameters) = parsed(source, Kind::ContentType).unwrap();
    assert_eq!(parameters.len(), 2);
    for (source, kind) in [
        (b"attachment/x".as_slice(), Kind::ContentDisposition),
        (b"base64/x", Kind::TransferEncoding),
        (b"\"attachment\"", Kind::ContentDisposition),
        (b"\"base64\"", Kind::TransferEncoding),
        (b"text/plain;; x=y", Kind::ContentType),
        (b"text/html; charset=UTF-8;", Kind::ContentType),
        (
            b"application/pdf; name=Rechnung M\xc3\xa4rz.pdf",
            Kind::ContentType,
        ),
    ] {
        assert_eq!(parsed(source, kind), Err(Error::Malformed), "{source:?}");
    }
}
#[test]
fn every_partial_job_budget_retires_provisional_parameters() {
    let source = b"text/plain; x=y; z=\"a\r\n b\" (tail)";
    let mut work = work();
    let mut budget = crate::nfc::HeaderBudget::new();
    let expected = drain(&mut Budgeted::new(
        source,
        Kind::ContentType,
        &mut work,
        &mut budget,
    ));
    assert_eq!(expected.1, Ok(()));
    let visits = 100_000_000 - work.remaining().io_bytes;
    let records = 2_000_000 - work.remaining().records;
    let mut late = false;
    for (io_bytes, records, stop) in (0..visits)
        .map(|cut| (cut, records, Stop::IoBytes))
        .chain((0..records).map(|cut| (visits, cut, Stop::Records)))
    {
        let mut work = Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes,
                records,
                ..Charge::default()
            },
        );
        let mut budget = crate::nfc::HeaderBudget::new();
        let mut cursor = Budgeted::new(source, Kind::ContentType, &mut work, &mut budget);
        let (events, result) = drain(&mut cursor);
        assert_eq!(result, Err(Error::Work(stop)));
        assert!(expected.0.starts_with(&events));
        late |= events
            .iter()
            .any(|event| matches!(event, Status::Parameter(_)));
        let before = cursor.work.remaining();
        assert_eq!(cursor.check_deadline(Tick(1)), result);
        assert_eq!(cursor.poll(Tick(1)), Err(Error::Work(stop)));
        assert_eq!(cursor.work.remaining(), before);
        assert_eq!(
            16 * 1024 * 1024 - cursor.budget.source_bytes_remaining(),
            io_bytes - before.io_bytes
        );
        assert_eq!(work.stopped(), Some(stop));
    }
    assert!(late);
}

#![allow(clippy::unwrap_used, clippy::panic)]
use super::*;
use crate::{admission::work::Charge, ports::Deadline};
fn meter() -> Meter {
    Meter::new(
        Deadline::after(Tick(0), 100).unwrap(),
        Charge {
            io_bytes: 100_000_000,
            records: 100_000_000,
            output_bytes: 100_000_000,
            ..Charge::default()
        },
    )
}
fn input(source: &[u8]) -> Input<'_> {
    Input {
        source,
        base: 100,
        header_limit: 1_000_000,
        source_end: SourceEnd::Eof,
    }
}
fn limited(bytes: u64, steps: u64) -> HeaderBudget {
    let mut budget = HeaderBudget::new();
    budget
        .charge(
            &mut meter(),
            Tick(1),
            budget.source_bytes_remaining() - bytes,
            budget.steps_remaining() - steps,
            &mut 0,
        )
        .unwrap();
    budget
}
fn drain(cursor: &mut Cursor<'_, '_>) -> Result<usize, Error> {
    for turn in 1..100_000 {
        let before = cursor.work.remaining();
        let bytes = cursor.budget.source_bytes_remaining();
        let steps = cursor.budget.steps_remaining();
        let status = cursor.poll(Tick(1));
        let after = cursor.work.remaining();
        assert!(before.io_bytes - after.io_bytes <= 256);
        assert!(before.records - after.records <= 16);
        assert!(steps - cursor.budget.steps_remaining() <= 256);
        assert_eq!(
            bytes - cursor.budget.source_bytes_remaining(),
            before.io_bytes - after.io_bytes
        );
        assert_eq!(before.output_bytes, after.output_bytes);
        match status {
            Ok(Status::Yield) => assert_eq!(cursor.selection(), None),
            Ok(Status::Complete) => {
                assert!(cursor.selection().is_some());
                assert_eq!(cursor.poll(Tick(100)), Ok(Status::Complete));
                assert_eq!(cursor.work.remaining(), after);
                return Ok(turn);
            }
            Err(error) => {
                assert_eq!(cursor.selection(), None);
                assert_eq!(cursor.poll(Tick(1)), Err(error));
                assert_eq!(cursor.check_deadline(Tick(1)), Err(error));
                assert_eq!(cursor.work.remaining(), after);
                return Err(error);
            }
        }
    }
    panic!("label selection did not finish")
}
fn value(source: &[u8], field: Field) -> &[u8] {
    td_header::resident::slice(source, 100, field.value_start..field.value_end).unwrap()
}
#[test]
fn first_valid_complete_values_and_original_owner_projection() {
    let source = concat!(
        "Content-ID: <local>\r\n",
        "Content-ID: <a@b> <c@d>\r\n",
        "cOnTeNt-Id \t: (🐈) <A@B> (y)\r\n",
        "Content-ID: <late@id>\r\n",
        "Content-Language: en,,FR\r\n",
        "CONTENT-LANGUAGE: (x) en-GB,\r\n",
        "\tFR (y)\r\n",
        "Content-Language: de\r\n",
        "\r\n",
        "ignored",
    )
    .as_bytes();
    let mut work = meter();
    let mut budget = HeaderBudget::new();
    let wp = &work as *const Meter;
    let bp = &budget as *const HeaderBudget;
    let mut cursor = Cursor::new(input(source), &mut work, &mut budget).unwrap();
    drain(&mut cursor).unwrap();
    let (work, budget, selected) = cursor.finish(Tick(1)).unwrap();
    assert!(std::ptr::eq(work, wp));
    assert!(std::ptr::eq(budget, bp));
    let cid = value(source, selected.content_id.unwrap());
    let language = value(source, selected.content_language.unwrap());
    assert_eq!(cid, " (🐈) <A@B> (y)".as_bytes());
    assert_eq!(language, b" (x) en-GB,\r\n\tFR (y)");
    assert_eq!(selected.end.body_start, 100 + source.len() as u64 - 7);
    let mut cursor = crate::mime_content_id::Cursor::new(cid, work, budget);
    let mut text = String::new();
    for _ in 0..1000 {
        match cursor.poll(Tick(1)).unwrap() {
            crate::mime_content_id::Status::Scalar(c) => text.push(c),
            crate::mime_content_id::Status::Complete => break,
            _ => {}
        }
    }
    assert_eq!(text, "A@B");
    let (work, budget) = cursor.finish(Tick(1)).unwrap();
    let mut cursor = crate::mime_language::Cursor::new(language, work, budget);
    let mut tags = Vec::new();
    for _ in 0..1000 {
        match cursor.poll(Tick(1)).unwrap() {
            crate::mime_language::Status::Tag(e) => {
                tags.push(language.get(e.start..e.end).unwrap())
            }
            crate::mime_language::Status::Complete => break,
            _ => {}
        }
    }
    assert_eq!(tags, vec![b"en-GB".as_slice(), b"FR"]);
    let (work, budget) = cursor.finish(Tick(1)).unwrap();
    assert!(std::ptr::eq(work, wp));
    assert!(std::ptr::eq(budget, bp));
}
#[test]
fn exact_absolute_extents_and_body_boundaries() {
    let source = b"Content-ID: <A@b>\r\nContent-Language: en, FR\r\n\r\nbody";
    let mut work = meter();
    let mut budget = HeaderBudget::new();
    let mut cursor = Cursor::new(input(source), &mut work, &mut budget).unwrap();
    drain(&mut cursor).unwrap();
    let (_, _, selected) = cursor.finish(Tick(1)).unwrap();
    assert_eq!(
        selected,
        Selection {
            content_id: Some(Field {
                name_start: 100,
                name_end: 110,
                value_start: 111,
                value_end: 117
            }),
            content_language: Some(Field {
                name_start: 119,
                name_end: 135,
                value_start: 136,
                value_end: 143
            }),
            end: End {
                body_start: 147,
                header_bytes: 45
            }
        }
    );
    for source in [
        b"".as_slice(),
        b"Unknown: x\r\n\r\nContent-ID: <a@b>",
        b" garbage\nContent-ID: <a@b>",
        b"Content-ID: <bad>\r\nContent-Language: en,\r\n\r\n",
    ] {
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut cursor = Cursor::new(input(source), &mut work, &mut budget).unwrap();
        drain(&mut cursor).unwrap();
        let (_, _, s) = cursor.finish(Tick(1)).unwrap();
        assert_eq!(s.content_id, None);
        assert_eq!(s.content_language, None);
    }
    let source = b"Content-ID: <a@b>\r\nbroken line\nContent-Language: en\n";
    let mut work = meter();
    let mut budget = HeaderBudget::new();
    let mut cursor = Cursor::new(input(source), &mut work, &mut budget).unwrap();
    drain(&mut cursor).unwrap();
    let (_, _, s) = cursor.finish(Tick(1)).unwrap();
    assert!(s.content_id.is_some());
    assert_eq!(s.content_language, None);
    assert_eq!(s.end.body_start, 119);
}
#[test]
fn source_completeness_structural_limits_and_duplicate_skipping() {
    let source = b"Content-ID: <a@b>\r\nContent-Language: en\r\n";
    let mut work = meter();
    let mut budget = HeaderBudget::new();
    let mut i = input(source);
    i.source_end = SourceEnd::Prefix;
    let mut cursor = Cursor::new(i, &mut work, &mut budget).unwrap();
    assert_eq!(drain(&mut cursor), Err(Error::Truncated));
    assert_eq!(cursor.finish(Tick(1)).err(), Some(Error::Truncated));
    let complete = b"Content-ID: <a@b>\r\n\r\n";
    let mut i = input(complete);
    i.source_end = SourceEnd::Prefix;
    i.header_limit = 19;
    let mut cursor = Cursor::new(i, &mut work, &mut budget).unwrap();
    drain(&mut cursor).unwrap();
    cursor.finish(Tick(1)).unwrap();
    for limit in 0..19 {
        let mut i = input(complete);
        i.header_limit = limit;
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut cursor = Cursor::new(i, &mut work, &mut budget).unwrap();
        assert_eq!(
            drain(&mut cursor),
            Err(Error::Headers(mime_headers::Error::HeaderLimit))
        );
    }
    let deep = format!("{}x{}", "(".repeat(33), ")".repeat(33));
    for name in ["Content-ID", "Content-Language"] {
        let source = format!("{name}: {deep}\r\n\r\n");
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut cursor = Cursor::new(input(source.as_bytes()), &mut work, &mut budget).unwrap();
        assert_eq!(drain(&mut cursor), Err(Error::NestingLimit));
        assert_eq!(cursor.finish(Tick(1)).err(), Some(Error::NestingLimit));
    }
    for name in ["Content-ID", "Content-Language"] {
        let source = format!("Content-ID: <a@b>\r\n{name}: {deep}\r\n\r\n");
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut cursor = Cursor::new(input(source.as_bytes()), &mut work, &mut budget).unwrap();
        let result = drain(&mut cursor);
        if name == "Content-ID" {
            assert!(result.is_ok());
            cursor.finish(Tick(1)).unwrap();
        } else {
            assert_eq!(result, Err(Error::NestingLimit));
            assert_eq!(cursor.finish(Tick(1)).err(), Some(Error::NestingLimit));
        }
    }
    let mut i = input(b"x");
    i.base = u64::MAX;
    assert_eq!(
        Cursor::new(i, &mut meter(), &mut HeaderBudget::new()).err(),
        Some(Error::InvalidRange)
    );
}
#[test]
fn every_original_allowance_cut_and_exact_successful_grants() {
    let source = concat!(
        "Content-ID: <bad>\r\n",
        "Content-ID: <a@b>\r\n",
        "Content-Language: en,\r\n",
        "Content-Language: FR\r\n",
        "\r\n",
        "body",
    )
    .as_bytes();
    let mut work = meter();
    let initial = work.remaining();
    let mut budget = HeaderBudget::new();
    let initial_steps = budget.steps_remaining();
    let mut cursor = Cursor::new(input(source), &mut work, &mut budget).unwrap();
    drain(&mut cursor).unwrap();
    let (_, _, expected) = cursor.finish(Tick(1)).unwrap();
    let bytes = initial.io_bytes - work.remaining().io_bytes;
    let records = initial.records - work.remaining().records;
    let steps = initial_steps - budget.steps_remaining();
    assert_eq!(value(source, expected.content_id.unwrap()), b" <a@b>");
    assert_eq!(value(source, expected.content_language.unwrap()), b" FR");
    assert_eq!(records, steps.div_ceil(16));
    for (b, s) in (0..bytes)
        .map(|cut| (cut, steps))
        .chain((0..steps).map(|cut| (bytes, cut)))
    {
        let mut work = meter();
        let mut budget = limited(b, s);
        let mut cursor = Cursor::new(input(source), &mut work, &mut budget).unwrap();
        assert_eq!(drain(&mut cursor), Err(Error::InterpretationLimit));
        assert_eq!(
            cursor.finish(Tick(1)).err(),
            Some(Error::InterpretationLimit)
        );
        assert_eq!(work.stopped(), None);
        assert_eq!(
            Cursor::new(input(b""), &mut work, &mut budget)
                .unwrap()
                .poll(Tick(1)),
            Err(Error::InterpretationLimit)
        );
    }
    for (b, r, stop) in (0..bytes)
        .map(|cut| (cut, records, Stop::IoBytes))
        .chain((0..records).map(|cut| (bytes, cut, Stop::Records)))
    {
        let mut work = Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: b,
                records: r,
                ..Charge::default()
            },
        );
        let mut budget = HeaderBudget::new();
        let mut cursor = Cursor::new(input(source), &mut work, &mut budget).unwrap();
        assert_eq!(drain(&mut cursor), Err(Error::Work(stop)));
        assert_eq!(cursor.finish(Tick(1)).err(), Some(Error::Work(stop)));
        assert_eq!(work.stopped(), Some(stop));
        budget.charge(&mut meter(), Tick(1), 0, 0, &mut 0).unwrap();
    }
    let mut work = Meter::new(
        Deadline::after(Tick(0), 100).unwrap(),
        Charge {
            io_bytes: bytes,
            records,
            ..Charge::default()
        },
    );
    let mut budget = limited(bytes, steps);
    let mut cursor = Cursor::new(input(source), &mut work, &mut budget).unwrap();
    drain(&mut cursor).unwrap();
    assert_eq!(cursor.finish(Tick(1)).unwrap().2, expected);
    assert_eq!(work.remaining().io_bytes, 0);
    assert_eq!(work.remaining().records, 0);
    assert_eq!(budget.source_bytes_remaining(), 0);
    assert_eq!(budget.steps_remaining(), 0);
}
#[test]
fn every_live_deadline_and_fresh_handoff_retirement() {
    let source = b"Content-ID: <a@b>\r\nContent-Language: en\r\n\r\n";
    let mut work = meter();
    let mut budget = HeaderBudget::new();
    let mut cursor = Cursor::new(input(source), &mut work, &mut budget).unwrap();
    let turns = drain(&mut cursor).unwrap();
    cursor.finish(Tick(1)).unwrap();
    for cut in 0..turns {
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut cursor = Cursor::new(input(source), &mut work, &mut budget).unwrap();
        for _ in 0..cut {
            assert_eq!(cursor.poll(Tick(1)), Ok(Status::Yield));
        }
        assert_eq!(cursor.finish(Tick(1)).err(), Some(Error::InvalidState));
        for finish in [false, true] {
            let mut work = meter();
            let mut budget = HeaderBudget::new();
            let mut cursor = Cursor::new(input(source), &mut work, &mut budget).unwrap();
            for _ in 0..cut {
                assert_eq!(cursor.poll(Tick(1)), Ok(Status::Yield));
            }
            if finish {
                assert_eq!(
                    cursor.finish(Tick(100)).err(),
                    Some(Error::Work(Stop::Deadline))
                );
            } else {
                assert_eq!(cursor.poll(Tick(100)), Err(Error::Work(Stop::Deadline)));
                assert_eq!(cursor.selection(), None);
                assert_eq!(
                    cursor.finish(Tick(1)).err(),
                    Some(Error::Work(Stop::Deadline))
                );
            }
        }
    }
    for finish in [false, true] {
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut cursor = Cursor::new(input(source), &mut work, &mut budget).unwrap();
        drain(&mut cursor).unwrap();
        if !finish {
            assert_eq!(
                cursor.check_deadline(Tick(100)),
                Err(Error::Work(Stop::Deadline))
            );
            assert_eq!(cursor.selection(), None);
        }
        assert_eq!(
            cursor.finish(Tick(100)).err(),
            Some(Error::Work(Stop::Deadline))
        );
    }
    assert_eq!(
        Cursor::new(input(source), &mut meter(), &mut HeaderBudget::new())
            .unwrap()
            .finish(Tick(1))
            .err(),
        Some(Error::InvalidState)
    );
}

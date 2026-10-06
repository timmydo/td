#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;
use crate::{header_property, header_urls as urls, time::Deadline};
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
    for _ in 0..1000 {
        if let header_property::Status::Complete(value) =
            cursor.poll(Tick(1), &mut work(1000)).unwrap()
        {
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
fn remaining(cursor: &URLs<'_, '_>) -> (Charge, u64) {
    match &cursor.0.owner {
        Owner::Budgets(work, budget, _) => (work.remaining(), budget.steps_remaining()),
        Owner::Value(source) => source.remaining().unwrap(),
        Owner::Retired => panic!("retired coordinator"),
    }
}
fn drain(cursor: &mut URLs<'_, '_>, width: usize) -> (Vec<u8>, Result<End, Error>) {
    assert!(std::mem::size_of_val(cursor) <= 1024);
    let mut bytes = Vec::new();
    let mut output = [0xa5; 8];
    for _ in 0..100_000 {
        output.fill(0xa5);
        let (before, steps) = remaining(cursor);
        let result = cursor.poll(Tick(1), &mut output[..width]);
        let (after, after_steps) = remaining(cursor);
        assert!(before.io_bytes - after.io_bytes <= 255);
        assert!(steps - after_steps <= 256);
        assert!(before.records - after.records <= 16);
        assert!(before.output_bytes - after.output_bytes <= 4);
        match result {
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
    panic!("URLs property did not finish");
}
#[test]
fn url_fields_preserve_last_all_no_and_list_post_authorization() {
    let source = b"List-Post:<mailto:a@b>, <https://EXAMPLE/%2f?x=1#F>\nList-Post:<x:a> (bad\nList-Post:NO\n\n";
    for width in 1..=8 {
        for (key, expected) in [
            ("header:List-Post:asURLs", "[]"),
            (
                "header:lIsT-pOsT:asURLs:all",
                "[[\"mailto:a@b\",\"https://EXAMPLE/%2f?x=1#F\"],null,[]]",
            ),
            ("header:X-Missing:asURLs", "null"),
            ("header:X-Missing:asURLs:all", "[]"),
        ] {
            let mut work = work(1000);
            let mut budget = HeaderBudget::new();
            let mut cursor = URLs::new(input(source, key), &mut work, &mut budget).unwrap();
            let (bytes, result) = drain(&mut cursor, width);
            let end = result.unwrap();
            assert_eq!(bytes, expected.as_bytes());
            assert_eq!(
                cursor.poll(Tick(100), &mut []),
                Ok(Progress {
                    written: 0,
                    status: Status::Complete(end)
                })
            );
            cursor.check_deadline(Tick(1)).unwrap();
        }
    }
    for (name, value, expected) in [
        ("List-Post", "(why) NO (tail)", "[]"),
        ("List-Post", "no", "null"),
        ("List-Help", "NO", "null"),
        ("List-Host", "NO", "null"),
        ("List-Post", "", "null"),
        (
            "List-Help",
            "<x://[::1]>,<x://[v1.a:b]>",
            "[\"x://[::1]\",\"x://[v1.a:b]\"]",
        ),
        ("List-Help", "<x: a\r\n b>", "[\"x:ab\"]"),
        ("List-Help", "<x://[bad]>", "null"),
        ("List-Help", "<x:a>,<bad>", "null"),
        ("List-Help", "<https://x/é>", "null"),
    ] {
        let source = format!("{name}:{value}\n\n");
        let key = format!("header:{name}:asURLs");
        let mut work = work(1000);
        let mut budget = HeaderBudget::new();
        let mut cursor = URLs::new(input(source.as_bytes(), &key), &mut work, &mut budget).unwrap();
        let (bytes, result) = drain(&mut cursor, 1);
        result.unwrap();
        assert_eq!(bytes, expected.as_bytes());
    }
}
#[test]
fn composed_url_charges_equal_independent_selection_parsing_and_json() {
    let source = b"List-Post:<x:a>\nList-Post:NO\nList-Post:bad\n\n";
    let key = "header:List-Post:asURLs:all";
    let mut reference_work = work(1000);
    let mut reference_budget = HeaderBudget::new();
    reference_budget
        .charge_local(&mut reference_work, Tick(1), 9, 1, &mut 0)
        .unwrap();
    let mut selector = header_select::Cursor::new(
        source,
        0,
        source.len() as u64,
        property(key),
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
        let mut cursor = urls::Budgeted::new(
            &source[field.value_start as usize..field.value_end as usize],
            urls::Mode::ListPost,
            &mut reference_work,
            &mut reference_budget,
        );
        loop {
            match cursor.poll(Tick(1)) {
                Ok(urls::Status::Complete) | Err(urls::Error::Malformed) => break,
                Ok(_) => {}
                Err(error) => panic!("{error}"),
            }
        }
    }
    let expected = b"[[\"x:a\"],[],null]";
    reference_work
        .charge(
            Tick(1),
            Charge {
                output_bytes: expected.len() as u64,
                ..Charge::default()
            },
        )
        .unwrap();
    let mut work = work(1000);
    let mut budget = HeaderBudget::new();
    let mut cursor = URLs::new(input(source, key), &mut work, &mut budget).unwrap();
    let (bytes, result) = drain(&mut cursor, 1);
    result.unwrap();
    assert_eq!(bytes, expected);
    assert_eq!(work.remaining(), reference_work.remaining());
    assert_eq!(
        budget.source_bytes_remaining(),
        reference_budget.source_bytes_remaining()
    );
    assert_eq!(budget.steps_remaining(), reference_budget.steps_remaining());
}
#[test]
fn resource_cutoffs_never_become_null_or_a_successful_prefix() {
    let mut late = false;
    for source in [
        b"List-Post:<x:a>\n\n".as_slice(),
        b"List-Post:<x:a> (bad\n\n",
        b"List-Post:NO\n\n",
        b"List-Post:<x://[::1]>\nList-Post:<x:b>\n\n",
    ] {
        let key = "header:List-Post:asURLs:all";
        let mut work = work(1000);
        let mut budget = HeaderBudget::new();
        let (expected, result) = drain(
            &mut URLs::new(input(source, key), &mut work, &mut budget).unwrap(),
            1,
        );
        result.unwrap();
        let cost = 16_000_000 - budget.steps_remaining();
        let visits = 16 * 1024 * 1024 - budget.source_bytes_remaining();
        let output_cost = 1000 - work.remaining().output_bytes;
        for (bytes, steps, output) in (0..cost)
            .map(|cut| (visits, cut, output_cost))
            .chain((0..visits).map(|cut| (cut, cost, output_cost)))
            .chain((0..output_cost).map(|cut| (visits, cost, cut)))
        {
            let mut work = self::work(output);
            let mut budget = HeaderBudget::new();
            budget
                .charge_local(
                    &mut self::work(1000),
                    Tick(1),
                    16 * 1024 * 1024 - bytes,
                    16_000_000 - steps,
                    &mut 0,
                )
                .unwrap();
            let mut cursor = URLs::new(input(source, key), &mut work, &mut budget).unwrap();
            let (emitted, result) = drain(&mut cursor, 1);
            if output < output_cost {
                assert!(matches!(
                    result,
                    Err(Error::Work(Stop::OutputBytes))
                        | Err(Error::URLs(urls::Error::Work(Stop::OutputBytes)))
                        | Err(Error::Json(json_string::Error::URLs(urls::Error::Work(
                            Stop::OutputBytes
                        ))))
                ));
            } else {
                assert!(matches!(
                    result,
                    Err(Error::InterpretationLimit)
                        | Err(Error::URLs(urls::Error::InterpretationLimit))
                        | Err(Error::Json(json_string::Error::URLs(
                            urls::Error::InterpretationLimit
                        )))
                        | Err(Error::Selection(header_select::Error::InterpretationLimit))
                ));
                assert_eq!(work.stopped(), None);
            }
            assert!(expected.starts_with(&emitted));
            late |= emitted.ends_with(b"\"]");
        }
    }
    assert!(late);
}
#[test]
fn nesting_selection_and_final_deadline_refusals_remain_errors() {
    let nested = format!("List-Help:<x:a>{}\n\n", "(".repeat(33));
    let mut work = work(1000);
    let mut budget = HeaderBudget::new();
    let mut cursor = URLs::new(
        input(nested.as_bytes(), "header:List-Help:asURLs"),
        &mut work,
        &mut budget,
    )
    .unwrap();
    assert_eq!(
        drain(&mut cursor, 1),
        (vec![], Err(Error::URLs(urls::Error::NestingLimit)))
    );
    let source = b"List-Help:<x:a>\nList-Help:<x:b>\n\n";
    let mut limited = input(source, "header:List-Help:asURLs:all");
    limited.header_limit = b"List-Help:<x:a>\n".len() as u64 + 1;
    let mut cursor = URLs::new(limited, &mut work, &mut budget).unwrap();
    let (bytes, result) = drain(&mut cursor, 1);
    assert_eq!(bytes, b"[[\"x:a\"]");
    assert!(matches!(result, Err(Error::Selection(_))));
    let mut cursor = URLs::new(
        input(b"List-Help:<x:a>\n\n", "header:List-Help:asURLs"),
        &mut work,
        &mut budget,
    )
    .unwrap();
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
        cursor.poll(Tick(1), &mut [0; 8]),
        Err(Error::Work(Stop::Deadline))
    );
}

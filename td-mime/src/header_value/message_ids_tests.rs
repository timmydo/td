#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;
use crate::{header_message_ids as ids, header_property, time::Deadline};
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
fn remaining(cursor: &MessageIds<'_, '_>) -> (Charge, u64) {
    match &cursor.0.owner {
        Owner::Budgets(work, budget, _) => (work.remaining(), budget.steps_remaining()),
        Owner::Value(source) => source.remaining().unwrap(),
        Owner::Retired => panic!("retired coordinator"),
    }
}
fn drain(cursor: &mut MessageIds<'_, '_>, width: usize) -> (Vec<u8>, Result<End, Error>) {
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
        assert!(before.output_bytes - after.output_bytes <= 8);
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
    panic!("MessageIds property did not finish");
}
#[test]
fn message_ids_preserve_nested_arrays_nulls_and_case_insensitive_modes() {
    let source = b"References: old <a@b><c@d> tail\r\nReferences: bad (tail\r\nReferences: words only\r\n\r\n";
    for width in 1..=8 {
        for (key, expected) in [
            ("header:References:asMessageIds", "[]"),
            (
                "header:rEfErEnCeS:asMessageIds:all",
                "[[\"a@b\",\"c@d\"],null,[]]",
            ),
            ("header:X-Missing:asMessageIds", "null"),
            ("header:X-Missing:asMessageIds:all", "[]"),
        ] {
            let mut work = work(1000);
            let mut budget = HeaderBudget::new();
            let mut cursor = MessageIds::new(input(source, key), &mut work, &mut budget).unwrap();
            let (bytes, result) = drain(&mut cursor, width);
            let end = result.unwrap();
            assert_eq!(bytes, expected.as_bytes());
            assert!(!cursor.is_encoding_problem());
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
        ("iN-rEpLy-tO", "old <a@b> words", "[\"a@b\"]"),
        ("References", "", "[]"),
        ("X-Reference", "old <a@b>", "null"),
        ("Message-ID", "<a@b><c@d>", "[\"a@b\",\"c@d\"]"),
        ("Message-ID", "<a@b> (bad", "null"),
        ("Message-ID", "", "null"),
    ] {
        let source = format!("{name}:{value}\n\n");
        let key = format!("header:{name}:asMessageIds");
        let mut work = work(1000);
        let mut budget = HeaderBudget::new();
        let mut cursor =
            MessageIds::new(input(source.as_bytes(), &key), &mut work, &mut budget).unwrap();
        let (bytes, result) = drain(&mut cursor, 1);
        result.unwrap();
        assert_eq!(bytes, expected.as_bytes());
    }
}
#[test]
fn identifier_json_escapes_controls_and_preserves_decomposed_identity() {
    for (value, expected, problem) in [
        ("<🐈@b>", "[\"🐈@b\"]", false),
        ("<e\u{301}@EXAMPLE>", "[\"e\u{301}@EXAMPLE\"]", false),
        ("<\u{fdd0}@b>", "[\"�@b\"]", true),
        ("<\"\\\0\"@[x]>", "[\"\\\"\\\\\\u0000\\\"@[x]\"]", false),
        ("<\"a\r\n b\"@[c\n\td]>", "[\"\\\"a b\\\"@[c\\td]\"]", false),
        ("<=?utf-8?Q?name?=@b>", "[\"=?utf-8?Q?name?=@b\"]", false),
    ] {
        let source = format!("Message-ID:{value}\n\n");
        for width in 1..=8 {
            let mut work = work(1000);
            let mut budget = HeaderBudget::new();
            let mut cursor = MessageIds::new(
                input(source.as_bytes(), "header:Message-ID:asMessageIds"),
                &mut work,
                &mut budget,
            )
            .unwrap();
            let (bytes, result) = drain(&mut cursor, width);
            result.unwrap();
            assert_eq!(bytes, expected.as_bytes(), "{value:?}");
            assert_eq!(cursor.is_encoding_problem(), problem);
        }
    }
}
#[test]
fn only_selected_fields_contribute_encoding_diagnostics() {
    let source = "Message-ID:<\u{fdd0}@b>\nMessage-ID:<a@b>\n\n";
    for (key, expected, problem) in [
        ("header:Message-ID:asMessageIds", "[\"a@b\"]", false),
        (
            "header:Message-ID:asMessageIds:all",
            "[[\"�@b\"],[\"a@b\"]]",
            true,
        ),
    ] {
        let mut work = work(1000);
        let mut budget = HeaderBudget::new();
        let mut cursor =
            MessageIds::new(input(source.as_bytes(), key), &mut work, &mut budget).unwrap();
        let (bytes, result) = drain(&mut cursor, 1);
        result.unwrap();
        assert_eq!(bytes, expected.as_bytes());
        assert_eq!(cursor.is_encoding_problem(), problem);
    }
}
#[test]
fn composed_costs_match_selection_conversion_and_json_output() {
    let source = b"References:<a@b><c@d>\nReferences:bad (tail\nReferences:words only\n\n";
    let key = "header:References:asMessageIds:all";
    let mut reference_work = work(1000);
    let mut reference_budget = HeaderBudget::new();
    reference_budget
        .charge_local(&mut reference_work, Tick(1), 10, 1, &mut 0)
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
        let mut cursor = ids::project::Budgeted::new(
            &source[field.value_start as usize..field.value_end as usize],
            ids::Mode::ObsoletePhrases,
            &mut reference_work,
            &mut reference_budget,
        );
        loop {
            match cursor.poll(Tick(1)) {
                Ok(ids::project::Status::Complete) | Err(ids::Error::Malformed) => break,
                Ok(_) => {}
                Err(error) => panic!("{error}"),
            }
        }
    }
    let expected = b"[[\"a@b\",\"c@d\"],null,[]]";
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
    let mut cursor = MessageIds::new(input(source, key), &mut work, &mut budget).unwrap();
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
fn output_and_selection_refusals_retire_complete_identifier_prefixes() {
    let source = b"Message-ID:<a@b>\n\n";
    let mut full = work(1000);
    let mut budget = HeaderBudget::new();
    drain(
        &mut MessageIds::new(
            input(source, "header:Message-ID:asMessageIds"),
            &mut full,
            &mut budget,
        )
        .unwrap(),
        1,
    )
    .1
    .unwrap();
    let cost = 1000 - full.remaining().output_bytes;
    for output in 0..cost {
        let mut work = work(output);
        let mut budget = HeaderBudget::new();
        let mut cursor = MessageIds::new(
            input(source, "header:Message-ID:asMessageIds"),
            &mut work,
            &mut budget,
        )
        .unwrap();
        let (bytes, result) = drain(&mut cursor, 1);
        assert!(matches!(
            result,
            Err(Error::MessageIds(ids::Error::Work(Stop::OutputBytes)))
                | Err(Error::Json(json_string::Error::MessageIds(
                    ids::Error::Work(Stop::OutputBytes)
                )))
        ));
        assert!(b"[\"a@b\"]".starts_with(&bytes));
    }
    let source = b"Message-ID:<a@b>\nMessage-ID:<c@d>\n\n";
    let mut work = work(1000);
    let mut budget = HeaderBudget::new();
    let mut input = input(source, "header:Message-ID:asMessageIds:all");
    input.header_limit = b"Message-ID:<a@b>\n".len() as u64 + 1;
    let mut cursor = MessageIds::new(input, &mut work, &mut budget).unwrap();
    let (bytes, result) = drain(&mut cursor, 1);
    assert_eq!(bytes, b"[[\"a@b\"]");
    assert!(matches!(result, Err(Error::Selection(_))));
}
#[test]
fn aggregate_cutoffs_cannot_become_null_or_an_accepted_prefix() {
    let mut late = false;
    for source in [
        b"References:<a@b>\n\n".as_slice(),
        b"References:<a@b> (bad\n\n",
        b"References:old <a@b>\nReferences:<c@d>\n\n",
    ] {
        let key = "header:References:asMessageIds:all";
        let mut work = work(1000);
        let mut budget = HeaderBudget::new();
        let expected = drain(
            &mut MessageIds::new(input(source, key), &mut work, &mut budget).unwrap(),
            1,
        )
        .0;
        let cost = 16_000_000 - budget.steps_remaining();
        let visits = 16 * 1024 * 1024 - budget.source_bytes_remaining();
        for (bytes, remaining) in (0..cost)
            .map(|cut| (visits, cut))
            .chain((0..visits).map(|cut| (cut, cost)))
        {
            let mut work = self::work(1000);
            let mut budget = HeaderBudget::new();
            budget
                .charge_local(
                    &mut self::work(1000),
                    Tick(1),
                    16 * 1024 * 1024 - bytes,
                    16_000_000 - remaining,
                    &mut 0,
                )
                .unwrap();
            let mut cursor = MessageIds::new(input(source, key), &mut work, &mut budget).unwrap();
            let (bytes, result) = drain(&mut cursor, 1);
            assert!(matches!(
                result,
                Err(Error::InterpretationLimit)
                    | Err(Error::MessageIds(ids::Error::InterpretationLimit))
                    | Err(Error::Json(json_string::Error::MessageIds(
                        ids::Error::InterpretationLimit
                    )))
                    | Err(Error::Selection(header_select::Error::InterpretationLimit))
            ));
            assert!(expected.starts_with(&bytes));
            late |= bytes.ends_with(b"\"]");
            assert_eq!(work.stopped(), None);
        }
    }
    assert!(late);
}
#[test]
fn nesting_and_final_admission_retire_instead_of_making_null() {
    let nested = format!("Message-ID:<a@b>{}\n\n", "(".repeat(33));
    let mut work = work(1000);
    let mut budget = HeaderBudget::new();
    let mut cursor = MessageIds::new(
        input(nested.as_bytes(), "header:Message-ID:asMessageIds"),
        &mut work,
        &mut budget,
    )
    .unwrap();
    assert_eq!(
        drain(&mut cursor, 1),
        (vec![], Err(Error::MessageIds(ids::Error::NestingLimit)))
    );
    let mut cursor = MessageIds::new(
        input(b"Message-ID:<a@b>\n\n", "header:Message-ID:asMessageIds"),
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

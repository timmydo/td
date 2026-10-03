#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;
use crate::{header_addresses as parsed, header_property, ports::Deadline};
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
fn remaining(cursor: &GroupedAddresses<'_, '_>) -> (Charge, u64) {
    match &cursor.0.owner {
        Owner::Budgets(work, budget, _) => (work.remaining(), budget.steps_remaining()),
        Owner::Value(source) => source.remaining().unwrap(),
        Owner::Retired => panic!("retired coordinator"),
    }
}
fn drain(cursor: &mut GroupedAddresses<'_, '_>, width: usize) -> (Vec<u8>, Result<End, Error>) {
    assert!(std::mem::size_of_val(cursor) <= 2560);
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
        assert!(before.output_bytes - after.output_bytes <= 9);
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
    panic!("GroupedAddresses property did not finish");
}
#[test]
fn grouped_fields_preserve_named_empty_and_unnamed_runs() {
    let source = b"To: a@b, c@d, G:Jo <e@f>; Empty:; \"\":; z@w\nTo: Last:;\n\n";
    for width in 1..=8 {
        for (key, expected) in [
            (
                "header:To:asGroupedAddresses",
                r#"[{"name":"Last","addresses":[]}]"#,
            ),
            (
                "header:tO:asGroupedAddresses:all",
                r#"[[{"name":null,"addresses":[{"name":null,"email":"a@b"},{"name":null,"email":"c@d"}]},{"name":"G","addresses":[{"name":"Jo","email":"e@f"}]},{"name":"Empty","addresses":[]},{"name":"","addresses":[]},{"name":null,"addresses":[{"name":null,"email":"z@w"}]}],[{"name":"Last","addresses":[]}]]"#,
            ),
            ("header:X-Missing:asGroupedAddresses", "null"),
            ("header:X-Missing:asGroupedAddresses:all", "[]"),
        ] {
            let mut work = work(10_000);
            let mut budget = HeaderBudget::new();
            let mut scratch = nfc::Scratch::new();
            let mut cursor =
                GroupedAddresses::new(input(source, key), &mut scratch, &mut work, &mut budget)
                    .unwrap();
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
        }
    }
    for (value, expected, diagnostic) in [
        (
            "=?utf-8?q?=FF?=:;",
            r#"[{"name":"=?utf-8?q?=FF?=","addresses":[]}]"#,
            false,
        ),
        ("", "[]", false),
        (" , (c),; ;", "[]", false),
        (
            "G:;H:",
            r#"[{"name":"G","addresses":[]},{"name":"H","addresses":[]}]"#,
            false,
        ),
        (
            "G:a@b,H:c@d;",
            r#"[{"name":"G","addresses":[{"name":null,"email":"a@b"},{"name":null,"email":"H:c@d"}]}]"#,
            false,
        ),
        (
            "@bad:a@b, c@d; z@w",
            r#"[{"name":null,"addresses":[{"name":null,"email":"@bad:a@b"},{"name":null,"email":"c@d"}]},{"name":null,"addresses":[{"name":null,"email":"z@w"}]}]"#,
            false,
        ),
        (
            "=?utf-8?q?e=CC=81?= : e\u{301} <e\u{301}@EXAMPLE>;",
            "[{\"name\":\"é\",\"addresses\":[{\"name\":\"é\",\"email\":\"e\u{301}@EXAMPLE\"}]}]",
            false,
        ),
        (
            "=?utf-8?q?=FF?= :;",
            r#"[{"name":"�","addresses":[]}]"#,
            true,
        ),
        (
            "\"G\\\"x\": bad;",
            r#"[{"name":"G\"x","addresses":[{"name":null,"email":"bad"}]}]"#,
            false,
        ),
        (
            "G:a@b(Name), \"\" <c@d>",
            r#"[{"name":"G","addresses":[{"name":"Name","email":"a@b"},{"name":"","email":"c@d"}]}]"#,
            false,
        ),
    ] {
        let source = format!("To:{value}\n\n");
        let mut work = work(10_000);
        let mut budget = HeaderBudget::new();
        let mut scratch = nfc::Scratch::new();
        let mut cursor = GroupedAddresses::new(
            input(source.as_bytes(), "header:To:asGroupedAddresses"),
            &mut scratch,
            &mut work,
            &mut budget,
        )
        .unwrap();
        let (bytes, result) = drain(&mut cursor, 1);
        result.unwrap();
        assert_eq!(bytes, expected.as_bytes(), "{value:?}");
        assert_eq!(cursor.is_encoding_problem(), diagnostic);
    }
}
#[test]
fn group_diagnostics_belong_only_to_selected_fields() {
    let source = b"To:=?utf-8?q?=FF?= :;\nTo:G:;\n\n";
    for (key, expected, diagnostic) in [
        (
            "header:To:asGroupedAddresses",
            r#"[{"name":"G","addresses":[]}]"#,
            false,
        ),
        (
            "header:To:asGroupedAddresses:all",
            r#"[[{"name":"�","addresses":[]}],[{"name":"G","addresses":[]}]]"#,
            true,
        ),
    ] {
        let mut work = work(1000);
        let mut budget = HeaderBudget::new();
        let mut scratch = nfc::Scratch::new();
        let mut cursor =
            GroupedAddresses::new(input(source, key), &mut scratch, &mut work, &mut budget)
                .unwrap();
        let (bytes, result) = drain(&mut cursor, 1);
        result.unwrap();
        assert_eq!(bytes, expected.as_bytes());
        assert_eq!(cursor.is_encoding_problem(), diagnostic);
    }
}
#[test]
fn exact_composed_charges_preserve_parser_credit_through_name_and_address_work() {
    use crate::{decode_work::Parsing, header_address_text, header_mailbox::Name, header_name};
    let source = b"To: G:Jo <a@b>,bad; c@d(Name)\nTo: Empty:;\n\n";
    let key = "header:To:asGroupedAddresses:all";
    let mut work = work(10_000);
    let mut budget = HeaderBudget::new();
    let mut scratch = nfc::Scratch::new();
    let (json, result) = drain(
        &mut GroupedAddresses::new(input(source, key), &mut scratch, &mut work, &mut budget)
            .unwrap(),
        1,
    );
    result.unwrap();
    let mut reference = self::work(10_000);
    let mut reference_budget = HeaderBudget::new();
    let mut reference_scratch = nfc::Scratch::new();
    let mut selector = header_select::Cursor::new(
        source,
        4096,
        source.len() as u64,
        property(key),
        header_select::SourceEnd::Eof,
    );
    loop {
        match selector
            .poll_with_budget(Tick(1), &mut reference, &mut reference_budget)
            .unwrap()
        {
            header_select::Status::Yield => {}
            header_select::Status::Complete(_) => break,
            header_select::Status::Match(field) => {
                let value =
                    &source[(field.value_start - 4096) as usize..(field.value_end - 4096) as usize];
                let mut parser = parsed::Cursor::new(value);
                let mut credit = 0;
                loop {
                    match parser
                        .poll_with_work(
                            Tick(1),
                            &mut Parsing::new(&mut reference, &mut reference_budget, &mut credit),
                        )
                        .unwrap()
                    {
                        parsed::Status::Complete => break,
                        parsed::Status::BeginGroup(Some(extent)) => {
                            let mut name = header_name::Cursor::new(
                                value,
                                extent,
                                header_name::Kind::Phrase,
                                &mut reference,
                                &mut reference_budget,
                                &mut reference_scratch,
                            )
                            .unwrap();
                            while name.poll(Tick(1)).unwrap() != nfc::Status::Complete {}
                        }
                        parsed::Status::Mailbox(mailbox) => {
                            let (name, extent, mode) = match mailbox {
                                parsed::Address::Parsed(mailbox) => (
                                    mailbox.name,
                                    mailbox.address,
                                    header_address_text::Mode::Parsed,
                                ),
                                parsed::Address::Raw(extent) => {
                                    (None, extent, header_address_text::Mode::Fallback)
                                }
                            };
                            if let Some(name) = name {
                                let (extent, kind) = match name {
                                    Name::Phrase(extent) => (extent, header_name::Kind::Phrase),
                                    Name::Comment(extent) => (extent, header_name::Kind::Comment),
                                };
                                let mut name = header_name::Cursor::new(
                                    value,
                                    extent,
                                    kind,
                                    &mut reference,
                                    &mut reference_budget,
                                    &mut reference_scratch,
                                )
                                .unwrap();
                                while name.poll(Tick(1)).unwrap() != nfc::Status::Complete {}
                            }
                            let mut text = header_address_text::Budgeted::new(
                                &value[extent.start..extent.end],
                                mode,
                                &mut reference,
                                &mut reference_budget,
                            );
                            while text.poll(Tick(1)).unwrap()
                                != header_address_text::Status::Complete
                            {
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }
    reference
        .charge(
            Tick(1),
            Charge {
                output_bytes: json.len() as u64,
                ..Charge::default()
            },
        )
        .unwrap();
    assert_eq!(work.remaining(), reference.remaining());
    assert_eq!(
        budget.source_bytes_remaining(),
        reference_budget.source_bytes_remaining()
    );
    assert_eq!(budget.steps_remaining(), reference_budget.steps_remaining());
}
fn limited(bytes: u64, steps: u64) -> HeaderBudget {
    let mut budget = HeaderBudget::new();
    budget
        .charge(
            &mut work(0),
            Tick(1),
            budget.source_bytes_remaining() - bytes,
            budget.steps_remaining() - steps,
            &mut 0,
        )
        .unwrap();
    budget
}
fn name_limit(error: crate::header_name::Error, output: bool) -> bool {
    use crate::{header_comment::Error as C, header_name::Error as N, header_phrase::Error as P};
    match error {
        N::Phrase(P::InterpretationLimit)
        | N::Comment(C::InterpretationLimit)
        | N::Normalize(nfc::Error::InterpretationLimit) => !output,
        N::Phrase(P::Work(Stop::OutputBytes))
        | N::Comment(C::Work(Stop::OutputBytes))
        | N::Normalize(nfc::Error::Work(Stop::OutputBytes)) => output,
        _ => false,
    }
}
fn resource(error: Error, output: bool) -> bool {
    use crate::header_address_text::Error as A;
    match error {
        Error::InterpretationLimit
        | Error::Selection(header_select::Error::InterpretationLimit)
        | Error::Addresses(parsed::Error::InterpretationLimit)
        | Error::AddressText(A::InterpretationLimit)
        | Error::Json(json_string::Error::Address(A::InterpretationLimit)) => !output,
        Error::Work(Stop::OutputBytes)
        | Error::AddressText(A::Work(Stop::OutputBytes))
        | Error::Json(json_string::Error::Address(A::Work(Stop::OutputBytes))) => output,
        Error::Name(error) | Error::Json(json_string::Error::Name(error)) => {
            name_limit(error, output)
        }
        _ => false,
    }
}
#[test]
fn every_partial_budget_retires_provisional_objects_and_completed_inner_lists() {
    let mut object = false;
    let mut list = false;
    for source in [
        b"To:a@b\n\n".as_slice(),
        b"To:Jo <a@b>,bad\nTo:c@d(Name)\n\n",
        b"To:=?utf-8?q?e=CC=81?= :;\nTo:\xff\n\n",
        b"To:\"\" <a@b>,c@d\n\n",
    ] {
        let key = "header:To:asGroupedAddresses:all";
        let mut work = work(1000);
        let mut budget = HeaderBudget::new();
        let mut scratch = nfc::Scratch::new();
        let (expected, result) = drain(
            &mut GroupedAddresses::new(input(source, key), &mut scratch, &mut work, &mut budget)
                .unwrap(),
            1,
        );
        result.unwrap();
        let visits = 16 * 1024 * 1024 - budget.source_bytes_remaining();
        let steps = 16_000_000 - budget.steps_remaining();
        let output = 1000 - work.remaining().output_bytes;
        for (bytes, steps, output_bytes, is_output) in (0..visits)
            .map(|cut| (cut, steps, output, false))
            .chain((0..steps).map(|cut| (visits, cut, output, false)))
            .chain((0..output).map(|cut| (visits, steps, cut, true)))
        {
            let mut work = self::work(output_bytes);
            let mut budget = limited(bytes, steps);
            let mut cursor =
                GroupedAddresses::new(input(source, key), &mut scratch, &mut work, &mut budget)
                    .unwrap();
            let (emitted, result) = drain(&mut cursor, 1);
            let error = result.unwrap_err();
            assert!(resource(error, is_output), "unexpected cause: {error}");
            assert!(expected.starts_with(&emitted));
            object |= emitted.ends_with(b"}");
            list |= emitted.ends_with(b"}]");
            if !is_output {
                assert_eq!(work.stopped(), None);
            }
        }
        let mut work = self::work(output);
        let mut budget = limited(visits, steps);
        let (bytes, result) = drain(
            &mut GroupedAddresses::new(input(source, key), &mut scratch, &mut work, &mut budget)
                .unwrap(),
            1,
        );
        result.unwrap();
        assert_eq!(bytes, expected);
    }
    assert!(object && list);
}
#[test]
fn late_nesting_selection_and_deadline_failure_do_not_publish_a_partial_value() {
    let nested = format!("To:a@b,{}\n\n", "(".repeat(33));
    let mut work = work(10_000);
    let mut budget = HeaderBudget::new();
    let mut scratch = nfc::Scratch::new();
    let mut cursor = GroupedAddresses::new(
        input(nested.as_bytes(), "header:To:asGroupedAddresses"),
        &mut scratch,
        &mut work,
        &mut budget,
    )
    .unwrap();
    let (bytes, result) = drain(&mut cursor, 1);
    assert_eq!(
        bytes,
        br#"[{"name":null,"addresses":[{"name":null,"email":"a@b"}"#
    );
    assert_eq!(result, Err(Error::Addresses(parsed::Error::NestingLimit)));
    let source = b"To:a@b\nTo:c@d\n\n";
    let mut selected = input(source, "header:To:asGroupedAddresses:all");
    selected.header_limit = b"To:a@b\n".len() as u64 + 1;
    let mut cursor = GroupedAddresses::new(selected, &mut scratch, &mut work, &mut budget).unwrap();
    let (bytes, result) = drain(&mut cursor, 1);
    assert_eq!(
        bytes,
        br#"[[{"name":null,"addresses":[{"name":null,"email":"a@b"}]}]"#
    );
    assert!(matches!(result, Err(Error::Selection(_))));
    let mut cursor = GroupedAddresses::new(
        input(b"To:a@b\n\n", "header:To:asGroupedAddresses"),
        &mut scratch,
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
        cursor.poll(Tick(1), &mut [0]),
        Err(Error::Work(Stop::Deadline))
    );
}

#[test]
fn address_forms_cannot_share_the_others_property_authorization() {
    let mut work = work(1000);
    let mut budget = HeaderBudget::new();
    let mut scratch = nfc::Scratch::new();
    assert!(matches!(
        GroupedAddresses::new(
            input(b"To:a@b\n\n", "header:To:asAddresses"),
            &mut scratch,
            &mut work,
            &mut budget
        ),
        Err(Error::UnsupportedForm)
    ));
    assert!(matches!(
        Addresses::new(
            input(b"To:a@b\n\n", "header:To:asGroupedAddresses"),
            &mut scratch,
            &mut work,
            &mut budget
        ),
        Err(Error::UnsupportedForm)
    ));
}

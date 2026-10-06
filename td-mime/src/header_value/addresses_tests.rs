#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;
use crate::{header_addresses as parsed, header_property, time::Deadline};
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
fn remaining(cursor: &Addresses<'_, '_>) -> (Charge, u64) {
    match &cursor.0.owner {
        Owner::Budgets(work, budget, _) => (work.remaining(), budget.steps_remaining()),
        Owner::Value(source) => source.remaining().unwrap(),
        Owner::Retired => panic!("retired coordinator"),
    }
}
fn drain(cursor: &mut Addresses<'_, '_>, width: usize) -> (Vec<u8>, Result<End, Error>) {
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
    panic!("Addresses property did not finish");
}
#[test]
fn flattened_fields_keep_last_all_absence_and_recovered_mailbox_values() {
    let source = b"To: G: Jo <a@b>,bad; Empty:; c@d(Name)\nTo: \"\" <@route:q@EXAMPLE>\n\n";
    for width in 1..=8 {
        for (key, expected) in [
            (
                "header:To:asAddresses",
                r#"[{"name":"","email":"q@EXAMPLE"}]"#,
            ),
            (
                "header:tO:asAddresses:all",
                r#"[[{"name":"Jo","email":"a@b"},{"name":null,"email":"bad"},{"name":"Name","email":"c@d"}],[{"name":"","email":"q@EXAMPLE"}]]"#,
            ),
            ("header:X-Missing:asAddresses", "null"),
            ("header:X-Missing:asAddresses:all", "[]"),
        ] {
            let mut work = work(10_000);
            let mut budget = HeaderBudget::new();
            let mut scratch = nfc::Scratch::new();
            let mut cursor =
                Addresses::new(input(source, key), &mut scratch, &mut work, &mut budget).unwrap();
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
        ("", "[]", false),
        ("G:; , (c),; H:;", "[]", false),
        (
            "e\u{301} <e\u{301}@EXAMPLE>",
            "[{\"name\":\"é\",\"email\":\"e\u{301}@EXAMPLE\"}]",
            false,
        ),
        (
            "a@b (e\u{301}), \"\" <c@d>",
            r#"[{"name":"é","email":"a@b"},{"name":"","email":"c@d"}]"#,
            false,
        ),
        (
            "=?utf-8?q?=FF?=: a@b;",
            r#"[{"name":null,"email":"a@b"}]"#,
            false,
        ),
        (
            "=?utf-8?q?=FF?= <a@b>",
            r#"[{"name":"�","email":"a@b"}]"#,
            true,
        ),
        (
            "a@b (one) (two), <a@b> (ignored)",
            r#"[{"name":"one","email":"a@b"},{"name":null,"email":"a@b"}]"#,
            false,
        ),
        (
            "G:a@b,H:c@d;",
            r#"[{"name":null,"email":"a@b"},{"name":null,"email":"H:c@d"}]"#,
            false,
        ),
        (
            "\"unclosed,tail",
            r#"[{"name":null,"email":"\"unclosed,tail"}]"#,
            false,
        ),
    ] {
        let source = format!("To:{value}\n\n");
        let mut work = work(10_000);
        let mut budget = HeaderBudget::new();
        let mut scratch = nfc::Scratch::new();
        let mut cursor = Addresses::new(
            input(source.as_bytes(), "header:To:asAddresses"),
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
fn encoding_diagnostics_include_only_selected_fields_and_projected_names() {
    let source = b"To: \xff\nTo: a@b\n\n";
    for (key, expected, diagnostic) in [
        (
            "header:To:asAddresses",
            r#"[{"name":null,"email":"a@b"}]"#,
            false,
        ),
        (
            "header:To:asAddresses:all",
            r#"[[{"name":null,"email":"�"}],[{"name":null,"email":"a@b"}]]"#,
            true,
        ),
    ] {
        let mut work = work(1000);
        let mut budget = HeaderBudget::new();
        let mut scratch = nfc::Scratch::new();
        let mut cursor =
            Addresses::new(input(source, key), &mut scratch, &mut work, &mut budget).unwrap();
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
    let key = "header:To:asAddresses:all";
    let mut work = work(10_000);
    let mut budget = HeaderBudget::new();
    let mut scratch = nfc::Scratch::new();
    let (json, result) = drain(
        &mut Addresses::new(input(source, key), &mut scratch, &mut work, &mut budget).unwrap(),
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
        .charge_local(
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
        b"To:G:;\nTo:\xff\n\n",
        b"To:\"\" <a@b>,c@d\n\n",
    ] {
        let key = "header:To:asAddresses:all";
        let mut work = work(1000);
        let mut budget = HeaderBudget::new();
        let mut scratch = nfc::Scratch::new();
        let (expected, result) = drain(
            &mut Addresses::new(input(source, key), &mut scratch, &mut work, &mut budget).unwrap(),
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
                Addresses::new(input(source, key), &mut scratch, &mut work, &mut budget).unwrap();
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
            &mut Addresses::new(input(source, key), &mut scratch, &mut work, &mut budget).unwrap(),
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
    let mut cursor = Addresses::new(
        input(nested.as_bytes(), "header:To:asAddresses"),
        &mut scratch,
        &mut work,
        &mut budget,
    )
    .unwrap();
    let (bytes, result) = drain(&mut cursor, 1);
    assert_eq!(bytes, br#"[{"name":null,"email":"a@b"}"#);
    assert_eq!(result, Err(Error::Addresses(parsed::Error::NestingLimit)));
    let source = b"To:a@b\nTo:c@d\n\n";
    let mut selected = input(source, "header:To:asAddresses:all");
    selected.header_limit = b"To:a@b\n".len() as u64 + 1;
    let mut cursor = Addresses::new(selected, &mut scratch, &mut work, &mut budget).unwrap();
    let (bytes, result) = drain(&mut cursor, 1);
    assert_eq!(bytes, br#"[[{"name":null,"email":"a@b"}]"#);
    assert!(matches!(result, Err(Error::Selection(_))));
    let mut cursor = Addresses::new(
        input(b"To:a@b\n\n", "header:To:asAddresses"),
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

#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;
use crate::{header_property, ports::Deadline};
fn work() -> Meter {
    Meter::new(
        Deadline::after(Tick(0), 100).unwrap(),
        Charge {
            io_bytes: 100_000_000,
            records: 2_000_000,
            output_bytes: 1_000_000,
            ..Charge::default()
        },
    )
}
fn property(key: &str) -> Property<'_> {
    let mut cursor = header_property::Cursor::new(key, header_property::Context::Email);
    let mut work = work();
    for _ in 0..1000 {
        if let header_property::Status::Complete(value) = cursor.poll(Tick(1), &mut work).unwrap() {
            return value.unwrap();
        }
    }
    panic!("property did not complete");
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
fn drain(cursor: &mut Text<'_, '_>, width: usize) -> Vec<u8> {
    assert!(std::mem::size_of_val(cursor) <= 1664);
    let mut output = [0xa5; 8];
    let mut bytes = Vec::new();
    for _ in 0..100_000 {
        output.fill(0xa5);
        let progress = cursor.poll(Tick(1), &mut output[..width]).unwrap();
        assert!(progress.written <= 6);
        bytes.extend_from_slice(&output[..progress.written]);
        assert!(output[progress.written..].iter().all(|byte| *byte == 0xa5));
        if let Status::Complete(end) = progress.status {
            assert!(end.body_start >= 4096);
            assert_eq!(
                cursor.poll(Tick(100), &mut []),
                Ok(Progress {
                    written: 0,
                    status: Status::Complete(end)
                })
            );
            cursor.check_deadline(Tick(1)).unwrap();
            return bytes;
        }
    }
    panic!("Text property did not finish");
}
#[test]
fn text_properties_normalize_words_and_folds_and_keep_raw_identity_separate() {
    let source = b"Subject: =?utf-8?Q?cafe=CC=81?=\r\nSubject: e\xcc\x81\r\n\tend\r\n\r\n";
    for width in 1..=8 {
        for (key, expected) in [
            ("subject", "\"é\\tend\""),
            ("header:Subject:asText:all", "[\"café\",\"é\\tend\"]"),
            ("header:Comments:asText", "null"),
            ("header:Comments:asText:all", "[]"),
        ] {
            let mut work = work();
            let mut budget = HeaderBudget::new();
            let mut scratch = nfc::Scratch::new();
            let mut cursor =
                Text::new(input(source, key), &mut scratch, &mut work, &mut budget).unwrap();
            assert_eq!(drain(&mut cursor, width), expected.as_bytes());
            assert!(!cursor.is_encoding_problem());
            assert_eq!(
                1_000_000 - work.remaining().output_bytes,
                expected.len() as u64
            );
        }
    }
    for (source, expected, problem) in [
        (b"Subject:\n\n".as_slice(), "\"\"", false),
        (b"Subject: \xff\n\n", "\"�\"", true),
        (b"Subject: \xef\xb7\x90\n\n", "\"�\"", true),
    ] {
        let mut work = work();
        let mut budget = HeaderBudget::new();
        let mut scratch = nfc::Scratch::new();
        let mut cursor = Text::new(
            input(source, "subject"),
            &mut scratch,
            &mut work,
            &mut budget,
        )
        .unwrap();
        assert_eq!(drain(&mut cursor, 1), expected.as_bytes());
        assert_eq!(cursor.is_encoding_problem(), problem);
    }
    let mut work = work();
    let mut budget = HeaderBudget::new();
    let mut cursor = Raw::new(
        source,
        4096,
        source.len() as u64,
        property("header:Subject"),
        header_select::SourceEnd::Eof,
        &mut work,
        &mut budget,
    )
    .unwrap();
    assert_eq!(
        super::tests::drain(&mut cursor, 1).0,
        "\" e\u{301}\\r\\n\\tend\"".as_bytes()
    );
}
fn normalized_json(
    source: &[u8],
    scratch: &mut nfc::Scratch,
    work: &mut Meter,
    budget: &mut HeaderBudget,
) -> Vec<u8> {
    let mut source = nfc::Cursor::from_unstructured_header(source, scratch, work, budget);
    let mut cursor = json_string::Cursor::new(&mut source);
    let mut output = [0; 1];
    let mut bytes = Vec::new();
    for _ in 0..100_000 {
        let progress = cursor.poll(Tick(1), &mut output).unwrap();
        bytes.extend_from_slice(&output[..progress.written]);
        if progress.status == json_string::Status::Complete {
            return bytes;
        }
    }
    panic!("reference did not finish");
}
#[test]
fn overflow_scratch_handoffs_keep_exact_selection_and_conversion_charges() {
    let first = format!(" a{}\u{323}", "\u{301}".repeat(300));
    let source = format!("Subject:{first}\r\nSubject: plain\r\n\r\n");
    let selected = property("header:Subject:asText:all");
    let mut reference_work = work();
    let mut reference_budget = HeaderBudget::new();
    reference_budget
        .charge(&mut reference_work, Tick(1), 7, 1, &mut 0)
        .unwrap();
    let mut scratch = nfc::Scratch::new();
    let mut selector = header_select::Cursor::new(
        source.as_bytes(),
        0,
        source.len() as u64,
        selected,
        header_select::SourceEnd::Eof,
    );
    let mut fields = Vec::new();
    let mut complete = false;
    for _ in 0..100_000 {
        match selector
            .poll_with_budget(Tick(1), &mut reference_work, &mut reference_budget)
            .unwrap()
        {
            header_select::Status::Match(field) => fields.push(field),
            header_select::Status::Yield => {}
            header_select::Status::Complete(_) => {
                complete = true;
                break;
            }
        }
    }
    assert!(complete);
    assert_eq!(fields.len(), 2);
    let mut expected = vec![b'['];
    for (index, field) in fields.iter().enumerate() {
        if index != 0 {
            expected.push(b',');
        }
        expected.extend(normalized_json(
            &source.as_bytes()[field.value_start as usize..field.value_end as usize],
            &mut scratch,
            &mut reference_work,
            &mut reference_budget,
        ));
    }
    expected.push(b']');
    reference_work
        .charge(
            Tick(1),
            Charge {
                output_bytes: 3,
                ..Charge::default()
            },
        )
        .unwrap();
    let mut work = work();
    let mut budget = HeaderBudget::new();
    let mut cursor = Text::new(
        input(source.as_bytes(), "header:Subject:asText:all"),
        &mut scratch,
        &mut work,
        &mut budget,
    )
    .unwrap();
    assert_eq!(drain(&mut cursor, 1), expected);
    assert_eq!(work.remaining(), reference_work.remaining());
    assert_eq!(
        budget.source_bytes_remaining(),
        reference_budget.source_bytes_remaining()
    );
    assert_eq!(budget.steps_remaining(), reference_budget.steps_remaining());
}
#[test]
fn text_refusal_after_a_provisional_value_and_final_checks_are_terminal() {
    let mut work = work();
    let mut budget = HeaderBudget::new();
    let mut scratch = nfc::Scratch::new();
    let mut spec = input(b"Subject: a\nOther: long\n\n", "header:Subject:asText:all");
    spec.header_limit = 12;
    let mut cursor = Text::new(spec, &mut scratch, &mut work, &mut budget).unwrap();
    let mut output = [0xa5; 1];
    let mut prefix = Vec::new();
    let mut refused = false;
    for _ in 0..1000 {
        match cursor.poll(Tick(1), &mut output) {
            Ok(progress) => {
                assert!(!matches!(progress.status, Status::Complete(_)));
                prefix.extend_from_slice(&output[..progress.written]);
            }
            Err(error) => {
                assert_eq!(
                    error,
                    Error::Selection(header_select::Error::Headers(
                        crate::mime_headers::Error::HeaderLimit
                    ))
                );
                output.fill(0xa5);
                assert_eq!(cursor.poll(Tick(1), &mut output), Err(error));
                assert_eq!(output, [0xa5]);
                refused = true;
                break;
            }
        }
    }
    assert!(refused);
    assert_eq!(prefix, b"[\"a\"");
    let mut work = self::work();
    let mut budget = HeaderBudget::new();
    let mut cursor = Text::new(
        input(b"Subject: a", "subject"),
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
    assert_eq!(drain(&mut cursor, 1), b"\"a\"");
    assert_eq!(
        cursor.check_deadline(Tick(100)),
        Err(Error::Work(Stop::Deadline))
    );
    assert_eq!(
        cursor.poll(Tick(1), &mut output),
        Err(Error::Work(Stop::Deadline))
    );
    let mut work = self::work();
    let mut budget = HeaderBudget::new();
    assert!(matches!(
        Text::new(
            input(b"X:a", "header:X"),
            &mut scratch,
            &mut work,
            &mut budget
        ),
        Err(Error::UnsupportedForm)
    ));
}
#[test]
fn normalized_handoff_refuses_partial_and_failed_sources() {
    let mut work = work();
    let mut budget = HeaderBudget::new();
    let mut scratch = nfc::Scratch::new();
    let cursor = nfc::Cursor::from_unstructured_header(b"a", &mut scratch, &mut work, &mut budget);
    assert!(matches!(cursor.finish(), Err(nfc::Error::InvalidState)));
    let mut cursor =
        nfc::Cursor::from_unstructured_header(b"abc", &mut scratch, &mut work, &mut budget);
    let mut scalar = false;
    for _ in 0..100 {
        if matches!(cursor.poll(Tick(1)).unwrap(), nfc::Status::Scalar(_)) {
            scalar = true;
            break;
        }
    }
    assert!(scalar);
    assert!(matches!(cursor.finish(), Err(nfc::Error::InvalidState)));
    let mut cursor =
        nfc::Cursor::from_unstructured_header(b"", &mut scratch, &mut work, &mut budget);
    let mut complete = false;
    for _ in 0..100 {
        if cursor.poll(Tick(1)).unwrap() == nfc::Status::Complete {
            complete = true;
            break;
        }
    }
    assert!(complete);
    assert_eq!(
        cursor.charge_output(Tick(100), 0),
        Err(nfc::Error::Work(Stop::Deadline))
    );
    assert!(matches!(
        cursor.finish(),
        Err(nfc::Error::Work(Stop::Deadline))
    ));
}

#[test]
fn normalized_output_refusal_retires_the_whole_provisional_value() {
    let mut work = Meter::new(
        Deadline::after(Tick(0), 100).unwrap(),
        Charge {
            io_bytes: 10000,
            records: 10000,
            output_bytes: 1,
            ..Charge::default()
        },
    );
    let mut budget = HeaderBudget::new();
    let mut scratch = nfc::Scratch::new();
    let mut cursor = Text::new(
        input(b"Subject: a", "subject"),
        &mut scratch,
        &mut work,
        &mut budget,
    )
    .unwrap();
    let mut output = [0; 1];
    let mut prefix = Vec::new();
    let mut refused = false;
    for _ in 0..1000 {
        match cursor.poll(Tick(1), &mut output) {
            Ok(progress) => {
                assert!(!matches!(progress.status, Status::Complete(_)));
                prefix.extend_from_slice(&output[..progress.written]);
            }
            Err(error) => {
                assert_eq!(
                    error,
                    Error::Json(json_string::Error::Source(nfc::Error::Work(
                        Stop::OutputBytes
                    )))
                );
                output.fill(0xa5);
                assert_eq!(cursor.poll(Tick(1), &mut output), Err(error));
                assert_eq!(cursor.check_deadline(Tick(1)), Err(error));
                assert_eq!(output, [0xa5]);
                refused = true;
                break;
            }
        }
    }
    assert!(refused);
    assert_eq!(prefix, b"\"");
    assert_eq!(work.stopped(), Some(Stop::OutputBytes));
}

#[test]
fn text_grammar_is_checked_before_output_in_single_and_all_modes() {
    for key in [
        "header:Content-Transfer-Encoding:asText",
        "header:Content-Transfer-Encoding:asText:all",
        "header:Unknown:asText",
    ] {
        let mut work = work();
        let mut budget = HeaderBudget::new();
        let mut scratch = nfc::Scratch::new();
        let source = b"Content-Transfer-Encoding: secret\nUnknown: secret\n\n";
        let mut cursor =
            Text::new(input(source, key), &mut scratch, &mut work, &mut budget).unwrap();
        let mut output = [0xa5; 8];
        assert_eq!(
            cursor.poll(Tick(1), &mut output),
            Err(Error::UnsupportedGrammar)
        );
        assert_eq!(
            cursor.poll(Tick(1), &mut output),
            Err(Error::UnsupportedGrammar)
        );
        assert_eq!(output, [0xa5; 8]);
        assert_eq!(work.remaining().output_bytes, 1_000_000);
    }
    for (key, source) in [
        (
            "header:sUbJeCt:asText",
            b"Subject: =?utf-8?Q?caf=C3=A9?=\n\n".as_slice(),
        ),
        (
            "header:cOmMeNtS:asText",
            b"Comments: =?utf-8?Q?caf=C3=A9?=\n\n",
        ),
    ] {
        let mut work = work();
        let mut budget = HeaderBudget::new();
        let mut scratch = nfc::Scratch::new();
        let mut cursor =
            Text::new(input(source, key), &mut scratch, &mut work, &mut budget).unwrap();
        assert_eq!(drain(&mut cursor, 1), "\"café\"".as_bytes());
    }
}

#[test]
fn mime_parameter_text_decodes_comments_and_keeps_literal_syntax_before_nfc() {
    for field in ["Content-Type", "Content-Disposition"] {
        for (value, expected, problem) in [
            (
                r#"attachment; filename="a\" (=?utf-8?Q?no?=)""#,
                r#"attachment; filename="a\" (=?utf-8?Q?no?=)""#,
                false,
            ),
            (
                r#"attachment; filename="a\\" (=?utf-8?Q?yes?=)"#,
                r#"attachment; filename="a\\" (yes)"#,
                false,
            ),
            (
                "attachment; filename=\"cafe\u{301}\"",
                "attachment; filename=\"café\"",
                false,
            ),
            (
                "text/plain; name=\"=?utf-8?Q?cafe=CC=81?=\" (=?utf-8?Q?e=CC=81?=)",
                "text/plain; name=\"=?utf-8?Q?cafe=CC=81?=\" (é)",
                false,
            ),
            (
                "=?utf-8?Q?no?=; name= =?utf-8?Q?no?= ; name*=utf-8''caf%C3%A9",
                "=?utf-8?Q?no?=; name= =?utf-8?Q?no?= ; name*=utf-8''caf%C3%A9",
                false,
            ),
            (
                "attachment (=?utf-8?Q?one?=\r\n\t=?utf-8?Q?two?=); filename=\"(=?utf-8?Q?no?=)\"",
                "attachment (onetwo); filename=\"(=?utf-8?Q?no?=)\"",
                false,
            ),
            (
                "text/plain (outer(=?utf-8?Q?yes?=)tail) (\\é=?utf-8?Q?yes?=)",
                "text/plain (outer(yes)tail) (\\éyes)",
                false,
            ),
            (
                "attachment (=?utf-8?Q?=FF?=) (=?unknown?Q?literal?=)",
                "attachment (�) (=?unknown?Q?literal?=)",
                true,
            ),
            (
                "attachment (=?utf-8?Q?=22=29=3B?= =?utf-8?Q?still?=); filename=x",
                "attachment (\");still); filename=x",
                false,
            ),
            (
                "text/plain (=?utf-8?Q?yes?=); name=\"unterminated (=?utf-8?Q?no?=)",
                "text/plain (yes); name=\"unterminated (=?utf-8?Q?no?=)",
                false,
            ),
            (
                "attachment (\\=?utf-8?Q?no?=) (=?utf-8?Q?no\\thing?=)",
                "attachment (\\=?utf-8?Q?no?=) (=?utf-8?Q?no\\thing?=)",
                false,
            ),
        ] {
            let source = format!("{field}: {value}\r\n\r\n");
            let key = format!("header:{field}:asText");
            let expected = td_json::Json::from(expected).to_string();
            for width in 1..=8 {
                let mut work = work();
                let mut budget = HeaderBudget::new();
                let mut scratch = nfc::Scratch::new();
                let mut cursor = Text::new(
                    input(source.as_bytes(), &key),
                    &mut scratch,
                    &mut work,
                    &mut budget,
                )
                .unwrap();
                assert_eq!(
                    drain(&mut cursor, width),
                    expected.as_bytes(),
                    "{field}: {value}"
                );
                assert_eq!(cursor.is_encoding_problem(), problem);
            }
        }
    }
}
#[test]
fn comma_refusal_uses_the_text_owner_error() {
    let mut work = Meter::new(
        Deadline::after(Tick(0), 100).unwrap(),
        Charge {
            io_bytes: 10000,
            records: 10000,
            output_bytes: 4,
            ..Charge::default()
        },
    );
    let mut budget = HeaderBudget::new();
    let mut scratch = nfc::Scratch::new();
    let mut cursor = Text::new(
        input(b"Subject:a\nSubject:b\n\n", "header:Subject:asText:all"),
        &mut scratch,
        &mut work,
        &mut budget,
    )
    .unwrap();
    let mut output = [0xa5; 1];
    let mut prefix = Vec::new();
    let mut failed = false;
    for _ in 0..1000 {
        match cursor.poll(Tick(1), &mut output) {
            Ok(progress) => prefix.extend_from_slice(&output[..progress.written]),
            Err(error) => {
                assert_eq!(error, Error::Text(nfc::Error::Work(Stop::OutputBytes)));
                output.fill(0xa5);
                assert_eq!(cursor.poll(Tick(1), &mut output), Err(error));
                assert_eq!(output, [0xa5]);
                failed = true;
                break;
            }
        }
    }
    assert!(failed);
    assert_eq!(prefix, b"[\"a\"");
}

#[test]
fn description_and_user_fields_use_text_rules_in_email_and_body_parts() {
    for field in ["Content-Description", "X-Custom", "x-short", "X-"] {
        let source = format!("{field}: \u{fffd}\n{field}: (literal) \" =?utf-8?q?e=CC=81?= \"\n\n");
        for context in [
            header_property::Context::Email,
            header_property::Context::BodyPart,
        ] {
            for width in 1..=8 {
                for (suffix, expected) in [
                    ("", "\"(literal) \\\" é \\\"\""),
                    (":all", "[\"�\",\"(literal) \\\" é \\\"\"]"),
                ] {
                    let key = format!("header:{field}:asText{suffix}");
                    let mut request = header_property::Cursor::new(&key, context);
                    let mut work = work();
                    let property = loop {
                        if let header_property::Status::Complete(value) =
                            request.poll(Tick(1), &mut work).unwrap()
                        {
                            break value.unwrap();
                        }
                    };
                    let mut spec = input(source.as_bytes(), &key);
                    spec.property = property;
                    let mut budget = HeaderBudget::new();
                    let mut scratch = nfc::Scratch::new();
                    let mut cursor = Text::new(spec, &mut scratch, &mut work, &mut budget).unwrap();
                    assert_eq!(drain(&mut cursor, width), expected.as_bytes(), "{key}");
                    assert!(!cursor.is_encoding_problem());
                }
            }
        }
    }
    for (key, expected, diagnostic) in [
        ("header:x-custom:asText", "\"é\"", false),
        ("header:x-custom:asText:all", "[\"�\",\"é\"]", true),
        ("header:X-Missing:asText", "null", false),
        ("header:X-Missing:asText:all", "[]", false),
    ] {
        let mut work = work();
        let mut budget = HeaderBudget::new();
        let mut scratch = nfc::Scratch::new();
        let mut cursor = Text::new(
            input(b"X-Custom:\xff\nX-Custom:e\xcc\x81\n\n", key),
            &mut scratch,
            &mut work,
            &mut budget,
        )
        .unwrap();
        assert_eq!(drain(&mut cursor, 1), expected.as_bytes());
        assert_eq!(cursor.is_encoding_problem(), diagnostic);
    }
}
#[test]
fn text_grammar_admission_preserves_known_costs_and_bounds_prefix_work() {
    for (name, visits, steps, accepted) in [
        ("Subject", 7, 1, true),
        ("List-Id", 14, 2, true),
        ("Keywords", 16, 2, true),
        ("cOmMeNtS", 8, 1, true),
        ("cOnTeNt-DeScRiPtIoN", 19, 1, true),
        ("X-", 2, 1, true),
        ("x-long-header", 2, 1, true),
        ("X-Cats!", 16, 3, true),
        ("X-Custom", 18, 3, true),
        ("X-12345678901234567", 40, 3, true),
        ("X", 0, 1, false),
        ("X_Header", 18, 3, false),
        ("Content-Type", 12, 1, true),
        ("cOnTeNt-TyPe", 12, 1, true),
        ("X-Spam-Level", 14, 2, true),
        ("Unknown12345", 14, 2, false),
        ("Content-DispositioN", 38, 2, true),
        ("Unknown123456789012", 40, 3, false),
        ("Unknown", 16, 3, false),
    ] {
        let mut work = work();
        let initial = work.remaining();
        let mut budget = HeaderBudget::new();
        let mut scratch = nfc::Scratch::new();
        let mut workspace = (&mut scratch, crate::header_text::Grammar::Text);
        let result = TextMode::validate(name, Tick(1), &mut work, &mut budget, &mut workspace);
        if name == "Keywords" {
            assert!(workspace.1 == crate::header_text::Grammar::Keywords);
        } else if name == "List-Id" {
            assert!(workspace.1 == crate::header_text::Grammar::ListId);
        } else if name.eq_ignore_ascii_case("Content-Type")
            || name.eq_ignore_ascii_case("Content-Disposition")
        {
            assert!(workspace.1 == crate::header_text::Grammar::MimeParameters);
        } else {
            assert!(workspace.1 == crate::header_text::Grammar::Text);
        }
        assert_eq!(
            result,
            if accepted {
                Ok(())
            } else {
                Err(Error::UnsupportedGrammar)
            },
            "{name}"
        );
        assert_eq!(
            initial.io_bytes - work.remaining().io_bytes,
            visits,
            "{name}"
        );
        assert_eq!(initial.records - work.remaining().records, steps, "{name}");
        assert_eq!(16_000_000 - budget.steps_remaining(), steps);
        assert_eq!(16 * 1024 * 1024 - budget.source_bytes_remaining(), visits);
        assert_eq!(work.remaining().output_bytes, initial.output_bytes);
    }
}
#[test]
fn text_name_admission_refuses_before_json_on_every_partial_allowance() {
    for (field, visits, steps) in [
        ("Content-Description", 19, 1),
        ("Content-Type", 12, 1),
        ("X-Spam-Level", 14, 2),
        ("Content-Disposition", 38, 2),
        ("X-12345678901234567", 40, 3),
    ] {
        let key = format!("header:{field}:asText:all");
        let source = format!("{field}:secret\n\n");
        for (bytes, steps) in (0..visits)
            .map(|cut| (cut, steps))
            .chain((0..steps).map(|cut| (visits, cut)))
        {
            let mut work = work();
            let mut budget = HeaderBudget::new();
            budget
                .charge(
                    &mut self::work(),
                    Tick(1),
                    budget.source_bytes_remaining() - bytes,
                    budget.steps_remaining() - steps,
                    &mut 0,
                )
                .unwrap();
            let mut scratch = nfc::Scratch::new();
            let mut cursor = Text::new(
                input(source.as_bytes(), &key),
                &mut scratch,
                &mut work,
                &mut budget,
            )
            .unwrap();
            let mut output = [0xa5; 8];
            assert_eq!(
                cursor.poll(Tick(1), &mut output),
                Err(Error::InterpretationLimit)
            );
            assert_eq!(
                cursor.poll(Tick(1), &mut output),
                Err(Error::InterpretationLimit)
            );
            assert_eq!(output, [0xa5; 8]);
            assert_eq!(work.remaining().output_bytes, 1_000_000);
            assert_eq!(work.stopped(), None);
        }
    }
}

#[test]
fn prefix_admission_preserves_paid_comparison_when_job_budget_refuses() {
    for (field, cost, cases) in [
        (
            "X-12345678901234567",
            19,
            [
                (18, 2, Stop::IoBytes, 0),
                (19, 2, Stop::IoBytes, 19),
                (20, 2, Stop::IoBytes, 19),
                (37, 3, Stop::IoBytes, 19),
                (38, 3, Stop::IoBytes, 38),
                (39, 3, Stop::IoBytes, 38),
                (40, 0, Stop::Records, 0),
                (40, 1, Stop::Records, 19),
                (40, 2, Stop::Records, 38),
            ]
            .as_slice(),
        ),
        (
            "X-Spam-Level",
            12,
            [
                (11, 2, Stop::IoBytes, 0),
                (12, 2, Stop::IoBytes, 12),
                (13, 2, Stop::IoBytes, 12),
                (14, 0, Stop::Records, 0),
                (14, 1, Stop::Records, 12),
            ]
            .as_slice(),
        ),
    ] {
        let source = format!("{field}:secret\n\n");
        let key = format!("header:{field}:asText:all");
        for &(io_bytes, records, stop, paid) in cases {
            let mut work = Meter::new(
                Deadline::after(Tick(0), 100).unwrap(),
                Charge {
                    io_bytes,
                    records,
                    output_bytes: 100,
                    ..Charge::default()
                },
            );
            let mut budget = HeaderBudget::new();
            let mut scratch = nfc::Scratch::new();
            let mut cursor = Text::new(
                input(source.as_bytes(), &key),
                &mut scratch,
                &mut work,
                &mut budget,
            )
            .unwrap();
            let mut output = [0xa5; 8];
            assert_eq!(cursor.poll(Tick(1), &mut output), Err(Error::Work(stop)));
            assert_eq!(cursor.poll(Tick(1), &mut output), Err(Error::Work(stop)));
            assert_eq!(output, [0xa5; 8]);
            assert_eq!(work.stopped(), Some(stop));
            assert_eq!(work.remaining().io_bytes, io_bytes - paid);
            assert_eq!(work.remaining().records, records - paid / cost);
            assert_eq!(work.remaining().output_bytes, 100);
            assert_eq!(budget.source_bytes_remaining(), 16 * 1024 * 1024 - paid);
            assert_eq!(budget.steps_remaining(), 16_000_000 - paid / cost);
        }
    }
}

#[test]
fn structured_text_preserves_syntax_and_decodes_only_source_grammar_positions() {
    let cases: &[(&str, &[u8], &str, bool)] = &[
        (
            "Content-Type",
            b"text/plain (\\\xe1=?utf-8?Q?yes?=); name=\"(=?utf-8?Q?no?=)\"",
            "text/plain (\\�yes); name=\"(=?utf-8?Q?no?=)\"",
            true,
        ),
        (
            "Content-Disposition",
            b"attachment (\\\xe1=?utf-8?Q?yes?=); filename=\"(=?utf-8?Q?no?=)\"",
            "attachment (\\�yes); filename=\"(=?utf-8?Q?no?=)\"",
            true,
        ),
        (
            "List-Id",
            b" \"<\" =?utf-8?Q?yes?= <x>",
            "\"<\" yes <x>",
            false,
        ),
        ("List-Id", b" (<) =?utf-8?Q?yes?= <x>", "(<) yes <x>", false),
        ("Keywords", b" (\\\xe1=?utf-8?Q?yes?=)", "(\\�yes)", true),
        (
            "Keywords",
            b" (\\\xc3\xa9=?utf-8?Q?yes?=)",
            "(\\éyes)",
            false,
        ),
        (
            "List-Id",
            b" (\\\xe1=?utf-8?Q?yes?=) <x>",
            "(\\�yes) <x>",
            true,
        ),
        (
            "List-Id",
            b" (\\\xc3\xa9=?utf-8?Q?yes?=) <x>",
            "(\\éyes) <x>",
            false,
        ),
        (
            "Keywords",
            b" =?utf-8?Q?cafe=CC=81?= , plain",
            "café , plain",
            false,
        ),
        (
            "Keywords",
            b" =?utf-8?Q?one?= \r\n\t=?utf-8?B?dHdv?=",
            "onetwo",
            false,
        ),
        (
            "Keywords",
            b"\" =?utf-8?Q?quoted?= \" , =?utf-8?Q?yes?= ",
            "\" =?utf-8?Q?quoted?= \" , yes ",
            false,
        ),
        (
            "Keywords",
            b" =?utf-8?Q?no?=, =?utf-8?Q?yes?= ",
            "=?utf-8?Q?no?=, yes ",
            false,
        ),
        ("Keywords", b",=?utf-8?Q?no?= ", ",=?utf-8?Q?no?= ", false),
        (
            "Keywords",
            b" (=?utf-8?Q?one?= \t=?utf-8?Q?two?=)",
            "(onetwo)",
            false,
        ),
        (
            "Keywords",
            b" (=?utf-8?Q?one?=(=?utf-8?Q?two?=))",
            "(one(two))",
            false,
        ),
        (
            "Keywords",
            b" (\\=?utf-8?Q?no?=)",
            "(\\=?utf-8?Q?no?=)",
            false,
        ),
        ("Keywords", b" (\\x=?utf-8?Q?yes?=)", "(\\xyes)", false),
        (
            "Keywords",
            b" (x)=?utf-8?Q?no?= ",
            "(x)=?utf-8?Q?no?= ",
            false,
        ),
        ("Keywords", b" =?utf-8?Q?a,b?= ", "=?utf-8?Q?a,b?= ", false),
        ("Keywords", b" (=?utf-8?Q?a,b?=)", "(a,b)", false),
        (
            "Keywords",
            b" (=?utf-8?Q?a(b?=)",
            "(=?utf-8?Q?a(b?=)",
            false,
        ),
        (
            "Keywords",
            b" =?unknown?Q?one?= \t=?utf-8?Q?two?=",
            "=?unknown?Q?one?= \ttwo",
            false,
        ),
        ("Keywords", b" =?utf-8?Q?=FF=00=01?=", "�", true),
        ("Keywords", b" a\0\xff\xef\xb7\x90", "a��", true),
        (
            "Keywords",
            b" \"unterminated =?utf-8?Q?no?=",
            "\"unterminated =?utf-8?Q?no?=",
            false,
        ),
        (
            "List-Id",
            b" =?utf-8?Q?cafe=CC=81?= <list.example.org>",
            "café <list.example.org>",
            false,
        ),
        (
            "List-Id",
            b" \" =?utf-8?Q?no?= \" <list.example.org>",
            "\" =?utf-8?Q?no?= \" <list.example.org>",
            false,
        ),
        (
            "List-Id",
            b" (=?utf-8?Q?yes?=) <list.example.org>",
            "(yes) <list.example.org>",
            false,
        ),
        (
            "List-Id",
            b" < =?utf-8?Q?no?= > =?utf-8?Q?tail?=",
            "< =?utf-8?Q?no?= > =?utf-8?Q?tail?=",
            false,
        ),
        (
            "List-Id",
            b" =?utf-8?Q?no?=<list.example.org>",
            "=?utf-8?Q?no?=<list.example.org>",
            false,
        ),
        (
            "List-Id",
            b" \"a\\\" =?utf-8?Q?no?=\" =?utf-8?Q?yes?= <x>",
            "\"a\\\" =?utf-8?Q?no?=\" yes <x>",
            false,
        ),
    ];
    for &(field, value, expected, problem) in cases {
        let mut scratch = nfc::Scratch::new();
        for width in 1..=8 {
            let source = [field.as_bytes(), b":", value, b"\r\n\r\nbody"].concat();
            for all in [false, true] {
                let key = format!("header:{field}:asText{}", if all { ":all" } else { "" });
                let mut work = work();
                let mut budget = HeaderBudget::new();
                let mut cursor =
                    Text::new(input(&source, &key), &mut scratch, &mut work, &mut budget).unwrap();
                let encoded = td_json::Json::from(expected).to_string();
                let expected = if all { format!("[{encoded}]") } else { encoded };
                assert_eq!(
                    drain(&mut cursor, width),
                    expected.as_bytes(),
                    "{field} {value:?}"
                );
                assert_eq!(cursor.is_encoding_problem(), problem);
            }
        }
    }
}

#[test]
fn structured_text_selection_scratch_replay_and_refusal_keep_original_budgets() {
    for field in ["Keywords", "List-Id", "Content-Type", "Content-Disposition"] {
        let mime = field.starts_with("Content-");
        let word = if mime {
            "(=?utf-8?Q?cafe=CC=81?=)"
        } else {
            "=?utf-8?Q?cafe=CC=81?="
        };
        let decoded = if mime { "(café)" } else { "café" };
        let long = format!(" a{}\u{323}", "\u{301}".repeat(300));
        let source = format!("{field}:{long}\r\n{field}: {word}\r\n\r\n");
        let expected = format!("[\"ạ{}\",\"{decoded}\"]", "\u{301}".repeat(300));
        let key = format!("header:{field}:asText:all");
        let mut scratch = nfc::Scratch::new();
        for width in 1..=8 {
            let mut work = work();
            let mut budget = HeaderBudget::new();
            let mut cursor = Text::new(
                input(source.as_bytes(), &key),
                &mut scratch,
                &mut work,
                &mut budget,
            )
            .unwrap();
            assert_eq!(drain(&mut cursor, width), expected.as_bytes());
            assert!(!cursor.is_encoding_problem());
        }
        for (source, expected) in [(b"".as_slice(), b"[]".as_slice()), (b"Other:a\n\n", b"[]")] {
            let mut work = work();
            let mut budget = HeaderBudget::new();
            let mut cursor =
                Text::new(input(source, &key), &mut scratch, &mut work, &mut budget).unwrap();
            assert_eq!(drain(&mut cursor, 1), expected);
        }
        let depth32 = format!(
            "{field}: {}=?utf-8?Q?yes?={}\r\n\r\n",
            "(".repeat(32),
            ")".repeat(32)
        );
        let expected32 = format!("[\"{}yes{}\"]", "(".repeat(32), ")".repeat(32));
        let mut admitted_work = work();
        let mut admitted_budget = HeaderBudget::new();
        let mut admitted = Text::new(
            input(depth32.as_bytes(), &key),
            &mut scratch,
            &mut admitted_work,
            &mut admitted_budget,
        )
        .unwrap();
        assert_eq!(drain(&mut admitted, 1), expected32.as_bytes());
        for first in ["\"open", "(open"] {
            let word = "=?utf-8?Q?a,b?= (=?utf-8?Q?yes?=)";
            let decoded = "=?utf-8?Q?a,b?= (yes)";
            let source = format!("{field}:{first}\r\n{field}: {word}\r\n\r\n");
            let expected = td_json::Json::from(vec![first, decoded]).to_string();
            let mut work = work();
            let mut budget = HeaderBudget::new();
            let mut cursor = Text::new(
                input(source.as_bytes(), &key),
                &mut scratch,
                &mut work,
                &mut budget,
            )
            .unwrap();
            assert_eq!(drain(&mut cursor, 1), expected.as_bytes());
        }
        let source = format!("{field}: {}x{}\r\n\r\n", "(".repeat(33), ")".repeat(33));
        let mut work = work();
        let mut budget = HeaderBudget::new();
        let mut cursor = Text::new(
            input(source.as_bytes(), &key),
            &mut scratch,
            &mut work,
            &mut budget,
        )
        .unwrap();
        let mut output = [0; 1];
        let mut refused = false;
        for _ in 0..10000 {
            match cursor.poll(Tick(1), &mut output) {
                Ok(progress) => assert!(!matches!(progress.status, Status::Complete(_))),
                Err(error) => {
                    assert_eq!(
                        error,
                        Error::Json(json_string::Error::Source(nfc::Error::InterpretationLimit))
                    );
                    output[0] = 0xa5;
                    assert_eq!(cursor.poll(Tick(1), &mut output), Err(error));
                    assert_eq!(cursor.check_deadline(Tick(1)), Err(error));
                    assert_eq!(output, [0xa5]);
                    refused = true;
                    break;
                }
            }
        }
        assert!(refused);
    }
}

#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;
use crate::{header_property, ports::Deadline};
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
fn input<'a>(bytes: &'a [u8], key: &'a str) -> Input<'a> {
    let mut cursor = header_property::Cursor::new(key, header_property::Context::Email);
    let mut work = work(1000);
    for _ in 0..1000 {
        if let header_property::Status::Complete(value) = cursor.poll(Tick(1), &mut work).unwrap() {
            return Input {
                bytes,
                base: 4096,
                header_limit: bytes.len() as u64,
                property: value.unwrap(),
                source_end: header_select::SourceEnd::Eof,
            };
        }
    }
    panic!("property did not finish");
}
fn drain(cursor: &mut Cursor<'_, '_>, width: usize) -> (Vec<u8>, End) {
    assert!(std::mem::size_of_val(cursor) <= 2560);
    let mut bytes = Vec::new();
    for _ in 0..100_000 {
        let mut output = [0xa5; 8];
        let progress = cursor.poll(Tick(1), &mut output[..width]).unwrap();
        assert!(progress.written <= 6);
        bytes.extend_from_slice(&output[..progress.written]);
        assert!(output[progress.written..].iter().all(|byte| *byte == 0xa5));
        if let Status::Complete(end) = progress.status {
            return (bytes, end);
        }
    }
    panic!("property did not finish");
}
const CASES: &[(&str, &[u8], &str, bool)] = &[
    ("asRaw", b" \xff\0", "\" �\"", true),
    ("asText", b" =?utf-8?q?cafe=CC=81?=", "\"café\"", false),
    (
        "asAddresses",
        b" Jo <a@b>",
        r#"[{"name":"Jo","email":"a@b"}]"#,
        false,
    ),
    (
        "asGroupedAddresses",
        b" G:a@b;",
        r#"[{"name":"G","addresses":[{"name":null,"email":"a@b"}]}]"#,
        false,
    ),
    ("asMessageIds", b" <a@b> <c@d>", r#"["a@b","c@d"]"#, false),
    (
        "asDate",
        b" 1 Jan 2000 00:00 +0000",
        "\"2000-01-01T00:00:00Z\"",
        false,
    ),
    (
        "asURLs",
        b" <https://x.test/>",
        r#"["https://x.test/"]"#,
        false,
    ),
];
#[test]
fn selected_forms_share_scratch_and_preserve_values_occurrences_and_absence() {
    let mut scratch = nfc::Scratch::new();
    for &(form, value, expected, problem) in CASES {
        let source = [b"X-Value:".as_slice(), value, b"\n\n"].concat();
        for all in [false, true] {
            let key = format!("header:X-Value:{form}{}", if all { ":all" } else { "" });
            let expected = if all {
                format!("[{expected}]")
            } else {
                expected.to_owned()
            };
            for width in 1..=8 {
                for missing in [false, true] {
                    let source = if missing {
                        b"Other:ignored\n\n".as_slice()
                    } else {
                        &source
                    };
                    let expected = if missing {
                        if all {
                            "[]"
                        } else {
                            "null"
                        }
                    } else {
                        &expected
                    };
                    let mut work = work(10_000);
                    let mut budget = HeaderBudget::new();
                    let mut cursor =
                        Cursor::new(input(source, &key), &mut scratch, &mut work, &mut budget)
                            .unwrap();
                    assert_eq!(
                        cursor.poll(Tick(1), &mut []),
                        Ok(Progress {
                            written: 0,
                            status: Status::NeedOutput
                        })
                    );
                    let (bytes, end) = drain(&mut cursor, width);
                    assert_eq!(bytes, expected.as_bytes(), "{key}");
                    assert_eq!(end.body_start, 4096 + source.len() as u64);
                    assert_eq!(cursor.is_encoding_problem(), problem && !missing);
                    assert!(!cursor.has_unverified_leap());
                    assert_eq!(
                        cursor.poll(Tick(100), &mut []),
                        Ok(Progress {
                            written: 0,
                            status: Status::Complete(end)
                        })
                    );
                    cursor.check_deadline(Tick(1)).unwrap();
                    assert!(cursor.check_deadline(Tick(100)).is_err());
                    assert!(cursor.poll(Tick(1), &mut [0xa5]).is_err());
                    assert!(work.remaining().output_bytes <= 10_000 - bytes.len() as u64);
                    assert!(budget.steps_remaining() < 16_000_000);
                }
            }
        }
    }
}
#[test]
fn diagnostics_and_unsupported_grammar_remain_selected_value_policy() {
    for (key, source, expected, encoding, leap) in [
        (
            "header:X-Custom:asText",
            b"X-Custom:\xff\n\n".as_slice(),
            "\"�\"",
            true,
            false,
        ),
        (
            "header:Message-ID:asMessageIds",
            "Message-ID:<\u{fdd0}@b>\n\n".as_bytes(),
            r#"["�@b"]"#,
            true,
            false,
        ),
        (
            "header:Date:asDate",
            b"Date:31 Dec 2020 23:59:60 +0000\n\n".as_slice(),
            "null",
            false,
            true,
        ),
        (
            "header:To:asAddresses",
            b"To:=?utf-8?q?=FF?= <a@b>\n\n",
            r#"[{"name":"�","email":"a@b"}]"#,
            true,
            false,
        ),
        (
            "header:To:asGroupedAddresses",
            b"To:=?utf-8?q?=FF?= :;\n\n",
            r#"[{"name":"�","addresses":[]}]"#,
            true,
            false,
        ),
    ] {
        let mut work = work(1000);
        let mut budget = HeaderBudget::new();
        let mut scratch = nfc::Scratch::new();
        let mut cursor =
            Cursor::new(input(source, key), &mut scratch, &mut work, &mut budget).unwrap();
        assert_eq!(drain(&mut cursor, 1).0, expected.as_bytes());
        assert_eq!(cursor.is_encoding_problem(), encoding);
        assert_eq!(cursor.has_unverified_leap(), leap);
    }
    let mut work = work(1000);
    let mut budget = HeaderBudget::new();
    let mut scratch = nfc::Scratch::new();
    let mut cursor = Cursor::new(
        input(b"Keywords:secret\n\n", "header:Keywords:asText"),
        &mut scratch,
        &mut work,
        &mut budget,
    )
    .unwrap();
    let mut output = [0xa5; 8];
    assert_eq!(
        cursor.poll(Tick(1), &mut output),
        Err(Error::UnsupportedGrammar)
    );
    assert_eq!(
        cursor.poll(Tick(1), &mut output),
        Err(Error::UnsupportedGrammar)
    );
    assert_eq!(
        cursor.check_deadline(Tick(1)),
        Err(Error::UnsupportedGrammar)
    );
    assert_eq!(output, [0xa5; 8]);
    assert_eq!(work.remaining().output_bytes, 1000);
}
#[test]
fn all_selected_forms_latch_partial_output_refusal_without_touching_the_sink() {
    for &(form, value, _, _) in CASES {
        let source = [b"X-Value:".as_slice(), value, b"\n\n"].concat();
        let key = format!("header:X-Value:{form}:all");
        let mut work = work(1);
        let mut budget = HeaderBudget::new();
        let mut scratch = nfc::Scratch::new();
        let mut cursor =
            Cursor::new(input(&source, &key), &mut scratch, &mut work, &mut budget).unwrap();
        let mut emitted = Vec::new();
        let mut failed = false;
        for _ in 0..1000 {
            let mut output = [0xa5; 1];
            match cursor.poll(Tick(1), &mut output) {
                Ok(progress) => {
                    emitted.extend_from_slice(&output[..progress.written]);
                    assert!(!matches!(progress.status, Status::Complete(_)));
                }
                Err(error) => {
                    assert_eq!(output, [0xa5]);
                    assert_eq!(cursor.poll(Tick(1), &mut output), Err(error));
                    assert_eq!(cursor.check_deadline(Tick(1)), Err(error));
                    failed = true;
                    break;
                }
            }
        }
        assert!(failed, "{key}");
        assert_eq!(emitted, b"[");
        assert_eq!(work.stopped(), Some(Stop::OutputBytes));
    }
}

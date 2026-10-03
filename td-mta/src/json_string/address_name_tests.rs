#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;
use crate::{
    header_name::{Extent, Kind},
    nfc::{HeaderBudget, Scratch},
    ports::Deadline,
};
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
fn limited(bytes: u64, steps: u64) -> HeaderBudget {
    let mut budget = HeaderBudget::new();
    budget
        .charge(
            &mut work(),
            Tick(1),
            budget.source_bytes_remaining() - bytes,
            budget.steps_remaining() - steps,
            &mut 0,
        )
        .unwrap();
    budget
}
#[derive(Clone, Copy)]
enum Mode {
    Address(header_address_text::Mode),
    Name(Kind),
}
fn remaining(cursor: &Cursor<'_, '_, '_>) -> (Charge, u64) {
    match &cursor.source {
        Source::BudgetedAddress(cursor) => cursor.remaining(),
        Source::Name(cursor) => cursor.remaining().unwrap(),
        _ => panic!("unexpected source"),
    }
}
fn drain(cursor: &mut Cursor<'_, '_, '_>, width: usize) -> (Vec<u8>, bool, Result<(), Error>) {
    assert!(std::mem::size_of_val(cursor) <= 64);
    let mut text = Vec::new();
    for _ in 0..100_000 {
        let before = remaining(cursor);
        let mut output = vec![0xa5; width];
        let step = cursor.poll(Tick(1), &mut output);
        let after = remaining(cursor);
        let (visits, steps) = if matches!(cursor.source, Source::Name(_)) {
            (255, 256)
        } else {
            (160, 255)
        };
        assert!(before.0.io_bytes - after.0.io_bytes <= visits);
        assert!(before.0.records - after.0.records <= 16);
        assert!(before.1 - after.1 <= steps);
        let max_output = if matches!(cursor.source, Source::Name(_)) {
            6
        } else {
            8
        };
        assert!(before.0.output_bytes - after.0.output_bytes <= max_output);
        match step {
            Ok(progress) => {
                assert!(progress.written <= 6);
                text.extend_from_slice(&output[..progress.written]);
                if progress.status == Status::Complete {
                    assert_eq!(
                        cursor.poll(Tick(100), &mut output),
                        Ok(Progress {
                            written: 0,
                            status: Status::Complete
                        })
                    );
                    assert_eq!(remaining(cursor), after);
                    return (text, cursor.is_encoding_problem(), Ok(()));
                }
            }
            Err(error) => {
                assert!(output.iter().all(|byte| *byte == 0xa5));
                assert_eq!(cursor.poll(Tick(1), &mut output), Err(error));
                assert_eq!(cursor.check_deadline(Tick(1)), Err(error));
                assert_eq!(remaining(cursor), after);
                return (text, cursor.is_encoding_problem(), Err(error));
            }
        }
    }
    panic!("budgeted address/name JSON did not finish");
}
fn run(
    source: &[u8],
    mode: Mode,
    width: usize,
    work: &mut Meter,
    budget: &mut HeaderBudget,
) -> (Vec<u8>, bool, Result<(), Error>) {
    match mode {
        Mode::Address(mode) => {
            let mut source = header_address_text::Budgeted::new(source, mode, work, budget);
            drain(&mut Cursor::from_budgeted_address(&mut source), width)
        }
        Mode::Name(kind) => {
            let mut scratch = Scratch::new();
            let mut source = header_name::Cursor::new(
                source,
                Extent {
                    start: 0,
                    end: source.len(),
                },
                kind,
                work,
                budget,
                &mut scratch,
            )
            .unwrap();
            drain(&mut Cursor::from_name(&mut source), width)
        }
    }
}
#[test]
fn short_buffers_preserve_address_identity_and_normalize_only_names() {
    for (source, mode, expected, diagnostic) in [
        (
            "e\u{301}@EXAMPLE",
            Mode::Address(header_address_text::Mode::Parsed),
            "\"e\u{301}@EXAMPLE\"",
            false,
        ),
        (
            " \tbad\r\n addr ",
            Mode::Address(header_address_text::Mode::Fallback),
            "\"bad addr\"",
            false,
        ),
        (
            "\"\\\0\"@b",
            Mode::Address(header_address_text::Mode::Parsed),
            "\"\\\"\\\\\\u0000\\\"@b\"",
            false,
        ),
        (
            "\u{fdd0}@b",
            Mode::Address(header_address_text::Mode::Parsed),
            "\"�@b\"",
            true,
        ),
        (
            "\"a\\\"b\\\\c\"",
            Mode::Name(Kind::Phrase),
            "\"a\\\"b\\\\c\"",
            false,
        ),
        (
            "=?utf-8?q?e?= =?utf-8?q?=CC=81?=",
            Mode::Name(Kind::Phrase),
            "\"é\"",
            false,
        ),
        ("(e\u{301})", Mode::Name(Kind::Comment), "\"é\"", false),
        ("()", Mode::Name(Kind::Comment), "\"\"", false),
        ("=?utf-8?q?=FF?=", Mode::Name(Kind::Phrase), "\"�\"", true),
    ] {
        for width in 1..=8 {
            assert_eq!(
                run(
                    source.as_bytes(),
                    mode,
                    width,
                    &mut work(),
                    &mut HeaderBudget::new()
                ),
                (expected.as_bytes().to_vec(), diagnostic, Ok(())),
                "{source:?}"
            );
        }
    }
}
#[test]
fn quoting_adds_exact_wire_bytes_without_extra_grammar_or_nfc_steps() {
    for (source, mode) in [
        (
            b"a@b".as_slice(),
            Mode::Address(header_address_text::Mode::Parsed),
        ),
        (
            b"\0\xff",
            Mode::Address(header_address_text::Mode::Fallback),
        ),
        (b"=?utf-8?q?e=CC=81?=", Mode::Name(Kind::Phrase)),
        (b"(e\xcc\x81)", Mode::Name(Kind::Comment)),
    ] {
        let mut work = work();
        let mut budget = HeaderBudget::new();
        let (json, _, result) = run(source, mode, 1, &mut work, &mut budget);
        result.unwrap();
        let mut reference = self::work();
        let mut reference_budget = HeaderBudget::new();
        match mode {
            Mode::Address(mode) => {
                let mut source = header_address_text::Budgeted::new(
                    source,
                    mode,
                    &mut reference,
                    &mut reference_budget,
                );
                while source.poll(Tick(1)).unwrap() != header_address_text::Status::Complete {}
            }
            Mode::Name(kind) => {
                let mut scratch = Scratch::new();
                let mut source = header_name::Cursor::new(
                    source,
                    Extent {
                        start: 0,
                        end: source.len(),
                    },
                    kind,
                    &mut reference,
                    &mut reference_budget,
                    &mut scratch,
                )
                .unwrap();
                while source.poll(Tick(1)).unwrap() != nfc::Status::Complete {}
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
}
fn aggregate(error: Error) -> bool {
    matches!(
        error,
        Error::Address(header_address_text::Error::InterpretationLimit)
            | Error::Name(header_name::Error::Normalize(
                nfc::Error::InterpretationLimit
            ))
            | Error::Name(header_name::Error::Phrase(
                crate::header_phrase::Error::InterpretationLimit
            ))
            | Error::Name(header_name::Error::Comment(
                crate::header_comment::Error::InterpretationLimit
            ))
    )
}
fn output_stop(error: Error) -> bool {
    matches!(
        error,
        Error::Address(header_address_text::Error::Work(Stop::OutputBytes))
            | Error::Name(header_name::Error::Normalize(nfc::Error::Work(
                Stop::OutputBytes
            )))
            | Error::Name(header_name::Error::Phrase(
                crate::header_phrase::Error::Work(Stop::OutputBytes)
            ))
            | Error::Name(header_name::Error::Comment(
                crate::header_comment::Error::Work(Stop::OutputBytes)
            ))
    )
}
#[test]
fn every_partial_resource_allowance_retires_the_provisional_json() {
    let mut late = false;
    let mut quote = false;
    for (source, mode) in [
        (
            b"\"\\\0\"@b".as_slice(),
            Mode::Address(header_address_text::Mode::Parsed),
        ),
        (
            b" \xffx\xe2\x82 ",
            Mode::Address(header_address_text::Mode::Fallback),
        ),
        (b"\"a\\\"b\\\\c\"", Mode::Name(Kind::Phrase)),
        (b"(e\xcc\x81 x)", Mode::Name(Kind::Comment)),
    ] {
        let mut work = work();
        let mut budget = HeaderBudget::new();
        let expected = run(source, mode, 1, &mut work, &mut budget);
        expected.2.unwrap();
        let visits = 16 * 1024 * 1024 - budget.source_bytes_remaining();
        let steps = 16_000_000 - budget.steps_remaining();
        let output = 1_000_000 - work.remaining().output_bytes;
        for (bytes, steps) in (0..steps)
            .map(|cut| (visits, cut))
            .chain((0..visits).map(|cut| (cut, steps)))
        {
            let mut work = self::work();
            let mut budget = limited(bytes, steps);
            let result = run(source, mode, 1, &mut work, &mut budget);
            assert!(aggregate(result.2.unwrap_err()));
            assert!(expected.0.starts_with(&result.0));
            late |= result.0.len() > 1;
            assert_eq!(work.stopped(), None);
        }
        for output_bytes in 0..output {
            let mut work = Meter::new(
                Deadline::after(Tick(0), 100).unwrap(),
                Charge {
                    output_bytes,
                    ..self::work().remaining()
                },
            );
            let result = run(source, mode, 1, &mut work, &mut HeaderBudget::new());
            let error = result.2.unwrap_err();
            assert!(output_stop(error));
            if let Mode::Name(kind) = mode {
                if output_bytes == 0 {
                    let expected_error = match kind {
                        Kind::Phrase => Error::Name(header_name::Error::Phrase(
                            crate::header_phrase::Error::Work(Stop::OutputBytes),
                        )),
                        Kind::Comment => Error::Name(header_name::Error::Comment(
                            crate::header_comment::Error::Work(Stop::OutputBytes),
                        )),
                    };
                    assert_eq!(error, expected_error);
                    assert!(result.0.is_empty());
                }
                if output_bytes == expected.0.len() as u64 - 1 {
                    assert_eq!(
                        error,
                        Error::Name(header_name::Error::Normalize(nfc::Error::Work(
                            Stop::OutputBytes
                        ),))
                    );
                    assert_eq!(result.0, expected.0[..expected.0.len() - 1]);
                }
            }
            assert!(expected.0.starts_with(&result.0));
            quote |= result.0.len() == expected.0.len() - 1;
        }
        let mut exact_work = Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                output_bytes: output,
                ..self::work().remaining()
            },
        );
        assert_eq!(
            run(
                source,
                mode,
                1,
                &mut exact_work,
                &mut limited(visits, steps)
            ),
            expected
        );
    }
    assert!(late && quote);
}
#[test]
fn malformed_sources_and_post_completion_deadline_are_still_terminal() {
    for (source, mode, expected) in [
        (
            b"a@b bad".as_slice(),
            Mode::Address(header_address_text::Mode::Parsed),
            Error::Address(header_address_text::Error::Malformed),
        ),
        (
            b"word (bad",
            Mode::Name(Kind::Phrase),
            Error::Name(header_name::Error::Phrase(
                crate::header_phrase::Error::Malformed,
            )),
        ),
        (
            b"(name) tail",
            Mode::Name(Kind::Comment),
            Error::Name(header_name::Error::Comment(
                crate::header_comment::Error::Malformed,
            )),
        ),
    ] {
        let result = run(source, mode, 1, &mut work(), &mut HeaderBudget::new());
        assert_eq!(result, (b"\"".to_vec(), false, Err(expected)));
    }
    let mut work = work();
    let mut budget = HeaderBudget::new();
    let mut scratch = Scratch::new();
    let mut name = header_name::Cursor::new(
        b"()",
        Extent { start: 0, end: 2 },
        Kind::Comment,
        &mut work,
        &mut budget,
        &mut scratch,
    )
    .unwrap();
    let mut cursor = Cursor::from_name(&mut name);
    let before = remaining(&cursor);
    assert_eq!(
        cursor.poll(Tick(1), &mut []),
        Ok(Progress {
            written: 0,
            status: Status::NeedOutput
        })
    );
    assert_eq!(remaining(&cursor), before);
    assert_eq!(drain(&mut cursor, 1), (b"\"\"".to_vec(), false, Ok(())));
    let error = Error::Name(header_name::Error::Normalize(nfc::Error::Work(
        Stop::Deadline,
    )));
    assert_eq!(cursor.check_deadline(Tick(100)), Err(error));
    assert_eq!(cursor.poll(Tick(1), &mut [0]), Err(error));
    let mut work = self::work();
    let mut budget = HeaderBudget::new();
    let mut address = header_address_text::Budgeted::new(
        b"a@b",
        header_address_text::Mode::Parsed,
        &mut work,
        &mut budget,
    );
    let mut cursor = Cursor::from_budgeted_address(&mut address);
    assert_eq!(drain(&mut cursor, 1).2, Ok(()));
    let error = Error::Address(header_address_text::Error::Work(Stop::Deadline));
    assert_eq!(cursor.check_deadline(Tick(100)), Err(error));
    assert_eq!(cursor.poll(Tick(1), &mut [0]), Err(error));
}

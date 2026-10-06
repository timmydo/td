#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;
use crate::{
    time::Deadline,
    work::{Charge, Stop},
};
fn work() -> Meter {
    Meter::new(
        Deadline::after(Tick(0), 100).unwrap(),
        Charge {
            io_bytes: 100_000_000,
            records: 2_000_000,
            output_bytes: 100,
            ..Charge::default()
        },
    )
}
fn limited(bytes: u64, steps: u64) -> HeaderBudget {
    let mut budget = HeaderBudget::new();
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
fn remaining(cursor: &Cursor<'_, '_>) -> (Charge, u64) {
    match &cursor.owner {
        Owner::Validate { work, budget, .. } => (work.remaining(), budget.steps_remaining()),
        Owner::Normalize(cursor) => cursor.remaining(),
        Owner::Retired => panic!("unexpected retired owner"),
    }
}
fn drain(cursor: &mut Cursor<'_, '_>) -> (String, bool, Result<(), Error>) {
    assert!(std::mem::size_of_val(cursor) <= 1280);
    let mut text = String::new();
    for _ in 0..1_000_000 {
        let (before, steps) = remaining(cursor);
        let status = cursor.poll(Tick(1));
        let (after, left) = remaining(cursor);
        assert!(before.io_bytes - after.io_bytes <= 255);
        assert!(before.records - after.records <= 16);
        assert!(steps - left <= 256);
        assert_eq!(before.output_bytes, after.output_bytes);
        match status {
            Ok(Status::Yield) => {}
            Ok(Status::Scalar(value)) => text.push(value),
            Ok(Status::Complete) => {
                assert_eq!(cursor.poll(Tick(100)), Ok(Status::Complete));
                assert_eq!(remaining(cursor), (after, left));
                return (text, cursor.is_encoding_problem(), Ok(()));
            }
            Err(error) => return (text, cursor.is_encoding_problem(), Err(error)),
        }
    }
    panic!("display name did not finish");
}
fn extent(source: &[u8]) -> Extent {
    Extent {
        start: 0,
        end: source.len(),
    }
}
#[test]
fn selected_phrase_and_comment_names_validate_decode_and_normalize() {
    for (source, kind, expected, problem) in [
        ("e\u{301}", Kind::Phrase, "é", false),
        ("\"\"", Kind::Phrase, "", false),
        ("=?utf-8?q?e?= =?utf-8?q?=CC=81?=", Kind::Phrase, "é", false),
        (
            "=?utf-8?q?e?= (x) =?utf-8?q?=CC=81?=",
            Kind::Phrase,
            "e \u{301}",
            false,
        ),
        ("\"\\\0e\u{301}\u{fdd0}\"", Kind::Phrase, "é�", true),
        ("=?utf-8?q?=FF?=", Kind::Phrase, "�", true),
        ("(e\u{301})", Kind::Comment, "é", false),
        ("()", Kind::Comment, "", false),
        (
            "(=?utf-8?q?e?= =?utf-8?q?=CC=81?=)",
            Kind::Comment,
            "é",
            false,
        ),
    ] {
        let field = format!("before {source} after");
        let mut work = work();
        let mut budget = HeaderBudget::new();
        let mut scratch = Scratch::new();
        let mut cursor = Cursor::new(
            field.as_bytes(),
            Extent {
                start: 7,
                end: 7 + source.len(),
            },
            kind,
            &mut work,
            &mut budget,
            &mut scratch,
        )
        .unwrap();
        assert_eq!(
            drain(&mut cursor),
            (expected.to_owned(), problem, Ok(())),
            "{source}"
        );
    }
    let tail = format!(
        "=?utf-8?q?=C3=A9?={} =?utf-8?q?z?=",
        " =?utf-8?q?=CC=95=CD=84?=".repeat(257)
    );
    let expected = format!(
        "é{}{}z",
        "\u{308}\u{301}".repeat(257),
        "\u{315}".repeat(257)
    );
    let comment = format!("({tail})");
    let mut work = work();
    let mut budget = HeaderBudget::new();
    let mut scratch = Scratch::new();
    for (source, kind) in [
        (tail.as_bytes(), Kind::Phrase),
        (comment.as_bytes(), Kind::Comment),
    ] {
        let mut cursor = Cursor::new(
            source,
            extent(source),
            kind,
            &mut work,
            &mut budget,
            &mut scratch,
        )
        .unwrap();
        assert_eq!(drain(&mut cursor), (expected.clone(), false, Ok(())));
    }
}
#[test]
fn malformed_or_nested_name_never_emits_normalized_text() {
    let nested = "(".repeat(33);
    for (source, kind, expected) in [
        (
            b"a@b".as_slice(),
            Kind::Phrase,
            Error::Phrase(header_phrase::Error::Malformed),
        ),
        (
            b"word (bad",
            Kind::Phrase,
            Error::Phrase(header_phrase::Error::Malformed),
        ),
        (
            b"\xff",
            Kind::Phrase,
            Error::Phrase(header_phrase::Error::Malformed),
        ),
        (
            b"(name) extra",
            Kind::Comment,
            Error::Comment(header_comment::Error::Malformed),
        ),
        (
            b" (name)",
            Kind::Comment,
            Error::Comment(header_comment::Error::Malformed),
        ),
        (
            nested.as_bytes(),
            Kind::Comment,
            Error::Comment(header_comment::Error::NestingLimit),
        ),
    ] {
        let mut work = work();
        let mut budget = HeaderBudget::new();
        let mut scratch = Scratch::new();
        let mut cursor = Cursor::new(
            source,
            extent(source),
            kind,
            &mut work,
            &mut budget,
            &mut scratch,
        )
        .unwrap();
        assert_eq!(drain(&mut cursor), (String::new(), false, Err(expected)));
        let before = remaining(&cursor);
        assert_eq!(cursor.poll(Tick(1)), Err(expected));
        assert_eq!(cursor.check_deadline(Tick(1)), Err(expected));
        assert_eq!(remaining(&cursor), before);
        assert!(matches!(cursor.owner, Owner::Validate { .. }));
    }
}
fn interpretation(error: Error) -> bool {
    matches!(
        error,
        Error::Phrase(header_phrase::Error::InterpretationLimit)
            | Error::Comment(header_comment::Error::InterpretationLimit)
            | Error::Normalize(nfc::Error::InterpretationLimit)
    )
}
#[test]
fn every_partial_allowance_latches_across_validation_normalization_and_fields() {
    let mut parsed = false;
    let mut normalized = false;
    let mut late = false;
    for (source, kind) in [
        ("=?utf-8?q?e?= =?utf-8?q?=CC=81?= x", Kind::Phrase),
        ("(e\u{301} x)", Kind::Comment),
        ("word (bad", Kind::Phrase),
    ] {
        let source = source.as_bytes();
        let mut work = work();
        let mut budget = HeaderBudget::new();
        let mut scratch = Scratch::new();
        let expected = drain(
            &mut Cursor::new(
                source,
                extent(source),
                kind,
                &mut work,
                &mut budget,
                &mut scratch,
            )
            .unwrap(),
        );
        let visits = 16 * 1024 * 1024 - budget.source_bytes_remaining();
        let steps = 16_000_000 - budget.steps_remaining();
        for (bytes, steps) in (0..steps)
            .map(|cut| (visits, cut))
            .chain((0..visits).map(|cut| (cut, steps)))
        {
            let mut work = self::work();
            let mut budget = limited(bytes, steps);
            let mut scratch = Scratch::new();
            let mut cursor = Cursor::new(
                source,
                extent(source),
                kind,
                &mut work,
                &mut budget,
                &mut scratch,
            )
            .unwrap();
            let (text, _, result) = drain(&mut cursor);
            let error = result.unwrap_err();
            assert!(interpretation(error));
            assert!(expected.0.starts_with(&text));
            late |= !text.is_empty();
            parsed |= matches!(cursor.owner, Owner::Validate { .. });
            normalized |= matches!(cursor.owner, Owner::Normalize(_));
            let before = remaining(&cursor);
            assert_eq!(cursor.poll(Tick(1)), Err(error));
            assert_eq!(cursor.check_deadline(Tick(1)), Err(error));
            assert_eq!(remaining(&cursor), before);
            assert_eq!(work.stopped(), None);
            assert_eq!(
                Cursor::new(
                    b"()",
                    extent(b"()"),
                    Kind::Comment,
                    &mut work,
                    &mut budget,
                    &mut scratch
                )
                .unwrap()
                .poll(Tick(1))
                .unwrap_err(),
                Error::Comment(header_comment::Error::InterpretationLimit)
            );
        }
        let mut work = self::work();
        let mut budget = limited(visits, steps);
        assert_eq!(
            drain(
                &mut Cursor::new(
                    source,
                    extent(source),
                    kind,
                    &mut work,
                    &mut budget,
                    &mut scratch
                )
                .unwrap()
            ),
            expected
        );
    }
    assert!(parsed && normalized && late);
}
#[test]
fn composed_costs_match_separate_validation_and_normalization_with_private_credit() {
    for (source, kind) in [
        (b"\"\"".as_slice(), Kind::Phrase),
        (b"=?utf-8?q?e=CC=81?=", Kind::Phrase),
        (b"()", Kind::Comment),
        (b"(e\xcc\x81)", Kind::Comment),
    ] {
        let mut work = work();
        let mut budget = HeaderBudget::new();
        let mut scratch = Scratch::new();
        let expected = drain(
            &mut Cursor::new(
                source,
                extent(source),
                kind,
                &mut work,
                &mut budget,
                &mut scratch,
            )
            .unwrap(),
        );
        let mut reference_work = self::work();
        let mut reference_budget = HeaderBudget::new();
        let mut reference_scratch = Scratch::new();
        let mut credit = 0;
        let mut normalized = match kind {
            Kind::Phrase => {
                let mut grammar = header_phrase::Cursor::new(source);
                while !matches!(
                    grammar
                        .poll_with_work(
                            Tick(1),
                            &mut Parsing::new(
                                &mut reference_work,
                                &mut reference_budget,
                                &mut credit
                            )
                        )
                        .unwrap(),
                    header_phrase::Status::Complete(_)
                ) {}
                nfc::Cursor::from_phrase(
                    grammar.into_validated().unwrap(),
                    source,
                    extent(source),
                    &mut reference_scratch,
                    &mut reference_work,
                    &mut reference_budget,
                )
                .unwrap()
            }
            Kind::Comment => {
                let mut grammar = header_comment::Cursor::new(source);
                while grammar
                    .poll_with_work(
                        Tick(1),
                        &mut Parsing::new(&mut reference_work, &mut reference_budget, &mut credit),
                    )
                    .unwrap()
                    != header_comment::Status::Complete
                {}
                nfc::Cursor::from_comment(
                    grammar.into_validated().unwrap(),
                    &mut reference_scratch,
                    &mut reference_work,
                    &mut reference_budget,
                )
            }
        };
        let mut text = String::new();
        loop {
            match normalized.poll(Tick(1)).unwrap() {
                Status::Scalar(value) => text.push(value),
                Status::Yield => {}
                Status::Complete => break,
            }
        }
        assert_eq!(expected, (text, normalized.is_encoding_problem(), Ok(())));
        assert_eq!(work.remaining(), reference_work.remaining());
        assert_eq!(
            budget.source_bytes_remaining(),
            reference_budget.source_bytes_remaining()
        );
        assert_eq!(budget.steps_remaining(), reference_budget.steps_remaining());
    }
}
#[test]
fn invalid_extents_and_live_deadlines_keep_typed_failures() {
    let mut work = work();
    let mut budget = HeaderBudget::new();
    let mut scratch = Scratch::new();
    for extent in [Extent { start: 3, end: 2 }, Extent { start: 0, end: 4 }] {
        assert!(matches!(
            Cursor::new(
                b"abc",
                extent,
                Kind::Phrase,
                &mut work,
                &mut budget,
                &mut scratch
            ),
            Err(Error::InvalidState)
        ));
    }
    assert_eq!(work.remaining(), self::work().remaining());
    for (kind, input) in [
        (Kind::Phrase, b"name".as_slice()),
        (Kind::Comment, b"(name)".as_slice()),
    ] {
        for stage in 0..3 {
            let mut work = self::work();
            let mut budget = HeaderBudget::new();
            let mut cursor = Cursor::new(
                input,
                extent(input),
                kind,
                &mut work,
                &mut budget,
                &mut scratch,
            )
            .unwrap();
            if stage >= 1 {
                let mut reached = false;
                for _ in 0..1000 {
                    cursor.poll(Tick(1)).unwrap();
                    if matches!(cursor.owner, Owner::Normalize(_)) {
                        reached = true;
                        break;
                    }
                }
                assert!(reached);
            }
            if stage == 2 {
                assert_eq!(drain(&mut cursor).2, Ok(()));
            }
            let before = remaining(&cursor);
            cursor.check_deadline(Tick(1)).unwrap();
            assert_eq!(remaining(&cursor), before);
            let error = if stage == 0 {
                match kind {
                    Kind::Phrase => Error::Phrase(header_phrase::Error::Work(Stop::Deadline)),
                    Kind::Comment => Error::Comment(header_comment::Error::Work(Stop::Deadline)),
                }
            } else {
                Error::Normalize(nfc::Error::Work(Stop::Deadline))
            };
            assert_eq!(cursor.check_deadline(Tick(100)), Err(error));
            assert_eq!(cursor.poll(Tick(1)), Err(error));
        }
    }
    for kind in [Kind::Phrase, Kind::Comment] {
        for (io_bytes, records, stop) in [(0, 1000, Stop::IoBytes), (1000, 0, Stop::Records)] {
            let mut work = Meter::new(
                Deadline::after(Tick(0), 100).unwrap(),
                Charge {
                    io_bytes,
                    records,
                    ..Charge::default()
                },
            );
            let mut budget = HeaderBudget::new();
            let source = if kind == Kind::Phrase {
                b"name".as_slice()
            } else {
                b"(name)"
            };
            let mut cursor = Cursor::new(
                source,
                extent(source),
                kind,
                &mut work,
                &mut budget,
                &mut scratch,
            )
            .unwrap();
            let expected = match kind {
                Kind::Phrase => Error::Phrase(header_phrase::Error::Work(stop)),
                Kind::Comment => Error::Comment(header_comment::Error::Work(stop)),
            };
            assert_eq!(drain(&mut cursor).2, Err(expected));
            assert_eq!(cursor.poll(Tick(1)), Err(expected));
            assert_eq!(work.stopped(), Some(stop));
            budget
                .charge_local(&mut self::work(), Tick(1), 0, 0, &mut 0)
                .unwrap();
        }
    }
}

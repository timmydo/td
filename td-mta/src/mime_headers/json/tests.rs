#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;
use crate::{admission::work::Stop, ports::Deadline};
const SOURCE: &[u8] = b"x-A: one\r\nX-a: two\r\nFold: e\xcc\x81\r\n\t=?utf-8?Q?a?=\r\n\r\nbody";
const EXPECTED: &str = "[{\"name\":\"x-A\",\"value\":\" one\"},{\"name\":\"X-a\",\"value\":\" two\"},{\"name\":\"Fold\",\"value\":\" e\u{301}\\r\\n\\t=?utf-8?Q?a?=\"}]";
fn meter() -> Meter {
    Meter::new(
        Deadline::after(Tick(0), 100).unwrap(),
        Charge {
            io_bytes: 1_000_000,
            records: 1_000_000,
            output_bytes: 1_000_000,
            ..Charge::default()
        },
    )
}
fn input(source: &[u8], source_end: SourceEnd) -> Input<'_> {
    Input {
        source,
        base: 0,
        source_end,
        header_limit: 1_000_000,
    }
}
fn drain(cursor: &mut Cursor<'_, '_>, width: usize) -> Result<Vec<u8>, Error> {
    let mut bytes = Vec::new();
    for _ in 0..20000 {
        let mut output = [0xa5; 65];
        let p = cursor.poll(Tick(1), &mut output[..width])?;
        assert!(p.written <= width.min(64));
        assert!(output[p.written..].iter().all(|b| *b == 0xa5));
        bytes.extend_from_slice(&output[..p.written]);
        if p.status == Status::Complete {
            return Ok(bytes);
        }
        assert!(cursor.value().is_none());
    }
    panic!("header array stalled")
}
#[test]
fn ordered_case_preserving_raw_objects_have_an_independent_literal_oracle() {
    for width in [1, 2, 6, 16, 64, 65] {
        for end in [SourceEnd::Eof, SourceEnd::Prefix] {
            let mut work = meter();
            let mut budget = HeaderBudget::new();
            let before = work.remaining();
            let mut cursor =
                Cursor::new(input(SOURCE, end), &mut work, &mut budget, Tick(1)).unwrap();
            assert_eq!(drain(&mut cursor, width).unwrap(), EXPECTED.as_bytes());
            let completion = cursor.finish(Tick(1)).unwrap();
            assert!(!completion.is_encoding_problem);
            assert_eq!(completion.end.body_start, (SOURCE.len() - 4) as u64);
            assert_eq!(completion.end.header_bytes, (SOURCE.len() - 6) as u64);
            assert_eq!(
                before.output_bytes - completion.work.remaining().output_bytes,
                EXPECTED.len() as u64
            );
            assert_eq!(completion.work.remaining().unlinks, before.unlinks);
        }
    }
}
#[test]
fn empty_eof_separator_lf_and_tolerant_body_boundary_are_explicit() {
    for (source, end, expected, body) in [
        (b"".as_slice(), SourceEnd::Eof, "[]", 0),
        (b"\r\nbody", SourceEnd::Prefix, "[]", 2),
        (
            b"A:b\n\nbody",
            SourceEnd::Prefix,
            "[{\"name\":\"A\",\"value\":\"b\"}]",
            5,
        ),
        (b"not a header\r\nbody", SourceEnd::Prefix, "[]", 0),
        (
            b"A:",
            SourceEnd::Eof,
            "[{\"name\":\"A\",\"value\":\"\"}]",
            2,
        ),
    ] {
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut cursor = Cursor::new(input(source, end), &mut work, &mut budget, Tick(1)).unwrap();
        assert_eq!(drain(&mut cursor, 1).unwrap(), expected.as_bytes());
        assert_eq!(cursor.finish(Tick(1)).unwrap().end.body_start, body);
    }
}
#[test]
fn raw_utf8_replacement_nul_removal_and_json_escaping_are_preserved() {
    let source = b"Odd: a\0\xe1\x80z\"\\\x01\r\n\r\n";
    let expected = "[{\"name\":\"Odd\",\"value\":\" a\u{fffd}z\\\"\\\\\\u0001\"}]";
    let mut work = meter();
    let mut budget = HeaderBudget::new();
    let mut cursor = Cursor::new(
        input(source, SourceEnd::Eof),
        &mut work,
        &mut budget,
        Tick(1),
    )
    .unwrap();
    assert_eq!(drain(&mut cursor, 1).unwrap(), expected.as_bytes());
    assert!(cursor.finish(Tick(1)).unwrap().is_encoding_problem);
}
#[test]
fn every_incomplete_resident_prefix_refuses_and_hides_provisional_results() {
    let source = b"A:a\r\nB:b\r\n\r\n";
    for cut in 0..source.len() {
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut cursor = Cursor::new(
            input(&source[..cut], SourceEnd::Prefix),
            &mut work,
            &mut budget,
            Tick(1),
        )
        .unwrap();
        assert_eq!(drain(&mut cursor, 6), Err(Error::Truncated));
        assert!(cursor.value().is_none());
        assert_eq!(cursor.poll(Tick(1), &mut []), Err(Error::Truncated));
        assert_eq!(cursor.finish(Tick(1)).err(), Some(Error::Truncated));
    }
}
#[test]
fn fixed_extents_and_header_limits_refuse_without_growing_storage() {
    for base in [0, u64::MAX - SOURCE.len() as u64] {
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut entity = input(SOURCE, SourceEnd::Eof);
        entity.base = base;
        let mut cursor = Cursor::new(entity, &mut work, &mut budget, Tick(1)).unwrap();
        assert_eq!(drain(&mut cursor, 64).unwrap(), EXPECTED.as_bytes());
        assert_eq!(
            cursor.finish(Tick(1)).unwrap().end.body_start,
            base + SOURCE.len() as u64 - 4
        );
    }
    let mut work = meter();
    let mut budget = HeaderBudget::new();
    let mut entity = input(SOURCE, SourceEnd::Eof);
    entity.base = u64::MAX;
    assert_eq!(
        Cursor::new(entity, &mut work, &mut budget, Tick(1)).err(),
        Some(Error::Headers(super::super::Error::Offset))
    );
    for limit in [0, 1, (SOURCE.len() - 7) as u64] {
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut entity = input(SOURCE, SourceEnd::Eof);
        entity.header_limit = limit;
        let mut cursor = Cursor::new(entity, &mut work, &mut budget, Tick(1)).unwrap();
        assert_eq!(
            drain(&mut cursor, 64),
            Err(Error::Headers(super::super::Error::HeaderLimit))
        );
        assert!(cursor.value().is_none());
    }
}
#[test]
fn zero_capacity_is_unpaid_and_output_refusals_precede_copy() {
    let mut work = meter();
    let mut budget = HeaderBudget::new();
    let before = (
        work.remaining(),
        budget.source_bytes_remaining(),
        budget.steps_remaining(),
    );
    let mut cursor = Cursor::new(
        input(SOURCE, SourceEnd::Eof),
        &mut work,
        &mut budget,
        Tick(1),
    )
    .unwrap();
    for _ in 0..3 {
        assert_eq!(
            cursor.poll(Tick(1), &mut []),
            Ok(Progress {
                written: 0,
                status: Status::NeedOutput
            })
        );
    }
    let Owner::Budgets(work, budget) = &cursor.owner else {
        panic!("empty output changed owner");
    };
    assert_eq!(
        (
            work.remaining(),
            budget.source_bytes_remaining(),
            budget.steps_remaining()
        ),
        before
    );
    assert_eq!(drain(&mut cursor, 64).unwrap(), EXPECTED.as_bytes());
    for capacity in [
        0,
        1,
        6,
        10,
        11,
        18,
        24,
        25,
        (EXPECTED.len() - 1) as u64,
        EXPECTED.len() as u64,
    ] {
        let work = meter();
        let mut work = Meter::new(
            work.deadline(),
            Charge {
                output_bytes: capacity,
                ..work.remaining()
            },
        );
        let mut budget = HeaderBudget::new();
        let mut cursor = Cursor::new(
            input(SOURCE, SourceEnd::Eof),
            &mut work,
            &mut budget,
            Tick(1),
        )
        .unwrap();
        if capacity == EXPECTED.len() as u64 {
            assert_eq!(drain(&mut cursor, 1).unwrap(), EXPECTED.as_bytes());
            assert_eq!(
                cursor
                    .finish(Tick(1))
                    .unwrap()
                    .work
                    .remaining()
                    .output_bytes,
                0
            );
        } else {
            let error = drain(&mut cursor, 1).unwrap_err();
            let expected = if [10, 11, 24, 25].contains(&capacity) {
                Error::Json(json_string::Error::Raw(header_raw::Error::Work(
                    Stop::OutputBytes,
                )))
            } else {
                Error::Admission(nfc::Error::Work(Stop::OutputBytes))
            };
            assert_eq!(error, expected);
            assert!(cursor.value().is_none());
            let error = cursor.failure.unwrap();
            assert_eq!(cursor.poll(Tick(1), &mut [0; 1]), Err(error));
            assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
        }
    }
    for (capacity, raw) in [
        (0, false),
        (6, false),
        (10, true),
        (11, true),
        (18, false),
        (24, true),
        (25, true),
    ] {
        let mut work = Meter::new(
            meter().deadline(),
            Charge {
                output_bytes: capacity,
                ..meter().remaining()
            },
        );
        let mut budget = HeaderBudget::new();
        let mut cursor = Cursor::new(
            input(SOURCE, SourceEnd::Eof),
            &mut work,
            &mut budget,
            Tick(1),
        )
        .unwrap();
        let expected = if raw {
            Error::Json(json_string::Error::Raw(header_raw::Error::Work(
                Stop::OutputBytes,
            )))
        } else {
            Error::Admission(nfc::Error::Work(Stop::OutputBytes))
        };
        let mut refused = false;
        for _ in 0..20000 {
            let mut output = [0xa5; 64];
            match cursor.poll(Tick(1), &mut output) {
                Ok(progress) => assert_ne!(progress.status, Status::Complete),
                Err(error) => {
                    assert_eq!(error, expected);
                    assert_eq!(output, [0xa5; 64]);
                    refused = true;
                    break;
                }
            }
        }
        assert!(refused);
        assert!(cursor.value().is_none());
    }
    let mut work = Meter::new(
        meter().deadline(),
        Charge {
            output_bytes: 0,
            ..meter().remaining()
        },
    );
    let mut budget = HeaderBudget::new();
    let mut cursor = Cursor::new(
        input(SOURCE, SourceEnd::Eof),
        &mut work,
        &mut budget,
        Tick(1),
    )
    .unwrap();
    let mut output = [0xa5; 64];
    assert_eq!(
        cursor.poll(Tick(1), &mut output),
        Err(Error::Admission(nfc::Error::Work(Stop::OutputBytes)))
    );
    assert_eq!(output, [0xa5; 64]);
}
fn deadline(error: Error) -> bool {
    matches!(
        error,
        Error::Admission(nfc::Error::Work(Stop::Deadline))
            | Error::Raw(header_raw::Error::Work(Stop::Deadline))
    )
}
#[test]
fn every_unfinished_turn_and_explicit_completed_boundary_stays_fresh() {
    let mut work = meter();
    let mut budget = HeaderBudget::new();
    let mut cursor = Cursor::new(
        input(SOURCE, SourceEnd::Eof),
        &mut work,
        &mut budget,
        Tick(1),
    )
    .unwrap();
    let mut turns = 0;
    while cursor.poll(Tick(1), &mut [0; 64]).unwrap().status != Status::Complete {
        turns += 1;
    }
    for cut in 0..=turns {
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut cursor = Cursor::new(
            input(SOURCE, SourceEnd::Eof),
            &mut work,
            &mut budget,
            Tick(1),
        )
        .unwrap();
        for _ in 0..cut {
            assert_eq!(
                cursor.poll(Tick(1), &mut [0; 64]).unwrap().status,
                Status::Yield
            );
        }
        let expected = match &cursor.owner {
            Owner::Budgets(..) => Error::Admission(nfc::Error::Work(Stop::Deadline)),
            Owner::Raw(..) => Error::Raw(header_raw::Error::Work(Stop::Deadline)),
            Owner::Retired => panic!("unfinished healthy owner retired"),
        };
        let error = cursor.poll(Tick(100), &mut []).unwrap_err();
        assert_eq!(error, expected);
        assert!(cursor.value().is_none());
        assert_eq!(cursor.poll(Tick(1), &mut [0; 64]), Err(error));
        assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
    }
    for release in [false, true] {
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut cursor = Cursor::new(
            input(SOURCE, SourceEnd::Eof),
            &mut work,
            &mut budget,
            Tick(1),
        )
        .unwrap();
        drain(&mut cursor, 64).unwrap();
        let before = cursor.value();
        assert_eq!(
            cursor.poll(Tick(100), &mut []),
            Ok(Progress {
                written: 0,
                status: Status::Complete
            })
        );
        assert_eq!(cursor.value(), before);
        if release {
            assert_eq!(
                cursor.finish(Tick(100)).err(),
                Some(Error::Admission(nfc::Error::Work(Stop::Deadline)))
            );
        } else {
            let error = cursor.check_deadline(Tick(100)).unwrap_err();
            assert_eq!(error, Error::Admission(nfc::Error::Work(Stop::Deadline)));
            assert!(cursor.value().is_none());
            assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
        }
    }
    let mut work = meter();
    let mut budget = HeaderBudget::new();
    assert!(deadline(
        Cursor::new(
            input(SOURCE, SourceEnd::Eof),
            &mut work,
            &mut budget,
            Tick(100)
        )
        .err()
        .unwrap()
    ));
}
#[test]
fn original_budget_handoff_and_premature_finish_are_exact() {
    let mut work = meter();
    let mut budget = HeaderBudget::new();
    let work_ptr = &work as *const Meter;
    let budget_ptr = &budget as *const HeaderBudget;
    let mut cursor = Cursor::new(
        input(SOURCE, SourceEnd::Eof),
        &mut work,
        &mut budget,
        Tick(1),
    )
    .unwrap();
    assert!(std::mem::size_of_val(&cursor) + std::mem::size_of::<HeaderBudget>() <= 512);
    drain(&mut cursor, 64).unwrap();
    let result = cursor.finish(Tick(1)).unwrap();
    assert_eq!(result.work as *const Meter, work_ptr);
    assert_eq!(result.budget as *const HeaderBudget, budget_ptr);
    assert!(result.budget.steps_remaining() < HeaderBudget::new().steps_remaining());
    assert_eq!(
        Cursor::new(
            input(SOURCE, SourceEnd::Eof),
            result.work,
            result.budget,
            Tick(1)
        )
        .unwrap()
        .finish(Tick(1))
        .err(),
        Some(Error::InvalidState)
    );
}
#[test]
fn original_job_interpretation_and_scan_turn_limits_are_enforced() {
    // Seven scanner bytes plus one repeated lookahead; two Raw replay bytes.
    // Nine scanner, five literal, two begin, and ten Raw steps. Three owners
    // each prepay a record block: coordinator sixteen steps, each Raw five.
    let source = b"A:a\r\n\r\n";
    let mut work = meter();
    let mut budget = HeaderBudget::new();
    let before = (
        work.remaining(),
        budget.source_bytes_remaining(),
        budget.steps_remaining(),
    );
    let mut cursor = Cursor::new(
        input(source, SourceEnd::Eof),
        &mut work,
        &mut budget,
        Tick(1),
    )
    .unwrap();
    let expected = b"[{\"name\":\"A\",\"value\":\"a\"}]";
    assert_eq!(drain(&mut cursor, 64).unwrap(), expected);
    let completion = cursor.finish(Tick(1)).unwrap();
    assert_eq!(before.0.io_bytes - completion.work.remaining().io_bytes, 10);
    assert_eq!(before.0.records - completion.work.remaining().records, 3);
    assert_eq!(
        before.0.output_bytes - completion.work.remaining().output_bytes,
        expected.len() as u64
    );
    assert_eq!(before.0.unlinks, completion.work.remaining().unlinks);
    assert_eq!(before.1 - completion.budget.source_bytes_remaining(), 10);
    assert_eq!(before.2 - completion.budget.steps_remaining(), 26);
    // Enough aggregate bytes for the entire scanner source, but not its Raw replay.
    let mut setup = Meter::new(
        meter().deadline(),
        Charge {
            io_bytes: u64::MAX,
            records: u64::MAX,
            ..Charge::default()
        },
    );
    let mut budget = HeaderBudget::new();
    budget
        .charge(
            &mut setup,
            Tick(1),
            budget.source_bytes_remaining() - source.len() as u64,
            0,
            &mut 0,
        )
        .unwrap();
    let mut work = meter();
    let mut cursor = Cursor::new(
        input(source, SourceEnd::Eof),
        &mut work,
        &mut budget,
        Tick(1),
    )
    .unwrap();
    assert_eq!(
        drain(&mut cursor, 64),
        Err(Error::Json(json_string::Error::Raw(
            header_raw::Error::InterpretationLimit
        )))
    );
    assert!(cursor.value().is_none());
    for records in [false, true] {
        let template = meter();
        let capacity = if records {
            Charge {
                records: 0,
                ..template.remaining()
            }
        } else {
            Charge {
                io_bytes: 0,
                ..template.remaining()
            }
        };
        let mut work = Meter::new(template.deadline(), capacity);
        let mut budget = HeaderBudget::new();
        let mut cursor = Cursor::new(
            input(SOURCE, SourceEnd::Eof),
            &mut work,
            &mut budget,
            Tick(1),
        )
        .unwrap();
        let expected = if records {
            Error::Admission(nfc::Error::Work(Stop::Records))
        } else {
            Error::Headers(super::super::Error::Work(Stop::IoBytes))
        };
        assert_eq!(drain(&mut cursor, 64), Err(expected));
        assert!(cursor.value().is_none());
    }
    for steps in [false, true] {
        let mut setup = Meter::new(
            meter().deadline(),
            Charge {
                io_bytes: u64::MAX,
                records: u64::MAX,
                ..Charge::default()
            },
        );
        let mut budget = HeaderBudget::new();
        budget
            .charge(
                &mut setup,
                Tick(1),
                if steps {
                    0
                } else {
                    budget.source_bytes_remaining()
                },
                if steps { budget.steps_remaining() } else { 0 },
                &mut 0,
            )
            .unwrap();
        let mut work = meter();
        let mut cursor = Cursor::new(
            input(SOURCE, SourceEnd::Eof),
            &mut work,
            &mut budget,
            Tick(1),
        )
        .unwrap();
        let expected = if steps {
            Error::Admission(nfc::Error::InterpretationLimit)
        } else {
            Error::Headers(super::super::Error::InterpretationLimit)
        };
        assert_eq!(drain(&mut cursor, 64), Err(expected));
        assert!(cursor.value().is_none());
    }
    let mut source = vec![b'A'; 1024];
    source.extend_from_slice(b":b\r\n\r\n");
    let mut work = meter();
    let mut budget = HeaderBudget::new();
    let mut cursor = Cursor::new(
        input(&source, SourceEnd::Eof),
        &mut work,
        &mut budget,
        Tick(1),
    )
    .unwrap();
    cursor.poll(Tick(1), &mut [0; 64]).unwrap();
    for _ in 0..4 {
        let Owner::Budgets(work, budget) = &cursor.owner else {
            panic!("scan lost budgets");
        };
        let before = (work.remaining().io_bytes, budget.steps_remaining());
        assert_eq!(
            cursor.poll(Tick(1), &mut [0; 64]).unwrap().status,
            Status::Yield
        );
        let Owner::Budgets(work, budget) = &cursor.owner else {
            panic!("scan lost budgets");
        };
        assert!(before.0 - work.remaining().io_bytes <= 255);
        assert!(before.1 - budget.steps_remaining() <= 256);
    }
    let bytes = drain(&mut cursor, 64).unwrap();
    assert!(bytes.ends_with(b"\"value\":\"b\"}]"));
}
/// Source, budgets and output are cold; polling/consuming refusals are measured.
pub fn probe(mut snapshot: impl FnMut()) {
    for trial in 0..8 {
        let source: &[u8] = match trial {
            0 => b"",
            2 => b"Odd: a\0\xe1\x80z\r\n\r\n",
            _ => SOURCE,
        };
        let mut work = meter();
        if trial == 4 {
            work = Meter::new(
                work.deadline(),
                Charge {
                    output_bytes: 0,
                    ..work.remaining()
                },
            );
        }
        let mut budget = HeaderBudget::new();
        let mut entity = input(
            source,
            if trial == 3 {
                SourceEnd::Prefix
            } else {
                SourceEnd::Eof
            },
        );
        if trial == 5 {
            entity.header_limit = 0;
        }
        if trial == 3 {
            entity.source = b"A:a\r\nB:b";
        }
        let mut output = [0; 64];
        snapshot();
        let mut cursor = Cursor::new(entity, &mut work, &mut budget, Tick(1)).unwrap();
        let mut done = false;
        let mut refusal = None;
        for _ in 0..20000 {
            match cursor.poll(if trial == 6 { Tick(100) } else { Tick(1) }, &mut output) {
                Ok(p) if p.status == Status::Complete => {
                    done = true;
                    break;
                }
                Ok(_) => {}
                Err(error) => {
                    assert!((3..=6).contains(&trial));
                    assert!(cursor.value().is_none());
                    refusal = Some(error);
                    break;
                }
            }
        }
        if trial <= 2 || trial == 7 {
            assert!(done);
            if trial == 7 {
                assert!(deadline(cursor.finish(Tick(100)).err().unwrap()));
            } else {
                let completion = cursor.finish(Tick(1)).unwrap();
                assert_eq!(completion.is_encoding_problem, trial == 2);
            }
        } else {
            assert!(!done);
            let expected = match trial {
                3 => Error::Truncated,
                4 => Error::Admission(nfc::Error::Work(Stop::OutputBytes)),
                5 => Error::Headers(super::super::Error::HeaderLimit),
                6 => Error::Admission(nfc::Error::Work(Stop::Deadline)),
                _ => panic!("unexpected refusal trial"),
            };
            assert_eq!(refusal, Some(expected));
            assert_eq!(cursor.poll(Tick(1), &mut output), Err(expected));
            assert_eq!(cursor.finish(Tick(1)).err(), Some(expected));
        }
        snapshot();
    }
}

#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;
use crate::{
    admission::work::{Charge, Stop},
    ports::Deadline,
};
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
fn drain(cursor: &mut Cursor<'_, '_>, width: usize) -> (Vec<u8>, Result<usize, Error>, u64) {
    let mut bytes = Vec::new();
    let mut maximum_output = 0;
    for turn in 1..100_000 {
        let mut output = [0xa5; 8];
        let before = cursor.source.cursor.remaining().unwrap();
        let result = cursor.poll(Tick(1), &mut output[..width]);
        if let Some(after) = cursor.source.cursor.remaining() {
            assert!(before.0.io_bytes - after.0.io_bytes <= 225);
            assert!(before.0.records - after.0.records <= 29);
            assert!(before.2 - after.2 <= 452);
            assert_eq!(before.1 - after.1, before.0.io_bytes - after.0.io_bytes);
            let spent = before.0.output_bytes - after.0.output_bytes;
            assert!(spent <= 10);
            maximum_output = maximum_output.max(spent);
        }
        match result {
            Ok(progress) => {
                assert!(progress.written <= 6);
                assert!(output[progress.written..].iter().all(|byte| *byte == 0xa5));
                bytes.extend_from_slice(&output[..progress.written]);
                if progress.status == Status::Complete {
                    assert!(cursor.end().is_some());
                    let remaining = cursor.source.cursor.remaining();
                    assert_eq!(
                        cursor.poll(Tick(100), &mut []),
                        Ok(Progress {
                            written: 0,
                            status: Status::Complete
                        })
                    );
                    assert_eq!(cursor.source.cursor.remaining(), remaining);
                    return (bytes, Ok(turn), maximum_output);
                }
                assert_eq!(cursor.end(), None);
                let remaining = cursor.source.cursor.remaining();
                assert_eq!(
                    cursor.poll(Tick(1), &mut []),
                    Ok(Progress {
                        written: 0,
                        status: Status::NeedOutput
                    })
                );
                assert_eq!(cursor.source.cursor.remaining(), remaining);
            }
            Err(error) => {
                assert_eq!(output, [0xa5; 8]);
                assert_eq!(cursor.end(), None);
                assert_eq!(cursor.poll(Tick(100), &mut output), Err(error));
                assert_eq!(cursor.check_deadline(Tick(100)), Err(error));
                return (bytes, Err(error), maximum_output);
            }
        }
    }
    panic!("location JSON did not finish")
}
fn scalar_run(
    source: &[u8],
    work: &mut Meter,
    budget: &mut HeaderBudget,
) -> (String, Result<End, super::super::Error>) {
    let mut value = String::new();
    let mut cursor = super::super::Cursor::new(source, work, budget);
    for _ in 0..100_000 {
        match cursor.poll(Tick(1)) {
            Ok(super::super::Status::Yield) => {}
            Ok(super::super::Status::Scalar(ch)) => value.push(ch),
            Ok(super::super::Status::Complete) => {
                return (value, cursor.finish(Tick(1)).map(|(_, _, end)| end))
            }
            Err(error) => return (value, Err(error)),
        }
    }
    panic!("scalar location did not finish")
}
fn scalar_value(source: &[u8], work: &mut Meter, budget: &mut HeaderBudget) -> (String, End) {
    let (value, end) = scalar_run(source, work, budget);
    (value, end.unwrap())
}
#[test]
fn short_drains_match_shared_json_and_original_field_costs() {
    for (source, expected) in [
        (b"(x) ../A%2fb?x=Y#Z (tail)".as_slice(), "../A%2fb?x=Y#Z"),
        (
            b"(x) =?utf-8?Q?e=CC=81?=\r\n =?ascii?Q?/a=20b?= (tail)",
            "e\u{301}/a b",
        ),
        (b"=?utf-8?Q?=FF?= =?ascii?B?Zm9v?=", "�foo"),
        (b"=?ascii?Q?=00=22=5C=0A?=", "\"\\"),
        (
            b"=?ascii?Q?a?= =?unknown?Q?b?=",
            "=?ascii?Q?a?==?unknown?Q?b?=",
        ),
        (b"=?utf-8?Q?=F0=9F=90=88?=", "🐈"),
        (b"", ""),
    ] {
        let mut reference_work = meter();
        let mut reference_budget = HeaderBudget::new();
        let (value, end) = scalar_value(source, &mut reference_work, &mut reference_budget);
        assert_eq!(value, expected);
        let json = td_json::Json::Str(value).to_string();
        for width in 1..=8 {
            let mut work = meter();
            let mut budget = HeaderBudget::new();
            let identity = (std::ptr::from_ref(&work), std::ptr::from_ref(&budget));
            let mut cursor = Cursor::new(source, &mut work, &mut budget);
            assert_eq!(cursor.end(), None);
            assert_eq!(
                cursor.poll(Tick(1), &mut []),
                Ok(Progress {
                    written: 0,
                    status: Status::NeedOutput
                })
            );
            let (bytes, result, maximum_output) = drain(&mut cursor, width);
            if expected == "🐈" {
                assert_eq!(maximum_output, 8);
            }
            assert!(result.is_ok());
            assert_eq!(bytes, json.as_bytes());
            let (work, budget, got) = cursor.finish(Tick(1)).unwrap();
            assert_eq!(got, end);
            assert_eq!(
                (std::ptr::from_ref(&*work), std::ptr::from_ref(&*budget)),
                identity
            );
            assert_eq!(
                work.remaining().io_bytes,
                reference_work.remaining().io_bytes
            );
            assert_eq!(work.remaining().records, reference_work.remaining().records);
            assert_eq!(
                work.remaining().output_bytes + json.len() as u64,
                reference_work.remaining().output_bytes
            );
            assert_eq!(
                budget.source_bytes_remaining(),
                reference_budget.source_bytes_remaining()
            );
            assert_eq!(budget.steps_remaining(), reference_budget.steps_remaining());
            scalar_value(b"../next", work, budget);
        }
    }
}
fn grants(kind: usize, cap: u64) -> (Meter, HeaderBudget) {
    let mut work = meter();
    let mut budget = HeaderBudget::new();
    if kind < 2 {
        budget
            .charge(
                &mut meter(),
                Tick(1),
                if kind == 0 {
                    budget.source_bytes_remaining() - cap
                } else {
                    0
                },
                if kind == 1 {
                    budget.steps_remaining() - cap
                } else {
                    0
                },
                &mut 0,
            )
            .unwrap();
    } else {
        let mut charge = work.remaining();
        match kind {
            2 => charge.io_bytes = cap,
            3 => charge.records = cap,
            4 => charge.output_bytes = cap,
            _ => panic!("bad grant"),
        }
        work = Meter::new(Deadline::after(Tick(0), 100).unwrap(), charge);
    }
    (work, budget)
}
#[test]
fn every_original_cut_and_exact_grant_retires_or_completes_whole_json() {
    for source in [
        b"../a".as_slice(),
        b"=?ascii?Q?=00=22?=",
        b"=?unknown?Q?a?=",
    ] {
        let mut reference_work = meter();
        let mut reference_budget = HeaderBudget::new();
        let (value, expected) = scalar_value(source, &mut reference_work, &mut reference_budget);
        let wanted = td_json::Json::Str(value).to_string().into_bytes();
        let costs = [
            HeaderBudget::new().source_bytes_remaining()
                - reference_budget.source_bytes_remaining(),
            HeaderBudget::new().steps_remaining() - reference_budget.steps_remaining(),
            100_000_000 - reference_work.remaining().io_bytes,
            100_000_000 - reference_work.remaining().records,
            100_000_000 - reference_work.remaining().output_bytes + wanted.len() as u64,
        ];
        assert!(costs.iter().all(|cost| *cost > 0));
        for (kind, used) in costs.into_iter().enumerate() {
            for cap in 0..=used {
                let (mut work, mut budget) = grants(kind, cap);
                let mut cursor = Cursor::new(source, &mut work, &mut budget);
                let (bytes, result, _) = drain(&mut cursor, 1);
                assert_eq!(result.is_ok(), cap == used);
                assert!(wanted.starts_with(&bytes));
                if cap == used {
                    let (work, budget, end) = cursor.finish(Tick(1)).unwrap();
                    assert_eq!(end, expected);
                    assert_eq!(bytes, wanted);
                    let remaining = [
                        budget.source_bytes_remaining(),
                        budget.steps_remaining(),
                        work.remaining().io_bytes,
                        work.remaining().records,
                        work.remaining().output_bytes,
                    ];
                    assert_eq!(remaining[kind], 0);
                } else {
                    let error = result.unwrap_err();
                    if kind < 4 {
                        let (mut expected_work, mut expected_budget) = grants(kind, cap);
                        let (_, expected) =
                            scalar_run(source, &mut expected_work, &mut expected_budget);
                        assert_eq!(error, Error::Source(expected.unwrap_err()));
                    } else {
                        use super::super::Error as Field;
                        assert!(matches!(
                            error,
                            Error::Source(
                                Field::Selection(crate::mime_location_selection::Error::Work(
                                    Stop::OutputBytes
                                )) | Field::Words(crate::mime_location_word::Error::Work(
                                    Stop::OutputBytes
                                )) | Field::Literal(crate::mime_location_literal::Error::Work(
                                    Stop::OutputBytes
                                )) | Field::Admission(crate::nfc::Error::Work(Stop::OutputBytes))
                            )
                        ));
                    }
                    assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
                    match kind {
                        0 | 1 => assert_eq!(work.stopped(), None),
                        2 => assert_eq!(work.stopped(), Some(Stop::IoBytes)),
                        3 => assert_eq!(work.stopped(), Some(Stop::Records)),
                        4 => assert_eq!(work.stopped(), Some(Stop::OutputBytes)),
                        _ => {}
                    }
                }
            }
        }
    }
}
#[test]
fn every_prefix_deadline_and_premature_finish_including_escape_drains() {
    for source in [
        b"../a".as_slice(),
        b"=?ascii?Q?=00=22?=",
        b"=?unknown?Q?a?=",
    ] {
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut cursor = Cursor::new(source, &mut work, &mut budget);
        let (_, result, _) = drain(&mut cursor, 1);
        let turns = result.unwrap();
        for cut in 0..=turns {
            for trial in 0..4 {
                let mut work = meter();
                let mut budget = HeaderBudget::new();
                let mut cursor = Cursor::new(source, &mut work, &mut budget);
                for _ in 0..cut {
                    cursor.poll(Tick(1), &mut [0]).unwrap();
                }
                use super::super::{Error as FieldError, Owner};
                let deadline = Error::Source(match &cursor.source.cursor.owner {
                    Owner::Selection(_) => FieldError::Selection(
                        crate::mime_location_selection::Error::Work(Stop::Deadline),
                    ),
                    Owner::Words(_) => {
                        FieldError::Words(crate::mime_location_word::Error::Work(Stop::Deadline))
                    }
                    Owner::Literal(_) => FieldError::Literal(
                        crate::mime_location_literal::Error::Work(Stop::Deadline),
                    ),
                    Owner::Complete(..) => {
                        FieldError::Admission(crate::nfc::Error::Work(Stop::Deadline))
                    }
                    Owner::Retired => panic!("unexpected retirement"),
                });
                if trial == 0 {
                    if cut == turns {
                        assert_eq!(
                            cursor.poll(Tick(100), &mut []),
                            Ok(Progress {
                                written: 0,
                                status: Status::Complete
                            })
                        );
                    }
                    assert_eq!(cursor.check_deadline(Tick(100)), Err(deadline));
                    assert_eq!(cursor.end(), None);
                    assert_eq!(cursor.poll(Tick(1), &mut [0]), Err(deadline));
                    assert_eq!(cursor.finish(Tick(1)).err(), Some(deadline));
                } else if trial == 1 {
                    assert_eq!(cursor.finish(Tick(100)).err(), Some(deadline));
                } else if trial == 2 {
                    if cut == turns {
                        assert_eq!(
                            cursor.poll(Tick(100), &mut [0]),
                            Ok(Progress {
                                written: 0,
                                status: Status::Complete
                            })
                        );
                        assert_eq!(cursor.check_deadline(Tick(100)), Err(deadline));
                    } else {
                        assert_eq!(cursor.poll(Tick(100), &mut [0]), Err(deadline));
                    }
                    assert_eq!(cursor.end(), None);
                    assert_eq!(cursor.poll(Tick(1), &mut [0]), Err(deadline));
                    assert_eq!(cursor.finish(Tick(1)).err(), Some(deadline));
                } else if cut == turns {
                    assert!(cursor.finish(Tick(1)).is_ok());
                } else {
                    assert_eq!(cursor.finish(Tick(1)).err(), Some(Error::InvalidState));
                }
            }
        }
    }
}
#[test]
fn malformed_fields_and_json_charges_keep_exact_phase_context() {
    use super::super::Error as FieldError;
    for (source, expected) in [
        (
            b"(broken".as_slice(),
            FieldError::Selection(crate::mime_location_selection::Error::Malformed),
        ),
        (
            b"../a\r\nX",
            FieldError::Words(crate::mime_location_word::Error::MalformedFold),
        ),
        (
            b"/a%",
            FieldError::Literal(crate::mime_location_literal::Error::MalformedUri),
        ),
    ] {
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut cursor = Cursor::new(source, &mut work, &mut budget);
        let (bytes, result, _) = drain(&mut cursor, 1);
        assert_eq!(result, Err(Error::Source(expected)));
        assert_eq!(bytes, b"\"");
    }
    for (source, cap, expected) in [
        (
            b"../a".as_slice(),
            0,
            FieldError::Selection(crate::mime_location_selection::Error::Work(
                Stop::OutputBytes,
            )),
        ),
        (
            b"",
            1,
            FieldError::Admission(crate::nfc::Error::Work(Stop::OutputBytes)),
        ),
        (
            b"=?ascii?Q?=22?=",
            2,
            FieldError::Words(crate::mime_location_word::Error::Work(Stop::OutputBytes)),
        ),
        (
            b"../a",
            2,
            FieldError::Literal(crate::mime_location_literal::Error::Work(Stop::OutputBytes)),
        ),
    ] {
        let (mut work, mut budget) = grants(4, cap);
        let mut cursor = Cursor::new(source, &mut work, &mut budget);
        let (_, result, _) = drain(&mut cursor, 1);
        assert_eq!(result, Err(Error::Source(expected)));
    }
}

#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;
use crate::{
    admission::work::{Charge, Stop},
    header_select::SourceEnd,
    ports::Deadline,
};
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
fn drain(cursor: &mut Cursor<'_, '_, '_>) -> Result<(), Error> {
    for _ in 0..20000 {
        if cursor.poll(Tick(1))? == Status::Complete {
            return Ok(());
        }
        assert!(cursor.value().is_none());
    }
    panic!("retained header array stalled")
}
fn deadline(error: Error) -> bool {
    matches!(
        error,
        Error::Admission(crate::nfc::Error::Work(Stop::Deadline))
            | Error::Raw(crate::header_raw::Error::Work(Stop::Deadline))
    )
}
fn completed_costs(cursor: &Cursor<'_, '_, '_>) -> (Charge, u64, u64) {
    let super::super::Owner::Budgets(work, budget) = &cursor.framing.owner else {
        panic!("complete framing lost original budgets");
    };
    (
        work.remaining(),
        budget.source_bytes_remaining(),
        budget.steps_remaining(),
    )
}
#[test]
fn exact_and_spare_windows_keep_literal_raw_bytes_and_original_identity() {
    for spare in [0, 1, 64] {
        let mut output = vec![0xa5; EXPECTED.len() + spare];
        let identity = output.as_ptr();
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut cursor = Cursor::new(
            input(SOURCE, SourceEnd::Eof),
            &mut work,
            &mut budget,
            &mut output,
            Tick(1),
        )
        .unwrap();
        drain(&mut cursor).unwrap();
        let before = (
            completed_costs(&cursor),
            cursor.value().map(|v| {
                (
                    v.end,
                    v.is_encoding_problem,
                    v.fragment.as_ptr(),
                    v.fragment.len(),
                )
            }),
        );
        for now in [Tick(1), Tick(100)] {
            assert_eq!(cursor.poll(now), Ok(Status::Complete));
            assert_eq!(
                (
                    completed_costs(&cursor),
                    cursor.value().map(|v| (
                        v.end,
                        v.is_encoding_problem,
                        v.fragment.as_ptr(),
                        v.fragment.len()
                    ))
                ),
                before
            );
        }
        let value = cursor.value().unwrap();
        assert_eq!(value.fragment, EXPECTED.as_bytes());
        assert_eq!(value.fragment.as_ptr(), identity);
        assert!(!value.is_encoding_problem);
        assert_eq!(value.end.body_start, (SOURCE.len() - 4) as u64);
        let (retained, completion) = cursor.finish(Tick(1)).unwrap();
        assert_eq!(retained.fragment.as_ptr(), identity);
        assert_eq!(retained.fragment, EXPECTED.as_bytes());
        assert_eq!(retained.end, completion.end);
        assert_eq!(retained.is_encoding_problem, completion.is_encoding_problem);
        assert_eq!(&output[EXPECTED.len()..], vec![0xa5; spare]);
    }
}
#[test]
fn every_short_window_refuses_stickily_and_hides_whole_output() {
    for capacity in 0..EXPECTED.len() {
        let mut output = vec![0xa5; capacity];
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut cursor = Cursor::new(
            input(SOURCE, SourceEnd::Eof),
            &mut work,
            &mut budget,
            &mut output,
            Tick(1),
        )
        .unwrap();
        assert_eq!(drain(&mut cursor), Err(Error::ResponseCapacity));
        assert!(cursor.value().is_none());
        assert_eq!(cursor.poll(Tick(100)), Err(Error::ResponseCapacity));
        assert_eq!(cursor.check_deadline(Tick(1)), Err(Error::ResponseCapacity));
        assert_eq!(cursor.finish(Tick(1)).err(), Some(Error::ResponseCapacity));
    }
}
#[test]
fn empty_arrays_and_raw_replacement_keep_complete_passive_labels() {
    for (source, expected, encoding) in [
        (b"".as_slice(), "[]", false),
        (
            b"Odd: a\0\xe1\x80z\r\n\r\n",
            "[{\"name\":\"Odd\",\"value\":\" a\u{fffd}z\"}]",
            true,
        ),
    ] {
        let mut output = vec![0xa5; expected.len()];
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut cursor = Cursor::new(
            input(source, SourceEnd::Eof),
            &mut work,
            &mut budget,
            &mut output,
            Tick(1),
        )
        .unwrap();
        drain(&mut cursor).unwrap();
        assert_eq!(cursor.value().unwrap().is_encoding_problem, encoding);
        let (value, completion) = cursor.finish(Tick(1)).unwrap();
        assert_eq!(value.fragment, expected.as_bytes());
        assert_eq!(value.is_encoding_problem, encoding);
        assert_eq!(completion.is_encoding_problem, encoding);
    }
}
#[test]
fn original_prefix_and_wire_refusals_hide_retained_prefixes() {
    let prefix = b"[{\"name\":\"A\",\"value\":\"a\"}";
    let mut work = meter();
    let mut budget = HeaderBudget::new();
    let initial = (
        work.remaining(),
        budget.source_bytes_remaining(),
        budget.steps_remaining(),
    );
    for (spare, expected) in [(0, Error::ResponseCapacity), (1, Error::Truncated)] {
        let before = (
            work.remaining(),
            budget.source_bytes_remaining(),
            budget.steps_remaining(),
        );
        let mut output = vec![0xa5; prefix.len() + spare];
        let mut cursor = Cursor::new(
            input(b"A:a\r\nB:b", SourceEnd::Prefix),
            &mut work,
            &mut budget,
            &mut output,
            Tick(1),
        )
        .unwrap();
        assert_eq!(drain(&mut cursor), Err(expected));
        assert!(cursor.value().is_none());
        assert_eq!(cursor.finish(Tick(1)).err(), Some(expected));
        assert_eq!(&output[..prefix.len()], prefix);
        assert!(work.remaining().io_bytes < before.0.io_bytes);
        assert!(work.remaining().records < before.0.records);
        assert!(work.remaining().output_bytes < before.0.output_bytes);
        assert!(budget.source_bytes_remaining() < before.1);
        assert!(budget.steps_remaining() < before.2);
    }
    assert!(work.remaining().io_bytes < initial.0.io_bytes);
    for prefix in [false, true] {
        let mut output = [0xa5; 512];
        let mut work = meter();
        if !prefix {
            work = Meter::new(
                work.deadline(),
                Charge {
                    output_bytes: 1,
                    ..work.remaining()
                },
            );
        }
        let mut budget = HeaderBudget::new();
        let entity = if prefix {
            input(b"A:a\r\nB:b", SourceEnd::Prefix)
        } else {
            input(SOURCE, SourceEnd::Eof)
        };
        let mut cursor = Cursor::new(entity, &mut work, &mut budget, &mut output, Tick(1)).unwrap();
        let error = drain(&mut cursor).unwrap_err();
        let expected = if prefix {
            Error::Truncated
        } else {
            Error::Admission(crate::nfc::Error::Work(Stop::OutputBytes))
        };
        assert_eq!(error, expected);
        assert!(cursor.value().is_none());
        assert_eq!(cursor.poll(Tick(1)), Err(error));
        assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
    }
}
#[test]
fn constructor_unfinished_and_explicit_completed_boundaries_stay_fresh() {
    for capacity in 0..EXPECTED.len() {
        for release in [false, true] {
            let mut output = vec![0xa5; capacity];
            let mut work = meter();
            let mut budget = HeaderBudget::new();
            let mut cursor = Cursor::new(
                input(SOURCE, SourceEnd::Eof),
                &mut work,
                &mut budget,
                &mut output,
                Tick(1),
            )
            .unwrap();
            for _ in 0..20000 {
                if cursor.window.provisional().unwrap().len() == capacity {
                    break;
                }
                assert_eq!(cursor.poll(Tick(1)).unwrap(), Status::Yield);
            }
            assert_eq!(cursor.window.provisional().unwrap().len(), capacity);
            assert!(cursor.value().is_none());
            let expected = match &cursor.framing.owner {
                super::super::Owner::Budgets(..) => {
                    Error::Admission(crate::nfc::Error::Work(Stop::Deadline))
                }
                super::super::Owner::Raw(..) => {
                    Error::Raw(crate::header_raw::Error::Work(Stop::Deadline))
                }
                super::super::Owner::Retired => panic!("healthy full-window owner retired"),
            };
            if release {
                assert_eq!(cursor.finish(Tick(100)).err(), Some(expected));
            } else {
                assert_eq!(cursor.poll(Tick(100)), Err(expected));
                assert!(cursor.value().is_none());
                assert_eq!(cursor.finish(Tick(1)).err(), Some(expected));
            }
        }
    }
    let mut output = [0xa5; 512];
    let mut work = meter();
    let mut budget = HeaderBudget::new();
    let before = (
        work.remaining(),
        budget.source_bytes_remaining(),
        budget.steps_remaining(),
    );
    assert_eq!(
        Cursor::new(
            input(SOURCE, SourceEnd::Eof),
            &mut work,
            &mut budget,
            &mut output,
            Tick(100)
        )
        .err(),
        Some(Error::Admission(crate::nfc::Error::Work(Stop::Deadline)))
    );
    assert_eq!(
        (
            work.remaining(),
            budget.source_bytes_remaining(),
            budget.steps_remaining()
        ),
        before
    );
    assert_eq!(output, [0xa5; 512]);
    let mut work = meter();
    let mut budget = HeaderBudget::new();
    let mut cursor = Cursor::new(
        input(SOURCE, SourceEnd::Eof),
        &mut work,
        &mut budget,
        &mut output,
        Tick(1),
    )
    .unwrap();
    let mut turns = 0;
    while cursor.poll(Tick(1)).unwrap() != Status::Complete {
        turns += 1;
    }
    for cut in 0..=turns {
        let mut output = [0xa5; 512];
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut cursor = Cursor::new(
            input(SOURCE, SourceEnd::Eof),
            &mut work,
            &mut budget,
            &mut output,
            Tick(1),
        )
        .unwrap();
        for _ in 0..cut {
            assert_eq!(cursor.poll(Tick(1)).unwrap(), Status::Yield);
        }
        let expected = match &cursor.framing.owner {
            super::super::Owner::Budgets(..) => {
                Error::Admission(crate::nfc::Error::Work(Stop::Deadline))
            }
            super::super::Owner::Raw(..) => {
                Error::Raw(crate::header_raw::Error::Work(Stop::Deadline))
            }
            super::super::Owner::Retired => panic!("healthy unfinished owner retired"),
        };
        let error = cursor.poll(Tick(100)).unwrap_err();
        assert_eq!(error, expected);
        assert!(cursor.value().is_none());
        assert_eq!(cursor.poll(Tick(1)), Err(error));
        assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
    }
    for release in [false, true] {
        let mut output = [0xa5; 512];
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut cursor = Cursor::new(
            input(SOURCE, SourceEnd::Eof),
            &mut work,
            &mut budget,
            &mut output,
            Tick(1),
        )
        .unwrap();
        drain(&mut cursor).unwrap();
        let before = cursor.value().map(|v| {
            (
                v.end,
                v.is_encoding_problem,
                v.fragment.as_ptr(),
                v.fragment.len(),
            )
        });
        let costs = completed_costs(&cursor);
        assert_eq!(cursor.poll(Tick(100)), Ok(Status::Complete));
        assert_eq!(completed_costs(&cursor), costs);
        assert_eq!(
            cursor.value().map(|v| (
                v.end,
                v.is_encoding_problem,
                v.fragment.as_ptr(),
                v.fragment.len()
            )),
            before
        );
        if release {
            assert_eq!(
                cursor.finish(Tick(100)).err(),
                Some(Error::Admission(crate::nfc::Error::Work(Stop::Deadline)))
            );
        } else {
            let error = cursor.check_deadline(Tick(100)).unwrap_err();
            assert_eq!(
                error,
                Error::Admission(crate::nfc::Error::Work(Stop::Deadline))
            );
            assert!(cursor.value().is_none());
            assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
        }
    }
}
#[test]
fn consuming_handoff_keeps_original_budgets_and_rejects_unfinished_release() {
    let mut output = [0xa5; 512];
    let mut work = meter();
    let mut budget = HeaderBudget::new();
    let work_ptr = &work as *const Meter;
    let budget_ptr = &budget as *const HeaderBudget;
    let mut cursor = Cursor::new(
        input(SOURCE, SourceEnd::Eof),
        &mut work,
        &mut budget,
        &mut output,
        Tick(1),
    )
    .unwrap();
    assert!(std::mem::size_of_val(&cursor) + std::mem::size_of::<HeaderBudget>() <= 640);
    drain(&mut cursor).unwrap();
    let (_, completion) = cursor.finish(Tick(1)).unwrap();
    assert_eq!(completion.work as *const Meter, work_ptr);
    assert_eq!(completion.budget as *const HeaderBudget, budget_ptr);
    assert_eq!(
        Cursor::new(
            input(SOURCE, SourceEnd::Eof),
            completion.work,
            completion.budget,
            &mut [],
            Tick(1)
        )
        .unwrap()
        .finish(Tick(1))
        .err(),
        Some(Error::InvalidState)
    );
}
#[test]
fn retention_charges_exactly_the_original_stream_without_extra_work() {
    let mut streamed = Vec::new();
    let mut work = meter();
    let mut budget = HeaderBudget::new();
    let mut cursor = super::super::Cursor::new(
        input(SOURCE, SourceEnd::Eof),
        &mut work,
        &mut budget,
        Tick(1),
    )
    .unwrap();
    loop {
        let mut bytes = [0; 64];
        let p = cursor.poll(Tick(1), &mut bytes).unwrap();
        streamed.extend_from_slice(&bytes[..p.written]);
        if p.status == Status::Complete {
            break;
        }
    }
    let completion = cursor.finish(Tick(1)).unwrap();
    let costs = (
        completion.work.remaining(),
        completion.budget.source_bytes_remaining(),
        completion.budget.steps_remaining(),
    );
    let mut output = [0; 512];
    let mut work = meter();
    let mut budget = HeaderBudget::new();
    let mut cursor = Cursor::new(
        input(SOURCE, SourceEnd::Eof),
        &mut work,
        &mut budget,
        &mut output,
        Tick(1),
    )
    .unwrap();
    drain(&mut cursor).unwrap();
    let (value, completion) = cursor.finish(Tick(1)).unwrap();
    assert_eq!(value.fragment, streamed);
    assert_eq!(value.fragment, EXPECTED.as_bytes());
    assert_eq!(
        (
            completion.work.remaining(),
            completion.budget.source_bytes_remaining(),
            completion.budget.steps_remaining()
        ),
        costs
    );
}
/// Original source, budgets and fixed output are prepared cold.
pub fn probe(mut snapshot: impl FnMut()) {
    for trial in 0..8 {
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut output = [0xa5; 512];
        let entity = match trial {
            0 => input(b"", SourceEnd::Eof),
            2 => input(b"Odd: a\0\xe1\x80z\r\n\r\n", SourceEnd::Eof),
            3 => input(b"A:a\r\nB:b", SourceEnd::Prefix),
            _ => input(SOURCE, SourceEnd::Eof),
        };
        let window = if trial == 4 {
            &mut output[..1]
        } else if trial == 6 {
            &mut output[..0]
        } else {
            &mut output[..]
        };
        snapshot();
        let result = Cursor::new(
            entity,
            &mut work,
            &mut budget,
            window,
            if trial == 5 { Tick(100) } else { Tick(1) },
        );
        if trial == 5 {
            assert!(deadline(result.err().unwrap()));
        } else {
            let mut cursor = result.unwrap();
            let mut done = false;
            let mut refusal = None;
            for _ in 0..20000 {
                match cursor.poll(if trial == 6 { Tick(100) } else { Tick(1) }) {
                    Ok(Status::Complete) => {
                        done = true;
                        break;
                    }
                    Ok(Status::Yield) => {}
                    Ok(Status::NeedOutput) => panic!("retention needs output"),
                    Err(error) => {
                        refusal = Some(error);
                        break;
                    }
                }
            }
            if trial == 3 || trial == 4 || trial == 6 {
                assert!(!done);
                let expected = match trial {
                    3 => Error::Truncated,
                    4 => Error::ResponseCapacity,
                    6 => Error::Admission(crate::nfc::Error::Work(Stop::Deadline)),
                    _ => panic!("unexpected refusal"),
                };
                assert_eq!(refusal, Some(expected));
                assert!(cursor.value().is_none());
                assert_eq!(cursor.finish(Tick(1)).err(), Some(expected));
            } else {
                assert!(done);
                let costs = completed_costs(&cursor);
                assert_eq!(cursor.poll(Tick(100)), Ok(Status::Complete));
                assert_eq!(completed_costs(&cursor), costs);
                if trial == 7 {
                    assert!(deadline(cursor.finish(Tick(100)).err().unwrap()));
                } else {
                    let (value, completion) = cursor.finish(Tick(1)).unwrap();
                    assert_eq!(value.is_encoding_problem, trial == 2);
                    assert_eq!(completion.is_encoding_problem, trial == 2);
                    if trial == 1 {
                        assert_eq!(value.fragment, EXPECTED.as_bytes());
                    }
                    if trial == 0 {
                        assert_eq!(value.fragment, b"[]");
                    }
                }
            }
        }
        snapshot();
    }
}

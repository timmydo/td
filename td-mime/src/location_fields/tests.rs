#![allow(clippy::unwrap_used, clippy::panic)]
use super::*;
use crate::{
    time::Deadline,
    work::{Charge, Stop},
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
fn input(source: &[u8]) -> Input<'_> {
    Input {
        source,
        base: 100,
        header_limit: 1_000_000,
        source_end: SourceEnd::Eof,
    }
}
fn drain(cursor: &mut Cursor<'_, '_>) -> Result<usize, Error> {
    for turn in 1..100_000 {
        let before = cursor.remaining().unwrap();
        let result = cursor.poll(Tick(1));
        if let Some(after) = cursor.remaining() {
            assert!(before.0.io_bytes - after.0.io_bytes <= 256);
            assert!(before.0.records - after.0.records <= 29);
            assert!(before.0.output_bytes - after.0.output_bytes <= 4);
            assert!(before.2 - after.2 <= 452);
            assert_eq!(before.1 - after.1, before.0.io_bytes - after.0.io_bytes);
        }
        match result {
            Ok(Status::Yield) => assert_eq!(cursor.selection(), None),
            Ok(Status::Complete) => {
                assert!(cursor.selection().is_some());
                assert_eq!(cursor.poll(Tick(100)), Ok(Status::Complete));
                return Ok(turn);
            }
            Err(error) => {
                assert_eq!(cursor.selection(), None);
                assert_eq!(cursor.poll(Tick(100)), Err(error));
                assert_eq!(cursor.check_deadline(Tick(1)), Err(error));
                return Err(error);
            }
        }
    }
    panic!("location discovery did not finish")
}
fn value(source: &[u8], field: Field) -> &[u8] {
    td_header::resident::slice(source, 100, field.value_start..field.value_end).unwrap()
}
#[test]
fn first_valid_extent_empty_presence_and_source_bound_retention() {
    for (source, expected, encoded, repair) in [
        (b"Content-Location: a%\r\nContent-Location: (bad\r\ncOnTeNt-LoCaTiOn: ../A%2fb\r\nContent-Location: x%\r\n\r\nContent-Location: body".as_slice(), Some(b" ../A%2fb".as_slice()), false, false),
        (b"Content-Location: \r\nContent-Location: ../later\r\n\r\n", Some(b" ".as_slice()), false, false),
        (b"Content-Location: =?utf-8?Q?=FF?=\r\n\r\n", Some(b" =?utf-8?Q?=FF?=".as_slice()), true, true),
        (b"Content-Location: x%\r\nOther: hi\r\n\r\n", None, false, false),
        (b"Other: hi\r\n\r\n", None, false, false),
    ] {
        let mut work = meter(); let mut budget = HeaderBudget::new();
        let pointers = (std::ptr::from_ref(&work), std::ptr::from_ref(&budget));
        let mut cursor = Cursor::new(input(source), &mut work, &mut budget).unwrap();
        drain(&mut cursor).unwrap();
        let (work, budget, selected) = cursor.finish(Tick(1)).unwrap();
        assert_eq!((std::ptr::from_ref(&*work), std::ptr::from_ref(&*budget)), pointers);
        let selected_value = selected.content_location.map(|field| value(source, field));
        assert_eq!(selected_value, expected);
        assert_eq!(selected.location_end.is_some(), expected.is_some());
        if let Some(end) = selected.location_end {
            assert_eq!(end.encoded_words, encoded); assert_eq!(end.encoding_problem, repair);
        }
        let mut backing = [0; 128];
        let mut retained = location::retained::Cursor::new(selected_value, &mut backing, work, budget);
        for _ in 0..1000 {
            if retained.poll(Tick(1)).unwrap() == location::retained::Status::Complete { break; }
        }
        let (retained, work, budget) = retained.finish(Tick(1)).unwrap();
        assert_eq!(retained.value.is_some(), expected.is_some());
        assert_eq!(retained.end, selected.location_end);
        if expected == Some(b" ".as_slice()) { assert_eq!(retained.value, Some(b"\"\"".as_slice())); }
        assert_eq!((std::ptr::from_ref(&*work), std::ptr::from_ref(&*budget)), pointers);
    }
}
#[test]
fn malformed_discard_restores_only_original_live_owners() {
    for (source, expected) in [
        (
            b"(bad".as_slice(),
            location::Error::Selection(crate::location_selection::Error::Malformed),
        ),
        (
            b"a%".as_slice(),
            location::Error::Literal(crate::location_literal::Error::MalformedUri),
        ),
        (
            b"a\nX".as_slice(),
            location::Error::Words(crate::location_word::Error::MalformedFold),
        ),
    ] {
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let pointers = (std::ptr::from_ref(&work), std::ptr::from_ref(&budget));
        let mut field = location::Cursor::new(source, &mut work, &mut budget);
        let mut failed = false;
        for _ in 0..1000 {
            if let Err(error) = field.poll(Tick(1)) {
                assert_eq!(error, expected);
                failed = true;
                break;
            }
        }
        assert!(failed);
        assert_eq!(field.end(), None);
        let before = field.remaining().unwrap();
        let (work, budget) = field.discard_malformed(Tick(1)).unwrap();
        assert_eq!(
            (
                work.remaining(),
                budget.source_bytes_remaining(),
                budget.steps_remaining()
            ),
            before
        );
        assert_eq!(
            (std::ptr::from_ref(&*work), std::ptr::from_ref(&*budget)),
            pointers
        );
        assert!(work.remaining().io_bytes < 100_000_000);
        assert!(budget.source_bytes_remaining() < HeaderBudget::new().source_bytes_remaining());
        let mut next = location::Cursor::new(b"../next", work, budget);
        for _ in 0..1000 {
            if next.poll(Tick(1)).unwrap() == location::Status::Complete {
                break;
            }
        }
        next.finish(Tick(1)).unwrap();
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut field = location::Cursor::new(source, &mut work, &mut budget);
        for _ in 0..1000 {
            if field.poll(Tick(1)).is_err() {
                break;
            }
        }
        assert_eq!(
            field.discard_malformed(Tick(100)).err(),
            Some(location::Error::Admission(nfc::Error::Work(Stop::Deadline)))
        );
    }
    let mut work = Meter::new(Deadline::after(Tick(0), 100).unwrap(), Charge::default());
    let mut budget = HeaderBudget::new();
    let mut field = location::Cursor::new(b"a%", &mut work, &mut budget);
    let error = field.poll(Tick(1)).unwrap_err();
    assert_eq!(field.discard_malformed(Tick(1)).err(), Some(error));
    let mut work = meter();
    let mut budget = HeaderBudget::new();
    budget
        .charge_local(
            &mut meter(),
            Tick(1),
            budget.source_bytes_remaining(),
            budget.steps_remaining(),
            &mut 0,
        )
        .unwrap();
    let mut field = location::Cursor::new(b"a%", &mut work, &mut budget);
    let error = field.poll(Tick(1)).unwrap_err();
    assert_eq!(
        error,
        location::Error::Selection(crate::location_selection::Error::InterpretationLimit)
    );
    assert_eq!(field.discard_malformed(Tick(1)).err(), Some(error));
    let deep = "(".repeat(33);
    let mut work = meter();
    let mut budget = HeaderBudget::new();
    let mut field = location::Cursor::new(deep.as_bytes(), &mut work, &mut budget);
    let mut error = None;
    for _ in 0..1000 {
        if let Err(failure) = field.poll(Tick(1)) {
            error = Some(failure);
            break;
        }
    }
    assert_eq!(
        error,
        Some(location::Error::Selection(
            crate::location_selection::Error::NestingLimit
        ))
    );
    assert_eq!(field.discard_malformed(Tick(1)).err(), error);
}
#[test]
fn whole_header_errors_and_refusals_cannot_be_rescued_by_duplicates() {
    for source in [
        b"Content-Location: ../ok\r\n".as_slice(),
        b"Content-Location: ../ok\r\nOther: x\r\n",
    ] {
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut i = input(source);
        i.source_end = SourceEnd::Prefix;
        let mut cursor = Cursor::new(i, &mut work, &mut budget).unwrap();
        assert_eq!(drain(&mut cursor), Err(Error::Truncated));
    }
    let source = b"Content-Location: ../ok\r\nContent-Location: ../later\r\n\r\n";
    let mut work = meter();
    let mut budget = HeaderBudget::new();
    let mut i = input(source);
    i.base = u64::MAX;
    assert!(matches!(
        Cursor::new(i, &mut work, &mut budget),
        Err(Error::InvalidRange)
    ));
    for limit in 0..source.len() as u64 - 2 {
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut i = input(source);
        i.header_limit = limit;
        let mut cursor = Cursor::new(i, &mut work, &mut budget).unwrap();
        assert_eq!(
            drain(&mut cursor),
            Err(Error::Headers(headers::Error::HeaderLimit))
        );
    }
    let mut work = meter();
    let mut budget = HeaderBudget::new();
    let mut i = input(source);
    i.header_limit = source.len() as u64 - 2;
    let mut cursor = Cursor::new(i, &mut work, &mut budget).unwrap();
    drain(&mut cursor).unwrap();
    cursor.finish(Tick(1)).unwrap();
}
#[test]
fn every_progress_prefix_fresh_deadline_and_premature_finish() {
    let source = b"Content-Location: x%\r\nContent-Location: =?ascii?Q?a?=\r\nOther: x\r\n\r\nbody";
    let mut work = meter();
    let mut budget = HeaderBudget::new();
    let mut cursor = Cursor::new(input(source), &mut work, &mut budget).unwrap();
    let turns = drain(&mut cursor).unwrap();
    cursor.finish(Tick(1)).unwrap();
    for cut in 0..=turns {
        for trial in 0..4 {
            let mut work = meter();
            let mut budget = HeaderBudget::new();
            let mut cursor = Cursor::new(input(source), &mut work, &mut budget).unwrap();
            for _ in 0..cut {
                cursor.poll(Tick(1)).unwrap();
            }
            let deadline = match &cursor.owner {
                Owner::Budgets(..) => Error::Admission(nfc::Error::Work(Stop::Deadline)),
                Owner::Field(field) => Error::Location(field.deadline_error()),
                Owner::Retired => panic!("unexpected retirement"),
            };
            if trial == 0 {
                if cut == turns {
                    assert_eq!(cursor.poll(Tick(100)), Ok(Status::Complete));
                }
                assert_eq!(cursor.check_deadline(Tick(100)), Err(deadline));
                assert_eq!(cursor.selection(), None);
            } else if trial == 1 {
                assert_eq!(cursor.finish(Tick(100)).err(), Some(deadline));
            } else if trial == 2 {
                if cut == turns {
                    assert!(cursor.finish(Tick(1)).is_ok());
                } else {
                    assert_eq!(cursor.finish(Tick(1)).err(), Some(Error::InvalidState));
                }
            } else if cut == turns {
                assert_eq!(cursor.poll(Tick(100)), Ok(Status::Complete));
                assert_eq!(cursor.check_deadline(Tick(100)), Err(deadline));
                assert_eq!(cursor.selection(), None);
            } else {
                assert_eq!(cursor.poll(Tick(100)), Err(deadline));
                assert_eq!(cursor.selection(), None);
            }
        }
    }
}
fn reference(source: &[u8]) -> (Selection, Charge, u64, u64) {
    let mut work = meter();
    let mut budget = HeaderBudget::new();
    let mut scanner = Scanner::new(100, 1_000_000);
    let mut consumed = 0usize;
    let mut credit = 0;
    let mut selected = None;
    let mut location_end = None;
    let mut turns = 0;
    let end = loop {
        turns += 1;
        assert!(turns < 1000, "independent header scanner did not finish");
        let progress = scanner
            .poll_with_work(
                source.get(consumed..).unwrap(),
                true,
                Tick(1),
                &mut Aggregate::new(&mut work, &mut budget, &mut credit),
            )
            .unwrap();
        consumed += progress.consumed;
        match progress.status {
            headers::Status::Field(field) => {
                let name =
                    td_header::resident::slice(source, 100, field.name_start..field.name_end)
                        .unwrap();
                let compared = b"Content-Location".len() as u64;
                let same_length = name.len() as u64 == compared;
                header_work::Work::charge(
                    &mut Aggregate::new(&mut work, &mut budget, &mut credit),
                    Tick(1),
                    header_work::Charge {
                        visits: if same_length { compared * 2 } else { 0 },
                        steps: if same_length { compared } else { 1 },
                        records: 1,
                    },
                )
                .unwrap();
                if same_length
                    && name.eq_ignore_ascii_case(b"Content-Location")
                    && selected.is_none()
                {
                    let mut field_cursor =
                        location::Cursor::new(value(source, field), &mut work, &mut budget);
                    let mut complete = false;
                    for _ in 0..1000 {
                        match field_cursor.poll(Tick(1)) {
                            Ok(location::Status::Complete) => {
                                complete = true;
                                break;
                            }
                            Ok(location::Status::Yield | location::Status::Scalar(_)) => {}
                            Err(error) => {
                                assert!(matches!(
                                    error,
                                    location::Error::Selection(
                                        crate::location_selection::Error::Malformed
                                    ) | location::Error::Words(
                                        crate::location_word::Error::MalformedFold
                                    ) | location::Error::Literal(
                                        crate::location_literal::Error::MalformedUri
                                    )
                                ));
                                break;
                            }
                        }
                    }
                    if complete {
                        location_end = Some(field_cursor.finish(Tick(1)).unwrap().2);
                        selected = Some(field);
                    } else {
                        let before = field_cursor.remaining().unwrap();
                        let (original_work, original_budget) =
                            field_cursor.discard_malformed(Tick(1)).unwrap();
                        assert_eq!(
                            (
                                original_work.remaining(),
                                original_budget.source_bytes_remaining(),
                                original_budget.steps_remaining()
                            ),
                            before
                        );
                    }
                }
            }
            headers::Status::Complete(end) => break end,
            headers::Status::Yield => {}
            headers::Status::NeedInput => panic!("complete reference needs input"),
        }
    };
    header_work::Work::charge(
        &mut Aggregate::new(&mut work, &mut budget, &mut credit),
        Tick(1),
        header_work::Charge {
            steps: 1,
            records: 1,
            ..header_work::Charge::default()
        },
    )
    .unwrap();
    (
        Selection {
            content_location: selected,
            location_end,
            end,
        },
        work.remaining(),
        budget.source_bytes_remaining(),
        budget.steps_remaining(),
    )
}
fn grants(kind: usize, cap: u64) -> (Meter, HeaderBudget) {
    let mut work = meter();
    let mut budget = HeaderBudget::new();
    if kind < 2 {
        budget
            .charge_local(
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
fn independent_scanner_and_public_field_costs_pin_every_original_cut() {
    for source in [
        b"Content-Location: x%\r\nContent-Location: (bad\r\nContent-Location: ../ok\r\n\r\n"
            .as_slice(),
        b"Other: x\r\nContent-Location: ../ok\r\nContent-Location: x%\r\n\r\nbody".as_slice(),
        b"Content-Location: =?ascii?Q?a?= =?ascii?Q?b?=\r\nUnknown: x\r\n\r\n",
    ] {
        let (expected, remaining, bytes, steps) = reference(source);
        let costs = [
            HeaderBudget::new().source_bytes_remaining() - bytes,
            HeaderBudget::new().steps_remaining() - steps,
            100_000_000 - remaining.io_bytes,
            100_000_000 - remaining.records,
            100_000_000 - remaining.output_bytes,
        ];
        for (kind, used) in costs.into_iter().enumerate() {
            assert!(used > 0, "resource {kind} has no cuts");
            for cap in 0..=used {
                let (mut work, mut budget) = grants(kind, cap);
                let mut cursor = Cursor::new(input(source), &mut work, &mut budget).unwrap();
                let result = drain(&mut cursor);
                assert_eq!(result.is_ok(), cap == used);
                if cap < used {
                    let error = result.unwrap_err();
                    assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
                } else {
                    let (work, budget, selected) = cursor.finish(Tick(1)).unwrap();
                    assert_eq!(selected, expected);
                    let actual = [
                        budget.source_bytes_remaining(),
                        budget.steps_remaining(),
                        work.remaining().io_bytes,
                        work.remaining().records,
                        work.remaining().output_bytes,
                    ];
                    assert_eq!(actual.get(kind), Some(&0));
                }
            }
        }
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut cursor = Cursor::new(input(source), &mut work, &mut budget).unwrap();
        drain(&mut cursor).unwrap();
        let (work, budget, selected) = cursor.finish(Tick(1)).unwrap();
        assert_eq!(selected, expected);
        assert_eq!(work.remaining(), remaining);
        assert_eq!(budget.source_bytes_remaining(), bytes);
        assert_eq!(budget.steps_remaining(), steps);
    }
}

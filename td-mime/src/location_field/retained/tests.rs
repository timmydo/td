#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
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
fn drain(cursor: &mut Cursor<'_, '_>) -> Result<usize, Error> {
    assert!(cursor.view().is_none());
    for turn in 1..100_000 {
        match cursor.poll(Tick(1)) {
            Ok(Status::Yield) => {
                assert!(cursor.view().is_none());
                assert!(!cursor.is_complete());
            }
            Ok(Status::Complete) => {
                assert!(cursor.view().is_some());
                assert!(cursor.is_complete());
                assert_eq!(cursor.poll(Tick(100)), Ok(Status::Complete));
                return Ok(turn);
            }
            Err(error) => {
                assert!(cursor.view().is_none());
                assert!(!cursor.is_complete());
                assert_eq!(cursor.poll(Tick(100)), Err(error));
                assert_eq!(cursor.check_deadline(Tick(100)), Err(error));
                return Err(error);
            }
        }
    }
    panic!("retained location did not finish")
}
fn standalone(
    source: Option<&[u8]>,
    work: &mut Meter,
    budget: &mut HeaderBudget,
) -> (Option<Vec<u8>>, Option<End>) {
    let Some(source) = source else {
        return (None, None);
    };
    let mut cursor = json::Cursor::new(source, work, budget);
    let mut value = Vec::new();
    for _ in 0..100_000 {
        let mut output = [0; 6];
        let progress = cursor.poll(Tick(1), &mut output).unwrap();
        value.extend_from_slice(&output[..progress.written]);
        if progress.status == json::Status::Complete {
            return (Some(value), Some(cursor.finish(Tick(1)).unwrap().2));
        }
    }
    panic!("standalone JSON did not finish")
}
#[test]
fn exact_retention_matches_public_json_costs_presence_and_original_reuse() {
    for source in [
        None,
        Some(b"".as_slice()),
        Some(b"(x) ../A%2fb (tail)"),
        Some(b"=?utf-8?Q?e=CC=81?= =?ascii?Q?=22=5C?="),
        Some(b"=?utf-8?Q?=FF?="),
        Some(b"=?utf-8?B?////////?="),
        Some(b"=?windows-1252?Q?=80=81?="),
        Some(b"=?ascii?Q?=22=5C=22=5C=22=5C?="),
        Some(b"=?unknown?Q?a?="),
    ] {
        let mut reference_work = meter();
        let mut reference_budget = HeaderBudget::new();
        let (expected, end) = standalone(source, &mut reference_work, &mut reference_budget);
        if source == Some(b"=?utf-8?B?////////?=".as_slice()) {
            assert_eq!(expected.as_deref(), Some("\"������\"".as_bytes()));
            assert!(end.unwrap().encoding_problem);
        }
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let pointers = (std::ptr::from_ref(&work), std::ptr::from_ref(&budget));
        let mut backing = vec![0xa5; expected.as_ref().map_or(0, Vec::len)];
        let backing_pointer = backing.as_ptr();
        let mut cursor = Cursor::new(source, &mut backing, &mut work, &mut budget);
        assert!(drain(&mut cursor).is_ok());
        let view = cursor.view().unwrap();
        assert_eq!(view.value, expected.as_deref());
        assert_eq!(view.end, end);
        let (retained, work, budget) = cursor.finish(Tick(1)).unwrap();
        assert_eq!(retained.value, expected.as_deref());
        assert_eq!(retained.end, end);
        if let Some(value) = retained.value {
            assert_eq!(value.as_ptr(), backing_pointer);
        }
        assert_eq!(
            (std::ptr::from_ref(&*work), std::ptr::from_ref(&*budget)),
            pointers
        );
        assert_eq!(work.remaining(), reference_work.remaining());
        assert_eq!(
            budget.source_bytes_remaining(),
            reference_budget.source_bytes_remaining()
        );
        assert_eq!(budget.steps_remaining(), reference_budget.steps_remaining());
        let mut next_backing = [0; 32];
        let mut next = Cursor::new(Some(b"../next"), &mut next_backing, work, budget);
        drain(&mut next).unwrap();
        let (_, work, budget) = next.finish(Tick(1)).unwrap();
        assert_eq!(
            (std::ptr::from_ref(&*work), std::ptr::from_ref(&*budget)),
            pointers
        );
        let bound = capacity_bound(source.map_or(0, <[u8]>::len)).unwrap();
        assert!(expected.as_ref().is_none_or(|value| value.len() <= bound));
    }
    assert_eq!(capacity_bound(usize::MAX), None);
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
        };
        work = Meter::new(Deadline::after(Tick(0), 100).unwrap(), charge);
    }
    (work, budget)
}
#[test]
fn every_capacity_and_original_resource_cut_preserves_whole_visibility() {
    for source in [
        b"../a".as_slice(),
        b"=?ascii?Q?=22=5C?=",
        b"=?unknown?Q?a?=",
    ] {
        let mut reference_work = meter();
        let mut reference_budget = HeaderBudget::new();
        let (expected, _) = standalone(Some(source), &mut reference_work, &mut reference_budget);
        let expected = expected.unwrap();
        for size in 0..=expected.len() {
            let mut work = meter();
            let mut budget = HeaderBudget::new();
            let mut backing = vec![0; size];
            let mut cursor = Cursor::new(Some(source), &mut backing, &mut work, &mut budget);
            let result = drain(&mut cursor);
            assert_eq!(result.is_ok(), size == expected.len());
            if size < expected.len() {
                assert_eq!(result, Err(Error::OutputCapacity));
                assert_eq!(cursor.finish(Tick(1)).err(), Some(Error::OutputCapacity));
            } else {
                assert_eq!(
                    cursor.finish(Tick(1)).unwrap().0.value,
                    Some(expected.as_slice())
                );
            }
        }
        let costs = [
            HeaderBudget::new().source_bytes_remaining()
                - reference_budget.source_bytes_remaining(),
            HeaderBudget::new().steps_remaining() - reference_budget.steps_remaining(),
            100_000_000 - reference_work.remaining().io_bytes,
            100_000_000 - reference_work.remaining().records,
            100_000_000 - reference_work.remaining().output_bytes,
        ];
        for (kind, used) in costs.into_iter().enumerate() {
            assert!(used > 0, "resource {kind} has no exercised cuts");
            for cap in 0..=used {
                let (mut work, mut budget) = grants(kind, cap);
                let mut backing = vec![0; expected.len()];
                let mut cursor = Cursor::new(Some(source), &mut backing, &mut work, &mut budget);
                let result = drain(&mut cursor);
                assert_eq!(result.is_ok(), cap == used);
                if cap < used {
                    let error = result.unwrap_err();
                    assert!(matches!(error, Error::Projection(_)));
                    assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
                } else {
                    let (retained, work, budget) = cursor.finish(Tick(1)).unwrap();
                    assert_eq!(retained.value, Some(expected.as_slice()));
                    let remaining = [
                        budget.source_bytes_remaining(),
                        budget.steps_remaining(),
                        work.remaining().io_bytes,
                        work.remaining().records,
                        work.remaining().output_bytes,
                    ];
                    assert_eq!(remaining[kind], 0);
                }
            }
        }
    }
}
fn assert_deadline(error: Error) {
    use super::super::Error as Field;
    match error {
        Error::Admission(nfc::Error::Work(Stop::Deadline)) => {}
        Error::Projection(json::Error::Source(
            Field::Selection(crate::location_selection::Error::Work(Stop::Deadline))
            | Field::Words(crate::location_word::Error::Work(Stop::Deadline))
            | Field::Literal(crate::location_literal::Error::Work(Stop::Deadline))
            | Field::Admission(nfc::Error::Work(Stop::Deadline)),
        )) => {}
        other => panic!("wrong fresh refusal: {other:?}"),
    }
}
#[test]
fn every_prefix_fresh_admission_and_premature_finish_retire_metadata() {
    for source in [
        None,
        Some(b"../a".as_slice()),
        Some(b"=?ascii?Q?=22=5C?="),
        Some(b"=?unknown?Q?a?="),
    ] {
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut backing = [0; 128];
        let mut cursor = Cursor::new(source, &mut backing, &mut work, &mut budget);
        let turns = drain(&mut cursor).unwrap();
        for cut in 0..=turns {
            for trial in 0..3 {
                let mut work = meter();
                let mut budget = HeaderBudget::new();
                let mut backing = [0; 128];
                let mut cursor = Cursor::new(source, &mut backing, &mut work, &mut budget);
                for _ in 0..cut {
                    cursor.poll(Tick(1)).unwrap();
                }
                if trial == 0 {
                    if cut == turns {
                        assert_eq!(cursor.poll(Tick(100)), Ok(Status::Complete));
                    }
                    let error = cursor.check_deadline(Tick(100)).unwrap_err();
                    assert_deadline(error);
                    assert!(cursor.view().is_none());
                    assert!(matches!(cursor.owner, Owner::Retired));
                    assert_eq!(cursor.poll(Tick(1)), Err(error));
                    assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
                } else if trial == 1 {
                    assert_deadline(cursor.finish(Tick(100)).err().unwrap());
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
fn malformed_selected_fields_and_relocated_progress_never_expose_partial_values() {
    for source in [b"(broken".as_slice(), b"../a%", b"../a\r\nX"] {
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut backing = [0; 64];
        let mut cursor = Cursor::new(Some(source), &mut backing, &mut work, &mut budget);
        let error = drain(&mut cursor).unwrap_err();
        assert!(matches!(error, Error::Projection(_)));
        assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
    }
    let source = b"=?utf-8?Q?e=CC=81?= =?ascii?Q?=22=5C?= =?utf-8?Q?=FF?=";
    let mut expected_work = meter();
    let mut expected_budget = HeaderBudget::new();
    let (expected, end) = standalone(Some(source), &mut expected_work, &mut expected_budget);
    let mut work = meter();
    let mut budget = HeaderBudget::new();
    let mut backing = [0; 128];
    let mut left = Some(Cursor::new(
        Some(source),
        &mut backing,
        &mut work,
        &mut budget,
    ));
    let mut right = None;
    let mut complete = false;
    for turn in 0..100_000 {
        let mut cursor = if turn % 2 == 0 {
            left.take().unwrap()
        } else {
            right.take().unwrap()
        };
        let status = cursor.poll(Tick(1)).unwrap();
        if status == Status::Complete {
            let (retained, work, budget) = cursor.finish(Tick(1)).unwrap();
            assert_eq!(retained.value, expected.as_deref());
            assert_eq!(retained.end, end);
            assert_eq!(work.remaining(), expected_work.remaining());
            assert_eq!(
                budget.source_bytes_remaining(),
                expected_budget.source_bytes_remaining()
            );
            assert_eq!(budget.steps_remaining(), expected_budget.steps_remaining());
            complete = true;
            break;
        }
        assert!(cursor.view().is_none());
        if turn % 2 == 0 {
            right = Some(cursor)
        } else {
            left = Some(cursor)
        };
    }
    assert!(complete);
}

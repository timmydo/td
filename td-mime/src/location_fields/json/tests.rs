#![allow(clippy::unwrap_used, clippy::panic)]
use super::*;
use crate::{header_select::SourceEnd, time::Deadline, work::Charge};
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
        match cursor.poll(Tick(1)) {
            Ok(Status::Yield) => assert!(cursor.view().is_none()),
            Ok(Status::Complete) => {
                assert!(cursor.view().is_some());
                return Ok(turn);
            }
            Err(error) => {
                assert!(cursor.view().is_none());
                assert_eq!(cursor.poll(Tick(1)), Err(error));
                assert_eq!(cursor.check_deadline(Tick(1)), Err(error));
                return Err(error);
            }
        }
    }
    panic!("source-bound location did not finish")
}
fn reference<'w>(
    source: &[u8],
    backing: &'w mut [u8],
    work: &'w mut Meter,
    budget: &'w mut HeaderBudget,
) -> (Retained<'w>, &'w mut Meter, &'w mut HeaderBudget, usize) {
    let mut turns = 0;
    let mut select = Selector::new(input(source), work, budget).unwrap();
    for _ in 0..100_000 {
        turns += 1;
        if select.poll(Tick(1)).unwrap() == super::super::Status::Complete {
            break;
        }
    }
    let (work, budget, selection) = select.finish(Tick(1)).unwrap();
    let selected = selection.content_location.map(|field| {
        td_header::resident::slice(source, 100, field.value_start..field.value_end).unwrap()
    });
    let mut projection = retained::Cursor::new(selected, backing, work, budget);
    for _ in 0..100_000 {
        turns += 1;
        if projection.poll(Tick(1)).unwrap() == retained::Status::Complete {
            break;
        }
    }
    let (projected, work, budget) = projection.finish(Tick(1)).unwrap();
    assert_eq!(projected.end, selection.location_end);
    (
        Retained {
            selection,
            value: projected.value,
        },
        work,
        budget,
        turns,
    )
}
#[test]
fn exact_original_source_matches_public_composition_and_reuses_owners() {
    let cases: [(&[u8], Option<&[u8]>); 4] = [
        (
            concat!(
                "Content-Location: a%\r\nContent-Location: ../A%2fb\r\n",
                "Content-Location: ../other\r\n\r\nContent-Location: body"
            )
            .as_bytes(),
            Some(b"\"../A%2fb\"".as_slice()),
        ),
        (
            b"Content-Location: \r\nContent-Location: ../later\r\n\r\n",
            Some(b"\"\"".as_slice()),
        ),
        (
            b"Content-Location: =?utf-8?Q?=FF?=\r\n\r\n",
            Some("\"�\"".as_bytes()),
        ),
        (b"Content-Location: a%\r\nOther: x\r\n\r\n", None),
    ];
    for (source, wanted) in cases {
        let mut reference_work = meter();
        let mut reference_budget = HeaderBudget::new();
        let mut reference_backing = [0; 256];
        let (expected, rw, rb, expected_turns) = reference(
            source,
            &mut reference_backing,
            &mut reference_work,
            &mut reference_budget,
        );
        assert_eq!(expected.value, wanted);
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut backing = [0; 256];
        let original = (std::ptr::from_ref(&work), std::ptr::from_ref(&budget));
        let mut cursor = Cursor::new(input(source), &mut backing, &mut work, &mut budget).unwrap();
        assert_eq!(drain(&mut cursor).unwrap(), expected_turns);
        let (actual, work, budget) = cursor.finish(Tick(1)).unwrap();
        assert_eq!(actual.value, expected.value);
        assert_eq!(actual.selection, expected.selection);
        assert_eq!(
            (std::ptr::from_ref(&*work), std::ptr::from_ref(&*budget)),
            original
        );
        assert_eq!(work.remaining(), rw.remaining());
        assert_eq!(budget.source_bytes_remaining(), rb.source_bytes_remaining());
        assert_eq!(budget.steps_remaining(), rb.steps_remaining());
        let mut next = crate::language::Cursor::new(b"fr", work, budget);
        for _ in 0..1000 {
            if next.poll(Tick(1)).unwrap() == crate::language::Status::Complete {
                break;
            }
        }
        next.finish(Tick(1)).unwrap();
    }
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
        let mut grant = work.remaining();
        match kind {
            2 => grant.io_bytes = cap,
            3 => grant.records = cap,
            4 => grant.output_bytes = cap,
            _ => panic!("bad grant"),
        };
        work = Meter::new(Deadline::after(Tick(0), 100).unwrap(), grant);
    }
    (work, budget)
}
#[test]
fn every_original_cut_and_backing_capacity_hides_partial_selection() {
    let source =
        b"Content-Location: a%\r\nContent-Location: =?utf-8?Q?=C3=A9?=\r\nOther: x\r\n\r\n";
    let mut work = meter();
    let mut budget = HeaderBudget::new();
    let mut backing = [0; 256];
    let (expected, work, budget, _) = reference(source, &mut backing, &mut work, &mut budget);
    let costs = [
        HeaderBudget::new().source_bytes_remaining() - budget.source_bytes_remaining(),
        HeaderBudget::new().steps_remaining() - budget.steps_remaining(),
        100_000_000 - work.remaining().io_bytes,
        100_000_000 - work.remaining().records,
        100_000_000 - work.remaining().output_bytes,
    ];
    for (kind, used) in costs.into_iter().enumerate() {
        assert!(used > 0);
        for cap in 0..=used {
            let (mut work, mut budget) = grants(kind, cap);
            let mut backing = [0; 256];
            let mut cursor =
                Cursor::new(input(source), &mut backing, &mut work, &mut budget).unwrap();
            let result = drain(&mut cursor);
            assert_eq!(result.is_ok(), cap == used);
            if let Err(error) = result {
                assert!(resource_refusal(error, kind), "resource {kind}: {error:?}");
                assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
            } else {
                let (actual, _, _) = cursor.finish(Tick(1)).unwrap();
                assert_eq!(actual.value, expected.value);
                assert_eq!(actual.selection, expected.selection);
            }
        }
    }
    let required = expected.value.unwrap().len();
    for capacity in 0..=required {
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut backing = [0; 256];
        let mut cursor = Cursor::new(
            input(source),
            backing.get_mut(..capacity).unwrap(),
            &mut work,
            &mut budget,
        )
        .unwrap();
        let result = drain(&mut cursor);
        if capacity < required {
            assert_eq!(
                result,
                Err(Error::Retention(retained::Error::OutputCapacity))
            );
            assert_eq!(
                cursor.finish(Tick(1)).err(),
                Some(Error::Retention(retained::Error::OutputCapacity))
            );
        } else {
            let (actual, _, _) = cursor.finish(Tick(1)).unwrap();
            assert_eq!(actual.value, expected.value);
        }
    }
}
fn resource_refusal(error: Error, kind: usize) -> bool {
    use crate::work::Stop;
    use crate::{
        location_field as f, location_literal as l, location_selection as s, location_word as w,
    };
    fn stop(stop: Stop, kind: usize) -> bool {
        matches!(
            (kind, stop),
            (2, Stop::IoBytes) | (3, Stop::Records) | (4, Stop::OutputBytes)
        )
    }
    fn admission(error: nfc::Error, kind: usize) -> bool {
        match error {
            nfc::Error::InterpretationLimit => kind < 2,
            nfc::Error::Work(error) => stop(error, kind),
            _ => false,
        }
    }
    fn field(error: f::Error, kind: usize) -> bool {
        match error {
            f::Error::Selection(s::Error::InterpretationLimit)
            | f::Error::Words(w::Error::InterpretationLimit)
            | f::Error::Literal(l::Error::InterpretationLimit) => kind < 2,
            f::Error::Selection(s::Error::Work(error))
            | f::Error::Words(w::Error::Work(error))
            | f::Error::Literal(l::Error::Work(error)) => stop(error, kind),
            f::Error::Admission(error) => admission(error, kind),
            _ => false,
        }
    }
    match error {
        Error::Selection(super::super::Error::Headers(
            crate::headers::Error::InterpretationLimit,
        )) => kind < 2,
        Error::Selection(super::super::Error::Headers(crate::headers::Error::Work(error))) => {
            stop(error, kind)
        }
        Error::Selection(super::super::Error::Location(error)) => field(error, kind),
        Error::Selection(super::super::Error::Admission(error))
        | Error::Retention(retained::Error::Admission(error))
        | Error::Admission(error) => admission(error, kind),
        Error::Retention(retained::Error::Projection(td_json::string::Error::Source(error))) => {
            field(error, kind)
        }
        _ => false,
    }
}
#[test]
fn every_turn_respects_the_larger_original_child_ceiling() {
    let source = b"Content-Location: a%\r\nContent-Location: =?utf-8?Q?=F0=9F=90=88?=\r\n\r\n";
    let mut work = meter();
    let mut budget = HeaderBudget::new();
    let mut backing = [0; 128];
    let mut cursor = Cursor::new(input(source), &mut backing, &mut work, &mut budget).unwrap();
    let turns = drain(&mut cursor).unwrap();
    cursor.finish(Tick(1)).unwrap();
    let mut before = (
        meter().remaining(),
        HeaderBudget::new().source_bytes_remaining(),
        HeaderBudget::new().steps_remaining(),
    );
    for cut in 1..=turns {
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut backing = [0; 128];
        let mut cursor = Cursor::new(input(source), &mut backing, &mut work, &mut budget).unwrap();
        for _ in 0..cut {
            cursor.poll(Tick(1)).unwrap();
        }
        assert_eq!(cursor.finish(Tick(1)).is_ok(), cut == turns);
        let after = (
            work.remaining(),
            budget.source_bytes_remaining(),
            budget.steps_remaining(),
        );
        assert!(before.0.io_bytes - after.0.io_bytes <= 256);
        assert!(before.0.records - after.0.records <= 29);
        assert!(before.0.output_bytes - after.0.output_bytes <= 10);
        assert!(before.2 - after.2 <= 452);
        assert_eq!(before.0.io_bytes - after.0.io_bytes, before.1 - after.1);
        before = after;
    }
}
fn deadline(error: Error) -> bool {
    use crate::work::Stop;
    use crate::{
        location_field as f, location_literal as l, location_selection as s, location_word as w,
    };
    fn field(error: f::Error) -> bool {
        matches!(
            error,
            f::Error::Admission(nfc::Error::Work(Stop::Deadline))
                | f::Error::Selection(s::Error::Work(Stop::Deadline))
                | f::Error::Words(w::Error::Work(Stop::Deadline))
                | f::Error::Literal(l::Error::Work(Stop::Deadline))
        )
    }
    match error {
        Error::Admission(nfc::Error::Work(Stop::Deadline))
        | Error::Selection(super::super::Error::Admission(nfc::Error::Work(Stop::Deadline)))
        | Error::Retention(retained::Error::Admission(nfc::Error::Work(Stop::Deadline))) => true,
        Error::Selection(super::super::Error::Location(error)) => field(error),
        Error::Retention(retained::Error::Projection(td_json::string::Error::Source(error))) => {
            field(error)
        }
        _ => false,
    }
}
#[test]
fn every_prefix_deadline_precedes_premature_finish_and_retires_whole_view() {
    let source = b"Content-Location: a%\r\nContent-Location: ../x\r\n\r\n";
    let mut work = meter();
    let mut budget = HeaderBudget::new();
    let mut backing = [0; 128];
    let mut cursor = Cursor::new(input(source), &mut backing, &mut work, &mut budget).unwrap();
    let turns = drain(&mut cursor).unwrap();
    cursor.finish(Tick(1)).unwrap();
    for cut in 0..=turns {
        for trial in 0..3 {
            let mut work = meter();
            let mut budget = HeaderBudget::new();
            let mut backing = [0; 128];
            let mut cursor =
                Cursor::new(input(source), &mut backing, &mut work, &mut budget).unwrap();
            for _ in 0..cut {
                cursor.poll(Tick(1)).unwrap();
            }
            if trial == 0 {
                if cut == turns {
                    assert_eq!(cursor.poll(Tick(100)), Ok(Status::Complete));
                }
                let error = cursor.check_deadline(Tick(100)).unwrap_err();
                assert!(deadline(error));
                assert!(cursor.view().is_none());
                assert_eq!(cursor.poll(Tick(1)), Err(error));
                assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
            } else if trial == 1 {
                assert!(deadline(cursor.finish(Tick(100)).err().unwrap()));
            } else if cut == turns {
                cursor.finish(Tick(1)).unwrap();
            } else {
                assert_eq!(cursor.finish(Tick(1)).err(), Some(Error::InvalidState));
            }
        }
    }
}
#[test]
fn incomplete_headers_and_late_scanner_refusals_never_reach_retention() {
    let source = b"Content-Location: ../ok\r\nOther: x\r\n";
    let mut work = meter();
    let mut budget = HeaderBudget::new();
    let mut backing = [0xA5; 128];
    let mut i = input(source);
    i.source_end = SourceEnd::Prefix;
    {
        let mut cursor = Cursor::new(i, &mut backing, &mut work, &mut budget).unwrap();
        assert_eq!(
            drain(&mut cursor),
            Err(Error::Selection(super::super::Error::Truncated))
        );
    }
    assert_eq!(backing, [0xA5; 128]);
    let source = b"Content-Location: ../ok\r\nOther: x\r\n\r\n";
    let mut i = input(source);
    i.header_limit = 25;
    {
        let mut cursor = Cursor::new(i, &mut backing, &mut work, &mut budget).unwrap();
        assert_eq!(
            drain(&mut cursor),
            Err(Error::Selection(super::super::Error::Headers(
                crate::headers::Error::HeaderLimit
            )))
        );
    }
    assert_eq!(backing, [0xA5; 128]);
}

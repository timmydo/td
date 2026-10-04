#![allow(clippy::unwrap_used, clippy::panic)]
use super::*;
use crate::{admission::work::Charge, ports::Deadline};
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
fn limited(bytes: u64, steps: u64) -> HeaderBudget {
    let mut budget = HeaderBudget::new();
    budget
        .charge(
            &mut meter(),
            Tick(1),
            budget.source_bytes_remaining() - bytes,
            budget.steps_remaining() - steps,
            &mut 0,
        )
        .unwrap();
    budget
}
fn drain(cursor: &mut Cursor<'_, '_>) -> (Vec<(u8, usize)>, Result<usize, Error>) {
    let mut events = Vec::new();
    for turn in 1..100_000 {
        match cursor.poll(Tick(1)) {
            Ok(Status::Yield) => assert!(!cursor.is_complete()),
            Ok(Status::Octet { byte, position }) => {
                assert_eq!(cursor.source.get(position), Some(&byte));
                assert!(!cursor.is_complete());
                events.push((byte, position));
            }
            Ok(Status::Complete) => {
                assert!(cursor.is_complete());
                assert_eq!(cursor.poll(Tick(100)), Ok(Status::Complete));
                return (events, Ok(turn));
            }
            Err(error) => {
                assert!(!cursor.is_complete());
                assert_eq!(cursor.poll(Tick(1)), Err(error));
                assert_eq!(cursor.check_deadline(Tick(1)), Err(error));
                return (events, Err(error));
            }
        }
    }
    panic!("literal field cursor did not finish")
}
#[test]
fn exact_field_offsets_costs_and_original_owner_handoff() {
    let source = b"(lead) a\r\n b (tail)";
    let mut work = meter();
    let mut budget = HeaderBudget::new();
    let work_ptr = &work as *const Meter;
    let budget_ptr = &budget as *const HeaderBudget;
    let mut cursor = Cursor::new(source, &mut work, &mut budget);
    let (events, turns) = drain(&mut cursor);
    assert_eq!(events, vec![(b'a', 7), (b'b', 11)]);
    assert_eq!(turns, Ok(26));
    let (work, budget, range) = cursor.finish(Tick(1)).unwrap();
    assert_eq!(range, Spelling { start: 7, end: 12 });
    assert!(std::ptr::eq(work, work_ptr));
    assert!(std::ptr::eq(budget, budget_ptr));
    assert_eq!(100_000_000 - work.remaining().io_bytes, 35);
    assert_eq!(16_000_000 - budget.steps_remaining(), 76);
    assert_eq!(100_000_000 - work.remaining().records, 6);
    assert_eq!(100_000_000 - work.remaining().output_bytes, 2);
    assert_eq!(16 * 1024 * 1024 - budget.source_bytes_remaining(), 35);
}
#[test]
fn literal_empty_parenthesis_and_word_marker_spellings() {
    for (source, wanted) in [
        (b"".as_slice(), b"".as_slice()),
        (b" (only)\t", b""),
        (b"(lead) ../a(b) (tail)", b"../a(b)"),
        (b"(lead) a (b) c (tail)", b"a(b)c"),
        (b"a (bad (tail)", b"a(bad(tail)"),
        (b"(lead) //host/a%2Fb?X#Y (tail)", b"//host/a%2Fb?X#Y"),
        (b"(lead) =?ascii?Q?a_b?= (tail)", b"=?ascii?Q?a_b?="),
        (b"(lead) http://[::1]/x (tail)", b"http://[::1]/x"),
    ] {
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut cursor = Cursor::new(source, &mut work, &mut budget);
        let (events, result) = drain(&mut cursor);
        assert!(result.is_ok());
        assert_eq!(
            events.iter().map(|(byte, _)| *byte).collect::<Vec<_>>(),
            wanted
        );
        let (_, _, range) = cursor.finish(Tick(1)).unwrap();
        let expected = source
            .iter()
            .enumerate()
            .filter_map(|(position, &byte)| {
                (position >= range.start
                    && position < range.end
                    && !matches!(byte, b' ' | b'\t' | b'\r' | b'\n'))
                .then_some((byte, position))
            })
            .collect::<Vec<_>>();
        assert_eq!(events, expected);
    }
}
#[test]
fn whole_field_failures_emit_no_literal_prefix() {
    let deep = format!("{}x{}a", "(".repeat(33), ")".repeat(33));
    for (source, error) in [
        (b"(bad".as_slice(), Error::MalformedCfws),
        (deep.as_bytes(), Error::NestingLimit),
        (b"(x) a% (tail)", Error::MalformedUri),
        (b"(x) http://[:::]/ (tail)", Error::MalformedUri),
        (b"(x) a\rX (tail)", Error::MalformedFold),
        (b"(x) \xc3\xa9 (tail)", Error::MalformedUri),
    ] {
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut cursor = Cursor::new(source, &mut work, &mut budget);
        assert_eq!(drain(&mut cursor), (Vec::new(), Err(error)));
        assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
        assert_eq!(work.remaining().output_bytes, 100_000_000);
    }
}
#[test]
fn every_allowance_cut_and_exact_combined_grants() {
    let source = b"(lead) a\r\n b (tail)";
    let expected = vec![(b'a', 7), (b'b', 11)];
    let mut interpretation_late = false;
    for (bytes, steps) in (0..35)
        .map(|cut| (cut, 76))
        .chain((0..76).map(|cut| (35, cut)))
    {
        let mut work = meter();
        let mut budget = limited(bytes, steps);
        let mut cursor = Cursor::new(source, &mut work, &mut budget);
        let (events, result) = drain(&mut cursor);
        assert!(expected.starts_with(&events));
        interpretation_late |= !events.is_empty();
        assert_eq!(result, Err(Error::InterpretationLimit));
        assert_eq!(
            cursor.finish(Tick(1)).err(),
            Some(Error::InterpretationLimit)
        );
        assert_eq!(work.stopped(), None);
        assert_eq!(
            Cursor::new(b"a", &mut work, &mut budget).poll(Tick(1)),
            Err(Error::InterpretationLimit)
        );
    }
    assert!(interpretation_late);
    let mut job_late = false;
    for (bytes, records, output, stop) in (0..35)
        .map(|cut| (cut, 6, 2, Stop::IoBytes))
        .chain((0..6).map(|cut| (35, cut, 2, Stop::Records)))
        .chain((0..2).map(|cut| (35, 6, cut, Stop::OutputBytes)))
    {
        let mut work = Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: bytes,
                records,
                output_bytes: output,
                ..Charge::default()
            },
        );
        let mut budget = HeaderBudget::new();
        let mut cursor = Cursor::new(source, &mut work, &mut budget);
        let (events, result) = drain(&mut cursor);
        assert!(expected.starts_with(&events));
        job_late |= !events.is_empty();
        assert_eq!(result, Err(Error::Work(stop)));
        assert_eq!(cursor.finish(Tick(1)).err(), Some(Error::Work(stop)));
        assert_eq!(work.stopped(), Some(stop));
        budget.charge(&mut meter(), Tick(1), 0, 0, &mut 0).unwrap();
    }
    assert!(job_late);
    let mut work = Meter::new(
        Deadline::after(Tick(0), 100).unwrap(),
        Charge {
            io_bytes: 35,
            records: 6,
            output_bytes: 2,
            ..Charge::default()
        },
    );
    let mut budget = limited(35, 76);
    let mut cursor = Cursor::new(source, &mut work, &mut budget);
    assert_eq!(drain(&mut cursor), (expected, Ok(26)));
    cursor.finish(Tick(1)).unwrap();
    assert_eq!(work.remaining().io_bytes, 0);
    assert_eq!(work.remaining().records, 0);
    assert_eq!(work.remaining().output_bytes, 0);
    assert_eq!(budget.source_bytes_remaining(), 0);
    assert_eq!(budget.steps_remaining(), 0);
}
#[test]
fn every_live_deadline_and_fresh_final_retirement() {
    let source = b"(lead) a\r\n b (tail)";
    let turns = drain(&mut Cursor::new(
        source,
        &mut meter(),
        &mut HeaderBudget::new(),
    ))
    .1
    .unwrap();
    for cut in 0..turns {
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut cursor = Cursor::new(source, &mut work, &mut budget);
        for _ in 0..cut {
            cursor.poll(Tick(1)).unwrap();
        }
        assert_eq!(cursor.poll(Tick(100)), Err(Error::Work(Stop::Deadline)));
        assert_eq!(cursor.poll(Tick(1)), Err(Error::Work(Stop::Deadline)));
        assert!(!cursor.is_complete());
        assert_eq!(
            cursor.finish(Tick(1)).err(),
            Some(Error::Work(Stop::Deadline))
        );
    }
    for cut in 0..turns {
        for now in [Tick(1), Tick(100)] {
            let mut work = meter();
            let mut budget = HeaderBudget::new();
            let mut cursor = Cursor::new(source, &mut work, &mut budget);
            for _ in 0..cut {
                assert_ne!(cursor.poll(Tick(1)).unwrap(), Status::Complete);
            }
            let expected = if now == Tick(1) {
                Error::InvalidState
            } else {
                Error::Work(Stop::Deadline)
            };
            assert_eq!(cursor.finish(now).err(), Some(expected));
        }
    }
    for check in [false, true] {
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut cursor = Cursor::new(source, &mut work, &mut budget);
        drain(&mut cursor).1.unwrap();
        if check {
            assert_eq!(
                cursor.check_deadline(Tick(100)),
                Err(Error::Work(Stop::Deadline))
            );
            assert!(!cursor.is_complete());
        }
        assert_eq!(
            cursor.finish(Tick(100)).err(),
            Some(Error::Work(Stop::Deadline))
        );
    }
    assert_eq!(
        Cursor::new(source, &mut meter(), &mut HeaderBudget::new())
            .finish(Tick(1))
            .err(),
        Some(Error::InvalidState)
    );
    assert_eq!(
        Cursor::new(source, &mut meter(), &mut HeaderBudget::new())
            .finish(Tick(100))
            .err(),
        Some(Error::Work(Stop::Deadline))
    );
}

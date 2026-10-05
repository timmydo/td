#![allow(clippy::unwrap_used, clippy::panic)]
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
fn drain(mut cursor: Cursor<'_, '_>) -> (String, Result<End, Error>, usize) {
    let mut output = String::new();
    for turn in 1..100_000 {
        match cursor.poll(Tick(1)) {
            Ok(Status::Yield) => {
                assert_eq!(cursor.end(), None);
                assert!(!cursor.is_complete());
            }
            Ok(Status::Scalar(value)) => {
                assert_eq!(cursor.end(), None);
                assert!(!cursor.is_complete());
                output.push(value);
            }
            Ok(Status::Complete) => {
                assert!(cursor.is_complete());
                assert_eq!(cursor.poll(Tick(100)), Ok(Status::Complete));
                return (output, cursor.finish(Tick(1)).map(|(_, _, end)| end), turn);
            }
            Err(error) => {
                assert_eq!(cursor.end(), None);
                assert!(!cursor.is_complete());
                if matches!(
                    error,
                    Error::Selection(selection::Error::Malformed)
                        | Error::Words(word::Error::MalformedFold)
                        | Error::Literal(literal::Error::MalformedUri)
                ) {
                    assert!(matches!(cursor.owner, Owner::Rejected(..)));
                } else {
                    assert!(matches!(cursor.owner, Owner::Retired));
                }
                assert_eq!(cursor.poll(Tick(100)), Err(error));
                assert_eq!(cursor.check_deadline(Tick(100)), Err(error));
                assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
                return (output, Err(error), turn);
            }
        }
    }
    panic!("field did not finish")
}
fn standalone(
    source: &[u8],
    work: &mut Meter,
    budget: &mut HeaderBudget,
) -> (String, Result<End, Error>, usize) {
    fn interpret(
        source: &[u8],
        work: &mut Meter,
        budget: &mut HeaderBudget,
        output: &mut String,
        turns: &mut usize,
    ) -> Result<End, Error> {
        let mut select = selection::Cursor::new(source, work, budget);
        loop {
            *turns += 1;
            if matches!(
                select.poll(Tick(1)).map_err(Error::Selection)?,
                selection::Status::Complete(_)
            ) {
                break;
            }
        }
        let (work, budget, spelling) = select.finish(Tick(1)).map_err(Error::Selection)?;
        let selected = source.get(spelling.start..spelling.end).unwrap();
        let mut words = word::Cursor::new_run(selected, work, budget);
        loop {
            *turns += 1;
            match words.poll(Tick(1)).map_err(Error::Words)? {
                word::Status::Yield => {}
                word::Status::Scalar(value) => output.push(value),
                word::Status::Complete => break,
            }
        }
        let (work, budget, end) = words.finish(Tick(1)).map_err(Error::Words)?;
        if end.recognized {
            return Ok(End {
                spelling,
                encoded_words: true,
                encoding_problem: end.encoding_problem,
            });
        }
        assert!(output.is_empty());
        let mut literal = literal::Cursor::new(selected, work, budget);
        loop {
            *turns += 1;
            match literal.poll(Tick(1)).map_err(Error::Literal)? {
                literal::Status::Yield => {}
                literal::Status::Octet { byte, .. } => output.push(char::from(byte)),
                literal::Status::Complete => break,
            }
        }
        literal.finish(Tick(1)).map_err(Error::Literal)?;
        Ok(End {
            spelling,
            encoded_words: false,
            encoding_problem: false,
        })
    }
    let mut output = String::new();
    let mut turns = 0;
    let end = interpret(source, work, budget, &mut output, &mut turns);
    (output, end, turns)
}
#[test]
fn complete_field_forms_match_standalone_children_and_original_costs() {
    for (source, wanted, encoded, problem) in [
        (
            b"(lead) ../A%2fb?x=Y#Z (tail)".as_slice(),
            "../A%2fb?x=Y#Z",
            false,
            false,
        ),
        (
            b"(lead) =?utf-8?Q?e=CC=81?=\r\n =?ascii?Q?/a=20b?= (tail)",
            "e\u{301}/a b",
            true,
            false,
        ),
        (b"=?utf-8?Q?=FF?= =?ascii?B?Zm9v?=", "�foo", true, true),
        (
            b"=?ascii?Q?a?= =?unknown?Q?b?=",
            "=?ascii?Q?a?==?unknown?Q?b?=",
            false,
            false,
        ),
        (
            b"https://x/=?ascii?Q?a?=",
            "https://x/=?ascii?Q?a?=",
            false,
            false,
        ),
        (b"", "", false, false),
    ] {
        let mut reference_work = meter();
        let mut reference_budget = HeaderBudget::new();
        let reference = standalone(source, &mut reference_work, &mut reference_budget);
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let (output, end, turns) = drain(Cursor::new(source, &mut work, &mut budget));
        assert_eq!((output.clone(), end, turns), reference);
        assert_eq!(output, wanted);
        let end = end.unwrap();
        assert_eq!(
            (end.encoded_words, end.encoding_problem),
            (encoded, problem)
        );
        assert!(source.get(end.spelling.start..end.spelling.end).is_some());
        assert_eq!(work.remaining(), reference_work.remaining());
        assert_eq!(
            budget.source_bytes_remaining(),
            reference_budget.source_bytes_remaining()
        );
        assert_eq!(budget.steps_remaining(), reference_budget.steps_remaining());
    }
}
#[test]
fn malformed_fields_never_emit_prefixes_or_rescue_resource_failure() {
    for (source, error) in [
        (
            b"(broken".as_slice(),
            Error::Selection(selection::Error::Malformed),
        ),
        (
            b"=?ascii?Q?a?=\r\n",
            Error::Words(word::Error::MalformedFold),
        ),
        (b"../a\r\nX", Error::Words(word::Error::MalformedFold)),
        (b"/a%", Error::Literal(literal::Error::MalformedUri)),
        (b"/a\xff", Error::Literal(literal::Error::MalformedUri)),
    ] {
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let (output, result, _) = drain(Cursor::new(source, &mut work, &mut budget));
        assert!(output.is_empty());
        assert_eq!(result, Err(error));
    }
    let mut work = Meter::new(Deadline::after(Tick(0), 100).unwrap(), Charge::default());
    let mut budget = HeaderBudget::new();
    let (_, result, _) = drain(Cursor::new(b"=?unknown?Q?a?=", &mut work, &mut budget));
    assert_eq!(
        result,
        Err(Error::Selection(selection::Error::Work(Stop::Records)))
    );
    assert_eq!(work.stopped(), Some(Stop::Records));

    let source = b"=?unknown?Q?a?=";
    let mut selection_work = meter();
    let mut selection_budget = HeaderBudget::new();
    let mut select = selection::Cursor::new(source, &mut selection_work, &mut selection_budget);
    while !matches!(
        select.poll(Tick(1)).unwrap(),
        selection::Status::Complete(_)
    ) {}
    let (selection_work, _, _) = select.finish(Tick(1)).unwrap();
    let selection_records = 100_000_000 - selection_work.remaining().records;
    let (mut work, mut budget) = grants(3, selection_records + 1);
    let (output, result, _) = drain(Cursor::new(source, &mut work, &mut budget));
    assert!(output.is_empty());
    assert_eq!(result, Err(Error::Words(word::Error::Work(Stop::Records))));
    assert_eq!(work.stopped(), Some(Stop::Records));
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
        let mut grant = work.remaining();
        match kind {
            2 => grant.io_bytes = cap,
            3 => grant.records = cap,
            4 => grant.output_bytes = cap,
            _ => panic!("bad grant"),
        }
        work = Meter::new(Deadline::after(Tick(0), 100).unwrap(), grant);
    }
    (work, budget)
}
#[test]
fn every_original_resource_cut_matches_public_children() {
    for source in [
        b"=?utf-8?Q?=C3=A9?= =?ascii?Q?/x?=".as_slice(),
        b"(x) http://[::1]/a (tail)",
        b"=?ascii?Q?a?= =?unknown?Q?b?=",
    ] {
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let expected = standalone(source, &mut work, &mut budget);
        assert!(expected.1.is_ok());
        let cost = [
            HeaderBudget::new().source_bytes_remaining() - budget.source_bytes_remaining(),
            HeaderBudget::new().steps_remaining() - budget.steps_remaining(),
            100_000_000 - work.remaining().io_bytes,
            100_000_000 - work.remaining().records,
            100_000_000 - work.remaining().output_bytes,
        ];
        assert!(cost.iter().all(|value| *value > 0));
        for (kind, used) in cost.into_iter().enumerate() {
            for cap in 0..=used {
                let (mut reference_work, mut reference_budget) = grants(kind, cap);
                let reference = standalone(source, &mut reference_work, &mut reference_budget);
                let (mut work, mut budget) = grants(kind, cap);
                let (output, end, turns) = drain(Cursor::new(source, &mut work, &mut budget));
                assert_eq!((output, end, turns), reference);
                assert_eq!(end.is_ok(), cap == used);
                assert_eq!(work.remaining(), reference_work.remaining());
                assert_eq!(work.stopped(), reference_work.stopped());
                assert_eq!(
                    budget.source_bytes_remaining(),
                    reference_budget.source_bytes_remaining()
                );
                assert_eq!(budget.steps_remaining(), reference_budget.steps_remaining());
            }
        }
    }
}
#[test]
fn every_prefix_fresh_admission_and_original_owner_handoff() {
    for source in [
        b"=?ascii?Q?a?= =?ascii?Q?b?=".as_slice(),
        b"../a",
        b"=?ascii?Q?a?= =?unknown?Q?b?=",
    ] {
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let (_, end, turns) = drain(Cursor::new(source, &mut work, &mut budget));
        assert!(end.is_ok());
        for cut in 0..=turns {
            for trial in 0..3 {
                let mut work = meter();
                let mut budget = HeaderBudget::new();
                let mut cursor = Cursor::new(source, &mut work, &mut budget);
                for _ in 0..cut {
                    cursor.poll(Tick(1)).unwrap();
                }
                let deadline = match &cursor.owner {
                    Owner::Selection(_) => Error::Selection(selection::Error::Work(Stop::Deadline)),
                    Owner::Words(_) => Error::Words(word::Error::Work(Stop::Deadline)),
                    Owner::Literal(_) => Error::Literal(literal::Error::Work(Stop::Deadline)),
                    Owner::Complete(..) => Error::Admission(nfc::Error::Work(Stop::Deadline)),
                    Owner::Rejected(..) | Owner::Retired => panic!("unexpected retirement"),
                };
                if trial == 0 {
                    if cut == turns {
                        assert_eq!(cursor.poll(Tick(100)), Ok(Status::Complete));
                    }
                    assert_eq!(cursor.check_deadline(Tick(100)), Err(deadline));
                    assert_eq!(cursor.end(), None);
                    assert!(matches!(cursor.owner, Owner::Retired));
                    assert_eq!(cursor.poll(Tick(1)), Err(deadline));
                } else if trial == 1 {
                    assert_eq!(cursor.finish(Tick(100)).err(), Some(deadline));
                } else if cut == turns {
                    assert!(cursor.finish(Tick(1)).is_ok());
                } else {
                    assert_eq!(cursor.finish(Tick(1)).err(), Some(Error::InvalidState));
                }
            }
        }
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let pointers = (std::ptr::from_ref(&work), std::ptr::from_ref(&budget));
        let mut cursor = Cursor::new(source, &mut work, &mut budget);
        while cursor.poll(Tick(1)).unwrap() != Status::Complete {}
        let (work, budget, _) = cursor.finish(Tick(1)).unwrap();
        assert_eq!(
            (std::ptr::from_ref(&*work), std::ptr::from_ref(&*budget)),
            pointers
        );
        let mut next = crate::mime_language::Cursor::new(b"fr", work, budget);
        while next.poll(Tick(1)).unwrap() != crate::mime_language::Status::Complete {}
        let (work, budget) = next.finish(Tick(1)).unwrap();
        assert_eq!(
            (std::ptr::from_ref(&*work), std::ptr::from_ref(&*budget)),
            pointers
        );
    }
}

//! One caller-selected Content-ID as a bounded, provisional JSON string.
use crate::{admission::work::Meter, nfc::HeaderBudget, ports::Tick};
pub use td_json::string::{Progress, Status};
pub type Error = td_json::string::Error<super::Error>;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct End {
    pub is_encoding_problem: bool,
}
struct Source<'a, 'w> {
    cursor: super::Cursor<'a, 'w>,
}
impl td_json::string::Source for Source<'_, '_> {
    type Context = Tick;
    type Error = super::Error;
    fn charge_output(&mut self, now: Tick, bytes: u64) -> Result<(), Self::Error> {
        self.cursor
            .inner
            .charge_output(now, bytes)
            .map_err(super::Error::from)
    }
    fn poll(&mut self, now: Tick) -> Result<td_json::string::Scalar, Self::Error> {
        self.cursor.poll(now).map(|status| match status {
            super::Status::Scalar(value) => td_json::string::Scalar::Value(value),
            super::Status::Complete => td_json::string::Scalar::Complete,
            super::Status::Yield | super::Status::Begin | super::Status::End => {
                td_json::string::Scalar::Yield
            }
        })
    }
}
/// Binds the shared framer to one CID source and original job/header owners.
/// Input authorization, field selection and publication remain caller duties.
/// Output stays provisional through Complete and fresh consuming finish.
/// Missing/malformed-field null mapping belongs to the selecting coordinator.
/// ```compile_fail,E0277
/// fn copied<T: Copy>() {} copied::<td_mta::mime_content_id::json::Cursor<'_, '_>>();
/// ```
/// ```compile_fail,E0277
/// fn cloned<T: Clone>() {} cloned::<td_mta::mime_content_id::json::Cursor<'_, '_>>();
/// ```
pub struct Cursor<'a, 'w> {
    source: Source<'a, 'w>,
    frame: td_json::string::Frame<super::Error>,
    complete: bool,
}
impl<'a, 'w> Cursor<'a, 'w> {
    pub fn new(source: &'a [u8], work: &'w mut Meter, budget: &'w mut HeaderBudget) -> Self {
        Self {
            source: Source {
                cursor: super::Cursor::new(source, work, budget),
            },
            frame: td_json::string::Frame::new(),
            complete: false,
        }
    }
    pub fn end(&self) -> Option<End> {
        if !self.complete {
            return None;
        }
        self.source
            .cursor
            .is_encoding_problem()
            .map(|is_encoding_problem| End {
                is_encoding_problem,
            })
    }
    pub fn check_deadline(&mut self, now: Tick) -> Result<(), Error> {
        let result = self.frame.check_admission(&mut self.source, now);
        if result.is_err() {
            self.complete = false;
        }
        result
    }
    pub fn poll(&mut self, now: Tick, output: &mut [u8]) -> Result<Progress, Error> {
        let result = self.frame.poll(&mut self.source, now, output);
        self.complete = matches!(
            result,
            Ok(Progress {
                status: Status::Complete,
                ..
            })
        );
        result
    }
    pub fn finish(
        mut self,
        now: Tick,
    ) -> Result<(&'w mut Meter, &'w mut HeaderBudget, End), Error> {
        self.check_deadline(now)?;
        let end = self.end().ok_or(Error::InvalidState)?;
        let (work, budget) = self.source.cursor.finish(now).map_err(Error::Source)?;
        Ok((work, budget, end))
    }
}
const _: () =
    assert!(std::mem::size_of::<Cursor<'_, '_>>() + std::mem::size_of::<HeaderBudget>() <= 640);

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
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
    fn drain(cursor: &mut Cursor<'_, '_>, width: usize) -> (Vec<u8>, Result<usize, Error>) {
        let mut output = Vec::new();
        let mut window = [0; 6];
        for turn in 1..100_000 {
            let (before, steps) = cursor.source.cursor.inner.remaining();
            let progress = cursor.poll(Tick(1), &mut window[..width]);
            let (after, remaining) = cursor.source.cursor.inner.remaining();
            assert!(before.io_bytes - after.io_bytes <= 160);
            assert!(before.records - after.records <= 16);
            assert!(steps - remaining <= 255);
            assert!(before.output_bytes - after.output_bytes <= 10);
            match progress {
                Ok(progress) => {
                    assert!(progress.written <= 6);
                    output.extend_from_slice(&window[..progress.written]);
                    if progress.status == Status::Complete {
                        assert!(cursor.end().is_some());
                        assert_eq!(
                            cursor.poll(Tick(100), &mut []),
                            Ok(Progress {
                                written: 0,
                                status: Status::Complete
                            })
                        );
                        assert_eq!(cursor.source.cursor.inner.remaining(), (after, remaining));
                        return (output, Ok(turn));
                    }
                    assert!(cursor.end().is_none());
                }
                Err(error) => {
                    assert!(cursor.end().is_none());
                    assert_eq!(cursor.poll(Tick(1), &mut window), Err(error));
                    assert_eq!(cursor.check_deadline(Tick(1)), Err(error));
                    assert_eq!(cursor.source.cursor.inner.remaining(), (after, remaining));
                    return (output, Err(error));
                }
            }
        }
        panic!("CID JSON did not finish")
    }
    #[test]
    fn literal_json_short_drains_exact_costs_and_original_handoff() {
        for (source, decoded, json, problem) in [
            ("(🐈) <A@b> (y)", "A@b", "\"A@b\"", false),
            (
                "<e\u{301}@EXAMPLE>",
                "e\u{301}@EXAMPLE",
                "\"e\u{301}@EXAMPLE\"",
                false,
            ),
            (r#"<"a\\b"@c>"#, r#""a\\b"@c"#, r#""\"a\\\\b\"@c""#, false),
            (
                "<=?utf-8?Q?name?=@b>",
                "=?utf-8?Q?name?=@b",
                "\"=?utf-8?Q?name?=@b\"",
                false,
            ),
            ("<\u{fdd0}@b>", "�@b", "\"�@b\"", true),
            ("<🐈@b>", "🐈@b", "\"🐈@b\"", false),
            (
                "<\"a\u{1}\"@b>",
                "\"a\u{1}\"@b",
                "\"\\\"a\\u0001\\\"@b\"",
                false,
            ),
            ("<a@[c\n\td]>", "a@[c\td]", "\"a@[c\\td]\"", false),
        ] {
            for width in 1..=6 {
                let mut work = meter();
                let initial = work.remaining();
                let mut budget = HeaderBudget::new();
                let identity = (std::ptr::from_ref(&work), std::ptr::from_ref(&budget));
                let mut cursor = Cursor::new(source.as_bytes(), &mut work, &mut budget);
                assert_eq!(cursor.end(), None);
                let before = cursor.source.cursor.inner.remaining();
                assert_eq!(
                    cursor.poll(Tick(1), &mut []),
                    Ok(Progress {
                        written: 0,
                        status: Status::NeedOutput
                    })
                );
                assert_eq!(cursor.source.cursor.inner.remaining(), before);
                let (bytes, done) = drain(&mut cursor, width);
                assert!(done.is_ok());
                assert_eq!(bytes, json.as_bytes());
                let (work, budget, end) = cursor.finish(Tick(1)).unwrap();
                assert_eq!(end.is_encoding_problem, problem);
                assert_eq!(
                    (std::ptr::from_ref(&*work), std::ptr::from_ref(&*budget)),
                    identity
                );
                assert_eq!(
                    initial.output_bytes - work.remaining().output_bytes,
                    2 * decoded.len() as u64 + json.len() as u64
                );
                let mut next = super::super::Cursor::new(b"<x@y>", work, budget);
                loop {
                    if next.poll(Tick(1)).unwrap() == super::super::Status::Complete {
                        break;
                    }
                }
                let (work, budget) = next.finish(Tick(1)).unwrap();
                assert_eq!(
                    (std::ptr::from_ref(&*work), std::ptr::from_ref(&*budget)),
                    identity
                );
            }
        }
    }
    #[test]
    fn whole_value_refusal_retires_provisional_open_quote() {
        let deep = format!("<a@b> {}x{}", "(".repeat(33), ")".repeat(33));
        for (source, error) in [
            (b"<local>".as_slice(), super::super::Error::Malformed),
            (b"<a@b> <c@d>", super::super::Error::Malformed),
            (b"<a@b> tail", super::super::Error::Malformed),
            (b"<\xff@b>", super::super::Error::Malformed),
            (deep.as_bytes(), super::super::Error::NestingLimit),
        ] {
            let mut work = meter();
            let mut budget = HeaderBudget::new();
            let mut cursor = Cursor::new(source, &mut work, &mut budget);
            let (bytes, result) = drain(&mut cursor, 1);
            assert_eq!(bytes, b"\"");
            assert_eq!(result, Err(Error::Source(error)));
            assert_eq!(cursor.finish(Tick(1)).err(), Some(Error::Source(error)));
        }
    }
    #[test]
    fn every_original_resource_cut_and_exact_successful_grants() {
        let source = b"<\"a\\b\"@c>";
        let mut work = meter();
        let initial = work.remaining();
        let mut budget = HeaderBudget::new();
        let initial_header = (budget.source_bytes_remaining(), budget.steps_remaining());
        let mut cursor = Cursor::new(source, &mut work, &mut budget);
        let (full_json, done) = drain(&mut cursor, 1);
        assert!(done.is_ok());
        cursor.finish(Tick(1)).unwrap();
        let used = [
            initial_header.0 - budget.source_bytes_remaining(),
            initial_header.1 - budget.steps_remaining(),
            initial.io_bytes - work.remaining().io_bytes,
            initial.records - work.remaining().records,
            initial.output_bytes - work.remaining().output_bytes,
        ];
        let mut after_output = [false; 5];
        for (kind, cost) in used.into_iter().enumerate() {
            for cap in 0..cost {
                let mut work = meter();
                let mut budget = HeaderBudget::new();
                if kind < 2 {
                    let bytes = if kind == 0 {
                        cap
                    } else {
                        budget.source_bytes_remaining()
                    };
                    let steps = if kind == 1 {
                        cap
                    } else {
                        budget.steps_remaining()
                    };
                    budget
                        .charge(
                            &mut meter(),
                            Tick(1),
                            budget.source_bytes_remaining() - bytes,
                            budget.steps_remaining() - steps,
                            &mut 0,
                        )
                        .unwrap();
                } else {
                    let mut grants = work.remaining();
                    match kind {
                        2 => grants.io_bytes = cap,
                        3 => grants.records = cap,
                        4 => grants.output_bytes = cap,
                        _ => panic!("bad cut"),
                    }
                    work = Meter::new(Deadline::after(Tick(0), 100).unwrap(), grants);
                }
                let mut cursor = Cursor::new(source, &mut work, &mut budget);
                let (bytes, result) = drain(&mut cursor, 1);
                after_output[kind] |= bytes.len() > 1;
                assert!(full_json.starts_with(&bytes));
                let expected = Error::Source(match kind {
                    0 | 1 => super::super::Error::InterpretationLimit,
                    2 => super::super::Error::Work(Stop::IoBytes),
                    3 => super::super::Error::Work(Stop::Records),
                    4 => super::super::Error::Work(Stop::OutputBytes),
                    _ => panic!("bad cut"),
                });
                assert_eq!(result, Err(expected));
                assert_eq!(cursor.finish(Tick(1)).err(), Some(expected));
            }
        }
        assert_eq!(after_output, [true; 5]);
        let mut work = Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: used[2],
                records: used[3],
                output_bytes: used[4],
                ..Charge::default()
            },
        );
        let mut budget = HeaderBudget::new();
        budget
            .charge(
                &mut meter(),
                Tick(1),
                budget.source_bytes_remaining() - used[0],
                budget.steps_remaining() - used[1],
                &mut 0,
            )
            .unwrap();
        let mut cursor = Cursor::new(source, &mut work, &mut budget);
        assert!(drain(&mut cursor, 1).1.is_ok());
        cursor.finish(Tick(1)).unwrap();
        assert_eq!(work.remaining(), Charge::default());
        assert_eq!(
            (budget.source_bytes_remaining(), budget.steps_remaining()),
            (0, 0)
        );
    }
    #[test]
    fn every_live_deadline_and_premature_finish_cut() {
        let source = b"<a@b>";
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut cursor = Cursor::new(source, &mut work, &mut budget);
        let turns = drain(&mut cursor, 1).1.unwrap();
        cursor.finish(Tick(1)).unwrap();
        for cut in 0..=turns {
            for trial in 0..4 {
                let mut work = meter();
                let mut budget = HeaderBudget::new();
                let mut cursor = Cursor::new(source, &mut work, &mut budget);
                let mut byte = [0];
                for _ in 0..cut {
                    cursor.poll(Tick(1), &mut byte).unwrap();
                }
                let expected = Error::Source(super::super::Error::Work(Stop::Deadline));
                if trial == 0 {
                    assert_eq!(cursor.check_deadline(Tick(100)), Err(expected));
                    assert_eq!(cursor.end(), None);
                    assert_eq!(cursor.poll(Tick(1), &mut byte), Err(expected));
                    assert_eq!(cursor.finish(Tick(1)).err(), Some(expected));
                } else if trial == 1 {
                    assert_eq!(cursor.finish(Tick(100)).err(), Some(expected));
                } else if trial == 2 && cut < turns {
                    assert_eq!(cursor.poll(Tick(100), &mut byte), Err(expected));
                    assert_eq!(cursor.end(), None);
                    assert_eq!(cursor.poll(Tick(1), &mut byte), Err(expected));
                    assert_eq!(cursor.finish(Tick(1)).err(), Some(expected));
                } else if cut < turns {
                    assert_eq!(cursor.finish(Tick(1)).err(), Some(Error::InvalidState));
                } else {
                    assert!(cursor.finish(Tick(1)).is_ok());
                }
            }
        }
    }
}

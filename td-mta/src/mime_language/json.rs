//! One caller-selected Content-Language value as a provisional JSON array.
use crate::{
    admission::work::{Charge, Meter},
    decode_work::{Parsing, Work},
    nfc::HeaderBudget,
    ports::Tick,
};
pub use td_json::string_array::{Progress, Role, Status};
pub type Error = td_json::string_array::Error<super::Error>;
struct Source<'a, 'w> {
    cursor: super::Cursor<'a, 'w>,
    bytes: &'a [u8],
    tag: Option<super::Extent>,
    position: usize,
}
impl td_json::string_array::Source for Source<'_, '_> {
    type Context = Tick;
    type Error = super::Error;
    fn charge_output(
        &mut self,
        now: Tick,
        output_bytes: u64,
        _: td_json::string_array::Role,
    ) -> Result<(), Self::Error> {
        self.cursor.check_deadline(now)?;
        self.cursor
            .work
            .charge(
                now,
                Charge {
                    output_bytes,
                    ..Charge::default()
                },
            )
            .map_err(super::Error::Work)
    }
    fn poll(
        &mut self,
        now: Tick,
        _: td_json::string_array::Role,
    ) -> Result<td_json::string_array::Event, Self::Error> {
        use td_json::string_array::Event;
        if let Some(tag) = self.tag {
            if self.position == tag.end {
                self.tag = None;
                return Ok(Event::End);
            }
            Parsing::new(
                self.cursor.work,
                self.cursor.budget,
                &mut self.cursor.credit,
            )
            .charge(
                now,
                Charge {
                    io_bytes: 1,
                    ..Charge::default()
                },
            )
            .map_err(|error| super::Error::from(td_header::language_list::Error::Work(error)))?;
            let byte = self
                .bytes
                .get(self.position)
                .copied()
                .ok_or(super::Error::InvalidState)?;
            if !(byte.is_ascii_alphanumeric() || byte == b'-') {
                return Err(super::Error::InvalidState);
            }
            self.position = self
                .position
                .checked_add(1)
                .ok_or(super::Error::InvalidState)?;
            return Ok(Event::Scalar(char::from(byte)));
        }
        match self.cursor.poll(now)? {
            super::Status::Yield => Ok(Event::Yield),
            super::Status::Complete => Ok(Event::Complete),
            super::Status::Tag(tag) => {
                if tag.start >= tag.end || tag.end > self.bytes.len() {
                    return Err(super::Error::InvalidState);
                }
                self.position = tag.start;
                self.tag = Some(tag);
                Ok(Event::Begin)
            }
        }
    }
}
/// Bind one authorized complete selected field value and original job/header owners.
/// Tag order, case and duplicates remain literal. Every fragment stays provisional
/// through whole-list syntax success and fresh consuming finish; null mapping and
/// publication are caller duties. No source grant, list storage or fresh allowance.
/// ```compile_fail,E0277
/// fn copied<T: Copy>() {} copied::<td_mta::mime_language::json::Cursor<'_, '_>>();
/// ```
/// ```compile_fail,E0277
/// fn cloned<T: Clone>() {} cloned::<td_mta::mime_language::json::Cursor<'_, '_>>();
/// ```
pub struct Cursor<'a, 'w> {
    source: Source<'a, 'w>,
    frame: td_json::string_array::Frame<super::Error>,
}
impl<'a, 'w> Cursor<'a, 'w> {
    pub fn new(bytes: &'a [u8], work: &'w mut Meter, budget: &'w mut HeaderBudget) -> Self {
        Self {
            source: Source {
                cursor: super::Cursor::new(bytes, work, budget),
                bytes,
                tag: None,
                position: 0,
            },
            frame: td_json::string_array::Frame::new(),
        }
    }
    pub fn is_complete(&self) -> bool {
        self.frame.is_complete() && self.source.cursor.is_complete()
    }
    pub fn check_deadline(&mut self, now: Tick) -> Result<(), Error> {
        self.frame.check_admission(&mut self.source, now)
    }
    pub fn poll(&mut self, now: Tick, output: &mut [u8]) -> Result<Progress, Error> {
        self.frame.poll(&mut self.source, now, output)
    }
    pub fn finish(mut self, now: Tick) -> Result<(&'w mut Meter, &'w mut HeaderBudget), Error> {
        self.check_deadline(now)?;
        if !self.is_complete() {
            return Err(Error::InvalidState(Role::Array));
        }
        self.source.cursor.finish(now).map_err(Error::Source)
    }
}
const _: () =
    assert!(std::mem::size_of::<Cursor<'_, '_>>() + std::mem::size_of::<HeaderBudget>() <= 512);

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
    type Drained = (Vec<u8>, Result<usize, Error>);
    fn drain(cursor: &mut Cursor<'_, '_>, width: usize) -> Drained {
        drain_measured(cursor, width).0
    }
    fn drain_measured(cursor: &mut Cursor<'_, '_>, width: usize) -> (Drained, [u64; 4]) {
        let mut maximum = [0; 4];
        let mut output = Vec::new();
        let mut window = [0; 6];
        for turn in 1..100_000 {
            let (before, steps) = (
                cursor.source.cursor.work.remaining(),
                cursor.source.cursor.budget.steps_remaining(),
            );
            let replay = cursor
                .source
                .tag
                .is_some_and(|tag| cursor.source.position < tag.end);
            let position = cursor.source.position;
            let progress = cursor.poll(Tick(1), &mut window[..width]);
            let (after, remaining) = (
                cursor.source.cursor.work.remaining(),
                cursor.source.cursor.budget.steps_remaining(),
            );
            assert!(before.io_bytes - after.io_bytes <= 160);
            assert!(before.records - after.records <= 12);
            assert!(steps - remaining <= 192);
            assert!(before.output_bytes - after.output_bytes <= 1);
            let costs = [
                before.io_bytes - after.io_bytes,
                steps - remaining,
                before.records - after.records,
                before.output_bytes - after.output_bytes,
            ];
            for (high, cost) in maximum.iter_mut().zip(costs) {
                *high = (*high).max(cost);
            }
            if replay && cursor.source.position > position && progress.is_ok() {
                assert_eq!(cursor.source.position - position, 1);
                assert_eq!((costs[0], costs[1], costs[3]), (1, 1, 1));
                assert!(costs[2] <= 1);
            }
            match progress {
                Ok(progress) => {
                    assert!(progress.written <= 1);
                    output.extend_from_slice(&window[..progress.written]);
                    if progress.status == Status::Complete {
                        assert!(cursor.is_complete());
                        assert_eq!(
                            cursor.poll(Tick(100), &mut []),
                            Ok(Progress {
                                written: 0,
                                status: Status::Complete
                            })
                        );
                        assert_eq!(
                            (
                                cursor.source.cursor.work.remaining(),
                                cursor.source.cursor.budget.steps_remaining()
                            ),
                            (after, remaining)
                        );
                        return ((output, Ok(turn)), maximum);
                    }
                    assert!(!cursor.is_complete());
                }
                Err(error) => {
                    assert!(!cursor.is_complete());
                    assert_eq!(cursor.poll(Tick(1), &mut window), Err(error));
                    assert_eq!(cursor.check_deadline(Tick(1)), Err(error));
                    assert_eq!(
                        (
                            cursor.source.cursor.work.remaining(),
                            cursor.source.cursor.budget.steps_remaining()
                        ),
                        (after, remaining)
                    );
                    return ((output, Err(error)), maximum);
                }
            }
        }
        panic!("language JSON did not finish")
    }
    #[test]
    fn literal_arrays_exact_grammar_replay_costs_and_original_handoff() {
        for (source, expected, replay) in [
            (
                "(🐈) EN-us, en-US, x-Ab12 (tail)",
                r#"["EN-us","en-US","x-Ab12"]"#,
                16,
            ),
            ("en, en", r#"["en","en"]"#, 4),
            ("en-scouse", r#"["en-scouse"]"#, 9),
            ("(x) i-mingo,\r\n\tfr", r#"["i-mingo","fr"]"#, 9),
        ] {
            let mut grammar_work = meter();
            let mut grammar_budget = HeaderBudget::new();
            let mut grammar = super::super::Cursor::new(
                source.as_bytes(),
                &mut grammar_work,
                &mut grammar_budget,
            );
            while grammar.poll(Tick(1)).unwrap() != super::super::Status::Complete {}
            grammar.finish(Tick(1)).unwrap();
            let visits = meter().remaining().io_bytes - grammar_work.remaining().io_bytes;
            let steps = HeaderBudget::new().steps_remaining() - grammar_budget.steps_remaining();
            for width in 1..=6 {
                let mut work = meter();
                let initial = work.remaining();
                let mut budget = HeaderBudget::new();
                let identity = (std::ptr::from_ref(&work), std::ptr::from_ref(&budget));
                let mut cursor = Cursor::new(source.as_bytes(), &mut work, &mut budget);
                let before = (
                    cursor.source.cursor.work.remaining(),
                    cursor.source.cursor.budget.steps_remaining(),
                );
                assert_eq!(
                    cursor.poll(Tick(1), &mut []),
                    Ok(Progress {
                        written: 0,
                        status: Status::NeedOutput
                    })
                );
                assert_eq!(
                    (
                        cursor.source.cursor.work.remaining(),
                        cursor.source.cursor.budget.steps_remaining()
                    ),
                    before
                );
                let (bytes, done) = drain(&mut cursor, width);
                assert!(done.is_ok());
                assert_eq!(bytes, expected.as_bytes());
                let (work, budget) = cursor.finish(Tick(1)).unwrap();
                assert_eq!(
                    (std::ptr::from_ref(&*work), std::ptr::from_ref(&*budget)),
                    identity
                );
                assert_eq!(
                    initial.output_bytes - work.remaining().output_bytes,
                    expected.len() as u64
                );
                assert_eq!(
                    initial.io_bytes - work.remaining().io_bytes,
                    visits + replay
                );
                assert_eq!(
                    HeaderBudget::new().steps_remaining() - budget.steps_remaining(),
                    steps + replay
                );
                assert_eq!(
                    initial.records - work.remaining().records,
                    (steps + replay).div_ceil(16)
                );
                let mut next = super::super::Cursor::new(b"fr", work, budget);
                while next.poll(Tick(1)).unwrap() != super::super::Status::Complete {}
                let (work, budget) = next.finish(Tick(1)).unwrap();
                assert_eq!(
                    (std::ptr::from_ref(&*work), std::ptr::from_ref(&*budget)),
                    identity
                );
            }
        }
        let long = format!("({})x{}", "🐈".repeat(1024), "-abcdefgh".repeat(1024));
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let (result, maximum) =
            drain_measured(&mut Cursor::new(long.as_bytes(), &mut work, &mut budget), 6);
        assert!(result.1.is_ok());
        assert_eq!(maximum, [160, 192, 12, 1]);
    }
    #[test]
    fn malformed_tail_retires_provisional_array_and_tags() {
        let deep = format!("en {}x{}", "(".repeat(33), ")".repeat(33));
        for (source, expected, prefix) in [
            (
                b"".as_slice(),
                super::super::Error::Malformed,
                b"[".as_slice(),
            ),
            (b"en,", super::super::Error::Malformed, b"[\"en\""),
            (b"en;q=1", super::super::Error::Malformed, b"[\"en\""),
            (b"en (bad", super::super::Error::Malformed, b"[\"en\""),
            (
                deep.as_bytes(),
                super::super::Error::NestingLimit,
                b"[\"en\"",
            ),
        ] {
            let mut work = meter();
            let mut budget = HeaderBudget::new();
            let mut cursor = Cursor::new(source, &mut work, &mut budget);
            let (bytes, result) = drain(&mut cursor, 1);
            assert_eq!(bytes, prefix);
            assert_eq!(result, Err(Error::Source(expected)));
            assert_eq!(cursor.finish(Tick(1)).err(), Some(Error::Source(expected)));
        }
    }
    #[test]
    fn every_original_resource_cut_and_exact_successful_grants() {
        let source = b"(x) EN-us, fr (tail)";
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
                if kind < 2 {
                    let mut next = super::super::Cursor::new(b"en", &mut work, &mut budget);
                    assert_eq!(
                        next.poll(Tick(1)),
                        Err(super::super::Error::InterpretationLimit)
                    );
                    assert_eq!(work.stopped(), None);
                } else {
                    let stop = match kind {
                        2 => Stop::IoBytes,
                        3 => Stop::Records,
                        4 => Stop::OutputBytes,
                        _ => panic!("bad cut"),
                    };
                    assert_eq!(work.stopped(), Some(stop));
                }
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
        let source = b"en, fr (tail)";
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut cursor = Cursor::new(source, &mut work, &mut budget);
        let turns = drain(&mut cursor, 1).1.unwrap();
        cursor.finish(Tick(1)).unwrap();
        for cut in 0..=turns {
            for trial in 0..4 {
                if cut == turns && trial == 2 {
                    continue;
                }
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
                    assert!(!cursor.is_complete());
                    assert_eq!(cursor.poll(Tick(1), &mut byte), Err(expected));
                    assert_eq!(cursor.finish(Tick(1)).err(), Some(expected));
                } else if trial == 1 {
                    assert_eq!(cursor.finish(Tick(100)).err(), Some(expected));
                } else if trial == 2 && cut < turns {
                    assert_eq!(cursor.poll(Tick(100), &mut byte), Err(expected));
                    assert!(!cursor.is_complete());
                    assert_eq!(cursor.poll(Tick(1), &mut byte), Err(expected));
                    assert_eq!(cursor.finish(Tick(1)).err(), Some(expected));
                } else if cut < turns {
                    assert_eq!(
                        cursor.finish(Tick(1)).err(),
                        Some(Error::InvalidState(Role::Array))
                    );
                } else {
                    assert!(cursor.finish(Tick(1)).is_ok());
                }
            }
        }
    }
}

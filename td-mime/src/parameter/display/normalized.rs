//! NFC over original-source parameter display with quota-free replay checkpoints.
use super::{Checkpoint, Cursor as Display, Decoded, Status as DisplayStatus};
use crate::{
    decode_work,
    fields::Kind,
    nfc::{self, HeaderBudget, Scratch},
    parameter::{Attribute, Error as ParameterError},
    time::Tick,
    unicode::{self, Decomposition},
    work::{Charge, Meter},
};
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Parameter(ParameterError),
    Normalization(nfc::Error),
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Parameter(error) => write!(f, "parameter normalization source: {error}"),
            Self::Normalization(error) => write!(f, "parameter normalization: {error}"),
        }
    }
}
impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Parameter(error) => Some(error),
            Self::Normalization(error) => Some(error),
        }
    }
}
impl From<td_nfc::Error<Error>> for Error {
    fn from(error: td_nfc::Error<Error>) -> Self {
        match error {
            td_nfc::Error::Source(error) => error,
            td_nfc::Error::InvalidState => Self::Normalization(nfc::Error::InvalidState),
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    Yield,
    Scalar(char),
    Complete(Decoded),
}
#[derive(Clone, Copy)]
struct Source<'a> {
    source: &'a [u8],
    kind: Kind,
    attribute: Attribute,
    checkpoint: Checkpoint<'a>,
    turn: u64,
    pending: Option<Decomposition>,
    next: u8,
    decoded: Option<Decoded>,
}
struct Context<'w> {
    work: &'w mut Meter,
    budget: &'w mut HeaderBudget,
    credit: &'w mut u8,
    now: Tick,
    decoded: &'w mut Option<Decoded>,
}
impl Context<'_> {
    fn charge(&mut self, steps: u64) -> Result<(), Error> {
        self.budget
            .charge_local(self.work, self.now, 0, steps, self.credit)
            .map_err(Error::Normalization)
    }
}
impl td_nfc::Admission for Context<'_> {
    type Error = Error;
    fn step(&mut self) -> Result<(), Error> {
        self.charge(1)
    }
}
impl td_nfc::Source for Source<'_> {
    type Error = Error;
    fn at(&self, other: &Self) -> bool {
        std::ptr::eq(self.source, other.source)
            && self.kind == other.kind
            && self.attribute == other.attribute
            && self.turn == other.turn
            && self.pending == other.pending
            && self.next == other.next
    }
    fn compose(a: char, b: char) -> Result<Option<char>, Error> {
        unicode::compose(a, b).map_err(|_| Error::Normalization(nfc::Error::InvalidTable))
    }
    fn is_encoding_problem(&self) -> bool {
        // The passive source diagnostic becomes authoritative at source EOF.
        self.decoded.is_some_and(|d| d.is_encoding_problem)
    }
    fn turns(&self) -> u8 {
        1
    }
}
impl td_nfc::Reader<Context<'_>> for Source<'_> {
    fn read(&mut self, ctx: &mut Context<'_>) -> Result<td_nfc::Read, Error> {
        if self.pending.is_none() {
            ctx.charge(1)?;
            let mut display = Display::resume(self.checkpoint);
            let status = display
                .poll_with_work(
                    ctx.now,
                    &mut decode_work::Parsing::new(ctx.work, ctx.budget, ctx.credit),
                )
                .map_err(Error::Parameter)?;
            // Later decomposition refusal retires the enclosing engine.
            self.turn = self
                .turn
                .checked_add(1)
                .ok_or(Error::Normalization(nfc::Error::InvalidState))?;
            self.checkpoint = display.checkpoint().map_err(Error::Parameter)?;
            let scalar = match status {
                DisplayStatus::Yield => return Ok(td_nfc::Read::Yield),
                DisplayStatus::Complete(decoded) => {
                    self.decoded = Some(decoded);
                    *ctx.decoded = Some(decoded);
                    return Ok(td_nfc::Read::End);
                }
                DisplayStatus::Scalar(value) => value,
            };
            ctx.charge(1)?;
            self.pending = Some(
                unicode::decompose(scalar)
                    .map_err(|_| Error::Normalization(nfc::Error::InvalidTable))?,
            );
            self.next = 0;
        }
        ctx.charge(1)?;
        let pending = self
            .pending
            .ok_or(Error::Normalization(nfc::Error::InvalidState))?;
        let value = pending
            .iter()
            .nth(usize::from(self.next))
            .ok_or(Error::Normalization(nfc::Error::InvalidState))?;
        let class = unicode::combining_class(value)
            .map_err(|_| Error::Normalization(nfc::Error::InvalidTable))?;
        self.next = self
            .next
            .checked_add(1)
            .ok_or(Error::Normalization(nfc::Error::InvalidState))?;
        if usize::from(self.next) == pending.iter().len() {
            self.pending = None;
            self.next = 0;
        }
        Ok(td_nfc::Read::Cell(td_nfc::Cell { value, class }))
    }
}
/// One owner binds original budgets and scratch across all checkpoint replay.
/// Scalars remain provisional; drain the complete source, charge output and
/// obtain fresh final admission before publication. Cached completion is inert.
/// ```compile_fail,E0277
/// fn copied<T: Copy>() {} copied::<td_mime::parameter::display::normalized::Cursor<'_, '_>>();
/// ```
/// ```compile_fail,E0277
/// fn cloned<T: Clone>() {} cloned::<td_mime::parameter::display::normalized::Cursor<'_, '_>>();
/// ```
pub struct Cursor<'a, 'w> {
    engine: td_nfc::Cursor<'w, Source<'a>>,
    work: &'w mut Meter,
    budget: &'w mut HeaderBudget,
    credit: u8,
    decoded: Option<Decoded>,
}
impl<'a, 'w> Cursor<'a, 'w> {
    pub fn new(
        source: &'a [u8],
        kind: Kind,
        attribute: Attribute,
        scratch: &'w mut Scratch,
        work: &'w mut Meter,
        budget: &'w mut HeaderBudget,
    ) -> Result<Self, Error> {
        let checkpoint = Display::new(source, kind, attribute)
            .checkpoint()
            .map_err(Error::Parameter)?;
        Ok(Self {
            engine: td_nfc::Cursor::new(
                Source {
                    source,
                    kind,
                    attribute,
                    checkpoint,
                    turn: 0,
                    pending: None,
                    next: 0,
                    decoded: None,
                },
                scratch,
            ),
            work,
            budget,
            credit: 0,
            decoded: None,
        })
    }
    pub fn poll(&mut self, now: Tick) -> Result<Status, Error> {
        let event = self
            .engine
            .poll(&mut Context {
                work: self.work,
                budget: self.budget,
                credit: &mut self.credit,
                now,
                decoded: &mut self.decoded,
            })
            .map_err(Error::from);
        let result = event.and_then(|event| match event {
            td_nfc::Status::Yield => Ok(Status::Yield),
            td_nfc::Status::Scalar(value) => Ok(Status::Scalar(value)),
            td_nfc::Status::Complete => {
                let decoded = self
                    .decoded
                    .ok_or(Error::Normalization(nfc::Error::InvalidState))?;
                Ok(Status::Complete(decoded))
            }
        });
        self.outcome(result)
    }
    fn outcome<T>(&mut self, result: Result<T, Error>) -> Result<T, Error> {
        if let Err(error) = result {
            self.decoded = None;
            return match self.engine.check(|| Err(error)) {
                Err(original) => Err(Error::from(original)),
                Ok(()) => Err(error),
            };
        }
        result
    }
    /// Obtain fresh admission and release only healthy Done. The final scalar
    /// may precede Complete; serialize/charge that scalar before release.
    pub fn finish(
        mut self,
        now: Tick,
    ) -> Result<(&'w mut Meter, &'w mut HeaderBudget, &'w mut Scratch), Error> {
        self.check_deadline(now)?;
        Ok((
            self.work,
            self.budget,
            self.engine.into_scratch().map_err(Error::from)?,
        ))
    }
    pub fn check_deadline(&mut self, now: Tick) -> Result<(), Error> {
        let result = self
            .engine
            .check(|| {
                self.budget
                    .charge_local(self.work, now, 0, 0, &mut self.credit)
                    .map_err(Error::Normalization)
            })
            .map_err(Error::from);
        self.outcome(result)
    }
    pub fn charge_output(&mut self, now: Tick, bytes: u64) -> Result<(), Error> {
        let result = self
            .engine
            .check(|| {
                self.budget
                    .charge_local(self.work, now, 0, 0, &mut self.credit)
                    .map_err(Error::Normalization)?;
                self.work
                    .charge(
                        now,
                        Charge {
                            output_bytes: bytes,
                            ..Charge::default()
                        },
                    )
                    .map_err(|stop| Error::Normalization(nfc::Error::Work(stop)))
            })
            .map_err(Error::from);
        self.outcome(result)
    }
}
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::time::Deadline;

    fn work() -> Meter {
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
    #[test]
    fn all_original_quota_cuts_and_final_output_deadline_retire_normalization() {
        for source in [
            b"attachment;filename*=utf-8''e%CC%81".as_slice(),
            b"attachment;filename=\"=?utf-8?Q?e?= =?utf-8?Q?=CC=81?=\"",
            b"attachment;filename=saved;filename*=utf-8''%xx",
            b"attachment;x=missing",
            b"attachment;filename=first;filename=second;filename*=utf-8'en'%xx",
        ] {
            let mut original = work();
            let mut budget = HeaderBudget::new();
            let mut scratch = Scratch::new();
            let start = (
                original.remaining(),
                budget.source_bytes_remaining(),
                budget.steps_remaining(),
            );
            let mut c = Cursor::new(
                source,
                Kind::ContentDisposition,
                Attribute::Filename,
                &mut scratch,
                &mut original,
                &mut budget,
            )
            .unwrap();
            let decoded = loop {
                let before = (
                    c.work.remaining(),
                    c.budget.source_bytes_remaining(),
                    c.budget.steps_remaining(),
                );
                c.check_deadline(Tick(1)).unwrap();
                let event = c.poll(Tick(1)).unwrap();
                assert!(before.0.io_bytes - c.work.remaining().io_bytes <= 225);
                assert!(before.0.records - c.work.remaining().records <= 30);
                assert!(before.1 - c.budget.source_bytes_remaining() <= 225);
                assert!(before.2 - c.budget.steps_remaining() <= 457);
                match event {
                    Status::Complete(d) => break d,
                    Status::Scalar(ch) => c.charge_output(Tick(1), ch.len_utf8() as u64).unwrap(),
                    _ => {}
                }
            };
            let visits = start.1 - c.budget.source_bytes_remaining();
            let steps = start.2 - c.budget.steps_remaining();
            let records = start.0.records - c.work.remaining().records;
            let cached = (
                c.work.remaining(),
                c.budget.source_bytes_remaining(),
                c.budget.steps_remaining(),
                c.credit,
            );
            assert_eq!(c.poll(Tick(100)), Ok(Status::Complete(decoded)));
            assert_eq!(
                (
                    c.work.remaining(),
                    c.budget.source_bytes_remaining(),
                    c.budget.steps_remaining(),
                    c.credit
                ),
                cached
            );
            let late = Error::Normalization(nfc::Error::Work(crate::work::Stop::Deadline));
            assert_eq!(c.check_deadline(Tick(100)), Err(late));
            assert_eq!(c.poll(Tick(1)), Err(late));
            assert_eq!(c.charge_output(Tick(1), 0), Err(late));
            assert!(matches!(c.finish(Tick(1)), Err(error) if error == late));
            for flavor in 0..4 {
                let total = match flavor {
                    0 | 2 => visits,
                    1 => steps,
                    _ => records,
                };
                for limit in 0..total {
                    let mut original = if flavor < 2 {
                        work()
                    } else {
                        Meter::new(
                            Deadline::after(Tick(0), 100).unwrap(),
                            Charge {
                                io_bytes: if flavor == 2 { limit } else { visits },
                                records: if flavor == 3 { limit } else { records },
                                output_bytes: 100_000_000,
                                ..Charge::default()
                            },
                        )
                    };
                    let mut budget = HeaderBudget::new();
                    if flavor < 2 {
                        let bytes = if flavor == 0 {
                            budget.source_bytes_remaining() - limit
                        } else {
                            0
                        };
                        let steps = if flavor == 1 {
                            budget.steps_remaining() - limit
                        } else {
                            0
                        };
                        budget
                            .charge_local(&mut original, Tick(1), bytes, steps, &mut 0)
                            .unwrap();
                    }
                    let mut scratch = Scratch::new();
                    let mut c = Cursor::new(
                        source,
                        Kind::ContentDisposition,
                        Attribute::Filename,
                        &mut scratch,
                        &mut original,
                        &mut budget,
                    )
                    .unwrap();
                    let error = loop {
                        match c.poll(Tick(1)) {
                            Ok(Status::Complete(_)) => panic!("cut completed normalization"),
                            Ok(Status::Scalar(ch)) => {
                                c.charge_output(Tick(1), ch.len_utf8() as u64).unwrap()
                            }
                            Ok(_) => {}
                            Err(error) => break error,
                        }
                    };
                    match (flavor, error) {
                        (
                            0 | 1,
                            Error::Parameter(ParameterError::InterpretationLimit)
                            | Error::Normalization(nfc::Error::InterpretationLimit),
                        ) => {}
                        (
                            2,
                            Error::Parameter(ParameterError::Work(crate::work::Stop::IoBytes))
                            | Error::Normalization(nfc::Error::Work(crate::work::Stop::IoBytes)),
                        ) => {}
                        (
                            3,
                            Error::Parameter(ParameterError::Work(crate::work::Stop::Records))
                            | Error::Normalization(nfc::Error::Work(crate::work::Stop::Records)),
                        ) => {}
                        _ => panic!("wrong cut {flavor}: {error:?}"),
                    }
                    let retired = (
                        c.work.remaining(),
                        c.budget.source_bytes_remaining(),
                        c.budget.steps_remaining(),
                        c.credit,
                    );
                    assert_eq!(c.poll(Tick(1)), Err(error));
                    assert_eq!(c.check_deadline(Tick(1)), Err(error));
                    assert_eq!(c.charge_output(Tick(1), 0), Err(error));
                    assert_eq!(
                        (
                            c.work.remaining(),
                            c.budget.source_bytes_remaining(),
                            c.budget.steps_remaining(),
                            c.credit
                        ),
                        retired
                    );
                    assert!(matches!(c.finish(Tick(1)), Err(e) if e == error));
                }
            }
        }
    }

    #[test]
    fn partial_output_and_invalid_purpose_cannot_publish_or_release() {
        for limit in 0..3 {
            let mut original = Meter::new(
                Deadline::after(Tick(0), 100).unwrap(),
                Charge {
                    io_bytes: 100_000,
                    records: 100_000,
                    output_bytes: limit,
                    ..Charge::default()
                },
            );
            let mut budget = HeaderBudget::new();
            let mut scratch = Scratch::new();
            let mut c = Cursor::new(
                b"attachment;filename*=utf-8''e%CC%81x",
                Kind::ContentDisposition,
                Attribute::Filename,
                &mut scratch,
                &mut original,
                &mut budget,
            )
            .unwrap();
            let failure = loop {
                match c.poll(Tick(1)).unwrap() {
                    Status::Yield => {}
                    Status::Complete(_) => panic!("output cut completed"),
                    Status::Scalar(ch) => {
                        if let Err(error) = c.charge_output(Tick(1), ch.len_utf8() as u64) {
                            break error;
                        }
                    }
                }
            };
            assert_eq!(
                failure,
                Error::Normalization(nfc::Error::Work(crate::work::Stop::OutputBytes))
            );
            assert_eq!(c.decoded, None);
            let before = (
                c.work.remaining(),
                c.budget.source_bytes_remaining(),
                c.budget.steps_remaining(),
                c.credit,
            );
            assert_eq!(c.poll(Tick(1)), Err(failure));
            assert_eq!(c.check_deadline(Tick(1)), Err(failure));
            assert_eq!(
                (
                    c.work.remaining(),
                    c.budget.source_bytes_remaining(),
                    c.budget.steps_remaining(),
                    c.credit
                ),
                before
            );
            assert!(matches!(c.finish(Tick(1)), Err(error) if error == failure));
        }
        for (source, kind, attribute) in [
            (
                b"text/plain".as_slice(),
                Kind::ContentType,
                Attribute::Boundary,
            ),
            (b"base64", Kind::TransferEncoding, Attribute::Filename),
            (
                b"attachment;filename=x;",
                Kind::ContentDisposition,
                Attribute::Filename,
            ),
        ] {
            let mut original = work();
            let mut budget = HeaderBudget::new();
            let mut scratch = Scratch::new();
            let mut c = Cursor::new(
                source,
                kind,
                attribute,
                &mut scratch,
                &mut original,
                &mut budget,
            )
            .unwrap();
            let mut failed = false;
            for _ in 0..10000 {
                match c.poll(Tick(1)) {
                    Ok(Status::Yield) => {}
                    Ok(_) => panic!("invalid source emitted output"),
                    Err(error) => {
                        assert_eq!(
                            error,
                            Error::Parameter(if source.last() == Some(&b';') {
                                ParameterError::Malformed
                            } else {
                                ParameterError::InvalidState
                            })
                        );
                        assert_eq!(c.decoded, None);
                        assert_eq!(c.poll(Tick(1)), Err(error));
                        assert!(matches!(c.finish(Tick(1)), Err(e) if e == error));
                        failed = true;
                        break;
                    }
                }
            }
            assert!(failed);
        }
    }
    #[test]
    fn maximal_word_turns_and_long_literal_stream_stay_bounded() {
        for (source, expected, maximal) in [
            (
                format!("attachment;filename=\"=?utf-8?Q?{}?=\"", "a".repeat(63)),
                63,
                true,
            ),
            (
                format!("attachment;filename=\"{}\"", "🐈".repeat(1024)),
                4096,
                false,
            ),
        ] {
            let mut original = work();
            let mut budget = HeaderBudget::new();
            let mut scratch = Scratch::new();
            let mut c = Cursor::new(
                source.as_bytes(),
                Kind::ContentDisposition,
                Attribute::Filename,
                &mut scratch,
                &mut original,
                &mut budget,
            )
            .unwrap();
            let mut produced = 0;
            let mut peak_visits = 0;
            let mut peak_steps = 0;
            loop {
                let before = (
                    c.work.remaining(),
                    c.budget.source_bytes_remaining(),
                    c.budget.steps_remaining(),
                );
                let event = c.poll(Tick(1)).unwrap();
                let visits = before.1 - c.budget.source_bytes_remaining();
                let steps = before.2 - c.budget.steps_remaining();
                assert!(visits <= 225);
                assert!(steps <= 457);
                assert!(before.0.records - c.work.remaining().records <= 30);
                peak_visits = peak_visits.max(visits);
                peak_steps = peak_steps.max(steps);
                match event {
                    Status::Yield => {}
                    Status::Scalar(ch) => produced += ch.len_utf8(),
                    Status::Complete(_) => break,
                }
            }
            assert_eq!(produced, expected);
            if maximal {
                assert_eq!(peak_visits, 225);
                assert_eq!(peak_steps, 455);
            }
        }
    }
    #[test]
    fn original_sources_normalize_and_fit_the_parser_reservation() {
        assert!(std::mem::size_of::<Source<'_>>() <= 1088);
        assert!(
            std::mem::size_of::<Cursor<'_, '_>>() + std::mem::size_of::<HeaderBudget>() <= 4608
        );
        assert!(
            std::mem::size_of::<Cursor<'_, '_>>()
                + 2 * std::mem::size_of::<Source<'_>>()
                + std::mem::size_of::<Display<'_>>()
                + std::mem::size_of::<Context<'_>>()
                + std::mem::size_of::<HeaderBudget>()
                <= 16 * 1024
        );

        for (source, want) in [
            (
                "attachment;filename=\"e\u{301}\"".to_owned(),
                "é".to_owned(),
            ),
            (
                "attachment;filename*=utf-8''e%CC%81".to_owned(),
                "é".to_owned(),
            ),
            (
                "attachment;filename*1*=%81;filename*0*=utf-8''e%CC".to_owned(),
                "é".to_owned(),
            ),
            (
                "attachment;filename=\"=?utf-8?Q?e?= =?utf-8?Q?=CC=81?=\"".to_owned(),
                "é".to_owned(),
            ),
            (
                format!(
                    "attachment;filename=\"a{}{}x\"",
                    "\u{301}".repeat(257),
                    "\u{327}".repeat(257)
                ),
                format!("á{}{}x", "\u{327}".repeat(257), "\u{301}".repeat(256)),
            ),
        ] {
            let mut work = Meter::new(
                Deadline::after(Tick(0), 100).unwrap(),
                Charge {
                    io_bytes: 100_000_000,
                    records: 100_000_000,
                    output_bytes: 100_000_000,
                    ..Charge::default()
                },
            );
            let mut budget = HeaderBudget::new();
            let mut scratch = Scratch::new();
            let mut cursor = Cursor::new(
                source.as_bytes(),
                Kind::ContentDisposition,
                Attribute::Filename,
                &mut scratch,
                &mut work,
                &mut budget,
            )
            .unwrap();
            let mut output = String::new();
            loop {
                match cursor.poll(Tick(1)).unwrap() {
                    Status::Yield => {}
                    Status::Scalar(value) => output.push(value),
                    Status::Complete(_) => break,
                }
            }
            assert_eq!(output, want);
            cursor.check_deadline(Tick(1)).unwrap();
            assert!(cursor.finish(Tick(1)).is_ok());
        }
    }
    #[test]
    fn overflow_replay_preserves_turn_bounds_original_cuts_and_eof_result() {
        let marks = "\u{301}".repeat(257);
        let forms = [
            (
                format!("attachment;filename=\"a{marks}\""),
                format!("á{}", "\u{301}".repeat(256)),
            ),
            (
                format!(
                    "attachment;filename*1*=%81{};filename*0*=utf-8'en'a%CC",
                    "%CC%81".repeat(256)
                ),
                format!("á{}", "\u{301}".repeat(256)),
            ),
            (
                format!(
                    "attachment;filename=\"=?utf-8?Q?a?= {}\"",
                    vec!["=?utf-8?Q?=CC=81?="; 257].join(" ")
                ),
                format!("á{}", "\u{301}".repeat(256)),
            ),
        ];
        for (source, want) in forms {
            let mut original = work();
            let mut budget = HeaderBudget::new();
            let mut scratch = Scratch::new();
            let mut c = Cursor::new(
                source.as_bytes(),
                Kind::ContentDisposition,
                Attribute::Filename,
                &mut scratch,
                &mut original,
                &mut budget,
            )
            .unwrap();
            let mut targets = Vec::new();
            let mut output = String::new();
            let mut completed = false;
            for turn in 0..100_000 {
                let inspection = c.engine.inspect().unwrap();
                let replay = inspection.overflow
                    && matches!(inspection.mode, td_nfc::Mode::Compute | td_nfc::Mode::Emit);
                let mode = inspection.mode;
                let before = (
                    c.work.remaining(),
                    c.budget.source_bytes_remaining(),
                    c.budget.steps_remaining(),
                );
                let event = c.poll(Tick(1)).unwrap();
                let costs = [
                    before.1 - c.budget.source_bytes_remaining(),
                    before.2 - c.budget.steps_remaining(),
                    before.0.io_bytes - c.work.remaining().io_bytes,
                    before.0.records - c.work.remaining().records,
                ];
                assert!(costs[0] <= 225 && costs[1] <= 457 && costs[2] <= 225 && costs[3] <= 30);
                if replay {
                    for (flavor, cost) in costs.into_iter().enumerate() {
                        if cost > 0
                            && !targets
                                .iter()
                                .any(|(f, m, _, _)| *f == flavor && *m == mode)
                        {
                            targets.push((flavor, mode, turn, cost));
                        }
                    }
                }
                match event {
                    Status::Scalar(value) => output.push(value),
                    Status::Complete(decoded) => {
                        assert!(!decoded.is_encoding_problem);
                        assert!(decoded.selection.plan.is_some());
                        completed = true;
                        break;
                    }
                    Status::Yield => {}
                }
            }
            assert!(completed);
            assert_eq!(output, want);
            assert_eq!(
                targets.len(),
                8,
                "both replay phases must consume all four allowances"
            );
            assert!(c.budget.source_bytes_remaining() > 0 && c.budget.steps_remaining() > 0);
            c.check_deadline(Tick(1)).unwrap();
            c.finish(Tick(1)).unwrap();
            for (flavor, _, turn, cost) in targets {
                for limit in 0..cost {
                    let mut original = work();
                    let mut budget = HeaderBudget::new();
                    let mut scratch = Scratch::new();
                    let mut c = Cursor::new(
                        source.as_bytes(),
                        Kind::ContentDisposition,
                        Attribute::Filename,
                        &mut scratch,
                        &mut original,
                        &mut budget,
                    )
                    .unwrap();
                    for _ in 0..turn {
                        assert!(!matches!(c.poll(Tick(1)).unwrap(), Status::Complete(_)));
                    }
                    if flavor < 2 {
                        let bytes = if flavor == 0 {
                            c.budget.source_bytes_remaining() - limit
                        } else {
                            0
                        };
                        let steps = if flavor == 1 {
                            c.budget.steps_remaining() - limit
                        } else {
                            0
                        };
                        c.budget
                            .charge_local(c.work, Tick(1), bytes, steps, &mut 0)
                            .unwrap();
                    } else {
                        let charge = if flavor == 2 {
                            Charge {
                                io_bytes: c.work.remaining().io_bytes - limit,
                                ..Charge::default()
                            }
                        } else {
                            Charge {
                                records: c.work.remaining().records - limit,
                                ..Charge::default()
                            }
                        };
                        c.work.charge(Tick(1), charge).unwrap();
                    }
                    let error = c.poll(Tick(1)).err().unwrap();
                    assert!(c.decoded.is_none());
                    let stopped = (
                        c.work.remaining(),
                        c.budget.source_bytes_remaining(),
                        c.budget.steps_remaining(),
                        c.credit,
                    );
                    assert_eq!(c.poll(Tick(1)), Err(error));
                    assert_eq!(c.check_deadline(Tick(1)), Err(error));
                    assert_eq!(c.charge_output(Tick(1), 0), Err(error));
                    assert_eq!(
                        (
                            c.work.remaining(),
                            c.budget.source_bytes_remaining(),
                            c.budget.steps_remaining(),
                            c.credit
                        ),
                        stopped
                    );
                    assert!(matches!(c.finish(Tick(1)), Err(e) if e == error));
                }
                let mut original = work();
                let mut budget = HeaderBudget::new();
                let mut scratch = Scratch::new();
                let mut c = Cursor::new(
                    source.as_bytes(),
                    Kind::ContentDisposition,
                    Attribute::Filename,
                    &mut scratch,
                    &mut original,
                    &mut budget,
                )
                .unwrap();
                for _ in 0..turn {
                    c.poll(Tick(1)).unwrap();
                }
                let error = c.poll(Tick(100)).err().unwrap();
                assert!(c.decoded.is_none());
                assert_eq!(c.poll(Tick(1)), Err(error));
                assert!(matches!(c.finish(Tick(1)), Err(e) if e == error));
            }
        }
    }
}

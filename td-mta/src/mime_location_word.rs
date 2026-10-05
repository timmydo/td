//! Caller-selected and placement-authorized URI encoded-word spellings.
//! This helper does not select words, trim CFWS or validate a Content-Location.
use crate::{
    admission::work::{Charge, Meter, Stop},
    decode_work::{Conversion, Error as InnerError, Lexical, Parsing, Work},
    encoded_word::{decode::Progress, Context, Descriptor, Word, MAX_TOKEN_OCTETS},
    nfc::HeaderBudget,
    ports::Tick,
};
use td_header::uri::{unfold, word_token};
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    MalformedFold,
    Work(Stop),
    InterpretationLimit,
    InvalidState,
}
impl From<InnerError> for Error {
    fn from(error: InnerError) -> Self {
        match error {
            InnerError::Work(stop) => Self::Work(stop),
            InnerError::InterpretationLimit => Self::InterpretationLimit,
            InnerError::InvalidState => Self::InvalidState,
        }
    }
}
impl From<unfold::Error<InnerError>> for Error {
    fn from(error: unfold::Error<InnerError>) -> Self {
        match error {
            unfold::Error::Malformed => Self::MalformedFold,
            unfold::Error::Work(error) => Self::from(error),
            unfold::Error::InvalidState => Self::InvalidState,
        }
    }
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MalformedFold => f.write_str("malformed URI word fold"),
            Self::Work(stop) => write!(f, "URI word work: {stop}"),
            Self::InterpretationLimit => f.write_str("URI word interpretation limit"),
            Self::InvalidState => f.write_str("invalid URI word state"),
        }
    }
}
impl std::error::Error for Error {}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    Yield,
    Scalar(char),
    Complete,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct End {
    /// False requests whole selected-spelling literal replay by the owner.
    pub recognized: bool,
    pub encoding_problem: bool,
}
#[derive(Clone, Copy)]
enum Phase {
    Wire,
    Recognize,
    Decode,
    RunFold,
    RunScan,
    RunRecognize,
    RunReplay,
    RunReplayRecognize,
    Complete,
}
/// Owns one fixed 75-octet logical token; immutable during decoding.
/// new reads one word; new_run validates and replays a complete word run.
/// Complete unfolding precedes recognition and every provisional scalar.
/// Unknown or oversized words complete unrecognized without scalar output.
/// The caller retains placement, whole-spelling fallback and publication.
/// ```compile_fail,E0277
/// fn copied<T: Copy>() {} copied::<td_mta::mime_location_word::Cursor<'_, '_>>();
/// ```
/// ```compile_fail,E0277
/// fn cloned<T: Clone>() {} cloned::<td_mta::mime_location_word::Cursor<'_, '_>>();
/// ```
pub struct Cursor<'a, 'w> {
    source: &'a [u8],
    wire: unfold::Cursor<'a, InnerError>,
    token: word_token::Token,
    run: bool,
    recognized: bool,
    encoding_problem: bool,
    words: u64,
    last_word_end: Option<usize>,
    rejected_run: bool,
    decoded_words: u64,
    scratch: [u8; MAX_TOKEN_OCTETS],
    length: u8,
    oversized: bool,
    descriptor: Option<Descriptor>,
    progress: Option<Progress>,
    phase: Phase,
    work: &'w mut Meter,
    budget: &'w mut HeaderBudget,
    credit: u8,
    failure: Option<Error>,
}
impl<'a, 'w> Cursor<'a, 'w> {
    pub const fn new(source: &'a [u8], work: &'w mut Meter, budget: &'w mut HeaderBudget) -> Self {
        Self {
            source,
            wire: unfold::Cursor::new(source),
            token: word_token::Token::new(),
            run: false,
            recognized: false,
            encoding_problem: false,
            words: 0,
            last_word_end: None,
            rejected_run: false,
            decoded_words: 0,
            scratch: [0; MAX_TOKEN_OCTETS],
            length: 0,
            oversized: false,
            descriptor: None,
            progress: None,
            phase: Phase::Wire,
            work,
            budget,
            credit: 0,
            failure: None,
        }
    }
    /// Supply a complete placement-authorized run of URI logical encoded words.
    /// Full unfolding and recognition of every word precede any scalar.
    /// Adjacent words require an original wire-whitespace gap.
    /// Unknown, oversized, touching or mixed spelling requests whole-run fallback.
    pub const fn new_run(
        source: &'a [u8],
        work: &'w mut Meter,
        budget: &'w mut HeaderBudget,
    ) -> Self {
        let mut cursor = Self::new(source, work, budget);
        cursor.run = true;
        cursor.phase = Phase::RunFold;
        cursor
    }
    pub fn end(&self) -> Option<End> {
        if self.failure.is_some() || !matches!(self.phase, Phase::Complete) {
            return None;
        }
        Some(End {
            recognized: self.recognized,
            encoding_problem: self.encoding_problem
                || self
                    .progress
                    .as_ref()
                    .is_some_and(Progress::is_encoding_problem),
        })
    }
    fn outcome<T>(&mut self, result: Result<T, Error>) -> Result<T, Error> {
        if let Err(error) = result {
            self.failure = Some(error);
        }
        result
    }
    pub fn check_deadline(&mut self, now: Tick) -> Result<(), Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = self
            .budget
            .charge(self.work, now, 0, 0, &mut self.credit)
            .map_err(InnerError::from)
            .map_err(Error::from);
        self.outcome(result)
    }
    /// Consuming discard only; the enclosing owner proves a syntax refusal and
    /// freshly admits these original budgets before returning or reusing them.
    pub(crate) fn discard(self) -> (&'w mut Meter, &'w mut HeaderBudget) {
        (self.work, self.budget)
    }
    #[cfg(test)]
    pub(crate) fn remaining(&self) -> (Charge, u64, u64) {
        (
            self.work.remaining(),
            self.budget.source_bytes_remaining(),
            self.budget.steps_remaining(),
        )
    }
    /// Crate-internal framing charge; the enclosing caller retires on refusal.
    pub(crate) fn charge_output(&mut self, now: Tick, bytes: u64) -> Result<(), Error> {
        self.check_deadline(now)?;
        let result = self
            .work
            .charge(
                now,
                Charge {
                    output_bytes: bytes,
                    ..Charge::default()
                },
            )
            .map_err(Error::Work);
        self.outcome(result)
    }
    pub fn poll(&mut self, now: Tick) -> Result<Status, Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if matches!(self.phase, Phase::Complete) {
            return Ok(Status::Complete);
        }
        self.check_deadline(now)?;
        let result = self.step(now);
        self.outcome(result)
    }
    fn step(&mut self, now: Tick) -> Result<Status, Error> {
        let mut parsing = Parsing::new(self.work, self.budget, &mut self.credit);
        match self.phase {
            Phase::RunFold => {
                if self.wire.poll(&mut Lexical::new(&mut parsing, now))? == unfold::Status::Complete
                {
                    self.wire = unfold::Cursor::new(self.source);
                    self.phase = Phase::RunScan;
                }
                Ok(Status::Yield)
            }
            Phase::Wire | Phase::RunScan | Phase::RunReplay => {
                match self.wire.poll(&mut Lexical::new(&mut parsing, now))? {
                    unfold::Status::Yield => {}
                    unfold::Status::Complete => match self.phase {
                        Phase::Wire => self.phase = Phase::Recognize,
                        Phase::RunScan => {
                            if self.rejected_run
                                || self.oversized
                                || self.length != 0
                                || self.words == 0
                            {
                                self.phase = Phase::Complete;
                                return Ok(Status::Complete);
                            }
                            self.recognized = true;
                            self.wire = unfold::Cursor::new(self.source);
                            self.phase = Phase::RunReplay;
                        }
                        Phase::RunReplay => {
                            if self.length != 0 || self.decoded_words != self.words {
                                return Err(Error::InvalidState);
                            }
                            self.phase = Phase::Complete;
                            return Ok(Status::Complete);
                        }
                        _ => return Err(Error::InvalidState),
                    },
                    unfold::Status::Octet { byte, position } => {
                        parsing.charge(
                            now,
                            Charge {
                                records: 1,
                                ..Charge::default()
                            },
                        )?;
                        if self.run
                            && self.length == 0
                            && self.words != 0
                            && matches!(self.phase, Phase::RunScan)
                            && self.last_word_end == Some(position)
                        {
                            self.rejected_run = true;
                        }
                        let framed = if self.run {
                            self.token.feed(byte)
                        } else {
                            word_token::Status::More
                        };
                        if let Some(slot) = self.scratch.get_mut(usize::from(self.length)) {
                            *slot = byte;
                            self.length = self.length.checked_add(1).ok_or(Error::InvalidState)?;
                        } else {
                            self.oversized = true;
                        }
                        if self.run && framed == word_token::Status::Rejected {
                            self.rejected_run = true;
                        }
                        if matches!(self.phase, Phase::RunReplay)
                            && (self.oversized || self.rejected_run)
                        {
                            return Err(Error::InvalidState);
                        }
                        if !self.oversized
                            && !self.rejected_run
                            && framed == word_token::Status::Complete
                        {
                            if matches!(self.phase, Phase::RunScan) {
                                self.last_word_end =
                                    Some(position.checked_add(1).ok_or(Error::InvalidState)?);
                            }
                            match self.phase {
                                Phase::RunScan => self.phase = Phase::RunRecognize,
                                Phase::RunReplay => self.phase = Phase::RunReplayRecognize,
                                _ => {}
                            }
                        }
                    }
                }
                Ok(Status::Yield)
            }
            Phase::Recognize | Phase::RunRecognize | Phase::RunReplayRecognize => {
                parsing.charge(
                    now,
                    Charge {
                        records: 1,
                        ..Charge::default()
                    },
                )?;
                let token = self
                    .scratch
                    .get(..usize::from(self.length))
                    .ok_or(Error::InvalidState)?;
                if !self.oversized && !self.rejected_run && (!self.run || self.token.is_complete())
                {
                    if let Some(word) =
                        Word::recognize_with_work(token, Context::Text, now, &mut parsing)?
                    {
                        if matches!(self.phase, Phase::RunRecognize) {
                            self.words = self.words.checked_add(1).ok_or(Error::InvalidState)?;
                            self.length = 0;
                            self.token = word_token::Token::new();
                            self.phase = Phase::RunScan;
                            return Ok(Status::Yield);
                        }
                        self.descriptor = Some(word.descriptor(token).ok_or(Error::InvalidState)?);
                        self.progress = Some(Progress::new(word));
                        self.recognized = true;
                        self.phase = Phase::Decode;
                        return Ok(Status::Yield);
                    }
                }
                if matches!(self.phase, Phase::RunReplayRecognize) {
                    return Err(Error::InvalidState);
                }
                if matches!(self.phase, Phase::RunRecognize) {
                    self.rejected_run = true;
                    self.phase = Phase::RunScan;
                    return Ok(Status::Yield);
                }
                self.phase = Phase::Complete;
                Ok(Status::Complete)
            }
            Phase::Decode => {
                let token = self
                    .scratch
                    .get(..usize::from(self.length))
                    .ok_or(Error::InvalidState)?;
                let word = self
                    .descriptor
                    .and_then(|descriptor| descriptor.resume(token))
                    .ok_or(Error::InvalidState)?;
                let progress = self.progress.as_mut().ok_or(Error::InvalidState)?;
                match progress.poll_with_work(word, now, &mut parsing)? {
                    crate::encoded_word::decode::Status::Yield => Ok(Status::Yield),
                    crate::encoded_word::decode::Status::Complete => {
                        self.encoding_problem |= progress.is_encoding_problem();
                        if self.run {
                            self.decoded_words = self
                                .decoded_words
                                .checked_add(1)
                                .ok_or(Error::InvalidState)?;
                            self.length = 0;
                            self.token = word_token::Token::new();
                            self.descriptor = None;
                            self.progress = None;
                            self.phase = Phase::RunReplay;
                            return Ok(Status::Yield);
                        }
                        self.phase = Phase::Complete;
                        Ok(Status::Complete)
                    }
                    crate::encoded_word::decode::Status::Scalar(value) => {
                        Conversion::new(self.work, self.budget, &mut self.credit).charge(
                            now,
                            Charge {
                                output_bytes: value.len_utf8() as u64,
                                ..Charge::default()
                            },
                        )?;
                        Ok(Status::Scalar(value))
                    }
                }
            }
            Phase::Complete => Err(Error::InvalidState),
        }
    }
    pub fn finish(
        mut self,
        now: Tick,
    ) -> Result<(&'w mut Meter, &'w mut HeaderBudget, End), Error> {
        self.check_deadline(now)?;
        let end = self.end().ok_or(Error::InvalidState)?;
        Ok((self.work, self.budget, end))
    }
}
const _: () =
    assert!(std::mem::size_of::<Cursor<'_, '_>>() + std::mem::size_of::<HeaderBudget>() <= 512);
const _: () = assert!(std::mem::size_of::<unfold::Cursor<'_, InnerError>>() <= 64);

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
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
    fn drain(mut cursor: Cursor<'_, '_>) -> (String, Result<End, Error>) {
        let mut output = String::new();
        for _ in 0..100_000 {
            let before = cursor.work.remaining();
            let steps = cursor.budget.steps_remaining();
            let status = cursor.poll(Tick(1));
            let after = cursor.work.remaining();
            assert!(before.io_bytes - after.io_bytes <= 225);
            assert!(before.records - after.records <= 29);
            assert!(steps - cursor.budget.steps_remaining() <= 452);
            assert!(before.output_bytes - after.output_bytes <= 4);
            match status {
                Ok(Status::Yield) => assert_eq!(cursor.end(), None),
                Ok(Status::Scalar(value)) => {
                    assert_eq!(cursor.end(), None);
                    output.push(value);
                }
                Ok(Status::Complete) => {
                    assert!(cursor.end().is_some());
                    let completed_steps = cursor.budget.steps_remaining();
                    assert_eq!(cursor.poll(Tick(100)), Ok(Status::Complete));
                    assert_eq!(cursor.work.remaining(), after);
                    assert_eq!(cursor.budget.steps_remaining(), completed_steps);
                    return (output, cursor.finish(Tick(1)).map(|(_, _, end)| end));
                }
                Err(error) => {
                    assert_eq!(cursor.end(), None);
                    assert_eq!(cursor.poll(Tick(1)), Err(error));
                    assert_eq!(cursor.check_deadline(Tick(1)), Err(error));
                    assert_eq!(cursor.work.remaining(), after);
                    return (output, Err(error));
                }
            }
        }
        panic!("selected URI word did not finish")
    }

    #[test]
    fn whole_runs_preserve_literals_diagnostics_and_per_word_ceiling() {
        for (source, wanted, problem) in [
            (
                b"=?utf-8?Q?e=CC=81?= =?ascii?Q?/a=20b?=".as_slice(),
                "e\u{301}/a b",
                false,
            ),
            (
                b"=?as\r\n cii?B?Zm 9v?=\r\n =?ascii?Q?/bar?=",
                "foo/bar",
                false,
            ),
            (b"=?utf-8?Q?=FF=00=EF=B7=90?= =?ascii?Q?ok?=", "��ok", true),
            (b"=?ascii*en?Q?a?=", "a", false),
        ] {
            let mut work = meter();
            let mut budget = HeaderBudget::new();
            let (actual, end) = drain(Cursor::new_run(source, &mut work, &mut budget));
            assert_eq!(actual, wanted);
            assert_eq!(
                end,
                Ok(End {
                    recognized: true,
                    encoding_problem: problem
                })
            );
            assert_eq!(
                100_000_000 - work.remaining().output_bytes,
                wanted.len() as u64
            );
        }
        let longest = format!("=?ascii?Q?{}?= =?ascii?Q?b?=", "a".repeat(63));
        let wanted = "a".repeat(63) + "b";
        assert_eq!(
            drain(Cursor::new_run(
                longest.as_bytes(),
                &mut meter(),
                &mut HeaderBudget::new()
            )),
            (
                wanted,
                Ok(End {
                    recognized: true,
                    encoding_problem: false
                })
            )
        );
        // Single-token behavior remains a distinct caller-selected scope.
        assert_eq!(
            drain(Cursor::new(
                b"=?ascii?Q?a?= =?ascii?Q?b?=",
                &mut meter(),
                &mut HeaderBudget::new()
            )),
            (
                String::new(),
                Ok(End {
                    recognized: false,
                    encoding_problem: false
                })
            )
        );
    }
    #[test]
    fn whole_run_refusal_and_fallback_precede_every_scalar() {
        let oversized = format!("=?ascii?Q?a?= =?ascii?Q?{}?=", "a".repeat(64));
        for source in [
            b"".as_slice(),
            b"=?ascii?Q?a?= =?unknown?Q?b?=",
            b"=?ascii?Q?a?=tail",
            b"=?ascii?Q?x?==?ascii?Q?x?=",
            b"=?ascii?Q?a?= =?ascii?Q?b?==?ascii?Q?c?=",
            b"prefix=?ascii?Q?a?=",
            b"=?ascii?Q?a?= =?ascii?Q??=",
            oversized.as_bytes(),
        ] {
            let mut work = meter();
            let mut budget = HeaderBudget::new();
            assert_eq!(
                drain(Cursor::new_run(source, &mut work, &mut budget)),
                (
                    String::new(),
                    Ok(End {
                        recognized: false,
                        encoding_problem: false
                    })
                )
            );
            assert_eq!(work.remaining().output_bytes, 100_000_000);
        }
        for source in [
            b"=?ascii?Q?a?= =?ascii?Q?b?=\r\n".as_slice(),
            b"=?ascii?Q?a?=\nX",
        ] {
            let mut work = meter();
            let mut budget = HeaderBudget::new();
            assert_eq!(
                drain(Cursor::new_run(source, &mut work, &mut budget)),
                (String::new(), Err(Error::MalformedFold))
            );
            assert_eq!(work.remaining().output_bytes, 100_000_000);
        }
    }
    #[test]
    fn every_run_resource_cut_and_exact_original_handoff() {
        let source = b"=?utf-8?Q?=C3=A9?=\r\n =?ascii?Q?/x?=";
        let mut work = meter();
        let initial = work.remaining();
        let mut budget = HeaderBudget::new();
        let expected = drain(Cursor::new_run(source, &mut work, &mut budget));
        assert!(expected.1.is_ok());
        let used = [
            HeaderBudget::new().source_bytes_remaining() - budget.source_bytes_remaining(),
            HeaderBudget::new().steps_remaining() - budget.steps_remaining(),
            initial.io_bytes - work.remaining().io_bytes,
            initial.records - work.remaining().records,
            initial.output_bytes - work.remaining().output_bytes,
        ];
        assert!(used.iter().all(|cost| *cost > 0));
        for (kind, cost) in used.into_iter().enumerate() {
            for cap in 0..cost {
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
                    let mut grants = work.remaining();
                    match kind {
                        2 => grants.io_bytes = cap,
                        3 => grants.records = cap,
                        4 => grants.output_bytes = cap,
                        _ => panic!("bad cut"),
                    };
                    work = Meter::new(Deadline::after(Tick(0), 100).unwrap(), grants);
                }
                let actual = drain(Cursor::new_run(source, &mut work, &mut budget));
                assert!(expected.0.starts_with(&actual.0));
                assert_eq!(
                    actual.1,
                    Err(if kind < 2 {
                        Error::InterpretationLimit
                    } else {
                        Error::Work(match kind {
                            2 => Stop::IoBytes,
                            3 => Stop::Records,
                            4 => Stop::OutputBytes,
                            _ => panic!("bad cut"),
                        })
                    })
                );
                assert_eq!(
                    work.stopped(),
                    if kind < 2 {
                        None
                    } else {
                        Some(match kind {
                            2 => Stop::IoBytes,
                            3 => Stop::Records,
                            4 => Stop::OutputBytes,
                            _ => panic!("bad cut"),
                        })
                    }
                );
                if kind < 2 {
                    assert_eq!(
                        Cursor::new_run(source, &mut work, &mut budget).check_deadline(Tick(1)),
                        Err(Error::InterpretationLimit)
                    );
                }
            }
        }
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
        assert_eq!(
            drain(Cursor::new_run(source, &mut work, &mut budget)),
            expected
        );
        assert_eq!(work.remaining(), Charge::default());
        assert_eq!(budget.source_bytes_remaining(), 0);
        assert_eq!(budget.steps_remaining(), 0);
    }
    #[test]
    fn every_run_deadline_cut_retires_and_restores_original_owners() {
        for (source, recognized) in [
            (b"=?ascii?Q?a?= =?ascii?Q?b?=".as_slice(), true),
            (b"=?ascii?Q?a?= =?unknown?Q?b?=", false),
            (b"=?ascii?Q?a?= tail", false),
        ] {
            let mut work = meter();
            let mut budget = HeaderBudget::new();
            let identity = (std::ptr::from_ref(&work), std::ptr::from_ref(&budget));
            let mut cursor = Cursor::new_run(source, &mut work, &mut budget);
            let mut turns = 0;
            loop {
                turns += 1;
                if cursor.poll(Tick(1)).unwrap() == Status::Complete {
                    break;
                }
            }
            let (work, budget, end) = cursor.finish(Tick(1)).unwrap();
            assert_eq!(end.recognized, recognized);
            assert_eq!(
                (std::ptr::from_ref(&*work), std::ptr::from_ref(&*budget)),
                identity
            );
            let mut next = crate::mime_language::Cursor::new(b"fr", work, budget);
            for _ in 0..1000 {
                if next.poll(Tick(1)).unwrap() == crate::mime_language::Status::Complete {
                    break;
                }
            }
            let (work, budget) = next.finish(Tick(1)).unwrap();
            assert_eq!(
                (std::ptr::from_ref(&*work), std::ptr::from_ref(&*budget)),
                identity
            );
            for cut in 0..=turns {
                for trial in 0..3 {
                    let mut work = meter();
                    let mut budget = HeaderBudget::new();
                    let mut cursor = Cursor::new_run(source, &mut work, &mut budget);
                    for _ in 0..cut {
                        cursor.poll(Tick(1)).unwrap();
                    }
                    if trial == 0 {
                        assert_eq!(
                            cursor.check_deadline(Tick(100)),
                            Err(Error::Work(Stop::Deadline))
                        );
                        assert_eq!(cursor.end(), None);
                        assert_eq!(cursor.poll(Tick(1)), Err(Error::Work(Stop::Deadline)));
                    } else if trial == 1 {
                        assert_eq!(
                            cursor.finish(Tick(100)).err(),
                            Some(Error::Work(Stop::Deadline))
                        );
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
    fn folded_words_move_and_keep_whole_token_semantics() {
        for (source, wanted, problem) in [
            (b"=?utf-8?Q?e=CC=81?=".as_slice(), "e\u{301}", false),
            (b"=?utf-8?Q?e=CC\r\n =81?=", "e\u{301}", false),
            (b"=?as\n\tcii*en?B?Zm 9v?=", "foo", false),
            (b"=?utf-8?Q?=FF=00=EF=B7=90?=", "��", true),
            (b"=?ascii?B?Zh==?=", "�", true),
            (b"=?ascii?Q?(x)\"y?=", "(x)\"y", false),
        ] {
            let mut work = meter();
            let mut budget = HeaderBudget::new();
            let (actual, end) = drain(Cursor::new(source, &mut work, &mut budget));
            assert_eq!(actual, wanted);
            assert_eq!(
                end,
                Ok(End {
                    recognized: true,
                    encoding_problem: problem
                })
            );
            assert_eq!(
                100_000_000 - work.remaining().output_bytes,
                wanted.len() as u64
            );
        }
        let oversized = format!("=?ascii?Q?{}?=", "a".repeat(100));
        let prefix = format!("=?ascii?Q?{}?=extra", "a".repeat(63));
        for source in [
            b"".as_slice(),
            b"=?unknown?Q?a?=",
            b"=?ascii?Q?a?=x",
            oversized.as_bytes(),
            prefix.as_bytes(),
        ] {
            assert_eq!(
                drain(Cursor::new(source, &mut meter(), &mut HeaderBudget::new())),
                (
                    String::new(),
                    Ok(End {
                        recognized: false,
                        encoding_problem: false
                    })
                )
            );
        }
        let mut bad_tail = oversized.into_bytes();
        bad_tail.extend_from_slice(b"\r\n");
        for source in [b"=?ascii?Q?a?=\nX".as_slice(), bad_tail.as_slice()] {
            assert_eq!(
                drain(Cursor::new(source, &mut meter(), &mut HeaderBudget::new())),
                (String::new(), Err(Error::MalformedFold))
            );
        }
    }
    #[test]
    fn exact_logical_boundary_and_maximum_recognition_costs() {
        let source = format!("=?ascii?Q?{}\r\n     {}?=", "a".repeat(31), "a".repeat(32));
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut cursor = Cursor::new(source.as_bytes(), &mut work, &mut budget);
        let mut output = String::new();
        let mut recognized_turn = false;
        let mut complete = false;
        for _ in 0..1000 {
            let recognition = matches!(cursor.phase, Phase::Recognize);
            let before = cursor.work.remaining();
            let steps = cursor.budget.steps_remaining();
            let status = cursor.poll(Tick(1)).unwrap();
            if recognition {
                recognized_turn = true;
                assert_eq!(before.io_bytes - cursor.work.remaining().io_bytes, 225);
                assert_eq!(steps - cursor.budget.steps_remaining(), 452);
                assert_eq!(before.records - cursor.work.remaining().records, 29);
            }
            match status {
                Status::Scalar(value) => output.push(value),
                Status::Complete => {
                    complete = true;
                    break;
                }
                Status::Yield => {}
            }
        }
        assert!(complete && recognized_turn);
        assert_eq!(output, "a".repeat(63));
        assert!(cursor.finish(Tick(1)).unwrap().2.recognized);
        let too_long = format!("=?ascii?Q?{}?=", "a".repeat(64));
        assert_eq!(too_long.len(), 76);
        assert_eq!(
            drain(Cursor::new(
                too_long.as_bytes(),
                &mut meter(),
                &mut HeaderBudget::new()
            )),
            (
                String::new(),
                Ok(End {
                    recognized: false,
                    encoding_problem: false
                })
            )
        );
    }
    #[test]
    fn decoding_survives_scratch_relocation_on_every_turn() {
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut left = Some(Cursor::new(
            b"=?ascii*en?B?Zm9vYmFy?=",
            &mut work,
            &mut budget,
        ));
        let mut right = None;
        let mut output = String::new();
        let mut complete = false;
        for _ in 0..1000 {
            let cursor = left.as_mut().unwrap();
            let left_ptr = cursor.scratch.as_ptr();
            let status = cursor.poll(Tick(1)).unwrap();
            right = left.take();
            assert_ne!(right.as_ref().unwrap().scratch.as_ptr(), left_ptr);
            match status {
                Status::Scalar(value) => output.push(value),
                Status::Complete => {
                    complete = true;
                    break;
                }
                Status::Yield => {}
            }
            let right_ptr = right.as_ref().unwrap().scratch.as_ptr();
            let status = right.as_mut().unwrap().poll(Tick(1)).unwrap();
            left = right.take();
            assert_ne!(left.as_ref().unwrap().scratch.as_ptr(), right_ptr);
            match status {
                Status::Scalar(value) => output.push(value),
                Status::Complete => {
                    complete = true;
                    break;
                }
                Status::Yield => {}
            }
        }
        assert!(complete);
        assert_eq!(output, "foobar");
        let cursor = left.or(right).unwrap();
        assert!(cursor.finish(Tick(1)).unwrap().2.recognized);
    }
    #[test]
    fn run_decoding_survives_every_turn_scratch_relocation() {
        let source = b"=?utf-8?Q?e=CC=81?= \r\n =?ascii*en?B?Zm9vYmFy?= =?utf-8?Q?=FF?=";
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let expected = drain(Cursor::new_run(source, &mut work, &mut budget));
        let expected_work = work.remaining();
        let expected_header = (budget.source_bytes_remaining(), budget.steps_remaining());
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut left = Some(Cursor::new_run(source, &mut work, &mut budget));
        let mut right = None;
        let mut output = String::new();
        let mut complete = false;
        for _ in 0..10_000 {
            let cursor = left.as_mut().unwrap();
            let pointer = cursor.scratch.as_ptr();
            let status = cursor.poll(Tick(1)).unwrap();
            right = left.take();
            assert_ne!(right.as_ref().unwrap().scratch.as_ptr(), pointer);
            match status {
                Status::Scalar(value) => output.push(value),
                Status::Complete => {
                    complete = true;
                    break;
                }
                Status::Yield => {}
            }
            let pointer = right.as_ref().unwrap().scratch.as_ptr();
            let status = right.as_mut().unwrap().poll(Tick(1)).unwrap();
            left = right.take();
            assert_ne!(left.as_ref().unwrap().scratch.as_ptr(), pointer);
            match status {
                Status::Scalar(value) => output.push(value),
                Status::Complete => {
                    complete = true;
                    break;
                }
                Status::Yield => {}
            }
        }
        assert!(complete);
        let (work, budget, end) = left.or(right).unwrap().finish(Tick(1)).unwrap();
        assert_eq!((output, Ok(end)), expected);
        assert_eq!(work.remaining(), expected_work);
        assert_eq!(
            (budget.source_bytes_remaining(), budget.steps_remaining()),
            expected_header
        );
    }
    #[test]
    fn every_budget_cut_retires_scalars_and_original_owners() {
        let maximum = format!("=?ascii?Q?{}\r\n     {}?=", "a".repeat(31), "a".repeat(32));
        let oversized = format!("=?ascii?Q?{}?=extra", "a".repeat(63));
        fn cursor<'a, 'w>(
            run: bool,
            source: &'a [u8],
            work: &'w mut Meter,
            budget: &'w mut HeaderBudget,
        ) -> Cursor<'a, 'w> {
            if run {
                Cursor::new_run(source, work, budget)
            } else {
                Cursor::new(source, work, budget)
            }
        }
        for run in [false, true] {
            for source in [
                b"=?utf-8?Q?=C3\r\n =A9xy?=".as_slice(),
                b"=?ascii?B?Zm9v?=",
                b"=?unknown?Q?a?=",
                b"=?ascii?Q?a?=x",
                b"=?ascii?Q?x?==?ascii?Q?x?=",
                maximum.as_bytes(),
                oversized.as_bytes(),
            ] {
                let mut work = meter();
                let mut budget = HeaderBudget::new();
                let expected = drain(cursor(run, source, &mut work, &mut budget));
                assert!(expected.1.is_ok());
                let visits = 100_000_000 - work.remaining().io_bytes;
                let total_records = 100_000_000 - work.remaining().records;
                let total_output = 100_000_000 - work.remaining().output_bytes;
                let steps = HeaderBudget::new().steps_remaining() - budget.steps_remaining();
                for (bytes, records, output, stop) in (0..visits)
                    .map(|cut| (cut, total_records, total_output, Stop::IoBytes))
                    .chain((0..total_records).map(|cut| (visits, cut, total_output, Stop::Records)))
                    .chain(
                        (0..total_output)
                            .map(|cut| (visits, total_records, cut, Stop::OutputBytes)),
                    )
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
                    let actual = drain(cursor(run, source, &mut work, &mut budget));
                    assert!(expected.0.starts_with(&actual.0));
                    assert_eq!(actual.1, Err(Error::Work(stop)));
                }
                for (bytes, remaining) in (0..visits)
                    .map(|cut| (cut, steps))
                    .chain((0..steps).map(|cut| (visits, cut)))
                {
                    let mut budget = HeaderBudget::new();
                    budget
                        .charge(
                            &mut meter(),
                            Tick(1),
                            budget.source_bytes_remaining() - bytes,
                            budget.steps_remaining() - remaining,
                            &mut 0,
                        )
                        .unwrap();
                    let mut work = meter();
                    let actual = drain(cursor(run, source, &mut work, &mut budget));
                    assert!(expected.0.starts_with(&actual.0));
                    assert_eq!(actual.1, Err(Error::InterpretationLimit));
                    assert_eq!(
                        cursor(run, source, &mut work, &mut budget).poll(Tick(1)),
                        Err(Error::InterpretationLimit)
                    );
                }
                let mut work = Meter::new(
                    Deadline::after(Tick(0), 100).unwrap(),
                    Charge {
                        io_bytes: visits,
                        records: total_records,
                        output_bytes: total_output,
                        ..Charge::default()
                    },
                );
                assert_eq!(
                    drain(cursor(run, source, &mut work, &mut HeaderBudget::new())),
                    expected
                );
                assert_eq!(work.remaining().io_bytes, 0);
                assert_eq!(work.remaining().records, 0);
                assert_eq!(work.remaining().output_bytes, 0);
            }
        }
    }
    #[test]
    fn late_original_admission_and_consuming_handoff() {
        let source = b"=?ascii?Q?ab?=";
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let work_ptr = &work as *const Meter;
        let budget_ptr = &budget as *const HeaderBudget;
        let mut cursor = Cursor::new(source, &mut work, &mut budget);
        let mut turns = 0;
        loop {
            turns += 1;
            if cursor.poll(Tick(1)).unwrap() == Status::Complete {
                break;
            }
        }
        let (work, budget, end) = cursor.finish(Tick(1)).unwrap();
        assert!(std::ptr::eq(work, work_ptr));
        assert!(std::ptr::eq(budget, budget_ptr));
        assert!(end.recognized);
        for cut in 0..turns {
            let mut work = meter();
            let mut budget = HeaderBudget::new();
            let mut cursor = Cursor::new(source, &mut work, &mut budget);
            for _ in 0..cut {
                cursor.poll(Tick(1)).unwrap();
            }
            assert_eq!(cursor.poll(Tick(100)), Err(Error::Work(Stop::Deadline)));
            assert_eq!(cursor.end(), None);
            assert_eq!(cursor.poll(Tick(1)), Err(Error::Work(Stop::Deadline)));
        }
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut cursor = Cursor::new(source, &mut work, &mut budget);
        loop {
            if cursor.poll(Tick(1)).unwrap() == Status::Complete {
                break;
            }
        }
        assert_eq!(
            cursor.check_deadline(Tick(100)),
            Err(Error::Work(Stop::Deadline))
        );
        assert_eq!(cursor.end(), None);
        assert_eq!(
            cursor.finish(Tick(1)).err(),
            Some(Error::Work(Stop::Deadline))
        );
        assert_eq!(
            Cursor::new(source, &mut meter(), &mut HeaderBudget::new())
                .finish(Tick(1))
                .err(),
            Some(Error::InvalidState)
        );
    }
}

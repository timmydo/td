//! Header Text scalars before NFC; field/form authorization is external.
pub use crate::encoded_word::decode::Status;
use crate::{
    admission::work::{Charge, Meter, Stop},
    decode_work::Work,
    encoded_word::{self, Context, Word},
    mime_charset::{Charset, Decoder, Status as Decoded},
    ports::Tick,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Work(Stop),
    InterpretationLimit,
    InvalidState,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Work(error) => write!(f, "header text work: {error}"),
            Self::InvalidState => f.write_str("invalid header text state"),
            Self::InterpretationLimit => f.write_str("header interpretation limit"),
        }
    }
}
impl std::error::Error for Error {}
impl From<crate::decode_work::Error> for Error {
    fn from(error: crate::decode_work::Error) -> Self {
        match error {
            crate::decode_work::Error::Work(stop) => Self::Work(stop),
            crate::decode_work::Error::InterpretationLimit => Self::InterpretationLimit,
            crate::decode_work::Error::InvalidState => Self::InvalidState,
        }
    }
}
impl From<crate::mime_charset::Error> for Error {
    fn from(error: crate::mime_charset::Error) -> Self {
        match error {
            crate::mime_charset::Error::Work(stop) => Self::Work(stop),
            crate::mime_charset::Error::InvalidState => Self::InvalidState,
        }
    }
}
impl From<crate::encoded_word::decode::Error> for Error {
    fn from(error: crate::encoded_word::decode::Error) -> Self {
        match error {
            crate::encoded_word::decode::Error::Work(stop) => Self::Work(stop),
            crate::encoded_word::decode::Error::InvalidState => Self::InvalidState,
        }
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum Grammar {
    Text,
    Keywords,
    ListId,
}
#[derive(Clone, Copy, Eq, PartialEq)]
struct Placement {
    grammar: Grammar,
    depth: u8,
    quoted: bool,
    escaped: bool,
    identifier: bool,
}
impl Placement {
    const fn new(grammar: Grammar) -> Self {
        Self {
            grammar,
            depth: 0,
            quoted: false,
            escaped: false,
            identifier: false,
        }
    }
    fn context(self) -> Option<Context> {
        if self.grammar == Grammar::Text {
            Some(Context::Text)
        } else if self.quoted || self.escaped || self.identifier {
            None
        } else if self.depth != 0 {
            Some(Context::Comment)
        } else {
            Some(Context::Phrase)
        }
    }
    fn delimiter(self, byte: u8) -> bool {
        match self.context() {
            Some(Context::Comment) => matches!(byte, b'(' | b')' | b'\\'),
            Some(Context::Phrase) => b"()<>[]:;@\\,\".".contains(&byte),
            _ => false,
        }
    }
    fn consume(&mut self, byte: u8, scalar_complete: bool) -> Result<bool, Error> {
        if self.grammar == Grammar::Text || self.identifier {
            return Ok(false);
        }
        if self.escaped {
            if scalar_complete {
                self.escaped = false;
                return Ok(self.depth != 0);
            }
            return Ok(false);
        }
        if byte == b'\\' && (self.quoted || self.depth != 0) {
            self.escaped = true;
            return Ok(false);
        }
        if self.depth != 0 {
            match byte {
                b'(' => {
                    if self.depth == 32 {
                        return Err(Error::InterpretationLimit);
                    }
                    self.depth += 1;
                }
                b')' => self.depth -= 1,
                _ => {}
            }
            // Leaving a comment still requires actual LWS for phrase words.
            return Ok(matches!(byte, b'(' | b')'));
        }
        if byte == b'"' {
            self.quoted = !self.quoted;
        } else if !self.quoted && byte == b'(' {
            self.depth = 1;
            return Ok(true);
        } else if !self.quoted && byte == b'<' && self.grammar == Grammar::ListId {
            // No phrase follows the list identifier, including malformed tails.
            self.identifier = true;
        }
        Ok(false)
    }
}

#[derive(Clone, Copy)]
enum Phase {
    Leading,
    Boundary,
    Literal,
    Gap,
    Candidate,
    Recognize,
    EmitGap,
    Word,
    Finish,
    Complete,
}
/// Copies retain the immutable source and decoder state, never work counters.
#[derive(Clone, Copy)]
pub struct Cursor<'a> {
    source: &'a [u8],
    placement: Placement,
    turn: u64,
    position: usize,
    scan: usize,
    token_start: usize,
    gap_end: usize,
    phase: Phase,
    literal: Decoder,
    word: Option<encoded_word::decode::Cursor<'a>>,
    problem: bool,
    failure: Option<Error>,
}
impl<'a> Cursor<'a> {
    /// Supply one authorized unstructured field value, without its final ending.
    pub const fn new(source: &'a [u8]) -> Self {
        Self::with_grammar(source, Grammar::Text)
    }
    pub(crate) const fn with_grammar(source: &'a [u8], grammar: Grammar) -> Self {
        Self {
            source,
            placement: Placement::new(grammar),
            turn: 0,
            position: 0,
            scan: 0,
            token_start: 0,
            gap_end: 0,
            phase: Phase::Leading,
            literal: Decoder::new(Charset::Utf8),
            word: None,
            problem: false,
            failure: None,
        }
    }
    pub const fn is_encoding_problem(&self) -> bool {
        self.problem
            || self.literal.is_encoding_problem()
            || match &self.word {
                Some(word) => word.is_encoding_problem(),
                None => false,
            }
    }
    // Immutable deterministic input makes the turn ordinal an O(1) exact
    // checkpoint identity. Failed cursors are retired before comparison.
    pub(crate) fn at(&self, other: &Self) -> bool {
        std::ptr::eq(self.source, other.source)
            && self.turn == other.turn
            && self.placement.grammar == other.placement.grammar
    }
    pub fn poll(&mut self, now: Tick, work: &mut Meter) -> Result<Status, Error> {
        self.poll_with_work(now, work)
    }
    pub(crate) fn poll_with_work(
        &mut self,
        now: Tick,
        work: &mut impl Work,
    ) -> Result<Status, Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if matches!(self.phase, Phase::Complete) {
            return Ok(Status::Complete);
        }
        let result = self.step(now, work).and_then(|status| {
            self.turn = self.turn.checked_add(1).ok_or(Error::InvalidState)?;
            Ok(status)
        });
        if let Err(error) = result {
            self.failure = Some(error);
        }
        result
    }
    fn byte(&self, at: usize, now: Tick, work: &mut impl Work) -> Result<Option<u8>, Error> {
        if at > self.source.len() {
            return Err(Error::InvalidState);
        }
        work.charge(
            now,
            Charge {
                io_bytes: u64::from(at < self.source.len()),
                ..Charge::default()
            },
        )
        .map_err(Error::from)?;
        Ok(self.source.get(at).copied())
    }
    // Fold removal preserves the following whitespace. Return that octet
    // and its complete raw extent so checkpoints need no unfolding buffer.
    fn atom(
        &self,
        at: usize,
        now: Tick,
        work: &mut impl Work,
    ) -> Result<Option<(u8, usize)>, Error> {
        let Some(byte) = self.byte(at, now, work)? else {
            return Ok(None);
        };
        let next = at.checked_add(1).ok_or(Error::InvalidState)?;
        let after_line = match byte {
            b'\r' if self.byte(next, now, work)? == Some(b'\n') => {
                Some(next.checked_add(1).ok_or(Error::InvalidState)?)
            }
            b'\n' => Some(next),
            _ => None,
        };
        if let Some(after_line) = after_line {
            if let Some(space @ (b' ' | b'\t')) = self.byte(after_line, now, work)? {
                return Ok(Some((
                    space,
                    after_line.checked_add(1).ok_or(Error::InvalidState)?,
                )));
            }
        }
        Ok(Some((byte, next)))
    }
    fn begin_candidate(&mut self, at: usize) {
        self.token_start = at;
        self.scan = at;
        self.phase = Phase::Candidate;
    }
    fn reject_candidate(&mut self) {
        self.gap_end = self.token_start;
        self.phase = if self.position < self.gap_end {
            Phase::EmitGap
        } else {
            Phase::Literal
        };
    }
    fn literal_byte(
        &mut self,
        byte: u8,
        next: usize,
        now: Tick,
        work: &mut impl Work,
    ) -> Result<Status, Error> {
        let decoded = self.literal.poll_with_work(&[byte], false, now, work)?;
        if decoded.consumed == 1 {
            if self.placement.grammar != Grammar::Text {
                work.charge(
                    now,
                    Charge {
                        records: 1,
                        ..Charge::default()
                    },
                )?;
            }
            let boundary = self
                .placement
                .consume(byte, matches!(decoded.status, Decoded::Scalar(_)))?;
            self.position = next;
            if !matches!(self.phase, Phase::EmitGap) {
                self.phase = if boundary || matches!(byte, b' ' | b'\t') {
                    Phase::Boundary
                } else {
                    Phase::Literal
                };
            }
        } else if decoded.consumed != 0 {
            return Err(Error::InvalidState);
        } else if self.placement.escaped && matches!(decoded.status, Decoded::Scalar(_)) {
            // A malformed escaped prefix emits replacement before revisiting
            // its unconsumed lookahead, which may begin an encoded word.
            work.charge(
                now,
                Charge {
                    records: 1,
                    ..Charge::default()
                },
            )?;
            self.placement.escaped = false;
            if self.placement.depth != 0 {
                self.phase = Phase::Boundary;
            }
        }
        self.literal_status(decoded.status)
    }
    fn literal_status(&mut self, status: Decoded) -> Result<Status, Error> {
        self.problem |= self.literal.is_encoding_problem();
        match status {
            Decoded::NeedInput => Ok(Status::Yield),
            Decoded::Complete if matches!(self.phase, Phase::Finish) => {
                self.phase = Phase::Complete;
                Ok(Status::Complete)
            }
            Decoded::Complete => Err(Error::InvalidState),
            Decoded::Scalar('\0') => Ok(Status::Yield),
            Decoded::Scalar(value) => {
                let code = u32::from(value);
                if matches!(code, 0xfdd0..=0xfdef) || code & 0xffff >= 0xfffe {
                    self.problem = true;
                    Ok(Status::Scalar('\u{fffd}'))
                } else {
                    Ok(Status::Scalar(value))
                }
            }
        }
    }
    fn step(&mut self, now: Tick, work: &mut impl Work) -> Result<Status, Error> {
        work.charge(now, Charge::default()).map_err(Error::from)?;
        match self.phase {
            Phase::Leading => {
                match self.atom(self.position, now, work)? {
                    Some((b' ', next)) => self.position = next,
                    _ => self.phase = Phase::Boundary,
                }
                Ok(Status::Yield)
            }
            Phase::Boundary => {
                match self.atom(self.position, now, work)? {
                    Some((b'=', _)) if self.placement.context().is_some() => {
                        self.begin_candidate(self.position)
                    }
                    Some((byte, next)) => return self.literal_byte(byte, next, now, work),
                    None => self.phase = Phase::Finish,
                }
                Ok(Status::Yield)
            }
            Phase::Literal | Phase::EmitGap => {
                if matches!(self.phase, Phase::EmitGap) && self.position == self.gap_end {
                    self.phase = Phase::Literal;
                    return Ok(Status::Yield);
                }
                match self.atom(self.position, now, work)? {
                    Some((byte, next)) => self.literal_byte(byte, next, now, work),
                    None => {
                        self.phase = Phase::Finish;
                        Ok(Status::Yield)
                    }
                }
            }
            Phase::Gap => {
                match self.atom(self.scan, now, work)? {
                    Some((b' ' | b'\t', next)) => self.scan = next,
                    Some((b'=', _)) if self.placement.context().is_some() => {
                        self.begin_candidate(self.scan)
                    }
                    _ => {
                        self.token_start = self.scan;
                        self.reject_candidate();
                    }
                }
                Ok(Status::Yield)
            }
            Phase::Candidate => {
                match self.atom(self.scan, now, work)? {
                    Some((b' ' | b'\t', _)) | None => self.phase = Phase::Recognize,
                    Some((byte, _)) if self.placement.delimiter(byte) => {
                        self.phase = Phase::Recognize
                    }
                    Some((_, next)) => {
                        self.scan = next;
                        if self
                            .scan
                            .checked_sub(self.token_start)
                            .ok_or(Error::InvalidState)?
                            > 75
                        {
                            self.reject_candidate();
                        }
                    }
                }
                Ok(Status::Yield)
            }
            Phase::Recognize => {
                let token = self
                    .source
                    .get(self.token_start..self.scan)
                    .ok_or(Error::InvalidState)?;
                let context = self.placement.context().ok_or(Error::InvalidState)?;
                if context == Context::Phrase {
                    if let Some(previous) = self.token_start.checked_sub(1) {
                        if !matches!(self.byte(previous, now, work)?, Some(b' ' | b'\t')) {
                            self.reject_candidate();
                            return Ok(Status::Yield);
                        }
                    }
                }
                // Phrase encoded words need actual LWS on their right; a comma
                // or quote is a token boundary but cannot supply that spacing.
                let right = if context == Context::Phrase {
                    self.atom(self.scan, now, work)?
                } else {
                    None
                };
                if context == Context::Phrase
                    && right.is_some_and(|(byte, _)| !matches!(byte, b' ' | b'\t'))
                {
                    self.reject_candidate();
                    return Ok(Status::Yield);
                }
                match Word::recognize_with_work(token, context, now, work).map_err(Error::from)? {
                    Some(word) => {
                        self.word = Some(encoded_word::decode::Cursor::new(word));
                        self.position = self.scan;
                        self.phase = Phase::Word;
                    }
                    None => self.reject_candidate(),
                }
                Ok(Status::Yield)
            }
            Phase::Word => {
                let word = self.word.as_mut().ok_or(Error::InvalidState)?;
                let status = word.poll_with_work(now, work)?;
                self.problem |= word.is_encoding_problem();
                if status == Status::Complete {
                    self.word = None;
                    self.scan = self.position;
                    self.phase = Phase::Gap;
                    Ok(Status::Yield)
                } else {
                    Ok(status)
                }
            }
            Phase::Finish => {
                let decoded = self.literal.poll_with_work(&[], true, now, work)?;
                self.literal_status(decoded.status)
            }
            Phase::Complete => Ok(Status::Complete),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]
    use super::*;
    use crate::ports::Deadline;
    fn work() -> Meter {
        Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: 16 * 1024 * 1024,
                records: 2_000_000,
                ..Charge::default()
            },
        )
    }
    fn text(input: &[u8]) -> (String, bool) {
        let mut cursor = Cursor::new(input);
        assert!(std::mem::size_of_val(&cursor) <= 208);
        let mut work = work();
        let mut output = String::new();
        for _ in 0..10000 {
            let before = work.remaining();
            let status = cursor.poll(Tick(1), &mut work).unwrap();
            assert!(before.records - work.remaining().records <= 226);
            assert!(before.io_bytes - work.remaining().io_bytes <= 225);
            match status {
                Status::Scalar(value) => output.push(value),
                Status::Yield => {}
                Status::Complete => {
                    return (output, cursor.is_encoding_problem());
                }
            }
        }
        panic!("header did not finish");
    }
    #[test]
    fn unfolding_trims_only_initial_spaces_and_preserves_literal_text() {
        for (input, expected, problem) in [
            (b"".as_slice(), "", false),
            (b"   hello  ", "hello  ", false),
            (b" \t hello\r\n world\n\t!", "\t hello world\t!", false),
            (b"\r\n  hello", "hello", false),
            (b"a\r\nb\rc\n", "a\r\nb\rc\n", false),
            (b"a\0b\xe1\0\x80", "ab��", true),
            (b"e\xcc\x81", "e\u{301}", false),
            (b"\xef\xbf\xbf", "�", true),
            (b"\xc2\x85", "\u{85}", false),
        ] {
            assert_eq!(text(input), (expected.to_owned(), problem), "{input:?}");
        }
    }
    #[test]
    fn words_require_whole_tokens_and_adjacent_gap_suppression_is_conditional() {
        for (input, expected, problem) in [
            (
                b"=?utf-8?Q?one?= \r\n\t=?utf-8?B?dHdv?=".as_slice(),
                "onetwo",
                false,
            ),
            (b"=?utf-8?Q?one?= \r\n\ttext", "one \ttext", false),
            (b"=?utf-8?Q?one?=\r\n =?utf-8?Q?two?=", "onetwo", false),
            (b"=?utf-8?Q?one?=\r\n x", "one x", false),
            (b"=?utf-8?Q?one?=\n\t=?utf-8?Q?two?=", "onetwo", false),
            (b"=?utf-8?Q?one?= \r\n\t", "one \t", false),
            (
                b"=?utf-8?Q?one?=  =?unknown?Q?two?=",
                "one  =?unknown?Q?two?=",
                false,
            ),
            (b"a=?utf-8?Q?one?=", "a=?utf-8?Q?one?=", false),
            (b"=?utf-8?Q?one?=x", "=?utf-8?Q?one?=x", false),
            (
                b"=?utf-8?Q?one?==?utf-8?Q?two?=",
                "=?utf-8?Q?one?==?utf-8?Q?two?=",
                false,
            ),
            (b"=?utf-8?Q?=E2=82?= =?utf-8?Q?=AC?=", "��", true),
            (b"=?utf-8?Q?a=QZ?= =?utf-8?Q?b?=", "a�QZb", true),
            (b"=?utf-8?Q?=00?=  x", "  x", false),
        ] {
            assert_eq!(text(input), (expected.to_owned(), problem), "{input:?}");
        }
    }
    #[test]
    fn long_false_tokens_and_whitespace_replay_stay_bounded() {
        let long = format!("=?utf-8?Q?{}?=", "a".repeat(1000));
        assert_eq!(text(long.as_bytes()), (long.clone(), false));
        let gap = format!("=?utf-8?Q?a?={}b", " \r\n\t".repeat(300));
        assert_eq!(
            text(gap.as_bytes()),
            (format!("a{}b", " \t".repeat(300)), false)
        );
        let source = vec![b'x'; 1024 * 1024];
        let mut cursor = Cursor::new(&source);
        let mut work = work();
        let mut count = 0;
        let mut complete = false;
        for _ in 0..source.len() + 10 {
            match cursor.poll(Tick(1), &mut work).unwrap() {
                Status::Scalar('x') => count += 1,
                Status::Scalar(_) => panic!("unexpected scalar"),
                Status::Yield => {}
                Status::Complete => {
                    complete = true;
                    break;
                }
            }
        }
        assert!(complete);
        assert_eq!(count, source.len());
        assert_eq!(work.remaining().records, 2_000_000 - source.len() as u64);
    }
    #[test]
    fn every_phase_replays_without_refunding_and_failures_retire() {
        assert_eq!(
            Error::Work(Stop::Records).to_string(),
            "header text work: work record budget exhausted"
        );
        assert_eq!(Error::InvalidState.to_string(), "invalid header text state");
        for input in [
            b"  a\xe1\x80 =?utf-8?Q?e=CC=81?= \r\n\t=?utf-8?B?4oKs?=  tail".as_slice(),
            b"=?utf-8?Q?x?= \t=?unknown?Q?x?=",
            b"=?utf-8?Q?=E1=QZ=80?=  ",
            b"tail\xe1\x80",
            b"=?utf-8?Q?one?=\r\n =?utf-8?Q?two?=",
            b"a =?unknown?Q?x?=",
            b"=?utf-8?Q?aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa?=",
        ] {
            let mut cursor = Cursor::new(input);
            let mut work = work();
            let mut done = false;
            for _ in 0..1000 {
                let saved = cursor;
                let before = work.remaining();
                let first = cursor.poll(Tick(1), &mut work).unwrap();
                let remaining = work.remaining();
                let problem = cursor.is_encoding_problem();
                cursor = saved;
                assert_eq!(cursor.poll(Tick(1), &mut work).unwrap(), first);
                assert_eq!(cursor.is_encoding_problem(), problem);
                assert_eq!(
                    remaining.io_bytes - work.remaining().io_bytes,
                    before.io_bytes - remaining.io_bytes
                );
                assert_eq!(
                    remaining.records - work.remaining().records,
                    before.records - remaining.records
                );
                let mut expired = saved;
                let mut timed_work = self::work();
                assert_eq!(
                    expired.poll(Tick(100), &mut timed_work),
                    Err(Error::Work(Stop::Deadline))
                );
                let mut fresh = self::work();
                let before = fresh.remaining();
                assert_eq!(
                    expired.poll(Tick(1), &mut fresh),
                    Err(Error::Work(Stop::Deadline))
                );
                assert_eq!(fresh.remaining(), before);
                if first == Status::Complete {
                    done = true;
                    let before = work.remaining();
                    assert_eq!(cursor.poll(Tick(100), &mut work), Ok(Status::Complete));
                    assert_eq!(work.remaining(), before);
                    break;
                }
            }
            assert!(done);
        }
        for (source, io, records, stop) in [
            (b"x".as_slice(), 0, 100, Stop::IoBytes),
            (b"x".as_slice(), 100, 0, Stop::Records),
            (b"=?utf-8?Q?a?=".as_slice(), 10000, 1, Stop::Records),
        ] {
            let mut cursor = Cursor::new(source);
            let mut limited = Meter::new(
                Deadline::after(Tick(0), 100).unwrap(),
                Charge {
                    io_bytes: io,
                    records,
                    ..Charge::default()
                },
            );
            let mut failed = false;
            for _ in 0..100 {
                match cursor.poll(Tick(1), &mut limited) {
                    Err(error) => {
                        assert_eq!(error, Error::Work(stop));
                        assert_eq!(cursor.poll(Tick(1), &mut self::work()), Err(error));
                        failed = true;
                        break;
                    }
                    Ok(Status::Complete) => panic!("capacity fixture completed"),
                    Ok(_) => {}
                }
            }
            assert!(failed);
        }
    }
}

#[cfg(test)]
mod structured_tests {
    #![allow(clippy::unwrap_used, clippy::panic)]
    use super::*;
    use crate::ports::Deadline;
    #[test]
    fn lexical_charge_refusal_retires_even_with_a_fresh_job_meter() {
        for grammar in [Grammar::Keywords, Grammar::ListId] {
            let mut cursor = Cursor::with_grammar(b"x", grammar);
            let mut work = Meter::new(
                Deadline::after(Tick(0), 100).unwrap(),
                Charge {
                    io_bytes: 1000,
                    records: 1,
                    ..Charge::default()
                },
            );
            let mut refused = false;
            for _ in 0..10 {
                match cursor.poll(Tick(1), &mut work) {
                    Ok(status) => assert_eq!(status, Status::Yield),
                    Err(error) => {
                        assert_eq!(error, Error::Work(Stop::Records));
                        let mut fresh = Meter::new(
                            Deadline::after(Tick(0), 100).unwrap(),
                            Charge {
                                io_bytes: 1000,
                                records: 1000,
                                ..Charge::default()
                            },
                        );
                        let before = fresh.remaining();
                        assert_eq!(cursor.poll(Tick(1), &mut fresh), Err(error));
                        assert_eq!(fresh.remaining(), before);
                        refused = true;
                        break;
                    }
                }
            }
            assert!(refused);
        }
    }
    #[test]
    fn structured_checkpoints_replay_lexical_state_without_refunding_work() {
        for grammar in [Grammar::Keywords, Grammar::ListId] {
            for bytes in [
                b" \"a\\\" =?utf-8?Q?no?=\" (=?utf-8?Q?e=CC=81?= \t=?utf-8?Q?two?=) <x>".as_slice(),
                b" (\\x=?utf-8?Q?yes?=) =?utf-8?Q?=FF?= ",
                b" =?utf-8?B?@@?= =?utf-8?Q?no?=, a\xff",
            ] {
                let mut cursor = Cursor::with_grammar(bytes, grammar);
                assert!(std::mem::size_of_val(&cursor) <= 208);
                assert!(!cursor.at(&Cursor::new(bytes)));
                let mut work = Meter::new(
                    Deadline::after(Tick(0), 100).unwrap(),
                    Charge {
                        io_bytes: 1_000_000,
                        records: 1_000_000,
                        ..Charge::default()
                    },
                );
                let mut complete = false;
                for _ in 0..10000 {
                    let saved = cursor;
                    let before = work.remaining();
                    let first = cursor.poll(Tick(1), &mut work).unwrap();
                    let after = work.remaining();
                    assert!(before.io_bytes - after.io_bytes <= 229);
                    assert!(before.records - after.records <= 226);
                    let advanced = cursor;
                    cursor = saved;
                    assert_eq!(cursor.poll(Tick(1), &mut work).unwrap(), first);
                    assert!(cursor.at(&advanced));
                    assert_eq!(
                        before.io_bytes - after.io_bytes,
                        after.io_bytes - work.remaining().io_bytes
                    );
                    assert_eq!(
                        before.records - after.records,
                        after.records - work.remaining().records
                    );
                    if first == Status::Complete {
                        complete = true;
                        break;
                    }
                }
                assert!(complete);
            }
        }
    }
}

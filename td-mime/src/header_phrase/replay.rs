//! Charged token traversal of an immutable, fully validated phrase.
use super::{Error, Extent, Kind, Status, Token, Validated};
use crate::decode_work::{Error as DecodeError, Work};
use crate::{
    header_message_ids::atext,
    time::Tick,
    work::{Charge, Meter},
};
#[derive(Clone, Copy)]
enum Phase {
    Cfws,
    Comment,
    Atom,
    Quoted,
    Complete,
}
/// Copies retain source and lexical progress, never counters or output authority.
#[derive(Clone, Copy)]
pub struct Cursor<'a> {
    source: &'a [u8],
    position: usize,
    previous: usize,
    start: usize,
    phase: Phase,
    depth: u8,
    escaped: bool,
    failure: Option<DecodeError>,
}
impl<'a> Cursor<'a> {
    pub(super) const fn new(validated: Validated<'a>) -> Self {
        Self {
            source: validated.source,
            position: 0,
            previous: 0,
            start: 0,
            phase: Phase::Cfws,
            depth: 0,
            escaped: false,
            failure: None,
        }
    }
    fn tail(&self) -> Extent {
        Extent {
            start: self.previous,
            end: self.source.len(),
        }
    }
    fn advance(&mut self) -> Result<(), DecodeError> {
        self.position = self
            .position
            .checked_add(1)
            .ok_or(DecodeError::InvalidState)?;
        if self.position > self.source.len() {
            return Err(DecodeError::InvalidState);
        }
        Ok(())
    }
    fn token(&mut self, kind: Kind) -> Result<Status, DecodeError> {
        if self.previous > self.start
            || self.start >= self.position
            || self.position > self.source.len()
        {
            return Err(DecodeError::InvalidState);
        }
        let token = Token {
            leading: Extent {
                start: self.previous,
                end: self.start,
            },
            text: Extent {
                start: self.start,
                end: self.position,
            },
            kind,
        };
        self.previous = self.position;
        self.phase = Phase::Cfws;
        Ok(Status::Token(token))
    }
    pub fn poll(&mut self, now: Tick, work: &mut Meter) -> Result<Status, Error> {
        self.poll_with_work(now, work).map_err(|error| match error {
            DecodeError::Work(stop) => Error::Work(stop),
            DecodeError::InvalidState | DecodeError::InterpretationLimit => Error::InvalidState,
        })
    }
    pub(super) fn poll_with_work(
        &mut self,
        now: Tick,
        work: &mut impl Work,
    ) -> Result<Status, DecodeError> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if matches!(self.phase, Phase::Complete) {
            return Ok(Status::Complete(self.tail()));
        }
        let result = self.step(now, work);
        if let Err(error) = result {
            self.failure = Some(error);
        }
        result
    }
    pub(super) fn checkpoint(&self) -> Result<Checkpoint<'a>, DecodeError> {
        if self.failure.is_some()
            || !matches!(self.phase, Phase::Cfws)
            || self.position != self.previous
            || self.position == 0
        {
            return Err(DecodeError::InvalidState);
        }
        Ok(Checkpoint {
            source: self.source,
            position: self.position,
        })
    }
    fn step(&mut self, now: Tick, work: &mut impl Work) -> Result<Status, DecodeError> {
        if self.position > self.source.len() {
            return Err(DecodeError::InvalidState);
        }
        for _ in 0..32 {
            work.charge(
                now,
                Charge {
                    records: 1,
                    io_bytes: u64::from(self.position < self.source.len()),
                    ..Charge::default()
                },
            )?;
            let byte = self.source.get(self.position).copied();
            if matches!(self.phase, Phase::Atom) {
                if byte.is_some_and(atext) {
                    self.advance()?;
                    continue;
                }
                return self.token(Kind::Atom);
            }
            let Some(byte) = byte else {
                if !matches!(self.phase, Phase::Cfws) {
                    return Err(DecodeError::InvalidState);
                }
                self.phase = Phase::Complete;
                return Ok(Status::Complete(self.tail()));
            };
            match self.phase {
                Phase::Cfws => match byte {
                    b' ' | b'\t' | b'\r' | b'\n' => self.advance()?,
                    b'(' => {
                        self.depth = 1;
                        self.advance()?;
                        self.phase = Phase::Comment;
                    }
                    b'"' => {
                        self.start = self.position;
                        self.advance()?;
                        self.phase = Phase::Quoted;
                    }
                    b'.' => {
                        self.start = self.position;
                        self.advance()?;
                        return self.token(Kind::Dot);
                    }
                    byte if atext(byte) => {
                        self.start = self.position;
                        self.phase = Phase::Atom;
                        return Ok(Status::Yield);
                    }
                    _ => return Err(DecodeError::InvalidState),
                },
                Phase::Comment => {
                    self.advance()?;
                    if self.escaped {
                        self.escaped = false;
                    } else {
                        match byte {
                            b'\\' => self.escaped = true,
                            b'(' => {
                                self.depth =
                                    self.depth.checked_add(1).ok_or(DecodeError::InvalidState)?;
                                if self.depth > 32 {
                                    return Err(DecodeError::InvalidState);
                                }
                            }
                            b')' => {
                                self.depth =
                                    self.depth.checked_sub(1).ok_or(DecodeError::InvalidState)?;
                                if self.depth == 0 {
                                    self.phase = Phase::Cfws;
                                }
                            }
                            _ => {}
                        }
                    }
                }
                Phase::Quoted => {
                    self.advance()?;
                    if self.escaped {
                        self.escaped = false;
                    } else {
                        match byte {
                            b'\\' => self.escaped = true,
                            b'"' => return self.token(Kind::Quoted),
                            _ => {}
                        }
                    }
                }
                Phase::Atom | Phase::Complete => return Err(DecodeError::InvalidState),
            }
        }
        Ok(Status::Yield)
    }
}

#[derive(Clone, Copy)]
pub(super) struct Checkpoint<'a> {
    source: &'a [u8],
    position: usize,
}
impl<'a> Checkpoint<'a> {
    pub(super) const fn initial(proof: Validated<'a>) -> Self {
        Self {
            source: proof.source,
            position: 0,
        }
    }
    pub(super) const fn replay(self) -> Cursor<'a> {
        Cursor {
            position: self.position,
            previous: self.position,
            ..Cursor::new(Validated {
                source: self.source,
            })
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::{header_phrase, time::Deadline, work::Stop};
    fn work() -> Meter {
        Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: 20_000_000,
                records: 20_000_000,
                ..Charge::default()
            },
        )
    }
    fn validate(source: &[u8]) -> (Validated<'_>, Vec<Status>) {
        let mut cursor = header_phrase::Cursor::new(source);
        let mut meter = work();
        let mut expected = Vec::new();
        for _ in 0..1_000_000 {
            let status = cursor.poll(Tick(1), &mut meter).unwrap();
            if status != Status::Yield {
                expected.push(status);
            }
            if matches!(status, Status::Complete(_)) {
                return (cursor.into_validated().unwrap(), expected);
            }
        }
        panic!("validation did not finish");
    }
    fn collect(mut cursor: Cursor<'_>, meter: &mut Meter) -> Vec<Status> {
        let mut statuses = Vec::new();
        for _ in 0..1_000_000 {
            let before = meter.remaining();
            let status = cursor.poll(Tick(1), meter).unwrap();
            let after = meter.remaining();
            assert!(before.io_bytes - after.io_bytes <= 32);
            assert!(before.records - after.records <= 32);
            assert_eq!(before.output_bytes, after.output_bytes);
            if status != Status::Yield {
                statuses.push(status);
            }
            if matches!(status, Status::Complete(_)) {
                assert_eq!(cursor.poll(Tick(100), meter), Ok(status));
                assert_eq!(meter.remaining(), after);
                return statuses;
            }
        }
        panic!("replay did not finish");
    }
    #[test]
    fn proof_requires_complete_success_and_retains_failure() {
        assert!(matches!(
            header_phrase::Cursor::new(b"word").into_validated(),
            Err(Error::InvalidState)
        ));
        let mut cursor = header_phrase::Cursor::new(b"word (tail)");
        let mut meter = work();
        while !matches!(cursor.poll(Tick(1), &mut meter).unwrap(), Status::Token(_)) {}
        assert!(matches!(cursor.into_validated(), Err(Error::InvalidState)));
        for (source, now, expected) in [
            (b"word@bad".as_slice(), Tick(1), Error::Malformed),
            (b"word", Tick(100), Error::Work(Stop::Deadline)),
        ] {
            let mut cursor = header_phrase::Cursor::new(source);
            loop {
                match cursor.poll(now, &mut meter) {
                    Ok(Status::Complete(_)) => panic!("bad phrase completed"),
                    Ok(_) => {}
                    Err(error) => {
                        assert_eq!(error, expected);
                        break;
                    }
                }
            }
            assert!(matches!(cursor.into_validated(), Err(error) if error == expected));
        }
        let (proof, expected) = validate(b"word (tail)");
        assert!(std::mem::size_of_val(&proof) <= 16);
        assert!(std::mem::size_of_val(&proof.replay()) <= 80);
        assert_eq!(collect(proof.replay(), &mut work()), expected);
    }
    #[test]
    fn replay_matches_validator_tokens_and_exact_gaps() {
        let long = format!(
            " ({}{}) {} \"{}\"{}",
            "(".repeat(31),
            ")".repeat(31),
            "🐈".repeat(10_000),
            "x".repeat(10_000),
            " ".repeat(10_000)
        );
        for source in [
            b" a\"b\"..c (tail)".as_slice(),
            b"\"\".",
            b"\"\\\0\" (\\) nested (x)) a",
            b"a\r\n \"b\n\tc\\\"d\"\r\n\t(tail)",
            "例e\u{301}\u{fdd0} (🐈) \"\\🐈\"".as_bytes(),
            b"=?utf-8?q?a?=(x)=?utf-8?q?b?= \"=?utf-8?q?c?=\"",
            long.as_bytes(),
        ] {
            let (proof, expected) = validate(source);
            assert_eq!(collect(proof.replay(), &mut work()), expected);
        }
        // Independent grammar products exercise shared-parser/replayer agreement.
        for first in ["a", "例", "\"\"", "\"a\\\"b\""] {
            for gap in ["", " ", "\r\n\t", "(x)", " (a(\\)b)) "] {
                for last in ["word", ".", "\"last\"", "=?utf-8?q?x?="] {
                    let source = format!("(head){first}{gap}{last} (tail)");
                    let (proof, expected) = validate(source.as_bytes());
                    assert_eq!(collect(proof.replay(), &mut work()), expected, "{source}");
                }
            }
        }
    }
    #[test]
    fn every_checkpoint_replays_suffix_and_charges_again() {
        let mut escaped_quote = false;
        let mut escaped_comment = false;
        let mut nested_comment = false;
        for pad in 0..32 {
            for source in [
                format!("a (outer(inner {}\\)x)) tail", "x".repeat(pad)),
                format!("a \"{}\\\"end\". tail", "q".repeat(pad)),
            ] {
                let (proof, expected) = validate(source.as_bytes());
                let mut cursor = proof.replay();
                let mut meter = work();
                let mut emitted = 0;
                let mut complete = false;
                for _ in 0..1000 {
                    escaped_quote |= matches!(cursor.phase, Phase::Quoted) && cursor.escaped;
                    escaped_comment |= matches!(cursor.phase, Phase::Comment) && cursor.escaped;
                    nested_comment |= matches!(cursor.phase, Phase::Comment) && cursor.depth > 1;
                    let before = meter.remaining();
                    let checkpoint = cursor;
                    assert_eq!(collect(checkpoint, &mut meter), expected[emitted..]);
                    let spent = meter.remaining();
                    assert!(spent.records < before.records);
                    assert_eq!(collect(checkpoint, &mut meter), expected[emitted..]);
                    let twice = meter.remaining();
                    assert_eq!(
                        before.records - spent.records,
                        spent.records - twice.records
                    );
                    assert_eq!(
                        before.io_bytes - spent.io_bytes,
                        spent.io_bytes - twice.io_bytes
                    );
                    let status = cursor.poll(Tick(1), &mut meter).unwrap();
                    if status != Status::Yield {
                        emitted += 1;
                    }
                    if matches!(status, Status::Complete(_)) {
                        complete = true;
                        break;
                    }
                }
                assert!(complete, "checkpoint traversal did not finish");
            }
        }
        assert!(escaped_quote && escaped_comment && nested_comment);
    }
    #[test]
    fn exact_work_includes_peeks_and_eof() {
        for (source, io, records) in [("a", 2, 4), ("\"\"", 2, 3), ("a.", 4, 5)] {
            let (proof, _) = validate(source.as_bytes());
            let mut meter = work();
            let before = meter.remaining();
            collect(proof.replay(), &mut meter);
            assert_eq!(before.io_bytes - meter.remaining().io_bytes, io);
            assert_eq!(before.records - meter.remaining().records, records);
        }
    }
    #[test]
    fn each_lexical_phase_latches_refusal_including_copies() {
        let source = format!(
            "a{} ({}x) \"{}\".",
            "b".repeat(70),
            "x".repeat(70),
            "q".repeat(70)
        );
        let (proof, _) = validate(source.as_bytes());
        for target in [Phase::Cfws, Phase::Atom, Phase::Comment, Phase::Quoted] {
            for (io_bytes, records, now, expected) in [
                (0, 1000, Tick(1), Stop::IoBytes),
                (1000, 0, Tick(1), Stop::Records),
                (1000, 1000, Tick(100), Stop::Deadline),
            ] {
                let mut cursor = proof.replay();
                let mut meter = work();
                while std::mem::discriminant(&cursor.phase) != std::mem::discriminant(&target) {
                    assert!(!matches!(
                        cursor.poll(Tick(1), &mut meter).unwrap(),
                        Status::Complete(_)
                    ));
                }
                let mut limited = Meter::new(
                    Deadline::after(Tick(0), 100).unwrap(),
                    Charge {
                        io_bytes,
                        records,
                        ..Charge::default()
                    },
                );
                assert_eq!(cursor.poll(now, &mut limited), Err(Error::Work(expected)));
                let mut copied = cursor;
                let before = meter.remaining();
                assert_eq!(cursor.poll(Tick(1), &mut meter), Err(Error::Work(expected)));
                assert_eq!(copied.poll(Tick(1), &mut meter), Err(Error::Work(expected)));
                assert_eq!(meter.remaining(), before);
            }
        }
    }
}

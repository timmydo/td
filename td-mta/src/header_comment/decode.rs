//! Comment fallback-name scalars from a complete immutable grammar proof.
use super::Validated;
use crate::{
    admission::work::{Charge, Meter},
    decode_work::Work,
    encoded_word::{self, Context, Word},
    ports::Tick,
};
pub use crate::{encoded_word::decode::Status, header_text::Error};
#[derive(Clone, Copy)]
enum Phase<'a> {
    Leading,
    Boundary,
    Literal,
    Gap,
    Candidate,
    Recognize,
    Word(encoded_word::decode::Cursor<'a>),
    Complete,
}
#[derive(Clone, Copy)]
pub struct Cursor<'a> {
    source: &'a [u8],
    position: usize,
    scan: usize,
    token_start: usize,
    turn: u64,
    phase: Phase<'a>,
    gap: bool,
    previous_word: bool,
    emitted: bool,
    space_pending: bool,
    held: Option<char>,
    problem: bool,
    failure: Option<Error>,
}
impl<'a> Cursor<'a> {
    pub(super) const fn new(proof: Validated<'a>) -> Self {
        Self {
            source: proof.source,
            position: 1,
            scan: 1,
            token_start: 1,
            turn: 0,
            phase: Phase::Leading,
            gap: false,
            previous_word: false,
            emitted: false,
            space_pending: false,
            held: None,
            problem: false,
            failure: None,
        }
    }
    pub(crate) fn at(&self, other: &Self) -> bool {
        std::ptr::eq(self.source, other.source) && self.turn == other.turn
    }
    pub const fn is_encoding_problem(&self) -> bool {
        self.problem
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
        let result = self.step(now, work).and_then(|mut status| {
            self.turn = self.turn.checked_add(1).ok_or(Error::InvalidState)?;
            if let Status::Scalar(value) = status {
                if self.space_pending {
                    self.held = Some(value);
                    self.space_pending = false;
                    status = Status::Scalar(' ');
                }
                self.emitted = true;
            }
            Ok(status)
        });
        if let Err(error) = result {
            self.failure = Some(error);
        }
        result
    }
    fn end(&self) -> Result<usize, Error> {
        self.source.len().checked_sub(1).ok_or(Error::InvalidState)
    }
    fn byte(&self, at: usize, now: Tick, work: &mut impl Work) -> Result<Option<u8>, Error> {
        let end = self.end()?;
        if at > end {
            return Err(Error::InvalidState);
        }
        work.charge(
            now,
            Charge {
                io_bytes: u64::from(at < end),
                ..Charge::default()
            },
        )?;
        if at == end {
            return Ok(None);
        }
        Ok(Some(*self.source.get(at).ok_or(Error::InvalidState)?))
    }
    fn atom(
        &self,
        at: usize,
        now: Tick,
        work: &mut impl Work,
    ) -> Result<Option<(u8, usize, bool)>, Error> {
        td_header::projection::atom(at, true, |at| self.byte(at, now, work))
            .map(|atom| atom.map(|atom| (atom.value, atom.next, atom.escaped)))
            .map_err(|error| match error {
                td_header::projection::Error::Read(error) => error,
                td_header::projection::Error::IncompletePair
                | td_header::projection::Error::InvalidState => Error::InvalidState,
            })
    }
    fn begin_candidate(&mut self, position: usize) {
        self.token_start = position;
        self.scan = position;
        self.phase = Phase::Candidate;
    }
    fn reject(&mut self) -> Status {
        self.position = self.token_start;
        self.phase = Phase::Literal;
        self.previous_word = false;
        self.space_pending |= self.gap && self.emitted;
        self.gap = false;
        Status::Yield
    }
    fn literal(&mut self, now: Tick, work: &mut impl Work) -> Result<Status, Error> {
        let Some((first, mut next, escaped)) = self.atom(self.position, now, work)? else {
            self.phase = Phase::Complete;
            return Ok(Status::Complete);
        };
        let width = match first {
            0..=0x7f => 1,
            0xc2..=0xdf => 2,
            0xe0..=0xef => 3,
            0xf0..=0xf4 => 4,
            _ => return Err(Error::InvalidState),
        };
        let mut bytes = [first, 0, 0, 0];
        for cell in bytes.get_mut(1..width).ok_or(Error::InvalidState)? {
            let (byte, end, _) = self.atom(next, now, work)?.ok_or(Error::InvalidState)?;
            *cell = byte;
            next = end;
        }
        work.charge(
            now,
            Charge {
                io_bytes: width as u64,
                ..Charge::default()
            },
        )?;
        let value = std::str::from_utf8(bytes.get(..width).ok_or(Error::InvalidState)?)
            .map_err(|_| Error::InvalidState)?
            .chars()
            .next()
            .ok_or(Error::InvalidState)?;
        self.position = next;
        self.previous_word = false;
        self.phase = if escaped || matches!(first, b'(' | b')') {
            Phase::Boundary
        } else {
            Phase::Literal
        };
        if value == '\0' {
            return Ok(Status::Yield);
        }
        let code = u32::from(value);
        if matches!(code, 0xfdd0..=0xfdef) || code & 0xffff >= 0xfffe {
            self.problem = true;
            return Ok(Status::Scalar('\u{fffd}'));
        }
        Ok(Status::Scalar(value))
    }
    fn step(&mut self, now: Tick, work: &mut impl Work) -> Result<Status, Error> {
        work.charge(
            now,
            Charge {
                records: 1,
                ..Charge::default()
            },
        )?;
        if let Some(value) = self.held.take() {
            return Ok(Status::Scalar(value));
        }
        match self.phase {
            Phase::Leading => {
                match self.atom(self.position, now, work)? {
                    Some((b' ' | b'\t', next, false)) => self.position = next,
                    _ => self.phase = Phase::Boundary,
                }
                Ok(Status::Yield)
            }
            Phase::Boundary | Phase::Literal => {
                match self.atom(self.position, now, work)? {
                    Some((b' ' | b'\t', _, false)) => {
                        self.scan = self.position;
                        self.gap = false;
                        self.phase = Phase::Gap;
                    }
                    Some((b'=', _, false)) if matches!(self.phase, Phase::Boundary) => {
                        self.gap = false;
                        self.begin_candidate(self.position);
                    }
                    _ => return self.literal(now, work),
                }
                Ok(Status::Yield)
            }
            Phase::Gap => {
                match self.atom(self.scan, now, work)? {
                    Some((b' ' | b'\t', next, false)) => {
                        self.gap = true;
                        self.scan = next;
                    }
                    Some((b'=', _, false)) if self.gap => self.begin_candidate(self.scan),
                    None => {
                        self.phase = Phase::Complete;
                        return Ok(Status::Complete);
                    }
                    _ => {
                        self.position = self.scan;
                        self.phase = Phase::Literal;
                        self.space_pending |= self.gap && self.emitted;
                        self.gap = false;
                    }
                }
                Ok(Status::Yield)
            }
            Phase::Candidate => {
                match self.byte(self.scan, now, work)? {
                    None | Some(b' ' | b'\t' | b'\r' | b'\n' | b'(' | b')' | b'\\') => {
                        self.phase = Phase::Recognize
                    }
                    Some(_) => {
                        self.scan = self.scan.checked_add(1).ok_or(Error::InvalidState)?;
                        if self
                            .scan
                            .checked_sub(self.token_start)
                            .ok_or(Error::InvalidState)?
                            > 75
                        {
                            return Ok(self.reject());
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
                if let Some(word) = Word::recognize_with_work(token, Context::Comment, now, work)? {
                    self.space_pending |= self.gap && self.emitted && !self.previous_word;
                    self.gap = false;
                    self.position = self.scan;
                    self.phase = Phase::Word(encoded_word::decode::Cursor::new(word));
                    Ok(Status::Yield)
                } else {
                    Ok(self.reject())
                }
            }
            Phase::Word(mut word) => {
                let status = word.poll_with_work(now, work)?;
                self.problem |= word.is_encoding_problem();
                self.phase = Phase::Word(word);
                if status == Status::Complete {
                    self.previous_word = true;
                    self.scan = self.position;
                    self.gap = false;
                    self.phase = Phase::Gap;
                    Ok(Status::Yield)
                } else {
                    Ok(status)
                }
            }
            Phase::Complete => Ok(Status::Complete),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::{admission::work::Stop, header_comment, ports::Deadline};
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
    fn cursor(input: &[u8]) -> Cursor<'_> {
        let mut parser = header_comment::Cursor::new(input);
        let mut meter = work();
        while parser.poll(Tick(1), &mut meter).unwrap() != header_comment::Status::Complete {}
        parser.into_validated().unwrap().decode()
    }
    fn collect(mut cursor: Cursor<'_>, meter: &mut Meter) -> (String, bool) {
        assert!(std::mem::size_of_val(&cursor) <= 192);
        let mut text = String::new();
        for _ in 0..2_000_000 {
            let before = meter.remaining();
            let status = cursor.poll(Tick(1), meter).unwrap();
            let after = meter.remaining();
            assert!(before.records - after.records <= 227);
            assert!(before.io_bytes - after.io_bytes <= 225);
            assert_eq!(before.output_bytes, after.output_bytes);
            match status {
                Status::Yield => {}
                Status::Scalar(value) => text.push(value),
                Status::Complete => {
                    assert_eq!(cursor.poll(Tick(100), meter), Ok(Status::Complete));
                    assert_eq!(after, meter.remaining());
                    return (text, cursor.is_encoding_problem());
                }
            }
        }
        panic!("comment decode did not finish");
    }
    fn text(input: &str) -> (String, bool) {
        collect(cursor(input.as_bytes()), &mut work())
    }
    #[test]
    fn shared_projection_preserves_source_admission_and_eof_policy() {
        #[derive(Default)]
        struct Counts {
            calls: u64,
            visits: u64,
        }
        impl Work for Counts {
            fn charge(
                &mut self,
                _now: Tick,
                charge: Charge,
            ) -> Result<(), crate::decode_work::Error> {
                self.calls += 1;
                self.visits += charge.io_bytes;
                assert_eq!(charge.records, 0);
                Ok(())
            }
        }
        let source = b"(\\\r\\\n\\ x)";
        let cursor = cursor(source);
        let mut counts = Counts::default();
        assert_eq!(
            cursor.atom(1, Tick(1), &mut counts).unwrap(),
            Some((b' ', 7, true))
        );
        assert_eq!((counts.calls, counts.visits), (6, 6));
        let before = counts.calls;
        assert_eq!(
            cursor.atom(source.len() - 1, Tick(1), &mut counts).unwrap(),
            None
        );
        assert_eq!(counts.calls - before, 1);
        assert_eq!(counts.visits, 6);
    }
    #[test]
    fn comments_preserve_nested_and_escaped_text_with_explicit_whitespace_policy() {
        for (source, expected) in [
            ("()", ""),
            ("( \t\r\n )", ""),
            ("( Name\r\n \tlast )", "Name last"),
            ("(a(b)c)", "a(b)c"),
            ("(a\\(b\\)c)", "a(b)c"),
            ("( ( name ) )", "( name )"),
            ("(\\ name\\ )", " name "),
            ("(\\\0 a \\\0)", "a"),
            ("(a \\\0 b)", "a b"),
            ("(\"quoted\")", "\"quoted\""),
            ("(e\u{301}例)", "e\u{301}例"),
            ("(\\例)", "例"),
            ("(\\\t)", "\t"),
            ("(\\\r)", "\r"),
            ("(\\\u{7f})", "\u{7f}"),
            ("(\u{1})", "\u{1}"),
            ("(a\\\r\n b)", "a b"),
            ("(a\\\n b)", "a b"),
            ("(a\\\r\\\n\\ b)", "a b"),
        ] {
            assert_eq!(text(source), (expected.to_owned(), false), "{source:?}");
        }
        assert_eq!(text("(\u{fdd0})"), ("�".to_owned(), true));
        let source = format!("( {} )", "🐈".repeat(10_000));
        assert_eq!(text(&source), ("🐈".repeat(10_000), false));
    }
    #[test]
    fn only_original_unescaped_comment_boundaries_authorize_encoded_words() {
        let word = "=?utf-8?q?x?=";
        for (source, expected) in [
            (format!("({word})"), "x".to_owned()),
            (format!("(({word}))"), "(x)".to_owned()),
            (format!("({word}\r\n {word})"), "xx".to_owned()),
            (format!("({word}\n {word})"), "xx".to_owned()),
            (format!("(\\\r\\\n {word})"), " x".to_owned()),
            (format!("(\\\r\n {word})"), " x".to_owned()),
            (format!("(\\\n {word})"), " x".to_owned()),
            (format!("({word}\\\r\\\n {word})"), "x x".to_owned()),
            (format!("({word} ({word}))"), "x (x)".to_owned()),
            (format!("({word}(nested))"), "x(nested)".to_owned()),
            (format!("((nested){word})"), "(nested)x".to_owned()),
            (format!("(a{word})"), format!("a{word}")),
            (format!("({word}a)"), format!("{word}a")),
            (format!("(\\{word})"), word.to_owned()),
            (format!("(\\({word})"), "(x".to_owned()),
            (format!("({word}\\))"), "x)".to_owned()),
            (format!("(\\x{word})"), "xx".to_owned()),
            (format!("({word}\\x)"), "xx".to_owned()),
            (format!("(\\ {word})"), " x".to_owned()),
            (format!("({word}\\ )"), "x ".to_owned()),
            (
                format!("({word} =?unknown?q?y?=)"),
                "x =?unknown?q?y?=".to_owned(),
            ),
        ] {
            assert_eq!(text(&source), (expected, false), "{source:?}");
        }
        assert_eq!(text("(=?utf-8?q?=FF?=)"), ("�".to_owned(), true));
        assert_eq!(text("(a =?utf-8?q?=00?=)"), ("a".to_owned(), false));
        assert_eq!(text("(=?utf-8?q?_x_?=)"), (" x ".to_owned(), false));
    }
    #[test]
    fn all_copied_states_reproduce_the_remaining_scalars_and_charge_again() {
        let source = b"(a =?utf-8?q?e=CC=81?= (x\\)y)\r\n z)";
        let mut cursor = cursor(source);
        let mut meter = work();
        let expected = collect(cursor, &mut work()).0;
        let mut emitted = String::new();
        for _ in 0..1000 {
            let before = meter.remaining();
            let (suffix, _) = collect(cursor, &mut meter);
            assert_eq!(format!("{emitted}{suffix}"), expected);
            assert!(meter.remaining().records < before.records);
            match cursor.poll(Tick(1), &mut meter).unwrap() {
                Status::Yield => {}
                Status::Scalar(value) => emitted.push(value),
                Status::Complete => return,
            }
        }
        panic!("copied comment did not finish");
    }
    #[test]
    fn failures_stay_terminal_after_a_prefix_and_across_copies() {
        let mut cursor = cursor(b"(name =?utf-8?q?rest?=)");
        let mut meter = work();
        while !matches!(cursor.poll(Tick(1), &mut meter).unwrap(), Status::Scalar(_)) {}
        for (io_bytes, records, now, expected) in [
            (0, 1000, Tick(1), Stop::IoBytes),
            (1000, 0, Tick(1), Stop::Records),
            (1000, 1000, Tick(100), Stop::Deadline),
        ] {
            let mut copy = cursor;
            let mut limited = Meter::new(
                Deadline::after(Tick(0), 100).unwrap(),
                Charge {
                    io_bytes,
                    records,
                    ..Charge::default()
                },
            );
            let error = loop {
                match copy.poll(now, &mut limited) {
                    Ok(Status::Complete) => panic!("refused comment completed"),
                    Ok(_) => {}
                    Err(error) => break error,
                }
            };
            assert_eq!(error, Error::Work(expected));
            let before = meter.remaining();
            let mut failed = copy;
            assert_eq!(failed.poll(Tick(1), &mut meter), Err(error));
            assert_eq!(before, meter.remaining());
        }
    }
    #[test]
    fn small_literal_work_includes_boundary_peeks_and_conversion() {
        for (source, io, records) in [("()", 0, 2), ("(a)", 4, 3)] {
            let mut meter = work();
            let before = meter.remaining();
            collect(cursor(source.as_bytes()), &mut meter);
            assert_eq!(before.io_bytes - meter.remaining().io_bytes, io);
            assert_eq!(before.records - meter.remaining().records, records);
        }
    }
}

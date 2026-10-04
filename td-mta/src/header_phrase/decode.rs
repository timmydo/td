//! Display-name phrase scalars; normalization and publication are external.
use super::{replay, Extent, Kind, Status as TokenStatus, Validated};
use crate::{
    admission::work::{Charge, Meter},
    decode_work::Work,
    encoded_word::{self, Context, Word},
    ports::Tick,
};
pub use crate::{encoded_word::decode::Status, header_text::Error};

#[derive(Clone, Copy)]
enum Phase<'a> {
    Classify(replay::Cursor<'a>),
    Replay(replay::Cursor<'a>),
    Gap,
    Recognize,
    Trim { last: usize },
    Literal,
    Word(encoded_word::decode::Cursor<'a>),
    Complete,
}
/// Copies retain lexical state, never the caller's work allowance.
#[derive(Clone, Copy)]
pub struct Cursor<'a> {
    field: &'a [u8],
    name: Extent,
    resume: replay::Checkpoint<'a>,
    phase: Phase<'a>,
    token: Extent,
    scan: usize,
    // Successful turns identify deterministic NFC checkpoints without prefix scans.
    turn: u64,
    kind: Kind,
    first: bool,
    sole_quoted: bool,
    gap: bool,
    pure_lws: bool,
    previous_word: bool,
    problem: bool,
    failure: Option<Error>,
}
impl<'a> Cursor<'a> {
    /// Bind the proof to its exact extent within one admitted field value.
    pub fn new(proof: Validated<'a>, field: &'a [u8], name: Extent) -> Result<Self, Error> {
        let source = field.get(name.start..name.end).ok_or(Error::InvalidState)?;
        if !std::ptr::eq(source, proof.source) {
            return Err(Error::InvalidState);
        }
        Ok(Self {
            field,
            name,
            resume: replay::Checkpoint::initial(proof),
            phase: Phase::Classify(proof.replay()),
            token: Extent { start: 0, end: 0 },
            scan: 0,
            turn: 0,
            kind: Kind::Atom,
            first: true,
            sole_quoted: false,
            gap: false,
            pure_lws: true,
            previous_word: false,
            problem: false,
            failure: None,
        })
    }
    pub const fn is_encoding_problem(&self) -> bool {
        self.problem
    }
    pub(crate) fn at(&self, other: &Self) -> bool {
        std::ptr::eq(self.field, other.field) && self.name == other.name && self.turn == other.turn
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
    fn source(&self) -> Result<&'a [u8], Error> {
        self.field
            .get(self.name.start..self.name.end)
            .ok_or(Error::InvalidState)
    }
    fn byte(&self, at: usize, now: Tick, work: &mut impl Work) -> Result<Option<u8>, Error> {
        let source = self.source()?;
        if at > source.len() {
            return Err(Error::InvalidState);
        }
        work.charge(
            now,
            Charge {
                io_bytes: u64::from(at < source.len()),
                ..Charge::default()
            },
        )?;
        Ok(source.get(at).copied())
    }
    fn field_byte(&self, at: usize, now: Tick, work: &mut impl Work) -> Result<Option<u8>, Error> {
        if at > self.field.len() {
            return Err(Error::InvalidState);
        }
        work.charge(
            now,
            Charge {
                io_bytes: u64::from(at < self.field.len()),
                ..Charge::default()
            },
        )?;
        Ok(self.field.get(at).copied())
    }
    fn separated(&self, now: Tick, work: &mut impl Work) -> Result<bool, Error> {
        let start = self
            .name
            .start
            .checked_add(self.token.start)
            .ok_or(Error::InvalidState)?;
        if let Some(previous) = start.checked_sub(1) {
            if !matches!(self.field_byte(previous, now, work)?, Some(b' ' | b'\t')) {
                return Ok(false);
            }
        }
        let end = self
            .name
            .start
            .checked_add(self.token.end)
            .ok_or(Error::InvalidState)?;
        let next = match self.field_byte(end, now, work)? {
            None | Some(b' ' | b'\t') => return Ok(true),
            Some(b'\r') => {
                let next = end.checked_add(1).ok_or(Error::InvalidState)?;
                if self.field_byte(next, now, work)? != Some(b'\n') {
                    return Ok(false);
                }
                next.checked_add(1).ok_or(Error::InvalidState)?
            }
            Some(b'\n') => end.checked_add(1).ok_or(Error::InvalidState)?,
            _ => return Ok(false),
        };
        Ok(matches!(
            self.field_byte(next, now, work)?,
            Some(b' ' | b'\t')
        ))
    }
    // Unquote first, then unfold the logical bytes, including escaped CR/LF.
    fn atom(
        &self,
        at: usize,
        now: Tick,
        work: &mut impl Work,
    ) -> Result<Option<(u8, usize)>, Error> {
        td_header::projection::atom(at, self.kind == Kind::Quoted, |at| {
            if at == self.token.end {
                return Ok(None);
            }
            if at > self.token.end {
                return Err(Error::InvalidState);
            }
            self.byte(at, now, work)?
                .ok_or(Error::InvalidState)
                .map(Some)
        })
        .map(|atom| atom.map(|atom| (atom.value, atom.next)))
        .map_err(|error| match error {
            td_header::projection::Error::Read(error) => error,
            td_header::projection::Error::IncompletePair
            | td_header::projection::Error::InvalidState => Error::InvalidState,
        })
    }
    fn literal(&mut self, now: Tick, work: &mut impl Work) -> Result<Status, Error> {
        let Some((first, mut next)) = self.atom(self.scan, now, work)? else {
            self.phase = Phase::Replay(self.resume.replay());
            return Ok(Status::Yield);
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
            let (byte, end) = self.atom(next, now, work)?.ok_or(Error::InvalidState)?;
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
        self.scan = next;
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
        match &mut self.phase {
            Phase::Classify(cursor) => {
                match cursor.poll_with_work(now, work)? {
                    TokenStatus::Yield => {}
                    TokenStatus::Token(token) if self.first && token.kind == Kind::Quoted => {
                        self.first = false;
                        self.sole_quoted = true;
                    }
                    TokenStatus::Token(_) => {
                        self.first = true;
                        self.sole_quoted = false;
                        self.phase = Phase::Replay(self.resume.replay());
                    }
                    TokenStatus::Complete(_) => {
                        self.first = true;
                        self.phase = Phase::Replay(self.resume.replay());
                    }
                }
                Ok(Status::Yield)
            }
            Phase::Replay(cursor) => {
                match cursor.poll_with_work(now, work)? {
                    TokenStatus::Yield => {}
                    TokenStatus::Complete(_) => {
                        self.phase = Phase::Complete;
                        return Ok(Status::Complete);
                    }
                    TokenStatus::Token(token) => {
                        self.resume = cursor.checkpoint()?;
                        self.token = token.text;
                        self.scan = token.leading.start;
                        self.kind = token.kind;
                        self.gap = !self.first && token.leading.start < token.leading.end;
                        self.pure_lws = true;
                        self.phase = Phase::Gap;
                    }
                }
                Ok(Status::Yield)
            }
            Phase::Gap => {
                for _ in 0..32 {
                    if self.scan == self.token.start {
                        self.phase = Phase::Recognize;
                        break;
                    }
                    work.charge(
                        now,
                        Charge {
                            records: 1,
                            ..Charge::default()
                        },
                    )?;
                    let byte = self
                        .byte(self.scan, now, work)?
                        .ok_or(Error::InvalidState)?;
                    self.pure_lws &= matches!(byte, b' ' | b'\t' | b'\r' | b'\n');
                    self.scan = self.scan.checked_add(1).ok_or(Error::InvalidState)?;
                }
                Ok(Status::Yield)
            }
            Phase::Recognize => {
                let word = if self.kind == Kind::Atom && self.separated(now, work)? {
                    let token = self
                        .source()?
                        .get(self.token.start..self.token.end)
                        .ok_or(Error::InvalidState)?;
                    Word::recognize_with_work(token, Context::Phrase, now, work)?
                } else {
                    None
                };
                let space = self.gap && !(self.pure_lws && self.previous_word && word.is_some());
                self.previous_word = word.is_some();
                self.first = false;
                if let Some(word) = word {
                    self.phase = Phase::Word(encoded_word::decode::Cursor::new(word));
                } else {
                    if self.kind == Kind::Quoted {
                        self.token.start =
                            self.token.start.checked_add(1).ok_or(Error::InvalidState)?;
                        self.token.end =
                            self.token.end.checked_sub(1).ok_or(Error::InvalidState)?;
                    }
                    self.scan = self.token.start;
                    self.phase = if self.sole_quoted {
                        Phase::Trim {
                            last: self.token.start,
                        }
                    } else {
                        Phase::Literal
                    };
                }
                if space {
                    Ok(Status::Scalar(' '))
                } else {
                    Ok(Status::Yield)
                }
            }
            Phase::Trim { last } => {
                let mut last = *last;
                match self.atom(self.scan, now, work)? {
                    Some((byte, next)) => {
                        if matches!(byte, b' ' | b'\t' | 0) {
                            if self.scan == self.token.start {
                                self.token.start = next;
                            }
                        } else {
                            last = next;
                        }
                        self.scan = next;
                        self.phase = Phase::Trim { last };
                    }
                    None => {
                        self.token.end = last.max(self.token.start);
                        self.phase = Phase::Literal;
                        self.scan = self.token.start;
                    }
                }
                Ok(Status::Yield)
            }
            Phase::Literal => self.literal(now, work),
            Phase::Word(word) => {
                let status = word.poll_with_work(now, work)?;
                self.problem |= word.is_encoding_problem();
                if status == Status::Complete {
                    self.phase = Phase::Replay(self.resume.replay());
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
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::{admission::work::Stop, ports::Deadline};
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
    fn cursor(field: &[u8], name: Extent) -> Cursor<'_> {
        let mut parser = super::super::Cursor::new(&field[name.start..name.end]);
        let mut meter = work();
        while !matches!(
            parser.poll(Tick(1), &mut meter).unwrap(),
            TokenStatus::Complete(_)
        ) {}
        Cursor::new(parser.into_validated().unwrap(), field, name).unwrap()
    }
    fn collect(mut cursor: Cursor<'_>, meter: &mut Meter) -> (String, bool) {
        let mut text = String::new();
        assert!(
            std::mem::size_of_val(&cursor) <= 224,
            "cursor size {}",
            std::mem::size_of_val(&cursor)
        );
        for _ in 0..2_000_000 {
            let before = meter.remaining();
            let status = cursor.poll(Tick(1), meter).unwrap();
            let after = meter.remaining();
            assert!(before.io_bytes - after.io_bytes <= 230);
            assert!(before.records - after.records <= 227);
            assert_eq!(before.output_bytes, after.output_bytes);
            match status {
                Status::Yield => {}
                Status::Scalar(value) => text.push(value),
                Status::Complete => {
                    assert_eq!(cursor.poll(Tick(100), meter), Ok(Status::Complete));
                    assert_eq!(meter.remaining(), after);
                    return (text, cursor.is_encoding_problem());
                }
            }
        }
        panic!("phrase decoding did not finish");
    }
    fn text(source: &[u8]) -> (String, bool) {
        collect(
            cursor(
                source,
                Extent {
                    start: 0,
                    end: source.len(),
                },
            ),
            &mut work(),
        )
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
        let source = b"\"\\\r\\\n\\ x\"";
        let mut cursor = cursor(
            source,
            Extent {
                start: 0,
                end: source.len(),
            },
        );
        cursor.token = Extent {
            start: 1,
            end: source.len() - 1,
        };
        cursor.kind = Kind::Quoted;
        let mut counts = Counts::default();
        assert_eq!(
            cursor.atom(1, Tick(1), &mut counts).unwrap(),
            Some((b' ', 7))
        );
        assert_eq!((counts.calls, counts.visits), (6, 6));
        let before = counts.calls;
        assert_eq!(
            cursor.atom(source.len() - 1, Tick(1), &mut counts).unwrap(),
            None
        );
        assert_eq!(counts.calls - before, 0);
        assert_eq!(counts.visits, 6);
    }
    #[test]
    fn literal_names_unquote_unfold_and_trim_only_a_sole_quoted_word() {
        for (source, expected) in [
            ("  John (ignore) Doe. (tail)", "John Doe."),
            ("a\"b\"..c", "ab..c"),
            ("\"  James\\ Smythe \t\"", "James Smythe"),
            ("\"a\r\n \tb\"", "a \tb"),
            ("\" \t\r\n \"", ""),
            ("\"\"", ""),
            ("\" a \" b", " a  b"),
            ("\"\\\0 a\"", "a"),
            ("\"a \\\0\"", "a"),
            ("\"a\\\r\n b\"", "a b"),
            ("\"\\\0\\\u{1}\"", "\u{1}"),
            ("\"=?utf-8?q?one?=\"", "=?utf-8?q?one?="),
            ("e\u{301}例", "e\u{301}例"),
            ("a(comment)b", "a b"),
        ] {
            assert_eq!(
                text(source.as_bytes()),
                (expected.to_owned(), false),
                "{source:?}"
            );
        }
        assert_eq!(text("\u{fdd0}".as_bytes()), ("�".to_owned(), true));
        let long = format!("\" {} \"", "🐈".repeat(10_000));
        assert_eq!(text(long.as_bytes()), ("🐈".repeat(10_000), false));
    }
    #[test]
    fn encoded_words_require_actual_placement_and_only_suppress_pure_lws() {
        for (source, expected, problem) in [
            ("=?utf-8?q?one?= \r\n\t=?utf-8?b?dHdv?=", "onetwo", false),
            ("=?utf-8?q?one?= (x) =?utf-8?q?two?=", "one two", false),
            (
                "=?utf-8?q?one?=(x)=?utf-8?q?two?=",
                "=?utf-8?q?one?= =?utf-8?q?two?=",
                false,
            ),
            ("=?utf-8?q?one?=.", "=?utf-8?q?one?=.", false),
            (
                "=?utf-8?q?one?= =?unknown?q?two?=",
                "one =?unknown?q?two?=",
                false,
            ),
            ("=?utf-8?q?one?= \"two\"", "one two", false),
            ("=?utf-8?q?=00=09=7F?=", "", false),
            ("=?utf-8?q?=EF=B7=90?=", "�", true),
            ("=?utf-8?q?=FF?=", "�", true),
            ("=?utf-8?q?_name_?=", " name ", false),
        ] {
            assert_eq!(
                text(source.as_bytes()),
                (expected.to_owned(), problem),
                "{source}"
            );
        }
        for fold in ["\r\n ", "\n "] {
            assert_eq!(
                text(format!("=?utf-8?q?one?={fold}=?utf-8?q?two?=").as_bytes()),
                ("onetwo".to_owned(), false)
            );
        }
        let word = "=?utf-8?q?one?=";
        for (prefix, suffix, expected) in [
            ("", "<a@b>", word),
            ("", ":a@b;", word),
            ("", " <a@b>", "one"),
            ("", " :a@b;", "one"),
            ("a@b,", " <c@d>", word),
            ("a@b, ", " <c@d>", "one"),
        ] {
            let field = format!("{prefix}{word}{suffix}");
            let source = cursor(
                field.as_bytes(),
                Extent {
                    start: prefix.len(),
                    end: prefix.len() + word.len(),
                },
            );
            assert_eq!(
                collect(source, &mut work()),
                (expected.to_owned(), false),
                "{field}"
            );
        }
    }
    #[test]
    fn exact_work_counts_classification_replay_trimming_and_literal_conversion() {
        for (source, io, records) in [("a", 6, 17), ("\"\"", 4, 14), ("\" a \"", 15, 24)] {
            let mut cursor = cursor(
                source.as_bytes(),
                Extent {
                    start: 0,
                    end: source.len(),
                },
            );
            let mut meter = work();
            let before = meter.remaining();
            while cursor.poll(Tick(1), &mut meter).unwrap() != Status::Complete {}
            assert_eq!(before.io_bytes - meter.remaining().io_bytes, io, "{source}");
            assert_eq!(
                before.records - meter.remaining().records,
                records,
                "{source}"
            );
        }
    }
    #[test]
    fn aggregate_interpretation_refusal_survives_child_replay_and_copy() {
        struct RefuseAfterParent(bool);
        impl Work for RefuseAfterParent {
            fn charge(&mut self, _: Tick, _: Charge) -> Result<(), crate::decode_work::Error> {
                if self.0 {
                    return Err(crate::decode_work::Error::InterpretationLimit);
                }
                self.0 = true;
                Ok(())
            }
        }
        let mut cursor = cursor(b"name", Extent { start: 0, end: 4 });
        assert_eq!(
            cursor.poll_with_work(Tick(1), &mut RefuseAfterParent(false)),
            Err(Error::InterpretationLimit)
        );
        let mut copy = cursor;
        let mut meter = work();
        let before = meter.remaining();
        assert_eq!(
            copy.poll(Tick(1), &mut meter),
            Err(Error::InterpretationLimit)
        );
        assert_eq!(meter.remaining(), before);
    }
    #[test]
    fn proof_must_bind_the_actual_field_extent() {
        let first = b"name".to_vec();
        let second = first.clone();
        let mut parser = super::super::Cursor::new(&first);
        while !matches!(
            parser.poll(Tick(1), &mut work()).unwrap(),
            TokenStatus::Complete(_)
        ) {}
        let proof = parser.into_validated().unwrap();
        assert!(matches!(
            Cursor::new(proof, &second, Extent { start: 0, end: 4 }),
            Err(Error::InvalidState)
        ));
        assert!(matches!(
            Cursor::new(proof, &first, Extent { start: 1, end: 4 }),
            Err(Error::InvalidState)
        ));
    }
    #[test]
    fn copied_positions_reproduce_suffixes_and_spend_the_live_meter() {
        for source in [
            "=?utf-8?q?e=CC=81?= =?utf-8?q?x?=",
            "\" \ta\\\"e\r\n  \"",
            "word(comment)\"name\"",
        ] {
            let mut source_cursor = cursor(
                source.as_bytes(),
                Extent {
                    start: 0,
                    end: source.len(),
                },
            );
            let expected = text(source.as_bytes()).0;
            let mut emitted = String::new();
            let mut meter = work();
            loop {
                let before = meter.remaining();
                let copy = source_cursor;
                let (suffix, _) = collect(copy, &mut meter);
                assert_eq!(format!("{emitted}{suffix}"), expected);
                assert!(meter.remaining().records < before.records);
                match source_cursor.poll(Tick(1), &mut meter).unwrap() {
                    Status::Yield => {}
                    Status::Scalar(value) => emitted.push(value),
                    Status::Complete => break,
                }
            }
        }
    }
    #[test]
    fn failures_retire_progress_and_copies_under_replacement_meters() {
        let input = b"=?utf-8?q?one?= (comment) \"two\"";
        let initial = cursor(
            input,
            Extent {
                start: 0,
                end: input.len(),
            },
        );
        let mut cursor = initial;
        let mut meter = work();
        let mut emitted = false;
        for _ in 0..1000 {
            for (io_bytes, records, now, expected) in [
                (0, 1000, Tick(1), Stop::IoBytes),
                (1000, 0, Tick(1), Stop::Records),
                (1000, 1000, Tick(100), Stop::Deadline),
            ] {
                let mut copied = cursor;
                let mut limited = Meter::new(
                    Deadline::after(Tick(0), 100).unwrap(),
                    Charge {
                        io_bytes,
                        records,
                        ..Charge::default()
                    },
                );
                for _ in 0..1000 {
                    match copied.poll(now, &mut limited) {
                        Ok(Status::Complete) => break,
                        Ok(_) => {}
                        Err(error) => {
                            assert_eq!(error, Error::Work(expected));
                            let before = meter.remaining();
                            let mut failed_copy = copied;
                            assert_eq!(failed_copy.poll(Tick(1), &mut meter), Err(error));
                            assert_eq!(meter.remaining(), before);
                            break;
                        }
                    }
                }
            }
            match cursor.poll(Tick(1), &mut meter).unwrap() {
                Status::Yield => {}
                Status::Scalar(_) => emitted = true,
                Status::Complete => {
                    assert!(emitted);
                    return;
                }
            }
        }
        panic!("failure traversal did not finish");
    }
}

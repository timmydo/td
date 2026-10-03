//! Shared raw identifier grammar; public construction parses MessageIds lists.
mod budgeted;
pub mod project;
use crate::{
    admission::work::{Charge, Meter, Stop},
    decode_work::Work,
    header_cfws, header_delimited,
    ports::Tick,
};
pub use budgeted::Budgeted;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Mode {
    Strict,
    /// Public selection is restricted to References and In-Reply-To.
    ObsoletePhrases,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Extent {
    pub start: usize,
    pub end: usize,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    Yield,
    Begin,
    Part(Extent),
    End,
    Complete,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Malformed,
    NestingLimit,
    Work(Stop),
    InterpretationLimit,
    InvalidState,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed => f.write_str("malformed message-id list"),
            Self::NestingLimit => f.write_str("message-id comment nesting limit"),
            Self::Work(error) => write!(f, "message-id work: {error}"),
            Self::InterpretationLimit => f.write_str("header interpretation limit"),
            Self::InvalidState => f.write_str("invalid message-id cursor state"),
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
impl From<Stop> for Error {
    fn from(error: Stop) -> Self {
        Self::Work(error)
    }
}
#[derive(Clone, Copy)]
enum Grammar {
    Between,
    LeftWord,
    LeftTail,
    RightStart,
    RightAtom,
    RightTail,
    LiteralTail,
}
#[derive(Clone, Copy)]
enum Phase {
    Cfws,
    Syntax,
    Atom { start: usize, emit: bool },
    Delimited { emit: bool },
    Complete,
}
#[derive(Clone, Copy, Eq, PartialEq)]
enum Purpose {
    MessageIds,
    AddrSpec,
    Phrase,
    RouteDomain,
}
/// Every Begin/Part/End is provisional until Complete validates the full input.
/// Public list input ends at field value_end; private inputs are single candidates.
pub struct Cursor<'a> {
    source: &'a [u8],
    mode: Mode,
    purpose: Purpose,
    position: usize,
    grammar: Grammar,
    phase: Phase,
    cfws: Option<header_cfws::Cursor<'a>>,
    delimited: Option<header_delimited::Cursor<'a>>,
    any: bool,
    phrase: bool,
    failure: Option<Error>,
}
impl<'a> Cursor<'a> {
    pub const fn new(source: &'a [u8], mode: Mode) -> Self {
        Self {
            source,
            mode,
            purpose: Purpose::MessageIds,
            position: 0,
            grammar: Grammar::Between,
            phase: Phase::Cfws,
            cfws: None,
            delimited: None,
            any: false,
            phrase: false,
            failure: None,
        }
    }
    // Enter the shared local/domain grammar without copying enclosing angles.
    pub(crate) const fn addr_spec(source: &'a [u8]) -> Self {
        let mut cursor = Self::new(source, Mode::Strict);
        cursor.purpose = Purpose::AddrSpec;
        cursor.grammar = Grammar::LeftWord;
        cursor
    }
    // Reuse word and CFWS parsing while retaining tokens for display decoding.
    pub(crate) const fn phrase(source: &'a [u8]) -> Self {
        let mut cursor = Self::new(source, Mode::ObsoletePhrases);
        cursor.purpose = Purpose::Phrase;
        cursor
    }
    // A route domain ends at its own comma or the route slice's colon boundary.
    pub(crate) const fn route_domain(source: &'a [u8]) -> Self {
        let mut cursor = Self::new(source, Mode::Strict);
        cursor.purpose = Purpose::RouteDomain;
        cursor.grammar = Grammar::RightStart;
        cursor
    }
    pub(crate) fn route_domain_end(&self) -> Option<usize> {
        (self.purpose == Purpose::RouteDomain && matches!(self.phase, Phase::Complete))
            .then_some(self.position)
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
        let result = self.step(now, work);
        if let Err(error) = result {
            self.failure = Some(error);
        }
        result
    }
    fn step(&mut self, now: Tick, work: &mut impl Work) -> Result<Status, Error> {
        match self.phase {
            Phase::Cfws => {
                let cursor = self
                    .cfws
                    .get_or_insert_with(|| header_cfws::Cursor::new(self.source, self.position));
                let status = cursor
                    .poll_with_work(now, work)
                    .map_err(|error| match error {
                        header_cfws::Error::Malformed => Error::Malformed,
                        header_cfws::Error::NestingLimit => Error::NestingLimit,
                        header_cfws::Error::Work(error) => Error::Work(error),
                        header_cfws::Error::InvalidState => Error::InvalidState,
                        header_cfws::Error::InterpretationLimit => Error::InterpretationLimit,
                    })?;
                if let header_cfws::Status::Complete(end) = status {
                    self.position = end.position;
                    self.cfws = None;
                    self.phase = Phase::Syntax;
                }
                Ok(Status::Yield)
            }
            Phase::Syntax => self.syntax(now, work),
            Phase::Atom { start, emit } => self.atom(start, emit, now, work),
            Phase::Delimited { emit } => {
                let cursor = self.delimited.as_mut().ok_or(Error::InvalidState)?;
                let status = cursor
                    .poll_with_work(now, work)
                    .map_err(|error| match error {
                        header_delimited::Error::Malformed => Error::Malformed,
                        header_delimited::Error::Work(error) => Error::Work(error),
                        header_delimited::Error::InvalidState => Error::InvalidState,
                        header_delimited::Error::InterpretationLimit => Error::InterpretationLimit,
                    })?;
                match status {
                    header_delimited::Status::Yield => Ok(Status::Yield),
                    header_delimited::Status::Complete(extent) => {
                        self.position = extent.end;
                        self.delimited = None;
                        self.finish_word();
                        Ok(if emit {
                            Status::Part(Extent {
                                start: extent.start,
                                end: extent.end,
                            })
                        } else {
                            Status::Yield
                        })
                    }
                }
            }
            Phase::Complete => Ok(Status::Complete),
        }
    }
    fn visit(&self, now: Tick, work: &mut impl Work) -> Result<Option<u8>, Error> {
        work.charge(
            now,
            Charge {
                records: 1,
                io_bytes: u64::from(self.position < self.source.len()),
                ..Charge::default()
            },
        )?;
        Ok(self.source.get(self.position).copied())
    }
    fn advance(&mut self, width: usize) -> Result<(), Error> {
        self.position = self
            .position
            .checked_add(width)
            .ok_or(Error::InvalidState)?;
        if self.position > self.source.len() {
            return Err(Error::InvalidState);
        }
        Ok(())
    }
    fn punctuation(&mut self, next: Grammar) -> Result<Status, Error> {
        let start = self.position;
        self.advance(1)?;
        self.grammar = next;
        self.phase = Phase::Cfws;
        Ok(Status::Part(Extent {
            start,
            end: self.position,
        }))
    }
    fn word(&mut self, kind: Option<header_delimited::Kind>, next: Grammar, emit: bool) -> Status {
        self.grammar = next;
        if let Some(kind) = kind {
            self.delimited = Some(header_delimited::Cursor::new(
                self.source,
                self.position,
                kind,
            ));
            self.phase = Phase::Delimited { emit };
        } else {
            self.phase = Phase::Atom {
                start: self.position,
                emit,
            };
        }
        Status::Yield
    }
    fn syntax(&mut self, now: Tick, work: &mut impl Work) -> Result<Status, Error> {
        use header_delimited::Kind;
        let byte = self.visit(now, work)?;
        match (self.grammar, byte) {
            (Grammar::Between, None)
                if self.any
                    || (self.purpose == Purpose::MessageIds
                        && self.mode == Mode::ObsoletePhrases
                        && self.source.is_empty()) =>
            {
                self.phase = Phase::Complete;
                Ok(Status::Complete)
            }
            (Grammar::Between, Some(b'<')) if self.purpose == Purpose::MessageIds => {
                self.advance(1)?;
                self.grammar = Grammar::LeftWord;
                self.phase = Phase::Cfws;
                self.phrase = false;
                Ok(Status::Begin)
            }
            (Grammar::Between, Some(byte)) if self.mode == Mode::ObsoletePhrases => {
                if byte == b'.' && self.phrase {
                    let part = self.punctuation(Grammar::Between)?;
                    Ok(if self.purpose == Purpose::Phrase {
                        part
                    } else {
                        Status::Yield
                    })
                } else if byte == b'"' {
                    Ok(self.word(
                        Some(Kind::QuotedString),
                        Grammar::Between,
                        self.purpose == Purpose::Phrase,
                    ))
                } else if atext(byte) {
                    Ok(self.word(None, Grammar::Between, self.purpose == Purpose::Phrase))
                } else {
                    Err(Error::Malformed)
                }
            }
            (Grammar::LeftWord, Some(b'"')) => {
                Ok(self.word(Some(Kind::QuotedString), Grammar::LeftTail, true))
            }
            (Grammar::LeftWord, Some(byte)) if atext(byte) => {
                Ok(self.word(None, Grammar::LeftTail, true))
            }
            (Grammar::LeftTail, Some(b'.')) => self.punctuation(Grammar::LeftWord),
            (Grammar::LeftTail, Some(b'@')) => self.punctuation(Grammar::RightStart),
            (Grammar::RightStart, Some(b'[')) => {
                Ok(self.word(Some(Kind::DomainLiteral), Grammar::LiteralTail, true))
            }
            (Grammar::RightStart | Grammar::RightAtom, Some(byte)) if atext(byte) => {
                Ok(self.word(None, Grammar::RightTail, true))
            }
            (Grammar::RightTail, Some(b'.')) => self.punctuation(Grammar::RightAtom),
            (Grammar::RightTail | Grammar::LiteralTail, None)
                if matches!(self.purpose, Purpose::AddrSpec | Purpose::RouteDomain) =>
            {
                self.phase = Phase::Complete;
                Ok(Status::Complete)
            }
            (Grammar::RightTail | Grammar::LiteralTail, Some(b','))
                if self.purpose == Purpose::RouteDomain =>
            {
                self.phase = Phase::Complete;
                Ok(Status::Complete)
            }
            (Grammar::RightTail | Grammar::LiteralTail, Some(b'>'))
                if self.purpose == Purpose::MessageIds =>
            {
                self.advance(1)?;
                self.grammar = Grammar::Between;
                self.phase = Phase::Cfws;
                self.any = true;
                Ok(Status::End)
            }
            _ => Err(Error::Malformed),
        }
    }
    fn finish_word(&mut self) {
        self.phase = Phase::Cfws;
        if matches!(self.grammar, Grammar::Between) {
            self.phrase = true;
            self.any = true;
        }
    }
    fn atom(
        &mut self,
        start: usize,
        emit: bool,
        now: Tick,
        work: &mut impl Work,
    ) -> Result<Status, Error> {
        for _ in 0..32 {
            let byte = self.visit(now, work)?;
            if !byte.is_some_and(atext) {
                if self.position == start {
                    return Err(Error::Malformed);
                }
                self.finish_word();
                return Ok(if emit {
                    Status::Part(Extent {
                        start,
                        end: self.position,
                    })
                } else {
                    Status::Yield
                });
            }
            let byte = byte.ok_or(Error::InvalidState)?;
            let width = if byte.is_ascii() {
                1
            } else {
                let width = match byte {
                    0xc2..=0xdf => 2,
                    0xe0..=0xef => 3,
                    0xf0..=0xf4 => 4,
                    _ => return Err(Error::Malformed),
                };
                let available = self
                    .source
                    .len()
                    .checked_sub(self.position)
                    .ok_or(Error::InvalidState)?
                    .min(width);
                work.charge(
                    now,
                    Charge {
                        io_bytes: available as u64,
                        ..Charge::default()
                    },
                )?;
                let end = self
                    .position
                    .checked_add(available)
                    .ok_or(Error::InvalidState)?;
                let bytes = self
                    .source
                    .get(self.position..end)
                    .ok_or(Error::InvalidState)?;
                if available != width || std::str::from_utf8(bytes).is_err() {
                    return Err(Error::Malformed);
                }
                width
            };
            self.advance(width)?;
        }
        Ok(Status::Yield)
    }
}
pub(crate) fn atext(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || byte >= 128
        || matches!(
            byte,
            b'!' | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'-'
                | b'/'
                | b'='
                | b'?'
                | b'^'
                | b'_'
                | b'`'
                | b'{'
                | b'|'
                | b'}'
                | b'~'
        )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::ports::Deadline;
    fn work() -> Meter {
        Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: 10_000_000,
                records: 10_000_000,
                ..Charge::default()
            },
        )
    }
    fn collect(source: &[u8], mode: Mode) -> Result<Vec<Vec<u8>>, Error> {
        let mut cursor = Cursor::new(source, mode);
        assert!(std::mem::size_of_val(&cursor) <= 256);
        let mut work = work();
        let mut result = Vec::new();
        let mut current = None;
        for _ in 0..100_000 {
            let before = work.remaining();
            let status = cursor.poll(Tick(1), &mut work);
            let after = work.remaining();
            assert!(before.io_bytes - after.io_bytes <= 160);
            assert!(before.records - after.records <= 32);
            assert_eq!(before.output_bytes, after.output_bytes);
            match status? {
                Status::Yield => {}
                Status::Begin => {
                    assert!(current.is_none());
                    current = Some(Vec::new());
                }
                Status::Part(extent) => {
                    current
                        .as_mut()
                        .unwrap()
                        .extend_from_slice(&source[extent.start..extent.end]);
                }
                Status::End => result.push(current.take().unwrap()),
                Status::Complete => {
                    assert!(current.is_none());
                    assert_eq!(cursor.poll(Tick(100), &mut work), Ok(Status::Complete));
                    assert_eq!(work.remaining(), after);
                    return Ok(result);
                }
            }
        }
        panic!("message-id list did not finish");
    }
    #[test]
    fn literal_lists_strip_only_grammatical_cfws_and_outer_angles() {
        for (source, expected) in [
            ("<a@b>", vec!["a@b"]),
            (
                " (outer) < (l) a (c) . b @ c (d) . e > (end)",
                vec!["a.b@c.e"],
            ),
            ("<a@b><c@d>", vec!["a@b", "c@d"]),
            ("<a@b>\r\n (two) <c@d>", vec!["a@b", "c@d"]),
            (
                r#"<" a (b) . @ \" c".d@[ x (y) \] z ]>"#,
                vec![r#"" a (b) . @ \" c".d@[ x (y) \] z ]"#],
            ),
            ("<\"\"@[]>", vec!["\"\"@[]"]),
            (
                "<a@b> (🐈 (nested)) <é@例.テスト>",
                vec!["a@b", "é@例.テスト"],
            ),
            ("<\"a\r\n b\"@[c\n\td]>", vec!["\"a\r\n b\"@[c\n\td]"]),
            ("<\"\\\0\"@[\\\r]>", vec!["\"\\\0\"@[\\\r]"]),
            ("<\u{fdd0}@\u{10ffff}>", vec!["\u{fdd0}@\u{10ffff}"]),
            (
                "<!#$%&'*+-/=?^_`{|}~@EXAMPLE>",
                vec!["!#$%&'*+-/=?^_`{|}~@EXAMPLE"],
            ),
        ] {
            let expected: Vec<_> = expected
                .iter()
                .map(|value| value.as_bytes().to_vec())
                .collect();
            for mode in [Mode::Strict, Mode::ObsoletePhrases] {
                assert_eq!(
                    collect(source.as_bytes(), mode),
                    Ok(expected.clone()),
                    "{source:?}"
                );
            }
        }
    }
    #[test]
    fn obsolete_phrases_are_explicitly_authorized_and_discarded() {
        for (source, expected) in [
            (
                "old phrase <a@b> trailing.words... <c@d>",
                vec!["a@b", "c@d"],
            ),
            ("\"fake <x@y>\" <real@id>", vec!["real@id"]),
            ("only phrase", vec![]),
            ("", vec![]),
            ("word. (x) .", vec![]),
            ("\"\"", vec![]),
            ("<a@b>\"old\"<c@d>", vec!["a@b", "c@d"]),
            ("=?utf-8?Q?legacy?= <x@y>", vec!["x@y"]),
        ] {
            let expected = expected
                .iter()
                .map(|value| value.as_bytes().to_vec())
                .collect();
            assert_eq!(
                collect(source.as_bytes(), Mode::ObsoletePhrases),
                Ok(expected),
                "{source:?}"
            );
            assert_eq!(
                collect(source.as_bytes(), Mode::Strict),
                Err(Error::Malformed),
                "{source:?}"
            );
        }
        for source in [
            b". word".as_slice(),
            b"<a@b> .",
            b"a@b",
            b"a,b",
            b"(only)",
            b"\xff <a@b>",
            b"\xc3 <a@b>",
            b"\"\xff\" <a@b>",
            b" ",
            b"\t",
            b"\r\n ",
        ] {
            assert_eq!(
                collect(source, Mode::ObsoletePhrases),
                Err(Error::Malformed),
                "{source:?}"
            );
        }
    }
    #[test]
    fn raw_parts_need_unfolding_before_identifier_comparison() {
        use crate::mime_unfold::{Decoder, Status as UnfoldStatus};
        for (source, expected) in [
            (
                b"<\"a\r\n b\"@[c\n\td]>".as_slice(),
                b"\"a b\"@[c\td]".as_slice(),
            ),
            (b"<\"a b\"@[c\td]>", b"\"a b\"@[c\td]"),
            (b"<\"a\\\r\n b\"@c>", b"\"a\\ b\"@c"),
        ] {
            let values = collect(source, Mode::Strict).unwrap();
            assert_eq!(values.len(), 1);
            let raw = &values[0];
            let mut decoder = Decoder::default();
            let mut output = [0; 64];
            let mut work = Meter::new(
                Deadline::after(Tick(0), 100).unwrap(),
                Charge {
                    io_bytes: 1000,
                    output_bytes: 1000,
                    records: 1000,
                    ..Charge::default()
                },
            );
            let progress = decoder
                .poll(raw, &mut output, true, Tick(1), &mut work)
                .unwrap();
            assert_eq!(progress.status, UnfoldStatus::Complete);
            assert_eq!(progress.consumed, raw.len());
            assert_eq!(&output[..progress.written], expected);
            assert_eq!(1000 - work.remaining().output_bytes, expected.len() as u64);
        }
    }
    #[test]
    fn bad_structure_unicode_and_late_tails_invalidate_the_whole_list() {
        for source in [
            b"".as_slice(),
            b" ",
            b"(only)",
            b"a@b",
            b"<>",
            b"<@b>",
            b"<a@>",
            b"<a@b",
            b"<a@b>>",
            b"<a b@c>",
            b"<a..b@c>",
            b"<.a@b>",
            b"<a.@b>",
            b"<a@.b>",
            b"<a@b.>",
            b"<a@b..c>",
            b"<a@b.[c]>",
            b"<a@[b].c>",
            b"<a@\"b\">",
            b"<a@[[b]]>",
            b"<a@b>,<c@d>",
            b"<a@b>junk",
            b"<a@b>(unclosed",
            b"<a@b><bad>",
            b"<a@b>\r",
            b"<a@b>\nx",
            b"<a\0@b>",
            b"<a@b\0>",
            b"<\xff@b>",
            b"<a@\xed\xa0\x80>",
            b"<\xc0\x80@b>",
            b"<a@\xf4\x90\x80\x80>",
            b"<a@\xe2\x82",
            b"<\"a\\\"@b>",
            b"<a@[b\\]>",
            b"<a(b)z@d>",
            b"<a@b(c)d>",
        ] {
            assert_eq!(
                collect(source, Mode::Strict),
                Err(Error::Malformed),
                "{source:?}"
            );
        }
    }
    #[test]
    fn exact_charges_long_atoms_and_nested_comments_are_bounded() {
        for (source, bytes) in [("<a@b>", 14), ("<🐈@b>", 18)] {
            let mut cursor = Cursor::new(source.as_bytes(), Mode::Strict);
            let mut work = work();
            let before = work.remaining();
            for _ in 0..100 {
                if cursor.poll(Tick(1), &mut work).unwrap() == Status::Complete {
                    break;
                }
            }
            assert_eq!(cursor.poll(Tick(1), &mut work), Ok(Status::Complete));
            assert_eq!(before.io_bytes - work.remaining().io_bytes, bytes);
            assert_eq!(before.records - work.remaining().records, 16);
        }
        let local = "🐈".repeat(10_000);
        let source = format!("<{local}@b>");
        assert_eq!(
            collect(source.as_bytes(), Mode::Strict),
            Ok(vec![format!("{local}@b").into_bytes()])
        );
        let good = format!("{}{}<a@b>", "(".repeat(32), ")".repeat(32));
        assert_eq!(
            collect(good.as_bytes(), Mode::Strict),
            Ok(vec![b"a@b".to_vec()])
        );
        let bad = format!("{}{}<a@b>", "(".repeat(33), ")".repeat(33));
        assert_eq!(
            collect(bad.as_bytes(), Mode::Strict),
            Err(Error::NestingLimit)
        );
    }
    #[test]
    fn provisional_results_and_every_failure_retire_the_cursor() {
        let mut cursor = Cursor::new(b"<a@b><bad>", Mode::Strict);
        let mut work = work();
        let mut ends = 0;
        let error = loop {
            match cursor.poll(Tick(1), &mut work) {
                Ok(Status::End) => ends += 1,
                Ok(Status::Complete) => panic!("accepted malformed tail"),
                Ok(_) => {}
                Err(error) => break error,
            }
        };
        assert_eq!(ends, 1);
        assert_eq!(error, Error::Malformed);
        assert_eq!(cursor.poll(Tick(1), &mut self::work()), Err(error));
        for (bytes, records, now, expected) in [
            (0, 100, 1, Stop::IoBytes),
            (100, 0, 1, Stop::Records),
            (100, 100, 100, Stop::Deadline),
            (5, 100, 1, Stop::IoBytes),
            (100, 8, 1, Stop::Records),
        ] {
            let mut cursor = Cursor::new(b"<a@b>", Mode::Strict);
            let mut limited = Meter::new(
                Deadline::after(Tick(0), 100).unwrap(),
                Charge {
                    io_bytes: bytes,
                    records,
                    ..Charge::default()
                },
            );
            let mut refused = false;
            for _ in 0..100 {
                match cursor.poll(Tick(now), &mut limited) {
                    Err(error) => {
                        assert_eq!(error, Error::Work(expected));
                        refused = true;
                        break;
                    }
                    Ok(Status::Complete) => panic!("budget accepted"),
                    Ok(_) => {}
                }
            }
            assert!(refused);
            assert_eq!(
                cursor.poll(Tick(1), &mut self::work()),
                Err(Error::Work(expected))
            );
        }
    }
}

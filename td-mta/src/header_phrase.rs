//! Raw phrase tokens and exact CFWS context; decoding and publication are external.
pub mod replay;
pub use crate::header_message_ids::Extent;
use crate::{
    admission::work::{Charge, Meter, Stop},
    header_message_ids,
    ports::Tick,
};
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Malformed,
    NestingLimit,
    Work(Stop),
    InvalidState,
}
impl From<header_message_ids::Error> for Error {
    fn from(error: header_message_ids::Error) -> Self {
        match error {
            header_message_ids::Error::Malformed => Self::Malformed,
            header_message_ids::Error::NestingLimit => Self::NestingLimit,
            header_message_ids::Error::Work(stop) => Self::Work(stop),
            header_message_ids::Error::InvalidState => Self::InvalidState,
        }
    }
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed => f.write_str("malformed phrase"),
            Self::NestingLimit => f.write_str("phrase comment nesting limit"),
            Self::Work(error) => write!(f, "phrase work: {error}"),
            Self::InvalidState => f.write_str("invalid phrase cursor state"),
        }
    }
}
impl std::error::Error for Error {}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Kind {
    Atom,
    Quoted,
    Dot,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Token {
    pub leading: Extent,
    pub text: Extent,
    pub kind: Kind,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    Yield,
    Token(Token),
    Complete(Extent),
}
/// A completed grammar check bound to one immutable source, not decoding authority.
///
/// ```compile_fail
/// let _ = td_mta::header_phrase::Validated { source: b"word" };
/// ```
#[derive(Clone, Copy)]
pub struct Validated<'a> {
    source: &'a [u8],
}
impl<'a> Validated<'a> {
    pub const fn replay(self) -> replay::Cursor<'a> {
        replay::Cursor::new(self)
    }
}
/// Each raw token is provisional until Complete validates the whole phrase.
pub struct Cursor<'a> {
    source: &'a [u8],
    inner: header_message_ids::Cursor<'a>,
    end: usize,
    complete: bool,
    failure: Option<Error>,
}
impl<'a> Cursor<'a> {
    pub const fn new(source: &'a [u8]) -> Self {
        Self {
            source,
            inner: header_message_ids::Cursor::phrase(source),
            end: 0,
            complete: false,
            failure: None,
        }
    }
    /// Consume only a completed parser; an earlier token is not a proof.
    pub fn into_validated(self) -> Result<Validated<'a>, Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if !self.complete {
            return Err(Error::InvalidState);
        }
        Ok(Validated {
            source: self.source,
        })
    }
    fn trailing(&self) -> Extent {
        Extent {
            start: self.end,
            end: self.source.len(),
        }
    }
    pub fn poll(&mut self, now: Tick, work: &mut Meter) -> Result<Status, Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if self.complete {
            return Ok(Status::Complete(self.trailing()));
        }
        let result = self.step(now, work);
        if let Err(error) = result {
            self.failure = Some(error);
        }
        result
    }
    fn step(&mut self, now: Tick, work: &mut Meter) -> Result<Status, Error> {
        match self.inner.poll(now, work)? {
            header_message_ids::Status::Yield => Ok(Status::Yield),
            header_message_ids::Status::Complete => {
                if self.end > self.source.len() {
                    return Err(Error::InvalidState);
                }
                self.complete = true;
                Ok(Status::Complete(self.trailing()))
            }
            header_message_ids::Status::Part(text) => {
                if text.start < self.end || text.start >= text.end || text.end > self.source.len() {
                    return Err(Error::InvalidState);
                }
                work.charge(
                    now,
                    Charge {
                        records: 1,
                        io_bytes: 1,
                        ..Charge::default()
                    },
                )
                .map_err(Error::Work)?;
                let kind = match self
                    .source
                    .get(text.start)
                    .copied()
                    .ok_or(Error::InvalidState)?
                {
                    b'"' => Kind::Quoted,
                    b'.' => Kind::Dot,
                    _ => Kind::Atom,
                };
                let leading = Extent {
                    start: self.end,
                    end: text.start,
                };
                self.end = text.end;
                Ok(Status::Token(Token {
                    leading,
                    text,
                    kind,
                }))
            }
            header_message_ids::Status::Begin | header_message_ids::Status::End => {
                Err(Error::InvalidState)
            }
        }
    }
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
    fn parse(source: &[u8]) -> Result<(Vec<Token>, Extent), Error> {
        let mut cursor = Cursor::new(source);
        assert!(std::mem::size_of_val(&cursor) <= 320);
        let mut work = work();
        let mut tokens = Vec::new();
        for _ in 0..1_000_000 {
            let before = work.remaining();
            let step = cursor.poll(Tick(1), &mut work);
            let after = work.remaining();
            assert!(before.io_bytes - after.io_bytes <= 161);
            assert!(before.records - after.records <= 33);
            assert_eq!(before.output_bytes, after.output_bytes);
            match step? {
                Status::Yield => {}
                Status::Token(token) => tokens.push(token),
                Status::Complete(tail) => {
                    assert_eq!(
                        cursor.poll(Tick(100), &mut work),
                        Ok(Status::Complete(tail))
                    );
                    assert_eq!(after, work.remaining());
                    return Ok((tokens, tail));
                }
            }
        }
        panic!("phrase did not finish");
    }
    fn spellings(source: &str) -> Vec<(&str, &str, Kind)> {
        let (tokens, _) = parse(source.as_bytes()).unwrap();
        tokens
            .into_iter()
            .map(|token| {
                (
                    &source[token.leading.start..token.leading.end],
                    &source[token.text.start..token.text.end],
                    token.kind,
                )
            })
            .collect()
    }
    #[test]
    fn words_dots_and_exact_cfws_are_separate_provisional_tokens() {
        let source = " John (c) \"Doe, Jo\". Jr ";
        assert_eq!(
            spellings(source),
            vec![
                (" ", "John", Kind::Atom),
                (" (c) ", "\"Doe, Jo\"", Kind::Quoted),
                ("", ".", Kind::Dot),
                (" ", "Jr", Kind::Atom),
            ]
        );
        let (_, tail) = parse(source.as_bytes()).unwrap();
        assert_eq!(&source[tail.start..tail.end], " ");
        assert_eq!(
            spellings("a\"b\"..c"),
            vec![
                ("", "a", Kind::Atom),
                ("", "\"b\"", Kind::Quoted),
                ("", ".", Kind::Dot),
                ("", ".", Kind::Dot),
                ("", "c", Kind::Atom),
            ]
        );
        assert_eq!(
            spellings("\"\"."),
            vec![("", "\"\"", Kind::Quoted), ("", ".", Kind::Dot)]
        );
    }
    #[test]
    fn encoding_candidates_remain_literal_with_their_placement_context() {
        let word = "=?utf-8?q?name?=";
        let source = format!("{word}(comment){word} \"{word}\"\r\n\t{word}(tail)");
        assert_eq!(
            spellings(&source),
            vec![
                ("", word, Kind::Atom),
                ("(comment)", word, Kind::Atom),
                (" ", "\"=?utf-8?q?name?=\"", Kind::Quoted),
                ("\r\n\t", word, Kind::Atom),
            ]
        );
        let (_, tail) = parse(source.as_bytes()).unwrap();
        assert_eq!(&source[tail.start..tail.end], "(tail)");
        for word in ["e\u{301}", "例", "\u{fdd0}", "\"a\r\n b\\\"c\""] {
            let tokens = spellings(word);
            assert_eq!(tokens.len(), 1);
            assert_eq!(tokens[0].1, word);
        }
        let source = "🐈".repeat(100_000);
        let (tokens, tail) = parse(source.as_bytes()).unwrap();
        assert_eq!(
            tokens,
            vec![Token {
                leading: Extent { start: 0, end: 0 },
                text: Extent {
                    start: 0,
                    end: source.len()
                },
                kind: Kind::Atom
            }]
        );
        assert_eq!(
            tail,
            Extent {
                start: source.len(),
                end: source.len()
            }
        );
    }
    #[test]
    fn complete_phrase_refuses_empty_and_nonphrase_tails() {
        for source in [
            b"".as_slice(),
            b" (comment) ",
            b".word",
            b"a@b",
            b"Name <a@b>",
            b"a,b",
            b"a:b",
            b"a;b",
            b"[x]",
            b"word (unclosed",
            b"word\r\n",
            b"word\0",
            b"word\xff",
        ] {
            assert_eq!(parse(source), Err(Error::Malformed), "{source:?}");
        }
        let (tokens, _) = parse(b"\"\\\0\"").unwrap();
        assert_eq!(tokens[0].kind, Kind::Quoted);
    }
    #[test]
    fn malformed_tail_retires_previously_emitted_tokens() {
        for (source, expected) in [
            (b"a@b".as_slice(), 1),
            (b"\"a\r\n b\".@bad", 2),
            (b"a. (unclosed", 2),
        ] {
            let mut cursor = Cursor::new(source);
            let mut meter = work();
            let mut tokens = 0;
            loop {
                match cursor.poll(Tick(1), &mut meter) {
                    Ok(Status::Yield) => {}
                    Ok(Status::Token(_)) => tokens += 1,
                    Ok(Status::Complete(_)) => panic!("malformed tail completed"),
                    Err(error) => {
                        assert_eq!(error, Error::Malformed);
                        break;
                    }
                }
            }
            assert_eq!(tokens, expected);
        }
    }
    #[test]
    fn exact_work_and_sticky_failures_include_classification() {
        // CFWS + syntax + atom byte/EOF + trailing CFWS/EOF + syntax EOF,
        // plus one charged leading-byte classification per token.
        for (source, io, records) in [("a", 4, 7), ("🐈", 8, 7)] {
            let mut cursor = Cursor::new(source.as_bytes());
            let mut meter = work();
            let before = meter.remaining();
            while !matches!(
                cursor.poll(Tick(1), &mut meter).unwrap(),
                Status::Complete(_)
            ) {}
            assert_eq!(before.io_bytes - meter.remaining().io_bytes, io);
            assert_eq!(before.records - meter.remaining().records, records);
        }
        let nesting = "(".repeat(33);
        for (source, io_bytes, records, now, expected) in [
            (
                b"a".as_slice(),
                3,
                1000,
                Tick(1),
                Error::Work(Stop::IoBytes),
            ),
            (b"a", 1000, 4, Tick(1), Error::Work(Stop::Records)),
            (b"a", 1000, 1000, Tick(100), Error::Work(Stop::Deadline)),
            (nesting.as_bytes(), 1000, 1000, Tick(1), Error::NestingLimit),
            (b"a@b", 1000, 1000, Tick(1), Error::Malformed),
        ] {
            let mut cursor = Cursor::new(source);
            let mut meter = Meter::new(
                Deadline::after(Tick(0), 100).unwrap(),
                Charge {
                    io_bytes,
                    records,
                    ..Charge::default()
                },
            );
            let error = loop {
                match cursor.poll(now, &mut meter) {
                    Ok(Status::Complete(_)) => panic!("bad source/work accepted"),
                    Ok(_) => {}
                    Err(error) => break error,
                }
            };
            assert_eq!(error, expected);
            let mut fresh = work();
            let before = fresh.remaining();
            assert_eq!(cursor.poll(Tick(1), &mut fresh), Err(expected));
            assert_eq!(before, fresh.remaining());
        }
    }
}

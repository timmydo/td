//! Strict resident RFC 2369 lists; complete validation precedes URL bytes.
mod uri;
use crate::{
    admission::work::{Charge, Meter, Stop},
    header_cfws,
    ports::Tick,
};
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Mode {
    URLs,
    ListPost,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    Yield,
    Begin,
    Byte(u8),
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
impl From<Stop> for Error {
    fn from(value: Stop) -> Self {
        Self::Work(value)
    }
}
impl From<header_cfws::Error> for Error {
    fn from(value: header_cfws::Error) -> Self {
        match value {
            header_cfws::Error::Malformed => Self::Malformed,
            header_cfws::Error::NestingLimit => Self::NestingLimit,
            header_cfws::Error::Work(stop) => Self::Work(stop),
            header_cfws::Error::InvalidState => Self::InvalidState,
            header_cfws::Error::InterpretationLimit => Self::InterpretationLimit,
        }
    }
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed => f.write_str("malformed URL header"),
            Self::NestingLimit => f.write_str("URL header comment nesting limit"),
            Self::Work(error) => write!(f, "URL header work: {error}"),
            Self::InterpretationLimit => f.write_str("header interpretation limit"),
            Self::InvalidState => f.write_str("invalid URL header cursor state"),
        }
    }
}
impl std::error::Error for Error {}
#[derive(Clone, Copy)]
enum Grammar {
    First,
    Item,
    Url,
    Tail,
    NoO,
    NoTail,
}
/// No growing URL/list storage; the enclosing owner reserves response capacity.
pub struct Cursor<'a> {
    source: &'a [u8],
    mode: Mode,
    position: usize,
    grammar: Grammar,
    cfws: Option<header_cfws::Cursor<'a>>,
    uri: uri::Validator,
    replay: bool,
    complete: bool,
    failure: Option<Error>,
}
impl<'a> Cursor<'a> {
    pub const fn new(source: &'a [u8], mode: Mode) -> Self {
        Self {
            source,
            mode,
            position: 0,
            grammar: Grammar::First,
            cfws: Some(header_cfws::Cursor::new(source, 0)),
            uri: uri::Validator::new(),
            replay: false,
            complete: false,
            failure: None,
        }
    }
    pub fn poll(&mut self, now: Tick, work: &mut Meter) -> Result<Status, Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if self.complete {
            return Ok(Status::Complete);
        }
        let result = self.step(now, work);
        if let Err(error) = result {
            self.failure = Some(error);
        }
        result
    }
    fn peek(&self, offset: usize, now: Tick, work: &mut Meter) -> Result<Option<u8>, Error> {
        let position = self
            .position
            .checked_add(offset)
            .ok_or(Error::InvalidState)?;
        let byte = self.source.get(position);
        work.charge(
            now,
            Charge {
                io_bytes: u64::from(byte.is_some()),
                ..Charge::default()
            },
        )?;
        Ok(byte.copied())
    }
    fn advance(&mut self, count: usize) -> Result<(), Error> {
        self.position = self
            .position
            .checked_add(count)
            .ok_or(Error::InvalidState)?;
        if self.position > self.source.len() {
            return Err(Error::InvalidState);
        }
        Ok(())
    }
    fn gap(&mut self, next: Grammar) {
        self.grammar = next;
        self.cfws = Some(header_cfws::Cursor::new(self.source, self.position));
    }
    fn event(&self, status: Status) -> Status {
        if self.replay {
            status
        } else {
            Status::Yield
        }
    }
    fn end(&mut self) -> Status {
        if self.replay {
            self.complete = true;
            Status::Complete
        } else {
            self.position = 0;
            self.replay = true;
            self.gap(Grammar::First);
            Status::Yield
        }
    }
    fn step(&mut self, now: Tick, work: &mut Meter) -> Result<Status, Error> {
        work.charge(
            now,
            Charge {
                records: 1,
                ..Charge::default()
            },
        )?;
        if let Some(cursor) = &mut self.cfws {
            if let header_cfws::Status::Complete(end) = cursor.poll(now, work)? {
                if end.position < self.position || end.position > self.source.len() {
                    return Err(Error::InvalidState);
                }
                self.position = end.position;
                self.cfws = None;
            }
            return Ok(Status::Yield);
        }
        let byte = self.peek(0, now, work)?;
        match self.grammar {
            Grammar::First | Grammar::Item => {
                if byte == Some(b'<') {
                    self.advance(1)?;
                    self.uri = uri::Validator::new();
                    self.grammar = Grammar::Url;
                    Ok(self.event(Status::Begin))
                } else if matches!(self.grammar, Grammar::First)
                    && self.mode == Mode::ListPost
                    && byte == Some(b'N')
                {
                    self.advance(1)?;
                    self.grammar = Grammar::NoO;
                    Ok(Status::Yield)
                } else {
                    Err(Error::Malformed)
                }
            }
            Grammar::NoO => {
                if byte != Some(b'O') {
                    return Err(Error::Malformed);
                }
                self.advance(1)?;
                self.gap(Grammar::NoTail);
                Ok(Status::Yield)
            }
            Grammar::NoTail => {
                if byte.is_some() {
                    Err(Error::Malformed)
                } else {
                    Ok(self.end())
                }
            }
            Grammar::Tail => match byte {
                None => Ok(self.end()),
                Some(b',') => {
                    self.advance(1)?;
                    self.gap(Grammar::Item);
                    Ok(Status::Yield)
                }
                _ => Err(Error::Malformed),
            },
            Grammar::Url => {
                let byte = byte.ok_or(Error::Malformed)?;
                match byte {
                    b'>' => {
                        self.uri.finish()?;
                        self.advance(1)?;
                        self.gap(Grammar::Tail);
                        Ok(self.event(Status::End))
                    }
                    b' ' | b'\t' => {
                        self.advance(1)?;
                        Ok(Status::Yield)
                    }
                    b'\r' | b'\n' => {
                        let ending = if byte == b'\r' {
                            if self.peek(1, now, work)? != Some(b'\n') {
                                return Err(Error::Malformed);
                            }
                            2
                        } else {
                            1
                        };
                        if !matches!(self.peek(ending, now, work)?, Some(b' ' | b'\t')) {
                            return Err(Error::Malformed);
                        }
                        self.advance(ending)?;
                        Ok(Status::Yield)
                    }
                    _ => {
                        self.uri.push(byte, now, work)?;
                        if self.replay {
                            work.charge(
                                now,
                                Charge {
                                    output_bytes: 1,
                                    ..Charge::default()
                                },
                            )?;
                        }
                        self.advance(1)?;
                        Ok(self.event(Status::Byte(byte)))
                    }
                }
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
                output_bytes: 10_000_000,
                ..Charge::default()
            },
        )
    }
    fn parse(source: &[u8], mode: Mode) -> Result<Vec<String>, Error> {
        let mut cursor = Cursor::new(source, mode);
        assert!(std::mem::size_of_val(&cursor) <= 256);
        let mut work = work();
        let mut result = Vec::new();
        let mut current = None;
        for _ in 0..2_000_000 {
            let before = work.remaining();
            let status = cursor.poll(Tick(1), &mut work);
            let after = work.remaining();
            assert!(before.io_bytes - after.io_bytes <= 160);
            assert!(before.records - after.records <= 65);
            assert!(before.output_bytes - after.output_bytes <= 1);
            match status? {
                Status::Yield => {}
                Status::Begin => {
                    assert!(current.is_none());
                    current = Some(String::new());
                }
                Status::Byte(b) => {
                    assert!(b.is_ascii());
                    current.as_mut().unwrap().push(char::from(b));
                }
                Status::End => result.push(current.take().unwrap()),
                Status::Complete => {
                    assert!(current.is_none());
                    assert_eq!(cursor.poll(Tick(100), &mut work), Ok(Status::Complete));
                    assert_eq!(after, work.remaining());
                    return Ok(result);
                }
            }
        }
        panic!("URL header did not finish");
    }
    #[test]
    fn list_order_spelling_comments_and_whitespace_are_preserved_or_removed() {
        for (source, expected) in [
            (
                "<mailto:list@example.org?subject=unsub%20me>",
                vec!["mailto:list@example.org?subject=unsub%20me"],
            ),
            (
                "(outer) < HTTPS : //Example.org/a?x=y#frag >(one),\r\n <mail\nto:x@y>",
                vec![],
            ),
            (
                "(outer) < HTTPS : //Example.org/a?x=y#frag >(one),\r\n <mail\n to:x@y>",
                vec!["HTTPS://Example.org/a?x=y#frag", "mailto:x@y"],
            ),
            (
                "<test:% a B (literal)>, <file:///a/../b>",
                vec!["test:%aB(literal)", "file:///a/../b"],
            ),
            (
                "<x:>, <x:/>, <x://>, <x:?#>",
                vec!["x:", "x:/", "x://", "x:?#"],
            ),
            (
                "<x://u:p:more@host:00080/p//q?x/y?z#q/?a>",
                vec!["x://u:p:more@host:00080/p//q?x/y?z#q/?a"],
            ),
            (
                "<x://%aa:%ff@exa%4Dmple:123/>",
                vec!["x://%aa:%ff@exa%4Dmple:123/"],
            ),
        ] {
            if expected.is_empty() {
                assert_eq!(parse(source.as_bytes(), Mode::URLs), Err(Error::Malformed));
            } else {
                assert_eq!(
                    parse(source.as_bytes(), Mode::URLs),
                    Ok(expected.into_iter().map(str::to_owned).collect()),
                    "{source:?}"
                );
            }
        }
        assert_eq!(
            parse(
                b"(post) <mailto:l@x>, <https://x/> (alternate)",
                Mode::ListPost
            ),
            Ok(vec!["mailto:l@x".to_owned(), "https://x/".to_owned()])
        );
        for source in ["NO", " (no posting) NO\r\n (moderated) "] {
            assert_eq!(parse(source.as_bytes(), Mode::ListPost), Ok(vec![]));
            assert_eq!(parse(source.as_bytes(), Mode::URLs), Err(Error::Malformed));
        }
        for source in ["no", "No", "N O", "NO, <x:>", "NOjunk", "<x:>, NO"] {
            assert_eq!(
                parse(source.as_bytes(), Mode::ListPost),
                Err(Error::Malformed),
                "{source}"
            );
        }
    }
    #[test]
    fn generic_authority_ipv6_and_future_literal_grammar() {
        for url in [
            "x://[::1]",
            "x://[::ffff:192.0.2.1]:65536/",
            "x://[1:2:3:4:5:6:7:8]/",
            "x://[ffff:ffff:ffff:ffff:ffff:ffff:255.255.255.255]/",
            "x://name@[::]/",
            "x://[v1.future:address]/",
            "x://[VfF.a!$&'()*+,;=:]/",
            "x://host:",
            "x://user:non-digit@host",
            "x://@/",
            "x://256.999.1.2/",
        ] {
            assert_eq!(
                parse(format!("<{url}>").as_bytes(), Mode::URLs),
                Ok(vec![url.to_owned()]),
                "{url}"
            );
        }
        for url in [
            "x://[xyz]",
            "x://[]",
            "x://[1:2:3]",
            "x://[::1",
            "x://::1/",
            "x://[::1]junk/",
            "x://[::1]@host",
            "x://[v.a]",
            "x://[v1.]",
            "x://[v1.%ab]",
            "x://host:abc/",
            "x://u@@host/",
            "x://abc[::1]/",
            "x://u:abc/",
            "x://[::1]:1:2/",
            "x://host:%31/",
            "x://[::ffff:999.2.3.4]/",
            "x://[fe80::1%25eth0]/",
            "x:a[b]",
            "x:?a[b]",
            "x:#a[b]",
        ] {
            assert_eq!(
                parse(format!("<{url}>").as_bytes(), Mode::URLs),
                Err(Error::Malformed),
                "{url}"
            );
        }
    }
    #[test]
    fn any_malformed_item_refuses_before_publishing_prefix() {
        let nested = format!("<x:>{}", "(".repeat(33));
        for (source, expected) in [
            (b"".as_slice(), Error::Malformed),
            (b" (x) ", Error::Malformed),
            (b"<x:>,", Error::Malformed),
            (b"<x:> <y:>", Error::Malformed),
            (b"<x:>, bad", Error::Malformed),
            (b"<x:> junk, <y:>", Error::Malformed),
            (b"<x:>, <>", Error::Malformed),
            (b"<x:>, <relative>", Error::Malformed),
            (b"<x:>, <1x:y>", Error::Malformed),
            (b"<x:>, <x:%>", Error::Malformed),
            (b"<x:>, <x:%0>", Error::Malformed),
            (b"<x:>, <x:%zz>", Error::Malformed),
            (b"<x:>, <x:a#b#c>", Error::Malformed),
            (b"<x:>, <x:\0>", Error::Malformed),
            (b"<x:>, <x:\xff>", Error::Malformed),
            (b"<x:>, <x:\r a>", Error::Malformed),
            (b"<x:>, <x:\nq>", Error::Malformed),
            (b"<x:>, <x:\r\nq>", Error::Malformed),
            (b"<x:> (unfinished", Error::Malformed),
            (nested.as_bytes(), Error::NestingLimit),
            ("<x:>, <https://é.example/>".as_bytes(), Error::Malformed),
        ] {
            let mut cursor = Cursor::new(source, Mode::URLs);
            let mut work = work();
            let before = work.remaining();
            let error = loop {
                match cursor.poll(Tick(1), &mut work) {
                    Ok(Status::Yield) => {}
                    Err(error) => break error,
                    Ok(_) => panic!("exposed malformed field"),
                }
            };
            assert_eq!(error, expected, "{source:?}");
            assert_eq!(before.output_bytes, work.remaining().output_bytes);
            let mut fresh = self::work();
            let before = fresh.remaining();
            assert_eq!(cursor.poll(Tick(1), &mut fresh), Err(expected));
            assert_eq!(before, fresh.remaining());
        }
    }
    #[test]
    fn long_names_paths_and_future_literals_use_fixed_state() {
        for url in [
            format!("x://{}/a", "a".repeat(100_000)),
            format!("x:/{}", "abc/".repeat(25_000)),
            format!("x://[v1.{}]", "a".repeat(100_000)),
        ] {
            assert_eq!(
                parse(format!("<{url}>").as_bytes(), Mode::URLs),
                Ok(vec![url])
            );
        }
    }
    #[test]
    fn exact_two_pass_work_and_fixed_literal_parse_cost() {
        for (source, mode, io, records, output) in [
            (b"<x:>".as_slice(), Mode::URLs, 10, 18, 2),
            (b"NO", Mode::ListPost, 6, 14, 0),
            (b"<x://[::1]>", Mode::URLs, 24, 160, 9),
        ] {
            let mut cursor = Cursor::new(source, mode);
            let mut work = work();
            let before = work.remaining();
            while cursor.poll(Tick(1), &mut work).unwrap() != Status::Complete {}
            let after = work.remaining();
            assert_eq!(before.io_bytes - after.io_bytes, io);
            assert_eq!(before.records - after.records, records);
            assert_eq!(before.output_bytes - after.output_bytes, output);
        }
    }
    #[test]
    fn work_refusals_stay_stopped_before_and_during_replay() {
        for (io_bytes, records, output_bytes, expected) in [
            (0, 1000, 1000, Stop::IoBytes),
            (1000, 0, 1000, Stop::Records),
            (1000, 1000, 0, Stop::OutputBytes),
            (1000, 1000, 1, Stop::OutputBytes),
            (1000, 64, 1000, Stop::Records),
        ] {
            let mut work = Meter::new(
                Deadline::after(Tick(0), 100).unwrap(),
                Charge {
                    io_bytes,
                    records,
                    output_bytes,
                    ..Charge::default()
                },
            );
            let mut cursor = Cursor::new(b"<x://[::1]>", Mode::URLs);
            let error = loop {
                match cursor.poll(Tick(1), &mut work) {
                    Ok(Status::Complete) => panic!("accepted insufficient work"),
                    Ok(_) => {}
                    Err(error) => break error,
                }
            };
            assert_eq!(error, Error::Work(expected));
            assert_eq!(cursor.poll(Tick(1), &mut self::work()), Err(error));
        }
        for replay in [false, true] {
            let mut cursor = Cursor::new(b"<x:>", Mode::URLs);
            let mut work = work();
            while cursor.replay != replay {
                assert_ne!(cursor.poll(Tick(1), &mut work).unwrap(), Status::Complete);
            }
            assert_eq!(
                cursor.poll(Tick(100), &mut work),
                Err(Error::Work(Stop::Deadline))
            );
            assert_eq!(
                cursor.poll(Tick(1), &mut self::work()),
                Err(Error::Work(Stop::Deadline))
            );
        }
    }
}

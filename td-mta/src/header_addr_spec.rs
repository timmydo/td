//! Single resident RFC 5322 addr-spec; raw parts stay provisional until Complete.
pub use crate::header_message_ids::Extent;
use crate::{
    admission::work::{Meter, Stop},
    header_message_ids,
    ports::Tick,
};
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    Yield,
    Part(Extent),
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
impl From<header_message_ids::Error> for Error {
    fn from(error: header_message_ids::Error) -> Self {
        match error {
            header_message_ids::Error::Malformed => Self::Malformed,
            header_message_ids::Error::NestingLimit => Self::NestingLimit,
            header_message_ids::Error::Work(stop) => Self::Work(stop),
            header_message_ids::Error::InvalidState => Self::InvalidState,
            header_message_ids::Error::InterpretationLimit => Self::InterpretationLimit,
        }
    }
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed => f.write_str("malformed addr-spec"),
            Self::NestingLimit => f.write_str("addr-spec comment nesting limit"),
            Self::Work(error) => write!(f, "addr-spec work: {error}"),
            Self::InterpretationLimit => f.write_str("header interpretation limit"),
            Self::InvalidState => f.write_str("invalid addr-spec cursor state"),
        }
    }
}
impl std::error::Error for Error {}
/// The shared identifier grammar is entered without enclosing angle brackets.
pub struct Cursor<'a> {
    inner: header_message_ids::Cursor<'a>,
    failure: Option<Error>,
}
impl<'a> Cursor<'a> {
    pub const fn new(source: &'a [u8]) -> Self {
        Self {
            inner: header_message_ids::Cursor::addr_spec(source),
            failure: None,
        }
    }
    pub fn poll(&mut self, now: Tick, work: &mut Meter) -> Result<Status, Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = match self.inner.poll(now, work) {
            Ok(header_message_ids::Status::Yield) => Ok(Status::Yield),
            Ok(header_message_ids::Status::Part(extent)) => Ok(Status::Part(extent)),
            Ok(header_message_ids::Status::Complete) => Ok(Status::Complete),
            Ok(header_message_ids::Status::Begin | header_message_ids::Status::End) => {
                Err(Error::InvalidState)
            }
            Err(error) => Err(error.into()),
        };
        if let Err(error) = result {
            self.failure = Some(error);
        }
        result
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::{admission::work::Charge, ports::Deadline};
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
    fn parts(source: &[u8]) -> Result<Vec<u8>, Error> {
        let mut cursor = Cursor::new(source);
        assert!(std::mem::size_of_val(&cursor) <= 288);
        let mut work = work();
        let mut result = Vec::new();
        for _ in 0..1_000_000 {
            let before = work.remaining();
            let step = cursor.poll(Tick(1), &mut work);
            let after = work.remaining();
            assert!(before.io_bytes - after.io_bytes <= 160);
            assert!(before.records - after.records <= 32);
            assert_eq!(before.output_bytes, after.output_bytes);
            match step? {
                Status::Yield => {}
                Status::Part(extent) => result.extend_from_slice(&source[extent.start..extent.end]),
                Status::Complete => {
                    assert_eq!(cursor.poll(Tick(100), &mut work), Ok(Status::Complete));
                    assert_eq!(after, work.remaining());
                    return Ok(result);
                }
            }
        }
        panic!("addr-spec did not finish");
    }
    #[test]
    fn bare_grammar_preserves_spelling_and_discards_only_grammatical_cfws() {
        for (source, expected) in [
            ("a@b", "a@b"),
            (" (name) a (local) . b @ (domain) c .d (end)", "a.b@c.d"),
            ("\"a b\".c@[x y]", "\"a b\".c@[x y]"),
            ("\"\"@[]", "\"\"@[]"),
            ("é@例.テスト", "é@例.テスト"),
            ("\"a\r\n b\"@[x\n\ty]", "\"a\r\n b\"@[x\n\ty]"),
            ("\"\\\0\"@[\\\r]", "\"\\\0\"@[\\\r]"),
            ("e\u{301}@EXAMPLE", "e\u{301}@EXAMPLE"),
            ("\u{fdd0}@\u{10ffff}", "\u{fdd0}@\u{10ffff}"),
            ("!#$%&'*+-/=?^_`{|}~@EXAMPLE", "!#$%&'*+-/=?^_`{|}~@EXAMPLE"),
        ] {
            assert_eq!(
                parts(source.as_bytes()),
                Ok(expected.as_bytes().to_vec()),
                "{source:?}"
            );
        }
    }
    #[test]
    fn display_names_angles_routes_lists_and_malformed_tails_refuse() {
        for source in [
            b"".as_slice(),
            b" (x) ",
            b"\\\"not quoted",
            b"a",
            b"a@",
            b"@b",
            b"a..b@c",
            b"a@b..c",
            b"a@b.",
            b"<a@b>",
            b"a@b>",
            b"a@b><c@d>",
            b"Name a@b",
            b"a@b,c@d",
            b"a@b;",
            b"a@b extra",
            b"a@b (unclosed",
            b"a@b\0",
            b"a@b\xff",
            b"@route:a@b",
            b"a@[x].b",
            b"a@b[x]",
            b"a@\"b\"",
        ] {
            assert_eq!(parts(source), Err(Error::Malformed), "{source:?}");
        }
    }
    #[test]
    fn bare_and_enclosed_grammars_agree_on_literal_semantics() {
        for source in [
            "a@b",
            " (x) \"a b\" .c @ d.e ",
            "é@[x,;y]",
            "a@b extra",
            "a@b>",
            "a@",
            "@b",
            "a\0@b",
        ] {
            let wrapped = format!("<{source}>");
            let mut parser = header_message_ids::Cursor::new(
                wrapped.as_bytes(),
                header_message_ids::Mode::Strict,
            );
            let mut work = work();
            let mut result = Vec::new();
            let outcome = loop {
                match parser.poll(Tick(1), &mut work) {
                    Ok(header_message_ids::Status::Complete) => break Ok(result),
                    Ok(header_message_ids::Status::Part(extent)) => {
                        result.extend_from_slice(&wrapped.as_bytes()[extent.start..extent.end])
                    }
                    Ok(_) => {}
                    Err(error) => break Err(Error::from(error)),
                }
            };
            assert_eq!(parts(source.as_bytes()), outcome, "{source:?}");
        }
        let long = "🐈".repeat(100_000);
        let source = format!("{long}@b");
        assert_eq!(parts(source.as_bytes()), Ok(source.as_bytes().to_vec()));
    }
    #[test]
    fn exact_work_and_sticky_resource_refusals() {
        for (source, io) in [("a@b", 9), ("🐈@b", 13)] {
            let mut cursor = Cursor::new(source.as_bytes());
            let mut work = work();
            let before = work.remaining();
            while cursor.poll(Tick(1), &mut work).unwrap() != Status::Complete {}
            assert_eq!(before.io_bytes - work.remaining().io_bytes, io);
            assert_eq!(before.records - work.remaining().records, 12);
        }
        let nesting = "(".repeat(33);
        for (source, io_bytes, records, now, expected) in [
            (
                b"a@b".as_slice(),
                0,
                1000,
                Tick(1),
                Error::Work(Stop::IoBytes),
            ),
            (b"a@b", 1000, 0, Tick(1), Error::Work(Stop::Records)),
            (b"a@b", 1000, 1000, Tick(100), Error::Work(Stop::Deadline)),
            (nesting.as_bytes(), 1000, 1000, Tick(1), Error::NestingLimit),
            (b"a@b bad", 1000, 1000, Tick(1), Error::Malformed),
        ] {
            let mut cursor = Cursor::new(source);
            let mut work = Meter::new(
                Deadline::after(Tick(0), 100).unwrap(),
                Charge {
                    io_bytes,
                    records,
                    ..Charge::default()
                },
            );
            let error = loop {
                match cursor.poll(now, &mut work) {
                    Ok(Status::Complete) => panic!("bad source/work accepted"),
                    Ok(_) => {}
                    Err(error) => break error,
                }
            };
            assert_eq!(error, expected);
            let mut fresh = self::work();
            let before = fresh.remaining();
            assert_eq!(cursor.poll(Tick(1), &mut fresh), Err(error));
            assert_eq!(before, fresh.remaining());
        }
    }
}

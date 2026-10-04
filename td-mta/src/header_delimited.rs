//! Mail admission adapter over the shared header lexical cursor.
use crate::{
    admission::work::{Meter, Stop},
    decode_work::{Error as DecodeError, Lexical, Work},
    ports::Tick,
};
pub use td_header::delimited::{Extent, Kind, Status};
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Malformed,
    Work(Stop),
    InterpretationLimit,
    InvalidState,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed => f.write_str("malformed delimited header token"),
            Self::Work(error) => write!(f, "delimited header token work: {error}"),
            Self::InterpretationLimit => f.write_str("header interpretation limit"),
            Self::InvalidState => f.write_str("invalid delimited header token state"),
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

/// Source ends at the enclosing field value_end, excluding its final ending.
/// The enclosing grammar authorizes placement and validates remaining syntax.
pub struct Cursor<'a> {
    inner: td_header::delimited::Cursor<'a, DecodeError>,
}
impl<'a> Cursor<'a> {
    pub(crate) fn checkpoint(
        &self,
    ) -> Result<td_header::delimited::Checkpoint<'a, DecodeError>, Error> {
        self.inner.checkpoint().map_err(Error::from)
    }
    pub(crate) fn resume(checkpoint: td_header::delimited::Checkpoint<'a, DecodeError>) -> Self {
        Self {
            inner: checkpoint.resume(),
        }
    }

    pub const fn new(source: &'a [u8], start: usize, kind: Kind) -> Self {
        Self {
            inner: td_header::delimited::Cursor::new(source, start, kind),
        }
    }
    pub const fn position(&self) -> usize {
        self.inner.position()
    }
    pub fn poll(&mut self, now: Tick, work: &mut Meter) -> Result<Status, Error> {
        self.poll_with_work(now, work)
    }
    pub(crate) fn poll_with_work(
        &mut self,
        now: Tick,
        work: &mut impl Work,
    ) -> Result<Status, Error> {
        self.inner
            .poll(&mut Lexical::new(work, now))
            .map_err(Error::from)
    }
}
impl From<td_header::delimited::Error<DecodeError>> for Error {
    fn from(error: td_header::delimited::Error<DecodeError>) -> Self {
        match error {
            td_header::delimited::Error::Malformed => Self::Malformed,
            td_header::delimited::Error::InvalidState => Self::InvalidState,
            td_header::delimited::Error::Work(error) => Self::from(error),
        }
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
                records: 1_000_000,
                ..Charge::default()
            },
        )
    }
    fn scan(source: &[u8], start: usize, kind: Kind) -> Result<Extent, Error> {
        let mut cursor = Cursor::new(source, start, kind);
        assert!(std::mem::size_of_val(&cursor) <= 64);
        let mut work = work();
        for _ in 0..100_000 {
            let before = work.remaining();
            let status = cursor.poll(Tick(1), &mut work);
            let after = work.remaining();
            assert!(before.io_bytes - after.io_bytes <= 160);
            assert!(before.records - after.records <= 32);
            assert_eq!(before.output_bytes, after.output_bytes);
            if let Status::Complete(extent) = status? {
                assert_eq!(cursor.position(), extent.end);
                assert_eq!(cursor.poll(Tick(100), &mut work), status);
                assert_eq!(after, work.remaining());
                return Ok(extent);
            }
        }
        panic!("delimited token did not finish");
    }
    #[test]
    fn raw_extents_keep_delimiters_escapes_folds_and_unicode() {
        for source in [
            b"\"\"".as_slice(),
            b"\"a(b)[c],;<@>\"",
            b"\"a\\\"b\\\\c\"",
            b"\"a\r\n b\n\tc\"",
            b"\"\x01\x0b\x0c\x1f\x7f\"",
            b"\"\\\0\\\r\\\n\"",
            "\"é\\🐈\u{fdd0}\"".as_bytes(),
        ] {
            assert_eq!(
                scan(source, 0, Kind::QuotedString),
                Ok(Extent {
                    start: 0,
                    end: source.len()
                }),
                "{source:?}"
            );
        }
        for source in [
            b"[]".as_slice(),
            b"[127.0.0.1]",
            b"[IPv6:::1]",
            b"[(x),;\"@]",
            b"[a\\]b\\[c]",
            b"[a\r\n b\n\tc]",
            b"[\x01\x0b\x0c\x1f\x7f]",
            b"[\\\0\\\r\\\n]",
            "[é\\🐈\u{fdd0}]".as_bytes(),
        ] {
            assert_eq!(
                scan(source, 0, Kind::DomainLiteral),
                Ok(Extent {
                    start: 0,
                    end: source.len()
                }),
                "{source:?}"
            );
        }
        assert_eq!(
            scan(b"xx\"a\"trailing", 2, Kind::QuotedString),
            Ok(Extent { start: 2, end: 5 })
        );
        assert_eq!(
            scan(b"xx[a]trailing", 2, Kind::DomainLiteral),
            Ok(Extent { start: 2, end: 5 })
        );
        assert_eq!(scan(b"", 1, Kind::QuotedString), Err(Error::InvalidState));
        assert_eq!(scan(b"", 1, Kind::DomainLiteral), Err(Error::InvalidState));
    }
    #[test]
    fn malformed_tokens_fail_without_an_accepted_prefix() {
        for source in [
            b"".as_slice(),
            b"x",
            b" \"x\"",
            b"\"",
            b"\"a",
            b"\"a\\",
            b"\"a\\\"",
            b"\"a\0b\"",
            b"\"a\rb\"",
            b"\"a\nb\"",
            b"\"a\r\nb\"",
            b"\"\xff\"",
            b"\"\xc0\x80\"",
            b"\"\xed\xa0\x80\"",
            b"\"\xf4\x90\x80\x80\"",
            b"\"\xe2\x82",
            b"\"\\\xc3",
        ] {
            assert_eq!(
                scan(source, 0, Kind::QuotedString),
                Err(Error::Malformed),
                "{source:?}"
            );
        }
        for source in [
            b"".as_slice(),
            b"x",
            b" [x]",
            b"[",
            b"[a",
            b"[a\\",
            b"[a\\]",
            b"[[]]",
            b"[a\0b]",
            b"[a\rb]",
            b"[a\nb]",
            b"[a\r\nb]",
            b"[\xff]",
            b"[\xc0\x80]",
            b"[\xed\xa0\x80]",
            b"[\xf4\x90\x80\x80]",
            b"[\xe2\x82",
            b"[\\\xc3",
        ] {
            assert_eq!(
                scan(source, 0, Kind::DomainLiteral),
                Err(Error::Malformed),
                "{source:?}"
            );
        }
    }
    #[test]
    fn exact_charges_and_long_bounded_turns() {
        for (source, kind, bytes, records) in [
            ("\"🐈\"".as_bytes(), Kind::QuotedString, 7, 3),
            ("[🐈]".as_bytes(), Kind::DomainLiteral, 7, 3),
            (b"\"a\r\n b\"".as_slice(), Kind::QuotedString, 8, 6),
            (b"[a\r\n b]".as_slice(), Kind::DomainLiteral, 8, 6),
        ] {
            let mut cursor = Cursor::new(source, 0, kind);
            let mut work = work();
            let before = work.remaining();
            assert!(matches!(
                cursor.poll(Tick(1), &mut work),
                Ok(Status::Complete(_))
            ));
            assert_eq!(before.io_bytes - work.remaining().io_bytes, bytes);
            assert_eq!(before.records - work.remaining().records, records);
        }
        for kind in [Kind::QuotedString, Kind::DomainLiteral] {
            let source = format!(
                "{}{}{}tail",
                char::from(match kind {
                    Kind::QuotedString => b'\"',
                    Kind::DomainLiteral => b'[',
                }),
                "🐈".repeat(10_000),
                char::from(match kind {
                    Kind::QuotedString => b'\"',
                    Kind::DomainLiteral => b']',
                })
            );
            let mut cursor = Cursor::new(source.as_bytes(), 0, kind);
            assert_eq!(cursor.poll(Tick(1), &mut work()), Ok(Status::Yield));
            assert_eq!(cursor.position(), 125);
            assert_eq!(
                scan(source.as_bytes(), 0, kind),
                Ok(Extent {
                    start: 0,
                    end: 40002
                })
            );
        }
    }
    #[test]
    fn failed_lookahead_and_escaped_unicode_have_exact_charges() {
        for (source, kind, bytes, records, succeeds) in [
            (b"\"a\rb".as_slice(), Kind::QuotedString, 4, 3, false),
            (b"[a\rb", Kind::DomainLiteral, 4, 3, false),
            (b"\"a\nb", Kind::QuotedString, 4, 3, false),
            (b"[a\nb", Kind::DomainLiteral, 4, 3, false),
            (b"\"a\r\nb", Kind::QuotedString, 5, 3, false),
            (b"[a\r\nb", Kind::DomainLiteral, 5, 3, false),
            (b"\"\xe2\x82", Kind::QuotedString, 4, 2, false),
            (b"[\xe2\x82", Kind::DomainLiteral, 4, 2, false),
            (b"\"", Kind::QuotedString, 1, 2, false),
            (b"[", Kind::DomainLiteral, 1, 2, false),
            ("\"\\🐈\"".as_bytes(), Kind::QuotedString, 8, 4, true),
            ("[\\🐈]".as_bytes(), Kind::DomainLiteral, 8, 4, true),
        ] {
            let mut cursor = Cursor::new(source, 0, kind);
            let mut work = work();
            let before = work.remaining();
            let expected = if succeeds {
                Ok(Status::Complete(Extent {
                    start: 0,
                    end: source.len(),
                }))
            } else {
                Err(Error::Malformed)
            };
            assert_eq!(cursor.poll(Tick(1), &mut work), expected);
            let after = work.remaining();
            assert_eq!(before.io_bytes - after.io_bytes, bytes);
            assert_eq!(before.records - after.records, records);
            assert_eq!(before.output_bytes, after.output_bytes);
            assert_eq!(cursor.poll(Tick(1), &mut work), expected);
            assert_eq!(work.remaining(), after);
        }
    }
    #[test]
    fn exact_field_slices_prevent_escaping_into_later_headers() {
        use crate::mime_headers::{Scanner, Status as HeaderStatus};
        for (source, kind) in [
            (b"X:\"a\\\nTo: b\"\n\n".as_slice(), Kind::QuotedString),
            (b"X:[a\\\r\nTo: b]\r\n\r\n".as_slice(), Kind::DomainLiteral),
        ] {
            let mut scanner = Scanner::new(0, 1024);
            let mut position = 0;
            let mut fields = Vec::new();
            let mut work = work();
            loop {
                let progress = scanner
                    .poll(&source[position..], true, Tick(1), &mut work)
                    .unwrap();
                position += progress.consumed;
                match progress.status {
                    HeaderStatus::Field(field) => fields.push(field),
                    HeaderStatus::Complete(_) => break,
                    HeaderStatus::Yield => {}
                    HeaderStatus::NeedInput => panic!("resident field input"),
                }
            }
            assert_eq!(fields.len(), 2);
            let field = fields[0];
            let value = &source[field.value_start as usize..field.value_end as usize];
            assert_eq!(value.len(), 3);
            assert_eq!(scan(value, 0, kind), Err(Error::Malformed));
            let next = fields[1];
            assert_eq!(
                &source[next.name_start as usize..next.name_end as usize],
                b"To"
            );
        }
    }
    #[test]
    fn syntax_and_budget_failures_remain_terminal() {
        for kind in [Kind::QuotedString, Kind::DomainLiteral] {
            let source = format!(
                "{}{}{}",
                char::from(match kind {
                    Kind::QuotedString => b'\"',
                    Kind::DomainLiteral => b'[',
                }),
                "x".repeat(100),
                char::from(match kind {
                    Kind::QuotedString => b'\"',
                    Kind::DomainLiteral => b']',
                })
            );
            let mut cursor = Cursor::new(source.as_bytes(), 0, kind);
            assert_eq!(cursor.poll(Tick(1), &mut work()), Ok(Status::Yield));
            assert_eq!(
                cursor.poll(Tick(100), &mut work()),
                Err(Error::Work(Stop::Deadline))
            );
            assert_eq!(
                cursor.poll(Tick(1), &mut work()),
                Err(Error::Work(Stop::Deadline))
            );
            for (capacity, expected) in [
                (
                    Charge {
                        records: 100,
                        ..Charge::default()
                    },
                    Stop::IoBytes,
                ),
                (
                    Charge {
                        io_bytes: 100,
                        ..Charge::default()
                    },
                    Stop::Records,
                ),
            ] {
                let mut cursor = Cursor::new(source.as_bytes(), 0, kind);
                let mut limited = Meter::new(Deadline::after(Tick(0), 100).unwrap(), capacity);
                assert_eq!(
                    cursor.poll(Tick(1), &mut limited),
                    Err(Error::Work(expected))
                );
                assert_eq!(
                    cursor.poll(Tick(1), &mut work()),
                    Err(Error::Work(expected))
                );
            }
            let mut cursor = Cursor::new(b"bad", 0, kind);
            assert_eq!(cursor.poll(Tick(1), &mut work()), Err(Error::Malformed));
            assert_eq!(cursor.poll(Tick(1), &mut work()), Err(Error::Malformed));
        }
    }
}

//! Optional structured-header CFWS; the enclosing grammar authorizes placement.
use crate::{
    admission::work::{Charge, Meter, Stop},
    decode_work::Work,
    ports::Tick,
};
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Comment {
    pub start: usize,
    pub end: usize,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct End {
    pub position: usize,
    pub consumed: bool,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    Comment(Comment),
    Yield,
    Complete(End),
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
            Self::Malformed => f.write_str("malformed header comment"),
            Self::NestingLimit => f.write_str("header comment nesting limit"),
            Self::Work(error) => write!(f, "header comment work: {error}"),
            Self::InterpretationLimit => f.write_str("header interpretation limit"),
            Self::InvalidState => f.write_str("invalid header comment state"),
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
/// Scans one optional CFWS run, leaving the first non-CFWS byte unconsumed.
/// Source ends at the enclosing field value_end, excluding its final ending.
/// This lexical helper does not establish raw-file field boundaries.
pub struct Cursor<'a> {
    source: &'a [u8],
    start: usize,
    position: usize,
    comment_start: usize,
    depth: u8,
    escaped: bool,
    complete: bool,
    failure: Option<Error>,
}
impl<'a> Cursor<'a> {
    pub const fn new(source: &'a [u8], start: usize) -> Self {
        Self {
            source,
            start,
            position: start,
            comment_start: start,
            depth: 0,
            escaped: false,
            complete: false,
            failure: None,
        }
    }
    pub const fn position(&self) -> usize {
        self.position
    }
    fn end(&self) -> End {
        End {
            position: self.position,
            consumed: self.position != self.start,
        }
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
        if self.complete {
            return Ok(Status::Complete(self.end()));
        }
        let result = self.step(now, work);
        if let Err(error) = result {
            self.failure = Some(error);
        }
        result
    }
    fn peek(&self, offset: usize, now: Tick, work: &mut impl Work) -> Result<Option<u8>, Error> {
        let position = self
            .position
            .checked_add(offset)
            .ok_or(Error::InvalidState)?;
        work.charge(
            now,
            Charge {
                io_bytes: u64::from(position < self.source.len()),
                ..Charge::default()
            },
        )?;
        Ok(self.source.get(position).copied())
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
    fn fold(&self, byte: u8, now: Tick, work: &mut impl Work) -> Result<Option<usize>, Error> {
        let width = if byte == b'\r' {
            if self.peek(1, now, work)? != Some(b'\n') {
                return Ok(None);
            }
            2
        } else {
            1
        };
        Ok(matches!(self.peek(width, now, work)?, Some(b' ' | b'\t')).then_some(width))
    }
    fn character(&self, byte: u8, now: Tick, work: &mut impl Work) -> Result<usize, Error> {
        let width = match byte {
            0..=127 => return Ok(1),
            0xc2..=0xdf => 2,
            0xe0..=0xef => 3,
            0xf0..=0xf4 => 4,
            _ => return Err(Error::Malformed),
        };
        // The bounded UTF-8 validator rereads the lead already inspected above.
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
        let text = self
            .source
            .get(self.position..end)
            .ok_or(Error::InvalidState)?;
        if available != width || std::str::from_utf8(text).is_err() {
            return Err(Error::Malformed);
        }
        Ok(width)
    }
    fn step(&mut self, now: Tick, work: &mut impl Work) -> Result<Status, Error> {
        if self.position > self.source.len() {
            return Err(Error::InvalidState);
        }
        for _ in 0..32 {
            work.charge(
                now,
                Charge {
                    records: 1,
                    ..Charge::default()
                },
            )?;
            let Some(byte) = self.peek(0, now, work)? else {
                if self.depth != 0 {
                    return Err(Error::Malformed);
                }
                self.complete = true;
                return Ok(Status::Complete(self.end()));
            };
            if self.escaped {
                let width = self.character(byte, now, work)?;
                self.advance(width)?;
                self.escaped = false;
                continue;
            }
            if matches!(byte, b' ' | b'\t') {
                self.advance(1)?;
                continue;
            }
            if matches!(byte, b'\r' | b'\n') {
                if let Some(width) = self.fold(byte, now, work)? {
                    self.advance(width)?;
                    continue;
                }
                if self.depth != 0 {
                    return Err(Error::Malformed);
                }
                self.complete = true;
                return Ok(Status::Complete(self.end()));
            }
            if byte == b'(' {
                if self.depth == 32 {
                    return Err(Error::NestingLimit);
                }
                if self.depth == 0 {
                    self.comment_start = self.position;
                }
                self.depth = self.depth.checked_add(1).ok_or(Error::InvalidState)?;
                self.advance(1)?;
                continue;
            }
            if self.depth == 0 {
                self.complete = true;
                return Ok(Status::Complete(self.end()));
            }
            match byte {
                b')' => {
                    self.depth = self.depth.checked_sub(1).ok_or(Error::InvalidState)?;
                    self.advance(1)?;
                    if self.depth == 0 {
                        return Ok(Status::Comment(Comment {
                            start: self.comment_start,
                            end: self.position,
                        }));
                    }
                }
                b'\\' => {
                    self.escaped = true;
                    self.advance(1)?;
                }
                0 => return Err(Error::Malformed),
                _ => {
                    let width = self.character(byte, now, work)?;
                    self.advance(width)?;
                }
            }
        }
        Ok(Status::Yield)
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
                records: 2_000_000,
                ..Charge::default()
            },
        )
    }
    fn scan(source: &[u8], start: usize) -> Result<(Vec<Comment>, End), Error> {
        let mut cursor = Cursor::new(source, start);
        assert!(std::mem::size_of_val(&cursor) <= 64);
        let mut work = work();
        let mut comments = Vec::new();
        for _ in 0..100_000 {
            let before = work.remaining();
            let result = cursor.poll(Tick(1), &mut work);
            let after = work.remaining();
            assert!(before.io_bytes - after.io_bytes <= 160);
            assert!(before.records - after.records <= 32);
            assert_eq!(before.output_bytes, after.output_bytes);
            match result? {
                Status::Comment(comment) => comments.push(comment),
                Status::Yield => {}
                Status::Complete(end) => {
                    assert_eq!(cursor.position(), end.position);
                    assert_eq!(cursor.poll(Tick(100), &mut work), Ok(Status::Complete(end)));
                    assert_eq!(work.remaining(), after);
                    return Ok((comments, end));
                }
            }
        }
        panic!("CFWS did not terminate");
    }
    #[test]
    fn optional_whitespace_comments_and_raw_extents_stop_at_next_token() {
        let source = b" (one)\r\n\t(two(nested)) <id>";
        assert_eq!(
            scan(source, 0),
            Ok((
                vec![Comment { start: 1, end: 6 }, Comment { start: 9, end: 22 }],
                End {
                    position: 23,
                    consumed: true
                }
            ))
        );
        for source in [
            b"".as_slice(),
            b"token",
            b")",
            b"\"(data)\"",
            b"\\",
            b"\0",
            b"\xff",
        ] {
            assert_eq!(
                scan(source, 0),
                Ok((
                    vec![],
                    End {
                        position: 0,
                        consumed: false
                    }
                ))
            );
        }
        assert_eq!(
            scan(b"abc \t()x", 3),
            Ok((
                vec![Comment { start: 5, end: 7 }],
                End {
                    position: 7,
                    consumed: true
                }
            ))
        );
        assert_eq!(
            scan(b" \r\nnot-fold", 0),
            Ok((
                vec![],
                End {
                    position: 1,
                    consumed: true
                }
            ))
        );
        assert_eq!(
            scan(b" \t\r\n \n\t", 0),
            Ok((
                vec![],
                End {
                    position: 7,
                    consumed: true
                }
            ))
        );
        assert_eq!(scan(b"", 1), Err(Error::InvalidState));
    }
    #[test]
    fn nesting_quoted_pairs_utf8_and_obsolete_controls_remain_raw() {
        for source in [
            b"()".as_slice(),
            b"(a(b)c)",
            b"(\\(x\\))",
            b"(\\\\)",
            b"(a\r\n b\n\tc)",
            b"(\x01\x0b\x0c\x1f\x7f)",
            b"(\\\0\\\r\\\n\\\x7f)",
            "(é 🐈 \\é)".as_bytes(),
            "(\u{fdd0})".as_bytes(),
        ] {
            assert_eq!(
                scan(source, 0),
                Ok((
                    vec![Comment {
                        start: 0,
                        end: source.len()
                    }],
                    End {
                        position: source.len(),
                        consumed: true
                    }
                )),
                "{source:?}"
            );
        }
        let valid = format!("{}{}", "(".repeat(32), ")".repeat(32));
        assert_eq!(
            scan(valid.as_bytes(), 0).unwrap().0,
            vec![Comment { start: 0, end: 64 }]
        );
        let over = format!("{}{}", "(".repeat(33), ")".repeat(33));
        assert_eq!(scan(over.as_bytes(), 0), Err(Error::NestingLimit));
        let escaped = format!("({})", "\\(".repeat(1000));
        assert!(scan(escaped.as_bytes(), 0).is_ok());
    }
    #[test]
    fn malformed_comments_do_not_complete_successful_prefixes() {
        for source in [
            b"(".as_slice(),
            b"(a",
            b"(a\\",
            b"(a(b)",
            b"(\0)",
            b"(a\r)",
            b"(a\nb)",
            b"(a\r\nb)",
            b"(a\r \t)",
            b"(\xff)",
            b"(\xc0\x80)",
            b"(\xed\xa0\x80)",
            b"(\xf4\x90\x80\x80)",
            b"(\xe2\x82)",
            b"(\xe2\x82",
            b"(\\\xc3",
            b"(\\\x80)",
            b"() (bad",
        ] {
            assert_eq!(scan(source, 0), Err(Error::Malformed), "{source:?}");
            let mut cursor = Cursor::new(source, 0);
            let mut work = work();
            let mut failed = false;
            for _ in 0..100 {
                if let Err(error) = cursor.poll(Tick(1), &mut work) {
                    assert_eq!(error, Error::Malformed);
                    assert_eq!(cursor.poll(Tick(1), &mut self::work()), Err(error));
                    failed = true;
                    break;
                }
            }
            assert!(failed);
        }
    }
    #[test]
    fn field_slicing_prevents_quoted_line_endings_crossing_fields() {
        use crate::mime_headers::{Scanner, Status as HeaderStatus};
        for source in [
            b"X:(a\\\nTo: b)\n\n".as_slice(),
            b"X:(a\\\r\nTo: b)\r\n\r\n",
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
            assert_eq!(value, b"(a\\");
            assert_eq!(scan(value, 0), Err(Error::Malformed));
            let next = fields[1];
            assert_eq!(
                &source[next.name_start as usize..next.name_end as usize],
                b"To"
            );
        }
    }
    #[test]
    fn exact_visits_charge_utf8_rereads_failed_folds_and_eof() {
        for (source, bytes, records, malformed) in [
            ("(🐈)".as_bytes(), 7, 4, false),
            (b" \r\nx".as_slice(), 4, 2, false),
            (b" \rX".as_slice(), 3, 2, false),
            (b" \nx".as_slice(), 3, 2, false),
            (b"(\xe2\x82".as_slice(), 4, 2, true),
            (b"(\\\xc3".as_slice(), 4, 3, true),
            (b"".as_slice(), 0, 1, false),
        ] {
            let mut cursor = Cursor::new(source, 0);
            let mut work = work();
            let before = work.remaining();
            loop {
                match cursor.poll(Tick(1), &mut work) {
                    Ok(Status::Comment(_) | Status::Yield) => {}
                    Ok(Status::Complete(_)) => {
                        assert!(!malformed);
                        break;
                    }
                    Err(Error::Malformed) => {
                        assert!(malformed);
                        break;
                    }
                    Err(error) => panic!("unexpected {error:?}"),
                }
            }
            assert_eq!(
                before.io_bytes - work.remaining().io_bytes,
                bytes,
                "{source:?}"
            );
            assert_eq!(
                before.records - work.remaining().records,
                records,
                "{source:?}"
            );
        }
    }
    #[test]
    fn long_comments_yield_and_work_refusals_are_sticky() {
        let source = format!("({}) token", "🐈".repeat(10_000));
        let mut cursor = Cursor::new(source.as_bytes(), 0);
        let mut work = work();
        assert_eq!(cursor.poll(Tick(1), &mut work), Ok(Status::Yield));
        assert_eq!(cursor.position(), 1 + 31 * 4);
        assert_eq!(
            cursor.poll(Tick(100), &mut work),
            Err(Error::Work(Stop::Deadline))
        );
        assert_eq!(
            cursor.poll(Tick(1), &mut self::work()),
            Err(Error::Work(Stop::Deadline))
        );
        let (comments, end) = scan(source.as_bytes(), 0).unwrap();
        assert_eq!(
            comments,
            vec![Comment {
                start: 0,
                end: 40002
            }]
        );
        assert_eq!(
            end,
            End {
                position: 40003,
                consumed: true
            }
        );
        for (capacity, expected) in [
            (
                Charge {
                    io_bytes: 2,
                    records: 100,
                    ..Charge::default()
                },
                Stop::IoBytes,
            ),
            (
                Charge {
                    io_bytes: 100,
                    records: 1,
                    ..Charge::default()
                },
                Stop::Records,
            ),
        ] {
            let mut cursor = Cursor::new("(🐈)".as_bytes(), 0);
            let mut limited = Meter::new(Deadline::after(Tick(0), 100).unwrap(), capacity);
            assert_eq!(
                cursor.poll(Tick(1), &mut limited),
                Err(Error::Work(expected))
            );
            assert_eq!(
                cursor.poll(Tick(1), &mut self::work()),
                Err(Error::Work(expected))
            );
        }
        let over = format!("{}{}", "(".repeat(33), ")".repeat(33));
        let mut cursor = Cursor::new(over.as_bytes(), 0);
        assert_eq!(cursor.poll(Tick(1), &mut self::work()), Ok(Status::Yield));
        assert_eq!(
            cursor.poll(Tick(1), &mut self::work()),
            Err(Error::NestingLimit)
        );
        assert_eq!(
            cursor.poll(Tick(1), &mut self::work()),
            Err(Error::NestingLimit)
        );
    }
}

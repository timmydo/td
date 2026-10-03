//! Structural recovery boundaries, not an address grammar or SMTP validator.
use crate::{
    admission::work::{Charge, Meter, Stop},
    ports::Tick,
};
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Separator {
    Comma,
    Semicolon,
    End,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Item {
    pub start: usize,
    pub end: usize,
    pub separator: Separator,
    pub unclosed: bool,
    pub colon: Option<usize>,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    Yield,
    Item(Item),
    Complete,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    NestingLimit,
    Work(Stop),
    InvalidState,
}
impl From<Stop> for Error {
    fn from(value: Stop) -> Self {
        Self::Work(value)
    }
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NestingLimit => f.write_str("address boundary nesting limit"),
            Self::Work(error) => write!(f, "address boundary work: {error}"),
            Self::InvalidState => f.write_str("invalid address boundary cursor state"),
        }
    }
}
impl std::error::Error for Error {}
/// Retains no token text. Input ends at scanner value_end without final ending.
pub struct Cursor<'a> {
    source: &'a [u8],
    start: usize,
    position: usize,
    colon: usize,
    comments: u8,
    angles: u8,
    quoted: bool,
    literal: bool,
    escaped: bool,
    complete: bool,
    failure: Option<Error>,
}
impl<'a> Cursor<'a> {
    pub const fn new(source: &'a [u8]) -> Self {
        Self {
            source,
            start: 0,
            position: 0,
            colon: usize::MAX,
            comments: 0,
            angles: 0,
            quoted: false,
            literal: false,
            escaped: false,
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
    fn nested(depth: &mut u8) -> Result<(), Error> {
        if *depth >= 32 {
            return Err(Error::NestingLimit);
        }
        *depth = depth.checked_add(1).ok_or(Error::InvalidState)?;
        Ok(())
    }
    fn unclosed(&self) -> bool {
        self.comments != 0 || self.angles != 0 || self.quoted || self.literal
    }
    fn step(&mut self, now: Tick, work: &mut Meter) -> Result<Status, Error> {
        if self.position > self.source.len() {
            return Err(Error::InvalidState);
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
            let Some(byte) = self.source.get(self.position).copied() else {
                self.complete = true;
                return Ok(Status::Item(Item {
                    start: self.start,
                    end: self.position,
                    separator: Separator::End,
                    unclosed: self.unclosed(),
                    colon: (self.colon != usize::MAX).then_some(self.colon),
                }));
            };
            let position = self.position;
            self.position = self.position.checked_add(1).ok_or(Error::InvalidState)?;
            if self.escaped {
                self.escaped = false;
                continue;
            }
            if self.comments != 0 {
                match byte {
                    b'\\' => self.escaped = true,
                    b'(' => Self::nested(&mut self.comments)?,
                    b')' => {
                        self.comments = self.comments.checked_sub(1).ok_or(Error::InvalidState)?
                    }
                    _ => {}
                }
            } else if self.quoted {
                match byte {
                    b'\\' => self.escaped = true,
                    b'"' => self.quoted = false,
                    _ => {}
                }
            } else if self.literal {
                match byte {
                    b'\\' => self.escaped = true,
                    b']' => self.literal = false,
                    _ => {}
                }
            } else {
                match byte {
                    b'(' => Self::nested(&mut self.comments)?,
                    b'"' => self.quoted = true,
                    b'[' => self.literal = true,
                    b'<' => Self::nested(&mut self.angles)?,
                    b'>' if self.angles != 0 => {
                        self.angles = self.angles.checked_sub(1).ok_or(Error::InvalidState)?
                    }
                    b':' if self.angles == 0 && self.colon == usize::MAX => self.colon = position,
                    b',' | b';' if self.angles == 0 => {
                        let item = Item {
                            start: self.start,
                            end: position,
                            separator: if byte == b',' {
                                Separator::Comma
                            } else {
                                Separator::Semicolon
                            },
                            unclosed: false,
                            colon: (self.colon != usize::MAX).then_some(self.colon),
                        };
                        self.start = self.position;
                        self.colon = usize::MAX;
                        return Ok(Status::Item(item));
                    }
                    _ => {}
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
                records: 10_000_000,
                ..Charge::default()
            },
        )
    }
    fn scan(source: &[u8]) -> Result<Vec<Item>, Error> {
        let mut cursor = Cursor::new(source);
        assert!(std::mem::size_of_val(&cursor) <= 64);
        let mut work = work();
        let mut result = Vec::new();
        for _ in 0..1_000_000 {
            let before = work.remaining();
            let step = cursor.poll(Tick(1), &mut work);
            let after = work.remaining();
            assert!(before.io_bytes - after.io_bytes <= 32);
            assert!(before.records - after.records <= 32);
            assert_eq!(before.output_bytes, after.output_bytes);
            match step? {
                Status::Yield => {}
                Status::Item(item) => result.push(item),
                Status::Complete => {
                    assert_eq!(cursor.poll(Tick(100), &mut work), Ok(Status::Complete));
                    assert_eq!(after, work.remaining());
                    return Ok(result);
                }
            }
        }
        panic!("address boundaries did not finish");
    }
    fn pieces(source: &[u8]) -> Vec<(&[u8], Separator, bool)> {
        scan(source)
            .unwrap()
            .into_iter()
            .map(|item| (&source[item.start..item.end], item.separator, item.unclosed))
            .collect()
    }
    #[test]
    fn first_unprotected_colon_is_item_metadata_only() {
        let source = b"G: a@b,H: I:c@d;\"q:r\" <@r:a@b>, a@[x:y], a(c:d)@b";
        let items = scan(source).unwrap();
        assert_eq!(
            items.iter().map(|item| item.colon).collect::<Vec<_>>(),
            vec![Some(1), Some(8), None, None, None]
        );
        assert_eq!(
            pieces(b"G: a@b"),
            [(b"G: a@b".as_slice(), Separator::End, false)]
        );
    }
    #[test]
    fn protected_commas_and_semicolons_are_never_recovery_boundaries() {
        let source = b"\"Doe, Jo; Jr\" <a@b>, c(x,(y;z))@d; q@[x,y;z], <@a,@b:q@c>, end";
        assert_eq!(
            pieces(source),
            [
                (b"\"Doe, Jo; Jr\" <a@b>".as_slice(), Separator::Comma, false),
                (b" c(x,(y;z))@d", Separator::Semicolon, false),
                (b" q@[x,y;z]", Separator::Comma, false),
                (b" <@a,@b:q@c>", Separator::Comma, false),
                (b" end", Separator::End, false),
            ]
        );
        assert_eq!(
            pieces(b"Friends: a@b,c@d; other@e"),
            [
                (b"Friends: a@b".as_slice(), Separator::Comma, false),
                (b"c@d", Separator::Semicolon, false),
                (b" other@e", Separator::End, false),
            ]
        );
        assert_eq!(
            pieces(b";,,"),
            [
                (b"".as_slice(), Separator::Semicolon, false),
                (b"", Separator::Comma, false),
                (b"", Separator::Comma, false),
                (b"", Separator::End, false),
            ]
        );
    }
    #[test]
    fn escapes_apply_only_inside_their_own_construct() {
        for item in [
            b"\"a\\\",b\"".as_slice(),
            b"a(x\\),y)@b",
            b"a@[x\\],y]",
            b"<a\"x,y\"@b>",
            b"a@[x(y,\"z)]",
        ] {
            let mut source = item.to_vec();
            source.extend_from_slice(b",tail");
            assert_eq!(
                pieces(&source),
                [
                    (item, Separator::Comma, false),
                    (b"tail".as_slice(), Separator::End, false)
                ]
            );
        }
        assert_eq!(
            pieces(b"a\\,b"),
            [
                (b"a\\".as_slice(), Separator::Comma, false),
                (b"b", Separator::End, false)
            ]
        );
        assert_eq!(
            pieces(b"a(x\"[,;<>y)@b,c"),
            [
                (b"a(x\"[,;<>y)@b".as_slice(), Separator::Comma, false),
                (b"c", Separator::End, false)
            ]
        );
    }
    #[test]
    fn unclosed_construct_consumes_only_the_remaining_field_item() {
        for tail in [
            b"\"a,b;c".as_slice(),
            b"a(x,y;z",
            b"a@[x,y;z",
            b"<a,b;c",
            b"\"a\\",
            b"(a\\",
            b"[a\\",
            b"<<a@b>,tail",
        ] {
            let mut source = b"ok,".to_vec();
            source.extend_from_slice(tail);
            assert_eq!(
                pieces(&source),
                [
                    (b"ok".as_slice(), Separator::Comma, false),
                    (tail, Separator::End, true)
                ]
            );
        }
        assert_eq!(
            pieces(b"<<a@b>>,c"),
            [
                (b"<<a@b>>".as_slice(), Separator::Comma, false),
                (b"c", Separator::End, false)
            ]
        );
    }
    #[test]
    fn arbitrary_bytes_folds_and_long_values_are_retained_without_validation() {
        let source = b" a\0\xff\r\n b ,\"c\n d\"";
        assert_eq!(
            pieces(source),
            [
                (b" a\0\xff\r\n b ".as_slice(), Separator::Comma, false),
                (b"\"c\n d\"", Separator::End, false)
            ]
        );
        assert_eq!(
            pieces(b"a)b]c>d,e"),
            [
                (b"a)b]c>d".as_slice(), Separator::Comma, false),
                (b"e", Separator::End, false),
            ]
        );
        for byte in 0..=255u8 {
            let expected = if matches!(byte, b',' | b';') {
                vec![
                    Item {
                        start: 0,
                        end: 0,
                        separator: if byte == b',' {
                            Separator::Comma
                        } else {
                            Separator::Semicolon
                        },
                        unclosed: false,
                        colon: None,
                    },
                    Item {
                        start: 1,
                        end: 1,
                        separator: Separator::End,
                        unclosed: false,
                        colon: None,
                    },
                ]
            } else {
                vec![Item {
                    start: 0,
                    end: 1,
                    separator: Separator::End,
                    unclosed: matches!(byte, b'(' | b'[' | b'<' | b'"'),
                    colon: (byte == b':').then_some(0),
                }]
            };
            assert_eq!(scan(&[byte]), Ok(expected), "byte {byte}");
        }
        let long = format!("\"{}\" <a@b>,c@d", "é,;".repeat(100_000));
        let items = scan(long.as_bytes()).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].end, long.len() - 4);
        assert_eq!(&long.as_bytes()[items[1].start..items[1].end], b"c@d");
    }
    #[test]
    fn exact_linear_work_empty_tail_and_nesting_ceiling() {
        for source in [b"".as_slice(), b"a,b;", b"(x) a@[b,c]", b"\"unclosed,"] {
            let mut cursor = Cursor::new(source);
            let mut work = work();
            let before = work.remaining();
            while cursor.poll(Tick(1), &mut work).unwrap() != Status::Complete {}
            let after = work.remaining();
            assert_eq!(before.io_bytes - after.io_bytes, source.len() as u64);
            assert_eq!(before.records - after.records, source.len() as u64 + 1);
        }
        for (open, close) in [('(', ')'), ('<', '>')] {
            let valid = format!(
                "{}a,b{};tail",
                open.to_string().repeat(32),
                close.to_string().repeat(32)
            );
            let items = scan(valid.as_bytes()).unwrap();
            assert_eq!(items.len(), 2);
            assert!(!items[0].unclosed);
            let invalid = open.to_string().repeat(33);
            assert_eq!(scan(invalid.as_bytes()), Err(Error::NestingLimit));
            let mut cursor = Cursor::new(invalid.as_bytes());
            let mut work = work();
            assert_eq!(cursor.poll(Tick(1), &mut work), Ok(Status::Yield));
            assert_eq!(cursor.poll(Tick(1), &mut work), Err(Error::NestingLimit));
            let mut fresh = self::work();
            let before = fresh.remaining();
            assert_eq!(cursor.poll(Tick(1), &mut fresh), Err(Error::NestingLimit));
            assert_eq!(before, fresh.remaining());
        }
    }
    #[test]
    fn resource_refusal_after_prior_items_is_sticky() {
        for (io_bytes, records, expected) in [(2, 100, Stop::IoBytes), (100, 2, Stop::Records)] {
            let mut cursor = Cursor::new(b"a,b");
            let mut work = Meter::new(
                Deadline::after(Tick(0), 100).unwrap(),
                Charge {
                    io_bytes,
                    records,
                    ..Charge::default()
                },
            );
            assert_eq!(
                cursor.poll(Tick(1), &mut work),
                Ok(Status::Item(Item {
                    start: 0,
                    end: 1,
                    separator: Separator::Comma,
                    unclosed: false,
                    colon: None,
                }))
            );
            assert_eq!(cursor.poll(Tick(1), &mut work), Err(Error::Work(expected)));
            let mut fresh = self::work();
            let before = fresh.remaining();
            assert_eq!(cursor.poll(Tick(1), &mut fresh), Err(Error::Work(expected)));
            assert_eq!(before, fresh.remaining());
        }
        let mut cursor = Cursor::new(b"a,b");
        let mut work = work();
        assert!(matches!(
            cursor.poll(Tick(1), &mut work),
            Ok(Status::Item(_))
        ));
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

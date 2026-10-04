//! Resident header Raw-form scalars; source authorization and JSON output are external.
pub use budgeted::Budgeted;
mod budgeted;
use crate::{
    admission::work::{Meter, Stop},
    decode_work::{Error as DecodeError, Work},
    mime_charset::{Charset, Decoder, Status as Decoded},
    ports::Tick,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Work(Stop),
    InterpretationLimit,
    InvalidState,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Work(stop) => write!(f, "Raw header work: {stop}"),
            Self::InterpretationLimit => f.write_str("Raw header interpretation limit"),
            Self::InvalidState => f.write_str("invalid Raw header state"),
        }
    }
}
impl std::error::Error for Error {}
impl From<DecodeError> for Error {
    fn from(error: DecodeError) -> Self {
        match error {
            DecodeError::Work(stop) => Self::Work(stop),
            DecodeError::InterpretationLimit => Self::InterpretationLimit,
            DecodeError::InvalidState => Self::InvalidState,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    Scalar(char),
    Yield,
    Complete,
}
/// A copied cursor retains the same immutable source and no work counters.
#[derive(Clone, Copy)]
pub struct Cursor<'a> {
    source: &'a [u8],
    position: usize,
    decoder: Decoder,
    noncharacter: bool,
    complete: bool,
    failure: Option<Error>,
}
impl<'a> Cursor<'a> {
    pub fn new(source: &'a [u8]) -> Self {
        Self {
            source,
            position: 0,
            decoder: Decoder::new(Charset::Utf8),
            noncharacter: false,
            complete: false,
            failure: None,
        }
    }
    pub const fn position(&self) -> usize {
        self.position
    }
    /// Final only at completion; NUL removal is the Raw policy, not malformed UTF-8.
    pub const fn is_encoding_problem(&self) -> bool {
        self.noncharacter || self.decoder.is_encoding_problem()
    }
    /// Decode/filter one scalar, or yield after a removed NUL. The owner brackets
    /// turns with fresh clock/cancellation checks and retains the live meter.
    pub fn poll(&mut self, now: Tick, meter: &mut Meter) -> Result<Status, Error> {
        self.poll_with_work(now, meter)
    }
    fn poll_with_work(&mut self, now: Tick, meter: &mut impl Work) -> Result<Status, Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if self.complete {
            return Ok(Status::Complete);
        }
        let result = self.advance(now, meter);
        if let Err(error) = result {
            self.failure = Some(error);
        }
        result
    }
    fn advance(&mut self, now: Tick, meter: &mut impl Work) -> Result<Status, Error> {
        let input = self
            .source
            .get(self.position..)
            .ok_or(Error::InvalidState)?;
        let decoded = self.decoder.poll_with_work(input, true, now, meter)?;
        self.position = self
            .position
            .checked_add(decoded.consumed)
            .ok_or(Error::InvalidState)?;
        match decoded.status {
            Decoded::Complete => {
                self.complete = true;
                Ok(Status::Complete)
            }
            Decoded::NeedInput => Err(Error::InvalidState),
            Decoded::Scalar(value) => {
                if value == '\0' {
                    return Ok(Status::Yield);
                }
                if crate::unicode::is_noncharacter(value) {
                    self.noncharacter = true;
                    Ok(Status::Scalar('\u{fffd}'))
                } else {
                    Ok(Status::Scalar(value))
                }
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::{
        admission::work::{Charge, Stop},
        ports::Deadline,
    };
    fn meter(io: u64, records: u64) -> Meter {
        Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: io,
                records,
                ..Charge::default()
            },
        )
    }
    pub(super) fn project(bytes: &[u8]) -> (String, bool) {
        let mut cursor = Cursor::new(bytes);
        assert!(std::mem::size_of_val(&cursor) <= 64);
        let mut work = meter(10000, 10000);
        let mut result = String::new();
        for _ in 0..10000 {
            match cursor.poll(Tick(1), &mut work).unwrap() {
                Status::Scalar(value) => result.push(value),
                Status::Yield => {}
                Status::Complete => {
                    assert_eq!(cursor.position(), bytes.len());
                    let remaining = work.remaining();
                    assert_eq!(cursor.poll(Tick(100), &mut work).unwrap(), Status::Complete);
                    assert_eq!(work.remaining(), remaining);
                    return (result, cursor.is_encoding_problem());
                }
            }
        }
        panic!("Raw cursor did not finish");
    }
    #[test]
    fn raw_preserves_folds_case_combining_marks_and_literal_encoded_words() {
        let mail = b"X-A: a\r\n\tb\0c\r\n\r\n";
        let mut scanner = crate::mime_headers::Scanner::new(0, 100);
        let mut work = meter(100, 100);
        let step = scanner.poll(mail, true, Tick(1), &mut work).unwrap();
        let field = match step.status {
            crate::mime_headers::Status::Field(field) => field,
            _ => panic!("missing fixture field"),
        };
        assert!(matches!(
            scanner
                .poll(&mail[step.consumed..], true, Tick(1), &mut work)
                .unwrap()
                .status,
            crate::mime_headers::Status::Complete(_)
        ));
        assert_eq!(
            project(&mail[field.value_start as usize..field.value_end as usize]),
            (" a\r\n\tbc".to_owned(), false)
        );
        for value in [
            "",
            "\t e\u{301} \r\n x  ",
            "=?utf-8?B?ZsO2?=",
            "\u{378}\u{fffd}",
            "\0",
        ] {
            assert_eq!(project(value.as_bytes()), (value.replace('\0', ""), false));
        }
        // Decode before removing NUL: it must not join two invalid fragments.
        assert_eq!(project(b"\xe1\0\x80"), ("��".to_owned(), true));
        assert_eq!(project(b"\xe1\x80"), ("�".to_owned(), true));
    }
    #[test]
    fn every_unicode_noncharacter_is_replaced_without_changing_neighbors() {
        for value in (0xfdd0..=0xfdef)
            .chain((0..=16).flat_map(|plane| [plane * 65536 + 65534, plane * 65536 + 65535]))
        {
            let mut bytes = [0; 4];
            let source = char::from_u32(value).unwrap().encode_utf8(&mut bytes);
            assert_eq!(project(source.as_bytes()), ("�".to_owned(), true));
        }
        assert_eq!(
            project("\u{fdcf}\u{fdf0}\u{1fffd}\u{20000}".as_bytes()),
            ("\u{fdcf}\u{fdf0}\u{1fffd}\u{20000}".to_owned(), false)
        );
    }
    #[test]
    fn skipped_nuls_yield_and_replay_is_charged_to_the_live_meter() {
        let mut cursor = Cursor::new(b"\0a");
        let mut work = meter(10, 10);
        assert_eq!(cursor.poll(Tick(1), &mut work).unwrap(), Status::Yield);
        assert_eq!(cursor.position(), 1);
        assert_eq!(
            (work.remaining().io_bytes, work.remaining().records),
            (9, 9)
        );
        let saved = cursor;
        assert_eq!(
            cursor.poll(Tick(1), &mut work).unwrap(),
            Status::Scalar('a')
        );
        cursor = saved;
        assert_eq!(
            cursor.poll(Tick(1), &mut work).unwrap(),
            Status::Scalar('a')
        );
        assert_eq!(
            (work.remaining().io_bytes, work.remaining().records),
            (7, 7)
        );
        for (io, records, tick, expected) in [
            (0, 10, 1, Stop::IoBytes),
            (10, 0, 1, Stop::Records),
            (10, 10, 100, Stop::Deadline),
        ] {
            let mut cursor = Cursor::new(b"a");
            let mut work = meter(io, records);
            assert_eq!(
                cursor.poll(Tick(tick), &mut work),
                Err(Error::Work(expected))
            );
            let mut fresh = meter(10, 10);
            let before = fresh.remaining();
            assert_eq!(cursor.poll(Tick(1), &mut fresh), Err(Error::Work(expected)));
            assert_eq!(fresh.remaining(), before);
        }
    }
}

//! Per-word transfer/charset decoding; surrounding placement and NFC are external.
use super::{Encoding, Word};
use crate::{
    admission::work::{Charge, Meter, Stop},
    mime_charset::{Decoder, Status as Decoded},
    ports::Tick,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Work(Stop),
    InvalidState,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Work(error) => write!(f, "encoded-word work: {error}"),
            Self::InvalidState => f.write_str("invalid encoded-word state"),
        }
    }
}
impl std::error::Error for Error {}
impl From<crate::mime_charset::Error> for Error {
    fn from(error: crate::mime_charset::Error) -> Self {
        match error {
            crate::mime_charset::Error::Work(stop) => Self::Work(stop),
            crate::mime_charset::Error::InvalidState => Self::InvalidState,
        }
    }
}
#[derive(Clone, Copy)]
enum Event {
    Byte(u8),
    Fault,
    End,
}
#[derive(Clone, Copy)]
struct Transfer<'a> {
    word: Word<'a>,
    position: u8,
    pending: [u8; 3],
    next: u8,
    len: u8,
}
impl<'a> Transfer<'a> {
    const fn new(word: Word<'a>) -> Self {
        Self {
            word,
            position: 0,
            pending: [0; 3],
            next: 0,
            len: 0,
        }
    }
    fn advance(&mut self, bytes: usize) -> Result<(), Error> {
        self.position = self
            .position
            .checked_add(u8::try_from(bytes).map_err(|_| Error::InvalidState)?)
            .ok_or(Error::InvalidState)?;
        Ok(())
    }
    fn take(&mut self) -> Result<Event, Error> {
        let byte = self
            .pending
            .get(usize::from(self.next))
            .copied()
            .ok_or(Error::InvalidState)?;
        self.next = self.next.checked_add(1).ok_or(Error::InvalidState)?;
        Ok(Event::Byte(byte))
    }
    fn poll(&mut self, now: Tick, work: &mut Meter) -> Result<Event, Error> {
        work.charge(
            now,
            Charge {
                records: 1,
                ..Charge::default()
            },
        )
        .map_err(Error::Work)?;
        if self.next < self.len {
            return self.take();
        }
        let input = self
            .word
            .payload()
            .get(usize::from(self.position)..)
            .ok_or(Error::InvalidState)?;
        if input.is_empty() {
            return Ok(Event::End);
        }
        match self.word.encoding() {
            Encoding::Q => {
                work.charge(
                    now,
                    Charge {
                        io_bytes: 1,
                        ..Charge::default()
                    },
                )
                .map_err(Error::Work)?;
                let byte = *input.first().ok_or(Error::InvalidState)?;
                let (event, consumed) = match byte {
                    b'_' => (Event::Byte(b' '), 1),
                    b'=' => {
                        let lookahead = input.len().saturating_sub(1).min(2);
                        work.charge(
                            now,
                            Charge {
                                io_bytes: lookahead as u64,
                                ..Charge::default()
                            },
                        )
                        .map_err(Error::Work)?;
                        match (
                            input.get(1).copied().and_then(hex),
                            input.get(2).copied().and_then(hex),
                        ) {
                            (Some(a), Some(b)) => (Event::Byte(a * 16 + b), 3),
                            _ => (Event::Fault, 1),
                        }
                    }
                    _ => (Event::Byte(byte), 1),
                };
                self.advance(consumed)?;
                Ok(event)
            }
            Encoding::B => {
                let count = input.len().min(4);
                work.charge(
                    now,
                    Charge {
                        io_bytes: count as u64,
                        ..Charge::default()
                    },
                )
                .map_err(Error::Work)?;
                let chunk = input.get(..count).ok_or(Error::InvalidState)?;
                let decoded = quartet(chunk, count == input.len());
                self.advance(count)?;
                match decoded {
                    Some((bytes, len)) => {
                        self.pending = bytes;
                        self.len = len;
                        self.next = 0;
                        self.take()
                    }
                    None => Ok(Event::Fault),
                }
            }
        }
    }
}
fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}
fn sextet(byte: u8) -> Option<u8> {
    match byte {
        b'A'..=b'Z' => Some(byte - b'A'),
        b'a'..=b'z' => Some(byte - b'a' + 26),
        b'0'..=b'9' => Some(byte - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}
fn quartet(input: &[u8], last: bool) -> Option<([u8; 3], u8)> {
    let [a, b, c, d] = input else {
        return None;
    };
    let a = sextet(*a)?;
    let b = sextet(*b)?;
    let first = a << 2 | b >> 4;
    if *c == b'=' {
        return (last && *d == b'=' && b & 15 == 0).then_some(([first, 0, 0], 1));
    }
    let c = sextet(*c)?;
    let second = (b & 15) << 4 | c >> 2;
    if *d == b'=' {
        return (last && c & 3 == 0).then_some(([first, second, 0], 2));
    }
    let d = sextet(*d)?;
    Some(([first, second, (c & 3) << 6 | d], 3))
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    Scalar(char),
    Yield,
    Complete,
}
/// Copies retain source/decoder state, not work. The owner keeps failure retirement.
#[derive(Clone, Copy)]
pub struct Cursor<'a> {
    transfer: Transfer<'a>,
    charset: Decoder,
    pending: Option<Event>,
    problem: bool,
    complete: bool,
    failure: Option<Error>,
}
impl<'a> Cursor<'a> {
    pub const fn new(word: Word<'a>) -> Self {
        Self {
            transfer: Transfer::new(word),
            charset: Decoder::new(word.charset()),
            pending: None,
            problem: false,
            complete: false,
            failure: None,
        }
    }
    /// Final only at completion. Dropping encoded controls is not an error.
    pub const fn is_encoding_problem(&self) -> bool {
        self.problem || self.charset.is_encoding_problem()
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
    fn step(&mut self, now: Tick, work: &mut Meter) -> Result<Status, Error> {
        work.charge(
            now,
            Charge {
                records: 1,
                ..Charge::default()
            },
        )
        .map_err(Error::Work)?;
        let event = match self.pending {
            Some(event) => event,
            None => {
                let event = self.transfer.poll(now, work)?;
                self.pending = Some(event);
                event
            }
        };
        let decoded = match event {
            Event::Byte(byte) => {
                let decoded = self.charset.poll(&[byte], false, now, work)?;
                if decoded.consumed == 1 {
                    self.pending = None;
                } else if decoded.consumed != 0 {
                    return Err(Error::InvalidState);
                }
                decoded
            }
            Event::Fault | Event::End => self.charset.poll(&[], true, now, work)?,
        };
        self.problem |= self.charset.is_encoding_problem();
        match decoded.status {
            Decoded::NeedInput => Ok(Status::Yield),
            Decoded::Scalar(value) => {
                let code = u32::from(value);
                if matches!(code, 0..=0x1f | 0x7f..=0x9f) {
                    return Ok(Status::Yield);
                }
                if matches!(code, 0xfdd0..=0xfdef) || code & 0xffff >= 0xfffe {
                    self.problem = true;
                    return Ok(Status::Scalar('\u{fffd}'));
                }
                Ok(Status::Scalar(value))
            }
            Decoded::Complete => match event {
                Event::Byte(_) => Err(Error::InvalidState),
                Event::Fault => {
                    self.problem = true;
                    self.pending = None;
                    self.charset = Decoder::new(self.transfer.word.charset());
                    Ok(Status::Scalar('\u{fffd}'))
                }
                Event::End => {
                    self.complete = true;
                    Ok(Status::Complete)
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]
    use super::*;
    use crate::{encoded_word::Context, ports::Deadline};
    fn work() -> Meter {
        Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: 10000,
                records: 10000,
                ..Charge::default()
            },
        )
    }
    fn decode(input: &[u8]) -> (String, bool) {
        let word = Word::recognize(input, Context::Text, Tick(1), &mut work())
            .unwrap()
            .unwrap();
        let mut cursor = Cursor::new(word);
        assert!(std::mem::size_of_val(&cursor) <= 128);
        let mut work = work();
        let mut output = String::new();
        for _ in 0..1000 {
            let before = work.remaining();
            let status = cursor.poll(Tick(1), &mut work).unwrap();
            assert!(before.records - work.remaining().records <= 3);
            assert!(before.io_bytes - work.remaining().io_bytes <= 5);
            assert_eq!(work.remaining().output_bytes, 0);
            match status {
                Status::Scalar(value) => output.push(value),
                Status::Yield => {}
                Status::Complete => {
                    assert_eq!(cursor.poll(Tick(100), &mut work), Ok(Status::Complete));
                    return (output, cursor.is_encoding_problem());
                }
            }
        }
        panic!("word did not finish");
    }
    #[test]
    fn valid_words_charsets_and_encoded_controls() {
        for (input, expected, problem) in [
            (
                b"=?utf-8?Q?hello_world=21?=".as_slice(),
                "hello world!",
                false,
            ),
            (b"=?utf-8?Q?f=C3=B6?=", "fö", false),
            (b"=?utf-8?b?4oKs?=", "€", false),
            (b"=?utf-8?B?SGVsbG8=?=", "Hello", false),
            (b"=?utf-8?B?Zg==?=", "f", false),
            (b"=?utf-8?B?Zm9v?=", "foo", false),
            (b"=?latin1?Q?caf=e9=80?=", "café", false),
            (b"=?cp1252?Q?=80=81?=", "€�", true),
            (b"=?ascii?Q?=80a?=", "�a", true),
            (b"=?utf-8?Q?a=00=09=0d=0A=7f=C2=85b?=", "ab", false),
            (b"=?utf-8?Q?=E1=00=80?=", "��", true),
            (b"=?utf-8?Q?=EF=B7=90?=", "�", true),
            (b"=?utf-8?Q?e=CC=81?=", "e\u{301}", false),
        ] {
            assert_eq!(decode(input), (expected.to_owned(), problem), "{input:?}");
        }
        let left = decode(b"=?utf-8?Q?=E2=82?=");
        let right = decode(b"=?utf-8?Q?=AC?=");
        assert_eq!(left, ("�".to_owned(), true));
        assert_eq!(right, ("�".to_owned(), true));
    }
    #[test]
    fn malformed_transfer_units_replace_and_resume() {
        for (input, expected) in [
            (b"=?utf-8?Q?a=QZb?=".as_slice(), "a�QZb"),
            (b"=?utf-8?Q?a=?=", "a�"),
            (b"=?utf-8?Q?=E1=QZ=80?=", "��QZ�"),
            (b"=?utf-8?B?SGVs----bG8=?=", "Hel�lo"),
            (b"=?utf-8?B?SGVsbG8?=", "Hel�"),
            (b"=?utf-8?B?Zh==?=", "�"),
            (b"=?utf-8?B?Zm9=?=", "�"),
            (b"=?utf-8?B?Zg==SGk=?=", "�Hi"),
            (b"=?utf-8?B?Zm8=Zm8=?=", "�fo"),
            (b"=?utf-8?B?Zg=A?=", "�"),
            (b"=?utf-8?B?YWHi----eA==?=", "aa��x"),
            (b"=?utf-8?B?====?=", "�"),
        ] {
            assert_eq!(decode(input), (expected.to_owned(), true), "{input:?}");
        }
    }
    #[test]
    fn copied_checkpoints_recharge_and_failures_retire() {
        let mut work = work();
        let word = Word::recognize(b"=?utf-8?B?4oKsZm8=?=", Context::Text, Tick(1), &mut work)
            .unwrap()
            .unwrap();
        let mut cursor = Cursor::new(word);
        let mut complete = false;
        for _ in 0..100 {
            let saved = cursor;
            let first = cursor.poll(Tick(1), &mut work).unwrap();
            let remaining = work.remaining();
            cursor = saved;
            assert_eq!(cursor.poll(Tick(1), &mut work).unwrap(), first);
            assert!(work.remaining().records < remaining.records);
            if first == Status::Complete {
                complete = true;
                break;
            }
        }
        assert!(complete);
        for (io, records, now, error) in [
            (0, 100, 1, Stop::IoBytes),
            (100, 0, 1, Stop::Records),
            (100, 100, 100, Stop::Deadline),
        ] {
            let mut work = Meter::new(
                Deadline::after(Tick(0), 100).unwrap(),
                Charge {
                    io_bytes: io,
                    records,
                    ..Charge::default()
                },
            );
            let mut cursor = Cursor::new(word);
            assert_eq!(cursor.poll(Tick(now), &mut work), Err(Error::Work(error)));
            let mut fresh = self::work();
            let before = fresh.remaining();
            assert_eq!(cursor.poll(Tick(1), &mut fresh), Err(Error::Work(error)));
            assert_eq!(fresh.remaining(), before);
        }
    }
    #[test]
    fn every_intermediate_checkpoint_replays_and_observes_deadlines() {
        for input in [
            b"=?utf-8?B?4oKsZm8=?=".as_slice(),
            b"=?utf-8?Q?=E1=QZ=80=00a?=",
            b"=?utf-8?B?YWHi----eA==?=",
            b"=?utf-8?B?4YA=?=",
        ] {
            let word = Word::recognize(input, Context::Text, Tick(1), &mut work())
                .unwrap()
                .unwrap();
            let mut cursor = Cursor::new(word);
            let mut work = work();
            let mut complete = false;
            for _ in 0..100 {
                let saved = cursor;
                let first = cursor.poll(Tick(1), &mut work).unwrap();
                let problem = cursor.is_encoding_problem();
                let mut expired = saved;
                let mut expired_work = self::work();
                assert_eq!(
                    expired.poll(Tick(100), &mut expired_work),
                    Err(Error::Work(Stop::Deadline))
                );
                let mut fresh = self::work();
                let before = fresh.remaining();
                assert_eq!(
                    expired.poll(Tick(1), &mut fresh),
                    Err(Error::Work(Stop::Deadline))
                );
                assert_eq!(fresh.remaining(), before);
                let mut replay = saved;
                assert_eq!(replay.poll(Tick(1), &mut work).unwrap(), first);
                assert_eq!(replay.is_encoding_problem(), problem);
                if first == Status::Complete {
                    complete = true;
                    break;
                }
            }
            assert!(complete);
        }
        let word = Word::recognize(b"=?ascii?B?Zm9v?=", Context::Text, Tick(1), &mut work())
            .unwrap()
            .unwrap();
        let mut cursor = Cursor::new(word);
        let mut work = Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: 4,
                records: 100,
                ..Charge::default()
            },
        );
        assert_eq!(
            cursor.poll(Tick(1), &mut work),
            Err(Error::Work(Stop::IoBytes))
        );
        assert_eq!(cursor.transfer.position, 4);
        assert_eq!(cursor.transfer.next, 1);
        assert_eq!(cursor.transfer.len, 3);
        assert_eq!(
            cursor.poll(Tick(1), &mut self::work()),
            Err(Error::Work(Stop::IoBytes))
        );
    }
}

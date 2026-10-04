//! Fixed-state decoding of the mail policy's explicit charset set.
use crate::{
    admission::work::{Charge, Meter, Stop},
    decode_work::{Error as DecodeError, Work},
    ports::Tick,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Charset {
    Utf8,
    Ascii,
    Latin1,
    Windows1252,
}
const LABELS: &[(&[u8], Charset)] = &[
    (b"utf-8", Charset::Utf8),
    (b"utf8", Charset::Utf8),
    (b"us-ascii", Charset::Ascii),
    (b"ascii", Charset::Ascii),
    (b"ansi_x3.4-1968", Charset::Ascii),
    (b"iso-8859-1", Charset::Latin1),
    (b"latin1", Charset::Latin1),
    (b"iso_8859-1", Charset::Latin1),
    (b"windows-1252", Charset::Windows1252),
    (b"cp1252", Charset::Windows1252),
];
/// Passive bounded matching of already admitted label octets; no charset default.
#[derive(Clone, Copy)]
pub struct Label {
    possible: [bool; LABELS.len()],
    position: u8,
}
impl Default for Label {
    fn default() -> Self {
        Self::new()
    }
}
impl Label {
    pub(crate) const FEED_RECORDS: u64 = LABELS.len() as u64;
    pub const fn new() -> Self {
        Self {
            possible: [true; LABELS.len()],
            position: 0,
        }
    }
    /// At most ten trusted alias comparisons. The caller owns admission.
    pub fn feed(&mut self, byte: u8) {
        for ((name, _), possible) in LABELS.iter().zip(self.possible.iter_mut()) {
            *possible &= name
                .get(usize::from(self.position))
                .is_some_and(|wanted| wanted.eq_ignore_ascii_case(&byte));
        }
        self.position = self.position.saturating_add(1);
    }
    pub fn finish(&self) -> Option<Charset> {
        LABELS
            .iter()
            .zip(self.possible.iter())
            .find_map(|((name, charset), possible)| {
                (*possible && name.len() == usize::from(self.position)).then_some(*charset)
            })
    }
}
impl Charset {
    /// Labels are already unquoted by the caller; whitespace is not trimmed.
    pub fn parse(label: &[u8]) -> Option<Self> {
        LABELS
            .iter()
            .find_map(|(name, charset)| label.eq_ignore_ascii_case(name).then_some(*charset))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    Scalar(char),
    NeedInput,
    Complete,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Progress {
    pub consumed: usize,
    pub status: Status,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Work(Stop),
    InvalidState,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Work(_) => "charset work budget exhausted",
            Self::InvalidState => "invalid charset decoder state",
        })
    }
}
impl std::error::Error for Error {}
impl From<crate::decode_work::Error> for Error {
    fn from(error: crate::decode_work::Error) -> Self {
        match error {
            crate::decode_work::Error::Work(stop) => Self::Work(stop),
            // Public Meter calls cannot reach an aggregate header refusal.
            crate::decode_work::Error::InterpretationLimit => Self::InvalidState,
            crate::decode_work::Error::InvalidState => Self::InvalidState,
        }
    }
}
/// One scalar per turn. Copies carry decoding state, never the enclosing meter.
#[derive(Clone, Copy)]
pub struct Decoder {
    charset: Charset,
    scalar: u32,
    remaining: u8,
    low: u8,
    high: u8,
    problem: bool,
    complete: bool,
    eof: bool,
    failure: Option<DecodeError>,
}
impl Decoder {
    pub const fn new(charset: Charset) -> Self {
        Self {
            charset,
            scalar: 0,
            remaining: 0,
            low: 0x80,
            high: 0xbf,
            problem: false,
            complete: false,
            eof: false,
            failure: None,
        }
    }
    pub const fn is_encoding_problem(&self) -> bool {
        self.problem
    }
    fn emit(
        &mut self,
        value: char,
        consumed: usize,
        now: Tick,
        meter: &mut impl Work,
    ) -> Result<Progress, DecodeError> {
        meter.charge(
            now,
            Charge {
                records: 1,
                ..Charge::default()
            },
        )?;
        Ok(Progress {
            consumed,
            status: Status::Scalar(value),
        })
    }
    fn replacement(
        &mut self,
        consumed: usize,
        now: Tick,
        meter: &mut impl Work,
    ) -> Result<Progress, DecodeError> {
        self.remaining = 0;
        self.problem = true;
        self.emit('\u{fffd}', consumed, now, meter)
    }
    /// Resume with unconsumed bytes. The owner brackets turns with clock checks.
    pub fn poll(
        &mut self,
        input: &[u8],
        last: bool,
        now: Tick,
        meter: &mut Meter,
    ) -> Result<Progress, Error> {
        self.poll_with_work(input, last, now, meter)
            .map_err(Error::from)
    }
    pub(crate) fn poll_with_work(
        &mut self,
        input: &[u8],
        last: bool,
        now: Tick,
        meter: &mut impl Work,
    ) -> Result<Progress, DecodeError> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if self.complete {
            return Ok(Progress {
                consumed: 0,
                status: Status::Complete,
            });
        }
        let result = self.advance(input, last, now, meter);
        if let Err(error) = result {
            self.failure = Some(error);
        }
        result
    }
    fn advance(
        &mut self,
        input: &[u8],
        last: bool,
        now: Tick,
        meter: &mut impl Work,
    ) -> Result<Progress, DecodeError> {
        let mut consumed = 0;
        if self.eof {
            meter.charge(now, Charge::default())?;
            self.complete = true;
            return Ok(Progress {
                consumed,
                status: Status::Complete,
            });
        }
        // A scalar needs at most four source bytes; invalid continuation lookahead
        // replaces the accepted prefix and is revisited on the following turn.
        for _ in 0..4 {
            meter.charge(
                now,
                Charge {
                    io_bytes: u64::from(consumed < input.len()),
                    ..Charge::default()
                },
            )?;
            let byte = input.get(consumed).copied();
            let Some(byte) = byte else {
                if !last {
                    return Ok(Progress {
                        consumed,
                        status: Status::NeedInput,
                    });
                }
                self.eof = true;
                if self.remaining != 0 {
                    return self.replacement(consumed, now, meter);
                }
                self.complete = true;
                return Ok(Progress {
                    consumed,
                    status: Status::Complete,
                });
            };
            if self.charset != Charset::Utf8 {
                consumed += 1;
                let value = match self.charset {
                    Charset::Ascii if byte >= 0x80 => {
                        return self.replacement(consumed, now, meter)
                    }
                    Charset::Windows1252 => match windows1252(byte) {
                        Some(value) => value,
                        None => return self.replacement(consumed, now, meter),
                    },
                    _ => char::from(byte),
                };
                return self.emit(value, consumed, now, meter);
            }
            if self.remaining != 0 {
                if byte < self.low || byte > self.high {
                    return self.replacement(consumed, now, meter);
                }
                // Lead masks and continuation bounds keep the accumulator within 21 bits.
                self.scalar = (self.scalar << 6) | u32::from(byte & 0x3f);
                self.remaining = self
                    .remaining
                    .checked_sub(1)
                    .ok_or(DecodeError::InvalidState)?;
                self.low = 0x80;
                self.high = 0xbf;
                consumed += 1;
                if self.remaining == 0 {
                    return self.emit(
                        char::from_u32(self.scalar).ok_or(DecodeError::InvalidState)?,
                        consumed,
                        now,
                        meter,
                    );
                }
                continue;
            }
            consumed += 1;
            let (scalar, remaining, low, high) = match byte {
                0..=0x7f => return self.emit(char::from(byte), consumed, now, meter),
                0xc2..=0xdf => (byte & 0x1f, 1, 0x80, 0xbf),
                0xe0 => (byte & 0xf, 2, 0xa0, 0xbf),
                0xe1..=0xec | 0xee..=0xef => (byte & 0xf, 2, 0x80, 0xbf),
                0xed => (byte & 0xf, 2, 0x80, 0x9f),
                0xf0 => (byte & 7, 3, 0x90, 0xbf),
                0xf1..=0xf3 => (byte & 7, 3, 0x80, 0xbf),
                0xf4 => (byte & 7, 3, 0x80, 0x8f),
                _ => return self.replacement(consumed, now, meter),
            };
            self.scalar = u32::from(scalar);
            self.remaining = remaining;
            self.low = low;
            self.high = high;
        }
        Err(DecodeError::InvalidState)
    }
}
fn windows1252(byte: u8) -> Option<char> {
    Some(match byte {
        0x80 => '\u{20ac}',
        0x81 | 0x8d | 0x8f | 0x90 | 0x9d => return None,
        0x82 => '\u{201a}',
        0x83 => '\u{0192}',
        0x84 => '\u{201e}',
        0x85 => '\u{2026}',
        0x86 => '\u{2020}',
        0x87 => '\u{2021}',
        0x88 => '\u{02c6}',
        0x89 => '\u{2030}',
        0x8a => '\u{0160}',
        0x8b => '\u{2039}',
        0x8c => '\u{0152}',
        0x8e => '\u{017d}',
        0x91 => '\u{2018}',
        0x92 => '\u{2019}',
        0x93 => '\u{201c}',
        0x94 => '\u{201d}',
        0x95 => '\u{2022}',
        0x96 => '\u{2013}',
        0x97 => '\u{2014}',
        0x98 => '\u{02dc}',
        0x99 => '\u{2122}',
        0x9a => '\u{0161}',
        0x9b => '\u{203a}',
        0x9c => '\u{0153}',
        0x9e => '\u{017e}',
        0x9f => '\u{0178}',
        _ => char::from(byte),
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::ports::Deadline;
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
    fn decode(bytes: &[u8], split: usize, charset: Charset) -> (String, bool) {
        let mut decoder = Decoder::new(charset);
        assert!(std::mem::size_of_val(&decoder) <= 32);
        let mut work = meter(10_000, 10_000);
        let mut result = String::new();
        for (input, last) in [(&bytes[..split], false), (&bytes[split..], true)] {
            let mut used = 0;
            for _ in 0..1024 {
                let step = decoder
                    .poll(&input[used..], last, Tick(1), &mut work)
                    .unwrap();
                assert!(step.consumed <= 4);
                used += step.consumed;
                match step.status {
                    Status::Scalar(value) => result.push(value),
                    Status::NeedInput => {
                        assert!(!last);
                        assert_eq!(used, input.len());
                        break;
                    }
                    Status::Complete => {
                        assert_eq!(used, input.len());
                        let before = work.remaining();
                        assert_eq!(
                            decoder
                                .poll(b"ignored", true, Tick(100), &mut work)
                                .unwrap(),
                            Progress {
                                consumed: 0,
                                status: Status::Complete
                            }
                        );
                        assert_eq!(work.remaining(), before);
                        return (result, decoder.is_encoding_problem());
                    }
                }
            }
        }
        panic!("decoder did not finish");
    }
    #[test]
    fn exact_label_set_is_case_insensitive_without_implicit_fallback() {
        for (label, expected) in [
            ("utf-8", Charset::Utf8),
            ("utf8", Charset::Utf8),
            ("us-ascii", Charset::Ascii),
            ("ascii", Charset::Ascii),
            ("ansi_x3.4-1968", Charset::Ascii),
            ("iso-8859-1", Charset::Latin1),
            ("latin1", Charset::Latin1),
            ("iso_8859-1", Charset::Latin1),
            ("windows-1252", Charset::Windows1252),
            ("cp1252", Charset::Windows1252),
        ] {
            assert_eq!(Charset::parse(label.as_bytes()), Some(expected));
            assert_eq!(
                Charset::parse(label.to_ascii_uppercase().as_bytes()),
                Some(expected)
            );
        }
        for label in [
            b"utf-16".as_slice(),
            b"",
            b" utf-8",
            b"utf-8 ",
            b"latin-1",
            b"\"utf8\"",
            b"utf8\0",
        ] {
            assert_eq!(Charset::parse(label), None);
        }
    }
    #[test]
    fn all_single_byte_values_preserve_latin1_and_distinguish_cp1252() {
        let bytes: Vec<u8> = (0..=255).collect();
        let latin: String = bytes.iter().copied().map(char::from).collect();
        let ascii: String = bytes
            .iter()
            .copied()
            .map(|v| if v < 128 { char::from(v) } else { '\u{fffd}' })
            .collect();
        let cp_c1 = "€�‚ƒ„…†‡ˆ‰Š‹Œ�Ž��‘’“”•–—˜™š›œ�žŸ";
        let cp = format!(
            "{}{}{}",
            bytes[..128]
                .iter()
                .copied()
                .map(char::from)
                .collect::<String>(),
            cp_c1,
            bytes[160..]
                .iter()
                .copied()
                .map(char::from)
                .collect::<String>()
        );
        for split in 0..=bytes.len() {
            assert_eq!(
                decode(&bytes, split, Charset::Latin1),
                (latin.clone(), false)
            );
            assert_eq!(decode(&bytes, split, Charset::Ascii), (ascii.clone(), true));
            assert_eq!(
                decode(&bytes, split, Charset::Windows1252),
                (cp.clone(), true)
            );
        }
    }
    #[test]
    fn utf8_maximal_subparts_are_chunk_independent() {
        for (bytes, expected, problem) in [
            (b"".as_slice(), "", false),
            (
                "\0\u{80}\u{7ff}\u{800}\u{d7ff}\u{e000}\u{fdd0}\u{ffff}\u{10000}\u{10ffff}"
                    .as_bytes(),
                "\0\u{80}\u{7ff}\u{800}\u{d7ff}\u{e000}\u{fdd0}\u{ffff}\u{10000}\u{10ffff}",
                false,
            ),
            (b"\xe1\x80".as_slice(), "�", true),
            (b"\xe1\x80a".as_slice(), "�a", true),
            (b"\xe0\x80\x80".as_slice(), "���", true),
            (b"\xed\xa0\x80".as_slice(), "���", true),
            (b"\xf0\x80\x80\x80".as_slice(), "����", true),
            (b"\xf4\x90\x80\x80".as_slice(), "����", true),
            (b"\xf0\x90\x80a".as_slice(), "�a", true),
            (b"\xc0\xaf\xf5\xff".as_slice(), "����", true),
        ] {
            for split in 0..=bytes.len() {
                assert_eq!(
                    decode(bytes, split, Charset::Utf8),
                    (expected.to_owned(), problem)
                );
            }
        }
        for first in 0..=255_u8 {
            for second in 0..=255_u8 {
                let bytes = [first, second];
                let expected = String::from_utf8_lossy(&bytes);
                for split in 0..=2 {
                    let (actual, problem) = decode(&bytes, split, Charset::Utf8);
                    assert_eq!(actual, expected, "{bytes:?}, split {split}");
                    assert_eq!(problem, std::str::from_utf8(&bytes).is_err());
                }
            }
        }
    }
    #[test]
    fn charged_replay_and_sticky_work_refusal_preserve_checkpoint_semantics() {
        let mut decoder = Decoder::new(Charset::Utf8);
        let mut work = meter(10, 10);
        assert_eq!(
            decoder
                .poll(b"\xe1", false, Tick(1), &mut work)
                .unwrap()
                .status,
            Status::NeedInput
        );
        let checkpoint = decoder;
        let before = work.remaining();
        assert_eq!(
            decoder.poll(b"\x80x", true, Tick(1), &mut work).unwrap(),
            Progress {
                consumed: 1,
                status: Status::Scalar('�')
            }
        );
        assert_eq!(work.remaining().io_bytes, before.io_bytes - 2);
        assert_eq!(
            decoder.poll(b"x", true, Tick(1), &mut work).unwrap().status,
            Status::Scalar('x')
        );
        decoder = checkpoint;
        assert_eq!(
            decoder
                .poll(b"\x80x", true, Tick(1), &mut work)
                .unwrap()
                .status,
            Status::Scalar('�')
        );
        assert_eq!(work.remaining().io_bytes, before.io_bytes - 5);
        assert_eq!(work.remaining().records, before.records - 3);
        for (io, records, tick, error) in [
            (0, 1, 1, Stop::IoBytes),
            (1, 0, 1, Stop::Records),
            (1, 1, 100, Stop::Deadline),
        ] {
            let mut decoder = Decoder::new(Charset::Latin1);
            let mut work = meter(io, records);
            assert_eq!(
                decoder.poll(b"a", true, Tick(tick), &mut work),
                Err(Error::Work(error))
            );
            let before = work.remaining();
            assert_eq!(
                decoder.poll(b"", true, Tick(1), &mut work),
                Err(Error::Work(error))
            );
            assert_eq!(work.remaining(), before);
            let mut fresh = meter(10, 10);
            let before = fresh.remaining();
            assert_eq!(
                decoder.poll(b"a", true, Tick(1), &mut fresh),
                Err(Error::Work(error))
            );
            assert_eq!(fresh.remaining(), before);
        }
    }
    #[test]
    fn observed_eof_survives_its_pending_replacement() {
        let mut decoder = Decoder::new(Charset::Utf8);
        let mut work = meter(10, 10);
        assert_eq!(
            decoder.poll(b"\xe1", true, Tick(1), &mut work).unwrap(),
            Progress {
                consumed: 1,
                status: Status::Scalar('�')
            }
        );
        let before = work.remaining();
        assert_eq!(
            decoder.poll(b"ignored", false, Tick(1), &mut work).unwrap(),
            Progress {
                consumed: 0,
                status: Status::Complete
            }
        );
        assert_eq!(work.remaining(), before);
    }
    #[test]
    fn incremental_aliases_equal_exact_slice_matching_at_every_cut() {
        assert_eq!(LABELS.len(), 10);
        assert_eq!(LABELS.len(), Label::new().possible.len());
        assert!(std::mem::size_of::<Label>() <= 16);
        for (name, charset) in LABELS {
            for split in 0..=name.len() {
                let mut matcher = Label::new();
                for byte in name.iter().take(split) {
                    matcher.feed(byte.to_ascii_uppercase());
                }
                assert_eq!(matcher.finish(), Charset::parse(name.get(..split).unwrap()));
                for byte in name.iter().skip(split) {
                    matcher.feed(byte.to_ascii_uppercase());
                }
                assert_eq!(matcher.finish(), Some(*charset));
                matcher.feed(b'x');
                assert_eq!(matcher.finish(), None);
            }
        }
        for input in [
            b"".as_slice(),
            b" utf-8",
            b"utf-8 ",
            b"utf",
            b"unknown",
            b"UTF-8*en",
            b"utf-8\0",
            b"\xff",
        ] {
            let mut matcher = Label::new();
            for byte in input {
                matcher.feed(*byte);
            }
            assert_eq!(matcher.finish(), Charset::parse(input));
            assert_eq!(matcher.finish(), None);
        }
        let mut matcher = Label::new();
        for _ in 0..100_000 {
            matcher.feed(b'a');
        }
        assert_eq!(matcher.finish(), None);
        assert_eq!(matcher.position, u8::MAX);
    }
}

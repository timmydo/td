//! Bounded JSON string serialization of an already authorized NFC source.
use crate::{nfc, ports::Tick};
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Source(nfc::Error),
    InvalidState,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Source(error) => write!(f, "JSON string source: {error}"),
            Self::InvalidState => f.write_str("invalid JSON string state"),
        }
    }
}
impl std::error::Error for Error {}
impl From<nfc::Error> for Error {
    fn from(value: nfc::Error) -> Self {
        Self::Source(value)
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    Yield,
    NeedOutput,
    Complete,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Progress {
    pub written: usize,
    pub status: Status,
}
#[derive(Clone, Copy)]
enum Phase {
    Open,
    Source,
    Close,
    DrainClose,
    Complete,
}
/// Every emitted byte is provisional until the containing property succeeds.
pub struct Cursor<'c, 'a, 'w> {
    source: &'c mut nfc::Cursor<'a, 'w>,
    pending: [u8; 6],
    used: usize,
    position: usize,
    phase: Phase,
    failure: Option<Error>,
}
impl<'c, 'a, 'w> Cursor<'c, 'a, 'w> {
    /// The source must be unpolled. Its owner ensures I-JSON character policy.
    /// Dropping before Complete abandons the whole property; do not rewrap the
    /// advanced source. Charged output is never refunded.
    pub fn new(source: &'c mut nfc::Cursor<'a, 'w>) -> Self {
        Self {
            source,
            pending: [0; 6],
            used: 0,
            position: 0,
            phase: Phase::Open,
            failure: None,
        }
    }
    pub const fn is_encoding_problem(&self) -> bool {
        self.source.is_encoding_problem()
    }
    /// Explicit post-turn/final deadline check; refusal retires even Complete.
    pub fn check_deadline(&mut self, now: Tick) -> Result<(), Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = self.source.charge_output(now, 0).map_err(Error::Source);
        if let Err(error) = result {
            self.failure = Some(error);
        }
        result
    }
    pub fn poll(&mut self, now: Tick, output: &mut [u8]) -> Result<Progress, Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if matches!(self.phase, Phase::Complete) {
            return Ok(Progress {
                written: 0,
                status: Status::Complete,
            });
        }
        let result = self.step(now, output);
        if let Err(error) = result {
            self.failure = Some(error);
        }
        result
    }
    fn stage(&mut self, now: Tick, used: usize) -> Result<(), Error> {
        if used == 0 || used > self.pending.len() {
            return Err(Error::InvalidState);
        }
        self.source.charge_output(now, used as u64)?;
        self.used = used;
        self.position = 0;
        Ok(())
    }
    fn step(&mut self, now: Tick, output: &mut [u8]) -> Result<Progress, Error> {
        self.check_deadline(now)?;
        if output.is_empty() {
            return Ok(Progress {
                written: 0,
                status: Status::NeedOutput,
            });
        }
        if self.position < self.used {
            let count = self
                .used
                .checked_sub(self.position)
                .ok_or(Error::InvalidState)?
                .min(output.len());
            let end = self
                .position
                .checked_add(count)
                .ok_or(Error::InvalidState)?;
            let bytes = self
                .pending
                .get(self.position..end)
                .ok_or(Error::InvalidState)?;
            output
                .get_mut(..count)
                .ok_or(Error::InvalidState)?
                .copy_from_slice(bytes);
            self.position = end;
            if self.position == self.used && matches!(self.phase, Phase::DrainClose) {
                self.phase = Phase::Complete;
            }
            return Ok(Progress {
                written: count,
                status: if matches!(self.phase, Phase::Complete) {
                    Status::Complete
                } else {
                    Status::Yield
                },
            });
        }
        match self.phase {
            Phase::Open | Phase::Close => {
                self.pending = [b'"', 0, 0, 0, 0, 0];
                self.stage(now, 1)?;
                self.phase = if matches!(self.phase, Phase::Open) {
                    Phase::Source
                } else {
                    Phase::DrainClose
                };
            }
            Phase::Source => match self.source.poll(now)? {
                nfc::Status::Yield => {}
                nfc::Status::Scalar(value) => {
                    let used = encode(value, &mut self.pending)?;
                    self.stage(now, used)?;
                }
                nfc::Status::Complete => self.phase = Phase::Close,
            },
            Phase::DrainClose | Phase::Complete => return Err(Error::InvalidState),
        }
        Ok(Progress {
            written: 0,
            status: Status::Yield,
        })
    }
}
fn encode(value: char, output: &mut [u8; 6]) -> Result<usize, Error> {
    let short = match value {
        '"' => Some(b'"'),
        '\\' => Some(b'\\'),
        '\u{8}' => Some(b'b'),
        '\u{c}' => Some(b'f'),
        '\n' => Some(b'n'),
        '\r' => Some(b'r'),
        '\t' => Some(b't'),
        _ => None,
    };
    if let Some(byte) = short {
        *output = [b'\\', byte, 0, 0, 0, 0];
        return Ok(2);
    }
    if value <= '\u{1f}' {
        let code = value as u8;
        let nibble = |value: u8| {
            if value < 10 {
                b'0' + value
            } else {
                b'a' + value - 10
            }
        };
        *output = [
            b'\\',
            b'u',
            b'0',
            b'0',
            nibble(code >> 4),
            nibble(code & 15),
        ];
        return Ok(6);
    }
    let mut bytes = [0; 4];
    let text = value.encode_utf8(&mut bytes);
    output
        .get_mut(..text.len())
        .ok_or(Error::InvalidState)?
        .copy_from_slice(text.as_bytes());
    Ok(text.len())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::{
        admission::work::{Charge, Meter, Stop},
        ports::Deadline,
    };
    fn work(output_bytes: u64) -> Meter {
        Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: 16 * 1024 * 1024,
                records: 2_000_000,
                output_bytes,
                ..Charge::default()
            },
        )
    }
    fn collect(input: &str, width: usize) -> (String, u64) {
        let mut scratch = nfc::Scratch::new();
        let mut work = work(1_000_000);
        let mut budget = nfc::HeaderBudget::new();
        let mut source = nfc::Cursor::new(input, &mut scratch, &mut work, &mut budget);
        let mut cursor = Cursor::new(&mut source);
        assert!(std::mem::size_of_val(&cursor) <= 64);
        let mut bytes = [0xa5; 16];
        let mut text = Vec::new();
        for _ in 0..2_000_000 {
            let progress = cursor
                .poll(Tick(1), bytes.get_mut(..width).unwrap())
                .unwrap();
            assert!(progress.written <= 6);
            text.extend_from_slice(bytes.get(..progress.written).unwrap());
            assert!(bytes.get(width..).unwrap().iter().all(|byte| *byte == 0xa5));
            if progress.status == Status::Complete {
                assert_eq!(
                    cursor.poll(Tick(100), &mut bytes).unwrap(),
                    Progress {
                        written: 0,
                        status: Status::Complete
                    }
                );
                cursor.check_deadline(Tick(1)).unwrap();
                return (
                    String::from_utf8(text).unwrap(),
                    1_000_000 - work.remaining().output_bytes,
                );
            }
        }
        panic!("JSON string did not complete");
    }
    #[test]
    fn strings_escape_controls_and_preserve_unicode_across_every_output_width() {
        for width in 1..=8 {
            for (source, expected) in [
                ("", "\"\""),
                (
                    "\"\\\t\n\r\u{8}\u{c}\0\u{1f}",
                    "\"\\\"\\\\\\t\\n\\r\\b\\f\\u0000\\u001f\"",
                ),
                ("/é例🐈\u{2028}\u{2029}", "\"/é例🐈\u{2028}\u{2029}\""),
                ("e\u{301}", "\"é\""),
            ] {
                let (text, charged) = collect(source, width);
                assert_eq!(text, expected);
                assert_eq!(charged, text.len() as u64);
            }
        }
        let source = format!("a{}", "\u{315}\u{300}".repeat(257));
        let expected = format!("\"à{}{}\"", "\u{300}".repeat(256), "\u{315}".repeat(257));
        assert_eq!(collect(&source, 1).0, expected);
    }
    #[test]
    fn scalar_encoding_covers_all_unicode_without_surrogates_or_unescaped_controls() {
        let mut output = [0; 6];
        for code in 0..=0x10ffff {
            let Some(value) = char::from_u32(code) else {
                continue;
            };
            let used = encode(value, &mut output).unwrap();
            let text = std::str::from_utf8(output.get(..used).unwrap()).unwrap();
            if value <= '\u{1f}' || matches!(value, '"' | '\\') {
                assert!(text.starts_with('\\'));
                assert!(matches!(used, 2 | 6));
                assert!(!text.chars().any(|ch| ch <= '\u{1f}'));
                let decoded = if let Some(hex) = text.strip_prefix(r"\u") {
                    char::from_u32(u32::from_str_radix(hex, 16).unwrap()).unwrap()
                } else {
                    match text {
                        "\\\"" => '"',
                        "\\\\" => '\\',
                        "\\b" => '\u{8}',
                        "\\f" => '\u{c}',
                        "\\n" => '\n',
                        "\\r" => '\r',
                        "\\t" => '\t',
                        _ => panic!("invalid JSON escape"),
                    }
                };
                assert_eq!(decoded, value);
            } else {
                assert_eq!(text.chars().count(), 1);
                assert_eq!(text.chars().next(), Some(value));
            }
        }
    }
    #[test]
    fn decoded_header_filtering_and_diagnostics_belong_to_the_source() {
        for (input, expected, problem) in [
            (b"=?utf-8?q?e=CC=81=22?=".as_slice(), "\"é\\\"\"", false),
            (b"=?utf-8?q?=FF?=", "\"�\"", true),
            (b"a\0b", "\"ab\"", false),
        ] {
            let mut scratch = nfc::Scratch::new();
            let mut meter = work(100);
            let mut budget = nfc::HeaderBudget::new();
            let mut source =
                nfc::Cursor::from_unstructured_header(input, &mut scratch, &mut meter, &mut budget);
            let mut cursor = Cursor::new(&mut source);
            let mut output = [0; 6];
            let mut text = Vec::new();
            loop {
                let progress = cursor.poll(Tick(1), &mut output).unwrap();
                text.extend_from_slice(output.get(..progress.written).unwrap());
                if progress.status == Status::Complete {
                    break;
                }
            }
            assert_eq!(text, expected.as_bytes());
            assert_eq!(cursor.is_encoding_problem(), problem);
        }
    }
    #[test]
    fn empty_output_neither_advances_nor_charges_but_checks_the_deadline() {
        let mut scratch = nfc::Scratch::new();
        let mut work = work(100);
        let before = work.remaining();
        let mut budget = nfc::HeaderBudget::new();
        let mut source = nfc::Cursor::new("x", &mut scratch, &mut work, &mut budget);
        let mut cursor = Cursor::new(&mut source);
        for _ in 0..4 {
            assert_eq!(
                cursor.poll(Tick(1), &mut []),
                Ok(Progress {
                    written: 0,
                    status: Status::NeedOutput
                })
            );
        }
        assert_eq!(
            cursor.poll(Tick(100), &mut []),
            Err(Error::Source(nfc::Error::Work(Stop::Deadline)))
        );
        assert_eq!(
            cursor.poll(Tick(1), &mut [0; 8]),
            Err(Error::Source(nfc::Error::Work(Stop::Deadline)))
        );
        assert_eq!(before, work.remaining());
    }
    #[test]
    fn staged_escape_checks_deadlines_before_copying_and_is_not_recharged() {
        let mut scratch = nfc::Scratch::new();
        let mut meter = work(8);
        let mut budget = nfc::HeaderBudget::new();
        let mut source = nfc::Cursor::new("\0", &mut scratch, &mut meter, &mut budget);
        let mut cursor = Cursor::new(&mut source);
        let mut output = [0; 1];
        loop {
            cursor.poll(Tick(1), &mut output).unwrap();
            if cursor.used == 6 && cursor.position == 1 {
                break;
            }
        }
        let (position, used) = (cursor.position, cursor.used);
        assert_eq!(
            cursor.poll(Tick(1), &mut []),
            Ok(Progress {
                written: 0,
                status: Status::NeedOutput
            })
        );
        assert_eq!((cursor.position, cursor.used), (position, used));
        output = [0xa5];
        let error = Error::Source(nfc::Error::Work(Stop::Deadline));
        assert_eq!(cursor.poll(Tick(100), &mut output), Err(error));
        assert_eq!(output, [0xa5]);
        assert_eq!(cursor.poll(Tick(1), &mut output), Err(error));
        assert_eq!(meter.remaining().output_bytes, 1);
    }
    #[test]
    fn output_and_deadline_refusals_latch_and_never_complete() {
        for limit in 0..8 {
            let mut scratch = nfc::Scratch::new();
            let mut work = work(limit);
            let mut budget = nfc::HeaderBudget::new();
            let mut source = nfc::Cursor::new("\0", &mut scratch, &mut work, &mut budget);
            let mut cursor = Cursor::new(&mut source);
            let mut output = [0xa5; 1];
            let error = loop {
                match cursor.poll(Tick(1), &mut output) {
                    Ok(Progress {
                        status: Status::Complete,
                        ..
                    }) => panic!("short output budget succeeded"),
                    Ok(_) => {}
                    Err(error) => break error,
                }
            };
            assert_eq!(error, Error::Source(nfc::Error::Work(Stop::OutputBytes)));
            assert_eq!(cursor.poll(Tick(1), &mut output), Err(error));
        }
        let mut scratch = nfc::Scratch::new();
        let mut work = work(8);
        let mut budget = nfc::HeaderBudget::new();
        let mut source = nfc::Cursor::new("\0", &mut scratch, &mut work, &mut budget);
        let mut cursor = Cursor::new(&mut source);
        let mut output = [0; 8];
        while cursor.poll(Tick(1), &mut output).unwrap().status != Status::Complete {}
        assert_eq!(
            cursor.check_deadline(Tick(100)),
            Err(Error::Source(nfc::Error::Work(Stop::Deadline)))
        );
        assert_eq!(
            cursor.poll(Tick(1), &mut output),
            Err(Error::Source(nfc::Error::Work(Stop::Deadline)))
        );
    }
}

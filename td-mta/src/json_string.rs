//! Bounded JSON strings from already selected header scalar sources.
use crate::{
    admission::work::{Charge, Meter, Stop},
    header_address_text, header_raw, nfc,
    ports::Tick,
};
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Source(nfc::Error),
    Raw(header_raw::Error),
    Address(header_address_text::Error),
    Work(Stop),
    InvalidState,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Source(error) => write!(f, "JSON string source: {error}"),
            Self::Raw(error) => write!(f, "JSON Raw source: {error}"),
            Self::Address(error) => write!(f, "JSON address source: {error}"),
            Self::Work(error) => write!(f, "JSON string output: {error}"),
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
enum Source<'c, 'a, 'w> {
    Normalized(&'c mut nfc::Cursor<'a, 'w>),
    Raw(&'c mut header_raw::Cursor<'a>, &'c mut Meter),
    Address(&'c mut header_address_text::Cursor<'a>, &'c mut Meter),
}
impl Source<'_, '_, '_> {
    fn charge_output(&mut self, now: Tick, output_bytes: u64) -> Result<(), Error> {
        match self {
            Self::Normalized(cursor) => cursor
                .charge_output(now, output_bytes)
                .map_err(Error::Source),
            Self::Raw(_, work) | Self::Address(_, work) => work
                .charge(
                    now,
                    Charge {
                        output_bytes,
                        ..Charge::default()
                    },
                )
                .map_err(Error::Work),
        }
    }
    const fn is_encoding_problem(&self) -> bool {
        match self {
            Self::Normalized(cursor) => cursor.is_encoding_problem(),
            Self::Raw(cursor, _) => cursor.is_encoding_problem(),
            Self::Address(cursor, _) => cursor.is_encoding_problem(),
        }
    }
    fn poll(&mut self, now: Tick) -> Result<nfc::Status, Error> {
        match self {
            Self::Normalized(cursor) => cursor.poll(now).map_err(Error::Source),
            Self::Raw(cursor, work) => match cursor.poll(now, work).map_err(Error::Raw)? {
                header_raw::Status::Yield => Ok(nfc::Status::Yield),
                header_raw::Status::Scalar(value) => Ok(nfc::Status::Scalar(value)),
                header_raw::Status::Complete => Ok(nfc::Status::Complete),
            },
            Self::Address(cursor, work) => match cursor.poll(now, work).map_err(Error::Address)? {
                header_address_text::Status::Yield => Ok(nfc::Status::Yield),
                header_address_text::Status::Scalar(value) => Ok(nfc::Status::Scalar(value)),
                header_address_text::Status::Complete => Ok(nfc::Status::Complete),
            },
        }
    }
}
/// Every emitted byte is provisional until the containing property succeeds.
pub struct Cursor<'c, 'a, 'w> {
    source: Source<'c, 'a, 'w>,
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
        Self::from_source(Source::Normalized(source))
    }
    /// Supply an unpolled Raw cursor and its job's live meter.
    pub fn from_raw(source: &'c mut header_raw::Cursor<'a>, work: &'c mut Meter) -> Self {
        Self::from_source(Source::Raw(source, work))
    }
    /// Supply an unpolled address cursor and its job's live meter.
    pub fn from_address(
        source: &'c mut header_address_text::Cursor<'a>,
        work: &'c mut Meter,
    ) -> Self {
        Self::from_source(Source::Address(source, work))
    }
    fn from_source(source: Source<'c, 'a, 'w>) -> Self {
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
        let result = self.source.charge_output(now, 0);
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
    fn drain(cursor: &mut Cursor<'_, '_, '_>, width: usize) -> String {
        assert!(std::mem::size_of_val(cursor) <= 64);
        let mut output = [0; 8];
        let mut text = Vec::new();
        for _ in 0..100_000 {
            let progress = cursor
                .poll(Tick(1), output.get_mut(..width).unwrap())
                .unwrap();
            text.extend_from_slice(output.get(..progress.written).unwrap());
            if progress.status == Status::Complete {
                return String::from_utf8(text).unwrap();
            }
        }
        panic!("identity JSON string did not finish");
    }
    #[test]
    fn raw_headers_keep_folds_and_decomposition_without_normalization() {
        for width in 1..=8 {
            for (input, expected, problem) in [
                (b"e\xcc\x81".as_slice(), "\"e\u{301}\"", false),
                (b"a\r\n\tb\0", r#""a\r\n\tb""#, false),
                (b"\"\\\xff", "\"\\\"\\\\�\"", true),
                (b"=?utf-8?q?e=CC=81?=", "\"=?utf-8?q?e=CC=81?=\"", false),
            ] {
                let mut source = header_raw::Cursor::new(input);
                let mut meter = work(1000);
                let mut cursor = Cursor::from_raw(&mut source, &mut meter);
                assert_eq!(drain(&mut cursor, width), expected);
                assert_eq!(cursor.is_encoding_problem(), problem);
                assert_eq!(1000 - meter.remaining().output_bytes, expected.len() as u64);
            }
        }
    }
    #[test]
    fn parsed_and_fallback_addresses_preserve_identity_and_charge_each_layer() {
        use header_address_text::Mode::{Fallback, Parsed};
        for width in 1..=8 {
            for (input, mode, expected, problem) in [
                (
                    b"e\xcc\x81@EXAMPLE.org".as_slice(),
                    Parsed,
                    "\"e\u{301}@EXAMPLE.org\"",
                    false,
                ),
                (b"(left)a@(right)b", Parsed, "\"a@b\"", false),
                (b"\"a\\b\"@b", Parsed, r#""\"a\\b\"@b""#, false),
                (b"=?utf-8?q?x?=@b", Parsed, "\"=?utf-8?q?x?=@b\"", false),
                (
                    b"  e\xcc\x81 broken\r\n value \t",
                    Fallback,
                    "\"e\u{301} broken value\"",
                    false,
                ),
                (b" \xff\0 ", Fallback, "\"�\\u0000\"", true),
            ] {
                let mut original = header_address_text::Cursor::new(input, mode);
                let mut original_meter = work(10000);
                while original.poll(Tick(1), &mut original_meter).unwrap()
                    != header_address_text::Status::Complete
                {}
                let conversion = 10000 - original_meter.remaining().output_bytes;
                let mut source = header_address_text::Cursor::new(input, mode);
                let mut meter = work(10000);
                let mut cursor = Cursor::from_address(&mut source, &mut meter);
                assert_eq!(drain(&mut cursor, width), expected);
                assert_eq!(cursor.is_encoding_problem(), problem);
                assert_eq!(
                    10000 - meter.remaining().output_bytes,
                    conversion + expected.len() as u64
                );
            }
        }
    }
    fn refusal(cursor: &mut Cursor<'_, '_, '_>, output: &mut [u8]) -> (Error, usize) {
        let mut written = 0;
        for _ in 0..10_000 {
            match cursor.poll(Tick(1), output) {
                Ok(Progress {
                    status: Status::Complete,
                    ..
                }) => panic!("refused JSON completed"),
                Ok(progress) => written += progress.written,
                Err(error) => return (error, written),
            }
        }
        panic!("JSON refusal did not finish");
    }
    #[test]
    fn raw_and_address_source_refusals_retire_provisional_json() {
        let mut raw = header_raw::Cursor::new(b"a");
        let mut meter = Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                output_bytes: 100,
                ..Charge::default()
            },
        );
        let mut cursor = Cursor::from_raw(&mut raw, &mut meter);
        let mut output = [0; 8];
        let (error, written) = refusal(&mut cursor, &mut output);
        assert_eq!(written, 1);
        assert_eq!(error, Error::Raw(header_raw::Error::Work(Stop::IoBytes)));
        assert_eq!(cursor.poll(Tick(1), &mut output), Err(error));
        let mut address =
            header_address_text::Cursor::new(b"a@", header_address_text::Mode::Parsed);
        let mut meter = work(100);
        let mut cursor = Cursor::from_address(&mut address, &mut meter);
        let (error, written) = refusal(&mut cursor, &mut output);
        assert_eq!(written, 1);
        assert_eq!(error, Error::Address(header_address_text::Error::Malformed));
        assert_eq!(cursor.poll(Tick(1), &mut output), Err(error));
        let mut raw = header_raw::Cursor::new(b"name");
        let mut meter = work(1);
        let mut cursor = Cursor::from_raw(&mut raw, &mut meter);
        let (error, written) = refusal(&mut cursor, &mut output);
        assert_eq!(written, 1);
        assert_eq!(error, Error::Work(Stop::OutputBytes));
        assert_eq!(cursor.check_deadline(Tick(1)), Err(error));
        let mut address =
            header_address_text::Cursor::new(b"a@b", header_address_text::Mode::Parsed);
        let mut meter = work(100);
        let mut cursor = Cursor::from_address(&mut address, &mut meter);
        assert_eq!(drain(&mut cursor, 1), "\"a@b\"");
        assert_eq!(
            cursor.check_deadline(Tick(100)),
            Err(Error::Work(Stop::Deadline))
        );
        assert_eq!(
            cursor.poll(Tick(1), &mut output),
            Err(Error::Work(Stop::Deadline))
        );
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

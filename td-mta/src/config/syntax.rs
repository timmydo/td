//! Bounded line syntax. Schema/reference/resource/permission checks follow parsing.
use std::{fmt, num::NonZeroU32};

pub const MAX_INPUT_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_LINES: u32 = 65_536;
pub const MAX_LINE_BYTES: usize = 8192;
pub const MAX_STRING_BYTES: usize = 4096;
pub const MAX_NAME_BYTES: usize = 64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Location {
    pub line: NonZeroU32,
    pub column: u16,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Code {
    Capacity,
    InvalidState,
    InputTooLarge,
    TooManyLines,
    LineTooLong,
    InvalidUtf8,
    ControlCharacter,
    ExpectedName,
    NameTooLong,
    ExpectedEquals,
    ExpectedValue,
    InvalidInteger,
    InvalidEscape,
    UnterminatedString,
    StringTooLong,
    ExpectedBracket,
    ExpectedSpace,
    TrailingData,
}
impl Code {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Capacity => "config_capacity",
            Self::InvalidState => "config_invalid_state",
            Self::InputTooLarge => "config_input_too_large",
            Self::TooManyLines => "config_too_many_lines",
            Self::LineTooLong => "config_line_too_long",
            Self::InvalidUtf8 => "config_invalid_utf8",
            Self::ControlCharacter => "config_control_character",
            Self::ExpectedName => "config_expected_name",
            Self::NameTooLong => "config_name_too_long",
            Self::ExpectedEquals => "config_expected_equals",
            Self::ExpectedValue => "config_expected_value",
            Self::InvalidInteger => "config_invalid_integer",
            Self::InvalidEscape => "config_invalid_escape",
            Self::UnterminatedString => "config_unterminated_string",
            Self::StringTooLong => "config_string_too_long",
            Self::ExpectedBracket => "config_expected_bracket",
            Self::ExpectedSpace => "config_expected_space",
            Self::TrailingData => "config_trailing_data",
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Diagnostic {
    pub code: Code,
    pub location: Location,
}
impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} at line {}, byte column {}",
            self.code.name(),
            self.location.line,
            self.location.column
        )
    }
}
impl std::error::Error for Diagnostic {}
fn diagnostic(code: Code, line: NonZeroU32, offset: usize) -> Diagnostic {
    Diagnostic {
        code,
        location: Location {
            line,
            column: u16::try_from(offset.saturating_add(1)).unwrap_or(u16::MAX),
        },
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub enum Value<'a> {
    Text(&'a str),
    Integer(u64),
    Boolean(bool),
}
impl fmt::Debug for Value<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Text(_) => "Text(<redacted>)",
            Self::Integer(_) => "Integer(<redacted>)",
            Self::Boolean(_) => "Boolean(<redacted>)",
        })
    }
}
#[derive(Clone, Copy, Eq, PartialEq)]
pub enum Statement<'a> {
    Empty,
    Section {
        location: Location,
        name: &'a str,
        label: Option<&'a str>,
    },
    Assignment {
        location: Location,
        key: &'a str,
        value_location: Location,
        value: Value<'a>,
    },
}
impl fmt::Debug for Statement<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Empty => "Empty",
            Self::Section { .. } => "Section(<redacted>)",
            Self::Assignment { .. } => "Assignment(<redacted>)",
        })
    }
}

/// Retains at most one physical line. The caller must process and advance it
/// before feeding more input; returned consumed bytes never include a next line.
pub struct Framer<'a> {
    storage: &'a mut [u8],
    length: usize,
    total: usize,
    number: NonZeroU32,
    ready: bool,
    eof: bool,
    failure: Option<Diagnostic>,
}
impl<'a> Framer<'a> {
    pub fn new(storage: &'a mut [u8]) -> Result<Self, Diagnostic> {
        let storage = storage.get_mut(..MAX_LINE_BYTES).ok_or(diagnostic(
            Code::Capacity,
            NonZeroU32::MIN,
            0,
        ))?;
        Ok(Self {
            storage,
            length: 0,
            total: 0,
            number: NonZeroU32::MIN,
            ready: false,
            eof: false,
            failure: None,
        })
    }
    fn fail<T>(&mut self, code: Code) -> Result<T, Diagnostic> {
        let error = diagnostic(code, self.number, self.length);
        self.failure = Some(error);
        Err(error)
    }
    pub fn feed(&mut self, input: &[u8]) -> Result<usize, Diagnostic> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if self.eof && !input.is_empty() {
            return self.fail(Code::InvalidState);
        }
        if self.ready || input.is_empty() {
            return Ok(0);
        }
        for (offset, byte) in input.iter().enumerate() {
            if self.number.get() > MAX_LINES {
                return self.fail(Code::TooManyLines);
            }
            if self.total >= MAX_INPUT_BYTES {
                return self.fail(Code::InputTooLarge);
            }
            let Some(cell) = self.storage.get_mut(self.length) else {
                return self.fail(Code::LineTooLong);
            };
            *cell = *byte;
            self.length =
                self.length
                    .checked_add(1)
                    .ok_or(diagnostic(Code::InvalidState, self.number, 0))?;
            self.total =
                self.total
                    .checked_add(1)
                    .ok_or(diagnostic(Code::InvalidState, self.number, 0))?;
            if *byte == b'\n' {
                self.ready = true;
                return offset
                    .checked_add(1)
                    .ok_or(diagnostic(Code::InvalidState, self.number, 0));
            }
        }
        Ok(input.len())
    }
    /// EOF may expose a final unterminated line; no later nonempty feed is legal.
    pub fn finish(&mut self) -> Result<(), Diagnostic> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        self.eof = true;
        self.ready = self.length != 0;
        Ok(())
    }
    // Private counters and fixed storage make the checked arithmetic/slice
    // fallbacks unreachable through safe calls; all input failures use fail.
    pub fn line(&self) -> Result<Option<(NonZeroU32, &[u8])>, Diagnostic> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if !self.ready {
            return Ok(None);
        }
        let bytes = self.storage.get(..self.length).ok_or(diagnostic(
            Code::InvalidState,
            self.number,
            0,
        ))?;
        Ok(Some((self.number, bytes)))
    }
    pub fn advance(&mut self) -> Result<(), Diagnostic> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if !self.ready {
            return self.fail(Code::InvalidState);
        }
        self.number =
            self.number
                .checked_add(1)
                .ok_or(diagnostic(Code::TooManyLines, self.number, 0))?;
        self.length = 0;
        self.ready = false;
        Ok(())
    }
    pub fn is_finished(&self) -> bool {
        self.eof && !self.ready && self.failure.is_none()
    }
    pub fn consumed_bytes(&self) -> usize {
        self.total
    }
}

struct Cursor<'a, 'b> {
    source: &'a [u8],
    scratch: &'b mut [u8],
    pos: usize,
    line: NonZeroU32,
}
impl Cursor<'_, '_> {
    fn error(&self, code: Code) -> Diagnostic {
        diagnostic(code, self.line, self.pos)
    }
    fn peek(&self) -> Option<u8> {
        self.source.get(self.pos).copied()
    }
    fn step(&mut self) -> Result<(), Diagnostic> {
        self.pos = self
            .pos
            .checked_add(1)
            .ok_or(self.error(Code::InvalidState))?;
        Ok(())
    }
    fn spaces(&mut self) -> Result<(), Diagnostic> {
        while matches!(self.peek(), Some(b' ' | b'\t')) {
            self.step()?;
        }
        Ok(())
    }
    fn name(&mut self) -> Result<std::ops::Range<usize>, Diagnostic> {
        let start = self.pos;
        if !matches!(self.peek(), Some(b'a'..=b'z')) {
            return Err(self.error(Code::ExpectedName));
        }
        while matches!(self.peek(), Some(b'a'..=b'z' | b'0'..=b'9' | b'_')) {
            if self.pos.saturating_sub(start) >= MAX_NAME_BYTES {
                return Err(self.error(Code::NameTooLong));
            }
            self.step()?;
        }
        Ok(start..self.pos)
    }
    fn quoted(&mut self) -> Result<usize, Diagnostic> {
        if self.peek() != Some(b'"') {
            return Err(self.error(Code::ExpectedValue));
        }
        self.step()?;
        let mut length = 0usize;
        loop {
            let Some(mut byte) = self.peek() else {
                return Err(self.error(Code::UnterminatedString));
            };
            if byte == b'"' {
                self.step()?;
                return Ok(length);
            }
            if byte < 0x20 || byte == 0x7f {
                return Err(self.error(Code::ControlCharacter));
            }
            let source_start = self.pos;
            if byte == b'\\' {
                let slash = self.pos;
                self.step()?;
                byte = match self.peek() {
                    Some(b'"') => b'"',
                    Some(b'\\') => b'\\',
                    _ => return Err(diagnostic(Code::InvalidEscape, self.line, slash)),
                };
            }
            if length >= MAX_STRING_BYTES {
                return Err(diagnostic(Code::StringTooLong, self.line, source_start));
            }
            let error = self.error(Code::Capacity);
            *self.scratch.get_mut(length).ok_or(error)? = byte;
            length = length
                .checked_add(1)
                .ok_or(self.error(Code::InvalidState))?;
            self.step()?;
        }
    }
    fn tail(&mut self) -> Result<(), Diagnostic> {
        self.spaces()?;
        match self.peek() {
            None | Some(b'#') => Ok(()),
            _ => Err(self.error(Code::TrailingData)),
        }
    }
}

/// Parses one physical line and borrows the source name plus decoded scratch.
/// Caller errors never contain source bytes; scratch tails are not erased.
pub fn parse_line<'a>(
    number: NonZeroU32,
    raw: &'a [u8],
    scratch: &'a mut [u8],
) -> Result<Statement<'a>, Diagnostic> {
    if raw.len() > MAX_LINE_BYTES {
        return Err(diagnostic(Code::LineTooLong, number, MAX_LINE_BYTES));
    }
    let raw = if let Some(without_lf) = raw.strip_suffix(b"\n") {
        without_lf.strip_suffix(b"\r").unwrap_or(without_lf)
    } else {
        raw
    };
    let text = std::str::from_utf8(raw)
        .map_err(|e| diagnostic(Code::InvalidUtf8, number, e.valid_up_to()))?;
    if let Some((offset, _)) = text.char_indices().find(|(_, c)| {
        (c.is_control() && *c != '\t')
            || matches!(*c, '\u{2028}' | '\u{2029}' | '\u{061c}' | '\u{200e}' | '\u{200f}'
                | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
    }) {
        return Err(diagnostic(Code::ControlCharacter, number, offset));
    }
    let mut c = Cursor {
        source: raw,
        scratch,
        pos: 0,
        line: number,
    };
    c.spaces()?;
    let location = diagnostic(Code::InvalidState, number, c.pos).location;
    if matches!(c.peek(), None | Some(b'#')) {
        return Ok(Statement::Empty);
    }
    if c.peek() == Some(b'[') {
        c.step()?;
        let name = c.name()?;
        let before = c.pos;
        c.spaces()?;
        let label = if c.peek() == Some(b'"') {
            if before == c.pos {
                return Err(c.error(Code::ExpectedSpace));
            }
            Some(c.quoted()?)
        } else {
            None
        };
        c.spaces()?;
        if c.peek() != Some(b']') {
            return Err(c.error(Code::ExpectedBracket));
        }
        c.step()?;
        c.tail()?;
        let error = c.error(Code::InvalidState);
        let name = text.get(name).ok_or(error)?;
        let label = match label {
            Some(length) => Some(
                std::str::from_utf8(c.scratch.get(..length).ok_or(error)?).map_err(|_| error)?,
            ),
            None => None,
        };
        return Ok(Statement::Section {
            location,
            name,
            label,
        });
    }
    let key = c.name()?;
    c.spaces()?;
    if c.peek() != Some(b'=') {
        return Err(c.error(Code::ExpectedEquals));
    }
    c.step()?;
    c.spaces()?;
    let value_location = diagnostic(Code::InvalidState, number, c.pos).location;
    let (string_length, value) = if c.peek() == Some(b'"') {
        (Some(c.quoted()?), Value::Text(""))
    } else {
        let start = c.pos;
        while !matches!(c.peek(), None | Some(b' ' | b'\t' | b'#')) {
            c.step()?;
        }
        let word = text.get(start..c.pos).ok_or(c.error(Code::InvalidState))?;
        let value = match word {
            "true" => Value::Boolean(true),
            "false" => Value::Boolean(false),
            _ if !word.is_empty() && word.bytes().all(|b| b.is_ascii_digit()) => {
                if word.len() > 1 && word.starts_with('0') {
                    return Err(diagnostic(Code::InvalidInteger, number, start));
                }
                Value::Integer(
                    word.parse()
                        .map_err(|_| diagnostic(Code::InvalidInteger, number, start))?,
                )
            }
            _ => return Err(diagnostic(Code::ExpectedValue, number, start)),
        };
        (None, value)
    };
    c.tail()?;
    let error = c.error(Code::InvalidState);
    let key = text.get(key).ok_or(error)?;
    let value = match string_length {
        Some(length) => Value::Text(
            std::str::from_utf8(c.scratch.get(..length).ok_or(error)?).map_err(|_| error)?,
        ),
        None => value,
    };
    Ok(Statement::Assignment {
        location,
        key,
        value_location,
        value,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    fn parse<'a>(raw: &'a [u8], scratch: &'a mut [u8]) -> Result<Statement<'a>, Diagnostic> {
        parse_line(NonZeroU32::MIN, raw, scratch)
    }
    #[test]
    fn framing_every_chunk_size_preserves_physical_lines_and_utf8() {
        let input = "# comment\r\nversion=1\n[alias \"a@example.test\"]\nlabel=\"é 🦀\"".as_bytes();
        let expected: Vec<&[u8]> = input.split_inclusive(|b| *b == b'\n').collect();
        for chunk_size in 1..=input.len() {
            let mut storage = [0; MAX_LINE_BYTES];
            let mut framer = Framer::new(&mut storage).unwrap();
            let mut lines = Vec::new();
            for chunk in input.chunks(chunk_size) {
                let mut remaining = chunk;
                while !remaining.is_empty() {
                    let count = framer.feed(remaining).unwrap();
                    remaining = &remaining[count..];
                    if let Some((number, line)) = framer.line().unwrap() {
                        assert_eq!(number.get() as usize, lines.len() + 1);
                        lines.push(line.to_vec());
                        framer.advance().unwrap();
                    }
                }
            }
            assert!(!framer.is_finished());
            framer.finish().unwrap();
            if let Some((number, line)) = framer.line().unwrap() {
                assert_eq!(number.get() as usize, lines.len() + 1);
                lines.push(line.to_vec());
                framer.advance().unwrap();
            }
            assert!(framer.is_finished());
            assert_eq!(framer.consumed_bytes(), input.len());
            assert_eq!(
                lines.iter().map(Vec::as_slice).collect::<Vec<_>>(),
                expected
            );
            framer.finish().unwrap();
            assert_eq!(framer.feed(&[]), Ok(0));
        }
    }
    #[test]
    fn empty_stream_has_no_physical_lines() {
        let mut storage = [0; MAX_LINE_BYTES];
        let mut framer = Framer::new(&mut storage).unwrap();
        assert_eq!(framer.feed(&[]), Ok(0));
        assert_eq!(framer.line(), Ok(None));
        assert!(!framer.is_finished());
        framer.finish().unwrap();
        assert!(framer.is_finished());
        assert_eq!(framer.line(), Ok(None));
        assert_eq!(framer.consumed_bytes(), 0);
        framer.finish().unwrap();
        assert!(framer.is_finished());
    }
    #[test]
    fn framer_saturation_eof_and_wrong_order_are_sticky() {
        let mut short = [0xaa; MAX_LINE_BYTES - 1];
        assert!(matches!(
            Framer::new(&mut short),
            Err(Diagnostic {
                code: Code::Capacity,
                ..
            })
        ));
        assert!(short.iter().all(|b| *b == 0xaa));
        let mut storage = [0; MAX_LINE_BYTES];
        let mut f = Framer::new(&mut storage).unwrap();
        assert_eq!(f.feed(b"a=1\nignored"), Ok(4));
        assert_eq!(f.feed(b"next"), Ok(0));
        assert_eq!(f.line().unwrap().unwrap().1, b"a=1\n");
        f.finish().unwrap();
        f.advance().unwrap();
        assert!(f.is_finished());
        let e = f.feed(b"x").unwrap_err();
        assert_eq!(e.code, Code::InvalidState);
        assert_eq!(f.finish(), Err(e));
        assert_eq!(f.line(), Err(e));
        assert!(!f.is_finished());
        let mut storage = [0; MAX_LINE_BYTES];
        let mut f = Framer::new(&mut storage).unwrap();
        let e = f.advance().unwrap_err();
        assert_eq!(f.feed(b"valid=1\n"), Err(e));
        let mut storage = [0; MAX_LINE_BYTES];
        let mut f = Framer::new(&mut storage).unwrap();
        assert_eq!(f.feed(&[b'x'; MAX_LINE_BYTES]), Ok(MAX_LINE_BYTES));
        let e = f.feed(b"\n").unwrap_err();
        assert_eq!(e.code, Code::LineTooLong);
        assert_eq!(usize::from(e.location.column), MAX_LINE_BYTES + 1);
        assert_eq!(f.finish(), Err(e));
        assert_eq!(f.consumed_bytes(), MAX_LINE_BYTES);
    }
    #[test]
    fn file_byte_and_line_caps_accept_exactly_their_bounds() {
        for overflow in [false, true] {
            let mut storage = [0; MAX_LINE_BYTES];
            let mut f = Framer::new(&mut storage).unwrap();
            let mut line = [b'#'; MAX_LINE_BYTES];
            line[MAX_LINE_BYTES - 1] = b'\n';
            for _ in 0..(MAX_INPUT_BYTES / MAX_LINE_BYTES) {
                assert_eq!(f.feed(&line), Ok(MAX_LINE_BYTES));
                f.advance().unwrap();
            }
            assert_eq!(f.consumed_bytes(), MAX_INPUT_BYTES);
            if overflow {
                let e = f.feed(b"#").unwrap_err();
                assert_eq!(e.code, Code::InputTooLarge);
                assert_eq!(f.finish(), Err(e));
            } else {
                f.finish().unwrap();
                assert!(f.is_finished());
            }
            let mut storage = [0; MAX_LINE_BYTES];
            let mut f = Framer::new(&mut storage).unwrap();
            for _ in 0..MAX_LINES {
                assert_eq!(f.feed(b"\n"), Ok(1));
                f.advance().unwrap();
            }
            if overflow {
                let e = f.feed(b"#").unwrap_err();
                assert_eq!(e.code, Code::TooManyLines);
                assert_eq!(e.location.line.get(), MAX_LINES + 1);
            } else {
                f.finish().unwrap();
                assert!(f.is_finished());
            }
        }
    }
    #[test]
    fn syntax_literal_oracles_cover_headers_values_comments_and_locations() {
        let mut out = [0; MAX_STRING_BYTES];
        let loc = Location {
            line: NonZeroU32::MIN,
            column: 1,
        };
        assert_eq!(
            parse(b" \t# private comment", &mut out),
            Ok(Statement::Empty)
        );
        assert_eq!(
            parse(b"[server]\r\n", &mut out),
            Ok(Statement::Section {
                location: loc,
                name: "server",
                label: None
            })
        );
        assert_eq!(
            parse(b"[alias \"a@example.test\"] # comment\n", &mut out),
            Ok(Statement::Section {
                location: loc,
                name: "alias",
                label: Some("a@example.test")
            })
        );
        assert_eq!(
            parse(b"n=18446744073709551615", &mut out),
            Ok(Statement::Assignment {
                location: loc,
                key: "n",
                value_location: Location { column: 3, ..loc },
                value: Value::Integer(u64::MAX)
            })
        );
        assert_eq!(
            parse(b"n=0#comment", &mut out),
            Ok(Statement::Assignment {
                location: loc,
                key: "n",
                value_location: Location { column: 3, ..loc },
                value: Value::Integer(0)
            })
        );
        for (raw, value) in [
            (b"on=true".as_slice(), true),
            (b"on=false".as_slice(), false),
        ] {
            assert_eq!(
                parse(raw, &mut out),
                Ok(Statement::Assignment {
                    location: loc,
                    key: "on",
                    value_location: Location { column: 4, ..loc },
                    value: Value::Boolean(value)
                })
            );
        }
        let raw = r#"subject = "a # [ ] \" \\ é 🦀" # ignored"#;
        assert_eq!(
            parse(raw.as_bytes(), &mut out),
            Ok(Statement::Assignment {
                location: loc,
                key: "subject",
                value_location: Location { column: 11, ..loc },
                value: Value::Text("a # [ ] \" \\ é 🦀")
            })
        );
        let statement = parse(b"  password = \"private_fixture\"", &mut out).unwrap();
        assert_eq!(format!("{statement:?}"), "Assignment(<redacted>)");
        assert_eq!(
            format!("{:?}", Value::Text("private_fixture")),
            "Text(<redacted>)"
        );
    }
    #[test]
    fn malformed_values_never_echo_source_and_have_precise_byte_locations() {
        let cases: &[(&[u8], Code, u16)] = &[
            (b"token= private_fixture", Code::ExpectedValue, 8),
            (b"a=01", Code::InvalidInteger, 3),
            (b"a=18446744073709551616", Code::InvalidInteger, 3),
            (b"a=-1", Code::ExpectedValue, 3),
            (b"a=1_000", Code::ExpectedValue, 3),
            (b"a=truex", Code::ExpectedValue, 3),
            (b"a=\"x\\q\"", Code::InvalidEscape, 5),
            (b"a=\"x", Code::UnterminatedString, 5),
            (b"a=\"x\\", Code::InvalidEscape, 5),
            (b"a 1", Code::ExpectedEquals, 3),
            (b"[alias\"a\"]", Code::ExpectedSpace, 7),
            (b"[alias \"a\"", Code::ExpectedBracket, 11),
            (b"a=1 extra", Code::TrailingData, 5),
            (b"[server] trailing", Code::TrailingData, 10),
            (b"[ server]", Code::ExpectedName, 2),
            (b"a=[]", Code::ExpectedValue, 3),
        ];
        let mut scratch = [0; MAX_STRING_BYTES];
        for &(raw, code, column) in cases {
            let e = parse(raw, &mut scratch).unwrap_err();
            assert_eq!(e.code, code, "{raw:?}");
            assert_eq!(e.location.column, column, "{raw:?}");
            assert!(!format!("{e} {e:?}").contains("private_fixture"));
        }
        let e = parse(b"token= private_fixture", &mut scratch).unwrap_err();
        assert_eq!(
            e.to_string(),
            "config_expected_value at line 1, byte column 8"
        );
    }
    #[test]
    fn utf8_and_control_validation_includes_comments_and_line_endings() {
        let mut scratch = [0; MAX_STRING_BYTES];
        for (raw, code, column) in [
            (b"a=\"\xff\"".as_slice(), Code::InvalidUtf8, 4),
            (b"#\xff".as_slice(), Code::InvalidUtf8, 2),
            (b"#\0".as_slice(), Code::ControlCharacter, 2),
            (b"a=1\r".as_slice(), Code::ControlCharacter, 4),
            (b"a=1\nnext".as_slice(), Code::ControlCharacter, 4),
            (b"a=\"\t\"".as_slice(), Code::ControlCharacter, 4),
            (b"#\x7f".as_slice(), Code::ControlCharacter, 2),
            ("\u{feff}version=1".as_bytes(), Code::ExpectedName, 1),
        ] {
            let e = parse(raw, &mut scratch).unwrap_err();
            assert_eq!(e.code, code);
            assert_eq!(e.location.column, column);
        }
        assert!(parse(b"a\t=\t1\r\n", &mut scratch).is_ok());
    }
    #[test]
    fn string_name_and_output_caps_do_not_silently_truncate() {
        let mut scratch = [0; MAX_STRING_BYTES];
        let name = "a".repeat(MAX_NAME_BYTES);
        let valid = format!("{name}=1");
        assert!(parse(valid.as_bytes(), &mut scratch).is_ok());
        let invalid = format!("{name}a=1");
        assert_eq!(
            parse(invalid.as_bytes(), &mut scratch).unwrap_err().code,
            Code::NameTooLong
        );
        for count in [MAX_STRING_BYTES, MAX_STRING_BYTES + 1] {
            let raw = format!("a=\"{}\"", "x".repeat(count));
            let result = parse(raw.as_bytes(), &mut scratch);
            if count == MAX_STRING_BYTES {
                assert!(
                    matches!(result,Ok(Statement::Assignment { value:Value::Text(text),.. }) if text.len()==count)
                );
            } else {
                assert_eq!(result.unwrap_err().code, Code::StringTooLong);
            }
        }
        let mut empty = [];
        assert!(matches!(
            parse(b"a=\"\"", &mut empty),
            Ok(Statement::Assignment {
                value: Value::Text(""),
                ..
            })
        ));
        assert_eq!(
            parse(b"a=\"x\"", &mut empty).unwrap_err().code,
            Code::Capacity
        );
        let too_long = vec![b'#'; MAX_LINE_BYTES + 1];
        assert_eq!(
            parse(&too_long, &mut scratch).unwrap_err().code,
            Code::LineTooLong
        );
    }
    #[test]
    fn arbitrary_single_bytes_and_truncations_never_panic() {
        let mut scratch = [0; MAX_STRING_BYTES];
        let examples = [
            b"[alias \"a@example.test\"]".as_slice(),
            b"name=\"quoted \\\" value\"",
            b"limit=18446744073709551615",
        ];
        for example in examples {
            for end in 0..=example.len() {
                let _ = parse(&example[..end], &mut scratch);
            }
            for position in 0..example.len() {
                let mut changed = example.to_vec();
                for byte in u8::MIN..=u8::MAX {
                    changed[position] = byte;
                    let _ = parse(&changed, &mut scratch);
                }
            }
        }
    }
    #[test]
    fn physical_line_edges_and_labels_have_literal_oracles() {
        let mut scratch = [0; MAX_STRING_BYTES];
        let mut exact = vec![b'#'; MAX_LINE_BYTES];
        exact[MAX_LINE_BYTES - 1] = b'\n';
        assert_eq!(parse(&exact, &mut scratch), Ok(Statement::Empty));
        exact[MAX_LINE_BYTES - 2] = b'\r';
        assert_eq!(parse(&exact, &mut scratch), Ok(Statement::Empty));
        exact.insert(1, b'#');
        assert_eq!(
            parse(&exact, &mut scratch).unwrap_err().code,
            Code::LineTooLong
        );
        for (raw, label) in [(r#"[alias ""]"#, ""), (r#"[alias "a\\b\"c"]"#, "a\\b\"c")] {
            assert!(
                matches!(parse(raw.as_bytes(), &mut scratch), Ok(Statement::Section { label: Some(text), .. }) if text == label)
            );
        }
        for count in [MAX_STRING_BYTES, MAX_STRING_BYTES + 1] {
            let raw = format!("[alias \"{}\"]", "x".repeat(count));
            let result = parse(raw.as_bytes(), &mut scratch);
            if count == MAX_STRING_BYTES {
                assert!(
                    matches!(result, Ok(Statement::Section { label: Some(text), .. }) if text.len() == count)
                );
            } else {
                assert_eq!(result.unwrap_err().code, Code::StringTooLong);
            }
        }
        let raw = format!(r#"a="{}\"""#, "x".repeat(MAX_STRING_BYTES));
        let e = parse(raw.as_bytes(), &mut scratch).unwrap_err();
        assert_eq!(e.code, Code::StringTooLong);
        assert_eq!(usize::from(e.location.column), 4 + MAX_STRING_BYTES);
    }
    #[test]
    fn unicode_controls_line_separators_and_direction_changes_are_rejected() {
        let mut scratch = [0; MAX_STRING_BYTES];
        for c in [
            '\u{85}', '\u{9b}', '\u{2028}', '\u{2029}', '\u{61c}', '\u{200e}', '\u{200f}',
            '\u{202a}', '\u{202e}', '\u{2066}', '\u{2069}',
        ] {
            for raw in [format!("# é{c}setting=true"), format!("a=\"é{c}x\"")] {
                let e = parse(raw.as_bytes(), &mut scratch).unwrap_err();
                assert_eq!(e.code, Code::ControlCharacter);
                assert_eq!(e.location.column, if raw.starts_with('#') { 5 } else { 6 });
            }
        }
        assert!(parse("a=\"عربي 🧑‍💻\"".as_bytes(), &mut scratch).is_ok());
    }
    #[test]
    fn version_one_diagnostic_names_are_fixed() {
        let cases = [
            (Code::Capacity, "config_capacity"),
            (Code::InvalidState, "config_invalid_state"),
            (Code::InputTooLarge, "config_input_too_large"),
            (Code::TooManyLines, "config_too_many_lines"),
            (Code::LineTooLong, "config_line_too_long"),
            (Code::InvalidUtf8, "config_invalid_utf8"),
            (Code::ControlCharacter, "config_control_character"),
            (Code::ExpectedName, "config_expected_name"),
            (Code::NameTooLong, "config_name_too_long"),
            (Code::ExpectedEquals, "config_expected_equals"),
            (Code::ExpectedValue, "config_expected_value"),
            (Code::InvalidInteger, "config_invalid_integer"),
            (Code::InvalidEscape, "config_invalid_escape"),
            (Code::UnterminatedString, "config_unterminated_string"),
            (Code::StringTooLong, "config_string_too_long"),
            (Code::ExpectedBracket, "config_expected_bracket"),
            (Code::ExpectedSpace, "config_expected_space"),
            (Code::TrailingData, "config_trailing_data"),
        ];
        for (code, name) in cases {
            assert_eq!(code.name(), name);
        }
    }
}

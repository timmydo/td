//! Incremental JSON structure and key scanner for large retained values.
//! Emits offsets and decoded key scalars without retaining strings or values.
//! Unlike the arena parser it does not reject duplicate keys; like that parser
//! it applies no I-JSON scalar policy. Retained-response producers own those rules.
#![cfg_attr(clippy, deny(warnings))]
use std::fmt;

/// Scanner frame capacity, independent of the arena parser's 128-frame limit.
pub const MAX_DEPTH: usize = 160;
/// Maximum consumed input bytes in one poll.
pub const TURN_BYTES: usize = 4096;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Syntax,
    Depth,
    Bytes,
    /// Invalid constructor limits or input supplied after completion.
    State,
}
impl fmt::Display for Error {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.write_str(match self {
            Self::Syntax => "invalid streamed JSON",
            Self::Depth => "streamed JSON nesting limit",
            Self::Bytes => "streamed JSON byte limit",
            Self::State => "invalid JSON scanner state",
        })
    }
}
impl std::error::Error for Error {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Kind {
    Object,
    Array,
    String,
    Number,
    Boolean,
    Null,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Event {
    /// Inclusive byte offset of the value's first byte.
    Start {
        kind: Kind,
        offset: u64,
    },
    /// Exclusive byte offset; pairs with Start at the same structural depth.
    End {
        kind: Kind,
        offset: u64,
    },
    /// The opening quote is at Scanner::offset() - 1 after this event.
    KeyStart,
    KeyChar(char),
    /// Scanner::offset() is immediately after the closing quote.
    KeyEnd,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Progress {
    pub consumed: usize,
    pub event: Option<Event>,
    pub complete: bool,
}
#[derive(Clone, Copy)]
enum Stage {
    ObjectFirst,
    ObjectKey,
    Colon,
    Value,
    ArrayFirst,
    ArrayValue,
    Comma,
}
#[derive(Clone, Copy)]
struct Frame {
    kind: Kind,
    stage: Stage,
}
const EMPTY: Frame = Frame {
    kind: Kind::Array,
    stage: Stage::ArrayFirst,
};
#[derive(Clone, Copy)]
enum Number {
    Minus,
    Zero,
    Integer,
    Dot,
    Fraction,
    Exponent,
    Sign,
    ExponentDigits,
}
impl Number {
    fn terminal(self) -> bool {
        matches!(
            self,
            Self::Zero | Self::Integer | Self::Fraction | Self::ExponentDigits
        )
    }
    fn next(self, byte: u8) -> Option<Self> {
        match (self, byte) {
            (Self::Minus, b'0') => Some(Self::Zero),
            (Self::Minus, b'1'..=b'9') => Some(Self::Integer),
            (Self::Integer, b'0'..=b'9') => Some(Self::Integer),
            (Self::Zero | Self::Integer, b'.') => Some(Self::Dot),
            (Self::Dot | Self::Fraction, b'0'..=b'9') => Some(Self::Fraction),
            (Self::Zero | Self::Integer | Self::Fraction, b'e' | b'E') => Some(Self::Exponent),
            (Self::Exponent, b'+' | b'-') => Some(Self::Sign),
            (Self::Exponent | Self::Sign | Self::ExponentDigits, b'0'..=b'9') => {
                Some(Self::ExponentDigits)
            }
            _ => None,
        }
    }
}
#[derive(Clone, Copy)]
enum StringMode {
    Plain,
    Escape,
    Hex {
        value: u16,
        digits: u8,
        high: Option<u16>,
    },
    LowSlash(u16),
    LowU(u16),
    Utf8 {
        bytes: [u8; 4],
        used: u8,
        length: u8,
    },
}
#[derive(Clone, Copy)]
enum Scalar {
    None,
    String {
        key: bool,
        mode: StringMode,
    },
    Number(Number),
    Literal {
        bytes: &'static [u8],
        next: usize,
        kind: Kind,
    },
}

/// Owns only bounded lexical/structural state. Feed one immutable byte stream in
/// order and honor consumed; keep any unconsumed suffix for the next poll.
/// `eof` means no bytes follow the supplied slice, not merely a short read.
/// Each poll inspects at most 4096 bytes and emits at most one event. Completion
/// requires EOF and a complete root, including trailing-whitespace validation.
/// Events remain provisional until complete; malformed tails invalidate them.
/// Consumers must discard all provisional selections on any subsequent error.
/// All failures are sticky. No file, allocation, source identity or work grant
/// is created here; the enclosing retained-file owner supplies those bounds.
pub struct Scanner {
    frames: [Frame; MAX_DEPTH],
    depth: usize,
    depth_limit: usize,
    byte_limit: u64,
    offset: u64,
    started: bool,
    root_done: bool,
    complete: bool,
    scalar: Scalar,
    failure: Option<Error>,
}
impl fmt::Debug for Scanner {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.write_str("Scanner { .. }")
    }
}
impl Scanner {
    pub fn new(byte_limit: u64, depth_limit: usize) -> Result<Self, Error> {
        if byte_limit == 0 || depth_limit == 0 || depth_limit > MAX_DEPTH {
            return Err(Error::State);
        }
        Ok(Self {
            frames: [EMPTY; MAX_DEPTH],
            depth: 0,
            depth_limit,
            byte_limit,
            offset: 0,
            started: false,
            root_done: false,
            complete: false,
            scalar: Scalar::None,
            failure: None,
        })
    }
    /// Bytes consumed so far, not an error location. A failed string byte may
    /// already be counted; offset after an error is diagnostic only.
    pub fn offset(&self) -> u64 {
        self.offset
    }
    pub fn poll(&mut self, input: &[u8], eof: bool) -> Result<Progress, Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = self.inner(input, eof);
        if let Err(error) = result {
            self.failure = Some(error);
        }
        result
    }
    fn inner(&mut self, input: &[u8], eof: bool) -> Result<Progress, Error> {
        if self.complete {
            return if input.is_empty() {
                Ok(Progress {
                    consumed: 0,
                    event: None,
                    complete: true,
                })
            } else {
                Err(Error::State)
            };
        }
        let mut used = 0;
        loop {
            if used == TURN_BYTES {
                return Ok(progress(used, None, false));
            }
            let Some(&byte) = input.get(used) else {
                if !eof {
                    return Ok(progress(used, None, false));
                }
                if let Scalar::Number(number) = self.scalar {
                    if !number.terminal() {
                        return Err(Error::Syntax);
                    }
                    let event = self.end_scalar(Kind::Number);
                    return Ok(progress(used, Some(event), false));
                }
                if !matches!(self.scalar, Scalar::None) || !self.root_done || self.depth != 0 {
                    return Err(Error::Syntax);
                }
                self.complete = true;
                return Ok(progress(used, None, true));
            };
            if self.offset >= self.byte_limit {
                return Err(Error::Bytes);
            }
            match self.scalar {
                Scalar::Number(number) => {
                    if let Some(next) = number.next(byte) {
                        self.scalar = Scalar::Number(next);
                        self.consume(&mut used)?;
                    } else if number.terminal() && delimiter(byte) {
                        let event = self.end_scalar(Kind::Number);
                        return Ok(progress(used, Some(event), false));
                    } else {
                        return Err(Error::Syntax);
                    }
                }
                Scalar::Literal { bytes, next, kind } => {
                    if bytes.get(next) != Some(&byte) {
                        return Err(Error::Syntax);
                    }
                    self.consume(&mut used)?;
                    if next + 1 == bytes.len() {
                        let event = self.end_scalar(kind);
                        return Ok(progress(used, Some(event), false));
                    }
                    self.scalar = Scalar::Literal {
                        bytes,
                        next: next + 1,
                        kind,
                    };
                }
                Scalar::String { key, mode } => {
                    self.consume(&mut used)?;
                    if let Some(event) = self.string(byte, key, mode)? {
                        return Ok(progress(used, Some(event), false));
                    }
                }
                Scalar::None => {
                    if whitespace(byte) {
                        self.consume(&mut used)?;
                        continue;
                    }
                    if self.root_done {
                        return Err(Error::Syntax);
                    }
                    let frame = self
                        .depth
                        .checked_sub(1)
                        .and_then(|i| self.frames.get(i))
                        .copied();
                    match frame {
                        Some(Frame {
                            kind: Kind::Object,
                            stage: Stage::ObjectFirst,
                        }) if byte == b'}' => {
                            self.consume(&mut used)?;
                            return Ok(progress(
                                used,
                                Some(self.end_container(Kind::Object)?),
                                false,
                            ));
                        }
                        Some(Frame {
                            kind: Kind::Object,
                            stage: Stage::ObjectFirst | Stage::ObjectKey,
                        }) => {
                            if byte != b'"' {
                                return Err(Error::Syntax);
                            }
                            self.consume(&mut used)?;
                            self.scalar = Scalar::String {
                                key: true,
                                mode: StringMode::Plain,
                            };
                            return Ok(progress(used, Some(Event::KeyStart), false));
                        }
                        Some(Frame {
                            stage: Stage::Colon,
                            ..
                        }) => {
                            if byte != b':' {
                                return Err(Error::Syntax);
                            }
                            self.consume(&mut used)?;
                            self.top()?.stage = Stage::Value;
                        }
                        Some(Frame {
                            kind,
                            stage: Stage::Comma,
                        }) => {
                            let close = if kind == Kind::Object { b'}' } else { b']' };
                            if byte == close {
                                self.consume(&mut used)?;
                                return Ok(progress(used, Some(self.end_container(kind)?), false));
                            }
                            if byte != b',' {
                                return Err(Error::Syntax);
                            }
                            self.consume(&mut used)?;
                            self.top()?.stage = if kind == Kind::Object {
                                Stage::ObjectKey
                            } else {
                                Stage::ArrayValue
                            };
                        }
                        Some(Frame {
                            kind: Kind::Array,
                            stage: Stage::ArrayFirst,
                        }) if byte == b']' => {
                            self.consume(&mut used)?;
                            return Ok(progress(
                                used,
                                Some(self.end_container(Kind::Array)?),
                                false,
                            ));
                        }
                        Some(Frame {
                            stage: Stage::Value | Stage::ArrayFirst | Stage::ArrayValue,
                            ..
                        })
                        | None => {
                            let offset = self.offset;
                            let kind = self.start_value(byte)?;
                            self.consume(&mut used)?;
                            return Ok(progress(used, Some(Event::Start { kind, offset }), false));
                        }
                        _ => return Err(Error::State),
                    }
                }
            }
        }
    }
    fn consume(&mut self, used: &mut usize) -> Result<(), Error> {
        self.offset = self.offset.checked_add(1).ok_or(Error::Bytes)?;
        *used += 1;
        Ok(())
    }
    fn top(&mut self) -> Result<&mut Frame, Error> {
        self.depth
            .checked_sub(1)
            .and_then(|i| self.frames.get_mut(i))
            .ok_or(Error::State)
    }
    fn start_value(&mut self, byte: u8) -> Result<Kind, Error> {
        if self.depth == 0 {
            if self.started {
                return Err(Error::Syntax);
            }
            self.started = true;
        } else {
            self.top()?.stage = Stage::Comma;
        }
        let kind = match byte {
            b'{' => Kind::Object,
            b'[' => Kind::Array,
            b'"' => {
                self.scalar = Scalar::String {
                    key: false,
                    mode: StringMode::Plain,
                };
                Kind::String
            }
            b'-' => {
                self.scalar = Scalar::Number(Number::Minus);
                Kind::Number
            }
            b'0' => {
                self.scalar = Scalar::Number(Number::Zero);
                Kind::Number
            }
            b'1'..=b'9' => {
                self.scalar = Scalar::Number(Number::Integer);
                Kind::Number
            }
            b't' => {
                self.scalar = Scalar::Literal {
                    bytes: b"true",
                    next: 1,
                    kind: Kind::Boolean,
                };
                Kind::Boolean
            }
            b'f' => {
                self.scalar = Scalar::Literal {
                    bytes: b"false",
                    next: 1,
                    kind: Kind::Boolean,
                };
                Kind::Boolean
            }
            b'n' => {
                self.scalar = Scalar::Literal {
                    bytes: b"null",
                    next: 1,
                    kind: Kind::Null,
                };
                Kind::Null
            }
            _ => return Err(Error::Syntax),
        };
        if matches!(kind, Kind::Object | Kind::Array) {
            if self.depth == self.depth_limit {
                return Err(Error::Depth);
            }
            *self.frames.get_mut(self.depth).ok_or(Error::Depth)? = Frame {
                kind,
                stage: if kind == Kind::Object {
                    Stage::ObjectFirst
                } else {
                    Stage::ArrayFirst
                },
            };
            self.depth += 1;
        }
        Ok(kind)
    }
    fn end_container(&mut self, kind: Kind) -> Result<Event, Error> {
        self.depth = self.depth.checked_sub(1).ok_or(Error::State)?;
        if self.depth == 0 {
            self.root_done = true;
        }
        Ok(Event::End {
            kind,
            offset: self.offset,
        })
    }
    fn end_scalar(&mut self, kind: Kind) -> Event {
        self.scalar = Scalar::None;
        if self.depth == 0 {
            self.root_done = true;
        }
        Event::End {
            kind,
            offset: self.offset,
        }
    }
    fn string(&mut self, byte: u8, key: bool, mode: StringMode) -> Result<Option<Event>, Error> {
        let mut next = StringMode::Plain;
        let mut character = None;
        match mode {
            StringMode::Plain => match byte {
                b'"' => {
                    if key {
                        self.scalar = Scalar::None;
                        self.top()?.stage = Stage::Colon;
                        return Ok(Some(Event::KeyEnd));
                    }
                    return Ok(Some(self.end_scalar(Kind::String)));
                }
                b'\\' => next = StringMode::Escape,
                0..=0x1f => return Err(Error::Syntax),
                0x20..=0x7f => character = Some(char::from(byte)),
                _ => {
                    let length = match byte {
                        0xc2..=0xdf => 2,
                        0xe0..=0xef => 3,
                        0xf0..=0xf4 => 4,
                        _ => return Err(Error::Syntax),
                    };
                    next = StringMode::Utf8 {
                        bytes: [byte, 0, 0, 0],
                        used: 1,
                        length,
                    };
                }
            },
            StringMode::Escape => match byte {
                b'"' | b'\\' | b'/' => character = Some(char::from(byte)),
                b'b' => character = Some('\u{8}'),
                b'f' => character = Some('\u{c}'),
                b'n' => character = Some('\n'),
                b'r' => character = Some('\r'),
                b't' => character = Some('\t'),
                b'u' => {
                    next = StringMode::Hex {
                        value: 0,
                        digits: 0,
                        high: None,
                    }
                }
                _ => return Err(Error::Syntax),
            },
            StringMode::LowSlash(high) => {
                if byte != b'\\' {
                    return Err(Error::Syntax);
                }
                next = StringMode::LowU(high);
            }
            StringMode::LowU(high) => {
                if byte != b'u' {
                    return Err(Error::Syntax);
                }
                next = StringMode::Hex {
                    value: 0,
                    digits: 0,
                    high: Some(high),
                };
            }
            StringMode::Hex {
                value,
                digits,
                high,
            } => {
                let digit = char::from(byte).to_digit(16).ok_or(Error::Syntax)? as u16;
                let value = (value << 4) | digit;
                if digits < 3 {
                    next = StringMode::Hex {
                        value,
                        digits: digits + 1,
                        high,
                    };
                } else if let Some(high) = high {
                    if !(0xdc00..=0xdfff).contains(&value) {
                        return Err(Error::Syntax);
                    }
                    character = char::from_u32(
                        0x10000 + ((u32::from(high) - 0xd800) << 10) + (u32::from(value) - 0xdc00),
                    );
                } else if (0xd800..=0xdbff).contains(&value) {
                    next = StringMode::LowSlash(value);
                } else {
                    character = Some(char::from_u32(u32::from(value)).ok_or(Error::Syntax)?);
                }
            }
            StringMode::Utf8 {
                mut bytes,
                used,
                length,
            } => {
                if byte & 0xc0 != 0x80 {
                    return Err(Error::Syntax);
                }
                *bytes.get_mut(usize::from(used)).ok_or(Error::State)? = byte;
                let used = used + 1;
                if used == length {
                    character = Some(
                        std::str::from_utf8(bytes.get(..usize::from(length)).ok_or(Error::State)?)
                            .map_err(|_| Error::Syntax)?
                            .chars()
                            .next()
                            .ok_or(Error::State)?,
                    );
                } else {
                    next = StringMode::Utf8 {
                        bytes,
                        used,
                        length,
                    };
                }
            }
        }
        self.scalar = Scalar::String { key, mode: next };
        Ok(if key {
            character.map(Event::KeyChar)
        } else {
            None
        })
    }
}
fn progress(consumed: usize, event: Option<Event>, complete: bool) -> Progress {
    Progress {
        consumed,
        event,
        complete,
    }
}
fn whitespace(byte: u8) -> bool {
    matches!(byte, b' ' | b'\n' | b'\r' | b'\t')
}
fn delimiter(byte: u8) -> bool {
    whitespace(byte) || matches!(byte, b',' | b']' | b'}')
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::*;

    fn scan(input: &[u8], width: usize, bytes: u64, depth: usize) -> Result<Vec<Event>, Error> {
        let mut scanner = Scanner::new(bytes, depth)?;
        let mut events = Vec::new();
        let mut offset: usize = 0;
        for _ in 0..input.len() * 4 + 10 {
            let end = offset.saturating_add(width).min(input.len());
            let next = scanner.poll(&input[offset..end], end == input.len())?;
            assert!(next.consumed <= TURN_BYTES && next.consumed <= end - offset);
            offset += next.consumed;
            assert_eq!(scanner.offset(), offset as u64);
            if let Some(event) = next.event {
                events.push(event);
            }
            if next.complete {
                assert_eq!(offset, input.len());
                assert!(scanner.poll(&[], true)?.complete);
                return Ok(events);
            }
        }
        panic!("scanner did not terminate");
    }
    #[test]
    fn all_fragment_widths_preserve_decoded_keys_and_exact_value_spans() {
        let input = br#" {"a\u002fb~\"\\\b\f\n\r\t\uD83D\uDE00":[null,true,false,-12.30E-4,{"":[]},"x\u0041"]} "#;
        let expected = scan(input, input.len(), 4096, 160).unwrap();
        for width in 1..=input.len() {
            assert_eq!(scan(input, width, 4096, 160).unwrap(), expected);
        }
        let mut keys = Vec::new();
        let mut key = String::new();
        let mut stack = Vec::new();
        for event in expected {
            match event {
                Event::KeyStart => key.clear(),
                Event::KeyChar(c) => key.push(c),
                Event::KeyEnd => keys.push(key.clone()),
                Event::Start { kind, offset } => stack.push((kind, offset)),
                Event::End { kind, offset } => {
                    let (start_kind, start) = stack.pop().unwrap();
                    assert_eq!(kind, start_kind);
                    crate::parse(
                        std::str::from_utf8(&input[start as usize..offset as usize]).unwrap(),
                    )
                    .unwrap();
                }
            }
        }
        assert!(stack.is_empty());
        assert_eq!(keys, ["a/b~\"\\\u{8}\u{c}\n\r\t😀", ""]);
    }
    #[test]
    fn scalar_numbers_and_nested_values_match_the_existing_parser() {
        for input in [
            "0",
            "-0",
            "12",
            "-987",
            "0.1",
            "-0.00001",
            "1e0",
            "1E+12",
            "0e-9",
            "1.2e-3",
            "true",
            "false",
            "null",
            r#""hello""#,
            r#""é漢😀""#,
            "{}",
            "[]",
            "[[]]",
            "{\"é漢😀\": {\"x\": 4}}",
            " [ 0 ,1, 2 ] \n",
        ] {
            assert!(crate::parse(input).is_ok(), "{input}");
            assert!(scan(input.as_bytes(), 1, 4096, 160).is_ok(), "{input}");
        }
        let input = "{\"é漢😀\": 0}";
        let decoded: String = scan(input.as_bytes(), 1, 4096, 160)
            .unwrap()
            .into_iter()
            .filter_map(|e| match e {
                Event::KeyChar(c) => Some(c),
                _ => None,
            })
            .collect();
        assert_eq!(decoded, "é漢😀");
    }
    #[test]
    fn malformed_grammar_escapes_and_utf8_fail_at_every_fragment_width() {
        let malformed: &[&[u8]] = &[
            b"",
            b" ",
            b"tru",
            b"trueX",
            b"null false",
            b"00",
            b"-",
            b"+1",
            b"01",
            b"1.",
            b"1e",
            b"1e+",
            b"-.1",
            b"1.e2",
            b"[",
            b"{",
            b"[1,]",
            b"{\"a\":1,}",
            b"{\"a\" 1}",
            b"{a:1}",
            br#"{"ok\x":0}"#,
            b"{\"ok\n\":0}",
            b"{\"ok\x80\":0}",
            br#"{"ok\uD83Dxx":0}"#,
            b"[,1]",
            b"{\"a\":}",
            b"[1 2]",
            b"[}",
            b"{]",
            b"{\"a\":1]\"}",
            b"\"",
            b"\"\\",
            b"\"\\u",
            br#""\x""#,
            br#""\uGGGG""#,
            br#""\uDE00""#,
            br#""\uD83D""#,
            br#""\uD83D\u0041""#,
            br#""\uD83Dxx""#,
            b"\"\n\"",
            b"\"\x80\"",
            b"\"\xc0\x80\"",
            b"\"\xe0\x80\x80\"",
            b"\"\xed\xa0\x80\"",
            b"\"\xf4\x90\x80\x80\"",
            b"\"\xe2X\x80\"",
            b"\"\xf5\x80\x80\x80\"",
        ];
        for input in malformed {
            assert!(
                crate::parse_slice(input).is_err(),
                "reference accepted {input:?}"
            );
            for width in 1..=input.len().max(1) {
                assert_eq!(
                    scan(input, width, 4096, 160),
                    Err(Error::Syntax),
                    "{input:?} width {width}"
                );
            }
        }
    }
    #[test]
    fn eof_is_required_and_failures_are_sticky() {
        let mut scanner = Scanner::new(10, 1).unwrap();
        let start = scanner.poll(b"12", false).unwrap();
        assert_eq!(start.consumed, 1);
        assert!(matches!(
            start.event,
            Some(Event::Start {
                kind: Kind::Number,
                offset: 0
            })
        ));
        assert_eq!(scanner.poll(b"2", false).unwrap(), progress(1, None, false));
        assert_eq!(scanner.poll(&[], false).unwrap(), progress(0, None, false));
        assert_eq!(
            scanner.poll(&[], true).unwrap(),
            progress(
                0,
                Some(Event::End {
                    kind: Kind::Number,
                    offset: 2
                }),
                false
            )
        );
        assert!(scanner.poll(&[], true).unwrap().complete);
        assert_eq!(scanner.poll(b"x", true), Err(Error::State));
        assert_eq!(scanner.poll(&[], true), Err(Error::State));
        let mut scanner = Scanner::new(10, 1).unwrap();
        assert_eq!(scanner.poll(b"x", true), Err(Error::Syntax));
        assert_eq!(scanner.poll(b"0", true), Err(Error::Syntax));
    }
    #[test]
    fn depth_and_byte_limits_are_exact_and_independent() {
        for depth in [1, 128, 160] {
            let input = format!("{}0{}", "[".repeat(depth), "]".repeat(depth));
            assert!(scan(input.as_bytes(), 1, input.len() as u64, depth).is_ok());
            assert_eq!(
                scan(input.as_bytes(), 1, input.len() as u64 - 1, depth),
                Err(Error::Bytes)
            );
            if depth > 1 {
                assert_eq!(
                    scan(input.as_bytes(), 1, 4096, depth - 1),
                    Err(Error::Depth)
                );
            }
        }
        for depth in [1, 128, 160] {
            let input = format!("{}0{}", "{\"k\":".repeat(depth), "}".repeat(depth));
            assert!(scan(input.as_bytes(), 1, input.len() as u64, depth).is_ok());
            if depth > 1 {
                assert_eq!(
                    scan(input.as_bytes(), 1, 4096, depth - 1),
                    Err(Error::Depth)
                );
            }
        }
        assert!(Scanner::new(0, 1).is_err());
        assert!(Scanner::new(1, 0).is_err());
        assert!(Scanner::new(1, 161).is_err());
        for input in ["0", "[]", "{}", "true", "\"\"", " 0 "] {
            assert!(scan(input.as_bytes(), 100, input.len() as u64, 1).is_ok());
        }
    }
    #[test]
    fn short_grammar_exhaustion_matches_the_existing_parser() {
        let alphabet = b"[]{}\",:01-.eE\\ut ";
        for length in 0..=4 {
            for mut number in 0..alphabet.len().pow(length) {
                let mut input = vec![0; length as usize];
                for byte in &mut input {
                    *byte = alphabet[number % alphabet.len()];
                    number /= alphabet.len();
                }
                let expected = crate::parse_slice(&input).is_ok();
                assert_eq!(scan(&input, 1, 64, 4).is_ok(), expected, "{input:?}");
            }
        }
    }
    #[test]
    fn long_scalars_yield_without_retaining_their_contents() {
        let input = format!("[\"{}\",{}]", "z".repeat(20000), "1".repeat(20000));
        let events = scan(input.as_bytes(), usize::MAX, input.len() as u64, 1).unwrap();
        assert_eq!(events.len(), 6);
        assert!(std::mem::size_of::<Scanner>() <= 512);
        fn send<T: Send>() {}
        send::<Scanner>();
        let mut scanner = Scanner::new(30000, 1).unwrap();
        scanner.poll(b"\"", false).unwrap();
        assert_eq!(
            scanner.poll(&vec![b'x'; 20000], false).unwrap(),
            progress(4096, None, false)
        );
        assert!(!format!("{scanner:?}").contains('x'));
    }
    #[test]
    fn whitespace_is_excluded_from_exact_spans_and_eof_events_stay_provisional() {
        let input = b" [ 0 , 12 , [ true ] ] ";
        let events = scan(input, 1, input.len() as u64, 160).unwrap();
        let mut stack = Vec::new();
        let mut spans = Vec::new();
        for event in events {
            match event {
                Event::Start { kind, offset } => stack.push((kind, offset)),
                Event::End { kind, offset } => {
                    let (start_kind, start) = stack.pop().unwrap();
                    assert_eq!(kind, start_kind);
                    let raw = &input[start as usize..offset as usize];
                    assert!(!whitespace(raw[0]) && !whitespace(raw[raw.len() - 1]));
                    spans.push((kind, start, offset));
                }
                _ => (),
            }
        }
        assert_eq!(
            spans,
            [
                (Kind::Number, 3, 4),
                (Kind::Number, 7, 9),
                (Kind::Boolean, 14, 18),
                (Kind::Array, 12, 20),
                (Kind::Array, 1, 22),
            ]
        );
        let mut scanner = Scanner::new(100, 160).unwrap();
        let input = b"[123";
        let mut offset = 0;
        let mut number_ended = false;
        let mut refused = false;
        for _ in 0..16 {
            match scanner.poll(&input[offset..], true) {
                Ok(next) => {
                    offset += next.consumed;
                    assert!(!next.complete);
                    number_ended |= matches!(
                        next.event,
                        Some(Event::End {
                            kind: Kind::Number,
                            ..
                        })
                    );
                }
                Err(error) => {
                    assert_eq!(error, Error::Syntax);
                    refused = true;
                    break;
                }
            }
        }
        assert!(refused);
        assert!(number_ended);
    }
    #[test]
    fn duplicate_keys_are_left_to_the_retained_producer_contract() {
        let input = br#"{"same":1,"same":2}"#;
        assert!(scan(input, 1, 4096, 160).is_ok());
        assert!(crate::parse(std::str::from_utf8(input).unwrap()).is_err());
    }
}

//! Allocation-free JSON parsing into caller-owned bytes and 16-byte tokens.
//! Strings (including keys) decode in place. Object iteration is sorted by
//! decoded key; array order is preserved. A failure invalidates both backings.
use crate::{valid_json_number, MAX_DEPTH};
use std::fmt;

/// Caller ceilings checked before byte inspection, container entry or token use.
/// Every value (including a container) and every object key consumes one token.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ParseLimits {
    pub max_bytes: usize,
    /// Container nesting, capped by the shared MAX_DEPTH even if larger here.
    pub max_depth: usize,
    pub max_tokens: usize,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ParseLimit {
    Bytes,
    Depth,
    Tokens,
}

const NONE: u32 = u32::MAX;
#[derive(Clone, Copy, PartialEq)]
#[repr(u32)]
enum Kind {
    Null,
    False,
    True,
    Number,
    String,
    Array,
    Object,
    Key,
}
/// Opaque caller-reserved storage; no token grants authority over other bytes.
#[derive(Clone, Copy)]
pub struct Token {
    // Scalars use byte spans; containers use first-child/count token metadata.
    start: u32,
    length: u32,
    next: u32,
    kind: Kind,
}
impl Token {
    pub const EMPTY: Self = Self {
        start: 0,
        length: 0,
        next: NONE,
        kind: Kind::Null,
    };
}
const _: () = assert!(std::mem::size_of::<Token>() == 16);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ErrorKind {
    Syntax,
    Limit(ParseLimit),
    Storage,
}
/// Redacted errors allocate nothing. Offsets are raw byte indices.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Error {
    pub kind: ErrorKind,
    pub offset: usize,
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "JSON arena {:?} at byte {}", self.kind, self.offset)
    }
}
impl std::error::Error for Error {}

/// The immutable borrows keep token offsets bound to their decoded backing.
pub struct Document<'a> {
    bytes: &'a [u8],
    tokens: &'a [Token],
}
impl<'a> Document<'a> {
    pub fn root(&self) -> Value<'a> {
        Value {
            bytes: self.bytes,
            tokens: self.tokens,
            index: 0,
        }
    }
    pub fn token_count(&self) -> usize {
        self.tokens.len()
    }
    /// Includes object keys and values, allowing whole-document scalar policies.
    pub fn strings(&self) -> impl Iterator<Item = &str> {
        self.tokens
            .iter()
            .filter(|token| matches!(token.kind, Kind::String | Kind::Key))
            .filter_map(|token| span(self.bytes, *token))
    }
}
#[derive(Clone, Copy)]
pub struct Value<'a> {
    bytes: &'a [u8],
    tokens: &'a [Token],
    index: u32,
}
impl<'a> Value<'a> {
    fn token(self) -> Option<Token> {
        self.tokens.get(self.index as usize).copied()
    }
    pub fn as_str(self) -> Option<&'a str> {
        let token = self.token()?;
        (token.kind == Kind::String)
            .then(|| span(self.bytes, token))
            .flatten()
    }
    pub fn as_number(self) -> Option<&'a str> {
        let token = self.token()?;
        (token.kind == Kind::Number)
            .then(|| span(self.bytes, token))
            .flatten()
    }
    pub fn as_bool(self) -> Option<bool> {
        match self.token()?.kind {
            Kind::True => Some(true),
            Kind::False => Some(false),
            _ => None,
        }
    }
    pub fn is_null(self) -> bool {
        self.token().is_some_and(|token| token.kind == Kind::Null)
    }
    pub fn as_array(self) -> Option<Array<'a>> {
        let token = self.token()?;
        (token.kind == Kind::Array).then_some(Array {
            value: self,
            next: token.start,
            left: token.length,
        })
    }
    pub fn as_object(self) -> Option<Object<'a>> {
        let token = self.token()?;
        (token.kind == Kind::Object).then_some(Object {
            value: self,
            next: token.start,
            left: token.length,
        })
    }
    /// Stream valid JSON through the shared scalar escaper without allocating.
    /// Output errors stop immediately; the caller owns retention and deadlines.
    pub fn write_json(self, out: &mut impl fmt::Write) -> fmt::Result {
        let token = self.token().ok_or(fmt::Error)?;
        match token.kind {
            Kind::Null => out.write_str("null"),
            Kind::False => out.write_str("false"),
            Kind::True => out.write_str("true"),
            Kind::Number => out.write_str(span(self.bytes, token).ok_or(fmt::Error)?),
            Kind::String => crate::write_escaped(span(self.bytes, token).ok_or(fmt::Error)?, out),
            Kind::Array => {
                out.write_char('[')?;
                for (index, value) in self.as_array().ok_or(fmt::Error)?.enumerate() {
                    if index != 0 {
                        out.write_char(',')?;
                    }
                    value.write_json(out)?;
                }
                out.write_char(']')
            }
            Kind::Object => {
                out.write_char('{')?;
                for (index, (key, value)) in self.as_object().ok_or(fmt::Error)?.enumerate() {
                    if index != 0 {
                        out.write_char(',')?;
                    }
                    crate::write_escaped(key, out)?;
                    out.write_char(':')?;
                    value.write_json(out)?;
                }
                out.write_char('}')
            }
            Kind::Key => Err(fmt::Error),
        }
    }
    pub fn get(self, name: &str) -> Option<Self> {
        let object = self.as_object()?;
        let mut index = object.next;
        for _ in 0..object.left {
            let token = self.tokens.get(index as usize).copied()?;
            match raw_span(self.bytes, token)?.cmp(name.as_bytes()) {
                std::cmp::Ordering::Equal => {
                    return Some(Self {
                        index: index.checked_add(1)?,
                        ..self
                    })
                }
                std::cmp::Ordering::Greater => return None,
                std::cmp::Ordering::Less => index = token.next,
            }
        }
        None
    }
}
#[derive(Clone, Copy)]
pub struct Array<'a> {
    value: Value<'a>,
    next: u32,
    left: u32,
}
impl<'a> Iterator for Array<'a> {
    type Item = Value<'a>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.left == 0 {
            return None;
        }
        let value = Value {
            index: self.next,
            ..self.value
        };
        self.next = value.token()?.next;
        self.left -= 1;
        Some(value)
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.left as usize, Some(self.left as usize))
    }
}
impl ExactSizeIterator for Array<'_> {}
#[derive(Clone, Copy)]
pub struct Object<'a> {
    value: Value<'a>,
    next: u32,
    left: u32,
}
impl<'a> Iterator for Object<'a> {
    type Item = (&'a str, Value<'a>);
    fn next(&mut self) -> Option<Self::Item> {
        if self.left == 0 {
            return None;
        }
        let index = self.next;
        let token = self.value.tokens.get(index as usize).copied()?;
        let key = span(self.value.bytes, token)?;
        self.next = token.next;
        self.left -= 1;
        Some((
            key,
            Value {
                index: index.checked_add(1)?,
                ..self.value
            },
        ))
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.left as usize, Some(self.left as usize))
    }
}
impl ExactSizeIterator for Object<'_> {}
fn span(bytes: &[u8], token: Token) -> Option<&str> {
    #[cfg(test)]
    tests::UTF8_BYTES.with(|count| count.set(count.get().saturating_add(u64::from(token.length))));
    std::str::from_utf8(raw_span(bytes, token)?).ok()
}
fn raw_span(bytes: &[u8], token: Token) -> Option<&[u8]> {
    if !matches!(token.kind, Kind::String | Kind::Key | Kind::Number) {
        return None;
    }
    let start = token.start as usize;
    let end = start.checked_add(token.length as usize)?;
    bytes.get(start..end)
}

pub fn parse<'a>(
    bytes: &'a mut [u8],
    tokens: &'a mut [Token],
    limits: ParseLimits,
) -> Result<Document<'a>, Error> {
    if bytes.len() > limits.max_bytes {
        return Err(Error {
            kind: ErrorKind::Limit(ParseLimit::Bytes),
            offset: limits.max_bytes,
        });
    }
    if bytes.len() >= NONE as usize || tokens.len() >= NONE as usize {
        return Err(Error {
            kind: ErrorKind::Storage,
            offset: 0,
        });
    }
    std::str::from_utf8(bytes).map_err(|error| Error {
        kind: ErrorKind::Syntax,
        offset: error.valid_up_to(),
    })?;
    let used = {
        let mut parser = Parser {
            bytes,
            tokens,
            limits,
            position: 0,
            used: 0,
        };
        parser.value(0)?;
        parser.whitespace();
        if parser.position != parser.bytes.len() {
            return Err(parser.error(ErrorKind::Syntax));
        }
        parser.used
    };
    Ok(Document {
        bytes,
        tokens: tokens.get(..used).ok_or(Error {
            kind: ErrorKind::Storage,
            offset: 0,
        })?,
    })
}
struct Parser<'a> {
    bytes: &'a mut [u8],
    tokens: &'a mut [Token],
    limits: ParseLimits,
    position: usize,
    used: usize,
}
impl Parser<'_> {
    fn error(&self, kind: ErrorKind) -> Error {
        Error {
            kind,
            offset: self.position,
        }
    }
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.position).copied()
    }
    fn whitespace(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\r' | b'\n')) {
            self.position += 1;
        }
    }
    fn require(&mut self, byte: u8) -> Result<(), Error> {
        if self.peek() != Some(byte) {
            return Err(self.error(ErrorKind::Syntax));
        }
        self.position += 1;
        Ok(())
    }
    fn allocate(&mut self, kind: Kind) -> Result<u32, Error> {
        if self.used >= self.limits.max_tokens {
            return Err(self.error(ErrorKind::Limit(ParseLimit::Tokens)));
        }
        if self.used >= self.tokens.len() {
            return Err(self.error(ErrorKind::Storage));
        }
        let index = self.used;
        self.used += 1;
        let token = Token {
            kind,
            next: self.used as u32,
            ..Token::EMPTY
        };
        self.set(index as u32, token)?;
        Ok(index as u32)
    }
    fn set(&mut self, index: u32, token: Token) -> Result<(), Error> {
        let error = self.error(ErrorKind::Storage);
        *self.tokens.get_mut(index as usize).ok_or(error)? = token;
        Ok(())
    }
    fn token(&self, index: u32) -> Result<Token, Error> {
        self.tokens
            .get(index as usize)
            .copied()
            .ok_or(self.error(ErrorKind::Storage))
    }
    fn value(&mut self, depth: usize) -> Result<u32, Error> {
        self.whitespace();
        let kind = match self.peek() {
            Some(b'n') => Kind::Null,
            Some(b't') => Kind::True,
            Some(b'f') => Kind::False,
            Some(b'"') => Kind::String,
            Some(b'-' | b'0'..=b'9') => Kind::Number,
            Some(b'[') => Kind::Array,
            Some(b'{') => Kind::Object,
            _ => return Err(self.error(ErrorKind::Syntax)),
        };
        if matches!(kind, Kind::Array | Kind::Object)
            && depth >= self.limits.max_depth.min(MAX_DEPTH)
        {
            return Err(self.error(ErrorKind::Limit(ParseLimit::Depth)));
        }
        let index = self.allocate(kind)?;
        if matches!(kind, Kind::Array | Kind::Object) {
            self.container(index, kind, depth + 1)?;
        } else {
            let (start, length) = match kind {
                Kind::String => self.string()?,
                Kind::Number => {
                    let start = self.position;
                    while self.peek().is_some_and(|byte| {
                        byte.is_ascii_digit() || matches!(byte, b'-' | b'+' | b'.' | b'e' | b'E')
                    }) {
                        self.position += 1;
                    }
                    let lexeme = self
                        .bytes
                        .get(start..self.position)
                        .and_then(|bytes| std::str::from_utf8(bytes).ok())
                        .ok_or(self.error(ErrorKind::Storage))?;
                    if !valid_json_number(lexeme) {
                        return Err(self.error(ErrorKind::Syntax));
                    }
                    (start as u32, (self.position - start) as u32)
                }
                _ => {
                    let literal: &[u8] = match kind {
                        Kind::Null => b"null",
                        Kind::True => b"true",
                        Kind::False => b"false",
                        _ => return Err(self.error(ErrorKind::Storage)),
                    };
                    for &byte in literal {
                        self.require(byte)?;
                    }
                    (0, 0)
                }
            };
            self.set(
                index,
                Token {
                    start,
                    length,
                    next: self.used as u32,
                    kind,
                },
            )?;
        }
        Ok(index)
    }
    fn container(&mut self, index: u32, kind: Kind, depth: usize) -> Result<(), Error> {
        self.position += 1;
        self.whitespace();
        let close = if kind == Kind::Array { b']' } else { b'}' };
        let mut first = NONE;
        let mut last = NONE;
        let mut count = 0u32;
        if self.peek() != Some(close) {
            loop {
                self.whitespace();
                if kind == Kind::Object {
                    if self.peek() != Some(b'"') {
                        return Err(self.error(ErrorKind::Syntax));
                    }
                    let key = self.allocate(Kind::Key)?;
                    let (start, length) = self.string()?;
                    self.set(
                        key,
                        Token {
                            start,
                            length,
                            next: NONE,
                            kind: Kind::Key,
                        },
                    )?;
                    if last != NONE {
                        let mut previous = self.token(last)?;
                        previous.next = key;
                        self.set(last, previous)?;
                    }
                    if first == NONE {
                        first = key;
                    }
                    last = key;
                    self.whitespace();
                    self.require(b':')?;
                    self.value(depth)?;
                } else {
                    let child = self.value(depth)?;
                    if first == NONE {
                        first = child;
                    }
                }
                count = count.checked_add(1).ok_or(self.error(ErrorKind::Storage))?;
                self.whitespace();
                if self.peek() == Some(close) {
                    break;
                }
                self.require(b',')?;
            }
        }
        self.require(close)?;
        if kind == Kind::Object {
            first = self.sort(first, count)?;
        }
        self.set(
            index,
            Token {
                start: first,
                length: count,
                next: self.used as u32,
                kind,
            },
        )
    }
    fn string(&mut self) -> Result<(u32, u32), Error> {
        self.require(b'"')?;
        let start = self.position;
        let mut output = start;
        loop {
            let byte = self.peek().ok_or(self.error(ErrorKind::Syntax))?;
            self.position += 1;
            if byte == b'"' {
                return Ok((start as u32, (output - start) as u32));
            }
            if byte < 32 {
                return Err(self.error(ErrorKind::Syntax));
            }
            if byte == b'\\' {
                let escape = self.peek().ok_or(self.error(ErrorKind::Syntax))?;
                self.position += 1;
                let scalar = match escape {
                    b'"' => '"',
                    b'\\' => '\\',
                    b'/' => '/',
                    b'b' => '\u{8}',
                    b'f' => '\u{c}',
                    b'n' => '\n',
                    b'r' => '\r',
                    b't' => '\t',
                    b'u' => {
                        let mut code = self.hex4()?;
                        if (0xd800..=0xdbff).contains(&code) {
                            self.require(b'\\')?;
                            self.require(b'u')?;
                            let low = self.hex4()?;
                            if !(0xdc00..=0xdfff).contains(&low) {
                                return Err(self.error(ErrorKind::Syntax));
                            }
                            code = 0x10000 + ((code - 0xd800) << 10) + low - 0xdc00;
                        }
                        char::from_u32(code).ok_or(self.error(ErrorKind::Syntax))?
                    }
                    _ => return Err(self.error(ErrorKind::Syntax)),
                };
                let mut encoded = [0; 4];
                for &byte in scalar.encode_utf8(&mut encoded).as_bytes() {
                    self.put(output, byte)?;
                    output += 1;
                }
            } else {
                self.put(output, byte)?;
                output += 1;
            }
        }
    }
    fn put(&mut self, output: usize, byte: u8) -> Result<(), Error> {
        if output >= self.position {
            return Err(self.error(ErrorKind::Storage));
        }
        let error = self.error(ErrorKind::Storage);
        *self.bytes.get_mut(output).ok_or(error)? = byte;
        Ok(())
    }
    fn hex4(&mut self) -> Result<u32, Error> {
        let mut code = 0u32;
        for _ in 0..4 {
            let byte = self.peek().ok_or(self.error(ErrorKind::Syntax))?;
            let digit = char::from(byte)
                .to_digit(16)
                .ok_or(self.error(ErrorKind::Syntax))?;
            self.position += 1;
            code = (code << 4) | digit;
        }
        Ok(code)
    }
    // In-place linked merge sort uses key-token links, not another key arena.
    fn sort(&mut self, head: u32, count: u32) -> Result<u32, Error> {
        if count < 2 {
            return Ok(head);
        }
        let left_count = count / 2;
        let mut cut = head;
        for _ in 1..left_count {
            cut = self.token(cut)?.next;
        }
        let mut token = self.token(cut)?;
        let right = token.next;
        token.next = NONE;
        self.set(cut, token)?;
        let mut left = self.sort(head, left_count)?;
        let mut right = self.sort(right, count - left_count)?;
        let mut result = NONE;
        let mut tail = NONE;
        while left != NONE || right != NONE {
            let choose_left = if right == NONE {
                true
            } else if left == NONE {
                false
            } else {
                let left_key = raw_span(self.bytes, self.token(left)?)
                    .ok_or(self.error(ErrorKind::Storage))?;
                let right_key = raw_span(self.bytes, self.token(right)?)
                    .ok_or(self.error(ErrorKind::Storage))?;
                match left_key.cmp(right_key) {
                    std::cmp::Ordering::Equal => {
                        return Err(Error {
                            kind: ErrorKind::Syntax,
                            offset: (self.token(right)?.start as usize)
                                .checked_sub(1)
                                .ok_or(self.error(ErrorKind::Storage))?,
                        })
                    }
                    std::cmp::Ordering::Less => true,
                    std::cmp::Ordering::Greater => false,
                }
            };
            let item = if choose_left {
                let item = left;
                left = self.token(left)?.next;
                item
            } else {
                let item = right;
                right = self.token(right)?.next;
                item
            };
            if tail == NONE {
                result = item;
            } else {
                let mut token = self.token(tail)?;
                token.next = item;
                self.set(tail, token)?;
            }
            tail = item;
        }
        let mut token = self.token(tail)?;
        token.next = NONE;
        self.set(tail, token)?;
        Ok(result)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::Json;
    std::thread_local! {
        pub(super) static UTF8_BYTES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    }
    fn limits() -> ParseLimits {
        ParseLimits {
            max_bytes: 1 << 20,
            max_depth: 128,
            max_tokens: 32768,
        }
    }
    #[test]
    fn large_last_sorting_key_does_not_revalidate_utf8_per_comparison() {
        let large = "\u{ffef}".repeat(200_000);
        let mut input = format!("{{\"{large}\":null");
        for index in 0..16_000 {
            input.push_str(&format!(",\"k{index:05}\":null"));
        }
        input.push('}');
        let mut bytes = input.into_bytes();
        let mut tokens = vec![Token::EMPTY; 32768];
        UTF8_BYTES.with(|count| count.set(0));
        let document = parse(&mut bytes, &mut tokens, limits()).unwrap();
        assert_eq!(UTF8_BYTES.with(|count| count.get()), 0);
        assert_eq!(
            document.root().as_object().unwrap().last().unwrap().0,
            large
        );
        UTF8_BYTES.with(|count| count.set(0));
        assert!(document.root().get("missing").is_none());
        assert!(document.root().get(&large).is_some());
        assert_eq!(UTF8_BYTES.with(|count| count.get()), 0);
    }
    #[test]
    fn depth_precedes_token_use_and_duplicates_point_to_a_key() {
        let mut bytes = b"[[0]]".to_vec();
        let mut tokens = [Token::EMPTY; 1];
        assert_eq!(
            parse(
                &mut bytes,
                &mut tokens,
                ParseLimits {
                    max_depth: 1,
                    max_tokens: 1,
                    ..limits()
                }
            )
            .err()
            .unwrap()
            .kind,
            ErrorKind::Limit(ParseLimit::Depth)
        );
        let mut bytes = br#"{"a":1,"a":2}"#.to_vec();
        let mut tokens = [Token::EMPTY; 8];
        let error = parse(&mut bytes, &mut tokens, limits()).err().unwrap();
        assert_eq!(error.kind, ErrorKind::Syntax);
        assert_eq!(error.offset, 7);
    }
    fn own(value: Value<'_>) -> Json {
        if let Some(text) = value.as_str() {
            Json::Str(text.into())
        } else if let Some(number) = value.as_number() {
            Json::Num(number.into())
        } else if let Some(boolean) = value.as_bool() {
            Json::Bool(boolean)
        } else if let Some(values) = value.as_array() {
            Json::Arr(values.map(own).collect())
        } else if let Some(values) = value.as_object() {
            Json::Obj(
                values
                    .map(|(key, value)| (key.into(), own(value)))
                    .collect(),
            )
        } else {
            assert!(value.is_null());
            Json::Null
        }
    }
    fn compare(input: &str) {
        let expected = crate::parse(input);
        let mut bytes = input.as_bytes().to_vec();
        let mut tokens = vec![Token::EMPTY; 32768];
        let actual = parse(&mut bytes, &mut tokens, limits());
        match (expected, actual) {
            (Ok(expected), Ok(actual)) => {
                let mut output = String::new();
                actual.root().write_json(&mut output).unwrap();
                assert_eq!(expected.to_canonical(), output, "{input}");
                assert_eq!(
                    expected.to_canonical(),
                    own(actual.root()).to_canonical(),
                    "{input}"
                );
            }
            (Err(_), Err(actual)) => assert_eq!(actual.kind, ErrorKind::Syntax, "{input}"),
            _ => panic!("parser acceptance disagreement for {input:?}"),
        }
    }
    #[test]
    fn agrees_with_owned_parser_on_strings_numbers_and_container_grammar() {
        for input in [
            "null",
            "true",
            "false",
            "0",
            "-0",
            "1.25e-90",
            "1e400",
            "9007199254740993",
            "[]",
            "{}",
            r#"{"z":[true,null,{"b":2,"a":1}],"a":"é☃"}"#,
            r#""\"\\\/\b\f\n\r\t\u0000\ud834\udd1e""#,
            r#"{"\u0061":1,"a":2}"#,
            r#"["\ud800"]"#,
            r#"["\udc00"]"#,
            r#""\uD800\u0000""#,
            r#""\uZZZZ""#,
            r#""\u123""#,
            r#""\x""#,
            "",
            " ",
            "[",
            "{",
            "[1,]",
            "{\"x\":1,}",
            "[1 2]",
            "{1:2}",
            "{} []",
            "01",
            "-",
            "+1",
            "1.",
            ".1",
            "1e",
            "1e+",
            "truefalse",
            "\"a\nb\"",
            "\"unterminated",
        ] {
            compare(input);
        }
    }
    #[test]
    fn bounded_random_documents_match_shared_owned_parser() {
        let alphabet = b"{}[]:,\"\\0123456789.-+eEtruefalsn \r\n\tuxabcdef";
        let mut seed = 91u64;
        for length in 0..80 {
            for _ in 0..40 {
                let mut text = String::new();
                for _ in 0..length {
                    seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                    text.push(char::from(alphabet[(seed >> 32) as usize % alphabet.len()]));
                }
                compare(&text);
            }
        }
    }
    #[test]
    fn wide_keys_sort_without_losing_value_subtrees_or_duplicate_checks() {
        let fields = (0..4096)
            .rev()
            .map(|i| format!("\"common-{i:04}\":[{i},{{\"nested\":{i}}}]"))
            .collect::<Vec<_>>()
            .join(",");
        compare(&format!("{{{fields}}}"));
        compare(&format!("{{{fields},\"common-2000\":null}}"));
        compare(r#"{"\u0062":[0],"a":[1],"\u0061":[2]}"#);
    }
    #[test]
    fn exact_limits_and_caller_storage_refuse_without_partial_document() {
        let input = br#"{"a":[0]}"#;
        let exact = ParseLimits {
            max_bytes: input.len(),
            max_depth: 2,
            max_tokens: 4,
        };
        let mut bytes = input.to_vec();
        let mut tokens = [Token::EMPTY; 4];
        let document = parse(&mut bytes, &mut tokens, exact).unwrap();
        assert_eq!(document.token_count(), 4);
        assert_eq!(document.strings().collect::<Vec<_>>(), ["a"]);
        for (limits, kind) in [
            (
                ParseLimits {
                    max_bytes: input.len() - 1,
                    ..exact
                },
                ParseLimit::Bytes,
            ),
            (
                ParseLimits {
                    max_depth: 1,
                    ..exact
                },
                ParseLimit::Depth,
            ),
            (
                ParseLimits {
                    max_tokens: 3,
                    ..exact
                },
                ParseLimit::Tokens,
            ),
        ] {
            assert_eq!(
                parse(&mut input.to_vec(), &mut tokens, limits)
                    .err()
                    .unwrap()
                    .kind,
                ErrorKind::Limit(kind)
            );
        }
        assert_eq!(
            parse(&mut input.to_vec(), &mut tokens[..3], exact)
                .err()
                .unwrap()
                .kind,
            ErrorKind::Storage
        );
        assert!(parse(
            &mut b"0".to_vec(),
            &mut tokens,
            ParseLimits {
                max_depth: 0,
                ..exact
            }
        )
        .is_ok());
        assert_eq!(
            parse(
                &mut b"0".to_vec(),
                &mut tokens,
                ParseLimits {
                    max_tokens: 0,
                    ..exact
                }
            )
            .err()
            .unwrap()
            .kind,
            ErrorKind::Limit(ParseLimit::Tokens)
        );
        let mut malformed = vec![0xff; 3];
        assert_eq!(
            parse(
                &mut malformed,
                &mut tokens,
                ParseLimits {
                    max_bytes: 2,
                    ..exact
                }
            )
            .err()
            .unwrap()
            .kind,
            ErrorKind::Limit(ParseLimit::Bytes)
        );
        assert_eq!(
            parse(&mut malformed, &mut tokens, exact)
                .err()
                .unwrap()
                .kind,
            ErrorKind::Syntax
        );
    }
    #[test]
    fn in_place_unicode_decoding_never_overwrites_later_input() {
        for scalar in ['\0', '\n', 'é', '☃', '\u{1d11e}', '\u{10ffff}'] {
            let escaped = if (scalar as u32) < 0x10000 {
                format!("\\u{:04x}", scalar as u32)
            } else {
                let code = scalar as u32 - 0x10000;
                format!(
                    "\\u{:04x}\\u{:04x}",
                    0xd800 + (code >> 10),
                    0xdc00 + (code & 1023)
                )
            };
            compare(&format!(
                r#"{{"a{escaped}é":"{escaped}☃","tail":["unchanged",{{"x":3}}]}}"#
            ));
        }
        assert_eq!(std::mem::size_of::<Token>(), 16);
    }
    #[test]
    fn streaming_output_stops_at_the_first_sink_failure() {
        struct Sink {
            left: usize,
            refused: bool,
        }
        impl fmt::Write for Sink {
            fn write_str(&mut self, text: &str) -> fmt::Result {
                assert!(!self.refused);
                if text.len() > self.left {
                    self.refused = true;
                    return Err(fmt::Error);
                }
                self.left -= text.len();
                Ok(())
            }
        }
        let mut bytes = r#"{"z":[true,false,null,1e30],"a":"\n\u0000é"}"#.as_bytes().to_vec();
        let mut tokens = [Token::EMPTY; 32];
        let document = parse(&mut bytes, &mut tokens, limits()).unwrap();
        let mut expected = String::new();
        document.root().write_json(&mut expected).unwrap();
        for capacity in 0..expected.len() {
            let mut sink = Sink {
                left: capacity,
                refused: false,
            };
            assert!(document.root().write_json(&mut sink).is_err());
            assert!(sink.refused);
        }
        assert!(document
            .root()
            .write_json(&mut Sink {
                left: expected.len(),
                refused: false
            })
            .is_ok());
    }
    #[test]
    fn nested_generated_values_and_truncated_prefixes_match() {
        let mut text = r#"{"é":"\u00e9","zero":0,"bool":false}"#.to_string();
        for level in 0..16 {
            compare(&text);
            for (end, _) in text.char_indices().take(100) {
                compare(&text[..end]);
            }
            text = if level % 2 == 0 {
                format!("[{text},null,true,\"tail\"]")
            } else {
                format!("{{\"z\":{text},\"a\":[]}}")
            };
        }
    }
    #[test]
    fn global_recursion_ceiling_cannot_be_raised() {
        let mut tokens = vec![Token::EMPTY; 256];
        let mut bytes = format!("{}0{}", "[".repeat(128), "]".repeat(128)).into_bytes();
        assert!(parse(&mut bytes, &mut tokens, limits()).is_ok());
        let mut bytes = format!("{}0{}", "[".repeat(129), "]".repeat(129)).into_bytes();
        assert_eq!(
            parse(
                &mut bytes,
                &mut tokens,
                ParseLimits {
                    max_depth: usize::MAX,
                    ..limits()
                }
            )
            .err()
            .unwrap()
            .kind,
            ErrorKind::Limit(ParseLimit::Depth)
        );
    }
}

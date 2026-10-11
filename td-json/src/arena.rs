//! Allocation-free JSON parsing into caller-owned bytes and 16-byte tokens.
//! Strings (including keys) decode in place. Object iteration is sorted by
//! decoded key; array order is preserved. A failure invalidates both backings.
use crate::MAX_DEPTH;
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
    /// One indexed token's decoded UTF-8 bytes, if it is a string or key.
    /// This does not rescan the string; callers can inspect bounded chunks.
    pub fn string_bytes_at(&self, index: usize) -> Option<&'a [u8]> {
        let token = *self.tokens.get(index)?;
        matches!(token.kind, Kind::String | Kind::Key)
            .then(|| raw_span(self.bytes, token))
            .flatten()
    }
    /// Constant-time, document-relative lookup. Indices are positions, not
    /// capabilities: callers must keep them with the document that issued them.
    pub fn value_at(&self, index: usize) -> Option<Value<'a>> {
        let token = self.tokens.get(index)?;
        (token.kind != Kind::Key).then_some(Value {
            bytes: self.bytes,
            tokens: self.tokens,
            index: u32::try_from(index).ok()?,
        })
    }
    /// One traversal continuation. Array traversal MUST also retain its element
    /// count: the final element may point beyond its container. Object key
    /// links are sorted and terminated. Neither form can leave this document.
    pub fn next_index(&self, index: usize) -> Option<usize> {
        let next = self.tokens.get(index)?.next;
        self.tokens.get(next as usize).map(|_| next as usize)
    }
    /// One object member at its key token, without rescanning the key bytes.
    pub fn member_at(&self, index: usize) -> Option<(&'a [u8], Value<'a>)> {
        let token = *self.tokens.get(index)?;
        if token.kind != Kind::Key {
            return None;
        }
        Some((
            raw_span(self.bytes, token)?,
            self.value_at(index.checked_add(1)?)?,
        ))
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
    /// Position in this document's token backing; never a cross-document ID.
    pub fn token_index(self) -> usize {
        self.index as usize
    }
    /// First child token of an array or first key token of an object.
    pub fn first_child_index(self) -> Option<usize> {
        let token = self.token()?;
        (matches!(token.kind, Kind::Array | Kind::Object) && token.length != 0)
            .then_some(token.start as usize)
    }
    fn token(self) -> Option<Token> {
        self.tokens.get(self.index as usize).copied()
    }
    /// Decoded, validated UTF-8 bytes without a whole-string revalidation.
    pub fn as_string_bytes(self) -> Option<&'a [u8]> {
        let token = self.token()?;
        (token.kind == Kind::String)
            .then(|| raw_span(self.bytes, token))
            .flatten()
    }
    pub fn as_str(self) -> Option<&'a str> {
        let token = self.token()?;
        (token.kind == Kind::String)
            .then(|| span(self.bytes, token))
            .flatten()
    }
    /// Validated JSON number bytes without whole-number UTF-8 rescanning.
    pub fn as_number_bytes(self) -> Option<&'a [u8]> {
        let token = self.token()?;
        (token.kind == Kind::Number)
            .then(|| raw_span(self.bytes, token))
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
impl<'a> Object<'a> {
    /// Advance one member without rescanning its decoded UTF-8 key.
    pub fn next_bytes(&mut self) -> Option<(&'a [u8], Value<'a>)> {
        if self.left == 0 {
            return None;
        }
        let index = self.next;
        let token = self.value.tokens.get(index as usize).copied()?;
        let key = raw_span(self.value.bytes, token)?;
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
}
impl<'a> Iterator for Object<'a> {
    type Item = (&'a str, Value<'a>);
    fn next(&mut self) -> Option<Self::Item> {
        let (key, value) = self.next_bytes()?;
        Some((checked_utf8(key)?, value))
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.left as usize, Some(self.left as usize))
    }
}
impl ExactSizeIterator for Object<'_> {}
fn span(bytes: &[u8], token: Token) -> Option<&str> {
    checked_utf8(raw_span(bytes, token)?)
}
fn checked_utf8(bytes: &[u8]) -> Option<&str> {
    #[cfg(test)]
    tests::UTF8_BYTES.with(|count| count.set(count.get().saturating_add(bytes.len() as u64)));
    std::str::from_utf8(bytes).ok()
}
fn raw_span(bytes: &[u8], token: Token) -> Option<&[u8]> {
    if !matches!(token.kind, Kind::String | Kind::Key | Kind::Number) {
        return None;
    }
    let start = token.start as usize;
    let end = start.checked_add(token.length as usize)?;
    bytes.get(start..end)
}

/// Parse to completion. Scheduling-sensitive callers use Cursor::step instead.
pub fn parse<'a>(
    bytes: &'a mut [u8],
    tokens: &'a mut [Token],
    limits: ParseLimits,
) -> Result<Document<'a>, Error> {
    let mut cursor = Cursor::new(bytes, tokens, limits)?;
    while !cursor.step()? {}
    cursor.finish()
}

#[derive(Clone, Copy)]
enum Place {
    First,
    Value,
    Key,
    Colon,
    After,
}
#[derive(Clone, Copy)]
struct Frame {
    index: u32,
    first: u32,
    last: u32,
    count: u32,
    kind: Kind,
    place: Place,
}
impl Frame {
    const EMPTY: Self = Self {
        index: NONE,
        first: NONE,
        last: NONE,
        count: 0,
        kind: Kind::Null,
        place: Place::First,
    };
}
#[derive(Clone, Copy)]
enum NumberState {
    Start,
    Sign,
    Zero,
    Integer,
    Dot,
    Fraction,
    Exponent,
    ExponentSign,
    ExponentDigits,
}
#[derive(Clone, Copy)]
enum Mode {
    Utf8,
    Grammar,
    String {
        index: u32,
        start: usize,
        output: usize,
    },
    Number {
        index: u32,
        start: usize,
        state: NumberState,
    },
    Literal {
        index: u32,
        matched: usize,
    },
    Sort(Sort),
    Done,
}
#[derive(Clone, Copy)]
enum SortPhase {
    Split,
    Merge,
}
#[derive(Clone, Copy)]
struct Sort {
    object: u32,
    width: u32,
    rest: u32,
    head: u32,
    tail: u32,
    left: u32,
    right: u32,
    left_count: u32,
    right_count: u32,
    split: u32,
    compared: usize,
    phase: SortPhase,
}

mod sealed {
    pub trait Pair<T> {}
    impl Pair<&mut [super::Token]> for &mut [u8] {}
    impl Pair<Box<[super::Token]>> for Box<[u8]> {}
    pub trait Storage {}
    impl<T> Storage for &mut [T] {}
    impl<T> Storage for Box<[T]> {}
}
/// Stable caller-provided backing. Sealed to slices and boxed slices so a
/// storage implementation cannot substitute a different arena between turns.
pub trait Storage<T>: sealed::Storage + std::ops::Deref<Target = [T]> + std::ops::DerefMut {}
impl<T> Storage<T> for &mut [T] {}
impl<T> Storage<T> for Box<[T]> {}

/// Supported complete parser backing pairs. Mixed owned/borrowed pairs have
/// no completion API and cannot be constructed.
pub trait StoragePair<T>: sealed::Pair<T> {}
impl StoragePair<&mut [Token]> for &mut [u8] {}
impl StoragePair<Box<[Token]>> for Box<[u8]> {}

/// Borrowed parser; existing callers retain ownership of both buffers.
pub type Cursor<'a> = Parser<&'a mut [u8], &'a mut [Token]>;
/// Moveable parser owning preallocated boxes, suitable for a queued worker job.
pub type OwnedCursor = Parser<Box<[u8]>, Box<[Token]>>;
/// Owned backings returned after a constructor or completion refusal.
pub type OwnedRefusal = Refusal<Box<[u8]>, Box<[Token]>>;

/// A refusal returns the same backings without publishing a partial document.
/// Debug output is redacted; buffers may contain credentials or mail.
pub struct Refusal<B, T> {
    error: Error,
    bytes: B,
    tokens: T,
}
impl<B, T> Refusal<B, T> {
    pub const fn error(&self) -> Error {
        self.error
    }
    pub fn into_buffers(self) -> (B, T) {
        (self.bytes, self.tokens)
    }
}
impl<B, T> fmt::Debug for Refusal<B, T> {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.debug_tuple("Refusal").field(&self.error).finish()
    }
}
impl<B, T> fmt::Display for Refusal<B, T> {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.error.fmt(out)
    }
}
impl<B, T> std::error::Error for Refusal<B, T> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}

/// Valid JSON with exclusively owned storage. Views borrow this owner; the
/// boxes can be recovered for another request only after those borrows end.
pub struct OwnedDocument {
    limits: ParseLimits,
    bytes: Box<[u8]>,
    tokens: Box<[Token]>,
    input_len: usize,
    used: usize,
}
impl OwnedDocument {
    /// Exact ceilings that admitted this document; retained for protocol handoff.
    pub const fn parse_limits(&self) -> ParseLimits {
        self.limits
    }
    pub fn token_capacity(&self) -> usize {
        self.tokens.len()
    }
    pub fn view(&self) -> Result<Document<'_>, Error> {
        let error = Error {
            kind: ErrorKind::Storage,
            offset: 0,
        };
        Ok(Document {
            bytes: self.bytes.get(..self.input_len).ok_or(error)?,
            tokens: self.tokens.get(..self.used).ok_or(error)?,
        })
    }
    pub fn into_buffers(self) -> (Box<[u8]>, Box<[Token]>) {
        (self.bytes, self.tokens)
    }
}

/// Fixed-state in-place parser. Each step performs at most 256 transitions;
/// a transition inspects at most 12 input bytes (one Unicode escape pair),
/// or compares one key byte. No whole-string or whole-object work is hidden
/// at completion. Callers retain responsibility for cancellation and deadlines.
/// Dropping or failing the cursor invalidates the partially modified backing.
pub struct Parser<B: Storage<u8>, T: Storage<Token>> {
    bytes: B,
    tokens: T,
    input_len: usize,
    limits: ParseLimits,
    position: usize,
    used: usize,
    stack: [Frame; MAX_DEPTH],
    depth: usize,
    root_started: bool,
    mode: Mode,
    failed: Option<Error>,
}
const _: () = assert!(std::mem::size_of::<Cursor<'_>>() <= 4096);
const _: () = assert!(std::mem::size_of::<OwnedCursor>() <= 4096);
impl<B: Storage<u8>, T: Storage<Token>> Parser<B, T> {
    /// Parse only the occupied prefix of a preallocated byte arena. Constructor
    /// refusal returns the same boxes/slices; no allocation or wiping occurs.
    pub fn with_input_length(
        bytes: B,
        tokens: T,
        input_len: usize,
        limits: ParseLimits,
    ) -> Result<Self, Refusal<B, T>>
    where
        B: StoragePair<T>,
    {
        let error = if input_len > bytes.len()
            || bytes.len() >= NONE as usize
            || tokens.len() >= NONE as usize
        {
            Some(Error {
                kind: ErrorKind::Storage,
                offset: 0,
            })
        } else if input_len > limits.max_bytes {
            Some(Error {
                kind: ErrorKind::Limit(ParseLimit::Bytes),
                offset: limits.max_bytes,
            })
        } else {
            None
        };
        if let Some(error) = error {
            return Err(Refusal {
                error,
                bytes,
                tokens,
            });
        }
        Ok(Self {
            bytes,
            tokens,
            input_len,
            limits,
            position: 0,
            used: 0,
            stack: [Frame::EMPTY; MAX_DEPTH],
            depth: 0,
            root_started: false,
            mode: Mode::Utf8,
            failed: None,
        })
    }
    /// Abandon parsing and recover both complete arenas, even after failure.
    /// Their contents may be partially decoded and have no validity guarantee.
    pub fn into_buffers(self) -> (B, T) {
        (self.bytes, self.tokens)
    }
    fn completion(&self) -> Result<(), Error> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        if !matches!(self.mode, Mode::Done) {
            return Err(self.error(ErrorKind::Storage));
        }
        Ok(())
    }
    /// True means the document can be consumed with finish. Errors are sticky.
    pub fn step(&mut self) -> Result<bool, Error> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        for _ in 0..256 {
            if matches!(self.mode, Mode::Done) {
                return Ok(true);
            }
            if let Err(error) = self.transition() {
                self.failed = Some(error);
                return Err(error);
            }
        }
        Ok(matches!(self.mode, Mode::Done))
    }
    fn error(&self, kind: ErrorKind) -> Error {
        Error {
            kind,
            offset: self.position,
        }
    }
    fn peek(&self) -> Option<u8> {
        if self.position < self.input_len {
            self.bytes.get(self.position).copied()
        } else {
            None
        }
    }
    fn require(&mut self, byte: u8) -> Result<(), Error> {
        if self.peek() != Some(byte) {
            return Err(self.error(ErrorKind::Syntax));
        }
        self.position += 1;
        Ok(())
    }
    fn token(&self, index: u32) -> Result<Token, Error> {
        self.tokens
            .get(index as usize)
            .copied()
            .ok_or(self.error(ErrorKind::Storage))
    }
    fn set(&mut self, index: u32, token: Token) -> Result<(), Error> {
        let error = self.error(ErrorKind::Storage);
        *self.tokens.get_mut(index as usize).ok_or(error)? = token;
        Ok(())
    }
    fn allocate(&mut self, kind: Kind) -> Result<u32, Error> {
        if self.used >= self.limits.max_tokens {
            return Err(self.error(ErrorKind::Limit(ParseLimit::Tokens)));
        }
        if self.used >= self.tokens.len() {
            return Err(self.error(ErrorKind::Storage));
        }
        let index = self.used as u32;
        self.used += 1;
        self.set(
            index,
            Token {
                kind,
                next: self.used as u32,
                ..Token::EMPTY
            },
        )?;
        Ok(index)
    }
    fn frame(&self) -> Result<Frame, Error> {
        self.depth
            .checked_sub(1)
            .and_then(|i| self.stack.get(i))
            .copied()
            .ok_or(self.error(ErrorKind::Storage))
    }
    fn set_frame(&mut self, frame: Frame) -> Result<(), Error> {
        let error = self.error(ErrorKind::Storage);
        let index = self.depth.checked_sub(1).ok_or(error)?;
        *self.stack.get_mut(index).ok_or(error)? = frame;
        Ok(())
    }
    fn transition(&mut self) -> Result<(), Error> {
        match self.mode {
            Mode::Utf8 => {
                let Some(byte) = self.peek() else {
                    self.position = 0;
                    self.mode = Mode::Grammar;
                    return Ok(());
                };
                let width = match byte {
                    0..=0x7f => 1,
                    0xc2..=0xdf => 2,
                    0xe0..=0xef => 3,
                    0xf0..=0xf4 => 4,
                    _ => return Err(self.error(ErrorKind::Syntax)),
                };
                let bytes = self
                    .bytes
                    .get(..self.input_len)
                    .and_then(|bytes| bytes.get(self.position..self.position + width))
                    .ok_or(self.error(ErrorKind::Syntax))?;
                std::str::from_utf8(bytes).map_err(|_| self.error(ErrorKind::Syntax))?;
                self.position += width;
                Ok(())
            }
            Mode::Grammar => self.grammar(),
            Mode::String {
                index,
                start,
                output,
            } => self.string_byte(index, start, output),
            Mode::Number {
                index,
                start,
                state,
            } => self.number_byte(index, start, state),
            Mode::Literal { index, matched } => {
                let literal: &[u8] = match self.token(index)?.kind {
                    Kind::Null => b"null",
                    Kind::True => b"true",
                    Kind::False => b"false",
                    _ => return Err(self.error(ErrorKind::Storage)),
                };
                if let Some(&byte) = literal.get(matched) {
                    self.require(byte)?;
                    self.mode = Mode::Literal {
                        index,
                        matched: matched + 1,
                    };
                } else {
                    self.mode = Mode::Grammar;
                }
                Ok(())
            }
            Mode::Sort(sort) => self.sort_step(sort),
            Mode::Done => Ok(()),
        }
    }
    fn grammar(&mut self) -> Result<(), Error> {
        if matches!(self.peek(), Some(b' ' | b'\t' | b'\r' | b'\n')) {
            self.position += 1;
            return Ok(());
        }
        if self.depth == 0 {
            if self.root_started {
                if self.peek().is_some() {
                    return Err(self.error(ErrorKind::Syntax));
                }
                self.mode = Mode::Done;
                return Ok(());
            }
            self.root_started = true;
            return self.value();
        }
        let mut frame = self.frame()?;
        let close = if frame.kind == Kind::Array {
            b']'
        } else {
            b'}'
        };
        if matches!(frame.place, Place::First | Place::After) && self.peek() == Some(close) {
            self.position += 1;
            self.depth -= 1;
            self.set(
                frame.index,
                Token {
                    start: frame.first,
                    length: frame.count,
                    next: self.used as u32,
                    kind: frame.kind,
                },
            )?;
            if frame.kind == Kind::Object && frame.count > 1 {
                self.mode = Mode::Sort(Sort {
                    object: frame.index,
                    width: 1,
                    rest: frame.first,
                    head: NONE,
                    tail: NONE,
                    left: NONE,
                    right: NONE,
                    left_count: 0,
                    right_count: 0,
                    split: 0,
                    compared: 0,
                    phase: SortPhase::Split,
                });
            }
            return Ok(());
        }
        match frame.place {
            Place::First => {
                frame.place = if frame.kind == Kind::Object {
                    Place::Key
                } else {
                    Place::Value
                };
                self.set_frame(frame)
            }
            Place::After => {
                self.require(b',')?;
                frame.place = if frame.kind == Kind::Object {
                    Place::Key
                } else {
                    Place::Value
                };
                self.set_frame(frame)
            }
            Place::Colon => {
                self.require(b':')?;
                frame.place = Place::Value;
                self.set_frame(frame)
            }
            Place::Key => {
                if self.peek() != Some(b'"') {
                    return Err(self.error(ErrorKind::Syntax));
                }
                let key = self.allocate(Kind::Key)?;
                if frame.last != NONE {
                    let mut token = self.token(frame.last)?;
                    token.next = key;
                    self.set(frame.last, token)?;
                }
                if frame.first == NONE {
                    frame.first = key;
                }
                frame.last = key;
                frame.place = Place::Colon;
                self.set_frame(frame)?;
                self.position += 1;
                self.mode = Mode::String {
                    index: key,
                    start: self.position,
                    output: self.position,
                };
                Ok(())
            }
            Place::Value => {
                if frame.kind == Kind::Array && frame.first == NONE {
                    frame.first = self.used as u32;
                }
                frame.count = frame
                    .count
                    .checked_add(1)
                    .ok_or(self.error(ErrorKind::Storage))?;
                frame.place = Place::After;
                self.set_frame(frame)?;
                self.value()
            }
        }
    }
    fn value(&mut self) -> Result<(), Error> {
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
            && self.depth >= self.limits.max_depth.min(MAX_DEPTH)
        {
            return Err(self.error(ErrorKind::Limit(ParseLimit::Depth)));
        }
        let index = self.allocate(kind)?;
        match kind {
            Kind::Array | Kind::Object => {
                let error = self.error(ErrorKind::Storage);
                *self.stack.get_mut(self.depth).ok_or(error)? = Frame {
                    index,
                    kind,
                    ..Frame::EMPTY
                };
                self.depth += 1;
                self.position += 1;
            }
            Kind::String => {
                self.position += 1;
                self.mode = Mode::String {
                    index,
                    start: self.position,
                    output: self.position,
                };
            }
            Kind::Number => {
                self.mode = Mode::Number {
                    index,
                    start: self.position,
                    state: NumberState::Start,
                }
            }
            _ => self.mode = Mode::Literal { index, matched: 0 },
        }
        Ok(())
    }
    fn number_byte(&mut self, index: u32, start: usize, state: NumberState) -> Result<(), Error> {
        use NumberState::*;
        let next = match (state, self.peek()) {
            (Start, Some(b'-')) => Some(Sign),
            (Start | Sign, Some(b'0')) => Some(Zero),
            (Start | Sign, Some(b'1'..=b'9')) => Some(Integer),
            (Integer, Some(b'0'..=b'9')) => Some(Integer),
            (Zero | Integer, Some(b'.')) => Some(Dot),
            (Dot | Fraction, Some(b'0'..=b'9')) => Some(Fraction),
            (Zero | Integer | Fraction, Some(b'e' | b'E')) => Some(Exponent),
            (Exponent, Some(b'+' | b'-')) => Some(ExponentSign),
            (Exponent | ExponentSign | ExponentDigits, Some(b'0'..=b'9')) => Some(ExponentDigits),
            _ => None,
        };
        if let Some(state) = next {
            self.position += 1;
            self.mode = Mode::Number {
                index,
                start,
                state,
            };
            return Ok(());
        }
        if !matches!(state, Zero | Integer | Fraction | ExponentDigits) {
            return Err(self.error(ErrorKind::Syntax));
        }
        self.set(
            index,
            Token {
                start: start as u32,
                length: (self.position - start) as u32,
                next: self.used as u32,
                kind: Kind::Number,
            },
        )?;
        self.mode = Mode::Grammar;
        Ok(())
    }
    fn string_byte(&mut self, index: u32, start: usize, mut output: usize) -> Result<(), Error> {
        let byte = self.peek().ok_or(self.error(ErrorKind::Syntax))?;
        self.position += 1;
        if byte == b'"' {
            let kind = self.token(index)?.kind;
            self.set(
                index,
                Token {
                    start: start as u32,
                    length: (output - start) as u32,
                    next: if kind == Kind::Key {
                        NONE
                    } else {
                        self.used as u32
                    },
                    kind,
                },
            )?;
            self.mode = Mode::Grammar;
            return Ok(());
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
            if output != self.position - 1 {
                self.put(output, byte)?;
            }
            output += 1;
        }
        self.mode = Mode::String {
            index,
            start,
            output,
        };
        Ok(())
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
            let digit = self
                .peek()
                .and_then(|byte| char::from(byte).to_digit(16))
                .ok_or(self.error(ErrorKind::Syntax))?;
            self.position += 1;
            code = (code << 4) | digit;
        }
        Ok(code)
    }
    fn sort_step(&mut self, mut sort: Sort) -> Result<(), Error> {
        match sort.phase {
            SortPhase::Split => {
                if sort.left == NONE {
                    if sort.rest == NONE {
                        let mut token = self.token(sort.object)?;
                        token.start = sort.head;
                        self.set(sort.object, token)?;
                        sort.width = sort.width.saturating_mul(2);
                        if sort.width >= token.length {
                            self.mode = Mode::Grammar;
                            return Ok(());
                        }
                        sort.rest = sort.head;
                        sort.head = NONE;
                        sort.tail = NONE;
                    }
                    sort.left = sort.rest;
                    sort.right = sort.rest;
                    sort.split = 0;
                }
                if sort.split < sort.width && sort.right != NONE {
                    sort.right = self.token(sort.right)?.next;
                    sort.split += 1;
                } else {
                    sort.left_count = sort.split;
                    sort.right_count = sort.width;
                    sort.phase = SortPhase::Merge;
                }
            }
            SortPhase::Merge => {
                if sort.left_count == 0 && (sort.right_count == 0 || sort.right == NONE) {
                    sort.rest = sort.right;
                    sort.left = NONE;
                    sort.phase = SortPhase::Split;
                } else {
                    let choose_left = if sort.left_count == 0 {
                        false
                    } else if sort.right_count == 0 || sort.right == NONE {
                        true
                    } else {
                        let left = raw_span(&self.bytes, self.token(sort.left)?)
                            .ok_or(self.error(ErrorKind::Storage))?;
                        let right = raw_span(&self.bytes, self.token(sort.right)?)
                            .ok_or(self.error(ErrorKind::Storage))?;
                        match (left.get(sort.compared), right.get(sort.compared)) {
                            (Some(a), Some(b)) if a == b => {
                                sort.compared += 1;
                                self.mode = Mode::Sort(sort);
                                return Ok(());
                            }
                            (None, None) => {
                                return Err(Error {
                                    kind: ErrorKind::Syntax,
                                    offset: self
                                        .token(sort.left)?
                                        .start
                                        .max(self.token(sort.right)?.start)
                                        .saturating_sub(1)
                                        as usize,
                                })
                            }
                            (None, Some(_)) => true,
                            (Some(_), None) => false,
                            (Some(a), Some(b)) => a < b,
                        }
                    };
                    sort.compared = 0;
                    let item = if choose_left {
                        let item = sort.left;
                        sort.left = self.token(item)?.next;
                        sort.left_count -= 1;
                        item
                    } else {
                        let item = sort.right;
                        sort.right = self.token(item)?.next;
                        sort.right_count -= 1;
                        item
                    };
                    if sort.tail == NONE {
                        sort.head = item;
                    } else {
                        let mut token = self.token(sort.tail)?;
                        token.next = item;
                        self.set(sort.tail, token)?;
                    }
                    sort.tail = item;
                    // Detach output; both input run cursors already retain their successors.
                    let mut token = self.token(item)?;
                    token.next = NONE;
                    self.set(item, token)?;
                }
            }
        }
        self.mode = Mode::Sort(sort);
        Ok(())
    }
}

impl<'a> Cursor<'a> {
    /// Convenience constructor using all borrowed input bytes.
    pub fn new(
        bytes: &'a mut [u8],
        tokens: &'a mut [Token],
        limits: ParseLimits,
    ) -> Result<Self, Error> {
        let input_len = bytes.len();
        Self::with_input_length(bytes, tokens, input_len, limits).map_err(|refusal| refusal.error)
    }
    /// Premature consumption reports Storage (caller misuse); no document is
    /// available before all parsing and sorting completes.
    pub fn finish(self) -> Result<Document<'a>, Error> {
        self.completion()?;
        let error = self.error(ErrorKind::Storage);
        Ok(Document {
            bytes: self.bytes.get(..self.input_len).ok_or(error)?,
            tokens: self.tokens.get(..self.used).ok_or(error)?,
        })
    }
}
impl OwnedCursor {
    /// Preserve both arenas on refusal. Successful views borrow the returned
    /// owner, which can later return the same buffers for a new request.
    pub fn finish(self) -> Result<OwnedDocument, OwnedRefusal> {
        if let Err(error) = self.completion() {
            return Err(Refusal {
                error,
                bytes: self.bytes,
                tokens: self.tokens,
            });
        }
        Ok(OwnedDocument {
            limits: self.limits,
            bytes: self.bytes,
            tokens: self.tokens,
            input_len: self.input_len,
            used: self.used,
        })
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
    fn borrowed_prefix_ignores_unreceived_capacity() {
        let mut bytes = *b"null garbage";
        let mut tokens = [Token::EMPTY; 4];
        let mut ceiling = limits();
        ceiling.max_bytes = 4;
        let mut cursor =
            Cursor::with_input_length(&mut bytes[..], &mut tokens[..], 4, ceiling).unwrap();
        while !cursor.step().unwrap() {}
        let document = cursor.finish().unwrap();
        assert!(document.root().is_null());
        assert_eq!(document.bytes.len(), 4);
    }

    #[test]
    fn occupied_limit_is_independent_of_capacity_and_storage_errors_win() {
        let mut bytes = vec![0xff; 64].into_boxed_slice();
        bytes[..3].copy_from_slice(b"[1]");
        let mut ceiling = limits();
        ceiling.max_bytes = 3;
        let tokens = vec![Token::EMPTY; 8].into_boxed_slice();
        let mut cursor = OwnedCursor::with_input_length(bytes, tokens, 3, ceiling).unwrap();
        while !cursor.step().unwrap() {}
        let (bytes, tokens) = cursor.finish().unwrap().into_buffers();
        let refusal = OwnedCursor::with_input_length(bytes, tokens, 99, ceiling)
            .err()
            .unwrap();
        assert_eq!(refusal.error().kind, ErrorKind::Storage);
    }

    #[test]
    fn string_and_escape_reads_stop_at_occupied_prefix() {
        for (input, length) in [(br#""abc""#.as_slice(), 4), (br#""\u0041""#.as_slice(), 5)] {
            let bytes = input.to_vec().into_boxed_slice();
            let tokens = vec![Token::EMPTY; 8].into_boxed_slice();
            let mut cursor =
                OwnedCursor::with_input_length(bytes, tokens, length, limits()).unwrap();
            loop {
                match cursor.step() {
                    Ok(false) => (),
                    Ok(true) => panic!("accepted an incomplete string"),
                    Err(error) => {
                        assert_eq!(error.kind, ErrorKind::Syntax);
                        break;
                    }
                }
            }
        }
    }

    #[test]
    fn document_indices_visit_sorted_members_and_wire_order_arrays() {
        let mut bytes = br#"{"z":[1,2],"a":{"b":null}}"#.to_vec();
        let mut tokens = [Token::EMPTY; 16];
        let document = parse(&mut bytes, &mut tokens, limits()).unwrap();
        assert!(document.value_at(usize::MAX).is_none());
        assert!(document.member_at(usize::MAX).is_none());
        assert!(document.next_index(usize::MAX).is_none());
        let first = document.root().first_child_index().unwrap();
        let (key, object) = document.member_at(first).unwrap();
        assert_eq!(key, b"a");
        assert!(document.value_at(first).is_none());
        assert!(document.member_at(object.token_index()).is_none());
        let (_, null) = document
            .member_at(object.first_child_index().unwrap())
            .unwrap();
        assert!(document.value_at(null.token_index()).unwrap().is_null());
        assert!(null.first_child_index().is_none());
        let last = document.next_index(first).unwrap();
        assert!(document.next_index(last).is_none());
        let (key, array) = document.member_at(last).unwrap();
        assert_eq!(key, b"z");
        let one = array.first_child_index().unwrap();
        let two = document.next_index(one).unwrap();
        assert_eq!(document.value_at(one).unwrap().as_number(), Some("1"));
        assert_eq!(document.value_at(two).unwrap().as_number(), Some("2"));
        // Array iteration ends at its count, not at a null link: here the
        // continuation is the next physical key outside this array.
        assert_eq!(document.next_index(two), Some(first));
        assert_eq!(array.as_array().unwrap().count(), 2);
    }

    #[test]
    fn owned_cursor_moves_between_turns_and_reuses_exact_arenas() {
        fn is_send<T: Send>() {}
        is_send::<OwnedCursor>();
        is_send::<OwnedDocument>();
        let mut bytes = vec![0xff; 8192].into_boxed_slice();
        let mut tokens = vec![Token::EMPTY; 32].into_boxed_slice();
        let byte_ptr = bytes.as_ptr();
        let token_ptr = tokens.as_ptr();
        for input in [
            format!("[\"{}\",42]", "x".repeat(4096)),
            "{\"a\":true}".into(),
        ] {
            bytes[..input.len()].copy_from_slice(input.as_bytes());
            let mut queue =
                Some(OwnedCursor::with_input_length(bytes, tokens, input.len(), limits()).unwrap());
            loop {
                let mut cursor = queue.take().unwrap();
                if cursor.step().unwrap() {
                    let owner = cursor.finish().unwrap();
                    let view = owner.view().unwrap();
                    let mut output = String::new();
                    view.root().write_json(&mut output).unwrap();
                    assert_eq!(output, input);
                    (bytes, tokens) = owner.into_buffers();
                    break;
                }
                queue = Some(cursor);
            }
            assert_eq!(bytes.as_ptr(), byte_ptr);
            assert_eq!(tokens.as_ptr(), token_ptr);
            assert_eq!(bytes.len(), 8192);
            assert_eq!(tokens.len(), 32);
        }
    }

    #[test]
    fn owned_refusals_return_buffers_without_disclosing_contents() {
        let bytes = b"secret".to_vec().into_boxed_slice();
        let tokens = vec![Token::EMPTY; 8].into_boxed_slice();
        let pointers = (bytes.as_ptr(), tokens.as_ptr());
        let refusal = OwnedCursor::with_input_length(bytes, tokens, 7, limits())
            .err()
            .unwrap();
        assert_eq!(refusal.error().kind, ErrorKind::Storage);
        assert!(!format!("{refusal:?}").contains("secret"));
        let (bytes, tokens) = refusal.into_buffers();
        let cursor = OwnedCursor::with_input_length(bytes, tokens, 6, limits()).unwrap();
        let refusal = cursor.finish().err().unwrap();
        assert_eq!(refusal.error().kind, ErrorKind::Storage);
        let (bytes, tokens) = refusal.into_buffers();
        let mut cursor = OwnedCursor::with_input_length(bytes, tokens, 6, limits()).unwrap();
        let error = cursor.step().unwrap_err();
        assert_eq!(cursor.step(), Err(error));
        let refusal = cursor.finish().err().unwrap();
        assert_eq!(refusal.error(), error);
        let (bytes, tokens) = refusal.into_buffers();
        assert_eq!((bytes.as_ptr(), tokens.as_ptr()), pointers);
        let mut ceiling = limits();
        ceiling.max_bytes = 5;
        let refusal = OwnedCursor::with_input_length(bytes, tokens, 6, ceiling)
            .err()
            .unwrap();
        assert_eq!(refusal.error().kind, ErrorKind::Limit(ParseLimit::Bytes));
        let (bytes, tokens) = refusal.into_buffers();
        let cursor = OwnedCursor::with_input_length(bytes, tokens, 6, limits()).unwrap();
        let (bytes, tokens) = cursor.into_buffers();
        assert_eq!((bytes.as_ptr(), tokens.as_ptr()), pointers);
    }

    #[test]
    fn occupied_prefix_is_the_utf8_and_grammar_boundary() {
        for (input, length) in [
            (b"null false".as_slice(), 4),
            (b"\"\xc3\xa9\"".as_slice(), 2),
        ] {
            let bytes = input.to_vec().into_boxed_slice();
            let tokens = vec![Token::EMPTY; 8].into_boxed_slice();
            let mut cursor =
                OwnedCursor::with_input_length(bytes, tokens, length, limits()).unwrap();
            if length == 4 {
                while !cursor.step().unwrap() {}
                assert!(cursor.finish().unwrap().view().unwrap().root().is_null());
            } else {
                assert_eq!(
                    cursor.step().unwrap_err(),
                    Error {
                        kind: ErrorKind::Syntax,
                        offset: 1
                    }
                );
            }
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
    #[test]
    fn cursor_yields_inside_large_strings_numbers_and_shared_key_prefixes() {
        for input in [
            format!("\"{}\"", "\\ud834\\udd1e".repeat(10000)),
            "1".repeat(100000),
            format!(
                "{{\"{}b\":0,\"{}a\":1}}",
                "x".repeat(20000),
                "x".repeat(20000)
            ),
        ] {
            let expected = crate::parse(&input).unwrap().to_canonical();
            let mut bytes = input.into_bytes();
            let mut tokens = [Token::EMPTY; 8];
            let mut cursor = Cursor::new(&mut bytes, &mut tokens, limits()).unwrap();
            let mut turns = 0;
            let mut sorting_turns = 0;
            let mut string_turns = 0;
            let mut number_turns = 0;
            loop {
                let before = cursor.position;
                let validating_utf8 = matches!(cursor.mode, Mode::Utf8);
                let done = cursor.step().unwrap();
                if !validating_utf8 {
                    assert!(cursor.position.saturating_sub(before) <= 256 * 12);
                }
                if done {
                    break;
                }
                turns += 1;
                if matches!(cursor.mode, Mode::String { .. }) {
                    string_turns += 1;
                }
                if matches!(cursor.mode, Mode::Number { .. }) {
                    number_turns += 1;
                }
                if matches!(cursor.mode, Mode::Sort(_)) {
                    sorting_turns += 1;
                }
                assert!(turns < 10000);
            }
            assert!(turns > 100);
            let document = cursor.finish().unwrap();
            if document.root().as_object().is_some() {
                assert!(sorting_turns > 50);
            } else if document.root().as_str().is_some() {
                assert!(string_turns > 20);
            } else {
                assert!(number_turns > 100);
            }
            let mut output = String::new();
            document.root().write_json(&mut output).unwrap();
            assert_eq!(output, expected);
        }
        assert!(std::mem::size_of::<Cursor<'_>>() <= 4096);
    }
    #[test]
    fn cursor_hides_partial_documents_and_repeats_terminal_errors() {
        let mut bytes = vec![b' '; 4096];
        let mut tokens = [Token::EMPTY; 8];
        let mut cursor = Cursor::new(&mut bytes, &mut tokens, limits()).unwrap();
        assert!(!cursor.step().unwrap());
        assert_eq!(cursor.finish().err().unwrap().kind, ErrorKind::Storage);
        for input in [
            br#"{"a":0,"a":1}"#.as_slice(),
            br#""\ud800""#,
            &[0xf0, 0x90, 0x80],
        ] {
            let mut bytes = input.to_vec();
            let mut cursor = Cursor::new(&mut bytes, &mut tokens, limits()).unwrap();
            let error = loop {
                match cursor.step() {
                    Err(error) => break error,
                    Ok(false) => (),
                    Ok(true) => panic!("malformed document accepted"),
                }
            };
            assert_eq!(cursor.step(), Err(error));
            assert_eq!(cursor.finish().err(), Some(error));
        }
    }
    #[test]
    fn sorting_all_permutations_preserves_nested_values_and_duplicate_offsets() {
        fn permutations(prefix: &mut Vec<usize>, rest: &mut Vec<usize>) {
            if rest.is_empty() {
                let text = prefix
                    .iter()
                    .map(|i| format!("\"k{i}\":[{{\"value\":{i}}},[]]"))
                    .collect::<Vec<_>>()
                    .join(",");
                compare(&format!("{{{text}}}"));
                compare(&format!("{{{text},\"k2\":false}}"));
            } else {
                for index in 0..rest.len() {
                    let value = rest.remove(index);
                    prefix.push(value);
                    permutations(prefix, rest);
                    prefix.pop();
                    rest.insert(index, value);
                }
            }
        }
        permutations(&mut Vec::new(), &mut (0..6).collect());
    }
    #[test]
    fn closed_duplicate_object_precedes_later_depth_token_and_storage_errors() {
        for (depth, token_limit, backing) in [(2, 32, 32), (128, 6, 32), (128, 32, 6)] {
            let mut bytes = br#"[{"a":1,"a":2},[[0]]]"#.to_vec();
            let mut tokens = vec![Token::EMPTY; backing];
            let error = parse(
                &mut bytes,
                &mut tokens,
                ParseLimits {
                    max_depth: depth,
                    max_tokens: token_limit,
                    ..limits()
                },
            )
            .err()
            .unwrap();
            assert_eq!(
                error,
                Error {
                    kind: ErrorKind::Syntax,
                    offset: 8
                }
            );
        }
    }
    #[test]
    fn utf8_error_offsets_match_whole_input_validation() {
        for invalid in [
            &[0xc2, b'x'][..],
            &[0xe0, 0x80, 0x80],
            &[0xf4, 0x90, 0x80, 0x80],
            &[0xf0, 0x90, 0x80],
        ] {
            for prefix in ["", "abc", "é☃"] {
                let mut bytes = prefix.as_bytes().to_vec();
                bytes.extend_from_slice(invalid);
                let expected = std::str::from_utf8(&bytes).err().unwrap().valid_up_to();
                let mut tokens = [Token::EMPTY; 8];
                assert_eq!(
                    parse(&mut bytes, &mut tokens, limits()).err().unwrap(),
                    Error {
                        kind: ErrorKind::Syntax,
                        offset: expected
                    }
                );
            }
        }
    }
    #[test]
    fn bounded_byte_views_preserve_decoding_and_type_boundaries() {
        let mut bytes = br#"{"\u0061":"\ud834\udd1e","z":[1,null]}"#.to_vec();
        let mut tokens = [Token::EMPTY; 10];
        let document = parse(&mut bytes, &mut tokens, limits()).unwrap();
        assert_eq!(document.string_bytes_at(0), None);
        assert_eq!(document.string_bytes_at(1), Some(b"a".as_slice()));
        assert_eq!(document.string_bytes_at(2), Some("𝄞".as_bytes()));
        assert_eq!(document.string_bytes_at(document.token_count()), None);
        let mut object = document.root().as_object().unwrap();
        let (key, value) = object.next_bytes().unwrap();
        assert_eq!(key, b"a");
        assert_eq!(value.as_string_bytes(), Some("𝄞".as_bytes()));
        let (key, value) = object.next_bytes().unwrap();
        assert_eq!(key, b"z");
        assert_eq!(value.as_string_bytes(), None);
        assert!(object.next_bytes().is_none());
    }
    #[test]
    fn ordinary_object_iteration_counts_key_revalidation() {
        let mut bytes = br#"{"key":null}"#.to_vec();
        let mut tokens = [Token::EMPTY; 3];
        let document = parse(&mut bytes, &mut tokens, limits()).unwrap();
        UTF8_BYTES.with(|count| count.set(0));
        assert_eq!(
            document.root().as_object().unwrap().next().unwrap().0,
            "key"
        );
        assert_eq!(UTF8_BYTES.with(|count| count.get()), 3);
        UTF8_BYTES.with(|count| count.set(0));
        assert_eq!(
            document.root().as_object().unwrap().next_bytes().unwrap().0,
            b"key"
        );
        assert_eq!(UTF8_BYTES.with(|count| count.get()), 0);
    }
}

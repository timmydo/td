//! The D-Bus wire format: the type grammar, the aligned reader, and the writer.
//!
//! D-Bus marshalling is the only wire format td-busd implements. Two properties
//! of it are the ones a fresh implementation gets wrong, so both are stated
//! once here and tested per entry rather than left to the code:
//!
//! * **Alignment is measured from the start of the MESSAGE**, not from the
//!   start of whatever buffer a reader happens to hold. Every `Reader` and
//!   `Writer` here therefore carries the invariant that its buffer begins at an
//!   8-aligned message offset — which both places that build one satisfy: the
//!   header-field array starts at offset 16, and a body starts at an offset
//!   padded to 8.
//! * **An array's leading alignment padding is counted OUTSIDE its declared
//!   length**, while the padding between later elements is inside it. A reader
//!   that has this backwards reads a plausible length from the wrong offset.

use std::fmt;
use std::str;

use crate::name;

/// The specification's cap on a signature, in bytes. It is a `u8` length on the
/// wire, so 255 is also as long as one can be spelled.
pub const MAX_SIGNATURE_LEN: usize = 255;

/// The specification's cap on one array's declared payload length (2^26).
///
/// The message layer's body ceiling is tighter and is what actually bounds an
/// array arriving in a message; this is the grammar's own bound, and it refuses
/// a declared length before anything is sized from it.
pub const MAX_ARRAY_BYTES: usize = 67_108_864;

/// Container nesting, per the specification's two counters (arrays, structs)
/// plus a third it leaves open.
///
/// A variant carries its own signature on the wire rather than in the enclosing
/// one, so nothing in a message's declared type bounds how deep variants go —
/// which makes an n-deep variant chain an n-deep recursion here for three bytes
/// each. Capping it is what keeps this walk's stack bounded by the grammar
/// rather than by the sender.
pub const MAX_NESTING: u32 = 32;

/// Container nesting of ANY kind, which is what actually bounds the recursion.
///
/// Three counters of 32 admit a 96-deep interleaving, so `MAX_NESTING` alone
/// does not describe the stack this walk can reach. 64 is the specification's
/// own combined array-and-struct allowance.
///
/// It is NOT true that nothing the specification permits is refused by this:
/// 32 arrays inside 32 structs is exactly 64, so a variant anywhere within —
/// which both per-kind counters still allow — is refused. Nor is 64 levels of
/// ordinary traffic: a dict entry counts as a struct here, so each `a{sv}`
/// level costs three and a property bag bottoms out around 21 deep. Both are
/// far past anything a desktop sends, which is why the constant stands and
/// this paragraph is what got corrected.
pub const MAX_NESTING_TOTAL: u32 = 2 * MAX_NESTING;

/// Elements per array, applied ONLY where the element type is a container.
///
/// An array is bounded by BYTES — `ay` is how D-Bus carries a blob, so a
/// notification icon is a byte array of millions of elements and a flat element
/// cap would refuse ordinary desktop traffic. Containers are where the byte
/// bound alone leaves per-element bookkeeping unbounded, and are the only case
/// this counts.
pub const MAX_CONTAINER_ELEMENTS: usize = 65_536;

/// How many elements `render` will walk, and how many bytes of a blob it will
/// spell. It is a diagnostic: a walked container past this is REFUSED, since a
/// partial decode is not an expectation anything can be checked against, while
/// a blob is elided, since its bytes are already known to decode.
const MAX_RENDER_ELEMENTS: usize = 1_024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Endian {
    Little,
    Big,
}

impl Endian {
    pub const LITTLE_BYTE: u8 = b'l';
    pub const BIG_BYTE: u8 = b'B';

    pub fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            Self::LITTLE_BYTE => Some(Self::Little),
            Self::BIG_BYTE => Some(Self::Big),
            _ => None,
        }
    }

    pub fn byte(self) -> u8 {
        match self {
            Self::Little => Self::LITTLE_BYTE,
            Self::Big => Self::BIG_BYTE,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WireError {
    Truncated,
    TrailingBytes,
    NonZeroPadding,
    BadSignature,
    SignatureTooLong,
    ReservedTypeCode,
    NestingTooDeep,
    EmptyStruct,
    BadDictEntry,
    NonBasicDictKey,
    NonNormalBool,
    BadUtf8,
    InteriorNul,
    MissingNul,
    BadObjectPath,
    ArrayLengthTooLarge,
    ArrayLengthMismatch,
    TooManyElements,
    FdIndexOutOfRange,
    ValueTooLarge,
    Overflow,
}

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Truncated => "a value runs past the end of its buffer",
            Self::TrailingBytes => "bytes remain after the declared type was read",
            Self::NonZeroPadding => "alignment padding is not zero",
            Self::BadSignature => "malformed type signature",
            Self::SignatureTooLong => "signature longer than 255 bytes",
            Self::ReservedTypeCode => "reserved or unknown type code",
            Self::NestingTooDeep => "container nesting past the limit",
            Self::EmptyStruct => "a struct must carry at least one field",
            Self::BadDictEntry => "a dict entry must be an array element with two fields",
            Self::NonBasicDictKey => "a dict entry key must be a basic type",
            Self::NonNormalBool => "a boolean is neither 0 nor 1",
            Self::BadUtf8 => "a string is not valid UTF-8",
            Self::InteriorNul => "a string carries an interior NUL",
            Self::MissingNul => "a string is not NUL-terminated",
            Self::BadObjectPath => "malformed object path",
            Self::ArrayLengthTooLarge => "an array declares more than 2^26 bytes",
            Self::ArrayLengthMismatch => "an array's elements do not fill its declared length",
            Self::TooManyElements => "too many container elements",
            Self::FdIndexOutOfRange => "a descriptor index names a descriptor that did not arrive",
            Self::ValueTooLarge => "a value is too large to marshal",
            Self::Overflow => "a length computation overflowed",
        })
    }
}

/// How many descriptors accompanied the message being read.
///
/// A body value of type `h` is an INDEX rather than a descriptor, so every one
/// is bounds-checked against what actually arrived: the count says how many are
/// there and says nothing about whether index 7 exists when three did.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Limits {
    pub fds: u32,
}

impl Limits {
    pub const NO_FDS: Self = Self { fds: 0 };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Kind {
    Array,
    Struct,
    Variant,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct Depth {
    array: u32,
    structure: u32,
    variant: u32,
    total: u32,
}

impl Depth {
    fn counter(&mut self, kind: Kind) -> &mut u32 {
        match kind {
            Kind::Array => &mut self.array,
            Kind::Struct => &mut self.structure,
            Kind::Variant => &mut self.variant,
        }
    }

    fn enter(&mut self, kind: Kind) -> Result<(), WireError> {
        let counter = self.counter(kind);
        *counter = counter.checked_add(1).ok_or(WireError::Overflow)?;
        if *counter > MAX_NESTING {
            return Err(WireError::NestingTooDeep);
        }
        self.total = self.total.checked_add(1).ok_or(WireError::Overflow)?;
        if self.total > MAX_NESTING_TOTAL {
            return Err(WireError::NestingTooDeep);
        }
        Ok(())
    }

    fn leave(&mut self, kind: Kind) {
        let counter = self.counter(kind);
        *counter = counter.saturating_sub(1);
        self.total = self.total.saturating_sub(1);
    }
}

/// The width of a fixed-size type whose every bit pattern is a legal value, or
/// `None` for anything needing a per-value check or a walk.
///
/// `b` is a u32 of which only two values decode and `h` is an index bounded by
/// what arrived, so neither is here however fixed its width.
fn flat_element_width(code: u8) -> Option<usize> {
    match code {
        b'y' => Some(1),
        b'n' | b'q' => Some(2),
        b'i' | b'u' => Some(4),
        b'x' | b't' | b'd' => Some(8),
        _ => None,
    }
}

/// The alignment of a value whose type begins with `code`, in bytes.
///
/// Arrays and variants were missing from an earlier draft of this table, which
/// is the kind of omission that does not fail loudly, so every row has a test.
pub fn alignment(code: u8) -> Result<usize, WireError> {
    Ok(match code {
        b'y' | b'g' | b'v' => 1,
        b'n' | b'q' => 2,
        b'b' | b'i' | b'u' | b'h' | b's' | b'o' | b'a' => 4,
        b'x' | b't' | b'd' | b'(' | b'{' => 8,
        _ => return Err(WireError::ReservedTypeCode),
    })
}

/// Whether `code` is a basic (non-container) type — what a dict entry key may
/// be, and what a match rule's `argN` can compare against.
pub fn is_basic(code: u8) -> bool {
    matches!(
        code,
        b'y' | b'b' | b'n' | b'q' | b'i' | b'u' | b'x' | b't' | b'd' | b's' | b'o' | b'g' | b'h'
    )
}

fn is_container(code: u8) -> bool {
    matches!(code, b'a' | b'v' | b'(' | b'{')
}

/// Length in bytes of the first complete type in `sig`, validating it.
pub fn complete_type_len(sig: &[u8]) -> Result<usize, WireError> {
    type_len(sig, &mut Depth::default())
}

/// As `complete_type_len`, but also accepting the `{kv}` an array may carry as
/// its element type and nothing else may.
fn element_type_len(sig: &[u8]) -> Result<usize, WireError> {
    match sig.first() {
        Some(&b'{') => dict_entry_len(sig, &mut Depth::default()),
        _ => complete_type_len(sig),
    }
}

fn type_len(sig: &[u8], depth: &mut Depth) -> Result<usize, WireError> {
    let code = *sig.first().ok_or(WireError::BadSignature)?;
    match code {
        b'y' | b'b' | b'n' | b'q' | b'i' | b'u' | b'x' | b't' | b'd' | b's' | b'o' | b'g'
        | b'h' | b'v' => Ok(1),
        b'a' => {
            depth.enter(Kind::Array)?;
            let rest = sig.get(1..).ok_or(WireError::BadSignature)?;
            let inner = match rest.first() {
                Some(&b'{') => dict_entry_len(rest, depth)?,
                _ => type_len(rest, depth)?,
            };
            depth.leave(Kind::Array);
            inner.checked_add(1).ok_or(WireError::Overflow)
        }
        b'(' => {
            depth.enter(Kind::Struct)?;
            let mut rest = sig.get(1..).ok_or(WireError::BadSignature)?;
            let mut fields = 0usize;
            let mut used = 0usize;
            loop {
                match rest.first() {
                    None => return Err(WireError::BadSignature),
                    Some(&b')') => break,
                    Some(_) => {}
                }
                let n = type_len(rest, depth)?;
                fields += 1;
                used = used.checked_add(n).ok_or(WireError::Overflow)?;
                rest = rest.get(n..).ok_or(WireError::BadSignature)?;
            }
            if fields == 0 {
                return Err(WireError::EmptyStruct);
            }
            depth.leave(Kind::Struct);
            used.checked_add(2).ok_or(WireError::Overflow)
        }
        // A dict entry is legal only as an array's element type. Standing alone
        // it is a signature to refuse, not one to read loosely.
        b'{' => Err(WireError::BadDictEntry),
        _ => Err(WireError::ReservedTypeCode),
    }
}

/// Length of `{kv}` including both braces, with `sig` positioned at the `{`.
fn dict_entry_len(sig: &[u8], depth: &mut Depth) -> Result<usize, WireError> {
    depth.enter(Kind::Struct)?;
    let inner = sig.get(1..).ok_or(WireError::BadSignature)?;
    if !is_basic(*inner.first().ok_or(WireError::BadSignature)?) {
        return Err(WireError::NonBasicDictKey);
    }
    let key = type_len(inner, depth)?;
    let after_key = inner.get(key..).ok_or(WireError::BadSignature)?;
    if after_key.first() == Some(&b'}') {
        return Err(WireError::BadDictEntry);
    }
    let value = type_len(after_key, depth)?;
    let after_value = after_key.get(value..).ok_or(WireError::BadSignature)?;
    if after_value.first() != Some(&b'}') {
        return Err(WireError::BadDictEntry);
    }
    depth.leave(Kind::Struct);
    key.checked_add(value)
        .and_then(|n| n.checked_add(2))
        .ok_or(WireError::Overflow)
}

/// A signature is zero or more complete types, at most 255 bytes.
pub fn validate_signature(sig: &str) -> Result<(), WireError> {
    if sig.len() > MAX_SIGNATURE_LEN {
        return Err(WireError::SignatureTooLong);
    }
    let mut rest = sig.as_bytes();
    while !rest.is_empty() {
        let n = complete_type_len(rest)?;
        rest = rest.get(n..).ok_or(WireError::BadSignature)?;
    }
    Ok(())
}

/// How many complete types a signature carries — a message's argument count.
pub fn signature_arity(sig: &str) -> Result<usize, WireError> {
    let mut rest = sig.as_bytes();
    let mut count = 0usize;
    while !rest.is_empty() {
        let n = complete_type_len(rest)?;
        count += 1;
        rest = rest.get(n..).ok_or(WireError::BadSignature)?;
    }
    Ok(count)
}

/// An aligned cursor over marshalled bytes.
///
/// `buf` must begin at an 8-aligned offset of the enclosing message, since
/// every alignment below is computed from position 0 of this buffer.
pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
    endian: Endian,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8], endian: Endian) -> Self {
        Self {
            buf,
            pos: 0,
            endian,
        }
    }

    /// A reader positioned partway into a message.
    ///
    /// The buffer, not the position, is what carries the 8-aligned invariant:
    /// the header-field array is read at position 12 OF THE WHOLE MESSAGE
    /// precisely so its elements align against offset 0 of the message rather
    /// than against a slice that starts at 12.
    pub fn at(buf: &'a [u8], position: usize, endian: Endian) -> Self {
        Self {
            buf,
            pos: position,
            endian,
        }
    }

    pub fn position(&self) -> usize {
        self.pos
    }

    /// Read the one complete type `signature` describes.
    pub fn value(&mut self, signature: &'a str, limits: Limits) -> Result<Value<'a>, WireError> {
        let sig = signature.as_bytes();
        if complete_type_len(sig)? != sig.len() {
            return Err(WireError::BadSignature);
        }
        let (value, _) = read_one(self, sig, &mut Depth::default(), &limits)?;
        Ok(value)
    }

    pub fn is_empty(&self) -> bool {
        self.pos >= self.buf.len()
    }

    fn align(&mut self, to: usize) -> Result<(), WireError> {
        let remainder = self.pos % to;
        if remainder == 0 {
            return Ok(());
        }
        let end = self
            .pos
            .checked_add(to - remainder)
            .ok_or(WireError::Overflow)?;
        let padding = self.buf.get(self.pos..end).ok_or(WireError::Truncated)?;
        if padding.iter().any(|byte| *byte != 0) {
            return Err(WireError::NonZeroPadding);
        }
        self.pos = end;
        Ok(())
    }

    fn take(&mut self, count: usize) -> Result<&'a [u8], WireError> {
        let end = self.pos.checked_add(count).ok_or(WireError::Overflow)?;
        let bytes = self.buf.get(self.pos..end).ok_or(WireError::Truncated)?;
        self.pos = end;
        Ok(bytes)
    }

    fn fixed<const N: usize>(&mut self) -> Result<[u8; N], WireError> {
        self.align(N)?;
        let bytes = self.take(N)?;
        <[u8; N]>::try_from(bytes).map_err(|_| WireError::Truncated)
    }

    pub fn u8(&mut self) -> Result<u8, WireError> {
        Ok(*self.take(1)?.first().ok_or(WireError::Truncated)?)
    }

    pub fn u16(&mut self) -> Result<u16, WireError> {
        let bytes = self.fixed::<2>()?;
        Ok(match self.endian {
            Endian::Little => u16::from_le_bytes(bytes),
            Endian::Big => u16::from_be_bytes(bytes),
        })
    }

    pub fn u32(&mut self) -> Result<u32, WireError> {
        let bytes = self.fixed::<4>()?;
        Ok(match self.endian {
            Endian::Little => u32::from_le_bytes(bytes),
            Endian::Big => u32::from_be_bytes(bytes),
        })
    }

    pub fn u64(&mut self) -> Result<u64, WireError> {
        let bytes = self.fixed::<8>()?;
        Ok(match self.endian {
            Endian::Little => u64::from_le_bytes(bytes),
            Endian::Big => u64::from_be_bytes(bytes),
        })
    }

    /// A `s` or `o`: a u32 length, that many bytes, and a NUL.
    pub fn string(&mut self) -> Result<&'a str, WireError> {
        let len = usize::try_from(self.u32()?).map_err(|_| WireError::Overflow)?;
        let bytes = self.take(len)?;
        if self.take(1)?.first() != Some(&0) {
            return Err(WireError::MissingNul);
        }
        if bytes.contains(&0) {
            return Err(WireError::InteriorNul);
        }
        str::from_utf8(bytes).map_err(|_| WireError::BadUtf8)
    }

    /// A `g`: a u8 length, that many bytes, and a NUL. Unlike a string it is
    /// unaligned, which is why the two cannot share a reader.
    pub fn signature(&mut self) -> Result<&'a str, WireError> {
        let len = usize::from(self.u8()?);
        let bytes = self.take(len)?;
        if self.take(1)?.first() != Some(&0) {
            return Err(WireError::MissingNul);
        }
        if bytes.contains(&0) {
            return Err(WireError::InteriorNul);
        }
        str::from_utf8(bytes).map_err(|_| WireError::BadUtf8)
    }
}

/// A decoded value.
///
/// Containers hold the validated byte range they occupy rather than a vector of
/// their contents, so decoding a message allocates nothing: an `ay` carrying a
/// megabyte icon costs one slice, not a million values. What a caller wants
/// out of a container it asks for, bounded, through `Seq`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Value<'a> {
    Byte(u8),
    Bool(bool),
    Int16(i16),
    Uint16(u16),
    Int32(i32),
    Uint32(u32),
    Int64(i64),
    Uint64(u64),
    Double(f64),
    Str(&'a str),
    ObjectPath(&'a str),
    Signature(&'a str),
    UnixFd(u32),
    Array(Seq<'a>),
    Struct(Seq<'a>),
    DictEntry(Seq<'a>),
    Variant(Seq<'a>),
}

impl<'a> Value<'a> {
    /// The text of a `s`, `o` or `g` — the three types a name, path or type
    /// arrives as, and the only ones a match rule compares as strings.
    pub fn as_str(&self) -> Option<&'a str> {
        match self {
            Self::Str(text) | Self::ObjectPath(text) | Self::Signature(text) => Some(text),
            _ => None,
        }
    }

    pub fn as_u32(&self) -> Option<u32> {
        match self {
            Self::Uint32(value) => Some(*value),
            _ => None,
        }
    }

    pub fn as_seq(&self) -> Option<Seq<'a>> {
        match self {
            Self::Array(seq) | Self::Struct(seq) | Self::DictEntry(seq) | Self::Variant(seq) => {
                Some(*seq)
            }
            _ => None,
        }
    }
}

/// The validated bytes of one container, re-readable on demand.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Seq<'a> {
    buf: &'a [u8],
    start: usize,
    end: usize,
    sig: &'a str,
    endian: Endian,
    /// An array repeats `sig`; a struct, dict entry or variant walks it once.
    repeat: bool,
    limits: Limits,
    depth: Depth,
}

impl<'a> Seq<'a> {
    /// The element type of an array, or the field types of everything else.
    pub fn signature(&self) -> &'a str {
        self.sig
    }

    /// The container's marshalled payload.
    pub fn bytes(&self) -> &'a [u8] {
        self.buf.get(self.start..self.end).unwrap_or(&[])
    }

    /// The payload of an `ay` without walking it element by element — the shape
    /// every D-Bus blob arrives in.
    pub fn as_bytes(&self) -> Option<&'a [u8]> {
        if self.repeat && self.sig == "y" {
            Some(self.bytes())
        } else {
            None
        }
    }

    /// Every value, refusing more than `limit` of them.
    ///
    /// The limit is the caller's because it is the caller that allocates: the
    /// decode itself is bounded in BYTES, and a caller asking for the elements
    /// of a large array is where a count starts to matter.
    pub fn values(&self, limit: usize) -> Result<Vec<Value<'a>>, WireError> {
        let mut out = Vec::new();
        self.walk(limit, |value| {
            out.push(value);
            true
        })?;
        Ok(out)
    }

    fn walk<F>(&self, limit: usize, mut visit: F) -> Result<(), WireError>
    where
        F: FnMut(Value<'a>) -> bool,
    {
        let mut reader = Reader {
            buf: self.buf,
            pos: self.start,
            endian: self.endian,
        };
        let mut count = 0usize;
        let mut sig = self.sig.as_bytes();
        loop {
            if self.repeat {
                if reader.pos >= self.end {
                    return Ok(());
                }
            } else if sig.is_empty() {
                return Ok(());
            }
            if count >= limit {
                return Err(WireError::TooManyElements);
            }
            let mut depth = self.depth;
            let (value, used) = read_element(&mut reader, sig, &mut depth, &self.limits)?;
            count += 1;
            if !self.repeat {
                sig = sig.get(used..).ok_or(WireError::BadSignature)?;
            }
            if !visit(value) {
                return Ok(());
            }
        }
    }
}

fn read_element<'a>(
    reader: &mut Reader<'a>,
    sig: &'a [u8],
    depth: &mut Depth,
    limits: &Limits,
) -> Result<(Value<'a>, usize), WireError> {
    if sig.first() == Some(&b'{') {
        read_dict_entry(reader, sig, depth, limits)
    } else {
        read_one(reader, sig, depth, limits)
    }
}

/// Read the value the first complete type in `sig` describes, returning it and
/// how many signature bytes it consumed.
fn read_one<'a>(
    reader: &mut Reader<'a>,
    sig: &'a [u8],
    depth: &mut Depth,
    limits: &Limits,
) -> Result<(Value<'a>, usize), WireError> {
    let code = *sig.first().ok_or(WireError::BadSignature)?;
    let value = match code {
        b'y' => Value::Byte(reader.u8()?),
        b'b' => match reader.u32()? {
            0 => Value::Bool(false),
            1 => Value::Bool(true),
            _ => return Err(WireError::NonNormalBool),
        },
        b'n' => Value::Int16(reader.u16()? as i16),
        b'q' => Value::Uint16(reader.u16()?),
        b'i' => Value::Int32(reader.u32()? as i32),
        b'u' => Value::Uint32(reader.u32()?),
        b'x' => Value::Int64(reader.u64()? as i64),
        b't' => Value::Uint64(reader.u64()?),
        b'd' => Value::Double(f64::from_bits(reader.u64()?)),
        b'h' => {
            let index = reader.u32()?;
            if index >= limits.fds {
                return Err(WireError::FdIndexOutOfRange);
            }
            Value::UnixFd(index)
        }
        b's' => Value::Str(reader.string()?),
        b'o' => {
            let path = reader.string()?;
            if !name::valid_object_path(path) {
                return Err(WireError::BadObjectPath);
            }
            Value::ObjectPath(path)
        }
        b'g' => {
            let signature = reader.signature()?;
            validate_signature(signature)?;
            Value::Signature(signature)
        }
        b'v' => return read_variant(reader, depth, limits),
        b'a' => return read_array(reader, sig, depth, limits),
        b'(' => return read_struct(reader, sig, depth, limits),
        b'{' => return Err(WireError::BadDictEntry),
        _ => return Err(WireError::ReservedTypeCode),
    };
    Ok((value, 1))
}

fn read_array<'a>(
    reader: &mut Reader<'a>,
    sig: &'a [u8],
    depth: &mut Depth,
    limits: &Limits,
) -> Result<(Value<'a>, usize), WireError> {
    depth.enter(Kind::Array)?;
    // Captured INSIDE the container: a later re-walk of these bytes starts one
    // level down, so it must not be allowed a level the decode was not.
    let inner_depth = *depth;
    let after_a = sig.get(1..).ok_or(WireError::BadSignature)?;
    let element_len = element_type_len(after_a)?;
    let element = after_a.get(..element_len).ok_or(WireError::BadSignature)?;
    let code = *element.first().ok_or(WireError::BadSignature)?;

    let declared = usize::try_from(reader.u32()?).map_err(|_| WireError::Overflow)?;
    if declared > MAX_ARRAY_BYTES {
        return Err(WireError::ArrayLengthTooLarge);
    }
    // Outside the declared length, and present even when the array is empty.
    reader.align(alignment(code)?)?;
    let start = reader.pos;
    let end = start.checked_add(declared).ok_or(WireError::Overflow)?;
    if end > reader.buf.len() {
        return Err(WireError::Truncated);
    }

    // A fixed-width element with nothing to check per value needs no walk: its
    // alignment equals its width and the array starts aligned, so the elements
    // are contiguous and the only question is whether they fill the length.
    // `b` and `h` are deliberately absent — each has a per-value predicate.
    if let Some(width) = flat_element_width(code) {
        if declared % width != 0 {
            return Err(WireError::ArrayLengthMismatch);
        }
        reader.pos = end;
    } else {
        let counted = is_container(code);
        let mut count = 0usize;
        while reader.pos < end {
            count += 1;
            if counted && count > MAX_CONTAINER_ELEMENTS {
                return Err(WireError::TooManyElements);
            }
            let before = reader.pos;
            read_element(reader, element, depth, limits)?;
            // Every element type consumes at least one byte, but nothing in the
            // grammar states it: without this a type that consumed none would
            // spin here rather than fail.
            if reader.pos <= before {
                return Err(WireError::ArrayLengthMismatch);
            }
            if reader.pos > end {
                return Err(WireError::ArrayLengthMismatch);
            }
        }
        if reader.pos != end {
            return Err(WireError::ArrayLengthMismatch);
        }
    }
    depth.leave(Kind::Array);

    let seq = Seq {
        buf: reader.buf,
        start,
        end,
        sig: str::from_utf8(element).map_err(|_| WireError::BadSignature)?,
        endian: reader.endian,
        repeat: true,
        limits: *limits,
        depth: inner_depth,
    };
    Ok((Value::Array(seq), element_len.checked_add(1).ok_or(WireError::Overflow)?))
}

fn read_struct<'a>(
    reader: &mut Reader<'a>,
    sig: &'a [u8],
    depth: &mut Depth,
    limits: &Limits,
) -> Result<(Value<'a>, usize), WireError> {
    depth.enter(Kind::Struct)?;
    let inner_depth = *depth;
    reader.align(8)?;
    let start = reader.pos;
    let mut rest = sig.get(1..).ok_or(WireError::BadSignature)?;
    let mut fields = 0usize;
    let mut used = 0usize;
    loop {
        match rest.first() {
            None => return Err(WireError::BadSignature),
            Some(&b')') => break,
            Some(_) => {}
        }
        let (_, consumed) = read_one(reader, rest, depth, limits)?;
        fields += 1;
        used = used.checked_add(consumed).ok_or(WireError::Overflow)?;
        rest = rest.get(consumed..).ok_or(WireError::BadSignature)?;
    }
    if fields == 0 {
        return Err(WireError::EmptyStruct);
    }
    depth.leave(Kind::Struct);
    let inner = sig.get(1..=used).ok_or(WireError::BadSignature)?;
    let seq = Seq {
        buf: reader.buf,
        start,
        end: reader.pos,
        sig: str::from_utf8(inner).map_err(|_| WireError::BadSignature)?,
        endian: reader.endian,
        repeat: false,
        limits: *limits,
        depth: inner_depth,
    };
    Ok((Value::Struct(seq), used.checked_add(2).ok_or(WireError::Overflow)?))
}

fn read_dict_entry<'a>(
    reader: &mut Reader<'a>,
    sig: &'a [u8],
    depth: &mut Depth,
    limits: &Limits,
) -> Result<(Value<'a>, usize), WireError> {
    depth.enter(Kind::Struct)?;
    let inner_depth = *depth;
    reader.align(8)?;
    let start = reader.pos;
    let inner = sig.get(1..).ok_or(WireError::BadSignature)?;
    if !is_basic(*inner.first().ok_or(WireError::BadSignature)?) {
        return Err(WireError::NonBasicDictKey);
    }
    let (_, key) = read_one(reader, inner, depth, limits)?;
    let after_key = inner.get(key..).ok_or(WireError::BadSignature)?;
    if after_key.first() == Some(&b'}') {
        return Err(WireError::BadDictEntry);
    }
    let (_, value) = read_one(reader, after_key, depth, limits)?;
    let after_value = after_key.get(value..).ok_or(WireError::BadSignature)?;
    if after_value.first() != Some(&b'}') {
        return Err(WireError::BadDictEntry);
    }
    depth.leave(Kind::Struct);
    let used = key.checked_add(value).ok_or(WireError::Overflow)?;
    let fields = inner.get(..used).ok_or(WireError::BadSignature)?;
    let seq = Seq {
        buf: reader.buf,
        start,
        end: reader.pos,
        sig: str::from_utf8(fields).map_err(|_| WireError::BadSignature)?,
        endian: reader.endian,
        repeat: false,
        limits: *limits,
        depth: inner_depth,
    };
    Ok((Value::DictEntry(seq), used.checked_add(2).ok_or(WireError::Overflow)?))
}

fn read_variant<'a>(
    reader: &mut Reader<'a>,
    depth: &mut Depth,
    limits: &Limits,
) -> Result<(Value<'a>, usize), WireError> {
    depth.enter(Kind::Variant)?;
    let inner_depth = *depth;
    let inner = reader.signature()?;
    // A variant carries exactly one complete type, never zero and never two.
    if complete_type_len(inner.as_bytes())? != inner.len() {
        return Err(WireError::BadSignature);
    }
    let start = reader.pos;
    read_one(reader, inner.as_bytes(), depth, limits)?;
    depth.leave(Kind::Variant);
    let seq = Seq {
        buf: reader.buf,
        start,
        end: reader.pos,
        sig: inner,
        endian: reader.endian,
        repeat: false,
        limits: *limits,
        depth: inner_depth,
    };
    Ok((Value::Variant(seq), 1))
}

/// Read a whole body: every top-level type in `signature`, consuming `bytes`
/// exactly. Anything left over is a signature/body mismatch.
pub fn read_body<'a>(
    bytes: &'a [u8],
    signature: &'a str,
    endian: Endian,
    limits: Limits,
) -> Result<Vec<Value<'a>>, WireError> {
    validate_signature(signature)?;
    let mut reader = Reader::new(bytes, endian);
    let mut out = Vec::new();
    let mut sig = signature.as_bytes();
    while !sig.is_empty() {
        let mut depth = Depth::default();
        let (value, used) = read_one(&mut reader, sig, &mut depth, &limits)?;
        out.push(value);
        sig = sig.get(used..).ok_or(WireError::BadSignature)?;
    }
    if !reader.is_empty() {
        return Err(WireError::TrailingBytes);
    }
    Ok(out)
}

/// A marshaller.
///
/// It carries the same 8-aligned-origin invariant `Reader` does. It does NOT
/// check that what a caller writes matches the signature it declared for an
/// array or a variant — nothing here relates a closure to a type string — so
/// the message layer reads its own body back against its SIGNATURE before
/// sending it, which is where that check lives.
pub struct Writer {
    buf: Vec<u8>,
    endian: Endian,
}

impl Writer {
    pub fn new(endian: Endian) -> Self {
        Self {
            buf: Vec::new(),
            endian,
        }
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.buf
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.buf
    }

    fn align(&mut self, to: usize) {
        let remainder = self.buf.len() % to;
        if remainder != 0 {
            self.buf.resize(self.buf.len().saturating_add(to - remainder), 0);
        }
    }

    /// Pad to `to`, which the message layer needs between a header and the
    /// 8-aligned body that follows it.
    pub fn align_to(&mut self, to: usize) -> Result<(), WireError> {
        if !matches!(to, 1 | 2 | 4 | 8) {
            return Err(WireError::BadSignature);
        }
        self.align(to);
        Ok(())
    }

    /// Append already-marshalled bytes — a body built by its own writer.
    pub fn append(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    pub fn byte(&mut self, value: u8) {
        self.buf.push(value);
    }

    pub fn bool(&mut self, value: bool) {
        self.uint32(u32::from(value));
    }

    pub fn int16(&mut self, value: i16) {
        self.uint16(value as u16);
    }

    pub fn uint16(&mut self, value: u16) {
        self.align(2);
        let bytes = match self.endian {
            Endian::Little => value.to_le_bytes(),
            Endian::Big => value.to_be_bytes(),
        };
        self.buf.extend_from_slice(&bytes);
    }

    pub fn int32(&mut self, value: i32) {
        self.uint32(value as u32);
    }

    pub fn uint32(&mut self, value: u32) {
        self.align(4);
        let bytes = match self.endian {
            Endian::Little => value.to_le_bytes(),
            Endian::Big => value.to_be_bytes(),
        };
        self.buf.extend_from_slice(&bytes);
    }

    pub fn int64(&mut self, value: i64) {
        self.uint64(value as u64);
    }

    pub fn uint64(&mut self, value: u64) {
        self.align(8);
        let bytes = match self.endian {
            Endian::Little => value.to_le_bytes(),
            Endian::Big => value.to_be_bytes(),
        };
        self.buf.extend_from_slice(&bytes);
    }

    pub fn double(&mut self, value: f64) {
        self.uint64(value.to_bits());
    }

    pub fn unix_fd(&mut self, index: u32) {
        self.uint32(index);
    }

    pub fn string(&mut self, text: &str) -> Result<(), WireError> {
        if text.as_bytes().contains(&0) {
            return Err(WireError::InteriorNul);
        }
        let len = u32::try_from(text.len()).map_err(|_| WireError::ValueTooLarge)?;
        self.uint32(len);
        self.buf.extend_from_slice(text.as_bytes());
        self.buf.push(0);
        Ok(())
    }

    pub fn object_path(&mut self, path: &str) -> Result<(), WireError> {
        if !name::valid_object_path(path) {
            return Err(WireError::BadObjectPath);
        }
        self.string(path)
    }

    pub fn signature(&mut self, sig: &str) -> Result<(), WireError> {
        validate_signature(sig)?;
        let len = u8::try_from(sig.len()).map_err(|_| WireError::SignatureTooLong)?;
        self.byte(len);
        self.buf.extend_from_slice(sig.as_bytes());
        self.buf.push(0);
        Ok(())
    }

    /// An array of `element`, whose declared length is backfilled once `fill`
    /// has written the elements — and which excludes the padding to the first
    /// of them.
    pub fn array<F>(&mut self, element: &str, fill: F) -> Result<(), WireError>
    where
        F: FnOnce(&mut Self) -> Result<(), WireError>,
    {
        if element_type_len(element.as_bytes())? != element.len() {
            return Err(WireError::BadSignature);
        }
        let code = *element.as_bytes().first().ok_or(WireError::BadSignature)?;
        self.align(4);
        let length_at = self.buf.len();
        self.buf.extend_from_slice(&[0; 4]);
        self.align(alignment(code)?);
        let start = self.buf.len();
        fill(self)?;
        let written = self.buf.len().checked_sub(start).ok_or(WireError::Overflow)?;
        if written > MAX_ARRAY_BYTES {
            return Err(WireError::ValueTooLarge);
        }
        let declared = u32::try_from(written).map_err(|_| WireError::ValueTooLarge)?;
        let bytes = match self.endian {
            Endian::Little => declared.to_le_bytes(),
            Endian::Big => declared.to_be_bytes(),
        };
        let end = length_at.checked_add(4).ok_or(WireError::Overflow)?;
        self.buf
            .get_mut(length_at..end)
            .ok_or(WireError::Overflow)?
            .copy_from_slice(&bytes);
        Ok(())
    }

    pub fn structure<F>(&mut self, fill: F) -> Result<(), WireError>
    where
        F: FnOnce(&mut Self) -> Result<(), WireError>,
    {
        self.align(8);
        fill(self)
    }

    pub fn dict_entry<F>(&mut self, fill: F) -> Result<(), WireError>
    where
        F: FnOnce(&mut Self) -> Result<(), WireError>,
    {
        self.align(8);
        fill(self)
    }

    pub fn variant<F>(&mut self, sig: &str, fill: F) -> Result<(), WireError>
    where
        F: FnOnce(&mut Self) -> Result<(), WireError>,
    {
        if complete_type_len(sig.as_bytes())? != sig.len() {
            return Err(WireError::BadSignature);
        }
        self.signature(sig)?;
        fill(self)
    }
}

/// The low nibble of `byte` as a hex digit. The mask makes the fallback
/// unreachable; it is there because this crate does not `unwrap`.
fn hex_digit(byte: u8) -> char {
    char::from(
        *b"0123456789abcdef"
            .get(usize::from(byte & 0xf))
            .unwrap_or(&b'?'),
    )
}

/// A value as text, for diagnostics and for the corpus's expected decodes.
///
/// Unsigned types render in hex and signed ones in decimal, so that a committed
/// expectation beside a committed byte stream can be checked by eye against it.
/// Both directions are bounded by `MAX_RENDER_ELEMENTS`: a walked container
/// past it is refused, a blob past it is elided.
pub fn render(value: &Value<'_>) -> Result<String, WireError> {
    let mut out = String::new();
    render_into(value, &mut out)?;
    Ok(out)
}

fn render_into(value: &Value<'_>, out: &mut String) -> Result<(), WireError> {
    match value {
        Value::Byte(v) => out.push_str(&format!("y:{v:#x}")),
        Value::Bool(v) => out.push_str(&format!("b:{v}")),
        Value::Int16(v) => out.push_str(&format!("n:{v}")),
        Value::Uint16(v) => out.push_str(&format!("q:{v:#x}")),
        Value::Int32(v) => out.push_str(&format!("i:{v}")),
        Value::Uint32(v) => out.push_str(&format!("u:{v:#x}")),
        Value::Int64(v) => out.push_str(&format!("x:{v}")),
        Value::Uint64(v) => out.push_str(&format!("t:{v:#x}")),
        Value::Double(v) => out.push_str(&format!("d:{:#018x}", v.to_bits())),
        Value::Str(v) => out.push_str(&format!("s:{v}")),
        Value::ObjectPath(v) => out.push_str(&format!("o:{v}")),
        Value::Signature(v) => out.push_str(&format!("g:{v}")),
        Value::UnixFd(v) => out.push_str(&format!("h:{v:#x}")),
        Value::Array(seq) => render_seq("a", seq, out)?,
        Value::Struct(seq) => render_seq("r", seq, out)?,
        Value::DictEntry(seq) => render_seq("e", seq, out)?,
        Value::Variant(seq) => render_seq("v", seq, out)?,
    }
    Ok(())
}

fn render_seq(tag: &str, seq: &Seq<'_>, out: &mut String) -> Result<(), WireError> {
    // A blob renders as its bytes rather than as a million one-byte values,
    // which is also the only way a real `ay` fits under the element cap. It is
    // bounded by the same cap all the same: this is a diagnostic, and a
    // 16 MiB icon rendered whole is twice its size in one String.
    if let Some(payload) = seq.as_bytes() {
        out.push_str("ay:");
        for byte in payload.iter().take(MAX_RENDER_ELEMENTS) {
            out.push(hex_digit(byte >> 4));
            out.push(hex_digit(*byte));
        }
        if payload.len() > MAX_RENDER_ELEMENTS {
            out.push_str("...");
        }
        return Ok(());
    }
    out.push_str(tag);
    out.push('[');
    for (index, value) in seq.values(MAX_RENDER_ELEMENTS)?.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        render_into(value, out)?;
    }
    out.push(']');
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read<'a>(bytes: &'a [u8], sig: &'a str) -> Result<Vec<Value<'a>>, WireError> {
        read_body(bytes, sig, Endian::Little, Limits::NO_FDS)
    }

    /// Arrays and variants were missing from a draft of this table. Every row
    /// is here so that removing one is a failing test rather than a parser that
    /// reads a plausible value from the wrong offset.
    #[test]
    fn every_type_code_has_its_specified_alignment() {
        for (code, expected) in [
            (b'y', 1),
            (b'g', 1),
            (b'v', 1),
            (b'n', 2),
            (b'q', 2),
            (b'b', 4),
            (b'i', 4),
            (b'u', 4),
            (b'h', 4),
            (b's', 4),
            (b'o', 4),
            (b'a', 4),
            (b'x', 8),
            (b't', 8),
            (b'd', 8),
            (b'(', 8),
            (b'{', 8),
        ] {
            assert_eq!(
                alignment(code),
                Ok(expected),
                "{} has the wrong alignment",
                char::from(code)
            );
        }
        for reserved in *b"rem*?z)}" {
            assert_eq!(alignment(reserved), Err(WireError::ReservedTypeCode));
        }
    }

    #[test]
    fn the_signature_grammar_accepts_what_the_specification_allows() {
        for sig in [
            "",
            "y",
            "yus",
            "a{sv}",
            "(is)",
            "aay",
            "a(oa{sv})",
            "v",
            "ah",
            "a{s(ii)}",
            "((y))",
        ] {
            assert_eq!(validate_signature(sig), Ok(()), "{sig} should be valid");
        }
        for (sig, error) in [
            ("m", WireError::ReservedTypeCode),
            ("r", WireError::ReservedTypeCode),
            ("*", WireError::ReservedTypeCode),
            ("(i", WireError::BadSignature),
            ("i)", WireError::ReservedTypeCode),
            ("()", WireError::EmptyStruct),
            ("{sv}", WireError::BadDictEntry),
            ("a{v s}", WireError::NonBasicDictKey),
            ("a{s}", WireError::BadDictEntry),
            ("a{si i}", WireError::BadDictEntry),
            ("a", WireError::BadSignature),
        ] {
            assert_eq!(validate_signature(sig), Err(error), "{sig} should be refused");
        }
    }

    #[test]
    fn a_signature_is_capped_at_255_bytes() {
        let at_cap = "y".repeat(MAX_SIGNATURE_LEN);
        assert_eq!(validate_signature(&at_cap), Ok(()));
        let over = "y".repeat(MAX_SIGNATURE_LEN + 1);
        assert_eq!(validate_signature(&over), Err(WireError::SignatureTooLong));
    }

    #[test]
    fn array_and_struct_nesting_stop_at_the_cap() {
        let arrays = |depth: usize| format!("{}y", "a".repeat(depth));
        assert_eq!(validate_signature(&arrays(MAX_NESTING as usize)), Ok(()));
        assert_eq!(
            validate_signature(&arrays(MAX_NESTING as usize + 1)),
            Err(WireError::NestingTooDeep)
        );

        let structs = |depth: usize| {
            format!("{}y{}", "(".repeat(depth), ")".repeat(depth))
        };
        assert_eq!(validate_signature(&structs(MAX_NESTING as usize)), Ok(()));
        assert_eq!(
            validate_signature(&structs(MAX_NESTING as usize + 1)),
            Err(WireError::NestingTooDeep)
        );
    }

    /// A variant's type is on the wire rather than in the enclosing signature,
    /// so this cap is the only thing that bounds the recursion. Three bytes buy
    /// a level.
    #[test]
    fn variant_nesting_stops_at_the_cap() {
        fn nested(levels: usize) -> Vec<u8> {
            let mut bytes = Vec::new();
            for _ in 1..levels {
                bytes.extend_from_slice(&[0x01, b'v', 0x00]);
            }
            bytes.extend_from_slice(&[0x01, b'y', 0x00, 0x2a]);
            bytes
        }
        assert!(read(&nested(MAX_NESTING as usize), "v").is_ok());
        assert_eq!(
            read(&nested(MAX_NESTING as usize + 1), "v"),
            Err(WireError::NestingTooDeep)
        );
    }

    /// Three counters of 32 admit a 96-deep interleaving, so the per-kind cap
    /// alone does not describe the recursion this walk can reach. The total is
    /// what does, and nothing the specification permits — 32 arrays inside 32
    /// structs — is refused by it.
    #[test]
    fn nesting_of_mixed_kinds_is_bounded_in_total() {
        // `a` and `(` alternating: each pair costs one of each counter, so the
        // per-kind cap is never what refuses this.
        fn alternating(pairs: usize) -> String {
            let mut sig = String::new();
            for _ in 0..pairs {
                sig.push_str("a(");
            }
            sig.push('y');
            for _ in 0..pairs {
                sig.push(')');
            }
            sig
        }
        let half = (MAX_NESTING_TOTAL / 2) as usize;
        assert_eq!(validate_signature(&alternating(half)), Ok(()));
        assert_eq!(
            validate_signature(&alternating(half + 1)),
            Err(WireError::NestingTooDeep)
        );
        // Two kinds cannot exceed the total without exceeding their own cap
        // first, so isolating it takes THREE: 32 variants over 17 array/struct
        // pairs is 66 levels with every counter at 17 or 32. Nothing but the
        // total refuses it, and at 16 pairs — 64 exactly — it decodes.
        fn chain_sig(pairs: usize) -> String {
            let mut sig = String::new();
            for _ in 0..pairs {
                sig.push_str("a(");
            }
            sig.push('y');
            for _ in 0..pairs {
                sig.push(')');
            }
            sig
        }
        fn write_chain(w: &mut Writer, pairs: usize) -> Result<(), WireError> {
            let Some(inner) = pairs.checked_sub(1) else {
                w.byte(0x2a);
                return Ok(());
            };
            let element = format!("({})", chain_sig(inner));
            w.array(&element, |w| w.structure(|w| write_chain(w, inner)))
        }
        fn wrap(w: &mut Writer, variants: usize, pairs: usize) -> Result<(), WireError> {
            let Some(inner) = variants.checked_sub(1) else {
                return write_chain(w, pairs);
            };
            let sig = if inner == 0 {
                chain_sig(pairs)
            } else {
                "v".to_string()
            };
            w.variant(&sig, |w| wrap(w, inner, pairs))
        }
        let mixed = |pairs: usize| {
            let mut writer = Writer::new(Endian::Little);
            wrap(&mut writer, MAX_NESTING as usize, pairs).expect("the value marshals");
            writer.into_bytes()
        };
        assert!(read(&mixed(16), "v").is_ok());
        assert_eq!(read(&mixed(17), "v"), Err(WireError::NestingTooDeep));
    }

    /// The fast path skips the per-element walk, so what makes it sound is that
    /// there is nothing to check per element. That rests on two premises the
    /// compiler cannot see, and both are pinned here rather than argued in a
    /// comment: a type with a per-value predicate must not be on it, and the
    /// width it declares must be the alignment the elements actually sit at.
    ///
    /// The regression this exists for is concrete. Adding `b'h' => Some(4)`
    /// makes `read_body(&[4,0,0,0, 9,0,0,0], "ah", …)` return `Ok` with a
    /// descriptor index that names nothing — a refusal §D puts at the decode
    /// boundary, crossing it instead and reaching routing.
    #[test]
    fn the_fast_path_carries_only_types_with_nothing_to_check() {
        for code in *b"bh" {
            assert_eq!(
                flat_element_width(code),
                None,
                "{} has a per-value check and must not skip the walk",
                char::from(code)
            );
        }
        // Every type that IS on it: the width must equal the alignment, since
        // the fast path infers contiguity from the array's own start alignment.
        for code in *b"ynqiuxtd" {
            let width = flat_element_width(code).expect("a flat width");
            assert_eq!(
                Some(width),
                alignment(code).ok(),
                "{}'s width and alignment disagree",
                char::from(code)
            );
        }
        // And nothing else is on it at all — a container or a string reaching
        // the fast path would skip its own bounds checks.
        for code in *b"sogav({" {
            assert_eq!(flat_element_width(code), None, "{}", char::from(code));
        }

        // The two excluded types are refused for real, in an ARRAY, which is
        // the path the fast path would have taken over.
        assert_eq!(
            read_body(&[4, 0, 0, 0, 9, 0, 0, 0], "ah", Endian::Little, Limits { fds: 1 }),
            Err(WireError::FdIndexOutOfRange)
        );
        assert_eq!(
            read(&[4, 0, 0, 0, 2, 0, 0, 0], "ab"),
            Err(WireError::NonNormalBool)
        );
    }

    /// An array of a fixed-width type is not walked element by element, so the
    /// check that its elements FILL its declared length has to be arithmetic
    /// rather than a consequence of the walk stopping where it should.
    #[test]
    fn a_fixed_width_array_must_fill_its_declared_length() {
        // Six bytes declared for a `u` array is one and a half elements.
        let mut bytes = vec![6, 0, 0, 0];
        bytes.extend_from_slice(&[1, 0, 0, 0, 2, 0]);
        assert_eq!(read(&bytes, "au"), Err(WireError::ArrayLengthMismatch));

        let mut good = vec![8, 0, 0, 0];
        good.extend_from_slice(&[1, 0, 0, 0, 2, 0, 0, 0]);
        let values = read(&good, "au").expect("a whole number of elements decodes");
        let seq = values
            .first()
            .and_then(Value::as_seq)
            .expect("an array value");
        assert_eq!(
            seq.values(4),
            Ok(vec![Value::Uint32(1), Value::Uint32(2)])
        );
    }

    /// `render` is a diagnostic: a blob past the cap is elided rather than
    /// refused, since its bytes are already known to decode, while a walked
    /// container past it is refused, since a partial decode is not an
    /// expectation anything can be compared against.
    #[test]
    fn a_blob_render_is_elided_and_a_walked_container_is_refused() {
        let big = MAX_RENDER_ELEMENTS + 5;
        let mut writer = Writer::new(Endian::Little);
        writer
            .array("y", |w| {
                for _ in 0..big {
                    w.byte(0xab);
                }
                Ok(())
            })
            .expect("a blob marshals");
        let bytes = writer.into_bytes();
        let values = read(&bytes, "ay").expect("a blob decodes");
        let text = render(values.first().expect("a value")).expect("a blob renders");
        assert!(text.starts_with("ay:abab"));
        assert!(text.ends_with("..."));
        assert_eq!(text.len(), "ay:".len() + MAX_RENDER_ELEMENTS * 2 + 3);

        let mut writer = Writer::new(Endian::Little);
        writer
            .array("(y)", |w| {
                for _ in 0..(MAX_RENDER_ELEMENTS + 1) {
                    w.structure(|w| {
                        w.byte(1);
                        Ok(())
                    })?;
                }
                Ok(())
            })
            .expect("an array of structs marshals");
        let bytes = writer.into_bytes();
        let values = read(&bytes, "a(y)").expect("it decodes");
        assert_eq!(
            render(values.first().expect("a value")),
            Err(WireError::TooManyElements)
        );
    }

    /// The padding to an array's first element is present even when there is no
    /// first element — eight bytes for an empty `at`, four of them padding that
    /// the declared length of zero does not count.
    #[test]
    fn an_empty_array_still_carries_its_leading_padding() {
        let mut writer = Writer::new(Endian::Little);
        writer.array("t", |_| Ok(())).expect("empty array");
        let bytes = writer.into_bytes();
        assert_eq!(bytes, vec![0, 0, 0, 0, 0, 0, 0, 0]);
        let values = read(&bytes, "at").expect("an empty array decodes");
        let seq = values
            .first()
            .and_then(Value::as_seq)
            .expect("an array value");
        assert_eq!(seq.values(8), Ok(Vec::new()));
    }

    /// `ay` is how D-Bus carries a blob, so the byte bound is the only bound: a
    /// flat element cap would refuse an ordinary notification icon.
    #[test]
    fn a_byte_array_far_past_the_container_cap_is_accepted() {
        let payload: Vec<u8> = (0..MAX_CONTAINER_ELEMENTS + 1000)
            .map(|index| (index % 251) as u8)
            .collect();
        let mut writer = Writer::new(Endian::Little);
        writer
            .array("y", |w| {
                for byte in &payload {
                    w.byte(*byte);
                }
                Ok(())
            })
            .expect("a large byte array marshals");
        let bytes = writer.into_bytes();
        let values = read(&bytes, "ay").expect("a large byte array decodes");
        let seq = values
            .first()
            .and_then(Value::as_seq)
            .expect("an array value");
        assert_eq!(seq.as_bytes(), Some(payload.as_slice()));
    }

    /// Where the elements ARE containers the count is bounded, because that is
    /// where per-element bookkeeping is what grows rather than the payload.
    #[test]
    fn an_array_of_containers_stops_at_the_element_cap() {
        let build = |count: usize| {
            let mut writer = Writer::new(Endian::Little);
            writer
                .array("v", |w| {
                    for _ in 0..count {
                        w.variant("y", |w| {
                            w.byte(1);
                            Ok(())
                        })?;
                    }
                    Ok(())
                })
                .expect("an array of variants marshals");
            writer.into_bytes()
        };
        assert!(read(&build(MAX_CONTAINER_ELEMENTS), "av").is_ok());
        assert_eq!(
            read(&build(MAX_CONTAINER_ELEMENTS + 1), "av"),
            Err(WireError::TooManyElements)
        );
    }

    #[test]
    fn padding_inside_a_struct_must_be_zero() {
        let good = [0x01, 0, 0, 0, 0, 0, 0, 0, 0x02];
        assert!(read(&good, "y(y)").is_ok());
        let bad = [0x01, 0xff, 0, 0, 0, 0, 0, 0, 0x02];
        assert_eq!(read(&bad, "y(y)"), Err(WireError::NonZeroPadding));
    }

    /// The index is checked against what arrived, not against the declared
    /// count alone: the last legal index is one less than the count.
    #[test]
    fn a_descriptor_index_is_bounded_by_the_descriptors_that_arrived() {
        let two = Limits { fds: 2 };
        assert!(read_body(&[0x01, 0, 0, 0], "h", Endian::Little, two).is_ok());
        assert_eq!(
            read_body(&[0x02, 0, 0, 0], "h", Endian::Little, two),
            Err(WireError::FdIndexOutOfRange)
        );
        assert_eq!(
            read_body(&[0x00, 0, 0, 0], "h", Endian::Little, Limits::NO_FDS),
            Err(WireError::FdIndexOutOfRange)
        );
    }

    #[test]
    fn a_body_shorter_than_its_signature_is_truncated_and_a_longer_one_trails() {
        assert_eq!(read(&[0x01], "yy"), Err(WireError::Truncated));
        assert_eq!(read(&[0x01, 0x02], "y"), Err(WireError::TrailingBytes));
    }

    #[test]
    fn a_sequence_can_be_read_and_bounded() {
        let mut writer = Writer::new(Endian::Little);
        writer
            .array("s", |w| {
                w.string("one")?;
                w.string("two")?;
                w.string("three")
            })
            .expect("an array of strings marshals");
        let bytes = writer.into_bytes();
        let values = read(&bytes, "as").expect("decodes");
        let seq = values
            .first()
            .and_then(Value::as_seq)
            .expect("an array value");
        let values = seq.values(3).expect("three strings");
        assert_eq!(
            values.iter().filter_map(Value::as_str).collect::<Vec<_>>(),
            vec!["one", "two", "three"]
        );
        assert_eq!(seq.values(2), Err(WireError::TooManyElements));
    }

    #[test]
    fn the_writer_refuses_what_the_reader_would_have_to() {
        let mut writer = Writer::new(Endian::Little);
        assert_eq!(writer.string("a\0b"), Err(WireError::InteriorNul));
        assert_eq!(writer.object_path("nope"), Err(WireError::BadObjectPath));
        assert_eq!(writer.signature("m"), Err(WireError::ReservedTypeCode));
        assert_eq!(
            writer.variant("yy", |_| Ok(())),
            Err(WireError::BadSignature)
        );
        assert_eq!(
            writer.array("yy", |_| Ok(())),
            Err(WireError::BadSignature)
        );
        assert_eq!(
            writer.signature(&"y".repeat(MAX_SIGNATURE_LEN + 1)),
            Err(WireError::SignatureTooLong)
        );
    }

    #[test]
    fn arity_counts_complete_types() {
        assert_eq!(signature_arity(""), Ok(0));
        assert_eq!(signature_arity("y"), Ok(1));
        assert_eq!(signature_arity("yus"), Ok(3));
        assert_eq!(signature_arity("a{sv}"), Ok(1));
        assert_eq!(signature_arity("(is)ay"), Ok(2));
    }

    #[test]
    fn basic_types_are_the_ones_a_dict_may_be_keyed_by() {
        for code in "ybnqiuxtdsogh".bytes() {
            assert!(is_basic(code), "{} should be basic", char::from(code));
        }
        for code in "avr(){}".bytes() {
            assert!(!is_basic(code), "{} should not be basic", char::from(code));
        }
    }

    #[test]
    fn the_endianness_byte_is_l_or_capital_b_and_nothing_else() {
        assert_eq!(Endian::from_byte(b'l'), Some(Endian::Little));
        assert_eq!(Endian::from_byte(b'B'), Some(Endian::Big));
        assert_eq!(Endian::from_byte(b'b'), None);
        assert_eq!(Endian::from_byte(b'L'), None);
        assert_eq!(Endian::Little.byte(), b'l');
        assert_eq!(Endian::Big.byte(), b'B');
    }
}

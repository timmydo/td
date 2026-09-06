//! A dependency-free JSON value, parser, writer, and `json!` macro — the
//! `serde_json` replacement for td's terminal applications.
//!
//! The value shape is td's engine `Json` (`engine/src/json.rs`): numbers are
//! kept as their raw lexeme (`Num(String)`) so re-serialisation is exact with
//! no f64 round-trip, and objects are an order-preserving `Vec<(String, Json)>`
//! rather than a map, so `to_string` reproduces the order keys were written in
//! and `to_canonical` sorts them for comparison. This copy needs none of the
//! engine copy's grandfathered `unwrap`/indexing allowances: the module-level
//! `deny` block below is the proof.
//!
//! Differences from `serde_json` a caller must know about:
//!
//! * variants are `Null, Bool, Num, Str, Arr, Obj` (not `Number/String/…`);
//! * `to_string` preserves insertion order; `serde_json`'s default map is a
//!   `BTreeMap`, so its output is key-sorted — `to_canonical` matches that;
//! * duplicate object keys are a parse error rather than last-one-wins;
//! * finite floats are written with Rust's shortest `Display` form plus a
//!   forced `.0` when it has no fraction or exponent, so `1.0` is `1.0` and
//!   `-0.0` is `-0.0`; unlike ryu it never picks exponent form, so `1e21`
//!   writes as `1000000000000000000000.0` (same value, different bytes).
//!   Non-finite floats become `null`, as `serde_json` does;
//! * there is no `Deserialize`/`Serialize` derive: a caller converts its own
//!   types by implementing [`ToJson`] and reads with the accessors.
//!
//! Nesting is capped at [`MAX_DEPTH`] on the way in, so hostile input cannot
//! overflow the stack in the recursive parser; a value built by hand (or by
//! `json!`) is bounded by the program's own source, and the recursive writer
//! and `PartialEq` inherit that bound.

#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::todo,
    clippy::unimplemented,
    clippy::indexing_slicing
)]

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::ops::Index;

/// Deepest array/object nesting the parser accepts.
pub const MAX_DEPTH: usize = 128;

/// A JSON value. Numbers keep their source lexeme; objects keep insertion
/// order and (from the parser) carry no duplicate keys.
#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Num(String),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

/// The value `Index` hands back for a missing key or an out-of-range element,
/// mirroring `serde_json`'s "indexing a `Value` never panics" contract.
static NULL: Json = Json::Null;

// ---------------------------------------------------------------------------
// Accessors
// ---------------------------------------------------------------------------

impl Json {
    /// The string value, if this is a `Str`.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::Str(s) => Some(s),
            _ => None,
        }
    }

    /// The elements, if this is an `Arr`.
    pub fn as_arr(&self) -> Option<&[Json]> {
        match self {
            Json::Arr(v) => Some(v),
            _ => None,
        }
    }

    /// `as_arr` under `serde_json`'s name.
    pub fn as_array(&self) -> Option<&[Json]> {
        self.as_arr()
    }

    /// The key/value pairs in insertion order, if this is an `Obj`.
    pub fn as_obj(&self) -> Option<&[(String, Json)]> {
        match self {
            Json::Obj(o) => Some(o),
            _ => None,
        }
    }

    /// `as_obj` under `serde_json`'s name.
    pub fn as_object(&self) -> Option<&[(String, Json)]> {
        self.as_obj()
    }

    /// The boolean value, if this is a `Bool`.
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Json::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// The number as `i64`, if the lexeme is an integer that fits. A lexeme
    /// with a fraction or exponent is not an integer (`1.0` and `1e2` are
    /// `None`), matching `serde_json`'s f64-backed numbers.
    pub fn as_i64(&self) -> Option<i64> {
        self.integer_lexeme()?.parse::<i64>().ok()
    }

    /// The number as `u64`, if the lexeme is a non-negative integer that fits.
    pub fn as_u64(&self) -> Option<u64> {
        self.integer_lexeme()?.parse::<u64>().ok()
    }

    /// The number as `f64`, parsed from the lexeme. Every lexeme the parser
    /// accepted is a valid Rust float literal, so this is `None` only for a
    /// non-number.
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Json::Num(n) => n.parse::<f64>().ok(),
            _ => None,
        }
    }

    fn integer_lexeme(&self) -> Option<&str> {
        match self {
            Json::Num(n) if !n.contains('.') && !n.contains('e') && !n.contains('E') => Some(n),
            _ => None,
        }
    }

    /// True iff this is `Null`.
    pub fn is_null(&self) -> bool {
        matches!(self, Json::Null)
    }

    /// True iff this is exactly `Bool(true)`.
    pub fn is_true(&self) -> bool {
        matches!(self, Json::Bool(true))
    }

    /// The value for `key`, if this is an `Obj` carrying it.
    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Obj(o) => o.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    /// Mutable access to the value for `key`.
    pub fn get_mut(&mut self, key: &str) -> Option<&mut Json> {
        match self {
            Json::Obj(o) => o.iter_mut().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    /// Walk a chain of object keys: `v.get_path(&["a", "b"])`.
    pub fn get_path(&self, path: &[&str]) -> Option<&Json> {
        let mut cur = self;
        for key in path {
            cur = cur.get(key)?;
        }
        Some(cur)
    }

    /// The `i`th element, if this is an `Arr` long enough.
    // Deliberately shares a name with the `Index` impl below: that one is
    // total (missing -> `Null`), this one reports absence.
    #[allow(clippy::should_implement_trait)]
    pub fn index(&self, i: usize) -> Option<&Json> {
        match self {
            Json::Arr(a) => a.get(i),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Mutators
// ---------------------------------------------------------------------------

impl Json {
    /// Set `key`, replacing an existing entry in place (its position is kept,
    /// as `serde_json`'s `Map::insert` does) and returning the old value.
    /// A no-op returning `None` when this is not an `Obj`.
    pub fn insert(&mut self, key: impl Into<String>, value: impl Into<Json>) -> Option<Json> {
        match self {
            Json::Obj(o) => {
                let key = key.into();
                let value = value.into();
                match o.iter_mut().find(|(k, _)| *k == key) {
                    Some(slot) => Some(std::mem::replace(&mut slot.1, value)),
                    None => {
                        o.push((key, value));
                        None
                    }
                }
            }
            _ => None,
        }
    }

    /// Remove `key`, returning its value.
    pub fn remove(&mut self, key: &str) -> Option<Json> {
        match self {
            Json::Obj(o) => o
                .iter()
                .position(|(k, _)| k == key)
                .map(|pos| o.remove(pos).1),
            _ => None,
        }
    }

    /// Append to an `Arr`. Returns false (and does nothing) otherwise.
    pub fn push(&mut self, value: impl Into<Json>) -> bool {
        match self {
            Json::Arr(a) => {
                a.push(value.into());
                true
            }
            _ => false,
        }
    }
}

impl Index<&str> for Json {
    type Output = Json;
    /// Total: a missing key, or a non-object, reads as `Null`.
    fn index(&self, key: &str) -> &Json {
        self.get(key).unwrap_or(&NULL)
    }
}

impl Index<usize> for Json {
    type Output = Json;
    /// Total: an out-of-range element, or a non-array, reads as `Null`.
    fn index(&self, i: usize) -> &Json {
        Json::index(self, i).unwrap_or(&NULL)
    }
}

// ---------------------------------------------------------------------------
// Building objects
// ---------------------------------------------------------------------------

/// Accumulates object entries in insertion order, replacing a repeated key in
/// place. This is what `json!({…})` expands into.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ObjectBuilder {
    pairs: Vec<(String, Json)>,
}

impl ObjectBuilder {
    pub fn new() -> Self {
        ObjectBuilder { pairs: Vec::new() }
    }

    pub fn with_capacity(n: usize) -> Self {
        ObjectBuilder {
            pairs: Vec::with_capacity(n),
        }
    }

    /// Set `key`, returning the value it replaced.
    pub fn insert(&mut self, key: String, value: Json) -> Option<Json> {
        match self.pairs.iter_mut().find(|(k, _)| *k == key) {
            Some(slot) => Some(std::mem::replace(&mut slot.1, value)),
            None => {
                self.pairs.push((key, value));
                None
            }
        }
    }

    /// Chaining form of `insert`.
    pub fn set(mut self, key: impl Into<String>, value: impl Into<Json>) -> Self {
        self.insert(key.into(), value.into());
        self
    }

    pub fn into_pairs(self) -> Vec<(String, Json)> {
        self.pairs
    }

    pub fn build(self) -> Json {
        Json::Obj(self.pairs)
    }
}

// ---------------------------------------------------------------------------
// Conversion
// ---------------------------------------------------------------------------

/// Borrowed conversion into [`Json`]. `json!` uses this (through the blanket
/// `From<&T>` impl) so an expression written into a `json!` is borrowed, never
/// moved — the same property `serde_json`'s `to_value(&expr)` has. A caller
/// converts its own types by implementing this trait.
pub trait ToJson {
    fn to_json(&self) -> Json;
}

impl<T: ToJson + ?Sized> ToJson for &T {
    fn to_json(&self) -> Json {
        (**self).to_json()
    }
}

/// The bridge that lets `json!` accept any `ToJson` expression by reference.
impl<T: ToJson + ?Sized> From<&T> for Json {
    fn from(v: &T) -> Json {
        v.to_json()
    }
}

impl ToJson for Json {
    fn to_json(&self) -> Json {
        self.clone()
    }
}

impl ToJson for bool {
    fn to_json(&self) -> Json {
        Json::Bool(*self)
    }
}

impl From<bool> for Json {
    fn from(v: bool) -> Json {
        Json::Bool(v)
    }
}

impl ToJson for str {
    fn to_json(&self) -> Json {
        Json::Str(self.to_string())
    }
}

impl ToJson for String {
    fn to_json(&self) -> Json {
        Json::Str(self.clone())
    }
}

impl From<String> for Json {
    fn from(v: String) -> Json {
        Json::Str(v)
    }
}

impl ToJson for Cow<'_, str> {
    fn to_json(&self) -> Json {
        Json::Str(self.clone().into_owned())
    }
}

impl From<Cow<'_, str>> for Json {
    fn from(v: Cow<'_, str>) -> Json {
        Json::Str(v.into_owned())
    }
}

macro_rules! json_int_conv {
    ($($t:ty),* $(,)?) => {$(
        impl ToJson for $t {
            fn to_json(&self) -> Json {
                Json::Num(self.to_string())
            }
        }
        impl From<$t> for Json {
            fn from(v: $t) -> Json {
                Json::Num(v.to_string())
            }
        }
    )*};
}

json_int_conv!(i8, i16, i32, i64, isize, u8, u16, u32, u64, usize);

/// Shortest `Display` form, forced to look like a float so it re-parses as one.
fn float_lexeme(finite: bool, mut s: String) -> Json {
    if !finite {
        // serde_json maps NaN/±inf to null rather than emitting invalid JSON.
        return Json::Null;
    }
    if !s.contains('.') && !s.contains('e') {
        s.push_str(".0");
    }
    Json::Num(s)
}

macro_rules! json_float_conv {
    ($($t:ty),* $(,)?) => {$(
        impl ToJson for $t {
            fn to_json(&self) -> Json {
                float_lexeme(self.is_finite(), self.to_string())
            }
        }
        impl From<$t> for Json {
            fn from(v: $t) -> Json {
                float_lexeme(v.is_finite(), v.to_string())
            }
        }
    )*};
}

json_float_conv!(f32, f64);

impl<T: ToJson> ToJson for Option<T> {
    fn to_json(&self) -> Json {
        match self {
            Some(v) => v.to_json(),
            None => Json::Null,
        }
    }
}

impl<T: Into<Json>> From<Option<T>> for Json {
    fn from(v: Option<T>) -> Json {
        match v {
            Some(v) => v.into(),
            None => Json::Null,
        }
    }
}

impl<T: ToJson> ToJson for [T] {
    fn to_json(&self) -> Json {
        Json::Arr(self.iter().map(ToJson::to_json).collect())
    }
}

impl<T: ToJson, const N: usize> ToJson for [T; N] {
    fn to_json(&self) -> Json {
        Json::Arr(self.iter().map(ToJson::to_json).collect())
    }
}

impl<T: ToJson> ToJson for Vec<T> {
    fn to_json(&self) -> Json {
        Json::Arr(self.iter().map(ToJson::to_json).collect())
    }
}

impl<T: Into<Json>> From<Vec<T>> for Json {
    fn from(v: Vec<T>) -> Json {
        Json::Arr(v.into_iter().map(Into::into).collect())
    }
}

/// Object from ordered pairs. Repeated keys keep the first position and the
/// last value, as `ObjectBuilder` does.
impl From<Vec<(String, Json)>> for Json {
    fn from(pairs: Vec<(String, Json)>) -> Json {
        let mut b = ObjectBuilder::with_capacity(pairs.len());
        for (k, v) in pairs {
            b.insert(k, v);
        }
        b.build()
    }
}

impl<K: AsRef<str>, V: ToJson> ToJson for BTreeMap<K, V> {
    fn to_json(&self) -> Json {
        Json::Obj(
            self.iter()
                .map(|(k, v)| (k.as_ref().to_string(), v.to_json()))
                .collect(),
        )
    }
}

impl<K: Into<String>, V: Into<Json>> From<BTreeMap<K, V>> for Json {
    fn from(m: BTreeMap<K, V>) -> Json {
        Json::Obj(m.into_iter().map(|(k, v)| (k.into(), v.into())).collect())
    }
}

/// Hash order is not reproducible, so map entries are emitted key-sorted —
/// the same order a `BTreeMap` (and hence `serde_json`) would give.
impl<K: AsRef<str>, V: ToJson> ToJson for HashMap<K, V> {
    fn to_json(&self) -> Json {
        let mut pairs: Vec<(String, Json)> = self
            .iter()
            .map(|(k, v)| (k.as_ref().to_string(), v.to_json()))
            .collect();
        pairs.sort_by(|a, b| a.0.cmp(&b.0));
        Json::Obj(pairs)
    }
}

impl<K: Into<String>, V: Into<Json>> From<HashMap<K, V>> for Json {
    fn from(m: HashMap<K, V>) -> Json {
        let mut pairs: Vec<(String, Json)> =
            m.into_iter().map(|(k, v)| (k.into(), v.into())).collect();
        pairs.sort_by(|a, b| a.0.cmp(&b.0));
        Json::Obj(pairs)
    }
}

// ---------------------------------------------------------------------------
// Writing
// ---------------------------------------------------------------------------

impl fmt::Display for Json {
    /// Compact, object keys in insertion order. Never emits a newline, so a
    /// value is always one NDJSON frame.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.write_compact(f, false)
    }
}

impl Json {
    /// Compact form with object keys sorted ascending at every level: two
    /// values compare equal as canonical text iff they carry the same keys and
    /// values. This is also the byte-for-byte shape `serde_json` emits, whose
    /// default map type is a `BTreeMap`.
    pub fn to_canonical(&self) -> String {
        let mut out = String::new();
        // Writing into a String cannot fail; the Result is fmt::Write's.
        let _ = self.write_compact(&mut out, true);
        out
    }

    /// `to_string()` as bytes.
    pub fn to_vec(&self) -> Vec<u8> {
        self.to_string().into_bytes()
    }

    /// Two-space indented form, insertion order.
    pub fn to_string_pretty(&self) -> String {
        let mut out = String::new();
        let _ = self.write_pretty(&mut out, 0);
        out
    }

    fn write_compact<W: fmt::Write>(&self, out: &mut W, sort: bool) -> fmt::Result {
        match self {
            Json::Null => out.write_str("null"),
            Json::Bool(b) => out.write_str(if *b { "true" } else { "false" }),
            Json::Num(n) => out.write_str(n),
            Json::Str(s) => write_escaped(s, out),
            Json::Arr(a) => {
                out.write_char('[')?;
                for (i, v) in a.iter().enumerate() {
                    if i > 0 {
                        out.write_char(',')?;
                    }
                    v.write_compact(out, sort)?;
                }
                out.write_char(']')
            }
            Json::Obj(o) => {
                out.write_char('{')?;
                if sort {
                    let mut sorted: Vec<&(String, Json)> = o.iter().collect();
                    sorted.sort_by(|a, b| a.0.cmp(&b.0));
                    for (i, (k, v)) in sorted.iter().enumerate() {
                        if i > 0 {
                            out.write_char(',')?;
                        }
                        write_escaped(k, out)?;
                        out.write_char(':')?;
                        v.write_compact(out, sort)?;
                    }
                } else {
                    for (i, (k, v)) in o.iter().enumerate() {
                        if i > 0 {
                            out.write_char(',')?;
                        }
                        write_escaped(k, out)?;
                        out.write_char(':')?;
                        v.write_compact(out, sort)?;
                    }
                }
                out.write_char('}')
            }
        }
    }

    fn write_pretty<W: fmt::Write>(&self, out: &mut W, level: usize) -> fmt::Result {
        let inner = level.saturating_add(1);
        match self {
            Json::Arr(a) if !a.is_empty() => {
                out.write_str("[\n")?;
                for (i, v) in a.iter().enumerate() {
                    if i > 0 {
                        out.write_str(",\n")?;
                    }
                    write_indent(out, inner)?;
                    v.write_pretty(out, inner)?;
                }
                out.write_char('\n')?;
                write_indent(out, level)?;
                out.write_char(']')
            }
            Json::Obj(o) if !o.is_empty() => {
                out.write_str("{\n")?;
                for (i, (k, v)) in o.iter().enumerate() {
                    if i > 0 {
                        out.write_str(",\n")?;
                    }
                    write_indent(out, inner)?;
                    write_escaped(k, out)?;
                    out.write_str(": ")?;
                    v.write_pretty(out, inner)?;
                }
                out.write_char('\n')?;
                write_indent(out, level)?;
                out.write_char('}')
            }
            // Scalars and empty containers are identical in both forms.
            other => other.write_compact(out, false),
        }
    }
}

fn write_indent<W: fmt::Write>(out: &mut W, level: usize) -> fmt::Result {
    for _ in 0..level {
        out.write_str("  ")?;
    }
    Ok(())
}

/// Write `s` as a JSON string. Everything but `"`, `\` and the C0 controls
/// passes through as UTF-8; the run between two escapes is copied in one go so
/// the common no-escape string costs one `write_str`.
fn write_escaped<W: fmt::Write>(s: &str, out: &mut W) -> fmt::Result {
    out.write_char('"')?;
    let mut run = 0usize;
    for (idx, c) in s.char_indices() {
        let short = match c {
            '"' => "\\\"",
            '\\' => "\\\\",
            '\n' => "\\n",
            '\r' => "\\r",
            '\t' => "\\t",
            '\u{08}' => "\\b",
            '\u{0c}' => "\\f",
            // Remaining C0 controls have no short form; \u00XX below.
            c if (c as u32) < 0x20 => "",
            _ => continue,
        };
        if let Some(chunk) = s.get(run..idx) {
            out.write_str(chunk)?;
        }
        if short.is_empty() {
            let n = c as u32;
            out.write_str("\\u00")?;
            out.write_char(char::from_digit((n >> 4) & 0xf, 16).unwrap_or('0'))?;
            out.write_char(char::from_digit(n & 0xf, 16).unwrap_or('0'))?;
        } else {
            out.write_str(short)?;
        }
        run = idx.saturating_add(c.len_utf8());
    }
    if let Some(chunk) = s.get(run..) {
        out.write_str(chunk)?;
    }
    out.write_char('"')
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// Where and why a parse stopped. `offset` is a byte index into the input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub offset: usize,
    pub message: String,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} at byte {}", self.message, self.offset)
    }
}

impl std::error::Error for ParseError {}

/// Parse one JSON document. Leading and trailing whitespace is fine; trailing
/// content is not.
pub fn parse(input: &str) -> Result<Json, ParseError> {
    let mut p = Parser {
        b: input.as_bytes(),
        i: 0,
        depth: 0,
    };
    let v = p.value()?;
    p.skip_ws();
    if p.i != p.b.len() {
        return Err(p.err("trailing content"));
    }
    Ok(v)
}

/// `parse` over bytes, which must be UTF-8.
pub fn parse_slice(bytes: &[u8]) -> Result<Json, ParseError> {
    match std::str::from_utf8(bytes) {
        Ok(s) => parse(s),
        Err(e) => Err(ParseError {
            offset: e.valid_up_to(),
            message: "input is not valid UTF-8".to_string(),
        }),
    }
}

impl std::str::FromStr for Json {
    type Err = ParseError;
    fn from_str(s: &str) -> Result<Json, ParseError> {
        parse(s)
    }
}

struct Parser<'a> {
    b: &'a [u8],
    i: usize,
    depth: usize,
}

impl Parser<'_> {
    fn err(&self, message: &str) -> ParseError {
        ParseError {
            offset: self.i,
            message: message.to_string(),
        }
    }

    fn peek(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }

    fn bump(&mut self) {
        self.i = self.i.saturating_add(1);
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.bump();
        }
    }

    /// Charge one level of nesting; the cap keeps the recursion bounded.
    fn enter(&mut self) -> Result<(), ParseError> {
        self.depth = self.depth.saturating_add(1);
        if self.depth > MAX_DEPTH {
            return Err(self.err("maximum nesting depth exceeded"));
        }
        Ok(())
    }

    fn leave(&mut self) {
        self.depth = self.depth.saturating_sub(1);
    }

    fn value(&mut self) -> Result<Json, ParseError> {
        self.skip_ws();
        let c = match self.peek() {
            Some(c) => c,
            None => return Err(self.err("unexpected end of input")),
        };
        match c {
            b'{' => {
                self.enter()?;
                let v = self.object()?;
                self.leave();
                Ok(v)
            }
            b'[' => {
                self.enter()?;
                let v = self.array()?;
                self.leave();
                Ok(v)
            }
            b'"' => Ok(Json::Str(self.string()?)),
            b't' => {
                self.lit("true")?;
                Ok(Json::Bool(true))
            }
            b'f' => {
                self.lit("false")?;
                Ok(Json::Bool(false))
            }
            b'n' => {
                self.lit("null")?;
                Ok(Json::Null)
            }
            b'-' | b'0'..=b'9' => self.number(),
            c => Err(self.err(&format!("unexpected byte 0x{c:02x}"))),
        }
    }

    fn lit(&mut self, s: &str) -> Result<(), ParseError> {
        if self
            .b
            .get(self.i..)
            .is_some_and(|rest| rest.starts_with(s.as_bytes()))
        {
            self.i = self.i.saturating_add(s.len());
            Ok(())
        } else {
            Err(self.err(&format!("expected `{s}'")))
        }
    }

    fn number(&mut self) -> Result<Json, ParseError> {
        let start = self.i;
        if self.peek() == Some(b'-') {
            self.bump();
        }
        while let Some(c) = self.peek() {
            if c.is_ascii_digit() || matches!(c, b'.' | b'e' | b'E' | b'+' | b'-') {
                self.bump();
            } else {
                break;
            }
        }
        let lex = self
            .b
            .get(start..self.i)
            .and_then(|s| std::str::from_utf8(s).ok())
            .ok_or_else(|| self.err("malformed number"))?;
        // The lexeme is kept verbatim and re-emitted verbatim, so a malformed
        // one must not slip through and become invalid output later.
        if !valid_json_number(lex) {
            return Err(ParseError {
                offset: start,
                message: format!("malformed number `{lex}'"),
            });
        }
        Ok(Json::Num(lex.to_string()))
    }

    fn string(&mut self) -> Result<String, ParseError> {
        self.bump(); // opening quote
        let mut out: Vec<u8> = Vec::new();
        loop {
            let run_start = self.i;
            while let Some(c) = self.peek() {
                if c == b'"' || c == b'\\' || c < 0x20 {
                    break;
                }
                self.bump();
            }
            if let Some(run) = self.b.get(run_start..self.i) {
                out.extend_from_slice(run);
            }
            match self.peek() {
                None => return Err(self.err("unterminated string")),
                Some(b'"') => {
                    self.bump();
                    // Input was &str and every escape yields a valid scalar, so
                    // this cannot fail; it is checked rather than asserted.
                    return String::from_utf8(out).map_err(|_| self.err("invalid UTF-8 in string"));
                }
                // RFC 8259: C0 controls must be escaped inside a string.
                Some(c) if c < 0x20 => {
                    return Err(self.err(&format!("unescaped control byte 0x{c:02x} in string")))
                }
                Some(_) => {
                    self.bump(); // backslash
                    self.escape(&mut out)?;
                }
            }
        }
    }

    fn escape(&mut self, out: &mut Vec<u8>) -> Result<(), ParseError> {
        let e = match self.peek() {
            Some(e) => e,
            None => return Err(self.err("escape at end of input")),
        };
        self.bump();
        match e {
            b'"' => out.push(b'"'),
            b'\\' => out.push(b'\\'),
            b'/' => out.push(b'/'),
            b'b' => out.push(0x08),
            b'f' => out.push(0x0c),
            b'n' => out.push(b'\n'),
            b'r' => out.push(b'\r'),
            b't' => out.push(b'\t'),
            b'u' => {
                let mut cp = u32::from(self.hex4()?);
                if (0xD800..=0xDBFF).contains(&cp) {
                    // High surrogate: only a \uXXXX low surrogate completes it.
                    if self.peek() == Some(b'\\')
                        && self.b.get(self.i.saturating_add(1)).copied() == Some(b'u')
                    {
                        self.bump();
                        self.bump();
                        let lo = u32::from(self.hex4()?);
                        if !(0xDC00..=0xDFFF).contains(&lo) {
                            return Err(self.err("expected a low surrogate"));
                        }
                        cp = 0x10000u32
                            .saturating_add((cp.saturating_sub(0xD800)) << 10)
                            .saturating_add(lo.saturating_sub(0xDC00));
                    } else {
                        return Err(self.err("unpaired high surrogate"));
                    }
                }
                let ch = char::from_u32(cp).ok_or_else(|| self.err("invalid code point"))?;
                let mut buf = [0u8; 4];
                out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
            }
            c => return Err(self.err(&format!("invalid escape `\\{}'", c as char))),
        }
        Ok(())
    }

    fn hex4(&mut self) -> Result<u16, ParseError> {
        let mut v: u16 = 0;
        for _ in 0..4 {
            let d = match self.peek() {
                Some(d) => d,
                None => return Err(self.err("truncated \\u escape")),
            };
            let n = match d {
                b'0'..=b'9' => d - b'0',
                b'a'..=b'f' => d - b'a' + 10,
                b'A'..=b'F' => d - b'A' + 10,
                _ => return Err(self.err("bad hex digit in \\u escape")),
            };
            v = (v << 4) | u16::from(n);
            self.bump();
        }
        Ok(v)
    }

    fn array(&mut self) -> Result<Json, ParseError> {
        self.bump(); // [
        let mut a = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.bump();
            return Ok(Json::Arr(a));
        }
        loop {
            a.push(self.value()?);
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.bump(),
                Some(b']') => {
                    self.bump();
                    return Ok(Json::Arr(a));
                }
                Some(_) => return Err(self.err("expected `,' or `]' in array")),
                None => return Err(self.err("unterminated array")),
            }
        }
    }

    fn object(&mut self) -> Result<Json, ParseError> {
        self.bump(); // {
        let mut o: Vec<(String, Json)> = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.bump();
            return Ok(Json::Obj(o));
        }
        loop {
            self.skip_ws();
            if self.peek() != Some(b'"') {
                return Err(self.err("expected object key string"));
            }
            let key_at = self.i;
            let k = self.string()?;
            self.skip_ws();
            if self.peek() != Some(b':') {
                return Err(self.err("expected `:' after object key"));
            }
            self.bump();
            let v = self.value()?;
            // `Obj` is an ordered Vec whose `get` takes the first match, so a
            // duplicate key would silently pick a winner. Reject it instead.
            if o.iter().any(|(existing, _)| *existing == k) {
                return Err(ParseError {
                    offset: key_at,
                    message: format!("duplicate object key `{k}'"),
                });
            }
            o.push((k, v));
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.bump(),
                Some(b'}') => {
                    self.bump();
                    return Ok(Json::Obj(o));
                }
                Some(_) => return Err(self.err("expected `,' or `}' in object")),
                None => return Err(self.err("unterminated object")),
            }
        }
    }
}

/// True iff `lex` is a well-formed JSON number (RFC 8259):
/// `-?(0|[1-9][0-9]*)(\.[0-9]+)?([eE][+-]?[0-9]+)?`.
fn valid_json_number(lex: &str) -> bool {
    let mut it = lex.as_bytes().iter().copied().peekable();
    if it.peek() == Some(&b'-') {
        it.next();
    }
    // Integer part: a lone 0, or a digit run not starting with 0.
    match it.next() {
        Some(b'0') => {}
        Some(c) if c.is_ascii_digit() => {
            while it.peek().is_some_and(u8::is_ascii_digit) {
                it.next();
            }
        }
        _ => return false,
    }
    if it.peek() == Some(&b'.') {
        it.next();
        let mut digits = 0usize;
        while it.peek().is_some_and(u8::is_ascii_digit) {
            it.next();
            digits = digits.saturating_add(1);
        }
        if digits == 0 {
            return false;
        }
    }
    if matches!(it.peek(), Some(b'e' | b'E')) {
        it.next();
        if matches!(it.peek(), Some(b'+' | b'-')) {
            it.next();
        }
        let mut digits = 0usize;
        while it.peek().is_some_and(u8::is_ascii_digit) {
            it.next();
            digits = digits.saturating_add(1);
        }
        if digits == 0 {
            return false;
        }
    }
    it.next().is_none()
}

// ---------------------------------------------------------------------------
// json! macro
//
// A tt-muncher in the shape serde_json's `json!` uses, so the call sites in td
// applications port over unchanged: `null`/`true`/`false` literals, nested
// `[…]` and `{…}`, trailing commas, keys that are string literals or arbitrary
// expressions (bare identifiers included), and values that are either `json!`
// syntax or any Rust expression whose type implements `ToJson`.
//
// Every path is `$crate::…`, so the macro works the same whether it is used
// from another crate (`use tdstd::json;`) or from the crate that defines it
// (`crate::json`) — as long as the module stays at the crate root as `json`.
// ---------------------------------------------------------------------------

/// Build a [`Json`] from JSON-shaped Rust syntax.
///
/// ```
/// use tdstd::json;
/// let name = "ada";
/// let v = json!({ "ok": true, "user": { "name": name }, "tags": ["a", "b"] });
/// assert_eq!(v.to_string(), r#"{"ok":true,"user":{"name":"ada"},"tags":["a","b"]}"#);
/// ```
#[macro_export]
macro_rules! json {
    ($($json:tt)+) => {
        $crate::__json_internal!($($json)+)
    };
}

#[macro_export]
#[doc(hidden)]
macro_rules! __json_vec {
    ($($content:tt)*) => {
        ::std::vec![$($content)*]
    };
}

/// Invoked with a token the muncher did not expect; it takes no arguments, so
/// the token itself is what the error message points at.
#[macro_export]
#[doc(hidden)]
macro_rules! __json_unexpected {
    () => {};
}

#[macro_export]
#[doc(hidden)]
macro_rules! __json_internal {
    //////////////////////////////////////////////////////////////////////////
    // Array body: @array [accumulated elements] remaining tokens
    //////////////////////////////////////////////////////////////////////////

    (@array [$($elems:expr,)*]) => {
        $crate::__json_vec![$($elems,)*]
    };

    (@array [$($elems:expr),*]) => {
        $crate::__json_vec![$($elems),*]
    };

    (@array [$($elems:expr,)*] null $($rest:tt)*) => {
        $crate::__json_internal!(@array [$($elems,)* $crate::__json_internal!(null)] $($rest)*)
    };

    (@array [$($elems:expr,)*] true $($rest:tt)*) => {
        $crate::__json_internal!(@array [$($elems,)* $crate::__json_internal!(true)] $($rest)*)
    };

    (@array [$($elems:expr,)*] false $($rest:tt)*) => {
        $crate::__json_internal!(@array [$($elems,)* $crate::__json_internal!(false)] $($rest)*)
    };

    (@array [$($elems:expr,)*] [$($array:tt)*] $($rest:tt)*) => {
        $crate::__json_internal!(@array [$($elems,)* $crate::__json_internal!([$($array)*])] $($rest)*)
    };

    (@array [$($elems:expr,)*] {$($map:tt)*} $($rest:tt)*) => {
        $crate::__json_internal!(@array [$($elems,)* $crate::__json_internal!({$($map)*})] $($rest)*)
    };

    (@array [$($elems:expr,)*] $next:expr, $($rest:tt)*) => {
        $crate::__json_internal!(@array [$($elems,)* $crate::__json_internal!($next),] $($rest)*)
    };

    (@array [$($elems:expr,)*] $last:expr) => {
        $crate::__json_internal!(@array [$($elems,)* $crate::__json_internal!($last)])
    };

    (@array [$($elems:expr),*] , $($rest:tt)*) => {
        $crate::__json_internal!(@array [$($elems,)*] $($rest)*)
    };

    (@array [$($elems:expr),*] $unexpected:tt $($rest:tt)*) => {
        $crate::__json_unexpected!($unexpected)
    };

    //////////////////////////////////////////////////////////////////////////
    // Object body: @object builder (pending key) (remaining) (remaining copy)
    //
    // The third group is a byte-for-byte copy of the second, kept so an error
    // arm can name the offending token after the second has been consumed.
    //////////////////////////////////////////////////////////////////////////

    (@object $object:ident () () ()) => {};

    (@object $object:ident [$($key:tt)+] ($value:expr) , $($rest:tt)*) => {
        let _ = $object.insert(($($key)+).into(), $value);
        $crate::__json_internal!(@object $object () ($($rest)*) ($($rest)*));
    };

    (@object $object:ident [$($key:tt)+] ($value:expr) $unexpected:tt $($rest:tt)*) => {
        $crate::__json_unexpected!($unexpected);
    };

    (@object $object:ident [$($key:tt)+] ($value:expr)) => {
        let _ = $object.insert(($($key)+).into(), $value);
    };

    (@object $object:ident ($($key:tt)+) (: null $($rest:tt)*) $copy:tt) => {
        $crate::__json_internal!(@object $object [$($key)+] ($crate::__json_internal!(null)) $($rest)*);
    };

    (@object $object:ident ($($key:tt)+) (: true $($rest:tt)*) $copy:tt) => {
        $crate::__json_internal!(@object $object [$($key)+] ($crate::__json_internal!(true)) $($rest)*);
    };

    (@object $object:ident ($($key:tt)+) (: false $($rest:tt)*) $copy:tt) => {
        $crate::__json_internal!(@object $object [$($key)+] ($crate::__json_internal!(false)) $($rest)*);
    };

    (@object $object:ident ($($key:tt)+) (: [$($array:tt)*] $($rest:tt)*) $copy:tt) => {
        $crate::__json_internal!(@object $object [$($key)+] ($crate::__json_internal!([$($array)*])) $($rest)*);
    };

    (@object $object:ident ($($key:tt)+) (: {$($map:tt)*} $($rest:tt)*) $copy:tt) => {
        $crate::__json_internal!(@object $object [$($key)+] ($crate::__json_internal!({$($map)*})) $($rest)*);
    };

    (@object $object:ident ($($key:tt)+) (: $value:expr , $($rest:tt)*) $copy:tt) => {
        $crate::__json_internal!(@object $object [$($key)+] ($crate::__json_internal!($value)) , $($rest)*);
    };

    (@object $object:ident ($($key:tt)+) (: $value:expr) $copy:tt) => {
        $crate::__json_internal!(@object $object [$($key)+] ($crate::__json_internal!($value)));
    };

    // Key with a colon but no value.
    (@object $object:ident ($($key:tt)+) (:) $copy:tt) => {
        $crate::__json_internal!();
    };

    // Key with neither colon nor value.
    (@object $object:ident ($($key:tt)+) () $copy:tt) => {
        $crate::__json_internal!();
    };

    // Colon where a key should start.
    (@object $object:ident () (: $($rest:tt)*) ($colon:tt $($copy:tt)*)) => {
        $crate::__json_unexpected!($colon);
    };

    // Comma inside a key.
    (@object $object:ident ($($key:tt)*) (, $($rest:tt)*) ($comma:tt $($copy:tt)*)) => {
        $crate::__json_unexpected!($comma);
    };

    // Munch one more token into the pending key.
    (@object $object:ident ($($key:tt)*) ($tt:tt $($rest:tt)*) $copy:tt) => {
        $crate::__json_internal!(@object $object ($($key)* $tt) ($($rest)*) ($($rest)*));
    };

    //////////////////////////////////////////////////////////////////////////
    // Values
    //////////////////////////////////////////////////////////////////////////

    (null) => {
        $crate::json::Json::Null
    };

    (true) => {
        $crate::json::Json::Bool(true)
    };

    (false) => {
        $crate::json::Json::Bool(false)
    };

    ([]) => {
        $crate::json::Json::Arr(::std::vec::Vec::new())
    };

    ([ $($tt:tt)+ ]) => {
        $crate::json::Json::Arr($crate::__json_internal!(@array [] $($tt)+))
    };

    ({}) => {
        $crate::json::Json::Obj(::std::vec::Vec::new())
    };

    ({ $($tt:tt)+ }) => {
        {
            let mut object = $crate::json::ObjectBuilder::new();
            $crate::__json_internal!(@object object () ($($tt)+) ($($tt)+));
            object.build()
        }
    };

    // Any other expression, borrowed (never moved) through `ToJson`.
    ($other:expr) => {
        $crate::json::Json::from(&$other)
    };
}

// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;

    fn p(s: &str) -> Json {
        parse(s).unwrap()
    }

    // --- parser: shapes -----------------------------------------------------

    #[test]
    fn parses_scalars() {
        assert_eq!(p("null"), Json::Null);
        assert_eq!(p("true"), Json::Bool(true));
        assert_eq!(p("false"), Json::Bool(false));
        assert_eq!(p("42"), Json::Num("42".into()));
        assert_eq!(p(r#""hi""#), Json::Str("hi".into()));
    }

    #[test]
    fn parses_nested_containers() {
        let v = p(r#"{"a":[1,{"b":null}],"c":{}}"#);
        assert_eq!(
            v.get_path(&["a"]).and_then(Json::as_arr).map(<[_]>::len),
            Some(2)
        );
        assert!(v
            .get_path(&["a"])
            .unwrap()
            .index(1)
            .unwrap()
            .get("b")
            .unwrap()
            .is_null());
        assert_eq!(v.get("c"), Some(&Json::Obj(Vec::new())));
    }

    #[test]
    fn tolerates_whitespace_everywhere() {
        let v = p(" \t\r\n { \"a\" : [ 1 , 2 ] } \n ");
        assert_eq!(v.to_string(), r#"{"a":[1,2]}"#);
    }

    #[test]
    fn parses_empty_containers() {
        assert_eq!(p("[]"), Json::Arr(Vec::new()));
        assert_eq!(p("{}"), Json::Obj(Vec::new()));
        assert_eq!(p(r#"{"a":[],"b":{}}"#).to_string(), r#"{"a":[],"b":{}}"#);
    }

    #[test]
    fn preserves_object_insertion_order() {
        assert_eq!(p(r#"{"b":1,"a":2}"#).to_string(), r#"{"b":1,"a":2}"#);
    }

    // --- parser: strings ----------------------------------------------------

    #[test]
    fn parses_every_short_escape() {
        let v = p(r#""a\"b\\c\/d\be\ff\ng\rh\ti""#);
        assert_eq!(v.as_str(), Some("a\"b\\c/d\u{08}e\u{0c}f\ng\rh\ti"));
    }

    #[test]
    fn parses_unicode_escapes_and_surrogate_pairs() {
        assert_eq!(p(r#""\u0041""#).as_str(), Some("A"));
        assert_eq!(p(r#""\u00e9""#).as_str(), Some("\u{e9}"));
        assert_eq!(p(r#""\u4e2d\u6587""#).as_str(), Some("\u{4e2d}\u{6587}"));
        // U+1F600 as a surrogate pair, and the same in upper-case hex.
        assert_eq!(p(r#""\ud83d\ude00""#).as_str(), Some("\u{1f600}"));
        assert_eq!(p(r#""\uD83D\uDE00""#).as_str(), Some("\u{1f600}"));
        assert_eq!(p(r#""\u0000""#).as_str(), Some("\u{0}"));
        // An escaped ASCII char is the same value as the char itself.
        assert_eq!(p(r#""\u0061\u002f""#).as_str(), Some("a/"));
    }

    #[test]
    fn rejects_lone_and_bad_surrogates() {
        assert!(parse(r#""\ud83d""#).is_err());
        assert!(parse(r#""\ud83dx""#).is_err());
        assert!(parse(r#""\ud83dA""#).is_err());
        // A lone low surrogate is not a valid code point.
        assert!(parse(r#""\ude00""#).is_err());
    }

    #[test]
    fn rejects_bad_escapes_and_raw_controls() {
        assert!(parse(r#""\x""#).is_err());
        assert!(parse(r#""\u12""#).is_err());
        assert!(parse(r#""\u12g4""#).is_err());
        assert!(parse("\"raw\nnewline\"").is_err());
        assert!(parse("\"raw\ttab\"").is_err());
        assert!(parse(r#""unterminated"#).is_err());
        assert!(parse(r#""trailing escape\"#).is_err());
    }

    #[test]
    fn passes_utf8_through_verbatim() {
        let v = p("\"héllo 中文 😀\"");
        assert_eq!(v.as_str(), Some("héllo 中文 😀"));
        assert_eq!(v.to_string(), "\"héllo 中文 😀\"");
    }

    // --- parser: numbers ----------------------------------------------------

    #[test]
    fn accepts_well_formed_numbers_verbatim() {
        for s in [
            "0",
            "-0",
            "1",
            "-1",
            "42",
            "1.5",
            "-3.14",
            "1e10",
            "1E10",
            "1e+10",
            "1e-10",
            "0.5",
            "-2.5e-3",
            "123456789",
            "18446744073709551615",
            "-9223372036854775808",
            "1e308",
            "-0.0",
        ] {
            let v = parse(s).unwrap_or_else(|e| panic!("{s} should parse: {e}"));
            assert_eq!(v.to_string(), s, "lexeme must round-trip verbatim");
        }
    }

    #[test]
    fn rejects_malformed_numbers() {
        for s in [
            "-", "1e", "1E+", "1..2", "1+2", "01", "-01", ".5", "1.", "1e1.0", "--1", "0x1",
            "1e--1", "+1", "Infinity", "NaN",
        ] {
            assert!(parse(s).is_err(), "{s} is not a valid JSON number");
        }
    }

    // --- parser: errors -----------------------------------------------------

    #[test]
    fn rejects_trailing_content() {
        assert!(parse("{}x").is_err());
        assert!(parse(r#"{"k":1} junk"#).is_err());
        assert!(parse("[1,2] [3]").is_err());
    }

    #[test]
    fn rejects_truncated_input() {
        for s in [
            "",
            "  ",
            "{",
            "[",
            r#"{"k""#,
            r#"{"k":"#,
            r#"{"k":1"#,
            "[1,",
            "tru",
            "nul",
        ] {
            assert!(parse(s).is_err(), "{s:?} is truncated");
        }
    }

    #[test]
    fn rejects_duplicate_object_keys() {
        assert!(parse(r#"{"a":1,"a":2}"#).is_err());
        assert!(parse(r#"{"a":{"b":1,"b":2}}"#).is_err());
        assert!(parse(r#"{"a":1,"b":2,"a":3}"#).is_err());
        assert!(parse(r#"{"a":1,"b":2}"#).is_ok());
    }

    #[test]
    fn error_carries_byte_offset() {
        let e = parse(r#"{"a":1} x"#).unwrap_err();
        assert_eq!(e.offset, 8);
        assert!(e.to_string().ends_with("at byte 8"), "{e}");

        let e = parse(r#"[1, @]"#).unwrap_err();
        assert_eq!(e.offset, 4);

        let e = parse(r#"{"a":1,"a":2}"#).unwrap_err();
        assert_eq!(e.offset, 7, "offset points at the duplicate key");

        let e = parse("[01]").unwrap_err();
        assert_eq!(e.offset, 1, "offset points at the start of the number");
    }

    #[test]
    fn parse_slice_validates_utf8() {
        assert_eq!(
            parse_slice(br#"{"a":1}"#).unwrap().to_string(),
            r#"{"a":1}"#
        );
        let bad = [b'"', 0xff, 0xfe, b'"'];
        let e = parse_slice(&bad).unwrap_err();
        assert_eq!(e.offset, 1);
        assert!(e.message.contains("UTF-8"));
    }

    #[test]
    fn from_str_impl_parses() {
        let v: Json = "[1,2]".parse().unwrap();
        assert_eq!(v, p("[1,2]"));
    }

    // --- parser: depth ------------------------------------------------------

    fn nest(depth: usize) -> String {
        let mut s = String::new();
        for _ in 0..depth {
            s.push('[');
        }
        for _ in 0..depth {
            s.push(']');
        }
        s
    }

    #[test]
    fn accepts_nesting_at_the_limit_and_rejects_beyond() {
        assert!(parse(&nest(MAX_DEPTH)).is_ok());
        let e = parse(&nest(MAX_DEPTH + 1)).unwrap_err();
        assert!(e.message.contains("depth"), "{e}");
        // Objects are charged the same way.
        let mut deep = String::new();
        for _ in 0..(MAX_DEPTH + 1) {
            deep.push_str(r#"{"a":"#);
        }
        deep.push('1');
        for _ in 0..(MAX_DEPTH + 1) {
            deep.push('}');
        }
        assert!(parse(&deep).is_err());
    }

    #[test]
    fn deep_nesting_round_trips_at_the_limit() {
        let text = nest(MAX_DEPTH);
        let v = p(&text);
        assert_eq!(v.to_string(), text);
        assert_eq!(p(&v.to_string()), v);
    }

    // --- writer -------------------------------------------------------------

    #[test]
    fn to_string_is_compact_and_ordered() {
        let v = json!({"b": 1, "a": [true, "x"], "c": {"d": null}});
        assert_eq!(v.to_string(), r#"{"b":1,"a":[true,"x"],"c":{"d":null}}"#);
    }

    #[test]
    fn to_canonical_sorts_keys_at_every_level() {
        let v = p(r#"{"b":1,"a":{"z":1,"y":2}}"#);
        assert_eq!(v.to_canonical(), r#"{"a":{"y":2,"z":1},"b":1}"#);
        let a = p(r#"{"name":"hello","version":"1"}"#);
        let b = p(r#"{"version":"1","name":"hello"}"#);
        assert_eq!(a.to_canonical(), b.to_canonical());
        assert_ne!(a.to_string(), b.to_string());
    }

    #[test]
    fn escapes_exactly() {
        let s = Json::Str("q\"b\\s\nn\rr\tt\u{08}b\u{0c}f\u{01}c\u{1f}z/ok".to_string());
        assert_eq!(
            s.to_string(),
            r#""q\"b\\s\nn\rr\tt\bb\ff\u0001c\u001fz/ok""#
        );
        // Keys take the same path.
        let v = Json::Obj(vec![("a\nb".to_string(), Json::Null)]);
        assert_eq!(v.to_string(), r#"{"a\nb":null}"#);
    }

    #[test]
    fn escaped_output_round_trips() {
        let s = Json::Str("\u{0}\u{1f}\"\\/\n\t中😀".to_string());
        assert_eq!(p(&s.to_string()), s);
    }

    #[test]
    fn to_string_never_emits_a_newline() {
        let v = json!({
            "body": "line one\nline two\r\n\ttabbed",
            "list": ["a\nb", {"c": "d\ne"}],
        });
        let text = v.to_string();
        assert!(!text.contains('\n'), "{text}");
        assert!(!text.contains('\r'), "{text}");
        // ...and the frame parses back to the same value.
        assert_eq!(p(&text), v);
    }

    #[test]
    fn to_vec_matches_to_string() {
        let v = json!({"a": [1, 2]});
        assert_eq!(v.to_vec(), v.to_string().into_bytes());
    }

    #[test]
    fn pretty_uses_two_space_indent() {
        let v = json!({"a": [1, {"b": null}], "c": {}, "d": []});
        assert_eq!(
            v.to_string_pretty(),
            "{\n  \"a\": [\n    1,\n    {\n      \"b\": null\n    }\n  ],\n  \"c\": {},\n  \"d\": []\n}"
        );
        assert_eq!(p(&v.to_string_pretty()), v);
    }

    // --- numbers on the way out --------------------------------------------

    #[test]
    fn floats_are_written_so_they_re_parse_as_floats() {
        assert_eq!(Json::from(1.0f64).to_string(), "1.0");
        assert_eq!(Json::from(-0.0f64).to_string(), "-0.0");
        assert_eq!(Json::from(0.5f64).to_string(), "0.5");
        assert_eq!(Json::from(0.1f32).to_string(), "0.1");
        assert_eq!(Json::from(1e21f64).to_string(), "1000000000000000000000.0");
        assert_eq!(
            Json::from(1e21f64).as_f64(),
            Some(1e21f64),
            "different bytes from serde_json, same value"
        );
        assert_eq!(Json::from(f64::NAN), Json::Null);
        assert_eq!(Json::from(f64::INFINITY), Json::Null);
        assert_eq!(Json::from(f32::NEG_INFINITY), Json::Null);
    }

    #[test]
    fn integers_keep_full_range() {
        assert_eq!(Json::from(u64::MAX).to_string(), "18446744073709551615");
        assert_eq!(Json::from(i64::MIN).to_string(), "-9223372036854775808");
        assert_eq!(p("18446744073709551615").as_u64(), Some(u64::MAX));
        assert_eq!(p("-9223372036854775808").as_i64(), Some(i64::MIN));
    }

    // --- accessors ----------------------------------------------------------

    #[test]
    fn accessors_report_the_right_type() {
        let v = p(r#"{"s":"x","n":7,"f":1.5,"b":true,"z":null,"a":[1],"o":{"k":1}}"#);
        assert_eq!(v.get("s").and_then(Json::as_str), Some("x"));
        assert_eq!(v.get("n").and_then(Json::as_i64), Some(7));
        assert_eq!(v.get("n").and_then(Json::as_u64), Some(7));
        assert_eq!(v.get("n").and_then(Json::as_f64), Some(7.0));
        assert_eq!(v.get("f").and_then(Json::as_f64), Some(1.5));
        assert_eq!(
            v.get("f").and_then(Json::as_i64),
            None,
            "1.5 is not integral"
        );
        assert_eq!(v.get("b").and_then(Json::as_bool), Some(true));
        assert!(v.get("b").unwrap().is_true());
        assert!(v.get("z").unwrap().is_null());
        assert_eq!(v.get("a").and_then(Json::as_array).map(<[_]>::len), Some(1));
        assert_eq!(
            v.get("o").and_then(Json::as_object).map(<[_]>::len),
            Some(1)
        );
        assert_eq!(v.get("missing"), None);
        assert_eq!(v.get("s").and_then(Json::as_arr), None);
        assert_eq!(v.as_str(), None);
        // Exponent and fraction lexemes are floats, as in serde_json.
        assert_eq!(p("1e2").as_u64(), None);
        assert_eq!(p("1e2").as_f64(), Some(100.0));
        assert_eq!(p("-1").as_u64(), None);
    }

    #[test]
    fn get_path_and_index_walk_structures() {
        let v = p(r#"{"a":{"b":{"c":[10,20]}}}"#);
        assert_eq!(
            v.get_path(&["a", "b", "c"])
                .and_then(Json::as_arr)
                .map(<[_]>::len),
            Some(2)
        );
        assert_eq!(
            v.get_path(&["a", "b", "c"])
                .unwrap()
                .index(1)
                .and_then(Json::as_i64),
            Some(20)
        );
        assert_eq!(v.get_path(&["a", "nope", "c"]), None);
        assert_eq!(v.get_path(&["a", "b", "c"]).unwrap().index(9), None);
    }

    #[test]
    fn indexing_is_total() {
        let v = p(r#"{"a":[1]}"#);
        assert_eq!(v["a"][0].as_i64(), Some(1));
        assert!(v["nope"].is_null());
        assert!(v["a"][7].is_null());
        assert!(v["a"]["nope"].is_null(), "wrong container reads as null");
    }

    // --- mutators -----------------------------------------------------------

    #[test]
    fn insert_replaces_in_place_and_reports_the_old_value() {
        let mut v = json!({"a": 1, "b": 2});
        assert_eq!(v.insert("c", 3), None);
        assert_eq!(v.insert("a", "one"), Some(Json::Num("1".into())));
        assert_eq!(v.to_string(), r#"{"a":"one","b":2,"c":3}"#);
        // Not an object: a no-op.
        let mut arr = json!([1]);
        assert_eq!(arr.insert("a", 1), None);
        assert_eq!(arr.to_string(), "[1]");
    }

    #[test]
    fn remove_and_push_and_get_mut() {
        let mut v = json!({"a": 1, "b": [1]});
        assert_eq!(v.remove("a"), Some(Json::Num("1".into())));
        assert_eq!(v.remove("a"), None);
        let list = v.get_mut("b").unwrap();
        assert!(list.push(2));
        assert!(list.push("three"));
        assert!(!v.push(1), "an object cannot be pushed to");
        assert_eq!(v.to_string(), r#"{"b":[1,2,"three"]}"#);
    }

    #[test]
    fn object_builder_keeps_insertion_order() {
        let v = ObjectBuilder::new()
            .set("z", 1)
            .set("a", "two")
            .set("z", true)
            .build();
        assert_eq!(v.to_string(), r#"{"z":true,"a":"two"}"#);
        assert_eq!(ObjectBuilder::default().build(), Json::Obj(Vec::new()));
    }

    // --- conversion ---------------------------------------------------------

    #[test]
    fn from_impls_cover_the_scalar_types() {
        assert_eq!(Json::from(true), Json::Bool(true));
        assert_eq!(Json::from("s"), Json::Str("s".into()));
        assert_eq!(Json::from(String::from("s")), Json::Str("s".into()));
        assert_eq!(Json::from(&String::from("s")), Json::Str("s".into()));
        assert_eq!(Json::from(Cow::Borrowed("s")), Json::Str("s".into()));
        assert_eq!(Json::from(-1i8).to_string(), "-1");
        assert_eq!(Json::from(-1i16).to_string(), "-1");
        assert_eq!(Json::from(-1i32).to_string(), "-1");
        assert_eq!(Json::from(-1i64).to_string(), "-1");
        assert_eq!(Json::from(-1isize).to_string(), "-1");
        assert_eq!(Json::from(1u8).to_string(), "1");
        assert_eq!(Json::from(1u16).to_string(), "1");
        assert_eq!(Json::from(1u32).to_string(), "1");
        assert_eq!(Json::from(1u64).to_string(), "1");
        assert_eq!(Json::from(1usize).to_string(), "1");
        // Identity, owned (through core's reflexive `From`) and borrowed.
        fn convert(v: impl Into<Json>) -> Json {
            v.into()
        }
        let v = json!({"a": 1});
        assert_eq!(convert(v.clone()), v);
        assert_eq!(Json::from(&v), v);
    }

    #[test]
    fn from_impls_cover_containers() {
        assert_eq!(Json::from(Some(1u8)).to_string(), "1");
        assert_eq!(Json::from(None::<u8>), Json::Null);
        assert_eq!(Json::from(vec!["a", "b"]).to_string(), r#"["a","b"]"#);
        assert_eq!(
            Json::from(vec![json!(1), json!(null)]).to_string(),
            "[1,null]"
        );
        let slice: &[u32] = &[1, 2];
        assert_eq!(Json::from(slice).to_string(), "[1,2]");
        assert_eq!(Json::from(&[1u32, 2][..]).to_string(), "[1,2]");
        let pairs = vec![("b".to_string(), json!(1)), ("a".to_string(), json!(2))];
        assert_eq!(Json::from(pairs).to_string(), r#"{"b":1,"a":2}"#);

        let mut bt: BTreeMap<String, Json> = BTreeMap::new();
        bt.insert("b".into(), json!(1));
        bt.insert("a".into(), json!(2));
        assert_eq!(Json::from(bt.clone()).to_string(), r#"{"a":2,"b":1}"#);
        assert_eq!(Json::from(&bt).to_string(), r#"{"a":2,"b":1}"#);

        let mut hm: HashMap<String, u32> = HashMap::new();
        hm.insert("b".into(), 1);
        hm.insert("a".into(), 2);
        assert_eq!(Json::from(hm.clone()).to_string(), r#"{"a":2,"b":1}"#);
        assert_eq!(Json::from(&hm).to_string(), r#"{"a":2,"b":1}"#);
    }

    #[test]
    fn caller_types_convert_by_implementing_to_json() {
        struct Addr {
            name: Option<String>,
            email: String,
        }
        impl ToJson for Addr {
            fn to_json(&self) -> Json {
                json!({"name": self.name, "email": self.email})
            }
        }
        let a = Addr {
            name: None,
            email: "a@b".into(),
        };
        assert_eq!(
            json!({"from": [a], "one": a}).to_string(),
            r#"{"from":[{"name":null,"email":"a@b"}],"one":{"name":null,"email":"a@b"}}"#
        );
    }

    // --- json! macro --------------------------------------------------------

    #[test]
    fn macro_builds_scalars() {
        assert_eq!(json!(null), Json::Null);
        assert_eq!(json!(true), Json::Bool(true));
        assert_eq!(json!(false), Json::Bool(false));
        assert_eq!(json!(1), Json::Num("1".into()));
        assert_eq!(json!(-2.5), Json::Num("-2.5".into()));
        assert_eq!(json!("s"), Json::Str("s".into()));
        assert_eq!(json!([]), Json::Arr(Vec::new()));
        assert_eq!(json!({}), Json::Obj(Vec::new()));
    }

    #[test]
    fn macro_builds_arrays_with_mixed_and_nested_elements() {
        let n = 7u32;
        let v = json!([null, true, false, 1, "s", n, [1, [2]], {"a": {"b": []}},]);
        assert_eq!(
            v.to_string(),
            r#"[null,true,false,1,"s",7,[1,[2]],{"a":{"b":[]}}]"#
        );
    }

    #[test]
    fn macro_accepts_trailing_commas_and_nesting() {
        let v = json!({
            "a": [1, 2,],
            "b": {"c": {"d": [{"e": 1,},],},},
        });
        assert_eq!(v.to_string(), r#"{"a":[1,2],"b":{"c":{"d":[{"e":1}]}}}"#);
    }

    #[test]
    fn macro_accepts_expression_keys() {
        let id = "eml-1";
        let owned = String::from("k");
        let v = json!({
            "literal": 1,
            id: 2,
            owned.as_str(): 3,
            (format!("f{}", 4)): 4,
        });
        assert_eq!(v.to_string(), r#"{"literal":1,"eml-1":2,"k":3,"f4":4}"#);
    }

    #[test]
    fn macro_borrows_rather_than_moves_its_values() {
        let name = String::from("ada");
        let list = vec!["a".to_string(), "b".to_string()];
        let opt: Option<u32> = Some(3);
        let v = json!({"name": name, "list": list, "opt": opt, "none": None::<u32>});
        // Every input is still usable: the macro converts by reference.
        assert_eq!(name, "ada");
        assert_eq!(list.len(), 2);
        assert_eq!(opt, Some(3));
        assert_eq!(
            v.to_string(),
            r#"{"name":"ada","list":["a","b"],"opt":3,"none":null}"#
        );
    }

    #[test]
    fn macro_takes_arbitrary_value_expressions() {
        let flag = true;
        let count = 3usize;
        let path = std::path::PathBuf::from("/tmp/x");
        let v = json!({
            "msg": format!("{} of {}", 1, count),
            "branch": if flag { "spam" } else { "ham" },
            "path": path.to_string_lossy(),
            "call": count.saturating_sub(1),
            "nested_json": json!({"inner": 1}),
            "cast": count as u64,
        });
        assert_eq!(
            v.to_string(),
            r#"{"msg":"1 of 3","branch":"spam","path":"/tmp/x","call":2,"nested_json":{"inner":1},"cast":3}"#
        );
    }

    #[test]
    fn macro_repeats_are_last_value_first_position() {
        assert_eq!(
            json!({"a": 1, "b": 2, "a": 3}).to_string(),
            r#"{"a":3,"b":2}"#
        );
    }

    // Shapes taken from tmc's src/jmap/client.rs and src/cli.rs, and tn's
    // src/cli.rs: these must compile and evaluate exactly as they do under
    // serde_json's json!.
    #[test]
    fn macro_matches_the_jmap_client_call_sites() {
        struct Client {
            account_id: String,
        }
        let this = Client {
            account_id: "acc".into(),
        };
        let id = "eml-1";
        let ids: Vec<String> = vec!["a".into(), "b".into()];
        let name = "Archive";
        let mailbox_id = "mb-1";
        let limit = 50u32;
        let position = 0usize;
        let properties = ["id", "subject"];

        assert_eq!(
            json!({"accountId": this.account_id, "ids": null}).to_string(),
            r#"{"accountId":"acc","ids":null}"#
        );
        assert_eq!(
            json!({
                "accountId": this.account_id,
                "create": {"newMailbox": {"name": name}}
            })
            .to_string(),
            r#"{"accountId":"acc","create":{"newMailbox":{"name":"Archive"}}}"#
        );
        assert_eq!(
            json!({"accountId": this.account_id, "destroy": [id]}).to_string(),
            r#"{"accountId":"acc","destroy":["eml-1"]}"#
        );

        let mut conditions = vec![json!({"inMailbox": mailbox_id})];
        conditions.push(json!({"text": "hi"}));
        let filter = json!({"operator": "AND", "conditions": conditions});
        assert_eq!(
            json!({
                "accountId": this.account_id,
                "filter": filter,
                "sort": [{"property": "receivedAt", "isAscending": false}],
                "collapseThreads": false,
                "limit": limit,
                "position": position
            })
            .to_string(),
            concat!(
                r#"{"accountId":"acc","filter":{"operator":"AND","conditions":"#,
                r#"[{"inMailbox":"mb-1"},{"text":"hi"}]},"sort":[{"property":"#,
                r#""receivedAt","isAscending":false}],"collapseThreads":false,"#,
                r#""limit":50,"position":0}"#
            )
        );
        assert_eq!(
            json!({
                "accountId": this.account_id,
                "ids": ids,
                "properties": properties,
                "fetchTextBodyValues": true
            })
            .to_string(),
            r#"{"accountId":"acc","ids":["a","b"],"properties":["id","subject"],"fetchTextBodyValues":true}"#
        );
        // Identifier key naming a runtime value, with an object value.
        assert_eq!(
            json!({
                "accountId": this.account_id,
                "update": {id: {"keywords/$seen": true}}
            })
            .to_string(),
            r#"{"accountId":"acc","update":{"eml-1":{"keywords/$seen":true}}}"#
        );
        // The `update` map built entry by entry, as `mark_emails_read` does.
        let mut update = ObjectBuilder::new();
        for i in &ids {
            update.insert(i.clone(), json!({"keywords/$seen": true}));
        }
        assert_eq!(
            json!({"accountId": this.account_id, "update": update.build()}).to_string(),
            r#"{"accountId":"acc","update":{"a":{"keywords/$seen":true},"b":{"keywords/$seen":true}}}"#
        );
        // move_email: two levels of identifier key, the inner one with a
        // literal `true` value.
        let to_mailbox_id = "mb-2";
        assert_eq!(
            json!({
                "accountId": this.account_id,
                "update": {id: {"mailboxIds": {to_mailbox_id: true}}}
            })
            .to_string(),
            r#"{"accountId":"acc","update":{"eml-1":{"mailboxIds":{"mb-2":true}}}}"#
        );
        // set_flagged: an identifier key whose value is a variable holding a
        // whole object, chosen by an if/else.
        let flagged = false;
        let update_val = if flagged {
            json!({"keywords/$flagged": true})
        } else {
            json!({"keywords/$flagged": null})
        };
        assert_eq!(
            json!({"accountId": this.account_id, "update": {id: update_val}}).to_string(),
            r#"{"accountId":"acc","update":{"eml-1":{"keywords/$flagged":null}}}"#
        );
        assert_eq!(update_val.get("keywords/$flagged"), Some(&Json::Null));
    }

    #[test]
    fn macro_matches_the_tmc_cli_call_sites() {
        struct Addr {
            name: Option<String>,
            email: String,
        }
        struct Email {
            id: String,
            from: Option<Vec<Addr>>,
            subject: Option<String>,
            preview: String,
            mailbox_ids: BTreeMap<String, bool>,
        }
        let email = Email {
            id: "e1".into(),
            from: Some(vec![Addr {
                name: Some("Ada".into()),
                email: "a@b".into(),
            }]),
            subject: None,
            preview: "hi".into(),
            mailbox_ids: BTreeMap::from([("mb-1".to_string(), true)]),
        };
        let from = email.from.as_ref().map(|addrs| {
            addrs
                .iter()
                .map(|a| json!({"name": a.name, "email": a.email}))
                .collect::<Vec<_>>()
        });
        let is_read = true;
        let mut obj = json!({
            "id": email.id,
            "from": from,
            "subject": email.subject,
            "is_read": is_read,
            "mailbox_ids": email.mailbox_ids.keys().collect::<Vec<_>>(),
        });
        // serde_json's `obj["preview"] = json!(v)` becomes `insert`.
        obj.insert("preview", json!(email.preview));
        assert_eq!(
            obj.to_string(),
            concat!(
                r#"{"id":"e1","from":[{"name":"Ada","email":"a@b"}],"subject":null,"#,
                r#""is_read":true,"mailbox_ids":["mb-1"],"preview":"hi"}"#
            )
        );

        // mutate_many_move: an array of per-id result objects, then a summary.
        let ids = ["a".to_string(), "b".to_string()];
        let mut results = Vec::with_capacity(ids.len());
        for id in &ids {
            results.push(json!({"id": id, "ok": false, "error": "nope"}));
        }
        let success = results
            .iter()
            .filter(|r| r.get("ok").and_then(Json::as_bool).unwrap_or(false))
            .count();
        let v = json!({
            "target_mailbox_id": "mb-2",
            "attempted": ids.len(),
            "succeeded": success,
            "failed": ids.len().saturating_sub(success),
            "results": results
        });
        assert_eq!(
            v.to_string(),
            concat!(
                r#"{"target_mailbox_id":"mb-2","attempted":2,"succeeded":0,"failed":2,"#,
                r#""results":[{"id":"a","ok":false,"error":"nope"},"#,
                r#"{"id":"b","ok":false,"error":"nope"}]}"#
            )
        );

        // ok_response: an object built from another object plus a flag.
        let mut resp = v;
        resp.insert("ok", true);
        assert_eq!(resp.get("ok").and_then(Json::as_bool), Some(true));

        // classify: f64 score and a borrowed &'static str verdict.
        let score = 0.5f64;
        let verdict = "spam";
        let confidence = 0.25f32;
        assert_eq!(
            json!({"score": score, "verdict": verdict, "confidence": confidence}).to_string(),
            r#"{"score":0.5,"verdict":"spam","confidence":0.25}"#
        );
    }

    #[test]
    fn macro_matches_the_tn_cli_call_sites() {
        struct Meta {
            url: String,
            last_fetched: u64,
        }
        let all_len = 2usize;
        let unread_count = 1usize;
        let meta = Some(Meta {
            url: "https://f".into(),
            last_fetched: 99,
        });
        let name = "Feed";
        let url = "https://f".to_string();
        let mut folders = Vec::new();
        folders.push(json!({
            "id": "all",
            "name": "All",
            "total": all_len,
            "unread": unread_count,
            "virtual": true,
        }));
        folders.push(json!({
            "id": url,
            "name": name,
            "url": meta.as_ref().map(|m| m.url.clone()),
            "total": all_len,
            "unread": unread_count,
            "last_fetched": meta.map(|m| m.last_fetched),
            "virtual": false,
        }));
        let hash = "abc";
        let error = json!({
            "ok": false,
            "error": format!("article not found: {}", hash),
        });
        let v = json!({
            "ok": true,
            "folders": folders,
            "err": error,
            "quit": true,
        });
        assert_eq!(
            v.to_string(),
            concat!(
                r#"{"ok":true,"folders":[{"id":"all","name":"All","total":2,"unread":1,"virtual":true},"#,
                r#"{"id":"https://f","name":"Feed","url":"https://f","total":2,"unread":1,"#,
                r#""last_fetched":99,"virtual":false}],"#,
                r#""err":{"ok":false,"error":"article not found: abc"},"quit":true}"#
            )
        );
        // The NDJSON loop's exit test.
        assert_eq!(v.get("quit").and_then(Json::as_bool), Some(true));
    }

    // --- round trips --------------------------------------------------------

    #[test]
    fn corpus_round_trips_through_text() {
        let corpus = [
            "null",
            "true",
            "false",
            "0",
            "-0",
            "-0.0",
            "1e308",
            "-1.5e-7",
            "18446744073709551615",
            "-9223372036854775808",
            r#""""#,
            r#""plain""#,
            r#""\u0000\u001f\"\\""#,
            "\"héllo 中文 😀\"",
            "[]",
            "{}",
            "[[[]]]",
            r#"{"a":{"b":{"c":[]}}}"#,
            r#"[1,"two",null,true,{"k":[1,2]}]"#,
            r#"{"z":1,"a":2,"m":{"y":[{"x":null}]}}"#,
        ];
        for text in corpus {
            let v = p(text);
            assert_eq!(v.to_string(), text, "{text} must re-emit verbatim");
            assert_eq!(p(&v.to_string()), v, "{text} must re-parse equal");
            assert_eq!(p(&v.to_canonical()), p(&v.to_canonical()));
            assert_eq!(p(&v.to_string_pretty()), v, "{text} pretty must re-parse");
            assert_eq!(parse_slice(&v.to_vec()).unwrap(), v);
        }
    }

    #[test]
    fn built_values_round_trip() {
        let v = json!({
            "unicode": "héllo\t中文 😀\u{1}",
            "nums": [0, -0.0, 1.5, 1e21, u64::MAX, i64::MIN],
            "empty": [{}, []],
            "deep": {"a": {"b": {"c": {"d": [null, true]}}}},
        });
        assert_eq!(p(&v.to_string()), v);
        assert_eq!(p(&v.to_string_pretty()), v);
    }
}

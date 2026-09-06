//! A TOML 1.0 reader and writer with no dependencies outside `std`.
//!
//! The parser is a single pass over the input that builds [`Toml`], an
//! insertion-ordered tree. It is written for configuration files a person
//! edits by hand: every parse error carries the line and column where the
//! input stopped making sense, and the accessor helpers turn a parsed tree
//! into application structs without a derive macro.
//!
//! Coverage is TOML 1.0 minus dates and times, which no td application
//! reads; a date or time value is refused by name rather than mis-parsed.

use std::fmt;

/// Nesting accepted before the parser gives up. Bounds recursion in
/// [`parse`], in the tree conversion, and in the writer for hostile input.
const MAX_DEPTH: usize = 64;

/// A TOML value. `Table` keeps document order, so a mapping pass sees keys
/// in the order the file wrote them.
#[derive(Debug, Clone, PartialEq)]
pub enum Toml {
    Str(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    Arr(Vec<Toml>),
    Table(Vec<(String, Toml)>),
}

// --- accessors ---------------------------------------------------------

impl Toml {
    /// The name this module uses for the value's type in error messages.
    pub fn type_name(&self) -> &'static str {
        match self {
            Toml::Str(_) => "string",
            Toml::Int(_) => "integer",
            Toml::Float(_) => "float",
            Toml::Bool(_) => "boolean",
            Toml::Arr(_) => "array",
            Toml::Table(_) => "table",
        }
    }

    pub fn is_table(&self) -> bool {
        matches!(self, Toml::Table(_))
    }

    /// The value of `key`, or `None` for a missing key or a non-table.
    pub fn get(&self, key: &str) -> Option<&Toml> {
        match self {
            Toml::Table(entries) => entries.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Toml::Str(s) => Some(s.as_str()),
            _ => None,
        }
    }

    pub fn as_int(&self) -> Option<i64> {
        match self {
            Toml::Int(i) => Some(*i),
            _ => None,
        }
    }

    /// An integer is accepted as a float, so `threshold = 1` reads as `1.0`.
    pub fn as_float(&self) -> Option<f64> {
        match self {
            Toml::Float(f) => Some(*f),
            Toml::Int(i) => Some(*i as f64),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Toml::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn as_arr(&self) -> Option<&[Toml]> {
        match self {
            Toml::Arr(items) => Some(items.as_slice()),
            _ => None,
        }
    }

    pub fn as_table(&self) -> Option<&[(String, Toml)]> {
        match self {
            Toml::Table(entries) => Some(entries.as_slice()),
            _ => None,
        }
    }

    /// This table's keys in document order; empty for a non-table.
    pub fn table_keys(&self) -> Vec<&str> {
        match self {
            Toml::Table(entries) => entries.iter().map(|(k, _)| k.as_str()).collect(),
            _ => Vec::new(),
        }
    }

    /// Keys present that `allowed` does not name, in document order. This is
    /// the `deny_unknown_fields` check, split out so a caller can report all
    /// of them at once.
    pub fn unknown_keys(&self, allowed: &[&str]) -> Vec<String> {
        match self {
            Toml::Table(entries) => entries
                .iter()
                .filter(|(k, _)| !allowed.contains(&k.as_str()))
                .map(|(k, _)| k.clone())
                .collect(),
            _ => Vec::new(),
        }
    }

    /// `deny_unknown_fields` as one call: the first unrecognised key becomes
    /// an error whose text is identical to serde's.
    pub fn check_known_keys(&self, allowed: &[&str]) -> Result<(), Error> {
        match self.unknown_keys(allowed).first() {
            Some(key) => Err(Error::unknown_field(key, allowed)),
            None => Ok(()),
        }
    }
}

/// `invalid type for `key`: expected T, found U`.
fn type_error(key: &str, expected: &str, found: &Toml) -> Error {
    Error::new(format!(
        "invalid type for `{key}`: expected {expected}, found {}",
        found.type_name()
    ))
}

/// Generates the `require_*` / `optional_*` pair for one accessor.
macro_rules! field_accessors {
    ($req:ident, $opt:ident, $as:ident, $ty:ty, $expected:literal) => {
        #[doc = concat!("The value of `key` as ", $expected, ", or an error naming the key.")]
        pub fn $req(&self, key: &str) -> Result<$ty, Error> {
            let value = self.require(key)?;
            value.$as().ok_or_else(|| type_error(key, $expected, value))
        }

        #[doc = concat!("The value of `key` as ", $expected, ", when the key is present.")]
        pub fn $opt(&self, key: &str) -> Result<Option<$ty>, Error> {
            match self.get(key) {
                None => Ok(None),
                Some(value) => value
                    .$as()
                    .map(Some)
                    .ok_or_else(|| type_error(key, $expected, value)),
            }
        }
    };
}

impl Toml {
    /// The value of `key`, or serde's `missing field \`key\`` error.
    pub fn require(&self, key: &str) -> Result<&Toml, Error> {
        self.get(key).ok_or_else(|| Error::missing_field(key))
    }

    field_accessors!(require_str, optional_str, as_str, &str, "a string");
    field_accessors!(require_int, optional_int, as_int, i64, "an integer");
    field_accessors!(require_float, optional_float, as_float, f64, "a float");
    field_accessors!(require_bool, optional_bool, as_bool, bool, "a boolean");
    field_accessors!(require_arr, optional_arr, as_arr, &[Toml], "an array");
    field_accessors!(
        require_table,
        optional_table,
        as_table_value,
        &Toml,
        "a table"
    );

    /// `self` when it is a table, so the table accessors can hand back a
    /// [`Toml`] the caller can walk further.
    fn as_table_value(&self) -> Option<&Toml> {
        match self {
            Toml::Table(_) => Some(self),
            _ => None,
        }
    }
}

/// Generates an unsigned-integer `require_*` / `optional_*` pair.
macro_rules! uint_accessors {
    ($req:ident, $opt:ident, $ty:ty) => {
        #[doc = concat!("The value of `key` as a `", stringify!($ty), "`.")]
        pub fn $req(&self, key: &str) -> Result<$ty, Error> {
            let value = self.require_int(key)?;
            <$ty>::try_from(value).map_err(|_| range_error(key, value, <$ty>::MAX as u64))
        }

        #[doc = concat!("The value of `key` as a `", stringify!($ty), "`, when present.")]
        pub fn $opt(&self, key: &str) -> Result<Option<$ty>, Error> {
            match self.optional_int(key)? {
                None => Ok(None),
                Some(value) => <$ty>::try_from(value)
                    .map(Some)
                    .map_err(|_| range_error(key, value, <$ty>::MAX as u64)),
            }
        }
    };
}

fn range_error(key: &str, value: i64, max: u64) -> Error {
    Error::new(format!(
        "invalid value for `{key}`: expected an integer between 0 and {max}, found {value}"
    ))
}

impl Toml {
    uint_accessors!(require_u32, optional_u32, u32);
    uint_accessors!(require_u64, optional_u64, u64);
    uint_accessors!(require_usize, optional_usize, usize);
}

// --- errors ------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Pos {
    line: usize,
    column: usize,
}

/// A parse or mapping failure. Parse failures carry the position they were
/// found at and print as `line N, column M: message`. Failures raised by the
/// accessor helpers have no position, because a parsed [`Toml`] keeps no
/// spans, and print as the bare message so a caller can wrap them verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    pos: Option<Pos>,
    message: String,
}

impl Error {
    /// An error at a known position, counted from 1 in characters.
    pub fn at(line: usize, column: usize, message: impl Into<String>) -> Error {
        Error {
            pos: Some(Pos { line, column }),
            message: message.into(),
        }
    }

    /// An error with no position, printed as its message alone.
    pub fn new(message: impl Into<String>) -> Error {
        Error {
            pos: None,
            message: message.into(),
        }
    }

    /// serde's `missing field \`name\``, so a hand-written mapping keeps the
    /// wording callers already assert on.
    pub fn missing_field(field: &str) -> Error {
        Error::new(missing_field_message(field))
    }

    /// serde's `unknown field \`name\`, expected one of ...`.
    pub fn unknown_field(field: &str, expected: &[&str]) -> Error {
        Error::new(unknown_field_message(field, expected))
    }

    pub fn line(&self) -> Option<usize> {
        self.pos.map(|p| p.line)
    }

    pub fn column(&self) -> Option<usize> {
        self.pos.map(|p| p.column)
    }

    /// The message without any position prefix.
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.pos {
            Some(p) => write!(f, "line {}, column {}: {}", p.line, p.column, self.message),
            None => f.write_str(&self.message),
        }
    }
}

impl std::error::Error for Error {}

/// serde's `missing field \`name\`` text.
pub fn missing_field_message(field: &str) -> String {
    format!("missing field `{field}`")
}

/// serde's `unknown field` text, including its one/two/many spellings of the
/// expected list.
pub fn unknown_field_message(field: &str, expected: &[&str]) -> String {
    let mut out = format!("unknown field `{field}`, ");
    match expected {
        [] => out.push_str("there are no fields"),
        [one] => out.push_str(&format!("expected `{one}`")),
        [first, second] => out.push_str(&format!("expected `{first}` or `{second}`")),
        many => {
            out.push_str("expected one of ");
            for (i, name) in many.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push_str(&format!("`{name}`"));
            }
        }
    }
    out
}

// --- writer ------------------------------------------------------------

impl fmt::Display for Toml {
    /// Writes TOML. A table prints as a document of `key = value` lines with
    /// nested tables inline; any other value prints as a TOML literal. The
    /// output parses back to an equal value.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Toml::Table(entries) => {
                for (key, value) in entries {
                    writeln!(f, "{} = {}", ValueKey(key), ValueRef(value, 0))?;
                }
                Ok(())
            }
            other => ValueRef(other, 0).fmt(f),
        }
    }
}

struct ValueKey<'a>(&'a str);

impl fmt::Display for ValueKey<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if !self.0.is_empty() && self.0.chars().all(is_bare_key_char) {
            f.write_str(self.0)
        } else {
            f.write_str(&quote_basic(self.0))
        }
    }
}

struct ValueRef<'a>(&'a Toml, usize);

impl fmt::Display for ValueRef<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.1 > MAX_DEPTH {
            return f.write_str("...");
        }
        match self.0 {
            Toml::Str(s) => f.write_str(&quote_basic(s)),
            Toml::Int(i) => write!(f, "{i}"),
            Toml::Float(x) => f.write_str(&write_float(*x)),
            Toml::Bool(b) => write!(f, "{b}"),
            Toml::Arr(items) => {
                f.write_str("[")?;
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    ValueRef(item, self.1 + 1).fmt(f)?;
                }
                f.write_str("]")
            }
            Toml::Table(entries) => {
                if entries.is_empty() {
                    return f.write_str("{}");
                }
                f.write_str("{ ")?;
                for (i, (key, value)) in entries.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{} = {}", ValueKey(key), ValueRef(value, self.1 + 1))?;
                }
                f.write_str(" }")
            }
        }
    }
}

/// TOML has no `NaN`/`Infinity` spellings, and a float must keep a `.` or an
/// exponent to read back as a float.
fn write_float(value: f64) -> String {
    if value.is_nan() {
        return if value.is_sign_negative() {
            "-nan"
        } else {
            "nan"
        }
        .to_string();
    }
    if value.is_infinite() {
        return if value < 0.0 { "-inf" } else { "inf" }.to_string();
    }
    format!("{value:?}")
}

fn quote_basic(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if is_control(c) => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

// --- parse tree under construction -------------------------------------

/// How a table came to exist, which decides what may be added to it later.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TableKind {
    /// Written as `[a]` or `[[a]]`, or the document root.
    Header,
    /// Created as the super-table of a header; a later `[a]` may claim it.
    Implicit,
    /// Created by a dotted key; closed to header definition.
    Dotted,
    /// Written as `{ ... }`; closed to everything.
    Inline,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArrayKind {
    /// Written as `[ ... ]`; closed.
    Static,
    /// Written as `[[a]]`; a later `[[a]]` appends to it.
    Tables,
}

#[derive(Debug)]
enum Val {
    Str(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    Arr(ArrayKind, Vec<Val>),
    Table(TableKind, Vec<(String, Val)>),
}

fn into_toml(value: Val) -> Toml {
    match value {
        Val::Str(s) => Toml::Str(s),
        Val::Int(i) => Toml::Int(i),
        Val::Float(f) => Toml::Float(f),
        Val::Bool(b) => Toml::Bool(b),
        Val::Arr(_, items) => Toml::Arr(items.into_iter().map(into_toml).collect()),
        Val::Table(_, entries) => Toml::Table(
            entries
                .into_iter()
                .map(|(key, value)| (key, into_toml(value)))
                .collect(),
        ),
    }
}

/// A dotted path as TOML would write it, for error messages.
fn path_str(path: &[String]) -> String {
    let mut out = String::new();
    for (i, part) in path.iter().enumerate() {
        if i > 0 {
            out.push('.');
        }
        out.push_str(&ValueKey(part).to_string());
    }
    out
}

fn is_bare_key_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

fn is_control(c: char) -> bool {
    (c < '\u{20}' && c != '\t') || c == '\u{7f}'
}

// --- tree mutation -----------------------------------------------------

/// Message for a `[header]` that names something already defined.
fn redefine_message(existing: &Val, path: &[String]) -> String {
    let name = path_str(path);
    match existing {
        Val::Table(TableKind::Dotted, _) => {
            format!("table `{name}` was already defined by a dotted key")
        }
        Val::Table(TableKind::Inline, _) => format!("cannot extend inline table `{name}`"),
        Val::Table(_, _) => format!("table `{name}` is defined more than once"),
        Val::Arr(ArrayKind::Tables, _) => format!("`{name}` is an array of tables"),
        Val::Arr(ArrayKind::Static, _) => format!("cannot extend array `{name}`"),
        _ => format!("`{name}` is already defined as a value"),
    }
}

/// Walks `path`'s parents from `root`, creating implicit tables, and returns
/// the entries of the table the last part must live in.
fn super_table<'v>(
    root: &'v mut Val,
    path: &[String],
    line: usize,
    column: usize,
) -> Result<&'v mut Vec<(String, Val)>, Error> {
    let mut cur = match root {
        Val::Table(_, entries) => entries,
        _ => return Err(Error::at(line, column, "the document root is not a table")),
    };
    let parents = path.len().saturating_sub(1);
    for (n, key) in path.iter().take(parents).enumerate() {
        let idx = match cur.iter().position(|(k, _)| k == key) {
            Some(i) => i,
            None => {
                cur.push((key.clone(), Val::Table(TableKind::Implicit, Vec::new())));
                cur.len().saturating_sub(1)
            }
        };
        let slot = match cur.get_mut(idx) {
            Some(slot) => slot,
            None => return Err(Error::at(line, column, "internal error: lost table slot")),
        };
        let here = path.get(..=n).unwrap_or(path);
        cur = match &mut slot.1 {
            Val::Table(TableKind::Inline, _) => {
                return Err(Error::at(
                    line,
                    column,
                    format!("cannot extend inline table `{}`", path_str(here)),
                ))
            }
            Val::Table(_, entries) => entries,
            Val::Arr(ArrayKind::Tables, items) => match items.last_mut() {
                Some(Val::Table(_, entries)) => entries,
                _ => {
                    return Err(Error::at(
                        line,
                        column,
                        format!("`{}` is not a table", path_str(here)),
                    ))
                }
            },
            Val::Arr(ArrayKind::Static, _) => {
                return Err(Error::at(
                    line,
                    column,
                    format!("cannot extend array `{}`", path_str(here)),
                ))
            }
            _ => {
                return Err(Error::at(
                    line,
                    column,
                    format!("`{}` is not a table", path_str(here)),
                ))
            }
        };
    }
    Ok(cur)
}

/// Applies a `[path]` or `[[path]]` header to the tree.
fn open_table(
    root: &mut Val,
    path: &[String],
    array: bool,
    line: usize,
    column: usize,
) -> Result<(), Error> {
    let Some(last) = path.last() else {
        return Err(Error::at(line, column, "a table header may not be empty"));
    };
    let cur = super_table(root, path, line, column)?;
    let existing = cur.iter().position(|(k, _)| k == last);
    match (array, existing) {
        (false, None) => cur.push((last.clone(), Val::Table(TableKind::Header, Vec::new()))),
        (false, Some(idx)) => {
            let slot = match cur.get_mut(idx) {
                Some(slot) => slot,
                None => return Err(Error::at(line, column, "internal error: lost table slot")),
            };
            // A super-table created for an earlier `[a.b]` may be claimed
            // once by its own `[a]` header; anything else is a redefinition.
            if let Val::Table(kind, _) = &mut slot.1 {
                if *kind == TableKind::Implicit {
                    *kind = TableKind::Header;
                    return Ok(());
                }
            }
            return Err(Error::at(line, column, redefine_message(&slot.1, path)));
        }
        (true, None) => cur.push((
            last.clone(),
            Val::Arr(
                ArrayKind::Tables,
                vec![Val::Table(TableKind::Header, Vec::new())],
            ),
        )),
        (true, Some(idx)) => {
            let slot = match cur.get_mut(idx) {
                Some(slot) => slot,
                None => return Err(Error::at(line, column, "internal error: lost table slot")),
            };
            match &mut slot.1 {
                Val::Arr(ArrayKind::Tables, items) => {
                    items.push(Val::Table(TableKind::Header, Vec::new()));
                }
                other => {
                    let name = path_str(path);
                    return Err(Error::at(
                        line,
                        column,
                        match other {
                            Val::Table(_, _) => format!("`{name}` is a table, not an array"),
                            _ => format!("`{name}` is not an array of tables"),
                        },
                    ));
                }
            }
        }
    }
    Ok(())
}

/// The entries of the table a header selected, for the key/value lines that
/// follow it.
fn entries_at<'v>(
    root: &'v mut Val,
    path: &[String],
    line: usize,
    column: usize,
) -> Result<&'v mut Vec<(String, Val)>, Error> {
    let mut cur = match root {
        Val::Table(_, entries) => entries,
        _ => return Err(Error::at(line, column, "the document root is not a table")),
    };
    for key in path {
        let idx = match cur.iter().position(|(k, _)| k == key) {
            Some(idx) => idx,
            None => return Err(Error::at(line, column, "internal error: missing table")),
        };
        let slot = match cur.get_mut(idx) {
            Some(slot) => slot,
            None => return Err(Error::at(line, column, "internal error: lost table slot")),
        };
        cur = match &mut slot.1 {
            Val::Table(_, entries) => entries,
            Val::Arr(ArrayKind::Tables, items) => match items.last_mut() {
                Some(Val::Table(_, entries)) => entries,
                _ => return Err(Error::at(line, column, "internal error: empty table array")),
            },
            _ => return Err(Error::at(line, column, "internal error: not a table")),
        };
    }
    Ok(cur)
}

/// Stores `value` at a (possibly dotted) key inside `entries`.
fn assign(
    entries: &mut Vec<(String, Val)>,
    path: &[String],
    value: Val,
    line: usize,
    column: usize,
) -> Result<(), Error> {
    let Some(last) = path.last() else {
        return Err(Error::at(line, column, "a key may not be empty"));
    };
    let mut cur = entries;
    let parents = path.len().saturating_sub(1);
    for (n, key) in path.iter().take(parents).enumerate() {
        let idx = match cur.iter().position(|(k, _)| k == key) {
            Some(idx) => idx,
            None => {
                cur.push((key.clone(), Val::Table(TableKind::Dotted, Vec::new())));
                cur.len().saturating_sub(1)
            }
        };
        let slot = match cur.get_mut(idx) {
            Some(slot) => slot,
            None => return Err(Error::at(line, column, "internal error: lost table slot")),
        };
        let here = path.get(..=n).unwrap_or(path);
        // Only a table this same dotted-key run created may be extended: a
        // dotted key may not reopen a `[header]` table or an inline table.
        cur = match &mut slot.1 {
            Val::Table(TableKind::Dotted, entries) => entries,
            other => {
                return Err(Error::at(
                    line,
                    column,
                    match other {
                        Val::Table(TableKind::Inline, _) => {
                            format!("cannot extend inline table `{}`", path_str(here))
                        }
                        Val::Table(_, _) => {
                            format!("`{}` was already defined by a table header", path_str(here))
                        }
                        _ => format!("`{}` is not a table", path_str(here)),
                    },
                ))
            }
        };
    }
    if cur.iter().any(|(k, _)| k == last) {
        return Err(Error::at(
            line,
            column,
            format!("duplicate key `{}`", path_str(path)),
        ));
    }
    cur.push((last.clone(), value));
    Ok(())
}

// --- parser ------------------------------------------------------------

/// Parses a TOML document and returns its root table.
///
/// Two deliberate deviations from TOML 1.0, both documented rather than
/// silent: date and time values are refused with a clear error, and a
/// multi-line string stores `\r\n` as `\n` so a file edited on either kind
/// of host reads the same.
pub fn parse(input: &str) -> Result<Toml, Error> {
    let chars: Vec<char> = input.chars().collect();
    let mut parser = Parser {
        src: &chars,
        i: 0,
        line: 1,
        col: 1,
    };
    parser.document()
}

struct Parser<'a> {
    src: &'a [char],
    i: usize,
    line: usize,
    col: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<char> {
        self.src.get(self.i).copied()
    }

    fn peek_at(&self, n: usize) -> Option<char> {
        self.src.get(self.i.saturating_add(n)).copied()
    }

    fn advance(&mut self) {
        if let Some(c) = self.src.get(self.i).copied() {
            self.i = self.i.saturating_add(1);
            if c == '\n' {
                self.line = self.line.saturating_add(1);
                self.col = 1;
            } else {
                self.col = self.col.saturating_add(1);
            }
        }
    }

    fn next_char(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.advance();
        Some(c)
    }

    fn eat(&mut self, c: char) -> bool {
        if self.peek() == Some(c) {
            self.advance();
            true
        } else {
            false
        }
    }

    fn err<T>(&self, message: impl Into<String>) -> Result<T, Error> {
        Err(Error::at(self.line, self.col, message))
    }

    fn skip_spaces(&mut self) {
        while matches!(self.peek(), Some(' ') | Some('\t')) {
            self.advance();
        }
    }

    fn skip_comment(&mut self) {
        if self.peek() == Some('#') {
            while let Some(c) = self.peek() {
                if c == '\n' {
                    break;
                }
                self.advance();
            }
        }
    }

    /// Whitespace, newlines and comments: legal between array elements and
    /// between document lines.
    fn skip_trivia(&mut self) {
        loop {
            match self.peek() {
                Some(' ' | '\t' | '\n' | '\r') => self.advance(),
                Some('#') => self.skip_comment(),
                _ => return,
            }
        }
    }

    /// After a value or header: an optional comment, then the line must end.
    fn finish_line(&mut self, what: &str) -> Result<(), Error> {
        self.skip_spaces();
        self.skip_comment();
        match self.peek() {
            None | Some('\n') => {
                self.advance();
                Ok(())
            }
            Some('\r') if self.peek_at(1) == Some('\n') => {
                self.advance();
                self.advance();
                Ok(())
            }
            Some(c) => self.err(format!("unexpected `{}` after {what}", c.escape_debug())),
        }
    }

    fn document(&mut self) -> Result<Toml, Error> {
        let mut root = Val::Table(TableKind::Header, Vec::new());
        let mut path: Vec<String> = Vec::new();
        loop {
            self.skip_trivia();
            let Some(c) = self.peek() else {
                break;
            };
            let (line, col) = (self.line, self.col);
            if c == '[' {
                self.advance();
                let array = self.eat('[');
                self.skip_spaces();
                let header = self.key()?;
                self.skip_spaces();
                if !self.eat(']') {
                    return self.err("expected `]` to close the table header");
                }
                if array && !self.eat(']') {
                    return self.err("expected `]]` to close the array-of-tables header");
                }
                open_table(&mut root, &header, array, line, col)?;
                path = header;
                self.finish_line("a table header")?;
            } else {
                let key = self.key()?;
                self.skip_spaces();
                if !self.eat('=') {
                    return self.err("expected `=` after a key");
                }
                self.skip_spaces();
                let value = self.value(1)?;
                let entries = entries_at(&mut root, &path, line, col)?;
                assign(entries, &key, value, line, col)?;
                self.finish_line("a value")?;
            }
        }
        Ok(into_toml(root))
    }

    /// A key, possibly dotted. Leaves the cursor on the first character
    /// after it.
    fn key(&mut self) -> Result<Vec<String>, Error> {
        let mut parts = Vec::new();
        loop {
            self.skip_spaces();
            let (line, col) = (self.line, self.col);
            let part = match self.peek() {
                Some('"') => {
                    if self.peek_at(1) == Some('"') && self.peek_at(2) == Some('"') {
                        return self.err("a key may not be a multi-line string");
                    }
                    self.basic_string()?
                }
                Some('\'') => {
                    if self.peek_at(1) == Some('\'') && self.peek_at(2) == Some('\'') {
                        return self.err("a key may not be a multi-line string");
                    }
                    self.literal_string()?
                }
                Some(c) if is_bare_key_char(c) => {
                    let mut bare = String::new();
                    while let Some(c) = self.peek() {
                        if is_bare_key_char(c) {
                            bare.push(c);
                            self.advance();
                        } else {
                            break;
                        }
                    }
                    bare
                }
                Some(c) => {
                    return Err(Error::at(
                        line,
                        col,
                        format!("expected a key, found `{}`", c.escape_debug()),
                    ))
                }
                None => {
                    return Err(Error::at(line, col, "expected a key, found end of input"));
                }
            };
            parts.push(part);
            self.skip_spaces();
            if self.peek() == Some('.') {
                self.advance();
            } else {
                break;
            }
        }
        if parts.len() > MAX_DEPTH {
            return self.err("key nested too deeply");
        }
        Ok(parts)
    }

    fn value(&mut self, depth: usize) -> Result<Val, Error> {
        if depth > MAX_DEPTH {
            return self.err("value nested too deeply");
        }
        match self.peek() {
            Some('"') => {
                let multi = self.peek_at(1) == Some('"') && self.peek_at(2) == Some('"');
                let s = if multi {
                    self.multiline_basic_string()?
                } else {
                    self.basic_string()?
                };
                Ok(Val::Str(s))
            }
            Some('\'') => {
                let multi = self.peek_at(1) == Some('\'') && self.peek_at(2) == Some('\'');
                let s = if multi {
                    self.multiline_literal_string()?
                } else {
                    self.literal_string()?
                };
                Ok(Val::Str(s))
            }
            Some('[') => self.array(depth),
            Some('{') => self.inline_table(depth),
            Some(c) if c.is_ascii_alphanumeric() || c == '+' || c == '-' => self.bare_value(),
            Some(c) => self.err(format!("expected a value, found `{}`", c.escape_debug())),
            None => self.err("expected a value, found end of input"),
        }
    }

    /// A number, boolean, `inf`/`nan`, or the date/time shapes we refuse.
    fn bare_value(&mut self) -> Result<Val, Error> {
        let (line, col) = (self.line, self.col);
        let mut token = String::new();
        while let Some(c) = self.peek() {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | ':' | '+' | '-') {
                token.push(c);
                self.advance();
            } else {
                break;
            }
        }
        token_value(&token, line, col)
    }

    fn array(&mut self, depth: usize) -> Result<Val, Error> {
        let (line, col) = (self.line, self.col);
        self.advance();
        let mut items = Vec::new();
        loop {
            self.skip_trivia();
            match self.peek() {
                None => return Err(Error::at(line, col, "unterminated array")),
                Some(']') => {
                    self.advance();
                    return Ok(Val::Arr(ArrayKind::Static, items));
                }
                _ => {}
            }
            items.push(self.value(depth.saturating_add(1))?);
            self.skip_trivia();
            match self.peek() {
                Some(',') => self.advance(),
                Some(']') => {
                    self.advance();
                    return Ok(Val::Arr(ArrayKind::Static, items));
                }
                None => return Err(Error::at(line, col, "unterminated array")),
                Some(c) => {
                    return self.err(format!(
                        "expected `,` or `]` in an array, found `{}`",
                        c.escape_debug()
                    ))
                }
            }
        }
    }

    fn inline_table(&mut self, depth: usize) -> Result<Val, Error> {
        let (line, col) = (self.line, self.col);
        self.advance();
        let mut entries: Vec<(String, Val)> = Vec::new();
        self.skip_spaces();
        if self.eat('}') {
            return Ok(Val::Table(TableKind::Inline, entries));
        }
        loop {
            self.skip_spaces();
            if matches!(self.peek(), Some('\n') | Some('\r')) {
                return Err(Error::at(line, col, "an inline table must be on one line"));
            }
            let (kl, kc) = (self.line, self.col);
            let key = self.key()?;
            self.skip_spaces();
            if !self.eat('=') {
                return self.err("expected `=` after a key in an inline table");
            }
            self.skip_spaces();
            let value = self.value(depth.saturating_add(1))?;
            assign(&mut entries, &key, value, kl, kc)?;
            self.skip_spaces();
            match self.peek() {
                Some(',') => {
                    self.advance();
                    self.skip_spaces();
                    if self.peek() == Some('}') {
                        return self.err("an inline table may not have a trailing comma");
                    }
                }
                Some('}') => {
                    self.advance();
                    return Ok(Val::Table(TableKind::Inline, entries));
                }
                Some('\n') | Some('\r') => {
                    return Err(Error::at(line, col, "an inline table must be on one line"))
                }
                None => return Err(Error::at(line, col, "unterminated inline table")),
                Some(c) => {
                    return self.err(format!(
                        "expected `,` or `}}` in an inline table, found `{}`",
                        c.escape_debug()
                    ))
                }
            }
        }
    }

    // --- strings -------------------------------------------------------

    fn basic_string(&mut self) -> Result<String, Error> {
        let (line, col) = (self.line, self.col);
        self.advance();
        let mut out = String::new();
        loop {
            let (cl, cc) = (self.line, self.col);
            match self.next_char() {
                None | Some('\n') => return Err(Error::at(line, col, "unterminated basic string")),
                Some('"') => return Ok(out),
                Some('\\') => self.escape(&mut out, false)?,
                Some(c) if is_control(c) => {
                    return Err(Error::at(cl, cc, control_message(c)));
                }
                Some(c) => out.push(c),
            }
        }
    }

    fn literal_string(&mut self) -> Result<String, Error> {
        let (line, col) = (self.line, self.col);
        self.advance();
        let mut out = String::new();
        loop {
            let (cl, cc) = (self.line, self.col);
            match self.next_char() {
                None | Some('\n') => {
                    return Err(Error::at(line, col, "unterminated literal string"))
                }
                Some('\'') => return Ok(out),
                Some(c) if is_control(c) => {
                    return Err(Error::at(cl, cc, control_message(c)));
                }
                Some(c) => out.push(c),
            }
        }
    }

    fn multiline_basic_string(&mut self) -> Result<String, Error> {
        let (line, col) = (self.line, self.col);
        self.advance();
        self.advance();
        self.advance();
        self.skip_leading_newline();
        let mut out = String::new();
        loop {
            let (cl, cc) = (self.line, self.col);
            match self.peek() {
                None => {
                    return Err(Error::at(line, col, "unterminated multi-line basic string"));
                }
                Some('"') => {
                    if let Some(done) = self.close_multiline('"', &mut out, line, col)? {
                        return Ok(done);
                    }
                }
                Some('\\') => {
                    self.advance();
                    self.escape(&mut out, true)?;
                }
                Some('\r') => self.take_crlf(&mut out, cl, cc)?,
                Some(c) if is_control(c) && c != '\n' => {
                    return Err(Error::at(cl, cc, control_message(c)));
                }
                Some(c) => {
                    out.push(c);
                    self.advance();
                }
            }
        }
    }

    fn multiline_literal_string(&mut self) -> Result<String, Error> {
        let (line, col) = (self.line, self.col);
        self.advance();
        self.advance();
        self.advance();
        self.skip_leading_newline();
        let mut out = String::new();
        loop {
            let (cl, cc) = (self.line, self.col);
            match self.peek() {
                None => {
                    return Err(Error::at(
                        line,
                        col,
                        "unterminated multi-line literal string",
                    ));
                }
                Some('\'') => {
                    if let Some(done) = self.close_multiline('\'', &mut out, line, col)? {
                        return Ok(done);
                    }
                }
                Some('\r') => self.take_crlf(&mut out, cl, cc)?,
                Some(c) if is_control(c) && c != '\n' => {
                    return Err(Error::at(cl, cc, control_message(c)));
                }
                Some(c) => {
                    out.push(c);
                    self.advance();
                }
            }
        }
    }

    /// A newline straight after the opening `"""` or `'''` is not content.
    fn skip_leading_newline(&mut self) {
        if self.peek() == Some('\n') {
            self.advance();
        } else if self.peek() == Some('\r') && self.peek_at(1) == Some('\n') {
            self.advance();
            self.advance();
        }
    }

    /// CRLF is stored as LF; a lone CR is not valid TOML.
    fn take_crlf(&mut self, out: &mut String, line: usize, col: usize) -> Result<(), Error> {
        self.advance();
        if self.peek() == Some('\n') {
            self.advance();
            out.push('\n');
            Ok(())
        } else {
            Err(Error::at(
                line,
                col,
                "a carriage return must be followed by a line feed",
            ))
        }
    }

    /// Handles a run of quote characters inside a multi-line string. The
    /// last three close it; up to two before those are content.
    fn close_multiline(
        &mut self,
        quote: char,
        out: &mut String,
        line: usize,
        col: usize,
    ) -> Result<Option<String>, Error> {
        let mut run = 0usize;
        while self.peek_at(run) == Some(quote) {
            run = run.saturating_add(1);
            if run > 5 {
                return Err(Error::at(
                    line,
                    col,
                    format!("more than five `{quote}` characters in a row"),
                ));
            }
        }
        if run < 3 {
            for _ in 0..run {
                out.push(quote);
                self.advance();
            }
            return Ok(None);
        }
        for _ in 0..run.saturating_sub(3) {
            out.push(quote);
        }
        for _ in 0..run {
            self.advance();
        }
        Ok(Some(std::mem::take(out)))
    }

    /// One escape sequence, the backslash already consumed.
    fn escape(&mut self, out: &mut String, multiline: bool) -> Result<(), Error> {
        let (line, col) = (self.line, self.col);
        let Some(c) = self.next_char() else {
            return Err(Error::at(line, col, "unterminated escape sequence"));
        };
        match c {
            'b' => out.push('\u{8}'),
            't' => out.push('\t'),
            'n' => out.push('\n'),
            'f' => out.push('\u{c}'),
            'r' => out.push('\r'),
            '"' => out.push('"'),
            '\\' => out.push('\\'),
            'u' => self.unicode_escape(out, 4)?,
            'U' => self.unicode_escape(out, 8)?,
            ' ' | '\t' | '\n' | '\r' if multiline => {
                // A backslash at end of line eats the newline and the
                // leading whitespace of the next one.
                let mut newline = c == '\n';
                loop {
                    match self.peek() {
                        Some('\n') => {
                            newline = true;
                            self.advance();
                        }
                        Some(' ' | '\t' | '\r') => self.advance(),
                        _ => break,
                    }
                }
                if !newline {
                    return Err(Error::at(
                        line,
                        col,
                        "a backslash may only be followed by whitespace to the end of the line",
                    ));
                }
            }
            c => {
                // A control character has no readable spelling next to a
                // backslash, so name its code point instead.
                let what = if is_control(c) || c == '\n' {
                    format!(
                        "invalid escape sequence: a backslash followed by U+{:04X}",
                        c as u32
                    )
                } else {
                    format!("invalid escape sequence `\\{c}`")
                };
                return Err(Error::at(line, col, what));
            }
        }
        Ok(())
    }

    fn unicode_escape(&mut self, out: &mut String, digits: usize) -> Result<(), Error> {
        let (line, col) = (self.line, self.col);
        let marker = if digits == 4 { 'u' } else { 'U' };
        let mut value: u32 = 0;
        let mut seen = String::new();
        for _ in 0..digits {
            let Some(c) = self.peek() else {
                return Err(Error::at(line, col, "unterminated unicode escape"));
            };
            let Some(d) = c.to_digit(16) else {
                return Err(Error::at(
                    line,
                    col,
                    format!("`\\{marker}` needs {digits} hexadecimal digits"),
                ));
            };
            value = value.saturating_mul(16).saturating_add(d);
            seen.push(c);
            self.advance();
        }
        match char::from_u32(value) {
            Some(c) => {
                out.push(c);
                Ok(())
            }
            None => Err(Error::at(
                line,
                col,
                format!("`\\{marker}{seen}` is not a unicode scalar value"),
            )),
        }
    }
}

fn control_message(c: char) -> String {
    format!("control character U+{:04X} in a string", c as u32)
}

// --- numbers and keywords ----------------------------------------------

/// Classifies a bare token: boolean, float keyword, integer, float, or a
/// date/time we refuse.
fn token_value(token: &str, line: usize, col: usize) -> Result<Val, Error> {
    match token {
        "true" => return Ok(Val::Bool(true)),
        "false" => return Ok(Val::Bool(false)),
        _ => {}
    }
    let (signed, negative, body) = match token.strip_prefix('+') {
        Some(rest) => (true, false, rest),
        None => match token.strip_prefix('-') {
            Some(rest) => (true, true, rest),
            None => (false, false, token),
        },
    };
    match body {
        "inf" => {
            return Ok(Val::Float(if negative {
                f64::NEG_INFINITY
            } else {
                f64::INFINITY
            }))
        }
        "nan" => return Ok(Val::Float(if negative { -f64::NAN } else { f64::NAN })),
        _ => {}
    }
    if looks_like_datetime(token) {
        return Err(Error::at(
            line,
            col,
            format!("date and time values are not supported (`{token}`)"),
        ));
    }
    for (prefix, radix) in [("0x", 16u32), ("0o", 8), ("0b", 2)] {
        if let Some(rest) = body.strip_prefix(prefix) {
            if signed {
                return Err(Error::at(
                    line,
                    col,
                    format!("a `{prefix}` integer may not carry a sign"),
                ));
            }
            let Some(digits) = radix_digits(rest, radix) else {
                return Err(Error::at(line, col, format!("invalid integer `{token}`")));
            };
            return match i64::from_str_radix(&digits, radix) {
                Ok(v) => Ok(Val::Int(v)),
                Err(_) => Err(Error::at(
                    line,
                    col,
                    format!("integer `{token}` does not fit in 64 signed bits"),
                )),
            };
        }
    }
    decimal_value(token, body, negative, line, col)
}

/// `1979-05-27`, `07:32:00` and friends. A `-` inside a token is a date
/// separator unless it is an exponent sign.
fn looks_like_datetime(token: &str) -> bool {
    if token.contains(':') {
        return true;
    }
    let chars: Vec<char> = token.chars().collect();
    for (i, c) in chars.iter().enumerate() {
        if *c == '-' && i > 0 && !matches!(chars.get(i.saturating_sub(1)), Some('e') | Some('E')) {
            return true;
        }
    }
    false
}

/// Digits of `radix` with TOML's underscore rule: a separator must sit
/// between two digits.
fn radix_digits(s: &str, radix: u32) -> Option<String> {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::new();
    let mut after_digit = false;
    for (i, c) in chars.iter().enumerate() {
        if c.is_digit(radix) {
            out.push(*c);
            after_digit = true;
        } else if *c == '_' {
            if !after_digit
                || !chars
                    .get(i.saturating_add(1))
                    .is_some_and(|n| n.is_digit(radix))
            {
                return None;
            }
            after_digit = false;
        } else {
            return None;
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// Decimal digits from `chars` at `i`, honouring the underscore rule.
fn take_digits(chars: &[char], i: &mut usize) -> Option<String> {
    let mut out = String::new();
    let mut after_digit = false;
    while let Some(c) = chars.get(*i) {
        if c.is_ascii_digit() {
            out.push(*c);
            after_digit = true;
            *i = i.saturating_add(1);
        } else if *c == '_' {
            if !after_digit
                || !chars
                    .get(i.saturating_add(1))
                    .is_some_and(|n| n.is_ascii_digit())
            {
                return None;
            }
            after_digit = false;
            *i = i.saturating_add(1);
        } else {
            break;
        }
    }
    Some(out)
}

fn decimal_value(
    token: &str,
    body: &str,
    negative: bool,
    line: usize,
    col: usize,
) -> Result<Val, Error> {
    let invalid = || Error::at(line, col, format!("invalid value `{token}`"));
    let chars: Vec<char> = body.chars().collect();
    let mut i = 0usize;
    let Some(int_part) = take_digits(&chars, &mut i) else {
        return Err(invalid());
    };
    if int_part.is_empty() {
        return Err(invalid());
    }
    if int_part.len() > 1 && int_part.starts_with('0') {
        return Err(Error::at(
            line,
            col,
            format!("number `{token}` has a leading zero"),
        ));
    }
    let mut text = String::new();
    if negative {
        text.push('-');
    }
    text.push_str(&int_part);
    let mut float = false;
    if chars.get(i) == Some(&'.') {
        float = true;
        i = i.saturating_add(1);
        let Some(frac) = take_digits(&chars, &mut i) else {
            return Err(invalid());
        };
        if frac.is_empty() {
            return Err(invalid());
        }
        text.push('.');
        text.push_str(&frac);
    }
    if matches!(chars.get(i), Some('e') | Some('E')) {
        float = true;
        i = i.saturating_add(1);
        text.push('e');
        if let Some(sign @ ('+' | '-')) = chars.get(i).copied() {
            text.push(sign);
            i = i.saturating_add(1);
        }
        let Some(exp) = take_digits(&chars, &mut i) else {
            return Err(invalid());
        };
        if exp.is_empty() {
            return Err(invalid());
        }
        text.push_str(&exp);
    }
    if i != chars.len() {
        return Err(invalid());
    }
    if float {
        match text.parse::<f64>() {
            Ok(v) => Ok(Val::Float(v)),
            Err(_) => Err(invalid()),
        }
    } else {
        match text.parse::<i64>() {
            Ok(v) => Ok(Val::Int(v)),
            Err(_) => Err(Error::at(
                line,
                col,
                format!("integer `{token}` does not fit in 64 signed bits"),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse, or fail the test with the parser's own message.
    fn t(input: &str) -> Toml {
        match parse(input) {
            Ok(value) => value,
            Err(e) => panic!("parse failed: {e}\n--- input ---\n{input}"),
        }
    }

    /// The rendered error of an input that must not parse.
    fn bad(input: &str) -> String {
        match parse(input) {
            Ok(value) => panic!("expected an error, parsed {value:?}"),
            Err(e) => e.to_string(),
        }
    }

    fn str_at(value: &Toml, key: &str) -> String {
        value
            .get(key)
            .and_then(Toml::as_str)
            .unwrap_or("")
            .to_string()
    }

    // --- shape ---------------------------------------------------------

    #[test]
    fn empty_document_is_an_empty_table() {
        assert_eq!(t(""), Toml::Table(Vec::new()));
        assert_eq!(t("\n\n   \t\n"), Toml::Table(Vec::new()));
    }

    #[test]
    fn comments_are_ignored_everywhere() {
        let doc = t("# leading\n\na = 1 # trailing\n# between\n[s] # after a header\nb = 2\n");
        assert_eq!(doc.get("a").and_then(Toml::as_int), Some(1));
        assert_eq!(
            doc.get("s").and_then(|s| s.get("b")).and_then(Toml::as_int),
            Some(2)
        );
    }

    #[test]
    fn bare_and_quoted_keys() {
        let doc = t("bare = 1\nbare-key = 2\nbare_key = 3\n1234 = 4\n\"quoted key\" = 5\n'literal key' = 6\n\"\" = 7\n");
        assert_eq!(doc.get("bare").and_then(Toml::as_int), Some(1));
        assert_eq!(doc.get("bare-key").and_then(Toml::as_int), Some(2));
        assert_eq!(doc.get("bare_key").and_then(Toml::as_int), Some(3));
        assert_eq!(doc.get("1234").and_then(Toml::as_int), Some(4));
        assert_eq!(doc.get("quoted key").and_then(Toml::as_int), Some(5));
        assert_eq!(doc.get("literal key").and_then(Toml::as_int), Some(6));
        assert_eq!(doc.get("").and_then(Toml::as_int), Some(7));
    }

    #[test]
    fn dotted_keys_build_tables() {
        let doc = t("a.b.c = 1\na.b.d = 2\na . e = 3\n\"x y\".z = 4\n");
        let a = doc.get("a").unwrap();
        let b = a.get("b").unwrap();
        assert_eq!(b.get("c").and_then(Toml::as_int), Some(1));
        assert_eq!(b.get("d").and_then(Toml::as_int), Some(2));
        assert_eq!(a.get("e").and_then(Toml::as_int), Some(3));
        assert_eq!(
            doc.get("x y")
                .and_then(|x| x.get("z"))
                .and_then(Toml::as_int),
            Some(4)
        );
        assert_eq!(doc.table_keys(), vec!["a", "x y"]);
    }

    #[test]
    fn table_headers_and_nesting() {
        let doc = t("[a]\nx = 1\n\n[a.b]\ny = 2\n\n[ c . d ]\nz = 3\n");
        assert_eq!(
            doc.get("a").and_then(|a| a.get("x")).and_then(Toml::as_int),
            Some(1)
        );
        assert_eq!(
            doc.get("a")
                .and_then(|a| a.get("b"))
                .and_then(|b| b.get("y"))
                .and_then(Toml::as_int),
            Some(2)
        );
        assert_eq!(
            doc.get("c")
                .and_then(|c| c.get("d"))
                .and_then(|d| d.get("z"))
                .and_then(Toml::as_int),
            Some(3)
        );
    }

    #[test]
    fn a_super_table_may_be_defined_after_its_child() {
        let doc = t("[a.b]\ny = 2\n\n[a]\nx = 1\n");
        let a = doc.get("a").unwrap();
        assert_eq!(a.get("x").and_then(Toml::as_int), Some(1));
        assert_eq!(
            a.get("b").and_then(|b| b.get("y")).and_then(Toml::as_int),
            Some(2)
        );
        // The implicit table keeps the order it was created in.
        assert_eq!(a.table_keys(), vec!["b", "x"]);
    }

    #[test]
    fn a_dotted_table_accepts_a_sub_table_header() {
        // TOML 1.0 allows adding under a dotted-key table, but not
        // redefining the dotted table itself.
        let doc = t("[fruit]\napple.color = \"red\"\n\n[fruit.apple.texture]\nsmooth = true\n");
        let apple = doc.get("fruit").and_then(|f| f.get("apple")).unwrap();
        assert_eq!(apple.get("color").and_then(Toml::as_str), Some("red"));
        assert_eq!(
            apple
                .get("texture")
                .and_then(|x| x.get("smooth"))
                .and_then(Toml::as_bool),
            Some(true)
        );
    }

    #[test]
    fn arrays_of_tables() {
        let doc = t("[[feed]]\nname = \"a\"\n\n[[feed]]\nname = \"b\"\n");
        let feeds = doc.get("feed").and_then(Toml::as_arr).unwrap();
        assert_eq!(feeds.len(), 2);
        assert_eq!(str_at(&feeds[0], "name"), "a");
        assert_eq!(str_at(&feeds[1], "name"), "b");
    }

    #[test]
    fn arrays_of_tables_take_sub_table_headers() {
        let doc = t("[[rule]]\nname = \"one\"\n[rule.match]\nheader = \"From\"\n[rule.actions]\nflag = true\n\n[[rule]]\nname = \"two\"\n[rule.match]\nheader = \"To\"\n[rule.actions]\nflag = false\n");
        let rules = doc.get("rule").and_then(Toml::as_arr).unwrap();
        assert_eq!(rules.len(), 2);
        assert_eq!(
            rules[0]
                .get("match")
                .and_then(|m| m.get("header"))
                .and_then(Toml::as_str),
            Some("From")
        );
        assert_eq!(
            rules[1]
                .get("actions")
                .and_then(|a| a.get("flag"))
                .and_then(Toml::as_bool),
            Some(false)
        );
    }

    #[test]
    fn inline_tables() {
        let doc = t("a = { x = 1, y = \"two\" }\nempty = {}\ndotted = { a.b = 1 }\n");
        let a = doc.get("a").unwrap();
        assert_eq!(a.get("x").and_then(Toml::as_int), Some(1));
        assert_eq!(a.get("y").and_then(Toml::as_str), Some("two"));
        assert_eq!(doc.get("empty"), Some(&Toml::Table(Vec::new())));
        assert_eq!(
            doc.get("dotted")
                .and_then(|d| d.get("a"))
                .and_then(|a| a.get("b"))
                .and_then(Toml::as_int),
            Some(1)
        );
    }

    #[test]
    fn nested_inline_tables_and_arrays() {
        let doc = t("m = { not = { any = [ { header = \"From\", regex = \"a@\" } ] } }\n");
        let inner = doc
            .get("m")
            .and_then(|m| m.get("not"))
            .and_then(|n| n.get("any"))
            .and_then(Toml::as_arr)
            .unwrap();
        assert_eq!(str_at(&inner[0], "header"), "From");
        assert_eq!(str_at(&inner[0], "regex"), "a@");
    }

    #[test]
    fn arrays_span_lines_and_allow_a_trailing_comma() {
        let doc = t("a = [\n  1, # one\n  2,\n\n  # a comment line\n  3,\n]\nb = []\n");
        assert_eq!(
            doc.get("a").and_then(Toml::as_arr).map(<[Toml]>::len),
            Some(3)
        );
        assert_eq!(
            doc.get("b").and_then(Toml::as_arr).map(<[Toml]>::len),
            Some(0)
        );
    }

    #[test]
    fn arrays_of_inline_tables() {
        let doc = t("all = [\n  { header = \"From\", regex = \"alice@\" },\n  { header = \"Subject\", regex = \"urgent\" },\n]\n");
        let all = doc.get("all").and_then(Toml::as_arr).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(str_at(&all[1], "header"), "Subject");
    }

    #[test]
    fn arrays_may_be_heterogeneous() {
        let doc = t("a = [1, \"two\", 3.5, true, [4], { b = 5 }]\n");
        let a = doc.get("a").and_then(Toml::as_arr).unwrap();
        assert_eq!(a.len(), 6);
        assert_eq!(a[0].as_int(), Some(1));
        assert_eq!(a[1].as_str(), Some("two"));
        assert_eq!(a[2].as_float(), Some(3.5));
        assert_eq!(a[3].as_bool(), Some(true));
        assert_eq!(a[4].as_arr().map(<[Toml]>::len), Some(1));
        assert!(a[5].is_table());
    }

    // --- strings -------------------------------------------------------

    #[test]
    fn basic_string_escapes() {
        let doc = t(r#"s = "a\tb\nc\r\"d\\e\bf\fg""#);
        assert_eq!(
            doc.get("s").and_then(Toml::as_str),
            Some("a\tb\nc\r\"d\\e\u{8}f\u{c}g")
        );
    }

    #[test]
    fn unicode_escapes() {
        let doc = t(r#"a = "\u00e9\U0001F600\u0041""#);
        assert_eq!(
            doc.get("a").and_then(Toml::as_str),
            Some("\u{e9}\u{1F600}A")
        );
    }

    #[test]
    fn literal_strings_keep_backslashes() {
        let doc = t("re = '\\d+\\s*'\nwin = 'C:\\Users\\td'\n");
        assert_eq!(doc.get("re").and_then(Toml::as_str), Some("\\d+\\s*"));
        assert_eq!(doc.get("win").and_then(Toml::as_str), Some("C:\\Users\\td"));
    }

    #[test]
    fn dotted_keys_inside_an_array_of_tables_element() {
        let doc = t("[[rule]]\nname = \"one\"\nmatch.header = \"From\"\nmatch.regex = \"a@\"\nactions.mark_read = true\n");
        let rules = doc.require_arr("rule").unwrap();
        assert_eq!(rules.len(), 1);
        let matched = rules[0].require_table("match").unwrap();
        assert_eq!(matched.require_str("header").unwrap(), "From");
        assert_eq!(matched.require_str("regex").unwrap(), "a@");
        assert!(rules[0]
            .require_table("actions")
            .unwrap()
            .require_bool("mark_read")
            .unwrap());
    }

    #[test]
    fn a_legacy_section_sits_beside_named_accounts() {
        // tmc reads `[jmap]` only when `[account.NAME]` produced nothing.
        let doc = t("[jmap]\nusername = \"legacy@example.com\"\n\n[account.personal]\nusername = \"me@example.com\"\n\n[account.work]\nusername = \"me@work.com\"\n");
        assert_eq!(doc.table_keys(), vec!["jmap", "account"]);
        let accounts = doc.require_table("account").unwrap().as_table().unwrap();
        let names: Vec<&str> = accounts.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(names, vec!["personal", "work"]);
        assert_eq!(
            accounts[1].1.require_str("username").unwrap(),
            "me@work.com"
        );
        assert_eq!(
            doc.require_table("jmap")
                .unwrap()
                .require_str("username")
                .unwrap(),
            "legacy@example.com"
        );
    }

    #[test]
    fn multi_line_basic_strings() {
        let doc = t("s = \"\"\"\nline one\nline two\n\"\"\"\n");
        assert_eq!(
            doc.get("s").and_then(Toml::as_str),
            Some("line one\nline two\n")
        );
    }

    #[test]
    fn multi_line_basic_string_line_ending_backslash() {
        let doc = t("s = \"\"\"\\\n    the quick \\\n    brown \\\n    fox\\\n\"\"\"\n");
        assert_eq!(
            doc.get("s").and_then(Toml::as_str),
            Some("the quick brown fox")
        );
    }

    #[test]
    fn multi_line_strings_may_contain_quotes() {
        let doc = t("a = \"\"\"he said \"hi\" twice\"\"\"\nb = \"\"\"\"quoted\"\"\"\"\nc = '''it's '' fine'''\n");
        assert_eq!(
            doc.get("a").and_then(Toml::as_str),
            Some("he said \"hi\" twice")
        );
        assert_eq!(doc.get("b").and_then(Toml::as_str), Some("\"quoted\""));
        assert_eq!(doc.get("c").and_then(Toml::as_str), Some("it's '' fine"));
    }

    #[test]
    fn multi_line_literal_strings() {
        let doc = t("s = '''\nraw \\n stays\n'''\n");
        assert_eq!(doc.get("s").and_then(Toml::as_str), Some("raw \\n stays\n"));
    }

    #[test]
    fn strings_carry_non_ascii_text() {
        let doc = t("s = \"héllo wörld ☃\"\n'ключ' = 'значение'\n");
        assert_eq!(doc.get("s").and_then(Toml::as_str), Some("héllo wörld ☃"));
        assert_eq!(doc.get("ключ").and_then(Toml::as_str), Some("значение"));
    }

    // --- numbers and booleans -------------------------------------------

    #[test]
    fn integers() {
        let doc = t("a = 0\nb = 42\nc = -17\nd = +99\ne = 1_000_000\nf = 9223372036854775807\ng = -9223372036854775808\n");
        assert_eq!(doc.get("a").and_then(Toml::as_int), Some(0));
        assert_eq!(doc.get("b").and_then(Toml::as_int), Some(42));
        assert_eq!(doc.get("c").and_then(Toml::as_int), Some(-17));
        assert_eq!(doc.get("d").and_then(Toml::as_int), Some(99));
        assert_eq!(doc.get("e").and_then(Toml::as_int), Some(1_000_000));
        assert_eq!(doc.get("f").and_then(Toml::as_int), Some(i64::MAX));
        assert_eq!(doc.get("g").and_then(Toml::as_int), Some(i64::MIN));
    }

    #[test]
    fn integers_with_a_radix_prefix() {
        let doc = t("h = 0xdead_BEEF\no = 0o755\nb = 0b1010_0001\nz = 0x0\n");
        assert_eq!(doc.get("h").and_then(Toml::as_int), Some(0xdead_beef));
        assert_eq!(doc.get("o").and_then(Toml::as_int), Some(0o755));
        assert_eq!(doc.get("b").and_then(Toml::as_int), Some(0b1010_0001));
        assert_eq!(doc.get("z").and_then(Toml::as_int), Some(0));
    }

    #[test]
    fn floats() {
        let doc = t("a = 0.9\nb = -0.2\nc = 3.5025\nd = 5e+22\ne = 1e06\nf = -2E-2\ng = 6.626e-34\nh = 1_000.000_1\n");
        assert_eq!(doc.get("a").and_then(Toml::as_float), Some(0.9));
        assert_eq!(doc.get("b").and_then(Toml::as_float), Some(-0.2));
        assert_eq!(doc.get("c").and_then(Toml::as_float), Some(3.5025));
        assert_eq!(doc.get("d").and_then(Toml::as_float), Some(5e22));
        assert_eq!(doc.get("e").and_then(Toml::as_float), Some(1e6));
        assert_eq!(doc.get("f").and_then(Toml::as_float), Some(-2e-2));
        assert_eq!(doc.get("g").and_then(Toml::as_float), Some(6.626e-34));
        assert_eq!(doc.get("h").and_then(Toml::as_float), Some(1000.0001));
    }

    #[test]
    fn float_keywords() {
        let doc = t("a = inf\nb = +inf\nc = -inf\nd = nan\ne = -nan\n");
        assert_eq!(doc.get("a").and_then(Toml::as_float), Some(f64::INFINITY));
        assert_eq!(doc.get("b").and_then(Toml::as_float), Some(f64::INFINITY));
        assert_eq!(
            doc.get("c").and_then(Toml::as_float),
            Some(f64::NEG_INFINITY)
        );
        assert!(doc.get("d").and_then(Toml::as_float).unwrap().is_nan());
        assert!(doc.get("e").and_then(Toml::as_float).unwrap().is_nan());
    }

    #[test]
    fn booleans() {
        let doc = t("yes = true\nno = false\n");
        assert_eq!(doc.get("yes").and_then(Toml::as_bool), Some(true));
        assert_eq!(doc.get("no").and_then(Toml::as_bool), Some(false));
        assert_eq!(bad("a = True"), "line 1, column 5: invalid value `True`");
    }

    #[test]
    fn crlf_line_endings_parse() {
        let doc = t("[ui]\r\npage_size = 25\r\n\r\n[[feed]]\r\nname = \"a\"\r\n");
        assert_eq!(
            doc.get("ui")
                .and_then(|u| u.get("page_size"))
                .and_then(Toml::as_int),
            Some(25)
        );
        assert_eq!(
            doc.get("feed").and_then(Toml::as_arr).map(<[Toml]>::len),
            Some(1)
        );
    }

    // --- errors ---------------------------------------------------------

    #[test]
    fn unterminated_strings() {
        assert_eq!(
            bad("a = \"oops\n"),
            "line 1, column 5: unterminated basic string"
        );
        assert_eq!(
            bad("a = 'oops"),
            "line 1, column 5: unterminated literal string"
        );
        assert_eq!(
            bad("a = \"\"\"oops"),
            "line 1, column 5: unterminated multi-line basic string"
        );
        assert_eq!(
            bad("a = '''oops"),
            "line 1, column 5: unterminated multi-line literal string"
        );
    }

    #[test]
    fn bad_escapes() {
        assert_eq!(
            bad(r#"a = "\q""#),
            "line 1, column 7: invalid escape sequence `\\q`"
        );
        assert_eq!(
            bad(r#"a = "\uZZZZ""#),
            "line 1, column 8: `\\u` needs 4 hexadecimal digits"
        );
        assert_eq!(
            bad(r#"a = "\ud800""#),
            "line 1, column 8: `\\ud800` is not a unicode scalar value"
        );
        assert!(bad("a = \"\"\"x\\ y\"\"\"").contains("whitespace to the end of the line"));
        // A line-ending backslash is a multi-line feature only, and the
        // message names the character without breaking its own line.
        assert_eq!(
            bad("a = \"x\\\ny\"\n"),
            "line 1, column 8: invalid escape sequence: a backslash followed by U+000A"
        );
    }

    #[test]
    fn control_characters_are_refused() {
        assert_eq!(
            bad("a = \"x\u{1}\""),
            "line 1, column 7: control character U+0001 in a string"
        );
        assert_eq!(
            bad("a = \"\"\"x\ry\"\"\""),
            "line 1, column 9: a carriage return must be followed by a line feed"
        );
    }

    #[test]
    fn duplicate_keys_are_errors() {
        assert_eq!(bad("a = 1\na = 2\n"), "line 2, column 1: duplicate key `a`");
        assert_eq!(
            bad("[s]\nx = 1\nx = 2\n"),
            "line 3, column 1: duplicate key `x`"
        );
        assert_eq!(
            bad("a.b = 1\na.b = 2\n"),
            "line 2, column 1: duplicate key `a.b`"
        );
        assert_eq!(
            bad("a = { b = 1, b = 2 }\n"),
            "line 1, column 14: duplicate key `b`"
        );
    }

    #[test]
    fn redefined_tables_are_errors() {
        assert_eq!(
            bad("[a]\n[a]\n"),
            "line 2, column 1: table `a` is defined more than once"
        );
        assert_eq!(
            bad("[a.b]\n[a.b]\n"),
            "line 2, column 1: table `a.b` is defined more than once"
        );
        assert_eq!(
            bad("a.b = 1\n[a]\n"),
            "line 2, column 1: table `a` was already defined by a dotted key"
        );
        assert_eq!(
            bad("[a.b]\nc = 1\n[a]\nb.d = 2\n"),
            "line 4, column 1: `b` was already defined by a table header"
        );
        assert_eq!(
            bad("a = 1\n[a]\n"),
            "line 2, column 1: `a` is already defined as a value"
        );
    }

    #[test]
    fn inline_tables_and_static_arrays_are_closed() {
        assert_eq!(
            bad("a = { b = 1 }\n[a.c]\n"),
            "line 2, column 1: cannot extend inline table `a`"
        );
        assert_eq!(
            bad("a = { b = 1 }\na.c = 2\n"),
            "line 2, column 1: cannot extend inline table `a`"
        );
        assert_eq!(
            bad("a = [1]\n[[a]]\n"),
            "line 2, column 1: `a` is not an array of tables"
        );
    }

    #[test]
    fn a_table_and_an_array_of_tables_may_not_share_a_name() {
        assert_eq!(
            bad("[[a]]\n[a]\n"),
            "line 2, column 1: `a` is an array of tables"
        );
        assert_eq!(
            bad("[a]\n[[a]]\n"),
            "line 2, column 1: `a` is a table, not an array"
        );
    }

    #[test]
    fn dates_and_times_are_refused_by_name() {
        for input in [
            "a = 1979-05-27T07:32:00Z",
            "a = 1979-05-27 07:32:00",
            "a = 1979-05-27T00:32:00-07:00",
            "a = 1979-05-27",
            "a = 07:32:00",
            "a = 00:32:00.999999",
        ] {
            let message = bad(input);
            assert!(
                message.contains("date and time values are not supported"),
                "{input} gave {message}"
            );
        }
    }

    #[test]
    fn malformed_numbers() {
        assert_eq!(
            bad("a = 0123"),
            "line 1, column 5: number `0123` has a leading zero"
        );
        assert_eq!(
            bad("a = 9223372036854775808"),
            "line 1, column 5: integer `9223372036854775808` does not fit in 64 signed bits"
        );
        assert_eq!(
            bad("a = 0xFFFFFFFFFFFFFFFF"),
            "line 1, column 5: integer `0xFFFFFFFFFFFFFFFF` does not fit in 64 signed bits"
        );
        assert_eq!(bad("a = 1__0"), "line 1, column 5: invalid value `1__0`");
        assert_eq!(bad("a = 1_"), "line 1, column 5: invalid value `1_`");
        assert_eq!(bad("a = 1."), "line 1, column 5: invalid value `1.`");
        assert_eq!(
            bad("a = .5"),
            "line 1, column 5: expected a value, found `.`"
        );
        assert_eq!(bad("a = 1e"), "line 1, column 5: invalid value `1e`");
        assert_eq!(
            bad("a = -0x1"),
            "line 1, column 5: a `0x` integer may not carry a sign"
        );
        assert_eq!(bad("a = 0b2"), "line 1, column 5: invalid integer `0b2`");
    }

    #[test]
    fn structural_nonsense() {
        assert_eq!(
            bad("a = @"),
            "line 1, column 5: expected a value, found `@`"
        );
        assert_eq!(
            bad("a = "),
            "line 1, column 5: expected a value, found end of input"
        );
        assert_eq!(bad("a b = 1"), "line 1, column 3: expected `=` after a key");
        assert_eq!(bad("= 1"), "line 1, column 1: expected a key, found `=`");
        assert_eq!(
            bad("a = 1 b = 2"),
            "line 1, column 7: unexpected `b` after a value"
        );
        assert_eq!(
            bad("[a"),
            "line 1, column 3: expected `]` to close the table header"
        );
        assert_eq!(
            bad("[[a]"),
            "line 1, column 5: expected `]]` to close the array-of-tables header"
        );
        assert_eq!(
            bad("[a] junk"),
            "line 1, column 5: unexpected `j` after a table header"
        );
        assert_eq!(
            bad("a = [1 2]"),
            "line 1, column 8: expected `,` or `]` in an array, found `2`"
        );
        assert_eq!(bad("a = [1"), "line 1, column 5: unterminated array");
        assert_eq!(
            bad("a = { b = 1"),
            "line 1, column 5: unterminated inline table"
        );
        assert_eq!(
            bad("a = { b = 1, }"),
            "line 1, column 14: an inline table may not have a trailing comma"
        );
        assert_eq!(
            bad("a = { b = 1,\n c = 2 }"),
            "line 1, column 5: an inline table must be on one line"
        );
        assert_eq!(bad("[]"), "line 1, column 2: expected a key, found `]`");
    }

    #[test]
    fn nesting_is_bounded() {
        let deep = format!("a = {}1{}", "[".repeat(80), "]".repeat(80));
        assert!(bad(&deep).contains("nested too deeply"));
        let wide = format!("{}b = 1", "a.".repeat(80));
        assert!(bad(&wide).contains("nested too deeply"));
    }

    #[test]
    fn errors_report_line_and_column() {
        let e = parse("[ui]\npage_size = 25\nscrolloff = @\n").unwrap_err();
        assert_eq!(e.line(), Some(3));
        assert_eq!(e.column(), Some(13));
        assert_eq!(e.message(), "expected a value, found `@`");
        assert_eq!(
            e.to_string(),
            "line 3, column 13: expected a value, found `@`"
        );
    }

    // --- shipped configurations ------------------------------------------

    #[test]
    fn shipped_tmc_configuration() {
        // td-firstboot's TMC_CONFIG, verbatim.
        let doc = t(concat!(
            "# td mail (tmc). Provisioned on first boot; edit freely, it is never rewritten.\n",
            "# Paths are as the application sees them inside its jail. The client reads\n",
            "# this file when it starts.\n",
            "\n",
            "[account.main]\n",
            "well_known_url = \"https://mail.example.com/.well-known/jmap\"\n",
            "username = \"you@example.com\"\n",
            "password_file = \"/home/td/.config/tmc/password\"\n",
        ));
        assert_eq!(doc.table_keys(), vec!["account"]);
        let main = doc.get("account").and_then(|a| a.get("main")).unwrap();
        assert_eq!(
            main.require_str("well_known_url").unwrap(),
            "https://mail.example.com/.well-known/jmap"
        );
        assert_eq!(main.require_str("username").unwrap(), "you@example.com");
        assert_eq!(
            main.require_str("password_file").unwrap(),
            "/home/td/.config/tmc/password"
        );
        assert_eq!(main.optional_str("password_command").unwrap(), None);
    }

    #[test]
    fn shipped_tn_configuration() {
        // td-firstboot's TN_CONFIG, verbatim.
        let doc = t(concat!(
            "# td news (tn). Provisioned on first boot; edit freely, it is never rewritten.\n",
            "# The client reads this file when it starts. The feeds below are public\n",
            "# starting points: replace or delete them, and nothing is fetched until you\n",
            "# name a feed of your own.\n",
            "\n",
            "[[feed]]\n",
            "name = \"LWN\"\n",
            "url = \"https://lwn.net/headlines/rss\"\n",
            "\n",
            "[[feed]]\n",
            "name = \"Rust Blog\"\n",
            "url = \"https://blog.rust-lang.org/feed.xml\"\n",
        ));
        let feeds = doc.require_arr("feed").unwrap();
        assert_eq!(feeds.len(), 2);
        assert_eq!(feeds[0].require_str("name").unwrap(), "LWN");
        assert_eq!(
            feeds[0].require_str("url").unwrap(),
            "https://lwn.net/headlines/rss"
        );
        assert_eq!(feeds[1].require_str("name").unwrap(), "Rust Blog");
        assert_eq!(
            feeds[1].require_str("url").unwrap(),
            "https://blog.rust-lang.org/feed.xml"
        );
    }

    /// tmc's documented configuration, with every section the application
    /// reads.
    const TMC_FULL: &str = r##"
[ui]
editor = "nvim"
browser = "firefox"
page_size = 100
scrolloff = 3
mouse = true
sync_interval_secs = 60

[mail]
archive_folder = "Archive"
deleted_folder = "Trash"
reply_from = "Me <me@example.com>"
rules_mailbox_regex = '^INBOX$'
my_email_regex = '^me@example\.com$'

[spam]
enabled = true
threshold = 0.95
ham_threshold = 0.1
min_training = 50

[theme]
bg = "#002b36"
fg = "#839496"
header_fg = "#268bd2"

[retention.archive]
folder = "Archive"
days = 365

[retention.trash]
folder = "Trash"
days = 30

[account.personal]
well_known_url = "https://mx.example.com/.well-known/jmap"
username = "me@example.com"
password_command = "pass show email/example.com"

[account.work]
well_known_url = "https://mx.work.com/.well-known/jmap"
username = "me@work.com"
password_file = "/home/td/.config/tmc/work-password"
"##;

    #[test]
    fn tmc_full_configuration() {
        let doc = t(TMC_FULL);
        assert_eq!(
            doc.table_keys(),
            vec!["ui", "mail", "spam", "theme", "retention", "account"]
        );
        let ui = doc.require_table("ui").unwrap();
        assert_eq!(ui.require_str("editor").unwrap(), "nvim");
        assert_eq!(ui.require_u32("page_size").unwrap(), 100);
        assert_eq!(ui.require_usize("scrolloff").unwrap(), 3);
        assert!(ui.require_bool("mouse").unwrap());
        assert_eq!(ui.require_u64("sync_interval_secs").unwrap(), 60);

        let mail = doc.require_table("mail").unwrap();
        assert_eq!(mail.require_str("rules_mailbox_regex").unwrap(), "^INBOX$");
        assert_eq!(
            mail.require_str("my_email_regex").unwrap(),
            r"^me@example\.com$"
        );

        let spam = doc.require_table("spam").unwrap();
        assert_eq!(spam.require_float("threshold").unwrap(), 0.95);
        assert_eq!(spam.require_float("ham_threshold").unwrap(), 0.1);
        assert_eq!(spam.require_u32("min_training").unwrap(), 50);

        assert_eq!(
            doc.require_table("theme")
                .unwrap()
                .require_str("bg")
                .unwrap(),
            "#002b36"
        );

        let retention = doc.require_table("retention").unwrap();
        assert_eq!(retention.table_keys(), vec!["archive", "trash"]);
        assert_eq!(
            retention
                .require_table("trash")
                .unwrap()
                .require_u32("days")
                .unwrap(),
            30
        );

        let account = doc.require_table("account").unwrap();
        assert_eq!(account.table_keys(), vec!["personal", "work"]);
        let work = account.require_table("work").unwrap();
        assert_eq!(work.optional_str("password_command").unwrap(), None);
        assert_eq!(
            work.optional_str("password_file").unwrap(),
            Some("/home/td/.config/tmc/work-password")
        );
    }

    /// A rules file exercising every condition shape tmc compiles.
    const TMC_RULES: &str = r#"
# tmc mail rules

[[rule]]
name = "mark newsletters read"
continue_processing = true
[rule.match]
header = "From"
regex = "newsletter@"
[rule.actions]
mark_read = true

[[rule]]
name = "flag urgent mail from alice"
skip_if_to_me = false
[rule.match]
all = [
    { header = "From", regex = "alice@" },
    { header = "Subject", regex = "urgent" },
]
[rule.actions]
flag = true
mark_unread = true

[[rule]]
name = "move alerts"
[rule.match]
header = "Subject"
regex = "\\[ALERT\\]"
[rule.actions]
move_to = "INBOX/Alerts"

[[rule]]
name = "not from the boss"
[rule.match]
not = { header = "From", regex = "boss@" }
[rule.actions]
mark_read = true

[[rule]]
name = "spam"
[rule.match]
all = [
    { header = "X-Tmc-Spam-Verdict", regex = "^spam$" },
    { not = { any = [
        { header = "From", regex = "alice@" },
        { header = "X-Mailing-List", regex = "dev" },
    ] } },
]
[rule.actions]
delete = true
unflag = true
[rule.triage]
action = "trash"
confidence = 0.9
"#;

    #[test]
    fn tmc_rules_file() {
        let doc = t(TMC_RULES);
        let rules = doc.require_arr("rule").unwrap();
        assert_eq!(rules.len(), 5);

        assert_eq!(
            rules[0].require_str("name").unwrap(),
            "mark newsletters read"
        );
        assert!(rules[0].require_bool("continue_processing").unwrap());
        assert_eq!(rules[0].optional_bool("skip_if_to_me").unwrap(), None);
        let first = rules[0].require_table("match").unwrap();
        assert_eq!(first.require_str("header").unwrap(), "From");
        assert_eq!(first.require_str("regex").unwrap(), "newsletter@");
        assert!(rules[0]
            .require_table("actions")
            .unwrap()
            .require_bool("mark_read")
            .unwrap());

        let all = rules[1]
            .require_table("match")
            .unwrap()
            .require_arr("all")
            .unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].require_str("regex").unwrap(), "alice@");
        assert_eq!(all[1].require_str("header").unwrap(), "Subject");

        // A basic string keeps the regex escaping the rules engine needs.
        assert_eq!(
            rules[2]
                .require_table("match")
                .unwrap()
                .require_str("regex")
                .unwrap(),
            r"\[ALERT\]"
        );
        assert_eq!(
            rules[2]
                .require_table("actions")
                .unwrap()
                .require_str("move_to")
                .unwrap(),
            "INBOX/Alerts"
        );

        let not = rules[3]
            .require_table("match")
            .unwrap()
            .require_table("not")
            .unwrap();
        assert_eq!(not.require_str("header").unwrap(), "From");

        // all -> [header, not -> any -> [header, header]]
        let nested = rules[4]
            .require_table("match")
            .unwrap()
            .require_arr("all")
            .unwrap();
        assert_eq!(
            nested[0].require_str("header").unwrap(),
            "X-Tmc-Spam-Verdict"
        );
        let any = nested[1]
            .require_table("not")
            .unwrap()
            .require_arr("any")
            .unwrap();
        assert_eq!(any.len(), 2);
        assert_eq!(any[1].require_str("header").unwrap(), "X-Mailing-List");
        let triage = rules[4].require_table("triage").unwrap();
        assert_eq!(triage.require_str("action").unwrap(), "trash");
        assert_eq!(triage.require_float("confidence").unwrap(), 0.9);
    }

    #[test]
    fn tn_full_configuration() {
        let doc = t(concat!(
            "[ui]\n",
            "page_size = 50\n",
            "scrolloff = 3\n",
            "mouse = false\n",
            "sync_interval_secs = 600\n",
            "browser = \"firefox\"\n",
            "\n",
            "[theme]\n",
            "bg = \"#002b36\"\n",
            "fg = \"#839496\"\n",
            "\n",
            "[[feed]]\n",
            "name = \"HN\"\n",
            "url = \"https://news.ycombinator.com/rss\"\n",
            "\n",
            "[[feed]]\n",
            "name = \"LWN\"\n",
            "url = \"https://lwn.net/headlines/rss\"\n",
        ));
        let ui = doc.require_table("ui").unwrap();
        assert_eq!(ui.require_usize("page_size").unwrap(), 50);
        assert!(!ui.require_bool("mouse").unwrap());
        assert_eq!(ui.require_u64("sync_interval_secs").unwrap(), 600);
        assert_eq!(doc.require_arr("feed").unwrap().len(), 2);
        assert_eq!(
            doc.require_table("theme")
                .unwrap()
                .optional_str("bold_fg")
                .unwrap(),
            None
        );
    }

    // --- accessors and mapping helpers ------------------------------------

    #[test]
    fn accessors_read_each_type() {
        let doc = t("s = \"x\"\ni = 7\nf = 1.5\nb = true\na = [1]\n[tbl]\nk = 1\n");
        assert_eq!(doc.get("s").and_then(Toml::as_str), Some("x"));
        assert_eq!(doc.get("i").and_then(Toml::as_int), Some(7));
        assert_eq!(doc.get("f").and_then(Toml::as_float), Some(1.5));
        assert_eq!(doc.get("b").and_then(Toml::as_bool), Some(true));
        assert_eq!(
            doc.get("a").and_then(Toml::as_arr).map(<[Toml]>::len),
            Some(1)
        );
        assert_eq!(
            doc.get("tbl")
                .and_then(Toml::as_table)
                .map(<[(String, Toml)]>::len),
            Some(1)
        );
        assert!(doc.get("tbl").is_some_and(Toml::is_table));
        assert!(!doc.get("i").is_some_and(Toml::is_table));
        assert_eq!(doc.get("missing"), None);
        // A non-table has no keys and no fields.
        assert_eq!(doc.get("i").unwrap().get("x"), None);
        assert!(doc.get("i").unwrap().table_keys().is_empty());
        assert_eq!(doc.get("s").unwrap().type_name(), "string");
        assert_eq!(doc.get("tbl").unwrap().type_name(), "table");
    }

    #[test]
    fn an_integer_reads_as_a_float() {
        let doc = t("threshold = 1\nham = 0\n");
        assert_eq!(doc.require_float("threshold").unwrap(), 1.0);
        assert_eq!(doc.optional_float("ham").unwrap(), Some(0.0));
        // The reverse does not hold.
        assert!(t("x = 1.5").optional_int("x").is_err());
    }

    #[test]
    fn require_reports_missing_and_wrong_types() {
        let doc = t("page_size = \"lots\"\n");
        assert_eq!(
            doc.require_str("username").unwrap_err().to_string(),
            "missing field `username`"
        );
        assert_eq!(
            doc.require_int("page_size").unwrap_err().to_string(),
            "invalid type for `page_size`: expected an integer, found string"
        );
        assert_eq!(
            doc.optional_bool("page_size").unwrap_err().to_string(),
            "invalid type for `page_size`: expected a boolean, found string"
        );
        assert_eq!(
            doc.require_table("page_size").unwrap_err().to_string(),
            "invalid type for `page_size`: expected a table, found string"
        );
        assert_eq!(
            doc.require_arr("page_size").unwrap_err().to_string(),
            "invalid type for `page_size`: expected an array, found string"
        );
        assert_eq!(
            doc.require_float("page_size").unwrap_err().to_string(),
            "invalid type for `page_size`: expected a float, found string"
        );
        // A mapping error carries no position, so it wraps verbatim.
        assert_eq!(doc.require_str("nope").unwrap_err().line(), None);
    }

    #[test]
    fn optional_helpers_return_none_for_absent_keys() {
        let doc = t("a = 1\n");
        assert_eq!(doc.optional_str("b").unwrap(), None);
        assert_eq!(doc.optional_int("b").unwrap(), None);
        assert_eq!(doc.optional_float("b").unwrap(), None);
        assert_eq!(doc.optional_bool("b").unwrap(), None);
        assert_eq!(doc.optional_arr("b").unwrap(), None);
        assert_eq!(doc.optional_table("b").unwrap(), None);
        assert_eq!(doc.optional_u32("b").unwrap(), None);
        assert_eq!(doc.optional_u64("b").unwrap(), None);
        assert_eq!(doc.optional_usize("b").unwrap(), None);
        assert_eq!(doc.optional_int("a").unwrap(), Some(1));
    }

    #[test]
    fn unsigned_helpers_check_range() {
        let doc = t("days = 365\nnegative = -1\nhuge = 4294967296\n");
        assert_eq!(doc.require_u32("days").unwrap(), 365);
        assert_eq!(doc.optional_usize("days").unwrap(), Some(365));
        assert_eq!(
            doc.require_u32("negative").unwrap_err().to_string(),
            "invalid value for `negative`: expected an integer between 0 and 4294967295, found -1"
        );
        assert_eq!(
            doc.optional_u32("huge").unwrap_err().to_string(),
            "invalid value for `huge`: expected an integer between 0 and 4294967295, found 4294967296"
        );
        assert_eq!(doc.require_u64("huge").unwrap(), 4_294_967_296);
    }

    #[test]
    fn unknown_keys_drive_deny_unknown_fields() {
        let doc =
            t("[jmap]\nwell_known_url = \"u\"\nusername = \"n\"\nbogus = 1\nalso_bogus = 2\n");
        let jmap = doc.require_table("jmap").unwrap();
        let allowed = [
            "well_known_url",
            "username",
            "password_command",
            "password_file",
        ];
        assert_eq!(jmap.unknown_keys(&allowed), vec!["bogus", "also_bogus"]);
        assert!(doc.unknown_keys(&["jmap"]).is_empty());
        assert_eq!(
            jmap.check_known_keys(&allowed).unwrap_err().to_string(),
            "unknown field `bogus`, expected one of `well_known_url`, `username`, `password_command`, `password_file`"
        );
        assert!(jmap
            .check_known_keys(&["well_known_url", "username", "bogus", "also_bogus"])
            .is_ok());
    }

    #[test]
    fn serde_shaped_messages() {
        // The two texts the applications' tests already assert on.
        assert_eq!(
            missing_field_message("username"),
            "missing field `username`"
        );
        assert_eq!(
            Error::missing_field("username").to_string(),
            "missing field `username`"
        );
        assert_eq!(
            unknown_field_message("bogus", &["ui", "mail", "jmap"]),
            "unknown field `bogus`, expected one of `ui`, `mail`, `jmap`"
        );
        assert_eq!(
            unknown_field_message("bogus", &["ui", "mail"]),
            "unknown field `bogus`, expected `ui` or `mail`"
        );
        assert_eq!(
            unknown_field_message("bogus", &["ui"]),
            "unknown field `bogus`, expected `ui`"
        );
        assert_eq!(
            unknown_field_message("bogus", &[]),
            "unknown field `bogus`, there are no fields"
        );
        assert_eq!(
            Error::unknown_field("bogus", &["ui", "mail", "jmap"]).message(),
            "unknown field `bogus`, expected one of `ui`, `mail`, `jmap`"
        );
    }

    #[test]
    fn an_unknown_section_is_caught_at_the_root() {
        // tmc's `test_unknown_section_or_key_errors`, hand-mapped.
        let doc = t("[bogus]\nfoo = \"bar\"\n\n[jmap]\nusername = \"u\"\n");
        let allowed = [
            "ui",
            "mail",
            "jmap",
            "account",
            "retention",
            "spam",
            "theme",
        ];
        let message = doc.check_known_keys(&allowed).unwrap_err().to_string();
        assert!(message.contains("unknown field"), "got: {message}");
        assert!(message.contains("`bogus`"), "got: {message}");
    }

    // --- writer -----------------------------------------------------------

    #[test]
    fn values_round_trip_through_the_writer() {
        let doc = t(TMC_FULL);
        let text = doc.to_string();
        assert_eq!(parse(&text).unwrap(), doc);

        let rules = t(TMC_RULES);
        assert_eq!(parse(&rules.to_string()).unwrap(), rules);
    }

    #[test]
    fn the_writer_quotes_what_needs_quoting() {
        let doc = Toml::Table(vec![
            ("bare-key_1".to_string(), Toml::Int(1)),
            (
                "needs quoting".to_string(),
                Toml::Str("a\"b\\c\n\u{1}".to_string()),
            ),
            ("".to_string(), Toml::Bool(false)),
            (
                "nested".to_string(),
                Toml::Table(vec![(
                    "x".to_string(),
                    Toml::Arr(vec![Toml::Float(1.5), Toml::Float(f64::INFINITY)]),
                )]),
            ),
            ("empty".to_string(), Toml::Table(Vec::new())),
        ]);
        assert_eq!(
            doc.to_string(),
            concat!(
                "bare-key_1 = 1\n",
                "\"needs quoting\" = \"a\\\"b\\\\c\\n\\u0001\"\n",
                "\"\" = false\n",
                "nested = { x = [1.5, inf] }\n",
                "empty = {}\n",
            )
        );
        assert_eq!(parse(&doc.to_string()).unwrap(), doc);
    }

    #[test]
    fn the_writer_renders_single_values() {
        assert_eq!(Toml::Str("hi".to_string()).to_string(), "\"hi\"");
        assert_eq!(Toml::Int(-3).to_string(), "-3");
        assert_eq!(Toml::Float(2.0).to_string(), "2.0");
        assert_eq!(Toml::Float(f64::NEG_INFINITY).to_string(), "-inf");
        assert_eq!(Toml::Float(f64::NAN).to_string(), "nan");
        assert_eq!(Toml::Bool(true).to_string(), "true");
        assert_eq!(Toml::Arr(Vec::new()).to_string(), "[]");
    }
}

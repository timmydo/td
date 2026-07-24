//! A minimal, dependency-free JSON value, parser, and CANONICAL writer — the one
//! copy shared by td-builder and td-recipe-eval (each formerly carried its own).
//!
//! The recipe surface emits its declarations as JSON and compares them through
//! `to_canonical` — object keys SORTED, whitespace removed — so equality means
//! "same key set + same values" with no external JSON library. The builder reads
//! the recipe PHASE/STEP data the derivation contract hands it (DESIGN §7.1) via
//! the `as_str`/`as_arr`/`get`/`is_true` accessors and re-emits sub-trees with
//! `to_json_string` (an alias for the same sorted-key canonical writer, so the
//! builder's TD_PHASES/TD_INPUT_MAP bytes are unchanged by the merge).
//!
//! Numbers are kept as their raw lexeme (`Num(String)`) so re-serialisation is
//! exact (no f64 round-trip); recipe and phase JSON today carry only strings,
//! bools, arrays and objects.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned

#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Num(String),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

impl Json {
    /// The string value, if this is a `Str`.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::Str(s) => Some(s),
            _ => None,
        }
    }
    /// The array elements, if this is an `Arr`.
    pub fn as_arr(&self) -> Option<&[Json]> {
        match self {
            Json::Arr(v) => Some(v),
            _ => None,
        }
    }
    /// The value for `key`, if this is an `Obj` carrying it (first match — recipe
    /// and phase objects are dup-free).
    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Obj(o) => o.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }
    /// True iff this is exactly `Bool(true)`.
    pub fn is_true(&self) -> bool {
        matches!(self, Json::Bool(true))
    }

    /// Serialise to canonical form: object keys sorted ascending, compact.
    pub fn to_canonical(&self) -> String {
        let mut out = String::new();
        self.write_canonical(&mut out);
        out
    }

    /// Alias for `to_canonical` under the builder's historical name (both emit
    /// sorted-key compact JSON — kept so the builder's call sites and byte output
    /// are unchanged by folding the two former copies into one).
    pub fn to_json_string(&self) -> String {
        self.to_canonical()
    }

    fn write_canonical(&self, out: &mut String) {
        match self {
            Json::Null => out.push_str("null"),
            Json::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Json::Num(n) => out.push_str(n),
            Json::Str(s) => write_json_string(s, out),
            Json::Arr(a) => {
                out.push('[');
                for (i, v) in a.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    v.write_canonical(out);
                }
                out.push(']');
            }
            Json::Obj(o) => {
                let mut keys: Vec<&(String, Json)> = o.iter().collect();
                keys.sort_by(|a, b| a.0.cmp(&b.0));
                out.push('{');
                for (i, (k, v)) in keys.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write_json_string(k, out);
                    out.push(':');
                    v.write_canonical(out);
                }
                out.push('}');
            }
        }
    }
}

fn write_json_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// True iff `lex` is a well-formed JSON number (RFC 8259):
/// `-?(0|[1-9][0-9]*)(\.[0-9]+)?([eE][+-]?[0-9]+)?`.
fn valid_json_number(lex: &str) -> bool {
    let b = lex.as_bytes();
    let n = b.len();
    let mut i = 0usize;
    if i < n && b[i] == b'-' {
        i += 1;
    }
    // int part: a lone 0, or a non-zero digit run.
    match b.get(i) {
        Some(b'0') => i += 1,
        Some(c) if c.is_ascii_digit() => {
            while i < n && b[i].is_ascii_digit() {
                i += 1;
            }
        }
        _ => return false,
    }
    // fraction: '.' then >=1 digit.
    if i < n && b[i] == b'.' {
        i += 1;
        let s = i;
        while i < n && b[i].is_ascii_digit() {
            i += 1;
        }
        if i == s {
            return false;
        }
    }
    // exponent: e/E, optional sign, >=1 digit.
    if i < n && (b[i] == b'e' || b[i] == b'E') {
        i += 1;
        if i < n && (b[i] == b'+' || b[i] == b'-') {
            i += 1;
        }
        let s = i;
        while i < n && b[i].is_ascii_digit() {
            i += 1;
        }
        if i == s {
            return false;
        }
    }
    i == n
}

/// Parse a JSON document. Accepts the standard grammar; leading/trailing
/// whitespace is allowed, trailing content is an error.
pub fn parse(input: &str) -> Result<Json, String> {
    let mut p = Parser {
        b: input.as_bytes(),
        i: 0,
    };
    let v = p.value()?;
    p.skip_ws();
    if p.i != p.b.len() {
        return Err(format!("trailing content at byte {}", p.i));
    }
    Ok(v)
}

struct Parser<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Parser<'a> {
    fn skip_ws(&mut self) {
        while self.i < self.b.len() {
            match self.b[self.i] {
                b' ' | b'\t' | b'\n' | b'\r' => self.i += 1,
                _ => break,
            }
        }
    }

    fn value(&mut self) -> Result<Json, String> {
        self.skip_ws();
        if self.i >= self.b.len() {
            return Err("unexpected end of input".into());
        }
        match self.b[self.i] {
            b'{' => self.object(),
            b'[' => self.array(),
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
            c => Err(format!("unexpected byte '{}' at {}", c as char, self.i)),
        }
    }

    fn lit(&mut self, s: &str) -> Result<(), String> {
        if self.b[self.i..].starts_with(s.as_bytes()) {
            self.i += s.len();
            Ok(())
        } else {
            Err(format!("expected '{}' at {}", s, self.i))
        }
    }

    fn number(&mut self) -> Result<Json, String> {
        let start = self.i;
        if self.i < self.b.len() && self.b[self.i] == b'-' {
            self.i += 1;
        }
        while self.i < self.b.len()
            && matches!(self.b[self.i], b'0'..=b'9' | b'.' | b'e' | b'E' | b'+' | b'-')
        {
            self.i += 1;
        }
        let lex = std::str::from_utf8(&self.b[start..self.i]).map_err(|e| e.to_string())?;
        // Numbers are kept as their raw lexeme (never used as an f64), but a
        // malformed lexeme must NOT slip through and later re-emit as invalid
        // JSON — validate the RFC 8259 grammar and fail loudly (rejects `-`,
        // `1e`, `1..2`, `1+2`, …).
        if !valid_json_number(lex) {
            return Err(format!("malformed number `{lex}' at byte {start}"));
        }
        Ok(Json::Num(lex.to_string()))
    }

    fn string(&mut self) -> Result<String, String> {
        self.i += 1; // opening quote
        let mut out: Vec<u8> = Vec::new();
        loop {
            if self.i >= self.b.len() {
                return Err("unterminated string".into());
            }
            let c = self.b[self.i];
            self.i += 1;
            match c {
                b'"' => return String::from_utf8(out).map_err(|e| e.to_string()),
                b'\\' => {
                    if self.i >= self.b.len() {
                        return Err("bad escape at end of input".into());
                    }
                    let e = self.b[self.i];
                    self.i += 1;
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
                            let mut cp = self.hex4()? as u32;
                            if (0xD800..=0xDBFF).contains(&cp) {
                                // high surrogate: expect a \uXXXX low surrogate
                                if self.i + 1 < self.b.len()
                                    && self.b[self.i] == b'\\'
                                    && self.b[self.i + 1] == b'u'
                                {
                                    self.i += 2;
                                    let lo = self.hex4()? as u32;
                                    if !(0xDC00..=0xDFFF).contains(&lo) {
                                        return Err("bad low surrogate".into());
                                    }
                                    cp = 0x10000 + ((cp - 0xD800) << 10) + (lo - 0xDC00);
                                } else {
                                    return Err("unpaired high surrogate".into());
                                }
                            }
                            let ch = char::from_u32(cp).ok_or("invalid codepoint")?;
                            let mut buf = [0u8; 4];
                            out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                        }
                        _ => return Err("bad escape".into()),
                    }
                }
                _ => out.push(c),
            }
        }
    }

    fn hex4(&mut self) -> Result<u16, String> {
        if self.i + 4 > self.b.len() {
            return Err("short \\u escape".into());
        }
        let mut v: u16 = 0;
        for _ in 0..4 {
            let d = self.b[self.i];
            let n = match d {
                b'0'..=b'9' => d - b'0',
                b'a'..=b'f' => d - b'a' + 10,
                b'A'..=b'F' => d - b'A' + 10,
                _ => return Err("bad hex digit in \\u escape".into()),
            };
            v = v * 16 + n as u16;
            self.i += 1;
        }
        Ok(v)
    }

    fn array(&mut self) -> Result<Json, String> {
        self.i += 1; // [
        let mut a = Vec::new();
        self.skip_ws();
        if self.i < self.b.len() && self.b[self.i] == b']' {
            self.i += 1;
            return Ok(Json::Arr(a));
        }
        loop {
            a.push(self.value()?);
            self.skip_ws();
            if self.i >= self.b.len() {
                return Err("unterminated array".into());
            }
            match self.b[self.i] {
                b',' => {
                    self.i += 1;
                }
                b']' => {
                    self.i += 1;
                    return Ok(Json::Arr(a));
                }
                _ => return Err("expected ',' or ']' in array".into()),
            }
        }
    }

    fn object(&mut self) -> Result<Json, String> {
        self.i += 1; // {
        let mut o = Vec::new();
        self.skip_ws();
        if self.i < self.b.len() && self.b[self.i] == b'}' {
            self.i += 1;
            return Ok(Json::Obj(o));
        }
        loop {
            self.skip_ws();
            if self.i >= self.b.len() || self.b[self.i] != b'"' {
                return Err("expected object key string".into());
            }
            let k = self.string()?;
            self.skip_ws();
            if self.i >= self.b.len() || self.b[self.i] != b':' {
                return Err("expected ':' after object key".into());
            }
            self.i += 1;
            let v = self.value()?;
            // Both engine bins assume dup-free objects (Obj is an order-preserving
            // Vec, and `get` first-wins). The builder's old BTreeMap silently kept
            // the LAST duplicate; rather than pick a winner, reject dup keys so a
            // meaning change can't slip through unseen (fail loudly).
            if o.iter().any(|(existing, _)| *existing == k) {
                return Err(format!("duplicate object key `{k}'"));
            }
            o.push((k, v));
            self.skip_ws();
            if self.i >= self.b.len() {
                return Err("unterminated object".into());
            }
            match self.b[self.i] {
                b',' => {
                    self.i += 1;
                }
                b'}' => {
                    self.i += 1;
                    return Ok(Json::Obj(o));
                }
                _ => return Err("expected ',' or '}' in object".into()),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- canonical writer (recipe surface) ---
    #[test]
    fn canonical_sorts_keys_and_compacts() {
        let j = parse(r#"{ "b": 1, "a": [true, "x"] }"#).unwrap();
        assert_eq!(j.to_canonical(), r#"{"a":[true,"x"],"b":1}"#);
    }

    #[test]
    fn parses_escapes_and_round_trips_canonically() {
        // a newline inside a string is emitted as \n; canonical writer round-trips it.
        let j = parse(r#"{"to":"exit 77;\n","q":"a\"b"}"#).unwrap();
        assert_eq!(j.to_canonical(), r#"{"q":"a\"b","to":"exit 77;\n"}"#);
    }

    #[test]
    fn key_order_does_not_affect_canonical_equality() {
        let a = parse(r#"{"name":"hello","version":"1"}"#).unwrap();
        let b = parse(r#"{"version":"1","name":"hello"}"#).unwrap();
        assert_eq!(a.to_canonical(), b.to_canonical());
    }

    #[test]
    fn rejects_trailing_content() {
        assert!(parse(r#"{}x"#).is_err());
    }

    // --- builder accessors + to_json_string (phase/step reading) ---
    #[test]
    fn parses_nested_phase_shape() {
        let v = parse(r#"{"phases":[{"name":"p","body":[{"substitute":"f","clauses":[{"from":"/bin/sh","to":"sh"}]}]}]}"#).unwrap();
        let ph = v.get("phases").unwrap().as_arr().unwrap();
        assert_eq!(ph.len(), 1);
        let body = ph[0].get("body").unwrap().as_arr().unwrap();
        let sub = &body[0];
        assert_eq!(sub.get("substitute").unwrap().as_str(), Some("f"));
        let cl = sub.get("clauses").unwrap().as_arr().unwrap();
        assert_eq!(cl[0].get("from").unwrap().as_str(), Some("/bin/sh"));
        assert_eq!(cl[0].get("to").unwrap().as_str(), Some("sh"));
    }

    #[test]
    fn handles_escapes_and_arrays_and_bool() {
        let v = parse(r#"{"a":"x\ny","b":["1","2"],"c":true,"d":{"e":"f"}}"#).unwrap();
        assert_eq!(v.get("a").unwrap().as_str(), Some("x\ny"));
        assert_eq!(v.get("b").unwrap().as_arr().unwrap().len(), 2);
        assert!(v.get("c").unwrap().is_true());
        assert_eq!(v.get("d").unwrap().get("e").unwrap().as_str(), Some("f"));
    }

    #[test]
    fn rejects_malformed() {
        assert!(parse("{").is_err());
        assert!(parse(r#"{"k":}"#).is_err());
        assert!(parse(r#"{"k":1} junk"#).is_err());
    }

    #[test]
    fn to_json_string_is_sorted_canonical() {
        // The builder re-emits parsed sub-trees via to_json_string; keys must sort.
        let v = parse(r#"{"out":"/x","in":"/y"}"#).unwrap();
        assert_eq!(v.to_json_string(), r#"{"in":"/y","out":"/x"}"#);
    }

    // --- number grammar: valid lexemes round-trip, malformed ones fail loudly ---
    #[test]
    fn accepts_well_formed_numbers() {
        for s in [
            "0", "-0", "1", "-1", "42", "1.5", "-3.14", "1e10", "1E10", "1e+10",
            "1e-10", "0.5", "-2.5e-3", "123456789",
        ] {
            let v = parse(s).unwrap_or_else(|e| panic!("{s} should parse: {e}"));
            assert_eq!(v.to_canonical(), s, "number lexeme must round-trip verbatim");
        }
    }

    #[test]
    fn rejects_malformed_numbers() {
        for s in [
            "-", "1e", "1E+", "1..2", "1+2", "01", "-01", ".5", "1.", "1e1.0",
            "--1", "0x1", "1e--1",
        ] {
            assert!(parse(s).is_err(), "{s} is not a valid JSON number");
        }
    }

    #[test]
    fn rejects_duplicate_object_keys() {
        assert!(parse(r#"{"a":"1","a":"2"}"#).is_err());
        assert!(parse(r#"{"a":{"b":"1","b":"2"}}"#).is_err());
        // non-adjacent duplicate
        assert!(parse(r#"{"a":"1","b":"2","a":"3"}"#).is_err());
        // distinct keys still parse
        assert!(parse(r#"{"a":"1","b":"2"}"#).is_ok());
    }
}

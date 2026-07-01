//! A minimal, dependency-free JSON value, parser, and CANONICAL writer.
//!
//! The recipe surface emits its declarations as JSON (the wire format the Guile
//! lowering bridge already consumes from boa). For the migration oracle we must
//! compare a Rust recipe's JSON to boa's JSON for the same `.ts` recipe; boa's
//! `JSON.stringify` emits object keys in source-literal order, so a byte compare
//! would be brittle. Instead both sides are run through `to_canonical` — keys
//! SORTED, whitespace removed — so equality means "same key set + same values",
//! exactly what the bridge cares about, with no external JSON library.
//!
//! Numbers are kept as their raw lexeme (`Num(String)`) so re-serialisation is
//! exact (no f64 round-trip), though recipe JSON today carries only strings,
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
    /// Serialise to canonical form: object keys sorted ascending, compact.
    pub fn to_canonical(&self) -> String {
        let mut out = String::new();
        self.write_canonical(&mut out);
        out
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

/// Parse a JSON document. Accepts the standard grammar boa's `JSON.stringify`
/// emits; trailing whitespace is allowed, trailing content is an error.
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

    #[test]
    fn canonical_sorts_keys_and_compacts() {
        let j = parse(r#"{ "b": 1, "a": [true, "x"] }"#).unwrap();
        assert_eq!(j.to_canonical(), r#"{"a":[true,"x"],"b":1}"#);
    }

    #[test]
    fn parses_escapes_and_round_trips_canonically() {
        // boa emits a newline inside a string as \n; canonical writer round-trips it.
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
}

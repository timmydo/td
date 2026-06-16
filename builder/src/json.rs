//! A minimal, zero-dependency JSON reader — just enough to parse the recipe
//! PHASE data `system/td-build.scm` hands the builder (DESIGN §7.1; the move
//! toward td's own tooling: td's builder INTERPRETS the recipe's phases itself,
//! rather than Guile pre-translating them). In keeping with td-builder's
//! hand-rolled style (ATerm, NAR, SQLite, SHA-256 are all dependency-free), this
//! avoids pulling a JSON crate.
//!
//! Supports the subset the phase DSL emits: objects, arrays, strings (with the
//! standard escapes), `true`/`false`/`null`. Numbers are parsed but unused. Fails
//! loudly on malformed input — a mis-parsed phase must not silently build something
//! else.

use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Json>),
    Obj(BTreeMap<String, Json>),
}

impl Json {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::Str(s) => Some(s),
            _ => None,
        }
    }
    pub fn as_arr(&self) -> Option<&[Json]> {
        match self {
            Json::Arr(v) => Some(v),
            _ => None,
        }
    }
    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Obj(m) => m.get(key),
            _ => None,
        }
    }
    pub fn is_true(&self) -> bool {
        matches!(self, Json::Bool(true))
    }
}

struct Parser<'a> {
    b: &'a [u8],
    i: usize,
}

pub fn parse(s: &str) -> Result<Json, String> {
    let mut p = Parser { b: s.as_bytes(), i: 0 };
    p.ws();
    let v = p.value()?;
    p.ws();
    if p.i != p.b.len() {
        return Err(format!("trailing data at byte {}", p.i));
    }
    Ok(v)
}

impl<'a> Parser<'a> {
    fn ws(&mut self) {
        while self.i < self.b.len() && matches!(self.b[self.i], b' ' | b'\t' | b'\n' | b'\r') {
            self.i += 1;
        }
    }
    fn peek(&self) -> Result<u8, String> {
        self.b.get(self.i).copied().ok_or_else(|| "unexpected end of JSON".to_string())
    }
    fn value(&mut self) -> Result<Json, String> {
        match self.peek()? {
            b'{' => self.object(),
            b'[' => self.array(),
            b'"' => Ok(Json::Str(self.string()?)),
            b't' => self.lit("true", Json::Bool(true)),
            b'f' => self.lit("false", Json::Bool(false)),
            b'n' => self.lit("null", Json::Null),
            c if c == b'-' || c.is_ascii_digit() => self.number(),
            c => Err(format!("unexpected byte '{}' at {}", c as char, self.i)),
        }
    }
    fn lit(&mut self, word: &str, v: Json) -> Result<Json, String> {
        if self.b[self.i..].starts_with(word.as_bytes()) {
            self.i += word.len();
            Ok(v)
        } else {
            Err(format!("expected `{word}' at {}", self.i))
        }
    }
    fn number(&mut self) -> Result<Json, String> {
        let start = self.i;
        while self.i < self.b.len()
            && matches!(self.b[self.i], b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9')
        {
            self.i += 1;
        }
        std::str::from_utf8(&self.b[start..self.i])
            .ok()
            .and_then(|s| s.parse::<f64>().ok())
            .map(Json::Num)
            .ok_or_else(|| format!("bad number at {start}"))
    }
    fn string(&mut self) -> Result<String, String> {
        debug_assert_eq!(self.b[self.i], b'"');
        self.i += 1;
        let mut out = String::new();
        loop {
            let c = self.peek()?;
            self.i += 1;
            match c {
                b'"' => return Ok(out),
                b'\\' => {
                    let e = self.peek()?;
                    self.i += 1;
                    match e {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'n' => out.push('\n'),
                        b't' => out.push('\t'),
                        b'r' => out.push('\r'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'u' => {
                            let hex = self.b.get(self.i..self.i + 4)
                                .ok_or("truncated \\u escape")?;
                            let code = u32::from_str_radix(
                                std::str::from_utf8(hex).map_err(|_| "bad \\u")?,
                                16,
                            )
                            .map_err(|_| "bad \\u hex")?;
                            self.i += 4;
                            out.push(char::from_u32(code).ok_or("bad \\u codepoint")?);
                        }
                        other => return Err(format!("bad escape \\{}", other as char)),
                    }
                }
                // Raw UTF-8 byte (multi-byte chars pass through verbatim).
                _ => out.push(c as char),
            }
        }
    }
    fn array(&mut self) -> Result<Json, String> {
        self.i += 1; // [
        let mut v = Vec::new();
        self.ws();
        if self.peek()? == b']' {
            self.i += 1;
            return Ok(Json::Arr(v));
        }
        loop {
            self.ws();
            v.push(self.value()?);
            self.ws();
            match self.peek()? {
                b',' => self.i += 1,
                b']' => {
                    self.i += 1;
                    return Ok(Json::Arr(v));
                }
                c => return Err(format!("expected ',' or ']' at {}, got '{}'", self.i, c as char)),
            }
        }
    }
    fn object(&mut self) -> Result<Json, String> {
        self.i += 1; // {
        let mut m = BTreeMap::new();
        self.ws();
        if self.peek()? == b'}' {
            self.i += 1;
            return Ok(Json::Obj(m));
        }
        loop {
            self.ws();
            if self.peek()? != b'"' {
                return Err(format!("expected object key string at {}", self.i));
            }
            let k = self.string()?;
            self.ws();
            if self.peek()? != b':' {
                return Err(format!("expected ':' after key at {}", self.i));
            }
            self.i += 1;
            self.ws();
            let val = self.value()?;
            m.insert(k, val);
            self.ws();
            match self.peek()? {
                b',' => self.i += 1,
                b'}' => {
                    self.i += 1;
                    return Ok(Json::Obj(m));
                }
                c => return Err(format!("expected ',' or '}}' at {}, got '{}'", self.i, c as char)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}

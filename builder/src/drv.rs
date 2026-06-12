//! ATerm `.drv` parser, matching the pinned daemon's grammar exactly
//! (nix/libstore/derivations.cc parseDerivation + nix/libutil/util.cc
//! parseString, read off the pin and recorded in plan/td-builder.md):
//!   - Derive([outputs],[inputDrvs],[inputSrcs],system,builder,[args],[env]);
//!   - an output is (name,path,hashAlgo,hash); an input drv is (path,[names]);
//!   - strings are double-quoted; after a backslash, `n`/`r`/`t` decode to
//!     LF/CR/TAB and ANY other character stands for itself (covers \" and \\);
//!   - lists separate/terminate on `,` / `]` with no whitespace anywhere;
//!   - paths must start with `/`.
//! Deviations are fail-closed only: the daemon ignores trailing bytes after
//! the final `)` and accepts non-UTF-8 string contents; we refuse both with
//! a positioned error (every real drv at the pin is UTF-8 with no trailer).
//! Representation: the daemon stores outputs/env in sorted maps and input
//! names in sets (dedup, last-wins); we keep file order with duplicates —
//! identical for real drvs, which are emitted from those sorted structures.

/// One (name,path,hashAlgo,hash) output entry, in file order.
#[derive(Debug)]
pub struct Output {
    pub name: String,
    pub path: String,
    pub hash_algo: String,
    pub hash: String,
}

#[derive(Debug)]
pub struct Derivation {
    pub outputs: Vec<Output>,
    pub input_drvs: Vec<(String, Vec<String>)>,
    pub input_srcs: Vec<String>,
    pub platform: String,
    pub builder: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

#[derive(Debug)]
pub struct ParseError {
    pub offset: usize,
    pub what: String,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "byte {}: {}", self.offset, self.what)
    }
}

struct Parser<'a> {
    input: &'a [u8],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn fail<T>(&self, what: impl Into<String>) -> Result<T, ParseError> {
        Err(ParseError { offset: self.pos, what: what.into() })
    }

    fn next(&mut self) -> Result<u8, ParseError> {
        match self.input.get(self.pos) {
            Some(&c) => {
                self.pos += 1;
                Ok(c)
            }
            None => self.fail("unexpected end of input"),
        }
    }

    fn expect(&mut self, lit: &str) -> Result<(), ParseError> {
        if self.input[self.pos..].starts_with(lit.as_bytes()) {
            self.pos += lit.len();
            Ok(())
        } else {
            self.fail(format!("expected `{lit}`"))
        }
    }

    /// The daemon's endOfList: consume `,` -> more elements, `]` -> done;
    /// anything else (the FIRST element of a non-empty list) is "not the
    /// end", consumed by the element parser that follows.
    fn end_of_list(&mut self) -> Result<bool, ParseError> {
        match self.input.get(self.pos) {
            Some(b',') => {
                self.pos += 1;
                Ok(false)
            }
            Some(b']') => {
                self.pos += 1;
                Ok(true)
            }
            Some(_) => Ok(false),
            None => self.fail("unexpected end of input in list"),
        }
    }

    fn string(&mut self) -> Result<String, ParseError> {
        let start = self.pos;
        self.expect("\"")?;
        let mut bytes = Vec::new();
        loop {
            match self.next()? {
                b'"' => break,
                b'\\' => match self.next()? {
                    b'n' => bytes.push(b'\n'),
                    b'r' => bytes.push(b'\r'),
                    b't' => bytes.push(b'\t'),
                    c => bytes.push(c),
                },
                c => bytes.push(c),
            }
        }
        match String::from_utf8(bytes) {
            Ok(s) => Ok(s),
            Err(_) => Err(ParseError {
                offset: start,
                what: "non-UTF-8 string contents".into(),
            }),
        }
    }

    fn path(&mut self) -> Result<String, ParseError> {
        let start = self.pos;
        let s = self.string()?;
        if s.starts_with('/') {
            Ok(s)
        } else {
            Err(ParseError {
                offset: start,
                what: format!("bad path `{s}' in derivation"),
            })
        }
    }

    fn strings(&mut self, are_paths: bool) -> Result<Vec<String>, ParseError> {
        let mut res = Vec::new();
        while !self.end_of_list()? {
            res.push(if are_paths { self.path()? } else { self.string()? });
        }
        Ok(res)
    }
}

/// Parse the full ATerm text of a `.drv` file.
pub fn parse(input: &[u8]) -> Result<Derivation, ParseError> {
    let mut p = Parser { input, pos: 0 };
    p.expect("Derive([")?;

    let mut outputs = Vec::new();
    while !p.end_of_list()? {
        p.expect("(")?;
        let name = p.string()?;
        p.expect(",")?;
        let path = p.path()?;
        p.expect(",")?;
        let hash_algo = p.string()?;
        p.expect(",")?;
        let hash = p.string()?;
        p.expect(")")?;
        outputs.push(Output { name, path, hash_algo, hash });
    }

    let mut input_drvs = Vec::new();
    p.expect(",[")?;
    while !p.end_of_list()? {
        p.expect("(")?;
        let drv_path = p.path()?;
        p.expect(",[")?;
        let names = p.strings(false)?;
        p.expect(")")?;
        input_drvs.push((drv_path, names));
    }

    p.expect(",[")?;
    let input_srcs = p.strings(true)?;
    p.expect(",")?;
    let platform = p.string()?;
    p.expect(",")?;
    let builder = p.path()?;

    let mut args = Vec::new();
    p.expect(",[")?;
    while !p.end_of_list()? {
        args.push(p.string()?);
    }

    let mut env = Vec::new();
    p.expect(",[")?;
    while !p.end_of_list()? {
        p.expect("(")?;
        let name = p.string()?;
        p.expect(",")?;
        let value = p.string()?;
        p.expect(")")?;
        env.push((name, value));
    }
    p.expect(")")?;
    if p.pos != input.len() {
        return p.fail("trailing bytes after the closing `)`");
    }

    Ok(Derivation { outputs, input_drvs, input_srcs, platform, builder, args, env })
}

/// Escape a value for the one-line `drv-parse` dump (the ATerm escapes,
/// so the dump round-trips visually even for multi-line env values).
fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out
}

/// The canonical line-based dump `td-builder drv-parse` prints.
pub fn dump(drv: &Derivation) -> String {
    let mut out = String::new();
    for o in &drv.outputs {
        out.push_str(&format!(
            "output {} {} {} {}\n",
            o.name,
            o.path,
            if o.hash_algo.is_empty() { "-" } else { &o.hash_algo },
            if o.hash.is_empty() { "-" } else { &o.hash }
        ));
    }
    for (path, names) in &drv.input_drvs {
        out.push_str(&format!("input-drv {} {}\n", path, names.join(",")));
    }
    for s in &drv.input_srcs {
        out.push_str(&format!("input-src {s}\n"));
    }
    out.push_str(&format!("platform {}\n", drv.platform));
    out.push_str(&format!("builder {}\n", drv.builder));
    for a in &drv.args {
        out.push_str(&format!("arg {}\n", escape(a)));
    }
    for (k, v) in &drv.env {
        out.push_str(&format!("env {}={}\n", escape(k), escape(v)));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // A miniature but complete drv exercising every section, shaped like the
    // pinned channel's output (single quotes swapped in for readability).
    fn sample() -> String {
        concat!(
            r#"Derive([("out","/gnu/store/aaa-x","","")],"#,
            r#"[("/gnu/store/bbb-dep.drv",["lib","out"])],"#,
            r#"["/gnu/store/ccc-src"],"x86_64-linux","/gnu/store/ddd-guile/bin/guile","#,
            r#"["--no-auto-compile","/gnu/store/ccc-src"],"#,
            r#"[("out","/gnu/store/aaa-x"),("allowSubstitutes","0")])"#
        )
        .to_string()
    }

    #[test]
    fn parses_every_section() {
        let drv = parse(sample().as_bytes()).unwrap();
        assert_eq!(drv.outputs.len(), 1);
        assert_eq!(drv.outputs[0].name, "out");
        assert_eq!(drv.outputs[0].path, "/gnu/store/aaa-x");
        assert_eq!(drv.outputs[0].hash_algo, "");
        assert_eq!(drv.input_drvs, vec![("/gnu/store/bbb-dep.drv".to_string(),
                                         vec!["lib".to_string(), "out".to_string()])]);
        assert_eq!(drv.input_srcs, vec!["/gnu/store/ccc-src"]);
        assert_eq!(drv.platform, "x86_64-linux");
        assert_eq!(drv.builder, "/gnu/store/ddd-guile/bin/guile");
        assert_eq!(drv.args, vec!["--no-auto-compile", "/gnu/store/ccc-src"]);
        assert_eq!(drv.env[0], ("out".to_string(), "/gnu/store/aaa-x".to_string()));
        assert_eq!(drv.env[1], ("allowSubstitutes".to_string(), "0".to_string()));
    }

    #[test]
    fn decodes_the_daemon_escapes() {
        // \n \r \t decode; after any other backslash the char stands for
        // itself — the daemon's parseString `else res += c` branch.
        let txt = r#"Derive([("out","/gnu/store/aaa-x","","")],[],[],"s","/b",[],[("v","a\nb\rc\td\"e\\f\qg")])"#;
        let drv = parse(txt.as_bytes()).unwrap();
        assert_eq!(drv.env[0].1, "a\nb\rc\td\"e\\fqg");
    }

    #[test]
    fn empty_lists_parse() {
        let txt = r#"Derive([("out","/gnu/store/aaa-x","","")],[],[],"s","/b",[],[])"#;
        let drv = parse(txt.as_bytes()).unwrap();
        assert!(drv.input_drvs.is_empty());
        assert!(drv.input_srcs.is_empty());
        assert!(drv.args.is_empty());
        assert!(drv.env.is_empty());
    }

    #[test]
    fn rejects_malformed_inputs() {
        // Wrong head keyword.
        assert!(parse(b"Derivf([],[],[],\"s\",\"/b\",[],[])").is_err());
        // Truncated mid-string.
        assert!(parse(br#"Derive([("out","/gnu/sto"#).is_err());
        // A non-absolute path where a path is required.
        let bad = r#"Derive([("out","gnu/store/aaa-x","","")],[],[],"s","/b",[],[])"#;
        let err = parse(bad.as_bytes()).unwrap_err();
        assert!(err.what.contains("bad path"));
        // Trailing garbage after the final `)` (fail-closed deviation).
        let trailer = format!("{}x", sample());
        assert!(parse(trailer.as_bytes()).unwrap_err().what.contains("trailing"));
    }

    #[test]
    fn dump_is_stable_and_escaped() {
        let drv = parse(sample().as_bytes()).unwrap();
        let d = dump(&drv);
        assert!(d.starts_with("output out /gnu/store/aaa-x - -\n"));
        assert!(d.contains("input-drv /gnu/store/bbb-dep.drv lib,out\n"));
        assert!(d.contains("platform x86_64-linux\n"));
        // Multi-line env values stay one dump line.
        let txt = r#"Derive([("out","/gnu/store/aaa-x","","")],[],[],"s","/b",[],[("v","a\nb")])"#;
        let d = dump(&parse(txt.as_bytes()).unwrap());
        assert!(d.contains("env v=a\\nb\n"));
    }
}

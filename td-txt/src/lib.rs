//! td-txt conformance harness — zero-dependency readers for the corpus in
//! `../spec`, a runner that executes the built `td-txt` multicall against each
//! case, and the xfail/skip overlay that keeps the gate green while td-txt grows.
//!
//! This is HOST-SIDE test tooling, not part of the shipped `td-txt` binary (the
//! recipe compiles `src/main.rs` and its sibling modules alone).
//!
//! # Why the corpus looks like this
//!
//! td-sh's oracle works because Oils' spec files are self-contained cases with
//! per-shell golden output already recorded — no external oracle binary needed in
//! an offline sandbox. `grep` and `sed` have no such single corpus, but they have
//! something better: their upstream test suites are DATA, not scripts, and each
//! datum already carries its expected result. So this harness reads three formats
//! and normalizes them into one `Case` — argv, input files, stdin, expected
//! stdout/stderr/status:
//!
//! 1. **Spencer `@`-separated rows** (`spec/gnu-grep/{bre,ere,spencer1}.tests`,
//!    vendored pristine from GNU grep). One row is `<status>@<pattern>@<input>`:
//!    run `grep [-E] -e <pattern>` with `<input>` on stdin, and the exit status
//!    must be `<status>`. A row with a fourth field is upstream's own "expected
//!    non-conformance" marker — upstream's awk driver does not assert it, and
//!    neither does this reader.
//! 2. **GNU sed triples** (`spec/gnu-sed/<name>.{sed,inp,good}`, vendored
//!    pristine): run `sed -f <name>.sed < <name>.inp`, and stdout must equal
//!    `<name>.good` byte for byte.
//! 3. **td-txt's own case files** (`spec/*.test.txt`), for everything a data
//!    corpus cannot express — the option surface, diagnostics, exit statuses,
//!    `-i` file rewriting. The format is Oils-shaped so the two harnesses read
//!    alike:
//!
//! ```text
//!   #### <description>        begins a case
//!   ## argv: grep -c foo      the command; POSIX-ish quoting, argv[0] = applet
//!   ## status: 1              a single-line assertion
//!   ## stdin:  …  ## END      a verbatim block (stdin/stdout/stderr)
//!   ## stdout-json: "a\n"     a block's exact bytes, for no-trailing-newline
//!   ## stderr-json: ""        likewise for stderr; "" asserts it stayed silent
//!   ## stderr-contains: sed:  a substring of stderr, for diagnostic wording
//!   ## file f.txt:  …  ## END an input file to materialize in the case dir
//!   ## file-json f.txt: "a\0"     that file's exact bytes (NUL, no newline, …)
//!   ## file-after f.txt: … ## END   that file's REQUIRED content afterwards
//!   ## no-file-after: f.txt   that file must NOT exist afterwards
//!   ## env NAME: value        added to the otherwise-cleared environment
//!   ## env NAME:  … ## END    the block form, and the only way to set it EMPTY
//!   #  (single hash)          a comment
//! ```
//!
//! Every case runs the multicall through a `<applet> -> td-txt` symlink, so
//! argv[0] dispatch — the way the image's `/bin/grep` will reach it — is what all
//! ~700 cases exercise, rather than one dedicated test.

#![deny(unsafe_code)]

use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Per-case wall-clock cap. A regex corpus contains patterns a backtracking
/// engine can spend a long time on; without this, one case would wedge the
/// shared land-on-green gate.
const CASE_TIMEOUT: Duration = Duration::from_secs(10);

/// Grace to collect a drained stream after the child exited or was killed.
const DRAIN_GRACE: Duration = Duration::from_secs(2);

/// A parse error, anchored to a 1-based source line.
#[derive(Debug)]
pub struct SpecError {
    pub line: usize,
    pub msg: String,
}

impl SpecError {
    fn new(line: usize, msg: impl Into<String>) -> Self {
        Self { line, msg: msg.into() }
    }
}

impl std::fmt::Display for SpecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "corpus parse error at line {}: {}", self.line, self.msg)
    }
}

impl std::error::Error for SpecError {}

/// What a case asserts about one output stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Stream {
    /// Byte for byte.
    Exact(Vec<u8>),
    /// Contains this text — for a diagnostic whose exact wording is not the
    /// contract (a `strerror` string differs across libcs).
    Contains(Vec<u8>),
}

/// What a case expects. `None` means "not asserted".
#[derive(Clone, Debug, Default)]
pub struct Expect {
    pub status: Option<i32>,
    pub stdout: Option<Vec<u8>>,
    pub stderr: Option<Stream>,
    /// Files whose content is checked after the run (`sed -i`, `s///w`).
    pub files_after: Vec<(String, Vec<u8>)>,
    /// Files that must NOT exist after the run. The complement of `files_after`,
    /// and not reachable through it: a `w` target GNU refuses to open differs from
    /// one it opens and never writes to only by whether the file is THERE.
    pub no_files_after: Vec<String>,
}

/// One executable conformance case, normalized from whichever corpus format it
/// came from.
#[derive(Clone, Debug)]
pub struct Case {
    /// Corpus-relative source, e.g. `gnu-grep/bre.tests` — the overlay key's
    /// first half.
    pub file: String,
    pub name: String,
    /// argv[0] is the applet name; the runner execs it through a symlink.
    pub argv: Vec<Vec<u8>>,
    /// Files to materialize in the case's working directory before the run.
    pub files: Vec<(String, Vec<u8>)>,
    /// Added to the otherwise-cleared environment. An applet that reads one is
    /// a case the argv alone cannot express.
    pub env: Vec<(String, Vec<u8>)>,
    pub stdin: Vec<u8>,
    pub expect: Expect,
}

/// The result of running one case.
#[derive(Clone, Debug)]
pub struct CaseOutcome {
    pub passed: bool,
    pub detail: Option<String>,
    pub timed_out: bool,
}

// ---- the native case format ----------------------------------------------

/// Split a `##` annotation into its key and the text after the colon. `None` for
/// the value means the annotation opens a block.
fn split_annotation(content: &str) -> Option<(String, Option<String>)> {
    let (key, rest) = content.split_once(':')?;
    let rest = rest.trim();
    let key = key.trim().to_string();
    if rest.is_empty() {
        return Some((key, None));
    }
    Some((key, Some(rest.to_string())))
}

/// Read a verbatim block: every line until one that is exactly `## END`.
fn read_block(lines: &[&str], start: usize) -> Result<(Vec<u8>, usize), SpecError> {
    let mut out: Vec<u8> = Vec::new();
    let mut i = start;
    loop {
        let Some(line) = lines.get(i) else {
            return Err(SpecError::new(start, "block is not terminated by `## END'"));
        };
        if line.trim_end() == "## END" {
            return Ok((out, i + 1));
        }
        out.extend_from_slice(line.as_bytes());
        out.push(b'\n');
        i += 1;
    }
}

/// The byte two hex digits spell, for the `\xHH` both escape tables use. BOTH
/// digits are required and both must be HEX: `from_str_radix` alone accepts a
/// leading `+`, so `\x+4` would silently mean 0x04 -- the same "spells something
/// else" trap the letter escapes are refused for.
fn hex_byte(hex: &[u8]) -> Option<u8> {
    if hex.len() != 2 || !hex.iter().all(u8::is_ascii_hexdigit) {
        return None;
    }
    let text = std::str::from_utf8(hex).ok()?;
    u8::from_str_radix(text, 16).ok()
}

/// Decode a JSON string literal (the `-json` annotation forms). Hand-rolled: the
/// crate carries no dependencies.
fn json_decode(value: &str, line: usize) -> Result<Vec<u8>, SpecError> {
    let chars: Vec<char> = value.trim().chars().collect();
    if chars.first() != Some(&'"') || chars.last() != Some(&'"') || chars.len() < 2 {
        return Err(SpecError::new(line, "a -json value must be a quoted string"));
    }
    let mut out: Vec<u8> = Vec::new();
    let mut i = 1usize;
    let end = chars.len() - 1;
    while i < end {
        let Some(c) = chars.get(i).copied() else { break };
        i += 1;
        if c != '\\' {
            let mut buf = [0u8; 4];
            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            continue;
        }
        let Some(esc) = chars.get(i).copied() else {
            return Err(SpecError::new(line, "trailing backslash in a -json value"));
        };
        i += 1;
        match esc {
            'n' => out.push(b'\n'),
            't' => out.push(b'\t'),
            'r' => out.push(b'\r'),
            '0' => out.push(0),
            '"' => out.push(b'"'),
            '\\' => out.push(b'\\'),
            'x' => {
                let hex: String = chars.get(i..i + 2).unwrap_or_default().iter().collect();
                i += 2;
                let byte = hex_byte(hex.as_bytes())
                    .ok_or_else(|| SpecError::new(line, "bad \\xHH escape in a -json value"))?;
                out.push(byte);
            }
            // JSON's own byte escape. Restricted to \u00XX: this corpus holds
            // BYTES, and a code point above 0xFF has no unambiguous byte
            // spelling here.
            'u' => {
                let hex: String = chars.get(i..i + 4).unwrap_or_default().iter().collect();
                let value = u32::from_str_radix(&hex, 16)
                    .map_err(|_| SpecError::new(line, "bad \\uXXXX escape in a -json value"))?;
                let byte = u8::try_from(value).map_err(|_| {
                    SpecError::new(line, "\\uXXXX above \\u00ff is not a single byte")
                })?;
                i += 4;
                out.push(byte);
            }
            other => return Err(SpecError::new(line, format!("unknown escape \\{other}"))),
        }
    }
    Ok(out)
}

/// POSIX-ish argv tokenizer: whitespace splits, single quotes are literal, double
/// quotes take `\` escapes, and a bare `\` escapes the next byte.
fn tokenize(text: &str, line: usize) -> Result<Vec<Vec<u8>>, SpecError> {
    let bytes = text.as_bytes();
    let mut argv: Vec<Vec<u8>> = Vec::new();
    let mut cur: Vec<u8> = Vec::new();
    let mut started = false;
    let mut i = 0usize;
    while let Some(b) = bytes.get(i).copied() {
        i += 1;
        match b {
            b' ' | b'\t' => {
                if started {
                    argv.push(std::mem::take(&mut cur));
                    started = false;
                }
            }
            b'\'' => {
                started = true;
                loop {
                    let Some(c) = bytes.get(i).copied() else {
                        return Err(SpecError::new(line, "unterminated single quote in argv"));
                    };
                    i += 1;
                    if c == b'\'' {
                        break;
                    }
                    cur.push(c);
                }
            }
            b'"' => {
                started = true;
                loop {
                    let Some(c) = bytes.get(i).copied() else {
                        return Err(SpecError::new(line, "unterminated double quote in argv"));
                    };
                    i += 1;
                    if c == b'"' {
                        break;
                    }
                    if c == b'\\' {
                        let Some(esc) = bytes.get(i).copied() else {
                            return Err(SpecError::new(line, "trailing backslash in argv"));
                        };
                        i += 1;
                        cur.push(match esc {
                            b'n' => b'\n',
                            b't' => b'\t',
                            // The `-json` table's third control byte. Without it
                            // `"\r"` is the letter, which a case asserting a CR
                            // would pass while testing something else.
                            b'r' => b'\r',
                            // `\xHH`, spelled as the `-json` table spells it. An
                            // argv byte above 0x7f has no other spelling here:
                            // this line reaches the tokenizer as UTF-8 text, so a
                            // raw one could not survive being read.
                            b'x' => {
                                let hex = bytes.get(i..i + 2).unwrap_or_default();
                                i += 2;
                                hex_byte(hex).ok_or_else(|| {
                                    SpecError::new(line, "bad \\xHH escape in argv")
                                })?
                            }
                            // Punctuation escapes ITSELF here (`\\`, `\"`, `\$`),
                            // which is the shell rule this borrows. A LETTER or
                            // DIGIT does not: it is the shape every escape that
                            // means something has, so `\0` or `\x41` silently
                            // spelling `0` or `x41` is the same trap `\r` was.
                            c if c.is_ascii_alphanumeric() => {
                                return Err(SpecError::new(
                                    line,
                                    format!("unknown escape \\{} in argv", char::from(c)),
                                ))
                            }
                            other => other,
                        });
                        continue;
                    }
                    cur.push(c);
                }
            }
            b'\\' => {
                started = true;
                let Some(esc) = bytes.get(i).copied() else {
                    return Err(SpecError::new(line, "trailing backslash in argv"));
                };
                i += 1;
                cur.push(esc);
            }
            _ => {
                started = true;
                cur.push(b);
            }
        }
    }
    if started {
        argv.push(cur);
    }
    if argv.is_empty() {
        return Err(SpecError::new(line, "`## argv:' is empty"));
    }
    // argv cannot hold a NUL -- `execve` takes NUL-terminated strings -- and a
    // case that asked for one would pass the well-formedness check and then fail
    // at SPAWN, which aborts the whole run instead of failing that one case.
    // Refused where it is written, which is also what makes "only a `-f` script
    // can carry a NUL into a diagnostic" true of the corpus and not just of a
    // shell.
    if argv.iter().any(|a| a.contains(&0)) {
        return Err(SpecError::new(line, "a NUL cannot go in argv"));
    }
    Ok(argv)
}

/// Parse one `*.test.txt` file.
pub fn parse_cases(text: &str, file: &str) -> Result<Vec<Case>, SpecError> {
    let lines: Vec<&str> = text.lines().collect();
    let mut cases: Vec<Case> = Vec::new();
    let mut cur: Option<Case> = None;
    let mut i = 0usize;
    while let Some(raw) = lines.get(i).copied() {
        let lineno = i + 1;
        i += 1;
        let trimmed = raw.trim_end();
        if let Some(desc) = trimmed.strip_prefix("####") {
            if let Some(case) = cur.take() {
                cases.push(case);
            }
            cur = Some(Case {
                file: file.to_string(),
                name: desc.trim().to_string(),
                argv: Vec::new(),
                files: Vec::new(),
                env: Vec::new(),
                stdin: Vec::new(),
                expect: Expect::default(),
            });
            continue;
        }
        if let Some(content) = trimmed.strip_prefix("## ").or_else(|| trimmed.strip_prefix("##")) {
            if content.trim() == "END" {
                return Err(SpecError::new(lineno, "`## END' outside a block"));
            }
            let Some(case) = cur.as_mut() else {
                return Err(SpecError::new(lineno, "annotation before the first `####' case"));
            };
            let Some((key, value)) = split_annotation(content) else {
                return Err(SpecError::new(lineno, "annotation needs `## <key>: <value>'"));
            };
            let value = match value {
                Some(v) => v.into_bytes(),
                None => {
                    let (block, next) = read_block(&lines, i)?;
                    i = next;
                    block
                }
            };
            apply_annotation(case, &key, value, lineno)?;
            continue;
        }
        if trimmed.starts_with('#') || trimmed.trim().is_empty() {
            continue;
        }
        return Err(SpecError::new(
            lineno,
            format!("stray text outside an annotation: {trimmed:?}"),
        ));
    }
    if let Some(case) = cur.take() {
        cases.push(case);
    }
    for case in &cases {
        if case.argv.is_empty() {
            return Err(SpecError::new(0, format!("case {:?} has no `## argv:'", case.name)));
        }
    }
    Ok(cases)
}

fn apply_annotation(
    case: &mut Case,
    key: &str,
    value: Vec<u8>,
    line: usize,
) -> Result<(), SpecError> {
    let text = || String::from_utf8_lossy(&value).trim().to_string();
    match key {
        "argv" => case.argv = tokenize(&String::from_utf8_lossy(&value), line)?,
        "stdin" => case.stdin = value,
        "stdin-json" => case.stdin = json_decode(&text(), line)?,
        "stdout" => case.expect.stdout = Some(value),
        "stdout-json" => case.expect.stdout = Some(json_decode(&text(), line)?),
        "stderr" => case.expect.stderr = Some(Stream::Exact(value)),
        "stderr-json" => case.expect.stderr = Some(Stream::Exact(json_decode(&text(), line)?)),
        "stderr-contains" => case.expect.stderr = Some(Stream::Contains(text().into_bytes())),
        "no-file-after" => case.expect.no_files_after.push(text()),
        "status" => {
            let n = text()
                .parse::<i32>()
                .map_err(|_| SpecError::new(line, "`## status:' needs an integer"))?;
            case.expect.status = Some(n);
        }
        other => {
            // `file-json`/`file-after-json` carry exact bytes, which is the only
            // way to write a file that holds a NUL or no trailing newline.
            if let Some(name) = other.strip_prefix("file-after-json ") {
                let bytes = json_decode(&text(), line)?;
                case.expect.files_after.push((name.trim().to_string(), bytes));
                return Ok(());
            }
            if let Some(name) = other.strip_prefix("file-json ") {
                let bytes = json_decode(&text(), line)?;
                case.files.push((name.trim().to_string(), bytes));
                return Ok(());
            }
            if let Some(name) = other.strip_prefix("file-after ") {
                case.expect.files_after.push((name.trim().to_string(), value));
                return Ok(());
            }
            if let Some(name) = other.strip_prefix("file ") {
                case.files.push((name.trim().to_string(), value));
                return Ok(());
            }
            // An EMPTY value is written as the block form, `## env NAME:' then
            // `## END' -- and it is not the same as leaving the variable out.
            if let Some(name) = other.strip_prefix("env ") {
                case.env.push((name.trim().to_string(), value));
                return Ok(());
            }
            return Err(SpecError::new(
                line,
                format!(
                    "unrecognized annotation key {other:?} — a typo here would silently \
                     assert nothing"
                ),
            ));
        }
    }
    Ok(())
}

// ---- vendored GNU grep regex suites --------------------------------------

/// Read one Spencer-format `.tests` file (see the module header for the row
/// grammar and why 4-field rows are not asserted).
pub fn parse_spencer(text: &str, file: &str, ere: bool) -> Result<Vec<Case>, SpecError> {
    let mut cases = Vec::new();
    for (idx, raw) in text.lines().enumerate() {
        let lineno = idx + 1;
        if raw.starts_with('#') || raw.trim().is_empty() {
            continue;
        }
        // Three fields is a case; a FOURTH is upstream's own expected-non-
        // conformance marker (`POSIX BOTCH`, `TO CORRECT`) and is not asserted.
        // Any other arity is a corrupted corpus and REDS rather than being
        // skipped — a silently shrinking suite is the one failure a conformance
        // gate must not have.
        let fields: Vec<&str> = raw.split('@').collect();
        if fields.len() == 4 {
            continue;
        }
        let (Some(status), Some(pattern), Some(input)) =
            (fields.first(), fields.get(1), fields.get(2))
        else {
            return Err(SpecError::new(lineno, "expected `status@pattern@input'"));
        };
        if fields.len() != 3 {
            return Err(SpecError::new(lineno, "expected `status@pattern@input'"));
        }
        let status = status
            .trim()
            .parse::<i32>()
            .map_err(|_| SpecError::new(lineno, "first field must be the expected exit status"))?;
        let mut argv: Vec<Vec<u8>> = vec![b"grep".to_vec()];
        if ere {
            argv.push(b"-E".to_vec());
        }
        argv.push(b"-e".to_vec());
        argv.push(pattern.as_bytes().to_vec());
        // Upstream pipes the subject through `echo`, so it arrives with a
        // trailing newline and no other transformation.
        let mut stdin = input.as_bytes().to_vec();
        stdin.push(b'\n');
        cases.push(Case {
            file: file.to_string(),
            name: raw.trim_end().to_string(),
            argv,
            files: Vec::new(),
            env: Vec::new(),
            stdin,
            // Only the status: upstream's driver discards the output.
            expect: Expect { status: Some(status), ..Expect::default() },
        });
    }
    Ok(cases)
}

// ---- the corpus ----------------------------------------------------------

/// The vendored GNU grep suites and the dialect each is written in (taken from
/// the `-E` in upstream's awk driver for that file).
const GREP_SUITES: &[(&str, bool)] = &[
    ("bre.tests", false),
    ("ere.tests", true),
    ("spencer1.tests", true),
];

/// The vendored GNU sed testsuite triples, PINNED rather than globbed: a
/// discovered set can silently shrink to nothing while every test stays green,
/// which is the same hole `GREP_SUITES` closes for the grep suites. The list is
/// exactly the uniform-recipe names from the 4.2.2 Makefile.tests (see
/// spec/README).
const SED_TRIPLES: &[&str] = &[
    "8bit",
    "8to7",
    "allsub",
    "amp-escape",
    "appquit",
    "bkslashes",
    "brackets",
    "dollar",
    "empty",
    "enable",
    "fasts",
    "flipcase",
    "head",
    "inclib",
    "insert",
    "khadafy",
    "linecnt",
    "mac-mf",
    "madding",
    "manis",
    "modulo",
    "newjis",
    "noeol",
    "numsub",
    "recall",
    "recall2",
    "sep",
    "space",
    "uniq",
    "xabcx",
    "xbxcx",
    "xbxcx3",
    "xemacs",
    "y-bracket",
    "y-newline",
];

fn read_file(path: &Path) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()).into())
}

/// Every case in the corpus, in a deterministic order. Shared by the gate and
/// the overlay generator so neither can enumerate a different set. A missing or
/// unreadable vendored file is an error, not a silently smaller corpus.
pub fn load_corpus(spec_dir: &Path) -> Result<Vec<Case>, Box<dyn std::error::Error>> {
    let mut cases: Vec<Case> = Vec::new();

    // 1. td-txt's own case files.
    let mut own: Vec<PathBuf> = Vec::new();
    for entry in std::fs::read_dir(spec_dir)? {
        let path = entry?.path();
        if path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.ends_with(".test.txt"))
        {
            own.push(path);
        }
    }
    own.sort();
    for path in &own {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default().to_string();
        let text = String::from_utf8(read_file(path)?)?;
        cases.extend(parse_cases(&text, &name)?);
    }

    // 2. The vendored GNU grep regex suites.
    let grep_dir = spec_dir.join("gnu-grep");
    for (name, ere) in GREP_SUITES {
        let text = String::from_utf8(read_file(&grep_dir.join(name))?)?;
        cases.extend(parse_spencer(&text, &format!("gnu-grep/{name}"), *ere)?);
    }
    // The Khadafy test: one ERE read from a file, over 32 lines that must all
    // come back unchanged.
    let regexp = read_file(&grep_dir.join("khadafy.regexp"))?;
    let lines = read_file(&grep_dir.join("khadafy.lines"))?;
    cases.push(Case {
        file: "gnu-grep/khadafy".to_string(),
        name: "every spelling of Khadafy matches the one ERE".to_string(),
        argv: vec![
            b"grep".to_vec(),
            b"-E".to_vec(),
            b"-f".to_vec(),
            b"khadafy.regexp".to_vec(),
            b"khadafy.lines".to_vec(),
        ],
        files: vec![
            ("khadafy.regexp".to_string(), regexp),
            ("khadafy.lines".to_string(), lines.clone()),
        ],
        env: Vec::new(),
        stdin: Vec::new(),
        expect: Expect { status: Some(0), stdout: Some(lines), ..Expect::default() },
    });

    // 3. The vendored GNU sed triples.
    let sed_dir = spec_dir.join("gnu-sed");
    for stem in SED_TRIPLES {
        let script = read_file(&sed_dir.join(format!("{stem}.sed")))?;
        let input = read_file(&sed_dir.join(format!("{stem}.inp")))?;
        let good = read_file(&sed_dir.join(format!("{stem}.good")))?;
        cases.push(Case {
            file: "gnu-sed".to_string(),
            name: (*stem).to_string(),
            argv: vec![b"sed".to_vec(), b"-f".to_vec(), format!("{stem}.sed").into_bytes()],
            files: vec![(format!("{stem}.sed"), script)],
            env: Vec::new(),
            stdin: input,
            expect: Expect { status: Some(0), stdout: Some(good), ..Expect::default() },
        });
    }
    Ok(cases)
}

// ---- running -------------------------------------------------------------

fn drain_pipe<R: Read + Send + 'static>(stream: Option<R>) -> std::sync::mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = std::sync::mpsc::channel();
    match stream {
        Some(mut s) => {
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                let _ = s.read_to_end(&mut buf); // bytes read before an error are kept
                let _ = tx.send(buf);
            });
        }
        None => {
            let _ = tx.send(Vec::new());
        }
    }
    rx
}

/// Wait for `child` up to `timeout` while draining its pipes, returning
/// `(status_or_-1, timed_out, stdout, stderr)`. Both pipes are drained on reader
/// threads started BEFORE the wait, so a case whose output exceeds the pipe
/// buffer keeps running instead of deadlocking on write.
fn wait_and_capture(
    mut child: Child,
    timeout: Duration,
) -> std::io::Result<(i32, bool, Vec<u8>, Vec<u8>)> {
    let out_rx = drain_pipe(child.stdout.take());
    let err_rx = drain_pipe(child.stderr.take());
    let deadline = Instant::now() + timeout;
    let mut timed_out = false;
    let status = loop {
        if let Some(st) = child.try_wait()? {
            break st;
        }
        if Instant::now() >= deadline {
            child.kill()?;
            timed_out = true;
            break child.wait()?;
        }
        std::thread::sleep(Duration::from_millis(2));
    };
    let out = out_rx.recv_timeout(DRAIN_GRACE).unwrap_or_default();
    let err = err_rx.recv_timeout(DRAIN_GRACE).unwrap_or_default();
    Ok((status.code().unwrap_or(-1), timed_out, out, err))
}

/// A throwaway working directory for one case, removed on drop.
struct CaseWorkdir(PathBuf);

impl CaseWorkdir {
    fn new() -> std::io::Result<Self> {
        use std::os::unix::fs::DirBuilderExt;
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let base = std::env::temp_dir();
        let pid = std::process::id();
        // Exclusive create: the name is predictable, so a symlink planted at it —
        // or a directory leaked by a crashed run whose pid the OS reused — must
        // red the create rather than be silently adopted.
        loop {
            let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let dir = base.join(format!("td-txt-case-{pid}-{seq}"));
            match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
                Ok(()) => return Ok(Self(dir)),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        }
    }
}

impl Drop for CaseWorkdir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0); // best effort; a case may leave files
    }
}

fn describe(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    if text.len() <= 400 {
        return format!("{text:?}");
    }
    let head: String = text.chars().take(400).collect();
    format!("{head:?}… ({} bytes)", bytes.len())
}

fn os_string(bytes: &[u8]) -> std::ffi::OsString {
    use std::os::unix::ffi::OsStringExt;
    std::ffi::OsString::from_vec(bytes.to_vec())
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }
    hay.windows(needle.len()).any(|w| w == needle)
}

/// Run one case against the built multicall. The applet is reached through a
/// `<applet> -> bin` symlink, so argv[0] dispatch is what every case exercises.
pub fn run_case(bin: &Path, case: &Case) -> Result<CaseOutcome, Box<dyn std::error::Error>> {
    let bin = std::fs::canonicalize(bin)?;
    let workdir = CaseWorkdir::new()?;
    let cwd = workdir.0.join("cwd");
    std::fs::create_dir(&cwd)?;
    for (name, content) in &case.files {
        let path = cwd.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, content)?;
    }
    let applet = case
        .argv
        .first()
        .map(|a| String::from_utf8_lossy(a).into_owned())
        .ok_or("case has an empty argv")?;
    let link = workdir.0.join(&applet);
    std::os::unix::fs::symlink(&bin, &link)?;

    let mut cmd = Command::new(&link);
    for arg in case.argv.get(1..).unwrap_or_default() {
        cmd.arg(os_string(arg));
    }
    // env_clear for determinism, plus the C locale these applets are written for:
    // a case must not change meaning because the host runs a UTF-8 locale.
    cmd.env_clear();
    for (name, value) in &case.env {
        cmd.env(name, os_string(value));
    }
    // LC_ALL goes on LAST so a case cannot take it back: the promise above is
    // unconditional, not a convention every case author has to keep.
    cmd.env("LC_ALL", "C");
    let mut child = cmd
        .current_dir(&cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    if let Some(mut sink) = child.stdin.take() {
        let input = case.stdin.clone();
        // On a thread: a case whose stdin exceeds the pipe buffer would otherwise
        // deadlock against a child that has not started reading. A closed pipe
        // (the applet exited early, as `-q` does) is an expected end, not an error.
        std::thread::spawn(move || {
            let _ = sink.write_all(&input);
        });
    }
    let (status, timed_out, stdout, stderr) = wait_and_capture(child, CASE_TIMEOUT)?;

    let mut problems: Vec<String> = Vec::new();
    if let Some(want) = case.expect.status {
        if status != want {
            problems.push(format!("status: want {want}, got {status}"));
        }
    }
    if let Some(want) = &case.expect.stdout {
        if *want != stdout {
            problems.push(format!("stdout: want {}, got {}", describe(want), describe(&stdout)));
        }
    }
    match &case.expect.stderr {
        Some(Stream::Exact(want)) if *want != stderr => {
            problems.push(format!("stderr: want {}, got {}", describe(want), describe(&stderr)));
        }
        Some(Stream::Contains(want)) if !contains(&stderr, want) => {
            problems.push(format!(
                "stderr: want it to contain {}, got {}",
                describe(want),
                describe(&stderr)
            ));
        }
        _ => {}
    }
    for (name, want) in &case.expect.files_after {
        match std::fs::read(cwd.join(name)) {
            Ok(got) if got == *want => {}
            Ok(got) => problems.push(format!(
                "file {name}: want {}, got {}",
                describe(want),
                describe(&got)
            )),
            Err(e) => problems.push(format!("file {name}: unreadable after the run ({e})")),
        }
    }
    for name in &case.expect.no_files_after {
        if cwd.join(name).symlink_metadata().is_ok() {
            problems.push(format!("file {name}: exists after the run, and must not"));
        }
    }
    if timed_out {
        problems.insert(0, format!("timed out after {}s", CASE_TIMEOUT.as_secs()));
    }
    // A case with no expectation at all would pass whatever td-txt did. Failing
    // it here, not just in the corpus lint, keeps every caller fail-closed.
    if !case.expect.asserts_something() {
        problems.push("the case asserts nothing".to_string());
    }
    Ok(CaseOutcome {
        passed: problems.is_empty(),
        detail: if problems.is_empty() { None } else { Some(problems.join("; ")) },
        timed_out,
    })
}

// ---- expectations overlay ------------------------------------------------

impl Expect {
    /// Whether this expectation can actually fail. A case that asserts nothing
    /// is a corpus bug: it reports green without proving anything.
    #[must_use]
    pub fn asserts_something(&self) -> bool {
        self.status.is_some()
            || self.stdout.is_some()
            || self.stderr.is_some()
            || !self.files_after.is_empty()
            || !self.no_files_after.is_empty()
    }
}

/// How a case's observed result relates to td-txt's declared expectation.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Disposition {
    /// Ran, matched, and was not listed — real green coverage.
    Pass,
    /// Ran, mismatched, and is listed `xfail` — a known gap, tolerated.
    XFail,
    /// Ran and matched, but is listed `xfail` — the gap closed; promote it.
    XPass,
    /// Ran, mismatched, and was not listed — a regression. Reds the gate.
    Fail,
    /// Listed `skip`; not run at all.
    Skip,
}

/// td-txt's known-gap manifest, kept OUTSIDE the corpus files so the vendored
/// GNU suites stay byte-for-byte pristine.
#[derive(Clone, Debug, Default)]
pub struct Expectations {
    xfail: BTreeSet<String>,
    skip: BTreeSet<String>,
}

/// The manifest key for a case: `<corpus-file>::<case name>`.
pub fn case_key(file: &str, name: &str) -> String {
    format!("{file}::{name}")
}

/// Overlay keys for a whole corpus, in corpus order. A name that repeats within
/// one file gets an ` ##N` occurrence suffix from its 2nd appearance on, so every
/// case stays individually addressable (the Spencer suites do repeat rows).
pub fn case_keys(cases: &[Case]) -> Vec<String> {
    let mut seen: std::collections::HashMap<(String, String), usize> =
        std::collections::HashMap::new();
    let mut keys = Vec::with_capacity(cases.len());
    for case in cases {
        let entry = seen.entry((case.file.clone(), case.name.clone())).or_insert(0);
        if *entry == 0 {
            keys.push(case_key(&case.file, &case.name));
        } else {
            keys.push(format!("{}::{} ##{}", case.file, case.name, *entry + 1));
        }
        *entry += 1;
    }
    keys
}

impl Expectations {
    /// Parse the overlay: `<xfail|skip> <file>::<case name>` per line, `#`
    /// comments ignored. A duplicate, contradictory or malformed entry is a hard
    /// error — a sloppy manifest must not silently mis-tolerate a case.
    pub fn parse(text: &str) -> Result<Self, SpecError> {
        let mut xfail = BTreeSet::new();
        let mut skip = BTreeSet::new();
        for (idx, raw) in text.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let ln = idx + 1;
            let (disp, key) = line.split_once(char::is_whitespace).ok_or_else(|| {
                SpecError::new(ln, "expectation needs `<xfail|skip> <file>::<case>'")
            })?;
            let key = key.trim();
            if !key.contains("::") {
                return Err(SpecError::new(ln, "expectation key must be `<file>::<case>'"));
            }
            let inserted = match disp {
                "xfail" => xfail.insert(key.to_string()),
                "skip" => skip.insert(key.to_string()),
                other => {
                    return Err(SpecError::new(
                        ln,
                        format!("unknown disposition {other:?} (want xfail|skip)"),
                    ))
                }
            };
            if !inserted {
                return Err(SpecError::new(ln, format!("duplicate expectation {key:?}")));
            }
        }
        if let Some(k) = xfail.intersection(&skip).next() {
            return Err(SpecError::new(0, format!("{k:?} listed as both xfail and skip")));
        }
        Ok(Self { xfail, skip })
    }

    fn is_xfail(&self, key: &str) -> bool {
        self.xfail.contains(key)
    }

    fn is_skip(&self, key: &str) -> bool {
        self.skip.contains(key)
    }

    fn keys(&self) -> impl Iterator<Item = &String> {
        self.xfail.iter().chain(self.skip.iter())
    }
}

/// One case run, classified against the overlay.
#[derive(Clone, Debug)]
pub struct ClassifiedOutcome {
    pub key: String,
    pub disposition: Disposition,
    pub detail: Option<String>,
}

/// Counts by disposition over a classified run.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Summary {
    pub pass: usize,
    pub xfail: usize,
    pub xpass: usize,
    pub fail: usize,
    pub skip: usize,
}

impl Summary {
    /// Green iff there is no regression and no stale-tolerated pass. Stale
    /// manifest keys are surfaced separately and must also be empty.
    pub fn is_green(&self) -> bool {
        self.fail == 0 && self.xpass == 0
    }
}

pub fn summarize(outcomes: &[ClassifiedOutcome]) -> Summary {
    let mut s = Summary::default();
    for o in outcomes {
        match o.disposition {
            Disposition::Pass => s.pass += 1,
            Disposition::XFail => s.xfail += 1,
            Disposition::XPass => s.xpass += 1,
            Disposition::Fail => s.fail += 1,
            Disposition::Skip => s.skip += 1,
        }
    }
    s
}

fn classify(key: String, outcome: &CaseOutcome, exp: &Expectations) -> ClassifiedOutcome {
    let disposition = match (outcome.passed, exp.is_xfail(&key)) {
        (true, false) => Disposition::Pass,
        (false, true) => Disposition::XFail,
        (true, true) => Disposition::XPass,
        (false, false) => Disposition::Fail,
    };
    ClassifiedOutcome { key, disposition, detail: outcome.detail.clone() }
}

/// Run every case, classifying each against `exp`. `skip` cases are not executed.
/// Returns the classified outcomes and the overlay keys that matched no case
/// (stale entries); a land-on-green caller reds on both.
///
/// The cases run across threads (`run_cases_concurrently`). The outcomes still
/// read in corpus order, and a case that could not be run at all reds the whole
/// run as it always did — after every other case has had its turn, rather than
/// before the ones queued behind it.
pub fn run_all_classified(
    bin: &Path,
    cases: &[Case],
    exp: &Expectations,
) -> Result<(Vec<ClassifiedOutcome>, Vec<String>), Box<dyn std::error::Error>> {
    let keys = case_keys(cases);
    // One slot per case, in corpus order. A collision or a `skip` is settled
    // here; everything else is queued with the slot its outcome will fill.
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut slots: Vec<Option<ClassifiedOutcome>> = Vec::new();
    let mut queued: Vec<(usize, String, &Case)> = Vec::new();
    for (case, key) in cases.iter().zip(keys) {
        // `case_keys` occurrence-qualifies repeats, so a collision here means two
        // cases really do map to one key — one overlay entry would then stand in
        // for both. Red it unconditionally rather than let it hide.
        if !seen.insert(key.clone()) {
            slots.push(Some(ClassifiedOutcome {
                key,
                disposition: Disposition::Fail,
                detail: Some("two cases map to the same overlay key (collision)".into()),
            }));
            continue;
        }
        if exp.is_skip(&key) {
            slots.push(Some(ClassifiedOutcome { key, disposition: Disposition::Skip, detail: None }));
            continue;
        }
        queued.push((slots.len(), key, case));
        slots.push(None);
    }
    let to_run: Vec<&Case> = queued.iter().map(|(_, _, case)| *case).collect();
    let outcomes = run_cases_concurrently(bin, &to_run);
    for ((slot, key, _), outcome) in queued.into_iter().zip(outcomes) {
        let outcome = outcome.map_err(|e| format!("{key}: {e}"))?;
        if let Some(entry) = slots.get_mut(slot) {
            *entry = Some(classify(key, &outcome, exp));
        }
    }
    let mut out: Vec<ClassifiedOutcome> = Vec::with_capacity(slots.len());
    for slot in slots {
        // Fails closed: a slot nothing filled is a case the run never graded,
        // not a case that passed.
        out.push(slot.ok_or("a queued case was never given an outcome")?);
    }
    let stale: Vec<String> = exp.keys().filter(|k| !seen.contains(*k)).cloned().collect();
    Ok((out, stale))
}

/// How many cases run at once. `RUST_TEST_THREADS` serializes the harness
/// itself when set (`--test-threads=1` alone does not export it), so it
/// serializes this too: a developer under strace or a sanitizer who asked for
/// one thread that way gets one. Otherwise every hardware thread the process
/// may use, which on Linux is the cgroup quota and the affinity mask rather
/// than the machine's count. Never more threads than cases, never fewer than
/// one.
fn corpus_width(cases: usize) -> usize {
    let asked = std::env::var("RUST_TEST_THREADS")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|n| *n > 0);
    asked
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |n| n.get()))
        .min(cases)
        .max(1)
}

/// Run `cases` across `corpus_width` threads and hand back each outcome in
/// the order its case was given. A case's error crosses the thread boundary
/// as text, since `Box<dyn Error>` is not `Send`; its message survives, its
/// `source()` chain does not, and nothing downcasts one.
///
/// Sound because `run_case` shares nothing between cases: a cleared
/// environment, a throwaway workdir named by a process-wide counter, and a
/// symlink to the binary rather than a copy, so no thread holds a write
/// descriptor that a sibling's child could inherit into an ETXTBSY. Cargo
/// already runs the other tests in this binary on threads beside this one, so
/// the process was never single-threaded to begin with; this only stops the
/// corpus itself from being the one serial test that bounds the suite.
///
/// That argument is about the harness, not the corpus. The corpus keeps to it
/// because every absolute operand a case names is a stateless device node
/// (`/dev/null`, `/dev/full`, `/dev/zero`), a per-process descriptor node
/// (`/dev/stdin`, `/dev/stdout`, `/dev/stderr`, `/dev/fd/1`,
/// `/proc/self/fd/0`), or a path that never exists, and every other operand
/// is relative and materialized into the case's own `cwd`; a case that wrote
/// to a shared absolute path would share it with whatever runs beside it.
/// `RUST_TEST_THREADS=1` restores the serial run.
fn run_cases_concurrently(bin: &Path, cases: &[&Case]) -> Vec<Result<CaseOutcome, String>> {
    if cases.is_empty() {
        return Vec::new();
    }
    let width = corpus_width(cases.len());
    let next = AtomicUsize::new(0);
    let slots: Vec<Mutex<Option<Result<CaseOutcome, String>>>> =
        cases.iter().map(|_| Mutex::new(None)).collect();
    std::thread::scope(|scope| {
        for _ in 0..width {
            scope.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                let Some(case) = cases.get(i) else { break };
                let outcome = run_case(bin, case).map_err(|e| e.to_string());
                if let Some(slot) = slots.get(i) {
                    *slot.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(outcome);
                }
            });
        }
    });
    slots
        .into_iter()
        .map(|slot| {
            slot.into_inner()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .unwrap_or_else(|| Err("the case was never run".to_string()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
# a comment
#### grep counts matching lines
## argv: grep -c foo
## stdin:
foo
bar
## END
## stdout:
1
## END
## status: 0

#### sed rewrites a file in place
## argv: sed -i s/a/b/ f.txt
## file f.txt:
a
## END
## file-after f.txt:
b
## END
## status: 0
";

    #[test]
    fn native_format_parses_argv_blocks_and_files() {
        let cases = parse_cases(SAMPLE, "sample.test.txt").unwrap();
        assert_eq!(cases.len(), 2);
        let first = cases.first().unwrap();
        assert_eq!(first.name, "grep counts matching lines");
        assert_eq!(first.argv, vec![b"grep".to_vec(), b"-c".to_vec(), b"foo".to_vec()]);
        assert_eq!(first.stdin, b"foo\nbar\n".to_vec());
        assert_eq!(first.expect.stdout, Some(b"1\n".to_vec()));
        assert_eq!(first.expect.status, Some(0));
        let second = cases.get(1).unwrap();
        assert_eq!(second.files, vec![("f.txt".to_string(), b"a\n".to_vec())]);
        assert_eq!(second.expect.files_after, vec![("f.txt".to_string(), b"b\n".to_vec())]);
    }

    #[test]
    fn no_file_after_names_a_file_and_is_an_assertion_on_its_own() {
        let text = "#### x\n## argv: sed --sandbox 'w out'\n## no-file-after: out\n";
        let cases = parse_cases(text, "s.test.txt").unwrap();
        let case = cases.first().unwrap();
        assert_eq!(case.expect.no_files_after, vec!["out".to_string()]);
        // Without this the case would be rejected for asserting nothing, and the
        // one property it exists to check would never be looked at.
        assert!(case.expect.asserts_something());
    }

    #[test]
    fn an_unknown_annotation_key_is_a_hard_error() {
        let text = "#### x\n## argv: grep a\n## stdotu: y\n";
        assert!(parse_cases(text, "f.test.txt").is_err());
    }

    #[test]
    fn a_case_without_argv_is_a_hard_error() {
        assert!(parse_cases("#### x\n## status: 0\n", "f.test.txt").is_err());
    }

    #[test]
    fn an_unterminated_block_is_a_hard_error() {
        assert!(parse_cases("#### x\n## argv: grep a\n## stdout:\nhi\n", "f.test.txt").is_err());
    }

    #[test]
    fn argv_tokenizer_handles_quotes_and_escapes() {
        let argv = tokenize(r#"grep -e 'a b' "c\td" e\ f"#, 1).unwrap();
        assert_eq!(
            argv,
            vec![
                b"grep".to_vec(),
                b"-e".to_vec(),
                b"a b".to_vec(),
                b"c\td".to_vec(),
                b"e f".to_vec(),
            ]
        );
    }

    /// A LETTER escape the tokenizer does not know must be refused rather than
    /// spelled as the letter: `"\r"` meaning `r` is a case that passes while
    /// asserting something else, and `\0` is the same trap unclosed.
    /// Punctuation still escapes itself, which is what `\\` and `\"` need.
    #[test]
    fn argv_tokenizer_refuses_an_unknown_letter_escape() {
        assert_eq!(tokenize(r#"sed "a\rb""#, 1).unwrap(), vec![b"sed".to_vec(), b"a\rb".to_vec()]);
        for bad in [r#"sed "a\0b""#, r#"sed "a\e""#] {
            assert!(tokenize(bad, 1).is_err(), "{bad} was accepted");
        }
        assert_eq!(
            tokenize(r#"sed "a\\b" "c\"d" "e\$f""#, 1).unwrap(),
            vec![b"sed".to_vec(), br"a\b".to_vec(), b"c\"d".to_vec(), b"e$f".to_vec()]
        );
    }

    /// The one escape that puts a byte no `&str` can carry into argv. A case
    /// about a diagnostic QUOTING a byte cannot be written without it: the byte
    /// has to reach the applet through argv, and this file is read as text.
    #[test]
    fn argv_tokenizer_takes_a_hex_escape() {
        assert_eq!(
            tokenize(r#"sed "-\x80" "\x41\x7f""#, 1).unwrap(),
            vec![b"sed".to_vec(), b"-\x80".to_vec(), b"A\x7f".to_vec()]
        );
        // A short or non-hex run is refused rather than read as the letter.
        // `\x+4` is the one that needs saying: `from_str_radix` takes a leading
        // `+`, so without the digit check it would quietly be 0x04.
        for bad in [r#"sed "a\x4""#, r#"sed "a\xzz""#, r#"sed "a\x""#, r#"sed "a\x+4""#] {
            assert!(tokenize(bad, 1).is_err(), "{bad} was accepted");
        }
        // A NUL is spellable now and argv cannot hold one, so it is refused
        // here rather than at `Command::spawn`, which would end the whole run.
        assert!(tokenize(r#"sed "a\x00b""#, 1).is_err(), "a NUL in argv was accepted");
        // The same escape in the `-json` table, which shares the reader.
        assert_eq!(json_decode(r#""\x41\x80""#, 1).unwrap(), b"A\x80".to_vec());
        for bad in [r#""\x+4""#, r#""\xzz""#, r#""\x4""#] {
            assert!(json_decode(bad, 1).is_err(), "{bad} was accepted");
        }
    }

    #[test]
    fn json_values_carry_exact_bytes() {
        assert_eq!(json_decode(r#""a\nb""#, 1).unwrap(), b"a\nb".to_vec());
        assert_eq!(json_decode(r#""""#, 1).unwrap(), Vec::<u8>::new());
        assert_eq!(json_decode(r#""\x41""#, 1).unwrap(), b"A".to_vec());
        assert!(json_decode("nope", 1).is_err());
    }

    #[test]
    fn a_malformed_spencer_row_reds_instead_of_shrinking_the_corpus() {
        assert!(parse_spencer("0@abc\n", "gnu-grep/bre.tests", false).is_err());
        assert!(parse_spencer("0@a@b@c@d\n", "gnu-grep/bre.tests", false).is_err());
    }

    #[test]
    fn spencer_rows_become_status_only_cases() {
        let text = "# comment\n0@abc@abc\n1@abc@xbc\n2@a\\(@EPAREN@TO CORRECT\n";
        let cases = parse_spencer(text, "gnu-grep/bre.tests", false).unwrap();
        // The 4-field row is upstream's non-conformance marker: not asserted.
        assert_eq!(cases.len(), 2);
        let first = cases.first().unwrap();
        assert_eq!(first.argv, vec![b"grep".to_vec(), b"-e".to_vec(), b"abc".to_vec()]);
        assert_eq!(first.stdin, b"abc\n".to_vec());
        assert_eq!(first.expect.status, Some(0));
        assert!(first.expect.stdout.is_none());
        assert_eq!(cases.get(1).and_then(|c| c.expect.status), Some(1));
    }

    #[test]
    fn ere_suites_pass_the_extended_flag() {
        let cases = parse_spencer("0@a+@aa\n", "gnu-grep/ere.tests", true).unwrap();
        assert_eq!(
            cases.first().map(|c| c.argv.clone()),
            Some(vec![b"grep".to_vec(), b"-E".to_vec(), b"-e".to_vec(), b"a+".to_vec()])
        );
    }

    #[test]
    fn repeated_case_names_get_occurrence_suffixes() {
        let mk = |name: &str| Case {
            file: "f".into(),
            name: name.into(),
            argv: vec![b"grep".to_vec()],
            files: Vec::new(),
            env: Vec::new(),
            stdin: Vec::new(),
            expect: Expect::default(),
        };
        let keys = case_keys(&[mk("a"), mk("b"), mk("a")]);
        assert_eq!(keys, vec!["f::a", "f::b", "f::a ##2"]);
    }

    /// The EMPTY value is the reason this needs its own test: a corpus case
    /// cannot tell it from any other value, since what the applet reads is
    /// whether the variable is there.
    #[test]
    fn env_takes_a_value_inline_and_an_empty_one_as_a_block() {
        let text = "#### c\n## argv: grep a\n## env A: 1\n## env B:\n## END\n";
        let cases = parse_cases(text, "f").unwrap();
        let env = &cases.first().unwrap().env;
        assert_eq!(env, &[("A".to_string(), b"1".to_vec()), ("B".to_string(), Vec::new())]);
    }

    #[test]
    fn overlay_rejects_a_malformed_or_duplicate_entry() {
        assert!(Expectations::parse("xfail f::a\nskip f::b\n").is_ok());
        assert!(Expectations::parse("xfail f::a\nxfail f::a\n").is_err());
        assert!(Expectations::parse("wat f::a\n").is_err());
        assert!(Expectations::parse("xfail nocolons\n").is_err());
        assert!(Expectations::parse("xfail f::a\nskip f::a\n").is_err());
    }

    #[test]
    fn classification_reds_a_regression_and_an_unexpected_pass() {
        let exp = Expectations::parse("xfail f::known\n").unwrap();
        let fail = CaseOutcome { passed: false, detail: None, timed_out: false };
        let pass = CaseOutcome { passed: true, detail: None, timed_out: false };
        assert_eq!(classify("f::known".into(), &fail, &exp).disposition, Disposition::XFail);
        assert_eq!(classify("f::known".into(), &pass, &exp).disposition, Disposition::XPass);
        assert_eq!(classify("f::new".into(), &fail, &exp).disposition, Disposition::Fail);
        assert_eq!(classify("f::new".into(), &pass, &exp).disposition, Disposition::Pass);
        let summary = summarize(&[
            classify("f::new".into(), &fail, &exp),
            classify("f::known".into(), &fail, &exp),
        ]);
        assert!(!summary.is_green());
        assert_eq!(summary.fail, 1);
        assert_eq!(summary.xfail, 1);
    }

    #[test]
    fn contains_is_a_byte_search() {
        assert!(contains(b"grep: invalid option", b"invalid option"));
        assert!(!contains(b"short", b"much longer needle"));
        assert!(contains(b"anything", b""));
    }
}

#[cfg(test)]
mod runner_tests {
    use super::*;
    use std::path::Path;

    fn case(name: &str, script: &str, stdout: &str) -> Case {
        Case {
            file: "runner.tests".to_string(),
            name: name.to_string(),
            argv: vec![b"sh".to_vec(), b"-c".to_vec(), script.as_bytes().to_vec()],
            files: Vec::new(),
            env: Vec::new(),
            stdin: Vec::new(),
            expect: Expect {
                status: Some(0),
                stdout: Some(stdout.as_bytes().to_vec()),
                ..Expect::default()
            },
        }
    }

    /// Outcomes come back in corpus order however the threads finish: the
    /// first case spins before it answers, so the ones after it are done
    /// first.
    #[test]
    fn outcomes_land_in_corpus_order_whichever_finishes_first() {
        let sh = Path::new("/bin/sh");
        if !sh.exists() {
            eprintln!("SKIP: no /bin/sh");
            return;
        }
        let cases = vec![
            case(
                "slow",
                "i=0; while [ $i -lt 30000 ]; do i=$((i+1)); done; echo slow",
                "slow\n",
            ),
            case("quick", "echo quick", "quick\n"),
            case("last", "echo last", "last\n"),
        ];
        let (out, stale) =
            run_all_classified(sh, &cases, &Expectations::default()).expect("the run");
        let keys: Vec<&str> = out.iter().map(|o| o.key.as_str()).collect();
        assert_eq!(keys, ["runner.tests::slow", "runner.tests::quick", "runner.tests::last"]);
        for o in &out {
            assert_eq!(o.disposition, Disposition::Pass, "{}: {:?}", o.key, o.detail);
        }
        assert!(stale.is_empty());
    }

    /// A case that cannot be run at all fails the run, naming the case, rather
    /// than standing as a slot nothing filled.
    #[test]
    fn a_case_that_cannot_run_fails_the_run_by_name() {
        let cases = vec![
            case("first", "echo first", "first\n"),
            case("second", "echo second", "second\n"),
        ];
        let err = run_all_classified(
            Path::new("/nonexistent/td-txt"),
            &cases,
            &Expectations::default(),
        )
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default();
        assert!(err.contains("runner.tests::first"), "{err:?}");
    }
}

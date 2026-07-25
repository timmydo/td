//! td-sh conformance harness — a zero-dependency reader for the Oils
//! (`oils-for-unix/oils`) spec-test format, a golden-resolver that picks the
//! expected result for a target shell identity chain (default: ash → dash), and
//! a runner that executes a shell binary against each case and diffs the result.
//!
//! This is HOST-SIDE test tooling, not part of the shipped `td-sh` binary (the
//! recipe compiles `src/main.rs` alone). It is the reusable asset the oracle plan
//! hinges on: future PRs drop vendored Oils `spec/*.test.sh` files in unchanged,
//! and this reader consumes them — so the seed corpus and the eventual bulk
//! import share one parser.
//!
//! Spec format (a real shell script annotated with comments):
//! ```text
//!   #### <description>          begins a case
//!   ## <key>: <value>           a single-line assertion/metadata on the case
//!   ## STDOUT:  ...  ## END      a multiline expected-stdout block (verbatim)
//!   ## STDERR:  ...  ## END      a multiline expected-stderr block (verbatim)
//!   ## <QUAL> <shells> <k>: v    a per-shell override (QUAL ∈ OK|OK-N|BUG|BUG-N|N-I)
//!   ## <QUAL> <shells> STDOUT:   a per-shell multiline override; shells `/`-separated
//!   #  (single hash) / blank      an ignored comment (also a no-op shell comment)
//!   <anything else>              a line of the case's shell code (verbatim)
//! ```
//! Bare `## key: value` lines before the first `####` are file-level metadata
//! (e.g. `## compare_shells: bash dash mksh ash`) and are ignored here.
//!
//! Golden resolution: for each of {status, stdout, stderr} independently, the
//! effective expectation is the qualified annotation whose shell list contains
//! the earliest identity in the target chain (ash, then dash), else the
//! unqualified (default/ideal) annotation. Matching dash/ash — not the osh ideal —
//! is what a busybox-`sh` replacement must do. Unspecified stdout/stderr is "not
//! asserted"; unspecified status defaults to 0 (Oils semantics). The qualifier
//! kind (OK/N-I/BUG) does not change WHICH value applies to a shell, only records
//! why the shells legitimately differ, so resolution keys on the shell list.
#![deny(unsafe_code)]

use std::path::Path;
use std::process::{Command, Stdio};

/// The default identity chain for td-sh: prefer busybox `ash`'s expected output
/// (what we replace), then `dash` (the same NetBSD-ash POSIX lineage), then the
/// unqualified default block.
pub const ASH_DASH_CHAIN: &[&str] = &["ash", "dash"];

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
        write!(f, "spec parse error at line {}: {}", self.line, self.msg)
    }
}

impl std::error::Error for SpecError {}

/// Why a per-shell annotation legitimately differs from the ideal. Retained for
/// reporting; resolution keys on the shell list, not the qualifier.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Qualifier {
    Default,
    Ok,
    Bug,
    Ni,
}

#[derive(Clone, Debug)]
struct Annotation {
    #[allow(dead_code)] // kept for future reporting; resolution keys on `shells`
    qualifier: Qualifier,
    shells: Vec<String>, // empty => the unqualified default
    key: String,         // as-parsed: status | stdout | STDOUT | stdout-json | stderr | ...
    is_block: bool,      // from a `KEY:` .. `## END` multiline block
    value: String,       // Line: text after ':'; Block: lines joined, each with a trailing '\n'
}

/// One spec case: a description, its shell snippet, and its annotations.
#[derive(Clone, Debug)]
pub struct SpecCase {
    pub name: String,
    pub code: String,
    pub line: usize, // 1-based line of the `####` header, for reporting
    annotations: Vec<Annotation>,
}

/// The resolved expectation for a specific shell identity chain.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Expected {
    pub status: i32,            // defaults to 0 when unspecified
    pub stdout: Option<String>, // None => not asserted
    pub stderr: Option<String>, // None => not asserted
}

/// The result of running one case.
#[derive(Clone, Debug)]
pub struct CaseOutcome {
    pub name: String,
    pub passed: bool,
    pub detail: Option<String>, // human-readable mismatch when failed
}

// ---- parsing -------------------------------------------------------------

fn as_qualifier(w: &str) -> Option<Qualifier> {
    match w {
        "N-I" => return Some(Qualifier::Ni),
        "OK" => return Some(Qualifier::Ok),
        "BUG" => return Some(Qualifier::Bug),
        _ => {}
    }
    if let Some(n) = w.strip_prefix("OK-") {
        if !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()) {
            return Some(Qualifier::Ok);
        }
    }
    if let Some(n) = w.strip_prefix("BUG-") {
        if !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()) {
            return Some(Qualifier::Bug);
        }
    }
    None
}

struct Head {
    qualifier: Qualifier,
    shells: Vec<String>,
    key: String,
    value: String,
    has_colon: bool,
}

/// Parse the text after `## ` into an optional `QUAL shells` prefix and a
/// `key: value` (or a bare token when there is no colon, e.g. `END`).
fn parse_ann_head(content: &str) -> Head {
    let mut rest = content;
    let mut qualifier = Qualifier::Default;
    let mut shells: Vec<String> = Vec::new();

    let first = rest.split_whitespace().next().unwrap_or("");
    if let Some(q) = as_qualifier(first) {
        qualifier = q;
        let after_first = rest.get(first.len()..).unwrap_or("").trim_start();
        let sh = after_first.split_whitespace().next().unwrap_or("");
        shells = sh.split('/').filter(|s| !s.is_empty()).map(|s| s.to_string()).collect();
        rest = after_first.get(sh.len()..).unwrap_or("").trim_start();
    }

    if let Some(idx) = rest.find(':') {
        let key = rest.get(..idx).unwrap_or("").trim().to_string();
        let value = rest.get(idx + 1..).unwrap_or("").trim_start().to_string();
        Head { qualifier, shells, key, value, has_colon: true }
    } else {
        Head { qualifier, shells, key: rest.trim().to_string(), value: String::new(), has_colon: false }
    }
}

fn is_block_opener(head: &Head) -> bool {
    head.has_colon && head.value.is_empty() && (head.key == "STDOUT" || head.key == "STDERR")
}

/// Collect a `## STDOUT:` block body verbatim (each line plus a trailing '\n')
/// until a `## END` line. Returns the body and the index just past `## END`.
fn read_block(lines: &[&str], start: usize) -> Result<(String, usize), SpecError> {
    let mut body = String::new();
    let mut j = start;
    while let Some(l) = lines.get(j) {
        if l.trim() == "## END" {
            return Ok((body, j + 1));
        }
        body.push_str(l);
        body.push('\n');
        j += 1;
    }
    Err(SpecError::new(start, "unterminated `## STDOUT:`/`## STDERR:` block (missing `## END`)"))
}

/// Flush a completed case, trimming trailing blank lines from its code (they sit
/// between the code and the next case's annotations and are shell no-ops anyway).
fn push_case(cases: &mut Vec<SpecCase>, cur: Option<SpecCase>) {
    if let Some(mut c) = cur {
        while c.code.ends_with('\n') {
            c.code.pop();
        }
        cases.push(c);
    }
}

/// Parse an Oils-format spec into its cases.
pub fn parse_spec(input: &str) -> Result<Vec<SpecCase>, SpecError> {
    let lines: Vec<&str> = input.lines().collect();
    let mut cases: Vec<SpecCase> = Vec::new();
    let mut cur: Option<SpecCase> = None;
    let mut i = 0;

    while let Some(line) = lines.get(i) {
        // Case header (4+ leading hashes) — must precede the `##` check.
        if line.starts_with("####") {
            push_case(&mut cases, cur.take());
            let name = line.trim_start_matches('#').trim().to_string();
            cur = Some(SpecCase { name, code: String::new(), line: i + 1, annotations: Vec::new() });
            i += 1;
            continue;
        }

        if line.starts_with("##") {
            let content = line.get(2..).unwrap_or("").trim_start();
            let head = parse_ann_head(content);
            if is_block_opener(&head) {
                let (body, next) = read_block(&lines, i + 1)?;
                if let Some(c) = cur.as_mut() {
                    c.annotations.push(Annotation {
                        qualifier: head.qualifier,
                        shells: head.shells,
                        key: head.key,
                        is_block: true,
                        value: body,
                    });
                }
                i = next;
                continue;
            }
            // A single-line annotation with a colon carries an assertion; a bare
            // token (e.g. a stray `## END`) is ignored. File-header annotations
            // (cur is None) are ignored too.
            if head.has_colon {
                if let Some(c) = cur.as_mut() {
                    c.annotations.push(Annotation {
                        qualifier: head.qualifier,
                        shells: head.shells,
                        key: head.key,
                        is_block: false,
                        value: head.value,
                    });
                }
            }
            i += 1;
            continue;
        }

        // A code line — verbatim, preserving indentation (here-docs depend on it).
        // A single-`#` line is NOT special-cased: it is a shell comment (a no-op)
        // AND a possible here-doc/quoted-string body line (e.g. `#!/bin/sh`), so it
        // must reach the shell verbatim — the same treatment Oils gives it. Only
        // collect code BEFORE a case's first annotation: the lines after the
        // annotations (blank separators before the next `####`) are not code.
        if let Some(c) = cur.as_mut() {
            if c.annotations.is_empty() {
                if !c.code.is_empty() {
                    c.code.push('\n');
                }
                c.code.push_str(line);
            }
        }
        i += 1;
    }

    push_case(&mut cases, cur.take());
    Ok(cases)
}

// ---- golden resolution ---------------------------------------------------

#[derive(Clone, Copy)]
enum Field {
    Status,
    Stdout,
    Stderr,
}

fn key_in_field(key: &str, field: Field) -> bool {
    match field {
        Field::Status => key == "status",
        Field::Stdout => key == "stdout" || key == "STDOUT" || key == "stdout-json",
        Field::Stderr => key == "stderr" || key == "STDERR" || key == "stderr-json",
    }
}

/// True if `key` is a case-level assertion key the resolver understands. Every
/// `##` line inside a case is an Oils assertion (metadata lives at file level,
/// before the first `####`), so a key that is none of these is a typo (e.g.
/// `## stats:` for `status`) or an unsupported assertion the resolver would
/// silently skip — either way the resolved golden would be wrong.
fn is_assertion_key(key: &str) -> bool {
    key_in_field(key, Field::Status)
        || key_in_field(key, Field::Stdout)
        || key_in_field(key, Field::Stderr)
}

impl SpecCase {
    /// Case-level annotation keys not recognized as an assertion (see
    /// `is_assertion_key`): empty for a well-formed case, non-empty when a typo
    /// or unsupported assertion would silently pick the wrong golden. A corpus
    /// validator should reject any case whose result here is non-empty.
    pub fn unrecognized_keys(&self) -> Vec<&str> {
        self.annotations
            .iter()
            .filter(|a| !is_assertion_key(&a.key))
            .map(|a| a.key.as_str())
            .collect()
    }
}

/// Pick the annotation that applies to `chain` for `field`: the earliest chain
/// identity that some annotation names wins; otherwise the unqualified default.
fn pick<'a>(case: &'a SpecCase, chain: &[&str], field: Field) -> Option<&'a Annotation> {
    let candidates: Vec<&Annotation> =
        case.annotations.iter().filter(|a| key_in_field(&a.key, field)).collect();
    for id in chain {
        if let Some(a) = candidates.iter().find(|a| a.shells.iter().any(|s| s == id)) {
            return Some(a);
        }
    }
    candidates.into_iter().find(|a| a.shells.is_empty())
}

/// Read exactly 4 hex digits starting at `*i`, advancing `*i` past them.
fn read_hex4(chars: &[char], i: &mut usize, line: usize) -> Result<u32, SpecError> {
    let mut code: u32 = 0;
    for _ in 0..4 {
        let Some(&h) = chars.get(*i) else {
            return Err(SpecError::new(line, "truncated \\u escape"));
        };
        let Some(d) = h.to_digit(16) else {
            return Err(SpecError::new(line, "non-hex digit in \\u escape"));
        };
        code = code * 16 + d;
        *i += 1;
    }
    Ok(code)
}

/// Decode a JSON string literal (including the surrounding quotes) to its text.
/// Used for `stdout-json:`/`stderr-json:` values (empty strings, embedded NULs,
/// no-trailing-newline cases). A `\uD800`-`\uDBFF` high surrogate is combined with
/// a following `\uDC00`-`\uDFFF` low surrogate into the supplementary code point
/// (Oils encodes non-BMP bytes in the real corpus this way); an unpaired surrogate
/// becomes U+FFFD. Indexed over a `char` vector so the surrogate pair can look
/// ahead without consuming the second escape when it turns out not to pair.
fn json_decode(value: &str, line: usize) -> Result<String, SpecError> {
    let chars: Vec<char> = value.trim().chars().collect();
    let mut i = 0usize;
    if chars.first() != Some(&'"') {
        return Err(SpecError::new(line, "json value must start with '\"'"));
    }
    i += 1;
    let mut out = String::new();
    loop {
        let Some(&c) = chars.get(i) else {
            return Err(SpecError::new(line, "unterminated json string"));
        };
        i += 1;
        match c {
            '"' => {
                // The value was trimmed, so the closing quote must be the last
                // char; anything after it is malformed (fail loud, don't truncate).
                if chars.get(i).is_some() {
                    return Err(SpecError::new(line, "trailing content after json string"));
                }
                return Ok(out);
            }
            '\\' => {
                let Some(&esc) = chars.get(i) else {
                    return Err(SpecError::new(line, "trailing backslash in json string"));
                };
                i += 1;
                match esc {
                    '"' => out.push('"'),
                    '\\' => out.push('\\'),
                    '/' => out.push('/'),
                    'n' => out.push('\n'),
                    't' => out.push('\t'),
                    'r' => out.push('\r'),
                    'b' => out.push('\u{0008}'),
                    'f' => out.push('\u{000C}'),
                    'u' => {
                        let hi = read_hex4(&chars, &mut i, line)?;
                        if (0xD800..=0xDBFF).contains(&hi) {
                            // High surrogate: pair with a following `\uXXXX` low
                            // surrogate, else emit U+FFFD for the lone high one and
                            // leave the second escape (if any) for normal decoding.
                            if chars.get(i) == Some(&'\\') && chars.get(i + 1) == Some(&'u') {
                                let after_hi = i;
                                i += 2;
                                let lo = read_hex4(&chars, &mut i, line)?;
                                if (0xDC00..=0xDFFF).contains(&lo) {
                                    let cp = 0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00);
                                    out.push(char::from_u32(cp).unwrap_or('\u{FFFD}'));
                                } else {
                                    out.push('\u{FFFD}');
                                    i = after_hi; // reparse the second escape normally
                                }
                            } else {
                                out.push('\u{FFFD}');
                            }
                        } else if (0xDC00..=0xDFFF).contains(&hi) {
                            out.push('\u{FFFD}'); // lone low surrogate
                        } else {
                            out.push(char::from_u32(hi).unwrap_or('\u{FFFD}'));
                        }
                    }
                    other => {
                        return Err(SpecError::new(line, format!("invalid json escape \\{other}")));
                    }
                }
            }
            other => out.push(other),
        }
    }
}

fn expected_stream(ann: &Annotation, line: usize) -> Result<String, SpecError> {
    if ann.is_block {
        // Block body is already verbatim with per-line trailing newlines.
        Ok(ann.value.clone())
    } else if ann.key.ends_with("-json") {
        json_decode(&ann.value, line)
    } else {
        // Single-line form: the value plus the implicit trailing newline.
        Ok(format!("{}\n", ann.value))
    }
}

/// Resolve the effective expectation for `case` under `chain`.
pub fn resolve(case: &SpecCase, chain: &[&str]) -> Result<Expected, SpecError> {
    let status = match pick(case, chain, Field::Status) {
        Some(a) => a
            .value
            .trim()
            .parse::<i32>()
            .map_err(|_| SpecError::new(case.line, format!("non-integer status {:?}", a.value)))?,
        None => 0,
    };
    let stdout = match pick(case, chain, Field::Stdout) {
        Some(a) => Some(expected_stream(a, case.line)?),
        None => None,
    };
    let stderr = match pick(case, chain, Field::Stderr) {
        Some(a) => Some(expected_stream(a, case.line)?),
        None => None,
    };
    Ok(Expected { status, stdout, stderr })
}

// ---- running -------------------------------------------------------------

/// Compare a shell's observed result against the resolved expectation.
fn evaluate(name: &str, expected: &Expected, status: i32, stdout: &[u8], stderr: &[u8]) -> CaseOutcome {
    let mut fails: Vec<String> = Vec::new();
    if status != expected.status {
        fails.push(format!("status: expected {}, got {}", expected.status, status));
    }
    if let Some(exp) = &expected.stdout {
        if exp.as_bytes() != stdout {
            fails.push(format!(
                "stdout: expected {:?}, got {:?}",
                exp,
                String::from_utf8_lossy(stdout)
            ));
        }
    }
    if let Some(exp) = &expected.stderr {
        if exp.as_bytes() != stderr {
            fails.push(format!(
                "stderr: expected {:?}, got {:?}",
                exp,
                String::from_utf8_lossy(stderr)
            ));
        }
    }
    let passed = fails.is_empty();
    CaseOutcome {
        name: name.to_string(),
        passed,
        detail: if passed { None } else { Some(fails.join("; ")) },
    }
}

/// Run one case: feed its code to `shell` via `-c`, in a cleared environment, and
/// diff stdout/stderr/status against the resolved expectation.
pub fn run_case(
    shell: &Path,
    case: &SpecCase,
    chain: &[&str],
) -> Result<CaseOutcome, Box<dyn std::error::Error>> {
    let expected = resolve(case, chain)?;
    // env_clear for determinism; the seed corpus uses only shell builtins, so no
    // PATH is needed. A future real td-sh that execs externals will take a
    // configurable environment here.
    let output = Command::new(shell)
        .arg("-c")
        .arg(&case.code)
        .env_clear()
        .stdin(Stdio::null())
        .output()?;
    // status.code() is None when killed by a signal; -1 can match no expectation.
    let status = output.status.code().unwrap_or(-1);
    Ok(evaluate(&case.name, &expected, status, &output.stdout, &output.stderr))
}

/// Parse and run every case in a spec file.
pub fn run_file(
    shell: &Path,
    path: &Path,
    chain: &[&str],
) -> Result<Vec<CaseOutcome>, Box<dyn std::error::Error>> {
    let text = std::fs::read_to_string(path)?;
    let cases = parse_spec(&text)?;
    let mut outcomes = Vec::with_capacity(cases.len());
    for case in &cases {
        outcomes.push(run_case(shell, case, chain)?);
    }
    Ok(outcomes)
}

/// Run every `*.test.sh` file in `dir` (non-recursive, sorted by name).
pub fn run_dir(
    shell: &Path,
    dir: &Path,
    chain: &[&str],
) -> Result<Vec<CaseOutcome>, Box<dyn std::error::Error>> {
    let mut paths: Vec<std::path::PathBuf> = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("sh")
            && path.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.ends_with(".test.sh"))
        {
            paths.push(path);
        }
    }
    paths.sort();
    let mut outcomes = Vec::new();
    for path in &paths {
        outcomes.extend(run_file(shell, path, chain)?);
    }
    Ok(outcomes)
}

/// (passed, total) over a set of outcomes.
pub fn tally(outcomes: &[CaseOutcome]) -> (usize, usize) {
    let passed = outcomes.iter().filter(|o| o.passed).count();
    (passed, outcomes.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
## compare_shells: bash dash mksh ash

#### echo two words
echo hello world
## STDOUT:
hello world
## END

#### exit status is honored
false
exit 3
## status: 3
## stdout-json: \"\"

#### per-shell override picks dash over default
echo x
## STDOUT:
default-ideal
## END
## OK dash STDOUT:
dash-says
## END
";

    #[test]
    fn parses_all_cases() -> Result<(), SpecError> {
        let cases = parse_spec(SAMPLE)?;
        assert_eq!(cases.len(), 3);
        assert_eq!(cases.first().map(|c| c.name.as_str()), Some("echo two words"));
        assert_eq!(cases.first().map(|c| c.code.as_str()), Some("echo hello world"));
        Ok(())
    }

    #[test]
    fn parses_heredoc_hash_body_lines() -> Result<(), SpecError> {
        // A `#`-leading here-doc body line must survive verbatim into the case code
        // (Oils bulk-import correctness): it is shell script text, not a spec
        // comment, so the parser must not drop it.
        let spec = "\
#### heredoc keeps a hash-leading line
cat <<'EOF2'
#!/bin/sh
echo real-line
EOF2
## STDOUT:
#!/bin/sh
echo real-line
## END
";
        let cases = parse_spec(spec)?;
        let c = cases.first().ok_or_else(|| SpecError::new(0, "missing case"))?;
        assert_eq!(c.code, "cat <<'EOF2'\n#!/bin/sh\necho real-line\nEOF2");
        Ok(())
    }

    #[test]
    fn flags_unrecognized_annotation_key() -> Result<(), SpecError> {
        // A typo of an assertion key must be surfaced, not silently resolved to
        // the default golden.
        let spec = "\
#### typo'd status key
false
## stats: 3
";
        let cases = parse_spec(spec)?;
        let c = cases.first().ok_or_else(|| SpecError::new(0, "missing case"))?;
        assert_eq!(c.unrecognized_keys(), vec!["stats"]);
        // A well-formed case reports none.
        let ok = parse_spec("#### ok\nfalse\n## status: 1\n")?;
        let oc = ok.first().ok_or_else(|| SpecError::new(0, "missing case"))?;
        assert!(oc.unrecognized_keys().is_empty());
        Ok(())
    }

    #[test]
    fn resolves_block_stdout_and_default_status() -> Result<(), SpecError> {
        let cases = parse_spec(SAMPLE)?;
        let c = cases.first().ok_or_else(|| SpecError::new(0, "missing case"))?;
        let e = resolve(c, ASH_DASH_CHAIN)?;
        assert_eq!(e.status, 0);
        assert_eq!(e.stdout.as_deref(), Some("hello world\n"));
        assert_eq!(e.stderr, None);
        Ok(())
    }

    #[test]
    fn resolves_status_and_json_empty_stdout() -> Result<(), SpecError> {
        let cases = parse_spec(SAMPLE)?;
        let c = cases.get(1).ok_or_else(|| SpecError::new(0, "missing case"))?;
        let e = resolve(c, ASH_DASH_CHAIN)?;
        assert_eq!(e.status, 3);
        assert_eq!(e.stdout.as_deref(), Some("")); // stdout-json "" => empty, no newline
        Ok(())
    }

    #[test]
    fn dash_override_wins_over_default_block() -> Result<(), SpecError> {
        let cases = parse_spec(SAMPLE)?;
        let c = cases.get(2).ok_or_else(|| SpecError::new(0, "missing case"))?;
        // Our chain is [ash, dash]; no ash block, so the dash override applies.
        let e = resolve(c, ASH_DASH_CHAIN)?;
        assert_eq!(e.stdout.as_deref(), Some("dash-says\n"));
        // A chain without dash falls back to the unqualified default.
        let e2 = resolve(c, &["mksh"])?;
        assert_eq!(e2.stdout.as_deref(), Some("default-ideal\n"));
        Ok(())
    }

    #[test]
    fn json_decode_handles_escapes() -> Result<(), SpecError> {
        assert_eq!(json_decode("\"a\\nb\"", 0)?, "a\nb");
        assert_eq!(json_decode("\"\\u0041\"", 0)?, "A");
        assert_eq!(json_decode("\"\"", 0)?, "");
        assert!(json_decode("no-quote", 0).is_err());
        assert!(json_decode("\"a\"junk", 0).is_err()); // trailing content rejected
        Ok(())
    }

    #[test]
    fn json_decode_combines_surrogate_pairs() -> Result<(), SpecError> {
        // U+1F600 GRINNING FACE encoded as a UTF-16 surrogate pair — the shape the
        // real Oils corpus uses for non-BMP bytes in `stdout-json`.
        assert_eq!(json_decode("\"\\uD83D\\uDE00\"", 0)?, "\u{1F600}");
        // A lone high surrogate is replaced, and a following non-pairing escape is
        // still decoded on its own.
        assert_eq!(json_decode("\"\\uD83D\\u0041\"", 0)?, "\u{FFFD}A");
        // A lone high surrogate at end-of-string, and a lone low surrogate.
        assert_eq!(json_decode("\"\\uD83D\"", 0)?, "\u{FFFD}");
        assert_eq!(json_decode("\"\\uDE00\"", 0)?, "\u{FFFD}");
        Ok(())
    }

    #[test]
    fn evaluate_reds_a_stdout_mismatch() {
        // A run that exits 0 but prints nothing must fail a case expecting output —
        // the harness's own red-detection, independent of any real shell.
        let expected = Expected { status: 0, stdout: Some("hello world\n".into()), stderr: None };
        let out = evaluate("echo two words", &expected, 0, b"", b"");
        assert!(!out.passed);
        assert!(out.detail.is_some());
    }

    #[test]
    fn evaluate_passes_when_output_matches() {
        let expected = Expected { status: 0, stdout: Some("hi\n".into()), stderr: None };
        let out = evaluate("x", &expected, 0, b"hi\n", b"");
        assert!(out.passed);
        assert_eq!(out.detail, None);
    }
}

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
//!   ## STDOUT:  ...  ## END      a multiline expected-stdout block
//!   ## STDERR:  ...  ## END      a multiline expected-stderr block
//!                                 (END is optional/lenient, per Oils: the block
//!                                 also ends at the next `##`/`####` or EOF, and
//!                                 `## END:` with trailing text still terminates.
//!                                 A `#` line inside a block is a COMMENT, so the
//!                                 format cannot express expected output whose
//!                                 first non-blank character is `#`)
//!   ## <QUAL> <shells> <k>: v    a per-shell override (QUAL ∈ OK|OK-N|BUG|BUG-N|N-I)
//!   ## <QUAL> <shells> STDOUT:   a per-shell multiline override; shells `/`-separated
//!   #  (single hash) / blank      an ignored comment (also a no-op shell comment)
//!   <anything else>              a line of the case's shell code (verbatim)
//! ```
//! Bare `## key: value` lines before the first `####` are file-level metadata.
//! All are ignored except `## compare_shells:`, which names the shells Oils ran
//! the file against and so bounds how far the identity chain may fall through
//! (see `effective_chain`).
//!
//! Golden resolution: for each of {status, stdout, stderr} independently, the
//! effective expectation is the qualified annotation whose shell list contains
//! the earliest identity in the target chain (ash, then dash) — but the chain
//! reaches only as far as the first identity the FILE compared
//! (`effective_chain`) — else the unqualified (default/ideal) annotation. Matching dash/ash — not the osh ideal —
//! is what a busybox-`sh` replacement must do. Unspecified stdout/stderr is "not
//! asserted"; unspecified status defaults to 0 (Oils semantics). The qualifier
//! kind (OK/N-I/BUG) does not change WHICH value applies to a shell, only records
//! why the shells legitimately differ, so resolution keys on the shell list.
#![deny(unsafe_code)]

use std::collections::BTreeSet;
use std::io::Read;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Per-case wall-clock cap. A bulk corpus contains cases that block forever
/// under a POSIX shell (`read` with no input, an unbounded loop); without this
/// one hung case would wedge the shared land-on-green gate. A timed-out case is
/// reported as a failure (its known-hang entry belongs on the `skip` list so it
/// is not re-run every gate).
const CASE_TIMEOUT: Duration = Duration::from_secs(10);

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
    /// The file's `## compare_shells:` list, stamped onto every case in it.
    /// Empty when the file declares none. See `effective_chain`.
    compare_shells: Vec<String>,
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
    pub timed_out: bool,        // hit CASE_TIMEOUT (a typed signal, not parsed from `detail`)
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

/// A block terminator, matching Oils' `END_MULTILINE_RE` (`re.match(r'##\s+END')`):
/// two hashes, at least one space, then `END`; any trailing text (a stray `:`,
/// a comment) is ignored, as in the real corpus (`## END:`).
fn is_end_marker(line: &str) -> bool {
    match line.strip_prefix("##") {
        Some(rest) => {
            let trimmed = rest.trim_start();
            rest.len() != trimmed.len() && trimmed.starts_with("END")
        }
        None => false,
    }
}

/// Collect a `## STDOUT:`/`## STDERR:` block body (each kept line plus a trailing
/// '\n'). The body runs to an explicit `## END` (consumed), OR — since Oils makes
/// the END token optional — to the next `##` annotation / `####` case (NOT
/// consumed, so the caller re-reads it) / EOF. Returns the body and the index at
/// which the caller should resume.
///
/// A line whose first non-blank character is `#` is a COMMENT on the expectation,
/// not expected output — Oils' `_ClassifyLine` drops it with
/// `line.lstrip().startswith('#')`, so this must too or the goldens it wrote read
/// differently here. A block therefore cannot express output starting with `#`;
/// the goldens that need to (`## stdout: ##`) use the single-line form, which
/// never reaches this function.
fn read_block(lines: &[&str], start: usize) -> (String, usize) {
    let mut body = String::new();
    let mut j = start;
    while let Some(l) = lines.get(j) {
        if is_end_marker(l) {
            return (body, j + 1);
        }
        // Optional-END: a new token (annotation or case header) ends the block
        // without being consumed.
        if l.starts_with("##") || l.starts_with("####") {
            return (body, j);
        }
        if l.trim_start().starts_with('#') {
            j += 1;
            continue;
        }
        body.push_str(l);
        body.push('\n');
        j += 1;
    }
    (body, j)
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
    let mut code_inline = false; // this case's code came from `## code:` (Oils: exclusive with body code)
    let mut compare_shells: Vec<String> = Vec::new();
    let mut i = 0;

    while let Some(line) = lines.get(i) {
        // Case header (4+ leading hashes) — must precede the `##` check.
        if line.starts_with("####") {
            push_case(&mut cases, cur.take());
            let name = line.trim_start_matches('#').trim().to_string();
            cur = Some(SpecCase {
                name,
                code: String::new(),
                line: i + 1,
                compare_shells: compare_shells.clone(),
                annotations: Vec::new(),
            });
            code_inline = false;
            i += 1;
            continue;
        }

        if line.starts_with("##") {
            let content = line.get(2..).unwrap_or("").trim_start();
            let head = parse_ann_head(content);
            if is_block_opener(&head) {
                let (body, next) = read_block(&lines, i + 1);
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
            // Inline-code form: `## code: <code>` (Oils) supplies the case's shell
            // code in an annotation instead of as body lines. Record it as the
            // code, not an assertion, so the case runs and `code` never looks like
            // a typo'd assertion key.
            if head.has_colon
                && head.key == "code"
                && head.qualifier == Qualifier::Default
                && head.shells.is_empty()
            {
                if let Some(c) = cur.as_mut() {
                    // Oils treats `## code:` as mutually exclusive with body code:
                    // fail closed on the mixed shape rather than silently discard
                    // body code already collected.
                    if !c.code.is_empty() {
                        return Err(SpecError::new(i + 1, "case mixes body code with `## code:`"));
                    }
                    c.code = head.value;
                    code_inline = true;
                }
                i += 1;
                continue;
            }
            // The one piece of file-level metadata that is not prose: the shells
            // Oils actually ran this file against. It decides how far the identity
            // chain may fall through (`effective_chain`), so it is read rather
            // than skipped with the rest.
            // `cur.is_none()` is load-bearing: inside a case this key is not a
            // header but an unrecognized assertion, and swallowing it would both
            // hide it from `unrecognized_keys` and silently re-aim every LATER
            // case in the file. The qualifier/shells check mirrors `## code:`
            // above, so `## OK ash compare_shells: ..` is not read as a header
            // with its qualifier quietly dropped.
            if head.has_colon
                && cur.is_none()
                && head.key == "compare_shells"
                && head.qualifier == Qualifier::Default
                && head.shells.is_empty()
            {
                compare_shells = head.value.split_whitespace().map(str::to_string).collect();
                i += 1;
                continue;
            }
            // A single-line annotation with a colon carries an assertion.
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
                i += 1;
                continue;
            }
            // A `##` line inside a case that is neither a block opener, `## code:`,
            // an assertion, nor a consumed `## END` is malformed — Oils raises on
            // it (`Invalid ## line`). Fail closed so a typo (`##END`, `## STODUT`)
            // can't silently truncate a golden. Before the first `####` (cur None)
            // a bare `##` line is file-level prose/metadata — ignore it.
            if cur.is_some() {
                return Err(SpecError::new(i + 1, format!("invalid `##` line: {line:?}")));
            }
            i += 1;
            continue;
        }

        // A code line — verbatim, preserving indentation (here-docs depend on it).
        // A `#` line is NOT dropped here, and this is a DELIBERATE divergence: Oils
        // strips it from code as well as from goldens, but a here-doc or quoted body
        // line (`#!/bin/sh`) is code too, and truncating one corrupts the case. It
        // costs the one golden that counts stripped lines; see `read_block`. Only
        // collect code BEFORE a case's first annotation: the lines after the
        // annotations (blank separators before the next `####`) are not code.
        if let Some(c) = cur.as_mut() {
            if c.annotations.is_empty() {
                if code_inline {
                    // `## code:` already supplied the code; only trailing blanks
                    // may follow. Real body code after it is the mixed shape Oils
                    // rejects — fail closed (mirror of the check above).
                    if !line.trim().is_empty() {
                        return Err(SpecError::new(i + 1, "case mixes `## code:` with body code"));
                    }
                } else {
                    if !c.code.is_empty() {
                        c.code.push('\n');
                    }
                    c.code.push_str(line);
                }
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
    /// The file's `## compare_shells:` tokens, for a corpus validator to check
    /// the spelling of: an unrecognisable one does not fail, it silently stops
    /// bounding the chain for that whole file.
    pub fn compare_shells(&self) -> &[String] {
        &self.compare_shells
    }

    pub fn unrecognized_keys(&self) -> Vec<&str> {
        self.annotations
            .iter()
            .filter(|a| !is_assertion_key(&a.key))
            .map(|a| a.key.as_str())
            .collect()
    }
}

/// Whether a `compare_shells` token names `id`. Oils writes versioned identities
/// (`bash-4.4` and `zsh-5.9` are both in this corpus), and a file that compared
/// one compared that shell; without this an `ash-1.37` header would switch the
/// bound off for its whole file with nothing to show for it.
fn names_identity(token: &str, id: &str) -> bool {
    token.strip_prefix(id).is_some_and(|rest| rest.is_empty() || rest.starts_with('-'))
}

/// `chain`, truncated after the first identity the FILE actually compared.
///
/// For a shell the file compared, the format already designates that shell's
/// golden: its own annotation if it has one, the unqualified block otherwise.
/// A LATER identity's block is another shell's divergence and was never a
/// statement about this one, so the chain must not reach it. (The tempting
/// stronger reading -- that silence means agreement -- is false, and this same
/// corpus refutes it: ash still fails 1263 cases whose files list ash. Silence
/// says nothing about whether ash MATCHES the default block, only that the
/// default block is the golden it is held to.) A file that never ran the shell
/// designates nothing, so there the fallthrough stands as the same-lineage
/// heuristic it always was.
fn effective_chain<'a>(case: &SpecCase, chain: &'a [&'a str]) -> &'a [&'a str] {
    match chain
        .iter()
        .position(|id| case.compare_shells.iter().any(|t| names_identity(t, id)))
    {
        Some(i) => chain.get(..=i).unwrap_or(chain),
        None => chain,
    }
}

/// Pick the annotation that applies to `chain` for `field`: the earliest
/// identity of the EFFECTIVE chain that some annotation names wins; otherwise
/// the unqualified default. Per field, so one field may resolve to a per-shell
/// block while another falls to the default.
fn pick<'a>(case: &'a SpecCase, chain: &[&str], field: Field) -> Option<&'a Annotation> {
    let candidates: Vec<&Annotation> =
        case.annotations.iter().filter(|a| key_in_field(&a.key, field)).collect();
    for id in effective_chain(case, chain) {
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
        timed_out: false,
    }
}

/// Grace to collect a drained stream AFTER the child has exited or been killed.
/// Normally a reader hits EOF the instant the child's last pipe writer closes, so
/// this is not consumed; it only bounds the pathological case where a backgrounded
/// descendant inherited the pipe and outlives the child. We abandon that reader
/// (leaking one thread blocked on `read`) rather than hang the shared gate — such
/// a case yields truncated output and belongs on the `skip` list. Generous so a
/// loaded host's post-exit EOF is never mistaken for a stuck reader.
const DRAIN_GRACE: Duration = Duration::from_secs(2);

/// Read `stream` to EOF on its own thread, delivering the bytes once on a channel.
/// A read error or a pipe that never reaches EOF yields no value (the caller treats
/// a missing value as empty), so the caller never blocks on the read itself.
fn drain_pipe<R: Read + Send + 'static>(stream: Option<R>) -> std::sync::mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = std::sync::mpsc::channel();
    match stream {
        Some(mut s) => {
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                let _ = s.read_to_end(&mut buf); // bytes read before an error are retained
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
/// `(status_code_or_-1, timed_out, stdout, stderr)`. A killed child reports code
/// `-1` (no exit code), which matches no expected status, so the case fails.
///
/// Both pipes are drained on reader threads started BEFORE the wait, so a case
/// whose output exceeds the pipe buffer (~64 KiB) keeps running instead of
/// deadlocking on write. On expiry the child is SIGKILLed and reaped. The final
/// collect is itself bounded by `DRAIN_GRACE`: a descendant that inherited the
/// pipe and outlives the child cannot make this block forever — we abandon the
/// reader and return what was captured. So this always returns in bounded time.
fn wait_and_capture(mut child: Child, timeout: Duration) -> std::io::Result<(i32, bool, Vec<u8>, Vec<u8>)> {
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

/// A throwaway working directory for one case: a case that redirects to a file
/// (`echo x > f`) would otherwise litter — or clobber — the gate's working tree.
/// Each case runs in its own temp dir, removed on drop (best-effort). Named by
/// pid + a per-process counter so parallel gate processes and successive cases
/// never collide.
struct CaseWorkdir(std::path::PathBuf);

impl CaseWorkdir {
    fn new() -> std::io::Result<Self> {
        use std::os::unix::fs::DirBuilderExt;
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        // Absolute, because the child's cwd MOVES into this dir: a relative
        // `TMPDIR` would make the exported `PATH` resolve against the case cwd
        // instead of naming the sibling that holds the shell, and every `$SH`
        // would 127 with nothing to say why.
        let base = std::path::absolute(std::env::temp_dir())?;
        let pid = std::process::id();
        // Exclusive create (not `create_dir_all`): the name is predictable, so a
        // symlink planted at it, or a dir leaked by a crashed run whose pid the OS
        // reused, must red the create rather than be silently adopted (stale files,
        // an escaped cwd). `create` fails on any existing leaf; a fresh `seq`
        // retries past a genuine collision. 0700 keeps the throwaway dir owner-only.
        loop {
            let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let dir = base.join(format!("td-sh-case-{pid}-{seq}"));
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
        let _ = std::fs::remove_dir_all(&self.0); // best-effort; a case may have left files
    }
}

/// Run one case: feed its code to `shell` via `-c`, in a cleared environment and
/// an isolated working directory, and diff stdout/stderr/status against the
/// resolved expectation. Bounded by `CASE_TIMEOUT`; a timed-out case fails with a
/// `timed out` detail.
pub fn run_case(
    shell: &Path,
    case: &SpecCase,
    chain: &[&str],
) -> Result<CaseOutcome, Box<dyn std::error::Error>> {
    let expected = resolve(case, chain)?;
    // env_clear for determinism; a case that execs an external will fail closed
    // (a PATH holding only the shell itself) rather than leak the host
    // environment into the result. The workdir is a throwaway temp dir (dropped
    // after the run) so a case that writes a file cannot touch the gate's tree.
    //
    // Absolutize the shell first: `current_dir` moves the child's cwd, so a
    // relative `shell` would otherwise resolve against the temp dir and not be
    // found.
    let shell = std::fs::canonicalize(shell)?;
    let workdir = CaseWorkdir::new()?;
    // `$TMP` is what Oils' own runner exports; without it every `$TMP/f` in the
    // corpus becomes `/f`, which fails on permissions or reads whatever the build
    // host happens to have there. It is a SIBLING of the cwd, as it is under Oils:
    // inside it, a case that writes `$TMP/f` and then globs `.` would see the two
    // merged (which is what globignore's `*` cases assert on).
    let cwd = workdir.0.join("cwd");
    let tmpdir = workdir.0.join("tmp");
    std::fs::create_dir(&cwd)?;
    std::fs::create_dir(&tmpdir)?;
    // `$SH` is the shell's IDENTITY, not its path, because 359 cases across 74
    // corpus files open with `case $SH in dash|ash) exit ;; esac` and compare it
    // to a bare name. Exporting an absolute path makes every one of those guards
    // miss and run a body the golden says was never reached. It has to stay
    // executable too (682 uses, mostly `$SH -c ...`), so a third sibling dir holds
    // a link under that name and is the whole of PATH -- which keeps `env_clear`'s
    // point, since a case reaching for `ls` still finds nothing.
    let identity = chain.first().copied().unwrap_or("sh");
    // `chain` is caller-supplied, and the identity is about to become a path
    // component AND the value of `$SH`. An absolute one would make `join` discard
    // the bindir entirely, `..` would escape it, and any slash would make `$SH`
    // bypass PATH and name a host file directly.
    if identity.is_empty() || identity.contains('/') || identity == "." || identity == ".." {
        return Err(format!("shell identity {identity:?} is not a single path component").into());
    }
    let bindir = workdir.0.join("bin");
    std::fs::create_dir(&bindir)?;
    // A link, not a copy. A copy looks safer -- a case could truncate the entry
    // without touching the binary under test -- but the kernel already closes
    // that: the entry IS the executable the case is running, so a write to it is
    // ETXTBSY (asserted below in tests). What a per-case copy does buy is a
    // second `fs::copy` holding a write fd while cargo's other test threads fork,
    // and their inherited fd turns unrelated execs into "Text file busy": 1 to 4
    // of the 11 conformance tests failed per run, differently each time, and it
    // woke the same latent race in `ProbeDir`.
    let entry = bindir.join(identity);
    std::os::unix::fs::symlink(&shell, &entry)?;
    // Spawn the ENTRY, not the canonicalized original, so the top-level shell and
    // a nested `$SH -c ..` are the same argv[0]. It matters for a multicall
    // binary: `canonicalize` resolves a busybox `ash` link back to `busybox`,
    // which then answers `-c: applet not found` and grades as a wrecked shell,
    // while the nested call through this entry works. Identical overlay for a
    // single-purpose shell; the difference is only that consistency.
    let child = Command::new(&entry)
        .arg("-c")
        .arg(&case.code)
        .env_clear()
        .env("SH", identity)
        .env("PATH", &bindir)
        .env("TMP", &tmpdir)
        .current_dir(&cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let (status, timed_out, stdout, stderr) = wait_and_capture(child, CASE_TIMEOUT)?;
    let mut outcome = evaluate(&case.name, &expected, status, &stdout, &stderr);
    if timed_out {
        outcome.passed = false;
        outcome.timed_out = true;
        outcome.detail = Some(match outcome.detail {
            Some(d) => format!("timed out after {}s; {d}", CASE_TIMEOUT.as_secs()),
            None => format!("timed out after {}s", CASE_TIMEOUT.as_secs()),
        });
    }
    Ok(outcome)
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

/// Every `*.test.sh` file in `dir` (non-recursive, sorted by name). The `.txt`
/// expectations overlay and any other non-corpus file are excluded, so vendored
/// spec files and td-sh's known-gap manifest can share the directory. Public so the
/// overlay generator (examples/gen_expectations.rs) enumerates the exact same set the
/// gate does — one source of truth, so no file can be in one but not the other.
pub fn spec_paths(dir: &Path) -> std::io::Result<Vec<std::path::PathBuf>> {
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
    Ok(paths)
}

/// Run every `*.test.sh` file in `dir` (non-recursive, sorted by name).
pub fn run_dir(
    shell: &Path,
    dir: &Path,
    chain: &[&str],
) -> Result<Vec<CaseOutcome>, Box<dyn std::error::Error>> {
    let mut outcomes = Vec::new();
    for path in &spec_paths(dir)? {
        outcomes.extend(run_file(shell, path, chain)?);
    }
    Ok(outcomes)
}

/// (passed, total) over a set of outcomes.
pub fn tally(outcomes: &[CaseOutcome]) -> (usize, usize) {
    let passed = outcomes.iter().filter(|o| o.passed).count();
    (passed, outcomes.len())
}

// ---- expectations overlay ------------------------------------------------

/// How a case's observed result relates to td-sh's declared expectation for it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Disposition {
    /// Ran, matched, and was not listed — real green coverage.
    Pass,
    /// Ran, mismatched, and is listed `xfail` — a known gap, tolerated.
    XFail,
    /// Ran and matched, but is listed `xfail` — the gap closed; the stale entry
    /// must be removed. Reds the gate so progress is always recorded.
    XPass,
    /// Ran, mismatched, and was not listed — a regression. Reds the gate.
    Fail,
    /// Listed `skip`; not run at all (a known hang or a case that needs a
    /// facility the `-c` harness does not provide).
    Skip,
}

/// td-sh's known-gap manifest: cases it cannot yet pass (`xfail`) or must not run
/// (`skip`). Kept OUTSIDE the spec files so the vendored Oils corpus stays
/// byte-for-byte pristine — the overlay is td-sh's view, not an edit to upstream.
#[derive(Clone, Debug, Default)]
pub struct Expectations {
    xfail: BTreeSet<String>,
    skip: BTreeSet<String>,
}

/// The manifest key for a case: `<spec-file-basename>::<case description>`.
pub fn case_key(file: &str, case_name: &str) -> String {
    format!("{file}::{case_name}")
}

/// Overlay keys for a whole file's cases, in file order. A description that
/// appears more than once in the file cannot be told apart by `case_key` alone,
/// so the 2nd and later occurrences get an ` ##N` occurrence suffix (the first
/// keeps the bare key). This makes every case individually addressable, so a
/// duplicate description can be xfail'd/tracked per-occurrence instead of collapsing
/// two cases onto one entry. Both the gate (`run_dir_classified`) and the overlay
/// generator derive keys through here, so the occurrence numbering matches.
pub fn case_keys(file: &str, cases: &[SpecCase]) -> Vec<String> {
    // Single O(n) pass: a running per-description count assigns the suffix. The 1st
    // occurrence keeps the bare key; the Nth (N>=2) gets ` ##N`. (No `total` check is
    // needed — reaching count>0 already means the description repeats.)
    let mut seen: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    let mut keys = Vec::with_capacity(cases.len());
    for case in cases {
        let count = seen.entry(case.name.as_str()).or_insert(0);
        if *count == 0 {
            keys.push(case_key(file, &case.name));
        } else {
            keys.push(format!("{file}::{} ##{}", case.name, *count + 1));
        }
        *count += 1;
    }
    keys
}

impl Expectations {
    /// Parse the overlay. Each non-blank, non-`#` line is
    /// `<xfail|skip> <file>::<case description>`; the key runs to end-of-line so a
    /// description may contain spaces. Duplicate or contradictory entries, an
    /// unknown disposition, or a key without `::` are hard errors — a sloppy
    /// manifest must not silently mis-tolerate a case.
    pub fn parse(text: &str) -> Result<Self, SpecError> {
        let mut xfail = BTreeSet::new();
        let mut skip = BTreeSet::new();
        for (idx, raw) in text.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let ln = idx + 1;
            let (disp, key) = line
                .split_once(char::is_whitespace)
                .ok_or_else(|| SpecError::new(ln, "expectation needs `<xfail|skip> <file>::<case>`"))?;
            let key = key.trim();
            if !key.contains("::") {
                return Err(SpecError::new(ln, "expectation key must be `<file>::<case>`"));
            }
            let inserted = match disp {
                "xfail" => xfail.insert(key.to_string()),
                "skip" => skip.insert(key.to_string()),
                other => {
                    return Err(SpecError::new(ln, format!("unknown disposition {other:?} (want xfail|skip)")));
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

    /// All listed keys (xfail then skip), for stale-entry detection.
    fn keys(&self) -> impl Iterator<Item = &String> {
        self.xfail.iter().chain(self.skip.iter())
    }
}

/// One case run, classified against the overlay.
#[derive(Clone, Debug)]
pub struct ClassifiedOutcome {
    pub key: String,
    pub disposition: Disposition,
    pub detail: Option<String>, // mismatch text for Fail/XFail
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
    /// manifest keys are surfaced separately (see `run_dir_classified`) and must
    /// also be empty for a clean gate.
    pub fn is_green(&self) -> bool {
        self.fail == 0 && self.xpass == 0
    }
}

/// Tally a classified run by disposition.
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

/// Classify one already-run outcome against the overlay for a given key.
fn classify(key: String, outcome: &CaseOutcome, exp: &Expectations) -> ClassifiedOutcome {
    let disposition = match (outcome.passed, exp.is_xfail(&key)) {
        (true, false) => Disposition::Pass,
        (false, true) => Disposition::XFail,
        (true, true) => Disposition::XPass,
        (false, false) => Disposition::Fail,
    };
    ClassifiedOutcome { key, disposition, detail: outcome.detail.clone() }
}

/// Backstop against a genuine overlay-key collision. `case_keys` occurrence-qualifies
/// duplicate descriptions, so within one corpus every case yields a distinct key and
/// `is_new` (from the `seen` set) is always true. A `false` therefore means two cases
/// mapped to the SAME key — a real bug, or an upstream description literally colliding
/// with an ` ##N` suffix — which would let one overlay entry (or one run result)
/// stand in for two cases. Red the gate unconditionally so the collision cannot hide,
/// whether or not the key is listed.
fn duplicate_conflicts(is_new: bool) -> bool {
    !is_new
}

/// Run every case under `dir`, classifying each against `exp`. `skip` cases are
/// not executed. Returns the classified outcomes and the list of overlay keys
/// that matched no case (stale entries — a typo, or a renamed/removed upstream
/// case); a caller enforcing land-on-green must red the gate when it is non-empty.
pub fn run_dir_classified(
    shell: &Path,
    dir: &Path,
    chain: &[&str],
    exp: &Expectations,
) -> Result<(Vec<ClassifiedOutcome>, Vec<String>), Box<dyn std::error::Error>> {
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut out: Vec<ClassifiedOutcome> = Vec::new();
    for path in &spec_paths(dir)? {
        let file = path.file_name().and_then(|n| n.to_str()).unwrap_or_default().to_string();
        let text = std::fs::read_to_string(path)?;
        let cases = parse_spec(&text)?;
        let keys = case_keys(&file, &cases);
        for (case, key) in cases.iter().zip(keys) {
            let is_new = seen.insert(key.clone());
            if duplicate_conflicts(is_new) {
                out.push(ClassifiedOutcome {
                    key,
                    disposition: Disposition::Fail,
                    detail: Some("two cases map to the same overlay key (collision) — cannot disambiguate".into()),
                });
                continue;
            }
            if exp.is_skip(&key) {
                out.push(ClassifiedOutcome { key, disposition: Disposition::Skip, detail: None });
                continue;
            }
            let outcome = run_case(shell, case, chain)?;
            out.push(classify(key, &outcome, exp));
        }
    }
    let stale: Vec<String> = exp.keys().filter(|k| !seen.contains(*k)).cloned().collect();
    Ok((out, stale))
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
        // The GOLDEN cannot say the same thing: a block drops its `#` line, so this
        // case is unmatchable in block form. Asserted so the limitation is visible
        // rather than lurking in a fixture -- a real one would use `stdout-json:`.
        assert_eq!(
            resolve(c, ASH_DASH_CHAIN)?.stdout.as_deref(),
            Some("echo real-line\n")
        );
        Ok(())
    }

    #[test]
    fn inline_code_annotation_supplies_the_case_code() -> Result<(), SpecError> {
        // Oils' `## code:` form: the shell code is in the annotation, not body
        // lines. It must become the case code and NOT be flagged as an unknown key.
        let spec = "\
#### Unterminated single quote
## code: ls foo bar '
## status: 2
";
        let cases = parse_spec(spec)?;
        let c = cases.first().ok_or_else(|| SpecError::new(0, "missing case"))?;
        assert_eq!(c.code, "ls foo bar '");
        assert!(c.unrecognized_keys().is_empty());
        assert_eq!(resolve(c, ASH_DASH_CHAIN)?.status, 2);
        Ok(())
    }

    #[test]
    fn block_end_is_lenient_and_optional() -> Result<(), SpecError> {
        // `## END:` (trailing colon) still terminates, per Oils' `re.match`.
        let colon = parse_spec("#### x\necho hi\n## STDOUT:\nhi\n## END:\n")?;
        let c = colon.first().ok_or_else(|| SpecError::new(0, "missing case"))?;
        assert_eq!(resolve(c, ASH_DASH_CHAIN)?.stdout.as_deref(), Some("hi\n"));

        // END is optional: a following `##` annotation ends the block, and an
        // empty per-shell block (the real `## N-I dash STDOUT:` / `## END:` shape)
        // resolves to empty output for that shell.
        let optional = parse_spec(
            "#### y\necho hi\n## STDOUT:\nideal\n## N-I dash STDOUT:\n## END:\n",
        )?;
        let c = optional.first().ok_or_else(|| SpecError::new(0, "missing case"))?;
        // Chain [ash, dash]: the dash block (empty) wins over the default.
        assert_eq!(resolve(c, ASH_DASH_CHAIN)?.stdout.as_deref(), Some(""));
        // A chain without dash falls back to the default block.
        assert_eq!(resolve(c, &["mksh"])?.stdout.as_deref(), Some("ideal\n"));
        Ok(())
    }

    #[test]
    fn block_comment_lines_are_not_expected_output() -> Result<(), SpecError> {
        // A `#` line inside an expectation block annotates the golden; Oils
        // writes these by running the shells, so treating one as output makes
        // the case unmatchable by any of them.
        let spec = "#### x\necho hi\n## STDOUT:\n# a note about dash\nhi\n## END\n";
        let c = parse_spec(spec)?;
        let c = c.first().ok_or_else(|| SpecError::new(0, "missing case"))?;
        assert_eq!(resolve(c, ASH_DASH_CHAIN)?.stdout.as_deref(), Some("hi\n"));

        // Indented too, as Oils' `line.lstrip().startswith('#')` does. The format
        // therefore cannot express expected output beginning with `#`.
        let indented = parse_spec("#### y\nx\n## STDOUT:\n  # note\nhi\n## END\n")?;
        let c = indented.first().ok_or_else(|| SpecError::new(0, "missing case"))?;
        assert_eq!(resolve(c, ASH_DASH_CHAIN)?.stdout.as_deref(), Some("hi\n"));

        // The corpus closes two blocks with a mistyped `# END`, which this rule
        // absorbs -- the following annotation is what actually ends the block.
        let typo = parse_spec("#### z\nx\n## STDOUT:\nhi\n# END\n## status: 0\n")?;
        let c = typo.first().ok_or_else(|| SpecError::new(0, "missing case"))?;
        let e = resolve(c, ASH_DASH_CHAIN)?;
        assert_eq!((e.stdout.as_deref(), e.status), (Some("hi\n"), 0));
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
    fn a_dash_override_does_not_outrank_a_file_that_compared_ash() -> Result<(), SpecError> {
        // SAMPLE's `## compare_shells:` lists ash, so Oils RAN ash here and wrote
        // no block for it -- which is evidence that the default block is ash's own
        // answer. The `## OK dash` block records where dash left it, not where we
        // should be, so it must not be inherited.
        let cases = parse_spec(SAMPLE)?;
        let c = cases.get(2).ok_or_else(|| SpecError::new(0, "missing case"))?;
        assert_eq!(resolve(c, ASH_DASH_CHAIN)?.stdout.as_deref(), Some("default-ideal\n"));

        // The same case in a file that never ran ash says nothing about ash, so the
        // chain still falls through to its same-lineage neighbour.
        let without = SAMPLE.replace("mksh ash", "mksh");
        let cases = parse_spec(&without)?;
        let c = cases.get(2).ok_or_else(|| SpecError::new(0, "missing case"))?;
        assert_eq!(resolve(c, ASH_DASH_CHAIN)?.stdout.as_deref(), Some("dash-says\n"));
        // A chain without dash falls back to the unqualified default.
        assert_eq!(resolve(c, &["mksh"])?.stdout.as_deref(), Some("default-ideal\n"));
        Ok(())
    }

    #[test]
    fn an_explicit_ash_block_still_wins_in_a_file_that_compared_ash() -> Result<(), SpecError> {
        // Truncating the chain only removes the FALLTHROUGH. Where ash diverged,
        // Oils wrote the block down, and that block is still what applies.
        let spec = "\
## compare_shells: dash ash

#### ash has its own answer
echo x
## STDOUT:
default-ideal
## END
## BUG ash STDOUT:
ash-says
## END
## OK dash STDOUT:
dash-says
## END
";
        let cases = parse_spec(spec)?;
        let c = cases.first().ok_or_else(|| SpecError::new(0, "missing case"))?;
        assert_eq!(resolve(c, ASH_DASH_CHAIN)?.stdout.as_deref(), Some("ash-says\n"));
        Ok(())
    }

    #[test]
    fn a_file_that_compared_only_dash_still_reaches_its_block() -> Result<(), SpecError> {
        // The truncation is keyed on the file's list, not on position: with ash
        // absent from it, `dash` is the first compared identity and the chain runs
        // to there and stops.
        let spec = "\
## compare_shells: bash dash mksh

#### only dash was compared
echo x
## STDOUT:
default-ideal
## END
## OK dash STDOUT:
dash-says
## END
";
        let cases = parse_spec(spec)?;
        let c = cases.first().ok_or_else(|| SpecError::new(0, "missing case"))?;
        assert_eq!(resolve(c, ASH_DASH_CHAIN)?.stdout.as_deref(), Some("dash-says\n"));
        Ok(())
    }

    #[test]
    fn compare_shells_inside_a_case_is_not_a_header() -> Result<(), SpecError> {
        // Two losses if the `cur.is_none()` guard goes: a misplaced header stops
        // being reported as an unrecognized key, and it silently re-aims every
        // LATER case in the file.
        let spec = "\
#### first
echo x
## compare_shells: ash
## STDOUT:
default-ideal
## END
## OK dash STDOUT:
dash-says
## END

#### second
echo y
## STDOUT:
default-ideal
## END
## OK dash STDOUT:
dash-says
## END
";
        let cases = parse_spec(spec)?;
        let first = cases.first().ok_or_else(|| SpecError::new(0, "missing case"))?;
        assert_eq!(first.unrecognized_keys(), vec!["compare_shells"]);
        let second = cases.get(1).ok_or_else(|| SpecError::new(0, "missing case"))?;
        assert_eq!(resolve(second, ASH_DASH_CHAIN)?.stdout.as_deref(), Some("dash-says\n"));
        // Same for a qualified spelling, which the `## code:` hook also refuses.
        let spec = spec.replace("## compare_shells: ash", "## OK ash compare_shells: ash");
        let cases = parse_spec(&spec)?;
        let first = cases.first().ok_or_else(|| SpecError::new(0, "missing case"))?;
        assert_eq!(first.unrecognized_keys(), vec!["compare_shells"]);
        Ok(())
    }

    #[test]
    fn a_versioned_identity_still_names_its_shell() -> Result<(), SpecError> {
        // `bash-4.4` and `zsh-5.9` are already in this corpus, so the spelling is
        // established; an `ash-1.37` must bound the chain, not silently stop it.
        assert!(names_identity("ash", "ash"));
        assert!(names_identity("ash-1.37", "ash"));
        assert!(!names_identity("ashx", "ash"));
        assert!(!names_identity("bash", "ash"));
        // Case-SENSITIVE deliberately. Being lenient here would quietly accept a
        // spelling the corpus validator is there to reject, so the two would
        // disagree about the same token.
        assert!(!names_identity("ASH", "ash"));
        let spec = "\
## compare_shells: bash-4.4 ash-1.37

#### versioned header still bounds the chain
echo x
## STDOUT:
default-ideal
## END
## OK dash STDOUT:
dash-says
## END
";
        let cases = parse_spec(spec)?;
        let c = cases.first().ok_or_else(|| SpecError::new(0, "missing case"))?;
        assert_eq!(resolve(c, ASH_DASH_CHAIN)?.stdout.as_deref(), Some("default-ideal\n"));
        Ok(())
    }

    #[test]
    fn truncation_can_leave_a_field_unasserted_and_that_is_the_rule() -> Result<(), SpecError> {
        // A field whose ONLY annotation names a later identity, with no
        // unqualified block, resolves to "not asserted" rather than inheriting
        // that shell's. Pinned deliberately: it is the format's own reading (the
        // unqualified block is the golden, and there is none), and it is the one
        // shape where the bound LOSES an assertion rather than correcting one.
        // No corpus case does this today -- 0 of 2798 go Some -> None -- so this
        // test is what keeps a future vendored drop from changing it in silence.
        let spec = "\
## compare_shells: bash mksh ash

#### only dash has a block, and there is no default
echo x
## OK dash STDOUT:
dash-says
## END
";
        let cases = parse_spec(spec)?;
        let c = cases.first().ok_or_else(|| SpecError::new(0, "missing case"))?;
        assert_eq!(resolve(c, ASH_DASH_CHAIN)?.stdout, None);
        Ok(())
    }

    #[test]
    fn a_file_with_no_compare_shells_line_keeps_the_whole_chain() -> Result<(), SpecError> {
        // Three of the corpus's files declare none. Nothing is known about either
        // identity there, so the heuristic stands unchanged.
        let spec = "\
#### no metadata at all
echo x
## STDOUT:
default-ideal
## END
## OK dash STDOUT:
dash-says
## END
";
        let cases = parse_spec(spec)?;
        let c = cases.first().ok_or_else(|| SpecError::new(0, "missing case"))?;
        assert_eq!(resolve(c, ASH_DASH_CHAIN)?.stdout.as_deref(), Some("dash-says\n"));
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

    fn outcome(passed: bool) -> CaseOutcome {
        CaseOutcome {
            name: "c".into(),
            passed,
            detail: if passed { None } else { Some("mismatch".into()) },
            timed_out: false,
        }
    }

    #[test]
    fn expectations_parse_reads_xfail_and_skip_with_spaces() -> Result<(), SpecError> {
        let exp = Expectations::parse(
            "# comment\n\
             xfail arith.test.sh::Add one to var\n\
             skip loop.test.sh::reads from stdin forever\n",
        )?;
        assert!(exp.is_xfail("arith.test.sh::Add one to var"));
        assert!(exp.is_skip("loop.test.sh::reads from stdin forever"));
        assert!(!exp.is_xfail("loop.test.sh::reads from stdin forever"));
        Ok(())
    }

    #[test]
    fn expectations_parse_rejects_malformed_lines() {
        assert!(Expectations::parse("wat arith.test.sh::x").is_err()); // unknown disposition
        assert!(Expectations::parse("xfail no-double-colon").is_err()); // missing `::`
        assert!(Expectations::parse("lonelytoken").is_err()); // no whitespace split
        assert!(Expectations::parse("xfail f::c\nxfail f::c").is_err()); // duplicate
        assert!(Expectations::parse("xfail f::c\nskip f::c").is_err()); // both xfail and skip
    }

    #[test]
    fn classify_maps_every_quadrant() {
        let exp = Expectations::parse("xfail f::gap").unwrap_or_default();
        // pass + unlisted => Pass; fail + unlisted => Fail (regression).
        assert_eq!(classify("f::ok".into(), &outcome(true), &exp).disposition, Disposition::Pass);
        assert_eq!(classify("f::reg".into(), &outcome(false), &exp).disposition, Disposition::Fail);
        // fail + listed => XFail (tolerated); pass + listed => XPass (promote).
        assert_eq!(classify("f::gap".into(), &outcome(false), &exp).disposition, Disposition::XFail);
        assert_eq!(classify("f::gap".into(), &outcome(true), &exp).disposition, Disposition::XPass);
    }

    #[test]
    fn summary_greens_only_without_fail_or_xpass() {
        let clean = [
            ClassifiedOutcome { key: "a".into(), disposition: Disposition::Pass, detail: None },
            ClassifiedOutcome { key: "b".into(), disposition: Disposition::XFail, detail: None },
            ClassifiedOutcome { key: "c".into(), disposition: Disposition::Skip, detail: None },
        ];
        let s = summarize(&clean);
        assert_eq!((s.pass, s.xfail, s.skip), (1, 1, 1));
        assert!(s.is_green());

        let regressed =
            [ClassifiedOutcome { key: "d".into(), disposition: Disposition::Fail, detail: None }];
        assert!(!summarize(&regressed).is_green());
        let stale_pass =
            [ClassifiedOutcome { key: "e".into(), disposition: Disposition::XPass, detail: None }];
        assert!(!summarize(&stale_pass).is_green());
    }

    #[test]
    fn duplicate_conflicts_backstops_key_collision() -> Result<(), SpecError> {
        // `case_keys` occurrence-qualifies duplicates, so `is_new` is true for every
        // case in a well-formed corpus. A `false` means a genuine key collision, which
        // reds the gate unconditionally — listed or not — so one entry/run can never
        // stand in for two cases.
        assert!(!duplicate_conflicts(true)); // first sight of a key never conflicts
        assert!(duplicate_conflicts(false)); // a repeat key is a collision -> red
        Ok(())
    }

    #[test]
    fn case_keys_reds_gate_on_adversarial_suffix_collision() -> Result<(), SpecError> {
        // Adversarial: a doubled "dup" makes `f::dup ##2`, which an upstream case
        // literally named "dup ##2" also produces. case_keys cannot tell them apart,
        // so the gate must red via the collision backstop, not silently mask one.
        let text = "#### dup\ntrue\n#### dup\ntrue\n#### dup ##2\ntrue\n";
        let cases = parse_spec(text)?;
        let keys = case_keys("f.test.sh", &cases);
        // 2nd "dup" and literal "dup ##2" collide on the same key.
        assert_eq!(keys.get(1), keys.get(2));
        let mut seen = BTreeSet::new();
        let collided = keys.iter().any(|k| !seen.insert(k.clone()));
        assert!(collided, "expected a key collision the gate would red on");
        Ok(())
    }

    #[test]
    fn case_keys_qualifies_repeated_descriptions() -> Result<(), SpecError> {
        // Two cases share "dup"; "solo" appears once. The first "dup" keeps the bare
        // key, later occurrences get an ` ##N` suffix, so every case is addressable.
        let text = "#### dup\ntrue\n#### solo\ntrue\n#### dup\ntrue\n#### dup\ntrue\n";
        let cases = parse_spec(text)?;
        let keys = case_keys("f.test.sh", &cases);
        assert_eq!(
            keys,
            vec![
                "f.test.sh::dup".to_string(),
                "f.test.sh::solo".to_string(),
                "f.test.sh::dup ##2".to_string(),
                "f.test.sh::dup ##3".to_string(),
            ]
        );
        Ok(())
    }

    #[test]
    fn parse_rejects_invalid_hash_line_inside_a_case() {
        // A `##` line inside a case that is neither assertion, block, nor `## END`
        // is malformed (Oils raises) — it must not silently truncate a golden.
        assert!(parse_spec("#### x\necho hi\n##BADLINE\n").is_err()); // no space
        assert!(parse_spec("#### x\necho hi\n## STODUT\n").is_err()); // typo, no colon
        // File-level `##` prose before the first case stays ignorable metadata.
        assert!(parse_spec("## just prose here\n#### x\necho hi\n## status: 0\n").is_ok());
    }

    #[test]
    fn parse_rejects_code_mixed_with_body_lines() -> Result<(), SpecError> {
        // `## code:` alone is the real corpus shape.
        let ok = parse_spec("#### x\n## code: echo hi\n## status: 0\n")?;
        assert_eq!(ok.first().map(|c| c.code.as_str()), Some("echo hi"));
        // Body code after `## code:` is the mixed shape Oils rejects.
        assert!(parse_spec("#### x\n## code: echo hi\necho again\n").is_err());
        // Body code before `## code:` too.
        assert!(parse_spec("#### x\necho first\n## code: echo hi\n").is_err());
        // A trailing blank line after `## code:` is tolerated, not treated as code.
        assert!(parse_spec("#### x\n## code: echo hi\n\n## status: 0\n").is_ok());
        Ok(())
    }
}

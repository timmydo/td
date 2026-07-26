//! Word expansion: tilde, parameter, command and arithmetic substitution, then
//! field splitting, pathname expansion and quote removal — in that order.
//!
//! Expansion runs over `QChar`s rather than `String`s. Two bits per character
//! decide everything downstream: `quoted` (never a pathname metacharacter,
//! never a field separator) and `expanded` (came out of a substitution, so IFS
//! may split it). Collapsing to text before splitting is the classic way to get
//! `x='a b'; f $x` and `f "$x"` both wrong.

use crate::ast::{Param, ParamOp, Seg, Word};
use crate::exec::{command_subst, Shell, Sig, R};
use crate::pattern;
use crate::{arith, exec};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QChar {
    pub c: char,
    pub quoted: bool,
    pub expanded: bool,
}

impl QChar {
    /// Unquoted source characters — what a pattern written literally in the
    /// program text looks like after expansion. A test-only convenience for the
    /// pattern matcher; real patterns arrive already expanded.
    #[cfg(test)]
    pub fn literal_str(s: &str) -> Vec<QChar> {
        s.chars()
            .map(|c| QChar {
                c,
                quoted: false,
                expanded: false,
            })
            .collect()
    }

    pub fn text(chars: &[QChar]) -> String {
        chars.iter().map(|q| q.c).collect()
    }
}

/// A field under construction. `had_quotes` records that some quoted segment
/// contributed, so an empty result is still a field: `""` is one empty
/// argument, while an unquoted empty `$x` is none at all.
#[derive(Default)]
struct Field {
    chars: Vec<QChar>,
    had_quotes: bool,
}

/// Full expansion of one word in an argument position: splitting and pathname
/// expansion included.
pub fn expand_fields(sh: &mut Shell, w: &Word) -> R<Vec<String>> {
    let fields = expand_raw(sh, w, false)?;
    let split = split_fields(sh, fields);
    let mut out = Vec::new();
    for field in split {
        out.extend(glob_field(sh, &field));
    }
    Ok(out)
}

pub fn expand_word_list(sh: &mut Shell, words: &[Word]) -> R<Vec<String>> {
    let mut out = Vec::new();
    for w in words {
        out.extend(expand_fields(sh, w)?);
    }
    Ok(out)
}

/// Expansion in a context that takes exactly one word: a redirection target, a
/// `case` subject, an assignment value. No splitting, no pathname expansion.
pub fn expand_single(sh: &mut Shell, w: &Word) -> R<String> {
    Ok(QChar::text(&expand_chars(sh, w, false)?))
}

/// Like `expand_single` but keeping the quoting bits, so the result can be used
/// as a pattern.
pub fn expand_pattern(sh: &mut Shell, w: &Word) -> R<Vec<QChar>> {
    expand_chars(sh, w, false)
}

/// Expand and flatten to a single run of characters, joining the fields that a
/// `$@` may have produced with a space.
fn expand_chars(sh: &mut Shell, w: &Word, force_quoted: bool) -> R<Vec<QChar>> {
    let fields = expand_raw(sh, w, force_quoted)?;
    let mut out: Vec<QChar> = Vec::new();
    for (i, f) in fields.iter().enumerate() {
        if i > 0 {
            out.push(QChar {
                c: ' ',
                quoted: true,
                expanded: false,
            });
        }
        out.extend(f.chars.iter().copied());
    }
    Ok(out)
}

/// Expand a word to its fields-before-splitting. More than one field can come
/// out already: `"$@"` produces one per positional parameter.
fn expand_raw(sh: &mut Shell, w: &Word, force_quoted: bool) -> R<Vec<Field>> {
    let mut done: Vec<Field> = Vec::new();
    let mut cur = Field::default();
    for (i, seg) in w.0.iter().enumerate() {
        match seg {
            Seg::Lit(s) => {
                // A tilde only expands at the very start of a word, and what it
                // expands to is not itself a pattern or split candidate.
                let (home, rest) = if i == 0 {
                    tilde_split(sh, s)
                } else {
                    (String::new(), s.as_str())
                };
                for c in home.chars() {
                    cur.chars.push(QChar {
                        c,
                        quoted: true,
                        expanded: false,
                    });
                }
                for c in rest.chars() {
                    cur.chars.push(QChar {
                        c,
                        quoted: force_quoted,
                        expanded: false,
                    });
                }
            }
            Seg::Quoted(s) => {
                cur.had_quotes = true;
                for c in s.chars() {
                    cur.chars.push(QChar {
                        c,
                        quoted: true,
                        expanded: false,
                    });
                }
            }
            Seg::Cmd { code, quoted } => {
                let q = *quoted || force_quoted;
                if q {
                    cur.had_quotes = true;
                }
                let out = command_subst(sh, code)?;
                push_expanded(&mut cur.chars, &out, q);
            }
            Seg::Arith { expr, quoted } => {
                let q = *quoted || force_quoted;
                if q {
                    cur.had_quotes = true;
                }
                let text = QChar::text(&expand_chars(sh, expr, false)?);
                let value = arith::eval(sh, &text)?;
                push_expanded(&mut cur.chars, &value.to_string(), q);
            }
            Seg::Param(p) => expand_param(sh, p, force_quoted, &mut done, &mut cur)?,
        }
    }
    done.push(cur);
    Ok(done)
}

fn push_expanded(out: &mut Vec<QChar>, text: &str, quoted: bool) {
    for c in text.chars() {
        out.push(QChar {
            c,
            quoted,
            expanded: true,
        });
    }
}

fn expand_param(
    sh: &mut Shell,
    p: &Param,
    force_quoted: bool,
    done: &mut Vec<Field>,
    cur: &mut Field,
) -> R<()> {
    let quoted = p.quoted || force_quoted;

    // `$@` is the one expansion that yields several fields on its own. When
    // there are no positional parameters it contributes nothing at all — and
    // deliberately does not mark the field as quoted, so a lone `"$@"` expands
    // to zero arguments rather than one empty one.
    if p.op.is_none() && p.name == "@" {
        let params = sh.params.clone();
        // Each positional becomes its own field; when quoted, every one is a
        // field even if empty (so `"$@"` over ("", "b") yields two args). But a
        // quoted `$@` over ZERO params must NOT mark the field as quoted, so a
        // lone `"$@"` expands to no words rather than one empty word (POSIX).
        if quoted && !params.is_empty() {
            cur.had_quotes = true;
        }
        for (i, v) in params.iter().enumerate() {
            if i > 0 {
                done.push(std::mem::replace(
                    cur,
                    Field {
                        chars: Vec::new(),
                        had_quotes: quoted,
                    },
                ));
            }
            push_expanded(&mut cur.chars, v, quoted);
        }
        return Ok(());
    }

    if quoted {
        cur.had_quotes = true;
    }

    if matches!(p.op, Some(ParamOp::Length)) {
        let len = match p.name.as_str() {
            "@" | "*" => sh.params.len(),
            _ => lookup(sh, &p.name).unwrap_or_default().chars().count(),
        };
        push_expanded(&mut cur.chars, &len.to_string(), quoted);
        return Ok(());
    }

    let raw = lookup(sh, &p.name);
    let empty = match &raw {
        None => true,
        Some(v) => v.is_empty(),
    };

    let value: Option<String> = match &p.op {
        None => {
            if raw.is_none() && sh.opts.nounset && !is_always_set(&p.name) {
                return Err(sh.fatal(&format!("{}: parameter not set", p.name), 2));
            }
            raw
        }
        Some(ParamOp::Length) => raw,
        Some(ParamOp::Default { word, colon }) => {
            if raw.is_none() || (*colon && empty) {
                let chars = expand_chars(sh, word, quoted)?;
                push_chars(&mut cur.chars, &chars, quoted);
                return Ok(());
            }
            raw
        }
        Some(ParamOp::Alt { word, colon }) => {
            if raw.is_none() || (*colon && empty) {
                Some(String::new())
            } else {
                let chars = expand_chars(sh, word, quoted)?;
                push_chars(&mut cur.chars, &chars, quoted);
                return Ok(());
            }
        }
        Some(ParamOp::Assign { word, colon }) => {
            if raw.is_none() || (*colon && empty) {
                let text = QChar::text(&expand_chars(sh, word, quoted)?);
                if !crate::ast::is_name(&p.name) {
                    return Err(sh.fatal(&format!("cannot assign to ${}", p.name), 2));
                }
                sh.set_var(&p.name, &text)?;
                Some(text)
            } else {
                raw
            }
        }
        Some(ParamOp::Error { word, colon }) => {
            if raw.is_none() || (*colon && empty) {
                let msg = QChar::text(&expand_chars(sh, word, quoted)?);
                let msg = if msg.is_empty() {
                    "parameter not set".to_string()
                } else {
                    msg
                };
                return Err(sh.fatal(&format!("{}: {msg}", p.name), 2));
            }
            raw
        }
        Some(ParamOp::TrimPrefix { pat, longest }) => {
            let subject = raw.unwrap_or_default();
            let units = pattern::compile(&expand_pattern(sh, pat)?);
            Some(pattern::strip_prefix(&units, &subject, *longest).unwrap_or(subject))
        }
        Some(ParamOp::TrimSuffix { pat, longest }) => {
            let subject = raw.unwrap_or_default();
            let units = pattern::compile(&expand_pattern(sh, pat)?);
            Some(pattern::strip_suffix(&units, &subject, *longest).unwrap_or(subject))
        }
    };

    push_expanded(&mut cur.chars, &value.unwrap_or_default(), quoted);
    Ok(())
}

/// Append already-expanded characters, keeping any quoting they carry but
/// marking them substitution results (so an unquoted `${x:-a b}` still splits).
fn push_chars(out: &mut Vec<QChar>, chars: &[QChar], quoted: bool) {
    for q in chars {
        out.push(QChar {
            c: q.c,
            quoted: q.quoted || quoted,
            expanded: true,
        });
    }
}

/// Parameters that are always set, so `set -u` never fires on them.
fn is_always_set(name: &str) -> bool {
    matches!(name, "?" | "#" | "$" | "!" | "-" | "0" | "@" | "*")
}

/// The value of a parameter, or `None` when it is unset.
pub fn lookup(sh: &Shell, name: &str) -> Option<String> {
    match name {
        "?" => Some(sh.status.to_string()),
        "#" => Some(sh.params.len().to_string()),
        "$" => Some(std::process::id().to_string()),
        "!" => Some(sh.last_bg.to_string()),
        "-" => Some(sh.opts.letters(sh.interactive)),
        "0" => Some(sh.arg0.clone()),
        "*" => {
            let sep = sh
                .get_var("IFS")
                .unwrap_or_else(|| " \t\n".to_string())
                .chars()
                .next()
                .map(String::from)
                .unwrap_or_default();
            Some(sh.params.join(&sep))
        }
        "@" => Some(sh.params.join(" ")),
        _ => {
            if let Ok(n) = name.parse::<usize>() {
                if n == 0 {
                    return Some(sh.arg0.clone());
                }
                return sh.params.get(n - 1).cloned();
            }
            sh.get_var(name)
        }
    }
}

/// Split a literal into (substituted home, remainder). `~` and `~/...` only:
/// `~user` needs the passwd database, which `std` does not expose, so it is left
/// as written — which is also what POSIX says to do for an unknown user.
fn tilde_split<'a>(sh: &Shell, s: &'a str) -> (String, &'a str) {
    let Some(rest) = s.strip_prefix('~') else {
        return (String::new(), s);
    };
    if !(rest.is_empty() || rest.starts_with('/')) {
        return (String::new(), s);
    }
    match sh.get_var("HOME") {
        Some(home) => (home, rest),
        None => (String::new(), s),
    }
}

fn tilde_expand(sh: &Shell, s: &str) -> String {
    let (home, rest) = tilde_split(sh, s);
    format!("{home}{rest}")
}

fn is_ifs_ws(c: char) -> bool {
    c == ' ' || c == '\t' || c == '\n'
}

/// IFS field splitting, applied only to characters that came out of an
/// expansion unquoted.
fn split_fields(sh: &Shell, fields: Vec<Field>) -> Vec<Vec<QChar>> {
    let ifs = sh.get_var("IFS").unwrap_or_else(|| " \t\n".to_string());
    let mut out: Vec<Vec<QChar>> = Vec::new();
    for field in fields {
        if ifs.is_empty() {
            if !field.chars.is_empty() || field.had_quotes {
                out.push(field.chars);
            }
            continue;
        }
        let is_delim = |q: &QChar| q.expanded && !q.quoted && ifs.contains(q.c);
        let chars = &field.chars;
        let mut i = 0usize;
        let before = out.len();
        // Leading IFS whitespace never makes an empty first field.
        while chars.get(i).is_some_and(|q| is_delim(q) && is_ifs_ws(q.c)) {
            i += 1;
        }
        while i < chars.len() {
            let mut piece = Vec::new();
            while let Some(q) = chars.get(i) {
                if is_delim(q) {
                    break;
                }
                piece.push(*q);
                i += 1;
            }
            out.push(piece);
            if i >= chars.len() {
                break;
            }
            while chars.get(i).is_some_and(|q| is_delim(q) && is_ifs_ws(q.c)) {
                i += 1;
            }
            // At most one non-whitespace separator delimits a field.
            if chars.get(i).is_some_and(|q| is_delim(q) && !is_ifs_ws(q.c)) {
                i += 1;
                while chars.get(i).is_some_and(|q| is_delim(q) && is_ifs_ws(q.c)) {
                    i += 1;
                }
            }
        }
        // An expansion that produced nothing is not an argument, unless quotes
        // asked for an empty one.
        if out.len() == before && field.had_quotes {
            out.push(Vec::new());
        }
    }
    out
}

/// Pathname expansion. With no match (or `set -f`) the field is used as written,
/// which is what POSIX requires and what scripts rely on.
fn glob_field(sh: &Shell, field: &[QChar]) -> Vec<String> {
    if sh.opts.noglob || !pattern::has_meta(field) {
        return vec![QChar::text(field)];
    }
    let mut matched = glob_walk(sh, field);
    if matched.is_empty() {
        return vec![QChar::text(field)];
    }
    matched.sort();
    matched
}

fn glob_walk(sh: &Shell, field: &[QChar]) -> Vec<String> {
    let absolute = field.first().is_some_and(|q| q.c == '/');
    let trailing_slash = field.last().is_some_and(|q| q.c == '/');
    let mut comps: Vec<Vec<QChar>> = Vec::new();
    let mut cur: Vec<QChar> = Vec::new();
    for q in field {
        if q.c == '/' {
            comps.push(std::mem::take(&mut cur));
        } else {
            cur.push(*q);
        }
    }
    comps.push(cur);

    let mut results: Vec<String> = vec![if absolute { "/".into() } else { String::new() }];
    let mut globbed = false;
    for comp in &comps {
        if comp.is_empty() {
            continue;
        }
        if !pattern::has_meta(comp) {
            let lit = QChar::text(comp);
            results = results.iter().map(|base| join_path(base, &lit)).collect();
            continue;
        }
        globbed = true;
        let units = pattern::compile(comp);
        // A leading `.` is only matched by an explicit literal `.`.
        let literal_dot = matches!(comp.first(), Some(q) if q.c == '.');
        let mut next = Vec::new();
        for base in &results {
            let dir = if base.is_empty() {
                sh.cwd.clone()
            } else {
                sh.resolve(base)
            };
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with('.') && !literal_dot {
                    continue;
                }
                if pattern::matches(&units, &name) {
                    next.push(join_path(base, &name));
                }
            }
        }
        results = next;
    }
    if !globbed {
        return Vec::new();
    }
    results
        .into_iter()
        .filter_map(|p| {
            let full = sh.resolve(&p);
            if trailing_slash {
                if full.is_dir() {
                    Some(format!("{p}/"))
                } else {
                    None
                }
            } else if full.symlink_metadata().is_ok() {
                Some(p)
            } else {
                None
            }
        })
        .collect()
}

fn join_path(base: &str, name: &str) -> String {
    if base.is_empty() {
        name.to_string()
    } else if base.ends_with('/') {
        format!("{base}{name}")
    } else {
        format!("{base}/{name}")
    }
}

/// Assignment values take tilde expansion after `=` and after every unquoted
/// `:` — the `PATH=~/bin:~/sbin` idiom.
pub fn expand_assign(sh: &mut Shell, w: &Word) -> R<String> {
    let mut segs: Vec<Seg> = Vec::new();
    let mut at_start = true;
    for seg in &w.0 {
        match seg {
            Seg::Lit(s) if at_start || s.contains(':') => {
                segs.push(Seg::Lit(tilde_after_colons(sh, s, at_start)));
            }
            other => segs.push(other.clone()),
        }
        at_start = false;
    }
    expand_single(sh, &Word(segs))
}

fn tilde_after_colons(sh: &Shell, s: &str, at_start: bool) -> String {
    let mut out = String::new();
    let mut first = true;
    for part in s.split(':') {
        if !first {
            out.push(':');
        }
        if first && !at_start {
            out.push_str(part);
        } else {
            out.push_str(&tilde_expand(sh, part));
        }
        first = false;
    }
    out
}

impl Shell {
    /// Report `msg` on stderr and unwind with `status` — used for the expansion
    /// errors POSIX makes fatal in a non-interactive shell (`set -u`, `${x:?}`).
    pub fn fatal(&self, msg: &str, status: i32) -> Sig {
        let _ = exec::write_stderr(self, msg);
        Sig::Exit(status)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh_with(pairs: &[(&str, &str)]) -> Shell {
        let mut sh = Shell::new_for_test();
        for (k, v) in pairs {
            let _ = sh.set_var(k, v);
        }
        sh
    }

    fn fields(sh: &mut Shell, src: &str) -> Vec<String> {
        let words = crate::lexer::tokenize(src)
            .ok()
            .map(|l| {
                l.toks
                    .into_iter()
                    .filter_map(|t| match t {
                        crate::lexer::Tok::Word(w) => Some(w),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let mut out = Vec::new();
        for w in &words {
            out.extend(expand_fields(sh, w).unwrap_or_default());
        }
        out
    }

    #[test]
    fn unquoted_expansion_splits_quoted_does_not() {
        let mut sh = sh_with(&[("x", "a b c")]);
        assert_eq!(fields(&mut sh, "$x"), vec!["a", "b", "c"]);
        assert_eq!(fields(&mut sh, "\"$x\""), vec!["a b c"]);
    }

    #[test]
    fn literal_text_is_never_split() {
        let mut sh = sh_with(&[("IFS", ":")]);
        // The literal colon is not a separator; the expanded one is.
        assert_eq!(fields(&mut sh, "a:b"), vec!["a:b"]);
        let mut sh = sh_with(&[("IFS", ":"), ("x", "a:b")]);
        assert_eq!(fields(&mut sh, "$x"), vec!["a", "b"]);
    }

    #[test]
    fn empty_expansion_yields_no_field_but_empty_quotes_do() {
        let mut sh = sh_with(&[("x", "")]);
        assert!(fields(&mut sh, "$x").is_empty());
        assert_eq!(fields(&mut sh, "\"$x\""), vec![""]);
        assert_eq!(fields(&mut sh, "''"), vec![""]);
    }

    #[test]
    fn parameter_defaults_and_trims() {
        let mut sh = sh_with(&[("f", "archive.tar.gz"), ("e", "")]);
        assert_eq!(fields(&mut sh, "${nope:-anon}"), vec!["anon"]);
        assert_eq!(fields(&mut sh, "${f:-anon}"), vec!["archive.tar.gz"]);
        assert_eq!(fields(&mut sh, "${e:-anon}"), vec!["anon"]);
        assert_eq!(fields(&mut sh, "${e-anon}"), vec![""; 0]);
        assert_eq!(fields(&mut sh, "${#f}"), vec!["14"]);
        assert_eq!(fields(&mut sh, "${f%.gz}"), vec!["archive.tar"]);
        assert_eq!(fields(&mut sh, "${f%%.*}"), vec!["archive"]);
        assert_eq!(fields(&mut sh, "${f#archive.}"), vec!["tar.gz"]);
    }

    #[test]
    fn default_word_is_split_but_its_quotes_survive() {
        let mut sh = sh_with(&[]);
        assert_eq!(fields(&mut sh, "${nope:-a b}"), vec!["a", "b"]);
        assert_eq!(fields(&mut sh, "${nope:-\"a b\"}"), vec!["a b"]);
    }

    #[test]
    fn positional_parameters() {
        let mut sh = sh_with(&[]);
        sh.params = vec!["one".into(), "two three".into()];
        assert_eq!(fields(&mut sh, "$#"), vec!["2"]);
        assert_eq!(fields(&mut sh, "\"$@\""), vec!["one", "two three"]);
        assert_eq!(fields(&mut sh, "$@"), vec!["one", "two", "three"]);
        assert_eq!(fields(&mut sh, "\"$*\""), vec!["one two three"]);
        sh.params.clear();
        assert!(fields(&mut sh, "\"$@\"").is_empty());
        assert_eq!(fields(&mut sh, "\"$*\""), vec![""]);
    }

    #[test]
    fn quoted_glob_characters_do_not_expand() {
        let mut sh = sh_with(&[]);
        // Nothing matches `*.nonexistent-td-sh`, so the field stays literal.
        assert_eq!(
            fields(&mut sh, "*.nonexistent-td-sh"),
            vec!["*.nonexistent-td-sh"]
        );
        assert_eq!(fields(&mut sh, "'*'"), vec!["*"]);
    }
}

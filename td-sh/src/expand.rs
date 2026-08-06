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

/// Split a word at the `name=` its RAW text begins with. ash decides this on the
/// unexpanded text (`isassignment`, ash.c:6180), so a quote anywhere in the
/// prefix disqualifies it -- `export "n"=$x` and `export n"="$x` are ordinary
/// words that field-split, and only a bare `Lit` can carry the name.
fn assignment_split(w: &Word) -> Option<(String, Word)> {
    let Some(Seg::Lit(first)) = w.0.first() else {
        return None;
    };
    let eq = first.bytes().position(|b| b == b'=')?;
    let name = first.get(..eq)?;
    if !crate::ast::is_name(name) {
        return None;
    }
    let mut rest = Vec::new();
    match first.get(eq + 1..) {
        Some(tail) if !tail.is_empty() => rest.push(Seg::Lit(tail.to_string())),
        _ => {}
    }
    rest.extend(w.0.iter().skip(1).cloned());
    Some((format!("{name}="), Word(rest)))
}

/// The builtins whose assignment-form operands are expanded as assignments:
/// ash's `BUILTIN_ASSIGN` table entries (ash.c:10160), the only ones for which
/// `pseudovarflag` is set.
fn is_assignment_builtin(word: &str) -> bool {
    matches!(word, "export" | "readonly" | "local" | "alias")
}

/// Expand words until at least one field exists, as ash's `fill_arglist`
/// (ash.c:8839) does. A word can expand to nothing, so "the next field" is not
/// "the next word", and the command word cannot be resolved until one appears.
fn fill_fields(
    sh: &mut Shell,
    argv: &mut Vec<String>,
    words: &mut std::slice::Iter<'_, Word>,
) -> R<bool> {
    let start = argv.len();
    for w in words.by_ref() {
        argv.extend(expand_fields(sh, w)?);
        if argv.len() > start {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Walk `command`'s options to the word it wraps, as ash's `parse_command_args`
/// (ash.c:8857) does: `-p` clusters are consumed, `--` ends them, and a bare `-`
/// is the operand. Any other option letter -- or running out of words -- means
/// the `command` builtin itself runs and nothing is wrapped, so this answers
/// `None` and the walk stops.
fn command_operand(
    sh: &mut Shell,
    argv: &mut Vec<String>,
    words: &mut std::slice::Iter<'_, Word>,
    head: usize,
) -> R<Option<usize>> {
    let mut i = head;
    loop {
        if argv.get(i + 1).is_none() && !fill_fields(sh, argv, words)? {
            return Ok(None);
        }
        i += 1;
        let Some(field) = argv.get(i) else {
            return Ok(None);
        };
        match field.as_bytes().split_first() {
            Some((b'-', opts)) if !opts.is_empty() => {
                if opts == b"-" {
                    if argv.get(i + 1).is_none() && !fill_fields(sh, argv, words)? {
                        return Ok(None);
                    }
                    return Ok(Some(i + 1));
                }
                if opts.iter().any(|&c| c != b'p') {
                    return Ok(None);
                }
            }
            _ => return Ok(Some(i)),
        }
    }
}

/// Expand a simple command's words. Identical to `expand_word_list` except for
/// the declaration builtins, whose `name=value` operands skip field splitting and
/// pathname expansion and take tilde expansion after the `=` -- the same handling
/// a real assignment gets, which is what makes `export n=$(cmd)` keep a value with
/// a space in it and `export PATH=~/bin` mean what it says. ash cannot decide this
/// until the command word is expanded and looked up, and neither can this.
pub fn expand_command_words(sh: &mut Shell, words: &[Word]) -> R<Vec<String>> {
    let mut argv: Vec<String> = Vec::new();
    let mut words = words.iter();
    let mut protect = false;
    if fill_fields(sh, &mut argv, &mut words)? {
        let mut head = 0;
        let mut nofunc = false;
        // ash's resolution loop (ash.c:10402): follow `command` through its options
        // to whatever it wraps, however many times it is repeated. Once one has been
        // followed the lookup drops functions (`DO_NOFUNC`), so `command export`
        // reaches the builtin past a function of that name -- while a function named
        // `command` is not the builtin and stops the walk before it starts.
        while let Some(name) = argv.get(head) {
            if !nofunc && sh.funcs.contains_key(name.as_str()) {
                break;
            }
            if name != "command" {
                protect = is_assignment_builtin(name);
                break;
            }
            match command_operand(sh, &mut argv, &mut words, head)? {
                Some(next) => {
                    head = next;
                    nofunc = true;
                }
                // `command` is a regular builtin, not an assignment one, so running
                // it rather than what it wraps protects nothing.
                None => break,
            }
        }
    }
    for w in words {
        if protect {
            if let Some((name, value)) = assignment_split(w) {
                let value = expand_assign(sh, &value)?;
                argv.push(format!("{name}{value}"));
                continue;
            }
        }
        argv.extend(expand_fields(sh, w)?);
    }
    Ok(argv)
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

/// A slice operand: expanded, then evaluated as arithmetic. An EMPTY one is 0,
/// which is what makes `${v: :2}` and `${v::2}` the first two characters --
/// `arith::eval` would call the empty expression an error.
fn slice_arith(sh: &mut Shell, w: &Word) -> R<i64> {
    let text = QChar::text(&expand_chars(sh, w, false)?);
    if text.trim().is_empty() {
        return Ok(0);
    }
    arith::eval(sh, &text)
}

/// Resolve `offset`/`length` against a sequence of `total` elements, returning
/// the half-open range to take. A NEGATIVE offset counts back from the end; a
/// negative LENGTH is not a count but an end index measured from the end, so
/// `${v:1:-1}` drops the last character. A length that puts the end before the
/// start is fatal, as it is in bash -- the alternative is silently returning
/// nothing for what is almost always an arithmetic mistake.
fn slice_range(
    sh: &mut Shell,
    total: usize,
    offset: i64,
    length: Option<i64>,
    end_relative: bool,
) -> R<(usize, usize)> {
    let total_i = i64::try_from(total).unwrap_or(i64::MAX);
    let start = if offset < 0 {
        let s = total_i.saturating_add(offset);
        // Further back than the whole string reaches: bash yields NOTHING
        // rather than clamping to the front, and raises no error doing it --
        // not even with a length, which the in-range case below would refuse.
        if s < 0 {
            return Ok((0, 0));
        }
        s
    } else if offset > total_i {
        // Past the end is empty, and short-circuits BEFORE the length is
        // looked at: `${v:7:-9}` on six characters is empty where `${v:6:-9}`
        // is the error below. Clamping instead of returning would turn the
        // first into the second.
        return Ok((0, 0));
    } else {
        offset
    };
    let end = match length {
        None => total_i,
        // For a STRING a negative length is an end index measured from the
        // end. For the POSITIONALS bash has no such form and refuses it
        // outright -- but only once the offset is in range, since both
        // out-of-range offsets above answer empty without looking at all.
        Some(l) if l < 0 && end_relative => total_i.saturating_add(l),
        Some(l) if l < 0 => return Err(sh.fatal(&format!("{l}: substring expression < 0"), 1)),
        Some(l) => start.saturating_add(l).min(total_i),
    };
    if end < start {
        let n = length.unwrap_or(0);
        return Err(sh.fatal(&format!("{n}: substring expression < 0"), 1));
    }
    let clamp = |v: i64| usize::try_from(v.clamp(0, total_i)).unwrap_or(0);
    Ok((clamp(start), clamp(end)))
}

fn slice_chars(sh: &mut Shell, chars: &[char], offset: i64, length: Option<i64>) -> R<String> {
    let (a, b) = slice_range(sh, chars.len(), offset, length, true)?;
    Ok(chars.get(a..b).unwrap_or_default().iter().collect())
}

/// `$@`/`$*` slice. The list carries `$0` at its head, so `${@:0}` reaches it --
/// and a NEGATIVE offset counts back over that same list rather than over the
/// positionals alone, which is why `${@: -4}` with three of them yields `$0`
/// and all three. One step further back is empty, as it is for a string.
fn slice_items(sh: &mut Shell, items: &[String], offset: i64, length: Option<i64>) -> R<Vec<String>> {
    let (a, b) = slice_range(sh, items.len(), offset, length, false)?;
    Ok(items.get(a..b).unwrap_or_default().to_vec())
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

    // `${v:off:len}`. Both operands are arithmetic, and for `$@`/`$*` the slice
    // is over the POSITIONALS rather than over their join -- so it can yield
    // several fields and is handled before the scalar path.
    if let Some(ParamOp::Substring { offset, length }) = &p.op {
        let off = slice_arith(sh, offset)?;
        let len = match length {
            Some(l) => Some(slice_arith(sh, l)?),
            None => None,
        };
        if p.name == "@" || p.name == "*" {
            // `${@:0}` is `$0` followed by the positionals, which is the one
            // place `$0` is reachable by index.
            let mut items = vec![sh.arg0.clone()];
            items.extend(sh.params.iter().cloned());
            let picked = slice_items(sh, &items, off, len)?;
            if p.name == "*" {
                // A quoted `$*` is ONE field however few elements it joins, so
                // this marks the field unconditionally where `$@` below does
                // not: `set -- "${*:9}"` passes one empty argument and
                // `set -- "${@:9}"` passes none.
                if quoted {
                    cur.had_quotes = true;
                }
                // Joined on IFS's first character, as a plain `$*` is.
                let sep = sh
                    .get_var("IFS")
                    .unwrap_or_else(|| " \t\n".to_string())
                    .chars()
                    .next()
                    .map(String::from)
                    .unwrap_or_default();
                push_expanded(&mut cur.chars, &picked.join(&sep), quoted);
                return Ok(());
            }
            // As for a plain `"$@"`: an EMPTY slice must not mark the field
            // quoted, or `set -- "${@:9}"` would pass one empty argument where
            // it should pass none.
            if quoted && !picked.is_empty() {
                cur.had_quotes = true;
            }
            for (i, v) in picked.iter().enumerate() {
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
        // A SCALAR slice is an ordinary quoted expansion, so an empty result is
        // still one empty field -- unlike the positional case above, where an
        // empty slice yields none. `"${v:2:0}"` is one argument and `"${@:9}"`
        // is zero, both measured.
        if quoted {
            cur.had_quotes = true;
        }
        nounset_check(sh, &p.name)?;
        let value = lookup(sh, &p.name).unwrap_or_default();
        let chars: Vec<char> = value.chars().collect();
        let picked = slice_chars(sh, &chars, off, len)?;
        push_expanded(&mut cur.chars, &picked, quoted);
        return Ok(());
    }

    if quoted {
        cur.had_quotes = true;
    }

    if matches!(p.op, Some(ParamOp::Length)) {
        let len = match p.name.as_str() {
            // Not the COUNT: ash and dash both give the character length of the
            // `$*` join, so `set -- a b c` makes `${#@}` 5 rather than 3, and
            // `IFS=:` makes it 3. `$#` is the count.
            "@" | "*" => lookup(sh, "*").unwrap_or_default().chars().count(),
            _ => {
                nounset_check(sh, &p.name)?;
                lookup(sh, &p.name).unwrap_or_default().chars().count()
            }
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
            nounset_check(sh, &p.name)?;
            raw
        }
        // Both are answered above, before this match runs.
        Some(ParamOp::Length | ParamOp::Substring { .. }) => raw,
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
            nounset_check(sh, &p.name)?;
            let subject = raw.unwrap_or_default();
            let units = pattern::compile(&expand_pattern(sh, pat)?);
            Some(pattern::strip_prefix(&units, &subject, *longest).unwrap_or(subject))
        }
        Some(ParamOp::TrimSuffix { pat, longest }) => {
            nounset_check(sh, &p.name)?;
            let subject = raw.unwrap_or_default();
            let units = pattern::compile(&expand_pattern(sh, pat)?);
            Some(pattern::strip_suffix(&units, &subject, *longest).unwrap_or(subject))
        }
        Some(ParamOp::Replace { pat, repl, all }) => {
            nounset_check(sh, &p.name)?;
            let subject = raw.unwrap_or_default();
            let units = pattern::compile(&expand_pattern(sh, pat)?);
            // The replacement is expanded even when the pattern turns out empty:
            // ash expands both before deciding, so `${v/$unset/${x:=y}}` still
            // assigns and a command substitution in it still runs.
            let rep = QChar::text(&expand_chars(sh, repl, quoted)?);
            if units.is_empty() {
                // No pattern: ash returns the value untouched rather than
                // matching the empty string everywhere.
                Some(subject)
            } else {
                Some(pattern::replace(&units, &subject, &rep, *all))
            }
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

/// `set -u` for the operators that READ the value: a bare `${v}`, `${#v}`, both
/// trims and patsub. The `-`, `+` and `=` forms SUPPLY a value for an unset
/// name, which is how a script asks about one without tripping the option; `?`
/// raises its own error for unset and so needs no help from here. All four
/// therefore skip this check rather than suppressing a failure.
fn nounset_check(sh: &mut Shell, name: &str) -> R<()> {
    // A DYNAMIC name is never unset -- ash's `lookupvar` runs the func BEFORE the
    // VUNSET test, so the value exists by the time anything asks. Reaching for
    // `lookup` here would draw, and the expansion's own read would then be the
    // SECOND draw: `set -u` alone would skip a number.
    let dynamic = sh.vars.get(name).is_some_and(|v| v.dynamic);
    if sh.opts.nounset && !is_always_set(name) && !dynamic && lookup(sh, name).is_none() {
        return Err(sh.fatal(&format!("{name}: parameter not set"), 2));
    }
    Ok(())
}

/// Parameters that are always set, so `set -u` never fires on them.
fn is_always_set(name: &str) -> bool {
    // `!` is deliberately ABSENT: it is unset until a background job runs.
    matches!(name, "?" | "#" | "$" | "-" | "0" | "@" | "*")
}

/// One draw from ash's `$RANDOM`, which also REWRITES the stored text (ash's
/// `VNOFUNC` write-back) -- so the value last handed out is what an exported
/// RANDOM carries into a child. That write is why the lookup path takes `&mut`.
fn random_value(sh: &mut Shell) -> String {
    let mut gen = sh.random.unwrap_or_else(|| {
        // ash's uninitialised state is `INIT_RANDOM_T(rnd, getpid(),
        // monotonic_us())` -- two DISTINCT inputs landing in different state
        // words, not one value used twice. The clock stands in for the monotonic
        // source; an unseeded sequence only has to differ per shell.
        let clock = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos());
        crate::random::Rand::init(std::process::id().max(1), clock)
    });
    let value = gen.next().to_string();
    sh.random = Some(gen);
    // Written straight into the map: `set_var` would fire the seeding hook and
    // reseed the generator from the value it just produced, which is what ash's
    // `VNOFUNC` avoids. The entry is always there -- only a DYNAMIC one reaches
    // here, and the flag lives on the entry.
    let allexport = sh.opts.allexport;
    if let Some(v) = sh.vars.get_mut("RANDOM") {
        v.value = Some(value.clone());
        // The write-back is a `setvareq`, which ORs in VEXPORT under `set -a`
        // like any other assignment -- so a first read under `-a` exports.
        v.exported |= allexport;
    }
    value
}

/// A plain variable's value, honouring the DYNAMIC names. Only `$RANDOM` is one,
/// and both the expansion path and the arithmetic evaluator go through here
/// because `$((RANDOM+RANDOM))` must draw TWICE -- reading the stored text
/// instead gives the seed added to itself.
pub fn var_value(sh: &mut Shell, name: &str) -> Option<String> {
    if sh.vars.get(name).is_some_and(|v| v.dynamic) {
        return Some(random_value(sh));
    }
    sh.get_var(name)
}

/// The value of a parameter, or `None` when it is unset.
pub fn lookup(sh: &mut Shell, name: &str) -> Option<String> {
    match name {
        "?" => Some(sh.status.to_string()),
        "#" => Some(sh.params.len().to_string()),
        "$" => Some(std::process::id().to_string()),
        "!" => sh.last_bg.map(|p| p.to_string()),
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
            var_value(sh, name)
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

/// Split a flattened pattern into path components. A `/` separates whether or
/// not a backslash precedes it -- ash's `expmeta` scan reaches the slash through
/// the escape and still treats it as the component boundary -- and that
/// backslash is dropped, so `dir\/*` walks `dir/` rather than looking for a
/// directory whose name ends in a backslash.
fn glob_components(chars: &[char]) -> Vec<Vec<char>> {
    let mut comps = Vec::new();
    let mut cur = Vec::new();
    let mut i = 0usize;
    while let Some(&c) = chars.get(i) {
        match (c, chars.get(i + 1)) {
            ('\\', Some('/')) => {
                comps.push(std::mem::take(&mut cur));
                i += 2;
            }
            ('\\', Some(&n)) => {
                cur.push('\\');
                cur.push(n);
                i += 2;
            }
            ('/', _) => {
                comps.push(std::mem::take(&mut cur));
                i += 1;
            }
            _ => {
                cur.push(c);
                i += 1;
            }
        }
    }
    comps.push(cur);
    comps
}

fn glob_walk(sh: &Shell, field: &[QChar]) -> Vec<String> {
    let comps = glob_components(&pattern::preglob(field));
    // A trailing separator selects directories only. Belt-and-braces on Linux,
    // where the trailing slash left ON the path below makes the kernel refuse to
    // stat anything else -- this states the rule locally instead of resting on
    // that, and on the slash surviving `resolve`.
    let trailing_slash = comps.len() > 1 && comps.last().is_some_and(Vec::is_empty);

    // The result is built out of the ORIGINAL text, separators included, as
    // ash's `expmeta` builds `expdir`. Repeated slashes are therefore copied
    // rather than normalised -- `dir//*` lists `dir//f1` -- and an EMPTY
    // component is exactly what carries the extra one. A leading empty
    // component is what roots the walk, so absoluteness needs no flag.
    let mut results: Vec<String> = vec![String::new()];
    let mut globbed = false;
    for (i, comp) in comps.iter().enumerate() {
        if i > 0 {
            for r in &mut results {
                r.push('/');
            }
        }
        if !pattern::has_meta_chars(comp) {
            // The literal components are PATHS, not patterns, so their escapes
            // are spent here rather than handed to the matcher.
            let lit = pattern::unescape(comp);
            for r in &mut results {
                r.push_str(&lit);
            }
            continue;
        }
        globbed = true;
        let units = pattern::compile_chars(comp);
        // A leading `.` is only matched by an explicit literal `.`, and ash
        // steps over ONE leading backslash before asking (ash.c:7957), so `\.f*`
        // reaches dotfiles exactly as `.f*` does.
        let dot_at = usize::from(comp.first() == Some(&'\\'));
        let literal_dot = comp.get(dot_at) == Some(&'.');
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
                    next.push(format!("{base}{name}"));
                }
            }
        }
        results = next;
    }
    if !globbed {
        return Vec::new();
    }
    // The trailing separator is already on the path, the empty final component
    // having carried it, so this only decides WHICH results survive.
    results
        .into_iter()
        .filter(|p| {
            let full = sh.resolve(p);
            if trailing_slash {
                full.is_dir()
            } else {
                full.symlink_metadata().is_ok()
            }
        })
        .collect()
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
        Sig::Abort(status)
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

    /// The single word `src` lexes to, for the cases that assert an ERROR --
    /// `fields` swallows one to keep the passing assertions readable.
    fn word(src: &str) -> Word {
        crate::lexer::tokenize(src)
            .ok()
            .and_then(|l| {
                l.toks.into_iter().find_map(|t| match t {
                    crate::lexer::Tok::Word(w) => Some(w),
                    _ => None,
                })
            })
            .unwrap_or_else(|| Word(Vec::new()))
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
    fn patsub_is_ashs_with_no_bash_anchors() {
        let mut sh = sh_with(&[("s", "xx_xx_xx"), ("v", "a/b/c"), ("e", "")]);
        assert_eq!(fields(&mut sh, "${s/xx?/yy_}"), vec!["yy_xx_xx"]);
        assert_eq!(fields(&mut sh, "${s//xx?/yy_}"), vec!["yy_yy_xx"]);
        // ash has no `#`/`%` anchors, so those are ordinary pattern characters
        // and a pattern starting with one simply does not match.
        assert_eq!(fields(&mut sh, "${s/#?xx/_yy}"), vec!["xx_xx_xx"]);
        assert_eq!(fields(&mut sh, "${s/%?xx/_yy}"), vec!["xx_xx_xx"]);
        // A leading slash is pattern data, so these replace `/` and `/b`.
        assert_eq!(fields(&mut sh, "${v////-}"), vec!["a-b-c"]);
        assert_eq!(fields(&mut sh, "${v///b/-}"), vec!["a-/c"]);
        // No replacement word deletes; an empty PATTERN returns the value whole.
        assert_eq!(fields(&mut sh, "${v//\\/}"), vec!["abc"]);
        assert_eq!(fields(&mut sh, "${v/}"), vec!["a/b/c"]);
        assert_eq!(fields(&mut sh, "${v//}"), vec!["a/b/c"]);
        // An unset name is the empty subject, not an error.
        assert_eq!(fields(&mut sh, "\"${nope/a/X}\""), vec![""]);
        assert_eq!(fields(&mut sh, "\"${e//a/X}\""), vec![""]);
        // An empty pattern against an EMPTY subject is the one place the two
        // empty-cases meet: the pattern wins, so nothing is replaced. Dropping
        // the empty-pattern guard shows up only here, because everywhere else an
        // empty pattern matches only the empty string and so never applies.
        assert_eq!(fields(&mut sh, "\"${e/${nope}/X}\""), vec![""]);
        assert_eq!(fields(&mut sh, "\"${e//${nope}/X}\""), vec![""]);
        // The REPLACEMENT is expanded even when the pattern came out empty, so an
        // assignment inside it still takes: ash expands both before deciding.
        assert_eq!(fields(&mut sh, "\"${e/${nope}/${w:=SIDE}}\""), vec![""]);
        assert_eq!(sh.get_var("w").as_deref(), Some("SIDE"));
    }

    /// Expand one source word, KEEPING the error -- `fields` swallows it, and
    /// whether an expansion fails is exactly what these assertions are about.
    fn try_expand(sh: &mut Shell, src: &str) -> R<Vec<String>> {
        let toks = crate::lexer::tokenize(src).map_err(|e| sh.fatal(&e, 2))?;
        let mut out = Vec::new();
        for t in toks.toks {
            if let crate::lexer::Tok::Word(w) = t {
                out.extend(expand_fields(sh, &w)?);
            }
        }
        Ok(out)
    }

    #[test]
    fn nounset_reaches_every_operator_that_reads_the_value() {
        let mut sh = sh_with(&[("set", "x")]);
        sh.opts.nounset = true;
        // Reading an unset name is an error however the value is consumed --
        // bare, its length, either trim, or patsub. Only `${#v}` and the three
        // pattern operators were missing the check.
        for src in ["${v}", "${#v}", "${v#x}", "${v%x}", "${v/x/y}", "${v//x/y}"] {
            assert!(
                try_expand(&mut sh, src).is_err(),
                "{src} should be a nounset error"
            );
        }
        // The forms that SUPPLY a value for an unset name suppress it, which is
        // how a script asks about a name without tripping `set -u`.
        for src in ["${v-d}", "${v:-d}", "${v+a}", "${v:+a}", "${v=d}"] {
            assert!(
                try_expand(&mut sh, src).is_ok(),
                "{src} should not trip nounset"
            );
        }
        // Set-but-empty is SET: only an unset name errors.
        let mut sh = sh_with(&[("e", "")]);
        sh.opts.nounset = true;
        for src in ["${e}", "${#e}", "${e#x}", "${e/x/y}"] {
            assert!(try_expand(&mut sh, src).is_ok(), "{src}");
        }
        // `$@`/`$*` are always set, even with no positionals.
        for src in ["${@}", "${*}", "${#@}", "${#*}"] {
            assert!(try_expand(&mut sh, src).is_ok(), "{src}");
        }
        // And their LENGTH is the length of the `$*` join, not the count -- so
        // asserting only that they do not error would bless a wrong value.
        let mut sh = sh_with(&[]);
        sh.params = vec!["a".into(), "b".into(), "c".into()];
        assert_eq!(fields(&mut sh, "${#@}"), vec!["5"]);
        assert_eq!(fields(&mut sh, "${#*}"), vec!["5"]);
        assert_eq!(fields(&mut sh, "$#"), vec!["3"]);
        let _ = sh.set_var("IFS", ":");
        assert_eq!(fields(&mut sh, "${#@}"), vec!["5"]);
        // `$!` is the one special parameter that is genuinely UNSET until a
        // background job runs, so it errors like any other unset name -- and
        // expands to nothing without `-u`, where a `0` would name process zero.
        let mut sh = Shell::new_for_test();
        assert_eq!(lookup(&mut sh, "!"), None);
        sh.opts.nounset = true;
        for src in ["${!}", "${#!}", "${!#x}", "${!/x/y}"] {
            assert!(try_expand(&mut sh, src).is_err(), "{src} should trip nounset");
        }
        assert!(try_expand(&mut sh, "${!-none}").is_ok());
        // The check fires BEFORE the pattern and replacement are expanded, so a
        // side effect in either never happens. The previous landing made patsub
        // expand its replacement eagerly, which is exactly what would hoist that
        // work above this check; nothing else in the tree would notice.
        let mut sh = sh_with(&[]);
        sh.opts.nounset = true;
        assert!(try_expand(&mut sh, "${v/${w:=D}/z}").is_err());
        assert_eq!(lookup(&mut sh, "w"), None, "the replacement must not have run");
        assert!(try_expand(&mut sh, "${v#${w:=D}}").is_err());
        assert_eq!(lookup(&mut sh, "w"), None, "the pattern must not have run");
        // With the subject SET both run, which is what makes the above a
        // statement about ordering rather than about patsub being lazy.
        let _ = sh.set_var("v", "A");
        assert!(try_expand(&mut sh, "${v/${w:=D}/z}").is_ok());
        assert_eq!(lookup(&mut sh, "w").as_deref(), Some("D"));
        sh.last_bg = Some(4321);
        for src in ["${!}", "${#!}"] {
            assert!(try_expand(&mut sh, src).is_ok(), "{src} after a background job");
        }
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

    /// An escaped slash still SEPARATES -- ash's `expmeta` scan reaches it
    /// through the escape -- and the backslash it carried is dropped.
    #[test]
    fn an_escaped_slash_is_still_a_component_separator() {
        let split = |s: &str| {
            super::glob_components(&s.chars().collect::<Vec<_>>())
                .iter()
                .map(|c| c.iter().collect::<String>())
                .collect::<Vec<_>>()
        };
        assert_eq!(split("dir/*"), vec!["dir", "*"]);
        assert_eq!(split("dir\\/*"), vec!["dir", "*"]);
        // The escape is kept on anything that is NOT a slash, because the
        // matcher still has to read it as an escape.
        assert_eq!(split("d\\ir/*"), vec!["d\\ir", "*"]);
        // A doubled backslash is a literal backslash, so the slash after it is
        // an ordinary separator and the pair survives into the component.
        assert_eq!(split("dir\\\\/*"), vec!["dir\\\\", "*"]);
        assert_eq!(split("/a/b"), vec!["", "a", "b"]);
        assert_eq!(split("a/"), vec!["a", ""]);
        // A trailing backslash has nothing to pair with and is kept whole.
        assert_eq!(split("a\\"), vec!["a\\"]);
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

    /// Every expectation below was measured against bash 5.2.
    #[test]
    fn a_substring_counts_from_either_end() {
        let mut sh = sh_with(&[("v", "abcdef")]);
        assert_eq!(fields(&mut sh, "${v:0}"), vec!["abcdef"]);
        assert_eq!(fields(&mut sh, "${v:2}"), vec!["cdef"]);
        assert_eq!(fields(&mut sh, "${v:2:3}"), vec!["cde"]);
        // A zero length is an empty field, not the rest of the string.
        assert_eq!(fields(&mut sh, "\"${v:2:0}\""), vec![""]);
        // An offset past the end clamps rather than failing.
        assert_eq!(fields(&mut sh, "\"${v:7}\""), vec![""]);
        // A negative offset counts back from the end -- and needs the space or
        // the parens, since `${v:-1}` is the DEFAULT operator.
        assert_eq!(fields(&mut sh, "${v: -2}"), vec!["ef"]);
        assert_eq!(fields(&mut sh, "${v:(-2)}"), vec!["ef"]);
        assert_eq!(fields(&mut sh, "${v:-1}"), vec!["abcdef"]);
        // A negative LENGTH is an end index measured from the end, not a count.
        assert_eq!(fields(&mut sh, "${v:1:-1}"), vec!["bcde"]);
        assert_eq!(fields(&mut sh, "\"${v:0:-6}\""), vec![""]);
        // Characters, not bytes.
        let mut u = sh_with(&[("v", "--\u{3bc}--")]);
        assert_eq!(fields(&mut u, "${v:1:3}"), vec!["-\u{3bc}-"]);
    }

    /// Both operands are ARITHMETIC, so they read variables and operators the
    /// way `$(( ))` does. An EMPTY one is zero, which `arith::eval` alone would
    /// call an error.
    #[test]
    fn substring_operands_are_arithmetic() {
        let mut sh = sh_with(&[("v", "abcdef"), ("n", "2")]);
        assert_eq!(fields(&mut sh, "${v:1+1}"), vec!["cdef"]);
        assert_eq!(fields(&mut sh, "${v:1+1:2*1}"), vec!["cd"]);
        assert_eq!(fields(&mut sh, "${v:n}"), vec!["cdef"]);
        assert_eq!(fields(&mut sh, "${v:n:n}"), vec!["cd"]);
        assert_eq!(fields(&mut sh, "${v:0x2:010}"), vec!["cdef"]);
        assert_eq!(fields(&mut sh, "${v:${#v}-2}"), vec!["ef"]);
        let mut e = sh_with(&[("v", "123")]);
        assert_eq!(fields(&mut e, "${v:0}"), vec!["123"]);
        assert_eq!(fields(&mut e, "\"${v::}\""), vec![""]);
        assert_eq!(fields(&mut e, "${v: }"), vec!["123"]);
    }

    /// The colon between offset and length is found at the TOP level, so one
    /// inside a substitution or a ternary is not it.
    #[test]
    fn the_slice_colon_is_found_at_the_top_level() {
        let mut sh = sh_with(&[("v", "abcdef")]);
        // `1>0?1:2` is one expression whose colon belongs to the ternary.
        assert_eq!(fields(&mut sh, "${v:1>0?1:2}"), vec!["bcdef"]);
        assert_eq!(fields(&mut sh, "${v:$(echo 1):$(echo 2)}"), vec!["bc"]);
    }

    /// `$@`/`$*` slice the POSITIONALS rather than their join, so a slice of
    /// `$@` is several fields. `${@:0}` is the one place `$0` has an index.
    #[test]
    fn a_positional_slice_yields_fields() {
        let mut sh = Shell::new_for_test();
        sh.params = vec!["a".into(), "b".into(), "c".into()];
        assert_eq!(fields(&mut sh, "${@:1:2}"), vec!["a", "b"]);
        assert_eq!(fields(&mut sh, "${@:2}"), vec!["b", "c"]);
        // A negative offset counts from the last POSITIONAL, not from `$0`.
        assert_eq!(fields(&mut sh, "${@: -2}"), vec!["b", "c"]);
        assert_eq!(fields(&mut sh, "\"${@:9}\""), Vec::<String>::new());
        // `$*` joins on IFS's first character instead.
        assert_eq!(fields(&mut sh, "\"${*:1:2}\""), vec!["a b"]);
        let _ = sh.set_var("IFS", "-");
        assert_eq!(fields(&mut sh, "\"${*:1:2}\""), vec!["a-b"]);
        // Quoting keeps each element one field even when it contains a blank.
        let mut q = Shell::new_for_test();
        q.params = vec!["x y".into(), "z".into()];
        assert_eq!(fields(&mut q, "\"${@:1:1}\""), vec!["x y"]);
        assert_eq!(fields(&mut q, "${@:1:1}"), vec!["x", "y"]);
    }

    /// An offset that reaches outside the sequence answers EMPTY and raises no
    /// error -- not even with a length, which an in-range offset would refuse.
    /// Clamping it to the front instead would return a prefix nobody asked for.
    #[test]
    fn an_out_of_range_offset_is_empty_rather_than_clamped() {
        let mut sh = sh_with(&[("v", "abcdef")]);
        assert_eq!(fields(&mut sh, "\"${v: -6}\""), vec!["abcdef"]);
        assert_eq!(fields(&mut sh, "\"${v: -7}\""), vec![""]);
        assert_eq!(fields(&mut sh, "\"${v: -7:3}\""), vec![""]);
        assert_eq!(fields(&mut sh, "\"${v: -7:-1}\""), vec![""]);
        // Past the END short-circuits BEFORE the length is looked at, which is
        // what separates these two: one is empty, the other an error.
        assert_eq!(fields(&mut sh, "\"${v:7:-9}\""), vec![""]);
        assert!(expand_fields(&mut sh, &word("${v:6:-9}")).is_err());
        assert!(expand_fields(&mut sh, &word("${v:2:-9}")).is_err());
    }

    /// A negative offset over the positionals counts back over the list that
    /// carries `$0`, so `${@: -4}` with three of them reaches `$0` itself. And
    /// a negative LENGTH, which is an end index for a string, is simply refused
    /// here -- bash has no end-relative form for the positionals.
    #[test]
    fn a_positional_slice_counts_over_arg0_and_refuses_a_negative_length() {
        let mut sh = Shell::new_for_test();
        sh.arg0 = "SH".into();
        sh.params = vec!["a".into(), "b".into(), "c".into()];
        assert_eq!(fields(&mut sh, "${@: -3}"), vec!["a", "b", "c"]);
        assert_eq!(fields(&mut sh, "${@: -4}"), vec!["SH", "a", "b", "c"]);
        assert_eq!(fields(&mut sh, "\"${@: -5}\""), Vec::<String>::new());
        assert!(expand_fields(&mut sh, &word("${@:1:-1}")).is_err());
        assert!(expand_fields(&mut sh, &word("${*:1:-1}")).is_err());
        // But an out-of-range offset still answers empty without looking at it.
        assert_eq!(fields(&mut sh, "\"${@:99:-1}\""), Vec::<String>::new());
    }

    /// A quoted `$*` is ONE field however few elements it joins, where a quoted
    /// `$@` selecting nothing is no field at all. Only `$#` shows the
    /// difference, which is why it needs its own test.
    #[test]
    fn a_quoted_star_slice_is_always_one_field() {
        let mut sh = Shell::new_for_test();
        sh.params = vec!["a".into(), "b".into(), "c".into()];
        assert_eq!(fields(&mut sh, "\"${*:9}\""), vec![""]);
        assert_eq!(fields(&mut sh, "\"${*:1:0}\""), vec![""]);
        assert_eq!(fields(&mut sh, "\"${@:9}\""), Vec::<String>::new());
        // Unquoted, an empty join is no field at all.
        assert_eq!(fields(&mut sh, "${*:9}"), Vec::<String>::new());
    }
}

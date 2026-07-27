//! `grep` — select lines matching a pattern.
//!
//! POSIX `grep` plus the GNU options td's own scripts and the busybox applet it
//! replaces actually use. Everything is byte-oriented (C locale, see `regex`);
//! inputs are read whole, so a pattern never straddles a read boundary.
//!
//! Deliberate omissions: no `-P` (PCRE — a second regex engine), no `--color`,
//! no `--include`/`--exclude` globs, and no `-b` byte offsets. Each is a diagnosed
//! `invalid option`, never a silent no-op, and each is pinned in
//! spec/divergence.test.txt.

use crate::regex::{Filter, OnBudget, Options, Regex};
use crate::util::{
    errmsg, number, path_bytes, print_line, read_input, records, show, walk, Out, VERSION,
};

const USAGE: &str = "usage: grep [-EFGHhicLlnoqsvwxrzZ] [-m NUM] [-A NUM] [-B NUM] [-C NUM] \
                     [-e PATTERN] [-f FILE] [PATTERN] [FILE]...";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Syntax {
    Basic,
    Extended,
    Fixed,
}

struct Conf {
    syntax: Syntax,
    icase: bool,
    invert: bool,
    word: bool,
    whole_line: bool,
    count: bool,
    files_with: bool,
    files_without: bool,
    /// `-Z`: terminate a FILE NAME with NUL instead of the byte that would
    /// normally follow it, so a name holding a newline survives the pipe to a
    /// NUL-separated consumer.
    null_name: bool,
    only: bool,
    quiet: bool,
    no_messages: bool,
    line_number: bool,
    /// `None` until `-H`/`-h` or the file count decides it.
    with_filename: Option<bool>,
    recursive: bool,
    max_count: Option<u64>,
    after: usize,
    before: usize,
    null_data: bool,
    /// `-a`: treat a file containing NUL as text instead of reporting a match.
    text: bool,
}

impl Default for Conf {
    fn default() -> Self {
        Self {
            syntax: Syntax::Basic,
            icase: false,
            invert: false,
            word: false,
            whole_line: false,
            count: false,
            files_with: false,
            files_without: false,
            null_name: false,
            only: false,
            quiet: false,
            no_messages: false,
            line_number: false,
            with_filename: None,
            recursive: false,
            max_count: None,
            after: 0,
            before: 0,
            null_data: false,
            text: false,
        }
    }
}

/// A compiled pattern set: literals for `-F`, regexes otherwise. One pattern per
/// line of every `-e`/`-f` argument, matching any of them selects a line.
enum Patterns {
    Fixed(Vec<Vec<u8>>),
    Regex(Vec<Regex>),
}

impl Grep {
    /// Whether NO line of any file can be selected, so the file need not be
    /// read: `-m 0`, an empty pattern list, or an every-line pattern that `-v`
    /// inverts. GNU short-circuits all three — even `-c` prints no count line.
    /// `-L` reports the ABSENCE of a match, so it is handled separately.
    fn settled(&self) -> bool {
        self.conf.max_count == Some(0)
            || (self.pats.is_empty() && !self.conf.invert)
            // `-x`/`-w` narrow an empty pattern to empty/word-bounded lines, so
            // it no longer selects everything and nothing can be concluded.
            || (self.match_all
                && self.conf.invert
                && !self.conf.whole_line
                && !self.conf.word)
    }
}

impl Patterns {
    fn is_empty(&self) -> bool {
        match self {
            Patterns::Fixed(v) => v.is_empty(),
            Patterns::Regex(v) => v.is_empty(),
        }
    }
}

struct Grep {
    conf: Conf,
    pats: Patterns,
    /// Some pattern is empty, so every line matches unless `-w`/`-x` narrows it.
    /// Only `settled` reads this; matching compiles an empty pattern like any other.
    match_all: bool,
}

fn is_word(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// The `-w` test splits in two, and this half depends only on the START. That is what
/// lets the scan rule out a start position without matching there (see `regex::Filter`),
/// so both callers must agree on it — hence one function rather than two copies.
fn word_start_ok(line: &[u8], s: usize) -> bool {
    s.checked_sub(1).is_none_or(|i| line.get(i).is_none_or(|b| !is_word(*b)))
}

/// The other half: the byte AFTER the span must not be a word byte either. Past the
/// end of the line counts as a boundary.
fn word_end_ok(line: &[u8], e: usize) -> bool {
    line.get(e).is_none_or(|b| !is_word(*b))
}

fn eq_fold(a: u8, b: u8, icase: bool) -> bool {
    if icase {
        return a.eq_ignore_ascii_case(&b);
    }
    a == b
}

/// Leftmost occurrence of `needle` in `hay` at or after `from`.
fn locate(hay: &[u8], needle: &[u8], from: usize, icase: bool) -> Option<(usize, usize)> {
    if needle.is_empty() {
        return if from <= hay.len() { Some((from, from)) } else { None };
    }
    let mut start = from;
    while start + needle.len() <= hay.len() {
        let mut ok = true;
        for (i, want) in needle.iter().enumerate() {
            let Some(got) = hay.get(start + i) else {
                ok = false;
                break;
            };
            if !eq_fold(*got, *want, icase) {
                ok = false;
                break;
            }
        }
        if ok {
            return Some((start, start + needle.len()));
        }
        start += 1;
    }
    None
}

impl Grep {
    /// The leftmost-then-longest match in `line` at or after `from`, honoring `-w`/`-x`
    /// — EXCEPT under `OnBudget::Existence`, where the span is only valid as a boolean:
    /// that mode stops at the first acceptable end, so `x\|xy` on `xy` reports `(0,1)`.
    /// Callers that print or edit the span pass `OnBudget::Fail`.
    ///
    /// `-w` cannot be a filter over `search`'s answer: GNU retries SHORTER matches at
    /// the same start before advancing, so a pattern whose greedy span is not
    /// word-bounded can still select the line on a shorter span. `grep -w '\.*'`
    /// selects `..a` (on `.`) and `.a` (on the empty span); testing only the greedy
    /// `..`/`.` would report no match. `search_filtered` carries the test.
    fn match_at(
        &self,
        line: &[u8],
        from: usize,
        on_budget: OnBudget,
    ) -> Result<Option<(usize, usize)>, String> {
        // An empty pattern takes no short cut here. It is compiled like any other, and
        // it still has to clear -x/-w: `grep -x ''` selects only EMPTY lines, and
        // `grep -w ''` only a word-bounded gap, which may lie well along the line
        // (`a.` matches on the span after the dot). Short-circuiting on "some pattern
        // is empty" also DISCARDED the other patterns, so `grep -w -e '' -e ab` missed
        // the `ab` GNU selects.
        let mut best: Option<(usize, usize)> = None;
        match &self.pats {
            Patterns::Fixed(lits) => {
                for lit in lits {
                    if settles(on_budget, best) {
                        return Ok(best);
                    }
                    let mut at = from;
                    while let Some((s, e)) = locate(line, lit, at, self.conf.icase) {
                        if self.acceptable(line, s, e) {
                            best = Some(better(best, (s, e)));
                            break;
                        }
                        at = s + 1;
                        if at > line.len() {
                            break;
                        }
                    }
                }
            }
            Patterns::Regex(res) => {
                // A pattern the budget could not settle does not decide the LIST: the
                // list is an OR, so a later pattern may still match, and only if none
                // does is the refusal the answer. Held aside rather than propagated.
                let mut refused: Option<String> = None;
                for re in res {
                    // A match already settles an existence question, and running a
                    // later pathological pattern could only turn that settled YES
                    // into `too complex`.
                    if settles(on_budget, best) {
                        return Ok(best);
                    }
                    let found = if self.conf.whole_line {
                        match from == 0 {
                            // `-x` consumes the span implicitly (it compares the end to
                            // the line length), so it never takes the relaxed budget.
                            true => re.matches_whole(line).map(|hit| match hit {
                                true => Some((0, line.len())),
                                false => None,
                            }),
                            false => Ok(None),
                        }
                    } else if self.conf.word {
                        // The word test belongs INSIDE the scan: GNU retries a shorter
                        // match at the same start before advancing, and the acceptable
                        // span can be shorter than the greedy one (or empty).
                        let span = |s: usize, e: usize| self.acceptable(line, s, e);
                        // Whether a start can bear a word-bounded span depends only on
                        // the byte BEFORE it, so the scan can rule one out without
                        // matching — see `Filter`.
                        let start = |s: usize| word_start_ok(line, s);
                        let filter = Filter { span: &span, start: &start };
                        // Searching every end costs more than testing one, so a
                        // pathological `-w` pattern can exhaust the budget where the
                        // one-span-per-start algorithm this replaced would have
                        // answered. That refusal is deliberate: the cheap answer is
                        // reachable, but it is the WRONG one — testing only the greedy
                        // span is the bug at the top of this file, and its "no match"
                        // is a false negative, which is worse than a diagnosed refusal.
                        re.search_filtered(line, from, &filter, on_budget)
                            .map(|c| c.map(|c| (c.start(), c.end())))
                    } else {
                        match on_budget {
                            OnBudget::Fail => re.search(line, from),
                            OnBudget::Existence => re.search_existence(line, from),
                        }
                        .map(|c| c.map(|c| (c.start(), c.end())))
                    };
                    match found {
                        Ok(Some(span)) => best = Some(better(best, span)),
                        Ok(None) => {}
                        // Spans are compared ACROSS patterns, so a caller that consumes
                        // one cannot proceed without every pattern's answer.
                        Err(e) if on_budget == OnBudget::Fail => return Err(e.msg),
                        Err(e) => refused = refused.or(Some(e.msg)),
                    }
                }
                if best.is_none() {
                    if let Some(msg) = refused {
                        return Err(msg);
                    }
                }
            }
        }
        Ok(best)
    }

    /// `-w`/`-x` filters on a candidate span.
    fn acceptable(&self, line: &[u8], s: usize, e: usize) -> bool {
        if self.conf.whole_line && (s != 0 || e != line.len()) {
            return false;
        }
        if self.conf.word && !(word_start_ok(line, s) && word_end_ok(line, e)) {
            return false;
        }
        true
    }

    fn selects(&self, line: &[u8]) -> Result<bool, String> {
        // Selection asks only WHETHER the line matches, so a budget exhausted with
        // a match in hand still answers it. `-o` below consumes the span and cannot.
        let hit = self.match_at(line, 0, OnBudget::Existence)?.is_some();
        Ok(hit != self.conf.invert)
    }
}

/// Whether a match in hand ends the search over the pattern LIST. Only when the
/// caller asked existence: `-o` and sed compare spans ACROSS patterns and need them all.
fn settles(on_budget: OnBudget, best: Option<(usize, usize)>) -> bool {
    on_budget == OnBudget::Existence && best.is_some()
}

/// Prefer the leftmost span, then the longest — the order `grep -o` prints in.
fn better(cur: Option<(usize, usize)>, cand: (usize, usize)) -> (usize, usize) {
    match cur {
        None => cand,
        Some((s, e)) => {
            if cand.0 < s || (cand.0 == s && cand.1 > e) {
                cand
            } else {
                (s, e)
            }
        }
    }
}

fn err(msg: &str) {
    eprintln!("grep: {msg}");
}

/// Applet entry point. `args[0]` is the invoked name.
pub fn main(args: &[Vec<u8>]) -> i32 {
    let mut conf = Conf::default();
    let mut patterns: Vec<Vec<u8>> = Vec::new();
    let mut pattern_seen = false;
    let mut files: Vec<Vec<u8>> = Vec::new();
    let mut operands: Vec<Vec<u8>> = Vec::new();
    let mut no_more_options = false;

    let mut i = 1usize;
    while let Some(arg) = args.get(i) {
        i += 1;
        if no_more_options || arg.first() != Some(&b'-') || arg.len() == 1 {
            operands.push(arg.clone());
            continue;
        }
        if arg.as_slice() == b"--" {
            no_more_options = true;
            continue;
        }
        if arg.starts_with(b"--") {
            let (name, inline) = split_long(arg);
            let (name, arity) = match resolve_long(&name) {
                Ok(pair) => pair,
                Err(msg) if msg.is_empty() => {
                    err(&format!("unrecognized option '{}'", show(arg)));
                    eprintln!("{USAGE}");
                    return 2;
                }
                Err(msg) => {
                    err(&msg);
                    eprintln!("{USAGE}");
                    return 2;
                }
            };
            // Arity is GNU's: a value-taking option accepts `=VALUE` or the next
            // argv element; a flag accepts neither.
            let value = match (arity, inline) {
                (Arg::None, Some(_)) => {
                    err(&format!("option '--{}' doesn't allow an argument", show(name)));
                    eprintln!("{USAGE}");
                    return 2;
                }
                (Arg::None, None) => None,
                (Arg::Required, Some(v)) => Some(v),
                (Arg::Required, None) => match args.get(i) {
                    Some(v) => {
                        i += 1;
                        Some(v.clone())
                    }
                    None => {
                        err(&format!("option '--{}' requires an argument", show(name)));
                        eprintln!("{USAGE}");
                        return 2;
                    }
                },
            };
            // Both answer on stdout and exit 0 the moment they are seen, before
            // any later option is applied — but AFTER the arity check above, so
            // `--version=x` is still the error GNU makes it. The TEXT is td-txt's
            // own: reporting a GNU banner would be a lie a caller could act on.
            if name == b"help" || name == b"version" {
                let line = if name == b"help" {
                    USAGE.to_string()
                } else {
                    format!("grep (td-txt) {VERSION}")
                };
                return match print_line(&line) {
                    Ok(()) => 0,
                    Err(e) => {
                        err(&format!("write error: {}", errmsg(&e)));
                        2
                    }
                };
            }
            match parse_long(&mut conf, name, value.as_deref(), &mut patterns, &mut pattern_seen) {
                Ok(()) => continue,
                // `resolve_long` already rejected an unknown name.
                Err(LongErr::Unknown) => {
                    err(&format!("unrecognized option '{}'", show(arg)));
                    eprintln!("{USAGE}");
                    return 2;
                }
                Err(LongErr::Message(m)) => {
                    err(&m);
                    return 2;
                }
            }
        }
        // A short-option cluster; an option that takes a value consumes the rest
        // of the cluster, or the next argument when the cluster ends.
        let mut j = 1usize;
        while let Some(opt) = arg.get(j).copied() {
            j += 1;
            let value_of = |j: &mut usize, i: &mut usize| -> Option<Vec<u8>> {
                if let Some(rest) = arg.get(*j..) {
                    if !rest.is_empty() {
                        *j = arg.len();
                        return Some(rest.to_vec());
                    }
                }
                let v = args.get(*i).cloned();
                if v.is_some() {
                    *i += 1;
                }
                v
            };
            match opt {
                b'E' => conf.syntax = Syntax::Extended,
                b'F' => conf.syntax = Syntax::Fixed,
                b'G' => conf.syntax = Syntax::Basic,
                b'i' | b'y' => conf.icase = true,
                b'v' => conf.invert = true,
                b'w' => conf.word = true,
                b'x' => conf.whole_line = true,
                b'c' => conf.count = true,
                b'l' => conf.files_with = true,
                b'L' => conf.files_without = true,
                b'Z' => conf.null_name = true,
                b'o' => conf.only = true,
                b'q' => conf.quiet = true,
                b's' => conf.no_messages = true,
                b'n' => conf.line_number = true,
                b'h' => conf.with_filename = Some(false),
                b'H' => conf.with_filename = Some(true),
                b'r' | b'R' => conf.recursive = true,
                b'z' => conf.null_data = true,
                b'a' => conf.text = true,
                b'e' => match value_of(&mut j, &mut i) {
                    Some(v) => {
                        push_expr(&mut patterns, &v);
                        pattern_seen = true;
                    }
                    None => {
                        err("option requires an argument -- 'e'");
                        eprintln!("{USAGE}");
                        return 2;
                    }
                },
                b'f' => match value_of(&mut j, &mut i) {
                    Some(v) => match read_input(&v) {
                        Ok(bytes) => {
                            push_file(&mut patterns, &bytes);
                            pattern_seen = true;
                        }
                        Err(e) => {
                            err(&format!("{}: {}", show(&v), errmsg(&e)));
                            return 2;
                        }
                    },
                    None => {
                        err("option requires an argument -- 'f'");
                        eprintln!("{USAGE}");
                        return 2;
                    }
                },
                b'm' => {
                    let Some(v) = value_of(&mut j, &mut i) else {
                        err(&format!("option requires an argument -- '{}'", char::from(opt)));
                        return 2;
                    };
                    let Some(limit) = parse_max_count(&v) else {
                        err("invalid max count");
                        return 2;
                    };
                    conf.max_count = limit;
                    break;
                }
                b'A' | b'B' | b'C' => {
                    let Some(v) = value_of(&mut j, &mut i) else {
                        err(&format!("option requires an argument -- '{}'", char::from(opt)));
                        return 2;
                    };
                    let Some(n) = parse_count(&v) else {
                        err(&format!("{}: invalid context length argument", show(&v)));
                        return 2;
                    };
                    apply_count(&mut conf, opt, n);
                }
                _ => {
                    err(&format!("invalid option -- '{}'", char::from(opt)));
                    eprintln!("{USAGE}");
                    return 2;
                }
            }
        }
    }

    // Without -e/-f the first operand is the pattern.
    let mut operands = operands.into_iter();
    if !pattern_seen {
        match operands.next() {
            Some(p) => push_expr(&mut patterns, &p),
            None => {
                eprintln!("{USAGE}");
                return 2;
            }
        }
    }
    files.extend(operands);

    let pats = match compile(&conf, &patterns) {
        Ok(p) => p,
        Err(msg) => {
            err(&msg);
            return 2;
        }
    };
    let match_all = patterns.iter().any(Vec::is_empty);

    // Recursion expands directories; a bare `grep -r pat` reads the working tree.
    let mut inputs: Vec<Vec<u8>> = Vec::new();
    let mut status_error = false;
    if conf.recursive {
        // A bare `grep -r PAT` walks the working tree but names its hits
        // `d/x`, not `./d/x` — the synthesized operand must not reach the
        // output, or a consumer of `grep -rl` sees paths GNU never prints.
        let implied_cwd = files.is_empty();
        if implied_cwd {
            files.push(b".".to_vec());
        }
        for f in &files {
            let (mut paths, mut errs) = (Vec::new(), Vec::new());
            walk(&crate::util::path_from_bytes(f), &mut paths, &mut errs);
            inputs.extend(paths.iter().map(|p| {
                let b = path_bytes(p);
                match implied_cwd {
                    true => b.strip_prefix(b"./".as_slice()).unwrap_or(&b).to_vec(),
                    false => b,
                }
            }));
            for (path, e) in &errs {
                if !conf.no_messages {
                    err(&format!("{}: {}", show(&path_bytes(path)), errmsg(e)));
                }
                status_error = true;
            }
        }
    } else if files.is_empty() {
        inputs.push(b"-".to_vec());
    } else {
        inputs = files.clone();
    }

    let show_name = conf
        .with_filename
        .unwrap_or(inputs.len() > 1 || (conf.recursive && !files.is_empty()));
    let grep = Grep { conf, pats, match_all };
    let mut out = Out::new();
    let mut any_match = false;
    let mut printed_groups = false;

    // Nothing to look for and no `-v` to invert it: GNU never OPENS the
    // operands, so a nonexistent one is not an error either. `-L` is the
    // exception — it must still report the file, so it still stats it.
    // `-L` must still report each file, so it reads (stats) them even when the
    // answer is already known.
    let settled = grep.settled() && !grep.conf.files_without;
    for path in &inputs {
        let data = if settled {
            Vec::new()
        } else {
            match read_input(path) {
                Ok(d) => d,
                Err(e) => {
                    if !grep.conf.no_messages {
                        err(&format!("{}: {}", show(path), errmsg(&e)));
                    }
                    status_error = true;
                    continue;
                }
            }
        };
        match search_file(&grep, &mut out, path, &data, show_name, &mut printed_groups) {
            Ok(hit) => any_match |= hit,
            Err(msg) => {
                err(&msg);
                return 2;
            }
        }
        if grep.conf.quiet && any_match {
            break;
        }
        if out.is_broken() {
            break;
        }
    }
    if out.flush().is_err() {
        err("write error");
        return 2;
    }
    if grep.conf.quiet && any_match {
        return 0;
    }
    if status_error {
        return 2;
    }
    if any_match {
        0
    } else {
        1
    }
}

enum LongErr {
    Unknown,
    Message(String),
}

fn split_long(arg: &[u8]) -> (Vec<u8>, Option<Vec<u8>>) {
    let body = arg.get(2..).unwrap_or_default();
    match body.iter().position(|b| *b == b'=') {
        Some(eq) => (
            body.get(..eq).unwrap_or_default().to_vec(),
            Some(body.get(eq + 1..).unwrap_or_default().to_vec()),
        ),
        None => (body.to_vec(), None),
    }
}

/// Every long option this applet knows, so an unambiguous PREFIX resolves the
/// way GNU's `getopt_long` accepts one (`grep --ignore-c`, `sed --quie`). An
/// exact name always wins over being a prefix of a longer one.
/// A long option's argument policy, so `--max-count 1` works and `--count=1`
/// is refused the way GNU's `getopt_long` refuses it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Arg {
    None,
    Required,
}

const LONG_OPTIONS: &[(&[u8], Arg)] = &[
    (b"after-context", Arg::Required),
    (b"basic-regexp", Arg::None),
    (b"before-context", Arg::Required),
    (b"context", Arg::Required),
    (b"count", Arg::None),
    (b"dereference-recursive", Arg::None),
    (b"extended-regexp", Arg::None),
    (b"file", Arg::Required),
    (b"files-with-matches", Arg::None),
    (b"files-without-match", Arg::None),
    (b"fixed-strings", Arg::None),
    (b"help", Arg::None),
    (b"ignore-case", Arg::None),
    (b"invert-match", Arg::None),
    (b"line-number", Arg::None),
    (b"line-regexp", Arg::None),
    (b"max-count", Arg::Required),
    (b"no-filename", Arg::None),
    (b"no-messages", Arg::None),
    (b"null", Arg::None),
    (b"null-data", Arg::None),
    (b"only-matching", Arg::None),
    (b"quiet", Arg::None),
    (b"recursive", Arg::None),
    (b"regexp", Arg::Required),
    (b"silent", Arg::None),
    (b"text", Arg::None),
    (b"version", Arg::None),
    (b"with-filename", Arg::None),
    (b"word-regexp", Arg::None),
];

fn resolve_long(name: &[u8]) -> Result<(&'static [u8], Arg), String> {
    let mut hits: Vec<(&'static [u8], Arg)> = Vec::new();
    for (cand, arity) in LONG_OPTIONS {
        if *cand == name {
            return Ok((cand, *arity));
        }
        if cand.starts_with(name) {
            hits.push((cand, *arity));
        }
    }
    match hits.as_slice() {
        [one] => Ok(*one),
        [] => Err(String::new()),
        many => {
            let list: Vec<String> = many.iter().map(|(n, _)| format!("'--{}'", show(n))).collect();
            Err(format!(
                "option '--{}' is ambiguous; possibilities: {}",
                show(name),
                list.join(" ")
            ))
        }
    }
}

fn parse_long(
    conf: &mut Conf,
    name: &[u8],
    value: Option<&[u8]>,
    patterns: &mut Vec<Vec<u8>>,
    pattern_seen: &mut bool,
) -> Result<(), LongErr> {
    let need = |value: Option<&[u8]>| -> Result<Vec<u8>, LongErr> {
        value
            .map(<[u8]>::to_vec)
            .ok_or_else(|| LongErr::Message(format!("option '--{}' requires an argument", show(name))))
    };
    let count = |value: Option<&[u8]>| -> Result<usize, LongErr> {
        let v = need(value)?;
        parse_count(&v).ok_or_else(|| LongErr::Message(format!("{}: invalid context length argument", show(&v))))
    };
    match name {
        b"extended-regexp" => conf.syntax = Syntax::Extended,
        b"fixed-strings" => conf.syntax = Syntax::Fixed,
        b"basic-regexp" => conf.syntax = Syntax::Basic,
        b"ignore-case" => conf.icase = true,
        b"invert-match" => conf.invert = true,
        b"word-regexp" => conf.word = true,
        b"line-regexp" => conf.whole_line = true,
        b"count" => conf.count = true,
        b"files-with-matches" => conf.files_with = true,
        b"files-without-match" => conf.files_without = true,
        b"null" => conf.null_name = true,
        b"only-matching" => conf.only = true,
        b"quiet" | b"silent" => conf.quiet = true,
        b"no-messages" => conf.no_messages = true,
        b"line-number" => conf.line_number = true,
        b"with-filename" => conf.with_filename = Some(true),
        b"no-filename" => conf.with_filename = Some(false),
        b"recursive" | b"dereference-recursive" => conf.recursive = true,
        b"null-data" => conf.null_data = true,
        b"text" => conf.text = true,
        b"regexp" => {
            push_expr(patterns, &need(value)?);
            *pattern_seen = true;
        }
        b"file" => {
            let path = need(value)?;
            let bytes = read_input(&path)
                .map_err(|e| LongErr::Message(format!("{}: {}", show(&path), errmsg(&e))))?;
            push_file(patterns, &bytes);
            *pattern_seen = true;
        }
        b"max-count" => {
            let v = need(value)?;
            conf.max_count = parse_max_count(&v)
                .ok_or_else(|| LongErr::Message("invalid max count".to_string()))?;
        }
        b"after-context" => conf.after = count(value)?,
        b"before-context" => conf.before = count(value)?,
        b"context" => {
            let n = count(value)?;
            conf.after = n;
            conf.before = n;
        }
        _ => return Err(LongErr::Unknown),
    }
    Ok(())
}

fn apply_count(conf: &mut Conf, opt: u8, n: usize) {
    match opt {
        b'A' => conf.after = n,
        b'B' => conf.before = n,
        _ => {
            conf.after = n;
            conf.before = n;
        }
    }
}

/// `-m`'s argument. GNU reads a NEGATIVE count as no limit at all, so it must
/// not go through the unsigned context parser.
fn parse_max_count(v: &[u8]) -> Option<Option<u64>> {
    if let Some(rest) = v.strip_prefix(b"-") {
        return parse_count(rest).map(|_| None);
    }
    parse_count(v).map(|n| Some(n as u64))
}

fn parse_count(v: &[u8]) -> Option<usize> {
    if v.is_empty() || !v.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let mut n: usize = 0;
    for b in v {
        n = n.checked_mul(10)?.checked_add(usize::from(b - b'0'))?;
    }
    Some(n)
}

/// A `-e` argument is a pattern LIST, and its final newline is a separator like
/// any other: `-e $'a\n'` is the two patterns `a` and `` (which matches every
/// line). That is the opposite of a `-f` file, whose final newline just ends the
/// last line.
fn push_expr(patterns: &mut Vec<Vec<u8>>, arg: &[u8]) {
    let mut start = 0usize;
    for (i, b) in arg.iter().enumerate() {
        if *b == b'\n' {
            patterns.push(arg.get(start..i).unwrap_or_default().to_vec());
            start = i + 1;
        }
    }
    patterns.push(arg.get(start..).unwrap_or_default().to_vec());
}

/// A `-f` file contributes one pattern per line — so an EMPTY file contributes
/// none at all, which matches nothing (GNU exits 1), unlike `-e ''`.
fn push_file(patterns: &mut Vec<Vec<u8>>, content: &[u8]) {
    for (line, _) in records(content, b'\n') {
        patterns.push(line.to_vec());
    }
}

fn compile(conf: &Conf, lines: &[Vec<u8>]) -> Result<Patterns, String> {
    if conf.syntax == Syntax::Fixed {
        let lines = lines.to_vec();
        return Ok(Patterns::Fixed(lines));
    }
    let opts = Options {
        ere: conf.syntax == Syntax::Extended,
        icase: conf.icase,
        escapes: false,
        multiline: false,
    };
    let mut res = Vec::with_capacity(lines.len());
    for line in lines {
        res.push(Regex::compile(line, opts).map_err(|e| e.msg)?);
    }
    Ok(Patterns::Regex(res))
}

/// Emit `file:` / `line:` prefixes. `sep` is `:` on a selected line and `-` on a
/// context line, as GNU prints them.
fn prefix(out: &mut Out, grep: &Grep, path: &[u8], show_name: bool, lineno: u64, sep: u8) -> std::io::Result<()> {
    if show_name {
        out.write(path)?;
        // -Z replaces only the byte after the NAME; a following line number
        // still carries the ordinary separator.
        out.write(&[if grep.conf.null_name { 0 } else { sep }])?;
    }
    if grep.conf.line_number {
        out.write(&number(lineno))?;
        out.write(&[sep])?;
    }
    Ok(())
}

fn search_file(
    grep: &Grep,
    out: &mut Out,
    path: &[u8],
    data: &[u8],
    show_name: bool,
    // Whether an earlier FILE already printed a context group. `printed_upto`
    // resets per file, so without this the `--` at a file boundary is lost.
    printed_before: &mut bool,
) -> Result<bool, String> {
    let sep = if grep.conf.null_data { 0u8 } else { b'\n' };
    let display: &[u8] = if path == b"-" { b"(standard input)" } else { path };
    // A binary file's MATCHING LINES are replaced by a notice — so `-o`, which
    // prints matched text, is suppressed too. `-c`/`-l`/`-L`/`-q` print no line
    // content at all, so they run normally and emit no notice.
    let counts_only = grep.conf.count
        || grep.conf.files_with
        || grep.conf.files_without
        || grep.conf.quiet;
    let binary = !grep.conf.text && !counts_only && data.contains(&0) && sep != 0;
    let lines = records(data, sep);
    let mut count: u64 = 0;
    let mut printed_upto: usize = 0; // 1-based line number already emitted
    let mut pending_after: usize = 0;
    let mut any = false;

    let io = |r: std::io::Result<()>| -> Result<(), String> { r.map_err(|e| format!("write error: {}", errmsg(&e))) };
    if grep.settled() {
        if grep.conf.files_without && !grep.conf.quiet {
            io(out.write(display))?;
            io(out.write(&[if grep.conf.null_name { 0 } else { b'\n' }]))?;
        }
        return Ok(false);
    }
    let mut limit_reached = false;

    for (idx, (line, _)) in lines.iter().enumerate() {
        let lineno = (idx + 1) as u64;
        // Past the -m limit only the trailing context of the last match is still
        // owed, so stop selecting but keep draining `pending_after`.
        let selected = !limit_reached && grep.selects(line)?;
        if selected {
            any = true;
            count += 1;
            if grep.conf.quiet || grep.conf.files_with || grep.conf.files_without {
                // -l/-L print one summary line below; -q prints nothing. Either
                // way the answer is settled, so stop reading this file.
                break;
            }
            if binary {
                // GNU 3.11 reports a binary match on STDERR and keeps stdout
                // clean, so a pipeline gets no stray bytes; `-a` opts out by
                // clearing `binary` (everything is text). One notice per file.
                err(&format!("{}: binary file matches", show(display)));
                return Ok(true);
            }
            if !grep.conf.count {
                // Trailing context of the previous group, then leading context.
                let start = idx.saturating_sub(grep.conf.before);
                if grep.conf.before > 0 || grep.conf.after > 0 {
                    let gap = match printed_upto {
                        0 => *printed_before,
                        upto => start > upto,
                    };
                    if gap {
                        io(out.write(b"--\n"))?;
                    }
                    *printed_before = true;
                    let ctx_start = start.max(printed_upto);
                    for (k, (ctx, ctx_term)) in lines.iter().enumerate().take(idx).skip(ctx_start) {
                        io(prefix(out, grep, display, show_name, (k + 1) as u64, b'-'))?;
                        io(out.write(ctx))?;
                        if *ctx_term || k + 1 < lines.len() {
                            io(out.write(&[sep]))?;
                        }
                    }
                }
                if grep.conf.only {
                    let mut at = 0usize;
                    while let Some((s, e)) = grep.match_at(line, at, OnBudget::Fail)? {
                        if e == s {
                            at = s + 1;
                            if at > line.len() {
                                break;
                            }
                            continue;
                        }
                        io(prefix(out, grep, display, show_name, lineno, b':'))?;
                        io(out.write(line.get(s..e).unwrap_or_default()))?;
                        io(out.write(&[sep]))?;
                        at = e;
                    }
                } else {
                    io(prefix(out, grep, display, show_name, lineno, b':'))?;
                    io(out.write(line))?;
                    // GNU terminates every line it prints, including a final
                    // input line that carried no newline of its own.
                    io(out.write(&[sep]))?;
                }
                printed_upto = idx + 1;
                pending_after = grep.conf.after;
            }
            if grep.conf.max_count.is_some_and(|m| count >= m) {
                // GNU still prints the trailing context of that last match.
                if pending_after == 0 {
                    break;
                }
                limit_reached = true;
            }
        } else if pending_after > 0 && !grep.conf.count && !grep.conf.only {
            io(prefix(out, grep, display, show_name, lineno, b'-'))?;
            io(out.write(line))?;
            io(out.write(&[sep]))?;
            printed_upto = idx + 1;
            pending_after -= 1;
        }
        if out.is_broken() {
            break;
        }
    }

    // POSIX: -q writes NOTHING to stdout, so it outranks -c/-l/-L.
    if grep.conf.quiet {
        return Ok(any);
    }
    if grep.conf.count {
        io(prefix(out, grep, display, show_name, 0, b':'))?;
        io(out.write(&number(count)))?;
        io(out.write(b"\n"))?;
    }
    if (grep.conf.files_with && any) || (grep.conf.files_without && !any) {
        io(out.write(display))?;
        io(out.write(&[if grep.conf.null_name { 0 } else { b'\n' }]))?;
    }
    Ok(any)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conf() -> Conf {
        Conf::default()
    }

    fn grep_with(conf: Conf, exprs: &[&str]) -> Grep {
        let mut patterns: Vec<Vec<u8>> = Vec::new();
        for e in exprs {
            push_expr(&mut patterns, e.as_bytes());
        }
        let pats = compile(&conf, &patterns).unwrap();
        let match_all = patterns.iter().any(Vec::is_empty);
        Grep { conf, pats, match_all }
    }

    #[test]
    fn word_option_rejects_a_match_inside_a_word() {
        let g = grep_with(Conf { word: true, ..conf() }, &["cat"]);
        assert!(g.selects(b"the cat sat").unwrap());
        assert!(!g.selects(b"concatenate").unwrap());
    }

    #[test]
    fn word_option_retries_at_a_later_start() {
        let g = grep_with(Conf { word: true, ..conf() }, &["cat"]);
        assert!(g.selects(b"concatenate cat").unwrap());
    }

    /// The empty span is word-bounded only between non-word characters, which need not
    /// be the line start — so `-w ''` has to scan. An earlier short-circuit tested only
    /// the first position and called `a.` no match.
    #[test]
    fn word_option_scans_for_a_word_bounded_gap_under_an_empty_pattern() {
        let g = grep_with(Conf { word: true, ..conf() }, &[""]);
        // The only word-bounded gap in `a.` is the one after the dot.
        assert_eq!(g.match_at(b"a.", 0, OnBudget::Fail).unwrap(), Some((2, 2)));
        assert!(g.selects(b"a.").unwrap());
        assert!(g.selects(b"a ").unwrap());
        // Every gap in `ab`/`a.b` touches a word character.
        assert!(!g.selects(b"ab").unwrap());
        assert!(!g.selects(b"a.b").unwrap());
        // `-x` is unaffected: an empty pattern still selects only the empty line.
        let x = grep_with(Conf { whole_line: true, ..conf() }, &[""]);
        assert!(x.selects(b"").unwrap());
        assert!(!x.selects(b"a").unwrap());
    }

    /// Retrying at a later start is not the whole rule: GNU also retries a SHORTER
    /// span at the SAME start, so the span `-w` accepts can be shorter than the
    /// greedy one — or empty. Testing only the greedy span reported no match here.
    #[test]
    fn word_option_retries_a_shorter_span_at_the_same_start() {
        let g = grep_with(Conf { word: true, ..conf() }, &[r"\.*"]);
        // Greedy at 0 is `..`, not word-bounded (`a` follows); `.` is.
        assert_eq!(g.match_at(b"..a", 0, OnBudget::Fail).unwrap(), Some((0, 1)));
        // Greedy at 0 is `.`, not word-bounded; only the EMPTY span is.
        assert_eq!(g.match_at(b".a", 0, OnBudget::Fail).unwrap(), Some((0, 0)));
        assert!(g.selects(b"..a").unwrap());
        assert!(g.selects(b".a").unwrap());
        // A word character on either side blocks the empty span too.
        let g2 = grep_with(Conf { word: true, ..conf() }, &["a*"]);
        assert!(!g2.selects(b"ab").unwrap());
    }

    /// `-w` searching every end is only affordable because a start whose left
    /// neighbour is a word byte is skipped without matching at all — on a line of
    /// words that is most of them. The observable claim is the span: the match must
    /// begin at a word edge, never inside one.
    #[test]
    fn word_option_starts_only_at_a_word_edge() {
        let g = grep_with(Conf { word: true, ..conf() }, &["at"]);
        assert_eq!(g.match_at(b"cat at", 0, OnBudget::Fail).unwrap(), Some((4, 6)));
        assert!(!g.selects(b"cat cat").unwrap());
    }

    /// A pattern list is an OR, so one pattern the budget cannot settle must not
    /// decide it. Either order answers: a match already found ends the loop before the
    /// costly pattern runs, and a costly pattern that runs FIRST has its refusal held
    /// aside while the rest of the list is tried. Only a list where nothing matches
    /// refuses — see the case above.
    #[test]
    fn a_costly_pattern_does_not_veto_a_match_from_another() {
        let line = "key=val ".repeat(80);
        let costly = r"[^:]*.*a\(ab\)*a*";
        for pats in [vec!["key", costly], vec![costly, "key"]] {
            let g = grep_with(Conf { word: true, ..conf() }, &pats);
            assert!(g.selects(line.as_bytes()).unwrap(), "order {pats:?}");
        }
    }

    /// A `-w` pattern too costly to search exhaustively REFUSES rather than answering
    /// from the cheap one-span-per-start algorithm. That algorithm is the bug this
    /// file's `-w` fix removed, and its answer here would be a false negative — GNU
    /// selects nothing on this line, but only an exhaustive search establishes that,
    /// and this one does not finish.
    #[test]
    fn word_option_refuses_rather_than_guessing_when_out_of_budget() {
        let g = grep_with(Conf { word: true, ..conf() }, &[r"[^:]*.*a\(ab\)*a*"]);
        let line = "key=val ".repeat(80);
        let err = g.selects(line.as_bytes()).unwrap_err();
        assert!(err.contains("too complex"), "{err}");
    }

    #[test]
    fn line_option_requires_the_whole_line() {
        let g = grep_with(Conf { whole_line: true, ..conf() }, &["a*"]);
        assert!(g.selects(b"aaa").unwrap());
        assert!(!g.selects(b"aaab").unwrap());
    }

    #[test]
    fn invert_flips_selection() {
        let g = grep_with(Conf { invert: true, ..conf() }, &["x"]);
        assert!(g.selects(b"abc").unwrap());
        assert!(!g.selects(b"xbc").unwrap());
    }

    #[test]
    fn fixed_strings_take_no_metacharacters() {
        let g = grep_with(Conf { syntax: Syntax::Fixed, ..conf() }, &["a.c"]);
        assert!(g.selects(b"xa.cy").unwrap());
        assert!(!g.selects(b"abc").unwrap());
    }

    #[test]
    fn a_pattern_argument_holds_one_pattern_per_line() {
        let g = grep_with(conf(), &["foo\nbar"]);
        assert!(g.selects(b"bar").unwrap());
        assert!(g.selects(b"foo").unwrap());
        assert!(!g.selects(b"baz").unwrap());
    }

    #[test]
    fn only_matching_reports_leftmost_longest_spans() {
        let g = grep_with(conf(), &["ab*"]);
        assert_eq!(g.match_at(b"xabb yab", 0, OnBudget::Fail).unwrap(), Some((1, 4)));
        assert_eq!(g.match_at(b"xabb yab", 4, OnBudget::Fail).unwrap(), Some((6, 8)));
    }

    #[test]
    fn parse_count_rejects_a_non_number() {
        assert_eq!(parse_count(b"12"), Some(12));
        assert_eq!(parse_count(b"1x"), None);
        assert_eq!(parse_count(b""), None);
    }
}

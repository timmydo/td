//! `grep` — select lines matching a pattern.
//!
//! POSIX `grep` plus the GNU options td's own scripts and the busybox applet it
//! replaces actually use. Everything is byte-oriented (C locale, see `regex`);
//! inputs are read whole, so a pattern never straddles a read boundary.
//!
//! Deliberate omissions: no `-P` (PCRE — a second regex engine), no `--color`,
//! no `--include`/`--exclude` globs, and no `-b` byte offsets. Each is a
//! diagnosed `invalid option`, never a silent no-op, and each is pinned in
//! spec/divergence.test.txt.

use crate::regex::{Filter, OnBudget, Options, Regex};
use crate::util::{
    byte_in, errmsg, name_in, number, path_bytes, posixly_correct, print_line, read_input,
    read_search, records, walk, Diag, Out, VERSION,
};

/// How many significant digits `-NUM` takes before refusing the run. GNU's own
/// scanner stops there; the `-A`/`-B`/`-C` argument has no such limit.
const NUM_DIGIT_CAP: usize = 21;

const USAGE: &str = "usage: grep [-aEFGHhicLlnoqRrsvwxyzZ] [-NUM] [-m NUM] [-A NUM] [-B NUM] [-C NUM] \
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
    /// `-l`/`-L`, and never both: they are opposite questions and GNU keeps the
    /// last one given.
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
    /// `-R`: follow every symlink the walk finds, not only the operand.
    logical: bool,
    max_count: Option<u64>,
    /// `-A`/`-B` as GIVEN, and `-C` separately: `-C` does not override an
    /// explicit `-A`, it supplies the default for whichever was not given, so
    /// `-A2 -C0` and `-C0 -A2` are the same thing. `None` is "not given" and is
    /// not the same as `Some(0)`, which still groups.
    after: Option<usize>,
    before: Option<usize>,
    both: Option<usize>,
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
            logical: false,
            max_count: None,
            after: None,
            before: None,
            both: None,
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
    /// read: `-m 0`, a list of NO patterns, or a list of nothing but empty
    /// patterns under `-v`.
    /// GNU short-circuits all three — even `-c` prints no count line.
    /// `-L` reports the ABSENCE of a match, so it is handled separately.
    fn settled(&self) -> bool {
        self.conf.max_count == Some(0)
            || (self.pats.is_empty() && !self.conf.invert)
            // `-x`/`-w` narrow an empty pattern to empty/word-bounded lines, so
            // it no longer selects everything and nothing can be concluded.
            || (self.only_empty
                && self.conf.invert
                && !self.conf.whole_line
                && !self.conf.word)
    }
}

impl Conf {
    /// Which lines `-o` prints spans for: GNU asks whether the line is a
    /// MATCHING one, which is the selected line without `-v` and the CONTEXT
    /// line with it. Not "does it match" -- past `-m` a context line can match
    /// and still print nothing.
    fn prints_selected_spans(&self) -> bool {
        !self.invert
    }

    fn prints_context_spans(&self) -> bool {
        self.invert
    }

    fn after_lines(&self) -> usize {
        self.after.or(self.both).unwrap_or(0)
    }

    fn before_lines(&self) -> usize {
        self.before.or(self.both).unwrap_or(0)
    }

    /// Whether a context flag was GIVEN at all. `-B0` still groups, so this
    /// cannot be decided from the counts being non-zero.
    fn has_context(&self) -> bool {
        self.after.is_some() || self.before.is_some() || self.both.is_some()
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
    /// Every pattern is empty, and there is one -- narrower than "every line
    /// matches", which `-e '' -e a` does too without GNU taking the short cut.
    /// Only `settled` reads this; matching compiles an empty pattern like any other.
    only_empty: bool,
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
    errb(msg.as_bytes());
}

/// `err` for a message carrying a RAW byte, which a `&str` cannot hold as one:
/// `char::from` widens it to a Unicode scalar and encodes it as UTF-8, so
/// `grep -\x80` would name its bad option with two bytes where GNU names it
/// with the one. No `cstr` here, unlike sed's: every name grep quotes comes from
/// argv, which is NUL-terminated, so none can carry the byte that would truncate.
fn errb(msg: &[u8]) {
    use std::io::Write;
    let mut out = b"grep: ".to_vec();
    out.extend_from_slice(msg);
    out.push(b'\n');
    let _ = std::io::stderr().write_all(&out);
}

/// Applet entry point. `args[0]` is the invoked name.
pub fn main(args: &[Vec<u8>]) -> i32 {
    let mut conf = Conf::default();
    let mut patterns: Vec<Vec<u8>> = Vec::new();
    let mut pattern_seen = false;
    // Which matcher was CHOSEN, as against the `Basic` that is merely the
    // default; only `choose_syntax` reads it.
    let mut syntax_chosen: Option<Syntax> = None;
    // Answered once the scan finishes without error, not where they are read.
    let mut show_help = false;
    let mut show_version = false;
    let mut files: Vec<Vec<u8>> = Vec::new();
    let mut operands: Vec<Vec<u8>> = Vec::new();
    let mut no_more_options = false;
    // grep gets only the option-order half: its ERE rules do not change with it.
    let posixly = posixly_correct();

    let mut i = 1usize;
    while let Some(arg) = args.get(i) {
        i += 1;
        if no_more_options || arg.first() != Some(&b'-') || arg.len() == 1 {
            operands.push(arg.clone());
            // Under POSIXLY_CORRECT the first operand ends options, so what
            // follows is a FILE -- `--` and a `-NUM` included.
            no_more_options |= posixly;
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
                    errb(&name_in("unrecognized option '", arg, "'"));
                    eprintln!("{USAGE}");
                    return 2;
                }
                Err(msg) => {
                    errb(&msg);
                    eprintln!("{USAGE}");
                    return 2;
                }
            };
            // Arity is GNU's: a value-taking option accepts `=VALUE` or the next
            // argv element; a flag accepts neither.
            let value = match (arity, inline) {
                (Arg::None, Some(_)) => {
                    errb(&name_in("option '--", name, "' doesn't allow an argument"));
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
                        errb(&name_in("option '--", name, "' requires an argument"));
                        eprintln!("{USAGE}");
                        return 2;
                    }
                },
            };
            // Neither answers here: GNU defers both to AFTER the scan, so an
            // error the scan itself raises outranks them whichever side of it
            // was written. Answered below the loop.
            if name == b"help" {
                show_help = true;
                continue;
            }
            if name == b"version" {
                show_version = true;
                continue;
            }
            match parse_long(
                &mut conf,
                name,
                value.as_deref(),
                &mut patterns,
                &mut pattern_seen,
                &mut syntax_chosen,
            ) {
                Ok(()) => continue,
                // `resolve_long` already rejected an unknown name.
                Err(LongErr::Unknown) => {
                    errb(&name_in("unrecognized option '", arg, "'"));
                    eprintln!("{USAGE}");
                    return 2;
                }
                Err(LongErr::Message(m)) => {
                    errb(&m);
                    return 2;
                }
                Err(LongErr::Handled) => return 2,
            }
        }
        // A short-option cluster; an option that takes a value consumes the rest
        // of the cluster, or the next argument when the cluster ends.
        let mut j = 1usize;
        while let Some(opt) = arg.get(j).copied() {
            j += 1;
            // `-NUM` is GNU's fourth spelling of a context count, and it is an
            // ordinary cluster element: the RUN of digits is the count, and
            // parsing goes on after it (`-2n`, `-n2`, `-2A1` all work). It sets
            // what `-C` sets, so an explicit `-A`/`-B` still outranks it.
            if opt.is_ascii_digit() {
                let start = j - 1;
                while arg.get(j).is_some_and(u8::is_ascii_digit) {
                    j += 1;
                }
                // `start < j <= arg.len()`, so the run is always there.
                let digits = arg.get(start..j).unwrap_or_default();
                // This spelling caps the run at 21 SIGNIFICANT digits where the
                // `-A`/`-B`/`-C` argument does not, and echoes the digits it
                // took before giving up. Leading zeros are not significant.
                let significant = digits.iter().skip_while(|b| **b == b'0').count();
                if significant > NUM_DIGIT_CAP {
                    let kept: Vec<u8> = digits
                        .iter()
                        .skip_while(|b| **b == b'0')
                        .take(NUM_DIGIT_CAP)
                        .copied()
                        .collect();
                    errb(&name_in("", &kept, "...: invalid context length argument"));
                    return 2;
                }
                // Under the cap a run of digits always reads -- the shared
                // reader saturates rather than failing -- so this is a floor.
                let Some(n) = parse_count(digits) else {
                    errb(&name_in("", digits, ": invalid context length argument"));
                    return 2;
                };
                conf.both = Some(n);
                continue;
            }
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
                // One arm each rather than a catch-all mapping back to a
                // syntax: a fourth selector must then name what it chooses.
                b'E' => {
                    if !choose_syntax(&mut conf, &mut syntax_chosen, Syntax::Extended) {
                        return 2;
                    }
                }
                b'F' => {
                    if !choose_syntax(&mut conf, &mut syntax_chosen, Syntax::Fixed) {
                        return 2;
                    }
                }
                b'G' => {
                    if !choose_syntax(&mut conf, &mut syntax_chosen, Syntax::Basic) {
                        return 2;
                    }
                }
                b'i' | b'y' => conf.icase = true,
                b'v' => conf.invert = true,
                b'w' => conf.word = true,
                b'x' => conf.whole_line = true,
                b'c' => conf.count = true,
                b'l' => (conf.files_with, conf.files_without) = (true, false),
                b'L' => (conf.files_with, conf.files_without) = (false, true),
                b'Z' => conf.null_name = true,
                b'o' => conf.only = true,
                b'q' => conf.quiet = true,
                b's' => conf.no_messages = true,
                b'n' => conf.line_number = true,
                b'h' => conf.with_filename = Some(false),
                b'H' => conf.with_filename = Some(true),
                b'r' => conf.recursive = true,
                b'R' => {
                    conf.recursive = true;
                    conf.logical = true;
                }
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
                            errb(&name_in("", &v, &format!(": {}", errmsg(&e))));
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
                        errb(&byte_in("option requires an argument -- '", opt, "'"));
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
                        errb(&byte_in("option requires an argument -- '", opt, "'"));
                        return 2;
                    };
                    let Some(n) = parse_count(&v) else {
                        errb(&name_in("", &v, ": invalid context length argument"));
                        return 2;
                    };
                    apply_count(&mut conf, opt, n);
                }
                _ => {
                    errb(&byte_in("invalid option -- '", opt, "'"));
                    eprintln!("{USAGE}");
                    return 2;
                }
            }
        }
    }

    // The scan finished clean, so `--help`/`--version` are answered here --
    // before the pattern and operands are looked at, which is why a bad PATTERN
    // or a missing FILE loses to them where a bad `-m` (read above) wins.
    // `--version` outranks `--help` whichever order they came in. The text is
    // td-txt's own: reporting a GNU banner would be a lie a caller could act on.
    if show_version || show_help {
        let line = if show_version {
            format!("grep (td-txt) {VERSION}")
        } else {
            USAGE.to_string()
        };
        return match print_line(&line) {
            Ok(()) => 0,
            Err(e) => {
                err(&format!("write error: {}", errmsg(&e)));
                2
            }
        };
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
    let only_empty = !patterns.is_empty() && patterns.iter().all(Vec::is_empty);

    // Recursion expands directories; a bare `grep -r pat` reads the working tree.
    // The flag is "the walk produced this", which is what keeps a FILE named `-`
    // found under the tree from being read as stdin -- GNU searches it.
    let mut inputs: Vec<(Vec<u8>, bool)> = Vec::new();
    let mut status_error = false;
    // Whether `-r` descended into a directory; with the operand count it decides
    // the NAME below. `walk` reports it rather than grep re-testing the operand.
    let mut descended = false;
    if conf.recursive {
        // A bare `grep -r PAT` walks the working tree but names its hits
        // `d/x`, not `./d/x` — the synthesized operand must not reach the
        // output, or a consumer of `grep -rl` sees paths GNU never prints.
        let implied_cwd = files.is_empty();
        if implied_cwd {
            files.push(b".".to_vec());
        }
        for f in &files {
            // `-` is stdin under `-r` too; there is nothing to walk and no
            // directory, so it neither descends nor names on its own.
            if f == b"-" {
                inputs.push((f.clone(), false));
                continue;
            }
            let found = walk(&crate::util::path_from_bytes(f), conf.logical);
            descended |= found.descended;
            // The synthesized `.` must not reach a DIAGNOSTIC either: GNU names the
            // same file the same way whether it is reporting it or matching in it.
            let shown = |p: &std::path::Path| -> Vec<u8> {
                let b = path_bytes(p);
                match implied_cwd {
                    true => b.strip_prefix(b"./".as_slice()).unwrap_or(&b).to_vec(),
                    false => b,
                }
            };
            inputs.extend(found.files.iter().map(|p| (shown(p), true)));
            for (path, diag) in &found.diags {
                let text = match diag {
                    // A cycle is a WARNING in GNU: reported, walked around, and the
                    // exit status left to whatever the search itself concluded.
                    Diag::Loop => "warning: recursive directory loop".to_string(),
                    Diag::Failed(e) => {
                        status_error = true;
                        errmsg(e)
                    }
                };
                if !conf.no_messages {
                    errb(&name_in("", &shown(path), &format!(": {text}")));
                }
            }
        }
    } else if files.is_empty() {
        inputs.push((b"-".to_vec(), false));
    } else {
        inputs = files.iter().map(|f| (f.clone(), false)).collect();
    }

    // GNU decides the name from the OPERANDS, not from how many files the walk
    // turned them into.
    let show_name = conf.with_filename.unwrap_or(files.len() > 1 || descended);
    let grep = Grep { conf, pats, only_empty };
    let mut out = Out::new();
    let mut any_match = false;
    let mut printed_groups = false;

    // Nothing to look for and no `-v` to invert it: GNU never OPENS the
    // operands, so a nonexistent one is not an error either. `-L` is the
    // exception — it must still report the file, so it still stats it.
    let settled = grep.settled() && !grep.conf.files_without;
    for (path, from_walk) in &inputs {
        // Named before the read, because a failure names it too: GNU reports
        // `(standard input): Is a directory`, not `-: Is a directory`.
        let display: &[u8] = match path.as_slice() == b"-" && !from_walk {
            true => b"(standard input)",
            false => path,
        };
        let data = if settled {
            Vec::new()
        } else {
            match read_search(path, *from_walk) {
                Ok(d) => d,
                Err(f) => {
                    if !grep.conf.no_messages {
                        errb(&name_in("", display, &format!(": {}", errmsg(&f.err))));
                    }
                    status_error = true;
                    if !f.opened {
                        continue;
                    }
                    // Opened and unreadable: GNU has processed the file, and found
                    // in it whatever the read managed to return -- nothing, for a
                    // directory, which is what makes `-c` print its zero.
                    f.partial
                }
            }
        };
        match search_file(&grep, &mut out, display, &data, show_name, &mut printed_groups) {
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
    Message(Vec<u8>),
    /// Already reported by the handler; the caller only supplies the status.
    Handled,
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

fn resolve_long(name: &[u8]) -> Result<(&'static [u8], Arg), Vec<u8>> {
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
        [] => Err(Vec::new()),
        many => {
            let mut msg = name_in("option '--", name, "' is ambiguous; possibilities:");
            for (n, _) in many {
                msg.extend_from_slice(b" '--");
                msg.extend_from_slice(n);
                msg.push(b'\'');
            }
            Err(msg)
        }
    }
}

fn parse_long(
    conf: &mut Conf,
    name: &[u8],
    value: Option<&[u8]>,
    patterns: &mut Vec<Vec<u8>>,
    pattern_seen: &mut bool,
    syntax_chosen: &mut Option<Syntax>,
) -> Result<(), LongErr> {
    let need = |value: Option<&[u8]>| -> Result<Vec<u8>, LongErr> {
        value
            .map(<[u8]>::to_vec)
            .ok_or_else(|| LongErr::Message(name_in("option '--", name, "' requires an argument")))
    };
    let count = |value: Option<&[u8]>| -> Result<usize, LongErr> {
        let v = need(value)?;
        parse_count(&v).ok_or_else(|| LongErr::Message(name_in("", &v, ": invalid context length argument")))
    };
    match name {
        b"extended-regexp" => {
            if !choose_syntax(conf, syntax_chosen, Syntax::Extended) {
                return Err(LongErr::Handled);
            }
        }
        b"fixed-strings" => {
            if !choose_syntax(conf, syntax_chosen, Syntax::Fixed) {
                return Err(LongErr::Handled);
            }
        }
        b"basic-regexp" => {
            if !choose_syntax(conf, syntax_chosen, Syntax::Basic) {
                return Err(LongErr::Handled);
            }
        }
        b"ignore-case" => conf.icase = true,
        b"invert-match" => conf.invert = true,
        b"word-regexp" => conf.word = true,
        b"line-regexp" => conf.whole_line = true,
        b"count" => conf.count = true,
        b"files-with-matches" => (conf.files_with, conf.files_without) = (true, false),
        b"files-without-match" => (conf.files_with, conf.files_without) = (false, true),
        b"null" => conf.null_name = true,
        b"only-matching" => conf.only = true,
        b"quiet" | b"silent" => conf.quiet = true,
        b"no-messages" => conf.no_messages = true,
        b"line-number" => conf.line_number = true,
        b"with-filename" => conf.with_filename = Some(true),
        b"no-filename" => conf.with_filename = Some(false),
        b"recursive" => conf.recursive = true,
        b"dereference-recursive" => {
            conf.recursive = true;
            conf.logical = true;
        }
        b"null-data" => conf.null_data = true,
        b"text" => conf.text = true,
        b"regexp" => {
            push_expr(patterns, &need(value)?);
            *pattern_seen = true;
        }
        b"file" => {
            let path = need(value)?;
            let bytes = read_input(&path)
                .map_err(|e| LongErr::Message(name_in("", &path, &format!(": {}", errmsg(&e)))))?;
            push_file(patterns, &bytes);
            *pattern_seen = true;
        }
        b"max-count" => {
            let v = need(value)?;
            conf.max_count = parse_max_count(&v)
                .ok_or_else(|| LongErr::Message(b"invalid max count".to_vec()))?;
        }
        b"after-context" => conf.after = Some(count(value)?),
        b"before-context" => conf.before = Some(count(value)?),
        b"context" => {
            let n = count(value)?;
            conf.both = Some(n);
        }
        _ => return Err(LongErr::Unknown),
    }
    Ok(())
}

/// GNU refuses two DIFFERENT matchers rather than keeping the last as `-l`/`-L`
/// do. Tested against what was CHOSEN, so `-EE` is fine and the defaulted
/// `Basic` is not a choice.
fn choose_syntax(conf: &mut Conf, chosen: &mut Option<Syntax>, want: Syntax) -> bool {
    if chosen.is_some_and(|had| had != want) {
        err("conflicting matchers specified");
        return false;
    }
    *chosen = Some(want);
    conf.syntax = want;
    true
}

fn apply_count(conf: &mut Conf, opt: u8, n: usize) {
    match opt {
        b'A' => conf.after = Some(n),
        b'B' => conf.before = Some(n),
        _ => {
            conf.both = Some(n);
        }
    }
}

/// A numeric option argument as GNU's reader takes one: leading whitespace, an
/// optional SIGN, decimal digits (leading zeros and all), and nothing after
/// them. What a negative MEANS is the caller's question -- `-m` reads one as no
/// limit where a context count refuses it -- so the sign is returned rather
/// than judged here.
struct Num {
    negative: bool,
    value: u64,
}

/// A value past what fits SATURATES rather than failing. GNU's reader does, and
/// any count past the input's length already means what the largest one means.
fn parse_num(v: &[u8]) -> Option<Num> {
    let mut i = 0usize;
    while matches!(v.get(i), Some(b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')) {
        i += 1;
    }
    let negative = match v.get(i) {
        Some(b'-') => {
            i += 1;
            true
        }
        Some(b'+') => {
            i += 1;
            false
        }
        _ => false,
    };
    let digits = v.get(i..)?;
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let mut value: u64 = 0;
    for b in digits {
        // The guard above makes every byte a digit, so the subtraction cannot
        // wrap; it saturates rather than `-` only because a bare `-` would
        // panic in debug if that guard were ever widened.
        value = value.saturating_mul(10).saturating_add(u64::from(b.saturating_sub(b'0')));
    }
    Some(Num { negative, value })
}

/// `-m`'s argument. A NEGATIVE count is no limit at all -- but `-0` is a zero
/// like any other, since only the VALUE being negative makes it unbounded.
fn parse_max_count(v: &[u8]) -> Option<Option<u64>> {
    let n = parse_num(v)?;
    if n.negative && n.value != 0 {
        return Some(None);
    }
    Some(Some(n.value))
}

/// A context count. Negative is refused, `-0` again excepted.
fn parse_count(v: &[u8]) -> Option<usize> {
    let n = parse_num(v)?;
    if n.negative && n.value != 0 {
        return None;
    }
    // Saturating, not `try_from(..).ok()?`: a count past `usize` is still a
    // count, and td-txt counts rather than allocating for it.
    Some(usize::try_from(n.value).unwrap_or(usize::MAX))
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
    // GNU drops a duplicate pattern, keeping the first. Only `lex_continues` can
    // see it, so this must precede the loop that decides which pattern ends the lex.
    let mut seen = std::collections::HashSet::with_capacity(lines.len());
    let lines: Vec<&Vec<u8>> = lines.iter().filter(|l| seen.insert(l.as_slice())).collect();
    if conf.syntax == Syntax::Fixed {
        return Ok(Patterns::Fixed(lines.into_iter().cloned().collect()));
    }
    let opts = Options {
        ere: conf.syntax == Syntax::Extended,
        icase: conf.icase,
        // grep warns and compiles where sed refuses: `grep -E '*a'` is `warning: *
        // at start of expression` and a working pattern, which spencer1 pins.
        strict_repeats: false,
        // grep has no `--posix`; the one rule that reads it is sed's.
        posix: false,
        // grep matches with its own dfa, which satisfies a mid-branch `$`. It falls
        // back to glibc for `-o` and for any pattern with a backreference, where GNU
        // then disagrees with itself; td-txt keeps the dfa reading throughout.
        glibc_engine: false,
        // Set per line below: GNU lexes the patterns JOINED, and `-x`/`-w` wrap
        // what it lexes, so a pattern ends the lex only when neither applies.
        lex_continues: false,
        // GNU grep never sets REG_NEWLINE, not even under `-z`, where a record can
        // hold newlines: `grep -z 'a.c'` matches across one and `-z '^c'` does not
        // anchor after one. sed's `M` is the only way into that rule.
        reg_newline: None,
    };
    let mut res = Vec::with_capacity(lines.len());
    let wrapped = conf.whole_line || conf.word;
    for (i, line) in lines.iter().enumerate() {
        let opts = Options { lex_continues: wrapped || i + 1 < lines.len(), ..opts };
        res.push(Regex::compile(line, opts).map_err(|e| e.msg)?);
    }
    Ok(Patterns::Regex(res))
}

/// Emit `file:` / `line:` prefixes. `sep` is `:` on a selected line and `-` on a
/// context line, as GNU prints them.
/// `lineno` is `None` where the output has no line to name -- a `-c` count line
/// takes the file name under `-n` but never a number, since the count is of the
/// whole file.
fn prefix(
    out: &mut Out,
    grep: &Grep,
    path: &[u8],
    show_name: bool,
    lineno: Option<u64>,
    sep: u8,
) -> std::io::Result<()> {
    if show_name {
        out.write(path)?;
        // -Z replaces only the byte after the NAME; a following line number
        // still carries the ordinary separator.
        out.write(&[if grep.conf.null_name { 0 } else { sep }])?;
    }
    if grep.conf.line_number {
        if let Some(n) = lineno {
            out.write(&number(n))?;
            out.write(&[sep])?;
        }
    }
    Ok(())
}

/// `-o` reduces a line to its matched spans. `sep` is the prefix byte, `:` on a
/// selected line and `-` on a context one; WHICH lines get here is the caller's
/// decision, and it is not "the ones that match" -- see `prints_spans`.
fn write_spans(
    grep: &Grep,
    out: &mut Out,
    display: &[u8],
    show_name: bool,
    lineno: u64,
    sep: u8,
    line: &[u8],
) -> Result<(), String> {
    let io = |r: std::io::Result<()>| -> Result<(), String> {
        r.map_err(|e| format!("write error: {}", errmsg(&e)))
    };
    let data_sep = if grep.conf.null_data { 0u8 } else { b'\n' };
    let mut at = 0usize;
    while let Some((s, e)) = grep.match_at(line, at, OnBudget::Fail)? {
        if e == s {
            at = s + 1;
            if at > line.len() {
                break;
            }
            continue;
        }
        io(prefix(out, grep, display, show_name, Some(lineno), sep))?;
        io(out.write(line.get(s..e).unwrap_or_default()))?;
        io(out.write(&[data_sep]))?;
        at = e;
    }
    Ok(())
}

fn search_file(
    grep: &Grep,
    out: &mut Out,
    // The NAME to print, already resolved: only an operand spells stdin `-`, so
    // the caller is the one that knows whether this came from the walk.
    display: &[u8],
    data: &[u8],
    show_name: bool,
    // Whether an earlier FILE already printed a context group. `printed_upto`
    // resets per file, so without this the `--` at a file boundary is lost.
    printed_before: &mut bool,
) -> Result<bool, String> {
    let sep = if grep.conf.null_data { 0u8 } else { b'\n' };
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
    // How far the output reaches, 1-based: the last line printed OR covered by
    // context. `-o` drops context lines but not the range they cover.
    let mut covered: usize = 0;
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
                errb(&name_in("", display, ": binary file matches"));
                // The notice goes to stderr, but GNU still counts the file as
                // having produced output, so the NEXT file opens with `--`.
                *printed_before = true;
                return Ok(true);
            }
            if !grep.conf.count {
                // Trailing context of the previous group, then leading context.
                let start = idx.saturating_sub(grep.conf.before_lines());
                if grep.conf.has_context() {
                    let gap = match covered {
                        0 => *printed_before,
                        upto => start > upto,
                    };
                    if gap {
                        io(out.write(b"--\n"))?;
                    }
                    *printed_before = true;
                    let ctx_start = start.max(covered);
                    for (k, (ctx, ctx_term)) in lines.iter().enumerate().take(idx).skip(ctx_start) {
                        if grep.conf.only {
                            if grep.conf.prints_context_spans() {
                                write_spans(grep, out, display, show_name, (k + 1) as u64, b'-', ctx)?;
                            }
                            continue;
                        }
                        io(prefix(out, grep, display, show_name, Some((k + 1) as u64), b'-'))?;
                        io(out.write(ctx))?;
                        if *ctx_term || k + 1 < lines.len() {
                            io(out.write(&[sep]))?;
                        }
                    }
                }
                if grep.conf.only {
                    if grep.conf.prints_selected_spans() {
                        write_spans(grep, out, display, show_name, lineno, b':', line)?;
                    }
                } else {
                    io(prefix(out, grep, display, show_name, Some(lineno), b':'))?;
                    io(out.write(line))?;
                    // GNU terminates every line it prints, including a final
                    // input line that carried no newline of its own.
                    io(out.write(&[sep]))?;
                }
                covered = idx + 1;
                pending_after = grep.conf.after_lines();
            }
            if grep.conf.max_count.is_some_and(|m| count >= m) {
                // GNU still prints the trailing context of that last match,
                // `-o` included: what `-o` changes is which lines print, not
                // how far the drain runs.
                if pending_after == 0 {
                    break;
                }
                limit_reached = true;
            }
        } else if pending_after > 0 && !grep.conf.count {
            if grep.conf.only {
                if grep.conf.prints_context_spans() {
                    write_spans(grep, out, display, show_name, lineno, b'-', line)?;
                }
            } else {
                io(prefix(out, grep, display, show_name, Some(lineno), b'-'))?;
                io(out.write(line))?;
                io(out.write(&[sep]))?;
            }
            covered = idx + 1;
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
    // `-l`/`-L` outrank `-c` the same way: GNU goes quiet for everything but the
    // NAME, so `grep -cl a f` prints `f` and not the count before it.
    if grep.conf.count && !grep.conf.files_with && !grep.conf.files_without {
        io(prefix(out, grep, display, show_name, None, b':'))?;
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
        let only_empty = !patterns.is_empty() && patterns.iter().all(Vec::is_empty);
        Grep { conf, pats, only_empty }
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

    /// The whole numeric grammar, byte by byte. Only LF is impossible in a
    /// corpus `## argv:` line -- it is ONE line -- but a raw VT/FF/CR there is
    /// an invisible byte in a file people read and edit, so all six live here.
    #[test]
    fn parse_num_takes_every_c_whitespace_byte_before_the_digits() {
        for ws in [b' ', b'\t', b'\n', 0x0b, 0x0c, b'\r'] {
            let arg = [ws, b'1'];
            assert_eq!(parse_count(&arg), Some(1), "leading {ws:#04x}");
            let signed = [ws, b'+', b'1'];
            assert_eq!(parse_count(&signed), Some(1), "leading {ws:#04x} then +");
            // Trailing is not leading: the same byte after the digits is fatal.
            let trailing = [b'1', ws];
            assert_eq!(parse_count(&trailing), None, "trailing {ws:#04x}");
        }
    }

    #[test]
    fn parse_num_saturates_rather_than_failing() {
        assert_eq!(parse_count(b"18446744073709551616"), Some(usize::MAX));
        assert_eq!(parse_count(b"99999999999999999999999999"), Some(usize::MAX));
        assert_eq!(parse_max_count(b"18446744073709551616"), Some(Some(u64::MAX)));
    }

    #[test]
    fn a_negative_means_what_the_option_says_it_means() {
        // A context count refuses one; `-m` reads it as no limit at all.
        assert_eq!(parse_count(b"-1"), None);
        assert_eq!(parse_max_count(b"-1"), Some(None));
        // `-0` is the negative whose VALUE is not, so both take it as a zero.
        assert_eq!(parse_count(b"-0"), Some(0));
        assert_eq!(parse_count(b"-00"), Some(0));
        assert_eq!(parse_max_count(b"-0"), Some(Some(0)));
    }

    #[test]
    fn parse_num_refuses_what_is_not_a_plain_decimal() {
        for bad in [&b"++1"[..], b"+-1", b"- 1", b"0x10", b"1e2", b"1_0", b"+", b"-", b".", b" "] {
            assert_eq!(parse_count(bad), None, "{bad:?}");
        }
        // Leading zeros are decimal, not octal.
        assert_eq!(parse_count(b"010"), Some(10));
    }
}

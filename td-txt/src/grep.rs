//! `grep` — select lines matching a pattern.
//!
//! POSIX `grep` plus the GNU options td's own scripts and the busybox applet it
//! replaces actually use. Everything is byte-oriented (C locale, see `regex`);
//! inputs are STREAMED a buffer at a time, so an operand costs a buffer rather
//! than its own size -- except under `-B N`, which retains N whole records by
//! definition -- and an endless one can still be answered. A record that spans
//! a buffer is assembled whole before it is matched, so a pattern never
//! straddles a read boundary.
//!
//! Deliberate omissions, by what they would cost to serve:
//!
//! * a second regex engine — `-P`/`--perl-regexp`, and `-X`, whose argument
//!   reaches the same one (`-X perl` IS `-P`);
//! * output this one does not produce — `--color`/`--colour`, whose escapes
//!   would need a second output path and a terminal test, and
//!   `-T`/`--initial-tab`;
//! * a file filter — `--include`, `--exclude`, `--exclude-from` and
//!   `--exclude-dir`, and `-I`, which is `--binary-files=without-match`: the
//!   one value of that option not served, the other two being the default and
//!   `-a`'s;
//! * nothing at all — `-u` and `-U`/`--binary`, whose second answer a POSIX
//!   system does not have.
//!
//! Each is DIAGNOSED and never a silent no-op: `invalid option` for the short
//! spellings, `unrecognized option` for the long ones — getopt's own split and
//! GNU's wording for both — except where a name must be in the TABLE to be
//! refused, which says `unsupported` instead. What decides whether a name
//! needs a row is the CONSEQUENCE of leaving it out, and that has to be
//! measured against THIS table rather than reasoned from which name is a
//! prefix of which — two drafts of this paragraph got it wrong in opposite
//! directions, one sorting by prefix geometry and one crediting a single row
//! with an effect that needed its whole group gone.
//!
//! Remove ONE row and only `--binary` changes what runs: it resolved to
//! `--binary-files` and took the next argv as its value, so a refusal became
//! a successful search. Every other single removal still ends in an error,
//! because the siblings that remain keep the abbreviation ambiguous — which
//! is exactly why these names arrived in GROUPS, and the group is the unit
//! the effect belongs to. Drop all three `--exclude*` and `--e` resolves to
//! `--extended-regexp`: `grep --e 'a+'` then matches `a` and `aa` as an ERE
//! where the refusal it replaced ran nothing. Drop both `--include` and
//! `--initial-tab` and `--in` resolves to `--invert-match`, printing the
//! NON-matching lines and exiting 0.
//!
//! For the rest, absence changes only the DIAGNOSTIC — `--exclude-from` and
//! `--exclude-dir` report `unrecognized` against the whole argument instead
//! of `unsupported` against the option, `--initial-tab` and `--perl-regexp`
//! likewise — because a name absent from the table is absent from every
//! ambiguity list and from every abbreviation that would reach it. Each named
//! above is pinned in spec/divergence.test.txt, except `--i`/`--in`, which
//! stopped diverging when `--initial-tab` landed and moved to
//! spec/grep-cli.test.txt. BOTH rosters are complete now — the short one
//! swept against grep.c:486, the long one against grep.c:504 — with ONE
//! name deliberately left out: `--unix-byte-offsets`. It is the only GNU
//! long option absent from the table, and it is absent because it is the
//! only one whose divergence is not a diagnostic. GNU answers it with
//! `warning: --unix-byte-offsets (-u) is obsolete` and then MATCHES, exit 0;
//! tabling it would change `unrecognized` to `unsupported` and leave the
//! status difference exactly where it is. `-u` carries the case.

use crate::regex::{Error, Filter, OnBudget, Options, Regex};
use crate::util::{
    byte_in, errmsg, name_in, number, path_bytes, posixly_correct, print_line, read_input,
    open_search, records, walk, DeviceRule, Diag, Input, Out, Records, VERSION,
};

/// How many significant digits `-NUM` takes before refusing the run. GNU's own
/// scanner stops there; the `-A`/`-B`/`-C` argument has no such limit.
const NUM_DIGIT_CAP: usize = 21;

const USAGE: &str = "usage: grep [-abEFGHhicLlnoqRrsVvwxyzZ] [-NUM] [-d ACTION] [-D ACTION] \
                     [-m NUM] [-A NUM] [-B NUM] [-C NUM] [-e PATTERN] [-f FILE] \
                     [PATTERN] [FILE]...";

/// `-d`/`--directories`, which is also where `-r` and `-R` land: GNU spells the
/// same setting three ways and the LAST one wins, so `grep -r -d skip a .` skips.
/// The `-R` deref is NOT part of it and is sticky -- `-R -d recurse` still
/// follows symlinks -- which is why `logical` stays a field of its own.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Dirs {
    /// Read a directory as a file, so the read fails with `Is a directory`.
    Read,
    /// Pass over it without a word.
    Skip,
    /// Descend into it, which is what `-r` and `-R` also ask for.
    Recurse,
}

/// `-D`/`--devices`, whose DEFAULT is neither of the two spellings it accepts: a
/// device named on the command line is read, one the walk found is skipped. GNU
/// calls that `READ_COMMAND_LINE_DEVICES`, and it is why `grep PAT fifo` waits
/// for a writer while `grep -r PAT dir` passes the same fifo by without a word.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Devices {
    ReadCommandLine,
    Read,
    Skip,
}

impl Devices {
    /// GNU's `skip_devices` (grep.c:618). `command_line` is false for a name the
    /// WALK produced, which is the only thing the default distinguishes.
    fn skips(self, command_line: bool) -> bool {
        self == Self::Skip || (self == Self::ReadCommandLine && !command_line)
    }
}

/// One thing to search. Three of its four fields would be the same bool in a
/// simpler program and are not here: `from_walk` decides what `-` MEANS and how
/// the input is named, `command_line` decides the device policy, and they part
/// company at the walk's ROOT -- which the walk produced and which is still an
/// operand. `device` is what the walk's stat already saw, so the policy need not
/// look again.
struct Operand {
    name: Vec<u8>,
    from_walk: bool,
    command_line: bool,
    device: bool,
}

impl Operand {
    /// An operand as WRITTEN, which is every input that did not come from a walk.
    /// Its type is not read here: an operand is opened before it is judged.
    fn named(name: Vec<u8>) -> Self {
        Self { name, from_walk: false, command_line: true, device: false }
    }
}

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
    byte_offset: bool,
    /// `None` until `-H`/`-h` or the file count decides it.
    with_filename: Option<bool>,
    dirs: Dirs,
    devices: Devices,
    /// `-R`: follow every symlink the walk finds, not only the operand. Sticky
    /// rather than part of `dirs`, since `-R -d recurse` still follows them.
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
            byte_offset: false,
            with_filename: None,
            dirs: Dirs::Read,
            devices: Devices::ReadCommandLine,
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
    /// Whether GNU would run its REGEX matcher for this pattern set — see
    /// `gnu_runs_regex_matcher`. Only the `-w -x` span reads it.
    regex_matcher: bool,
}

/// Which of GNU's matchers this run would end on (grep.c:2953-2972). It is not the
/// syntax the user asked for: a single-pattern `-F -w` run switches TO the regex
/// matcher because `-G` is typically faster for that, and a multi-pattern `-E`/`-G`
/// run whose patterns are all literal switches AWAY from it because `-F` is.
///
/// td-txt has no reason to care which is faster and does not switch. It cares
/// because the two matchers disagree about ONE observable: a `-w -x` span runs past
/// the record separator in `EGexecute` (dfasearch.c:512-519) and not in `Fexecute`
/// (kwsearch.c:152-156). So `grep -w -x -o -e ab` prints the extra byte and
/// `-e ab -e cd` prints it for neither line.
fn gnu_runs_regex_matcher(conf: &Conf, pats: &[&Vec<u8>]) -> bool {
    match conf.syntax == Syntax::Fixed {
        // `-F` -> `-G` for a single word-matching pattern in a unibyte locale.
        // `separator_in_span` is the only caller and asks only under `-w`.
        true => pats.len() == 1,
        // `-E`/`-G` -> `-F` when two or more patterns are all literal.
        false => pats.len() <= 1 || !all_literal(conf.syntax == Syntax::Extended, pats),
    }
}

/// GNU's `try_fgrep_pattern` (grep.c:2389-2456): can this pattern set be handed to
/// the fixed-string matcher? Read over the patterns joined by newlines, as GNU
/// reads them, so a `\` before the join is the `\<newline>` that refuses.
fn all_literal(ere: bool, pats: &[&Vec<u8>]) -> bool {
    let joined = pats.iter().map(|p| p.as_slice()).collect::<Vec<_>>().join(&b'\n');
    let mut i = 0;
    while let Some(&c) = joined.get(i) {
        match c {
            b'$' | b'*' | b'.' | b'[' | b'^' => return false,
            // Literal in a BRE, operators in an ERE.
            b'(' | b'+' | b'?' | b'{' | b'|' if ere => return false,
            b'\\' => {
                match joined.get(i + 1) {
                    // An operator or an assertion in both dialects.
                    Some(
                        b'\n' | b'B' | b'S' | b'W' | b'\'' | b'<' | b'b' | b's' | b'w' | b'`'
                        | b'>' | b'1'..=b'9',
                    ) => return false,
                    // A BRE's operators, where an ERE reads the escape as making
                    // them literal. `\)` rides with them so GEAcompile can
                    // complain about it rather than this deciding.
                    Some(b'(' | b'+' | b'?' | b'{' | b'|' | b')') if !ere => return false,
                    // Any other escape drops out, leaving the byte literal.
                    Some(_) => i += 2,
                    None => i += 1,
                }
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    true
}

fn is_word(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// GNU's `start_ptr`: whether the caller needs the exact SPAN or only whether the
/// line matches at all. Under `-w` these are not one question at two costs but two
/// questions with two ANSWERS, reached through different code in GNU — a line it
/// selects can have no span it will print, which `grep -o -w '\.*'` over `.a` shows:
/// status 0, nothing on stdout.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Want {
    /// Does the line match? GNU asks its dfa, built from the pattern WRAPPED in word
    /// boundaries — `\(^\|[^[:alnum:]_]\)\(PAT\)\([^[:alnum:]_]\|$\)`
    /// (grep/src/dfasearch.c:301-325) — and a hit with no backreference IS the answer
    /// (dfasearch.c:479). So this asks "is there ANY word-bounded span", which is
    /// what the filtered scan asks.
    Selection,
    /// Which span? GNU's retry loop — see `Grep::word_match`.
    Span,
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
        want: Want,
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
                            // `-F` reaches the same `match_lines` arm, so a `-w -x`
                            // span runs past the record separator here too.
                            let e = match self.conf.whole_line {
                                true => e + self.separator_in_span(want),
                                false => e,
                            };
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
                                // `-w` AND `-x` together is where the span reaches
                                // PAST the line. GNU's `start_ptr` shortcut is guarded
                                // by `!match_words` (dfasearch.c:512), so with both
                                // flags it falls through to the `match_lines` arm,
                                // whose length is `end - ptr` — measured past the
                                // record separator, which `-o` then prints
                                // (dfasearch.c:514-519).
                                true => Some((0, line.len() + self.separator_in_span(want))),
                                false => None,
                            }),
                            false => Ok(None),
                        }
                    } else if self.conf.word && want == Want::Span {
                        self.word_match(re, line, from)
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

    /// GNU's `-w` retry loop (grep/src/dfasearch.c:528-567), which is what decides
    /// the SPAN once the line is already selected. It is not a search for any
    /// word-bounded span — that is `Want::Selection`'s question, and the two
    /// disagree: `grep -o -w '..\?'` prints `a` and `c` of `a b c`, passing the
    /// perfectly word-bounded `b` that selection would have found.
    ///
    /// Given a match that is not word-bounded, GNU tries a SHORTER one anchored at
    /// the same start, and only when there is none does it advance the start by one
    /// and search again. Two details of that decide the divergences:
    ///
    /// The shrink window is cut at `s + len - from`, an offset measured from the
    /// SEARCH START but applied to a buffer beginning at the LINE start. Under `-o`
    /// the two part company as the line is consumed, so the window closes over the
    /// match and shrinking stops working once `len <= from`. That is why `b` above
    /// is skipped: at its start the greedy `b ` is not word-bounded, and the `b`
    /// that would be is past the window.
    ///
    /// And an EMPTY shrink does not count (`0 < shorter_len`), where an empty match
    /// `re_search` itself reports does.
    ///
    /// A pattern LIST is where this is still not GNU. Without a backreference
    /// GEAcompile hands the newline-joined list to glibc as ONE regex
    /// (dfasearch.c:236-295), so GNU runs this loop once over the union while
    /// td-txt runs it per pattern and takes the leftmost-longest of the answers.
    /// A start whose union span cannot be shrunk into a word can still be
    /// word-bounded for one pattern alone, so the two differ — see the xfail
    /// block in spec/grep-cli.test.txt. Selection is unaffected, the wrapped dfa
    /// over the union asking what the per-pattern scan asks.
    fn word_match(
        &self,
        re: &Regex,
        line: &[u8],
        from: usize,
    ) -> Result<Option<(usize, usize)>, Error> {
        // ONE budget for the whole slide, not one per search — see
        // `Regex::search_budgeted`.
        // ONE budget for the whole slide, not one per search — see
        // `Regex::search_budgeted`.
        let steps = &mut 0u64;
        // Always leftmost-then-LONGEST, even where the caller would have settled for
        // a boolean: the shrink walks DOWN from the longest span, so entering the
        // loop with a shorter one would start it somewhere GNU never does.
        let Some(caps) = re.search_budgeted(line, from, steps)? else {
            return Ok(None);
        };
        let (mut s, mut e) = (caps.start(), caps.end());
        loop {
            if word_start_ok(line, s) && word_end_ok(line, e) {
                return Ok(Some((s, e)));
            }
            // GNU's `len > 0`, and its `--len` BEFORE the call: the window ends one
            // byte short of this match, so what comes back is strictly shorter. The
            // width saturates rather than relying on that guard for its own sake —
            // `e - 1` under it is only non-negative because `e > s`.
            let shorter = match e > s {
                true => {
                    let width = e.saturating_sub(1).saturating_sub(from);
                    re.match_anchored(line, width, s, steps)?
                }
                false => None,
            };
            match shorter {
                // `len > 0` is GNU's. The second half is this crate's: a shrink
                // that did not SHORTEN is not one, and without the test the loop
                // spins on a span it keeps rediscovering. It cannot fire, the
                // window being cut strictly inside the match — which is the point,
                // since that makes termination structural rather than a property
                // of the arithmetic above it.
                Some(len) if len > 0 && s + len < e => e = s + len,
                _ => {
                    if s == line.len() {
                        return Ok(None);
                    }
                    let Some(caps) = re.search_budgeted(line, s + 1, steps)? else {
                        return Ok(None);
                    };
                    (s, e) = (caps.start(), caps.end());
                }
            }
        }
    }

    /// Whether an `-x` span runs one byte past the line, which is `1` only for the
    /// `-w`-and-`-x` span above.
    fn separator_in_span(&self, want: Want) -> usize {
        usize::from(self.conf.word && want == Want::Span && self.regex_matcher)
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
        let hit = self.match_at(line, 0, OnBudget::Existence, Want::Selection)?.is_some();
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
            let (name, arity) = match resolve_long(&name, arg) {
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
            // Arity is GNU's, and its THREE cases differ in what they accept
            // beyond `=VALUE`: a required argument also takes the next argv
            // element, an OPTIONAL one never does (so `--color always` leaves
            // `always` an operand), and a flag takes neither.
            let value = match (arity, inline) {
                (Arg::None, Some(_)) => {
                    errb(&name_in("option '--", name, "' doesn't allow an argument"));
                    eprintln!("{USAGE}");
                    return 2;
                }
                (Arg::None, None) => None,
                (Arg::Optional, v) => v,
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
                Err(LongErr::HandledWith(code)) => return code,
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
                b'b' => conf.byte_offset = true,
                b'h' => conf.with_filename = Some(false),
                b'H' => conf.with_filename = Some(true),
                // The flag, not an answer: the cluster must keep being read.
                b'V' => show_version = true,
                b'd' => {
                    let Some(v) = value_of(&mut j, &mut i) else {
                        errb(&byte_in("option requires an argument -- '", opt, "'"));
                        eprintln!("{USAGE}");
                        return 2;
                    };
                    let Some(action) = dirs_arg(&v) else { return 1 };
                    conf.dirs = action;
                }
                b'D' => {
                    let Some(v) = value_of(&mut j, &mut i) else {
                        errb(&byte_in("option requires an argument -- '", opt, "'"));
                        eprintln!("{USAGE}");
                        return 2;
                    };
                    let Some(action) = devices_arg(&v) else {
                        err("unknown devices method");
                        return 2;
                    };
                    conf.devices = action;
                }
                b'r' => conf.dirs = Dirs::Recurse,
                b'R' => {
                    conf.dirs = Dirs::Recurse;
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

    // `-R` flips the device DEFAULT (grep.c:3007): following symlinks is a claim
    // that what a link points AT should be read, and the default's whole content
    // is that the walk skips what it finds -- so `-R PAT dir` reads a fifo that
    // `-r PAT dir` passes by. Tested against the default alone, so an explicit
    // `-D skip` still wins however the two were ordered.
    if conf.logical && conf.devices == Devices::ReadCommandLine {
        conf.devices = Devices::Read;
    }

    // Recursion expands directories; a bare `grep -r pat` reads the working tree.
    let mut inputs: Vec<Operand> = Vec::new();
    let mut status_error = false;
    // Whether `-r` descended into a directory; with the operand count it decides
    // the NAME below. `walk` reports it rather than grep re-testing the operand.
    let mut descended = false;
    if conf.dirs == Dirs::Recurse {
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
                inputs.push(Operand::named(f.clone()));
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
            inputs.extend(found.files.iter().map(|f| Operand {
                name: shown(&f.path),
                from_walk: true,
                // The walk's ROOT is a command-line name to the device policy,
                // however far the walk below it goes -- GNU's FTS_ROOTLEVEL.
                command_line: f.root,
                device: f.device,
            }));
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
        inputs.push(Operand::named(b"-".to_vec()));
    } else {
        inputs = files.iter().cloned().map(Operand::named).collect();
    }

    // GNU decides the name from the OPERANDS, not from how many files the walk
    // turned them into.
    let show_name = conf.with_filename.unwrap_or(files.len() > 1 || descended);
    let regex_matcher = gnu_runs_regex_matcher(&conf, &deduped(&patterns));
    let grep = Grep { conf, pats, only_empty, regex_matcher };
    // Fallible because the sink DUPLICATES descriptor 1; grep's own error status.
    let mut out = match Out::new() {
        Ok(out) => out,
        Err(e) => {
            err(&format!("write error: {}", crate::util::errmsg(&e)));
            return 2;
        }
    };
    let mut any_match = false;
    let mut printed_groups = false;

    // Nothing to look for and no `-v` to invert it: GNU never OPENS the
    // operands, so a nonexistent one is not an error either. `-L` is the
    // exception — it must still report the file, so it still stats it.
    let settled = grep.settled() && !grep.conf.files_without;
    // Asked ONCE, of the pattern, as GNU asks it before it opens anything: may
    // an empty record be selected? Where it cannot, dropping a read that holds
    // only empty records changes no answer. A pattern that fails to answer is
    // taken as selecting, which only costs speed.
    let skip_zero_fills = !grep.selects(b"").unwrap_or(true);
    for op in &inputs {
        let (path, from_walk) = (&op.name, op.from_walk);
        // Named before the read, because a failure names it too: GNU reports
        // `(standard input): Is a directory`, not `-: Is a directory`.
        let display: &[u8] = match path.as_slice() == b"-" && !from_walk {
            true => b"(standard input)",
            false => path,
        };
        // Nothing to look for: GNU never OPENS these, and `search_file`'s own
        // settled branch would print nothing for a case that got this far
        // (`-L` clears `settled` here precisely so its name still gets said).
        if settled {
            continue;
        }
        // The default reads a device named as an operand and skips the same one
        // found underfoot, so the policy is asked about THIS name's provenance;
        // where it says skip, provenance decides again which side of the open
        // the question falls.
        let rule = match grep.conf.devices.skips(op.command_line) {
            false => DeviceRule::Read,
            true if op.command_line => DeviceRule::Descriptor,
            true => DeviceRule::Walked(op.device),
        };
        let input =
            match open_search(path, from_walk, grep.conf.dirs == Dirs::Skip, rule) {
            Ok(Some(input)) => input,
            // `-d skip` passes over a directory WITHOUT A WORD: no diagnostic
            // and no effect on the status, which is what distinguishes it from
            // the `read` default whose message is a read failure. The OPEN
            // still happened, so one that failed is reported below rather than
            // swallowed.
            Ok(None) => continue,
            Err(f) => {
                if !grep.conf.no_messages {
                    errb(&name_in("", display, &format!(": {}", errmsg(&f))));
                }
                status_error = true;
                continue;
            }
        };
        // A READ that fails part way is reported inside, because with a
        // streaming reader it can happen after output has already gone out --
        // a directory under `-d read` being the reachable case, which is what
        // makes `-c` print its zero.
        let sep = if grep.conf.null_data { 0u8 } else { b'\n' };
        let mut rec = Records::new(input, sep);
        // `-a` is the one switch that turns the binary verdict off, and the
        // zapping goes with it. Not gated on the counting modes: GNU suppresses
        // OUTPUT for a binary file only when it is printing lines, but it zaps
        // whatever it is doing, which is why `-c` sees the difference too.
        rec.zap_nuls(!grep.conf.text);
        // The other half: a read that is nothing but zeros is dropped rather
        // than split into one empty record per byte. Sound only where an empty
        // record cannot be SELECTED, which is a question about the pattern and
        // so is asked once, before any operand -- GNU asks it once too.
        rec.skip_zero_fills(skip_zero_fills);
        let mut read_failed = false;
        let hit = search_file(
            &grep,
            &mut out,
            display,
            &mut rec,
            show_name,
            &mut printed_groups,
            &mut read_failed,
        );
        status_error |= read_failed;
        match hit {
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
    /// Already reported, AND carrying its own status: GNU's argmatch failures
    /// exit 1 where every other usage error here exits 2.
    HandledWith(i32),
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

/// A long option's argument policy, so `--max-count 1` works and `--count=1`
/// is refused the way GNU's `getopt_long` refuses it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Arg {
    None,
    Required,
    /// GNU's `optional_argument`, which `--color` is grep's only user of. It
    /// takes `=VALUE` and NEVER the next argv element, so `grep --color always
    /// f` searches for `always` -- measured against GNU 3.11, where a
    /// `Required` reading would have eaten the pattern.
    Optional,
}

/// Names GNU declares TWICE for one option. `getopt_long` drops a later prefix
/// match that matches the FIRST one in both option and arity (getopt.c:230),
/// which is a comparison against the first rather than a global dedup -- so
/// this is only the "same option" half, and the caller tests the arity.
///
/// Two of the four can never fire, and are here because what makes two names
/// one option is the option rather than the letters: collapsing needs a prefix
/// matching BOTH with one of them matched first, and the only string prefixing
/// `quiet` and `silent` -- or `group-separator` and `no-group-separator` -- is
/// the empty one, whose first match is `--basic-regexp`. Measured: removing
/// either pair changes no spelling, `--=` included. The separator pair has a
/// SECOND, independent reason, which is the arity half below: GNU gives them
/// the same `val` but different arities, so the caller would refuse the
/// collapse even if a shared prefix existed. It is the only pair for which
/// that test could ever be the deciding one.
const SYNONYMS: &[&[&[u8]]] = &[
    &[b"fixed-regexp", b"fixed-strings"],
    &[b"color", b"colour"],
    &[b"group-separator", b"no-group-separator"],
    &[b"quiet", b"silent"],
];

/// Whether two long names are the same option under two spellings.
fn synonymous(a: &[u8], b: &[u8]) -> bool {
    SYNONYMS.iter().any(|g| g.contains(&a) && g.contains(&b))
}

/// Every long option this applet knows, so an unambiguous PREFIX resolves the
/// way GNU's `getopt_long` accepts one (`grep --ignore-c`), an exact name
/// always winning over being a prefix of a longer one.
///
/// GNU's `long_options[]` (grep.c:504), order intact. A name is here to be
/// RESOLVED, which is not the same as being served: most of these are refused
/// by the dispatch. Every GNU long name is here but `--unix-byte-offsets`,
/// whose divergence is a status rather than a diagnostic, so tabling it would
/// change the wording and leave the difference.
/// This is the order an ambiguity lists its possibilities in, so the table is
/// OUTPUT rather than housekeeping: sorted by name it answers `--d` with
/// `--dereference-recursive` first, which GNU never prints.
const LONG_OPTIONS: &[(&[u8], Arg)] = &[
    (b"basic-regexp", Arg::None),
    (b"extended-regexp", Arg::None),
    (b"fixed-regexp", Arg::None),
    (b"fixed-strings", Arg::None),
    (b"perl-regexp", Arg::None),
    (b"after-context", Arg::Required),
    (b"before-context", Arg::Required),
    (b"binary-files", Arg::Required),
    (b"byte-offset", Arg::None),
    (b"context", Arg::Required),
    (b"color", Arg::Optional),
    (b"colour", Arg::Optional),
    (b"count", Arg::None),
    (b"devices", Arg::Required),
    (b"directories", Arg::Required),
    (b"exclude", Arg::Required),
    (b"exclude-from", Arg::Required),
    (b"exclude-dir", Arg::Required),
    (b"file", Arg::Required),
    (b"files-with-matches", Arg::None),
    (b"files-without-match", Arg::None),
    (b"group-separator", Arg::Required),
    (b"help", Arg::None),
    (b"include", Arg::Required),
    (b"ignore-case", Arg::None),
    (b"no-ignore-case", Arg::None),
    (b"initial-tab", Arg::None),
    (b"label", Arg::Required),
    (b"line-buffered", Arg::None),
    (b"line-number", Arg::None),
    (b"line-regexp", Arg::None),
    (b"max-count", Arg::Required),
    (b"no-filename", Arg::None),
    (b"no-group-separator", Arg::None),
    (b"no-messages", Arg::None),
    (b"null", Arg::None),
    (b"null-data", Arg::None),
    (b"only-matching", Arg::None),
    (b"quiet", Arg::None),
    (b"recursive", Arg::None),
    (b"dereference-recursive", Arg::None),
    (b"regexp", Arg::Required),
    (b"invert-match", Arg::None),
    (b"silent", Arg::None),
    (b"text", Arg::None),
    // Resolved, not served: leaving it out makes it a silent prefix of
    // `--binary-files`. Its arm refuses.
    (b"binary", Arg::None),
    (b"version", Arg::None),
    (b"with-filename", Arg::None),
    (b"word-regexp", Arg::None),
];

/// `arg` is the whole argv element and `name` the part of it between the `--`
/// and any `=`. glibc names the ELEMENT in both diagnostics -- `--i` and `--i=1`
/// are the same ambiguity and it reports each as given -- so resolving on one
/// and reporting the other is the point of the two arguments.
fn resolve_long(name: &[u8], arg: &[u8]) -> Result<(&'static [u8], Arg), Vec<u8>> {
    let mut hits: Vec<(&'static [u8], Arg)> = Vec::new();
    for (cand, arity) in LONG_OPTIONS {
        if *cand == name {
            return Ok((cand, *arity));
        }
        if !cand.starts_with(name) {
            continue;
        }
        // A second spelling of the first match is neither an ambiguity nor a
        // possibility to report, which is glibc's rule and not a tidy-up.
        if let Some((first, first_arity)) = hits.first() {
            if first_arity == arity && synonymous(first, cand) {
                continue;
            }
        }
        hits.push((cand, *arity));
    }
    match hits.as_slice() {
        [one] => Ok(*one),
        [] => Err(Vec::new()),
        many => {
            let mut msg = name_in("option '", arg, "' is ambiguous; possibilities:");
            for (n, _) in many {
                msg.extend_from_slice(b" '--");
                msg.extend_from_slice(n);
                msg.push(b'\'');
            }
            Err(msg)
        }
    }
}

/// Why gnulib's `argmatch` declined, which is not the same message.
enum NoMatch {
    Invalid,
    /// The value prefixed more than one name and matched none exactly.
    Ambiguous,
}

/// GNU's `argmatch`: an exact name, or an unambiguous PREFIX of one. Note that
/// an ambiguity does NOT end the scan -- an exact match found LATER still wins,
/// which is gnulib's own order and matters as soon as one name prefixes another
/// (`skip` under a list that also held `skip-all`). Unreachable with the three
/// names below, and written this way so the next argmatch option inherits it
/// rather than the bug.
///
/// No empty-string guard: `""` prefixes everything, so GNU calls it AMBIGUOUS
/// rather than invalid, and `-d ''` says so. The obvious defensive line is the
/// divergence.
fn argmatch<T: Copy>(value: &[u8], names: &[(&[u8], T)]) -> Result<T, NoMatch> {
    let mut hit = None;
    let mut ambiguous = false;
    for (name, payload) in names {
        if *name == value {
            return Ok(*payload);
        }
        if name.starts_with(value) {
            ambiguous |= hit.is_some();
            hit = Some(*payload);
        }
    }
    match hit {
        Some(payload) if !ambiguous => Ok(payload),
        Some(_) => Err(NoMatch::Ambiguous),
        None => Err(NoMatch::Invalid),
    }
}

/// `-D`/`--devices`'s argument, which is NOT `-d`'s. The two options look like a
/// matched pair and resolve nothing alike: GNU parses `-d` with `XARGMATCH` and
/// `-D` with plain `STREQ` (grep.c:2529), so `-d rec` is `recurse` while `-D rea`
/// is an error. The diagnostic differs with it -- it names no option and quotes
/// no argument, where `-d`'s does both and says `for '--directories'` -- and so
/// does the STATUS, 2 here against `-d`'s 1. Three divergences between adjacent
/// letters, none of them inferable from the others.
///
/// Returned rather than reported, unlike `dirs_arg`: both callers say the same
/// thing and differ only in how they leave, which is the shape `dirs_arg` cannot
/// use because its message is built from the value.
fn devices_arg(value: &[u8]) -> Option<Devices> {
    match value {
        b"read" => Some(Devices::Read),
        b"skip" => Some(Devices::Skip),
        _ => None,
    }
}

/// `--binary-files`'s argument, as `conf.text`. Two of GNU's three modes are
/// spellings of what this grep already does, so they are served and only
/// `without-match` is refused -- and refused as a KNOWN type nothing here
/// implements, which is not what GNU's own message for an unknown one says.
fn binary_files_arg(value: &[u8]) -> Option<bool> {
    match value {
        b"binary" => Some(false),
        b"text" => Some(true),
        b"without-match" => {
            err("unsupported binary-files type 'without-match'");
            None
        }
        _ => {
            err("unknown binary-files type");
            None
        }
    }
}

/// `-d`/`--directories`'s argument. A failure is reported here rather than
/// returned, because the two callers spell their exits differently and the
/// message is the same either way -- and it exits 1, not the 2 every other usage
/// error uses.
fn dirs_arg(value: &[u8]) -> Option<Dirs> {
    const NAMES: [(&[u8], Dirs); 3] =
        [(b"read", Dirs::Read), (b"recurse", Dirs::Recurse), (b"skip", Dirs::Skip)];
    match argmatch(value, &NAMES) {
        Ok(action) => Some(action),
        Err(no) => complain_dirs(value, matches!(no, NoMatch::Ambiguous)),
    }
}

/// Always `None`; the return type is what lets `dirs_arg` end in one expression.
fn complain_dirs(value: &[u8], ambiguous: bool) -> Option<Dirs> {
    let what = if ambiguous { "ambiguous" } else { "invalid" };
    let mut msg = format!("{what} argument ").into_bytes();
    crate::util::quote_arg(value, &mut msg);
    msg.extend_from_slice(b" for '--directories'");
    errb(&msg);
    eprintln!("Valid arguments are:");
    for name in ["read", "recurse", "skip"] {
        eprintln!("  - '{name}'");
    }
    eprintln!("{USAGE}");
    None
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
        b"fixed-regexp" | b"fixed-strings" => {
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
        b"byte-offset" => conf.byte_offset = true,
        b"with-filename" => conf.with_filename = Some(true),
        b"no-filename" => conf.with_filename = Some(false),
        b"directories" => {
            let v = need(value)?;
            conf.dirs = dirs_arg(&v).ok_or(LongErr::HandledWith(1))?;
        }
        b"devices" => {
            let v = need(value)?;
            let Some(action) = devices_arg(&v) else {
                err("unknown devices method");
                return Err(LongErr::Handled);
            };
            conf.devices = action;
        }
        b"recursive" => conf.dirs = Dirs::Recurse,
        b"dereference-recursive" => {
            conf.dirs = Dirs::Recurse;
            conf.logical = true;
        }
        b"null-data" => conf.null_data = true,
        b"text" => conf.text = true,
        b"binary-files" => {
            let v = need(value)?;
            conf.text = binary_files_arg(&v).ok_or(LongErr::Handled)?;
        }
        // Tabled to be refused rather than served — why each is in the table
        // at all differs and is the module doc's. The usage block goes with it
        // because this is an OPTION error, where the value errors above are
        // GNU's `die()` and print none.
        b"exclude"
        | b"exclude-from"
        | b"exclude-dir"
        | b"include"
        | b"binary"
        | b"initial-tab"
        | b"perl-regexp"
        | b"color"
        | b"colour"
        | b"group-separator"
        | b"no-group-separator"
        | b"no-ignore-case"
        | b"label"
        | b"line-buffered" => {
            errb(&name_in("unsupported option '--", name, "'"));
            eprintln!("{USAGE}");
            return Err(LongErr::Handled);
        }
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

/// GNU drops a duplicate pattern, keeping the first, and COUNTS what is left --
/// `n_patterns` is incremented only for one it inserted (grep.c:182-201), which
/// is what makes `-e ab -e ab` a one-pattern run to the matcher switch.
fn deduped(lines: &[Vec<u8>]) -> Vec<&Vec<u8>> {
    let mut seen = std::collections::HashSet::with_capacity(lines.len());
    lines.iter().filter(|l| seen.insert(l.as_slice())).collect()
}

fn compile(conf: &Conf, lines: &[Vec<u8>]) -> Result<Patterns, String> {
    // The duplicate drop must precede the loop that decides which pattern ends
    // the lex, since only `lex_continues` can see it.
    let lines = deduped(lines);
    if conf.syntax == Syntax::Fixed {
        return Ok(Patterns::Fixed(lines.into_iter().cloned().collect()));
    }
    let opts = Options {
        ere: conf.syntax == Syntax::Extended,
        icase: conf.icase,
        // grep warns and compiles where sed refuses: `grep -E '*a'` is `warning: *
        // at start of expression` and a working pattern, which spencer1 pins.
        strict_repeats: false,
        // grep has no posixicity at all -- neither a `--posix` nor a
        // POSIXLY_CORRECT reading of a pattern -- so both of sed's levels are
        // off here. Its own unmatched-`)` answer is neither of sed's, and the
        // two halves come from different fields: `strict_repeats` above is why
        // the ERE `)` is a literal, and THIS false is why the BRE `\)` is an
        // error.
        posix: false,
        unmatched_rparen_ordinary: false,
        // GNU grep hands dfa `DFA_CONFUSING_BRACKETS_ERROR`, so dfa.c:1142 takes
        // the `dfaerror` arm and `[:alpha:]` without its outer bracket is an
        // error whatever the environment says. sed sets that bit nowhere, which
        // is why the same lint is `dfawarn` there and the variable can discard
        // it. Wiring the variable in here is the obvious wrong fix, so two
        // grep-cli cases pin this `false` against it.
        confusing_bracket_ok: false,
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
        // grep has no `s` and no address, so nothing recompiles: this is only
        // ever sed's question.
        no_sub: false,
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
/// WHERE a printed line sits: the record number `-n` prints and the byte offset
/// `-b` prints. Paired because every prefix wants both or neither and they are
/// written together, and kept as two `Option`s rather than one because under
/// `-o` the offset moves per span while the number does not.
#[derive(Clone, Copy)]
struct At {
    no: Option<u64>,
    byte: Option<u64>,
}

fn prefix(
    out: &mut Out,
    grep: &Grep,
    path: &[u8],
    show_name: bool,
    at: At,
    sep: u8,
) -> std::io::Result<()> {
    if show_name {
        out.write(path)?;
        // -Z replaces only the byte after the NAME; a following line number
        // still carries the ordinary separator.
        out.write(&[if grep.conf.null_name { 0 } else { sep }])?;
    }
    if grep.conf.line_number {
        if let Some(n) = at.no {
            out.write(&number(n))?;
            out.write(&[sep])?;
        }
    }
    // AFTER the line number: `grep -bn` prints `2:6:beta`, not the other way
    // round, and the two are independent rather than one implying the other.
    if grep.conf.byte_offset {
        if let Some(n) = at.byte {
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
    // The LINE's position. Each span reprints it with its own byte offset, this
    // one's plus the span's place in the line, which is what `-bo` prints.
    line_at: At,
    sep: u8,
    line: &[u8],
) -> Result<(), String> {
    let io = |r: std::io::Result<()>| -> Result<(), String> {
        r.map_err(|e| format!("write error: {}", errmsg(&e)))
    };
    let data_sep = if grep.conf.null_data { 0u8 } else { b'\n' };
    // A cursor INTO the line, not an offset: `At` next door means the other one.
    let mut scan = 0usize;
    while let Some((s, e)) = grep.match_at(line, scan, OnBudget::Fail, Want::Span)? {
        if e == s {
            scan = s + 1;
            if scan > line.len() {
                break;
            }
            continue;
        }
        let span = At {
            no: line_at.no,
            byte: line_at
                .byte
                .map(|b| b.saturating_add(u64::try_from(s).unwrap_or(u64::MAX))),
        };
        io(prefix(out, grep, display, show_name, span, sep))?;
        io(out.write(line.get(s..e.min(line.len())).unwrap_or_default()))?;
        // A span reaching PAST the line is `-w -x`: GNU measured it past the record
        // separator, so that byte is part of the match and prints before the one
        // `-o` adds itself. Hence the blank line `grep -w -x -o` leaves per match.
        if e > line.len() {
            io(out.write(&[data_sep]))?;
        }
        io(out.write(&[data_sep]))?;
        scan = e;
    }
    Ok(())
}

/// The `-B` window: the last `cap` records, kept because a match cannot be known
/// until the lines BEFORE it have already gone past. Slots are reused rather than
/// reallocated per line, and a search without `-B` builds none at all.
struct Before {
    cap: usize,
    slots: Vec<(Vec<u8>, bool, u64, u64)>,
    /// Where the next push lands, once the ring is full.
    head: usize,
}

impl Before {
    /// Slots are added as lines arrive rather than reserved up front: `-B` takes
    /// a count with no upper bound (`-B 99999999999999` is answered, not
    /// refused), and reserving that many would abort before the first line was
    /// read. The ring can never need more slots than the file has lines.
    fn new(cap: usize) -> Self {
        Self { cap, slots: Vec::new(), head: 0 }
    }

    /// `Err` is an allocation that failed. `-B N` retains N whole records, so
    /// this is the one place a search's memory is driven by an OPTION rather
    /// than by a buffer, and it has to fail the way a read does -- a diagnosed
    /// `out of memory` and exit 2, not the abort a bare `Vec::push` gives.
    fn push(&mut self, line: &[u8], term: bool, no: u64, at: u64) -> Result<(), ()> {
        if self.cap == 0 {
            return Ok(());
        }
        if self.slots.len() < self.cap {
            let mut text = Vec::new();
            text.try_reserve(line.len()).map_err(|_| ())?;
            text.extend_from_slice(line);
            self.slots.try_reserve(1).map_err(|_| ())?;
            self.slots.push((text, term, no, at));
            return Ok(());
        }
        if let Some(slot) = self.slots.get_mut(self.head) {
            slot.0.clear();
            slot.0.try_reserve(line.len()).map_err(|_| ())?;
            slot.0.extend_from_slice(line);
            slot.1 = term;
            slot.2 = no;
            slot.3 = at;
        }
        self.head = self.head.saturating_add(1) % self.cap;
        Ok(())
    }

    /// The retained lines from `first` onward, oldest first, which is the order
    /// they print in. Where to start is ARITHMETIC rather than a scan: the ring
    /// holds a contiguous run ending at the line before the current one, so a
    /// `-B 1000000` window is not walked a million times to print three lines.
    fn iter_from(
        &self,
        first: u64,
        oldest: u64,
    ) -> impl Iterator<Item = &(Vec<u8>, bool, u64, u64)> {
        let n = self.slots.len();
        let base = if n < self.cap { 0 } else { self.head };
        let skip = usize::try_from(first.saturating_sub(oldest)).unwrap_or(usize::MAX).min(n);
        (skip..n).filter_map(move |i| self.slots.get(base.saturating_add(i) % n.max(1)))
    }

    /// The line number of the oldest record retained, given the current one.
    fn oldest(&self, lineno: u64) -> u64 {
        lineno.saturating_sub(self.slots.len() as u64)
    }
}

fn search_file(
    grep: &Grep,
    out: &mut Out,
    // The NAME to print, already resolved: only an operand spells stdin `-`, so
    // the caller is the one that knows whether this came from the walk.
    display: &[u8],
    rec: &mut Records<Input>,
    show_name: bool,
    // Whether an earlier FILE already printed a context group. `printed_upto`
    // resets per file, so without this the `--` at a file boundary is lost.
    printed_before: &mut bool,
    // Set when the read failed PART WAY: the caller owns the exit status, and a
    // file that produced output before failing still counts as having matched.
    read_failed: &mut bool,
) -> Result<bool, String> {
    let sep = if grep.conf.null_data { 0u8 } else { b'\n' };
    // A binary file's MATCHING LINES are replaced by a notice — so `-o`, which
    // prints matched text, is suppressed too. `-c`/`-l`/`-L`/`-q` print no line
    // content at all, so they run normally and emit no notice.
    let counts_only = grep.conf.count
        || grep.conf.files_with
        || grep.conf.files_without
        || grep.conf.quiet;
    let mut count: u64 = 0;
    // How far the output reaches, as a LINE NUMBER: the last line printed OR
    // covered by context, 0 for none. `-o` drops context lines but not the range
    // they cover. A number rather than the index it was, since streaming means
    // there is no longer a slice to index into.
    let mut covered: u64 = 0;
    let mut pending_after: usize = 0;
    let mut any = false;

    let io = |r: std::io::Result<()>| -> Result<(), String> { r.map_err(|e| format!("write error: {}", errmsg(&e))) };
    if grep.settled() {
        // Nothing can match, but the operand was OPENED, and only a READ
        // discovers that it cannot be read -- `grep -L -m 0 a DIR f` reports
        // `Is a directory` and exits 2 in GNU. One record is enough: the
        // failure is the first fill's. The whole-file reader this replaced got
        // this for free by reading before it ever reached here.
        if let Err(e) = rec.next() {
            if !grep.conf.no_messages {
                errb(&name_in("", display, &format!(": {}", errmsg(&e))));
            }
            *read_failed = true;
        }
        if grep.conf.files_without && !grep.conf.quiet {
            io(out.write(display))?;
            io(out.write(&[if grep.conf.null_name { 0 } else { b'\n' }]))?;
        }
        return Ok(false);
    }
    let mut limit_reached = false;
    let before_cap = grep.conf.before_lines();
    // The window is only ever READ by the context block, which every
    // counts-only mode skips -- so `-c -B 5` keeps nothing rather than copying
    // a record per line it will never print.
    let keep_window = before_cap > 0 && !counts_only;
    let mut window = Before::new(before_cap);
    let mut lineno: u64 = 0;

    loop {
        match rec.next() {
            Ok(true) => {}
            Ok(false) => break,
            // Opened and unreadable. GNU reports it and keeps whatever it had
            // already printed, so the read failure ends this file rather than
            // discarding its output.
            Err(e) => {
                if !grep.conf.no_messages {
                    errb(&name_in("", display, &format!(": {}", errmsg(&e))));
                }
                *read_failed = true;
                break;
            }
        }
        // Empty records the reader dropped before this one still occupy line
        // numbers -- GNU credits `totalnl` for exactly this, and `grep -z -n`
        // over a megabyte of NULs is where it shows.
        let (dropped, joined) = rec.take_skipped();
        if dropped > 0 && keep_window && !joined {
            // They also sit BETWEEN this record and whatever the window still
            // holds, so leaving that untouched would offer a record from before
            // the run as this one's neighbour. Every dropped record is empty and
            // they are consecutive, so replaying the last `before_cap` of them
            // is the whole of the repair -- and bounded, however long the run.
            // Not when the gap was JOINED into this record, though: then it left
            // no records at all and a replay would invent context.
            let replay = dropped.min(before_cap as u64);
            // They occupy `lineno + 1 ..= lineno + dropped`, so the tail of that
            // range starts one PAST the subtraction.
            let first =
                lineno.saturating_add(dropped).saturating_sub(replay).saturating_add(1);
            // Their BYTES are the tail of the run, which ended where this
            // record begins -- one byte each, being a separator and nothing else.
            let first_at = rec.offset().saturating_sub(replay);
            for i in 0..replay {
                let at = first_at.saturating_add(i);
                if window.push(b"", true, first.saturating_add(i), at).is_err() {
                    return Err("out of memory".to_string());
                }
            }
        }
        lineno = lineno.saturating_add(dropped).saturating_add(1);
        let at = rec.offset();
        let line = rec.line();
        // The binary verdict is asked HERE rather than once per file: it is the
        // buffer this record arrived in that decides, and a NUL in a later
        // buffer must not unprint a match already emitted from an earlier one.
        let binary = !grep.conf.text && !counts_only && rec.binary() && sep != 0;
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
                // The window holds the last `before_cap` records, so the range
                // wanted here is always inside it.
                let first_ctx = lineno.saturating_sub(before_cap as u64).max(1);
                if grep.conf.has_context() {
                    let gap = match covered {
                        0 => *printed_before,
                        upto => first_ctx > upto.saturating_add(1),
                    };
                    if gap {
                        io(out.write(b"--\n"))?;
                    }
                    *printed_before = true;
                    let ctx_start = first_ctx.max(covered.saturating_add(1));
                    // The window holds lines strictly older than this one, so
                    // `ctx_start` alone bounds the run -- there is no upper end
                    // to test for.
                    let oldest = window.oldest(lineno);
                    for (ctx, ctx_term, no, ctx_at) in window.iter_from(ctx_start, oldest) {
                        if grep.conf.only {
                            if grep.conf.prints_context_spans() {
                                let w = At { no: Some(*no), byte: Some(*ctx_at) };
                                write_spans(grep, out, display, show_name, w, b'-', ctx)?;
                            }
                            continue;
                        }
                        let w = At { no: Some(*no), byte: Some(*ctx_at) };
                        io(prefix(out, grep, display, show_name, w, b'-'))?;
                        io(out.write(ctx))?;
                        // Only the LAST record can lack a separator, so this is
                        // the same test the whole-file form spelled as "or there
                        // is a line after it".
                        if *ctx_term {
                            io(out.write(&[sep]))?;
                        }
                    }
                }
                if grep.conf.only {
                    if grep.conf.prints_selected_spans() {
                        let w = At { no: Some(lineno), byte: Some(at) };
                        write_spans(grep, out, display, show_name, w, b':', line)?;
                    }
                } else {
                    let w = At { no: Some(lineno), byte: Some(at) };
                    io(prefix(out, grep, display, show_name, w, b':'))?;
                    io(out.write(line))?;
                    // GNU terminates every line it prints, including a final
                    // input line that carried no newline of its own.
                    io(out.write(&[sep]))?;
                }
                covered = lineno;
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
                    let w = At { no: Some(lineno), byte: Some(at) };
                    write_spans(grep, out, display, show_name, w, b'-', line)?;
                }
            } else {
                let w = At { no: Some(lineno), byte: Some(at) };
                io(prefix(out, grep, display, show_name, w, b'-'))?;
                io(out.write(line))?;
                io(out.write(&[sep]))?;
            }
            covered = lineno;
            pending_after -= 1;
        }
        if out.is_broken() {
            break;
        }
        if keep_window && window.push(rec.line(), rec.terminated(), lineno, at).is_err() {
            if !grep.conf.no_messages {
                errb(&name_in("", display, ": out of memory"));
            }
            *read_failed = true;
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
        io(prefix(out, grep, display, show_name, At { no: None, byte: None }, b':'))?;
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

    /// glibc dedupes by comparing the option STRUCT -- same `has_arg`, `flag`
    /// and `val` -- so an accidentally duplicated entry collapses there for
    /// free. td-txt names its pairs instead, which is narrower on purpose but
    /// leaves two things the compiler will not check: a repeated name would be
    /// reported as ambiguous with ITSELF, and a typo in the roster silently
    /// stops a real pair collapsing.
    #[test]
    fn the_option_table_and_its_synonym_roster_agree() {
        let names: Vec<&[u8]> = LONG_OPTIONS.iter().map(|(n, _)| *n).collect();
        for (i, n) in names.iter().enumerate() {
            assert!(
                !names.get(i + 1..).unwrap_or_default().contains(n),
                "{} is declared twice, and would be ambiguous with itself",
                String::from_utf8_lossy(n)
            );
        }
        for group in SYNONYMS {
            assert!(group.len() >= 2, "a synonym group needs two names");
            for n in *group {
                assert!(
                    names.contains(n),
                    "{} is in the synonym roster but not the table",
                    String::from_utf8_lossy(n)
                );
            }
            // Deliberately NOT asserted: that a group agrees about its
            // argument policy. glibc collapses on `val` AND `has_arg`, so the
            // roster is the first half and the arity test at the call site is
            // the second. Enumerating GNU's four same-option groups, nothing
            // reaches that second half: `fixed-*` and `color`/`colour` agree
            // about arity, and the one pair that does NOT --
            // `--group-separator` against `--no-group-separator`, both
            // GROUP_SEPARATOR_OPTION -- shares no prefix but the empty one,
            // where neither can be the first match. It is kept because the
            // rule belongs to getopt_long rather than to this table.
        }
    }

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
        let regex_matcher =
            gnu_runs_regex_matcher(&conf, &deduped(&patterns));
        Grep { conf, pats, only_empty, regex_matcher }
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
        assert_eq!(g.match_at(b"a.", 0, OnBudget::Fail, Want::Selection).unwrap(), Some((2, 2)));
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
        assert_eq!(g.match_at(b"..a", 0, OnBudget::Fail, Want::Selection).unwrap(), Some((0, 1)));
        // Greedy at 0 is `.`, not word-bounded; only the EMPTY span is.
        assert_eq!(g.match_at(b".a", 0, OnBudget::Fail, Want::Selection).unwrap(), Some((0, 0)));
        // ...and that span is where the two questions part company: GNU's retry
        // loop rejects an empty SHRINK, so the line selects with no span to print.
        assert_eq!(g.match_at(b".a", 0, OnBudget::Fail, Want::Span).unwrap(), None);
        // Where the shorter span is not empty both agree on it.
        assert_eq!(g.match_at(b"..a", 0, OnBudget::Fail, Want::Span).unwrap(), Some((0, 1)));
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
        assert_eq!(g.match_at(b"cat at", 0, OnBudget::Fail, Want::Selection).unwrap(), Some((4, 6)));
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
        assert_eq!(g.match_at(b"xabb yab", 0, OnBudget::Fail, Want::Span).unwrap(), Some((1, 4)));
        assert_eq!(g.match_at(b"xabb yab", 4, OnBudget::Fail, Want::Span).unwrap(), Some((6, 8)));
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

    /// The one property of `argmatch` that today's three names cannot exercise,
    /// since none of them prefixes another: an exact match found AFTER an
    /// ambiguity still wins. A scan that returned at the ambiguity would refuse
    /// `skip` here, which is what gnulib does not do.
    #[test]
    fn an_exact_argmatch_outranks_an_earlier_ambiguity() {
        let names: [(&[u8], u8); 3] = [(b"skip-all", 1), (b"skip-any", 2), (b"skip", 3)];
        assert_eq!(argmatch(b"skip", &names).ok(), Some(3));
        // Still ambiguous when nothing matches exactly, and order-independent.
        assert!(matches!(argmatch(b"skip-a", &names), Err(NoMatch::Ambiguous)));
        assert!(matches!(argmatch(b"", &names), Err(NoMatch::Ambiguous)));
        assert_eq!(argmatch(b"skip-al", &names).ok(), Some(1));
        assert!(matches!(argmatch(b"nope", &names), Err(NoMatch::Invalid)));
    }
}

//! Differential test: the `spec_helpers` grep against a reference grep.
//!
//!     cargo run --release --manifest-path td-sh/Cargo.toml \
//!         --example grep_differential -- <reference-grep> <spec-helpers-binary>
//!
//! NOT run by the gate, and deliberately: it needs a real grep to compare
//! against, and the gate may have none -- an undeclared dependency is exactly
//! what td does not allow. It is committed because it is the evidence behind
//! the applet's claims, and evidence nobody can re-run is a claim nobody can
//! check. Every defect the applet's history records was found here first.
//!
//! Pass a REFERENCE, not whatever `grep` is on PATH: on the machine this was
//! written on that is ugrep in its compatible mode, which is a different
//! program to be right about. GNU grep 3.11 is what the numbers were taken
//! against.
//!
//! Two halves, because they fail differently. The SWEEP compares a fixed matrix
//! of patterns, inputs and option sets: it is what regressions show up in. The
//! REFUSALS assert that a construct outside the served subset comes back
//! status 2 -- the applet's whole safety argument, since a matcher that
//! approximated one would report a wrong match and the spec case would be
//! graded on it.
//!
//! A status-2 answer in the SWEEP is a mismatch, not a licensed outcome. Every
//! pattern the sweeps draw from is inside the served subset, so nothing there
//! should refuse -- and counting a refusal as success would let a matcher that
//! refused EVERYTHING report zero disagreements with nothing left to compare.
//! The shapes that must refuse are REFUSALS, which asserts the opposite.

#![forbid(unsafe_code)]

use std::io::Write;
use std::process::{Command, Stdio};

/// Patterns inside the served subset, in both dialects.
const PATTERNS: &[&str] = &[
    "foo", "^foo", "foo$", "^$", ".", "a.c", "a*", "aa*", "^a*$", "[0-9]", "[0-9][0-9]", "[^/]",
    "[a-z]", "[A-Z]", "[abc]", "[]]", "[-a]", "[a-]", "z=", "^z", "=$", "x", "^", "$", "b*", ".*",
    "aab", "^aaa$", "[0-9]*", "a[0-9]b", "^[a-z]*$", "\\.", "\\*", "\\C-o", "[a-z0-9]*",
    // The sets `\s`/`\w` name, and the bracketed classes they are short for.
    // Both dialects spell these the same way, which is why they are here rather
    // than in either dialect-only table.
    "\\s", "\\S", "\\w", "\\W", "[[:space:]]", "[[:alpha:]]", "[[:digit:]]", "[[:alnum:]_]",
    "[^[:space:]]", "[[:space:][:digit:]]", "[[:upper:]]", "[[:punct:]]", "[[:xdigit:]]",
    "[[:blank:]]", "[[:print:]]", "[[:graph:]]", "[[:cntrl:]]", "[[:lower:]]", "[x[:digit:]]",
    "[[:digit:]x]", "^[[:space:]]*$",
    // …and the near misses of the range rule above, where the `-` really is an
    // ordinary member because a `]` follows it.
    "[[:digit:]-]", "[-[:digit:]]", "[a[:digit:]-]",
    // The typo's own near misses, which GNU READS rather than diagnoses: an
    // empty name, and the two where a `-` makes it a range instead. They are
    // the other side of the refusals below, and the boundary is only pinned by
    // sweeping both.
    "[::]", "[:-:]", "[:a-b:]", "[:ab:c]", "[x:a:]", "[:a:x]",
];

/// …and the ones only an extended expression can express.
const ERE_ONLY: &[&str] = &["[0-9]+", "a+", "[a-z]+b", "^[0-9]+$", "a?b", "aa?", "[^/]+$", "x+"];

/// A BRE reads these as literals where an ERE reads operators, so they belong
/// to the basic dialect alone.
const BRE_ONLY: &[&str] = &[
    "a+b", "a?b", "*x", "a{2}", "a|b", "(ab)",
    // GNU's two BRE repeat extensions, which an extended expression spells
    // without the backslash -- `s/ \+/ /g` is the corpus's commonest sed script.
    "a\\+", " \\+", "x\\?", "[ \\t]\\+", "[0-9]\\+", "^a\\+$", "[[:space:]]\\+",
];

const INPUTS: &[&str] = &[
    "foo\nbar\nfoobar\n",
    "a12b\n7\nxx\n",
    "\n",
    "",
    "no trailing newline",
    "Foo\nFOO\nfoo\n",
    "/usr/local/bin\n/etc\n",
    "aaa\naab\nabb\n",
    "z=1\nzz=2\nA=3\n",
    "[bracket]\n^caret\ndollar$\n",
    "one two  three\n\ttabbed\n",
    "aaaa\n",
    "C-o\nC-s\n",
    "a\nb\nc\na\nb\na\n",
    // For the classes: the whitespace run `s/ \+/ /g` collapses, a vertical tab
    // (whitespace to C and not to Rust), the punctuation/underscore boundary
    // `\w` draws, and control bytes `[[:cntrl:]]` picks out.
    "a   b\t\tc\n   \n",
    "a\x0bb\n",
    "ab_9!\n__\n",
    "x\x01y\x7f\n",
    "MiXeD Case 42\n",
];

/// Option sets. `-o` crossed with `-A` is here because it was NOT: selection
/// and printing are separate questions, and printing inside the scan lost both
/// the separator and a context line's own matches until a sweep crossed them.
const OPTIONS: &[&[&str]] = &[
    &[],
    &["-o"],
    &["-i"],
    &["-v"],
    &["-q"],
    &["-A", "1"],
    &["-A", "0"],
    &["-o", "-A", "1"],
    &["-o", "-A", "0"],
    &["-o", "-i"],
    &["-v", "-A", "1"],
    &["-m", "1"],
    &["-o", "-v"],
];

/// Constructs OUTSIDE the subset. Each must come back status 2: these are the
/// shapes where approximating would be a silently wrong match rather than a
/// visible failure. The dialect each is refused in is part of the claim -- `a+b`
/// is a literal to a BRE and an operator to an ERE, so only one of the two is
/// a refusal.
const REFUSALS: &[(&str, &str)] = &[
    // Alternation, grouping and intervals.
    ("-E", "a|b"),
    ("-E", "(ab)"),
    ("-E", "a{2}"),
    ("-E", "a{2,3}"),
    ("", "\\|"),
    ("", "\\(ab\\)"),
    ("", "\\+"),
    ("", "\\?"),
    ("", "\\{2\\}"),
    // A repeat OF a repeat, which GNU accepts and this deliberately does not.
    // The BRE spellings are the ones the two-byte operator makes possible, and
    // they were the claim in the message with nothing pinning it.
    ("", "a\\+\\+"),
    ("", "a\\+*"),
    ("", "a*\\+"),
    ("", "x\\?\\?"),
    ("-E", "a++"),
    // GNU escapes: operators, not literals. `\bfoo\b` matched `bfoob` and
    // missed `foo bar` while these were read as the letters they contain.
    ("", "\\bfoo\\b"),
    ("-E", "\\bfoo\\b"),
    ("", "\\B"),
    ("", "\\<foo"),
    ("", "foo\\>"),
    ("", "\\1"),
    ("-E", "\\1"),
    // The single-bracket typo, which is the spelling that turns up: as a plain
    // class it silently means `{:,a,l,p,h}`. The BRACKETED form is served now,
    // so only the typo and the malformed spellings remain here.
    ("", "[:alpha:]"),
    ("-E", "[:digit:]"),
    // GNU diagnoses the typo by SHAPE, not by whether the name could be a class
    // -- `[:/:]` and `[: :]` as readily as `[:alpha:]`. Requiring the name to be
    // alphanumeric served these silently, which is the failure this whole entry
    // is about.
    ("", "[:/:]"),
    ("", "[: :]"),
    ("", "[:_:]"),
    ("", "[:a1:]"),
    ("", "[^:/:]"),
    ("", "[[:foo:]]"),
    ("", "[[:space]]"),
    ("", "[[:space:]"),
    ("-E", "[[:foo:]]"),
    // A named class cannot START a range: GNU's "Invalid range end". Reading it
    // as the set plus `-` plus the endpoint is a class that matches MORE than
    // was asked for -- found by two reviewers at once, and by neither sweep.
    ("", "[[:digit:]-a]"),
    ("-E", "[[:digit:]-a]"),
    ("", "[[:alpha:]-9]"),
    ("", "[[:digit:]-[:alpha:]]"),
    // …nor END one, which is the same error from the other side and reaches the
    // parser by a different route: the range takes the `[` as its endpoint, so
    // what is left reads as the ordinary letters the class is spelled with.
    ("", "[ -[:digit:]]"),
    ("-E", "[ -[:digit:]]"),
    ("", "[a-[:alpha:]]"),
    ("", "[[=a=]]"),
    ("", "[[.a.]]"),
    // A collating symbol as a range end is the one spelling here GNU ACCEPTS.
    // Refusing it is deliberate and follows from the two entries above: this
    // matcher serves no collating symbol at all, so there is nothing for a
    // range to end at.
    ("", "[a-[.x.]]"),
    // Malformed classes.
    ("", "[abc"),
    ("", "a["),
    ("", "[z-a]"),
    ("", "[a-b-c]"),
    // An ERE makes `^`/`$` anchors ANYWHERE; a BRE makes them literals away
    // from the ends, which is served, so only the extended spelling refuses.
    ("-E", "a$b"),
    ("-E", "a^b"),
    ("-E", "^^ab"),
    ("-E", "$$"),
    // A repeat with nothing to repeat, and a repeat of a repeat.
    ("-E", "*x"),
    ("-E", "^*x"),
    ("", "a**"),
    ("-E", "a**+"),
    ("-E", "a+*"),
    ("-E", "a?+"),
    // A trailing backslash has nothing to escape.
    ("", "\\"),
];

struct Ran {
    status: i32,
    stdout: Vec<u8>,
}

fn run(bin: &str, args: &[String], input: &str) -> Result<Ran, String> {
    use std::os::unix::process::CommandExt;
    let mut child = Command::new(bin)
        // The C locale, PINNED rather than inherited. A range is collation
        // order to GNU and byte order here, so under this machine's
        // `en_US.utf8` the two agree only where the alphabet happens to be
        // contiguous -- `[!-[]` matches `A` under C and does not under
        // en_US.utf8, and every class could differ the same way. Inheriting it
        // would make the comparison a fact about whoever ran it.
        .env("LC_ALL", "C")
        // The helper is a MULTICALL: it chooses its applet from `argv[0]`, so
        // invoking it by its own path would reach the "no applet" arm and
        // report 2 for everything -- which this harness would then count as
        // a refusal and compare nothing at all.
        .arg0("grep")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("{bin}: {e}"))?;
    if let Some(mut w) = child.stdin.take() {
        // A reference grep may stop reading early (`-q` exits at its first
        // hit), so a refused write is that program's answer, not an error.
        let _ = w.write_all(input.as_bytes());
    }
    let out = child.wait_with_output().map_err(|e| format!("{bin}: {e}"))?;
    Ok(Ran { status: out.status.code().unwrap_or(-1), stdout: out.stdout })
}

/// A deterministic xorshift, so a failing run reproduces from its seed alone.
/// Hand-rolled because the engine carries no external crates.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, n: usize) -> usize {
        match n {
            0 => 0,
            n => usize::try_from(self.next() % n as u64).unwrap_or(0),
        }
    }

    fn pick<'a, T>(&mut self, xs: &'a [T]) -> Option<&'a T> {
        xs.get(self.below(xs.len()))
    }
}

/// The last four COMPOSE a named class with something else inside one bracket,
/// which is the folding path rather than the class one: a bracket holding only
/// `[[:digit:]]` is served by a parser that read the class and stopped, and
/// says nothing about one that has to go on reading after it.
const ATOMS: &[&str] =
    &["a", "b", "z", "1", "7", "=", "/", ".", "[0-9]", "[a-z]", "[^/]", "[abc]", "[^abc]", "x",
      "\\.", "-", ":", "\\s", "\\w", "\\S", "\\W", "[[:digit:]]", "[[:alpha:]]",
      "[^[:space:]]", "[[:punct:]]", "[[:digit:]abc]", "[x[:alpha:]]",
      "[[:digit:][:punct:]]", "[^[:upper:]0-9]"];

fn generated(rng: &mut Rng, ere: bool) -> String {
    // A BRE spells the two GNU repeats with a backslash, and they are the ones
    // whose operator is TWO bytes -- so a generator without them never asks
    // what the second byte does.
    let reps: &[&str] = match ere {
        true => &["", "", "", "*", "+", "?"],
        false => &["", "", "", "*", "\\+", "\\?"],
    };
    let mut pat = String::new();
    for _ in 0..=rng.below(4) {
        pat.push_str(rng.pick(ATOMS).copied().unwrap_or("a"));
        pat.push_str(rng.pick(reps).copied().unwrap_or(""));
    }
    // Anchors are attached at the ends, where both dialects agree they are
    // anchors -- an ERE reads one in the MIDDLE as an anchor too, which the
    // refusal table above covers instead.
    if rng.below(10) < 3 {
        pat.insert(0, '^');
    }
    if rng.below(10) < 3 {
        pat.push('$');
    }
    pat
}

fn generated_input(rng: &mut Rng) -> String {
    // `A` so `upper` and the complements are asked about something, and the
    // VERTICAL TAB because that is where `space` diverges: C's `isspace`
    // counts it and Rust's `is_ascii_whitespace` does not, so a haystack
    // without one agrees with the wrong definition.
    let alphabet: &[u8] = b"abz017=/x:- A\t\x0b";
    let mut out = String::new();
    for _ in 0..rng.below(6) {
        for _ in 0..rng.below(9) {
            let c = rng.pick(alphabet).copied().unwrap_or(b'a');
            out.push(char::from(c));
        }
        out.push('\n');
    }
    out
}

struct Tally {
    compared: usize,
    refused: usize,
    mismatched: usize,
}

fn compare(
    reference: &str,
    helper: &str,
    args: &[String],
    input: &str,
    tally: &mut Tally,
) -> Result<(), String> {
    let ours = run(helper, args, input)?;
    // A refusal here is a MISMATCH, not a licensed outcome. Every pattern the
    // sweeps below draw from is inside the served subset -- the fixed tables by
    // construction, the generated ones because `generated` composes in-subset
    // atoms and at most one repeat each -- so nothing should refuse, and
    // counting a refusal as success would let an applet that refused
    // EVERYTHING report zero disagreements with nothing left to compare. The
    // shapes that must refuse are checked by REFUSALS above, which asserts the
    // opposite.
    if ours.status == 2 {
        tally.refused += 1;
        tally.mismatched += 1;
        println!("REFUSED (in-subset) args={args:?} input={input:?}");
        return Ok(());
    }
    let theirs = run(reference, args, input)?;
    tally.compared += 1;
    if (theirs.status, &theirs.stdout) != (ours.status, &ours.stdout) {
        tally.mismatched += 1;
        if tally.mismatched <= 20 {
            println!("MISMATCH args={args:?} input={input:?}");
            println!("  reference -> {} {:?}", theirs.status, String::from_utf8_lossy(&theirs.stdout));
            println!("  ours      -> {} {:?}", ours.status, String::from_utf8_lossy(&ours.stdout));
        }
    }
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let (Some(reference), Some(helper)) = (argv.first(), argv.get(1)) else {
        return Err("usage: grep_differential <reference-grep> <spec-helpers-binary> [rounds] [seed]"
            .into());
    };
    let rounds: usize = argv.get(2).and_then(|s| s.parse().ok()).unwrap_or(25_000);
    let seed: u64 = argv.get(3).and_then(|s| s.parse().ok()).unwrap_or(0x2026_0811);

    let mut tally = Tally { compared: 0, refused: 0, mismatched: 0 };

    // --- the refusal contract --------------------------------------------
    let mut wrongly_accepted = 0usize;
    for (flag, pat) in REFUSALS {
        let mut args: Vec<String> = Vec::new();
        if !flag.is_empty() {
            args.push((*flag).to_string());
        }
        args.push("--".to_string());
        args.push((*pat).to_string());
        let ours = run(helper, &args, "ab\nfoo bar\nbfoob\n1\n")?;
        if ours.status != 2 {
            wrongly_accepted += 1;
            println!("ACCEPTED (should refuse) {flag} {pat:?} -> status {}", ours.status);
        }
    }
    println!("refusals: {} checked, {wrongly_accepted} wrongly accepted", REFUSALS.len());

    // --- the systematic sweep --------------------------------------------
    for ere in [false, true] {
        let dialect: Vec<&str> = match ere {
            true => ERE_ONLY.iter().copied().collect(),
            false => BRE_ONLY.iter().copied().collect(),
        };
        for pat in PATTERNS.iter().chain(dialect.iter()) {
            for input in INPUTS {
                for opts in OPTIONS {
                    let mut args: Vec<String> = Vec::new();
                    if ere {
                        args.push("-E".to_string());
                    }
                    args.extend(opts.iter().map(|o| (*o).to_string()));
                    args.push("--".to_string());
                    args.push((*pat).to_string());
                    compare(reference, helper, &args, input, &mut tally)?;
                }
            }
        }
    }
    // …and the fixed-string matcher, where no byte is an operator.
    for pat in ["foo", "a.c", "[0-9]", "a*", "^foo", "$", "z=", "a+b"] {
        for input in INPUTS {
            for opts in [&[][..], &["-o"][..], &["-i"][..]] {
                let mut args: Vec<String> = vec!["-F".to_string()];
                args.extend(opts.iter().map(|o| (*o).to_string()));
                args.push("--".to_string());
                args.push(pat.to_string());
                compare(reference, helper, &args, input, &mut tally)?;
            }
        }
    }
    println!(
        "sweep: {} compared, {} refused, {} mismatched",
        tally.compared, tally.refused, tally.mismatched
    );

    // --- the randomised sweep --------------------------------------------
    let mut rng = Rng(seed | 1);
    for _ in 0..rounds {
        let ere = rng.below(2) == 1;
        let pat = generated(&mut rng, ere);
        let input = generated_input(&mut rng);
        let mut args: Vec<String> = Vec::new();
        if ere {
            args.push("-E".to_string());
        }
        if let Some(opts) = rng.pick(OPTIONS) {
            args.extend(opts.iter().map(|o| (*o).to_string()));
        }
        args.push("--".to_string());
        args.push(pat);
        compare(reference, helper, &args, &input, &mut tally)?;
    }
    println!(
        "with {rounds} random rounds (seed {seed}): {} compared, {} refused, {} mismatched",
        tally.compared, tally.refused, tally.mismatched
    );

    match tally.mismatched + wrongly_accepted {
        0 => Ok(()),
        n => Err(format!("{n} disagreement(s) with {reference}").into()),
    }
}

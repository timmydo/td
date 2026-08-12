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
//! A status-2 answer in the sweep is not a mismatch. It is the applet declining
//! to guess, which is always a correct outcome; it is counted and reported so a
//! subset that quietly shrank is visible.

#![forbid(unsafe_code)]

use std::io::Write;
use std::process::{Command, Stdio};

/// Patterns inside the served subset, in both dialects.
const PATTERNS: &[&str] = &[
    "foo", "^foo", "foo$", "^$", ".", "a.c", "a*", "aa*", "^a*$", "[0-9]", "[0-9][0-9]", "[^/]",
    "[a-z]", "[A-Z]", "[abc]", "[]]", "[-a]", "[a-]", "z=", "^z", "=$", "x", "^", "$", "b*", ".*",
    "aab", "^aaa$", "[0-9]*", "a[0-9]b", "^[a-z]*$", "\\.", "\\*", "\\C-o", "[a-z0-9]*",
];

/// …and the ones only an extended expression can express.
const ERE_ONLY: &[&str] = &["[0-9]+", "a+", "[a-z]+b", "^[0-9]+$", "a?b", "aa?", "[^/]+$", "x+"];

/// A BRE reads these as literals where an ERE reads operators, so they belong
/// to the basic dialect alone.
const BRE_ONLY: &[&str] = &["a+b", "a?b", "*x", "a{2}", "a|b", "(ab)"];

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
    // GNU escapes: operators, not literals. `\bfoo\b` matched `bfoob` and
    // missed `foo bar` while these were read as the letters they contain.
    ("", "\\bfoo\\b"),
    ("-E", "\\bfoo\\b"),
    ("", "\\B"),
    ("", "\\w"),
    ("", "\\W"),
    ("", "\\s"),
    ("", "\\S"),
    ("", "\\<foo"),
    ("", "foo\\>"),
    ("", "\\1"),
    ("-E", "\\1"),
    // Named classes -- BOTH spellings. The single-bracket typo is the one that
    // turns up, and as a plain class it silently means `{:,a,l,p,h}`.
    ("", "[[:alpha:]]"),
    ("", "[:alpha:]"),
    ("-E", "[:digit:]"),
    ("", "[[=a=]]"),
    ("", "[[.a.]]"),
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

const ATOMS: &[&str] =
    &["a", "b", "z", "1", "7", "=", "/", ".", "[0-9]", "[a-z]", "[^/]", "[abc]", "[^abc]", "x",
      "\\.", "-", ":"];

fn generated(rng: &mut Rng, ere: bool) -> String {
    let reps: &[&str] = match ere {
        true => &["", "", "", "*", "+", "?"],
        false => &["", "", "", "*"],
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
    let alphabet: &[u8] = b"abz017=/x:- ";
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

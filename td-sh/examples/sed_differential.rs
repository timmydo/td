//! `spec_helpers sed` against a reference GNU sed, byte for byte.
//!
//! The same argument as `grep_differential`: the corpus GRADES this applet's
//! output as the shell's, so a plausible-but-wrong substitution is a spec
//! failure blamed on td-sh. Hand-picked cases found the two divergences that
//! mattered (`\t` is a TAB to sed and a stray `t` to grep, and an empty match
//! abutting the previous one is not replaced); everything past that is what a
//! generator is for.
//!
//! What is compared is stdout, stderr's PRESENCE, and the exit status. Not
//! stderr's text: GNU's diagnostics are its own and no corpus golden reads one.
//!
//! Run it against real GNU sed:
//!
//! ```text
//! cargo build --release --manifest-path td-sh/Cargo.toml
//! cargo run --release --manifest-path td-sh/Cargo.toml --example sed_differential \
//!   -- /usr/bin/sed td-sh/target/release/spec_helpers [rounds] [seed]
//! ```

#![forbid(unsafe_code)]

use std::io::Write;
use std::process::{Command, Stdio};

/// Scripts inside the served subset. Every one must agree byte for byte.
const SCRIPTS: &[&str] = &[
    "s/a/X/",
    "s/a/X/g",
    "s/ \\+/ /g",
    "s/[ \\t]\\+/ /g",
    "s/\"//g",
    "s/:$//",
    "s/^#/%/",
    "s/[[:space:]]*$//",
    "s/.*sh /sh /g",
    "s/=/@/g",
    "s/x*/-/g",
    "s/a*/-/g",
    "s/b/[&]/g",
    "s/a/&&/g",
    "s/(/[/g",
    "s:x:$X:g",
    "s,a,b,g",
    "s/^- y$/- 'y'/",
    "s/[0-9]\\+/N/g",
    "s/[[:digit:]]/#/g",
    "s/\\s\\+/_/g",
    "s/\\w/./g",
    "s/^/>/",
    "s/$/</",
    "s/a/\\t/g",
    "s/a/\\n/g",
    "s/\\x41/Z/g",
    "s/e/E/;s/o/O/",
    "s/a/1/\ns/b/2/",
];

/// …and the shapes outside it, each of which must come back status 2. A wrong
/// answer here is silent, which is why none is approximated.
const REFUSALS: &[&str] = &[
    // Commands other than `s`.
    "d",
    "p",
    "1d",
    "$d",
    "/x/d",
    "/x/s/a/b/",
    "y/abc/xyz/",
    "q",
    "2,4d",
    // Flags that change WHICH text comes out.
    "s/a/b/p",
    "s/a/b/2",
    "s/a/b/2g",
    "s/a/b/i",
    "s/a/b/w out",
    // A backreference needs capture groups, which the matcher has none of --
    // and so does the GROUP itself, `\(` being an operator to a BRE rather than
    // the literal paren it looks like.
    "s/\\(a\\)/\\1/",
    "s/a/\\1/",
    "s/\\(/(/g",
    // Case conversion changes the text a fall-through would emit verbatim.
    "s/a/\\Ux/",
    "s/a/\\Lx/",
    "s/a/\\ux/",
    // The empty regex is "reuse the last one", which this does not track.
    "s//x/",
    // Delimiters this matcher gives meaning to: `\.` inside would stop being an
    // escaped dot.
    "s.a.b.",
    "s*a*b*",
    "s[a[b[",
    // Malformed.
    "s/a",
    "s/a/b",
    "s",
    "s/",
    // Whitespace follows a command but does not SEPARATE two: GNU rejects this
    // and reading the space as a separator silently ran both.
    "s/a/b/ s/b/c/",
    // …and a repeated flag is "multiple `g' options to `s' command".
    "s/a/b/gg",
    // A raw newline inside a field is an unterminated `s`, which is what a
    // script mis-split across `-e` looks like once they are joined.
    "s/a/x\ny/",
    "s/a\nb/x/",
    // `&` cannot be the delimiter: `\&` would have to be both the escaped
    // delimiter and the literal ampersand. GNU serves it; this refuses.
    "s&a&x&",
    "s&a&x\\&y&",
    // Patterns outside the matcher's own subset.
    "s/a\\|b/x/",
    "s/\\ba/x/",
    "s/a\\{2\\}/x/",
];

const INPUTS: &[&str] = &[
    "abc\n",
    "a  b   c\n",
    "text\n",
    "x=y=z\n",
    "\n",
    "",
    "no trailing newline",
    "foo:\n:\n",
    "\"quoted\" text\n",
    "shell function here\n",
    "- y\n",
    "aaa\nbbb\n",
    "a\tb\t\tc\n",
    "#comment\n",
    "/usr/local/bin/sh args\n",
    "12 345 6\n",
    "AZaz09_\n",
    "   \n",
    "a\n\n\nb\n",
];

/// `-E`, `-r` and `--regexp-extended` are the same switch and each was asserted
/// by nothing: a mutant inverting `--regexp-extended` to `ere = false` survived
/// the whole suite.
const OPTS: &[&[&str]] = &[&[], &["-e"], &["--expression"]];

/// …and the extended dialect, which needs its own scripts because `+` and `?`
/// are operators there and literals in a BRE.
const ERE_SCRIPTS: &[&str] = &["s/a+/X/", "s/a+/X/g", "s/[0-9]+/N/g", "s/ +/ /g", "s/a?b/Y/g"];

const ERE_OPTS: &[&[&str]] = &[&["-E"], &["-r"], &["--regexp-extended"]];

struct Ran {
    status: i32,
    stdout: Vec<u8>,
    spoke: bool,
}

fn run(bin: &str, args: &[String], input: &str) -> Result<Ran, String> {
    use std::os::unix::process::CommandExt;
    let mut child = Command::new(bin)
        // The C locale, pinned for the grep differential's reason: a range is
        // collation order to GNU and byte order here.
        .env("LC_ALL", "C")
        // The helper is a MULTICALL and picks its applet from `argv[0]`, so it
        // has to be CALLED `sed`; invoking it by its own path reaches the "no
        // applet" arm and reports 2 for everything, which would read as a
        // refusal and compare nothing at all.
        .arg0("sed")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn {bin}: {e}"))?;
    let bytes = input.as_bytes().to_vec();
    if let Some(mut sink) = child.stdin.take() {
        // A script that never reads (a refusal) closes stdin, and writing to it
        // then fails -- which is the refusal, not a harness error.
        let _ = sink.write_all(&bytes);
    }
    let out = child.wait_with_output().map_err(|e| format!("wait {bin}: {e}"))?;
    Ok(Ran {
        status: out.status.code().unwrap_or(-1),
        stdout: out.stdout,
        spoke: !out.stderr.is_empty(),
    })
}

struct Tally {
    compared: usize,
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
    // A refusal is never a correct outcome here: every script this is called
    // with is inside the served subset, so an applet that started refusing them
    // would otherwise sail through with nothing left to disagree about.
    if ours.status == 2 {
        tally.compared += 1;
        tally.mismatched += 1;
        println!("REFUSED (in-subset) args={args:?} input={input:?}");
        return Ok(());
    }
    let theirs = run(reference, args, input)?;
    tally.compared += 1;
    if ours.status != theirs.status || ours.stdout != theirs.stdout || ours.spoke != theirs.spoke {
        tally.mismatched += 1;
        println!(
            "MISMATCH args={args:?} input={input:?}\n  ours  = {:?} status {} spoke {}\n  theirs= {:?} status {} spoke {}",
            String::from_utf8_lossy(&ours.stdout),
            ours.status,
            ours.spoke,
            String::from_utf8_lossy(&theirs.stdout),
            theirs.status,
            theirs.spoke,
        );
    }
    Ok(())
}

/// A deterministic xorshift, so a failing run reproduces from its seed alone.
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

/// Pattern pieces the matcher serves, so a generated script stays in subset.
const PAT: &[&str] = &[
    "a", "b", "x", "=", " ", "\\t", "[0-9]", "[a-z]", "[^a]", "[[:space:]]", "[[:digit:]]",
    "\\s", "\\w", ".", "a*", "[0-9]*", "\\s\\+", "a\\+", "b\\?", "^a", "c$", "[ \\t]",
];

/// …and replacement pieces, `&` among them since it is the one that reads the
/// match back. The CHARACTER ESCAPES are here because their absence is why
/// 100000 generated rounds could not see `\a`/`\f`/`\v` being emitted as the
/// letter: a generator that never writes an escape cannot find one resolved
/// wrongly.
const REP: &[&str] =
    &["X", "", "&", "[&]", "-", "\\t", "\\n", "&&", "_", "y", "\\a", "\\f", "\\v", "\\r",
      "\\x41", "\\x4", "\\x", "\\&", "\\\\"];

/// The delimiters the corpus spells, plus `,` -- varied because `\<delim>` has
/// to become a literal delimiter, which is a per-delimiter rule.
const DELIMS: &[char] = &['/', ':', ';', ','];

fn generated(rng: &mut Rng) -> String {
    let d = rng.pick(DELIMS).copied().unwrap_or('/');
    let mut pat = String::new();
    for _ in 0..=rng.below(3) {
        let piece = rng.pick(PAT).copied().unwrap_or("a");
        // A piece containing the delimiter would end the field early and mean
        // something else; the escaped spelling is what the applet must resolve.
        match piece.contains(d) {
            true => pat.push('a'),
            false => pat.push_str(piece),
        }
    }
    let mut rep = String::new();
    for _ in 0..=rng.below(2) {
        let piece = rng.pick(REP).copied().unwrap_or("X");
        match piece.contains(d) {
            true => rep.push('X'),
            false => rep.push_str(piece),
        }
    }
    let g = match rng.below(2) {
        0 => "g",
        _ => "",
    };
    format!("s{d}{pat}{d}{rep}{d}{g}")
}

fn generated_input(rng: &mut Rng) -> String {
    let alphabet: &[u8] = b"abx= \t=09c\n";
    let mut out = String::new();
    for _ in 0..rng.below(5) {
        for _ in 0..rng.below(8) {
            let c = rng.pick(alphabet).copied().unwrap_or(b'a');
            out.push(char::from(c));
        }
        out.push('\n');
    }
    out
}

fn main() -> Result<(), String> {
    let argv: Vec<String> = std::env::args().collect();
    let (Some(reference), Some(helper)) = (argv.get(1), argv.get(2)) else {
        return Err("usage: sed_differential <reference-sed> <spec-helpers-binary> [rounds] [seed]"
            .to_string());
    };
    let rounds: usize = argv.get(3).and_then(|s| s.parse().ok()).unwrap_or(20_000);
    let seed: u64 = argv.get(4).and_then(|s| s.parse().ok()).unwrap_or(0x2026_0812);

    let mut wrongly_accepted = 0usize;
    for script in REFUSALS {
        let args = vec![(*script).to_string()];
        let ours = run(helper, &args, "abc\nx\n")?;
        if ours.status != 2 {
            wrongly_accepted += 1;
            println!("ACCEPTED (should refuse) {script:?} -> status {}", ours.status);
        }
    }
    println!("refusals: {} checked, {wrongly_accepted} wrongly accepted", REFUSALS.len());

    let mut tally = Tally { compared: 0, mismatched: 0 };
    for script in SCRIPTS {
        for input in INPUTS {
            for opt in OPTS {
                let mut args: Vec<String> = opt.iter().map(|o| (*o).to_string()).collect();
                args.push((*script).to_string());
                compare(reference, helper, &args, input, &mut tally)?;
            }
        }
    }
    // The extended dialect, which the BRE scripts above cannot reach.
    for script in ERE_SCRIPTS {
        for input in INPUTS {
            for opt in ERE_OPTS {
                let mut args: Vec<String> = opt.iter().map(|o| (*o).to_string()).collect();
                args.push((*script).to_string());
                compare(reference, helper, &args, input, &mut tally)?;
            }
        }
    }
    println!("sweep: {} compared, {} mismatched", tally.compared, tally.mismatched);

    // FILE OPERANDS, which the generated rounds cannot reach: they feed one
    // stdin stream, and a file's end is a LINE's end. Concatenating two
    // operands merged the first's unterminated last line into the second's
    // first, and nothing that reads a single stream could have seen it.
    let dir = std::env::temp_dir().join(format!("sed-differential-{seed}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).map_err(|e| format!("scratch dir: {e}"))?;
    let bodies: &[&str] = &["a", "a\n", "ab\ncd", "", "\n", "x=y\n", "p\nq\n"];
    for (n, first) in bodies.iter().enumerate() {
        for (m, second) in bodies.iter().enumerate() {
            let f1 = dir.join(format!("f{n}a"));
            let f2 = dir.join(format!("f{m}b"));
            std::fs::write(&f1, first).map_err(|e| format!("write: {e}"))?;
            std::fs::write(&f2, second).map_err(|e| format!("write: {e}"))?;
            for script in ["s/^/>/", "s/$/</", "s/a/X/g", "s/z*/-/g"] {
                let args = vec![
                    script.to_string(),
                    f1.to_string_lossy().into_owned(),
                    f2.to_string_lossy().into_owned(),
                ];
                compare(reference, helper, &args, "", &mut tally)?;
            }
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    println!("with file operands: {} compared, {} mismatched", tally.compared, tally.mismatched);

    let mut rng = Rng(seed | 1);
    for _ in 0..rounds {
        let script = generated(&mut rng);
        let input = generated_input(&mut rng);
        compare(reference, helper, &[script], &input, &mut tally)?;
    }
    println!(
        "with {rounds} random rounds (seed {seed}): {} compared, {} mismatched",
        tally.compared, tally.mismatched
    );
    match tally.mismatched == 0 && wrongly_accepted == 0 {
        true => Ok(()),
        false => Err(format!(
            "{} mismatches, {wrongly_accepted} wrongly accepted",
            tally.mismatched
        )),
    }
}

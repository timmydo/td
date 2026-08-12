//! Differential test: the `spec_helpers` head/tail/tac/od against the real ones.
//!
//!     cargo run --release --manifest-path td-sh/Cargo.toml \
//!         --example bytes_differential -- <reference-bin-dir> <spec-helpers-binary>
//!
//! NOT run by the gate, and deliberately, for `grep_differential`'s reason: it
//! needs real coreutils to compare against and the gate may have none, which is
//! an undeclared dependency. It is committed because it is the evidence behind
//! these four applets' claims, and evidence nobody can re-run is a claim nobody
//! can check.
//!
//! The reference is a DIRECTORY holding `head`, `tail`, `tac` and `od` -- pass
//! the GNU coreutils one rather than trusting PATH, which on the machine this
//! was written on has answered with a different program before. GNU coreutils
//! 9.1 is what the numbers were taken against.
//!
//! What is compared is stdout, the exit status, and whether anything was said on
//! stderr. Not stderr's TEXT: these applets report a failed open through Rust's
//! `io::Error`, which spells the same errno differently from GNU ("No such file
//! or directory (os error 2)"), and no corpus golden reads either. That an error
//! was reported at all is the part that matters and is checked.
//!
//! `od` is the one with a LAYOUT rather than a subset of the input, so it is
//! swept hardest: every address radix crossed with every served type list,
//! against inputs chosen to land on and either side of the sixteen-byte row --
//! and on runs of identical rows, which GNU collapses to a `*`.

#![forbid(unsafe_code)]

use std::io::Write;
use std::process::{Command, Stdio};

/// Inputs, as bytes rather than text: `od` exists to render the ones no string
/// can hold, and the applets are byte-oriented all the way through.
const INPUTS: &[&[u8]] = &[
    b"",
    b"\n",
    b"a",
    b"a\n",
    b"a\nb",
    b"one\ntwo\nthree\nfour\n",
    b"one\ntwo\nthree\nfour",
    b"\n\n\n",
    b"0123456789abcdef",
    b"0123456789abcdefg",
    b"0123456789abcdef0123456789abcdef",
    b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    b"aaaaaaaaaaaaaaaabbbbbbbbbbbbbbbbaaaaaaaaaaaaaaaa",
    b"a\tb\\c\x07\x08\x0c\x0b\r\x00d",
    b"\x00\x01\x1f\x7f\x80\xff",
    b" \x20\x7e\x21",
    b"\xc3\xa9\xe2\x82\xac",
    b"line with spaces\nand\ttabs\n",
];

/// `head` and `tail` share an option surface, so they share its sweep -- which
/// is the point of their sharing a parser.
const COUNTS: &[&[&str]] = &[
    &[],
    &["-c", "0"],
    &["-c", "1"],
    &["-c", "4"],
    &["-c", "5"],
    &["-c", "16"],
    &["-c", "999"],
    &["-n", "0"],
    &["-n", "1"],
    &["-n", "2"],
    &["-n", "3"],
    &["-n", "999"],
    &["-c4"],
    &["-n1"],
    &["--bytes", "4"],
    &["--lines", "2"],
    &["--bytes=3"],
    &["--lines=1"],
    // The LAST count given wins, and the two spellings of "both were given"
    // disagree about which that is.
    &["-c", "4", "-n", "2"],
    &["-n", "2", "-c", "4"],
];

/// Every served `od` shape: an address radix crossed with a type list. The
/// repeated and reversed lists are here because a type list is ORDERED and its
/// column width comes from the widest member -- `-t x1 -t c` and `-t c -t x1`
/// print the same two lines the other way up, and `-c -c` prints one twice.
const OD_ADDRS: &[&[&str]] = &[&[], &["-A", "n"], &["-A", "o"], &["-A", "d"], &["-A", "x"]];

const OD_TYPES: &[&[&str]] =
    &[&["-t", "x1"], &["-c"], &["-t", "c"], &["-t", "c", "-t", "x1"], &["-t", "x1", "-t", "c"],
      &["-c", "-c"], &["-tx1"], &["-tc"], &["-cc"], &["-c", "-t", "x1", "-c"]];

/// Arguments OUTSIDE the served subset. Each must come back status 2: a
/// plausible answer to one of these would be graded as the shell's output, which
/// is the whole reason the applets refuse rather than guess.
const REFUSALS: &[(&str, &[&str])] = &[
    // `tail -c +3` is GNU's "from byte 3 onward" -- the other end of the stream
    // from what a bare count means, and `usize::from_str` accepts the `+`.
    ("tail", &["-c", "+3"]),
    ("head", &["-c", "+3"]),
    ("tail", &["-n", "+2"]),
    ("head", &["-n", "-1"]),
    ("tail", &["-c", "-1"]),
    ("head", &["-c", "x"]),
    ("head", &["-c"]),
    ("head", &["-n"]),
    // The obsolete count spelling, which the two programs' own documentation
    // reads differently.
    ("head", &["-5"]),
    ("tail", &["-5"]),
    // Options with a real meaning nothing here implements.
    ("head", &["-q"]),
    ("head", &["-v"]),
    ("head", &["-z"]),
    ("tail", &["-f"]),
    ("tail", &["--follow"]),
    ("head", &["--silent"]),
    ("tac", &["-r"]),
    ("tac", &["-s", "x"]),
    ("tac", &["-b"]),
    // `od`'s type alphabet: every one of these has a width and a byte order.
    ("od", &["-A", "n", "-t", "x2"]),
    ("od", &["-A", "n", "-t", "x4"]),
    ("od", &["-A", "n", "-t", "x"]),
    ("od", &["-A", "n", "-t", "o2"]),
    ("od", &["-A", "n", "-t", "u1"]),
    ("od", &["-A", "n", "-t", "d2"]),
    ("od", &["-A", "n", "-t", "f4"]),
    ("od", &["-A", "n", "-t", "a"]),
    ("od", &["-A", "n", "-t", "xC"]),
    ("od", &["-A", "n", "-t", "x1c"]),
    ("od", &["-A", "n", "-b"]),
    ("od", &["-A", "n", "-x"]),
    ("od", &["-A", "n", "-d"]),
    ("od", &["-A", "n", "-o"]),
    ("od", &["-A", "n", "-v"]),
    ("od", &["-A", "n", "-w2"]),
    ("od", &["-A", "n", "-N", "2"]),
    ("od", &["-A", "n", "-j", "1"]),
    ("od", &["-A", "z", "-t", "x1"]),
    ("od", &["-A", "-t", "x1"]),
    ("od", &["--format=x1"]),
    // A bare `od` is GNU's `-t o2`, a layout nothing in the corpus grades.
    ("od", &[]),
    ("od", &["-A", "n"]),
];

struct Ran {
    status: i32,
    stdout: Vec<u8>,
    spoke: bool,
}

fn run(bin: &str, applet: &str, args: &[String], input: &[u8]) -> Result<Ran, String> {
    use std::os::unix::process::CommandExt;
    let mut child = Command::new(bin)
        // The helper is a MULTICALL and picks its applet from `argv[0]`, so
        // invoking it by its own path would reach the "no applet" arm and
        // report 2 for everything -- which this harness would then count as a
        // refusal and compare nothing at all.
        .arg0(applet)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("{bin}: {e}"))?;
    if let Some(mut w) = child.stdin.take() {
        // A reference `head -c 4` stops reading at its fourth byte, so a refused
        // write is that program's answer rather than an error.
        let _ = w.write_all(input);
    }
    let out = child.wait_with_output().map_err(|e| format!("{bin}: {e}"))?;
    Ok(Ran {
        status: out.status.code().unwrap_or(-1),
        stdout: out.stdout,
        spoke: !out.stderr.is_empty(),
    })
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

/// Bytes weighted towards the ones that are a CASE rather than a value:
/// newlines (every record boundary), NUL and the escapes `od -c` names, and the
/// high half no text encoding covers.
fn generated_input(rng: &mut Rng) -> Vec<u8> {
    let mut out = Vec::new();
    for _ in 0..rng.below(70) {
        out.push(match rng.below(10) {
            0..=3 => b'\n',
            4 => 0x00,
            5 => match rng.pick(b"\x07\x08\x09\x0b\x0c\x0d\\") {
                Some(b) => *b,
                None => b'\\',
            },
            6 => u8::try_from(0x80 + rng.below(0x80)).unwrap_or(0xff),
            _ => u8::try_from(0x20 + rng.below(0x5f)).unwrap_or(b'a'),
        });
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
    applet: &str,
    args: &[String],
    input: &[u8],
    may_refuse: bool,
    tally: &mut Tally,
) -> Result<(), String> {
    let ours = run(helper, applet, args, input)?;
    // A refusal is only a correct outcome where the SHAPE is outside the served
    // subset. Everywhere else it is a mismatch, and it has to be counted as one:
    // every option set the sweeps below draw from is inside the subset, so an
    // applet that started refusing `--bytes=3` -- or one that refused
    // everything -- would otherwise sail through with zero disagreements,
    // because there is nothing left to compare. Only the multi-operand sets in
    // `sweep_files` are allowed it, and only for the three applets that refuse
    // a second operand by design.
    if ours.status == 2 {
        tally.refused += 1;
        if !may_refuse {
            tally.mismatched += 1;
            println!("REFUSED (in-subset) {applet} args={args:?} input={:?}",
                     String::from_utf8_lossy(input));
        }
        return Ok(());
    }
    let theirs = run(&format!("{reference}/{applet}"), applet, args, input)?;
    tally.compared += 1;
    if (theirs.status, &theirs.stdout, theirs.spoke) != (ours.status, &ours.stdout, ours.spoke) {
        tally.mismatched += 1;
        if tally.mismatched <= 20 {
            println!("MISMATCH {applet} args={args:?} input={:?}", String::from_utf8_lossy(input));
            println!(
                "  reference -> {} spoke={} {:?}",
                theirs.status,
                theirs.spoke,
                String::from_utf8_lossy(&theirs.stdout)
            );
            println!(
                "  ours      -> {} spoke={} {:?}",
                ours.status,
                ours.spoke,
                String::from_utf8_lossy(&ours.stdout)
            );
        }
    }
    Ok(())
}

/// The multi-file half, which needs files rather than a pipe. `head`'s banner is
/// the only thing in these four applets whose SHAPE comes from the operand list,
/// and `redirect-multi`'s goldens carry it verbatim -- including the case where
/// one of the named files does not exist, which those cases reach by globbing a
/// name the shell may not have created.
fn sweep_files(reference: &str, helper: &str, tally: &mut Tally) -> Result<(), String> {
    let dir = std::env::temp_dir().join(format!("bytes-differential-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let files: &[(&str, &[u8])] = &[
        ("a", b"A1\nA2\nA3\n"),
        ("b", b"B1\n"),
        ("empty", b""),
        ("nonl", b"no-newline"),
        ("long", b"1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n11\n12\n"),
    ];
    for (name, body) in files {
        std::fs::write(dir.join(name), body).map_err(|e| format!("{name}: {e}"))?;
    }
    // `missing` is deliberately never created.
    let operand_sets: &[&[&str]] = &[
        &["a"],
        &["a", "b"],
        &["a", "b", "nonl"],
        &["a", "empty", "b"],
        &["empty", "empty"],
        &["missing"],
        &["missing", "a"],
        &["a", "missing"],
        &["a", "missing", "b"],
        &["nonl", "a"],
        &["long", "a"],
        // `-` is stdin among named files, which GNU banners as `standard
        // input` rather than as the dash that asked for it.
        &["-", "a"],
        &["a", "-"],
    ];
    for applet in ["head", "tail", "tac", "od"] {
        for operands in operand_sets {
            for opts in COUNTS {
                // Only `head` and `tail` take a count; the other two get the
                // operands alone, which is the whole of their surface.
                if !matches!(applet, "head" | "tail") && !opts.is_empty() {
                    continue;
                }
                let mut args: Vec<String> = opts.iter().map(|o| (*o).to_string()).collect();
                if applet == "od" {
                    args.push("-A".into());
                    args.push("n".into());
                    args.push("-t".into());
                    args.push("x1".into());
                }
                args.push("--".to_string());
                args.extend(operands.iter().map(|name| match *name {
                    // `-` is stdin, not a file in the scratch directory.
                    "-" => "-".to_string(),
                    _ => dir.join(name).to_string_lossy().into_owned(),
                }));
                // `head` serves any number of operands; the other three refuse a
                // second one, so that -- and only that -- is a licensed refusal.
                let may_refuse = applet != "head" && operands.len() > 1;
                compare(reference, helper, applet, &args, b"", may_refuse, tally)?;
            }
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let (Some(reference), Some(helper)) = (argv.first(), argv.get(1)) else {
        return Err("usage: bytes_differential <reference-bin-dir> <spec-helpers-binary> \
                    [rounds] [seed]"
            .into());
    };
    let rounds: usize = argv.get(2).and_then(|s| s.parse().ok()).unwrap_or(20_000);
    let seed: u64 = argv.get(3).and_then(|s| s.parse().ok()).unwrap_or(0x2026_0811);

    let mut tally = Tally { compared: 0, refused: 0, mismatched: 0 };

    // --- the refusal contract --------------------------------------------
    let mut wrongly_accepted = 0usize;
    for (applet, args) in REFUSALS {
        let args: Vec<String> = args.iter().map(|a| (*a).to_string()).collect();
        let ours = run(helper, applet, &args, b"abc\ndef\n")?;
        if ours.status != 2 {
            wrongly_accepted += 1;
            println!("ACCEPTED (should refuse) {applet} {args:?} -> status {}", ours.status);
        }
    }
    println!("refusals: {} checked, {wrongly_accepted} wrongly accepted", REFUSALS.len());

    // --- the systematic sweep --------------------------------------------
    for input in INPUTS {
        for applet in ["head", "tail"] {
            for opts in COUNTS {
                let args: Vec<String> = opts.iter().map(|o| (*o).to_string()).collect();
                compare(reference, helper, applet, &args, input, false, &mut tally)?;
            }
        }
        compare(reference, helper, "tac", &[], input, false, &mut tally)?;
        for addr in OD_ADDRS {
            for types in OD_TYPES {
                let mut args: Vec<String> = addr.iter().map(|o| (*o).to_string()).collect();
                args.extend(types.iter().map(|o| (*o).to_string()));
                compare(reference, helper, "od", &args, input, false, &mut tally)?;
            }
        }
    }
    sweep_files(reference, helper, &mut tally)?;
    println!(
        "sweep: {} compared, {} refused, {} mismatched",
        tally.compared, tally.refused, tally.mismatched
    );

    // --- the randomised sweep --------------------------------------------
    let mut rng = Rng(seed | 1);
    for _ in 0..rounds {
        let input = generated_input(&mut rng);
        let applet = rng.pick(&["head", "tail", "tac", "od"]).copied().unwrap_or("od");
        let mut args: Vec<String> = Vec::new();
        match applet {
            "head" | "tail" => {
                if let Some(opts) = rng.pick(COUNTS) {
                    args.extend(opts.iter().map(|o| (*o).to_string()));
                }
            }
            "od" => {
                if let Some(addr) = rng.pick(OD_ADDRS) {
                    args.extend(addr.iter().map(|o| (*o).to_string()));
                }
                if let Some(types) = rng.pick(OD_TYPES) {
                    args.extend(types.iter().map(|o| (*o).to_string()));
                }
            }
            _ => {}
        }
        compare(reference, helper, applet, &args, &input, false, &mut tally)?;
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

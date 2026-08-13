//! What do the corpus's REMAINING failures actually need?
//!
//! The overlay says how many cases fail; it does not say what would fix them,
//! and that is the question every increment on this workstream starts with.
//! Guessing it wrong is expensive in the direction that does not show up: the
//! externals looked like the obvious seam right up until staging all nine
//! remaining ones was measured at 13 cases between them, less than `sed` alone
//! had just delivered.
//!
//! So this runs every case the overlay GRADES -- its skips are not run, both
//! because their verdict here is a fact about the host and because running one
//! creates the shared `/tmp` state it is skipped for -- and buckets the
//! failures by what the failure looks like: the status td-sh returned where it
//! differed, and where in the corpus the case lives. The ones that came back
//! 127 are asked which name they wanted.
//!
//! It is a census, not a verdict, and its numbers are APPROXIMATE IN BOTH
//! DIRECTIONS -- which is the part to keep hold of, because only one direction
//! is obvious. Over: a case can want a missing name and be wrong about
//! something else too, and where the name has to be guessed from the source,
//! every command-position word of a failing case is counted rather than the one
//! that was not found. Under: the guess only sees cases whose STATUS was 127
//! and only words this scanner reads as a command, so a name can matter more
//! than it ranks. `ls` sits at 2 here and staging it unblocks 6. What settles a
//! number is staging the thing and re-running, as `sed`'s 14 was settled before
//! a line of it was written.
//!
//! A ranking is also not a list of things WORTH fixing, which is the trap this
//! tool was quiet about long enough to cost two increments. A file that names
//! no ash golden -- most of them -- is graded on dash's, because the chain falls
//! through; where the two shells disagree, matching that golden means breaking
//! ash, and ash is the one td-sh follows. It has happened twice with a whole
//! cluster at the top of the ranking. `redirect.test.sh` wants status 2 for a
//! failed redirection and dash agrees, but `toysh-posix` records
//! `OK ash status: 1` beside `OK dash status: 2` for the same failure. Taking
//! it promoted 12 cases and broke 5, and every one of the 5 was ash-graded.
//! `builtin-umask.test.sh` wants dash's symbolic parser, and
//! `conformance.rs`'s `umask_is_ashs_including_the_symbolic_form` holds values
//! measured on busybox 1.37.0 that contradict it clause by clause.
//!
//! So the ranking is SPLIT, on two axes. First by the identity each case is
//! graded as; then by whether every field that DIFFERED came from a block of
//! that identity's own, rather than from the unqualified one. Only the first is
//! a recorded answer from the shell. The unqualified block is osh's ideal, and
//! a shell the file compared saying nothing about it does not mean it agrees --
//! `effective_chain` in `lib.rs` argues that at length, and this corpus refutes
//! the stronger reading outright.
//!
//! Per FIELD because that is how the golden resolves: a case can carry an ash
//! `stderr` block, take its stdout from the default, and fail on the stdout,
//! which is a failure against the ideal however the case as a whole is
//! designated. Asking per case instead put six of those in the wrong column.
//!
//! That second axis is not a technicality. `builtin-trap-err.test.sh` leads the
//! ash column at 17, and 14 of those are the ideal: the whole bash ERR trap,
//! held against a shell with no block in that file to say it has one. The 36
//! cases across the corpus where ash's OWN block is the golden of every field
//! that differed are the ones nothing needs to be assumed about.
//!
//! Before taking a cluster from any other bucket, find out what ash does -- the
//! corpus sometimes records it in a neighbouring file, and this crate's own
//! busybox-measured tests are the other place to look.
//!
//! ```text
//! cargo build --release --manifest-path td-sh/Cargo.toml --bins --examples
//! ./td-sh/target/release/examples/gap_census \
//!   td-sh/target/release/td-sh td-sh/target/release/spec_helpers td-sh/spec
//! ```

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use td_sh::{
    annotates_field, case_keys, designates, graded_identity, parse_spec, run_case,
    spec_paths, ASH_DASH_CHAIN,
};

/// The identity half of a bucket key for a case designating neither chain
/// shell, whose golden is therefore always the unqualified block -- osh's
/// ideal, which is bash-shaped and mostly out of scope for this shell.
const NEITHER: &str = "neither";

/// How a bucket key reads. The two halves are the two axes: which identity the
/// golden is resolved for, and whether every field that DIFFERED came from that
/// identity's own block rather than the unqualified one.
fn label((id, own): (&str, bool)) -> String {
    let from = match own {
        true => "its own block",
        false => "the ideal",
    };
    format!("{id} ({from})")
}

/// The command a `not found` diagnostic NAMES, where the golden asserted stderr
/// and so kept it.
///
/// Only some cases carry it -- a golden that asserts stdout alone leaves stderr
/// out of `detail` -- but where it is there it is the exact answer the word
/// scan below can only approximate, so it is preferred over that scan.
fn named_by_stderr(detail: &str) -> Option<String> {
    let at = detail.find("stderr:")?;
    let rest = detail.get(at..)?;
    let mark = rest.find("td-sh: ")?;
    let tail = rest.get(mark + "td-sh: ".len()..)?;
    let end = tail.find(": not found")?;
    let name = tail.get(..end)?;
    match name.is_empty() || name.contains(char::is_whitespace) {
        true => None,
        false => Some(name.to_string()),
    }
}

/// The words a case uses in COMMAND position, approximately.
///
/// Approximately because doing it exactly means parsing the shell, which is the
/// thing under test. A word is taken where a command can start: at the top of a
/// line, or after one of the operators that ends the previous command. Only
/// plain names are kept -- no assignment, no redirect, no expansion -- because
/// the question this answers is which NAME a case wanted, and a name is what
/// the shell would have looked up.
fn command_words(code: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in code.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        for piece in line.split(['|', ';', '&']) {
            let piece = piece.trim_start_matches(|c: char| c.is_whitespace() || c == '(');
            // The command is the first word that is not an ASSIGNMENT: `IFS=x
            // cmd` runs `cmd`. Skipping them has to happen on whole tokens,
            // because a scan that stops at the `=` reads `IFS` as the command
            // and never reaches `cmd` -- which is what this did, and why
            // `PATH`, `HOME`, `IFS` and `PWD` were ranked as names the shell
            // could not find.
            let Some(token) = piece.split_whitespace().find(|t| !t.contains('=')) else {
                continue;
            };
            let word: String = token
                .chars()
                .take_while(|c| {
                    c.is_ascii_alphanumeric()
                        || matches!(c, '_' | '-' | '.' | '/')
                })
                .collect();
            // A bare `.` or `/` is a path fragment this scan cannot read as a
            // name, not a command.
            if word.is_empty() || word.chars().all(|c| c == '.' || c == '/') {
                continue;
            }
            // Keywords are not names the shell looks up.
            if matches!(word.as_str(), "if" | "then" | "else" | "elif" | "fi" | "for" | "while"
                                     | "until" | "do" | "done" | "case" | "esac" | "in"
                                     | "function" | "select" | "time" | "echo" | "true"
                                     | "false" | "cd" | "set" | "unset" | "export" | "read"
                                     | "shift" | "return" | "exit" | "eval" | "test") {
                continue;
            }
            out.push(word);
        }
    }
    out.sort();
    out.dedup();
    out
}

/// The status td-sh returned, read back out of the mismatch text.
///
/// Out of the TEXT because that is all a `CaseOutcome` carries. `None` means
/// the status MATCHED and only bytes differed; 127 is the `not found` status,
/// which is why that bucket is the missing-name one.
fn got_status(detail: &str) -> Option<i32> {
    // ANCHORED, not searched: `evaluate` pushes the status line first, so it is
    // at the start when it is there at all. Searching found the marker inside a
    // quoted stdout too -- an expected output that happens to contain
    // `status: expected 3, got 9` parsed as a status of 9.
    let rest = match detail.starts_with("status: expected ") {
        true => detail,
        false => return None,
    };
    let got = rest.find(", got ")?;
    let tail = rest.get(got + ", got ".len()..)?;
    // A leading `-` is part of the number: the harness reports a
    // signal-terminated shell as -1, and reading only digits made
    // `got -1` look like no status difference at all.
    let mut seen = String::new();
    for (n, c) in tail.chars().enumerate() {
        match c {
            '-' if n == 0 => seen.push(c),
            c if c.is_ascii_digit() => seen.push(c),
            _ => break,
        }
    }
    seen.parse().ok()
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let usage = "usage: gap_census <shell-binary> <spec-helpers-binary> <spec-dir>";
    let shell = PathBuf::from(args.next().ok_or(usage)?);
    let helpers = PathBuf::from(args.next().ok_or(usage)?);
    let dir = PathBuf::from(args.next().ok_or(usage)?);
    let shell = std::fs::canonicalize(&shell)?;
    let helpers = std::fs::canonicalize(&helpers)?;

    // The overlay's `skip` set, which this must NOT run. A skip is a case the
    // overlay records as unevaluatable HERE -- it reads the Oils repo tree, or
    // depends on a shared `/tmp` path, or measures syscalls -- and running one
    // does two things this tool has no business doing. It grades a case whose
    // verdict is a fact about the host, so 22 of them counted as passes and 76
    // as failures, a fifth of the 127 bucket among them; and it CREATES the
    // shared state, leaving `/tmp/spam`, `/tmp/mv` and friends behind on the
    // build host. `gen_expectations` filters them before running for exactly
    // that reason, and this reuses its recorded decision rather than
    // re-deriving it.
    let overlay = dir.join("expectations.txt");
    let text = std::fs::read_to_string(&overlay)
        .map_err(|e| format!("{}: {e}", overlay.display()))?;
    let skipped: BTreeSet<String> = text
        .lines()
        .filter_map(|l| l.strip_prefix("skip "))
        .map(|k| k.trim().to_string())
        .collect();
    if skipped.is_empty() {
        return Err(format!("{} lists no skips; refusing to census without it \
                            (it is the set this must not run)", overlay.display())
            .into());
    }

    // Split by the identity each case is GRADED as, which is the one thing the
    // ranking below cannot say and the one that decides whether a cluster is
    // reachable at all. See the note at the head of this file.
    // Keyed by (identity, its-own-block) so the ORDER below is the tuple's and
    // no label has to be parsed back out of a string.
    let mut per_file_by_id: BTreeMap<(&str, bool), BTreeMap<String, usize>> = BTreeMap::new();
    let mut by_identity: BTreeMap<(&str, bool), usize> = BTreeMap::new();
    let mut wanted: BTreeMap<String, usize> = BTreeMap::new();
    let mut named: BTreeMap<String, usize> = BTreeMap::new();
    let mut buckets: BTreeMap<String, usize> = BTreeMap::new();
    let mut failures: Vec<String> = Vec::new();
    let mut ran = 0usize;
    let mut passed = 0usize;

    for path in &spec_paths(&dir)? {
        let file = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string();
        let text = std::fs::read_to_string(path)?;
        // A file that will not parse, or a case that will not RUN, is not a
        // gap in the shell -- it is this tool failing to look. Both were
        // skipped silently, so a census over a broken tree printed a confident
        // ranking of nothing at all and exited 0. They are counted and fatal
        // now, for the reason `gen_expectations` aborts rather than emitting a
        // partial overlay: a measurement nobody can tell is incomplete is
        // worse than no measurement.
        let cases = match parse_spec(&text) {
            Ok(c) => c,
            Err(e) => {
                failures.push(format!("parse-error {file}: {e}"));
                continue;
            }
        };
        for (case, key) in cases.iter().zip(case_keys(&file, &cases)) {
            if skipped.contains(&key) {
                continue;
            }
            let outcome = match run_case(&shell, &helpers, case, ASH_DASH_CHAIN) {
                Ok(o) => o,
                Err(e) => {
                    failures.push(format!("run-error {key}: {e}"));
                    continue;
                }
            };
            ran += 1;
            if outcome.passed {
                passed += 1;
                continue;
            }
            // Three identities, not two: `graded_identity` falls back to the
            // chain head, so a case designating NEITHER shell also reports
            // "ash". And each is split again by where the golden came from,
            // because being graded as ash is not the same as ash having said
            // anything -- `effective_chain` spells out why silence is not
            // agreement, and 14 of `builtin-trap-err`'s 17 are the bash ERR
            // trap held against a shell with no block in that file.
            let id = match graded_identity(case, ASH_DASH_CHAIN) {
                Some(id) if designates(case, id) => id,
                _ => NEITHER,
            };
            // Per FIELD, because that is how the golden resolves and how a
            // mismatch is reported: a case carrying an ash `stderr` block and
            // failing on its stdout is measured against the ideal, whatever the
            // case as a whole designates. Every differing field has to be the
            // shell's own for the failure to be one.
            let own = id != NEITHER
                && !outcome.mismatched.is_empty()
                && outcome.mismatched.iter().all(|f| annotates_field(case, id, *f));
            *by_identity.entry((id, own)).or_default() += 1;
            *per_file_by_id.entry((id, own)).or_default().entry(file.clone()).or_default() += 1;
            if outcome.timed_out || outcome.truncated {
                *buckets.entry("not evaluated (timeout/cap)".into()).or_default() += 1;
                continue;
            }
            let detail = outcome.detail.unwrap_or_default();
            if got_status(&detail) == Some(127) {
                // Where the golden asserted stderr, the diagnostic NAMES the
                // command and there is nothing to approximate.
                match named_by_stderr(&detail) {
                    Some(name) => {
                        *named.entry(name).or_default() += 1;
                    }
                    None => {
                        for word in command_words(&case.code) {
                            *wanted.entry(word).or_default() += 1;
                        }
                    }
                }
            }
            let what = match got_status(&detail) {
                // 127 is the `not found` status and the shell produces it for
                // nothing else, so this bucket IS the missing-name one.
                Some(127) => "a name that is not there (127)".to_string(),
                // The shell's own error status: a syntax error, or a builtin
                // refusing its arguments.
                Some(2) => "the shell reported an error (2)".to_string(),
                Some(n) => format!("a different status ({n})"),
                None => "the right status, different bytes".to_string(),
            };
            *buckets.entry(what).or_default() += 1;
        }
    }

    println!("{ran} cases run, {passed} pass, {} fail\n", ran - passed);
    println!("BY WHAT THE FAILURE LOOKS LIKE");
    let mut rows: Vec<(&String, &usize)> = buckets.iter().collect();
    rows.sort_by_key(|(_, n)| std::cmp::Reverse(**n));
    for (what, n) in rows {
        println!("  {n:5}  {what}");
    }
    println!("\nWHICH SHELL THE GOLDEN CAME FROM");
    // Two axes, and the second is the one that stops this being read as a
    // to-do list. "designates", not "ran": a case designates a shell by its
    // file's header OR by carrying a block for it, and 31 (file, id) pairs here
    // do it the second way. Then `its own block` versus `the ideal`: only the
    // first is a recorded answer FROM that shell. The ideal is osh's, and
    // silence from a shell the file compared says nothing about whether it
    // agrees -- `effective_chain` in lib.rs is where that is argued.
    println!("  ash      designates ash. `its own block' means every field that");
    println!("           DIFFERED resolved to a block of ash's own, so it is a");
    println!("           recorded gap; `the ideal' is only what ash was measured");
    println!("           against, and silence is not agreement");
    println!("  dash     designates dash and NOT ash, so the chain fell through --");
    println!("           reachable where the shells agree, a trap where they do");
    println!("           not, and split the same two ways");
    println!("  {NEITHER}  designates neither, so the golden is always the ideal");
    let mut ids: Vec<(&(&str, bool), &usize)> = by_identity.iter().collect();
    ids.sort_by_key(|(_, n)| std::cmp::Reverse(**n));
    for (key, n) in ids {
        println!("  {n:5}  graded as {}", label(*key));
    }
    // ash before dash before neither, and each shell's own block before the
    // ideal -- the order the tuple key already has.
    let mut order: Vec<(&(&str, bool), &BTreeMap<String, usize>)> =
        per_file_by_id.iter().collect();
    order.sort_by_key(|((id, own), _)| {
        let head = ASH_DASH_CHAIN.iter().position(|c| c == id).unwrap_or(ASH_DASH_CHAIN.len());
        (head, !*own, *id)
    });
    for (key, files) in order {
        println!("\nWHERE THEY ARE, graded as {}, most-failing file first", label(*key));
        let mut files: Vec<(&String, &usize)> = files.iter().collect();
        files.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
        let shown = 15.min(files.len());
        for (name, n) in files.iter().take(shown) {
            println!("  {n:5}  {name}");
        }
        if let Some(rest) = files.len().checked_sub(shown).filter(|r| *r > 0) {
            let tail: usize = files.iter().skip(shown).map(|(_, n)| **n).sum();
            println!("  {tail:5}  ({rest} further files)");
        }
    }
    println!("\nNAMES THE DIAGNOSTIC ITSELF GAVE (goldens that assert stderr)");
    let mut exact: Vec<(&String, &usize)> = named.iter().collect();
    exact.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
    match exact.is_empty() {
        true => println!("  (none)"),
        false => {
            for (name, n) in &exact {
                println!("  {n:5}  {name}");
            }
        }
    }
    println!("\nAND THE REST, GUESSED FROM COMMAND-POSITION WORDS");
    println!("  (a case counts for EVERY name it uses, so a staged helper like");
    println!("   `mkdir` ranks here for co-occurring, not for being missing)");
    let mut names: Vec<(&String, &usize)> = wanted.iter().collect();
    names.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
    for (name, n) in names.iter().take(30) {
        println!("  {n:5}  {name}");
    }
    if !failures.is_empty() {
        for line in failures.iter().take(20) {
            eprintln!("{line}");
        }
        return Err(format!(
            "{} case(s)/file(s) could not be measured; the census above is \
             INCOMPLETE and its numbers are not a census of anything",
            failures.len()
        )
        .into());
    }
    Ok(())
}

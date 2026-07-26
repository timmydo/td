//! Regenerate `spec/expectations.txt`, the GENERATED xfail/skip overlay for the
//! td-txt conformance corpus. Committed so the overlay is reproducible rather
//! than a hand-edited artifact: any maintainer can re-derive it after a pin bump
//! or as td-txt gains coverage, and review can diff a regenerated file against
//! the commit.
//!
//! Usage (build the multicall first, then point the generator at it and the corpus):
//!   cargo build --release --manifest-path td-txt/Cargo.toml
//!   cargo run --release --manifest-path td-txt/Cargo.toml \
//!     --example gen_expectations -- \
//!     td-txt/target/release/td-txt td-txt/spec > td-txt/spec/expectations.txt
//!
//! It runs every case through the built td-txt under the SAME isolated harness
//! the gate uses (`td_txt::run_case`) and buckets each:
//!   pass  -> unlisted.
//!   skip  -> the case TIMED OUT, so running it costs the shared gate ten
//!            seconds for a result already known to be wrong.
//!   xfail -> any other wrong answer.
//! Keys are occurrence-qualified via `td_txt::case_keys`, so a case name that
//! repeats within one corpus file is addressable per-occurrence.
//!
//! Fail-closed: a corpus load error or a per-case run error is collected and, if
//! any occurred, the tool prints them and exits NON-ZERO WITHOUT emitting the
//! overlay — a broken regeneration never silently overwrites the committed file
//! with partial output.
//!
//! This is dev tooling, not shipped code, but it is held to td's defensive style
//! (no unwrap/expect/panic, `.get` over indexing) so clippy stays clean.

use std::path::PathBuf;
use td_txt::{case_keys, load_corpus, run_case};

const HEADER: &str = "\
# td-txt conformance expectations overlay
#
# td-txt's known-gap manifest for the corpus in this directory (see spec/README).
# Kept out of the case files so the vendored GNU suites stay byte-for-byte
# pristine.
#
# GENERATED, not hand-curated, by examples/gen_expectations.rs: every case is run
# once through the built td-txt in the isolated harness and bucketed. A passing
# case is unlisted; every non-passing case is listed here so the gate stays green
# while td-txt grows. As td-txt improves, a listed case that starts passing reds
# the gate as an XPASS so it is promoted (removed here). Regenerate (do not
# hand-edit) on a pin bump or when passes change:
#   cargo build --release --manifest-path td-txt/Cargo.toml
#   cargo run --release --manifest-path td-txt/Cargo.toml --example gen_expectations \\
#     -- td-txt/target/release/td-txt td-txt/spec > td-txt/spec/expectations.txt
#
# Format, one per line:   <xfail|skip> <corpus-file>::<case name>
#   xfail = td-txt runs it and gets the wrong answer today. It still RUNS every
#           gate; the failure is tolerated, a regression (an unlisted case that
#           fails) reds the gate, and an unexpected PASS (XPASS) reds it so the
#           entry is promoted.
#   skip  = not run at all, because the case TIMED OUT: it would cost the shared
#           gate the full per-case timeout for an answer already known to be
#           wrong.
";

fn main() -> std::process::ExitCode {
    let mut args = std::env::args_os().skip(1);
    let (Some(bin), Some(spec)) = (args.next(), args.next()) else {
        eprintln!("usage: gen_expectations <td-txt binary> <spec dir>");
        return std::process::ExitCode::from(2);
    };
    let bin = PathBuf::from(bin);
    let spec = PathBuf::from(spec);

    let cases = match load_corpus(&spec) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("gen_expectations: cannot load the corpus: {e}");
            return std::process::ExitCode::from(1);
        }
    };
    let keys = case_keys(&cases);

    let mut lines: Vec<String> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    let mut pass = 0usize;
    for (case, key) in cases.iter().zip(keys) {
        match run_case(&bin, case) {
            Ok(outcome) if outcome.passed => pass += 1,
            Ok(outcome) if outcome.timed_out => lines.push(format!("skip {key}")),
            Ok(_) => lines.push(format!("xfail {key}")),
            Err(e) => errors.push(format!("{key}: {e}")),
        }
    }

    if !errors.is_empty() {
        eprintln!("gen_expectations: {} case(s) could not be run:", errors.len());
        for e in &errors {
            eprintln!("  {e}");
        }
        eprintln!("no overlay emitted");
        return std::process::ExitCode::from(1);
    }

    print!("{HEADER}");
    if lines.is_empty() {
        println!("#");
        println!("# EMPTY: td-txt passes every case in the corpus today.");
    }
    for line in &lines {
        println!("{line}");
    }
    eprintln!(
        "gen_expectations: {pass} pass, {} listed ({} cases total)",
        lines.len(),
        cases.len()
    );
    std::process::ExitCode::SUCCESS
}

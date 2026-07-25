//! Regenerate `spec/expectations.txt`, the GENERATED xfail/skip overlay for the
//! vendored Oils spec corpus. Committed so the overlay is reproducible rather than
//! a hand-edited artifact: any maintainer can re-derive it after a pin bump or as
//! td-sh gains coverage, and review can diff a regenerated file against the commit.
//!
//! Usage (build the shell first, then point the generator at it and the corpus):
//!   cargo build --release --manifest-path td-sh/Cargo.toml
//!   cargo run --release --manifest-path td-sh/Cargo.toml \
//!     --example gen_expectations -- \
//!     target/release/td-sh td-sh/spec > td-sh/spec/expectations.txt
//!
//! It runs every case in <spec-dir> through the built td-sh under the SAME isolated
//! `-c` harness the gate uses (`td_sh::run_case`), and buckets each case:
//!   pass  -> unlisted, UNLESS its comment-stripped code reads the repo tree (see
//!            `reads_repo_tree`): a pass there is a probable false-green because the
//!            isolated cwd stages no tree, so those are emitted as `skip`.
//!   skip  -> cannot be evaluated faithfully here (needs the `argv.py` helper, timed
//!            out per the typed `CaseOutcome::timed_out`, or is repo-tree/fixture bound).
//!   xfail -> any other genuine wrong-answer failure (includes status-127
//!            missing-external failures under the cleared-env harness).
//! Keys are occurrence-qualified via `td_sh::case_keys`, so a description repeated
//! within a file is addressable per-occurrence and never collapses two cases onto
//! one entry. `spec_paths` is shared with the gate so both enumerate the same files.
//!
//! Fail-closed: parse errors, per-case run errors, and overlay-key collisions are
//! collected and, if any occurred, the tool prints them and exits NON-ZERO WITHOUT
//! emitting the overlay — a broken regeneration never silently overwrites the
//! committed file with partial output.
//!
//! This is dev tooling, not shipped code, but it is held to td's defensive style
//! (no unwrap/expect/panic, `.get` over indexing) so `clippy --all-targets` stays
//! clean.
use std::collections::BTreeSet;
use std::path::PathBuf;
use td_sh::{case_keys, parse_spec, run_case, spec_paths, ASH_DASH_CHAIN};

// Header emitted verbatim atop the overlay. Documents the xfail/skip contract for
// a maintainer reading `expectations.txt` directly; kept in sync with this tool.
const HEADER: &str = "\
# td-sh conformance expectations overlay
#
# td-sh's known-gap manifest for the VENDORED Oils spec corpus. Kept out of the
# spec files so the corpus stays byte-for-byte pristine (see spec/README).
#
# GENERATED, not hand-curated, by examples/gen_expectations.rs: every case is run
# once through the built td-sh in the isolated `-c` harness and bucketed. A passing
# case is unlisted; every non-passing case is listed here so the gate stays green
# while td-sh grows. As td-sh improves, a listed case that starts passing reds the
# gate as an XPASS so it is promoted (removed here). Regenerate (do not hand-edit)
# on a pin bump or when passes change:
#   cargo build --release --manifest-path td-sh/Cargo.toml
#   cargo run --release --manifest-path td-sh/Cargo.toml --example gen_expectations \\
#     -- target/release/td-sh td-sh/spec > td-sh/spec/expectations.txt
#
# Format, one per line:   <xfail|skip> <spec-file>::<case description>
#   xfail = td-sh runs it and gets the wrong answer today. It still RUNS every
#           gate; the failure is tolerated, a regression (an unlisted case that
#           fails) reds the gate, and an unexpected PASS (XPASS) reds it so the
#           entry is promoted. NOTE on honesty at this scale: many xfails are
#           status-127 `command not found` failures, because the cleared-env `-c`
#           harness exposes no external PATH and no `argv.py`. Those are real
#           mismatches today, but they measure the harness's minimalism as much as
#           td-sh; a PATH/externals + argv.py rig (deferred) would convert a large
#           block of them into true builtin/parser pass-fail signal.
#   skip  = not run at all, because it cannot be evaluated faithfully here:
#           (a) it needs a facility the `-c` harness does not provide (the Oils
#               `argv.py` helper), detected by the code mentioning `argv.py`;
#           (b) td-sh hangs/loops on it, so it would time out (10s) every gate;
#           (c) it depends on the Oils repo tree the isolated cwd does not stage
#               (a `spec/testdata/*` fixture, `$REPO_ROOT`, a pre-made `_tmp/`, or
#               reading `spec/`), detected by `reads_repo_tree` (the code mentions
#               `spec/`, `REPO_ROOT`, `testdata`, or `_tmp/`) — which fails for an
#               environment reason, or degenerates into a FALSE PASS (e.g. `find
#               spec/` empty on both sides of a comparison) that would mask real
#               regressions. A PASSING tree-reading case is skipped for the same
#               false-green reason.
#           These heuristics are conservative substring matches over the case code
#           with FULL-LINE comments stripped, so a token mentioned only in prose does
#           not force a skip. KNOWN over-match (safe direction): a token inside an
#           INLINE comment (`cmd  # ... spec/ ...`) still trips it, so a self-contained
#           case can be over-skipped and lose gate coverage; a follow-up fixture/argv
#           rig plus proper comment tokenizing would let most of (a) and (c) run.
#
# The key is `<file>::<description>`, occurrence-qualified: a description repeated
# within a file gets an ` ##N` suffix on its 2nd+ occurrence (see case_keys), so
# every case is addressable and no entry masks two cases. If two cases ever still
# map to one key (a genuine collision), the gate reds unconditionally, listed or not
# (see duplicate_conflicts).
";

// Drop full-line shell comments before the skip heuristics scan, so a tree/helper
// token that appears only in prose (e.g. `# analogous to the spec/loop case`) does
// not force a self-contained case to `skip` and silently remove it from the gate.
// Inline comments (`cmd  # ...`) are left intact: stripping them safely needs quote
// tracking, and an inline mention only over-skips (the safe direction) — noted in
// the overlay header as a known conservative over-match.
fn executable_code(code: &str) -> String {
    let mut out = String::with_capacity(code.len());
    for line in code.lines() {
        if line.trim_start().starts_with('#') {
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

fn needs_missing_helper(code: &str) -> bool {
    code.contains("argv.py")
}

// Reads the Oils checkout the isolated `-c` cwd does not stage. Such a case cannot
// exercise td-sh faithfully: it fails for an environment reason, or degenerates into
// a false pass (e.g. `find spec/` empty on both sides). Conservative substring match
// over comment-stripped code.
fn reads_repo_tree(code: &str) -> bool {
    code.contains("spec/")
        || code.contains("REPO_ROOT")
        || code.contains("testdata")
        || code.contains("_tmp/")
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let shell = PathBuf::from(
        args.next().ok_or("usage: gen_expectations <shell-binary> <spec-dir>")?,
    );
    let dir = PathBuf::from(
        args.next().ok_or("usage: gen_expectations <shell-binary> <spec-dir>")?,
    );

    let mut xfail: Vec<String> = Vec::new();
    let mut skip: Vec<String> = Vec::new();
    // Distinct-key set: a repeat means two cases mapped to one overlay key, which the
    // gate's duplicate_conflicts reds on — so treat it as a hard generation error.
    let mut seen: BTreeSet<String> = BTreeSet::new();
    // Any error means the overlay would be incomplete/bogus; collect all, then abort
    // WITHOUT emitting, so a failed regeneration can never overwrite the committed
    // file with silent-partial output (it exits non-zero instead).
    let mut errors: Vec<String> = Vec::new();

    for path in &spec_paths(&dir)? {
        let file =
            path.file_name().and_then(|n| n.to_str()).unwrap_or_default().to_string();
        let text = std::fs::read_to_string(path)?;
        let cases = match parse_spec(&text) {
            Ok(c) => c,
            Err(e) => {
                errors.push(format!("parse-error {file}: {e}"));
                continue;
            }
        };
        let keys = case_keys(&file, &cases);
        for (case, key) in cases.iter().zip(keys) {
            if !seen.insert(key.clone()) {
                errors.push(format!("overlay-key collision (two cases, one key): {key}"));
                continue;
            }
            let exec = executable_code(&case.code);
            match run_case(&shell, case, ASH_DASH_CHAIN) {
                Ok(outcome) if outcome.passed => {
                    // A tree-reading pass is a probable false-green: skip it.
                    if reads_repo_tree(&exec) {
                        skip.push(key);
                    }
                }
                Ok(outcome) => {
                    if needs_missing_helper(&exec) || outcome.timed_out || reads_repo_tree(&exec) {
                        skip.push(key);
                    } else {
                        xfail.push(key);
                    }
                }
                // A run error is infrastructure (bad shell path, spawn/workdir failure,
                // or a case with no golden for this chain) — not a real xfail. Abort.
                Err(e) => errors.push(format!("run-error {key}: {e}")),
            }
        }
    }

    if !errors.is_empty() {
        for e in &errors {
            eprintln!("{e}");
        }
        return Err(format!(
            "{} error(s) during generation; overlay NOT emitted (exit non-zero)",
            errors.len()
        )
        .into());
    }

    xfail.sort();
    skip.sort();
    let mut out = String::with_capacity(HEADER.len() + (xfail.len() + skip.len()) * 48);
    out.push_str(HEADER);
    out.push_str("\n# ---- xfail: td-sh runs, wrong answer (known gaps) ----\n");
    for key in &xfail {
        out.push_str("xfail ");
        out.push_str(key);
        out.push('\n');
    }
    out.push_str("\n# ---- skip: not evaluated here (see header) ----\n");
    for key in &skip {
        out.push_str("skip ");
        out.push_str(key);
        out.push('\n');
    }

    // write_all (not print!) so a broken pipe returns an error instead of panicking.
    std::io::Write::write_all(&mut std::io::stdout(), out.as_bytes())?;
    eprintln!("generated: {} xfail, {} skip", xfail.len(), skip.len());
    Ok(())
}

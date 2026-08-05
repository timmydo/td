//! Regenerate `spec/expectations.txt`, the GENERATED xfail/skip overlay for the
//! vendored Oils spec corpus. Committed so the overlay is reproducible rather than
//! a hand-edited artifact: any maintainer can re-derive it after a pin bump or as
//! td-sh gains coverage, and review can diff a regenerated file against the commit.
//!
//! Usage (build the shell first, then point the generator at it and the corpus):
//!   cargo build --release --manifest-path td-sh/Cargo.toml
//!   cargo run --release --manifest-path td-sh/Cargo.toml \
//!     --example gen_expectations -- \
//!     td-sh/target/release/td-sh td-sh/target/release/spec_argv td-sh/spec \
//!     > td-sh/spec/expectations.txt
//!
//! It runs every case in <spec-dir> through the built td-sh under the SAME isolated
//! `-c` harness the gate uses (`td_sh::run_case`), and buckets each case:
//!   pass  -> unlisted, UNLESS its comment-stripped code reads the repo tree (see
//!            `reads_repo_tree`) or depends on shared `/tmp` state (see
//!            `depends_on_shared_tmp`): a pass there is a probable false-green,
//!            because the isolated cwd stages neither, so those are emitted as
//!            `skip`.
//!   skip  -> cannot be evaluated faithfully here (times out per the typed
//!            `CaseOutcome::timed_out`, or is repo-tree/fixture bound, or turns on
//!            shared `/tmp` state the harness does not control).
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
// A crate root of its own: `main.rs`'s lint reaches nothing here. `forbid`
// rather than `deny` because no scoped allow belongs in this one, and
// `forbid` is the spelling a later `#[allow(unsafe_code)]` cannot override.
#![forbid(unsafe_code)]

use std::collections::BTreeSet;
use std::path::PathBuf;
use td_sh::{case_keys, graded_identity, parse_spec, run_case, spec_paths, ASH_DASH_CHAIN};

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
#     -- td-sh/target/release/td-sh td-sh/target/release/spec_argv td-sh/spec \\
#     > td-sh/spec/expectations.txt
#
# Format, one per line:   <xfail|skip> <spec-file>::<case description>
#   xfail = td-sh runs it and gets the wrong answer today. It still RUNS every
#           gate; the failure is tolerated, a regression (an unlisted case that
#           fails) reds the gate, and an unexpected PASS (XPASS) reds it so the
#           entry is promoted. NOTE on honesty at this scale: many xfails are
#           status-127 `command not found` failures, because the cleared-env `-c`
#           harness exposes no external PATH beyond the shell itself and `argv.py`.
#           Those are real mismatches today, but they measure the harness's
#           minimalism as much as td-sh; serving more of the externals the corpus
#           reaches for (deferred) would convert a further block of them into true
#           builtin/parser pass-fail signal.
#   skip  = not run at all, because it cannot be evaluated faithfully here:
#           (a) td-sh hangs/loops on it, so it would time out (10s) every gate;
#           (b) it depends on the Oils repo tree the isolated cwd does not stage
#               (a `spec/testdata/*` fixture, `$REPO_ROOT`, a pre-made `_tmp/`, or
#               reading `spec/`), detected by `reads_repo_tree` (the code mentions
#               `spec/`, `REPO_ROOT`, `testdata`, or `_tmp/`) — which fails for an
#               environment reason, or degenerates into a FALSE PASS (e.g. `find
#               spec/` empty on both sides of a comparison) that would mask real
#               regressions. A PASSING tree-reading case is skipped for the same
#               false-green reason.
#           (c) it depends on SHARED `/tmp` state the isolated cwd neither owns nor
#               cleans, detected by `depends_on_shared_tmp` in the three shapes the
#               corpus spells it: an absolute path under `/tmp` (the code mentions
#               `/tmp/`), the same path reached through `HOME=/tmp`, and a test of
#               `/tmp`'s own MODE (`-k /tmp`). Sixteen cases `mkdir -p` or `touch`
#               such a path and then depend on it -- `cd` there and print `pwd`, put
#               it on `PATH`, make it `$HOME` -- or ask whether `/tmp` is sticky. The
#               staging command is not on the harness PATH, so whether the path
#               exists is a fact about the HOST, and so is the mode: five flip
#               verdict once an earlier run under a real shell has left the
#               directories behind, and `-k for sticky bit` flips on any host whose
#               `/tmp` is not sticky. Listing them either way would red the gate on
#               whichever machine disagrees. A PASSING such case is skipped for the
#               same false-green reason, and `-k for sticky bit` IS one -- so its
#               disagreement would red as a REGRESSION, not as the milder XPASS the
#               rest would. Fifteen MOVE here; the sixteenth (`builtin-dirs::cd
#               replaces the lowest entry`) was already skipped by (b), whose
#               `spec/` token its `/tmp/oils-spec/` path contains.
#               Bare `/tmp` is deliberately NOT a shape: ten cases need only that it
#               EXIST, which is universal, and one names the relative `./tmp.sh`.
#           These heuristics are conservative substring matches over the case code
#           with FULL-LINE comments stripped, so a token mentioned only in prose does
#           not force a skip. KNOWN over-match (safe direction): a token inside an
#           INLINE comment (`cmd  # ... spec/ ...`) still trips it, so a self-contained
#           case can be over-skipped and lose gate coverage; a follow-up fixture rig
#           plus proper comment tokenizing would let most of (b) run.
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

// Depends on shared `/tmp` state the isolated cwd neither owns nor cleans, in the
// three shapes the corpus spells it: an absolute path under it, the same path
// reached through `HOME=/tmp`, and a test of `/tmp`'s own MODE. The staging
// command (`mkdir`, `touch`) is withheld from the harness PATH, so the path exists
// only when an EARLIER run under a real shell left it behind -- and one case
// `rmdir`s it again. The verdict is then a fact about the host: the same commit is
// green on a machine that has run the corpus with a real PATH and red on a fresh
// one, in whichever direction the leftovers point. Matched on the dependency
// rather than on today's verdict, as `reads_repo_tree` is: a case masked by a
// SECOND withheld external is stable only until that one is served, and a category
// defined by what a case DEPENDS ON is one the next reader can check by eye.
//
// Bare `/tmp` is deliberately not a shape of its own: ten cases need only that it
// EXIST, which is universal, and one names the relative `./tmp.sh`.
fn depends_on_shared_tmp(code: &str) -> bool {
    code.contains("/tmp/") || code.contains("HOME=/tmp") || code.contains("-k /tmp")
}

/// Identities this shell CANNOT be staged as. Each probe is a real case run
/// through `run_case`, so it exercises the same staging the corpus run does; a
/// probe whose spec does not actually resolve to the identity it names would
/// prove nothing, so that is checked too and reported as a generation error.
fn probe_identities(
    shell: &std::path::Path,
    argv_helper: &std::path::Path,
    errors: &mut Vec<String>,
) -> Result<Vec<&'static str>, Box<dyn std::error::Error>> {
    let mut bad = Vec::new();
    for id in ASH_DASH_CHAIN {
        let src = format!("## compare_shells: {id}\n\n#### probe\n:\n");
        let cases = parse_spec(&src)?;
        let Some(case) = cases.first() else {
            errors.push(format!("identity probe for `{id}` parsed to no case"));
            continue;
        };
        if graded_identity(case, ASH_DASH_CHAIN) != Some(id) {
            errors.push(format!("identity probe for `{id}` does not resolve to it"));
            continue;
        }
        if !run_case(shell, argv_helper, case, ASH_DASH_CHAIN)?.passed {
            bad.push(*id);
        }
    }
    Ok(bad)
}

/// Does the staged `argv.py` actually ANSWER? Requiring the argument only proves
/// a path was typed: a helper that exists but cannot run -- not executable, an
/// interpreter shebang that resolves to nothing on the harness's one-entry PATH,
/// a binary for another architecture -- canonicalizes and symlinks like any
/// other, and then every one of the 294 cases that call it fails at exec. That
/// is a 127 indistinguishable from a shell gap, which is the failure this whole
/// argument exists to prevent, and it comes out as a clean exit 0 over an
/// overlay with 110 extra xfails in it. So the helper is asked a question whose
/// answer it cannot fake, through the same `run_case` staging the corpus uses.
fn probe_helper(
    shell: &std::path::Path,
    argv_helper: &std::path::Path,
) -> Result<Option<String>, Box<dyn std::error::Error>> {
    // Two arguments, one of them quoted-with-a-space, so a helper that merely
    // echoed its input could not pass either.
    let src = "## compare_shells: ash\n\n#### helper probe\nargv.py x 'y z'\n\
               ## stdout: ['x', 'y z']\n";
    let cases = parse_spec(src)?;
    let Some(case) = cases.first() else {
        return Ok(Some("helper probe parsed to no case".to_string()));
    };
    let outcome = run_case(shell, argv_helper, case, ASH_DASH_CHAIN)?;
    if outcome.passed {
        return Ok(None);
    }
    Ok(Some(format!(
        "{} does not answer as `argv.py`: the probe `argv.py x 'y z'` did not \
         print `['x', 'y z']` ({}). Every case that asks what a word expanded \
         to would be recorded as a shell gap.",
        argv_helper.display(),
        outcome.detail.unwrap_or_else(|| "no detail".to_string()),
    )))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let shell = PathBuf::from(args.next().ok_or("usage: gen_expectations <shell-binary> <argv-helper> <spec-dir>")?);
    // The `argv.py` stand-in, REQUIRED rather than derived from the shell's
    // directory: without it the 294 cases that ask what a word expanded to fail
    // with a 127 indistinguishable from a shell gap, and this tool would write
    // that into the committed overlay.
    let argv_helper = PathBuf::from(args.next().ok_or("usage: gen_expectations <shell-binary> <argv-helper> <spec-dir>")?);
    let dir = PathBuf::from(args.next().ok_or("usage: gen_expectations <shell-binary> <argv-helper> <spec-dir>")?);

    let mut xfail: Vec<String> = Vec::new();
    let mut skip: Vec<String> = Vec::new();
    // Distinct-key set: a repeat means two cases mapped to one overlay key, which the
    // gate's duplicate_conflicts reds on — so treat it as a hard generation error.
    let mut seen: BTreeSet<String> = BTreeSet::new();
    // Any error means the overlay would be incomplete/bogus; collect all, then abort
    // WITHOUT emitting, so a failed regeneration can never overwrite the committed
    // file with silent-partial output (it exits non-zero instead).
    let mut errors: Vec<String> = Vec::new();

    // A case is staged under the identity it is GRADED as, and a MULTICALL shell
    // reads that name out of argv[0] to choose an applet -- so pointing this at
    // busybox makes every dash-graded case die with `applet not found`, and the
    // overlay that comes out looks like 500 wrong answers instead of one
    // unusable pairing. Probe each identity once with `:` and fail closed.
    for id in probe_identities(&shell, &argv_helper, &mut errors)? {
        errors.push(format!(
            "{} cannot run as `{id}`: a case graded as `{id}` is staged under that \
             name, and a multicall binary reads it from argv[0]. Grade this shell \
             only on the cases whose identity it can take.",
            shell.display()
        ));
    }

    if let Some(bad) = probe_helper(&shell, &argv_helper)? {
        errors.push(bad);
    }

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
            match run_case(&shell, &argv_helper, case, ASH_DASH_CHAIN) {
                Ok(outcome) if outcome.passed => {
                    // A pass that reads the tree, or that depended on a directory
                    // the host happened to have, is a probable false-green.
                    if reads_repo_tree(&exec) || depends_on_shared_tmp(&exec) {
                        skip.push(key);
                    }
                }
                Ok(outcome) => {
                    if outcome.timed_out
                        || reads_repo_tree(&exec)
                        || depends_on_shared_tmp(&exec)
                    {
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

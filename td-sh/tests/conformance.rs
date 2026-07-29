//! td-sh conformance: run the Oils-format spec corpus through the built `td-sh`
//! binary and hold the line on what it passes.
//!
//! The corpus is a mix of vendored-pristine Oils spec files and td-sh's own seed
//! cases (see ../spec/README). td-sh cannot yet pass every upstream case, so a
//! per-case overlay (../spec/expectations.txt) records the known gaps:
//!   - a case td-sh does not pass yet is listed `xfail` — it still runs, its
//!     failure is tolerated, but a REGRESSION (an unlisted case that fails) reds
//!     the gate, and an unexpected PASS (`xpass`) reds it so the entry is promoted;
//!   - a case that cannot be evaluated faithfully in this isolated `-c` harness is
//!     listed `skip` and not run: it needs the `argv.py` helper, hangs/times out,
//!     or depends on the Oils repo tree the sandbox cwd does not stage (which would
//!     fail for an environment reason or degenerate into a false pass). The overlay
//!     header enumerates these categories.
//! A stale overlay entry (matching no case) also reds the gate, so the manifest
//! cannot rot. This is the shared `cargo-test` gate every agent's `affected-checks`
//! preflight runs, so it stays green and blocking.
//!
//! The corpus STRUCTURE is validated separately by `corpus_is_well_formed`, which
//! parses and resolves every `spec/*.test.sh` without running the shell — so a
//! malformed corpus file is caught even if the behavioral run is skipped.
//!
//! Watch case-by-case detail explicitly:
//!   cargo test --manifest-path td-sh/Cargo.toml -- --nocapture
//!
//! Conformance green is one of two gates before td-sh becomes the image
//! `/bin/sh`; the busybox `ash_test` parity gate is the other (system-x86-64,
//! a later PR). Importing the remaining Oils files (and the `argv.py` helper rig
//! the `skip` cases need) is follow-up work.

use std::path::{Path, PathBuf};

use td_sh::{
    parse_spec, resolve, run_case, run_dir_classified, summarize, Disposition, Expectations,
    ASH_DASH_CHAIN,
};

fn spec_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("spec")
}

/// Parse and resolve every spec file on each gate run so a malformed corpus file
/// reds in-loop. Runs no shell, so it catches a structural corpus defect (bad
/// annotation, unterminated block, typo'd assertion key) independently of the
/// behavioral run below.
#[test]
fn corpus_is_well_formed() -> Result<(), Box<dyn std::error::Error>> {
    let spec_dir = spec_dir();
    let mut files = 0usize;
    let mut total_cases = 0usize;
    for entry in std::fs::read_dir(&spec_dir)? {
        let path = entry?.path();
        let is_spec = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.ends_with(".test.sh"));
        if !is_spec {
            continue;
        }
        let text = std::fs::read_to_string(&path)?;
        let cases = parse_spec(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        assert!(!cases.is_empty(), "no cases parsed from {}", path.display());
        for case in &cases {
            // Reject a typo'd/unsupported assertion key that would otherwise
            // resolve to a silently-wrong default golden.
            let unknown = case.unrecognized_keys();
            assert!(
                unknown.is_empty(),
                "{} [case: {}]: unrecognized annotation key(s) {:?} — a typo or an \
                 unsupported assertion the resolver would silently skip",
                path.display(),
                case.name,
                unknown,
            );
            resolve(case, ASH_DASH_CHAIN)
                .map_err(|e| format!("{} [case: {}]: {e}", path.display(), case.name))?;
        }
        files += 1;
        total_cases += cases.len();
    }
    assert!(files > 0, "no *.test.sh files under {}", spec_dir.display());
    assert!(total_cases > 0, "corpus parsed to zero cases");
    Ok(())
}

/// Build `td-sh` and run every spec case, classified against the overlay. Green
/// iff there is no regression (an unlisted case that fails), no unexpected pass
/// (a listed `xfail` that now passes — promote it), and no stale overlay entry.
#[test]
fn corpus_conformance() -> Result<(), Box<dyn std::error::Error>> {
    let shell = PathBuf::from(env!("CARGO_BIN_EXE_td-sh"));
    let spec_dir = spec_dir();

    let exp_text = std::fs::read_to_string(spec_dir.join("expectations.txt")).unwrap_or_default();
    let exp = Expectations::parse(&exp_text).map_err(|e| format!("expectations.txt: {e}"))?;

    let (outcomes, stale) = run_dir_classified(&shell, &spec_dir, ASH_DASH_CHAIN, &exp)?;
    let s = summarize(&outcomes);
    eprintln!(
        "td-sh conformance: {} pass, {} xfail, {} skip  |  {} regressions, {} to-promote, {} stale",
        s.pass, s.xfail, s.skip, s.fail, s.xpass, stale.len()
    );

    let regressions: Vec<&_> =
        outcomes.iter().filter(|o| o.disposition == Disposition::Fail).collect();
    for o in &regressions {
        eprintln!("REGRESSION {}: {}", o.key, o.detail.clone().unwrap_or_default());
    }
    let to_promote: Vec<&_> =
        outcomes.iter().filter(|o| o.disposition == Disposition::XPass).collect();
    for o in &to_promote {
        eprintln!("XPASS (remove from expectations.txt) {}", o.key);
    }
    for k in &stale {
        eprintln!("STALE expectations.txt entry (matches no case): {k}");
    }

    assert!(s.pass > 0, "corpus produced zero passing cases — harness or build broken");
    assert!(regressions.is_empty(), "{} regression(s) — see REGRESSION lines above", regressions.len());
    assert!(to_promote.is_empty(), "{} xfail now pass(es) — promote them, see XPASS lines", to_promote.len());
    assert!(stale.is_empty(), "{} stale overlay entr(ies) — see STALE lines above", stale.len());
    Ok(())
}

/// A case whose output far exceeds the ~64 KiB pipe buffer must be captured
/// without deadlocking: `wait_and_capture` drains both pipes on reader threads
/// started before the wait, so the child keeps running instead of blocking on
/// write. The pre-fix drain-after-exit path would block the writer and only
/// return after `CASE_TIMEOUT` (a false timeout), so the elapsed-time bound is
/// what distinguishes the fix from the bug. No stdout assertion: this exercises
/// capture and liveness, not the bytes.
#[test]
fn large_output_case_is_captured_without_deadlock() -> Result<(), Box<dyn std::error::Error>> {
    let shell = PathBuf::from(env!("CARGO_BIN_EXE_td-sh"));
    let spec = "#### big output\n\
        i=0; while [ \"$i\" -lt 4000 ]; do echo \
        0123456789012345678901234567890123456789012345678; i=$((i+1)); done\n\
        ## status: 0\n";
    let cases = parse_spec(spec)?;
    let case = cases.first().ok_or("no case parsed")?;
    let start = std::time::Instant::now();
    let outcome = run_case(&shell, case, ASH_DASH_CHAIN)?;
    assert!(outcome.passed, "large-output case failed: {:?}", outcome.detail);
    assert!(
        start.elapsed() < std::time::Duration::from_secs(5),
        "large-output case took {:?} — a drain deadlock hitting CASE_TIMEOUT",
        start.elapsed(),
    );
    Ok(())
}

/// A function's prefix assignment is EXPORTED for the duration of the call, as an
/// external command's environment would be. Only a real child can observe that,
/// and the in-process unit-test harness captures stdout in a buffer no child can
/// write to — so this spawns the built shell and lets it run ITSELF as the child,
/// which is the one external a target-side gate can count on.
#[test]
fn a_functions_temp_binding_is_exported_to_a_child() -> Result<(), Box<dyn std::error::Error>> {
    let shell = PathBuf::from(env!("CARGO_BIN_EXE_td-sh"));
    // The path travels in the environment rather than in the script text, so a
    // build directory containing a quote or a space cannot break the case.
    let out = std::process::Command::new(&shell)
        .arg("-c")
        .arg("f() { \"$TD_SH_BIN\" -c 'echo got=[$TD_SH_TEMP]'; }; TD_SH_TEMP=dd f; f")
        .env("TD_SH_BIN", &shell)
        .env_remove("TD_SH_TEMP")
        .output()?;
    assert_eq!(String::from_utf8_lossy(&out.stdout), "got=[dd]\ngot=[]\n");
    Ok(())
}

/// A bare `local x` clears the VALUE and keeps the entry, so the name is absent
/// from a child's environment while localised but exports again the moment the
/// function assigns it — the `local PATH; PATH=...; cmd` idiom. The `unset`
/// builtin drops the attribute with the name instead. Only a child can see any of
/// that, so this runs the built shell as its own child, as the test above does.
#[test]
fn a_localised_name_still_exports_once_assigned() -> Result<(), Box<dyn std::error::Error>> {
    let shell = PathBuf::from(env!("CARGO_BIN_EXE_td-sh"));
    let child = "\"$TD_SH_BIN\" -c 'echo [${TD_SH_X-UNSET}]'";
    for (body, want) in [
        // Localised and reassigned: the child sees the NEW value.
        (format!("f() {{ local TD_SH_X; TD_SH_X=H; {child}; }}; f"), "[H]\n"),
        // Localised and left alone: absent, not the outer value and not empty.
        (format!("f() {{ local TD_SH_X; {child}; }}; f"), "[UNSET]\n"),
        // `unset` takes the attribute with it, so a later assignment does NOT
        // reach the child.
        (format!("unset TD_SH_X; TD_SH_X=H; {child}"), "[UNSET]\n"),
        // Restored on the way out.
        (format!("f() {{ local TD_SH_X; }}; f; {child}"), "[G]\n"),
        // `export NAME` before any value is the same state from the other side.
        (format!("unset TD_SH_X; export TD_SH_X; {child}"), "[UNSET]\n"),
        (format!("unset TD_SH_X; export TD_SH_X; TD_SH_X=H; {child}"), "[H]\n"),
    ] {
        let out = std::process::Command::new(&shell)
            .arg("-c")
            .arg(&body)
            .env("TD_SH_BIN", &shell)
            .env("TD_SH_X", "G")
            .output()?;
        assert_eq!(String::from_utf8_lossy(&out.stdout), want, "{body}");
    }
    Ok(())
}

/// A case that redirects into a relative filename must not touch the gate's
/// working tree: `run_case` isolates each case in a throwaway temp working
/// directory. Corpus cases like `var-num.test.sh::$0 with filename` do exactly
/// this, and redirect-heavy files import cleanly only because of the isolation.
#[test]
fn file_writing_case_does_not_pollute_cwd() -> Result<(), Box<dyn std::error::Error>> {
    let shell = PathBuf::from(env!("CARGO_BIN_EXE_td-sh"));
    // Process-unique so it can't collide with a real working-tree file (the
    // start-of-test cleanup would otherwise delete a user's file) and concurrent
    // gate processes don't race on one name. A drop guard removes it on every exit
    // path, so even a broken-isolation run (failing assert) leaves the tree clean.
    let marker = format!("td_sh_pollution_marker_{}", std::process::id());
    struct Cleanup(String);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    let _ = std::fs::remove_file(&marker); // in case a prior same-pid run left it
    let _guard = Cleanup(marker.clone());
    let spec = format!("#### writes a file\necho hi > {marker}\n## status: 0\n");
    let cases = parse_spec(&spec)?;
    let case = cases.first().ok_or("no case parsed")?;
    let outcome = run_case(&shell, case, ASH_DASH_CHAIN)?;
    assert!(outcome.passed, "redirect case failed: {:?}", outcome.detail);
    assert!(!Path::new(&marker).exists(), "case leaked {marker} into the cwd — isolation broken");
    Ok(())
}

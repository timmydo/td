//! td-sh conformance: run the seed Oils-format spec corpus through the built
//! `td-sh` binary and require every case to pass.
//!
//! This WAS the RED TDD baseline (the exit-0 stub failed every case); the
//! interpreter now passes the whole seed corpus, so the behavioral test runs in
//! the plain `cargo test` that the shared `cargo-test` gate — and every agent's
//! `affected-checks` cargo-test preflight — executes. It builds the debug
//! `td-sh` and runs each case against it (no target toolchain needed), so it
//! stays a green, blocking gate that locks conformance in: a regression that
//! breaks any case reds the gate.
//!
//! The corpus STRUCTURE is validated separately by `seed_corpus_is_well_formed`,
//! which parses and resolves every `spec/*.test.sh` without running the shell —
//! so a malformed corpus file is caught even if the behavioral run is skipped.
//!
//! Watch case-by-case detail explicitly:
//!   cargo test --manifest-path td-sh/Cargo.toml -- --nocapture
//!
//! Conformance green is one of two gates before td-sh becomes the image
//! `/bin/sh`; the busybox `ash_test` parity gate and the bulk Oils import are
//! the remaining ones (system-x86-64), handled in later PRs.

use std::path::{Path, PathBuf};

use td_sh::{parse_spec, resolve, run_dir, tally, ASH_DASH_CHAIN};

/// Parse and resolve every seed spec file on each gate run so a malformed corpus
/// file reds in-loop. Runs no shell, so it catches a structural corpus defect
/// (bad annotation, unterminated block) independently of the behavioral run below.
#[test]
fn seed_corpus_is_well_formed() -> Result<(), Box<dyn std::error::Error>> {
    let spec_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("spec");
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
    assert!(total_cases > 0, "seed corpus parsed to zero cases");
    Ok(())
}

/// Build `td-sh` and run every seed spec case against it, requiring all to pass.
/// Green and blocking: a regression that breaks any case reds the shared gate.
#[test]
fn seed_corpus_conformance() -> Result<(), Box<dyn std::error::Error>> {
    let shell = PathBuf::from(env!("CARGO_BIN_EXE_td-sh"));
    let spec_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("spec");

    let outcomes = run_dir(&shell, &spec_dir, ASH_DASH_CHAIN)?;
    let (passed, total) = tally(&outcomes);

    for o in &outcomes {
        if !o.passed {
            match &o.detail {
                Some(detail) => eprintln!("FAIL {}: {}", o.name, detail),
                None => eprintln!("FAIL {}", o.name),
            }
        }
    }
    eprintln!("td-sh conformance: {passed}/{total} cases pass");

    assert!(total > 0, "no spec cases found under {}", spec_dir.display());
    assert_eq!(
        passed, total,
        "{} of {} conformance cases failing — see FAIL lines above",
        total - passed,
        total
    );
    Ok(())
}

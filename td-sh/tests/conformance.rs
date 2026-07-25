//! td-sh conformance: run the seed Oils-format spec corpus through the built
//! `td-sh` binary and require every case to pass.
//!
//! This is the RED TDD baseline this PR establishes: the exit-0 stub fails every
//! case. It is `#[ignore]`d ON PURPOSE so it does NOT run in the plain `cargo
//! test` that the shared `cargo-test` gate — and every agent's `affected-checks`
//! cargo-test preflight — executes. A perpetually-red shared gate would break
//! land-on-green for the whole repo; the red baseline must stay opt-in until the
//! interpreter can pass it. The harness's own red-detection logic is proven
//! GREEN by the unit tests in `lib.rs` (`evaluate_reds_the_exit0_stub_baseline`).
//!
//! The STRUCTURE of the corpus is still validated in-loop, though: the GREEN
//! `seed_corpus_is_well_formed` test below parses and resolves every `spec/*.test.sh`
//! on each gate run — it never runs the shell, so it stays green while catching a
//! malformed corpus file (bad annotation, unterminated block) that the ignored
//! behavioral test would otherwise never surface.
//!
//! Watch progress explicitly:
//!   cargo test --manifest-path td-sh/Cargo.toml -- --ignored --nocapture
//!
//! Future PRs grow the interpreter until this is green, then drop `#[ignore]`;
//! only once this AND the busybox `ash_test` parity gate pass is td-sh wired into
//! the image as `/bin/sh` (system-x86-64).

use std::path::{Path, PathBuf};

use td_sh::{parse_spec, resolve, run_dir, tally, ASH_DASH_CHAIN};

/// GREEN, non-ignored: parse and resolve every seed spec file on each gate run so
/// a malformed corpus file reds in-loop (the behavioral RUN stays `#[ignore]`d
/// below, off the shared gate). Runs no shell, so it cannot go red on the stub.
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

#[test]
#[ignore = "RED TDD baseline: the exit-0 stub fails every conformance case; run with --ignored"]
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
        "{} of {} conformance cases failing (expected while td-sh is a stub)",
        total - passed,
        total
    );
    Ok(())
}

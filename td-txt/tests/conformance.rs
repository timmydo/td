//! td-txt conformance: run the corpus through the built `td-txt` multicall and
//! hold the line on what it passes.
//!
//! The corpus (see ../spec/README) is the vendored-pristine GNU grep regex suites
//! and GNU sed testsuite triples, plus td-txt's own `*.test.txt` cases for the
//! option surface. td-txt does not pass every upstream case yet, so a per-case
//! overlay (../spec/expectations.txt) records the known gaps:
//!   - a case td-txt does not pass yet is listed `xfail` — it still runs, its
//!     failure is tolerated, but a REGRESSION (an unlisted case that fails) reds
//!     the gate, and an unexpected PASS (`xpass`) reds it so the entry is promoted;
//!   - a case that cannot be evaluated faithfully here is listed `skip` and not
//!     run at all.
//! A stale overlay entry (matching no case) also reds the gate, so the manifest
//! cannot rot. This is the shared `cargo-test` gate every agent's `affected-checks`
//! preflight runs, so it stays green and blocking.
//!
//! The corpus STRUCTURE is validated separately by `corpus_is_well_formed`, which
//! loads and normalizes every case without running the binary — so a malformed
//! corpus file or a missing vendored input is caught even when the behavioral run
//! is not what broke.
//!
//! Watch case-by-case detail explicitly:
//!   cargo test --manifest-path td-txt/Cargo.toml -- --nocapture

use std::path::{Path, PathBuf};

use td_txt::{
    load_corpus, run_all_classified, run_case, summarize, Case, Disposition, Expect, Expectations,
    Stream,
};

fn spec_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("spec")
}

fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_td-txt"))
}

/// Load and normalize every case on each gate run, so a malformed corpus file, a
/// missing vendored `.inp`/`.good`, or a typo'd annotation reds in-loop — without
/// depending on the behavioral run below.
/// Raise this with the corpus; it exists to catch a corpus that SHRANK.
const CORPUS_FLOOR: usize = 660;

#[test]
fn corpus_is_well_formed() -> Result<(), Box<dyn std::error::Error>> {
    let cases = load_corpus(&spec_dir())?;
    // A floor just under today's count: a vendored file that stops parsing, or a
    // reader that starts skipping rows, shrinks the corpus silently otherwise.
    assert!(
        cases.len() >= CORPUS_FLOOR,
        "corpus shrank to {} cases (floor {CORPUS_FLOOR}) — is a vendored file missing or unparsed?",
        cases.len()
    );
    let mut applets = std::collections::BTreeSet::new();
    for case in &cases {
        assert!(!case.name.is_empty(), "a case in {} has no name", case.file);
        let applet = case
            .argv
            .first()
            .map(|a| String::from_utf8_lossy(a).into_owned())
            .ok_or("case with an empty argv")?;
        assert!(
            applet == "grep" || applet == "sed",
            "{}::{} invokes {applet:?}, which td-txt does not serve",
            case.file,
            case.name
        );
        applets.insert(applet);
        assert!(
            case.expect.asserts_something(),
            "{}::{} asserts nothing — it would pass no matter what td-txt did",
            case.file,
            case.name
        );
    }
    assert_eq!(
        applets.into_iter().collect::<Vec<_>>(),
        vec!["grep".to_string(), "sed".to_string()],
        "the corpus must exercise both applets"
    );
    Ok(())
}


/// Run every corpus case, classified against the overlay. Green iff there is no
/// regression (an unlisted case that fails), no unexpected pass (a listed `xfail`
/// that now passes — promote it), and no stale overlay entry.
#[test]
fn corpus_conformance() -> Result<(), Box<dyn std::error::Error>> {
    let spec_dir = spec_dir();
    let cases = load_corpus(&spec_dir)?;
    // Must EXIST. Treating a missing overlay as empty would turn a lost file
    // into "no known gaps", which reds only by accident.
    let exp_text = std::fs::read_to_string(spec_dir.join("expectations.txt"))
        .map_err(|e| format!("spec/expectations.txt: {e} (regenerate with examples/gen_expectations.rs)"))?;
    let exp = Expectations::parse(&exp_text).map_err(|e| format!("expectations.txt: {e}"))?;

    let (outcomes, stale) = run_all_classified(&bin(), &cases, &exp)?;
    let s = summarize(&outcomes);
    eprintln!(
        "td-txt conformance: {} pass, {} xfail, {} skip  |  {} regressions, {} to-promote, {} stale",
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
    assert!(
        regressions.is_empty(),
        "{} regression(s) — see REGRESSION lines above",
        regressions.len()
    );
    assert!(
        to_promote.is_empty(),
        "{} xfail now pass(es) — promote them, see XPASS lines",
        to_promote.len()
    );
    assert!(stale.is_empty(), "{} stale overlay entr(ies) — see STALE lines above", stale.len());
    Ok(())
}

/// A case whose output far exceeds the ~64 KiB pipe buffer must be captured
/// without deadlocking: `wait_and_capture` drains both pipes on reader threads
/// started before the wait, and stdin is written on its own thread, so a large
/// input and a large output cannot wedge each other. The elapsed-time bound is
/// what distinguishes the fix from the bug — a deadlock would only return at
/// CASE_TIMEOUT.
#[test]
fn a_large_case_streams_without_deadlocking() -> Result<(), Box<dyn std::error::Error>> {
    let mut input = Vec::new();
    for _ in 0..20_000 {
        input.extend_from_slice(b"0123456789012345678901234567890123456789\n");
    }
    let case = Case {
        file: "inline".into(),
        name: "large input and output".into(),
        argv: vec![b"grep".to_vec(), b"0".to_vec()],
        files: Vec::new(),
        stdin: input.clone(),
        expect: Expect { status: Some(0), stdout: Some(input), ..Expect::default() },
    };
    let start = std::time::Instant::now();
    let outcome = run_case(&bin(), &case)?;
    assert!(outcome.passed, "large case failed: {:?}", outcome.detail);
    assert!(
        start.elapsed() < std::time::Duration::from_secs(5),
        "large case took {:?} — a drain deadlock hitting the case timeout",
        start.elapsed()
    );
    Ok(())
}

/// A repetition over a long line must not run the matcher out of stack. The
/// engine backtracks by recursion, so this is a CRASH (SIGSEGV, no diagnostic),
/// not a wrong answer — invisible to a corpus case whose input is a few bytes.
/// A single-byte body must match outright; a multi-byte one must at worst be
/// REFUSED with a diagnostic, never abort.
#[test]
fn a_long_line_does_not_overflow_the_matcher_stack() -> Result<(), Box<dyn std::error::Error>> {
    let mut line = vec![b'a'; 200_000];
    line.push(b'\n');
    let counted = Case {
        file: "inline".into(),
        name: "starred single byte over a 200 KB line".into(),
        argv: vec![b"grep".to_vec(), b"-c".to_vec(), b"a*".to_vec()],
        files: Vec::new(),
        stdin: line.clone(),
        expect: Expect { status: Some(0), stdout: Some(b"1\n".to_vec()), ..Expect::default() },
    };
    let outcome = run_case(&bin(), &counted)?;
    assert!(outcome.passed, "long-line repetition failed: {:?}", outcome.detail);

    // A grouped body cannot use the flat path, so the step budget must stop it
    // with an error rather than a segfault.
    let grouped = Case {
        file: "inline".into(),
        name: "starred group over a 200 KB line".into(),
        argv: vec![b"grep".to_vec(), b"-c".to_vec(), br"\(ab\)*".to_vec()],
        files: Vec::new(),
        stdin: line,
        expect: Expect::default(),
    };
    let outcome = run_case(&bin(), &grouped)?;
    assert!(!outcome.timed_out, "starred group hung on a long line");
    Ok(())
}

/// The repetition depth cap only helps if the stack can hold that many frames.
/// `main.rs` asserts the arithmetic at compile time; this asserts the REALITY,
/// by running one iteration under the cap through the built binary. A frame that
/// grew past the reserved budget aborts here instead of in a user's pipeline.
#[test]
fn a_repetition_just_under_the_depth_cap_does_not_abort(
) -> Result<(), Box<dyn std::error::Error>> {
    let mut line = Vec::new();
    for _ in 0..19_999 {
        line.extend_from_slice(b"ab");
    }
    line.push(b'\n');
    let case = Case {
        file: "inline".into(),
        name: "19999 iterations of a two-byte body".into(),
        argv: vec![b"grep".to_vec(), b"-c".to_vec(), br"\(ab\)*".to_vec()],
        files: Vec::new(),
        stdin: line,
        expect: Expect { status: Some(0), stdout: Some(b"1\n".to_vec()), ..Expect::default() },
    };
    let outcome = run_case(&bin(), &case)?;
    assert!(outcome.passed, "depth just under the cap: {:?}", outcome.detail);
    Ok(())
}

/// The third stack axis: `m_seq` recurses per concatenated atom, so pattern
/// LENGTH can overflow too. Bounded like the others, and refused rather than
/// aborting.
#[test]
fn a_pattern_past_the_concat_cap_is_refused() -> Result<(), Box<dyn std::error::Error>> {
    let case = Case {
        file: "inline".into(),
        name: "100001 concatenated atoms".into(),
        argv: vec![b"grep".to_vec(), vec![b'.'; 100_001]],
        files: Vec::new(),
        stdin: b"a\n".to_vec(),
        expect: Expect {
            status: Some(2),
            stderr: Some(Stream::Contains(b"regular expression is too complex".to_vec())),
            ..Expect::default()
        },
    };
    let outcome = run_case(&bin(), &case)?;
    assert!(outcome.passed, "concat cap: {:?}", outcome.detail);
    Ok(())
}

/// Group nesting is capped for the same reason, and the cap is a DIVERGENCE:
/// GNU grep 3.11 matches at this depth. It lives here rather than in
/// spec/divergence.test.txt only because the pattern is 20 KB of parentheses.
#[test]
fn nesting_past_the_cap_is_refused_rather_than_aborting() -> Result<(), Box<dyn std::error::Error>>
{
    let mut deep = Vec::new();
    for _ in 0..5001 {
        deep.extend_from_slice(br"\(");
    }
    deep.push(b'a');
    for _ in 0..5001 {
        deep.extend_from_slice(br"\)");
    }
    let case = Case {
        file: "inline".into(),
        name: "5001 nested groups".into(),
        argv: vec![b"grep".to_vec(), deep],
        files: Vec::new(),
        stdin: b"a\n".to_vec(),
        expect: Expect {
            status: Some(2),
            stderr: Some(Stream::Contains(b"regular expression is too complex".to_vec())),
            ..Expect::default()
        },
    };
    let outcome = run_case(&bin(), &case)?;
    assert!(outcome.passed, "deep nesting: {:?}", outcome.detail);
    Ok(())
}

/// The harness isolates each case in a throwaway directory, so a case that writes
/// a file (every `sed -i` case does) cannot touch the gate's working tree.
#[test]
fn a_file_writing_case_does_not_pollute_the_working_tree() -> Result<(), Box<dyn std::error::Error>>
{
    let marker = format!("td_txt_pollution_marker_{}", std::process::id());
    struct Cleanup(String);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    let _ = std::fs::remove_file(&marker);
    let _guard = Cleanup(marker.clone());
    let case = Case {
        file: "inline".into(),
        name: "sed -i rewrites its file".into(),
        argv: vec![
            b"sed".to_vec(),
            b"-i".to_vec(),
            b"s/a/b/".to_vec(),
            marker.clone().into_bytes(),
        ],
        files: vec![(marker.clone(), b"a\n".to_vec())],
        stdin: Vec::new(),
        expect: Expect {
            status: Some(0),
            files_after: vec![(marker.clone(), b"b\n".to_vec())],
            ..Expect::default()
        },
    };
    let outcome = run_case(&bin(), &case)?;
    assert!(outcome.passed, "in-place case failed: {:?}", outcome.detail);
    assert!(
        !Path::new(&marker).exists(),
        "case leaked {marker} into the working tree — isolation is broken"
    );
    Ok(())
}

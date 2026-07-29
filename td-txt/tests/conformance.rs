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
const CORPUS_FLOOR: usize = 1321;

#[test]
fn corpus_is_well_formed() -> Result<(), Box<dyn std::error::Error>> {
    let cases = load_corpus(&spec_dir())?;
    // Today's count exactly: a vendored file that stops parsing, or a reader that
    // starts skipping rows, shrinks the corpus silently otherwise.
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

/// A throwaway directory for the filesystem-level `sed -i` tests below, removed
/// on drop so a failing assertion cannot leave a tree behind.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> std::io::Result<Self> {
        let dir = std::env::temp_dir().join(format!("td-txt-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir)?;
        Ok(Self(dir))
    }

    fn join(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn sed_in(dir: &TempDir, args: &[&str]) -> std::io::Result<std::process::Output> {
    std::process::Command::new(bin())
        .arg("sed")
        .args(args)
        .current_dir(&dir.0)
        .output()
}

/// `sed -i` writes a NEW file and renames it over the name, which is what makes a
/// symlink operand get REPLACED instead of resolved. Writing through the name —
/// the obvious implementation, and the one this replaced — silently gives every
/// `-i` run the `--follow-symlinks` behavior the applet refuses to be asked for.
/// The case files cannot express this: the harness materializes regular files
/// only, so it lives here.
#[test]
fn sed_in_place_replaces_a_symlink_rather_than_following_it(
) -> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new("symlink")?;
    std::fs::write(dir.join("real"), b"a\n")?;
    std::os::unix::fs::symlink("real", dir.join("link"))?;

    let out = sed_in(&dir, &["-i", "s/a/b/", "link"])?;
    assert!(out.status.success(), "sed -i on a symlink: {out:?}");
    assert!(
        !std::fs::symlink_metadata(dir.join("link"))?.file_type().is_symlink(),
        "-i left `link' a symlink, so it followed it and edited the target"
    );
    assert_eq!(std::fs::read(dir.join("link"))?, b"b\n");
    assert_eq!(
        std::fs::read(dir.join("real"))?,
        b"a\n",
        "-i rewrote the symlink's TARGET; GNU leaves it untouched"
    );
    Ok(())
}

/// The same rename is what breaks a hard link: the other name keeps the old
/// content and the old inode. Writing through the name would have changed both.
#[test]
fn sed_in_place_breaks_a_hard_link() -> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new("hardlink")?;
    std::fs::write(dir.join("one"), b"a\n")?;
    std::fs::hard_link(dir.join("one"), dir.join("two"))?;

    let out = sed_in(&dir, &["-i", "s/a/b/", "one"])?;
    assert!(out.status.success(), "sed -i on a hard link: {out:?}");
    assert_eq!(std::fs::read(dir.join("one"))?, b"b\n");
    assert_eq!(
        std::fs::read(dir.join("two"))?,
        b"a\n",
        "-i edited the shared inode, so every other name for it changed too"
    );
    Ok(())
}

/// Two more consequences of the rename, both of which a caller relies on: the new
/// file carries the original's mode (a create default would silently relax or
/// tighten it), and a read-only file can be rewritten at all — writing through the
/// name cannot open it.
#[test]
fn sed_in_place_keeps_the_mode_and_rewrites_a_read_only_file(
) -> Result<(), Box<dyn std::error::Error>> {
    use std::os::unix::fs::PermissionsExt;
    let dir = TempDir::new("mode")?;
    for (name, mode) in [("exec", 0o754u32), ("readonly", 0o444)] {
        std::fs::write(dir.join(name), b"a\n")?;
        std::fs::set_permissions(dir.join(name), std::fs::Permissions::from_mode(mode))?;
    }

    let out = sed_in(&dir, &["-i", "s/a/b/", "exec", "readonly"])?;
    assert!(out.status.success(), "sed -i mode case: {out:?}");
    for (name, mode) in [("exec", 0o754u32), ("readonly", 0o444)] {
        assert_eq!(std::fs::read(dir.join(name))?, b"b\n", "{name} was not rewritten");
        assert_eq!(
            std::fs::metadata(dir.join(name))?.permissions().mode() & 0o777,
            mode,
            "{name} lost its mode across the rename"
        );
    }
    // The scratch file is renamed, never left behind.
    let mut names: Vec<String> = std::fs::read_dir(&dir.0)?
        .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().into_owned()))
        .collect();
    names.sort();
    assert_eq!(names, vec!["exec".to_string(), "readonly".to_string()]);
    Ok(())
}

/// A failed backup rename must not leave the scratch file behind either, and it
/// is a RUNTIME failure (exit 4) — the script compiled fine, the filesystem
/// refused — so it carries no `-e expression #N' prefix blaming the script.
#[test]
fn a_failed_in_place_backup_is_exit_4_and_leaves_no_scratch_file(
) -> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new("backup")?;
    std::fs::write(dir.join("f"), b"a\n")?;

    // `*` expands to the operand as written, so this names a directory that does
    // not exist and the rename fails.
    // The suffix is ATTACHED to -i, never a separate argument.
    let out = sed_in(&dir, &["-inodir/*", "s/a/b/", "f"])?;
    assert_eq!(out.status.code(), Some(4), "want exit 4, got {out:?}");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.starts_with("sed: cannot rename f:"), "unexpected diagnostic: {err:?}");
    assert!(
        !err.contains("expression"),
        "a filesystem failure blamed the script: {err:?}"
    );
    let names: Vec<String> = std::fs::read_dir(&dir.0)?
        .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().into_owned()))
        .collect();
    assert_eq!(names, vec!["f".to_string()], "a scratch file was left behind");
    assert_eq!(std::fs::read(dir.join("f"))?, b"a\n", "the original was modified anyway");
    Ok(())
}

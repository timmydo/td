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
const CORPUS_FLOOR: usize = 1649;

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
        env: Vec::new(),
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
        env: Vec::new(),
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
        env: Vec::new(),
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
        env: Vec::new(),
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
        env: Vec::new(),
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
        env: Vec::new(),
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
        env: Vec::new(),
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

fn grep_in(dir: &TempDir, args: &[&str]) -> std::io::Result<std::process::Output> {
    std::process::Command::new(bin())
        .arg("grep")
        .args(args)
        .current_dir(&dir.0)
        .output()
}

/// `-r` collects REGULAR files only. The case that forced this is a FIFO, whose
/// open blocks until a writer arrives — `grep -r` in a tree holding one hung
/// forever where GNU skips it and finishes. `std` cannot make a FIFO, so the
/// test uses the other non-regular file it CAN make; both are the same rule, and
/// a socket fails the open loudly (`No such device or address`) where the FIFO
/// fails silently by never returning.
#[test]
fn grep_r_skips_a_non_regular_file_it_finds() -> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new("grep-r-socket")?;
    std::fs::create_dir(dir.join("t"))?;
    std::fs::write(dir.join("t").join("f"), b"a\n")?;
    let _sock = std::os::unix::net::UnixListener::bind(dir.join("t").join("s"))?;

    let out = grep_in(&dir, &["-rl", "a", "t"])?;
    assert_eq!(out.status.code(), Some(0), "want exit 0, got {out:?}");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "t/f\n");
    assert_eq!(String::from_utf8_lossy(&out.stderr), "", "the socket was opened anyway");
    Ok(())
}

/// The root of the walk is FOLLOWED and everything under it is not, which is
/// GNU's `-r`: a symlinked directory named as an OPERAND is descended, and one
/// found by the walk is skipped. `-R` follows both, and is still a bare synonym
/// for `-r` here (spec/README's walk gap).
#[test]
fn grep_r_follows_a_symlinked_directory_operand_but_not_one_it_finds(
) -> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new("grep-r-symlink")?;
    std::fs::create_dir(dir.join("real"))?;
    std::fs::write(dir.join("real").join("f"), b"a\n")?;
    std::fs::create_dir(dir.join("t"))?;
    std::fs::write(dir.join("t").join("g"), b"a\n")?;
    std::os::unix::fs::symlink("../real", dir.join("t").join("link"))?;
    std::os::unix::fs::symlink("real", dir.join("operand"))?;

    let named = grep_in(&dir, &["-rl", "a", "operand"])?;
    assert_eq!(named.status.code(), Some(0), "want exit 0, got {named:?}");
    assert_eq!(
        String::from_utf8_lossy(&named.stdout),
        "operand/f\n",
        "a symlinked directory named as an operand was not descended"
    );

    let found = grep_in(&dir, &["-rl", "a", "t"])?;
    assert_eq!(found.status.code(), Some(0), "want exit 0, got {found:?}");
    assert_eq!(
        String::from_utf8_lossy(&found.stdout),
        "t/g\n",
        "a symlink found by the walk was descended"
    );
    Ok(())
}

/// Only an OPERAND spells stdin. A file the walk found that happens to be named
/// `-` is searched like any other, which GNU does and reading stdin for it would
/// not -- with a live writer on stdin that substitution never returns.
#[test]
fn grep_r_searches_a_walked_file_named_dash() -> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new("grep-r-dashfile")?;
    std::fs::write(dir.join("-"), b"a\n")?;
    std::fs::write(dir.join("other"), b"a\n")?;

    let out = grep_in(&dir, &["-rn", "a"])?;
    assert_eq!(out.status.code(), Some(0), "want exit 0, got {out:?}");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "-:1:a\nother:1:a\n");
    Ok(())
}

/// `-` is stdin under `-r` as much as without it. It is not a directory, so it
/// neither descends nor names on its own — the name here comes from there being
/// two operands.
#[test]
fn grep_r_reads_stdin_for_a_dash_operand() -> Result<(), Box<dyn std::error::Error>> {
    use std::io::Write;
    let dir = TempDir::new("grep-r-stdin")?;
    std::fs::write(dir.join("f"), b"a\n")?;

    let mut child = std::process::Command::new(bin())
        .arg("grep")
        .args(["-rn", "a", "-", "f"])
        .current_dir(&dir.0)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()?;
    // Not `if let`: a missing pipe would leave the child blocking on stdin, so the
    // one test here that could hang the gate must fail instead.
    child.stdin.take().ok_or("no stdin pipe")?.write_all(b"a\nb\n")?;
    let out = child.wait_with_output()?;
    assert_eq!(out.status.code(), Some(0), "want exit 0, got {out:?}");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "(standard input):1:a\nf:1:a\n"
    );
    Ok(())
}

/// `-R` is the LOGICAL walk: every symlink is followed, not only the operand,
/// and the non-regular files `-r` skips are read. The socket here is the same
/// stand-in for a device as above — GNU's `-R` opens it and reports, where its
/// `-r` skips it silently, because the two flags' `--devices` defaults differ.
#[test]
fn grep_upper_r_follows_what_the_walk_finds_and_reads_devices(
) -> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new("grep-R-logical")?;
    std::fs::create_dir(dir.join("real"))?;
    std::fs::write(dir.join("real").join("f"), b"a\n")?;
    std::fs::create_dir(dir.join("t"))?;
    std::fs::write(dir.join("t").join("g"), b"a\n")?;
    std::os::unix::fs::symlink("../real", dir.join("t").join("link"))?;

    let logical = grep_in(&dir, &["-Rl", "a", "t"])?;
    assert_eq!(logical.status.code(), Some(0), "want exit 0, got {logical:?}");
    assert_eq!(
        String::from_utf8_lossy(&logical.stdout),
        "t/g\nt/link/f\n",
        "-R did not follow the symlink the walk found"
    );

    let _sock = std::os::unix::net::UnixListener::bind(dir.join("t").join("s"))?;
    let physical = grep_in(&dir, &["-rl", "a", "t"])?;
    assert_eq!(String::from_utf8_lossy(&physical.stdout), "t/g\n");
    assert_eq!(String::from_utf8_lossy(&physical.stderr), "", "-r opened the socket");
    let read_devices = grep_in(&dir, &["-Rl", "a", "t"])?;
    assert!(
        String::from_utf8_lossy(&read_devices.stderr).contains("t/s:"),
        "-R skipped the socket instead of reading it: {read_devices:?}"
    );
    Ok(())
}

/// Following symlinks is what lets a directory be reached from inside itself, so
/// the logical walk carries its ancestor chain. GNU WARNS and carries on — the
/// exit status is whatever the search concluded, here a match.
#[test]
fn grep_upper_r_warns_on_a_directory_loop_and_keeps_going(
) -> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new("grep-R-loop")?;
    std::fs::create_dir(dir.join("t"))?;
    std::fs::write(dir.join("t").join("f"), b"a\n")?;
    std::os::unix::fs::symlink(".", dir.join("t").join("self"))?;

    let out = grep_in(&dir, &["-Rl", "a", "t"])?;
    assert_eq!(out.status.code(), Some(0), "a loop made the status an error: {out:?}");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "t/f\n");
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        "grep: t/self: warning: recursive directory loop\n"
    );

    // A link to a SIBLING is two names for one directory without being a cycle,
    // and it is followed. Only an ancestor is refused -- which is why the test
    // above uses `.` and this one does not point back into `t`.
    std::fs::remove_file(dir.join("t").join("self"))?;
    std::fs::create_dir(dir.join("sib"))?;
    std::fs::write(dir.join("sib").join("g"), b"a\n")?;
    std::os::unix::fs::symlink("../sib", dir.join("t").join("side"))?;
    let sibling = grep_in(&dir, &["-Rl", "a", "t"])?;
    assert_eq!(
        String::from_utf8_lossy(&sibling.stdout),
        "t/f\nt/side/g\n",
        "a link to a sibling was refused as a cycle: {sibling:?}"
    );
    assert_eq!(String::from_utf8_lossy(&sibling.stderr), "");
    Ok(())
}

/// The line is drawn at the OPEN, not at the read: a directory grep opened and
/// could not read counts as a file it processed and found nothing in, while a
/// file it could not open at all does not. The corpus cannot express the second
/// half -- its harness materializes readable files -- so it lives here.
#[test]
fn grep_counts_what_it_opened_and_could_not_read_but_not_what_it_could_not_open(
) -> Result<(), Box<dyn std::error::Error>> {
    use std::os::unix::fs::PermissionsExt;
    let dir = TempDir::new("grep-unreadable")?;
    std::fs::create_dir(dir.join("d"))?;
    std::fs::write(dir.join("shut"), b"alpha\n")?;
    std::fs::set_permissions(dir.join("shut"), std::fs::Permissions::from_mode(0o000))?;

    // Running as root defeats the mode, and then the file is simply readable.
    let readable = std::fs::File::open(dir.join("shut")).is_ok();
    if !readable {
        let shut = grep_in(&dir, &["-c", "alpha", "shut"])?;
        assert_eq!(shut.status.code(), Some(2), "want exit 2, got {shut:?}");
        assert_eq!(
            String::from_utf8_lossy(&shut.stdout),
            "",
            "a file that never opened earned a count line"
        );
    }

    let opened = grep_in(&dir, &["-c", "alpha", "d"])?;
    assert_eq!(opened.status.code(), Some(2), "want exit 2, got {opened:?}");
    assert_eq!(String::from_utf8_lossy(&opened.stdout), "0\n");
    assert!(String::from_utf8_lossy(&opened.stderr).contains("Is a directory"));

    // The same rule reached through stdin, which is where the NAME in the
    // diagnostic matters: GNU blames `(standard input)`, never the `-`.
    let as_stdin = std::process::Command::new(bin())
        .arg("grep")
        .args(["-c", "alpha", "-"])
        .current_dir(&dir.0)
        .stdin(std::process::Stdio::from(std::fs::File::open(dir.join("d"))?))
        .output()?;
    assert_eq!(as_stdin.status.code(), Some(2), "want exit 2, got {as_stdin:?}");
    assert_eq!(String::from_utf8_lossy(&as_stdin.stdout), "0\n");
    assert_eq!(
        String::from_utf8_lossy(&as_stdin.stderr),
        "grep: (standard input): Is a directory\n"
    );
    Ok(())
}

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
const CORPUS_FLOOR: usize = 2273;

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

/// GNU's `#n` test for a FILE script is `prog.file && !prog.base && 2 ==
/// ftell(prog.file)` -- an absolute offset -- so a `-f -` script on a
/// descriptor handed over ALREADY PART-READ does not carry the rule: the two bytes
/// begin what td-txt reads but not what the file holds. Seekability alone would
/// take it, and that half of the rule has no corpus case — the harness can hand a
/// case a pipe or a file, but not a pre-positioned one.
#[test]
fn sed_hash_n_wants_a_script_stream_that_started_at_its_beginning()
-> Result<(), Box<dyn std::error::Error>> {
    use std::io::Seek;
    let dir = TempDir::new("sed-hash-n-offset")?;
    std::fs::write(dir.join("data"), b"A\nB\n")?;
    std::fs::write(dir.join("s"), b"xyz#n\np\n")?;
    std::fs::write(dir.join("s0"), b"#n\np\n")?;

    let mut part_read = std::fs::File::open(dir.join("s"))?;
    part_read.seek(std::io::SeekFrom::Start(3))?;
    let out = std::process::Command::new(bin())
        .arg("sed")
        .args(["-f", "-", "data"])
        .current_dir(&dir.0)
        .stdin(std::process::Stdio::from(part_read))
        .output()?;
    assert!(out.status.success() && out.stderr.is_empty(), "{out:?}");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "A\nA\nB\nB\n", "{out:?}");

    // The control: the same two bytes at the file's own start DO carry it.
    let whole = std::fs::File::open(dir.join("s0"))?;
    let out0 = std::process::Command::new(bin())
        .arg("sed")
        .args(["-f", "-", "data"])
        .current_dir(&dir.0)
        .stdin(std::process::Stdio::from(whole))
        .output()?;
    assert!(out0.status.success() && out0.stderr.is_empty(), "{out0:?}");
    assert_eq!(String::from_utf8_lossy(&out0.stdout), "A\nB\n", "{out0:?}");
    Ok(())
}

/// Whether a `-f -` script can carry `#n` must be decided by STAT, never by opening
/// fd 0 again: a second open waits for a writer FOR EVER when stdin is a fifo,
/// which is how the first version of this hung. The hang cannot be provoked from a
/// test — std cannot make a fifo and the gate runs no third-party program — so the
/// property is guarded against the source, as td's other confinement tests do for
/// what no compiler checks. It is a TEXT guard and not a proof: it reads a LINE at
/// a time, so a reopen split across two -- a path bound to a variable, or a
/// multi-line `OpenOptions::new()...open(...)` -- is not something it can see, and
/// on one line it still only knows the path spellings and openers listed below.
/// Two reviewers in a row got a hanging reopen past it, one by BUILDING the path
/// and one by naming a DIFFERENT magic link to fd 0; each time the answer was to
/// widen what it matches rather than to claim the class was covered. Treat the
/// list as the guard's real extent, not as a statement about reopens in general.
/// It also reads `src/sed.rs` ALONE, so the same probe in another module is
/// invisible to it -- which is fine only while `read_script` lives here.
#[test]
fn sed_script_stdin_seekability_is_a_stat_not_a_reopen()
-> Result<(), Box<dyn std::error::Error>> {
    let src =
        std::fs::read_to_string(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/sed.rs"))?;
    let (_, after) = src.split_once("fn read_script(").ok_or("read_script is gone")?;
    // The EARLIEST top-level item ends the body, not `fn` alone: the function is
    // followed by its error type now, so stopping at the next `fn` would swallow
    // that and quietly widen what the assertion below is allowed to match.
    let end = ["\nfn ", "\nenum ", "\nstruct ", "\nimpl ", "\n#[derive"]
        .iter()
        .filter_map(|k| after.find(k))
        .min()
        .ok_or("read_script has no end")?;
    let body = after.get(..end).ok_or("read_script has no end")?;
    assert!(
        body.contains("metadata(\"/proc/self/fd/0\")"),
        "the stdin arm must ask stat, which does not join a fifo's open handshake"
    );
    // Nowhere in the file may a name for fd 0 reach something that OPENS it. Only
    // code lines: a comment naming both is discussion, not a reopen. `fs::read(`
    // carries its paren so `read_dir`/`read_link` are not swept up with it, and
    // the opener list is by SUFFIX -- `::open(`, `.open(` -- so `File::options()`
    // and any other builder ending in `.open(` are caught too, which naming the
    // constructors was not. The path list is `/fd/` rather than any spelling of
    // the whole name: `/proc/self/fd/0` is only ONE of the magic links to fd 0,
    // and `/proc/<pid>/fd/0`, `/proc/thread-self/fd/0` and `/dev/fd/0` are the
    // same descriptor by other names. Two reviewers in a row got a HANGING reopen
    // past this scan, the first by building the path with `format!` and the
    // second by picking a different link, so what it matches is now the segment
    // they all share. There is no longer an exception to carve out: `R`'s source
    // reaches fd 0 through the special-file table, so no line NAMES fd 0 and
    // opens something. `read_source` still opens whatever name it is handed, and
    // outside POSIXLY_EXTENDED that name can be `/dev/stdin` again — which is
    // what GNU does there too, hang and all.
    for line in src.lines() {
        if line.trim_start().starts_with("//") {
            continue;
        }
        let names_fd0 = ["/dev/stdin", "/fd/"].iter().any(|n| line.contains(n));
        let opens = [
            "::open(",
            ".open(",
            "OpenOptions",
            "fs::read(",
            "read_to_string(",
            "fs::copy(",
        ]
        .iter()
        .any(|o| line.contains(o));
        assert!(!(names_fd0 && opens), "fd 0 must not be reopened: {line}");
    }
    Ok(())
}

/// The `-f` script's NAME reaches the diagnostic RAW, end to end. `locus_at`'s
/// own test builds the `Origin::File` by hand, so it pins the formatting and not
/// the plumbing that fills it: a review turned `Origin::File(f.clone())` into a
/// lossy re-encoding of the same name and the whole suite stayed green while
/// diverging from GNU. The case files cannot say it -- an annotation names its
/// file as text -- but a test can, `OsStr::from_bytes` naming one fine.
#[test]
fn a_f_script_named_in_non_utf8_is_reported_raw() -> Result<(), Box<dyn std::error::Error>> {
    use std::os::unix::ffi::OsStrExt;

    let dir = TempDir::new("rawname")?;
    let name = std::ffi::OsStr::from_bytes(b"h\xffi.sed");
    std::fs::write(dir.0.join(name), b"\x80p\n")?;

    let out = std::process::Command::new(bin())
        .arg("sed")
        .arg("-f")
        .arg(name)
        .current_dir(&dir.0)
        .output()?;
    assert_eq!(
        out.stderr,
        b"sed: file h\xffi.sed line 1: unknown command: `\x80'\n".to_vec(),
        "the -f name or the byte it quotes was not written raw"
    );
    assert_eq!(out.status.code(), Some(1));
    Ok(())
}

/// Every diagnostic that NAMES something off argv writes its bytes, as GNU's
/// `%s` does. A file name is not UTF-8 in general, and rendering one through
/// `from_utf8_lossy` replaced each stray byte with U+FFFD -- so the message
/// named a file the operator never passed, and a script grepping its own stderr
/// for the name it used found nothing. The case files cannot say this, an
/// annotation naming its file as text, so the shapes are driven from here;
/// every expectation below is GNU sed 4.9 / grep 3.11's own bytes.
#[test]
fn a_diagnostic_names_a_file_in_raw_bytes() -> Result<(), Box<dyn std::error::Error>> {
    use std::os::unix::ffi::OsStrExt;

    struct Row {
        applet: &'static str,
        args: Vec<Vec<u8>>,
        dirs: Vec<Vec<u8>>,
        files: Vec<(Vec<u8>, Vec<u8>)>,
        want: Vec<u8>,
        status: i32,
        /// The two option rows carry a usage line after the first, and its
        /// wording is a divergence of its own (spec/README).
        first_line_only: bool,
    }

    let bad = b"no\xffsuch".to_vec();
    let badd = b"d\xffir".to_vec();
    let mut wtarget = bad.clone();
    wtarget.extend_from_slice(b"/x");
    let mut wcmd = b"w ".to_vec();
    wcmd.extend_from_slice(&wtarget);
    let mut scmd = b"s/a/X/w ".to_vec();
    scmd.extend_from_slice(&wtarget);
    let mut rcmd = b"r ".to_vec();
    rcmd.extend_from_slice(&badd);
    let mut badbak = bad.clone();
    badbak.extend_from_slice(b".bak");
    let mut badbakx = badbak.clone();
    badbakx.extend_from_slice(b"/x");
    let mut longopt = b"--".to_vec();
    longopt.extend_from_slice(&bad);
    let mut ctxopt = b"--context=".to_vec();
    ctxopt.extend_from_slice(&bad);
    let mut fileopt = b"--file=".to_vec();
    fileopt.extend_from_slice(&bad);

    let row = |applet, args: Vec<Vec<u8>>, dirs: Vec<Vec<u8>>, files, want: &[u8], status| Row {
        applet,
        args,
        dirs,
        files,
        want: want.to_vec(),
        status,
        first_line_only: false,
    };
    let mut rows = vec![
        row(
            "sed",
            vec![b"p".to_vec(), bad.clone()],
            vec![],
            vec![],
            b"sed: can't read no\xffsuch: No such file or directory",
            2,
        ),
        row(
            "sed",
            vec![b"p".to_vec(), badd.clone()],
            vec![badd.clone()],
            vec![],
            b"sed: read error on d\xffir: Is a directory",
            4,
        ),
        row(
            "sed",
            vec![b"-i".to_vec(), b"p".to_vec(), bad.clone()],
            vec![],
            vec![],
            b"sed: can't read no\xffsuch: No such file or directory",
            2,
        ),
        row(
            "sed",
            vec![wcmd, b"IN".to_vec()],
            vec![],
            vec![(b"IN".to_vec(), b"a\n".to_vec())],
            b"sed: couldn't open file no\xffsuch/x: No such file or directory",
            4,
        ),
        row(
            "sed",
            vec![scmd, b"IN".to_vec()],
            vec![],
            vec![(b"IN".to_vec(), b"a\n".to_vec())],
            b"sed: couldn't open file no\xffsuch/x: No such file or directory",
            4,
        ),
        row(
            "grep",
            vec![b"x".to_vec(), bad.clone()],
            vec![],
            vec![],
            b"grep: no\xffsuch: No such file or directory",
            2,
        ),
        row(
            "grep",
            vec![b"x".to_vec(), badd.clone()],
            vec![badd.clone()],
            vec![],
            b"grep: d\xffir: Is a directory",
            2,
        ),
        row(
            "grep",
            vec![b"x".to_vec(), bad.clone()],
            vec![],
            vec![(bad.clone(), b"x\x00\n".to_vec())],
            b"grep: no\xffsuch: binary file matches",
            0,
        ),
        row(
            "grep",
            vec![b"-f".to_vec(), bad.clone(), b"IN".to_vec()],
            vec![],
            vec![(b"IN".to_vec(), b"a\n".to_vec())],
            b"grep: no\xffsuch: No such file or directory",
            2,
        ),
        row(
            "grep",
            vec![fileopt, b"IN".to_vec()],
            vec![],
            vec![(b"IN".to_vec(), b"a\n".to_vec())],
            b"grep: no\xffsuch: No such file or directory",
            2,
        ),
        row(
            "grep",
            vec![ctxopt, b"x".to_vec(), b"IN".to_vec()],
            vec![],
            vec![(b"IN".to_vec(), b"a\n".to_vec())],
            b"grep: no\xffsuch: invalid context length argument",
            2,
        ),
        // Four more sites, one row each: the same wording can be raised from
        // more than one place, and a row only defends the site it reaches.
        // `-s` reads its operands through a different arm than a lone one.
        row(
            "sed",
            vec![b"-s".to_vec(), b"p".to_vec(), badd.clone()],
            vec![badd.clone()],
            vec![],
            b"sed: read error on d\xffir: Is a directory",
            4,
        ),
        // `r` names its file through `read_source`, a third such site.
        row(
            "sed",
            vec![rcmd, b"IN".to_vec()],
            vec![badd.clone()],
            vec![(b"IN".to_vec(), b"a\n".to_vec())],
            b"sed: read error on d\xffir: Is a directory",
            4,
        ),
        // `-i` refuses before the read, so this is `couldn't edit` and not the
        // `can't read` two rows above.
        row(
            "sed",
            vec![b"-i".to_vec(), b"p".to_vec(), badd.clone()],
            vec![badd.clone()],
            vec![],
            b"sed: couldn't edit d\xffir: not a regular file",
            4,
        ),
        // The `-C`/`-A`/`-B` count, which is a different site from `--context=`.
        row(
            "grep",
            vec![b"-C".to_vec(), bad.clone(), b"x".to_vec(), b"IN".to_vec()],
            vec![],
            vec![(b"IN".to_vec(), b"a\n".to_vec())],
            b"grep: no\xffsuch: invalid context length argument",
            2,
        ),
        // The BACKUP rename, reached by making the backup NAME a non-empty
        // directory. The opening line of this landing advertises this message,
        // so it should not be the one nothing reaches.
        row(
            "sed",
            vec![b"-i.bak".to_vec(), b"s/a/b/".to_vec(), bad.clone()],
            vec![badbak.clone()],
            vec![(bad.clone(), b"a\n".to_vec()), (badbakx, b"z\n".to_vec())],
            b"sed: cannot rename no\xffsuch: Is a directory",
            4,
        ),
    ];
    // The unrecognized-option pair, whose second line is the recorded usage gap.
    for (applet, status) in [("sed", 1), ("grep", 2)] {
        let mut args = vec![longopt.clone()];
        if applet == "sed" {
            args.push(b"p".to_vec());
        } else {
            args.push(b"x".to_vec());
        }
        args.push(b"IN".to_vec());
        let mut want = applet.as_bytes().to_vec();
        want.extend_from_slice(b": unrecognized option '--no\xffsuch'");
        rows.push(Row {
            applet,
            args,
            dirs: vec![],
            files: vec![(b"IN".to_vec(), b"a\n".to_vec())],
            want,
            status,
            first_line_only: true,
        });
    }

    for (i, r) in rows.iter().enumerate() {
        let dir = TempDir::new(&format!("rawname-{i}"))?;
        for d in &r.dirs {
            std::fs::create_dir_all(dir.0.join(std::ffi::OsStr::from_bytes(d)))?;
        }
        for (name, data) in &r.files {
            std::fs::write(dir.0.join(std::ffi::OsStr::from_bytes(name)), data)?;
        }
        let mut cmd = std::process::Command::new(bin());
        cmd.arg(r.applet);
        for a in &r.args {
            cmd.arg(std::ffi::OsStr::from_bytes(a));
        }
        let out = cmd.current_dir(&dir.0).output()?;
        let (got, want) = if r.first_line_only {
            let first = out.stderr.split(|b| *b == b'\n').next().unwrap_or_default().to_vec();
            (first, r.want.clone())
        } else {
            let mut want = r.want.clone();
            want.push(b'\n');
            (out.stderr.clone(), want)
        };
        assert_eq!(
            got,
            want,
            "row {i} ({} {:?}) did not name the file the way GNU does",
            r.applet,
            r.args.iter().map(|a| String::from_utf8_lossy(a).into_owned()).collect::<Vec<_>>()
        );
        assert_eq!(out.status.code(), Some(r.status), "row {i} status");
    }
    Ok(())
}

/// Two more sites the same landing touched, held to td-txt's OWN bytes rather
/// than to GNU's: each diverges from GNU somewhere the corpus cannot compare
/// (spec/README), so GNU is not the whole oracle here and the name and the
/// shape are what is under test. Without these the label splice and the
/// hand-rolled possibilities join -- which replaced a `join(" ")` that could
/// not carry bytes -- are defended by nothing.
#[test]
fn a_diverging_diagnostic_still_names_in_raw_bytes() -> Result<(), Box<dyn std::error::Error>> {
    use std::os::unix::ffi::OsStrExt;

    let dir = TempDir::new("rawname-diverge")?;
    std::fs::write(dir.0.join("IN"), b"a\n")?;

    // A branch to a label nothing defines. GNU words this `can't find label for
    // jump to', but writes the NAME the same way -- raw.
    let out = std::process::Command::new(bin())
        .arg("sed")
        .arg(std::ffi::OsStr::from_bytes(b"b no\xffsuch"))
        .arg("IN")
        .current_dir(&dir.0)
        .output()?;
    assert_eq!(
        out.stderr,
        b"sed: can't operate on label `no\xffsuch'\n".to_vec(),
        "the unresolved label was not named raw"
    );
    assert_eq!(out.status.code(), Some(4));

    // The ambiguity list. GNU names itself by argv[0] and td-txt by the applet,
    // so the LINE still diverges; everything after that prefix does not, which
    // is what the corpus case pins. Here it is spelled with a raw byte in the
    // VALUE, which is the only way one reaches this message -- the abbreviation
    // itself has to prefix a real option name, and every one of those is ASCII.
    // That covers both halves the raw-name landing rewrote: the name splice and
    // the hand-rolled join, one space between possibilities each spelled
    // `'--name'`, which replaced a `join(" ")` over `String`s that could not
    // carry the byte.
    let amb = std::process::Command::new(bin())
        .arg("sed")
        .arg(std::ffi::OsStr::from_bytes(b"--s=\xff"))
        .args(["p", "IN"])
        .current_dir(&dir.0)
        .output()?;
    // The usage line follows it, and its wording is a divergence of its own.
    let first = amb.stderr.split(|b| *b == b'\n').next().unwrap_or_default().to_vec();
    assert_eq!(
        first,
        b"sed: option '--s=\xff' is ambiguous; possibilities: '--silent' '--sandbox' '--separate'"
            .to_vec(),
        "the ambiguous name lost its raw byte, or the join lost a separator or a quote"
    );
    assert_eq!(amb.status.code(), Some(1));

    // The scratch file `-i` creates, reached with a directory this process may
    // not write. GNU and td-txt spell the temp NAME differently (`sedXMHTSD`
    // against `sed<pid>.tmp`) and that is not a contract, so what is pinned is
    // the raw directory component in front of it.
    use std::os::unix::fs::PermissionsExt;
    let ro = TempDir::new("rawname-tmpfail")?;
    let sub = ro.0.join(std::ffi::OsStr::from_bytes(b"d\xffir"));
    std::fs::create_dir_all(&sub)?;
    std::fs::write(sub.join("IN"), b"a\n")?;
    std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o500))?;
    let tmp = std::process::Command::new(bin())
        .arg("sed")
        .args(["-i", "s/a/b/"])
        .arg(std::ffi::OsStr::from_bytes(b"d\xffir/IN"))
        .current_dir(&ro.0)
        .output()?;
    // Put it back before `TempDir` tries to remove the tree.
    std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o755))?;
    assert!(
        tmp.stderr.starts_with(b"sed: couldn't open temporary file d\xffir/sed"),
        "the scratch file's directory was not named raw: {:?}",
        String::from_utf8_lossy(&tmp.stderr)
    );
    assert_eq!(tmp.status.code(), Some(4));
    Ok(())
}

/// grep STREAMS: it reports a match without waiting for end of input. The
/// corpus cannot ask for this — a case supplies stdin as bytes and the harness
/// closes it, and "did not wait" is exactly a question about a stream that has
/// not ended.
///
/// The write end is held OPEN for the whole assertion, so a grep that read to
/// EOF before searching would still be blocked when the deadline passes. Before
/// this landed that is what happened, which is the same defect as
/// `grep -q a /dev/urandom` never returning.
#[test]
fn grep_answers_from_a_stream_that_has_not_ended() -> Result<(), Box<dyn std::error::Error>> {
    use std::io::Write;
    for args in [&["-q", "hit"][..], &["-m", "1", "hit"], &["-l", "hit"]] {
        let mut child = std::process::Command::new(bin())
            .arg("grep")
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .spawn()?;
        let mut sink = child.stdin.take().ok_or("no stdin pipe")?;
        sink.write_all(b"hit\n")?;
        sink.flush()?;
        // `sink` stays alive: the pipe is still open and no EOF is coming.
        let mut waited = 0;
        let code = loop {
            if let Some(st) = child.try_wait()? {
                break st.code();
            }
            if waited >= 10_000 {
                let _ = child.kill();
                return Err(format!("grep {args:?} never answered while the pipe stayed open").into());
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
            waited += 20;
        };
        assert_eq!(code, Some(0), "grep {args:?} answered, but not with a match");
        drop(sink);
        let _ = child.wait();
    }
    Ok(())
}

/// What a `q` LEAVES IN A PIPE, which is the whole point of reading a block at a
/// time rather than a file at a time. A seek cannot serve this — a pipe has
/// nowhere to put back what it was given — so the only fix is not over-reading,
/// and the only way to observe it is a SECOND reader on the same pipe.
///
/// The corpus cannot ask for this: it runs one child and closes its stdin. Here
/// the pipe's read end is duplicated, one copy going to sed and one staying
/// behind, so what sed did not take is still readable afterwards. Measured
/// against GNU sed 4.9, which leaves the same 15904 bytes.
///
/// The feeder is a THREAD that keeps writing while this one drains, which is
/// what makes the test independent of how big a pipe the kernel gave us: a
/// producer that had to fit the whole feed before anybody read would block for
/// ever on a one-page pipe, which is what a user over `pipe-user-pages-soft`
/// gets. It hands over after the first BLOCK so sed's one read cannot see a
/// short buffer — the assertion below is about exactly which bytes are left.
#[test]
fn sed_leaves_the_rest_of_a_pipe_for_the_next_reader()
-> Result<(), Box<dyn std::error::Error>> {
    use std::io::{Read, Write};
    // 2500 lines of 8 bytes = 20000, several blocks, so the leftover is exact.
    let feed: Vec<u8> = (0..2500u32).flat_map(|i| format!("{i:07}\n").into_bytes()).collect();
    // `-s` opens its operands on a path of its own, so it is asked separately.
    for args in [&["1q"][..], &["-s", "1q"][..]] {
        let (reader, writer) = std::io::pipe()?;
        let mut mine = reader.try_clone()?;

        let (tx, rx) = std::sync::mpsc::channel();
        let sending = feed.clone();
        let feeder = std::thread::spawn(move || -> std::io::Result<()> {
            let mut writer = writer;
            let (head, tail) = sending.split_at(4096.min(sending.len()));
            writer.write_all(head)?;
            let _ = tx.send(());
            writer.write_all(tail)
        });
        rx.recv()?;

        let out = std::process::Command::new(bin())
            .arg("sed")
            .args(args)
            .stdin(std::process::Stdio::from(reader))
            .output()?;
        assert_eq!(out.stdout, b"0000000\n".to_vec());

        let mut rest = Vec::new();
        mine.read_to_end(&mut rest)?;
        feeder.join().map_err(|_| "feeder panicked")??;
        assert_eq!(
            rest.len(),
            feed.len() - 4096,
            "sed {args:?} took {} bytes off the pipe, not one 4 KiB block",
            feed.len() - rest.len()
        );
        // And what is left starts exactly where the block ended, not at a record
        // boundary -- GNU leaves a partial line here too.
        assert_eq!(rest.get(..8), feed.get(4096..4104));
    }
    Ok(())
}

/// sed answers from a stream that has not ended, for the same reason grep does
/// and pinned the same way: the write end stays OPEN for the whole assertion, so
/// a sed that read to EOF before its first cycle would still be blocked when the
/// deadline passes. Before the incremental reader landed, that is what happened —
/// `sed 1q /dev/urandom` never returned.
///
/// `-s` is here because it is a SECOND reader over the same operands: the path
/// that restarts line numbers per file opens each one itself, and it read them
/// whole for one commit longer than this one — `sed -s 1q /dev/urandom` hung
/// where the plain form already returned.
#[test]
fn sed_answers_from_a_stream_that_has_not_ended() -> Result<(), Box<dyn std::error::Error>> {
    use std::io::Write;
    for (script, sep) in [("1q", false), ("1{p;q}", false), ("$!{1q}", false), ("1q", true)] {
        let mut args = vec!["-n"];
        if sep {
            args.push("-s");
        }
        args.extend(["-e", script]);
        let mut child = std::process::Command::new(bin())
            .arg("sed")
            .args(&args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .spawn()?;
        let mut sink = child.stdin.take().ok_or("no stdin pipe")?;
        sink.write_all(b"one\ntwo\n")?;
        sink.flush()?;
        let mut waited = 0;
        loop {
            if child.try_wait()?.is_some() {
                break;
            }
            if waited >= 10_000 {
                let _ = child.kill();
                return Err(format!("sed {args:?} {script:?} never answered on an open pipe").into());
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
            waited += 20;
        }
        drop(sink);
        let _ = child.wait();
    }
    Ok(())
}

/// The binary verdict follows the STREAM: a NUL makes every match from the
/// buffer it was found in onward one notice, and matches already printed stay
/// printed. Needs an operand bigger than a buffer, which no corpus case is.
///
/// What is asserted is the RULE, not the arithmetic. GNU grows its read buffer
/// to hold the longest line, so exactly which matches fall on which side of the
/// flip moves with the data; these two cases are far enough apart (a match in
/// the first buffer, a NUL 150 KiB later) that no buffer size in the plausible
/// range puts them together.
#[test]
fn a_late_nul_does_not_unprint_an_early_match() -> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new("stream-binary")?;
    let mut buf = vec![b'p'; 300_000];
    for i in (0..buf.len()).step_by(80) {
        if let Some(b) = buf.get_mut(i) {
            *b = b'\n';
        }
    }
    let put = |buf: &mut Vec<u8>, at: usize, s: &[u8]| {
        for (i, b) in s.iter().enumerate() {
            if let Some(slot) = buf.get_mut(at + i) {
                *slot = *b;
            }
        }
    };
    put(&mut buf, 100, b"\nmatchme\n");
    put(&mut buf, 250_000, b"\n\x00\n");
    std::fs::write(dir.0.join("mixed"), &buf)?;
    let out = std::process::Command::new(bin())
        .arg("grep")
        .args(["matchme", "mixed"])
        .current_dir(&dir.0)
        .output()?;
    assert_eq!(
        out.stdout,
        b"matchme\n".to_vec(),
        "the early match was swallowed by a NUL 150 KiB after it"
    );
    assert_eq!(out.status.code(), Some(0));

    // The same file with the NUL FIRST is binary throughout, notice and all.
    let mut early = buf.clone();
    put(&mut early, 50, b"\n\x00\n");
    std::fs::write(dir.0.join("early"), &early)?;
    let out = std::process::Command::new(bin())
        .arg("grep")
        .args(["matchme", "early"])
        .current_dir(&dir.0)
        .output()?;
    assert_eq!(out.stdout, Vec::new(), "a NUL before the match still printed the line");
    assert_eq!(out.stderr, b"grep: early: binary file matches\n".to_vec());

    // STICKY: a NUL in one buffer keeps the file binary even where the buffer
    // holding the match has none of its own. Verified against GNU 3.11, which
    // reports the notice here rather than the line. Without this the flag could
    // be cleared by the next clean buffer and every other assertion in this
    // file would still pass.
    let mut late = vec![b'p'; 400_000];
    for i in (0..late.len()).step_by(80) {
        if let Some(b) = late.get_mut(i) {
            *b = b'\n';
        }
    }
    put(&mut late, 100_000, b"\n\x00\n");
    put(&mut late, 250_000, b"\nmatchme\n");
    std::fs::write(dir.0.join("sticky"), &late)?;
    let out = std::process::Command::new(bin())
        .arg("grep")
        .args(["matchme", "sticky"])
        .current_dir(&dir.0)
        .output()?;
    assert_eq!(
        out.stdout,
        Vec::new(),
        "a NUL-free later buffer made the file text again and printed the line"
    );
    assert_eq!(out.stderr, b"grep: sticky: binary file matches\n".to_vec());
    Ok(())
}

/// `-B` context spanning a buffer boundary. The window is a ring of copies now
/// rather than a slice of the whole file, so a match just past a boundary must
/// still print the lines that preceded it from the buffer BEFORE — the one case
/// where the streaming rewrite could silently drop output.
#[test]
fn before_context_survives_a_buffer_boundary() -> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new("stream-ctx")?;
    // Lines of a fixed width so the boundary lands mid-file at a known line.
    let width = 100;
    let per_buf = (96 << 10) / width;
    let mut text = Vec::new();
    let hit = per_buf + 3;
    for n in 1..=(per_buf + 10) {
        let body = if n == hit { b'h' } else { b'.' };
        text.extend_from_slice(&vec![body; width - 1]);
        text.push(b'\n');
    }
    std::fs::write(dir.0.join("wide"), &text)?;
    let out = std::process::Command::new(bin())
        .arg("grep")
        .args(["-n", "-B", "3", &"h".repeat(width - 1), "wide"])
        .current_dir(&dir.0)
        .output()?;
    let got = String::from_utf8_lossy(&out.stdout);
    let nums: Vec<&str> = got.lines().filter_map(|l| l.split(['-', ':']).next()).collect();
    assert_eq!(
        nums,
        vec![
            (hit - 3).to_string(),
            (hit - 2).to_string(),
            (hit - 1).to_string(),
            hit.to_string()
        ],
        "the -B window lost lines across the buffer boundary: {got:?}"
    );
    Ok(())
}

/// `-d skip` is decided from the OPENED descriptor, not from the name: a
/// directory this process may not open is reported like any other unopenable
/// operand, at exit 2. A corpus case cannot ask for it — nothing in the format
/// sets a mode — and getting it wrong is silent, which is the whole point.
///
/// The `-R` dereference is checked here for the same reason: the format makes no
/// symlinks, so the claim that `logical` survives a later `-d recurse` has
/// nowhere else to live.
#[test]
fn directory_actions_answer_from_the_descriptor_and_leave_r_sticky()
-> Result<(), Box<dyn std::error::Error>> {
    use std::os::unix::fs::PermissionsExt;
    let dir = TempDir::new("dirs-desc")?;
    std::fs::write(dir.0.join("f1"), b"a\n")?;
    let shut = dir.0.join("noperm");
    std::fs::create_dir_all(&shut)?;
    std::fs::set_permissions(&shut, std::fs::Permissions::from_mode(0o000))?;

    let run = |args: &[&str]| -> Result<(i32, Vec<u8>, Vec<u8>), Box<dyn std::error::Error>> {
        let out = std::process::Command::new(bin())
            .arg("grep")
            .args(args)
            .current_dir(&dir.0)
            .output()?;
        Ok((out.status.code().unwrap_or(-1), out.stdout, out.stderr))
    };

    // Silent + exit 1 is what a name-based stat gives; GNU gives both of these.
    for args in [&["-d", "skip", "a", "noperm"][..], &["-c", "-d", "skip", "a", "noperm"]] {
        let (code, _, stderr) = run(args)?;
        assert_eq!(code, 2, "{args:?} did not report the unopenable directory");
        assert_eq!(stderr, b"grep: noperm: Permission denied\n".to_vec(), "{args:?}");
    }
    // `-s` suppresses the message and KEEPS the status, as it does elsewhere.
    let (code, _, stderr) = run(&["-s", "-d", "skip", "a", "noperm"])?;
    assert_eq!((code, stderr), (2, Vec::new()));
    // A directory it CAN open is still passed over without a word, and a good
    // operand beside it still succeeds — the skip must not touch the status.
    std::fs::set_permissions(&shut, std::fs::Permissions::from_mode(0o755))?;
    let (code, stdout, stderr) = run(&["-d", "skip", "a", "f1", "noperm"])?;
    assert_eq!((code, stdout, stderr), (0, b"f1:a\n".to_vec(), Vec::new()));

    // `-R`'s dereference is sticky: a LATER `-d recurse` re-asks for the descent
    // and says nothing about symlinks, so the walk still follows one it finds.
    // `-r` alone does not, which is what makes this an assertion about `-R`.
    let tree = TempDir::new("dirs-sticky")?;
    std::fs::create_dir_all(tree.0.join("t/real"))?;
    std::fs::write(tree.0.join("t/real/hit"), b"a\n")?;
    std::os::unix::fs::symlink("real", tree.0.join("t/link"))?;
    let walked = |args: &[&str]| -> Result<Vec<u8>, Box<dyn std::error::Error>> {
        let out = std::process::Command::new(bin())
            .arg("grep")
            .args(args)
            .current_dir(&tree.0)
            .output()?;
        let mut lines: Vec<Vec<u8>> =
            out.stdout.split(|b| *b == b'\n').filter(|l| !l.is_empty()).map(<[u8]>::to_vec).collect();
        lines.sort();
        Ok(lines.join(&b'\n'))
    };
    let follows = b"t/link/hit:a\nt/real/hit:a".to_vec();
    let does_not = b"t/real/hit:a".to_vec();
    assert_eq!(walked(&["-R", "-d", "recurse", "a", "t"])?, follows, "-R lost its deref to -d");
    assert_eq!(walked(&["-d", "recurse", "-R", "a", "t"])?, follows);
    assert_eq!(walked(&["-R", "a", "t"])?, follows);
    assert_eq!(walked(&["-d", "recurse", "a", "t"])?, does_not, "-d recurse followed a symlink");
    assert_eq!(walked(&["-r", "a", "t"])?, does_not);
    Ok(())
}

/// `-d`'s bad-argument diagnostic ESCAPES what it quotes, which is the half of
/// GNU's `quote()` a corpus case cannot ask for: `## argv:` splits on
/// whitespace, so no case can put a newline — the byte the escape exists for —
/// into an option's value.
///
/// Measured against GNU grep 3.11 under `LC_ALL=C`, the locale every golden here
/// was derived under and the only one td's image sets. A UTF-8 locale would pick
/// U+2018/U+2019 for the quotes; the escaping is the same either way.
#[test]
fn a_bad_directories_argument_is_quoted_the_way_gnu_quotes_it()
-> Result<(), Box<dyn std::error::Error>> {
    use std::os::unix::ffi::OsStrExt;
    let dir = TempDir::new("dirs-quote")?;
    std::fs::write(dir.0.join("IN"), b"a\n")?;

    // A newline is the one that MATTERS: unescaped it ends the diagnostic early
    // and the rest reads as a second one. A backslash, a quote and a high byte
    // cover the three other arms; `\2001` pins the octal at three digits, since
    // two would read back as a different byte followed by nothing.
    let cases: [(&[u8], &[u8]); 5] = [
        (b"b\no", br"'b\no'"),
        (b"b\\o", br"'b\\o'"),
        (b"b'o", br"'b\'o'"),
        (b"b\xffo", br"'b\377o'"),
        (b"\x801", br"'\2001'"),
    ];
    for (value, quoted) in cases {
        let out = std::process::Command::new(bin())
            .arg("grep")
            .arg("-d")
            .arg(std::ffi::OsStr::from_bytes(value))
            .args(["a", "IN"])
            .current_dir(&dir.0)
            .output()?;
        let mut want = b"grep: invalid argument ".to_vec();
        want.extend_from_slice(quoted);
        want.extend_from_slice(b" for '--directories'\n");
        want.extend_from_slice(b"Valid arguments are:\n  - 'read'\n  - 'recurse'\n  - 'skip'\n");
        assert!(
            out.stderr.starts_with(&want),
            "argument {value:?} was not quoted as GNU quotes it: {:?}",
            String::from_utf8_lossy(&out.stderr)
        );
        // The message is ONE line: an unescaped byte would make it two.
        assert_eq!(
            out.stderr.iter().filter(|b| **b == b'\n').count(),
            // The quoted line, `Valid arguments are:`, three names, the usage.
            6,
            "the diagnostic gained or lost a line for {value:?}"
        );
        assert_eq!(out.status.code(), Some(1));
    }
    Ok(())
}

/// `-f -` reading a DIRECTORY is the same deliberate refusal the four
/// `spec/divergence.test.txt` cases pin for the named spelling, and the corpus
/// cannot say it: a case supplies stdin as BYTES, so there is no way to
/// annotate "stdin is a directory". GNU auto-prints at exit 0 here.
///
/// It names the STREAM rather than the `-` it was spelled as, which is GNU's own
/// machinery and not a preference: `compile_file` binds the name `-` alone to
/// `stdin`, and `utils_fp_name` reports that stream as `stdin`.
#[test]
fn a_dash_f_script_read_from_a_directory_is_refused(
) -> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new("fdash-dir")?;
    let sub = dir.0.join("D");
    std::fs::create_dir_all(&sub)?;
    std::fs::write(dir.0.join("IN"), b"a\nb\n")?;

    let out = std::process::Command::new(bin())
        .args(["sed", "-f", "-", "IN"])
        .stdin(std::process::Stdio::from(std::fs::File::open(&sub)?))
        .current_dir(&dir.0)
        .output()?;
    assert_eq!(
        out.stderr,
        b"sed: read error on stdin: Is a directory\n".to_vec(),
        "a directory on stdin was not refused the way a named one is"
    );
    assert_eq!(out.stdout, b"".to_vec());
    assert_eq!(out.status.code(), Some(4));
    Ok(())
}

/// `R /dev/stdin` reads the descriptor it was GIVEN and not the name reopened,
/// which is the whole point of the aliasing — and a corpus case cannot show it.
/// The harness hands the child a PIPE, and `/proc/self/fd/0` on a pipe reopens
/// the same pipe, so the two are indistinguishable there however the read is
/// done. A regular file positioned PAST its start tells them apart with nothing
/// but `std`: the descriptor continues from where it was left, while reopening
/// starts over and hands back the head as well.
///
/// The failure this stands in for needs a fifo, which no safe API here can make:
/// where standard input is one whose writer has gone, fd 0 is at end of file
/// while opening the name waits for a writer that never comes, and the shell
/// hangs outright.
#[test]
fn an_r_source_of_dev_stdin_continues_the_descriptor(
) -> Result<(), Box<dyn std::error::Error>> {
    use std::io::Seek as _;

    let dir = TempDir::new("r-stdin-offset")?;
    std::fs::write(dir.0.join("IN"), b"A\nB\n")?;
    std::fs::write(dir.0.join("S"), b"HEAD\nTAIL1\nTAIL2\n")?;
    let mut handed = std::fs::File::open(dir.0.join("S"))?;
    handed.seek(std::io::SeekFrom::Start(5))?;

    let out = std::process::Command::new(bin())
        .args(["sed", "-n", "-e", "R /dev/stdin", "-e", "p", "IN"])
        .stdin(std::process::Stdio::from(handed))
        .current_dir(&dir.0)
        .output()?;
    assert_eq!(
        out.stdout,
        b"A\nTAIL1\nB\nTAIL2\n".to_vec(),
        "the name was reopened: the head came back with the tail"
    );
    assert_eq!(out.stderr, b"".to_vec());
    assert_eq!(out.status.code(), Some(0));

    // The same argv OUTSIDE POSIXLY_EXTENDED, where GNU consults no table and
    // `R` opens the name: the reopen starts at zero, so the head it skipped
    // above comes back. `--posix` cannot show this — it withdraws `R` — so
    // `POSIXLY_CORRECT` is the only reachable spelling of the gate's other side,
    // and without this the gate could be dropped from the READ path with every
    // case and test still green.
    let mut handed = std::fs::File::open(dir.0.join("S"))?;
    handed.seek(std::io::SeekFrom::Start(5))?;
    let unaliased = std::process::Command::new(bin())
        .args(["sed", "-n", "-e", "R /dev/stdin", "-e", "p", "IN"])
        .stdin(std::process::Stdio::from(handed))
        .env("POSIXLY_CORRECT", "1")
        .current_dir(&dir.0)
        .output()?;
    assert_eq!(
        unaliased.stdout,
        b"A\nHEAD\nB\nTAIL1\n".to_vec(),
        "the table was still consulted outside POSIXLY_EXTENDED"
    );
    assert_eq!(unaliased.status.code(), Some(0));
    Ok(())
}

/// The `-f` wording follows WHICH CALL failed and not the errno it failed with.
/// Every open failure the corpus pins is ENOENT and every read failure is
/// EISDIR, so those two agree perfectly with the two arms and an implementation
/// keyed on the errno passes all of them -- a review demonstrated exactly that.
/// These are open failures with neither errno, and both agree with GNU byte for
/// byte. The corpus cannot express either: its harness materializes readable
/// regular files, so there is no way to annotate a mode or a symlink.
#[test]
fn a_dash_f_open_failure_is_named_by_the_call_not_the_errno(
) -> Result<(), Box<dyn std::error::Error>> {
    use std::os::unix::fs::PermissionsExt;
    let dir = TempDir::new("fdash-openfail")?;
    std::fs::write(dir.join("IN"), b"a\n")?;

    // ELOOP, which no privilege defeats.
    std::os::unix::fs::symlink("loop2", dir.join("loop1"))?;
    std::os::unix::fs::symlink("loop1", dir.join("loop2"))?;
    let looped = std::process::Command::new(bin())
        .args(["sed", "-f", "loop1", "IN"])
        .current_dir(&dir.0)
        .output()?;
    assert_eq!(
        looped.stderr,
        b"sed: couldn't open file loop1: Too many levels of symbolic links\n".to_vec(),
        "an open that failed was reported as a read"
    );
    assert_eq!(looped.status.code(), Some(4));

    // EACCES, which root does defeat -- so the mode is checked rather than the uid.
    std::fs::write(dir.join("shut.sed"), b"p\n")?;
    std::fs::set_permissions(dir.join("shut.sed"), std::fs::Permissions::from_mode(0o000))?;
    if std::fs::File::open(dir.join("shut.sed")).is_err() {
        let shut = std::process::Command::new(bin())
            .args(["sed", "-f", "shut.sed", "IN"])
            .current_dir(&dir.0)
            .output()?;
        assert_eq!(
            shut.stderr,
            b"sed: couldn't open file shut.sed: Permission denied\n".to_vec(),
            "an open that failed was reported as a read"
        );
        assert_eq!(shut.status.code(), Some(4));
    }
    Ok(())
}

/// A read error on the aliased standard input names the STREAM and not the name
/// the script spelled, which is GNU answering from the stream it registered
/// rather than from the path. Uncaseable for the same reason as the test above:
/// a case supplies stdin as BYTES, and a directory is the reachable way to make
/// that read fail.
#[test]
fn an_r_read_error_on_standard_input_names_the_stream(
) -> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new("r-stdin-dir")?;
    let sub = dir.0.join("D");
    std::fs::create_dir_all(&sub)?;
    std::fs::write(dir.0.join("IN"), b"a\n")?;

    let out = std::process::Command::new(bin())
        .args(["sed", "-n", "-e", "R /dev/stdin", "-e", "p", "IN"])
        .stdin(std::process::Stdio::from(std::fs::File::open(&sub)?))
        .current_dir(&dir.0)
        .output()?;
    assert_eq!(
        out.stderr,
        b"sed: read error on stdin: Is a directory\n".to_vec(),
        "the failed read named the path rather than the stream"
    );
    assert_eq!(out.stdout, b"".to_vec());
    assert_eq!(out.status.code(), Some(4));
    Ok(())
}

/// Each special name's direction belongs to the STREAM and not to the descriptor
/// behind it: GNU decides both refusals in the C library, with no syscall made,
/// so opening standard output for reading as well does not make `R /dev/stdout`
/// readable and opening standard input for writing does not make `w /dev/stdin`
/// writable. A case cannot say it — the harness hands the child pipes, and this
/// needs a descriptor whose access mode contradicts its stream's.
///
/// The second half is also what guards the file behind standard input. Resolving
/// `/dev/stdin` as a PATH to create would TRUNCATE it, so the assertion on the
/// bytes afterwards is the one that would catch that coming back.
#[test]
fn a_special_stream_refuses_by_direction_not_by_descriptor(
) -> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new("special-direction")?;
    std::fs::write(dir.0.join("IN"), b"LINE\n")?;
    let both_ways = |name: &str| -> std::io::Result<std::fs::File> {
        std::fs::OpenOptions::new().read(true).write(true).open(dir.0.join(name))
    };

    std::fs::write(dir.0.join("OUT"), b"KEPT\n")?;
    let read_out = std::process::Command::new(bin())
        .args(["sed", "-n", "-e", "R /dev/stdout", "-e", "p", "IN"])
        .stdout(std::process::Stdio::from(both_ways("OUT")?))
        .current_dir(&dir.0)
        .output()?;
    assert_eq!(
        read_out.stderr,
        b"sed: read error on stdout: Bad file descriptor\n".to_vec(),
        "a readable descriptor made the write-only STREAM readable"
    );
    assert_eq!(read_out.status.code(), Some(4));
    assert_eq!(std::fs::read(dir.0.join("OUT"))?, b"KEPT\n".to_vec());

    std::fs::write(dir.0.join("DATA"), b"PRECIOUS\n")?;
    let write_in = std::process::Command::new(bin())
        .args(["sed", "-n", "-e", "w /dev/stdin", "-e", "p", "IN"])
        .stdin(std::process::Stdio::from(both_ways("DATA")?))
        .current_dir(&dir.0)
        .output()?;
    assert_eq!(
        write_in.stderr,
        b"sed: couldn't write 4 items to stdin: Bad file descriptor\n".to_vec(),
        "a writable descriptor made the read-only STREAM writable"
    );
    assert_eq!(write_in.stdout, b"".to_vec());
    assert_eq!(write_in.status.code(), Some(4));
    assert_eq!(
        std::fs::read(dir.0.join("DATA"))?,
        b"PRECIOUS\n".to_vec(),
        "the file behind standard input was opened as a path and truncated"
    );

    // The same refusal reached through a `v` rather than through the default
    // level, which is what makes this more than a repeat: under POSIXLY_CORRECT
    // the special-file table is withdrawn, so `w /dev/stdin` resolves as a PATH
    // and TRUNCATES whatever is redirected onto fd 0 -- and a `v` takes the
    // table back, which turns that into the refusal above. A corpus case can
    // only see the refusal: the harness gives every child a pipe, so there is no
    // file behind fd 0 to lose.
    std::fs::write(dir.0.join("KEEP"), b"PRECIOUS\n")?;
    let promoted = std::process::Command::new(bin())
        .args(["sed", "-n", "-e", "v", "-e", "w /dev/stdin", "-e", "p", "IN"])
        .env("POSIXLY_CORRECT", "1")
        .stdin(std::process::Stdio::from(both_ways("KEEP")?))
        .current_dir(&dir.0)
        .output()?;
    assert_eq!(
        promoted.stderr,
        b"sed: couldn't write 4 items to stdin: Bad file descriptor\n".to_vec(),
        "a v did not take the special-file table back"
    );
    assert_eq!(promoted.status.code(), Some(4));
    assert_eq!(
        std::fs::read(dir.0.join("KEEP"))?,
        b"PRECIOUS\n".to_vec(),
        "the file behind standard input was truncated through a promoted w"
    );
    Ok(())
}

/// A seekable standard input is left positioned after the last record sed
/// CONSUMED, not at end of file: POSIX's "shall not consume more input than it
/// needs", and what makes `{ sed 1q; cat; } < f` — read a header, hand the rest
/// to another program — the working idiom it is meant to be.
///
/// The corpus cannot say this. Its harness gives every child a PIPE, which is
/// exactly the case where there is nothing to give back and both sed and GNU
/// leave nothing, so the divergence is invisible there. This needs a REGULAR
/// file on descriptor 0 and a second descriptor onto the same file DESCRIPTION
/// to read what the child left behind — `try_clone` rather than a second
/// `File::open`, which would be an independent offset that could not observe the
/// child at all.
#[test]
fn sed_leaves_a_seekable_stdin_after_the_last_record_it_read(
) -> Result<(), Box<dyn std::error::Error>> {
    use std::io::Read;

    let dir = TempDir::new("sed-stdin-offset")?;
    std::fs::write(dir.0.join("four"), b"l1\nl2\nl3\nl4\n")?;
    // A last record with no separator: the accounting must count the bytes of a
    // record, not one separator per record, or this one is off by one.
    std::fs::write(dir.0.join("unterm"), b"u1\nu2\nu3")?;
    // `-z` makes the separator a NUL, so a row that counted `\n` would be wrong
    // by one per record here and right everywhere else.
    std::fs::write(dir.0.join("zrec"), b"z1\0z2\0z3\0")?;
    std::fs::write(dir.0.join("zunterm"), b"z1\0z2\0z3")?;
    // A named operand to pair stdin with, for the multi-operand rows.
    std::fs::write(dir.0.join("one"), b"ONE\n")?;

    let rest_after = |file: &str, args: &[&str]| -> Result<(Vec<u8>, Vec<u8>, Option<i32>), Box<dyn std::error::Error>> {
        let given = std::fs::File::open(dir.0.join(file))?;
        let mut mine = given.try_clone()?;
        let out = std::process::Command::new(bin())
            .arg("sed")
            .args(args)
            .current_dir(&dir.0)
            .stdin(std::process::Stdio::from(given))
            .output()?;
        let mut rest = Vec::new();
        mine.read_to_end(&mut rest)?;
        Ok((out.stdout, rest, out.status.code()))
    };

    // Each row is GNU sed 4.9's own answer, taken by running it the same way.
    let rows: &[(&str, &[&str], &[u8], &[u8])] = &[
        ("four", &["-n", "1q"], b"", b"l2\nl3\nl4\n"),
        ("four", &["-n", "3q"], b"", b"l4\n"),
        ("four", &["-n", "2Q"], b"", b"l3\nl4\n"),
        ("four", &["-n", "-e", "1{p;q}"], b"l1\n", b"l2\nl3\nl4\n"),
        // `N` consumes two records for one cycle, so what is owed back is not a
        // function of the cycle count.
        ("four", &["-n", "-e", "N;q"], b"", b"l3\nl4\n"),
        // `-s` reaches the input through a different construction of the same
        // stream, and must account for it the same way.
        ("four", &["-s", "-n", "1q"], b"", b"l2\nl3\nl4\n"),
        ("unterm", &["-n", "2q"], b"", b"u3"),
        // `-z`: the separator counted per record is the one in force, not `\n`.
        ("zrec", &["-n", "-z", "1q"], b"", b"z2\0z3\0"),
        ("zunterm", &["-n", "-z", "1q"], b"", b"z2\0z3"),
        // The `$` LOOKAHEAD is the third code path that can set the count, and
        // the only one where stdin is opened mid-cycle rather than by the reader:
        // `$!` on the last line of `one` opens `-`, reads it whole, and the `q`
        // then owes ALL of it back.
        ("four", &["-n", "-e", "1{$!p}", "-e", "1q", "one", "-"], b"ONE\n", b"l1\nl2\nl3\nl4\n"),
        // Multiple operands: only the OPEN one can owe anything, so which side
        // stdin is on decides how much.
        ("four", &["-n", "2q", "-", "one"], b"", b"l3\nl4\n"),
        ("four", &["-n", "2q", "one", "-"], b"", b"l2\nl3\nl4\n"),
        // Two dashes: the second open reads 0 bytes at end of file and must not
        // overwrite what the first one is owed.
        ("four", &["-n", "2q", "-", "-"], b"", b"l3\nl4\n"),
        // The controls. A script that reads to the end leaves nothing, which is
        // what says the rewind is not unconditional; and no `q` at all is the
        // same.
        ("four", &["-n", "$q"], b"", b""),
        ("four", &["-n", "p"], b"l1\nl2\nl3\nl4\n", b""),
    ];
    for (file, args, want_out, want_rest) in rows {
        let (stdout, rest, code) = rest_after(file, args)?;
        assert_eq!(code, Some(0), "{args:?} over {file}");
        assert_eq!(stdout, want_out.to_vec(), "{args:?} over {file}: stdout");
        assert_eq!(
            rest,
            want_rest.to_vec(),
            "{args:?} over {file}: what was left on descriptor 0"
        );
    }

    // A PIPE has nothing to give back, and asking must not be an error: the
    // give-back declines on anything but a regular file rather than failing.
    // stderr is PIPED, or the assertion below would inspect an empty buffer and
    // pass however loudly the child complained.
    let mut child = std::process::Command::new(bin())
        .args(["sed", "-n", "1q"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    {
        use std::io::Write;
        let mut w = child.stdin.take().ok_or("the child was given no stdin pipe")?;
        // Propagated: a write that never arrived would leave the child reading an
        // empty stream, where the give-back has nothing to decline in the first
        // place and the test would prove nothing.
        w.write_all(b"p1\np2\np3\n")?;
    }
    let piped = child.wait_with_output()?;
    assert_eq!(piped.status.code(), Some(0), "a pipe made the give-back fail");
    assert_eq!(piped.stderr, b"".to_vec(), "{piped:?}");
    Ok(())
}

/// The give-back survives a run that DIES: GNU leaves standard input after the
/// records it consumed even when it exits 4, so `sed -e '1r .'` — which reads one
/// record and then fails on a directory — must still hand the rest back. Held
/// separately from the rows above because it is the one case where the run's
/// result and the repositioning are both errors, and the run's is the one that
/// must be reported.
#[test]
fn a_run_that_dies_still_gives_back_what_it_did_not_read(
) -> Result<(), Box<dyn std::error::Error>> {
    use std::io::Read;

    let dir = TempDir::new("sed-stdin-offset-fatal")?;
    std::fs::write(dir.0.join("four"), b"l1\nl2\nl3\nl4\n")?;
    // Each row is GNU sed 4.9's own answer, all four parts of it. Both paths
    // reach the input through their own construction of the stream, so both have
    // to hold the run's result rather than propagate it; and the two exit codes
    // are different failure KINDS, a read that died at 4 and a script that could
    // not run at 1.
    let rows: &[(&[&str], i32, &[u8], &[u8], &[u8])] = &[
        (
            &["sed", "-e", "1r ."],
            4,
            b"l1\n",
            b"sed: read error on .: Is a directory\n",
            b"l2\nl3\nl4\n",
        ),
        (
            &["sed", "-s", "-e", "1r ."],
            4,
            b"l1\n",
            b"sed: read error on .: Is a directory\n",
            b"l2\nl3\nl4\n",
        ),
        (
            &["sed", "-n", "2s//X/"],
            1,
            b"",
            b"sed: -e expression #1, char 0: no previous regular expression\n",
            b"l3\nl4\n",
        ),
    ];
    for (args, status, want_out, want_err, want_rest) in rows {
        let given = std::fs::File::open(dir.0.join("four"))?;
        let mut mine = given.try_clone()?;
        let out = std::process::Command::new(bin())
            .args(*args)
            .current_dir(&dir.0)
            .stdin(std::process::Stdio::from(given))
            .output()?;
        let mut rest = Vec::new();
        mine.read_to_end(&mut rest)?;

        assert_eq!(out.status.code(), Some(*status), "{args:?}: {out:?}");
        assert_eq!(out.stdout, want_out.to_vec(), "{args:?}: {out:?}");
        assert_eq!(out.stderr, want_err.to_vec(), "{args:?}: {out:?}");
        assert_eq!(
            rest,
            want_rest.to_vec(),
            "{args:?}: a run that died left descriptor 0 at end of file"
        );
    }
    Ok(())
}

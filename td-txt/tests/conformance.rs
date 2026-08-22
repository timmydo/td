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
const CORPUS_FLOOR: usize = 2582;

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
/// fails silently by never returning. The walk no longer drops them — that is
/// `--devices`' answer now, and the DEFAULT is what this pins — so the silence
/// below is also the proof that nothing opened the socket.
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
/// found by the walk is skipped. `-R` follows both, and is no longer a bare
/// synonym for `-r`: it also flips the `--devices` default (grep.c:3007).
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
    // opens something. `copy_source` still opens whatever name it is handed, and
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
        // `r` names its file through `copy_source`, a third such site.
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

/// Two sites the raw-name landing touched, held to td-txt's OWN bytes rather
/// than to GNU's: each diverges from GNU somewhere the corpus cannot compare,
/// so GNU is not the whole oracle here and the name and the shape are what is
/// under test. The ambiguity line's divergence is spec/README's; the scratch
/// file's NAME is nobody's contract, as the comment below it says. Without
/// these the hand-rolled possibilities join -- which replaced a `join(" ")`
/// that could not carry bytes -- is defended by nothing.
#[test]
fn a_diverging_diagnostic_still_names_in_raw_bytes() -> Result<(), Box<dyn std::error::Error>> {
    use std::os::unix::ffi::OsStrExt;

    let dir = TempDir::new("rawname-diverge")?;
    std::fs::write(dir.0.join("IN"), b"a\n")?;

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

/// A stdout that IS `/dev/null` takes `-L`'s exemption from the `-m0` short cut
/// away (grep.c:2894-2896): GNU clears `list_files` for it exactly as for `-q`,
/// since neither can show a file name, and only then tests whether the short cut
/// applies. The corpus cannot ask for this — it captures stdout through a pipe,
/// and the whole condition is what stdout IS — so the two runs differ only in
/// that. Measured against GNU grep 3.11 under `LC_ALL=C`.
#[test]
fn a_dev_null_stdout_takes_the_l_exemption_away() -> Result<(), Box<dyn std::error::Error>> {
    let run = |null: bool| -> Result<Vec<u8>, Box<dyn std::error::Error>> {
        let out = std::process::Command::new(bin())
            .args(["grep", "-L", "-m0", "-e", r"\d"])
            .stdin(std::process::Stdio::null())
            .stdout(match null {
                true => std::process::Stdio::null(),
                false => std::process::Stdio::piped(),
            })
            .stderr(std::process::Stdio::piped())
            .env("LC_ALL", "C")
            .output()?;
        Ok(out.stderr)
    };
    // Down a pipe `-L` still names a file, so the short cut does not apply and
    // the pattern is lexed and linted.
    assert_eq!(
        String::from_utf8_lossy(&run(false)?),
        "grep: warning: stray \\ before d\n",
        "-L -m0 down a pipe should lint",
    );
    // Into `/dev/null` it names nothing, so the short cut applies and nothing is
    // compiled at all.
    assert_eq!(String::from_utf8_lossy(&run(true)?), "", "-L -m0 into /dev/null should be silent");
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

/// `R /dev/stdin` answers from a stream that has not ended, which is the half of
/// the shared reader the corpus cannot ask for: every case there writes stdin whole
/// and closes it, so a source read whole passes them all. The write end stays OPEN
/// across the assertion — a run that swallowed standard input first would still be
/// blocked in that read when the deadline passed, which is what it did before the
/// one reader existed.
#[test]
fn r_upper_stdin_answers_before_the_stream_ends() -> Result<(), Box<dyn std::error::Error>> {
    use std::io::{Read, Write};
    let dir = TempDir::new("r-stdin-stream")?;
    std::fs::write(dir.0.join("IN"), b"a\nb\n")?;
    let mut child = std::process::Command::new(bin())
        .arg("sed")
        .args(["-n", "-e", "R /dev/stdin", "-e", "p", "IN"])
        .current_dir(&dir.0)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    let mut sink = child.stdin.take().ok_or("no stdin pipe")?;
    let mut out = child.stdout.take().ok_or("no stdout pipe")?;
    sink.write_all(b"s1\ns2\n")?;
    sink.flush()?;

    // On a thread, because a read that never answers is exactly the failure: the
    // deadline is the parent's, and killing the child is what releases it.
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = [0u8; 10];
        let _ = tx.send(out.read_exact(&mut buf).map(|()| buf));
    });
    let got = rx.recv_timeout(std::time::Duration::from_secs(20));
    let _ = child.kill();
    let _ = child.wait();
    drop(sink);
    let buf = got.map_err(|_| "R /dev/stdin answered nothing while its stream was open")??;
    // Interleaved, one source line per cycle: the operand's line, then stdin's.
    // Exactly what the two cycles produce, so the read cannot wait on a byte that
    // only a closed pipe would bring.
    assert_eq!(&buf, b"a\ns1\nb\ns2\n");
    Ok(())
}

/// `r` dumps its source as it READS it, and the two halves of that are what this
/// asserts. Bytes come OUT of an ENDLESS source, which a reader that swallowed the
/// whole source first could never manage; and closing the pipe ENDS the run, which
/// is the only end an endless dump has — `Out` swallows the EPIPE and latches, so a
/// copy that never consulted the latch would write into a closed pipe for ever.
/// GNU dies of SIGPIPE there, which a program the Rust runtime leaves ignoring it
/// cannot; exit 0 is this crate's answer to a reader that left, as it is grep's,
/// so that is what is required rather than GNU's status. The corpus cannot ask for
/// any of this: every case there waits for an exit status, and this one has to
/// close a descriptor to get one.
#[test]
fn r_dumps_a_source_that_never_ends() -> Result<(), Box<dyn std::error::Error>> {
    use std::io::Read;
    let dir = TempDir::new("r-endless")?;
    std::fs::write(dir.0.join("IN"), b"a\n")?;
    // A SECOND queued `r` behind the endless one, and a third whose source cannot
    // be read at all: reached after the pipe closes, neither may read, hang or
    // report. A directory is the reachable read failure, exit 4 -- which nobody is
    // there to see, and which is the wrong answer to a reader that left.
    std::fs::create_dir(dir.0.join("DIR"))?;
    let mut child = std::process::Command::new(bin())
        .arg("sed")
        .args(["-n", "-e", "r /dev/zero", "-e", "r DIR", "-e", "r /dev/zero", "-e", "p", "IN"])
        .current_dir(&dir.0)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    let mut out = child.stdout.take().ok_or("no stdout pipe")?;
    // On a thread, because the read is what may never return: the thread's end is
    // what closes the pipe, and killing the child is what releases it if not.
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = [0u8; 16];
        let _ = tx.send(out.read_exact(&mut buf).map(|()| buf));
    });
    let got = rx.recv_timeout(std::time::Duration::from_secs(20));
    let buf = match got {
        Ok(buf) => buf,
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err("sed wrote nothing from an endless `r` source in 20s".into());
        }
    }?;
    // The line `p` printed first, then the source's own bytes.
    assert_eq!(buf.get(..2), Some(b"a\n".as_slice()));
    assert_eq!(buf.get(2..), Some([0u8; 14].as_slice()), "the dump is /dev/zero's bytes");

    let mut waited = 0;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if waited >= 20_000 {
            let _ = child.kill();
            let _ = child.wait();
            return Err("sed kept dumping an endless `r` source into a CLOSED pipe".into());
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
        waited += 20;
    };
    assert!(status.success(), "a closed reader is not a failure: {status:?}");
    Ok(())
}

/// A `w` target is buffered as GNU's is, which is observable only through an `r`
/// of the same file in the same run.
#[test]
fn a_w_target_flushes_on_the_buffer_boundary() -> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new("w-buffer")?;
    // Each row is GNU sed 4.9's own answer: how much of a `w` target an `r` of the
    // same file can see, which is the only way the SIZE of that buffer is visible
    // from outside. The corpus cannot hold these -- its files are written inline,
    // and the smallest interesting one is 4100 bytes.
    //
    // Every answer past the first two is a multiple of 4096 and never the amount
    // written, which is what says the buffer fills to CAPACITY before writing
    // rather than flushing early to make room. A `BufWriter` of the same capacity
    // was measured against these and answers 4000/4000/4000/8000/8000/16000 from
    // the third row on, so every one of those six rows is a discriminator.
    let rows: &[(usize, usize)] = &[
        (1000, 0),
        (4000, 0),
        (4100, 4096),
        (5000, 4096),
        (8000, 4096),
        (8300, 8192),
        (12000, 8192),
        (20000, 16384),
    ];
    for (total, want) in rows {
        // 100-byte lines, so the totals are exact.
        let mut input = Vec::new();
        for i in 0..(total / 100) {
            input.extend_from_slice(format!("L{i:03}").as_bytes());
            input.extend(std::iter::repeat_n(b'x', 95));
            input.push(b'\n');
        }
        assert_eq!(input.len(), *total, "the input is not the length the row says");
        std::fs::write(dir.0.join("h"), &input)?;
        let out = std::process::Command::new(bin())
            .arg("sed")
            .args(["-n", "-e", "w wf", "-e", "$r wf", "h"])
            .current_dir(&dir.0)
            .output()?;
        assert_eq!(out.status.code(), Some(0), "stderr: {}", String::from_utf8_lossy(&out.stderr));
        assert_eq!(
            out.stdout.len(),
            *want,
            "{total} bytes written to the w target: the r saw the wrong amount"
        );
        // What it saw must be the PREFIX of what was written, not merely its size.
        assert_eq!(out.stdout, input.get(..*want).unwrap_or_default(), "at {total}");
        // And the file itself is whole once the run ends.
        assert_eq!(std::fs::read(dir.0.join("wf"))?.len(), *total, "at {total}");
    }

    // An EXACT fill is the case the rows above cannot reach, every one of them
    // crossing the boundary mid-line. stdio OVERFLOWS rather than topping up: a
    // full buffer is not a reason to write, having more to put and nowhere to put
    // it is. So a `w` of one 4096-byte record leaves an `r` nothing, where
    // flushing at capacity would show it all 4096. Each row is one record of
    // `len` bytes including its separator, repeated `times`, and `want` is again
    // GNU sed 4.9's own answer for what the `r` sees across the whole run.
    let exact: &[(usize, usize, usize)] = &[
        (4096, 1, 0),
        (4095, 1, 0),
        (4097, 1, 4096),
        (4096, 2, 4096),
    ];
    for (len, times, want) in exact {
        let mut input = Vec::new();
        for _ in 0..*times {
            input.extend(std::iter::repeat_n(b'A', len.saturating_sub(1)));
            input.push(b'\n');
        }
        std::fs::write(dir.0.join("h"), &input)?;
        let out = std::process::Command::new(bin())
            .arg("sed")
            .args(["-n", "-e", "w wf", "-e", "r wf", "h"])
            .current_dir(&dir.0)
            .output()?;
        assert_eq!(out.status.code(), Some(0), "stderr: {}", String::from_utf8_lossy(&out.stderr));
        assert_eq!(out.stdout.len(), *want, "{times} record(s) of {len} bytes");
        assert_eq!(std::fs::read(dir.0.join("wf"))?, input, "{times} of {len}");
    }
    Ok(())
}

/// Under `--posix` the special-file table is not consulted, so `w /dev/stdout`
/// is an ORDINARY open of that path -- a SECOND stdio buffer over the same pipe.
/// Which of the two reaches it first is then settled by which FILLS first, and
/// the sink takes two records per input record (the `p` and the auto-print)
/// where the `w` target takes one. So the run opens with 4096 bytes of the
/// sink's stream, which is GNU sed 4.9's own answer; a sink buffered any other
/// way opens with the `w` target's block instead, which is what a `BufWriter`
/// over `std::io::stdout()` did. This is the only byte-observable consequence of
/// the SINK's buffer size -- the flushed TOTAL is `floor(n / 4096) * 4096` for
/// any scheme that fills before it writes, so nothing else here can see it.
#[test]
fn a_posix_w_of_dev_stdout_is_a_second_buffer_over_one_pipe()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new("posix-stdout-buffer")?;
    // 41 records of 100 bytes: enough that the sink crosses 4096 and the `w`
    // target, at half the volume, does not.
    let (mut input, mut doubled) = (Vec::new(), Vec::new());
    for i in 0..41 {
        let mut rec = format!("L{i:03}").into_bytes();
        rec.extend(std::iter::repeat_n(b'x', 95));
        rec.push(b'\n');
        input.extend_from_slice(&rec);
        doubled.extend_from_slice(&rec);
        doubled.extend_from_slice(&rec);
    }
    assert_eq!(input.len(), 4100, "the input is not the length this test needs");
    std::fs::write(dir.0.join("h"), &input)?;
    let out = std::process::Command::new(bin())
        .arg("sed")
        .args(["--posix", "-e", "p", "-e", "w /dev/stdout", "h"])
        .current_dir(&dir.0)
        .output()?;
    assert_eq!(out.status.code(), Some(0), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(out.stdout.len(), input.len() * 3, "the p, the auto-print and the w target");
    assert_eq!(
        out.stdout.get(..4096),
        doubled.get(..4096),
        "the run opens with the sink's block, not the w target's"
    );
    Ok(())
}

/// A CYCLE that keeps writing after the READER has gone away. GNU takes SIGPIPE
/// and stops there; the Rust runtime ignores that signal before `main`, so `Out`
/// takes EPIPE and LATCHES instead, and until the cycle asked, nothing stopped
/// it -- `sed -z -n p /dev/zero | head` never returned.
///
/// Tested over a FINITE input so THIS regression is a wrong NUMBER rather than a
/// hung gate -- the run that does not notice processes all million bytes, and the
/// `w` target is what records how far it got, since the reader that would say so
/// is exactly the one that went away. That only bounds the regressions it models,
/// though, so the read and the wait carry deadlines besides.
///
/// The bound is 250_000 against a physical ceiling of about 69_700 -- the pipe's
/// 65_536 plus the sink's 4096 plus a record -- and what the run actually leaves
/// depends on who is reading: 8200 through this harness, 69_700 through a shell
/// pipeline. GNU leaves 8192 most runs and 4096 in a few, the difference being
/// which flushes beat the signal; the rest goes with its buffers.
#[test]
fn a_cycle_stops_once_the_reader_has_gone() -> Result<(), Box<dyn std::error::Error>> {
    use std::io::{Read as _, Seek as _};
    let dir = TempDir::new("broken-sink-cycle")?;
    let mut input = Vec::new();
    let mut i = 0;
    while input.len() < 1_000_000 {
        input.extend_from_slice(format!("L{i:07}").as_bytes());
        input.extend(std::iter::repeat_n(b'x', 91));
        input.push(b'\n');
        i += 1;
    }
    std::fs::write(dir.0.join("big"), &input)?;
    // Two scripts, because the write that breaks the pipe can be in either of two
    // places and a different check catches each. `p` writes INSIDE the cycle, so
    // the cycle abandons the `w` after it and the record it was on is swallowed
    // without being recorded; the auto-print writes AFTER the cycle returned, so
    // the `w` has already run and the swallowed input matches the record exactly.
    // Miss the between-cycles check and the second reads one more record before
    // anything notices, which is the whole of the placement argument.
    for (script, extra) in [(vec!["-n", "-e", "p", "-e", "w OUT"], 100u64), (vec!["-e", "w OUT"], 0)]
    {
        let _ = std::fs::remove_file(dir.0.join("OUT"));
        // Through STDIN, and a `try_clone` of it kept here: the child shares that
        // file DESCRIPTION, so its final offset is readable from this side and says
        // exactly how much of the input the run swallowed.
        let mut mine = std::fs::File::open(dir.0.join("big"))?;
        let theirs = mine.try_clone()?;
        let mut child = std::process::Command::new(bin())
            .arg("sed")
            .args(&script)
            .current_dir(&dir.0)
            .stdin(std::process::Stdio::from(theirs))
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()?;
        close_the_reader(&mut child)?;
        // Drained BEFORE the wait: stderr is a pipe, so a regression that filled
        // it would block the child in a write while this blocked in `wait` -- the
        // hung gate this test is shaped to avoid, arriving by the other door.
        let mut err = Vec::new();
        if let Some(mut e) = child.stderr.take() {
            e.read_to_end(&mut err)?;
        }
        let status = wait_bounded(&mut child, 20)?.ok_or("the run never ended")?;
        assert_eq!(status.code(), Some(0), "{script:?}: {}", String::from_utf8_lossy(&err));
        assert!(err.is_empty(), "{script:?}: a closed reader is not a diagnostic: {err:?}");
        let wrote = std::fs::metadata(dir.0.join("OUT"))?.len();
        assert!(wrote < input.len() as u64, "{script:?}: did not stop at all: wrote {wrote}");
        assert!(
            wrote <= 250_000,
            "{script:?}: kept going long past the closed reader: wrote {wrote} of {}",
            input.len()
        );
        // And it swallowed exactly the records it processed, plus the one it was
        // in the middle of where the write that broke the pipe was the cycle's
        // own. GNU does that too, dying at that same write. Never TWO records,
        // which is what asking before the read rather than after it buys.
        let consumed = mine.stream_position()?;
        assert_eq!(
            consumed,
            wrote + extra,
            "{script:?}: the run swallowed input beyond the record the write failed on"
        );
        let mut rest = Vec::new();
        mine.read_to_end(&mut rest)?;
        assert_eq!(
            rest.first(),
            Some(&b'L'),
            "{script:?}: the next reader was left in the middle of a record"
        );
        assert_eq!(
            rest.len() as u64,
            input.len() as u64 - consumed,
            "{script:?}: the rest is the rest"
        );
    }
    Ok(())
}

/// Read a few bytes from a child's stdout and then CLOSE that end, which is what
/// makes its next write fail. On a thread with a deadline, as the neighbouring
/// stream tests do it, because a read that never answers is one of the failures
/// these are for.
fn close_the_reader(child: &mut std::process::Child) -> Result<(), Box<dyn std::error::Error>> {
    use std::io::Read as _;
    let mut out = child.stdout.take().ok_or("the child has no stdout")?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut head = [0u8; 8];
        let read = out.read_exact(&mut head).is_ok();
        // The pipe closes HERE, which is the whole point of the thread.
        drop(out);
        let _ = tx.send(read);
    });
    match rx.recv_timeout(std::time::Duration::from_secs(20)) {
        Ok(true) => Ok(()),
        Ok(false) => Err("the run ended before it wrote anything to read".into()),
        Err(_) => Err("the run never wrote anything".into()),
    }
}

/// Wait for a child, but never forever: a hang is the failure being tested for
/// here, and a test that waits for one is a gate that never finishes. `None` is
/// the deadline passing, and the child is killed on the way out.
fn wait_bounded(
    child: &mut std::process::Child,
    secs: u64,
) -> Result<Option<std::process::ExitStatus>, Box<dyn std::error::Error>> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    loop {
        if let Some(done) = child.try_wait()? {
            return Ok(Some(done));
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(None);
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// A script that LOOPS without ending its cycle -- `b` back to a label -- never
/// reaches the between-cycles check, so the closed reader has to be noticed
/// inside the cycle too. `sed -n -e ':a' -e p -e 'b a' file | head -c 8` ran
/// forever with only the outer check, which is what a cross-model review found.
///
/// Unbounded by construction (the loop never advances the input), so this one
/// cannot be made finite the way the between-cycles test is; it gets a watchdog
/// instead, and a regression is a killed child rather than a hung gate.
#[test]
fn a_script_loop_stops_once_the_reader_has_gone() -> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new("broken-sink-loop")?;
    std::fs::write(dir.0.join("one"), b"x\n")?;
    for script in [
        // `b` back to a label: the input never advances at all.
        ":a;p;ba",
        // `t`, which loops on the substitution having taken.
        ":a;s/x/xy/;p;ta",
        // And `D`, which restarts the script WITHOUT reading, so it re-enters
        // the cycle by the third of the three doors.
        "p;s/^/z\\n/;D",
    ] {
        let mut child = std::process::Command::new(bin())
            .arg("sed")
            .args(["-n", script, "one"])
            .current_dir(&dir.0)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()?;
        close_the_reader(&mut child)?;
        let done = wait_bounded(&mut child, 20)?;
        let code = done.ok_or_else(|| format!("`{script}` never noticed the closed reader"))?;
        assert_eq!(code.code(), Some(0), "`{script}`");
    }
    Ok(())
}

/// `-s` opens its operands one at a time, so a run that stops because the reader
/// has gone must say so in a way the operand loop can tell from an ordinary end
/// of file -- otherwise it opens the NEXT one. It reported a missing operand
/// (exit 2, with a diagnostic) where GNU, dead of SIGPIPE, says nothing at all;
/// a FIFO nobody opens for writing hung outright. The second is left out of the
/// test for the reason it is a bug: it does not terminate when it regresses.
#[test]
fn a_separate_run_stops_at_the_operand_the_reader_broke_on()
-> Result<(), Box<dyn std::error::Error>> {
    use std::io::Read as _;
    let dir = TempDir::new("broken-sink-separate")?;
    let mut input = Vec::new();
    for i in 0..200_000u32 {
        input.extend_from_slice(format!("L{i:06}\n").as_bytes());
    }
    std::fs::write(dir.0.join("big"), &input)?;
    // Both places the stop can be decided, since each returns to the operand loop
    // by its own route: `p` alone breaks between cycles, and `p` followed by
    // another command breaks inside one.
    for script in [vec!["-s", "-n", "p"], vec!["-s", "-n", "-e", "p", "-e", "w OUT"]] {
        let mut child = std::process::Command::new(bin())
            .arg("sed")
            .args(&script)
            .args(["big", "definitely-not-here"])
            .current_dir(&dir.0)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()?;
        close_the_reader(&mut child)?;
        let mut err = Vec::new();
        if let Some(mut e) = child.stderr.take() {
            e.read_to_end(&mut err)?;
        }
        let done = wait_bounded(&mut child, 20)?.ok_or("the run never ended")?;
        assert!(
            err.is_empty(),
            "{script:?}: the operand after the broken one was opened: {}",
            String::from_utf8_lossy(&err)
        );
        assert_eq!(done.code(), Some(0), "{script:?}: a closed reader is not an error");
    }
    Ok(())
}

/// `l` submits its listing in pieces because GNU's `do_list` writes it a BYTE at
/// a time, and the piece must be one UNDER the buffer rather than equal to it: a
/// `put` of exactly the capacity finds no room on the first write of a run --
/// stdio's buffer not existing until its first overflow -- and so goes straight
/// to the descriptor, ahead of anything else sharing it. Both cross-model reviews
/// of that landing found the equal-to case independently, and nothing in this
/// crate would have.
///
/// Visible only where a second stream shares the descriptor, so stdout and stderr
/// are handed ONE file description here (a `try_clone` dup, which shares the
/// offset as `2>&1` does; two opens of a path would not). The numbers are GNU sed
/// 4.9's. The 4094 row is the piece size: it moves to 4094 when the piece is the
/// whole capacity. The 8190 row is the submission itself: it moves to 8190 when
/// the listing goes over whole, which is what the build before it did.
#[test]
fn a_listing_is_submitted_in_pieces_the_buffer_can_hold() -> Result<(), Box<dyn std::error::Error>>
{
    let dir = TempDir::new("l-pieces")?;
    for (record, total, marker) in [(4094usize, 8191usize, 8189usize), (8190, 16383, 16381)] {
        let mut input = vec![b'0'; record];
        input.push(b'\n');
        std::fs::write(dir.0.join("L"), &input)?;
        let file = std::fs::File::create(dir.0.join("both"))?;
        let shared = file.try_clone()?;
        let status = std::process::Command::new(bin())
            .arg("sed")
            .args(["-n", "-e", "l 0", "-e", "w /dev/stderr", "L"])
            .current_dir(&dir.0)
            .stdout(std::process::Stdio::from(file))
            .stderr(std::process::Stdio::from(shared))
            .status()?;
        assert_eq!(status.code(), Some(0), "record of {record}");
        let body = std::fs::read(dir.0.join("both"))?;
        assert_eq!(body.len(), total, "record of {record}");
        assert_eq!(
            body.iter().position(|b| *b == b'$'),
            Some(marker),
            "record of {record}: the listing reached the descriptor at the wrong point"
        );
    }
    Ok(())
}

/// An `r` source bigger than one read block, for the half the endless test cannot
/// reach: that the copy LOOPS. A `copy_source` writing its first block and
/// returning satisfies every other test in this crate while truncating every `r`
/// source over 4 KiB, since the corpus writes its files inline and none of them is
/// that big. The bytes are compared whole rather than counted, so a block dropped
/// or repeated in the middle fails too.
#[test]
fn an_r_source_crosses_read_blocks() -> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new("r-blocks-dump")?;
    // Two blocks and a bit, and every line distinct, so a repeat is not a match.
    let source: Vec<u8> = (0..1200u32).flat_map(|i| format!("s{i:06}\n").into_bytes()).collect();
    assert!(source.len() > 2 * 4096, "the source has to span blocks");
    std::fs::write(dir.0.join("SRC"), &source)?;
    std::fs::write(dir.0.join("IN"), b"a\nb\n")?;

    let out = std::process::Command::new(bin())
        .arg("sed")
        .args(["-n", "-e", "r SRC", "-e", "p", "IN"])
        .current_dir(&dir.0)
        .output()?;
    assert_eq!(out.status.code(), Some(0), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    // `r` dumps the WHOLE source once per cycle, after that cycle's own line.
    let mut want = Vec::new();
    for line in [b"a\n".as_slice(), b"b\n".as_slice()] {
        want.extend_from_slice(line);
        want.extend_from_slice(&source);
    }
    assert_eq!(out.stdout.len(), want.len(), "the dump is not the source's length");
    assert_eq!(out.stdout, want);
    Ok(())
}

/// An `R` source bigger than one read block, which the corpus cannot hold: its
/// files are written inline, and this one has to span several 4 KiB reads and put
/// a RECORD across a boundary. Both halves are pinned — that the lines arrive in
/// order and whole, and that `-s` rewinds the reader rather than only a cursor,
/// which a reader that kept its buffer would fail by handing back records from
/// before the seek.
#[test]
fn an_r_source_crosses_read_blocks_and_still_rewinds()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new("r-blocks")?;
    // Line 900 is 9 KiB long, so it spans blocks however they fall.
    let mut source = Vec::new();
    for i in 0..1200u32 {
        if i == 900 {
            source.extend(std::iter::repeat_n(b'L', 9000));
            source.push(b'\n');
            continue;
        }
        source.extend_from_slice(format!("{i:07}\n").as_bytes());
    }
    std::fs::write(dir.0.join("SRC"), &source)?;
    let lines: Vec<&[u8]> = source.split(|b| *b == b'\n').collect();

    // One operand of 1000 lines: `R` hands over one source line per cycle, so the
    // 1000th comes from well past the first block and past the long record.
    let operand: Vec<u8> = (0..1000u32).flat_map(|i| format!("o{i}\n").into_bytes()).collect();
    std::fs::write(dir.0.join("IN"), &operand)?;

    let out = std::process::Command::new(bin())
        .arg("sed")
        .args(["-n", "-e", "R SRC", "-e", "p", "IN"])
        .current_dir(&dir.0)
        .output()?;
    assert_eq!(out.status.code(), Some(0), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let got: Vec<&[u8]> = out.stdout.split(|b| *b == b'\n').collect();
    // Interleaved: operand line, source line, operand line, source line ...
    for i in 0..1000usize {
        assert_eq!(got.get(i * 2), Some(&format!("o{i}").into_bytes().as_slice()));
        assert_eq!(got.get(i * 2 + 1), lines.get(i), "source line {i} came back wrong");
    }

    // `-s` restarts a seekable source per operand, so the second file gets line 0
    // again. Two one-line operands make that the whole of the output.
    std::fs::write(dir.0.join("P"), b"p\n")?;
    std::fs::write(dir.0.join("Q"), b"q\n")?;
    let out = std::process::Command::new(bin())
        .arg("sed")
        .args(["-s", "-n", "-e", "R SRC", "-e", "p", "P", "Q"])
        .current_dir(&dir.0)
        .output()?;
    assert_eq!(out.stdout, b"p\n0000000\nq\n0000000\n".to_vec());
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

/// A run that DIES repositions standard input too, and exactly ONCE. Each way out
/// of the `-s`/`-i` loop is its own exit -- a fatal read, a refused in-place
/// operand -- and the reader is the RUN's now, so every one of them owes the
/// position back. Once, because the count is computed from the reader and a seek
/// does not change it: a second call moves the descriptor again by the same
/// amount, which is a stray `can't reposition stdin` when the result is negative
/// and a silent over-rewind when it is not.
///
/// Both shapes need a REGULAR file on descriptor 0 and a second handle onto the
/// same description, which is why they are here and not in the corpus; the `-i`
/// one also checks the file it DID edit, since the run is meant to die after it.
#[test]
fn a_dying_run_repositions_standard_input_once() -> Result<(), Box<dyn std::error::Error>> {
    use std::io::Read;
    let dir = TempDir::new("sed-stdin-dies")?;
    std::fs::write(dir.0.join("four"), b"l1\nl2\nl3\nl4\n")?;
    std::fs::write(dir.0.join("one"), b"ONE\n")?;
    // Opening a directory succeeds and the first READ of it fails, which is how a
    // run is made to die having already taken a line off descriptor 0.
    std::fs::create_dir_all(dir.0.join("D"))?;

    let rows: &[(&[&str], &[u8], &[u8])] = &[
        // A fatal read under `-s`: `R` took `l1`, so `l2`..`l4` are owed back.
        (
            &["-s", "-n", "-e", "R /dev/stdin", "-e", "p", "one", "D"],
            b"sed: read error on D: Is a directory\n",
            b"l2\nl3\nl4\n",
        ),
        // A refused `-i` operand, which gives up on the whole run from a third
        // place: the first operand was edited and the second cannot be.
        (
            &["-i", "-e", "R /dev/stdin", "A", "D"],
            b"sed: couldn't edit D: not a regular file\n",
            b"l2\nl3\nl4\n",
        ),
    ];
    for (args, want_err, want_rest) in rows {
        std::fs::write(dir.0.join("A"), b"a\n")?;
        let given = std::fs::File::open(dir.0.join("four"))?;
        let mut mine = given.try_clone()?;
        let out = std::process::Command::new(bin())
            .arg("sed")
            .args(*args)
            .current_dir(&dir.0)
            .stdin(std::process::Stdio::from(given))
            .output()?;
        let mut rest = Vec::new();
        mine.read_to_end(&mut rest)?;
        // EXACTLY the one diagnostic: a second give-back would add its own.
        assert_eq!(out.stderr, want_err.to_vec(), "{args:?}: stderr");
        assert_eq!(out.status.code(), Some(4), "{args:?}");
        assert_eq!(rest, want_rest.to_vec(), "{args:?}: what was left on descriptor 0");
    }
    // The operand the `-i` run DID edit keeps what `R` gave it, which is what says
    // the run got that far.
    assert_eq!(std::fs::read(dir.0.join("A"))?, b"a\nl1\n".to_vec());
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
        // `R /dev/stdin` reads the SAME descriptor, so what it takes is delivered
        // as much as a cycle's record is -- it was printed -- and handing it back
        // would repeat it to whoever reads next. Before the one shared reader
        // these left NOTHING: `R` swallowed standard input to end of file.
        ("four", &["-n", "-e", "R /dev/stdin", "-e", "p", "one"], b"ONE\nl1\n", b"l2\nl3\nl4\n"),
        ("four", &["-n", "-e", "R /dev/stdin", "-e", "R /dev/stdin", "-e", "p", "one"], b"ONE\nl1\nl2\n", b"l3\nl4\n"),
        // A `q` before the source is spent owes back the rest, as it does for a
        // record the cycle read.
        ("four", &["-n", "-e", "R /dev/stdin", "-e", "p", "-e", "q", "one"], b"ONE\nl1\n", b"l2\nl3\nl4\n"),
        // Both roles on one descriptor: the `-` operand goes on from where `R`
        // left it, so between them they reach the end and nothing is owed.
        ("four", &["-n", "-e", "R /dev/stdin", "-e", "p", "one", "-"], b"ONE\nl1\nl2\nl3\nl4\n", b""),
        // `-s` makes a stream per operand while the reader is the RUN's, which is
        // why the give-back is the run's too: repositioning between operands would
        // rewind a descriptor whose buffered records the next operand still reads.
        ("four", &["-s", "-n", "-e", "R /dev/stdin", "-e", "p", "one", "one"], b"ONE\nl1\nONE\nl2\n", b"l3\nl4\n"),
        // `-u` reads a RECORD at a time instead of a block, so there is nothing
        // to give back -- and the descriptor must still come out in the same
        // place, since the count is the reader's rather than the block's.
        ("four", &["-u", "-n", "1q"], b"", b"l2\nl3\nl4\n"),
        ("four", &["-u", "-n", "-e", "N;q"], b"", b"l3\nl4\n"),
        ("unterm", &["-u", "-n", "2q"], b"", b"u3"),
        ("four", &["-u", "-s", "-n", "1q"], b"", b"l2\nl3\nl4\n"),
        ("four", &["-u", "-n", "-e", "R /dev/stdin", "-e", "p", "one"], b"ONE\nl1\n", b"l2\nl3\nl4\n"),
        // The two rows that are NOT GNU's answer, deliberately. The `$' lookahead
        // is a pushback, and glibc does not return a pushed-back byte to the file
        // offset on an unbuffered stream, so GNU leaves each of these ONE BYTE
        // further on and its next reader loses that byte. Matching it would mean
        // mis-positioning a shared descriptor on purpose. Both shapes are here
        // because they lose a byte from different places: the first from the
        // record the lookahead opened `-' to peek at, the second from the one the
        // `q' was about to leave behind.
        // GNU offset 1, this 0 -- GNU's next reader gets `1\nl2\nl3\nl4\n'.
        ("four", &["-u", "-n", "-e", "1{$!p}", "-e", "1q", "one", "-"], b"ONE\n", b"l1\nl2\nl3\nl4\n"),
        // GNU offset 4, this 3 -- GNU's next reader gets `2\nl3\nl4\n'. Unflagged
        // both leave 3, which is the row below it.
        ("four", &["-u", "-n", "-e", "1{$!p}", "-e", "1q"], b"l1\n", b"l2\nl3\nl4\n"),
        ("four", &["-n", "-e", "1{$!p}", "-e", "1q"], b"l1\n", b"l2\nl3\nl4\n"),
        // The controls. A script that reads to the end leaves nothing, which is
        // what says the rewind is not unconditional; and no `q` at all is the
        // same.
        ("four", &["-n", "$q"], b"", b""),
        ("four", &["-n", "p"], b"l1\nl2\nl3\nl4\n", b""),
        ("four", &["-u", "-n", "p"], b"l1\nl2\nl3\nl4\n", b""),
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

/// A run of NULs longer than the read buffer is DROPPED rather than turned into
/// one empty record per byte, and the records it stood for still occupy line
/// numbers. `-z` is what makes that visible: there is no binary verdict under
/// it, so nothing suppresses the output the numbers are printed on. The corpus
/// cannot carry this -- the run has to outlast a 96 KiB buffer to be dropped at
/// all, which is not a `file-json` literal anybody should read.
#[test]
fn a_skipped_run_of_nuls_still_counts_its_lines() -> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new("skip-nuls")?;
    let run = 300 * 1024;
    let mut data = vec![0u8; run];
    data.extend_from_slice(b"hello\x00world\x00");
    std::fs::write(dir.0.join("z"), &data)?;
    // Goldens measured from GNU grep 3.11 under LC_ALL=C.
    for (args, want) in [
        (vec!["grep", "-z", "-n", "hello", "z"], format!("{}:hello\0", run + 1)),
        (vec!["grep", "-z", "-n", "world", "z"], format!("{}:world\0", run + 2)),
        (vec!["grep", "-z", "-c", "hello", "z"], "1\n".to_string()),
        (
            vec!["grep", "-z", "-n", "-A", "1", "hello", "z"],
            format!("{}:hello\0{}-world\0", run + 1, run + 2),
        ),
        // And the three where an empty record IS selected, so nothing may be
        // dropped at all: the count is the proof it was not.
        (vec!["grep", "-z", "-c", "^$", "z"], format!("{run}\n")),
        (vec!["grep", "-z", "-c", "^", "z"], format!("{}\n", run + 2)),
        (vec!["grep", "-z", "-v", "-c", "hello", "z"], format!("{}\n", run + 1)),
    ] {
        let out = std::process::Command::new(bin())
            .args(&args)
            .current_dir(&dir.0)
            .env("LC_ALL", "C")
            .output()?;
        assert_eq!(out.status.code(), Some(0), "{args:?}: {out:?}");
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            want,
            "{args:?}: a dropped run did not keep its line numbers"
        );
    }

    // And with `-a` and no `-z` the verdict never trips, so the NULs are
    // ORDINARY BYTES of one long line rather than separators -- nothing may be
    // dropped, and the anchor is what says so: `^hello` must not match a line
    // that opens with a quarter-megabyte of NULs. Dropping those reads would
    // leave the line `hello`. Only that combination: under `-z -a` skipping IS
    // enabled, since GNU gates it on the separator (`skip_empty_lines && !eol`)
    // and not on the verdict at all there.
    // A WHOLE number of read buffers, so every NUL lands in a fill that could
    // be dropped and `hello` opens a fresh one. At a length that is not a
    // multiple, the last fill carries NULs of its own and the anchor fails
    // whether or not the earlier fills were wrongly dropped -- which is how
    // the first version of this test passed against the bug it was for.
    let flat_run = 3 * 96 * 1024;
    let mut flat = vec![0u8; flat_run];
    flat.extend_from_slice(b"hello\n");
    std::fs::write(dir.0.join("a"), &flat)?;
    for (args, want) in [
        (vec!["grep", "-a", "-c", "^hello", "a"], "0\n"),
        (vec!["grep", "-a", "-c", "hello", "a"], "1\n"),
        (vec!["grep", "-a", "-c", "^", "a"], "1\n"),
    ] {
        let out = std::process::Command::new(bin())
            .args(&args)
            .current_dir(&dir.0)
            .env("LC_ALL", "C")
            .output()?;
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            want,
            "{args:?}: a run of NULs was dropped where it is not a separator"
        );
    }
    Ok(())
}

/// A dropped run of NULs JOINS an open record to whatever follows it, because
/// the separators that would have ended the record were in the read that went
/// away. That is GNU's behaviour and it is observable: the joined record is
/// selected by a pattern spanning the gap, and neither half stands as a record
/// of its own. Found by review, after a first version of this landing declined
/// to skip while a record was open -- which read as the safer choice and was a
/// divergence. Goldens measured from GNU grep 3.11 under LC_ALL=C.
#[test]
fn a_dropped_run_joins_an_open_record_as_gnu_does() -> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new("skip-join")?;
    // The carry must fill a whole read buffer so the NUL run starts exactly at
    // the next one -- otherwise the run shares a fill with the carry, no fill is
    // all zeros, and nothing is dropped at all.
    let buf = 96 * 1024;
    let mut data = b"foo".to_vec();
    data.extend(std::iter::repeat_n(b'x', buf - 3));
    data.extend(std::iter::repeat_n(0u8, 2 * buf));
    data.extend_from_slice(b"bar\x00");
    std::fs::write(dir.0.join("j"), &data)?;
    for (args, want, code) in [
        // The join exists...
        (vec!["grep", "-z", "-c", "^foo.*bar$", "j"], "1\n", 0),
        (vec!["grep", "-z", "-c", "^foo", "j"], "1\n", 0),
        // ...and the far half is not a record on its own.
        (vec!["grep", "-z", "-c", "^bar$", "j"], "0\n", 1),
    ] {
        let out = std::process::Command::new(bin())
            .args(&args)
            .current_dir(&dir.0)
            .env("LC_ALL", "C")
            .output()?;
        assert_eq!(String::from_utf8_lossy(&out.stdout), want, "{args:?}");
        assert_eq!(out.status.code(), Some(code), "{args:?}");
    }
    Ok(())
}

/// `-B` context across a dropped run. The dropped records are empty and
/// consecutive, so the window has to be told about them -- otherwise it offers
/// a record from BEFORE the run as this one's neighbour, carrying the line
/// number it had then. Where the run was JOINED into an open record instead
/// there are no such records, and replaying them would be context GNU does not
/// print. Both halves are here. Goldens from GNU grep 3.11.
#[test]
fn context_across_a_dropped_run_is_numbered_as_gnu_numbers_it()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new("skip-ctx")?;
    let buf = 96 * 1024;
    // Nothing open when the whole-zero fills arrive: they stand as empty records.
    let mut standalone = b"a\x00".to_vec();
    standalone.extend(std::iter::repeat_n(0u8, 2 * buf));
    standalone.extend_from_slice(b"hello\x00");
    std::fs::write(dir.0.join("s"), &standalone)?;
    // A record IS open when they arrive: the gap joins into it and leaves none.
    let mut joined = vec![b'y'; buf];
    joined.extend(std::iter::repeat_n(0u8, 2 * buf));
    joined.extend_from_slice(b"hello\x00");
    std::fs::write(dir.0.join("j"), &joined)?;
    let n = 2 * buf as u64;
    for (args, want) in [
        (
            vec!["grep", "-z", "-n", "-B", "1", "hello", "s"],
            format!("{}-\0{}:hello\0", n + 1, n + 2),
        ),
        (
            vec!["grep", "-z", "-n", "-B", "3", "hello", "s"],
            format!("{}-\0{}-\0{}-\0{}:hello\0", n - 1, n, n + 1, n + 2),
        ),
        // Joined: one record, numbered past the gap, and nothing before it.
        (vec!["grep", "-z", "-n", "-B", "1", "hello", "j"], String::new()),
    ] {
        let out = std::process::Command::new(bin())
            .args(&args)
            .current_dir(&dir.0)
            .env("LC_ALL", "C")
            .output()?;
        let got = String::from_utf8_lossy(&out.stdout).to_string();
        if want.is_empty() {
            assert!(
                got.starts_with(&format!("{}:", n + 1)),
                "{args:?}: a joined gap was given context: {:?}",
                got.get(..60)
            );
            assert_eq!(got.matches('\0').count(), 1, "{args:?}: more than one record out");
        } else {
            assert_eq!(got, want, "{args:?}");
        }
    }
    Ok(())
}

/// `-D`'s ARGUMENT is not `-d`'s, which is the part of this pair no reading of
/// either would predict. GNU parses `-d` with argmatch and `-D` with `STREQ`
/// (grep.c:2529), so a prefix that resolves for one is an error for the other --
/// and the diagnostic differs with it, naming no option and quoting no value
/// where `-d`'s does both, and exiting 2 where `-d` exits 1. Three divergences
/// between adjacent letters. Goldens from GNU grep 3.11 under LC_ALL=C.
#[test]
fn the_devices_argument_is_exact_where_the_directories_one_is_a_prefix()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new("dev-arg")?;
    std::fs::write(dir.join("f"), b"hello\n")?;
    for (args, code, err) in [
        (vec!["grep", "-D", "read", "hello", "f"], 0, ""),
        (vec!["grep", "-D", "skip", "hello", "f"], 0, ""),
        (vec!["grep", "-D", "rea", "hello", "f"], 2, "grep: unknown devices method\n"),
        (vec!["grep", "-D", "r", "hello", "f"], 2, "grep: unknown devices method\n"),
        (vec!["grep", "-D", "ski", "hello", "f"], 2, "grep: unknown devices method\n"),
        (vec!["grep", "-D", "", "hello", "f"], 2, "grep: unknown devices method\n"),
        (vec!["grep", "-D", "READ", "hello", "f"], 2, "grep: unknown devices method\n"),
        (vec!["grep", "--devices=nope", "hello", "f"], 2, "grep: unknown devices method\n"),
        (vec!["grep", "--devices=skip", "hello", "f"], 0, ""),
        (vec!["grep", "--devices", "skip", "hello", "f"], 0, ""),
    ] {
        let out = std::process::Command::new(bin())
            .args(&args)
            .current_dir(&dir.0)
            .env("LC_ALL", "C")
            .output()?;
        assert_eq!(String::from_utf8_lossy(&out.stderr), err, "{args:?}");
        assert_eq!(out.status.code(), Some(code), "{args:?}");
    }
    // `-d`'s side of the contrast. Its FIRST LINE is what is compared: the rest
    // is argmatch's list of valid arguments and the usage block, both already
    // pinned by the corpus, and neither is what distinguishes the two options.
    for (args, code, first) in [
        (vec!["grep", "-d", "rec", "hello", "f"], 0, ""),
        (
            vec!["grep", "-d", "r", "hello", "f"],
            1,
            "grep: ambiguous argument 'r' for '--directories'",
        ),
        (
            vec!["grep", "-d", "nope", "hello", "f"],
            1,
            "grep: invalid argument 'nope' for '--directories'",
        ),
    ] {
        let out = std::process::Command::new(bin())
            .args(&args)
            .current_dir(&dir.0)
            .env("LC_ALL", "C")
            .output()?;
        let err = String::from_utf8_lossy(&out.stderr).to_string();
        assert_eq!(err.lines().next().unwrap_or(""), first, "{args:?}");
        assert_eq!(out.status.code(), Some(code), "{args:?}");
    }
    Ok(())
}

/// The device DEFAULT is neither spelling `-D` accepts: a device NAMED as an
/// operand is read, and the same device found by the WALK is skipped without a
/// word. Goldens from GNU grep 3.11.
#[test]
fn a_device_is_read_when_named_and_skipped_when_found()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new("dev-default")?;
    std::fs::create_dir(dir.join("d"))?;
    std::fs::write(dir.join("d").join("plain"), b"hello\n")?;
    let _sock = std::os::unix::net::UnixListener::bind(dir.join("d").join("s"))?;
    for (args, code, want) in [
        // Found by the walk: skipped by the default and by -D skip alike, and
        // SILENTLY -- no diagnostic, no effect on the status.
        (vec!["grep", "-r", "hello", "d"], 0, "d/plain:hello\n"),
        (vec!["grep", "-r", "-D", "skip", "hello", "d"], 0, "d/plain:hello\n"),
        // A device named as an operand is read unless asked otherwise, and
        // /dev/null reads as empty rather than as a missing file.
        (vec!["grep", "hello", "/dev/null"], 1, ""),
        (vec!["grep", "-D", "skip", "hello", "/dev/null"], 1, ""),
        // Skipping it does not disturb a real operand beside it.
        (
            vec!["grep", "-D", "skip", "-H", "hello", "/dev/null", "d/plain"],
            0,
            "d/plain:hello\n",
        ),
        // A DIRECTORY is not a device: -D skip leaves -d's answer alone, and the
        // two skips compose rather than one standing in for the other.
        (vec!["grep", "-D", "skip", "-d", "skip", "hello", "d"], 1, ""),
    ] {
        let out = std::process::Command::new(bin())
            .args(&args)
            .current_dir(&dir.0)
            .env("LC_ALL", "C")
            .output()?;
        assert_eq!(String::from_utf8_lossy(&out.stdout), want, "{args:?}");
        assert_eq!(String::from_utf8_lossy(&out.stderr), "", "{args:?}");
        assert_eq!(out.status.code(), Some(code), "{args:?}");
    }
    Ok(())
}

/// WHERE the device question is asked differs between the two paths, and a
/// socket is what makes that observable. A name the WALK produced is decided
/// from a stat BEFORE the open -- GNU's `grepdirent` skips without opening,
/// "since opening might have side effects on a device" -- while an OPERAND is
/// opened first and decided from the descriptor. Opening a socket path fails
/// `ENXIO`, so the split is directly visible: the same socket is silent when
/// found and loud when named, and `-D skip` does NOT quiet the named one,
/// because the open fails before any policy is consulted. Deciding the walk case
/// after the open instead turns the first row into a diagnostic GNU never
/// prints. Goldens from GNU grep 3.11.
#[test]
fn where_the_device_question_is_asked_differs_between_walk_and_operand()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new("dev-where")?;
    std::fs::create_dir(dir.join("t"))?;
    std::fs::write(dir.join("t").join("f"), b"a\n")?;
    let _sock = std::os::unix::net::UnixListener::bind(dir.join("t").join("s"))?;
    let enxio = "grep: t/s: No such device or address\n";
    for (args, code, out_want, err_want) in [
        // Found by the walk and skipped: never opened, so never reported.
        (vec!["grep", "-rl", "a", "t"], 0, "t/f\n", ""),
        (vec!["grep", "-r", "-D", "skip", "-l", "a", "t"], 0, "t/f\n", ""),
        // Found by the walk and READ: now it is opened, and the open fails.
        (vec!["grep", "-r", "-D", "read", "-l", "a", "t"], 2, "t/f\n", enxio),
        // Named as an operand: opened under every policy, so always reported.
        (vec!["grep", "-l", "a", "t/s"], 2, "", enxio),
        (vec!["grep", "-D", "skip", "-l", "a", "t/s"], 2, "", enxio),
        (vec!["grep", "-D", "read", "-l", "a", "t/s"], 2, "", enxio),
    ] {
        let out = std::process::Command::new(bin())
            .args(&args)
            .current_dir(&dir.0)
            .env("LC_ALL", "C")
            .output()?;
        assert_eq!(String::from_utf8_lossy(&out.stdout), out_want, "{args:?}");
        assert_eq!(String::from_utf8_lossy(&out.stderr), err_want, "{args:?}");
        assert_eq!(out.status.code(), Some(code), "{args:?}");
    }
    Ok(())
}

/// `-R` flips the device DEFAULT (grep.c:3007), which is the one consequence of
/// `-R` that is not about symlinks at all: `-R PAT dir` opens a socket that
/// `-r PAT dir` passes by, and the ENXIO that follows is how it shows. An
/// explicit `-D skip` still wins, whichever order the two came in. Goldens from
/// GNU grep 3.11.
#[test]
fn dereference_recursive_flips_the_device_default()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new("dev-flip")?;
    std::fs::create_dir(dir.join("t"))?;
    std::fs::write(dir.join("t").join("f"), b"a\n")?;
    let _sock = std::os::unix::net::UnixListener::bind(dir.join("t").join("s"))?;
    let enxio = "grep: t/s: No such device or address\n";
    for (args, code, err_want) in [
        (vec!["grep", "-rl", "a", "t"], 0, ""),
        (vec!["grep", "-Rl", "a", "t"], 2, enxio),
        (vec!["grep", "-Rl", "-D", "skip", "a", "t"], 0, ""),
        (vec!["grep", "-D", "skip", "-Rl", "a", "t"], 0, ""),
        (vec!["grep", "-Rl", "-D", "read", "a", "t"], 2, enxio),
    ] {
        let out = std::process::Command::new(bin())
            .args(&args)
            .current_dir(&dir.0)
            .env("LC_ALL", "C")
            .output()?;
        assert_eq!(String::from_utf8_lossy(&out.stdout), "t/f\n", "{args:?}");
        assert_eq!(String::from_utf8_lossy(&out.stderr), err_want, "{args:?}");
        assert_eq!(out.status.code(), Some(code), "{args:?}");
    }
    Ok(())
}


/// A command-line device whose READ and SKIP are TOLD APART, which `/dev/null`
/// cannot do: it reads as empty, so both answers are exit 1 with no output, and
/// a drift axis that made the default skip argv-named devices came back green
/// against it. `/dev/zero` under `-z -a -m1 '^'` matches one NUL-terminated
/// record instead, so reading exits 0 and skipping exits 1.
///
/// The `-r` rows are the walk ROOT, which GNU counts as a command-line name
/// however far the walk below it goes (`fts_level == FTS_ROOTLEVEL`). Marking
/// every walk result as found-not-named made `grep -rzam1 '^' /dev/zero` exit 1
/// where GNU exits 0 -- the same misreading would have let `-r -D skip` pass
/// silently over a named socket instead of reporting its failed open. Goldens
/// from GNU grep 3.11.
#[test]
fn a_named_device_is_read_and_the_walks_root_is_still_a_named_one()
-> Result<(), Box<dyn std::error::Error>> {
    for (args, code) in [
        (vec!["grep", "-zam1", "^", "/dev/zero"], 0),
        (vec!["grep", "-zam1", "-D", "read", "^", "/dev/zero"], 0),
        (vec!["grep", "-zam1", "-D", "skip", "^", "/dev/zero"], 1),
        // The walk's root is an operand, so these answer as the rows above do.
        (vec!["grep", "-rzam1", "^", "/dev/zero"], 0),
        (vec!["grep", "-rzam1", "-D", "read", "^", "/dev/zero"], 0),
        (vec!["grep", "-rzam1", "-D", "skip", "^", "/dev/zero"], 1),
        (vec!["grep", "-Rzam1", "^", "/dev/zero"], 0),
    ] {
        let out = std::process::Command::new(bin())
            .args(&args)
            .env("LC_ALL", "C")
            .output()?;
        assert_eq!(String::from_utf8_lossy(&out.stderr), "", "{args:?}");
        assert_eq!(out.status.code(), Some(code), "{args:?}");
    }
    Ok(())
}

/// The walk's ROOT again, on the side where being wrong is loud rather than
/// quiet: a socket NAMED as the root of a `-r` walk is opened, because an
/// operand is, and the failed open is reported even under `-D skip`. The same
/// socket one level down is silent. One file, two provenances, opposite answers.
/// Goldens from GNU grep 3.11.
#[test]
fn a_socket_named_as_the_walk_root_is_opened_where_one_below_it_is_not()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new("dev-root")?;
    std::fs::create_dir(dir.join("t"))?;
    std::fs::write(dir.join("t").join("f"), b"a\n")?;
    let _sock = std::os::unix::net::UnixListener::bind(dir.join("t").join("s"))?;
    let enxio = "grep: t/s: No such device or address\n";
    for (args, code, out_want, err_want) in [
        (vec!["grep", "-rl", "a", "t/s"], 2, "", enxio),
        (vec!["grep", "-r", "-D", "skip", "-l", "a", "t/s"], 2, "", enxio),
        (vec!["grep", "-rl", "a", "t"], 0, "t/f\n", ""),
    ] {
        let out = std::process::Command::new(bin())
            .args(&args)
            .current_dir(&dir.0)
            .env("LC_ALL", "C")
            .output()?;
        assert_eq!(String::from_utf8_lossy(&out.stdout), out_want, "{args:?}");
        assert_eq!(String::from_utf8_lossy(&out.stderr), err_want, "{args:?}");
        assert_eq!(out.status.code(), Some(code), "{args:?}");
    }
    Ok(())
}


/// A DIRECTORY is not a device, so `-D` must not quietly take over what `-d`
/// answers. The two compose: `-D skip` alone leaves a directory operand to `-d`'s
/// default, which READS it and fails with `Is a directory`, and `-c` still prints
/// its zero on the way. Widening `is_device` to cover directories turns all of
/// that into a silent exit 1 — and no other test here notices, drift having found
/// that axis green until this one existed. Goldens from GNU grep 3.11.
#[test]
fn a_directory_is_not_a_device() -> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new("dev-dir")?;
    std::fs::create_dir(dir.join("d"))?;
    std::fs::write(dir.join("d").join("plain"), b"hello\n")?;
    let isdir = "grep: d: Is a directory\n";
    for (args, code, out_want, err_want) in [
        (vec!["grep", "-D", "skip", "hello", "d"], 2, "", isdir),
        (vec!["grep", "-D", "read", "hello", "d"], 2, "", isdir),
        (vec!["grep", "hello", "d"], 2, "", isdir),
        (vec!["grep", "-D", "skip", "-c", "hello", "d"], 2, "0\n", isdir),
        // `-d` is what answers for a directory, and it still does.
        (vec!["grep", "-D", "skip", "-d", "skip", "hello", "d"], 1, "", ""),
        (vec!["grep", "-D", "skip", "-d", "recurse", "hello", "d"], 0, "d/plain:hello\n", ""),
    ] {
        let out = std::process::Command::new(bin())
            .args(&args)
            .current_dir(&dir.0)
            .env("LC_ALL", "C")
            .output()?;
        assert_eq!(String::from_utf8_lossy(&out.stdout), out_want, "{args:?}");
        assert_eq!(String::from_utf8_lossy(&out.stderr), err_want, "{args:?}");
        assert_eq!(out.status.code(), Some(code), "{args:?}");
    }
    Ok(())
}


/// `-b` across a run of NULs the reader SKIPPED, where td-txt deliberately does
/// not print what GNU prints. GNU credits `totalnl` for a dropped read
/// (grep.c:1024) and never credits `totalcc`, which advances only by what a
/// buffer held (grep.c:1627) — so `-b`, printing `totalcc + (beg - bufbeg)`
/// (grep.c:1196), reports the bytes GNU SCANNED rather than the position in the
/// file. That is `run mod 98304`, one `INITIAL_BUFSIZE` (grep.c:886), and it
/// MOVES with the input: 1696 at 100000, 32768 at 131072, 0 at 196608. The 1, 4
/// and 16 MiB runs all give 65536 because that family is congruent mod 96 KiB.
///
/// So the number is not meaningless — it is a real quantity, just not the one
/// `-b` names. What settles it is that GNU CONTRADICTS ITSELF: `-n` counts the
/// discarded lines where `-b` declines to count the discarded bytes, on the same
/// record, so `grep -z -b -n` prints `1048577:65536:hello` — one line, two
/// fields, two different accounts of where the record is. td-txt prints
/// `1048577:1048576:hello`, and the assertion below is that the two fields
/// agree rather than that the offset takes any particular value.
///
/// This is the same trade the streaming entry makes elsewhere: reproduce GNU's
/// RULE, decline arithmetic that contradicts the rule's own neighbour.
#[test]
fn a_byte_offset_past_a_skipped_run_counts_the_bytes_that_were_there()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new("b-skip")?;
    // A whole number of 96 KiB read buffers, so every NUL lands in a fill that
    // can be dropped and `hello` opens a fresh one.
    let run = 3 * 96 * 1024;
    let mut data = vec![0u8; run];
    data.extend_from_slice(b"hello\x00");
    std::fs::write(dir.join("z"), &data)?;
    let want_at = run.to_string();
    let want_no = (run + 1).to_string();
    for (args, want) in [
        (vec!["grep", "-z", "-b", "hello", "z"], format!("{want_at}:hello\0")),
        (vec!["grep", "-z", "-n", "hello", "z"], format!("{want_no}:hello\0")),
        // Both together: the two fields describe the same record, which is the
        // property GNU loses here.
        (
            vec!["grep", "-z", "-b", "-n", "hello", "z"],
            format!("{want_no}:{want_at}:hello\0"),
        ),
    ] {
        let out = std::process::Command::new(bin())
            .args(&args)
            .current_dir(&dir.0)
            .env("LC_ALL", "C")
            .output()?;
        assert_eq!(String::from_utf8_lossy(&out.stdout), want, "{args:?}");
        assert_eq!(out.status.code(), Some(0), "{args:?}");
    }
    // The `-B` replay carries offsets too, and they are the run's own tail
    // rather than the match's. A record BEFORE the run is what makes GNU agree
    // about the context at all, so this shape isolates the offsets: GNU prints
    // these three lines with these NUMBERS, under offsets 98303..=98306 that
    // step by one from a start the discarded reads never moved.
    let mut lead = b"a\x00".to_vec();
    lead.extend(std::iter::repeat_n(0u8, 2 * 96 * 1024));
    lead.extend_from_slice(b"hello\x00");
    std::fs::write(dir.join("s"), &lead)?;
    let n = 2 * 96 * 1024;
    let out = std::process::Command::new(bin())
        .args(["grep", "-z", "-n", "-b", "-B", "3", "hello", "s"])
        .current_dir(&dir.0)
        .env("LC_ALL", "C")
        .output()?;
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        format!(
            "{}-{}-\0{}-{}-\0{}-{}-\0{}:{}:hello\0",
            n - 1,
            n - 1,
            n,
            n,
            n + 1,
            n + 1,
            n + 2,
            n + 2
        ),
    );
    Ok(())
}

/// `grep -r` sorts each directory's entries and descends at the point a
/// directory is reached -- fts(3)'s own traversal with a name comparator, where
/// GNU passes NONE (`fts_open (fts_arg, opts, NULL)`, grep.c:1868) and so takes
/// whatever order the kernel returns. GNU's listing is therefore a property of
/// the filesystem rather than of grep: two checkouts of one tree can print the
/// same matches in a different order, so there is no golden to match here, for
/// GNU either.
///
/// Per-directory and NOT by whole path, which this tree is shaped to tell
/// apart: `-` and `.` sort below `/`, so a path sort puts `mid-z` and `mid.a`
/// before `mid/aaa` where this one takes `mid`'s children first. Asserting
/// mere sortedness would pass under either and pin neither.
///
/// `divergence.test.txt` carries the same decision as a corpus case, on the
/// three-path minimum that separates those two orders. What that minimum
/// cannot tell apart -- it holds one directory, which sorts first among the
/// root's entries either way -- is a walk that took every DIRECTORY before any
/// file. This tree can: three directories and five files at the root, with
/// `alpha` sorting between `adir` and `mid`, so descent-where-met and
/// directories-first disagree about where it goes.
#[test]
fn a_recursive_walk_sorts_each_directory_where_gnu_takes_the_kernels_order()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new("walk-order")?;
    let root = dir.join("root");
    std::fs::create_dir_all(root.join("mid"))?;
    std::fs::create_dir_all(root.join("adir"))?;
    std::fs::create_dir_all(root.join("zdir"))?;
    // Created in an order that is neither sorted nor the expected one.
    for rel in [
        "zeta", "alpha", "mid/beta", "mid/aaa", "adir/x", "zdir/y", "mmm",
        "mid-z", "mid.a",
    ] {
        std::fs::write(root.join(rel), b"hit\n")?;
    }
    let out = std::process::Command::new(bin())
        .args(["grep", "-r", "-l", "hit", "root"])
        .current_dir(&dir.0)
        .env("LC_ALL", "C")
        .output()?;
    let got: Vec<String> =
        String::from_utf8_lossy(&out.stdout).lines().map(str::to_string).collect();
    let want = [
        "root/adir/x",
        "root/alpha",
        "root/mid/aaa",
        "root/mid/beta",
        "root/mid-z",
        "root/mid.a",
        "root/mmm",
        "root/zdir/y",
        "root/zeta",
    ];
    // Status first: a crash after partial output would otherwise be reported as
    // a wrong ORDER, which is a long way from what happened. With stderr, since
    // that is where the reason for a non-zero status is and `got` is stdout.
    assert_eq!(
        out.status.code(),
        Some(0),
        "grep -r failed: stdout={got:?} stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    // `want` is written out rather than derived from `got`: a sortedness check
    // on the output alone passes for any sorted list, a dropped file and a
    // doubled one included.
    assert_eq!(got, want, "the walk did not sort each directory in turn");

    // The two guards below are about the FIXTURE, not the walk: `want` reds for
    // any reordering, so what needs saying is that this tree still separates
    // the orders it was built to separate. Simplify it and both go quiet while
    // the test keeps passing against a new `want`.
    let mut by_path = got.clone();
    by_path.sort();
    assert_ne!(got, by_path, "this tree no longer tells the two sorts apart");
    // A root FILE emitted before a root DIRECTORY's contents is the whole of
    // what this tree adds over the corpus case, which has one directory and so
    // cannot distinguish a directories-first walk from this one. Both paths are
    // resolved rather than compared as `Option`s: a missing one is the fixture
    // change this guard exists to catch, and `None < Some` would call it a pass.
    let pos = |p: &str| {
        got.iter().position(|g| g == p).ok_or_else(|| format!("{p} left the fixture"))
    };
    assert!(
        pos("root/alpha")? < pos("root/mid/aaa")?,
        "this tree no longer tells descent-where-met from directories-first"
    );
    Ok(())
}

/// A record JOINED across a dropped run keeps the offset it began at, not one
/// past the gap. The two halves of `take_skipped` split here exactly as they do
/// for `-B` context: a run with nothing open starts the next record past
/// itself, and a run that a carry spans belongs to the record already going.
/// Without that split the joined record would report the offset of its TAIL,
/// which is a position it does not occupy.
#[test]
fn a_joined_record_keeps_the_offset_it_began_at()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new("b-join")?;
    let buf = 96 * 1024;
    // The carry fills a whole buffer, so the run starts exactly at the next one.
    let mut data = b"foo".to_vec();
    data.extend(std::iter::repeat_n(b'x', buf - 3));
    data.extend(std::iter::repeat_n(0u8, 2 * buf));
    data.extend_from_slice(b"bar\x00");
    std::fs::write(dir.join("j"), &data)?;
    let out = std::process::Command::new(bin())
        .args(["grep", "-z", "-b", "^foo.*bar$", "j"])
        .current_dir(&dir.0)
        .env("LC_ALL", "C")
        .output()?;
    let got = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(
        got.starts_with("0:foo"),
        "the joined record reported an offset past the gap it began before: {:?}",
        got.get(..24)
    );
    // Under `-o` the span is indexed in the ASSEMBLED record, so `bar` reports
    // 98304 and not the 294912 it physically sits at: the record is not
    // contiguous, and a span inside one has no single position in the input.
    // GNU prints 98304 here too -- the divergence above is about where a record
    // STARTS and reaches no further than that.
    let out = std::process::Command::new(bin())
        .args(["grep", "-z", "-b", "-o", "bar", "j"])
        .current_dir(&dir.0)
        .env("LC_ALL", "C")
        .output()?;
    assert_eq!(String::from_utf8_lossy(&out.stdout), "98304:bar\0");
    Ok(())
}

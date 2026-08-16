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
//!     listed `skip` and not run: it hangs/times out, depends on the Oils repo
//!     tree the sandbox cwd does not stage, or turns on shared `/tmp` state (each
//!     of which would fail for an environment reason or degenerate into a false
//!     pass). The overlay header enumerates these categories.
//!
//! The `xfail` COUNT is not a backlog, and a large share of it is not about
//! this shell at all: a case dies the same way on an external program the
//! harness withholds as on a builtin td-sh does not have -- both are `not
//! found`. The overlay header carries the standing note; measure before
//! mining the list, because the two causes are indistinguishable from the
//! failure alone. That measurement HAS now been taken (688 of 1249 name a
//! missing command), and `src/bin/spec_helpers.rs` serves eight of those
//! externals -- `cat`, `mkdir`, `touch`, `rm`, `wc`, `sleep`, `seq`, `chmod`
//! -- alongside the Oils `.py`
//! helpers, which is why those cases grade. `grep`/`sed`/`od` are withheld
//! still: they need a pattern engine this helper cannot borrow.
//!
//! One thing that note does not say: `[[` is NOT out-of-model --
//! busybox ash provides it under `ASH_BASH_COMPAT`, which td's `defconfig`
//! build enables, so those are real parity gaps rather than a bash-only feature
//! to write off. Promote an entry when a gap actually closes -- `to-promote`
//! reds the gate to make sure it is.
//!
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
//! a later PR). Importing the remaining Oils files, and serving more of the
//! externals the corpus reaches for, is follow-up work.

// A crate root of its own: `main.rs`'s lint reaches nothing here. `forbid`
// rather than `deny` because no scoped allow belongs in this one, and
// `forbid` is the spelling a later `#[allow(unsafe_code)]` cannot override.
#![forbid(unsafe_code)]

use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};

use td_sh::{
    graded_identity, parse_spec, resolve, run_case, run_dir_classified, summarize,
    CaseWorkdir, Disposition, Expectations, ASH_DASH_CHAIN,
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
            // An unrecognisable `compare_shells` token does not fail, it silently
            // stops bounding the identity chain for the whole file. The two
            // spellings a human would plausibly write are the `/`-separated one
            // (which is how the shell list two lines below is written) and a
            // capitalised name; both are rejected here so they red rather than
            // disappear. Versioned names (`bash-4.4`, `zsh-5.9`) are real.
            for token in case.compare_shells() {
                let ok = token.starts_with(|c: char| c.is_ascii_lowercase())
                    && token
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '.');
                assert!(
                    ok,
                    "{}: `## compare_shells:` token {token:?} is not a plain shell identity — \
                     it would silently stop bounding the golden chain for this file",
                    path.display(),
                );
            }
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

/// Whether `code` NAMES the staged bin directory, which is one of the two ways
/// a case can reach the build artifact the entries link to. Three spellings — a
/// path under `$PATH`, a `cd` that makes it the cwd, and a command LOOKUP, which
/// prints the staged path without the case ever naming `$PATH`.
///
/// This is NOT the whole guard, and believing it was is what left a hole: the
/// staged directory is a SIBLING of the case's cwd, so `..` names it too, and
/// `: > ../bin/cat` passes every rule here. What covers that is `redirects_to`
/// below, over every staged name rather than only the two Oils ones.
///
/// Quotes are stripped before any of it. `: > "$PATH"/cat` names the directory
/// exactly as the bare spelling does, and matching only the bare one let
/// ordinary shell quoting walk straight through.
fn reaches_into_path(code: &str) -> bool {
    let bare: String = code.chars().filter(|c| !matches!(c, '"' | '\'')).collect();
    ["$PATH/", "${PATH}/"].iter().any(|p| bare.contains(p))
        || bare.lines().any(|l| {
            let l = l.trim_start();
            l.starts_with("cd ") && l.contains("PATH")
        })
        || looks_up_a_staged_applet(&bare)
}

/// Verbs that WRITE through a name, shared with the bare-name check below.
const MUTATORS: [&str; 7] = ["rm ", "mv ", "cp ", "ln ", "chmod ", "truncate ", "tee "];

/// A command lookup naming a staged applet, in a position that WRITES through
/// the answer. Under this harness `command -v cat` prints the entry in the
/// staged directory, so `: > $(command -v cat)` reaches the build artifact
/// without the case spelling `$PATH` at all.
///
/// Unlike the `$PATH` arm, this one has to ask what the answer is FOR: the
/// corpus contains a bare `command -v cat` (builtin-meta, `command -v doesn't
/// find executable dir`) that only prints it, and flagging that would red the
/// import over a case touching nothing.
fn looks_up_a_staged_applet(code: &str) -> bool {
    code.lines().any(|line| {
        let Some(at) = staged_lookup_at(line) else {
            return false;
        };
        let before = line.get(..at).unwrap_or_default();
        before.contains('>') || MUTATORS.iter().any(|verb| before.trim_start().starts_with(verb))
    })
}

/// Where in `line` a lookup of a staged applet begins, if there is one.
fn staged_lookup_at(line: &str) -> Option<usize> {
    for verb in ["command -v ", "type -p ", "which "] {
        for (at, _) in line.match_indices(verb) {
            let after = line.get(at + verb.len()..).unwrap_or_default().trim_start();
            let named = td_sh::SPEC_HELPERS.iter().any(|applet| {
                // A boundary, so `which category` is not `which cat`.
                after.strip_prefix(applet).is_some_and(|tail| {
                    !tail.starts_with(|c: char| c.is_alphanumeric() || c == '_' || c == '-')
                })
            });
            if named {
                return Some(at);
            }
        }
    }
    None
}

/// The narrowing above is only sound if it still fires. Reading `$PATH` is not
/// reaching into it — one corpus case does exactly that — and the two ways in
/// are refused.
#[test]
fn reaching_into_the_staged_path_is_still_caught() {
    for benign in [
        "echo $PATH",
        "PATH=/x:$PATH; echo hi",
        // The real case that made the blanket check wrong.
        "PATH=\"$PATH:.\"\na[",
        "cd /tmp",
    ] {
        assert!(!reaches_into_path(benign), "false positive on {benign:?}");
    }
    for hostile in [
        ": > $PATH/cat",
        "rm ${PATH}/argv.py",
        "cd $PATH\n: > cat",
        "cd \"$PATH\"",
        "  cd $PATH && echo x",
        // Ordinary quoting, which the first form of this guard let through.
        ": > \"$PATH\"/cat",
        ": > \"$PATH/cat\"",
        ": > '$PATH'/cat",
        ": > \"${PATH}\"/cat",
        // A lookup, which names the directory without spelling `$PATH`.
        ": > $(command -v cat)",
        ": > `which touch`",
        ": > $(type -p argv.py)",
    ] {
        assert!(reaches_into_path(hostile), "missed {hostile:?}");
    }
}

/// The lookup arm is the one that could over-match: every staged name is an
/// ordinary word, and `which` in a case about something else must not red the
/// import.
#[test]
fn a_lookup_is_only_a_reach_when_it_names_a_staged_applet() {
    for benign in [
        // The corpus's own uses: neither names a staged applet.
        "echo $(which $SH)",
        "which python2",
        // A longer word that merely STARTS with one.
        "command -v category",
        "which removed-thing",
        // Naming an applet without looking it up is just running it.
        "cat file",
        "type",
        // The real corpus case: it PRINTS the path and writes nothing.
        "PATH=\"_tmp:$PATH\"\ncommand -v cat\necho status=$?",
    ] {
        assert!(!reaches_into_path(benign), "false positive on {benign:?}");
    }
    // …but the same lookup feeding a write is the whole point of the arm.
    for hostile in ["rm $(command -v cat)", "tee `which touch` < x"] {
        assert!(reaches_into_path(hostile), "missed {hostile:?}");
    }
}

/// The SIBLING route, which `reaches_into_path` cannot see and which the first
/// form of this guard left open for every name but the two Oils ones. Each of
/// these was demonstrated to take the staged artifact to 0 bytes through a
/// symlink of the real shape.
#[test]
fn a_redirect_onto_a_staged_name_is_caught_however_it_is_spelled() {
    for hostile in [
        ": > ../bin/cat",
        ": > $TMP/../bin/cat",
        ": > \"../bin/cat\"",
        ": >| ../bin/cat",
        ": >> ../bin/argv.py",
        ": > $PATH/cat",
        ": > ${PATH%%:*}/cat",
        // After a `cd` into the staged directory the target is a BARE name.
        "cd $PATH\n: > cat",
        "p=$PATH\n: > $p/cat",
    ] {
        let hit = td_sh::SPEC_HELPERS.iter().any(|a| redirects_to(hostile, a));
        assert!(hit, "missed {hostile:?}");
    }
    for benign in [
        // A longer word that merely ENDS with a staged name is not that name.
        ": > catalog",
        ": > mycat",
        ": > format",
        // Ordinary redirects the corpus is full of.
        "echo x > out",
        "cat f > $TMP/g",
        "echo hi >&2",
    ] {
        let hit = td_sh::SPEC_HELPERS.iter().any(|a| redirects_to(benign, a));
        assert!(!hit, "false positive on {benign:?}");
    }
}

/// The dispatch itself: `argv[0]` picks the applet, and the STATUS and the
/// DIAGNOSTIC reach the caller.
///
/// Every applet is tested as a pure function, which cannot see any of that —
/// `exit(done.status)` replaced by `exit(0)`, and the stderr write dropped, both
/// left the whole gate green. That is the commit's central claim ("an external
/// is graded on its STATUS as much as its output") resting on nothing, so this
/// runs the built binary through a symlink the way `run_case` does.
#[test]
fn the_binary_reports_its_applets_status_and_diagnostic(
) -> Result<(), Box<dyn std::error::Error>> {
    // A Drop guard, not a tidy-up at the end: every step below can return early
    // on `?`, and a leaked directory of symlinks into `target/` outlives the run.
    struct Scratch(PathBuf);
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let path = std::env::temp_dir().join(format!("spec-helpers-dispatch-{}", std::process::id()));
    // Only a directory this test could have left is cleared. Anything ELSE at
    // that name is somebody else's, and `create_dir` then fails loudly —
    // deleting it would be a worse answer to a predictable path than refusing.
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir(&path)?;
    let scratch = Scratch(path);
    let dir = &scratch.0;
    let run = |applet: &str, args: &[&str]| -> Result<_, Box<dyn std::error::Error>> {
        let link = dir.join(applet);
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(spec_helpers_bin(), &link)?;
        Ok(std::process::Command::new(&link).args(args).output()?)
    };
    // A missing operand: coreutils' own 1, with a diagnostic.
    let out = run("cat", &["no-such-file-here"])?;
    assert_eq!(out.status.code(), Some(1), "cat of a missing file");
    assert!(!out.stderr.is_empty(), "no diagnostic reached stderr");
    // An option nobody implemented: 2, so it cannot read as a shell result.
    let out = run("touch", &["-d", "2017/12/31", "f"])?;
    assert_eq!(out.status.code(), Some(2), "an unsupported option");
    assert!(String::from_utf8_lossy(&out.stderr).contains("unsupported"));
    // A name the multicall does not serve at all.
    let out = run("nosuchapplet", &[])?;
    assert_eq!(out.status.code(), Some(2), "an unknown applet");
    // …and the success path still says 0 with nothing on stderr.
    let out = run("argv.py", &["a", "b"])?;
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(out.stdout, b"['a', 'b']\n");
    assert!(out.stderr.is_empty());
    // `wc -` reads STDIN rather than a file of that name, and still prints the
    // operand as GNU does. Here rather than in a unit test, which would inherit
    // whatever stdin `cargo test` was handed and block on a terminal.
    let link = dir.join("wc");
    std::os::unix::fs::symlink(spec_helpers_bin(), &link)?;
    let mut child = std::process::Command::new(&link)
        .args(["-l", "-"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    if let Some(mut w) = child.stdin.take() {
        std::io::Write::write_all(&mut w, b"a\nb\n")?;
    }
    let out = child.wait_with_output()?;
    assert_eq!(out.status.code(), Some(0), "wc - failed: {:?}", out.stderr);
    assert_eq!(out.stdout, b"2 -\n");
    // Every applet above is reached through its own dispatch arm. `sleep`'s is
    // pinned here rather than by its unit test, which calls the function
    // directly: an arm wired to the wrong applet — or to nothing — would leave
    // that test green while `sleep 0.05` in a case returned at once.
    let start = std::time::Instant::now();
    let out = run("sleep", &["0.05"])?;
    assert_eq!(out.status.code(), Some(0), "sleep: {:?}", out.stderr);
    assert!(out.stdout.is_empty() && out.stderr.is_empty());
    assert!(start.elapsed() >= std::time::Duration::from_millis(45), "sleep did not sleep");
    // The grep family is THREE dispatch arms onto one applet, differing only
    // in the preset each passes. Wired to the same preset — or to each other —
    // the unit tests would still pass, since they call `grep` directly with
    // the preset as an argument. `+` is what separates them: a repeat to
    // `egrep`, a literal to `grep`, and to `fgrep` so is `.`.
    // `fgrep` is asked with `a.b` rather than `a+b`, because `+` is a literal
    // to a BASIC expression too — so `a+b` cannot tell `-F` from no preset at
    // all, and the arm could be wired to either and still look right.
    for (name, pat, want) in [
        ("grep", "a+b", "a+b\n"),
        ("egrep", "ax+b", "axb\n"),
        ("fgrep", "a.b", ""),
    ] {
        let link = dir.join(name);
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(spec_helpers_bin(), &link)?;
        let mut child = std::process::Command::new(&link)
            .args(["-o", pat])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()?;
        if let Some(mut w) = child.stdin.take() {
            std::io::Write::write_all(&mut w, b"a+b\naxb\n")?;
        }
        let out = child.wait_with_output()?;
        let expect = match want.is_empty() {
            true => Some(1),
            false => Some(0),
        };
        assert_eq!(out.status.code(), expect, "{name}: {:?}", out.stderr);
        assert_eq!(String::from_utf8_lossy(&out.stdout), want, "{name} read `{pat}` wrongly");
    }
    // The byte-slicing four. `head` and `tail` answer the SAME question about
    // opposite ends of one stream, so a swapped pair is the mis-wiring their
    // unit tests — which call each function directly — cannot see; `-n 1` over
    // three lines tells them apart in one byte. `tac` is asked for a reversal
    // no prefix or suffix of the input equals, and `od` for a rendering that is
    // neither.
    for (name, args, want) in [
        ("head", &["-n", "1"][..], "1\n"),
        ("tail", &["-n", "1"][..], "3\n"),
        ("tac", &[][..], "3\n2\n1\n"),
        ("od", &["-A", "n", "-t", "x1"][..], " 31 0a 32 0a 33 0a\n"),
        // `sed` edits every line rather than selecting among them, so its
        // answer is one no other applet here can produce.
        ("sed", &["s/[0-9]/#/"][..], "#\n#\n#\n"),
    ] {
        let link = dir.join(name);
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(spec_helpers_bin(), &link)?;
        let mut child = std::process::Command::new(&link)
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()?;
        if let Some(mut w) = child.stdin.take() {
            std::io::Write::write_all(&mut w, b"1\n2\n3\n")?;
        }
        let out = child.wait_with_output()?;
        assert_eq!(out.status.code(), Some(0), "{name}: {:?}", out.stderr);
        assert_eq!(String::from_utf8_lossy(&out.stdout), want, "{name} answered wrongly");
    }
    // `head`'s multi-file banner needs OPERANDS, and it is the one output shape
    // in these four that the corpus goldens carry verbatim.
    std::fs::write(dir.join("hb1"), b"A\n")?;
    std::fs::write(dir.join("hb2"), b"B\n")?;
    let out = run(
        "head",
        &["--", &dir.join("hb1").to_string_lossy(), &dir.join("hb2").to_string_lossy()],
    )?;
    assert_eq!(out.status.code(), Some(0), "head of two files: {:?}", out.stderr);
    let banner = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(banner.contains("<==\nA\n\n==> "), "no blank line between files: {banner:?}");
    // …and `-` among them is bannered `standard input`, not `-`. Here rather
    // than in a unit test for `wc -`'s reason: calling the function directly
    // would inherit whatever stdin `cargo test` was handed and block on a
    // terminal.
    let link = dir.join("head");
    let mut child = std::process::Command::new(&link)
        .args(["--", "-", &dir.join("hb1").to_string_lossy()])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    if let Some(mut w) = child.stdin.take() {
        std::io::Write::write_all(&mut w, b"S\n")?;
    }
    let out = child.wait_with_output()?;
    assert_eq!(out.status.code(), Some(0), "head - : {:?}", out.stderr);
    let banner = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(banner.starts_with("==> standard input <==\nS\n\n==> "), "{banner:?}");
    // The other three applets whose dispatch arms nothing else here reaches.
    let out = run("seq", &["3"])?;
    assert_eq!((out.status.code(), out.stdout.as_slice()), (Some(0), b"1\n2\n3\n".as_slice()));
    let out = run("mkdir", &[&dir.join("d").to_string_lossy()])?;
    assert_eq!(out.status.code(), Some(0), "mkdir: {:?}", out.stderr);
    assert!(dir.join("d").is_dir());
    // `chmod` and `rm` are the two confined applets, and their root comes from
    // the ENVIRONMENT rather than an argument — which is the half a unit test
    // calling the function with an explicit root cannot reach. `$TMP` is set
    // here rather than inherited, so the workspace is a known directory and
    // the verdict does not turn on whatever `cargo test` was run with.
    let victim = dir.join("victim");
    std::fs::write(&victim, b"x")?;
    std::fs::create_dir_all(dir.join("case/tmp"))?;
    let confined = |applet: &str, args: &[&str]| -> Result<_, Box<dyn std::error::Error>> {
        let link = dir.join(applet);
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(spec_helpers_bin(), &link)?;
        Ok(std::process::Command::new(&link)
            .args(args)
            .env("TMP", dir.join("case/tmp"))
            .output()?)
    };
    for applet in ["chmod", "rm"] {
        let args: &[&str] = match applet {
            "chmod" => &["000"],
            _ => &["-rf"],
        };
        let mut argv = args.to_vec();
        let target = victim.to_string_lossy().into_owned();
        argv.push(&target);
        let out = confined(applet, &argv)?;
        assert_eq!(out.status.code(), Some(2), "{applet} outside a workdir was allowed");
        assert!(String::from_utf8_lossy(&out.stderr).contains("refusing"), "{applet} was quiet");
    }
    assert!(std::fs::read(&victim).is_ok(), "the file outside the workdir did not survive");
    // …and the same applet still serves a file INSIDE that workspace, so the
    // refusal above is the confinement and not the applet failing outright.
    let ok = dir.join("case/f");
    std::fs::write(&ok, b"x")?;
    let out = confined("chmod", &["600", &ok.to_string_lossy()])?;
    assert_eq!(out.status.code(), Some(0), "chmod inside the workdir: {:?}", out.stderr);
    Ok(())
}

/// The mutating-verb half, which keys on a PATH for the same reason.
#[test]
fn a_mutating_verb_is_only_caught_when_it_names_a_path() {
    for (line, applet) in [("cp x ../bin/cat", "cat"), ("mv $PATH/rm y", "rm")] {
        assert!(names_a_path_to(line, applet), "missed {line:?}");
    }
    for (line, applet) in [
        // The 41 ordinary uses this exists to let through.
        ("rm -f myfile", "rm"),
        ("rm dir/cmd", "rm"),
        ("touch a b", "touch"),
        // A path ending in a LONGER word, not the applet.
        ("cp x ../bin/catalog", "cat"),
    ] {
        assert!(!names_a_path_to(line, applet), "false positive on {line:?}");
    }
}

/// Whether any redirect in `code` TARGETS `name` — as a bare word (the cwd,
/// after a `cd` into the staged directory) or as any path ending in `/name`,
/// which is what `../bin/cat` is.
///
/// Quotes are stripped first, and `>|` is stepped over: without that its target
/// reads as the empty token and the clobber form walks through. `>>` needs
/// nothing, since the scan simply hits the second `>`.
fn redirects_to(code: &str, name: &str) -> bool {
    let bare: String = code.chars().filter(|c| !matches!(c, '"' | '\'')).collect();
    let mut rest = bare.as_str();
    while let Some(pos) = rest.find('>') {
        let after = rest.get(pos + 1..).unwrap_or_default().trim_start();
        let after = after.strip_prefix('|').unwrap_or(after).trim_start();
        let end = after
            .find(|c: char| c.is_whitespace() || matches!(c, ';' | '&' | '|' | ')'))
            .unwrap_or(after.len());
        let hit = after
            .get(..end)
            .and_then(|t| t.strip_suffix(name))
            .is_some_and(|lead| lead.is_empty() || lead.ends_with('/'));
        if hit {
            return true;
        }
        rest = rest.get(pos + 1..).unwrap_or_default();
    }
    false
}

/// Whether `line` names `applet` as a PATH — some word ending in `/applet`.
/// A BARE mention is an ordinary command (`rm f` is 41 corpus cases, which is
/// what made the verb scan below unusable over the externals); a slash before
/// the name is the only way one of these reaches the staged directory.
fn names_a_path_to(line: &str, applet: &str) -> bool {
    line.match_indices(applet).any(|(at, _)| {
        let before = line.get(..at).unwrap_or_default();
        let after = line.get(at + applet.len()..).unwrap_or_default();
        before.ends_with('/')
            && !after.starts_with(|c: char| c.is_alphanumeric() || matches!(c, '_' | '-' | '.'))
    })
}

/// Every staged applet is a SYMLINK to this crate's build artifact, so a case
/// that wrote through one would truncate the real binary — outside its own
/// throwaway workdir, and so for every later and every parallel case, which
/// would then grade against a helper that prints nothing. The shell entry is not
/// exposed the same way: it IS the executable the case is running, so the kernel
/// answers a write to it with ETXTBSY. The helpers get no such protection —
/// each is running only for the instant it takes to answer — which is why this
/// exists. Demonstrated, not assumed: through a symlink of the same shape,
/// `: > $PATH/argv.py` and `: > ../bin/cat` each take the target to 0 bytes.
///
/// No case in the pinned corpus writes to one, and this is what keeps that a
/// CHECKED property rather than a fact about today's import — a future corpus
/// file that does reds here, at the import, instead of silently corrupting a
/// regenerated overlay the gate would then enforce. A guard, not a fix: what
/// removes the exposure is staging a read-only copy, which costs the very
/// per-case `fs::copy` whose write fd the shell-staging note in `lib.rs` avoids.
#[test]
fn no_case_writes_to_the_staged_helper() -> Result<(), Box<dyn std::error::Error>> {
    let spec_dir = spec_dir();
    let mut checked = 0usize;
    for path in td_sh::spec_paths(&spec_dir)? {
        let text = std::fs::read_to_string(&path)?;
        for case in parse_spec(&text).map_err(|e| format!("{}: {e}", path.display()))? {
            let code = &case.code;
            // Naming the directory outright. A case that merely READS `$PATH`
            // (twenty do, mostly putting it on the right of a `:` to build a
            // longer one) is not touching anything.
            assert!(
                !reaches_into_path(code),
                "{} [case: {}]: reaches into the staged PATH directory, which holds \
                 a symlink to this crate's build artifact",
                path.display(),
                case.name,
            );
            // Every staged applet, not just the Oils two: they are links to ONE
            // binary, so truncating any of them breaks all of them. Both checks
            // below key on a PATH rather than a bare word, which is what lets
            // them cover the externals -- measured over the corpus, each fires
            // on zero cases, where a bare-name verb scan fires on 41.
            for applet in td_sh::SPEC_HELPERS {
                assert!(
                    !redirects_to(code, applet),
                    "{} [case: {}]: redirects onto `{applet}`, which would truncate \
                     the real `spec_helpers` binary for every other case",
                    path.display(),
                    case.name,
                );
                for verb in MUTATORS {
                    let mutates = code
                        .lines()
                        .any(|l| l.contains(verb) && names_a_path_to(l, applet));
                    assert!(
                        !mutates,
                        "{} [case: {}]: `{verb}` names a path to `{applet}`, which is \
                         a symlink to this crate's build artifact",
                        path.display(),
                        case.name,
                    );
                }
            }
            checked += 1;
        }
    }
    assert!(checked > 0, "scanned no cases");
    Ok(())
}

/// The runner's ENVIRONMENT contract, which 359 corpus guards turn on and which
/// only a real child can demonstrate. Written as a spec so each expectation is
/// the golden itself: `$SH` is the identity the case is GRADED as rather than a
/// path, it is still executable under that name, and PATH holds it and nothing
/// else — so a case reaching for an ordinary external still finds none. Pinned
/// here because the corpus overlay would catch a regression in any of these only
/// as a scattered count, which says nothing about which property broke.
#[test]
fn a_case_meets_the_shell_by_identity_on_a_path_of_its_own(
) -> Result<(), Box<dyn std::error::Error>> {
    let shell = PathBuf::from(env!("CARGO_BIN_EXE_td-sh"));
    let spec = "\
#### the identity guard fires
case $SH in ash) echo GUARD ;; *) echo MISS ;; esac
## STDOUT:
GUARD
## END

#### the identity is executable under that name
$SH -c 'echo nested'
## STDOUT:
nested
## END

#### PATH holds the shell and nothing else
command -v ash >/dev/null && echo found-shell
ls / >/dev/null 2>&1 || echo no-externals
## STDOUT:
found-shell
no-externals
## END
";
    for case in parse_spec(spec)? {
        let outcome = run_case(&shell, &spec_helpers_bin(), &case, ASH_DASH_CHAIN)?;
        assert!(outcome.passed, "{}: {}", case.name, outcome.detail.unwrap_or_default());
    }
    // And it follows the chain rather than being a fixed `ash`.
    let spec = "\
#### the chain head decides it when the case designates nothing
case $SH in dash) echo DASH ;; *) echo OTHER ;; esac
## STDOUT:
DASH
## END
";
    for case in parse_spec(spec)? {
        let outcome = run_case(&shell, &spec_helpers_bin(), &case, &["dash"])?;
        assert!(outcome.passed, "{}: {}", case.name, outcome.detail.unwrap_or_default());
    }

    // The identity follows the golden. A file that designates no ash grades
    // against dash's block, so the case must MEET dash — running as `ash` there
    // makes the guard take a branch the golden says was never reached. Written
    // as separate cases because the rule is a priority order: the header alone,
    // an annotation alone (a real corpus shape — a file whose header omits ash
    // still carrying an `ash` block, which `pick` reaches), and a file that
    // designates neither, where the chain head stands.
    let spec = "\
## compare_shells: bash dash mksh

#### the header decides when no annotation names a chain shell
case $SH in ash) echo ASH ;; dash) echo DASH ;; *) echo OTHER ;; esac
## STDOUT:
DASH
## END

#### a dash-graded case can still exec $SH
$SH -c 'echo nested'
## STDOUT:
nested
## END

#### an ash annotation outranks a header that omits ash
case $SH in ash) echo ASH ;; dash) echo DASH ;; *) echo OTHER ;; esac
## STDOUT:
DASH
## END
## OK ash STDOUT:
ASH
## END
";
    for case in parse_spec(spec)? {
        let outcome = run_case(&shell, &spec_helpers_bin(), &case, ASH_DASH_CHAIN)?;
        assert!(outcome.passed, "{}: {}", case.name, outcome.detail.unwrap_or_default());
    }
    let spec = "\
## compare_shells: bash mksh

#### a file that designates neither leaves the chain head
case $SH in ash) echo ASH ;; dash) echo DASH ;; *) echo OTHER ;; esac
## STDOUT:
ASH
## END
";
    for case in parse_spec(spec)? {
        let outcome = run_case(&shell, &spec_helpers_bin(), &case, ASH_DASH_CHAIN)?;
        assert!(outcome.passed, "{}: {}", case.name, outcome.detail.unwrap_or_default());
    }
    // And the identity a case is graded as is readable without running it, so an
    // analysis can tell which reference shell is the right one to grade a case
    // against — see the note on multicall references in the landing message.
    let cases = parse_spec(
        "## compare_shells: bash dash mksh\n\n#### x\ntrue\n## OK ash STDOUT:\n## END\n",
    )?;
    assert_eq!(
        cases.first().and_then(|c| graded_identity(c, ASH_DASH_CHAIN)),
        Some("ash")
    );
    // A block for a LATER chain element decides it when nothing earlier is
    // designated — the mirror of the case above, and the shape that would
    // otherwise grade a dash divergence while running as ash.
    let cases = parse_spec("#### x\ntrue\n## N-I dash status: 2\n")?;
    assert_eq!(
        cases.first().and_then(|c| graded_identity(c, ASH_DASH_CHAIN)),
        Some("dash")
    );
    // An annotation's shell name matches EXACTLY, because `pick` matches it
    // exactly; the `-version` tolerance belongs to `compare_shells` tokens
    // alone. Tolerating it here would designate a shell whose block `pick`
    // would then decline to use.
    let cases = parse_spec("#### x\ntrue\n## N-I dash-0.5.12 status: 2\n")?;
    assert_eq!(
        cases.first().and_then(|c| graded_identity(c, ASH_DASH_CHAIN)),
        Some("ash")
    );

    // The identity bounds EVERY field, not just the one that named it. This is
    // `var-sub-quote::single quotes work inside character classes` in miniature:
    // ash has its own stdout block, dash has a status block, and the header
    // omits ash. Grading stdout as ash while taking dash's status would be a
    // golden no shell ever produced — so the status here must be the default 0,
    // and reaching dash's `2` is the failure this pins.
    let spec = "\
## compare_shells: dash bash mksh

#### a later shell's block for another field does not reach us
echo \"$SH\"
## STDOUT:
dash
## END
## BUG ash STDOUT:
ash
## END
## N-I dash status: 2
";
    for case in parse_spec(spec)? {
        let outcome = run_case(&shell, &spec_helpers_bin(), &case, ASH_DASH_CHAIN)?;
        assert!(outcome.passed, "{}: {}", case.name, outcome.detail.unwrap_or_default());
    }

    // The PATH entry is a symlink to the binary under test, so `: > "$PATH/$SH"`
    // aims a truncation straight at the build artifact — and cannot land it,
    // because that binary is the one this very case is executing and the kernel
    // answers ETXTBSY. That is why the entry does not need to be a copy. The
    // assertion is on the artifact's SIZE rather than on the case's status: a
    // failed redirection on `:` is POSIX-fatal, so the case is expected to die.
    let before = std::fs::metadata(&shell)?.len();
    let spec = "\
#### a case cannot truncate the binary under test
: > \"$PATH/$SH\"
## status: 1
";
    for case in parse_spec(spec)? {
        let outcome = run_case(&shell, &spec_helpers_bin(), &case, ASH_DASH_CHAIN)?;
        assert!(outcome.passed, "{}: {}", case.name, outcome.detail.unwrap_or_default());
    }
    assert_eq!(
        std::fs::metadata(&shell)?.len(),
        before,
        "the shell under test was truncated through its own PATH entry"
    );

    // The identity becomes both a path component and the value of `$SH`, and the
    // chain is caller-supplied, so a path cannot be smuggled through it.
    let cases = parse_spec("#### identity is validated\ntrue\n")?;
    let case = cases.first().ok_or("missing case")?;
    for bad in ["../escape", "/abs", "a/b", "", ".", ".."] {
        assert!(
            run_case(&shell, &spec_helpers_bin(), case, &[bad]).is_err(),
            "identity {bad:?} was accepted as a path component"
        );
    }
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

    let (outcomes, stale) = run_dir_classified(&shell, &spec_helpers_bin(), &spec_dir, ASH_DASH_CHAIN, &exp)?;
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
    let outcome = run_case(&shell, &spec_helpers_bin(), case, ASH_DASH_CHAIN)?;
    assert!(outcome.passed, "large-output case failed: {:?}", outcome.detail);
    assert!(
        start.elapsed() < std::time::Duration::from_secs(5),
        "large-output case took {:?} — a drain deadlock hitting CASE_TIMEOUT",
        start.elapsed(),
    );
    Ok(())
}

/// The endless-output case, end to end, which is the hazard the cap exists for
/// and which only became reachable when `cat` was staged. It must come back
/// FAILED and say why, rather than growing the gate's heap for ten seconds and
/// then grading whatever fit.
///
/// It must also be FAST. Hitting the cap closes the read end, so the producer
/// dies of EPIPE instead of being waited out — a case that took the full
/// `CASE_TIMEOUT` would mean the pipe was left open and an orphan left spinning
/// against a reader that no longer wants it.
#[test]
fn an_endless_case_is_bounded_and_reported() -> Result<(), Box<dyn std::error::Error>> {
    let shell = PathBuf::from(env!("CARGO_BIN_EXE_td-sh"));
    let spec = "#### endless\ncat </dev/zero\n## status: 0\n";
    let cases = parse_spec(spec)?;
    let case = cases.first().ok_or("no case parsed")?;
    let start = std::time::Instant::now();
    let outcome = run_case(&shell, &spec_helpers_bin(), case, ASH_DASH_CHAIN)?;
    let elapsed = start.elapsed();
    assert!(!outcome.passed, "an endless case was graded as a pass");
    let detail = outcome.detail.unwrap_or_default();
    assert!(
        detail.contains("truncated"),
        "the truncation went unreported: {detail}"
    );
    assert!(
        elapsed < std::time::Duration::from_secs(5),
        "took {elapsed:?} — the producer was waited out, not closed on"
    );
    // STDERR is the other half of the decision, and it is a separate drain with
    // a separate cap: reporting only on stdout's would grade a case that wrote
    // megabytes to stderr on a prefix of them.
    let spec = "#### endless stderr\ncat </dev/zero >&2\n## status: 0\n";
    let cases = parse_spec(spec)?;
    let case = cases.first().ok_or("no case parsed")?;
    let outcome = run_case(&shell, &spec_helpers_bin(), case, ASH_DASH_CHAIN)?;
    assert!(!outcome.passed, "an endless stderr case was graded as a pass");
    assert!(outcome.truncated, "stderr's truncation was not reported");
    Ok(())
}

/// A case sees the shell IDENTITY as `$0`, not the entry's absolute path. The
/// corpus itself only reaches this through `spec-harness-bug.test.sh`'s `echo
/// $0 | grep -o sh`, which counts `sh` in whatever directory the gate was built
/// in -- so it is pinned here, where the answer cannot depend on that.
#[test]
fn a_case_sees_the_shell_identity_as_its_argv0() -> Result<(), Box<dyn std::error::Error>> {
    let shell = PathBuf::from(env!("CARGO_BIN_EXE_td-sh"));
    // `ash`, not `sh`: the identity is the one the case is GRADED against, so
    // it is also what `$SH` holds and what a nested `$SH -c` would report.
    let spec = "#### argv0\necho \"[$0]\"\n## STDOUT:\n[ash]\n## END\n";
    let cases = parse_spec(spec)?;
    let case = cases.first().ok_or("no case parsed")?;
    let outcome = run_case(&shell, &spec_helpers_bin(), case, ASH_DASH_CHAIN)?;
    assert!(outcome.passed, "argv0 case failed: {:?}", outcome.detail);
    // And the nested call agrees, which is the sentence the harness comment
    // makes and the reason argv[0] is spelled rather than left to the path.
    let spec = "#### nested argv0\necho \"[$0]\"; $SH -c 'echo \"[$0]\"'\n\
        ## STDOUT:\n[ash]\n[ash]\n## END\n";
    let cases = parse_spec(spec)?;
    let case = cases.first().ok_or("no case parsed")?;
    let outcome = run_case(&shell, &spec_helpers_bin(), case, ASH_DASH_CHAIN)?;
    assert!(outcome.passed, "nested argv0 case failed: {:?}", outcome.detail);
    Ok(())
}

/// `argv[0]` is the WORD, in both directions: what this shell reports as its
/// own `$0`, and what it hands a child. Only the binary shows either -- the
/// in-process harness spawns nothing and has no `argv[0]` of its own -- and
/// td-sh is both parent and child because it is the only argv-reporting
/// program the gate may assume exists.
#[test]
fn argv0_is_the_word_the_shell_was_given_not_the_path_it_resolved()
-> Result<(), Box<dyn std::error::Error>> {
    let shell = PathBuf::from(env!("CARGO_BIN_EXE_td-sh"));
    // Exclusively created and dropped with the test, so a planted symlink at a
    // predictable name cannot redirect the link written just below.
    let work = CaseWorkdir::new()?;
    let bin = work.path().join("bin");
    std::fs::create_dir(&bin)?;
    let link = bin.join("mysh");
    std::os::unix::fs::symlink(&shell, &link)?;

    // Its OWN `$0`: the name it was reached by, verbatim.
    let out = std::process::Command::new(&link).args(["-c", "echo $0"]).output()?;
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        format!("{}\n", link.display())
    );
    // `-c NAME` and a script operand still win over it.
    let out = std::process::Command::new(&link)
        .args(["-c", "echo $0", "NAME"])
        .output()?;
    assert_eq!(String::from_utf8_lossy(&out.stdout), "NAME\n");
    let script_file = work.path().join("s.sh");
    std::fs::write(&script_file, "echo $0\n")?;
    let out = std::process::Command::new(&link).arg(&script_file).output()?;
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        format!("{}\n", script_file.display())
    );

    // The leading `-` a login shell is announced by, which a resolved path can
    // never carry: this is the guard a profile opens with.
    let out = std::process::Command::new(&link)
        .arg0("-sh")
        .args(["-c", "case $0 in -*) echo LOGIN;; *) echo NOT;; esac"])
        .output()?;
    assert_eq!(String::from_utf8_lossy(&out.stdout), "LOGIN\n");

    // Every generated script QUOTES the directory: `$TMPDIR` may hold a space
    // or a glob character, and an unquoted `cd` would fail somewhere else in a
    // way that says nothing about argv[0].
    let dir = bin.display();
    // What it hands a CHILD found on PATH: `mysh`, the word, where the resolved
    // path was going out before -- a name the caller never wrote.
    let script = format!("PATH='{dir}'\nmysh -c 'echo $0'\n");
    let out = std::process::Command::new(&shell).arg("-c").arg(&script).output()?;
    assert_eq!(String::from_utf8_lossy(&out.stdout), "mysh\n");

    // A slash-bearing word goes through as WRITTEN rather than canonicalised,
    // so a relative spelling stays relative.
    let script = format!("cd '{dir}'\n./mysh -c 'echo $0'\n");
    let out = std::process::Command::new(&shell).arg("-c").arg(&script).output()?;
    assert_eq!(String::from_utf8_lossy(&out.stdout), "./mysh\n");

    // `exec` is a SECOND call site -- `CommandExt::exec` rather than `spawn` --
    // which the plain-command case above does not reach.
    let script = format!("PATH='{dir}'\nexec mysh -c 'echo $0'\n");
    let out = std::process::Command::new(&shell).arg("-c").arg(&script).output()?;
    assert_eq!(String::from_utf8_lossy(&out.stdout), "mysh\n");

    // `exec -a NAME` is the one thing that overrides the word, in both
    // spellings. The LAST `-a` wins.
    for form in ["-a renamed", "-arenamed", "-a first -a renamed"] {
        let script = format!("PATH='{dir}'\nexec {form} mysh -c 'echo $0'\n");
        let out = std::process::Command::new(&shell).arg("-c").arg(&script).output()?;
        assert_eq!(String::from_utf8_lossy(&out.stdout), "renamed\n", "{form}");
    }
    // The OTHER call site: a subshell cannot replace the process, so it falls
    // back to the ordinary spawn, which has its own `arg0`.
    let script = format!("PATH='{dir}'\n(exec -a renamed mysh -c 'echo $0')\n");
    let out = std::process::Command::new(&shell).arg("-c").arg(&script).output()?;
    assert_eq!(String::from_utf8_lossy(&out.stdout), "renamed\n");

    // `-a` takes the next word RAW, so `--` is a NAME and not a terminator.
    // Spelled absolutely: a bare name would meet ash's search divergence below
    // and so could not be measured against it.
    let script = format!("exec -a -- '{}' -c 'echo $0'\n", shell.display());
    let out = std::process::Command::new(&shell).arg("-c").arg(&script).output()?;
    assert_eq!(String::from_utf8_lossy(&out.stdout), "--\n");

    // It renames WITHOUT redirecting the search, unlike ash (ash.c:8354). The
    // two names must be DIFFERENT PROGRAMS or the assertion is vacuous.
    use std::os::unix::fs::PermissionsExt;
    let decoy = bin.join("decoy");
    std::fs::write(&decoy, format!("#!{}\necho I-AM-DECOY\n", shell.display()))?;
    std::fs::set_permissions(&decoy, std::fs::Permissions::from_mode(0o755))?;
    // A decoy that cannot EXECUTE would leave the rows below passing whatever
    // the shell did, which is the vacuity they exist to escape.
    let out = std::process::Command::new(&decoy).output()?;
    assert_eq!(String::from_utf8_lossy(&out.stdout), "I-AM-DECOY\n");
    for form in ["exec -a decoy mysh", "(exec -a decoy mysh"] {
        let close = if form.starts_with('(') { ")" } else { "" };
        let script = format!("PATH='{dir}'\n{form} -c 'echo $0 ran'{close}\n");
        let out = std::process::Command::new(&shell).arg("-c").arg(&script).output()?;
        assert_eq!(String::from_utf8_lossy(&out.stdout), "decoy ran\n", "{form}");
    }
    Ok(())
}

/// The INVOCATION's own option refusal, which is `set`'s: ash runs one
/// `options()` (ash.c:11452) from both, so the two spellings split there the
/// same way -- a letter raises with 2, a bad `-o NAME` reports through
/// `ash_msg`, which sets no status, and startup unwinds on it at 0. Only the
/// binary reaches this; `run_capturing` starts after it.
#[test]
fn the_command_lines_own_option_refusal_splits_the_way_sets_does()
-> Result<(), Box<dyn std::error::Error>> {
    let shell = PathBuf::from(env!("CARGO_BIN_EXE_td-sh"));
    // The command never runs in EITHER case -- what differs is the status.
    // A diagnostic names the shell by `$0`, which for a spawned binary is the
    // path it was invoked by. Building the expectation from that path rather
    // than hard-coding a name is what pins the prefix to argv[0].
    let me = shell.display().to_string();
    let args: [(&[&str], &str, i32); 4] = [
        (&["-z", "-c", "echo ALIVE"], "illegal option -z\n", 2),
        (&["+z", "-c", "echo ALIVE"], "illegal option +z\n", 2),
        (&["-o", "bogus", "-c", "echo ALIVE"], "illegal option -o bogus\n", 0),
        (&["+o", "bogus", "-c", "echo ALIVE"], "illegal option +o bogus\n", 0),
    ];
    for (args, err, code) in args {
        let out = std::process::Command::new(&shell).args(args).output()?;
        assert_eq!(String::from_utf8_lossy(&out.stderr), format!("{me}: {err}"), "{args:?}");
        assert_eq!(out.stdout, b"", "{args:?}");
        assert_eq!(out.status.code(), Some(code), "{args:?}");
    }
    // The prefix here is not the one the builtins dropped: it is ash's `arg0`
    // field, which at option-parse time is the whole prefix (`commandname` is
    // still NULL, so no name and no line follow it).
    let out = std::process::Command::new(&shell)
        .args(["-o", "nounset", "-c", "echo ok"])
        .output()?;
    assert_eq!((out.stdout, out.status.code()), (b"ok\n".to_vec(), Some(0)));
    Ok(())
}

/// The script OPERAND is opened before the shell exists, so its failure is the
/// one diagnostic the in-process harness cannot reach -- and it was still
/// printing `io::Error`'s Display after every other site had stopped. ash words
/// it through the same `setinputfile` the `.` builtin uses (ash.c:11257), so it
/// takes that quoted form too.
#[test]
fn a_missing_script_operand_is_reported_the_way_ash_reports_it()
-> Result<(), Box<dyn std::error::Error>> {
    let shell = PathBuf::from(env!("CARGO_BIN_EXE_td-sh"));
    let dir = std::env::temp_dir().join(format!("td-sh-cliopen-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    let missing = dir.join("nope-script");
    let out = std::process::Command::new(&shell).arg(&missing).output()?;
    let err = String::from_utf8_lossy(&out.stderr).into_owned();
    assert_eq!(
        err,
        format!(
            "{}: can't open '{}': No such file or directory\n",
            shell.display(),
            missing.display()
        )
    );
    assert_eq!(out.status.code(), Some(2));
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

/// The spawn path with REAL stdio, and `exec`'s own, which the in-process
/// harness cannot reach: it buffers stdout, so both fall back to the piped
/// spawn. Both errnos come back from `Command`, not from `resolve_program`.
#[test]
fn a_spawn_failure_with_real_stdio_gives_the_systems_reason()
-> Result<(), Box<dyn std::error::Error>> {
    use std::os::unix::fs::PermissionsExt;
    let shell = PathBuf::from(env!("CARGO_BIN_EXE_td-sh"));
    let dir = std::env::temp_dir().join(format!("td-sh-spawnreal-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    let f = dir.join("noexec");
    std::fs::write(&f, b"x")?;
    // A slash in the name, so the execute bit is the KERNEL's to object to.
    std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o644))?;
    let p = f.display();
    let me = shell.display().to_string();
    for (src, want) in [
        (format!("{p}"), format!("{me}: {p}: Permission denied\n")),
        // `exec` is a builtin, so it is `commandname` while it runs and the
        // diagnostic carries a line; the bare spelling above enters no builtin
        // and `-c` sets none, so it carries neither name nor line.
        (format!("exec {p}"), format!("{me}: exec: line 1: {p}: Permission denied\n")),
    ] {
        let out = std::process::Command::new(&shell).arg("-c").arg(&src).output()?;
        assert_eq!(String::from_utf8_lossy(&out.stderr), want, "src: {src}");
        assert_eq!(out.status.code(), Some(126), "src: {src}");
    }
    let _ = std::fs::remove_dir_all(&dir);
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

/// `trap '' SIG` is a KERNEL disposition, and the whole point of it being one is
/// that `execve` carries it into the children a script starts. Nothing
/// in-process can show that — a disposition is per-process, so only a real child
/// can be asked — and the answer has to come out of the kernel rather than out
/// of td-sh, which is what `/proc/self/status`'s `SigIgn:` mask is for. The
/// child is the shell running ITSELF, the one external a target-side gate can
/// count on, and it reports the mask with builtins alone.
///
/// SIGPIPE (bit 0x1000) is this guarantee's one documented exception: Rust's
/// runtime ignores SIGPIPE before `main`, `std::process::Command` undoes that in
/// each child it spawns, and neither is reachable from safe code — so `trap ''
/// PIPE` cannot reach a child, and a td-sh child could not tell an inherited
/// ignore from the one its own runtime just installed.
///
/// The floor a row is measured against is therefore not a constant: whatever ran
/// this test is in the child's mask too, so an absolute assertion fails under an
/// init that ignores SIGHUP — the child reports 0x1003 — for a reason that is
/// nothing to do with the shell. It is not FREE either, and pinning it to
/// exactly `own | pipe` is what keeps the "and NOTHING else" an absolute
/// assertion used to carry: a shell that leaked an ignore of its own would
/// otherwise be absorbed into the baseline instead of breaking it.
#[test]
fn an_ignore_trap_is_inherited_by_a_child() -> Result<(), Box<dyn std::error::Error>> {
    let shell = PathBuf::from(env!("CARGO_BIN_EXE_td-sh"));
    let child = "\"$TD_SH_BIN\" -c 'while read l; do case \"$l\" in SigIgn:*) echo \"$l\";; \
                 esac; done < /proc/self/status'";
    // Bits are counted from signal 1, so SIGPIPE (13) is bit 12.
    let pipe = 1u64 << 12;
    let to_mask = |hex: &str, from: &str| -> Result<u64, Box<dyn std::error::Error>> {
        Ok(u64::from_str_radix(hex.trim(), 16)
            .map_err(|e| format!("{from}: unreadable mask {hex:?}: {e}"))?)
    };
    // Two parsers on purpose: `/proc/self/status` is a document to SEARCH, while
    // the probe prints one line and nothing else. Extra output THERE is the shell
    // doing something no row asked for -- `trap 'echo x'` running its action
    // eagerly, say -- so it must fail rather than be skipped past.
    let own_mask = |text: &str| -> Result<u64, Box<dyn std::error::Error>> {
        let hex = text
            .lines()
            .find_map(|l| l.strip_prefix("SigIgn:"))
            .ok_or("the test's own: no SigIgn: line in /proc/self/status")?;
        to_mask(hex, "the test's own")
    };
    let mask = |body: &str| -> Result<u64, Box<dyn std::error::Error>> {
        let out = std::process::Command::new(&shell)
            .arg("-c")
            .arg(body)
            .env("TD_SH_BIN", &shell)
            .output()?;
        // Reported rather than discarded: a shell that died leaves no mask, and
        // "unreadable mask" would name the symptom instead of the reason.
        if !out.status.success() {
            return Err(format!(
                "{body}: shell failed with {}: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr)
            )
            .into());
        }
        let text = String::from_utf8_lossy(&out.stdout);
        // Exactly one line: a blank one is extra output too, so only the final
        // newline comes off and any other is a failure.
        let line = text.strip_suffix('\n').unwrap_or(&text);
        let hex = line
            .strip_prefix("SigIgn:")
            .filter(|_| !line.contains('\n'))
            .ok_or_else(|| format!("{body}: unexpected probe output {text:?}"))?;
        to_mask(hex, body)
    };
    // `execve` carries THIS process's ignores into the child and `Command` resets
    // only SIGPIPE, so the floor is knowable exactly rather than merely tolerated.
    let own = own_mask(&std::fs::read_to_string("/proc/self/status")?)?;
    let base = mask(child)?;
    assert_eq!(
        base, own | pipe,
        "a fresh shell ignores more than it inherited: {base:#x} from {own:#x}"
    );
    // EVERY signal the baseline leaves free, not a preferred one: a spawn
    // force-derives the two the interrupt guard moves, so INT and QUIT cannot see
    // a subshell that failed to hand a disposition back, where the other three
    // can. Running all of them is what makes the set independent of whatever the
    // environment happens to ignore -- and POSIX 2.9.3 hands an asynchronous list
    // `SIG_IGN` for INT and QUIT, so `… &` from ash really does remove two.
    let free: Vec<(&str, u64)> = [
        ("INT", 1u64 << 1),
        ("QUIT", 1 << 2),
        ("USR1", 1 << 9),
        ("USR2", 1 << 11),
        ("TERM", 1 << 14),
    ]
    .into_iter()
    .filter(|&(_, bit)| base & bit == 0)
    .collect();
    if free.is_empty() {
        return Err("every candidate signal is already ignored".into());
    }
    for (sig, bit) in free {
        for (body, extra) in [
            (format!("trap '' {sig}; {child}"), bit),
            // A subshell's trap table is its OWN, so the child started after it is
            // derived from a table with no ignore in it. For a signal outside the
            // guard's pair this also catches the KERNEL disposition not being handed
            // back, which for INT and QUIT a spawn normalises away first.
            (format!("( trap '' {sig} ); {child}"), 0),
            // A catcher cannot be installed, so it leaves the default behind.
            (format!("trap 'echo x' {sig}; {child}"), 0),
            // The pair that pins `fork_shell` CARRYING `sig_may_set`, and the only
            // pair that can: from outside, a subshell that cleared an inherited
            // ignore is indistinguishable from one that declined to touch it —
            // both leave the parent ignoring it. The difference is visible only to
            // the subshell's OWN children. Without the carried cache the subshell
            // would re-query, find the ignore its parent installed, mistake it for
            // one inherited from outside the process, and decline; then the first
            // line below would report an ignore rather than none.
            (format!("trap '' {sig}; ( trap - {sig}; {child} )"), 0),
            (format!("trap '' {sig}; ( trap - {sig} ); {child}"), bit),
        ] {
            // The filter above already guarantees this for `extra` of `bit` or 0; it
            // is here so a row added with some OTHER signal's bit cannot be absorbed.
            assert_eq!(base & extra, 0, "{body}: the baseline already ignores {extra:#x}");
            let want = base | extra;
            let seen = mask(&body)?;
            assert_eq!(seen, want, "{body}: child inherited {seen:#x}, wanted {want:#x}");
        }
    }
    Ok(())
}

/// A REGULAR builtin's error ends the COMMAND; a SPECIAL one ends the shell.
///
/// ash wraps every builtin in its own handler and re-raises only when the command
/// word was special (ash.c:10619). Surviving is what makes `cd`'s write ORDER
/// observable at all, so it is pinned here too: ash writes OLDPWD before `curdir`
/// moves (ash.c:2865, 2883) and has already chdir'd, so a refusal on OLDPWD
/// leaves the `pwd` builtin behind while a CHILD sees the new directory. A test
/// that asked only `pwd` could not tell that order from moving both together.
#[test]
fn a_regular_builtins_error_ends_the_command_not_the_shell(
) -> Result<(), Box<dyn std::error::Error>> {
    let shell = PathBuf::from(env!("CARGO_BIN_EXE_td-sh"));
    let run = |program: &str| -> Result<(String, i32), Box<dyn std::error::Error>> {
        let out = std::process::Command::new(&shell)
            .arg("-c")
            .arg(program)
            .env("TD_SH_BIN", &shell)
            .output()?;
        let code = out.status.code().ok_or_else(|| format!("{program}: killed"))?;
        Ok((String::from_utf8_lossy(&out.stdout).into_owned(), code))
    };
    // The diagnostic goes to stderr, the status stands, and the line runs on.
    for program in [
        "readonly N; read N </dev/null; echo AFTER",
        "readonly O; getopts ab O -a; echo AFTER",
        "readonly OPTIND; getopts ab O -a; echo AFTER",
        "cd /; readonly PWD; cd /tmp; echo AFTER",
        "cd /; readonly OLDPWD; cd /tmp; echo AFTER",
    ] {
        assert_eq!(run(program)?, ("AFTER\n".to_string(), 0), "{program}");
    }
    // ...and a special builtin still ends it, which is what keeps this a rule
    // rather than "nothing is fatal". `local` is special in ash but not POSIX.
    for program in [
        "f() { readonly L; local L; }; f; echo AFTER",
        "readonly E=1; export E=2; echo AFTER",
    ] {
        assert_eq!(run(program)?, (String::new(), 2), "{program}");
    }
    // Surviving also makes `getopts`' hidden cursor observable, and a REFUSED
    // write has to leave the scan restartable rather than advanced -- ash parks
    // a sentinel on entry and restores the cursor only once every write lands,
    // so the next call rescans from word 1. Asserting only that the line ran on
    // would miss this entirely: the shell survives either way.
    for (program, want) in [
        ("set -- -a -b; readonly O; getopts ab O; getopts ab X; echo \"$X/$OPTIND\"", "a/2\n"),
        (
            "set -- -a -b -c; getopts abc X; readonly Y; getopts abc Y; \
             getopts abc Z; echo \"$Z/$OPTIND\"",
            "a/2\n",
        ),
        // OPTARG is refusable too, and earlier in the same scan.
        (
            "set -- -a -b; getopts ab X; readonly OPTARG; getopts ab Y; \
             getopts ab Z; echo \"$Z/$OPTIND\"",
            "/2\n",
        ),
        // OPTARG is UNSET when the scan ends (ash.c:11692), which td-sh skipped
        // -- and that one is visible with no `readonly` in play at all.
        ("set -- -c val; getopts \"c:\" O; getopts \"c:\" O; echo \"[$OPTARG]\"", "[]\n"),
        // The unset is refusable exactly as the writes are, `unsetvar` being
        // `setvar(s, NULL, 0)`, so it ends the command and restarts the scan.
        (
            "readonly OPTARG=old; set -- -Z; getopts a O; echo \"rc=$? OPTIND=$OPTIND\"",
            "rc=2 OPTIND=1\n",
        ),
        // ...and with nothing refused the scan simply runs on, so the rows above
        // are about the refusal rather than about `getopts` generally.
        ("set -- -a -b; getopts ab O; getopts ab X; echo \"$X/$OPTIND\"", "b/3\n"),
    ] {
        assert_eq!(run(program)?.0, want, "{program}");
    }
    // Only `/` and `/tmp`, so the case needs nothing the kernel does not give it.
    let cd = |ro: &str, show: &str| {
        format!(
            "cd /; cd /tmp; readonly {ro}; cd /; \
             echo \"builtin=$(pwd) child=$(\"$TD_SH_BIN\" -c pwd) {show}\""
        )
    };
    // The refusal lands BEFORE the shell's idea of the directory moves...
    assert_eq!(run(&cd("OLDPWD", "PWD=$PWD"))?.0, "builtin=/tmp child=/ PWD=/tmp\n");
    // ...and after it, for the write that comes second.
    assert_eq!(run(&cd("PWD", "OLDPWD=$OLDPWD"))?.0, "builtin=/ child=/ OLDPWD=/tmp\n");
    Ok(())
}

/// The Oils helper stand-in the corpus expects on PATH, built by this crate as
/// a second bin. `run_case` stages it under every name in `SPEC_HELPERS`.
fn spec_helpers_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_spec_helpers"))
}

/// Wait for `child`, killing it if it outlives `limit`. Returns whether it ended
/// on its own.
///
/// A test about a TIMEOUT must not be able to hang the suite: a `read -t 1` that
/// read its argument as seconds-times-a-thousand would otherwise sit for a
/// quarter of an hour with nothing to say about why.
fn wait_within(
    child: &mut std::process::Child,
    limit: std::time::Duration,
) -> Result<bool, Box<dyn std::error::Error>> {
    let deadline = std::time::Instant::now() + limit;
    loop {
        if child.try_wait()?.is_some() {
            return Ok(true);
        }
        if std::time::Instant::now() >= deadline {
            // Ignored, not propagated: a child that exited between the
            // `try_wait` above and here makes `kill` fail, and reporting THAT
            // instead of the assertion below would hide the real result.
            let _ = child.kill();
            return Ok(false);
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// `read -t` on a descriptor that can really block: it waits, and it stops.
///
/// Every in-crate test of this builtin reads something that cannot wait — a
/// here-document cursor, `/dev/null`, a sibling pipeline stage that is about to
/// finish — so all of them pass against a `read -t` that ignores its timeout
/// entirely, which is what this shell did until poll(2) landed. The wait is only
/// observable over a real pipe whose writer is a process that has not written,
/// so the harness holds that writer itself and hands the child the read end as
/// its stdin. That is the FIFO case without needing a FIFO: td-sh has no
/// `mkfifo`, and `std` cannot make one without a dependency.
///
/// Timings are asserted as loose one-sided bounds. The point is not that the
/// wait is accurate but that it happens at all and ends by itself; a 4-second
/// ceiling on a 5-second timeout distinguishes "the write woke it" from "it sat
/// out the timeout and reported one" without pinning the scheduler.
#[test]
fn read_dash_t_waits_on_a_pipe_that_can_block() -> Result<(), Box<dyn std::error::Error>> {
    use std::io::Write as _;
    let shell = PathBuf::from(env!("CARGO_BIN_EXE_td-sh"));

    // Nothing is ever written and the write end stays open, so the timeout is
    // the only thing that can end this read. Status 1 is ash's answer, which
    // this shell's corpus already pins; bash reports 142.
    let (reader, writer) = std::io::pipe()?;
    let start = std::time::Instant::now();
    let mut child = std::process::Command::new(&shell)
        .arg("-c")
        .arg("read -t 1 v; echo \"rc=$? [$v]\"")
        .stdin(std::process::Stdio::from(reader))
        .stdout(std::process::Stdio::piped())
        .spawn()?;
    let ended = wait_within(&mut child, std::time::Duration::from_secs(30))?;
    let waited = start.elapsed();
    let out = child.wait_with_output()?;
    drop(writer);
    assert!(ended, "read -t 1 never returned on a pipe with no writer activity");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "rc=1 []\n");
    assert!(
        waited >= std::time::Duration::from_millis(900),
        "read -t 1 returned after {waited:?}, so it did not wait"
    );

    // ... and a write DURING the timeout is what ends it, well before the
    // timeout would have.
    let (reader, mut writer) = std::io::pipe()?;
    let start = std::time::Instant::now();
    let mut child = std::process::Command::new(&shell)
        .arg("-c")
        .arg("read -t 5 v; echo \"rc=$? [$v]\"")
        .stdin(std::process::Stdio::from(reader))
        .stdout(std::process::Stdio::piped())
        .spawn()?;
    std::thread::sleep(std::time::Duration::from_millis(300));
    writer.write_all(b"hi\n")?;
    let ended = wait_within(&mut child, std::time::Duration::from_secs(30))?;
    let waited = start.elapsed();
    let out = child.wait_with_output()?;
    assert!(ended, "read -t 5 never returned after its input arrived");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "rc=0 [hi]\n");
    assert!(
        waited < std::time::Duration::from_secs(4),
        "read -t 5 took {waited:?}, so it sat out the timeout instead of waking"
    );

    // A PARTIAL line, which is what makes the deadline have to survive the
    // first byte. The writer sends two bytes with no delimiter and keeps the
    // pipe open, so poll reports ready, the read consumes both, and there is
    // nothing more to come. A timeout checked once -- before the loop rather
    // than before each byte -- blocks here forever.
    let (reader, mut writer) = std::io::pipe()?;
    let start = std::time::Instant::now();
    let mut child = std::process::Command::new(&shell)
        .arg("-c")
        .arg("read -t 1 v; echo \"rc=$? [$v]\"")
        .stdin(std::process::Stdio::from(reader))
        .stdout(std::process::Stdio::piped())
        .spawn()?;
    writer.write_all(b"ab")?;
    let ended = wait_within(&mut child, std::time::Duration::from_secs(30))?;
    let waited = start.elapsed();
    let out = child.wait_with_output()?;
    drop(writer);
    assert!(ended, "read -t 1 hung on a partial line");
    // ash reports 1 and drops what followed the last field delimiter, where an
    // EOF assigns it. bash differs on both halves (142, and `ab` assigned).
    assert_eq!(String::from_utf8_lossy(&out.stdout), "rc=1 []\n");
    assert!(
        waited >= std::time::Duration::from_millis(900),
        "read -t 1 gave up after {waited:?} instead of waiting out its deadline"
    );

    // ... and with SEVERAL names, the fields the loop completed before the
    // deadline stand, because `read` assigns each as it consumes the separator
    // rather than at the end. Only the trailing partial is dropped. That makes
    // "a timeout assigns nothing" wrong as a blanket statement, which is why it
    // is pinned here rather than left to a comment.
    let (reader, mut writer) = std::io::pipe()?;
    let mut child = std::process::Command::new(&shell)
        .arg("-c")
        .arg("IFS=: read -t 1 a b; echo \"rc=$? a=[$a] b=[$b]\"")
        .stdin(std::process::Stdio::from(reader))
        .stdout(std::process::Stdio::piped())
        .spawn()?;
    writer.write_all(b"x:y")?;
    let ended = wait_within(&mut child, std::time::Duration::from_secs(30))?;
    let out = child.wait_with_output()?;
    drop(writer);
    assert!(ended, "read -t 1 hung with several names");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "rc=1 a=[x] b=[]\n");
    Ok(())
}

/// A script on stdin is read only as far as it has RUN, so the commands in it
/// share the descriptor with the script itself.
///
/// This used to slurp stdin whole before running anything, which is invisible
/// until a command reads stdin too: `read` found end of input and the line it
/// should have taken was then executed as a command. POSIX requires a shell not
/// to consume more of a non-seekable input than the commands it has executed
/// need, and bash does not.
#[test]
fn a_stdin_script_leaves_the_rest_of_itself_for_read()
-> Result<(), Box<dyn std::error::Error>> {
    use std::io::Write as _;
    let shell = PathBuf::from(env!("CARGO_BIN_EXE_td-sh"));
    for (script, want) in [
        // The line after `read` is data, not a command.
        ("read v\nDATA\necho \"got=[$v]\"\n", "got=[DATA]\n"),
        // Two of them in a row, so the position advances rather than resetting.
        (
            "read a\nONE\nread b\nTWO\necho \"[$a][$b]\"\n",
            "[ONE][TWO]\n",
        ),
        // A multi-line construct still reads as far as it needs and no further.
        (
            "for i in 1 2; do\n  read v\n  echo \"i=$i v=$v\"\ndone\nA\nB\n",
            "i=1 v=A\ni=2 v=B\n",
        ),
        // A here-document is consumed by the parser, not by `read`.
        ("cat <<EOF\nbody\nEOF\nread v\nAFTER\necho \"[$v]\"\n", "body\n[AFTER]\n"),
    ] {
        let mut child = std::process::Command::new(&shell)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()?;
        match child.stdin.take() {
            Some(mut w) => w.write_all(script.as_bytes())?,
            None => return Err("no stdin pipe".into()),
        }
        let out = child.wait_with_output()?;
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            want,
            "script {script:?}"
        );
    }
    Ok(())
}

/// A stdin that is a REGULAR FILE takes the same script to the same place a
/// pipe does.
///
/// The test above drives stdin through a pipe, which can only be read a byte at
/// a time — there is no way to give an over-read back. A file is read a block at
/// a time and rewound to the byte after the newline instead, and that rewind is
/// the whole of the path's correctness: without it the block takes lines the
/// script's own `read` is owed, and every case here prints the wrong thing while
/// the piped ones stay green. So both paths are run over the same scripts and
/// both are held to the same expected output, rather than to each other — two
/// paths agreeing on the wrong answer is the failure this would otherwise miss.
///
/// Each child is BOUNDED, because the two ways a rewind can be wrong fail
/// differently: rewinding too little loses bytes and shows up as a mismatch,
/// while rewinding too far re-reads the newline forever, and an unbounded
/// `output()` would hang the suite instead of reddening it.
///
/// Only builtins appear, so the assertion is about the descriptor rather than
/// about what happens to be on PATH.
#[test]
fn a_file_stdin_script_agrees_with_a_piped_one()
-> Result<(), Box<dyn std::error::Error>> {
    use std::io::Write as _;
    let shell = PathBuf::from(env!("CARGO_BIN_EXE_td-sh"));
    let long = "x".repeat(700);
    let cases = [
        ("read v\nDATA\necho \"got=[$v]\"\n".to_string(), "got=[DATA]\n"),
        (
            "read a\nONE\nread b\nTWO\necho \"[$a][$b]\"\n".to_string(),
            "[ONE][TWO]\n",
        ),
        (
            "for i in 1 2; do\n  read v\n  echo \"i=$i v=$v\"\ndone\nA\nB\n".to_string(),
            "i=1 v=A\ni=2 v=B\n",
        ),
        // A here-document is consumed by the PARSER rather than by `read`, so
        // its lines go through the reader under test.
        ("read v <<EOF\nbody\nEOF\necho \"[$v]\"\n".to_string(), "[body]\n"),
        // Input ENDING in a fold. This reader reaches that decision by a route
        // of its own -- an unsealed pull rewinds, the source answers `Eof`, the
        // scan seals and the re-pull spends the fold -- so the whole-string
        // reader passing says nothing about it.
        ("echo x \\\n".to_string(), "x\n"),
        // A construct left open at the end of a LINE, for the same reason: the
        // whole-string reader has the second `)` in hand already, and only this
        // one has to ask for it.
        ("echo $((1+2)\\\n)\n".to_string(), "3\n"),
        // A SCRIPT line longer than SCRIPT_BLOCK, so the seekable path has to
        // accumulate across reads before it finds a newline to rewind to. The
        // length has to be in a line the PARSER reads: as data after `read v`
        // it would be consumed by the builtin a byte at a time instead, and
        // the accumulation branch would never run. The `read` that follows is
        // what checks the rewind ending a multi-block line, since it can only
        // see `DATA` if the descriptor came back to the right place.
        (
            format!("v={long}\nread w\nDATA\necho \"[$w][${{#v}}]\"\n"),
            "[DATA][700]\n",
        ),
        // A first line ending EXACTLY on the block boundary: `v=` + 253 + the
        // newline is 256, so the read stops with nothing over-read and the
        // rewind is skipped. That is the one path through the arithmetic that
        // never issues a seek, and EOF is the only other way to reach it.
        (
            format!("v={}\nread w\nDATA\necho \"[$w][${{#v}}]\"\n", "x".repeat(253)),
            "[DATA][253]\n",
        ),
        // No trailing newline, so the last line ends at end of input rather
        // than at a byte the rewind can point past.
        ("read v\nDATA\necho \"end=[$v]\"".to_string(), "end=[DATA]\n"),
    ];
    let dir = std::env::temp_dir().join(format!("td-sh-stdin-{}", std::process::id()));
    // Fresh, so a leak from a previous failing run cannot feed this one.
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    for (i, (script, want)) in cases.iter().enumerate() {
        let mut child = std::process::Command::new(&shell)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()?;
        match child.stdin.take() {
            Some(mut w) => w.write_all(script.as_bytes())?,
            None => return Err("no stdin pipe".into()),
        }
        let ended = wait_within(&mut child, std::time::Duration::from_secs(30))?;
        let piped = child.wait_with_output()?;
        assert!(ended, "piped {script:?} did not finish");
        // Status first, so a shell that died reports what it said rather than
        // presenting as an empty-string mismatch.
        assert!(
            piped.status.success(),
            "piped {script:?} failed: {}",
            String::from_utf8_lossy(&piped.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&piped.stdout),
            *want,
            "piped: {script:?}"
        );

        let path = dir.join(format!("case{i}.sh"));
        std::fs::write(&path, script)?;
        let mut child = std::process::Command::new(&shell)
            .stdin(std::process::Stdio::from(std::fs::File::open(&path)?))
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()?;
        let ended = wait_within(&mut child, std::time::Duration::from_secs(30))?;
        let seekable = child.wait_with_output()?;
        // A rewind that goes one byte too far re-reads the newline forever, so
        // this is the assertion that catches it.
        assert!(ended, "file {script:?} did not finish");
        assert!(
            seekable.status.success(),
            "file {script:?} failed: {}",
            String::from_utf8_lossy(&seekable.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&seekable.stdout),
            *want,
            "file: {script:?}"
        );
    }
    std::fs::remove_dir_all(&dir)?;
    Ok(())
}

/// The shell-ending paths a stdin script has still end it once that script is
/// read incrementally rather than whole.
///
/// None of this is new behaviour — the slurping version ran units one at a time
/// out of the string it had read, so it reported a syntax error after the
/// commands before it too, with the same status. That is exactly why it is
/// pinned here: replacing the reader is the kind of change that would break
/// `exit`, the EXIT trap or `set -e` without any of them being its subject.
#[test]
fn a_stdin_script_still_ends_where_it_should()
-> Result<(), Box<dyn std::error::Error>> {
    use std::io::Write as _;
    let shell = PathBuf::from(env!("CARGO_BIN_EXE_td-sh"));
    let mut child = std::process::Command::new(&shell)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    match child.stdin.take() {
        Some(mut w) => w.write_all(b"echo before\nif\n")?,
        None => return Err("no stdin pipe".into()),
    }
    let out = child.wait_with_output()?;
    assert_eq!(String::from_utf8_lossy(&out.stdout), "before\n");
    assert_eq!(out.status.code(), Some(2), "a syntax error ends the script");
    // Unchanged from the slurping reader, which is the point of asserting it.
    assert!(
        !String::from_utf8_lossy(&out.stderr).is_empty(),
        "the error is reported"
    );

    // ... and the shell-ending paths still end it: `exit` stops reading, an
    // EXIT trap still runs, and `set -e` still reports the failing status.
    //
    // The last three are about the BYTES rather than the control flow, and each
    // is a regression a line-at-a-time reader invites: the script's own text is
    // not this reader's to normalise. A final line with no newline must stay
    // unterminated, or a trailing `\` becomes a line continuation and the
    // script is incomplete instead of complete; a carriage return is data in a
    // script where it is noise on a console; and input that fails to READ is
    // reported and exits 2, where end of input is a script that simply ended.
    for (script, want_out, want_code) in [
        ("echo a\nexit 7\necho never\n", "a\n", 7),
        ("trap 'echo bye' EXIT\necho hi\n", "hi\nbye\n", 0),
        ("set -e\nfalse\necho never\n", "", 1),
        ("printf '<%s>' foo\\", "<foo\\>", 0),
        ("echo one\r\necho two\r\n", "one\r\ntwo\r\n", 0),
        ("v='a\rb'\nprintf '%s|' \"$v\"\n", "a\rb|", 0),
    ] {
        let mut child = std::process::Command::new(&shell)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()?;
        match child.stdin.take() {
            Some(mut w) => w.write_all(script.as_bytes())?,
            None => return Err("no stdin pipe".into()),
        }
        let out = child.wait_with_output()?;
        assert_eq!(String::from_utf8_lossy(&out.stdout), want_out, "{script:?}");
        assert_eq!(out.status.code(), Some(want_code), "{script:?}");
    }

    // A script that is not UTF-8 is REPORTED, not silently taken for the end of
    // one. Bytes, not `&str`, because the case is a byte no `&str` can hold.
    let mut child = std::process::Command::new(&shell)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    match child.stdin.take() {
        Some(mut w) => w.write_all(b"\xff\n")?,
        None => return Err("no stdin pipe".into()),
    }
    let out = child.wait_with_output()?;
    assert_eq!(out.status.code(), Some(2), "a script that will not decode");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("stdin"),
        "the failure names stdin: {:?}",
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(())
}

/// An operator split by a `\<newline>` reads the same however the script
/// arrives. The line-at-a-time readers are the ones that can get this wrong,
/// because they see the fold at the end of what they have read and the operator
/// is not finished there — measured, a stdin-fed `true &\<newline>& echo yes`
/// ran `true &` and reported a syntax error on the second line, where the same
/// text under `-c` ran. Both spellings of each shape below run under busybox
/// ash 1.37.0.
#[test]
fn an_operator_split_across_lines_reads_the_same_from_stdin()
-> Result<(), Box<dyn std::error::Error>> {
    use std::io::Write as _;
    let shell = PathBuf::from(env!("CARGO_BIN_EXE_td-sh"));
    for (script, want) in [
        ("true &\\\n& echo yes\n", "yes\n"),
        ("false |\\\n| echo yes\n", "yes\n"),
        ("case a in a) echo hit ;\\\n; esac\n", "hit\n"),
        ("read v <\\\n<E\nbody\nE\necho $v\n", "body\n"),
        // The one shape whose rewind must UNDO lexer state: `<<` reserves a
        // here-document slot and arms `awaiting` before the fold that follows
        // can leave the operator open, so `Mark` has to carry both back.
        ("read v <<\\\n-E\n\tbody\nE\necho $v\n", "body\n"),
    ] {
        let mut child = std::process::Command::new(&shell)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()?;
        match child.stdin.take() {
            Some(mut w) => w.write_all(script.as_bytes())?,
            None => return Err("no stdin pipe".into()),
        }
        let out = child.wait_with_output()?;
        assert_eq!(String::from_utf8_lossy(&out.stdout), want, "{script:?}");
        assert_eq!(String::from_utf8_lossy(&out.stderr), "", "{script:?}");
        assert_eq!(out.status.code(), Some(0), "{script:?}");
        // The `-c` spelling is the one that already worked, so it is what the
        // stdin answer is held against rather than only against `want`. All
        // three fields, or a diagnostic beside the right output would pass.
        let same = std::process::Command::new(&shell).arg("-c").arg(script).output()?;
        assert_eq!(String::from_utf8_lossy(&same.stdout), want, "-c {script:?}");
        assert_eq!(String::from_utf8_lossy(&same.stderr), "", "-c {script:?}");
        assert_eq!(same.status.code(), Some(0), "-c {script:?}");
    }
    Ok(())
}

/// The `read` builtin takes ONE BYTE off stdin, leaving the rest for the next
/// reader — `sh -c 'read a; cat'` on a pipe.
///
/// It used to read through `std::io::Stdin`, a `BufReader`, so the first `read`
/// took up to 8 KiB off a descriptor it shares and `cat` saw nothing. That was a
/// divergence on its own, and `read -t` made it a wrong ANSWER too: bytes in the
/// shell's buffer are invisible to `poll(2)`, which can only see what is still
/// in the kernel, so a line already in hand read as a timeout.
#[test]
fn read_leaves_the_rest_of_stdin_for_the_next_reader()
-> Result<(), Box<dyn std::error::Error>> {
    use std::io::Write as _;
    let shell = PathBuf::from(env!("CARGO_BIN_EXE_td-sh"));
    // `cat` is an external command the harness cannot rely on, so the second
    // reader is the shell's own `read` in a CHILD process: it shares the
    // descriptor exactly as `cat` would, and it exists wherever the test does.
    let body = "read a; echo \"a=$a\"; \"$TD_SH_BIN\" -c 'read b; echo \"b=$b\"'";
    let mut child = std::process::Command::new(&shell)
        .arg("-c")
        .arg(body)
        .env("TD_SH_BIN", &shell)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()?;
    match child.stdin.take() {
        Some(mut w) => w.write_all(b"one\ntwo\n")?,
        None => return Err("no stdin pipe".into()),
    }
    let out = child.wait_with_output()?;
    assert_eq!(String::from_utf8_lossy(&out.stdout), "a=one\nb=two\n");
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
        // `unset` of a LOCALISED name keeps the entry the way a bare `local` does,
        // so a reassignment still exports; on a global it takes the attribute away
        // and the reassignment does not.
        (format!("f() {{ local TD_SH_X; unset TD_SH_X; TD_SH_X=H; {child}; }}; f"), "[H]\n"),
        // Being INSIDE a function is not the test -- only the declaration is, so an
        // undeclared name loses the attribute at depth just as it does at the top.
        (format!("f() {{ unset TD_SH_X; TD_SH_X=H; {child}; }}; f"), "[UNSET]\n"),
        // The EXIT trap runs inside the frame the shell died in, so the name it
        // declared still reads as declared there.
        // (through a helper, so the child's single quotes do not nest in the trap's)
        (format!("c() {{ {child}; }}; trap 'unset TD_SH_X; TD_SH_X=H; c' EXIT; f() {{ local TD_SH_X; exit 0; }}; f"), "[H]\n"),
        // `set -a` ORs VEXPORT into what an unset writes (ash.c:2417), so the free
        // test can never hold and the entry survives -- marked for export whether
        // or not it ever was, and whether or not it was declared.
        (format!("set -a; unset TD_SH_X; set +a; TD_SH_X=H; {child}"), "[H]\n"),
        (format!("f() {{ local TD_SH_X; set -a; unset TD_SH_X; set +a; TD_SH_X=H; {child}; }}; f"), "[H]\n"),
        // The declaration lives on the VARIABLE, so an INNER function's `unset`
        // sees an outer frame's `local` too -- and a subshell, which gets a copy
        // of the variables, carries the answer with it.
        (format!("i() {{ unset TD_SH_X; }}; f() {{ local TD_SH_X; i; TD_SH_X=H; {child}; }}; f"), "[H]\n"),
        (format!("f() {{ local TD_SH_X; ( unset TD_SH_X; TD_SH_X=H; {child} ); }}; f"), "[H]\n"),
        // ... and unwinds with the frame: after `f` returns the name is global
        // again, so `unset` there takes the attribute with it.
        (format!("f() {{ local TD_SH_X; }}; f; unset TD_SH_X; TD_SH_X=H; {child}"), "[UNSET]\n"),
    ]
    .into_iter()
    // A name the shell has never seen: `local` still has to record the
    // declaration, which means creating the entry to hold it (ash's
    // `setvar(name, NULL, VSTRFIXED)`). Kept apart because the cases above all
    // start from an environment variable, which pre-creates it.
    .chain({
        let child = "\"$TD_SH_BIN\" -c 'echo [${TD_SH_Y-UNSET}]'";
        [
            (format!("f() {{ local TD_SH_Y; export TD_SH_Y; unset TD_SH_Y; TD_SH_Y=H; {child}; }}; f"), "[H]\n"),
            (format!("i() {{ unset TD_SH_Y; }}; f() {{ local TD_SH_Y; export TD_SH_Y; i; TD_SH_Y=H; {child}; }}; f"), "[H]\n"),
            // `set -a` creates the entry outright for a name the shell never had.
            (format!("set -a; unset TD_SH_Y; set +a; TD_SH_Y=H; {child}"), "[H]\n"),
        ]
    }) {
        let out = std::process::Command::new(&shell)
            .arg("-c")
            .arg(&body)
            .env("TD_SH_BIN", &shell)
            .env("TD_SH_X", "G")
            // The TD_SH_Y cases test a name the shell has never seen, so it must
            // not arrive from whatever environment the gate itself ran in.
            .env_remove("TD_SH_Y")
            .output()?;
        assert_eq!(String::from_utf8_lossy(&out.stdout), want, "{body}");
    }
    Ok(())
}

/// ash prints only the part of a name that IS a name (`endofname`, ash.c:11580),
/// so an environment entry no shell can name -- which td-sh still passes through
/// to its children -- cannot turn `export -p` into something `eval` rejects. Only
/// a real child can carry such a name in, so this cannot be a unit test.
///
/// One entry ash does NOT survive is an empty name: it prints `export ='v'`,
/// which `eval` rejects. td-sh cannot emit that line at all, because it needs a
/// name that is BOTH empty and whole, and no import produces one -- Rust's
/// environment parse looks for the `=` from offset 1, so the name it yields is
/// never empty, and `export ''` is a usage error. The cost of answering it that
/// way is `=a=b`, which imports as a name of `=a` and lists as a bare `export`
/// where ash prints `export ='a=b'`. That and an undecodable name -- which ash
/// lists truncated and td-sh does not list at all, not being a variable -- are
/// the two entries whose listing is not ash's.
#[test]
fn the_export_listing_is_eval_safe() -> Result<(), Box<dyn std::error::Error>> {
    let shell = PathBuf::from(env!("CARGO_BIN_EXE_td-sh"));
    let run = |src: &str| -> Result<String, Box<dyn std::error::Error>> {
        let out = std::process::Command::new(&shell)
            .arg("-c")
            .arg(src)
            .env_clear()
            .env("PATH", "/usr/bin")
            .env("test-test", "v")
            .env("TD_SH_Q", "it's")
            .env("1digit", "v")
            .env("TD_SH_A", "1")
            .env("az", "1")
            .env("a\u{e9}", "2")
            // A leading `_` starts a name and an interior digit continues one --
            // the two halves of `is_name`/`is_in_name` that nothing else here
            // exercises, and each of which a wrong scan drops silently.
            .env("_ok", "1")
            .env("a1", "2")
            .output()?;
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    };
    // Byte for byte what ash prints for this environment, PWD dropped because it
    // is the test's own directory. Asserted as a SEQUENCE, since the ordering is
    // half the rule: ash sorts the RAW names and truncates when it prints, so
    // `az` comes out before the line `a\u{e9}` renders as -- the visible names are
    // not in order, and an implementation that sorted what it printed would be
    // wrong in exactly this shape.
    let listing = run("export -p")?;
    let lines: Vec<&str> = listing.lines().filter(|l| !l.starts_with("export PWD=")).collect();
    assert_eq!(
        lines,
        [
            // A leading digit is not a name START, so the prefix is empty and the
            // whole entry is a bare `export`; a non-ASCII first byte is the same.
            "export ",
            "export PATH='/usr/bin'",
            // Seeded by the shell itself, and exported -- so this listing is
            // now byte-for-byte ash's, which it was not while SHLVL was absent.
            "export SHLVL='1'",
            "export TD_SH_A='1'",
            // A quote closes the run and re-opens it around a `"`-quoted one.
            "export TD_SH_Q='it'\"'\"'s'",
            "export _ok='1'",
            "export a1='2'",
            "export az='1'",
            "export a",
            "export test",
        ],
        "{listing}"
    );
    // So the whole listing is something the shell can read back: the value with
    // the quote in it survives the round trip. It is not IDEMPOTENT -- the bare
    // `export` line lists again when evaluated -- but ash does that too, and the
    // point of `endofname` is that the text parses at all.
    let round_trip = run("eval \"$(export -p)\"; echo \"[$TD_SH_Q]\"")?;
    assert!(round_trip.ends_with("[it's]\n"), "{round_trip}");
    Ok(())
}

/// `umask` is td-sh's first builtin backed by a raw syscall, and its symbolic
/// form is busybox's `bb_parse_mode`: clauses act on the PERMITTED bits, left to
/// right, each seeing what the last one left. Every value here was measured on
/// busybox 1.37.0 ash.
#[test]
fn umask_is_ashs_including_the_symbolic_form() -> Result<(), Box<dyn std::error::Error>> {
    let shell = PathBuf::from(env!("CARGO_BIN_EXE_td-sh"));
    let run = |src: &str| -> Result<(String, i32), Box<dyn std::error::Error>> {
        let out = std::process::Command::new(&shell)
            .arg("-c")
            .arg(src)
            .env_clear()
            .output()?;
        Ok((
            String::from_utf8_lossy(&out.stdout).into_owned(),
            out.status.code().unwrap_or(-1),
        ))
    };
    // Reading prints four octal digits; setting takes numeric or symbolic.
    assert_eq!(run("umask 077; umask")?, ("0077\n".to_string(), 0));
    assert_eq!(run("umask 22; umask")?, ("0022\n".to_string(), 0));
    assert_eq!(run("umask 0777; umask")?, ("0777\n".to_string(), 0));
    // `-S` reports what a new file WOULD get, not the mask.
    assert_eq!(run("umask 022; umask -S")?, ("u=rwx,g=rx,o=rx\n".to_string(), 0));
    assert_eq!(run("umask 777; umask -S")?, ("u=,g=,o=\n".to_string(), 0));
    // Clauses are sequential and see the running value...
    assert_eq!(run("umask 0; umask u=rwx,u-w; umask")?, ("0200\n".to_string(), 0));
    assert_eq!(run("umask 0; umask u=rwx,g=rx,o=; umask")?, ("0027\n".to_string(), 0));
    // ...a permcopy reads that running value too...
    assert_eq!(run("umask 022; umask g=u; umask")?, ("0002\n".to_string(), 0));
    // ...`X` is execute only where execute is already permitted...
    assert_eq!(run("umask 0; umask u=X; umask")?, ("0600\n".to_string(), 0));
    assert_eq!(run("umask 777; umask u=X; umask")?, ("0777\n".to_string(), 0));
    // ...and a bare `who` means "all EXCEPT what the mask already covers", which
    // is why the same clause does different things from different masks.
    assert_eq!(run("umask 0; umask =r; umask")?, ("0333\n".to_string(), 0));
    assert_eq!(run("umask 077; umask =r; umask")?, ("0377\n".to_string(), 0));
    // `=` CLEARS BEFORE THE PERMS ARE READ, so `X` and a permcopy in the same
    // clause see the cleared value. Reading them first is the plausible order
    // and it is wrong in both directions: `a=X` would be 0666 and `u=u` 0022.
    assert_eq!(run("umask 0; umask a=X; umask")?, ("0777\n".to_string(), 0));
    assert_eq!(run("umask 022; umask u=u; umask")?, ("0722\n".to_string(), 0));
    assert_eq!(run("umask 027; umask ugo=rX; umask")?, ("0333\n".to_string(), 0));
    // A bare-`who` `=` clears all nine bits, NOT just the ones outside the
    // mask. Only visible once an earlier clause has pushed the running value
    // outside that set, which is why it needs a two-clause case.
    assert_eq!(
        run("umask 077; umask u=rwx,g+rwx,=r; umask")?,
        ("0377\n".to_string(), 0)
    );
    // One clause holds a LIST of actions, each seeing what the last left.
    assert_eq!(run("umask 0; umask u+r-w; umask")?, ("0200\n".to_string(), 0));
    // `s` and `t` are ordinary perms that happen to name bits outside the nine.
    // So neither is universally legal or universally inert: what decides is
    // whether the RESULT outgrew 0777. `o=rwxs` keeps no setuid bit and is
    // fine; `a+t` keeps sticky and so is an illegal mode.
    assert_eq!(run("umask 0; umask u=t; umask")?, ("0700\n".to_string(), 0));
    assert_eq!(run("umask 022; umask o=rwxs; umask")?, ("0020\n".to_string(), 0));
    assert_eq!(run("umask 022; umask u-s,g-w; umask")?, ("0022\n".to_string(), 0));
    // A permcopy consumes ONE character, so anything after it must be another
    // action -- `u=gr` is an error, not a silently truncated `u=g`. Truncating
    // is the dangerous shape: a typo would set a DIFFERENT mask, not fail.
    assert_eq!(run("umask 022; umask u=gr; echo st=$?; umask")?,
        ("st=2\n0022\n".to_string(), 0));
    // Options bundle, `--` ends them, and a lone `-` is an operand. `-Sp` is
    // the case that proves EVERY character is checked: `-SS` alone passes
    // whether the loop reads one flag or all of them.
    assert_eq!(run("umask 022; umask -SS")?, ("u=rwx,g=rx,o=rx\n".to_string(), 0));
    assert_eq!(run("umask 022; umask -Sp; echo st=$?")?, ("st=2\n".to_string(), 0));
    assert_eq!(run("umask 022; umask -SSp; echo st=$?")?, ("st=2\n".to_string(), 0));
    assert_eq!(run("umask 022; umask --")?, ("0022\n".to_string(), 0));
    assert_eq!(run("umask 022; umask -S -- 077; umask")?, ("0077\n".to_string(), 0));
    assert_eq!(run("umask 022; umask -; umask")?, ("0022\n".to_string(), 0));
    // Errors are status 2 and NOT fatal, so the script keeps going -- and the
    // mask is UNCHANGED. Asserting only that nothing printed would not tell a
    // rejected mode from one silently accepted, since neither prints.
    for bad in [
        "8", "abc", "b=rwx", "99999999999", "07777", "+077", "u=s", "g+s", " ", "a+t",
        "=t", "u=gr", "ao-ux", "u", "0778", "0x22", "22x",
    ] {
        let (stdout, code) = run(&format!("umask \"{bad}\"; echo after"))?;
        assert_eq!(code, 0, "`umask {bad}` must not end the script");
        assert_eq!(stdout, "after\n", "`umask {bad}` must not print");
        let (stdout, _) = run(&format!("umask 022; umask \"{bad}\"; umask"))?;
        assert_eq!(stdout, "0022\n", "`umask {bad}` changed the mask");
    }
    // A permcopy reads the RUNNING value, so a clause before it is visible:
    // entry-based copying would answer 0300 here, and 0227 below.
    assert_eq!(run("umask 0; umask u=r,g=u; umask")?, ("0330\n".to_string(), 0));
    assert_eq!(run("umask 077; umask u=rx,g=u; umask")?, ("0227\n".to_string(), 0));
    // An empty operand and extra operands are both accepted silently.
    assert_eq!(run("umask 077; umask \"\"; umask")?, ("0077\n".to_string(), 0));
    assert_eq!(run("umask 077 077; umask")?, ("0077\n".to_string(), 0));
    // An unknown option is an option error, not a mode error.
    assert_eq!(run("umask -p; echo after")?.0, "after\n");

    // A SUBSHELL's mask is its own. ash forks, so nothing a subshell does can
    // reach the parent; td-sh's subshells are in-process clones sharing one
    // kernel mask, so this is the one piece of subshell state that needs an
    // explicit save/restore -- and it is invisible to every assertion above.
    assert_eq!(run("umask 022; (umask 077); umask")?, ("0022\n".to_string(), 0));
    assert_eq!(
        run("umask 022; (umask 077; umask); umask")?,
        ("0077\n0022\n".to_string(), 0)
    );
    // Command substitution, pipeline stages and an async list are subshells too
    // -- one assertion per `fork_shell` call site, since a site that forgot the
    // guard would leak and nothing else here would notice.
    assert_eq!(
        run("umask 022; x=$(umask 077; umask); echo $x; umask")?,
        ("0077\n0022\n".to_string(), 0)
    );
    assert_eq!(run("umask 022; umask 077 | :; umask")?, ("0022\n".to_string(), 0));
    assert_eq!(run("umask 022; : | umask 077; umask")?, ("0022\n".to_string(), 0));
    // The async body prints nothing on purpose: td-sh runs it synchronously
    // (no job control yet), so a body that printed would pin THAT rather than
    // the mask.
    assert_eq!(run("umask 022; (umask 077) & umask")?, ("0022\n".to_string(), 0));
    // ...nesting restores to the right level, not to the outermost...
    assert_eq!(
        run("umask 022; (umask 077; (umask 002; umask); umask); umask")?,
        ("0002\n0077\n0022\n".to_string(), 0)
    );
    // ...and an `exit` out of the subshell still restores, which is why the
    // guard restores on Drop rather than at the end of the body.
    assert_eq!(run("umask 022; (umask 077; exit 3); echo $?; umask")?, ("3\n0022\n".to_string(), 0));
    // A brace group is NOT a subshell, so there the mask does persist.
    assert_eq!(run("umask 022; { umask 077; }; umask")?, ("0077\n".to_string(), 0));
    Ok(())
}

/// `$RANDOM` is ash's one DYNAMIC variable. Every expected value here was
/// measured on busybox 1.37.0 ash: a script that seeds asks for one SPECIFIC
/// sequence, so "produces numbers in range" is not the same answer.
#[test]
fn random_is_ashs_seeded_generator() -> Result<(), Box<dyn std::error::Error>> {
    let shell = PathBuf::from(env!("CARGO_BIN_EXE_td-sh"));
    let run = |src: &str| -> Result<String, Box<dyn std::error::Error>> {
        let out = std::process::Command::new(&shell)
            .arg("-c")
            .arg(src)
            .env_clear()
            .output()?;
        assert_eq!(String::from_utf8_lossy(&out.stderr), "", "{src}");
        assert!(out.status.success(), "{src} exited {:?}", out.status.code());
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    };
    let six = "echo $RANDOM $RANDOM $RANDOM $RANDOM $RANDOM $RANDOM";
    for (seed, want) in [
        ("0", "3240 22231 2355 11491 7008 14858"),
        ("1", "9882 31274 32415 17757 4881 16130"),
        ("42", "20351 9206 20506 13396 18747 8898"),
        ("4294967295", "29350 13153 5018 5161 8973 25390"),
        // `strtoul`, so a numeric prefix counts and a non-number is 0...
        ("5x", "3710 1948 21657 10135 29411 21828"),
        ("5", "3710 1948 21657 10135 29411 21828"),
        ("abc", "3240 22231 2355 11491 7008 14858"),
        ("0x10", "3240 22231 2355 11491 7008 14858"),
        // ...a negative seed is negated as UNSIGNED, so -1 is 4294967295...
        ("-1", "29350 13153 5018 5161 8973 25390"),
        // ...and an absurd one saturates at ULONG_MAX, whose low 32 bits are the
        // same all-ones, which is why it matches -1 rather than 0.
        ("18446744073709551616", "29350 13153 5018 5161 8973 25390"),
    ] {
        assert_eq!(run(&format!("RANDOM={seed}; {six}"))?, format!("{want}\n"), "seed {seed}");
    }
    // Re-seeding restarts the sequence rather than continuing it.
    assert_eq!(run("RANDOM=1; echo $RANDOM; RANDOM=1; echo $RANDOM")?, "9882\n9882\n");
    // ARITHMETIC draws too, and draws once per mention: reading the stored text
    // instead would add the seed to itself.
    assert_eq!(run("RANDOM=1; echo $((RANDOM+RANDOM))")?, "41156\n");
    // A read REWRITES the stored text (ash's `VNOFUNC` write-back), so the value
    // last drawn is what an exported RANDOM carries. Asserted through `export -p`
    // because `echo $RANDOM` twice would pass with NO write-back at all -- the
    // second read simply draws again.
    let listing = run("RANDOM=1; export RANDOM; echo $RANDOM >/dev/null; export -p")?;
    assert!(listing.contains("export RANDOM='9882'"), "{listing}");
    // `set -a` reaches that write-back too: ash's `setvareq` ORs VEXPORT in, so a
    // read under `-a` exports a name that was assigned before the option was set.
    let listing = run("RANDOM=1; set -a; echo $RANDOM >/dev/null; export -p")?;
    assert!(listing.contains("export RANDOM='9882'"), "{listing}");
    // `unset` switches the generator off for good; the name is then ORDINARY,
    // so an assignment reads back literally instead of seeding.
    assert_eq!(run("unset RANDOM; echo \"[$RANDOM]\"")?, "[]\n");
    assert_eq!(run("unset RANDOM; RANDOM=1; echo \"[$RANDOM]\"")?, "[1]\n");
    // A DYNAMIC variable is exempt from the readonly refusal, which ash spells
    // `(flags & (VREADONLY|VDYNAMIC)) == VREADONLY`.
    assert_eq!(run("readonly RANDOM; RANDOM=1; echo $RANDOM")?, "9882\n");
    // A subshell must NOT replay the parent's sequence -- ash clears the
    // generator in the child on purpose -- and must not disturb it either.
    let out = run("RANDOM=1; (echo $RANDOM >/dev/null); echo $RANDOM")?;
    assert_eq!(out, "9882\n", "the subshell must not consume the parent's draw");
    // ...and must not INHERIT it either: an inheriting child would draw the
    // parent's un-consumed 9882, and five of them would draw it five times.
    let kids = run("RANDOM=1; for i in 1 2 3 4 5; do (echo $RANDOM); done")?;
    let kids: Vec<&str> = kids.split_whitespace().collect();
    assert_eq!(kids.len(), 5, "{kids:?}");
    assert!(kids.iter().any(|v| *v != "9882"), "children inherited the parent generator: {kids:?}");
    for v in &kids {
        assert!(v.parse::<u32>().is_ok_and(|n| n <= 32767), "out of range: {v}");
    }

    // An INHERITED `RANDOM` seeds too: ash imports the environment through
    // `setvareq`, which fires the name's func. A shell that merely stored the
    // string would draw an unrelated sequence here.
    for (seed, want) in [("1", "9882 31274"), ("42", "20351 9206"), ("5x", "3710 1948")] {
        let out = std::process::Command::new(&shell)
            .arg("-c")
            .arg("echo $RANDOM $RANDOM")
            .env_clear()
            .env("RANDOM", seed)
            .output()?;
        assert_eq!(String::from_utf8_lossy(&out.stdout), format!("{want}\n"), "env RANDOM={seed}");
    }

    // A PREFIX assignment is undone by restoring the saved text, not by unsetting
    // the name: ash puts the valueless varinit text back, which reseeds with
    // `strtoul("")` = 0. Treating the restore as an unset would retire the
    // generator instead, and every later `$RANDOM` would be empty.
    assert_eq!(run("RANDOM=1 true; echo $RANDOM $RANDOM")?, "3240 22231\n");
    assert_eq!(run("RANDOM=5; RANDOM=1 true; echo $RANDOM")?, "3710\n");

    // `local RANDOM` unsets the name for the call -- ash's `mklocal` reaches
    // `unsetvar` -- so it reads empty and the generator is suspended, NOT
    // retired: the frame's restore puts the flag back with the binding, and the
    // sequence resumes where the seed left it.
    assert_eq!(
        run("f(){ local RANDOM; echo \"[$RANDOM]\"; }; RANDOM=1; f; echo $RANDOM $RANDOM")?,
        "[]\n9882 31274\n"
    );
    // With a VALUE the local assignment seeds like any other, and the outer
    // binding comes back on return.
    assert_eq!(run("f(){ local RANDOM=7; echo $RANDOM; }; RANDOM=1; f; echo $RANDOM")?, "17008\n9882\n");

    // `set -u` must not COST a draw. The nounset check and the expansion are two
    // lookups of the same name, and a dynamic lookup has a side effect -- so a
    // shell that checks by looking up skips every other number under `-u` alone.
    assert_eq!(run("set -u; RANDOM=1; echo $RANDOM $RANDOM")?, "9882 31274\n");
    assert_eq!(run("set -u; RANDOM=1; echo ${#RANDOM}; echo $RANDOM")?, "4\n31274\n");
    assert_eq!(run("set -u; RANDOM=1; echo ${RANDOM#x}; echo $RANDOM")?, "9882\n31274\n");

    // `strtoul`'s range error is ULONG_MAX and is NOT negated afterwards, so an
    // absurd NEGATIVE seed is all-ones like an absurd positive one -- while a
    // magnitude that still fits is negated as usual.
    assert_eq!(run("RANDOM=-18446744073709551616; echo $RANDOM")?, "29350\n");
    assert_eq!(run("RANDOM=-18446744073709551615; echo $RANDOM")?, "9882\n");

    // `readonly` does not block the UNSET, because ash applies the same
    // `(VREADONLY|VDYNAMIC)` exemption there -- but the attribute SURVIVES it, so
    // the next assignment is refused even though the name is no longer dynamic.
    assert_eq!(run("readonly RANDOM; unset RANDOM; echo \"[$RANDOM]\"")?, "[]\n");
    let out = std::process::Command::new(&shell)
        .arg("-c")
        .arg("readonly RANDOM; unset RANDOM; RANDOM=1; echo reached")
        .env_clear()
        .output()?;
    assert!(String::from_utf8_lossy(&out.stderr).contains("RANDOM: is read only"));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "");

    // The untaken side of a `?:` is not evaluated AT ALL, so it must not draw --
    // while the dead side of `&&`/`||` IS evaluated by ash and must still draw.
    // One `live` flag cannot say both.
    assert_eq!(run("RANDOM=1; echo $((1?0:RANDOM)); echo $RANDOM")?, "0\n9882\n");
    assert_eq!(run("RANDOM=1; echo $((0?RANDOM:0)); echo $RANDOM")?, "0\n9882\n");
    assert_eq!(run("RANDOM=1; echo $((0?RANDOM:RANDOM)); echo $RANDOM")?, "9882\n31274\n");
    assert_eq!(run("RANDOM=1; echo $((0&&RANDOM)); echo $RANDOM")?, "0\n31274\n");
    assert_eq!(run("RANDOM=1; echo $((1||RANDOM)); echo $RANDOM")?, "1\n31274\n");

    // `unset` is the one thing that DOES retire it: the name is then genuinely
    // unset, so `set -u` fires on it.
    let out = std::process::Command::new(&shell)
        .arg("-c")
        .arg("unset RANDOM; set -u; echo $RANDOM; echo reached")
        .env_clear()
        .output()?;
    assert!(String::from_utf8_lossy(&out.stderr).contains("RANDOM: parameter not set"));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "");
    Ok(())
}

/// The names ash seeds when the environment carries none. They are ordinary
/// variables a script can reassign; their ABSENCE is what made `set -u` fatal on
/// idioms like `${HOSTNAME%%.*}` that work in every other shell. Spawned rather
/// than run in-process, because the seeding happens in `Shell::new` and the
/// in-process harness deliberately builds a barer shell.
#[test]
fn the_shell_seeds_the_names_ash_seeds() -> Result<(), Box<dyn std::error::Error>> {
    let shell = PathBuf::from(env!("CARGO_BIN_EXE_td-sh"));
    let run = |src: &str, env: &[(&str, &str)]| -> Result<String, Box<dyn std::error::Error>> {
        let mut cmd = std::process::Command::new(&shell);
        cmd.arg("-c").arg(src).env_clear();
        for (k, v) in env {
            cmd.env(k, v);
        }
        let out = cmd.output()?;
        // A spurious diagnostic or a non-zero exit would otherwise pass unseen.
        assert_eq!(String::from_utf8_lossy(&out.stderr), "", "{src}");
        assert_eq!(out.status.code(), Some(0), "{src}");
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    };
    assert_eq!(
        run("echo \"[$PATH][$PS1][$PS2][$PS4]\"", &[])?,
        "[/sbin:/usr/sbin:/bin:/usr/bin][\\w \\$ ][> ][+ ]\n"
    );
    // `/proc` answers these two, so neither needs `getppid(2)` nor
    // `gethostname(2)` -- a new syscall would be an UNSAFE.md amendment. Pinned
    // to the EXACT values: "some digits" would pass for any wrong pid, and
    // "non-empty" for any wrong host.
    assert_eq!(
        run("echo $PPID", &[])?,
        format!("{}\n", std::process::id()),
        "PPID must name this test process"
    );
    // Compared against the kernel's bytes less its terminating newline, not a
    // trim: the newline is procfs framing and any other space is the hostname.
    let kernel = std::fs::read_to_string("/proc/sys/kernel/hostname")?;
    let kernel = kernel.strip_suffix('\n').unwrap_or(&kernel);
    assert_eq!(run("echo \"$HOSTNAME\"", &[])?, format!("{kernel}\n"));
    // An inherited value wins over the default, so these are defaults and not
    // constants the shell imposes.
    assert_eq!(run("echo \"$PS2\"", &[("PS2", "%")])?, "%\n");
    assert_eq!(run("echo \"$HOSTNAME\"", &[("HOSTNAME", "given")])?, "given\n");
    assert_eq!(run("echo \"$PATH\"", &[("PATH", "/given")])?, "/given\n");
    // PPID is the exception: ash sets it UNGUARDED (ash.c:14540), so a stale
    // exported value from a parent is replaced rather than believed. Asserting
    // "inherited wins" for the whole group would have blessed the opposite.
    assert_eq!(
        run("echo $PPID", &[("PPID", "999")])?,
        format!("{}\n", std::process::id()),
        "an inherited PPID must lose"
    );
    // ...and the import's export flag survives that overwrite, as ash's does.
    assert!(run("export -p", &[("PPID", "999")])?
        .lines()
        .any(|l| l == format!("export PPID='{}'", std::process::id())));
    // SHLVL counts nested shells in an UNSIGNED 32-BIT counter, which only shows
    // at the edges: saturating, or staying signed, differs on the last two.
    // `None` is "absent"; `Some("")` is set-and-empty. ash reaches 1 by two
    // different routes (`p ? atoi(p) : 0` against `atoi("")`), so both are pinned.
    for (env, want) in [
        (None, "1"),
        (Some(""), "1"),
        (Some("4"), "5"),
        (Some("zz"), "1"),
        (Some(" 4"), "5"),
        (Some("0"), "1"),
        (Some("-1"), "0"),
        (Some("-3"), "4294967294"),
        (Some("4294967295"), "0"),
        // A leading numeric PREFIX counts; the scan stops at the first
        // non-digit rather than rejecting the whole value.
        (Some("4x"), "5"),
        (Some("1e2"), "2"),
        // Past LONG_MAX the scan saturates, and the low 32 bits of that are
        // -1 -- so an absurd value yields 0, not 1.
        (Some("99999999999999999999"), "0"),
        (Some("-99999999999999999999"), "1"),
    ] {
        let got = match env {
            None => run("echo $SHLVL", &[])?,
            Some(v) => run("echo $SHLVL", &[("SHLVL", v)])?,
        };
        assert_eq!(got, format!("{want}\n"), "SHLVL={env:?}");
    }
    Ok(())
}

/// An environment entry that is not valid UTF-8 used to ABORT the shell before it
/// ran a line, because `std::env::vars` unwraps. Only a real child can carry one
/// in, so this cannot be a unit test.
#[test]
fn a_non_utf8_environment_entry_does_not_abort_the_shell() -> Result<(), Box<dyn std::error::Error>>
{
    use std::os::unix::ffi::OsStrExt;
    let shell = PathBuf::from(env!("CARGO_BIN_EXE_td-sh"));
    let osb = std::ffi::OsStr::from_bytes;
    let run = |src: &str| -> Result<std::process::Output, Box<dyn std::error::Error>> {
        Ok(std::process::Command::new(&shell)
            .arg("-c")
            .arg(src)
            // TWO undecodable names, the first with an undecodable VALUE as well:
            // one entry with a clean value cannot show that the whole pair is
            // replayed, and a single entry cannot show that the SECOND one is.
            .env(osb(b"TD_SH_\xff"), osb(b"N\xfe"))
            .env(osb(b"TD_SH_\xfd"), "M")
            .env("TD_SH_BADVAL", osb(b"x\xfe"))
            .output()?)
    };
    // The shell RUNS. This is the whole point: the abort came before any line of
    // the script, so every invocation died, not just one that named the entry.
    let out = run("echo ok")?;
    assert_eq!((out.status.code(), out.stdout.as_slice()), (Some(0), b"ok\n".as_slice()));

    // A VALUE that does not decode reads back as U+FFFD, which is what `read` and
    // `$( )` already do with one. ash keeps the byte; that difference is the whole
    // shell's, not this import's, and is deliberate here rather than incidental.
    let out = run("printf %s \"$TD_SH_BADVAL\"")?;
    assert_eq!(out.stdout, "x\u{fffd}".as_bytes());

    // A NAME that does not decode is NOT a variable -- nothing can spell it -- but
    // it still reaches a child byte for byte, which is where mangling it would do
    // real damage. `cat` rather than the shell itself, because reading the entry
    // back through the shell is exactly what cannot preserve it.
    let child_env = |src: &str| -> Result<Vec<Vec<u8>>, Box<dyn std::error::Error>> {
        let out = run(src)?;
        assert!(out.status.success(), "{src}: {:?}", out.status);
        Ok(out.stdout.split(|&b| b == 0).map(<[u8]>::to_vec).collect())
    };
    let entries = child_env("exec cat /proc/self/environ")?;
    assert!(entries.contains(&b"TD_SH_\xff=N\xfe".to_vec()), "{entries:?}");
    assert!(entries.contains(&b"TD_SH_\xfd=M".to_vec()), "{entries:?}");
    // The lossy value is what a CHILD gets too. Reading it back through `printf`
    // above cannot show that: an implementation that decoded lossily AND kept the
    // raw bytes aside would satisfy that assertion and still hand the child the
    // original, since the opaque entries are applied after the exported ones.
    assert!(entries.contains(&"TD_SH_BADVAL=x\u{fffd}".as_bytes().to_vec()), "{entries:?}");
    assert!(!entries.contains(&b"TD_SH_BADVAL=x\xfe".to_vec()), "{entries:?}");
    // And nothing EXTRA: an import that both carried the entry and inserted a
    // lossy-named variable for it would pass every `contains` above while sending
    // the child a name ash never sends.
    assert!(
        !entries.iter().any(|e| e.starts_with("TD_SH_\u{fffd}".as_bytes())),
        "{entries:?}"
    );
    // Again from a subshell, which reaches a child through the OTHER spawn site.
    let entries = child_env("( cat /proc/self/environ )")?;
    assert!(entries.contains(&b"TD_SH_\xff=N\xfe".to_vec()), "{entries:?}");
    assert!(entries.contains(&b"TD_SH_\xfd=M".to_vec()), "{entries:?}");
    Ok(())
}

/// A throwaway directory that removes itself. Exclusive create at 0700, not
/// `create_dir_all`, and no pre-emptive remove: the name is predictable, so
/// anything already there must RED the create rather than be adopted.
struct ScratchDir(PathBuf);

impl ScratchDir {
    fn new(tag: &str) -> Result<Self, Box<dyn std::error::Error>> {
        use std::os::unix::fs::DirBuilderExt as _;
        let base = std::env::temp_dir();
        for seq in 0..64u32 {
            let candidate = base.join(format!("td-sh-{tag}-{}-{seq}", std::process::id()));
            match std::fs::DirBuilder::new().mode(0o700).create(&candidate) {
                Ok(()) => return Ok(Self(candidate)),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e.into()),
            }
        }
        Err("no free temp directory name".into())
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Pathname expansion spends the escapes in a LITERAL path component, because
/// those components are paths rather than patterns -- ash's `expmeta` unescapes
/// them before `opendir` (ash.c:7873). Driven against a real directory, since
/// the whole question is what gets opened.
#[test]
fn globbing_spends_the_escapes_in_a_literal_component() -> Result<(), Box<dyn std::error::Error>> {
    let scratch = ScratchDir::new("glob")?;
    let root = &scratch.0;
    std::fs::create_dir(root.join("dir"))?;
    std::fs::create_dir(root.join("dir/sub"))?;
    std::fs::write(root.join("dir/f1"), "")?;
    std::fs::write(root.join("dir/.hid"), "")?;
    std::fs::write(root.join("dir/sub/g1"), "")?;
    // A directory whose name really does contain a backslash, reached only by
    // DOUBLING it -- the case that separates "drop the escape" from "drop every
    // backslash".
    std::fs::create_dir(root.join("od\\dd"))?;
    std::fs::write(root.join("od\\dd/z1"), "")?;
    // Kept out of `dir/` so the rows above keep measuring what they name. A
    // trailing separator asks whether the path RESOLVES to a directory, which a
    // symlink to one does and a symlink to a file does not.
    std::fs::create_dir(root.join("lnk"))?;
    std::fs::create_dir(root.join("lnk/real"))?;
    std::fs::write(root.join("lnk/plain"), "")?;
    std::os::unix::fs::symlink("real", root.join("lnk/ldir"))?;
    std::os::unix::fs::symlink("plain", root.join("lnk/lfile"))?;
    std::os::unix::fs::symlink("nowhere", root.join("lnk/ldangle"))?;

    let shell = PathBuf::from(env!("CARGO_BIN_EXE_td-sh"));
    let run = |src: &str| -> Result<String, Box<dyn std::error::Error>> {
        let out = std::process::Command::new(&shell)
            .arg("-c")
            .arg(src)
            .current_dir(root)
            .output()?;
        assert_eq!(String::from_utf8_lossy(&out.stderr), "", "{src}");
        assert_eq!(out.status.code(), Some(0), "{src}");
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    };
    for (src, want) in [
        // Before the wildcard...
        (r#"x='d\ir/*'; echo $x"#, "dir/f1 dir/sub\n"),
        (r#"x='\dir/*'; echo $x"#, "dir/f1 dir/sub\n"),
        // ...and after it: `expmeta` recurses and unescapes there too.
        (r#"x='*/\f1'; echo $x"#, "dir/f1\n"),
        (r#"x='*/\sub/g1'; echo $x"#, "dir/sub/g1\n"),
        (r#"x='d\ir/su\b/*'; echo $x"#, "dir/sub/g1\n"),
        // A doubled backslash is one literal backslash in the path.
        (r#"x='od\\dd/*'; echo $x"#, "od\\dd/z1\n"),
        // An escaped slash still SEPARATES; the backslash it carried is dropped.
        (r#"x='dir\/*'; echo $x"#, "dir/f1 dir/sub\n"),
        // ash steps over ONE leading backslash before asking about the dot, so
        // an escaped one still reaches dotfiles.
        (r#"x='dir/\.h*'; echo $x"#, "dir/.hid\n"),
        (r#"x='dir/.h*'; echo $x"#, "dir/.hid\n"),
        // With NO metacharacter anywhere, nothing is globbed and nothing is
        // unescaped -- the word is used as written. This is the pair that makes
        // the rule "escapes are spent by globbing", not "escapes are removed".
        (r#"x='d\ir/\f1'; echo $x"#, "d\\ir/\\f1\n"),
        (r#"x='dir/f\*'; echo $x"#, "dir/f\\*\n"),
        // No match, so the field stays exactly as written, escapes included.
        (r#"x='no\such/*'; echo $x"#, "no\\such/*\n"),
        // REPEATED separators are copied through, not normalised: the result is
        // built out of the original text, so an empty component carries its own
        // slash. Collapsing them would rewrite a path the script wrote.
        ("echo dir//f*", "dir//f1\n"),
        ("echo dir///f*", "dir///f1\n"),
        ("echo dir//sub//*", "dir//sub//g1\n"),
        ("echo .//dir/f*", ".//dir/f1\n"),
        // ...but a MATCHED component contributes exactly one, so the slashes
        // after it are the ones the field wrote and no more.
        ("echo *//f1", "dir//f1\n"),
        ("echo d*/f1", "dir/f1\n"),
        // A trailing separator selects DIRECTORIES only, so the two plain files
        // are not candidates and a pattern matching only files finds nothing.
        ("echo dir/*/", "dir/sub/\n"),
        ("echo dir/f*/", "dir/f*/\n"),
        // A symlink to a directory RESOLVES to one and is selected; a symlink to
        // a file, and a dangling one, are not. That is the pair separating "is a
        // directory" from "is not a symlink".
        ("echo lnk/*/", "lnk/ldir/ lnk/real/\n"),
        ("echo lnk/l*/", "lnk/ldir/\n"),
        ("echo dir//*/", "dir//sub/\n"),
    ] {
        assert_eq!(run(src)?, want, "{src}");
    }
    // An ESCAPED leading slash still roots the walk. The field's FIRST character
    // is the backslash, so reading absoluteness off the raw word rather than off
    // the split components looks below the cwd instead.
    let abs = root.to_string_lossy();
    assert_eq!(
        run(&format!(r"x='\{abs}/dir/f*'; echo $x"))?,
        format!("{abs}/dir/f1\n")
    );
    Ok(())
}

/// A throwaway directory holding the shell under test on PATH, named
/// `td_sh_probe`. A real executable rather than a `#!/bin/sh` script, so no gate
/// needs a host interpreter to exist at an absolute path.
struct ProbeDir(ScratchDir);

impl ProbeDir {
    fn new(tag: &str) -> Result<Self, Box<dyn std::error::Error>> {
        // The exclusive claim and the cleanup are `ScratchDir`'s; what is added
        // here is a name on PATH for the shell under test, which the gate then
        // EXECUTES -- so adopting a directory that already existed would be
        // adopting whatever program was in it.
        //
        // A SYMLINK, not a copy: copying opens the probe for WRITING, and a
        // sibling test's fork inherits that descriptor until its own exec, so
        // exec'ing the probe in that window fails ETXTBSY (reported as 126).
        // Linking writes nothing, so the window does not exist.
        let scratch = ScratchDir::new(tag)?;
        let probe = scratch.0.join("td_sh_probe");
        std::os::unix::fs::symlink(PathBuf::from(env!("CARGO_BIN_EXE_td-sh")), &probe)?;
        Ok(Self(scratch))
    }

    fn run(&self, src: &str) -> Result<(i32, String), Box<dyn std::error::Error>> {
        let out = std::process::Command::new(PathBuf::from(env!("CARGO_BIN_EXE_td-sh")))
            .arg("-c")
            .arg(src)
            .output()?;
        // The assertions compare status and stdout, so a failure here otherwise
        // reads as a bare `(126, "")` with the reason discarded -- which is what
        // made the race this helper used to have expensive to identify. cargo
        // shows a passing test's stderr to nobody and a failing one's to whoever
        // is reading, which is the right audience either way.
        if !out.stderr.is_empty() {
            eprint!("{}", String::from_utf8_lossy(&out.stderr));
        }
        Ok((
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).into_owned(),
        ))
    }
}

/// A prefix assignment is applied to the SHELL for the command's duration, not
/// merely handed to the child, so `PATH=dir prog` locates `prog` in `dir`. Only a
/// real child shows it: the builtin route already applied prefixes to shell state,
/// so nothing in-process could tell the two models apart.
#[test]
fn a_prefix_assignment_reaches_the_external_lookup()
-> Result<(), Box<dyn std::error::Error>> {
    let probe = ProbeDir::new("prefix")?;
    let path = probe.0 .0.display();
    let ran = (0, "RAN\n".to_string());
    assert_eq!(probe.run(&format!("PATH={path} td_sh_probe -c 'echo RAN'"))?, ran);
    // ... and the child sees it too, since it is EXPORTED for that run rather than
    // passed alongside.
    assert_eq!(
        probe.run(&format!("PATH={path} td_sh_probe -c 'echo $PATH'"))?,
        (0, format!("{path}\n"))
    );
    // The shell's own binding is restored afterwards -- on the failing path as
    // well, where the lookup never resolved.
    assert_eq!(
        probe.run(&format!("PATH=orig; PATH={path} td_sh_probe -c ':'; echo [$PATH]"))?,
        (0, "[orig]\n".into())
    );
    assert_eq!(
        probe.run("PATH=orig; PATH=zz td_sh_probe -c ':'; echo [$PATH]")?.1,
        "[orig]\n"
    );
    assert_eq!(
        probe.run("x=1 td_sh_no_such_69; echo \"$? [$x]\"")?,
        (0, "127 []\n".into())
    );
    // A name the shell did not have reaches the child EXPORTED, and is gone from
    // the shell -- exported and unset are separate halves and both are asserted.
    assert_eq!(
        probe.run(&format!(
            "PATH={path} x=1 td_sh_probe -c 'echo [$x]'; echo after=[$x]"
        ))?,
        (0, "[1]\nafter=[]\n".into())
    );
    // Applied left to right AS THEY GO, so a later one sees what an earlier one
    // just set -- the other half of "set on the shell" rather than "collected and
    // handed over", and what `ash_test/ash-vars/var_serial.tests` checks.
    assert_eq!(
        probe.run(&format!(
            "PATH={path}; a=a; b=b; c=c; b=$a c=$b td_sh_probe -c 'echo c=$c b=$b'"
        ))?,
        (0, "c=a b=a\n".into())
    );
    // Redirections are applied BEFORE the assignments, so a target that names one
    // of them expands to the value it had before the command -- and this one then
    // fails, skipping the command entirely.
    assert_eq!(
        probe.run(&format!(
            "X=/no/such/dir-td-sh-70/f; PATH={path} X=/dev/null td_sh_probe -c 'echo RAN' >\"$X\"; echo st=$?"
        ))?.1,
        "st=1\n"
    );
    // ... and one that fails to EXPAND leaves the old value standing, which is what
    // an EXIT trap then sees. Both halves of the order are observable.
    assert_eq!(
        probe.run(&format!(
            "trap 'echo trap=[$X]' EXIT; X=old; PATH={path} X=new td_sh_probe -c ':' >${{u:?boom}}"
        ))?.1,
        "trap=[old]\n"
    );
    // The assignment's own expansion runs with the redirections already in force.
    assert_eq!(
        probe.run(&format!(
            "PATH={path} y=$(echo E >&2) td_sh_probe -c ':' 2>/dev/null; echo done"
        ))?,
        (0, "done\n".into())
    );
    // The LAST of a repeated name wins, as it would in a plain assignment.
    assert_eq!(
        probe.run(&format!("PATH={path} v=1 v=2 td_sh_probe -c 'echo [$v]'"))?.1,
        "[2]\n"
    );
    // The export FLAG is restored, both directions: a variable that was not
    // exported does not stay exported, and one that WAS keeps its old value AND
    // its export. A second child is the probe for both -- host-free, where reading
    // `export -p` through a pipe would need a `grep` on the machine.
    assert_eq!(
        probe.run(&format!(
            "PATH={path}; u=1; u=2 td_sh_probe -c ':'; td_sh_probe -c 'echo [$u]'"
        ))?.1,
        "[]\n"
    );
    assert_eq!(
        probe.run(&format!(
            "PATH={path}; export x=orig; x=1 td_sh_probe -c ':'; td_sh_probe -c 'echo [$x]'"
        ))?.1,
        "[orig]\n"
    );
    // A repeated name is rolled back to what it was BEFORE the first of them, not
    // to what the first one set.
    assert_eq!(
        probe.run(&format!("PATH={path} x=1 x=2 td_sh_probe -c ':'; echo \"[$x]\""))?.1,
        "[]\n"
    );
    // An unwind part-way through the list leaves the frame standing for the EXIT
    // trap, so the assignments that DID take are still visible there -- the
    // `defer_vars` branch, which the redirect-word case above cannot reach because
    // nothing has been assigned by then.
    assert_eq!(
        probe.run(&format!(
            "trap 'echo t=[$x]' EXIT; PATH={path} x=1 y=${{u:?boom}} td_sh_probe -c ':'"
        ))?.1,
        "t=[1]\n"
    );
    // A readonly target is fatal before anything runs, as it is for a builtin.
    assert_eq!(
        probe.run(&format!("readonly PATH=zz; PATH={path} td_sh_probe -c ':'"))?.0,
        2
    );
    Ok(())
}

/// `command -p` must move the EXECUTION lookup, not only the query. Only a real
/// child can show it: the assertion needs a program that exists on `PATH` and not
/// on the default utility path, which means creating one.
#[test]
fn command_p_moves_the_execution_lookup() -> Result<(), Box<dyn std::error::Error>> {
    let probe = ProbeDir::new("cmd-p")?;
    let run = |src: &str| probe.run(src);
    let path = probe.0 .0.display();
    // Without `-p` the probe runs; with it the lookup no longer reads PATH, so it
    // is not there to run -- and the QUERY answers the same way, which is the
    // property that makes `command -pv` honest about `command -p`.
    let ran = (0, "RAN\n".to_string());
    let probe_run = "td_sh_probe -c 'echo RAN'";
    assert_eq!(run(&format!("PATH={path} command {probe_run}"))?, ran);
    assert_eq!(run(&format!("PATH={path} command -p {probe_run}"))?.0, 127);
    assert_eq!(
        run(&format!("PATH={path} command -v td_sh_probe"))?.1,
        format!("{path}/td_sh_probe\n")
    );
    assert_eq!(run(&format!("PATH={path} command -pv td_sh_probe"))?.0, 127);
    // `-p` moves the LOOKUP and nothing else, so a child still inherits the PATH
    // the shell has. A slash in the name skips the search entirely, which is the
    // only way to run something under `-p` that is not on the default path.
    let echo_path = format!("{path}/td_sh_probe -c 'echo $PATH'");
    assert_eq!(
        run(&format!("PATH={path} command -p {echo_path}"))?,
        (0, format!("{path}\n"))
    );
    // A `command` wrapper inside another one keeps the outer `-p`, as ash's own
    // loop does -- but a QUERY inside one does not, because that re-parses.
    assert_eq!(run(&format!("PATH={path} command -p command {probe_run}"))?.0, 127);
    assert_eq!(run(&format!("PATH={path} command -p command command {probe_run}"))?.0, 127);
    assert_eq!(
        run(&format!("PATH={path} command -p command -v td_sh_probe"))?.1,
        format!("{path}/td_sh_probe\n")
    );
    assert_eq!(run(&format!("PATH={path} command -- command {probe_run}"))?, ran);
    // `--` ends the level's options; it does not undo the `-p` already read.
    assert_eq!(run(&format!("PATH={path} command -p -- {probe_run}"))?.0, 127);
    // A slash skips the search on BOTH sides, so the query answers under `-p` too
    // -- the half that would otherwise disagree with the execution above.
    assert_eq!(
        run(&format!("PATH={path} command -pv {path}/td_sh_probe"))?,
        (0, format!("{path}/td_sh_probe\n"))
    );
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
    let outcome = run_case(&shell, &spec_helpers_bin(), case, ASH_DASH_CHAIN)?;
    assert!(outcome.passed, "redirect case failed: {:?}", outcome.detail);
    assert!(!Path::new(&marker).exists(), "case leaked {marker} into the cwd — isolation broken");
    Ok(())
}

/// A LOGIN shell reads the profiles, and a non-login one does not.
///
/// This is the mechanism the image's greeter turns on: `td-login` execs the shell
/// with `argv[0]` set to `-sh`, and `/etc/profile` is what prints the greeter
/// marker, parks a boot-fail target and exports `PS1`/`XDG_RUNTIME_DIR`. A shell
/// that ignores the convention starts a session with none of it, and nothing
/// about that failure mentions profiles — the boot simply never says it reached
/// the greeter.
#[test]
fn a_login_shell_reads_the_profiles() -> Result<(), Box<dyn std::error::Error>> {
    let home = profile_home("profile")?;
    let run = |arg0: &str, extra: &[&str]| login_run(&home, arg0, extra, "echo got=$FROM_PROFILE");

    // Every one of these reads the HOST's `/etc/profile` too, and a test cannot
    // mount over it, so what it contributes is MEASURED rather than assumed: the
    // same login with no user profile is the control, and the assertion is what
    // OUR file adds on top of it.
    let control = login_run(&home, "-sh", &[], ":")?.0;
    assert!(
        !control.contains("PROFILE-RAN"),
        "the host's /etc/profile prints this test's marker: {control:?}"
    );
    std::fs::write(home.join(".profile"), "export FROM_PROFILE=yes\necho PROFILE-RAN\n")?;

    // The Bourne convention: a leading `-` on argv[0] IS the login flag, and it
    // is the only channel `login` has for saying so.
    let (out, _) = run("-sh", &[])?;
    assert_eq!(
        out,
        format!("{control}PROFILE-RAN\ngot=yes\n"),
        "a `-`-prefixed argv[0] did not read the profile (or the host's \
         /etc/profile ended the login before ours was reached)"
    );
    // ...and asking outright does the same thing.
    assert_eq!(run("td-sh", &["-l"])?.0, format!("{control}PROFILE-RAN\ngot=yes\n"));
    // A plain shell does NOT read them -- neither ours nor the host's, which is
    // why this one can still be exact: a profile run per subshell would re-export
    // the operator's environment under every command a script starts.
    assert_eq!(run("td-sh", &[])?.0, "got=\n");
    let _ = std::fs::remove_dir_all(&home);
    Ok(())
}

/// A profile is sourced INTO the session it is setting up, not into a shell that
/// merely precedes it. Both halves of that are load-bearing and neither is
/// visible from inside the profile's own output alone.
///
/// The invocation state first: `$0`, `$@` and `$-` are what the session will see
/// by the time the profile runs, so `set --` in one is not overwritten a moment
/// later and `case $- in *i*)` -- the guard nearly every distributed
/// `/etc/profile` opens with -- answers for the right shell. The interactive `i`
/// needs a terminal to observe and this crate can open none without `unsafe`, so
/// the same ordering is pinned through `s`, which a stdin-script login sets on
/// exactly the same line.
///
/// Then `exit`: it ends the LOGIN, with its status, and the EXIT trap runs when
/// the session ends rather than when the profile does. td's own `/etc/profile`
/// closes its autotest branch with `exit 0` to power the VM off; a shell that
/// swallowed it would sit at a prompt on ttyS0 until the boot timed out.
#[test]
fn a_profile_is_sourced_into_the_session_it_sets_up() -> Result<(), Box<dyn std::error::Error>> {
    let home = profile_home("sourced")?;

    std::fs::write(home.join(".profile"), "echo state=[$0][$*][$-]\n")?;
    let (out, _) = login_run(&home, "td-sh", &["-l"], "echo body")?;
    assert!(
        out.contains("state=[myname][a b][]"),
        "the operands were not bound before the profile: {out:?}"
    );
    // The same ordering, one line further on: `-s` is set for a stdin script by
    // the branch that also decides `interactive`, so `s` here IS the `i` above.
    let mut cmd = std::process::Command::new(PathBuf::from(env!("CARGO_BIN_EXE_td-sh")));
    cmd.args(["-l"])
        .env_clear()
        .env("HOME", &home)
        .env("PATH", "/nonexistent")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped());
    let mut child = cmd.spawn()?;
    if let Some(mut w) = child.stdin.take() {
        std::io::Write::write_all(&mut w, b"echo body\n")?;
    }
    let out = child.wait_with_output()?;
    // Only the `$-` half is asserted here: `$0` for a shell with no name operand
    // is `td-sh` where dash and ash report `argv[0]`, which is a pre-existing gap
    // this commit neither creates nor closes, and pinning it would pin the gap.
    // The `$-` above is dash's answer rather than busybox ash's, which also
    // carries `c` for a `-c` shell; dash's `optletters` has no `c` at all.
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("][s]"),
        "a stdin login did not have `s` in `$-` by profile time: {out:?}"
    );

    // `exit` ends the login there, with its own status, and nothing the shell was
    // invoked to run happens after it.
    std::fs::write(home.join(".profile"), "trap 'echo BYE' EXIT\nexit 7\necho NOT-REACHED\n")?;
    let (out, code) = login_run(&home, "-sh", &[], "echo NOT-REACHED-EITHER")?;
    assert_eq!(code, Some(7), "an `exit` in a profile did not end the login");
    assert!(!out.contains("NOT-REACHED"), "the profile ran on past its exit: {out:?}");
    assert!(out.ends_with("BYE\n"), "the EXIT trap did not run on the way out: {out:?}");

    // A profile whose function ABORTS mid-way leaves nothing of itself behind.
    // `Sig::Abort` unwinds without undoing the bindings it passed, so recovering
    // means undoing them here as an interactive prompt does, or a `local` from a
    // function that died is the session's from then on. `-i`, because that is the
    // shell that RECOVERS -- a non-interactive one ends on the same error, which
    // the sibling test pins -- and it is the shell td-login starts.
    std::fs::write(
        home.join(".profile"),
        "f() { local FROM_PROFILE=leaked; : ${missing:?}; }\nf\n",
    )?;
    let (out, code) = login_run(&home, "-sh", &["-i"], "echo got=[$FROM_PROFILE] st=$?")?;
    assert_eq!(code, Some(0), "the interactive login did not recover: {out:?}");
    assert!(
        out.ends_with("got=[] st=2\n"),
        "an aborted profile function left its `local` or the wrong `$?`: {out:?}"
    );

    // ...and without one, the trap a profile installs waits for the SESSION to
    // end, which is the difference between sourcing a profile and running it.
    std::fs::write(home.join(".profile"), "trap 'echo BYE' EXIT\n")?;
    let (out, code) = login_run(&home, "-sh", &[], "echo body")?;
    assert_eq!(code, Some(0));
    assert!(out.ends_with("body\nBYE\n"), "the EXIT trap fired early: {out:?}");
    let _ = std::fs::remove_dir_all(&home);
    Ok(())
}

/// The shell stops listening to the terminal's interrupt and quit characters for
/// exactly as long as a foreground command runs, and the command does not
/// inherit that.
///
/// Every claim here is read out of `/proc` BY THE CHILD, which is the only
/// vantage point that can see both processes at the moment it matters: its own
/// ignore mask, and the mask of the shell that is at that instant blocked
/// waiting for it. That is deliberate — a Ctrl-C would need a terminal, and this
/// crate can open none without `unsafe`, so the dispositions the keystroke would
/// meet are what is asserted instead. The probe is written with builtins alone
/// (`read`, `case`, a redirection), so it needs nothing on `PATH`.
#[test]
fn the_shell_stops_listening_to_the_terminal_while_a_child_runs(
) -> Result<(), Box<dyn std::error::Error>> {
    let shell = PathBuf::from(env!("CARGO_BIN_EXE_td-sh"));
    let sh = shell.display().to_string();
    let run = |program: &str| -> Result<Vec<u64>, Box<dyn std::error::Error>> {
        let out = std::process::Command::new(&shell).args(["-c", program]).output()?;
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        let mut masks = Vec::new();
        for line in text.lines() {
            let hex = line.split('=').nth(1).ok_or_else(|| format!("probe said {text:?}"))?;
            masks.push(u64::from_str_radix(hex.trim(), 16)?);
        }
        Ok(masks)
    };
    let read_mask = |who: &str, path: &str| {
        format!("while read k v; do case $k in SigIgn:) echo {who}=$v;; esac; done < {path}")
    };
    // The probe reads a byte from stdin BEFORE looking at either mask, and that
    // is a happens-before rather than a delay: the shell creates the thread that
    // writes it only after `InterruptibleChild::hold` has returned, so a probe
    // that got the byte cannot have read the parent's mask too early. Without it
    // the child races the guard and the test flakes on a loaded machine.
    let probe = format!(
        "read _go; {}; {}",
        read_mask("child", "/proc/self/status"),
        read_mask("shell", "/proc/$PPID/status")
    );
    // A here-document is what makes the shell buffer stdin for the child at all.
    let feed = "<<EOF\ngo\nEOF\n";
    // SIGINT is bit 1 and SIGQUIT bit 2. What a td-sh already ignores before its
    // own `main` (Rust's runtime does SIGPIPE) is the BASELINE, measured rather
    // than assumed, and every assertion below is a delta on it.
    let base = *run(&read_mask("idle", "/proc/self/status"))?
        .first()
        .ok_or("no idle mask")?;
    assert_eq!(base & 0b110, 0, "a fresh shell already ignores an interrupt: {base:#x}");
    let held = base | 0b110;

    // An ordinary external command: the shell is deaf to both for as long as it
    // waits, and the child is not -- which is the whole trick, since a child that
    // inherited the ignore would be a command Ctrl-C could not end.
    assert_eq!(run(&format!(r#""{sh}" -c '{probe}' {feed}"#))?, vec![base, held]);

    // The same while the shell is draining a CAPTURED stderr rather than sitting
    // in `wait`: for `x=$(cmd 2>&1)` that read is where the command is spent, so
    // a guard taken beside the wait would arrive after it was already over.
    assert_eq!(
        run(&format!(r#"x=$("{sh}" -c '{probe}' 2>&1 {feed}); echo "$x""#))?,
        vec![base, held]
    );

    // `trap '' INT` is the OPERATOR's and reaches the children, on the second
    // external command as much as the first -- what a guard that restored
    // `SIG_DFL` over it would have destroyed after the first.
    assert_eq!(
        run(&format!(
            r#"trap '' INT; "{sh}" -c '{probe}' {feed}"{sh}" -c '{probe}' {feed}"#
        ))?,
        vec![base | 0b10, held, base | 0b10, held]
    );

    // ...and with no child running the shell is listening again: the guard is a
    // loan, not a mode.
    assert_eq!(run(&read_mask("idle", "/proc/self/status"))?, vec![base]);
    Ok(())
}

/// A stage that never set a umask does not undo a SIBLING's.
///
/// `umask(2)` is per-process and stages are threads of one, so a subshell's
/// capture-and-restore -- invisible when subshells ran one at a time -- became a
/// stage reaching into a sibling's lifetime the moment it happened to exit. The
/// direction is what makes it worth a test: the file comes out MORE permissive
/// than the stage that created it asked for, and nothing about the file says so.
///
/// What this does NOT claim is per-stage isolation. Two stages that both set a
/// mask still share one, which is the divergence from a forking shell that
/// UNSAFE.md records; this is only the case where a stage that asked for
/// nothing overrode one that did.
#[test]
fn a_stage_that_set_no_umask_restores_none() -> Result<(), Box<dyn std::error::Error>> {
    let shell = PathBuf::from(env!("CARGO_BIN_EXE_td-sh"));
    let dir = ScratchDir::new("umask-stage")?;
    let mode = |name: &str| -> Result<u32, Box<dyn std::error::Error>> {
        use std::os::unix::fs::PermissionsExt as _;
        Ok(std::fs::metadata(dir.0.join(name))?.permissions().mode() & 0o777)
    };
    let run = |program: &str| -> Result<(), Box<dyn std::error::Error>> {
        let out = std::process::Command::new(&shell)
            .args(["-c", program])
            .current_dir(&dir.0)
            .output()?;
        assert!(out.status.success(), "{program}: {out:?}");
        Ok(())
    };
    // The sibling exits while the setter is still running, which is the whole
    // shape: its restore lands between the `umask` and the file it guards.
    run("umask 022; { umask 077; sleep 0.3; : > a; } | sleep 0.05")?;
    assert_eq!(mode("a")?, 0o600, "a sibling's exit undid the mask the stage set");
    // The ordinary subshell restore is untouched by that, in both directions.
    run("umask 022; ( umask 077; : > /dev/null ); : > b")?;
    assert_eq!(mode("b")?, 0o644, "a subshell's mask escaped it");
    run("umask 077; ( umask 022; : > /dev/null ); : > c")?;
    assert_eq!(mode("c")?, 0o600, "a subshell's looser mask escaped it");
    run("umask 022; ( : > /dev/null ); : > d")?;
    assert_eq!(mode("d")?, 0o644);
    Ok(())
}

/// A pipeline stage's `trap ''` reaches ITS children and no sibling's.
///
/// Stages are threads of one process, so the kernel disposition is a single cell
/// they share. A stage that INSTALLED its ignore would hand it to whatever a
/// sibling spawned at the same moment, and would leave the parent holding it
/// after the pipeline ended -- so a stage records the ignore and the SPAWN
/// installs the spawning stage's own intent for as long as it takes to create
/// the child. Read from `/proc` by the child, as the test above is, because
/// nothing else can see the disposition at the instant it is copied.
#[test]
fn a_stages_trap_reaches_its_own_children_only(
) -> Result<(), Box<dyn std::error::Error>> {
    let shell = PathBuf::from(env!("CARGO_BIN_EXE_td-sh"));
    let sh = shell.display().to_string();
    let run = |program: &str| -> Result<Vec<u64>, Box<dyn std::error::Error>> {
        let out = std::process::Command::new(&shell).args(["-c", program]).output()?;
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        let mut masks = Vec::new();
        for line in text.lines() {
            let hex = line.split('=').nth(1).ok_or_else(|| format!("probe said {text:?}"))?;
            masks.push(u64::from_str_radix(hex.trim(), 16)?);
        }
        Ok(masks)
    };
    let probe = "while read k v; do case $k in SigIgn:) echo child=$v;; esac; done \
                 < /proc/self/status";
    // Everything below is a delta on what a fresh td-sh already ignores (Rust's
    // runtime does SIGPIPE), measured rather than assumed.
    let base = *run(&format!(r#""{sh}" -c '{probe}'"#))?.first().ok_or("no base mask")?;
    // A consumer written with builtins alone, so the pipeline needs nothing on
    // `PATH` beyond the shell under test.
    let sink = "while read l; do echo \"$l\"; done";

    // SIGTERM is bit 15, and is deliberately NOT one of the two the interrupt
    // guard moves -- so it is the case that a fix bounded to those would miss.
    let term = 1u64 << 14;
    assert_eq!(
        run(&format!(r#"{{ trap '' TERM; "{sh}" -c '{probe}'; }} | {sink}"#))?,
        vec![base | term],
        "a stage's own child did not inherit its ignore"
    );
    // The same stage's ignore must NOT reach a SIBLING's child. `echo go` is a
    // happens-before and the `sleep` after it keeps stage one INSIDE the trap
    // for the whole of the sibling's spawn -- without both, a stage that
    // installed its ignore globally would have restored it again before the
    // sibling got there, and the case would pass for the wrong reason.
    assert_eq!(
        run(&format!(
            r#"{{ trap '' TERM; echo go; sleep 1; }} | {{ read _go; "{sh}" -c '{probe}'; }}"#
        ))?,
        vec![base],
        "a sibling stage's child inherited an ignore nobody asked it for"
    );
    // ...and the parent is not left holding it after the pipeline ends. Two
    // stages have to touch the SAME signal for that, which is the shape the
    // corruption needs: each stage restores what it SAW when it changed one, so
    // stage two captures stage one's ignore and puts it back after both are
    // gone. SIGTERM rather than SIGINT because a spawn rewrites the two signals
    // the guard moves, which would hide the leak from the probe that reads it.
    assert_eq!(
        run(&format!(
            r#"{{ trap '' TERM; sleep 0.3; }} | {{ sleep 0.1; trap - TERM; sleep 0.4; }}; \
               "{sh}" -c '{probe}'"#
        ))?,
        vec![base],
        "the pipeline left its ignore installed in the parent"
    );

    // A stage that CLEARS an ignore its parent installed is the case the trap
    // table alone cannot answer: the table no longer mentions the signal, but
    // the process is still holding it, because clearing it in a stage does not
    // reach the kernel. Both spellings of clearing it, since a non-empty action
    // wants `SIG_DFL` in the child exactly as `trap -` does.
    for stage in ["trap - TERM", "trap 'echo hi' TERM"] {
        assert_eq!(
            run(&format!(
                r#"trap '' TERM; {{ {stage}; "{sh}" -c '{probe}'; }} | {sink}"#
            ))?,
            vec![base],
            "a stage that cleared its parent's ignore still handed it to a child"
        );
    }
    // ...and the parent's ignore is still inherited by a stage that leaves it
    // alone, which is the half that must NOT change.
    assert_eq!(
        run(&format!(r#"trap '' TERM; {{ "{sh}" -c '{probe}'; }} | {sink}"#))?,
        vec![base | term],
        "a stage lost an ignore it never touched"
    );

    // SIGCHLD is the one the table may record and the spawn must NEVER install:
    // ignoring it is POSIX's request that children be AUTO-REAPED, which costs
    // the very status this is about to wait for. dash and bash both keep it.
    let status = |program: &str| -> Result<String, Box<dyn std::error::Error>> {
        let out = std::process::Command::new(&shell).args(["-c", program]).output()?;
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    };
    assert_eq!(status(&format!(r#"trap '' CHLD; "{sh}" -c 'exit 3'; echo st=$?"#))?, "st=3");
    assert_eq!(
        status(&format!(
            r#"{{ trap '' CHLD; "{sh}" -c 'exit 3'; echo st=$?; }} | {sink}"#
        ))?,
        "st=3"
    );
    Ok(())
}

/// An interrupt is not swallowed at an in-process clone boundary.
///
/// td-sh's subshells, pipeline stages and command substitutions are CLONES in
/// one process, and each rightly confines an `exit` or a fatal error to itself:
/// a forked one would only ever have ended that process. An interrupt is the
/// opposite -- the terminal signals the whole foreground process group, so a
/// forked shell at every level would have died of it -- and confining it left
/// `x=$(sleep 100); echo after` printing `after`, which is the loop nothing can
/// stop wearing a different hat.
///
/// Driven from a child that signals ITSELF, because the terminal's own Ctrl-C
/// needs a terminal this crate cannot open without `unsafe`. That works because
/// td-sh installs no SIGINT handler and so cannot be told directly that it was
/// interrupted: it INFERS one from a foreground child dying of SIGINT, and the
/// inference cannot tell a group signal from a child signalled alone. bash can,
/// because it has a handler to be told with; the precise version of this arrives
/// with the handler amendment or with job control, and until then this is the
/// behaviour and this pins it.
#[test]
fn an_interrupt_is_not_confined_to_a_clone() -> Result<(), Box<dyn std::error::Error>> {
    let shell = PathBuf::from(env!("CARGO_BIN_EXE_td-sh"));
    // POSIX puts a shell at this path and `kill` is one of its builtins. If the
    // host's cannot produce a signal-killed child there is nothing to observe,
    // and a green test would be saying otherwise.
    let probe = std::process::Command::new("/bin/sh").args(["-c", "kill -INT $$"]).output();
    assert!(
        matches!(&probe, Ok(o) if o.status.code().is_none()),
        "/bin/sh -c 'kill -INT $$' did not die of a signal ({probe:?}) - this test needs \
         a child it can have killed, and has none. On a td image that is expected rather \
         than a broken host: `/bin/sh` is td-sh, whose interrupt guard swallows a SIGINT \
         a child sends to the shell itself."
    );
    let dies = r#"/bin/sh -c 'kill -INT $$'"#;
    for boundary in [
        format!("{dies}; echo AFTER"),
        format!("x=$({dies}); echo AFTER"),
        format!("({dies}); echo AFTER"),
        format!("{dies} | :; echo AFTER"),
        format!("for i in 1 2; do {dies}; done; echo AFTER"),
    ] {
        let out = std::process::Command::new(&shell).args(["-c", &boundary]).output()?;
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            "",
            "the interrupt was swallowed by `{boundary}`"
        );
        assert_eq!(
            out.status.code(),
            Some(130),
            "`{boundary}` did not report POSIX's 128 + SIGINT"
        );
    }
    // `&` is the one boundary that does NOT carry it out, and it is the only
    // one where that is right: a background job dying of a signal is not the
    // shell dying of one, which is what bash reports here too (`AFTER`, 0).
    // While `&` ran the list SYNCHRONOUSLY the opposite was true, because the
    // job was the shell's own foreground work -- confining the interrupt then
    // left `while :; do cmd & done` unkillable, since the guard the job took
    // covered the loop as well. A job is `concurrent` now and takes no guard,
    // so the loop stays killable without the propagation --
    // `a_shell_with_a_background_job_can_still_be_interrupted` is what holds
    // that.
    let out = std::process::Command::new(&shell)
        .args(["-c", &format!("{dies} & echo AFTER")])
        .output()?;
    assert_eq!(String::from_utf8_lossy(&out.stdout), "AFTER\n");
    assert_eq!(out.status.code(), Some(0));
    // An interrupt is on its way OUT of the shell, so it leaves a dying
    // function's frame standing for the EXIT trap rather than popping it --
    // the same rule `exit` and a fatal error follow. Without that the trap
    // reads the caller's `v`, and one that says `local` at all fails outright
    // with "not in a function".
    let out = std::process::Command::new(&shell)
        .args([
            "-c",
            &format!("trap 'echo T=$v' EXIT; f() {{ local v=in; {dies}; }}; v=out; f"),
        ])
        .output()?;
    assert_eq!(String::from_utf8_lossy(&out.stdout), "T=in\n");
    assert_eq!(out.status.code(), Some(130));

    // ...but `trap '' INT` is the operator saying the shell is not to be
    // interrupted, so nothing is inferred from a child that dies anyway.
    let out = std::process::Command::new(&shell)
        .args(["-c", &format!("trap '' INT; {dies}; echo AFTER")])
        .output()?;
    assert_eq!(String::from_utf8_lossy(&out.stdout), "AFTER\n");
    assert_eq!(out.status.code(), Some(0));

    // A child that dies of SIGINT is only the shell's own interrupt if the shell
    // would have DIED of one. Under `trap '' INT` it would not -- and the child
    // has to RESET the signal to die of it at all, since it inherits the ignore,
    // which is why the case above cannot reach this arm and this one can. Drop
    // the disposition half of that test and the shell aborts here instead.
    let out = std::process::Command::new(&shell)
        .args(["-c", "trap '' INT; /bin/sh -c 'trap - INT; kill -INT $$'; echo AFTER"])
        .output()?;
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "AFTER\n",
        "a child's death by SIGINT ended a shell that had asked to be uninterruptible"
    );
    assert_eq!(out.status.code(), Some(0));

    // And the ignore is the ENCLOSING shell's to hold, which a clone can move
    // out from under: `trap - INT` inside one resets the disposition for the
    // whole process, since these are in-process clones. A forked shell would
    // have had the SUBSHELL die while the parent's ignore stood, so the
    // question has to be re-asked once the clone's restore has run. Asking
    // inside it ends a shell that asked to be uninterruptible; all three
    // boundaries answered that way before this, and `bash --posix` prints
    // AFTER for each.
    for boundary in [
        format!("(trap - INT; {dies}); echo AFTER"),
        format!("x=$(trap - INT; {dies}); echo AFTER"),
        format!("(trap - INT; {dies}) | :; echo AFTER"),
    ] {
        let out = std::process::Command::new(&shell)
            .args(["-c", &format!("trap '' INT; {boundary}")])
            .output()?;
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            "AFTER\n",
            "`{boundary}` let a clone's own interrupt past the enclosing ignore"
        );
        assert_eq!(out.status.code(), Some(0), "`{boundary}` did not survive");
    }
    Ok(())
}

/// A private `$HOME` for a profile test, empty and its own.
fn profile_home(tag: &str) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let home = std::env::temp_dir().join(format!("td-sh-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home)?;
    Ok(home)
}

/// One login shell over that `$HOME`, as `(stdout, exit status)`. `$0` and the
/// positional parameters are always the same three words, so a profile can report
/// what it was handed.
fn login_run(
    home: &std::path::Path,
    arg0: &str,
    extra: &[&str],
    cmd: &str,
) -> Result<(String, Option<i32>), Box<dyn std::error::Error>> {
    use std::os::unix::process::CommandExt;
    let mut c = std::process::Command::new(PathBuf::from(env!("CARGO_BIN_EXE_td-sh")));
    c.arg0(arg0)
        .args(extra)
        .args(["-c", cmd, "myname", "a", "b"])
        .env_clear()
        .env("HOME", home)
        .env("PATH", "/nonexistent");
    let out = c.output()?;
    Ok((String::from_utf8_lossy(&out.stdout).into_owned(), out.status.code()))
}

/// A DIAGNOSTIC writer ends on a broken pipe, as the stdout writer already did.
///
/// The stdout rule landed with concurrent pipelines; these two are the half it
/// did not reach, because `set -x` and the shell's own error lines discard the
/// write result. With `head` gone every write fails and nothing looked, so both
/// spun forever at constant memory — which no `ulimit` catches, hence `timeout`
/// and an asserted STATUS: a shape that regresses here produces the right first
/// line and then never exits, and only the status says so.
///
/// The first line is what a caller sees either way, so it is compared to bash's
/// on the shape whose text both shells share.
#[test]
fn a_broken_pipe_ends_a_diagnostic_writer() -> Result<(), Box<dyn std::error::Error>> {
    let shell = PathBuf::from(env!("CARGO_BIN_EXE_td-sh"));
    for (program, want) in [
        // xtrace: nothing here writes to stdout at all, so the pipe is fed by
        // the trace alone.
        ("set -x; while :; do :; done 2>&1 | head -1", Some("+ :\n")),
        // ...and the shell's own error path, whose text is its own.
        ("while :; do cd /nope; done 2>&1 | head -1", None),
    ] {
        let out = std::process::Command::new("/bin/sh")
            .args([
                "-c",
                &format!("exec timeout 20 {} -c {}", shell.display(), shell_quote(program)),
            ])
            .output()?;
        assert_eq!(
            out.status.code(),
            Some(0),
            "`{program}` did not end on the broken pipe (124 is the timeout killing a spin)"
        );
        if let Some(want) = want {
            assert_eq!(String::from_utf8_lossy(&out.stdout), want, "`{program}`");
        } else {
            assert_eq!(String::from_utf8_lossy(&out.stdout).lines().count(), 1);
        }
    }
    // `set -n` must not be able to strand a pending broken pipe. It returns
    // from `run_command` before anything runs, so a flag left pending when
    // noexec came on could never be looked at again — and `set +n` cannot turn
    // it off. Checking AFTER the command is what closes that: the flag is seen
    // by the very command that set it. Found by review against a first cut that
    // only checked on entry.
    let out = std::process::Command::new("/bin/sh")
        .args([
            "-c",
            &format!(
                "exec timeout 20 {} -c {}",
                shell.display(),
                shell_quote("set -x; while sleep .2; do set -n; done 2>&1 | head -1")
            ),
        ])
        .output()?;
    assert_eq!(out.status.code(), Some(0), "`set -n` hid a pending broken pipe");

    // ...and AFTER the command, or the LAST one's diagnostic is never noticed:
    // the script ends and reports its own status where bash reports 141.
    //
    // The reader is closed HERE rather than by a `head` in a pipeline, so the
    // status read back is the SHELL's own and not the last stage's. The `sleep`
    // puts the write after the close either way.
    for program in ["set -x; sleep 0.3; :", "echo one >&2; sleep 0.3; cd /nope"] {
        use std::io::Read as _;
        let mut child = std::process::Command::new(&shell)
            .args(["-c", program])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()?;
        let mut err = child.stderr.take().ok_or("no stderr pipe")?;
        // ONE line first, then close — `head -1` exactly. Closing straight away
        // would break the FIRST write instead, which an entry-only check
        // already catches on the next command; it is the LAST write that has
        // nothing after it to notice.
        let mut byte = [0u8; 1];
        while let Ok(1) = err.read(&mut byte) {
            if byte[0] == b'\n' {
                break;
            }
        }
        drop(err);
        assert_eq!(
            child.wait()?.code(),
            Some(141),
            "`{program}` did not report 128+SIGPIPE for its last command"
        );
    }

    // An EXIT trap still RUNS, which is the whole difference between this and a
    // signal death and what the stdout rule beside it already gave. The first
    // cut let the trap's own first command meet the pending flag and return
    // having done nothing, losing every cleanup trap on this path.
    let dir = ScratchDir::new("epipe-trap")?;
    for (program, tag) in [
        ("while :; do cd /nope; done", "diag"),
        ("while :; do echo x; done", "stdout"),
    ] {
        // TWO commands, because the action must run to COMPLETION. With the
        // check placed after each command a one-command trap finishes before
        // the pending flag is ever looked at, so only the second marker tells
        // whether the trap was cut short.
        let first = dir.0.join(format!("{tag}-1"));
        let second = dir.0.join(format!("{tag}-2"));
        let out = std::process::Command::new("/bin/sh")
            .args([
                "-c",
                &format!(
                    "exec timeout 20 {} -c {} 2>&1 | head -1 >/dev/null",
                    shell.display(),
                    shell_quote(&format!(
                        "trap 'echo T > {}; echo T > {}' EXIT; {program}",
                        first.display(),
                        second.display()
                    ))
                ),
            ])
            .output()?;
        assert!(out.status.success(), "`{program}` wedged: {out:?}");
        assert!(
            first.exists(),
            "the EXIT trap did not run at all on the {tag} broken-pipe path"
        );
        assert!(
            second.exists(),
            "the EXIT trap was cut short on the {tag} broken-pipe path"
        );
    }

    // A diagnostic write that FAILS for any other reason still just reports.
    // `2>&-` is the case that matters and the one a broader rule would break:
    // the write itself fails with EBADF, not EPIPE, and both shells carry on.
    // Without this the difference between "ends on a broken pipe" and "ends on
    // any failed diagnostic" is untested, since an ordinary `cd /nope` writes
    // to a stderr that works.
    for program in [
        "cd /nope 2>&-; echo alive",
        "set -x; cd /nope 2>&-; echo alive",
        "echo hi >&-; echo rc=$?; cd /nope; echo alive",
    ] {
        let out = std::process::Command::new(&shell).args(["-c", program]).output()?;
        assert!(
            String::from_utf8_lossy(&out.stdout).contains("alive"),
            "`{program}` ended the shell on a diagnostic that was not a broken pipe: {out:?}"
        );
    }
    Ok(())
}

/// A stage the OS refuses a thread for is not something `!` can turn into
/// success.
///
/// The status was `!`'s to negate, so `! cmd | cmd` reported 0 when the shell
/// had in fact run none of it — the one answer a caller must never get from a
/// pipeline that did not happen. It unwinds now, as a pipe this could not create
/// already did.
///
/// Driven by `RLIMIT_NPROC` rather than a memory limit, deliberately: threads
/// count against it, so the refusal is EAGAIN at the exact call being tested
/// with nothing else starved. A limit generous enough to let the threads through
/// leaves nothing to assert, so the test says so rather than passing quietly.
#[test]
fn an_unstarted_stage_is_not_negatable() -> Result<(), Box<dyn std::error::Error>> {
    let shell = PathBuf::from(env!("CARGO_BIN_EXE_td-sh"));
    let ten = ": | : | : | : | : | : | : | : | : | :";
    let mut exercised = false;
    for program in [ten.to_string(), format!("! {ten}")] {
        let out = std::process::Command::new("/bin/sh")
            .args([
                "-c",
                &format!(
                    "ulimit -u 40 2>/dev/null; exec {} -c {}",
                    shell.display(),
                    shell_quote(&program)
                ),
            ])
            .output()?;
        let err = String::from_utf8_lossy(&out.stderr).into_owned();
        if !err.contains("pipeline stage did not start") {
            continue;
        }
        exercised = true;
        assert_eq!(
            out.status.code(),
            Some(126),
            "`{program}` answered {:?} for a pipeline that never ran: {err}",
            out.status.code()
        );
    }
    if !exercised {
        eprintln!(
            "an_unstarted_stage_is_not_negatable: RLIMIT_NPROC left every thread \
             startable, so nothing was exercised"
        );
    }
    Ok(())
}

/// A pipeline STREAMS: the consumer runs while the producer is still producing.
///
/// Both shapes here hung or died before, and neither is exotic — they are what
/// an operator types. With the producer buffered whole, `head` never ran until
/// `cat /dev/zero` had finished, which is never, so the shell grew a `Vec` until
/// the machine gave out; and `yes | head` is the same thing with a cheaper
/// producer. A pipe is a fixed kernel buffer, so the first is bounded and the
/// second ends when `head` closes its end.
///
/// Bounded with `ulimit -v`, so a regression is a FAILURE rather than a machine
/// that stops responding — the whole point is that unbounded growth is gone, and
/// a test that proves it by exhausting memory would be the bug it is testing for.
#[test]
fn a_pipeline_streams_rather_than_buffering_its_producer() -> Result<(), Box<dyn std::error::Error>>
{
    let shell = PathBuf::from(env!("CARGO_BIN_EXE_td-sh"));
    for (program, want) in [
        // The unbounded producer: `head` closing its end is what ends `cat`.
        ("cat /dev/zero | head -c 1 | wc -c", "1\n"),
        // The endless one: nothing here ever reaches EOF on its own.
        ("yes striped | head -n 2", "striped\nstriped\n"),
        // A BUILTIN producer, which no pipe descriptor ends: the write returns
        // EPIPE and that has to end the stage, or the loop spins forever.
        ("while :; do echo x; done | head -n 1", "x\n"),
    ] {
        let out = std::process::Command::new("/bin/sh")
            .args([
                "-c",
                // 256 MiB is far under what buffering `/dev/zero` needs and far
                // over what streaming needs.
                // `timeout` as well as `ulimit`: the memory shapes regress into
                // an abort, but the EPIPE one regresses into a SPIN at constant
                // memory, which no limit catches. Without this the test wedges
                // the gate instead of failing it — the same "a regression is a
                // failure, not a machine that stops responding" the limit is
                // for.
                &format!(
                    "ulimit -v 262144; exec timeout 20 {} -c {}",
                    shell.display(),
                    shell_quote(program)
                ),
            ])
            .output()?;
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            want,
            "`{program}` did not stream"
        );
        // The STATUS too, or the timeout above reads as success: a producer that
        // emits its first line and then spins on EPIPE gives `head` exactly the
        // expected bytes, and only the exit status says the shell had to be
        // killed to end it.
        assert_eq!(
            out.status.code(),
            Some(0),
            "`{program}` produced the right bytes but did not end on its own"
        );
    }
    Ok(())
}

/// A pipeline can still be stopped from the keyboard.
///
/// Stages are THREADS of one process, so a disposition one of them installs
/// covers all of them. The interrupt guard taken by the stage running `cat`
/// therefore covered the stage running `while :; do :; done` — and that one can
/// only ever be stopped by a signal, so the pipeline became unkillable and the
/// operator had no in-band way out of it. Strictly worse than the death the
/// guard replaces, and on the image this is the login shell.
///
/// A pipeline is not covered by the guard for that reason, which is what
/// `bash --posix` does on every one of these: it dies of the signal, as this
/// shell did before pipelines streamed.
#[test]
fn a_pipeline_can_still_be_interrupted() -> Result<(), Box<dyn std::error::Error>> {
    use std::os::unix::process::CommandExt;
    let shell = PathBuf::from(env!("CARGO_BIN_EXE_td-sh"));
    for program in [
        // Nothing here ends on its own, and no pipe closing can stop the
        // producer: only the signal can.
        "while :; do :; done | cat",
        "while :; do :; done | sleep 30",
        "while :; do echo x > /dev/null; done | cat",
    ] {
        let mut child = std::process::Command::new(&shell)
            .args(["-c", program])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            // Its own process group, so the signal below reaches the whole
            // pipeline the way a terminal's would rather than one process of it.
            .process_group(0)
            .spawn()?;
        std::thread::sleep(std::time::Duration::from_millis(300));
        // Through the shell's own `kill`, so this needs no external binary.
        let _ = std::process::Command::new("/bin/sh")
            .args(["-c", &format!("kill -INT -{}", child.id())])
            .status();
        let start = std::time::Instant::now();
        loop {
            match child.try_wait()? {
                Some(status) => {
                    assert!(
                        status.code() != Some(0),
                        "`{program}` reported success after being interrupted"
                    );
                    break;
                }
                None if start.elapsed() > std::time::Duration::from_secs(5) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!(
                        "`{program}` ignored the interrupt entirely - the operator \
                         has no way to stop it from the terminal"
                    );
                }
                None => std::thread::sleep(std::time::Duration::from_millis(20)),
            }
        }
    }
    Ok(())
}

/// A shell with a background job running can still be stopped from the keyboard.
///
/// The same argument as the pipeline above, and the same flag: a job is a THREAD
/// of this process, so the interrupt guard it took while waiting on its own
/// external command would cover the shell's foreground work too. `sleep 30 &`
/// followed by a spin is that exactly — the job holds the guard for thirty
/// seconds while the loop, which only a signal can stop, runs unprotected inside
/// it. Marking a job `concurrent` is what keeps this killable, and this is the
/// assertion that says so: with that one line removed the shell survived the
/// interrupt 6 times out of 6, and with it the shell died 6 of 6.
///
/// It is the background half of what `a_pipeline_can_still_be_interrupted`
/// pins, and it exists because `&` running its list synchronously used to make
/// the question moot.
#[test]
fn a_shell_with_a_background_job_can_still_be_interrupted(
) -> Result<(), Box<dyn std::error::Error>> {
    use std::os::unix::process::CommandExt;
    let shell = PathBuf::from(env!("CARGO_BIN_EXE_td-sh"));
    // The job has to be one that really BLOCKS, or it exits at once, no guard is
    // ever taken, and the shell dies for want of one — passing while measuring
    // nothing. That is the `/bin/sleep` mistake this test's own history records,
    // so `sleep` is checked to exist and to sleep before it is relied on.
    let probe = std::time::Instant::now();
    let slept = std::process::Command::new("/bin/sh").args(["-c", "sleep 0.4"]).status();
    assert!(
        matches!(&slept, Ok(s) if s.success()) && probe.elapsed().as_millis() >= 300,
        "`sleep` does not exist or does not sleep ({slept:?}) - this test needs a job \
         that blocks long enough to hold the guard, and has none"
    );
    // Nothing here ends on its own: the job outlasts the test by far and the
    // loop is builtins only, so the signal is the only way out.
    let program = "sleep 30 & while :; do :; done";
    let mut child = std::process::Command::new(&shell)
        .args(["-c", program])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        // Its own group, so the signal arrives the way a terminal's would.
        .process_group(0)
        .spawn()?;
    std::thread::sleep(std::time::Duration::from_millis(300));
    // Asserted, not discarded: a `kill` that never ran would leave the shell
    // running and this test blaming it for ignoring a signal nobody sent.
    let signalled = std::process::Command::new("/bin/sh")
        .args(["-c", &format!("kill -INT -{}", child.id())])
        .status();
    assert!(
        matches!(&signalled, Ok(s) if s.success()),
        "could not signal the process group ({signalled:?}) - nothing was measured"
    );
    let start = std::time::Instant::now();
    loop {
        match child.try_wait()? {
            Some(status) => {
                assert!(status.code() != Some(0), "reported success after being interrupted");
                return Ok(());
            }
            None if start.elapsed() > std::time::Duration::from_secs(5) => {
                let _ = child.kill();
                let _ = child.wait();
                panic!(
                    "`{program}` ignored the interrupt - a background job's guard \
                     covered the shell, and the operator has no way to stop it"
                );
            }
            None => std::thread::sleep(std::time::Duration::from_millis(20)),
        }
    }
}

/// A loop that starts jobs faster than it waits for them must not take the
/// process out.
///
/// A finished thread still holds its stack mappings until something JOINS it, so
/// a table that only ever grew ran the process out of `vm.max_map_count` after a
/// few tens of thousands of iterations. The failure is not the graceful one at
/// the spawn site: `clone(2)` succeeds and the new thread panics inside std's
/// own bootstrap setting up its guard page, and this crate aborts on a panic —
/// so `while :; do cmd & done`, the shape the interrupt work above is all about,
/// died of SIGABRT in about a second. Reaping the ones already finished is what
/// bounds it; measured aborting at 40000 before and clean after.
#[test]
fn a_loop_of_jobs_does_not_exhaust_the_process() -> Result<(), Box<dyn std::error::Error>> {
    let shell = PathBuf::from(env!("CARGO_BIN_EXE_td-sh"));
    let out = std::process::Command::new(&shell)
        .args(["-c", "i=0; while [ $i -lt 40000 ]; do true & i=$((i+1)); done; echo ok"])
        .output()?;
    assert_eq!(String::from_utf8_lossy(&out.stdout), "ok\n");
    // Not merely non-zero: an abort has no code at all, and saying which it was
    // is the difference between "the shell reported an error" and "the shell
    // died". `stderr` carries the panic when it is the latter.
    assert_eq!(
        out.status.code(),
        Some(0),
        "40000 background jobs did not survive: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(())
}

/// The three places `commandname` comes from that no in-process harness can
/// reach: a script named on the command line, `-c` WITH a name operand, and
/// bare `-c` with neither. ash sets it at ash.c:14630 for the first, jumps
/// into that same line for the second (`goto setarg0`), and leaves it null
/// for the third -- which is why an identical failure says where it happened
/// in two of the three and not in the last.
#[test]
fn where_a_diagnostic_says_it_happened_depends_on_how_the_shell_was_started(
) -> Result<(), Box<dyn std::error::Error>> {
    let shell = PathBuf::from(env!("CARGO_BIN_EXE_td-sh"));
    let dir = std::env::temp_dir().join(format!("td-sh-diagwhere-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    let script = dir.join("s.sh");
    std::fs::write(&script, ":\n:\nnosuchcmd_xyz\n")?;
    let sp = script.to_string_lossy().into_owned();

    let err = |args: &[&str]| -> Result<String, Box<dyn std::error::Error>> {
        let out = std::process::Command::new(&shell).args(args).output()?;
        Ok(String::from_utf8_lossy(&out.stderr).into_owned())
    };
    // A script FILE: `commandname` equals `$0`, so the component is dropped
    // and the line survives alone.
    assert_eq!(err(&[&sp])?, format!("{sp}: line 3: nosuchcmd_xyz: not found\n"));
    // Bare `-c`: neither. The command entered no builtin, and nothing else set
    // a name.
    assert_eq!(err(&["-c", "nosuchcmd_xyz"])?, format!("{}: nosuchcmd_xyz: not found\n", shell.display()));
    // `-c` WITH a name: the name is `$0` and `commandname` both, so this is the
    // script shape again -- a line, no component.
    assert_eq!(err(&["-c", "nosuchcmd_xyz", "myname"])?, "myname: line 1: nosuchcmd_xyz: not found\n");
    // And a builtin under bare `-c` DOES name itself, since it sets the name
    // for as long as it runs.
    assert_eq!(
        err(&["-c", "cd /nope/x"])?,
        format!("{}: cd: line 1: can't cd to /nope/x: No such file or directory\n", shell.display())
    );
    // The component is dropped whenever it repeats `$0`, whatever `$0` is:
    // named `cd`, the builtin's own component disappears.
    assert_eq!(
        err(&["-c", "cd /nope/x", "cd"])?,
        "cd: line 1: can't cd to /nope/x: No such file or directory\n"
    );
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

/// The OTHER half of `ash_vmsg`'s gate, `pf_fd > 0`, which decides nothing
/// unless the shell is interactive -- `!iflag` already carries every
/// non-interactive shape, so a suite that never runs `-i` can delete this
/// field and stay green. `-i` needs no terminal, which is what makes it
/// testable at all.
///
/// A subshell is here for a different reason: ash's `forkchild` copies the
/// address space rather than clearing it, so a stage inherits the name and the
/// file-ness both, and a diagnostic inside `( … )` says where it is.
#[test]
fn an_interactive_shell_reports_a_line_only_for_input_it_opened(
) -> Result<(), Box<dyn std::error::Error>> {
    let shell = PathBuf::from(env!("CARGO_BIN_EXE_td-sh"));
    let dir = std::env::temp_dir().join(format!("td-sh-diagfd-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    let script = dir.join("t.sh");
    std::fs::write(&script, ":\n:\ncd /nope/x\n")?;
    let sp = script.to_string_lossy().into_owned();
    let want = "can't cd to /nope/x: No such file or directory\n";

    let out = std::process::Command::new(&shell).args(["-i", &sp]).output()?;
    assert_eq!(String::from_utf8_lossy(&out.stderr), format!("{sp}: cd: line 3: {want}"));
    // The SAME source on stdin: a real file, but one the shell did not open,
    // which is ash's `pf_fd` being 0 rather than positive.
    let out = std::process::Command::new(&shell)
        .arg("-i")
        .stdin(std::fs::File::open(&script)?)
        .output()?;
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        format!("{}: cd: {want}", shell.display())
    );
    // A file the shell opens ITSELF while interactive is back in the first
    // group, so sourcing one reports the line again.
    let out = std::process::Command::new(&shell)
        .args(["-i", "-c", &format!(". {sp}")])
        .output()?;
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        format!("{}: cd: line 3: {want}", shell.display())
    );

    // A subshell inherits both fields; without that this loses the line. Run
    // INTERACTIVELY, or `!iflag` alone carries it and only the name is tested.
    std::fs::write(&script, ":\n:\n( nosuchcmd_zz )\n")?;
    let out = std::process::Command::new(&shell).args(["-i", &sp]).output()?;
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        format!("{sp}: line 3: nosuchcmd_zz: not found\n")
    );

    // A profile is the third file the shell opens itself, and the only one
    // that is ALWAYS read by an interactive shell -- so it is the only place
    // this field can be observed without `-i` being passed for the test's own
    // sake.
    std::fs::write(dir.join(".profile"), ":\n:\ncd /nope/x\n")?;
    let out = std::process::Command::new(&shell)
        .args(["-i", "-l", "-c", ":"])
        .env("HOME", &dir)
        .output()?;
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains(&format!("cd: line 3: {want}")), "{err:?}");

    // And the builtin name is given back on the ERROR path too: `getopts`
    // raises here, and the command after it must not inherit its name.
    let out = std::process::Command::new(&shell)
        .args(["-c", "set -- -a; getopts ab 1bad; nosuchcmd_zz"])
        .output()?;
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.ends_with(&format!("{}: nosuchcmd_zz: not found\n", shell.display())),
        "{err:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

/// `main` evaluates on a thread of its own, and only the BINARY can show it:
/// every in-process test goes through a capturing harness, which spawns one
/// itself, so reverting `main.rs` alone leaves the whole suite green and the
/// half the `ulimit -s` argument is about uncovered.
///
/// The kernel is what to ask. A shell on the process's own stack has one
/// thread; one that spawned its own has two, `main` being blocked in the join.
#[test]
fn the_shell_evaluates_on_a_thread_it_made_itself() -> Result<(), Box<dyn std::error::Error>> {
    let shell = PathBuf::from(env!("CARGO_BIN_EXE_td-sh"));
    let out = std::process::Command::new(&shell)
        .args([
            "-c",
            "while read k v; do if [ \"$k\" = Threads: ]; then echo $v; fi; \
             done < /proc/self/status",
        ])
        .output()?;
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "2",
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(())
}

/// `exec` is the one exit a join cannot follow, so it has to join first.
///
/// `execve` replaces the image and every job thread stops mid-instruction, which
/// would make "a shell outlives the jobs it started" quietly false for exactly
/// one path — and silently, since what is lost is output nobody sees go missing.
/// Driven through the real binary rather than the in-process harness: with a
/// captured stdout `exec` runs the command in place instead of replacing
/// anything, so the harness never reaches the code this is about.
#[test]
fn exec_does_not_abandon_a_running_job() -> Result<(), Box<dyn std::error::Error>> {
    let shell = PathBuf::from(env!("CARGO_BIN_EXE_td-sh"));
    let out = std::process::Command::new(&shell)
        .args([
            "-c",
            "{ i=0; while [ $i -lt 30000 ]; do i=$((i+1)); done; echo late; } & \
             exec /bin/sh -c 'echo replaced'",
        ])
        .output()?;
    assert_eq!(String::from_utf8_lossy(&out.stdout), "late\nreplaced\n");
    Ok(())
}

/// A job blocked on a descriptor only the SHELL still holds must not deadlock
/// the join that ends the shell.
///
/// Dropping the job table joins every job, so the order the shell's own fields
/// go in is load-bearing: with the table released first, the shell sat holding
/// its whole descriptor table while it waited, and `cat fifo & exec 3>fifo`
/// never came back — the job waiting for an EOF only the shell could give, the
/// shell waiting for the job. Unbounded, so not the documented "a job costs the
/// shell its own exit time" trade.
#[test]
fn a_job_waiting_on_the_shells_own_descriptor_does_not_hang() -> Result<(), Box<dyn std::error::Error>>
{
    let shell = PathBuf::from(env!("CARGO_BIN_EXE_td-sh"));
    let dir = std::env::temp_dir().join(format!("td-sh-jobfifo-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    let fifo = dir.join("f");
    let path = fifo.to_string_lossy().into_owned();
    // std cannot make a FIFO, and this test is nothing without one: a probe
    // that quietly failed would leave `cat` reporting "not found" at once and
    // the deadlock untested.
    let made = std::process::Command::new("/bin/sh")
        .args(["-c", &format!("mkfifo {}", shell_quote(&path))])
        .status();
    assert!(
        matches!(&made, Ok(s) if s.success()) && fifo.exists(),
        "could not create a FIFO at {path} ({made:?}) - this test needs one"
    );
    let q = shell_quote(&path);
    // Both shapes, because two different mechanisms answer them: the shell's own
    // is the ORDER of its fields (`fds` before `jobs`, so the table goes first),
    // and a subshell's is `Subshell::drop` releasing before it joins. Each was
    // measured hanging for good with its own fix removed and the other in place.
    for program in [
        format!("cat {q} & exec 3>{q}; echo done"),
        format!("( cat {q} & exec 3>{q} ); echo after"),
    ] {
        let mut child = std::process::Command::new(&shell)
            .args(["-c", &program])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()?;
        let start = std::time::Instant::now();
        let outcome = loop {
            match child.try_wait()? {
                Some(status) => break Some(status),
                None if start.elapsed() > std::time::Duration::from_secs(10) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
                None => std::thread::sleep(std::time::Duration::from_millis(20)),
            }
        };
        if outcome.is_none() {
            let _ = std::fs::remove_dir_all(&dir);
            panic!("`{program}` never exited: the shell's own fd 3 held the job open");
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

/// Single-quote `s` for a POSIX shell.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// A profile INTERRUPTED falls where a fatal error in one falls, and by the same
/// rule: ash's top-level handler exits a non-interactive login and recovers an
/// interactive one. bash is the other way — it abandons the rest of the profile
/// and runs the command anyway — so this is a place the two references disagree
/// and the choice has to be made rather than fallen into. It reached the
/// stray-`break` catch-all before, which took bash's side without saying so and
/// left `$?` untouched besides.
#[test]
fn an_interrupted_profile_ends_a_non_interactive_login() -> Result<(), Box<dyn std::error::Error>>
{
    let probe = std::process::Command::new("/bin/sh").args(["-c", "kill -INT $$"]).output();
    assert!(
        matches!(&probe, Ok(o) if o.status.code().is_none()),
        "/bin/sh -c 'kill -INT $$' did not die of a signal - this test cannot run"
    );
    let home = profile_home("profile-interrupt")?;
    std::fs::write(home.join(".profile"), "/bin/sh -c 'kill -INT $$'\necho REST_OF_PROFILE\n")?;

    // Non-interactive: the login ends at 130 and the command never runs.
    let (out, code) = login_run(&home, "-sh", &[], "echo RAN_COMMAND")?;
    assert_eq!(out, "", "an interrupted profile still started the session");
    assert_eq!(code, Some(130), "an interrupted login did not report 128 + SIGINT");

    // Interactive: the prompt is what an operator gets back, so the rest of the
    // profile is abandoned but the session — and the command — survive, with the
    // interrupt's status standing.
    let (out, code) = login_run(&home, "-sh", &["-i"], "echo status=$?")?;
    assert_eq!(out, "status=130\n", "an interactive login did not recover the interrupt");
    assert_eq!(code, Some(0));

    let _ = std::fs::remove_dir_all(&home);
    Ok(())
}

/// A profile that fails, or is absent, does not cost the operator the session.
#[test]
fn a_broken_profile_is_not_a_failed_login() -> Result<(), Box<dyn std::error::Error>> {
    use std::os::unix::process::CommandExt;
    let shell = PathBuf::from(env!("CARGO_BIN_EXE_td-sh"));
    let home = profile_home("badprofile")?;
    let run = |cmd: &str| -> Result<(String, String, bool), Box<dyn std::error::Error>> {
        let out = std::process::Command::new(&shell)
            .arg0("-sh")
            .args(["-c", cmd])
            .env_clear()
            .env("HOME", &home)
            .env("PATH", "/nonexistent")
            .output()?;
        Ok((
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
            out.status.success(),
        ))
    };
    // The host's `/etc/profile` is read here too (see the sibling test), so both
    // streams are measured with no user profile first and asserted as deltas.
    // `:` so the control's own output is empty and what it holds is the host's.
    let (host_out, host_err, _) = run(":")?;

    // A command that is not there, then one that is: the profile keeps going, and
    // neither its failure nor its status reaches the shell.
    std::fs::write(home.join(".profile"), "no_such_command_at_all\nexport LATER=set\n")?;
    let (out, _, ok) = run("echo later=$LATER status=$?")?;
    assert!(ok, "a failing profile ended the login");
    assert_eq!(out, format!("{host_out}later=set status=0\n"));

    // ...and `$?` CARRIES: a profile's last command is the last command, which is
    // what dash and busybox ash both do -- neither touches `exitstatus` between
    // the profiles and what follows. Tidying it here would be a divergence.
    std::fs::write(home.join(".profile"), "false\n")?;
    let (out, _, ok) = run("echo status=$?")?;
    assert!(ok, "a profile ending in a failure ended the login");
    assert_eq!(out, format!("{host_out}status=1\n"));

    // A FATAL error is a different thing from a failing command, and ash draws
    // the line where td-sh now does: its top-level handler exits when
    // `iflag == 0`, so a NON-interactive login over a broken profile never runs
    // what it was invoked for. Measured against busybox ash, which answers rc 2
    // and no stdout for both a `${x:?}` and a syntax error, exactly as this does.
    // The image is unaffected -- td-login starts an INTERACTIVE shell, which
    // recovers in both shells, so a typo in `/etc/profile` still does not cost
    // the operator their session.
    for fatal in ["if\necho NOT-REACHED\n", ": ${missing:?}\necho NOT-REACHED\n"] {
        std::fs::write(home.join(".profile"), fatal)?;
        let (out, err, ok) = run("echo NOT-REACHED-EITHER")?;
        assert!(!ok, "a fatal profile error did not end a non-interactive login");
        assert_eq!(out, host_out, "the login ran on past a fatal profile error: {out:?}");
        // ...and it is reported against the file it is in -- `$0: <path>: ...`,
        // the shape `.` gives a sourced file, because a diagnostic naming no file
        // is one an operator cannot act on. `$0` is this login shell's own `-sh`,
        // which is the argv[0] the run helper above hands it.
        let added = err.strip_prefix(host_err.as_str()).unwrap_or(&err);
        assert!(
            added.starts_with(&format!("-sh: {}/.profile: ", home.display()))
                || added.starts_with("-sh: missing: "),
            "the error did not name the profile: {added:?}"
        );
    }

    // No profile at all is the ordinary case, and adds nothing to either stream:
    // a complaint here would print on every console at every boot.
    std::fs::remove_file(home.join(".profile"))?;
    let (out, err, _) = run("echo ok")?;
    assert_eq!(out, format!("{host_out}ok\n"));
    assert_eq!(err, host_err);
    let _ = std::fs::remove_dir_all(&home);
    Ok(())
}

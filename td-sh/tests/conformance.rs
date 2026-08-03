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
    graded_identity, parse_spec, resolve, run_case, run_dir_classified, summarize,
    Disposition, Expectations, ASH_DASH_CHAIN,
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
        let outcome = run_case(&shell, &case, ASH_DASH_CHAIN)?;
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
        let outcome = run_case(&shell, &case, &["dash"])?;
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
        let outcome = run_case(&shell, &case, ASH_DASH_CHAIN)?;
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
        let outcome = run_case(&shell, &case, ASH_DASH_CHAIN)?;
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
        let outcome = run_case(&shell, &case, ASH_DASH_CHAIN)?;
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
        let outcome = run_case(&shell, &case, ASH_DASH_CHAIN)?;
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
            run_case(&shell, case, &[bad]).is_err(),
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
    // `gethostname(2)` -- a new syscall would be an AGENTS.md amendment. Pinned
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

/// A throwaway directory holding a COPY of the shell under test, named
/// `td_sh_probe`. A copy rather than a `#!/bin/sh` script, so no gate needs a host
/// interpreter to exist at an absolute path.
struct ProbeDir(ScratchDir);

impl ProbeDir {
    fn new(tag: &str) -> Result<Self, Box<dyn std::error::Error>> {
        use std::os::unix::fs::PermissionsExt;
        // The exclusive claim and the cleanup are `ScratchDir`'s; what is added
        // here is a copy of the shell under test, which the gate then EXECUTES --
        // so adopting a directory that already existed would be adopting whatever
        // program was in it.
        let scratch = ScratchDir::new(tag)?;
        let dir = &scratch.0;
        let probe = dir.join("td_sh_probe");
        std::fs::copy(PathBuf::from(env!("CARGO_BIN_EXE_td-sh")), &probe)?;
        std::fs::set_permissions(&probe, std::fs::Permissions::from_mode(0o755))?;
        Ok(Self(scratch))
    }

    fn run(&self, src: &str) -> Result<(i32, String), Box<dyn std::error::Error>> {
        let out = std::process::Command::new(PathBuf::from(env!("CARGO_BIN_EXE_td-sh")))
            .arg("-c")
            .arg(src)
            .output()?;
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
    let outcome = run_case(&shell, case, ASH_DASH_CHAIN)?;
    assert!(outcome.passed, "redirect case failed: {:?}", outcome.detail);
    assert!(!Path::new(&marker).exists(), "case leaked {marker} into the cwd — isolation broken");
    Ok(())
}

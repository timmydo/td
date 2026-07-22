//! `td-builder daily` — the DAILY BACKSTOP runner (was ci/daily-full-suite.sh,
//! deleted in the same change; rust-migration, human 2026-07-06).
//!
//! Main does not block landings on the full `td-builder check`: engine/heavy
//! changes land on the bounded per-change tiers + review, and the full
//! heavy+daily+system suite runs ONCE DAILY on fresh main here, driven by a
//! scheduled agent that heals any regression by forming a fix-or-revert the
//! integrator lands (no auto-land — a human integrates). This subcommand is the
//! mechanical half: run the whole suite on fresh main and write a machine-readable
//! verdict; the agent reads the verdict and does the triage + fix-or-revert.
//!
//! Usage:  td-builder daily [--no-system] [--verdict FILE]
//!
//! Exit is a bitfield over the suites — 1 heavy red, 2 system red; 0 = all
//! green, up to 3. A leg that could not run
//! because the RUNNER is not provisioned for it (the base loop toolchain does not
//! resolve on PATH for heavy/system) does NOT set its bit — the leg's
//! `td-builder check` exits `EXIT_UNPROVISIONED` (69, a stable machine signal —
//! no FATAL-prose grepping), recorded in the verdict as env_error. Setup errors exit
//! 8/9/10, kept out of the bitfield range:
//!   8  - unknown CLI argument
//!   9  - git fetch of origin/main failed (or no td-builder to run the legs)
//!   10 - runner not provisioned for EVERY leg: heavy/system's loop toolchain is
//!        unresolved. No gate ran anywhere, so there is nothing to triage/revert.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::check_loop::{find_in_path, EXIT_UNPROVISIONED};

const HELP: &str = "\
td-builder daily [--no-system] [--verdict FILE]

Run the full td-builder check suite (heavy + daily + system) on
fresh origin/main and write a machine-readable verdict. Exit is a bitfield: 1 heavy
red, 2 system red; 8/9 setup error; 10 runner not provisioned for any
leg. Unprovisioned heavy/system legs do not set a bit.";

const ENV_ERROR_MSG: &str =
    "runner not provisioned: the loop prelude could not resolve the base loop \
     toolchain on PATH — no gate ran, not a code regression";

pub fn cli(args: &[String]) -> std::process::ExitCode {
    std::process::ExitCode::from(run(args).clamp(0, 255) as u8)
}

/// The per-leg outcome the pure verdict function reasons over: each leg's exit
/// code plus whether that code was `EXIT_UNPROVISIONED`. Kept free of IO so the
/// scenario matrix can drive `compute_verdict` directly (the unit test below).
#[derive(Clone, Copy)]
struct LegRc {
    rc: i32,
    unprovisioned: bool,
}

impl LegRc {
    fn from_code(code: i32) -> Self {
        LegRc {
            rc: code,
            unprovisioned: code == EXIT_UNPROVISIONED,
        }
    }
    fn green() -> Self {
        LegRc {
            rc: 0,
            unprovisioned: false,
        }
    }
}

struct VerdictInput {
    run_system: bool,
    heavy: LegRc,
    system: LegRc,
    harness: LegRc,
}

/// What the caller does with the run, decided purely from the leg outcomes.
struct Verdict {
    heavy_state: &'static str,
    system_state: &'static str,
    harness_state: &'static str,
    env_error: bool,
    harness_env_error: bool,
    /// Final process exit code (bitfield 0..=7, or 10 for the all-unprovisioned abort).
    exit_code: i32,
    /// Every leg unprovisioned — nothing ran anywhere, print the abort and stop.
    abort_all_unprovisioned: bool,
    /// A current leg was unprovisioned after another leg ran: a partial, not a
    /// full-suite proof — do NOT record `.td-last-green` or publish.
    partial: bool,
    /// Full suite green: record `.td-last-green` and publish substitutes.
    all_green: bool,
}

/// The verdict/exit contract, as a pure function so the scenario matrix is a unit
/// test (the shell could only verify it by stubbing td-builder). Every leg is
/// symmetric: an unprovisioned current leg (its `td-builder check` exited
/// `EXIT_UNPROVISIONED`) never sets a red bit AND never counts toward a full-suite
/// green — a tier that could not run means this is not a full-suite proof, so the
/// run is PARTIAL and `.td-last-green` is withheld. (`env_error` = heavy
/// unprovisioned, which also skips system.) The retired harness field stays in
/// the verdict for compatibility but no longer contributes to the exit decision.
fn compute_verdict(inp: &VerdictInput) -> Verdict {
    let env_error = inp.heavy.unprovisioned;
    // system is only attempted when heavy is provisioned; if it ran and came back
    // unprovisioned (exit 69), treat it symmetrically — no red bit, PARTIAL.
    let system_unprov = inp.run_system && !env_error && inp.system.unprovisioned;
    let harness_env_error = inp.harness.unprovisioned;

    let heavy_state = if env_error {
        "unprovisioned"
    } else if inp.heavy.rc != 0 {
        "red"
    } else {
        "green"
    };
    let system_state = if !inp.run_system {
        "skipped"
    } else if env_error || system_unprov {
        "unprovisioned"
    } else if inp.system.rc == 0 {
        "green"
    } else {
        "red"
    };
    let harness_state = if harness_env_error {
        "unprovisioned"
    } else if inp.harness.rc == 0 {
        "green"
    } else {
        "red"
    };

    // Heavy is the first current leg. If it is unprovisioned, system is skipped
    // and the retired harness producer no longer exists, so no gate ran anywhere:
    // abort 10, no triage/revert.
    if env_error {
        return Verdict {
            heavy_state,
            system_state,
            harness_state,
            env_error,
            harness_env_error,
            exit_code: 10,
            abort_all_unprovisioned: true,
            partial: false,
            all_green: false,
        };
    }

    // Bitfield over REAL regressions only: an unprovisioned leg never sets its bit.
    let mut rc = 0;
    if !env_error && inp.heavy.rc != 0 {
        rc += 1;
    }
    if inp.run_system && !env_error && !system_unprov && inp.system.rc != 0 {
        rc += 2;
    }

    // Any current leg unprovisioned (heavy/system) means this is not a full-suite
    // proof: PARTIAL, and `.td-last-green` / publish are withheld. all_green requires
    // every attempted leg to have actually run green.
    let any_unprovisioned = env_error || system_unprov;
    let partial = rc == 0 && any_unprovisioned;
    let all_green = rc == 0 && !any_unprovisioned;

    Verdict {
        heavy_state,
        system_state,
        harness_state,
        env_error,
        harness_env_error,
        exit_code: rc,
        abort_all_unprovisioned: false,
        partial,
        all_green,
    }
}

fn run(args: &[String]) -> i32 {
    let mut run_system = true;
    let mut verdict_path = String::from(".td-daily-verdict");
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--no-system" => run_system = false,
            "--verdict" => match it.next() {
                Some(v) => verdict_path = v.clone(),
                None => {
                    eprintln!("td-builder daily: --verdict needs a FILE argument");
                    return 8;
                }
            },
            "-h" | "--help" => {
                println!("{HELP}");
                return 0;
            }
            other => {
                eprintln!("td-builder daily: unknown arg: {other}");
                return 8;
            }
        }
    }

    let root = match std::env::current_dir() {
        Ok(r) if r.join("tests").is_dir() => r,
        Ok(_) => {
            eprintln!("td-builder daily: run from the repo root (tests/ not found)");
            return 8;
        }
        Err(e) => {
            eprintln!("td-builder daily: cannot resolve cwd: {e}");
            return 8;
        }
    };

    // Fresh main.
    if !git(&root, &["fetch", "origin", "main", "-q"]) {
        eprintln!("td-builder daily: fetch of origin/main failed");
        return 9;
    }
    let main = git_capture(&root, &["rev-parse", "--short", "origin/main"]).unwrap_or_default();
    let main = main.trim();

    // The binary that runs the legs: the freshly-built engine (host cargo), else
    // the pre-placed guix-free stage0 (a harness runner ships one). This is a
    // DIFFERENT binary from the daily orchestrator, so the legs are re-execs.
    let tdb = match locate_tdb(&root) {
        Some(p) => p,
        None => {
            eprintln!("td-builder daily: no td-builder (need host cargo or a pre-placed stage0)");
            return 9;
        }
    };

    println!(">> daily backstop: full td-builder check on origin/main ({main})");

    // Heavy: an empty chain cache (#317) selects the cold per-worktree ladder over the
    // shared daemon, and TD_SUBST_FORCE_BUILD forces the from-seed toolchain build +
    // republish so the daily does not fetch its own prior publish and self-starve.
    // Cross-run build-cache reuse is unconditional now, so consecutive dailies reuse this
    // ladder's warm cache; the daily does NOT clear it. A from-stage0 clean-room proof is
    // an explicit `td-recipe-eval clear-store` on this ladder (chain cache empty) before the
    // run — a deliberate operator step, no longer an automatic nightly guarantee. The
    // pin-verified seed store is retained rather than wiped, so a seed-pin change reds
    // `authenticate_seed_db` here until that same `clear-store` (fail-closed, hinted).
    let hlog = leg_log("heavy");
    let heavy_code = run_leg(
        &root,
        &tdb,
        &["check"],
        &[
            ("TD_CHECK_CHAIN_CACHE", ""),
            ("TD_SUBST_FORCE_BUILD", "1"),
            ("TD_BUILD_JOBS", &jobs()),
        ],
        &hlog,
    );
    let heavy = LegRc::from_code(heavy_code);
    let heavy_fail = if heavy.unprovisioned {
        ENV_ERROR_MSG.to_string()
    } else {
        grep_fails(&hlog)
    };
    if heavy.unprovisioned {
        println!(">> daily backstop: {ENV_ERROR_MSG}");
    }
    let _ = std::fs::remove_file(&hlog); // the shell's `trap rm` — don't leak leg logs

    // System: only when provisioned (it needs the same loop toolchain as heavy).
    let mut system = LegRc::green();
    let mut system_fail = String::new();
    if run_system && !heavy.unprovisioned {
        println!(">> daily backstop: td-builder check check-system on origin/main ({main})");
        let slog = leg_log("system");
        let code = run_leg(
            &root,
            &tdb,
            &["check", "check-system"],
            &[("TD_BUILD_JOBS", &jobs())],
            &slog,
        );
        system = LegRc::from_code(code);
        system_fail = grep_fails(&slog);
        let _ = std::fs::remove_file(&slog);
    }

    // The /td/store harness tier was removed with its producer gate; keep the
    // verdict field green for compatibility until the follow-up reintroduces it
    // on the recipe-graph path.
    let harness = LegRc::green();
    let harness_fail = String::new();

    let v = compute_verdict(&VerdictInput {
        run_system,
        heavy,
        system,
        harness,
    });

    let env_error_msg = if v.env_error { ENV_ERROR_MSG } else { "" };
    write_verdict(
        &root,
        &verdict_path,
        main,
        &v,
        env_error_msg,
        heavy.rc,
        &heavy_fail,
        system.rc,
        &system_fail,
        harness.rc,
        &harness_fail,
    );

    if v.abort_all_unprovisioned {
        println!(
            ">> daily backstop: RUNNER NOT PROVISIONED at {main} — no gate could run anywhere \
             (heavy/system: {ENV_ERROR_MSG})"
        );
        println!(
            ">> daily backstop: this is a HOST setup gap, not a code regression — no fix-or-revert \
             PR is warranted. Provision the loop toolchain (heavy/system), then re-run."
        );
        cat(&root, &verdict_path);
        return 10;
    }
    if v.env_error {
        println!(
            ">> daily backstop: heavy/system SKIPPED (runner not provisioned: {ENV_ERROR_MSG}) — \
             no gate ran"
        );
    }

    if v.partial {
        println!(
            ">> daily backstop: PARTIAL at {main} — a tier was unprovisioned this run \
             (heavy={} system={} harness={}); nothing that ran is red, but this is not a \
             full-suite proof — .td-last-green NOT recorded",
            v.heavy_state, v.system_state, v.harness_state
        );
    } else if v.all_green {
        let _ = std::fs::write(root.join(".td-last-green"), format!("{main}\n"));
        println!(">> daily backstop: ALL GREEN at {main} (recorded .td-last-green)");
        publish_substitutes(&root, &tdb);
    } else {
        println!(
            ">> daily backstop: RED (heavy_rc={} system_rc={} harness_rc={}) — agent: triage \
             `git log <last-green>..{main}`, reproduce the failing gate, form a fix-or-revert \
             and record it in issues/open/daily-red.md (the integrator lands it). Suspect-revert \
             helper: ci/revert-suspect.sh --ref <sha>",
            heavy.rc, system.rc, harness.rc
        );
    }
    cat(&root, &verdict_path);
    v.exit_code
}

fn jobs() -> String {
    std::env::var("TD_BUILD_JOBS").unwrap_or_else(|_| "4".to_string())
}

/// git in `root`, inheriting env; true on success.
fn git(root: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .args(args)
        .current_dir(root)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn git_capture(root: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .ok()?;
    if out.status.success() {
        Some(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        None
    }
}

/// The check binary: host cargo builds the fresh engine, else the pre-placed
/// stage0 (its CURRENT placement out of the #309 memo, which the stale-sweep
/// always keeps; glob fallback for a shipped store with no memo).
fn locate_tdb(root: &Path) -> Option<PathBuf> {
    if find_in_path("cargo").is_some() {
        let built = Command::new("cargo")
            .args([
                "build",
                "--release",
                "--quiet",
                "--manifest-path",
                "builder/Cargo.toml",
            ])
            .current_dir(root)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if built {
            let p = root.join("builder/target/release/td-builder");
            if is_exec(&p) {
                return Some(p);
            }
        }
    }
    let base = root.join(".td-build-cache/stage0");
    if let Ok(meta) = std::fs::read_to_string(base.join(".stage0-meta")) {
        if let Some(cb) = meta.lines().nth(1) {
            if let Some(name) = Path::new(cb.trim()).file_name() {
                let p = base.join("store").join(name).join("bin/td-builder");
                if is_exec(&p) {
                    return Some(p);
                }
            }
        }
    }
    // Glob fallback: first store/*/bin/td-builder.
    let store = base.join("store");
    let mut hits: Vec<PathBuf> = std::fs::read_dir(&store)
        .ok()?
        .flatten()
        .map(|e| e.path().join("bin/td-builder"))
        .filter(|p| is_exec(p))
        .collect();
    hits.sort();
    hits.into_iter().next()
}

/// Run one check leg, combined stdout+stderr to `log`, inheriting the parent env
/// plus the given overrides. Returns the child exit code (1 if it could not spawn).
fn run_leg(root: &Path, tdb: &Path, args: &[&str], envs: &[(&str, &str)], log: &Path) -> i32 {
    let f = match std::fs::File::create(log) {
        Ok(f) => f,
        Err(e) => {
            eprintln!(
                "td-builder daily: cannot open leg log {}: {e}",
                log.display()
            );
            return 1;
        }
    };
    let ferr = match f.try_clone() {
        Ok(f) => f,
        Err(e) => {
            eprintln!("td-builder daily: cannot dup leg log: {e}");
            return 1;
        }
    };
    let mut cmd = Command::new(tdb);
    cmd.args(args)
        .current_dir(root)
        .stdout(Stdio::from(f))
        .stderr(Stdio::from(ferr));
    for (k, val) in envs {
        cmd.env(k, val);
    }
    cmd.status().ok().and_then(|s| s.code()).unwrap_or(1)
}

/// The first 5 gate-failure / pre-gate FATAL lines, `;`-joined, for the verdict's
/// *_fail field (names WHICH gate failed — not a provisioned/regression verdict,
/// which is the exit code's job).
fn grep_fails(log: &Path) -> String {
    let text = std::fs::read_to_string(log).unwrap_or_default();
    text.lines()
        .filter(|l| l.starts_with("FAIL") || l.starts_with("td-builder check: FATAL"))
        .take(5)
        .collect::<Vec<_>>()
        .join(";")
}

/// Per-leg combined-output log — lives in the system tempdir (not the tree),
/// keyed by pid so concurrent runs on one box don't clobber each other.
fn leg_log(leg: &str) -> PathBuf {
    std::env::temp_dir().join(format!("td-daily-{}-{}.log", std::process::id(), leg))
}

#[allow(clippy::too_many_arguments)]
fn write_verdict(
    root: &Path,
    path: &str,
    main: &str,
    v: &Verdict,
    env_error_msg: &str,
    heavy_rc: i32,
    heavy_fail: &str,
    system_rc: i32,
    system_fail: &str,
    harness_rc: i32,
    harness_fail: &str,
) {
    let date = Command::new("date")
        .arg("-Is")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    let body = format!(
        "commit={main}\n\
         date={date}\n\
         env_error={}\n\
         env_error_msg={env_error_msg}\n\
         heavy={}\n\
         heavy_rc={heavy_rc}\n\
         heavy_fail={heavy_fail}\n\
         system={}\n\
         system_rc={system_rc}\n\
         system_fail={system_fail}\n\
         harness={}\n\
         harness_rc={harness_rc}\n\
         harness_env_error={}\n\
         harness_fail={harness_fail}\n",
        v.env_error as u8,
        v.heavy_state,
        v.system_state,
        v.harness_state,
        v.harness_env_error as u8,
    );
    if let Err(e) = std::fs::write(root.join(path), body) {
        eprintln!("td-builder daily: cannot write verdict {path}: {e}");
    }
}

fn cat(root: &Path, path: &str) {
    if let Ok(s) = std::fs::read_to_string(root.join(path)) {
        print!("{s}");
    }
}

/// A regular file with an execute bit — the `[ -x ]` guard the deleted shell put
/// on `$TDB`. is_file() alone is not enough: a partial cargo output or a placed
/// store file that lost +x would be picked as the check binary, then every leg's
/// spawn fails with code 1 and scores as a spurious heavy RED instead of the clean
/// "no td-builder" setup exit.
fn is_exec(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// On all-green: sign + publish the from-seed exports this run produced so the
/// per-PR loop FETCHES prebuilt closures instead of rebuilding. Guarded — each
/// block is a clear no-op unless the daily runner supplies TD_SUBST_PRIVKEY + a
/// td-subst binary. (The seed publisher still shells out to its existing helper;
/// porting it is a separate rust-migration slice.)
fn publish_substitutes(root: &Path, tdb: &Path) {
    let store = std::env::var("TD_SUBST_STORE").unwrap_or_else(|_| {
        std::env::var("HOME")
            .map(|h| format!("{h}/.td/subst"))
            .unwrap_or_default()
    });
    let privkey = std::env::var("TD_SUBST_PRIVKEY").unwrap_or_default();
    let sb = std::env::var("TD_SUBST_BIN")
        .ok()
        .or_else(|| find_in_path("td-subst").map(|p| p.display().to_string()))
        .unwrap_or_default();

    publish_export(
        root,
        &store,
        &privkey,
        &sb,
        "toolchain-subst-export",
        "publish-toolchain-subst",
        "the lock-keyed toolchain",
        false,
    );
    publish_export(
        root,
        &store,
        &privkey,
        &sb,
        "x86_64-closure-export",
        "publish-x86_64-closure",
        "the x86_64 toolchain closure",
        true,
    );
    publish_export(
        root,
        &store,
        &privkey,
        &sb,
        "x86_64-native-closure-export",
        "publish-x86_64-native-closure",
        "the native x86_64 toolchain closure",
        false,
    );

    // seed: still shell out to its existing publisher.
    if privkey.is_empty() || sb.is_empty() {
        println!(">> publish-seed-subst: SKIP — TD_SUBST_PRIVKEY / td-subst binary not set");
        return;
    }
    let seed_ok = Command::new("sh")
        .args([
            "tools/publish-seed-subst.sh",
            "tests/td-builder-rust.lock",
            &store,
        ])
        .current_dir(root)
        .env("TD_BUILDER", tdb)
        .env("TD_SUBST_BIN", &sb)
        .env("TD_SUBST_PRIVKEY", &privkey)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !seed_ok {
        println!(">> publish-seed-subst: WARN — seed publish failed; not published");
    }
}

/// One inline sign+copy publish block (gate export dir → signed substitute store).
#[allow(clippy::too_many_arguments)]
fn publish_export(
    root: &Path,
    store: &str,
    privkey: &str,
    sb: &str,
    export: &str,
    label: &str,
    what: &str,
    stash_td_subst: bool,
) {
    let exp = root.join(".td-build-cache").join(export);
    if !has_narinfo(&exp) {
        println!(
            ">> {label}: SKIP — no export at .td-build-cache/{export} (nothing built this run)"
        );
        return;
    }
    if privkey.is_empty() || sb.is_empty() {
        println!(">> {label}: SKIP — TD_SUBST_PRIVKEY / td-subst binary not set");
        return;
    }
    let signed = Command::new(sb)
        .args(["sign", &exp.display().to_string(), privkey])
        .current_dir(root)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !signed {
        println!(">> {label}: WARN — td-subst sign failed; not published");
        return;
    }
    let _ = std::fs::create_dir_all(store);
    let copied = Command::new("cp")
        .args(["-a", &format!("{}/.", exp.display()), &format!("{store}/")])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !copied {
        println!(">> {label}: WARN — copy into {store} failed; not published");
        return;
    }
    if stash_td_subst {
        // Stash the consumer's td-subst into the store so the host prelude exposes it.
        let _ = Command::new("cp")
            .args(["-a", sb, &format!("{store}/td-subst")])
            .status();
    }
    println!(">> {label}: signed + published {what} to {store} (the loop resolver fetches it; trust = tests/td-subst.pub)");
}

fn has_narinfo(dir: &Path) -> bool {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten().any(|e| {
                e.path()
                    .extension()
                    .map(|x| x == "narinfo")
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unprov() -> LegRc {
        LegRc::from_code(EXIT_UNPROVISIONED)
    }
    fn red(code: i32) -> LegRc {
        LegRc::from_code(code)
    }

    // The scenario matrix ci/daily-full-suite.sh could only verify by stubbing
    // td-builder — now a real unit test of the decision function.

    #[test]
    fn all_green_records_and_publishes() {
        let v = compute_verdict(&VerdictInput {
            run_system: true,
            heavy: LegRc::green(),
            system: LegRc::green(),
            harness: LegRc::green(),
        });
        assert_eq!(v.exit_code, 0);
        assert!(v.all_green);
        assert!(!v.partial);
        assert_eq!(v.heavy_state, "green");
        assert_eq!(v.harness_state, "green");
    }

    #[test]
    fn heavy_unprovisioned_aborts_10_because_no_current_leg_ran() {
        let v = compute_verdict(&VerdictInput {
            run_system: true,
            heavy: unprov(),
            system: LegRc::green(), // never ran; ignored because env_error
            harness: LegRc::green(),
        });
        assert_eq!(v.exit_code, 10);
        assert!(v.abort_all_unprovisioned);
        assert!(!v.partial);
        assert!(!v.all_green, "abort must NOT record .td-last-green");
        assert_eq!(v.heavy_state, "unprovisioned");
        assert_eq!(v.system_state, "unprovisioned");
        assert_eq!(v.harness_state, "green");
    }

    #[test]
    fn retired_harness_red_does_not_set_a_bit() {
        let v = compute_verdict(&VerdictInput {
            run_system: true,
            heavy: LegRc::green(),
            system: LegRc::green(),
            harness: red(1),
        });
        assert_eq!(v.exit_code, 0);
        assert!(!v.partial);
        assert!(v.all_green);
        assert_eq!(v.harness_state, "red");
    }

    #[test]
    fn real_heavy_regression_sets_bit_1() {
        let v = compute_verdict(&VerdictInput {
            run_system: true,
            heavy: red(1),
            system: LegRc::green(),
            harness: LegRc::green(),
        });
        assert_eq!(v.exit_code, 1);
        assert_eq!(v.heavy_state, "red");
        assert!(!v.all_green);
        assert!(!v.partial);
    }

    #[test]
    fn every_leg_unprovisioned_aborts_10() {
        let v = compute_verdict(&VerdictInput {
            run_system: true,
            heavy: unprov(),
            system: LegRc::green(),
            harness: unprov(),
        });
        assert_eq!(v.exit_code, 10);
        assert!(v.abort_all_unprovisioned);
        assert!(!v.all_green);
        assert!(!v.partial);
    }

    #[test]
    fn heavy_and_system_both_red_sets_bits_1_and_2() {
        let v = compute_verdict(&VerdictInput {
            run_system: true,
            heavy: red(1),
            system: red(1),
            harness: LegRc::green(),
        });
        assert_eq!(v.exit_code, 3);
        assert_eq!(v.system_state, "red");
    }

    #[test]
    fn no_system_flag_skips_that_leg() {
        let v = compute_verdict(&VerdictInput {
            run_system: false,
            heavy: LegRc::green(),
            system: LegRc::green(),
            harness: LegRc::green(),
        });
        assert_eq!(v.system_state, "skipped");
        assert_eq!(v.exit_code, 0);
        assert!(v.all_green);
    }

    // The harness tier is retired. Keep reporting its compatibility field, but do
    // not let it block the current heavy+system daily proof.
    #[test]
    fn retired_harness_unprovisioned_does_not_block_all_green() {
        let v = compute_verdict(&VerdictInput {
            run_system: true,
            heavy: LegRc::green(),
            system: LegRc::green(),
            harness: unprov(),
        });
        assert_eq!(v.exit_code, 0);
        assert!(!v.partial);
        assert!(v.all_green);
        assert_eq!(v.harness_state, "unprovisioned");
    }

    // Cross-model review (codex P2): a system leg that itself exits 69 while heavy is
    // provisioned must NOT set the system red bit (contract: unprovisioned legs never
    // set a bit) — it is unprovisioned, hence PARTIAL.
    #[test]
    fn system_unprovisioned_does_not_set_a_red_bit() {
        let v = compute_verdict(&VerdictInput {
            run_system: true,
            heavy: LegRc::green(),
            system: unprov(),
            harness: LegRc::green(),
        });
        assert_eq!(v.exit_code, 0, "no red bit for an unprovisioned system leg");
        assert_eq!(v.system_state, "unprovisioned");
        assert!(v.partial);
        assert!(!v.all_green);
    }
}

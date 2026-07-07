//! `td-builder daily` — the DAILY BACKSTOP runner (was ci/daily-full-suite.sh,
//! deleted in the same change; rust-migration, human 2026-07-06).
//!
//! Main does not block PRs on the full `td-builder check`: engine/heavy PRs land
//! on the bounded per-PR tiers + review, and the full heavy+daily+system suite
//! runs ONCE DAILY on fresh main here, driven by a scheduled agent that heals any
//! regression by opening a FIX-OR-REVERT PR (no auto-merge — a human merges). This
//! subcommand is the mechanical half: run the whole suite on fresh main and write a
//! machine-readable verdict; the agent reads the verdict and does the triage + PR.
//!
//! Usage:  td-builder daily [--no-system] [--verdict FILE]
//!
//! Exit is a bitfield over the suites — 1 heavy red, 2 system red, 4 harness red
//! (the /td/store harness tier); 0 = all green, up to 7. A leg that could not run
//! because the RUNNER is not provisioned for it (the base loop toolchain does not
//! resolve on PATH for heavy/system; no local + no fetchable harness for the
//! harness leg) does NOT set its bit — the leg's `td-builder check` exits
//! `EXIT_UNPROVISIONED` (69, a stable machine signal — no FATAL-prose grepping),
//! recorded in the verdict as env_error / harness_env_error. Setup errors exit
//! 8/9/10, kept out of the bitfield range:
//!   8  - unknown CLI argument
//!   9  - git fetch of origin/main failed (or no td-builder to run the legs)
//!   10 - runner not provisioned for EVERY leg: heavy/system's loop toolchain is
//!        unresolved AND the harness tier has no local/fetchable /td/store harness.
//!        No gate ran anywhere, so there is nothing to triage/revert. A runner that
//!        can run at least the harness leg does NOT hit this exit.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::check_loop::EXIT_UNPROVISIONED;

const HELP: &str = "\
td-builder daily [--no-system] [--verdict FILE]

Run the full td-builder check suite (heavy + daily + system + /td/store harness) on
fresh origin/main and write a machine-readable verdict. Exit is a bitfield: 1 heavy
red, 2 system red, 4 harness red; 8/9 setup error; 10 runner not provisioned for any
leg. Unprovisioned legs (base loop toolchain / harness absent) do not set a bit.";

const ENV_ERROR_MSG: &str =
    "runner not provisioned: the loop prelude could not resolve the base loop \
     toolchain on PATH — no gate ran, not a code regression";

const HARNESS_ENV_MSG: &str =
    "runner not provisioned: no local /td/store harness and none fetchable from a \
     substitute store — not a code regression";

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
    /// Harness green but heavy/system unprovisioned: a partial, not a full-suite
    /// proof — do NOT record `.td-last-green` or publish.
    partial: bool,
    /// Full suite green: record `.td-last-green` and publish substitutes.
    all_green: bool,
}

/// The exact verdict/exit contract ci/daily-full-suite.sh implemented, as a pure
/// function so the scenario matrix is a unit test (the shell only ever verified it
/// by stubbing td-builder). heavy/system share the "loop toolchain unprovisioned"
/// state: system is only attempted when heavy is provisioned.
fn compute_verdict(inp: &VerdictInput) -> Verdict {
    let env_error = inp.heavy.unprovisioned;
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
    } else if env_error {
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

    // Every leg unprovisioned → nothing ran; abort 10, no triage.
    if env_error && harness_env_error {
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
    if !env_error && inp.run_system && inp.system.rc != 0 {
        rc += 2;
    }
    if !harness_env_error && inp.harness.rc != 0 {
        rc += 4;
    }

    // rc==0 && env_error: the harness leg (the only one that ran) is green — but
    // heavy/system never ran, so it is not a full-suite proof.
    let partial = rc == 0 && env_error;
    let all_green = rc == 0 && !env_error;

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

    // Heavy: force-cold chain cache (#317) so the daily stays the authoritative
    // from-seed proof, and force the from-seed toolchain build + republish so the
    // daily does not fetch its own prior publish and self-starve.
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
    }

    // Harness: the /td/store harness tier (busybox+make). ALWAYS attempted — its
    // own precondition is "harness locally persisted or fetchable from a substitute
    // store", independent of whether heavy/system ran. `check check-harness` exits
    // EXIT_UNPROVISIONED when it has neither.
    println!(">> daily backstop: td-builder check check-harness on origin/main ({main}) — /td/store harness tier");
    let xlog = leg_log("harness");
    let harness_code = run_leg(&root, &tdb, &["check", "check-harness"], &[], &xlog);
    let harness = LegRc::from_code(harness_code);
    let harness_fail = if harness.unprovisioned {
        HARNESS_ENV_MSG.to_string()
    } else {
        grep_fails(&xlog)
    };
    if harness.unprovisioned {
        println!(">> daily backstop: {HARNESS_ENV_MSG}");
    }

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
             (heavy/system: {ENV_ERROR_MSG}; harness: {HARNESS_ENV_MSG})"
        );
        println!(
            ">> daily backstop: this is a HOST setup gap, not a code regression — no fix-or-revert \
             PR is warranted. Provision the loop toolchain (heavy/system) or expose a /td/store \
             harness substitute (harness), then re-run."
        );
        cat(&root, &verdict_path);
        return 10;
    }
    if v.env_error {
        println!(
            ">> daily backstop: heavy/system SKIPPED (runner not provisioned: {ENV_ERROR_MSG}) — \
             the harness leg ran independently"
        );
    }

    if v.partial {
        println!(
            ">> daily backstop: PARTIAL at {main} — harness leg GREEN; heavy/system unprovisioned \
             this run — not a full-suite proof, .td-last-green NOT recorded"
        );
    } else if v.all_green {
        let _ = std::fs::write(root.join(".td-last-green"), format!("{main}\n"));
        println!(">> daily backstop: ALL GREEN at {main} (recorded .td-last-green)");
        publish_substitutes(&root, &tdb);
    } else {
        println!(
            ">> daily backstop: RED (heavy_rc={} system_rc={} harness_rc={}) — agent: triage \
             `git log <last-green>..{main}`, reproduce the failing gate, open a FIX-OR-REVERT PR \
             (no auto-merge). Suspect-revert helper: ci/revert-suspect.sh --ref <sha> --open-pr",
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
    if which("cargo").is_some() {
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
            eprintln!("td-builder daily: cannot open leg log {}: {e}", log.display());
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

fn is_exec(p: &Path) -> bool {
    p.is_file()
}

fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var("PATH").ok()?;
    for dir in path.split(':').filter(|d| !d.is_empty()) {
        let p = Path::new(dir).join(name);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

/// On all-green: sign + publish the from-seed exports this run produced so the
/// per-PR loop FETCHES prebuilt closures instead of rebuilding. Guarded — each
/// block is a clear no-op unless the daily runner supplies TD_SUBST_PRIVKEY + a
/// td-subst binary. (The seed/harness publishers still shell out to their
/// tools/publish-*.sh; porting those is a separate rust-migration slice.)
fn publish_substitutes(root: &Path, tdb: &Path) {
    let store = std::env::var("TD_SUBST_STORE").unwrap_or_else(|_| {
        std::env::var("HOME")
            .map(|h| format!("{h}/.td/subst"))
            .unwrap_or_default()
    });
    let privkey = std::env::var("TD_SUBST_PRIVKEY").unwrap_or_default();
    let sb = std::env::var("TD_SUBST_BIN")
        .ok()
        .or_else(|| which("td-subst").map(|p| p.display().to_string()))
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

    // seed + harness: still shell out to their existing publishers.
    if privkey.is_empty() || sb.is_empty() {
        println!(">> publish-seed-subst: SKIP — TD_SUBST_PRIVKEY / td-subst binary not set");
        println!(">> publish-harness-subst: SKIP — TD_SUBST_PRIVKEY / td-subst binary not set");
        return;
    }
    let seed_ok = Command::new("sh")
        .args(["tools/publish-seed-subst.sh", "tests/td-builder-rust.lock", &store])
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

    if !root.join(".td-build-cache/harness/store").is_dir()
        || !non_empty(&root.join(".td-build-cache/harness/rel"))
    {
        println!(">> publish-harness-subst: SKIP — no persisted .td-build-cache/harness (gate 420 did not complete this run)");
        return;
    }
    let harness_ok = Command::new("sh")
        .args(["tools/publish-harness-subst.sh", ".td-build-cache/harness", &store])
        .current_dir(root)
        .env("TD_BUILDER", tdb)
        .env("TD_SUBST_BIN", &sb)
        .env("TD_SUBST_PRIVKEY", &privkey)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if harness_ok {
        println!(">> publish-harness-subst: signed + published the /td/store harness to {store} (a runner FETCHES it for check-harness)");
    } else {
        println!(">> publish-harness-subst: WARN — harness publish failed; not published");
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
        println!(">> {label}: SKIP — no export at .td-build-cache/{export} (nothing built this run)");
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

fn non_empty(p: &Path) -> bool {
    std::fs::metadata(p).map(|m| m.len() > 0).unwrap_or(false)
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
    fn heavy_unprovisioned_harness_green_is_partial() {
        let v = compute_verdict(&VerdictInput {
            run_system: true,
            heavy: unprov(),
            system: LegRc::green(), // never ran; ignored because env_error
            harness: LegRc::green(),
        });
        assert_eq!(v.exit_code, 0);
        assert!(v.partial, "harness green + heavy unprovisioned => PARTIAL");
        assert!(!v.all_green, "PARTIAL must NOT record .td-last-green");
        assert_eq!(v.heavy_state, "unprovisioned");
        assert_eq!(v.system_state, "unprovisioned");
        assert_eq!(v.harness_state, "green");
    }

    #[test]
    fn heavy_unprovisioned_harness_red_sets_only_bit_4() {
        let v = compute_verdict(&VerdictInput {
            run_system: true,
            heavy: unprov(),
            system: LegRc::green(),
            harness: red(1),
        });
        assert_eq!(v.exit_code, 4, "only the harness bit — heavy is unprovisioned, not red");
        assert!(!v.partial);
        assert!(!v.all_green);
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
}

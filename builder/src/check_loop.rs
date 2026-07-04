//! check_loop.rs — `td-builder check`: the loop's HOST PRELUDE, ported from the
//! old shell check.sh so that check.sh shrinks to a guix-free cargo bootstrap
//! shim (human direction 2026-07-03: "I don't want guix anywhere near check.sh" —
//! the host rust toolchain is the part the user brings; everything after
//! `cargo build` is td's own code).
//!
//! What runs here, in order (the exact sequence the shell prelude ran; the
//! rationale comments live with each step):
//!   1. the guix-free `check-harness` tier branch (never touches guix),
//!   2. the host-guix == pinned-channel integrity guard,
//!   3. the netns-probe discrimination check,
//!   4. stage0 provisioning (the guix-free loop-container provider, #294),
//!   5. the loop toolchain PATH (guix shell --search-paths — spawned as a child
//!      process; the package list lives in tools/loop-toolchain.txt so the CI
//!      image enumerators read the same single source),
//!   6. the warm prelude (subst store, source/crate warms, build daemon),
//!   7. the machine-wide slot dir, and
//!   8. the sandboxed gate run: TB host-sandbox --expose-cwd -- TB gate-run.
//!
//! The guix invocations here (describe/build/shell) are the loop's EXISTING,
//! ratcheted surface relocated from shell into typed code — not new surface; they
//! retire with the /td/store userland, exactly as before (directive 6).

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

fn fatal(msg: &str) -> String {
    format!("td-builder check: FATAL: {msg}")
}

/// First `name` on PATH (the child-spawn resolver `Command` itself uses).
fn find_in_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var("PATH").ok()?;
    for dir in path.split(':') {
        if dir.is_empty() {
            continue;
        }
        let p = Path::new(dir).join(name);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

fn run_capture(cmd: &mut Command) -> Result<String, String> {
    let out = cmd
        .stderr(Stdio::inherit())
        .output()
        .map_err(|e| format!("could not spawn {:?}: {e}", cmd.get_program()))?;
    if !out.status.success() {
        return Err(format!("{:?} exited {}", cmd.get_program(), out.status));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The pinned channel commit out of channels.scm (the shell `sed -n
/// 's/.*(commit *"..."/p'` — a 40-hex string inside `(commit "…")`).
fn pinned_commit(root: &Path) -> Result<String, String> {
    let text = std::fs::read_to_string(root.join("channels.scm"))
        .map_err(|e| fatal(&format!("cannot read channels.scm: {e}")))?;
    for (i, _) in text.match_indices("(commit") {
        let (_, from_match) = text.split_at(i);
        let after = from_match.trim_start_matches("(commit").trim_start();
        if let Some(rest) = after.strip_prefix('"') {
            let hex: String = rest.chars().take_while(|c| c.is_ascii_hexdigit()).collect();
            if hex.len() == 40 {
                return Ok(hex);
            }
        }
    }
    Err(fatal("could not parse pinned commit from channels.scm"))
}

/// --- Integrity guard: host guix must equal the pinned channel commit ----------
/// The offline/no-download property holds ONLY because the host system guix is
/// the exact commit channels.scm pins: time-machine to a *different* commit would
/// recompute the channel-instance derivation, miss the warm store, and try to
/// download it. Fail loudly rather than silently going online.
fn guard_pinned_guix(root: &Path) -> Result<(), String> {
    let pinned = pinned_commit(root)?;
    let desc = run_capture(Command::new("guix").args(["describe", "-f", "recutils"]))
        .unwrap_or_default();
    let host = desc
        .lines()
        .find_map(|l| l.strip_prefix("commit:").map(|v| v.trim().to_string()))
        .unwrap_or_default();
    if host.is_empty() {
        // Distinguish "guix missing/broken" from a genuine pin mismatch — an
        // empty commit in the mismatch message sent operators to re-pin
        // channels.scm when the real problem was `guix describe` itself.
        return Err(fatal(
            "could not read the host guix commit (`guix describe` failed — is guix \
             installed and on PATH?). The loop needs the pinned host guix; a guix-less \
             host runs only `./check.sh check-harness`.",
        ));
    }
    if host != pinned {
        return Err(fatal(&format!(
            "host guix ({host}) != pinned channel ({pinned}).\n  The offline loop assumes \
             they match (see HISTORY.md). Refusing to run a check that would silently \
             download substitutes."
        )));
    }
    Ok(())
}

/// --- Offline-isolation control: the netns probe mechanism must discriminate ---
/// The offline probes assert "only `lo` in /proc/net/dev" inside builders; that
/// only has teeth if the same mechanism reports a non-loopback interface where
/// network IS present — observable only here on the host. Fail loudly on a host
/// with no non-lo interface (the probes would be vacuously green).
fn guard_netns_probe() -> Result<(), String> {
    let text = std::fs::read_to_string("/proc/net/dev")
        .map_err(|e| fatal(&format!("cannot read /proc/net/dev: {e}")))?;
    let has_non_lo = text.lines().any(|l| {
        l.split_once(':')
            .map(|(name, _)| {
                let name = name.trim();
                !name.is_empty() && name != "lo" && !name.contains(' ') && !name.contains('|')
            })
            .unwrap_or(false)
    });
    if !has_non_lo {
        return Err(fatal(
            "the host netns shows no non-loopback interface in /proc/net/dev, so the \
             offline rung's loopback-only probes cannot discriminate an isolated netns \
             from a working one on this host.",
        ));
    }
    Ok(())
}

/// Provision the guix-free stage0 td-builder (the loop-container provider,
/// workstream E #294) via the existing shell machinery and return $TB.
fn provision_stage0(root: &Path) -> Result<String, String> {
    let out = run_capture(
        Command::new("sh")
            .arg("-c")
            .arg(". tests/cache-lib.sh && provision_stage0 1>&2 && printf '%s' \"$TB\"")
            .current_dir(root),
    )
    .map_err(|e| {
        fatal(&format!("could not provision the guix-free stage0 td-builder for the loop sandbox ({e})"))
    })?;
    let tb = out.trim().to_string();
    if tb.is_empty() || !Path::new(&tb).is_file() {
        return Err(fatal("stage0 provisioning returned no usable $TB"));
    }
    Ok(tb)
}

/// The loop toolchain PATH: the packages `guix shell -C` used to put on PATH,
/// provisioned as a profile (no container). The package list is
/// tools/loop-toolchain.txt — ONE source shared with the CI image enumerators
/// (ci/lower-*-drvs.sh), which used to sed-scrape the shell prelude for it.
/// This `guix shell` is the loop substrate's last guix-provisioned piece; it
/// retires when td's own /td/store userland supplies these tools.
fn provision_toolchain(root: &Path) -> Result<String, String> {
    let pkgs_text = std::fs::read_to_string(root.join("tools/loop-toolchain.txt"))
        .map_err(|e| fatal(&format!("cannot read tools/loop-toolchain.txt: {e}")))?;
    let pkgs: Vec<&str> = pkgs_text.split_whitespace().collect();
    if pkgs.is_empty() {
        return Err(fatal("tools/loop-toolchain.txt is empty"));
    }
    let mut cmd = Command::new("guix");
    cmd.args(["shell", "--no-substitutes", "--no-offload"])
        .args(&pkgs)
        .arg("--search-paths")
        .current_dir(root);
    let out = run_capture(&mut cmd)
        .map_err(|e| fatal(&format!("could not provision the loop toolchain PATH ({e})")))?;
    // `export PATH="<bin:sbin>${PATH:+:}$PATH"` — take the leading non-`$` run.
    for line in out.lines() {
        if let Some(rest) = line.strip_prefix("export PATH=\"") {
            let val: String = rest.chars().take_while(|c| *c != '$' && *c != '"').collect();
            if !val.is_empty() {
                return Ok(val);
            }
        }
    }
    Err(fatal("could not provision the loop toolchain PATH (no PATH line)"))
}

/// Wrap a warm step with `timeout` (TD_WARM_TIMEOUT, default 600s) when
/// coreutils timeout exists — one hung mirror must not stall the prelude.
fn warm_argv(base: &[String]) -> Vec<String> {
    let secs = std::env::var("TD_WARM_TIMEOUT").unwrap_or_else(|_| "600".to_string());
    if find_in_path("timeout").is_some() {
        let mut v = vec!["timeout".to_string(), secs];
        v.extend(base.iter().cloned());
        v
    } else {
        base.to_vec()
    }
}

fn spawn_argv(argv: &[String], root: &Path, envs: &[(String, String)]) -> Option<std::process::Child> {
    let (head, rest) = argv.split_first()?;
    let mut cmd = Command::new(head);
    cmd.args(rest).current_dir(root);
    for (k, v) in envs {
        cmd.env(k, v);
    }
    cmd.spawn().ok()
}

fn warm_status(argv: &[String], root: &Path, envs: &[(String, String)]) -> bool {
    let wrapped = warm_argv(argv);
    match spawn_argv(&wrapped, root, envs) {
        Some(mut child) => child.wait().map(|s| s.success()).unwrap_or(false),
        None => false,
    }
}

fn warm_capture(argv: &[String], root: &Path, envs: &[(String, String)]) -> String {
    let wrapped = warm_argv(argv);
    let Some((head, rest)) = wrapped.split_first() else { return String::new() };
    let mut cmd = Command::new(head);
    cmd.args(rest).current_dir(root).stderr(Stdio::null());
    for (k, v) in envs {
        cmd.env(k, v);
    }
    cmd.output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

fn s(v: &str) -> String {
    v.to_string()
}

/// The working-tree content key for the verdict journal (issue #320): sha256
/// over git HEAD + the full dirty diff + every untracked file's bytes — ANY
/// tree change yields a new key, so a --resume skip can never survive an edit
/// (whole-tree invalidation, deliberately no per-gate cleverness). None when
/// git is unavailable (resume then refuses to run).
fn tree_key(root: &Path) -> Option<String> {
    let git = |args: &[&str]| -> Option<Vec<u8>> {
        let out = Command::new("git").args(args).current_dir(root).output().ok()?;
        if !out.status.success() {
            return None;
        }
        Some(out.stdout)
    };
    let mut h = crate::sha256::Sha256::new();
    h.update(&git(&["rev-parse", "HEAD"])?);
    h.update(&git(&["diff", "HEAD"])?);
    let status = git(&["status", "--porcelain=v1", "-uall", "-z"])?;
    h.update(&status);
    // Untracked file CONTENTS too — `git diff` cannot see them, and an edited
    // untracked input changing a gate's behavior must invalidate the journal.
    for entry in status.split(|b| *b == 0) {
        let line = String::from_utf8_lossy(entry);
        if let Some(path) = line.strip_prefix("?? ") {
            if let Ok(bytes) = std::fs::read(root.join(path)) {
                h.update(path.as_bytes());
                h.update(&bytes);
            }
        }
    }
    Some(crate::sha256::to_base16(&h.finalize()))
}

/// The substitute-store exposure (x64-toolchain-subst): tools/warm-subst.sh
/// echoes `export TD_SUBST_*='…'` lines when a prior daily populated ~/.td/subst;
/// parse them into child env (host-sandbox preserves TD_SUBST_*). No-op on a
/// cold machine. TD_SUBST_FORCE_BUILD=1 (the daily's authoritative run)
/// suppresses the exposure so the daily always builds from seed.
fn subst_env(root: &Path) -> Vec<(String, String)> {
    if std::env::var("TD_SUBST_FORCE_BUILD").ok().as_deref() == Some("1") {
        return Vec::new();
    }
    let out = warm_capture(&[s("sh"), s("tools/warm-subst.sh")], root, &[]);
    let mut envs = Vec::new();
    for line in out.lines() {
        let Some(rest) = line.strip_prefix("export ") else { continue };
        let Some((k, v)) = rest.split_once('=') else { continue };
        if !k.starts_with("TD_SUBST_") {
            continue;
        }
        let v = v.trim().trim_matches('\'');
        envs.push((k.to_string(), v.to_string()));
    }
    envs
}

/// The heavy-tier warm prelude: source-bootstrap tarballs + rust crate closures
/// (td-feed), all BEST-EFFORT (the gates enforce presence), fanned out in
/// batches of TD_WARM_JOBS exactly as the shell prelude did.
fn heavy_warms(root: &Path) {
    // td-fetch's own crate closure (its own warm — not the cargo-proxy).
    let _ = warm_status(&[s("sh"), s("tools/warm-td-fetch-crates.sh")], root, &[]);

    // Resolve ONE host td-feed binary: the gate's td-built one, else a host
    // cargo build of feed/.
    let mut tdfeed = String::new();
    if let Ok(entries) = std::fs::read_dir(root.join(".td-build-cache/td-feed/sd/newstore")) {
        let mut candidates: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path().join("bin/td-feed"))
            .filter(|p| p.is_file())
            .collect();
        candidates.sort();
        if let Some(p) = candidates.first() {
            tdfeed = p.display().to_string();
        }
    }
    if tdfeed.is_empty() && find_in_path("cargo").is_some() {
        let built = Command::new("cargo")
            .args(["build", "--release", "--quiet"])
            .current_dir(root.join("feed"))
            .status()
            .map(|st| st.success())
            .unwrap_or(false);
        if built {
            tdfeed = root.join("feed/target/release/td-feed").display().to_string();
        }
    }
    if tdfeed.is_empty() || !Path::new(&tdfeed).is_file() {
        eprintln!(
            "td-builder check: no td-feed binary for the heavy warm (build feed/ with cargo) — \
             skipping (best-effort; the heavy gates enforce presence)"
        );
        return;
    }

    // `td-feed warm sources` (serial-first), routed through the ONE shared
    // td-feed serve daemon when feed-ensure can start/reuse it.
    let mut src_envs = vec![(s("TD_ROOT"), root.display().to_string())];
    let faddr = warm_capture(
        &[s("sh"), s("tools/feed-ensure.sh")],
        root,
        &[(s("TD_FEED_BIN"), tdfeed.clone())],
    );
    if !faddr.is_empty() {
        src_envs.push((s("TD_FEED_BASE"), format!("http://{faddr}")));
    }
    let _ = warm_status(&[tdfeed.clone(), s("warm"), s("sources")], root, &src_envs);

    // Corpus crate warms: independent, fanned out in batches of TD_WARM_JOBS.
    let warm_jobs: usize = std::env::var("TD_WARM_JOBS")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .filter(|n| *n >= 1)
        .unwrap_or(4);
    let specs: [&[&str]; 10] = [
        &["warm", "crate", "ripgrep", "14.1.1"],
        &["warm", "crate", "sd", "1.0.0"],
        &["warm", "crate", "fd-find", "10.2.0", "fd"],
        &["warm", "crate", "procs", "0.14.10"],
        &["warm", "crate", "eza", "0.21.6"],
        &["warm", "crate", "bat", "0.25.0"],
        &["warm", "crate", "coreutils", "0.9.0", "uutils"],
        &["warm", "crate", "youki", "0.6.0"],
        &["warm", "crate", "uu_cat", "0.9.0", "cat"],
        // Local-source variant: russh's 188-crate DEP closure only.
        &["warm", "crate-local", "tests/russh-demo", "russh"],
    ];
    let envs = vec![(s("TD_ROOT"), root.display().to_string())];
    let mut running: Vec<(std::process::Child, Vec<String>)> = Vec::new();
    let drain = |running: &mut Vec<(std::process::Child, Vec<String>)>| {
        for (mut c, argv) in running.drain(..) {
            let ok = c.wait().map(|st| st.success()).unwrap_or(false);
            if !ok {
                eprintln!(
                    "td-builder check: cargo-proxy warm (best-effort) failed/timed out: {}",
                    argv.join(" ")
                );
            }
        }
    };
    for spec in specs {
        let mut argv = vec![tdfeed.clone()];
        argv.extend(spec.iter().map(|a| s(a)));
        let wrapped = warm_argv(&argv);
        if let Some(child) = spawn_argv(&wrapped, root, &envs) {
            running.push((child, argv));
        } else {
            eprintln!(
                "td-builder check: cargo-proxy warm (best-effort) could not spawn: {}",
                argv.join(" ")
            );
        }
        if running.len() >= warm_jobs {
            drain(&mut running);
        }
    }
    drain(&mut running);
}

/// The guix-free `check-harness` tier: enter td's OWN /td/store harness via the
/// stage0 td-builder — handled BEFORE the guix guard/toolchain so this tier
/// never invokes guix (the stage0 warm path spawns none once placed).
fn run_check_harness(root: &Path) -> Result<i32, String> {
    let hdir = root.join(".td-build-cache/harness");
    let rel_f = hdir.join("rel");
    if !hdir.join("store").is_dir() || !rel_f.is_file() {
        return Err(fatal(&format!(
            "no provisioned /td/store harness at {}.\n  Provision it on a guix capture host \
             first:  ./check.sh userland-x86_64-store-native\n  (builds busybox+make at \
             /td/store + persists them here); ship the dir to a guix-less VM.",
            hdir.display()
        )));
    }
    let hrel = std::fs::read_to_string(&rel_f)
        .map_err(|e| fatal(&format!("cannot read {}: {e}", rel_f.display())))?;
    let hrel = hrel.trim();
    if hrel.is_empty() {
        return Err(fatal("empty harness rel"));
    }
    let hbin = format!("/td/store/{hrel}/bin");
    let tb = {
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg(". tests/cache-lib.sh && load_stage0 1>&2 && printf '%s' \"$TB\"")
            .env("TD_STAGE0_BASE", root.join(".td-build-cache/stage0"))
            .current_dir(root);
        let out = run_capture(&mut cmd)
            .map_err(|_| fatal("could not build the guix-free stage0 td-builder."))?;
        out.trim().to_string()
    };
    println!(">> check-harness: entering td's /td/store harness via the guix-free stage0 td-builder ({tb})");
    println!("   harness: {}/store  set: {hrel}  (guix + /gnu/store ABSENT inside)", hdir.display());
    let st = Command::new(&tb)
        .args([
            "host-sandbox",
            "--expose-cwd",
            "--store-from",
            &format!("{}/store", hdir.display()),
            "--store-at",
            "/td/store",
            "--no-daemon",
            "--",
            &format!("{hbin}/make"),
            "-f",
            "mk/harness.mk",
            &format!("HBIN={hbin}"),
            &format!("SHELL={hbin}/sh"),
            "check-harness-inner",
        ])
        .current_dir(root)
        .status()
        .map_err(|e| fatal(&format!("could not enter the harness: {e}")))?;
    Ok(st.code().unwrap_or(1))
}

pub fn cli(args: &[String]) -> ExitCode {
    match run(args) {
        Ok(code) => ExitCode::from(code.clamp(0, 255) as u8),
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &[String]) -> Result<i32, String> {
    let root = std::env::current_dir().map_err(|e| fatal(&format!("cannot resolve cwd: {e}")))?;
    if !root.join("tests").is_dir() || !root.join("channels.scm").is_file() {
        return Err(fatal("run from the repo root (tests/ + channels.scm not found)"));
    }

    // Parse args LOUDLY: `-j N`/`-jN` overrides the local worker width; any other
    // flag is a hard error (the old shell prelude forwarded "$@" to make, so
    // silently dropping a flag here would turn e.g. a throttle request into a
    // full-width run — the opposite of the user's intent).
    let mut goals: Vec<String> = Vec::new();
    let mut jobs_flag: Option<usize> = None;
    let mut resume = false;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let jv = if a == "-j" || a == "--jobs" {
            Some(it.next().cloned().unwrap_or_default())
        } else {
            a.strip_prefix("-j").map(str::to_string)
        };
        if a == "-j" || a == "--jobs" || (a.starts_with("-j") && a.len() > 2) {
            match jv.as_deref().unwrap_or("").trim().parse::<usize>() {
                Ok(n) if n >= 1 => jobs_flag = Some(n),
                _ => return Err(fatal(&format!("bad {a} value — -j needs a positive integer"))),
            }
        } else if a == "--resume" {
            resume = true;
        } else if a.starts_with('-') {
            return Err(fatal(&format!(
                "unknown flag `{a}` — td-builder check takes goals (tiers/gate names), \
                 -j N, and --resume; there is no make behind this anymore"
            )));
        } else {
            goals.push(a.clone());
        }
    }
    if goals.is_empty() {
        goals.push("check".to_string());
    }

    // The guix-free harness tier: never reaches the guix guard below.
    if goals.first().map(String::as_str) == Some("check-harness") {
        return run_check_harness(&root);
    }

    guard_pinned_guix(&root)?;
    guard_netns_probe()?;

    // Host guix stays first on PATH inside the sandbox (its dir prepended).
    let hostguix_dir = find_in_path("guix")
        .and_then(|p| std::fs::canonicalize(p).ok())
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .ok_or_else(|| fatal("no guix on PATH"))?;

    // Light tiers own no heavy gate — skip the heavy warms + daemon (exactly the
    // shell prelude's goal scan).
    let heavy_warm = goals.iter().any(|g| {
        !matches!(g.as_str(), "check-fast" | "check-engine" | "list-gates" | "gate-timing-report")
    });

    let tb = provision_stage0(&root)?;
    let toolchain = provision_toolchain(&root)?;

    let mut child_envs: Vec<(String, String)> = vec![
        (s("PATH"), format!("{}:{toolchain}", hostguix_dir.display())),
        (s("GUIX_BUILD_OPTIONS"), s("--no-substitutes --no-offload")),
    ];
    // The runner's knobs must cross the sandbox boundary (host-sandbox
    // preserves the TD_CHECK_ prefix): without this, TD_CHECK_SLOTS=… ./check.sh
    // would be silently dead and gate-run would always default to nproc.
    // TD_CHECK_CHAIN_CACHE rides along for the same reason: `TD_CHECK_CHAIN_CACHE= ./check.sh`
    // (set-and-empty) is the operator's force-cold switch for the #317 warm
    // chain-brick default — the daily backstop uses it to stay authoritative.
    for k in ["TD_CHECK_SLOTS", "TD_CHECK_SLOTS_DIR", "TD_CHECK_JOBS", "TD_CHECK_CHAIN_CACHE"] {
        if let Ok(v) = std::env::var(k) {
            child_envs.push((k.to_string(), v));
        }
    }
    // The verdict-journal tree key (issue #320): computed on the HOST (git is
    // not in the sandbox toolchain) and forwarded so gate-run journals every
    // PASS; --resume additionally skips journaled-green gates for this exact
    // key. TD_CHECK_FULL forces everything, resume included.
    if std::env::var("TD_CHECK_FULL").is_ok() && resume {
        eprintln!("td-builder check: TD_CHECK_FULL is set — ignoring --resume");
        resume = false;
    }
    match tree_key(&root) {
        Some(key) => child_envs.push((s("TD_CHECK_TREE"), key)),
        None if resume => {
            return Err(fatal(
                "--resume needs a git working tree to key the verdict journal, and                  `git` failed here — cannot prove the tree is unchanged, refusing to skip",
            ))
        }
        None => {}
    }
    child_envs.extend(subst_env(&root));

    if heavy_warm {
        heavy_warms(&root);
        // The shared build daemon: the loop's single machine-wide BUILD limiter
        // (host-side; it must outlive this check). Only the heavy tier needs it.
        let sock = warm_capture(&[s("sh"), s("tools/build-daemon-ensure.sh")], &root, &[]);
        if sock.is_empty() {
            eprintln!(
                "td-builder check: WARNING: could not start the shared build daemon \
                 (build-daemon-ensure.sh); corpus gates will fail loudly"
            );
        } else {
            child_envs.push((s("TD_DAEMON_SOCKET"), sock));
        }
    }

    // The machine-wide slot dir must exist HOST-SIDE so host-sandbox binds
    // ~/.td/build-daemon (same absolute path inside) — that bind is what makes
    // the gate runner's slot pool machine-wide. The chain-brick cache (#317's
    // flipped shared-state default) lives under the same bind for the same
    // reason: every check sandbox sees it at the same absolute path, RW.
    if let Ok(home) = std::env::var("HOME") {
        let _ = std::fs::create_dir_all(Path::new(&home).join(".td/build-daemon/slots"));
        let _ = std::fs::create_dir_all(Path::new(&home).join(".td/build-daemon/chain"));
    }

    let jobs = jobs_flag.unwrap_or_else(|| {
        std::env::var("TD_CHECK_JOBS")
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
            .filter(|n| *n >= 1)
            .unwrap_or_else(crate::gates::nproc)
    });

    // nice/ionice the whole loop so it yields to interactive work; the slot pool
    // and daemon budget bound how MUCH runs, nice bounds its priority.
    let tdnice = std::env::var("TD_NICE").unwrap_or_else(|_| "10".to_string());
    let mut argv: Vec<String> = Vec::new();
    if find_in_path("nice").is_some() {
        argv.extend([s("nice"), s("-n"), tdnice]);
        if find_in_path("ionice").is_some() {
            argv.extend([s("ionice"), s("-c2"), s("-n7")]);
        }
    }
    argv.extend([tb.clone(), s("host-sandbox"), s("--expose-cwd"), s("--"), tb, s("gate-run")]);
    argv.extend([s("-j"), jobs.to_string()]);
    if resume {
        argv.push(s("--resume"));
    }
    argv.extend(goals);

    let (head, rest) = argv
        .split_first()
        .ok_or_else(|| fatal("internal: empty loop argv"))?;
    let mut cmd = Command::new(head);
    cmd.args(rest).current_dir(&root);
    for (k, v) in &child_envs {
        cmd.env(k, v);
    }
    let st = cmd
        .status()
        .map_err(|e| fatal(&format!("could not start the loop sandbox: {e}")))?;
    let _ = std::io::stdout().flush();
    Ok(st.code().unwrap_or(1))
}

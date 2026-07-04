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

/// A writable cgroup-v2 subtree delegated to this uid, or None (issue #328).
/// Probe order: TD_CGROUP_ROOT (explicit) → /sys/fs/cgroup/td (the documented
/// Guix System/Shepherd delegation: one root-side
///   mkdir /sys/fs/cgroup/td
///   echo +memory > /sys/fs/cgroup/cgroup.subtree_control
///   chown -R <loop-user> /sys/fs/cgroup/td
/// ) → the process's OWN cgroup dir (systemd hosts: user@.service subtrees are
/// Delegate=yes, so /proc/self/cgroup names a dir we own). Writability is
/// proven by actually creating a child (the only test that matters).
fn cgroup_delegated_root() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(v) = std::env::var("TD_CGROUP_ROOT") {
        // `off` forces the NON-cgroup path on a delegated machine — keeps the
        // watchdog fallback testable where cgroup mode would otherwise win
        // (human direction 2026-07-03).
        if matches!(v.as_str(), "off" | "none" | "0") {
            eprintln!("td-builder check: cgroup mode disabled (TD_CGROUP_ROOT={v}) — using the watchdog fallback");
            return None;
        }
        if !v.is_empty() {
            candidates.push(PathBuf::from(v));
        }
    }
    candidates.push(PathBuf::from("/sys/fs/cgroup/td"));
    if let Ok(selfcg) = std::fs::read_to_string("/proc/self/cgroup") {
        if let Some(path) = selfcg.lines().find_map(|l| l.strip_prefix("0::")) {
            candidates.push(PathBuf::from(format!("/sys/fs/cgroup{}", path.trim())));
        }
    }
    for c in candidates {
        // Must be cgroup2fs, not merely a writable directory: on a plain dir
        // every 'cgroup file' write would create ordinary files and appear to
        // succeed — cgroup mode would engage with ZERO kernel enforcement
        // while also disabling the watchdog (review finding).
        if !c.join("cgroup.controllers").is_file() {
            continue;
        }
        let probe = c.join(format!("td-probe-{}", std::process::id()));
        if std::fs::create_dir(&probe).is_ok() {
            let _ = std::fs::remove_dir(&probe);
            return Some(c);
        }
    }
    None
}

/// Prepare the per-run cgroup parent under the delegated root: enable the
/// memory controller for its children and return the run dir. Best-effort —
/// any failure means "no cgroup mode this run" (the watchdog fallback holds).
fn cgroup_run_dir(root: &Path) -> Option<PathBuf> {
    // Sweep DEAD runs' leftovers: a run dir can't remove itself (the check
    // process sits in its own host leaf until exit), so each run reaps its
    // predecessors — empty leaves + parents whose pid is gone rmdir cleanly;
    // a LIVE concurrent run's dirs are populated and refuse, which is the
    // correct discrimination.
    if let Ok(entries) = std::fs::read_dir(root) {
        for e in entries.flatten() {
            let p = e.path();
            let name = e.file_name();
            let Some(n) = name.to_str() else { continue };
            if !(n.starts_with("run-") || n.starts_with("td-test-")) || !p.is_dir() {
                continue;
            }
            // LIVENESS, not emptiness: a live concurrent run's just-created
            // leaf is momentarily empty (between cgroup_enter and the body's
            // self-move) and rmdir would spuriously red its gate with exit 97
            // (review finding). Only dirs whose owning pid is GONE are reaped.
            let alive = n
                .rsplit_once('-')
                .and_then(|(_, pid)| pid.parse::<u32>().ok())
                .map(|pid| Path::new(&format!("/proc/{pid}")).exists())
                .unwrap_or(true);
            if alive {
                continue;
            }
            if let Ok(children) = std::fs::read_dir(&p) {
                for c in children.flatten() {
                    if c.path().is_dir() {
                        let _ = std::fs::remove_dir(c.path());
                    }
                }
            }
            let _ = std::fs::remove_dir(&p);
        }
    }
    let run = root.join(format!("run-{}", std::process::id()));
    std::fs::create_dir(&run).ok()?;
    // THE FIRST HOP: migrating a process needs write access to the COMMON
    // ANCESTOR's cgroup.procs, and this process starts OUTSIDE the delegated
    // subtree — so self-move ONCE here (into a host leaf; the run dir itself
    // must stay process-free, it has child controllers). Every descendant
    // (sandbox → gate-run → gates) then inherits, and the gates' own moves
    // (host leaf → gate leaf) share the user-owned run dir as ancestor —
    // always permitted. If THIS write is EPERM, the delegation lacks the
    // first-hop grant — group-writable root cgroup.procs (chgrp+g+w for the
    // loop user's group), or a PAM session hook placing sessions inside the
    // subtree as systemd's PID1 does — fall back loudly.
    // ORDER MATTERS (review finding): the self-move must precede enabling
    // controllers — the no-internal-process rule EBUSYes a subtree_control
    // write while the cgroup has member processes, so the own-cgroup
    // (systemd scope) candidate only works if we vacate it FIRST.
    let host_leaf = run.join("host");
    if std::fs::create_dir(&host_leaf).is_err()
        || std::fs::write(host_leaf.join("cgroup.procs"), std::process::id().to_string())
            .is_err()
    {
        let _ = std::fs::remove_dir(&host_leaf);
        let _ = std::fs::remove_dir(&run);
        eprintln!(
            "td-builder check: delegated cgroup subtree found but the FIRST HOP into it \
             is denied (common-ancestor cgroup.procs) — grant it once, e.g.  \
             sudo sh -c 'chgrp <loop-group> /sys/fs/cgroup/cgroup.procs && chmod g+w \
             /sys/fs/cgroup/cgroup.procs'  (cgroupfs perms reset at boot: persist it \
             in the system config; issue #328)"
        );
        return None;
    }
    // Controllers, after vacating: root (may only now be empty in the scope
    // case), then the run dir (its processes live in leaves, never in itself).
    let _ = std::fs::write(root.join("cgroup.subtree_control"), "+memory");
    if std::fs::write(run.join("cgroup.subtree_control"), "+memory").is_err() {
        // Leave the dirs for the next run's sweep — this process now SITS in
        // run/host and cannot rmdir it.
        eprintln!(
            "td-builder check: delegated cgroup subtree found but the memory controller \
             could not be enabled for it — falling back to the watchdog"
        );
        return None;
    }
    Some(run)
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

/// The substitute-store exposure (x64-toolchain-subst, human 2026-06-28;
/// native since #318 axis 2 — was tools/warm-subst.sh): if a prior DAILY run
/// populated a persistent signed substitute store (~/.td/subst: a stashed
/// td-subst binary + the published closure narinfos), expose TD_SUBST_* to
/// the loop sandbox (host-sandbox binds ~/.td/subst ro + preserves
/// TD_SUBST_*). The toolchain gates then FETCH the lock-keyed closure instead
/// of rebuilding ~98 min from seed, FALLING BACK to from-seed on ANY miss.
/// This NEVER fetches or builds td-subst — the DAILY is the sole producer; a
/// COLD machine (no prior daily) exposes nothing and the gate builds from
/// seed (the substitute is an optimization, never a correctness dependency).
/// TD_SUBST_FORCE_BUILD=1 (the daily's authoritative run) suppresses the
/// exposure so the daily always builds from seed.
fn subst_env(root: &Path) -> Vec<(String, String)> {
    if std::env::var("TD_SUBST_FORCE_BUILD").ok().as_deref() == Some("1") {
        return Vec::new();
    }
    let store = match std::env::var("TD_SUBST_STORE") {
        Ok(v) if !v.trim().is_empty() => PathBuf::from(v),
        _ => match std::env::var("HOME") {
            Ok(h) => Path::new(&h).join(".td/subst"),
            Err(_) => return Vec::new(),
        },
    };
    subst_env_at(&store, &root.join("tests/td-subst.pub"))
}

/// A USABLE store = the daily's stashed td-subst binary + at least one signed
/// narinfo + the pinned trust anchor. Any missing piece => expose nothing.
fn subst_env_at(store: &Path, pubkey: &Path) -> Vec<(String, String)> {
    use std::os::unix::fs::PermissionsExt as _;
    let bin = store.join("td-subst");
    let executable = std::fs::metadata(&bin)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false);
    if !executable {
        return Vec::new();
    }
    let has_narinfo = std::fs::read_dir(store)
        .map(|rd| rd.flatten().any(|e| e.path().extension().is_some_and(|x| x == "narinfo")))
        .unwrap_or(false);
    if !has_narinfo {
        return Vec::new();
    }
    if !std::fs::metadata(pubkey).map(|m| m.is_file() && m.len() > 0).unwrap_or(false) {
        return Vec::new();
    }
    vec![
        (s("TD_SUBST_BIN"), bin.display().to_string()),
        (s("TD_SUBST_STORE"), store.display().to_string()),
        (s("TD_SUBST_PUBKEY"), pubkey.display().to_string()),
    ]
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
    // td-feed serve daemon when `td-feed ensure-serve` can start/reuse it
    // (native since #318 axis 2 — was tools/feed-ensure.sh).
    let mut src_envs = vec![(s("TD_ROOT"), root.display().to_string())];
    let faddr = warm_capture(&[tdfeed.clone(), s("ensure-serve")], root, &[]);
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

/// Ensure ONE shared, persistent td build daemon is running for this host and
/// return its Unix-socket PATH (native since #318 axis 2 — was
/// tools/build-daemon-ensure.sh). Idempotent + concurrency-safe (an exclusive
/// file lock serializes ensures): the FIRST caller starts the daemon; every
/// later caller (any worktree, any agent) reuses it. This is how N agents on N
/// worktrees SHARE one builder with ONE global budget — the machine-wide build
/// limiter. The daemon realizes drvs submitted over the socket (`td-builder
/// daemon`), bounded to TD_BUILD_JOBS concurrent builds; the per-drv builder
/// override travels with each request, so one shared daemon serves every
/// worktree.
///
/// The daemon BINARY is the provisioned stage0 `tb` (TD_DAEMON_BUILDER
/// overrides) — the same deterministic current-tree build the loop's client
/// (cache-lib) resolves, so the client and the serving daemon always speak the
/// same request grammar. The socket/pid/log are keyed by the binary's CONTENT
/// hash: a daemon started by a different (e.g. older-grammar) td-builder lives
/// on a different socket, so an ensure never reuses a stale-grammar daemon;
/// old-binary daemons idle out on their own sockets.
///
/// Env: TD_DAEMON_DIR (shared dir, default ~/.td/build-daemon),
/// TD_DAEMON_BUILDER (daemon binary override), TD_DAEMON_SEED_DIR (the
/// start-time seed store DIR, default /gnu/store — content-scanned for the
/// input closure, #267; only bare-drv requests use it), TD_BUILD_JOBS (the
/// global budget — inherited by the spawned daemon), TD_NICE (nice level for
/// the daemon + its build children, default 10).
fn ensure_build_daemon(root: &Path, tb: &str) -> Result<String, String> {
    let daemon_dir = match std::env::var("TD_DAEMON_DIR") {
        Ok(v) if !v.trim().is_empty() => PathBuf::from(v),
        _ => {
            let home = std::env::var("HOME").map_err(|_| s("no HOME for TD_DAEMON_DIR"))?;
            Path::new(&home).join(".td/build-daemon")
        }
    };
    let store = daemon_dir.join("store");
    std::fs::create_dir_all(&store).map_err(|e| format!("mkdir {}: {e}", store.display()))?;
    let seed_dir = std::env::var("TD_DAEMON_SEED_DIR").unwrap_or_else(|_| s("/gnu/store"));
    let daemon_tb = match std::env::var("TD_DAEMON_BUILDER") {
        Ok(v) if !v.trim().is_empty() && Path::new(&v).is_file() => v,
        _ => tb.to_string(),
    };

    // Key the socket/pid/log by the daemon binary's CONTENT hash (grammar skew
    // guard — see the doc comment).
    let bytes = std::fs::read(&daemon_tb).map_err(|e| format!("read {daemon_tb}: {e}"))?;
    let mut h = crate::sha256::Sha256::new();
    h.update(&bytes);
    let full = crate::sha256::to_base16(&h.finalize());
    let key: String = full.chars().take(16).collect();
    let sock = daemon_dir.join(format!("socket.{key}"));
    let pid_f = daemon_dir.join(format!("daemon.{key}.pid"));
    let log_f = daemon_dir.join(format!("daemon.{key}.log"));

    // Serialize concurrent ensures so two agents never both start a daemon.
    // The lock file is O_CLOEXEC (std default), so the spawned daemon does not
    // inherit-and-hold it; it releases when this fn returns.
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false) // a lock handle only; its content is never written
        .open(daemon_dir.join("daemon.lock"))
        .map_err(|e| format!("open daemon.lock: {e}"))?;
    lock.lock().map_err(|e| format!("lock daemon.lock: {e}"))?;

    // Reuse a live daemon.
    let pid_alive = |pf: &Path| -> bool {
        std::fs::read_to_string(pf)
            .ok()
            .and_then(|t| t.trim().parse::<u32>().ok())
            .is_some_and(|pid| Path::new(&format!("/proc/{pid}")).exists())
    };
    let is_socket = |p: &Path| -> bool {
        use std::os::unix::fs::FileTypeExt as _;
        std::fs::symlink_metadata(p).map(|m| m.file_type().is_socket()).unwrap_or(false)
    };
    if pid_alive(&pid_f) && is_socket(&sock) {
        return Ok(sock.display().to_string());
    }

    // Start a fresh daemon, detached in its OWN process group so it outlives
    // this check AND survives the terminal's ^C/hangup signals (the machine-
    // wide limiter must persist across checks — the shell's `nohup` role).
    // nice/ionice it so its build children (the corpus builds — the real
    // CPU/IO) yield to interactive work; the global budget bounds how MANY run
    // at once. TD_BUILD_JOBS reaches the daemon by plain env inheritance.
    let log = std::fs::File::create(&log_f).map_err(|e| format!("create {}: {e}", log_f.display()))?;
    let log2 = log.try_clone().map_err(|e| format!("clone log fd: {e}"))?;
    let _ = std::fs::remove_file(&sock);
    let tdnice = std::env::var("TD_NICE").unwrap_or_else(|_| s("10"));
    let mut argv: Vec<String> = Vec::new();
    if find_in_path("nice").is_some() {
        argv.extend([s("nice"), s("-n"), tdnice]);
        if find_in_path("ionice").is_some() {
            argv.extend([s("ionice"), s("-c2"), s("-n7")]);
        }
    }
    argv.extend([
        daemon_tb,
        s("daemon"),
        sock.display().to_string(),
        seed_dir,
        store.display().to_string(),
    ]);
    let (head, rest) = argv.split_first().ok_or_else(|| s("internal: empty daemon argv"))?;
    let mut cmd = Command::new(head);
    cmd.args(rest)
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log2));
    {
        use std::os::unix::process::CommandExt as _;
        cmd.process_group(0);
    }
    let mut child = cmd.spawn().map_err(|e| format!("spawn the build daemon: {e}"))?;
    let _ = std::fs::write(&pid_f, format!("{}\n", child.id()));

    // Wait for it to bind the socket.
    for _ in 0..100 {
        if is_socket(&sock) {
            return Ok(sock.display().to_string());
        }
        if child.try_wait().ok().flatten().is_some() {
            let tail = std::fs::read_to_string(&log_f).unwrap_or_default();
            return Err(format!("the daemon exited before binding:\n{tail}"));
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let tail = std::fs::read_to_string(&log_f).unwrap_or_default();
    Err(format!("the daemon did not bind {}:\n{tail}", sock.display()))
}

/// Resolve the guix-free stage0 td-builder (the loop-container provider). Both a harness FETCH
/// (nar-restore) and entering the harness sandbox need it, and it never invokes guix (the stage0
/// warm path spawns none once placed).
fn load_stage0_tb(root: &Path) -> Result<String, String> {
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg(". tests/cache-lib.sh && load_stage0 1>&2 && printf '%s' \"$TB\"")
        .env("TD_STAGE0_BASE", root.join(".td-build-cache/stage0"))
        .current_dir(root);
    let out = run_capture(&mut cmd)
        .map_err(|_| fatal("could not build the guix-free stage0 td-builder."))?;
    Ok(out.trim().to_string())
}

/// A guix-less runner may have an EMPTY .td-build-cache/harness (no local heavy build, and no
/// guix to run one). If a signed substitute store carries the harness (the daily published it,
/// #314), FETCH + verify + restore it here rather than FATALing — tools/resolve-harness.sh checks
/// the ed25519 signature against the pinned tests/td-subst.pub. Best-effort: any MISS (no store,
/// no entry, bad sig, wrong path) leaves the harness absent so the caller fails CLOSED. There is
/// no from-source fallback on a guix-less runner — the fetch IS the provisioning path.
fn try_fetch_harness(root: &Path, hdir: &Path, tb: &str) {
    // Resolve the substitute store exactly as tools/warm-subst.sh does for the toolchain: the
    // daily stashes its td-subst binary + the signed narinfos under ~/.td/subst.
    let store = std::env::var("TD_SUBST_STORE").unwrap_or_else(|_| {
        std::env::var("HOME")
            .map(|h| format!("{h}/.td/subst"))
            .unwrap_or_default()
    });
    if store.is_empty() {
        return;
    }
    let bin = std::env::var("TD_SUBST_BIN").unwrap_or_else(|_| format!("{store}/td-subst"));
    let pub_key = std::env::var("TD_SUBST_PUBKEY")
        .unwrap_or_else(|_| root.join("tests/td-subst.pub").to_string_lossy().into_owned());
    // Cheap negatives — a usable store carries the signed harness narinfo + the pinned anchor.
    // Any missing piece → no fetch (the caller then fails closed). The td-subst bin is NOT
    // file-checked when it is a bare PATH name (resolve-harness just execs $TD_SUBST_BIN, exactly
    // like resolve-toolchain); only a path-shaped bin that is absent is a cheap skip.
    if !Path::new(&store).join("td-harness.narinfo").is_file()
        || !Path::new(&pub_key).is_file()
        || (bin.contains('/') && !Path::new(&bin).is_file())
    {
        return;
    }
    println!(
        ">> check-harness: no local harness — FETCHING the signed /td/store harness from {store} (verified vs {pub_key})"
    );
    let st = Command::new("sh")
        .arg("tools/resolve-harness.sh")
        .arg(hdir)
        .env("TD_SUBST_STORE", &store)
        .env("TD_SUBST_BIN", &bin)
        .env("TD_SUBST_PUBKEY", &pub_key)
        .env("TD_BUILDER", tb)
        .current_dir(root)
        .status();
    match st {
        Ok(s) if s.success() => println!(
            "   check-harness: fetched + restored the /td/store harness to {}",
            hdir.display()
        ),
        _ => eprintln!(
            "   check-harness: no verified harness substitute available at {store} — failing closed"
        ),
    }
}

/// The guix-free `check-harness` tier: enter td's OWN /td/store harness via the
/// stage0 td-builder — handled BEFORE the guix guard/toolchain so this tier
/// never invokes guix (the stage0 warm path spawns none once placed).
fn run_check_harness(root: &Path) -> Result<i32, String> {
    let hdir = root.join(".td-build-cache/harness");
    let rel_f = hdir.join("rel");

    // The guix-free stage0 td-builder: nar-restore tool for a FETCH AND the sandbox provider.
    let tb = load_stage0_tb(root)?;

    // Absent locally? A guix-less runner FETCHES the signed harness from a substitute store (#314)
    // rather than FATALing. Any miss leaves it absent -> the fail-closed message below.
    if !hdir.join("store").is_dir() || !rel_f.is_file() {
        try_fetch_harness(root, &hdir, &tb);
    }
    if !hdir.join("store").is_dir() || !rel_f.is_file() {
        return Err(fatal(&format!(
            "no provisioned /td/store harness at {} (and none fetchable from a substitute store).\n  \
             Provision it on a guix capture host:  td-builder check userland-x86_64-store-native\n  \
             (builds busybox+make + the C toolchain at /td/store, persists them here), then ship the \
             dir to a guix-less VM — or expose the signed substitute store the daily published the \
             harness to (TD_SUBST_STORE + tests/td-subst.pub) so this host FETCHES it.",
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
        match ensure_build_daemon(&root, &tb) {
            Ok(sock) => child_envs.push((s("TD_DAEMON_SOCKET"), sock)),
            Err(e) => eprintln!(
                "td-builder check: WARNING: could not start the shared build daemon \
                 ({e}); corpus gates will fail loudly"
            ),
        }
    }

    // Per-gate cgroup memory limits (issue #328): when the host delegates a
    // writable cgroup-v2 subtree, gate-run gives every gate a child cgroup
    // with memory.max/high — the escape-proof successor to the RSS watchdog
    // (which stays the fallback everywhere else). Deliberately AFTER the
    // daemon warm: the self-move happens here, so the detached persistent
    // build daemon (started above, outliving this check) is NOT captured in
    // this run's host leaf (review finding — it would pin the run dir forever
    // and a recycled pid would then silently lose cgroup mode on EEXIST).
    let cgroup_run = cgroup_delegated_root().and_then(|r| cgroup_run_dir(&r));
    match &cgroup_run {
        Some(dir) => {
            child_envs.push((s("TD_CHECK_CGROUP"), dir.display().to_string()));
        }
        // (The off-knob and first-hop branches already said their piece.)
        None if !matches!(
            std::env::var("TD_CGROUP_ROOT").ok().as_deref(),
            Some("off") | Some("none") | Some("0")
        ) =>
        {
            eprintln!(
                "td-builder check: no delegated cgroup subtree — per-gate tree memory \
                 budgets fall back to the sampling watchdog (delegation setup: issue #328)"
            )
        }
        None => {}
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
    // Best-effort cgroup cleanup: gate leaves are removed by gate-run; the
    // per-run parent goes here (empty by now; a leftover only wastes a dir).
    if let Some(dir) = &cgroup_run {
        // NOTE: this process still SITS in dir/host, so that rmdir fails and
        // the run dir lingers until the process exits — harmless (empty dirs),
        // and the next run uses a fresh pid-keyed dir. Gate leaves go now.
        if let Ok(entries) = std::fs::read_dir(dir) {
            for e in entries.flatten() {
                if e.path().is_dir() {
                    let _ = std::fs::remove_dir(e.path());
                }
            }
        }
        let _ = std::fs::remove_dir(dir);
    }
    let _ = std::io::stdout().flush();
    Ok(st.code().unwrap_or(1))
}

/// `td-builder check-rung HARNESS [ARGS...]` — DEV ITERATION helper (NOT a
/// gate, NOT part of the loop; native since #318 axis 2 — was
/// tools/check-rung.sh). Run a cached-chain bootstrap dev harness INSIDE td's
/// loop sandbox, so sandbox-only failures (no `bzip2`/no `/bin/sh` on PATH,
/// env_clear + C locale, the read-only /gnu/store) surface in MINUTES against
/// the already-built chain in .td-build-cache/ — instead of a ~40-min
/// from-the-seed gate round-trip just to discover a one-line unpack/shebang
/// bug. The dev harnesses otherwise run on the HOST (which has bzip2, /bin/sh,
/// a full locale), so they cannot catch the class of bug that only bites in
/// the sandbox.
///
/// Purely an inner-loop accelerator: the AUTHORITATIVE gate still builds the
/// whole chain from the seed with substitutes off (prime directive 1). Once a
/// harness is green here, run the real `td-builder check bootstrap-<rung>`.
///
/// The sandbox + toolchain provisioning is EXACTLY the loop prelude's (same
/// stage0 container provider, same loop-toolchain list — notably WITHOUT
/// bzip2, so a missing-bzip2 bug still reproduces).
pub fn check_rung_cli(args: &[String]) -> ExitCode {
    match check_rung(args) {
        Ok(code) => ExitCode::from(code.clamp(0, 255) as u8),
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}

fn check_rung(args: &[String]) -> Result<i32, String> {
    let Some((harness, rest)) = args.split_first() else {
        return Err(s("usage: td-builder check-rung HARNESS [ARGS...]"));
    };
    if !Path::new(harness).is_file() {
        return Err(format!("check-rung: no such harness: {harness}"));
    }
    let root = std::env::current_dir().map_err(|e| fatal(&format!("cannot resolve cwd: {e}")))?;
    if !root.join("tests").is_dir() || !root.join("channels.scm").is_file() {
        return Err(fatal("run from the repo root (tests/ + channels.scm not found)"));
    }
    let tb = provision_stage0(&root).map_err(|e| {
        format!("check-rung: FATAL: could not provision the guix-free stage0 td-builder for the sandbox ({e})")
    })?;
    let toolchain = provision_toolchain(&root)?;
    eprintln!(
        ">> check-rung: {harness} inside td-builder host-sandbox (cached chain reused; \
         sandbox env matches the gate)"
    );
    let mut cmd = Command::new(&tb);
    cmd.args(["host-sandbox", "--expose-cwd", "--", "sh"])
        .arg(harness)
        .args(rest)
        .env("PATH", toolchain)
        .env("GUIX_BUILD_OPTIONS", "--no-substitutes --no-offload")
        .current_dir(&root);
    // Replace this process, exactly as the shell helper's `exec` did.
    use std::os::unix::process::CommandExt as _;
    let e = cmd.exec();
    Err(fatal(&format!("check-rung: could not exec the sandbox: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("td-subst-env-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// A usable store: executable td-subst + a narinfo + a non-empty pubkey.
    fn populate(store: &Path, pubkey: &Path) {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::write(store.join("td-subst"), b"#!/bin/sh\n").unwrap();
        std::fs::set_permissions(store.join("td-subst"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        std::fs::write(store.join("x.narinfo"), b"StorePath: /x\n").unwrap();
        std::fs::write(pubkey, b"pinned-trust-anchor\n").unwrap();
    }

    #[test]
    fn subst_env_exposes_a_usable_store() {
        let d = scratch("usable");
        let (store, pubkey) = (d.join("subst"), d.join("td-subst.pub"));
        std::fs::create_dir_all(&store).unwrap();
        populate(&store, &pubkey);
        let envs = subst_env_at(&store, &pubkey);
        let keys: Vec<&str> = envs.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, ["TD_SUBST_BIN", "TD_SUBST_STORE", "TD_SUBST_PUBKEY"]);
        assert!(envs.iter().any(|(k, v)| k == "TD_SUBST_BIN" && v.ends_with("/td-subst")));
    }

    #[test]
    fn subst_env_is_empty_when_any_piece_is_missing() {
        // Each missing piece independently means "expose nothing" — the gate
        // then builds from seed (the substitute is never a correctness dep).
        for missing in ["bin", "exec-bit", "narinfo", "pubkey"] {
            let d = scratch(missing);
            let (store, pubkey) = (d.join("subst"), d.join("td-subst.pub"));
            std::fs::create_dir_all(&store).unwrap();
            populate(&store, &pubkey);
            match missing {
                "bin" => std::fs::remove_file(store.join("td-subst")).unwrap(),
                "exec-bit" => {
                    use std::os::unix::fs::PermissionsExt as _;
                    std::fs::set_permissions(
                        store.join("td-subst"),
                        std::fs::Permissions::from_mode(0o644),
                    )
                    .unwrap();
                }
                "narinfo" => std::fs::remove_file(store.join("x.narinfo")).unwrap(),
                _ => std::fs::write(&pubkey, b"").unwrap(),
            }
            assert!(
                subst_env_at(&store, &pubkey).is_empty(),
                "missing {missing} must expose nothing"
            );
        }
    }
}

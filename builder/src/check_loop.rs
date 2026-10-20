//! check_loop.rs — `td-builder check`: the loop's HOST PRELUDE, ported from the
//! old shell check.sh, which shrank to a guix-free cargo bootstrap shim before
//! being retired outright (human direction 2026-07-03: "I don't want guix
//! anywhere near check.sh" — the host rust toolchain is the part the user
//! brings; everything after `cargo build` is td's own code).
//!
//! What runs here, in order (the exact sequence the shell prelude ran; the
//! rationale comments live with each step):
//!   1. the netns-probe discrimination check,
//!   2. stage0 provisioning (the guix-free loop-container provider, #294),
//!   3. the evaluator placement and the warm prelude (subst store,
//!      source/crate warms),
//!   4. the sandboxed gate run: TB host-sandbox --expose-cwd --no-daemon
//!      --store-item-at SRC DEST -- TB gate-run. The sandbox mounts NO store
//!      directory and NO userland: its one store item is the stage0
//!      td-builder, bound read-only at its /td/store path, and its PATH names
//!      only a directory that does not exist. Every gate body is typed Rust
//!      run by that td-builder; a program it spawns is named by path, or
//!      found on a PATH the body builds itself (cargo-test's toolchain).
//!
//! (The host-guix == pinned-channel integrity guard that used to run `guix
//! describe` was removed in #406 — it only warned on drift, so dropping it is
//! behavior-preserving for a correctly-pinned host and drops one guix subprocess
//! per run.)

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

use crate::host_bin::{arm_check_child, host_cargo_bin, vendor_is_complete, wait_with_deadline};

fn fatal(msg: &str) -> String {
    format!("td-builder check: FATAL: {msg}")
}

/// Exit code `td-builder check` uses when it aborts because THIS HOST cannot
/// provision the loop — no toolchain reachable — as opposed to a gate genuinely
/// going red. It is the stable machine signal the callers read to tell "nothing
/// could run here" from "a real regression", instead of grepping FATAL prose
/// out of the log (the coupling that broke twice — #268, then #315).
/// EX_UNAVAILABLE from sysexits(3): "service unavailable".
///
/// A graph that no host can build is NOT this: that is
/// `EXIT_PROVENANCE_REJECTED`, and it reds.
/// Defined in td-engine so td-recipe-eval speaks the identical contract.
pub use td_engine::exit::EXIT_UNPROVISIONED;

/// The changed paths of a change confined to roster crates, or those crates
/// where the paths cannot travel, space-separated, set by
/// `affected-checks --run` on the `td-builder check` it spawns and forwarded
/// into the gate sandbox: the recipe-checks gate runs only the checks a
/// change to them can reach. `TD_CHECK_FULL` overrides it.
pub const CHECK_SCOPE_ENV: &str = "TD_CHECK_SCOPE";

/// The recipe-checks scope: the space-separated paths or directory names in
/// `TD_CHECK_SCOPE`, unless `TD_CHECK_FULL` is set or the value is empty,
/// which both mean every check. Read here by the loop for the journal key
/// and by the gate for its list, through this one function.
pub(crate) fn check_scope(full: bool, scope: Option<&str>) -> Option<Vec<String>> {
    if full {
        return None;
    }
    let dirs: Vec<String> = scope?.split_whitespace().map(str::to_string).collect();
    (!dirs.is_empty()).then_some(dirs)
}

/// A branch run's mark (td_engine::exit): `affected-checks --run`, and so
/// `ready`, sets it; main-integration scrubs it, and `TD_CHECK_FULL`
/// overrides it.
pub const CHECK_DEFER_ENV: &str = td_engine::exit::CHECK_DEFER_ENV;

/// Whether this run defers engine-only recipe checks to main.
pub(crate) fn check_defers(full: bool, defer: bool) -> bool {
    !full && defer
}

/// The verdict journal key for a run under `scope`: the tree key alone for
/// an unscoped run, and sha256 over the tree key and the scope for a scoped
/// one. A scoped recipe-checks PASS proves less than the gate's name says,
/// so it must not be what an unscoped `--resume` on the same tree finds
/// (review's finding); the same scope on the same tree still finds it. A
/// run that defers proves less in the same way, so it keys apart too.
fn scoped_tree_key(key: &str, scope: Option<&[String]>, defers: bool) -> String {
    if scope.is_none() && !defers {
        return key.to_string();
    }
    let mut h = crate::sha256::Sha256::new();
    h.update(key.as_bytes());
    if let Some(dirs) = scope {
        h.update(b"\0scope\0");
        h.update(dirs.join(" ").as_bytes());
    }
    if defers {
        h.update(b"\0defers-engine\0");
    }
    crate::sha256::to_base16(&h.finalize())
}

/// The error-string half of the same signal, and the exit that prints the
/// sentinel: both in the engine crate's `exit` beside the code, since the
/// build verbs raise them too (see `engine_set`).
use td_engine::exit::{unprovisioned_exit, UNPROVISIONED_TAG};

/// The stderr LOG token every `EXIT_UNPROVISIONED` exit prints (see
/// [`unprovisioned_exit`]). gate-run reads a finished gate's captured log and
/// tolerates a 69 as Unprovisioned ONLY when this exact token is present — proof
/// the 69 came from td's own provisioning path, not a coincidental
/// EX_UNAVAILABLE from an unrelated tool or an accidental `exit 69`. Distinctive
/// on purpose so it never collides with build-log noise (Codex review, re #469).
pub(crate) use td_engine::exit::UNPROVISIONED_SENTINEL;

/// The stable stdout/stderr token for GREEN THAT IS NOT FULL COVERAGE. The gate
/// runner prints it when a run finishes green with ≥1 gate SKIPPED as
/// Unprovisioned; a gate that skipped some of its own sub-checks prints it too,
/// since the property a caller greps for — "this exit 0 did NOT run everything"
/// — is the same one, and one token for one property beats two that can drift.
/// A distinct token from the per-gate [`UNPROVISIONED_SENTINEL`], and a STABLE
/// machine signal rather than FATAL prose — the coupling that broke twice, #268
/// then #315, is exactly what a stable token avoids.
pub(crate) const GATES_SKIPPED_SENTINEL: &str = "[td-gates-skipped:re#469]";

/// The two ways a check can end unhappily. `Unprovisioned` is a RUNNER-setup gap
/// (nothing ran — not a code regression); `Fatal` is every other hard error.
/// `cli()` maps the former to `EXIT_UNPROVISIONED` and the latter to failure, so
/// the distinction survives as an exit code a caller can branch on. A bare
/// `String` (already `fatal()`-prefixed) converts to `Fatal` via `?`.
enum CheckError {
    Unprovisioned(String),
    Fatal(String),
}

impl From<String> for CheckError {
    fn from(s: String) -> Self {
        CheckError::Fatal(s)
    }
}

impl std::fmt::Display for CheckError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CheckError::Unprovisioned(m) | CheckError::Fatal(m) => f.write_str(m),
        }
    }
}

/// First `name` on PATH (the child-spawn resolver `Command` itself uses).
pub(crate) fn find_in_path(name: &str) -> Option<PathBuf> {
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
/// workstream E #294) and return $TB with the canonical store path its package
/// was hashed for (stage0_place's answer, on a memo hit too) — a direct
/// `stage0::stage0_place` call (no ambient host sh anywhere in setup, re #469).
/// The base default matches the gate bodies' PlacedStage0, so the prelude and
/// the gates share one placement.
///
/// A host with no toolchain to compile it is the loop's one provisioning gap
/// (`Unprovisioned`, exit 69: nothing could run here); a toolchain that is
/// present and fails is `Fatal`.
fn provision_stage0(root: &Path) -> Result<(String, String), CheckError> {
    let base = match std::env::var("TD_STAGE0_BASE") {
        Ok(v) if !v.is_empty() => PathBuf::from(v),
        _ => root.join(".td-build-cache/stage0"),
    };
    let cb = crate::stage0::stage0_place(root, &base).map_err(|e| stage0_place_error(&e))?;
    let placed = Path::new(&cb)
        .file_name()
        .ok_or_else(|| fatal("stage0 placement returned a malformed store path"))?;
    let tb = base.join("store").join(placed).join("bin/td-builder");
    if !tb.is_file() {
        return Err(fatal("stage0 provisioning returned no usable $TB").into());
    }
    Ok((tb.display().to_string(), cb))
}

/// A stage0_place failure as the loop reports it: the tagged toolchain gap is
/// `Unprovisioned`, anything else `Fatal`.
fn stage0_place_error(e: &str) -> CheckError {
    let gap = e.strip_prefix(UNPROVISIONED_TAG);
    let msg = fatal(&format!(
        "could not provision the guix-free stage0 td-builder for the loop sandbox ({})",
        gap.unwrap_or(e)
    ));
    match gap {
        Some(_) => CheckError::Unprovisioned(msg),
        None => CheckError::Fatal(msg),
    }
}

/// The loop sandbox's PATH: a directory that does not exist, so a bare-name
/// spawn inside fails rather than searching anywhere.
const NO_PATH: &str = "/path-not-set";

/// td's own store prefix — where the stage0 item is hashed for and must
/// appear inside the sandbox (the recipe graph builds everything with
/// `TD_STORE_DIR=/td/store`). The loop never mounts a store DIRECTORY and
/// never resolves a tool from the host.
const TD_STORE_DIR: &str = "/td/store";

/// The stage0 td-builder as a loop store item: its placement, bound read-only
/// at the canonical /td/store path it was placed for. Static and
/// self-contained, it is the fixture the own-root and sandbox gates run. A
/// placement hashed for another store (a host `TD_STORE_DIR`) is refused
/// rather than bound at a path it was not hashed for.
fn stage0_item(tb: &str, canonical: &str) -> Result<(String, String), String> {
    let pkg = Path::new(tb)
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| fatal(&format!("no stage0 package dir above {tb}")))?;
    let base = pkg
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| fatal(&format!("no stage0 package name for {}", pkg.display())))?;
    let dest = format!("{TD_STORE_DIR}/{base}");
    if canonical != dest {
        return Err(fatal(&format!(
            "the stage0 td-builder was placed for {canonical}, not {dest}; the loop binds \
             it only at the path it was hashed for"
        )));
    }
    Ok((pkg.display().to_string(), dest))
}

/// A timeout(1)-style duration: bare integer seconds or an integer with an
/// s/m/h/d suffix. None on anything else (fractions are out of scope).
/// Shared with the gate runner's TD_CHECK_GATE_TIMEOUT so a duration means one
/// thing across the crate.
pub(crate) fn parse_timeout_secs(v: &str) -> Option<u64> {
    let (num, mult) = match v.chars().last() {
        Some('s') => (v.get(..v.len() - 1)?, 1),
        Some('m') => (v.get(..v.len() - 1)?, 60),
        Some('h') => (v.get(..v.len() - 1)?, 3600),
        Some('d') => (v.get(..v.len() - 1)?, 86400),
        _ => (v, 1),
    };
    num.parse::<u64>().ok().and_then(|n| n.checked_mul(mult))
}

/// The ONE interpretation of TD_WARM_TIMEOUT, shared by every warm step —
/// the `timeout`-wrapped children (warm_argv) and the native crate warm — so
/// the knob means one thing across the prelude: seconds (timeout(1) suffixes
/// s/m/h/d accepted), default 600, `0` disables (None), an unparseable value
/// warns loudly and takes the default rather than silently diverging.
fn warm_timeout_secs() -> Option<u64> {
    let raw = match std::env::var("TD_WARM_TIMEOUT") {
        Ok(v) => v.trim().to_string(),
        Err(_) => return Some(600),
    };
    match parse_timeout_secs(&raw) {
        Some(0) => None,
        Some(n) => Some(n),
        None => {
            eprintln!(
                "td-builder check: TD_WARM_TIMEOUT `{raw}` is not seconds (integer, \
                 s/m/h/d suffix ok) — using the 600s default"
            );
            Some(600)
        }
    }
}

/// Wrap a warm step with `timeout` (warm_timeout_secs) when coreutils timeout
/// exists — one hung mirror must not stall the prelude.
fn warm_argv(base: &[String]) -> Vec<String> {
    match warm_timeout_secs() {
        Some(secs) if find_in_path("timeout").is_some() => {
            let mut v = vec!["timeout".to_string(), secs.to_string()];
            v.extend(base.iter().cloned());
            v
        }
        _ => base.to_vec(),
    }
}

fn spawn_argv(
    argv: &[String],
    root: &Path,
    envs: &[(String, String)],
) -> Option<std::process::Child> {
    let (head, rest) = argv.split_first()?;
    let mut cmd = Command::new(head);
    cmd.args(rest).current_dir(root);
    for (k, v) in envs {
        cmd.env(k, v);
    }
    arm_check_child(&mut cmd);
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
    let Some((head, rest)) = wrapped.split_first() else {
        return String::new();
    };
    let mut cmd = Command::new(head);
    cmd.args(rest).current_dir(root).stderr(Stdio::null());
    for (k, v) in envs {
        cmd.env(k, v);
    }
    arm_check_child(&mut cmd);
    cmd.output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

fn s(v: &str) -> String {
    v.to_string()
}

/// sha256 of a file's bytes, streamed rather than read whole. None when it
/// cannot be read.
pub(crate) fn file_digest(path: &Path) -> Option<Vec<u8>> {
    let mut file = std::fs::File::open(path).ok()?;
    let mut h = crate::sha256::Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = std::io::Read::read(&mut file, &mut buf).ok()?;
        if n == 0 {
            break;
        }
        h.update(buf.get(..n)?);
    }
    Some(h.finalize().to_vec())
}

/// The working-tree content key for the verdict journal (issue #320): sha256
/// over HEAD's tree + the full dirty diff + every untracked file's bytes —
/// ANY content change yields a new key, so a --resume skip can never survive
/// an edit (whole-tree invalidation, deliberately no per-gate cleverness) —
/// and over the bytes of `gate_runner`, the td-builder whose compiled-in
/// gate bodies run them. HEAD's tree, not HEAD: no gate reads history or a
/// commit message, so an amend that changes only the message keeps the key
/// and `ready` reuses what already passed. None when git is unavailable
/// (resume then refuses to run).
pub(crate) fn tree_key(root: &Path, gate_runner: &Path) -> Option<String> {
    let git = |args: &[&str]| -> Option<Vec<u8>> {
        let mut cmd = Command::new("git");
        cmd.args(args).current_dir(root);
        arm_check_child(&mut cmd);
        let out = cmd.output().ok()?;
        if !out.status.success() {
            return None;
        }
        Some(out.stdout)
    };
    // The runner enters as its own fixed-length digest, resolved as the
    // loop runs it, from `root`.
    let runner = file_digest(&root.join(gate_runner))?;
    let mut h = crate::sha256::Sha256::new();
    h.update(&runner);
    h.update(&git(&["rev-parse", "HEAD^{tree}"])?);
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
/// native since #318 axis 2 — was tools/warm-subst.sh): if a persistent signed
/// substitute store exists (~/.td/subst: a stashed td-subst binary + published
/// closure narinfos), expose TD_SUBST_* to the loop sandbox (host-sandbox binds
/// ~/.td/subst ro + preserves TD_SUBST_*). The toolchain gates then FETCH the
/// lock-keyed closure instead of rebuilding ~98 min from seed, FALLING BACK to
/// from-seed on ANY miss. This NEVER fetches or builds td-subst; a COLD machine
/// exposes nothing and the gate builds from seed (the substitute is an
/// optimization, never a correctness dependency).
///
/// NOTE: nothing in-tree PRODUCES that store today — the publisher retired with
/// the per-rung shell gates (#444) and the daily runner that drove it. This is
/// the consumer half, kept live and inert until a producer returns.
fn subst_env(root: &Path) -> Vec<(String, String)> {
    let store = match std::env::var("TD_SUBST_STORE") {
        Ok(v) if !v.trim().is_empty() => PathBuf::from(v),
        _ => match std::env::var("HOME") {
            Ok(h) => Path::new(&h).join(".td/subst"),
            Err(_) => return Vec::new(),
        },
    };
    subst_env_at(&store, &root.join("tests/td-subst.pub"))
}

/// A USABLE store = a stashed td-subst binary + at least one signed narinfo +
/// the pinned trust anchor. Any missing piece => expose nothing.
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
        .map(|rd| {
            rd.flatten()
                .any(|e| e.path().extension().is_some_and(|x| x == "narinfo"))
        })
        .unwrap_or(false);
    if !has_narinfo {
        return Vec::new();
    }
    if !std::fs::metadata(pubkey)
        .map(|m| m.is_file() && m.len() > 0)
        .unwrap_or(false)
    {
        return Vec::new();
    }
    vec![
        (s("TD_SUBST_BIN"), bin.display().to_string()),
        (s("TD_SUBST_STORE"), store.display().to_string()),
        (s("TD_SUBST_PUBKEY"), pubkey.display().to_string()),
    ]
}

/// A gate's cached td-built binary out of its newstore dir: the
/// lexicographically-first EXECUTABLE `<newstore>/*/bin/<bin>` — the
/// deterministic pick the shell's `ls | head -1` made, with the shell's `-x`
/// requirement kept so a permission-mangled cache entry falls through to the
/// cargo fallback instead of failing every spawn. None when the cache is cold.
fn newstore_bin(root: &Path, newstore_rel: &str, bin: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::read_dir(root.join(newstore_rel))
        .ok()?
        .flatten()
        .map(|e| e.path().join("bin").join(bin))
        .filter(|p| {
            std::fs::metadata(p)
                .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
                .unwrap_or(false)
        })
        .min()
}

/// Build the merged `net` control-plane crate (td-net) with the host cargo, statically,
/// then return an APPLET-named link to it (td-fetch/td-feed/td-subst) beside the binary —
/// so argv[0] basename multicall dispatch selects the applet with the caller's argv
/// UNCHANGED. One td-net build is shared across the applets. None on any build/link
/// failure (best-effort warm; the gates enforce presence).
fn host_net_applet(root: &Path, applet: &str, deadline: Option<Instant>) -> Option<PathBuf> {
    let td_net = host_cargo_bin(root, "net", "td-net", deadline)?;
    let link = td_net.with_file_name(applet);
    if link == td_net {
        return Some(link);
    }
    // Point the applet link at the freshly built td-net. A RELATIVE target keeps
    // /proc/self/exe resolving to td-net — feed's daemon self-spawn relies on that
    // (net/src/feed.rs). Do it race-free: concurrent preludes warm the same target dir, so a
    // naive remove-then-symlink lets one caller observe the other's half-done state and
    // spuriously skip. Fast-path an already-correct link; else create at a per-process temp
    // name and rename() over the destination (atomic replace of any stale/dangling link); on a
    // lost race, accept the peer's correct link.
    let want = Path::new("td-net");
    if std::fs::read_link(&link).ok().as_deref() == Some(want) {
        return Some(link);
    }
    let tmp = link.with_file_name(format!(".{applet}.tmp.{}", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    if let Err(e) = std::os::unix::fs::symlink(want, &tmp) {
        eprintln!(
            "td-builder check: net applet link {}: {e} — skipping host build",
            link.display()
        );
        return None;
    }
    if let Err(e) = std::fs::rename(&tmp, &link) {
        let _ = std::fs::remove_file(&tmp);
        if std::fs::read_link(&link).ok().as_deref() == Some(want) {
            return Some(link);
        }
        eprintln!(
            "td-builder check: net applet link {}: {e} — skipping host build",
            link.display()
        );
        return None;
    }
    Some(link)
}

/// Prepare the admitted crypto/SQLite union before offline host or sandbox Cargo.
pub(crate) fn warm_mail_sources(root: &Path) -> Result<(), String> {
    crate::crypto_build::validate(root)?;
    if vendor_is_complete(root, "td-mta", Some("td-mta/Cargo.lock")) {
        return Ok(());
    }
    let deadline = warm_timeout_secs().map(|n| Instant::now() + Duration::from_secs(n));
    let feed = newstore_bin(root, ".td-build-cache/td-feed/sd/newstore", "td-feed")
        .or_else(|| host_net_applet(root, "td-feed", deadline))
        .ok_or("cannot prepare native mail sources: td-feed unavailable")?;
    let mut command = Command::new(feed);
    command
        .args(["warm", "crate-local", "td-mta", "td-mta"])
        .stdin(Stdio::null())
        .current_dir(root);
    arm_check_child(&mut command);
    let mut child = command
        .spawn()
        .map_err(|e| format!("prepare native mail sources: {e}"))?;
    if !wait_with_deadline(&mut child, deadline)
        || !vendor_is_complete(root, "td-mta", Some("td-mta/Cargo.lock"))
    {
        return Err("native mail source preparation incomplete".into());
    }
    Ok(())
}

/// Native mail tests alone need the real fetch fixture's locked sources.
pub(crate) fn warm_mail_client_sources(root: &Path) -> Result<(), String> {
    warm_mail_sources(root)?;
    warm_td_crate_closure(root);
    if !vendor_is_complete(root, "td-net", Some("net/Cargo.lock")) {
        return Err("native mail fetch fixture source preparation incomplete".into());
    }
    Ok(())
}

/// Prepare the target td-net recipe's canonical vendor set before the gates
/// run. Use the same td-feed operation and recipe lock as the later warm
/// prelude; an already prepared VM cache needs no registry access.
/// A cold cache uses the current source-built host applet so every writer takes
/// the preparation lock. Its Cargo build and warming share one best-effort
/// budget; this ordinary producer warm does not retain partial-job progress.
fn warm_td_crate_closure(root: &Path) {
    warm_td_crate_closure_resolving(root, |deadline| host_net_applet(root, "td-feed", deadline));
}

fn warm_td_crate_closure_resolving(
    root: &Path,
    resolve: impl FnOnce(Option<Instant>) -> Option<PathBuf>,
) {
    // Match td-feed's warm fast path before resolving a tool: resolution itself
    // can run host Cargo. The recipe stager still verifies the archive bytes.
    if vendor_is_complete(root, "td-net", Some("net/Cargo.lock")) {
        return;
    }
    let deadline = warm_timeout_secs().map(|n| Instant::now() + Duration::from_secs(n));
    let Some(feed) = resolve(deadline) else {
        eprintln!(
            "td-builder check: warm td-net crates: no td-feed binary — skipping (PREP best-effort)"
        );
        return;
    };
    let mut command = Command::new(feed);
    command
        .args(["warm", "crate-local", "net", "td-net"])
        .stdin(Stdio::null())
        .current_dir(root);
    arm_check_child(&mut command);
    let complete = match command.spawn() {
        Ok(mut child) => wait_with_deadline(&mut child, deadline),
        Err(_) => false,
    };
    if !complete || !vendor_is_complete(root, "td-net", Some("net/Cargo.lock")) {
        eprintln!("td-builder check: warm td-net crates: preparation incomplete; the recipe gate enforces presence");
    }
}

/// Generate the host-produced Linux UAPI header seeds
/// (`$HOME/.td/sources/linux-headers-<ver>-{i386,x86_64}.tar`) the toolchain
/// rungs intern, before any gate interns them (heavy_warms runs later).
///
/// `newstore_bin` picks a td-built `td-feed` by store hash, so a pre-#536 binary
/// that still writes the seed as `.tar.gz` can win — it exits 0 yet the `.tar` the
/// intern needs never appears. So verify the `.tar` landed and retry any missing
/// arch once with the current source-built `feed/`. Best-effort and idempotent;
/// the intern is the fail-closed enforcement (issue #546).
fn warm_kernel_headers_seed(root: &Path) {
    const ARCHES: &[&str] = &["i386", "x86_64"];
    // Bound the cargo fallback like warm_td_crate_closure; the warm itself is
    // already `timeout`-wrapped.
    let deadline = warm_timeout_secs().map(|n| Instant::now() + Duration::from_secs(n));

    let newstore = newstore_bin(root, ".td-build-cache/td-feed/sd/newstore", "td-feed");
    let primary = match newstore.clone() {
        Some(p) => p,
        None => match host_net_applet(root, "td-feed", deadline) {
            Some(p) => p,
            None => {
                eprintln!(
                    "td-builder check: no td-feed binary to warm the kernel-headers seed \
                     — skipping (best-effort; the loop intern enforces presence)"
                );
                return;
            }
        },
    };

    let missing = warm_kh_arches(root, &primary, ARCHES);
    if missing.is_empty() {
        return;
    }
    // Only a stale newstore pick is worth retrying against the source build; a
    // source build already IS this tree.
    if newstore.is_none() {
        eprintln!(
            "td-builder check: kernel-headers seed still absent for {} — the loop intern \
             will report it",
            missing.join(", ")
        );
        return;
    }
    let Some(fresh) = host_net_applet(root, "td-feed", deadline) else {
        eprintln!(
            "td-builder check: kernel-headers seed still absent for {} and no source-built \
             td-feed to retry — the loop intern will report it",
            missing.join(", ")
        );
        return;
    };
    let missing_refs: Vec<&str> = missing.iter().map(String::as_str).collect();
    let still = warm_kh_arches(root, &fresh, &missing_refs);
    if !still.is_empty() {
        eprintln!(
            "td-builder check: kernel-headers seed still absent for {} after retry — the \
             loop intern will report it",
            still.join(", ")
        );
    }
}

/// Warm the kernel-headers seed for each arch, returning the arches whose `.tar` is
/// still absent afterwards (a warm can exit 0 yet leave a pre-#536 `.tar.gz`).
fn warm_kh_arches(root: &Path, tdfeed: &Path, arches: &[&str]) -> Vec<String> {
    let tdfeed = tdfeed.display().to_string();
    // The warm resolves rust/cc via `$TD_BUILDER_SELF provision-*`
    // (tests/recipe-eval-tool.sh). Set it only when current_exe() resolves, so a
    // failure inherits any ambient value rather than clobbering it with "".
    let mut envs = vec![(s("TD_ROOT"), root.display().to_string())];
    if let Ok(exe) = std::env::current_exe() {
        envs.push((s("TD_BUILDER_SELF"), exe.display().to_string()));
    }
    let mut missing = Vec::new();
    for arch in arches {
        if !warm_status(
            &[tdfeed.clone(), s("warm"), s("kernel-headers"), s(arch)],
            root,
            &envs,
        ) {
            eprintln!(
                "td-builder check: kernel-headers seed warm (best-effort) failed/timed out \
                 for {arch}"
            );
        }
        if !kh_seed_present(&crate::bootstrap::shared_sources_dir(), arch) {
            missing.push((*arch).to_string());
        }
    }
    missing
}

/// True when the uncompressed `.tar` seed for `arch` is present in the shared sources cache
/// (matched by name, version-independent; a `.tar.gz` is NOT a match — the intern needs the
/// `.tar`).
fn kh_seed_present(sources: &Path, arch: &str) -> bool {
    let suffix = format!("-{arch}.tar");
    std::fs::read_dir(sources)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .any(|n| n.starts_with("linux-headers-") && n.ends_with(&suffix))
}

fn heavy_warms(root: &Path, eval: Option<&str>) {
    // The td-net crate closure (td-feed) is warmed earlier
    // (warm_td_crate_closure) — not here.

    // Resolve ONE host td-feed binary: the gate's td-built one, else a host
    // cargo build of feed/.
    let newstore = newstore_bin(root, ".td-build-cache/td-feed/sd/newstore", "td-feed");
    let Some(tdfeed) = newstore
        .clone()
        .or_else(|| host_net_applet(root, "td-feed", None))
    else {
        eprintln!(
            "td-builder check: no td-feed binary for the heavy warm (build feed/ with cargo) — \
             skipping (best-effort; the heavy gates enforce presence)"
        );
        return;
    };
    let tdfeed = tdfeed.display().to_string();

    // `td-feed warm sources` (serial-first). Ordinary invocations start/reuse
    // the shared feed daemon. The rootless check host sets
    // TD_FEED_NO_DAEMON: a daemon created inside the check PID namespace could
    // not persist safely, so ensure-serve fails fast and warm sources uses its
    // verified, constant-memory direct-fetch fallback.
    let mut src_envs = vec![(s("TD_ROOT"), root.display().to_string())];
    // `warm sources` resolves the recipe-owned pins by BUILDING td-recipe-eval through
    // tests/recipe-eval-tool.sh, which hard-requires TD_BUILDER_SELF; without it the
    // warm dies on the unset variable before fetching anything. Guarded as
    // warm_kh_arches does, so an unresolvable current_exe() inherits rather than
    // clobbers.
    if let Ok(exe) = std::env::current_exe() {
        src_envs.push((s("TD_BUILDER_SELF"), exe.display().to_string()));
    }
    let faddr = warm_capture(&[tdfeed.clone(), s("ensure-serve")], root, &[]);
    if !faddr.is_empty() {
        src_envs.push((s("TD_FEED_BASE"), format!("http://{faddr}")));
    }
    if !warm_status(&[tdfeed.clone(), s("warm"), s("sources")], root, &src_envs) {
        eprintln!(
            "td-builder check: warm sources failed — the recipe-owned source pins may be cold \
             (best-effort; the gates enforce presence)"
        );
    }
    // The Debian build root's layer and .debs (build_root.rs), which `warm
    // sources` leaves alone; the build-root recipes' checks assemble from them.
    if !warm_status(
        &[tdfeed.clone(), s("warm"), s("build-root")],
        root,
        &src_envs,
    ) {
        eprintln!(
            "td-builder check: warm build-root failed — the build root's pins may be cold \
             (best-effort; `td-builder build-root assemble` enforces presence)"
        );
    }

    // Corpus crate warms are independent, but compiler memory is the limiting
    // resource on the hosts this scheduler protects.
    let warm_jobs = crate::check_memory::build_jobs().min(4);
    let specs: [&[&str]; 6] = [
        &["warm", "crate", "sd", "1.0.0"],
        &["warm", "crate", "procs", "0.14.10"],
        &["warm", "crate", "eza", "0.21.6"],
        &["warm", "crate", "bat", "0.25.0"],
        &["warm", "crate", "youki", "0.6.0"],
        &["warm", "crate", "uu_cat", "0.9.0", "cat"],
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
    warm_vendor_closures(root, eval, &tdfeed, &envs, newstore.is_some(), warm_jobs);
}

/// Vendor the locked crate closure of every rust rung the recipe catalog
/// declares a `Cargo.lock` for.
///
/// The catalog already derives these argvs (`vendor_warm_args`), but only two
/// things drove them: the explicit `td-recipe-eval warm`, and
/// `warm_operator_inputs`, which is gated on an INTERACTIVE stdin
/// (`check_runner.rs`). `td-builder check` is neither, so under `check` nothing
/// vendored a rung's closure: `provision_auto_vendor` requires
/// `crate-vendor/<stem>/vendor` whenever the committed lock has any registry
/// package (`stage_verified_vendor`'s `allow_missing` is `registry == 0`), so
/// such a rung's recipe check failed on every fresh worktree, on unmodified
/// main, for a reason no diff introduced.
///
/// The job list is ASKED FOR, not restated here: the pin, its hash, the
/// committed lock and the stem are recipe data, and a second copy in the
/// prelude is drift this file could not notice.
fn warm_vendor_closures(
    root: &Path,
    eval: Option<&str>,
    tdfeed: &str,
    envs: &[(String, String)],
    tdfeed_from_newstore: bool,
    warm_jobs: usize,
) {
    let Some(eval) = built_recipe_eval(root, eval) else {
        eprintln!(
            "td-builder check: vendor warm (best-effort) skipped -- no built \
             td-recipe-eval to derive the crate closures"
        );
        return;
    };
    let listing = warm_capture(
        &[eval.display().to_string(), s("vendor-warm-args")],
        root,
        envs,
    );
    // Each line is the warm argv with the LOCK appended: `warm ... <dest> <lock>`.
    // The lock is what td-feed stamps into the completion marker, so taking it
    // from the same line is what keeps this reader's verdict and td-feed's the
    // same verdict.
    let jobs: Vec<Vec<String>> = listing
        .lines()
        .map(|line| line.split('\t').map(s).collect::<Vec<String>>())
        .filter(|fields| fields.len() >= 4)
        .collect();
    if jobs.is_empty() {
        // warm_capture returns "" for a failed spawn, a non-zero exit and an
        // empty listing alike, and discards the child's stderr -- so name all
        // three rather than blame the recipes for a stale evaluator.
        eprintln!(
            "td-builder check: vendor warm (best-effort) skipped -- \
             `{} vendor-warm-args' printed nothing, or could not run (an \
             evaluator predating the verb reports it as unknown)",
            eval.display()
        );
        return;
    }
    let cold = run_vendor_warms(root, tdfeed, envs, &jobs, warm_jobs);
    if cold.is_empty() {
        return;
    }
    // Retry only what a DIFFERENT binary could plausibly fix: a warm that
    // rejected the verb exits non-zero. A clean exit that left no marker is
    // td-feed declining on purpose (an absent pinned archive), and 124 is the
    // timeout -- neither is helped by rebuilding feed/ and spending a second
    // budget on it.
    let retry: Vec<Vec<String>> = cold
        .iter()
        .filter(|c| c.code.is_some_and(|code| code != 0 && code != 124))
        .map(|c| c.job.clone())
        .collect();
    // A stale newstore td-feed may predate a warm verb and reject it, exiting
    // non-zero without writing a vendor dir. Same shape as the
    // kernel-headers seed (issue #546): build the source `feed/` once and retry
    // only what is still absent. A source build already IS this tree, so a
    // non-newstore pick has nothing better to retry with.
    if retry.is_empty() || !tdfeed_from_newstore {
        report_cold_vendors(&cold, "");
        return;
    }
    let deadline = warm_timeout_secs().map(|n| Instant::now() + Duration::from_secs(n));
    let Some(fresh) = host_net_applet(root, "td-feed", deadline) else {
        report_cold_vendors(&cold, " and no source-built td-feed to retry with");
        return;
    };
    let still = run_vendor_warms(root, &fresh.display().to_string(), envs, &retry, warm_jobs);
    report_cold_vendors(&still, " after retry");
}

/// Run each warm argv, `warm_jobs` at a time, and return the ones whose vendor
/// is still not COMPLETE afterwards. Re-surveys rather than trusting the exit
/// status, like `warm_kernel_headers_seed`: a warm can exit 0 and still leave
/// nothing the consumer accepts.
fn run_vendor_warms(
    root: &Path,
    tdfeed: &str,
    envs: &[(String, String)],
    jobs: &[Vec<String>],
    warm_jobs: usize,
) -> Vec<ColdVendor> {
    let mut codes: Vec<(String, Option<i32>)> = Vec::new();
    // The dest travels WITH the child, rather than being read back off the
    // spawned argv: that argv has had the trailing lock stripped for td-feed, so
    // `vendor_dest`'s "field before the lock" would name the field before the
    // dest instead. Keying the exit statuses under that would leave every code
    // unfindable, and the retry below — the reason the statuses are collected —
    // permanently empty (review finding).
    let mut running: Vec<(std::process::Child, Vec<String>, String)> = Vec::new();
    let drain = |running: &mut Vec<(std::process::Child, Vec<String>, String)>,
                 codes: &mut Vec<(String, Option<i32>)>| {
        for (mut child, argv, dest) in running.drain(..) {
            let status = child.wait().ok();
            let code = status.and_then(|st| st.code());
            if !status.is_some_and(|st| st.success()) {
                eprintln!(
                    "td-builder check: vendor warm (best-effort) failed/timed out \
                     (raise TD_WARM_TIMEOUT if the closure is large): {}",
                    argv.join(" ")
                );
            }
            codes.push((dest, code));
        }
    };
    for job in jobs {
        let mut argv = vec![tdfeed.to_string()];
        argv.extend(vendor_argv(job).iter().cloned());
        let wrapped = warm_argv(&argv);
        if let Some(child) = spawn_argv(&wrapped, root, envs) {
            running.push((child, argv, vendor_dest(job).to_string()));
        } else {
            eprintln!(
                "td-builder check: vendor warm (best-effort) could not spawn: {}",
                argv.join(" ")
            );
            codes.push((vendor_dest(job).to_string(), None));
        }
        if running.len() >= warm_jobs.max(1) {
            drain(&mut running, &mut codes);
        }
    }
    drain(&mut running, &mut codes);
    jobs.iter()
        .filter(|job| !vendor_is_complete(root, vendor_dest(job), vendor_lock(job)))
        .map(|job| ColdVendor {
            code: codes
                .iter()
                .find(|(dest, _)| dest == vendor_dest(job))
                .and_then(|(_, code)| *code),
            job: job.clone(),
        })
        .collect()
}

/// A vendor that is still not complete, with the exit status of the warm that
/// was supposed to fill it -- the two together are what decide a retry.
struct ColdVendor {
    job: Vec<String>,
    code: Option<i32>,
}

fn report_cold_vendors(cold: &[ColdVendor], suffix: &str) {
    for c in cold {
        eprintln!(
            "td-builder check: crate vendor for {} still incomplete{suffix} -- the \
             recipe check will report it",
            vendor_dest(&c.job)
        );
    }
}

/// The recipe stem a vendor warm targets: the field before the trailing lock.
fn vendor_dest(job: &[String]) -> &str {
    job.len()
        .checked_sub(2)
        .and_then(|i| job.get(i))
        .map(String::as_str)
        .unwrap_or("?")
}

/// The lock td-feed pins this rung's closure with, and stamps into the marker.
fn vendor_lock(job: &[String]) -> Option<&str> {
    job.last().map(String::as_str)
}

/// The argv itself, without the trailing lock this side reads.
fn vendor_argv(job: &[String]) -> &[String] {
    job.split_last().map(|(_, rest)| rest).unwrap_or(&[])
}

/// An already-built `td-recipe-eval`, WITHOUT building one: the operator
/// override, else `placed`, what `recipe_eval_place` built from this tree in
/// this run, else the workspace binary. Not the placement sentinel: a run
/// whose placement hit a toolchain gap leaves it naming an older tree.
///
/// Deliberately no `cargo` fallback. `bootstrap::recipe_source_pins_output`
/// has one, and it compiles the recipes crate into a target dir of its own --
/// a second full build tree, unbounded and unarmed, which a best-effort warm
/// cannot afford.
fn built_recipe_eval(root: &Path, placed: Option<&str>) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt as _;
    let executable = |p: &Path| {
        std::fs::metadata(p)
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    };
    if let Some(v) = std::env::var_os("TD_RECIPE_EVAL") {
        let p = PathBuf::from(v);
        if executable(&p) {
            return Some(p);
        }
    }
    if let Some(placed) = placed.map(PathBuf::from).filter(|p| executable(p)) {
        return Some(placed);
    }
    let path = root.join("target/release/td-recipe-eval");
    executable(&path).then_some(path)
}

pub fn cli(args: &[String]) -> ExitCode {
    if matches!(args, [flag] if matches!(flag.as_str(), "-h" | "--help")) {
        println!("usage: td-builder check [-j N] [--resume] [GOAL...]");
        println!(
            "Runs through a lazy per-user memory host shared by concurrent worktrees; no sudo or setup step is required."
        );
        return ExitCode::SUCCESS;
    }
    match run(args) {
        Ok(code) => ExitCode::from(code.clamp(0, 255) as u8),
        Err(e @ CheckError::Unprovisioned(_)) => {
            eprintln!("{e}");
            unprovisioned_exit()
        }
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &[String]) -> Result<i32, CheckError> {
    let root = std::env::current_dir().map_err(|e| fatal(&format!("cannot resolve cwd: {e}")))?;
    if !root.join("tests").is_dir() {
        return Err(fatal("run from the repo root (tests/ not found)").into());
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
                _ => {
                    return Err(
                        fatal(&format!("bad {a} value — -j needs a positive integer")).into(),
                    )
                }
            }
        } else if a == "--resume" {
            resume = true;
        } else if a.starts_with('-') {
            return Err(fatal(&format!(
                "unknown flag `{a}` — td-builder check takes goals (tiers/gate names), \
                 -j N, and --resume; there is no make behind this anymore"
            ))
            .into());
        } else {
            goals.push(a.clone());
        }
    }
    // The integration tier is no gate: it runs on the host, after the
    // gates, so it leaves the goals the sandbox is handed.
    let integration = goals.iter().any(|g| g == crate::integration::GOAL);
    goals.retain(|g| g != crate::integration::GOAL);
    if integration
        && goals
            .iter()
            .any(|g| matches!(g.as_str(), "list-gates" | "gate-timing-report"))
    {
        return Err(fatal(&format!(
            "{} runs the oracles; it does not combine with a listing goal",
            crate::integration::GOAL
        ))
        .into());
    }
    if integration && goals.is_empty() {
        // The tier alone runs no gate, so it provisions nothing the sandbox
        // needs: only the evaluator that drives the oracles.
        let placed =
            crate::stage0::recipe_eval_place(&root, &root.join(".td-build-cache/recipe-eval"));
        return integration_tier(&root, placed);
    }
    if goals.is_empty() {
        goals.push("check".to_string());
    }

    guard_netns_probe()?;

    // No guix process, no host tool and no userland: the loop PATH is empty,
    // and gate work is td-builder's typed Rust or a program named by its
    // absolute store path.

    // Light tiers own no heavy gate — skip the heavy warms + daemon (exactly the
    // shell prelude's goal scan).
    let heavy_warm = goals.iter().any(|g| {
        !matches!(
            g.as_str(),
            "check-fast" | "check-engine" | "list-gates" | "gate-timing-report"
        )
    });

    let (tb, stage0_canon) = provision_stage0(&root)?;
    // Place the EVALUATOR here too, for the same reason as stage0: the gates run
    // in the sandbox, which has no toolchain, so a build inside it cannot
    // succeed. Seeding the memo out here is what lets a warm tree reuse it in
    // there.
    //
    // ONLY a toolchain gap is tolerated. A host that HAS a toolchain and still
    // fails to build — a recipes regression, a non-static link — must red here:
    // swallowing it would let the sandbox retry, get 69 for want of a compiler,
    // and report the regression as a tolerated skip.
    // ... but a goal that only PRINTS needs no evaluator, and must not pay a
    // compile — or die on an unrelated fault — to list gates.
    let listing_only = goals
        .iter()
        .all(|g| matches!(g.as_str(), "list-gates" | "gate-timing-report"));
    let placed = if listing_only {
        Ok(String::new())
    } else {
        crate::stage0::recipe_eval_place(&root, &root.join(".td-build-cache/recipe-eval"))
    };
    if let Err(e) = &placed {
        match e.strip_prefix(UNPROVISIONED_TAG) {
            Some(why) => eprintln!(
                "td-builder check: no td-recipe-eval for the gates — {why} \
                 (they degrade to their own Unprovisioned skip)"
            ),
            None => {
                return Err(CheckError::Fatal(fatal(&format!(
                    "could not build td-recipe-eval for the gates ({e})"
                ))))
            }
        }
    }
    // Best-effort and idempotent; the gates that intern these enforce their
    // presence. Only the heavy tiers have a consumer, and the header warm
    // resolves (and may build) td-feed before it can tell it has nothing to do.
    if heavy_warm {
        warm_td_crate_closure(&root);
        warm_kernel_headers_seed(&root);
    }
    let disabled = std::env::var("TD_CHECK_DISABLE").unwrap_or_default();
    if !listing_only && crate::gates::goals_include(&goals, "cargo-test", &disabled)? {
        if let Err(e) = warm_mail_client_sources(&root) {
            eprintln!("td-builder check: native mail source preparation: {e}; the cargo gate enforces presence when provisioned");
        }
    }
    // A directory that does not exist, as the derivation jail sets: host-sandbox
    // forwards this PATH, an unset one would forward the host's, and an empty
    // one is searched as the current directory, which here is the worktree.
    let mut child_envs: Vec<(String, String)> = vec![(s("PATH"), s(NO_PATH))];
    // TD_CHECK_DISABLE forwards the gate-disable list (gate names / `pool:<name>`
    // tokens) so `TD_CHECK_DISABLE=… td-builder check` reaches the in-sandbox runner.
    // Every build uses the one shared warm ladder (re #469) — there is no
    // cold-cache toggle to forward; `td-recipe-eval clear-store` is the only way
    // to force a cold climb.
    // TD_CHECK_FULL is forwarded as well: inside the sandbox it is what makes
    // a recipe check run in full rather than answer from its verdict memo
    // (check_runner.rs), the same knob that ignores --resume below. So is
    // TD_CHECK_SCOPE, the paths (or crates) a change is confined to, which
    // the recipe-checks gate narrows its list by (gate_bodies.rs), and
    // TD_CHECK_DEFER_ENGINE, a branch run's mark that leaves engine-only
    // recipe checks to main (check_runner.rs).
    for k in [
        "TD_CHECK_DISABLE",
        "TD_CHECK_FULL",
        CHECK_SCOPE_ENV,
        CHECK_DEFER_ENV,
    ] {
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
    let scope = check_scope(
        std::env::var_os("TD_CHECK_FULL").is_some(),
        std::env::var(CHECK_SCOPE_ENV).ok().as_deref(),
    );
    let defers = check_defers(
        std::env::var_os("TD_CHECK_FULL").is_some(),
        std::env::var_os(CHECK_DEFER_ENV).is_some(),
    );
    match tree_key(&root, Path::new(&tb)) {
        Some(key) => child_envs.push((
            s("TD_CHECK_TREE"),
            scoped_tree_key(&key, scope.as_deref(), defers),
        )),
        None if resume => {
            return Err(fatal(
                "--resume needs a git working tree and a readable gate runner to key the verdict journal, and `git` or reading the runner failed here — cannot prove the tree is unchanged, refusing to skip",
            )
            .into())
        }
        None => {}
    }
    child_envs.extend(subst_env(&root));

    if heavy_warm {
        let eval = placed.as_deref().ok().filter(|p| !p.is_empty());
        heavy_warms(&root, eval);
    }

    let host_jobs = crate::check_memory::hosted_gate_capacity();
    let jobs = jobs_flag.map_or(host_jobs, |requested| requested.min(host_jobs));

    // nice/ionice the whole loop so it yields to interactive work; the host's
    // memory tokens bound how much runs, while nice bounds its priority.
    let tdnice = std::env::var("TD_NICE").unwrap_or_else(|_| "10".to_string());
    let mut argv: Vec<String> = Vec::new();
    if let Some(nice) = find_in_path("nice") {
        argv.extend([nice.display().to_string(), s("-n"), tdnice]);
        if let Some(ionice) = find_in_path("ionice") {
            argv.extend([ionice.display().to_string(), s("-c2"), s("-n7")]);
        }
    }
    // The sandbox mounts NO store directory and NO host tool — only the
    // stage0 item, bound read-only (the drv build jail's input-only model).
    let (stage0_src, stage0_dest) = stage0_item(&tb, &stage0_canon)?;
    argv.extend([
        tb.clone(),
        s("host-sandbox"),
        s("--expose-cwd"),
        s("--no-daemon"),
        s("--store-item-at"),
        stage0_src,
        stage0_dest,
    ]);
    argv.extend([s("--"), tb, s("gate-run")]);
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
    crate::sandbox::die_with_parent(&mut cmd);
    let st = cmd
        .status()
        .map_err(|e| fatal(&format!("could not start the loop sandbox: {e}")))?;
    let code = st.code().unwrap_or(1);
    let _ = std::io::stdout().flush();
    if !integration {
        return Ok(code);
    }
    if code != 0 {
        // A red or unprovisioned gate already says the image is not worth
        // booting; say the tier was not reached rather than leave it silent.
        println!(
            ">> {}: not run — the gates exited {code}",
            crate::integration::GOAL
        );
        return Ok(code);
    }
    integration_tier(&root, placed)
}

/// The system-level oracles on the host, with the evaluator `placed`: a
/// toolchain gap is the tier's unprovisioned exit, any other placement
/// failure fatal.
fn integration_tier(root: &Path, placed: Result<String, String>) -> Result<i32, CheckError> {
    let goal = crate::integration::GOAL;
    let code = match placed {
        Ok(eval) => crate::integration::run(root, &eval),
        Err(e) => match e.strip_prefix(UNPROVISIONED_TAG) {
            Some(why) => {
                eprintln!("td-builder check: {goal}: no td-recipe-eval for the oracles — {why}");
                println!(">> {goal}: UNPROVISIONED — no evaluator on this host; no oracle ran");
                eprintln!("{}", td_engine::exit::UNPROVISIONED_SENTINEL);
                EXIT_UNPROVISIONED
            }
            None => {
                return Err(CheckError::Fatal(fatal(&format!(
                    "could not build td-recipe-eval for {goal} ({e})"
                ))))
            }
        },
    };
    let _ = std::io::stdout().flush();
    Ok(code)
}

#[cfg(test)]
mod tests {
    /// The loop's PATH must not let a bare name reach the current directory,
    /// which in the sandbox is the worktree. An empty PATH does (glibc and
    /// musl both read it as one empty entry, the cwd); NO_PATH does not.
    #[test]
    fn the_loop_path_reaches_no_bare_name_in_the_cwd() {
        let _dir_cleanup = crate::test_temp::TempDir::new("td-no-path").unwrap();
        let dir = _dir_cleanup.join("fixture");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::os::unix::fs::symlink(
            std::env::current_exe().unwrap(),
            dir.join("td-no-path-probe"),
        )
        .unwrap();
        let spawn = |path: &str| {
            Command::new("td-no-path-probe")
                .args(["--list", "--exact", "no-such-test"])
                .env("PATH", path)
                .current_dir(&dir)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
        };
        assert!(spawn("").is_ok(), "an empty PATH should search the cwd");
        assert_eq!(
            spawn(NO_PATH).map_err(|e| e.kind()).err(),
            Some(std::io::ErrorKind::NotFound)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_a_tagged_stage0_gap_skips_the_loop() {
        let gap = stage0_place_error(&format!("{UNPROVISIONED_TAG}no rustc reachable"));
        assert!(
            matches!(&gap, CheckError::Unprovisioned(m) if m.contains("(no rustc reachable)")),
            "{gap}"
        );
        // The tag is dropped from the report: the skip line says why, not "UNPROVISIONED: ".
        assert!(!gap.to_string().contains(UNPROVISIONED_TAG), "{gap}");
        let broken = stage0_place_error("cargo build failed: linker error");
        assert!(matches!(broken, CheckError::Fatal(_)), "{broken}");
    }

    /// The catalog line's fields, and the argv actually spawned from it.
    ///
    /// These three read the SAME line differently — dest is the field before
    /// the lock, the lock is last, and td-feed is handed everything but the
    /// lock — and the exit statuses collected from the spawned argv are keyed
    /// by dest. Reading the dest back off the SPAWNED argv (which no longer has
    /// the lock) silently names the wrong field, leaves every status
    /// unfindable, and quietly disables the source-built-td-feed retry.
    #[test]
    fn a_vendor_line_splits_into_an_argv_a_dest_and_a_lock() {
        let line: Vec<String> = ["warm", "crate-local", "net", "td-net", "net/Cargo.lock"]
            .iter()
            .map(|f| f.to_string())
            .collect();

        assert_eq!(super::vendor_dest(&line), "td-net");
        assert_eq!(super::vendor_lock(&line), Some("net/Cargo.lock"));
        let argv = super::vendor_argv(&line);
        assert_eq!(
            argv.len(),
            line.len() - 1,
            "the lock is not passed to td-feed"
        );
        assert_eq!(
            argv.last().map(String::as_str),
            Some("td-net"),
            "the spawned argv ends at the dest"
        );

        // The `warm crate` form, whose dest is not the last field of the line
        // either. Short and malformed lines must not panic.
        let crate_line: Vec<String> = ["warm", "crate", "coreutils", "0.9.0", "uutils", "L"]
            .iter()
            .map(|f| f.to_string())
            .collect();
        assert_eq!(super::vendor_dest(&crate_line), "uutils");
        assert_eq!(super::vendor_dest(&[]), "?");
        assert_eq!(super::vendor_lock(&[]), None);
        assert!(super::vendor_argv(&[]).is_empty());
    }

    use super::*;

    fn scratch(name: &str) -> crate::test_temp::TempDir {
        crate::test_temp::TempDir::new(&format!("td-subst-env-{name}")).unwrap()
    }

    /// A usable store: executable td-subst + a narinfo + a non-empty pubkey.
    fn populate(store: &Path, pubkey: &Path) {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::write(store.join("td-subst"), b"#!/bin/sh\n").unwrap();
        std::fs::set_permissions(
            store.join("td-subst"),
            std::fs::Permissions::from_mode(0o755),
        )
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
        assert!(envs
            .iter()
            .any(|(k, v)| k == "TD_SUBST_BIN" && v.ends_with("/td-subst")));
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

    #[test]
    fn parse_timeout_secs_accepts_timeout1_durations() {
        // Bare seconds and the timeout(1) integer suffixes — the ONE
        // TD_WARM_TIMEOUT grammar every warm step shares.
        assert_eq!(parse_timeout_secs("600"), Some(600));
        assert_eq!(parse_timeout_secs("0"), Some(0));
        assert_eq!(parse_timeout_secs("90s"), Some(90));
        assert_eq!(parse_timeout_secs("30m"), Some(1800));
        assert_eq!(parse_timeout_secs("2h"), Some(7200));
        assert_eq!(parse_timeout_secs("1d"), Some(86400));
    }

    #[test]
    fn parse_timeout_secs_rejects_garbage() {
        for bad in ["", "s", "m", "-5", "1.5", "5x", "m30", "30 m"] {
            assert_eq!(parse_timeout_secs(bad), None, "`{bad}` must not parse");
        }
    }

    #[test]
    fn kh_seed_present_matches_only_the_uncompressed_tar() {
        // A pre-#536 `.tar.gz` must not count; the match is version-independent
        // and arch tokens must not cross-match (#546).
        let d = scratch("kh-seed");
        let src = d.join("sources");
        std::fs::create_dir_all(&src).unwrap();

        // Only the stale gzip present -> not satisfied for either arch.
        std::fs::write(src.join("linux-headers-4.14.67-i386.tar.gz"), b"gz").unwrap();
        std::fs::write(src.join("linux-headers-4.14.67-x86_64.tar.gz"), b"gz").unwrap();
        assert!(!kh_seed_present(&src, "i386"));
        assert!(!kh_seed_present(&src, "x86_64"));

        // The uncompressed seed for i386 lands -> only i386 satisfied.
        std::fs::write(src.join("linux-headers-4.14.67-i386.tar"), b"tar").unwrap();
        assert!(kh_seed_present(&src, "i386"));
        assert!(!kh_seed_present(&src, "x86_64"));

        // A different pinned version still matches (keyed on name, not version).
        std::fs::write(src.join("linux-headers-9.9.9-x86_64.tar"), b"tar").unwrap();
        assert!(kh_seed_present(&src, "x86_64"));

        // An x86_64-only cache is absent for i386.
        let e = scratch("kh-seed-arch");
        let esrc = e.join("sources");
        std::fs::create_dir_all(&esrc).unwrap();
        std::fs::write(esrc.join("linux-headers-4.14.67-x86_64.tar"), b"tar").unwrap();
        assert!(!kh_seed_present(&esrc, "i386"));
        assert!(kh_seed_present(&esrc, "x86_64"));
    }

    #[test]
    fn kh_seed_present_false_when_sources_dir_absent() {
        // A sources dir that does not exist -> not present, no panic.
        let d = scratch("kh-seed-nodir");
        assert!(!kh_seed_present(&d.join("sources"), "i386"));
    }
}

#[cfg(test)]
mod scope_key_tests {
    use super::*;

    /// The journal key is the content and the gate runner, not the commit:
    /// an amend that changes only the message keeps it, so a restarted
    /// `ready` reuses what passed, while an edit, an untracked file or
    /// another runner each change it.
    #[test]
    fn the_tree_key_survives_a_message_amend_and_nothing_else() {
        let _root_cleanup = crate::test_temp::TempDir::new(&format!(
            "td-tree-key-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
        .unwrap();
        let root = _root_cleanup.join("fixture");
        std::fs::create_dir_all(root.join("repo")).unwrap();
        let repo = root.join("repo");
        if Command::new("git").arg("--version").output().is_err() {
            eprintln!("SKIP: no git");
            return;
        }
        let git = |args: &[&str]| -> bool {
            Command::new("git")
                .args(args)
                .current_dir(&repo)
                .env_remove("GIT_DIR")
                .env_remove("GIT_WORK_TREE")
                .env_remove("GIT_INDEX_FILE")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
                .output()
                .is_ok_and(|o| o.status.success())
        };
        assert!(git(&["init", "-q"]));
        let runner = root.join("td-builder");
        std::fs::write(&runner, b"runner one").unwrap();
        std::fs::write(repo.join("code.rs"), "fn main() {}\n").unwrap();
        assert!(git(&["add", "code.rs"]));
        assert!(git(&["commit", "-qm", "first message"]));
        let key = tree_key(&repo, &runner).unwrap();
        assert!(git(&["commit", "--amend", "-qm", "second message"]));
        assert_eq!(tree_key(&repo, &runner).unwrap(), key, "message-only amend");

        std::fs::write(&runner, b"runner two").unwrap();
        assert_ne!(tree_key(&repo, &runner).unwrap(), key, "another runner");
        std::fs::write(&runner, b"runner one").unwrap();

        std::fs::write(repo.join("code.rs"), "fn main() { () }\n").unwrap();
        let edited = tree_key(&repo, &runner).unwrap();
        assert_ne!(edited, key, "a dirty edit");
        assert!(git(&["commit", "-qam", "edit"]));
        let committed = tree_key(&repo, &runner).unwrap();
        assert_ne!(committed, key, "a committed edit");

        std::fs::write(repo.join("new.rs"), "x").unwrap();
        assert_ne!(tree_key(&repo, &runner).unwrap(), committed, "untracked");
        assert!(tree_key(&repo, &root.join("missing")).is_none());
    }

    /// A scoped run journals under a key of its own: the tree key is
    /// untouched without a scope, and two scopes, or a scope and none, never
    /// share a key, while the same scope keys the same.
    #[test]
    fn a_scoped_run_keys_its_journal_apart() {
        let s = |xs: &[&str]| xs.iter().map(|x| (*x).to_string()).collect::<Vec<_>>();
        assert_eq!(scoped_tree_key("abc", None, false), "abc");
        let sh = scoped_tree_key("abc", Some(&s(&["td-sh"])), false);
        assert_ne!(sh, "abc");
        assert_ne!(
            sh,
            scoped_tree_key("abc", Some(&s(&["td-sh", "td-txt"])), false)
        );
        assert_ne!(sh, scoped_tree_key("abd", Some(&s(&["td-sh"])), false));
        assert_eq!(sh, scoped_tree_key("abc", Some(&s(&["td-sh"])), false));
        // The scoped key is the one journaled before deferral existed.
        let mut h = crate::sha256::Sha256::new();
        h.update(b"abc\0scope\0td-sh");
        assert_eq!(sh, crate::sha256::to_base16(&h.finalize()));
        // A deferring run keys apart from a full one, scoped or not, and
        // from the same scope run in full.
        let deferring = scoped_tree_key("abc", None, true);
        assert_ne!(deferring, "abc");
        assert_ne!(deferring, sh);
        let both = scoped_tree_key("abc", Some(&s(&["td-sh"])), true);
        assert_ne!(both, sh);
        assert_ne!(both, deferring);
        assert_eq!(both, scoped_tree_key("abc", Some(&s(&["td-sh"])), true));
        // TD_CHECK_FULL runs every check, so it never defers.
        assert!(check_defers(false, true));
        assert!(!check_defers(true, true));
        assert!(!check_defers(false, false));
        assert_eq!(
            check_scope(false, Some("td-sh td-portal")),
            Some(s(&["td-sh", "td-portal"]))
        );
        assert_eq!(check_scope(false, Some("  ")), None);
        assert_eq!(check_scope(false, None), None);
        assert_eq!(check_scope(true, Some("td-sh")), None);
    }
}

#[cfg(test)]
mod vendor_prelude_tests {
    #[test]
    fn vendor_marker_refuses_symlinks_and_oversized_contents() {
        let _root_cleanup = crate::test_temp::TempDir::new("td-vendor-marker").unwrap();
        let root = _root_cleanup.join("fixture");
        let cache = root.join(".td-build-cache/crate-vendor/td-net/vendor");
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::create_dir_all(root.join("net")).unwrap();
        let lock = root.join("net/Cargo.lock");
        std::fs::write(&lock, "lock fixture").unwrap();
        let digest = crate::sha256::sha256_file(&lock).unwrap();
        let marker = cache.join(".warm-complete");
        let target = root.join("marker-target");
        std::fs::write(&target, format!("{digest}\n0\n")).unwrap();
        std::os::unix::fs::symlink(&target, &marker).unwrap();
        assert!(!super::vendor_is_complete(
            &root,
            "td-net",
            Some("net/Cargo.lock")
        ));
        std::fs::remove_file(&marker).unwrap();
        std::fs::write(&marker, format!("{digest}\n{}", "x".repeat(4096))).unwrap();
        assert!(!super::vendor_is_complete(
            &root,
            "td-net",
            Some("net/Cargo.lock")
        ));
        std::fs::write(&marker, format!("{digest}\n0\n")).unwrap();
        assert!(super::vendor_is_complete(
            &root,
            "td-net",
            Some("net/Cargo.lock")
        ));
        std::fs::remove_dir_all(root).unwrap();
    }

    use super::*;

    #[test]
    fn consumed_cache_is_checked_before_any_network_capable_tool_resolution() {
        let _root_cleanup = crate::test_temp::TempDir::new("td-vendor-prelude").unwrap();
        let root = _root_cleanup.join("fixture");
        let _ = std::fs::remove_dir_all(&root);
        let cache = root.join(".td-build-cache/crate-vendor/td-net/vendor");
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::create_dir_all(root.join("net")).unwrap();
        let lock = root.join("net/Cargo.lock");
        std::fs::write(&lock, "[[package]]\nname = \"fixture\"\n").unwrap();
        let digest = crate::sha256::sha256_file(&lock).unwrap();
        let marker = cache.join(".warm-complete");
        for (contents, should_resolve) in [
            (None, true),
            (Some("stale"), true),
            (Some(digest.as_str()), false),
        ] {
            let _ = std::fs::remove_file(&marker);
            if let Some(contents) = contents {
                std::fs::write(&marker, format!("{contents}\n0\n")).unwrap();
            }
            let called = std::cell::Cell::new(false);
            warm_td_crate_closure_resolving(&root, |_| {
                called.set(true);
                None
            });
            assert_eq!(called.get(), should_resolve);
        }
        // Exercise the actual production entry too: this empty checkout has
        // no Cargo manifest, host tool cache or fetched registry packages.
        warm_td_crate_closure(&root);
        std::fs::write(&lock, "changed lock").unwrap();
        let called = std::cell::Cell::new(false);
        warm_td_crate_closure_resolving(&root, |_| {
            called.set(true);
            None
        });
        assert!(called.get());
        std::fs::remove_dir_all(root).unwrap();
    }
}

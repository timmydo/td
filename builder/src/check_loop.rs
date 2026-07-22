//! check_loop.rs — `td-builder check`: the loop's HOST PRELUDE, ported from the
//! old shell check.sh so that check.sh shrinks to a guix-free cargo bootstrap
//! shim (human direction 2026-07-03: "I don't want guix anywhere near check.sh" —
//! the host rust toolchain is the part the user brings; everything after
//! `cargo build` is td's own code).
//!
//! What runs here, in order (the exact sequence the shell prelude ran; the
//! rationale comments live with each step):
//!   1. the netns-probe discrimination check,
//!   2. stage0 provisioning (the guix-free loop-container provider, #294),
//!   3. the loop userland: td-BUILT busybox + make from the recipe graph
//!      (`provision_userland`) — host tools are forbidden, and the loop fails
//!      CLOSED (exit `EXIT_UNPROVISIONED`) while the bootstrap graph still
//!      declares scaffolding the chain has not built (re #469),
//!   4. the warm prelude (subst store, source/crate warms, build daemon),
//!   5. the machine-wide slot dir, and
//!   6. the sandboxed gate run: TB host-sandbox --expose-cwd --no-daemon
//!      --store-item ITEM… --store-item-at SRC DEST… -- TB gate-run. The
//!      sandbox mounts NO store directory: only the loop's declared inputs —
//!      the seed-lock closure (`loop_store_items`) at its own paths and the
//!      td-built userland at /td/store — each bound read-only, the drv build
//!      jail's input-only model.
//!
//! (The host-guix == pinned-channel integrity guard that used to run `guix
//! describe` was removed in #406 — it only warned on drift, so dropping it is
//! behavior-preserving for a correctly-pinned host and drops one guix subprocess
//! per run.)

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

fn fatal(msg: &str) -> String {
    format!("td-builder check: FATAL: {msg}")
}

/// Exit code `td-builder check` uses when it aborts because the loop cannot be
/// provisioned at all — today: the td-built userland is unbuildable because the
/// bootstrap graph still declares host scaffolding, which planning rejects (re
/// #469) — as
/// opposed to a gate genuinely going red. It is the stable machine signal the
/// daily backstop (`td-builder daily`) reads to tell "nothing could run here"
/// from "a real regression", instead of grepping FATAL prose out of the log
/// (the coupling that broke twice — #268, then #315). EX_UNAVAILABLE from
/// sysexits(3): "service unavailable", i.e. this host cannot run the loop.
pub const EXIT_UNPROVISIONED: i32 = 69;

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
/// workstream E #294) and return $TB — a direct `stage0::stage0_place` call
/// (no ambient host sh anywhere in setup, re #469). The base default matches
/// cache-lib's load_stage0, so the prelude and the gates share one placement.
fn provision_stage0(root: &Path) -> Result<String, String> {
    let base = match std::env::var("TD_STAGE0_BASE") {
        Ok(v) if !v.is_empty() => PathBuf::from(v),
        _ => root.join(".td-build-cache/stage0"),
    };
    let cb = crate::stage0::stage0_place(root, &base).map_err(|e| {
        fatal(&format!(
            "could not provision the guix-free stage0 td-builder for the loop sandbox ({e})"
        ))
    })?;
    let placed = Path::new(&cb)
        .file_name()
        .ok_or_else(|| fatal("stage0 placement returned a malformed store path"))?;
    let tb = base.join("store").join(placed).join("bin/td-builder");
    if !tb.is_file() {
        return Err(fatal("stage0 provisioning returned no usable $TB"));
    }
    Ok(tb.display().to_string())
}

/// td's own store prefix — where the loop userland's items are hashed for and
/// must appear inside the sandbox (the recipe graph builds everything with
/// `TD_STORE_DIR=/td/store`). The loop never mounts a store DIRECTORY and
/// never resolves a tool from the host: its userland is td-BUILT
/// (`LOOP_USERLAND_STEMS`), and the only other store bytes it exposes are the
/// DECLARED seed-lock closures (`seed_lock_roots`) — each bound read-only,
/// item by item, at its own path.
const TD_STORE_DIR: &str = "/td/store";

/// The td-built loop userland: the recipe stems whose outputs provide every
/// process-driving tool on the loop sandbox PATH. `busybox-x86-64` (static; sh,
/// env, ls, sed, grep, awk, tar, gzip, cat, …) and `make-x86-64` (static GNU
/// make) are built FROM SOURCE by the recipe chain (stage0 → mes → tcc → … →
/// gcc-x86-64-native). Being static they carry no runtime closure — two items
/// are the whole userland. bash and the GNU text tools are deliberately NOT
/// here: gate bodies run under busybox sh, and recipe rung builds admit NO
/// host executable at all — an input is a prior recipe output or a pinned
/// seed source, or planning rejects it (re #469).
const LOOP_USERLAND_STEMS: &[&str] = &["busybox-x86-64", "make-x86-64"];

/// The resolved loop userland: `(durable host copy, canonical /td/store item
/// path)` per stem — the `--store-item-at` binds — plus the colon-joined
/// in-sandbox bin dirs that become the sandbox PATH.
struct LoopUserland {
    items: Vec<(String, String)>,
    path: String,
}

/// The durable home of the loop userland's host copies + its resolution map:
/// `$TD_LOOP_USERLAND_DIR|~/.td/loop-userland`. Recipe builds materialize
/// outputs in per-run scratch dirs, so the prelude copies the resolved items
/// here once and reuses them while the fingerprint matches.
///
/// Deliberately NOT under `~/.td/build-daemon`: that dir is bound READ-WRITE
/// into every loop sandbox (the daemon socket + output store), so a userland
/// living there would have a writable in-sandbox alias behind the read-only
/// `/td/store/<item>` mounts — a gate could poison the current and future
/// loops' executable bytes. This dir is not bound at all; only the sandbox
/// setup (host-side) reads it.
fn loop_userland_dir() -> Result<PathBuf, String> {
    match std::env::var("TD_LOOP_USERLAND_DIR") {
        Ok(v) if !v.trim().is_empty() => Ok(PathBuf::from(v)),
        _ => {
            let home = std::env::var("HOME").map_err(|_| s("no HOME for TD_LOOP_USERLAND_DIR"))?;
            Ok(Path::new(&home).join(".td/loop-userland"))
        }
    }
}

/// The userland's freshness key: sha256 over the resolved td-recipe-eval
/// binary — the recipe catalog IS that binary (every recipe body, source pin,
/// and applet list is compiled in, and the chain admits no input beyond what
/// it declares, re #469) — PLUS every in-repo seed patch
/// (`seed/patches/*.patch`, sorted, name and bytes). Patches are the one
/// chain input the runner reads from the TREE at build time rather than from
/// the compiled catalog (its pinsum keys them for the same reason), so a
/// patch-only change must re-key the userland even though the evaluator
/// binary is byte-identical. Any recipe/pin/patch/evaluator change re-keys
/// the map, so a stale machine-wide userland — including one built before
/// host scaffolding was outlawed — can never false-green the change under
/// review.
///
/// Known limits (freshness mechanism, not a security boundary): the hash is
/// taken before build-run execs the binary, so a concurrent rebuild of the
/// evaluator between the two can mis-key one provision (self-healing — the
/// next run re-keys and re-provisions); and `TD_RECIPE_EVAL` is an operator
/// override that can cache whatever ITS binary builds under its own
/// fingerprint — pointing it at a pre-cutover evaluator is the operator
/// running old code deliberately, the same trust as any explicit override.
fn userland_fingerprint(root: &Path, eval: &str) -> Result<String, String> {
    let bytes = std::fs::read(eval).map_err(|e| format!("read {eval}: {e}"))?;
    let mut h = crate::sha256::Sha256::new();
    h.update(&bytes);
    let patches = root.join("seed/patches");
    let mut names: Vec<String> = match std::fs::read_dir(&patches) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .filter_map(|e| e.file_name().to_str().map(str::to_string))
            .filter(|n| n.ends_with(".patch"))
            .collect(),
        // No patch dir: the binary alone keys the userland (a tree that
        // GAINS the dir re-keys by growing the hashed list).
        Err(_) => Vec::new(),
    };
    names.sort();
    for name in &names {
        let p = patches.join(name);
        let bytes = std::fs::read(&p).map_err(|e| format!("read {}: {e}", p.display()))?;
        h.update(name.as_bytes());
        h.update(&[0]);
        h.update(&bytes);
        h.update(&[0]);
    }
    Ok(crate::sha256::to_base16(&h.finalize()))
}

/// Resolve the td-built loop userland, provisioning it if needed.
///
/// Warm path: `loop-userland.<fingerprint>.map` opens with `fingerprint
/// <sha256>` matching `userland_fingerprint` (the CURRENT evaluator + seed
/// patches — a recipe, pin, patch, or applet change re-keys it) and names a
/// host copy under `loop_userland_dir()` for every stem, each with an
/// existing `bin/` and on-disk bytes NAR-hashing to the hash recorded at
/// publication — the same content check `verify_staged_item` applies to
/// build inputs, because these items are mounted and EXECUTED by every gate
/// (re #469). The map FILENAME carries the fingerprint so concurrent
/// worktrees — whose evaluator binaries fingerprint differently — never
/// clobber each other's map between one provision's write and its validating
/// re-read, and each keeps its own warm path instead of re-provisioning on
/// every alternation.
///
/// Cold path: run `td-recipe-eval build-run busybox-x86-64 <stems…>`, which
/// realizes the recipe chain (replayed from the warm chain cache when
/// provisioned; built from the seed when not — that first build is the
/// long one and says so loudly). The evaluator REJECTS the graph at planning
/// while any rung still declares host scaffolding (exit 69 → Unprovisioned
/// here): the loop fails closed until busybox/make build solely from audited
/// seeds and prior td recipe outputs (re #469). On success, publish each
/// `TD_RECIPE_RUN_OUT` item
/// into the durable dir and rewrite the map atomically. Items are
/// input-addressed (the basename embeds the recipe+input key), so an
/// already-present basename whose bytes HASH to the fresh build's is reused
/// as-is — concurrent cold provisions from parallel worktrees stay safe
/// (whoever publishes first wins) — while a mismatching copy is quarantined
/// aside and republished; either way the map records only bytes verified
/// against this chain's own build. td-recipe-eval itself resolves via
/// `TD_RECIPE_EVAL` or a fresh
/// host `cargo build`; a host with neither is a provisioning gap
/// (`CheckError::Unprovisioned`), not a code regression.
fn provision_userland(root: &Path) -> Result<LoopUserland, CheckError> {
    let dir = loop_userland_dir().map_err(|e| CheckError::Fatal(fatal(&e)))?;
    let eval = resolve_recipe_eval_bin(root)?;
    let fingerprint = userland_fingerprint(root, &eval).map_err(|e| CheckError::Fatal(fatal(&e)))?;
    let map_path = dir.join(format!("loop-userland.{fingerprint}.map"));
    if let Some(ul) = read_userland_map(&dir, &map_path, &fingerprint) {
        return Ok(ul);
    }

    eprintln!(
        "td-builder check: provisioning the td-built loop userland ({}) via the recipe \
         chain — a warm chain cache replays in minutes; a cold host builds the bootstrap \
         chain from the seed first (hours, once)",
        LOOP_USERLAND_STEMS.join(" ")
    );
    // Cold-cache branch (map miss): warm the td-tool crate closure (td-fetch)
    // BEFORE the build-run that consumes it. This is where cold runs of EVERY
    // tier pass — light tiers too — so the closure is vendored offline whenever
    // the chain is (re-)provisioned from cold, not only on the heavy warm
    // prelude. A warm-map hit returned above, so this never re-warms an
    // already-provisioned userland. Best-effort (the recipe check enforces
    // presence).
    warm_td_crate_closure(root);
    // …and generate the kernel-headers seeds here, since heavy_warms runs after
    // this. Without it the loop reds at "kernel-headers tarball not warm" (#546).
    warm_kernel_headers_seed(root);
    let mut cmd = Command::new(&eval);
    cmd.arg("build-run").arg("busybox-x86-64");
    for stem in LOOP_USERLAND_STEMS {
        cmd.arg(stem);
    }
    let out = cmd
        .current_dir(root)
        .output()
        .map_err(|e| fatal(&format!("could not run {eval} build-run: {e}")))?;
    if !out.status.success() {
        let tail: Vec<&str> = std::str::from_utf8(&out.stderr)
            .unwrap_or("")
            .lines()
            .rev()
            .take(8)
            .collect();
        let tail: Vec<&str> = tail.into_iter().rev().collect();
        // Exit 69 (EX_UNAVAILABLE) is td-recipe-eval's PLANNING-time
        // provenance rejection: the bootstrap graph still declares host
        // scaffolding, which is no longer an admissible input class (re
        // #469), so NO host can provision this userland. The loop fails
        // CLOSED as Unprovisioned — there is no host-tool fallback and no
        // grandfathered userland (the fingerprint keys the map to the
        // current, rejecting evaluator).
        if out.status.code() == Some(69) {
            return Err(CheckError::Unprovisioned(fatal(&format!(
                "the loop userland ({}) cannot be provisioned: the recipe chain rejected \
                 its own inputs' provenance — the bootstrap rungs still declare host \
                 scaffolding, and only audited seeds and prior td recipe outputs are \
                 admissible (re #469). The loop runs NO gates until the chain builds \
                 its scaffolding as recipe outputs:\n{}",
                LOOP_USERLAND_STEMS.join(" "),
                tail.join("\n")
            ))));
        }
        return Err(CheckError::Fatal(fatal(&format!(
            "loop userland build failed (td-recipe-eval build-run busybox-x86-64):\n{}",
            tail.join("\n")
        ))));
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    std::fs::create_dir_all(&dir)
        .map_err(|e| fatal(&format!("mkdir {}: {e}", dir.display())))?;
    let mut map = format!("fingerprint {fingerprint}\n");
    for stem in LOOP_USERLAND_STEMS {
        let prefix = format!("TD_RECIPE_RUN_OUT {stem} ");
        let built = stdout
            .lines()
            .rev()
            .find_map(|l| l.strip_prefix(prefix.as_str()))
            .map(str::trim)
            .ok_or_else(|| {
                fatal(&format!(
                    "loop userland: build-run reported no output path for {stem}"
                ))
            })?;
        let base = Path::new(built)
            .file_name()
            .and_then(|b| b.to_str())
            .ok_or_else(|| fatal(&format!("loop userland: unusable output path {built}")))?;
        // The recipe chain's freshly built bytes are the authority: their NAR
        // hash is what the map will vouch for, and whatever ends up at the
        // durable path must hash to it before it is mapped (re #469).
        let want = crate::sandbox::nar_hash_of(Path::new(built))
            .map_err(|e| fatal(&format!("loop userland: hash {built}: {e}")))?;
        let durable = dir.join(base);
        // Input-addressed basename: an existing durable copy SHOULD be this
        // content, but only its hash proves it — a matching copy is reused
        // untouched; a mismatching one (corruption, or bytes published before
        // hashes were recorded) is renamed aside, never mounted. The rename's
        // own failure is ignored: a leftover bad copy just fails the final
        // verify below, which is the fail-closed path anyway.
        if durable.is_dir() {
            let have = crate::sandbox::nar_hash_of(&durable)
                .map_err(|e| fatal(&format!("loop userland: hash {}: {e}", durable.display())))?;
            if have != want {
                let quarantine = dir.join(format!(".bad.{base}.{}", std::process::id()));
                let _ = std::fs::remove_dir_all(&quarantine);
                let _ = std::fs::rename(&durable, &quarantine);
            }
        }
        // Publish via scratch-copy + rename; a rename that loses to a
        // concurrent publisher (EEXIST/ENOTEMPTY) leaves THEIR copy in place,
        // and the verify below only maps it if it matches our build.
        if !durable.is_dir() {
            let tmp = dir.join(format!(".tmp.{base}.{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&tmp);
            copy_tree_preserving(Path::new(built), &tmp)
                .map_err(|e| fatal(&format!("loop userland: copy {built}: {e}")))?;
            if let Err(e) = std::fs::rename(&tmp, &durable) {
                let _ = std::fs::remove_dir_all(&tmp);
                if !durable.is_dir() {
                    return Err(CheckError::Fatal(fatal(&format!(
                        "loop userland: place {}: {e}",
                        durable.display()
                    ))));
                }
            }
        }
        let have = crate::sandbox::nar_hash_of(&durable)
            .map_err(|e| fatal(&format!("loop userland: hash {}: {e}", durable.display())))?;
        if have != want {
            return Err(CheckError::Fatal(fatal(&format!(
                "loop userland: published {} hashes {have} but the fresh build hashes {want} — \
                 refusing to map executable bytes the chain did not produce (re #469)",
                durable.display()
            ))));
        }
        map.push_str(&format!("{stem} {base} {want}\n"));
    }
    let tmp_map = map_path.with_extension(format!("tmp.{}", std::process::id()));
    std::fs::write(&tmp_map, &map).map_err(|e| fatal(&format!("write loop-userland.map: {e}")))?;
    std::fs::rename(&tmp_map, &map_path)
        .map_err(|e| fatal(&format!("place loop-userland.map: {e}")))?;
    read_userland_map(&dir, &map_path, &fingerprint).ok_or_else(|| {
        CheckError::Fatal(fatal(
            "loop userland: the freshly written map did not validate — refusing a userland-less loop",
        ))
    })
}

/// Parse + validate the durable userland map against the CURRENT fingerprint:
/// the `fingerprint` line matches, every stem present with a recorded NAR
/// hash, every item dir present with a `bin/`, and every item's on-disk bytes
/// re-hash to the record — CONTENT is verified, not existence, so a corrupted
/// or tampered durable copy is a cache miss (the caller re-provisions), never
/// a mount (re #469). Any miss returns None. Hash-less lines (pre-hash map
/// format) fail to parse, so old maps self-invalidate.
fn read_userland_map(dir: &Path, map_path: &Path, fingerprint: &str) -> Option<LoopUserland> {
    let content = std::fs::read_to_string(map_path).ok()?;
    let fp = content
        .lines()
        .find_map(|l| l.strip_prefix("fingerprint "))?
        .trim();
    if fp != fingerprint {
        return None;
    }
    let mut items: Vec<(String, String)> = Vec::new();
    let mut bins: Vec<String> = Vec::new();
    for stem in LOOP_USERLAND_STEMS {
        let (base, want) = content.lines().find_map(|l| {
            let mut f = l.split_whitespace();
            let (k, base, want) = (f.next()?, f.next()?, f.next()?);
            (k == *stem
                && f.next().is_none()
                && !base.contains('/')
                && want.starts_with("sha256:"))
            .then(|| (base.to_string(), want.to_string()))
        })?;
        let host = dir.join(&base);
        if !host.join("bin").is_dir() {
            return None;
        }
        if crate::sandbox::nar_hash_of(&host).ok()? != want {
            return None;
        }
        let canon = format!("{TD_STORE_DIR}/{base}");
        bins.push(format!("{canon}/bin"));
        items.push((host.display().to_string(), canon));
    }
    Some(LoopUserland {
        items,
        path: bins.join(":"),
    })
}

/// The td-recipe-eval binary: `TD_RECIPE_EVAL` (explicit override), else a
/// host `cargo build` of recipes/ — the same host-brings-cargo seed the stage0
/// provision uses, and ALWAYS the current tree (a no-change rebuild is
/// seconds; the build-recipes sentinel is deliberately NOT consulted first —
/// it can lag the tree by a loop iteration, and a stale evaluator would both
/// build and fingerprint yesterday's userland). The sentinel is the last
/// resort for a cargo-less host running a pre-built loop.
///
/// The error variants are load-bearing (review finding): a host WITHOUT cargo
/// is a provisioning gap (`Unprovisioned` → exit 69, PARTIAL in the tiers),
/// but a host WITH cargo whose recipes/ tree fails to COMPILE is a code
/// regression and must red (`Fatal`) — conflating them would let a broken
/// recipes crate exit 0 through affected-checks.
fn resolve_recipe_eval_bin(root: &Path) -> Result<String, CheckError> {
    if let Ok(v) = std::env::var("TD_RECIPE_EVAL") {
        if !v.trim().is_empty() && Path::new(&v).is_file() {
            return Ok(v);
        }
    }
    if find_in_path("cargo").is_some() {
        let status = Command::new("cargo")
            .args(["build", "--release", "--quiet"])
            .current_dir(root.join("recipes"))
            .status()
            .map_err(|e| CheckError::Fatal(fatal(&format!("spawn cargo build (recipes): {e}"))))?;
        if !status.success() {
            return Err(CheckError::Fatal(fatal(
                "the recipes crate does not compile (`cargo build --release` in recipes/ \
                 failed) — a code regression, not a provisioning gap",
            )));
        }
        let p = root.join("recipes/target/release/td-recipe-eval");
        if p.is_file() {
            return Ok(p.display().to_string());
        }
        return Err(CheckError::Fatal(fatal(&format!(
            "cargo build succeeded but {} is missing",
            p.display()
        ))));
    }
    let sentinel = root.join(".td-build-cache/recipe-eval/recipe-eval-path");
    let path = std::fs::read_to_string(&sentinel)
        .map(|t| t.trim().to_string())
        .unwrap_or_default();
    if !path.is_empty() && Path::new(&path).is_file() {
        return Ok(path);
    }
    Err(CheckError::Unprovisioned(fatal(
        "no td-recipe-eval to resolve the loop userland: set TD_RECIPE_EVAL or run \
         `cargo build --release --manifest-path recipes/Cargo.toml` and re-run",
    )))
}

/// Copy a tree preserving file modes and symlinks AS symlinks — the userland
/// items are busybox applet farms (symlinks at the busybox binary) whose links
/// must survive the copy byte-identically.
fn copy_tree_preserving(src: &Path, dst: &Path) -> Result<(), String> {
    let md = std::fs::symlink_metadata(src).map_err(|e| format!("stat {}: {e}", src.display()))?;
    let ft = md.file_type();
    if ft.is_symlink() {
        let target =
            std::fs::read_link(src).map_err(|e| format!("readlink {}: {e}", src.display()))?;
        std::os::unix::fs::symlink(&target, dst)
            .map_err(|e| format!("symlink {}: {e}", dst.display()))?;
        return Ok(());
    }
    if ft.is_dir() {
        std::fs::create_dir_all(dst).map_err(|e| format!("mkdir {}: {e}", dst.display()))?;
        let rd = std::fs::read_dir(src).map_err(|e| format!("readdir {}: {e}", src.display()))?;
        for entry in rd {
            let entry = entry.map_err(|e| format!("readdir {}: {e}", src.display()))?;
            copy_tree_preserving(&entry.path(), &dst.join(entry.file_name()))?;
        }
        std::fs::set_permissions(dst, md.permissions())
            .map_err(|e| format!("chmod {}: {e}", dst.display()))?;
        return Ok(());
    }
    std::fs::copy(src, dst).map_err(|e| format!("copy {}: {e}", src.display()))?;
    Ok(())
}

/// The store paths of the SEED locks that gate bodies resolve INSIDE the
/// sandbox (format `NAME <store-path>`): tests/td-builder-rust.lock
/// (tools/provision-{rust,cc}.sh branch 2 execs its rust/cc paths directly)
/// and tests/td-subst.lock (td-subst's pinned build closure) — both serve the
/// pinned RUST toolchain seed (AGENTS.md's allowed control plane), never a
/// bootstrap build: recipe rungs admit NO host executable at all (planning
/// rejects the provenance, re #469). Every lock path present on the host is a
/// DECLARED loop input and joins the closure roots. An absent path is skipped
/// — the consumers fall through / fail loudly the same way they do today on a
/// host without it.
pub(crate) fn seed_lock_roots(root: &Path) -> Vec<String> {
    let mut roots: Vec<String> = Vec::new();
    for lock in ["tests/td-builder-rust.lock", "tests/td-subst.lock"] {
        let Ok(content) = std::fs::read_to_string(root.join(lock)) else {
            continue;
        };
        for p in parse_seed_lock(&content) {
            if Path::new(&p).exists() && !roots.contains(&p) {
                roots.push(p);
            }
        }
    }
    roots
}

/// Parse the seed lock's `NAME <store-path>` lines (comments/blank skipped)
/// into the deduped path list. Only absolute paths qualify — the lock is the
/// DECLARED seed, whatever store its paths live in; there is no hardcoded
/// prefix to check against.
fn parse_seed_lock(content: &str) -> Vec<String> {
    let mut paths: Vec<String> = Vec::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some(path) = line.split_whitespace().nth(1) else {
            continue;
        };
        if path.starts_with('/') && !paths.iter().any(|p| p == path) {
            paths.push(path.to_string());
        }
    }
    paths
}

/// The seed store DIRECTORY, DERIVED from the declared lock paths (the unique
/// parent of the lock items) — never a hardcoded prefix. This is the candidate
/// dir the closure scan indexes and the build daemon's default seed dir; on a
/// guix host the locks point under /gnu/store so that is what derives, and a
/// re-captured lock on another host derives that host's store with no code
/// change. Locks spanning several parents keep the FIRST (warned) — the seed
/// is one store by construction.
fn seed_store_dir(roots: &[String]) -> Option<String> {
    let mut dirs: Vec<String> = Vec::new();
    for r in roots {
        if let Some(parent) = Path::new(r).parent() {
            let d = parent.display().to_string();
            if !d.is_empty() && d != "/" && !dirs.contains(&d) {
                dirs.push(d);
            }
        }
    }
    if dirs.len() > 1 {
        eprintln!(
            "td-builder check: WARNING: seed locks span {} store dirs ({}); scanning only \
             the first",
            dirs.len(),
            dirs.join(" ")
        );
    }
    dirs.into_iter().next()
}

/// Everything the loop sandbox mounts from the SEED store — the full declared
/// input set, computed so the sandbox never mounts a store DIRECTORY, only
/// declared items (the td-built userland is separate — `provision_userland`):
///
///   1. the runtime CLOSURE of `roots` (the seed locks' declared paths —
///      see `seed_lock_roots`), and
///   2. the closure of each `scan_files` binary: host-built ELF executables
///      that RUN INSIDE the sandbox (the stage0 td-builder, the daily's stashed
///      td-subst), whose libc/gcc-lib references are found by content-scanning
///      the binary itself — declared, not assumed to coincide with (1).
///
/// The candidate store dir is DERIVED from the lock paths (`seed_store_dir`),
/// never hardcoded. The closure walk is the same no-DB content scan
/// realize_drv uses (`scan_candidate_index` + `scan_closure_hybrid`; never
/// /var/guix), and it is RECOMPUTED FROM THE SCAN ON EVERY RUN: the
/// loop-closure CACHE is deleted (re #469, round-7 review). Its hit path
/// accepted any superset of items whose extras merely looked like top-level
/// seed-store children — path-shape validation, not proof of membership in
/// the actual reference closure — so stale or edited host state could add
/// undeclared executable store items to the next loop sandbox. This list
/// names the exact `--store-item` binds of the run: nothing but the live
/// scan may produce it. Until an audited seed carries an explicit closure
/// manifest, the recompute — O(seed-store bytes), once per `check`
/// invocation, small next to the gate ladder it admits — is the price of
/// that authority. Returns the sorted item paths (roots included).
fn loop_store_items(
    root: &Path,
    roots: &[String],
    scan_files: &[String],
) -> Result<Vec<String>, String> {
    let Some(store_dir) = seed_store_dir(roots) else {
        // No declared seed at all: nothing to scan, nothing to bind. The
        // sandbox still gets the td-built userland; anything that needed the
        // seed (a dynamically linked stage0) fails loudly inside.
        eprintln!(
            "td-builder check: WARNING: no seed-lock paths exist on this host — binding \
             no seed store items (tests/td-*.lock)"
        );
        return Ok(Vec::new());
    };
    let store_dir = store_dir.as_str();
    // Sweep the RETIRED cache locations unconditionally, so a poisoned or stale
    // leftover can never be re-read by an older binary: the pre-round-5 in-tree
    // file (inside the tree every gate sandbox mounts read-write) and the
    // round-6 per-worktree file under the loop-userland dir.
    let _ = std::fs::remove_file(root.join(".td-build-cache/loop-closure.list"));
    if let Ok(dir) = loop_userland_dir() {
        let mut h = crate::sha256::Sha256::new();
        h.update(root.display().to_string().as_bytes());
        let key = crate::sha256::to_base16(&h.finalize());
        let key = key.get(..16).unwrap_or(&key);
        let _ = std::fs::remove_file(dir.join(format!("loop-closure.{key}.list")));
    }
    let (candidates, on_disk) =
        crate::scan_candidate_index(&[store_dir.to_string()], store_dir)?;
    let mut scanner = crate::scan::Scanner::new(&candidates).map_err(|e| e.to_string())?;
    let mut all_roots: std::collections::BTreeSet<String> = roots.iter().cloned().collect();
    for f in scan_files {
        scanner.reset();
        crate::nar::write_nar(&mut scanner, Path::new(f))
            .map_err(|e| format!("content-scan {f}: {e}"))?;
        all_roots.extend(scanner.refs());
    }
    eprintln!(
        "td-builder check: content-scanning the loop's seed-lock closure ({} roots; \
         recomputed every run — no closure cache, re #469)",
        all_roots.len()
    );
    let empty = std::collections::HashMap::new();
    let root_list: Vec<String> = all_roots.iter().cloned().collect();
    let seen = crate::scan_closure_hybrid(&mut scanner, &on_disk, &empty, &root_list)?;
    Ok(seen.into_iter().collect())
}

/// The host-built binaries that RUN INSIDE the loop sandbox and whose store
/// references must therefore be closure roots: the stage0 td-builder (gate-run
/// itself) and, when the substitute store is usable, its stashed td-subst (the
/// toolchain gates exec it). `check` and `check-rung` share this so the
/// closure cache stays stable across both.
fn loop_scan_files(root: &Path, tb: &str) -> Vec<String> {
    let mut files = vec![tb.to_string()];
    for (k, v) in subst_env(root) {
        if k == "TD_SUBST_BIN" {
            files.push(v);
        }
    }
    files
}

fn native_applet_path(root: &Path, provider: &str) -> Result<String, String> {
    let bin = root
        .join(".td-build-cache/loop-applets")
        .join(std::process::id().to_string())
        .join("bin");
    std::fs::create_dir_all(&bin).map_err(|e| format!("mkdir {}: {e}", bin.display()))?;
    for applet in ["mount", "flock"] {
        let path = bin.join(applet);
        let tmp = bin.join(format!(".{applet}.tmp.{}", std::process::id()));
        let _ = std::fs::remove_file(&tmp);
        std::os::unix::fs::symlink(provider, &tmp)
            .map_err(|e| format!("symlink {} -> {provider}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, &path)
            .map_err(|e| format!("rename {} -> {}: {e}", tmp.display(), path.display()))?;
    }
    Ok(bin.display().to_string())
}

fn loop_path_with_native_applets(root: &Path, tb: &str, toolchain: &str) -> Result<String, String> {
    let applets = native_applet_path(root, tb)?;
    Ok(format!("{applets}:{toolchain}"))
}

/// A timeout(1)-style duration: bare integer seconds or an integer with an
/// s/m/h/d suffix. None on anything else (fractions are out of scope).
fn parse_timeout_secs(v: &str) -> Option<u64> {
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

/// Wait for a warm child under an optional deadline: block when there is
/// none; past it (or on a wait error), kill the child and report failure —
/// a killed child is a failed warm step, never a failed check.
fn wait_with_deadline(child: &mut std::process::Child, deadline: Option<Instant>) -> bool {
    let Some(d) = deadline else {
        return child.wait().map(|st| st.success()).unwrap_or(false);
    };
    loop {
        match child.try_wait() {
            Ok(Some(st)) => return st.success(),
            Ok(None) if Instant::now() >= d => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
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
        || std::fs::write(
            host_leaf.join("cgroup.procs"),
            std::process::id().to_string(),
        )
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
        let out = Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .ok()?;
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

/// Host-cargo fallback for a warm-prelude tool: `cargo build --release` in
/// `<dir>/` and return `target/release/<bin>`. None when cargo is absent, the
/// build fails, or it outlives the warm deadline (a hung cargo — e.g. a stale
/// target-dir lock — must not stall the prelude; every warm is best-effort,
/// the gates enforce presence).
/// Resolve `bin` in a `:`-joined PATH fragment (the form `stage0::provision_*`
/// return) to an absolute executable: the child runs under a provisioned
/// toolchain PATH, so the binary must come from THERE, not an ambient lookup.
fn find_in_frags(frags: &str, bin: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt as _;
    frags.split(':').filter(|f| !f.is_empty()).find_map(|d| {
        let p = Path::new(d).join(bin);
        // Require the exec bit, matching stage0::find_in_path and
        // gate_bodies::find_in_path_frags: a non-executable same-named file
        // earlier on PATH must not shadow the real tool.
        let ok = std::fs::metadata(&p)
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false);
        ok.then_some(p)
    })
}

/// Build a td network tool (`dir` = feed/fetch — the only tools this is called
/// for) with the HOST cargo, STATICALLY linked (crt-static against a matched
/// glibc), and return the binary. Best-effort: any missing piece (no toolchain,
/// no static glibc, a non-static result) logs and returns `None` so the warm
/// degrades to the td-built binary or is skipped — it never returns a
/// DYNAMICALLY linked control-plane tool, which would drag a mutable
/// host/guix-home runpath and flake with `libgcc_s.so.1` exit 127 (re #469).
///
/// Scope: td-feed/td-fetch run on the HOST (the warm's network prep), where NSS
/// is present, so the static glibc's runtime `dlopen` of NSS modules during DNS
/// resolves normally. `assert_static` proves an empty STARTUP closure (no
/// PT_INTERP/DT_NEEDED/run-path — the flake fix), NOT that DNS needs zero runtime
/// DSOs. td-subst is deliberately NOT built here: it is sourced ambiently
/// (`TD_SUBST_BIN`/PATH, `daily::publish_substitutes`) and runs inside the
/// NEWNET-isolated loop sandbox, so its name-resolution/NSS posture is a separate
/// question (PR #534 discussion) — statically linking it is out of scope.
///
/// Unlike the pure-std tools, the network crates (ureq/rustls/ring) pull in
/// PROC-MACROS that must compile for the host compiler, so `+crt-static` cannot
/// go in a global RUSTFLAGS (it would try to statically link the proc-macro
/// dylibs — "does not support these crate types"). Instead pass `--target
/// <host-triple>` and set the PER-TARGET rustflags env: cargo then applies the
/// static flags to the final binary + its normal deps only, leaving host-kind
/// build scripts / proc-macros dynamic. Ambient RUSTFLAGS/CARGO_ENCODED_RUSTFLAGS
/// are removed so they cannot win over the per-target var (cargo's flag-source
/// precedence) and silently defeat the static flags. The compiler is pinned too:
/// RUSTC to the provisioned rustc (the one the triple was read from) and
/// RUSTC_WRAPPER/RUSTC_WORKSPACE_WRAPPER removed, so no ambient rustc or wrapper
/// interposes on the control-plane build. ring's `cc-rs` build script needs a `cc`
/// (CC/HOST_CC/TARGET_CC and the per-target CC_<triple> forms → the gcc-toolchain's
/// `gcc`, AR alongside — every form cc-rs consults is pinned so an ambient one
/// cannot outrank them), and the rustc LINKER is pinned to that SAME gcc so the
/// link uses the compiler matched with the static glibc — else a mismatched-glibc
/// binary links + passes assert_static yet SIGSEGVs at startup (this path has no
/// smoke-run to catch it).
fn host_cargo_bin(root: &Path, dir: &str, bin: &str, deadline: Option<Instant>) -> Option<PathBuf> {
    let penv = crate::stage0::ProvisionEnv::from_env(root);
    let rustpath = match crate::stage0::provision_rust(&penv) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("td-builder check: static {bin}: no rust toolchain ({e}) — skipping host build");
            return None;
        }
    };
    let ccpath = match crate::stage0::provision_cc(&penv) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("td-builder check: static {bin}: no C toolchain ({e}) — skipping host build");
            return None;
        }
    };
    let glibc_static = match crate::stage0::provision_glibc_static(&penv) {
        Ok(g) => g,
        Err(e) => {
            eprintln!(
                "td-builder check: static {bin}: no matched static glibc ({e}) — skipping host build"
            );
            return None;
        }
    };
    let Some(cargo) = find_in_frags(&rustpath, "cargo") else {
        eprintln!("td-builder check: static {bin}: no cargo on the provisioned rust toolchain — skipping host build");
        return None;
    };
    let Some(rustc) = find_in_frags(&rustpath, "rustc") else {
        eprintln!("td-builder check: static {bin}: no rustc on the provisioned rust toolchain — skipping host build");
        return None;
    };
    let Some(cc) = find_in_frags(&ccpath, "cc").or_else(|| find_in_frags(&ccpath, "gcc")) else {
        eprintln!("td-builder check: static {bin}: no cc/gcc on the provisioned C toolchain — skipping host build");
        return None;
    };
    let Some(ar) = find_in_frags(&ccpath, "ar") else {
        eprintln!("td-builder check: static {bin}: no ar on the provisioned C toolchain — skipping host build");
        return None;
    };

    // Host target triple from `rustc -vV`'s `host:` line, and the matching
    // per-target env-var suffix (`<TRIPLE>` uppercased with '-' → '_').
    let vv = match Command::new(&rustc).arg("-vV").stdin(Stdio::null()).output() {
        Ok(o) => o,
        Err(e) => {
            eprintln!("td-builder check: static {bin}: `rustc -vV` failed ({e}) — skipping host build");
            return None;
        }
    };
    let vv_text = String::from_utf8_lossy(&vv.stdout);
    let Some(triple) = vv_text
        .lines()
        .find_map(|l| l.strip_prefix("host: "))
        .map(str::trim)
    else {
        eprintln!("td-builder check: static {bin}: no `host:` line in `rustc -vV` — skipping host build");
        return None;
    };
    let triple = triple.to_string();
    // cargo normalizes BOTH `-` and `.` to `_` in the CARGO_TARGET_<triple>_* env
    // var name (host triples are dot-free today, but match cargo's rule exactly).
    let tvar = triple.to_uppercase().replace(['-', '.'], "_");
    let target_flags_var = format!("CARGO_TARGET_{tvar}_RUSTFLAGS");
    let target_linker_var = format!("CARGO_TARGET_{tvar}_LINKER");
    // The rustc linker is pinned via target_linker_var below (not `-C linker` in
    // the rustflags), so pass None here.
    let rustflags = crate::stage0::static_rustflags(&glibc_static, None);
    // cc-rs (ring's C build) resolves the compiler/archiver from the FIRST of
    // several env forms, and the per-target-suffixed forms outrank the plain
    // CC/HOST_CC/AR we set. Pin every form cc-rs consults to the matched
    // toolchain so an ambient CC_<triple>/AR_<triple>/HOST_AR cannot slip a
    // different compiler into a control-plane binary (review PR #534, Codex P2).
    // cc-rs reads both the dash and underscore triple spellings.
    let triple_us = triple.replace('-', "_");
    let cc_target = format!("CC_{triple}");
    let cc_target_us = format!("CC_{triple_us}");
    let ar_target = format!("AR_{triple}");
    let ar_target_us = format!("AR_{triple_us}");
    let ambient_path = std::env::var("PATH").unwrap_or_default();
    let new_path = format!("{rustpath}:{ccpath}:{ambient_path}");

    let child = Command::new(&cargo)
        .args(["build", "--release", "--quiet", "--target", &triple])
        .current_dir(root.join(dir))
        .env("PATH", &new_path)
        // Pin the compiler itself: an inherited RUSTC would build with a
        // different rustc than the one we read the triple from, and an inherited
        // RUSTC_WRAPPER (e.g. sccache) would interpose on the control-plane build.
        // Set the wrappers to "" (not env_remove): an ABSENT var lets cargo fall
        // back to a `.cargo/config.toml` `build.rustc-wrapper`, whereas an empty
        // value means "no wrapper" regardless of config (Agy review, PR #534).
        .env("RUSTC", &rustc)
        .env("RUSTC_WRAPPER", "")
        .env("RUSTC_WORKSPACE_WRAPPER", "")
        .env("CC", &cc)
        .env("HOST_CC", &cc)
        .env("TARGET_CC", &cc)
        .env(&cc_target, &cc)
        .env(&cc_target_us, &cc)
        .env("AR", &ar)
        .env("HOST_AR", &ar)
        .env("TARGET_AR", &ar)
        .env(&ar_target, &ar)
        .env(&ar_target_us, &ar)
        .env(&target_linker_var, &cc)
        .env(&target_flags_var, &rustflags)
        .env_remove("RUSTFLAGS")
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .stdin(Stdio::null())
        .spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) => {
            eprintln!("td-builder check: static {bin}: cannot spawn cargo ({e}) — skipping host build");
            return None;
        }
    };
    if !wait_with_deadline(&mut child, deadline) {
        return None;
    }
    let p = root
        .join(dir)
        .join("target")
        .join(&triple)
        .join("release")
        .join(bin);
    if !p.is_file() {
        return None;
    }
    // Fail closed on a non-static result rather than hand a dynamic control-plane
    // binary to the warm (re #469).
    if let Err(e) = crate::elf::assert_static(&p) {
        eprintln!(
            "td-builder check: static {bin}: host build produced a non-static binary ({e}) — skipping"
        );
        return None;
    }
    Some(p)
}

/// One `[[package]]` entry of a Cargo.lock that carries a checksum — `(name,
/// version, sha256)`. The checksummed entries are the vendored crates-io deps;
/// the root (path) crate has no checksum and is excluded, exactly the
/// reduction the retired shell awk did.
pub(crate) fn parse_lock_checksums(lock: &str) -> Vec<(String, String, String)> {
    // Tolerate non-canonical whitespace around `=` and leading indentation so a
    // hand-edited lock cannot slip a field past the trust-boundary scanners; a
    // cargo-written lock (column-0 fields, single space) parses identically.
    fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
        let (k, v) = line.trim().split_once('=')?;
        if k.trim() != key {
            return None;
        }
        let inner = v.trim().strip_prefix('"')?;
        let end = inner.find('"')?;
        inner.get(..end).filter(|s| !s.is_empty())
    }
    let mut out = Vec::new();
    let (mut name, mut ver): (Option<&str>, Option<&str>) = (None, None);
    for line in lock.lines() {
        if line.starts_with("[[package]]") {
            (name, ver) = (None, None);
        } else if let Some(v) = field(line, "name") {
            name = Some(v);
        } else if let Some(v) = field(line, "version") {
            ver = Some(v);
        } else if let Some(sum) = field(line, "checksum") {
            if let (Some(n), Some(v)) = (name, ver) {
                out.push((n.to_string(), v.to_string(), sum.to_string()));
            }
        }
    }
    out
}

/// Warm a td crate's OWN dependency closure (native since this port — was
/// tools/warm-td-fetch-crates.sh, the prelude's last `sh tools/…` spawn;
/// #318 axis 2): host-side NETWORK PREP that GETs each `.crate` of
/// `LOCK_REL`'s Cargo.lock GUIX-FREE with td's OWN fetcher (td-fetch), pinned
/// by the UPSTREAM lock checksum (NOT a guix artifact), into the flat vendor
/// dir the crate's recipe check interns and builds from (TD_VENDOR_DIR):
/// `.td-build-cache/crate-vendor/NAME`. The fetcher is always td-fetch — td
/// dogfoods its own fetcher for every td-tool closure — and it honors
/// TD_FEED_BASE so the reads route through the shared feed when it is up.
/// Best-effort like every warm (no td-fetch binary / no network → warn and
/// return; the gate reports if it actually runs cold), and the whole warm —
/// cargo fallback included — shares ONE warm_timeout_secs budget exactly as
/// the shell's single `timeout` over the script did: one hung mirror must
/// not stall the prelude.
///
/// NAME selects both the vendor subdir and the log label ("td-fetch"): the
/// crate closure is a declared offline vendor set warmed before the chain that
/// consumes it — a live crates.io resolution is not a fixed-output fetch.
fn warm_crate_closure(root: &Path, lock_rel: &str, name: &str) {
    let lock_path = root.join(lock_rel);
    let Ok(lock) = std::fs::read_to_string(&lock_path) else {
        eprintln!(
            "td-builder check: warm {name} crates: no {} — skipping",
            lock_path.display()
        );
        return;
    };
    let dest = root.join(format!(".td-build-cache/crate-vendor/{name}"));
    if std::fs::create_dir_all(&dest).is_err() {
        eprintln!(
            "td-builder check: warm {name} crates: cannot create {} — skipping",
            dest.display()
        );
        return;
    }
    // The deadline covers the WHOLE warm including a cargo build of the
    // fetcher, exactly as the shell's one `timeout` over the script did.
    let deadline = warm_timeout_secs().map(|n| Instant::now() + Duration::from_secs(n));
    // Locate or build td-fetch (the fetcher), reused across crates.
    let Some(tdf) = newstore_bin(
        root,
        ".td-build-cache/td-fetch-recipe-check/sd/newstore",
        "td-fetch",
    )
    .or_else(|| host_cargo_bin(root, "fetch", "td-fetch", deadline)) else {
        eprintln!(
            "td-builder check: warm {name} crates: no td-fetch binary — skipping (PREP best-effort)"
        );
        return;
    };
    let mut complete = true;
    for (crate_name, ver, sum) in parse_lock_checksums(&lock) {
        let nv = format!("{crate_name}-{ver}");
        let out = dest.join(format!("{nv}.crate"));
        if crate::sha256::sha256_file(&out).ok().as_deref() == Some(sum.as_str()) {
            continue; // already warm + verified
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            eprintln!("td-builder check: warm {name} crates: TD_WARM_TIMEOUT budget exhausted — stopping");
            complete = false;
            break;
        }
        let url = format!("https://static.crates.io/crates/{crate_name}/{nv}.crate");
        // Pid-suffixed tmp: concurrent preludes (normal on this box) each
        // write their own, so one warm's rename never publishes bytes another
        // warm is still writing.
        let tmp = dest.join(format!("{nv}.crate.{}.tmp", std::process::id()));
        // td-fetch verifies the pin itself; its one success line STREAMS to
        // our stderr (the shell's `>&2` — a dup'd fd, never a pipe, so a
        // chatty child cannot deadlock the warm), and a fetch outliving the
        // budget is killed rather than left to stall the prelude.
        let mut cmd = Command::new(&tdf);
        cmd.args(["fetch", &url, &sum]).arg(&tmp).current_dir(root);
        {
            use std::os::fd::AsFd as _;
            if let Ok(err_fd) = std::io::stderr().as_fd().try_clone_to_owned() {
                cmd.stdout(Stdio::from(err_fd));
            }
        }
        let fetched = match cmd.spawn() {
            Ok(mut child) => wait_with_deadline(&mut child, deadline),
            Err(_) => false,
        };
        if fetched && crate::sha256::sha256_file(&tmp).ok().as_deref() == Some(sum.as_str()) {
            let _ = std::fs::rename(&tmp, &out);
        } else {
            let _ = std::fs::remove_file(&tmp);
            eprintln!("td-builder check: warm {name} crates: could not td-fetch/verify {nv}");
        }
    }
    let n = std::fs::read_dir(&dest)
        .map(|rd| {
            rd.flatten()
                .filter(|e| e.path().extension().is_some_and(|x| x == "crate"))
                .count()
        })
        .unwrap_or(0);
    eprintln!(
        "td-builder check: warm {name} crates: {n} crates in {} (td-fetched, Cargo.lock-pinned, guix-free){}",
        dest.display(),
        if complete { "" } else { " — INCOMPLETE (TD_WARM_TIMEOUT exhausted)" }
    );
}

/// The heavy-tier warm prelude: source-bootstrap tarballs + rust crate closures
/// (td-feed), all BEST-EFFORT (the gates enforce presence), fanned out in
/// batches of TD_WARM_JOBS exactly as the shell prelude did.
/// The td-tool crate-closure warm (td-fetch): host-side network PREP that
/// populates the offline vendor set `.td-build-cache/crate-vendor/{name}`
/// BEFORE the chain that consumes it is provisioned. This MUST run ahead of
/// `provision_userland` — the loop userland realizes the bootstrap chain
/// (mes → tcc → … → busybox/make). Gating the warm behind provisioning is the
/// deadlock the review flagged: provisioning fails at the first host-bash rung,
/// so a warm placed after it never runs. Best-effort (a missing fetcher / no
/// network warns; the recipe checks enforce presence).
fn warm_td_crate_closure(root: &Path) {
    // td-fetch's own crate closure (its own warm — not the cargo-proxy).
    warm_crate_closure(root, "fetch/Cargo.lock", "td-fetch");
}

/// Generate the host-produced Linux UAPI header seeds
/// (`$HOME/.td/sources/linux-headers-<ver>-{i386,x86_64}.tar`) the toolchain
/// rungs intern, before the `build-run` that interns them (heavy_warms runs later).
///
/// `newstore_bin` picks a td-built `td-feed` by store hash, so a pre-#536 binary
/// that still writes the seed as `.tar.gz` can win — it exits 0 yet the `.tar` the
/// intern needs never appears. So verify the `.tar` landed and retry any missing
/// arch once with the current source-built `feed/`. Best-effort and idempotent;
/// the intern is the fail-closed enforcement (issue #546).
fn warm_kernel_headers_seed(root: &Path) {
    const ARCHES: [&str; 2] = ["i386", "x86_64"];
    // Bound the cargo fallback like warm_td_crate_closure; the warm itself is
    // already `timeout`-wrapped.
    let deadline = warm_timeout_secs().map(|n| Instant::now() + Duration::from_secs(n));

    let newstore = newstore_bin(root, ".td-build-cache/td-feed/sd/newstore", "td-feed");
    let primary = match newstore.clone() {
        Some(p) => p,
        None => match host_cargo_bin(root, "feed", "td-feed", deadline) {
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

    let missing = warm_kh_arches(root, &primary, &ARCHES);
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
    let Some(fresh) = host_cargo_bin(root, "feed", "td-feed", deadline) else {
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

fn heavy_warms(root: &Path) {
    // The td-tool crate closure (td-fetch) is warmed earlier, before
    // provision_userland (warm_td_crate_closure) — not here.

    // Resolve ONE host td-feed binary: the gate's td-built one, else a host
    // cargo build of feed/.
    let Some(tdfeed) = newstore_bin(root, ".td-build-cache/td-feed/sd/newstore", "td-feed")
        .or_else(|| host_cargo_bin(root, "feed", "td-feed", None))
    else {
        eprintln!(
            "td-builder check: no td-feed binary for the heavy warm (build feed/ with cargo) — \
             skipping (best-effort; the heavy gates enforce presence)"
        );
        return;
    };
    let tdfeed = tdfeed.display().to_string();

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
/// start-time seed store DIR, default: derived from the declared seed-lock
/// paths (`seed_store_dir`) — content-scanned host-side for the
/// input closure, #267; only bare-drv requests use it), TD_BUILD_JOBS (the
/// global budget — inherited by the spawned daemon), TD_NICE (nice level for
/// the daemon + its build children, default 10).
/// The daemon's runtime dir (sockets, pid files, the blessed seed-closure db):
/// `TD_DAEMON_DIR` or `$HOME/.td/build-daemon`. Shared by `ensure_build_daemon`
/// (which creates state here) and the daemon/child verbs (which RE-DERIVE the
/// same paths rather than trusting an argv, re #469 round-8).
pub(crate) fn daemon_runtime_dir() -> Result<PathBuf, String> {
    match std::env::var("TD_DAEMON_DIR") {
        Ok(v) if !v.trim().is_empty() => Ok(PathBuf::from(v)),
        _ => {
            let home = std::env::var("HOME").map_err(|_| s("no HOME for TD_DAEMON_DIR"))?;
            Ok(Path::new(&home).join(".td/build-daemon"))
        }
    }
}

/// The DERIVED path of the blessed seed-closure db for (repo ROOT, SEED-DIR):
/// keyed by the repo's own checked-in seed-lock declarations, never by a
/// caller-supplied path. This is the round-8 origin-authentication fix for the
/// `BlessedSeedClosure` intake: the daemon and its `daemon-build`/`daemon-check`
/// children re-derive the db location from the same repo-declared inputs
/// `ensure_build_daemon` blessed under, so NO public interface accepts a
/// caller-selected db path as manifest authority — a raw path can no longer
/// become a typed origin. (A same-user writer can still forge the file at the
/// derived location; that trust-domain limit is the same one recorded on the
/// receipt layer, and a daemon-owned provenance db remains the follow-on,
/// re #472.) `None` when the repo declares no seed-lock roots — there is
/// nothing to bless, and strict staging fails closed on unvouched items.
pub(crate) fn blessed_seed_db_path(
    root: &Path,
    seed_dir: &str,
) -> Result<Option<PathBuf>, String> {
    let roots = seed_lock_roots(root);
    if roots.is_empty() {
        return Ok(None);
    }
    let mut h = crate::sha256::Sha256::new();
    h.update(seed_dir.as_bytes());
    for r in &roots {
        h.update(b"\n");
        h.update(r.as_bytes());
    }
    let bfull = crate::sha256::to_base16(&h.finalize());
    let bkey: String = bfull.chars().take(16).collect();
    Ok(Some(
        daemon_runtime_dir()?.join(format!("seed-bless.{bkey}.db")),
    ))
}

/// The host seed-store DIRECTORY every derived-bless consumer keys on:
/// `TD_DAEMON_SEED_DIR` (operator override) or the unique parent of the
/// repo's declared seed-lock paths. ONE derivation shared by the blesser
/// (`ensure_seed_bless`), the daemon (`ensure_build_daemon`), and the ladder
/// entrances' derived-db lookup (`derived_bless_db_auto` in main) — the
/// bless-db key includes this string, so every consumer must derive it the
/// same way or fail closed on a db that is not there.
pub(crate) fn daemon_seed_dir(root: &Path) -> Option<String> {
    match std::env::var("TD_DAEMON_SEED_DIR") {
        Ok(v) if !v.trim().is_empty() => Some(v),
        _ => seed_store_dir(&seed_lock_roots(root)),
    }
}

/// BLESS the declared seed closure once per root set (re #469): strict
/// staging manifests are unconditional, so every build that stages the
/// host-provisioned toolchain — the daemon's children AND the ladder's
/// `build-plan`/`build-recipe`, whose control-plane builder carries the §5
/// seed's glibc/gcc-lib in its runtime closure — needs a td-owned db
/// vouching that closure. The `seed-bless` verb derives the roots ITSELF
/// from the repo's checked-in seed-lock declarations (cwd = root below) —
/// no caller-supplied roots file exists to tamper with. The db lives at the
/// DERIVED `blessed_seed_db_path` location, keyed by those declared roots
/// (store paths embed content hashes, so a pin bump derives a new key and
/// re-blesses); an existing db is REUSED, never rewritten — re-blessing
/// would re-trust whatever is currently on disk, the existence-as-authority
/// hole the manifest closes. Never handed around: every consumer RE-DERIVES
/// the path from the same repo-declared inputs (re #469 round-8 — a
/// caller-selected db path is no longer an intake). Runs in the check
/// prelude BEFORE userland provisioning (whose recipe builds already need
/// the authority), and again from `ensure_build_daemon` (idempotent).
pub(crate) fn ensure_seed_bless(root: &Path, tb: &str) -> Result<(), String> {
    let Some(seed_dir) = daemon_seed_dir(root) else {
        eprintln!(
            "td-builder check: WARNING: no seed store dir (no declared seed-lock roots and \
             TD_DAEMON_SEED_DIR unset) — nothing to bless (re #469)"
        );
        return Ok(());
    };
    match blessed_seed_db_path(root, &seed_dir)? {
        None => {
            eprintln!(
                "td-builder check: WARNING: no declared seed-lock roots to bless — strict \
                 builds will red on unvouched closure items (re #469)"
            );
        }
        Some(db) => {
            if !db.exists() {
                eprintln!(
                    "td-builder check: blessing the declared seed closure into {} — \
                     a one-time hash of the pinned toolchain (re #469)",
                    db.display()
                );
                if let Some(parent) = db.parent() {
                    std::fs::create_dir_all(parent)
                        .map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
                }
                // Pid-unique tmp + atomic rename: concurrent blessers (two
                // agents' preludes) each write their own tmp of the SAME
                // deterministic closure; whichever renames last wins with
                // identical content, and no reader ever sees a partial db.
                let tmp = db.with_extension(format!("db.tmp.{}", std::process::id()));
                let out = Command::new(tb)
                    .args(["seed-bless", &seed_dir])
                    .arg(&tmp)
                    .current_dir(root)
                    .output()
                    .map_err(|e| format!("spawn seed-bless: {e}"))?;
                if !out.status.success() {
                    let _ = std::fs::remove_file(&tmp);
                    return Err(format!(
                        "seed-bless failed: {}",
                        String::from_utf8_lossy(&out.stderr)
                    ));
                }
                std::fs::rename(&tmp, &db)
                    .map_err(|e| format!("rename {} -> {}: {e}", tmp.display(), db.display()))?;
            }
        }
    }
    Ok(())
}

fn ensure_build_daemon(root: &Path, tb: &str) -> Result<String, String> {
    let daemon_dir = daemon_runtime_dir()?;
    let store = daemon_dir.join("store");
    std::fs::create_dir_all(&store).map_err(|e| format!("mkdir {}: {e}", store.display()))?;
    // Default seed store = DERIVED from the declared seed-lock paths, read
    // HOST-SIDE by the daemon (never a hardcoded prefix; the loop SANDBOX
    // never mounts it). A lock-less host must say where its seed lives.
    let seed_dir = daemon_seed_dir(root).ok_or_else(|| {
        s("no seed store dir: the seed locks name no existing paths and \
           TD_DAEMON_SEED_DIR is unset")
    })?;
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
        std::fs::symlink_metadata(p)
            .map(|m| m.file_type().is_socket())
            .unwrap_or(false)
    };
    if pid_alive(&pid_f) && is_socket(&sock) {
        return Ok(sock.display().to_string());
    }

    // The blessed seed closure must exist before this daemon's children build
    // (idempotent — `ensure_seed_bless` reuses an existing db; the prelude
    // normally blessed already, but the daemon path stays self-sufficient).
    ensure_seed_bless(root, &daemon_tb)?;

    // Start a fresh daemon, detached in its OWN process group so it outlives
    // this check AND survives the terminal's ^C/hangup signals (the machine-
    // wide limiter must persist across checks — the shell's `nohup` role).
    // nice/ionice it so its build children (the corpus builds — the real
    // CPU/IO) yield to interactive work; the global budget bounds how MANY run
    // at once. TD_BUILD_JOBS reaches the daemon by plain env inheritance.
    let log =
        std::fs::File::create(&log_f).map_err(|e| format!("create {}: {e}", log_f.display()))?;
    let log2 = log.try_clone().map_err(|e| format!("clone log fd: {e}"))?;
    let _ = std::fs::remove_file(&sock);
    let tdnice = std::env::var("TD_NICE").unwrap_or_else(|_| s("10"));
    let mut argv: Vec<String> = Vec::new();
    if let Some(nice) = find_in_path("nice") {
        argv.extend([nice.display().to_string(), s("-n"), tdnice]);
        if let Some(ionice) = find_in_path("ionice") {
            argv.extend([ionice.display().to_string(), s("-c2"), s("-n7")]);
        }
    }
    argv.extend([
        daemon_tb,
        s("daemon"),
        sock.display().to_string(),
        seed_dir,
        store.display().to_string(),
    ]);
    // The blessed seed-closure db is NOT an argument: the daemon's build
    // children re-derive its location from the repo's own seed-lock
    // declarations (`blessed_seed_db_path` over their cwd = root, spawned
    // below with `current_dir(root)`), so no argv or env channel can add
    // manifest authority (re #469 round-8 origin authentication).
    let (head, rest) = argv
        .split_first()
        .ok_or_else(|| s("internal: empty daemon argv"))?;
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
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("spawn the build daemon: {e}"))?;
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
    Err(format!(
        "the daemon did not bind {}:\n{tail}",
        sock.display()
    ))
}

pub fn cli(args: &[String]) -> ExitCode {
    match run(args) {
        Ok(code) => ExitCode::from(code.clamp(0, 255) as u8),
        Err(e @ CheckError::Unprovisioned(_)) => {
            eprintln!("{e}");
            ExitCode::from(EXIT_UNPROVISIONED as u8)
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
    if goals.is_empty() {
        goals.push("check".to_string());
    }

    guard_netns_probe()?;

    // No guix process and no host tool remains: the loop PATH is only the
    // td-BUILT userland (busybox/make) plus td-builder's native applets. Gate
    // text/tree work must invoke td-builder typed helpers or that userland —
    // never GNU sed/grep/findutils from a seed lock.

    // Light tiers own no heavy gate — skip the heavy warms + daemon (exactly the
    // shell prelude's goal scan).
    let heavy_warm = goals.iter().any(|g| {
        !matches!(
            g.as_str(),
            "check-fast" | "check-engine" | "list-gates" | "gate-timing-report"
        )
    });

    let tb = provision_stage0(&root)?;
    // Bless the declared seed closure BEFORE any recipe build: userland
    // provisioning realizes the chain through build-plan, whose staging
    // manifest needs the blessed db to vouch the control-plane builder's
    // host-seed runtime closure (glibc/gcc-lib) — blessing only at daemon
    // ensure time (after userland) deadlocked a cold host (re #469 round-8).
    ensure_seed_bless(&root, &tb)?;
    let ul = provision_userland(&root)?;
    let toolchain = loop_path_with_native_applets(&root, &tb, &ul.path).map_err(|e| {
        CheckError::Fatal(fatal(&format!(
            "could not provision loop native applets ({e})"
        )))
    })?;

    let mut child_envs: Vec<(String, String)> = vec![(s("PATH"), toolchain)];
    // The runner's knobs must cross the sandbox boundary (host-sandbox
    // preserves the TD_CHECK_ prefix): without this, TD_CHECK_SLOTS=… ./check.sh
    // would be silently dead and gate-run would always default to nproc.
    // TD_CHECK_DISABLE forwards the gate-disable list (gate names / `pool:<name>`
    // tokens) so `TD_CHECK_DISABLE=… td-builder check` reaches the in-sandbox runner.
    // Every build uses the one shared warm ladder (re #469) — there is no
    // cold-cache toggle to forward; `td-recipe-eval clear-store` is the only way
    // to force a cold climb.
    for k in [
        "TD_CHECK_SLOTS",
        "TD_CHECK_SLOTS_DIR",
        "TD_CHECK_JOBS",
        "TD_CHECK_DISABLE",
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
    match tree_key(&root) {
        Some(key) => child_envs.push((s("TD_CHECK_TREE"), key)),
        None if resume => {
            return Err(fatal(
                "--resume needs a git working tree to key the verdict journal, and `git` failed here — cannot prove the tree is unchanged, refusing to skip",
            )
            .into())
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
    // the gate runner's slot pool machine-wide. The shared ladder lives under the
    // same bind (created lazily by the first build), so it too is reachable at the
    // same absolute path, RW, from every check sandbox.
    if let Ok(home) = std::env::var("HOME") {
        let _ = std::fs::create_dir_all(Path::new(&home).join(".td/build-daemon/slots"));
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
    if let Some(nice) = find_in_path("nice") {
        argv.extend([nice.display().to_string(), s("-n"), tdnice]);
        if let Some(ionice) = find_in_path("ionice") {
            argv.extend([ionice.display().to_string(), s("-c2"), s("-n7")]);
        }
    }
    // The sandbox mounts NO store directory and NO host tool — only the loop's
    // declared input ITEMS, each bound read-only (the drv build jail's
    // input-only model): the td-BUILT userland at its /td/store paths, the
    // seed locks' declared closures, and the closures of the host-built
    // binaries that run inside (the stage0 td-builder; the stashed td-subst
    // when exposed).
    let roots = seed_lock_roots(&root);
    let items = loop_store_items(&root, &roots, &loop_scan_files(&root, &tb)).map_err(|e| {
        fatal(&format!(
            "could not compute the loop sandbox's store-item inputs ({e})"
        ))
    })?;
    argv.extend([
        tb.clone(),
        s("host-sandbox"),
        s("--expose-cwd"),
        s("--no-daemon"),
    ]);
    for it in &items {
        argv.extend([s("--store-item"), it.clone()]);
    }
    for (src, dest) in &ul.items {
        argv.extend([s("--store-item-at"), src.clone(), dest.clone()]);
    }
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
/// env_clear + C locale, the read-only per-item store binds) surface in MINUTES against
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
/// The sandbox + userland provisioning is EXACTLY the loop prelude's (same
/// stage0 container provider, same td-built busybox/make userland — notably
/// WITHOUT bzip2, so a missing-bzip2 bug still reproduces).
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
    if !root.join("tests").is_dir() {
        return Err(fatal("run from the repo root (tests/ not found)"));
    }
    let tb = provision_stage0(&root).map_err(|e| {
        format!("check-rung: FATAL: could not provision the guix-free stage0 td-builder for the sandbox ({e})")
    })?;
    // check-rung is a dev helper, not the loop: it does not branch on the
    // provisioned/regression distinction, so collapse CheckError back to a string.
    let ul = provision_userland(&root).map_err(|e| e.to_string())?;
    let toolchain = loop_path_with_native_applets(&root, &tb, &ul.path)
        .map_err(|e| format!("check-rung: FATAL: could not provision loop native applets ({e})"))?;
    // The same input-only store exposure as the loop (per-item binds, no store
    // directory mounted; same scan-file set, recomputed from the live scan).
    let roots = seed_lock_roots(&root);
    let items = loop_store_items(&root, &roots, &loop_scan_files(&root, &tb))
        .map_err(|e| format!("check-rung: FATAL: {e}"))?;
    eprintln!(
        ">> check-rung: {harness} inside td-builder host-sandbox (cached chain reused; \
         sandbox env matches the gate)"
    );
    let mut cmd = Command::new(&tb);
    let mut sandbox_args: Vec<String> =
        vec![s("host-sandbox"), s("--expose-cwd"), s("--no-daemon")];
    for it in &items {
        sandbox_args.extend([s("--store-item"), it.clone()]);
    }
    for (src, dest) in &ul.items {
        sandbox_args.extend([s("--store-item-at"), src.clone(), dest.clone()]);
    }
    sandbox_args.extend([s("--"), s("sh")]);
    cmd.args(sandbox_args)
    .arg(harness)
    .args(rest)
    .env("PATH", toolchain)
    .env("TD_BUILDER_SELF", &tb)
    .current_dir(&root);
    // Replace this process, exactly as the shell helper's `exec` did.
    use std::os::unix::process::CommandExt as _;
    let e = cmd.exec();
    Err(fatal(&format!(
        "check-rung: could not exec the sandbox: {e}"
    )))
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

    // The blessed-seed db location is a PURE FUNCTION of the repo's seed-lock
    // declarations + the seed dir (re #469 round-8): the daemon and its
    // children DERIVE it — there is no argv/env channel that can point the
    // BlessedSeedClosure origin at a caller-selected db. Same declarations →
    // same basename; different seed dir or root set → different basename; no
    // declarations → no bless db at all.
    #[test]
    fn blessed_seed_db_path_derives_from_declarations_only() {
        let d = scratch("bless-derive");
        // Two fake declared seed roots that exist on disk (seed_lock_roots
        // keeps only existing absolute paths).
        let (r1, r2) = (d.join("store").join("aaa-tool"), d.join("store").join("bbb-lib"));
        std::fs::create_dir_all(d.join("store")).unwrap();
        std::fs::write(&r1, b"t").unwrap();
        std::fs::write(&r2, b"l").unwrap();
        let repo = d.join("repo");
        std::fs::create_dir_all(repo.join("tests")).unwrap();
        std::fs::write(
            repo.join("tests/td-builder-rust.lock"),
            format!("tool {}\nlib {}\n", r1.display(), r2.display()),
        )
        .unwrap();
        let name_at = |root: &Path, seed_dir: &str| {
            blessed_seed_db_path(root, seed_dir)
                .unwrap()
                .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        };
        let a = name_at(&repo, "/seed/dir").unwrap();
        assert_eq!(a, name_at(&repo, "/seed/dir").unwrap(), "must be deterministic");
        assert!(
            a.starts_with("seed-bless.") && a.ends_with(".db"),
            "derived name shape: {a}"
        );
        assert_ne!(
            a,
            name_at(&repo, "/other/dir").unwrap(),
            "a different seed dir must derive a different bless db"
        );
        // Dropping a declared root changes the derivation too.
        std::fs::write(
            repo.join("tests/td-builder-rust.lock"),
            format!("tool {}\n", r1.display()),
        )
        .unwrap();
        assert_ne!(a, name_at(&repo, "/seed/dir").unwrap());
        // No declarations → None: nothing to bless, nothing to derive.
        let bare = d.join("bare-repo");
        std::fs::create_dir_all(&bare).unwrap();
        assert_eq!(blessed_seed_db_path(&bare, "/seed/dir").unwrap(), None);
        std::fs::remove_dir_all(&d).ok();
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
    fn parse_seed_lock_keeps_absolute_paths_deduped() {
        // The lock is the DECLARED seed, whatever store its paths live in —
        // any absolute path qualifies; only relative/malformed lines drop.
        let lock = "\
# comment line
aaa-rust-1.93.0 /td/store/aaa-rust-1.93.0
bbb-cargo /td/store/bbb-rust-1.93.0-cargo extra-field
bbb-again /td/store/bbb-rust-1.93.0-cargo

malformed-line-without-path
ccc-relative not/absolute
";
        assert_eq!(
            parse_seed_lock(lock),
            vec![
                "/td/store/aaa-rust-1.93.0".to_string(),
                "/td/store/bbb-rust-1.93.0-cargo".to_string(),
            ]
        );
    }

    #[test]
    fn seed_store_dir_derives_the_unique_parent() {
        // The candidate store dir comes from the declared lock paths, never a
        // hardcoded prefix.
        let roots = vec![
            "/td/store/aaa-bash-5.2.37".to_string(),
            "/td/store/bbb-make-4.4.1".to_string(),
        ];
        assert_eq!(seed_store_dir(&roots), Some("/td/store".to_string()));
        // Several parents: the FIRST wins (warned) — the seed is one store.
        let mixed = vec![
            "/td/store/aaa-bash-5.2.37".to_string(),
            "/other/store/ccc-sed-4.9".to_string(),
        ];
        assert_eq!(seed_store_dir(&mixed), Some("/td/store".to_string()));
        // No roots, or roots with no usable parent: no store dir.
        assert_eq!(seed_store_dir(&[]), None);
        assert_eq!(seed_store_dir(&["/toplevel".to_string()]), None);
    }

    #[test]
    fn userland_map_validates_every_stem_or_reprovisions() {
        let d = scratch("loop-userland");
        let map = d.join("loop-userland.map");
        let fp = "f".repeat(64);
        // A valid map: the current fingerprint, every stem present, each item
        // dir with a bin/ and a recorded NAR hash its bytes verify against.
        let mut content = format!("fingerprint {fp}\n");
        for (i, stem) in LOOP_USERLAND_STEMS.iter().enumerate() {
            let base = format!("hash{i}-{stem}-1.0");
            let bin = d.join(&base).join("bin");
            std::fs::create_dir_all(&bin).unwrap();
            std::fs::write(bin.join("tool"), format!("#!{stem}\n")).unwrap();
            let want = crate::sandbox::nar_hash_of(&d.join(&base)).unwrap();
            content.push_str(&format!("{stem} {base} {want}\n"));
        }
        std::fs::write(&map, &content).unwrap();
        let ul = read_userland_map(&d, &map, &fp).expect("valid map resolves");
        assert_eq!(ul.items.len(), LOOP_USERLAND_STEMS.len());
        for ((host, canon), stem) in ul.items.iter().zip(LOOP_USERLAND_STEMS) {
            assert!(host.starts_with(d.to_str().unwrap()), "host copy under the durable dir");
            assert_eq!(canon, &format!("{TD_STORE_DIR}/hash{}-{stem}-1.0",
                LOOP_USERLAND_STEMS.iter().position(|s| s == stem).unwrap()));
            assert!(ul.path.contains(&format!("{canon}/bin")), "bin dir on the PATH");
        }
        // A fingerprint mismatch invalidates the WHOLE map: a userland built
        // by any other evaluator — e.g. one that still admitted host
        // scaffolding — is never grandfathered in.
        assert!(read_userland_map(&d, &map, &"0".repeat(64)).is_none());
        // TAMPERED bytes are a cache miss, not a mount: flip one byte in an
        // item and the recorded NAR hash no longer verifies (re #469).
        let item0 = d.join(format!("hash0-{}-1.0", LOOP_USERLAND_STEMS[0]));
        std::fs::write(item0.join("bin/tool"), b"#!poisoned\n").unwrap();
        assert!(read_userland_map(&d, &map, &fp).is_none());
        std::fs::write(
            item0.join("bin/tool"),
            format!("#!{}\n", LOOP_USERLAND_STEMS[0]),
        )
        .unwrap();
        assert!(read_userland_map(&d, &map, &fp).is_some(), "restored bytes verify again");
        // A stem whose item lost its bin/ invalidates the whole map (the
        // caller re-provisions) — no partial userland.
        std::fs::remove_dir_all(item0.join("bin")).unwrap();
        assert!(read_userland_map(&d, &map, &fp).is_none());
        // A path-carrying value is rejected (the map holds basenames only), as
        // is a hash-less line (the pre-hash map format self-invalidates) and a
        // map with no fingerprint line at all.
        std::fs::write(
            &map,
            format!("fingerprint {fp}\nbusybox-x86-64 ../escape sha256:aa\nmake-x86-64 x sha256:aa\n"),
        )
        .unwrap();
        assert!(read_userland_map(&d, &map, &fp).is_none());
        std::fs::write(
            &map,
            format!("fingerprint {fp}\nbusybox-x86-64 x\nmake-x86-64 x\n"),
        )
        .unwrap();
        assert!(read_userland_map(&d, &map, &fp).is_none());
        std::fs::write(&map, "busybox-x86-64 x sha256:aa\nmake-x86-64 x sha256:aa\n").unwrap();
        assert!(read_userland_map(&d, &map, &fp).is_none());
        // A missing map is simply cold.
        assert!(read_userland_map(&d, &d.join("absent.map"), &fp).is_none());
    }

    // The userland freshness key covers the evaluator binary AND the in-repo
    // seed patches: patches are chain inputs the runner reads from the TREE at
    // build time, so a patch-only change (binary byte-identical) must re-key
    // the userland — a stale one could false-green the patch under review.
    #[test]
    fn userland_fingerprint_keys_on_evaluator_and_seed_patches() {
        let d = scratch("userland-fp");
        let eval = d.join("td-recipe-eval");
        std::fs::write(&eval, b"evaluator-v1").unwrap();
        let eval_s = eval.to_str().unwrap().to_string();
        let root = d.join("tree");
        std::fs::create_dir_all(root.join("seed/patches")).unwrap();
        let fp0 = userland_fingerprint(&root, &eval_s).unwrap();
        // A patch appearing re-keys; its CONTENT changing re-keys again.
        std::fs::write(root.join("seed/patches/a.patch"), b"-x\n+y\n").unwrap();
        let fp1 = userland_fingerprint(&root, &eval_s).unwrap();
        assert_ne!(fp0, fp1, "a new seed patch re-keys the userland");
        std::fs::write(root.join("seed/patches/a.patch"), b"-x\n+z\n").unwrap();
        let fp2 = userland_fingerprint(&root, &eval_s).unwrap();
        assert_ne!(fp1, fp2, "a patch-only change re-keys the userland");
        // Non-.patch files in the dir do not key; a tree with NO patch dir
        // keys on the binary alone.
        std::fs::write(root.join("seed/patches/README"), b"notes").unwrap();
        assert_eq!(fp2, userland_fingerprint(&root, &eval_s).unwrap());
        let bare = d.join("bare");
        std::fs::create_dir_all(&bare).unwrap();
        let fp_bare = userland_fingerprint(&bare, &eval_s).unwrap();
        assert_ne!(fp_bare, fp2);
        // The evaluator's bytes still key.
        std::fs::write(&eval, b"evaluator-v2").unwrap();
        assert_ne!(fp2, userland_fingerprint(&root, &eval_s).unwrap());
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
    fn parse_lock_checksums_takes_only_checksummed_packages() {
        // The root (path) crate carries no checksum and must be excluded; the
        // vendored crates-io deps carry one each.
        let lock = "\
# This file is automatically @generated by Cargo.\n\
version = 3\n\
\n\
[[package]]\n\
name = \"adler2\"\n\
version = \"2.0.0\"\n\
source = \"registry+https://github.com/rust-lang/crates.io-index\"\n\
checksum = \"512761e0bb2578dd7380c6baaa0f4ce03e84f95e960231d1dec8bf4d7d6e2627\"\n\
\n\
[[package]]\n\
name = \"td-fetch\"\n\
version = \"0.1.0\"\n\
dependencies = [\n\
 \"ureq\",\n\
]\n\
\n\
[[package]]\n\
name = \"ureq\"\n\
version = \"2.10.1\"\n\
source = \"registry+https://github.com/rust-lang/crates.io-index\"\n\
checksum = \"b74fc6b57825be3373f7054754755f03ac3a8f5d70015f0ffa7ebd06bfeeeb67\"\n";
        let got = parse_lock_checksums(lock);
        assert_eq!(
            got,
            vec![
                (
                    "adler2".to_string(),
                    "2.0.0".to_string(),
                    "512761e0bb2578dd7380c6baaa0f4ce03e84f95e960231d1dec8bf4d7d6e2627".to_string()
                ),
                (
                    "ureq".to_string(),
                    "2.10.1".to_string(),
                    "b74fc6b57825be3373f7054754755f03ac3a8f5d70015f0ffa7ebd06bfeeeb67".to_string()
                ),
            ]
        );
    }

    #[test]
    fn parse_lock_checksums_covers_the_real_td_fetch_lock() {
        // The td-fetch recipe check asserts ≥70 vendored crates in the warmed dir;
        // the parser must see at least that many in the real fetch/Cargo.lock
        // (drift guard: a lockfile-format change that blinds the parser reds
        // here, not as a silently-cold warm).
        let lock =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../fetch/Cargo.lock"))
                .unwrap();
        let got = parse_lock_checksums(&lock);
        assert!(
            got.len() >= 70,
            "only {} checksummed packages parsed",
            got.len()
        );
        assert!(
            got.iter()
                .all(|(n, v, s)| !n.is_empty() && !v.is_empty() && s.len() == 64),
            "malformed triplet parsed from the real lock"
        );
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

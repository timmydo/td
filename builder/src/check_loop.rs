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
//!   3. the loop PATH: host-provided tools from `LOOP_TOOLCHAIN`,
//!   4. the warm prelude (subst store, source/crate warms, build daemon),
//!   5. the machine-wide slot dir, and
//!   6. the sandboxed gate run: TB host-sandbox --expose-cwd --no-daemon
//!      --store-item ITEM… -- TB gate-run. The sandbox mounts NO store
//!      directory: only the loop's declared input ITEMS — the resolved
//!      toolchain closure (`loop_store_items`) — each bound read-only at its
//!      own path, the drv build jail's input-only model.
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

/// Exit code `td-builder check` uses when it aborts because the RUNNER is not
/// provisioned to run the loop at all (the base loop toolchain does not resolve
/// on PATH) — as
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
    let applets = current_binary_native_applet_path(root)?;
    let path = std::env::var("PATH").unwrap_or_default();
    let self_exe = std::env::current_exe().map_err(|e| {
        fatal(&format!(
            "could not resolve current td-builder executable: {e}"
        ))
    })?;
    let out = run_capture(
        Command::new("sh")
            .arg("-c")
            .arg(". tests/cache-lib.sh && provision_stage0 1>&2 && printf '%s' \"$TB\"")
            .env("PATH", format!("{applets}:{path}"))
            .env("TD_BUILDER_SELF", &self_exe)
            .current_dir(root),
    )
    .map_err(|e| {
        fatal(&format!(
            "could not provision the guix-free stage0 td-builder for the loop sandbox ({e})"
        ))
    })?;
    let tb = out.trim().to_string();
    if tb.is_empty() || !Path::new(&tb).is_file() {
        return Err(fatal("stage0 provisioning returned no usable $TB"));
    }
    Ok(tb)
}

fn current_binary_native_applet_path(root: &Path) -> Result<String, String> {
    let current = std::env::current_exe()
        .map_err(|e| {
            fatal(&format!(
                "cannot resolve current td-builder executable: {e}"
            ))
        })?
        .display()
        .to_string();
    native_applet_path(root, &current)
        .map_err(|e| fatal(&format!("could not provision stage0 native applets ({e})")))
}

/// The host store prefix the loop toolchain must resolve under. The loop
/// sandbox never mounts this (or any) store DIRECTORY: the prelude computes
/// the resolved toolchain's runtime closure (`loop_store_items`) and binds
/// each closure ITEM read-only at its own path (`--store-item`), and binds
/// NOTHING else of the host FS (no /usr, /bin, /home). A toolchain bin dir is
/// therefore reachable INSIDE the sandbox only if it physically lies under
/// this prefix — a `/usr/bin` tool on a foreign-distro guix host would vanish.
const SANDBOX_STORE_PREFIX: &str = "/gnu/store/";

/// The loop toolchain: the host-PATH tools the loop sandbox needs to run its
/// gate bodies (make/sh/…). A list of representative BINARIES, one per package —
/// resolving `env` gives the whole coreutils bin dir. sed/grep/findutils are
/// deliberately absent; loop checks must use td-builder typed helpers or
/// td-built userland instead. tar/gzip are absent too: no gate body spawns
/// them — loop-level unpacking is td-builder's own native tar.rs/gzip.rs, and
/// the drv-sandbox tar runs from a derivation's DECLARED inputs, never this
/// PATH. Native loop applets for syscall-only host tools (`mount --bind`,
/// `flock`) are provided by td-builder itself and prepended by
/// `loop_path_with_native_applets`.
const LOOP_TOOLCHAIN: &[&str] = &["make", "bash", "sh", "env"];

/// The resolved loop toolchain: the sandbox PATH (deduped in-store bin dirs,
/// colon-joined) plus the store ITEMS those dirs live under — the closure
/// ROOTS `loop_store_items` expands into the sandbox's per-item binds.
struct LoopToolchain {
    path: String,
    roots: Vec<String>,
}

/// The core loop toolchain, resolved from the HOST PATH: the host brings
/// only the base process-driving tools (the "check the right tools are on $PATH"
/// model), exactly as it already brings the rust/cc toolchain the stage0 seed
/// build resolves via tools/provision-{rust,cc}.sh — no `guix shell` subprocess.
/// For each expected tool in `LOOP_TOOLCHAIN` we find it on PATH and
/// CANONICALIZE to its real bin dir. Canonicalization + the store-prefix check
/// matter — the loop sandbox binds ONLY the resolved toolchain's closure items
/// (under SANDBOX_STORE_PREFIX) over a fresh tmpfs, so a profile-symlink dir
/// (~/.guix-home/profile/bin) OR a distro dir (/usr/bin on a Debian+guix host)
/// would not resolve inside; only the real `/gnu/store/<pkg>/bin` target does.
/// The deduped in-store dirs become the sandbox PATH; their store items become
/// closure roots.
///
/// A tool that is ABSENT from the host PATH, or that resolves OUTSIDE the bound
/// store (so it would vanish inside the sandbox), is reported in one loud warning
/// line (a misconfigured runner is visible) but is NOT fatal for a heavy-only
/// tool: the gate that needs it fails loudly, exactly as the best-effort warms
/// let their gates enforce presence — a host missing mount must still run
/// check-engine/check-pr. (With the list currently equal to the core set, the
/// heavy-only warn branch is vacuous; it is the contract for the next heavy
/// addition, not a live path.) Fatal ONLY when a CORE tool (sh/bash/make/env) failed
/// to resolve to an in-store bin dir — without those no gate body runs at all,
/// and that fatal is a `CheckError::Unprovisioned`, so `cli()` exits
/// `EXIT_UNPROVISIONED`: the machine signal `td-builder daily` reads to classify
/// this as a runner-provisioning gap rather than a code regression.
fn provision_toolchain() -> Result<LoopToolchain, CheckError> {
    let path_var = std::env::var("PATH").unwrap_or_default();
    let mut dirs: Vec<String> = Vec::new();
    let mut resolved: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut missing: Vec<&str> = Vec::new(); // not on PATH at all
    let mut off_store: Vec<&str> = Vec::new(); // on PATH but NEVER under the bound store
    for &t in LOOP_TOOLCHAIN {
        // Scan EVERY PATH entry for the tool and take the FIRST whose REAL dir is under the
        // store the sandbox binds — not just the first PATH hit. On a guix-on-foreign-distro
        // host /usr/bin/env may precede the in-store env; the first hit is off-store but a
        // usable in-store copy is later on PATH, so stopping at the first match would
        // false-fatal a loop the sandbox could actually run. A dir the sandbox never exposes
        // is worse than useless (Ok now, `command not found` for every gate later).
        let mut found_on_path = false;
        let mut in_store: Option<String> = None;
        for dir in path_var.split(':').filter(|d| !d.is_empty()) {
            let p = Path::new(dir).join(t);
            if !p.is_file() {
                continue;
            }
            found_on_path = true;
            if let Some(real) = std::fs::canonicalize(&p)
                .ok()
                .and_then(|c| c.parent().map(|d| d.display().to_string()))
            {
                if real.starts_with(SANDBOX_STORE_PREFIX) {
                    in_store = Some(real);
                    break;
                }
            }
        }
        match in_store {
            Some(dir) => {
                resolved.insert(t);
                if !dirs.contains(&dir) {
                    dirs.push(dir); // dedupe (e.g. sh + bash share one bin dir)
                }
            }
            None if found_on_path => off_store.push(t),
            None => missing.push(t),
        }
    }
    // Core tools every gate body needs — checked against what ACTUALLY resolved to
    // an in-store dir (mere presence on PATH is not enough: a /usr/bin bash is
    // invisible inside the sandbox). A host without them cannot run the loop.
    for core in ["sh", "bash", "make", "env"] {
        if !resolved.contains(core) {
            return Err(CheckError::Unprovisioned(fatal(&format!(
                "loop toolchain: core tool `{core}` did not resolve to a path under \
                 {SANDBOX_STORE_PREFIX} on the host PATH — the loop sandbox exposes only \
                 the resolved toolchain's store items (not /usr/bin etc.), so the base \
                 userland (bash/coreutils/make) must be on PATH FROM there, e.g. a guix \
                 profile. host-brings-the-tools; LOOP_TOOLCHAIN in check_loop.rs"
            ))));
        }
    }
    if !missing.is_empty() || !off_store.is_empty() {
        let mut why = String::new();
        if !missing.is_empty() {
            why.push_str(&format!("not on PATH: {}", missing.join(" ")));
        }
        if !off_store.is_empty() {
            if !why.is_empty() {
                why.push_str("; ");
            }
            why.push_str(&format!(
                "on PATH but outside {SANDBOX_STORE_PREFIX} (invisible in the sandbox): {}",
                off_store.join(" ")
            ));
        }
        eprintln!(
            "td-builder check: loop toolchain: {} heavy-only tool(s) unavailable ({why}); \
             the gates that need them will fail loudly — expose them under \
             {SANDBOX_STORE_PREFIX} on the runner PATH (host-brings-the-tools; \
             LOOP_TOOLCHAIN in check_loop.rs)",
            missing.len() + off_store.len()
        );
    }
    if dirs.is_empty() {
        return Err(CheckError::Unprovisioned(fatal(
            "loop toolchain: no expected tool resolved to an in-store bin dir on the host PATH",
        )));
    }
    let mut roots: Vec<String> = Vec::new();
    for d in &dirs {
        if let Some(item) = store_item_of(d) {
            if !roots.contains(&item) {
                roots.push(item);
            }
        }
    }
    Ok(LoopToolchain {
        path: dirs.join(":"),
        roots,
    })
}

/// The store ITEM a resolved bin dir lives under: `/gnu/store/<item>/bin` →
/// `/gnu/store/<item>` — the whole package is the closure root, not just its
/// bin dir (the package's lib/, libexec/, share/ are runtime surface too).
fn store_item_of(dir: &str) -> Option<String> {
    let rest = dir.strip_prefix(SANDBOX_STORE_PREFIX)?;
    let item = rest.split('/').next().filter(|c| !c.is_empty())?;
    Some(format!("{SANDBOX_STORE_PREFIX}{item}"))
}

/// The store paths of the rust/cc SEED toolchain lock (tests/td-builder-rust.lock,
/// format `NAME <store-path>`): gate bodies resolve these INSIDE the sandbox
/// (tools/provision-{rust,cc}.sh branch 2 reads the lock and execs its paths
/// directly), so every lock path present on the host is a DECLARED loop input
/// and joins the closure roots. An absent path is skipped — the provision
/// scripts fall through the same way, and the gate that needs it fails loudly.
fn seed_lock_roots(root: &Path) -> Vec<String> {
    let Ok(content) = std::fs::read_to_string(root.join("tests/td-builder-rust.lock")) else {
        return Vec::new();
    };
    parse_seed_lock(&content)
        .into_iter()
        .filter(|p| Path::new(p).is_dir())
        .collect()
}

/// Parse the seed lock's `NAME <store-path>` lines (comments/blank skipped) into
/// the deduped path list, keeping only paths under the loop's store prefix.
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
        if path.starts_with(SANDBOX_STORE_PREFIX) && !paths.iter().any(|p| p == path) {
            paths.push(path.to_string());
        }
    }
    paths
}

/// A parsed `.td-build-cache/loop-closure.list`: the scanned host-built
/// binaries as `(path, mtime_ns, len, store refs)`, the merged closure ROOTS,
/// and the resulting closure ITEMS.
struct ClosureCache {
    files: Vec<(String, u64, u64, Vec<String>)>,
    roots: std::collections::BTreeSet<String>,
    items: Vec<String>,
}

/// Everything the loop sandbox mounts from the host store — the full input set,
/// computed so the sandbox never mounts a store DIRECTORY, only declared items:
///
///   1. the runtime CLOSURE of `roots` (the resolved LOOP_TOOLCHAIN packages +
///      the seed lock's rust/cc toolchain — see `seed_lock_roots`), and
///   2. the closure of each `scan_files` binary: host-built ELF executables
///      that RUN INSIDE the sandbox (the stage0 td-builder, the daily's stashed
///      td-subst), whose glibc/gcc-lib references are found by content-scanning
///      the binary itself — declared, not assumed to coincide with (1).
///
/// The closure walk is the same no-DB content scan realize_drv uses
/// (`scan_candidate_index` + `scan_closure_hybrid`; never /var/guix). The
/// result is CACHED in `.td-build-cache/loop-closure.list`: store items are
/// immutable, so the cache holds while the root set matches, every scanned
/// file's (mtime, len) matches, and every item still exists — a rebuilt stage0
/// re-scans one file; a changed root set or a GC'd item re-scans the closure.
/// Returns the sorted item paths (roots included).
fn loop_store_items(
    root: &Path,
    roots: &[String],
    scan_files: &[String],
) -> Result<Vec<String>, String> {
    let store_dir = SANDBOX_STORE_PREFIX.trim_end_matches('/');
    let cache_path = root.join(".td-build-cache/loop-closure.list");
    let cached = read_closure_cache(&cache_path);

    // The scanner (a readdir of the whole host store + the candidate index) is
    // built lazily, at most once — the warm path (nothing changed) never pays it.
    let mut scan_state: Option<(
        crate::scan::Scanner,
        std::collections::HashMap<String, String>,
    )> = None;

    let mut all_roots: std::collections::BTreeSet<String> = roots.iter().cloned().collect();
    let mut files: Vec<(String, u64, u64, Vec<String>)> = Vec::with_capacity(scan_files.len());
    for f in scan_files {
        let (mtime, len) = file_sig(f)?;
        let cached_refs = cached.as_ref().and_then(|c| {
            c.files
                .iter()
                .find(|(p, m, l, _)| p == f && *m == mtime && *l == len)
                .map(|(_, _, _, refs)| refs.clone())
        });
        let refs = match cached_refs {
            Some(refs) => refs,
            None => {
                let st = ensure_scanner(&mut scan_state, store_dir)?;
                st.0.reset();
                crate::nar::write_nar(&mut st.0, Path::new(f))
                    .map_err(|e| format!("content-scan {f}: {e}"))?;
                st.0.refs()
            }
        };
        all_roots.extend(refs.iter().cloned());
        files.push((f.clone(), mtime, len, refs));
    }

    if let Some(c) = &cached {
        if c.roots == all_roots && c.items.iter().all(|i| Path::new(i).exists()) {
            // Same closure; refresh the file signatures if only those moved
            // (e.g. a rebuilt stage0 whose refs did not change).
            if c.files != files {
                let _ = write_closure_cache(&cache_path, &files, &all_roots, &c.items);
            }
            return Ok(c.items.clone());
        }
    }

    eprintln!(
        "td-builder check: content-scanning the loop toolchain closure ({} roots; \
         cached in .td-build-cache/loop-closure.list for later runs)",
        all_roots.len()
    );
    let st = ensure_scanner(&mut scan_state, store_dir)?;
    let empty = std::collections::HashMap::new();
    let root_list: Vec<String> = all_roots.iter().cloned().collect();
    let seen = crate::scan_closure_hybrid(&mut st.0, &st.1, &empty, &root_list)?;
    let items: Vec<String> = seen.into_iter().collect();
    if let Err(e) = write_closure_cache(&cache_path, &files, &all_roots, &items) {
        eprintln!("td-builder check: WARNING: loop-closure cache not written ({e})");
    }
    Ok(items)
}

/// Build the store scanner once: the candidate index over the host store dir +
/// the on-disk map, exactly the `store-closure-scan` machinery.
fn ensure_scanner<'a>(
    state: &'a mut Option<(
        crate::scan::Scanner,
        std::collections::HashMap<String, String>,
    )>,
    store_dir: &str,
) -> Result<
    &'a mut (
        crate::scan::Scanner,
        std::collections::HashMap<String, String>,
    ),
    String,
> {
    if state.is_none() {
        let (candidates, on_disk) =
            crate::scan_candidate_index(&[store_dir.to_string()], store_dir)?;
        let scanner = crate::scan::Scanner::new(&candidates).map_err(|e| e.to_string())?;
        *state = Some((scanner, on_disk));
    }
    state
        .as_mut()
        .ok_or_else(|| fatal("internal: loop-closure scanner missing after init"))
}

/// `(mtime in ns since epoch, byte length)` — the change signature of a scanned
/// host-built binary. An unreadable mtime degrades to 0 (never matches, so the
/// file is just re-scanned — safe, only slower).
fn file_sig(path: &str) -> Result<(u64, u64), String> {
    let md = std::fs::metadata(path).map_err(|e| format!("{path}: {e}"))?;
    let mtime = md
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(0));
    Ok((mtime, md.len()))
}

/// Read the loop-closure cache (tab-separated `file`/`fref`/`root`/`item`
/// lines; `#` comments). Any malformed line invalidates the WHOLE cache
/// (None → rescan) — the cache is an accelerator, never authority.
fn read_closure_cache(path: &Path) -> Option<ClosureCache> {
    let content = std::fs::read_to_string(path).ok()?;
    let mut files: Vec<(String, u64, u64, Vec<String>)> = Vec::new();
    let mut roots = std::collections::BTreeSet::new();
    let mut items: Vec<String> = Vec::new();
    for line in content.lines() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split('\t');
        match parts.next()? {
            "file" => {
                let mtime = parts.next()?.parse::<u64>().ok()?;
                let len = parts.next()?.parse::<u64>().ok()?;
                files.push((parts.next()?.to_string(), mtime, len, Vec::new()));
            }
            "fref" => files.last_mut()?.3.push(parts.next()?.to_string()),
            "root" => {
                roots.insert(parts.next()?.to_string());
            }
            "item" => items.push(parts.next()?.to_string()),
            _ => return None,
        }
    }
    Some(ClosureCache {
        files,
        roots,
        items,
    })
}

fn write_closure_cache(
    path: &Path,
    files: &[(String, u64, u64, Vec<String>)],
    roots: &std::collections::BTreeSet<String>,
    items: &[String],
) -> Result<(), String> {
    let mut out = String::new();
    out.push_str("# td loop-closure cache — regenerated by `td-builder check`; do not edit.\n");
    for (p, mtime, len, refs) in files {
        out.push_str(&format!("file\t{mtime}\t{len}\t{p}\n"));
        for r in refs {
            out.push_str(&format!("fref\t{r}\n"));
        }
    }
    for r in roots {
        out.push_str(&format!("root\t{r}\n"));
    }
    for i in items {
        out.push_str(&format!("item\t{i}\n"));
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    std::fs::write(path, out).map_err(|e| format!("write {}: {e}", path.display()))
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
fn host_cargo_bin(root: &Path, dir: &str, bin: &str, deadline: Option<Instant>) -> Option<PathBuf> {
    find_in_path("cargo")?;
    let mut child = Command::new("cargo")
        .args(["build", "--release", "--quiet"])
        .current_dir(root.join(dir))
        .spawn()
        .ok()?;
    if !wait_with_deadline(&mut child, deadline) {
        return None;
    }
    let p = root.join(dir).join("target/release").join(bin);
    p.is_file().then_some(p)
}

/// One `[[package]]` entry of a Cargo.lock that carries a checksum — `(name,
/// version, sha256)`. The checksummed entries are the vendored crates-io deps;
/// the root (path) crate has no checksum and is excluded, exactly the
/// reduction the retired shell awk did.
fn parse_lock_checksums(lock: &str) -> Vec<(String, String, String)> {
    fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
        line.strip_prefix(key)?
            .strip_prefix(" = \"")?
            .split('"')
            .next()
            .filter(|v| !v.is_empty())
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

/// td-fetch's OWN crate closure (native since this port — was
/// tools/warm-td-fetch-crates.sh, the prelude's last `sh tools/…` spawn;
/// #318 axis 2): host-side NETWORK PREP that GETs each `.crate` of
/// fetch/Cargo.lock GUIX-FREE with td's OWN fetcher (td-fetch), pinned by the
/// UPSTREAM lock checksum (NOT a guix artifact), into the flat vendor dir the
/// td-fetch recipe check interns and builds td-fetch from (TD_VENDOR_DIR).
/// td-fetch does every GET — td dogfoods its own fetcher, and td-fetch honors
/// TD_FEED_BASE so the reads route through the shared feed when it is up.
/// Best-effort like every warm (no td-fetch binary / no network → warn and
/// return; the gate reports if it actually runs cold), and the whole warm —
/// cargo fallback included — shares ONE warm_timeout_secs budget exactly as
/// the shell's single `timeout` over the script did: one hung mirror must
/// not stall the prelude.
fn warm_td_fetch_crates(root: &Path) {
    let lock_path = root.join("fetch/Cargo.lock");
    let Ok(lock) = std::fs::read_to_string(&lock_path) else {
        eprintln!(
            "td-builder check: warm td-fetch crates: no {} — skipping",
            lock_path.display()
        );
        return;
    };
    let dest = root.join(".td-build-cache/crate-vendor/td-fetch");
    if std::fs::create_dir_all(&dest).is_err() {
        eprintln!(
            "td-builder check: warm td-fetch crates: cannot create {} — skipping",
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
            "td-builder check: warm td-fetch crates: no td-fetch binary — skipping (PREP best-effort)"
        );
        return;
    };
    let mut complete = true;
    for (name, ver, sum) in parse_lock_checksums(&lock) {
        let nv = format!("{name}-{ver}");
        let out = dest.join(format!("{nv}.crate"));
        if crate::sha256::sha256_file(&out).ok().as_deref() == Some(sum.as_str()) {
            continue; // already warm + verified
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            eprintln!("td-builder check: warm td-fetch crates: TD_WARM_TIMEOUT budget exhausted — stopping");
            complete = false;
            break;
        }
        let url = format!("https://static.crates.io/crates/{name}/{nv}.crate");
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
            eprintln!("td-builder check: warm td-fetch crates: could not td-fetch/verify {nv}");
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
        "td-builder check: warm td-fetch crates: {n} crates in {} (td-fetched, Cargo.lock-pinned, guix-free){}",
        dest.display(),
        if complete { "" } else { " — INCOMPLETE (TD_WARM_TIMEOUT exhausted)" }
    );
}

/// The heavy-tier warm prelude: source-bootstrap tarballs + rust crate closures
/// (td-feed), all BEST-EFFORT (the gates enforce presence), fanned out in
/// batches of TD_WARM_JOBS exactly as the shell prelude did.
fn heavy_warms(root: &Path) {
    // td-fetch's own crate closure (its own warm — not the cargo-proxy).
    warm_td_fetch_crates(root);

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
/// start-time seed store DIR, default: the loop's host store
/// (SANDBOX_STORE_PREFIX) — content-scanned host-side for the
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
    // Default seed store = the loop's host store, read HOST-SIDE by the daemon
    // (the ONE remaining host-store coupling, deleted with SANDBOX_STORE_PREFIX
    // when the loop's userland goes td-built). The loop SANDBOX never mounts it.
    let seed_dir = std::env::var("TD_DAEMON_SEED_DIR")
        .unwrap_or_else(|_| s(SANDBOX_STORE_PREFIX.trim_end_matches('/')));
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

    // No guix process remains: the loop PATH is only the host-PATH toolchain
    // declared in LOOP_TOOLCHAIN. Gate text/tree work must invoke
    // td-builder typed helpers or td-built userland instead of inheriting GNU
    // sed/grep/findutils from a seed lock.

    // Light tiers own no heavy gate — skip the heavy warms + daemon (exactly the
    // shell prelude's goal scan).
    let heavy_warm = goals.iter().any(|g| {
        !matches!(
            g.as_str(),
            "check-fast" | "check-engine" | "list-gates" | "gate-timing-report"
        )
    });

    let tb = provision_stage0(&root)?;
    let tc = provision_toolchain()?;
    let toolchain = loop_path_with_native_applets(&root, &tb, &tc.path).map_err(|e| {
        CheckError::Fatal(fatal(&format!(
            "could not provision loop native applets ({e})"
        )))
    })?;

    let mut child_envs: Vec<(String, String)> = vec![(s("PATH"), toolchain)];
    // The runner's knobs must cross the sandbox boundary (host-sandbox
    // preserves the TD_CHECK_ prefix): without this, TD_CHECK_SLOTS=… ./check.sh
    // would be silently dead and gate-run would always default to nproc.
    // TD_CHECK_CHAIN_CACHE rides along for the same reason: `TD_CHECK_CHAIN_CACHE= ./check.sh`
    // (set-and-empty) is the operator's force-cold switch for the #317 warm
    // chain-brick default — the daily backstop uses it to stay authoritative.
    // TD_CHECK_DISABLE forwards the gate-disable list (gate names / `pool:<name>`
    // tokens) so `TD_CHECK_DISABLE=… td-builder check` reaches the in-sandbox runner.
    for k in [
        "TD_CHECK_SLOTS",
        "TD_CHECK_SLOTS_DIR",
        "TD_CHECK_JOBS",
        "TD_CHECK_CHAIN_CACHE",
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
    if let Some(nice) = find_in_path("nice") {
        argv.extend([nice.display().to_string(), s("-n"), tdnice]);
        if let Some(ionice) = find_in_path("ionice") {
            argv.extend([ionice.display().to_string(), s("-c2"), s("-n7")]);
        }
    }
    // The sandbox mounts NO store directory — only the loop's declared input
    // ITEMS, each bound read-only at its own path (the drv build jail's
    // input-only model): the resolved toolchain closure, the seed lock's
    // rust/cc closure, and the closures of the host-built binaries that run
    // inside (the stage0 td-builder; the stashed td-subst when exposed).
    let mut roots = tc.roots.clone();
    roots.extend(seed_lock_roots(&root));
    let mut scan_files = vec![tb.clone()];
    if let Some((_, bin)) = child_envs.iter().find(|(k, _)| k == "TD_SUBST_BIN") {
        scan_files.push(bin.clone());
    }
    let items = loop_store_items(&root, &roots, &scan_files).map_err(|e| {
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
/// The sandbox + toolchain provisioning is EXACTLY the loop prelude's (same
/// stage0 container provider, same `LOOP_TOOLCHAIN` list — notably WITHOUT
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
    if !root.join("tests").is_dir() {
        return Err(fatal("run from the repo root (tests/ not found)"));
    }
    let tb = provision_stage0(&root).map_err(|e| {
        format!("check-rung: FATAL: could not provision the guix-free stage0 td-builder for the sandbox ({e})")
    })?;
    // check-rung is a dev helper, not the loop: it does not branch on the
    // provisioned/regression distinction, so collapse CheckError back to a string.
    let tc = provision_toolchain().map_err(|e| e.to_string())?;
    let toolchain = loop_path_with_native_applets(&root, &tb, &tc.path)
        .map_err(|e| format!("check-rung: FATAL: could not provision loop native applets ({e})"))?;
    // The same input-only store exposure as the loop (per-item binds, no store
    // directory mounted): toolchain closure + seed lock closure + the stage0
    // td-builder's own closure (harnesses run it via TD_BUILDER_SELF).
    let mut roots = tc.roots.clone();
    roots.extend(seed_lock_roots(&root));
    let items = loop_store_items(&root, &roots, std::slice::from_ref(&tb))
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
    fn store_item_of_takes_the_package_not_the_bin_dir() {
        // The closure root is the whole store ITEM (its lib/, libexec/, share/
        // are runtime surface), never just the resolved bin dir.
        assert_eq!(
            store_item_of("/gnu/store/abc123-bash-5.2.37/bin").as_deref(),
            Some("/gnu/store/abc123-bash-5.2.37")
        );
        assert_eq!(
            store_item_of("/gnu/store/abc123-make-4.4").as_deref(),
            Some("/gnu/store/abc123-make-4.4")
        );
        assert_eq!(store_item_of("/usr/bin"), None, "off-store dir has no item");
        assert_eq!(store_item_of("/gnu/store/"), None, "the bare prefix is not an item");
    }

    #[test]
    fn parse_seed_lock_keeps_in_store_paths_deduped() {
        let lock = "\
# comment line
aaa-rust-1.93.0 /gnu/store/aaa-rust-1.93.0
bbb-cargo /gnu/store/bbb-rust-1.93.0-cargo extra-field
bbb-again /gnu/store/bbb-rust-1.93.0-cargo

malformed-line-without-path
ccc-off-store /opt/rust-1.93.0
";
        assert_eq!(
            parse_seed_lock(lock),
            vec![
                "/gnu/store/aaa-rust-1.93.0".to_string(),
                "/gnu/store/bbb-rust-1.93.0-cargo".to_string(),
            ]
        );
    }

    #[test]
    fn closure_cache_round_trips_and_rejects_garbage() {
        let d = scratch("loop-closure");
        let path = d.join("loop-closure.list");
        let files = vec![(
            "/repo/.td-build-cache/stage0/td-builder".to_string(),
            123u64,
            456u64,
            vec!["/gnu/store/ggg-glibc-2.39".to_string()],
        )];
        let roots: std::collections::BTreeSet<String> = [
            "/gnu/store/aaa-bash-5.2.37".to_string(),
            "/gnu/store/ggg-glibc-2.39".to_string(),
        ]
        .into_iter()
        .collect();
        let items = vec![
            "/gnu/store/aaa-bash-5.2.37".to_string(),
            "/gnu/store/ggg-glibc-2.39".to_string(),
        ];
        write_closure_cache(&path, &files, &roots, &items).unwrap();
        let c = read_closure_cache(&path).expect("cache parses back");
        assert_eq!(c.files, files);
        assert_eq!(c.roots, roots);
        assert_eq!(c.items, items);

        // Any malformed line invalidates the WHOLE cache — accelerator, never
        // authority.
        std::fs::write(&path, "item\t/gnu/store/x\nbogus-line\n").unwrap();
        assert!(read_closure_cache(&path).is_none());
        // A missing cache file is simply a miss.
        assert!(read_closure_cache(&d.join("absent.list")).is_none());
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
}

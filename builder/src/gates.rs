//! gates.rs — td's OWN gate runner: `td-builder gate-run`, the loop scheduler that
//! replaced `make` + the `Makefile` on the spine (`./check.sh` reaches it via
//! `td-builder check`, which execs it inside the loop sandbox).
//!
//! The Makefile used make for exactly four things: the gate-fragment registry, the
//! ordering graph (cheap serial-first, heavy after the last cheap gate, BUILD_GATES
//! after build-recipes), `-jN --output-sync=target`, and the `.SHELLFLAGS`
//! per-recipe timing hack. None of that needed make — every gate was `.PHONY`, so
//! make's actual value (file-dependency tracking) was never used — and make could
//! not give the loop the two scheduling properties it wants:
//!
//!   • MACHINE-WIDE concurrency: N agents' concurrent checks share ONE slot pool
//!     (exclusively-flocked slot files, default `~/.td/build-daemon/slots` — a
//!     path host-sandbox already binds into every check sandbox). flock dies with
//!     the holder, so a SIGKILLed gate can never leak a slot. Every run can
//!     therefore use `-j$(nproc)` without N runs multiplying to N×nproc: the pool
//!     (TD_CHECK_SLOTS, default 2×nproc — over-provisioned; memory admission +
//!     the per-gate rlimit backstop are the safety limits, #319) caps the box, `-j` only the local
//!     width. This replaces the retired AGENTS.md "two checks, -j2, stagger by
//!     hand" guidance — scheduling is the runner's job now, not the agents'.
//!   • DATA-DRIVEN order: ready heavy gates start longest-first from the previous
//!     run's wall-clock table (.td-build-cache/gate-timing/latest.txt), so LPT
//!     packing no longer lives in hand-renumbered <NNN> filename prefixes (the
//!     prefixes remain the stable registration/serial order and the tiebreak).
//!
//! Gates are STRUCTURED RUST, compiled in — no runtime parsing of any gate
//! format (human direction 2026-07-03). Each gate is one self-registering file
//! `src/gate_defs/<NNN>-<name>.rs` exporting `pub fn gate() -> GateDef` (the
//! same one-file-per-entry pattern as the recipe catalog, recipes/build.rs
//! #295); `build.rs` generates the stem-sorted registry this module includes.
//! The `<NNN>` filename prefix keeps the registration/serial order the retired
//! mk/gates/*.mk fragments carried, and the compiler enforces the structure a
//! parser used to check — a malformed gate is a build error, never a mis-run.
//!
//! A GateDef's `script` is PLAIN BASH (no make escaping), executed as one
//! `bash -c` with cwd = repo root and TD_GUIX exported (the pinned
//! `guix time-machine -C channels.scm --` prefix the remaining guix-surface
//! invocations go through). Output is buffered per gate (`--output-sync=target`
//! parity), first red stops new gates while running ones drain, and timing
//! events keep the exact per-gate START/END line format
//! tools/gate-timing-report.sh reads.

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Condvar, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// `$TD_GUIX` — the pinned time-machine prefix every remaining guix invocation in
/// a gate body goes through (exported to every gate).
pub const GUIX_CMD: &str = "guix time-machine -C channels.scm --";
/// The synthetic build-phase node (the former Makefile `build-recipes` target).
const BUILD_RECIPES: &str = "build-recipes";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Pool {
    Cheap,
    Heavy,
    Fast,
    System,
    Engine,
    Parked,
}

/// One gate, declared as compiled Rust data in `src/gate_defs/<NNN>-<name>.rs`.
/// The registry (`build.rs`) collects every file's `gate()` into `all()`.
pub struct GateDef {
    /// The goal name (`./check.sh <name>` runs it) — must equal the defining
    /// file's stem minus its `<NNN>-` prefix (checked by `load`).
    pub name: &'static str,
    /// Self-registration into the check tiers.
    pub pools: &'static [Pool],
    /// Explicit ordering prerequisites (gate names).
    pub needs: &'static [&'static str],
    /// Waits on the `build-recipes` phase (the former BUILD_GATES pool).
    pub build_gate: bool,
    /// Package recipes this gate asserts on — contributed to the build phase
    /// (the former BUILD_SPECS pool).
    pub specs: &'static [&'static str],
    /// The gate body: plain bash, run as one `bash -c` from the repo root.
    pub script: &'static str,
}

mod registry {
    include!(concat!(env!("OUT_DIR"), "/gate_registry.rs"));
}

#[derive(Clone, Debug)]
struct Gate {
    name: String,
    pools: Vec<Pool>,
    /// The plain-bash body (everything after `run:`), executed as one `bash -c`.
    body: String,
    /// Ordering prerequisites (gate names). All gates are phony, so make's old
    /// normal-vs-order-only (`|`) distinction collapses to "runs before".
    deps: Vec<String>,
    /// Extra env for the body (the synthetic build-recipes node uses this).
    extra_env: Vec<(String, String)>,
    /// The def's own spec list, exported to the body as TD_GATE_SPECS — the
    /// single source both the build phase and the gate's assertion loop read.
    specs: Vec<String>,
}

struct GateSet {
    /// Registration order = sorted src/gate_defs/*.rs stem order (the <NNN> prefix).
    gates: Vec<Gate>,
    index: HashMap<String, usize>,
    build_specs: Vec<String>,
}

impl GateSet {
    fn members(&self, p: Pool) -> Vec<usize> {
        self.gates
            .iter()
            .enumerate()
            .filter(|(_, g)| g.pools.contains(&p))
            .map(|(i, _)| i)
            .collect()
    }
    fn names(&self, p: Pool) -> Vec<String> {
        self.members(p)
            .iter()
            .filter_map(|i| self.gates.get(*i).map(|g| g.name.clone()))
            .collect()
    }
}

/// A word that may name a gate or build spec.
fn valid_word(w: &str) -> bool {
    !w.is_empty()
        && w.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '+' | '-'))
}

/// The registered gate definitions, stem-sorted (the `<NNN>-` prefixes) — the
/// compiled equivalent of globbing the old fragment directory. Exposed
/// crate-wide so affected-checks reads the SAME registry instead of parsing.
pub(crate) fn defs() -> Vec<(&'static str, GateDef)> {
    registry::all()
}

/// Build the runtime gate set from the compiled registry. The structure is
/// compiler-enforced; what remains checked here is the cross-gate consistency a
/// single file cannot see (name↔stem, duplicates, dep resolution).
fn load() -> Result<GateSet, String> {
    let mut gates: Vec<Gate> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    let mut build_specs: Vec<String> = Vec::new();
    let mut build_gates: Vec<String> = Vec::new();

    for (stem, def) in defs() {
        // The stem is `<NNN>-<gate-name>`; the def must carry the same name, so
        // a file rename can never silently re-key a gate.
        let expected = stem.get(4..).unwrap_or("");
        if def.name != expected {
            return Err(format!(
                "gate-run: src/gate_defs/{stem}.rs declares gate `{}` — the name must \
                 equal the file stem minus its <NNN>- prefix (`{expected}`)",
                def.name
            ));
        }
        if !valid_word(def.name) {
            return Err(format!("gate-run: invalid gate name `{}`", def.name));
        }
        if def.pools.is_empty() {
            return Err(format!("gate-run: gate `{}` is in no pool", def.name));
        }
        if def.script.trim().is_empty() {
            return Err(format!("gate-run: gate `{}` has an empty script", def.name));
        }
        for w in def.needs.iter().chain(def.specs) {
            if !valid_word(w) {
                return Err(format!("gate-run: gate `{}`: invalid word `{w}`", def.name));
            }
        }
        if index.contains_key(def.name) {
            return Err(format!("gate-run: duplicate gate `{}`", def.name));
        }
        if def.build_gate {
            build_gates.push(def.name.to_string());
        }
        build_specs.extend(def.specs.iter().map(|s| s.to_string()));
        index.insert(def.name.to_string(), gates.len());
        gates.push(Gate {
            name: def.name.to_string(),
            pools: def.pools.to_vec(),
            body: def.script.to_string(),
            deps: def.needs.iter().map(|d| d.to_string()).collect(),
            extra_env: Vec::new(),
            specs: def.specs.iter().map(|s| s.to_string()).collect(),
        });
    }

    let mut set = GateSet { gates, index, build_specs };
    derive_graph(&mut set, &build_gates)?;
    Ok(set)
}

/// The ordering graph (the former Makefile's generated graph): chain the cheap
/// gates serially, gate heavy/system/engine pools on the last cheap gate, add the
/// synthetic build-recipes node after the cheap chain, and make every BUILD_GATE
/// wait on it.
fn derive_graph(set: &mut GateSet, build_gates: &[String]) -> Result<(), String> {
    let cheap = set.members(Pool::Cheap);
    let cheap_names: Vec<String> = cheap
        .iter()
        .filter_map(|i| set.gates.get(*i).map(|g| g.name.clone()))
        .collect();
    for pair in cheap_names.windows(2) {
        if let (Some(prev), Some(cur)) = (pair.first(), pair.get(1)) {
            if let Some(gi) = set.index.get(cur).copied() {
                if let Some(g) = set.gates.get_mut(gi) {
                    g.deps.push(prev.clone());
                }
            }
        }
    }
    let last_cheap = cheap_names.last().cloned();

    if set.index.contains_key(BUILD_RECIPES) {
        return Err("gate-run: a fragment defines `build-recipes` — that name is the runner's build-phase node".to_string());
    }
    for spec in &set.build_specs {
        if !valid_word(spec) {
            return Err(format!("gate-run: invalid specs entry `{spec}`"));
        }
    }
    let br = Gate {
        name: BUILD_RECIPES.to_string(),
        pools: Vec::new(),
        body: "bash tests/build-recipes.sh".to_string(),
        deps: last_cheap.iter().cloned().collect(),
        extra_env: vec![("TD_BUILD_SPECS".to_string(), set.build_specs.join(" "))],
        specs: Vec::new(),
    };
    set.index.insert(BUILD_RECIPES.to_string(), set.gates.len());
    set.gates.push(br);

    if let Some(lc) = &last_cheap {
        for p in [Pool::Heavy, Pool::System, Pool::Engine] {
            for gi in set.members(p) {
                if let Some(g) = set.gates.get_mut(gi) {
                    if g.name != *lc && !g.deps.contains(lc) {
                        g.deps.push(lc.clone());
                    }
                }
            }
        }
    }
    for name in build_gates {
        if let Some(gi) = set.index.get(name).copied() {
            if let Some(g) = set.gates.get_mut(gi) {
                if !g.deps.contains(&BUILD_RECIPES.to_string()) {
                    g.deps.push(BUILD_RECIPES.to_string());
                }
            }
        }
    }
    // Every dep must resolve — an unknown dep would deadlock the scheduler.
    let known: HashSet<String> = set.index.keys().cloned().collect();
    for g in &set.gates {
        for d in &g.deps {
            if !known.contains(d) {
                return Err(format!("gate-run: gate `{}` depends on unknown `{d}`", g.name));
            }
        }
    }
    Ok(())
}

/// Expand the requested goals into the set of node indices to run (make
/// semantics kept: prerequisites always run, so take the transitive dep closure).
fn expand_goals(set: &GateSet, goals: &[String]) -> Result<HashSet<usize>, String> {
    let mut sel: HashSet<usize> = HashSet::new();
    let add_pool = |sel: &mut HashSet<usize>, p: Pool| sel.extend(set.members(p));
    for goal in goals {
        match goal.as_str() {
            "check" => {
                add_pool(&mut sel, Pool::Cheap);
                if let Some(i) = set.index.get(BUILD_RECIPES) {
                    sel.insert(*i);
                }
                add_pool(&mut sel, Pool::Heavy);
            }
            "check-fast" => {
                add_pool(&mut sel, Pool::Cheap);
                add_pool(&mut sel, Pool::Fast);
            }
            "check-system" => {
                add_pool(&mut sel, Pool::Cheap);
                add_pool(&mut sel, Pool::System);
            }
            "check-engine" => {
                add_pool(&mut sel, Pool::Cheap);
                add_pool(&mut sel, Pool::Engine);
            }
            name => match set.index.get(name) {
                Some(i) => {
                    sel.insert(*i);
                }
                None => {
                    return Err(format!(
                        "gate-run: unknown goal `{name}` — a tier \
                         (check/check-fast/check-system/check-engine), a gate name \
                         (`td-builder gate-run list-gates`), or build-recipes"
                    ))
                }
            },
        }
    }
    // Transitive closure over deps.
    loop {
        let mut grew = false;
        let cur: Vec<usize> = sel.iter().copied().collect();
        for i in cur {
            let Some(g) = set.gates.get(i) else { continue };
            for d in &g.deps {
                if let Some(di) = set.index.get(d) {
                    if sel.insert(*di) {
                        grew = true;
                    }
                }
            }
        }
        if !grew {
            return Ok(sel);
        }
    }
}

/// Per-gate wall-clock history (seconds) from the last timing report — the
/// data-driven LPT order. Missing/unparseable => empty (fallback: <NNN> order).
fn duration_table(root: &Path) -> HashMap<String, f64> {
    let mut out = HashMap::new();
    let path = root.join(".td-build-cache/gate-timing/latest.txt");
    let Ok(text) = std::fs::read_to_string(path) else {
        return out;
    };
    for line in text.lines() {
        let mut it = line.split_whitespace();
        let (Some(name), Some(_kind), Some(secs)) = (it.next(), it.next(), it.next()) else {
            continue;
        };
        if name.starts_with('#') || name == "GATE" {
            continue;
        }
        if let Ok(v) = secs.parse::<f64>() {
            out.insert(name.to_string(), v);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// The machine-wide slot pool.

/// The cross-agent concurrency cap: N slot files, each held by an exclusive
/// flock for the duration of one running gate. Every concurrent `gate-run` (any
/// worktree, any agent) contends on the same files, so the box-wide running-gate
/// count never exceeds the pool size no matter how many checks run at once.
struct SlotPool {
    dir: Option<PathBuf>,
    n: usize,
    /// Memory-admission reserve (GiB): a free slot is only taken while
    /// MemAvailable stays above this, EXCEPT when no other slot is held (the
    /// daemon's `admit` no-deadlock rule, mirrored — if nothing else runs, the
    /// pressure isn't ours and blocking the whole loop forever is worse; the
    /// per-gate rlimit backstop contains the runaway). `<= 0` disables.
    min_free_gib: f64,
}

enum Grant {
    /// No pool configured — the local `-j` width is the only cap.
    NoPool,
    /// A held slot; dropping the file releases the flock.
    Held(std::fs::File),
    /// The run failed while waiting — do not start the gate.
    Aborted,
}

impl SlotPool {
    fn acquire(&self, aborted: &dyn Fn() -> bool) -> Grant {
        let Some(dir) = &self.dir else { return Grant::NoPool };
        // Loop-invariant: compute the slot paths once, not per 200ms poll.
        let paths: Vec<PathBuf> = (0..self.n).map(|i| dir.join(format!("slot-{i}"))).collect();
        loop {
            // One sweep: take the first free slot, and COUNT the held ones —
            // the memory admission below needs to know whether anything else
            // is running box-wide.
            let mut opened_any = false;
            let mut held = 0usize;
            let mut got: Option<std::fs::File> = None;
            for p in &paths {
                let Ok(f) = std::fs::OpenOptions::new().create(true).append(true).open(p)
                else {
                    continue;
                };
                opened_any = true;
                use std::os::fd::AsRawFd;
                match crate::sys::flock_try_exclusive(f.as_raw_fd()) {
                    Ok(true) if got.is_none() => got = Some(f),
                    Ok(true) => {} // free; dropping f releases the probe flock
                    Ok(false) => held += 1,
                    Err(_) => {}
                }
            }
            if !opened_any {
                // Every slot file is unopenable (permissions, ENOSPC, read-only
                // mount): spinning forever would hang the whole check silently.
                // Degrade to unpooled — same posture as slot_pool_from_env's
                // cannot-create fallback — and say so.
                eprintln!(
                    "gate-run: cannot open any slot file under {} — running WITHOUT the \
                     machine-wide slot pool (local -j is the only cap)",
                    dir.display()
                );
                return Grant::NoPool;
            }
            if let Some(f) = got {
                // Memory admission (the over-provisioned pool's OOM guard, issue
                // #319): with CPU slots > cores, free memory — not slot count —
                // is the binding safety limit. Defer the grant while
                // MemAvailable is below the reserve, unless nothing else holds a
                // slot (the daemon admit()'s no-deadlock rule).
                let mem_ok = held == 0
                    || self.min_free_gib <= 0.0
                    || crate::build_daemon::mem_available_gib()
                        .map(|g| g >= self.min_free_gib)
                        .unwrap_or(true);
                if mem_ok {
                    return Grant::Held(f);
                }
                drop(f); // give the slot back while memory is tight
            }
            if aborted() {
                return Grant::Aborted;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }
}

/// Is util-linux `prlimit` resolvable on PATH? (The loop toolchain provisions
/// util-linux, so inside the sandbox this is normally true.)
fn prlimit_available() -> bool {
    let Ok(path) = std::env::var("PATH") else { return false };
    path.split(':')
        .filter(|d| !d.is_empty())
        .any(|d| Path::new(d).join("prlimit").is_file())
}

pub(crate) fn nproc() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1)
}

/// Build the slot pool from the environment. TD_CHECK_SLOTS sizes it (default
/// 2×nproc — deliberately OVER-PROVISIONED, issue #319: most heavy gates are
/// single-threaded or daemon/IO-blocked for long stretches, so slot=gate at
/// nproc left cores idle; memory admission + the per-gate rlimit backstop are
/// the safety limits instead. 0 disables). TD_CHECK_SLOTS_DIR overrides the
/// shared directory (default ~/.td/build-daemon/slots — bound into every check
/// sandbox at the same absolute path, so concurrent sandboxed checks really do
/// contend). TD_MIN_FREE_GIB (default 4, the build daemon's knob) sets the
/// memory-admission reserve.
fn slot_pool_from_env() -> SlotPool {
    let n = match std::env::var("TD_CHECK_SLOTS") {
        Ok(v) => v.trim().parse::<usize>().unwrap_or_else(|_| 2 * nproc()),
        Err(_) => 2 * nproc(),
    };
    let min_free_gib = std::env::var("TD_MIN_FREE_GIB")
        .ok()
        .and_then(|v| v.trim().parse::<f64>().ok())
        .unwrap_or(4.0);
    if n == 0 {
        return SlotPool { dir: None, n: 0, min_free_gib };
    }
    let dir = match std::env::var("TD_CHECK_SLOTS_DIR") {
        Ok(d) if !d.is_empty() => PathBuf::from(d),
        _ => match std::env::var("HOME") {
            Ok(h) => Path::new(&h).join(".td/build-daemon/slots"),
            Err(_) => {
                eprintln!(
                    "gate-run: no HOME and no TD_CHECK_SLOTS_DIR — running WITHOUT the \
                     machine-wide slot pool (local -j is the only cap)"
                );
                return SlotPool { dir: None, n: 0, min_free_gib };
            }
        },
    };
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!(
            "gate-run: cannot create slot dir {}: {e} — running WITHOUT the machine-wide \
             slot pool (local -j is the only cap)",
            dir.display()
        );
        return SlotPool { dir: None, n: 0, min_free_gib };
    }
    SlotPool { dir: Some(dir), n, min_free_gib }
}

// ---------------------------------------------------------------------------
// Execution.

fn now_ns() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

/// Append one timing event (`<gate>\tSTART|END\t<ns>` — the format
/// tools/gate-timing-report.sh reduces); best-effort (a logging hiccup must
/// never change a gate's outcome).
fn timing_event(log: Option<&Path>, gate: &str, kind: &str) {
    let Some(log) = log else { return };
    if let Some(parent) = log.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(log) {
        let _ = writeln!(f, "{gate}\t{kind}\t{}", now_ns());
    }
}

/// Run one gate's body under `bash -c` (through `prlimit --data` when a
/// per-process memory cap is configured), stdout+stderr appended in order to
/// LOG_PATH (the per-gate output buffer). Returns success.
fn run_gate(g: &Gate, root: &Path, log_path: &Path, timing: Option<&Path>, mem_mib: u64) -> bool {
    let mut logf = match std::fs::File::create(log_path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("gate-run: cannot open log for gate {}: {e}", g.name);
            return false;
        }
    };
    timing_event(timing, &g.name, "START");
    let ok = (|| {
        let (out, err) = match (logf.try_clone(), logf.try_clone()) {
            (Ok(o), Ok(e)) => (o, e),
            _ => return false,
        };
        let mut cmd = if mem_mib > 0 {
            let mut c = std::process::Command::new("prlimit");
            c.arg(format!("--data={}", mem_mib.saturating_mul(1024 * 1024)))
                .arg("bash");
            c
        } else {
            std::process::Command::new("bash")
        };
        cmd.arg("-c")
            .arg(&g.body)
            .current_dir(root)
            .env("TD_GUIX", GUIX_CMD)
            .stdout(std::process::Stdio::from(out))
            .stderr(std::process::Stdio::from(err));
        if !g.specs.is_empty() {
            cmd.env("TD_GATE_SPECS", g.specs.join(" "));
        }
        for (k, v) in &g.extra_env {
            cmd.env(k, v);
        }
        match cmd.status() {
            Ok(st) if st.success() => true,
            Ok(st) => {
                let _ = writeln!(
                    logf,
                    "gate-run: FAIL: gate {} — body exited {}",
                    g.name,
                    st.code().unwrap_or(-1)
                );
                false
            }
            Err(e) => {
                let _ = writeln!(logf, "gate-run: FAIL: gate {}: cannot spawn bash: {e}", g.name);
                false
            }
        }
    })();
    timing_event(timing, &g.name, "END");
    ok
}

/// Dump one finished gate's buffered output atomically (--output-sync=target
/// parity), with a one-line PASS/FAIL trailer. Raw bytes, not String: build
/// logs routinely carry non-UTF-8 (compiler/tar output), and read_to_string
/// would silently drop the WHOLE log — the one thing a red gate must not lose.
fn print_gate_output(name: &str, log_path: &Path, ok: bool, secs: f64) {
    let body = std::fs::read(log_path).unwrap_or_default();
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    let _ = lock.write_all(&body);
    let verdict = if ok { "PASS" } else { "FAIL" };
    let _ = writeln!(lock, "[gate-run] {name}: {verdict} ({secs:.1}s)");
    let _ = lock.flush();
}

/// The verdict journal for one tree key: a line per gate that passed.
fn journal_path(root: &Path, key: &str) -> PathBuf {
    root.join(".td-build-cache/gate-verdicts").join(key)
}

fn journal_read(root: &Path, key: &str) -> HashSet<String> {
    std::fs::read_to_string(journal_path(root, key))
        .map(|t| t.lines().map(str::to_string).collect())
        .unwrap_or_default()
}

/// Append one PASS (best-effort — journaling must never affect a verdict).
fn journal_pass(root: &Path, key: &str, gate: &str) {
    let p = journal_path(root, key);
    if let Some(parent) = p.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&p) {
        let _ = writeln!(f, "{gate}");
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum St {
    Pending,
    Running,
    Done,
    Failed,
}

struct Sched {
    st: HashMap<usize, St>,
    fail: bool,
    running: usize,
}

struct RunCfg {
    root: PathBuf,
    jobs: usize,
    pool: SlotPool,
    /// The per-run timing event log (None = timing disabled, TD_GATE_TIMING=0).
    timing_log: Option<PathBuf>,
    /// Where per-gate output buffers live.
    log_dir: PathBuf,
    /// The working-tree content key (TD_CHECK_TREE, computed host-side by
    /// `td-builder check` from git HEAD + dirty diff + untracked contents).
    /// When present, every PASS is journaled under it; None disables journaling.
    tree_key: Option<String>,
    /// --resume: skip gates journaled green for THIS tree key (issue #320).
    /// Opt-in, interactive iteration only — CI and the daily never pass it.
    resume: bool,
    /// Per-PROCESS RLIMIT_DATA cap for gate bodies, in MiB (0 = off). Applied
    /// via util-linux `prlimit` from the provisioned toolchain: with the pool
    /// over-provisioned past nproc (#319), one runaway allocator must die by
    /// its own limit — a clean red gate — instead of triggering the box
    /// OOM-killer. Per-process, so a make -jN tree of modest compilers passes.
    gate_mem_mib: u64,
}

/// True when a node contends on the machine-wide pool: everything except the
/// sub-5s serial cheap gates, build-recipes, and the BUILD_GATES behind it —
/// those two classes submit to the shared build daemon, whose own global budget
/// (TD_BUILD_JOBS) is their real limiter; holding a box-wide slot while blocked
/// on the daemon would double-count the box and starve the CPU-heavy gates.
fn takes_slot(g: &Gate) -> bool {
    g.name != BUILD_RECIPES
        && !g.pools.contains(&Pool::Cheap)
        && !g.deps.iter().any(|d| d == BUILD_RECIPES)
}

fn lock_sched<'a>(m: &'a Mutex<Sched>) -> std::sync::MutexGuard<'a, Sched> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Run the selected nodes. Returns Ok(true) if everything passed.
fn run_selected(set: &GateSet, selected: &HashSet<usize>, cfg: &RunCfg) -> Result<bool, String> {
    if selected.is_empty() {
        return Err("gate-run: nothing selected".to_string());
    }
    std::fs::create_dir_all(&cfg.log_dir)
        .map_err(|e| format!("gate-run: cannot create {}: {e}", cfg.log_dir.display()))?;

    // Priority: build-recipes first (it unblocks every BUILD_GATE), then measured
    // duration descending (LPT), unknown-duration gates ahead of known ones (a new
    // gate is assumed long until measured). Ties: registration (<NNN>) order.
    let durations = duration_table(&cfg.root);
    let prio = |i: usize| -> f64 {
        let Some(g) = set.gates.get(i) else { return 0.0 };
        if g.name == BUILD_RECIPES {
            return f64::INFINITY;
        }
        match durations.get(&g.name) {
            Some(d) => *d,
            None => 1e18,
        }
    };

    let dep_idx: Vec<Vec<usize>> = set
        .gates
        .iter()
        .map(|g| g.deps.iter().filter_map(|d| set.index.get(d).copied()).collect())
        .collect();

    // --resume: gates journaled green for THIS tree key start as Done — loudly,
    // so a green-with-skips run is visually distinct from a full green run.
    let mut initial: HashMap<usize, St> = selected.iter().map(|i| (*i, St::Pending)).collect();
    if cfg.resume {
        if let Some(key) = &cfg.tree_key {
            let green = journal_read(&cfg.root, key);
            let mut skipped = 0usize;
            for (&i, st) in initial.iter_mut() {
                let Some(g) = set.gates.get(i) else { continue };
                if green.contains(&g.name) {
                    *st = St::Done;
                    println!("[gate-run] {}: SKIPPED(resume — green for this exact tree)", g.name);
                    skipped += 1;
                }
            }
            if skipped > 0 {
                println!(
                    "[gate-run] resume: {skipped} gate(s) skipped from the verdict journal                      (key {key}); any tree change invalidates the whole journal"
                );
            }
        }
    }
    let sched = Mutex::new(Sched { st: initial, fail: false, running: 0 });
    let cv = Condvar::new();

    let pick_ready = |s: &Sched| -> Option<usize> {
        let mut best: Option<(f64, usize)> = None;
        for (&i, &st) in &s.st {
            if st != St::Pending {
                continue;
            }
            let deps = dep_idx.get(i).map(Vec::as_slice).unwrap_or(&[]);
            let ready = deps
                .iter()
                .all(|d| !s.st.contains_key(d) || s.st.get(d) == Some(&St::Done));
            if !ready {
                continue;
            }
            let p = prio(i);
            let better = match best {
                None => true,
                // Higher priority wins; on a tie the LOWER registration index
                // (earlier <NNN>) wins — stable, deterministic order.
                Some((bp, bi)) => p > bp || (p == bp && i < bi),
            };
            if better {
                best = Some((p, i));
            }
        }
        best.map(|(_, i)| i)
    };

    let jobs = cfg.jobs.max(1);
    std::thread::scope(|scope| {
        for _ in 0..jobs {
            scope.spawn(|| loop {
                let gi = {
                    let mut s = lock_sched(&sched);
                    loop {
                        if s.fail {
                            return;
                        }
                        if let Some(i) = pick_ready(&s) {
                            s.st.insert(i, St::Running);
                            s.running += 1;
                            break i;
                        }
                        let pending = s.st.values().any(|st| *st == St::Pending);
                        if !pending {
                            return;
                        }
                        if s.running == 0 {
                            // Pending gates but nothing running and nothing ready:
                            // a dependency cycle. Fail loudly rather than hang.
                            eprintln!("gate-run: dependency cycle among pending gates");
                            s.fail = true;
                            cv.notify_all();
                            return;
                        }
                        s = cv
                            .wait(s)
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                    }
                };
                let Some(g) = set.gates.get(gi) else { return };
                let mut _slot_hold: Option<std::fs::File> = None;
                if takes_slot(g) {
                    match cfg.pool.acquire(&|| lock_sched(&sched).fail) {
                        Grant::Held(f) => _slot_hold = Some(f),
                        Grant::NoPool => {}
                        Grant::Aborted => {
                            let mut s = lock_sched(&sched);
                            s.st.insert(gi, St::Pending);
                            s.running -= 1;
                            cv.notify_all();
                            return;
                        }
                    }
                }
                let log_path = cfg.log_dir.join(format!("{}.log", g.name));
                let started = std::time::Instant::now();
                let ok =
                    run_gate(g, &cfg.root, &log_path, cfg.timing_log.as_deref(), cfg.gate_mem_mib);
                print_gate_output(&g.name, &log_path, ok, started.elapsed().as_secs_f64());
                if ok {
                    if let Some(key) = &cfg.tree_key {
                        journal_pass(&cfg.root, key, &g.name);
                    }
                }
                let mut s = lock_sched(&sched);
                s.st.insert(gi, if ok { St::Done } else { St::Failed });
                s.running -= 1;
                if !ok {
                    s.fail = true;
                }
                cv.notify_all();
            });
        }
    });

    let s = lock_sched(&sched);
    let all_done = s.st.values().all(|st| *st == St::Done);
    if !all_done {
        let failed: Vec<&str> = s
            .st
            .iter()
            .filter(|(_, st)| **st == St::Failed)
            .filter_map(|(i, _)| set.gates.get(*i).map(|g| g.name.as_str()))
            .collect();
        let skipped = s.st.values().filter(|st| **st == St::Pending).count();
        eprintln!(
            "gate-run: RED — failed: {}{}",
            if failed.is_empty() { "(none — internal error)".to_string() } else { failed.join(" ") },
            if skipped > 0 { format!(" ({skipped} gates not started)") } else { String::new() }
        );
    }
    Ok(all_done)
}

// ---------------------------------------------------------------------------
// CLI.

fn print_pools(set: &GateSet) {
    let line = |label: &str, p: Pool| {
        let names = set.names(p);
        println!("{label} ({}): {}", names.len(), names.join(" "));
    };
    line("cheap ", Pool::Cheap);
    line("heavy ", Pool::Heavy);
    line("fast  ", Pool::Fast);
    line("system", Pool::System);
    line("engine", Pool::Engine);
    line("parked", Pool::Parked);
}

/// Re-print the newest run's per-gate table (the former Makefile
/// gate-timing-report target). Best-effort, like the old `|| true` recipe.
fn run_timing_report(root: &Path, heavy_gates: &str) {
    let dir = root.join(".td-build-cache/gate-timing");
    let latest = dir.join("latest.txt");
    let mut cmd = std::process::Command::new("sh");
    cmd.arg("tools/gate-timing-report.sh")
        .arg(&dir)
        .arg(&latest)
        .env("TD_HEAVY_GATES", heavy_gates)
        .current_dir(root);
    let _ = cmd.status();
}

pub fn cli(args: &[String]) -> ExitCode {
    let mut jobs: usize = match std::env::var("TD_CHECK_JOBS") {
        Ok(v) => v.trim().parse().unwrap_or_else(|_| nproc()),
        Err(_) => nproc(),
    };
    let mut goals: Vec<String> = Vec::new();
    let mut resume = false;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "-j" || a == "--jobs" {
            let Some(v) = it.next() else {
                eprintln!("gate-run: {a} needs a value");
                return ExitCode::from(2);
            };
            match v.trim().parse::<usize>() {
                Ok(n) => jobs = n,
                Err(_) => {
                    eprintln!("gate-run: bad {a} value `{v}`");
                    return ExitCode::from(2);
                }
            }
        } else if let Some(n) = a.strip_prefix("-j") {
            match n.trim().parse::<usize>() {
                Ok(v) => jobs = v,
                Err(_) => {
                    eprintln!("gate-run: bad -j value `{n}`");
                    return ExitCode::from(2);
                }
            }
        } else if a == "--resume" {
            resume = true;
        } else if a == "--list" {
            goals.push("list-gates".to_string());
        } else {
            goals.push(a.clone());
        }
    }
    if goals.is_empty() {
        goals.push("check".to_string());
    }

    let root = match std::env::current_dir() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("gate-run: cannot resolve cwd: {e}");
            return ExitCode::FAILURE;
        }
    };
    let set = match load() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };

    // The two report-style goals are standalone (parity with the old Makefile
    // targets); they don't mix with gate goals.
    if goals.iter().any(|g| g == "list-gates") {
        if goals.len() > 1 {
            eprintln!("gate-run: list-gates does not combine with other goals");
            return ExitCode::from(2);
        }
        print_pools(&set);
        return ExitCode::SUCCESS;
    }
    if goals.iter().any(|g| g == "gate-timing-report") {
        if goals.len() > 1 {
            eprintln!("gate-run: gate-timing-report does not combine with other goals");
            return ExitCode::from(2);
        }
        run_timing_report(&root, &set.names(Pool::Heavy).join(" "));
        return ExitCode::SUCCESS;
    }

    let selected = match expand_goals(&set, &goals) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };

    let timing_log = if std::env::var("TD_GATE_TIMING").ok().as_deref() == Some("0") {
        None
    } else {
        Some(root.join(format!(".td-build-cache/gate-timing/run-{}.log", now_ns())))
    };
    // TD_CHECK_GATE_MEM_MIB: per-process gate memory cap (default 8192; 0 off).
    let mut gate_mem_mib: u64 = std::env::var("TD_CHECK_GATE_MEM_MIB")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(8192);
    if gate_mem_mib > 0 && !prlimit_available() {
        eprintln!(
            "gate-run: no `prlimit` on PATH — running WITHOUT the per-gate memory              backstop (TD_CHECK_GATE_MEM_MIB={gate_mem_mib} requested)"
        );
        gate_mem_mib = 0;
    }
    let tree_key = std::env::var("TD_CHECK_TREE").ok().filter(|k| !k.is_empty());
    if resume && tree_key.is_none() {
        eprintln!(
            "gate-run: --resume needs the TD_CHECK_TREE key (td-builder check computes it              from git); refusing to guess — running everything"
        );
        resume = false;
    }
    let cfg = RunCfg {
        root: root.clone(),
        jobs,
        pool: slot_pool_from_env(),
        timing_log,
        log_dir: std::env::temp_dir().join(format!("td-gate-run-{}", std::process::id())),
        tree_key,
        resume,
        gate_mem_mib,
    };
    match run_selected(&set, &selected, &cfg) {
        Ok(true) => {
            // Parity with the old check/check-system targets: print the per-gate
            // timing table on a green full run (best-effort).
            if goals.iter().any(|g| g == "check") {
                run_timing_report(&root, &set.names(Pool::Heavy).join(" "));
            } else if goals.iter().any(|g| g == "check-system") {
                run_timing_report(&root, &set.names(Pool::System).join(" "));
            }
            ExitCode::SUCCESS
        }
        Ok(false) => ExitCode::from(2),
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_loads_and_holds_the_gate_ladder() {
        // The registry is compiled in, so this runs EVERYWHERE cargo test runs —
        // including the guix td-builder package build (unlike the old
        // repo-tree-reading parser tests, which had to skip there).
        let set = load().unwrap();
        // The pools the Makefile assembled on the day of the cutover (the counts
        // only grow as gates are added; membership spot-checks are structural).
        let cheap = set.names(Pool::Cheap);
        // Membership + relative order, NOT exact vectors: adding a gate must
        // never require touching this file (the one-file-per-gate property).
        let pos = |n: &str| cheap.iter().position(|x| x == n);
        let (e, gd, gs) = (pos("eval"), pos("guix-dependence"), pos("guix-surface"));
        assert!(e.is_some() && gd.is_some() && gs.is_some(), "cheap chain lost a member");
        assert!(e < gd && gd < gs, "cheap chain order changed");
        let heavy = set.names(Pool::Heavy);
        assert!(heavy.len() >= 99, "heavy pool shrank: {}", heavy.len());
        for g in ["bootstrap", "td-subst", "cargo-test", "rust-userland-x86_64-store-native"] {
            assert!(heavy.iter().any(|n| n == g), "missing heavy gate {g}");
        }
        assert!(set.names(Pool::Engine).iter().any(|n| n == "cargo-test"));
        let system = set.names(Pool::System);
        for g in ["oci-native", "rust-userland-image"] {
            assert!(system.iter().any(|n| n == g), "missing system gate {g}");
        }
        // Fragment-declared specs feed the synthetic build-recipes node.
        for s in ["hello", "bash", "pcre2"] {
            assert!(set.build_specs.iter().any(|x| x == s), "missing build spec {s}");
        }
        // The explicit fragment dep survived; the derived graph holds.
        let fs = set.gates.iter().find(|g| g.name == "feed-shared").unwrap();
        assert!(fs.deps.iter().any(|d| d == "td-feed"));
        let gs = set.gates.iter().find(|g| g.name == "guix-surface").unwrap();
        assert!(gs.deps.iter().any(|d| d == "guix-dependence"));
        let ts = set.gates.iter().find(|g| g.name == "td-subst").unwrap();
        assert!(ts.deps.iter().any(|d| d == BUILD_RECIPES));
        assert!(ts.deps.iter().any(|d| d == "guix-surface"));
        let br = set.gates.iter().find(|g| g.name == BUILD_RECIPES).unwrap();
        assert!(br.extra_env.iter().any(|(k, _)| k == "TD_BUILD_SPECS"));
        // Every body is non-empty plain bash (no make-isms survived conversion).
        for g in &set.gates {
            assert!(!g.body.trim().is_empty(), "{} has an empty body", g.name);
            assert!(!g.body.contains("$(CURDIR)"), "{} kept a make var", g.name);
            assert!(!g.body.contains("$$"), "{} kept make $$ escaping", g.name);
        }
    }

    /// A tiny synthetic gate set exercising the REAL scheduler + bash execution
    /// path (not a mock): cheap gates run strictly serially, a failure
    /// fail-fasts (later gates never start), and a BUILD_GATE waits for
    /// build-recipes.
    fn synth(dir: &Path, lines: &[(&str, Pool, &str, &[&str])]) -> GateSet {
        let mut gates = Vec::new();
        let mut index = HashMap::new();
        for (name, pool, cmd, deps) in lines {
            index.insert(name.to_string(), gates.len());
            gates.push(Gate {
                name: name.to_string(),
                pools: vec![*pool],
                body: cmd.replace("{D}", &dir.display().to_string()),
                deps: deps.iter().map(|d| d.to_string()).collect(),
                extra_env: Vec::new(),
                specs: Vec::new(),
            });
        }
        GateSet { gates, index, build_specs: Vec::new() }
    }

    fn cfg(dir: &Path, jobs: usize, slots: Option<(PathBuf, usize)>) -> RunCfg {
        let (sdir, n) = match slots {
            Some((d, n)) => (Some(d), n),
            None => (None, 0),
        };
        RunCfg {
            root: dir.to_path_buf(),
            jobs,
            pool: SlotPool { dir: sdir, n, min_free_gib: 0.0 },
            timing_log: None,
            log_dir: dir.join("logs"),
            tree_key: None,
            resume: false,
            gate_mem_mib: 0,
        }
    }

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("td-gates-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn cheap_gates_run_serially_and_in_order() {
        let d = tmpdir("serial");
        let set = synth(
            &d,
            &[
                ("a", Pool::Cheap, "test ! -e {D}/b.ran && touch {D}/a.ran", &[]),
                ("b", Pool::Cheap, "test -e {D}/a.ran && touch {D}/b.ran", &["a"]),
            ],
        );
        let sel = expand_goals(&set, &["check-fast".to_string()]).unwrap();
        assert!(run_selected(&set, &sel, &cfg(&d, 4, None)).unwrap());
        assert!(d.join("a.ran").exists() && d.join("b.ran").exists());
    }

    #[test]
    fn a_red_gate_fail_fasts_and_exits_nonzero() {
        let d = tmpdir("red");
        let set = synth(
            &d,
            &[
                ("a", Pool::Cheap, "exit 3", &[]),
                ("late", Pool::Heavy, "touch {D}/late.ran", &["a"]),
            ],
        );
        let sel = expand_goals(&set, &["check".to_string()]).unwrap();
        assert!(!run_selected(&set, &sel, &cfg(&d, 4, None)).unwrap());
        assert!(!d.join("late.ran").exists(), "gate behind a red gate must not start");
    }

    #[test]
    fn memory_admission_defers_while_another_slot_is_held() {
        use std::os::fd::AsRawFd;
        let d = tmpdir("mem");
        let slots = d.join("slots");
        std::fs::create_dir_all(&slots).unwrap();
        // Simulate "someone else is running": hold slot-0 ourselves.
        let holder = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(slots.join("slot-0"))
            .unwrap();
        assert!(crate::sys::flock_try_exclusive(holder.as_raw_fd()).unwrap());
        // An impossibly-high reserve: with a held slot present, the free slot
        // must NOT be granted (deferred), so the aborted() escape is taken.
        let pool = SlotPool { dir: Some(slots.clone()), n: 2, min_free_gib: 1e9 };
        assert!(matches!(pool.acquire(&|| true), Grant::Aborted));
        // Same reserve, but nothing else held: the no-deadlock rule admits.
        drop(holder);
        assert!(matches!(pool.acquire(&|| true), Grant::Held(_)));
        // Reserve disabled: always admits.
        let pool = SlotPool { dir: Some(slots), n: 2, min_free_gib: 0.0 };
        assert!(matches!(pool.acquire(&|| true), Grant::Held(_)));
    }

    #[test]
    fn gate_mem_backstop_contains_a_runaway_allocator() {
        if !prlimit_available() {
            return; // dev host without util-linux prlimit; the sandbox has it
        }
        let d = tmpdir("rlimit");
        // ~64 MiB heap allocation in bash (command substitution buffers it).
        let hog = r#"x=$(head -c 67108864 /dev/zero | tr '\0' a); echo grew ${#x}"#;
        let set = synth(&d, &[("hog", Pool::Heavy, hog, &[])]);
        let sel = expand_goals(&set, &["hog".to_string()]).unwrap();
        // VERIFIED-RED half: capped at 16 MiB per process, the allocator dies
        // and the gate reds cleanly (no box OOM).
        let mut c = cfg(&d, 2, None);
        c.gate_mem_mib = 16;
        assert!(!run_selected(&set, &sel, &c).unwrap(), "16MiB cap must red the hog");
        // Green half: with the cap off the same body passes.
        let c = cfg(&d, 2, None);
        assert!(run_selected(&set, &sel, &c).unwrap(), "uncapped hog must pass");
    }

    #[test]
    fn slot_pool_bounds_cross_gate_concurrency() {
        let d = tmpdir("slots");
        // Two heavy gates with no ordering between them; a 1-slot pool must
        // serialize them. Each gate asserts the other is not mid-flight.
        let probe = "test ! -e {D}/busy && touch {D}/busy && sleep 0.3 && rm {D}/busy";
        let set = synth(
            &d,
            &[("h1", Pool::Heavy, probe, &[]), ("h2", Pool::Heavy, probe, &[])],
        );
        let sel = expand_goals(&set, &["check".to_string()]).unwrap();
        let slots = d.join("slots");
        std::fs::create_dir_all(&slots).unwrap();
        assert!(run_selected(&set, &sel, &cfg(&d, 4, Some((slots, 1)))).unwrap());
    }

    #[test]
    fn build_gate_waits_for_build_recipes() {
        // Uses the real derive path: a heavy BUILD_GATE must see build-recipes'
        // effect. Here build-recipes is synthesized directly.
        let d = tmpdir("bg");
        let mut set = synth(
            &d,
            &[
                ("consumer", Pool::Heavy, "test -e {D}/br.ran && touch {D}/ok", &["build-recipes"]),
            ],
        );
        let idx = set.gates.len();
        set.gates.push(Gate {
            name: BUILD_RECIPES.to_string(),
            pools: Vec::new(),
            body: format!("sleep 0.1 && touch {}/br.ran", d.display()),
            deps: Vec::new(),
            extra_env: Vec::new(),
            specs: Vec::new(),
        });
        set.index.insert(BUILD_RECIPES.to_string(), idx);
        let sel = expand_goals(&set, &["consumer".to_string()]).unwrap();
        assert!(run_selected(&set, &sel, &cfg(&d, 4, None)).unwrap());
        assert!(d.join("ok").exists());
    }

    #[test]
    fn resume_skips_journaled_greens_only_for_the_identical_tree_key() {
        let d = tmpdir("resume");
        let runs = |f: &str| -> usize {
            std::fs::read_to_string(d.join(f)).map(|t| t.lines().count()).unwrap_or(0)
        };
        // `a` passes and is journaled; `b` reds every time (so each run's
        // journal state is observable through a's re-execution count).
        let set = synth(
            &d,
            &[
                ("a", Pool::Heavy, "echo run >> {D}/a.runs", &[]),
                ("b", Pool::Heavy, "echo run >> {D}/b.runs; exit 1", &["a"]),
            ],
        );
        let sel = expand_goals(&set, &["check".to_string()]).unwrap();
        let with = |key: Option<&str>, resume: bool| {
            let mut c = cfg(&d, 2, None);
            c.root = d.clone();
            c.tree_key = key.map(str::to_string);
            c.resume = resume;
            c
        };
        // Red run journals a's PASS under key k1.
        assert!(!run_selected(&set, &sel, &with(Some("k1"), false)).unwrap());
        assert_eq!((runs("a.runs"), runs("b.runs")), (1, 1));
        // Resume, same key: a SKIPPED (not re-run), b re-runs.
        assert!(!run_selected(&set, &sel, &with(Some("k1"), true)).unwrap());
        assert_eq!((runs("a.runs"), runs("b.runs")), (1, 2), "a must be skipped on resume");
        // VERIFIED-RED half: a DIFFERENT key (any tree change) invalidates the
        // whole journal — a re-runs.
        assert!(!run_selected(&set, &sel, &with(Some("k2"), true)).unwrap());
        assert_eq!(runs("a.runs"), 2, "a key change must invalidate every skip");
        // A plain (non-resume) run ignores the journal entirely.
        assert!(!run_selected(&set, &sel, &with(Some("k1"), false)).unwrap());
        assert_eq!(runs("a.runs"), 3, "non-resume runs must ignore the journal");
    }

    #[test]
    fn unknown_goal_is_an_error() {
        let set = load().unwrap();
        assert!(expand_goals(&set, &["not-a-gate".to_string()]).is_err());
    }
}

//! gates.rs — td's OWN gate runner: `td-builder gate-run`, the loop scheduler that
//! replaced `make` + the `Makefile` on the spine (`td-builder check` execs it
//! inside the loop sandbox).
//!
//! The Makefile used make for exactly four things: the gate-fragment registry, the
//! ordering graph (cheap serial-first, heavy after the last cheap gate, BUILD_GATES
//! after build-recipes), `-jN --output-sync=target`, and the `.SHELLFLAGS`
//! per-recipe timing hack. None of that needed make — every gate was `.PHONY`, so
//! make's actual value (file-dependency tracking) was never used — and make could
//! not give the loop the two scheduling properties it wants:
//!
//!   • PER-USER concurrency: concurrent checks share one lazy rootless host and
//!     its memory-token pool. Token flocks die with their holder, so a killed
//!     gate cannot leak capacity. Local `-j` may lower, but never raise, the
//!     memory-derived width.
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
//! A GateDef's `script` is PLAIN POSIX SHELL (no make escaping), executed as
//! one `sh -c` with cwd = repo root — inside the loop sandbox `sh` is the
//! td-built busybox ash, so no bashisms. One deliberate extension: gate
//! bodies rely on `set -o pipefail` (POSIX.1-2024, not in older POSIX sh) to
//! keep a red left of a pipe from being greened by the right side — safe
//! because the interpreter is not "whatever sh" but the PINNED busybox
//! (1.37.0) ash the loop itself built, which supports it; a shell without
//! pipefail errors on the `set` line (fail-closed), never mis-greens.
//! (The remaining deferred corpus/seed gates
//! realize their guix-built seed by calling host `guix` directly — the seed
//! bytes retire last per the north star / #412.) Output is buffered per gate
//! (`--output-sync=target`
//! parity), first red stops new gates while running ones drain, and timing
//! events keep the exact per-gate START/END line format the native report
//! reducer (gate_timing.rs) reads.

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Condvar, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The synthetic build-phase node (the former Makefile `build-recipes` target).
const BUILD_RECIPES: &str = "build-recipes";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Pool {
    Cheap,
    /// Behavioral gates — everything the full `check` runs beyond the cheap
    /// serial prefix, from the store/sandbox assertions to the deep from-seed
    /// bootstrap rungs and the from-source package corpus. One pool: there is
    /// no per-change subset any more, because a warm build cache makes the
    /// deep rungs a cache hit rather than a rebuild.
    Heavy,
    Fast,
    Engine,
    Parked,
}

/// Does this pool run in the plain full `check`? (The coverage question
/// affected-checks' default_check_covers_target asks.) THE single source of
/// that taxonomy — extend HERE when adding a pool, never in a per-site match
/// list (a missed site silently mis-answers).
pub(crate) fn pool_in_full_check(p: Pool) -> bool {
    matches!(p, Pool::Cheap | Pool::Heavy)
}

/// One gate, declared as compiled Rust data in `src/gate_defs/<NNN>-<name>.rs`.
/// The registry (`build.rs`) collects every file's `gate()` into `all()`.
pub struct GateDef {
    /// The goal name (`td-builder check <name>` runs it) — must equal the defining
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
    /// Non-blocking (allow-failure) tag: when a tagged gate FAILS the runner
    /// TOLERATES it — no fail-fast, and the run is not reded by it (it is reported
    /// as a non-blocking failure). A tagged gate that PASSES is unaffected (still
    /// full coverage). Reserved for gates whose realization depends on a host
    /// capability not every runner can satisfy, so a host that cannot is not
    /// blocked by them while a host that can still covers them normally.
    pub non_blocking: bool,
    /// The gate body: plain POSIX shell, run as one `sh -c` from the repo root.
    pub script: &'static str,
}

mod registry {
    include!(concat!(env!("OUT_DIR"), "/gate_registry.rs"));
}

#[derive(Clone, Debug)]
struct Gate {
    name: String,
    pools: Vec<Pool>,
    /// The plain-shell body (everything after `run:`), executed as one `sh -c`.
    body: String,
    /// Ordering prerequisites (gate names). All gates are phony, so make's old
    /// normal-vs-order-only (`|`) distinction collapses to "runs before".
    deps: Vec<String>,
    /// Extra env for the body (the synthetic build-recipes node uses this).
    extra_env: Vec<(String, String)>,
    /// The def's own spec list, exported to the body as TD_GATE_SPECS — the
    /// single source both the build phase and the gate's assertion loop read.
    specs: Vec<String>,
    /// Allow-failure tag (see GateDef::non_blocking): a failure is tolerated
    /// (no fail-fast, does not red the run).
    non_blocking: bool,
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
        // Empty script ⟺ native (typed-Rust) gate (#318 axis 3): a native gate
        // carries no shell and is run via `td-builder gate-body <name>`; a shell
        // gate must carry a script. Mismatch either way is a load-time error, so
        // a typo (empty script with no registered body, or a body-registered
        // gate that still ships shell) can never silently no-op.
        let native = crate::gate_bodies::is_native(def.name);
        if def.script.trim().is_empty() != native {
            return Err(if native {
                format!(
                    "gate-run: native gate `{}` must have an empty script (its body is \
                     gate_bodies.rs)",
                    def.name
                )
            } else {
                format!("gate-run: gate `{}` has an empty script", def.name)
            });
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
            non_blocking: def.non_blocking,
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
        body: "sh tests/build-recipes.sh".to_string(),
        deps: last_cheap.iter().cloned().collect(),
        extra_env: vec![("TD_BUILD_SPECS".to_string(), set.build_specs.join(" "))],
        specs: Vec::new(),
        // build-recipes builds the corpus via the shared daemon — it fails
        // on a host that cannot realize the seed, so it is non-blocking
        // too (its SoftFailed still satisfies its BUILD_GATE dependents'
        // readiness).
        non_blocking: true,
    };
    set.index.insert(BUILD_RECIPES.to_string(), set.gates.len());
    set.gates.push(br);

    if let Some(lc) = &last_cheap {
        for p in [Pool::Heavy, Pool::Engine] {
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

/// A goal string that selects a whole TIER (a pool or combination of pools)
/// rather than naming one gate — the single source of truth `expand_goals`
/// and `explicit_goal_indices` both dispatch on, so the two can't drift apart
/// (issue #377 review).
fn is_tier_keyword(goal: &str) -> bool {
    matches!(goal, "check" | "check-fast" | "check-engine")
}

/// Expand the requested goals into the set of node indices to run (make
/// semantics kept: prerequisites always run, so take the transitive dep closure).
fn expand_goals(set: &GateSet, goals: &[String]) -> Result<HashSet<usize>, String> {
    let mut sel: HashSet<usize> = HashSet::new();
    let add_pool = |sel: &mut HashSet<usize>, p: Pool| sel.extend(set.members(p));
    for goal in goals {
        if is_tier_keyword(goal) {
            match goal.as_str() {
                // `check` is the ONE behavioral tier: there is no bounded
                // per-change subset to hold a subset relation against, so a
                // gate that is in a pool is in the check every agent runs.
                "check" => {
                    add_pool(&mut sel, Pool::Cheap);
                    if let Some(i) = set.index.get(BUILD_RECIPES) {
                        sel.insert(*i);
                    }
                    add_pool(&mut sel, Pool::Heavy);
                }
                "check-fast" => {
                    // LOCAL-ONLY, VACUOUS: the Cheap + Fast pools are both empty (the
                    // fast tier's former content — the cheap guix gates
                    // eval/guix-dependence/guix-surface — is gone, #409), so check-fast
                    // expands to {} and passes as a no-op (run_selected treats an empty
                    // selection as a pass). There is no hosted `check-fast` CI job; the
                    // per-PR engine check is the HOST `cargo-test` job. The keyword is
                    // kept only so existing tooling/goal-lists don't break; running it
                    // locally proves nothing.
                    add_pool(&mut sel, Pool::Cheap);
                    add_pool(&mut sel, Pool::Fast);
                }
                "check-engine" => {
                    add_pool(&mut sel, Pool::Cheap);
                    add_pool(&mut sel, Pool::Engine);
                }
                _ => {
                    return Err(format!(
                        "gate-run: internal error: `{goal}` is a tier keyword with no \
                         dispatch arm (is_tier_keyword/expand_goals out of sync)"
                    ))
                }
            }
            continue;
        }
        match set.index.get(goal.as_str()) {
            Some(i) => {
                sel.insert(*i);
            }
            None => {
                return Err(format!(
                    "gate-run: unknown goal `{goal}` — a tier \
                     (check/check-fast/check-engine), a gate name \
                     (`td-builder gate-run list-gates`), or build-recipes"
                ))
            }
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

/// The subset of `goals` naming a gate DIRECTLY (not a tier keyword) — see
/// `RunCfg::explicit_goals` (issue #377). Deliberately does NOT take the
/// transitive-dep closure `expand_goals` does: a dependency pulled in to
/// satisfy an explicit goal is still only along for the ride.
fn explicit_goal_indices(set: &GateSet, goals: &[String]) -> HashSet<usize> {
    let mut out = HashSet::new();
    for goal in goals {
        if is_tier_keyword(goal) {
            continue;
        }
        if let Some(i) = set.index.get(goal.as_str()) {
            out.insert(*i);
        }
    }
    out
}

/// Scope the synthetic build-recipes node's TD_BUILD_SPECS to the specs the
/// SELECTED gates declare, by FILTERING the static `build_specs` accumulation —
/// order, duplicates, everything about the surviving entries is identical to
/// the full list by construction (selecting every spec-carrying gate
/// reproduces it exactly). The full `check` goal and an explicit
/// `build-recipes` goal keep the whole pool. The body always runs even with
/// ZERO scoped specs: build-recipes is also the build-gate PRELUDE (the
/// stage0-seed realize + the td-recipe-eval build that `load_recipe_eval`
/// fails-fast without) — only the per-spec pre-build scopes down
/// (tests/build-recipes.sh tolerates an empty list).
fn scope_build_recipes(set: &mut GateSet, selected: &HashSet<usize>, goals: &[String]) {
    if goals.iter().any(|g| g == "check" || g == BUILD_RECIPES) {
        return;
    }
    let Some(bi) = set.index.get(BUILD_RECIPES).copied() else { return };
    if !selected.contains(&bi) {
        return;
    }
    let specs: String = {
        let mut wanted: HashSet<&str> = HashSet::new();
        for (i, g) in set.gates.iter().enumerate() {
            if selected.contains(&i) {
                wanted.extend(g.specs.iter().map(String::as_str));
            }
        }
        let kept: Vec<&str> = set
            .build_specs
            .iter()
            .map(String::as_str)
            .filter(|s| wanted.contains(s))
            .collect();
        kept.join(" ")
    };
    let Some(br) = set.gates.get_mut(bi) else { return };
    for (k, v) in br.extra_env.iter_mut() {
        if k == "TD_BUILD_SPECS" {
            *v = specs.clone();
        }
    }
}

/// Gate-disable support (TD_CHECK_DISABLE): from `selected`, drop every index in
/// `disabled` AND every gate that transitively depends on a dropped one — a gate
/// cannot run without its prerequisite. The result stays dep-closed over what
/// remains, so the scheduler never blocks waiting on a prerequisite that will
/// never run.
fn drop_disabled(
    set: &GateSet,
    selected: &HashSet<usize>,
    disabled: &HashSet<usize>,
) -> HashSet<usize> {
    let mut kept: HashSet<usize> = selected.difference(disabled).copied().collect();
    loop {
        let mut shrank = false;
        for i in kept.iter().copied().collect::<Vec<usize>>() {
            let Some(g) = set.gates.get(i) else { continue };
            let has_dropped_dep = g.deps.iter().any(|d| match set.index.get(d) {
                Some(di) => !kept.contains(di),
                None => false,
            });
            if has_dropped_dep {
                kept.remove(&i);
                shrank = true;
            }
        }
        if !shrank {
            return kept;
        }
    }
}

/// Parse a `pool:<name>` token used by TD_CHECK_DISABLE (case-sensitive lower).
fn parse_pool(name: &str) -> Option<Pool> {
    match name {
        "cheap" => Some(Pool::Cheap),
        "heavy" => Some(Pool::Heavy),
        "fast" => Some(Pool::Fast),
        "engine" => Some(Pool::Engine),
        "parked" => Some(Pool::Parked),
        _ => None,
    }
}

/// Apply a TD_CHECK_DISABLE `spec` to `selected`: parse comma/space-separated
/// gate NAMES and `pool:<name>` tokens into the disabled index set, then drop
/// those gates and their dependents (`drop_disabled`). Returns the kept set and
/// the list of tokens that matched no gate or pool (surfaced, not silently
/// dropped). This is the whole gate-disable mechanism — a way to turn gates off
/// without editing the gate definitions.
fn filter_disabled(
    set: &GateSet,
    selected: &HashSet<usize>,
    spec: &str,
) -> (HashSet<usize>, Vec<String>) {
    let mut disabled: HashSet<usize> = HashSet::new();
    let mut unknown: Vec<String> = Vec::new();
    for tok in spec.split([',', ' ', '\t', '\n']).filter(|t| !t.is_empty()) {
        if let Some(pname) = tok.strip_prefix("pool:") {
            match parse_pool(pname) {
                Some(p) => disabled.extend(set.members(p)),
                None => unknown.push(tok.to_string()),
            }
        } else if let Some(i) = set.index.get(tok) {
            disabled.insert(*i);
        } else {
            unknown.push(tok.to_string());
        }
    }
    (drop_disabled(set, selected, &disabled), unknown)
}

/// Per-gate wall-clock history (seconds) from the last timing report — the
/// data-driven LPT order. Missing/unparseable => empty (fallback: <NNN> order).
fn duration_table(root: &Path) -> HashMap<String, f64> {
    let path = root.join(".td-build-cache/gate-timing/latest.txt");
    let Ok(text) = std::fs::read_to_string(path) else {
        return HashMap::new();
    };
    parse_duration_table(&text)
}

/// Parse the timing report's `name kind seconds` rows (the table
/// gate_timing::report writes back; split out so the write→read round trip
/// is unit-tested in gate_timing.rs).
pub(crate) fn parse_duration_table(text: &str) -> HashMap<String, f64> {
    let mut out = HashMap::new();
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

/// TD_CHECK_GATE_TIMEOUT's default: the wall-clock floor under which no gate is
/// ever killed.
const DEFAULT_GATE_TIMEOUT_SECS: u64 = 4 * 3600;

/// TD_CHECK_GATE_TIMEOUT_FACTOR's default — how many times its own measured span
/// a gate may take before the runner calls it hung.
const DEFAULT_GATE_TIMEOUT_FACTOR: u64 = 10;

/// TD_CHECK_GATE_TIMEOUT_MAX's default: the ceiling no scaled budget may pass.
const DEFAULT_GATE_TIMEOUT_MAX_SECS: u64 = 24 * 3600;

/// A gate's wall-clock budget in seconds: the floor, or `factor` times what the
/// gate was last MEASURED at, whichever is larger, capped by the ceiling.
/// 0 = unbudgeted.
///
/// Data-driven because no single constant fits both ends of this table — gates
/// span half a second to half an hour, so a budget tight enough to catch a hung
/// SHORT gate would kill a healthy long one. The floor is deliberately far above
/// every measured gate: this is a runaway backstop, not a latency SLA, and a
/// budget that reds a slow-but-working gate is worse than the runaway it would
/// have caught. A gate with no measurement gets the floor, which is also how the
/// LPT order treats one (assumed long until measured).
///
/// The CEILING is what stops the budget teaching itself to be useless. A gate
/// killed at its budget still emits its END event, and a tolerated failure —
/// `recipe-checks` is `non_blocking` — still leaves the run green, so the
/// timing report writes that span and the next run would scale from the TIMEOUT
/// rather than from a real duration: 4h becomes 40h becomes 400h. Capping the
/// scaled term bounds that ratchet at one step.
fn gate_timeout_budget(
    floor_secs: u64,
    factor: u64,
    ceiling_secs: u64,
    measured_secs: Option<f64>,
) -> u64 {
    if floor_secs == 0 {
        return 0;
    }
    let scaled = match measured_secs {
        // Saturating rather than wrapping, and rounding UP: a corrupt or absurd
        // table row must never produce a SMALL budget, which would red every
        // gate that read it, and truncating would put the budget under the
        // factor this promises.
        Some(d) if d.is_finite() && d > 0.0 => match (d * factor as f64).ceil() {
            s if s >= u64::MAX as f64 => u64::MAX,
            s => s as u64,
        },
        _ => 0,
    };
    // The ceiling never drops below the floor: a misconfigured pair must not be
    // able to produce a budget tighter than the floor promises.
    floor_secs.max(scaled).min(ceiling_secs.max(floor_secs))
}

// ---------------------------------------------------------------------------
// The per-user memory-token pool.

/// Every hosted check, gate, and persistent-daemon build contends on the same
/// rootless token files created by the user's lazy check host.
struct SlotPool;

enum Grant {
    /// A held memory grant; dropping it releases every token flock.
    Held(crate::check_memory::MemoryPermit),
    /// Direct `gate-run` use outside `td-builder check`: serialized and small.
    Standalone,
    /// The run failed while waiting — do not start the gate.
    Aborted,
}

impl SlotPool {
    fn acquire(&self, aborted: &dyn Fn() -> bool) -> Grant {
        if std::env::var_os(crate::check_memory::HOST_CHILD_ENV).is_none() {
            return Grant::Standalone;
        }
        match crate::check_memory::gate_permit(aborted) {
            Ok(permit) => Grant::Held(permit),
            Err(e) => {
                eprintln!("gate-run: cannot acquire hosted memory grant: {e}");
                Grant::Aborted
            }
        }
    }
}

pub(crate) fn nproc() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1)
}

fn slot_pool_from_env() -> SlotPool {
    SlotPool
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
/// gate_timing.rs reduces); best-effort (a logging hiccup must
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

/// Run one gate's body under `sh -c` (capped by a pre_exec
/// setrlimit(RLIMIT_DATA) when a per-process memory cap is configured),
/// stdout+stderr appended in order to LOG_PATH (the per-gate output buffer).
/// Returns success.
/// Snapshot every current descendant of `root`, including descendants that
/// created their own process group. RSS is read from `/proc/PID/statm`.
fn process_tree_snapshot(root: u32) -> Vec<(u32, u64)> {
    const PAGE: u64 = 4096; // platform pinned x86_64-linux
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut rows = Vec::new();
    for e in entries.flatten() {
        let name = e.file_name();
        let Some(pid_text) = name
            .to_str()
            .filter(|n| n.bytes().all(|b| b.is_ascii_digit()))
        else {
            continue;
        };
        let Some(pid) = pid_text.parse::<u32>().ok() else {
            continue;
        };
        let Some((_state, parent, _group)) = proc_state_parent_pgrp(pid_text) else {
            continue;
        };
        let rss = std::fs::read_to_string(format!("/proc/{pid}/statm"))
            .ok()
            .and_then(|statm| {
                statm
                    .split_whitespace()
                    .nth(1)
                    .and_then(|value| value.parse::<u64>().ok())
            })
            .unwrap_or(0)
            .saturating_mul(PAGE);
        rows.push((pid, parent, rss));
    }
    let mut tree = Vec::new();
    let mut frontier = vec![root];
    while let Some(parent) = frontier.pop() {
        for (pid, ppid, rss) in &rows {
            if (*pid == root || *ppid == parent) && !tree.iter().any(|(seen, _)| seen == pid) {
                tree.push((*pid, *rss));
                frontier.push(*pid);
            }
        }
    }
    tree
}

fn process_tree_rss_bytes(root: u32) -> u64 {
    process_tree_snapshot(root)
        .into_iter()
        .fold(0u64, |total, (_, rss)| total.saturating_add(rss))
}

fn kill_process_tree(root: u32) {
    let mut pids: Vec<u32> = process_tree_snapshot(root)
        .into_iter()
        .map(|(pid, _)| pid)
        .filter(|pid| *pid != root)
        .collect();
    pids.sort_unstable_by(|a, b| b.cmp(a));
    let _ = crate::sys::kill_process_group(root, crate::sys::SIGKILL);
    let _ = crate::sys::kill_pid(i64::from(root), crate::sys::SIGKILL);
    for pid in pids {
        let _ = crate::sys::kill_pid(i64::from(pid), crate::sys::SIGKILL);
    }
}

/// LIVE members of process group `pgid`. Zombies are excluded: the gate's
/// namespace supervisor is held unreaped while teardown owns its process-group
/// id. The nested PID namespace is the stronger boundary: when its PID 1 exits,
/// the kernel removes even descendants that escaped this group.
fn live_survivors(pgid: u32) -> Vec<u32> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else { return out };
    for e in entries.flatten() {
        let name = e.file_name();
        let Some(pid) = name.to_str().filter(|n| n.bytes().all(|b| b.is_ascii_digit())) else {
            continue;
        };
        let Some((state, _parent, group)) = proc_state_parent_pgrp(pid) else { continue };
        if group != pgid || state == 'Z' {
            continue;
        }
        if let Ok(n) = pid.parse() {
            out.push(n);
        }
    }
    out
}

/// A process's state, parent, and process group from `/proc/<pid>/stat`, the one
/// parse both the RSS sampler and sweep use. They are fields 3 through 5, taken
/// after the LAST ')' because comm is field 2 and may contain both spaces and
/// parentheses — counting from the left mis-parses `sh -c 'x) y'`.
fn proc_state_parent_pgrp(pid: &str) -> Option<(char, u32, u32)> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let mut it = stat.rsplit_once(')')?.1.split_whitespace();
    let state = it.next()?.chars().next()?;
    let parent = it.next()?.parse().ok()?;
    Some((state, parent, it.next()?.parse().ok()?))
}

/// How much of one survivor's argv reaches the log. argv can be ARG_MAX long
/// (2 MiB on Linux) and there is nothing to learn past the first screenful.
/// A soft bound: the char that crosses it is escaped whole rather than split.
const CMDLINE_MAX: usize = 200;

/// How many survivors are named. A gate that forked a thousand of something
/// says the same thing in twenty lines, and the count above them is exact.
const CMDLINE_LINES_MAX: usize = 20;

/// The drain after the sweep's kill. Two seconds of 20ms ticks: delivery is
/// immediate for anything runnable, and what is not runnable is in an
/// uninterruptible sleep that no longer bound would fix either. The window is
/// spelled out rather than multiplied, since `Duration * u32` panics on
/// overflow and this crate takes no panicking arithmetic off a test.
const DRAIN_TICK: Duration = Duration::from_millis(20);
const DRAIN_TICKS: u32 = 100;
const DRAIN_WINDOW: Duration = Duration::from_secs(2);

/// So a 2 MiB argv is not read to print 200 bytes of it. Every raw byte
/// contributes at least one to the output (a NUL becomes a space), so reading
/// this many guarantees the cut fires for anything longer.
const CMDLINE_READ: u64 = CMDLINE_MAX as u64 + 2;

/// `pid: argv` for a process with a readable cmdline, or None.
///
/// The argv is ESCAPED rather than printed raw, which is the half that is not
/// cosmetic: it is whatever a gate spawned, and this line lands in the log a
/// later pass re-reads. That pass's sentinel is printable, so escaping alone
/// cannot make the line safe — what makes it safe is that the sentinel is read
/// BEFORE the sweep writes. Escaping closes the rest: without it an argument
/// holding a newline could forge a whole `gate-run:` line.
fn live_cmdline(pid: u32) -> Option<String> {
    use std::io::Read;
    let mut raw = Vec::new();
    std::fs::File::open(format!("/proc/{pid}/cmdline"))
        .and_then(|f| f.take(CMDLINE_READ).read_to_end(&mut raw))
        .ok()?;
    if raw.is_empty() {
        return None;
    }
    Some(format_cmdline(pid, &raw))
}

/// The formatting half of `live_cmdline`, split out because it is the half with
/// a property to check and `/proc` cannot be asked for an argv of choice.
fn format_cmdline(pid: u32, raw: &[u8]) -> String {
    let mut line = format!("{pid}: ");
    let mut cut = false;
    // The terminating NUL is stripped rather than the parts filtered, so an
    // argument that IS empty stays in the line it is printed on.
    for (n, arg) in raw.strip_suffix(&[0]).unwrap_or(raw).split(|b| *b == 0).enumerate() {
        // Checked HERE as well as per char, because the separator is what an
        // empty argument contributes: a NUL-padded cmdline is all separators
        // and would otherwise grow the line without ever entering the loop
        // below, so nothing would mark it cut.
        if line.len() >= CMDLINE_MAX {
            cut = true;
            break;
        }
        if n > 0 {
            line.push(' ');
        }
        for c in String::from_utf8_lossy(arg).chars() {
            if line.len() >= CMDLINE_MAX {
                cut = true;
                break;
            }
            escape_char(c, &mut line);
        }
        if cut {
            break;
        }
    }
    if cut {
        line.push_str("...");
    }
    line
}

/// One char into `out`, control bytes as `\xNN` and `\` doubled so the escape
/// cannot be spelled by the argv itself.
fn escape_char(c: char, out: &mut String) {
    if c == '\\' {
        out.push_str("\\\\");
    } else if c.is_control() {
        let mut buf = [0u8; 4];
        for b in c.encode_utf8(&mut buf).as_bytes() {
            out.push('\\');
            out.push('x');
            out.push(hex_nibble(b >> 4));
            out.push(hex_nibble(b & 0xf));
        }
    } else {
        out.push(c);
    }
}

fn hex_nibble(n: u8) -> char {
    let n = n & 0xf;
    char::from(if n < 10 { b'0' + n } else { b'a' + n - 10 })
}

/// How one gate ended. `Unprovisioned` (the body exited EXIT_UNPROVISIONED, 69)
/// is a runner-setup gap — no toolchain reachable in this jail — tolerated like a
/// non-blocking failure but reported as a skip, NOT a red. Any other nonzero exit
/// is `Failed` (a real regression). (re #469.)
#[derive(Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Passed,
    Unprovisioned,
    Failed,
}

/// True iff the finished gate's captured log carries the unprovisioned sentinel
/// every td-builder `EXIT_UNPROVISIONED` exit prints. Read AFTER `wait()` — the
/// child is dead and its writes to the log are flushed — so a token its
/// provisioning path emitted is present. Bytes, not String: a build log routinely
/// carries non-UTF-8, and a lossy decode must not drop the token.
fn log_has_unprovisioned_sentinel(log_path: &Path, upto: u64) -> bool {
    use std::io::Read;
    let needle = crate::check_loop::UNPROVISIONED_SENTINEL.as_bytes();
    if needle.is_empty() {
        return true;
    }
    // KMP keeps the sentinel check constant-memory even when a failing gate
    // emitted a multi-gigabyte log before exit 69. The bounded file handle is
    // read only through the body-length snapshot, excluding survivor-sweep
    // diagnostics appended afterwards.
    let mut prefix = vec![0usize; needle.len()];
    let mut matched = 0usize;
    for index in 1..needle.len() {
        while matched > 0 && needle.get(index) != needle.get(matched) {
            matched = prefix
                .get(matched.saturating_sub(1))
                .copied()
                .unwrap_or(0);
        }
        if needle.get(index) == needle.get(matched) {
            matched = matched.saturating_add(1);
        }
        if let Some(slot) = prefix.get_mut(index) {
            *slot = matched;
        }
    }
    let Ok(file) = std::fs::File::open(log_path) else {
        return false;
    };
    let mut reader = file.take(upto);
    let mut buf = [0u8; 64 * 1024];
    loop {
        let count = match reader.read(&mut buf) {
            Ok(0) => return false,
            Ok(count) => count,
            Err(_) => return false,
        };
        let Some(bytes) = buf.get(..count) else {
            return false;
        };
        for byte in bytes {
            while matched > 0 && Some(byte) != needle.get(matched) {
                matched = prefix
                    .get(matched.saturating_sub(1))
                    .copied()
                    .unwrap_or(0);
            }
            if Some(byte) == needle.get(matched) {
                matched = matched.saturating_add(1);
            }
            if matched == needle.len() {
                return true;
            }
        }
    }
}

fn run_gate(
    g: &Gate,
    root: &Path,
    log_path: &Path,
    timing: Option<&Path>,
    goal_words: &str,
    mem_mib: u64,
    tree_mem_mib: u64,
    timeout_secs: u64,
    job_budget_bytes: u64,
    grant_held: bool,
) -> Outcome {
    let mut logf = match std::fs::File::create(log_path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("gate-run: cannot open log for gate {}: {e}", g.name);
            return Outcome::Failed;
        }
    };
    timing_event(timing, &g.name, "START");
    // A native (typed-Rust) gate carries an empty body: run it as `<current_exe>
    // gate-body <name>` instead of `sh -c <script>` (#318 axis 3).
    let native = g.body.trim().is_empty();
    let body = g.body.clone();
    let self_exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            let _ = writeln!(logf, "gate-run: FAIL: gate {}: cannot resolve current_exe: {e}", g.name);
            timing_event(timing, &g.name, "END");
            return Outcome::Failed;
        }
    };
    let gate_request_lock = if grant_held {
        match crate::check_memory::create_gate_request_lock() {
            Ok(path) => Some(path),
            Err(e) => {
                let _ = writeln!(
                    logf,
                    "gate-run: FAIL: gate {}: cannot create daemon-request lease: {e}",
                    g.name
                );
                timing_event(timing, &g.name, "END");
                return Outcome::Failed;
            }
        }
    } else {
        None
    };
    let outcome = (|| {
        let (out, err) = match (logf.try_clone(), logf.try_clone()) {
            (Ok(o), Ok(e)) => (o, e),
            _ => return Outcome::Failed,
        };
        // Each body runs behind an already-exec'd namespace supervisor. Linux
        // tears down every remaining member when namespace PID 1 exits,
        // including double forks that called setsid, so a gate permit is a
        // process-lifetime boundary without a privileged cgroup.
        let (body_program, body_args): (String, Vec<String>) = if native {
            (
                self_exe.display().to_string(),
                vec!["gate-body".to_string(), g.name.clone()],
            )
        } else {
            ("sh".to_string(), vec!["-c".to_string(), body])
        };
        #[cfg(not(test))]
        let mut cmd = {
            let mut command = std::process::Command::new(&self_exe);
            command
                .arg("check-pidns-run")
                .arg(&body_program)
                .args(&body_args);
            command
        };
        // A unit-test binary is a libtest harness rather than td-builder's main,
        // so re-exec its one hidden helper test and pass the real body through
        // private environment fields. Production always uses the verb above.
        #[cfg(test)]
        let mut cmd = {
            let mut command = std::process::Command::new(&self_exe);
            command
                .args([
                    "--exact",
                    "gates::tests::pid_namespace_exec_helper",
                    "--quiet",
                    "--test-threads=1",
                ])
                .env("TD_TEST_PIDNS_PROGRAM", &body_program)
                .env("TD_TEST_PIDNS_ARG_COUNT", body_args.len().to_string());
            for (index, arg) in body_args.iter().enumerate() {
                command.env(format!("TD_TEST_PIDNS_ARG_{index}"), arg);
            }
            command
        };
        // A pre_exec setrlimit(RLIMIT_DATA) caps the wrapper and everything it
        // forks/execs — td's own prlimit(1) replacement, so the memory backstop
        // needs no host binary inside the loop sandbox.
        if mem_mib > 0 {
            crate::sandbox::cap_child_data_rlimit(&mut cmd, mem_mib.saturating_mul(1024 * 1024));
        }
        cmd.current_dir(root)
            .env("TD_GATE_GOALS", goal_words)
            .env("TD_BUILDER_SELF", &self_exe)
            .env(
                crate::check_memory::JOB_BUDGET_ENV,
                job_budget_bytes.to_string(),
            )
            .env(
                "CARGO_BUILD_JOBS",
                crate::check_memory::jobs_for_budget(job_budget_bytes, nproc()).to_string(),
            )
            .stdout(std::process::Stdio::from(out))
            .stderr(std::process::Stdio::from(err));
        if grant_held {
            cmd.env(crate::check_memory::GATE_GRANT_HELD_ENV, "1");
            if let Some(path) = &gate_request_lock {
                cmd.env(crate::check_memory::GATE_REQUEST_LOCK_ENV, path);
            }
        } else {
            cmd.env_remove(crate::check_memory::GATE_GRANT_HELD_ENV);
            cmd.env_remove(crate::check_memory::GATE_REQUEST_LOCK_ENV);
        }
        if !g.specs.is_empty() {
            cmd.env("TD_GATE_SPECS", g.specs.join(" "));
        }
        for (k, v) in &g.extra_env {
            cmd.env(k, v);
        }
        // Own process group: the tree watchdog kills the ordinary tree by pgid
        // plus a descendant snapshot for nested process groups, and a gate's
        // children must never share the runner's group.
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        // …and the gate dies with the runner. The watchdog above only protects a
        // run whose runner is alive to fire it; a runner that is KILLED would
        // otherwise leave the gate reparented to init, spinning with nothing
        // waiting on it.
        //
        // INVARIANT: whichever thread spawns here must be the one that waits
        // below. PDEATHSIG watches the parent THREAD, so spawning from the
        // watchdog thread — or handing the `Child` to another — would SIGKILL
        // every gate the moment that thread finished, reported as an ordinary
        // body failure (exit 137) with nothing naming the cause.
        crate::sandbox::die_with_parent(&mut cmd);
        // A NATIVE gate execs this binary, which cargo may be relinking — see
        // `crate::spawn`. The message below says "bash" for either kind, which
        // is wrong for a native gate and was written when every gate was a
        // shell one, so it names what was actually spawned now.
        let spawned = crate::spawn::past_a_busy_program(|| cmd.spawn());
        let what = if native { "the gate body" } else { "bash" };
        let mut child = match spawned {
            Ok(c) => c,
            Err(e) => {
                let _ =
                    writeln!(logf, "gate-run: FAIL: gate {}: cannot spawn {what}: {e}", g.name);
                timing_event(timing, &g.name, "END");
                return Outcome::Failed;
            }
        };
        let pgid = child.id();
        let stop = std::sync::atomic::AtomicBool::new(false);
        let breached = std::sync::atomic::AtomicBool::new(false);
        let timed_out = std::sync::atomic::AtomicBool::new(false);
        let watch_rss = tree_mem_mib > 0;
        let status = std::thread::scope(|ws| {
            let watchdog = (watch_rss || timeout_secs > 0).then(|| {
                ws.spawn(|| {
                    let budget = tree_mem_mib.saturating_mul(1024 * 1024);
                    // Held as a DURATION compared against `elapsed`, not as an
                    // Instant: `Instant + Duration` panics when the sum is
                    // unrepresentable, and this crate may not panic — a watchdog
                    // that died there would leave `child.wait()` blocking on the
                    // very hang it exists to end.
                    let started = std::time::Instant::now();
                    let limit = match timeout_secs {
                        0 => None,
                        n => Some(Duration::from_secs(n)),
                    };
                    // RSS keeps its 500ms cadence while the STOP flag is read
                    // five times as often, and the wait is a PARK rather than a
                    // sleep: the scope joins this thread, so whatever is left of
                    // an un-woken interval is added to every gate's measured
                    // span — and that span feeds the LPT order and the budget
                    // above. Unparked below the instant the child is waited on,
                    // so nothing is added at all.
                    let mut tick: u32 = 0;
                    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                        if watch_rss
                            && tick.is_multiple_of(5)
                            && process_tree_rss_bytes(pgid) > budget
                        {
                            breached.store(true, std::sync::atomic::Ordering::Relaxed);
                            kill_process_tree(pgid);
                            return;
                        }
                        // The TREE, not only the child: a hang is usually in a
                        // grandchild (a test binary under cargo under sh), and
                        // compiler phases may have created another group.
                        // `stop` is re-read immediately before the kill, which
                        // narrows the window where a gate exiting on its deadline
                        // is signalled anyway; what makes a kill that still slips
                        // through HARMLESS is that the waiter below does not
                        // reap. The pgid is a pid, and only the reap releases it,
                        // so until this thread is joined the worst a late kill
                        // can reach is our own dead group. (The RSS arm needs no
                        // guard at all: a zombie's /proc/<pid>/statm is all
                        // zeroes, so the group reads zero RSS from the moment it
                        // dies — held or reaped — and cannot fire late.)
                        if limit.is_some_and(|l| started.elapsed() >= l)
                            && !stop.load(std::sync::atomic::Ordering::Relaxed)
                        {
                            timed_out.store(true, std::sync::atomic::Ordering::Relaxed);
                            kill_process_tree(pgid);
                            return;
                        }
                        tick = tick.wrapping_add(1);
                        std::thread::park_timeout(Duration::from_millis(100));
                    }
                })
            });
            // Dropped exactly as `Child::wait` would, and before a wait that does
            // not: a gate inherits stdin today, so this is a no-op, but piping it
            // later would otherwise deadlock here rather than in `wait`.
            drop(child.stdin.take());
            // Wait for the body WITHOUT reaping it. The pgid IS this pid, and a
            // reap frees the number the instant it returns, so a watchdog
            // scheduled between the reap and the store below would signal
            // whatever group the kernel handed it to next. A zombie holds the
            // number until this scope joins the watchdog.
            let early = match crate::sys::wait_exited_no_reap(child.id()) {
                Ok(()) => None,
                // Only a kernel without waitid(2) should reach this. Fall back to
                // the reaping wait it replaced: the gate stays bounded, and what
                // it gives up is named in the log rather than left to be
                // inferred from a missing line.
                Err(e) => {
                    let _ = writeln!(
                        logf,
                        "gate-run: gate {}: waitid(WNOWAIT) failed ({e}); falling back to a \
                         reaping wait — the late-kill window is open for this gate",
                        g.name
                    );
                    Some(child.wait())
                }
            };
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
            // Wake it now rather than letting the scope wait out its interval.
            // An unpark before the park is not lost — it leaves a token the next
            // park consumes — so this cannot deadlock the join.
            if let Some(h) = &watchdog {
                h.thread().unpark();
            }
            early
        });
        // The scope joined the watchdog, so no further kill can be issued.
        //
        // A body that EXITS leaves whatever it started still running: the
        // watchdog only kills on the deadline or an RSS breach, so nothing at
        // all tore the group down on the healthy path, and a backgrounded test
        // binary reparented to init and kept spinning with nobody waiting on it
        // (re f370e471, which recorded this and deferred it).
        //
        // It goes HERE, between the join and the reap, and that placement is the
        // whole reason the GROUP kill can exist: the pgid IS the leader's pid,
        // and the kernel keeps that number reserved while the group still has a
        // member — the leader's own zombie being one. So this is the last
        // instant it is provably ours rather than one the kernel may have
        // reissued. On the fallback arm the wait already reaped, so the group is
        // not signalled there.
        //
        // The LISTING happens on BOTH ARMS OF THE WAIT — that axis, not the
        // watchdog's, which suppresses it below. It costs the one /proc walk the
        // drain then repeats, and it is the only thing that connects a stray
        // process to the gate that left it.
        //
        // First, though, the log's LENGTH — everything written to it so far.
        // The unprovisioned sentinel is found by an unanchored search, and
        // everything written BELOW this point is a survivor's argv, which is
        // text a gate chose; without a boundary a background process named after
        // the token would turn a real failure into a tolerated skip. Escaping
        // cannot close that one, since the token is printable. (The mark is not
        // "what the body wrote": a survivor holds the log fd too. That is not a
        // new capability — the body could always print the token itself — so the
        // boundary is about what this code appends, and nothing more.) The
        // length rather than the answer because reading the log here would put a
        // multi-megabyte read and scan on every gate's happy path for a value
        // only the failing arm ever looks at.
        //
        // Fails CLOSED. An unreadable stat leaves no boundary at all, and the
        // safe direction on this one is a genuine toolchain gap reported as a
        // red rather than a real regression tolerated as a skip.
        let body_log_len = std::fs::metadata(log_path).map(|m| m.len()).unwrap_or(0);
        let survivors = live_survivors(pgid);
        // On the deadline and RSS arms the watchdog has ALREADY SIGKILLed this
        // group, so what the walk just found is the gate's own children partway
        // through dying — still R or S, cmdline already unreadable — rather than
        // anything the gate left behind. Those arms have said what happened; a
        // list of their victims would overcount and name nothing actionable.
        // They are still DRAINED below: the point there is that the next gate
        // does not start on top of them.
        let already_killed = breached.load(std::sync::atomic::Ordering::Relaxed)
            || timed_out.load(std::sync::atomic::Ordering::Relaxed);
        if !survivors.is_empty() && !already_killed {
            let _ = writeln!(
                logf,
                "gate-run: gate {} left {} process(es) running; {}",
                g.name,
                survivors.len(),
                if status.is_none() {
                    "killing the group"
                } else {
                    "the group is NOT killed — the fallback wait already released its id, so \
                     that half may name a stranger's processes"
                }
            );
            for pid in survivors.iter().take(CMDLINE_LINES_MAX) {
                // Named even with no argv to name it by, or the count above
                // would be a number with nothing accounting for it.
                match live_cmdline(*pid) {
                    Some(line) => {
                        let _ = writeln!(logf, "gate-run:   left behind: {line}");
                    }
                    None => {
                        let _ = writeln!(
                            logf,
                            "gate-run:   left behind: {pid}: <no cmdline — mid-exec, or gone \
                             since the walk>"
                        );
                    }
                }
            }
            if let Some(more) = survivors.len().checked_sub(CMDLINE_LINES_MAX).filter(|n| *n > 0) {
                let _ = writeln!(logf, "gate-run:   left behind: … and {more} more");
            }
        }
        let killed_group = status.is_none();
        if killed_group {
            // One process-group signal reaches the ordinary descendant tree.
            let _ = crate::sys::kill_process_group(pgid, crate::sys::SIGKILL);
        }
        // kill(2) QUEUES a signal, it does not deliver one, so without this the
        // runner returns and starts the next gate while a survivor still holds a
        // lock, a port or a core — the failure this exists to prevent, arriving
        // a moment later instead. Draining is safe for exactly as long as the
        // kill was: the leader's zombie still pins the pgid, and it is excluded
        // from the walk that decides when to stop.
        //
        // RE-WALKED each tick rather than retained over the pids already found.
        // Retaining is the cheaper shape and the wrong one: a survivor that
        // forked between the walk and the kill is in the group and dies with it,
        // but it was never on that list, so the drain would return while it was
        // still alive. Re-walking also makes this
        // MEMBERSHIP rather than bare existence, so a reissued pid cannot be
        // mistaken for a survivor and waited on. The walk is /proc-wide, and
        // paying for it fifty times a second is why the loop is entered only
        // when the pre-kill walk found something — a gate that has already
        // misbehaved.
        let mut remaining = survivors.len();
        if killed_group && remaining > 0 {
            let mut ticks = 0;
            while remaining > 0 && ticks < DRAIN_TICKS {
                std::thread::sleep(DRAIN_TICK);
                ticks += 1;
                remaining = live_survivors(pgid).len();
            }
            if remaining > 0 {
                // Reported rather than waited out: nothing bounds an
                // uninterruptible sleep, and a gate that hangs the runner is
                // worse than one that leaks.
                //
                // NOT suppressed on the watchdog's arms, unlike the listing
                // above: that one names processes expected to be dying anyway,
                // while this one is a teardown that did not finish, which is
                // worth saying on every path. And it is inside the kill, so it
                // cannot claim a wait that was never made.
                let _ = writeln!(
                    logf,
                    "gate-run: gate {} — {remaining} process(es) outlived the kill by \
                     {DRAIN_WINDOW:?} and are still running",
                    g.name
                );
            }
        }
        let status = match status {
            Some(st) => st,
            None => child.wait(),
        };
        if breached.load(std::sync::atomic::Ordering::Relaxed) {
            let _ = writeln!(
                logf,
                "gate-run: FAIL: gate {} — process-tree RSS exceeded the {tree_mem_mib} MiB \
                 hosted grant; the whole descendant tree was killed",
                g.name
            );
        }
        // A process the watchdog killed cannot also have exited 0, so a
        // SUCCESSFUL status means the flag was set in the instant AFTER the body
        // finished and the kill reached a group already dead — held unreaped
        // above precisely so it is still OURS to signal. Believe the status:
        // reporting a gate that ran to completion as a timeout would red a green
        // branch on nothing but scheduling. (The RSS flag needs no such reading —
        // a dead group reads zero, so it never fires late.)
        let timed_out = timed_out.load(std::sync::atomic::Ordering::Relaxed)
            && !matches!(&status, Ok(st) if st.success());
        if timed_out {
            // Every term is named, not just the floor: when the SCALED term wins
            // the printed number is not TD_CHECK_GATE_TIMEOUT's value, and an
            // operator who raised that knob to match it would see nothing change.
            let _ = writeln!(
                logf,
                "gate-run: FAIL: gate {} — exceeded its {timeout_secs}s wall-clock budget \
                 (the larger of TD_CHECK_GATE_TIMEOUT and TD_CHECK_GATE_TIMEOUT_FACTOR x this \
                 gate's last measured span, capped by TD_CHECK_GATE_TIMEOUT_MAX); the whole \
                 descendant tree was killed",
                g.name
            );
        }
        match status {
            Ok(st) if st.success() && !breached.load(std::sync::atomic::Ordering::Relaxed) => {
                Outcome::Passed
            }
            Ok(st) => {
                // Either budget's kill lands as a SIGKILL exit, so both suppress
                // the status-derived reporting below: the budget message above
                // already said what happened, and 137 is a consequence of it.
                let breached = breached.load(std::sync::atomic::Ordering::Relaxed) || timed_out;
                // Exit 69 (EX_UNAVAILABLE), NOT a budget kill, AND the gate log
                // carries the sentinel td's own provisioning path prints: the body
                // could not provision a toolchain in this jail (re #469). Tolerated
                // as Unprovisioned, not a red — the host preflight is the
                // enforcement. Requiring the sentinel keeps the 69 TIGHT: a bare
                // `exit 69` (a stray EX_UNAVAILABLE from an unrelated tool, or an
                // accidental exit) has no token and stays a real failure, so a
                // genuine regression can never masquerade as a skip (Codex review).
                // Searched only as far as the body's own output reached: the
                // sweep appends a survivor's argv below that mark, and the
                // token is printable, so a background process named after it
                // would otherwise turn this real failure into a tolerated skip.
                if !breached
                    && td_engine::exit::host_gap_from_parts(
                        st.code(),
                        log_has_unprovisioned_sentinel(log_path, body_log_len),
                    )
                {
                    let _ = writeln!(
                        logf,
                        "gate-run: gate {} — SKIPPED (unprovisioned): body exited {} (no toolchain reachable in the loop sandbox)",
                        g.name,
                        crate::check_loop::EXIT_UNPROVISIONED
                    );
                    return Outcome::Unprovisioned;
                }
                if !breached {
                    let _ = writeln!(
                        logf,
                        "gate-run: FAIL: gate {} — body exited {}",
                        g.name,
                        st.code().unwrap_or(-1)
                    );
                }
                Outcome::Failed
            }
            Err(e) => {
                let _ = writeln!(logf, "gate-run: FAIL: gate {}: wait failed: {e}", g.name);
                Outcome::Failed
            }
        }
    })();
    if let Some(path) = gate_request_lock {
        let _ = std::fs::remove_file(path);
    }
    timing_event(timing, &g.name, "END");
    outcome
}

/// Dump one finished gate's buffered output atomically (--output-sync=target
/// parity), with a one-line PASS/FAIL trailer. Raw bytes, not String: build
/// logs routinely carry non-UTF-8 (compiler/tar output), and read_to_string
/// would silently drop the WHOLE log — the one thing a red gate must not lose.
fn print_gate_output(name: &str, log_path: &Path, outcome: Outcome, non_blocking: bool, secs: f64) {
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    // Keep target-synchronized output without allocating the entire, attacker-
    // or compiler-controlled gate log in the runner after the gate exits.
    if let Ok(mut body) = std::fs::File::open(log_path) {
        let _ = std::io::copy(&mut body, &mut lock);
    }
    let verdict = match outcome {
        Outcome::Passed => "PASS",
        Outcome::Unprovisioned => {
            "SKIPPED (unprovisioned — no toolchain in the jail; host cargo-test preflight enforces this)"
        }
        Outcome::Failed if non_blocking => "FAIL (non-blocking — tolerated)",
        Outcome::Failed => "FAIL",
    };
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
    /// A non-blocking gate that FAILED: tolerated — it satisfies dependents (like
    /// Done for readiness) and does not red the run, but is reported distinctly.
    SoftFailed,
    /// A gate whose body could not provision a toolchain in the jail (exit 69,
    /// re #469): tolerated like SoftFailed (satisfies dependents, does not red),
    /// but reported as a skip — it did NOT run, so unlike a SoftFailed it stays
    /// tolerated even when named as the explicit goal (an honest "cannot run
    /// here", not a silenced failure). Never journaled green (a provisioned host
    /// must re-run it).
    Unprovisioned,
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
    remove_logs: bool,
    /// The working-tree content key (TD_CHECK_TREE, computed host-side by
    /// `td-builder check` from git HEAD + dirty diff + untracked contents).
    /// When present, every PASS is journaled under it; None disables journaling.
    tree_key: Option<String>,
    /// --resume: skip gates journaled green for THIS tree key (issue #320).
    /// Opt-in, interactive iteration only — an automated run never passes it.
    resume: bool,
    /// TD_CHECK_GATE_TIMEOUT: the wall-clock FLOOR of a gate's budget, in
    /// seconds (0 = unbudgeted). The effective budget is this or a multiple of
    /// the gate's measured span, whichever is larger — see gate_timeout_budget.
    gate_timeout_secs: u64,
    /// TD_CHECK_GATE_TIMEOUT_FACTOR: that multiple.
    gate_timeout_factor: u64,
    /// TD_CHECK_GATE_TIMEOUT_MAX: the ceiling on the scaled term, which bounds
    /// the ratchet a timed-out gate's own recorded span would otherwise start.
    gate_timeout_max_secs: u64,
    /// AGGREGATE tree budget per gate, in MiB (0 = off): a watchdog samples the
    /// gate's descendant-tree RSS and SIGKILLs the captured tree on breach —
    /// the layer the per-process rlimit below cannot provide (N children each
    /// under the per-process cap can collectively exceed the box). The host's
    /// admission reserve remains the final backstop for a process that fully
    /// daemonizes and is reparented between samples.
    gate_tree_mem_mib: u64,
    /// Per-PROCESS RLIMIT_DATA cap for gate bodies, in MiB (0 = off). Applied
    /// via a pre_exec setrlimit in the spawned body (sys::set_rlimit — no host
    /// util-linux anywhere): with the pool over-provisioned past nproc (#319),
    /// one runaway allocator must die by its own limit — a clean red gate —
    /// instead of triggering the box OOM-killer. Per-process, so a make -jN
    /// tree of modest compilers passes.
    gate_mem_mib: u64,
    /// Original requested goal words, exported to gate bodies that need to
    /// distinguish a tier run from a direct gate run.
    goal_words: String,
    /// Gate indices named DIRECTLY in the invocation's goals (issue #377) —
    /// e.g. `store-verify` in `td-builder check store-verify` — as opposed to
    /// pulled in only via a tier keyword or as another goal's dependency. A
    /// `non_blocking` gate's failure is tolerated (SoftFailed) when it is
    /// merely along for the ride, but NOT when it IS the goal: asking "is
    /// this one gate green?" must not silently report green for a red gate.
    explicit_goals: HashSet<usize>,
}

/// True when a node takes a per-user memory grant. Only sub-five-second cheap
/// gates bypass the pool. A daemon request launched by a granted gate reuses
/// this grant, so build gates remain accounted without double-counting.
fn takes_slot(g: &Gate) -> bool {
    !g.pools.contains(&Pool::Cheap)
}

fn lock_sched<'a>(m: &'a Mutex<Sched>) -> std::sync::MutexGuard<'a, Sched> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn prepare_gate_log_dir(path: &Path) -> Result<(), String> {
    std::fs::create_dir_all(path)
        .map_err(|e| format!("gate-run: cannot create {}: {e}", path.display()))?;
    crate::sandbox::require_disk_backed(path).map_err(|e| {
        format!(
            "gate-run: log directory {} is not proven disk-backed: {e}",
            path.display()
        )
    })
}

/// Run the selected nodes. Returns Ok(true) if everything passed.
fn run_selected(set: &GateSet, selected: &HashSet<usize>, cfg: &RunCfg) -> Result<bool, String> {
    if selected.is_empty() {
        // A tier keyword can legitimately expand to NO gates — e.g. check-fast
        // since the guix gates that populated the Fast pool retired. Nothing to
        // run is a PASS, not an error; the REQUIRED tiers are held non-empty by
        // the every_tier_keyword_* registry test, not by this guard.
        eprintln!("gate-run: no gates selected for the given goals — nothing to run");
        return Ok(true);
    }
    prepare_gate_log_dir(&cfg.log_dir)?;

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
                    "[gate-run] resume: {skipped} gate(s) skipped from the verdict journal (key {key}); any tree change invalidates the whole journal"
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
            // A dep is satisfied when it is Done, SoftFailed, or Unprovisioned (a
            // tolerated failure/skip must not wedge its dependents as Pending —
            // e.g. build-recipes skipped for no toolchain must not strand the
            // store gates, which drive the host-mounted binaries, not a recompile).
            let ready = deps.iter().all(|d| {
                !s.st.contains_key(d)
                    || matches!(
                        s.st.get(d),
                        Some(St::Done) | Some(St::SoftFailed) | Some(St::Unprovisioned)
                    )
            });
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
                let mut slot_hold: Option<crate::check_memory::MemoryPermit> = None;
                if takes_slot(g) {
                    match cfg.pool.acquire(&|| lock_sched(&sched).fail) {
                        Grant::Held(permit) => slot_hold = Some(permit),
                        Grant::Standalone => {}
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
                let job_budget_bytes = slot_hold
                    .as_ref()
                    .map(crate::check_memory::MemoryPermit::bytes)
                    .or_else(crate::check_memory::request_job_budget)
                    .unwrap_or(crate::check_memory::TOKEN_BYTES);
                let outcome = run_gate(
                    g,
                    &cfg.root,
                    &log_path,
                    cfg.timing_log.as_deref(),
                    &cfg.goal_words,
                    cfg.gate_mem_mib,
                    cfg.gate_tree_mem_mib,
                    gate_timeout_budget(
                        cfg.gate_timeout_secs,
                        cfg.gate_timeout_factor,
                        cfg.gate_timeout_max_secs,
                        durations.get(&g.name).copied(),
                    ),
                    job_budget_bytes,
                    slot_hold.is_some(),
                );
                print_gate_output(
                    &g.name,
                    &log_path,
                    outcome,
                    g.non_blocking,
                    started.elapsed().as_secs_f64(),
                );
                // The whole check's /tmp is a private disk bind, not tmpfs,
                // and each finished log is discarded immediately after its
                // target-synchronized stream. This bounds both RAM and normal-
                // exit disk retention when several chatty gates run together.
                if cfg.remove_logs {
                    let _ = std::fs::remove_file(&log_path);
                }
                // Only a real PASS is journaled: an Unprovisioned skip must re-run
                // on a provisioned host, so it is never recorded green for --resume.
                if outcome == Outcome::Passed {
                    if let Some(key) = &cfg.tree_key {
                        journal_pass(&cfg.root, key, &g.name);
                    }
                }
                let mut s = lock_sched(&sched);
                // Passed → Done. Unprovisioned (exit 69) → tolerated skip. A failing
                // non-blocking gate SoftFails. All three satisfy dependents and do
                // NOT set `fail`, so none triggers the fail-fast that stops others;
                // only a hard Failed reds the run.
                let new_st = match outcome {
                    Outcome::Passed => St::Done,
                    Outcome::Unprovisioned => St::Unprovisioned,
                    Outcome::Failed if g.non_blocking => St::SoftFailed,
                    Outcome::Failed => St::Failed,
                };
                s.st.insert(gi, new_st);
                s.running -= 1;
                if new_st == St::Failed {
                    s.fail = true;
                }
                cv.notify_all();
            });
        }
    });

    let s = lock_sched(&sched);
    let names = |want: St| -> Vec<&str> {
        s.st
            .iter()
            .filter(|(_, st)| **st == want)
            .filter_map(|(i, _)| set.gates.get(*i).map(|g| g.name.as_str()))
            .collect()
    };
    // Non-blocking failures are tolerated: reported, but they do NOT red the run
    // — EXCEPT a SoftFailed gate that is itself the explicit goal (issue #377):
    // that one is about to red the run below, so it's excluded here rather than
    // reported as "tolerated" right before a contradictory RED line.
    let mut soft: Vec<&str> = Vec::new();
    let mut explicit_soft: Vec<&str> = Vec::new();
    for (i, st) in s.st.iter() {
        if *st != St::SoftFailed {
            continue;
        }
        let Some(name) = set.gates.get(*i).map(|g| g.name.as_str()) else { continue };
        if cfg.explicit_goals.contains(i) {
            explicit_soft.push(name);
        } else {
            soft.push(name);
        }
    }
    if !soft.is_empty() {
        eprintln!(
            "gate-run: {} non-blocking gate(s) FAILED but tolerated (not blocking): {}",
            soft.len(),
            soft.join(" ")
        );
    }
    let unprov = names(St::Unprovisioned);
    if !unprov.is_empty() {
        // The leading token is the stable whole-suite signal to grep for: a run
        // that exits green yet printed this SKIPPED ≥1 gate, so its green is not
        // a full-suite proof (re #469).
        eprintln!(
            "gate-run: {} {} gate(s) SKIPPED — no toolchain reachable in the jail (re #469); \
             tolerated, the host cargo-test preflight enforces these: {}",
            crate::check_loop::GATES_SKIPPED_SENTINEL,
            unprov.len(),
            unprov.join(" ")
        );
    }
    // Green iff every gate ended Done, Unprovisioned (a tolerated skip — it did
    // not run, so it stays tolerated even as the explicit goal), or SoftFailed
    // AND not the explicit goal (issue #377 — a SoftFailed gate that IS the goal
    // must red the run; one merely along for the ride stays tolerated) — no hard
    // Failed, none left Pending/Running by fail-fast.
    let green = s.st.iter().all(|(i, st)| match st {
        St::Done | St::Unprovisioned => true,
        St::SoftFailed => !cfg.explicit_goals.contains(i),
        St::Failed | St::Pending | St::Running => false,
    });
    if !green {
        let failed = names(St::Failed);
        let skipped = s.st.values().filter(|st| **st == St::Pending).count();
        eprintln!(
            "gate-run: RED — failed: {}{}{}",
            if failed.is_empty() && explicit_soft.is_empty() {
                "(none — internal error)".to_string()
            } else {
                failed.join(" ")
            },
            if !explicit_soft.is_empty() {
                format!(
                    " (non-blocking but explicitly requested, so not tolerated: {})",
                    explicit_soft.join(" ")
                )
            } else {
                String::new()
            },
            if skipped > 0 { format!(" ({skipped} gates not started)") } else { String::new() }
        );
    }
    Ok(green)
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
    line("engine", Pool::Engine);
    line("parked", Pool::Parked);
}

/// Re-print the newest run's per-gate table (the former Makefile
/// gate-timing-report target, native since #318 axis 2). Best-effort.
fn run_timing_report(root: &Path, heavy_gates: &[String]) {
    crate::gate_timing::report(root, heavy_gates);
}

/// The long-running gates the timing table classifies as heavy — ONE list so
/// the report goal and the green-run epilogue cannot drift.
fn long_gate_names(set: &GateSet) -> Vec<String> {
    set.names(Pool::Heavy)
}

pub fn cli(args: &[String]) -> ExitCode {
    let host_jobs = if std::env::var_os(crate::check_memory::HOST_CHILD_ENV).is_some() {
        crate::check_memory::hosted_gate_capacity()
    } else {
        1
    };
    let mut jobs = host_jobs;
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
                Ok(n) if n > 0 => jobs = n.min(host_jobs),
                _ => {
                    eprintln!("gate-run: bad {a} value `{v}`");
                    return ExitCode::from(2);
                }
            }
        } else if let Some(n) = a.strip_prefix("-j") {
            match n.trim().parse::<usize>() {
                Ok(v) if v > 0 => jobs = v.min(host_jobs),
                _ => {
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
    let mut set = match load() {
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
        run_timing_report(&root, &long_gate_names(&set));
        return ExitCode::SUCCESS;
    }

    let selected = match expand_goals(&set, &goals) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };

    // A way to disable gates WITHOUT editing gate definitions: TD_CHECK_DISABLE
    // lists what to skip — bare gate NAMES and/or `pool:<cheap|heavy|fast|
    // engine|parked>` tokens (comma/space separated). gate-run drops the named
    // gates AND anything that transitively depends on them (dep-closure prune), so
    // the scheduler never blocks on a prerequisite that won't run. Unknown tokens
    // are reported, not silently ignored. (Used e.g. to turn off the guix-dependent
    // gates on a host where guix can't satisfy them — `pool:heavy pool:system` —
    // without touching every gate def; re #350.)
    let selected = match std::env::var("TD_CHECK_DISABLE") {
        Ok(v) if !v.trim().is_empty() => {
            let (kept, unknown) = filter_disabled(&set, &selected, &v);
            if !unknown.is_empty() {
                eprintln!(
                    "gate-run: TD_CHECK_DISABLE: unknown gate/pool token(s) ignored: {}",
                    unknown.join(", ")
                );
            }
            let dropped = selected.len() - kept.len();
            if dropped > 0 {
                eprintln!(
                    "gate-run: TD_CHECK_DISABLE — skipping {dropped} disabled gate(s) [and any \
                     dependents]; running {} gate(s).",
                    kept.len()
                );
            }
            kept
        }
        _ => selected,
    };

    // Scope the synthetic build-recipes phase to the SELECTED gates' specs:
    // pre-building the whole 18-package corpus to run one gate was the old
    // behavior; the phase now builds exactly the specs the selected gates
    // declare. The full `check` (and an explicit `build-recipes` goal) keeps
    // the whole pool.
    scope_build_recipes(&mut set, &selected, &goals);

    let timing_log = if std::env::var("TD_GATE_TIMING").ok().as_deref() == Some("0") {
        None
    } else {
        Some(root.join(format!(".td-build-cache/gate-timing/run-{}.log", now_ns())))
    };
    let gate_budget_mib = std::env::var(crate::check_memory::GATE_TOKENS_ENV)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(1)
        .saturating_mul(crate::check_memory::TOKEN_BYTES / (1024 * 1024));
    let gate_mem_mib = gate_budget_mib.min(4096);
    let tree_key = std::env::var("TD_CHECK_TREE").ok().filter(|k| !k.is_empty());
    if resume && tree_key.is_none() {
        eprintln!(
            "gate-run: --resume needs the TD_CHECK_TREE key (td-builder check computes it from git); refusing to guess — running everything"
        );
        resume = false;
    }
    let gate_tree_mem_mib = gate_budget_mib;
    // TD_CHECK_GATE_TIMEOUT: the per-gate wall-clock floor (timeout(1) suffixes
    // s/m/h/d accepted; 0 disables). Four hours is far above every gate this
    // table has ever measured — the longest is under half an hour — because the
    // failure it exists for is a gate spinning for DAYS, and killing a healthy
    // long gate would be worse than the runaway it caught. An unparseable value
    // warns and takes the default rather than silently disabling the backstop.
    let gate_timeout_secs: u64 = match std::env::var("TD_CHECK_GATE_TIMEOUT") {
        Err(_) => DEFAULT_GATE_TIMEOUT_SECS,
        Ok(raw) => match crate::check_loop::parse_timeout_secs(raw.trim()) {
            Some(n) => n,
            None => {
                eprintln!(
                    "gate-run: TD_CHECK_GATE_TIMEOUT `{raw}` is not a duration (integer, \
                     s/m/h/d suffix ok) — using the {DEFAULT_GATE_TIMEOUT_SECS}s default"
                );
                DEFAULT_GATE_TIMEOUT_SECS
            }
        },
    };
    // Every term warns rather than defaulting in silence, for the reason the
    // floor does: each one alone decides the budget, so a typo in any of them
    // is a backstop that is not the one anybody configured.
    let gate_timeout_factor: u64 = match std::env::var("TD_CHECK_GATE_TIMEOUT_FACTOR") {
        Err(_) => DEFAULT_GATE_TIMEOUT_FACTOR,
        Ok(raw) => match raw.trim().parse() {
            Ok(n) => n,
            Err(_) => {
                eprintln!(
                    "gate-run: TD_CHECK_GATE_TIMEOUT_FACTOR `{raw}` is not a whole number — \
                     using the {DEFAULT_GATE_TIMEOUT_FACTOR}x default"
                );
                DEFAULT_GATE_TIMEOUT_FACTOR
            }
        },
    };
    let gate_timeout_max_secs: u64 = match std::env::var("TD_CHECK_GATE_TIMEOUT_MAX") {
        Err(_) => DEFAULT_GATE_TIMEOUT_MAX_SECS,
        Ok(raw) => match crate::check_loop::parse_timeout_secs(raw.trim()) {
            Some(n) => n,
            None => {
                eprintln!(
                    "gate-run: TD_CHECK_GATE_TIMEOUT_MAX `{raw}` is not a duration (integer, \
                     s/m/h/d suffix ok) — using the {DEFAULT_GATE_TIMEOUT_MAX_SECS}s default"
                );
                DEFAULT_GATE_TIMEOUT_MAX_SECS
            }
        },
    };
    let cfg = RunCfg {
        root: root.clone(),
        jobs,
        pool: slot_pool_from_env(),
        timing_log,
        log_dir: std::env::temp_dir().join(format!("td-gate-run-{}", std::process::id())),
        remove_logs: true,
        tree_key,
        resume,
        gate_mem_mib,
        gate_tree_mem_mib,
        gate_timeout_secs,
        gate_timeout_factor,
        gate_timeout_max_secs,
        goal_words: goals.join(" "),
        explicit_goals: explicit_goal_indices(&set, &goals),
    };
    match run_selected(&set, &selected, &cfg) {
        Ok(true) => {
            // Print the per-gate timing table on a green full run (best-effort).
            if goals.iter().any(|g| g == "check") {
                run_timing_report(&root, &long_gate_names(&set));
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
    fn unprovisioned_sentinel_search_is_bounded_and_crosses_read_chunks() {
        let d = tmpdir("sentinel-stream");
        let path = d.join("gate.log");
        let needle = crate::check_loop::UNPROVISIONED_SENTINEL.as_bytes();
        let prefix_len = (64usize * 1024).saturating_sub(needle.len() / 2);
        let mut contents = vec![b'x'; prefix_len];
        contents.extend_from_slice(needle);
        contents.extend_from_slice(b" ignored survivor diagnostics");
        std::fs::write(&path, &contents).unwrap();

        let through_needle = prefix_len.saturating_add(needle.len()) as u64;
        assert!(log_has_unprovisioned_sentinel(&path, through_needle));
        assert!(!log_has_unprovisioned_sentinel(
            &path,
            through_needle.saturating_sub(1)
        ));
    }

    /// Re-exec target for `run_gate` under libtest. The production binary has
    /// the hidden `check-pidns-run` verb; a unit-test executable has libtest's
    /// generated main instead, so this one exact test supplies the same wrapper
    /// without weakening what the tests exercise.
    #[test]
    fn pid_namespace_exec_helper() {
        let Ok(program) = std::env::var("TD_TEST_PIDNS_PROGRAM") else {
            return;
        };
        let count = std::env::var("TD_TEST_PIDNS_ARG_COUNT")
            .ok()
            .and_then(|raw| raw.parse::<usize>().ok())
            .unwrap_or(0);
        let mut argv = vec![program];
        for index in 0..count {
            match std::env::var(format!("TD_TEST_PIDNS_ARG_{index}")) {
                Ok(arg) => argv.push(arg),
                Err(e) => {
                    eprintln!("missing PID namespace test argument {index}: {e}");
                    std::process::exit(125);
                }
            }
        }
        let status = match crate::sandbox::pid_namespace_status(&argv) {
            Ok(status) => status,
            Err(e) => {
                eprintln!("PID namespace test wrapper failed: {e}");
                std::process::exit(125);
            }
        };
        use std::os::unix::process::ExitStatusExt as _;
        let code = status.code().unwrap_or_else(|| {
            status
                .signal()
                .map(|signal| 128i32.saturating_add(signal))
                .unwrap_or(125)
        });
        std::process::exit(code);
    }

    /// Every compiled gate_def that resolves the evaluator must propagate that
    /// step's exit status. It exits 69 with the unprovisioned sentinel when no
    /// toolchain is reachable in the jail, and run_gate reads exactly that as a
    /// tolerated SKIP; a command substitution that drops the status leaves
    /// TD_RECIPE_EVAL empty, so the gate execs an empty name and dies 126 under
    /// the jail's busybox ash — RED where the contract says skip.
    ///
    /// The rule is unconditional, with no `set -e` exemption: whether errexit is
    /// in force at the call is not decidable from the line (a later `set +e`, a
    /// `||` context, or a subshell all suspend it). Scope is the compiled
    /// registry — the synthesized build-recipes node's body is not covered here.
    ///
    /// Both spellings are checked so the rule cannot go vacuous: the gates route
    /// through `recipe-eval-place` now, and a future one reaching for the tool
    /// script directly must still propagate.
    #[test]
    fn a_gate_that_builds_the_evaluator_propagates_its_exit_status() {
        let mut resolvers = std::collections::BTreeSet::new();
        for (stem, def) in defs() {
            for line in def.script.lines() {
                // A comment naming the verb is prose, not an invocation: auditing
                // one would fail on a sentence, and counting one would let a real
                // resolution go missing under the floor below.
                if line.trim_start().starts_with('#') {
                    continue;
                }
                // EVERY occurrence is audited, not just the first, and each is
                // judged on its own command — these scripts put several
                // `;`-separated commands on one physical line, so neither an
                // earlier command's `|| exit $?` nor a later one may vouch for it.
                for tok in ["recipe-eval-place", "recipe-eval-tool.sh"] {
                    for (at, _) in line.match_indices(tok) {
                        let rest = line.get(at + tok.len()..).unwrap_or("");
                        let cmd = rest.split_once(';').map_or(rest, |(c, _)| c);
                        resolvers.insert(stem.clone());
                        assert!(
                            cmd.contains("|| exit $?"),
                            "src/gate_defs/{stem}.rs resolves td-recipe-eval without \
                             propagating its exit status:\n    {}\nA 69 (unprovisioned) then \
                             reds the gate instead of skipping it. Add `|| exit $?` — `set -e` \
                             does not count.",
                            line.trim()
                        );
                    }
                }
            }
        }
        // The ladder gates that resolve an evaluator are 360/364/414/422/426.
        // Counted by STEM, so no amount of prose in one gate can stand in for
        // another that stopped resolving — the rule cannot quietly go vacuous.
        assert!(
            resolvers.len() >= 5,
            "only {} gate defs resolve td-recipe-eval ({resolvers:?}); the rule is guarding \
             less than it was written for",
            resolvers.len()
        );
    }

    #[test]
    fn registry_loads_and_holds_the_gate_ladder() {
        // The registry is compiled in, so this runs EVERYWHERE cargo test runs —
        // including the guix td-builder package build (unlike the old
        // repo-tree-reading parser tests, which had to skip there).
        let set = load().unwrap();
        // The cheap serial-first tier is currently EMPTY: its only members were the
        // guix-oracle gates (eval/guix-dependence/guix-surface), retired under the
        // keep/retire rule (AGENTS.md "Test the feature, not the possibility"). The
        // scheduler handles an empty cheap pool — last_cheap is None, so heavy gates
        // carry no serial-barrier dep and start subject to the slot pool. A future
        // cheap gate may be added without touching this file.
        // Heavy is the whole behavioral tier: the PR-sized gates AND the slow
        // from-seed rungs + from-source corpus that used to be a separate Daily
        // pool. The floor covers both halves — the pool may gain members but
        // never lose one.
        let heavy = set.names(Pool::Heavy);
        // Thresholds ratchet DOWN as guix gates retire (the guix-removal workstream).
        // The guix-SEEDED corpus (the retired guix recipe-checks gate + the 35
        // corpus recipes) and the guix seed-capture/td-shell/subst gates were deleted — they
        // built packages on guix's gcc-toolchain/rust, not td's mes-rooted /td/store
        // toolchain — leaving only the /td/store store-native ladder, the store
        // primitives, and the engine. These guard against ACCIDENTAL loss, not
        // deliberate retirement — lower them in the same PR that removes gates.
        //
        // #397 lowered these floors: the 25 duplicate per-rung `bootstrap-<rung>.sh`
        // shell gates were retired (24 Daily + 1 Heavy — `bootstrap-cc`, the only one
        // of the 25 in the Heavy pool). The shell-helper cleanup then removed the
        // compatibility-only chain-cache gate; recipe-owned checks and the x86_64
        // gates are the surviving recipe-graph coverage. Floors are set to the EXACT
        // post-retirement counts
        // (zero headroom, matching the pre-#397 convention: 19/32/51 were exact matches
        // too) — these guard against ACCIDENTAL loss, so slack beyond the deliberate
        // retirement just lets a future PR silently drop more gates unnoticed.
        assert!(heavy.len() >= 23, "heavy pool shrank below the retirement floor: {}", heavy.len());
        for g in ["cargo-test", "store-verify", "recipe-checks"] {
            assert!(heavy.iter().any(|n| n == g), "missing heavy gate {g}");
        }
        assert!(set.names(Pool::Engine).iter().any(|n| n == "cargo-test"));
        // (The build_specs corpus is empty since the guix-seeded corpus retired —
        // build-recipes is now a corpus-free stage0 + recipe-eval prelude only.)
        // The derived graph holds: the synthetic build-recipes prelude node is present.
        let br = set.gates.iter().find(|g| g.name == BUILD_RECIPES).unwrap();
        assert!(br.extra_env.iter().any(|(k, _)| k == "TD_BUILD_SPECS"));
        assert!(takes_slot(br), "build-recipes performs compiler work and needs a grant");
        for (_, def) in defs().into_iter().filter(|(_, def)| def.build_gate) {
            let gate = set.gates.iter().find(|gate| gate.name == def.name).unwrap();
            assert!(takes_slot(gate), "build gate {} bypassed memory admission", def.name);
        }
        // Every bash body is non-empty plain bash (no make-isms survived
        // conversion). A NATIVE (typed-Rust) gate (#318 axis 3) legitimately has
        // an empty body — it runs via `td-builder gate-body <name>` — so it is
        // asserted empty-and-registered instead (the empty ⟺ is_native pairing
        // that `load` enforces).
        for g in &set.gates {
            if crate::gate_bodies::is_native(&g.name) {
                assert!(g.body.trim().is_empty(), "{} is native but carries bash", g.name);
                continue;
            }
            assert!(!g.body.trim().is_empty(), "{} has an empty body", g.name);
            assert!(!g.body.contains("$(CURDIR)"), "{} kept a make var", g.name);
            assert!(!g.body.contains("$$"), "{} kept make $$ escaping", g.name);
        }
    }

    /// Every `check-run` a gate body spells must match the CURRENT argv:
    /// `check-run STEM [INDEX]`. The retired `[pr|daily|all]` scope word sat
    /// BEFORE the index, so a body left holding it passes a non-numeric string
    /// where the index goes. `parse_index` rejects that (fail-closed), but only
    /// once a provisioned in-jail run reaches the gate — which is exactly the
    /// slow, host-dependent feedback this catches at `cargo test` instead.
    #[test]
    fn gate_bodies_spell_check_run_with_the_current_argv() {
        let set = load().unwrap();
        let mut seen = 0usize;
        for g in &set.gates {
            for line in g.body.lines() {
                let Some((_, rest)) = line.split_once("check-run ") else {
                    continue;
                };
                seen += 1;
                let args: Vec<&str> = rest.split_whitespace().collect();
                // STEM then an optional 1-based INDEX — nothing else.
                assert!(
                    args.len() <= 2,
                    "{}: `check-run {rest}` passes more than STEM [INDEX] \
                     (the retired scope word?)",
                    g.name
                );
                if let Some(index) = args.get(1) {
                    assert!(
                        index.parse::<usize>().is_ok_and(|n| n > 0),
                        "{}: `check-run {rest}` — `{index}` is not a 1-based index",
                        g.name
                    );
                }
            }
        }
        assert!(seen >= 3, "expected the x86_64 gate bodies to drive check-run");
    }

    /// `check` is the ONE behavioral tier — it selects every gate in a pool
    /// `pool_in_full_check` admits, with no subset carved out of it. The retired
    /// check-pr/daily split is what let a gate be registered yet never run.
    #[test]
    fn the_full_check_selects_every_gate_in_a_checked_pool() {
        let set = load().unwrap();
        let full = expand_goals(&set, &["check".to_string()]).unwrap();
        for (i, g) in set.gates.iter().enumerate() {
            if g.pools.iter().any(|p| pool_in_full_check(*p)) {
                assert!(full.contains(&i), "`check` lost gate {}", g.name);
            }
        }
        let bi = *set.index.get(BUILD_RECIPES).unwrap();
        assert!(full.contains(&bi), "build-recipes left the check");
    }

    #[test]
    fn every_tier_keyword_selects_a_nonempty_set_against_the_real_registry() {
        // A tier keyword that expands to {} makes `run_selected` error with
        // "nothing selected" and exit non-zero — a silently vacuous gate is a
        // real hazard when a whole pool retires (as the Cheap pool did). The
        // earlier expand_goals tests use synthetic pools (synth(...)); this one
        // runs the REAL load() registry so emptying a pool (as retiring the whole
        // Cheap pool did) cannot slip through. Guards the check-fast → Engine fold.
        let set = load().unwrap();
        // check-fast is intentionally omitted: its pool is empty since the guix
        // gates that populated it (the cheap fast-tier gates) retired, so it
        // legitimately expands to {} and PASSES as a no-op (run_selected treats
        // empty as a pass). check-engine/check MUST stay non-empty — they carry
        // the real coverage.
        for goal in ["check-engine", "check"] {
            let sel = expand_goals(&set, &[goal.to_string()]).unwrap();
            assert!(!sel.is_empty(), "tier keyword `{goal}` expanded to the empty set");
        }
    }

    #[test]
    fn build_recipes_specs_scope_to_the_selection() {
        let br_specs = |set: &GateSet| {
            set.gates
                .iter()
                .find(|g| g.name == BUILD_RECIPES)
                .and_then(|g| g.extra_env.iter().find(|(k, _)| k == "TD_BUILD_SPECS"))
                .map(|(_, v)| v.clone())
                .unwrap()
        };
        // The guix-seeded corpus (the only spec-carrying gates) retired, so build_specs
        // is empty and scoping is a no-op — the point now is that the build-recipes
        // PRELUDE body (stage0 seed + td-recipe-eval; load_recipe_eval fails-fast without
        // its sentinel) still runs for a build_gate selection, never no-op'd.
        let mut set = load().unwrap();
        assert!(set.build_specs.is_empty(), "no corpus specs after the guix-corpus retirement");
        let goals = vec!["store-verify".to_string()];
        let sel = expand_goals(&set, &goals).unwrap();
        scope_build_recipes(&mut set, &sel, &goals);
        assert_eq!(br_specs(&set), "");
        let br = set.gates.iter().find(|g| g.name == BUILD_RECIPES).unwrap();
        assert!(br.body.contains("build-recipes.sh"), "the prelude body must survive scoping");
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
                non_blocking: false,
            });
        }
        GateSet { gates, index, build_specs: Vec::new() }
    }

    fn cfg(dir: &Path, jobs: usize, _slots: Option<(PathBuf, usize)>) -> RunCfg {
        RunCfg {
            root: dir.to_path_buf(),
            jobs,
            pool: SlotPool,
            timing_log: None,
            log_dir: dir.join("logs"),
            remove_logs: false,
            tree_key: None,
            resume: false,
            gate_mem_mib: 0,
            gate_tree_mem_mib: 0,
            // Unbudgeted by default so a slow test host cannot red the rest of
            // this module; the timeout tests set their own.
            gate_timeout_secs: 0,
            gate_timeout_factor: DEFAULT_GATE_TIMEOUT_FACTOR,
            gate_timeout_max_secs: DEFAULT_GATE_TIMEOUT_MAX_SECS,
            goal_words: String::new(),
            explicit_goals: HashSet::new(),
        }
    }

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("td-gates-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn gate_logs_reject_a_memory_backed_tmp_directory() {
        let shm = Path::new("/dev/shm");
        if !shm.is_dir() {
            return;
        }
        let dir = shm.join(format!("td-gate-log-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let error = prepare_gate_log_dir(&dir).unwrap_err();

        assert!(error.contains("not proven disk-backed"), "{error}");
        let _ = std::fs::remove_dir_all(dir);
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
    fn non_blocking_gate_failure_is_tolerated_and_does_not_fail_fast() {
        // A `non_blocking` gate that FAILS must not red the run and must not stop
        // other gates (the deferred corpus/seed gates, on a host that cannot
        // realize the guix-built seed).
        let d = tmpdir("nonblock");
        let mut set = synth(
            &d,
            &[
                ("softfail", Pool::Cheap, "exit 7", &[]), // fails
                ("after", Pool::Cheap, "touch {D}/after.ran", &["softfail"]), // depends on it
                ("indep", Pool::Cheap, "touch {D}/indep.ran", &[]), // independent
            ],
        );
        // tag `softfail` non-blocking (as the guix-pin gate_defs are).
        let si = *set.index.get("softfail").unwrap();
        set.gates.get_mut(si).unwrap().non_blocking = true;

        let sel = expand_goals(&set, &["check-fast".to_string()]).unwrap();
        // GREEN despite softfail failing — the failure is tolerated.
        assert!(
            run_selected(&set, &sel, &cfg(&d, 4, None)).unwrap(),
            "a non-blocking gate's failure must not red the run"
        );
        // No fail-fast: the dependent (its SoftFailed dep is satisfied) AND the
        // independent gate both ran.
        assert!(
            d.join("after.ran").exists(),
            "a dependent of a soft-failed non-blocking gate must still run"
        );
        assert!(d.join("indep.ran").exists(), "an independent gate must still run");

        // Contrast: WITHOUT the tag the same failure reds the run + fail-fasts the
        // dependent (this is the existing blocking behavior).
        let d2 = tmpdir("nonblock-blocking");
        let set2 = synth(
            &d2,
            &[
                ("softfail", Pool::Cheap, "exit 7", &[]),
                ("after", Pool::Heavy, "touch {D}/after.ran", &["softfail"]),
            ],
        );
        let sel2 = expand_goals(&set2, &["check".to_string()]).unwrap();
        assert!(
            !run_selected(&set2, &sel2, &cfg(&d2, 4, None)).unwrap(),
            "an untagged (blocking) failure must still red the run"
        );
        assert!(!d2.join("after.ran").exists(), "blocking failure must fail-fast the dependent");
    }

    #[test]
    fn non_blocking_gate_named_as_the_explicit_goal_reds_the_run() {
        // Issue #377: `td-builder check store-verify` on a red-but-non_blocking
        // store-verify must NOT exit 0 — the non_blocking tolerance is for gates
        // pulled in as a tier member or a dependency, not for the gate the caller
        // is directly asking about.
        let d = tmpdir("nonblock-explicit");
        let mut set = synth(&d, &[("softfail", Pool::Cheap, "exit 7", &[])]);
        let si = *set.index.get("softfail").unwrap();
        set.gates.get_mut(si).unwrap().non_blocking = true;

        let sel = expand_goals(&set, &["softfail".to_string()]).unwrap();
        let mut c = cfg(&d, 4, None);
        c.explicit_goals = explicit_goal_indices(&set, &["softfail".to_string()]);
        assert!(
            !run_selected(&set, &sel, &c).unwrap(),
            "a non_blocking gate named directly as the goal must red, not silently pass"
        );

        // Contrast: the SAME gate, selected only via its tier, stays tolerated —
        // unaffected by this fix (matches the existing test above).
        let sel_tier = expand_goals(&set, &["check-fast".to_string()]).unwrap();
        let c_tier = cfg(&d, 4, None); // explicit_goals empty: not named directly
        assert!(
            run_selected(&set, &sel_tier, &c_tier).unwrap(),
            "the same gate reached only via a tier must still be tolerated"
        );
    }

    #[test]
    fn unprovisioned_gate_exit_69_is_tolerated_and_does_not_fail_fast() {
        // Shape A (re #469): a gate that exits EXIT_UNPROVISIONED (69) — its body
        // could not provision a toolchain in the jail — is tolerated like a
        // non-blocking failure WITHOUT the gate being tagged non_blocking, so a
        // genuine compile regression (any other nonzero) still reds. This is what
        // lets the in-loop compile gates (cargo-test/recipe-rs/build-recipes)
        // degrade to a skip post-guix while the host preflight enforces them.
        //
        // The 69 is tolerated ONLY when the body ALSO emits the sentinel td's own
        // provisioning path prints (proof the 69 is a real toolchain gap, not a
        // stray EX_UNAVAILABLE) — a bare `exit 69` reds, asserted below.
        let unprov = format!("echo '{}' >&2; exit 69", crate::check_loop::UNPROVISIONED_SENTINEL);
        let d = tmpdir("unprov");
        let set = synth(
            &d,
            &[
                // NOT tagged non_blocking — the 69 + sentinel earns tolerance.
                ("compile", Pool::Cheap, unprov.as_str(), &[]),
                ("after", Pool::Cheap, "touch {D}/after.ran", &["compile"]),
                ("indep", Pool::Cheap, "touch {D}/indep.ran", &[]),
            ],
        );
        let sel = expand_goals(&set, &["check-fast".to_string()]).unwrap();
        assert!(
            run_selected(&set, &sel, &cfg(&d, 4, None)).unwrap(),
            "an exit-69 (unprovisioned) gate must not red the run"
        );
        assert!(
            d.join("after.ran").exists(),
            "a dependent of an unprovisioned (skipped) gate must still run"
        );
        assert!(d.join("indep.ran").exists(), "an independent gate must still run");

        // Contrast: any OTHER nonzero from an untagged gate is a real regression —
        // it reds and fail-fasts, exactly as before this fix.
        let d2 = tmpdir("unprov-red");
        let set2 = synth(
            &d2,
            &[
                ("compile", Pool::Cheap, "exit 1", &[]),
                ("after", Pool::Heavy, "touch {D}/after.ran", &["compile"]),
            ],
        );
        let sel2 = expand_goals(&set2, &["check".to_string()]).unwrap();
        assert!(
            !run_selected(&set2, &sel2, &cfg(&d2, 4, None)).unwrap(),
            "a non-69 failure from an untagged gate must still red the run"
        );
        assert!(
            !d2.join("after.ran").exists(),
            "a real (non-69) failure must fail-fast the dependent"
        );

        // Contrast: exit 69 WITHOUT the sentinel is NOT tolerated — a stray
        // EX_UNAVAILABLE (or accidental `exit 69`) from code that is not td's
        // provisioning path reds and fail-fasts, so the 69 tolerance can never be
        // spoofed by an unrelated failure that merely shares the code (Codex).
        let d3 = tmpdir("unprov-bare69");
        let set3 = synth(
            &d3,
            &[
                ("compile", Pool::Cheap, "exit 69", &[]),
                ("after", Pool::Heavy, "touch {D}/after.ran", &["compile"]),
            ],
        );
        let sel3 = expand_goals(&set3, &["check".to_string()]).unwrap();
        assert!(
            !run_selected(&set3, &sel3, &cfg(&d3, 4, None)).unwrap(),
            "a bare exit-69 with no sentinel must red the run, not be tolerated"
        );
        assert!(
            !d3.join("after.ran").exists(),
            "a bare (sentinel-less) 69 must fail-fast the dependent"
        );
    }

    #[test]
    fn unprovisioned_gate_named_as_the_explicit_goal_stays_tolerated() {
        // Unlike a SoftFailed gate (issue #377), an Unprovisioned gate did NOT run
        // — it could not be provisioned here — so naming it directly
        // (`td-builder check cargo-test` on a toolchain-less host) reports an honest
        // skip, not a red. Asking "is this gate green here?" when it cannot run at
        // all is answered "skipped", never a fabricated failure.
        let unprov = format!("echo '{}' >&2; exit 69", crate::check_loop::UNPROVISIONED_SENTINEL);
        let d = tmpdir("unprov-explicit");
        let set = synth(&d, &[("compile", Pool::Cheap, unprov.as_str(), &[])]);
        let sel = expand_goals(&set, &["compile".to_string()]).unwrap();
        let mut c = cfg(&d, 4, None);
        c.explicit_goals = explicit_goal_indices(&set, &["compile".to_string()]);
        assert!(
            run_selected(&set, &sel, &c).unwrap(),
            "an unprovisioned gate named directly as the goal stays a tolerated skip"
        );
    }

    #[test]
    fn td_check_disable_skips_named_and_pooled_gates_and_prunes_dependents() {
        // TD_CHECK_DISABLE mechanism: a spec of gate NAMES + `pool:<name>` tokens
        // drops those gates and anything depending on them; unknown tokens are
        // surfaced, not silently ignored.
        let d = tmpdir("disable");
        let set = synth(
            &d,
            &[
                ("cheapgate", Pool::Cheap, "true", &[]),
                ("enginegate", Pool::Engine, "true", &[]),
                ("heavy_a", Pool::Heavy, "true", &[]),
                ("heavy_b", Pool::Heavy, "true", &[]),
                ("fastgate", Pool::Fast, "true", &[]),
                // in a KEPT pool, but transitively needs a dropped heavy gate:
                ("needs_heavy", Pool::Cheap, "true", &["heavy_a"]),
            ],
        );
        // selected = the whole set (as if expand_goals closed over it).
        let selected: HashSet<usize> = (0..set.gates.len()).collect();

        // Drive the real entry point: a spec mixing a pool token, a bare name, and
        // a bogus token — commas AND spaces as separators.
        let (kept, unknown) =
            filter_disabled(&set, &selected, "pool:heavy, fastgate  bogus-name");
        let names: HashSet<&str> = kept
            .iter()
            .filter_map(|i| set.gates.get(*i).map(|g| g.name.as_str()))
            .collect();

        // `pool:heavy` drops both heavy gates; `fastgate` drops the named fast gate.
        assert!(!names.contains("heavy_a"));
        assert!(!names.contains("heavy_b"));
        assert!(!names.contains("fastgate"));
        // gates with no disabled dependency survive.
        assert!(names.contains("cheapgate"));
        assert!(names.contains("enginegate"));
        // a gate depending on a dropped gate is pruned too — else the scheduler
        // would block forever on a prerequisite that never runs.
        assert!(
            !names.contains("needs_heavy"),
            "a gate depending on a dropped gate must be pruned"
        );
        assert_eq!(names.len(), 2, "only the two independent kept gates remain");
        // the bogus token is reported, not silently dropped.
        assert_eq!(unknown, vec!["bogus-name".to_string()]);
        // and a bare `pool:bogus` is unknown too, while known pools/names parse.
        let (_, unk2) = filter_disabled(&set, &selected, "pool:bogus cheapgate");
        assert_eq!(unk2, vec!["pool:bogus".to_string()]);
    }

    #[test]
    fn gate_mem_backstop_contains_a_runaway_allocator() {
        // Native pre_exec setrlimit — no host prlimit needed, so no host guard.
        let d = tmpdir("rlimit");
        // ~64 MiB heap allocation in the shell (command substitution buffers it).
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
    fn tree_watchdog_kills_a_collectively_oversized_process_group() {
        let d = tmpdir("tree");
        // Four children, ~32 MiB each — every one modest, ~128 MiB together.
        // NOTE the echo after sleep: bash EXECS a trailing command over itself,
        // which would free the 32 MiB string before the sampler's first tick —
        // the trailing echo keeps each subshell (and its allocation) resident.
        let hog = r#"for i in 1 2 3 4; do ( x=$(head -c 33554432 /dev/zero | tr '\0' a); sleep 3; echo ${#x} ) & done; wait; echo tree-done"#;
        let set = synth(&d, &[("tree", Pool::Heavy, hog, &[])]);
        let sel = expand_goals(&set, &["tree".to_string()]).unwrap();
        // VERIFIED-RED half: a 64 MiB TREE budget kills the group (each child is
        // far under any per-process cap — only the aggregate trips).
        let mut c = cfg(&d, 2, None);
        c.gate_tree_mem_mib = 64;
        assert!(!run_selected(&set, &sel, &c).unwrap(), "64MiB tree budget must red the group");
        // Green half: watchdog off, the same tree passes.
        let c = cfg(&d, 2, None);
        assert!(run_selected(&set, &sel, &c).unwrap(), "unbudgeted tree must pass");
    }

    #[test]
    fn process_tree_snapshot_crosses_a_child_process_group() {
        use std::os::unix::process::CommandExt as _;

        let mut command = std::process::Command::new("sh");
        command.arg("-c").arg("sleep 5").process_group(0);
        let mut child = command.spawn().unwrap();
        let child_pid = child.id();
        std::thread::sleep(Duration::from_millis(20));
        let found = process_tree_snapshot(std::process::id())
            .iter()
            .any(|(pid, _)| *pid == child_pid);
        let _ = crate::sys::kill_process_group(child_pid, crate::sys::SIGKILL);
        let _ = child.wait();
        assert!(
            found,
            "a compiler phase in its own process group remains in the gate RSS tree"
        );
    }

    #[test]
    fn gate_pid_namespace_reaps_a_detached_background_process() {
        let d = tmpdir("pidns-reap");
        let escaped = d.join("escaped");
        let body = format!(
            "setsid sh -c 'sleep 1; touch {}' >/dev/null 2>&1 &",
            escaped.display()
        );
        let set = synth(&d, &[("detach", Pool::Heavy, &body, &[])]);
        let sel = expand_goals(&set, &["detach".to_string()]).unwrap();
        assert!(run_selected(&set, &sel, &cfg(&d, 1, None)).unwrap());
        std::thread::sleep(Duration::from_millis(1200));
        assert!(
            !escaped.exists(),
            "a setsid descendant survived after its gate released the permit"
        );
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
            non_blocking: false,
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

    /// The budget policy. Every arm is about the SAME failure: a budget smaller
    /// than the gate's honest runtime reds a working build, which is worse than
    /// the runaway the budget exists to catch. So the floor is a floor —
    /// nothing below it, whatever the table says.
    #[test]
    fn a_gate_budget_is_never_below_its_floor() {
        const CAP: u64 = DEFAULT_GATE_TIMEOUT_MAX_SECS;
        // Disabled stays disabled, whatever was measured.
        assert_eq!(gate_timeout_budget(0, 10, CAP, None), 0);
        assert_eq!(gate_timeout_budget(0, 10, CAP, Some(1e9)), 0);
        // Unmeasured (a new gate) gets the floor.
        assert_eq!(gate_timeout_budget(3600, 10, CAP, None), 3600);
        // A short gate is covered by the floor, not by its own tiny span:
        // 10 x 0.5s would be a five-second budget.
        assert_eq!(gate_timeout_budget(3600, 10, CAP, Some(0.5)), 3600);
        // A long gate scales past the floor.
        assert_eq!(gate_timeout_budget(3600, 10, CAP, Some(1000.0)), 10_000);
        // Rounding is UP: truncating would put the budget under the factor.
        assert_eq!(gate_timeout_budget(1, 10, CAP, Some(1.99)), 20);
        // factor 0 degrades to the floor rather than to zero — "no scaling",
        // never "no budget".
        assert_eq!(gate_timeout_budget(3600, 0, CAP, Some(1000.0)), 3600);
        // A corrupt table row must not produce a SMALL budget.
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -1.0, 0.0] {
            assert_eq!(
                gate_timeout_budget(3600, 10, CAP, Some(bad)),
                3600,
                "a {bad} span must fall back to the floor"
            );
        }
        // …nor may a huge one wrap to something tiny. Asserted as the CEILING
        // rather than as ">= floor": the loose form passed while this returned
        // u64::MAX, which was the one value that made the watchdog's deadline
        // arithmetic unrepresentable — the test blessed the crashing case.
        assert_eq!(gate_timeout_budget(3600, 10, CAP, Some(f64::MAX)), CAP);
        assert_eq!(gate_timeout_budget(3600, u64::MAX, CAP, Some(1000.0)), CAP);

        // The ceiling bounds the ratchet: a gate killed at a 4h budget records
        // that span, and scaling from it would ask for 40h next run.
        assert_eq!(gate_timeout_budget(4 * 3600, 10, CAP, Some(4.0 * 3600.0)), CAP);
        // It is a cap on the SCALED term, never a cut below the floor: a
        // ceiling misconfigured under the floor must not tighten the budget.
        assert_eq!(gate_timeout_budget(3600, 10, 60, Some(1000.0)), 3600);
        assert_eq!(gate_timeout_budget(3600, 10, 60, None), 3600);
    }

    /// The gap this closes: before it, the runner watched a gate's MEMORY and
    /// not its clock, so a hung body blocked `child.wait()` forever. Eight
    /// `spec_helpers` test binaries spun for three days behind exactly that.
    ///
    /// A sleeping body rather than a busy loop, so a broken watchdog shows up as
    /// this test waiting rather than as a test that pins a core — and 30s rather
    /// than the 300 first written, which bounds what a REGRESSION costs the
    /// suite. The budget is 1s, so the headroom is still thirty-fold.
    #[test]
    fn a_gate_that_overruns_its_wall_clock_budget_is_killed() {
        let d = tmpdir("timeout");
        let set = synth(&d, &[("hang", Pool::Cheap, "sleep 30", &[])]);
        let sel = expand_goals(&set, &["check-fast".to_string()]).unwrap();
        let mut c = cfg(&d, 1, None);
        c.gate_timeout_secs = 1;
        let started = std::time::Instant::now();
        // The run is RED: a killed gate is a failed gate, not a tolerated skip.
        assert!(!run_selected(&set, &sel, &c).unwrap());
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(60),
            "the watchdog must kill the gate near its budget, not at the body's own end (took {elapsed:?})"
        );
        // The log must SAY it was the clock: a bare "exited 137" would send the
        // next reader hunting for a crash that never happened.
        let log = std::fs::read_to_string(d.join("logs/hang.log")).unwrap();
        assert!(
            log.contains("wall-clock budget") && log.contains("TD_CHECK_GATE_TIMEOUT"),
            "the timeout must name itself and its knob; got:\n{log}"
        );
        assert!(
            !log.contains("body exited"),
            "a budget kill must not also report the SIGKILL as the body's own exit; got:\n{log}"
        );
    }

    /// A body that exits while many things it started keep running: every one
    /// must be dead before the gate returns. Before the PID-namespace boundary
    /// the gate passed and the sleepers spun on, reparented to init.
    #[test]
    fn a_gate_does_not_leave_its_descendants_running() {
        let d = tmpdir("sweep");
        // Each child would leave a delayed marker if it outlived the body. Sixty
        // makes this exercise namespace teardown under a fork burst rather than
        // merely the one-child happy path.
        let body = r#"i=0; while [ $i -lt 60 ]; do sh -c 'sleep 1; echo escaped >> {D}/escaped' </dev/null >/dev/null 2>&1 & i=$((i+1)); done"#;
        let set = synth(&d, &[("leaky", Pool::Cheap, body, &[])]);
        let sel = expand_goals(&set, &["check-fast".to_string()]).unwrap();
        let c = cfg(&d, 1, None);
        assert!(run_selected(&set, &sel, &c).unwrap(), "the body exits 0, so the gate passes");
        std::thread::sleep(Duration::from_millis(1200));
        assert!(
            !d.join("escaped").exists(),
            "a descendant survived after the gate returned and wrote its delayed marker"
        );
    }

    /// A survivor's argv is whatever a gate spawned, and it is written into the
    /// gate's own log. A newline in it must not be able to start a line — the
    /// listing would otherwise be forgeable, whatever else reads the log later.
    #[test]
    fn a_survivors_argv_cannot_forge_a_log_line() {
        let raw = b"sleep\0\ngate-run: gate x is UNPROVISIONED\0".as_slice();
        let line = format_cmdline(41, raw);
        assert_eq!(line.lines().count(), 1, "one survivor is one line; got {line:?}");
        assert!(line.contains("\\x0a"), "the newline must be escaped; got {line:?}");
        assert!(!line.contains('\n'), "and must not survive as itself; got {line:?}");
    }

    /// The escape has to be unambiguous, or an argv can spell it: `\x0a` typed
    /// literally must not read back as an escaped newline.
    #[test]
    fn the_cmdline_escape_cannot_be_spelled_by_the_argv() {
        assert_eq!(format_cmdline(7, b"a\\x0ab\0"), "7: a\\\\x0ab");
        assert_eq!(format_cmdline(7, b"\r\x1b\0"), "7: \\x0d\\x1b");
    }

    /// argv can be ARG_MAX (2 MiB), and the log is read by a person. An empty
    /// argument still takes its place, so the line shows what was actually run.
    #[test]
    fn a_long_cmdline_is_truncated_and_an_empty_argument_is_kept() {
        let mut raw = b"prog\0\0tail\0".to_vec();
        assert_eq!(format_cmdline(9, &raw), "9: prog  tail");
        raw = [b"prog\0".as_slice(), &b"x".repeat(100_000), b"\0"].concat();
        let line = format_cmdline(9, &raw);
        assert!(line.ends_with("..."), "a cut line must say so; got {} bytes", line.len());
        // A soft bound: the char that crosses CMDLINE_MAX is escaped whole, so
        // the worst case is 199 + 8 (a two-byte C1 control at `\xNN\xNN`) + 3.
        assert!(line.len() <= CMDLINE_MAX + 10, "and must be bounded; got {} bytes", line.len());
    }

    /// The bounded read must not make a short argv look truncated, nor a long
    /// one look complete: CMDLINE_READ is what ties the two together.
    ///
    /// The all-NUL case is the one this got wrong. A NUL is a separator, and
    /// the separator was pushed OUTSIDE the length check — so a cmdline of them
    /// grew the line past the cap without the loop that marks it cut ever
    /// running, and a truncated line was presented as a complete one.
    #[test]
    fn the_bounded_cmdline_read_still_marks_what_it_cut() {
        let long = [b"p\0".as_slice(), &b"y".repeat(4096)].concat();
        let capped = long.get(..CMDLINE_READ as usize).unwrap();
        assert!(format_cmdline(3, capped).ends_with("..."), "a capped read is still a cut line");
        let padded = format_cmdline(3, &[0u8; CMDLINE_READ as usize]);
        assert!(padded.ends_with("..."), "and so is one that is all separators; got {padded:?}");
        assert!(padded.len() <= CMDLINE_MAX + 10, "and bounded; got {} bytes", padded.len());
        let short = b"p\0-v\0";
        assert_eq!(format_cmdline(3, short), "3: p -v", "and a short argv is untouched");
    }

    /// A survivor's argv is text a gate chose and the unprovisioned sentinel is
    /// printable, so escaping cannot stop one spelling it. Namespace teardown
    /// now removes the process before any survivor listing is possible; a bare
    /// `exit 69` must stay a real failure and the detached argv must not be
    /// copied into the body-output portion of the log.
    #[test]
    fn a_survivors_argv_cannot_forge_the_unprovisioned_sentinel() {
        let d = tmpdir("forge69");
        // The trailing `:` stops the shell tail-exec'ing `sleep` over its own
        // argv, which is where the token has to stay to be swept up.
        let body = format!(
            "sh -c 'sleep 30; :' '{}' </dev/null >/dev/null 2>&1 & exit 69",
            crate::check_loop::UNPROVISIONED_SENTINEL
        );
        let set = synth(&d, &[("forger", Pool::Cheap, body.as_str(), &[])]);
        let sel = expand_goals(&set, &["check-fast".to_string()]).unwrap();
        assert!(
            !run_selected(&set, &sel, &cfg(&d, 1, None)).unwrap(),
            "a bare exit 69 must red even when a survivor's argv carries the sentinel"
        );
        let log = std::fs::read_to_string(d.join("logs/forger.log")).unwrap();
        assert!(
            !log.contains(crate::check_loop::UNPROVISIONED_SENTINEL),
            "a detached argv must not forge the body-output sentinel; got:\n{log}"
        );
    }

    /// A pid whose cmdline cannot be read — it vanished between the walk and
    /// the read, or it is mid-`execve`. The count comes from the walk and the
    /// argv only from `/proc`, so the two must not be able to disagree.
    #[test]
    fn a_pid_with_no_readable_cmdline_answers_none() {
        // pid 0 is never a process, so this is that shape without racing one.
        assert_eq!(live_cmdline(0), None);
    }

    /// The other half, and the one that matters for false positives: with no
    /// budget set, nothing about the watchdog may touch a gate that finishes.
    #[test]
    fn an_unbudgeted_gate_is_left_alone() {
        let d = tmpdir("timeout-off");
        let set = synth(&d, &[("slow", Pool::Cheap, "sleep 2; touch {D}/slow.ran", &[])]);
        let sel = expand_goals(&set, &["check-fast".to_string()]).unwrap();
        let c = cfg(&d, 1, None); // gate_timeout_secs = 0
        assert!(run_selected(&set, &sel, &c).unwrap());
        assert!(d.join("slow.ran").exists(), "an unbudgeted gate must run to completion");
    }

    /// A gate must not outlive the runner that spawned it. The incident this
    /// guards left eight test binaries reparented to init, still spinning three
    /// days later, because nothing tied their lives to the run's.
    ///
    /// Driven through a re-exec of this binary because the property is about the
    /// SPAWNER dying, and the test process cannot be the one to die: the child
    /// role spawns a sleeper and exits at once, and the grader then requires the
    /// sleeper to be gone. The control arm — the same spawn UNARMED — is what
    /// keeps it from passing vacuously, since a sleeper that died for any other
    /// reason would satisfy the armed half alone.
    ///
    /// What it does NOT observe is thread-vs-process: libtest runs this on a
    /// spawned thread, so the armed sleeper dies when that THREAD exits either
    /// way. The production invariant that makes the difference — spawn and wait
    /// on one thread — is documented at the call site, not asserted here.
    #[test]
    fn a_gate_child_does_not_outlive_the_runner() {
        const ROLE: &str = "TD_TEST_PDEATHSIG_ROLE";
        const NAME: &str = "gates::tests::a_gate_child_does_not_outlive_the_runner";

        // The re-exec'd half: spawn a sleeper, print its pid, exit immediately.
        // Nothing here asserts — the grader reads the outcome out of /proc.
        if let Ok(role) = std::env::var(ROLE) {
            let mut cmd = std::process::Command::new("sleep");
            // NULL stdio, or the sleeper inherits the pipes `output()` is
            // reading and the grader cannot see EOF until the sleep ENDS —
            // which made the control arm block its full 30s and then grade an
            // already-exited process. Measured: 30.02s and a lost race with
            // inherited pipes, 23ms and a live sleeper without them.
            cmd.arg("30")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            if role == "armed" {
                crate::sandbox::die_with_parent(&mut cmd);
            }
            if let Ok(child) = cmd.spawn() {
                println!("PID {}", child.id());
            }
            return;
        }

        // A zombie keeps its /proc entry, so a pathname test alone would read a
        // killed sleeper as alive — and an exited control as surviving, which is
        // the vacuity the control arm exists to prevent. These are grandchildren
        // and cannot be reaped here, so the state field is the only honest
        // answer; it follows the LAST ')', a comm being able to contain both
        // spaces and parentheses.
        let alive = |pid: u32| -> bool {
            let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
                return false;
            };
            match stat.rsplit_once(')') {
                Some((_, rest)) => !matches!(rest.split_whitespace().next(), Some("Z") | None),
                None => false,
            }
        };
        // Run one role and hand back the sleeper's pid.
        let spawn_role = |role: &str| -> Option<u32> {
            let exe = std::env::current_exe().ok()?;
            let out = std::process::Command::new(exe)
                .args(["--exact", NAME, "--nocapture"])
                .env(ROLE, role)
                .output()
                .ok()?;
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .find_map(|l| l.strip_prefix("PID ")?.trim().parse::<u32>().ok())
        };

        let armed = spawn_role("armed").expect("the armed role must report a pid");
        let mut leaked: Vec<u32> = vec![armed];
        // `output()` has already reaped the spawner, so the kernel has delivered
        // the signal; poll rather than assume the target has been scheduled.
        let mut gone = false;
        for _ in 0..100 {
            if !alive(armed) {
                gone = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let bare = spawn_role("bare");
        let bare_survived = bare.is_some_and(alive);
        // SIGNALLED, not reaped: these are grandchildren, so this process cannot
        // wait on them — the kill is only what stops a 30s sleeper outliving the
        // test. Both go before either assert, and an `alive` check guards each,
        // so a failing run neither leaks a sleeper nor signals a recycled pid.
        leaked.extend(bare);
        for pid in leaked {
            if alive(pid) {
                let _ = crate::sys::kill_pid(i64::from(pid), crate::sys::SIGKILL);
            }
        }
        assert!(gone, "an armed child must die with the process that spawned it");
        assert!(
            bare_survived,
            "the control must OUTLIVE its spawner, or the armed half proves nothing"
        );
    }
}

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
//!     (TD_CHECK_SLOTS, default 8×nproc — a runaway BRAKE, not a schedule; memory admission +
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
//! events keep the exact per-gate START/END line format the native report
//! reducer (gate_timing.rs) reads.

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
    /// PR-sized behavioral gates: part of the full `check` AND of the bounded
    /// `check-pr` tier (the ~10-minute per-PR budget, human 2026-07-04).
    Heavy,
    /// Daily-only gates (the deep from-seed bootstrap rungs, the from-source
    /// package corpus, the seed-capture family): part of the full `check` the
    /// daily backstop runs (ci/daily-full-suite.sh, fix-or-revert healing),
    /// NOT of `check-pr`. Still runnable by name (`td-builder check <gate>`).
    Daily,
    Fast,
    System,
    Engine,
    Parked,
}

/// Does this pool run somewhere in the per-PR tiers (check-pr / check-fast /
/// check-engine)? THE single source the affected-checks partition derives
/// "deferred to the daily backstop" from — extend HERE when adding a pool,
/// never in a per-site match list (a missed site silently mis-partitions).
pub(crate) fn pool_runs_per_pr(p: Pool) -> bool {
    matches!(p, Pool::Cheap | Pool::Heavy | Pool::Fast | Pool::Engine)
}

/// Does this pool run in the plain full `check`? (The coverage question
/// affected-checks' default_check_covers_target asks.)
pub(crate) fn pool_in_full_check(p: Pool) -> bool {
    matches!(p, Pool::Cheap | Pool::Heavy | Pool::Daily)
}

/// The gate-state default, FLIPPED per the human direction of 2026-07-03 (#317):
/// gates share warm, machine-wide builder state by default; a gate declares a
/// private (cold) store only when clean-slate behavior IS the feature under test.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StoreMode {
    /// The default: the gate may read/populate the machine-wide content-keyed
    /// caches (the shared daemon store, the chain-brick cache). The runner
    /// exports TD_CHECK_CHAIN_CACHE (default `~/.td/build-daemon/chain`, a path
    /// host-sandbox binds into every check sandbox) unless the caller already
    /// set it — an explicitly EMPTY ambient TD_CHECK_CHAIN_CACHE forces a cold run.
    Shared,
    /// The explicit opt-out for gates whose assertions require a cold store
    /// (hermeticity/offline/sandbox probes, GC semantics, seed-alone standup):
    /// the runner force-clears TD_CHECK_CHAIN_CACHE so no warm state can leak in.
    Private,
}

/// A TYPED artifact input (#353): a store-path artifact the gate's body
/// consumes, declared on the GateDef instead of derived by shell inside it.
/// The runner resolves each declaration (gate_inputs.rs) BEFORE the script
/// body runs and exports the path as `TD_GATE_INPUT_<NAME>` (upper-cased,
/// `-`→`_`); an unresolvable input (missing lock, no match, ambiguous match)
/// REDS the gate without running its body. This replaces the per-gate
/// `grep -- '-<stem>-' LOCK | head -1` / `store-closure-scan | grep | head -1`
/// wiring and makes the gate's artifact dependencies inspectable
/// (`gate-run list-gates` prints them).
#[derive(Clone, Copy, Debug)]
pub struct ArtifactInput {
    /// The env handle: the body reads `TD_GATE_INPUT_<name upper-snake>`.
    pub name: &'static str,
    pub kind: InputKind,
}

/// How an ArtifactInput is resolved (gate_inputs::resolve).
#[derive(Clone, Copy, Debug)]
pub enum InputKind {
    /// The UNIQUE entry of a td lock whose PATH names package `stem` —
    /// exact-package matching on the basename after the 32-char hash
    /// (`bash` matches `…-bash-5.2.37`, never `…-bash-static-5.2.37`), and
    /// two matches are an error, never a silent first-wins.
    LockEntry { lock: &'static str, stem: &'static str },
    /// The UNIQUE member naming `member_stem` in the CONTENT-SCANNED runtime
    /// closure of the lock entry naming `root_stem` (the daemon's
    /// scanForReferences walk, scanned in the root's own store dir).
    ClosureMember {
        lock: &'static str,
        root_stem: &'static str,
        member_stem: &'static str,
    },
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
    /// Typed artifact inputs (#353): resolved by the runner before the body
    /// runs, exported as `TD_GATE_INPUT_<NAME>` — see ArtifactInput.
    pub inputs: &'static [ArtifactInput],
    /// Shared (warm, the default) vs Private (cold) builder state — see StoreMode.
    pub store: StoreMode,
    /// Non-blocking (allow-failure) tag: when a tagged gate FAILS the runner
    /// TOLERATES it — no fail-fast, and the run is not reded by it (it is reported
    /// as a non-blocking failure). A tagged gate that PASSES is unaffected (still
    /// full coverage). Used for the gates that fail when the host guix != the
    /// channels.scm pin, so a pin-drifted host is not blocked by them while a
    /// correctly-pinned host still runs and covers them normally.
    pub non_blocking: bool,
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
    /// Typed artifact inputs (#353), resolved per run — see ArtifactInput.
    inputs: Vec<ArtifactInput>,
    /// Shared/Private builder state (the #317 flip) — wired into the body's env.
    store: StoreMode,
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

/// Input declarations: each name must be a valid env handle, and no two may
/// collide on the MAPPED env var — env_var folds case and maps `-`/`.`/`+`
/// all to `_`, so comparing raw names would let `bash-static` and
/// `bash.static` both export TD_GATE_INPUT_BASH_STATIC, silently shadowing
/// each other (exactly what this check exists to prevent).
fn validate_input_decls(gate: &str, inputs: &[ArtifactInput]) -> Result<(), String> {
    for (i, inp) in inputs.iter().enumerate() {
        if !valid_word(inp.name) {
            return Err(format!("gate-run: gate `{gate}`: invalid input name `{}`", inp.name));
        }
        let var = crate::gate_inputs::env_var(inp.name);
        if let Some(o) =
            inputs.iter().take(i).find(|o| crate::gate_inputs::env_var(o.name) == var)
        {
            return Err(format!(
                "gate-run: gate `{gate}`: inputs `{}` and `{}` collide on the same {var} env var",
                o.name, inp.name
            ));
        }
    }
    Ok(())
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
        // carries no bash and is run via `td-builder gate-body <name>`; a bash
        // gate must carry a script. Mismatch either way is a load-time error, so
        // a typo (empty script with no registered body, or a body-registered
        // gate that still ships bash) can never silently no-op.
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
        validate_input_decls(def.name, def.inputs)?;
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
            inputs: def.inputs.to_vec(),
            store: def.store,
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
        body: "bash tests/build-recipes.sh".to_string(),
        deps: last_cheap.iter().cloned().collect(),
        extra_env: vec![("TD_BUILD_SPECS".to_string(), set.build_specs.join(" "))],
        specs: Vec::new(),
        inputs: Vec::new(),
        store: StoreMode::Shared,
        // build-recipes builds the corpus via the guix-seeded daemon — it fails
        // when host guix != pin, so it is non-blocking too (its SoftFailed still
        // satisfies its BUILD_GATE dependents' readiness).
        non_blocking: true,
    };
    set.index.insert(BUILD_RECIPES.to_string(), set.gates.len());
    set.gates.push(br);

    if let Some(lc) = &last_cheap {
        for p in [Pool::Heavy, Pool::Daily, Pool::System, Pool::Engine] {
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
    matches!(goal, "check" | "check-pr" | "check-fast" | "check-system" | "check-engine")
}

/// Expand the requested goals into the set of node indices to run (make
/// semantics kept: prerequisites always run, so take the transitive dep closure).
fn expand_goals(set: &GateSet, goals: &[String]) -> Result<HashSet<usize>, String> {
    let mut sel: HashSet<usize> = HashSet::new();
    let add_pool = |sel: &mut HashSet<usize>, p: Pool| sel.extend(set.members(p));
    for goal in goals {
        if is_tier_keyword(goal) {
            match goal.as_str() {
                // check-pr is the bounded per-PR tier (~10 min, human
                // 2026-07-04): the full check MINUS the daily-only pool — one
                // arm so the subset relation holds by construction. The daily
                // backstop (ci/daily-full-suite.sh) runs the full `check`, so
                // the daily pool keeps its coverage nightly.
                "check" | "check-pr" => {
                    add_pool(&mut sel, Pool::Cheap);
                    if let Some(i) = set.index.get(BUILD_RECIPES) {
                        sel.insert(*i);
                    }
                    add_pool(&mut sel, Pool::Heavy);
                    if goal == "check" {
                        add_pool(&mut sel, Pool::Daily);
                    }
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
                     (check/check-pr/check-fast/check-system/check-engine), a gate name \
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
        "daily" => Some(Pool::Daily),
        "fast" => Some(Pool::Fast),
        "system" => Some(Pool::System),
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
    /// Memory-PRESSURE admission ceiling (PSI `some avg10`, %): unlike
    /// MemAvailable — which LAGS allocation — pressure leads OOM and catches
    /// reclaim-thrash, so it is the primary "go up until a memory limit"
    /// signal (human direction re #319). `<= 0` disables.
    psi_limit: f64,
    /// Grant pacing (ms): at most one slot grant per interval box-wide, so a
    /// herd of ready gates cannot all pass the memory checks in the window
    /// BEFORE any of them has allocated (the lag the pace exists to damp).
    pace_ms: u64,
}

/// PSI memory `some avg10` from /proc/pressure/memory (None: no PSI).
fn mem_psi_some_avg10() -> Option<f64> {
    parse_psi_some_avg10(&std::fs::read_to_string("/proc/pressure/memory").ok()?)
}

fn parse_psi_some_avg10(text: &str) -> Option<f64> {
    let line = text.lines().find(|l| l.starts_with("some "))?;
    let field = line.split_whitespace().find_map(|w| w.strip_prefix("avg10="))?;
    field.parse().ok()
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
    /// The pace gate: one grant per pace_ms box-wide. Serialized by a flock'd
    /// pace file whose contents are the last grant's ns timestamp; a busy lock
    /// means another runner is granting RIGHT NOW — defer (that IS the pace).
    fn pace_grant(&self, dir: &Path) -> bool {
        if self.pace_ms == 0 {
            return true;
        }
        let p = dir.join("grant.pace");
        let Ok(f) = std::fs::OpenOptions::new().create(true).read(true).write(true).open(&p)
        else {
            return true; // pacing is damping, never a correctness gate
        };
        use std::os::fd::AsRawFd;
        if !matches!(crate::sys::flock_try_exclusive(f.as_raw_fd()), Ok(true)) {
            return false;
        }
        let last: u128 = std::fs::read_to_string(&p)
            .ok()
            .and_then(|t| t.trim().parse().ok())
            .unwrap_or(0);
        let now = now_ns();
        if now.saturating_sub(last) < u128::from(self.pace_ms) * 1_000_000 {
            return false;
        }
        let _ = std::fs::write(&p, now.to_string());
        true
    }

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
                let mem_ok = self.min_free_gib <= 0.0
                    || crate::build_daemon::mem_available_gib()
                        .map(|g| g >= self.min_free_gib)
                        .unwrap_or(true);
                let psi_ok = self.psi_limit <= 0.0
                    || mem_psi_some_avg10().map(|p| p < self.psi_limit).unwrap_or(true);
                if held == 0 || (mem_ok && psi_ok && self.pace_grant(dir)) {
                    return Grant::Held(f);
                }
                drop(f); // give the slot back while memory is tight or the pace gate defers
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
        Ok(v) => v.trim().parse::<usize>().unwrap_or_else(|_| 8 * nproc()),
        Err(_) => 8 * nproc(),
    };
    let min_free_gib = std::env::var("TD_MIN_FREE_GIB")
        .ok()
        .and_then(|v| v.trim().parse::<f64>().ok())
        .unwrap_or(4.0);
    let psi_limit = std::env::var("TD_CHECK_MEM_PSI")
        .ok()
        .and_then(|v| v.trim().parse::<f64>().ok())
        .unwrap_or(10.0);
    let pace_ms = std::env::var("TD_CHECK_GRANT_PACE_MS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(250);
    if n == 0 {
        return SlotPool { dir: None, n: 0, min_free_gib, psi_limit, pace_ms };
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
                return SlotPool { dir: None, n: 0, min_free_gib, psi_limit, pace_ms };
            }
        },
    };
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!(
            "gate-run: cannot create slot dir {}: {e} — running WITHOUT the machine-wide \
             slot pool (local -j is the only cap)",
            dir.display()
        );
        return SlotPool { dir: None, n: 0, min_free_gib, psi_limit, pace_ms };
    }
    SlotPool { dir: Some(dir), n, min_free_gib, psi_limit, pace_ms }
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

/// Run one gate's body under `bash -c` (through `prlimit --data` when a
/// per-process memory cap is configured), stdout+stderr appended in order to
/// LOG_PATH (the per-gate output buffer). Returns success.
/// Sum the resident bytes of every process in PGID's process group
/// (/proc/*/stat field 5 == pgid; RSS from /proc/*/statm resident pages).
fn pgroup_rss_bytes(pgid: u32) -> u64 {
    const PAGE: u64 = 4096; // platform pinned x86_64-linux
    let Ok(entries) = std::fs::read_dir("/proc") else { return 0 };
    let mut total = 0u64;
    for e in entries.flatten() {
        let name = e.file_name();
        let Some(pid) = name.to_str().filter(|n| n.bytes().all(|b| b.is_ascii_digit())) else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else { continue };
        // pgrp is field 5, but comm (field 2) may contain spaces — parse after
        // the closing paren.
        let Some(after) = stat.rsplit_once(')').map(|(_, a)| a) else { continue };
        let mut it = after.split_whitespace();
        let _state = it.next();
        let _ppid = it.next();
        let Some(grp) = it.next() else { continue };
        if grp != pgid.to_string() {
            continue;
        }
        let Ok(statm) = std::fs::read_to_string(format!("/proc/{pid}/statm")) else { continue };
        if let Some(resident) = statm.split_whitespace().nth(1).and_then(|v| v.parse::<u64>().ok())
        {
            total = total.saturating_add(resident.saturating_mul(PAGE));
        }
    }
    total
}

/// Create the gate's child cgroup with the tree budget; returns its dir.
fn cgroup_enter(run_dir: &Path, gate: &str, budget_mib: u64) -> Option<PathBuf> {
    let cg = run_dir.join(gate);
    std::fs::create_dir(&cg).ok()?;
    let bytes = budget_mib.saturating_mul(1024 * 1024);
    if std::fs::write(cg.join("memory.max"), bytes.to_string()).is_err() {
        let _ = std::fs::remove_dir(&cg);
        return None;
    }
    // Throttle-before-kill: reclaim pressure starts at 90% of the cap.
    let _ = std::fs::write(cg.join("memory.high"), (bytes / 10 * 9).to_string());
    // Swap must be bounded too: memory.swap.max defaults to `max`, so on a
    // swap-enabled host the kernel would page the gate out instead of
    // OOM-killing — the budget silently unenforced and the host thrashed
    // (review finding). Absent file = kernel without swap accounting: swap
    // can't be charged there either way.
    let swap = cg.join("memory.swap.max");
    if swap.is_file() && std::fs::write(&swap, "0").is_err() {
        let _ = std::fs::remove_dir(&cg);
        return None;
    }
    Some(cg)
}

/// oom_kill count from the cgroup's memory.events (0 when unreadable).
fn cgroup_oom_kills(cg: &Path) -> u64 {
    std::fs::read_to_string(cg.join("memory.events"))
        .ok()
        .and_then(|t| {
            t.lines()
                .find_map(|l| l.strip_prefix("oom_kill "))
                .and_then(|v| v.trim().parse().ok())
        })
        .unwrap_or(0)
}

fn run_gate(
    g: &Gate,
    root: &Path,
    log_path: &Path,
    timing: Option<&Path>,
    goal_words: &str,
    chain_cache: Option<&str>,
    mem_mib: u64,
    tree_mem_mib: u64,
    cgroup_dir: Option<&Path>,
) -> bool {
    let mut logf = match std::fs::File::create(log_path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("gate-run: cannot open log for gate {}: {e}", g.name);
            return false;
        }
    };
    timing_event(timing, &g.name, "START");
    // Typed artifact inputs (#353): resolve every declaration BEFORE the body
    // runs. A failure reds the gate with the reason in its log — the body
    // never starts on a missing/misdeclared input.
    let mut input_env: Vec<(String, String)> = Vec::new();
    for inp in &g.inputs {
        match crate::gate_inputs::resolve(root, inp) {
            Ok(p) => {
                let _ = writeln!(logf, "[gate-run] {}: input {} = {p}", g.name, inp.name);
                input_env.push((crate::gate_inputs::env_var(inp.name), p));
            }
            Err(e) => {
                let _ = writeln!(
                    logf,
                    "gate-run: FAIL: gate {}: cannot resolve declared input `{}`: {e}",
                    g.name, inp.name
                );
                timing_event(timing, &g.name, "END");
                return false;
            }
        }
    }
    // Cgroup mode (primary when delegated): the BODY self-moves into the gate's
    // cgroup before anything else runs — written by the child itself, so there
    // is no parent-side move race with early forks.
    let gate_cg = match (cgroup_dir, tree_mem_mib) {
        (Some(run_dir), b) if b > 0 => cgroup_enter(run_dir, &g.name, b),
        _ => None,
    };
    // A native (typed-Rust) gate carries an empty body: run it as `<current_exe>
    // gate-body <name>` instead of `bash -c <script>` (#318 axis 3). Same
    // wrapper (prlimit/cgroup/pgroup/env); the native body self-moves into the
    // gate cgroup itself (gate_bodies::cli), so the bash cgroup prelude — which
    // is bash-only — is skipped for it.
    let native = g.body.trim().is_empty();
    // The enter path travels via env, NEVER interpolated into the bash text:
    // an env-derived run dir containing a quote would otherwise escape the
    // quoting and execute as code (review finding).
    let body = match &gate_cg {
        Some(_) if !native => format!(
            "echo $$ > \"$TD_GATE_CG\" || {{ echo 'gate-run: cannot enter the gate cgroup' >&2; exit 97; }}\n{}",
            g.body
        ),
        _ => g.body.clone(),
    };
    let self_exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            let _ = writeln!(logf, "gate-run: FAIL: gate {}: cannot resolve current_exe: {e}", g.name);
            timing_event(timing, &g.name, "END");
            return false;
        }
    };
    let ok = (|| {
        let (out, err) = match (logf.try_clone(), logf.try_clone()) {
            (Ok(o), Ok(e)) => (o, e),
            _ => return false,
        };
        // The inner program is `bash -c <body>` (bash gate) or `<self> gate-body
        // <name>` (native gate); prlimit --data wraps either when mem_mib > 0.
        let mut cmd = if mem_mib > 0 {
            let mut c = std::process::Command::new("prlimit");
            c.arg(format!("--data={}", mem_mib.saturating_mul(1024 * 1024)));
            if native {
                c.arg(&self_exe).arg("gate-body").arg(&g.name);
            } else {
                c.arg("bash").arg("-c").arg(&body);
            }
            c
        } else if native {
            let mut c = std::process::Command::new(&self_exe);
            c.arg("gate-body").arg(&g.name);
            c
        } else {
            let mut c = std::process::Command::new("bash");
            c.arg("-c").arg(&body);
            c
        };
        cmd.current_dir(root)
            .env("TD_GUIX", GUIX_CMD)
            .env("TD_GATE_GOALS", goal_words)
            .stdout(std::process::Stdio::from(out))
            .stderr(std::process::Stdio::from(err));
        // The #317 gate-state wiring: Shared (the default) gets the machine-wide
        // chain-brick cache exported; Private gets TD_CHECK_CHAIN_CACHE FORCE-CLEARED —
        // set-and-empty, NOT unset: the consuming libs default an UNSET var to the
        // warm home, so only an explicit "" keeps warm state out of a gate whose
        // feature is clean-slate behavior. (This one env var IS the whole per-gate
        // store-mode contract — deliberately no second signal to keep in sync.)
        match g.store {
            StoreMode::Shared => {
                if let Some(cc) = chain_cache {
                    cmd.env("TD_CHECK_CHAIN_CACHE", cc);
                }
            }
            StoreMode::Private => {
                cmd.env("TD_CHECK_CHAIN_CACHE", "");
            }
        }
        if let Some(cg) = &gate_cg {
            cmd.env("TD_GATE_CG", cg.join("cgroup.procs"));
        }
        if !g.specs.is_empty() {
            cmd.env("TD_GATE_SPECS", g.specs.join(" "));
        }
        for (k, v) in &input_env {
            cmd.env(k, v);
        }
        for (k, v) in &g.extra_env {
            cmd.env(k, v);
        }
        // Own process group: the tree watchdog kills by pgid, and a gate's
        // children must never share the runner's group.
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                let _ = writeln!(logf, "gate-run: FAIL: gate {}: cannot spawn bash: {e}", g.name);
                timing_event(timing, &g.name, "END");
                return false;
            }
        };
        let pgid = child.id();
        let stop = std::sync::atomic::AtomicBool::new(false);
        let breached = std::sync::atomic::AtomicBool::new(false);
        let status = std::thread::scope(|ws| {
            if tree_mem_mib > 0 && gate_cg.is_none() {
                ws.spawn(|| {
                    let budget = tree_mem_mib.saturating_mul(1024 * 1024);
                    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                        if pgroup_rss_bytes(pgid) > budget {
                            breached.store(true, std::sync::atomic::Ordering::Relaxed);
                            let _ = crate::sys::kill_process_group(pgid, crate::sys::SIGKILL);
                            return;
                        }
                        std::thread::sleep(Duration::from_millis(500));
                    }
                });
            }
            let st = child.wait();
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
            st
        });
        if let Some(cg) = &gate_cg {
            if cgroup_oom_kills(cg) > 0 {
                breached.store(true, std::sync::atomic::Ordering::Relaxed);
                let _ = writeln!(
                    logf,
                    "gate-run: FAIL: gate {} — the kernel OOM-killed inside its cgroup \
                     ({tree_mem_mib} MiB memory.max, TD_CHECK_GATE_TREE_MEM_MIB)",
                    g.name
                );
            }
            // The group is dead or done; empty cgroups rmdir immediately, a
            // straggler zombie can delay it a moment.
            for _ in 0..10 {
                if std::fs::remove_dir(cg).is_ok() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(200));
            }
        }
        if breached.load(std::sync::atomic::Ordering::Relaxed) && gate_cg.is_none() {
            let _ = writeln!(
                logf,
                "gate-run: FAIL: gate {} — process-tree RSS exceeded the {tree_mem_mib} MiB \
                 budget (TD_CHECK_GATE_TREE_MEM_MIB); the whole process group was killed",
                g.name
            );
        }
        match status {
            Ok(st) if st.success() && !breached.load(std::sync::atomic::Ordering::Relaxed) => true,
            Ok(st) => {
                if !breached.load(std::sync::atomic::Ordering::Relaxed) {
                    let _ = writeln!(
                        logf,
                        "gate-run: FAIL: gate {} — body exited {}",
                        g.name,
                        st.code().unwrap_or(-1)
                    );
                }
                false
            }
            Err(e) => {
                let _ = writeln!(logf, "gate-run: FAIL: gate {}: wait failed: {e}", g.name);
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
fn print_gate_output(name: &str, log_path: &Path, ok: bool, non_blocking: bool, secs: f64) {
    let body = std::fs::read(log_path).unwrap_or_default();
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    let _ = lock.write_all(&body);
    let verdict = if ok {
        "PASS"
    } else if non_blocking {
        "FAIL (non-blocking — tolerated)"
    } else {
        "FAIL"
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
    /// The delegated per-run cgroup dir (TD_CHECK_CGROUP, issue #328). When
    /// present, each gate runs in its own child cgroup with memory.max set to
    /// the tree budget (kernel-enforced, escape-proof — a setsid() child stays
    /// in its cgroup) and the sampling watchdog below is NOT used. None =
    /// undelegated host = watchdog fallback.
    cgroup_dir: Option<PathBuf>,
    /// AGGREGATE tree budget per gate, in MiB (0 = off): a watchdog samples the
    /// gate's process group's summed RSS and SIGKILLs the whole group on breach
    /// — the layer the per-process rlimit below cannot provide (N children each
    /// under the per-process cap can collectively exceed the box; human review
    /// re #319). KNOWN GAP: a setsid() escapee leaves the process group and the
    /// sampler's sight — the cgroup v2 layer is the escape-proof successor once
    /// the host delegates a subtree.
    gate_tree_mem_mib: u64,
    /// Per-PROCESS RLIMIT_DATA cap for gate bodies, in MiB (0 = off). Applied
    /// via util-linux `prlimit` from the provisioned toolchain: with the pool
    /// over-provisioned past nproc (#319), one runaway allocator must die by
    /// its own limit — a clean red gate — instead of triggering the box
    /// OOM-killer. Per-process, so a make -jN tree of modest compilers passes.
    gate_mem_mib: u64,
    /// The warm chain-brick cache exported to Shared gates (#317): the ambient
    /// TD_CHECK_CHAIN_CACHE if the caller set one (empty = the operator's force-cold
    /// switch), else `~/.td/build-daemon/chain`. None (no HOME) leaves the gate
    /// env untouched. Private gates ALWAYS get TD_CHECK_CHAIN_CACHE force-cleared.
    chain_cache: Option<String>,
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
            // A dep is satisfied when it is Done OR SoftFailed (a tolerated
            // non-blocking failure must not wedge its dependents as Pending).
            let ready = deps.iter().all(|d| {
                !s.st.contains_key(d)
                    || matches!(s.st.get(d), Some(St::Done) | Some(St::SoftFailed))
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
                let ok = run_gate(
                    g,
                    &cfg.root,
                    &log_path,
                    cfg.timing_log.as_deref(),
                    &cfg.goal_words,
                    cfg.chain_cache.as_deref(),
                    cfg.gate_mem_mib,
                    cfg.gate_tree_mem_mib,
                    cfg.cgroup_dir.as_deref(),
                );
                print_gate_output(
                    &g.name,
                    &log_path,
                    ok,
                    g.non_blocking,
                    started.elapsed().as_secs_f64(),
                );
                if ok {
                    if let Some(key) = &cfg.tree_key {
                        journal_pass(&cfg.root, key, &g.name);
                    }
                }
                let mut s = lock_sched(&sched);
                // A non-blocking gate that fails SoftFails: it satisfies dependents
                // and does not red the run, and — crucially — does not set `fail`,
                // so it never triggers the fail-fast that would stop other gates.
                let new_st = if ok {
                    St::Done
                } else if g.non_blocking {
                    St::SoftFailed
                } else {
                    St::Failed
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
    // Green iff every gate ended Done, or SoftFailed AND not the explicit goal
    // (issue #377 — a SoftFailed gate that IS the goal must red the run; one
    // that's merely along for the ride stays tolerated) — no hard Failed,
    // none left Pending/Running by fail-fast.
    let green = s.st.iter().all(|(i, st)| match st {
        St::Done => true,
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
    line("daily ", Pool::Daily);
    line("fast  ", Pool::Fast);
    line("system", Pool::System);
    line("engine", Pool::Engine);
    line("parked", Pool::Parked);
    // Declared artifact inputs (#353): the inspectable per-gate dependency
    // graph the shell wiring used to bury (one line per gate that declares any).
    for g in &set.gates {
        if g.inputs.is_empty() {
            continue;
        }
        let decls: Vec<String> = g
            .inputs
            .iter()
            .map(|i| {
                let d = match &i.kind {
                    InputKind::LockEntry { lock, stem } => format!("lock-entry({lock}#{stem})"),
                    InputKind::ClosureMember { lock, root_stem, member_stem } => {
                        format!("closure-member({lock}#{root_stem} -> {member_stem})")
                    }
                };
                format!("{}={d}", crate::gate_inputs::env_var(i.name))
            })
            .collect();
        println!("inputs {}: {}", g.name, decls.join(" "));
    }
}

/// Re-print the newest run's per-gate table (the former Makefile
/// gate-timing-report target, native since #318 axis 2). Best-effort.
fn run_timing_report(root: &Path, heavy_gates: &[String]) {
    crate::gate_timing::report(root, heavy_gates);
}

/// The long-running gates the timing table classifies as heavy (heavy + daily)
/// — ONE list so the report goal and the green-run epilogue cannot drift.
fn long_gate_names(set: &GateSet) -> Vec<String> {
    let mut v = set.names(Pool::Heavy);
    v.extend(set.names(Pool::Daily));
    v
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
    // lists what to skip — bare gate NAMES and/or `pool:<cheap|heavy|daily|fast|
    // system|engine|parked>` tokens (comma/space separated). gate-run drops the named
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

    // Scope the synthetic build-recipes phase to the SELECTED gates' specs (the
    // per-PR budget, human 2026-07-04): pre-building the whole 18-package corpus
    // to run one gate was the old behavior; the phase now builds exactly the
    // specs the selected gates declare. The full `check` (and an explicit
    // `build-recipes` goal) keeps the whole pool — the daily backstop is
    // byte-identical to before.
    scope_build_recipes(&mut set, &selected, &goals);

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
            "gate-run: no `prlimit` on PATH — running WITHOUT the per-gate memory backstop (TD_CHECK_GATE_MEM_MIB={gate_mem_mib} requested)"
        );
        gate_mem_mib = 0;
    }
    let tree_key = std::env::var("TD_CHECK_TREE").ok().filter(|k| !k.is_empty());
    if resume && tree_key.is_none() {
        eprintln!(
            "gate-run: --resume needs the TD_CHECK_TREE key (td-builder check computes it from git); refusing to guess — running everything"
        );
        resume = false;
    }
    // TD_CHECK_GATE_TREE_MEM_MIB: aggregate per-gate process-tree budget
    // (default 16 GiB; 0 off). Enforced by the runner itself via /proc — no
    // prlimit, no cgroups needed.
    let gate_tree_mem_mib: u64 = std::env::var("TD_CHECK_GATE_TREE_MEM_MIB")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(16384);
    // The warm-cache home for Shared gates (#317): ambient TD_CHECK_CHAIN_CACHE wins
    // (set-but-empty = the operator's force-cold switch), else the machine-wide
    // default under ~/.td/build-daemon (bound into every check sandbox).
    let chain_cache = match std::env::var("TD_CHECK_CHAIN_CACHE") {
        Ok(v) => Some(v),
        Err(_) => std::env::var("HOME").ok().map(|h| format!("{h}/.td/build-daemon/chain")),
    };
    let cfg = RunCfg {
        root: root.clone(),
        jobs,
        pool: slot_pool_from_env(),
        timing_log,
        log_dir: std::env::temp_dir().join(format!("td-gate-run-{}", std::process::id())),
        tree_key,
        resume,
        gate_mem_mib,
        gate_tree_mem_mib,
        chain_cache,
        goal_words: goals.join(" "),
        explicit_goals: explicit_goal_indices(&set, &goals),
        cgroup_dir: std::env::var("TD_CHECK_CGROUP")
            .ok()
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .filter(|p| p.is_dir()),
    };
    match run_selected(&set, &selected, &cfg) {
        Ok(true) => {
            // Parity with the old check/check-system targets: print the per-gate
            // timing table on a green full run (best-effort).
            if goals.iter().any(|g| g == "check") {
                run_timing_report(&root, &long_gate_names(&set));
            } else if goals.iter().any(|g| g == "check-pr") {
                run_timing_report(&root, &set.names(Pool::Heavy));
            } else if goals.iter().any(|g| g == "check-system") {
                run_timing_report(&root, &set.names(Pool::System));
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
        // The heavy/daily split (the ~10-min per-PR budget, human 2026-07-04):
        // heavy = PR-sized behavioral gates, daily = the slow from-seed rungs +
        // from-source corpus the daily backstop covers. Together they are the
        // full check — the split may move members but never lose one.
        let heavy = set.names(Pool::Heavy);
        let daily = set.names(Pool::Daily);
        assert!(heavy.len() >= 45, "heavy (PR) pool shrank: {}", heavy.len());
        assert!(daily.len() >= 40, "daily pool shrank: {}", daily.len());
        assert!(heavy.len() + daily.len() >= 85, "the full check lost gates");
        for g in ["bootstrap", "td-subst", "cargo-test", "recipe-checks", "td-shell"] {
            assert!(heavy.iter().any(|n| n == g), "missing heavy gate {g}");
        }
        for g in ["bootstrap-gcc-mesboot", "recipe-checks-daily"] {
            assert!(daily.iter().any(|n| n == g), "missing daily gate {g}");
        }
        assert!(set.names(Pool::Engine).iter().any(|n| n == "cargo-test"));
        let system = set.names(Pool::System);
        for g in ["oci-native", "rust-userland-image"] {
            assert!(system.iter().any(|n| n == g), "missing system gate {g}");
        }
        // Fragment-declared specs feed the synthetic build-recipes node.
        for s in ["hello"] {
            assert!(set.build_specs.iter().any(|x| x == s), "missing build spec {s}");
        }
        // Typed artifact inputs (#353): the first cut-over gate declares its
        // inputs instead of grepping locks in shell.
        let tsd = set.gates.iter().find(|g| g.name == "toolchain-subst-default").unwrap();
        assert_eq!(tsd.inputs.len(), 2, "toolchain-subst-default lost its declared inputs");
        assert!(tsd.inputs.iter().any(|i| i.name == "coreutils"));
        assert!(tsd.inputs.iter().any(|i| i.name == "bash-static"));
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

    #[test]
    fn check_pr_is_the_full_check_minus_the_daily_pool() {
        let set = load().unwrap();
        let pr = expand_goals(&set, &["check-pr".to_string()]).unwrap();
        let full = expand_goals(&set, &["check".to_string()]).unwrap();
        assert!(pr.is_subset(&full), "check-pr selected a gate the full check does not");
        for i in set.members(Pool::Daily) {
            assert!(!pr.contains(&i), "a daily-only gate leaked into check-pr");
            assert!(full.contains(&i), "the full check lost a daily gate");
        }
        for i in set.members(Pool::Cheap).into_iter().chain(set.members(Pool::Heavy)) {
            assert!(pr.contains(&i), "check-pr lost a cheap/heavy gate");
        }
        let bi = *set.index.get(BUILD_RECIPES).unwrap();
        assert!(pr.contains(&bi) && full.contains(&bi), "build-recipes left a tier");
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
        // A single spec-carrying gate scopes the phase to its own spec.
        let mut set = load().unwrap();
        let goals = vec!["recipe-checks".to_string()];
        let sel = expand_goals(&set, &goals).unwrap();
        scope_build_recipes(&mut set, &sel, &goals);
        assert_eq!(br_specs(&set), "hello");
        // The full check keeps the whole pool, byte-identical to the static env.
        let mut set = load().unwrap();
        let all = set.build_specs.join(" ");
        let goals = vec!["check".to_string()];
        let sel = expand_goals(&set, &goals).unwrap();
        scope_build_recipes(&mut set, &sel, &goals);
        assert_eq!(br_specs(&set), all);
        // A spec-less selection (a store-DB gate builds its subject in-gate)
        // scopes the pre-build to nothing, but the BODY still runs — it is the
        // build-gate prelude (stage0 seed + td-recipe-eval; load_recipe_eval
        // fails-fast without its sentinel), so it must never be no-op'd.
        let mut set = load().unwrap();
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
                inputs: Vec::new(),
                store: StoreMode::Shared,
                non_blocking: false,
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
            pool: SlotPool { dir: sdir, n, min_free_gib: 0.0, psi_limit: 0.0, pace_ms: 0 },
            timing_log: None,
            log_dir: dir.join("logs"),
            tree_key: None,
            resume: false,
            gate_mem_mib: 0,
            gate_tree_mem_mib: 0,
            chain_cache: None,
            goal_words: String::new(),
            explicit_goals: HashSet::new(),
            cgroup_dir: None,
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
    fn non_blocking_gate_failure_is_tolerated_and_does_not_fail_fast() {
        // A `non_blocking` gate that FAILS must not red the run and must not stop
        // other gates (the guix-pin gates, on a drifted host).
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
    fn input_names_colliding_on_the_mapped_env_var_are_rejected() {
        // env_var folds case and maps -/./+ to _, so the dedup must compare the
        // MAPPED names — raw-name comparison would let these shadow each other.
        const K: InputKind = InputKind::LockEntry { lock: "x.lock", stem: "bash" };
        let a = ArtifactInput { name: "bash-static", kind: K };
        let b = ArtifactInput { name: "bash.static", kind: K };
        let err = validate_input_decls("g", &[a, b]).unwrap_err();
        assert!(err.contains("collide on the same TD_GATE_INPUT_BASH_STATIC"), "got: {err}");
        // distinct mapped names pass; an invalid name is rejected.
        let c = ArtifactInput { name: "coreutils", kind: K };
        assert!(validate_input_decls("g", &[a, c]).is_ok());
        let bad = ArtifactInput { name: "no/slash", kind: K };
        assert!(validate_input_decls("g", &[bad]).is_err());
    }

    #[test]
    fn a_declared_input_is_resolved_and_exported_to_the_body() {
        // Drive the REAL runner path: the gate's body sees the resolved path in
        // TD_GATE_INPUT_<NAME> and asserts it equals the lock's entry.
        let d = tmpdir("inputs-env");
        const H: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        std::fs::write(d.join("t.lock"), format!("{H}-make-4.4.1 /gnu/store/{H}-make-4.4.1\n"))
            .unwrap();
        let mut set = synth(
            &d,
            &[(
                "uses-input",
                Pool::Cheap,
                "test \"$TD_GATE_INPUT_MAKE\" = \"/gnu/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-make-4.4.1\" && touch {D}/env.ok",
                &[],
            )],
        );
        let gi = *set.index.get("uses-input").unwrap();
        set.gates.get_mut(gi).unwrap().inputs = vec![ArtifactInput {
            name: "make",
            kind: InputKind::LockEntry { lock: "t.lock", stem: "make" },
        }];
        let sel = expand_goals(&set, &["check-fast".to_string()]).unwrap();
        assert!(run_selected(&set, &sel, &cfg(&d, 2, None)).unwrap());
        assert!(d.join("env.ok").exists(), "the body did not see the resolved input");
    }

    #[test]
    fn an_unresolvable_input_reds_the_gate_without_running_its_body() {
        // The verified-red half of #353: a misdeclared input (a stem the lock
        // does not carry) fails the gate BEFORE the body starts.
        let d = tmpdir("inputs-red");
        const H: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        std::fs::write(d.join("t.lock"), format!("{H}-make-4.4.1 /gnu/store/{H}-make-4.4.1\n"))
            .unwrap();
        let mut set = synth(&d, &[("bad-input", Pool::Cheap, "touch {D}/ran", &[])]);
        let gi = *set.index.get("bad-input").unwrap();
        set.gates.get_mut(gi).unwrap().inputs = vec![ArtifactInput {
            name: "gawk",
            kind: InputKind::LockEntry { lock: "t.lock", stem: "gawk" },
        }];
        let sel = expand_goals(&set, &["check-fast".to_string()]).unwrap();
        assert!(!run_selected(&set, &sel, &cfg(&d, 2, None)).unwrap());
        assert!(!d.join("ran").exists(), "the body must not run on an unresolvable input");
        // The reason lands in the gate's log, not just a generic exit.
        let log = std::fs::read_to_string(d.join("logs/bad-input.log")).unwrap();
        assert!(log.contains("cannot resolve declared input `gawk`"), "log: {log}");
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
                ("sysgate", Pool::System, "true", &[]),
                // in a KEPT pool, but transitively needs a dropped heavy gate:
                ("needs_heavy", Pool::Cheap, "true", &["heavy_a"]),
            ],
        );
        // selected = the whole set (as if expand_goals closed over it).
        let selected: HashSet<usize> = (0..set.gates.len()).collect();

        // Drive the real entry point: a spec mixing a pool token, a bare name, and
        // a bogus token — commas AND spaces as separators.
        let (kept, unknown) =
            filter_disabled(&set, &selected, "pool:heavy, sysgate  bogus-name");
        let names: HashSet<&str> = kept
            .iter()
            .filter_map(|i| set.gates.get(*i).map(|g| g.name.as_str()))
            .collect();

        // `pool:heavy` drops both heavy gates; `sysgate` drops the named system gate.
        assert!(!names.contains("heavy_a"));
        assert!(!names.contains("heavy_b"));
        assert!(!names.contains("sysgate"));
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
        let pool = SlotPool { dir: Some(slots.clone()), n: 2, min_free_gib: 1e9, psi_limit: 0.0, pace_ms: 0 };
        assert!(matches!(pool.acquire(&|| true), Grant::Aborted));
        // Same reserve, but nothing else held: the no-deadlock rule admits.
        // Poll briefly: another test thread's Command::spawn may have forked
        // while `holder` was open — the child inherits the flock'd fd until
        // its exec's CLOEXEC closes it, keeping the lock alive a few ms past
        // drop(). Production tolerates the same window as one poll cycle.
        drop(holder);
        let mut admitted = false;
        for _ in 0..40 {
            if matches!(pool.acquire(&|| true), Grant::Held(_)) {
                admitted = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(admitted, "no-deadlock rule must admit once the inherited fd clears");
        // Reserve disabled: always admits.
        let pool = SlotPool { dir: Some(slots), n: 2, min_free_gib: 0.0, psi_limit: 0.0, pace_ms: 0 };
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
    fn cgroup_mode_enforces_the_tree_budget_when_delegated() {
        // Runs ONLY where a writable delegated cgroup subtree exists (systemd
        // user session, or the documented root-side /sys/fs/cgroup/td setup);
        // everywhere else the watchdog fallback tests carry the budget
        // property. The check_loop probe is replicated inline.
        let probe = |c: &Path| -> bool {
            let p = c.join(format!("td-probe-{}", std::process::id()));
            std::fs::create_dir(&p).map(|_| { let _ = std::fs::remove_dir(&p); true }).unwrap_or(false)
        };
        let root = [PathBuf::from("/sys/fs/cgroup/td")]
            .into_iter()
            .chain(
                std::fs::read_to_string("/proc/self/cgroup")
                    .ok()
                    .and_then(|t| t.lines().find_map(|l| l.strip_prefix("0::").map(|p| PathBuf::from(format!("/sys/fs/cgroup{}", p.trim()))))),
            )
            .find(|c| probe(c));
        let Some(root) = root else { return };
        let _ = std::fs::write(root.join("cgroup.subtree_control"), "+memory");
        let run = root.join(format!("td-test-{}", std::process::id()));
        std::fs::create_dir(&run).unwrap();
        if std::fs::write(run.join("cgroup.subtree_control"), "+memory").is_err() {
            let _ = std::fs::remove_dir(&run);
            return; // memory controller not delegatable here
        }
        let d = tmpdir("cg");
        let hog = r#"x=$(head -c 134217728 /dev/zero | tr '\0' a); sleep 1; echo ${#x}"#;
        let set = synth(&d, &[("cghog", Pool::Heavy, hog, &[])]);
        let sel = expand_goals(&set, &["cghog".to_string()]).unwrap();
        // FIRST HOP: self-move into a host leaf so the gate bodies' own moves
        // are within-delegation (mirrors cgroup_run_dir; without it the enter
        // write is EPERM and the red half would be red for the WRONG reason —
        // which is exactly how the first live run of this test caught the
        // common-ancestor rule).
        let host = run.join("host");
        std::fs::create_dir(&host).unwrap();
        if std::fs::write(host.join("cgroup.procs"), std::process::id().to_string()).is_err() {
            let _ = std::fs::remove_dir(&host);
            let _ = std::fs::remove_dir(&run);
            return; // delegation without the first-hop grant — see check_loop
        }
        let mut c = cfg(&d, 2, None);
        c.cgroup_dir = Some(run.clone());
        c.gate_tree_mem_mib = 32; // 128 MiB allocation vs a 32 MiB cgroup cap
        assert!(!run_selected(&set, &sel, &c).unwrap(), "cgroup memory.max must red the hog");
        // The RIGHT red: the kernel OOM inside the cgroup, named in the log —
        // never the enter-failure exit 97 (that once masked an EPERM as red).
        let log = std::fs::read_to_string(d.join("logs/cghog.log")).unwrap_or_default();
        assert!(log.contains("OOM-killed inside its cgroup"), "red must be the cgroup OOM, got: {log}");
        let mut c2 = cfg(&d, 2, None);
        c2.cgroup_dir = Some(run.clone());
        c2.gate_tree_mem_mib = 1024;
        assert!(run_selected(&set, &sel, &c2).unwrap(), "roomy cgroup cap must pass");
        let _ = std::fs::remove_dir(&host);
        let _ = std::fs::remove_dir(&run);
    }

    #[test]
    fn psi_parser_reads_some_avg10() {
        let sample = "some avg10=3.25 avg60=1.00 avg300=0.10 total=1\nfull avg10=0.00 avg60=0.00 avg300=0.00 total=0\n";
        assert_eq!(parse_psi_some_avg10(sample), Some(3.25));
        assert_eq!(parse_psi_some_avg10("garbage"), None);
    }

    #[test]
    fn pace_gate_defers_within_the_interval_and_admits_after() {
        let d = tmpdir("pace");
        let pool =
            SlotPool { dir: Some(d.clone()), n: 1, min_free_gib: 0.0, psi_limit: 0.0, pace_ms: 200 };
        assert!(pool.pace_grant(&d), "first grant is free");
        assert!(!pool.pace_grant(&d), "second grant inside the interval defers");
        std::thread::sleep(Duration::from_millis(220));
        assert!(pool.pace_grant(&d), "grant admits after the interval");
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
            inputs: Vec::new(),
            store: StoreMode::Shared,
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

    #[test]
    fn store_modes_are_audited() {
        let set = load().unwrap();
        let mut private: Vec<&str> = set
            .gates
            .iter()
            .filter(|g| g.store == StoreMode::Private)
            .map(|g| g.name.as_str())
            .collect();
        private.sort_unstable();
        // The DELIBERATE cold-store audit (#317): exactly the gates whose feature is
        // clean-slate behavior. Tagging a new gate Private means extending this list in
        // the same PR — a conscious act, like the one-way guix-surface ratchets.
        assert_eq!(
            private,
            vec![
                "bootstrap-seed",
                "build-hermetic",
                "chain-cache",
                "corpus-seed",
                "harness-seed",
                "rust-seed",
                "sandbox-hardening",
                "seed-build",
                "seed-unpack",
                "store-gc",
                "store-gc-sweep",
                "td-offline",
                "td-shell-seed",
            ]
        );
        // The default is Shared — the #317 flip: warm machine-wide state unless a gate
        // declares that cold IS its feature.
        for g in ["store-persist", "bootstrap", "recipe-checks"] {
            let gate = set.gates.iter().find(|x| x.name == g).unwrap();
            assert_eq!(gate.store, StoreMode::Shared, "{g} must default Shared");
        }
    }

    /// Through the REAL scheduler + bash execution — the #317 poison differential:
    /// with a warm cache configured and a poison marker seeded in it, the Shared gate
    /// sees exactly that cache (and can read the marker), while the Private gate gets
    /// TD_CHECK_CHAIN_CACHE force-cleared to SET-AND-EMPTY (not unset — the consuming libs
    /// default an unset var to the warm home, so only an explicit "" is cold).
    #[test]
    fn store_mode_wires_the_chain_cache_env() {
        let d = tmpdir("storemode");
        let cache = d.join("cache");
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::write(cache.join("poison"), b"poisoned artifact").unwrap();
        let mut set = synth(
            &d,
            &[
                (
                    "wshared",
                    Pool::Heavy,
                    r#"test "${TD_CHECK_CHAIN_CACHE:-}" = "{D}/cache" && test -f "$TD_CHECK_CHAIN_CACHE/poison" && touch {D}/shared.ok"#,
                    &[],
                ),
                (
                    "wprivate",
                    Pool::Heavy,
                    r#"test "${TD_CHECK_CHAIN_CACHE+set}" = set && test -z "$TD_CHECK_CHAIN_CACHE" && touch {D}/private.ok"#,
                    &[],
                ),
            ],
        );
        if let Some(i) = set.index.get("wprivate").copied() {
            if let Some(g) = set.gates.get_mut(i) {
                g.store = StoreMode::Private;
            }
        }
        let sel = expand_goals(&set, &["check".to_string()]).unwrap();
        let mut c = cfg(&d, 2, None);
        c.chain_cache = Some(cache.display().to_string());
        assert!(run_selected(&set, &sel, &c).unwrap());
        assert!(d.join("shared.ok").exists(), "shared gate did not see the configured warm cache");
        assert!(
            d.join("private.ok").exists(),
            "private gate's TD_CHECK_CHAIN_CACHE was not force-cleared to set-and-empty"
        );
    }
}

//! affected-checks — `td-builder affected-checks` (rust-migration C1).
//!
//! Maps a branch's changed paths to a right-sized check set and decides whether the
//! full `./check.sh` is waived or required — the local PR-readiness gate (CLAUDE.md
//! §"Diff-sized local check and waiver"). This is the cutover of
//! `tools/affected-checks.sh`: the 1284-line shell dispatcher is DELETED and callers
//! invoke this subcommand directly.
//!
//! Proven equivalent before the shell was removed (directive 4 — own, then diverge):
//! the development PR diffed this port's `--path` output byte-for-byte against the
//! live shell over 180+ paths. With the shell retired, the durable guards that
//! remain are `run_self_test` ported to native Rust `#[test]`s (the dynamic mapping,
//! over the real `gate_defs`/`tests` tree) + `renders_exact_output_for_static_paths`
//! (frozen full-render byte-equality) — both run every PR via the required
//! `cargo-test` job / `check-engine` smoke.
//!
//! Surfaces preserved exactly: `--run`, `--committed-only`, `--base REF`,
//! `--path FILE`, `--self-test`, `--help`. The mapping `case` arms are mirrored
//! IN ORDER (first match wins); the renderer reproduces the shell stdout
//! byte-for-byte.
//!
//! The shell rooted itself with `cd "$(dirname "$0")/.."`; the subcommand resolves
//! the repo root via `git rev-parse --show-toplevel` (falling back to CWD outside a
//! git repo), so it is CWD-robust. The library functions take an explicit `root` so
//! tests are CWD-independent.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::todo,
    clippy::unimplemented,
    clippy::indexing_slicing
)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

// ---------------------------------------------------------------------------
// shell `case` glob matcher: `*` matches any run INCLUDING `/` (case-glob, not
// filename-glob), `?` one char, `[...]` a class; `|` separates alternatives.
// ---------------------------------------------------------------------------

fn class_match(pat: &[u8], start: usize, ch: u8) -> Option<(bool, usize)> {
    let mut i = start + 1;
    let mut negate = false;
    if i < pat.len() && (pat[i] == b'!' || pat[i] == b'^') {
        negate = true;
        i += 1;
    }
    let mut matched = false;
    let mut first = true;
    while i < pat.len() {
        if pat[i] == b']' && !first {
            return Some((matched ^ negate, i + 1));
        }
        first = false;
        if i + 2 < pat.len() && pat[i + 1] == b'-' && pat[i + 2] != b']' {
            let (lo, hi) = (pat[i], pat[i + 2]);
            if ch >= lo && ch <= hi {
                matched = true;
            }
            i += 3;
        } else {
            if pat[i] == ch {
                matched = true;
            }
            i += 1;
        }
    }
    None // no closing `]` ⇒ caller treats `[` as a literal
}

fn glob_match(pat: &str, s: &str) -> bool {
    let pat = pat.as_bytes();
    let s = s.as_bytes();
    let mut p = 0usize;
    let mut c = 0usize;
    let mut star: Option<(usize, usize)> = None; // (pat idx after '*', s idx consumed)
    while c < s.len() {
        if p < pat.len() {
            match pat[p] {
                b'*' => {
                    p += 1;
                    star = Some((p, c));
                    continue;
                }
                b'?' => {
                    p += 1;
                    c += 1;
                    continue;
                }
                b'[' => {
                    if let Some((ok, np)) = class_match(pat, p, s[c]) {
                        if ok {
                            p = np;
                            c += 1;
                            continue;
                        }
                    } else if s[c] == b'[' {
                        p += 1;
                        c += 1;
                        continue;
                    }
                }
                ch => {
                    if ch == s[c] {
                        p += 1;
                        c += 1;
                        continue;
                    }
                }
            }
        }
        match star {
            Some((sp, sc)) => {
                p = sp;
                c = sc + 1;
                star = Some((sp, sc + 1));
            }
            None => return false,
        }
    }
    while p < pat.len() && pat[p] == b'*' {
        p += 1;
    }
    p == pat.len()
}

/// A `case` arm pattern: `|`-separated alternatives, any of which may match.
fn pattern_matches(alts: &str, s: &str) -> bool {
    alts.split('|').any(|a| glob_match(a, s))
}

// ---------------------------------------------------------------------------
// Selection accumulator — insertion-ordered dedup (the shell `contains_word`).
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Selection {
    preflights: Vec<String>,
    targets: Vec<String>,
    notes: Vec<String>,
}

fn push_unique(v: &mut Vec<String>, x: &str) {
    if !v.iter().any(|e| e == x) {
        v.push(x.to_string());
    }
}

impl Selection {
    fn add_preflight(&mut self, x: &str) {
        push_unique(&mut self.preflights, x);
    }
    fn add_target(&mut self, x: &str) {
        push_unique(&mut self.targets, x);
    }
    fn add_note(&mut self, x: &str) {
        push_unique(&mut self.notes, x);
    }
}

// ---------------------------------------------------------------------------
// Gate registry access. The gates are compiled Rust (src/gate_defs/*.rs, the
// build.rs registry) — the old mk/gates sed extractors became direct registry
// reads: this binary IS the runner, so the diff mapping and the scheduler read
// the SAME table by construction.
// ---------------------------------------------------------------------------

/// Sorted absolute paths of `builder/src/gate_defs/*.rs` (one file per gate —
/// the paths the diff mapping routes on).
fn gate_files(root: &Path) -> Result<Vec<PathBuf>, String> {
    let dir = root.join("builder/src/gate_defs");
    // Errors are returned, not flattened away: the caller asserts every gate
    // file maps, so a listing that silently came back short is that assertion
    // passing over the files it did not see.
    let rd = std::fs::read_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let mut v: Vec<PathBuf> = Vec::new();
    for e in rd {
        let p = e.map_err(|err| format!("{}: {err}", dir.display()))?.path();
        // Mirrors `builder/build.rs`'s skip: a dropping like `.005-x.rs` is a
        // `.rs` to `extension()` and is not a gate to the generator, so keeping
        // it here would red the self-test over a file nothing registers.
        let dotted = p
            .file_name()
            .is_some_and(|n| n.as_encoded_bytes().first() == Some(&b'.'));
        if p.extension().is_some_and(|x| x == "rs") && !dotted {
            v.push(p);
        }
    }
    v.sort();
    Ok(v)
}

/// The registry def whose file stem (`<NNN>-<gate>`) matches.
fn def_for_stem(stem: &str) -> Option<crate::gates::GateDef> {
    crate::gates::defs()
        .into_iter()
        .find(|(s, _)| *s == stem)
        .map(|(_, d)| d)
}

fn stem_of(file: &Path) -> Option<String> {
    file.file_stem()
        .and_then(|s| s.to_str())
        .map(str::to_string)
}

/// The gate target a def file maps to. Engine-only gates return None (the
/// check-engine smoke covers them — parity with the old extractor, which
/// scanned CHEAP/HEAVY/FAST/SYSTEM/PARKED and intentionally not ENGINE).
/// PARKED gates stay mapped: a parked gate (a human unhooked it pending a
/// tracked fix) remains an on-demand `./check.sh <gate>` target.
fn target_from_gate_file(file: &Path) -> Option<String> {
    let stem = stem_of(file)?;
    let def = def_for_stem(&stem)?;
    let mapped = def
        .pools
        .iter()
        .any(|p| !matches!(p, crate::gates::Pool::Engine));
    if mapped {
        Some(def.name.to_string())
    } else {
        None
    }
}

fn build_gates(_root: &Path) -> Vec<String> {
    crate::gates::defs()
        .into_iter()
        .filter(|(_, d)| d.build_gate)
        .map(|(_, d)| d.name.to_string())
        .collect()
}

/// Would a plain `td-builder check` (cheap+heavy gates + build-recipes) cover
/// `target`?
/// The pool question is gates.rs's (`pool_in_full_check`), not a local list.
fn default_check_covers_target(_root: &Path, target: &str) -> bool {
    if target == "check-fast" || target == "check" || target == "build-recipes" {
        return true;
    }
    crate::gates::defs().into_iter().any(|(_, d)| {
        d.name == target && d.pools.iter().any(|p| crate::gates::pool_in_full_check(*p))
    })
}

// ---------------------------------------------------------------------------
// Mapping helpers.
// ---------------------------------------------------------------------------

fn add_gate_file_targets(sel: &mut Selection, gate: &str) {
    sel.add_target(gate);
    // Gate defs are compiled Rust in the engine crate now: the AGENTS.md deny
    // lints are enforced by cargo clippy/test, so a gate-file edit runs the
    // engine smoke too — the old .mk fragments carried no Rust and needed none.
    sel.add_preflight("cargo-test");
    sel.add_target("check-engine");
}

fn add_build_gate_targets(root: &Path, sel: &mut Selection) {
    sel.add_target("build-recipes");
    for g in build_gates(root) {
        sel.add_target(&g);
    }
}

/// The engine sources a TARGET binary compiles in through a `#[path]` include,
/// and the full consumer list each one's note should name. Everything else
/// under `engine/src/` reaches only the two control-plane bins.
///
/// This is a DECLARATION, not the thing that adds a gate: `recipe-checks` is a
/// build gate, so `add_build_gate_targets` already selects it for every engine
/// source, target-included or not. What the table buys is that the set is
/// written down once — named in the router's note, pinned per entry by a test,
/// and re-asserted below independently of `recipe-checks` staying a build gate.
///
/// An entry whose consumer has not landed yet costs NOTHING, which follows from
/// the paragraph above rather than being a separate claim: `add_target` dedups
/// and `recipe-checks` is already selected for every engine source, so a table
/// path and a non-table path render the same target SET — only the order
/// differs. A `#[path]` include whose source is MISSING here is the direction
/// that matters.
///
/// Being in this table does NOT mean a gate builds that consumer. Nothing
/// builds td-net from source (recorded in aa347e60), so a change to
/// ed25519.rs/sha512.rs still runs its ring differential nowhere; naming the
/// consumer here at least makes that gap visible at the routing decision.
const TARGET_INCLUDED_ENGINE_SOURCES: &[(&str, &str)] = &[
    (
        "engine/src/sha256.rs",
        "td-builder, td-recipe-eval, target-static td-boot, and the td-compositor terminal corpus verifier/importer",
    ),
    (
        "engine/src/crc32.rs",
        "td-builder's xz and gzip decoders, and target-static td-install (through gpt.rs)",
    ),
    (
        "engine/src/gpt.rs",
        "target-static td-install; neither control-plane bin uses it",
    ),
    (
        "engine/src/fat.rs",
        "target-static td-install; neither control-plane bin uses it",
    ),
    (
        "engine/src/ed25519.rs",
        "target-static td-boot and td-net's cfg(test) ring differential; neither control-plane bin uses it",
    ),
    (
        "engine/src/sha512.rs",
        "target-static td-boot (paired with ed25519.rs, which reaches its hash as crate::sha512) and td-net's cfg(test) ring differential; neither control-plane bin uses it",
    ),
];

/// The consumer list for a target-included engine source, or `None` for an
/// engine source that reaches only the control-plane bins.
fn target_included_consumers(p: &str) -> Option<&'static str> {
    TARGET_INCLUDED_ENGINE_SOURCES
        .iter()
        .find_map(|&(src, consumers)| (src == p).then_some(consumers))
}

fn add_recipe_graph_targets(sel: &mut Selection) {
    sel.add_target("bootstrap-x86_64-toolchain-store-native");
    sel.add_target("bootstrap-x86_64-native-gcc-store-native");
    sel.add_target("bootstrap-x86_64-self-gcc-store-native");
    sel.add_target("recipe-checks");
}

// The 25 per-rung `bootstrap-<rung>.sh` gates that used to prove each i686
// mesboot→store-native rung individually are retired (#397): their `build_*`
// shell ladders were 80-95% duplicate of the recipe graph runner. There are now
// two live consumers of the i686 base graph's pinned inputs: recipe-owned
// checks, and the x86_64 gates whose cross/native/self rungs sit on top of the
// same stage0→mes→tcc→…→gcc-14→binutils-244→glibc-241 graph. The old
// "downstream slice" concept (CHAIN + add_chain, a 28-entry dependency array
// sliced per arm) is gone — every pinned input routes to that consumer set.
// What's accepted as lost: per-rung double-build reproducibility, the
// `store-ns` sandboxed no-guix round-trip, and the `subst-export`/`nar-restore`
// round-trip that some of the deep store-native rungs' scripts also checked —
// nothing ports those checks elsewhere. The remaining recipe-owned checks
// are the live coverage.
fn add_chain_targets(sel: &mut Selection) {
    add_recipe_graph_targets(sel);
}

// ---------------------------------------------------------------------------
// map_path — the `case` ladder, arm-for-arm with the shell (first match wins).
// ---------------------------------------------------------------------------

fn map_path(root: &Path, p: &str, sel: &mut Selection) {
    // Ignored local metadata. `target/*` is the shared workspace build dir
    // (builder/recipes/engine); `*` crosses `/`, so this covers target/release/…
    if pattern_matches(".claude/*|.td-build-cache/*|target/*", p) {
        return;
    }

    if pattern_matches(
        "check.sh|builder/build.rs|builder/src/gates.rs|builder/src/check_loop.rs",
        p,
    ) {
        // The loop spine used to escalate to the FULL loop; it now validates on
        // the whole behavioral tier (which exercises the runner/prelude end to
        // end over every gate) + the engine unit tests (the ~10-min per-PR budget, human
        // 2026-07-04).
        sel.add_preflight("shell-syntax");
        sel.add_preflight("cargo-test");
        // `check` already contains the cargo-test GATE (Pool::Heavy) — no
        // explicit target, or spine diffs would run the engine suite twice.
        sel.add_target("check");
        sel.add_note(&format!(
            "{p} touches the loop spine: validated by the full check (the runner runs \
             itself over every gate)."
        ));
        return;
    }

    if glob_match("builder/src/gate_defs/*.rs", p) {
        sel.add_preflight("shell-syntax");
        sel.add_preflight("affected-self-test");
        let abs = root.join(p);
        if abs.is_file() {
            match target_from_gate_file(&abs) {
                Some(gate) if !gate.is_empty() => add_gate_file_targets(sel, &gate),
                _ => {
                    sel.add_target("check");
                    sel.add_note(&format!(
                        "{p} does not register a gate target — running the full check."
                    ));
                }
            }
        } else {
            sel.add_target("check");
            sel.add_note(&format!(
                "{p} was deleted; affected-checks cannot infer the removed gate target — \
                 running the full check (a stale reference to the gate reds there)."
            ));
        }
        return;
    }

    // Tombstones for the shell the native store bodies replaced (#318 axis 3):
    // the DELETING diff still routes to the gates that absorbed the logic
    // (a deleted path has no file to introspect, so map it explicitly).
    if p == "tests/store-subject.sh" {
        for g in [
            "store-register",
            "store-gc",
            "store-verify",
            "store-gc-sweep",
            "store-add-referenced",
            "store-backend",
        ] {
            sel.add_target(g);
        }
        return;
    }
    if p == "tests/store-ns.sh" {
        sel.add_target("store-ns");
        return;
    }

    // Native (typed-Rust) gate BODIES (#318 axis 3): a body change must run the
    // native gates it implements (the former tests/store-*.sh / gate script
    // mapping), plus the engine smoke for the shared helpers.
    if p == "builder/src/gate_bodies.rs" {
        sel.add_preflight("cargo-test");
        sel.add_target("check-engine");
        for g in [
            "store-add",
            "store-add-tree",
            "store-register",
            "store-gc",
            "store-gc-sweep",
            "store-add-referenced",
            "store-verify",
            "store-backend",
            "store-ns",
            "recipe-rs",
            "recipe-checks",
            "store-native-profile",
            "sandbox-hardening",
            "toolchain-input-addressed",
            "toolchain-x86_64-input-addressed",
        ] {
            sel.add_target(g);
        }
        return;
    }

    if HOST_ONLY_ENGINE_SOURCES.contains(&p) {
        // Host-side branch tooling that lives in the engine crate but is not
        // the engine: reachable only from its own subcommand, and reading
        // nothing a recipe, a store operation or a sandbox touches. The
        // from-source rungs therefore cannot observe a change here, so the
        // ~2-min smoke (which still compiles it into td-builder) is the whole
        // behavioural signal and the full check is not owed.
        sel.add_preflight("cargo-test");
        sel.add_target("check-engine");
        sel.add_note(&format!(
            "{p} is host-side branch tooling, not the build engine: the check-engine smoke covers it and the from-source rungs cannot reach it (they never invoke its subcommand)."
        ));
        return;
    }

    if pattern_matches("builder/Cargo.toml|builder/src/*", p) {
        // The ~2-min check-engine SMOKE tier (cargo-test: compile + unit tests) is
        // the FAST signal, but the engine is what the from-source rungs exercise, so
        // it also takes the whole behavioral tier. It used to stop at the smoke tier
        // because the daily backstop supplied the from-source coverage eventually;
        // with that gone, stopping here would leave the engine's own deep coverage
        // with no runner at all.
        sel.add_preflight("cargo-test");
        sel.add_target("check-engine");
        sel.add_target("check");
        sel.add_note(&format!(
            "{p} is the td-builder build engine: the ~2-min check-engine smoke (compile + unit tests) is the fast signal; the from-source build coverage is the full check (DESIGN §7.2)."
        ));
        return;
    }

    // The shared std-only engine lib (JSON + SHA-256) AND the workspace-root
    // manifest/lock. The engine compiles INTO both td-builder (build engine ->
    // check-engine) and td-recipe-eval (recipe surface -> recipe-rs + the package
    // build gates), and the root Cargo.toml/Cargo.lock govern how both bins build
    // (release profile, member set, dependency graph). So route the UNION of the
    // builder and recipes rules — a conservative superset. builder/Cargo.lock and
    // recipes/Cargo.lock are TOMBSTONES: the per-crate locks folded into the one
    // workspace-root Cargo.lock, so a diff deleting them routes to the same check.
    if pattern_matches(
        "engine/Cargo.toml|engine/src/*|Cargo.toml|Cargo.lock|builder/Cargo.lock|recipes/Cargo.lock",
        p,
    ) {
        sel.add_preflight("shell-syntax");
        sel.add_preflight("cargo-test");
        sel.add_target("check-engine");
        sel.add_target("recipe-rs");
        let consumers = if let Some(consumers) = target_included_consumers(p) {
            // Redundant TODAY — `add_build_gate_targets` below adds recipe-checks
            // for every engine source, because it is a build gate. Stated anyway:
            // a target-included source needs the gate that BUILDS the target
            // crate, and that requirement should not rest on recipe-checks
            // happening to stay in the build-gate set.
            sel.add_target("recipe-checks");
            consumers
        } else {
            "td-builder and td-recipe-eval"
        };
        add_build_gate_targets(root, sel);
        sel.add_note(&format!(
            "{p} is shared engine/workspace code compiled into {consumers}: validated by check-engine (compile + unit tests) and the recipe/package build gates."
        ));
        return;
    }

    if p == "recipes/src/source_pins.rs" {
        sel.add_preflight("shell-syntax");
        sel.add_preflight("cargo-test");
        sel.add_target("recipe-rs");
        sel.add_target("bootstrap-seed");
        sel.add_target("bootstrap-mes");
        add_build_gate_targets(root, sel);
        add_chain_targets(sel);
        return;
    }

    // Tombstones for the deleted external source-pin side table. These paths
    // exist only in the branch diff that removes them; the live owner is
    // recipes/src/source_pins.rs.
    if pattern_matches("seed/sources/*.lock", p) {
        sel.add_preflight("shell-syntax");
        sel.add_target("recipe-rs");
        sel.add_target("bootstrap-seed");
        sel.add_target("bootstrap-mes");
        add_build_gate_targets(root, sel);
        add_chain_targets(sel);
        return;
    }

    // Tombstone for the deleted recipe emit wrapper. The live path invokes
    // td-recipe-eval directly; this path exists only in branch diffs that
    // remove the legacy shell wrapper.
    if p == "tests/recipe-emit.sh" {
        sel.add_preflight("shell-syntax");
        return;
    }

    // seed/seed-digests.txt — the compiled seed-digest table (re #469),
    // include_str!-compiled into BOTH planners (td-recipe-eval's
    // seed_digests.rs and td-builder's auto_seed_provenance). The
    // digest-coverage unit tests in both crates are the direct check
    // (cargo-test); a row change shifts what the planners ADMIT, so the
    // recipe self-consistency and package build gates run too.
    // A row edit is checkable directly for the LOCAL sources (their bytes are in the
    // checkout); the fetched pins' rows still need a warm-cache `seed-digests` run.
    if p == "seed/seed-digests.txt" {
        sel.add_preflight("cargo-test");
        sel.add_preflight("local-source-digests");
        sel.add_target("recipe-rs");
        add_build_gate_targets(root, sel);
        return;
    }

    if pattern_matches(
        "recipes/*|recipes/src/*|recipes/Cargo.toml|tests/recipe-eval-tool.sh",
        p,
    ) {
        // The td-recipe crate IS the package + system-spec surface (boa/TS retired).
        // It feeds the corpus build path (cache-lib emits via td-recipe-eval) — so a
        // catalog change can affect ANY built package. Run recipe-rs (self-consistency
        // + manifest sync) and the package build gates. (spec-diff retired with the
        // museum tier; the guix-dependence census retired with the guix-oracle gates.)
        // The cargo-test preflight carries the crate's unit tests + clippy while the
        // in-loop gates are unprovisionable (re #469).
        sel.add_preflight("shell-syntax");
        sel.add_preflight("cargo-test");
        // A catalog edit can add, retarget, or drop a `local_source`, any of which
        // changes which trees the table must pin.
        sel.add_preflight("local-source-digests");
        sel.add_target("recipe-rs");
        if glob_match("recipes/src/recipes/*.rs", p) {
            sel.add_target("recipe-checks");
        }
        add_build_gate_targets(root, sel);
        return;
    }

    if p == "tests/recipe-checks.sh" {
        sel.add_target("recipe-checks");
        return;
    }

    if pattern_matches("net/*|net/src/*|net/Cargo.toml|net/Cargo.lock", p) {
        // The merged td-net (fetch/feed/subst). It holds the host-PREP warm that feeds the
        // recipe-graph consumers (`warm sources` + `warm kernel-headers`) → the chain targets
        // (former feed coverage); AND, since the old fetch/* rule mapped to the broad
        // behavioral tier, a net-only change keeps that too — without it such a diff
        // would run nothing while waiving the full check. The union of BOTH former
        // rules. No gate builds td-net from source;
        // the warm compiles it.
        sel.add_target("check");
        add_chain_targets(sel);
        return;
    }

    if p == "tests/td-feed.index" {
        // The shared feed index is a pinned manifest for the source/crate bytes
        // consumed by recipe graph warmers.
        add_chain_targets(sel);
        return;
    }

    // Tombstone (#460): the shell body became the native gate_bodies::toolchain_input_addressed;
    // the deleting diff still routes to the gate that absorbed the logic.
    if p == "tests/toolchain-input-addressed.sh" {
        sel.add_target("toolchain-input-addressed");
        return;
    }

    if p == "tests/td-toolchain-x86_64.lock" {
        sel.add_preflight("shell-syntax");
        sel.add_target("toolchain-x86_64-input-addressed");
        sel.add_target("bootstrap-x86_64-toolchain-store-native");
        return;
    }

    // #410: the tests/td-toolchain-rust-x86_64.lock mapping was removed with the rust-toolchain
    // recipe-graph cutover — that gate-assembled lock and its consumer gate (416) are retired.
    // Tombstone (#460): the shell body became gate_bodies::toolchain_x86_64_input_addressed. The
    // gate def (418-*.rs) is handled by the generic gate_defs/*.rs arm above; the deleted shell
    // still routes to the gate that absorbed the logic.
    if p == "tests/toolchain-x86_64-input-addressed.sh" {
        sel.add_target("toolchain-x86_64-input-addressed");
        return;
    }

    if p == "tests/td-toolchain.lock" {
        sel.add_preflight("shell-syntax");
        sel.add_target("toolchain-input-addressed");
        sel.add_target("toolchain-x86_64-input-addressed");
        return;
    }

    // tests/build-recipes.sh IS the build phase (the former Makefile build-recipes
    // recipe, run by the gate runner) — a change to it affects every build gate,
    // exactly like the build-phase helpers below. (tests/stage0-builder.sh is a
    // tombstone: the placement logic became builder/src/stage0.rs — `td-builder
    // stage0-place`, re #469; the deleting diff still routes to the build gates
    // that consume the placement.)
    if pattern_matches(
        "tests/build-recipes.sh|tests/cache-lib.sh|tests/stage0-builder.sh",
        p,
    ) {
        sel.add_preflight("shell-syntax");
        add_build_gate_targets(root, sel);
        return;
    }

    // Tombstone (#460): the shell body became the native gate_bodies::sandbox_hardening;
    // the deleting diff still routes to the gate that absorbed the logic.
    if p == "tests/sandbox-hardening.sh" {
        sel.add_target("sandbox-hardening");
        return;
    }

    // bootstrap-seed / bootstrap-mes have NO shell driver — they are STRUCTURED Rust
    // recipes (`td-builder bootstrap-recipe {seed,mes}`, builder/src/bootstrap.rs,
    // rust-migration C2; the old tests/bootstrap-{seed,mes}.sh were deleted). Source
    // pin edits now live in recipes/src/source_pins.rs and route through the recipe
    // crate arm above, which selects the recipe engine plus all build gates.

    // Tombstones for the deleted shell recipe-graph compatibility helpers. The
    // deleting diff maps to the current Rust recipe-graph consumers.
    if pattern_matches(
        "tests/bootstrap-chain.sh|tests/ladder-lib.sh|tests/chain-cache.sh|tests/chain-cache-lib.sh|tests/repro-lib.sh",
        p,
    ) {
        add_chain_targets(sel);
        return;
    }

    // --- the i686 mesboot→store-native recipe graph's vendored patches (#397) ---
    // Source pin changes are recipe edits now; patch byte changes still route to
    // the live graph consumers: recipe-owned checks and the x86_64 gates.
    if pattern_matches("seed/patches/binutils-boot-*.patch|seed/patches/gcc-boot-2.95.3.patch|seed/patches/glibc-boot-2.2.5.patch|seed/patches/glibc-bootstrap-system-2.2.5.patch|seed/patches/gcc-boot-4.6.4.patch|seed/patches/glibc-boot-2.16.0.patch|seed/patches/glibc-bootstrap-system-2.16.0.patch", p)
    {
        sel.add_preflight("shell-syntax");
        add_chain_targets(sel);
        return;
    }
    // Tombstones for the retired x86_64 shell drivers/libs. The live orchestration is
    // recipe-owned; these deleting diffs still route to the gates that delegate
    // into td-recipe-eval check-run.
    if pattern_matches("tests/bootstrap-x86_64-native-gcc-store-native.sh", p) {
        sel.add_target("bootstrap-x86_64-native-gcc-store-native");
        return;
    }
    if pattern_matches("tests/bootstrap-x86_64-self-gcc-store-native.sh", p) {
        sel.add_target("bootstrap-x86_64-self-gcc-store-native");
        return;
    }
    // The NATIVE x86_64 toolchain's input-addressed key file: consumed by the native gcc gate (422,
    // builds+interns the native toolchain at these lock paths) and the self-host gate (426, obtains the
    // native toolchain as its builder).
    // (A recipe-rev bump here re-keys the path.) The rust runtime gate (416) that also fetched this as
    // the linker was retired with the rust-toolchain recipe-graph cutover (#410).
    if pattern_matches("tests/td-toolchain-x86_64-native.lock", p) {
        sel.add_target("bootstrap-x86_64-native-gcc-store-native");
        sel.add_target("bootstrap-x86_64-self-gcc-store-native");
        return;
    }
    if pattern_matches(
        "tests/bootstrap-x86_64-toolchain-store-native.sh|tests/x86_64-cross-fns.sh|tests/x86_64-subst-lib.sh|builder/src/gate_defs/414-bootstrap-x86_64-toolchain-store-native.rs",
        p,
    ) {
        sel.add_target("bootstrap-x86_64-toolchain-store-native");
        // The old shared libs also defined the rung-X2 native driver, fetch-or-build
        // obtainers, and rung-X3 self-host helpers; their deletion still routes to
        // all three x86_64 gates.
        sel.add_target("bootstrap-x86_64-native-gcc-store-native");
        sel.add_target("bootstrap-x86_64-self-gcc-store-native");
        return;
    }

    if glob_match("seed/stage0/*", p) {
        sel.add_preflight("shell-syntax");
        sel.add_target("bootstrap-seed");
        add_chain_targets(sel);
        return;
    }

    // Tombstone (#460): the shell body became the native gate_bodies::store_native_profile;
    // the deleting diff still routes to the gate that absorbed the logic.
    if p == "tests/store-native-profile.sh" {
        sel.add_target("store-native-profile");
        return;
    }

    if p == "tests/heal-revert.sh" {
        // The heal primitive's behavioral test — git is absent from the loop
        // sandbox, so it is not a ./check.sh gate; the dev host runs it directly.
        sel.add_preflight("shell-syntax");
        sel.add_preflight("heal-revert");
        return;
    }

    // Tombstones for the deleted SEED-build resolver behavioral tests: the
    // resolvers (tools/provision-{rust,cc}.sh + tools/bootstrap-td-builder.sh)
    // were ported into builder/src/stage0.rs (`td-builder provision-{rust,cc}`
    // / `stage0-place`, re #469), unit-tested there via cargo-test; the shell
    // tests' gates had already retired with the guix-invoking gates. These
    // paths exist only in deleting diffs. (The deleted tools/*.sh route
    // through the tools arm below.)
    if pattern_matches("tests/provision-rust.sh|tests/provision-cc.sh", p) {
        sel.add_preflight("cargo-test");
        return;
    }
    if p == "ci/revert-suspect.sh" {
        // Editing the heal primitive runs its behavioral test (the dev host has git).
        sel.add_preflight("shell-syntax");
        sel.add_preflight("heal-revert");
        return;
    }
    if pattern_matches("ci/*.sh|tools/*.sh", p) {
        sel.add_preflight("shell-syntax");
        return;
    }

    if p == "start" {
        sel.add_preflight("shell-syntax");
        return;
    }

    if pattern_matches("*.md|DESIGN.md|CLAUDE.md|.gitignore", p) {
        return; // docs — no checks
    }

    // td-boot's protocol.rs is the deployment contract, `#[path]`-included by
    // three OTHER trees: two recipes (system-x86-64, qemu_boot) and — since the
    // manifest header and size bound moved into it — the host-side td-net, whose
    // `td-deploy` signs what td-boot verifies. The td-boot rule below covers the
    // first two through recipe-checks but compiles no net, and no gate builds
    // td-net from source: the WARM does, which is a chain target. So this file
    // takes the td-boot rule plus those, or a change here that breaks only the
    // signer runs nothing.
    if p == "td-boot/src/protocol.rs" {
        sel.add_preflight("cargo-test");
        sel.add_target("check");
        sel.add_target("recipe-checks");
        add_chain_targets(sel);
        return;
    }

    // The target guest crates compile offline in the cargo-test preflight. Their
    // sources are embedded into target-static recipes, whose link/behavior tests
    // live in the recipe-owned checks.
    if pattern_matches(
        "td-kexec/*|td-kexec/src/*|td-kexec/Cargo.toml|td-kexec/Cargo.lock|td-boot/*|td-boot/src/*|td-boot/Cargo.toml|td-boot/Cargo.lock",
        p,
    ) {
        sel.add_preflight("cargo-test");
        sel.add_target("check");
        sel.add_target("recipe-checks");
        return;
    }

    // td-netd: the target-built static network bring-up daemon — SAME shape as
    // td-kexec. A standalone dependency-free pure-std crate OUTSIDE the engine, so
    // route it to the cargo-test preflight (host-native clippy/test). Its src/main.rs
    // is `include_str!`'d verbatim into the td-netd RECIPE AND packed into
    // system-x86-64, so a helper-source edit changes the TARGET artifact and a
    // static-link regression is invisible to host cargo; also route to
    // recipe-checks (which statically links + shape-asserts it via
    // td-netd-test). Its RECIPE files under recipes/src/recipes/ are routed by the
    // recipes arm above, not here.
    if pattern_matches("td-netd/*|td-netd/src/*|td-netd/Cargo.toml|td-netd/Cargo.lock", p) {
        sel.add_preflight("cargo-test");
        sel.add_target("check");
        sel.add_target("recipe-checks");
        return;
    }

    // td-sh: the target-built POSIX shell, a standalone std-only crate OUTSIDE the
    // engine workspace — same routing as td-txt below, and for the same reason. Its
    // lib.rs conformance harness + unit tests lint/test on the host cargo-test
    // preflight. src/main.rs is `include_str!`'d into the td-sh RECIPE, so a
    // source edit changes the TARGET artifact and a static-link regression is
    // invisible to host cargo — so also route to recipe-checks, which statically
    // links it and runs `-c 'exit 0'` via td-sh-test.
    //
    // What recipe-checks does NOT cover: td-sh is packed into system-x86-64 and IS
    // the boot path since the flip — both `/init`s and every generated /etc script
    // are interpreted by it, and td-login execs it by absolute path — but
    // `system-x86-64` owns no gated check (it is absent from `check-list`), so its
    // `shape_check` probes of the packed shell and the boot oracle behind
    // `qemu-boot-system` run only in a full image build, which no gate runs. The
    // host-side recipe TESTS are what stand in, and they are cargo-test's.
    //
    // Its RECIPE files under recipes/src/recipes/ are routed by the recipes arm
    // above, not here. The spec/ corpus and tests/ carry no standalone shell
    // scripts, so no shell-syntax.
    if pattern_matches(
        "td-sh/*|td-sh/src/*|td-sh/tests/*|td-sh/spec/*|td-sh/Cargo.toml|td-sh/Cargo.lock",
        p,
    ) {
        sel.add_preflight("cargo-test");
        sel.add_target("check");
        sel.add_target("recipe-checks");
        return;
    }

    // td-txt: the target-built static TEXT multicall (grep/sed), a standalone
    // std-only crate OUTSIDE the engine workspace — same routing as td-sh. Its
    // lib.rs conformance harness, its unit tests, and the corpus run (the vendored
    // GNU grep/sed suites in spec/) all ride the host cargo-test preflight.
    // src/main.rs and its modules are `include_str!`'d into the td-txt
    // RECIPE, so a source edit changes the TARGET artifact and a static-link
    // regression is invisible to host cargo — so also route to recipe-checks
    // (recipe-checks statically links + smoke-runs it via td-txt-test) AND it is
    // packed into system-x86-64, serving /bin/grep and /bin/sed. That farm is on the
    // BOOT PATH — /etc/rootcheck greps /proc/mounts with it — so recipe-checks
    // is also what runs the qemu boot oracle that waits for TD_TXT_RUNTIME_MARKER. Its
    // RECIPE files under recipes/src/recipes/ are routed by the recipes arm above,
    // not here. spec/ holds corpus DATA (no standalone shell scripts), so no
    // shell-syntax preflight.
    if pattern_matches(
        "td-txt/*|td-txt/src/*|td-txt/tests/*|td-txt/examples/*|td-txt/spec/*|td-txt/spec/gnu-grep/*|td-txt/spec/gnu-sed/*|td-txt/Cargo.toml|td-txt/Cargo.lock",
        p,
    ) {
        sel.add_preflight("cargo-test");
        sel.add_target("check");
        sel.add_target("recipe-checks");
        return;
    }

    // td-util: the target-built static diagnostics multicall (clear/which/free/ps/
    // dmesg), a standalone std-only crate OUTSIDE the engine workspace — same routing
    // as td-sh. Its unit tests lint/test on the host cargo-test preflight;
    // src/main.rs and its modules are `include_str!`'d into the td-util RECIPE, so a
    // source edit changes the TARGET artifact and a static-link regression is
    // invisible to host cargo — so also route to recipe-checks (recipe-checks
    // statically links + exercises it via td-util-test) AND it is packed into
    // system-x86-64, serving the /bin diagnostics farm. Its RECIPE files under
    // recipes/src/recipes/ are routed by the recipes arm above, not here.
    if pattern_matches(
        "td-util/*|td-util/src/*|td-util/Cargo.toml|td-util/Cargo.lock",
        p,
    ) {
        sel.add_preflight("cargo-test");
        sel.add_target("check");
        sel.add_target("recipe-checks");
        return;
    }

    // td-firstboot: the target-built static per-machine identity provisioner
    // (machine-id, the SSH host key, a deny-all authorized_keys under /var/lib/td),
    // a standalone std-only crate OUTSIDE the engine workspace — same routing as
    // td-util. Its unit tests (the /proc/mounts persistence decision, the machine-id
    // format, the argv) lint/test on the host cargo-test preflight;
    // src/main.rs and its modules are `include_str!`'d into the td-firstboot RECIPE,
    // so a source edit changes the TARGET artifact and a static-link regression is
    // invisible to host cargo — so also route to recipe-checks (recipe-checks
    // statically links + exercises provisioning twice via
    // td-firstboot-test) AND it is packed into system-x86-64 as the sysinit job that
    // fills the /var targets every MUTABLE_ETC symlink points at, so a source edit
    // here decides whether a booted machine has an identity at all. Its RECIPE files
    // under recipes/src/recipes/ are routed by the recipes arm above.
    if pattern_matches(
        "td-firstboot/*|td-firstboot/src/*|td-firstboot/Cargo.toml|td-firstboot/Cargo.lock",
        p,
    ) {
        sel.add_preflight("cargo-test");
        sel.add_target("check");
        sel.add_target("recipe-checks");
        return;
    }

    // tests/sshd: the td-owned SSH daemon SOURCE, built as the `sshd` local-source
    // recipe and shipped in system-x86-64. Unlike the crates above it does NOT ride
    // the cargo-test preflight: it is the one target crate with dependencies (the
    // vendored russh closure, incl. a C crate), so it cannot compile in the
    // dependency-free offline preflight. Its coverage is recipe-checks — the sshd
    // recipe builds it, and `qemu-boot-system` runs the daemon on the image (the
    // selftest marker, and `keygen` minting this machine's host identity for
    // td-firstboot). The full check still runs for the fast repo-wide guards.
    //
    // It DOES ride local-source-digests: these bytes ARE the `sshd-source` seed, so
    // editing them re-addresses it and seed/seed-digests.txt must be regenerated in
    // the same landing. Omitting that is not hypothetical — it is how the row went
    // stale, and nothing here caught it, because the key stayed present and every
    // warm ladder kept answering from the pre-edit tree.
    if pattern_matches(
        "tests/sshd/*|tests/sshd/src/*|tests/sshd/Cargo.toml|tests/sshd/Cargo.lock",
        p,
    ) {
        sel.add_preflight("local-source-digests");
        sel.add_target("check");
        sel.add_target("recipe-checks");
        return;
    }

    // The review-record fixture two crates share: `td-builder ready` gates on it
    // and td-review displays it, from separate implementations that cannot
    // depend on each other. Editing a case must run BOTH suites, which the one
    // cargo-test preflight does.
    if p == "tests/review-record.cases" {
        sel.add_preflight("cargo-test");
        return;
    }

    // td-review: the HOST-side integrator TUI. In neither bootstrap graph — no
    // recipe builds it and it never enters a closure — so unlike the crates
    // above there is no target artifact for recipe-checks to link.
    //
    // The only arm that selects NO check target. `cargo-test` is the one gate
    // whose body names td-review at all — clippy --all-targets, its tests, and
    // its 1-package lock — and the preflight above covers all three and then
    // some: it runs the tests with --include-ignored where the gate runs
    // --bins, and its lock guard also rejects a `source =` line. Superset, not
    // equal, and in the safe direction. `check` would add the cheap+heavy pools and
    // build-recipes on top — the source-bootstrap ladder from the stage0 seed,
    // an hour of mes/tcc/gcc that cannot read a host-side crate. Bounded means
    // bounded by what the diff can break; a selection nothing in it inspects is
    // latency, and latency is what gets a pre-push check skipped.
    if pattern_matches(
        "td-review/*|td-review/src/*|td-review/tests/*|td-review/Cargo.toml|td-review/Cargo.lock",
        p,
    ) {
        sel.add_preflight("cargo-test");
        return;
    }

    // td-init: the target-built static boot-glue multicall (init/reboot/poweroff/
    // halt/switch_root/cttyhack/hostname), a standalone std-only crate OUTSIDE the
    // engine workspace — same routing as td-util, which it complements. Its unit
    // tests AND its unsafe-confinement test (the scoped-allow count and the syscall
    // roster, which the compiler cannot check) run on the host cargo-test
    // preflight; src/main.rs and its modules are `include_str!`'d into the td-init
    // RECIPE, so a source edit changes the TARGET artifact and a static-link
    // regression is invisible to host cargo — so also route to recipe-checks
    // (recipe-checks statically links + exercises it via td-init-test) AND it is
    // packed into system-x86-64 as /init, the initramfs pivot, and the /bin boot-glue
    // farm, so a source edit here changes what PID 1 IS. Its RECIPE files under
    // recipes/src/recipes/ are routed by the recipes arm above.
    if pattern_matches(
        "td-init/*|td-init/src/*|td-init/Cargo.toml|td-init/Cargo.lock",
        p,
    ) {
        sel.add_preflight("cargo-test");
        sel.add_target("check");
        sel.add_target("recipe-checks");
        return;
    }

    // td-login: the target-built static credential multicall (login/su), a standalone
    // std-only crate OUTSIDE the engine workspace — same routing as td-init, whose
    // unsafe-exception shape it shares. Its unit tests AND its confinement tests (the
    // scoped-allow count, the three-syscall roster, and the ORDER the credential
    // syscalls are issued in — none of which the compiler checks) run on the host
    // cargo-test cargo-test preflight; src/main.rs and its modules are `include_str!`'d
    // into the td-login RECIPE, so a source edit changes the TARGET artifact and a
    // static-link regression is invisible to host cargo — so also route to
    // recipe-checks (recipe-checks statically links + exercises it via
    // td-login-test) AND it is packed into system-x86-64 as the /bin/{login,su} farm,
    // so a source edit here changes how the machine hands out credentials. Its RECIPE
    // files under recipes/src/recipes/ are routed by the recipes arm above, and
    // THREAT-MODEL.md by the docs arm above that — the confinement tests read the
    // crate's SOURCE, so prose no gate can check reds nothing.
    if pattern_matches(
        "td-login/*|td-login/src/*|td-login/Cargo.toml|td-login/Cargo.lock",
        p,
    ) {
        sel.add_preflight("cargo-test");
        sel.add_target("check");
        sel.add_target("recipe-checks");
        return;
    }

    // td-svc: the target-built service supervisor, a standalone std-only crate
    // OUTSIDE the engine workspace — same routing as td-login/td-init, and the
    // fifth unsafe exception: it `#![deny(unsafe_code)]`s with ONE scoped allow on
    // src/sys.rs's `syscall2`, carrying `kill(2)` alone. DESIGN.md beside it is
    // the normative spec for that surface and for why every OTHER capability it
    // needs is reachable through safe std. Its unit tests run on the host
    // cargo-test preflight;
    // src/main.rs and its modules are `include_str!`'d into the td-svc RECIPE, so
    // a source edit changes the TARGET artifact and a static-link regression is
    // invisible to host cargo — hence recipe-checks too (which
    // statically links + exercises it via td-svc-test). Its RECIPE files under
    // recipes/src/recipes/ are routed by the recipes arm above, and DESIGN.md by
    // the docs arm — no gate can check prose, so prose alone reds nothing.
    if pattern_matches(
        "td-svc/*|td-svc/src/*|td-svc/Cargo.toml|td-svc/Cargo.lock",
        p,
    ) {
        sel.add_preflight("cargo-test");
        sel.add_target("check");
        sel.add_target("recipe-checks");
        return;
    }

    // td-seatd and td-compositor are the target-built single-user UI substrate.
    // Both are standalone dependency-free crates whose sources are embedded into
    // target-static recipes and packed into system-x86-64. Host cargo holds their
    // Rust rules and confinement tests; recipe-checks holds the static link and
    // boot integration. DESIGN.md remains documentation-only under the docs arm.
    if pattern_matches(
        "td-seatd/*|td-seatd/src/*|td-seatd/Cargo.toml|td-seatd/Cargo.lock|td-compositor/*|td-compositor/src/*|td-compositor/Cargo.toml|td-compositor/Cargo.lock",
        p,
    ) {
        sel.add_preflight("cargo-test");
        sel.add_target("check");
        sel.add_target("recipe-checks");
        return;
    }

    // Catch-all: an unmapped path used to require the FULL loop; it now runs
    // the whole behavioral tier — there is no narrower honest answer.
    sel.add_target("check");
    sel.add_note(&format!(
        "No mapping for {p} — running the full check. Update \
         builder/src/affected.rs with a mapping for it."
    ));
}

// ---------------------------------------------------------------------------
// Rendering — byte-for-byte with the shell's stdout.
// ---------------------------------------------------------------------------

fn preflight_cmd(name: &str, changed: &[String]) -> Option<String> {
    match name {
        "shell-syntax" => Some("  bash -n start tests/*.sh ci/*.sh tools/*.sh".to_string()),
        "heal-revert" => Some("  bash tests/heal-revert.sh".to_string()),
        // Rendered from the SAME list that runs, so a scoped run cannot print a
        // command it will not issue — the dry run is what a reader trusts.
        "cargo-test" => Some(render_cargo_test(&cargo_test_cmds(changed))),
        "affected-self-test" => Some("  td-builder affected-checks --self-test".to_string()),
        "local-source-digests" => Some("  td-recipe-eval local-source-digests".to_string()),
        _ => None,
    }
}

/// The one-line summary of a cargo-test command set: the historical wording,
/// with the manifests it will actually visit.
fn render_cargo_test(cmds: &[&str]) -> String {
    let workspace = cmds.iter().any(|c| c.contains("--workspace"));
    let mut o = String::from("  cargo test + clippy --frozen");
    if workspace {
        o.push_str(" --workspace (builder/recipes/engine)");
    }
    let mut seen: Vec<&str> = Vec::new();
    for c in cmds {
        if let Some(krate) = cmd_manifest_crate(c) {
            if !seen.contains(&krate) {
                seen.push(krate);
                if workspace || seen.len() > 1 {
                    o.push_str(" +");
                }
                o.push_str(" --manifest-path ");
                o.push_str(krate);
                o.push_str("/Cargo.toml");
            }
        }
    }
    // ANY, not all — as the full-table wording always was. The line is a
    // summary of a command set, not a per-crate transcript, and the same
    // approximation already covers `--all-targets`/`--bins` differing per
    // entry.
    if cmds.iter().any(|c| c.contains("--include-ignored")) {
        o.push_str(" -- --include-ignored");
    }
    o
}

struct Header<'a> {
    explicit: bool,
    base: &'a str,
    merge_base: &'a str,
}

/// Produce the full dry-run stdout (the text the shell prints before executing),
/// including the trailing "Dry run only" note when `run` is false.
fn format_output(header: &Header, changed: &[String], sel: &Selection, run: bool) -> String {
    let mut o = String::new();
    if header.explicit {
        o.push_str("affected-checks: explicit path mode\n");
    } else {
        o.push_str(&format!(
            "affected-checks: base={} merge-base={}\n",
            header.base, header.merge_base
        ));
    }
    o.push('\n');
    o.push_str("Changed paths:\n");
    for p in changed {
        o.push_str(&format!("  {p}\n"));
    }
    o.push('\n');

    if sel.preflights.is_empty() && sel.targets.is_empty() {
        o.push_str("Selected checks: none (docs-only or ignored local metadata)\n");
    } else {
        o.push_str("Selected checks:\n");
        for pre in &sel.preflights {
            if let Some(cmd) = preflight_cmd(pre, changed) {
                o.push_str(&cmd);
                o.push('\n');
            }
        }
        if !sel.targets.is_empty() {
            o.push_str(&format!("  {}\n", check_command(&sel.targets)));
        }
    }

    o.push('\n');
    if header.explicit {
        o.push_str("Waiver: inspection only (--path does not prove the branch diff)\n");
        // Nothing escalates to the full loop, so the branch-mode policy is always waived.
        o.push_str("Branch-mode policy for these paths: the full check would be waived\n");
    } else {
        o.push_str("Waiver: the full check waived by affected-checks for this diff\n");
    }

    if !sel.notes.is_empty() {
        o.push('\n');
        o.push_str("Notes:\n");
        for n in &sel.notes {
            o.push_str(&format!("  - {n}\n"));
        }
    }

    if !run {
        o.push('\n');
        o.push_str("Dry run only. Re-run with --run to execute.\n");
    }
    o
}

/// Whether this path routes to any check at all — the question `ready` asks of
/// an UNTRACKED file, which the committed-only checks do not see but a compile
/// does. Asking the real mapping beats guessing by extension.
pub(crate) fn selects_checks(root: &Path, path: &str) -> bool {
    let mut sel = Selection::default();
    map_path(root, path, &mut sel);
    // The catch-all arm adds `check` to EVERY unmapped path, so "selected
    // something" is true of any file at all — scratch notes included. The note
    // it leaves behind is what tells a mapped path from an unmapped one, and
    // only a mapped one has any business reddening a pre-push gate.
    !sel.notes.iter().any(|n| n.starts_with("No mapping for"))
        && (!sel.preflights.is_empty() || !sel.targets.is_empty())
}

fn compute_selection(root: &Path, changed: &[String]) -> Selection {
    let mut sel = Selection::default();
    for p in changed {
        if !p.is_empty() {
            map_path(root, p, &mut sel);
        }
    }
    sel
}

// ---------------------------------------------------------------------------
// Self-test — the shell `run_self_test`, ported to native assertions. Returns the
// list of failure messages (empty ⇒ pass). The durable guard (no Guix, no shell).
// ---------------------------------------------------------------------------

const HELP: &str = "\
Select a right-sized check set from the diff against main.

  td-builder affected-checks              # print selected checks
  td-builder affected-checks --run        # execute selected checks
  td-builder affected-checks --base main  # compare against another base
  td-builder affected-checks --path FILE  # inspect the mapping for FILE
  td-builder affected-checks --self-test  # verify the mapping table

This is the local PR-readiness gate for diffs it can classify. It maps changed
paths to focused gate targets and prints whether the full check is waived or
still required.
";

/// The dry-run render for `--path PATH` (explicit mode, run=0) — the exact text
/// the shell `$0 --path PATH` prints, used by the self-test and the differential.
fn path_output(root: &Path, path: &str) -> String {
    let mut changed: Vec<String> = vec![path.to_string()];
    changed.retain(|s| !s.is_empty());
    changed.sort();
    changed.dedup();
    let sel = compute_selection(root, &changed);
    let header = Header {
        explicit: true,
        base: "origin/main",
        merge_base: "",
    };
    format_output(&header, &changed, &sel, false)
}

/// The command that runs `targets`, printed VERBATIM as `--run` executes it.
/// `check <gate>` is not redundant even though the tier already selects the gate:
/// naming it makes it an EXPLICIT goal, and a `non_blocking` gate's failure reds
/// the run only when it is explicit (`gates::explicit_goal_indices`). Collapsing
/// the list would print a command that greens where `--run` reds.
fn check_command(targets: &[String]) -> String {
    format!("td-builder check {}", targets.join(" "))
}

fn last_check_targets(output: &str) -> Vec<String> {
    let mut line: Option<&str> = None;
    for l in output.lines() {
        if let Some(rest) = l.strip_prefix("  td-builder check ") {
            line = Some(rest);
        }
    }
    line.map(|l| l.split_whitespace().map(str::to_string).collect())
        .unwrap_or_default()
}

pub fn run_self_test(root: &Path) -> Vec<String> {
    let mut failures: Vec<String> = Vec::new();
    let mut fail = |m: String| failures.push(m);

    // The printed command IS the executed one, so one predicate covers both.
    let has_target = |path: &str, target: &str| -> bool {
        last_check_targets(&path_output(root, path))
            .iter()
            .any(|t| t == target)
    };
    let runs_target = has_target;
    // Preflights are not on the check-target lines at all, so a target
    // assertion cannot see them: read the selection directly.
    let selects_preflight = |path: &str, preflight: &str| -> bool {
        compute_selection(root, &[path.to_string()])
            .preflights
            .iter()
            .any(|p| p == preflight)
    };
    macro_rules! assert_target {
        ($path:expr, $target:expr) => {
            if !has_target($path, $target) {
                fail(format!("{}: expected check target '{}'", $path, $target));
            }
        };
    }
    macro_rules! assert_runs {
        ($path:expr, $target:expr) => {
            if !runs_target($path, $target) {
                fail(format!(
                    "{}: expected PER-PR (run) target '{}'",
                    $path, $target
                ));
            }
        };
    }
    macro_rules! assert_preflight {
        ($path:expr, $preflight:expr) => {
            if !selects_preflight($path, $preflight) {
                fail(format!("{}: expected preflight '{}'", $path, $preflight));
            }
        };
    }
    macro_rules! assert_no_target {
        ($path:expr, $target:expr) => {
            if has_target($path, $target) {
                fail(format!("{}: must NOT select target '{}'", $path, $target));
            }
        };
    }
    macro_rules! assert_contains {
        ($path:expr, $needle:expr) => {{
            let out = path_output(root, $path);
            if !out.contains($needle) {
                fail(format!("{}: missing '{}'", $path, $needle));
            }
        }};
    }
    macro_rules! assert_not_contains {
        ($path:expr, $needle:expr) => {{
            let out = path_output(root, $path);
            if out.contains($needle) {
                fail(format!("{}: must not mention '{}'", $path, $needle));
            }
        }};
    }
    macro_rules! assert_branch_policy {
        ($path:expr, $policy:expr) => {{
            let out = path_output(root, $path);
            let needle = format!("Branch-mode policy for these paths: {}", $policy);
            if !out.contains(&needle) {
                fail(format!("{}: missing '{}'", $path, needle));
            }
        }};
    }

    // --help honesty (the shell asserts the usage extractor stops at the first
    // non-comment line — trivially structural for the Rust static help string).
    if !HELP.contains("--self-test") {
        fail("--help: missing '--self-test'".into());
    }
    if HELP.contains("set -euo pipefail") {
        fail("--help: unexpectedly contains 'set -euo pipefail'".into());
    }
    if HELP.contains("cd \"$(dirname \"$0\")/..\"") {
        fail("--help: unexpectedly contains shell cd line".into());
    }

    // Default-coverage classifier.
    if !default_check_covers_target(root, "check-fast") {
        fail("default coverage: missing check-fast".into());
    }
    if !default_check_covers_target(root, "build-recipes") {
        fail("default coverage: missing build-recipes".into());
    }
    if !default_check_covers_target(root, "cargo-test") {
        fail("default coverage: missing cargo-test".into());
    }
    if !default_check_covers_target(root, "check") {
        fail("default coverage: missing check (the whole behavioral tier)".into());
    }
    // A negative case: the classifier must not answer "covered" for everything.
    // `check-engine` is a tier keyword the plain `check` does NOT expand to, and it
    // is not a gate name, so it must classify as NOT covered.
    if default_check_covers_target(root, "check-engine") {
        fail("default coverage: check-engine is not covered by the plain check".into());
    }
    if default_check_covers_target(root, "no-such-gate-xyz") {
        fail("default coverage: an unknown target must not classify as covered".into());
    }
    if !default_check_covers_target(root, "recipe-checks") {
        fail("default coverage: recipe-checks is covered by the plain check".into());
    }

    // Every gate file maps (via the builder/src/gate_defs/*.rs arm) to its own gate target.
    match gate_files(root) {
        Err(e) => fail(format!("gate defs unreadable: {e}")),
        // Zero files is the vacuous pass this whole loop would otherwise be.
        Ok(fs) if fs.is_empty() => fail("gate defs: none found to check".into()),
        Ok(fs) => {
            for f in fs {
                let Some(name) = f.file_name() else {
                    fail(format!("{}: no file name", f.display()));
                    continue;
                };
                let rel = format!("builder/src/gate_defs/{}", name.to_string_lossy());
                match target_from_gate_file(&f) {
                    Some(gate) if !gate.is_empty() => assert_target!(&rel, &gate),
                    _ => fail(format!("{rel}: no gate registration found")),
                }
            }
        }
    }

    // Every BUILD_GATE is selected by the build-phase arm (build-recipes is the
    // phase itself; cache-lib is its helper).
    for bg in build_gates(root) {
        assert_target!("tests/build-recipes.sh", &bg);
        assert_target!("tests/cache-lib.sh", &bg);
    }

    assert_target!("tests/recipe-checks.sh", "recipe-checks");
    for tombstone in [
        "tests/chain-cache-lib.sh",
        "tests/chain-cache.sh",
        "tests/bootstrap-chain.sh",
        "tests/ladder-lib.sh",
        "tests/repro-lib.sh",
    ] {
        assert_target!(tombstone, "recipe-checks");
        assert_target!(tombstone, "bootstrap-x86_64-toolchain-store-native");
        assert_target!(tombstone, "bootstrap-x86_64-native-gcc-store-native");
        assert_target!(tombstone, "bootstrap-x86_64-self-gcc-store-native");
    }

    // A gate-file change still selects the dispatcher's own self-test preflight
    // (now the in-process `td-builder affected-checks --self-test`) and is waived.
    assert_contains!(
        "builder/src/gate_defs/325-cargo-test.rs",
        "td-builder affected-checks --self-test"
    );
    assert_branch_policy!(
        "builder/src/gate_defs/325-cargo-test.rs",
        "the full check would be waived"
    );
    assert_branch_policy!("tests/repro-lib.sh", "the full check would be waived");
    // Native (typed-Rust) gate bodies (#318 axis 3): a body change runs its gates
    // (the former tests/store-*.sh / gate-script mapping) + the engine smoke.
    assert_target!("builder/src/gate_bodies.rs", "store-register");
    assert_target!("builder/src/gate_bodies.rs", "store-ns");
    assert_target!("builder/src/gate_bodies.rs", "check-engine");
    assert_target!("builder/src/gate_bodies.rs", "recipe-rs");
    assert_target!("builder/src/gate_bodies.rs", "recipe-checks");
    // #460: the four former tests/*.sh gate bodies became native gate_bodies fns.
    assert_target!("builder/src/gate_bodies.rs", "store-native-profile");
    assert_target!("builder/src/gate_bodies.rs", "sandbox-hardening");
    assert_target!("builder/src/gate_bodies.rs", "toolchain-input-addressed");
    assert_target!("builder/src/gate_bodies.rs", "toolchain-x86_64-input-addressed");
    // Their deleted shell drivers are tombstoned to the gate that absorbed each.
    assert_target!("tests/store-native-profile.sh", "store-native-profile");
    assert_target!("tests/sandbox-hardening.sh", "sandbox-hardening");
    assert_target!("tests/toolchain-input-addressed.sh", "toolchain-input-addressed");
    // The Rust td-recipe crate IS the package + spec surface (boa/TS retired): a
    // catalog edit runs recipe-rs and the package build gates.
    assert_target!("recipes/src/catalog.rs", "recipe-rs");
    // Recipes are one self-registering file each under src/recipes/ (issue #295);
    // the nested path must select the same gate (glob `*` crosses `/`).
    assert_target!("recipes/src/recipes/make-test.rs", "recipe-rs");
    assert_target!("recipes/src/recipes/make-test.rs", "recipe-checks");
    assert_target!("recipes/build.rs", "recipe-rs");
    assert_target!("recipes/Cargo.toml", "recipe-rs");
    assert_target!("builder/src/gate_defs/207-recipe-rs.rs", "recipe-rs");
    // The td-builder build engine (its own src) rides the check-engine smoke.
    assert_target!("builder/src/main.rs", "check-engine");
    // The shared td-engine lib compiles INTO both bins, so an engine-source or
    // workspace-root manifest/lock change routes the UNION: check-engine (builder
    // side) AND recipe-rs + the package build gates (recipe side).
    assert_target!("engine/src/json.rs", "check-engine");
    assert_target!("engine/src/json.rs", "recipe-rs");
    // The premise the table rests on, pinned rather than asserted in a comment:
    // json.rs is target-included by NOTHING and still selects recipe-checks,
    // because it is a build gate. If that ever stops being true this line reds,
    // and the explicit selection beside the table stops being redundant.
    assert_target!("engine/src/json.rs", "recipe-checks");
    // One line per TARGET_INCLUDED_ENGINE_SOURCES entry, and deliberately NOT a
    // loop over that table: an assertion generated from the table compares the
    // code with itself and passes however the table is edited — which the first
    // draft of this did, and a verify-red probe caught. These needles are
    // written out by hand, so deleting an entry reds the line naming it.
    //
    // The NOTE is what they check, because it is the only rendered thing that
    // distinguishes a target-included source. `recipe-checks` is a build gate,
    // so the target assertions below hold for engine/src/json.rs too: they
    // guard that the gate is selected AT ALL, not that the table selected it.
    assert_target!("engine/src/sha256.rs", "check-engine");
    assert_target!("engine/src/sha256.rs", "recipe-checks");
    assert_contains!("engine/src/sha256.rs", "target-static td-boot");
    assert_contains!("engine/src/crc32.rs", "target-static td-install");
    assert_contains!("engine/src/gpt.rs", "target-static td-install");
    assert_contains!("engine/src/fat.rs", "target-static td-install");
    assert_contains!("engine/src/ed25519.rs", "target-static td-boot");
    assert_contains!("engine/src/sha512.rs", "target-static td-boot");
    // The other half. Without it a table that matched EVERY engine source would
    // satisfy every line above, and the note would claim a target consumer for
    // control-plane-only code.
    assert_not_contains!("engine/src/json.rs", "target-static");
    // And the membership itself, written out independently of the table so the
    // two must be edited together. The lines above only catch an entry REMOVED,
    // or a table grown to match everything; this also catches one added quietly,
    // which for a routing table is the direction that decides which gate a
    // target binary's sources get.
    let expected_target_included = [
        "engine/src/sha256.rs",
        "engine/src/crc32.rs",
        "engine/src/gpt.rs",
        "engine/src/fat.rs",
        "engine/src/ed25519.rs",
        "engine/src/sha512.rs",
    ];
    let actual_target_included: Vec<&str> = TARGET_INCLUDED_ENGINE_SOURCES
        .iter()
        .map(|(src, _)| *src)
        .collect();
    if actual_target_included != expected_target_included {
        fail(format!(
            "TARGET_INCLUDED_ENGINE_SOURCES membership changed: {actual_target_included:?}"
        ));
    }
    // And that each names a file that EXISTS. `map_path` never stats an engine
    // source, so a renamed or mistyped entry would quietly stop matching and
    // route its file as control-plane-only — the failure this table exists to
    // prevent, reached by typo. Comparing the table to the filesystem is not
    // the self-comparison the note assertions had to avoid.
    for (src, _) in TARGET_INCLUDED_ENGINE_SOURCES {
        if !root.join(src).is_file() {
            fail(format!("{src} is in TARGET_INCLUDED_ENGINE_SOURCES but does not exist"));
        }
    }
    assert_target!("engine/Cargo.toml", "check-engine");
    assert_target!("engine/Cargo.toml", "recipe-rs");
    assert_target!("Cargo.toml", "check-engine");
    assert_target!("Cargo.toml", "recipe-rs");
    assert_target!("Cargo.lock", "check-engine");
    assert_target!("Cargo.lock", "recipe-rs");
    // Tombstones: the per-crate locks folded into the root Cargo.lock; a diff
    // deleting them routes to the same workspace check (not the No-mapping fallback).
    assert_target!("builder/Cargo.lock", "check-engine");
    assert_target!("builder/Cargo.lock", "recipe-rs");
    assert_target!("recipes/Cargo.lock", "check-engine");
    assert_target!("recipes/Cargo.lock", "recipe-rs");
    // The merged td-net gets the union of the former fetch/feed rules: the chain targets
    // (no gate builds it from source; its main.rs holds the warm-sources consumer smoked by
    // the i686 chain's proof set) AND the whole behavioral tier (former fetch coverage).
    assert_target!("net/Cargo.lock", "recipe-checks");
    assert_target!("net/src/main.rs", "recipe-checks");
    assert_target!("net/src/fetch.rs", "check");
    assert_target!("net/Cargo.toml", "check");
    // td-kexec/src is include_str!'d into the target artifact, so a helper-source edit
    // rides the host cargo preflight AND is recorded against recipe-checks,
    // which statically links it via td-kexec-test.
    assert_target!("td-kexec/src/main.rs", "check");
    assert_target!("td-kexec/src/main.rs", "recipe-checks");
    // td-sh mirrors td-kexec: standalone std-only crate, main.rs include_str!'d into
    // its recipe, so host cargo preflight + the recipe-checks static-link proof.
    assert_target!("td-sh/src/main.rs", "check");
    assert_target!("td-sh/src/main.rs", "recipe-checks");
    assert_target!("td-sh/src/lib.rs", "check");
    assert_target!("td-sh/src/lib.rs", "recipe-checks");
    assert_target!("td-sh/spec/smoke.test.sh", "check");
    assert_target!("td-sh/spec/smoke.test.sh", "recipe-checks");

    // td-txt mirrors td-sh: standalone std-only crate, main.rs + modules
    // include_str!'d into the recipe, corpus DATA under spec/ (including the
    // vendored GNU suites in the two subdirectories).
    assert_target!("td-txt/src/main.rs", "check");
    assert_target!("td-txt/src/main.rs", "recipe-checks");
    assert_target!("td-txt/src/lib.rs", "check");
    assert_target!("td-txt/spec/grep-cli.test.txt", "check");
    assert_target!("td-txt/spec/gnu-sed/appquit.sed", "check");
    assert_target!("td-txt/spec/gnu-grep/bre.tests", "recipe-checks");
    assert_target!("td-util/src/main.rs", "check");
    assert_target!("td-util/src/main.rs", "recipe-checks");
    assert_target!("td-util/src/ps.rs", "check");
    assert_target!("td-util/src/ps.rs", "recipe-checks");
    assert_target!("td-util/src/less.rs", "check");
    assert_target!("td-util/src/less.rs", "recipe-checks");
    // The crate's one unsafe surface and the module that owns the struct offsets.
    assert_target!("td-util/src/sys.rs", "check");
    assert_target!("td-util/src/sys.rs", "recipe-checks");
    assert_target!("td-util/src/term.rs", "check");
    assert_target!("td-util/src/term.rs", "recipe-checks");
    assert_target!("td-util/src/cat.rs", "check");
    assert_target!("td-util/src/cat.rs", "recipe-checks");
    assert_target!("td-util/src/fileattr.rs", "check");
    assert_target!("td-util/src/fileattr.rs", "recipe-checks");
    assert_target!("td-util/src/fileops.rs", "check");
    assert_target!("td-util/src/fileops.rs", "recipe-checks");
    assert_target!("td-util/src/printf.rs", "check");
    assert_target!("td-util/src/printf.rs", "recipe-checks");
    assert_target!("td-util/src/sleep.rs", "check");
    assert_target!("td-util/src/sleep.rs", "recipe-checks");
    assert_target!("td-util/src/test.rs", "check");
    assert_target!("td-util/src/test.rs", "recipe-checks");
    assert_target!("td-util/Cargo.lock", "check");
    assert_target!("td-util/Cargo.lock", "recipe-checks");
    // td-firstboot mirrors td-util: every source is include_str!'d into the
    // td-firstboot recipe and packed into system-x86-64 as a sysinit job, so host
    // cargo cargo-test preflight + the recipe-checks static-link/provisioning proof.
    assert_target!("td-firstboot/src/main.rs", "check");
    assert_target!("td-firstboot/src/main.rs", "recipe-checks");
    assert_target!("td-firstboot/src/mounts.rs", "check");
    assert_target!("td-firstboot/src/mounts.rs", "recipe-checks");
    assert_target!("td-firstboot/src/machineid.rs", "check");
    assert_target!("td-firstboot/Cargo.lock", "check");
    assert_target!("td-firstboot/Cargo.lock", "recipe-checks");
    assert_target!("td-firstboot/clippy.toml", "check");
    // tests/sshd is the one target crate WITH dependencies, so it rides the
    // tier rather than the dependency-free cargo-test preflight.
    assert_target!("tests/sshd/src/main.rs", "check");
    assert_target!("tests/sshd/src/main.rs", "recipe-checks");
    assert_target!("tests/sshd/Cargo.lock", "recipe-checks");
    // Its bytes ARE the `sshd-source` seed, so any edit re-addresses it and the
    // seed-digest row must be regenerated in the same landing. The row going stale
    // unnoticed is what this mapping exists to prevent.
    assert_preflight!("tests/sshd/src/main.rs", "local-source-digests");
    assert_preflight!("tests/sshd/Cargo.toml", "local-source-digests");
    assert_preflight!("tests/sshd/Cargo.lock", "local-source-digests");
    assert_preflight!("seed/seed-digests.txt", "local-source-digests");
    assert_preflight!("recipes/src/recipes/sshd.rs", "local-source-digests");
    // td-init mirrors td-util, including its confined syscall module: every source
    // is include_str!'d into the td-init recipe, so host cargo preflight
    // + the recipe-checks static-link proof.
    assert_target!("td-init/src/main.rs", "check");
    assert_target!("td-init/src/main.rs", "recipe-checks");
    assert_target!("td-init/src/sys.rs", "check");
    assert_target!("td-init/src/sys.rs", "recipe-checks");
    assert_target!("td-init/src/losetup.rs", "check");
    assert_target!("td-init/src/losetup.rs", "recipe-checks");
    assert_target!("td-init/src/devt.rs", "check");
    assert_target!("td-init/src/devt.rs", "recipe-checks");
    assert_target!("td-init/src/mknod.rs", "check");
    assert_target!("td-init/src/mknod.rs", "recipe-checks");
    assert_target!("td-init/src/syncfs.rs", "check");
    assert_target!("td-init/src/syncfs.rs", "recipe-checks");
    assert_target!("td-init/Cargo.lock", "check");
    assert_target!("td-init/Cargo.lock", "recipe-checks");
    // td-login mirrors td-init, confined syscall module included: every source is
    // include_str!'d into the td-login recipe, so the host cargo preflight
    // and the recipe-checks static-link proof both apply. THREAT-MODEL.md is normative —
    // the confinement tests assert what it says — so it routes with the sources.
    assert_target!("td-login/src/main.rs", "check");
    assert_target!("td-login/src/main.rs", "recipe-checks");
    assert_target!("td-login/src/creds.rs", "check");
    assert_target!("td-login/src/creds.rs", "recipe-checks");
    assert_target!("td-login/src/sys.rs", "check");
    assert_target!("td-login/src/sys.rs", "recipe-checks");
    // ...but THREAT-MODEL.md routes as DOCS, like every other `*.md` in the repo:
    // the confinement tests read the crate's SOURCE, so prose that no gate can check
    // is prose that reds nothing. Editing it alone is a documentation change; editing
    // what it specifies routes through the sources above.
    assert_no_target!("td-login/THREAT-MODEL.md", "check");
    assert_target!("td-login/Cargo.lock", "check");
    assert_target!("td-login/Cargo.lock", "recipe-checks");
    assert_preflight!("td-login/src/creds.rs", "cargo-test");
    // td-svc mirrors td-login's routing, unsafe surface included: every source is
    // include_str!'d into the td-svc recipe, so the host cargo preflight
    // and the recipe-checks static-link proof both apply. DESIGN.md is normative prose
    // and routes as docs, like td-login's THREAT-MODEL.md.
    assert_target!("td-svc/src/main.rs", "check");
    assert_target!("td-svc/src/main.rs", "recipe-checks");
    assert_target!("td-svc/src/supervise.rs", "check");
    assert_target!("td-svc/src/supervise.rs", "recipe-checks");
    assert_target!("td-svc/src/procfs.rs", "check");
    assert_target!("td-svc/src/procfs.rs", "recipe-checks");
    // The syscall layer and its one caller-side module, named individually: these
    // are the crate's whole unsafe surface, and a routing change that stopped
    // covering them would stop running the confinement tests that hold it.
    assert_target!("td-svc/src/sys.rs", "check");
    assert_target!("td-svc/src/sys.rs", "recipe-checks");
    assert_preflight!("td-svc/src/sys.rs", "cargo-test");
    assert_target!("td-svc/src/cad.rs", "check");
    assert_target!("td-svc/src/cad.rs", "recipe-checks");
    assert_target!("td-svc/src/evict.rs", "check");
    assert_target!("td-svc/src/evict.rs", "recipe-checks");
    assert_target!("td-svc/src/logs.rs", "check");
    assert_target!("td-svc/src/logs.rs", "recipe-checks");
    assert_no_target!("td-svc/DESIGN.md", "check");
    assert_target!("td-svc/clippy.toml", "check");
    assert_target!("td-svc/Cargo.lock", "check");
    assert_target!("td-svc/Cargo.lock", "recipe-checks");
    assert_preflight!("td-svc/src/order.rs", "cargo-test");
    // The UI pair mirrors the target-static crates above. The compositor's sys.rs
    // assertion is the routing guard for its user-approved wl_shm unsafe exception.
    assert_target!("td-seatd/src/main.rs", "check");
    assert_target!("td-seatd/src/main.rs", "recipe-checks");
    assert_preflight!("td-seatd/src/main.rs", "cargo-test");
    assert_target!("td-seatd/Cargo.lock", "recipe-checks");
    assert_target!("td-compositor/src/main.rs", "check");
    assert_target!("td-compositor/src/main.rs", "recipe-checks");
    assert_target!("td-compositor/src/server.rs", "recipe-checks");
    assert_target!("td-compositor/src/sys.rs", "recipe-checks");
    assert_preflight!("td-compositor/src/sys.rs", "cargo-test");
    // td-term's keyboard and PTY adapters ship in the same multicall, so they
    // route like every other compositor module; pty.rs is the only permitted
    // caller of the confined terminal ioctls.
    assert_target!("td-compositor/src/keys.rs", "recipe-checks");
    assert_preflight!("td-compositor/src/keys.rs", "cargo-test");
    assert_target!("td-compositor/src/font.rs", "recipe-checks");
    assert_preflight!("td-compositor/src/font.rs", "cargo-test");
    assert_target!("td-compositor/src/font_data.rs", "recipe-checks");
    assert_preflight!("td-compositor/src/font_data.rs", "cargo-test");
    assert_target!("td-compositor/tools/import-unifont.rs", "recipe-checks");
    assert_preflight!("td-compositor/tools/import-unifont.rs", "cargo-test");
    assert_target!("td-compositor/src/pty.rs", "recipe-checks");
    assert_preflight!("td-compositor/src/pty.rs", "cargo-test");
    assert_target!("td-compositor/src/render.rs", "recipe-checks");
    assert_preflight!("td-compositor/src/render.rs", "cargo-test");
    // Rendered goldens route like the terminal corpus beside them.
    assert_target!("td-compositor/spec/render/renditions.ppm", "check");
    assert_preflight!("td-compositor/spec/render/renditions.ppm", "cargo-test");
    assert_target!("td-compositor/spec/term/input.term", "check");
    assert_preflight!("td-compositor/spec/term/input.term", "cargo-test");
    assert_target!("td-compositor/tools/import-libvterm.rs", "check");
    assert_preflight!("td-compositor/tools/import-libvterm.rs", "cargo-test");
    assert_target!("td-compositor/tools/libvterm-0.3.3.sources", "check");
    assert_preflight!(
        "td-compositor/tools/libvterm-0.3.3.sources",
        "cargo-test"
    );
    assert_target!("td-compositor/spec/term/cursor.term", "check");
    assert_preflight!("td-compositor/spec/term/cursor.term", "cargo-test");
    assert_target!("td-compositor/Cargo.lock", "recipe-checks");
    assert_no_target!("td-compositor/DESIGN.md", "check");
    assert_preflight!("start", "shell-syntax");
    // td-netd/src is include_str!'d into the target artifact (its recipe AND packed
    // into system-x86-64), so a helper-source edit rides the host cargo preflight
    // AND is recorded against recipe-checks, which statically links + shape-asserts
    // it via td-netd-test.
    assert_target!("td-netd/src/main.rs", "check");
    assert_target!("td-netd/src/main.rs", "recipe-checks");
    assert_target!("td-netd/Cargo.toml", "check");
    assert_target!("td-netd/Cargo.toml", "recipe-checks");
    assert_target!("td-boot/src/main.rs", "check");
    assert_target!("td-boot/src/main.rs", "recipe-checks");
    assert_target!("td-boot/Cargo.toml", "check");
    assert_target!("td-boot/Cargo.toml", "recipe-checks");
    // protocol.rs is the deployment contract three OTHER trees compile: two
    // recipes and — since the manifest header and bound moved into it — td-net.
    // What distinguishes it from the rest of td-boot is the chain targets: no
    // gate builds td-net from source, the recipe-graph WARM does, so a break
    // that only reaches the signer runs nothing without them. `td-boot/src/
    // main.rs` selecting none of the three is the other half of the pin — the
    // rule has to be about this file and not about the crate.
    assert_target!("td-boot/src/protocol.rs", "check");
    assert_target!("td-boot/src/protocol.rs", "recipe-checks");
    assert_target!(
        "td-boot/src/protocol.rs",
        "bootstrap-x86_64-toolchain-store-native"
    );
    assert_target!(
        "td-boot/src/protocol.rs",
        "bootstrap-x86_64-native-gcc-store-native"
    );
    assert_target!("td-boot/src/protocol.rs", "bootstrap-x86_64-self-gcc-store-native");
    assert_preflight!("td-boot/src/protocol.rs", "cargo-test");
    assert_no_target!(
        "td-boot/src/main.rs",
        "bootstrap-x86_64-toolchain-store-native"
    );
    assert_no_target!(
        "td-boot/src/main.rs",
        "bootstrap-x86_64-native-gcc-store-native"
    );
    assert_no_target!("td-boot/src/main.rs", "bootstrap-x86_64-self-gcc-store-native");
    // Nothing builds td-review as a target artifact, so no -test recipe links
    // it — and it selects no check target at all, so these assert what RUNS
    // rather than only what is recorded: `cargo-test` is the whole selection,
    // and it is the only gate whose body names the crate.
    assert_preflight!("td-review/src/main.rs", "cargo-test");
    assert_preflight!("td-review/src/land.rs", "cargo-test");
    assert_preflight!("td-review/tests/land.rs", "cargo-test");
    assert_preflight!("td-review/Cargo.toml", "cargo-test");
    assert_preflight!("td-review/Cargo.lock", "cargo-test");
    assert_no_target!("td-review/src/main.rs", "recipe-checks");
    assert_no_target!("td-review/Cargo.toml", "recipe-checks");
    // The bootstrap ladder cannot read a host-side crate: selecting it here
    // bought nothing and cost the hour that makes a pre-push check get skipped.
    assert_no_target!("td-review/src/main.rs", "check");
    assert_no_target!("td-review/src/land.rs", "check");
    assert_no_target!("td-review/tests/land.rs", "check");
    assert_no_target!("td-review/Cargo.toml", "check");
    assert_no_target!("td-review/Cargo.lock", "check");
    // The record fixture is read by the builder AND td-review suites; the one
    // cargo-test preflight runs both, and it must not escalate to the full check.
    assert_preflight!("tests/review-record.cases", "cargo-test");
    assert_no_target!("tests/review-record.cases", "check");
    assert_target!("tests/td-toolchain.lock", "toolchain-input-addressed");
    assert_target!(
        "tests/td-toolchain.lock",
        "toolchain-x86_64-input-addressed"
    );
    assert_target!(
        "tests/td-toolchain-x86_64.lock",
        "toolchain-x86_64-input-addressed"
    );
    assert_target!(
        "tests/td-toolchain-x86_64.lock",
        "bootstrap-x86_64-toolchain-store-native"
    );
    assert_target!(
        "tests/x86_64-subst-lib.sh",
        "bootstrap-x86_64-toolchain-store-native"
    );
    assert_target!(
        "tests/bootstrap-x86_64-native-gcc-store-native.sh",
        "bootstrap-x86_64-native-gcc-store-native"
    );
    assert_target!(
        "tests/bootstrap-x86_64-self-gcc-store-native.sh",
        "bootstrap-x86_64-self-gcc-store-native"
    );
    assert_target!(
        "tests/x86_64-cross-fns.sh",
        "bootstrap-x86_64-self-gcc-store-native"
    );
    assert_target!(
        "builder/src/gate_defs/426-bootstrap-x86_64-self-gcc-store-native.rs",
        "bootstrap-x86_64-self-gcc-store-native"
    );
    assert_target!(
        "builder/src/gate_defs/422-bootstrap-x86_64-native-gcc-store-native.rs",
        "bootstrap-x86_64-native-gcc-store-native"
    );
    assert_target!(
        "tests/x86_64-cross-fns.sh",
        "bootstrap-x86_64-native-gcc-store-native"
    );
    assert_target!(
        "tests/toolchain-x86_64-input-addressed.sh",
        "toolchain-x86_64-input-addressed"
    );
    assert_target!(
        "builder/src/gate_defs/418-toolchain-x86_64-input-addressed.rs",
        "toolchain-x86_64-input-addressed"
    );
    // Recipe-owned source pins route through the recipe-engine gate and the build gates.
    assert_target!("recipes/src/source_pins.rs", "recipe-rs");
    assert_target!("recipes/src/source_pins.rs", "recipe-checks");
    assert_target!("recipes/src/source_pins.rs", "bootstrap-seed");
    assert_target!("recipes/src/source_pins.rs", "bootstrap-mes");
    // bootstrap-seed / bootstrap-mes are structured Rust recipes (no shell driver):
    // source-pin edits route via the recipe crate; the recipe code
    // (builder/src/bootstrap.rs) validates on the check-engine smoke + cargo-test.
    assert_target!("seed/stage0/AMD64/hex0_AMD64.hex0", "bootstrap-seed");
    assert_target!("builder/src/bootstrap.rs", "check-engine");
    assert_branch_policy!("builder/src/bootstrap.rs", "the full check would be waived");
    // The td-builder build engine validates on the check-engine SMOKE tier.
    assert_target!("builder/src/sandbox.rs", "check-engine");
    assert_branch_policy!("builder/src/main.rs", "the full check would be waived");
    assert_branch_policy!("builder/src/sandbox.rs", "the full check would be waived");
    assert_branch_policy!("builder/Cargo.toml", "the full check would be waived");
    // The loop spine and unmapped paths have no focused gate to name, so they
    // escalate to the whole behavioral tier.
    assert_runs!("builder/src/gates.rs", "check");
    assert_branch_policy!("builder/src/gates.rs", "the full check would be waived");
    assert_runs!("new/unmapped.file", "check");
    assert_branch_policy!("new/unmapped.file", "the full check would be waived");
    // A chain diff names BOTH its focused target (bootstrap-seed) and the proof of
    // the whole ladder (recipe-checks) — #397: the per-rung bootstrap-gcc-mesboot
    // gate this used to name is retired. Both RUN now; the tier that used to
    // hold recipe-checks back is gone.
    assert_runs!("seed/stage0/AMD64/hex0_AMD64.hex0", "bootstrap-seed");
    assert_target!("seed/stage0/AMD64/hex0_AMD64.hex0", "recipe-checks");
    assert_runs!("recipes/src/source_pins.rs", "recipe-rs");
    assert_runs!("recipes/src/catalog.rs", "recipe-rs");

    // The heal primitive's behavioral test moved from CI into the `heal-revert`
    // preflight (GitHub is a backup remote only): editing the primitive or its
    // test selects it (the dev host has git; the loop sandbox does not).
    assert_contains!("ci/revert-suspect.sh", "bash tests/heal-revert.sh");
    assert_contains!("tests/heal-revert.sh", "bash tests/heal-revert.sh");

    failures
}

// ---------------------------------------------------------------------------
// CLI.
// ---------------------------------------------------------------------------

/// git's lines, or `None` if the command failed — for the queries whose empty
/// answer would otherwise be indistinguishable from success with no results.
fn git_lines_checked(root: &Path, args: &[&str]) -> Option<Vec<String>> {
    let out = Command::new("git").args(args).current_dir(root).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).lines().map(str::to_string).collect())
}

pub(crate) fn git_ok(root: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .args(args)
        .current_dir(root)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// The repo root, the way the shell roots itself (`cd "$(dirname "$0")/.."`):
/// `git rev-parse --show-toplevel` when git is present, else CWD. Keeps the
/// subcommand CWD-robust like the oracle; outside a git repo it falls back to CWD.
pub(crate) fn resolve_root() -> PathBuf {
    if let Ok(o) = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
    {
        if o.status.success() {
            let top = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if !top.is_empty() {
                return PathBuf::from(top);
            }
        }
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

fn sort_unique(mut v: Vec<String>) -> Vec<String> {
    v.retain(|s| !s.is_empty());
    v.sort();
    v.dedup();
    v
}

/// Run the loop entry — THIS binary's `check` subcommand (check.sh is retired;
/// the td programs are called directly, #318).
fn run_self_check(root: &Path, targets: &[String]) -> i32 {
    let Ok(me) = std::env::current_exe() else {
        return 1;
    };
    let mut args: Vec<String> = vec!["check".to_string()];
    args.extend(targets.iter().cloned());
    run_command(root, &me.display().to_string(), &args)
}

fn run_command(root: &Path, program: &str, args: &[String]) -> i32 {
    Command::new(program)
        .args(args)
        .current_dir(root)
        .status()
        .ok()
        .and_then(|s| s.code())
        .unwrap_or(1)
}

fn run_shell(root: &Path, script: &str) -> i32 {
    Command::new("bash")
        .arg("-c")
        .arg(script)
        .current_dir(root)
        .status()
        .ok()
        .and_then(|s| s.code())
        .unwrap_or(1)
}

/// Single-quote a path for the bash `-c` string preflights run through. Refuses a
/// path holding a single quote rather than guessing at an escape — the caller then
/// runs without the variable instead of running a mis-quoted command.
fn shell_quote(p: &Path) -> Option<String> {
    let s = p.to_str()?;
    if s.contains('\'') {
        return None;
    }
    Some(format!("'{s}'"))
}

/// Exactly one `[[package]]` and no external `source = `, as gate 325 spells
/// the AGENTS.md dependency-free rule. Both, because they catch different
/// things: the count catches a new crate, the source line catches a registry
/// one that a stale count would miss.
fn assert_dependency_free(root: &Path, lock: &str, packages: usize) -> Result<(), String> {
    // An unreadable lock is a failure, not a pass: a guard that answers OK when
    // it cannot see the file is a guard that has silently stopped guarding, and
    // gate 325's `count-line-exact` fails the same way on a missing one.
    let text = std::fs::read_to_string(root.join(lock))
        .map_err(|e| format!("{lock} could not be read: {e}"))?;
    dependency_free(lock, &text, packages)
}

/// The check itself, over the lock's TEXT — no filesystem, so its cases are
/// literals in the test rather than a fixture tree.
fn dependency_free(lock: &str, text: &str, expected: usize) -> Result<(), String> {
    let found = text.lines().filter(|l| l.trim() == "[[package]]").count();
    if found != expected {
        return Err(format!(
            "{lock} lists {found} packages, expected exactly {expected} (its own path \
             members) — it must carry ZERO external crates (AGENTS.md 'Rust code'); \
             adding one is a reviewed decision"
        ));
    }
    if text.lines().any(|l| l.trim_start().starts_with("source = \"")) {
        return Err(format!(
            "{lock} carries an external `source = ` — it must carry ZERO external crates \
             (AGENTS.md 'Rust code'); adding one is a reviewed decision"
        ));
    }
    Ok(())
}

/// Every lock this preflight answers for, and the `[[package]]` count it must
/// carry: the engine workspace's three path members, one apiece for the
/// standalone crates.
///
/// Gate 325 asserts these too, but it degrades to a tolerated Unprovisioned
/// SKIP on every host today (re #469) and names this preflight the authoritative
/// enforcement — so in the only tier that executes, this is where the
/// dependency-free claim is actually checked. `--frozen` does not stand in: it
/// demands that the committed lock RESOLVE, not that it be empty.
const DEPENDENCY_FREE_LOCKS: [(&str, usize); 14] = [
    ("Cargo.lock", 3),
    ("td-kexec/Cargo.lock", 1),
    ("td-sh/Cargo.lock", 1),
    ("td-txt/Cargo.lock", 1),
    ("td-netd/Cargo.lock", 1),
    ("td-boot/Cargo.lock", 1),
    ("td-util/Cargo.lock", 1),
    ("td-init/Cargo.lock", 1),
    ("td-firstboot/Cargo.lock", 1),
    ("td-login/Cargo.lock", 1),
    ("td-svc/Cargo.lock", 1),
    ("td-seatd/Cargo.lock", 1),
    ("td-compositor/Cargo.lock", 1),
    ("td-review/Cargo.lock", 1),
];

/// Standalone crates that NO recipe embeds, and so the only ones whose diff
/// cannot reach the engine workspace.
///
/// Every other td-* crate has its sources `include_str!`'d into its recipe
/// (`recipes/src/recipes/<crate>.rs`) so the lintable crate and the shipped
/// binary are one text — which makes `recipes`, and therefore `cargo test
/// --workspace`, a READER of those files. Checked against the tree rather than
/// trusted, in `unembedded_crates_are_really_unembedded`.
const UNEMBEDDED_CRATES: [&str; 1] = ["td-review"];

/// Files under `builder/src` that are NOT the build engine, and so do not owe
/// the from-source behavioural tier the rest of that directory does.
///
/// `builder/src/*` is otherwise one blanket rule, correctly: `sandbox.rs`,
/// `nar.rs`, the store and recipe evaluation are exactly what the from-source
/// rungs exercise. `ready.rs` is a branch gate — it parses commit messages and
/// shells out to git, is reached only from `td-builder ready`, and imports one
/// engine module. Nothing a rung runs can observe it.
///
/// `affected.rs` is deliberately NOT here, though the same could be said of its
/// dependencies: it DECIDES which checks run, so exempting it would let the
/// dispatcher narrow its own coverage with the tier that would have caught it
/// switched off.
///
/// Checked against the tree rather than trusted, in
/// `host_only_sources_are_not_reachable_from_the_engine`.
const HOST_ONLY_ENGINE_SOURCES: [&str; 1] = ["builder/src/ready.rs"];

/// The subset of `CARGO_TEST_CMDS` a diff over `changed` can actually
/// invalidate.
///
/// Narrow ONLY where it is provably sound: every changed path must sit inside
/// an `UNEMBEDDED_CRATES` directory. Anything else — `builder/`, `recipes/`, a
/// crate a recipe embeds, an unmapped file, an empty diff — takes the whole
/// table, so every unknown fails safe.
fn cargo_test_cmds(changed: &[String]) -> Vec<&'static str> {
    let mut scoped: Vec<&'static str> = Vec::new();
    for p in changed {
        // A `..` is refused rather than resolved: `td-review/../td-sh/x.rs`
        // starts with the crate and names another. git never emits one, so this
        // is reachable only through `--path`.
        if p.contains("..") {
            return CARGO_TEST_CMDS.to_vec();
        }
        let owner = UNEMBEDDED_CRATES.iter().find(|c| {
            // The separator matters: `td-reviewer/x` is not `td-review/x`.
            p.starts_with(*c) && p.as_bytes().get(c.len()) == Some(&b'/')
        });
        match owner {
            Some(c) if !scoped.contains(c) => scoped.push(c),
            Some(_) => {}
            None => return CARGO_TEST_CMDS.to_vec(),
        }
    }
    if scoped.is_empty() {
        return CARGO_TEST_CMDS.to_vec();
    }
    let narrowed: Vec<&'static str> = CARGO_TEST_CMDS
        .iter()
        .copied()
        .filter(|cmd| cmd_manifest_crate(cmd).is_some_and(|c| scoped.contains(&c)))
        .collect();
    // An EMPTY narrowing must never be taken at face value: the preflight's
    // loop over nothing exits 0 having run no cargo at all — a green that
    // tested the crate not at all. `unembedded_crates_have_cargo_commands`
    // keeps this unreachable; this is what happens if it ever is not.
    match narrowed.is_empty() {
        true => CARGO_TEST_CMDS.to_vec(),
        false => narrowed,
    }
}

/// The crate a `--manifest-path <crate>/Cargo.toml` command names; None for the
/// `--workspace` ones, which belong to no single crate.
fn cmd_manifest_crate(cmd: &str) -> Option<&str> {
    Some(cmd.split_once("--manifest-path ")?.1.split_once('/')?.0)
}

/// What the `cargo-test` preflight runs, in order. A const so the lock roster
/// above can be checked against it: a crate tested here whose lock is not
/// guarded there would be dependency-free by assertion only.
const CARGO_TEST_CMDS: [&str; 28] = [
    "cargo test --frozen --workspace",
    "cargo test --frozen --manifest-path td-kexec/Cargo.toml",
    "cargo test --frozen --manifest-path td-sh/Cargo.toml",
    "cargo test --frozen --manifest-path td-txt/Cargo.toml",
    "cargo test --frozen --manifest-path td-netd/Cargo.toml",
    "cargo test --frozen --manifest-path td-boot/Cargo.toml",
    "cargo test --frozen --manifest-path td-util/Cargo.toml",
    "cargo test --frozen --manifest-path td-init/Cargo.toml",
    "cargo test --frozen --manifest-path td-firstboot/Cargo.toml",
    "cargo test --frozen --manifest-path td-login/Cargo.toml",
    "cargo test --frozen --manifest-path td-svc/Cargo.toml",
    "cargo test --frozen --manifest-path td-seatd/Cargo.toml",
    "cargo test --frozen --manifest-path td-compositor/Cargo.toml",
    "cargo test --frozen --manifest-path td-review/Cargo.toml -- --include-ignored",
    "cargo clippy --frozen --workspace",
    "cargo clippy --frozen --manifest-path td-kexec/Cargo.toml",
    "cargo clippy --frozen --manifest-path td-sh/Cargo.toml",
    "cargo clippy --frozen --manifest-path td-txt/Cargo.toml",
    "cargo clippy --frozen --manifest-path td-netd/Cargo.toml",
    "cargo clippy --frozen --manifest-path td-boot/Cargo.toml",
    "cargo clippy --frozen --manifest-path td-util/Cargo.toml",
    "cargo clippy --frozen --manifest-path td-init/Cargo.toml",
    "cargo clippy --frozen --manifest-path td-firstboot/Cargo.toml --all-targets",
    "cargo clippy --frozen --manifest-path td-login/Cargo.toml",
    "cargo clippy --frozen --manifest-path td-svc/Cargo.toml --all-targets",
    "cargo clippy --frozen --manifest-path td-seatd/Cargo.toml --all-targets",
    "cargo clippy --frozen --manifest-path td-compositor/Cargo.toml --all-targets",
    "cargo clippy --frozen --manifest-path td-review/Cargo.toml --all-targets",
];

fn run_preflight(root: &Path, name: &str, changed: &[String]) -> i32 {
    match name {
        "shell-syntax" => run_shell(root, "bash -n start tests/*.sh ci/*.sh tools/*.sh"),
        "heal-revert" => run_shell(root, "bash tests/heal-revert.sh"),
        // BOTH engine crates, tests AND clippy: the AGENTS.md deny-lints only
        // fire under the clippy driver, and the in-loop cargo-test gate (325)
        // is unreachable while the loop is UNPROVISIONED (re #469) — this
        // host preflight is the per-PR enforcement in the meantime (review
        // finding: recipes tests + clippy ran in NO automated per-PR tier).
        "cargo-test" => {
            // Before any cargo call, and for EVERY lock rather than only the
            // crate whose path selected this: gate 325 asserts these, and gate
            // 325 does not run (see DEPENDENCY_FREE_LOCKS). So a td-sh-only
            // branch reds here on a td-review lock — deliberate. The claim is
            // repo-wide, the roster is the same either way, and a guard that
            // only looks where the diff already pointed is one that never
            // catches the crate nobody was looking at.
            for (lock, packages) in DEPENDENCY_FREE_LOCKS {
                if let Err(e) = assert_dependency_free(root, lock, packages) {
                    eprintln!("affected-checks: {e}");
                    return 1;
                }
            }
            // The target-built guest programs ride the SAME preflight: all are
            // dependency-free pure std, while their static TARGET links ride recipe-checks.
            // builder + recipes + the shared engine lib are one cargo workspace,
            // so --workspace lints/tests all three in one invocation; the target
            // programs (td-kexec, td-sh, td-txt, td-netd, td-boot, td-util,
            // td-init, td-firstboot, td-login, td-svc, td-seatd, and
            // td-compositor) are
            // standalone crates and ride the preflight explicitly, as does the
            // host-side td-review integrator tool. td-sh's conformance corpus run
            // is NOT `#[ignore]`d: this plain `cargo test` runs the whole corpus,
            // so a regression, an unexpected pass or a stale overlay entry reds
            // the preflight rather than waiting for a tier nothing runs.
            // td-review goes the other way: its
            // App-level tests drive a real git repo, are `#[ignore]`d so the
            // git-less sandbox gate stays honest, and run HERE via
            // --include-ignored — this preflight is their only tier.
            for cmd in cargo_test_cmds(changed) {
                let code = run_shell(root, cmd);
                if code != 0 {
                    return code;
                }
            }
            0
        }
        // The dispatcher's own self-test — run IN-PROCESS (the shell oracle is gone,
        // and this binary IS the dispatcher), so no `td-builder` re-resolution.
        "affected-self-test" => {
            let failures = run_self_test(root);
            for f in &failures {
                eprintln!("FAIL: {f}");
            }
            if failures.is_empty() {
                println!("PASS: affected-checks self-test");
                0
            } else {
                eprintln!("affected-checks self-test: {} failure(s)", failures.len());
                1
            }
        }
        // Re-hash every `local_source` tree and compare it to the row
        // seed/seed-digests.txt pins (re #469). Cheap by construction — the bytes are
        // already in the checkout, so there is no fetch and no ladder — which is why
        // this can be a preflight while the fetched pins' equivalent (`seed-digests`,
        // whole universe, warm cache required) cannot. The key-set coverage unit test
        // does not overlap it: an edited local source keeps its key, so coverage stays
        // green while the row describes a tree that no longer exists.
        // td-recipe-eval hashes through td-builder, so hand it THIS binary rather than
        // rebuilding one: cargo would be replacing the executable it is running from,
        // and a stale one would not matter anyway — td-builder only NAR-hashes here;
        // the compiled table being checked is td-recipe-eval's, which cargo rebuilds.
        "local-source-digests" => {
            let cmd = "cargo run --release --frozen --quiet --manifest-path recipes/Cargo.toml \
                       --bin td-recipe-eval -- local-source-digests";
            match std::env::current_exe().ok().and_then(|p| shell_quote(&p)) {
                Some(tb) => run_shell(root, &format!("TD_BUILDER_SELF={tb} {cmd}")),
                None => run_shell(root, cmd),
            }
        }
        _ => 0,
    }
}

pub fn main(args: &[String]) -> ExitCode {
    ExitCode::from(run(args))
}

/// The dispatcher proper, returning the code rather than an opaque `ExitCode`:
/// `td-builder ready` runs the same bounded selection in-process and has to know
/// whether it passed.
pub fn run(args: &[String]) -> u8 {
    let root = resolve_root();

    let mut base = "origin/main".to_string();
    let mut run = false;
    let mut committed_only = false;
    let mut self_test = false;
    let mut explicit_paths: Vec<String> = Vec::new();

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--run" => run = true,
            "--self-test" => self_test = true,
            "--committed-only" => committed_only = true,
            "--base" => {
                i += 1;
                if i >= args.len() {
                    eprintln!("affected-checks: --base needs a ref");
                    return 2;
                }
                base = args[i].clone();
            }
            "--path" => {
                i += 1;
                if i >= args.len() {
                    eprintln!("affected-checks: --path needs a path");
                    return 2;
                }
                explicit_paths.push(args[i].clone());
            }
            "-h" | "--help" => {
                print!("{HELP}");
                return 0;
            }
            other => {
                eprintln!("affected-checks: unknown arg '{other}'");
                eprint!("{HELP}");
                return 2;
            }
        }
        i += 1;
    }

    if self_test {
        let failures = run_self_test(&root);
        for f in &failures {
            eprintln!("FAIL: {f}");
        }
        if failures.is_empty() {
            println!("PASS: affected-checks self-test");
            return 0;
        }
        eprintln!("affected-checks self-test: {} failure(s)", failures.len());
        return 1;
    }

    // --- assemble the changed-path set ---
    let explicit = !explicit_paths.is_empty();
    let mut merge_base = String::new();
    let changed: Vec<String> = if explicit {
        sort_unique(explicit_paths.clone())
    } else {
        if !git_ok(
            &root,
            &["rev-parse", "--verify", &format!("{base}^{{commit}}")],
        ) {
            if base == "origin/main" && git_ok(&root, &["rev-parse", "--verify", "main^{commit}"]) {
                base = "main".to_string();
            } else {
                eprintln!("affected-checks: base ref '{base}' is not available");
                return 2;
            }
        }
        // The shell's `merge_base=$(git merge-base …)` runs under `set -e`, so a
        // merge-base failure (no common ancestor / shallow clone) aborts non-zero;
        // mirror that rather than continue with an empty merge-base + bogus header.
        match Command::new("git")
            .args(["merge-base", &base, "HEAD"])
            .current_dir(&root)
            .output()
        {
            Ok(o) if o.status.success() => {
                merge_base = String::from_utf8_lossy(&o.stdout)
                    .lines()
                    .next()
                    .unwrap_or("")
                    .to_string();
            }
            Ok(o) => {
                eprint!("{}", String::from_utf8_lossy(&o.stderr));
                return o.status.code().unwrap_or(1) as u8;
            }
            Err(e) => {
                eprintln!("affected-checks: git merge-base failed: {e}");
                return 1;
            }
        }
        // EVERY changed-path query is checked: an empty answer from a FAILED
        // diff would be "no changed paths" a few lines below, which exits 0
        // having run nothing — the same fail-open `ready` refuses on its own
        // queries.
        //
        // `--no-renames` because `--name-only` reports only the DESTINATION of
        // a detected rename, and the selection is a function of the paths it is
        // given: `git mv builder/src/x.rs td-review/src/x.rs` would arrive as a
        // td-review path alone, hiding the deletion that can red the workspace.
        // Off, a rename is a delete plus an add and both sides are seen.
        let Some(mut all) = git_lines_checked(
            &root,
            &["diff", "--no-renames", "--name-only", &merge_base, "HEAD"],
        ) else {
            eprintln!(
                "affected-checks: git diff --no-renames --name-only {merge_base} HEAD failed"
            );
            return 1;
        };
        if !committed_only {
            // Checked for the same reason the committed query is, and the
            // narrowing below is why it can no longer be left: a swallowed
            // failure here used to mis-size the check TARGETS, and now also
            // hides the dirty `builder/` path that is the difference between
            // two cargo commands and twenty-eight.
            for q in [
                &["diff", "--no-renames", "--name-only"][..],
                &["diff", "--cached", "--no-renames", "--name-only"][..],
                &["ls-files", "--others", "--exclude-standard"][..],
            ] {
                let Some(lines) = git_lines_checked(&root, q) else {
                    eprintln!("affected-checks: git {} failed", q.join(" "));
                    return 1;
                };
                all.extend(lines);
            }
        }
        sort_unique(all)
    };

    if changed.is_empty() {
        println!("affected-checks: no changed paths relative to {base}");
        return 0;
    }

    let sel = compute_selection(&root, &changed);
    let header = Header {
        explicit,
        base: &base,
        merge_base: &merge_base,
    };
    print!("{}", format_output(&header, &changed, &sel, run));

    if !run {
        return 0;
    }

    // --- execute ---
    for pre in &sel.preflights {
        let code = run_preflight(&root, pre, &changed);
        if code != 0 {
            return code as u8;
        }
    }

    // Nothing escalates to the full loop: every diff runs its bounded selected
    // targets.
    if !sel.targets.is_empty() {
        let code = run_self_check(&root, &sel.targets);
        // EXIT_UNPROVISIONED is the loop's documented "nothing could run
        // here" machine signal. It is explained loudly here but PROPAGATED UNCHANGED —
        // never rewritten to success: the run did not validate the targets,
        // and the exit code must say so (PR review); the caller decides what
        // PARTIAL means for its tier. Today EVERY host is in that state: the
        // bootstrap graph cannot build the loop userland without host
        // scaffolding, which planning rejects (re #469) — the preflights
        // above (cargo test, on the AGENTS.md rust-toolchain control plane)
        // are the per-PR validation until the chain is self-hosting.
        if code == crate::check_loop::EXIT_UNPROVISIONED {
            println!(
                "affected-checks: check targets [{}] exited UNPROVISIONED (69) — the loop \
                 cannot run until the bootstrap graph builds its own userland (re #469); \
                 preflights above are the coverage until then; exit code 69 propagated \
                 unchanged",
                sel.targets.join(" ")
            );
        }
        return code as u8;
    }

    0
}

// ---------------------------------------------------------------------------
// Tests — the durable self-test guard + the removable shell differential oracle.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn repo_root() -> PathBuf {
        // builder/ → repo root.
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .to_path_buf()
    }

    /// The fixtures these two tests read, asserted rather than used to SKIP.
    ///
    /// There was one marker for both, and it required `check.sh` — which
    /// 578b4ef5 retired, stranding it. The `cargo test` copy of both tests has
    /// been skipping ever since: an `eprintln!` + `return` inside a `#[test]`
    /// is captured, so it read as green. (The suite itself kept running through
    /// its other two entry points, the `--self-test` verb and the
    /// `affected-self-test` preflight, neither of which consults a marker.)
    ///
    /// Both halves are now hard failures, because the environment the skip
    /// defended does not exist in this tree: it was justified by a package build
    /// whose source is `builder/` alone, and no recipe builds `builder`. A tree
    /// missing these should RED — a test that decides for itself not to run is
    /// the failure mode that produced the stranding above.
    ///
    /// They are separate because the two tests read different things:
    /// `run_self_test`'s only filesystem read is `builder/src/gate_defs`,
    /// through `gate_files`, while the lock roster reads the SIBLING crates'
    /// locks. The lock half is asked of the roster's own first entry, so a
    /// renamed lock moves the check with it instead of stranding it again.
    fn require_gate_defs(root: &Path) {
        assert!(
            root.join("builder/src/gate_defs").is_dir(),
            "builder/src/gate_defs absent at {} — the self-test's only fixture",
            root.display()
        );
    }

    fn require_sibling_locks(root: &Path) {
        let Some((first, _)) = DEPENDENCY_FREE_LOCKS.first() else {
            panic!("DEPENDENCY_FREE_LOCKS is empty");
        };
        assert!(
            root.join(first).is_file(),
            "{first} absent at {} — the lock roster's own first entry",
            root.display()
        );
    }

    /// Every `.rs` under `dir`, recursively — the recipes crate nests its
    /// modules, so a flat read would miss the file that matters.
    ///
    /// Unwraps rather than skipping an unreadable entry: its caller asserts the
    /// ABSENCE of a string, so a directory silently not walked is a guard that
    /// passes because it looked at nothing.
    fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
        for e in std::fs::read_dir(dir).unwrap() {
            let e = e.unwrap();
            // The entry's OWN type, which does not follow a symlink: `is_dir()`
            // on the path would, and a link cycle would recurse until the stack
            // ran out rather than fail. A symlink is REFUSED rather than
            // skipped, since either way of skipping one is this scan looking at
            // less than it reports — a link to a directory would drop a whole
            // subtree, and one to a file would drop the file.
            let ty = e.file_type().unwrap();
            let p = e.path();
            assert!(!ty.is_symlink(), "{p:?}: symlink in a scanned source tree");
            if ty.is_dir() {
                collect_rs_files(&p, out);
            } else if ty.is_file() && p.extension().is_some_and(|x| x == "rs") {
                out.push(p);
            }
        }
    }

    #[test]
    fn glob_basics() {
        assert!(glob_match("builder/src/*", "builder/src/a/b.rs")); // '*' spans '/'
        assert!(glob_match("*.md", "a/b.md"));
        assert!(glob_match("recipes/src/*.rs", "recipes/src/source_pins.rs"));
        assert!(!glob_match(
            "recipes/src/*.rs",
            "recipes/src/source_pins.rsX"
        ));
        assert!(!glob_match("CHEAP_GATES", "CHEAP_GATESX"));
        assert!(pattern_matches("check.sh|builder/src/gates.rs", "check.sh"));
        assert!(!pattern_matches(
            "check.sh|builder/src/gates.rs",
            "check.sh2"
        ));
    }

    // DURABLE: the dispatcher's own policy, exercised over the real gate_defs +
    // tests tree. Runs on every PR via the required `cargo-test` job. No shell,
    // no Guix — it still holds with no oracle in the room.
    #[test]
    fn self_test_passes_against_repo() {
        let root = repo_root();
        require_gate_defs(&root);
        let failures = run_self_test(&root);
        assert!(failures.is_empty(), "self-test failures: {failures:#?}");
    }

    /// The self-test walks `gate_files` and reds on any entry no gate
    /// registers, so it has to see the same files the GENERATOR does. An
    /// editor dropping is a `.rs` to `extension()` and is skipped by
    /// `builder/build.rs`, which is a real working tree failing a test over a
    /// file that is not a gate.
    #[test]
    fn a_gate_dir_dropping_is_skipped_as_the_generator_skips_it() {
        let root = std::env::temp_dir().join(format!("td-gate-files-{}", std::process::id()));
        let defs = root.join("builder/src/gate_defs");
        std::fs::remove_dir_all(&root).ok();
        std::fs::create_dir_all(&defs).unwrap();
        // `.#100-real.rs` is the one the dot-skip is for — an emacs lock whose
        // extension IS `rs`. The `.swp` and the `.txt` ride the pre-existing
        // extension filter, and are here so dropping that filter also reds.
        for n in ["100-real.rs", ".100-real.rs.swp", ".#100-real.rs", "notes.txt"] {
            std::fs::write(defs.join(n), "").unwrap();
        }
        let got: Vec<String> = gate_files(&root)
            .unwrap()
            .iter()
            .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .collect();
        std::fs::remove_dir_all(&root).ok();
        assert_eq!(got, vec!["100-real.rs".to_string()]);
        // The MIRROR, pinned against the generator's own source: nothing else
        // ties the two rules together, so a build.rs that later skipped `~`
        // backups too would diverge from this one silently — and the divergence
        // presents as the self-test redding a working tree, which is the
        // failure this test exists for.
        let gen = std::fs::read_to_string(repo_root().join("builder/build.rs")).unwrap();
        assert!(
            gen.contains("name.starts_with('.')"),
            "build.rs no longer skips dot names the way gate_files does"
        );
    }

    /// The premise the cargo-test narrowing rests on, checked against the tree
    /// rather than trusted: no workspace member reads these crates' files, so
    /// `cargo test --workspace` cannot be redded by one.
    ///
    /// Three legs, because the workspace has three ways to become a reader: a
    /// recipe embedding the sources, a manifest depending on the crate, and a
    /// workspace source opening one of its files. Each is the change that would
    /// make the narrowing unsound, and each reds HERE rather than by quietly
    /// skipping a suite that had started to matter.
    ///
    /// It is TEXTUAL, and so bounded: a path assembled at compile time by
    /// `concat!` would defeat all three. What it defends is the ordinary way a
    /// crate comes to be read, which is the way every embedded crate is read
    /// today.
    #[test]
    fn unembedded_crates_are_really_unembedded() {
        // Gated on what this test READS, not a repo-wide sentinel.
        let dir = repo_root().join("recipes/src");
        if !dir.is_dir() {
            eprintln!("SKIP: {dir:?} absent (builder-only sandbox)");
            return;
        }
        let mut sources = Vec::new();
        collect_rs_files(&dir, &mut sources);
        assert!(!sources.is_empty(), "no recipe sources found under {dir:?}");
        let texts: Vec<(PathBuf, String)> = sources
            .into_iter()
            .map(|f| {
                let t = std::fs::read_to_string(&f).unwrap();
                (f, t)
            })
            .collect();
        // POSITIVE CONTROL. Without one the scan can go vacuous — if recipes
        // ever stop naming their crates the way they do now, every entry below
        // passes because nothing matches anything.
        assert!(
            texts.iter().any(|(_, t)| t.contains("td-sh")),
            "the scan found no mention of td-sh, which IS embedded — it is \
             looking at the wrong thing and would pass any roster"
        );
        // The NAME, not one spelling of a relative path: a nested module needs
        // a fourth `../`, and `concat!` would spell it no way this could
        // predict. A recipe crate that so much as NAMES one of these is the
        // signal, and today it names none.
        for krate in UNEMBEDDED_CRATES {
            for (f, text) in &texts {
                assert!(
                    !text.contains(krate),
                    "{krate} is named by {f:?}, so the recipes crate may read it \
                     and the cargo-test narrowing for it is no longer sound"
                );
            }
        }
        // …and no workspace member may depend on one as a crate, which would
        // make the workspace suite a reader without naming a path at all.
        for manifest in [
            "builder/Cargo.toml",
            "recipes/Cargo.toml",
            "engine/Cargo.toml",
        ] {
            let text = std::fs::read_to_string(repo_root().join(manifest)).unwrap();
            for krate in UNEMBEDDED_CRATES {
                assert!(
                    !text.contains(krate),
                    "{manifest} names {krate}, so the workspace depends on it"
                );
            }
        }
        // …and no OTHER workspace member may read one's files either. `recipes`
        // is held to naming the crate at all, which `builder` cannot be — it
        // legitimately spells `td-review/Cargo.toml` in the very table this
        // narrowing filters. So the bar there is a READ: the crate name on a
        // line that also opens something. That is how a workspace member would
        // actually come to read the crate — `builder/src/ready.rs` already
        // records that its `parse_cases` is mirrored in td-review's
        // `record.rs`, and a test that read that file to compare the two is
        // exactly the change this must red on.
        const READERS: [&str; 5] = [
            "include_str!",
            "include_bytes!",
            "read_to_string",
            "File::open",
            "#[path",
        ];
        for dir in ["builder/src", "engine/src"] {
            let d = repo_root().join(dir);
            if !d.is_dir() {
                continue;
            }
            let mut srcs = Vec::new();
            collect_rs_files(&d, &mut srcs);
            assert!(!srcs.is_empty(), "no sources under {d:?}");
            for f in srcs {
                let text = std::fs::read_to_string(&f).unwrap();
                for line in text.lines() {
                    for krate in UNEMBEDDED_CRATES {
                        assert!(
                            !(line.contains(krate) && READERS.iter().any(|r| line.contains(r))),
                            "{f:?} reads {krate}: {line:?}"
                        );
                    }
                }
            }
        }
    }

    /// Every module reached through a `crate::`/`super::` path, however it is
    /// spelled — a `use` line, a `pub use`, an alias, or an inline call in a
    /// function body. Parsing `use crate::` lines alone missed all but the
    /// first, and `affected.rs` itself reaches `crate::gates` that way.
    ///
    /// A braced list (`use crate::{a, b}`) names no single module and is
    /// recorded as `{`, so it is REFUSED rather than silently skipped.
    fn crate_paths_named(text: &str) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for prefix in ["crate::", "super::"] {
            let mut rest = text;
            while let Some(at) = rest.find(prefix) {
                let tail = rest.get(at.saturating_add(prefix.len())..).unwrap_or("");
                let ident: String = tail
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                    .collect();
                let name = match ident.is_empty() {
                    true => tail.chars().next().map(String::from).unwrap_or_default(),
                    false => ident,
                };
                if !name.is_empty() && !out.contains(&name) {
                    out.push(name);
                }
                rest = tail;
            }
        }
        out
    }

    /// Source with `//` comments cut away. These scans are described in prose
    /// that necessarily NAMES what they look for, so without this the module
    /// defining them reds itself. Line-based, as the crate's other source
    /// scans are; a `//` inside a string literal cuts the rest of that line,
    /// which can only make the scan see less on a line no engine path hides in.
    fn strip_line_comments(text: &str) -> String {
        text.lines()
            .map(|l| l.split_once("//").map_or(l, |(code, _)| code))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// `needle` at an identifier boundary. A bare `contains` for a module name
    /// followed by `::` matches `already::` too — fail-safe, but it reds on
    /// the wrong file.
    fn names_at_boundary(text: &str, needle: &str) -> bool {
        let mut from = 0usize;
        while let Some(at) = text.get(from..).and_then(|t| t.find(needle)) {
            let abs = from.saturating_add(at);
            let before = text.get(..abs).and_then(|t| t.chars().next_back());
            if !before.is_some_and(|c| c.is_ascii_alphanumeric() || c == '_') {
                return true;
            }
            from = abs.saturating_add(needle.len());
        }
        false
    }

    /// The premise `HOST_ONLY_ENGINE_SOURCES` rests on, checked rather than
    /// trusted, plus the mapping actually taking effect.
    ///
    /// The IN direction is the SOUNDNESS argument: the exemption says the
    /// from-source tier cannot catch a regression introduced by editing this
    /// file, which holds exactly when nothing that tier runs calls into it.
    /// The OUT direction is a bound on what the file may BECOME — it is not
    /// transitive and deliberately not asserted to be: `affected.rs` reaches
    /// `crate::gates` and `crate::check_loop`, so a closure assertion would be
    /// false today and the exemption does not rest on one.
    ///
    /// Both scans are TEXTUAL and n=1-shaped: a second roster entry would need
    /// the permitted-import set widened and the IN scan taught that two
    /// host-only modules may name each other.
    #[test]
    fn host_only_sources_are_not_reachable_from_the_engine() {
        let root = repo_root();
        let mut sources = Vec::new();
        collect_rs_files(&root.join("builder/src"), &mut sources);

        for rel in HOST_ONLY_ENGINE_SOURCES {
            let Some(stem) = rel
                .strip_prefix("builder/src/")
                .and_then(|f| f.strip_suffix(".rs"))
            else {
                panic!("{rel} is not a builder/src module path");
            };

            // The exemption must TAKE. This arm sits after five that claim
            // paths first, so an entry one of those already routes would be a
            // dead roster: exempt in the author's head and heavy in fact.
            let targets = last_check_targets(&path_output(&root, rel));
            assert!(
                targets.iter().any(|t| t == "check-engine"),
                "{rel} does not select check-engine: {targets:?}"
            );
            assert!(
                !targets.iter().any(|t| t == "check"),
                "{rel} still selects the full check, so its entry is dead: {targets:?}"
            );

            // OUT: the module reaches no engine module but the one it is
            // routed by. An inline `crate::store::…` here would widen what a
            // store change could break in it.
            //
            // Over the SHIPPED half only. `super::` is the crate root from a
            // top-level module — which is what makes `use super::store` worth
            // scanning for — but inside `mod tests` it is the module itself,
            // so `use super::*;` would read as a crate path. Test code is not
            // what the from-source tier runs either way.
            let whole = strip_line_comments(&std::fs::read_to_string(root.join(rel)).unwrap());
            let text = whole.split("#[cfg(test)]").next().unwrap_or_default();
            assert!(
                text.len() < whole.len(),
                "{rel} has no #[cfg(test)] module — the split is not doing what \
                 this scan assumes"
            );
            let named = crate_paths_named(text);
            // POSITIVE CONTROL, for the reason the IN scan has one: a scan
            // that finds nothing passes any roster.
            assert!(
                !named.is_empty(),
                "{rel} names no crate:: path at all — the scan is looking for \
                 the wrong thing"
            );
            for m in &named {
                assert_eq!(m, "affected", "{rel} reaches crate::{m}");
            }

            // IN: nothing but `main.rs` names it. A module the engine's own
            // code called would BE engine code, whatever this roster says.
            let mut named_by_main = false;
            for f in &sources {
                let body = strip_line_comments(&std::fs::read_to_string(f).unwrap());
                // Both spellings: `main.rs` declares the module and calls
                // `ready::main`, while another module would need
                // `crate::ready` — including through `use crate::ready as r`,
                // which names no `ready::` anywhere.
                let names = names_at_boundary(&body, &format!("{stem}::"))
                    || crate_paths_named(&body).iter().any(|m| m == stem);
                if f.ends_with(rel) {
                    continue;
                }
                if f.ends_with("builder/src/main.rs") {
                    named_by_main |= names;
                    continue;
                }
                assert!(
                    !names,
                    "{f:?} reaches {stem}, so it is not called only from the \
                     subcommand dispatch"
                );
            }
            assert!(
                named_by_main,
                "main.rs never names {stem} — the scan is looking for the \
                 wrong thing and would pass any roster"
            );
        }
    }

    /// Every roster entry must HAVE commands in the table. Without this the
    /// filter yields an empty list for it, and a preflight that loops over
    /// nothing exits 0 having run no cargo at all.
    #[test]
    fn unembedded_crates_have_cargo_commands() {
        for krate in UNEMBEDDED_CRATES {
            let mine: Vec<&str> = CARGO_TEST_CMDS
                .iter()
                .copied()
                .filter(|c| cmd_manifest_crate(c) == Some(krate))
                .collect();
            // Both KINDS, not merely two commands: `render_cargo_test` prints
            // the words "cargo test + clippy" unconditionally, so a crate that
            // had lost its test line would advertise a run it no longer does.
            for want in ["cargo test ", "cargo clippy "] {
                assert!(
                    mine.iter().any(|c| c.starts_with(want)),
                    "{krate} has no `{want}` command: {mine:?}"
                );
            }
        }
    }

    /// `cmd_manifest_crate` is the whole of what decides which suites a
    /// narrowed run keeps, and it reads ONE spelling. `--manifest-path=<p>`
    /// (which cargo accepts) parses as None, i.e. as a workspace command, and
    /// so would be dropped from every narrowed set silently; a nested manifest
    /// would name the wrong crate. Neither is reachable with today's table —
    /// this is what keeps it that way.
    #[test]
    fn every_cargo_command_has_the_shape_the_parser_assumes() {
        for cmd in CARGO_TEST_CMDS {
            if cmd.contains("--workspace") {
                assert_eq!(cmd_manifest_crate(cmd), None, "{cmd:?}");
                continue;
            }
            let krate = cmd_manifest_crate(cmd).unwrap_or_else(|| panic!("{cmd:?}: unparsed"));
            assert!(
                cmd.contains(&format!("--manifest-path {krate}/Cargo.toml")),
                "{cmd:?} is not `--manifest-path <crate>/Cargo.toml`"
            );
        }
    }

    /// The narrowing itself, in both directions. The unsound direction is the
    /// one that matters: anything the rule does not recognise must take the
    /// whole table, so a new crate or an unmapped path fails safe.
    #[test]
    fn cargo_commands_narrow_only_for_unembedded_crates() {
        let all = CARGO_TEST_CMDS.len();
        let one = |p: &str| cargo_test_cmds(&[p.to_string()]);
        // td-review alone: its own manifest and nothing else — no --workspace,
        // which is where the seed-recipe builds and tarball decoding live.
        let scoped = one("td-review/src/land.rs");
        assert_eq!(scoped.len(), 2, "test + clippy for one crate: {scoped:?}");
        assert!(scoped.iter().all(|c| c.contains("td-review/Cargo.toml")));
        assert!(!scoped.iter().any(|c| c.contains("--workspace")));
        // A crate a recipe embeds must NOT narrow.
        assert_eq!(one("td-sh/src/main.rs").len(), all);
        assert_eq!(one("td-compositor/src/pty.rs").len(), all);
        // Neither may the engine, an unmapped path, or an empty diff.
        assert_eq!(one("builder/src/affected.rs").len(), all);
        assert_eq!(one("recipes/src/recipes/td-sh.rs").len(), all);
        assert_eq!(one("who/knows.rs").len(), all);
        assert_eq!(cargo_test_cmds(&[]).len(), all);
        // A prefix is not a directory: `td-reviewer/` is a different crate.
        assert_eq!(one("td-reviewer/src/main.rs").len(), all);
        // Nor may a `..` that starts with the crate and names another.
        assert_eq!(one("td-review/../td-sh/src/main.rs").len(), all);
        // One narrowable path does not license the OTHERS in the same diff.
        assert_eq!(
            cargo_test_cmds(&[
                "td-review/src/land.rs".to_string(),
                "builder/src/gates.rs".to_string(),
            ])
            .len(),
            all,
            "a mixed diff must take the whole table"
        );
    }

    /// The rendered line and the executed list come from one call, so the dry
    /// run cannot advertise a command the run will not issue.
    #[test]
    fn the_printed_cargo_line_matches_what_would_run() {
        let changed = vec!["td-review/src/land.rs".to_string()];
        let line = preflight_cmd("cargo-test", &changed).unwrap_or_default();
        let running: Vec<&str> = cargo_test_cmds(&changed)
            .iter()
            .filter_map(|c| cmd_manifest_crate(c))
            .collect();
        assert!(!running.is_empty(), "nothing would run: {line:?}");
        // Both directions, over EVERY crate the full table names. The "names
        // each one that runs" half is vacuous on its own: a render that printed
        // all thirteen manifests would satisfy it, and printing a command the
        // preflight will not run is exactly the lie this test exists to catch.
        for cmd in CARGO_TEST_CMDS {
            let Some(krate) = cmd_manifest_crate(cmd) else {
                continue;
            };
            let manifest = format!("--manifest-path {krate}/Cargo.toml");
            assert_eq!(
                line.contains(&manifest),
                running.contains(&krate),
                "{line:?} disagrees with what would run about {krate}"
            );
        }
        assert!(!line.contains("--workspace"), "{line:?}");
        // …and the unnarrowed line is unchanged from what it always printed.
        let full =
            preflight_cmd("cargo-test", &["builder/src/main.rs".to_string()]).unwrap_or_default();
        assert!(full.starts_with("  cargo test + clippy --frozen --workspace (builder/recipes/engine) + --manifest-path td-kexec/Cargo.toml"));
        assert!(full.ends_with("--manifest-path td-review/Cargo.toml -- --include-ignored"));
    }

    /// A listing error must RED rather than come back short: the caller's loop
    /// asserts every gate file maps, and over an empty list it asserts nothing.
    #[test]
    fn an_unreadable_gate_dir_is_an_error_not_an_empty_list() {
        let root = std::env::temp_dir().join(format!("td-gate-none-{}", std::process::id()));
        std::fs::remove_dir_all(&root).ok();
        std::fs::create_dir_all(&root).unwrap();
        let got = gate_files(&root);
        std::fs::remove_dir_all(&root).ok();
        assert!(got.is_err(), "a missing gate dir must be an error: {got:?}");
    }

    /// Gate 325 asserts these lock shapes and does not run (re #469), so this
    /// preflight check is what actually stands between the crates and a
    /// dependency. It has to red on both shapes.
    #[test]
    fn the_dependency_free_guard_reds_on_a_crate_and_on_a_source_line() {
        let lock = "td-review/Cargo.lock";
        assert!(dependency_free(lock, "[[package]]\nname = \"td-review\"\n", 1).is_ok());
        let two = dependency_free(
            lock,
            "[[package]]\nname = \"td-review\"\n\n[[package]]\nname = \"ureq\"\n",
            1,
        );
        assert!(two.is_err_and(|e| e.contains("lists 2 packages")), "a second crate must red");
        let sourced = dependency_free(
            lock,
            "[[package]]\nname = \"ureq\"\nsource = \"registry+https://example.invalid\"\n",
            1,
        );
        assert!(
            sourced.is_err_and(|e| e.contains("external `source = `")),
            "a registry crate must red even when the count still reads 1"
        );
        assert!(dependency_free(lock, "", 1).is_err(), "an empty lock is not a pass");
        // The workspace root carries three path members, and a fourth is the
        // shape that must red there.
        let three = "[[package]]\na\n[[package]]\nb\n[[package]]\nc\n";
        assert!(dependency_free("Cargo.lock", three, 3).is_ok());
        assert!(dependency_free("Cargo.lock", three, 1).is_err());

        // Every roster entry against the real guard, including the read. This
        // is the enforcement of AGENTS.md's dependency-free rule over every td
        // crate's committed lock, and it shared the stranded marker above — so
        // its `cargo test` copy was skipping for the same six weeks.
        let root = repo_root();
        require_sibling_locks(&root);
        for (lock, packages) in DEPENDENCY_FREE_LOCKS {
            assert!(
                assert_dependency_free(&root, lock, packages).is_ok(),
                "the committed {lock} must pass its own guard"
            );
        }
        assert!(
            assert_dependency_free(&root, "td-review/nope.lock", 1).is_err(),
            "an unreadable lock reds rather than passing"
        );
    }

    /// The roster and the command list are two hand-written copies of the same
    /// crate set. A crate tested by the preflight whose lock is not guarded
    /// would be dependency-free by assertion only — which is how gate 325's own
    /// hand-written list would drift too.
    #[test]
    fn every_crate_the_preflight_tests_has_its_lock_guarded() {
        for cmd in CARGO_TEST_CMDS {
            let Some(rest) = cmd.split("--manifest-path ").nth(1) else {
                // `--workspace`: the root lock, which the roster carries.
                assert!(
                    DEPENDENCY_FREE_LOCKS.iter().any(|(l, _)| *l == "Cargo.lock"),
                    "the workspace lock must be guarded"
                );
                continue;
            };
            let Some(krate) = rest.split('/').next() else { continue };
            let lock = format!("{krate}/Cargo.lock");
            assert!(
                DEPENDENCY_FREE_LOCKS.iter().any(|(l, _)| *l == lock),
                "{cmd}: {lock} is not in DEPENDENCY_FREE_LOCKS"
            );
        }
    }

    // DURABLE renderer guard (replaces the now-deleted shell differential — the
    // shell oracle was the removable migration leg, retired with the cutover,
    // directive 4). Asserts the FULL `--path` render byte-for-byte for paths whose
    // mapping is INDEPENDENT of repo files, so it is fully deterministic and runs
    // EVERYWHERE — including the builder-only package sandbox (no repo tree needed).
    // The dynamic mappings stay covered by `self_test_passes_against_repo`.
    #[test]
    fn renders_exact_output_for_static_paths() {
        let root = repo_root();
        let expect = |lines: &[&str]| -> String {
            let mut s = lines.join("\n");
            s.push('\n');
            s
        };

        // builder/src/* → check-engine smoke + the engine note (waived).
        assert_eq!(
            path_output(&root, "builder/src/main.rs"),
            expect(&[
                "affected-checks: explicit path mode",
                "",
                "Changed paths:",
                "  builder/src/main.rs",
                "",
                "Selected checks:",
                "  cargo test + clippy --frozen --workspace (builder/recipes/engine) + --manifest-path td-kexec/Cargo.toml + --manifest-path td-sh/Cargo.toml + --manifest-path td-txt/Cargo.toml + --manifest-path td-netd/Cargo.toml + --manifest-path td-boot/Cargo.toml + --manifest-path td-util/Cargo.toml + --manifest-path td-init/Cargo.toml + --manifest-path td-firstboot/Cargo.toml + --manifest-path td-login/Cargo.toml + --manifest-path td-svc/Cargo.toml + --manifest-path td-seatd/Cargo.toml + --manifest-path td-compositor/Cargo.toml + --manifest-path td-review/Cargo.toml -- --include-ignored",
                "  td-builder check check-engine check",
                "",
                "Waiver: inspection only (--path does not prove the branch diff)",
                "Branch-mode policy for these paths: the full check would be waived",
                "",
                "Notes:",
                "  - builder/src/main.rs is the td-builder build engine: the ~2-min check-engine smoke (compile + unit tests) is the fast signal; the from-source build coverage is the full check (DESIGN §7.2).",
                "",
                "Dry run only. Re-run with --run to execute.",
            ])
        );

        // Loop spine → the whole behavioral tier + engine tests (waived).
        assert_eq!(
            path_output(&root, "check.sh"),
            expect(&[
                "affected-checks: explicit path mode",
                "",
                "Changed paths:",
                "  check.sh",
                "",
                "Selected checks:",
                "  bash -n start tests/*.sh ci/*.sh tools/*.sh",
                "  cargo test + clippy --frozen --workspace (builder/recipes/engine) + --manifest-path td-kexec/Cargo.toml + --manifest-path td-sh/Cargo.toml + --manifest-path td-txt/Cargo.toml + --manifest-path td-netd/Cargo.toml + --manifest-path td-boot/Cargo.toml + --manifest-path td-util/Cargo.toml + --manifest-path td-init/Cargo.toml + --manifest-path td-firstboot/Cargo.toml + --manifest-path td-login/Cargo.toml + --manifest-path td-svc/Cargo.toml + --manifest-path td-seatd/Cargo.toml + --manifest-path td-compositor/Cargo.toml + --manifest-path td-review/Cargo.toml -- --include-ignored",
                "  td-builder check check",
                "",
                "Waiver: inspection only (--path does not prove the branch diff)",
                "Branch-mode policy for these paths: the full check would be waived",
                "",
                "Notes:",
                "  - check.sh touches the loop spine: validated by the full check (the runner runs itself over every gate).",
                "",
                "Dry run only. Re-run with --run to execute.",
            ])
        );

        // td-review → the cargo-test preflight, scoped to td-review's OWN
        // manifest, and NOTHING else: the only selection with no `td-builder
        // check` line at all. Pinned as exact output because both absences are
        // the point — a stray target would restore an hour of bootstrap builds,
        // and a stray manifest would restore the workspace suite, for a crate
        // none of it reads.
        assert_eq!(
            path_output(&root, "td-review/src/land.rs"),
            expect(&[
                "affected-checks: explicit path mode",
                "",
                "Changed paths:",
                "  td-review/src/land.rs",
                "",
                "Selected checks:",
                "  cargo test + clippy --frozen --manifest-path td-review/Cargo.toml -- --include-ignored",
                "",
                "Waiver: inspection only (--path does not prove the branch diff)",
                "Branch-mode policy for these paths: the full check would be waived",
                "",
                "Dry run only. Re-run with --run to execute.",
            ])
        );

        // Docs → no checks (waived).
        assert_eq!(
            path_output(&root, "README.md"),
            expect(&[
                "affected-checks: explicit path mode",
                "",
                "Changed paths:",
                "  README.md",
                "",
                "Selected checks: none (docs-only or ignored local metadata)",
                "",
                "Waiver: inspection only (--path does not prove the branch diff)",
                "Branch-mode policy for these paths: the full check would be waived",
                "",
                "Dry run only. Re-run with --run to execute.",
            ])
        );

        // Catch-all → the whole behavioral tier (waived).
        assert_eq!(
            path_output(&root, "totally/unmapped/path.xyz"),
            expect(&[
                "affected-checks: explicit path mode",
                "",
                "Changed paths:",
                "  totally/unmapped/path.xyz",
                "",
                "Selected checks:",
                "  td-builder check check",
                "",
                "Waiver: inspection only (--path does not prove the branch diff)",
                "Branch-mode policy for these paths: the full check would be waived",
                "",
                "Notes:",
                "  - No mapping for totally/unmapped/path.xyz — running the full check. Update builder/src/affected.rs with a mapping for it.",
                "",
                "Dry run only. Re-run with --run to execute.",
            ])
        );
    }
}

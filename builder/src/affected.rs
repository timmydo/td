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

use std::collections::HashSet;
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
    /// Affected gates that are DAILY/SYSTEM-tier: named for the record but not
    /// run per-PR — `td-builder daily` covers them nightly with
    /// fix-or-revert healing (the ~10-min per-PR budget, human 2026-07-04).
    deferred: Vec<String>,
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
fn gate_files(root: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(root.join("builder/src/gate_defs"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().map(|e| e == "rs").unwrap_or(false))
        .collect();
    v.sort();
    v
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

/// Would a plain `td-builder check` (cheap+heavy+daily gates + build-recipes)
/// cover `target`? (`check-pr` is a subset of the plain check by construction.)
/// The pool question is gates.rs's (`pool_in_full_check`), not a local list.
fn default_check_covers_target(_root: &Path, target: &str) -> bool {
    if target == "check-fast" || target == "check-pr" || target == "build-recipes" {
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

fn add_recipe_graph_targets(sel: &mut Selection) {
    sel.add_target("bootstrap-x86_64-toolchain-store-native");
    sel.add_target("bootstrap-x86_64-native-gcc-store-native");
    sel.add_target("bootstrap-x86_64-self-gcc-store-native");
    sel.add_target("recipe-checks-daily");
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
// nothing ports those checks elsewhere. The remaining recipe-owned daily checks
// are the live coverage.
fn add_chain_targets(sel: &mut Selection) {
    add_recipe_graph_targets(sel);
}

// ---------------------------------------------------------------------------
// map_path — the `case` ladder, arm-for-arm with the shell (first match wins).
// ---------------------------------------------------------------------------

fn map_path(root: &Path, p: &str, sel: &mut Selection) {
    // Ignored local metadata.
    if pattern_matches(".claude/*|.td-build-cache/*|builder/target/*", p) {
        return;
    }

    if pattern_matches(
        "check.sh|builder/build.rs|builder/src/gates.rs|builder/src/check_loop.rs",
        p,
    ) {
        // The loop spine used to escalate to the FULL loop; it now validates on
        // the bounded check-pr tier (which exercises the runner/prelude end to
        // end over the whole PR pool) + the engine unit tests — the daily-tier
        // gates are the daily backstop's job (the ~10-min per-PR budget, human
        // 2026-07-04).
        sel.add_preflight("shell-syntax");
        sel.add_preflight("cargo-test");
        // check-pr already contains the cargo-test GATE (Pool::Heavy) — no
        // explicit target, or spine diffs would run the engine suite twice.
        sel.add_target("check-pr");
        sel.add_note(&format!(
            "{p} touches the loop spine: validated by the bounded check-pr tier (the runner \
             runs itself over the whole PR pool); the daily backstop covers the daily tier."
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
                    sel.add_target("check-pr");
                    sel.add_note(&format!(
                        "{p} does not register a gate target — running the bounded check-pr \
                         tier; the daily backstop covers the daily-tier gates."
                    ));
                }
            }
        } else {
            sel.add_target("check-pr");
            sel.add_note(&format!(
                "{p} was deleted; affected-checks cannot infer the removed gate target — \
                 running the bounded check-pr tier (a stale reference to the gate reds there \
                 or in the daily backstop)."
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
            "recipe-checks-daily",
            "store-native-profile",
            "sandbox-hardening",
            "toolchain-input-addressed",
            "toolchain-x86_64-input-addressed",
        ] {
            sel.add_target(g);
        }
        return;
    }

    if pattern_matches("builder/Cargo.toml|builder/Cargo.lock|builder/src/*", p) {
        // The td-builder build engine validates on the ~2-min check-engine SMOKE tier
        // (DESIGN §7.2): cargo-test (compile + unit tests), NOT the from-source corpus
        // (that is the DAILY backstop). cargo-test also runs as a host preflight.
        sel.add_preflight("cargo-test");
        sel.add_target("check-engine");
        sel.add_note(&format!(
            "{p} is the td-builder build engine: validated by the ~2-min check-engine smoke (compile + unit tests); the from-source build coverage is the DAILY backstop (DESIGN §7.2), not a per-PR gate."
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
    if p == "seed/seed-digests.txt" {
        sel.add_preflight("cargo-test");
        sel.add_target("recipe-rs");
        add_build_gate_targets(root, sel);
        return;
    }

    if pattern_matches(
        "recipes/*|recipes/src/*|recipes/Cargo.toml|recipes/Cargo.lock|tests/recipe-eval-tool.sh",
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
        sel.add_target("recipe-rs");
        if glob_match("recipes/src/recipes/*.rs", p) {
            sel.add_target("recipe-checks-daily");
        }
        add_build_gate_targets(root, sel);
        return;
    }

    if p == "tests/recipe-checks.sh" {
        sel.add_target("recipe-checks-daily");
        return;
    }

    if pattern_matches("net/*|net/src/*|net/Cargo.toml|net/Cargo.lock", p) {
        // The merged td-net (fetch/feed/subst). It holds the host-PREP warm that feeds the
        // recipe-graph consumers (`warm sources` + `warm kernel-headers`) → the chain targets
        // (former feed coverage); AND, since the old fetch/* mapped to the bounded check-pr
        // tier, a net-only change must keep that per-PR validation — the chain targets are
        // daily-DEFERRED, so without check-pr a net-only diff would run nothing while waiving
        // the full check. The union of BOTH former rules. No gate builds td-net from source;
        // the warm compiles it.
        sel.add_target("check-pr");
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
    // the live graph consumers: recipe-owned checks and the x86_64 daily gates.
    if pattern_matches("seed/patches/binutils-boot-*.patch|seed/patches/gcc-boot-2.95.3.patch|seed/patches/glibc-boot-2.2.5.patch|seed/patches/glibc-bootstrap-system-2.2.5.patch|seed/patches/gcc-boot-4.6.4.patch|seed/patches/glibc-boot-2.16.0.patch|seed/patches/glibc-bootstrap-system-2.16.0.patch", p)
    {
        sel.add_preflight("shell-syntax");
        add_chain_targets(sel);
        return;
    }
    // Tombstones for the retired x86_64 shell drivers/libs. The live orchestration is
    // recipe-owned; these deleting diffs still route to the daily gates that delegate
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
    // native toolchain as its builder) — both Daily/system tier, deferred to the daily backstop.
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
        // all three daily x86_64 gates.
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

    if pattern_matches("*.md|DESIGN.md|CLAUDE.md|.gitignore", p) {
        return; // docs — no checks
    }

    // td-kexec: the target-built guest kexec helper, a standalone crate OUTSIDE the
    // builder/recipes engine. Unlike fetch/feed/subst it is dependency-free pure std
    // and compiles offline, so route it to the cargo-test preflight (host-native
    // clippy/test of the same source). No per-PR gate builds it from source (that is
    // daily/operator), so the bounded target is check-pr. But src/main.rs is
    // `include_str!`'d verbatim into the td-kexec RECIPE — a helper-source edit changes
    // the TARGET artifact, and a target-static-link regression (e.g. the libgcc_eh
    // gap) is invisible to host cargo. So also route to recipe-checks-daily (daily
    // tier -> recorded as deferred to the daily backstop, which statically links it via
    // td-kexec-test). Its RECIPE files under recipes/src/recipes/ are routed by the
    // recipes arm above, not here.
    if pattern_matches("td-kexec/*|td-kexec/src/*|td-kexec/Cargo.toml|td-kexec/Cargo.lock", p) {
        sel.add_preflight("cargo-test");
        sel.add_target("check-pr");
        sel.add_target("recipe-checks-daily");
        return;
    }

    // Catch-all: an unmapped path used to require the FULL loop; it now runs
    // the bounded check-pr tier (the ~10-min per-PR budget) and leans on the
    // daily backstop for the daily-tier gates.
    sel.add_target("check-pr");
    sel.add_note(&format!(
        "No mapping for {p} — running the bounded check-pr tier; the daily backstop covers \
         the daily-tier gates. Update builder/src/affected.rs with a mapping for it."
    ));
}

// ---------------------------------------------------------------------------
// Rendering — byte-for-byte with the shell's stdout.
// ---------------------------------------------------------------------------

fn preflight_cmd(name: &str) -> Option<&'static str> {
    match name {
        "shell-syntax" => Some("  bash -n tests/*.sh ci/*.sh tools/*.sh"),
        "heal-revert" => Some("  bash tests/heal-revert.sh"),
        "cargo-test" => {
            Some("  cargo test + clippy --manifest-path {builder,recipes,td-kexec}/Cargo.toml")
        }
        "affected-self-test" => Some("  td-builder affected-checks --self-test"),
        _ => None,
    }
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

    if sel.preflights.is_empty() && sel.targets.is_empty() && sel.deferred.is_empty() {
        o.push_str("Selected checks: none (docs-only or ignored local metadata)\n");
    } else {
        o.push_str("Selected checks:\n");
        for pre in &sel.preflights {
            if let Some(cmd) = preflight_cmd(pre) {
                o.push_str(cmd);
                o.push('\n');
            }
        }
        if !sel.targets.is_empty() {
            o.push_str(&format!("  td-builder check {}\n", sel.targets.join(" ")));
        }
        if !sel.deferred.is_empty() {
            o.push_str(&format!(
                "Deferred to the daily backstop (daily/system tier — not run per-PR; \
                 `td-builder daily` heals regressions by fix-or-revert PR):\n  {}\n",
                sel.deferred.join(" ")
            ));
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

/// The gate names that run ONLY in the daily/system tiers (no membership in
/// any per-PR pool — gates.rs's `pool_runs_per_pr` is the single source of
/// that taxonomy). Such targets are honest members of the affected set but are
/// NOT run per-PR — the daily backstop covers them. Built once per selection.
fn daily_tier_only_names() -> HashSet<String> {
    use crate::gates::Pool;
    crate::gates::defs()
        .into_iter()
        .filter(|(_, d)| {
            !d.pools.iter().any(|p| crate::gates::pool_runs_per_pr(*p))
                && d.pools
                    .iter()
                    .any(|p| matches!(p, Pool::Daily | Pool::System))
        })
        .map(|(_, d)| d.name.to_string())
        .collect()
}

fn compute_selection(root: &Path, changed: &[String]) -> Selection {
    let mut sel = Selection::default();
    for p in changed {
        if !p.is_empty() {
            map_path(root, p, &mut sel);
        }
    }
    // The per-PR budget partition: daily/system-tier gates (and the
    // check-system tier itself) leave the run list and are reported as
    // deferred — the mapping arms stay honest about what a diff AFFECTS, the
    // partition decides what runs per-PR. targets is already deduped, so the
    // partition halves are too.
    let daily = daily_tier_only_names();
    let (run, defer): (Vec<String>, Vec<String>) = sel
        .targets
        .drain(..)
        .partition(|t| t != "check-system" && !daily.contains(t));
    sel.targets = run;
    sel.deferred = defer;
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
paths to focused gate targets (daily/system-tier gates are named but deferred
to the daily backstop — the ~10-min per-PR budget) and prints whether the full
check is waived or still required.
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

/// The targets on the "Deferred to the daily backstop" line (the indented line
/// right after the header).
fn deferred_targets(output: &str) -> Vec<String> {
    let mut lines = output.lines();
    while let Some(l) = lines.next() {
        if l.starts_with("Deferred to the daily backstop") {
            return lines
                .next()
                .map(|l| l.split_whitespace().map(str::to_string).collect())
                .unwrap_or_default();
        }
    }
    Vec::new()
}

pub fn run_self_test(root: &Path) -> Vec<String> {
    let mut failures: Vec<String> = Vec::new();
    let mut fail = |m: String| failures.push(m);

    // A mapping "selects" a target when it either RUNS it per-PR or names it on
    // the deferred-to-daily line — both prove the diff→gate table is right; the
    // run/defer split is the tier partition's job, asserted separately below.
    let has_target = |path: &str, target: &str| -> bool {
        let out = path_output(root, path);
        last_check_targets(&out).iter().any(|t| t == target)
            || deferred_targets(&out).iter().any(|t| t == target)
    };
    let runs_target = |path: &str, target: &str| -> bool {
        last_check_targets(&path_output(root, path))
            .iter()
            .any(|t| t == target)
    };
    let defers_target = |path: &str, target: &str| -> bool {
        deferred_targets(&path_output(root, path))
            .iter()
            .any(|t| t == target)
    };
    macro_rules! assert_target {
        ($path:expr, $target:expr) => {
            if !has_target($path, $target) {
                fail(format!(
                    "{}: expected ./check.sh target '{}'",
                    $path, $target
                ));
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
    macro_rules! assert_deferred {
        ($path:expr, $target:expr) => {
            if !defers_target($path, $target) {
                fail(format!(
                    "{}: expected DEFERRED-to-daily target '{}'",
                    $path, $target
                ));
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
    if default_check_covers_target(root, "check-system") {
        fail("default coverage: check-system is not covered by plain ./check.sh".into());
    }
    if !default_check_covers_target(root, "check-pr") {
        fail("default coverage: missing check-pr (a subset of the plain check)".into());
    }
    if !default_check_covers_target(root, "recipe-checks-daily") {
        fail("default coverage: daily gates are covered by the plain check".into());
    }

    // Every gate file maps (via the builder/src/gate_defs/*.rs arm) to its own gate target.
    for f in gate_files(root) {
        let rel = format!(
            "builder/src/gate_defs/{}",
            f.file_name().unwrap().to_string_lossy()
        );
        match target_from_gate_file(&f) {
            Some(gate) if !gate.is_empty() => assert_target!(&rel, &gate),
            _ => fail(format!("{rel}: no gate registration found")),
        }
    }

    // Every BUILD_GATE is selected by the build-phase arm (build-recipes is the
    // phase itself; cache-lib is its helper).
    for bg in build_gates(root) {
        assert_target!("tests/build-recipes.sh", &bg);
        assert_target!("tests/cache-lib.sh", &bg);
    }

    assert_target!("tests/recipe-checks.sh", "recipe-checks-daily");
    for tombstone in [
        "tests/chain-cache-lib.sh",
        "tests/chain-cache.sh",
        "tests/bootstrap-chain.sh",
        "tests/ladder-lib.sh",
        "tests/repro-lib.sh",
    ] {
        assert_target!(tombstone, "recipe-checks-daily");
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
    assert_target!("builder/src/gate_bodies.rs", "recipe-checks-daily");
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
    assert_target!("recipes/src/recipes/make-test.rs", "recipe-checks-daily");
    assert_target!("recipes/build.rs", "recipe-rs");
    assert_target!("recipes/Cargo.toml", "recipe-rs");
    assert_target!("builder/src/gate_defs/207-recipe-rs.rs", "recipe-rs");
    // The merged td-net gets the union of the former fetch/feed rules: the chain targets
    // (no gate builds it from source; its main.rs holds the warm-sources consumer smoked by
    // the i686 chain's proof set) AND the bounded check-pr tier (former fetch coverage) so a
    // net-ONLY diff still runs a per-PR gate rather than only daily-deferred chain targets.
    assert_target!("net/Cargo.lock", "recipe-checks-daily");
    assert_target!("net/src/main.rs", "recipe-checks-daily");
    assert_target!("net/src/fetch.rs", "check-pr");
    assert_target!("net/Cargo.toml", "check-pr");
    // td-kexec/src is include_str!'d into the target artifact, so a helper-source edit
    // rides the host cargo preflight (check-pr) AND is recorded as deferred to the daily
    // backstop (recipe-checks-daily statically links it via td-kexec-test).
    assert_target!("td-kexec/src/main.rs", "check-pr");
    assert_target!("td-kexec/src/main.rs", "recipe-checks-daily");
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
    assert_target!("recipes/src/source_pins.rs", "recipe-checks-daily");
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
    // The per-PR budget (human 2026-07-04): NOTHING escalates to the FULL loop.
    // The loop spine and unmapped paths validate on the bounded check-pr tier;
    // daily/system-tier gates are named but deferred.
    assert_runs!("builder/src/gates.rs", "check-pr");
    assert_branch_policy!("builder/src/gates.rs", "the full check would be waived");
    assert_runs!("new/unmapped.file", "check-pr");
    assert_branch_policy!("new/unmapped.file", "the full check would be waived");
    // The run/defer partition: a chain diff RUNS its PR-sized target (bootstrap-seed,
    // Heavy) and DEFERS the daily proof of the whole ladder (recipe-checks-daily, Daily —
    // #397: the per-rung bootstrap-gcc-mesboot gate this used to name is retired); a
    // catalog edit RUNS the recipe-engine gate per-PR.
    assert_runs!("seed/stage0/AMD64/hex0_AMD64.hex0", "bootstrap-seed");
    assert_deferred!("seed/stage0/AMD64/hex0_AMD64.hex0", "recipe-checks-daily");
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

fn git_lines(root: &Path, args: &[&str]) -> Vec<String> {
    let out = Command::new("git").args(args).current_dir(root).output();
    match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
            .lines()
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    }
}

fn git_ok(root: &Path, args: &[&str]) -> bool {
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
fn resolve_root() -> PathBuf {
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

fn run_preflight(root: &Path, name: &str) -> i32 {
    match name {
        "shell-syntax" => run_shell(root, "bash -n tests/*.sh ci/*.sh tools/*.sh"),
        "heal-revert" => run_shell(root, "bash tests/heal-revert.sh"),
        // BOTH engine crates, tests AND clippy: the AGENTS.md deny-lints only
        // fire under the clippy driver, and the in-loop cargo-test gate (325)
        // is unreachable while the loop is UNPROVISIONED (re #469) — this
        // host preflight is the per-PR enforcement in the meantime (review
        // finding: recipes tests + clippy ran in NO automated per-PR tier).
        "cargo-test" => {
            // td-kexec (the target-built guest kexec helper) rides the SAME preflight:
            // it is dependency-free pure std, so unlike fetch/feed/subst it compiles
            // offline, and its own `[lints]` deny-set (no unwrap/panic/indexing) must be
            // enforced per-PR too. The static TARGET link is daily/operator (td-kexec-test);
            // this leg is the host-native clippy/test of the same source.
            for cmd in [
                "cargo test --frozen --manifest-path builder/Cargo.toml",
                "cargo test --frozen --manifest-path recipes/Cargo.toml",
                "cargo test --frozen --manifest-path td-kexec/Cargo.toml",
                "cargo clippy --frozen --manifest-path builder/Cargo.toml",
                "cargo clippy --frozen --manifest-path recipes/Cargo.toml",
                "cargo clippy --frozen --manifest-path td-kexec/Cargo.toml",
            ] {
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
        _ => 0,
    }
}

pub fn main(args: &[String]) -> ExitCode {
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
                    return ExitCode::from(2);
                }
                base = args[i].clone();
            }
            "--path" => {
                i += 1;
                if i >= args.len() {
                    eprintln!("affected-checks: --path needs a path");
                    return ExitCode::from(2);
                }
                explicit_paths.push(args[i].clone());
            }
            "-h" | "--help" => {
                print!("{HELP}");
                return ExitCode::SUCCESS;
            }
            other => {
                eprintln!("affected-checks: unknown arg '{other}'");
                eprint!("{HELP}");
                return ExitCode::from(2);
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
            return ExitCode::SUCCESS;
        }
        eprintln!("affected-checks self-test: {} failure(s)", failures.len());
        return ExitCode::FAILURE;
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
                return ExitCode::from(2);
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
                return ExitCode::from(o.status.code().unwrap_or(1) as u8);
            }
            Err(e) => {
                eprintln!("affected-checks: git merge-base failed: {e}");
                return ExitCode::from(1);
            }
        }
        let mut all = git_lines(&root, &["diff", "--name-only", &merge_base, "HEAD"]);
        if !committed_only {
            all.extend(git_lines(&root, &["diff", "--name-only"]));
            all.extend(git_lines(&root, &["diff", "--cached", "--name-only"]));
            all.extend(git_lines(
                &root,
                &["ls-files", "--others", "--exclude-standard"],
            ));
        }
        sort_unique(all)
    };

    if changed.is_empty() {
        println!("affected-checks: no changed paths relative to {base}");
        return ExitCode::SUCCESS;
    }

    let sel = compute_selection(&root, &changed);
    let header = Header {
        explicit,
        base: &base,
        merge_base: &merge_base,
    };
    print!("{}", format_output(&header, &changed, &sel, run));

    if !run {
        return ExitCode::SUCCESS;
    }

    // --- execute ---
    for pre in &sel.preflights {
        let code = run_preflight(&root, pre);
        if code != 0 {
            return ExitCode::from(code as u8);
        }
    }

    // Nothing escalates to the full loop: every diff runs its bounded selected
    // targets; daily/system-tier gates it affects are named + deferred above.
    if !sel.targets.is_empty() {
        let code = run_self_check(&root, &sel.targets);
        // EXIT_UNPROVISIONED is the loop's documented "nothing could run
        // here" machine signal (the daily's LegRc treats it as PARTIAL with
        // no red bit). It is explained loudly here but PROPAGATED UNCHANGED —
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
                 preflights above are the per-PR coverage, the daily backstop records the \
                 gap as PARTIAL; exit code 69 propagated unchanged",
                sel.targets.join(" ")
            );
        }
        return ExitCode::from(code as u8);
    }

    ExitCode::SUCCESS
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

    /// The repo fixtures the self-test reads (`tests/` + the gate-def files) are present only
    /// when cargo runs from the full checkout — the `cargo-test` GATE and the required
    /// CI `cargo-test` job, both on every PR. The `td-builder` GUIX package build runs
    /// `cargo test` too, but its source is `local-file "../builder"` — ONLY the crate,
    /// no `tests/` or check.sh — so the self-test skips there (not a weakening: the gate
    /// + CI still run it fully every PR). Markers must be repo files OUTSIDE `builder/`.
    fn repo_tree_present(root: &Path) -> bool {
        root.join("builder/src/gate_defs").is_dir() && root.join("check.sh").is_file()
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
        if !repo_tree_present(&root) {
            eprintln!(
                "SKIP self-test: repo tree absent at {} (builder-only sandbox)",
                root.display()
            );
            return;
        }
        let failures = run_self_test(&root);
        assert!(failures.is_empty(), "self-test failures: {failures:#?}");
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
                "  cargo test + clippy --manifest-path {builder,recipes,td-kexec}/Cargo.toml",
                "  td-builder check check-engine",
                "",
                "Waiver: inspection only (--path does not prove the branch diff)",
                "Branch-mode policy for these paths: the full check would be waived",
                "",
                "Notes:",
                "  - builder/src/main.rs is the td-builder build engine: validated by the ~2-min check-engine smoke (compile + unit tests); the from-source build coverage is the DAILY backstop (DESIGN §7.2), not a per-PR gate.",
                "",
                "Dry run only. Re-run with --run to execute.",
            ])
        );

        // Loop spine → the bounded check-pr tier + engine tests (waived; the
        // daily backstop covers the daily tier — the per-PR budget, 2026-07-04).
        assert_eq!(
            path_output(&root, "check.sh"),
            expect(&[
                "affected-checks: explicit path mode",
                "",
                "Changed paths:",
                "  check.sh",
                "",
                "Selected checks:",
                "  bash -n tests/*.sh ci/*.sh tools/*.sh",
                "  cargo test + clippy --manifest-path {builder,recipes,td-kexec}/Cargo.toml",
                "  td-builder check check-pr",
                "",
                "Waiver: inspection only (--path does not prove the branch diff)",
                "Branch-mode policy for these paths: the full check would be waived",
                "",
                "Notes:",
                "  - check.sh touches the loop spine: validated by the bounded check-pr tier (the runner runs itself over the whole PR pool); the daily backstop covers the daily tier.",
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

        // Catch-all → the bounded check-pr tier (waived; the daily backstop
        // covers the daily-tier gates — the per-PR budget, 2026-07-04).
        assert_eq!(
            path_output(&root, "totally/unmapped/path.xyz"),
            expect(&[
                "affected-checks: explicit path mode",
                "",
                "Changed paths:",
                "  totally/unmapped/path.xyz",
                "",
                "Selected checks:",
                "  td-builder check check-pr",
                "",
                "Waiver: inspection only (--path does not prove the branch diff)",
                "Branch-mode policy for these paths: the full check would be waived",
                "",
                "Notes:",
                "  - No mapping for totally/unmapped/path.xyz — running the bounded check-pr tier; the daily backstop covers the daily-tier gates. Update builder/src/affected.rs with a mapping for it.",
                "",
                "Dry run only. Re-run with --run to execute.",
            ])
        );
    }
}

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

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned

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
    full_required: Vec<String>,
    /// Affected gates that are DAILY/SYSTEM-tier: named for the record but not
    /// run per-PR — ci/daily-full-suite.sh covers them nightly with
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
    fn require_full(&mut self, x: &str) {
        push_unique(&mut self.full_required, x);
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
    crate::gates::defs().into_iter().find(|(s, _)| *s == stem).map(|(_, d)| d)
}

fn stem_of(file: &Path) -> Option<String> {
    file.file_stem().and_then(|s| s.to_str()).map(str::to_string)
}

/// The gate target a def file maps to. Engine-only gates return None (the
/// check-engine smoke covers them — parity with the old extractor, which
/// scanned CHEAP/HEAVY/FAST/SYSTEM/PARKED and intentionally not ENGINE).
/// PARKED gates stay mapped: a parked gate (a human unhooked it pending a
/// tracked fix) remains an on-demand `./check.sh <gate>` target.
fn target_from_gate_file(file: &Path) -> Option<String> {
    let stem = stem_of(file)?;
    let def = def_for_stem(&stem)?;
    let mapped = def.pools.iter().any(|p| !matches!(p, crate::gates::Pool::Engine));
    if mapped {
        Some(def.name.to_string())
    } else {
        None
    }
}

/// The def file's spec list (the former `*_SPECS :=` extraction).
fn specs_in_file(file: &Path) -> Vec<String> {
    stem_of(file)
        .and_then(|s| def_for_stem(&s))
        .map(|d| d.specs.iter().map(|s| s.to_string()).collect())
        .unwrap_or_default()
}

fn build_gates(_root: &Path) -> Vec<String> {
    crate::gates::defs()
        .into_iter()
        .filter(|(_, d)| d.build_gate)
        .map(|(_, d)| d.name.to_string())
        .collect()
}

/// First gate whose `specs` contains `spec` → its target.
fn target_for_build_spec(_root: &Path, spec: &str) -> Option<String> {
    crate::gates::defs()
        .into_iter()
        .find(|(_, d)| d.specs.iter().any(|s| *s == spec))
        .map(|(_, d)| d.name.to_string())
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

fn map_recipe_spec(root: &Path, spec: &str, sel: &mut Selection) {
    match spec {
        "hello" => {
            sel.add_target("recipe-checks");
            sel.add_target("recipe-checks-daily");
        }
        "make" | "sed" | "grep" | "xz" | "diffutils" | "patch" | "file" | "coreutils"
        | "gawk" | "tar" | "findutils" | "bash" | "libsigsegv" | "libunistring"
        | "pcre2" | "ncurses" | "readline" => sel.add_target("recipe-checks-daily"),
        "td-builder" => sel.add_target("check-pr"),
        "td-vendor-demo" | "td-cmake-demo" | "td-fetch" => sel.add_target("recipe-checks"),
        "td-russh-demo" | "cat" | "eza" | "bat" | "sd" | "procs" | "fd" | "ripgrep"
        | "uutils" | "youki" => sel.add_target("recipe-checks-daily"),
        "td-feed" => sel.add_target("check-pr"),
        "td-subst" => sel.add_target("check-pr"),
        "pkg-config" => {
            sel.add_target("check-pr");
            sel.add_note("pkg-config is authored but not yet built by a td gate; running the bounded check-pr tier.");
        }
        _ => {
            if let Some(t) = target_for_build_spec(root, spec) {
                sel.add_target(&t);
            } else {
                sel.add_target("check-pr");
                sel.add_note(&format!(
                    "No recipe-specific mapping for '{spec}' — running the bounded check-pr tier; \
                     the daily backstop covers the daily-tier gates. Update builder/src/affected.rs \
                     with a mapping for it."
                ));
            }
        }
    }
}

// The i686 mesboot→store-native chain in dependency order. Each "chain" arm adds
// itself + everything downstream (a contiguous slice); x86_64 hangs off
// gcc-14/binutils-244/glibc-241, so the pure-i686-userland arms stop before it.
const CHAIN: [&str; 29] = [
    "bootstrap-seed",
    "bootstrap-cc",
    "bootstrap-mes",
    "bootstrap-mescc",
    "bootstrap-tcc",
    "bootstrap-make",
    "bootstrap-tools",
    "bootstrap-patch",
    "bootstrap-binutils",
    "bootstrap-gcc",
    "bootstrap-glibc",
    "bootstrap-gcc-mesboot0",
    "bootstrap-binutils-mesboot1",
    "bootstrap-make-mesboot",
    "bootstrap-gcc-core-mesboot1",
    "bootstrap-gcc-mesboot1",
    "bootstrap-binutils-gawk-mesboot",
    "bootstrap-glibc-mesboot",
    "bootstrap-gcc-mesboot",
    "bootstrap-toolchain-store-native",
    "bootstrap-glibc-shared-store-native",
    "bootstrap-gcc-mesboot-wrapper",
    "bootstrap-hello-userland",
    "bootstrap-binutils-244-store-native",
    "bootstrap-gcc-mesboot-494-store-native",
    "bootstrap-gcc-14-store-native",
    "bootstrap-glibc-241-store-native",
    "recipe-checks-daily",
    "bootstrap-x86_64-toolchain-store-native",
];

fn add_chain(sel: &mut Selection, start: usize, end: usize) {
    for t in &CHAIN[start..end] {
        sel.add_target(t);
    }
}

// ---------------------------------------------------------------------------
// map_path — the `case` ladder, arm-for-arm with the shell (first match wins).
// ---------------------------------------------------------------------------

fn map_path(root: &Path, p: &str, sel: &mut Selection) {
    // Ignored local metadata.
    if pattern_matches(".claude/*|.td-build-cache/*|builder/target/*", p) {
        return;
    }

    if pattern_matches("check.sh|builder/build.rs|builder/src/gates.rs|builder/src/check_loop.rs", p) {
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
    if p == "tests/store-relocate.sh" {
        sel.add_target("store-relocate");
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
            "store-relocate",
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

    if pattern_matches(
        "recipes/*|recipes/src/*|recipes/Cargo.toml|recipes/Cargo.lock|tests/recipes-meta.json|tests/recipe-emit.sh|tests/recipe-eval-tool.sh",
        p,
    ) {
        // The td-recipe crate IS the package + system-spec surface (boa/TS retired).
        // It feeds the corpus build path (cache-lib emits via td-recipe-eval) — so a
        // catalog change can affect ANY built package. Run recipe-rs (self-consistency
        // + manifest sync) and the package build gates. (spec-diff retired with the
        // museum tier; the guix-dependence census retired with the guix-oracle gates.)
        sel.add_preflight("shell-syntax");
        sel.add_target("recipe-rs");
        add_build_gate_targets(root, sel);
        return;
    }

    if pattern_matches("fetch/*|fetch/src/*|fetch/Cargo.toml|fetch/Cargo.lock", p) {
        // recipe-checks builds td-fetch from the warmed vendor dir. (The td-feed
        // gate, the other consumer, retired with the guix-invoking gates.)
        sel.add_target("recipe-checks");
        return;
    }

    if pattern_matches("feed/*|feed/src/*|feed/Cargo.toml|feed/Cargo.lock", p) {
        // main.rs holds the host-PREP warm that feeds the corpus + bootstrap gates,
        // so smoke a representative consumer of each warm family: recipe-checks-daily
        // (`warm crate`) and bootstrap-glibc (`warm sources` + `warm kernel-headers`).
        // (The td-feed / feed-shared gates retired with the guix-invoking gates.)
        sel.add_target("recipe-checks-daily");
        sel.add_target("bootstrap-glibc");
        return;
    }

    if p == "tests/toolchain-input-addressed.sh" {
        sel.add_preflight("shell-syntax");
        sel.add_target("toolchain-input-addressed");
        return;
    }

    if p == "tests/td-toolchain-x86_64.lock" {
        sel.add_preflight("shell-syntax");
        sel.add_target("toolchain-x86_64-input-addressed");
        sel.add_target("bootstrap-x86_64-toolchain-store-native");
        return;
    }

    if p == "tests/td-toolchain-rust-x86_64.lock" {
        sel.add_preflight("shell-syntax");
        // the RELINKED rust tree's input-addressed lock: its consumer is the rust runtime
        // gate (which assembles+relinks+publishes it). A pin/recipe-rev change re-keys +
        // re-publishes the tree. (The rust-userland-x86_64 gate retired with the guix gates.)
        sel.add_target("rust-x86_64-runtime-store-native");
        return;
    }

    if pattern_matches(
        "tests/toolchain-x86_64-input-addressed.sh|builder/src/gate_defs/418-toolchain-x86_64-input-addressed.rs",
        p,
    ) {
        sel.add_preflight("shell-syntax");
        sel.add_target("toolchain-x86_64-input-addressed");
        return;
    }

    if p == "tests/td-toolchain.lock" {
        sel.add_preflight("shell-syntax");
        sel.add_target("toolchain-input-addressed");
        sel.add_target("toolchain-subst-default");
        sel.add_target("toolchain-x86_64-input-addressed");
        return;
    }

    if pattern_matches(
        "tests/toolchain-subst-default.sh|tools/resolve-toolchain.sh|tools/publish-toolchain-subst.sh|tests/td-subst.pub",
        p,
    ) {
        sel.add_preflight("shell-syntax");
        sel.add_target("toolchain-subst-default");
        return;
    }

    // The guix-less-runner harness shipping mechanism (#314): the consumer resolver, the daily's
    // producer, and the gate that drives them. run_check_harness (check_loop.rs, the spine) also
    // calls resolve-harness.sh, but a spine touch already escalates to the full check.
    if pattern_matches(
        "tests/harness-subst.sh|tools/resolve-harness.sh|tools/publish-harness-subst.sh",
        p,
    ) {
        sel.add_preflight("shell-syntax");
        sel.add_target("harness-subst");
        return;
    }


    if glob_match("tests/*-no-guix.lock", p) {
        let spec = p.strip_prefix("tests/").unwrap_or(p);
        let spec = spec.strip_suffix("-no-guix.lock").unwrap_or(spec);
        map_recipe_spec(root, spec, sel);
        return;
    }

    if pattern_matches(
        "tests/td-vendor-demo.lock|tests/td-vendor-demo-source.scm|tests/vendor-demo/*|tests/vendor-demo/src/*",
        p,
    ) {
        sel.add_target("recipe-checks");
        return;
    }

    if pattern_matches("tests/td-russh-demo.lock|tests/td-russh-demo-source.scm|tests/russh-demo/*", p) {
        sel.add_target("recipe-checks-daily");
        return;
    }

    if pattern_matches("tests/td-cmake-demo.lock|tests/cmake-demo/*", p) {
        sel.add_target("recipe-checks");
        return;
    }

    if p == "tests/cat-uutils.lock" {
        sel.add_target("recipe-checks-daily");
        return;
    }
    if p == "tests/eza.lock" {
        sel.add_target("recipe-checks-daily");
        return;
    }
    if p == "tests/bat.lock" {
        sel.add_target("recipe-checks-daily");
        return;
    }
    if p == "tests/sd.lock" {
        sel.add_target("recipe-checks-daily");
        return;
    }
    if p == "tests/procs.lock" {
        sel.add_target("recipe-checks-daily");
        return;
    }
    if p == "tests/fd.lock" {
        sel.add_target("recipe-checks-daily");
        return;
    }
    if p == "tests/ripgrep.lock" {
        sel.add_target("recipe-checks-daily");
        return;
    }
    if p == "tests/uutils-coreutils.lock" {
        sel.add_target("recipe-checks-daily");
        return;
    }
    if p == "tests/youki.lock" {
        sel.add_target("recipe-checks-daily");
        return;
    }
    if p == "tests/td-fetch.lock" {
        sel.add_target("recipe-checks");
        return;
    }
    if p == "tests/crate-free-build.sh" {
        sel.add_preflight("shell-syntax");
        sel.add_target("recipe-checks-daily");
        return;
    }

    if pattern_matches("tests/recipe-checks.sh|tests/recipe-check-lib.sh", p) {
        sel.add_preflight("shell-syntax");
        sel.add_target("recipe-checks");
        sel.add_target("recipe-checks-daily");
        return;
    }

    if p == "tests/intern-src.sh" {
        sel.add_preflight("shell-syntax");
        sel.add_target("check-pr");
        sel.add_target("recipe-checks-daily");
        return;
    }

    // tests/build-recipes.sh IS the build phase (the former Makefile build-recipes
    // recipe, run by the gate runner) — a change to it affects every build gate,
    // exactly like the build-phase helpers below.
    if pattern_matches(
        "tests/build-recipes.sh|tests/build-pkg.sh|tests/cache-lib.sh|tests/stage0-builder.sh",
        p,
    ) {
        sel.add_preflight("shell-syntax");
        add_build_gate_targets(root, sel);
        return;
    }

    if p == "tests/sandbox-hardening.sh" {
        sel.add_preflight("shell-syntax");
        sel.add_target("sandbox-hardening");
        return;
    }

    if p == "tests/td-shell.sh" {
        sel.add_preflight("shell-syntax");
        sel.add_target("td-shell");
        return;
    }

    if p == "tests/td-shell-userland.sh" {
        sel.add_preflight("shell-syntax");
        sel.add_target("td-shell-userland");
        return;
    }

    if p == "tests/td-shell-seed.sh" {
        sel.add_preflight("shell-syntax");
        sel.add_target("td-shell-seed");
        return;
    }

    if p == "tests/profile.sh" {
        sel.add_preflight("shell-syntax");
        sel.add_target("profile");
        return;
    }

    // bootstrap-seed / bootstrap-mes have NO shell driver — they are STRUCTURED Rust
    // recipes (`td-builder bootstrap-recipe {seed,mes}`, builder/src/bootstrap.rs,
    // rust-migration C2; the old tests/bootstrap-{seed,mes}.sh were deleted). A
    // recipe-code change routes via the builder/src/* arm above (check-engine smoke +
    // the cargo-test preflight, which builds brick 0 end-to-end on every PR); the seed
    // tree (seed/stage0/*) and the mes lock (seed/sources/mes-*.lock) route to these
    // gates via the chain arms below.
    if p == "tests/bootstrap-cc.sh" {
        sel.add_preflight("shell-syntax");
        sel.add_target("bootstrap-cc");
        return;
    }
    if p == "tests/bootstrap-mescc.sh" {
        sel.add_preflight("shell-syntax");
        sel.add_target("bootstrap-mescc");
        return;
    }
    if p == "tests/bootstrap-tcc.sh" {
        sel.add_preflight("shell-syntax");
        sel.add_target("bootstrap-tcc");
        return;
    }
    if p == "tests/bootstrap-make.sh" {
        sel.add_preflight("shell-syntax");
        sel.add_target("bootstrap-make");
        return;
    }

    if pattern_matches(
        "tests/bootstrap-tools.sh|seed/sources/gzip-*.lock|seed/sources/tcc-0.9.27*.lock",
        p,
    ) {
        sel.add_preflight("shell-syntax");
        sel.add_target("bootstrap-tools");
        return;
    }

    if pattern_matches("tests/rust-store-native.sh|seed/sources/rust-*.lock", p) {
        sel.add_preflight("shell-syntax");
        sel.add_target("rust-store-native");
        sel.add_target("rust-x86_64-runtime-store-native");
        return;
    }

    if pattern_matches(
        "tests/rust-x86_64-runtime-store-native.sh|seed/sources/zlib-*.lock|builder/src/gate_defs/416-rust-x86_64-runtime-store-native.rs",
        p,
    ) {
        sel.add_preflight("shell-syntax");
        sel.add_target("rust-x86_64-runtime-store-native");
        return;
    }

    if pattern_matches(
        "tests/userland-x86_64-store-native.sh|seed/sources/busybox-*.lock|seed/sources/make-4.4*.lock|builder/src/gate_defs/420-userland-x86_64-store-native.rs",
        p,
    ) {
        sel.add_preflight("shell-syntax");
        sel.add_target("userland-x86_64-store-native");
        // Gate 420 also PERSISTS the /td/store harness the guix-free tier consumes
        // (host-sandbox-stage0 inc2c). `check-harness` is a check.sh-intercepted tier
        // (its own container), not a make gate, so it cannot join the other ./check.sh
        // targets — run it as its own invocation after provisioning.
        sel.add_note("run `td-builder check check-harness` separately to validate the guix-free /td/store harness tier (host-sandbox-stage0 inc2c) — it consumes the harness gate 420 persisted.");
        return;
    }

    // The guix-free harness loop (host-sandbox-stage0 inc2c): mk/harness.mk + the inner
    // loop body run by `./check.sh check-harness`. The tier consumes the harness gate 420
    // persists, so provision it via gate 420; `check-harness` is a check.sh tier (its own
    // container, not a joinable make gate) and is run as a separate invocation.
    if pattern_matches("tests/harness-loop.sh|mk/harness.mk", p) {
        sel.add_preflight("shell-syntax");
        sel.add_target("userland-x86_64-store-native");
        sel.add_note("run `td-builder check check-harness` separately to validate the guix-free /td/store harness tier (host-sandbox-stage0 inc2c).");
        return;
    }

    // --- the i686 mesboot→store-native chain: each rung + everything downstream ---
    if pattern_matches("tests/bootstrap-patch.sh|seed/sources/patch-*.lock", p) {
        sel.add_preflight("shell-syntax");
        add_chain(sel, 7, 29);
        return;
    }
    if pattern_matches(
        "tests/bootstrap-binutils.sh|seed/sources/binutils-2.20*.lock|seed/patches/binutils-boot-*.patch",
        p,
    ) {
        sel.add_preflight("shell-syntax");
        add_chain(sel, 8, 29);
        return;
    }
    if pattern_matches(
        "tests/bootstrap-gcc.sh|seed/sources/gcc-core-2.95.3.lock|seed/patches/gcc-boot-2.95.3.patch",
        p,
    ) {
        sel.add_preflight("shell-syntax");
        add_chain(sel, 9, 29);
        return;
    }
    if pattern_matches(
        "tests/bootstrap-glibc.sh|seed/sources/glibc-2.2.5.lock|seed/sources/linux-*.lock|seed/patches/glibc-boot-2.2.5.patch|seed/patches/glibc-bootstrap-system-2.2.5.patch",
        p,
    ) {
        sel.add_preflight("shell-syntax");
        add_chain(sel, 10, 29);
        return;
    }
    if p == "tests/bootstrap-gcc-mesboot0.sh" {
        sel.add_preflight("shell-syntax");
        add_chain(sel, 11, 12);
        return;
    }
    if p == "tests/bootstrap-binutils-mesboot1.sh" {
        sel.add_preflight("shell-syntax");
        add_chain(sel, 12, 13);
        return;
    }
    if pattern_matches("tests/bootstrap-make-mesboot.sh|seed/sources/make-3.82.lock", p) {
        sel.add_preflight("shell-syntax");
        add_chain(sel, 13, 14);
        return;
    }
    if pattern_matches(
        "tests/bootstrap-gcc-core-mesboot1.sh|seed/sources/gcc-core-4.6.4.lock|seed/sources/gmp-*.lock|seed/sources/mpfr-*.lock|seed/sources/mpc-*.lock|seed/patches/gcc-boot-4.6.4.patch",
        p,
    ) {
        sel.add_preflight("shell-syntax");
        add_chain(sel, 14, 29);
        return;
    }
    if pattern_matches("tests/bootstrap-gcc-mesboot1.sh|seed/sources/gcc-g++-4.6.4.lock", p) {
        sel.add_preflight("shell-syntax");
        add_chain(sel, 15, 29);
        return;
    }
    if pattern_matches("tests/bootstrap-binutils-gawk-mesboot.sh|seed/sources/gawk-*.lock", p) {
        sel.add_preflight("shell-syntax");
        add_chain(sel, 16, 29);
        return;
    }
    if pattern_matches(
        "tests/bootstrap-glibc-mesboot.sh|seed/sources/glibc-mesboot-2.16.0.lock|seed/patches/glibc-boot-2.16.0.patch|seed/patches/glibc-bootstrap-system-2.16.0.patch",
        p,
    ) {
        sel.add_preflight("shell-syntax");
        add_chain(sel, 17, 29);
        return;
    }
    if pattern_matches("tests/bootstrap-gcc-mesboot.sh|seed/sources/gcc-4.9.4.lock", p) {
        sel.add_preflight("shell-syntax");
        add_chain(sel, 18, 29);
        return;
    }
    if p == "tests/bootstrap-toolchain-store-native.sh" {
        sel.add_preflight("shell-syntax");
        add_chain(sel, 19, 28); // pure i686 userland — stops before x86_64
        return;
    }
    if p == "tests/bootstrap-glibc-shared-store-native.sh" {
        sel.add_preflight("shell-syntax");
        add_chain(sel, 20, 28);
        return;
    }
    if p == "tests/bootstrap-gcc-mesboot-wrapper.sh" {
        sel.add_preflight("shell-syntax");
        add_chain(sel, 21, 28);
        return;
    }
    if pattern_matches("tests/bootstrap-hello-userland.sh|seed/sources/hello-2.10.lock", p) {
        sel.add_preflight("shell-syntax");
        add_chain(sel, 22, 28);
        return;
    }
    if p == "tests/repro-lib.sh" {
        // shared reproducibility normalization — exercise the modern repro rungs.
        sel.add_preflight("shell-syntax");
        add_chain(sel, 23, 27);
        return;
    }
    if pattern_matches("tests/bootstrap-binutils-244-store-native.sh|seed/sources/binutils-2.44.lock", p) {
        sel.add_preflight("shell-syntax");
        add_chain(sel, 23, 29);
        return;
    }
    if p == "tests/bootstrap-gcc-mesboot-494-store-native.sh" {
        sel.add_preflight("shell-syntax");
        add_chain(sel, 24, 28);
        return;
    }
    if pattern_matches(
        "tests/bootstrap-gcc-14-store-native.sh|seed/sources/gcc-14.3.0.lock|seed/sources/gcc14-*.lock",
        p,
    ) {
        sel.add_preflight("shell-syntax");
        add_chain(sel, 25, 29);
        return;
    }
    if pattern_matches("tests/bootstrap-glibc-241-store-native.sh|seed/sources/glibc-2.41.lock", p) {
        sel.add_preflight("shell-syntax");
        add_chain(sel, 26, 29);
        return;
    }
    if pattern_matches("tests/bootstrap-hello-corpus-store-native.sh|seed/sources/hello-2.12.2.lock", p) {
        sel.add_preflight("shell-syntax");
        sel.add_target("recipe-checks-daily");
        return;
    }
    // bootstrap-chain.sh is the SHARED from-seed toolchain chain; its consumers are the sed
    // corpus gate, the hello corpus gate (#327), and store-persist (other store-native gates
    // can migrate to it later, each adding itself here). Since #317 the chain's bricks persist
    // through the warm chain-brick cache (tests/chain-cache-lib.sh), so a chain/lib change also
    // re-proves the chain-cache gate (hit/poison/cold semantics). (hello-corpus's OWN-file arm
    // is above; here it is a chain CONSUMER, re-proved when the shared chain changes. chain arm
    // ported from PR #203's affected-checks.sh.)
    if pattern_matches("tests/bootstrap-sed-corpus-store-native.sh|tests/bootstrap-chain.sh", p) {
        sel.add_preflight("shell-syntax");
        sel.add_target("recipe-checks-daily");
        sel.add_target("store-persist");
        sel.add_target("chain-cache");
        return;
    }
    // The warm chain-brick cache itself (#317): the chain-cache gate drives the real lib's
    // hit/build/save/poison/cold paths; the chain consumers exercise the reuse path in anger.
    if pattern_matches("tests/chain-cache.sh|tests/chain-cache-lib.sh", p) {
        sel.add_preflight("shell-syntax");
        sel.add_target("chain-cache");
        sel.add_target("store-persist");
        return;
    }
    // The rung-X2 native gcc gate's consumer test: a native x86_64 gcc/binutils built on top of the
    // cross toolchain. Maps only to gate 422 (the native build is downstream of the cross rungs). The
    // gate's gate_defs/422-*.rs file is already handled by the generic gate_defs arm above.
    if pattern_matches("tests/bootstrap-x86_64-native-gcc-store-native.sh", p) {
        sel.add_preflight("shell-syntax");
        sel.add_target("bootstrap-x86_64-native-gcc-store-native");
        return;
    }
    // The rung-X3 self-hosting gate's own driver (gcc-rebuilds-gcc, #298): maps only to gate 426.
    if pattern_matches("tests/bootstrap-x86_64-self-gcc-store-native.sh", p) {
        sel.add_preflight("shell-syntax");
        sel.add_target("bootstrap-x86_64-self-gcc-store-native");
        return;
    }
    // The NATIVE x86_64 toolchain's input-addressed key file: consumed by the native gcc gate (422,
    // builds+interns the native toolchain at these lock paths), the self-host gate (426, obtains the
    // native toolchain as its builder), and the rust runtime gate (416, fetches it as the linker) —
    // all Daily/system tier, deferred to the daily backstop. (A recipe-rev bump here re-keys the path.)
    if pattern_matches("tests/td-toolchain-x86_64-native.lock", p) {
        sel.add_target("bootstrap-x86_64-native-gcc-store-native");
        sel.add_target("bootstrap-x86_64-self-gcc-store-native");
        sel.add_target("rust-x86_64-runtime-store-native");
        return;
    }
    if pattern_matches(
        "tests/bootstrap-x86_64-toolchain-store-native.sh|tests/x86_64-cross-fns.sh|tests/x86_64-subst-lib.sh|builder/src/gate_defs/414-bootstrap-x86_64-toolchain-store-native.rs",
        p,
    ) {
        sel.add_preflight("shell-syntax");
        add_chain(sel, 28, 29);
        // x86_64-cross-fns.sh also defines the rung-X2 native driver (run_x86_64_native), the shared
        // fetch-or-build obtainers, and the rung-X3 self-host fns — so a change to it must re-run the
        // native gcc gate AND the self-host gate, not only the cross toolchain gate.
        sel.add_target("bootstrap-x86_64-native-gcc-store-native");
        sel.add_target("bootstrap-x86_64-self-gcc-store-native");
        return;
    }

    if glob_match("seed/sources/make-*.lock", p) {
        sel.add_preflight("shell-syntax");
        add_chain(sel, 5, 29);
        return;
    }
    if glob_match("seed/sources/tcc-0.9.26*.lock", p) {
        sel.add_preflight("shell-syntax");
        add_chain(sel, 4, 29);
        return;
    }
    if glob_match("seed/sources/nyacc-*.lock", p) {
        sel.add_preflight("shell-syntax");
        add_chain(sel, 3, 29);
        return;
    }
    if pattern_matches("seed/sources/mes-*.lock", p) {
        sel.add_preflight("shell-syntax");
        add_chain(sel, 2, 29);
        return;
    }
    if glob_match("seed/stage0/*", p) {
        sel.add_preflight("shell-syntax");
        add_chain(sel, 0, 29);
        return;
    }

    if p == "tests/store-native-profile.sh" {
        sel.add_preflight("shell-syntax");
        sel.add_target("store-native-profile");
        return;
    }

    if pattern_matches("tests/seed-tarball.sh|tools/build-seed-tarball.sh", p) {
        sel.add_preflight("shell-syntax");
        sel.add_target("seed-tarball");
        return;
    }
    if p == "tests/seed-unpack.sh" {
        sel.add_preflight("shell-syntax");
        sel.add_target("seed-unpack");
        return;
    }
    if pattern_matches("tests/seed-build.sh|tools/warm-seed.sh|tests/td-seed.lock", p) {
        sel.add_preflight("shell-syntax");
        sel.add_target("seed-build");
        return;
    }
    if p == "tests/store-persist.sh" {
        sel.add_preflight("shell-syntax");
        sel.add_target("store-persist");
        return;
    }
    if p == "tests/corpus-seed.sh" {
        sel.add_preflight("shell-syntax");
        sel.add_target("corpus-seed");
        return;
    }
    if p == "tests/rust-seed.sh" {
        sel.add_preflight("shell-syntax");
        sel.add_target("rust-seed");
        return;
    }
    if p == "tests/harness-seed.sh" {
        sel.add_preflight("shell-syntax");
        sel.add_target("harness-seed");
        return;
    }


    if p == "tests/recipe-rs.sh" {
        sel.add_preflight("shell-syntax");
        sel.add_target("recipe-rs");
        return;
    }
    if p == "tests/heal-revert.sh" {
        // CI-lint-only test of the heal primitive — git is absent from the loop
        // sandbox, so it is not a ./check.sh gate; shell-syntax suffices locally.
        sel.add_preflight("shell-syntax");
        return;
    }

    if pattern_matches(
        "ci/build-ci-image.sh|ci/import-store.sh|ci/lower-*.sh|.github/setup-branch-protection.sh|.github/workflows/*",
        p,
    ) {
        // CI/runner-gating files used to escalate to the full local loop; the
        // local loop never exercises hosted CI, so the honest local check is the
        // syntax preflight — the workflow run after push is the real test.
        sel.add_preflight("shell-syntax");
        sel.add_note(&format!(
            "{p} affects CI or branch protection; inspect the workflow result after push."
        ));
        return;
    }

    // (The td-builder SEED-build resolver scripts tools/provision-{rust,cc}.sh +
    // tools/bootstrap-td-builder.sh and tests/provision-{rust,cc}.sh fall through to
    // the shell-syntax preflight below — their gates (bootstrap/provision-rust/
    // provision-cc) retired with the guix-invoking gates.)
    if pattern_matches("ci/*.sh|tools/*.sh", p) {
        sel.add_preflight("shell-syntax");
        return;
    }

    if pattern_matches("ci/*|.github/workflows/*|.github/*", p) {
        sel.add_preflight("shell-syntax");
        sel.add_note(&format!(
            "{p} affects CI or branch protection; inspect the workflow result after push."
        ));
        return;
    }

    if pattern_matches("*.md|DESIGN.md|CLAUDE.md|.gitignore", p) {
        return; // docs — no checks
    }

    if p == "channels.scm" {
        sel.add_target("check-fast");
        sel.require_full(&format!(
            "{p} changed; the dependency pin affects the whole loop."
        ));
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
        "shell-syntax" => {
            Some("  bash -n check.sh tests/*.sh ci/*.sh tools/*.sh .github/setup-branch-protection.sh")
        }
        "cargo-test" => Some("  cargo test --manifest-path builder/Cargo.toml"),
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
                 ci/daily-full-suite.sh heals regressions by fix-or-revert PR):\n  {}\n",
                sel.deferred.join(" ")
            ));
        }
    }

    o.push('\n');
    if header.explicit {
        o.push_str("Waiver: inspection only (--path does not prove the branch diff)\n");
        if sel.full_required.is_empty() {
            o.push_str("Branch-mode policy for these paths: the full check would be waived\n");
        } else {
            o.push_str("Branch-mode policy for these paths: the full check would be required\n");
            for n in &sel.full_required {
                o.push_str(&format!("  - {n}\n"));
            }
        }
    } else if sel.full_required.is_empty() {
        o.push_str("Waiver: the full check waived by affected-checks for this diff\n");
    } else {
        o.push_str("Waiver: the full check required before marking ready\n");
        for n in &sel.full_required {
            o.push_str(&format!("  - {n}\n"));
        }
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
                && d.pools.iter().any(|p| matches!(p, Pool::Daily | Pool::System))
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
    let header = Header { explicit: true, base: "origin/main", merge_base: "" };
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
        last_check_targets(&path_output(root, path)).iter().any(|t| t == target)
    };
    let defers_target = |path: &str, target: &str| -> bool {
        deferred_targets(&path_output(root, path)).iter().any(|t| t == target)
    };
    macro_rules! assert_target {
        ($path:expr, $target:expr) => {
            if !has_target($path, $target) {
                fail(format!("{}: expected ./check.sh target '{}'", $path, $target));
            }
        };
    }
    macro_rules! assert_runs {
        ($path:expr, $target:expr) => {
            if !runs_target($path, $target) {
                fail(format!("{}: expected PER-PR (run) target '{}'", $path, $target));
            }
        };
    }
    macro_rules! assert_deferred {
        ($path:expr, $target:expr) => {
            if !defers_target($path, $target) {
                fail(format!("{}: expected DEFERRED-to-daily target '{}'", $path, $target));
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
    if !default_check_covers_target(root, "bootstrap-gcc-mesboot") {
        fail("default coverage: daily gates are covered by the plain check".into());
    }

    // Every gate file maps (via the builder/src/gate_defs/*.rs arm) to its own gate target.
    for f in gate_files(root) {
        let rel = format!("builder/src/gate_defs/{}", f.file_name().unwrap().to_string_lossy());
        match target_from_gate_file(&f) {
            Some(gate) if !gate.is_empty() => assert_target!(&rel, &gate),
            _ => fail(format!("{rel}: no gate registration found")),
        }
    }


    // Every BUILD_GATE is selected by the build-phase arm (build-recipes is the
    // phase itself; build-pkg/cache-lib are its helpers).
    for bg in build_gates(root) {
        assert_target!("tests/build-recipes.sh", &bg);
        assert_target!("tests/build-pkg.sh", &bg);
        assert_target!("tests/cache-lib.sh", &bg);
    }

    // The warm chain-brick cache (#317): the lib and its gate map to chain-cache, and
    // the shared chain re-proves BOTH consumers + the cache gate.
    assert_target!("tests/chain-cache-lib.sh", "chain-cache");
    assert_target!("tests/chain-cache.sh", "chain-cache");
    assert_target!("tests/bootstrap-chain.sh", "store-persist");
    assert_target!("tests/bootstrap-chain.sh", "chain-cache");
    // The store-native corpus recipe checks source the shared chain, so a chain
    // change must re-prove the daily recipe-check wrapper too.
    assert_target!("tests/bootstrap-chain.sh", "recipe-checks-daily");
    assert_target!(
        "tests/bootstrap-hello-corpus-store-native.sh",
        "recipe-checks-daily"
    );

    // Spec→gate routing: a recipe/lock for a gate's SPEC selects that gate.
    for f in gate_files(root) {
        let gate = match target_from_gate_file(&f) {
            Some(g) if !g.is_empty() => g,
            _ => continue,
        };
        for spec in specs_in_file(&f) {
            let lock = format!("tests/{spec}-no-guix.lock");
            if root.join(&lock).is_file() {
                assert_target!(&lock, &gate);
            }
        }
    }

    // A gate-file change still selects the dispatcher's own self-test preflight
    // (now the in-process `td-builder affected-checks --self-test`) and is waived.
    assert_contains!("builder/src/gate_defs/325-cargo-test.rs", "td-builder affected-checks --self-test");
    assert_branch_policy!("builder/src/gate_defs/325-cargo-test.rs", "the full check would be waived");
    assert_target!("tests/repro-lib.sh", "bootstrap-binutils-244-store-native");
    assert_branch_policy!("tests/repro-lib.sh", "the full check would be waived");
    // Native (typed-Rust) gate bodies (#318 axis 3): a body change runs its gates
    // (the former tests/store-*.sh / gate-script mapping) + the engine smoke.
    assert_target!("builder/src/gate_bodies.rs", "store-register");
    assert_target!("builder/src/gate_bodies.rs", "store-ns");
    assert_target!("builder/src/gate_bodies.rs", "store-relocate");
    assert_target!("builder/src/gate_bodies.rs", "check-engine");
    // The Rust td-recipe crate IS the package + spec surface (boa/TS retired): a
    // catalog edit runs recipe-rs and the package build gates.
    assert_target!("recipes/src/catalog.rs", "recipe-rs");
    assert_target!("recipes/src/catalog.rs", "recipe-checks");
    assert_target!("recipes/src/catalog.rs", "recipe-checks-daily");
    // Recipes are one self-registering file each under src/recipes/ (issue #295);
    // the nested path must select the same gates (glob `*` crosses `/`).
    assert_target!("recipes/src/recipes/hello.rs", "recipe-rs");
    assert_target!("recipes/src/recipes/hello.rs", "recipe-checks");
    assert_target!("recipes/src/recipes/hello.rs", "recipe-checks-daily");
    assert_target!("recipes/build.rs", "recipe-rs");
    assert_target!("recipes/Cargo.toml", "recipe-rs");
    assert_target!("tests/recipe-rs.sh", "recipe-rs");
    assert_target!("tests/recipes-meta.json", "recipe-rs");
    assert_target!("tests/td-russh-demo.lock", "recipe-checks-daily");
    assert_target!("tests/russh-demo/Cargo.lock", "recipe-checks-daily");
    // td-fetch's crate-closure warm is native in check_loop.rs: the lock it parses
    // maps to recipe-checks (which consumes the warmed vendor dir). (The td-feed gate,
    // the other consumer, retired with the guix-invoking gates.)
    assert_target!("fetch/Cargo.lock", "recipe-checks");
    // A feed/src change smokes a representative consumer of each warm family:
    // recipe-checks-daily (`warm crate`) and bootstrap-glibc (`warm sources`).
    // (td-feed / feed-shared / seed-subst retired with the guix-invoking gates.)
    assert_target!("feed/src/main.rs", "recipe-checks-daily");
    assert_target!("feed/src/main.rs", "bootstrap-glibc");
    assert_target!("tests/toolchain-subst-default.sh", "toolchain-subst-default");
    assert_target!("tools/resolve-toolchain.sh", "toolchain-subst-default");
    assert_target!("tools/publish-toolchain-subst.sh", "toolchain-subst-default");
    assert_target!("tests/td-subst.pub", "toolchain-subst-default");
    assert_target!("tests/td-toolchain.lock", "toolchain-subst-default");
    assert_target!("tests/td-toolchain.lock", "toolchain-input-addressed");
    assert_target!("tests/td-toolchain.lock", "toolchain-x86_64-input-addressed");
    assert_target!("tests/td-toolchain-x86_64.lock", "toolchain-x86_64-input-addressed");
    assert_target!("tests/td-toolchain-x86_64.lock", "bootstrap-x86_64-toolchain-store-native");
    assert_target!("tests/x86_64-subst-lib.sh", "bootstrap-x86_64-toolchain-store-native");
    assert_target!(
        "tests/bootstrap-x86_64-native-gcc-store-native.sh",
        "bootstrap-x86_64-native-gcc-store-native"
    );
    assert_target!(
        "tests/bootstrap-x86_64-self-gcc-store-native.sh",
        "bootstrap-x86_64-self-gcc-store-native"
    );
    assert_target!("tests/x86_64-cross-fns.sh", "bootstrap-x86_64-self-gcc-store-native");
    assert_target!(
        "builder/src/gate_defs/426-bootstrap-x86_64-self-gcc-store-native.rs",
        "bootstrap-x86_64-self-gcc-store-native"
    );
    assert_target!(
        "builder/src/gate_defs/422-bootstrap-x86_64-native-gcc-store-native.rs",
        "bootstrap-x86_64-native-gcc-store-native"
    );
    assert_target!("tests/x86_64-cross-fns.sh", "bootstrap-x86_64-native-gcc-store-native");
    assert_target!("tests/toolchain-x86_64-input-addressed.sh", "toolchain-x86_64-input-addressed");
    assert_target!(
        "builder/src/gate_defs/418-toolchain-x86_64-input-addressed.rs",
        "toolchain-x86_64-input-addressed"
    );
    assert_target!(
        "tests/userland-x86_64-store-native.sh",
        "userland-x86_64-store-native"
    );
    assert_target!("seed/sources/busybox-1.37.0.lock", "userland-x86_64-store-native");
    assert_target!("seed/sources/make-4.4.1.lock", "userland-x86_64-store-native");
    assert_target!(
        "builder/src/gate_defs/420-userland-x86_64-store-native.rs",
        "userland-x86_64-store-native"
    );
    assert_target!("tests/td-cmake-demo.lock", "recipe-checks");
    assert_target!("tests/uutils-coreutils.lock", "recipe-checks-daily");
    assert_target!("tests/cat-uutils.lock", "recipe-checks-daily");
    assert_target!("tests/youki.lock", "recipe-checks-daily");
    assert_target!("tests/cmake-demo/CMakeLists.txt", "recipe-checks");
    assert_target!("tests/recipe-checks.sh", "recipe-checks");
    assert_target!("tests/recipe-checks.sh", "recipe-checks-daily");
    assert_target!("tests/recipe-check-lib.sh", "recipe-checks");
    assert_target!("tests/recipe-check-lib.sh", "recipe-checks-daily");
    assert_target!("tests/intern-src.sh", "check-pr");
    assert_target!("tests/intern-src.sh", "recipe-checks-daily");
    // bootstrap-seed / bootstrap-mes are structured Rust recipes (no shell driver):
    // the seed tree + the mes lock route to the gates via the chain; the recipe code
    // (builder/src/bootstrap.rs) validates on the check-engine smoke + cargo-test.
    assert_target!("seed/stage0/AMD64/hex0_AMD64.hex0", "bootstrap-seed");
    assert_target!("seed/sources/mes-0.27.1.lock", "bootstrap-mes");
    assert_target!("builder/src/bootstrap.rs", "check-engine");
    assert_branch_policy!("builder/src/bootstrap.rs", "the full check would be waived");
    // The td-builder build engine validates on the check-engine SMOKE tier.
    assert_target!("builder/src/sandbox.rs", "check-engine");
    assert_branch_policy!("builder/src/main.rs", "the full check would be waived");
    assert_branch_policy!("builder/src/sandbox.rs", "the full check would be waived");
    assert_branch_policy!("builder/Cargo.toml", "the full check would be waived");
    // The per-PR budget (human 2026-07-04): only channels.scm still escalates to
    // the FULL loop. The loop spine and unmapped paths validate on the bounded
    // check-pr tier; daily/system-tier gates are named but deferred.
    assert_branch_policy!("channels.scm", "the full check would be required");
    assert_runs!("builder/src/gates.rs", "check-pr");
    assert_branch_policy!("builder/src/gates.rs", "the full check would be waived");
    assert_runs!("new/unmapped.file", "check-pr");
    assert_branch_policy!("new/unmapped.file", "the full check would be waived");
    // The run/defer partition: a chain diff RUNS its PR-sized rungs and DEFERS
    // the deep ones; the corpus arms defer the from-source package gates; the
    // system tier defers wholesale.
    assert_runs!("seed/stage0/AMD64/hex0_AMD64.hex0", "bootstrap-seed");
    assert_deferred!("seed/stage0/AMD64/hex0_AMD64.hex0", "bootstrap-gcc-mesboot");
    assert_deferred!("tests/crate-free-build.sh", "recipe-checks-daily");
    assert_runs!("recipes/src/catalog.rs", "recipe-checks");
    assert_deferred!("recipes/src/catalog.rs", "recipe-checks-daily");

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
    if let Ok(o) = Command::new("git").args(["rev-parse", "--show-toplevel"]).output() {
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
    let Ok(me) = std::env::current_exe() else { return 1 };
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
        "shell-syntax" => run_shell(
            root,
            "bash -n tests/*.sh ci/*.sh tools/*.sh .github/setup-branch-protection.sh",
        ),
        "cargo-test" => run_shell(root, "cargo test --manifest-path builder/Cargo.toml"),
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
        if !git_ok(&root, &["rev-parse", "--verify", &format!("{base}^{{commit}}")]) {
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
                merge_base = String::from_utf8_lossy(&o.stdout).lines().next().unwrap_or("").to_string();
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
            all.extend(git_lines(&root, &["ls-files", "--others", "--exclude-standard"]));
        }
        sort_unique(all)
    };

    if changed.is_empty() {
        println!("affected-checks: no changed paths relative to {base}");
        return ExitCode::SUCCESS;
    }

    let sel = compute_selection(&root, &changed);
    let header = Header { explicit, base: &base, merge_base: &merge_base };
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

    if !sel.full_required.is_empty() {
        if explicit {
            // Shell: `echo` (blank line to STDOUT) then the message `>&2`.
            println!();
            eprintln!("affected-checks: --path is inspection only; run the full check for these paths in branch mode");
            return ExitCode::from(20);
        }

        let mut uncovered: Vec<String> = Vec::new();
        let mut skipped: Vec<String> = Vec::new();
        for t in &sel.targets {
            if default_check_covers_target(&root, t) {
                skipped.push(t.clone());
            } else {
                uncovered.push(t.clone());
            }
        }

        if !uncovered.is_empty() {
            let code = run_self_check(&root, &uncovered);
            if code != 0 {
                return ExitCode::from(code as u8);
            }
        }
        if !skipped.is_empty() {
            println!(
                "\naffected-checks: escalation active; the full check covers skipped target(s): {}",
                skipped.join(" ")
            );
        }

        println!("\naffected-checks: escalation active; running the full check");
        let code = run_self_check(&root, &[]);
        return ExitCode::from(code as u8);
    } else if !sel.targets.is_empty() {
        let code = run_self_check(&root, &sel.targets);
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
        Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf()
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
        assert!(glob_match("seed/sources/make-*.lock", "seed/sources/make-4.4.lock"));
        assert!(!glob_match("seed/sources/make-*.lock", "seed/sources/make-4.4.lockX"));
        assert!(!glob_match("CHEAP_GATES", "CHEAP_GATESX"));
        assert!(pattern_matches("check.sh|builder/src/gates.rs", "check.sh"));
        assert!(!pattern_matches("check.sh|builder/src/gates.rs", "check.sh2"));
    }

    #[test]
    fn spec_extraction_matches_shell_word_ops() {
        // tests/<spec>-no-guix.lock → <spec> (shell `${p##tests/}` then `${p%-no-guix.lock}`).
        let p = "tests/td-russh-demo-no-guix.lock";
        let spec = p.strip_prefix("tests/").unwrap().strip_suffix("-no-guix.lock").unwrap();
        assert_eq!(spec, "td-russh-demo");
    }

    // DURABLE: the dispatcher's own policy, exercised over the real gate_defs +
    // tests tree. Runs on every PR via the required `cargo-test` job. No shell,
    // no Guix — it still holds with no oracle in the room.
    #[test]
    fn self_test_passes_against_repo() {
        let root = repo_root();
        if !repo_tree_present(&root) {
            eprintln!("SKIP self-test: repo tree absent at {} (builder-only sandbox)", root.display());
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
                "  cargo test --manifest-path builder/Cargo.toml",
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
                "  bash -n check.sh tests/*.sh ci/*.sh tools/*.sh .github/setup-branch-protection.sh",
                "  cargo test --manifest-path builder/Cargo.toml",
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

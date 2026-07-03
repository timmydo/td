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
//! over the real `mk/gates`/`tests` tree) + `renders_exact_output_for_static_paths`
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
// mk/gates/*.mk parsing (the shell `sed` extractors).
// ---------------------------------------------------------------------------

/// Sorted absolute paths of `mk/gates/*.mk` (shell glob order = byte sort).
fn gate_files(root: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(root.join("mk/gates"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().map(|e| e == "mk").unwrap_or(false))
        .collect();
    v.sort();
    v
}

/// `^(NAME)[[:space:]]*+=[[:space:]]*REST` → REST, or None. NAME must be a whole
/// token (so `CHEAP_GATESX` does not match `CHEAP_GATES`).
fn parse_assign<'a>(line: &'a str, name: &str, op: &str) -> Option<&'a str> {
    let rest = line.strip_prefix(name)?;
    let rest = rest.trim_start_matches([' ', '\t']);
    let rest = rest.strip_prefix(op)?;
    Some(rest.trim_start_matches([' ', '\t']))
}

const GATE_POOLS: [&str; 4] = ["CHEAP_GATES", "HEAVY_GATES", "FAST_GATES", "SYSTEM_GATES"];

/// First `CHEAP/HEAVY/FAST/SYSTEM_GATES += <target>` line's target (shell
/// `target_from_gate_file`; note ENGINE/BUILD are intentionally NOT in the set).
fn target_from_gate_file(file: &Path) -> Option<String> {
    let body = std::fs::read_to_string(file).ok()?;
    for line in body.lines() {
        for name in GATE_POOLS {
            if let Some(rest) = parse_assign(line, name, "+=") {
                return Some(rest.to_string());
            }
        }
    }
    None
}

/// Every `<X>_SPECS := ...` token in a single gate file.
fn specs_in_file(file: &Path) -> Vec<String> {
    let body = match std::fs::read_to_string(file) {
        Ok(b) => b,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    for line in body.lines() {
        // `^[A-Za-z0-9_-]*_SPECS[[:space:]]*:=[[:space:]]*REST`
        if let Some((lhs, rhs)) = line.split_once(":=") {
            if lhs.starts_with(|c: char| c.is_whitespace()) {
                continue;
            }
            let lhs = lhs.trim_end_matches([' ', '\t']);
            if lhs.ends_with("_SPECS")
                && lhs
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
            {
                out.extend(rhs.split_whitespace().map(str::to_string));
            }
        }
    }
    out
}

/// Every `NAME += <token>` value across all gate files (NAME in `names`).
fn collect_registrations(root: &Path, names: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    for f in gate_files(root) {
        let body = match std::fs::read_to_string(&f) {
            Ok(b) => b,
            Err(_) => continue,
        };
        for line in body.lines() {
            for name in names {
                if let Some(rest) = parse_assign(line, name, "+=") {
                    out.push(rest.to_string());
                }
            }
        }
    }
    out
}

fn build_gates(root: &Path) -> Vec<String> {
    collect_registrations(root, &["BUILD_GATES"])
}

/// First gate file whose `*_SPECS` contains `spec` → its registered target.
fn target_for_build_spec(root: &Path, spec: &str) -> Option<String> {
    for f in gate_files(root) {
        let target = match target_from_gate_file(&f) {
            Some(t) => t,
            None => continue,
        };
        if specs_in_file(&f).iter().any(|s| s == spec) {
            return Some(target);
        }
    }
    None
}

/// Would a plain `./check.sh` (cheap+heavy gates + build-recipes) cover `target`?
fn default_check_covers_target(root: &Path, target: &str) -> bool {
    if target == "check-fast" || target == "build-recipes" {
        return true;
    }
    collect_registrations(root, &["CHEAP_GATES", "HEAVY_GATES"])
        .iter()
        .any(|g| g == target)
}

// ---------------------------------------------------------------------------
// Mapping helpers.
// ---------------------------------------------------------------------------

fn add_gate_file_targets(sel: &mut Selection, gate: &str) {
    sel.add_target(gate);
    if gate == "offline" {
        // The old Guix oracle and td's own offline builder enforce the same durable
        // isolation property; edits to either side need both.
        sel.add_target("td-offline");
    }
}

fn add_build_gate_targets(root: &Path, sel: &mut Selection) {
    sel.add_target("build-recipes");
    for g in build_gates(root) {
        sel.add_target(&g);
    }
}

fn map_recipe_spec(root: &Path, spec: &str, sel: &mut Selection) {
    if let Some(t) = target_for_build_spec(root, spec) {
        sel.add_target(&t);
        return;
    }
    match spec {
        "td-builder" => sel.add_target("rust-build"),
        "td-vendor-demo" => sel.add_target("rust-vendor"),
        "td-russh-demo" => sel.add_target("rust-russh"),
        "td-cmake-demo" => sel.add_target("cmake"),
        "cat" => sel.add_target("rust-uutils"),
        "eza" => sel.add_target("rust-eza"),
        "bat" => sel.add_target("rust-bat"),
        "sd" => sel.add_target("rust-sd"),
        "procs" => sel.add_target("rust-procs"),
        "fd" => sel.add_target("rust-fd"),
        "ripgrep" => sel.add_target("rust-ripgrep"),
        "uutils" => sel.add_target("rust-coreutils"),
        "youki" => sel.add_target("rust-youki"),
        "td-fetch" => sel.add_target("rust-fetch"),
        "td-feed" => sel.add_target("td-feed"),
        "td-subst" => sel.add_target("td-subst"),
        "perturbed" => sel.add_target("drv-emit"),
        "pkg-config" => {
            sel.add_target("guix-dependence");
            sel.add_note("pkg-config is authored but excluded from td-built census until it has an own-builder gate.");
        }
        _ => {
            sel.add_target("check-fast");
            sel.require_full(&format!(
                "No recipe-specific mapping for '{spec}'; update builder/src/affected.rs or run full ./check.sh."
            ));
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
    "bootstrap-hello-corpus-store-native",
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

    if pattern_matches("Makefile|check.sh", p) {
        sel.add_preflight("shell-syntax");
        sel.add_target("check-fast");
        sel.add_target("cargo-test");
        sel.require_full(&format!(
            "{p} touches the loop spine; affected-checks cannot waive the full loop."
        ));
        return;
    }

    if glob_match("mk/gates/*.mk", p) {
        sel.add_preflight("shell-syntax");
        sel.add_preflight("affected-self-test");
        let abs = root.join(p);
        if abs.is_file() {
            match target_from_gate_file(&abs) {
                Some(gate) if !gate.is_empty() => add_gate_file_targets(sel, &gate),
                _ => {
                    sel.add_target("check-fast");
                    sel.require_full(&format!(
                        "{p} does not register a gate target; update the gate or run full ./check.sh."
                    ));
                }
            }
        } else {
            sel.add_target("check-fast");
            sel.require_full(&format!(
                "{p} was deleted or is unavailable; affected-checks cannot infer the removed gate target."
            ));
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
        // It feeds the corpus build path (cache-lib emits via td-recipe-eval), the spec
        // differential, and the guix-dependence census manifest — so a catalog change
        // can affect ANY built package. Run recipe-rs (self-consistency + manifest
        // sync), spec-diff, the census, and the package build gates.
        sel.add_preflight("shell-syntax");
        sel.add_target("recipe-rs");
        sel.add_target("spec-diff");
        sel.add_target("guix-dependence");
        add_build_gate_targets(root, sel);
        return;
    }

    if pattern_matches("fetch/*|fetch/src/*|fetch/Cargo.toml|fetch/Cargo.lock", p) {
        sel.add_target("rust-fetch");
        return;
    }

    if pattern_matches("feed/*|feed/src/*|feed/Cargo.toml|feed/Cargo.lock", p) {
        // td-feed builds the mirror + runs its selftests (incl. the offline `warm-selftest`
        // for the consolidated `td-feed warm <action>` orchestration). main.rs ALSO holds the
        // host-PREP warm that feeds the corpus + bootstrap gates (the former warm-*.sh), so
        // smoke a representative consumer of each warm family: rust-ripgrep (`warm crate`) and
        // bootstrap-glibc (`warm sources` + `warm kernel-headers`).
        sel.add_target("td-feed");
        sel.add_target("rust-ripgrep");
        sel.add_target("bootstrap-glibc");
        return;
    }

    if pattern_matches("subst/*|subst/src/*|subst/Cargo.toml|subst/Cargo.lock", p) {
        sel.add_target("td-subst");
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
        // the RELINKED rust tree's input-addressed lock: its consumers are the rust runtime
        // gate (which assembles+relinks+publishes it) and the rust userland gate (which sources
        // the runtime gate's assembly). A pin/recipe-rev change re-keys + re-publishes the tree.
        sel.add_target("rust-x86_64-runtime-store-native");
        sel.add_target("rust-userland-x86_64-store-native");
        return;
    }

    if pattern_matches(
        "tests/toolchain-x86_64-input-addressed.sh|mk/gates/418-toolchain-x86_64-input-addressed.mk",
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


    if glob_match("tests/*-no-guix.lock", p) {
        let spec = p.strip_prefix("tests/").unwrap_or(p);
        let spec = spec.strip_suffix("-no-guix.lock").unwrap_or(spec);
        map_recipe_spec(root, spec, sel);
        return;
    }

    if pattern_matches("tests/td-builder-rust.lock|tests/td-builder-source.scm", p) {
        sel.add_target("rust-build");
        return;
    }

    if pattern_matches(
        "tests/td-vendor-demo.lock|tests/td-vendor-demo-source.scm|tests/vendor-demo/*|tests/vendor-demo/src/*",
        p,
    ) {
        sel.add_target("rust-vendor");
        return;
    }

    if pattern_matches("tests/td-russh-demo.lock|tests/td-russh-demo-source.scm|tests/russh-demo/*", p) {
        sel.add_target("rust-russh");
        return;
    }

    if pattern_matches("tests/td-cmake-demo.lock|tests/cmake-demo/*", p) {
        sel.add_target("cmake");
        return;
    }

    if p == "tests/cat-uutils.lock" {
        sel.add_target("rust-uutils");
        return;
    }
    if p == "tests/eza.lock" {
        sel.add_target("rust-eza");
        return;
    }
    if p == "tests/bat.lock" {
        sel.add_target("rust-bat");
        return;
    }
    if p == "tests/sd.lock" {
        sel.add_target("rust-sd");
        return;
    }
    if p == "tests/procs.lock" {
        sel.add_target("rust-procs");
        return;
    }
    if p == "tests/fd.lock" {
        sel.add_target("rust-fd");
        return;
    }
    if p == "tests/ripgrep.lock" {
        sel.add_target("rust-ripgrep");
        return;
    }
    if p == "tests/uutils-coreutils.lock" {
        sel.add_target("rust-coreutils");
        return;
    }
    if p == "tests/youki.lock" {
        sel.add_target("rust-youki");
        return;
    }
    if p == "tests/td-fetch.lock" {
        sel.add_target("rust-fetch");
        return;
    }
    if pattern_matches("tests/td-feed.lock|tests/td-feed.index", p) {
        sel.add_target("td-feed");
        return;
    }
    if p == "tests/td-subst.lock" {
        sel.add_target("td-subst");
        return;
    }

    if p == "tools/gen-feed-index.sh" {
        sel.add_preflight("shell-syntax");
        sel.add_target("td-feed");
        return;
    }
    if p == "tools/feed-ensure.sh" {
        sel.add_preflight("shell-syntax");
        sel.add_target("feed-shared");
        return;
    }
    if p == "tools/warm-td-fetch-crates.sh" {
        sel.add_preflight("shell-syntax");
        sel.add_target("rust-fetch");
        sel.add_target("td-feed");
        return;
    }

    if p == "tests/crate-free-build.sh" {
        sel.add_preflight("shell-syntax");
        sel.add_target("rust-ripgrep");
        sel.add_target("rust-sd");
        sel.add_target("rust-fd");
        sel.add_target("rust-procs");
        sel.add_target("rust-eza");
        sel.add_target("rust-bat");
        sel.add_target("rust-coreutils");
        sel.add_target("rust-uutils");
        sel.add_target("rust-youki");
        sel.add_target("rust-userland-image");
        return;
    }

    if p == "tests/rust-userland-image.sh" {
        sel.add_preflight("shell-syntax");
        sel.add_target("rust-userland-image");
        return;
    }

    if pattern_matches("tests/build-pkg.sh|tests/cache-lib.sh|tests/stage0-builder.sh", p) {
        sel.add_preflight("shell-syntax");
        add_build_gate_targets(root, sel);
        return;
    }

    // The store-backend gate cluster's shared subject-swap helper (R3): it builds the
    // td subject + closure for exactly these six store-DB gates, so a change to it routes
    // to them (not every build gate).
    if p == "tests/store-subject.sh" {
        sel.add_preflight("shell-syntax");
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

    if glob_match("tests/check-memo*", p) {
        sel.add_target("memo");
        return;
    }

    if pattern_matches(
        "tests/td-builder-nar.scm|tests/td-builder-s3-drvs.scm|tests/td-builder-s4-drv.scm",
        p,
    ) {
        sel.add_target("td-builder");
        return;
    }

    if p == "tests/drv-emit-drv.scm" {
        sel.add_target("drv-emit");
        return;
    }
    if p == "tests/td-drv-build-drv.scm" {
        sel.add_target("td-drv-build");
        return;
    }
    if p == "tests/resolve-lock.scm" {
        sel.add_target("resolve");
        return;
    }

    if glob_match("tests/rootless*", p) {
        sel.add_preflight("shell-syntax");
        sel.add_target("rootless");
        return;
    }

    if p == "tests/offline-drv.scm" {
        sel.add_target("offline");
        sel.add_target("td-offline");
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
        "tests/rust-x86_64-runtime-store-native.sh|seed/sources/zlib-*.lock|mk/gates/416-rust-x86_64-runtime-store-native.mk",
        p,
    ) {
        sel.add_preflight("shell-syntax");
        sel.add_target("rust-x86_64-runtime-store-native");
        // gate 424 (#258 rust userland) sources this script ASSEMBLE-ONLY for its /td/store
        // toolchain assembly, so a change here must re-validate that consumer too.
        sel.add_target("rust-userland-x86_64-store-native");
        return;
    }

    // #258 rust userland: ripgrep built by the native x86_64 /td/store toolchain (gate 424).
    if pattern_matches(
        "tests/rust-x86_64-userland-store-native.sh|mk/gates/424-rust-userland-x86_64-store-native.mk",
        p,
    ) {
        sel.add_preflight("shell-syntax");
        sel.add_target("rust-userland-x86_64-store-native");
        return;
    }

    if pattern_matches(
        "tests/userland-x86_64-store-native.sh|seed/sources/busybox-*.lock|seed/sources/make-4.4*.lock|mk/gates/420-userland-x86_64-store-native.mk",
        p,
    ) {
        sel.add_preflight("shell-syntax");
        sel.add_target("userland-x86_64-store-native");
        // Gate 420 also PERSISTS the /td/store harness the guix-free tier consumes
        // (host-sandbox-stage0 inc2c). `check-harness` is a check.sh-intercepted tier
        // (its own container), not a make gate, so it cannot join the other ./check.sh
        // targets — run it as its own invocation after provisioning.
        sel.add_note("run `./check.sh check-harness` separately to validate the guix-free /td/store harness tier (host-sandbox-stage0 inc2c) — it consumes the harness gate 420 persisted.");
        return;
    }

    // The guix-free harness loop (host-sandbox-stage0 inc2c): mk/harness.mk + the inner
    // loop body run by `./check.sh check-harness`. The tier consumes the harness gate 420
    // persists, so provision it via gate 420; `check-harness` is a check.sh tier (its own
    // container, not a joinable make gate) and is run as a separate invocation.
    if pattern_matches("tests/harness-loop.sh|mk/harness.mk", p) {
        sel.add_preflight("shell-syntax");
        sel.add_target("userland-x86_64-store-native");
        sel.add_note("run `./check.sh check-harness` separately to validate the guix-free /td/store harness tier (host-sandbox-stage0 inc2c).");
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
        add_chain(sel, 27, 28);
        return;
    }
    // bootstrap-chain.sh is the SHARED from-seed toolchain chain; the sed corpus gate is its
    // only consumer today (other store-native gates can migrate to it later, each adding
    // itself here). A change to either re-runs the from-seed sed corpus build. (ported from
    // PR #203's affected-checks.sh arm during the cutover rebase.)
    if pattern_matches("tests/bootstrap-sed-corpus-store-native.sh|tests/bootstrap-chain.sh", p) {
        sel.add_preflight("shell-syntax");
        sel.add_target("bootstrap-sed-corpus-store-native");
        return;
    }
    // The rung-X2 native gcc gate's consumer test: a native x86_64 gcc/binutils built on top of the
    // cross toolchain. Maps only to gate 422 (the native build is downstream of the cross rungs). The
    // gate's mk/gates/422-*.mk file is already handled by the generic `mk/gates/*.mk` arm above.
    if pattern_matches("tests/bootstrap-x86_64-native-gcc-store-native.sh", p) {
        sel.add_preflight("shell-syntax");
        sel.add_target("bootstrap-x86_64-native-gcc-store-native");
        return;
    }
    if pattern_matches(
        "tests/bootstrap-x86_64-toolchain-store-native.sh|tests/x86_64-cross-fns.sh|tests/x86_64-subst-lib.sh|mk/gates/414-bootstrap-x86_64-toolchain-store-native.mk",
        p,
    ) {
        sel.add_preflight("shell-syntax");
        add_chain(sel, 28, 29);
        // x86_64-cross-fns.sh also defines the rung-X2 native rungs (build_*_x86_64_native), so a change
        // to it must re-run the native gcc gate too, not only the cross toolchain gate.
        sel.add_target("bootstrap-x86_64-native-gcc-store-native");
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

    if p == "tests/store-ns.sh" {
        sel.add_preflight("shell-syntax");
        sel.add_target("store-ns");
        return;
    }
    if p == "tests/store-relocate.sh" {
        sel.add_preflight("shell-syntax");
        sel.add_target("store-relocate");
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

    if glob_match("tests/guix-dependence.*", p) {
        sel.add_target("guix-dependence");
        return;
    }
    // guix-surface.sh + its TWO baselines (guix-surface.expected packager set,
    // guix-surface-shrink.expected directive-8 set) all route to the gate.
    if glob_match("tests/guix-surface*", p) {
        sel.add_preflight("shell-syntax");
        sel.add_target("guix-surface");
        return;
    }

    if p == "tests/recipe-rs.sh" {
        sel.add_preflight("shell-syntax");
        sel.add_target("recipe-rs");
        return;
    }
    if p == "tests/spec-diff.scm" {
        sel.add_target("spec-diff");
        return;
    }

    // The pinned td-system lowering lock, its channel-bump capture tool + regen
    // driver, and the shared resolution seam (td_system_closure) feed the two gates
    // that consume the pinned system root (oci resolves + content-scans it;
    // oci-load's plain-image leg shares the seam). tests/oci-system-closure.scm is
    // the RETIRED predecessor (the per-gate `guix repl` lowering these replaced):
    // mapped here so its deletion routes to its two consumer gates instead of
    // falling through to the broad `tests/oci*` -> check-system glob.
    if pattern_matches(
        "tests/td-system.lock|tests/td-system-lock.scm|tests/td-system-lib.sh|tools/td-system-lock-regen.sh|tests/oci-system-closure.scm",
        p,
    ) {
        sel.add_preflight("shell-syntax");
        sel.add_target("oci");
        sel.add_target("oci-load");
        return;
    }

    if p == "system/td-builder.scm" {
        sel.add_target("td-builder");
        sel.add_target("rust-build");
        return;
    }
    if p == "system/td.scm" {
        sel.add_preflight("shell-syntax");
        sel.add_target("check-system");
        sel.require_full(&format!(
            "{p} is exclusive landing spine; coordinate the landing and run the full local loop."
        ));
        return;
    }

    if pattern_matches(
        "system/*|tests/place*|tests/verify-place*|tests/registry*|tests/manifest*|tests/generation*|tests/oci*",
        p,
    ) {
        sel.add_preflight("shell-syntax");
        sel.add_target("check-system");
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
        sel.add_preflight("shell-syntax");
        sel.require_full(&format!(
            "{p} affects CI or runner gating; affected-checks cannot waive the full local loop."
        ));
        sel.add_note(&format!(
            "{p} affects CI or branch protection; inspect the workflow result after push."
        ));
        return;
    }

    // The td-builder SEED build's toolchain resolvers (provision-rust = rustc/cargo,
    // provision-cc = the C linker) and the seed build driver feed the `bootstrap` gate
    // (stage0 compile) and are covered behaviorally by the provision-rust/provision-cc gates
    // (provided/rustup|system resolution + a provided-toolchain build).
    if pattern_matches(
        "tools/provision-rust.sh|tools/provision-cc.sh|tools/bootstrap-td-builder.sh",
        p,
    ) {
        sel.add_preflight("shell-syntax");
        sel.add_target("bootstrap");
        sel.add_target("provision-rust");
        sel.add_target("provision-cc");
        return;
    }
    if p == "tests/provision-rust.sh" {
        sel.add_preflight("shell-syntax");
        sel.add_target("provision-rust");
        return;
    }
    if p == "tests/provision-cc.sh" {
        sel.add_preflight("shell-syntax");
        sel.add_target("provision-cc");
        return;
    }

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

    if p == "DIGESTS.md" {
        sel.require_full(&format!(
            "{p} is exclusive landing spine; coordinate the landing and run the full local loop."
        ));
        return;
    }

    if pattern_matches("*.md|HISTORY.md|DESIGN.md|CLAUDE.md|DIGESTS.md|.gitignore", p) {
        return; // docs — no checks
    }

    if p == "channels.scm" {
        sel.add_target("check-fast");
        sel.add_target("guix-dependence");
        // The pinned system lock (tests/td-system.lock) is input-anchored on
        // channels.scm: a bump PRs must regenerate it (tools/td-system-lock-regen.sh)
        // or these gates red deterministically at the input-sha256 pin — select them
        // so the red surfaces in the bump PR, not a day later in the daily backstop.
        sel.add_target("oci");
        sel.add_target("oci-load");
        sel.require_full(&format!(
            "{p} changed; the dependency pin affects the whole loop."
        ));
        return;
    }

    // Catch-all.
    sel.add_target("check-fast");
    sel.require_full(&format!(
        "No mapping for {p}; update builder/src/affected.rs or run full ./check.sh."
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

    if sel.preflights.is_empty() && sel.targets.is_empty() {
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
            o.push_str(&format!("  ./check.sh {}\n", sel.targets.join(" ")));
        }
    }

    o.push('\n');
    if header.explicit {
        o.push_str("Waiver: inspection only (--path does not prove the branch diff)\n");
        if sel.full_required.is_empty() {
            o.push_str("Branch-mode policy for these paths: full ./check.sh would be waived\n");
        } else {
            o.push_str("Branch-mode policy for these paths: full ./check.sh would be required\n");
            for n in &sel.full_required {
                o.push_str(&format!("  - {n}\n"));
            }
        }
    } else if sel.full_required.is_empty() {
        o.push_str("Waiver: full ./check.sh waived by affected-checks for this diff\n");
    } else {
        o.push_str("Waiver: full ./check.sh required before marking ready\n");
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
paths to focused Make targets and prints whether the full ./check.sh is waived
or still required.
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
        if let Some(rest) = l.strip_prefix("  ./check.sh ") {
            line = Some(rest);
        }
    }
    line.map(|l| l.split_whitespace().map(str::to_string).collect())
        .unwrap_or_default()
}

pub fn run_self_test(root: &Path) -> Vec<String> {
    let mut failures: Vec<String> = Vec::new();
    let mut fail = |m: String| failures.push(m);

    let has_target = |path: &str, target: &str| -> bool {
        last_check_targets(&path_output(root, path)).iter().any(|t| t == target)
    };
    macro_rules! assert_target {
        ($path:expr, $target:expr) => {
            if !has_target($path, $target) {
                fail(format!("{}: expected ./check.sh target '{}'", $path, $target));
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
    if default_check_covers_target(root, "oci-load") {
        fail("default coverage: system gate oci-load is not covered by plain ./check.sh".into());
    }

    // Every gate file maps (via the mk/gates/*.mk arm) to its own gate target.
    for f in gate_files(root) {
        let rel = format!("mk/gates/{}", f.file_name().unwrap().to_string_lossy());
        match target_from_gate_file(&f) {
            Some(gate) if !gate.is_empty() => assert_target!(&rel, &gate),
            _ => fail(format!("{rel}: no gate registration found")),
        }
    }

    if root.join("mk/gates/185-offline.mk").is_file() {
        assert_target!("mk/gates/185-offline.mk", "offline");
        assert_target!("mk/gates/185-offline.mk", "td-offline");
    }

    // Every BUILD_GATE is selected by the build-pkg/cache-lib arm.
    for bg in build_gates(root) {
        assert_target!("tests/build-pkg.sh", &bg);
        assert_target!("tests/cache-lib.sh", &bg);
    }

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
    assert_contains!("mk/gates/325-cargo-test.mk", "td-builder affected-checks --self-test");
    assert_branch_policy!("mk/gates/325-cargo-test.mk", "full ./check.sh would be waived");
    assert_target!("tests/repro-lib.sh", "bootstrap-binutils-244-store-native");
    assert_branch_policy!("tests/repro-lib.sh", "full ./check.sh would be waived");
    // The Rust td-recipe crate IS the package + spec surface (boa/TS retired): a
    // catalog edit runs recipe-rs, spec-diff, the census, and the package build gates.
    assert_target!("recipes/src/catalog.rs", "recipe-rs");
    assert_target!("recipes/src/catalog.rs", "spec-diff");
    assert_target!("recipes/src/catalog.rs", "guix-dependence");
    assert_target!("recipes/src/catalog.rs", "corpus-no-guix");
    assert_target!("recipes/Cargo.toml", "recipe-rs");
    assert_target!("tests/recipe-rs.sh", "recipe-rs");
    assert_target!("tests/recipes-meta.json", "recipe-rs");
    assert_target!("tests/spec-diff.scm", "spec-diff");
    assert_target!("tests/td-russh-demo.lock", "rust-russh");
    assert_target!("tests/russh-demo/Cargo.lock", "rust-russh");
    assert_target!("tests/td-feed.lock", "td-feed");
    assert_target!("tests/td-feed.index", "td-feed");
    assert_target!("tools/feed-ensure.sh", "feed-shared");
    assert_target!("tools/warm-td-fetch-crates.sh", "rust-fetch");
    assert_target!("tools/warm-td-fetch-crates.sh", "td-feed");
    // The consolidated warm orchestration lives in feed/src/main.rs (the former warm-*.sh):
    // a feed change smokes td-feed (build + warm-selftest) + a representative consumer of
    // each warm family.
    assert_target!("feed/src/main.rs", "rust-ripgrep");
    assert_target!("feed/src/main.rs", "bootstrap-glibc");
    assert_target!("feed/src/main.rs", "td-feed");
    assert_target!("tests/td-subst.lock", "td-subst");
    assert_target!("subst/src/main.rs", "td-subst");
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
        "mk/gates/422-bootstrap-x86_64-native-gcc-store-native.mk",
        "bootstrap-x86_64-native-gcc-store-native"
    );
    assert_target!("tests/x86_64-cross-fns.sh", "bootstrap-x86_64-native-gcc-store-native");
    assert_target!("tests/toolchain-x86_64-input-addressed.sh", "toolchain-x86_64-input-addressed");
    assert_target!(
        "mk/gates/418-toolchain-x86_64-input-addressed.mk",
        "toolchain-x86_64-input-addressed"
    );
    assert_target!(
        "tests/rust-x86_64-userland-store-native.sh",
        "rust-userland-x86_64-store-native"
    );
    assert_target!(
        "mk/gates/424-rust-userland-x86_64-store-native.mk",
        "rust-userland-x86_64-store-native"
    );
    assert_target!(
        "tests/rust-x86_64-runtime-store-native.sh",
        "rust-userland-x86_64-store-native"
    );
    assert_target!(
        "tests/userland-x86_64-store-native.sh",
        "userland-x86_64-store-native"
    );
    assert_target!("seed/sources/busybox-1.37.0.lock", "userland-x86_64-store-native");
    assert_target!("seed/sources/make-4.4.1.lock", "userland-x86_64-store-native");
    assert_target!(
        "mk/gates/420-userland-x86_64-store-native.mk",
        "userland-x86_64-store-native"
    );
    assert_target!("tests/td-cmake-demo.lock", "cmake");
    assert_target!("tests/uutils-coreutils.lock", "rust-coreutils");
    assert_target!("tests/cat-uutils.lock", "rust-uutils");
    assert_target!("tests/youki.lock", "rust-youki");
    assert_target!("tests/cmake-demo/CMakeLists.txt", "cmake");
    assert_target!("tests/guix-surface.sh", "guix-surface");
    assert_target!("tests/guix-surface.expected", "guix-surface");
    assert_target!("tests/guix-surface-shrink.expected", "guix-surface");
    assert_target!("tests/td-system.lock", "oci");
    assert_target!("tests/td-system.lock", "oci-load");
    assert_target!("tests/td-system-lock.scm", "oci");
    assert_target!("tests/td-system-lock.scm", "oci-load");
    assert_target!("tests/td-system-lib.sh", "oci");
    assert_target!("tools/td-system-lock-regen.sh", "oci-load");
    assert_target!("tests/oci-system-closure.scm", "oci");
    assert_target!("channels.scm", "oci");
    // bootstrap-seed / bootstrap-mes are structured Rust recipes (no shell driver):
    // the seed tree + the mes lock route to the gates via the chain; the recipe code
    // (builder/src/bootstrap.rs) validates on the check-engine smoke + cargo-test.
    assert_target!("seed/stage0/AMD64/hex0_AMD64.hex0", "bootstrap-seed");
    assert_target!("seed/sources/mes-0.27.1.lock", "bootstrap-mes");
    assert_target!("builder/src/bootstrap.rs", "check-engine");
    assert_branch_policy!("builder/src/bootstrap.rs", "full ./check.sh would be waived");
    // The td-builder build engine validates on the check-engine SMOKE tier.
    assert_target!("builder/src/sandbox.rs", "check-engine");
    assert_branch_policy!("builder/src/main.rs", "full ./check.sh would be waived");
    assert_branch_policy!("builder/src/sandbox.rs", "full ./check.sh would be waived");
    assert_branch_policy!("builder/Cargo.toml", "full ./check.sh would be waived");
    assert_target!("system/td.scm", "check-system");
    assert_branch_policy!("check.sh", "full ./check.sh would be required");
    assert_branch_policy!("channels.scm", "full ./check.sh would be required");
    assert_branch_policy!("system/td.scm", "full ./check.sh would be required");
    assert_branch_policy!("DIGESTS.md", "full ./check.sh would be required");
    assert_branch_policy!("new/unmapped.file", "full ./check.sh would be required");

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
            "bash -n check.sh tests/*.sh ci/*.sh tools/*.sh .github/setup-branch-protection.sh",
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
            eprintln!("affected-checks: --path is inspection only; run full ./check.sh for these paths in branch mode");
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
            let code = run_command(&root, "./check.sh", &uncovered);
            if code != 0 {
                return ExitCode::from(code as u8);
            }
        }
        if !skipped.is_empty() {
            println!(
                "\naffected-checks: escalation active; full ./check.sh covers skipped target(s): {}",
                skipped.join(" ")
            );
        }

        println!("\naffected-checks: escalation active; running full ./check.sh");
        let code = run_command(&root, "./check.sh", &[]);
        return ExitCode::from(code as u8);
    } else if !sel.targets.is_empty() {
        let code = run_command(&root, "./check.sh", &sel.targets);
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

    /// The repo fixtures the self-test reads (`mk/gates` + `tests/`) are present only
    /// when cargo runs from the full checkout — the `cargo-test` GATE and the required
    /// CI `cargo-test` job, both on every PR. The `td-builder` GUIX package build runs
    /// `cargo test` too, but its source is `local-file "../builder"` — ONLY the crate,
    /// no `mk/gates`/`tests/` — so the self-test skips there (not a weakening: the gate
    /// + CI still run it fully every PR). Markers must be repo files OUTSIDE `builder/`.
    fn repo_tree_present(root: &Path) -> bool {
        root.join("mk/gates").is_dir() && root.join("check.sh").is_file()
    }

    #[test]
    fn glob_basics() {
        assert!(glob_match("builder/src/*", "builder/src/a/b.rs")); // '*' spans '/'
        assert!(glob_match("*.md", "a/b.md"));
        assert!(glob_match("seed/sources/make-*.lock", "seed/sources/make-4.4.lock"));
        assert!(!glob_match("seed/sources/make-*.lock", "seed/sources/make-4.4.lockX"));
        assert!(!glob_match("CHEAP_GATES", "CHEAP_GATESX"));
        assert!(pattern_matches("Makefile|check.sh", "check.sh"));
        assert!(!pattern_matches("Makefile|check.sh", "Makefile2"));
    }

    #[test]
    fn spec_extraction_matches_shell_word_ops() {
        // tests/<spec>-no-guix.lock → <spec> (shell `${p##tests/}` then `${p%-no-guix.lock}`).
        let p = "tests/td-russh-demo-no-guix.lock";
        let spec = p.strip_prefix("tests/").unwrap().strip_suffix("-no-guix.lock").unwrap();
        assert_eq!(spec, "td-russh-demo");
    }

    // DURABLE: the dispatcher's own policy, exercised over the real mk/gates +
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
                "  ./check.sh check-engine",
                "",
                "Waiver: inspection only (--path does not prove the branch diff)",
                "Branch-mode policy for these paths: full ./check.sh would be waived",
                "",
                "Notes:",
                "  - builder/src/main.rs is the td-builder build engine: validated by the ~2-min check-engine smoke (compile + unit tests); the from-source build coverage is the DAILY backstop (DESIGN §7.2), not a per-PR gate.",
                "",
                "Dry run only. Re-run with --run to execute.",
            ])
        );

        // Loop spine → full loop required.
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
                "  ./check.sh check-fast cargo-test",
                "",
                "Waiver: inspection only (--path does not prove the branch diff)",
                "Branch-mode policy for these paths: full ./check.sh would be required",
                "  - check.sh touches the loop spine; affected-checks cannot waive the full loop.",
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
                "Branch-mode policy for these paths: full ./check.sh would be waived",
                "",
                "Dry run only. Re-run with --run to execute.",
            ])
        );

        // Catch-all → check-fast + require_full (now points at the moved file).
        assert_eq!(
            path_output(&root, "totally/unmapped/path.xyz"),
            expect(&[
                "affected-checks: explicit path mode",
                "",
                "Changed paths:",
                "  totally/unmapped/path.xyz",
                "",
                "Selected checks:",
                "  ./check.sh check-fast",
                "",
                "Waiver: inspection only (--path does not prove the branch diff)",
                "Branch-mode policy for these paths: full ./check.sh would be required",
                "  - No mapping for totally/unmapped/path.xyz; update builder/src/affected.rs or run full ./check.sh.",
                "",
                "Dry run only. Re-run with --run to execute.",
            ])
        );
    }
}

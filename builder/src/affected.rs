//! affected-checks — `td-builder affected-checks` (rust-migration C1).
//!
//! A faithful port of `tools/affected-checks.sh` (the biggest single shell file):
//! map a branch's changed paths to a right-sized check set and decide whether the
//! full `./check.sh` is waived or required. This is the local PR-readiness gate
//! (CLAUDE.md §"Diff-sized local check and waiver").
//!
//! The shell script is the ORACLE: the `run_self_test` here is ported to native
//! Rust `#[test]`s (the durable guard, runs on every PR via the required
//! `cargo-test` job / `check-engine` smoke), and a differential `#[test]` diffs
//! this port's `--path` output byte-for-byte against the live shell script
//! (the removable "own, then diverge" migration oracle — directive 4).
//!
//! Surfaces preserved exactly: `--run`, `--committed-only`, `--base REF`,
//! `--path FILE`, `--self-test`, `--help`. The mapping `case` arms are mirrored
//! IN ORDER (first match wins); the renderer reproduces the shell stdout
//! byte-for-byte so the differential holds.
//!
//! The shell roots itself with `cd "$(dirname "$0")/.."`; the subcommand resolves
//! the repo root via `git rev-parse --show-toplevel` (falling back to CWD outside a
//! git repo), so it is CWD-robust like the oracle. The library functions take an
//! explicit `root` so tests are CWD-independent.

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
                "No recipe-specific mapping for '{spec}'; update affected-checks.sh or run full ./check.sh."
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

/// Strip everything through the last `/recipe-` (shell `${p##*/recipe-}`).
fn after_last_recipe(p: &str) -> &str {
    match p.rfind("/recipe-") {
        Some(i) => &p[i + "/recipe-".len()..],
        None => p,
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

    if pattern_matches("ts-eval/*|ts-eval/src/*|ts-eval/Cargo.toml|ts-eval/Cargo.lock", p) {
        sel.add_target("ts-eval");
        sel.add_target("ts-diff");
        return;
    }

    if pattern_matches("fetch/*|fetch/src/*|fetch/Cargo.toml|fetch/Cargo.lock", p) {
        sel.add_target("rust-fetch");
        return;
    }

    if pattern_matches("feed/*|feed/src/*|feed/Cargo.toml|feed/Cargo.lock", p) {
        sel.add_target("td-feed");
        return;
    }

    if pattern_matches("subst/*|subst/src/*|subst/Cargo.toml|subst/Cargo.lock", p) {
        sel.add_target("td-subst");
        return;
    }

    if pattern_matches("tests/td-tsgo.lock|tests/tsgo.sh|tools/warm-tsgo.sh", p) {
        sel.add_preflight("shell-syntax");
        sel.add_target("tsgo-pin");
        sel.add_target("ts");
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

    if glob_match("tests/ts/recipe-*-perturbed.ts", p) {
        let spec = after_last_recipe(p);
        let spec = spec.strip_suffix("-perturbed.ts").unwrap_or(spec);
        map_recipe_spec(root, spec, sel);
        return;
    }

    if glob_match("tests/ts/recipe-*.ts", p) {
        let spec = after_last_recipe(p);
        let spec = spec.strip_suffix(".ts").unwrap_or(spec);
        map_recipe_spec(root, spec, sel);
        return;
    }

    if pattern_matches("tests/ts/spec-*.ts|tests/ts/td-spec.d.ts|tests/ts/spec-v0.expected.js", p) {
        sel.add_target("ts");
        sel.add_target("ts-diff");
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

    if pattern_matches("tools/warm-cargo-proxy.sh|tests/crate-free-build.sh", p) {
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
        return;
    }

    if p == "tools/warm-cargo-proxy-local.sh" {
        sel.add_preflight("shell-syntax");
        sel.add_target("rust-russh");
        return;
    }

    if pattern_matches("tests/build-pkg.sh|tests/cache-lib.sh|tests/stage0-builder.sh", p) {
        sel.add_preflight("shell-syntax");
        add_build_gate_targets(root, sel);
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
    if p == "tests/td-drv-add-drv.scm" {
        sel.add_target("td-drv-add");
        return;
    }
    if p == "tests/td-drv-assemble-drv.scm" {
        sel.add_target("td-drv-assemble");
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

    if p == "tests/bootstrap-seed.sh" {
        sel.add_preflight("shell-syntax");
        sel.add_target("bootstrap-seed");
        return;
    }
    if p == "tests/bootstrap-cc.sh" {
        sel.add_preflight("shell-syntax");
        sel.add_target("bootstrap-cc");
        return;
    }
    if p == "tests/bootstrap-mes.sh" {
        sel.add_preflight("shell-syntax");
        sel.add_target("bootstrap-mes");
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
        "tests/bootstrap-glibc.sh|seed/sources/glibc-2.2.5.lock|seed/sources/linux-*.lock|seed/patches/glibc-boot-2.2.5.patch|seed/patches/glibc-bootstrap-system-2.2.5.patch|tools/warm-kernel-headers.sh",
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
    if pattern_matches(
        "tests/bootstrap-x86_64-toolchain-store-native.sh|tests/x86_64-cross-fns.sh|tests/x86_64-subst-lib.sh|tools/warm-kernel-headers-x86_64.sh|mk/gates/414-bootstrap-x86_64-toolchain-store-native.mk",
        p,
    ) {
        sel.add_preflight("shell-syntax");
        add_chain(sel, 28, 29);
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
    if pattern_matches("seed/sources/mes-*.lock|tools/warm-bootstrap-sources.sh", p) {
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
    if glob_match("tests/guix-surface.*", p) {
        sel.add_preflight("shell-syntax");
        sel.add_target("guix-surface");
        return;
    }

    if pattern_matches("tests/ts-emit.sh|tests/ts-check.sh", p) {
        sel.add_preflight("shell-syntax");
        sel.add_target("ts");
        sel.add_target("ts-diff");
        return;
    }
    if p == "tests/ts-eval-check.sh" {
        sel.add_preflight("shell-syntax");
        sel.add_target("ts-eval");
        return;
    }

    if p == "system/td-builder.scm" {
        sel.add_target("td-builder");
        sel.add_target("rust-build");
        return;
    }
    if p == "system/td-ts.scm" {
        sel.add_target("ts");
        sel.add_target("ts-eval");
        sel.add_target("ts-diff");
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
        "system/*|tests/boot*|tests/reset*|tests/vm-lib.sh|tests/container.scm|tests/run-image.sh|tests/rollback*|tests/place*|tests/verify-place*|tests/registry*|tests/manifest*|tests/generation*|tests/oci*",
        p,
    ) {
        sel.add_preflight("shell-syntax");
        sel.add_target("check-system");
        return;
    }

    if pattern_matches("PLAN.md|plan/tracks/*|tools/plan-index.sh", p) {
        sel.add_preflight("plan-index");
        return;
    }

    if p == "tools/affected-checks.sh" {
        sel.add_preflight("shell-syntax");
        sel.add_preflight("affected-self-test");
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

    if pattern_matches("*.md|plan/*|HISTORY.md|DESIGN.md|CLAUDE.md|DIGESTS.md", p) {
        return; // docs — no checks
    }

    if p == "channels.scm" {
        sel.add_target("check-fast");
        sel.add_target("guix-dependence");
        sel.require_full(&format!(
            "{p} changed; the dependency pin affects the whole loop."
        ));
        return;
    }

    // Catch-all.
    sel.add_target("check-fast");
    sel.require_full(&format!(
        "No mapping for {p}; update affected-checks.sh or run full ./check.sh."
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
        "plan-index" => Some("  tools/plan-index.sh --check"),
        "affected-self-test" => Some("  tools/affected-checks.sh --self-test"),
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
    if default_check_covers_target(root, "oci-diff") {
        fail("default coverage: system gate oci-diff is not covered by plain ./check.sh".into());
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
            let recipe = format!("tests/ts/recipe-{spec}.ts");
            if root.join(&recipe).is_file() {
                assert_target!(&recipe, &gate);
            }
            let lock = format!("tests/{spec}-no-guix.lock");
            if root.join(&lock).is_file() {
                assert_target!(&lock, &gate);
            }
        }
    }

    // Explicit spot-checks (verbatim from the shell self-test).
    assert_contains!("tools/affected-checks.sh", "tools/affected-checks.sh --self-test");
    assert_branch_policy!("tools/affected-checks.sh", "full ./check.sh would be waived");
    assert_target!("tests/repro-lib.sh", "bootstrap-binutils-244-store-native");
    assert_branch_policy!("tests/repro-lib.sh", "full ./check.sh would be waived");
    assert_target!("tests/ts/recipe-td-russh-demo.ts", "rust-russh");
    assert_target!("tests/td-russh-demo.lock", "rust-russh");
    assert_target!("tests/russh-demo/Cargo.lock", "rust-russh");
    assert_target!("tools/warm-cargo-proxy-local.sh", "rust-russh");
    assert_target!("tests/ts/recipe-td-feed.ts", "td-feed");
    assert_target!("tests/td-feed.lock", "td-feed");
    assert_target!("tests/td-feed.index", "td-feed");
    assert_target!("tools/feed-ensure.sh", "feed-shared");
    assert_target!("tools/warm-td-fetch-crates.sh", "rust-fetch");
    assert_target!("tools/warm-td-fetch-crates.sh", "td-feed");
    assert_target!("tools/warm-cargo-proxy.sh", "rust-ripgrep");
    assert_target!("feed/src/main.rs", "td-feed");
    assert_target!("tests/ts/recipe-td-subst.ts", "td-subst");
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
    assert_target!("tests/toolchain-x86_64-input-addressed.sh", "toolchain-x86_64-input-addressed");
    assert_target!(
        "mk/gates/418-toolchain-x86_64-input-addressed.mk",
        "toolchain-x86_64-input-addressed"
    );
    assert_target!("tests/ts/recipe-td-cmake-demo.ts", "cmake");
    assert_target!("tests/td-cmake-demo.lock", "cmake");
    assert_target!("tests/ts/recipe-uutils.ts", "rust-coreutils");
    assert_target!("tests/uutils-coreutils.lock", "rust-coreutils");
    assert_target!("tests/cat-uutils.lock", "rust-uutils");
    assert_target!("tests/ts/recipe-youki.ts", "rust-youki");
    assert_target!("tests/youki.lock", "rust-youki");
    assert_target!("tests/cmake-demo/CMakeLists.txt", "cmake");
    assert_target!("tests/ts/recipe-perturbed.ts", "drv-emit");
    assert_target!("tests/guix-surface.sh", "guix-surface");
    assert_target!("tests/guix-surface.expected", "guix-surface");
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
        "plan-index" => run_command(root, "tools/plan-index.sh", &["--check".to_string()]),
        "affected-self-test" => {
            run_command(root, "tools/affected-checks.sh", &["--self-test".to_string()])
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

    /// The repo fixtures these tests read (mk/gates + the shell oracle) are present
    /// only when cargo runs from the full checkout — the `cargo-test` GATE and the
    /// required CI `cargo-test` job, both on every PR. The `td-builder` GUIX package
    /// build runs `cargo test` too, but its source is `local-file "../builder"` —
    /// ONLY the crate, no `mk/gates`/`tools/` — so these tests skip there (not a
    /// weakening: the gate + CI still run them fully every PR).
    fn repo_tree_present(root: &Path) -> bool {
        root.join("mk/gates").is_dir() && root.join("tools/affected-checks.sh").is_file()
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
        assert_eq!(after_last_recipe("tests/ts/recipe-td-russh-demo.ts"), "td-russh-demo.ts");
        assert_eq!(
            "tests/ts/recipe-foo-perturbed.ts"
                .rsplit_once("/recipe-")
                .map(|(_, s)| s.strip_suffix("-perturbed.ts").unwrap()),
            Some("foo")
        );
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

    // REMOVABLE MIGRATION ORACLE (directive 4 — own, then diverge): the Rust
    // `--path` render is byte-for-byte identical to the live shell oracle for a
    // broad path corpus. Guarded to skip where bash / the script is unavailable
    // (e.g. the loop sandbox's minimal `guix shell` PATH); it DOES run in the
    // required CI `cargo-test` job (plain ubuntu) on every PR. Deletable the day
    // `tools/affected-checks.sh` is retired.
    #[test]
    fn matches_shell_oracle_byte_for_byte() {
        let root = repo_root();
        let script = root.join("tools/affected-checks.sh");
        if !repo_tree_present(&root) {
            eprintln!("SKIP oracle differential: repo tree absent at {} (builder-only sandbox)", root.display());
            return;
        }
        // Probe: can we run the shell oracle here at all?
        let probe = Command::new("bash")
            .arg(&script)
            .args(["--path", "DESIGN.md"])
            .current_dir(&root)
            .output();
        let probe = match probe {
            Ok(o) if o.status.success() => o,
            _ => {
                eprintln!("SKIP oracle differential: cannot run bash/the shell oracle in this env");
                return;
            }
        };
        // Sanity: the shell really produced the dispatcher output.
        assert!(String::from_utf8_lossy(&probe.stdout).contains("affected-checks: explicit path mode"));

        // Corpus: every gate file, every recipe spec, + a hand-picked path per arm.
        let mut corpus: Vec<String> = Vec::new();
        for f in gate_files(&root) {
            corpus.push(format!("mk/gates/{}", f.file_name().unwrap().to_string_lossy()));
        }
        if let Ok(rd) = std::fs::read_dir(root.join("tests/ts")) {
            for e in rd.flatten() {
                let n = e.file_name().to_string_lossy().to_string();
                if n.starts_with("recipe-") && n.ends_with(".ts") {
                    corpus.push(format!("tests/ts/{n}"));
                }
            }
        }
        corpus.extend(
            [
                "Makefile",
                "check.sh",
                "channels.scm",
                "system/td.scm",
                "system/td-builder.scm",
                "system/td-ts.scm",
                "system/foo.scm",
                "builder/src/main.rs",
                "builder/Cargo.toml",
                "DIGESTS.md",
                "DESIGN.md",
                "README.md",
                "plan/tracks/x.md",
                "plan/notes.md",
                "PLAN.md",
                "tools/affected-checks.sh",
                "tools/plan-index.sh",
                "tools/warm-cargo-proxy.sh",
                "tools/warm-cargo-proxy-local.sh",
                "tools/warm-td-fetch-crates.sh",
                "tools/feed-ensure.sh",
                "ci/build-ci-image.sh",
                "ci/import-store.sh",
                "ci/other.sh",
                ".github/workflows/ci.yml",
                "tests/build-pkg.sh",
                "tests/cache-lib.sh",
                "tests/repro-lib.sh",
                "tests/heal-revert.sh",
                "tests/td-russh-demo.lock",
                "tests/cat-uutils.lock",
                "tests/foo-no-guix.lock",
                "tests/td-toolchain.lock",
                "tests/td-toolchain-x86_64.lock",
                "tests/guix-surface.sh",
                "tests/guix-dependence.scm",
                "tests/boot.scm",
                "tests/store-ns.sh",
                "tests/bootstrap-patch.sh",
                "tests/bootstrap-toolchain-store-native.sh",
                "tests/bootstrap-binutils-244-store-native.sh",
                "tests/bootstrap-gcc-14-store-native.sh",
                "tests/rust-store-native.sh",
                "seed/sources/make-4.4.lock",
                "seed/sources/mes-0.27.lock",
                "seed/stage0/hex0",
                "subst/src/main.rs",
                "feed/src/main.rs",
                "fetch/src/main.rs",
                ".claude/whatever",
                "totally/unmapped/path.xyz",
            ]
            .iter()
            .map(|s| s.to_string()),
        );

        let mut diverged: Vec<String> = Vec::new();
        for p in &corpus {
            let shell = Command::new("bash")
                .arg(&script)
                .args(["--path", p])
                .current_dir(&root)
                .output()
                .expect("run shell oracle");
            let shell_out = String::from_utf8_lossy(&shell.stdout).to_string();
            let rust_out = path_output(&root, p);
            if shell_out != rust_out {
                diverged.push(format!(
                    "--- {p} ---\nSHELL:\n{shell_out}\nRUST:\n{rust_out}\n"
                ));
            }
        }
        assert!(
            diverged.is_empty(),
            "rust port diverged from the shell oracle on {} path(s):\n{}",
            diverged.len(),
            diverged.join("\n")
        );
    }
}

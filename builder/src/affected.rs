//! affected-checks — `td-builder affected-checks` (rust-migration C1).
//!
//! Maps a branch's changed paths to a right-sized check set and decides whether the
//! full `td-builder check` is waived or required — the local pre-push gate
//! (AGENTS.md §"Tests"). This is the cutover of
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
/// tracked fix) remains an on-demand `td-builder check <gate>` target.
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
/// Being in this table does NOT mean a target gate builds that consumer.
/// The host `net-test` preflight now compiles td-net and runs its differential
/// tests; no target/bootstrap gate embeds that external-dependency crate.
const TARGET_INCLUDED_ENGINE_SOURCES: &[(&str, &str)] = &[
    (
        "engine/src/sha256.rs",
        "td-builder, td-recipe-eval, target-static td-boot, and the td-compositor terminal corpus verifier/importer",
    ),
    (
        "engine/src/crc32.rs",
        "td-builder's xz decoder, the shared gzip decoder, and target-static td-install (through gpt.rs)",
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
    (
        "engine/src/ed25519_sign.rs",
        "td-net's cfg(test) ring differential ONLY — never td-boot, which is the point of it being a separate file from ed25519.rs; neither control-plane bin uses it",
    ),
];

/// ASCII double quote, as a BYTE and not as a char literal. Two shipped sites
/// here want a quote, so they share one name rather than each spelling it. It
/// dates from when `mod tests` REFUSED a quote char literal in the shipped
/// half of a file carrying a git query — this one does — and stays because a
/// byte is what both sites compare; `lex` neutralises that spelling now.
const DQUOTE: u8 = 0x22;

/// Crates whose source a RECIPE stages for a target build from a hand-written
/// file list, paired with that recipe. That list and the crate's `#[path]`
/// includes live in different files and nothing but the scan below relates
/// them: an include added without its `WriteFile` compiles under host cargo and
/// fails an hour later inside recipe-checks.
///
/// Every crate whose recipe stages a `.rs` file, not only the two that reach
/// `engine/src/` — the pair is what a `#[path]` include costs, and which TREE
/// the included file lives in has nothing to do with it.
/// `every_recipe_that_stages_rust_sources_is_in_the_roster` holds this to the
/// tree, so an entry cannot be quietly omitted.
const TARGET_STATIC_RECIPES: &[(&str, &str)] = &[
    ("td-audio/src", "recipes/src/recipes/td-audio.rs"),
    ("td-boot/src", "recipes/src/recipes/td-boot.rs"),
    ("td-busd/src", "recipes/src/recipes/td-busd.rs"),
    ("td-compositor/src", "recipes/src/recipes/td-compositor.rs"),
    ("td-firstboot/src", "recipes/src/recipes/td-firstboot.rs"),
    ("td-init/src", "recipes/src/recipes/td-init.rs"),
    ("td-install/src", "recipes/src/recipes/td-install.rs"),
    ("td-jail/src", "recipes/src/recipes/td-jail.rs"),
    ("td-kexec/src", "recipes/src/recipes/td-kexec.rs"),
    ("td-login/src", "recipes/src/recipes/td-login.rs"),
    ("td-netd/src", "recipes/src/recipes/td-netd.rs"),
    ("td-portal/src", "recipes/src/recipes/td-portal.rs"),
    ("td-profiler/src", "recipes/src/recipes/td-profiler.rs"),
    ("td-seatd/src", "recipes/src/recipes/td-seatd.rs"),
    ("td-sh/src", "recipes/src/recipes/td-sh.rs"),
    ("td-svc/src", "recipes/src/recipes/td-svc.rs"),
    ("td-txt/src", "recipes/src/recipes/td-txt.rs"),
    ("td-util/src", "recipes/src/recipes/td-util.rs"),
];

/// Where a `#[path]` include resolves to in a recipe's STAGED tree.
///
/// `#[path]` is relative to the including file's own directory, and these
/// crates put every include in `<crate>/src/main.rs`, so the include is walked
/// against the crate's src dir: `td-install/src` + `../../engine/src/gpt.rs`
/// gives `engine/src/gpt.rs`, which a recipe must stage at
/// `{src}/engine/src/gpt.rs` for the RELATIVE include to resolve at all.
///
/// `None` if the walk climbs above the repository root, which is not a path any
/// recipe can stage.
fn staged_destination(src_dir: &str, included: &str) -> Option<String> {
    let mut parts: Vec<&str> = src_dir.split('/').filter(|p| !p.is_empty()).collect();
    for part in included.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            other => parts.push(other),
        }
    }
    Some(format!("{{src}}/{}", parts.join("/")))
}

/// `path` relative to the repository root, with `/` separators.
fn repo_relative(root: &Path, path: &Path) -> Option<String> {
    Some(path.strip_prefix(root).ok()?.to_string_lossy().replace('\\', "/"))
}

/// The DIRECTORY holding `path`, relative to the repository root. `#[path]`
/// resolves against the including file's own directory, so this is what an
/// include is walked from.
fn repo_relative_dir(root: &Path, path: &Path) -> Option<String> {
    repo_relative(root, path.parent()?)
}

/// Every `#[path = "…"]` in `text`, each paired with whether its item is gated
/// on `cfg(test)`.
///
/// The gate may sit on either side of the `#[path]` — attributes on one item
/// are unordered — so the whole contiguous run of attribute lines around it is
/// read, not just the line before.
fn path_includes(text: &str) -> Vec<(String, bool)> {
    let lines: Vec<&str> = text.lines().collect();
    let mut out = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let Some(rest) = line.split_once("#[path = \"").map(|(_, r)| r) else {
            continue;
        };
        let Some((included, _)) = rest.split_once(DQUOTE as char) else {
            continue;
        };
        let attr_run = |range: &mut dyn Iterator<Item = usize>| {
            let mut gated = false;
            for j in range {
                let Some(l) = lines.get(j).map(|l| l.trim()) else {
                    break;
                };
                if !l.starts_with("#[") {
                    break;
                }
                gated |= l.contains("cfg(test)");
            }
            gated
        };
        let gated = attr_run(&mut (0..i).rev()) || attr_run(&mut (i.saturating_add(1)..lines.len()));
        out.push((included.to_string(), gated));
    }
    out
}

/// Every `.rs` under `dir`, recursively. A non-recursive read would never look
/// inside a `src/<dir>/mod.rs`.
fn collect_rs_recursive(dir: &Path, out: &mut Vec<PathBuf>) {
    let mut pending = vec![dir.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }
}

/// `text` with `//` line comments removed, so a source scan sees CODE.
///
/// The confinement scans below look for tokens that must not appear, and the
/// files they scan are the files most likely to DISCUSS those tokens: td-boot's
/// header explains at length why it does not include the signer, and naming it
/// there is worth more than the scan's convenience. Without this, that comment
/// alone red the gate — which is how this came to exist.
///
/// A `//` inside a STRING LITERAL is not a comment, and cutting there would
/// discard the rest of a real line — `let u = "http://x"; mod ed25519_sign;`
/// would strip the declaration and hide it from the scan. So a cut only happens
/// where the quotes before it are balanced. That is a heuristic and not a lexer
/// (it knows nothing of escapes or raw strings), which is sound in the
/// conservative direction: an unbalanced count means NO cut, so the scan sees
/// MORE text rather than less, and the failure mode is a false positive that
/// someone must look at rather than a token that slipped past.
///
/// Line comments only. A `/* */` between two tokens would defeat it, which is
/// why the scanned set is small and hand-written rather than a glob.
///
/// `mod tests` carries a SECOND stripper, `strip_comments`, and the two are
/// deliberately not one. That one also cuts block comments and so must carry
/// depth ACROSS lines, which desynchronises on a hashed raw string — the git
/// query scan reads `lex` rather than either of them for that reason. The
/// files THIS scan reads are full of them, and a desync here fails the other
/// way: text mis-stripped is a token that slipped past.
pub(crate) fn strip_line_comments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.lines() {
        let mut quotes = 0usize;
        let mut cut = None;
        let bytes = line.as_bytes();
        for (i, byte) in bytes.iter().enumerate() {
            if *byte == DQUOTE {
                quotes = quotes.saturating_add(1);
            } else if quotes.is_multiple_of(2)
                && *byte == b'/'
                && bytes.get(i.saturating_add(1)) == Some(&b'/')
            {
                cut = Some(i);
                break;
            }
        }
        match cut {
            Some(at) => out.push_str(line.get(..at).unwrap_or_default()),
            None => out.push_str(line),
        }
        out.push('\n');
    }
    out
}

/// Does `text` declare a function named exactly `name`?
///
/// A plain `contains("fn sign")` is what a first draft used, and it matched the
/// test helper `fn signature_of` — so the boundary is checked rather than
/// assumed. Both `fn name(` and `fn name<` count; anything continuing the
/// identifier does not.
fn declares_fn(text: &str, name: &str) -> bool {
    let needle = format!("fn {name}");
    let mut from = 0usize;
    while let Some(at) = text.get(from..).and_then(|rest| rest.find(&needle)) {
        let end = from.saturating_add(at).saturating_add(needle.len());
        let next = text.get(end..).and_then(|rest| rest.chars().next());
        if !next.is_some_and(|c| c.is_alphanumeric() || c == '_') {
            return true;
        }
        from = end;
    }
    false
}

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

fn map_path(root: &Path, roster: &Result<Vec<GateCrate>, String>, p: &str, sel: &mut Selection) {
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

    if p == "engine/src/gzip.rs" {
        sel.add_preflight("cargo-test");
        sel.add_preflight("net-test");
        sel.add_target("check-engine");
        sel.add_target("check");
        sel.add_note(
            "engine/src/gzip.rs is shared by td-builder source extraction and the bounded OSTree importer: check-engine is the fast signal and the full check exercises the from-source consumer.",
        );
        return;
    }

    // The shared std-only engine lib and its wire fixtures, plus the
    // workspace-root manifest/lock. The engine compiles INTO td-builder (build
    // engine -> check-engine), td-recipe-eval (recipe surface -> recipe-rs +
    // package build gates), and td-net (host preflight), and the fixtures
    // determine its tests. Route their UNION conservatively.
    // builder/Cargo.lock and recipes/Cargo.lock are tombstones for the
    // workspace-root lock.
    if pattern_matches(
        "engine/Cargo.toml|engine/src/*|engine/tests/*|Cargo.toml|Cargo.lock|builder/Cargo.lock|recipes/Cargo.lock",
        p,
    ) {
        sel.add_preflight("shell-syntax");
        sel.add_preflight("cargo-test");
        if pattern_matches("engine/Cargo.toml|engine/src/*|engine/tests/*", p) {
            sel.add_preflight("net-test");
        }
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
            "td-builder, td-recipe-eval, and td-net"
        };
        add_build_gate_targets(root, sel);
        if pattern_matches("engine/tests/*", p) {
            sel.add_note(&format!(
                "{p} is a td-engine test input: cargo-test and check-engine exercise it directly, while recipe-rs and the package build gates cover the shared engine consumers."
            ));
        } else {
            sel.add_note(&format!(
                "{p} is shared engine/workspace code compiled into {consumers}: validated by check-engine (compile + unit tests) and the recipe/package build gates."
            ));
        }
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

    // .cargo/config.toml declares the `runner` every cargo TEST binary is
    // executed through (builder/src/run_capped.rs), so an edit changes the
    // memory ceiling every test in the tree runs under, and a broken edit
    // leaves them uncapped. The cargo-test preflight is the direct check: its
    // attestation reds when the runner does not apply.
    //
    // recipe-rs too, and not for symmetry: the preflight's 36 commands are all
    // host-gnu, while `recipe_rs` runs `cargo test --target
    // x86_64-unknown-linux-musl --manifest-path recipes/Cargo.toml` from the
    // repo root (gate_bodies.rs), so it is the only tier that exercises the
    // MUSL runner entry. Without it a typo confined to that entry passes the
    // preflight and dies in the full check.
    if p == ".cargo/config.toml" {
        sel.add_preflight("cargo-test");
        sel.add_preflight("start-bootstrap");
        sel.add_target("recipe-rs");
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
        // rules. The host net-test preflight compiles and tests td-net; no
        // target gate builds its external-dependency crate from source.
        sel.add_preflight("net-test");
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
        // sandbox, so it is not a gate at all; the dev host runs it directly.
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

    if p == "start" || p == "tests/start.sh" {
        sel.add_preflight("shell-syntax");
        sel.add_preflight("start-bootstrap");
        return;
    }

    // Unlike generic prose, the profiler design is its normative runtime and
    // evidence contract. Route it before the docs waiver so an amendment runs
    // both the host parser/boundedness tests and target image integration.
    if p == "td-profiler/DESIGN.md" {
        sel.add_preflight("cargo-test");
        sel.add_target("check");
        sel.add_target("recipe-checks");
        return;
    }

    if pattern_matches("*.md|DESIGN.md|CLAUDE.md|.gitignore", p) {
        return; // docs — no checks
    }

    // td-boot's protocol.rs is the deployment contract, `#[path]`-included by
    // two OTHER trees: the td-recipe lib root, which is where every recipe and
    // the qemu oracle reach it, and — since the manifest header and size bound
    // moved into it — the host-side td-net, whose `td-deploy` signs what td-boot
    // verifies. The td-boot rule below covers the first through recipe-checks
    // but compiles no net, and no gate builds td-net from source: the WARM does,
    // which is a chain target. So this file takes the td-boot rule plus those,
    // or a change here that breaks only the signer runs nothing.
    //
    // `realfile.rs` joined it in that class when the real-bounded-file rule
    // stopped being written three times: td-net includes it for the same
    // reason — the signer must refuse exactly what the verifier refuses — so
    // it needs the same chain targets. The rule is about these two FILES and
    // not about the crate, which is why `td-boot/src/main.rs` is pinned to the
    // narrower one beside the assertions below.
    if p == "td-boot/src/protocol.rs" || p == "td-boot/src/realfile.rs" {
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

    // td-jail is target-static like td-kexec, and its recipe test is the
    // functional unprivileged namespace/PID-1 probe. Host cargo holds the Rust
    // and unsafe-confinement tests; recipe-checks links and runs the shipped
    // binary rather than a host approximation.
    if pattern_matches(
        "td-jail/*|td-jail/src/*|td-jail/Cargo.toml|td-jail/Cargo.lock",
        p,
    ) {
        sel.add_preflight("cargo-test");
        sel.add_target("check");
        sel.add_target("recipe-checks");
        return;
    }

    // td-busd is target-static like td-jail, and its recipe test runs the
    // shipped binary's own codec selftest: the same committed byte streams the
    // host tests decode, decoded by the target build. Host cargo holds the Rust
    // rules and the no-unsafe confinement test.
    //
    // spec/ holds interop DATA — recorded conversations, and upstream's own
    // auth scripts a directory deeper — and examples/ the recorder that writes
    // the former. All are host-side: src/recorded.rs and src/authscript.rs
    // replay them under cfg(test), so the recipe never stages them. All already
    // match `td-busd/*`, at either depth, since `glob_match` is fnmatch WITHOUT
    // FNM_PATHNAME and its star crosses `/`. They are named in the assertions
    // below rather than here: an added alternative would change no decision
    // while implying the star stops at a separator.
    //
    // The recipe-checks selection below is wider than these two paths need,
    // since the recipe never sees them. That over-selection predates this and
    // is left alone rather than special-cased here.
    if pattern_matches(
        "td-busd/*|td-busd/src/*|td-busd/Cargo.toml|td-busd/Cargo.lock",
        p,
    ) {
        sel.add_preflight("cargo-test");
        sel.add_target("check");
        sel.add_target("recipe-checks");
        return;
    }

    // The first portal is another target-static process. Its recipe stages its
    // own sources plus the broker's canonical message/name/wire modules; the
    // td-busd arm above already sends those shared-module changes through the
    // same full cargo and recipe tiers.
    if pattern_matches(
        "td-portal/*|td-portal/src/*|td-portal/Cargo.toml|td-portal/Cargo.lock",
        p,
    ) {
        sel.add_preflight("cargo-test");
        sel.add_target("check");
        sel.add_target("recipe-checks");
        return;
    }

    // td-audio: the audio daemon. A dependency-free crate embedded into its
    // target-static recipe. Host cargo holds its PCM struct-layout, discovery,
    // mixer, tone and unsafe-confinement assertions; recipe-checks holds the
    // source-built static link. It is not selected into an image yet — the
    // `/dev/snd` ownership, the unit, and the service-only credential class
    // APPLICATIONS.md §K.5 names are still owed — so there is no image
    // assertion to route to.
    if pattern_matches(
        "td-audio/*|td-audio/src/*|td-audio/Cargo.toml|td-audio/Cargo.lock",
        p,
    ) {
        sel.add_preflight("cargo-test");
        sel.add_target("check");
        sel.add_target("recipe-checks");
        return;
    }

    // The collector is a dependency-free crate embedded into its target-static
    // recipe and packed into system-x86-64. Host cargo holds its parser,
    // boundedness, and unsafe-confinement assertions; recipe-checks holds the
    // source-built static link and image integration.
    if pattern_matches(
        "td-profiler/*|td-profiler/src/*|td-profiler/Cargo.toml|td-profiler/Cargo.lock",
        p,
    ) {
        sel.add_preflight("cargo-test");
        sel.add_target("check");
        sel.add_target("recipe-checks");
        return;
    }

    // td-install: the disk installer. Same standalone-crate shape as td-boot, and
    // it shares that crate's `protocol.rs` plus the engine GPT/FAT32/CRC-32
    // writers through `#[path]`, so host cargo is what holds its Rust rules and
    // its layout tests. `recipe-checks` joins them because `td-install.rs` now
    // links the same source statically for the target and `td-install-test.rs`
    // runs THAT binary over a real destination — a link regression and a
    // signature written to the wrong offset are both invisible to host cargo.
    if pattern_matches(
        "td-install/*|td-install/src/*|td-install/Cargo.toml|td-install/Cargo.lock",
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

    // A path inside a crate the roster DISCOVERED, that no arm above maps: the
    // crate is new. Deriving the roster is only half of "a crate joins the gate
    // by existing" — the other half is being SELECTED. Without this the roster
    // would carry the crate while a branch touching only that crate never ran
    // the commands that compile it, so its lock guard, tests and clippy would
    // be listed and never executed. The targets stay the catch-all's: which
    // recipe embeds a new crate, if any, is exactly what an author still has to
    // say, and the note keeps asking them to.
    match roster {
        Ok(crates) => {
            let mine = crates.iter().find(|c| {
                // The separator matters: `td-shell/x` is not `td-sh/x`.
                p.starts_with(&c.name) && p.as_bytes().get(c.name.len()) == Some(&b'/')
            });
            if let Some(krate) = mine.map(|c| c.name.as_str()) {
                sel.add_preflight("cargo-test");
                sel.add_target("check");
                // Deliberately NOT the "No mapping for" wording: that
                // prefix is how `selects_checks_with` tells a mapped path
                // from scratch, and an untracked file in a new crate DOES
                // route to a real check now. Calling it unmapped would let
                // `ready` wave it through as a scratch note.
                sel.add_note(&format!(
                    "{p} is in discovered crate {krate} — running its cargo-test \
                     preflight and the full check. Add a mapping in \
                     builder/src/affected.rs for a narrower answer."
                ));
                return;
            }
        }
        // Never in silence: the fallback below is still the whole behavioural
        // tier, but it is NOT the preflight, and a reader has to be told which
        // answer they got.
        Err(e) => sel.add_note(&format!(
            "the gate roster could not be read ({e}) — no cargo-test preflight \
             was selected for {p}"
        )),
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

fn preflight_cmd(root: &Path, name: &str, changed: &[String]) -> Option<String> {
    match name {
        "shell-syntax" => Some("  bash -n start tests/*.sh ci/*.sh tools/*.sh".to_string()),
        "start-bootstrap" => Some("  bash tests/start.sh".to_string()),
        "heal-revert" => Some("  bash tests/heal-revert.sh".to_string()),
        // Rendered from the SAME list that runs, so a scoped run cannot print a
        // command it will not issue — the dry run is what a reader trusts.
        "cargo-test" => match cargo_test_cmds(root, changed) {
            Ok(cmds) => Some(render_cargo_test(&cmds)),
            // A dry run must not print a command set it could not compute. The
            // render is what a reader trusts, so an unreadable roster says so
            // here rather than quietly advertising a narrower run.
            Err(e) => Some(format!("  cargo-test: UNAVAILABLE — {e}")),
        },
        "net-test" => {
            Some("  CC=gcc cargo test --frozen --manifest-path net/Cargo.toml".to_string())
        }
        "affected-self-test" => Some("  td-builder affected-checks --self-test".to_string()),
        "local-source-digests" => Some("  td-recipe-eval local-source-digests".to_string()),
        _ => None,
    }
}

/// The one-line summary of a cargo-test command set: the historical wording,
/// with the manifests it will actually visit.
fn render_cargo_test(cmds: &[String]) -> String {
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
                // Beside the crate that DECLARED them. The old wording appended
                // the args once at the end of the whole line, which read
                // correctly only while the one crate that has them sorted last;
                // over a derived roster that spelling names whichever crate
                // happens to be final instead. `--all-targets` stays
                // unrendered, as it always was: this line summarises which
                // manifests are visited, and clippy flags do not change that.
                if let Some(args) = declared_test_args(cmds, krate) {
                    o.push_str(" -- ");
                    o.push_str(args);
                }
            }
        }
    }
    o
}

/// The `--` suffix of `krate`'s own `cargo test` command, if it declared one.
fn declared_test_args<'a>(cmds: &'a [String], krate: &str) -> Option<&'a str> {
    cmds.iter()
        .find(|c| c.starts_with("cargo test ") && cmd_manifest_crate(c) == Some(krate))?
        .split_once(" -- ")
        .map(|(_, args)| args)
}

struct Header<'a> {
    explicit: bool,
    base: &'a str,
    merge_base: &'a str,
}

/// Produce the full dry-run stdout (the text the shell prints before executing),
/// including the trailing "Dry run only" note when `run` is false.
fn format_output(
    root: &Path,
    header: &Header,
    changed: &[String],
    sel: &Selection,
    run: bool,
) -> String {
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
            if let Some(cmd) = preflight_cmd(root, pre, changed) {
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

/// The untracked paths that route to a real check, with the roster read ONCE.
///
/// `ready` asks this of every untracked file, which the committed-only checks
/// do not see but a compile does. The roster is hoisted out of the filter
/// rather than re-scanning the tree and re-parsing every manifest per path
/// (AGENTS.md, 'Rust code': keep work out of hot loops), and an empty list
/// scans nothing at all.
pub(crate) fn blocking_untracked<'a>(root: &Path, paths: &[&'a str]) -> Vec<&'a str> {
    if paths.is_empty() {
        return Vec::new();
    }
    let roster = discover_gate_crates(root);
    paths
        .iter()
        .copied()
        .filter(|p| selects_checks_with(root, &roster, p))
        .collect()
}

/// Whether ONE path routes to any check at all. Asking the real mapping beats
/// guessing by extension.
fn selects_checks_with(
    root: &Path,
    roster: &Result<Vec<GateCrate>, String>,
    path: &str,
) -> bool {
    let mut sel = Selection::default();
    map_path(root, roster, path, &mut sel);
    // The catch-all arm adds `check` to EVERY unmapped path, so "selected
    // something" is true of any file at all — scratch notes included. The note
    // it leaves behind is what tells a mapped path from an unmapped one, and
    // only a mapped one has any business reddening a pre-push gate.
    !sel.notes.iter().any(|n| n.starts_with("No mapping for"))
        && (!sel.preflights.is_empty() || !sel.targets.is_empty())
}

fn compute_selection(root: &Path, changed: &[String]) -> Selection {
    let mut sel = Selection::default();
    // Resolved ONCE for the whole diff: the new-crate arm would otherwise
    // re-scan the tree for every unmapped path (AGENTS.md, 'Rust code': keep
    // work out of hot loops), and two scans in one run could disagree about
    // which crates exist.
    let roster = discover_gate_crates(root);
    for p in changed {
        if !p.is_empty() {
            map_path(root, &roster, p, &mut sel);
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
    format_output(root, &header, &changed, &sel, false)
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
    assert_target!("engine/src/gzip.rs", "check-engine");
    assert_target!("engine/src/gzip.rs", "check");
    assert_preflight!("engine/src/gzip.rs", "net-test");
    assert_contains!("engine/src/gzip.rs", "source extraction");
    assert_preflight!("engine/src/ostree.rs", "net-test");
    assert_preflight!("engine/src/lib.rs", "net-test");
    assert_target!(
        "engine/tests/fixtures/flathub-firefox-154.commit.hex",
        "check-engine"
    );
    assert_target!(
        "engine/tests/fixtures/flathub-firefox-154.commit.hex",
        "recipe-rs"
    );
    assert_contains!(
        "engine/tests/fixtures/flathub-firefox-154.commit.hex",
        "is a td-engine test input"
    );
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
    assert_contains!("engine/src/ed25519_sign.rs", "never td-boot");
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
        "engine/src/ed25519_sign.rs",
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
    // The ed25519 SPLIT, asserted against the tree because the compiler cannot
    // see it and the note above is only a note. `ed25519.rs` is the file the
    // verifying boot shim `#[path]`-includes, so a signer reaching it puts
    // one in the boot binary; the whole reason `ed25519_sign.rs` is a separate
    // file is that neither half of that is otherwise enforced. Two ways in, so
    // both are shut: td-boot naming a path to the signer, and the signer's own
    // entry points migrating into the file td-boot does include.
    //
    // The needles are what a signer NEEDS rather than what one is called: a
    // secret seed to expand (`SEED_LEN`) and something to expand it with
    // (`fn sign`, `fn public_key`). A file with none of the three cannot derive
    // a key from a secret whatever its functions are named. The two function
    // needles check an identifier BOUNDARY — the first draft's plain substring
    // matched the test helper `fn signature_of` and red against a clean tree.
    // RECURSIVE, because a non-recursive read would never look inside
    // `td-boot/src/<dir>/mod.rs` — a module tree this crate does not have today
    // and which nothing stops it growing.
    let mut boot_files = Vec::new();
    collect_rs_recursive(&root.join("td-boot/src"), &mut boot_files);
    if boot_files.is_empty() {
        fail("td-boot/src has no .rs files to scan for the ed25519 split".to_string());
    }
    for path in &boot_files {
        let Ok(text) = std::fs::read_to_string(path) else {
            fail(format!("cannot read {}", path.display()));
            continue;
        };
        if strip_line_comments(&text).contains("ed25519_sign") {
            fail(format!(
                "{} names ed25519_sign in CODE: the signer must not reach the boot binary \
                 (engine/src/ed25519_sign.rs, td-install/DESIGN.md §6)",
                path.display()
            ));
        }
    }
    // Every engine source a target-static crate `#[path]`-includes must ALSO be
    // staged by that crate's recipe, which builds from a hand-written file list
    // rather than from cargo. Those two lists are edited in different files and
    // nothing related them: an include added without its `WriteFile` compiles
    // here and fails only inside recipe-checks, an hour away. That is precisely
    // the loop the landing which added this was splitting itself in two to
    // avoid, so the correspondence is asserted rather than remembered.
    //
    // One entry per crate that has such a recipe. td-install joined td-boot with
    // the recipe that builds it, and a crate MISSING from this roster is the
    // failure it exists for — so the roster is checked against the tree below.
    for (src_dir, recipe_path) in TARGET_STATIC_RECIPES {
        let mut sources = Vec::new();
        collect_rs_recursive(&root.join(src_dir), &mut sources);
        if sources.is_empty() {
            fail(format!("{src_dir} has no .rs files to check against {recipe_path}"));
            continue;
        }
        // Comment-stripped, and that is not tidiness: a recipe's own prose
        // explains what its `#[path]`-relative staging is FOR and names the
        // files while doing it, so a scan over raw text is satisfied by the
        // sentence that describes the staging rather than by the staging.
        let recipe = match std::fs::read_to_string(root.join(recipe_path)) {
            Ok(text) => strip_line_comments(&text),
            Err(e) => {
                fail(format!("cannot read {recipe_path}: {e}"));
                continue;
            }
        };
        // TEST-ONLY modules are not staged and must not be asked for: rustc
        // builds these recipes without `--test`, so a `#[cfg(test)] #[path]`
        // include is never compiled and its file has no business in the target
        // tree. Collected first because the property is INHERITED — the includes
        // inside a test-only module are test-only too, however they are written
        // there (td-compositor's `term_spec.rs` reaches `engine/src/sha256.rs`).
        let mut test_only: Vec<String> = Vec::new();
        for path in &sources {
            let Ok(text) = std::fs::read_to_string(path) else {
                continue;
            };
            for (included, gated) in path_includes(&strip_line_comments(&text)) {
                if !gated {
                    continue;
                }
                if let Some(dir) = repo_relative_dir(root, path) {
                    if let Some(dest) = staged_destination(&dir, &included) {
                        test_only.push(dest);
                    }
                }
            }
        }
        for path in &sources {
            let Ok(text) = std::fs::read_to_string(path) else {
                continue;
            };
            let (Some(file_dir), Some(own)) =
                (repo_relative_dir(root, path), repo_relative(root, path))
            else {
                continue;
            };
            // A file reached only under `cfg(test)` is itself test-only.
            if test_only.iter().any(|t| *t == format!("{{src}}/{own}")) {
                continue;
            }
            for (included, gated) in path_includes(&strip_line_comments(&text)) {
                if gated {
                    continue;
                }
                let src_dir = file_dir.as_str();
                let included = included.as_str();
                // The staged DESTINATION, not the file's name. A recipe names
                // each source twice — `include_str!` to read it at compile time
                // and `WriteFile` to stage it — and only the second is what the
                // target build compiles, so a basename match is satisfied by the
                // `include_str!` of a file the recipe no longer stages. The
                // destination is also not a proxy for the requirement but IS it:
                // the include is RELATIVE, so it resolves only if the staged
                // tree mirrors the repository at exactly this path.
                let Some(want) = staged_destination(src_dir, included) else {
                    fail(format!(
                        "{} #[path]-includes {included}, which climbs above the \
                         repository root and cannot be staged",
                        path.display()
                    ));
                    continue;
                };
                if !recipe.contains(&want) {
                    fail(format!(
                        "{} #[path]-includes {included}, which {recipe_path} \
                         does not stage at {want} — the target build would \
                         fail in recipe-checks, not here",
                        path.display()
                    ));
                }
            }
        }
    }
    match std::fs::read_to_string(root.join("engine/src/ed25519.rs")) {
        Ok(verifier) => {
            let verifier = strip_line_comments(&verifier);
            for name in ["sign", "public_key"] {
                if declares_fn(&verifier, name) {
                    fail(format!(
                        "engine/src/ed25519.rs declares `fn {name}`: it is verify-only, and \
                         the boot shim includes it — a signer belongs in ed25519_sign.rs"
                    ));
                }
            }
            // Substring is right for this one: nothing in a verify-only file has
            // a reason to spell SEED_LEN at all, so there is no boundary case to
            // get wrong and the loose direction is a loud failure, not a pass.
            if verifier.contains("SEED_LEN") {
                fail(
                    "engine/src/ed25519.rs names SEED_LEN: a seed is the secret half, and \
                     this is the file the boot shim includes"
                        .to_string(),
                );
            }
        }
        Err(e) => fail(format!("cannot read engine/src/ed25519.rs: {e}")),
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
    // The merged td-net gets the union of the former fetch/feed rules plus its
    // host unit-test preflight. No target gate builds the external-dependency
    // crate; main.rs still holds the warm-sources consumer smoked by the chain.
    assert_target!("net/Cargo.lock", "recipe-checks");
    assert_target!("net/src/main.rs", "recipe-checks");
    assert_target!("net/src/fetch.rs", "check");
    assert_target!("net/Cargo.toml", "check");
    assert_preflight!("net/src/ostree.rs", "net-test");
    assert_preflight!("net/src/http.rs", "net-test");
    assert_preflight!("net/Cargo.toml", "net-test");
    assert_contains!(
        "net/src/ostree.rs",
        "CC=gcc cargo test --frozen --manifest-path net/Cargo.toml"
    );
    // td-kexec/src is include_str!'d into the target artifact, so a helper-source edit
    // rides the host cargo preflight AND is recorded against recipe-checks,
    // which statically links it via td-kexec-test.
    assert_target!("td-kexec/src/main.rs", "check");
    assert_target!("td-kexec/src/main.rs", "recipe-checks");
    assert_target!("td-jail/src/main.rs", "check");
    assert_target!("td-jail/src/main.rs", "recipe-checks");
    assert_target!("td-jail/src/sys.rs", "check");
    assert_target!("td-jail/src/sys.rs", "recipe-checks");
    assert_preflight!("td-jail/src/transition.rs", "cargo-test");
    assert_target!("td-jail/Cargo.lock", "check");
    assert_target!("td-jail/Cargo.lock", "recipe-checks");
    assert_target!("td-busd/src/main.rs", "check");
    assert_target!("td-busd/src/main.rs", "recipe-checks");
    assert_preflight!("td-busd/src/wire.rs", "cargo-test");
    assert_preflight!("td-busd/src/message.rs", "cargo-test");
    // The interop corpus and its recorder ride the host cargo preflight, which
    // is what replays them. Regression pins rather than verified-red evidence:
    // `td-busd/*` already matches both, so these passed before the corpus
    // existed and would notice only a future narrowing of the matcher.
    assert_preflight!("td-busd/spec/libdbus-listnames.conversation", "cargo-test");
    assert_preflight!("td-busd/spec/auth/external-successful.auth-script", "cargo-test");
    assert_preflight!("td-busd/examples/dbus-capture.rs", "cargo-test");
    assert_target!("td-busd/Cargo.lock", "check");
    assert_target!("td-busd/Cargo.lock", "recipe-checks");
    assert_target!("td-portal/src/main.rs", "check");
    assert_target!("td-portal/src/main.rs", "recipe-checks");
    assert_preflight!("td-portal/src/settings.rs", "cargo-test");
    assert_preflight!("td-portal/default-settings.conf", "cargo-test");
    assert_target!("td-portal/Cargo.lock", "check");
    assert_target!("td-portal/Cargo.lock", "recipe-checks");
    assert_preflight!("td-audio/src/main.rs", "cargo-test");
    assert_preflight!("td-audio/src/sys.rs", "cargo-test");
    assert_preflight!("td-audio/src/pcm.rs", "cargo-test");
    assert_preflight!("td-audio/Cargo.lock", "cargo-test");
    assert_target!("td-audio/src/main.rs", "check");
    assert_target!("td-audio/src/main.rs", "recipe-checks");
    assert_target!("td-audio/Cargo.lock", "recipe-checks");
    assert_preflight!("td-profiler/src/main.rs", "cargo-test");
    assert_preflight!("td-profiler/src/perf.rs", "cargo-test");
    assert_preflight!("td-profiler/src/raw.rs", "cargo-test");
    assert_preflight!("td-profiler/Cargo.lock", "cargo-test");
    assert_preflight!("td-profiler/DESIGN.md", "cargo-test");
    assert_target!("td-profiler/src/main.rs", "check");
    assert_target!("td-profiler/src/main.rs", "recipe-checks");
    assert_target!("td-profiler/DESIGN.md", "check");
    assert_target!("td-profiler/DESIGN.md", "recipe-checks");
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
    assert_preflight!("seed/seed-digests.txt", "local-source-digests");
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
    assert_preflight!("td-install/src/main.rs", "cargo-test");
    assert_target!("td-install/src/main.rs", "check");
    // `td-install.rs` links this source statically for the target and
    // `td-install-test.rs` runs THAT binary over a real destination, so a
    // change here can break a target link or move a signature off its offset
    // with host cargo entirely green.
    assert_target!("td-install/src/main.rs", "recipe-checks");
    assert_preflight!("td-install/Cargo.lock", "cargo-test");
    assert_preflight!("td-install/Cargo.toml", "cargo-test");
    // Its `clippy.toml` is not configuration ABOUT the checks, it IS one: the
    // disallowed-path roster that keeps every filesystem call inside the
    // crate's two choke points lives there, so an edit to it has to run the
    // clippy leg that reads it.
    assert_preflight!("td-install/clippy.toml", "cargo-test");
    assert_target!("td-install/clippy.toml", "check");
    // Same shape one level up: `.cargo/config.toml` is not configuration ABOUT
    // the checks, it IS one — it names the runner that bounds every test
    // binary's memory, so an edit runs the tier whose attestation proves the
    // runner still applies.
    assert_preflight!(".cargo/config.toml", "cargo-test");
    assert_preflight!(".cargo/config.toml", "start-bootstrap");
    // recipe-rs is the ONLY tier that runs a musl-target cargo test, so it is
    // the only one that would catch a break confined to the musl runner entry.
    assert_target!(".cargo/config.toml", "recipe-rs");
    // The installer's own specification is documentation, as td-svc's and
    // td-compositor's are: the docs arm runs BEFORE the crate arm, so a spec
    // edit does not drag the crate's checks in behind it.
    assert_no_target!("td-install/DESIGN.md", "check");
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
    assert_preflight!("start", "start-bootstrap");
    assert_preflight!("tests/start.sh", "shell-syntax");
    assert_preflight!("tests/start.sh", "start-bootstrap");
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
    // realfile.rs is in that same class and pinned the same way: td-net
    // `#[path]`-includes it so the signer refuses what the verifier refuses,
    // and without the chain targets an edit here that breaks only the signer
    // would compile td-boot and td-install and build no net at all.
    assert_target!("td-boot/src/realfile.rs", "check");
    assert_target!("td-boot/src/realfile.rs", "recipe-checks");
    assert_target!(
        "td-boot/src/realfile.rs",
        "bootstrap-x86_64-toolchain-store-native"
    );
    assert_target!(
        "td-boot/src/realfile.rs",
        "bootstrap-x86_64-native-gcc-store-native"
    );
    assert_target!("td-boot/src/realfile.rs", "bootstrap-x86_64-self-gcc-store-native");
    assert_preflight!("td-boot/src/realfile.rs", "cargo-test");
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
    assert_contains!("start", "bash tests/start.sh");
    assert_contains!("tests/start.sh", "bash tests/start.sh");

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

/// A standalone target crate the `cargo-test` preflight gates, discovered from
/// the tree rather than listed here.
///
/// APPLICATIONS.md §V.0 named the shape this replaces as the thing concurrent
/// agents collide on: two central tables, each carrying a hardcoded array
/// length, so every pair of agents adding a crate conflicted on the row AND on
/// the count. Both are now derived. A crate joins the gate by EXISTING — a
/// `td-*/Cargo.toml` at the repo root — so adding one touches no file outside
/// the new crate's own directory.
///
/// The prefix is the whole of the discovery rule because it already describes
/// exactly the gated set: the three workspace members are compiled by
/// `--workspace`, `net` is the external-dependency tier whose lock is
/// deliberately NOT dependency-free, and none of them wears it. A crate that
/// wants the gate and cannot wear the prefix is an amendment here, which is the
/// same reviewed act that adding a row used to be.
#[derive(Debug, Clone, PartialEq, Eq)]
struct GateCrate {
    /// The directory name, which is also the crate name and the manifest path
    /// the commands are spelled with: `td-sh`.
    name: String,
    /// Whether clippy lints test and bench targets too. DECLARED rather than
    /// assumed in either direction: turning it on repo-wide is more lint
    /// coverage and a separate reviewed change, and defaulting it on would red
    /// crates never clean under it on a diff that did not touch the target that
    /// is unclean.
    clippy_all_targets: bool,
    /// Extra arguments after `cargo test`'s `--` separator. Declared for the
    /// same reason: `--include-ignored` runs a DIFFERENT suite rather than more
    /// of the same one, and inheriting it repo-wide would run every crate's
    /// deliberately-ignored tests.
    test_args: Option<String>,
}

/// The `td-*` directories under `root` carrying a `Cargo.toml`, alphabetically
/// so a rendered command list never depends on readdir order.
///
/// An unreadable root, an unreadable manifest, and a malformed declaration are
/// ERRORS rather than skips. This list decides which crates are compiled at
/// all, so a discovery that quietly returns fewer of them is the dispatcher
/// narrowing its own coverage — the exact failure `HOST_ONLY_ENGINE_SOURCES`
/// refuses to allow for `affected.rs` itself. An empty answer is refused for
/// the same reason: a preflight looping over nothing exits 0 having compiled
/// nothing.
fn discover_gate_crates(root: &Path) -> Result<Vec<GateCrate>, String> {
    let entries = std::fs::read_dir(root)
        .map_err(|e| format!("{} could not be read: {e}", root.display()))?;
    let mut names: Vec<String> = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| format!("{} could not be walked: {e}", root.display()))?;
        let raw = entry.file_name();
        // Candidacy is decided on the lossy form: whether a name that is not
        // UTF-8 matters is answered below, once we know it is a crate at all.
        if !raw.to_string_lossy().starts_with("td-") {
            continue;
        }
        let path = entry.path();
        // Only a DIRECTORY holding a manifest is a gate member, and both
        // questions come BEFORE any judgement of the name. `read_dir` yields
        // plain files too, so asking about the name first makes a stray
        // `td-notes.txt` — or a `td-sh.orig` backup with no manifest in it —
        // red every check on the branch. `metadata` rather than `is_dir()`, so
        // "not a directory" and "could not tell" stay different answers.
        match std::fs::metadata(&path) {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => continue,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(format!("{} could not be inspected: {e}", path.display())),
        }
        // `is_file()` answers false for a permission error or a symlink loop as
        // readily as for a missing file, which would drop a real crate from the
        // roster without a word.
        let manifest = path.join("Cargo.toml");
        match std::fs::metadata(&manifest) {
            Ok(meta) if meta.is_file() => {}
            // A `td-*` directory that is not a crate is not a gate member.
            Ok(_) => continue,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(format!("{} could not be inspected: {e}", manifest.display())),
        }
        // It IS a crate. Only now must its name be one a command can spell: the
        // name is interpolated into the same `bash -c` string as the declared
        // args, and into a `--manifest-path`. A crate-shaped directory under an
        // unusable name is refused rather than gated, because the alternative is
        // running cargo against a backup copy of a crate.
        let Some(name) = raw.to_str() else {
            return Err(format!(
                "{}: a crate directory whose name is not UTF-8 cannot be gated",
                path.display()
            ));
        };
        let bad = name
            .chars()
            .find(|c| !c.is_ascii_alphanumeric() && *c != '-' && *c != '_');
        if let Some(bad) = bad {
            return Err(format!(
                "{name}: a gated crate directory may not contain `{bad}` — rename \
                 it or move it out of the repo root"
            ));
        }
        names.push(name.to_string());
    }
    names.sort();
    let mut out: Vec<GateCrate> = Vec::new();
    for name in names {
        let manifest = root.join(&name).join("Cargo.toml");
        let text = std::fs::read_to_string(&manifest)
            .map_err(|e| format!("{} could not be read: {e}", manifest.display()))?;
        out.push(parse_gate_crate(&name, &text)?);
    }
    if out.is_empty() {
        return Err(format!(
            "no `td-*/Cargo.toml` under {} — the gate roster must not be empty",
            root.display()
        ));
    }
    Ok(out)
}

/// The table a crate declares its gating in, normalized (no brackets).
const GATE_SECTION: &str = "package.metadata.td-gate";

/// A section header line reduced to the table it names: no trailing comment, no
/// brackets, no per-segment quoting, no incidental whitespace.
///
/// `[ package.metadata."td-gate" ] # note` and `[package.metadata.td-gate]` are
/// the same table to cargo, so they must be the same table here — otherwise the
/// spellings cargo accepts are exactly the ones that read as silence.
fn normalize_header(line: &str) -> Option<(String, bool)> {
    let head = line.split('#').next().unwrap_or(line).trim();
    let inner = head.strip_prefix('[')?.strip_suffix(']')?;
    // `[[x]]` is an array of tables, not the table. Unwrapping the second pair
    // lets the NEAR-MISS check below see it; `array` keeps it from being read
    // as a declaration.
    let (inner, array) = match inner.strip_prefix('[').and_then(|i| i.strip_suffix(']')) {
        Some(i) => (i, true),
        None => (inner, false),
    };
    let mut out = String::new();
    for seg in inner.split('.') {
        if !out.is_empty() {
            out.push('.');
        }
        out.push_str(seg.trim().trim_matches(['"', '\'']).trim());
    }
    Some((out, array))
}

/// Whether a normalized header is a NEAR MISS for the gate table: the same
/// block, misspelled. Case and `_`/`-` are folded because those are the typos a
/// manifest author actually makes.
fn resembles_gate_section(header: &str) -> bool {
    let relaxed = header.replace('_', "-").to_ascii_lowercase();
    if relaxed == GATE_SECTION {
        return true;
    }
    match relaxed.strip_prefix("package.metadata.") {
        Some(rest) => rest.starts_with("td"),
        None => false,
    }
}

/// The optional `[package.metadata.td-gate]` block, over the manifest TEXT so
/// its cases are literals in the test rather than a fixture tree.
///
/// Cargo ignores `package.metadata`, which makes it the crate's own place to
/// say how it wants to be gated without a central table hearing about it.
/// Absent means the defaults, which is what most crates want and what makes
/// adding one a zero-central-edit act. An UNKNOWN key is an error rather than a
/// shrug: a mistyped `clippy-all-targets` that parsed as nothing would silently
/// drop lint coverage, which is the failure this whole roster exists to stop.
fn parse_gate_crate(name: &str, manifest: &str) -> Result<GateCrate, String> {
    let mut out = GateCrate {
        name: name.to_string(),
        clippy_all_targets: false,
        test_args: None,
    };
    let mut inside = false;
    let mut in_metadata_root = false;
    for line in manifest.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            let Some((header, array)) = normalize_header(line) else {
                return Err(format!("{name}: `{line}` is not a section header"));
            };
            inside = !array && header == GATE_SECTION;
            in_metadata_root = !array && header == "package.metadata";
            // A NEAR MISS is an error rather than an unrelated table. The
            // per-key strictness below buys nothing if the block that NAMES
            // those keys can be misspelled into silence: `td_gate`, `td-gat`
            // and `TD-GATE` would each take every default and say nothing,
            // which is the exact failure the unknown-key refusal exists to
            // prevent. Anything else under `package.metadata.td*` is refused
            // too — a future unrelated `td-` table there is a reviewed rename,
            // which is cheaper than a silent default.
            if !inside && resembles_gate_section(&header) {
                // `[[package.metadata.td-gate]]` reaches here too: an array of
                // tables is not the table, and reading it as one would be a
                // guess about what its author meant.
                return Err(format!(
                    "{name}: `[{header}]` is not `[{GATE_SECTION}]` — fix the \
                     name or remove it, rather than leaving a block that \
                     declares nothing"
                ));
            }
            continue;
        }
        // Cargo also accepts the dotted (`td-gate.clippy-all-targets = …`) and
        // inline-table (`td-gate = { … }`) spellings of the same table. This
        // parser reads neither, so it REFUSES them rather than taking the
        // defaults and saying nothing.
        if in_metadata_root {
            if let Some((key, _)) = line.split_once('=') {
                // Folded the way the header is, and with every quote removed
                // rather than trimmed: `"td-gate".clippy-all-targets` leaves a
                // quote in the middle that a trim would keep.
                let key: String = key
                    .chars()
                    .filter(|c| *c != '"' && *c != '\'' && !c.is_whitespace())
                    .collect::<String>()
                    .replace('_', "-")
                    .to_ascii_lowercase();
                if key == "td-gate" || key.starts_with("td-gate.") {
                    return Err(format!(
                        "{name}: spell the gate block as `[{GATE_SECTION}]`, \
                         not as `{line}`"
                    ));
                }
            }
        }
        if !inside || line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            return Err(format!("{name}: `{line}` is not `key = value`"));
        };
        match key.trim() {
            "clippy-all-targets" => {
                out.clippy_all_targets = gate_bool(name, "clippy-all-targets", value.trim())?;
            }
            "test-args" => out.test_args = Some(gate_string(name, "test-args", value.trim())?),
            other => {
                return Err(format!(
                    "{name}: unknown [package.metadata.td-gate] key `{other}`"
                ));
            }
        }
    }
    Ok(out)
}

/// A declared boolean. The first word, so a trailing `# comment` is tolerated
/// the way the rest of these manifests write one.
fn gate_bool(krate: &str, key: &str, value: &str) -> Result<bool, String> {
    // Up to a trailing comment, which TOML does not require a space before.
    // `gate_string` gets the same tolerance for free by halting at its closing
    // quote, and the two should not disagree about what a comment is.
    match value.split('#').next().unwrap_or(value).trim() {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(format!("{krate}: `{key}` wants true or false, got `{value}`")),
    }
}

/// A declared string, read to its closing quote so a trailing comment is
/// tolerated. Empty is refused: it would render a `--` separator with nothing
/// after it and change the command for no stated reason.
fn gate_string(krate: &str, key: &str, value: &str) -> Result<String, String> {
    let rest = value
        .strip_prefix('"')
        .ok_or_else(|| format!("{krate}: `{key}` wants a quoted string, got `{value}`"))?;
    let end = rest
        .find('"')
        .ok_or_else(|| format!("{krate}: `{key}` has no closing quote: `{value}`"))?;
    let inner = rest.get(..end).unwrap_or_default();
    if inner.trim().is_empty() {
        return Err(format!("{krate}: `{key}` is empty"));
    }
    // These args are interpolated into a command string that `run_shell` hands
    // to `bash -c`, so they are VALIDATED rather than quoted: quoting would
    // change the rendered command a reader is asked to trust, and no legitimate
    // cargo flag needs a shell metacharacter. Before this commit both halves of
    // that string were Rust consts; a manifest can now contribute to it.
    if let Some(bad) = inner.chars().find(|c| !is_shell_safe(*c)) {
        return Err(format!(
            "{krate}: `{key}` may not contain `{bad}` — it is interpolated \
             into a shell command"
        ));
    }
    Ok(inner.to_string())
}

/// The characters a declared value may contribute to a `bash -c` string: enough
/// for any cargo flag, and nothing a shell acts on.
fn is_shell_safe(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '=' | ',' | '.' | '/' | '+' | ':' | ' ')
}

/// How many `[[package]]` entries the workspace lock must carry: one per
/// member, since the workspace is dependency-free. Read from the root manifest
/// so adding a member does not need a count changed here as well.
fn workspace_member_count(root: &Path) -> Result<usize, String> {
    let manifest = root.join("Cargo.toml");
    let text = std::fs::read_to_string(&manifest)
        .map_err(|e| format!("{} could not be read: {e}", manifest.display()))?;
    for line in text.lines() {
        let Some(rest) = line.trim().strip_prefix("members") else {
            continue;
        };
        let Some(rest) = rest.trim_start().strip_prefix('=') else {
            continue;
        };
        let Some(rest) = rest.trim_start().strip_prefix('[') else {
            return Err("root Cargo.toml `members` is not an inline array".to_string());
        };
        let Some(end) = rest.find(']') else {
            return Err("root Cargo.toml `members` does not close on its line".to_string());
        };
        let n = rest
            .get(..end)
            .unwrap_or_default()
            .split(',')
            .filter(|s| !s.trim().is_empty())
            .count();
        if n == 0 {
            return Err("root Cargo.toml lists no workspace members".to_string());
        }
        return Ok(n);
    }
    Err("root Cargo.toml has no `members =` line".to_string())
}

/// Every lock this preflight answers for, and the `[[package]]` count it must
/// carry: one per member for the workspace root, one apiece for the standalone
/// crates.
///
/// Gate 325 asserts these too, but it degrades to a tolerated Unprovisioned
/// SKIP on every host today (re #469) and names this preflight the authoritative
/// enforcement — so in the only tier that executes, this is where the
/// dependency-free claim is actually checked. `--frozen` does not stand in: it
/// demands that the committed lock RESOLVE, not that it be empty.
fn dependency_free_locks(root: &Path) -> Result<Vec<(String, usize)>, String> {
    let mut out = vec![("Cargo.lock".to_string(), workspace_member_count(root)?)];
    for krate in discover_gate_crates(root)? {
        out.push((format!("{}/Cargo.lock", krate.name), 1));
    }
    Ok(out)
}

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

/// The subset of the derived command list a diff over `changed` can actually
/// invalidate.
///
/// Narrow ONLY where it is provably sound: every changed path must sit inside
/// an `UNEMBEDDED_CRATES` directory. Anything else — `builder/`, `recipes/`, a
/// crate a recipe embeds, an unmapped file, an empty diff — takes the whole
/// list, so every unknown fails safe.
fn cargo_test_cmds(root: &Path, changed: &[String]) -> Result<Vec<String>, String> {
    let all = cargo_test_cmds_all(root)?;
    let mut scoped: Vec<&'static str> = Vec::new();
    for p in changed {
        // A `..` is refused rather than resolved: `td-review/../td-sh/x.rs`
        // starts with the crate and names another. git never emits one, so this
        // is reachable only through `--path`.
        if p.contains("..") {
            return Ok(all);
        }
        let owner = UNEMBEDDED_CRATES.iter().find(|c| {
            // The separator matters: `td-reviewer/x` is not `td-review/x`.
            p.starts_with(*c) && p.as_bytes().get(c.len()) == Some(&b'/')
        });
        match owner {
            Some(c) if !scoped.contains(c) => scoped.push(c),
            Some(_) => {}
            None => return Ok(all),
        }
    }
    if scoped.is_empty() {
        return Ok(all);
    }
    let narrowed: Vec<String> = all
        .iter()
        .filter(|cmd| {
            cmd_manifest_crate(cmd).is_some_and(|c| scoped.contains(&c))
        })
        .cloned()
        .collect();
    // An EMPTY narrowing must never be taken at face value: the preflight's
    // loop over nothing exits 0 having run no cargo at all — a green that
    // tested the crate not at all. `unembedded_crates_have_cargo_commands`
    // keeps this unreachable; this is what happens if it ever is not.
    match narrowed.is_empty() {
        true => Ok(all),
        false => Ok(narrowed),
    }
}

/// The crate a `--manifest-path <crate>/Cargo.toml` command names; None for the
/// `--workspace` ones, which belong to no single crate.
fn cmd_manifest_crate(cmd: &str) -> Option<&str> {
    Some(cmd.split_once("--manifest-path ")?.1.split_once('/')?.0)
}

/// What the `cargo-test` preflight runs, in order: every `cargo test` before
/// every `cargo clippy`, the workspace before the standalone crates.
///
/// Derived from the SAME roster the lock guard reads, so the two can no longer
/// be two hand-written copies of one crate set that drift apart — which is what
/// `every_crate_the_preflight_tests_has_its_lock_guarded` and its reverse were
/// written to catch after `td-install` landed guarded but uncompiled.
fn cargo_test_cmds_all(root: &Path) -> Result<Vec<String>, String> {
    let crates = discover_gate_crates(root)?;
    let mut out = vec!["cargo test --frozen --workspace".to_string()];
    for k in &crates {
        let mut cmd = format!("cargo test --frozen --manifest-path {}/Cargo.toml", k.name);
        if let Some(args) = &k.test_args {
            cmd.push_str(" -- ");
            cmd.push_str(args);
        }
        out.push(cmd);
    }
    out.push("cargo clippy --frozen --workspace".to_string());
    for k in &crates {
        let mut cmd = format!("cargo clippy --frozen --manifest-path {}/Cargo.toml", k.name);
        if k.clippy_all_targets {
            cmd.push_str(" --all-targets");
        }
        out.push(cmd);
    }
    Ok(out)
}

fn run_preflight(root: &Path, name: &str, changed: &[String]) -> i32 {
    match name {
        "shell-syntax" => run_shell(root, "bash -n start tests/*.sh ci/*.sh tools/*.sh"),
        "start-bootstrap" => run_shell(root, "bash tests/start.sh"),
        "heal-revert" => run_shell(root, "bash tests/heal-revert.sh"),
        // BOTH engine crates, tests AND clippy: the AGENTS.md deny-lints only
        // fire under the clippy driver, and the in-loop cargo-test gate (325)
        // is unreachable while the loop is UNPROVISIONED (re #469) — this
        // host preflight is the per-PR enforcement in the meantime (review
        // finding: recipes tests + clippy ran in NO automated per-PR tier).
        "cargo-test" => {
            // Before any cargo call, and for EVERY lock rather than only the
            // crate whose path selected this: gate 325 asserts these, and gate
            // 325 does not run (see `dependency_free_locks`). So a td-sh-only
            // branch reds here on a td-review lock — deliberate. The claim is
            // repo-wide, the roster is the same either way, and a guard that
            // only looks where the diff already pointed is one that never
            // catches the crate nobody was looking at.
            let locks = match dependency_free_locks(root) {
                Ok(locks) => locks,
                Err(e) => {
                    eprintln!("affected-checks: {e}");
                    return 1;
                }
            };
            for (lock, packages) in locks {
                if let Err(e) = assert_dependency_free(root, &lock, packages) {
                    eprintln!("affected-checks: {e}");
                    return 1;
                }
            }
            // The target-built guest programs ride the SAME preflight: all are
            // dependency-free pure std, while their static TARGET links ride
            // recipe-checks. builder + recipes + the shared engine lib are one
            // cargo workspace, so --workspace lints/tests all three in one
            // invocation; every `td-*` crate is standalone and rides the
            // preflight explicitly, discovered rather than listed (the roster
            // above). td-sh's conformance corpus run is NOT `#[ignore]`d: this
            // plain `cargo test` runs the whole corpus, so a regression, an
            // unexpected pass or a stale overlay entry reds the preflight
            // rather than waiting for a tier nothing runs. td-review goes the
            // other way: its App-level tests drive a real git repo, are
            // `#[ignore]`d so the git-less sandbox gate stays honest, and run
            // HERE through the `test-args` it declares — this preflight is
            // their only tier.
            let cmds = match cargo_test_cmds(root, changed) {
                Ok(cmds) => cmds,
                Err(e) => {
                    eprintln!("affected-checks: {e}");
                    return 1;
                }
            };
            for cmd in cmds {
                let code = run_shell(root, &cmd);
                if code != 0 {
                    return code;
                }
            }
            0
        }
        "net-test" => run_shell(
            root,
            "CC=gcc cargo test --frozen --manifest-path net/Cargo.toml",
        ),
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
    print!("{}", format_output(&root, &header, &changed, &sel, run));

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
        let locks = gate_locks();
        let Some((first, _)) = locks.first() else {
            panic!("the lock roster is empty");
        };
        assert!(
            root.join(first).is_file(),
            "{first} absent at {} — the lock roster's own first entry",
            root.display()
        );
    }

    /// The three derived rosters, resolved against the real tree. Tests may
    /// panic where production code may not (AGENTS.md, 'Rust code'), and a
    /// roster that cannot be read is a broken checkout rather than a finding.
    fn gate_roster() -> Vec<GateCrate> {
        discover_gate_crates(&repo_root()).expect("gate roster")
    }

    fn gate_cmds() -> Vec<String> {
        cargo_test_cmds_all(&repo_root()).expect("cargo command list")
    }

    fn gate_locks() -> Vec<(String, usize)> {
        dependency_free_locks(&repo_root()).expect("lock roster")
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

    /// The ed25519 confinement scan reads CODE, so the comment strip it reads
    /// through has to keep code and drop prose — and its dangerous failure is
    /// the silent one, a strip that removes too much and blinds the scan
    /// without failing anything.
    #[test]
    fn the_comment_strip_keeps_code_and_drops_prose() {
        // The shape that made this necessary: td-boot's header names the
        // signer in order to say it is absent.
        assert!(!strip_line_comments("// ed25519_sign.rs is deliberately absent")
            .contains("ed25519_sign"));
        assert!(!strip_line_comments("    //! reaches ed25519_sign").contains("ed25519_sign"));
        // Code survives, including code with a comment after it — the case a
        // naive "drop any line containing //" would lose.
        assert!(strip_line_comments("mod ed25519_sign; // the signer")
            .contains("mod ed25519_sign;"));
        assert!(strip_line_comments("let x = 1;\nlet y = 2;").contains("let y = 2;"));
        // And the strip must not join lines: two declarations on separate lines
        // stay on separate lines, or a needle could straddle the seam.
        assert_eq!(strip_line_comments("a // x\nb").lines().count(), 2);
        // A `//` inside a string literal is not a comment. Cutting there would
        // discard the rest of a real line and HIDE what follows from the scan,
        // which is the one direction this must not fail in.
        assert!(
            strip_line_comments(r#"let u = "http://x"; mod ed25519_sign;"#)
                .contains("mod ed25519_sign;"),
            "a URL must not swallow the declaration after it"
        );
        // Balanced quotes then a real comment still cuts.
        assert!(!strip_line_comments(r#"let u = "x"; // ed25519_sign"#).contains("ed25519_sign"));
        // The evasion a reviewer proposed against the first draft: a DOUBLED
        // SLASH inside a path string. `src//ed25519_sign.rs` is the same file to
        // Linux, so a stripper that cut there would let the include through.
        assert!(
            strip_line_comments(r#"#[path = "../../engine/src//ed25519_sign.rs"] mod signing;"#)
                .contains("ed25519_sign"),
            "a doubled slash inside a path string must not hide the include"
        );
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

    /// Source with COMMENTS cut away, STRING-AWARE, with string state reset
    /// per LINE.
    ///
    /// String-aware because a `//` inside a literal is not a comment, and
    /// cutting there deletes the rest of the line — including a `--name-only`
    /// this scan exists to see; `git -c url.https://x.insteadOf=y ...` is the
    /// shape that does it.
    ///
    /// Per line because a plain quote toggle is WRONG about this tree: a
    /// `'"'` char literal opens a string that never closes, and `builder/src`
    /// is full of them (`drv.rs`, `oci.rs`, `stage0.rs`, `check_loop.rs`,
    /// `main.rs`). Resetting bounds what one costs to its own line. The price
    /// is that a literal genuinely spanning lines is read as CODE from its
    /// second line on, which callers that can rule those literals out should
    /// not pay — `lex` is that door.
    ///
    /// BLOCK comments go too, and that is not tidiness: a comment is the one
    /// place `--no-renames` can sit inside an argv without reaching git, so
    /// `/* TODO: add --no-renames */` beside a bare query would satisfy the
    /// rule outright. They NEST in Rust, so the depth is counted rather than
    /// the first `*/` taken. Depth crosses lines in BOTH forms: a block
    /// comment has no per-line ambiguity to protect against.
    fn strip_comments(text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        let mut depth = 0usize;
        for line in text.lines() {
            let mut in_str = false;
            let mut esc = false;
            let mut kept = String::with_capacity(line.len());
            let mut it = line.char_indices().peekable();
            while let Some((i, c)) = it.next() {
                let rest = line.get(i..).unwrap_or_default();
                if depth > 0 {
                    if rest.starts_with("/*") {
                        depth = depth.saturating_add(1);
                        it.next();
                    } else if rest.starts_with("*/") {
                        depth = depth.saturating_sub(1);
                        it.next();
                    }
                    continue;
                }
                if in_str {
                    kept.push(c);
                    match c {
                        _ if esc => esc = false,
                        '\\' => esc = true,
                        '"' => in_str = false,
                        _ => {}
                    }
                    continue;
                }
                if rest.starts_with("//") {
                    break;
                }
                if rest.starts_with("/*") {
                    depth = 1;
                    it.next();
                    continue;
                }
                if c == '"' {
                    in_str = true;
                }
                kept.push(c);
            }
            out.push_str(&kept);
            out.push('\n');
        }
        out
    }

    /// The length in CHARS of the char literal at `at`, or `None` where that
    /// `'` opens a LIFETIME or a loop label instead.
    ///
    /// Both spellings begin with the same character, and telling them apart is
    /// what lets `lex` rewrite a literal whose CONTENT would desynchronise
    /// something downstream — `'"'` is a string delimiter to every quote
    /// toggle that reads `lex`'s output, and `'['`/`']'` are brackets to
    /// `bracket_spans`, which would hand back a span belonging to a different
    /// argv.
    ///
    /// It is decidable by LOOKAHEAD, which is how rustc decides it: a literal
    /// is one character or one escape between two quotes, and a lifetime is an
    /// identifier with no closing quote at all — so `'a'` is a literal and
    /// `'a` in `&'a str` is not, whatever follows either. The earlier form
    /// matched four fixed SPELLINGS instead and so had no notion of a closing
    /// quote, which review walked out through: `'a'['b']` matched `'['` across
    /// the close of `'a'` and deleted a bracket, which can close an argv span
    /// early and let a bare query inherit a neighbour's flag. Recognising the
    /// literal WHOLE is what retires that, and with it the caveat that this
    /// only held outside a macro token tree.
    fn char_literal_len(chars: &[char], at: usize) -> Option<usize> {
        let get = |n: usize| chars.get(at.saturating_add(n)).copied();
        let hex = |n: usize| get(n).is_some_and(|c| c.is_ascii_hexdigit());
        if get(0) != Some('\'') {
            return None;
        }
        // The index of the char that must be the CLOSING quote. A lifetime
        // never begins with a backslash, so `'\` is a literal whatever
        // follows; what the escape decides is only how long it is.
        let close = match get(1)? {
            '\\' => match get(2)? {
                'x' if hex(3) && hex(4) => 5,
                'u' => {
                    if get(3) != Some('{') {
                        return None;
                    }
                    // Six is Rust's own cap on the digits, so a run longer
                    // than that is not a literal and must not be walked past.
                    let digits = (4..10).take_while(|n| hex(*n)).count();
                    let end = 4usize.saturating_add(digits);
                    if get(end) != Some('}') {
                        return None;
                    }
                    end.saturating_add(1)
                }
                'x' => return None,
                _ => 3,
            },
            // `''` is not a literal, and reading it as one would consume the
            // quote that opens the NEXT one.
            '\'' | '\n' | '\r' | '\t' => return None,
            _ => 2,
        };
        (get(close) == Some('\'')).then(|| close.saturating_add(1))
    }

    /// Whether the `r` at `at` OPENS a raw string rather than ending an
    /// identifier.
    ///
    /// Asked only from CODE state, which is what makes it answerable: inside a
    /// string `"-r"` the question never arises, and that is the false red this
    /// replaced — two rounds of review wrote `"other"` and `"-r"` followed by a
    /// comment ending in `\"`, and watched a heuristic read the text between
    /// them as raw content. In code the only remaining question is whether the
    /// `r` ends an identifier, since `foor"x"` is not a raw string and `foo
    /// r"x"` is.
    fn opens_raw(chars: &[char], at: usize) -> bool {
        let back = |n: usize| at.checked_sub(n).and_then(|p| chars.get(p)).copied();
        let boundary = match back(1) {
            // The prefix letters are not the boundary, so it is one further
            // back; anything else IS the boundary. `f` is here for `lex`'s
            // reason: it is not a prefix Rust has, and reading one it never
            // adds costs nothing.
            Some('b' | 'c' | 'f') => back(2),
            other => other,
        };
        match boundary {
            None => true,
            // `#` is part of an identifier here rather than a boundary,
            // because `r#r"x"` is a RAW IDENTIFIER named `r` followed by an
            // ordinary string — review read the second `r` as a raw-string
            // opener, which desynchronises the rest of the file. It costs a
            // raw string opening immediately after a `#`, which needs either
            // a just-CLOSED raw string (`r#""#r"`) or a macro token tree, and
            // no valid Rust in this tree writes either.
            Some(c) => !c.is_alphanumeric() && c != '_' && c != '#',
        }
    }

    /// `rest` past the whitespace and comments that decide nothing, which is
    /// what rustc skips before asking whether a `#!` opened a shebang or an
    /// inner attribute.
    fn past_trivia(rest: &str) -> &str {
        let mut at = rest.trim_start();
        loop {
            at = match at.strip_prefix("//") {
                Some(line) => match line.split_once('\n') {
                    Some((_, after)) => after.trim_start(),
                    None => "",
                },
                None => match at.strip_prefix("/*") {
                    // Not nested: `/*` inside is a comment to rustc too, and
                    // the only question here is where the FIRST `[` is.
                    Some(block) => match block.split_once("*/") {
                        Some((_, after)) => after.trim_start(),
                        None => "",
                    },
                    None => return at,
                },
            };
        }
    }

    /// `text` with comments cut away and every RAW string rewritten as an
    /// ordinary one.
    ///
    /// TOTAL: there is no text it declines to read. An earlier form returned
    /// the one construct it could not settle and the caller skipped that file,
    /// which review showed was the silent skip in a new place — the escape was
    /// answered by `strip_comments`, a per-line toggle that loses a query for
    /// three reasons of which prose is only one, so a file carrying both a
    /// query and a `'"'` went unjudged. Five `builder/src` files carry that
    /// literal today.
    ///
    /// This is a lexer rather than a quote toggle, and it is here because the
    /// toggle kept being wrong in ways that were silent PASSES. It reads what
    /// Rust reads: a shebang first line, line and nested block comments,
    /// ordinary strings with their escapes — across lines, which a toggle
    /// resetting per line gets wrong — and raw strings by hash count.
    ///
    /// It REWRITES rather than merely reads them because `bracket_spans` and
    /// `string_span` see this output and are quote toggles themselves. A raw
    /// string's content may hold a bare `"`, which would desynchronise both to
    /// the end of the file; emitted as an ordinary literal, with `"` and `\`
    /// escaped, it means the same thing to them. Nothing a query is made of —
    /// a flag, a subcommand, a path — is changed by that escaping.
    fn lex(text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        // A BOM is not Rust either, and rustc removes it BEFORE looking for a
        // shebang — so a file carrying both would keep its shebang here and
        // read it as code.
        let start = text.strip_prefix('\u{feff}').unwrap_or(text);
        // A shebang is not Rust and rustc skips it whole. It may hold anything,
        // an unmatched quote included, and reading one as code carried a string
        // into the file under it.
        // `#! [attr]` with a space is an INNER ATTRIBUTE to rustc, which skips
        // whitespace AND COMMENTS before deciding, so the bracket is looked for
        // past both. Whitespace alone was not enough: `#!/*` opening a comment
        // that closes lines later is an attribute to rustc and was a shebang
        // here, and cutting only its first line left the REST of that comment
        // to be read as code — an unmatched quote in it then swallowed the
        // file, silently.
        let body = match start.strip_prefix("#!") {
            Some(rest) if !past_trivia(rest).starts_with('[') => match rest.split_once('\n') {
                Some((_, after)) => {
                    // Its NEWLINE survives, as a comment's does: what is left
                    // has the line structure the file had, and a line number
                    // read off this output is the file's own.
                    out.push('\n');
                    after
                }
                None => "",
            },
            _ => start,
        };
        let chars: Vec<char> = body.chars().collect();
        let mut i = 0usize;
        while let Some(&c) = chars.get(i) {
            let next = chars.get(i.saturating_add(1)).copied();
            // A char literal, in code, is rewritten to a fixed harmless one:
            // its content is never part of a query, and left alone it can be
            // a quote or a bracket to something reading this output.
            // Neutralised rather than refused, since `lex` having no failure
            // to report is what leaves the scan no path that DECLINES to
            // judge a file — where its last silent skip lived. Read before
            // the quote below, since a literal's own quotes are the
            // misreading being closed.
            if c == '\'' {
                if let Some(len) = char_literal_len(&chars, i) {
                    out.push_str("'x'");
                    i = i.saturating_add(len);
                    continue;
                }
            }
            // Comments next: inside neither a string nor a raw one, `//` and
            // `/*` are the only things that are not code.
            if c == '/' && next == Some('/') {
                while chars.get(i).is_some_and(|ch| *ch != '\n') {
                    i = i.saturating_add(1);
                }
                continue;
            }
            if c == '/' && next == Some('*') {
                let mut depth = 1usize;
                i = i.saturating_add(2);
                while depth > 0 {
                    match (chars.get(i).copied(), chars.get(i.saturating_add(1)).copied()) {
                        (None, _) => break,
                        (Some('/'), Some('*')) => {
                            depth = depth.saturating_add(1);
                            i = i.saturating_add(2);
                        }
                        (Some('*'), Some('/')) => {
                            depth = depth.saturating_sub(1);
                            i = i.saturating_add(2);
                        }
                        // Newlines survive a comment, so what is left still has
                        // the line structure the file had.
                        (Some('\n'), _) => {
                            out.push('\n');
                            i = i.saturating_add(1);
                        }
                        _ => i = i.saturating_add(1),
                    }
                }
                continue;
            }
            // `f` is not a prefix Rust has: `fr"x"` is a reserved-prefix ERROR
            // today, so no valid file holds one. It is here because the cost of
            // being wrong is asymmetric — an unrecognised prefix is read as an
            // identifier and then an ordinary string, which for `fr"\"` cuts the
            // rest of the line and passes a bare query silently — while the cost
            // of recognising one Rust never adds is nothing, that spelling being
            // unwritable.
            if c == 'r' || (matches!(c, 'b' | 'c' | 'f') && next == Some('r')) {
                let at = if c == 'r' { i } else { i.saturating_add(1) };
                let mut j = at.saturating_add(1);
                let mut hashes = 0usize;
                while chars.get(j) == Some(&'#') {
                    hashes = hashes.saturating_add(1);
                    j = j.saturating_add(1);
                }
                if chars.get(j) == Some(&'"') && opens_raw(&chars, at) {
                    out.push('"');
                    i = j.saturating_add(1);
                    // A raw string ends at the FIRST `"` followed by its own
                    // hash count, and holds no escapes at all — which is why a
                    // toggle read `r"\"` as still open and this does not.
                    loop {
                        match chars.get(i).copied() {
                            None => break,
                            Some('"') if (1..=hashes)
                                .all(|k| chars.get(i.saturating_add(k)) == Some(&'#')) =>
                            {
                                out.push('"');
                                i = i.saturating_add(hashes).saturating_add(1);
                                break;
                            }
                            Some(ch) => {
                                if ch == '"' || ch == '\\' {
                                    out.push('\\');
                                }
                                out.push(ch);
                                i = i.saturating_add(1);
                            }
                        }
                    }
                    continue;
                }
            }
            if c == '"' {
                out.push(c);
                i = i.saturating_add(1);
                let mut esc = false;
                while let Some(&ch) = chars.get(i) {
                    out.push(ch);
                    i = i.saturating_add(1);
                    match ch {
                        _ if esc => esc = false,
                        '\\' => esc = true,
                        '"' => break,
                        _ => {}
                    }
                }
                continue;
            }
            out.push(c);
            i = i.saturating_add(1);
        }
        out
    }


    /// `--no-renames` present as an ARGUMENT of `span` rather than anywhere in
    /// its bytes. `&["diff", "--format=--no-renames", "--name-only"]` names the
    /// flag without ever passing it, and a `contains` reads that as compliance.
    ///
    /// `--no-renames` present as a WHOLE ARGUMENT of `span` rather than
    /// anywhere in its bytes. `&["diff", "--format=--no-renames",
    /// "--name-only"]` names the flag without ever passing it, and a
    /// `contains` reads that as compliance.
    ///
    /// WHAT SPLITS the span into arguments is the difference between the two
    /// kinds, which is why the caller says which it found, and neither split
    /// is a rule about the characters ADJACENT to the flag — three rounds of
    /// review took that apart one adjacency at a time. A SHELL string is
    /// split by the shell on whitespace, and the quotes that survived Rust's
    /// own escaping are then removed from each word: `git diff
    /// \"--no-renames\"` really does pass the flag, while
    /// `core.pager='--no-renames'` is ONE word forming the config value
    /// `core.pager=--no-renames`, which git accepts and which is not the
    /// flag. An ARGV literal is split by Rust at its unescaped quotes, so an
    /// argument is the whole text between two of them — `"ignored
    /// --no-renames"` is one argument git never reads the flag out of, and
    /// the escaped quotes `lex` writes when it rewrites a raw string are
    /// content rather than a split.
    fn passes_no_renames(span: &str, needle_at: Option<usize>) -> bool {
        const FLAG: &str = "--no-renames";
        if let Some(at) = needle_at {
            let command = shell_command_at(span, at);
            return shell_words(command).iter().any(|word| word == FLAG);
        }
        let mut inside = false;
        let mut argument = String::new();
        let mut esc = false;
        // `--` ends git's option parsing, so everything after it is a
        // pathspec: `&["diff", "--name-only", "--", "--no-renames"]` names a
        // FILE by that name and passes git no flag at all.
        let mut pathspecs = false;
        for (i, c) in span.char_indices() {
            if esc {
                argument.push(c);
                esc = false;
                continue;
            }
            match c {
                '\\' if inside => {
                    esc = true;
                    argument.push(c);
                }
                '"' => {
                    // A DIRECT element of the array, which is what an argument
                    // is: a literal inside a nested expression is not one, and
                    // `concat!("core.pager=x ", "--no-renames")` builds a
                    // single CONFIG VALUE git accepts while lending the argv a
                    // flag it never receives. Read off what FOLLOWS the
                    // closing quote, since that is where an element ends.
                    let after = span
                        .get(i.saturating_add(1)..)
                        .and_then(|t| t.trim_start().chars().next());
                    if inside && matches!(after, Some(',' | ']') | None) {
                        if argument == "--" {
                            pathspecs = true;
                        } else if !pathspecs && argument == FLAG {
                            return true;
                        }
                    }
                    argument.clear();
                    inside = !inside;
                }
                _ if inside => argument.push(c),
                _ => {}
            }
        }
        false
    }

    /// The declaration this tree opens its test modules with.
    const TEST_MOD: &str = "#[cfg(test)]\nmod tests";

    /// `lexed` with its test module CUT OUT, or `None` where the module cannot
    /// be found whole.
    ///
    /// Cut out rather than truncated at, which is the difference between
    /// scanning a file and scanning the top of one. An earlier form kept
    /// everything ABOVE the marker and so read a file's tail only when the
    /// module happened to end it — `recipes/src/recipes/td-sh.rs` declares
    /// `pub fn recipe()` 32 lines BELOW its tests, and that code went unjudged
    /// with nothing downstream able to say so: the `checked > 0` control stays
    /// satisfied by the other files. Review found the hazard by construction;
    /// the live file was found by looking.
    ///
    /// Excluded rather than scanned because the tests below must issue a bare
    /// `--name-only` as their positive control, and a rule that refused those
    /// would be a rule against demonstrating itself.
    ///
    /// Split at the test MODULE rather than at the first `#[cfg(test)]`: that
    /// attribute also gates single items — `builder/src/main.rs` has one on a
    /// `mod` DECLARATION at line 30 — and splitting there discarded the other
    /// 13 000 lines of that file while the count downstream stayed satisfied
    /// by other files.
    ///
    /// TWICE is refused rather than resolved: nothing in this tree writes two,
    /// so refusing costs nothing and guessing costs the scan. Both the marker
    /// and the braces are read off `blank_strings(lexed)`, so a literal
    /// holding either is data rather than structure — `lex` has already taken
    /// the comments, which leaves no way to write one that is not code.
    fn shipped_half(lexed: &str) -> Option<String> {
        let code = blank_strings(lexed);
        if code.matches(TEST_MOD).count() > 1 {
            return None;
        }
        let Some(at) = code.find(TEST_MOD) else {
            return Some(lexed.to_string());
        };
        let body = at.saturating_add(TEST_MOD.len());
        // `mod tests;` would send the walk below to the next brace group in
        // the file and cut THAT out instead, which is a silent under-scan of
        // whatever lies between. Nothing here writes one, so it is refused.
        if !code.get(body..)?.trim_start().starts_with('{') {
            return None;
        }
        let mut depth = 0usize;
        let mut end = None;
        for (n, c) in code.get(body..)?.char_indices() {
            match c {
                '{' => depth = depth.saturating_add(1),
                '}' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        end = Some(body.saturating_add(n).saturating_add(1));
                        break;
                    }
                }
                _ => {}
            }
        }
        let (above, below) = (lexed.get(..at)?, lexed.get(end?..)?);
        Some(format!("{above}{below}"))
    }

    /// `lexed` with the CONTENT of every string literal blanked, byte offsets
    /// preserved so a position found in the result indexes the input.
    ///
    /// Only sound on `lex`'s output, where every string is an ordinary one
    /// with its escapes intact: raw strings are already rewritten and comments
    /// are already gone, so a quote here always delimits. Newlines survive for
    /// `lex`'s reason — what is left has the line structure the file had.
    fn blank_strings(lexed: &str) -> String {
        let mut out = String::with_capacity(lexed.len());
        let mut chars = lexed.chars();
        while let Some(c) = chars.next() {
            out.push(c);
            if c != '"' {
                continue;
            }
            let mut esc = false;
            for ch in chars.by_ref() {
                let closing = !esc && ch == '"';
                esc = !esc && ch == '\\';
                match ch {
                    _ if closing => out.push('"'),
                    '\n' => out.push('\n'),
                    // Spaces per BYTE rather than per char: the offsets this
                    // hands back index the input, which is the whole of why
                    // the blanking is length-preserving.
                    other => out.extend(std::iter::repeat_n(' ', other.len_utf8())),
                }
                if closing {
                    break;
                }
            }
        }
        out
    }

    /// The one COMMAND of `span` that holds the needle at `at`.
    ///
    /// A shell string can carry several, and a flag in one of them is not a
    /// flag in another: `git diff --name-only; git log --no-renames` passes
    /// git the flag on a query that does not take `--name-only` at all, and
    /// judging the whole string read that as compliance. Every separator is a
    /// bound in both directions — `;`, `&`, `|` and a newline sequence
    /// commands, and `(`, `)` and a backtick open or close a subshell, whose
    /// contents are a command of their own. Quotes suppress all of them, which
    /// is what keeps `core.pager='a;b'` one command.
    ///
    /// Nothing shipped is multi-command today, so this closes a latent hole
    /// rather than a live one.
    fn shell_command_at(span: &str, at: usize) -> &str {
        const SEPARATORS: [char; 7] = [';', '&', '|', '\n', '(', ')', '`'];
        let mut start = 0usize;
        let mut quote: Option<char> = None;
        let mut chars = span.char_indices();
        while let Some((i, c)) = chars.next() {
            if c == '\\' {
                chars.next();
                continue;
            }
            match quote {
                Some(open) if c == open => quote = None,
                Some(_) => {}
                None if c == '"' || c == '\'' => quote = Some(c),
                None if SEPARATORS.contains(&c) => {
                    if i < at {
                        start = i.saturating_add(c.len_utf8());
                    } else {
                        return span.get(start..i).unwrap_or_default();
                    }
                }
                None => {}
            }
        }
        span.get(start..).unwrap_or_default()
    }

    /// `span` as a shell would split it into WORDS.
    ///
    /// Whitespace divides words only OUTSIDE quotes, and the quotes are then
    /// removed from the word they delimited — which is the whole of why this
    /// is a walk and not a `split_whitespace`. Both directions matter and a
    /// splitter without quote state gets one of them wrong whichever way it
    /// guesses: `core.pager='x --no-renames'` is ONE word, the config value
    /// git is handed, and `diff "--no-renames"` is two, the second of which
    /// really is the flag. Review measured both against the simpler rule.
    ///
    /// Rust's own escaping survives into the span, so `\"` is a quote the
    /// SHELL sees rather than a backslash it does.
    fn shell_words(span: &str) -> Vec<String> {
        let mut words = Vec::new();
        let mut word = String::new();
        let mut quote: Option<char> = None;
        let mut started = false;
        let mut chars = span.chars();
        while let Some(c) = chars.next() {
            let (c, escaped) = match c {
                '\\' => match chars.next() {
                    Some(next) => (next, true),
                    None => break,
                },
                other => (other, false),
            };
            match quote {
                Some(open) if c == open => quote = None,
                Some(_) => word.push(c),
                // A `#` beginning a word opens a bash COMMENT, so nothing
                // after it is passed to anything: `git diff --name-only #
                // --no-renames` names the flag where the shell will never
                // read it, which is the same evasion `passes_no_renames`
                // splits arguments to refuse. Quoted or mid-word it is an
                // ordinary character, which is why this sits here rather
                // than at the top of the loop.
                None if c == '#' && !escaped && !started => break,
                None if c == '"' || c == '\'' => {
                    quote = Some(c);
                    started = true;
                }
                None if c.is_ascii_whitespace() => {
                    if started {
                        words.push(std::mem::take(&mut word));
                        started = false;
                    }
                }
                None => {
                    word.push(c);
                    started = true;
                }
            }
        }
        if started {
            words.push(word);
        }
        words
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
            let whole = strip_comments(&std::fs::read_to_string(root.join(rel)).unwrap());
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
                let body = strip_comments(&std::fs::read_to_string(f).unwrap());
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

    /// Byte ranges of every bracket group, STRING-AWARE: a `[` or `]` inside a
    /// string literal is data, and counting it would desynchronise the depth
    /// and hand back a span belonging to a different argv.
    fn bracket_spans(text: &str) -> Vec<(usize, usize)> {
        let mut spans = Vec::new();
        let mut stack: Vec<usize> = Vec::new();
        let mut in_str = false;
        let mut esc = false;
        for (i, c) in text.char_indices() {
            if in_str {
                match c {
                    _ if esc => esc = false,
                    '\\' => esc = true,
                    '"' => in_str = false,
                    _ => {}
                }
                continue;
            }
            match c {
                '"' => in_str = true,
                '[' => stack.push(i),
                ']' => {
                    if let Some(s) = stack.pop() {
                        spans.push((s, i.saturating_add(1)));
                    }
                }
                _ => {}
            }
        }
        spans
    }

    /// The string literal containing byte `at`, if any — the fallback span for
    /// a COMBINED command string (`run_shell(root, "git diff --name-only")`),
    /// which is in no argv literal at all and would otherwise be invisible.
    fn string_span(text: &str, at: usize) -> Option<(usize, usize)> {
        let mut in_str = false;
        let mut esc = false;
        let mut start = 0usize;
        for (i, c) in text.char_indices() {
            if in_str {
                match c {
                    _ if esc => esc = false,
                    '\\' => esc = true,
                    '"' => {
                        in_str = false;
                        if start <= at && at < i {
                            return Some((start, i));
                        }
                    }
                    _ => {}
                }
                continue;
            }
            if c == '"' {
                in_str = true;
                start = i.saturating_add(1);
            }
        }
        None
    }

    /// The first place in `text` that asks git for `--name-only` without
    /// `--no-renames` alongside it.
    ///
    /// The span is the smallest enclosing bracket group, or failing that the
    /// enclosing string literal. Anything with neither yields an empty span and
    /// so reds: a builder chain of `.arg(...)` calls cannot be judged by reading,
    /// and refusing is the fail-safe direction.
    ///
    /// WHICH of the two it is travels with it, because that is what says how
    /// the span is split into arguments — see `passes_no_renames`. An empty
    /// span is neither and reds whatever it is told.
    ///
    /// The LITERAL holding the needle is what decides, rather than whether a
    /// bracket group encloses it. A string that is EXACTLY `--name-only` is
    /// one element of an argv, and the span to judge is the argv around it; a
    /// string holding more is a command LINE, and the only way a command line
    /// travels as one argument is through a shell. Preferring the bracket
    /// group made `vec!["git diff --no-renames --name-only"]` a false red —
    /// no such site exists today, and a false red here stops the whole gate.
    fn bare_name_only(text: &str) -> Option<String> {
        const NEEDLE: &str = "--name-only";
        let spans = bracket_spans(text);
        let mut from = 0usize;
        while let Some(rel) = text.get(from..).and_then(|t| t.find(NEEDLE)) {
            let at = from.saturating_add(rel);
            let held = string_span(text, at);
            let literal = held.and_then(|(s, e)| text.get(s..e).map(|t| (t, s)));
            let (span, shell) = match literal {
                // A command LINE is a string holding more than the flag AND
                // naming the program that reads it. Without that second half
                // an argv ELEMENT holding two flags — `["diff", "--name-only
                // --no-renames"]` — read as a shell line and passed, where git
                // gets one argument it rejects; review measured it green.
                Some((held, s)) if held != NEEDLE && names_at_boundary(held, "git") => {
                    (held, Some(at.saturating_sub(s)))
                }
                _ => (
                    spans
                        .iter()
                        .copied()
                        .filter(|(s, e)| *s < at && at < *e)
                        .min_by_key(|(s, e)| e.saturating_sub(*s))
                        .and_then(|(s, e)| text.get(s..e))
                        .unwrap_or(""),
                    None,
                ),
            };
            if !passes_no_renames(span, shell) {
                return Some(span.split_whitespace().collect::<Vec<_>>().join(" "));
            }
            from = at.saturating_add(NEEDLE.len());
        }
        None
    }

    /// The span logic itself, on literals. Without this the whole mechanism is
    /// unpinned: `checked` below is counted with `contains`, never through
    /// `bare_name_only`, so a body that simply returned `None` would satisfy
    /// every other assertion in both crates forever.
    #[test]
    fn the_name_only_scan_reads_a_span_rather_than_a_file() {
        let bad = r#"g.run(&["diff", "--name-only"]);"#;
        let good = r#"g.run(&["diff", "--no-renames", "--name-only"]);"#;
        assert!(bare_name_only(bad).is_some(), "a bare argv must red");
        assert_eq!(bare_name_only(good), None, "a flagged argv must pass");
        // PER-ARGV, not per-file: a flagged query does not excuse a bare one
        // sitting beside it.
        assert!(
            bare_name_only(&format!("{good}\n{bad}")).is_some(),
            "a bare argv beside a flagged one must still red"
        );
        // The innermost group wins, so an outer array of argvs cannot lend its
        // flag to an inner one.
        let nested = r#"for a in [["diff", "--no-renames"], ["diff", "--name-only"]] {}"#;
        assert!(bare_name_only(nested).is_some(), "{nested}");
        // A COMBINED command string has no argv; the literal is the span.
        assert!(bare_name_only(r#"run_shell(root, "git diff --name-only")"#).is_some());
        assert_eq!(
            bare_name_only(r#"run_shell(root, "git diff --no-renames --name-only")"#),
            None
        );
        // A `//` inside a string is not a comment: the needle after it must
        // still be seen.
        let url = r#"g.run(&["-c", "url.https://x.insteadOf=y", "diff", "--name-only"]);"#;
        assert!(
            bare_name_only(&strip_comments(url)).is_some(),
            "a string containing // must not hide the query"
        );
        // …and a real comment IS stripped, or this file reds itself.
        assert_eq!(
            bare_name_only(&strip_comments("// mentions --name-only\n")),
            None
        );
        // A bracket inside a string must not desynchronise the span.
        let braced = r#"g.run(&["diff", "--no-renames", "--name-only", "--format=[x"]);"#;
        assert_eq!(bare_name_only(braced), None, "{braced}");
        // A BLOCK comment may not satisfy the rule: `--no-renames` written
        // there never reaches git, so the query beside it is bare.
        let blocked = "g.run(&[\"diff\", \"--name-only\", /* TODO: --no-renames */]);";
        assert!(
            bare_name_only(&strip_comments(blocked)).is_some(),
            "a commented --no-renames must not pass the query beside it"
        );
        // Block comments NEST, so the inner close may not end the outer one.
        let nested_block = "g.run(&[\"diff\", \"--name-only\", /* a /* b */ --no-renames */]);";
        assert!(
            bare_name_only(&strip_comments(nested_block)).is_some(),
            "{nested_block}"
        );
        // A char literal holding a QUOTE is neutralised rather than refused,
        // in BOTH spellings, since `'\"'` is the same literal. `lex` returning
        // one and the caller skipping that file is what review walked out
        // through, so the file is judged now and the literal means `'x'`.
        let read = lex;
        assert_eq!(read("let c = '\"';"), "let c = 'x';");
        assert_eq!(read("let c = '\\\"';"), "let c = 'x';");
        assert_eq!(read("let c = b'\\\"';"), "let c = b'x';");
        // …and the QUERY beside one is judged, which is the whole of the fix.
        // `strip_comments` answered the old escape and loses this query to the
        // `//` in a URL inside a literal spanning lines, so the file was
        // skipped whole. Five `builder/src` files carry that char literal.
        let hidden = "const C: char = '\"';\nrun_shell(root, \"git -c\nurl.https://x.insteadOf=y diff --name-only\");";
        assert!(!strip_comments(hidden).contains("--name-only"));
        assert!(bare_name_only(&read(hidden)).is_some(), "{hidden}");
        // …the same with the literal in an ARGV, and with the char literal
        // after the query rather than before it.
        let after = "const Q: &[&str] = &[\"diff\", \"--name-only\"];\nconst C: char = '\"';";
        assert!(bare_name_only(&read(after)).is_some(), "{after}");
        // A quote inside a COMMENT or a STRING is data `lex` has consumed by
        // then, so neither is rewritten and neither ends a literal.
        assert_eq!(read("let x = 1; // spelled '\"' here"), "let x = 1; ");
        assert_eq!(read("let s = \"a '\\\"' b\";"), "let s = \"a '\\\"' b\";");
        // A LIFETIME is left alone, which is what makes the rewrite safe to
        // do at all: `'` names one, and no lifetime's name starts with `\"`.
        assert_eq!(
            read("fn f<'a>(s: &'a str) -> &'a str { s }"),
            "fn f<'a>(s: &'a str) -> &'a str { s }"
        );
        // Everything else `lex` READS, and each of these was a silent PASS
        // under the quote toggle it replaced. A raw string holding a quote…
        let hashed = "let s = r#\"a \" b\"#; g.run(&[\"diff\", \"--name-only\"]);";
        assert!(bare_name_only(&read(hashed)).is_some(), "{hashed}");
        // …a hashless one ending in a backslash, which closes for rustc and
        // stayed open for the toggle…
        let trailing = "let s = r\"\\\"; g.run(&[\"-c\", \"url.https://x\", \
                        \"diff\", \"--name-only\"]);";
        assert!(bare_name_only(&read(trailing)).is_some(), "{trailing}");
        // …an ordinary literal SPANNING LINES, whose second line the per-line
        // toggle read as code and cut at the `//` in the URL…
        let spans_lines = "run_shell(root, \"git -c\nurl.https://x.insteadOf=y diff --name-only\");";
        assert!(!strip_comments(spans_lines).contains("--name-only"));
        assert!(bare_name_only(&read(spans_lines)).is_some(), "{spans_lines}");
        let compliant =
            "run_shell(root, \"git -c\nurl.https://x diff --no-renames --name-only\");";
        assert_eq!(bare_name_only(&read(compliant)), None);
        // …and a SHEBANG, which is not Rust, may hold an unmatched quote, and
        // carried a string into the file under it. A BOM before it is not Rust
        // either and rustc takes it off FIRST, so the shebang is still one.
        let shebang = "#!/usr/bin/env \"ignored\nfn f() { g.run(&[\"-c\", \
                       \"url.https://x\", \"diff\", \"--name-only\"]); }";
        assert!(bare_name_only(&read(shebang)).is_some(), "{shebang}");
        let bom = format!("\u{feff}{shebang}");
        assert!(bare_name_only(&read(&bom)).is_some(), "a BOM hid the shebang");
        // An inner attribute is not a shebang, and `#![forbid(unsafe_code)]`
        // opens most of this tree's files — nor is `#! [attr]`, which rustc
        // reads past the space.
        assert!(read("#![forbid(unsafe_code)]\nlet x = 1;").contains("forbid"));
        assert!(read("#! [forbid(unsafe_code)]\nlet x = 1;").contains("forbid"));
        // The shebang's NEWLINE survives, so a line number read off this output
        // is the file's own. Review removed the push and watched nothing red.
        assert_eq!(read("#!/bin/sh\nlet x = 1;"), "\nlet x = 1;");
        // Block comments NEST here too. Review deleted the depth arm and every
        // other assertion in this file stayed green: `strip_comments` had this
        // test and `lex` had none, which is how one gets a silent PASS.
        let nested = "g.run(&[\"diff\", \"--name-only\", /* a /* b */ --no-renames */]);";
        assert!(bare_name_only(&read(nested)).is_some(), "{}", read(nested));
        // …and `lex` must ASK `opens_raw` rather than merely have it. An `r`
        // ending an identifier opens nothing, so this text is unchanged.
        assert_eq!(read("let s = foor\"a\\b\";"), "let s = foor\"a\\b\";");
        // A raw string is rewritten as an ORDINARY one, because the span
        // helpers downstream are quote toggles: its content keeps its bytes
        // and gives them back a literal they can lex.
        assert_eq!(read("let s = r\"a\\b\";"), "let s = \"a\\\\b\";");
        assert_eq!(read("let s = r#\"a \" b\"#;"), "let s = \"a \\\" b\";");
        assert_eq!(read("let s = br\"x\";"), "let s = \"x\";");
        assert_eq!(read("let s = cr\"x\";"), "let s = \"x\";");
        // `fr` is not a prefix Rust HAS, and is read as one for the asymmetry
        // named where `opens_raw` takes it: nothing can write that spelling,
        // and reading it as an identifier plus an ordinary string cuts the
        // rest of the line. Review deleted both `'f'` arms and every other
        // assertion here stayed green.
        assert_eq!(read("let s = fr\"x\";"), "let s = \"x\";");
        let effed = "let s = fr\"\\\"; g.run(&[\"diff\", \"--name-only\"]);";
        assert!(bare_name_only(&read(effed)).is_some(), "{}", read(effed));
        // A RAW IDENTIFIER is not a raw string: rustc reads `r#r` as the
        // identifier `r`, so the `\"` below is an ordinary escape and not a
        // raw string's content. Reading it as one desynchronises the rest.
        let raw_ident = "let _ = sink!(r#r\"\\\"\"); g.run(&[\"diff\", \"--name-only\"]);";
        assert_eq!(
            read(raw_ident),
            "let _ = sink!(r#r\"\\\"\"); g.run(&[\"diff\", \"--name-only\"]);"
        );
        assert!(bare_name_only(&read(raw_ident)).is_some(), "{raw_ident}");
        // `r` ending an IDENTIFIER opens nothing, and neither does one inside
        // a string. Two rounds of review wrote the second as `"other"` and as
        // `"-r"` followed by a comment ending in an escaped quote, and watched
        // a heuristic refuse the whole file for it.
        assert_eq!(read("let s = \"-r\"; // spell a quote as \\\""), "let s = \"-r\"; ");
        let prefixed: Vec<char> = "let s = br\"x\";".chars().collect();
        assert!(opens_raw(&prefixed, 9), "`b` is the prefix, not the boundary");
        let ident: Vec<char> = "let s = foor\"x\";".chars().collect();
        assert!(!opens_raw(&ident, 11), "an `r` ending an identifier opens nothing");
        // The flag must be an ARGUMENT, not any occurrence in the span.
        let faked = r#"g.run(&["diff", "--format=--no-renames", "--name-only"]);"#;
        assert!(bare_name_only(faked).is_some(), "a flag named inside another must not pass");
        // …and an ESCAPED quote is not the boundary that would make it one.
        // `lex` writes those, rewriting a raw string's inner quote, so a
        // single argument holding the flag's spelling must not read as two.
        let inner = "g.run(&[\"diff\", r#\"--output=x\"--no-renames\"#, \"--name-only\"]);";
        assert!(
            bare_name_only(&read(inner)).is_some(),
            "an escaped quote is content, not an argument boundary: {}",
            read(inner)
        );
        // A char literal holding a BRACKET is data too, and `bracket_spans`
        // counts brackets — so the span judged would be a different argv.
        let bracketed = "g.run(&[arg(']'), \"--name-only\", arg('[')]);";
        assert_eq!(read(bracketed), "g.run(&[arg('x'), \"--name-only\", arg('x')]);");
        assert!(bare_name_only(&read(bracketed)).is_some(), "{bracketed}");
        // WHITESPACE splits a shell string and not an argv literal. The flag
        // inside one argument is passed to git as part of that argument, so
        // the query beside it is bare — including in the form git ACCEPTS,
        // where the value of `-c` may hold spaces and the run really does
        // detect renames.
        let inline = r#"g.run(&["diff", "ignored --no-renames", "--name-only"]);"#;
        assert!(bare_name_only(inline).is_some(), "{inline}");
        let config = r#"g.run(&["-c", "core.pager=x --no-renames", "diff", "--name-only"]);"#;
        assert!(bare_name_only(config).is_some(), "{config}");
        // …while the shell form, which is what the whitespace split exists
        // for, still passes: there the shell is what splits the string.
        assert_eq!(
            bare_name_only(r#"run_shell(root, "git diff --no-renames --name-only")"#),
            None
        );
        // A SHELL QUOTE is not a word boundary the way whitespace is, and the
        // two directions of that are a pair. `core.pager='--no-renames'` is
        // one word bash hands git as a config VALUE — the quotes vanish and
        // the flag is never passed…
        let quoted = "run_shell(root, \"git -c core.pager='--no-renames' diff --name-only\")";
        assert!(bare_name_only(quoted).is_some(), "{quoted}");
        // …while a quote with WHITESPACE before it delimits a word whose
        // content is exactly the flag, escaped here because Rust's own
        // literal needs it. git gets `--no-renames`, so this must pass.
        let escaped = "run_shell(root, \"git diff \\\"--no-renames\\\" --name-only\")";
        assert_eq!(bare_name_only(escaped), None, "{escaped}");
        // …and a quoted word holding MORE than the flag is that same config
        // value with the quotes moved, which a whitespace split alone reads
        // as the flag. Review wrote it green against the simpler rule.
        let inside = "run_shell(root, \"git -c core.pager='x --no-renames' diff --name-only\")";
        assert!(bare_name_only(inside).is_some(), "{inside}");
        // A command STRING inside a bracket group is judged by the string it
        // is, not by the group around it: the literal holding the needle is
        // the whole command, so the shell splits it. This was a false red.
        let vectored = r#"let cmds = vec!["git diff --no-renames --name-only"];"#;
        assert_eq!(bare_name_only(vectored), None, "{vectored}");
        // …while a literal that IS the flag is one element of an argv, so the
        // argv around it is the span even though both sit in brackets.
        let element = r#"let cmds = vec![["diff", "ignored --no-renames", "--name-only"]];"#;
        assert!(bare_name_only(element).is_some(), "{element}");
        // A char literal is recognised WHOLE, so its closing quote is never
        // read as an opening one. Matching fixed SPELLINGS instead deleted a
        // bracket here, which can close an argv span early and let a bare
        // query inherit a neighbour's flag.
        assert_eq!(lex("let c = 'a'['b'];"), "let c = 'x'['x'];");
        assert_eq!(lex("let q = '\\'';"), "let q = 'x';");
        assert_eq!(lex("let n = '\\u{7d}';"), "let n = 'x';");
        // …while a lifetime is not a literal, whatever follows it: the two
        // spellings share a sigil and only lookahead tells them apart.
        let borrows = "fn f<'a>(x: &'a str) -> &'a str { x }";
        assert_eq!(lex(borrows), borrows);
        // rustc skips COMMENTS as well as whitespace before deciding a `#!`
        // opened an inner attribute, so one that spans lines is not a shebang
        // and its content is not code. Cutting only its first line left the
        // rest to be read as code, quote and all.
        assert_eq!(lex("#!/*\n\"\n*/[allow(x)]\nq();"), "#!\n\n[allow(x)]\nq();");
        // A flag in ANOTHER command of the same string is not this query's:
        // both of these pass git a flag on a query that is not the one asking
        // for `--name-only`, and judging the whole string read that as
        // compliance.
        let sequenced = "run_shell(root, \"git diff --name-only; git log --no-renames\")";
        assert!(bare_name_only(sequenced).is_some(), "{sequenced}");
        let substituted = "run_shell(root, \"git diff --name-only $(true --no-renames )\")";
        assert!(bare_name_only(substituted).is_some(), "{substituted}");
        // …while a separator inside quotes divides nothing.
        let quoted_semi = "run_shell(root, \"git -c core.pager='a;b' diff --no-renames --name-only\")";
        assert_eq!(bare_name_only(quoted_semi), None, "{quoted_semi}");
        // An argv ELEMENT holding two flags is not a command line: git gets
        // one argument it rejects. A literal is judged as shell only if it
        // NAMES the program that would split it.
        let two_in_one = r#"g.run(&["diff", "--name-only --no-renames"]);"#;
        assert!(bare_name_only(two_in_one).is_some(), "{two_in_one}");
        // `--` ends git's options, so what follows names a file rather than
        // asking for anything.
        let after_ddash = r#"g.run(&["diff", "--name-only", "--", "--no-renames"]);"#;
        assert!(bare_name_only(after_ddash).is_some(), "{after_ddash}");
        // …and a literal inside a NESTED expression is not an argument: this
        // one builds one config value git accepts, and the flag never reaches
        // it as an option.
        let nested = r#"g.run(&["-c", concat!("core.pager=x ", "--no-renames"), "diff", "--name-only"]);"#;
        assert!(bare_name_only(nested).is_some(), "{nested}");
        // A `#` beginning a word opens a bash comment, so the flag after one
        // is never passed. This was a silent pass.
        let hidden_flag = "run_shell(root, \"git diff --name-only # --no-renames\")";
        assert!(bare_name_only(hidden_flag).is_some(), "{hidden_flag}");
        // …and mid-word it is an ordinary character, so the flag beyond it
        // still counts. Reading every `#` as a comment would false-red here.
        let hashed = "run_shell(root, \"git diff --pretty=a#b --no-renames --name-only\")";
        assert_eq!(bare_name_only(hashed), None, "{hashed}");
        // The SHIPPED half is told from the test half by a marker that must
        // occur once, and the module is CUT OUT rather than truncated at:
        // `td-sh.rs` really does declare an item below its tests, and
        // truncating left that item unjudged with the `checked > 0` control
        // still satisfied by the other files.
        let owned = |s: &str| Some(s.to_string());
        assert_eq!(shipped_half("let x = 1;"), owned("let x = 1;"));
        let split = format!("shipped();\n{TEST_MOD} {{ let s = \"}}\"; }}");
        assert_eq!(shipped_half(&split), owned("shipped();\n"));
        assert_eq!(shipped_half(&format!("{split}\nbelow();")), owned("shipped();\n\nbelow();"));
        assert_eq!(shipped_half(&format!("{split}\n{TEST_MOD} {{}}")), None);
        // A marker inside a STRING is data: `lex` has already taken the
        // comments, so blanking the strings leaves nothing that can spell one
        // without being code. Both of these used to cut the file short.
        let quoted = format!("let s = \"{TEST_MOD} {{}}\";\nshipped();");
        assert_eq!(shipped_half(&quoted), owned(&quoted));
        let commented = lex(&format!("/* {TEST_MOD} {{}} */\nshipped();"));
        // Two newlines: the comment's own survives, as it must for a line
        // number read off this output to be the file's.
        assert_eq!(shipped_half(&commented), owned("\n\nshipped();"));
        // …and an unterminated module refuses rather than cutting to the end.
        assert_eq!(shipped_half(&format!("shipped();\n{TEST_MOD} {{")), None);
        assert_eq!(shipped_half(&format!("shipped();\n{TEST_MOD};")), None);
    }

    /// `--name-only` reports only the DESTINATION of a detected rename, so any
    /// decision made from its output silently stops seeing the source half of
    /// a `git mv`. That produced two separate defects — a check selection that
    /// missed the deletion a rename left behind, and a `docs-only` waiver that
    /// let a commit deleting Rust source waive all three reviews — and FIVE
    /// call sites had to be found by reading, because nothing said the rule.
    ///
    /// This is the rule. Every `--name-only` in shipped code carries
    /// `--no-renames` in the same span; a site that genuinely wants rename
    /// detection reds here and says so in its landing.
    ///
    /// Shipped half only, and split at the TEST MODULE rather than at the
    /// first `#[cfg(test)]`: that attribute also gates single items —
    /// `builder/src/main.rs` has one on a `mod` DECLARATION at line 30 — and
    /// splitting there discarded the other 13 000 lines of that file while the
    /// count below stayed satisfied by other files.
    #[test]
    fn no_shipped_name_only_query_lets_git_detect_renames() {
        let mut sources = Vec::new();
        collect_rs_files(&repo_root().join("builder/src"), &mut sources);
        assert!(!sources.is_empty(), "no sources to scan");
        let mut checked = 0usize;
        for f in &sources {
            let raw = std::fs::read_to_string(f).unwrap();
            // ONE pass, and one that always answers: `lex` reads every file,
            // so there is no branch here that declines to judge one. The
            // branch there used to be is what review walked out through — a
            // refusal answered by `strip_comments`, which cannot see a query
            // its per-line toggle has lost.
            //
            // Lex BEFORE splitting, which is the order the split needs rather
            // than a tidying: `shipped_half` reads the marker and the braces
            // as CODE, and only after this pass is a `"…"` in the file
            // guaranteed to be a string and a `//` guaranteed to be gone.
            let lexed = lex(&raw);
            let Some(shipped) = shipped_half(&lexed) else {
                panic!(
                    "{f:?} does not carry exactly one whole test module, so \
                     the shipped half cannot be told from the test half"
                )
            };
            if !shipped.contains("--name-only") {
                continue;
            }
            checked = checked.saturating_add(shipped.matches("--name-only").count());
            assert_eq!(
                bare_name_only(&shipped),
                None,
                "{f:?} asks git for --name-only without --no-renames"
            );
        }
        // POSITIVE CONTROL over ARGVS, not files: a scan that reached no query
        // would pass this crate however its git commands were written.
        //
        // Pinned to the NUMBER rather than to "more than none", which is what
        // four of this commit's own arguments turn on: every silent skip
        // above was invisible because the other files kept `checked > 0`
        // satisfied. A count says which, and the cost of pinning it is one
        // line whenever a query is added or removed — the same cost every
        // other pinned count in this tree has, and the same reason.
        assert_eq!(
            checked, 5,
            "{checked} shipped --name-only queries were read, not 5 — a query \
             that stopped being read looks exactly like one that was deleted"
        );
        // …and main.rs specifically, which the old split reduced to 30 lines.
        let main = strip_comments(
            &std::fs::read_to_string(repo_root().join("builder/src/main.rs")).unwrap(),
        );
        let shipped = match main.find(TEST_MOD) {
            Some(at) => main.get(..at).unwrap_or_default(),
            None => main.as_str(),
        };
        assert!(
            shipped.lines().count() > 1000,
            "main.rs scanned as {} lines — the split is truncating it again",
            shipped.lines().count()
        );
    }

    /// Every roster entry must HAVE commands in the table. Without this the
    /// filter yields an empty list for it, and a preflight that loops over
    /// nothing exits 0 having run no cargo at all.
    #[test]
    fn unembedded_crates_have_cargo_commands() {
        let cmds = gate_cmds();
        for krate in UNEMBEDDED_CRATES {
            let mine: Vec<&str> = cmds
                .iter()
                .map(String::as_str)
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
        for cmd in gate_cmds() {
            let cmd = cmd.as_str();
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
        let root = repo_root();
        let all = gate_cmds().len();
        let one = |p: &str| cargo_test_cmds(&root, &[p.to_string()]).expect("narrowing");
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
        assert_eq!(cargo_test_cmds(&root, &[]).expect("narrowing").len(), all);
        // A prefix is not a directory: `td-reviewer/` is a different crate.
        assert_eq!(one("td-reviewer/src/main.rs").len(), all);
        // Nor may a `..` that starts with the crate and names another.
        assert_eq!(one("td-review/../td-sh/src/main.rs").len(), all);
        // One narrowable path does not license the OTHERS in the same diff.
        assert_eq!(
            cargo_test_cmds(
                &root,
                &[
                    "td-review/src/land.rs".to_string(),
                    "builder/src/gates.rs".to_string(),
                ]
            )
            .expect("narrowing")
            .len(),
            all,
            "a mixed diff must take the whole table"
        );
    }

    /// The rendered line and the executed list come from one call, so the dry
    /// run cannot advertise a command the run will not issue.
    #[test]
    fn the_printed_cargo_line_matches_what_would_run() {
        let root = repo_root();
        let changed = vec!["td-review/src/land.rs".to_string()];
        let line = preflight_cmd(&root, "cargo-test", &changed).unwrap_or_default();
        let ran = cargo_test_cmds(&root, &changed).expect("narrowing");
        let running: Vec<&str> = ran.iter().filter_map(|c| cmd_manifest_crate(c)).collect();
        assert!(!running.is_empty(), "nothing would run: {line:?}");
        // Both directions, over EVERY crate the full table names. The "names
        // each one that runs" half is vacuous on its own: a render that printed
        // all thirteen manifests would satisfy it, and printing a command the
        // preflight will not run is exactly the lie this test exists to catch.
        for cmd in gate_cmds() {
            let Some(krate) = cmd_manifest_crate(&cmd) else {
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
        let full = preflight_cmd(&root, "cargo-test", &["builder/src/main.rs".to_string()])
            .unwrap_or_default();
        assert!(full.starts_with(
            "  cargo test + clippy --frozen --workspace (builder/recipes/engine) + --manifest-path "
        ));
        // Every gated crate by name rather than one pinned spelling of the
        // whole line: the roster is derived now, so an expectation that listed
        // it would be the central table this change removed, in a test's
        // clothes. The declared `test-args` are checked because they are the
        // one part a crate can change without changing the set.
        for krate in gate_roster() {
            let manifest = format!("--manifest-path {}/Cargo.toml", krate.name);
            assert!(full.contains(&manifest), "{full:?} omits {}", krate.name);
        }
        assert!(
            full.contains("--manifest-path td-review/Cargo.toml -- --include-ignored"),
            "{full:?} lost td-review's declared test args"
        );
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

    /// The declaration parser, over manifest TEXT so its cases are literals
    /// rather than a fixture tree.
    #[test]
    fn a_crate_declares_how_it_is_gated_and_a_typo_reds() {
        // Absent block: the defaults, which is what most crates want and what
        // makes adding a crate a zero-central-edit act.
        let bare = "[package]\nname = \"td-x\"\n\n[workspace]\n";
        let got = parse_gate_crate("td-x", bare).expect("defaults");
        assert!(!got.clippy_all_targets);
        assert_eq!(got.test_args, None);

        // Both keys, with the trailing comments these manifests write.
        let full = "[package.metadata.td-gate]\n\
                    clippy-all-targets = true # lints test targets too\n\
                    test-args = \"--include-ignored\" # its only tier\n";
        let got = parse_gate_crate("td-x", full).expect("declared");
        assert!(got.clippy_all_targets);
        assert_eq!(got.test_args.as_deref(), Some("--include-ignored"));

        // Every spelling cargo accepts for the SAME table is the same table
        // here. Each of these was a silent default before review found it.
        for ok in [
            "[package.metadata.td-gate] # how this crate is gated\nclippy-all-targets = true\n",
            "[ package.metadata.td-gate ]\nclippy-all-targets = true\n",
            "[package.metadata.\"td-gate\"]\nclippy-all-targets = true\n",
        ] {
            assert!(
                parse_gate_crate("td-x", ok).expect("accepted spelling").clippy_all_targets,
                "{ok:?} declares the flag and must be read"
            );
        }
        // …and every NEAR MISS reds rather than declaring nothing, including the
        // dotted and inline-table spellings this parser does not read.
        for miss in [
            "[package.metadata.td-gat]\nclippy-all-targets = true\n",
            "[package.metadata.td-gate2]\nclippy-all-targets = true\n",
            "[package.metadata.td_gate]\nclippy-all-targets = true\n",
            "[package.metadata.TD-GATE]\nclippy-all-targets = true\n",
            "[package.metadata]\ntd-gate.clippy-all-targets = true\n",
            "[package.metadata]\ntd-gate = { clippy-all-targets = true }\n",
        ] {
            assert!(parse_gate_crate("td-x", miss).is_err(), "{miss:?} must red");
        }
        // The dotted and inline-table spellings are folded the same way the
        // header is — case, `_`, and quoting anywhere in the key.
        for miss in [
            "[package.metadata]\ntd_gate = { clippy-all-targets = true }\n",
            "[package.metadata]\ntd_gate.clippy-all-targets = true\n",
            "[package.metadata]\nTD-GATE = { clippy-all-targets = true }\n",
            "[package.metadata]\n\"td-gate\".clippy-all-targets = true\n",
            // TOML ignores whitespace around a dotted key's parts, and the
            // header path folds it, so this path must too.
            "[package.metadata]\ntd-gate . clippy-all-targets = true\n",
            "[[package.metadata.td-gate]]\nclippy-all-targets = true\n",
        ] {
            assert!(parse_gate_crate("td-x", miss).is_err(), "{miss:?} must red");
        }

        // An unrelated table under `[package.metadata]` is still nobody's
        // business but its owner's.
        let docs = "[package.metadata.docs.rs]\nall-features = true\n";
        assert!(parse_gate_crate("td-x", docs).is_ok(), "{docs:?}");

        // A declared value reaches a `bash -c` string, so a shell metacharacter
        // is refused rather than quoted.
        for evil in [
            "[package.metadata.td-gate]\ntest-args = \"--x; rm -rf /\"\n",
            "[package.metadata.td-gate]\ntest-args = \"--x $(id)\"\n",
            "[package.metadata.td-gate]\ntest-args = \"--x `id`\"\n",
            "[package.metadata.td-gate]\ntest-args = \"--x && id\"\n",
        ] {
            assert!(parse_gate_crate("td-x", evil).is_err(), "{evil:?} must red");
        }

        // A trailing comment on the HEADER does not hide the block…
        let commented = "[package.metadata.td-gate] # how this crate is gated\n\
                         clippy-all-targets = true\n";
        assert!(parse_gate_crate("td-x", commented).expect("commented").clippy_all_targets);
        // …a bool tolerates the comment a string already did, space or not…
        let tight = "[package.metadata.td-gate]\nclippy-all-targets = true#no space\n";
        assert!(parse_gate_crate("td-x", tight).expect("tight").clippy_all_targets);
        // …and a NEAR-MISS header reds rather than declaring nothing, which is
        // the one way the per-key strictness below could be talked out of.
        for miss in [
            "[package.metadata.td-gat]\nclippy-all-targets = true\n",
            "[package.metadata.td-gate2]\nclippy-all-targets = true\n",
        ] {
            assert!(parse_gate_crate("td-x", miss).is_err(), "{miss:?} must red");
        }

        // A later section ENDS the block, so a key below it is not ours…
        let after = "[package.metadata.td-gate]\nclippy-all-targets = true\n\
                     [profile.release]\nclippy-all-targets = false\n";
        assert!(parse_gate_crate("td-x", after).expect("scoped").clippy_all_targets);
        // …and neither is a different metadata table.
        let other = "[package.metadata.docs.rs]\nclippy-all-targets = true\n";
        assert!(!parse_gate_crate("td-x", other).expect("other").clippy_all_targets);

        // Every malformed shape REDS rather than reading as a default. A typo
        // that parsed as "no flag" would silently drop lint coverage, which is
        // the failure the whole roster exists to stop.
        for bad in [
            "[package.metadata.td-gate]\nclipy-all-targets = true\n",
            "[package.metadata.td-gate]\nclippy-all-targets = yes\n",
            "[package.metadata.td-gate]\nclippy-all-targets\n",
            "[package.metadata.td-gate]\ntest-args = --include-ignored\n",
            "[package.metadata.td-gate]\ntest-args = \"\"\n",
            "[package.metadata.td-gate]\ntest-args = \"unterminated\n",
        ] {
            assert!(parse_gate_crate("td-x", bad).is_err(), "{bad:?} must red");
        }
    }

    /// Discovery is over the TREE, so a crate joins the gate by existing — and
    /// a `td-*` directory that is not a crate does not join it by name alone.
    #[test]
    fn a_new_crate_joins_the_roster_by_existing() {
        let root = std::env::temp_dir().join(format!("td-gate-discover-{}", std::process::id()));
        std::fs::remove_dir_all(&root).ok();
        std::fs::create_dir_all(root.join("td-new")).unwrap();
        std::fs::write(root.join("td-new/Cargo.toml"), "[package]\nname = \"td-new\"\n").unwrap();
        std::fs::create_dir_all(root.join("td-loud")).unwrap();
        std::fs::write(
            root.join("td-loud/Cargo.toml"),
            "[package]\n[package.metadata.td-gate]\nclippy-all-targets = true\n",
        )
        .unwrap();
        // A `td-*` directory that is NOT a crate, and a crate that is not `td-*`
        // — `net` is the external-dependency tier and must never be gated here.
        std::fs::create_dir_all(root.join("td-notacrate")).unwrap();
        std::fs::create_dir_all(root.join("net")).unwrap();
        std::fs::write(root.join("net/Cargo.toml"), "[package]\nname = \"td-net\"\n").unwrap();

        let got = discover_gate_crates(&root);
        std::fs::remove_dir_all(&root).ok();
        let got = got.expect("discovery");
        let names: Vec<&str> = got.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["td-loud", "td-new"], "alphabetical, crates only");
        assert!(got.iter().any(|c| c.name == "td-loud" && c.clippy_all_targets));
        assert!(got.iter().any(|c| c.name == "td-new" && !c.clippy_all_targets));
    }

    /// A crate the roster discovered, that no arm above maps, still reaches the
    /// preflight that COMPILES it.
    ///
    /// Deriving the roster is only half of "a crate joins the gate by existing";
    /// the other half is being selected. A branch touching only a new crate used
    /// to take the catch-all, which runs the behavioural tier and NOT the cargo
    /// preflight — so the new crate's lock guard, tests and clippy were listed
    /// in the roster and never executed. Found in review.
    #[test]
    fn a_new_crate_selects_the_preflight_that_compiles_it() {
        let root = std::env::temp_dir().join(format!("td-gate-select-{}", std::process::id()));
        std::fs::remove_dir_all(&root).ok();
        std::fs::create_dir_all(root.join("td-fresh/src")).unwrap();
        std::fs::write(
            root.join("td-fresh/Cargo.toml"),
            "[package]\nname = \"td-fresh\"\n",
        )
        .unwrap();

        let roster = discover_gate_crates(&root);
        let mut mine = Selection::default();
        map_path(&root, &roster, "td-fresh/src/main.rs", &mut mine);
        // A path in NO discovered crate must keep the catch-all exactly, or this
        // arm has become a second catch-all that runs cargo for anything.
        let mut other = Selection::default();
        map_path(&root, &roster, "who/knows.rs", &mut other);
        // …and a crate whose name is only a PREFIX of the discovered one is a
        // different crate.
        let mut near = Selection::default();
        map_path(&root, &roster, "td-fresher/src/main.rs", &mut near);
        std::fs::remove_dir_all(&root).ok();

        assert!(
            mine.preflights.iter().any(|x| x == "cargo-test"),
            "a new crate must reach its own preflight: {:?}",
            mine.preflights
        );
        assert!(
            mine.targets.iter().any(|x| x == "check"),
            "and keep the catch-all's target: {:?}",
            mine.targets
        );
        assert!(
            !other.preflights.iter().any(|x| x == "cargo-test"),
            "an unmapped non-crate path must not: {:?}",
            other.preflights
        );
        assert!(
            !near.preflights.iter().any(|x| x == "cargo-test"),
            "td-fresher is not td-fresh: {:?}",
            near.preflights
        );
    }

    /// A `td-`-prefixed thing that is NOT a crate leaves the roster alone.
    ///
    /// Discovery judges a name only after it knows the entry is a directory
    /// holding a manifest. Asking in the other order made a stray `td-notes.txt`
    /// — or a `td-sh.orig` backup — an error that reds every check on the
    /// branch, which is a new failure mode rather than a guard. Found in the
    /// confirmation pass.
    #[test]
    fn a_stray_td_entry_does_not_red_the_roster() {
        let root = std::env::temp_dir().join(format!("td-gate-stray-{}", std::process::id()));
        std::fs::remove_dir_all(&root).ok();
        std::fs::create_dir_all(root.join("td-real")).unwrap();
        std::fs::write(root.join("td-real/Cargo.toml"), "[package]\n").unwrap();
        // A scratch FILE, and a backup DIRECTORY with no manifest in it. Both
        // carry a `.` that a crate directory may not, and neither is a crate.
        std::fs::write(root.join("td-notes.txt"), "scratch\n").unwrap();
        std::fs::create_dir_all(root.join("td-sh.orig")).unwrap();

        let got = discover_gate_crates(&root);
        std::fs::remove_dir_all(&root).ok();
        let got = got.expect("a stray td- entry must not red the roster");
        let names: Vec<&str> = got.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["td-real"]);
    }

    /// …but a crate-SHAPED directory under a name no command can spell is
    /// refused rather than gated: the alternative is running cargo against a
    /// backup copy of a crate.
    #[test]
    fn a_crate_under_an_unusable_name_is_refused() {
        let root = std::env::temp_dir().join(format!("td-gate-badname-{}", std::process::id()));
        std::fs::remove_dir_all(&root).ok();
        std::fs::create_dir_all(root.join("td-real")).unwrap();
        std::fs::write(root.join("td-real/Cargo.toml"), "[package]\n").unwrap();
        std::fs::create_dir_all(root.join("td-sh.orig")).unwrap();
        std::fs::write(root.join("td-sh.orig/Cargo.toml"), "[package]\n").unwrap();

        let got = discover_gate_crates(&root);
        std::fs::remove_dir_all(&root).ok();
        assert!(got.is_err(), "a crate-shaped `td-sh.orig` must red: {got:?}");
    }

    /// An EMPTY discovery is refused rather than returned: the preflight's loop
    /// over nothing exits 0 having compiled nothing at all.
    #[test]
    fn an_empty_gate_roster_is_an_error() {
        let root = std::env::temp_dir().join(format!("td-gate-empty-{}", std::process::id()));
        std::fs::remove_dir_all(&root).ok();
        std::fs::create_dir_all(&root).unwrap();
        let got = discover_gate_crates(&root);
        std::fs::remove_dir_all(&root).ok();
        assert!(got.is_err(), "an empty roster must red: {got:?}");
    }

    /// The workspace lock's expected package count follows the members list
    /// rather than being a number written twice, and it must agree with the
    /// lock actually committed.
    #[test]
    fn the_workspace_lock_count_follows_the_members_list() {
        let root = repo_root();
        let members = workspace_member_count(&root).expect("members");
        let lock = std::fs::read_to_string(root.join("Cargo.lock")).expect("Cargo.lock");
        let packages = lock.lines().filter(|l| l.trim() == "[[package]]").count();
        assert_eq!(
            members, packages,
            "the members list and the committed workspace lock disagree"
        );

        let bad = std::env::temp_dir().join(format!("td-gate-members-{}", std::process::id()));
        std::fs::remove_dir_all(&bad).ok();
        std::fs::create_dir_all(&bad).unwrap();
        std::fs::write(bad.join("Cargo.toml"), "[workspace]\nresolver = \"2\"\n").unwrap();
        let got = workspace_member_count(&bad);
        std::fs::remove_dir_all(&bad).ok();
        assert!(got.is_err(), "a manifest with no members must red: {got:?}");
    }

    /// Every `td-*` crate in the tree reaches BOTH rosters.
    ///
    /// Both are derived from one scan, so this cannot fail today — which is the
    /// point: it is what replaces the AGENTS.md instruction to remember two
    /// tables, and it reds if a future filter is added to one roster and not
    /// the other. `td-install` landed guarded but uncompiled once already.
    #[test]
    fn every_td_crate_in_the_tree_is_gated() {
        let root = repo_root();
        let locks = gate_locks();
        let cmds = gate_cmds();
        let mut seen = 0usize;
        for entry in std::fs::read_dir(&root).expect("repo root") {
            let name = entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .into_owned();
            if !name.starts_with("td-") || !root.join(&name).join("Cargo.toml").is_file() {
                continue;
            }
            seen = seen.saturating_add(1);
            let lock = format!("{name}/Cargo.lock");
            assert!(
                locks.iter().any(|(l, _)| *l == lock),
                "{name} is a crate in the tree but {lock} is not guarded"
            );
            let manifest = format!("--manifest-path {name}/Cargo.toml");
            for driver in ["cargo test", "cargo clippy"] {
                assert!(
                    cmds.iter()
                        .any(|c| c.starts_with(driver) && c.contains(&manifest)),
                    "{name} has no `{driver}` command — its lints and tests never run"
                );
            }
        }
        // A positive control: a scan that found nothing would pass every
        // assertion above without checking a single crate.
        assert!(seen > 1, "the tree scan found {seen} td-* crates");
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
        for (lock, packages) in gate_locks() {
            assert!(
                assert_dependency_free(&root, &lock, packages).is_ok(),
                "the committed {lock} must pass its own guard"
            );
        }
        assert!(
            assert_dependency_free(&root, "td-review/nope.lock", 1).is_err(),
            "an unreadable lock reds rather than passing"
        );
    }

    /// The roster and the command list are one derived set, and this holds the
    /// two halves to each other. It cannot fail while both come from a single
    /// scan — which is the point: it is what reds if a later filter is added to
    /// one and not the other. It caught a real drift when the two WERE
    /// hand-written copies, and gate 325's own hand-written list still is one.
    #[test]
    fn every_crate_the_preflight_tests_has_its_lock_guarded() {
        let locks = gate_locks();
        for cmd in gate_cmds() {
            let Some(rest) = cmd.split("--manifest-path ").nth(1) else {
                // `--workspace`: the root lock, which the roster carries.
                assert!(
                    locks.iter().any(|(l, _)| l == "Cargo.lock"),
                    "the workspace lock must be guarded"
                );
                continue;
            };
            let Some(krate) = rest.split('/').next() else { continue };
            let lock = format!("{krate}/Cargo.lock");
            assert!(
                locks.iter().any(|(l, _)| *l == lock),
                "{cmd}: {lock} is not in the lock roster"
            );
        }
    }

    /// The REVERSE of the above, and the direction that was missing.
    ///
    /// The check above runs commands → locks, so a crate could be added to the
    /// lock roster with no command and stay silent: its lock is asserted
    /// dependency-free while nothing ever compiles it, which is a crate that
    /// only APPEARS to be checked. `td-install` landed exactly that way — the
    /// gate-file lines were correct and inert, because the in-loop `cargo-test`
    /// gate is unprovisioned and this preflight is what actually runs.
    ///
    /// Both halves are required per crate: `cargo test` alone leaves the
    /// AGENTS.md deny-lints unenforced, since they only fire under the clippy
    /// driver.
    #[test]
    fn every_guarded_lock_has_a_preflight_that_compiles_it() {
        let cmds = gate_cmds();
        for (lock, _) in gate_locks() {
            let krate = match lock.strip_suffix("/Cargo.lock") {
                // The workspace root, which `--workspace` covers.
                None => continue,
                Some(krate) => krate,
            };
            let manifest = format!("--manifest-path {krate}/Cargo.toml");
            for driver in ["cargo test", "cargo clippy"] {
                assert!(
                    cmds
                        .iter()
                        .any(|cmd| cmd.starts_with(driver) && cmd.contains(&manifest)),
                    "{krate} is in the lock roster but no `{driver}` in the \
                     command list compiles it — its lints and tests never run \
                     (AGENTS.md, 'Rust code')"
                );
            }
        }
    }

    /// `TARGET_STATIC_RECIPES` is complete, checked the direction that can go
    /// silent.
    ///
    /// The scan it drives runs roster → recipe, so a crate whose recipe stages
    /// Rust sources while the crate is absent from the roster is scanned by
    /// nothing: its `#[path]` includes and its recipe's file list are free to
    /// disagree again, which is the whole failure the scan exists for. So the
    /// recipes are asked rather than the roster believed.
    ///
    /// The question asked of each recipe is whether it STAGES a `.rs` file —
    /// a `"{src}/….rs"` destination — deliberately not whether it names an
    /// `include_str!`. The include is a compile-time read of this repository
    /// and can be spelled over several lines, through `concat!`, or by a
    /// helper; the staged destination is the thing the scan goes on to look
    /// for, so keying on it is what makes the two agree by construction.
    #[test]
    fn every_recipe_that_stages_rust_sources_is_in_the_roster() {
        let dir = repo_root().join("recipes/src/recipes");
        let mut recipes = Vec::new();
        collect_rs_recursive(&dir, &mut recipes);
        assert!(!recipes.is_empty(), "no recipes to scan");
        let mut checked = 0usize;
        for path in &recipes {
            let text = strip_line_comments(&std::fs::read_to_string(path).unwrap());
            let stages_rust = text.lines().any(|line| {
                line.split("\"{src}/")
                    .skip(1)
                    .any(|rest| rest.split(DQUOTE as char).next().is_some_and(|p| p.ends_with(".rs")))
            });
            if !stages_rust {
                continue;
            }
            let rel = path.strip_prefix(repo_root()).unwrap().to_string_lossy().replace('\\', "/");
            assert!(
                TARGET_STATIC_RECIPES.iter().any(|(_, r)| *r == rel),
                "{rel} stages Rust sources but is not in TARGET_STATIC_RECIPES, \
                 so nothing checks its file list against the crate's #[path] includes"
            );
            checked = checked.saturating_add(1);
        }
        // POSITIVE CONTROL: a scan that matched no recipe would pass whatever
        // the roster said.
        assert_eq!(
            checked,
            TARGET_STATIC_RECIPES.len(),
            "the roster and the recipes that stage Rust sources are different sizes"
        );
    }

    // DURABLE renderer guard (replaces the now-deleted shell differential — the
    // shell oracle was the removable migration leg, retired with the cutover,
    // directive 4). Asserts the FULL `--path` render byte-for-byte for paths whose
    // mapping is INDEPENDENT of repo files.
    //
    // It no longer runs with NO repo tree: one line of each expectation is the
    // cargo-test render, which is derived from the sibling `td-*` crates rather
    // than read from a constant. In the builder-only package sandbox it SKIPS,
    // the way `unembedded_crates_are_really_unembedded` does. Pinning the crate
    // list here instead would rebuild the central table this mechanism removed.
    // The dynamic mappings stay covered by `self_test_passes_against_repo`.
    #[test]
    fn renders_exact_output_for_static_paths() {
        let root = repo_root();
        // The unnarrowed cargo line, derived for the same reason as above: the
        // rest of each expectation below stays pinned byte-for-byte, which is
        // what these tests are for.
        let Ok(cmds) = cargo_test_cmds_all(&root) else {
            eprintln!("SKIP: no sibling td-* crates (builder-only sandbox)");
            return;
        };
        let full_cargo = render_cargo_test(&cmds);
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
                &full_cargo,
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
                &full_cargo,
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

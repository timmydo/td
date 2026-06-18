# rust-build — td-builder owns Rust-from-source builds

Handle: claude-fable-a00773 · claimed 2026-06-17 · section: side

## Why (human-directed, 2026-06-17)

The "Rust replacements" only matter on the no-gnu-build-system axis if td builds
them ITSELF. Today td-builder is an autotools phase runner only
(`builder/src/build.rs`); Rust apps (incl. #80's procs/fd/…) come via Guix's
`cargo-build-system`, which still rides the C toolchain. This track gives
td-builder a `buildSystem:"rust"` cargo phase runner — the build LOGIC becomes
td's Rust code, not Guix's cargo-build-system Guile; the rustc/gcc seed stays
external (§5, retired last). See [[td-youki-postponed]], [[td-shipped-package-rebaseline]].

## Existing scaffolding to reuse
- `system/td-build.scm`: `td-rust-build-derivation` / `write-td-build-spec`
  lower a recipe to a drv whose BUILDER is td-builder; env `TD_INPUTS`,
  `TD_CONFIGURE_FLAGS`, `TD_PHASES`. `%td-build-tool-names` = the gnu toolchain
  (no rustc/cargo yet). ("rust" in those names = td-builder-the-Rust-program.)
- `build.rs`: phase runner dispatched per recipe; currently gnu/autotools only.
- `(system td-builder)`: `%builder-source = (local-file "../builder" …)` (target/
  .cargo excluded) — the in-tree builder tree as a store source. Self-host reuses it.
- rust 1.93.0 in channel, outputs `(rust-src tools out cargo)` (rustc=out, cargo
  output), + gcc-toolchain — all warm/substitutable.

## Increments (one PR if feasible; land 1[+2] if uutils balloons)
1. **Rust phase runner + self-host.** `buildSystem:"rust"` → build.rs cargo runner
   (set-paths → `cargo build --release --offline --frozen` → install $out/bin);
   td-build.scm supplies rust+cargo+gcc-toolchain via TD_INPUTS for rust recipes.
   Target: td-builder builds td-builder (zero-dep std-only → no vendoring). Gate:
   [STRUCTURAL] built with guix/Guile off PATH; [DURABLE behavioral] binary runs
   --version; [DURABLE repro] td-builder check double-build agrees; [MIGRATION
   ORACLE, removable] vs the guix cargo-build-system td-builder (path may diverge —
   cargo determinism differs; report honestly, not a hard gate).
2. **Vendored deps.** A crate WITH deps; `cargo vendor` output pinned as
   fixed-output; `cargo build --offline` against it. Proves the dep path.
3. **uutils tool.** A single uutils utility from source via td's builder — the real
   coreutils-replacement demo. May exceed one PR (crate graph); document if so.

## Cargo reproducibility notes (the risk)
- Determinism: SOURCE_DATE_EPOCH, `--frozen --offline`, stable target dir, strip,
  `--remap-path-prefix` to kill absolute build paths. td-builder `check`
  double-build is the durable repro oracle (not guix --check).
- Byte-identity vs guix cargo-build-system is NOT required (own-then-diverge):
  keep it as a labeled migration-oracle leg only.

## Sub-task ladder
1. [x] claim + plan-index → draft PR #81
2. [x] cargo phase runner `run_rust` in build.rs (+ `rust-build` dispatch in main.rs)
3. [x] `td-rust-selfhost-derivation` in td-build.scm (rust+cargo+gcc seed inputs);
       `%builder-source` exported from td-builder.scm
4. [~] self-host gate (330-rust-build.mk) + tests/rust-build-drv.scm — gate run in progress
5. [ ] verified-red on the gate
6. [ ] vendored-deps increment (Inc.2) — assess after Inc.1 green
7. [ ] uutils increment (Inc.3) — assess / may be follow-up
8. [ ] full ./check.sh green; sub-agent review; ready + auto-merge

## Findings (de-risk, all on host before wiring)
- cargo builds td-builder OFFLINE from the store toolchain (rust + rust:cargo +
  gcc-toolchain + coreutils + bash); ~4s. ✓
- REPRODUCIBLE: two builds at DIFFERENT build-dir paths → byte-identical binary
  (`7f80f5a2…`) via `--remap-path-prefix` + SOURCE_DATE_EPOCH=1. ✓
- Linking via gcc-toolchain's gcc (ld-wrapper) injects RUNPATH → gcc-toolchain/lib
  (libc + libgcc_s) + glibc interpreter → the output runs on the guix system. ✓
- GOTCHA: a derivation-input's multiple sub-outputs MUST be in SORTED order
  (`'("cargo" "out")`, not `'("out" "cargo")`) or the daemon recomputes a
  different drv hash → "has incorrect output" rejection.
- GOTCHA: `%builder-source` (local-file) lowers to a store PATH (interned), not a
  derivation → it is an input-SOURCE (`#:sources`), not an input-derivation.
- End-to-end: `guix build` of the self-host drv ran `td-builder rust-build` →
  cargo build offline → installed `7xrm…/bin/td-builder`, distinct from guix's
  `fspw8…/bin/td-builder`. ✓

## Verified-red evidence
- GREEN: `./check.sh rust-build` passes — [STRUCTURAL] builder=td-builder arg
  rust-build; [DURABLE behavioral] td-built td-builder RUNS (nar-hash
  sha256:4a4cff56…) and agrees with the guix-built one; [DURABLE repro]
  td-builder check double-build agrees reproducible (1097-item closure);
  [MIGRATION ORACLE] distinct from guix's `fspw8…-td-builder`.
- RED (teeth): with `run_rust` installing a non-working file (Cargo.toml) as the
  binary, the gate FAILS (`GATE_EXIT=2`): "rust-build produced no td-builder
  binary" — the installed file is not an executable. Restored → green. Proves the
  gate gates on rust-build producing a real working binary, not just `cargo`
  exiting 0.

## Scope decision (increments 2/3)
Increment 1 is a complete, green milestone (td-builder owns a Rust-from-source
build path; self-hosting; reproducible by its own double-build). Increments 2
(vendored deps) and 3 (a uutils tool) are each milestone-sized (cargo-vendor
plumbing; a 150+-crate graph) — better as FOLLOW-UP PRs on this track than one
sprawling, hard-to-review change. Recommend landing Inc.1, then Inc.2, then Inc.3.

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
1. [ ] claim + plan-index → draft PR
2. [ ] buildSystem dispatch (ts-eval → recipe JSON → td-build.scm → build.rs)
3. [ ] cargo phase runner in build.rs; rust toolchain inputs
4. [ ] self-host recipe + gate; verified-red
5. [ ] vendored-deps increment
6. [ ] uutils increment (or document as next)
7. [ ] full ./check.sh green; sub-agent review; ready + auto-merge

## Verified-red evidence
(to fill in)

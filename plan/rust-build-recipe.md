# rust-build-recipe — route the rust-build self-host onto the build-recipe rail

Handle: claude-fable-c018e3 · claimed 2026-06-18 · section: side

## Why (human-directed, 2026-06-18)

PR #81 gave td-builder its own `rust-build` cargo runner, but the self-host was
LOWERED by a Guile `(derivation …)` in `system/td-build.scm`
(`td-rust-selfhost-derivation`) and realized by guix-daemon. Meanwhile the
**own-builder-daemon** track (#69–#74, another agent) built the rail this should
ride: `td-builder build-recipe` — resolve every input from a pinned lock (no
`specification->package`), ASSEMBLE the `.drv` in Rust (`store::assemble_drv`, no
guix `(derivation …)`), REALIZE it daemon-free (`realize_drv`), all with guix/Guile
SCRUBBED FROM PATH (the nano-no-guix / toolchain-no-guix gates). Human: "refactor it
… fit it onto their move-off-guix PR." So: drop the Guile construction, route the
self-host through `build-recipe`.

## What changed

- **`builder/src/main.rs` `build_recipe`**: dispatch on the recipe's `buildSystem` —
  `"gnu"` → `arg autotools-build` (configureFlags/phases, unchanged byte-for-byte),
  `"rust"` → `arg rust-build` + `TD_RUST_BINS` from the recipe's `bins`. Additive;
  the gnu spec is identical, so the corpus/toolchain/nano gates' drvs don't move.
- **`tests/ts/td-spec.d.ts`**: `BuildSystem = "gnu" | "rust"`; `bins?`; `source?`
  optional (a rust self-host's source comes from the lock, not a fetchSource URL).
- **`tests/ts/recipe-td-builder.ts`** (NEW): td-builder's own recipe, buildSystem
  rust, bins ["td-builder"], no source (lock-supplied).
- **`tests/td-builder-rust.lock`** (NEW): pinned SEED (rust out=rustc + cargo +
  gcc-toolchain + coreutils + bash). The source line is NOT pinned (the builder tree
  changes every edit) — the gate interns the CURRENT tree and appends it.
- **`tests/td-builder-source.scm`** (NEW): the one bit of source PREP still through
  guix — intern the live builder tree, print `SRC=<path>` (the daemon must register
  it so `realize` can stage it). Same `%builder-source` the td-builder package uses.
  NOT build construction (analogous to guix realizing nano's source tarball).
- **`mk/gates/330-rust-build.mk`**: rewritten onto build-recipe; PREP realizes the
  seed + interns the source (guix-allowed), BUILD runs `build-recipe` with guix/Guile
  off PATH. Legs: [STRUCTURAL] off-PATH build; [DURABLE behavioral] runs + agrees
  with guix-built; [DURABLE repro] td-builder check double-build; [MIGRATION ORACLE]
  distinct from guix's cargo-build-system td-builder.
- **DELETED**: `system/td-build.scm` `td-rust-selfhost-derivation` + `%td-rust-seed-names`;
  `tests/rust-build-drv.scm`. (`td-rust-build-derivation` STAYS — it is the subject
  oracle for the drv-emit / td-drv-add / td-drv-assemble / td-drv-build gates.)

## Sub-task ladder
1. [x] claim + plan-index → draft PR
2. [x] `build_recipe` buildSystem dispatch (cargo check clean)
3. [x] td-spec.d.ts + recipe-td-builder.ts + lock + source helper
4. [x] gate 330 rewritten; delete the Guile path
5. [ ] `./check.sh rust-build` green
6. [ ] verified-red on the new gate
7. [ ] full `./check.sh` green; sub-agent review; ready + auto-merge

## Verified-red evidence
(to fill: break buildSystem dispatch / the off-PATH structural leg, watch red)

# plan/td-drv-build.md — the end-to-end td-driven build (capstone)

Track: **td-drv-build** (DESIGN §7.1, approved 2026-06-13 — §4.3 gate-1, human
go-ahead "start on A", 2026-06-13).
Claim: claude-fable-4a2e33, 2026-06-13.
Single writer: the claiming agent.

## Goal

The capstone of the §5 move-off-Guile arc. The pieces exist:
- #18 surface (TypeScript), #20 corpus (TS-authored recipe), #21 the own Rust builder
  (`td-builder autotools-build` replaces gnu-build-system), #22 the `.drv` construction
  (`td-builder drv-emit`, byte-identical to guix).
- td-builder S1–S4: the Rust executor builds a `.drv` in a userns sandbox NAR-equal to
  the daemon (up to the system image drv).

This track STITCHES emit + execute: for the `td-build` hello derivation, td-builder
EMITS the `.drv` and EXECUTES it, output NAR-equal to the daemon's build of the same
recipe. So both CONSTRUCT and EXECUTE are td's Rust — the derivation's builder is
`td-builder autotools-build` (#21) run by `td-builder build` (S3/S4) — with NO Guile
in either. The daemon is ONLY the differential oracle (prime directive 4).

## Scope boundary (honest)

Still Guix's, retired last (§5):
- input RESOLUTION — which toolchain/source store paths are the inputs;
- the input CLOSURE computation (`guix gc -R`);
- the daemon BUILDS the inputs (gcc-toolchain, …). Only the TOP derivation (hello) is
  td-constructed + td-executed.
This is the same boundary td-builder S4 already lives under (it rebuilt the top image
drv with daemon-built inputs).

## De-risk (2026-06-13) — PASSED before any rung

`td-builder build <hello.drv> <closure> <scratch>` built the hello autotools recipe in
td-builder's EXISTING sandbox (no S4-deferred feature needed) and registered NAR hash
`2e34810a…` == the daemon's recorded hash at the same path `8piymvsm…-hello-2.12.2`,
size 343000, deriver + references equal. So executor=td + builder=td (autotools-build)
is daemon-equal out of the box.

## Plan

- `td-builder drv-emit-to ORACLE OUT` — construct the `.drv` from ORACLE's skeleton
  (#22 `construct_drv`) and WRITE it to OUT (drv-emit verifies; this persists it so the
  executor can build it). Small addition.
- `tests/td-drv-build-drv.scm` — lower the hello drv, daemon-build it, emit oracle
  facts (HELLO_DRV/OUT/HASH/NARSIZE/DERIVER/INPUT) — mirrors td-builder-s4-drv.scm.
- `td-drv-build` rung (heavy): build td-builder; lower + daemon-build the hello drv for
  the oracle facts; `drv-emit-to` the emitted `.drv` (assert byte-identical to guix's);
  stage the input closure; `td-builder build` the EMITTED `.drv`; assert the registered
  output (path, NAR hash, size, deriver) == the daemon's recorded facts.

## Sub-task ladder

1. Charter: graduate §6→§7.1 (this entry is new, not a parked item), claim, this file.
   — DONE 2026-06-13.
2. `drv-emit-to` + the rung. Verify red: an emit defect breaks byte-identity; an
   executor defect (NAR) breaks the differential.
3. Full `./check.sh` green; PR.

## Implementation progress

- **DONE 2026-06-13.** `td-builder drv-emit-to ORACLE OUT` (write the constructed
  `.drv`); `sandbox.rs` derives the build-dir name from the output path for a
  non-store-path `.drv` (store-path inputs — the td-builder rung — unaffected); the
  build's registered deriver is the `.drv`'s canonical store path (computed for a
  scratch input). New heavy `td-drv-build` rung GREEN in-sandbox (`./check.sh
  td-drv-build`): td emits the hello `.drv` byte-identical to guix's (drv-emit
  verifies — no `cmp`, absent in the sandbox), builds the EMITTED file in the
  td-builder userns sandbox, and the registration (path, NAR hash `78f8eec8…`, size,
  deriver) equals the daemon's. Staged closure 1126 items.
- Gotcha: the check sandbox has NO diffutils — `cmp` is unavailable. Byte-identity is
  asserted via `td-builder drv-emit` (#22's verify) instead.

## Verified-red log

`td-drv-build` rung, each driven via `./check.sh td-drv-build`, restored after:
- **R1 emit/construct** — `fixed:out:`→`fixed:outX:` in `hash_derivation_modulo`
  (store.rs) ⇒ the construction diverges ⇒ RED "td's construction is not
  byte-identical to guix's .drv" (the differential's emit half). exit 2.
- **R2 executor NAR** — file-contents padding 8→4 in `nar.rs` ⇒ a NAR unit test
  (`known_nar_of_single_file`) fails inside `guix build td-builder` ⇒ rung red at the
  build. (The differential-level NAR red — a defect past the unit fixtures — is on the
  td-builder track's S4, whose `build` this rung reuses.)
- **R2' executor deriver** — `{}.drv`→`{}X.drv` in the build arm's deriver computation
  (main.rs, NOT unit-tested) ⇒ td-builder builds fine but the differential reds:
  "deriver mismatch — td '…hello-2.12.2X.drv' vs daemon '…hello-2.12.2.drv'". exit 2.

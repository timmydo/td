# bootstrap-recipes-rs — tests/bootstrap-*.sh → structured Rust recipes

rust-migration **C2** (`plan/rust-migration.md`, "C. Scripts → Rust"). Sibling of
C1 (`affected-checks-rs` #226). Port the source-bootstrap shell drivers
(`tests/bootstrap-*.sh`) to **structured recipes** in the `td-builder` engine.

## Why structured

Every `tests/bootstrap-*.sh` is the same skeleton:

```
[pinned-input]  verify the input bytes == the pin (vendored sha, or a td-fetched
                tarball == its seed/sources/*.lock sha256)
build           rung-specific: drive the seed/prior-rung tools over the source
[no-guix]       the artifact carries no /gnu/store bytes; the build ran guix-off-env
[behavioral]    the artifact does its job (runs, evaluates, returns 42, …)
[repro]         a second independent build is byte-identical
```

Only **build** and **behavioral** differ per rung. The shell copies the other
three legs 28 times. The Rust framework makes them ONE typed implementation; each
rung is a `Recipe { name, brick, pins, build, artifacts, checks }` value.

## Design (`builder/src/bootstrap.rs`, std-only — keep the crate dependency-free)

- `Pin::Vendored { path, sha256 }` — in-repo auditable bytes (the seed bricks).
- `Pin::Source { lock }` — a `seed/sources/<n>.lock` (url/sha256/file); the warmed
  `.td-build-cache/sources/<file>` must match the lock sha256 (td-fetched, the
  offline loop never egresses — `tools/warm-bootstrap-sources.sh` host-prelude).
- `Recipe.build: fn(&Ctx) -> Result<Built>` runs the rung with a SCRUBBED env
  (`Command::env_clear()` — the Rust `env -i`, the "guix off env" proof).
- `run(recipe)` = the generic leg runner: pins → build(r1) → no-guix(artifacts) →
  checks → build(r2) → repro(artifacts) → PASS report.
- `td-builder bootstrap-recipe <name>` / `--list` subcommand.

## Verification — own, then diverge (directive 4)

The seed gate is **all-durable** (no guix oracle — the seed IS the irreducible
bottom), so the Rust runner asserts the SAME durable legs. Proof tiers:

- **Durable, fast** (`cargo test`): pure-logic `#[test]`s (lock parse, kaem.run
  extraction, the per-leg red paths) run everywhere. The repo-tree integration
  tests (the **seed** rung built end-to-end + the wrong-pin red) run in the
  repo-rooted cargo-test job (affected-checks step + CI) where `seed/stage0` is
  present; they SKIP inside the hermetic `check-engine` crate build (only `builder/`
  is staged there), keeping that smoke "compile + unit tests, no source build".
  Verified-red by perturbing each pin/leg.
- **Durable, heavy**: gates `bootstrap-seed` (360) + `bootstrap-mes` (364) now RUN
  the Rust recipe (`td-builder bootstrap-recipe {seed,mes}`) in the loop sandbox.
- **CUTOVER (directive 3 — called out in the PR):** the shell
  `tests/bootstrap-{seed,mes}.sh` are DELETED and gates 360/364 repointed to the
  Rust recipe. No shell oracle is kept — these gates are all-durable (no guix
  oracle; the shell was the prior *implementation*, not a guix differential). The
  Rust recipe asserts a SUPERSET of the shell's legs; own-then-diverge was done
  before deletion (mes-m2 proven byte-identical, sha 203e5516…; seed asserts the
  same pins). The duplicate `*-rs` gates were removed.

## Sub-tasks

1. [x] framework + `seed` recipe; subcommand; cargo `#[test]`; verified-red.
2. [x] `mes` recipe (Pin::Source, kaem.run input-list port, M2P/BE/M1/HEX2 chain);
       byte-identical to the shell oracle.
3. [x] CUTOVER: delete the shell drivers, repoint gates 360/364 to the Rust recipe,
       update the affected.rs routing (seed tree + mes lock → gates; bootstrap.rs →
       check-engine).
4. [x] code review over the branch; address; PR ready; land.

Follow-ups (own PRs): mescc, tcc, gcc-mesboot*, glibc-mesboot, … — each ports its
shell driver to a `Recipe`, deletes the shell, and repoints its gate.

## Verified-red log

Each shared leg has a native red test in `builder/src/bootstrap.rs` (`cargo test
bootstrap`, 9 tests green):
- `seed_recipe_builds_and_passes_all_legs` — the real seed rung is GREEN end-to-end.
- `wrong_vendored_pin_reds_pinned_input` — a wrong pin reds `[pinned-input]`.
- `gnu_store_in_artifact_reds_no_guix` — a `/gnu/store` byte reds `[no-guix]`.
- `nondeterministic_build_reds_repro` — a non-deterministic build reds `[repro]`.
- `failing_check_reds_run` — a failing behavioral check reds the run.
- `green_synthetic_passes` — the synthetic control is green (legs not vacuous).

Sub-task 1 (framework + seed) done: green via `cargo test` and via the stage0
binary (`td-builder bootstrap-recipe seed` → all legs + PASS).

Sub-task 2 (mes) done: `td-builder bootstrap-recipe mes` (TD_SOURCES_DIR → the warm
cache) → all legs + PASS. Reproducible (two builds same sha). **Own-then-diverge:
the Rust-built mes-m2 is BYTE-IDENTICAL to the shell oracle's** —
`203e5516bbde550e40602ca43435502760bad5f35929eef6596cd225ed0c1c27` from both
`bootstrap-recipe mes` and an instrumented `tests/bootstrap-mes.sh` run — so the
kaem.run input-list port + the M2P/BE/M1/HEX2 chain are faithful. `cargo test
m2planet_units_extracted_in_order_with_cpu_subst` covers the extraction.

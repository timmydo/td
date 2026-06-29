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
- **Durable, heavy** (new `mk/gates/*-rs.mk`): the Rust runner builds seed + mes
  in the loop sandbox.
- **Removable oracle**: the shell `tests/bootstrap-{seed,mes}.sh` + gates 360/364
  stay UNCHANGED as the live driver and the differential oracle. During dev the
  Rust-built artifact is confirmed byte-identical to the shell-built one (both are
  reproducible + build from the same pin). No cutover this PR (same scoping as
  #226 — cutover would make the gate depend on a built td-builder).

## Sub-tasks (smallest-correct first)

1. [ ] framework + `seed` recipe; subcommand; cargo `#[test]`; gate `361-…-rs.mk`.
       Verify green + verified-red (cargo test, `bootstrap-recipe seed`).
2. [ ] `mes` recipe (Pin::Source, kaem.run input-list port, M2P/BE/M1/HEX2 chain);
       gate `365-…-rs.mk`. Verify against the warm mes-0.27.1 tarball.
3. [ ] code-review subagent over the branch; address; PR ready; land.

Follow-ups (own PRs/tracks): mescc, tcc, gcc-mesboot*, glibc-mesboot, …; then the
cutover (shell driver → thin shim exec'ing `td-builder bootstrap-recipe`).

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

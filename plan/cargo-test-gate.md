# Track: cargo-test-gate (side-track)

**Handle:** claude-fable-840226 · **Claimed:** 2026-06-15

## Goal

Add a gate that runs td-builder's Rust unit tests DIRECTLY via `cargo test`
(offline, toolchain-only) instead of only inside the `cargo-build-system`
package build. First step of the loop-latency brainstorm's "push logic down into
fast unit tests" lever: 39 `#[test]`s already live in `builder/src/*.rs` but the
only way to exercise them today is a full `guix build` of the td-builder package
(a release rebuild that ~15 heavy gates trigger). A direct `cargo test` gives
sub-20s feedback on that logic.

## Design decisions

- **Scope: `builder/` only.** All 39 tests are there; `ts-eval` has 0 tests and
  vendors boa through a fixed-output (running cargo test on it offline would need
  the vendored registry wired up) — out of scope for this increment.
- **HEAVY, not FAST.** The required per-PR CI checks are `lint` + `check-fast`,
  and `check-fast` runs offline against the small `td-ci-fast` image, which ships
  node+tsc+cheap-rung closures but **no rust toolchain** (ci/lower-fast-drvs.sh).
  Tagging `cargo-test` FAST would red the required check offline. So it goes in
  HEAVY: it runs in the dev-machine full `./check.sh` (the §7.2 step-2 landing
  gate) and the ci-image pipeline's full `td-ci` validate job — both carry rust.
  Promoting it to FAST is a follow-up: add the rust+builder closure to
  ci/lower-fast-drvs.sh and rebuild the (no longer so small) fast image.
- **Offline:** `guix shell --no-substitutes --no-offload rust rust:cargo` resolves
  from the warm store (rust is already in td-builder's build closure). `cargo test
  --frozen` (= --locked --offline) with a dep-free crate touches no network.
- **Scratch dir** `.cargo-test-scratch/` (CARGO_HOME + CARGO_TARGET_DIR) at repo
  root, gitignored, wiped each run. It is OUTSIDE `builder/`, so it cannot perturb
  the td-builder package source hash (local-file "../builder").
- **Anti-vacuous:** assert a `test result: ok. <N> passed` line with N>=1, so a
  build that silently compiled/ran 0 tests cannot green the gate.

## Sub-task ladder

1. [ ] gate fragment mk/gates/325-cargo-test.mk + .gitignore entry; green via
   `./check.sh cargo-test`.
2. [ ] verified-red: break a builder test, watch the gate red, revert.
3. [ ] sub-agent diff review against CLAUDE.md.
4. [ ] full `./check.sh` green; land per §7.2.

## Verified-red evidence

(to fill in)

## Measurement

(record `./check.sh cargo-test` wall time here)

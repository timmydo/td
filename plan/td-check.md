# plan/td-check.md — td owns the reproducibility oracle (replace `guix build --check`)

Track: **td-check** (DESIGN §7.1, gate-2 — human go-ahead 2026-06-13: "then the
gate-2 items (td-check oracle, loop sandbox)"). Claim: claude-fable-4a2e33,
2026-06-13. Single writer.

## Goal

Prime directive 1 says *reproducibility is a test*; today that verdict comes from
`guix build --check` (the daemon builds twice and compares). This makes td compute
that verdict ITSELF: `td-builder check DRV CLOSURE SCRATCH` executes the `.drv` TWICE
in two independent user-namespace sandbox runs (reusing the #25 executor
`sandbox::build`) and compares the per-output NAR hashes (reusing the #21/S2 NAR
serializer + SHA-256). Equal ⇒ reproducible; that is td's own `--check`, with no
daemon and no `guix build --check` in the verdict.

This is the OBSERVE step of gate-2, done honestly: the rung does NOT remove
`guix build --check` from any existing rung (that would be weakening — directive 3).
It ADDS a rung where td's verdict is PROVEN to match guix's `--check` verdict on the
same `.drv` — the differential-before-replacement (directive 4) that a later increment
needs before `guix build --check` can be retired for a target.

## Scope boundary (honest)

- Input RESOLUTION (which toolchain/source store paths are inputs) stays Guix's — the
  toolchain is retired last (§5).
- The daemon still BUILDS the inputs (the staged closure) and is the source of the
  oracle `--check` verdict. Only the TOP derivation's reproducibility is computed by
  td's double-build. Same boundary as [[td-drv-build]].

## How

- `builder/src/main.rs`: `check FILE.drv CLOSURE-FILE SCRATCH-DIR` — read+parse the
  `.drv`, read the closure, `sandbox::build` into `SCRATCH/r1` then `SCRATCH/r2`,
  NAR-hash each output of both runs (`nar_hash_path`, the Path variant factored from
  the existing `nar-hash` subcommand), and assert hash(r1)==hash(r2) per output.
  Prints `CHECK <out> <store-path> <hash> reproducible`; exits 3 (NON-REPRODUCIBLE)
  if any output diverges, 0 if all reproducible. Std-only, no new crate.
- `Makefile` `td-check` rung: lower the `td-build` hello `.drv` + stage its closure
  (`guix gc -R`, reused from [[td-drv-build]]); `td-builder check` the `.drv` (td's
  double-build agrees → reproducible); then assert the differential oracle
  `guix build --check "$drv"` also agrees (exit 0). Heavy (two td hello compiles +
  the oracle `--check`), so it slots in the heavy pool.

## Differential / honesty

The rung proves td's verdict == guix's `--check` verdict on the SAME `.drv`: td's
double-build says reproducible (two independent NAR hashes equal) AND `guix build
--check` says reproducible. Not idempotency — the two td builds are independent userns
runs into separate scratch trees. `guix build --check` remains the oracle (directive
4); nothing existing is loosened (directive 3).

## Sub-task ladder

1. Claim + `check` subcommand + `nar_hash_path` factor. — sub-task A.
2. The `td-check` rung. Verify red: a non-deterministic builder makes the two td
   builds diverge ⇒ `td-builder check` exits non-zero ⇒ rung red (and guix --check
   would fail too — both agree). — sub-task B.
3. Full `./check.sh` green; PR. — sub-task C.

## Verified-red log

(filled as each assertion is seen red.)

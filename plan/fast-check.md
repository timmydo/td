# plan/fast-check.md — halve the warm inner-loop (memoize td's reproducibility double-build)

Track: **fast-check** (side-track; make `./check.sh` faster).
Claim: claude-fable-aeb054, 2026-06-15.
Single writer: the claiming agent.

## The finding (measured, not guessed)

Warm per-gate timing on origin/main HEAD (3ab6ea7), `./check.sh <gate>`:

| gate | warm | note |
|---|---|---|
| `corpus` (base) | 32s | memoized `guix --check`, no td double-build — fixed overhead floor |
| `corpus-pkgconfig` | 128s | un-memoized `td-builder check` double-build (pkg-config compiled twice) |
| `corpus-gzip` | 80s | un-memoized double-build |
| `reset` (VM) | 17s | image cached — VM boots already cheap (CoW reset + #62) |
| `boot-disk` (VM) | 17s | already amortized (#62) |

Conclusions:
- VM-boot amortization is **not** the lever any more — warm boots are ~17s.
- The dominant *avoidable* warm cost is td's OWN reproducibility proof:
  `tests/td-check-repro.sh` runs `td-builder check`, which rebuilds the recipe
  from source TWICE every run — even when the drv is unchanged. The `guix --check`
  legs (which one might suspect as "testing guix") are ALREADY memoized
  (`tests/check-memo.sh`) and cheap warm; they only bite cold/CI.
- So the fix is the trick already applied to `guix --check`: memoize the td-builder
  double-build, keyed on the drv hash.

## The change

Make the shared helper `tests/td-check-repro.sh` verdict-memoized, mirroring
`tests/check-memo.sh` exactly:
- **key**: the drv store path (content-addressed — a changed/perturbed recipe is a
  different drv ⇒ always a MISS ⇒ the real double-build runs ⇒ verified-red intact).
- **guards** (all from check-memo, reusing check.sh's already-exported `TD_CHECK_*`):
  env-id binding (`TD_CHECK_ENV`; empty under CI ⇒ never memoize, fail closed),
  bounded TTL (`TD_CHECK_TTL_DAYS`, default 7, refuse >14), `TD_CHECK_FULL=1` bypass.
- **on-hit cheap re-assertion** (parity with check-memo constraint 5): the verdict's
  output paths must still be valid in the daemon DB with the recorded NAR hash+size
  (reusing `tests/check-memo-info.scm`; the output is in the store because the gate
  ran `guix build $td_drv` before the helper). Any disagreement ⇒ MISS.
- verdicts live in `.td-check-verdicts/` (separate from check-memo's
  `.check-verdicts/` to avoid the `$(basename drv).verdict` filename collision),
  host-local, gitignored, NEVER committed.

Single-place change: all 4 recipe gates (pkgconfig/libatomic/popt/gzip) call this one
helper, and every future recipe gate the input-recipes track adds gets memoization for
free with no extra wiring.

## Loosening disclosure (directive 3 / DESIGN §4.3 gate-2)

This SKIPS the td-builder double-build on a fresh hit — the same trade the human
already approved for `check-memo` (guix `--check`). Surfaced here and in the PR so it
is approved knowingly. It can only ever skip work for an UNCHANGED drv already proven
reproducible in THIS environment within the TTL; `TD_CHECK_FULL=1 ./check.sh` runs the
full double-build unconditionally.

## Coordination

`tests/td-check-repro.sh` is shared by the input-recipes track's recipe gates. This PR
only ADDS memo behavior (same interface: `td-check-repro.sh TB DRV INFILE SCRATCH`), so
a new recipe gate keeps working unchanged and gains memoization automatically. Rebase if
that track edits the helper.

## Verified-red log

The safety property: memoization must NEVER falsely green — a stale/forged verdict
must MISS and re-run the real double-build. Exercised on `corpus-gzip` (drv
`6pajp3gyq2sr4s6j12zw36qnbk8l023q-gzip-1.14.drv`), each run captured the helper's
`TD-CHECK MEMO` line and the gate's PASS/FAIL:

| # | setup | observed decision | gate | time |
|---|---|---|---|---|
| A | cold (no verdict) | MISS (no verdict) → RECORD | PASS | 62s |
| B | warm | **HIT — double-build skipped** | PASS | **21s** |
| D | `TD_CHECK_FULL=1` | MISS (forced full) → rebuild | PASS | 80s |
| C | recorded NAR hash tampered | MISS (verdict/DB mismatch) → rebuild | PASS | 63s |
| E | verdict `env` line tampered | MISS (foreign environment) → rebuild | PASS | 63s |
| F | warm again | HIT | PASS | 23s |

So a hit happens ONLY for an unchanged drv whose recorded NAR hashes still match the
daemon DB in this same environment; tampering the hash (C), forging the environment
(E), or forcing full (D) each correctly fall back to the real `td-builder check`
double-build — the gate never greens off a stale verdict. A changed/perturbed recipe
is a different drv ⇒ no verdict ⇒ a (A)-style miss. (The expired/malformed-timestamp
guards are the exact check-memo code on the same control flow.)

## Measured speedup

- Per recipe gate (`corpus-gzip`): warm repeat **80s → 21s** (the un-memoized
  double-build is the ~60s skipped on a hit). `corpus-pkgconfig`, the heaviest, was
  128s un-memoized warm; the same ~60–95s double-build is now skipped on a hit.
- Full `./check.sh` A/B on this same tree (only `.td-check-verdicts/` differs):
  (to fill in — cold-verdicts vs warm-verdicts full-loop wall time.)

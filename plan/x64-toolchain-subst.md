# x64-toolchain-subst — make x86_64 the canonical, SUBSTITUTED /td/store toolchain

Handle: claude-opus-cedce1

## Problem (verified 2026-06-28, main @ #218)

The substitute mechanism is real protocol-wise but the x86_64 cross toolchain — the one we
actually want to fetch — feeds no real bytes into it. The x86_64 cross (#201) is
content-addressed (path varies build-to-build), so a consumer can't NAME it to fetch it; its
~90-min build is recomputed and discarded every run. #219 then gave the x86_64 toolchain a
stable input-addressed key + addressing gate (418), but that gate keys the path with a
**static-bash FIXTURE** — it proves the *addressing*, not that the *real* cross-built x86_64
glibc/gcc bytes live at that path. So the gap this PR closes: tie the lock-keyed path to the
REAL cross-built x86_64 toolchain and run a dynamic x86_64 binary off it.

## Landscape (what landed while this track was in flight)

- **#219 (toolchain-x86_64-input-addressed)** — `tests/td-toolchain-x86_64.lock` + gate 418:
  the stable input-addressed KEY for the x86_64 toolchain (fixture-keyed addressing). This PR
  REUSES that lock unchanged (took main's copy on rebase).
- **#213 (toolchain-subst-default)** — wired `publish-toolchain-subst.sh` into
  `ci/daily-full-suite.sh` + a persistent `~/.td/subst` store; gate 412 emits the real
  toolchain as a signed substitute export. This was the publisher work originally scoped here
  as "PR2" — now DONE upstream.
- **#218 (rust-store-native)** — the x86_64 Rust runtime leg runs from `/td/store` (green).

## Direction (human 2026-06-28)

x86_64 is the canonical /td/store toolchain (locked, published, fetched by default). i686 is
the bootstrap intermediate. The i686→x86_64 split stays at the **gcc 14 path** (build the i686
chain up through gcc 14.3.0, then cross with it — human OK'd; gate 414 / #201 already does this).
NOT an earlier gcc-4.x split.

## Ladder

- **PR1 (LANDED #215) — REAL x86_64 bytes at the lock-keyed path.** Gate 414 interns the REAL
  cross-built x86_64 glibc 2.41 at the input-addressed path from `tests/td-toolchain-x86_64.lock`
  (#219's lock) and RUNS a dynamic x86_64 binary whose interp IS that path → 42, /gnu/store absent.
- **PR2 (DONE upstream by #213)** — publisher wired into `ci/daily-full-suite.sh` + persistent
  `~/.td/subst` store + gate 412 substitute export (i686).
- **PR3 (THIS PR, #223) — the per-PR build-SKIP.** Reworked from #223's earlier "fetch-in-addition"
  (which proved the consumer capability but still always built — INSUFFICIENT per the human
  2026-06-28). Now gate 414 RESOLVES the 3-component closure FIRST; on HIT it places them at
  /td/store and SKIPS the ~98-min from-seed build; on MISS it builds from seed, interns +
  subst-exports the closure (the daily signs + publishes). A unified `x86_64_verify_closure`
  compiles+runs an x86_64 program with the closure (built OR fetched) → 42, /gnu/store absent.
  td-subst is host-provisioned (the daily stashes it). DELIBERATE directive-1 relaxation,
  human-approved. KEY FINDING: fetch-instead-of-build was wired for NO arch before this. Full design
  in "The REAL skip" below; folds in the old "PR3b".

## The REAL skip — locked design (human 2026-06-28, the 5 points)

Goal: a per-PR toolchain gate SKIPS the ~98-min from-seed build by FETCHING the toolchain
CLOSURE from a persistent `~/.td/subst` exposed into the sandbox; falls back to from-seed on MISS.

- **Closure = the 3 `td-toolchain-x86_64.lock` components** {binutils-2.44-x86_64, gcc-14.3.0-x86_64,
  glibc-2.41-x86_64}. FINDING: the cross gcc/binutils are built `-static` (`_mk_static_wrapper` →
  `gcc-14 -static`, used as stage2 `CC`), so they are static i686 binaries that DO NOT need the
  i686 glibc-2.16 runtime — the closure is just the 3 x86_64 components, each already lock-keyed.
  (If validation ever shows the cross gcc isn't fully static, add the i686 runtime; the `-static`
  wrapper says it is.)
- **td-subst provisioning (resolves the ts-eval cascade):** the per-PR gate must NOT build td-subst
  (gate 414 isn't a BUILD_GATE → no ts-eval sentinel → `ts-emit` fails; making it a BUILD_GATE drags
  in the whole corpus). Instead the **DAILY** (full `./check.sh`, has td-subst via build-recipes)
  builds the closure from seed, publishes the signed closure to `~/.td/subst`, AND stashes the
  `td-subst` binary there. `check.sh` host-prep (a `tools/warm-subst.sh`, feed-shared pattern)
  exposes `TD_SUBST_BIN`/`TD_SUBST_STORE`/`TD_SUBST_PUBKEY` into the sandbox. Per-PR gate CONSUMES.
- **Gate restructure (the actual skip):** move `load_stage0` + a resolve step to the TOP of gate 414
  (before the i686 base build). If `TD_SUBST_BIN`+`STORE` set AND resolve HITs all 3 → place at
  /td/store, set XBU/XGCC2/XGLIBC to fetched, SKIP `build_*`+`run_x86_64_cross`; else build from seed
  + intern all 3. `verify_x86_64_ownroot` then compiles+runs from whichever toolchain (fetched/built).
- **Daily publisher** (`ci/daily-full-suite.sh`): publish all 3 closure components (signed) + stash td-subst.
- Revert #223's in-gate td-subst/round-trip (the ts-eval dead-end) — td-subst now comes from host-prep.

Increment order: (A) gate: intern all 3 + resolve-first SKIP branch using env td-subst, verify from
either; (B) check.sh host-prep warm-subst.sh (EXCLUSIVE); (C) daily publisher closure+stash. Skip is
testable only against a populated store (manual: build once → publish → re-run → HIT). Multi-hour,
multiple ~98-min validations.
- **PR3b — FOLDED INTO PR3 (done).** The per-PR full-build SKIP is implemented above. (The closure
  turned out to be just the 3 x86_64 components — the cross gcc/binutils are static i686, so NO i686
  glibc-2.16 runtime is needed in the closure, simpler than first feared.)
- **PR4 — x86_64 userland + i686 demotion.** Build the corpus userland (hello/sed/…) x86_64;
  stop publishing/consuming the i686 final toolchain — keep it only as the cross intermediate.

## Verified-red (2026-06-28, td-builder built from this branch)

Path-function legs, red against the real `td-builder` (no toolchain build needed — the key is a
pure function of the lock):

- `[distinct-arch]` — GREEN: x86_64 glibc path `qvfcl8…-glibc-2.41-x86_64` ≠ i686
  `i8fh6m8…-glibc-2.41`. RED: rename the x86_64 lock to the i686 `name`+component names → the
  path COLLAPSES onto the i686 path (`i8fh6m8…`), i.e. the `IAGL != ILGL` leg would red. The
  `-x86_64` differentiation is load-bearing for the no-collision guarantee.
- `[load-bearing]` — RED: flip the glibc-2.41 input pin in the x86_64 lock → the path MOVES
  (`vr9c6v…`), confirming the key tracks the declared inputs.

Behavioral leg `[behavioral/input-addressed]` (run the interned x86_64 binary → 42): assertions
are fail-closed (`IAGL` is always a real interned path or the call errors red, so the equality
`test "$IAGL" = "$WANTGL"` can't pass vacuously) and reuse gate 414's already-verified-red
store-ns own-root mechanism ("drop the baked interp → can't run in own-root"). Exercised
end-to-end by the from-seed `./check.sh bootstrap-x86_64-toolchain-store-native` run.

## Verified-red — PR3 subst round-trip (`tests/x86_64-subst-lib.sh`)

- `[subst/run-from-fetched]` (a program runs from the FETCHED-not-rebuilt glibc → 42): the red is
  the same own-root mechanism as PR1 — if the fetched bytes aren't placed at the lock path, or the
  fetch returns wrong/empty, the program's baked interp is missing → no `FRC=42`. The fetch result
  is also fail-closed: `resolve-toolchain.sh` prints a path ONLY on a verified HIT (else exit 1).
- `[subst/fallback]`, `[subst/self-discrimination]` (cold store / wrong key / wrong StorePath):
  these legs ARE the red — each asserts the resolver REJECTS a bad substitute (exit 1). They were
  proven red-equivalent for i686 in gate 359 (the resolver code is arch-agnostic); a resolver that
  wrongly ACCEPTED any of them reds the leg. Exercised end-to-end by the from-seed run on the REAL
  x86_64 bytes.

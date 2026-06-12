# Track: check-memo (side-track)

**Claim status:** see `PLAN.md`.
**Origin:** approved by the human 2026-06-12. This file is the record of BOTH
§4.3 gates for this change: gate 1 (roadmap addition — the §7.1 entry) and
gate 2 (weakening the loop — the verdict-memoization policy below changes when
the `--check` rebuild runs, which is a loosening of an existing assertion
schedule and therefore needed explicit human sign-off, regardless of
justification). The human's approval of the chartering PR is the signature.
**Scope authority:** DESIGN §7.1.

## The decision being signed off

The `guix build --check` reproducibility legs of the heavy rungs MAY skip the
rebuild-and-compare when a **recorded verdict** shows that this exact
derivation already rebuilt bit-identically in an equivalent environment and
the verdict is fresh. On a miss (no verdict, changed drv, expired, foreign,
or force-full), the leg runs the real `guix build --check` exactly as today
and records the verdict on green.

Motivation (measured, `plan/loop-latency.md`): the `--check` legs re-realise
multi-GB image derivations every run and dominate the unchanged-tree floor
(~430s of ~534s serial; still the bulk of the ~341s `-j2` floor at 18 rungs,
more at 24). On an unchanged tree the drv hash is unchanged, so the rebuild
re-answers a question whose deterministic inputs have not changed.

**The trade, stated honestly:** rebuilding an UNCHANGED drv catches
environmental and probabilistic nondeterminism. This is not hypothetical —
on 2026-06-12 the hosted CI runner caught a `docker-image.tar.gz` drv whose
output is filesystem-dependent (readdir-order divergence, btrfs dev box vs
ext4 runner — see `plan/ci-gate.md`); environment-dependence like that is
exactly what constraint 2's same-environment keying preserves detection of.
The policy trades some unchanged-drv re-checking for cycle time; constraints
1–6 below are BINDING (changing any of them re-opens gate 2; strengthening
is free, as always).

## Binding constraints

1. **Verdict key = the drv store path** (content-addressed: covers the full
   input closure), recording the output NAR hash it matched. A changed drv
   can never hit. Verified-red required (see obligations).
2. **Verdicts are host-local state adjacent to the store, never committed to
   the repo.** A verdict greens only the environment that produced it;
   cross-host/cross-store reuse is forbidden. For ephemeral CI runners,
   "same environment" may be defined as (runner image version, imported CI
   store image digest, channel pin) — but verdict reuse in CI stays OFF, and
   enabling it RE-OPENS GATE 2 (explicit human sign-off, recorded here):
   once the CI `check` is a required merge check, memoizing its legs further
   loosens when a required assertion runs. Until then CI runs the full legs.
3. **Verdicts expire.** TTL default 7 days; tightening is free, loosening
   beyond 14 days re-opens gate 2, never infinite. Expiry forces a real
   rebuild — the periodic re-check that bounds how long environmental drift
   can hide.
4. **A force-full knob bypasses all verdicts** (env var or make target,
   wired through `./check.sh`). Oracle re-baselines (any `DIGESTS.md`
   change) and any suspected nondeterminism MUST run force-full; the
   re-baseline procedure must say so where it is documented.
5. **A hit is a cheap assertion, not a no-op:** the leg still asserts the
   output is valid in the store DB and that its recorded NAR hash equals the
   verdict's hash. A vanished or tampered-DB output cannot green on a hit.
6. **Nothing else changes.** The rung list, every other assertion, the
   build-before-check semantics on a miss, hermeticity, and the
   offline/no-substitutes posture are untouched. This policy applies only to
   the `guix build --check` reproducibility legs.

## Acceptance (the §7.1 test)

With a warm store and fresh verdicts, the full `./check.sh` unchanged-tree
floor drops measurably (record before/after here; expectation: the OCI
`--check` legs collapse from minutes to seconds) with all rungs green; the
force-full knob demonstrably runs the original full ladder; and the four
verified-reds below are on record in this file.

## Verified-red obligations

- **(A) drv change always rebuilds:** after a green run, change an input so
  the drv hash moves; assert the leg really rebuilt (not hit). Red variant:
  key verdicts by name instead of drv hash and watch the changed drv falsely
  hit.
- **(B) expiry forces rebuild:** age a verdict past TTL; assert rebuild. Red
  variant: drop the TTL check and watch the stale verdict hit.
- **(C) foreign verdicts rejected:** a verdict record produced under another
  store/environment identity must miss. Red variant: drop the identity check.
- **(D) detection power intact on a miss:** an injected-nondeterminism drv
  (no verdict) reds exactly as today.

## Suggested sub-task ladder

1. Verdict record format + store location + environment identity; the
   read/check/record helper; force-full knob through `check.sh` (spine —
   exclusive landing, §7.3).
2. Wire one `--check` leg (pick the slowest: `generation-image`) through the
   helper; verified-reds A–D on it (Makefile/tests — spine, §7.3).
3. Roll out to the remaining `--check` legs (spine, §7.3); re-measure the
   floor; record before/after numbers here.
4. (Optional — re-opens gate 2 per constraint 2; human sign-off recorded
   here first) CI verdict cache per constraint 2's environment definition.

## Working state

- 2026-06-12: track chartered; sign-off recorded (this file + DESIGN §7.1
  entry). UNCLAIMED — no implementation yet.
- Relation to parked td-check (§6): td-check replaces the engine that does
  rebuild-and-compare; this track changes only WHEN it runs. If td-check
  graduates later, it inherits this policy and its constraints unchanged.
- 2026-06-12 claude-fable-580472: claimed (PR #12). **Spine announcement
  (§7.3):** this track lands edits to `check.sh` and the `Makefile` —
  exclusive-landing rules apply; siblings rebase.
- **Baseline (the "before" number):** unchanged-tree full `./check.sh` on the
  dev host, 24 rungs, warm store — **440s wall (7m20.3s), green** (2026-06-12;
  measured while a small concurrent rung run was also active, so the quiet
  floor is slightly lower; reference: 341s at 18 rungs, 2026-06-10).
- **S1 done (sub-ladder step 1):** `tests/check-memo.sh` (read/check/record
  helper; misses fail CLOSED into the real `--check`), `tests/check-memo-info.scm`
  (daemon-DB validity + NAR hash/size query — the constraint-5 assertion),
  `tests/check-memo-drvs.scm` (fixtures: det, same-name/different-hash det',
  µs-clock nondet), the permanent `memo` rung (miss/hit/changed-drv/expiry/
  foreign/tamper/force-full/empty-identity/TTL-cap/nondet legs, every loop),
  and check.sh's host-side environment identity
  (`machine-id:store-fs-type:pinned-commit`, EMPTY under CI or when unknown —
  fail closed) carried in via `--preserve='^TD_CHECK_'`, with
  `TD_CHECK_FULL=1 ./check.sh` as the constraint-4 knob. `./check.sh memo`
  green.

### Verified-red evidence — the `memo` rung (S1, 2026-06-12)

Each helper mutation run via `./check.sh memo`; ALL red (make exit 2), each
caught at the intended leg; helper restored from the committed green after
each (every run also re-proved the wiring assert + earlier legs green first):

- **(A twin — keying)** verdict key by NAME (`basename | cut -c34-`) + drv
  field comparison dropped → RED at the hit leg ("the second sight did not
  HIT"): name-keyed lookup vs path-keyed record diverged — any keying
  inconsistency reds the rung before the changed-drv leg is even reached.
- **(B twin — expiry)** TTL comparison dropped (`elif false`) → RED: "an
  EXPIRED verdict did not miss".
- **(C twin — identity)** env comparison dropped → RED: "a FOREIGN verdict
  did not miss".
- **(constraint 5 — tamper)** verdict/DB comparison dropped → RED: "a
  TAMPERED verdict did not miss".
- **(D twin — exit honesty)** `--check` exit swallowed (`|| true`) → RED:
  "the helper GREENED a deliberately nondeterministic drv on a miss —
  detection power lost".
- **(wiring)** `--preserve='^TD_CHECK_'` removed from check.sh → RED:
  "TD_CHECK_ENV is not exported into the sandbox".

### Memoization boundary (constraint 6, decided at S1)

The helper applies ONLY to the pure reproducibility `--check` legs. Two rungs
keep their direct `guix build --check` calls on purpose and are NOT wired:

- **`offline`** — its `--check` exists to RE-EXECUTE the sandbox probe's
  behavioral assertions every loop (the rung's own comment); memoizing it
  would loosen when those assertions run, which is gate-2 territory.
- **`rootless`** — its `--check` runs inside the rootless daemon and IS the
  differential under test, not a reproducibility leg of ours.

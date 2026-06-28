# toolchain-subst-default — working notes (claude-opus-0b4464, 2026-06-28)

Make the lock-keyed /td/store toolchain substitute the **default** for the loop: a toolchain
gate FETCHES the signed substitute instead of rebuilding the ~18-rung from-seed chain
(~90 min). Human steer 2026-06-28 ("loop substitutes too"): the per-PR/local loop no longer
builds the toolchain from source — `ci/daily-full-suite.sh` on fresh main is the SOLE
remaining from-seed authoritative build + the publisher. **Deliberate directive-1
relaxation** — surfaced in the gate header + the PR for knowing sign-off (directive 3).

Builds on [[td-toolchain-input-addressed]] (#204, the stable key) and [[toolchain-subst]]
(#207, the lock-keyed publish→fetch leg in gate 358), which deferred exactly this wiring.

## What already exists (reuse, do not rebuild)

- `td-builder toolchain-key LOCK` / `toolchain-path LOCK [NAME]` — the stable input-addressed
  /td/store path from `tests/td-toolchain.lock` (a pure function of declared inputs).
- `td-builder store-add-input-addressed NAME KEY SRC STORE-DIR DB` — intern at that path.
- `td-builder subst-export DB STORE-DIR OUTDIR PATH...` — write `<hash>.narinfo` + `nar/…`.
- `subst keygen/sign/serve/fetch` — ed25519 sign + loopback serve + verify-on-fetch.
- `td-builder nar-restore NAR DST` — restore a fetched nar byte-identically.
- gate 358 proves the whole round-trip + self-discrimination (tampered narinfo / corrupt nar
  / wrong key all red), but with EPHEMERAL served dirs + EPHEMERAL keys per run.

## The genuinely-new pieces

1. **Pinned trust anchor** `tests/td-subst.pub` — the ed25519 public key the loop verifies
   fetched substitutes against. Private half = host/daily-runner secret (like the bot keys),
   NEVER in-repo.
2. **Persistent substitute store** keyed by the toolchain lock (served loopback in the
   netns-offline loop; populated by the publisher / host-prep, like `warm-tsgo`).
3. **Consumer-default resolver** `tools/resolve-toolchain.sh` (sourced by the bootstrap
   gate): compute `toolchain-path` → if the persistent store has it AND it verifies (sig vs
   pinned pub + narinfo StorePath == lock-computed path + NarHash) → serve+fetch+restore →
   echo the path; ELSE echo nothing → caller builds from seed (+ publishes).
4. **Publisher** `tools/publish-toolchain-subst.sh` (daily suite, post from-seed build):
   intern the built toolchain input-addressed → `subst-export` + `sign` → persistent store.
   Includes the gate-412 `store-add-recursive` → `store-add-input-addressed` + `subst-export`
   swap (the deferred real-bytes producer).

Granularity = whole-toolchain (the lock already keys the whole toolchain → one fetch
replaces all rungs). Trust = signature + input-addressed name (the toolchain is NOT
byte-reproducible; repro-equality is task 3, separate).

## Sub-task ladder (each green before the next)

- [ ] **0** Track claim + draft PR (this commit).
- [x] **1** `resolve-toolchain.sh` + pinned `tests/td-subst.pub` + gate
      `toolchain-subst-default` (mk/gates/359) proving DEFAULT fetch (warm store → fetch +
      verify + restore + RUN, no build) AND fall-back (cold store / wrong key / wrong
      StorePath → MISS exit 1 → from-seed). affected-checks maps the new paths (+
      td-toolchain.lock now keys both gates). **Committed `6570a39`.** Validated end-to-end
      via host smoke (cargo-built td-builder+td-subst, signed with the REAL pinned key —
      all five legs green). The full in-sandbox gate run (builds td-subst, corpus prelude)
      defers like #207.
- [~] **2** `publish-toolchain-subst.sh` DONE + gate-covered (the gate's producer leg calls
      it; full publish→resolve round-trip with the REAL pinned keypair green in host smoke).
      REMAINING (from-seed-dependent, daily-suite / warm-source work — NOT validatable
      per-PR with cold sources, the #207 deferral): the gate-412 `store-add-recursive` →
      `store-add-input-addressed` + `subst-export` swap, and the `ci/daily-full-suite.sh`
      post-build publisher call. These touch the heavy/exclusive spine → land as a follow-up.
- [ ] **3** Adopt the resolver in the real bootstrap toolchain gate(s) with from-seed
      fallback; surface the directive-1 relaxation in the gate header.

## Validation

The resolver + fallback validate in-session on a fixture (subst built from source offline,
crate FODs warm in /gnu/store — like gate 358). The literal ~90-min from-seed publish runs
in the DAILY suite (cold sources off the per-PR path; the #207-accepted deferral pattern).

## Verified-red evidence

### Sub-task 1 (2026-06-28; perturb resolve-toolchain.sh, revert, reconfirm green)
- **fall-back signal** (the resolver's most load-bearing property): `miss() … exit 0`
  instead of `exit 1` → the host smoke's MISS-cold leg flipped to `FAIL: resolver returned
  0 on a COLD store (should MISS)`. Reverted via `git checkout` (green committed first);
  reconfirmed `ALL SMOKE LEGS GREEN`. A miss MUST signal fall-back, not silently succeed.
- **StorePath==lock-path check** — INCONCLUSIVE as a sole-guard verified-red: disabling it
  (`x$fsp = x$fsp`) still MISSed the wrong-path leg, because `td-subst fetch` independently
  rejects a basename mismatch (the smoke's tamper changed the whole StorePath incl. the
  hash). So that check is DEFENSE-IN-DEPTH (catches a prefix-only tamper fetch would pass),
  not the sole guard. The signature + NarHash + basename guards are td-subst's, already
  verified-red in gate 358 (the wrong-key leg here re-exercises the signature guard).

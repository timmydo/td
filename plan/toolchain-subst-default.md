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
- [x] **2** `publish-toolchain-subst.sh` (#209) + the SWITCH-ON producer (branch
      worktree-toolchain-subst-switchon): gate 412 interns glibc-2.41 INPUT-ADDRESSED
      (store-add-input-addressed @ toolchain-key) and adds a real-bytes subst leg (subst-export
      → nar-restore → run the prebuilt program against the FETCHED libc in the own-root → 42;
      export persisted at .td-build-cache/toolchain-subst-export). `ci/daily-full-suite.sh`
      signs + publishes that export to the loop's substitute store on a green run (guarded by
      TD_SUBST_PRIVKEY/BIN). Validated: export→restore round-trip + gate-412 export→daily-publish
      →resolver-fetch chain (host smokes, real pinned key); gate 412's ~90-min from-seed run
      validates the REAL glibc-2.41 bytes end-to-end (in progress).
- [~] **3** Consumer-gate adoption: the resolver (#209) is landed + proven AND the daily suite
      now publishes the REAL toolchain, so any consumer calling `resolve-toolchain.sh` gets the
      toolchain by fetch. Wiring it into a specific downstream heavy gate (hello-corpus / rust /
      a fast check-rung path) as fetch-or-build is the remaining incremental step (each needs its
      own ~90-min validation) — follow-up.

## Validation

The resolver + fallback validate in-session on a fixture (subst built from source offline,
crate FODs warm in /gnu/store — like gate 358). The literal ~90-min from-seed publish runs
in the DAILY suite (cold sources off the per-PR path; the #207-accepted deferral pattern).

## Verified-red evidence

### In-sandbox gate GREEN (2026-06-28, `./check.sh toolchain-subst-default` RC=0)
All legs pass in the loop sandbox: td-built subst from source → publisher signs the
lock-keyed toolchain → resolver default-fetch RUNs the fetched-not-built binary
(RAN-FETCHED) → fall-back on a cold store → reds on a wrong key / wrong StorePath →
structural anchor. Two in-sandbox-ONLY gate-harness bugs caught + fixed (resolver logic
unaffected; host smoke already proved it): (1) `xargs $(GUIX) build` — a Makefile var copied
into a standalone shell script (→ `guix=${GUIX:-guix}`); (2) helper scripts invoked under
`env -i PATH="$cu/bin"` (coreutils has no `sh`) → added `shdir=$(dirname "$(command -v sh)")`
to the five scrubbed PATHs. LESSON (mirrors [[td-toolchain-input-addressed]]'s no-awk): a
host smoke with cargo binaries misses sandbox-PATH/make-var bugs — run the real gate.

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

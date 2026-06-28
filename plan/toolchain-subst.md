# toolchain-subst — working notes (claude-opus-b3b7ea, 2026-06-28)

Tasks **2b** (Register toolchain → td-subst + serve it) and **2c** (Consumer fetch) from
the post-#199 todo. Depends on **2a** ([[td-toolchain-input-addressed]], #204): the
toolchain now has a stable input-addressed key, so a consumer can NAME it before fetching.

## What already exists (don't rebuild)

- **gate 358 `td-subst`** proves the whole substitute protocol for CONTENT-addressed paths:
  `td-builder subst-export` (publish) → `subst sign` → `subst serve` → `subst fetch`
  (verify ed25519 sig + NarHash) → `nar-restore` byte-identical, plus the build-recipe
  CONSUMER HOOK (`TD_SUBST_URL` → `CACHE=subst`), with self-discrimination (tampered
  narinfo / corrupted nar / wrong key all red). It builds the subst binary from source.
- **2a** gives `td-builder toolchain-key`, `toolchain-path LOCK [NAME]`,
  `store-add-input-addressed`, and `tests/td-toolchain.lock`.

## The new bit (2b/2c): the lock-keyed substitute

Added as a leg INSIDE gate 358 (reuses its already-built subst binary `$ts` + keys), so no
new heavy build and no duplication:

1. Producer interns a real runnable fixture (a static bash from hello's pinned closure) at
   `P = /td/store/<key>-glibc-2.41` via `store-add-input-addressed glibc-2.41 $(toolchain-key
   tests/td-toolchain.lock) …`.
2. `subst-export P` → narinfo (StorePath = P, the logical /td/store path; bytes read flat
   from the physical store-dir) → `subst sign` → `subst serve` on loopback.
3. CONSUMER computes `Pc = toolchain-path tests/td-toolchain.lock glibc-2.41` from the LOCK
   ALONE; assert `Pc == P` (producer + consumer independently agree).
4. `subst fetch <basename Pc>` → verify sig + assert narinfo StorePath == Pc + NarHash →
   `nar-restore` → RUN the fetched binary (static, runs directly). A toolchain path obtained
   WITHOUT building it.
5. Self-discrimination: a wrong public key reds the fetch.

**Trust model (honest):** the toolchain is NOT byte-reproducible (cc1 stamp, ar/install
mtimes), so the substitute is trusted by the **ed25519 signature + the input-addressed
name**, NOT by repro-equality (358's repro leg / `td-builder check` byte-identity). Adding
repro-equality is **task 3** (byte-reproducible toolchain), tracked separately.

## Deferred (documented, not wired here): the literal toolchain bytes

The fixture stands in for a toolchain component — the input-addressed naming + subst path is
content-agnostic, so gcc/glibc flow through the IDENTICAL machinery. Wiring the LITERAL
bytes = swap gate 412's `store-add-recursive glibc-2.41 …` (and gcc-14.3.0/binutils-2.44) to
`store-add-input-addressed … $(toolchain-key …)` + a `subst-export` of the built tree. That
is a ~90-min from-seed build (gate 412), which runs in the DAILY heavy suite, not per-PR —
and this worktree has cold sources, so it can't be validated here. Left as the producer step
for whoever runs the daily suite / has warm sources. The mechanism is proven; only the
literal byte-source is swapped.

## Verified-red (observed on the host smoke before the gate run)

- **path-agreement**: a consumer with a perturbed lock copy computes a DIFFERENT path
  (`hz3jbcjw…-glibc-2.41` vs the producer's `hqw7304f…-glibc-2.41`) → the `Pc == P`
  assertion reds. The consumer can only name the producer's path with the SAME lock.
- **signature load-bearing**: a wrong public key reds the fetch (rejected); the right key
  fetch succeeds — so the self-discrimination guard catches acceptance.
- Host smoke (debug td-builder + host-built subst + a real bash-static fixture) ran the
  whole leg green: producer interns at /td/store/hqw7304f…-glibc-2.41, subst-export+sign+
  serve, consumer derives the same path from the lock, fetches by basename, StorePath
  matches, restore + run → `RAN-FETCHED`, wrong key rejected.

## Validation

Running the td-subst gate exercises the new leg (subst built from source offline — its crate
FODs are warm in /gnu/store). affected-checks maps mk/gates/358 → the `td-subst` target.

# x64-toolchain-subst — make x86_64 the canonical, SUBSTITUTED /td/store toolchain

Handle: claude-opus-cedce1

## Problem (verified 2026-06-28, main @ #212)

The substitute mechanism is real protocol-wise but feeds no real toolchain bytes: per-PR gate
358 serves a static-bash FIXTURE relabeled glibc-2.41; the only lock is i686; nothing is
interned/published anywhere; `ci/daily-full-suite.sh` never calls `publish-toolchain-subst.sh`
(orphaned) so `resolve-toolchain.sh` misses 100% and falls back to from-seed. The x86_64 cross
toolchain (#201) is content-addressed (path varies), has no lock, and isn't wired to the
resolver — so its ~90-min build is recomputed and discarded every run.

## Direction (human 2026-06-28)

x86_64 is the canonical /td/store toolchain (locked, published, fetched by default). i686 is
the bootstrap intermediate. The i686→x86_64 split stays at the **gcc 14 path** (build the i686
chain up through gcc 14.3.0, then cross with it — human OK'd; gate 414 / #201 already does this).
NOT an earlier gcc-4.x split.

## Ladder

- **PR1 (this PR) — x86_64 interned at a stable, fetchable path.** `tests/td-toolchain-x86_64.lock`
  (x86_64 sibling of the i686 lock) + gate 414 (`x86_64-cross-fns.sh`) now interns the REAL
  x86_64 glibc 2.41 at the lock-keyed input-addressed path and RUNS a dynamic x86_64 binary whose
  interp IS that path — a real capability + demo, distinct from the i686 paths. (Folded the thin
  standalone addressing gate back into this real gate per the substantial-PR steer.)
- **PR2 — real publisher + populated store.** Wire `publish-toolchain-subst.sh` into
  `ci/daily-full-suite.sh` (the orphan) + a persistent `~/.td/subst` store + a check.sh host-prep
  so the store is actually populated.
- **PR3 — consume by default.** Wire `resolve-toolchain.sh` into the x86_64 toolchain-consuming
  gates (fetch by default, fall back to from-seed on miss).
- **PR4 — x86_64 userland + i686 demotion.** Build the corpus userland (hello/sed/…) x86_64;
  stop publishing/consuming the i686 final toolchain — keep it only as the cross intermediate.

## Verified-red

- Gate 414 `[input-addressed]`: break the lock↔interned-path equality, or make the x86_64 path
  equal the i686 path → the leg reds. (Run from the seed; heavy.)

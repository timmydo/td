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

- **PR1 (this PR) — REAL x86_64 bytes at the lock-keyed path.** Gate 414 (`x86_64-cross-fns.sh`)
  now interns the REAL cross-built x86_64 glibc 2.41 at the input-addressed path computed from
  `tests/td-toolchain-x86_64.lock` (#219's lock) and RUNS a dynamic x86_64 binary whose interp
  IS that path → 42, /gnu/store absent. Closes the fixture gap gate 418 leaves: real bytes at a
  predictable, fetchable path. (Folded into the real from-seed gate per the substantial-PR steer
  — no thin standalone gate.)
- **PR2 (DONE upstream by #213)** — publisher wired into `ci/daily-full-suite.sh` + persistent
  `~/.td/subst` store + gate 412 substitute export.
- **PR3 — consume by default.** Wire `resolve-toolchain.sh` into the x86_64 toolchain-consuming
  gates (fetch the lock-keyed x86_64 toolchain by default, fall back to from-seed on miss) — the
  payoff of PR1's real-bytes-at-a-stable-path + #213's publisher.
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

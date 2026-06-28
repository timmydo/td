# toolchain-x86_64-input-addressed — working notes

Handle: claude-opus-5cd532 · 2026-06-28 · the x86_64 parallel of #204 (toolchain-input-addressed,
i686). First brick of "wire the x86_64 toolchain as a fetchable substitute" so the rust
compile/userland rungs ([[rust-store-native]] rungs 3/4) FETCH the x86_64 toolchain instead of
the ~90-min from-seed rebuild.

## What

`tests/td-toolchain-x86_64.lock` + gate `toolchain-x86_64-input-addressed` (mk/gates/418).
The x86_64 toolchain (cross binutils-2.44 + cross gcc-14.3.0 + x86_64 glibc-2.41 + libgcc_s,
built from the seed by gate 416 via `tests/x86_64-cross-fns.sh`, #201) is not byte-reproducible,
so `store-add-recursive`'s content-addressed path varies build-to-build. The lock + `td-builder
toolchain-key/toolchain-path` give a stable INPUT-ADDRESSED path (a pure function of the declared
inputs) the subst consumer can name before fetching — exactly #204, for x86_64.

Key insight: the x86_64 toolchain consumes the **same** pinned source set as i686 (the cross is a
BUILD configuration `--target=x86_64-pc-linux-gnu` over identical sources; the x86_64 UAPI headers
derive from the already-pinned `linux-4.14.67`). So the lock mirrors `tests/td-toolchain.lock`'s 24
sources + 7 patches verbatim, and ARCH is the key discriminator: distinct `name` +
x86_64 `component` names re-key it (both feed `key()`), giving a distinct `/td/store` path with
zero source duplication. Verified: x86_64 key `18b77f35…` ≠ i686 `44c83d09…`.

## Legs (all durable, td-native, no guix oracle)

pinned-sync · arch-parity (shares i686's exact source set; only name/recipe-rev/component differ) ·
distinct-key (arch re-keys → no i686 collision) · stable-key (deterministic distinct paths) ·
load-bearing (recipe-rev + a pin move the addressing) · behavioral (a real binary at the
x86_64-keyed path runs in the store-ns own-root, /gnu/store absent). Diff/cmp-free (sandbox has
neither — sha256 compares + grep/sed directive-kind checks).

## Verified-red (2026-06-28, committed green first per [[td-commit-before-red-variants]])

- **[distinct-key]** — rewrote the lock's `name`+`component`s to the i686 names (removed the arch
  discriminator) → lock byte-identical to i686 → key collided (`44c83d09…`) → gate RED:
  `FAIL: [distinct-key] x86_64 key collides with i686 … arch did not re-key` (arch-parity still
  PASSED, isolating the leg). The discriminator is load-bearing.
- **[arch-parity]** — dropped one input (`gawk-3.1.8`) → pinned-sync still PASSED (23 valid pins ≥ 20)
  but RED: `FAIL: [arch-parity] x86_64 input/patch set differs from i686`. arch-parity catches a
  source-set divergence pinned-sync alone misses.

Restored green after each (`git checkout tests/td-toolchain-x86_64.lock`).

## Next (the substitute payoff — follow-on PRs)

- Producer: gate 416 (or a daily-suite producer) interns the built x86_64 components at the
  input-addressed paths (`store-add-input-addressed` with this lock's key) + `subst-export` (the
  #207 machinery), so the bytes are published once.
- Consumer: the rust compile/userland rungs (and a re-iterating runtime gate) compute the x86_64
  toolchain paths from this lock and FETCH (with from-seed fallback for the authoritative loop —
  directive 1) instead of rebuilding. See plan/td-subst.md (#207, the i686 parallel).

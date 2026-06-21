# daemon-td-drv — the daemon realizes a td-assembled .drv (the td-artifact bridge, step 1)

Handle: claude-opus-3267ea — started 2026-06-20. Implements the human-chosen bridge
mechanism (approach A', 2026-06-20): to ship a td-built artifact in the daemon-built system
image, the daemon must be able to realize it. See memory `td-rust-focus-distro-direction`.

## The problem (proven)

td-built outputs live in td's OWN store (build_and_register writes td's store-db); the
system image is daemon-built. The td-placed stage0 td-builder is NOT daemon-valid (confirmed
via `guix gc --references`), so the daemon can't build a .drv whose builder is the stage0.
The vendored crates ARE daemon-valid (fixed-output fetches).

## The mechanism (PROVEN with hello)

1. **Assemble with a daemon-valid builder** — run `td-builder build-recipe` WITHOUT the stage0
   override, so the .drv's builder is the GUIX-built td-builder (daemon-registered). All
   input-srcs (crates/source/builder) are then daemon-valid.
2. **Instantiate into the daemon store** — a Guile helper reads the .drv, extracts its
   store-ROOT references (`/gnu/store/<32hash>-<name>`, NOT sub-paths like `…/bin/td-builder`
   — that was the first-try bug), `add-text-to-store`s the .drv with those refs.
3. **Daemon builds it** — `build-derivations` → the daemon runs td-builder in ITS sandbox
   (autotools/rust phases run), output registered in `/gnu/store`.

Proof: the daemon built td's hello .drv → output at `qw9f…-hello-2.12.2` (the SAME path td's
own daemon-free realize produced — the .drv is deterministic regardless of realizer) → the
binary runs ("Hello, world!"). So td-builder works as a daemon builder.

## Increment 1 (this PR) — the bridge capability + gate

- Reusable Guile helper (`tests/td-daemon-instantiate.scm` or similar): instantiate a
  td-assembled .drv into the daemon store + build it; return the (now daemon-valid) output.
- Gate `daemon-td-drv` (heavy): td assembles td's COREUTILS .drv with the guix-built builder,
  the helper instantiates + the daemon realizes it → daemon-valid 79-util multicall;
  behavioral (mkdir/cp/cat/ls/mv/rm) + DURABLE distinct-path (≠ guix's coreutils) + the
  daemon-built path equals td's daemon-free realize (own-then-diverge consistency).

## Increment 2 (follow-up) — ship it

`SystemSpec.tdPackages: ["uutils"]` (TS) → `<td-config>` `td-packages` → `td-config->
operating-system` instantiates td's coreutils .drv (the helper) and folds the output into the
profile. Re-baseline the `system/td.scm` oracle + the ts-diff/typed differential. The booted
PATH then has td-built `coreutils`. Reusable for every Rust tool td builds.

## Verified-red

- (to fill) gate: perturb the builder (use the stage0, daemon-invalid) → daemon build fails
  ("not in the store"); or break a multicall assertion.

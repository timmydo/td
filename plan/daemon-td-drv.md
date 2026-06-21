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

## Result (green)

Gate `daemon-td-drv` PASSES: td assembled uu_cat's .drv with the guix-built (daemon-valid)
td-builder, the helper instantiated it into the daemon store, and the DAEMON realized it →
daemon-VALID `cat` at `/gnu/store/qwjwj…-cat-0.9.0` — the SAME path td's own daemon-free
realize produced (the .drv is realizer-independent) — that round-trips file + stdin.

## guix-surface (directive 3 — called out for sign-off)

+1 packager site: `mk/gates/358-daemon-td-drv.mk (system td-builder) td-builder` (12→13).
The daemon needs a daemon-VALID builder; the stage0 isn't, the guix-built td-builder is — so
A' inherently re-uses it as the BUILDER SEED (retired when td has its own builder daemon).
ts-eval uses `load_ts_eval` (td's own — no site). Re-baselined `tests/guix-surface.expected`.

## Verified-red (confirmed)

- **Daemon-valid builder is load-bearing**: running the helper on the cached STAGE0-builder
  .drv (builder `j30c…-td-builder`, NOT daemon-valid) → the daemon build FAILS (`build of …
  uutils-0.9.0.drv failed`). So the bridge genuinely depends on the guix-built daemon-valid
  builder; the gate's pass is not vacuous.
- **realizer-independence is load-bearing**: the gate asserts the daemon-built path EQUALS
  td's daemon-free path — if td's .drv assembly diverged from guix's path algorithm, the
  daemon would compute a different output and the equality reds.

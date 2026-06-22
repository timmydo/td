# source-bootstrap — td's toolchain from source at /td/store, no guix bytes ever

Handle: claude-fable-db65ca · branch: td-native-store (PR: decision + native build engine)

## Decision (human, 2026-06-21)

> "source bootstrap first, no guix seed ever."

North star sharpened to **no guix *bytes*** (not just no guix process). A guix-captured
seed — even static — fails it: a static `bash` embeds 11 `/gnu/store` strings (glibc
locale/gconv/zoneinfo, bash's own `sh`/bashdb, a bare `/gnu/store`) and its provenance is
guix. A `/gnu/store→/td/store` byte rewrite (store-relocate, #140) only **relabels** guix
bytes — it does not make them td's. So the guix seed (frozen tarball OR relocated) is
**rejected as the foundation**. td's toolchain is built **from source at `/td/store`**.

This **supersedes** the relocated-seed Phases 2–3 of [[user-pm]]: store-relocate (#140) is
demoted from "the break" to at most a removable differential oracle. The native build path
(Phase 1/3) survives — it's the engine this track builds *on*.

## What's already landed (the enabler, this branch)

- **Native `/td/store` build path.** `td-builder build` (and `build-recipe`) stage inputs
  and set `NIX_STORE` at the active `store::store_dir()` (`/td/store` under `TD_STORE_DIR`),
  and the output is content-addressed at that prefix (`make_store_path_in`, Phase 1). So a
  `TD_STORE_DIR=/td/store` build is **native** — re-hashed at `/td/store`, NO post-hoc
  rewrite. (`builder/src/sandbox.rs`: `store_prefix()`, `store_path_name_in`; unit test
  `store_path_name_honors_the_active_prefix`. Validated e2e locally with a stand-in static
  builder; the guix-seed e2e gate was dropped — no non-guix static binary exists yet, which
  is exactly what brick 1 creates.)
- **stage0-builder flock.** Serialized stage0 build+place so concurrent gates sharing a
  `TD_STAGE0_BASE` don't collide ("File exists"). The bootstrap's own stage0 reuses this.

## The bootstrap is a PORT, not research

The bottom of the chain is auditable and reproducible — guix ships exactly this as its
"Full-Source Bootstrap"; live-bootstrap and stage0-posix are the upstream sources. We
vendor/pin those sources, build each stage from source at `/td/store`, and diff against the
guix oracle (same source both ways) until the oracle is retired.

## Brick ladder (each brick: one stage, from source, native at /td/store, verified-red)

The irreducible seed is a tiny hand-auditable binary (stage0-posix `hex0`, a few hundred
bytes) — NOT guix-built. Vendor it + the stage sources into the repo (offline loop), build
upward:

0. **seed + harness** — ✅ DONE (2026-06-22, kaem port). Vendored stage0-posix-x86 `3b9c2bb`'s
   229-byte `hex0-seed` + 618-byte `kaem-optional-seed` + hex sources + the seed kaem script
   into `seed/stage0/`. The `bootstrap-seed` gate (`mk/gates/360`) runs `kaem-optional-seed`
   over `mescc-tools-seed-kaem.kaem` with guix/Guile off env → a full `hex0` + `kaem-0`,
   ALL-DURABLE: seeds match pins (no-guix/auditable), self-reproduce from their hex source,
   the built hex0 works as an assembler, and two runs are byte-identical. (`/td/store` placement
   is deferred to the bricks that produce the dynamic toolchain; the stage0 assemblers are
   static, no store paths.) Next: brick 1 drives `kaem-0` over the rest of the chain.
1. **stage0-posix → M2** — `hex0`→`hex1`→`hex2`→`M0`→`cc_*`/`M2-Planet`: a minimal C
   compiler, all at `/td/store`.
2. **mes + mescc-tools** — GNU Mes (Scheme) + `mescc` build a richer C environment.
3. **tinycc** — bootstrap TinyCC from Mes; the first self-respecting C compiler.
4. **gcc (old) → gcc (modern)** — staged gcc builds, `--prefix=/td/store`.
5. **glibc + binutils** — the C library + linker/assembler, native `/td/store` RUNPATH.
6. **coreutils / bash / make / sed / grep / tar / gzip / …** — the build userland td's
   recipes already assume, now from the `/td/store` source toolchain.
7. **retire the guix seed** — the corpus locks (`hello-no-guix.lock`, …) point at the
   `/td/store` toolchain; the guix toolchain seed is removed from every build's inputs;
   guix remains only as the removable `guix build --check` oracle (retired last, §5).

## Durable vs oracle

Each brick carries DURABLE assertions (the stage binary RUNS and builds the next stage; its
output is native `/td/store`, reproducible under `td-builder check`; NO `/gnu/store` byte in
it) and may carry a REMOVABLE guix oracle (the same source built by guix produces an
equivalent tree). The oracle is deleted when guix is retired; the durable legs are the keep.

## Verified-red

- Native build engine (this branch): revert the `NIX_STORE`→`store_dir()` wiring →
  the build sees `NIX_STORE=/gnu/store` → the "ran at /td/store" leg reds. (Seen locally.)

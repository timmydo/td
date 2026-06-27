# store-native-userspace — working notes

Handle: claude-opus-5cd532 · claimed 2026-06-27 · section: side (parallel-safe)

## Goal

A usable userspace assembled WITHOUT the guix `operating-system` (`system/td.scm`):
`td-builder profile` unions `/td/store`-native tools into a symlink-tree profile, run in a
`store-ns` own-root with `/gnu/store` ABSENT. The td-native replacement that lets the `.scm`
system path retire — and the destination the rust userland ([[td-rust-store-native-track]])
slots into once `glibc-final` unblocks running rust binaries from `/td/store`.

Why this is the right "until glibc-final" work: it is the ONE rung of the rust-userspace
ladder that is NOT glibc-2.17-gated — it uses the `/td/store`-NATIVE **C** userland the gcc
agent's GCC 14.3.0 already builds (C binaries run fine on the existing `/td/store` glibc
2.16.0). Non-colliding: consumes the gcc lane's toolchain output; touches no spine file.

## Ladder

1. **profile --store-native** (DONE, inc 1) — `td-builder profile --store-native PROFILE PKG…`
   reads the PHYSICAL package dirs but points the symlinks at the LOGICAL store paths
   (`store::store_dir()/<basename>/bin/…`), so the profile resolves inside a `store-ns`
   own-root where `/td/store` is the bound store and the physical scratch dir is absent.
   Unit-tested (logical vs physical targets, collision still rejected). Fixed a latent bug:
   the collision check used `exists()` (follows the dangling logical link) → now
   `symlink_metadata()` (lexists). builder/src → validates on check-engine.
2. **own-root profile gate** (DONE, inc 2) — `mk/gates/412-store-native-profile.mk` +
   `tests/store-native-profile.sh`: `profile --store-native` builds a profile whose entries
   are LOGICAL /td/store symlinks that RESOLVE + RUN in a `store-ns` own-root, /gnu/store
   ABSENT (bash 5.2.37 via /td/store/profile/bin). GATE GREEN; verified-red (drop
   --store-native → physical links → structural leg reds). The tool is bash-static (the cheap
   store-ns runner pattern); this proves the ASSEMBLY mechanism end-to-end. NOTE: the full
   guix-FREE multi-tool userspace gate (below) needs the ~40-min chain (no shared chain lib →
   it would duplicate the gcc lane's ~700 lines, low marginal value over #192/#197 which
   already prove guix-free C tools build+run from /td/store) — DEFERRED until td-subst
   chain-caching makes /td/store tools cheap to consume, or it's worth the duplication.
   Gotcha: nested backticks (outer `out=\`…\`` + inner `\`command -v\``) mangle → use a probe
   FILE in the store (bound in the own-root); the loop sandbox has no awk.
3. **guix-free userland tools** (DEFERRED, heavy) — model on `bootstrap-hello-userland` (#192): the
   `/td/store` toolchain compiles ≥2 small programs from source (build-wrapper bakes the
   `/td/store` interp+RUNPATH), interns them at `/td/store`, `profile --store-native` unions
   them, and `store-ns` runs `profile/bin/*` together → each prints/returns its result with
   `/gnu/store` ABSENT. Durable legs: behavioral (the profiled tools run from `/td/store`),
   structural (profile entries are logical `/td/store` symlinks; `/gnu/store` absent),
   no-guix (built + assembled with no guix process / no `/gnu/store` bytes). Heavy (~40-min
   chain build from the seed) — iterate with the cached-chain dev harness.
4. **rust joins** (post-glibc-final) — the relinked `/td/store` rust tools
   ([[td-rust-store-native-track]] rungs 1–3) are added to the same `profile --store-native`
   + own-root → a usable Rust userspace, `/gnu/store` absent, no `.scm`.

## Verified-red plan

- inc 1: a collision across two packages must still error (tested: `profile_rejects_collision`).
- inc 2 — DONE 2026-06-27: dropped `--store-native` (physical links) and re-ran the gate →
  it REDDENED at the structural leg: `FAIL: profile/bin/bash is not a logical /td/store link
  (got: /tmp/…/td-store/…-bash-static/bin/bash)`. Restored the green script (committed first,
  per [[td-commit-before-red-variants]]). The `--store-native` mode is load-bearing.

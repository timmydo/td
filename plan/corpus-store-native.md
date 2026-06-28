# corpus-store-native — working notes

Goal (task 4 of the post-glibc-2.41 plan): build MORE real corpus packages with td's OWN /td/store
toolchain, each a leaf gate, extending the BRICK 8 pattern past GNU hello. This drives the guix
gcc-toolchain out of the corpus baseline — the userland is built by td's toolchain, not guix's.

## Approach — the converged BRICK 8 / toolchain-subst engine path (NOT the mesboot wrapper)

The human chose (2026-06-28) the engine approach over the gate-404 hello-userland mesboot wrapper:
build the corpus package via `td-builder build-recipe` with the MODERN /td/store toolchain
substituted for guix's gcc-toolchain. The template is `tests/bootstrap-hello-corpus-store-native.sh`
(gate 414, [[toolchain-subst]]). Each package gate:

1. Build the full modern toolchain from the seed (chain → gcc 4.9.4 → gcc 14.3.0 + binutils 2.44 →
   glibc 2.41). This block is copied VERBATIM from the hello-corpus gate (not a new failure surface).
2. Re-check the toolchain: link + run a C and a C++ program → 42 (durable toolchain leg).
3. Assemble a guix-gcc-toolchain-shaped /td/store toolchain: gcc/g++ WRAPPER (--sysroot glibc 2.41,
   interp/RUNPATH baked, link flags only when linking) + binutils 2.44 + ar/ranlib LD_LIBRARY_PATH
   wrappers; rewrite every dynamic bin's PT_INTERP → glibc 2.41 via `td-builder elf-set-interp`.
   Intern it at /td/store (store-add-recursive).
4. Substitute the `-gcc-toolchain-` line in the package's `tests/<pkg>-no-guix.lock` with the
   /td/store toolchain (+ glibc-2.41); realize the rest of the lock's guix seed closure (the build
   env: bash/coreutils/make/sed/grep/gawk/tar/gzip/…) via warm-seed.
5. `td-builder build-recipe <pkg>.json <newlock> …` with TD_EXTRA_DBS (closure_multi) — builds the
   package at the guix corpus version using the existing `recipe-<pkg>.ts`.
6. Verify: (a) interp = /td/store glibc 2.41; (b) [no-guix-toolchain] no ref to the substituted-out
   guix gcc-toolchain; (c) behavioral: the binary runs in a store-ns own-root, /gnu/store ABSENT.

## Ladder

- [x] **Inc 1 — GNU sed 4.9** (this PR): gate 416 `bootstrap-sed-corpus-store-native`, leaf
  affected-checks case. Reuses recipe-sed.ts (already authored, proven by corpus-no-guix) +
  sed-no-guix.lock (already pinned; carries sed-source + the gcc-toolchain to substitute). NO new
  seed lock, NO engine change. Behavioral: own-root sed transforms `foo`→`bar` (a real substitution).
- [ ] grep / tar / gzip / make / coreutils / bash — same pattern, one leaf gate each (split across
  agents). Each already has a recipe-<pkg>.ts + <pkg>-no-guix.lock (the corpus-no-guix set), so each
  is a thin copy of this gate with the package swapped.

## Why sed 4.9 (not the mesboot 4.2.2 wrapper version)

The earlier mesboot-wrapper attempt (gate-404 pattern, gcc 4.6.4 + glibc 2.16, sed 4.2.2) was
scaffolding — discarded after the human picked the engine approach. The engine path builds the EXACT
guix corpus version (sed 4.9, matching sed-no-guix.lock) so it genuinely SUBSTITUTES guix's
gcc-toolchain in the corpus build (task 4's "Why"). sed 4.9 (2022) is the same era as hello 2.12.2,
so its gettext `po/` uses `SHELL=@SHELL@` (no hardcoded /bin/sh) and the engine build env is a full
guix seed closure (real bash/make/…), so the "no /bin/sh in the sandbox" class that bit the mesboot
wrapper does not recur.

## Verified-red (CONFIRMED via the cached-toolchain harness, 2026-06-28)

- **behavioral leg**: break the own-root substitution (`s/zzz/bar/` so it can't match `foo`) → the
  own-root sed output is just `foo` (no `bar`) → `FAIL: brick8: sed did not substitute foo->bar from
  /td/store: foo` (exit 1). The substitution leg is load-bearing.
- The interp + no-guix-toolchain legs are the brick-8 template's, already exercised by the hello gate.

Both green and red were run on a cached modern toolchain (the harness reuses `seddev2/chain.env`), so
each brick-8 iteration is ~2 min instead of a ~90-min from-seed toolchain rebuild.

## check-rung vs check.sh — the brick-8 environment gaps (dev-harness only)

The brick-8 engine path needs three things the real `./check.sh` provides but `tools/check-rung.sh`
does not, so a bare `check-rung` harness can't run brick8 (these are NOT gate bugs):

1. **awk** — not in the declared loop toolchain; it reaches the gate only via check.sh's
   `$hostguix_dir` on PATH. FIXED in the gate itself: the two gcc-toolchain lock rewrites now use
   `grep`/`sed` (declared toolchain) — hermetic, and the harness no longer needs awk.
2. **guix** — `xargs guix build` (realize the guix build-env seed closure) needs the `guix` binary,
   on PATH via `$hostguix_dir` under check.sh. The dev harness adds it (`run-guix.sh` mirrors check.sh's
   `PATH=$hostguix_dir:$toolchain`). Inherent to the brick-8 pattern (the build env stays guix, §5).
3. **td-ts-eval sentinel** — written by the `build-recipes` prelude; `./check.sh <single-heavy-target>`
   does NOT trigger build-recipes. Warm it first (run `./check.sh build-recipes`, which writes
   `.td-build-cache/rust-ts-eval/tseval-path`), then the gate's `load_ts_eval` finds it. Same
   requirement as the hello-corpus gate. The authoritative run is `./check.sh build-recipes
   bootstrap-sed-corpus-store-native` (or build-recipes once, then the gate).

## Notes / gotchas

- i686 (the /td/store toolchain is i686; the x86_64 lift is a separate track). Corpus C tools are
  fine as i686.
- Heavy from-seed gate (~90 min — it builds the whole modern toolchain). The toolchain block is the
  slow part and is the proven hello-corpus block; only the corpus package step differs.
- No new seed/sources lock: the sed source comes from sed-no-guix.lock's `sed-source` (guix-realized
  in the seed closure), exactly as the hello-corpus gate uses hello's source.

# corpus-store-native — working notes

Goal (task 4 of the post-glibc-2.41 plan): build MORE real corpus packages with td's OWN /td/store
toolchain, each a leaf gate, extending the BRICK 8 pattern past GNU hello. This drives the guix
gcc-toolchain out of the corpus baseline — the userland is built by td's toolchain, not guix's.

## Shared chain library (dedup, 2026-06-28 — human-directed)

The ~850-line seed→…→gcc-14.3.0+binutils-2.44+glibc-2.41 chain was copy-pasted across 10
bootstrap-*-store-native gates. Human asked to stop duplicating it. The merged toolchain-subst work
(#204/#207/#209/#213) makes the toolchain *fetchable* (tools/resolve-toolchain.sh + the input-addressed
td-toolchain.lock + a daily publisher) but does NOT yet let a corpus gate be thin: only `glibc-2.41` is
published as a substitute (the last rung; gcc-14/binutils-2.44 are not), the substitute store is
populated only by the daily suite (per-PR/local MISSes → from-seed), and there is no shared from-seed
builder. So the fix is to EXTRACT the chain: **`tests/bootstrap-chain.sh`** now holds the chain VERBATIM
(helpers + every `build_*` function + a `bootstrap_modern_toolchain` orchestrator that builds + verifies
the toolchain and sets the globals GCC14/GLIBC241/BMB244SB/CC1/cpath/KH_TB). The sed gate sources it and
drops from 1163 → ~185 lines. Pure code move (behavior-preserving). The other 9 gates can migrate to the
lib later (each needs its own from-seed re-validation; tracked as follow-up). When ALL toolchain
components become substitutable, the lib's `bootstrap_modern_toolchain` becomes the single place to add
a fetch-by-default + from-seed-fallback.

## Approach — the converged BRICK 8 / toolchain-subst engine path (NOT the mesboot wrapper)

The human chose (2026-06-28) the engine approach over the gate-404 hello-userland mesboot wrapper:
build the corpus package via `td-builder build-recipe` with the MODERN /td/store toolchain
substituted for guix's gcc-toolchain. The template is `tests/bootstrap-hello-corpus-store-native.sh`
(gate 414, [[toolchain-subst]]). Each package gate:

1. `bootstrap_modern_toolchain` (from `tests/bootstrap-chain.sh`) builds the full modern toolchain from
   the seed (chain → gcc 4.9.4 → gcc 14.3.0 + binutils 2.44 → glibc 2.41). Shared lib; not re-inlined.
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

## From-seed root cause + fix (2026-06-29) — a real pre-existing bug, also hits hello-corpus

The from-seed `./check.sh` run failed for a long time with `configure: error: C compiler cannot
create executables` in build-recipe, while the cached-toolchain harness always passed. Ruled out
contention (failed on a quiet box), PATH, MAKEFLAGS (all rungs clear it), TMPDIR-location, and
`/td/store` pollution. A faithful link diagnostic (reproduce the assembled-toolchain link in a
store-ns own-root with `-v`) pinned it: **the make-built `ld` kept a build-dir PT_INTERP**
(`/tmp/tmp.XXX/glibcsharedbuild/out/lib/ld-linux.so.2`) instead of `/td/store/…-glibc-2.41/…`.

Cause: brick8's `elf-set-interp` rewrites PT_INTERP **in place, shrink-or-equal** (td's `elf.rs`
has no patchelf-style grow). The toolchain binaries' build-time interp lives under `$TMPDIR`. Under
**check.sh's default `TMPDIR=/tmp`** the build interp is 58 chars — **shorter** than the `/td/store`
target (71) — so the in-place rewrite silently no-ops (`|| true`) and `ld`/`as` keep a dead
build-dir interp that doesn't exist in build-recipe's `/td/store`-only pivot sandbox → ld can't
start → link fails. **check-rung used a long worktree `TMPDIR`**, so its build interp was long
enough — which is exactly why it always worked there and the bug hid. This hits the landed
`bootstrap-hello-corpus-store-native` gate **identically** (same chain + check.sh); it just hasn't
been run from-seed via check.sh yet (it landed dev-green; the daily suite hasn't run).

Fix (in `tests/bootstrap-chain.sh`): `bootstrap_modern_toolchain` pins a deliberately-long `TMPDIR`
under `.td-build-cache` + a hard length assertion (≥75) so the in-place rewrite always fits and can
never silently regress. **Validated from seed** via `./check.sh bootstrap-sed-corpus-store-native`:
build-recipe builds sed 4.9, interp=`/td/store/…-glibc-2.41/ld-linux.so.2`, no guix gcc-toolchain
ref, sed runs `foo→bar`, /gnu/store ABSENT — PASS. Proper long-term fix: grow support in `elf.rs`
(PT_INTERP grow), which would remove the TMPDIR-length dependency entirely (follow-up).

## Notes / gotchas

- i686 (the /td/store toolchain is i686; the x86_64 lift is a separate track). Corpus C tools are
  fine as i686.
- Heavy from-seed gate (~90 min — it builds the whole modern toolchain). The toolchain block is the
  slow part and is the proven hello-corpus block; only the corpus package step differs.
- No new seed/sources lock: the sed source comes from sed-no-guix.lock's `sed-source` (guix-realized
  in the seed closure), exactly as the hello-corpus gate uses hello's source.

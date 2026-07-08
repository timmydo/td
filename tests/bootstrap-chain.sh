#!/bin/sh
# tests/bootstrap-chain.sh — the SHARED from-seed modern-toolchain driver. #397 retired the
# last per-rung bootstrap-*.sh consumers (their build_* ladders duplicated this driver); the
# surviving consumers are tests/chain-cache.sh, recipe-checks-daily, and tests/x86_64-cross-fns.sh (via
# tests/ladder-lib.sh / tests/chain-cache-lib.sh). The caller sets `set -eu` + ROOT=$(pwd),
# runs load_stage0 ($TB + TD_BUILDER_*), sources this, then calls `bootstrap_modern_toolchain`.
#
# #378 slices 2+3: the ~850-line build_* shell LADDER IS DELETED. Every rung — stage0 →
# mes → tcc → make/patch → binutils/gcc-2.95 → the mesboot ladder → gcc-4.9.4 →
# gcc-14.3.0 + binutils-2.44 + glibc-2.41 — is a RECIPE (recipes/src/recipes/*.rs, typed
# steps) built by the ENGINE: ONE `td-builder build-plan --auto glibc-241` realizes the
# whole 20-rung graph, chaining each rung on the prior rungs' outputs (native-inputs /
# td-recipe-output edges), with every rung's lock SYNTHESIZED from its recipe JSON (#429 —
# no hand-written lock). tests/ladder-lib.sh is the only shell left: interning the pinned
# sources/patches/seed (content-addressed), resolving the DECLARED host tools, and driving
# the engine — plumbing, not build logic.
#
# Warm reuse (#317): the ladder workdir is machine-wide under TD_CHECK_CHAIN_CACHE's
# regime (Shared gates get a warm dir; Private/empty = cold fresh) — but the CACHE is the
# engine's own per-step cached_realization, NAR-verified per reuse and keyed by each
# rung's drv (recipe JSON + lock + builder-of-record), so a recipe OR ENGINE change
# re-keys exactly the affected rungs — stronger than the old file-keyed brick cache (and
# the retired TMPDIR/interp-length hack: store paths are stable inside sandboxes, so no
# brick ever bakes a movable path). flock serializes concurrent gates on the warm dir;
# the loser cache-hits through.
#
# Exports for the gate tails:
#   GCC14         = the gcc 14.3.0 stage prefix (…/stage/td/store/gcc-14.3.0, host dir)
#   GLIBC241      = the glibc 2.41 stage prefix (ld-scripts relocated, kernel headers in)
#   BMB244SB      = the binutils 2.44 output dir (bin/ has as/ld/readelf — DYNAMIC vs the
#                   shared glibc 2.16; they run inside store-ns/sandboxes, not host-side)
#   CC1           = gcc 14's cc1 (host path, for the [no-guix] byte scan)
#   LADDER_TDSTORE= the plan's shared td-store (canonical-named outputs — `cp -a
#                   "$LADDER_TDSTORE/$base"` stages a rung into a verify store WITHOUT
#                   re-hashing, so baked canonical references keep resolving)
#   GLSHARED_BASE / BU244_BASE / GCC14_BASE / GLIBC241_BASE = those outputs' canonical
#                   store basenames (verify-store staging + interp assertions).

fail() { echo "FAIL: $*" >&2; exit 1; }
sha() { sha256sum "$1" | cut -d' ' -f1; }
lf() {
  _want="$2"
  while IFS=' ' read -r _key _rest; do
    [ "$_key" = "$_want" ] || continue
    printf '%s\n' "$_rest"
    return 0
  done < "$1"
  return 1
}

bootstrap_modern_toolchain() {
# --- [pinned-input] every chain tarball + vendored boot patch matches its sha256 pin -----------
MES_LOCK=`ls seed/sources/mes-*.lock | head -1`;       NYACC_LOCK=`ls seed/sources/nyacc-*.lock | head -1`
TCC_LOCK=`ls seed/sources/tcc-0.9.26*.lock | head -1`; MAKE_LOCK=`ls seed/sources/make-3.80.lock`
PATCH_LOCK=`ls seed/sources/patch-*.lock | head -1`;   BU_LOCK=`ls seed/sources/binutils-2.20.1a*.lock | head -1`
GCC_LOCK=`ls seed/sources/gcc-core-2.95*.lock | head -1`
GLIBC_LOCK=`ls seed/sources/glibc-2.2.5*.lock | head -1`; LINUX_LOCK=`ls seed/sources/linux-*.lock | head -1`
MAKE382_LOCK=`ls seed/sources/make-3.82.lock`; GCC464_LOCK=`ls seed/sources/gcc-core-4.6.4.lock`
GPP464_LOCK=`ls seed/sources/gcc-g++-4.6.4.lock`; GMP_LOCK=`ls seed/sources/gmp-*.lock`
MPFR_LOCK=`ls seed/sources/mpfr-2.4.2.lock`; MPC_LOCK=`ls seed/sources/mpc-1.0.3.lock`
GAWK_LOCK=`ls seed/sources/gawk-*.lock`; GLIBC216_LOCK=`ls seed/sources/glibc-mesboot-2.16.0.lock`
GCC494_LOCK=`ls seed/sources/gcc-4.9.4.lock`; GCC14_LOCK=`ls seed/sources/gcc-14.3.0.lock`
GMP63_LOCK=`ls seed/sources/gcc14-gmp-*.lock`; MPFR421_LOCK=`ls seed/sources/gcc14-mpfr-*.lock`
MPC131_LOCK=`ls seed/sources/gcc14-mpc-*.lock`; BU244_LOCK=`ls seed/sources/binutils-2.44.lock`
GLIBC241_LOCK=`ls seed/sources/glibc-2.41.lock`
for l in "$MES_LOCK" "$NYACC_LOCK" "$TCC_LOCK" "$MAKE_LOCK" "$PATCH_LOCK" "$BU_LOCK" "$GCC_LOCK" \
         "$GLIBC_LOCK" "$LINUX_LOCK" "$MAKE382_LOCK" "$GCC464_LOCK" "$GPP464_LOCK" "$GMP_LOCK" \
         "$MPFR_LOCK" "$MPC_LOCK" "$GAWK_LOCK" "$GLIBC216_LOCK" "$GCC494_LOCK" "$GCC14_LOCK" \
         "$GMP63_LOCK" "$MPFR421_LOCK" "$MPC131_LOCK" "$BU244_LOCK" "$GLIBC241_LOCK"; do
  test -n "$l" || fail "missing a seed/sources/*.lock"
  f=".td-build-cache/sources/`lf "$l" file`"
  test -f "$f" || fail "pinned tarball not warm ($f) — run 'td-feed warm sources'"
  test "`sha "$f"`" = "`lf "$l" sha256`" || fail "warmed $f sha256 != lock pin"
done
_kh_file=`lf "$LINUX_LOCK" file`
KH_VER=${_kh_file#linux-}
KH_VER=${KH_VER%%.tar*}
KH_TB=".td-build-cache/sources/linux-headers-$KH_VER-i386.tar.gz"
test -f "$KH_TB" || fail "kernel-headers tarball not warm ($KH_TB) — run 'td-feed warm sources'"
for pp in binutils-boot-2.20.1a:f6be78a06f2c9905e019ade08f701e5468386cf1934aa27757a64c619571da20 \
          gcc-boot-2.95.3:3c42f413b78b341cc064adc505a64445aa4b8c9fc6ce4f7a35a719c8ba92830e \
          glibc-boot-2.2.5:a8de80055076ce1915faed6d9f4380fcf67ee8dad2b4e739c74c9f977213dfdb \
          glibc-bootstrap-system-2.2.5:a8a214f78c96723fee3d9d26b59249029e617bc720880ca2789a66ed73e2c7d0 \
          gcc-boot-4.6.4:0dfcb1813ca54eafad0d3bbec17b423d6e50ab76d730b35eb6df7018ed43edff \
          glibc-boot-2.16.0:3de61d25fff5924723ec8fb0a57d37305f8e25b9e65d3d67a6535dbe08ac0e88 \
          glibc-bootstrap-system-2.16.0:061cf1269b9d497962389c8b0c52659f8294ae16e0963d146b6599f096bb50ff; do
  pf="$ROOT/seed/patches/${pp%%:*}.patch"
  test -f "$pf" || fail "vendored patch missing ($pf)"
  test "`sha "$pf"`" = "${pp##*:}" || fail "vendored patch sha256 != pin ($pf)"
done
echo "   [pinned-input] all chain tarballs + kernel headers + 7 vendored boot patches match their pins"

# --- the recipe LADDER: one engine drive over the 20-rung graph -------------------------------
command -v load_recipe_eval >/dev/null 2>&1 || . tests/cache-lib.sh
load_recipe_eval 2>/dev/null || {
  sh tests/recipe-eval-tool.sh "$ROOT/.td-build-cache/recipe-eval" >/dev/null || fail "could not build td-recipe-eval (tests/recipe-eval-tool.sh)"
  load_recipe_eval || fail "no td-recipe-eval sentinel after recipe-eval-tool"
}
. tests/ladder-lib.sh
# Warm (Shared gates): the machine-wide ladder dir — per-step drv-keyed, NAR-verified reuse.
# Cold (TD_CHECK_CHAIN_CACHE explicitly empty — Private gates / the daily force-cold): a fresh
# worktree-local dir, everything from the 229-byte seed.
TD_CHECK_CHAIN_CACHE="${TD_CHECK_CHAIN_CACHE-${HOME:+$HOME/.td/build-daemon/chain}}"
if [ -n "$TD_CHECK_CHAIN_CACHE" ]; then
  LWDIR="$HOME/.td/build-daemon/ladder"
else
  LWDIR="$ROOT/.td-build-cache/ladder-cold"
fi
# The lock lives OUTSIDE LWDIR (a STABLE sibling) and is taken BEFORE the cold-mode wipe: a
# lock file inside LWDIR would be unlinked by a peer's `rm -rf "$LWDIR"`, so its flock would
# guard nothing and two force-cold gates (both Shared/Daily, run in parallel on the daily
# backstop) would build into the same dir and corrupt each other's from-seed run. Held for
# the WHOLE body (not released after ladder_build): the tail's `_lout` reads build-*.out,
# which a peer's ladder_build truncates — so a peer must not run until this gate's tail is
# done reading. The kernel drops the fd on process exit; concurrent gates serialize and the
# waiter then cache-hits the warm ladder (or, cold, rebuilds from seed after the winner).
mkdir -p "`dirname "$LWDIR"`"
exec 9>"$LWDIR.lock"; flock 9 || fail "ladder: flock failed"
test -n "$TD_CHECK_CHAIN_CACHE" || rm -rf "$LWDIR"   # cold: from-scratch, now serialized under the lock
mkdir -p "$LWDIR"
ladder_setup "$LWDIR" || fail "ladder_setup (intern/tools) failed"
ladder_emit stage0 mes tcc make-mesboot0 patch-mesboot binutils-mesboot0 gcc-core-mesboot0 \
  mesboot-headers glibc-mesboot0 gcc-mesboot0 binutils-mesboot1 make-mesboot gcc-mesboot1 \
  binutils-mesboot gawk-mesboot glibc-mesboot gcc-mesboot glibc-mesboot-shared gcc-14 \
  binutils-244 glibc-241 || fail "ladder recipe emit failed"
# Locks are no longer written here (#429): build-plan --auto SYNTHESIZES each rung's lock
# straight from its recipe JSON's declared inputs/nativeInputs/sourceInput.
echo "   [ladder] build-plan --auto glibc-241: the 20-rung recipe graph, from the 229-byte seed"
ladder_build glibc-241 >/dev/null || fail "the recipe ladder did not build (see $LWDIR/build-glibc-241.err)"

# --- tail exports + [no-guix] re-asserts (every run, warm or cold) ----------------------------
LADDER_TDSTORE="$LWDIR/scratch/tdstore"
_lout() { _o=`"$TB" text extract-prefix-last "STEP $1 " "$LWDIR/build-glibc-241.out"`; test -n "$_o" || fail "no STEP output for $1"; printf '%s' "${_o##*/}"; }
GCC14_BASE=`_lout gcc-14`; GLIBC241_BASE=`_lout glibc-241`
BU244_BASE=`_lout binutils-244`; GLSHARED_BASE=`_lout glibc-mesboot-shared`
GCC14="$LADDER_TDSTORE/$GCC14_BASE/stage/td/store/gcc-14.3.0"
GLIBC241="$LADDER_TDSTORE/$GLIBC241_BASE/stage/td/store/glibc-2.41"
BMB244SB="$LADDER_TDSTORE/$BU244_BASE"
CC1=`ls "$GCC14"/libexec/gcc/i686-unknown-linux-gnu/14.3.0/cc1 2>/dev/null || true`
test -e "$GLIBC241/lib/libc.so.6" -a -e "$GLIBC241/lib/ld-linux.so.2" || fail "glibc 2.41 missing libc.so.6/ld-linux.so.2"
test -e "$GLIBC241/include/linux/limits.h" || fail "kernel headers not present in glibc 2.41 include"
# Scan the toolchain's LINK INPUTS — the objects/libs a compiled program actually pulls in
# (glibc crt + gcc-internal libgcc/crtbegin/crtend + static libstdc++), not just the driver.
# A /gnu/store byte in ANY of these injects guix into every program td's toolchain builds; the
# old leg checked only libc.so.6/gcc/cc1 and missed the crt/libgcc surface. (This is the
# toolchain-provenance seal; a corpus binary may still carry harmless NON-load-bearing residue
# from the guix-seed BUILD TOOLS — make/bash/coreutils, retired last in #312 — so the assertion
# is on what td PRODUCES, not on the whole compiled artifact.)
GCCLIBEXEC="$GCC14/lib/gcc/i686-unknown-linux-gnu/14.3.0"
for b in "$GLIBC241/lib/libc.so.6" "$GCC14/bin/gcc" "$CC1" \
         "$GLIBC241/lib/crt1.o" "$GLIBC241/lib/crti.o" "$GLIBC241/lib/crtn.o" \
         "$GCCLIBEXEC/libgcc.a" "$GCCLIBEXEC/crtbegin.o" "$GCCLIBEXEC/crtend.o" \
         "$GCC14/lib/libstdc++.a"; do
  test -n "$b" -a -e "$b" || fail "toolchain link input missing ($b)"
  if ! "$TB" text not-contains '/gnu/store' "$b"; then fail "$b contains /gnu/store bytes"; fi
done
echo "   [no-guix] recipe ladder: seed → … → gcc 14.3.0 + binutils 2.44 → glibc 2.41; no /gnu/store in libc.so.6 / gcc / cc1 / crt / libgcc / libstdc++"
}

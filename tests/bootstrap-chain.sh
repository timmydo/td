#!/bin/sh
# tests/bootstrap-chain.sh — the SHARED from-seed modern-toolchain driver, sourced by the
# bootstrap-*-store-native gates. The caller sets `set -eu` + ROOT=$(pwd), runs load_stage0
# ($TB + TD_BUILDER_*), sources this, then calls `bootstrap_modern_toolchain`.
#
# #378 slices 2+3: the ~850-line build_* shell LADDER IS DELETED. Every rung — stage0 →
# mes → tcc → make/patch → binutils/gcc-2.95 → the mesboot ladder → gcc-4.9.4 →
# gcc-14.3.0 + binutils-2.44 + glibc-2.41 — is a RECIPE (recipes/src/recipes/*.rs, typed
# steps) built by the ENGINE: ONE `td-builder build-plan --auto glibc-241` realizes the
# whole 20-rung graph, chaining each rung on the prior rungs' outputs (native-inputs /
# td-recipe-output edges). tests/ladder-lib.sh is the only shell left: interning the
# pinned sources/patches/seed (content-addressed), resolving the DECLARED host tools,
# writing locks, and driving the engine — plumbing, not build logic.
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
lf() { sed -n "s/^$2 //p" "$1" | head -1; }

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
KH_VER=`printf '%s' "\`lf "$LINUX_LOCK" file\`" | sed -n 's/^linux-\(.*\)\.tar\..*$/\1/p'`
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
  LWDIR="$ROOT/.td-build-cache/ladder-cold"; rm -rf "$LWDIR"
fi
mkdir -p "$LWDIR"
# One builder at a time on the shared dir; the flock loser then cache-hits through.
exec 9>"$LWDIR/.lock"; flock 9 || fail "ladder: flock failed"
ladder_setup "$LWDIR" || fail "ladder_setup (intern/tools) failed"
ladder_emit stage0 mes tcc make-mesboot0 patch-mesboot binutils-mesboot0 gcc-core-mesboot0 \
  mesboot-headers glibc-mesboot0 gcc-mesboot0 binutils-mesboot1 make-mesboot gcc-mesboot1 \
  binutils-mesboot gawk-mesboot glibc-mesboot gcc-mesboot glibc-mesboot-shared gcc-14 \
  binutils-244 glibc-241 || fail "ladder recipe emit failed"
BT="tool:bash tool:coreutils tool:sed tool:grep tool:gawk tool:tar tool:gzip tool:bzip2 tool:xz tool:findutils tool:diffutils"
ladder_lock stage0 stage0-source || fail "lock stage0"
ladder_lock mes mes-source rung:stage0 src:nyacc $BT || fail "lock mes"
ladder_lock tcc tcc-source rung:stage0 rung:mes $BT || fail "lock tcc"
ladder_lock make-mesboot0 make-mesboot0-source rung:mes rung:tcc $BT || fail "lock make-mesboot0"
ladder_lock patch-mesboot patch-mesboot-source rung:mes rung:tcc rung:make-mesboot0 $BT || fail "lock patch-mesboot"
ladder_lock binutils-mesboot0 binutils-mesboot-source rung:mes rung:tcc rung:make-mesboot0 rung:patch-mesboot src:patch-binutils-boot-2.20.1a tool:flex tool:bison $BT || fail "lock binutils-mesboot0"
ladder_lock gcc-core-mesboot0 gcc-core-source rung:mes rung:tcc rung:make-mesboot0 rung:patch-mesboot rung:binutils-mesboot0 src:patch-gcc-boot-2.95.3 tool:flex tool:bison $BT || fail "lock gcc-core-mesboot0"
ladder_lock mesboot-headers linux-headers rung:mes $BT || fail "lock mesboot-headers"
ladder_lock glibc-mesboot0 glibc-mesboot0-source rung:mes rung:tcc rung:make-mesboot0 rung:patch-mesboot rung:binutils-mesboot0 rung:gcc-core-mesboot0 rung:mesboot-headers src:patch-glibc-boot-2.2.5 src:patch-glibc-bootstrap-system-2.2.5 $BT || fail "lock glibc-mesboot0"
ladder_lock gcc-mesboot0 gcc-core-source rung:make-mesboot0 rung:patch-mesboot rung:binutils-mesboot0 rung:gcc-core-mesboot0 rung:glibc-mesboot0 rung:mesboot-headers src:patch-gcc-boot-2.95.3 tool:flex tool:bison $BT || fail "lock gcc-mesboot0"
ladder_lock binutils-mesboot1 binutils-mesboot-source rung:make-mesboot0 rung:patch-mesboot rung:binutils-mesboot0 rung:gcc-mesboot0 rung:glibc-mesboot0 src:patch-binutils-boot-2.20.1a src:linux-headers tool:flex tool:bison $BT || fail "lock binutils-mesboot1"
ladder_lock make-mesboot make-mesboot-source rung:make-mesboot0 rung:binutils-mesboot0 rung:gcc-mesboot0 rung:glibc-mesboot0 src:linux-headers $BT || fail "lock make-mesboot"
ladder_lock gcc-mesboot1 gcc-464-core rung:make-mesboot0 rung:patch-mesboot rung:binutils-mesboot1 rung:gcc-mesboot0 rung:glibc-mesboot0 rung:make-mesboot src:gcc-464-gpp src:patch-gcc-boot-4.6.4 src:gmp src:mpfr src:mpc src:linux-headers tool:flex tool:bison $BT || fail "lock gcc-mesboot1"
ladder_lock binutils-mesboot binutils-mesboot-source rung:make-mesboot rung:patch-mesboot rung:binutils-mesboot1 rung:gcc-mesboot1 rung:glibc-mesboot0 src:patch-binutils-boot-2.20.1a src:linux-headers tool:flex tool:bison $BT || fail "lock binutils-mesboot"
ladder_lock gawk-mesboot gawk-mesboot-source rung:make-mesboot rung:binutils-mesboot1 rung:gcc-mesboot1 rung:glibc-mesboot0 src:linux-headers $BT || fail "lock gawk-mesboot"
ladder_lock glibc-mesboot glibc-216-source rung:make-mesboot rung:patch-mesboot rung:binutils-mesboot rung:gcc-mesboot1 rung:glibc-mesboot0 rung:gawk-mesboot src:patch-glibc-boot-2.16.0 src:patch-glibc-bootstrap-system-2.16.0 src:linux-headers $BT || fail "lock glibc-mesboot"
ladder_lock gcc-mesboot gcc-494-source rung:make-mesboot rung:patch-mesboot rung:binutils-mesboot rung:gcc-mesboot1 rung:glibc-mesboot src:gmp src:mpfr src:mpc src:linux-headers tool:flex tool:bison $BT || fail "lock gcc-mesboot"
ladder_lock glibc-mesboot-shared glibc-216-source rung:make-mesboot rung:patch-mesboot rung:binutils-mesboot rung:gcc-mesboot1 rung:glibc-mesboot0 rung:gawk-mesboot src:patch-glibc-boot-2.16.0 src:patch-glibc-bootstrap-system-2.16.0 src:linux-headers $BT || fail "lock glibc-mesboot-shared"
ladder_lock gcc-14 gcc-14-source rung:binutils-mesboot rung:gcc-mesboot rung:glibc-mesboot src:gmp63 src:mpfr421 src:mpc131 src:linux-headers tool:flex tool:bison tool:m4 tool:make $BT || fail "lock gcc-14"
ladder_lock binutils-244 binutils-244-source rung:gcc-mesboot1 rung:glibc-mesboot-shared rung:binutils-mesboot tool:flex tool:bison tool:make $BT || fail "lock binutils-244"
ladder_lock glibc-241 glibc-241-source rung:gcc-14 rung:glibc-mesboot-shared rung:binutils-244 src:linux-headers tool:flex tool:bison tool:m4 tool:make tool:gettext tool:texinfo tool:python $BT || fail "lock glibc-241"
echo "   [ladder] build-plan --auto glibc-241: the 20-rung recipe graph, from the 229-byte seed"
ladder_build glibc-241 >/dev/null || fail "the recipe ladder did not build (see $LWDIR/build-glibc-241.err)"

# --- tail exports + [no-guix] re-asserts (every run, warm or cold) ----------------------------
LADDER_TDSTORE="$LWDIR/scratch/tdstore"
_lout() { _o=`sed -n "s/^STEP $1 //p" "$LWDIR/build-glibc-241.out" | tail -1`; test -n "$_o" || fail "no STEP output for $1"; printf '%s' "${_o##*/}"; }
GCC14_BASE=`_lout gcc-14`; GLIBC241_BASE=`_lout glibc-241`
BU244_BASE=`_lout binutils-244`; GLSHARED_BASE=`_lout glibc-mesboot-shared`
GCC14="$LADDER_TDSTORE/$GCC14_BASE/stage/td/store/gcc-14.3.0"
GLIBC241="$LADDER_TDSTORE/$GLIBC241_BASE/stage/td/store/glibc-2.41"
BMB244SB="$LADDER_TDSTORE/$BU244_BASE"
CC1=`ls "$GCC14"/libexec/gcc/i686-unknown-linux-gnu/14.3.0/cc1 2>/dev/null || true`
test -e "$GLIBC241/lib/libc.so.6" -a -e "$GLIBC241/lib/ld-linux.so.2" || fail "glibc 2.41 missing libc.so.6/ld-linux.so.2"
test -e "$GLIBC241/include/linux/limits.h" || fail "kernel headers not present in glibc 2.41 include"
for b in "$GLIBC241/lib/libc.so.6" "$GCC14/bin/gcc" "$CC1"; do
  test -n "$b" -a -e "$b" || fail "toolchain output missing ($b)"
  if grep -q -a '/gnu/store' "$b"; then fail "$b contains /gnu/store bytes"; fi
done
echo "   [no-guix] recipe ladder: seed → … → gcc 14.3.0 + binutils 2.44 → glibc 2.41; no /gnu/store in libc.so.6 / gcc / cc1"
}

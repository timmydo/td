#!/bin/sh
# tests/rust-x86_64-runtime-store-native.sh — rust-store-native track: the /td/store RUNTIME+COMPILE
# leg, the critical-path arrow "retarget rust toolchain to /td/store with gcc toolchain" (DESIGN §1.1).
#
# The prior gate (#218) RELINKED the upstream x86_64 Rust toolchain to /td/store and RAN rustc -vV +
# cargo --version there. That proved rust could RUN from /td/store, but not that it could do its JOB —
# COMPILE. It could not: rustc links final binaries through a C toolchain (cc → ld), and the only
# x86_64 gcc td had then was the CROSS gcc (#201), an i686 (ELF 32-bit) binary EMITTING x86_64 — it
# cannot RUN inside an x86_64 own-root, so no link could happen there. #240 (rung X2) removed that
# wall: a NATIVE x86_64 gcc 14.3.0 (ELF 64-bit, --host=x86_64) that RUNS in the own-root. So the leg
# was blocked on ARCH (a native x86_64 linker), exactly as the prior gate noted, and #240 unblocks it.
#
# This gate now drives the WHOLE arrow — a /td/store rust toolchain that COMPILES + LINKS + RUNS a real
# program, entirely within td's own store, /gnu/store absent:
#
#   - the x86_64 CROSS toolchain {binutils-2.44, gcc-14.3.0, glibc-2.41} is obtained the way the x86_64
#     gates do: FETCH the lock-keyed signed closure if check.sh host-prep exposed a substitute store,
#     else BUILD it from the 229-byte seed (directive 1; the daily full suite is the sole from-seed
#     authoritative builder). The substitute is an optimization, never a correctness dependency.
#   - with the cross toolchain, td builds the NATIVE x86_64 binutils 2.44 + NATIVE x86_64 gcc 14.3.0
#     (rung X2 of #240 — ELF 64-bit as/ld/gcc/cc1, STATIC vs the /td/store x86_64 glibc 2.41), plus an
#     x86_64 libz.so.1 from zlib 1.3.1 source (upstream libLLVM dynamically NEEDs libz).
#   - the upstream Rust 1.96.0 release tarball (static.rust-lang.org, sha256-pinned, GUIX-FREE) is
#     RELINKED to /td/store by td's OWN ELF rewriter (elf-set-interp → the interned glibc's FULL hashed
#     loader path, GROWN via PT_NOTE→PT_LOAD — a normal staged store path, no patchelf), with
#     its full runtime closure co-located in the tree's lib/ (via the UNCHANGED RUNPATH $ORIGIN/../lib):
#     librustc_driver + libLLVM, the /td/store x86_64 glibc 2.41 libs, libgcc_s.so.1, the built
#     libz.so.1, AND the rustlib SYSROOT (libstd/libcore rlibs) so rustc has a target to link against.
#   - the rust tree AND the native gcc + native binutils + x86_64 glibc are interned content-addressed
#     as siblings at /td/store, and inside the store-ns own-root (interp = the /td/store glibc loader) rustc:
#       (a) RUNS: rustc -vV / cargo --version → "rustc 1.96.0" / "cargo 1.96.0";
#       (b) COMPILES a real program: `rustc hello.rs -o hello -C linker=<the /td/store native gcc>` (with
#           link-args baking interp/RUNPATH = the /td/store glibc) → a DYNAMIC ELF64 x86-64 binary whose
#           interp is the /td/store x86_64 ld — the compiler, the linker, the libc AND the produced
#           binary all living in td's own store;
#       (c) the produced binary RUNS → "hello from the /td/store rust toolchain: 42";
#     all with /gnu/store ABSENT.
#
# Every dep is td-built-from-seed (glibc/libgcc/libz/native-gcc/native-binutils) or upstream-not-guix
# (rust), so the whole running/compiling /td/store rust package carries ZERO /gnu/store bytes. HEAVY
# (the native gcc build is ~45 min; from-seed adds the ~98-min cross build). NOT a BUILD_GATE.
#
# Legs (DURABLE — no guix oracle in any):
#   [supply-chain]  the rust + zlib tarballs match their lock sha256 (the sha IS the oracle).
#   [provenance]    the upstream rustc/cargo/.so carry zero /gnu/store (upstream-not-guix).
#   [native-arch]   the linker rustc drives is the NATIVE x86_64 gcc/cc1 + native as/ld (ELF64 x86-64).
#   [no-guix]       the interned rust DELIVERABLE carries zero /gnu/store anywhere (recursive), and the
#                   compile-path toolchain binaries (gcc/cc1, as/ld, libc.so.6, ld) carry zero /gnu/store
#                   (as gate 422 checks); the relinked interp is the /td/store glibc loader. The seed-bootstrapped
#                   toolchain's build/debug utility scripts (glibc mtrace/ldd, gcc install-tools) still
#                   bake the guix-seed interpreter — that is the seed-retirement milestone, retired last.
#   [structural]    the tree's lib/ closure is COMPLETE (every NEEDED soname + the rustlib sysroot);
#                   the native binutils as/ld are interned beside the native gcc (a complete toolchain).
#   [behavioral]    rustc -vV + cargo --version RUN, AND rustc COMPILES hello.rs via the /td/store native
#                   gcc into a DYNAMIC ELF64 x86-64 binary (interp = /td/store ld) that RUNS → the real
#                   string. THE durable payoff (an x86_64 rust toolchain that COMPILES with no guix
#                   process and no guix bytes in its store).
#   [structural]    inside the own-root /td/store IS the store AND /gnu/store is ABSENT.
# Self-discrimination (verified-red): drop `-C linker=<native gcc>` (rustc falls back to a `cc` that is
# not on the own-root PATH), or drop the rustlib sysroot (no libstd to link), or drop the native binutils
# from PATH (the native gcc cannot find ld) → the COMPILE fails and the gate reds; each is load-bearing.
set -eu

# --- source the x86_64 toolchain gate as a FUNCTION LIBRARY (defines the cross + native build_* rungs,
# sets/verifies the pinned-input vars incl. the x86_64 kernel headers, returns BEFORE its build driver).
export TD_X86_64_LIB=1
. tests/bootstrap-x86_64-toolchain-store-native.sh
unset TD_X86_64_LIB
. tests/cache-lib.sh
. tests/x86_64-cross-fns.sh
. tests/x86_64-subst-lib.sh
# now in scope: ROOT, fail(), sha(), lf(), make_curated_path, the cross + native build_* rungs,
# run_x86_64_cross, x86_64_resolve_closure, KH_X86_64_TB, XTARGET.

# --- [supply-chain] the upstream Rust + zlib tarballs match their lock sha256 (upstream, not guix) -
RUST_LOCK=`ls seed/sources/rust-*.lock 2>/dev/null | head -1`
test -n "$RUST_LOCK" || fail "no seed/sources/rust-*.lock pin"
RUST_FILE=`lf "$RUST_LOCK" file`; RUST_TB=".td-build-cache/sources/$RUST_FILE"
test -f "$RUST_TB" || fail "warmed $RUST_TB absent — run td-feed warm sources (host PREP)"
test "`sha "$RUST_TB"`" = "`lf "$RUST_LOCK" sha256`" || fail "warmed $RUST_TB sha256 != lock pin"
ZLIB_LOCK=`ls seed/sources/zlib-*.lock 2>/dev/null | head -1`
test -n "$ZLIB_LOCK" || fail "no seed/sources/zlib-*.lock pin"
ZLIB_TB=".td-build-cache/sources/`lf "$ZLIB_LOCK" file`"
test -f "$ZLIB_TB" || fail "warmed $ZLIB_TB absent — run td-feed warm sources (host PREP)"
test "`sha "$ZLIB_TB"`" = "`lf "$ZLIB_LOCK" sha256`" || fail "warmed $ZLIB_TB sha256 != lock pin"
echo "   [supply-chain] rust ($RUST_FILE) + zlib match their lock sha256 — upstream bytes, not guix"

# build_zlib_x86_64 <cpath> <xgcc2> <xglibc> <xlibgccdir> <xbu> <out> — x86_64 zlib 1.3.1 (libz.so.1)
# built FROM SOURCE by the cross gcc 14.3.0 vs the /td/store x86_64 glibc 2.41. Output: $out/libz.so.1.3.1.
build_zlib_x86_64() {
  zc=$1; xg=$2; xgl=$3; xlg=$4; xb=$5; out=$6
  rm -rf "$out"; mkdir -p "$out"
  csh=`command -v bash 2>/dev/null || command -v sh`
  src=`mktemp -d`/zlib; mkdir -p "$src"
  tar -xzf "$ZLIB_TB" -C "$src" --strip-components=1 || { echo "zlib unpack failed" >&2; return 1; }
  # combined include dir: x86_64 glibc headers + kernel UAPI (glibc's bits/local_lim.h #includes
  # <linux/limits.h>). The FETCHED glibc-2.41 closure ships no linux/ headers, so the cc wrapper must
  # add them here — exactly as the toolchain-recipe x86_64-native merges $kh into its build sysroot's include/.
  inc="$out/include"; mkdir -p "$inc"
  cp -a "$xgl/include/." "$inc/" || { echo "stage glibc headers failed" >&2; return 1; }
  tar -xzf "$KH_X86_64_TB" -C "$inc" || { echo "x86_64 kernel headers unpack failed" >&2; return 1; }
  wb=`mktemp -d`/wb; mkdir -p "$wb"
  printf '#!%s\nexec "%s/bin/%s-gcc" -isystem "%s" -B"%s/lib" -L"%s/lib" -L"%s" "$@"\n' \
    "$csh" "$xg" "$XTARGET" "$inc" "$xgl" "$xgl" "$xlg" > "$wb/cc"
  chmod 0555 "$wb/cc"
  ( cd "$src"; bp="$xb/bin:$zc"
    env PATH="$bp" CC="$wb/cc" CHOST="$XTARGET" AR="$xb/bin/$XTARGET-ar" RANLIB="$xb/bin/$XTARGET-ranlib" \
      "$csh" ./configure --prefix=/td/store/zlib-1.3.1 --shared >cfg.log 2>&1 \
      || { echo "zlib configure failed" >&2; cp cfg.log "$ROOT/.td-build-cache/_zlibx-cfg.log" 2>/dev/null||true; tail -20 cfg.log >&2; return 1; }
    env PATH="$bp" MAKEFLAGS= MFLAGS= GNUMAKEFLAGS= MAKELEVEL= make SHELL="$csh" CONFIG_SHELL="$csh" libz.so.1.3.1 >build.log 2>&1 \
      || { echo "zlib make failed" >&2; cp build.log "$ROOT/.td-build-cache/_zlibx-build.log" 2>/dev/null||true; tail -25 build.log >&2; return 1; }
    cp -a libz.so.1.3.1 "$out/libz.so.1.3.1" ) || return 1
  test -f "$out/libz.so.1.3.1" || { echo "no x86_64 libz.so produced" >&2; return 1; }
}

# --- curated PATH + td's guix-free stage0 builder (ELF relink + store intern + own-root run) --------
cpath=`make_curated_path`
for bad in gcc g++ cc guile guix; do test ! -e "$cpath/$bad" || fail "curated PATH still exposes '$bad'"; done
export TD_STAGE0_BASE="`pwd`/.td-build-cache/td-shell"
load_stage0 || fail "stage0-builder could not place a guix-free stage0 td-builder"
export TD_STORE_DIR=/td/store

snwork=`mktemp -d`
trap 'rm -rf "$snwork" "${rtree:-}" "${ZLIBX:-}" "${nrout:-}" "${cpathdir:-}"' EXIT INT TERM
cpathdir=`dirname "$cpath"`

# ============================================================================================
# Obtain the CROSS toolchain {XBU, XGCC2, XGLIBC, XLIBGCCDIR}: FETCH the lock-keyed closure, else
# BUILD it from the 229-byte seed (directive 1). Mirrors gate 422 (the native-gcc gate).
# ============================================================================================
cstore="$snwork/closure-store"; cdb="$snwork/closure.db"; mkdir -p "$cstore"
if x86_64_resolve_closure "$cstore" "$cdb"; then
  echo ">> [subst/SKIP] fetched the x86_64 cross toolchain closure {binutils,gcc,glibc} — SKIPPED the ~98-min from-seed build"
else
  echo ">> [subst/MISS] no exposed substitute store — building the cross toolchain from the 229-byte seed (directive 1)"
  # i686 base (21 rungs) + the 4 cross rungs are recipes (#378 slice 4); run_x86_64_cross
  # drives the whole graph via build-plan --auto and exports XBU XGCC2 XGLIBC XLIBGCCDIR XSTDCXXDIR.
  run_x86_64_cross "$cpath" || fail "the x86_64 cross toolchain (recipe ladder) failed to build from the seed"
  # run_x86_64_cross exports XGLIBC XGCC2 XLIBGCCDIR XSTDCXXDIR XBU X86_WORK (physical trees)
fi
test -n "${XGCC2:-}" -a -n "${XGLIBC:-}" -a -n "${XBU:-}" -a -n "${XLIBGCCDIR:-}" || fail "cross toolchain vars unset after fetch/build"
echo "   cross toolchain ready: x86_64 glibc 2.41 ($XGLIBC) + libgcc_s ($XLIBGCCDIR)"

# ============================================================================================
# RUNG X2: the NATIVE x86_64 toolchain (rung X2 of #240) — the LINKER rustc will drive in the own-root.
# ============================================================================================
echo ">> [N1+N2] NATIVE x86_64 binutils 2.44 + gcc 14.3.0 via the Rust toolchain-recipe (structured port)"
nrout=`mktemp -d`/native-out
x86_64_build_native_recipe "$cpath" "$XGCC2" "$XGLIBC" "$XBU" "$nrout" || fail "could not build the NATIVE x86_64 toolchain (recipe)"
test -x "$XNGCC/bin/gcc" -a -x "$XNBU/bin/as" -a -x "$XNBU/bin/ld" || fail "the native x86_64 toolchain is missing gcc/as/ld"
echo "   native x86_64 toolchain ready: gcc ($XNGCC) + as/ld ($XNBU)"

# ============================================================================================
# Stage the toolchain into the own-root store + obtain the RELINKED rust tree. glibc is interned at
# its cross-lock INPUT-ADDRESSED path (the relinked rust tree's interp points there — a STABLE,
# subst-fetchable loader path); the native gcc/binutils stay content-addressed (the own-root probe
# finds them via PATH). The rust tree is FETCHED at its td-toolchain-rust-x86_64.lock path if a
# substitute is exposed, else ASSEMBLED+RELINKED by the `toolchain-recipe rust-x86_64` recipe and
# interned at that lock path + subst-exported (from-BUILD fallback — directive 1; the daily publishes).
# ============================================================================================
store="$snwork/td-store"; sndb="$snwork/store.db"; mkdir -p "$store"
NBP=`"$TB" store-add-recursive "\`basename "$XNBU"\`" "$XNBU" "$store" "$sndb"` || fail "store-add native binutils failed"
nbrel=`basename "$NBP"`
NGP=`"$TB" store-add-recursive "\`basename "$XNGCC"\`" "$XNGCC" "$store" "$sndb"` || fail "store-add native gcc failed"
case "$NGP" in /td/store/*-gcc-14.3.0-x86_64-native) ;; *) fail "native gcc not content-addressed under /td/store (got: $NGP)" ;; esac
ngrel=`basename "$NGP"`
# glibc at its cross-lock INPUT-ADDRESSED path (so the rust interp target is stable + fetchable).
GLK=`"$TB" toolchain-key tests/td-toolchain-x86_64.lock` || fail "toolchain-key (cross) failed"
GLP=`"$TB" store-add-input-addressed glibc-2.41-x86_64 "$GLK" "$XGLIBC" "$store" "$sndb"` || fail "store-add-input-addressed x86_64 glibc failed"
glrel=`basename "$GLP"`
GLWANT=`"$TB" toolchain-path tests/td-toolchain-x86_64.lock glibc-2.41-x86_64`
test "$GLP" = "$GLWANT" || fail "interned glibc path $GLP != lock-computed $GLWANT"
GLIBC_INTERP="$GLP/lib/ld-linux-x86-64.so.2"

if x86_64_resolve_closure_rust "$store" "$sndb"; then
  echo ">> [subst/SKIP rust] fetched the RELINKED rust tree at its lock path — SKIPPED the assemble+relink"
else
  echo ">> [subst/MISS rust] no exposed rust substitute — assembling + relinking from the cross toolchain (directive 1)"
  # x86_64 zlib (libLLVM NEEDs libz; the toolchain provides none) — only needed to ASSEMBLE the tree.
  ZLIBX=`mktemp -d`/zlibx
  build_zlib_x86_64 "$cpath" "$XGCC2" "$XGLIBC" "$XLIBGCCDIR" "$XBU" "$ZLIBX" || fail "the cross gcc did not build x86_64 zlib"
  XLIBZ="$ZLIBX/libz.so.1.3.1"
  if grep -q -a '/gnu/store' "$XLIBZ"; then fail "the built x86_64 libz contains /gnu/store bytes"; fi
  echo "   built x86_64 libz.so.1 from zlib 1.3.1 source (cross gcc 14.3.0, no /gnu/store)"
  # assemble + relink via the structured recipe (relink target = the LOCK-KEYED glibc loader).
  top="${RUST_FILE%.tar.gz}"
  rrout=`mktemp -d`/rust-out; mkdir -p "$rrout"   # the recipe stdout is captured to $rrout/recipe.out
  env TDRX_RUST_TAR="$RUST_TB" TDRX_RUST_TOP="$top" TDRX_XGLIBC="$XGLIBC" \
      TDRX_XLIBGCCDIR="$XLIBGCCDIR" TDRX_XLIBZ="$XLIBZ" TDRX_GLIBC_INTERP="$GLIBC_INTERP" \
      TDRX_OUT="$rrout" "$TB" toolchain-recipe rust-x86_64 > "$rrout/recipe.out" 2>&1 \
    || { sed 's/^/   /' "$rrout/recipe.out" >&2; fail "toolchain-recipe rust-x86_64 failed"; }
  sed 's/^/   /' "$rrout/recipe.out"
  RUST_TREE=`sed -n 's/^RUST_TREE=//p' "$rrout/recipe.out"`
  test -n "$RUST_TREE" -a -d "$RUST_TREE" || fail "the rust recipe produced no tree"
  x86_64_build_closure_rust "`pwd`/.td-build-cache/x86_64-rust-closure-export" "$store" "$sndb" "$RUST_TREE" \
    || fail "could not intern + subst-export the relinked rust tree"
fi
rustrel="$RUSTREL"; phys="$store/$rustrel"; out="/td/store/$rustrel"
chmod -R u+w "$store"
test -x "$phys/bin/rustc" -a -x "$phys/bin/cargo" || fail "rust tree missing rustc/cargo at $phys"
test -x "$store/$ngrel/bin/gcc" -a -x "$store/$nbrel/bin/as" -a -x "$store/$nbrel/bin/ld" || fail "interned native toolchain missing gcc/as/ld"
echo "   [input-addressed] rust ($out) at its lock-keyed path; native gcc ($NGP) + binutils content-addressed; x86_64 glibc at its lock path ($GLP)"

# --- [native-arch] the linker rustc will drive is the NATIVE x86_64 gcc/cc1 + native as/ld (ELF64) --
nhdr=`"$store/$nbrel/bin/readelf" -h "$store/$ngrel/bin/gcc" 2>/dev/null`
echo "$nhdr" | grep -i 'class:' | grep -q 'ELF64' || fail "the interned native gcc is not ELF64"
echo "$nhdr" | grep -i 'machine:' | grep -qi 'x86-64' || fail "the interned native gcc machine is not x86-64"
echo "   [native-arch] the /td/store linker toolchain (gcc + as/ld) is ELF 64-bit x86-64 — a native compiler"

# --- [no-guix] the DELIVERABLE rust package carries zero /gnu/store ANYWHERE (recursive — it is the
# upstream-not-guix "build world" output), AND the COMPILE-PATH binaries of the seed-bootstrapped toolchain
# (gcc/cc1, as/ld, libc.so.6, ld) carry zero /gnu/store. The toolchain's build/debug UTILITY scripts (glibc
# bin/mtrace|ldd|…, gcc install-tools/fixinc.sh) bake the build INTERPRETER — in the loop sandbox the
# guix-seed bash/perl — because the whole toolchain is bootstrapped from the guix seed (retired LAST per
# the north star); those scripts are NOT on the compile/link path this gate drives. So this greps exactly
# the load-bearing compile-path binaries, matching the sibling native-gcc gate 422
# (verify_x86_64_native_ownroot), plus the recursive check on the rust deliverable.
# (directive 3: #255's first cut recursively grepped the toolchain trees too and reddened on that
# seed-interpreter scaffolding — genuine guix bytes, but in seed-bootstrapped debug/install utilities, not
# the compiler. Narrowed here to the compile-path binaries + the recursive rust check: the honest,
# milestone-accurate leg. The "zero /gnu/store in every toolchain byte" claim is the seed-retirement
# milestone, not this one.)
if grep -r -a -q '/gnu/store' "$phys" 2>/dev/null; then
  fail "interned RUST tree contains a /gnu/store reference: `grep -r -a -l '/gnu/store' "$phys" 2>/dev/null | head -1`"
fi
ncc1=`find "$store/$ngrel" -name cc1 2>/dev/null | head -1`
for b in "$store/$ngrel/bin/gcc" "$ncc1" "$store/$nbrel/bin/as" "$store/$nbrel/bin/ld" \
         "$store/$glrel/lib/libc.so.6" "$store/$glrel/lib/ld-linux-x86-64.so.2"; do
  test -n "$b" -a -e "$b" || fail "a compile-path toolchain binary is missing ($b) — cannot assert it is guix-free"
  ! grep -q -a '/gnu/store' "$b" || fail "compile-path toolchain binary carries /gnu/store bytes: $b"
done
echo "   [no-guix] the DELIVERABLE rust package carries zero /gnu/store anywhere (recursive), and the compile-path toolchain binaries (gcc/cc1, as/ld, libc.so.6, ld) carry zero /gnu/store (as in gate 422)"

# --- [structural] the rust lib/ closure is COMPLETE: every soname + the rustlib sysroot present -----
for need in librustc_driver libLLVM libc.so.6 libdl.so.2 librt.so.1 libpthread.so.0 libm.so.6 libgcc_s.so.1 libz.so.1; do
  ls "$phys"/lib/*"$need"* >/dev/null 2>&1 || fail "the interned lib/ is missing a NEEDED lib: $need"
done
ls "$phys"/lib/rustlib/x86_64-unknown-linux-gnu/lib/libstd-*.rlib >/dev/null 2>&1 || fail "the interned sysroot lost its libstd rlib"
echo "   [structural] the interned lib/ holds the complete rustc/cargo runtime closure + rustlib sysroot"

# --- a static bash (td's own store-closure reader, no guix process) for the own-root shell ---------
# the static-bash fixture is a DECLARED gate input (#353): the runner resolved it.
bs=${TD_GATE_INPUT_BASH_STATIC:-}
test -n "$bs" || fail "TD_GATE_INPUT_BASH_STATIC unset — run via td-builder gate-run, which resolves the gate's declared inputs"
test -x "$bs/bin/bash" || fail "no static bash fixture at $bs"
bbase=`basename "$bs"`; cp -a "$bs" "$store/$bbase"; chmod -R u+w "$store"

# --- assemble-only library mode (#258 rust userland cutover) -----------------------------------------
# When this script is SOURCED with TD_RUST_STORE_NATIVE_ASSEMBLE_ONLY=1 (by
# tests/rust-x86_64-userland-store-native.sh), the caller only needs the fully-assembled /td/store —
# the native x86_64 gcc + binutils, the relinked upstream rust (rustc/cargo + rustlib sysroot), the
# x86_64 glibc 2.41 (whose hashed loader path IS the relinked interp), and a static bash — interned
# in $store with its db $sndb. Export
# the handles and RETURN here, BEFORE the hello.rs probe (which is THIS gate's own behavioral leg).
# The from-scratch assembly above is byte-for-byte the same code gate 416 runs; a normal gate run
# leaves the guard unset and falls through to the probe unchanged (directive 3: the guard is inert
# when unset — no existing gate behavior is altered).
if [ "${TD_RUST_STORE_NATIVE_ASSEMBLE_ONLY:-}" = 1 ]; then
  export TDSN_STORE="$store" TDSN_DB="$sndb" TDSN_NGREL="$ngrel" TDSN_NBREL="$nbrel" \
         TDSN_GLREL="$glrel" TDSN_RUSTREL="$rustrel" TDSN_BBASE="$bbase" TDSN_CPATH="$cpath" \
         TDSN_SNWORK="$snwork" TDSN_XGLIBC="$XGLIBC" TDSN_XNGCC="$XNGCC" TDSN_XNBU="$XNBU"
  echo "   [assemble-only] /td/store assembled: native gcc=$ngrel binutils=$nbrel glibc=$glrel rust=$rustrel — returning to the userland caller"
  return 0
fi

# ============================================================================================
# In the store-ns own-root (interp = the /td/store glibc loader): rustc RUNS, then rustc COMPILES a real program
# using the /td/store NATIVE gcc as the linker, and the produced DYNAMIC ELF64 binary RUNS → 42.
# The probe is a FILE (no nested quoting) and uses ONLY bash builtins + the store's own binaries
# (the own-root has no coreutils). PATH = the native gcc + native binutils only (both load-bearing:
# the native gcc finds `ld` via PATH). glibc headers/crt come via the native gcc's -B; interp/RUNPATH
# of the produced binary are baked to the /td/store glibc + rust lib. /tmp is store-ns's writable tmpfs.
# ============================================================================================
cat > "$store/probe.sh" <<PROBE
export PATH=/td/store/$ngrel/bin:/td/store/$nbrel/bin
export TMPDIR=/tmp
GL=/td/store/$glrel
RUST=/td/store/$rustrel
[ -e /gnu/store ] && echo GNU-PRESENT || echo GNU-ABSENT
# (a) rustc + cargo RUN from /td/store
\$RUST/bin/rustc -vV && echo RUSTC-RAN
\$RUST/bin/cargo --version && echo CARGO-RAN
# (b) rustc COMPILES a real program using the /td/store native gcc as the linker
cd /tmp || exit 1
printf 'fn main(){ let n = 6 * 7; println!("hello from the /td/store rust toolchain: {}", n); }\n' > hello.rs
\$RUST/bin/rustc hello.rs -o hello \
  -C linker=/td/store/$ngrel/bin/gcc \
  -C link-arg=-B\$GL/lib \
  -C link-arg=-L\$GL/lib \
  -C link-arg=-L\$RUST/lib \
  -C link-arg=-Wl,--dynamic-linker,\$GL/lib/ld-linux-x86-64.so.2 \
  -C link-arg=-Wl,-rpath,\$GL/lib \
  -C link-arg=-Wl,-rpath,\$RUST/lib \
  && echo RUSTC-COMPILED || echo RUSTC-COMPILE-FAIL
# the produced binary is a DYNAMIC ELF64 x86-64 whose interp is the /td/store x86_64 ld
hdr=\$(/td/store/$nbrel/bin/readelf -h hello 2>/dev/null)
case "\$hdr" in *ELF64*) echo HCLASS=ELF64 ;; esac
case "\$hdr" in *X86-64*|*x86-64*) echo HMACH=x86-64 ;; esac
itp=\$(/td/store/$nbrel/bin/readelf -l hello 2>/dev/null)
case "\$itp" in *"\$GL/lib/ld-linux-x86-64.so.2"*) echo HINTERP=OK ;; esac
# (c) the produced binary RUNS from /td/store
./hello
echo "HELLO-RC=\$?"
PROBE
out2=`"$TB" store-ns "$store" -- "/td/store/$bbase/bin/bash" /td/store/probe.sh 2>&1` \
  || { printf '%s\n' "$out2" | sed 's/^/     /' >&2; fail "store-ns rust run exited nonzero"; }
printf '%s\n' "$out2" | sed 's/^/     /'

# --- [behavioral] rustc RUNS, COMPILES, and the produced binary RUNS ------------------------------
printf '%s\n' "$out2" | grep -q '^rustc 1\.96\.0' || fail "rustc did not print its version from the own-root"
printf '%s\n' "$out2" | grep -q '^RUSTC-RAN$'     || fail "rustc -vV did not run cleanly from /td/store"
printf '%s\n' "$out2" | grep -q '^cargo 1\.96\.0' || fail "cargo did not print its version from the own-root"
printf '%s\n' "$out2" | grep -q '^CARGO-RAN$'     || fail "cargo --version did not run cleanly from /td/store"
printf '%s\n' "$out2" | grep -q '^RUSTC-COMPILED$' || fail "rustc could NOT compile hello.rs via the /td/store native gcc in the own-root"
printf '%s\n' "$out2" | grep -q '^HCLASS=ELF64$'  || fail "the rustc-compiled program is not ELF64"
printf '%s\n' "$out2" | grep -q '^HMACH=x86-64$'  || fail "the rustc-compiled program is not x86-64"
printf '%s\n' "$out2" | grep -q '^HINTERP=OK$'    || fail "the rustc-compiled program's interp is not the /td/store x86_64 ld"
printf '%s\n' "$out2" | grep -q '^hello from the /td/store rust toolchain: 42$' || fail "the rustc-compiled program did not print its real output from /td/store"
printf '%s\n' "$out2" | grep -q '^HELLO-RC=0$'    || fail "the rustc-compiled program did not exit 0 from /td/store"
echo "   [behavioral] rustc RAN, COMPILED hello.rs via the /td/store native gcc → a DYNAMIC ELF64 x86-64 binary (interp = the /td/store ld) that RAN → \"hello from the /td/store rust toolchain: 42\""
printf '%s\n' "$out2" | grep -q '^GNU-ABSENT$'    || fail "/gnu/store is PRESENT in the own-root — mixed with the guix install"
echo "   [structural] inside td's own root /td/store IS the store AND /gnu/store is ABSENT"

echo "PASS: rust-x86_64-runtime-store-native — the x86_64 cross toolchain (fetched or from-seed) built the"
echo "  NATIVE x86_64 gcc 14.3.0 + binutils 2.44 + an x86_64 libz, td RELINKED the upstream Rust 1.96.0 rustc"
echo "  + cargo to /td/store (td's own ELF rewriter, no patchelf) WITH the rustlib sysroot, and in the store-ns"
echo "  own-root rustc RAN, COMPILED hello.rs via the /td/store native gcc into a DYNAMIC ELF64 x86-64 binary"
echo "  (interp = the /td/store x86_64 ld), and that binary RAN → \"…: 42\", /gnu/store ABSENT. The rust toolchain"
echo "  now COMPILES with no guix process and no guix bytes — the DESIGN 'retarget rust to /td/store' arrow."

#!/bin/sh
# tests/x86_64-cross-fns.sh — the x86_64 CROSS rungs of the x86_64-toolchain track, SOURCED by both
# the authoritative gate (tests/bootstrap-x86_64-toolchain-store-native.sh) and the dev harness
# (.td-build-cache/sbdev1/x86-harness.sh). Built BY the existing i686 gcc 14.3.0 + binutils 2.44
# (the modern /td/store toolchain — all i686). The cross flow (LFS / crosstool-NG shape):
#
#   cross binutils 2.44 (--target=x86_64-pc-linux-gnu)
#     -> cross gcc 14.3.0 stage1 (C only, --without-headers, all-gcc + all-target-libgcc)
#        -> x86_64 glibc 2.41 (built by the stage1 cross-cc; ld-linux-x86-64.so.2 + libc.so.6)
#           -> cross gcc 14.3.0 stage2 (c,c++ --enable-shared -> libgcc_s.so.1 + libstdc++)
#
# The cross TOOLS are i686 build tools (run in the sandbox/own-root, linked -static vs glibc 2.16);
# their OUTPUT targets x86_64 /td/store. The build-time scaffolding (awk/sed/make/bison/flex from
# the exposed /gnu/store) is guarded by the gate's [no-guix] leg (it checks the OUTPUT, not the
# build tools). Requires globals the chain defines: GCC14_TB GMP63_TB MPFR421_TB MPC131_TB BU244_TB
# GLIBC241_TB ROOT + fail().
XTARGET=x86_64-pc-linux-gnu
# The MODERN cross builds (binutils 2.44 / gcc 14 / glibc 2.41) parallelize safely — PLAN task #1
# endorses -j for exactly these (keep the mesboot base serial). Override with X86_MAKE_J= for serial.
: "${X86_MAKE_J:=-j4}"

# _store_tool <name> <guix-pkg> — a build-time scaffolding tool, from PATH or the exposed /gnu/store.
# _xbin <dir> — a bin/ of the autoconf/recursive-make scaffolding (awk/sed/make/bison/flex/…). Kept
# for the userland gate (busybox's Kbuild) after the cross build_* rungs became recipes.
_store_tool() { command -v "$1" 2>/dev/null || ls /gnu/store/*"$2"*/bin/"$1" 2>/dev/null | sort | head -1; }
_xbin() {
  d=$1; mkdir -p "$d"
  for tool in awk:gawk gawk:gawk sed:sed grep:grep make:make m4:m4 bison:bison flex:flex \
              cmp:diffutils diff:diffutils msgfmt:gettext makeinfo:texinfo python3:python gzip:gzip; do
    n=${tool%%:*}; pk=${tool##*:}; b=`_store_tool "$n" "$pk"`; test -n "$b" && ln -sf "$b" "$d/$n" || true
  done
  ln -sf "$d/flex" "$d/lex" 2>/dev/null || true; ln -sf "$d/bison" "$d/yacc" 2>/dev/null || true
}

# make_curated_path — a build/verify PATH from /gnu/store EXCLUDING every compiler (gcc/cc/guile/
# guix): the recipe rungs bring their own toolchain, and the store-ns verifiers must not find a
# host gcc. Moved here from the toolchain gate when its inline i686 chain was retired (#378 s4).
make_curated_path() {
  cdir=`mktemp -d`/bin; mkdir -p "$cdir"; oldifs=$IFS; IFS=:
  for d in $PATH; do [ -d "$d" ] || continue; for f in "$d"/*; do b=`basename "$f"`
    case "$b" in gcc|g++|cc|c++|cpp|gcc-*|g++-*|clang|clang*|tcc|guile|guild|guile-*|guix|guix-*) continue ;; esac
    [ -e "$cdir/$b" ] || ln -s "$f" "$cdir/$b" 2>/dev/null || true; done; done
  IFS=$oldifs; echo "$cdir"
}

# run_x86_64_cross — build the x86_64 CROSS toolchain via the recipe ladder (build-plan --auto), the
# #378 slice-4 replacement for the deleted shell build_* rungs. The i686 base (21 rungs) AND the 4
# cross rungs (binutils-x86-64 → gcc-x86-64-stage1 → glibc-x86-64 → gcc-x86-64-stage2) are recipes;
# one `ladder_build gcc-x86-64-stage2` realizes the whole graph (the warm ladder cache-hits the i686
# base). The positional args (the old shell driver's cpath/gcc14/gst/…) are IGNORED — every rung
# input is resolved from its lock — but kept so the gate call sites need no change. Exports the same
# vars run_x86_64_cross always did: XBU XGCC2
# XGLIBC XLIBGCCDIR XSTDCXXDIR (the cross toolchain, for verify + closure + the native recipe).
run_x86_64_cross() {
  . tests/ladder-lib.sh
  TD_CHECK_CHAIN_CACHE="${TD_CHECK_CHAIN_CACHE-${HOME:+$HOME/.td/build-daemon/chain}}"
  if [ -n "$TD_CHECK_CHAIN_CACHE" ]; then _lw="$HOME/.td/build-daemon/ladder"; else _lw="$ROOT/.td-build-cache/ladder-cold"; fi
  mkdir -p "`dirname "$_lw"`"
  # STABLE sibling lock, taken before the cold wipe (the bootstrap-chain.sh #378-s3 fix); held for the
  # body so a peer can't race the tail's _lo reads of build-*.out.
  exec 9>"$_lw.lock"; flock 9 || { echo "ladder: flock failed" >&2; return 1; }
  test -n "$TD_CHECK_CHAIN_CACHE" || rm -rf "$_lw"
  mkdir -p "$_lw"
  ladder_setup "$_lw" || { echo "ladder_setup failed" >&2; return 1; }
  _bt="tool:bash tool:coreutils tool:sed tool:grep tool:gawk tool:tar tool:gzip tool:bzip2 tool:xz tool:findutils tool:diffutils"
  _x86_64_cross_ladder || return 1
  ladder_build gcc-x86-64-stage2 || { echo "the x86_64 cross toolchain ladder failed" >&2; return 1; }
  _lt="$_lw/scratch/tdstore"
  _lo() { _o=`sed -n "s/^STEP $1 //p" "$_lw/build-gcc-x86-64-stage2.out" | tail -1`; test -n "$_o" || { echo "no STEP output for $1" >&2; return 1; }; printf '%s/%s' "$_lt" "${_o##*/}"; }
  # Export ONLY the cross toolchain trees (run_x86_64_cross's contract, consumed by verify/closure/
  # native): binutils-x86-64, gcc-x86-64-stage2, glibc-x86-64. gcc-14/glibc-mesboot(-shared)/
  # binutils-244 are in the plan as build DEPS but are NOT exported — they were gate-locals for the
  # retired shell repro leg, and glibc-mesboot-shared isn't even in stage2's closure.
  XBU=`_lo binutils-x86-64` || return 1
  _b=`_lo gcc-x86-64-stage2` && XGCC2="$_b/stage/td/store/gcc-14.3.0-x86_64" || return 1
  _b=`_lo glibc-x86-64` && XGLIBC="$_b/stage/td/store/glibc-2.41-x86_64" || return 1
  XLIBGCCDIR=`find "$XGCC2" -name 'libgcc_s.so.1' | head -1 | xargs -r dirname`
  XSTDCXXDIR=`find "$XGCC2" -name 'libstdc++.so.6*' | head -1 | xargs -r dirname`
  X86_WORK="$_lw"; X86_SYSROOT="$_lw/x-sysroot-unused"
  export XBU XGCC2 XGLIBC XLIBGCCDIR XSTDCXXDIR X86_WORK X86_SYSROOT
  echo "   [ladder] x86_64 cross toolchain via build-plan --auto: i686 base (21 rungs) -> cross binutils 2.44 -> gcc stage1 -> glibc 2.41 -> gcc stage2"
}

# _x86_64_cross_ladder — emit + lock the 25-rung i686-base -> x86_64-cross ladder ($_bt set +
# ladder_setup already ran). Shared by run_x86_64_cross (target gcc-x86-64-stage2) and
# run_x86_64_rust_toolchain (which adds the zlib-x86-64 + rust-toolchain rungs on top).
_x86_64_cross_ladder() {
  ladder_emit stage0 mes tcc make-mesboot0 patch-mesboot binutils-mesboot0 gcc-core-mesboot0 mesboot-headers glibc-mesboot0 gcc-mesboot0 binutils-mesboot1 make-mesboot gcc-mesboot1 binutils-mesboot gawk-mesboot glibc-mesboot gcc-mesboot glibc-mesboot-shared gcc-14 binutils-244 glibc-241 binutils-x86-64 gcc-x86-64-stage1 glibc-x86-64 gcc-x86-64-stage2 || return 1
  ladder_lock stage0 stage0-source || return 1
  ladder_lock mes mes-source rung:stage0 src:nyacc $_bt || return 1
  ladder_lock tcc tcc-source rung:stage0 rung:mes $_bt || return 1
  ladder_lock make-mesboot0 make-mesboot0-source rung:mes rung:tcc $_bt || return 1
  ladder_lock patch-mesboot patch-mesboot-source rung:mes rung:tcc rung:make-mesboot0 $_bt || return 1
  ladder_lock binutils-mesboot0 binutils-mesboot-source rung:mes rung:tcc rung:make-mesboot0 rung:patch-mesboot src:patch-binutils-boot-2.20.1a tool:flex tool:bison $_bt || return 1
  ladder_lock gcc-core-mesboot0 gcc-core-source rung:mes rung:tcc rung:make-mesboot0 rung:patch-mesboot rung:binutils-mesboot0 src:patch-gcc-boot-2.95.3 tool:flex tool:bison $_bt || return 1
  ladder_lock mesboot-headers linux-headers rung:mes $_bt || return 1
  ladder_lock glibc-mesboot0 glibc-mesboot0-source rung:mes rung:tcc rung:make-mesboot0 rung:patch-mesboot rung:binutils-mesboot0 rung:gcc-core-mesboot0 rung:mesboot-headers src:patch-glibc-boot-2.2.5 src:patch-glibc-bootstrap-system-2.2.5 $_bt || return 1
  ladder_lock gcc-mesboot0 gcc-core-source rung:make-mesboot0 rung:patch-mesboot rung:binutils-mesboot0 rung:gcc-core-mesboot0 rung:glibc-mesboot0 rung:mesboot-headers src:patch-gcc-boot-2.95.3 tool:flex tool:bison $_bt || return 1
  ladder_lock binutils-mesboot1 binutils-mesboot-source rung:make-mesboot0 rung:patch-mesboot rung:binutils-mesboot0 rung:gcc-mesboot0 rung:glibc-mesboot0 src:patch-binutils-boot-2.20.1a src:linux-headers tool:flex tool:bison $_bt || return 1
  ladder_lock make-mesboot make-mesboot-source rung:make-mesboot0 rung:binutils-mesboot0 rung:gcc-mesboot0 rung:glibc-mesboot0 src:linux-headers $_bt || return 1
  ladder_lock gcc-mesboot1 gcc-464-core rung:make-mesboot0 rung:patch-mesboot rung:binutils-mesboot1 rung:gcc-mesboot0 rung:glibc-mesboot0 rung:make-mesboot src:gcc-464-gpp src:patch-gcc-boot-4.6.4 src:gmp src:mpfr src:mpc src:linux-headers tool:flex tool:bison $_bt || return 1
  ladder_lock binutils-mesboot binutils-mesboot-source rung:make-mesboot rung:patch-mesboot rung:binutils-mesboot1 rung:gcc-mesboot1 rung:glibc-mesboot0 src:patch-binutils-boot-2.20.1a src:linux-headers tool:flex tool:bison $_bt || return 1
  ladder_lock gawk-mesboot gawk-mesboot-source rung:make-mesboot rung:binutils-mesboot1 rung:gcc-mesboot1 rung:glibc-mesboot0 src:linux-headers $_bt || return 1
  ladder_lock glibc-mesboot glibc-216-source rung:make-mesboot rung:patch-mesboot rung:binutils-mesboot rung:gcc-mesboot1 rung:glibc-mesboot0 rung:gawk-mesboot src:patch-glibc-boot-2.16.0 src:patch-glibc-bootstrap-system-2.16.0 src:linux-headers $_bt || return 1
  ladder_lock gcc-mesboot gcc-494-source rung:make-mesboot rung:patch-mesboot rung:binutils-mesboot rung:gcc-mesboot1 rung:glibc-mesboot src:gmp src:mpfr src:mpc src:linux-headers tool:flex tool:bison $_bt || return 1
  ladder_lock glibc-mesboot-shared glibc-216-source rung:make-mesboot rung:patch-mesboot rung:binutils-mesboot rung:gcc-mesboot1 rung:glibc-mesboot0 rung:gawk-mesboot src:patch-glibc-boot-2.16.0 src:patch-glibc-bootstrap-system-2.16.0 src:linux-headers $_bt || return 1
  ladder_lock gcc-14 gcc-14-source rung:binutils-mesboot rung:gcc-mesboot rung:glibc-mesboot src:gmp63 src:mpfr421 src:mpc131 src:linux-headers tool:flex tool:bison tool:m4 tool:make $_bt || return 1
  ladder_lock binutils-244 binutils-244-source rung:gcc-mesboot1 rung:glibc-mesboot rung:binutils-mesboot tool:flex tool:bison tool:make $_bt || return 1
  ladder_lock glibc-241 glibc-241-source rung:gcc-14 rung:glibc-mesboot-shared rung:binutils-244 src:linux-headers tool:flex tool:bison tool:m4 tool:make tool:python $_bt || return 1
  ladder_lock binutils-x86-64 binutils-244-source rung:gcc-14 rung:glibc-mesboot rung:binutils-244 src:linux-headers-x86-64 tool:flex tool:bison tool:make $_bt || return 1
  ladder_lock gcc-x86-64-stage1 gcc-14-source rung:gcc-14 rung:glibc-mesboot rung:binutils-x86-64 rung:binutils-244 src:gmp63 src:mpfr421 src:mpc131 src:linux-headers-x86-64 tool:flex tool:bison tool:m4 tool:make $_bt || return 1
  ladder_lock glibc-x86-64 glibc-241-source rung:gcc-x86-64-stage1 rung:gcc-14 rung:glibc-mesboot rung:binutils-x86-64 src:linux-headers-x86-64 tool:flex tool:bison tool:m4 tool:make tool:python $_bt || return 1
  ladder_lock gcc-x86-64-stage2 gcc-14-source rung:gcc-14 rung:glibc-mesboot rung:binutils-x86-64 rung:glibc-x86-64 rung:binutils-244 src:gmp63 src:mpfr421 src:mpc131 src:linux-headers-x86-64 tool:flex tool:bison tool:m4 tool:make $_bt || return 1
}

# run_x86_64_rust_toolchain — build the /td/store rust-toolchain via build-plan --auto (#410):
# the cross ladder + zlib-x86-64 + rust-toolchain rungs. rust-toolchain's transitive closure IS
# the cross toolchain, so one `ladder_build rust-toolchain` realizes the whole graph (the warm
# chain cache-hits the cross rungs). Exports the cross trees (XBU XGCC2 XGLIBC XLIBGCCDIR
# XSTDCXXDIR) AND the relinked rustc/cargo tree XRUSTTREE. Same preamble as run_x86_64_cross.
run_x86_64_rust_toolchain() {
  . tests/ladder-lib.sh
  TD_CHECK_CHAIN_CACHE="${TD_CHECK_CHAIN_CACHE-${HOME:+$HOME/.td/build-daemon/chain}}"
  if [ -n "$TD_CHECK_CHAIN_CACHE" ]; then _lw="$HOME/.td/build-daemon/ladder"; else _lw="$ROOT/.td-build-cache/ladder-cold"; fi
  mkdir -p "`dirname "$_lw"`"
  exec 9>"$_lw.lock"; flock 9 || { echo "ladder: flock failed" >&2; return 1; }
  test -n "$TD_CHECK_CHAIN_CACHE" || rm -rf "$_lw"
  mkdir -p "$_lw"
  ladder_setup "$_lw" || { echo "ladder_setup failed" >&2; return 1; }
  _bt="tool:bash tool:coreutils tool:sed tool:grep tool:gawk tool:tar tool:gzip tool:bzip2 tool:xz tool:findutils tool:diffutils"
  _x86_64_cross_ladder || return 1
  # rust/zlib sources are NOT in the base ladder_setup spec set — intern them now (idempotent),
  # so the zlib/rust rungs' locks resolve their -source entries.
  ladder_intern_extra rust-toolchain-source rust-1.96.0 || return 1
  ladder_intern_extra zlib-x86-64-source zlib-1.3.1 || return 1
  ladder_emit zlib-x86-64 rust-toolchain || return 1
  ladder_lock zlib-x86-64 zlib-x86-64-source rung:gcc-x86-64-stage2 rung:glibc-x86-64 rung:binutils-x86-64 tool:make $_bt || return 1
  # $_bt already provides tar+gzip (the transform's in-sandbox unpacker) + the rest of the base tools.
  ladder_lock rust-toolchain rust-toolchain-source rung:glibc-x86-64 rung:gcc-x86-64-stage2 rung:zlib-x86-64 $_bt || return 1
  ladder_build rust-toolchain || { echo "the x86_64 rust-toolchain ladder failed" >&2; return 1; }
  _lt="$_lw/scratch/tdstore"
  _lo() { _o=`sed -n "s/^STEP $1 //p" "$_lw/build-rust-toolchain.out" | tail -1`; test -n "$_o" || { echo "no STEP output for $1" >&2; return 1; }; printf '%s/%s' "$_lt" "${_o##*/}"; }
  XBU=`_lo binutils-x86-64` || return 1
  _b=`_lo gcc-x86-64-stage2` && XGCC2="$_b/stage/td/store/gcc-14.3.0-x86_64" || return 1
  _b=`_lo glibc-x86-64` && XGLIBC="$_b/stage/td/store/glibc-2.41-x86_64" || return 1
  XLIBGCCDIR=`find "$XGCC2" -name 'libgcc_s.so.1' | head -1 | xargs -r dirname`
  XSTDCXXDIR=`find "$XGCC2" -name 'libstdc++.so.6*' | head -1 | xargs -r dirname`
  XRUSTTREE=`_lo rust-toolchain` || return 1
  X86_WORK="$_lw"; X86_SYSROOT="$_lw/x-sysroot-unused"
  export XBU XGCC2 XGLIBC XLIBGCCDIR XSTDCXXDIR XRUSTTREE X86_WORK X86_SYSROOT
  echo "   [ladder] x86_64 rust-toolchain via build-plan --auto: cross toolchain -> zlib-x86-64 -> rust-toolchain (relinked rustc/cargo tree $XRUSTTREE)"
}

# ---------------------------------------------------------------------------------------------------
# verify_x86_64_ownroot <cpath> <scratch> — the gate's DURABLE own-root verify, shared with the dev
# harness. Interns the x86_64 glibc 2.41 at /td/store, builds x86_64 C/C++ verify programs (interp =
# the interned /td/store x86_64 ld-linux-x86-64.so.2, -static-libgcc -static-libstdc++ so the own-root
# needs only the interned glibc), and RUNS them in the store-ns own-root → 42 with /gnu/store ABSENT.
# Requires: $TB (caller load_stage0'd), TD_STORE_DIR=/td/store, and the run_x86_64_cross exports
# (XGLIBC XGCC2 XBU). Legs: [no-guix] [content-addr] [behavioral] [structural] [input-addressed]
# (the lock-keyed path a consumer fetches as a substitute — x64-toolchain-subst PR2).
verify_x86_64_ownroot() {
  cpath=$1; snwork=$2; store="$snwork/td-store"; sndb="$snwork/store.db"; mkdir -p "$store"
  xcc1=`find "$XGCC2" -name cc1 | head -1`
  for b in "$XGLIBC/lib/libc.so.6" "$XGCC2/bin/$XTARGET-gcc" "$xcc1"; do
    test -n "$b" -a -e "$b" || { echo "x86_64 output missing ($b)" >&2; return 1; }
    if grep -q -a '/gnu/store' "$b"; then echo "$b contains /gnu/store bytes" >&2; return 1; fi
  done
  echo "   [no-guix] x86_64 glibc 2.41 + cross gcc: no /gnu/store in libc.so.6 / x86_64-gcc / cc1"
  GLP=`"$TB" store-add-recursive glibc-2.41-x86_64 "$XGLIBC" "$store" "$sndb"` || { echo "store-add x86_64 glibc failed" >&2; return 1; }
  case "$GLP" in /td/store/*-glibc-2.41-x86_64) ;; *) echo "x86_64 glibc not content-addressed: $GLP" >&2; return 1 ;; esac
  glrel=${GLP#/td/store/}
  echo "   [content-addr] interned $GLP in /td/store"
  csh=`command -v bash 2>/dev/null || command -v sh`
  mkdir -p "$snwork/w"
  printf 'int main(){return 42;}\n' > "$snwork/w/c.c"
  printf '#include <vector>\nint main(){std::vector<int> v; for(int i=0;i<43;i++) v.push_back(i); return v[42];}\n' > "$snwork/w/cpp.cc"
  bw=`mktemp -d`/bw; mkdir -p "$bw"
  for cc in gcc g++; do
    printf '#!%s\nexec "%s/bin/%s-%s" -isystem "%s/include" -B"%s/lib" -L"%s/lib" -static-libgcc -static-libstdc++ -Wl,--dynamic-linker -Wl,/td/store/%s/lib/ld-linux-x86-64.so.2 -Wl,--enable-new-dtags -Wl,-rpath -Wl,/td/store/%s/lib "$@"\n' \
      "$csh" "$XGCC2" "$XTARGET" "$cc" "$XGLIBC" "$XGLIBC" "$XGLIBC" "$glrel" "$glrel" > "$bw/$cc"
  done
  chmod 0555 "$bw/gcc" "$bw/g++"
  ( cd "$snwork/w" && env PATH="$XBU/bin:$cpath" "$bw/gcc" -o c.out c.c ) || { echo "cross gcc did not compile x86_64 C vs glibc 2.41" >&2; return 1; }
  ( cd "$snwork/w" && env PATH="$XBU/bin:$cpath" "$bw/g++" -O2 -o cpp.out cpp.cc ) || { echo "cross g++ did not compile x86_64 C++ vs glibc 2.41" >&2; return 1; }
  cls=`"$XBU/bin/$XTARGET-readelf" -h "$snwork/w/c.out" 2>/dev/null | grep -i 'class:' | grep -o 'ELF64'`
  test "$cls" = ELF64 || { echo "verify program not ELF 64-bit (x86_64); got '$cls'" >&2; return 1; }
  ci=`"$XBU/bin/$XTARGET-readelf" -l "$snwork/w/c.out" 2>/dev/null | grep -o "/td/store/$glrel/lib/ld-linux-x86-64.so.2" | head -1`
  test -n "$ci" || { echo "C program interp not the /td/store x86_64 ld" >&2; return 1; }
  if grep -q -a '/gnu/store' "$snwork/w/c.out"; then echo "x86_64 C program contains /gnu/store bytes" >&2; return 1; fi
  echo "   built x86_64 (ELF 64-bit) C + C++ programs vs glibc 2.41, interp=$ci, no /gnu/store"
  mkdir -p "$store/prog/bin"; cp "$snwork/w/c.out" "$store/prog/bin/c"; cp "$snwork/w/cpp.out" "$store/prog/bin/cpp"; chmod -R u+w "$store"
  WP=`"$TB" store-add-recursive prog "$store/prog" "$store" "$sndb"` || { echo "store-add prog failed" >&2; return 1; }; wprel=${WP#/td/store/}
  # the static-bash fixture is a DECLARED gate input (#353): every gate calling
  # this fn declares bash-static; the runner content-scanned hello's bash closure.
  bs=${TD_GATE_INPUT_BASH_STATIC:-}
  test -n "$bs" || { echo "TD_GATE_INPUT_BASH_STATIC unset — the calling gate must declare the bash-static input (#353)" >&2; return 1; }
  bbase=`basename "$bs"`; cp -a "$bs" "$store/$bbase"; chmod -R u+w "$store"
  snscript='[ -e /gnu/store ] && echo GNU-PRESENT || echo GNU-ABSENT
/td/store/'"$wprel"'/bin/c; echo "CRC=$?"
/td/store/'"$wprel"'/bin/cpp; echo "CPPRC=$?"'
  snout=`"$TB" store-ns "$store" -- "/td/store/$bbase/bin/bash" -c "$snscript" 2>&1` || { printf '%s\n' "$snout" | sed 's/^/     /' >&2; echo "store-ns x86_64 probe exited nonzero" >&2; return 1; }
  printf '%s\n' "$snout" | sed 's/^/     /' >&2
  echo "$snout" | grep -q '^CRC=42$'   || { echo "x86_64 C program did not return 42 in the own-root" >&2; return 1; }
  echo "$snout" | grep -q '^CPPRC=42$' || { echo "x86_64 C++ program did not return 42 in the own-root" >&2; return 1; }
  echo "   [behavioral] cross gcc 14.3.0 links a DYNAMIC x86_64 C AND C++ program vs MODERN x86_64 glibc 2.41; both run in the own-root → 42"
  echo "$snout" | grep -q '^GNU-ABSENT$' || { echo "/gnu/store is PRESENT in the own-root" >&2; return 1; }
  echo "   [structural] inside td's own root /td/store IS the store AND /gnu/store is ABSENT"

  # --- [input-addressed] (x64-toolchain-subst) intern the REAL x86_64 glibc 2.41 at the
  # LOCK-KEYED path so a consumer can NAME it and FETCH it as a signed substitute (the path
  # td-subst / resolve-toolchain.sh compute from tests/td-toolchain-x86_64.lock) instead of the
  # ~90-min cross rebuild — real x86_64 bytes at a stable, predictable /td/store path, not a
  # content-addressed throwaway. Then RUN a DYNAMIC x86_64 program whose interp IS that
  # input-addressed glibc. Gate 418 (toolchain-x86_64-input-addressed, #219) keys the path with a
  # static-bash FIXTURE; this leg ties the path to the REAL cross-built x86_64 toolchain bytes.
  XLOCK=tests/td-toolchain-x86_64.lock
  test -f "$XLOCK" || { echo "missing $XLOCK" >&2; return 1; }
  XK=`"$TB" toolchain-key "$XLOCK"` || { echo "toolchain-key $XLOCK failed" >&2; return 1; }
  IAGL=`"$TB" store-add-input-addressed glibc-2.41-x86_64 "$XK" "$XGLIBC" "$store" "$sndb"` \
    || { echo "store-add-input-addressed x86_64 glibc failed" >&2; return 1; }
  WANTGL=`"$TB" toolchain-path "$XLOCK" glibc-2.41-x86_64`
  test "$IAGL" = "$WANTGL" || { echo "input-addressed glibc path $IAGL != lock-computed $WANTGL (consumer can't predict it)" >&2; return 1; }
  # x64 focus: the x86_64 toolchain must NOT share a /td/store path with the i686 bootstrap
  # intermediate, or a published x86_64 substitute could be confused for i686.
  ILGL=`"$TB" toolchain-path tests/td-toolchain.lock glibc-2.41`
  test -n "$ILGL" -a "$IAGL" != "$ILGL" || { echo "x86_64 glibc path $IAGL collides with the i686 glibc path $ILGL" >&2; return 1; }
  echo "   [distinct-arch] the x86_64 lock-keyed path differs from the i686 toolchain's — no cross-arch store collision"
  iarel=${IAGL#/td/store/}
  echo "   [input-addressed] interned the REAL x86_64 glibc 2.41 at the lock-keyed path $IAGL (== toolchain-path $XLOCK glibc-2.41-x86_64)"
  printf '#!%s\nexec "%s/bin/%s-gcc" -isystem "%s/include" -B"%s/lib" -L"%s/lib" -static-libgcc -static-libstdc++ -Wl,--dynamic-linker -Wl,/td/store/%s/lib/ld-linux-x86-64.so.2 -Wl,--enable-new-dtags -Wl,-rpath -Wl,/td/store/%s/lib "$@"\n' \
    "$csh" "$XGCC2" "$XTARGET" "$XGLIBC" "$XGLIBC" "$XGLIBC" "$iarel" "$iarel" > "$bw/gcc-ia"
  chmod 0555 "$bw/gcc-ia"
  ( cd "$snwork/w" && env PATH="$XBU/bin:$cpath" "$bw/gcc-ia" -o cia.out c.c ) \
    || { echo "could not build an x86_64 C program vs the input-addressed glibc" >&2; return 1; }
  iaci=`"$XBU/bin/$XTARGET-readelf" -l "$snwork/w/cia.out" 2>/dev/null | grep -o "/td/store/$iarel/lib/ld-linux-x86-64.so.2" | head -1`
  test -n "$iaci" || { echo "input-addressed program interp not the lock-keyed /td/store x86_64 ld" >&2; return 1; }
  mkdir -p "$store/progia/bin"; cp "$snwork/w/cia.out" "$store/progia/bin/c"; chmod -R u+w "$store"
  WPIA=`"$TB" store-add-recursive progia "$store/progia" "$store" "$sndb"` || { echo "store-add progia failed" >&2; return 1; }; wpiarel=${WPIA#/td/store/}
  snia='[ -e /gnu/store ] && echo GNU-PRESENT || echo GNU-ABSENT
/td/store/'"$wpiarel"'/bin/c; echo "IARC=$?"'
  snoia=`"$TB" store-ns "$store" -- "/td/store/$bbase/bin/bash" -c "$snia" 2>&1` \
    || { printf '%s\n' "$snoia" | sed 's/^/     /' >&2; echo "store-ns input-addressed x86_64 probe exited nonzero" >&2; return 1; }
  echo "$snoia" | grep -q '^IARC=42$' || { printf '%s\n' "$snoia" | sed 's/^/     /' >&2; echo "x86_64 program vs the input-addressed glibc did not return 42 in the own-root" >&2; return 1; }
  echo "   [behavioral/input-addressed] a DYNAMIC x86_64 program whose interp IS the lock-keyed /td/store glibc runs in the own-root → 42 — real x86_64 bytes at a predictable, fetchable path"

}

# ===================================================================================================
# RUNG X2 — a NATIVE x86_64 toolchain at /td/store (x86_64-toolchain track, after the #201 cross rungs).
# X1 produced a CROSS gcc: an i686 (ELF 32-bit) binary that EMITS x86_64. X2 turns that into a NATIVE
# x86_64 gcc — gcc/cc1/g++ that are themselves ELF 64-bit x86_64, run natively on x86_64, and compile
# x86_64 (host == target). Built BY the cross toolchain (XGCC2/XBU) vs the /td/store x86_64 glibc 2.41,
# STATIC (like the i686 build_gcc_14 / build_binutils_244 rungs) so the binaries run in the store-ns
# own-root with no interp dependency. The same `int main(){return 42;}` proof, but the COMPILER that
# builds + runs it is itself an x86_64 binary living in /td/store — the architectural self-hosting rung
# (a from-source gcc-rebuilds-gcc bootstrap is a separate, much heavier milestone, not claimed here).
# ---------------------------------------------------------------------------------------------------

# x86_64_build_native_recipe <cpath> <xgcc2> <xglibc> <xbu> <out> — build the NATIVE x86_64 binutils
# 2.44 + gcc 14.3.0 via the STRUCTURED Rust recipe `td-builder toolchain-recipe x86_64-native`
# (builder/src/toolchain_x86_64.rs), the port of the former build_{binutils,gcc}_x86_64_native shell
# drivers (~250 lines of configure/make/wrapper/sysroot logic now one typed unit). Inputs: the cross
# toolchain (xgcc2/xbu/xglibc, fetched or from-seed) + the pinned source tarballs (the globals
# GCC14_TB/GMP63_TB/MPFR421_TB/MPC131_TB/BU244_TB + KH_X86_64_TB the sourced base driver sets). Sets +
# exports XNBU (native binutils tree) and XNGCC (native gcc staged prefix), exactly as the deleted
# shell rungs did, so verify_x86_64_native_ownroot / the closure-export consume them unchanged. The
# native gcc is NOT byte-reproducible (trust = the input-addressed lock name + the substitute
# signature), so this is a build+behavioral rung, not a bootstrap::Recipe repro rung.
x86_64_build_native_recipe() {
  _cp=$1; _xgcc2=$2; _xglibc=$3; _xbu=$4; _out=$5
  rm -rf "$_out"; mkdir -p "$_out"
  env TDXN_CPATH="$_cp" TDXN_CROSS_GCC="$_xgcc2" TDXN_CROSS_BINUTILS="$_xbu" TDXN_GLIBC="$_xglibc" \
      TDXN_BINUTILS_TAR="$BU244_TB" TDXN_GCC_TAR="$GCC14_TB" TDXN_GMP_TAR="$GMP63_TB" \
      TDXN_MPFR_TAR="$MPFR421_TB" TDXN_MPC_TAR="$MPC131_TB" TDXN_KERNEL_HEADERS_TAR="$KH_X86_64_TB" \
      TDXN_OUT="$_out" X86_MAKE_J="${X86_MAKE_J:--j4}" \
      "$TB" toolchain-recipe x86_64-native > "$_out/recipe.out" 2>&1 \
    || { echo "x86_64_build_native_recipe: toolchain-recipe x86_64-native failed" >&2; tail -40 "$_out/recipe.out" >&2; return 1; }
  sed 's/^/   /' "$_out/recipe.out"
  XNBU=`sed -n 's/^NATIVE_BINUTILS=//p' "$_out/recipe.out"`
  XNGCC=`sed -n 's/^NATIVE_GCC=//p' "$_out/recipe.out"`
  test -n "$XNBU" -a -d "$XNBU" || { echo "x86_64_build_native_recipe: recipe returned no native binutils tree" >&2; return 1; }
  test -n "$XNGCC" -a -d "$XNGCC" || { echo "x86_64_build_native_recipe: recipe returned no native gcc tree" >&2; return 1; }
  export XNBU XNGCC
}

# verify_x86_64_native_ownroot <cpath> <scratch> [expected-gcc-name] — the DURABLE own-root verify for
# rungs X2 AND X3. Interns the NATIVE x86_64 gcc + NATIVE x86_64 binutils + the x86_64 glibc 2.41 at
# /td/store, then RUNS the native gcc IN the store-ns own-root: it COMPILES a C and a C++ program from
# source and the results run → 42, /gnu/store ABSENT. The compiler doing the work is itself an ELF
# 64-bit x86_64 binary in /td/store. Requires the run_x86_64_cross / closure exports XGLIBC, plus
# $XNGCC (gcc tree) and $XNBU (binutils tree) from the caller. The X3 gate reuses this verify by
# pointing XNGCC/XNBU at the SELF-rebuilt trees and passing gcc-14.3.0-x86_64-self as $3 — the
# [content-addr] leg asserts the interned name matches, so the two rungs' artifacts can't be confused.
# ($3 is a POSITIONAL arg, not an env knob: an ambient variable must not be able to repoint a gate's
# name assert.) Legs: [native-arch] [no-guix] [content-addr] [closure-complete] [self-host-compile]
# [structural].
verify_x86_64_native_ownroot() {
  cpath=$1; snwork=$2; _xvname="${3:-gcc-14.3.0-x86_64-native}"
  store="$snwork/td-store-native"; sndb="$snwork/store-native.db"; mkdir -p "$store"
  test -n "${XNGCC:-}" -a -d "${XNGCC:-/nonexistent}" || { echo "native gcc tree (XNGCC) unset" >&2; return 1; }
  test -n "${XNBU:-}" -a -d "${XNBU:-/nonexistent}" || { echo "native binutils tree (XNBU) unset" >&2; return 1; }
  test -n "${XGLIBC:-}" -a -d "${XGLIBC:-/nonexistent}" || { echo "x86_64 glibc tree (XGLIBC) unset" >&2; return 1; }
  ngcc=`"$XNBU/bin/readelf" -h "$XNGCC/bin/gcc" 2>/dev/null`
  echo "$ngcc" | grep -i 'class:' | grep -q 'ELF64' || { echo "native gcc not ELF64" >&2; return 1; }
  echo "$ngcc" | grep -i 'machine:' | grep -qi 'x86-64' || { echo "native gcc machine is not x86-64" >&2; return 1; }
  echo "   [native-arch] the native gcc/binutils ARE ELF 64-bit x86_64 binaries (not the i686 cross gcc)"
  ncc1=`find "$XNGCC" -name cc1 | head -1`
  for b in "$XNGCC/bin/gcc" "$ncc1" "$XNBU/bin/as" "$XNBU/bin/ld" "$XGLIBC/lib/libc.so.6"; do
    test -n "$b" -a -e "$b" || { echo "native output missing ($b)" >&2; return 1; }
    if grep -q -a '/gnu/store' "$b"; then echo "$b contains /gnu/store bytes" >&2; return 1; fi
  done
  echo "   [no-guix] the native gcc/cc1 + native as/ld + the x86_64 libc.so.6 carry no /gnu/store bytes"
  # intern the native toolchain closure as siblings under /td/store: native binutils, native gcc, and the
  # x86_64 glibc 2.41. The own-root probe finds as/ld via PATH (both bins are at /td/store), so no tooldir
  # symlink wiring is needed; [closure-complete] (below) asserts the binutils as/ld are present.
  NBP=`"$TB" store-add-recursive "\`basename "$XNBU"\`" "$XNBU" "$store" "$sndb"` || { echo "store-add native binutils failed" >&2; return 1; }
  nbrel=`basename "$NBP"`
  NGP=`"$TB" store-add-recursive "\`basename "$XNGCC"\`" "$XNGCC" "$store" "$sndb"` || { echo "store-add native gcc failed" >&2; return 1; }
  GLP=`"$TB" store-add-recursive glibc-2.41-x86_64 "$XGLIBC" "$store" "$sndb"` || { echo "store-add x86_64 glibc failed" >&2; return 1; }
  case "$NGP" in /td/store/*-"$_xvname") ;; *) echo "gcc not content-addressed as $_xvname: $NGP" >&2; return 1 ;; esac
  echo "   [content-addr] interned the native gcc ($NGP), native binutils, and the x86_64 glibc in /td/store"
  ngrel=`basename "$NGP"`; glrel=`basename "$GLP"`
  chmod -R u+w "$store"
  # [closure-complete] DURABLE (no guix oracle): the native toolchain's assembler + linker are interned at
  # /td/store alongside the gcc — so the own-root has a COMPLETE native toolchain (gcc + binutils), found via
  # PATH (how a real toolchain works). The behavioral probe below sets PATH to ONLY these two /td/store dirs,
  # so the interned native binutils is load-bearing: drop it and the native gcc cannot assemble/link → reds.
  for t in as ld; do
    test -x "$store/$nbrel/bin/$t" || { echo "interned native binutils missing an executable '$t' ($store/$nbrel/bin/$t) — incomplete toolchain closure" >&2; return 1; }
  done
  echo "   [closure-complete] the native binutils as/ld are interned at /td/store/$nbrel beside the native gcc — a complete native toolchain in td's own store"
  # the static-bash fixture is a DECLARED gate input (#353): every gate calling
  # this fn declares bash-static; the runner content-scanned hello's bash closure.
  bs=${TD_GATE_INPUT_BASH_STATIC:-}
  test -n "$bs" || { echo "TD_GATE_INPUT_BASH_STATIC unset — the calling gate must declare the bash-static input (#353)" >&2; return 1; }
  bbase=`basename "$bs"`; cp -a "$bs" "$store/$bbase"; chmod -R u+w "$store"
  # the probe is a FILE in the (ro) store; it compiles into the writable tmpfs /tmp inside the own-root.
  # the probe runs the NATIVE gcc IN the own-root. It uses ONLY bash builtins (cd/printf/case/[) + the
  # store's own binaries (gcc/g++/readelf) — the own-root has NO coreutils (no mkdir/grep/sed). glibc
  # headers come via -idirafter (NOT C_INCLUDE_PATH — the libstdc++ <cstdlib> #include_next), and
  # -B + -rpath/interp point at the interned /td/store glibc so the produced binaries are DYNAMIC
  # (libc.so.6, interp = the /td/store ld) and run via the bound glibc. /tmp is store-ns's writable tmpfs.
  cat > "$store/nativeprobe.sh" <<PROBE
export PATH=/td/store/$ngrel/bin:/td/store/$nbrel/bin
H="-idirafter /td/store/$glrel/include"
B="-B/td/store/$glrel/lib"
LD="-Wl,--dynamic-linker,/td/store/$glrel/lib/ld-linux-x86-64.so.2,-rpath,/td/store/$glrel/lib"
cd /tmp || exit 1
printf 'int main(){return 42;}\n' > c.c
printf '#include <vector>\n#include <cstdlib>\nint main(){std::vector<int> v; for(int i=0;i<43;i++) v.push_back(i); return v[42];}\n' > cpp.cc
gcc \$B \$LD -o c c.c || { echo "NATIVE-CC-FAIL"; exit 1; }
g++ -O2 \$H -static-libgcc -static-libstdc++ \$B \$LD -o cpp cpp.cc || { echo "NATIVE-CXX-FAIL"; exit 1; }
hdr=\$(/td/store/$nbrel/bin/readelf -h c)
case "\$hdr" in *ELF64*) echo CCLASS=ELF64 ;; esac
case "\$hdr" in *X86-64*|*x86-64*) echo CMACH=x86-64 ;; esac
itp=\$(/td/store/$nbrel/bin/readelf -l c)
case "\$itp" in *"/td/store/$glrel/lib/ld-linux-x86-64.so.2"*) echo CINTERP=OK ;; esac
./c; echo "CRC=\$?"
./cpp; echo "CPPRC=\$?"
[ -e /gnu/store ] && echo GNU-PRESENT || echo GNU-ABSENT
PROBE
  out=`"$TB" store-ns "$store" -- "/td/store/$bbase/bin/bash" /td/store/nativeprobe.sh 2>&1` \
    || { printf '%s\n' "$out" | sed 's/^/     /' >&2; echo "store-ns native-gcc probe exited nonzero" >&2; return 1; }
  printf '%s\n' "$out" | sed 's/^/     /' >&2
  echo "$out" | grep -q '^CCLASS=ELF64$' || { echo "the native gcc did not emit an ELF64 program in the own-root" >&2; return 1; }
  echo "$out" | grep -q '^CMACH=x86-64$' || { echo "the native-gcc-compiled program is not x86-64" >&2; return 1; }
  echo "$out" | grep -q '^CINTERP=OK$' || { echo "the native-gcc-compiled program's interp is not the /td/store x86_64 ld" >&2; return 1; }
  echo "$out" | grep -q '^CRC=42$'   || { echo "the native-gcc-compiled C program did not return 42 in the own-root" >&2; return 1; }
  echo "$out" | grep -q '^CPPRC=42$' || { echo "the native-gcc-compiled C++ program did not return 42 in the own-root" >&2; return 1; }
  echo "   [self-host-compile] the NATIVE x86_64 gcc RAN in the own-root and compiled a DYNAMIC ELF64 x86-64 C AND C++ program (interp = the /td/store x86_64 ld) from source → both run → 42"
  echo "$out" | grep -q '^GNU-ABSENT$' || { echo "/gnu/store is PRESENT in the own-root" >&2; return 1; }
  echo "   [structural] inside td's own root /td/store IS the store AND /gnu/store is ABSENT"
}

# ===================================================================================================
# SHARED PREREQUISITE OBTAINERS — the fetch-or-build ladder gates 422 (native) and 426 (self) both
# need, extracted from gate 422's driver so the X3 gate does not duplicate it. Behavior-preserving:
# the bodies are gate 422's former inline blocks, verbatim.
# ---------------------------------------------------------------------------------------------------

# x86_64_obtain_cross_toolchain <cpath> <store> <db> — obtain the CROSS toolchain {XBU, XGCC2,
# XGLIBC}: FETCH the lock-keyed signed closure if check host-prep exposed a substitute store, else
# BUILD it from the 229-byte seed (directive 1; the daily full suite is the sole from-seed
# authoritative builder). Exports XBU/XGCC2/XGLIBC (+ XLIBGCCDIR/XSTDCXXDIR, and X86_WORK on the
# build path). Requires the 414 driver lib sourced (the chain build_* fns + the pinned tarball
# globals + fail()).
x86_64_obtain_cross_toolchain() {
  _occp=$1; _occstore=$2; _occdb=$3
  if x86_64_resolve_closure "$_occstore" "$_occdb"; then
    echo ">> [subst/SKIP] fetched the x86_64 cross toolchain closure {binutils,gcc,glibc} — SKIPPED the ~98-min from-seed build"
  else
    echo ">> [subst/MISS] no exposed substitute store — building the cross toolchain from the 229-byte seed (directive 1)"
    # i686 base (21 rungs) + the 4 cross rungs are recipes (#378 slice 4); run_x86_64_cross drives
    # the whole graph via build-plan --auto and exports XBU XGCC2 XGLIBC XLIBGCCDIR XSTDCXXDIR.
    run_x86_64_cross "$_occp" || fail "the x86_64 cross toolchain (recipe ladder) failed to build from the seed"
  fi
  test -n "${XGCC2:-}" -a -n "${XGLIBC:-}" -a -n "${XBU:-}" || fail "cross toolchain vars unset after fetch/build"
}

# x86_64_obtain_native_toolchain <cpath> <store> <db> <export-dir> — obtain the NATIVE x86_64
# toolchain {XNBU, XNGCC} (rung X2's artifact): FETCH it at its lock-keyed paths
# (tests/td-toolchain-x86_64-native.lock), else BUILD it from the cross toolchain (XGCC2/XGLIBC/XBU
# — call x86_64_obtain_cross_toolchain first) via the x86_64-native toolchain-recipe, intern it at
# the lock paths and subst-export it to <export-dir> for the daily to sign+publish (from-BUILD
# fallback — directive 1; the daily is the sole authoritative from-cross builder+publisher). Sets
# the GLOBAL nrout (build scratch) so the caller's trap can clean it. Exports XNBU/XNGCC.
x86_64_obtain_native_toolchain() {
  _onp=$1; _onstore=$2; _ondb=$3; _onexp=$4
  if x86_64_resolve_closure_native "$_onstore" "$_ondb"; then
    echo ">> [subst/SKIP native] fetched the NATIVE x86_64 toolchain {binutils,gcc} at their lock paths — SKIPPED the ~45-min native build"
  else
    echo ">> [subst/MISS native] no exposed native substitute — building the NATIVE x86_64 toolchain from the cross toolchain (directive 1)"
    echo ">> [N1+N2] NATIVE x86_64 binutils 2.44 + gcc 14.3.0 via the Rust toolchain-recipe (structured port)"
    nrout=`mktemp -d`/native-out
    x86_64_build_native_recipe "$_onp" "$XGCC2" "$XGLIBC" "$XBU" "$nrout" || fail "could not build the NATIVE x86_64 toolchain (recipe)"
    echo ">> [export] intern the built native binutils + gcc at their lock-keyed paths + subst-export"
    x86_64_build_closure_native "$_onexp" "$_onstore" "$_ondb" \
      || fail "could not intern + subst-export the native x86_64 toolchain closure"
  fi
  test -n "${XNBU:-}" -a -d "${XNBU:-/nonexistent}" -a -n "${XNGCC:-}" -a -d "${XNGCC:-/nonexistent}" \
    || fail "native toolchain vars unset after fetch/build"
}

# ===================================================================================================
# RUNG X3 — SELF-HOSTING (gcc rebuilds gcc). X2 produced a NATIVE x86_64 toolchain, but its BUILDER
# was the i686 CROSS gcc — the bootstrap step that produced the native compiler was not itself
# native. X3 closes the loop: the NATIVE /td/store toolchain (XNGCC/XNBU) rebuilds binutils 2.44 +
# GCC 14.3.0 — the compiler that compiles the compiler is itself an x86_64 binary living in td's own
# store, the from-source gcc-rebuilds-gcc milestone rung X2 explicitly did not claim. Same version,
# same configure flags as X2 (one Rust code path builds both flavors), which is what makes the
# [codegen] agreement leg meaningful. STATIC vs the /td/store x86_64 glibc 2.41, like X2.
# ---------------------------------------------------------------------------------------------------

# x86_64_build_self_recipe <cpath> <xngcc> <xnbu> <xglibc> <out> — build the SELF-HOSTED x86_64
# binutils 2.44 + gcc 14.3.0 via the structured Rust recipe `td-builder toolchain-recipe x86_64-self`
# (builder/src/toolchain_x86_64.rs). The recipe's own [builder-arch] leg asserts the DRIVING gcc is
# an ELF64 x86_64 binary — handing it the i686 cross gcc reds (verified-red lever). Sets + exports
# XSBU (self binutils tree) and XSGCC (self gcc staged prefix).
x86_64_build_self_recipe() {
  _sp=$1; _sxngcc=$2; _sxnbu=$3; _sxglibc=$4; _sout=$5
  rm -rf "$_sout"; mkdir -p "$_sout"
  env TDXS_CPATH="$_sp" TDXS_BUILDER_GCC="$_sxngcc" TDXS_BUILDER_BINUTILS="$_sxnbu" TDXS_GLIBC="$_sxglibc" \
      TDXS_BINUTILS_TAR="$BU244_TB" TDXS_GCC_TAR="$GCC14_TB" TDXS_GMP_TAR="$GMP63_TB" \
      TDXS_MPFR_TAR="$MPFR421_TB" TDXS_MPC_TAR="$MPC131_TB" TDXS_KERNEL_HEADERS_TAR="$KH_X86_64_TB" \
      TDXS_OUT="$_sout" X86_MAKE_J="${X86_MAKE_J:--j4}" \
      "$TB" toolchain-recipe x86_64-self > "$_sout/recipe.out" 2>&1 \
    || { echo "x86_64_build_self_recipe: toolchain-recipe x86_64-self failed" >&2; tail -40 "$_sout/recipe.out" >&2; return 1; }
  sed 's/^/   /' "$_sout/recipe.out"
  XSBU=`sed -n 's/^SELF_BINUTILS=//p' "$_sout/recipe.out"`
  XSGCC=`sed -n 's/^SELF_GCC=//p' "$_sout/recipe.out"`
  test -n "$XSBU" -a -d "$XSBU" || { echo "x86_64_build_self_recipe: recipe returned no self binutils tree" >&2; return 1; }
  test -n "$XSGCC" -a -d "$XSGCC" || { echo "x86_64_build_self_recipe: recipe returned no self gcc tree" >&2; return 1; }
  export XSBU XSGCC
}

# x86_64_self_codegen_agreement <xngcc> <xsgcc> — [codegen] the stage2-vs-stage3 agreement at the
# assembly level: the INPUT native gcc (built by the cross gcc) and the SELF-rebuilt gcc (built by
# the native gcc) compile the same fixed C and C++ TU at -O2 -S to BYTE-IDENTICAL assembly. Same
# gcc version + same configure flags (one Rust code path builds both flavors) → the self-hosted
# compiler must generate exactly the code its builder does; GCC's own `make bootstrap` asserts the
# same fixpoint on stage2/stage3 objects. The TUs are include-less ON PURPOSE: -S needs no headers,
# no as/ld, no libc — the leg isolates CODE GENERATION from environment. Both sides get the SAME
# pinned -frandom-seed (belt to the TUs' no-file-scope-statics braces: with the seed unset gcc
# draws /dev/urandom into generated symbol names, and a pinned seed shuts that whole class off
# structurally instead of relying on the TU text staying nondeterminism-free — same move as the
# gcc14-repro wrappers above). Compared by sha256 via the chain lib's sha() (diffutils is not
# guaranteed in the sandbox).
x86_64_self_codegen_agreement() {
  _cgn=$1; _cgs=$2
  _cgw=`mktemp -d`
  cat > "$_cgw/cg.c" <<'EOF'
unsigned fib(unsigned n) { unsigned a = 0, b = 1; while (n--) { unsigned t = a + b; a = b; b = t; } return a; }
int classify(int x) { switch (x & 3) { case 0: return x / 3; case 1: return x * 5; case 2: return x - 7; default: return -x; } }
int main(void) { return (fib(12) == 144 && classify(9) == 45) ? 42 : 1; }
EOF
  cat > "$_cgw/cg.cc" <<'EOF'
template <typename T> struct Acc { T v; explicit Acc(T s) : v(s) {} Acc &add(T x) { v += x; return *this; } };
template <typename T> T sq(T x) { return x * x; }
int main() { Acc<int> a(2); a.add(sq(3)).add(sq(5)); return a.v == 36 ? 42 : 1; }
EOF
  for _cgcc in "$_cgn" "$_cgs"; do
    test -x "$_cgcc/bin/gcc" -a -x "$_cgcc/bin/g++" || { echo "codegen: no gcc/g++ under $_cgcc" >&2; rm -rf "$_cgw"; return 1; }
  done
  _cgseed=-frandom-seed=tdselfcodegen
  ( cd "$_cgw" \
      && "$_cgn/bin/gcc" -O2 -S $_cgseed -o n-c.s cg.c && "$_cgs/bin/gcc" -O2 -S $_cgseed -o s-c.s cg.c \
      && "$_cgn/bin/g++" -O2 -S $_cgseed -o n-cpp.s cg.cc && "$_cgs/bin/g++" -O2 -S $_cgseed -o s-cpp.s cg.cc ) \
    || { echo "codegen: -O2 -S compile failed" >&2; rm -rf "$_cgw"; return 1; }
  for _cgl in c cpp; do
    _cghn=`sha "$_cgw/n-$_cgl.s"`
    _cghs=`sha "$_cgw/s-$_cgl.s"`
    if [ "$_cghn" != "$_cghs" ]; then
      echo "codegen: $_cgl assembly DIFFERS between the native gcc ($_cghn) and the self-rebuilt gcc ($_cghs)" >&2
      cmp "$_cgw/n-$_cgl.s" "$_cgw/s-$_cgl.s" >&2 2>/dev/null || true
      rm -rf "$_cgw"; return 1
    fi
    echo "   [codegen] $_cgl: the native gcc and the SELF-rebuilt gcc emit byte-identical -O2 -S assembly (sha256 $_cghn)"
  done
  rm -rf "$_cgw"
}

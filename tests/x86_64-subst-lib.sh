# tests/x86_64-subst-lib.sh — the x86_64 toolchain FETCH SHORT-CIRCUIT (x64-toolchain-subst,
# human 2026-06-28: "the per-PR loop FETCHES the toolchain instead of building it from seed").
#
# The per-PR loop SKIPS the ~98-min from-seed cross build by FETCHING the toolchain CLOSURE — the 3
# `tests/td-toolchain-x86_64.lock` components {binutils-2.44-x86_64, gcc-14.3.0-x86_64,
# glibc-2.41-x86_64} — from a persistent signed substitute store (`~/.td/subst`, exposed by
# check.sh host-prep as TD_SUBST_BIN/TD_SUBST_STORE/TD_SUBST_PUBKEY), and falls back to from-seed on
# ANY miss (the substitute is an optimization, never a correctness dependency — directive 1 / the
# human-approved relaxation: the DAILY full suite stays the sole from-seed authoritative build +
# publisher). The cross gcc/binutils are built `-static` (static i686 binaries), so the closure does
# NOT need the i686 glibc-2.16 runtime — it is exactly these 3 x86_64 components.
#
# td-subst is NOT built here: gate 414 is not a BUILD_GATE, so it has no ts-eval sentinel (`ts-emit`
# would fail) and making it one drags in the whole corpus. Instead the DAILY (which has td-subst via
# build-recipes) stashes the td-subst binary into ~/.td/subst alongside the published closure, and
# check.sh host-prep exposes it. This lib only CONSUMES TD_SUBST_BIN.
#
# Toolchain-var convention: XBU/XGCC2/XGLIBC point at the PHYSICAL bytes ($store/<base>) for the
# host-side compile; the baked interp/RUNPATH use the LOGICAL /td/store/<base> (the store-ns binds
# $store at /td/store at run time). Identical whether the closure was BUILT+interned or FETCHED.

X86_CLOSURE_NAMES="binutils-2.44-x86_64 gcc-14.3.0-x86_64 glibc-2.41-x86_64"
X86_LOCK=tests/td-toolchain-x86_64.lock

# _x86_point NAME PHYS — repoint the toolchain vars at a placed closure component (physical path).
_x86_point() {
  case "$1" in
    binutils-*) XBU="$2" ;;
    gcc-*) XGCC2="$2"
      XLIBGCCDIR=`find "$2" -name 'libgcc_s.so.1' 2>/dev/null | head -1 | xargs -r dirname`
      XSTDCXXDIR=`find "$2" -name 'libstdc++.so.6' 2>/dev/null | head -1 | xargs -r dirname` ;;
    glibc-*) XGLIBC="$2" ;;
  esac
}

# x86_64_build_closure OUT STORE DB — for the from-seed BUILD path: intern the 3 BUILT closure trees
# (XBU/XGCC2/XGLIBC) at their lock-keyed input-addressed paths in a FRESH closure STORE, and
# subst-EXPORT each (NAR + td-native narinfo, td-builder only — no td-subst/key) to OUT for the daily
# to sign+publish. INTERLEAVED on purpose: `store-add-input-addressed` REWRITES DB to the single path
# it just added, so each component must be exported WHILE it is the DB root (before the next intern).
# A FRESH store also avoids a "File exists" double-intern with verify_x86_64_ownroot's own glibc copy.
# Repoints XBU/XGCC2/XGLIBC at the interned PHYSICAL paths for x86_64_verify_closure.
x86_64_build_closure() {
  _out=$1; _store=$2; _db=$3
  _k=`"$TB" toolchain-key "$X86_LOCK"` || { echo "build_closure: toolchain-key failed" >&2; return 1; }
  mkdir -p "$_out"
  for nm in $X86_CLOSURE_NAMES; do
    case "$nm" in binutils-*) src="$XBU" ;; gcc-*) src="$XGCC2" ;; glibc-*) src="$XGLIBC" ;; esac
    test -n "$src" -a -d "$src" || { echo "build_closure: no built tree for $nm ($src)" >&2; return 1; }
    p=`"$TB" store-add-input-addressed "$nm" "$_k" "$src" "$_store" "$_db"` \
      || { echo "build_closure: store-add-input-addressed $nm failed" >&2; return 1; }
    want=`"$TB" toolchain-path "$X86_LOCK" "$nm"`
    test "$p" = "$want" || { echo "build_closure: $nm path $p != lock-computed $want" >&2; return 1; }
    "$TB" subst-export "$_db" "$_store" "$_out" "$p" >/dev/null \
      || { echo "build_closure: subst-export $nm ($p) failed" >&2; return 1; }
    test -f "$_out/`basename "$p"`.narinfo" || { echo "build_closure: no narinfo for $nm" >&2; return 1; }
    _x86_point "$nm" "$_store/`basename "$p"`"
    echo "   [closure] $nm interned at lock-keyed $p + subst-exported (the daily signs + publishes it)"
  done
  export XBU XGCC2 XGLIBC XLIBGCCDIR XSTDCXXDIR
}

# x86_64_resolve_closure STORE DB — if a subst store is exposed (TD_SUBST_BIN+TD_SUBST_STORE),
# resolve ALL 3 closure components, restore them into STORE at their lock paths, and repoint
# XBU/XGCC2/XGLIBC. Return 0 only if ALL 3 HIT; any MISS / no subst configured -> 1 (build from seed).
x86_64_resolve_closure() {
  _store=$1; _db=$2
  [ -n "${TD_SUBST_BIN:-}" ] && [ -n "${TD_SUBST_STORE:-}" ] || return 1
  _shdir=`dirname "$(command -v sh)"`
  _cu=`grep -- '-coreutils-' tests/td-subst.lock | sed 's/^[^ ]* //' | head -1`
  _dest=`mktemp -d`
  for nm in $X86_CLOSURE_NAMES; do
    p=`env -i PATH="$_cu/bin:$_shdir" TD_BUILDER="$TB" TD_SUBST_BIN="$TD_SUBST_BIN" \
        TD_SUBST_STORE="$TD_SUBST_STORE" TD_SUBST_PUBKEY="${TD_SUBST_PUBKEY:-tests/td-subst.pub}" \
        TD_STORE_DIR=/td/store sh tools/resolve-toolchain.sh "$X86_LOCK" "$nm" "$_dest"` \
      || { rm -rf "$_dest"; return 1; }   # MISS on any component → fall back to from-seed
    base=`basename "$p"`
    rm -rf "$_store/$base"; cp -a "$_dest/$base" "$_store/$base"; chmod -R u+w "$_store/$base"
    _x86_point "$nm" "$_store/$base"
    echo "   [closure/fetch] $nm restored at /td/store/$base (ed25519 sig + StorePath==lock-path + NarHash verified)"
  done
  rm -rf "$_dest"
  export XBU XGCC2 XGLIBC XLIBGCCDIR XSTDCXXDIR
  return 0
}

# ============================================================================================
# NATIVE layer (rung X2): the same input-addressed FETCH short-circuit for the NATIVE x86_64
# toolchain {native binutils-2.44, native gcc-14.3.0} — the ELF64 --host=x86_64 linker a /td/store
# rust toolchain drives (#240). Keyed by tests/td-toolchain-x86_64-native.lock (gate 419 / #264),
# DISTINCT from the cross closure above. Lets a consumer (gate 422 native-gcc, gate 416 rust) FETCH
# the native toolchain instead of the ~45-min from-cross rebuild, with from-BUILD fallback (directive 1;
# the daily is the sole from-cross authoritative builder + publisher). Var convention: XNBU/XNGCC point
# at the PHYSICAL bytes ($store/<base>); baked interp/RUNPATH use the LOGICAL /td/store/<base>.
X86_NATIVE_CLOSURE_NAMES="binutils-2.44-x86_64-native gcc-14.3.0-x86_64-native"
X86_NATIVE_LOCK=tests/td-toolchain-x86_64-native.lock

# _x86_point_native NAME PHYS — repoint the native toolchain vars at a placed component (physical path).
_x86_point_native() {
  case "$1" in
    binutils-*) XNBU="$2" ;;
    gcc-*) XNGCC="$2" ;;
  esac
}

# x86_64_build_closure_native OUT STORE DB — for the from-BUILD path: intern the 2 BUILT native trees
# (XNBU/XNGCC) at their lock-keyed input-addressed paths in a FRESH closure STORE, and subst-EXPORT each
# (NAR + td-native narinfo, td-builder only — no td-subst/key) to OUT for the daily to sign+publish.
# INTERLEAVED like the cross build_closure (store-add-input-addressed REWRITES DB to the path it added).
# Repoints XNBU/XNGCC at the interned PHYSICAL paths for verify_x86_64_native_ownroot.
x86_64_build_closure_native() {
  _out=$1; _store=$2; _db=$3
  _k=`"$TB" toolchain-key "$X86_NATIVE_LOCK"` || { echo "build_closure_native: toolchain-key failed" >&2; return 1; }
  mkdir -p "$_out"
  for nm in $X86_NATIVE_CLOSURE_NAMES; do
    case "$nm" in binutils-*) src="$XNBU" ;; gcc-*) src="$XNGCC" ;; esac
    test -n "$src" -a -d "$src" || { echo "build_closure_native: no built tree for $nm ($src)" >&2; return 1; }
    p=`"$TB" store-add-input-addressed "$nm" "$_k" "$src" "$_store" "$_db"` \
      || { echo "build_closure_native: store-add-input-addressed $nm failed" >&2; return 1; }
    want=`"$TB" toolchain-path "$X86_NATIVE_LOCK" "$nm"`
    test "$p" = "$want" || { echo "build_closure_native: $nm path $p != lock-computed $want" >&2; return 1; }
    "$TB" subst-export "$_db" "$_store" "$_out" "$p" >/dev/null \
      || { echo "build_closure_native: subst-export $nm ($p) failed" >&2; return 1; }
    test -f "$_out/`basename "$p"`.narinfo" || { echo "build_closure_native: no narinfo for $nm" >&2; return 1; }
    _x86_point_native "$nm" "$_store/`basename "$p"`"
    echo "   [closure-native] $nm interned at lock-keyed $p + subst-exported (the daily signs + publishes it)"
  done
  export XNBU XNGCC
}

# x86_64_resolve_closure_native STORE DB — if a subst store is exposed (TD_SUBST_BIN+TD_SUBST_STORE),
# resolve BOTH native components, restore them into STORE at their lock paths, and repoint XNBU/XNGCC.
# Return 0 only if BOTH HIT; any MISS / no subst configured -> 1 (build from the cross toolchain).
x86_64_resolve_closure_native() {
  _store=$1; _db=$2
  [ -n "${TD_SUBST_BIN:-}" ] && [ -n "${TD_SUBST_STORE:-}" ] || return 1
  _shdir=`dirname "$(command -v sh)"`
  _cu=`grep -- '-coreutils-' tests/td-subst.lock | sed 's/^[^ ]* //' | head -1`
  _dest=`mktemp -d`
  for nm in $X86_NATIVE_CLOSURE_NAMES; do
    p=`env -i PATH="$_cu/bin:$_shdir" TD_BUILDER="$TB" TD_SUBST_BIN="$TD_SUBST_BIN" \
        TD_SUBST_STORE="$TD_SUBST_STORE" TD_SUBST_PUBKEY="${TD_SUBST_PUBKEY:-tests/td-subst.pub}" \
        TD_STORE_DIR=/td/store sh tools/resolve-toolchain.sh "$X86_NATIVE_LOCK" "$nm" "$_dest"` \
      || { rm -rf "$_dest"; return 1; }   # MISS on either component → fall back to from-cross build
    base=`basename "$p"`
    rm -rf "$_store/$base"; cp -a "$_dest/$base" "$_store/$base"; chmod -R u+w "$_store/$base"
    _x86_point_native "$nm" "$_store/$base"
    echo "   [closure-native/fetch] $nm restored at /td/store/$base (ed25519 sig + StorePath==lock-path + NarHash verified)"
  done
  rm -rf "$_dest"
  export XNBU XNGCC
  return 0
}

# x86_64_bundle_tooldir GCCTREE — make the cross gcc SELF-CONTAINED for the FETCH path. The cross gcc is
# configured --with-as/--with-ld at the build-time binutils SCRATCH dir (a mktemp path); after a cold
# fetch that path is gone, and the closure binutils ships only the target-PREFIXED x86_64-pc-linux-gnu-
# {as,ld} (no plain as/ld), so gcc's PATH fallback finds nothing → "could not compile". Fix: install
# plain as/ld into gcc's OWN tooldir ($GCCTREE/$XTARGET/bin) — the dir gcc searches for the assembler/
# linker relative to argv[0] (`gcc -print-prog-name=as`). RELATIVE symlinks to the SIBLING binutils lock
# path, so they resolve in EVERY context the closure is unpacked as siblings: the host-side verify compile
# ($cstore), the store-ns own-root (/td/store bind), and a fetched consumer. MUST run BEFORE the gcc tree
# is interned so the links land in the published nar (td's nar preserves symlinks). Idempotent.
x86_64_bundle_tooldir() {
  _gcc=$1
  _bubase=`basename "$("$TB" toolchain-path "$X86_LOCK" binutils-2.44-x86_64)"` \
    || { echo "bundle_tooldir: could not compute the binutils lock path" >&2; return 1; }
  mkdir -p "$_gcc/$XTARGET/bin"
  ln -sf "../../../$_bubase/bin/$XTARGET-as" "$_gcc/$XTARGET/bin/as"
  ln -sf "../../../$_bubase/bin/$XTARGET-ld" "$_gcc/$XTARGET/bin/ld"
}

# x86_64_verify_closure CPATH STORE DB BASHBASE — compile a DYNAMIC x86_64 C program with the
# closure's cross gcc (XGCC2/XBU) against its glibc (XGLIBC), bake interp/RUNPATH = the glibc lock
# path, intern the program, and RUN it in the store-ns own-root (STORE bound at /td/store) -> 42,
# /gnu/store ABSENT. The DURABLE proof that the closure (built+interned OR fetched) is a working
# toolchain — the prerequisite a skip relies on.
x86_64_verify_closure() {
  _cpath=$1; _store=$2; _db=$3; _bbase=$4
  test -n "$XBU" -a -n "$XGCC2" -a -n "$XGLIBC" || { echo "verify_closure: closure vars unset" >&2; return 1; }
  # [no-guix] (DURABLE, runs on BOTH paths — the fetch path's static guix-byte-freeness leg, which
  # verify_x86_64_ownroot only runs on the build path): the closure libc + cross gcc carry no
  # /gnu/store bytes. Cheap (a grep), so a fetched substitute that smuggled guix bytes would red.
  _xcc1=`find "$XGCC2" -name cc1 2>/dev/null | head -1`
  for _b in "$XGLIBC/lib/libc.so.6" "$XGCC2/bin/$XTARGET-gcc" "$_xcc1"; do
    test -n "$_b" -a -e "$_b" || { echo "verify_closure: closure file missing ($_b)" >&2; return 1; }
    if grep -q -a '/gnu/store' "$_b"; then echo "verify_closure: [no-guix] $_b contains /gnu/store bytes" >&2; return 1; fi
  done
  echo "   [no-guix] the closure libc.so.6 + cross gcc/cc1 carry no /gnu/store bytes"
  # [self-contained] DURABLE (no guix oracle): the cross gcc must carry plain as/ld in its OWN tooldir
  # ($XTARGET/bin) so a FETCHED gcc (whose build-time --with-as scratch path is gone) finds the assembler/
  # linker relative to argv[0]. A regression that drops x86_64_bundle_tooldir reds HERE on BOTH paths — so
  # the build path (where the lingering scratch dir would still satisfy --with-as) can't mask a fetch-only
  # break. The compile below is the behavioral proof they WORK; this pins WHY (the bundled tooldir).
  for _t in as ld; do
    test -e "$XGCC2/$XTARGET/bin/$_t" || { echo "verify_closure: cross gcc tooldir missing '$_t' ($XGCC2/$XTARGET/bin/$_t) — not self-contained for fetch" >&2; return 1; }
  done
  echo "   [self-contained] cross gcc bundles as/ld in its own tooldir ($XTARGET/bin) — resolves them relative to argv[0], no --with-as scratch path"
  glrel=`basename "$XGLIBC"`
  csh=`command -v bash 2>/dev/null || command -v sh`
  w=`mktemp -d`; printf 'int main(){return 42;}\n' > "$w/c.c"
  bw=`mktemp -d`
  printf '#!%s\nexec "%s/bin/%s-gcc" -isystem "%s/include" -B"%s/lib" -L"%s/lib" -static-libgcc -Wl,--dynamic-linker -Wl,/td/store/%s/lib/ld-linux-x86-64.so.2 -Wl,--enable-new-dtags -Wl,-rpath -Wl,/td/store/%s/lib "$@"\n' \
    "$csh" "$XGCC2" "$XTARGET" "$XGLIBC" "$XGLIBC" "$XGLIBC" "$glrel" "$glrel" > "$bw/gcc"
  chmod 0555 "$bw/gcc"
  ( cd "$w" && env PATH="$XBU/bin:$_cpath" "$bw/gcc" -o c.out c.c ) \
    || { echo "verify_closure: closure cross gcc could not compile an x86_64 C program" >&2; return 1; }
  cls=`"$XBU/bin/$XTARGET-readelf" -h "$w/c.out" 2>/dev/null | grep -i 'class:' | grep -o 'ELF64'`
  test "$cls" = ELF64 || { echo "verify_closure: program not ELF64 (got '$cls')" >&2; return 1; }
  ci=`"$XBU/bin/$XTARGET-readelf" -l "$w/c.out" 2>/dev/null | grep -o "/td/store/$glrel/lib/ld-linux-x86-64.so.2" | head -1`
  test -n "$ci" || { echo "verify_closure: program interp not the lock-keyed /td/store x86_64 ld" >&2; return 1; }
  mkdir -p "$_store/cprog/bin"; cp "$w/c.out" "$_store/cprog/bin/c"; chmod -R u+w "$_store"
  wp=`"$TB" store-add-recursive cprog "$_store/cprog" "$_store" "$_db"` || { echo "verify_closure: store-add cprog failed" >&2; return 1; }
  wprel=${wp#/td/store/}
  sn='[ -e /gnu/store ] && echo GNU-PRESENT || echo GNU-ABSENT
/td/store/'"$wprel"'/bin/c; echo "CRC=$?"'
  out=`"$TB" store-ns "$_store" -- "/td/store/$_bbase/bin/bash" -c "$sn" 2>&1` \
    || { printf '%s\n' "$out" | sed 's/^/     /' >&2; echo "verify_closure: store-ns run exited nonzero" >&2; return 1; }
  echo "$out" | grep -q '^CRC=42$' || { printf '%s\n' "$out" | sed 's/^/     /' >&2; echo "verify_closure: x86_64 program did not return 42 from the closure toolchain" >&2; return 1; }
  echo "$out" | grep -q '^GNU-ABSENT$' || { echo "verify_closure: /gnu/store present in the own-root" >&2; return 1; }
}


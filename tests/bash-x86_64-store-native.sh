#!/bin/sh
# tests/bash-x86_64-store-native.sh — /td/store harness userland (NO GUIX BYTES), re #312: GNU
# bash 5.2.37 — the shell the gate RUNNER itself runs every gate body under (builder/src/gates.rs
# executes each `script` as one `bash -c <body>`) — built FROM upstream source (td-fetch, sha-
# pinned) by the from-seed /td/store x86_64 toolchain (reused from the x86_64 gate as a function
# library, fetched via warm-subst or built from the 229-byte seed), DYNAMIC vs the /td/store glibc
# 2.41 (interp = /td/store/ld), interned at /td/store, and RUN in the store-ns own-root the way the
# runner drives it — `bash -c` of a real multi-command body using bash-only features (arrays, [[ ]],
# ${v^^}) that busybox ash cannot — with /gnu/store ABSENT. This is the ladder's own interpreter
# coming from td's store instead of the guix `guix shell` prelude. 5.2.37 is the pinned-channel
# version (tests/*-no-guix.lock ship bash-5.2.37), so the /td/store bash matches it.
#
# Legs (DURABLE — no guix oracle):
#   [supply-chain] the bash-5.2.37 tarball matches its lock sha256 (the sha IS the oracle).
#   [provenance]   the built bash carries zero /gnu/store bytes.
#   [no-guix]      the interned /td/store tree (bash + td-built glibc/libgcc) has zero /gnu/store
#                  anywhere; the relinked interp is /td/store/ld.
#   [behavioral]   the /td/store bash RUNS in the store-ns own-root under `bash -c` (as the gate
#                  runner invokes it) and executes bash-only syntax → the expected output.
#   [structural]   inside the own-root /td/store IS the store AND /gnu/store is ABSENT.
# Verified-red (in-gate): without the elf-set-interp relink the own-root run FAILS (the build-dir
# interp does not exist in the own-root) — the relink is load-bearing.
#
# HEAVY (~90 min from the seed, ~15 with the warm-subst toolchain fetch; directive 1 — no cache).
# NOT a BUILD_GATE. Mirrors gate 420 (userland-x86_64-store-native) — same toolchain obtain +
# _xbin scaffolding + intern/own-root pattern; the shared prologue is issue #365 dedup territory.
set -eu

# --- source the x86_64 toolchain gate as a FUNCTION LIBRARY (build_* rungs + pinned vars) --
export TD_X86_64_LIB=1
. tests/bootstrap-x86_64-toolchain-store-native.sh
unset TD_X86_64_LIB
# in scope: ROOT, fail(), sha(), lf(), make_curated_path, the build_* rungs, KH_X86_64_TB.

# --- [supply-chain] bash-5.2.37 tarball matches its lock sha256 -----------------------------
BASH_LOCK=`ls seed/sources/bash-5.2.37.lock 2>/dev/null | head -1`
test -n "$BASH_LOCK" || fail "no seed/sources/bash-5.2.37.lock pin"
BASH_TB=".td-build-cache/sources/`lf "$BASH_LOCK" file`"
test -f "$BASH_TB" || fail "warmed $BASH_TB absent — run 'td-feed warm sources' (host PREP)"
test "`sha "$BASH_TB"`" = "`lf "$BASH_LOCK" sha256`" || fail "warmed $BASH_TB sha256 != lock pin"
echo "   [supply-chain] bash-5.2.37 matches its lock sha256 — upstream GNU bytes, not guix"

# An x86_64 cc wrapper that builds RUNNABLE binaries (interp = the build-dir glibc loader, so
# configure tests + build-time tools run now) + RUNPATH $ORIGIN/../lib (so the shipped tree finds
# its libs). The final binary's interp is relinked to /td/store/ld afterward. Identical to gate 420.
emit_cc() {
  csh=`command -v bash 2>/dev/null || command -v sh`
  printf '#!%s\nexec "%s/bin/%s-gcc" -isystem "%s/include" -idirafter "%s" -B"%s/lib" -L"%s/lib" -L"%s" -Wl,--dynamic-linker -Wl,"%s/lib/ld-linux-x86-64.so.2" -Wl,-rpath -Wl,"%s/lib:%s" "$@"\n' \
    "$csh" "$2" "$XTARGET" "$3" "$KHINC" "$3" "$3" "$4" "$3" "$3" "$4" > "$1"
  chmod 0555 "$1"
}

# build_bash_x86_64 <cpath> <xgcc2> <xglibc> <xlibgccdir> <xbu> <out> — GNU bash 5.2.37, autotools
# like GNU make. Configure+build with the runnable cc; YACC = the _xbin bison (parse.y → y.tab.c);
# --without-bash-malloc so bash uses the /td/store glibc allocator. Output: $out/bash (interp
# relinked later).
build_bash_x86_64() {
  mc=$1; xg=$2; xgl=$3; xlg=$4; xb=$5; out=$6
  rm -rf "$out"; mkdir -p "$out"
  csh=`command -v bash 2>/dev/null || command -v sh`
  src=`mktemp -d`/bash; mkdir -p "$src"
  tar -xzf "$BASH_TB" -C "$src" --strip-components=1 || { echo "bash unpack failed" >&2; return 1; }
  # The sandbox has NO /bin/sh: run configure THROUGH the curated shell and rewrite #!/bin/sh
  # helper shebangs (its shebang would otherwise fail "No such file or directory").
  find "$src" -type f -exec sed -i "1s|^#! */bin/sh\b|#!$csh|" {} + 2>/dev/null || true
  # Pin the autotools build-system files to ONE mtime so `make` does not try to re-run
  # automake/autoconf (absent → Error 127) — a target is rebuilt only when a prerequisite is
  # STRICTLY newer. parse.y → y.tab.c still regenerates via the _xbin bison (parse.y is source).
  find "$src" -type f \( -name '*.in' -o -name '*.am' -o -name '*.ac' -o -name '*.m4' -o -name configure \) -exec touch -t 202601010101 {} + 2>/dev/null || true
  wb=`mktemp -d`/wb; mkdir -p "$wb"; emit_cc "$wb/cc" "$xg" "$xgl" "$xlg"
  ( cd "$src"; bp="$xb/bin:$XBIN:$mc"   # $XBIN = the cross-fns _xbin scaffolding (awk/m4/bison/flex/cmp/...); target binaries find glibc via the absolute build-dir rpath the cc wrapper bakes (NO LD_LIBRARY_PATH — it would poison the host gawk)
    env PATH="$bp" CC="$wb/cc" CPP="$wb/cc -E" YACC="bison -y" CONFIG_SHELL="$csh" SHELL="$csh" \
      "$csh" ./configure --build="$XTARGET" --host="$XTARGET" --without-bash-malloc --disable-dependency-tracking >cfg.log 2>&1 \
      || { echo "bash configure failed" >&2; cp cfg.log "$ROOT/.td-build-cache/_bashx-cfg.log" 2>/dev/null||true; echo "--- config.log tail ---" >&2; grep -iE 'cpp|conftest|preprocess|cc1|No such|error|cannot' config.log 2>/dev/null | tail -30 >&2; return 1; }
    env PATH="$bp" MAKEFLAGS= MFLAGS= GNUMAKEFLAGS= MAKELEVEL= \
      make SHELL="$csh" CONFIG_SHELL="$csh" -j"$(nproc)" >build.log 2>&1 \
      || { echo "bash build failed" >&2; cp build.log "$ROOT/.td-build-cache/_bashx-build.log" 2>/dev/null||true; tail -25 build.log >&2; return 1; }
    cp -a bash "$out/bash" ) || return 1
  test -x "$out/bash" || { echo "no x86_64 bash produced" >&2; return 1; }
}

# ============================================================================================
# Build the i686 base FROM THE SEED, then CROSS UP to x86_64 — REUSING the x86_64 gate's rungs.
# (identical prologue to gate 420 / the rust-x86_64 gate; #365 dedup territory.)
# ============================================================================================
cpath=`make_curated_path`
for bad in gcc g++ cc guile guix; do test ! -e "$cpath/$bad" || fail "curated PATH still exposes '$bad'"; done
. tests/cache-lib.sh
. tests/x86_64-cross-fns.sh
. tests/x86_64-subst-lib.sh
export TD_STAGE0_BASE="`pwd`/.td-build-cache/td-shell"
load_stage0 || fail "stage0-builder could not place a guix-free stage0 td-builder"
export TD_STORE_DIR=/td/store
snwork=`mktemp -d`
BASHX="$snwork/bashx"
trap 'rm -rf "$snwork"; [ -n "${binsh_made:-}" ] && rm -f /bin/sh' EXIT INT TERM    # the build branch re-traps to also clean its chain temps
cstore="$snwork/closure-store"; cdb="$snwork/closure.db"; mkdir -p "$cstore"
bs=${TD_GATE_INPUT_BASH_STATIC:-}
test -n "$bs" || fail "TD_GATE_INPUT_BASH_STATIC unset — run via td-builder gate-run, which resolves the gate's declared inputs"
test -x "$bs/bin/bash" || fail "no static bash fixture at $bs"
bbase=`basename "$bs"`; cp -a "$bs" "$cstore/$bbase"; chmod -R u+w "$cstore"

# --- Get the x86_64 toolchain: FETCH the lock-keyed closure (x64-toolchain-subst) if a substitute
# store is exposed, else BUILD it from the 229-byte seed (directive 1). Either path sets
# XBU/XGCC2/XGLIBC/XLIBGCCDIR. The substitute is an optimization, NEVER a correctness dependency.
if x86_64_resolve_closure "$cstore" "$cdb"; then
  echo ">> [subst/SKIP] fetched the x86_64 toolchain closure {binutils,gcc,glibc} — SKIPPED the ~98-min from-seed build"
else
  echo ">> [subst/MISS] no exposed substitute store — building the x86_64 toolchain from the 229-byte seed (directive 1)"
  tc=`build_toolchain` || fail "the seed toolchain (brick 0+1) did not build"
  mesp=`build_mes_prefix "$tc" "$cpath"` || fail "Mes (MesCC self-host) did not build/install"
  TCCD=`mktemp -d`/tcc; build_tcc "$tc" "$cpath" "$mesp" "$TCCD" || fail "MesCC did not build tcc"
  MK=`mktemp -d`/makebuild; build_make "$tc" "$cpath" "$mesp" "$TCCD" "$MK" || fail "tcc did not build GNU Make 3.80"
  PD=`mktemp -d`/patchbuild; build_patch "$cpath" "$mesp" "$TCCD" "$MK" "$PD" || fail "the tcc-built make did not build patch"
  BD=`mktemp -d`/binutilsbuild; build_binutils "$cpath" "$mesp" "$TCCD" "$MK" "$PD" "$BD" || fail "the tcc-built make did not build binutils-mesboot0"
  GD=`mktemp -d`/gccbuild; build_gcc "$cpath" "$mesp" "$TCCD" "$MK" "$PD" "$BD" "$GD" || fail "the toolchain did not build gcc 2.95.3"
  HD=`mktemp -d`/headers; build_headers "$mesp" "$HD" || fail "could not install the kernel headers"
  GLD=`mktemp -d`/glibcbuild; build_glibc "$cpath" "$GD" "$BD" "$TCCD" "$MK" "$PD" "$HD" "$GLD" || fail "the seed toolchain did not build glibc 2.2.5"
  G2=`mktemp -d`/gcc2build; build_gcc_mesboot0 "$cpath" "$GD" "$BD" "$GLD" "$HD" "$MK" "$PD" "$G2" || fail "the toolchain did not rebuild gcc 2.95.3 against glibc"
  B2=`mktemp -d`/binutils1build; build_binutils_mesboot1 "$cpath" "$G2" "$BD" "$GLD" "$MK" "$PD" "$B2" || fail "gcc-mesboot0 did not rebuild binutils against glibc"
  MM=`mktemp -d`/makemesbootbuild; build_make_mesboot "$cpath" "$G2" "$BD" "$GLD" "$MK" "$MM" || fail "gcc-mesboot0 did not rebuild GNU Make against glibc"
  GM1=`mktemp -d`/gccmesboot1build; build_gcc_mesboot1 "$cpath" "$G2" "$B2" "$MM" "$GLD" "$PD" "$GM1" || fail "the toolchain did not build GCC 4.6.4 (c,c++)"
  BMB=`mktemp -d`/binutilsmesbootbuild; build_binutils_mesboot "$cpath" "$GM1" "$B2" "$GLD" "$MM" "$PD" "$BMB" || fail "gcc-mesboot1 did not rebuild binutils"
  GAWKMB=`mktemp -d`/gawkmesbootbuild; build_gawk_mesboot "$cpath" "$GM1" "$B2" "$GLD" "$MM" "$GAWKMB" || fail "gcc-mesboot1 did not build GNU awk"
  GOUT=`mktemp -d`/glibcmesbootbuild; build_glibc_mesboot "$cpath" "$GM1" "$BMB" "$GAWKMB" "$GLD" "$MM" "$PD" "$GOUT" || fail "the toolchain did not build glibc 2.16.0"
  GMB=`mktemp -d`/gccmesbootbuild; build_gcc_mesboot "$cpath" "$GM1" "$BMB" "$GOUT" "$MM" "$PD" "$GMB" || fail "the toolchain did not build gcc-mesboot (GCC 4.9.4)"
  GSH=`mktemp -d`/glibcsharedbuild; build_glibc_mesboot_shared "$cpath" "$GM1" "$BMB" "$GAWKMB" "$GLD" "$MM" "$PD" "$GSH" || fail "the toolchain did not build the SHARED glibc 2.16.0"
  GCC14B=`mktemp -d`/gcc14build; build_gcc_14 "$cpath" "$GMB/out" "$GOUT/out" "$BMB/out" "$GCC14B" || fail "the toolchain did not build MODERN GCC 14.3.0"
  BMB244SB=`mktemp -d`/bu244sbbuild; build_binutils_244 "$cpath" "$GM1/out" "$GSH/out" "$BMB/out" "$BMB244SB" || fail "the toolchain did not build the modern binutils 2.44"
  trap 'rm -rf "$snwork" "$tc" "$mesp" "`dirname "$TCCD"`" "`dirname "$MK"`" "`dirname "$PD"`" "`dirname "$BD"`" "`dirname "$GD"`" "`dirname "$HD"`" "`dirname "$GLD"`" "`dirname "$G2"`" "`dirname "$B2"`" "`dirname "$MM"`" "`dirname "$GM1"`" "`dirname "$BMB"`" "`dirname "$GAWKMB"`" "`dirname "$GOUT"`" "`dirname "$GMB"`" "`dirname "$GSH"`" "`dirname "$GCC14B"`" "`dirname "$BMB244SB"`" "`dirname "$cpath"`"; [ -n "${binsh_made:-}" ] && rm -f /bin/sh' EXIT INT TERM
  GCC14="$GCC14B/stage/td/store/gcc-14.3.0"; GST="$GOUT/out"
  echo "   built the i686 base: gcc 14.3.0 + glibc 2.16 (static+shared) + binutils 2.44"
  run_x86_64_cross "$cpath" "$GCC14" "$GST" "$GSH/out" "$BMB244SB" "$KH_X86_64_TB" || fail "the x86_64 cross rungs failed"
  verify_x86_64_ownroot "$cpath" "$snwork" || fail "the x86_64 own-root verify failed"
  x86_64_build_closure "`pwd`/.td-build-cache/x86_64-closure-export" "$cstore" "$cdb" || fail "could not intern + subst-export the x86_64 toolchain closure"
fi
x86_64_verify_closure "$cpath" "$cstore" "$cdb" "$bbase" || fail "the x86_64 closure toolchain did not compile+run an x86_64 program → 42"
echo "   x86_64 toolchain ready (XGCC2=$XGCC2)"

# --- autoconf/recursive-make scaffolding (awk/m4/bison/flex/cmp/...) via the cross-fns _xbin ---
XBIN="$snwork/xbin"; _xbin "$XBIN"; export XBIN
test -x "$XBIN/awk" -a -x "$XBIN/bison" || fail "_xbin produced no awk/bison for the build scaffolding"
# bash's Makefile calls find; add an explicit, bound /gnu/store findutils (build-driver; no bytes).
for t in find:findutils xargs:findutils; do
  n=${t%%:*}; pk=${t##*:}
  b=`ls /gnu/store/*-"$pk"-*/bin/"$n" 2>/dev/null | sort | head -1`
  test -n "$b" -a -x "$b" && ln -sf "$b" "$XBIN/$n" || true
done
# bash calls binutils by PLAIN name (AR=ar etc.; host==target x86_64) — give it the cross binutils.
for t in ar nm ranlib objcopy objdump strip size strings; do
  test -x "$XBU/bin/x86_64-pc-linux-gnu-$t" && ln -sf "$XBU/bin/x86_64-pc-linux-gnu-$t" "$XBIN/$t" || true
done
test -x "$XBIN/ar" || fail "no ar for the bash build"

# --- the x86_64 Linux UAPI headers (warm, pinned) for the cc wrapper's -idirafter --------------
KHINC="$snwork/kh"; mkdir -p "$KHINC"
tar -xzf "$KH_X86_64_TB" -C "$KHINC" || fail "could not extract the x86_64 kernel headers ($KH_X86_64_TB)"
test -f "$KHINC/linux/limits.h" || fail "x86_64 kernel headers missing linux/limits.h after extract"
export KHINC

# --- /bin/sh for popen()/system(): the sandbox lacks /bin/sh; bash's build + glibc's popen want it.
# The dev-shell root is a writable ephemeral tmpfs, so point /bin/sh at the curated shell:
# namespace-local, NEVER touches the host (the host root was pivoted away). The EXIT trap removes
# it if WE created it (else it would leak to later gates sharing the outer host-sandbox).
csh0=`command -v bash 2>/dev/null || command -v sh`
binsh_made=
[ -e /bin/sh ] || { mkdir -p /bin 2>/dev/null && ln -sf "$csh0" /bin/sh && binsh_made=1; }
test -e /bin/sh || fail "could not provide /bin/sh for popen() in the sandbox (root not writable?)"

# --- build GNU bash 5.2.37 dynamic vs the x86_64 glibc 2.41 ------------------------------------
build_bash_x86_64 "$cpath" "$XGCC2" "$XGLIBC" "$XLIBGCCDIR" "$XBU" "$BASHX" || fail "the cross gcc did not build GNU bash 5.2.37"
! grep -q -a '/gnu/store' "$BASHX/bash" || fail "$BASHX/bash contains /gnu/store bytes — not guix-free"
echo "   [provenance] the built bash carries zero /gnu/store bytes"

# --- assemble the self-contained tree (bash + glibc/libgcc closure in lib/) --------------------
tree="$snwork/tree"; mkdir -p "$tree/bin" "$tree/lib"
cp "$BASHX/bash" "$tree/bin/bash"
for soname in libc.so.6 libdl.so.2 librt.so.1 libpthread.so.0 libm.so.6 libresolv.so.2; do
  s=`ls "$XGLIBC/lib/$soname" 2>/dev/null | head -1`; test -n "$s" -a -e "$s" && cp -L "$s" "$tree/lib/$soname" || true
done
cp -L "$XLIBGCCDIR/libgcc_s.so.1" "$tree/lib/libgcc_s.so.1" || fail "no libgcc_s.so.1"
chmod -R u+w "$tree"
# relink bash's interp to /td/store/ld (RUNPATH already $ORIGIN/../lib from the link)
"$TB" elf-set-interp "$tree/bin/bash" /td/store/ld || fail "elf-set-interp bash"
case `"$TB" elf-interp "$tree/bin/bash"` in /td/store/*) ;; *) fail "interp of bash not relinked to /td/store" ;; esac
"$TB" elf-set-rpath "$tree/bin/bash" '$ORIGIN/../lib' || fail "elf-set-rpath bash"
# sh applet symlink so scripts with a #!/bin/sh path can reach bash-as-sh inside the tree
( cd "$tree/bin"; ln -sf bash sh )
echo "   [structural] bash interp relinked to /td/store/ld; sh symlink placed"

# --- intern the tree at /td/store + place the loader (a SEPARATE userland store under snwork) ---
ustore="$snwork/userland-store"; udb="$snwork/userland.db"; mkdir -p "$ustore"
out=`"$TB" store-add-recursive bash-x86_64-store-native "$tree" "$ustore" "$udb"` || fail "store-add-recursive"
case "$out" in /td/store/*-bash-x86_64-store-native) ;; *) fail "interned path not content-addressed under /td/store (got: $out)" ;; esac
phys="$ustore/`basename "$out"`"; rel=${out#/td/store/}
test -x "$phys/bin/bash" || fail "interned tree missing bash"
if grep -r -a -q '/gnu/store' "$phys" 2>/dev/null; then fail "interned set contains /gnu/store: `grep -r -a -l '/gnu/store' "$phys" 2>/dev/null | head -1`"; fi
echo "   [no-guix] interned $out — zero /gnu/store (bash + td-built glibc/libgcc)"
for need in libc.so.6 libm.so.6 libgcc_s.so.1; do ls "$phys"/lib/*"$need"* >/dev/null 2>&1 || fail "interned lib/ missing $need"; done
cp -L "$XGLIBC/lib/ld-linux-x86-64.so.2" "$ustore/ld" || fail "could not place the x86_64 loader at /td/store/ld"
! grep -q -a '/gnu/store' "$ustore/ld" || fail "the /td/store/ld loader contains /gnu/store bytes"

# --- RUN bash from /td/store in the store-ns own-root, driven as the gate RUNNER drives it: -----
# `bash -c <body>` of a real multi-command gate-body-style script using bash-only features
# (arrays, [[ ]], ${v^^}) that busybox ash cannot — the actual capability the loop depends on.
body='
[ -e /gnu/store ] && echo GNU-PRESENT || echo GNU-ABSENT
arr=(one two three); echo "ARR=${#arr[@]}:${arr[1]}"
v=hello; [[ $v == hel* ]] && echo "MATCH=${v^^}"
printf "VER=%s\n" "${BASH_VERSION%%(*}"
'
out2=`"$TB" store-ns "$ustore" -- "/td/store/$rel/bin/bash" -c "$body" 2>&1` \
  || { printf '%s\n' "$out2" | sed 's/^/     /' >&2; fail "store-ns bash run exited nonzero"; }
printf '%s\n' "$out2" | sed 's/^/     /'
printf '%s\n' "$out2" | grep -q '^ARR=3:two$'   || fail "bash arrays did not work from /td/store"
printf '%s\n' "$out2" | grep -q '^MATCH=HELLO$' || fail "bash [[ ]] / \${v^^} did not work from /td/store"
printf '%s\n' "$out2" | grep -q '^VER=5\.2'     || fail "bash did not report version 5.2 from /td/store"
echo "   [behavioral] the /td/store bash RAN under \`bash -c\` in the own-root (as the gate runner invokes it) → arrays + [[ ]] + \${v^^} worked, bash 5.2.37"
printf '%s\n' "$out2" | grep -q '^GNU-ABSENT$'  || fail "/gnu/store is PRESENT in the own-root"
echo "   [structural] inside td's own root /td/store IS the store AND /gnu/store is ABSENT"

echo "PASS: bash-x86_64-store-native — from the 229-byte seed (or the warm-subst toolchain), td built"
echo "  GNU bash 5.2.37 from upstream source, DYNAMIC vs the /td/store glibc 2.41, interned at /td/store,"
echo "  and RAN it in the store-ns own-root under \`bash -c\` (the way builder/src/gates.rs runs every gate"
echo "  body) → bash-only syntax worked, /gnu/store ABSENT, zero guix bytes. The ladder's own interpreter"
echo "  now comes from td's store instead of the guix \`guix shell\` prelude (re #312)."

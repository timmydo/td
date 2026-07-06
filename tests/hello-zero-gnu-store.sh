#!/bin/sh
# tests/hello-zero-gnu-store.sh — issue #388: the FIRST zero-/gnu/store `td-builder build-recipe`.
#
# The corpus gates (hello/sed store-native) substitute ONLY the gcc-toolchain to /td/store and warm every
# OTHER build tool (bash/coreutils/make/sed/grep/tar/gzip/…) from /gnu/store — the "guix-seeded corpus
# template" AGENTS.md declares CLOSED (it retires no guix bytes: the build ENV is still guix). This gate is
# the north-star re-aim's first payoff: GNU hello 2.12.2 built by `build-recipe` from a lock whose EVERY
# entry is /td/store — the gcc-toolchain, the build userland (busybox 1.37.0 sh+coreutils+sed+grep+awk+tar+
# gzip applets + GNU make 4.4.1, td-built at /td/store), AND the source (td-fetched, interned at /td/store).
# The lock carries ZERO /gnu/store entries (asserted), the build ENV stages no guix path, and the hello
# binary (interp = the /td/store glibc 2.41) RUNS in the store-ns own-root with /gnu/store ABSENT → greets.
#
# The build userland reuses the from-seed /td/store x86_64 toolchain (gate 420's `run_x86_64_cross` /
# `x86_64_resolve_closure`, sourced as a function library) to build busybox+make from upstream source — the
# same set gate 420 proves RUNS from /td/store. This gate adds the missing half: FEEDING that userland to
# `build-recipe` as its build tools (bash→sh via the engine's find_build_shell fallback; tar/make/gcc from
# the /td/store userland+toolchain), so a real package builds with no guix bytes in the loop.
#
# HEAVY (~90 min from the 229-byte seed, ~15 with the warm-subst toolchain fetch; directive 1 — no cache
# shortcut on the from-seed leg). x86_64 native. non_blocking (the #356 pin-drift family: on a dev box
# without an exposed subst store the toolchain builds from seed and can memory-kill, #371 — tolerated).
#
# Legs (DURABLE — no guix oracle in any):
#   [supply-chain] busybox + make + hello tarballs match their seed/sources/*.lock sha256 (the sha IS the oracle).
#   [zero-gnu-lock] the composed hello lock has ZERO /gnu/store entries (grep -c == 0) — the #388 assertion.
#   [no-guix-env]  the build sandbox stages no /gnu/store path (build-recipe's closure.txt has none).
#   [provenance]   the built hello carries zero /gnu/store bytes and its interp is the /td/store glibc 2.41.
#   [behavioral]   hello RUNS in the store-ns own-root → "Hello, world!", /gnu/store ABSENT.
#   [verified-red] perturbing a build input (dropping `make` from the userland) reds the build.
set -eu

ROOT=$(pwd)
fail() { echo "FAIL: $*" >&2; exit 1; }
sha() { sha256sum "$1" | cut -d' ' -f1; }
lf() { sed -n "s/^$2 //p" "$1" | head -1; }

# --- [supply-chain] busybox + make + hello tarballs match their pins -------------------------------
BB_LOCK=`ls seed/sources/busybox-*.lock 2>/dev/null | head -1`;  test -n "$BB_LOCK" || fail "no seed/sources/busybox-*.lock pin"
MK_LOCK=`ls seed/sources/make-4.4*.lock 2>/dev/null | head -1`;  test -n "$MK_LOCK" || fail "no seed/sources/make-4.4*.lock pin"
HL_LOCK=`ls seed/sources/hello-2.12*.lock 2>/dev/null | head -1`; test -n "$HL_LOCK" || fail "no seed/sources/hello-2.12*.lock pin"
BB_TB=".td-build-cache/sources/`lf "$BB_LOCK" file`"
MK_TB=".td-build-cache/sources/`lf "$MK_LOCK" file`"
HL_TB=".td-build-cache/sources/`lf "$HL_LOCK" file`"
for pair in "$BB_TB:`lf "$BB_LOCK" sha256`" "$MK_TB:`lf "$MK_LOCK" sha256`" "$HL_TB:`lf "$HL_LOCK" sha256`"; do
  f=${pair%:*}; want=${pair##*:}
  test -f "$f" || fail "pinned tarball not warm ($f) — run 'td-feed warm sources'"
  test "`sha "$f"`" = "$want" || fail "warmed $f sha256 != lock pin ($want)"
done
echo "   [supply-chain] busybox 1.37.0 + make 4.4.1 + hello 2.12.2 tarballs match their seed/sources pins — upstream bytes, not guix"

# --- x86_64 toolchain + the build-userland builders (gate 420's function libraries) -----------------
# The x86_64 toolchain gate sources as a FUNCTION LIBRARY under TD_X86_64_LIB=1: it runs the x86_64
# pinned-input checks and sets KH_X86_64_TB (+ ROOT/fail/sha/lf), returning before its build driver.
# Then the two low libs give make_curated_path / run_x86_64_cross / x86_64_* / _xbin / XTARGET.
export TD_X86_64_LIB=1
. tests/bootstrap-x86_64-toolchain-store-native.sh
unset TD_X86_64_LIB
. tests/cache-lib.sh
. tests/x86_64-cross-fns.sh
. tests/x86_64-subst-lib.sh
export TD_STAGE0_BASE="`pwd`/.td-build-cache/td-shell"
load_stage0 || fail "stage0-builder could not place a guix-free stage0 td-builder"
export TD_STORE_DIR=/td/store

snwork=`mktemp -d`
binsh_made=
trap 'rm -rf "$snwork"; [ -n "$binsh_made" ] && rm -f /bin/sh 2>/dev/null || true' EXIT INT TERM
cstore="$snwork/closure-store"; cdb="$snwork/closure.db"; mkdir -p "$cstore"
bs=${TD_GATE_INPUT_BASH_STATIC:-}
test -n "$bs" -a -x "${bs:-/nonexistent}/bin/bash" || fail "TD_GATE_INPUT_BASH_STATIC unset/invalid — run via td-builder gate-run (declared input)"
bbase=`basename "$bs"`; cp -a "$bs" "$cstore/$bbase"; chmod -R u+w "$cstore"

cpath=`make_curated_path`
for bad in gcc g++ cc guile guix; do test ! -e "$cpath/$bad" || fail "curated PATH still exposes '$bad'"; done

# Get the x86_64 toolchain: FETCH the lock-keyed closure if a substitute store is exposed, else BUILD it
# from the seed (directive 1). Either path sets XBU/XGCC2/XGLIBC/XLIBGCCDIR (host-runnable cross toolchain).
if x86_64_resolve_closure "$cstore" "$cdb"; then
  echo ">> [subst/SKIP] fetched the x86_64 toolchain closure {binutils,gcc,glibc} — SKIPPED the from-seed build"
else
  echo ">> [subst/MISS] no exposed substitute store — building the x86_64 toolchain from the 229-byte seed (directive 1)"
  run_x86_64_cross "$cpath" || fail "the x86_64 cross toolchain (recipe ladder) failed to build from the seed"
  verify_x86_64_ownroot "$cpath" "$snwork" || fail "the x86_64 own-root verify failed"
  x86_64_build_closure "`pwd`/.td-build-cache/x86_64-closure-export" "$cstore" "$cdb" || fail "could not intern + subst-export the x86_64 toolchain closure"
fi
x86_64_verify_closure "$cpath" "$cstore" "$cdb" "$bbase" || fail "the x86_64 closure toolchain did not compile+run an x86_64 program → 42"
echo "   x86_64 toolchain ready (XGCC2=$XGCC2)"

# --- build-scaffolding + kernel headers + /bin/sh (gate 420's setup; guix build DRIVERS, no output bytes) --
XBIN="$snwork/xbin"; _xbin "$XBIN"; export XBIN
test -x "$XBIN/awk" || fail "_xbin produced no awk for the build scaffolding"
for t in find:findutils xargs:findutils bzip2:bzip2; do
  n=${t%%:*}; pk=${t##*:}
  b=`ls /gnu/store/*-"$pk"-*/bin/"$n" 2>/dev/null | sort | head -1`
  test -n "$b" -a -x "$b" && ln -sf "$b" "$XBIN/$n" || true
done
test -x "$XBIN/find" -a -x "$XBIN/bzip2" || fail "missing find/bzip2 for the busybox Kbuild scaffolding"
for t in ar nm ranlib objcopy objdump strip size strings; do
  test -x "$XBU/bin/x86_64-pc-linux-gnu-$t" && ln -sf "$XBU/bin/x86_64-pc-linux-gnu-$t" "$XBIN/$t" || true
done
test -x "$XBIN/ar" || fail "no ar for the busybox build"
KHINC="$snwork/kh"; mkdir -p "$KHINC"
tar -xzf "$KH_X86_64_TB" -C "$KHINC" || fail "could not extract the x86_64 kernel headers ($KH_X86_64_TB)"
test -f "$KHINC/linux/limits.h" || fail "x86_64 kernel headers missing linux/limits.h"
export KHINC
csh0=`command -v bash 2>/dev/null || command -v sh`
[ -e /bin/sh ] || { mkdir -p /bin 2>/dev/null && ln -sf "$csh0" /bin/sh && binsh_made=1; }
test -e /bin/sh || fail "could not provide /bin/sh for popen() in the sandbox"

# ==========================================================================================================
# A cc wrapper that builds RUNNABLE x86_64 binaries (interp = the build-dir glibc loader, present on the host
# so configure tests + build-time tools run now). The final binaries' interp is relinked to the INTERNED
# /td/store glibc afterward (build-recipe convention — a full store-path interp, NOT gate 420's /td/store/ld).
emit_cc() {
  _out=$1
  printf '#!%s\nexec "%s/bin/%s-gcc" -isystem "%s/include" -idirafter "%s" -B"%s/lib" -L"%s/lib" -L"%s" -Wl,--dynamic-linker -Wl,"%s/lib/ld-linux-x86-64.so.2" -Wl,-rpath -Wl,"%s/lib:%s" "$@"\n' \
    "$csh0" "$XGCC2" "$XTARGET" "$XGLIBC" "$KHINC" "$XGLIBC" "$XGLIBC" "$XLIBGCCDIR" "$XGLIBC" "$XGLIBC" "$XLIBGCCDIR" > "$_out"
  chmod 0555 "$_out"
}
# build_make <out> — GNU make 4.4.1 (the build driver busybox lacks).
build_make() {
  _out=$1; rm -rf "$_out"; mkdir -p "$_out"
  src=`mktemp -d`/make; mkdir -p "$src"
  tar -xzf "$MK_TB" -C "$src" --strip-components=1 || { echo "make unpack failed" >&2; return 1; }
  find "$src" -type f -exec sed -i "1s|^#! */bin/sh\b|#!$csh0|" {} + 2>/dev/null || true
  find "$src" -type f \( -name '*.in' -o -name '*.am' -o -name '*.ac' -o -name '*.m4' -o -name configure \) -exec touch -t 202601010101 {} + 2>/dev/null || true
  wb=`mktemp -d`/wb; mkdir -p "$wb"; emit_cc "$wb/cc"
  ( cd "$src"; bp="$XBU/bin:$XBIN:$cpath"
    env PATH="$bp" CC="$wb/cc" CPP="$wb/cc -E" CONFIG_SHELL="$csh0" SHELL="$csh0" "$csh0" ./configure --build="$XTARGET" --host="$XTARGET" --disable-dependency-tracking >cfg.log 2>&1 \
      || { echo "make configure failed" >&2; tail -20 config.log 2>/dev/null >&2; return 1; }
    env PATH="$bp" MAKEFLAGS= MFLAGS= GNUMAKEFLAGS= MAKELEVEL= make SHELL="$csh0" CONFIG_SHELL="$csh0" >build.log 2>&1 \
      || { echo "make build failed" >&2; tail -25 build.log >&2; return 1; }
    cp -a make "$_out/make" ) || return 1
  test -x "$_out/make" || { echo "no make produced" >&2; return 1; }
}
# build_busybox <out> — busybox 1.37.0 dynamic (sh + the POSIX userland).
build_busybox() {
  _out=$1; rm -rf "$_out"; mkdir -p "$_out"
  src=`mktemp -d`/bb; mkdir -p "$src"
  "$XBIN/bzip2" -dc "$BB_TB" | tar -xf - -C "$src" --strip-components=1 || { echo "busybox unpack failed" >&2; return 1; }
  find "$src" -type f -exec sed -i "1s|^#! */bin/sh\b|#!$csh0|" {} + 2>/dev/null || true
  wb=`mktemp -d`/wb; mkdir -p "$wb"; emit_cc "$wb/cc"
  ( cd "$src"; bp="$XBU/bin:$XBIN:$cpath"
    env PATH="$bp" make CC="$wb/cc" HOSTCC="$wb/cc" SHELL="$csh0" CONFIG_SHELL="$csh0" defconfig >cfg.log 2>&1 \
      || { echo "busybox defconfig failed" >&2; tail -20 cfg.log >&2; return 1; }
    sed -i -E '/^#? *CONFIG_STATIC[ =]/d; /^#? *CONFIG_PIE[ =]/d; /^#? *CONFIG_EXTRA_LDFLAGS[ =]/d' .config
    { echo '# CONFIG_STATIC is not set'; echo '# CONFIG_PIE is not set'; echo "CONFIG_EXTRA_LDFLAGS=\"-L$XGLIBC/lib -L$XLIBGCCDIR\""; } >> .config
    yes "" | env PATH="$bp" make CC="$wb/cc" HOSTCC="$wb/cc" SHELL="$csh0" CONFIG_SHELL="$csh0" oldconfig >/dev/null 2>&1
    env PATH="$bp" MAKEFLAGS= MFLAGS= GNUMAKEFLAGS= MAKELEVEL= \
      make CC="$wb/cc" HOSTCC="$wb/cc" SKIP_STRIP=y SHELL="$csh0" CONFIG_SHELL="$csh0" -j"$(nproc)" >build.log 2>&1 \
      || { echo "busybox build failed" >&2; tail -25 build.log >&2; return 1; }
    cp -a busybox "$_out/busybox" ) || return 1
  test -x "$_out/busybox" || { echo "no busybox produced" >&2; return 1; }
}

echo "   --- building the /td/store build userland (busybox + make) from source ---"
MKX="$snwork/makex"; BBX="$snwork/bbx"
build_make    "$MKX" || fail "the cross gcc did not build GNU make 4.4.1"
build_busybox "$BBX" || fail "the cross gcc did not build busybox 1.37.0"
for b in "$MKX/make" "$BBX/busybox"; do ! grep -q -a '/gnu/store' "$b" || fail "$b contains /gnu/store bytes"; done
echo "   [provenance] built busybox + make carry zero /gnu/store bytes"

# ==========================================================================================================
# Intern the glibc, assemble+intern the /td/store userland and gcc-toolchain trees, and intern the source —
# every lock input, in ONE seed store the build-recipe closure spans.
bstore="$snwork/seed-store"; mkdir -p "$bstore"
# store-add-recursive REWRITES its db to the single path it adds, so each interned input gets its OWN
# db; they are merged into the build closure via TD_SEED_DB + TD_EXTRA_DBS (the corpus TD_EXTRA_DBS
# pattern). The store DIR is shared — content-scanned by build-recipe.
gldb="$snwork/gl.db"; updb="$snwork/up.db"; tcdb="$snwork/tc.db"; hsdb="$snwork/hs.db"
GLP=`"$TB" store-add-recursive glibc-2.41 "$XGLIBC" "$bstore" "$gldb"` || fail "store-add glibc-2.41 failed"
case "$GLP" in /td/store/*-glibc-2.41) ;; *) fail "glibc-2.41 not content-addressed at /td/store: $GLP" ;; esac
LD="$GLP/lib/ld-linux-x86-64.so.2"

# The userland tree: busybox + make + a comprehensive applet farm (everything hello's autotools
# configure/make/install invokes). interp → the interned glibc; RUNPATH → the interned glibc lib.
utree="$snwork/userland"; mkdir -p "$utree/bin"
cp "$BBX/busybox" "$utree/bin/busybox"; cp "$MKX/make" "$utree/bin/make"; chmod -R u+w "$utree"
for b in busybox make; do
  "$TB" elf-set-interp "$utree/bin/$b" "$LD" || fail "elf-set-interp userland $b"
  case `"$TB" elf-interp "$utree/bin/$b"` in /td/store/*) ;; *) fail "interp of $b not relinked to /td/store" ;; esac
  "$TB" elf-set-rpath "$utree/bin/$b" "$GLP/lib" || fail "elf-set-rpath userland $b"
done
# busybox applet symlinks by NAME — the autotools build toolset (sh + coreutils + text/archive tools).
( cd "$utree/bin"; for a in sh sed grep egrep fgrep awk gawk tar gzip gunzip bzip2 ls cat cp mv ln rm mkdir rmdir \
    chmod chown touch test true false expr echo printf env pwd dirname basename head tail sort uniq wc tr cut \
    tee find xargs install date sleep id uname od comm cmp diff seq split readlink realpath stat mktemp which; do
    ln -sf busybox "$a"; done )
UP=`"$TB" store-add-recursive hello-userland "$utree" "$bstore" "$updb"` || fail "store-add userland failed"
case "$UP" in /td/store/*-hello-userland) ;; *) fail "userland not content-addressed at /td/store: $UP" ;; esac
test -x "$bstore/`basename "$UP"`/bin/sh" -a -x "$bstore/`basename "$UP"`/bin/make" || fail "interned userland missing sh/make"
UPSH="$UP/bin/sh"
echo "   [content-addr] interned userland $UP (busybox sh+applets + GNU make)"

# The gcc-toolchain-shaped tree (ported from the i686 corpus assembly to x86_64): plain gcc/g++/ar/… wrappers
# around the cross toolchain, shebang = the /td/store busybox sh (zero guix), interp/sysroot/RUNPATH = the
# interned glibc. make invokes gcc/ar/ranlib by PLAIN name, so the wrappers carry those names.
tc="$snwork/gcc-toolchain"; mkdir -p "$tc/bin" "$tc/gcc"
cp -a "$XGCC2/." "$tc/gcc/"
for t in "$XBU"/bin/*; do bn=`basename "$t"`; cp -a "$t" "$tc/bin/$bn"; done
LIBGCC="$XLIBGCCDIR"
for cc in gcc g++; do
cat > "$tc/bin/$cc" <<WRAP
#!$UPSH
d=\$(cd "\$(dirname "\$(readlink -f "\$0")")/.." && pwd)
export PATH="\$d/bin:\$PATH"
unset C_INCLUDE_PATH CPLUS_INCLUDE_PATH
case " \$* " in
  *" -E "*|*" -c "*|*" -S "*|*" -M "*|*" -MM "*) set -- --sysroot=$GLP -B$GLP/lib "\$@" ;;
  *) set -- --sysroot=$GLP -B$GLP/lib -L$GLP/lib -L"$LIBGCC" -static-libgcc -static-libstdc++ -Wl,--dynamic-linker -Wl,$LD -Wl,--enable-new-dtags -Wl,-rpath -Wl,$GLP/lib "\$@" ;;
esac
exec "\$d/gcc/bin/$XTARGET-$cc" "\$@"
WRAP
done
chmod 0555 "$tc/bin/gcc" "$tc/bin/g++"
# Plain-name wrappers for the tools make invokes directly (ar/ranlib/nm/strip/objcopy/objdump/as/ld).
mkdir -p "$tc/bin/.real"
for tool in ar ranlib nm strip objcopy objdump as ld; do
  if [ -f "$tc/bin/$XTARGET-$tool" ]; then
    cp -a "$tc/bin/$XTARGET-$tool" "$tc/bin/.real/$tool"
    cat > "$tc/bin/$tool" <<AWRAP
#!$UPSH
exec "\$(cd "\$(dirname "\$(readlink -f "\$0")")" && pwd)/.real/$tool" "\$@"
AWRAP
    chmod 0555 "$tc/bin/$tool"
  fi
done
# Relink each x86_64-DYNAMIC bin's interp to the interned glibc loader. Arch-aware ON PURPOSE:
# static bins have no interp (skipped), and an i686 tool must NOT be repointed at the x86_64 ld
# (it would stop running) — only a bin already naming an x86-64 loader is retargeted.
find "$tc" -type f | while read -r t; do
  cur=`"$TB" elf-interp "$t" 2>/dev/null` || continue
  case "$cur" in *ld-linux-x86-64*) "$TB" elf-set-interp "$t" "$LD" >/dev/null 2>&1 || true ;; esac
done
TCP=`"$TB" store-add-recursive gcc-toolchain-tdstore "$tc" "$bstore" "$tcdb"` || fail "store-add gcc-toolchain failed"
case "$TCP" in /td/store/*-gcc-toolchain-tdstore) ;; *) fail "gcc-toolchain not content-addressed at /td/store: $TCP" ;; esac
echo "   [content-addr] assembled /td/store gcc-toolchain: $TCP"

# The source, interned at /td/store (the seed/sources fixed-output pattern) — NOT a /gnu/store path.
HSRC=`"$TB" store-add-recursive hello-source "$HL_TB" "$bstore" "$hsdb"` || fail "store-add hello-source failed"
case "$HSRC" in /td/store/*-hello-source) ;; *) fail "hello-source not content-addressed at /td/store: $HSRC" ;; esac
echo "   [content-addr] interned hello source $HSRC"

# ==========================================================================================================
# [zero-gnu-lock] compose the hello lock — EVERY entry a /td/store path, the #388 assertion.
newlock="$snwork/hello.lock"
{
  printf 'gcc-toolchain %s seed\n' "$TCP"
  printf 'glibc-2.41 %s seed\n' "$GLP"
  printf '%s %s seed\n' "`basename "$UP"`" "$UP"
  printf 'hello-source %s source\n' "$HSRC"
} > "$newlock"
gnu=`grep -c '/gnu/store' "$newlock" || true`
test "$gnu" -eq 0 || fail "[zero-gnu-lock] the hello lock has $gnu /gnu/store entries (must be 0): `grep '/gnu/store' "$newlock"`"
echo "   [zero-gnu-lock] the composed hello lock has ZERO /gnu/store entries:"
sed 's/^/       /' "$newlock" >&2

# ==========================================================================================================
# build-recipe hello with the ALL-/td/store seed store — no guix build, no warm-seed from /gnu/store.
load_recipe_eval 2>/dev/null || {
  sh tests/recipe-eval-tool.sh "$PWD/.td-build-cache/recipe-eval" >/dev/null || fail "could not build td-recipe-eval"
  load_recipe_eval || fail "no td-recipe-eval after recipe-eval-tool"
}
sh tests/recipe-emit.sh hello > "$snwork/hello.json" || fail "recipe-emit hello"
mkdir -p "$snwork/hb" "$snwork/tmp"
env -i HOME="$snwork" TMPDIR="$snwork/tmp" PATH="$cpath" \
  TD_BUILDER_PATH="$TD_BUILDER_PATH" TD_BUILDER_STORE="$TD_BUILDER_STORE" TD_BUILDER_DB="$TD_BUILDER_DB" \
  TD_SEED_STORE="$bstore" TD_SEED_DB="$gldb" TD_EXTRA_DBS="$updb:$tcdb:$hsdb" \
  "$TB" build-recipe "$snwork/hello.json" "$newlock" "$snwork/hb" "$gldb" >"$snwork/hb.out" 2>"$snwork/hb.err" \
  || { tail -30 "$snwork/hb.err" >&2; fail "build-recipe hello with the all-/td/store userland+toolchain"; }
o=`sed -n 's/^OUT=out //p' "$snwork/hb.out"`; test -n "$o" || fail "hello produced no output"
hdir="$snwork/hb/newstore/`basename "$o"`"; hbin="$hdir/bin/hello"; test -x "$hbin" || fail "no hello binary"

# [no-guix-env] the build closure staged NO /gnu/store path.
if grep -v '	' "$snwork/hb/closure.txt" 2>/dev/null | grep -q '^/gnu/store/'; then
  fail "[no-guix-env] the build staged a /gnu/store path: `grep -v '	' "$snwork/hb/closure.txt" | grep '^/gnu/store/' | head -1`"
fi
echo "   [no-guix-env] the build sandbox staged no /gnu/store path — the userland+toolchain are all /td/store"

# [provenance] the built hello links the /td/store glibc, no /gnu/store bytes.
hi=`"$TB" elf-interp "$hbin" 2>/dev/null | grep -o "$LD" | head -1`
test -n "$hi" || fail "hello not linked vs the /td/store glibc 2.41 (interp=`"$TB" elf-interp "$hbin"`)"
! grep -q -a '/gnu/store' "$hbin" || fail "hello contains /gnu/store bytes"
echo "   [provenance] build-recipe built hello 2.12.2 with the /td/store userland; interp=$hi; no /gnu/store bytes"

# [behavioral] hello RUNS in the store-ns own-root (glibc + userland-sh staged), /gnu/store ABSENT → greets.
vs="$snwork/verify"; mkdir -p "$vs"
cp -a "$bstore/`basename "$GLP"`" "$vs/`basename "$GLP"`"
cp -a "$bstore/`basename "$UP"`"  "$vs/`basename "$UP"`"
cp -a "$hdir" "$vs/`basename "$o"`"
chmod -R u+w "$vs"
obase=`basename "$o"`
runout=`"$TB" store-ns "$vs" -- "$UPSH" -c '[ -e /gnu/store ] && echo GNU-PRESENT || echo GNU-ABSENT; /td/store/'"$obase"'/bin/hello' 2>&1` \
  || { printf '%s\n' "$runout" | sed 's/^/     /' >&2; fail "store-ns hello run exited nonzero"; }
printf '%s\n' "$runout" | sed 's/^/     /' >&2
echo "$runout" | grep -q '^GNU-ABSENT$' || fail "[behavioral] /gnu/store is PRESENT in the own-root"
echo "$runout" | grep -q 'Hello, world!' || fail "[behavioral] hello did not greet from /td/store: $runout"
echo "   [behavioral] the zero-/gnu/store hello RUNS from /td/store in the own-root → \"Hello, world!\", /gnu/store ABSENT"

# [verified-red] the /td/store build userland is load-bearing: drop it from the lock and the SAME
# build-recipe reds (build.rs finds no sh/tar/make in TD_INPUTS) — fast, before any compile.
redlock="$snwork/hello-nolander.lock"
grep -v -- "$UP" "$newlock" > "$redlock"
mkdir -p "$snwork/hb-red"
if env -i HOME="$snwork" TMPDIR="$snwork/tmp" PATH="$cpath" \
     TD_BUILDER_PATH="$TD_BUILDER_PATH" TD_BUILDER_STORE="$TD_BUILDER_STORE" TD_BUILDER_DB="$TD_BUILDER_DB" \
     TD_SEED_STORE="$bstore" TD_SEED_DB="$gldb" TD_EXTRA_DBS="$updb:$tcdb:$hsdb" \
     "$TB" build-recipe "$snwork/hello.json" "$redlock" "$snwork/hb-red" "$gldb" >"$snwork/hb-red.out" 2>"$snwork/hb-red.err"; then
  fail "[verified-red] build-recipe SUCCEEDED with the /td/store userland removed — the userland is not load-bearing"
fi
grep -qE 'no bash or sh|tar not found|make not found' "$snwork/hb-red.err" \
  || { tail -10 "$snwork/hb-red.err" >&2; fail "[verified-red] the userland-removed build reded, but not on the missing build-tool assertion"; }
echo "   [verified-red] dropping the /td/store userland from the lock reds the build (no sh/tar/make) — the userland is load-bearing"

echo "PASS: issue #388 — GNU hello 2.12.2 built by td-builder build-recipe from a lock with ZERO /gnu/store"
echo "      entries (gcc-toolchain + busybox/make build userland + source, all td-built at /td/store); the"
echo "      build env staged no guix path; the binary links the /td/store glibc 2.41 and greets in the"
echo "      own-root, /gnu/store ABSENT. The FIRST build-recipe with no guix bytes in the loop."

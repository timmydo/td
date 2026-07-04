#!/bin/sh
# tests/bootstrap-hello-corpus-store-native.sh — source-bootstrap BRICK 8 (retire the guix toolchain seed,
# a MODERN glibc 2.41 (guix's glibc-final) at the dynamic /td/store — the FULL modern toolchain from the seed.
# From the 229-byte seed, td builds the whole chain → gcc-mesboot1 + binutils-mesboot + glibc 2.16.0 → GCC
# 4.9.4 → MODERN GCC 14.3.0, AND a sandbox-runnable MODERN binutils 2.44; then with gcc 14.3.0 + binutils 2.44
# it builds MODERN glibc 2.41 (a SHARED libc) against the linux kernel headers. glibc 2.41 is interned
# content-addressed into /td/store, and gcc 14.3.0 links a DYNAMIC C AND C++ (libstdc++ <vector>) program
# against the NEW glibc 2.41 (interp = /td/store glibc 2.41) that runs in the own-root → 42, /gnu/store ABSENT.
# With this, the full modern toolchain (gcc 14.3.0 + binutils 2.44 + glibc 2.41) lives at /td/store.
#
# glibc 2.41 builds smoothly with the modern gcc 14.3.0 (vs the mesboot-era glibc 2.16.0's many iterations).
# glibc-2.41-specific: it needs a modern binutils (2.44; 2.20.1a is too old) built SANDBOX-runnable (build-dir
# interp), `gawk` by name, and it FORBIDS DT_RPATH *and* DT_RUNPATH in the base libc.so.6 → the build CC bakes
# NO -rpath (only the interp); the build tools find glibc 2.16.0 via LD_LIBRARY_PATH. The verify programs are
# built in the sandbox (build-wrapper) then RUN standalone in the own-root. i686, serial. all sources td-fetched.
#
# The ~850-line seed→…→gcc-14.3.0+binutils-2.44+glibc-2.41 chain lives in the SHARED library
# tests/bootstrap-chain.sh (`bootstrap_modern_toolchain`); this gate sources it, so it carries ONLY the corpus
# step. Warm (#317/#327): the chain's bricks are reused from the machine-wide, content-keyed, NAR-verified
# cache instead of rebuilt from the 229-byte seed every run — an order-of-magnitude wall-time drop. Sharing
# never weakens the gate: every [pinned-input]/[no-guix]/[behavioral]/[structural] assertion below still runs
# each time; only the redundant toolchain REBUILD is skipped. Cold (TD_CHECK_CHAIN_CACHE=) still builds the
# whole chain from seed. This gate is the sibling of bootstrap-sed-corpus-store-native (GNU sed); same chain.
#
# Legs (DURABLE — no guix oracle in any):
#   [pinned-input] chain tarballs + boot patches + gcc-4.9.4 + gcc-14.3.0 + gmp/mpfr/mpc + binutils-2.44 + glibc-2.41 match sha256.
#   [no-guix]      built with gcc/g++/cc/guile/guix DENIED; no /gnu/store in glibc 2.41's libc.so.6 NOR gcc/cc1.
#   [content-addr] the interned paths are /td/store/<nar-hash>-<name>.
#   [behavioral]   gcc 14.3.0 links a DYNAMIC C AND C++ (libstdc++) program against the MODERN glibc 2.41
#                  (interp = /td/store glibc 2.41 ld-linux.so.2); both RUN in the own-root → 42, AND build-recipe
#                  builds GNU hello 2.12.2 with it; the hello binary (interp = /td/store glibc 2.41) RUNS → greets.
#   [no-guix-toolchain] the built hello references NO guix gcc-toolchain (the substituted-out compiler).
#   [structural]   inside the own-root /td/store IS the store AND /gnu/store is ABSENT.
set -eu

ROOT=$(pwd)
fail() { echo "FAIL: $*" >&2; exit 1; }
# The ~850-line seed→…→gcc-14.3.0+binutils-2.44+glibc-2.41 chain lives in the shared library
# tests/bootstrap-chain.sh (extracted; all bootstrap-*-store-native gates can source it). This gate
# adds ONLY the corpus step: build GNU hello 2.12.2 with that toolchain via td-builder build-recipe.
# stage0 FIRST: chain_cache_init (inside bootstrap_modern_toolchain) needs $TB for the warm
# brick cache's NAR verification (#317) — a chain run without TB would fail closed.
. tests/cache-lib.sh
export TD_STAGE0_BASE="`pwd`/.td-build-cache/td-shell"
load_stage0 || fail "stage0-builder could not place a guix-free stage0 td-builder"

. tests/bootstrap-chain.sh
bootstrap_modern_toolchain   # from the seed: builds + verifies the toolchain; sets GCC14/GLIBC241/BMB244SB/CC1/cpath/KH_TB
snwork=`mktemp -d`; store="$snwork/td-store"; sndb="$snwork/store.db"; mkdir -p "$store"
export TD_STORE_DIR=/td/store
GLP=`"$TB" store-add-recursive glibc-2.41 "$GLIBC241" "$store" "$sndb"` || fail "store-add glibc-2.41 failed"
case "$GLP" in /td/store/*-glibc-2.41) ;; *) fail "glibc-2.41 not content-addressed at /td/store: $GLP" ;; esac
glrel=${GLP#/td/store/}
echo "   [content-addr] interned $GLP in /td/store"

# Build the test C/C++ programs IN THE SANDBOX (the userland build-wrapper trick): real -B/-L/-isystem at the
# live glibc 2.41 build dir, the /td/store glibc 2.41 interp+RUNPATH baked; sandbox binutils 2.44 for as/ld.
# Then RUN the artifacts standalone in the own-root (no binutils needed there). gcc 14's static libstdc++
# (built vs glibc 2.16.0) runs on glibc 2.41 (backward-compatible). C++ exercises <vector>.
csh=`command -v bash 2>/dev/null || command -v sh`
bw=`mktemp -d`/bw; mkdir -p "$bw" "$snwork/w"
printf 'int main(){return 42;}\n' > "$snwork/w/c.c"
printf '#include <vector>\nint main(){std::vector<int> v; for(int i=0;i<43;i++) v.push_back(i); return v[42];}\n' > "$snwork/w/cpp.cc"
for cc in gcc g++; do
  printf '#!%s\nexec "%s/bin/%s" -isystem "%s/include" -B"%s/lib" -L"%s/lib" -L"%s/lib/gcc/i686-unknown-linux-gnu/14.3.0" -static-libgcc -static-libstdc++ -Wl,--dynamic-linker -Wl,/td/store/%s/lib/ld-linux.so.2 -Wl,--enable-new-dtags -Wl,-rpath -Wl,/td/store/%s/lib "$@"\n' \
    "$csh" "$GCC14" "$cc" "$GLIBC241" "$GLIBC241" "$GLIBC241" "$GCC14" "$glrel" "$glrel" > "$bw/$cc"
done
chmod 0555 "$bw/gcc" "$bw/g++"
( cd "$snwork/w" && env PATH="$BMB244SB/bin:$cpath" "$bw/gcc" -o c.out c.c ) || fail "gcc 14.3.0 did not compile a C program vs glibc 2.41"
( cd "$snwork/w" && env PATH="$BMB244SB/bin:$cpath" "$bw/g++" -O2 -o cpp.out cpp.cc ) || fail "g++ 14.3.0 did not compile a C++ program vs glibc 2.41"
ci=`"$BMB244SB/bin/readelf" -l "$snwork/w/c.out" 2>/dev/null | grep -o "/td/store/$glrel/lib/ld-linux.so.2" | head -1`
test -n "$ci" || fail "the C program interp is not the /td/store glibc 2.41 ld-linux"
if grep -q -a '/gnu/store' "$snwork/w/c.out"; then fail "the C program contains /gnu/store bytes"; fi
echo "   built C + C++ programs vs glibc 2.41, interp=$ci, no /gnu/store"
mkdir -p "$store/prog/bin"; cp "$snwork/w/c.out" "$store/prog/bin/c"; cp "$snwork/w/cpp.out" "$store/prog/bin/cpp"; chmod -R u+w "$store"
WP=`"$TB" store-add-recursive prog "$store/prog" "$store" "$sndb"` || fail "store-add prog failed"; wprel=${WP#/td/store/}
bashlock=`grep -- '-bash-' tests/hello-no-guix.lock | grep -v static | sed 's/^[^ ]* //' | head -1`
bs=`"$TB" store-closure-scan /gnu/store "$bashlock" | grep -- '-bash-static-' | head -1`
bbase=`basename "$bs"`; cp -a "$bs" "$store/$bbase"; chmod -R u+w "$store"
snscript='[ -e /gnu/store ] && echo GNU-PRESENT || echo GNU-ABSENT
/td/store/'"$wprel"'/bin/c; echo "CRC=$?"
/td/store/'"$wprel"'/bin/cpp; echo "CPPRC=$?"'
snout=`"$TB" store-ns "$store" -- "/td/store/$bbase/bin/bash" -c "$snscript" 2>&1` || { printf '%s\n' "$snout" | sed 's/^/     /' >&2; fail "store-ns glibc-2.41 probe exited nonzero"; }
printf '%s\n' "$snout" | sed 's/^/     /' >&2
echo "$snout" | grep -q '^CRC=42$'   || fail "the C program (vs glibc 2.41) did not return 42 in the own-root"
echo "$snout" | grep -q '^CPPRC=42$' || fail "the C++ program (vs glibc 2.41) did not return 42 in the own-root"
echo "   [behavioral] gcc 14.3.0 links a DYNAMIC C AND C++ (libstdc++) program against the MODERN glibc 2.41; both run in the own-root → 42"
echo "$snout" | grep -q '^GNU-ABSENT$' || fail "/gnu/store is PRESENT in the own-root — mixed with guix"
echo "   [structural] inside td's own root /td/store IS the store AND /gnu/store is ABSENT (unmixed from guix)"

# =====================================================================================================
# BRICK 8: the corpus is built by the /td/store toolchain, NOT guix's gcc-toolchain. `td-builder build-recipe`
# builds a REAL corpus package — GNU hello 2.12.2 (the exact version hello-no-guix.lock builds with guix's
# gcc-toolchain-15.2.0) — with the /td/store MODERN toolchain (gcc 14.3.0 + binutils 2.44 + glibc 2.41)
# substituted for guix's gcc-toolchain. Chained via the engine's closure_multi (TD_EXTRA_DBS) + multi-prefix
# sandbox staging. The hello binary links the /td/store glibc 2.41, references NO guix gcc-toolchain, and runs.
echo "   --- brick 8: build-recipe builds corpus GNU hello 2.12.2 with the /td/store toolchain ---"
b8=`mktemp -d`; bstore="$b8/seed-store"; bgdb="$b8/glibc.db"; btdb="$b8/toolchain.db"; mkdir -p "$bstore"
BMB="$BMB244SB"
BUILDBASH=`grep -- '-bash-5.2.37 ' tests/hello-no-guix.lock | grep -v -e static -e minimal | sed 's/^[^ ]* //' | head -1`/bin/bash
case "$BUILDBASH" in /gnu/store/*-bash-*/bin/bash) ;; *) fail "brick8: could not resolve the lock's bash" ;; esac
GLP8=`"$TB" store-add-recursive glibc-2.41 "$GLIBC241" "$bstore" "$bgdb"` || fail "brick8: store-add glibc-2.41 failed"
# Assemble a guix-gcc-toolchain-SHAPED /td/store toolchain: gcc 14 WRAPPER (--sysroot glibc 2.41 so gcc-internal
# headers precede glibc's; interp/RUNPATH baked; link flags only when LINKING; C_INCLUDE_PATH unset) + binutils
# 2.44. Every dynamic bin's PT_INTERP → glibc 2.41 (i686, via td's own elf-set-interp). ar/ranlib/… are wrapped
# to set LD_LIBRARY_PATH because make invokes them directly (not via the gcc wrapper).
tc="$b8/gcc-toolchain"; mkdir -p "$tc/bin" "$tc/gcc"
cp -a "$GCC14/." "$tc/gcc/"; cp -a "$GLIBC241/lib" "$tc/lib"; cp -a "$GCC14/lib/gcc" "$tc/lib/gcc"
for t in "$BMB"/bin/*; do cp -a "$t" "$tc/bin/`basename "$t"`"; done
for cc in gcc g++; do
cat > "$tc/bin/$cc" <<WRAP
#!$BUILDBASH
self=\$(command -v "\$0" 2>/dev/null || echo "\$0")
d=\$(cd "\$(dirname "\$(readlink -f "\$self")")/.." && pwd)
export LD_LIBRARY_PATH="$GLP8/lib\${LD_LIBRARY_PATH:+:\$LD_LIBRARY_PATH}"
unset C_INCLUDE_PATH CPLUS_INCLUDE_PATH
case " \$* " in
  *" -E "*|*" -c "*|*" -S "*|*" -M "*|*" -MM "*) set -- --sysroot=$GLP8 -B$GLP8/lib "\$@" ;;
  *) set -- --sysroot=$GLP8 -B$GLP8/lib -L$GLP8/lib -L"\$d/gcc/lib/gcc/i686-unknown-linux-gnu/14.3.0" -static-libgcc -Wl,--dynamic-linker -Wl,$GLP8/lib/ld-linux.so.2 -Wl,--enable-new-dtags -Wl,-rpath -Wl,$GLP8/lib "\$@" ;;
esac
exec "\$d/gcc/bin/$cc" "\$@"
WRAP
done
chmod 0555 "$tc/bin/gcc" "$tc/bin/g++"
find "$tc" -type f | while read -r t; do
  if "$TB" elf-interp "$t" >/dev/null 2>&1; then "$TB" elf-set-interp "$t" "$GLP8/lib/ld-linux.so.2" >/dev/null 2>&1 || true; fi
done
mkdir -p "$tc/bin/.real"
for tool in ar ranlib nm strip objcopy objdump; do
  if [ -f "$tc/bin/$tool" ] && [ ! -L "$tc/bin/$tool" ]; then
    mv "$tc/bin/$tool" "$tc/bin/.real/$tool"
    cat > "$tc/bin/$tool" <<AWRAP
#!$BUILDBASH
export LD_LIBRARY_PATH="$GLP8/lib\${LD_LIBRARY_PATH:+:\$LD_LIBRARY_PATH}"
exec "\$(cd "\$(dirname "\$(readlink -f "\$0")")" && pwd)/.real/$tool" "\$@"
AWRAP
    chmod 0555 "$tc/bin/$tool"
  fi
done
TCP=`"$TB" store-add-recursive gcc-toolchain-tdstore "$tc" "$bstore" "$btdb"` || fail "brick8: store-add gcc-toolchain failed"
echo "   [brick8] assembled /td/store gcc-toolchain: $TCP (glibc $GLP8)"
# Substitute the gcc-toolchain entry in hello's lock; glibc 2.41 stays in the closure via the toolchain's ref.
# (sed/cut, not awk: gawk is not in tools/loop-toolchain.txt, so bare `awk` dies on the loop PATH.)
oldtc=`grep -- '-gcc-toolchain-' tests/hello-no-guix.lock | head -1 | cut -d' ' -f2`
test -n "$oldtc" || fail "brick8: no gcc-toolchain in hello-no-guix.lock"
newlock="$b8/hello.lock"
sed "/-gcc-toolchain-/c\\
gcc-toolchain $TCP seed\\
glibc-2.41 $GLP8 seed" tests/hello-no-guix.lock > "$newlock"
grep ' /gnu/store/' "$newlock" | sed 's/^[^ ]* //' > "$b8/roots"
"$TB" store-query "$TD_BUILDER_DB" references 2>/dev/null | sed 's/^[^|]*|//' | grep '^/gnu/store/' >> "$b8/roots" || true
sort -u "$b8/roots" -o "$b8/roots"
xargs guix build < "$b8/roots" >/dev/null 2>&1 || fail "brick8: could not realize the guix seed closure"
seedline=`TB="$TB" TD_SEED_DB=/var/guix/db/db.sqlite sh tools/warm-seed.sh "$ROOT/.td-build-cache/seed-b8" $(cat "$b8/roots")` || fail "brick8: warm-seed failed"
WSTORE=`echo "$seedline" | cut -d' ' -f1`; WDB=`echo "$seedline" | cut -d' ' -f2`
for p in "$TCP" "$GLP8"; do cp -a "$bstore/`basename "$p"`" "$WSTORE/`basename "$p"`"; done
chmod -R u+w "$WSTORE/`basename "$TCP"`" "$WSTORE/`basename "$GLP8"`" 2>/dev/null || true
# emit the hello recipe (td-recipe-eval: the build-recipes prelude's sentinel when it ran
# first — a full check — else build it ourselves via the same tool: standalone `check
# bootstrap-hello-corpus-store-native` runs carry no prelude, and NOT being a BUILD_GATE
# means a cold full run can reach this line before build-recipes finishes), then build it.
load_recipe_eval 2>/dev/null || {
  sh tests/recipe-eval-tool.sh "$PWD/.td-build-cache/recipe-eval" >/dev/null || fail "brick8: could not build td-recipe-eval (recipe-eval-tool)"
  load_recipe_eval || fail "brick8: no td-recipe-eval after recipe-eval-tool"
}
sh tests/recipe-emit.sh hello > "$b8/hello.json" || fail "brick8: ts-emit hello"
mkdir -p "$b8/hb" "$b8/tmp"; cu=`grep -- '-coreutils-' "$newlock" | sed 's/^[^ ]* //' | head -1`
env -i HOME="$b8" TMPDIR="$b8/tmp" PATH="$cu/bin:$csh" \
  TD_BUILDER_PATH="$TD_BUILDER_PATH" TD_BUILDER_STORE="$TD_BUILDER_STORE" TD_BUILDER_DB="$TD_BUILDER_DB" \
  TD_SEED_STORE="$WSTORE" TD_SEED_DB="$WDB" TD_EXTRA_DBS="$bgdb:$btdb" \
  "$TB" build-recipe "$b8/hello.json" "$newlock" "$b8/hb" "$WDB" >"$b8/hb.out" 2>"$b8/hb.err" \
  || { tail -25 "$b8/hb.err" >&2; fail "brick8: build-recipe hello with the /td/store toolchain"; }
o=`sed -n 's/^OUT=out //p' "$b8/hb.out"`; test -n "$o" || fail "brick8: hello produced no output"
hdir="$b8/hb/newstore/`basename "$o"`"; hbin="$hdir/bin/hello"; test -x "$hbin" || fail "brick8: no hello binary"
# (a) the /td/store toolchain linked it: interp is the /td/store glibc 2.41.
hi=`"$BMB/bin/readelf" -l "$hbin" 2>/dev/null | grep -o "$GLP8/lib/ld-linux.so.2" | head -1`
test -n "$hi" || fail "brick8: hello not linked vs the /td/store glibc 2.41"
# (b) [no-guix-toolchain] NO reference to guix's gcc-toolchain (the substituted-OUT compiler).
if grep -q -a -- "$oldtc" "$hbin"; then fail "brick8: hello references the guix gcc-toolchain $oldtc"; fi
echo "   [brick8 no-guix-toolchain] build-recipe built hello 2.12.2 with the /td/store toolchain; interp=$hi; no guix gcc-toolchain ref"
# (c) [DURABLE behavioral] hello runs in an own-root holding the /td/store glibc 2.41 + a static bash.
vs="$b8/verify"; mkdir -p "$vs"; glb=`basename "$GLP8"`; hb2=`basename "$o"`
cp -a "$bstore/$glb" "$vs/$glb"; cp -a "$hdir" "$vs/$hb2"
bs8=`"$TB" store-closure-scan /gnu/store "$bashlock" | grep -- '-bash-static-' | head -1`
bb8=`basename "$bs8"`; cp -a "$bs8" "$vs/$bb8"; chmod -R u+w "$vs"
g8=`"$TB" store-ns "$vs" -- "/td/store/$bb8/bin/bash" -c "/td/store/$hb2/bin/hello" 2>&1` || { echo "$g8" | sed 's/^/     /' >&2; fail "brick8: store-ns hello run rc"; }
case "$g8" in *"Hello, world!"*) ;; *) fail "brick8: hello did not greet: $g8" ;; esac
echo "   [brick8 DURABLE behavioral] corpus hello runs from /td/store → $g8"
rm -rf "$ROOT/.td-build-cache/seed-b8" 2>/dev/null || true

echo "PASS: source-bootstrap brick 8 — from the 229-byte seed, td built the chain → GCC 4.9.4 → MODERN GCC"
echo "      14.3.0 + binutils 2.44 → MODERN glibc 2.41 (the full /td/store toolchain), then with that toolchain"
echo "      (NOT guix's gcc-toolchain-15.2.0) td-builder build-recipe built a REAL corpus package, GNU hello"
echo "      2.12.2: it links the /td/store glibc 2.41, references no guix gcc-toolchain, and runs in the"
echo "      own-root → \"Hello, world!\", /gnu/store ABSENT. The corpus is built by td's OWN toolchain."

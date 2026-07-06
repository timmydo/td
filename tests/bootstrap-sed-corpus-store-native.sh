#!/bin/sh
# tests/bootstrap-sed-corpus-store-native.sh — source-bootstrap BRICK 8 (retire the guix toolchain seed): a
# SECOND real corpus package built by td's OWN /td/store toolchain, after GNU hello — the same BRICK 8 engine
# path (bootstrap-hello-corpus-store-native), applied to GNU sed. "More corpus on the /td/store toolchain":
# drives the guix gcc-toolchain out of the corpus baseline. From the 229-byte seed td builds the whole chain →
# gcc-mesboot1 + binutils-mesboot + glibc 2.16.0 → GCC 4.9.4 → MODERN GCC 14.3.0 + a sandbox-runnable MODERN
# binutils 2.44 → MODERN glibc 2.41 (the full /td/store toolchain). Then, with THAT toolchain substituted for
# guix's gcc-toolchain-15.2.0, `td-builder build-recipe` builds a REAL corpus package — GNU sed 4.9, the exact
# version sed-no-guix.lock builds with guix's gcc-toolchain — chained via the engine's closure_multi
# (TD_EXTRA_DBS) + multi-prefix sandbox staging + 32-bit ELF interp rewriting. The sed binary links the
# /td/store glibc 2.41, references NO guix gcc-toolchain, and RUNS in the own-root, performing a real text
# substitution (foo→bar), /gnu/store ABSENT. A non-trivial GNU text processor (not a hello print) built by
# td's OWN toolchain — the substitution that retires the guix toolchain seed, extended to a 2nd package.
#
# The ~850-line seed→…→gcc-14.3.0+binutils-2.44+glibc-2.41 chain lives in the SHARED library
# tests/bootstrap-chain.sh (`bootstrap_modern_toolchain`); this gate sources it, so it carries ONLY the corpus
# step. The gate also re-checks the toolchain by linking + running a C and a C++ program → 42 before the sed build.
# i686, serial. all sources td-fetched; sed 4.9 + its build deps come from sed-no-guix.lock (guix-realized).
#
# Legs (DURABLE — no guix oracle in any):
#   [pinned-input] chain tarballs + boot patches + gcc-4.9.4 + gcc-14.3.0 + gmp/mpfr/mpc + binutils-2.44 + glibc-2.41 match sha256.
#   [no-guix]      built with gcc/g++/cc/guile/guix DENIED; no /gnu/store in glibc 2.41's libc.so.6 NOR gcc/cc1.
#   [content-addr] the interned paths are /td/store/<nar-hash>-<name>.
#   [behavioral]   the /td/store toolchain links a C+C++ program → 42, AND build-recipe builds GNU sed 4.9 with
#                  it; the sed binary (interp = /td/store glibc 2.41) RUNS in the own-root → substitutes foo→bar.
#   [no-guix-toolchain] the built sed references NO guix gcc-toolchain (the substituted-out compiler).
#   [structural]   inside the own-root /td/store IS the store AND /gnu/store is ABSENT.
set -eu

ROOT=$(pwd)
# The ~850-line seed→…→gcc-14.3.0+binutils-2.44+glibc-2.41 chain lives in the shared library
# tests/bootstrap-chain.sh (extracted; all bootstrap-*-store-native gates can source it). This gate
# adds ONLY the corpus step: build GNU sed 4.9 with that toolchain via td-builder build-recipe.
# stage0 FIRST: chain_cache_init (inside bootstrap_modern_toolchain) needs $TB for the warm
# brick cache's NAR verification (#317) — a chain run without TB would fail closed.
. tests/cache-lib.sh
export TD_STAGE0_BASE="`pwd`/.td-build-cache/td-shell"
load_stage0 || fail "stage0-builder could not place a guix-free stage0 td-builder"

. tests/bootstrap-chain.sh
bootstrap_modern_toolchain   # the 20-rung recipe ladder; sets GCC14/GLIBC241/BMB244SB/CC1 + LADDER_TDSTORE/*_BASE
snwork=`mktemp -d`; store="$snwork/td-store"; sndb="$snwork/store.db"; mkdir -p "$store"
export TD_STORE_DIR=/td/store
GLP=`"$TB" store-add-recursive glibc-2.41 "$GLIBC241" "$store" "$sndb"` || fail "store-add glibc-2.41 failed"
case "$GLP" in /td/store/*-glibc-2.41) ;; *) fail "glibc-2.41 not content-addressed at /td/store: $GLP" ;; esac
glrel=${GLP#/td/store/}
echo "   [content-addr] interned $GLP in /td/store"

# Build the test C/C++ programs INSIDE store-ns (#378: binutils 2.44's as/ld are dynamic vs the
# shared glibc 2.16 recipe output, so they run where /td/store canonicals resolve — the own-root,
# not the host). The rung outputs stage in at their CANONICAL names via cp -a from the ladder's
# td-store (no re-hash — baked references keep resolving).
csh=`command -v bash 2>/dev/null || command -v sh`
for b in "$GCC14_BASE" "$BU244_BASE" "$GLSHARED_BASE"; do
  cp -a "$LADDER_TDSTORE/$b" "$store/$b" || fail "staging $b into the verify store failed"
done
chmod -R u+w "$store" 2>/dev/null || true
# the static-bash fixture is a DECLARED gate input (#353): the runner resolved it.
bs=${TD_GATE_INPUT_BASH_STATIC:-}
test -n "$bs" || fail "TD_GATE_INPUT_BASH_STATIC unset — run via td-builder gate-run, which resolves the gate's declared inputs"
test -x "$bs/bin/bash" || fail "no static bash fixture at $bs"
bbase=`basename "$bs"`; cp -a "$bs" "$store/$bbase"; chmod -R u+w "$store"
mkdir -p "$store/w"
printf 'int main(){return 42;}\n' > "$store/w/c.c"
printf '#include <vector>\nint main(){std::vector<int> v; for(int i=0;i<43;i++) v.push_back(i); return v[42];}\n' > "$store/w/cpp.cc"
gcc14ns="/td/store/$GCC14_BASE/stage/td/store/gcc-14.3.0"
for cc in gcc g++; do
  printf '#!/td/store/%s/bin/bash\nexec "%s/bin/%s" -isystem "/td/store/%s/include" -B"/td/store/%s/lib" -L"/td/store/%s/lib" -L"%s/lib/gcc/i686-unknown-linux-gnu/14.3.0" -static-libgcc -static-libstdc++ -Wl,--dynamic-linker -Wl,/td/store/%s/lib/ld-linux.so.2 -Wl,--enable-new-dtags -Wl,-rpath -Wl,/td/store/%s/lib "$@"\n' \
    "$bbase" "$gcc14ns" "$cc" "$glrel" "$glrel" "$glrel" "$gcc14ns" "$glrel" "$glrel" > "$store/w/$cc"
done
chmod 0555 "$store/w/gcc" "$store/w/g++"
snscript='[ -e /gnu/store ] && echo GNU-PRESENT || echo GNU-ABSENT
cd /td/store/w
PATH=/td/store/'"$BU244_BASE"'/bin
export PATH
./gcc -o /tmp/c.out c.c || echo COMPILE-C-FAILED
./g++ -O2 -o /tmp/cpp.out cpp.cc || echo COMPILE-CPP-FAILED
/tmp/c.out; echo "CRC=$?"
/tmp/cpp.out; echo "CPPRC=$?"'
snout=`"$TB" store-ns "$store" -- "/td/store/$bbase/bin/bash" -c "$snscript" 2>&1` || { printf '%s\n' "$snout" | sed 's/^/     /' >&2; fail "store-ns glibc-2.41 build+run probe exited nonzero"; }
printf '%s\n' "$snout" | tail -6 | sed 's/^/     /' >&2
echo "$snout" | grep -q 'COMPILE-C-FAILED'   && fail "gcc 14.3.0 did not compile a C program vs glibc 2.41 in the own-root"
echo "$snout" | grep -q 'COMPILE-CPP-FAILED' && fail "g++ 14.3.0 did not compile a C++ program vs glibc 2.41 in the own-root"
echo "$snout" | grep -q '^CRC=42$'   || fail "the C program (vs glibc 2.41) did not return 42 in the own-root"
echo "$snout" | grep -q '^CPPRC=42$' || fail "the C++ program (vs glibc 2.41) did not return 42 in the own-root"
echo "   [behavioral] gcc 14.3.0 COMPILED AND LINKED a dynamic C and C++ (libstdc++) program against the"
echo "   MODERN glibc 2.41 INSIDE td's own root (binutils 2.44 as/ld from /td/store); both RAN → 42 — the exec itself proves the /td/store interp+libc chain"
echo "$snout" | grep -q '^GNU-ABSENT$' || fail "/gnu/store is PRESENT in the own-root — mixed with guix"
echo "   [structural] inside td's own root /td/store IS the store AND /gnu/store is ABSENT (unmixed from guix)"

# =====================================================================================================
# BRICK 8: the corpus is built by the /td/store toolchain, NOT guix's gcc-toolchain. `td-builder build-recipe`
# builds a REAL corpus package — GNU sed 4.9 (the exact version sed-no-guix.lock builds with guix's
# gcc-toolchain-15.2.0) — with the /td/store MODERN toolchain (gcc 14.3.0 + binutils 2.44 + glibc 2.41)
# substituted for guix's gcc-toolchain. Chained via the engine's closure_multi (TD_EXTRA_DBS) + multi-prefix
# sandbox staging. The sed binary links the /td/store glibc 2.41, references NO guix gcc-toolchain, and runs.
echo "   --- brick 8: build-recipe builds corpus GNU sed 4.9 with the /td/store toolchain ---"
b8=`mktemp -d`; bstore="$b8/seed-store"; bgdb="$b8/glibc.db"; btdb="$b8/toolchain.db"; mkdir -p "$bstore"
BMB="$BMB244SB"
BUILDBASH=`grep -- '-bash-5.2.37 ' tests/sed-no-guix.lock | grep -v -e static -e minimal | sed 's/^[^ ]* //' | head -1`/bin/bash
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
# Substitute the gcc-toolchain entry in sed's lock; glibc 2.41 stays in the closure via the toolchain's ref.
# Use grep/sed (both in the declared loop toolchain) rather than awk — awk/gawk is NOT in the loop profile
# (it is only on PATH via the host guix dir, an undeclared dependency), so grep/sed keeps this hermetic.
# the gcc-toolchain entry is a DECLARED gate input (#353): the runner resolved it.
oldtc=${TD_GATE_INPUT_GCC_TOOLCHAIN:-}
test -n "$oldtc" || fail "TD_GATE_INPUT_GCC_TOOLCHAIN unset — run via td-builder gate-run, which resolves the gate's declared inputs"
newlock="$b8/sed.lock"
# rewrite the lone gcc-toolchain line into the /td/store toolchain ref + glibc 2.41 (paths are /td/store/<base32>-…, no sed-special chars; | delimiter avoids the path slashes).
sed "s|^[^ ]*-gcc-toolchain-[^ ]* .*|gcc-toolchain $TCP seed\nglibc-2.41 $GLP8 seed|" tests/sed-no-guix.lock > "$newlock"
grep ' /gnu/store/' "$newlock" | sed 's/^[^ ]* //' > "$b8/roots"
"$TB" store-query "$TD_BUILDER_DB" references 2>/dev/null | sed 's/^[^|]*|//' | grep '^/gnu/store/' >> "$b8/roots" || true
sort -u "$b8/roots" -o "$b8/roots"
xargs guix build < "$b8/roots" >/dev/null 2>&1 || fail "brick8: could not realize the guix seed closure"
seedline=`TB="$TB" TD_SEED_DB=/gnu/store sh tools/warm-seed.sh "$ROOT/.td-build-cache/seed-b8" $(cat "$b8/roots")` || fail "brick8: warm-seed failed"
WSTORE=`echo "$seedline" | cut -d' ' -f1`; WDB=`echo "$seedline" | cut -d' ' -f2`
for p in "$TCP" "$GLP8"; do cp -a "$bstore/`basename "$p"`" "$WSTORE/`basename "$p"`"; done
chmod -R u+w "$WSTORE/`basename "$TCP"`" "$WSTORE/`basename "$GLP8"`" 2>/dev/null || true
# emit the sed recipe: load td's OWN Rust recipe evaluator (td-recipe-eval, built by the build-recipes
# prelude — the same dependency-free evaluator the build path and `td shell` use since the TypeScript
# surface was deleted in #224), then emit + build the recipe from the Rust catalog. When the prelude
# hasn't run (standalone `check` of this gate; a cold full run outracing build-recipes), build the
# evaluator ourselves via the same tool — it writes the same sentinel.
load_recipe_eval 2>/dev/null || {
  sh tests/recipe-eval-tool.sh "$PWD/.td-build-cache/recipe-eval" >/dev/null || fail "brick8: could not build td-recipe-eval (recipe-eval-tool)"
  load_recipe_eval || fail "brick8: no td-recipe-eval after recipe-eval-tool"
}
sh tests/recipe-emit.sh sed > "$b8/sed.json" || fail "brick8: recipe-emit sed"
mkdir -p "$b8/sb" "$b8/tmp"; cu=`grep -- '-coreutils-' "$newlock" | sed 's/^[^ ]* //' | head -1`
env -i HOME="$b8" TMPDIR="$b8/tmp" PATH="$cu/bin:$csh" \
  TD_BUILDER_PATH="$TD_BUILDER_PATH" TD_BUILDER_STORE="$TD_BUILDER_STORE" TD_BUILDER_DB="$TD_BUILDER_DB" \
  TD_SEED_STORE="$WSTORE" TD_SEED_DB="$WDB" TD_EXTRA_DBS="$bgdb:$btdb" \
  "$TB" build-recipe "$b8/sed.json" "$newlock" "$b8/sb" "$WDB" >"$b8/sb.out" 2>"$b8/sb.err" \
  || { tail -160 "$b8/sb.err" >&2; fail "brick8: build-recipe sed with the /td/store toolchain"; }
  # ^ 160 (was 25) so the whole config.log conftest-section tail the engine now appends
  #   on a configure failure lands in the gate log — #366's socklen_t flake was
  #   undiagnosable because the conftest cause (a compiler killed under memory pressure)
  #   was never captured (autoconf logs it to config.log, not the terminal).
o=`sed -n 's/^OUT=out //p' "$b8/sb.out"`; test -n "$o" || fail "brick8: sed produced no output"
sdir="$b8/sb/newstore/`basename "$o"`"; sbin="$sdir/bin/sed"; test -x "$sbin" || fail "brick8: no sed binary"
# (a) the /td/store toolchain linked it: interp is the /td/store glibc 2.41.
si=`"$TB" elf-interp "$sbin" 2>/dev/null | grep -o "$GLP8/lib/ld-linux.so.2" | head -1`
test -n "$si" || fail "brick8: sed not linked vs the /td/store glibc 2.41"
# (b) [no-guix-toolchain] NO reference to guix's gcc-toolchain (the substituted-OUT compiler).
if grep -q -a -- "$oldtc" "$sbin"; then fail "brick8: sed references the guix gcc-toolchain $oldtc"; fi
echo "   [brick8 no-guix-toolchain] build-recipe built sed 4.9 with the /td/store toolchain; interp=$si; no guix gcc-toolchain ref"
# (c) [DURABLE behavioral] sed runs in an own-root holding the /td/store glibc 2.41 + a static bash, and
# performs a real text substitution: s/foo/bar/ on "foo\nbaz" must yield "bar\nbaz" (a transform, not a print).
vs="$b8/verify"; mkdir -p "$vs"; glb=`basename "$GLP8"`; sb2=`basename "$o"`
cp -a "$bstore/$glb" "$vs/$glb"; cp -a "$sdir" "$vs/$sb2"
# the static-bash fixture is the DECLARED gate input resolved above (#353).
bs8="$bs"
bb8=`basename "$bs8"`; cp -a "$bs8" "$vs/$bb8"; chmod -R u+w "$vs"
sedrun='printf "foo\nbaz\n" | /td/store/'"$sb2"'/bin/sed "s/foo/bar/"'
g8=`"$TB" store-ns "$vs" -- "/td/store/$bb8/bin/bash" -c "$sedrun" 2>&1` || { echo "$g8" | sed 's/^/     /' >&2; fail "brick8: store-ns sed run rc"; }
printf '%s\n' "$g8" | sed 's/^/     /' >&2
echo "$g8" | grep -q '^bar$' || fail "brick8: sed did not substitute foo->bar from /td/store: $g8"
echo "$g8" | grep -q '^baz$' || fail "brick8: sed dropped the unmatched line from /td/store: $g8"
if echo "$g8" | grep -q '^foo$'; then fail "brick8: sed left its input unchanged (no substitution) from /td/store: $g8"; fi
echo "   [brick8 DURABLE behavioral] corpus sed runs from /td/store and transforms foo->bar (a real text substitution)"
rm -rf "$ROOT/.td-build-cache/seed-b8" 2>/dev/null || true

echo "PASS: source-bootstrap brick 8 (2nd corpus package, after hello) — from the 229-byte seed, td built the"
echo "      chain → GCC 4.9.4 → MODERN GCC 14.3.0 + binutils 2.44 → MODERN glibc 2.41 (the full /td/store"
echo "      toolchain), then with that toolchain (NOT guix's gcc-toolchain-15.2.0) td-builder build-recipe built"
echo "      a REAL corpus package, GNU sed 4.9: it links the /td/store glibc 2.41, references no guix"
echo "      gcc-toolchain, and runs in the own-root, substituting foo→bar, /gnu/store ABSENT. The corpus is"
echo "      built by td's OWN toolchain."

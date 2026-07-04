#!/bin/sh
# tests/bootstrap-sqlite-corpus-store-native.sh — /td/store harness userland (#312): sqlite, the
# ladder's store-DB parser oracle, built from its RECIPE by td's OWN /td/store toolchain — the
# bootstrap-hello/sed-corpus-store-native engine path (build-recipe + toolchain substitution),
# applied to the first #312 harness tool. From the 229-byte seed td builds the chain → MODERN GCC
# 14.3.0 + binutils 2.44 + glibc 2.41 (the full /td/store toolchain, warm via the shared chain
# cache); then, with THAT toolchain substituted for the lock's pinned gcc-toolchain-15.2.0,
# `td-builder build-recipe` builds sqlite 3.51.0 (the exact version sqlite-no-guix.lock realizes)
# chained via the engine's closure_multi (TD_EXTRA_DBS) + multi-prefix sandbox staging. The
# sqlite3 binary links the /td/store glibc 2.41, references NO seed gcc-toolchain, and RUNS in
# the own-root — driven exactly as the LADDER drives it (store-register's parser oracle): PRAGMA
# integrity_check + a ValidPaths read over a td-WRITTEN store DB, a real SQL write/read
# round-trip, and a garbage-file negative control — with /gnu/store ABSENT.
#
# Seed provisioning is guix-PROCESS-free (unlike the grandfathered hello/sed siblings): the
# pinned lock closure resolves via tools/resolve-seed.sh (td-subst, #311 — present roots are
# trusted, missing ones fetch from the signed substitute store, else FAIL CLOSED), and the warm
# seed capture content-scans the store DIRECTORY (no read of the packager's private DB). The
# seed BYTES stay the pinned toolchain seed — retired last, per the north star.
#
# Legs (DURABLE — no guix oracle in any):
#   [no-guix-toolchain] the built sqlite3 references NO seed gcc-toolchain (the substituted-out
#                       compiler); its PT_INTERP is the /td/store glibc 2.41 ld-linux.
#   [behavioral]        in the own-root, sqlite3 validates a td-WRITTEN store DB (PRAGMA
#                       integrity_check = ok), reads back the interned glibc path from
#                       ValidPaths (the store-register oracle role), and round-trips a real
#                       SQL write (CREATE/INSERT/SELECT SUM → 42) on the ns tmpfs.
#   [discrimination]    a garbage non-DB file makes sqlite3 FAIL (the oracle is not vacuous).
#   [structural]        inside the own-root /td/store IS the store AND /gnu/store is ABSENT.
set -eu

ROOT=$(pwd)
fail() { echo "FAIL: $*" >&2; exit 1; }
# stage0 FIRST: chain_cache_init (inside bootstrap_modern_toolchain) needs $TB for the warm
# brick cache's NAR verification (#317) — a chain run without TB would fail closed.
. tests/cache-lib.sh
export TD_STAGE0_BASE="`pwd`/.td-build-cache/td-shell"
load_stage0 || fail "stage0-builder could not place a guix-free stage0 td-builder"

# The ~850-line seed→…→gcc-14.3.0+binutils-2.44+glibc-2.41 chain lives in the shared library
# tests/bootstrap-chain.sh; this gate adds ONLY the corpus step (the hello/sed gates already
# re-check the toolchain by linking + running C/C++ programs; no need to triple-prove it here).
. tests/bootstrap-chain.sh
bootstrap_modern_toolchain   # from the seed: builds + verifies the toolchain; sets GCC14/GLIBC241/BMB244SB/CC1/cpath/KH_TB
export TD_STORE_DIR=/td/store

echo "   --- build-recipe builds sqlite 3.51.0 with the /td/store toolchain ---"
b8=`mktemp -d`; bstore="$b8/seed-store"; bgdb="$b8/glibc.db"; btdb="$b8/toolchain.db"; mkdir -p "$bstore"
BMB="$BMB244SB"
csh=`command -v bash 2>/dev/null || command -v sh`
BUILDBASH=`grep -- '-bash-5.2.37 ' tests/sqlite-no-guix.lock | grep -v -e static -e minimal | sed 's/^[^ ]* //' | head -1`/bin/bash
case "$BUILDBASH" in /gnu/store/*-bash-*/bin/bash) ;; *) fail "could not resolve the lock's bash" ;; esac
GLP8=`"$TB" store-add-recursive glibc-2.41 "$GLIBC241" "$bstore" "$bgdb"` || fail "store-add glibc-2.41 failed"
case "$GLP8" in /td/store/*-glibc-2.41) ;; *) fail "glibc-2.41 not content-addressed at /td/store: $GLP8" ;; esac
echo "   [content-addr] interned $GLP8 in /td/store"
# Assemble a gcc-toolchain-SHAPED /td/store toolchain (the hello/sed brick-8 pattern): gcc 14
# WRAPPER (--sysroot glibc 2.41 so gcc-internal headers precede glibc's; interp/RUNPATH baked;
# link flags only when LINKING; C_INCLUDE_PATH unset) + binutils 2.44. Every dynamic bin's
# PT_INTERP → glibc 2.41 (i686, via td's own elf-set-interp). ar/ranlib/… are wrapped to set
# LD_LIBRARY_PATH because make invokes them directly (not via the gcc wrapper).
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
TCP=`"$TB" store-add-recursive gcc-toolchain-tdstore "$tc" "$bstore" "$btdb"` || fail "store-add gcc-toolchain failed"
echo "   assembled /td/store gcc-toolchain: $TCP (glibc $GLP8)"
# Substitute the gcc-toolchain entry in sqlite's lock; glibc 2.41 stays in the closure via the
# toolchain's ref. (grep/sed, not awk: gawk is not in the loop profile.)
# the gcc-toolchain entry is a DECLARED gate input (#353): the runner resolved it.
oldtc=${TD_GATE_INPUT_GCC_TOOLCHAIN:-}
test -n "$oldtc" || fail "TD_GATE_INPUT_GCC_TOOLCHAIN unset — run via td-builder gate-run, which resolves the gate's declared inputs"
newlock="$b8/sqlite.lock"
sed "s|^[^ ]*-gcc-toolchain-[^ ]* .*|gcc-toolchain $TCP seed\nglibc-2.41 $GLP8 seed|" tests/sqlite-no-guix.lock > "$newlock"
# Seed provisioning WITHOUT a guix process (#311): present roots are trusted, missing ones come
# from td's own signed substitute store, else this fails closed (the non_blocking trade until
# #350 re-verifies the family blocking).
sh tools/resolve-seed.sh tests/sqlite-no-guix.lock || fail "resolve-seed could not supply the pinned lock closure (warm the host store or publish the seed substitutes)"
grep ' /gnu/store/' "$newlock" | sed 's/^[^ ]* //' > "$b8/roots"
"$TB" store-query "$TD_BUILDER_DB" references 2>/dev/null | sed 's/^[^|]*|//' | grep '^/gnu/store/' >> "$b8/roots" || true
sort -u "$b8/roots" -o "$b8/roots"
# Warm seed capture by CONTENT-SCAN: TD_SEED_DB is a store DIRECTORY, so the capture walks the
# closure from the roots by scanning bytes — no packager-private DB in the path (#311).
seedline=`TB="$TB" TD_SEED_DB=/gnu/store sh tools/warm-seed.sh "$ROOT/.td-build-cache/seed-b8-sqlite" $(cat "$b8/roots")` || fail "warm-seed failed"
WSTORE=`echo "$seedline" | cut -d' ' -f1`; WDB=`echo "$seedline" | cut -d' ' -f2`
for p in "$TCP" "$GLP8"; do cp -a "$bstore/`basename "$p"`" "$WSTORE/`basename "$p"`"; done
chmod -R u+w "$WSTORE/`basename "$TCP"`" "$WSTORE/`basename "$GLP8"`" 2>/dev/null || true
# emit the sqlite recipe via td's OWN Rust recipe evaluator (build it ourselves when the
# build-recipes prelude hasn't run — standalone `check` runs carry no prelude).
load_recipe_eval 2>/dev/null || {
  sh tests/recipe-eval-tool.sh "$PWD/.td-build-cache/recipe-eval" >/dev/null || fail "could not build td-recipe-eval (recipe-eval-tool)"
  load_recipe_eval || fail "no td-recipe-eval after recipe-eval-tool"
}
sh tests/recipe-emit.sh sqlite > "$b8/sqlite.json" || fail "recipe-emit sqlite"
mkdir -p "$b8/sb" "$b8/tmp"; cu=`grep -- '-coreutils-' "$newlock" | sed 's/^[^ ]* //' | head -1`
env -i HOME="$b8" TMPDIR="$b8/tmp" PATH="$cu/bin:$csh" \
  TD_BUILDER_PATH="$TD_BUILDER_PATH" TD_BUILDER_STORE="$TD_BUILDER_STORE" TD_BUILDER_DB="$TD_BUILDER_DB" \
  TD_SEED_STORE="$WSTORE" TD_SEED_DB="$WDB" TD_EXTRA_DBS="$bgdb:$btdb" \
  "$TB" build-recipe "$b8/sqlite.json" "$newlock" "$b8/sb" "$WDB" >"$b8/sb.out" 2>"$b8/sb.err" \
  || { tail -25 "$b8/sb.err" >&2; fail "build-recipe sqlite with the /td/store toolchain"; }
o=`sed -n 's/^OUT=out //p' "$b8/sb.out"`; test -n "$o" || fail "sqlite produced no output"
sdir="$b8/sb/newstore/`basename "$o"`"; sqb="$sdir/bin/sqlite3"; test -x "$sqb" || fail "no sqlite3 binary"
# (a) the /td/store toolchain linked it: interp is the /td/store glibc 2.41.
si=`"$BMB/bin/readelf" -l "$sqb" 2>/dev/null | grep -o "$GLP8/lib/ld-linux.so.2" | head -1`
test -n "$si" || fail "sqlite3 not linked vs the /td/store glibc 2.41"
# (b) [no-guix-toolchain] NO reference to the seed gcc-toolchain (the substituted-OUT compiler).
if grep -q -a -- "$oldtc" "$sqb"; then fail "sqlite3 references the substituted-out gcc-toolchain $oldtc"; fi
echo "   [no-guix-toolchain] build-recipe built sqlite 3.51.0 with the /td/store toolchain; interp=$si; no seed gcc-toolchain ref"
# (c) [DURABLE behavioral] sqlite3 runs in an own-root holding the /td/store glibc 2.41 + a
# static bash, driven as the LADDER drives it (store-register's parser-oracle role):
#   - PRAGMA integrity_check = ok over a td-WRITTEN store DB ($bgdb — store-add-recursive
#     wrote those SQLite bytes when interning glibc above);
#   - ValidPaths reads back the interned glibc /td/store path (content-addressed, so the
#     match is self-discriminating);
#   - a real SQL write/read round-trip on the ns tmpfs (CREATE/INSERT/SELECT SUM → 42);
#   - a garbage non-DB file must make it FAIL (the parser oracle is not vacuous).
vs="$b8/verify"; mkdir -p "$vs"; glb=`basename "$GLP8"`; sq2=`basename "$o"`
cp -a "$bstore/$glb" "$vs/$glb"; cp -a "$sdir" "$vs/$sq2"
# the static-bash fixture is a DECLARED gate input (#353): the runner resolved it.
bs=${TD_GATE_INPUT_BASH_STATIC:-}
test -n "$bs" || fail "TD_GATE_INPUT_BASH_STATIC unset — run via td-builder gate-run, which resolves the gate's declared inputs"
test -x "$bs/bin/bash" || fail "no static bash fixture at $bs"
bb8=`basename "$bs"`; cp -a "$bs" "$vs/$bb8"; chmod -R u+w "$vs"
cp "$bgdb" "$vs/td-store.db"
printf 'this is not a sqlite database\n' > "$vs/td-bad.db"
snscript='[ -e /gnu/store ] && echo GNU-PRESENT || echo GNU-ABSENT
sq=/td/store/'"$sq2"'/bin/sqlite3
export LD_LIBRARY_PATH=/td/store/'"$sq2"'/lib
ic=`"$sq" /td/store/td-store.db "PRAGMA integrity_check"`; echo "IC=$ic"
"$sq" /td/store/td-store.db "SELECT path FROM ValidPaths ORDER BY path"
sum=`"$sq" /tmp/t.db "CREATE TABLE t(v INTEGER); INSERT INTO t VALUES(41),(1); SELECT SUM(v) FROM t"`; echo "SUM=$sum"
"$sq" /td/store/td-bad.db "PRAGMA integrity_check" >/dev/null 2>&1 && echo "BAD=accepted" || echo "BAD=rejected"'
snout=`"$TB" store-ns "$vs" -- "/td/store/$bb8/bin/bash" -c "$snscript" 2>&1` || { printf '%s\n' "$snout" | sed 's/^/     /' >&2; fail "store-ns sqlite3 probe exited nonzero"; }
printf '%s\n' "$snout" | sed 's/^/     /' >&2
echo "$snout" | grep -q '^GNU-ABSENT$' || fail "/gnu/store is PRESENT in the own-root — mixed with the seed store"
echo "$snout" | grep -q '^IC=ok$' || fail "sqlite3 integrity_check over td's store DB did not return ok"
# the interned glibc path can appear in snout ONLY via the ValidPaths SELECT (nothing else
# prints it), and it embeds the content hash — a self-discriminating read-back.
echo "$snout" | grep -q -- "^$GLP8\$" || fail "sqlite3 did not read the interned glibc path back from td's ValidPaths"
echo "$snout" | grep -q '^SUM=42$' || fail "sqlite3 SQL write/read round-trip did not return 42"
echo "$snout" | grep -q '^BAD=rejected$' || fail "sqlite3 accepted a garbage non-DB (the parser oracle is vacuous)"
echo "   [DURABLE behavioral] sqlite3 runs from /td/store: integrity_check=ok on td-written DB bytes, ValidPaths reads back $GLP8, SQL round-trip → 42, garbage rejected"
echo "   [structural] inside td's own root /td/store IS the store AND /gnu/store is ABSENT"
rm -rf "$ROOT/.td-build-cache/seed-b8-sqlite" 2>/dev/null || true

echo "PASS: /td/store harness userland (#312, sqlite): from the 229-byte seed td built the chain → the"
echo "      full /td/store toolchain (gcc 14.3.0 + binutils 2.44 + glibc 2.41, warm via the shared chain"
echo "      cache), then with that toolchain (NOT the lock's pinned gcc-toolchain-15.2.0) td-builder"
echo "      build-recipe built sqlite 3.51.0 from its recipe: it links the /td/store glibc 2.41,"
echo "      references no seed gcc-toolchain, and runs in the own-root as the ladder's parser oracle —"
echo "      integrity_check + ValidPaths over a td-written store DB, a real SQL round-trip → 42, a"
echo "      garbage non-DB rejected — /gnu/store ABSENT. Seed provisioning is guix-process-free"
echo "      (resolve-seed + content-scan warm-seed)."

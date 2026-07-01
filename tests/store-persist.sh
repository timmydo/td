#!/bin/sh
# tests/store-persist.sh — the LOOP builds a corpus package into a PERSISTENT /td/store
# + DB, and a SEPARATE `td-builder` invocation SKIPS the rebuild by reading it back:
# incremental /td/store, build-into / read-back across builds, wired into the BUILD PATH
# (not a test-only subcommand), own-root with /gnu/store ABSENT.
#
# Reuses the store-native corpus path (tests/bootstrap-sed-corpus-store-native.sh, gate
# 416): from the 229-byte seed `bootstrap_modern_toolchain` builds the full /td/store
# toolchain (gcc 14.3.0 + binutils 2.44 + glibc 2.41), which is assembled into a
# guix-gcc-toolchain-shaped /td/store toolchain and substituted for guix's. Then
# `td-builder build-recipe` builds GNU sed 4.9 with it — CANONICALLY at /td/store
# (TD_STORE_DIR=/td/store) — into a PERSISTENT store P (TD_PERSIST_STORE/TD_PERSIST_DB):
#   * Invocation 1 (build-into): CACHE=miss — sed is built and interned into P + merged
#     into P's accumulating DB.
#   * Invocation 2 (SEPARATE process, fresh scratch): CACHE=persist — the build path
#     finds sed already valid in P and SKIPS the build, reading it back.
#   * The sed READ BACK FROM P runs in the own-root (/gnu/store ABSENT) and transforms
#     foo->bar.
#
# guix is only the one-time seed-capture source (inside warm-seed) + the seed toolchain
# (§5, retired last); the sed build reads the td-owned seed DB, not /var/guix.
#
# Legs (DURABLE — no guix oracle):
#   [DURABLE build-into]  build-recipe builds sed at /td/store and interns it into the
#                         fresh persistent store P (CACHE=miss; P/db created).
#   [DURABLE skip/read-back] a SEPARATE invocation SKIPS the build (CACHE=persist) — the
#                         build path read sed back from P; same output path.
#   [DURABLE behavioral]  the sed READ BACK FROM P runs in the own-root, /gnu/store
#                         ABSENT, and substitutes foo->bar.
#   [structural]          sed's canonical path is /td/store; its interp is /td/store
#                         glibc 2.41; no guix gcc-toolchain ref.
#
# Verified-red: builder/src/main.rs persistent_realization — neuter the `got != hash`
# integrity check → the persist unit test reds; and skipping the build-into (drop the
# TD_PERSIST_* on invocation 1) makes invocation 2 a CACHE=miss (rebuild, not a skip).
set -eu

ROOT=$(pwd)
fail() { echo "FAIL: $*" >&2; exit 1; }

# The ~850-line seed→…→gcc-14.3.0+binutils-2.44+glibc-2.41 chain (shared library).
. tests/bootstrap-chain.sh
bootstrap_modern_toolchain   # from the seed: builds + verifies the toolchain; sets GCC14/GLIBC241/BMB244SB/CC1/cpath/KH_TB

. tests/cache-lib.sh
export TD_STAGE0_BASE="`pwd`/.td-build-cache/td-shell"
load_stage0 || fail "stage0-builder could not place a guix-free stage0 td-builder"
csh=`command -v bash 2>/dev/null || command -v sh`

# --- Assemble a guix-gcc-toolchain-shaped /td/store toolchain (as gate 416) ------------------
b8=`mktemp -d`; bstore="$b8/seed-store"; bgdb="$b8/glibc.db"; btdb="$b8/toolchain.db"; mkdir -p "$bstore"
export TD_STORE_DIR=/td/store
BMB="$BMB244SB"
BUILDBASH=`grep -- '-bash-5.2.37 ' tests/sed-no-guix.lock | grep -v -e static -e minimal | sed 's/^[^ ]* //' | head -1`/bin/bash
case "$BUILDBASH" in /gnu/store/*-bash-*/bin/bash) ;; *) fail "could not resolve the lock's bash" ;; esac
GLP8=`"$TB" store-add-recursive glibc-2.41 "$GLIBC241" "$bstore" "$bgdb"` || fail "store-add glibc-2.41 failed"
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
echo "   [toolchain] assembled /td/store gcc-toolchain: $TCP (glibc $GLP8)"

# --- Substitute the /td/store toolchain into sed's lock + warm the seed inputs (as 416) -------
oldtc=`grep -- '-gcc-toolchain-' tests/sed-no-guix.lock | head -1 | sed 's/^[^ ]* //'`
test -n "$oldtc" || fail "no gcc-toolchain in sed-no-guix.lock"
newlock="$b8/sed.lock"
sed "s|^[^ ]*-gcc-toolchain-[^ ]* .*|gcc-toolchain $TCP seed\nglibc-2.41 $GLP8 seed|" tests/sed-no-guix.lock > "$newlock"
grep ' /gnu/store/' "$newlock" | sed 's/^[^ ]* //' > "$b8/roots"
"$TB" store-query "$TD_BUILDER_DB" references 2>/dev/null | sed 's/^[^|]*|//' | grep '^/gnu/store/' >> "$b8/roots" || true
sort -u "$b8/roots" -o "$b8/roots"
xargs guix build < "$b8/roots" >/dev/null 2>&1 || fail "could not realize the guix seed closure"
seedline=`TB="$TB" sh tools/warm-seed.sh "$ROOT/.td-build-cache/seed-persist" $(cat "$b8/roots")` || fail "warm-seed failed"
WSTORE=`echo "$seedline" | cut -d' ' -f1`; WDB=`echo "$seedline" | cut -d' ' -f2`
for p in "$TCP" "$GLP8"; do cp -a "$bstore/`basename "$p"`" "$WSTORE/`basename "$p"`"; done
chmod -R u+w "$WSTORE/`basename "$TCP"`" "$WSTORE/`basename "$GLP8"`" 2>/dev/null || true
load_recipe_eval || fail "no td-recipe-eval (the build-recipes prelude must run first)"
sh tests/recipe-emit.sh sed > "$b8/sed.json" || fail "recipe-emit sed"
cu=`grep -- '-coreutils-' "$newlock" | sed 's/^[^ ]* //' | head -1`; mkdir -p "$b8/tmp"

# === Persistent /td/store P — the incremental store the loop builds into. ===
P="$b8/P"; mkdir -p "$P/store"
build_sed() {   # build sed at /td/store into the persistent store P; $1 = scratch dir
  mkdir -p "$1"
  env -i HOME="$b8" TMPDIR="$b8/tmp" PATH="$cu/bin:$csh" \
    TD_BUILDER_PATH="$TD_BUILDER_PATH" TD_BUILDER_STORE="$TD_BUILDER_STORE" TD_BUILDER_DB="$TD_BUILDER_DB" \
    TD_SEED_STORE="$WSTORE" TD_SEED_DB="$WDB" TD_EXTRA_DBS="$bgdb:$btdb" \
    TD_STORE_DIR=/td/store TD_PERSIST_STORE="$P/store" TD_PERSIST_DB="$P/db" \
    "$TB" build-recipe "$b8/sed.json" "$newlock" "$1" "$WDB"
}

# --- Invocation 1: BUILD sed at /td/store, interning into the fresh persistent store P -------
build_sed "$b8/sb" >"$b8/sb.out" 2>"$b8/sb.err" || { tail -25 "$b8/sb.err" >&2; fail "invocation 1: build-recipe sed at /td/store"; }
grep -qx 'CACHE=miss' "$b8/sb.out" || { cat "$b8/sb.out" >&2; fail "invocation 1 should be a build (CACHE=miss)"; }
o=`sed -n 's/^OUT=out //p' "$b8/sb.out"`; test -n "$o" || fail "sed produced no output"
case "$o" in /td/store/*-sed*) ;; *) fail "sed output is not canonically at /td/store: $o" ;; esac
sbase=`basename "$o"`
test -d "$P/store/$sbase" || fail "build-into: sed not interned into the persistent /td/store P"
test -s "$P/db" || fail "build-into: persistent DB not created"
echo "   [DURABLE build-into] build-recipe built sed at /td/store ($o) and interned it into the fresh persistent store P (CACHE=miss)"

# --- Invocation 2 (SEPARATE process, FRESH scratch): SKIP — read sed back from P -------------
build_sed "$b8/sb2" >"$b8/sb2.out" 2>"$b8/sb2.err" || { tail -25 "$b8/sb2.err" >&2; fail "invocation 2: build-recipe sed (expected a persistent-store skip)"; }
grep -qx 'CACHE=persist' "$b8/sb2.out" || { cat "$b8/sb2.out" >&2; fail "invocation 2 should SKIP from the persistent store (CACHE=persist), not rebuild"; }
o2=`sed -n 's/^OUT=out //p' "$b8/sb2.out"`; test "$o" = "$o2" || fail "the skipped output ($o2) differs from the built one ($o)"
echo "   [DURABLE skip/read-back] a SEPARATE invocation SKIPPED the build (CACHE=persist) — the build path read sed back from P at the same /td/store path"

# --- [DURABLE behavioral] run the sed READ BACK FROM P in the own-root (/gnu/store ABSENT) ----
si=`"$BMB/bin/readelf" -l "$P/store/$sbase/bin/sed" 2>/dev/null | grep -o "$GLP8/lib/ld-linux.so.2" | head -1`
test -n "$si" || fail "the persisted sed is not linked vs the /td/store glibc 2.41"
if grep -q -a -- "$oldtc" "$P/store/$sbase/bin/sed"; then fail "the persisted sed references the substituted-out gcc-toolchain $oldtc"; fi
vs="$b8/verify"; mkdir -p "$vs"; glb=`basename "$GLP8"`
cp -a "$bstore/$glb" "$vs/$glb"
cp -a "$P/store/$sbase" "$vs/$sbase"        # <-- from the PERSISTENT store P, not the build scratch
# Resolve the own-root runner (a static bash) from the WARM-SEED db, not /var/guix — so
# no guix private-state read appears here (the seed capture, inside warm-seed, is the only
# guix source; directive 8: the should-shrink guix surface stays flat).
bashlock=`grep -- '-bash-' tests/sed-no-guix.lock | grep -v static | sed 's/^[^ ]* //' | head -1`
bs8=`"$TB" store-closure "$WDB" "$bashlock" | grep -- '-bash-static-' | head -1`
bb8=`basename "$bs8"`; cp -a "$bs8" "$vs/$bb8"; chmod -R u+w "$vs"
sedrun='[ -e /gnu/store ] && echo GNU-PRESENT || echo GNU-ABSENT
printf "foo\nbaz\n" | /td/store/'"$sbase"'/bin/sed "s/foo/bar/"'
g8=`"$TB" store-ns "$vs" -- "/td/store/$bb8/bin/bash" -c "$sedrun" 2>&1` || { echo "$g8" | sed 's/^/     /' >&2; fail "store-ns sed run rc"; }
printf '%s\n' "$g8" | sed 's/^/     /' >&2
echo "$g8" | grep -q '^GNU-ABSENT$' || fail "/gnu/store is PRESENT in the own-root"
echo "$g8" | grep -q '^bar$' || fail "the persisted sed did not substitute foo->bar from /td/store: $g8"
echo "$g8" | grep -q '^baz$' || fail "the persisted sed dropped the unmatched line: $g8"
if echo "$g8" | grep -q '^foo$'; then fail "the persisted sed left its input unchanged: $g8"; fi
echo "   [DURABLE behavioral] the sed READ BACK FROM the persistent /td/store P runs in the own-root (/gnu/store ABSENT) and transforms foo->bar"
rm -rf "$ROOT/.td-build-cache/seed-persist" 2>/dev/null || true

echo "PASS: the loop BUILT a corpus package (GNU sed 4.9) at /td/store into a PERSISTENT td store + DB,"
echo "      and a SEPARATE invocation SKIPPED the rebuild by reading it back — build-into / read-back"
echo "      across builds, wired into the BUILD PATH (TD_PERSIST_STORE/TD_PERSIST_DB), own-root with"
echo "      /gnu/store ABSENT. The incremental /td/store the loop builds into (scenario 1: skip-rebuild)."

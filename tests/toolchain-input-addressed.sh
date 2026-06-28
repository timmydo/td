#!/bin/sh
# tests/toolchain-input-addressed.sh — task 2a: give the /td/store modern toolchain a
# STABLE KEY. The toolchain (gcc-14.3.0 + binutils-2.44 + glibc-2.41, gate 412) is NOT
# byte-reproducible (cc1 stamp, ar/install mtimes), so `store-add-recursive`'s
# content-addressed path VARIES build-to-build — a td-subst consumer can't name what to
# fetch. This proves an INPUT-ADDRESSED path instead: its digest is `td-builder
# toolchain-key tests/td-toolchain.lock` (a hash of the toolchain's DECLARED inputs), so
# the path is a pure function of the inputs — identical across non-reproducible rebuilds
# and computable from the lock BEFORE fetching. That stable key is the prereq for td-subst
# chain-caching (2b/2c). See plan/toolchain-input-addressed.md.
#
# Legs (ALL DURABLE — no guix oracle in any; this is td-native addressing end to end):
#   [pinned-sync]   td-toolchain.lock's input/patch pins match seed/sources + seed/patches
#                   (the lock can't drift from the real toolchain inputs).
#   [stable-key]    toolchain-key + the 3 components' toolchain-path are deterministic
#                   across repeated invocations and yield distinct /td/store paths.
#   [content-indep] (the crux) two INDEPENDENT builds of DIFFERENT bytes under the same key
#                   land at the SAME input-addressed path — while content-addressed
#                   store-add-recursive of the same two bytes lands at DIFFERENT paths
#                   (the problem this fixes). Both register their REAL (differing) NAR hash.
#   [load-bearing]  perturbing ONE declared input pin moves the path (inputs are real).
#   [behavioral]    a real binary placed at an input-addressed /td/store path RUNS in the
#                   store-ns own-root.
#   [structural]    /gnu/store is ABSENT in that own-root.
set -eu
fail() { echo "FAIL: $*" >&2; exit 1; }
sha() { sha256sum "$1" | cut -d' ' -f1; }
LOCK=tests/td-toolchain.lock
test -f "$LOCK" || fail "missing $LOCK"

. tests/cache-lib.sh
load_stage0 || fail "stage0-builder could not place a guix-free stage0 td-builder"
echo ">> td-builder (stage0, guix-free): $TB"

work=`mktemp -d`
trap 'chmod -R u+w "$work" 2>/dev/null || true; rm -rf "$work"' EXIT INT TERM

# --- [pinned-sync] every lock pin mirrors the seed source / patch it names --------------------
# Build a FILE->sha256 map from seed/sources/*.lock (the `file`/`sha256` fields).
srcsha() {
  for l in seed/sources/*.lock; do
    f=`sed -n 's/^file //p' "$l" | head -1`
    [ "$f" = "$1" ] && { sed -n 's/^sha256 //p' "$l" | head -1; return 0; }
  done
  return 1
}
nin=0; npatch=0
while read -r kind shaval file; do
  case "$kind" in
    input)
      want=`srcsha "$file"` || fail "[pinned-sync] no seed/sources/*.lock declares file `$file`"
      [ "$shaval" = "$want" ] || fail "[pinned-sync] $file: lock pin $shaval != seed pin $want"
      nin=$((nin + 1)) ;;
    patch)
      pf="seed/patches/$file"
      test -f "$pf" || fail "[pinned-sync] vendored patch missing: $pf"
      got=`sha "$pf"`
      [ "$shaval" = "$got" ] || fail "[pinned-sync] $file: lock pin $shaval != file sha $got"
      npatch=$((npatch + 1)) ;;
  esac
done <<EOF
`grep -E '^(input|patch) ' "$LOCK"`
EOF
test "$nin" -ge 20 || fail "[pinned-sync] only $nin input pins — the toolchain has more inputs than that"
test "$npatch" -ge 4 || fail "[pinned-sync] only $npatch patch pins"
echo "   [pinned-sync] $nin source pins + $npatch patch pins match seed/sources + seed/patches"

# --- [stable-key] the key + component paths are deterministic and distinct --------------------
export TD_STORE_DIR=/td/store
K1=`"$TB" toolchain-key "$LOCK"`; K2=`"$TB" toolchain-key "$LOCK"`
[ "$K1" = "$K2" ] || fail "[stable-key] toolchain-key not deterministic ($K1 vs $K2)"
case "$K1" in *[!0-9a-f]* | "") fail "[stable-key] key is not a hex digest: $K1" ;; esac
GCCP=`"$TB" toolchain-path "$LOCK" gcc-14.3.0`
BUP=`"$TB" toolchain-path "$LOCK" binutils-2.44`
GLP=`"$TB" toolchain-path "$LOCK" glibc-2.41`
for p in "$GCCP" "$BUP" "$GLP"; do
  case "$p" in /td/store/*) ;; *) fail "[stable-key] not a /td/store path: $p" ;; esac
done
[ "`"$TB" toolchain-path "$LOCK" gcc-14.3.0`" = "$GCCP" ] || fail "[stable-key] toolchain-path not deterministic"
[ "$GCCP" != "$BUP" ] && [ "$GCCP" != "$GLP" ] && [ "$BUP" != "$GLP" ] || fail "[stable-key] components collide"
echo "   [stable-key] key=$K1; gcc/binutils/glibc each get a distinct, deterministic /td/store path"

# --- [content-indep] same key, different bytes -> SAME input-addressed path -------------------
# (the contrast: content-addressed store-add-recursive moves with the bytes.)
mkdir -p "$work/v1/bin" "$work/v2/bin"
printf 'AAAAA\n' > "$work/v1/bin/x"; printf 'BBBBB-different\n' > "$work/v2/bin/x"
IA1=`"$TB" store-add-input-addressed glibc-2.41 "$K1" "$work/v1" "$work/iaA" "$work/iaA.db"` || fail "store-add-input-addressed v1"
IA2=`"$TB" store-add-input-addressed glibc-2.41 "$K1" "$work/v2" "$work/iaB" "$work/iaB.db"` || fail "store-add-input-addressed v2"
[ "$IA1" = "$IA2" ] || fail "[content-indep] input-addressed path moved with content ($IA1 vs $IA2)"
[ "$IA1" = "$GLP" ] || fail "[content-indep] producer path $IA1 != toolchain-path $GLP (consumer can't predict it)"
CA1=`"$TB" store-add-recursive glibc-2.41 "$work/v1" "$work/caA" "$work/caA.db"` || fail "store-add-recursive v1"
CA2=`"$TB" store-add-recursive glibc-2.41 "$work/v2" "$work/caB" "$work/caB.db"` || fail "store-add-recursive v2"
[ "$CA1" != "$CA2" ] || fail "[content-indep] content-addressed paths did NOT move — fixture bytes are equal?"
# both input-addressed adds registered the REAL (differing) NAR hash, naming notwithstanding.
HA=`"$TB" store-query "$work/iaA.db" info | grep -F "$IA1|" | cut -d'|' -f2`
HB=`"$TB" store-query "$work/iaB.db" info | grep -F "$IA2|" | cut -d'|' -f2`
test -n "$HA" -a -n "$HB" || fail "[content-indep] input-addressed adds did not register a NAR hash"
[ "$HA" != "$HB" ] || fail "[content-indep] registered NAR hashes are equal — content integrity not recorded"
echo "   [content-indep] same key+different bytes -> same path $IA1 (content-addressed would split: $CA1 vs $CA2)"

# --- [load-bearing] perturbing one input pin moves the path ----------------------------------
# (sed, not awk — the loop sandbox has no awk; zero out the glibc-2.41 source pin.)
pert="$work/perturbed.lock"
sed 's/^input [0-9a-f]* glibc-2.41.tar.xz$/input 0000000000000000000000000000000000000000000000000000000000000000 glibc-2.41.tar.xz/' "$LOCK" > "$pert"
grep -q '^input 0000000000000000000000000000000000000000000000000000000000000000 glibc-2.41.tar.xz$' "$pert" || fail "[load-bearing] could not perturb the lock (glibc-2.41 input line not found)"
GLP_P=`"$TB" toolchain-path "$pert" glibc-2.41`
[ "$GLP_P" != "$GLP" ] || fail "[load-bearing] perturbing an input pin did NOT change the path"
echo "   [load-bearing] flipping one declared input pin moves glibc-2.41's path ($GLP -> $GLP_P)"

# --- [behavioral]+[structural] a real binary at an input-addressed path RUNS in the own-root --
# A static bash from hello's PINNED closure (td's own store-closure reader, no guix process) is
# a real runnable FIXTURE — placed input-addressed, then executed in the store-ns own-root.
bashpkg=`grep -- '-bash-' tests/hello-no-guix.lock | grep -v static | sed 's/^[^ ]* //' | head -1`
test -n "$bashpkg" || fail "no bash in hello's lock"
bs=`"$TB" store-closure /var/guix/db/db.sqlite "$bashpkg" | grep -- '-bash-static-' | head -1`
test -n "$bs" -a -x "$bs/bin/bash" || fail "no static bash in the closure of $bashpkg"
store="$work/store"; mkdir -p "$store"
RUNP=`"$TB" store-add-input-addressed bash-static "$K1" "$bs" "$store" "$work/store.db"` || fail "store-add-input-addressed bash-static"
case "$RUNP" in /td/store/*-bash-static) ;; *) fail "bash-static not input-addressed at /td/store: $RUNP" ;; esac
test -x "$store/`basename "$RUNP"`/bin/bash" || fail "interned bash-static missing physically"
out=`"$TB" store-ns "$store" -- "$RUNP/bin/bash" -c '[ -e /gnu/store ] && echo GNU-PRESENT || echo GNU-ABSENT; echo "RAN:$BASH_VERSION"'` \
  || { printf '%s\n' "$out" | sed 's/^/     /' >&2; fail "store-ns run from the input-addressed path exited nonzero"; }
printf '%s\n' "$out" | grep -q '^RAN:5' || fail "[behavioral] the binary did not run from its input-addressed /td/store path"
echo "   [behavioral] a real binary placed at the input-addressed path $RUNP RUNS in the own-root"
printf '%s\n' "$out" | grep -q '^GNU-ABSENT$' || fail "[structural] /gnu/store is PRESENT in the own-root"
echo "   [structural] /gnu/store is ABSENT in the own-root"

echo "PASS: toolchain-input-addressed — the /td/store modern toolchain has a STABLE input-addressed"
echo "  key (td-toolchain.lock + toolchain-key/path): a pure function of its declared inputs, so its"
echo "  path is identical across non-reproducible rebuilds and predictable from the lock — the prereq"
echo "  for td-subst chain-caching (2b/2c). A real binary placed there runs, /gnu/store absent."

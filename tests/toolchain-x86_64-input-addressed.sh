#!/bin/sh
# tests/toolchain-x86_64-input-addressed.sh — give the x86_64 /td/store toolchain (cross
# binutils-2.44 + cross gcc-14.3.0 + x86_64 glibc-2.41 + libgcc_s, built from the seed via
# run_x86_64_cross in tests/x86_64-cross-fns.sh, #201) a STABLE
# INPUT-ADDRESSED key — the x86_64 parallel of toolchain-input-addressed (#204, i686). Like
# i686, the toolchain is not byte-reproducible, so store-add-recursive's content-addressed
# path varies build-to-build and a td-subst consumer can't name what to fetch; the path must
# be a pure function of the DECLARED inputs instead (`td-builder toolchain-key/toolchain-path
# tests/td-toolchain-x86_64.lock`). This is the prereq for fetching the x86_64 toolchain
# instead of the ~90-min from-seed rebuild (the rust compile/userland rungs 3/4).
#
# The x86_64 toolchain consumes the SAME pinned source set as i686 (the cross is a BUILD
# configuration over identical sources), so ARCH is the key discriminator: a distinct `name`
# + x86_64 `component` names re-key it. The gate proves exactly that, with no source dup.
#
# Legs (ALL DURABLE — no guix oracle in any; td-native addressing end to end):
#   [pinned-sync]   the x86_64 lock's input/patch pins match seed/sources + seed/patches
#                   (the lock can't drift from the real toolchain inputs).
#   [arch-parity]   the x86_64 lock's input+patch SET is byte-identical to the i686 lock's,
#                   and the two locks differ ONLY in name / recipe-rev / component — so the
#                   x86_64 toolchain demonstrably shares i686's sources (no duplication).
#   [distinct-key]  ARCH is the discriminator: x86_64 key != i686 key, and every x86_64
#                   component path differs from i686's — no collision in one /td/store.
#   [stable-key]    toolchain-key + the 3 components' toolchain-path are deterministic across
#                   repeated invocations and yield distinct /td/store paths.
#   [load-bearing]  bumping recipe-rev moves the key; perturbing ONE input pin moves a path.
#   [behavioral]    a real binary placed at the x86_64-keyed input-addressed /td/store path
#                   RUNS in the store-ns own-root, /gnu/store ABSENT — the placement works.
set -eu
fail() { echo "FAIL: $*" >&2; exit 1; }
sha() { sha256sum "$1" | cut -d' ' -f1; }
LOCK=tests/td-toolchain-x86_64.lock
ILOCK=tests/td-toolchain.lock
test -f "$LOCK" || fail "missing $LOCK"
test -f "$ILOCK" || fail "missing $ILOCK (the i686 lock to compare against)"

. tests/cache-lib.sh
load_stage0 || fail "stage0-builder could not place a guix-free stage0 td-builder"
echo ">> td-builder (stage0, guix-free): $TB"

work=`mktemp -d`
trap 'chmod -R u+w "$work" 2>/dev/null || true; rm -rf "$work"' EXIT INT TERM

# --- [pinned-sync] every lock pin mirrors the seed source / patch it names ---------------------
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

# --- [arch-parity] the x86_64 lock shares i686's exact source set; only arch fields differ -----
# diff/cmp-free (the loop sandbox has neither): compare the sorted input+patch sets by sha256,
# and assert every non-comment directive in BOTH locks is one of name/recipe-rev/component/input/patch.
xh=`grep -E '^(input|patch) ' "$LOCK"  | sort | sha256sum`
ih=`grep -E '^(input|patch) ' "$ILOCK" | sort | sha256sum`
[ "$xh" = "$ih" ] || fail "[arch-parity] x86_64 input/patch set differs from i686 — the cross must reuse i686's sources"
for L in "$LOCK" "$ILOCK"; do
  bad=`grep -vE '^[[:space:]]*(#|$)' "$L" | sed -E 's/^([a-z-]+).*/\1/' | sort -u | grep -vE '^(name|recipe-rev|component|input|patch)$' | tr '\n' ' '`
  [ -z "$bad" ] || fail "[arch-parity] $L has an unexpected non-arch directive: $bad (only name/recipe-rev/component/input/patch allowed)"
done
echo "   [arch-parity] x86_64 lock shares i686's exact $nin+$npatch source set; only name/recipe-rev/component differ"

# --- [distinct-key] ARCH is the discriminator: distinct key + no component-path collision ------
export TD_STORE_DIR=/td/store
KX=`"$TB" toolchain-key "$LOCK"`
KI=`"$TB" toolchain-key "$ILOCK"`
[ "$KX" != "$KI" ] || fail "[distinct-key] x86_64 key collides with i686 ($KX) — arch did not re-key"
echo "   [distinct-key] x86_64 key $KX != i686 key $KI (arch re-keys with zero source duplication)"

# --- [stable-key] the key + component paths are deterministic, distinct, /td/store-rooted ------
K2=`"$TB" toolchain-key "$LOCK"`
[ "$KX" = "$K2" ] || fail "[stable-key] toolchain-key not deterministic ($KX vs $K2)"
case "$KX" in *[!0-9a-f]* | "") fail "[stable-key] key is not a hex digest: $KX" ;; esac
BUP=`"$TB" toolchain-path "$LOCK" binutils-2.44-x86_64`
GCCP=`"$TB" toolchain-path "$LOCK" gcc-14.3.0-x86_64`
GLP=`"$TB" toolchain-path "$LOCK" glibc-2.41-x86_64`
for p in "$BUP" "$GCCP" "$GLP"; do
  case "$p" in /td/store/*-*-x86_64) ;; *) fail "[stable-key] not an x86_64 /td/store path: $p" ;; esac
done
[ "`"$TB" toolchain-path "$LOCK" gcc-14.3.0-x86_64`" = "$GCCP" ] || fail "[stable-key] toolchain-path not deterministic"
[ "$GCCP" != "$BUP" ] && [ "$GCCP" != "$GLP" ] && [ "$BUP" != "$GLP" ] || fail "[stable-key] components collide"
# and each differs from i686's same-base component (no cross-arch path reuse).
[ "$GCCP" != "`"$TB" toolchain-path "$ILOCK" gcc-14.3.0`" ] || fail "[distinct-key] x86_64 gcc path == i686 gcc path"
echo "   [stable-key] key=$KX; cross binutils/gcc/glibc each get a distinct, deterministic x86_64 /td/store path"

# --- [load-bearing] recipe-rev AND an input pin each move the addressing -----------------------
sed 's/^recipe-rev 1$/recipe-rev 2/' "$LOCK" > "$work/rr.lock"
[ "`"$TB" toolchain-key "$work/rr.lock"`" != "$KX" ] || fail "[load-bearing] bumping recipe-rev did NOT move the key"
sed 's/^input [0-9a-f]* glibc-2.41.tar.xz$/input 0000000000000000000000000000000000000000000000000000000000000000 glibc-2.41.tar.xz/' "$LOCK" > "$work/pin.lock"
grep -q '^input 0000000000000000000000000000000000000000000000000000000000000000 glibc-2.41.tar.xz$' "$work/pin.lock" || fail "[load-bearing] could not perturb the glibc-2.41 input pin"
[ "`"$TB" toolchain-path "$work/pin.lock" glibc-2.41-x86_64`" != "$GLP" ] || fail "[load-bearing] perturbing an input pin did NOT move the path"
echo "   [load-bearing] recipe-rev bump moves the key; flipping one input pin moves glibc-2.41-x86_64's path"

# --- [behavioral]+[structural] a real binary at the x86_64-keyed path RUNS in the own-root -----
# A static bash from hello's PINNED closure (td's own store-closure reader, no guix process) is a
# real runnable FIXTURE — placed at the x86_64-keyed input-addressed path, run in the store-ns own-root.
# the static-bash fixture is a DECLARED gate input (#353): the runner
# content-scanned hello's bash closure and exported the unique bash-static member.
bs=${TD_GATE_INPUT_BASH_STATIC:-}
test -n "$bs" || fail "TD_GATE_INPUT_BASH_STATIC unset — run via td-builder gate-run, which resolves the gate's declared inputs"
test -x "$bs/bin/bash" || fail "no static bash fixture at $bs"
store="$work/store"; mkdir -p "$store"
RUNP=`"$TB" store-add-input-addressed bash-static-x86_64 "$KX" "$bs" "$store" "$work/store.db"` || fail "store-add-input-addressed bash-static-x86_64"
case "$RUNP" in /td/store/*-bash-static-x86_64) ;; *) fail "fixture not input-addressed at /td/store: $RUNP" ;; esac
test -x "$store/`basename "$RUNP"`/bin/bash" || fail "interned fixture missing physically"
out=`"$TB" store-ns "$store" -- "$RUNP/bin/bash" -c '[ -e /gnu/store ] && echo GNU-PRESENT || echo GNU-ABSENT; echo "RAN:$BASH_VERSION"'` \
  || { printf '%s\n' "$out" | sed 's/^/     /' >&2; fail "store-ns run from the x86_64-keyed input-addressed path exited nonzero"; }
printf '%s\n' "$out" | grep -q '^RAN:5' || fail "[behavioral] the binary did not run from its x86_64-keyed /td/store path"
printf '%s\n' "$out" | grep -q '^GNU-ABSENT$' || fail "[structural] /gnu/store is PRESENT in the own-root"
echo "   [behavioral] a real binary placed at the x86_64-keyed path $RUNP RUNS in the own-root, /gnu/store ABSENT"

echo "PASS: toolchain-x86_64-input-addressed — the x86_64 /td/store toolchain has a STABLE input-addressed"
echo "  key (td-toolchain-x86_64.lock + toolchain-key/path): a pure function of its declared inputs, sharing"
echo "  i686's exact source set with ARCH (name+components) as the sole discriminator — distinct from i686,"
echo "  predictable from the lock across non-reproducible rebuilds. The prereq for fetching the x86_64"
echo "  toolchain instead of the ~90-min from-seed rebuild (rust compile/userland rungs 3/4)."

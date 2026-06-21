#!/bin/sh
# tests/seed-tarball.sh — North-Star step 2 (CLAUDE.md), PR1: CAPTURE a toolchain seed
# closure into a frozen, portable tarball + manifest, and prove the seed survives the
# tarball byte-for-byte. tools/build-seed-tarball.sh tars the GC closure of a seed root
# (td-builder's own store-closure + nar-hash — td's SQLite reader + NAR serializer) and
# writes a `<path> <nar-hash>` manifest; this gate extracts the tar into a FRESH dir and
# asserts every tree is NAR-IDENTICAL to the manifest. That round-trip is durable: it
# holds with no guix in the room (it is td's own NAR both times), and it is what
# `td-builder seed-unpack` (next PR) needs — restore the seed into a td store, register
# from the manifest, build from it with no guix install.
#
# Bounded to one real seed root (hello's pinned bash, closure incl. glibc) so the gate
# is fast; the tool captures the whole toolchain the same way. The td-builder is the
# guix-free stage0. guix is only the SOURCE the seed is captured FROM (its store DB) +
# the removable closure oracle (`guix gc -R`) — no `guix build -e (system M)` packager.
#
# Legs:
#   [DURABLE structural]  the manifest covers the COMPLETE closure (count == store-closure)
#   [DURABLE round-trip]  every captured path is NAR-identical after tar -> extract
#                         (the seed survives the tarball, byte-faithful)
#   [REMOVABLE oracle]    td's captured closure == `guix gc -R ROOT` (matches guix)
set -eu

fail() { echo "FAIL: $*" >&2; exit 1; }

. tests/cache-lib.sh
export TD_STAGE0_BASE="`pwd`/.td-build-cache/td-shell"
load_stage0 || fail "stage0-builder could not place a guix-free stage0 td-builder"
echo ">> td-builder under test (stage0, guix-free): $TB"

work=`mktemp -d`
trap 'chmod -R u+w "$work" 2>/dev/null || true; rm -rf "$work"' EXIT INT TERM

# One real seed root: hello's pinned bash (a toolchain seed; closure pulls in glibc).
root=`grep -- '-bash-' tests/hello-no-guix.lock | sed 's/^[^ ]* //' | head -1`
test -n "$root" || fail "no bash seed in tests/hello-no-guix.lock"
guix build "$root" >/dev/null 2>&1 || fail "seed root $root is not realized (warm it first)"

# --- CAPTURE (the tool) -------------------------------------------------------
TB="$TB" TD_SEED_DB=/var/guix/db/db.sqlite sh tools/build-seed-tarball.sh "$work/cap" "$root" >/dev/null \
  || fail "build-seed-tarball failed"
test -s "$work/cap/seed.tar" -a -s "$work/cap/seed.manifest" || fail "no seed.tar / seed.manifest produced"
n=`grep -c . "$work/cap/seed.manifest"`
echo "   captured $n seed paths from `basename "$root"` -> seed.tar (`du -h "$work/cap/seed.tar" | cut -f1`)"

# --- [DURABLE structural] the manifest is the COMPLETE closure -----------------
exp=`"$TB" store-closure /var/guix/db/db.sqlite "$root" | sort -u | grep -c .`
test "$n" -eq "$exp" || fail "manifest has $n paths but the closure is $exp — incomplete capture"
echo "   [DURABLE structural] manifest covers the complete closure ($n == $exp paths)"

# --- [DURABLE round-trip] tar -> extract -> NAR-identical ----------------------
mkdir -p "$work/dest"
tar xf "$work/cap/seed.tar" -C "$work/dest" || fail "could not extract seed.tar"
checked=0
while read -r p h; do
  [ -n "$p" ] || continue
  rebased="$work/dest$p"
  test -e "$rebased" || fail "seed.tar is missing $p"
  got=`"$TB" nar-hash "$rebased"` || fail "nar-hash failed on the extracted $p"
  test "$got" = "$h" || fail "NAR mismatch after round-trip for $p (tar=$got manifest=$h)"
  checked=$((checked + 1))
done < "$work/cap/seed.manifest"
test "$checked" -eq "$n" || fail "round-trip checked $checked of $n paths"
echo "   [DURABLE round-trip] all $checked captured paths are NAR-identical after tar -> extract (seed survives the tarball)"

# --- [REMOVABLE oracle] td's closure == guix's `gc -R` -------------------------
guix gc -R "$root" | sort -u > "$work/oracle"
"$TB" store-closure /var/guix/db/db.sqlite "$root" | sort -u > "$work/tdclosure"
extra=`cat "$work/tdclosure" "$work/oracle" | sort | uniq -u | head -3`
test -z "$extra" || fail "td's captured closure differs from guix gc -R: $extra"
echo "   [REMOVABLE oracle] td's captured closure == guix gc -R $(basename "$root")"

echo "PASS: td captured a toolchain seed closure into a frozen tarball + manifest (its own"
echo "      store-closure + NAR serializer) and the seed is NAR-identical after a tar round-trip"
echo "      — the frozen seed survives the tarball, complete and byte-faithful (North-Star step 2 PR1)."

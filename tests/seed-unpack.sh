#!/bin/sh
# tests/seed-unpack.sh — North-Star step 2 (CLAUDE.md), PR2: RESTORE a frozen seed
# tarball into a td-owned store + register it, with NO daemon and NO /gnu/store write.
# `td-builder seed-unpack` extracts the tarball into a fresh DEST-STORE, verifies every
# restored tree is NAR-identical to the manifest, and writes DEST-DB (ValidPaths + Refs)
# FROM the manifest. This gate proves the seed survives the tarball INTO a usable td
# store: td's OWN store-closure reads a complete closure back out of DEST-DB (no daemon),
# every path present + NAR-identical. That registered seed store is what a build will
# stage from with no guix install (PR3 — build hello from it).
#
# Bounded to one real seed root (hello's pinned bash, closure incl. glibc). td-builder is
# the guix-free stage0; guix is only the capture SOURCE + the removable closure oracle.
#
# Legs:
#   [DURABLE round-trip]  seed-unpack restores + NAR-verifies the whole closure (no daemon)
#   [DURABLE structural]  every restored path is present + NAR-identical in the td store
#   [DURABLE structural]  td's reader sees the COMPLETE closure in the unpacked DEST-DB
#   [REMOVABLE oracle]    the unpacked closure == `guix gc -R ROOT`
set -eu

fail() { echo "FAIL: $*" >&2; exit 1; }

. tests/cache-lib.sh
export TD_STAGE0_BASE="`pwd`/.td-build-cache/td-shell"
load_stage0 || fail "stage0-builder could not place a guix-free stage0 td-builder"
echo ">> td-builder under test (stage0, guix-free): $TB"

work=`mktemp -d`
trap 'chmod -R u+w "$work" 2>/dev/null || true; rm -rf "$work"' EXIT INT TERM

root=`grep -- '-bash-' tests/hello-no-guix.lock | sed 's/^[^ ]* //' | head -1`
test -n "$root" || fail "no bash seed in tests/hello-no-guix.lock"
guix build "$root" >/dev/null 2>&1 || fail "seed root $root is not realized"

# CAPTURE (the tool) -> tar + manifest.
TB="$TB" TD_SEED_DB=/var/guix/db/db.sqlite sh tools/build-seed-tarball.sh "$work/cap" "$root" >/dev/null \
  || fail "build-seed-tarball failed"
n=`grep -c . "$work/cap/seed.manifest"`
echo "   captured $n seed paths from `basename "$root"`"

# UNPACK into a FRESH td store (no daemon, no /gnu/store write).
got=`"$TB" seed-unpack "$work/cap/seed.tar" "$work/cap/seed.manifest" "$work/store" "$work/seed.db"` \
  || fail "seed-unpack failed"
test "$got" -eq "$n" || fail "seed-unpack registered $got of $n paths"
echo "   [DURABLE round-trip] seed-unpack restored + NAR-verified $got paths into a td store (no daemon)"

# Every restored tree present + NAR-identical in the td store.
while read -r p h _size _refs; do
  [ -n "$p" ] || continue
  test -e "$work/store$p" || fail "the unpacked store is missing $p"
  test "`"$TB" nar-hash "$work/store$p"`" = "$h" || fail "restored $p is not NAR-identical"
done < "$work/cap/seed.manifest"
echo "   [DURABLE structural] every restored path is present + NAR-identical in the td store"

# td's OWN reader sees a COMPLETE closure in the unpacked DB — no daemon.
"$TB" store-closure "$work/seed.db" "$root" | sort -u > "$work/reg"
regn=`grep -c . "$work/reg"`
test "$regn" -eq "$n" || fail "DEST-DB closure of $root is $regn, manifest is $n — incomplete registration"
echo "   [DURABLE structural] td's reader reads the COMPLETE closure from the unpacked DB ($regn == $n)"

# [DURABLE] td re-derives the DB's reachability by CONTENT-SCANNING the unpacked bytes
# (store-closure-scan = the daemon's scanForReferences/scan.rs, bounded to the seed
# store) — and it must equal td's own store-db walk over the manifest-built DB. So the
# seed DB's references faithfully reflect the BYTES, not merely the refs the manifest
# recorded at capture time. No store DB in the scan, no guix.
seedstore="$work/store/gnu/store"
scan_cl=`"$TB" store-closure-scan "$seedstore" "$seedstore/$(basename "$root")" | sed 's#.*/##' | sort -u` \
  || fail "store-closure-scan failed"
db_cl=`sed 's#.*/##' "$work/reg" | sort -u`
test -n "$scan_cl" || fail "store-closure-scan produced an empty closure"
test "$scan_cl" = "$db_cl" \
  || { printf '%s\n' "$scan_cl" | sed 's/^/  scan: /' >&2; printf '%s\n' "$db_cl" | sed 's/^/  db:   /' >&2; fail "content-scan closure != the unpacked DB walk — the DB's refs don't reflect the bytes"; }
echo "   [DURABLE] td content-scanned the seed closure (`printf '%s\n' "$scan_cl" | grep -c .` paths) from the BYTES == the unpacked DB walk — the DB's refs reflect the bytes, not just the manifest (no store DB, no guix)"

# [REMOVABLE oracle] the unpacked closure == guix's gc -R.
guix gc -R "$root" | sort -u > "$work/oracle"
extra=`cat "$work/reg" "$work/oracle" | sort | uniq -u | head -3`
test -z "$extra" || fail "unpacked closure differs from guix gc -R: $extra"
echo "   [REMOVABLE oracle] the unpacked DB closure == guix gc -R `basename "$root"`"

echo "PASS: td-builder seed-unpack restored a frozen seed tarball into a td-owned store +"
echo "      registered it (NAR-verified, no daemon, no /gnu/store write); td's own reader"
echo "      reads the complete closure back out — the seed is usable with no guix install"
echo "      (North-Star step 2 PR2; build-from-seed is next)."

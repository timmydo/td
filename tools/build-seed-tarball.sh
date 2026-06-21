#!/bin/sh
# tools/build-seed-tarball.sh OUTDIR ROOT...  — capture a SEED closure into a frozen,
# portable tarball + manifest (North-Star step 2, CLAUDE.md: serve the toolchain seed
# from a tarball, not a host guix). Run ONCE on a guix host — like a channel bump, NOT
# in the loop. For the GC closure of the ROOT store paths over the store DB's Refs
# graph, it writes:
#   OUTDIR/seed.tar       — a tar of every closure member's /gnu/store/<base> tree
#   OUTDIR/seed.manifest  — one line per member: `<store-path> <nar-hash>`
# The closure + NAR hashes come from td-builder itself (store-closure + nar-hash, td's
# own SQLite reader + NAR serializer) — the guix store is only the SOURCE the seed is
# captured FROM, once. `td-builder seed-unpack` (next increment) restores the tar into a
# td store + registers it from this manifest, with no daemon and no /gnu/store write, so
# the loop builds from the tarball with no guix install.
#
# Env: TB (td-builder binary; default: the stage0 from .td-build-cache/td-shell, or PATH),
# TD_SEED_DB (store DB; default /var/guix/db/db.sqlite).
set -eu

out="${1:?usage: build-seed-tarball.sh OUTDIR ROOT...}"; shift
[ "$#" -ge 1 ] || { echo "build-seed-tarball: need at least one ROOT store path" >&2; exit 2; }
db="${TD_SEED_DB:-/var/guix/db/db.sqlite}"
tb="${TB:-td-builder}"
test -x "$tb" || command -v "$tb" >/dev/null 2>&1 || { echo "build-seed-tarball: no td-builder ($tb)" >&2; exit 1; }
mkdir -p "$out"

# 1. closure of every ROOT over the Refs graph (td's own reader), unioned + sorted.
: > "$out/closure.txt"
for r in "$@"; do
  case "$r" in /gnu/store/*) : ;; *) echo "build-seed-tarball: ROOT '$r' is not a store path" >&2; exit 2 ;; esac
  "$tb" store-closure "$db" "$r" >> "$out/closure.txt" \
    || { echo "build-seed-tarball: store-closure failed for $r" >&2; exit 1; }
done
sort -u "$out/closure.txt" -o "$out/closure.txt"
n=`grep -c . "$out/closure.txt" || true`
test "$n" -ge 1 || { echo "build-seed-tarball: empty closure" >&2; exit 1; }

# 2. manifest: each member's NAR hash (td's own serializer).
: > "$out/seed.manifest"
while IFS= read -r p; do
  [ -n "$p" ] || continue
  h=`"$tb" nar-hash "$p"` || { echo "build-seed-tarball: nar-hash failed for $p" >&2; exit 1; }
  printf '%s %s\n' "$p" "$h" >> "$out/seed.manifest"
done < "$out/closure.txt"

# 3. tar every member's tree (absolute /gnu/store/<base> paths; extract with `-C DEST`).
tar cf "$out/seed.tar" --files-from="$out/closure.txt" \
  || { echo "build-seed-tarball: tar failed" >&2; exit 1; }

echo "build-seed-tarball: captured $n store paths -> $out/seed.tar (+ seed.manifest)" >&2
echo "$out/seed.tar"

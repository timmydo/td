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

for r in "$@"; do
  case "$r" in /gnu/store/*) : ;; *) echo "build-seed-tarball: ROOT '$r' is not a store path" >&2; exit 2 ;; esac
done

# 1+2. manifest: `<path> <nar-hash> <nar-size> <ref,…>` per closure member — td's own
#      store-closure + NAR serializer + Refs reader (no daemon). seed-unpack consumes it.
"$tb" seed-manifest "$db" "$@" > "$out/seed.manifest" \
  || { echo "build-seed-tarball: seed-manifest failed" >&2; exit 1; }
n=`grep -c . "$out/seed.manifest" || true`
test "$n" -ge 1 || { echo "build-seed-tarball: empty closure" >&2; exit 1; }
# The tar file-list is column 1 of the manifest (the closure members, sorted).
cut -d' ' -f1 "$out/seed.manifest" > "$out/closure.txt"

# 3. tar every member's tree (absolute /gnu/store/<base> paths; extract with `-C DEST`).
tar cf "$out/seed.tar" --files-from="$out/closure.txt" \
  || { echo "build-seed-tarball: tar failed" >&2; exit 1; }

echo "build-seed-tarball: captured $n store paths -> $out/seed.tar (+ seed.manifest)" >&2
echo "$out/seed.tar"

#!/bin/sh
# tools/warm-seed.sh OUT-BASE ROOT...  — ensure a frozen toolchain seed is CAPTURED +
# UNPACKED into a reusable, content-addressed cache, and print where it lives. North-Star
# step 2: the loop builds from ONE warmed seed instead of re-capturing 660M every run — the
# step toward serving the toolchain seed from a pinned tarball, not a host guix.
#
# Cache key = sha256 of the sorted ROOT set (cheap; the roots are pinned-channel store paths,
# so the key changes iff the seed changes). On a HIT (OUT-BASE/<key>/seed-ok present) it
# reuses the unpacked store + DB; on a MISS it captures (tools/build-seed-tarball.sh) and
# `td-builder seed-unpack`s once, then drops the big tar (keeps the unpacked store + DB +
# manifest). Prints: `<TD_SEED_STORE> <TD_SEED_DB> <MANIFEST>` for a build to consume.
#
# Env: TB (td-builder; default `td-builder`), TD_SEED_DB (the one-time capture SOURCE,
# passed to seed-manifest: a store DB FILE, or a store DIRECTORY to content-scan — the
# latter captures the seed with NO /var/guix/db read; default /var/guix/db/db.sqlite).
set -eu

base="${1:?usage: warm-seed.sh OUT-BASE ROOT...}"; shift
[ "$#" -ge 1 ] || { echo "warm-seed: need at least one ROOT store path" >&2; exit 2; }
tb="${TB:-td-builder}"
srcdb="${TD_SEED_DB:-/var/guix/db/db.sqlite}"

key=`printf '%s\n' "$@" | sort -u | sha256sum | cut -d' ' -f1`
cache="$base/$key"
if [ -f "$cache/seed-ok" ] && [ -d "$cache/store/gnu/store" ] && [ -s "$cache/seed.db" ]; then
  printf '%s %s %s\n' "$cache/store/gnu/store" "$cache/seed.db" "$cache/seed.manifest"
  exit 0
fi

chmod -R u+w "$cache" 2>/dev/null || true
rm -rf "$cache"; mkdir -p "$cache"
TB="$tb" TD_SEED_DB="$srcdb" sh tools/build-seed-tarball.sh "$cache/cap" "$@" >/dev/null \
  || { echo "warm-seed: capture failed" >&2; exit 1; }
"$tb" seed-unpack "$cache/cap/seed.tar" "$cache/cap/seed.manifest" "$cache/store" "$cache/seed.db" >/dev/null \
  || { echo "warm-seed: seed-unpack failed" >&2; exit 1; }
cp "$cache/cap/seed.manifest" "$cache/seed.manifest"
rm -f "$cache/cap/seed.tar"          # drop the big tar; the unpacked store IS the warm seed
: > "$cache/seed-ok"
printf '%s %s %s\n' "$cache/store/gnu/store" "$cache/seed.db" "$cache/seed.manifest"

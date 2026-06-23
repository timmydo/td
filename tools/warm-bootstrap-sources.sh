#!/bin/sh
# warm-bootstrap-sources.sh — host-side NETWORK PREP that warms the pinned source-bootstrap
# tarballs (GNU Mes, later tinycc/gcc/glibc/binutils) with td's OWN fetcher, td-fetch. The
# offline loop has no egress, so — exactly like tools/warm-tsgo.sh — this runs on the HOST
# (check.sh's prelude / the CI image build), NOT inside the sandbox. It is the ONE place the
# bootstrap sources are fetched; the heavy `bootstrap-*` gates then read the warmed tarball from
# .td-build-cache/sources/ (verifying the lock's sha256 themselves), with NO guix-as-fetcher.
#
# Each upstream source is a lock under seed/sources/*.lock (url / sha256 / file). To add a
# bootstrap stage (brick 3+), drop a lock there — no edit here, no further check.sh touch.
#
# BEST-EFFORT by design: the bootstrap-* gates are HEAVY (not in check-fast / the CI fast image),
# so a runner that cannot warm them (no cargo to build td-fetch, no network) is fine — this warns
# and continues, and the consuming gate fails loudly only if it actually runs without its source.
# (Contrast warm-tsgo, which FATALs because tsgo IS needed by the fast tier.)
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
srcdir="$root/seed/sources"
dest="$root/.td-build-cache/sources"
set -- "$srcdir"/*.lock
{ [ "$1" = "$srcdir/*.lock" ] && [ ! -e "$1" ]; } && exit 0   # no locks yet -> nothing to warm
mkdir -p "$dest"

sha_of() { sha256sum "$1" 2>/dev/null | cut -d' ' -f1; }

# Locate or build td-fetch once (reused across locks): prefer the rust-fetch gate's stage0 build,
# else a plain host cargo build of fetch/ (cached). Either is just a binary to drive the fetch.
tdf=$(ls "$root"/.td-build-cache/rust-fetch/b/newstore/*/bin/td-fetch 2>/dev/null | head -1 || true)
if { [ -z "$tdf" ] || [ ! -x "$tdf" ]; } && command -v cargo >/dev/null 2>&1; then
  ( cd "$root/fetch" && cargo build --release --quiet ) && tdf="$root/fetch/target/release/td-fetch" || tdf=""
fi

rc=0
for lock in "$srcdir"/*.lock; do
  url=$(sed -n 's/^url //p'    "$lock" | head -1)
  sha=$(sed -n 's/^sha256 //p' "$lock" | head -1)
  file=$(sed -n 's/^file //p'  "$lock" | head -1)
  if [ -z "$url" ] || [ -z "$sha" ] || [ -z "$file" ]; then
    echo ">> warm-bootstrap-sources: $lock malformed (need url/sha256/file) — skipping" >&2; rc=1; continue
  fi
  out="$dest/$file"
  if [ -f "$out" ] && [ "$(sha_of "$out")" = "$sha" ]; then continue; fi   # already warm + verified
  if [ -z "$tdf" ] || [ ! -x "$tdf" ]; then
    echo ">> warm-bootstrap-sources: $file is cold and no td-fetch (build fetch/ with cargo to warm it) — skipping (PREP best-effort)" >&2
    rc=1; continue
  fi
  echo ">> warm-bootstrap-sources: fetching $file with td-fetch (host PREP) ..." >&2
  if "$tdf" fetch "$url" "$sha" "$out.tmp" >&2 && [ "$(sha_of "$out.tmp")" = "$sha" ]; then
    mv -f "$out.tmp" "$out"
    echo ">> warm-bootstrap-sources: warmed $out (td-fetched, sha256 verified)" >&2
  else
    rm -f "$out.tmp"
    echo ">> warm-bootstrap-sources: could not td-fetch/verify $file — skipping (the bootstrap gate will report if it runs)" >&2
    rc=1
  fi
done
# PREP is best-effort: never fail check.sh here (the heavy bootstrap-* gates enforce presence).
exit 0

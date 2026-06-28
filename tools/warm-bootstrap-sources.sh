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

# Locate the fetcher (td-fetch) and the SHARED mirror (td-feed), reused across locks: prefer
# each gate's td-built binary, else a host cargo build (cached). Either is just a binary.
tdf=$(ls "$root"/.td-build-cache/rust-fetch/b/newstore/*/bin/td-fetch 2>/dev/null | head -1 || true)
if { [ -z "$tdf" ] || [ ! -x "$tdf" ]; } && command -v cargo >/dev/null 2>&1; then
  ( cd "$root/fetch" && cargo build --release --quiet ) && tdf="$root/fetch/target/release/td-fetch" || tdf=""
fi
tdfeed=$(ls "$root"/.td-build-cache/td-feed/sd/newstore/*/bin/td-feed 2>/dev/null | head -1 || true)
if { [ -z "$tdfeed" ] || [ ! -x "$tdfeed" ]; } && command -v cargo >/dev/null 2>&1; then
  ( cd "$root/feed" && cargo build --release --quiet ) && tdfeed="$root/feed/target/release/td-feed" || tdfeed=""
fi

# Start/reuse the ONE shared td-feed daemon and route fetches through it, so the bootstrap
# sources are SHARED across worktrees + served offline once warm (tools/feed-ensure.sh). The
# FIRST worktree egresses (td-feed warm populates the shared store); the rest read it offline
# over loopback. Best-effort: any failure falls back to a direct td-fetch below.
feedstore="${TD_FEED_DIR:-$HOME/.td/feed}/store"
faddr=""
if [ -n "$tdfeed" ] && [ -x "$tdfeed" ]; then
  faddr=$(TD_FEED_BIN="$tdfeed" sh "$root/tools/feed-ensure.sh" 2>/dev/null || true)
  [ -n "$faddr" ] && echo ">> warm-bootstrap-sources: using the shared feed at http://$faddr (store $feedstore)" >&2
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

  got=""
  # Preferred: through the SHARED feed. Populate it (td-feed warm egresses only if the shared
  # store is cold — another worktree may already hold it), then td-fetch FROM the feed
  # (TD_FEED_BASE, offline). So the egress happens ONCE across all worktrees.
  if [ -n "$faddr" ] && [ -n "$tdfeed" ]; then
    path=$(printf '%s' "$url" | sed -E 's,^https?://,,')
    idx=$(mktemp); printf '%s %s %s\n' "$path" "$url" "$sha" > "$idx"
    "$tdfeed" warm "$idx" "$feedstore" >&2 || true
    rm -f "$idx"
    if TD_FEED_BASE="http://$faddr" "$tdf" fetch "$url" "$sha" "$out.tmp" >&2 && [ "$(sha_of "$out.tmp")" = "$sha" ]; then
      mv -f "$out.tmp" "$out"; got="the shared feed (http://$faddr)"
    else
      rm -f "$out.tmp"
    fi
  fi
  # Fallback: a direct td-fetch (feed unavailable, or a cold-feed miss) — the prior behavior.
  if [ -z "$got" ]; then
    if "$tdf" fetch "$url" "$sha" "$out.tmp" >&2 && [ "$(sha_of "$out.tmp")" = "$sha" ]; then
      mv -f "$out.tmp" "$out"; got="a direct td-fetch"
    else
      rm -f "$out.tmp"
      echo ">> warm-bootstrap-sources: could not warm $file (feed + direct both failed) — skipping (the bootstrap gate will report if it runs)" >&2
      rc=1; continue
    fi
  fi
  echo ">> warm-bootstrap-sources: warmed $out via $got (sha256 verified)" >&2
done
# Derived input: the sanitized Linux UAPI headers for glibc-mesboot0, produced FROM the pinned linux
# source via `make headers_install` on the host (the sandbox can't run the kernel build). Best-effort.
sh "$root/tools/warm-kernel-headers.sh" || true
# ARCH=x86_64 sibling headers for the x86_64-toolchain cross-glibc rung (same pinned linux source).
sh "$root/tools/warm-kernel-headers-x86_64.sh" || true
# PREP is best-effort: never fail check.sh here (the heavy bootstrap-* gates enforce presence).
exit 0

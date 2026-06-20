#!/bin/sh
# warm-tsgo.sh — host-side NETWORK PREP that warms the pinned tsgo tarball with td's OWN
# fetcher (move-off-Guile §5 consumer-swap). The offline loop has no egress, so this runs
# on the HOST (check.sh's prelude / the CI image build), NOT inside the sandbox. It is the
# ONE place tsgo is fetched; the loop's gates then read the pin (tests/td-tsgo.lock) with
# NO `guix build -e '(@ (system td-ts) td-tsgo-tarball)'`.
#
# Flow: if the pinned store path is already warm -> no-op (the common case, near-instant).
# Else: build td-fetch from source with a plain host `cargo build` (this BREAKS the
# tsgo<->td-fetch bootstrap circularity — a host cargo build needs no tsgo, unlike the
# rust-fetch gate's ts-emit recipe path), `td-fetch fetch` the tarball + verify its
# sha256, then have the guix DAEMON add-to-store the VERIFIED bytes. add-to-store of a
# flat file with the origin's name + sha256 lands at the SAME content-addressed FOD path
# the guix origin produces (proven), so the pin is stable and the CI image carries the
# identical path. The FETCHER is td-fetch; the daemon is only the store (retired last).
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
lock="$root/tests/td-tsgo.lock"
test -f "$lock" || { echo "warm-tsgo: no pin $lock" >&2; exit 1; }

url=$(sed -n 's/^url //p' "$lock" | head -1)
sha=$(sed -n 's/^sha256 //p' "$lock" | head -1)
path=$(sed -n 's/^path //p' "$lock" | head -1)
test -n "$url" -a -n "$sha" -a -n "$path" || { echo "warm-tsgo: malformed pin (need url/sha256/path)" >&2; exit 1; }

# Already warm -> nothing to do (offline loop reads $path directly).
if [ -e "$path" ]; then exit 0; fi

echo ">> warm-tsgo: $path is cold — fetching with td-fetch (host PREP) ..." >&2

# Locate or build td-fetch. Prefer the rust-fetch gate's stage0 build if present; else a
# plain host cargo build (cached). Either is just a binary to drive the fetch — the
# hermetic stage0 build is proven by the rust-fetch gate, not needed here.
tdf=$(ls "$root"/.td-build-cache/rust-fetch/b/newstore/*/bin/td-fetch 2>/dev/null | head -1 || true)
if [ -z "$tdf" ] || [ ! -x "$tdf" ]; then
  echo ">> warm-tsgo: building td-fetch via host cargo (one-time) ..." >&2
  ( cd "$root/fetch" && cargo build --release --quiet )
  tdf="$root/fetch/target/release/td-fetch"
fi
test -x "$tdf" || { echo "warm-tsgo: no td-fetch binary" >&2; exit 1; }

tmp=$(mktemp -d "${TMPDIR:-/tmp}/warm-tsgo.XXXXXX")
trap 'rm -rf "$tmp"' EXIT
"$tdf" fetch "$url" "$sha" "$tmp/blob" >&2

# The guix DAEMON stores the td-fetched + verified bytes (it is the store, not the
# fetcher). name = the FOD basename minus its hash prefix; add-to-store with that name +
# flat sha256 reproduces the origin's FOD path.
name=$(basename "$path" | sed 's/^[a-z0-9]*-//')
added=$(guix repl -- /dev/stdin "$tmp/blob" "$name" <<'SCM'
(use-modules (guix store) (ice-9 match))
(match (command-line)
  ((_ file name)
   (with-store store
     (display (add-to-store store name #f "sha256" file))
     (newline))))
SCM
)
test "$added" = "$path" || { echo "warm-tsgo: add-to-store landed at $added, not the pin $path — bump tests/td-tsgo.lock" >&2; exit 1; }
echo ">> warm-tsgo: warmed $path (td-fetched, daemon-stored)" >&2

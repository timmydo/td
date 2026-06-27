#!/bin/sh
# warm-rust-upstream.sh — host-side NETWORK PREP that warms the pinned UPSTREAM Rust
# release tarball with td's OWN fetcher (rust-store-native track). The offline loop has no
# egress, so this runs on the HOST (check.sh's prelude / CI image build), NOT inside the
# sandbox. It is the ONE place the rust tarball is fetched; the gate then reads the pin
# (tests/rust-upstream.lock) — there is NO `guix build`/guix origin (the bytes are upstream
# Rust, not guix; the sha256 content-address is the oracle).
#
# Flow: if the pinned store path is already warm -> no-op. Else: build td-fetch from source
# with a plain host `cargo build` (no tsgo needed — a host cargo build breaks the
# tsgo<->td-fetch bootstrap circularity), `td-fetch fetch` the tarball + verify its sha256,
# then have the guix DAEMON add-to-store the VERIFIED bytes. add-to-store of a flat file
# with the origin name + sha256 lands at the SAME content-addressed path the lock pins
# (asserted), so the pin is stable and the CI image carries the identical path. The FETCHER
# is td-fetch; the daemon is only the store (retired last). NO guix-origin fallback — this
# is a guix-free seed by construction.
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
lock="$root/tests/rust-upstream.lock"
test -f "$lock" || { echo "warm-rust-upstream: no pin $lock" >&2; exit 1; }

url=$(sed -n 's/^url //p' "$lock" | head -1)
sha=$(sed -n 's/^sha256 //p' "$lock" | head -1)
path=$(sed -n 's/^path //p' "$lock" | head -1)
test -n "$url" -a -n "$sha" -a -n "$path" || { echo "warm-rust-upstream: malformed pin (need url/sha256/path)" >&2; exit 1; }

# Already warm -> nothing to do (the gate reads $path directly).
if [ -e "$path" ]; then exit 0; fi

echo ">> warm-rust-upstream: $path is cold — fetching with td-fetch (host PREP) ..." >&2

# Locate or build td-fetch (just a binary to drive the fetch; the hermetic stage0 build is
# proven by the rust-fetch gate, not needed here).
tdf=$(ls "$root"/.td-build-cache/rust-fetch/b/newstore/*/bin/td-fetch 2>/dev/null | head -1 || true)
if { [ -z "$tdf" ] || [ ! -x "$tdf" ]; } && command -v cargo >/dev/null 2>&1; then
  echo ">> warm-rust-upstream: building td-fetch via host cargo (one-time) ..." >&2
  ( cd "$root/fetch" && cargo build --release --quiet )
  tdf="$root/fetch/target/release/td-fetch"
fi
[ -n "$tdf" ] && [ -x "$tdf" ] || {
  echo "warm-rust-upstream: no td-fetch and no cargo to build it — cannot fetch the rust tarball guix-free" >&2
  echo "  (the rust-store-native gate needs this PREP; it is not run on the cargo-less CI fast tier)" >&2
  exit 1
}

# td OWNS the fetch: td-fetch fetches + verifies sha256, then the guix DAEMON stores the
# verified bytes (it is the store, not the fetcher). name = the FOD basename minus its hash
# prefix; add-to-store with that name + flat sha256 reproduces the pinned path.
tmp=$(mktemp -d "${TMPDIR:-/tmp}/warm-rust-upstream.XXXXXX")
trap 'rm -rf "$tmp"' EXIT
"$tdf" fetch "$url" "$sha" "$tmp/blob" >&2
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
test "$added" = "$path" || { echo "warm-rust-upstream: add-to-store landed at $added, not the pin $path — bump tests/rust-upstream.lock" >&2; exit 1; }
echo ">> warm-rust-upstream: warmed $path (td-fetched, daemon-stored, guix-free)" >&2

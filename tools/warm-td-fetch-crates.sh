#!/bin/sh
# warm-td-fetch-crates.sh — host-side NETWORK PREP that fetches td-fetch's crate closure
# GUIX-FREE: td's OWN fetcher (td-fetch) GETs each `.crate` from static.crates.io, pinned by
# the UPSTREAM `fetch/Cargo.lock` checksum (NOT a guix artifact), into a flat vendor dir that
# the `rust-fetch` gate interns (store-add-recursive) and builds td-fetch from
# (TD_VENDOR_DIR). NO guix-daemon, NO `guix build`, NO `/gnu/store` crate FOD — the crate path
# is guix-free; only the rust/gcc toolchain seed stays guix (retired last). td-fetch honors
# TD_FEED_BASE, so when the shared feed is up these reads route through it (shared offline).
#
# Like the other warm-* PREP, runs on the HOST (the offline loop has no egress), best-effort:
# the gate is HEAVY (not check-fast), so a runner that cannot warm (no cargo, no network) is
# fine — it warns and the gate reports if it actually runs cold.
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
lock="$root/fetch/Cargo.lock"
dest="$root/.td-build-cache/crate-vendor/td-fetch"
test -f "$lock" || { echo "warm-td-fetch-crates: no $lock" >&2; exit 0; }
mkdir -p "$dest"

# Locate or build td-fetch (the fetcher), reused across crates.
tdf=$(ls "$root"/.td-build-cache/rust-fetch/b/newstore/*/bin/td-fetch 2>/dev/null | head -1 || true)
if { [ -z "$tdf" ] || [ ! -x "$tdf" ]; } && command -v cargo >/dev/null 2>&1; then
  ( cd "$root/fetch" && cargo build --release --quiet ) && tdf="$root/fetch/target/release/td-fetch" || tdf=""
fi
[ -n "$tdf" ] && [ -x "$tdf" ] || { echo "warm-td-fetch-crates: no td-fetch binary — skipping (PREP best-effort)" >&2; exit 0; }

sha_of() { sha256sum "$1" 2>/dev/null | cut -d' ' -f1; }

# Parse fetch/Cargo.lock: emit `name version checksum` for every [[package]] that HAS a
# checksum (the vendored crates-io deps; the root td-fetch crate has none).
nbad=0
awk '
  /^\[\[package\]\]/ { name=""; ver=""; sum="" }
  /^name = / { v=$0; gsub(/.*= *"|".*/,"",v); name=v }
  /^version = / { v=$0; gsub(/.*= *"|".*/,"",v); ver=v }
  /^checksum = / { v=$0; gsub(/.*= *"|".*/,"",v); sum=v; if (name && ver && sum) print name, ver, sum }
' "$lock" | while read -r name ver sum; do
  nv="$name-$ver"
  out="$dest/$nv.crate"
  [ -f "$out" ] && [ "$(sha_of "$out")" = "$sum" ] && continue        # already warm + verified
  url="https://static.crates.io/crates/$name/$nv.crate"
  if "$tdf" fetch "$url" "$sum" "$out.tmp" >&2 && [ "$(sha_of "$out.tmp")" = "$sum" ]; then
    mv -f "$out.tmp" "$out"
  else
    rm -f "$out.tmp"; echo "warm-td-fetch-crates: could not td-fetch/verify $nv" >&2
  fi
done

n=$(ls "$dest"/*.crate 2>/dev/null | wc -l)
echo "warm-td-fetch-crates: $n crates in $dest (td-fetched, Cargo.lock-pinned, guix-free)" >&2

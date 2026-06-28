#!/bin/sh
# warm-cargo-proxy-local.sh — GUIX-FREE host PREP that provisions a LOCAL rust crate's dep
# closure THROUGH td's OWN cargo-proxy (`td-feed cargo-proxy`). The sibling
# tools/warm-cargo-proxy.sh handles a crates.io package (downloads its source crate first); this
# one handles an IN-TREE source dir (e.g. tests/russh-demo) whose source is NOT on crates.io — so
# there is no source crate to fetch, only the dep closure. cargo does the resolution + fetch
# through the proxy (td's verifying egress: each `.crate` sha256 == the crates.io sparse-index
# cksum, the UPSTREAM pin — no guix-daemon, no `guix build`, no `/gnu/store` crate FOD).
#
# Leaves, for the OFFLINE gate to intern (`store-add-recursive`) + build via TD_VENDOR_DIR:
#   .td-build-cache/crate-vendor/<dest>/*.crate   the locked dep closure
# (the SOURCE tree is the in-tree SRCDIR itself, interned by the gate at gate time — not copied
# here, so the repo source stays the single source of truth.)
#
# Usage: warm-cargo-proxy-local.sh SRCDIR DEST
#   SRCDIR  the in-tree crate dir (must ship a Cargo.lock), e.g. tests/russh-demo.
#   DEST    the cache subdir under .td-build-cache/crate-vendor/, e.g. russh.
# Best-effort like the other warm-* PREP: a runner without cargo/network warns and the gate
# reports if it actually runs cold (these gates are HEAVY, not check-fast). Per-worktree proxy
# store + an OS-picked loopback port, so concurrent agents/worktrees never collide.
set -eu

srcdir=${1:?usage: warm-cargo-proxy-local.sh SRCDIR DEST}
dest=${2:?usage: warm-cargo-proxy-local.sh SRCDIR DEST}
root=$(cd "$(dirname "$0")/.." && pwd)
srcdir=$(cd "$root" && cd "$srcdir" 2>/dev/null && pwd || echo "")
[ -n "$srcdir" ] && [ -f "$srcdir/Cargo.lock" ] || { echo "warm-cargo-proxy-local: $2 has no Cargo.lock at the source dir — cannot pin the closure" >&2; exit 0; }
vendor="$root/.td-build-cache/crate-vendor/$dest"

# Already warm? (a populated vendor dir.)
if [ "$(ls "$vendor"/*.crate 2>/dev/null | wc -l)" -ge 1 ]; then
  echo "warm-cargo-proxy-local: $dest already warm ($(ls "$vendor"/*.crate | wc -l) crates) in $vendor" >&2
  exit 0
fi

command -v cargo >/dev/null 2>&1 || { echo "warm-cargo-proxy-local: no cargo — skipping $dest (PREP best-effort)" >&2; exit 0; }

# Locate or build td-feed (the cargo-proxy binary).
feed=$(ls "$root"/.td-build-cache/td-feed/sd/newstore/*/bin/td-feed 2>/dev/null | head -1 || true)
if [ -z "$feed" ] || [ ! -x "$feed" ]; then
  ( cd "$root/feed" && cargo build --release --quiet ) && feed="$root/feed/target/release/td-feed" || feed=""
fi
[ -n "$feed" ] && [ -x "$feed" ] || { echo "warm-cargo-proxy-local: no td-feed binary — skipping $dest" >&2; exit 0; }

work="$root/.td-build-cache/crate-vendor/$dest.work"; rm -rf "$work"; mkdir -p "$work/proxy-store"
# Start the cargo-proxy on an OS-picked loopback port; it prints the bound addr.
"$feed" cargo-proxy "$work/proxy-store" 127.0.0.1:0 > "$work/proxy.log" 2>&1 &
ppid=$!
trap 'kill "$ppid" 2>/dev/null || true' EXIT INT TERM
i=0; while [ "$i" -lt 100 ]; do grep -q 'cargo-proxy on http' "$work/proxy.log" 2>/dev/null && break; sleep 0.1; i=$((i+1)); done
addr=$(sed -n 's#td-feed: cargo-proxy on http://\([^/]*\)/.*#\1#p' "$work/proxy.log")
[ -n "$addr" ] || { echo "warm-cargo-proxy-local: proxy did not bind for $dest:" >&2; cat "$work/proxy.log" >&2; exit 0; }

# A FRESH CARGO_HOME routed at the proxy (sparse source replacement). GOTCHA (#163): `cargo
# vendor` IGNORES source replacement; `cargo fetch`/`build` HONOR it. The fresh home forces EVERY
# crate to be a proxy miss → verified td egress, none served from a prior cargo cache (so the
# vendored closure stays complete + pinned).
ch="$work/cargo-home"; rm -rf "$ch"; mkdir -p "$ch"
cat > "$ch/config.toml" <<EOF
[source.crates-io]
replace-with = "td-proxy"
[source.td-proxy]
registry = "sparse+http://$addr/"
EOF

# Fetch the FULL locked closure through the proxy from the LOCAL source's Cargo.lock.
( cd "$srcdir" && CARGO_HOME="$ch" cargo fetch --locked >/dev/null 2>&1 ) || { echo "warm-cargo-proxy-local: locked dep fetch failed for $dest (in $srcdir)" >&2; exit 0; }

# Publish the vendor set (the proxy's verified crate cache).
rm -rf "$vendor"; mkdir -p "$vendor"
n=0
for c in "$work/proxy-store/crates"/*.crate; do
  [ -f "$c" ] || continue
  cp -p "$c" "$vendor/"
  n=$((n+1))
done

kill "$ppid" 2>/dev/null || true; trap - EXIT INT TERM
rm -rf "$work"
test "$n" -ge 1 || { echo "warm-cargo-proxy-local: no crates vendored for $dest" >&2; exit 0; }
echo "warm-cargo-proxy-local: $dest — $n crates provisioned guix-free (cargo-proxy, $srcdir/Cargo.lock-pinned, sha==index cksum) in $vendor" >&2

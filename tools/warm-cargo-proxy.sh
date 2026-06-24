#!/bin/sh
# warm-cargo-proxy.sh — GUIX-FREE host PREP that provisions a rust package's SOURCE tree + its
# FULL crate closure THROUGH td's OWN cargo-proxy (`td-feed cargo-proxy`). cargo does the
# resolution + fetch; the proxy is td's verifying egress — each `.crate`'s sha256 == the
# crates.io sparse-index cksum (the UPSTREAM pin, NOT a guix artifact). No guix-daemon, no
# `guix build`, no `/gnu/store` crate FOD. This is the GENERIC mechanism (the human's "build a
# proxy so cargo does the heavy lifting") for ANY rust package — no per-package Cargo.lock
# parsing here; cargo resolves from the package's own shipped Cargo.lock.
#
# Leaves, for the OFFLINE gate to intern (`store-add-recursive`) + build via TD_VENDOR_DIR:
#   .td-build-cache/crate-vendor/<name>/src/<name>-<version>/   the extracted source tree
#   .td-build-cache/crate-vendor/<name>/vendor/*.crate          the locked dep closure
#
# Usage: warm-cargo-proxy.sh NAME VERSION
# Best-effort like the other warm-* PREP: a runner without cargo/network warns and the gate
# reports if it actually runs cold (these gates are HEAVY, not check-fast). Per-worktree proxy
# store + an OS-picked loopback port, so concurrent agents/worktrees never collide.
set -eu

name=${1:?usage: warm-cargo-proxy.sh NAME VERSION}
ver=${2:?usage: warm-cargo-proxy.sh NAME VERSION}
root=$(cd "$(dirname "$0")/.." && pwd)
dest="$root/.td-build-cache/crate-vendor/$name"
srcparent="$dest/src"
srcdir="$srcparent/$name-$ver"
vendor="$dest/vendor"

# Already warm? (a source tree with Cargo.toml + a populated vendor dir.)
if [ -f "$srcdir/Cargo.toml" ] && [ "$(ls "$vendor"/*.crate 2>/dev/null | wc -l)" -ge 1 ]; then
  echo "warm-cargo-proxy: $name-$ver already warm ($(ls "$vendor"/*.crate | wc -l) crates) in $dest" >&2
  exit 0
fi

command -v cargo >/dev/null 2>&1 || { echo "warm-cargo-proxy: no cargo — skipping $name-$ver (PREP best-effort)" >&2; exit 0; }
command -v tar >/dev/null 2>&1 || { echo "warm-cargo-proxy: no tar — skipping $name-$ver" >&2; exit 0; }

# Locate or build td-feed (the cargo-proxy binary).
feed=$(ls "$root"/.td-build-cache/rust-feed/b/newstore/*/bin/td-feed 2>/dev/null | head -1 || true)
if [ -z "$feed" ] || [ ! -x "$feed" ]; then
  ( cd "$root/feed" && cargo build --release --quiet ) && feed="$root/feed/target/release/td-feed" || feed=""
fi
[ -n "$feed" ] && [ -x "$feed" ] || { echo "warm-cargo-proxy: no td-feed binary — skipping $name-$ver" >&2; exit 0; }

work="$dest/work"; rm -rf "$work"; mkdir -p "$work/proxy-store"
# Start the cargo-proxy on an OS-picked loopback port; it prints the bound addr.
"$feed" cargo-proxy "$work/proxy-store" 127.0.0.1:0 > "$work/proxy.log" 2>&1 &
ppid=$!
trap 'kill "$ppid" 2>/dev/null || true' EXIT INT TERM
i=0; while [ "$i" -lt 100 ]; do grep -q 'cargo-proxy on http' "$work/proxy.log" 2>/dev/null && break; sleep 0.1; i=$((i+1)); done
addr=$(sed -n 's#td-feed: cargo-proxy on http://\([^/]*\)/.*#\1#p' "$work/proxy.log")
[ -n "$addr" ] || { echo "warm-cargo-proxy: proxy did not bind for $name-$ver:" >&2; cat "$work/proxy.log" >&2; exit 0; }

# Write a CARGO_HOME config that routes crates.io through the proxy (sparse source replacement).
# GOTCHA (#163): `cargo vendor` IGNORES source replacement; `cargo fetch`/`build` HONOR it.
cargo_home() {
  rm -rf "$1"; mkdir -p "$1"
  cat > "$1/config.toml" <<EOF
[source.crates-io]
replace-with = "td-proxy"
[source.td-proxy]
registry = "sparse+http://$addr/"
EOF
}

# 1) Grab the SOURCE crate through the proxy via a throwaway project. Its fresh-resolution deps
#    are discarded (we want the package's OWN pinned closure, not a fresh resolve); only the
#    source `.crate` the proxy verified + cached is kept.
ch1="$work/ch-src"; cargo_home "$ch1"
proj="$work/srcfetch"; rm -rf "$proj"; mkdir -p "$proj/src"
cat > "$proj/Cargo.toml" <<EOF
[package]
name = "td-src-fetch"
version = "0.0.0"
edition = "2021"
[dependencies]
$name = "=$ver"
EOF
echo 'fn main(){}' > "$proj/src/main.rs"
( cd "$proj" && CARGO_HOME="$ch1" cargo fetch >/dev/null 2>&1 ) || { echo "warm-cargo-proxy: source fetch failed for $name=$ver" >&2; exit 0; }
crate="$work/proxy-store/crates/$name-$ver.crate"
[ -f "$crate" ] || { echo "warm-cargo-proxy: proxy did not cache the source crate $name-$ver" >&2; exit 0; }

# 2) Extract the source crate -> the source tree.
rm -rf "$srcparent"; mkdir -p "$srcparent"
tar -xzf "$crate" -C "$srcparent"
[ -f "$srcdir/Cargo.toml" ] || { echo "warm-cargo-proxy: extracted source has no Cargo.toml at $srcdir" >&2; exit 0; }
[ -f "$srcdir/Cargo.lock" ] || { echo "warm-cargo-proxy: source $name-$ver ships no Cargo.lock — cannot pin the closure" >&2; exit 0; }

# 3) Fetch the FULL locked closure through the proxy from the source's OWN Cargo.lock, with a
#    clean proxy cache + a FRESH cargo home (so EVERY crate is a proxy miss → verified td egress,
#    none served from a prior cargo cache — that's how the closure stays complete + pinned).
rm -rf "$work/proxy-store/crates" "$work/proxy-store/index"
ch2="$work/ch-deps"; cargo_home "$ch2"
( cd "$srcdir" && CARGO_HOME="$ch2" cargo fetch --locked >/dev/null 2>&1 ) || { echo "warm-cargo-proxy: locked dep fetch failed for $name-$ver" >&2; exit 0; }

# 4) Publish the vendor set (the proxy's verified crate cache) + drop cargo build state from the
#    source tree so the interned source stays minimal + reproducible.
rm -rf "$vendor"; mkdir -p "$vendor"
n=0
for c in "$work/proxy-store/crates"/*.crate; do
  [ -f "$c" ] || continue
  cp -p "$c" "$vendor/"
  n=$((n+1))
done
rm -rf "$srcdir/target"

kill "$ppid" 2>/dev/null || true; trap - EXIT INT TERM
test "$n" -ge 1 || { echo "warm-cargo-proxy: no crates vendored for $name-$ver" >&2; exit 0; }
echo "warm-cargo-proxy: $name-$ver — source + $n crates provisioned guix-free (cargo-proxy, Cargo.lock-pinned, sha==index cksum) in $dest" >&2

#!/bin/sh
# tools/provision-rust.sh — resolve a guix-free Rust toolchain (rustc + cargo) for the
# td-builder SEED build and print a PATH fragment (colon-joined bin dirs) putting both on
# PATH. This is the HEAD of DESIGN.md §Provenance (line 45: `rustup -> rust toolchain ->
# build td tools -> ...`): the one external input td's whole userland is bootstrapped from
# is a Rust toolchain, NOT guix. Human 2026-07-01: "we can expect the user to provide it,
# otherwise use rustup in the scripts to fetch."
#
# Resolution order (first hit wins):
#   1. TD_RUST_HOME   — an explicitly PROVIDED toolchain ($TD_RUST_HOME/bin/{rustc,cargo}).
#   2. the pinned lock — the guix-built seed paths in $TD_LOCK (retired LAST, DESIGN §5).
#                        Used ONLY if those store paths are actually present, so today's
#                        guix dev-loop stays byte-identical while a guix-LESS host (where
#                        the /gnu/store paths do not exist) falls through to (3).
#   3. rustup          — install + use the pinned toolchain ($TD_RUST_VERSION) on a
#                        guix-less host. Host-prep (network, like warm-tsgo); never guix.
# Exit 3 if none resolve. NEVER invokes guix/guile.
set -eu

lock="${TD_LOCK:-tests/td-builder-rust.lock}"
ver="${TD_RUST_VERSION:-1.96.0}"

has_pair() { [ -x "$1/rustc" ] && [ -x "$1/cargo" ]; }
# Print rustc-bin[:cargo-bin], de-duplicated when they share a directory.
emit() { if [ "$1" = "$2" ]; then printf '%s\n' "$1"; else printf '%s:%s\n' "$1" "$2"; fi; }

# 1. Explicitly provided toolchain.
if [ -n "${TD_RUST_HOME:-}" ]; then
  b="$TD_RUST_HOME/bin"
  has_pair "$b" || { echo "provision-rust: TD_RUST_HOME=$TD_RUST_HOME has no bin/rustc + bin/cargo" >&2; exit 3; }
  emit "$b" "$b"; exit 0
fi

# 2. The pinned (guix seed) lock — only when its store paths are present on disk.
if [ -s "$lock" ]; then
  r=$(grep -- '-rust-[0-9]' "$lock" | grep -v -- '-cargo' | sed 's/^[^ ]* //' | head -1 || true)
  c=$(grep -- '-rust-.*-cargo' "$lock" | sed 's/^[^ ]* //' | head -1 || true)
  if [ -n "$r" ] && [ -n "$c" ] && [ -x "$r/bin/rustc" ] && [ -x "$c/bin/cargo" ]; then
    emit "$r/bin" "$c/bin"; exit 0
  fi
fi

# 3. rustup — fetch the pinned toolchain (guix-less host).
if command -v rustup >/dev/null 2>&1; then
  rustup toolchain install "$ver" --profile minimal --no-self-update >&2 \
    || { echo "provision-rust: rustup could not install toolchain $ver" >&2; exit 3; }
  p=$(rustup which --toolchain "$ver" rustc) \
    || { echo "provision-rust: 'rustup which rustc' failed for $ver" >&2; exit 3; }
  d=$(dirname "$p")
  has_pair "$d" || { echo "provision-rust: rustup toolchain $ver at $d lacks rustc+cargo" >&2; exit 3; }
  emit "$d" "$d"; exit 0
fi

echo "provision-rust: no Rust toolchain found — set TD_RUST_HOME to a provided toolchain," >&2
echo "  ensure the pinned lock seed is present, or install rustup (DESIGN §Provenance)." >&2
exit 3

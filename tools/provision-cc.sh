#!/bin/sh
# tools/provision-cc.sh — resolve a C toolchain (gcc/cc + ld/ar + libc) for the td-builder
# SEED build's link step and print its bin-dir PATH fragment. Increment 2 of the guix-free
# daily bootstrap (github issue #268): tools/provision-rust.sh supplies rustc/cargo, but rustc
# still shells out to a C linker driver (`cc`/`gcc`) to produce the binary. td-builder is
# std-only with no build script, so the seed build needs ONLY a Rust toolchain + this C linker
# — no coreutils/bash. This is the SEED's link cc; the `/td/store` gcc built from `mes
# bootstrap` (DESIGN.md §Provenance) is a separate, later thing.
#
# Order (mirrors tools/provision-rust.sh; human 2026-07-01 "expect the user to provide it,
# otherwise use rustup in the scripts to fetch"):
#   1. TD_CC_HOME   — an explicitly PROVIDED toolchain ($TD_CC_HOME/bin with gcc or cc).
#   2. the pinned lock — the guix gcc-toolchain seed ($TD_LOCK, retired LAST DESIGN §5), used
#                        ONLY when present, so today's guix dev loop stays byte-identical while
#                        a guix-less host falls through to (3).
#   3. system       — the bin dir of `cc`/`gcc` on PATH (a guix-less host's build-essential).
# Exit 3 if none. NEVER invokes guix.
set -eu

lock="${TD_LOCK:-tests/td-builder-rust.lock}"
has_cc() { [ -x "$1/gcc" ] || [ -x "$1/cc" ]; }

# 1. Explicitly provided toolchain.
if [ -n "${TD_CC_HOME:-}" ]; then
  b="$TD_CC_HOME/bin"
  has_cc "$b" || { echo "provision-cc: TD_CC_HOME=$TD_CC_HOME has no bin/gcc or bin/cc" >&2; exit 3; }
  printf '%s\n' "$b"; exit 0
fi

# 2. The pinned (guix seed) gcc-toolchain — only when its store path is present on disk.
if [ -s "$lock" ]; then
  g=
  while IFS=' ' read -r _name _path _rest; do
    case "$_path" in
      */*-gcc-toolchain-*) [ -n "$g" ] || g="$_path" ;;
    esac
  done < "$lock"
  if [ -n "$g" ] && has_cc "$g/bin"; then printf '%s\n' "$g/bin"; exit 0; fi
fi

# 3. System cc/gcc (guix-less host: build-essential).
cc=$(command -v cc 2>/dev/null || command -v gcc 2>/dev/null || true)
if [ -n "$cc" ]; then
  d=$(dirname "$cc")
  has_cc "$d" || { echo "provision-cc: the system cc at $d is not usable" >&2; exit 3; }
  printf '%s\n' "$d"; exit 0
fi

echo "provision-cc: no C toolchain found — set TD_CC_HOME to a provided toolchain, ensure the" >&2
echo "  pinned lock seed is present, or install a system cc/gcc (build-essential)." >&2
exit 3

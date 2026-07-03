#!/bin/sh
# check.sh — the single pass/fail command for td (DESIGN.md §1.1) — now a pure
# BOOTSTRAP SHIM: build td's own entry binary with the host cargo and hand over.
#
# NO GUIX HERE (human direction 2026-07-03): the host Rust toolchain is the one
# thing the user brings to the table (the initial seed); everything after this
# `cargo build` — the pinned-guix integrity guard, the loop toolchain
# provisioning, the warm prelude, the sandbox, the gate ladder — is td's own
# compiled code (`td-builder check`, builder/src/check_loop.rs; the gates
# themselves are compiled Rust too, builder/src/gate_defs/*.rs).
#
# Usage:
#   ./check.sh                # full loop: cheap structural gates -> build-recipes -> heavy gates
#   ./check.sh eval           # a single gate or tier in the same sandbox
#   ./check.sh check-harness  # the guix-free /td/store harness tier
#
# The builder crate is dependency-free (pure std), so this build needs no
# network and no vendored crates — any stable host rustc works.
set -eu

cd "$(dirname "$0")"

cargo build --release --quiet --manifest-path builder/Cargo.toml
exec builder/target/release/td-builder check "$@"

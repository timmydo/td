//! provision-rust — Increment 1 of the guix-free daily bootstrap (DESIGN.md §Provenance
//! head `rustup -> rust toolchain -> build td tools`; human 2026-07-01; github issue #268).
//! The td-builder SEED Rust toolchain (rustc/cargo) is resolved by tools/provision-rust.sh —
//! a PROVIDED toolchain (TD_RUST_HOME) or rustup on a guix-less host, else the pinned guix
//! lock (retired LAST §5) — instead of being hard-wired to the guix store paths. This gate
//! proves the resolver picks provided/lock/rustup IN ORDER and that a PROVIDED toolchain
//! actually builds a WORKING td-builder (not just that a path resolves), with the guix lock
//! staying the fallback so today's dev loop is byte-unchanged. The C-linker leg (gcc from the
//! `mes bootstrap -> gcc toolchain` provenance arrow) is a later increment; here gcc/
//! coreutils/bash stay the pinned seed. The seed store paths are materialized with
//! `guix build <store-path>` (as gate 170 does — realizing a pinned path, NOT a
//! `-e '(@ (system M) PKG)'` packager site, so the guix-surface ratchet is unaffected).

use crate::gates::{GateDef, Pool, StoreMode};

pub fn gate() -> GateDef {
    GateDef {
        name: "provision-rust",
        pools: &[Pool::Heavy],
        needs: &[],
        build_gate: false,
        specs: &[],
        store: StoreMode::Shared,
        script: r##"
echo ">> provision-rust: the td-builder seed Rust toolchain is provided-or-rustup (guix-free), resolved in order and used to build a working td-builder; the pinned guix lock is the fallback (dev loop unchanged)"
set -eu; \
lock="$PWD/tests/td-builder-rust.lock"; \
test -s "$lock" || { echo "ERROR: no lock $lock" >&2; exit 1; }; \
grep ' /gnu/store/' "$lock" | sed 's/^[^ ]* //' | xargs $TD_GUIX build >/dev/null \
  || { echo "ERROR: could not realize the pinned toolchain seed (regenerate the lock on a channel bump)" >&2; exit 1; }; \
sh tests/provision-rust.sh
"##,
    }
}

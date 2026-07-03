//! provision-cc — Increment 2 of the guix-free daily bootstrap (github issue #268). After
//! provision-rust (gate 172) supplies rustc/cargo, rustc still shells out to a C linker driver
//! to produce the binary; tools/provision-cc.sh resolves it — a PROVIDED toolchain (TD_CC_HOME)
//! or the system cc on a guix-less host, else the pinned guix gcc-toolchain (retired LAST §5) —
//! instead of bootstrap-td-builder.sh hard-wiring the guix store path. This gate proves the
//! resolver picks provided/lock/system IN ORDER and that a PROVIDED C toolchain (with a provided
//! Rust toolchain) actually builds a WORKING td-builder, with the guix gcc-toolchain staying the
//! fallback so today's dev loop is byte-unchanged. td-builder is std-only with no build script,
//! so the seed build needs ONLY rust + this cc (the removed coreutils/bash were never used).
//! Together with gate 172 the stage0 td-builder builds with NO guix. Seed store paths are
//! materialized with `guix build <store-path>` (as gate 170 does — a path realize, NOT a
//! `-e '(@ (system M) PKG)'` packager site, so the guix-surface ratchet is unaffected).

use crate::gates::{GateDef, Pool};

pub fn gate() -> GateDef {
    GateDef {
        name: "provision-cc",
        pools: &[Pool::Heavy],
        needs: &[],
        build_gate: false,
        specs: &[],
        script: r##"
echo ">> provision-cc: the td-builder seed C toolchain is provided-or-system (guix-free), resolved in order and used (with a provided Rust toolchain) to build a working td-builder; the guix gcc-toolchain is the fallback (dev loop unchanged)"
set -eu; \
lock="$PWD/tests/td-builder-rust.lock"; \
test -s "$lock" || { echo "ERROR: no lock $lock" >&2; exit 1; }; \
grep ' /gnu/store/' "$lock" | sed 's/^[^ ]* //' | xargs $TD_GUIX build >/dev/null \
  || { echo "ERROR: could not realize the pinned toolchain seed (regenerate the lock on a channel bump)" >&2; exit 1; }; \
sh tests/provision-cc.sh
"##,
    }
}

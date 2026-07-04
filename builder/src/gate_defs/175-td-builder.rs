//! td-builder S1 — the build tool builds ITSELF, guix-free, reproducibly (DESIGN §7.1).
//! R2 (#275) restructured S1: the subject td-builder is built by the cargo-bootstrapped
//! stage0 (NOT `guix build -e '(@ (system td-builder) td-builder)'`), and its repro leg
//! is a stage0 DOUBLE-bootstrap — td's own oracle, not `guix build --check`.
//! 
//! The old S2/S3/S4 legs (NAR differential vs the daemon's recorded hashes, userns build
//! differential vs the daemon, and the qcow2 SYSTEM-IMAGE differential) were RETIRED with
//! the guix-system museum tier (human direction, directive 3): their distinct
//! purpose was the guix/daemon byte-identity comparison, and the td-side capabilities
//! they exercised are covered by the live td-native gates — NAR hashing backs every
//! store registration/verify (store-add/register/verify/backend, 275–310), the userns
//! sandboxed build IS the corpus build path (build-hermetic 356, daemon-recipe 359,
//! td-drv-build 235), and the qcow2 guix image is retired outright.
//! 
//! S1 legs: build td-builder from source with the cargo-bootstrapped stage0
//! (tools/bootstrap-td-builder.sh — guix/Guile off PATH), RUN the binary and assert its
//! sentinel (the toolchain produced a WORKING executable — stronger than "cargo build
//! exited 0"), and prove reproducibility by a stage0 DOUBLE-bootstrap: two independent
//! builds are bit-identical (prime directive 1).
//! OFFLINE PRECONDITION (DESIGN §5): the pinned Rust closure must be warm in the host
//! store — S1 realises the stage0 toolchain seed (tests/td-builder-rust.lock) up front
//! (seed realize, retired last).

use crate::gates::{GateDef, Pool, StoreMode};

pub fn gate() -> GateDef {
    GateDef {
        name: "td-builder",
        pools: &[Pool::Heavy],
        needs: &[],
        build_gate: false,
        specs: &[],
        inputs: &[],
        store: StoreMode::Shared,
        non_blocking: false,
        script: r##"
echo ">> td-builder: reproducible offline self-build (S1 — stage0 double-bootstrap, guix/Guile off PATH)"
set -euo pipefail; \
scratch0="$PWD/.td-build-cache/td-builder-stage0"; rm -rf "$scratch0"; mkdir -p "$scratch0"; \
lock0="$PWD/tests/td-builder-rust.lock"; \
test -s "$lock0" || { echo "ERROR: no lock $lock0" >&2; exit 1; }; \
grep ' /gnu/store/' "$lock0" | sed 's/^[^ ]* //' | xargs $TD_GUIX build >/dev/null || { echo "ERROR: could not realize the stage0 toolchain seed (regenerate tests/td-builder-rust.lock on a channel bump)" >&2; exit 1; }; \
echo ">> S1: build the td-builder SUBJECT from source with the cargo-bootstrapped stage0 (guix/Guile off PATH — the guix-as-packager build retired in R2, #275)"; \
s0=`TD_LOCK="$lock0" sh tools/bootstrap-td-builder.sh "$scratch0/a"`; \
test -x "$s0" || { echo "FAIL: bootstrap produced no stage0 td-builder" >&2; exit 1; }; \
out=${s0%/bin/td-builder}; \
echo ">> run: the compiled binary must print its sentinel"; \
"$out/bin/td-builder" | grep -Eq '^td-builder [0-9.]+ ok$' \
  || { echo "FAIL: the compiled td-builder did not print its sentinel (or exited nonzero) — the toolchain did not produce a working binary." >&2; exit 1; }; \
echo ">> check: reproducibility of the td-builder binary (stage0 double-bootstrap — td's own oracle, not guix build --check)"; \
s0b=`TD_LOCK="$lock0" sh tools/bootstrap-td-builder.sh "$scratch0/b"`; \
ha=`sha256sum "$s0" | cut -d' ' -f1`; hb=`sha256sum "$s0b" | cut -d' ' -f1`; \
test "$ha" = "$hb" || { echo "FAIL: the two stage0 builds differ ($ha != $hb) — the td-builder build is NOT reproducible" >&2; exit 1; }; \
echo "   reproducible: two independent bootstraps are bit-identical (sha256 $ha)"; \
rm -rf "$scratch0"; \
echo "PASS: td-builder builds ITSELF from source on the cargo-bootstrapped stage0 (guix/Guile off PATH), the binary runs (sentinel), and two independent bootstraps are bit-identical (td's own reproducibility oracle)."
"##,
    }
}

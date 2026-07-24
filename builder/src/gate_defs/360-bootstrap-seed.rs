//! bootstrap-seed — source-bootstrap BRICK 0 (the north star: no guix BYTES). td's /td/store
//! toolchain is built up from a tiny, hand-auditable, NON-guix seed — stage0-posix's 229-byte
//! hex0-seed + 618-byte kaem-optional-seed (inside the pinned upstream stage0-posix source).
//! This gate runs the seed kaem build with guix/Guile SCRUBBED from env, producing the first
//! stage0 artifacts (a full hex0 + kaem-0) — no guix process, no /gnu/store in the build.
//! 
//! The driver is a STRUCTURED Rust recipe — `td-builder bootstrap-recipe seed`
//! (builder/src/bootstrap.rs, rust-migration C2): the old shell tests/bootstrap-seed.sh was
//! ported to typed Rust data + the shared leg runner and DELETED — no shell oracle kept (this is
//! all-durable; there is no guix oracle). The Rust recipe asserts a SUPERSET of the shell's legs
//! and was proven to produce the same pinned bytes. The pure-logic legs run as cargo unit tests;
//! the repo-rooted cargo-test preflight builds brick 0 end-to-end on every PR.
//! ALL-DURABLE (the seed is the irreducible bottom; there is no guix oracle):
//! [pinned-input] the source tarball and contained binary seeds match their pinned sha256
//! (auditable, not guix-built);
//! [no-guix] the build runs env-cleared; no /gnu/store byte in the artifacts;
//! [self-reproduction] the seed assembles its OWN hex source to a byte-identical seed (so the
//! binary seeds are verifiable from the human-readable hex, not blind trust);
//! [behavioral] the seed-built hex0 actually works as an assembler (reproduces kaem-0);
//! [repro] two independent runs are byte-identical.
//! Standalone + tiny (two ~hundred-byte assemblers, sub-second after the stage0 td-builder build)
//! — NOT a BUILD_GATE. Later bricks drive kaem-0 over the rest of the chain (mes→tinycc→gcc→glibc).

use crate::gates::{GateDef, Pool};

pub fn gate() -> GateDef {
    GateDef {
        name: "bootstrap-seed",
        pools: &[Pool::Heavy],
        needs: &[],
        build_gate: false,
        specs: &[],
        non_blocking: false,
        script: r##"
echo ">> bootstrap-seed: the structured Rust seed recipe builds the first stage0 artifacts with guix off env — self-reproducing, working, reproducible (source-bootstrap brick 0)"
set -euo pipefail; \
. tests/cache-lib.sh; export TD_STAGE0_BASE="$PWD/.td-build-cache/stage0"; load_stage0; tb="$TB"; \
case "$tb" in *.td-build-cache/stage0/*) : ;; *) echo "FAIL: td-builder is not the bootstrapped stage0 ($tb)" >&2; exit 1 ;; esac; \
test -x "$tb" || { echo "ERROR: could not build td-builder" >&2; exit 1; }; \
TD_RECIPE_EVAL=`sh tests/recipe-eval-tool.sh "$PWD/.td-build-cache/recipe-eval"`; export TD_RECIPE_EVAL; \
"$tb" bootstrap-recipe seed
"##,
    }
}

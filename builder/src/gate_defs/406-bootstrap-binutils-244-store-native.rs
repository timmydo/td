//! bootstrap-binutils-244-store-native — source-bootstrap BRICK 6/7 (the FINAL modern toolchain, rung A): the
//! FIRST modern toolchain component built from source by the dynamic /td/store toolchain. From the 229-byte
//! seed, td builds the chain → gcc-mesboot1 (4.6.4) + binutils-mesboot → a SHARED glibc 2.16.0, interns them
//! content-addressed into /td/store, then builds MODERN GNU Binutils 2.44 from source (unmodified ./configure
//! && make) with that toolchain. The as/ld/ar are DYNAMIC ELFs (interp + RUNPATH = the /td/store glibc),
//! interned at /td/store, and RUN in the store-ns own-root (/gnu/store ABSENT): they report version 2.44 AND
//! assemble+link a C program (gcc-mesboot1 -B at the new binutils) that returns 42. This is the binutils-boot0
//! of guix's final-toolchain ladder, td-native (gcc-boot0 next). Build via two build-wrappers (CC bakes
//! /td/store interp for the target as/ld; CC_FOR_BUILD bakes the live build-dir interp for in-tree build tools
//! like chew), -std=gnu99 (binutils 2.44 is C99+, gcc 4.6.4 default is gnu89), cross-style, --disable-gold.
//! BYTE-REPRODUCIBLE: two independent from-source builds, canonicalized by tests/repro-lib.sh (strip the
//! build-path-bearing DWARF + deterministic archives + drop libtool .la), land on the SAME content-addressed
//! /td/store path — a stable key for td-subst chaining/fetch. DURABLE: pinned-input, no-guix (no /gnu/store in
//! libc.so.6 NOR ld), content-addr, repro (intrinsic double-build, no guix oracle), behavioral (modern as/ld
//! 2.44 run+link → 42 from /td/store), structural (/td/store is the store, /gnu/store ABSENT). NOT a BUILD_GATE.

use crate::gates::{ArtifactInput, GateDef, InputKind, Pool, StoreMode};

pub fn gate() -> GateDef {
    GateDef {
        name: "bootstrap-binutils-244-store-native",
        pools: &[Pool::Daily],
        needs: &[],
        build_gate: false,
        specs: &[],
        // Typed artifact input (#353): the runnable static-bash fixture from the
        // pinned closure — resolved by the runner; the body's grep +
        // store-closure-scan hand-wiring is deleted.
        inputs: &[ArtifactInput {
            name: "bash-static",
            kind: InputKind::ClosureMember {
                lock: "tests/hello-no-guix.lock",
                root_stem: "bash",
                member_stem: "bash-static",
            },
        }],
        store: StoreMode::Shared,
        non_blocking: true,
        script: r##"
echo ">> bootstrap-binutils-244-store-native: the /td/store toolchain builds MODERN binutils 2.44 from source; as/ld run from /td/store, report 2.44, and link a program → 42, /gnu/store ABSENT (source-bootstrap brick 6/7, final-toolchain rung A)"
sh tests/bootstrap-binutils-244-store-native.sh
"##,
    }
}

//! bootstrap-x86_64-toolchain-store-native — source-bootstrap: CROSS the i686 full-source bootstrap UP to a
//! native x86_64 toolchain at /td/store (x86_64-toolchain track). The whole existing /td/store toolchain (gcc
//! 14.3.0 + binutils 2.44 + glibc 2.41) is i686/32-bit (ld-linux.so.2), but the upstream Rust pin is x86_64, so
//! Rust runtime coverage is blocked on ARCHITECTURE — not just glibc>=2.17. From the 229-byte
//! seed, td builds the i686 chain → gcc 14.3.0, then with it CROSSES UP (LFS/crosstool shape): cross binutils
//! 2.44 (--target=x86_64) → cross gcc 14 stage1 (C, no libc) → MODERN x86_64 glibc 2.41 (ld-linux-x86-64.so.2 +
//! libc.so.6) → cross gcc 14 stage2 (c,c++ --enable-shared → libgcc_s.so.1, which rustc needs). The x86_64 glibc
//! 2.41 is interned content-addressed at /td/store, and the cross gcc links a DYNAMIC x86_64 C AND C++ program
//! against it (interp = /td/store x86_64 ld-linux-x86-64.so.2) that runs in the own-root → 42, /gnu/store ABSENT.
//! DURABLE: pinned-input, no-guix (no /gnu/store in the x86_64 libc.so.6 NOR the cross gcc/cc1), content-addr,
//! behavioral (an ELF 64-bit C + C++ program runs vs the x86_64 glibc 2.41 from /td/store → 42), structural,
//! input-addressed (x64-toolchain-subst: the x86_64 glibc is ALSO interned at the LOCK-KEYED path from
//! tests/td-toolchain-x86_64.lock — the stable path a consumer fetches as a signed substitute, not a
//! content-addressed throwaway — and a program whose interp IS that path runs in the own-root → 42).
//! The package behavior assertion is owned by the `gcc-x86-64-stage2-test` recipe check; this gate is
//! only the daily scheduling boundary for that check. NOT a BUILD_GATE.

use crate::gates::{ArtifactInput, GateDef, InputKind, Pool};

pub fn gate() -> GateDef {
    GateDef {
        name: "bootstrap-x86_64-toolchain-store-native",
        pools: &[Pool::Daily],
        needs: &[],
        build_gate: false,
        specs: &[],
        inputs: &[ArtifactInput {
            name: "bash-static",
            kind: InputKind::ClosureMember {
                lock: "tests/td-subst.lock",
                root_stem: "bash",
                member_stem: "bash-static",
            },
        }],
        non_blocking: true,
        script: r##"
echo ">> recipe-check gcc-x86-64-stage2-test: build the x86_64 cross toolchain recipe graph and assert its output"
: "${TD_RECIPE_EVAL:=}"
if [ -z "$TD_RECIPE_EVAL" ] || [ ! -x "$TD_RECIPE_EVAL" ]; then
  TD_RECIPE_EVAL=$(sh tests/recipe-eval-tool.sh "$PWD/.td-build-cache/recipe-eval")
fi
exec "$TD_RECIPE_EVAL" check-run gcc-x86-64-stage2-test daily 1
"##,
    }
}

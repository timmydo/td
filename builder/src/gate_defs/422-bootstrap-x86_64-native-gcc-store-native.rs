//! bootstrap-x86_64-native-gcc-store-native — source-bootstrap: rung X2 of the x86_64-toolchain track,
//! after the #201 CROSS rungs. X1 produced a CROSS x86_64 gcc — itself an i686 (ELF 32-bit) binary that
//! EMITS x86_64. X2 turns that into a NATIVE x86_64 gcc: with the cross toolchain (fetched as the
//! lock-keyed signed closure, or built from the 229-byte seed) td builds NATIVE x86_64 binutils 2.44 +
//! NATIVE x86_64 GCC 14.3.0 — gcc/cc1/g++ that are themselves ELF 64-bit x86_64 (--build=--host=--target
//! =x86_64), STATIC vs the /td/store x86_64 glibc 2.41. The native gcc is interned at /td/store and RUN
//! in the store-ns own-root, where it COMPILES a C and a C++ program from source and both run -> 42,
//! /gnu/store ABSENT — the compiler doing the work is itself an x86_64 binary living in td's own store
//! (the architectural self-hosting rung; a from-source gcc-rebuilds-gcc bootstrap is not claimed here).
//! DURABLE: pinned-input, native-arch (the gcc/cc1/as/ld ARE ELF64 x86_64, not the i686 cross gcc),
//! no-guix (no /gnu/store in the native gcc/cc1/as/ld or the x86_64 libc.so.6), content-addr, behavioral
//! (the native gcc RUNS and compiles+links a C AND C++ program -> 42), structural (own-root /td/store, no
//! /gnu/store). The NATIVE gcc is ALWAYS BUILT (never fetched); only its cross-toolchain prerequisite may
//! be fetched. HEAVY (the native gcc build is ~45 min; from-seed adds the ~98-min cross build). NOT a
//! BUILD_GATE. The output assertions live with the `gcc-x86-64-native-test` recipe check; the native
//! binutils/gcc BUILD is the recipe ladder `binutils-x86-64-native` -> `gcc-x86-64-native`
//! (recipes/src/recipes/, driven by td-recipe-eval check-run) — the retirement of the
//! former `td-builder toolchain-recipe x86_64-native` imperative Rust path.

use crate::gates::{ArtifactInput, GateDef, InputKind, Pool};

pub fn gate() -> GateDef {
    GateDef {
        name: "bootstrap-x86_64-native-gcc-store-native",
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
        non_blocking: false,
        script: r##"
echo ">> recipe-check gcc-x86-64-native-test: build the native x86_64 gcc recipe graph and assert its output"
: "${TD_RECIPE_EVAL:=}"
if [ -z "$TD_RECIPE_EVAL" ] || [ ! -x "$TD_RECIPE_EVAL" ]; then
  TD_RECIPE_EVAL=$(sh tests/recipe-eval-tool.sh "$PWD/.td-build-cache/recipe-eval")
fi
exec "$TD_RECIPE_EVAL" check-run gcc-x86-64-native-test daily 1
"##,
    }
}

//! hello-zero-gnu-store — issue #388: the FIRST zero-/gnu/store `td-builder build-recipe`. GNU hello 2.12.2
//! is built by build-recipe from a lock whose EVERY entry is /td/store — the gcc-toolchain, the build
//! userland (busybox 1.37.0 sh+coreutils+sed+grep+awk+tar+gzip applets + GNU make 4.4.1, td-built at
//! /td/store from the from-seed x86_64 toolchain), AND the source (td-fetched, interned at /td/store). The
//! corpus gates substitute ONLY the gcc-toolchain and warm every OTHER build tool from /gnu/store — the
//! "guix-seeded corpus template" AGENTS.md declares CLOSED. This is the north-star re-aim's first payoff:
//! the build ENV carries no guix bytes. The build userland reuses gate 420's from-seed /td/store x86_64
//! toolchain + busybox/make build (as function libraries); this gate adds the missing half — FEEDING that
//! userland to build-recipe as its build tools (bash→sh via the engine's find_build_shell fallback,
//! tar/make/gcc from /td/store). DURABLE legs (no guix oracle): [supply-chain] busybox/make/hello tarballs
//! match their seed/sources pins; [zero-gnu-lock] the composed lock has grep -c '/gnu/store' == 0 (the #388
//! assertion); [no-guix-env] the build sandbox stages no /gnu/store path; [provenance] the built hello has
//! zero /gnu/store bytes and interp = the /td/store glibc 2.41; [behavioral] hello RUNS in the store-ns
//! own-root → "Hello, world!", /gnu/store ABSENT; [verified-red] dropping the userland from the lock reds
//! the build. HEAVY (~90 min from seed, ~15 with warm-subst; directive 1 — no cache on the from-seed leg).
//! non_blocking (the #356/#371 family: a dev box without an exposed subst store builds the toolchain from
//! seed and can memory-kill — tolerated). NOT a BUILD_GATE (it drives its own recipe-eval prelude).

use crate::gates::{ArtifactInput, GateDef, InputKind, Pool, StoreMode};

pub fn gate() -> GateDef {
    GateDef {
        name: "hello-zero-gnu-store",
        pools: &[Pool::Daily],
        needs: &[],
        build_gate: false,
        specs: &[],
        // The static-bash fixture (#353): x86_64_verify_closure links+runs a C
        // program in an own-root against it — the runner resolves it.
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
echo ">> hello-zero-gnu-store: GNU hello built by build-recipe from a lock with ZERO /gnu/store entries — the /td/store busybox+make userland IS the build env (the first build-recipe with no guix bytes in the loop, #388)"
sh tests/hello-zero-gnu-store.sh
"##,
    }
}

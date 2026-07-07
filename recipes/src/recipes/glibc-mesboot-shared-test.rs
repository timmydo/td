use crate::ladder::{base_inputs, base_path, link_bins, SH};
use crate::types::{Recipe, RecipeCheck, Step};

// glibc-mesboot-shared-test — the behavioral validation of the source-bootstrap chain's
// BRICK 6 dynamic toolchain (gcc-mesboot1 + binutils-mesboot + glibc-mesboot-shared),
// modeled as a RECIPE rather than a bespoke tests/ check script (cf. make-test.rs, #388
// rung 1; #397/#327 — replaces tests/bootstrap-glibc-shared-store-native.sh): it DEPENDS
// on the three rungs and, in its steps, LINKS a real C program DYNAMICALLY against the
// shared glibc and RUNS it. If any of the three is broken, a step exits nonzero -> this
// recipe's build fails -> the recipe check reds. So the feature (the first dynamic
// /td/store-native toolchain, where td-store paths get baked into a running binary) is
// exercised through the toolchain's real entry point (gcc, ld, the produced binary), with
// no shell scaffolding under tests/ beyond the generic build-plan --auto drive.
//
// [no-guix]: a byte-grep of the shared libc.so.6 — the same proof make-test.rs uses for
// its static binary, generalized to a dynamic library (a dynamic lib can't hide a
// /gnu/store reference any better than a static one). [behavioral]: the linked program's
// ELF interpreter is glibc-mesboot-shared's OWN loader (readelf-verified, not the build
// host's), and running it returns EXACTLY 42 (not just "exits zero") — the same
// non-vacuous exit-code assertion the retired shell script made via `RC=$?`.
//
// This recipe's OWN native_inputs graph (binutils-mesboot0/1 -> gcc-mesboot0/1 ->
// glibc-mesboot0 -> glibc-mesboot-shared) is a small early prefix of the full chain — but
// its RecipeCheck::daily body (below) drives through the SHARED bootstrap_modern_toolchain
// warm-chain function (tests/bootstrap-chain.sh), the same driver every other daily
// recipe-check shares, which always realizes the full 20-rung graph through glibc-241 (not
// just this recipe's own prefix). That's intentional, not wasteful in the steady state:
// the machine-wide chain-brick cache (#317) means whichever daily check runs first pays
// the full-chain cost once and every other daily check (hello, sed, this one, …) cache-hits
// it — a genuinely-cold run (e.g. a fresh worktree with no warm cache) pays for the whole
// chain regardless of which single recipe you're validating.
pub fn recipe() -> Recipe {
    let glibc = "{in:glibc-mesboot-shared}";
    let gcc = "{in:gcc-mesboot1}/bin/gcc";
    let readelf = "{in:binutils-mesboot}/bin/readelf";
    let mut steps = Vec::new();

    // [no-guix] the shared libc carries zero /gnu/store bytes. PATH must be set: steps run
    // env-cleared, and an unresolvable bare `grep` inside a bodyless `if` exits 0 by POSIX
    // rule (no branch taken -> exit status 0) — that would make this vacuously pass instead
    // of failing loudly.
    steps.push(
        Step::run(
            "{root}",
            &[
                SH,
                "-c",
                &format!(
                    "if grep -q -a /gnu/store '{glibc}/lib/libc.so.6'; then echo 'glibc-mesboot-shared embeds /gnu/store bytes' >&2; exit 1; fi"
                ),
            ],
        )
        .env("PATH", &base_path()),
    );

    // as/ld/ar/readelf onto {tools} (base_path()'s first component) so gcc's internal
    // assembler/linker invocations — and our own readelf check below — resolve to
    // binutils-mesboot, not some host toolchain.
    steps.push(link_bins("binutils-mesboot"));

    steps.push(Step::WriteFile {
        path: "{root}/hello.c".into(),
        content: "#include <stdio.h>\nint main(){printf(\"dyn-td-store\\n\");return 42;}\n".into(),
        exec: false,
    });

    // [behavioral] gcc-mesboot1 links a DYNAMIC program against the shared glibc: an
    // explicit --dynamic-linker/-rpath naming glibc-mesboot-shared's OWN loader (not the
    // build host's), matching what a real configure/make link step produces.
    steps.push(
        Step::run(
            "{root}",
            &[
                gcc,
                &format!("-Wl,--dynamic-linker={glibc}/lib/ld-linux.so.2"),
                &format!("-Wl,-rpath={glibc}/lib"),
                &format!("-B{glibc}/lib"),
                "-o",
                "d",
                "hello.c",
            ],
        )
        .env("PATH", &base_path())
        .env("C_INCLUDE_PATH", &format!("{glibc}/include"))
        .env(
            "LIBRARY_PATH",
            &format!("{glibc}/lib:{{in:gcc-mesboot1}}/lib/gcc/i686-unknown-linux-gnu/4.6.4"),
        ),
    );

    // [structural] the linked binary's interpreter is glibc-mesboot-shared's own loader.
    steps.push(
        Step::run(
            "{root}",
            &[
                SH,
                "-c",
                &format!(
                    "{readelf} -l d | grep -q '{glibc}/lib/ld-linux.so.2' || {{ echo 'wrong ELF interpreter (not glibc-mesboot-shared)' >&2; exit 1; }}"
                ),
            ],
        )
        .env("PATH", &base_path()),
    );

    // [behavioral] RUN it — the EXACT exit code the source program returns (42), not
    // just "exited zero" (a crashed/never-linked binary would also fail earlier steps,
    // but this is the same non-vacuous check the retired shell script made via `RC=$?`).
    steps.push(Step::run(
        "{root}",
        &[
            SH,
            "-c",
            "./d; rc=$?; test \"$rc\" -eq 42 || { echo \"dynamic program returned $rc, not 42\" >&2; exit 1; }",
        ],
    ));

    steps.push(
        Step::run(
            "{root}",
            &[
                SH,
                "-c",
                "mkdir -p '{out}'; printf 'PASS: gcc-mesboot1 linked a DYNAMIC program against glibc-mesboot-shared; it ran, interp=glibc-mesboot-shared, returned 42\\n' > '{out}/result'",
            ],
        )
        .env("PATH", &base_path()),
    );
    steps.push(Step::Require {
        paths: vec!["{out}/result".into()],
        exec: false,
    });

    Recipe::mesboot("glibc-mesboot-shared-test", "1.0")
        .native_inputs(&["gcc-mesboot1", "binutils-mesboot", "glibc-mesboot-shared"])
        .inputs_owned(base_inputs(&[]))
        .steps(steps)
        .checks(vec![RecipeCheck::daily(
            r#"
echo ">> recipe-check glibc-mesboot-shared-test: build-plan --auto builds+validates the source-bootstrap brick-6 dynamic toolchain (gcc-mesboot1 + binutils-mesboot + glibc-mesboot-shared) — a real C program links DYNAMICALLY against the shared glibc and RUNS, returning 42; a broken toolchain reds glibc-mesboot-shared-test's build"
sh <<'CHECK'
set -eu
ROOT=$(pwd)
fail() { echo "FAIL: $*" >&2; exit 1; }
. tests/cache-lib.sh
export TD_STAGE0_BASE="$PWD/.td-build-cache/td-shell"
load_stage0 || fail "stage0-builder could not place a guix-free stage0 td-builder"
. tests/bootstrap-chain.sh
bootstrap_modern_toolchain || fail "the recipe ladder did not build the chain"
export TD_STORE_DIR=/td/store
ladder_emit glibc-mesboot-shared-test || fail "emit the glibc-mesboot-shared-test recipe"
ladder_build glibc-mesboot-shared-test || fail "build-plan --auto glibc-mesboot-shared-test — the dynamic toolchain failed its own link+run test"
echo "PASS: glibc-mesboot-shared-test — the source-bootstrap brick-6 dynamic toolchain (gcc-mesboot1 + binutils-mesboot + glibc-mesboot-shared) linked and ran a real dynamic C program in the recipe sandbox (a broken toolchain would red this)"
CHECK
"#,
        )])
}

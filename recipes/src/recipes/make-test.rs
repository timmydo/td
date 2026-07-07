use crate::ladder::{base_inputs, base_path, SH};
use crate::types::{Recipe, RecipeCheck, Step};

// make-test — the behavioral validation of make-x86-64 (issue #388 rung 1), modeled as a
// RECIPE rather than a bespoke tests/ check script: it DEPENDS on make-x86-64 and, in its
// build steps, RUNS the built GNU make 4.4.1 to drive a real one-rule build and checks the
// result. If make is broken (wrong output, crashes, or embeds /gnu/store bytes), a step
// exits nonzero → make-test's build fails → the recipe check reds. So the feature ("a
// /td/store make that actually builds things") is exercised through make's real entry
// point, with no shell scaffolding under tests/.
//
// make-test compiles nothing — it just RUNS make — so it needs no toolchain, only the built
// make (native_input) + the base tools to script the checks. A static make has no ELF
// interp/RUNPATH, so "no runtime guix dependency" is proven by a byte-grep of the make
// binary (below); the elaborate /gnu/store-absent own-root namespace the corpus checks use
// buys nothing for a static binary.
//
// Validation is the recipe-owned RecipeCheck::daily (below): `build-plan --auto make-test`
// realizes the native toolchain → make-x86-64 → make-test; make-test's steps run make. HEAVY
// (from-seed native toolchain, #371) — deferred to the daily backstop, same posture as the
// rust-toolchain recipe check.
pub fn recipe() -> Recipe {
    let make = "{in:make-x86-64}/bin/make";
    let mut steps = Vec::new();
    // [provenance] the built make carries zero /gnu/store bytes. A static binary with no
    // /gnu/store reference cannot have a runtime guix dependency — this IS the no-guix leg.
    steps.push(
        Step::run(
            "{root}",
            &[
                SH,
                "-c",
                &format!(
                    "if grep -q -a /gnu/store '{make}'; then echo 'make embeds /gnu/store bytes' >&2; exit 1; fi"
                ),
            ],
        )
        .env("PATH", &base_path()),
    );
    // a real one-rule Makefile: exercises a variable ($(V)), a target/prereq, recipe
    // execution via SHELL, and the automatic variable $@. The recipe body uses only the
    // shell's printf builtin + redirection, so the build needs no coreutils.
    steps.push(Step::MkDir {
        path: "{root}/t".into(),
    });
    steps.push(Step::WriteFile {
        path: "{root}/t/Makefile".into(),
        content: "V := world\nall: greeting.txt\ngreeting.txt:\n\tprintf 'hello, %s\\n' '$(V)' > $@\n.PHONY: all\n"
            .into(),
        exec: false,
    });
    // [behavioral] RUN the built make → it drives the build (produces greeting.txt).
    steps.push(
        Step::run(
            "{root}/t",
            &[make, "SHELL={in:bash}/bin/bash"],
        )
        .env("PATH", &base_path()),
    );
    // assert make actually produced the expected output (not just exited 0).
    steps.push(
        Step::run(
            "{root}/t",
            &[
                SH,
                "-c",
                "grep -qx 'hello, world' greeting.txt || { echo 'make did not drive the build (greeting.txt wrong/absent)' >&2; exit 1; }",
            ],
        )
        .env("PATH", &base_path()),
    );
    // the recipe output: a marker + the built artifact, so make-test interns a small tree.
    steps.push(
        Step::run(
            "{root}/t",
            &[
                SH,
                "-c",
                "mkdir -p '{out}'; cp greeting.txt '{out}/greeting.txt'; printf 'PASS: GNU make 4.4.1 (native /td/store toolchain) drove a real build\\n' > '{out}/result'",
            ],
        )
        .env("PATH", &base_path()),
    );
    steps.push(Step::Require {
        paths: vec!["{out}/result".into()],
        exec: false,
    });
    Recipe::mesboot("make-test", "1.0")
        .native_inputs(&["make-x86-64"])
        .inputs_owned(base_inputs(&[]))
        .steps(steps)
        .checks(vec![RecipeCheck::daily(
            r#"
echo ">> recipe-check make-test: build-plan --auto builds+validates make-test — GNU make 4.4.1, built on the native /td/store toolchain (make-x86-64), RUNS a real build; a broken make reds make-test's build"
sh <<'CHECK'
set -eu
ROOT=$(pwd)
fail() { echo "FAIL: $*" >&2; exit 1; }
. tests/cache-lib.sh
. tests/x86_64-cross-fns.sh
. tests/ladder-lib.sh
export TD_STAGE0_BASE="$PWD/.td-build-cache/td-shell"
load_stage0 || fail "stage0-builder could not place a guix-free stage0 td-builder"
load_recipe_eval || fail "no td-recipe-eval"
export TD_STORE_DIR=/td/store
# build-plan --auto make-test realizes the whole graph: the native /td/store x86_64 toolchain
# (run_x86_64_native emits+locks it), make-x86-64, and make-test. make-test's steps RUN make.
run_x86_64_native || fail "the native /td/store x86_64 toolchain failed to build (build-plan --auto)"
ladder_intern_extra make-x86-64-source make-4.4.1 || fail "intern the pinned make 4.4.1 source"
ladder_emit make-x86-64 make-test || fail "emit the make-x86-64/make-test recipes"
ladder_lock make-x86-64 make-x86-64-source rung:gcc-x86-64-native rung:binutils-x86-64-native rung:glibc-x86-64 src:linux-headers-x86-64 tool:make $_bt || fail "compose the make-x86-64 lock"
# make-test has no source (its Makefile is written in-step); its lock is make-x86-64 (rung) +
# the base tools (ladder_lock always emits a -source line, so compose this one directly).
{ echo "make-x86-64 /td/store/pending-make-x86-64"; for e in $_bt; do t=${e#tool:}; echo "$t $(ladder_map "$t") seed"; done; } > "$LW/locks/make-test-no-guix.lock" || fail "compose the make-test lock"
ladder_build make-test || fail "build-plan --auto make-test — the built make failed its own build test"
echo "PASS: make-test — GNU make 4.4.1 built on the native /td/store toolchain drove a real build in the recipe sandbox (a broken make would red this)"
CHECK
"#,
        )])
}

use crate::ladder::{mesboot0_inputs, mesboot0_path, SH};
use crate::types::{CheckRunner, Recipe, RecipeCheck, Step};

// make-test — the behavioral validation of make-x86-64 (issue #388 rung 1), modeled as a
// RECIPE rather than a bespoke tests/ check script: it DEPENDS on make-x86-64 and, in its
// build steps, RUNS the built GNU make 4.4.1 to drive a real one-rule build and checks the
// result. If make is broken (wrong output, crashes, or embeds /gnu/store bytes), a step
// exits nonzero → make-test's build fails → the recipe check reds. So the feature ("a
// /td/store make that actually builds things") is exercised through make's real entry
// point, with no shell scaffolding under tests/.
//
// make-test compiles nothing — it just RUNS make — so it needs no toolchain, only the built
// make (native_input) + the mesboot0 tools to script the checks. A static make has no ELF
// interp/RUNPATH, so "no runtime guix dependency" is proven by a byte-grep of the make
// binary (below); the elaborate /gnu/store-absent own-root namespace the corpus checks use
// buys nothing for a static binary.
//
// Validation is the recipe-owned RecipeCheck::daily (below): `build-plan --auto make-test`
// realizes the native toolchain → make-x86-64 → make-test; make-test's steps run make. HEAVY
// (from-seed native toolchain, #371) — deferred to the daily backstop, same posture as the
// rust-toolchain recipe check.
// Host-free scripting tools: mesboot0 (grep-mesboot0 for the no-guix byte grep). re #469.
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
        .env("PATH", &mesboot0_path()),
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
    steps
        .push(Step::run("{root}/t", &[make, "SHELL={in:bash-mesboot}/bin/bash"]).env("PATH", &mesboot0_path()));
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
        .env("PATH", &mesboot0_path()),
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
        .env("PATH", &mesboot0_path()),
    );
    steps.push(Step::Require {
        paths: vec!["{out}/result".into()],
        exec: false,
    });
    Recipe::mesboot("make-test", "1.0")
        .native_inputs(&["make-x86-64"])
        .inputs_owned(mesboot0_inputs(&[]))
        .steps(steps)
        .checks(vec![RecipeCheck::daily(
            r#"
echo ">> recipe-check make-test: build-plan --auto builds+validates make-test — GNU make 4.4.1, built on the native /td/store toolchain (make-x86-64), RUNS a real build; a broken make reds make-test's build"
: "${TD_RECIPE_EVAL:=$PWD/target/release/td-recipe-eval}"
exec "$TD_RECIPE_EVAL" check-run make-test daily 1
"#,
        )
        .with_runner(CheckRunner::BuildOnly)])
}

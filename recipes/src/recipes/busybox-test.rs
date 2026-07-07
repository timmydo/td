use crate::ladder::{base_inputs, base_path, SH};
use crate::types::{Recipe, RecipeCheck, Step};

// busybox-test: behavioral validation of busybox-x86-64 (#388 rung 2), modeled
// as a recipe rather than a bespoke tests/ check script. It depends on the built
// BusyBox tree and runs installed applet links through PATH, so missing or broken
// sh/ls/grep/sed links red the build.
//
// BusyBox is static, so a byte grep of the busybox binary is the no-guix runtime
// leg; there is no interpreter/RUNPATH closure to stage in an own root. The
// applet-link behavior is the user-facing feature later rungs consume.
pub fn recipe() -> Recipe {
    let bb = "{in:busybox-x86-64}/bin";
    let mut steps = Vec::new();

    steps.push(
        Step::run(
            "{root}",
            &[
                SH,
                "-c",
                &format!(
                    "if grep -q -a /gnu/store '{bb}/busybox'; then echo 'busybox embeds /gnu/store bytes' >&2; exit 1; fi"
                ),
            ],
        )
        .env("PATH", &base_path()),
    );
    steps.push(Step::MkDir {
        path: "{root}/t".into(),
    });
    steps.push(Step::WriteFile {
        path: "{root}/t/in.txt".into(),
        content: "alpha beta\n".into(),
        exec: false,
    });
    steps.push(
        Step::run(
            "{root}/t",
            &[
                SH,
                "-c",
                &format!(
                    "export PATH='{bb}'; \
                     [ \"$(sh -c 'echo hi')\" = hi ] || {{ echo 'sh applet link failed' >&2; exit 1; }}; \
                     ls . | grep -qx in.txt || {{ echo 'ls/grep applet links failed' >&2; exit 1; }}; \
                     sed 's/beta/gamma/' in.txt > out.txt; \
                     grep -qx 'alpha gamma' out.txt || {{ echo 'sed applet link failed' >&2; exit 1; }}"
                ),
            ],
        )
        .env("PATH", bb),
    );
    steps.push(Step::MkDir {
        path: "{out}".into(),
    });
    steps.push(Step::CopyFiles {
        files: vec!["{root}/t/out.txt".into()],
        dest: "{out}".into(),
    });
    steps.push(Step::WriteFile {
        path: "{out}/result".into(),
        content: "PASS: BusyBox 1.37.0 applet links ran from the built /td/store tree\n".into(),
        exec: false,
    });
    steps.push(Step::Require {
        paths: vec!["{out}/result".into()],
        exec: false,
    });

    Recipe::mesboot("busybox-test", "1.0")
        .native_inputs(&["busybox-x86-64"])
        .inputs_owned(base_inputs(&[]))
        .steps(steps)
        .checks(vec![RecipeCheck::daily(
            r#"
echo ">> recipe-check busybox-test: build-plan --auto builds+validates busybox-test: BusyBox 1.37.0, built by make-x86-64 on the native /td/store toolchain, RUNS installed applet links"
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
run_x86_64_native || fail "the native /td/store x86_64 toolchain failed to build (build-plan --auto)"
ladder_intern_extra make-x86-64-source make-4.4.1 || fail "intern the pinned make 4.4.1 source"
ladder_intern_extra busybox-x86-64-source busybox-1.37.0 || fail "intern the pinned BusyBox 1.37.0 source"
ladder_emit make-x86-64 busybox-x86-64 busybox-test || fail "emit the make-x86-64/busybox-x86-64/busybox-test recipes"
ladder_build busybox-test || fail "build-plan --auto busybox-test: the built BusyBox applet links failed their build test"
echo "PASS: busybox-test: BusyBox 1.37.0 built by make-x86-64 on the native /td/store toolchain ran installed sh/ls/grep/sed applet links"
CHECK
"#,
        )])
}

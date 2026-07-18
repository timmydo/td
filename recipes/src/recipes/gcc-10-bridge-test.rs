use crate::types::{CheckRunner, Recipe, RecipeCheck, Step};

const GCC: &str = "{in:gcc-10-bridge}/bin/gcc";
const GPP: &str = "{in:gcc-10-bridge}/bin/g++";
const CC1: &str = "{in:gcc-10-bridge}/libexec/gcc/i686-unknown-linux-gnu/10.5.0/cc1";
const GCC_LIB: &str = "{in:gcc-10-bridge}/lib/gcc/i686-unknown-linux-gnu/10.5.0";
const BIN: &str = "{in:binutils-244}/bin";
const LIBC: &str = "{in:glibc-mesboot}";

// Exercise both the ordinary installed drivers and the exact cc1 option shape
// that exposed the failed direct GCC 4.9.4 -> GCC 14 build. In particular,
// -iprefix/-isysroot/two -isystem paths combined with -march=pentiumpro must
// return rather than looping through a corrupt option-table neg_index cycle.
pub fn recipe() -> Recipe {
    Recipe::mesboot("gcc-10-bridge-test", "1.0")
        .native_inputs(&["gcc-10-bridge", "binutils-244", "glibc-mesboot"])
        .steps(vec![
            Step::MkDir {
                path: "{root}/test".into(),
            },
            Step::WriteFile {
                path: "{root}/test/probe.c".into(),
                content: "int main(void) { return 0; }\n".into(),
                exec: false,
            },
            Step::WriteFile {
                path: "{root}/test/headers.c".into(),
                content: "#include <stdlib.h>\nint main(void) { void *p = malloc(1); free(p); return 0; }\n"
                    .into(),
                exec: false,
            },
            Step::WriteFile {
                path: "{root}/test/probe.cc".into(),
                content: "#include <cstdlib>\nint main() { void *p = std::malloc(1); std::free(p); return 0; }\n"
                    .into(),
                exec: false,
            },
            Step::run(
                "{root}/test",
                &[
                    CC1,
                    "-quiet",
                    "-iprefix",
                    &format!("{GCC_LIB}/"),
                    "-isysroot",
                    LIBC,
                    "-isystem",
                    &format!("{GCC_LIB}/include"),
                    "-isystem",
                    &format!("{GCC_LIB}/include-fixed"),
                    "{root}/test/probe.c",
                    "-quiet",
                    "-mtune=generic",
                    "-march=pentiumpro",
                    "-o",
                    "{root}/test/probe.s",
                ],
            ),
            Step::run(
                "{root}/test",
                &[
                    GCC,
                    "-E",
                    "-dM",
                    "-xc",
                    "{root}/test/probe.c",
                    "-o",
                    "{root}/test/macros",
                ],
            ),
            compile_step(GCC, "{root}/test/headers.c", "{root}/test/probe-c"),
            compile_step(GPP, "{root}/test/probe.cc", "{root}/test/probe-cxx"),
            Step::run("{root}/test", &["{root}/test/probe-c"]),
            Step::run("{root}/test", &["{root}/test/probe-cxx"]),
            Step::WriteFile {
                path: "{out}/result".into(),
                content: "gcc-10 bridge recipe test passed\n".into(),
                exec: false,
            },
            Step::Require {
                paths: vec![
                    "{root}/test/probe.s".into(),
                    "{root}/test/macros".into(),
                    "{out}/result".into(),
                ],
                exec: false,
            },
        ])
        .checks(vec![RecipeCheck::daily(
            r#"
echo "[td] running gcc-10-bridge-test recipe check"
: "${TD_RECIPE_EVAL:=$PWD/recipes/target/release/td-recipe-eval}"
exec "$TD_RECIPE_EVAL" check-run gcc-10-bridge-test daily 1
"#,
        )
        .with_runner(CheckRunner::BuildOnly)])
}

fn compile_step(compiler: &str, source: &str, output: &str) -> Step {
    Step::run(
        "{root}/test",
        &[
            compiler,
            "-static",
            "-idirafter",
            &format!("{LIBC}/include"),
            &format!("-B{BIN}/"),
            &format!("-B{LIBC}/lib/"),
            &format!("-L{LIBC}/lib"),
            "-o",
            output,
            source,
        ],
    )
}

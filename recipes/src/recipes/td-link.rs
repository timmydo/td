use crate::ladder::{target_rustc_linked_by, BUILD_ROOT_GLIBC as GLIBC, BUILD_ROOT_PATH as PATH};
use crate::types::{CheckRunner, Recipe, RecipeCheck, Step};

// td-link, the linker of every td Rust output (td-link/DESIGN.md): rustc's
// gcc-style link line onto the toolchain's rust-lld, with td glibc's and
// gcc-runtime's start files and libraries.
//
// It builds in the Debian build root only for its first stage. Debian's gcc
// links a throwaway static td-link from td glibc's and gcc-runtime's start
// files (`-B`) and libraries (`-L`); that stage links the shipped driver,
// and the shipped driver links itself again, which must reproduce it byte
// for byte. The linked bytes are therefore rust-lld's, driven by td-link's
// own logic, with no object of the root's in them. The root's objcopy then
// splits the debug companion and strips the shipped file, as it does for
// every build-root output.
const MAIN_RS: &str = include_str!("../../../td-link/src/main.rs");
const PLAN_RS: &str = include_str!("../../../td-link/src/plan.rs");
const RESPONSE_RS: &str = include_str!("../../../td-link/src/response.rs");

const RUSTC: &str = "{in:rust-toolchain}/bin/rustc";
const RUNTIME: &str = "{in:gcc-runtime}/lib";

/// Compile td-link linked by `linker` into `output`, with the store paths
/// it compiles in.
fn stage(linker: &str, output: &str, link_args: &[&str]) -> Step {
    let mut args = vec![
        "--edition=2021",
        "--target=x86_64-unknown-linux-gnu",
        "--crate-name=td_link",
        "-Copt-level=s",
        "-Cpanic=abort",
        "-Ctarget-feature=+crt-static",
        "-Crelocation-model=static",
    ];
    args.extend_from_slice(link_args);
    args.extend_from_slice(&["-o", output, "{src}/main.rs"]);
    target_rustc_linked_by("{src}", RUSTC, &args, linker)
        .env(
            "TD_LINK_LLD",
            "{in:rust-toolchain}/lib/rustlib/x86_64-unknown-linux-gnu/bin/rust-lld",
        )
        .env("TD_LINK_GLIBC", &format!("{GLIBC}/lib"))
        .env("TD_LINK_RUNTIME", RUNTIME)
        .env("SOURCE_DATE_EPOCH", "1")
}

pub fn recipe() -> Recipe {
    let glibc_b = format!("-Clink-arg=-B{GLIBC}/lib/");
    let glibc_l = format!("-Clink-arg=-L{GLIBC}/lib");
    let runtime_b = format!("-Clink-arg=-B{RUNTIME}/");
    let runtime_l = format!("-Clink-arg=-L{RUNTIME}");
    let mut steps = Vec::new();
    for path in ["{out}/bin", "{root}/stage1", "{root}/stage3"] {
        steps.push(Step::MkDir { path: path.into() });
    }
    for (name, content) in [
        ("main.rs", MAIN_RS),
        ("plan.rs", PLAN_RS),
        ("response.rs", RESPONSE_RS),
    ] {
        steps.push(Step::WriteFile {
            path: format!("{{src}}/{name}"),
            content: content.into(),
            exec: false,
        });
    }
    steps.push(
        stage(
            "/usr/bin/gcc",
            "{root}/stage1/td-link",
            &[&glibc_b, &runtime_b, &glibc_l, &runtime_l],
        )
        .env("PATH", PATH),
    );
    steps.push(stage("{root}/stage1/td-link", "{out}/bin/td-link", &[]));
    steps.push(stage("{out}/bin/td-link", "{root}/stage3/td-link", &[]));
    steps.push(Step::CompareFiles {
        left: "{out}/bin/td-link".into(),
        right: "{root}/stage3/td-link".into(),
    });
    steps.push(Step::split_debug_tree("{out}", "/usr/bin/objcopy"));
    steps.push(Step::assert_static(&["{out}/bin/td-link"]));

    Recipe::mesboot("td-link", "0.1")
        .build_root()
        .native_inputs(&["rust-toolchain", "glibc-x86-64", "gcc-runtime"])
        .steps(steps)
        .checks(vec![RecipeCheck::new(
            r#"
echo ">> recipe-check td-link: link the Rust link driver with itself, twice, byte-identically"
: "${TD_RECIPE_EVAL:=$PWD/target/release/td-recipe-eval}"
exec "$TD_RECIPE_EVAL" check-run td-link 1
"#,
        )
        .with_runner(CheckRunner::BuildOnly)])
}

use crate::ladder::{post_rust_inputs, post_rust_tool_farm, POST_RUST_SH};
use crate::types::{CheckRunner, Recipe, RecipeCheck, Step};

// The build-only bc's realized output on the post-Rust tool farm: a program
// shaped like the kernel's timeconst.bc (a function with an auto, arithmetic
// past 64 bits, obase=16, print with its \q escape, read() and halt, the GNU
// extensions that script needs), its version, and a static x86-64 ELF naming
// neither a foreign store nor the gawk and make that built it.
pub fn recipe() -> Recipe {
    let bc = "{in:bc-x86-64-self}/bin/bc";
    let readelf = "{in:binutils-x86-64-self}/bin/readelf";
    let script = format!(
        "set -e; \
         out=$(printf '%s\\n' 3 | '{bc}' -q '{{root}}/test/timeconst-shaped.bc'); \
         [ \"$out\" = '\"0x5555555555555555\"' ] || {{ echo \"bc timeconst-shaped: $out\" >&2; exit 1; }}; \
         v=$('{bc}' --version); \
         case $v in 'bc 1.08.2'*) ;; *) echo \"bc version: $v\" >&2; exit 1;; esac; \
         h=$('{readelf}' -h -l '{bc}'); \
         case $h in *'ELF64'*) ;; *) echo 'bc is not ELF64' >&2; exit 1;; esac; \
         case $h in *'Advanced Micro Devices X86-64'*) ;; *) echo 'bc is not x86-64' >&2; exit 1;; esac; \
         case $h in *INTERP*) echo 'bc is not static' >&2; exit 1;; esac; \
         for ref in /gnu/store -gawk-x86-64-self- -make-x86-64-self-; do \
           r=0; grep -a -q -F -e \"$ref\" '{bc}' || r=$?; \
           [ \"$r\" = 1 ] || {{ echo \"bc names $ref (grep status $r)\" >&2; exit 1; }}; \
         done"
    );
    let steps = vec![
        Step::Require {
            paths: vec![bc.into()],
            exec: true,
        },
        post_rust_tool_farm("{in:gawk-x86-64-self}/bin/gawk"),
        Step::MkDir {
            path: "{root}/test".into(),
        },
        // halt inside the function, so the call's return value is never
        // printed after the line it builds.
        Step::WriteFile {
            path: "{root}/test/timeconst-shaped.bc".into(),
            content: "define f(n) {\n\
                      \x20 auto t\n\
                      \x20 t = 2^64 / n\n\
                      \x20 obase = 16\n\
                      \x20 print \"\\q0x\", t, \"\\q\\n\"\n\
                      \x20 halt\n\
                      }\n\
                      f(read())\n"
                .into(),
            exec: false,
        },
        Step::run("{root}/test", &[POST_RUST_SH, "-c", &script]).env("PATH", "{tools}"),
        Step::MkDir {
            path: "{out}".into(),
        },
        Step::WriteFile {
            path: "{out}/result".into(),
            content: "PASS: the build-only GNU bc 1.08.2 is a static x86-64 ELF and ran a \
                      timeconst-shaped program on the post-Rust tool farm\n"
                .into(),
            exec: false,
        },
        Step::Require {
            paths: vec!["{out}/result".into()],
            exec: false,
        },
    ];

    Recipe::mesboot("bc-x86-64-self-test", "1.0")
        .native_inputs(&post_rust_inputs(
            "gawk-x86-64-self",
            &["bc-x86-64-self", "binutils-x86-64-self"],
        ))
        .steps(steps)
        .checks(vec![RecipeCheck::new(
            r#"
echo ">> recipe-check bc-x86-64-self-test: run the build-only GNU bc on the post-Rust tool farm and check its ELF"
: "${TD_RECIPE_EVAL:=$PWD/target/release/td-recipe-eval}"
exec "$TD_RECIPE_EVAL" check-run bc-x86-64-self-test 1
"#,
        )
        .with_runner(CheckRunner::BuildOnly)])
}

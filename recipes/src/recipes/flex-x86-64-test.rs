use crate::ladder::{mesboot0_inputs, mesboot0_path, SH};
use crate::types::{CheckRunner, Recipe, RecipeCheck, Step};

// flex-x86-64-test: behavioral validation of the source-built `flex` (#529),
// mirroring make-test / busybox-test (build the producer, RUN its output). The
// kernel rung needs flex to actually GENERATE a scanner — and flex's scanner
// generation is the one novel runtime path here: flex expands its skeleton
// (flex.skl) through the m4 whose absolute path was baked in at configure
// (m4-mesboot). A broken M4 bake, or a flex that cannot exec that m4, yields an
// EMPTY or malformed scanner — a failure that would otherwise only surface deep
// in the kernel's kconfig build. So this rung feeds flex a trivial `.l` and
// asserts the emitted C is a real, m4-expanded scanner:
//   1. it contains the `yylex` entry point flex always emits,
//   2. it contains a skeleton marker (`YY_BUF_SIZE`) — proof the m4 skeleton
//      expansion actually ran (an empty/short file means m4 never fired),
//   3. flex reports the expected `flex 2.6.4` version banner.
// m4-mesboot is declared as an input so the absolute M4 path baked into flex
// resolves in the test sandbox.
pub fn recipe() -> Recipe {
    let flex = "{in:flex-x86-64}/bin/flex";
    let mut steps = Vec::new();

    // A minimal but non-trivial scanner: a rule body forces flex through the
    // full skeleton expansion, not just a header stub.
    steps.push(Step::WriteFile {
        path: "{root}/t.l".into(),
        content: "%%\n[0-9]+    { return 1; }\n.|\\n     { }\n%%\nint yywrap(void){ return 1; }\n"
            .into(),
        exec: false,
    });
    steps.push(
        Step::run(
            "{root}",
            &[
                SH,
                "-c",
                &format!(
                    "'{flex}' -o '{{root}}/out.c' '{{root}}/t.l' || {{ echo 'flex failed to generate a scanner' >&2; exit 1; }}; \
                     grep -q 'yylex' '{{root}}/out.c' || {{ echo 'generated scanner has no yylex — flex produced no real output' >&2; exit 1; }}; \
                     grep -q 'YY_BUF_SIZE' '{{root}}/out.c' || {{ echo 'generated scanner lacks the m4-expanded skeleton (YY_BUF_SIZE) — the runtime m4 bake is broken' >&2; exit 1; }}; \
                     v=$('{flex}' --version 2>&1); \
                     printf '%s\\n' \"$v\" | grep -q 'flex 2[.]6[.]4' || {{ echo \"flex version banner is not 2.6.4 (got '$v')\" >&2; exit 1; }}"
                ),
            ],
        )
        .env("PATH", &mesboot0_path()),
    );

    steps.push(Step::MkDir {
        path: "{out}".into(),
    });
    steps.push(Step::WriteFile {
        path: "{out}/result".into(),
        content: "PASS: flex 2.6.4, source-built by the native /td/store x86_64 toolchain, generates a well-formed m4-expanded scanner\n".into(),
        exec: false,
    });
    steps.push(Step::Require {
        paths: vec!["{out}/result".into()],
        exec: false,
    });

    Recipe::mesboot("flex-x86-64-test", "1.0")
        .native_inputs(&["flex-x86-64", "m4-mesboot"])
        .inputs_owned(mesboot0_inputs(&[]))
        .steps(steps)
        .checks(vec![RecipeCheck::daily(
            r#"
echo ">> recipe-check flex-x86-64-test: build-plan --auto builds flex-x86-64 (GNU flex 2.6.4, source-built by the native /td/store x86_64 toolchain) and asserts it generates a well-formed m4-expanded scanner"
: "${TD_RECIPE_EVAL:=$PWD/recipes/target/release/td-recipe-eval}"
exec "$TD_RECIPE_EVAL" check-run flex-x86-64-test daily 1
"#,
        )
        .with_runner(CheckRunner::BuildOnly)])
}

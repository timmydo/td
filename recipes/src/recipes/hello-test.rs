use crate::ladder::{mesboot0_inputs, mesboot0_path, SH};
use crate::types::{CheckRunner, Recipe, RecipeCheck, Step};

// Behavioral capstone for GNU Hello (#424). This recipe runs as a normal
// build-plan subject inside td's own-root recipe sandbox: /td/store is the
// active store and /gnu/store is absent. It therefore proves both that the
// native recipe graph builds an ordinary dynamic package and that the package
// resolves only its declared td glibc closure at runtime.
pub fn recipe() -> Recipe {
    let hello = "{in:hello}/bin/hello";
    let readelf = "{in:binutils-x86-64-native}/bin/readelf";
    let xglibc = "{in:glibc-x86-64}/stage/td/store/glibc-2.41-x86_64";
    let mut steps = Vec::new();

    steps.push(
        Step::run(
            "{root}",
            &[
                SH,
                "-c",
                &format!(
                    "[ -d /td/store ] || {{ echo '/td/store is not the active store' >&2; exit 1; }}; \
                     [ ! -e /gnu/store ] || {{ echo '/gnu/store is visible in the hello sandbox' >&2; exit 1; }}; \
                     if grep -q -a /gnu/store '{hello}'; then echo 'hello embeds /gnu/store bytes' >&2; exit 1; fi; \
                     h=$('{readelf}' -h '{hello}'); \
                     printf '%s\\n' \"$h\" | grep -i 'class:' | grep -qi ELF64 || {{ echo 'hello is not ELF64' >&2; exit 1; }}; \
                     printf '%s\\n' \"$h\" | grep -i 'machine:' | grep -qi x86-64 || {{ echo 'hello is not x86-64' >&2; exit 1; }}; \
                     p=$('{readelf}' -l '{hello}'); \
                     printf '%s\\n' \"$p\" | grep -q '{xglibc}/lib/ld-linux-x86-64.so.2' || {{ echo 'hello does not use td glibc as its interpreter' >&2; exit 1; }}; \
                     actual=$('{hello}'); \
                     [ \"$actual\" = 'Hello, world!' ] || {{ echo \"hello output is wrong: $actual\" >&2; exit 1; }}"
                ),
            ],
        )
        .env("PATH", &mesboot0_path()),
    );
    steps.push(Step::WriteFile {
        path: "{out}/result".into(),
        content: "PASS: GNU Hello 2.10 built and ran on td's native /td/store toolchain\n".into(),
        exec: false,
    });
    steps.push(Step::Require {
        paths: vec!["{out}/result".into()],
        exec: false,
    });

    Recipe::mesboot("hello-test", "1.0")
        .native_inputs(&["hello", "binutils-x86-64-native", "glibc-x86-64"])
        .inputs_owned(mesboot0_inputs(&[]))
        .steps(steps)
        .checks(vec![
            RecipeCheck::daily(
                r#"
echo ">> recipe-check hello-test: build GNU Hello 2.10 with the native /td/store GCC/glibc/Make/BusyBox graph and run it with /gnu/store absent"
: "${TD_RECIPE_EVAL:=$PWD/recipes/target/release/td-recipe-eval}"
exec "$TD_RECIPE_EVAL" check-run hello-test daily 1
"#,
            )
            .with_runner(CheckRunner::BuildOnly),
        ])
}

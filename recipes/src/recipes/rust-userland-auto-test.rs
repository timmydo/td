use crate::ladder::{mesboot0_inputs, mesboot0_path, SH};
use crate::types::{CheckRunner, Recipe, RecipeCheck, Step};

// Keep in lockstep with NATIVE_GLIBC_STAGE and rust_toolchain::GLIBC_STAGE.
const GLIBC_STAGE: &str = "stage/td/store/glibc-2.41-x86_64";

fn dynamic_contract(label: &str, binary: &str, expected_needed: &str) -> Step {
    let readelf = "{in:binutils-x86-64-native}/bin/readelf";
    let glibc = format!("{{in:glibc-x86-64}}/{GLIBC_STAGE}");
    Step::run(
        "{root}",
        &[
            SH,
            "-c",
            &format!(
                "[ -x '{binary}' ] || {{ echo '{label} output is not executable' >&2; exit 1; }}; \
                 if grep -Fq -a /gnu/store '{binary}'; then echo '{label} embeds /gnu/store bytes' >&2; exit 1; fi; \
                 stage0='{{in:rust-stage0}}'; stage0_base=${{stage0##*/}}; \
                 if grep -Fq -a \"$stage0_base\" '{binary}'; then echo '{label} embeds Rust stage0 bytes' >&2; exit 1; fi; \
                 h=$('{readelf}' -h '{binary}') || {{ echo 'readelf -h failed on {label}' >&2; exit 1; }}; \
                 printf '%s\\n' \"$h\" | grep -i 'class:' | grep -qi ELF64 || {{ echo '{label} is not ELF64' >&2; exit 1; }}; \
                 printf '%s\\n' \"$h\" | grep -i 'machine:' | grep -qi x86-64 || {{ echo '{label} is not x86-64' >&2; exit 1; }}; \
                 p=$('{readelf}' -l '{binary}') || {{ echo 'readelf -l failed on {label}' >&2; exit 1; }}; \
                 printf '%s\\n' \"$p\" | grep -Fq '{glibc}/lib/ld-linux-x86-64.so.2' || {{ echo '{label} does not use the declared td glibc interpreter' >&2; exit 1; }}; \
                 d=$('{readelf}' -d '{binary}') || {{ echo 'readelf -d failed on {label}' >&2; exit 1; }}; \
                 needed=$(printf '%s\\n' \"$d\" | sed -n 's/^.*(NEEDED).*Shared library: \\[\\([^]]*\\)\\].*$/\\1/p' | sort); \
                 [ \"$needed\" = '{expected_needed}' ] || {{ echo \"{label} has an unexpected DT_NEEDED closure: $needed\" >&2; exit 1; }}; \
                 runpath=$(printf '%s\\n' \"$d\" | sed -n 's/^.*(RUNPATH).*Library runpath: \\[\\([^]]*\\)\\].*$/\\1/p'); \
                 [ -z \"$runpath\" ] || {{ echo \"{label} has an unexpected DT_RUNPATH: $runpath\" >&2; exit 1; }}; \
                 rpath=$(printf '%s\\n' \"$d\" | sed -n 's/^.*(RPATH).*Library rpath: \\[\\([^]]*\\)\\].*$/\\1/p'); \
                 [ \"$rpath\" = '{glibc}/lib' ] || {{ echo \"{label} has an unexpected DT_RPATH: $rpath\" >&2; exit 1; }}"
            ),
        ],
    )
    .env("PATH", &mesboot0_path())
}

pub fn recipe() -> Recipe {
    let rg = "{in:ripgrep}/bin/rg";
    let fd = "{in:fd}/bin/fd";
    let fixture = "{root}/fixtures/known-needle.txt";
    let mut steps = vec![
        dynamic_contract("ripgrep", rg, "ld-linux-x86-64.so.2\nlibc.so.6"),
        dynamic_contract("fd", fd, "libc.so.6"),
        Step::MkDir {
            path: "{root}/fixtures".into(),
        },
        Step::WriteFile {
            path: fixture.into(),
            content: "noise\nneedle\n".into(),
            exec: false,
        },
    ];
    steps.push(
        Step::run(
            "{root}",
            &[
                SH,
                "-c",
                &format!(
                    "actual=$('{rg}' --color never --no-filename '^needle$' '{fixture}') || {{ echo 'ripgrep search failed' >&2; exit 1; }}; \
                     [ \"$actual\" = 'needle' ] || {{ echo \"ripgrep returned unexpected output: $actual\" >&2; exit 1; }}; \
                     actual=$('{fd}' --color never --absolute-path '^known-needle[.]txt$' '{{root}}/fixtures') || {{ echo 'fd search failed' >&2; exit 1; }}; \
                     [ \"$actual\" = '{fixture}' ] || {{ echo \"fd returned unexpected output: $actual\" >&2; exit 1; }}"
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
        content: "PASS: ripgrep and fd are target-built auto graph nodes with the declared td glibc runtime closure\n".into(),
        exec: false,
    });
    steps.push(Step::Require {
        paths: vec!["{out}/result".into()],
        exec: false,
    });

    Recipe::mesboot("rust-userland-auto-test", "1.0")
        .native_inputs(&[
            "ripgrep",
            "fd",
            "binutils-x86-64-native",
            "glibc-x86-64",
            "rust-stage0",
        ])
        .inputs_owned(mesboot0_inputs(&[]))
        .steps(steps)
        .checks(vec![
            RecipeCheck::daily(
                r#"
echo ">> recipe-check rust-userland-auto-test: build-plan --auto builds ripgrep and fd with the source-built Rust/native toolchain, verifies their exact dynamic runtime closure, and runs real searches with /gnu/store absent"
: "${TD_RECIPE_EVAL:=$PWD/target/release/td-recipe-eval}"
exec "$TD_RECIPE_EVAL" check-run rust-userland-auto-test daily 1
"#,
            )
            .with_runner(CheckRunner::BuildOnly),
        ])
}

use super::cryptsetup_x86_64::VERSION_LINE;
use crate::ladder::{post_rust_inputs, post_rust_tool_farm, POST_RUST_SH};
use crate::types::{CheckRunner, Recipe, RecipeCheck, Step};

// Check the realized cryptsetup's compiled-in surface: the exact feature line,
// no external token loader, AES-XTS LUKS defaults and a shipped debug
// companion. LUKS2 I/O needs the target kernel's AF_ALG and device mapper,
// which the build host need not provide, so encrypted read/write belongs to
// the QEMU oracle (td-install/ENCRYPTION.md, acceptance evidence).
pub fn recipe() -> Recipe {
    let cs = "{in:cryptsetup-x86-64}";
    let path = "{tools}";
    let steps = vec![
        post_rust_tool_farm("{in:gawk-x86-64-self}/bin/gawk"),
        Step::run(
            "{root}",
            &[
                POST_RUST_SH,
                "-c",
                &format!(
                    "v=$('{cs}/bin/cryptsetup' --version) || exit 1; \
                     [ \"$v\" = '{VERSION_LINE}' ] || {{ echo \"cryptsetup reports '$v', expected '{VERSION_LINE}'\" >&2; exit 1; }}; \
                     '{cs}/bin/cryptsetup' --help > '{{root}}/help' || exit 1; \
                     grep -q -x -F 'LUKS2 external token plugin support is disabled.' '{{root}}/help' || {{ echo 'cryptsetup can load external LUKS2 token plugins' >&2; exit 1; }}; \
                     grep -q -F 'LUKS: aes-xts-plain64, Key: 256 bits, LUKS header hashing: sha256' '{{root}}/help' || {{ echo 'cryptsetup LUKS defaults are not AES-XTS with SHA-256' >&2; exit 1; }}; \
                     [ -s '{cs}/lib/debug/bin/cryptsetup.static.debug' ] || {{ echo 'cryptsetup has no debug companion' >&2; exit 1; }}"
                ),
            ],
        )
        .env("PATH", path),
        Step::MkDir {
            path: "{out}".into(),
        },
        Step::WriteFile {
            path: "{out}/result".into(),
            content: "PASS: target-built static cryptsetup reports only blkid and the kernel crypto API, loads no external token plugin, defaults LUKS to AES-XTS with SHA-256, and ships its debug companion\n".into(),
            exec: false,
        },
        Step::Require {
            paths: vec!["{out}/result".into()],
            exec: false,
        },
    ];

    Recipe::mesboot("cryptsetup-x86-64-test", "1.0")
        .native_inputs(&post_rust_inputs("gawk-x86-64-self", &["cryptsetup-x86-64"]))
        .steps(steps)
        .checks(vec![RecipeCheck::new(
            r#"
echo ">> recipe-check cryptsetup-x86-64-test: the static cryptsetup reports its exact feature line, refuses token plugins, defaults to AES-XTS and ships its debug companion"
: "${TD_RECIPE_EVAL:=$PWD/target/release/td-recipe-eval}"
exec "$TD_RECIPE_EVAL" check-run cryptsetup-x86-64-test 1
"#,
        )
        .with_runner(CheckRunner::BuildOnly)])
}

use crate::ladder::{mesboot0_inputs, mesboot0_path, SH};
use crate::types::{CheckRunner, Recipe, RecipeCheck, Step};

// linux-x86-64-test: behavioral validation of the source-built `vmlinux` (#529).
// td ships no in-sandbox qemu, so a boot smoke test is out of scope for this rung
// (it belongs with the later bzImage/qemu step); instead this asserts the artifact
// is a well-formed x86-64 kernel image, per repo policy that recipes test their
// output. Three checks over the built ELF:
//   1. it is an ELF64 x86-64 *executable* (readelf: class ELF64, machine x86-64,
//      type EXEC) — the EXEC assertion proves it was linked, so a stray
//      relocatable `.o` (which would still be ELF64/x86-64 and carry the banner
//      via init/version.o) cannot satisfy the check,
//   2. it carries the `Linux version 7.1.4` banner (init/version.o's
//      linux_banner[], always obj-y) — proof the kernel actually compiled and
//      linked, not just that some ELF exists,
//   3. it embeds no `/gnu/store` bytes — the no-guix host-free leg, mirroring
//      busybox-test (td's native toolchain is /td/store; a /gnu/store byte would
//      mean a host-guix compiler/lib leaked into the image).
pub fn recipe() -> Recipe {
    let vmlinux = "{in:linux-x86-64}/vmlinux";
    let readelf = "{in:binutils-x86-64-native}/bin/readelf";
    let mut steps = Vec::new();

    steps.push(
        Step::run(
            "{root}",
            &[
                SH,
                "-c",
                &format!(
                    "h=$('{readelf}' -h '{vmlinux}'); \
                     printf '%s\\n' \"$h\" | grep -i 'class:'   | grep -qi 'ELF64'  || {{ echo 'vmlinux is not ELF64' >&2; exit 1; }}; \
                     printf '%s\\n' \"$h\" | grep -i 'machine:' | grep -qi 'x86-64' || {{ echo 'vmlinux is not x86-64' >&2; exit 1; }}; \
                     printf '%s\\n' \"$h\" | grep -i 'type:'    | grep -qi 'EXEC'   || {{ echo 'vmlinux is not a linked ELF executable (EXEC) — a stray relocatable .o would still be ELF64/x86-64 and carry the banner' >&2; exit 1; }}"
                ),
            ],
        )
        .env("PATH", &mesboot0_path()),
    );
    steps.push(
        Step::run(
            "{root}",
            &[
                SH,
                "-c",
                &format!(
                    "grep -q -a 'Linux version 7.1.4' '{vmlinux}' || {{ echo 'vmlinux is missing the Linux 7.1.4 banner' >&2; exit 1; }}"
                ),
            ],
        )
        .env("PATH", &mesboot0_path()),
    );
    steps.push(
        Step::run(
            "{root}",
            &[
                SH,
                "-c",
                &format!(
                    "if grep -q -a /gnu/store '{vmlinux}'; then echo 'vmlinux embeds /gnu/store bytes' >&2; exit 1; fi"
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
        content: "PASS: Linux 7.1.4 vmlinux, source-built by the native /td/store x86_64 toolchain, is a well-formed ELF64 x86-64 image carrying the Linux banner\n".into(),
        exec: false,
    });
    steps.push(Step::Require {
        paths: vec!["{out}/result".into()],
        exec: false,
    });

    Recipe::mesboot("linux-x86-64-test", "1.0")
        .native_inputs(&["linux-x86-64", "binutils-x86-64-native"])
        .inputs_owned(mesboot0_inputs(&[]))
        .steps(steps)
        .checks(vec![RecipeCheck::daily(
            r#"
echo ">> recipe-check linux-x86-64-test: build-plan --auto builds linux-x86-64 (Linux 7.1.4 vmlinux, source-built by the native /td/store x86_64 GCC 14 + glibc 2.41 toolchain) and asserts a well-formed ELF64 x86-64 image with the Linux banner"
: "${TD_RECIPE_EVAL:=$PWD/recipes/target/release/td-recipe-eval}"
exec "$TD_RECIPE_EVAL" check-run linux-x86-64-test daily 1
"#,
        )
        .with_runner(CheckRunner::BuildOnly)])
}

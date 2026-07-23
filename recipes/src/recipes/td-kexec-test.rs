use crate::ladder::{mesboot0_inputs, mesboot0_path, SH};
use crate::types::{CheckRunner, Recipe, RecipeCheck, Step};

// td-kexec-test: behavioral validation of the target-built guest kexec helper.
// Running it (an actual kexec) needs a booted guest, which belongs with the
// operator qemu spike OUTSIDE the host-free sandbox; instead this asserts — per
// repo policy that recipes test their output — that the shipped binary is the
// self-contained STATIC ELF the image-based boot requires. It re-proves, with an
// independent readelf walk, what the producer's `assert_static` fail-closes on:
//   1. td-kexec is an ELF64 x86-64 *executable* (readelf: class ELF64, machine
//      x86-64, type EXEC) — EXEC (not DYN) is the non-PIE static shape,
//   2. it carries NO PT_INTERP program header — nothing asks a dynamic loader to
//      map it, so it runs in a kexec initramfs that ships no ld-linux,
//   3. it has NO dynamic NEEDED entry (a fully static link has no dynamic
//      section at all) — an EMPTY runtime closure, no libc.so to resolve.
// The behavioural proof that it actually kexecs is the operator qemu spike (two-
// kernel marker boot under -accel tcg), which cannot run in this BuildOnly rung.
pub fn recipe() -> Recipe {
    let bin = "{in:td-kexec}/bin/td-kexec";
    let readelf = "{in:binutils-x86-64-native}/bin/readelf";
    let mut steps = Vec::new();

    steps.push(
        Step::run(
            "{root}",
            &[
                SH,
                "-c",
                &format!(
                    "h=$('{readelf}' -h '{bin}' 2>/dev/null) || {{ echo 'readelf -h failed on td-kexec' >&2; exit 1; }}; \
                     printf '%s\\n' \"$h\" | grep -i 'class:'   | grep -qi 'ELF64'  || {{ echo 'td-kexec is not ELF64' >&2; exit 1; }}; \
                     printf '%s\\n' \"$h\" | grep -i 'machine:' | grep -qi 'x86-64' || {{ echo 'td-kexec is not x86-64' >&2; exit 1; }}; \
                     printf '%s\\n' \"$h\" | grep -qE 'Type:[[:space:]]+EXEC([[:space:]]|$)' || {{ echo 'td-kexec is not a static ET_EXEC — a DYN/PIE (Type: DYN, whose parenthetical also says Executable) would need runtime relocation' >&2; exit 1; }}"
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
                    "lout=$('{readelf}' -l '{bin}' 2>/dev/null) || {{ echo 'readelf -l failed on td-kexec (cannot verify absence of PT_INTERP)' >&2; exit 1; }}; \
                     if printf '%s\\n' \"$lout\" | grep -qi 'INTERP'; then echo 'td-kexec carries a PT_INTERP program header — it is not static' >&2; exit 1; fi"
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
                    "dout=$('{readelf}' -d '{bin}' 2>/dev/null) || {{ echo 'readelf -d failed on td-kexec (cannot verify absence of dynamic NEEDED)' >&2; exit 1; }}; \
                     if printf '%s\\n' \"$dout\" | grep -qi 'NEEDED'; then echo 'td-kexec has a dynamic NEEDED entry — its runtime closure is not empty' >&2; exit 1; fi"
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
        content: "PASS: td-kexec is a statically-linked ELF64 x86-64 executable (ET_EXEC) with no PT_INTERP and no dynamic NEEDED entry — a self-contained guest kexec helper with an empty runtime closure, runnable in a kexec initramfs with no dynamic loader\n".into(),
        exec: false,
    });
    steps.push(Step::Require {
        paths: vec!["{out}/result".into()],
        exec: false,
    });

    Recipe::mesboot("td-kexec-test", "1.0")
        .native_inputs(&["td-kexec", "binutils-x86-64-native"])
        .inputs_owned(mesboot0_inputs(&[]))
        .steps(steps)
        .checks(vec![RecipeCheck::daily(
            r#"
echo ">> recipe-check td-kexec-test: build-plan --auto builds td-kexec (the guest kexec helper, statically linked by the /td/store target Rust + native GCC/binutils/glibc toolchain) and asserts a self-contained static ELF64 x86-64 executable (ET_EXEC, no PT_INTERP, no dynamic NEEDED)"
: "${TD_RECIPE_EVAL:=$PWD/recipes/target/release/td-recipe-eval}"
exec "$TD_RECIPE_EVAL" check-run td-kexec-test daily 1
"#,
        )
        .with_runner(CheckRunner::BuildOnly)])
}

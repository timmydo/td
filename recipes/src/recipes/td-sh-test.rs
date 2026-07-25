use crate::ladder::{mesboot0_inputs, mesboot0_path, SH};
use crate::types::{CheckRunner, Recipe, RecipeCheck, Step};

// td-sh-test: build-shape validation of the target-built shell.
//
// This asserts — per repo policy that recipes test their output — that the
// shipped td-sh binary is the self-contained STATIC ELF the boot-critical
// `/bin/sh` slot requires (busybox `sh` runs in stage-1 init before the dynamic
// uutils glibc closure is reachable). It re-proves, with an independent readelf
// walk, what the producer's `assert_static` fail-closes on:
//   1. td-sh is an ELF64 x86-64 *executable* (readelf: class ELF64, machine
//      x86-64, type EXEC) — EXEC (not DYN) is the non-PIE static shape,
//   2. it carries NO PT_INTERP program header — nothing asks a dynamic loader to
//      map it, so it can run before the dynamic loader/closure is present,
//   3. it has NO dynamic NEEDED entry (a fully static link has no dynamic
//      section at all) — an EMPTY runtime closure, no libc.so to resolve.
// It then RUNS the binary (`td-sh -c 'exit 0'`) so a mis-built ELF with correct
// headers but a broken entry point / static link fails here, not just a shape
// mismatch.
//
// This is a BUILD-shape + smoke-exec check, not a conformance one. td-sh's actual
// POSIX behavior is driven by the host-side conformance harness in the td-sh crate
// (tests/conformance.rs over the Oils spec corpus): while `main.rs` is an exit-0
// stub that harness is a RED baseline, and it is deliberately kept off the
// shared per-change gate (it is `#[ignore]`d) so it cannot break land-on-green.
// The shape build + smoke-exec here is green as soon as the toolchain links the stub.
pub fn recipe() -> Recipe {
    let bin = "{in:td-sh}/bin/td-sh";
    let readelf = "{in:binutils-x86-64-native}/bin/readelf";
    let mut steps = Vec::new();

    steps.push(
        Step::run(
            "{root}",
            &[
                SH,
                "-c",
                &format!(
                    "h=$('{readelf}' -h '{bin}' 2>/dev/null) || {{ echo 'readelf -h failed on td-sh' >&2; exit 1; }}; \
                     printf '%s\\n' \"$h\" | grep -i 'class:'   | grep -qi 'ELF64'  || {{ echo 'td-sh is not ELF64' >&2; exit 1; }}; \
                     printf '%s\\n' \"$h\" | grep -i 'machine:' | grep -qi 'x86-64' || {{ echo 'td-sh is not x86-64' >&2; exit 1; }}; \
                     printf '%s\\n' \"$h\" | grep -qE 'Type:[[:space:]]+EXEC([[:space:]]|$)' || {{ echo 'td-sh is not a static ET_EXEC — a DYN/PIE (Type: DYN, whose parenthetical also says Executable) would need runtime relocation' >&2; exit 1; }}"
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
                    "lout=$('{readelf}' -l '{bin}' 2>/dev/null) || {{ echo 'readelf -l failed on td-sh (cannot verify absence of PT_INTERP)' >&2; exit 1; }}; \
                     if printf '%s\\n' \"$lout\" | grep -qi 'INTERP'; then echo 'td-sh carries a PT_INTERP program header — it is not static' >&2; exit 1; fi"
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
                    "dout=$('{readelf}' -d '{bin}' 2>/dev/null) || {{ echo 'readelf -d failed on td-sh (cannot verify absence of dynamic NEEDED)' >&2; exit 1; }}; \
                     if printf '%s\\n' \"$dout\" | grep -qi 'NEEDED'; then echo 'td-sh has a dynamic NEEDED entry — its runtime closure is not empty' >&2; exit 1; fi"
                ),
            ],
        )
        .env("PATH", &mesboot0_path()),
    );
    // Beyond the shape: actually RUN the static binary so a mis-built ELF with
    // correct headers but a broken entry point / bad static link fails here (the
    // shape checks alone would pass it). The exit-0 stub returns 0 for any argv;
    // when the interpreter lands, `-c 'exit 0'` is a real no-op program that still
    // exits 0, so this smoke check stays valid across the stub→interpreter cutover.
    steps.push(Step::run("{root}", &[bin, "-c", "exit 0"]));

    steps.push(Step::MkDir {
        path: "{out}".into(),
    });
    steps.push(Step::WriteFile {
        path: "{out}/result".into(),
        content: "PASS: td-sh is a statically-linked ELF64 x86-64 executable (ET_EXEC) with no PT_INTERP and no dynamic NEEDED entry, and runs (`-c 'exit 0'` returns 0) — a self-contained /bin/sh with an empty runtime closure, runnable in stage-1 init before the dynamic loader/closure is present\n".into(),
        exec: false,
    });
    steps.push(Step::Require {
        paths: vec!["{out}/result".into()],
        exec: false,
    });

    Recipe::mesboot("td-sh-test", "1.0")
        .native_inputs(&["td-sh", "binutils-x86-64-native"])
        .inputs_owned(mesboot0_inputs(&[]))
        .steps(steps)
        .checks(vec![RecipeCheck::daily(
            r#"
echo ">> recipe-check td-sh-test: build-plan --auto builds td-sh (the target /bin/sh, statically linked by the /td/store target Rust + native GCC/binutils/glibc toolchain), asserts a self-contained static ELF64 x86-64 executable (ET_EXEC, no PT_INTERP, no dynamic NEEDED), and runs it (-c 'exit 0')"
: "${TD_RECIPE_EVAL:=$PWD/target/release/td-recipe-eval}"
exec "$TD_RECIPE_EVAL" check-run td-sh-test daily 1
"#,
        )
        .with_runner(CheckRunner::BuildOnly)])
}

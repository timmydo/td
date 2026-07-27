use crate::ladder::{post_bootstrap_path, POST_BOOTSTRAP_SH};
use crate::types::{CheckRunner, Recipe, RecipeCheck, Step};

// td-netd-test: shape validation of the target-built network daemon. Its
// behavioural proof (bring a link up under QEMU user-net, resolve + reach a host)
// belongs to the operator qemu-boot-net spike OUTSIDE the host-free sandbox;
// instead this asserts — per repo policy that recipes test their output — that the
// shipped binary is the self-contained STATIC ELF the NSS-free design requires. It
// re-proves, with an independent readelf walk, what the producer's `assert_static`
// fail-closes on:
//   1. td-netd is an ELF64 x86-64 *executable* (readelf: class ELF64, machine
//      x86-64, type EXEC) — EXEC (not DYN) is the non-PIE static shape,
//   2. it carries NO PT_INTERP program header — nothing asks a dynamic loader to
//      map it (so glibc's dlopen-based NSS can never load), and
//   3. it has NO dynamic NEEDED entry (a fully static link has no dynamic section
//      at all) — an EMPTY runtime closure, no libc.so to resolve.
pub fn recipe() -> Recipe {
    let bin = "{in:td-netd}/bin/td-netd";
    let readelf = "{in:binutils-x86-64-self}/bin/readelf";
    let mut steps = Vec::new();

    steps.push(
        Step::run(
            "{root}",
            &[
                POST_BOOTSTRAP_SH,
                "-c",
                &format!(
                    "h=$('{readelf}' -h '{bin}' 2>/dev/null) || {{ echo 'readelf -h failed on td-netd' >&2; exit 1; }}; \
                     printf '%s\\n' \"$h\" | grep -i 'class:'   | grep -qi 'ELF64'  || {{ echo 'td-netd is not ELF64' >&2; exit 1; }}; \
                     printf '%s\\n' \"$h\" | grep -i 'machine:' | grep -qi 'x86-64' || {{ echo 'td-netd is not x86-64' >&2; exit 1; }}; \
                     printf '%s\\n' \"$h\" | grep -qE 'Type:[[:space:]]+EXEC([[:space:]]|$)' || {{ echo 'td-netd is not a static ET_EXEC — a DYN/PIE (Type: DYN, whose parenthetical also says Executable) would need runtime relocation' >&2; exit 1; }}"
                ),
            ],
        )
        .env("PATH", &post_bootstrap_path()),
    );
    steps.push(
        Step::run(
            "{root}",
            &[
                POST_BOOTSTRAP_SH,
                "-c",
                &format!(
                    "lout=$('{readelf}' -l '{bin}' 2>/dev/null) || {{ echo 'readelf -l failed on td-netd (cannot verify absence of PT_INTERP)' >&2; exit 1; }}; \
                     if printf '%s\\n' \"$lout\" | grep -qi 'INTERP'; then echo 'td-netd carries a PT_INTERP program header — it is not static and could invoke NSS' >&2; exit 1; fi"
                ),
            ],
        )
        .env("PATH", &post_bootstrap_path()),
    );
    steps.push(
        Step::run(
            "{root}",
            &[
                POST_BOOTSTRAP_SH,
                "-c",
                &format!(
                    "dout=$('{readelf}' -d '{bin}' 2>/dev/null) || {{ echo 'readelf -d failed on td-netd (cannot verify absence of dynamic NEEDED)' >&2; exit 1; }}; \
                     if printf '%s\\n' \"$dout\" | grep -qi 'NEEDED'; then echo 'td-netd has a dynamic NEEDED entry — its runtime closure is not empty' >&2; exit 1; fi"
                ),
            ],
        )
        .env("PATH", &post_bootstrap_path()),
    );

    steps.push(Step::MkDir {
        path: "{out}".into(),
    });
    steps.push(Step::WriteFile {
        path: "{out}/result".into(),
        content: "PASS: td-netd is a statically-linked ELF64 x86-64 executable (ET_EXEC) with no PT_INTERP and no dynamic NEEDED entry — a self-contained NSS-free network daemon with an empty runtime closure\n".into(),
        exec: false,
    });
    steps.push(Step::Require {
        paths: vec!["{out}/result".into()],
        exec: false,
    });

    Recipe::mesboot("td-netd-test", "1.0")
        .native_inputs(&["td-netd", "binutils-x86-64-self", "busybox-x86-64"])
        .steps(steps)
        .checks(vec![RecipeCheck::new(
            r#"
echo ">> recipe-check td-netd-test: build-plan --auto builds td-netd (the network bring-up daemon, statically linked by the /td/store target Rust + native GCC/binutils/glibc toolchain) and asserts a self-contained static ELF64 x86-64 executable (ET_EXEC, no PT_INTERP, no dynamic NEEDED)"
: "${TD_RECIPE_EVAL:=$PWD/target/release/td-recipe-eval}"
exec "$TD_RECIPE_EVAL" check-run td-netd-test 1
"#,
        )
        .with_runner(CheckRunner::BuildOnly)])
}

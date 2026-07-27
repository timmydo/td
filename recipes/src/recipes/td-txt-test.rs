use crate::ladder::{post_bootstrap_path, POST_BOOTSTRAP_SH};
use crate::types::{CheckRunner, Recipe, RecipeCheck, Step};

// td-txt-test: build-shape validation of the target-built text userland.
//
// This asserts — per repo policy that recipes test their output — that the
// shipped td-txt binary is the self-contained STATIC ELF its /bin slots require
// (`grep` and `sed` are reached from scripts that run before, or instead of, the
// dynamic uutils glibc closure). It re-proves, with an independent readelf walk,
// what the producer's `assert_static` fail-closes on:
//   1. td-txt is an ELF64 x86-64 *executable* (readelf: class ELF64, machine
//      x86-64, type EXEC) — EXEC (not DYN) is the non-PIE static shape,
//   2. it carries NO PT_INTERP program header — nothing asks a dynamic loader to
//      map it, so it can run before the dynamic loader/closure is present,
//   3. it has NO dynamic NEEDED entry (a fully static link has no dynamic
//      section at all) — an EMPTY runtime closure, no libc.so to resolve.
// It then RUNS the binary through BOTH dispatch paths a /bin farm uses — a
// `grep -> td-txt` symlink (argv[0]) and the explicit `td-txt sed …` form — so a
// mis-built ELF with correct headers but a broken entry point, or a multicall
// that lost an applet, fails here rather than on the boot oracle.
//
// This is a BUILD-shape + smoke-exec check, not a conformance one. td-txt's
// actual grep/sed behavior is driven by the host-side conformance harness in the
// td-txt crate (tests/conformance.rs over the vendored GNU corpora), which runs
// green in the shared per-change cargo-test gate. The shape build + smoke-exec
// here proves the SEPARATE property that harness cannot: that the target
// toolchain links td-txt into a self-contained static ELF.
pub fn recipe() -> Recipe {
    let bin = "{in:td-txt}/bin/td-txt";
    let readelf = "{in:binutils-x86-64-self}/bin/readelf";
    let mut steps = Vec::new();

    steps.push(
        Step::run(
            "{root}",
            &[
                POST_BOOTSTRAP_SH,
                "-c",
                &format!(
                    "h=$('{readelf}' -h '{bin}' 2>/dev/null) || {{ echo 'readelf -h failed on td-txt' >&2; exit 1; }}; \
                     printf '%s\\n' \"$h\" | grep -i 'class:'   | grep -qi 'ELF64'  || {{ echo 'td-txt is not ELF64' >&2; exit 1; }}; \
                     printf '%s\\n' \"$h\" | grep -i 'machine:' | grep -qi 'x86-64' || {{ echo 'td-txt is not x86-64' >&2; exit 1; }}; \
                     printf '%s\\n' \"$h\" | grep -qE 'Type:[[:space:]]+EXEC([[:space:]]|$)' || {{ echo 'td-txt is not a static ET_EXEC — a DYN/PIE (Type: DYN, whose parenthetical also says Executable) would need runtime relocation' >&2; exit 1; }}"
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
                    "lout=$('{readelf}' -l '{bin}' 2>/dev/null) || {{ echo 'readelf -l failed on td-txt (cannot verify absence of PT_INTERP)' >&2; exit 1; }}; \
                     if printf '%s\\n' \"$lout\" | grep -qi 'INTERP'; then echo 'td-txt carries a PT_INTERP program header — it is not static' >&2; exit 1; fi"
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
                    "dout=$('{readelf}' -d '{bin}' 2>/dev/null) || {{ echo 'readelf -d failed on td-txt (cannot verify absence of dynamic NEEDED)' >&2; exit 1; }}; \
                     if printf '%s\\n' \"$dout\" | grep -qi 'NEEDED'; then echo 'td-txt has a dynamic NEEDED entry — its runtime closure is not empty' >&2; exit 1; fi"
                ),
            ],
        )
        .env("PATH", &post_bootstrap_path()),
    );
    // Beyond the shape: RUN it. `--list` names the applets, a `grep` symlink
    // proves argv[0] dispatch (how the image's /bin farm reaches it), and the
    // explicit `td-txt sed …` form proves the un-symlinked path. Each asserts an
    // OUTPUT, so a binary that exits 0 without doing the work still fails.
    steps.push(
        Step::run(
            "{root}",
            &[
                POST_BOOTSTRAP_SH,
                "-c",
                &format!(
                    "list=$('{bin}' --list) || {{ echo 'td-txt --list failed' >&2; exit 1; }}; \
                     for a in grep sed; do \
                       printf '%s\\n' \"$list\" | grep -qx \"$a\" || {{ echo \"td-txt --list does not name $a\" >&2; exit 1; }}; \
                     done; \
                     mkdir -p {{root}}/smoke/bin; ln -sf '{bin}' {{root}}/smoke/bin/grep; \
                     printf 'alpha\\nbeta\\ngamma\\n' > {{root}}/smoke/in.txt; \
                     got=$({{root}}/smoke/bin/grep -c '^[bg]' {{root}}/smoke/in.txt) || {{ echo 'td-txt grep (argv[0] dispatch) failed' >&2; exit 1; }}; \
                     test \"$got\" = 2 || {{ echo \"td-txt grep counted $got, want 2\" >&2; exit 1; }}; \
                     got=$('{bin}' sed -n '2{{s/e/E/;p}}' {{root}}/smoke/in.txt) || {{ echo 'td-txt sed failed' >&2; exit 1; }}; \
                     test \"$got\" = bEta || {{ echo \"td-txt sed printed $got, want bEta\" >&2; exit 1; }}"
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
        content: "PASS: td-txt is a statically-linked ELF64 x86-64 executable (ET_EXEC) with no PT_INTERP and no dynamic NEEDED entry, lists its grep/sed applets, and runs both — through a /bin-style argv[0] symlink and through the explicit `td-txt <applet>` form — with an empty runtime closure\n".into(),
        exec: false,
    });
    steps.push(Step::Require {
        paths: vec!["{out}/result".into()],
        exec: false,
    });

    Recipe::mesboot("td-txt-test", "1.0")
        .native_inputs(&["td-txt", "binutils-x86-64-self", "busybox-x86-64"])
        .steps(steps)
        .checks(vec![RecipeCheck::daily(
            r#"
echo ">> recipe-check td-txt-test: build-plan --auto builds td-txt (the target grep/sed multicall, statically linked by the /td/store target Rust + native GCC/binutils/glibc toolchain), asserts a self-contained static ELF64 x86-64 executable (ET_EXEC, no PT_INTERP, no dynamic NEEDED), and runs both applets through argv[0] and explicit dispatch"
: "${TD_RECIPE_EVAL:=$PWD/target/release/td-recipe-eval}"
exec "$TD_RECIPE_EVAL" check-run td-txt-test daily 1
"#,
        )
        .with_runner(CheckRunner::BuildOnly)])
}

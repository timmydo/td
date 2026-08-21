use crate::ladder::{post_bootstrap_path, POST_BOOTSTRAP_SH, TD_JAIL_TRANSITION_MARKER};
use crate::types::{CheckRunner, Recipe, RecipeCheck, Step};

pub fn recipe() -> Recipe {
    let bin = "{in:td-jail}/bin/td-jail";
    let readelf = "{in:binutils-x86-64-self}/bin/readelf";
    let mut steps = Vec::new();

    steps.push(
        Step::run(
            "{root}",
            &[
                POST_BOOTSTRAP_SH,
                "-c",
                &format!(
                    "h=$('{readelf}' -h '{bin}' 2>/dev/null) || {{ echo 'readelf -h failed on td-jail' >&2; exit 1; }}; \
                     printf '%s\\n' \"$h\" | grep -i 'class:' | grep -qi 'ELF64' || {{ echo 'td-jail is not ELF64' >&2; exit 1; }}; \
                     printf '%s\\n' \"$h\" | grep -i 'machine:' | grep -qi 'x86-64' || {{ echo 'td-jail is not x86-64' >&2; exit 1; }}; \
                     printf '%s\\n' \"$h\" | grep -qE 'Type:[[:space:]]+EXEC([[:space:]]|$)' || {{ echo 'td-jail is not a static ET_EXEC' >&2; exit 1; }}; \
                     l=$('{readelf}' -l '{bin}' 2>/dev/null) || {{ echo 'readelf -l failed on td-jail' >&2; exit 1; }}; \
                     if printf '%s\\n' \"$l\" | grep -qi INTERP; then echo 'td-jail carries PT_INTERP' >&2; exit 1; fi; \
                     d=$('{readelf}' -d '{bin}' 2>/dev/null) || {{ echo 'readelf -d failed on td-jail' >&2; exit 1; }}; \
                     if printf '%s\\n' \"$d\" | grep -qi NEEDED; then echo 'td-jail carries a dynamic dependency' >&2; exit 1; fi"
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
                    "o=$('{bin}' --probe-transition 2>&1) || {{ echo \"td-jail namespace transition probe failed: $o\" >&2; exit 1; }}; \
                     [ \"$o\" = '{TD_JAIL_TRANSITION_MARKER} pid=1' ] || {{ echo \"td-jail transition returned the wrong proof: $o\" >&2; exit 1; }}; \
                     '{bin}' >/dev/null 2>&1 && {{ echo 'td-jail launched without a complete confinement path' >&2; exit 1; }}; :"
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
        content: format!(
            "PASS: td-jail is a static ELF64 x86-64 executable; the build-host policy permits fresh user, mount, PID, UTS and network namespaces, stage 1 observes stage 2 as PID 1 before emitting {TD_JAIL_TRANSITION_MARKER}, identity maps read back exactly, non-root stage 2 has no effective capabilities after exec, and application launch remains disabled; system-x86-64's QEMU oracle supplies the authoritative target-kernel proof\n"
        ),
        exec: false,
    });
    steps.push(Step::Require {
        paths: vec!["{out}/result".into()],
        exec: false,
    });

    Recipe::mesboot("td-jail-test", "1.0")
        .native_inputs(&["td-jail", "binutils-x86-64-self", "busybox-x86-64"])
        .steps(steps)
        .checks(vec![RecipeCheck::new(
            r#"
echo ">> recipe-check td-jail-test: build-plan --auto builds the static target td-jail, smoke-tests the build host's unprivileged namespace policy and stage-1/stage-2 PID-1 handoff, checks exact identity maps and post-exec capability removal, and keeps application launch disabled; the system QEMU oracle proves the target kernel"
: "${TD_RECIPE_EVAL:=$PWD/target/release/td-recipe-eval}"
exec "$TD_RECIPE_EVAL" check-run td-jail-test 1
"#,
        )
        .with_runner(CheckRunner::BuildOnly)])
}

use crate::ladder::{
    post_bootstrap_path, POST_BOOTSTRAP_SH, TD_JAIL_FIXTURE_BOOT_MARKER,
    TD_JAIL_TRANSITION_MARKER,
};
use crate::types::{CheckRunner, Recipe, RecipeCheck, Step};

pub fn recipe() -> Recipe {
    let bin = "{in:td-jail}/bin/td-jail";
    let readelf = "{in:binutils-x86-64-self}/bin/readelf";
    let probe = "{in:td-jail-seccomp-probe}/bin/td-jail-seccomp-probe";
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
                    "exec 9</proc/self/status; \
                     o=$(TD_JAIL_TEST_LEAK_FD=1 '{bin}' --probe-transition 2>&1) || {{ exec 9<&-; echo \"td-jail namespace transition probe failed: $o\" >&2; exit 1; }}; \
                     exec 9<&-; \
                     [ \"$o\" = '{TD_JAIL_TRANSITION_MARKER} pid=1' ] || {{ echo \"td-jail transition returned the wrong proof: $o\" >&2; exit 1; }}; \
                     '{bin}' >/dev/null 2>&1 && {{ echo 'td-jail accepted a bare internal invocation' >&2; exit 1; }}; :"
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
                    "'{bin}' --internal-write-seccomp-filter >'{{root}}/filter.bpf' || {{ echo 'td-jail could not export its compiled seccomp filter' >&2; exit 1; }}; \
                     '{probe}' --allow-inherited-confinement '{{root}}/filter.bpf'"
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
            "PASS: td-jail is a static ELF64 x86-64 executable; the build-host policy permits the complete namespace transition, stage 1 closes inherited descriptors, brings up and reads back isolated loopback, and installs an exact CAP_SYS_ADMIN exec bridge with an empty bounding set; stage 2 enters a read-back immutable tmpfs root with fresh proc/dev/devpts/shm/tmp/var-tmp and no old root, clears every capability, sets and reads back no-new-privileges, installs and reads back the compiled seccomp filter, naturally reaps filtered descendants as PID 1, and exercises bounded namespace-wide TERM and KILL survivor cleanup; the td-GCC-built non-shipped probe checks real filter errno and kill behavior, and a bare td-jail invocation cannot enter its internal interface; the host smoke leg may skip behavior under an inherited filter, while system-x86-64's QEMU oracle supplies the authoritative target-kernel transition through {TD_JAIL_TRANSITION_MARKER} and the installed-application launch proof, including a bounded loopback datagram plus writable and recursively read-only filesystem-grant oracles, through {TD_JAIL_FIXTURE_BOOT_MARKER}\n"
        ),
        exec: false,
    });
    steps.push(Step::Require {
        paths: vec!["{out}/result".into()],
        exec: false,
    });
    Recipe::mesboot("td-jail-test", "1.0")
        .native_inputs(&[
            "td-jail",
            "td-jail-seccomp-probe",
            "binutils-x86-64-self",
            "busybox-x86-64",
        ])
        .steps(steps)
        .checks(vec![RecipeCheck::new(
            r#"
echo ">> recipe-check td-jail-test: build-plan --auto builds the static target td-jail and a non-shipped td-GCC seccomp probe, smoke-tests namespace/mount/capability transition, installs and reads back no-new-privileges plus the compiled filter, attempts real errno/kill behavior only when the host has no inherited seccomp filter, verifies filtered PID-1 orphan reaping plus bounded TERM/KILL survivor cleanup, and refuses bare internal invocation; the system QEMU oracle proves installed launch on the target kernel"
: "${TD_RECIPE_EVAL:=$PWD/target/release/td-recipe-eval}"
exec "$TD_RECIPE_EVAL" check-run td-jail-test 1
"#,
        )
        .with_runner(CheckRunner::BuildOnly)])
}

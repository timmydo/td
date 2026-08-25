use crate::ladder::{
    post_bootstrap_path, POST_BOOTSTRAP_SH, TD_JAIL_FIXTURE_BOOT_MARKER,
    TD_JAIL_TRANSITION_MARKER,
};
use crate::types::{CheckRunner, Recipe, RecipeCheck, Step};
use td_engine::application::ApplicationProvenance;
use td_engine::application_spec::ApplicationSpec;
use td_engine::launcher::{ApplicationRegistry, LauncherTable};

const HOST_PACKAGE: &str =
    "/td/store/00000000000000000000000000000000-td-jail-fixture-0.1";
const HOST_RUNTIME: &str = "/td/store/00000000000000000000000000000000-empty-runtime-1";
const HOST_DEGRADATION_CGROUP: &str =
    "TD-JAIL-HOST-DEGRADATION aggregate-memory-and-task-caps=unenforced reason=no-delegated-cgroup";
const HOST_DEGRADATION_WAYLAND: &str =
    "TD-JAIL-HOST-DEGRADATION wayland-global-filter=unenforced reason=direct-host-socket";

struct HostFixture {
    manifest: String,
    spec: String,
    registry: String,
    launcher: String,
}

fn host_fixture() -> Result<HostFixture, String> {
    let fixture = super::td_jail_fixture::recipe();
    let name = fixture.name;
    let version = fixture.version;
    let declaration = fixture
        .application
        .ok_or_else(|| "td-jail fixture has no application declaration".to_string())?;
    let permissions = fixture
        .application_permissions
        .ok_or_else(|| "td-jail fixture has no permission policy".to_string())?;
    let launcher = fixture
        .application_launcher
        .ok_or_else(|| "td-jail fixture has no launcher declaration".to_string())?;
    let manifest = declaration.manifest(&name, &version, ApplicationProvenance::Source)?;
    let spec = ApplicationSpec::compile(&manifest, HOST_RUNTIME, permissions)?.to_keyfile();
    let registry =
        ApplicationRegistry::new(vec![(name.clone(), HOST_PACKAGE.to_string())])?.to_tsv();
    let launcher = launcher.bind(&name)?;
    let launcher = LauncherTable::new(vec![launcher])?.to_tsv();
    Ok(HostFixture {
        manifest: manifest.to_keyfile(),
        spec,
        registry,
        launcher,
    })
}

pub fn recipe() -> Recipe {
    let Ok(host) = host_fixture() else {
        return Recipe::mesboot("td-jail-test", "1.0").steps(vec![Step::Require {
            paths: vec!["{out}/invalid-host-fixture".into()],
            exec: false,
        }]);
    };
    let bin = "{in:td-jail}/bin/td-jail";
    let busd = "{in:td-busd}/bin/td-busd";
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

    for path in [
        "/home/td-jail-host/packages/00000000000000000000000000000000-td-jail-fixture-0.1/files/bin",
        "/home/td-jail-host/packages/00000000000000000000000000000000-empty-runtime-1/files",
        "/home/td-jail-host/etc",
        "/home/td-jail-host",
        "/home/td-jail-host/runtime",
    ] {
        steps.push(Step::MkDir { path: path.into() });
    }
    steps.push(Step::CopyFiles {
        files: vec!["{in:td-compositor}/bin/td-compositor".into()],
        dest:
            "/home/td-jail-host/packages/00000000000000000000000000000000-td-jail-fixture-0.1/files/bin"
                .into(),
    });
    for (path, content) in [
        (
            "/home/td-jail-host/packages/00000000000000000000000000000000-td-jail-fixture-0.1/manifest",
            host.manifest,
        ),
        (
            "/home/td-jail-host/packages/00000000000000000000000000000000-td-jail-fixture-0.1/spec",
            host.spec,
        ),
        ("/home/td-jail-host/etc/td-applications.tsv", host.registry),
        ("/home/td-jail-host/etc/td-launcher.tsv", host.launcher),
    ] {
        steps.push(Step::WriteFile {
            path: path.into(),
            content,
            exec: false,
        });
    }
    steps.push(Step::WriteFile {
        path: "/home/td-jail-host/etc/td-app-host.conf".into(),
        content: "format=1\npackage-root=/home/td-jail-host/packages\nstate-root=/home/td-jail-host/.td/app\nregistry=/home/td-jail-host/etc/td-applications.tsv\nlauncher-table=/home/td-jail-host/etc/td-launcher.tsv\ncgroup-root=none\n".into(),
        exec: false,
    });
    steps.push(
        Step::run(
            "{root}",
            &[
                POST_BOOTSTRAP_SH,
                "-c",
                &format!(
                    "chmod 0700 /home/td-jail-host/runtime || exit 1; \
                     mkdir -p /mnt/td-jail-fixture-pictures /var || exit 1; \
                     printf '%s\\n' td-jail-file-grant-v1 >/var/td-jail-fixture-file || exit 1; \
                     '{busd}' run --socket /home/td-jail-host/runtime/bus >/home/td-jail-host/bus.log 2>&1 & b=$!; \
                     '{busd}' run --socket /home/td-jail-host/runtime/wayland-test >/home/td-jail-host/wayland.log 2>&1 & w=$!; \
                     n=0; while {{ [ ! -S /home/td-jail-host/runtime/bus ] || [ ! -S /home/td-jail-host/runtime/wayland-test ]; }} && [ \"$n\" -lt 10 ]; do n=$((n+1)); sleep 1; done; \
                     if [ ! -S /home/td-jail-host/runtime/bus ] || [ ! -S /home/td-jail-host/runtime/wayland-test ]; then kill \"$b\" \"$w\" 2>/dev/null || :; wait \"$b\" 2>/dev/null || :; wait \"$w\" 2>/dev/null || :; echo 'td-jail host authorities did not become ready' >&2; exit 1; fi; \
                     o=$(XDG_RUNTIME_DIR=/home/td-jail-host/runtime WAYLAND_DISPLAY=wayland-test '{bin}' --host /home/td-jail-host/etc/td-app-host.conf {} selftest 2>&1); s=$?; \
                     kill \"$b\" \"$w\" 2>/dev/null || :; wait \"$b\" 2>/dev/null || :; wait \"$w\" 2>/dev/null || :; \
                     [ \"$s\" -eq 0 ] || {{ echo \"td-jail host fixture failed: $o\" >&2; exit 1; }}; \
                     e='{}\n{}'; \
                     [ \"$o\" = \"$e\" ] || {{ echo \"td-jail host degradation report changed: $o\" >&2; exit 1; }}",
                    crate::ladder::TD_JAIL_FIXTURE_NAME,
                    HOST_DEGRADATION_CGROUP,
                    HOST_DEGRADATION_WAYLAND,
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
            "PASS: td-jail is a static ELF64 x86-64 executable; the build-host policy permits the complete namespace transition, stage 1 closes inherited descriptors, brings up and reads back isolated loopback, and installs an exact CAP_SYS_ADMIN exec bridge with an empty bounding set; stage 2 enters a read-back immutable tmpfs root with fresh proc/dev/devpts/shm/tmp/var-tmp and no old root, clears every capability, sets and reads back no-new-privileges, installs and reads back the compiled seccomp filter, naturally reaps filtered descendants as PID 1, and exercises bounded namespace-wide TERM and KILL survivor cleanup; the td-GCC-built non-shipped probe checks real filter errno and kill behavior, and a bare td-jail invocation cannot enter its internal interface; explicit host mode launches the ordinary fixture identity and spec from a materialized prefix through the real td-busd registration path, binds caller-owned host session sockets, and emits the exact cgroup and Wayland-filter degradation report; the host smoke leg may skip behavior under an inherited filter, while system-x86-64's QEMU oracle supplies the authoritative target-kernel transition through {TD_JAIL_TRANSITION_MARKER} and the installed-application launch proof, including a bounded loopback datagram plus writable and recursively read-only filesystem-grant oracles, through {TD_JAIL_FIXTURE_BOOT_MARKER}\n"
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
            "td-busd",
            "td-compositor",
            "td-jail-seccomp-probe",
            "binutils-x86-64-self",
            "busybox-x86-64",
        ])
        .steps(steps)
        .checks(vec![RecipeCheck::new(
            r#"
echo ">> recipe-check td-jail-test: build-plan --auto builds the static target td-jail and a non-shipped td-GCC seccomp probe, launches the ordinary fixture from a host prefix with exact degradation diagnostics, smoke-tests namespace/mount/capability transition, installs and reads back no-new-privileges plus the compiled filter, attempts real errno/kill behavior only when the host has no inherited seccomp filter, verifies filtered PID-1 orphan reaping plus bounded TERM/KILL survivor cleanup, and refuses bare internal invocation; the system QEMU oracle proves installed launch on the target kernel"
: "${TD_RECIPE_EVAL:=$PWD/target/release/td-recipe-eval}"
exec "$TD_RECIPE_EVAL" check-run td-jail-test 1
"#,
        )
        .with_runner(CheckRunner::BuildOnly)])
}

#[cfg(test)]
mod tests {
    use super::{HOST_DEGRADATION_CGROUP, HOST_DEGRADATION_WAYLAND};

    #[test]
    fn host_degradation_contract_matches_td_jail() {
        let transition = include_str!("../../../td-jail/src/transition.rs");
        for diagnostic in [HOST_DEGRADATION_CGROUP, HOST_DEGRADATION_WAYLAND] {
            assert!(transition.contains(diagnostic));
        }
    }
}

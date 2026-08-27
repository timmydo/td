use crate::ladder::{
    post_bootstrap_path, POST_BOOTSTRAP_SH, TD_JAIL_FIXTURE_BOOT_MARKER, TD_JAIL_TRANSITION_MARKER,
};
use crate::types::{CheckRunner, Recipe, RecipeCheck, Step};
use td_engine::application::ApplicationProvenance;
use td_engine::application_spec::ApplicationSpec;
use td_engine::launcher::{ApplicationRegistry, LauncherTable};

const HOST_PACKAGE: &str = "/td/store/00000000000000000000000000000000-td-jail-fixture-0.1";
const HOST_RUNTIME: &str = "/td/store/00000000000000000000000000000000-empty-runtime-1";
const HOST_FIREFOX_PACKAGE: &str =
    "/td/store/00000000000000000000000000000000-firefox-154.0";
const HOST_FIREFOX_RUNTIME: &str =
    "/td/store/00000000000000000000000000000000-freedesktop-platform-25-08-25.08";
const HOST_FIREFOX_MARKER: &str = "TD-FIREFOX-JAIL-SMOKE-OK";
const HOST_DEGRADATION_CGROUP: &str =
    "TD-JAIL-HOST-DEGRADATION aggregate-memory-task-and-cpu-caps=unenforced reason=no-delegated-cgroup";
const HOST_DEGRADATION_WAYLAND: &str =
    "TD-JAIL-HOST-DEGRADATION wayland-global-filter=unenforced reason=direct-host-socket";

struct HostFixture {
    manifest: String,
    spec: String,
    shared_spec: String,
    firefox_manifest: String,
    firefox_spec: String,
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
    let shared_permissions = permissions
        .clone()
        .with_network()
        .map_err(|error| format!("extend td-jail host fixture network policy: {error}"))?;
    let launcher = fixture
        .application_launcher
        .ok_or_else(|| "td-jail fixture has no launcher declaration".to_string())?;
    let manifest = declaration.manifest(&name, &version, ApplicationProvenance::Source)?;
    let spec = ApplicationSpec::compile(&manifest, HOST_RUNTIME, permissions)?.to_keyfile();
    let shared_spec =
        ApplicationSpec::compile(&manifest, HOST_RUNTIME, shared_permissions)?.to_keyfile();
    let fixture_launcher = launcher.bind(&name)?;

    let firefox = super::firefox::recipe();
    let firefox_name = firefox.name;
    let firefox_version = firefox.version;
    let firefox_declaration = firefox
        .application
        .ok_or_else(|| "Firefox has no application declaration".to_string())?;
    let firefox_permissions = firefox
        .application_permissions
        .ok_or_else(|| "Firefox has no permission policy".to_string())?;
    let firefox_launcher = firefox
        .application_launcher
        .ok_or_else(|| "Firefox has no launcher declaration".to_string())?;
    let firefox_manifest = firefox_declaration.manifest(
        &firefox_name,
        &firefox_version,
        ApplicationProvenance::Foreign,
    )?;
    let firefox_spec = ApplicationSpec::compile(
        &firefox_manifest,
        HOST_FIREFOX_RUNTIME,
        firefox_permissions,
    )?
    .to_keyfile();
    let registry = ApplicationRegistry::new(vec![
        (firefox_name.clone(), HOST_FIREFOX_PACKAGE.to_string()),
        (name.clone(), HOST_PACKAGE.to_string()),
    ])?
    .to_tsv();
    let launcher = LauncherTable::new(vec![
        firefox_launcher.bind(&firefox_name)?,
        fixture_launcher,
    ])?
    .to_tsv();
    Ok(HostFixture {
        manifest: manifest.to_keyfile(),
        spec,
        shared_spec,
        firefox_manifest: firefox_manifest.to_keyfile(),
        firefox_spec,
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
        "/home/td-jail-host/packages/00000000000000000000000000000000-empty-runtime-1/files/etc",
        "/home/td-jail-host/packages/00000000000000000000000000000000-empty-runtime-1/files/etc/fonts",
        "/home/td-jail-host/packages/00000000000000000000000000000000-firefox-154.0/files/bin",
        "/home/td-jail-host/packages/00000000000000000000000000000000-firefox-154.0/files/lib/firefox",
        "/home/td-jail-host/packages/00000000000000000000000000000000-freedesktop-platform-25-08-25.08/files/bin",
        "/home/td-jail-host/packages/00000000000000000000000000000000-freedesktop-platform-25-08-25.08/files/lib",
        "/home/td-jail-host/packages/00000000000000000000000000000000-freedesktop-platform-25-08-25.08/files/lib64",
        "/home/td-jail-host/packages/00000000000000000000000000000000-freedesktop-platform-25-08-25.08/files/sbin",
        "/home/td-jail-host/packages/00000000000000000000000000000000-freedesktop-platform-25-08-25.08/files/etc",
        "/home/td-jail-host/Downloads",
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
    steps.push(Step::CopyFiles {
        files: vec!["{in:busybox-x86-64}/bin/busybox".into()],
        dest: "/home/td-jail-host/packages/00000000000000000000000000000000-freedesktop-platform-25-08-25.08/files/bin".into(),
    });
    steps.push(Step::Symlink {
        target: "busybox".into(),
        link: "/home/td-jail-host/packages/00000000000000000000000000000000-freedesktop-platform-25-08-25.08/files/bin/sh".into(),
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
        (
            "/home/td-jail-host/packages/00000000000000000000000000000000-td-jail-fixture-0.1/shared-spec",
            host.shared_spec,
        ),
        (
            "/home/td-jail-host/packages/00000000000000000000000000000000-firefox-154.0/manifest",
            host.firefox_manifest,
        ),
        (
            "/home/td-jail-host/packages/00000000000000000000000000000000-firefox-154.0/spec",
            host.firefox_spec,
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
        path: "/home/td-jail-host/packages/00000000000000000000000000000000-firefox-154.0/files/bin/firefox".into(),
        content: format!(
            "#!/bin/sh\nset -eu\nTMPDIR=$XDG_CACHE_HOME/tmp\nexport TMPDIR\n[ \"$LD_LIBRARY_PATH\" = /app/lib:/app/lib/firefox ] || exit 81\nfor path in /bin /lib /lib64 /sbin; do [ -L \"$path\" ] || exit 82; done\n[ -x /bin/sh ] || exit 83\n[ -d \"$TMPDIR\" ] || exit 84\n[ -w \"$TMPDIR\" ] || exit 85\nprintf '%s\\n' '{HOST_FIREFOX_MARKER}' >\"$XDG_RUNTIME_DIR/td-app/dynamic-ready\" || exit 86\n"
        ),
        exec: true,
    });
    steps.push(Step::WriteFile {
        path: "/home/td-jail-host/etc/td-app-host.conf".into(),
        content: "format=2\npackage-root=/home/td-jail-host/packages\nstate-root=/home/td-jail-host/.td/app\nregistry=/home/td-jail-host/etc/td-applications.tsv\nlauncher-table=/home/td-jail-host/etc/td-launcher.tsv\nca-bundle={in:ca-certificates}/share/ca-certificates/ca-bundle.crt\nresolv-conf=/home/td-jail-host/etc/resolv.conf\ncgroup-root=none\n".into(),
        exec: false,
    });
    steps.push(Step::WriteFile {
        path: "/home/td-jail-host/etc/resolv.conf".into(),
        content: "nameserver 127.0.0.1\n".into(),
        exec: false,
    });
    steps.push(Step::WriteFile {
        path: "/home/td-jail-host/packages/00000000000000000000000000000000-empty-runtime-1/files/etc/ld.so.conf".into(),
        content: "include /etc/ld.so.conf.d/*.conf\n".into(),
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
                     c=0; cp /home/td-jail-host/packages/00000000000000000000000000000000-td-jail-fixture-0.1/shared-spec /home/td-jail-host/packages/00000000000000000000000000000000-td-jail-fixture-0.1/spec || c=$?; \
                     p=$(XDG_RUNTIME_DIR=/home/td-jail-host/runtime WAYLAND_DISPLAY=wayland-test '{bin}' --host /home/td-jail-host/etc/td-app-host.conf {} selftest 2>&1); t=$?; \
                     f=$(XDG_RUNTIME_DIR=/home/td-jail-host/runtime WAYLAND_DISPLAY=wayland-test '{bin}' --host /home/td-jail-host/etc/td-app-host.conf firefox 2>&1); u=$?; \
                     kill \"$b\" \"$w\" 2>/dev/null || :; wait \"$b\" 2>/dev/null || :; wait \"$w\" 2>/dev/null || :; \
                     [ \"$s\" -eq 0 ] || {{ echo \"td-jail host fixture failed: $o\" >&2; exit 1; }}; \
                     [ \"$c\" -eq 0 ] || {{ echo 'td-jail host shared spec could not replace the isolated spec' >&2; exit 1; }}; \
                     [ \"$t\" -eq 0 ] || {{ echo \"td-jail host shared-network fixture failed: $p\" >&2; exit 1; }}; \
                     [ \"$u\" -eq 0 ] || {{ echo \"td-jail host Firefox-spec smoke failed with status $u: $f\" >&2; exit 1; }}; \
                     e='{}\n{}'; \
                     [ \"$o\" = \"$e\" ] || {{ echo \"td-jail host isolated degradation report changed: $o\" >&2; exit 1; }}; \
                     [ \"$p\" = \"$e\" ] || {{ echo \"td-jail host shared-network degradation report changed: $p\" >&2; exit 1; }}; \
                     [ \"$f\" = \"$e\" ] || {{ echo \"td-jail host Firefox-spec degradation report changed: $f\" >&2; exit 1; }}; \
                     [ \"$(cat /home/td-jail-host/runtime/td-app/firefox/dynamic-ready)\" = '{HOST_FIREFOX_MARKER}' ] || {{ echo 'td-jail host Firefox-spec marker is absent or wrong' >&2; exit 1; }}",
                    crate::ladder::TD_JAIL_FIXTURE_NAME,
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
            "PASS: td-jail is a static ELF64 x86-64 executable; the build-host policy permits the complete namespace transition, the application bootstrap executes its parent-death and terminal-containment setup before authority resolution, stage 1 closes inherited descriptors, preserves a policy-declared shared network or brings up and reads back isolated loopback, builds a selective immutable /etc with per-application identity, pinned CA trust, and nonempty file/directory runtime configuration binds, and installs an exact CAP_SYS_ADMIN exec bridge with an empty bounding set; stage 2 enters a read-back immutable tmpfs root with fresh proc/dev/devpts/shm/tmp/var-tmp and no old root, derives the exact /etc roster from /usr/etc, verifies its runtime nested mounts and conditional resolver bind, clears every capability, sets and reads back no-new-privileges, installs and reads back the compiled seccomp filter, naturally reaps filtered descendants as PID 1, and exercises bounded namespace-wide TERM and KILL survivor cleanup; the td-GCC-built non-shipped probe checks real filter errno and kill behavior, and a bare td-jail invocation cannot enter its internal interface; explicit host mode launches the ordinary fixture identity with both its isolated spec and a fixture-derived shared-network spec plus a source-built surrogate under the real Firefox spec from a materialized prefix through the real td-busd registration path, binds caller-owned host session sockets, verifies Firefox's exact loader path, immutable runtime aliases and private cache tmp, and emits the exact cgroup and Wayland-filter degradation report for all three; the host smoke leg may skip behavior under an inherited filter, while system-x86-64's QEMU oracle supplies the authoritative target-kernel isolated transition through {TD_JAIL_TRANSITION_MARKER} and the installed-application launch proof, including a bounded loopback datagram plus writable and recursively read-only filesystem-grant oracles, through {TD_JAIL_FIXTURE_BOOT_MARKER}\n"
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
            "ca-certificates",
            "binutils-x86-64-self",
            "busybox-x86-64",
        ])
        .steps(steps)
        .checks(vec![RecipeCheck::new(
            r#"
echo ">> recipe-check td-jail-test: build-plan --auto builds the static target td-jail and a non-shipped td-GCC seccomp probe, launches the ordinary fixture with isolated and shared network policy plus a source-built surrogate under the exact Firefox spec through the parent-death and terminal-containment bootstrap from a host prefix with exact degradation diagnostics and fixture-owned CA/resolver inputs, verifies the Firefox loader path, runtime aliases and cache tmp, smoke-tests selective immutable /etc plus namespace/mount/capability transition, installs and reads back no-new-privileges plus the compiled filter, attempts real errno/kill behavior only when the host has no inherited seccomp filter, verifies filtered PID-1 orphan reaping plus bounded TERM/KILL survivor cleanup, and refuses bare internal invocation; the system QEMU oracle proves installed launch on the target kernel"
: "${TD_RECIPE_EVAL:=$PWD/target/release/td-recipe-eval}"
exec "$TD_RECIPE_EVAL" check-run td-jail-test 1
"#,
        )
        .with_runner(CheckRunner::BuildOnly)])
}

#[cfg(test)]
mod tests {
    use super::{
        host_fixture, HOST_DEGRADATION_CGROUP, HOST_DEGRADATION_WAYLAND,
        HOST_FIREFOX_MARKER, HOST_FIREFOX_RUNTIME,
    };

    #[test]
    fn host_fixture_exercises_declared_shared_network() {
        let host = host_fixture().expect("host fixture");
        assert!(!host.spec.contains("shared=network"));
        assert!(host
            .shared_spec
            .contains("[Context]\nshared=network\nsockets=wayland\n"));
    }

    #[test]
    fn host_fixture_uses_the_exact_firefox_runtime_contract() {
        let host = host_fixture().expect("host fixture");
        assert!(host.firefox_manifest.contains("name=firefox\n"));
        assert!(host.firefox_manifest.contains("provenance=foreign\n"));
        assert!(host
            .firefox_spec
            .contains(&format!("runtime={HOST_FIREFOX_RUNTIME}\n")));
        assert!(host
            .firefox_spec
            .contains("LD_LIBRARY_PATH=/app/lib:/app/lib/firefox\n"));
        assert!(host.firefox_spec.contains("shared=network\n"));
        assert!(host
            .registry
            .starts_with("firefox\t/td/store/00000000000000000000000000000000-firefox-154.0\n"));

        let recipe = super::recipe();
        assert!(recipe.steps.iter().flatten().any(|step| matches!(
            step,
            crate::types::Step::WriteFile { path, content, exec: true }
                if path.ends_with("firefox-154.0/files/bin/firefox")
                    && content.starts_with("#!/bin/sh\n")
                    && content.contains("[ \"$LD_LIBRARY_PATH\" = /app/lib:/app/lib/firefox ]")
                    && content.contains(HOST_FIREFOX_MARKER)
        )));
        assert!(recipe.steps.iter().flatten().any(|step| matches!(
            step,
            crate::types::Step::Symlink { target, link }
                if target == "busybox" && link.ends_with("/files/bin/sh")
        )));
    }

    #[test]
    fn host_degradation_contract_matches_td_jail() {
        let transition = include_str!("../../../td-jail/src/transition.rs");
        for diagnostic in [HOST_DEGRADATION_CGROUP, HOST_DEGRADATION_WAYLAND] {
            assert!(transition.contains(diagnostic));
        }
    }

    #[test]
    fn host_config_names_fixture_owned_etc_inputs() {
        let recipe = super::recipe();
        assert!(recipe
            .native_inputs
            .as_deref()
            .unwrap_or_default()
            .contains(&"ca-certificates".to_string()));
        assert!(recipe.steps.iter().flatten().any(|step| matches!(
            step,
            crate::types::Step::WriteFile { path, content, exec: false }
                if path == "/home/td-jail-host/etc/td-app-host.conf"
                    && content.starts_with("format=2\n")
                    && content.contains("ca-bundle={in:ca-certificates}/share/ca-certificates/ca-bundle.crt\n")
                    && content.contains("resolv-conf=/home/td-jail-host/etc/resolv.conf\n")
        )));
        assert!(recipe.steps.iter().flatten().any(|step| matches!(
            step,
            crate::types::Step::MkDir { path }
                if path.ends_with("empty-runtime-1/files/etc/fonts")
        )));
        assert!(recipe.steps.iter().flatten().any(|step| matches!(
            step,
            crate::types::Step::WriteFile { path, content, exec: false }
                if path.ends_with("empty-runtime-1/files/etc/ld.so.conf")
                    && content == "include /etc/ld.so.conf.d/*.conf\n"
        )));
    }
}

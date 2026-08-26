use crate::ladder::{split_target_debug, target_rustc};
use crate::types::{Recipe, Step};

const MAIN_RS: &str = include_str!("../../../td-jail/src/main.rs");
const MODULES: &[(&str, &str)] = &[
    (
        "authority",
        include_str!("../../../td-jail/src/authority.rs"),
    ),
    ("bus", include_str!("../../../td-jail/src/bus.rs")),
    ("cgroup", include_str!("../../../td-jail/src/cgroup.rs")),
    (
        "permissions",
        include_str!("../../../engine/src/permissions.rs"),
    ),
    ("seccomp", include_str!("../../../td-jail/src/seccomp.rs")),
    ("sys", include_str!("../../../td-jail/src/sys.rs")),
    (
        "transition",
        include_str!("../../../td-jail/src/transition.rs"),
    ),
];

#[cfg(test)]
pub(crate) fn source(name: &str) -> Option<&'static str> {
    if name == "main" {
        return Some(MAIN_RS);
    }
    MODULES
        .iter()
        .find(|(module, _)| *module == name)
        .map(|(_, source)| *source)
}

#[cfg(test)]
fn declared_modules() -> Vec<&'static str> {
    let mut modules = Vec::new();
    for line in MAIN_RS.lines() {
        if let Some(module) = line
            .trim()
            .strip_prefix("mod ")
            .and_then(|rest| rest.strip_suffix(';'))
        {
            modules.push(module);
        }
    }
    modules
}

pub fn recipe() -> Recipe {
    let rustc = "{in:rust-toolchain}/bin/rustc";
    let gcc = "{in:gcc-x86-64-self}/stage/td/store/gcc-14.3.0-x86_64-self/bin/gcc";
    let gccbin = "{in:gcc-x86-64-self}/stage/td/store/gcc-14.3.0-x86_64-self/bin";
    let bbin = "{in:binutils-x86-64-self}/bin";
    let glib = "{in:glibc-x86-64}/stage/td/store/glibc-2.41-x86_64/lib";
    let objcopy = "{in:binutils-x86-64-self}/bin/objcopy";
    let ranlib = "{in:binutils-x86-64-self}/bin/ranlib";
    let libgcc_a = "{in:gcc-x86-64-self}/stage/td/store/gcc-14.3.0-x86_64-self/lib/gcc/x86_64-pc-linux-gnu/14.3.0/libgcc.a";

    let linker = format!("-Clinker={gcc}");
    let lib_b = format!("-Clink-arg=-B{glib}");
    let bin_b = format!("-Clink-arg=-B{bbin}");
    let path = format!("{bbin}:{gccbin}");

    let mut steps = vec![
        Step::MkDir {
            path: "{out}/bin".into(),
        },
        Step::WriteFile {
            path: "{src}/main.rs".into(),
            content: MAIN_RS.into(),
            exec: false,
        },
    ];
    for (name, source) in MODULES {
        steps.push(Step::WriteFile {
            path: format!("{{src}}/{name}.rs"),
            content: (*source).into(),
            exec: false,
        });
    }
    steps.push(Step::MkDir {
        path: "{root}/eh".into(),
    });
    steps.push(
        Step::run("{root}", &[objcopy, libgcc_a, "{root}/eh/libgcc_eh.a"]).env("PATH", &path),
    );
    steps.push(Step::run("{root}", &[ranlib, "{root}/eh/libgcc_eh.a"]).env("PATH", &path));
    steps.push(
        target_rustc(
            "{src}",
            rustc,
            &[
                "--edition",
                "2021",
                "--cfg",
                "feature=\"target-recipe\"",
                "-C",
                "opt-level=s",
                "--target",
                "x86_64-unknown-linux-gnu",
                "-C",
                "target-feature=+crt-static",
                "-C",
                "relocation-model=static",
                "-C",
                "panic=abort",
                &linker,
                "-L",
                glib,
                &lib_b,
                &bin_b,
                "-Clink-arg=-L{root}/eh",
                "-Clink-arg=-static-libgcc",
                "-o",
                "{out}/bin/td-jail",
                "{src}/main.rs",
            ],
        )
        .env("PATH", &path)
        .env("SOURCE_DATE_EPOCH", "1"),
    );
    steps.push(Step::Require {
        paths: vec!["{out}/bin/td-jail".into()],
        exec: true,
    });
    steps.push(split_target_debug("{out}"));
    steps.push(Step::assert_static(&["{out}/bin/td-jail"]));

    Recipe::mesboot("td-jail", "0.1")
        .native_inputs(&[
            "rust-toolchain",
            "gcc-x86-64-self",
            "binutils-x86-64-self",
            "glibc-x86-64",
        ])
        .steps(steps)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use td_engine::application::{ApplicationDeclaration, ApplicationProvenance};
    use td_engine::application_spec::ApplicationSpec;
    use td_engine::launcher::ApplicationRegistry;
    use td_engine::permissions::{FilesystemAccess, PermissionPolicy, PermissionSocket};

    #[allow(dead_code)]
    mod target_authority {
        include!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../td-jail/src/authority.rs"
        ));
    }

    #[test]
    fn embedded_rust_does_not_contain_live_recipe_templates() {
        for (name, source) in
            std::iter::once(("main", MAIN_RS)).chain(MODULES.iter().copied())
        {
            for template in [
                "{root}",
                "{src}",
                "{out}",
                "{tools}",
                "{jobs}",
                "{in:",
                "{payload:",
            ] {
                assert!(
                    !source.contains(template),
                    "{name}.rs contains recipe template {template}"
                );
            }
        }
    }

    #[test]
    fn recipe_stages_every_declared_module() {
        let mut declared = declared_modules();
        let mut staged: Vec<&str> = MODULES.iter().map(|(name, _)| *name).collect();
        declared.sort_unstable();
        staged.sort_unstable();
        assert_eq!(declared, staged);
    }

    #[test]
    fn recipe_embeds_the_transition_marker() {
        let transition = source("transition").expect("transition source");
        assert!(transition.contains(crate::ladder::TD_JAIL_TRANSITION_MARKER));
    }

    #[test]
    fn cgroup_paths_match_the_distribution_hierarchy() {
        let authority = source("authority").expect("authority source");
        assert!(authority.contains(&format!(
            "pub(crate) const CGROUP_ROOT: &str = {:?};",
            crate::ladder::TD_APPLICATION_CGROUP_ROOT
        )));
        let cgroup = source("cgroup").expect("cgroup source");
        assert!(cgroup.contains(&format!(
            "const DELEGATE_COMPONENT: &str = {:?};",
            crate::ladder::TD_APPLICATION_CGROUP_MEMBERSHIP_ROOT.trim_start_matches('/')
        )));
    }

    #[test]
    fn production_launch_reaps_without_running_the_diagnostic_fixture() {
        let transition = source("transition").expect("transition source");
        assert!(transition.contains(
            "if application.is_none() {\n        mount_reaper_probe(executable)?;\n    }"
        ));
        assert!(transition.contains("fn mount_reaper_probe(executable: &Path)"));
        assert!(!transition.contains("fs::copy(executable"));
        assert!(transition.contains(
            "Stage2Action::Probe => {\n            probe_pid1_lifecycle()?;"
        ));
        assert_eq!(transition.matches("probe_pid1_lifecycle()?").count(), 1);
        assert!(transition.contains(".stdin(Stdio::from(null_input))"));
        assert!(transition.contains(".stdout(Stdio::from(null_output))"));
        assert!(transition.contains(".stderr(Stdio::from(null_error))"));
        assert!(transition.contains("let (mut stage2_error, stage2_error_writer) = io::pipe()?;"));
        assert!(transition.contains("let mut child = command.spawn()?;\n        drop(command);"));
        assert!(transition.contains("sys::set_dumpable(false)?;"));
        assert!(transition.contains("sys::set_parent_death_signal()?;"));
        assert!(transition.contains("start_stage1_liveness_watcher()?;"));
        assert_eq!(transition.matches("sys::bring_up_loopback()").count(), 2);
        assert!(transition.contains(
            ".require_application_change(&before, application.isolate_network)?;"
        ));
        assert!(transition.contains(
            "if application.isolate_network {\n            sys::bring_up_loopback()"
        ));
        let sys = source("sys").expect("syscall source");
        for row in [
            "const SYS_IOCTL: usize = 16;",
            "const SYS_SETSID: usize = 112;",
            "const SIOCGIFFLAGS: usize = 0x8913;",
            "const SIOCSIFFLAGS: usize = 0x8914;",
            "struct IfreqFlags",
        ] {
            assert!(sys.contains(row), "td-jail syscall source lacks {row}");
        }
        let filter = transition
            .find("install_standard_seccomp_filter().map_err(|error|")
            .expect("seccomp installation");
        let nondumpable = transition
            .find("sys::set_dumpable(false)?;")
            .expect("nondumpable transition");
        let watcher = transition
            .find("start_stage1_liveness_watcher()?;")
            .expect("liveness watcher");
        let data_limit = transition
            .find("sys::set_and_require_data_limit(resources.memory_max_bytes)?;")
            .expect("data limit");
        let application = transition
            .find("run_application(&entry, &environment, &arguments)")
            .expect("application launch");
        assert!(
            filter < data_limit
                && data_limit < nondumpable
                && nondumpable < watcher
                && watcher < application,
            "stage 2 must install confinement and the inherited data limit before it creates a thread or launches the app"
        );
        assert!(transition.contains("if pid == application_pid {"));
        assert!(transition.contains(
            "application_status = Some(status);\n                    break;"
        ));
        assert!(transition.contains(
            "Some(_) => terminate_and_reap_survivors().err(),\n        None => None,"
        ));
        assert_eq!(transition.matches("terminate_and_reap_survivors()?").count(), 1);
        assert!(transition.contains("Some(_) => terminate_and_reap_survivors().err(),"));
        assert!(transition.contains("sys::terminate_namespace,"));
        assert!(transition.contains(
            "const SURVIVOR_TERM_TIMEOUT: Duration = Duration::from_secs(2);"
        ));
        assert!(transition.contains(
            "const SURVIVOR_KILL_TIMEOUT: Duration = Duration::from_secs(2);"
        ));
        assert!(transition.contains(
            "drain(SURVIVOR_TERM_TIMEOUT, &mut reaped, false)? == DrainOutcome::Drained"
        ));
        assert!(transition.contains(
            "drain(SURVIVOR_KILL_TIMEOUT, &mut reaped, true)? == DrainOutcome::Drained"
        ));
        assert!(transition.contains("probe_pid1_survivor_cleanup()"));
        assert!(transition.contains(
            "require_single_survivor_signal(&term_reaped, term_pid, sys::SIGTERM, \"TERM cleanup\")"
        ));
        assert!(transition.contains(
            "require_single_survivor_signal(&kill_reaped, kill_pid, sys::SIGKILL, \"KILL cleanup\")"
        ));
        let descendant_parent = transition
            .split_once("fn run_descendant_parent")
            .expect("descendant parent")
            .1
            .split_once("pub fn run_reaper_orphan")
            .expect("descendant orphan")
            .0;
        assert!(descendant_parent.contains(".stdout(Stdio::null())"));
        let writable_probe = transition
            .split_once("fn require_writable_directory")
            .expect("writable probe")
            .1
            .split_once("fn require_read_only_mount")
            .expect("read-only probe")
            .0;
        assert!(
            writable_probe.find("fs::remove_file").expect("unlink")
                < writable_probe.find("file.write_all").expect("write")
        );
    }

    #[test]
    fn jail_grammar_is_bound_to_the_image_and_spec_compilers() {
        let authority = source("authority").expect("authority source");
        assert!(authority.contains(&format!(
            "const CONFIG: &str = {:?};",
            crate::ladder::TD_APPLICATION_CONFIG_TEXT
        )));
        assert!(authority.contains(&format!(
            "const CONFIG_PATH: &str = {:?};",
            crate::ladder::TD_APPLICATION_CONFIG_PATH
        )));
        assert!(authority.contains(&format!(
            "const PACKAGE_ROOT: &str = {:?};",
            crate::ladder::TD_APPLICATION_PACKAGE_ROOT
        )));
        assert!(authority.contains(&format!(
            "const STATE_ROOT: &str = {:?};",
            crate::ladder::TD_APPLICATION_STATE_ROOT
        )));
        assert!(authority.contains(&format!(
            "pub(crate) const RUNTIME_ROOT_NAME: &str = {:?};",
            crate::ladder::TD_APPLICATION_RUNTIME_ROOT
        )));
        assert!(authority.contains(&format!(
            "const REGISTRY_PATH: &str = {:?};",
            crate::ladder::TD_APPLICATION_REGISTRY
        )));
        assert!(authority.contains("let mut builder = fs::DirBuilder::new();"));
        assert!(authority.contains("builder.mode(0o700);"));

        let main = source("main").expect("main source");
        assert!(main.contains(&format!(
            "const RESERVED_LAUNCHER_NAMES: &[&str] = &{:?};",
            td_engine::launcher::RESERVED_LAUNCHER_NAMES
        )));
        assert!(main.contains("if !RESERVED_LAUNCHER_NAMES.contains(&name)"));
        assert_eq!(
            target_authority::test_limits(),
            [
                td_engine::application_spec::MAX_APPLICATION_SPEC_BYTES,
                td_engine::launcher::MAX_APPLICATION_TABLE_BYTES,
                td_engine::launcher::MAX_APPLICATIONS,
                td_engine::application::MAX_APPLICATION_NAME_BYTES,
                td_engine::application_spec::MAX_SPEC_ENVIRONMENT_ENTRIES,
                td_engine::application::MAX_ENVIRONMENT_NAME_BYTES,
                td_engine::application::MAX_ENVIRONMENT_VALUE_BYTES,
                td_engine::application::MAX_ENTRY_BYTES,
            ]
        );
        for valid in ["firefox", "A.b_c-d9", ".hidden", &"a".repeat(32)] {
            assert!(td_engine::application::validate_application_name(valid).is_ok());
            assert!(target_authority::test_validate_application_name(valid).is_ok());
        }
        for invalid in [
            "",
            &"a".repeat(33),
            "-firefox",
            ".",
            "fire..fox",
            "fire/fox",
            "fire fox",
            "fírefox",
        ] {
            assert!(td_engine::application::validate_application_name(invalid).is_err());
            assert!(target_authority::test_validate_application_name(invalid).is_err());
        }
        for fragment in [
            "name.starts_with('-')",
            "name == \".\"",
            "name.contains(\"..\")",
            "byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-')",
        ] {
            assert!(authority.contains(fragment));
        }

        let declaration = ApplicationDeclaration::new(
            "empty-runtime",
            crate::ladder::TD_JAIL_FIXTURE_ENTRY,
        )
        .and_then(|declaration| declaration.with_alias(crate::ladder::TD_JAIL_FIXTURE_ALIAS))
        .expect("fixture declaration");
        let manifest = declaration
            .manifest(
                crate::ladder::TD_JAIL_FIXTURE_NAME,
                "0.1",
                ApplicationProvenance::Source,
            )
            .expect("fixture manifest");
        let permissions = PermissionPolicy::new()
            .with_socket(PermissionSocket::Wayland)
            .and_then(|permissions| {
                permissions.with_filesystem(
                    "xdg-download",
                    FilesystemAccess::ReadWrite,
                    true,
                )
            })
            .and_then(|permissions| permissions.with_memory_high(48 * 1024 * 1024))
            .and_then(|permissions| permissions.with_memory_max(64 * 1024 * 1024))
            .and_then(|permissions| permissions.with_pids_max(32))
            .and_then(|permissions| permissions.with_cpu_max(50_000, 100_000))
            .expect("Wayland, filesystem and resource policy");
        let spec = ApplicationSpec::compile(
            &manifest,
            "/td/store/0123456789abcdefghijklmnopqrstuv-empty-runtime-1",
            permissions,
        )
        .expect("fixture spec")
        .to_keyfile();
        target_authority::test_parse_spec(&spec)
            .expect("td-jail must consume the exact spec emitted by the compiler");
        // The two halves of the session bus are held to ONE value, by running
        // the jail's own contract over the spec the engine compiled. A draft
        // searched the emitted text for a literal built from an ENGINE constant
        // — which pins the engine against itself and notices nothing if td-jail
        // starts expecting a different path. `test_parse_spec` cannot do it
        // either: it is the grammar, and never consults the required-value
        // table. `validate_environment_list` is what a launch runs, so it is
        // what this runs.
        target_authority::test_validate_spec_environment(
            &spec,
            td_engine::application_spec::APPLICATION_UID,
        )
        .expect("td-jail's environment contract must accept the spec the engine compiles");
        // And it has teeth: the same spec with the bus somewhere else is
        // refused. Without this, a contract that had stopped checking the value
        // would pass the assertion above just as happily.
        let elsewhere = spec.replace(
            &format!(
                "DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/{}/bus",
                td_engine::application_spec::APPLICATION_UID
            ),
            &format!(
                "DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/{}/elsewhere",
                td_engine::application_spec::APPLICATION_UID
            ),
        );
        assert_ne!(elsewhere, spec, "the compiled spec must name the bus at all");
        assert!(
            target_authority::test_validate_spec_environment(
                &elsewhere,
                td_engine::application_spec::APPLICATION_UID,
            )
            .is_err(),
            "td-jail accepted a spec naming a bus it does not bind"
        );
        let package = "/td/store/0123456789abcdefghijklmnopqrstuv-td-jail-fixture-0.1";
        let registry = ApplicationRegistry::new(vec![(
            crate::ladder::TD_JAIL_FIXTURE_NAME.to_string(),
            package.to_string(),
        )])
        .expect("fixture registry");
        assert_eq!(
            target_authority::test_registry_entry(
                &registry.to_tsv(),
                crate::ladder::TD_JAIL_FIXTURE_NAME,
            )
            .expect("td-jail must consume the exact registry emitted by the compiler"),
            Some(PathBuf::from(package))
        );
        for (constant, value) in [
            ("SPEC_FORMAT", "format=1"),
            ("NAME_PREFIX", "name="),
            ("RUNTIME_PREFIX", "runtime="),
            ("ENTRY_PREFIX", "entry="),
            ("ENVIRONMENT_SECTION", "[Environment]"),
        ] {
            assert!(
                authority.contains(&format!("const {constant}: &str = {value:?};")),
                "td-jail source lacks {constant}={value:?}"
            );
            assert!(spec
                .lines()
                .any(|line| line == value || line.starts_with(value)));
        }
        assert!(spec.contains("[Context]\nsockets=wayland\n"));
        assert!(spec.contains("[Filesystem]\nxdg-download=rw:create\n"));
        assert!(spec.contains(
            "[Resources]\nmemory-high=50331648\nmemory-max=67108864\npids-max=32\ncpu-max=50000 100000\n"
        ));
    }
}

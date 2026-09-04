use crate::ladder::{split_target_debug, target_rustc};
use crate::types::{Recipe, Step};

const MAIN_RS: &str = include_str!("../../../td-jail/src/main.rs");
#[cfg(test)]
const BUILDER_APPLICATION_RS: &str = include_str!("../../../builder/src/application.rs");
const MODULES: &[(&str, &str)] = &[
    (
        "authority",
        include_str!("../../../td-jail/src/authority.rs"),
    ),
    ("bus", include_str!("../../../td-jail/src/bus.rs")),
    ("cgroup", include_str!("../../../td-jail/src/cgroup.rs")),
    ("firefox", include_str!("../../../td-jail/src/firefox.rs")),
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

    /// The §H item 12 marker is a literal in td-jail and a constant in
    /// `ladder.rs`, in different crates with no compiler relationship. The
    /// boot oracle reads the ladder one and the guest prints the td-jail
    /// one, so if they ever differ the marker simply never appears and the
    /// boot fails with the row's absence diagnostic rather than with the
    /// truth, which is that two strings drifted.
    #[test]
    fn recipe_embeds_the_kill_reaps_marker() {
        let transition = source("transition").expect("transition source");
        assert!(transition.contains(&format!(
            "pub const KILL_REAPS_MARKER: &str = \"{}\";",
            crate::ladder::TD_JAIL_KILL_REAPS_MARKER
        )));
    }

    /// §H item 12's driver, pinned at source level because its composition
    /// is what no test can reach.
    ///
    /// A reviewer showed the gap by mutation: deleting the
    /// `require_instance_shape` CALL, or replacing the
    /// `require_killed_by_signal` call with a bare wait, left the whole
    /// suite green. The helpers have tests; their USE had nothing, because
    /// reaching those lines needs a live instance, and building one needs
    /// `CLONE_NEWUSER` in a single-threaded process -- which libtest cannot
    /// give. This is the crate's usual answer to an invariant the compiler
    /// cannot express, applied to one a test cannot either.
    #[test]
    fn the_kill_reaps_driver_still_uses_the_checks_it_documents() {
        let transition = source("transition").expect("transition source");
        let driver = transition
            .split_once("fn observe_kill_reaps(")
            .expect("kill-reaps driver")
            .1
            .split_once("\nfn ")
            .expect("driver end")
            .0;
        for call in [
            "require_live(\"stage 2\", stage2)?",
            "require_live(\"the jailed descendant\", descendant)?",
            "watchdog.begin_teardown()?",
            "require_killed_by_signal(stage1.wait()?)?",
            "stage1.kill()?",
            "is_gone(stage2, before_stage2.starttime)?",
            "is_gone(descendant, before_descendant.starttime)?",
        ] {
            assert!(
                driver.contains(call),
                "the kill-reaps driver no longer calls {call}"
            );
        }
        // TWICE, not once: the shape is established, and then re-established
        // as late as possible before the kill, because an instance that
        // ended itself in between would leave exactly the evidence a
        // teardown leaves. A `contains` here would have been satisfied by
        // either call alone, and a reviewer's mutation deleting the second
        // one stayed green until this counted them.
        assert_eq!(
            driver.matches("require_instance_shape(stage2, descendant,").count(),
            2,
            "the driver must check the instance shape on both readings"
        );
        assert_eq!(
            driver.matches("require_live(").count(),
            4,
            "the driver must read both witnesses on both readings"
        );
        assert_eq!(
            driver
                .matches("cgroup::require_process_membership(")
                .count(),
            4,
            "the driver must read both witnesses' cgroup on both readings"
        );
        // And the kill is between the checks and the watching, which is the
        // only order in which any of it means anything.
        //
        // The LAST shape check, not the first. A reviewer's mutation moved
        // the late re-check to AFTER the kill: the count above still read
        // 2, a `find` still reported a check before the kill, and the whole
        // suite stayed green -- while the self-termination window the second
        // read exists to narrow was silently reopened. It re-reads structs
        // captured before the kill, so it would still pass at runtime too.
        let last_shape = driver
            .rfind("require_instance_shape")
            .expect("last shape check");
        let last_membership = driver
            .rfind("cgroup::require_process_membership")
            .expect("last cgroup membership check");
        let shape = driver.find("require_instance_shape").expect("shape check");
        let kill = driver.find("stage1.kill()?").expect("the kill");
        let watchdog = driver
            .find("watchdog.begin_teardown()?")
            .expect("teardown watchdog phase");
        let killed = driver
            .find("require_killed_by_signal")
            .expect("signal check");
        let watch = driver.find("is_gone(stage2").expect("the watch");
        let removed = driver
            .find("cgroup::wait_until_removed(")
            .expect("cgroup removal observation");
        assert!(
            shape < kill
                && last_shape < kill
                && last_membership < kill
                && watchdog < kill
                && kill < killed
                && killed < watch
                && watch < removed,
            "the driver must check the instance and cgroup -- BOTH times -- then kill, \
             confirm the signal, watch the processes, and observe leaf removal"
        );
        // The reuse guard is what gives the second reading its meaning: two
        // live pids that were checked are only the same two processes if
        // their start times did not move. Deleting it reds nothing else.
        assert!(
            driver.contains("confirm_stage2.starttime != before_stage2.starttime"),
            "the driver must confirm the witnesses were not replaced between readings"
        );
        assert!(
            driver.contains("confirm_descendant.starttime != before_descendant.starttime"),
            "the driver must confirm the descendant was not replaced between readings"
        );
        let watchdog_body = transition
            .split_once("fn start_probe_watchdog(")
            .expect("kill-reaps watchdog")
            .1
            .split_once("\n/// Block until")
            .expect("watchdog end")
            .0;
        let before = watchdog_body
            .find("recv_timeout(KILL_REAPS_CEILING)")
            .expect("pre-teardown watchdog phase");
        let teardown = watchdog_body
            .find("recv_timeout(KILL_REAPS_POST_CEILING)")
            .expect("teardown watchdog phase");
        assert!(before < teardown);
        assert!(watchdog_body.contains(
            "Err(mpsc::RecvTimeoutError::Disconnected) => return"
        ));
    }

    /// The two things stage 1 must NOT do, both of which it once did.
    ///
    /// Neither is a style preference; each is a kernel rule that made the
    /// probe unable to pass, and neither reds any test, because reaching
    /// stage 1's body needs a live jail.
    ///
    /// 1. No watchdog thread. `CLONE_NEWUSER` refuses a multi-threaded
    ///    caller, so no thread may exist before the unshare; and after
    ///    `unshare(CLONE_NEWPID)` the kernel refuses `CLONE_THREAD` while
    ///    `pid_ns_for_children` differs from the active pid namespace, which
    ///    is exactly what an unshared-but-unforked task has. There is no
    ///    moment in between, so stage 1 can never hold one.
    /// 2. No `/proc` walk. Stage 2 pivots in the mount namespace stage 1
    ///    created and shares, and `pivot_root(2)` re-roots every process in
    ///    it -- so once stage 2 has reported, stage 1's `/proc` is the
    ///    jail's, showing namespace pids. A host pid cannot be resolved
    ///    there.
    ///
    /// The second is about WALKING `/proc` for other processes, after the
    /// unshare. Stage 1 does read `/proc/self/stat`, once, BEFORE the
    /// unshare, to check its parent is still the driver -- that read is
    /// against the host's `/proc` and about stage 1 itself, so it is not
    /// what this forbids and the forbidden list is written to let it
    /// through.
    #[test]
    fn stage_1_neither_threads_nor_walks_proc() {
        let transition = source("transition").expect("transition source");
        let stage1 = transition
            .split_once("pub fn run_kill_reaps_stage_1(")
            .expect("kill-reaps stage-1 role")
            .1
            .split_once("\nfn ")
            .expect("stage-1 role end")
            .0;
        let body = stage1.split_once("\n}").map_or(stage1, |(body, _)| body);
        // Substrings, not full call spellings: a reviewer showed that
        // `std::thread::spawn` and `fs::read_dir(Path::new("/proc"))` both
        // slipped past an earlier list of four exact forms, which made this
        // pin look stronger than it was.
        for forbidden in [
            "start_probe_watchdog(",
            "thread::Builder",
            "thread::spawn",
            "find_jailed_descendant(",
            "read_dir(",
        ] {
            assert!(
                !body.contains(forbidden),
                "stage 1 must not use {forbidden}: it holds neither a thread nor a host \
                 /proc view, and both were once assumed"
            );
        }
        assert!(
            body.contains("sys::set_parent_death_signal()?;"),
            "stage 1's only bound is the driver's deadline reaching it through PDEATHSIG"
        );
    }

    /// The driver, not stage 1, resolves the descendant's host pid.
    ///
    /// This is the other half of the pin above: the walk has to happen
    /// somewhere, and the driver is the last process in the chain that
    /// still has the host's `/proc`, because it never unshares.
    #[test]
    fn the_driver_resolves_the_descendant_in_its_own_host_proc() {
        let transition = source("transition").expect("transition source");
        let driver = transition
            .split_once("fn observe_kill_reaps(")
            .expect("kill-reaps driver")
            .1
            .split_once("\nfn ")
            .expect("driver end")
            .0;
        assert!(
            driver.contains("find_jailed_descendant(stage2, namespace_pid)?"),
            "the driver must resolve the descendant itself"
        );
        // Before the liveness reads, or it would be measuring a pid it had
        // not yet resolved.
        let resolve = driver
            .find("find_jailed_descendant(")
            .expect("descendant resolution");
        let live = driver.find("require_live(").expect("liveness read");
        assert!(
            resolve < live,
            "the descendant must be resolved before it is measured"
        );
    }

    /// `run_stage2_kill_hold` starts a SECOND liveness watcher, and the
    /// ordering pin above it covers only the Launch arm's.
    ///
    /// UNSAFE.md §9 says the watcher is created after the filter readback
    /// so it inherits the filter, and names the embedded-source test as
    /// what holds that. This keeps that sentence true for both sites: the
    /// arm is dispatched from `run_stage2`'s terminal `match`, which is
    /// after the install, so pinning the dispatch pins the ordering.
    #[test]
    fn the_kill_hold_arm_starts_its_watcher_after_the_filter_too() {
        let transition = source("transition").expect("transition source");
        let install = transition
            .find(
                "install_standard_seccomp_filter(firefox_seccomp_probe)\
                 .map_err(|error|",
            )
            .expect("seccomp installation");
        let dispatch = transition
            .find("Stage2Action::KillHold { cgroup_membership } => {")
            .expect("kill-hold dispatch");
        assert!(
            install < dispatch,
            "the kill-hold arm must be dispatched after the filter is installed"
        );
        let membership = transition
            .get(dispatch..)
            .and_then(|arm| arm.find("cgroup::require_current_membership(&cgroup_membership)?;"))
            .map(|at| dispatch + at)
            .expect("kill-hold cgroup readback");
        let hold_dispatch = transition
            .get(membership..)
            .and_then(|arm| arm.find("run_stage2_kill_hold()"))
            .map(|at| membership + at)
            .expect("kill-hold terminal arm");
        assert!(
            dispatch < membership && membership < hold_dispatch,
            "the kill-hold arm must verify its application cgroup before waiting"
        );
        let hold = transition
            .split_once("fn run_stage2_kill_hold()")
            .expect("kill-hold arm")
            .1;
        assert!(
            hold.starts_with(" -> io::Result<()> {\n    start_stage1_liveness_watcher()?;"),
            "the kill-hold arm must start the liveness watcher first, so the mechanism \
             under test is the production one"
        );
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
        // The entry's stdio is three null devices, or under `devices=tty`
        // three clones of the terminal stage 2 proved; never anything else.
        assert!(transition.contains("let (input, output, error) = if terminal {"));
        assert!(transition.contains("Stdio::from(terminal.try_clone_to_owned()?),"));
        assert!(transition.contains("Stdio::from(fs::File::open(\"/dev/null\")?),"));
        assert!(transition.contains(
            "Stdio::from(OpenOptions::new().write(true).open(\"/dev/null\")?),"
        ));
        assert!(transition.contains(".stdin(input)\n        .stdout(output)\n        .stderr(error);"));
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
            .find(
                "install_standard_seccomp_filter(firefox_seccomp_probe)\
                 .map_err(|error|",
            )
            .expect("seccomp installation");
        let nondumpable = transition
            .find("sys::set_dumpable(false)?;")
            .expect("nondumpable transition");
        // Anchored on the readback that precedes it rather than on the call
        // alone: §H item 12's held stage 2 starts the SAME watcher, earlier
        // in the file, so a bare `find` would return that one and this
        // ordering assertion would silently be about a different frame.
        const WATCHER_AFTER_DUMPABLE_READBACK: &str = concat!(
            "return Err(io::Error::other(\"PID 1 remained dumpable\"));\n",
            "            }\n",
            "            start_stage1_liveness_watcher()?;",
        );
        let watcher = transition
            .find(WATCHER_AFTER_DUMPABLE_READBACK)
            .expect("liveness watcher");
        let data_limit = transition
            .find("sys::set_and_require_data_limit(resources.memory_max_bytes)?;")
            .expect("data limit");
        let application = transition
            .find("run_application(\n                &entry,")
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
        let transition = source("transition").expect("transition source");
        let alias_rows = "    (\"bin\", \"/usr/bin\"),\n    (\"lib\", \"/usr/lib\"),\n    (\"lib64\", \"/usr/lib64\"),\n    (\"sbin\", \"/usr/sbin\"),\n";
        assert!(BUILDER_APPLICATION_RS.contains(&format!(
            "const APPLICATION_ALIASES: &[(&str, &str)] = &[\n{alias_rows}];"
        )));
        assert!(transition.contains(&format!(
            "const RUNTIME_ALIASES: &[(&str, &str)] = &[\n{alias_rows}];"
        )));
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
            "/td/store/0123456789abcdfghijklmnpqrsvwxyz-empty-runtime-1",
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

        let firefox = crate::catalog::registry::firefox::recipe();
        let firefox_name = firefox.name;
        let firefox_version = firefox.version;
        let firefox_declaration = firefox.application.expect("Firefox declaration");
        let firefox_permissions = firefox
            .application_permissions
            .expect("Firefox permissions");
        let firefox_manifest = firefox_declaration
            .manifest(
                &firefox_name,
                &firefox_version,
                ApplicationProvenance::Foreign,
            )
            .expect("Firefox manifest");
        let firefox_spec = ApplicationSpec::compile(
            &firefox_manifest,
            "/td/store/0123456789abcdfghijklmnpqrsvwxyz-freedesktop-platform-25-08-25.08",
            firefox_permissions,
        )
        .expect("Firefox spec")
        .to_keyfile();
        target_authority::test_validate_spec_environment(
            &firefox_spec,
            td_engine::application_spec::APPLICATION_UID,
        )
        .expect("td-jail must accept the compiler-owned Firefox loader path");
        for altered in [
            firefox_spec.replace("LD_LIBRARY_PATH=/app/lib:/app/lib/firefox\n", ""),
            firefox_spec.replace(
                "LD_LIBRARY_PATH=/app/lib:/app/lib/firefox",
                "LD_LIBRARY_PATH=/app/lib",
            ),
            firefox_spec.replace("name=firefox\n", "name=other\n"),
        ] {
            assert!(
                target_authority::test_validate_spec_environment(
                    &altered,
                    td_engine::application_spec::APPLICATION_UID,
                )
                .is_err(),
                "td-jail accepted a Firefox loader path outside its exact application/runtime policy"
            );
        }
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

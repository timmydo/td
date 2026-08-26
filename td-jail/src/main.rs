//! td-jail — td's application confinement boundary.
//!
//! Product applications are selected by an installed argv[0] name and the
//! immutable image index. The exact `td-jail` argv[0] also exposes an explicit
//! development-host launch when no product configuration is installed.
#![deny(unsafe_code)]

mod authority;
mod bus;
mod cgroup;
#[allow(dead_code)]
#[cfg_attr(test, allow(clippy::unwrap_used))]
#[cfg_attr(
    not(feature = "target-recipe"),
    path = "../../engine/src/permissions.rs"
)]
mod permissions;
mod seccomp;
mod sys;
mod transition;

use std::io::Write;
use std::process::ExitCode;

const RESERVED_LAUNCHER_NAMES: &[&str] = &["td-jail", "td-jail-reaper-probe"];

#[derive(Clone, Copy, Eq, PartialEq)]
enum ApplicationLaunchKind {
    Product,
    Host,
}

fn run() -> std::io::Result<()> {
    let mut arguments = std::env::args_os();
    let argv0 = arguments.next().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "argv[0] is missing")
    })?;
    let name = authority::application_name(&argv0)?;
    let mut arguments = arguments.peekable();
    if application_launch_kind(name, arguments.peek()).is_some() {
        return transition::spawn_application_session(argv0, arguments);
    }
    match transition::parse_mode(arguments)? {
        transition::Mode::ApplicationSession {
            parent,
            argv0,
            arguments,
        } => {
            transition::contain_application_session(parent)?;
            launch_selected_application(argv0, arguments.into_iter())
        }
        transition::Mode::Probe => transition::probe_transition(),
        transition::Mode::ResourceProbe { application } => {
            transition::probe_resource_caps(&application)
        }
        transition::Mode::WriteFilter => transition::write_standard_filter(),
        transition::Mode::CgroupCleanupBootstrap { membership } => {
            transition::run_cgroup_cleanup_bootstrap(&membership)
        }
        transition::Mode::CgroupCleanupWatcher { membership } => {
            transition::run_cgroup_cleanup_watcher(&membership)
        }
        transition::Mode::Stage2 {
            token,
            identity,
            outside_identity,
            action,
        } => transition::run_stage2(token, identity, outside_identity, action),
        transition::Mode::ReaperChild => transition::run_reaper_child(),
        transition::Mode::ReaperOrphan => transition::run_reaper_orphan(),
        transition::Mode::SurvivorChild => transition::run_survivor_child(),
        transition::Mode::SurvivorOrphan => transition::run_survivor_orphan(),
    }
}

fn launch_selected_application<I>(argv0: std::ffi::OsString, arguments: I) -> std::io::Result<()>
where
    I: Iterator<Item = std::ffi::OsString>,
{
    let name = authority::application_name(&argv0)?;
    let mut arguments = arguments.peekable();
    match application_launch_kind(name, arguments.peek()) {
        Some(ApplicationLaunchKind::Product) => {
            transition::launch_application(authority::resolve(name, arguments)?)
        }
        Some(ApplicationLaunchKind::Host) => {
            drop(arguments.next());
            let config = arguments.next().ok_or_else(host_usage_error)?;
            let application = arguments.next().ok_or_else(host_usage_error)?;
            transition::launch_application(authority::resolve_host(
                &config,
                &application,
                arguments,
            )?)
        }
        None => Err(host_usage_error()),
    }
}

fn application_launch_kind(
    name: &str,
    first_argument: Option<&std::ffi::OsString>,
) -> Option<ApplicationLaunchKind> {
    if !RESERVED_LAUNCHER_NAMES.contains(&name) {
        return Some(ApplicationLaunchKind::Product);
    }
    if name == "td-jail"
        && first_argument.is_some_and(|argument| authority::is_host_argument(argument))
    {
        return Some(ApplicationLaunchKind::Host);
    }
    None
}

fn host_usage_error() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        "host usage: td-jail --host CONFIG APPLICATION [ARG ...]",
    )
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let _ = writeln!(std::io::stderr(), "td-jail: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod confinement {
    #![allow(clippy::panic, clippy::unwrap_used)]

    const AUTHORITY: &str = include_str!("authority.rs");
    const BUS: &str = include_str!("bus.rs");
    const CGROUP: &str = include_str!("cgroup.rs");
    const MAIN: &str = include_str!("main.rs");
    const PERMISSIONS: &str = include_str!("../../engine/src/permissions.rs");
    const SECCOMP: &str = include_str!("seccomp.rs");
    const SYS: &str = include_str!("sys.rs");
    const TRANSITION: &str = include_str!("transition.rs");

    #[test]
    fn unsafe_is_confined_to_one_syscall_instruction() {
        let shipped_main = MAIN.split_once("#[cfg(test)]").unwrap().0;
        assert!(shipped_main.contains("#![deny(unsafe_code)]"));
        assert_eq!(SYS.matches("#[allow(unsafe_code)]").count(), 1);
        assert_eq!(SYS.matches("unsafe {").count(), 1);
        assert_eq!(shipped_main.matches("#[allow(unsafe_code)]").count(), 0);
        assert_eq!(AUTHORITY.matches("#[allow(unsafe_code)]").count(), 0);
        assert_eq!(AUTHORITY.matches("unsafe {").count(), 0);
        // The bus client reaches its socket through `std`, so it names no
        // syscall of its own and adds nothing to surface #9.
        assert_eq!(BUS.matches("#[allow(unsafe_code)]").count(), 0);
        assert_eq!(BUS.matches("unsafe {").count(), 0);
        assert_eq!(CGROUP.matches("#[allow(unsafe_code)]").count(), 0);
        assert_eq!(CGROUP.matches("unsafe {").count(), 0);
        assert_eq!(PERMISSIONS.matches("#[allow(unsafe_code)]").count(), 0);
        assert_eq!(PERMISSIONS.matches("unsafe {").count(), 0);
        assert_eq!(SECCOMP.matches("#[allow(unsafe_code)]").count(), 0);
        assert_eq!(SECCOMP.matches("unsafe {").count(), 0);
        assert_eq!(TRANSITION.matches("#[allow(unsafe_code)]").count(), 0);
        assert_eq!(TRANSITION.matches("unsafe {").count(), 0);
    }

    #[test]
    fn host_mode_is_explicit_and_absent_from_the_product_configuration() {
        let shipped_main = MAIN.split_once("#[cfg(test)]").unwrap().0;
        assert!(shipped_main.contains("if name == \"td-jail\""));
        assert!(shipped_main.contains("authority::is_host_argument(argument)"));
        assert!(AUTHORITY.contains("match fs::symlink_metadata(CONFIG_PATH)"));
        assert!(AUTHORITY.contains(
            "host mode is unavailable when the product application configuration is installed"
        ));
    }

    /// §D's two-phase registration is an ORDERING, and no type expresses it.
    ///
    /// Both halves are load-bearing and both fail quietly if moved:
    ///
    /// Phase one must precede the SPAWN, so that the pending registration
    /// exists before anything inside the jail can connect. §D puts it before
    /// the `unshare` too, which is the cheap half: a refused registration then
    /// costs no namespaces, no mounts and no child to reap.
    ///
    /// A draft of this comment claimed the broker would otherwise record a pid
    /// it could not see. That is false — `unshare(CLONE_NEWPID)` does not move
    /// the caller and stage 1 keeps the old root — and the assertion is kept
    /// for the reasons above rather than the reason first given for it.
    ///
    /// Phase two must run before the write that releases stage 2, because that
    /// write is what lets the application run. `Command::spawn` has already
    /// returned by then, so completing afterwards leaves a window in which the
    /// application can connect while its registration is still pending. The
    /// broker fixes identity at accept, so such a connection stays wrong for
    /// its whole life however fast the completion then lands. Moved, this is a
    /// race that passes every test on an unloaded machine.
    ///
    /// A failed completion must kill the jail. An application the broker has
    /// no record of resolves `Unconfined` — full portal access for the one
    /// process on the system that is certainly confined.
    /// Rust source with its block comments removed, nesting included.
    ///
    /// Nesting matters because Rust allows it and because the naive version —
    /// stop at the first `*/` — leaves the tail of an outer comment behind,
    /// which is where commented-out code would reappear. An unterminated `/*`
    /// swallows the rest, which is what the compiler does with it too.
    fn without_block_comments(source: &str) -> String {
        let mut out = String::with_capacity(source.len());
        let mut rest = source;
        let mut depth = 0_usize;
        loop {
            let open = rest.find("/*");
            let close = rest.find("*/");
            match (depth, open, close) {
                (0, None, _) => {
                    out.push_str(rest);
                    return out;
                }
                (0, Some(at), _) => {
                    out.push_str(rest.get(..at).unwrap_or(""));
                    rest = rest.get(at.saturating_add(2)..).unwrap_or("");
                    depth = 1;
                }
                (_, Some(at), Some(shut)) if at < shut => {
                    rest = rest.get(at.saturating_add(2)..).unwrap_or("");
                    depth = depth.saturating_add(1);
                }
                (_, _, Some(shut)) => {
                    rest = rest.get(shut.saturating_add(2)..).unwrap_or("");
                    depth = depth.saturating_sub(1);
                }
                (_, _, None) => return out,
            }
        }
    }

    /// And its line comments.
    fn without_line_comments(source: &str) -> String {
        source
            .lines()
            .map(|line| match line.split_once("//") {
                Some((code, _)) => code,
                None => line,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The stripper is the thing standing between a commented-out phase two
    /// and a green suite, so it is tested rather than assumed.
    #[test]
    fn comments_are_stripped_including_nested_and_unterminated_blocks() {
        assert_eq!(without_block_comments("a/*b*/c"), "ac");
        assert_eq!(without_block_comments("a/*b/*c*/d*/e"), "ae");
        assert_eq!(without_block_comments("a/*b\nc*/d"), "ad");
        assert_eq!(without_block_comments("a/*b"), "a");
        assert_eq!(without_block_comments("plain"), "plain");
        // A `*/` with nothing open is not a comment and is left alone.
        assert_eq!(without_block_comments("a*/b"), "a*/b");
        assert_eq!(without_line_comments("keep // drop\nkeep2"), "keep \nkeep2");
        // The combination is what the test below uses, and the order matters:
        // a `//` inside a block comment must not end the block early.
        let both = without_line_comments(&without_block_comments("x/*\n// y\n*/z"));
        assert!(!both.contains('y'), "{both:?}");
        assert!(both.contains('x') && both.contains('z'), "{both:?}");
    }

    #[test]
    fn the_registration_brackets_the_launch() {
        let body = TRANSITION
            .split_once("pub fn launch_application")
            .unwrap()
            .1
            .split_once("fn read_launch_diagnostic")
            .unwrap()
            .0;
        // Comments are STRIPPED before anything is looked for. A draft of this
        // test searched the raw text, so commenting a line out — the ordinary
        // way somebody debugs a launch — left every assertion passing while
        // the behaviour was gone.
        //
        // BLOCK comments are stripped first, and a draft handled only line
        // comments. A reviewer wrapped phase two in `/* */` — the ordinary way
        // to comment out a multi-line block, and phase two is a multi-line
        // block — and every test stayed green while the jail released an
        // application it had never registered.
        let launch = without_line_comments(&without_block_comments(body));
        let at = |needle: &str| {
            launch
                .find(needle)
                .unwrap_or_else(|| panic!("launch_application no longer contains {needle}"))
        };
        // The GRANT reaches the wire, in the FIELD that carries it. A
        // reviewer replaced `&application.owned_names` with `&[]` and the
        // whole `td-jail` suite stayed green: `launch_application` needs
        // namespaces, so no unit test reaches this call, and the QEMU gate
        // that would is unrun here. That makes it the same class of rule as
        // the orderings below — real, untestable at runtime, and stated in
        // the source or nowhere.
        //
        // A second reviewer then defeated the first version of this
        // assertion, which looked for `&application.owned_names` anywhere: at
        // the time `register` took two adjacent `&[String]`, so SWAPPING them
        // left the substring present, sent the broker an empty own-set, and
        // recorded the granted names as predeclared services instead.
        // `register` takes a `Registration` now, so the swap has to be
        // written into a named field rather than slipped past a positional
        // list, and this pins that field.
        // The whole call, as one span. Pinning the two fields separately was
        // defeated the same way the plan pin was: build the `Registration`
        // into a `let mut request`, overwrite `request.owned = &[]`, pass
        // `request`. Both field substrings survived and the wire carried no
        // grant. A literal that is the ARGUMENT has nowhere to be edited
        // between construction and the call.
        assert!(
            launch.contains("identity.uid,\n        crate::bus::Registration {"),
            "the registration is no longer built as the argument of the call \
             that sends it, so there is a window in which its own-set can be \
             emptied after this test has seen it"
        );
        assert!(
            launch.contains("owned: &application.owned_names,"),
            "the launch no longer sends the permission file's own-set to the \
             broker as its own-set, so a sandboxed application may hold no name"
        );
        assert!(
            launch.contains("services: &[],"),
            "the launch predeclares services now, which is a change §D has to \
             carry rather than a slip in the argument this test is about"
        );
        assert_eq!(
            launch.matches("owned").count(),
            2,
            "`owned` is named somewhere else on the launch path, so the field \
             this test pins is not necessarily what reaches the broker"
        );
        assert!(at("ManagedCgroup::create(") < at("bus::register("));
        assert!(at("bus::register(") < at("sys::unshare_namespaces("));
        assert!(at("bus::register(") < at("close_inherited_descriptors("));
        assert!(at("bus::complete(") < at("proof_writer.write_all(&token)"));
        // The spawn is anchored too, and not because §D asks for it. Without
        // it the three assertions above hold while `command.spawn()` sits
        // ABOVE the unshare — every string still in order, and the child in
        // the host's namespaces. The sandbox would be gone and this test would
        // be green.
        assert!(at("sys::unshare_namespaces(") < at("command.spawn()"));
        assert!(at("command.spawn()") < at("bus::complete("));
        assert!(at("ManagedCgroup::create(") < at("sys::unshare_namespaces("));
        assert!(at("application_cgroup.attach(child.id())") < at("bus::complete("));
        assert!(at("application_cgroup.attach(child.id())") < at("proof_writer.write_all(&token)"));

        let refused = launch
            .split_once("bus::complete(")
            .unwrap()
            .1
            .split_once("proof_writer.write_all(&token)")
            .unwrap()
            .0;
        // All three, because killing is not the property — not releasing is.
        // A refactor that killed the child, wrote the proof anyway and then
        // returned the stored error would satisfy a `child.kill()` check on
        // its own, and would have released a jail the broker has no record of.
        assert!(
            refused.contains("child.kill()"),
            "a refused completion no longer kills the jail it opened"
        );
        assert!(
            refused.contains("child.wait()"),
            "a refused completion no longer reaps the jail it killed"
        );
        assert!(
            refused.contains("return Err("),
            "a refused completion no longer stops the launch, so the proof \
             write below it now runs and releases an unregistered jail"
        );
    }

    /// The plan's own-set is the permission file's, not an empty list.
    ///
    /// The companion to the `&application.owned_names` assertion above, at the
    /// other end of the same hop. `resolve` reads a real config, a real spec
    /// and a real store, so no unit test builds a `LaunchPlan` — a reviewer
    /// replaced this assignment with `Vec::new()` and all 111 `td-jail` tests
    /// passed while every application launched owning nothing.
    ///
    /// `granted_bus_names` itself IS unit-tested, which is exactly what makes
    /// this necessary rather than redundant: the rule is pinned and the wire
    /// to it was not, so the mutation that survived was not in the rule.
    #[test]
    fn a_launch_plan_carries_the_permission_files_own_entries() {
        let resolve = without_line_comments(&without_block_comments(AUTHORITY))
            .split_once("pub(crate) fn resolve")
            .unwrap()
            .1
            .split_once("pub(crate) fn resolve_resource_limits")
            .unwrap()
            .0
            .to_string();
        // The FIELD, initialised in place. Two earlier versions of this
        // assertion were defeated in review. The first looked for
        // `granted_bus_names(` anywhere in `resolve`, which a mutation
        // satisfies by calling it and discarding the result. The second
        // looked for the binding `let owned_names = granted_bus_names(…);`,
        // which a reviewer satisfied by leaving it in place and shadowing it
        // on the next line with `let owned_names = Vec::new();`. Both kept
        // every pinned substring and gave every launch an empty own-set.
        //
        // `resolve` now builds the field inside the struct literal, so there
        // is no binding to shadow and the pinned text is the assignment
        // itself. A determined rewrite can still defeat a substring — this
        // bounds spellings, not semantics — but the cheap accident it exists
        // to catch no longer has a shape.
        assert!(
            resolve.contains("owned_names: granted_bus_names(&spec.permissions),"),
            "a launch plan no longer takes its own-set from the permission \
             file, so an application may hold no name whatever its file says"
        );
        // Built in place AND never touched again. A reviewer defeated the
        // assertion above by keeping it word for word and then emptying the
        // field: bind the literal to `let mut plan`, `plan.owned_names.
        // clear()`, return `plan`. Every pinned substring survived and every
        // launch got an empty grant. One mention is the difference between
        // "the plan takes its own-set from the file" and "the plan mentions
        // taking its own-set from the file".
        assert_eq!(
            resolve.matches("owned_names").count(),
            1,
            "`owned_names` is named more than once while resolving a plan, so \
             the field this test pins is not necessarily the value that \
             survives to the caller"
        );
        assert!(
            resolve.contains("Ok(LaunchPlan {"),
            "the plan is no longer returned as the literal this test pins, so \
             there is a window between building it and handing it back"
        );
    }

    #[test]
    fn a_launch_plan_derives_network_isolation_from_the_permission_file() {
        let resolve = without_line_comments(&without_block_comments(AUTHORITY))
            .split_once("pub(crate) fn resolve")
            .unwrap()
            .1
            .split_once("pub(crate) fn resolve_resource_limits")
            .unwrap()
            .0
            .to_string();
        assert!(resolve.contains("let isolate_network = !spec.permissions.network();"));
        assert_eq!(resolve.matches("let isolate_network =").count(), 1);
        assert!(resolve.contains("isolate_network,"));
        assert!(resolve.contains("Ok(LaunchPlan {"));
    }

    #[test]
    fn cleanup_helper_owns_the_abandoned_leaf_protocol() {
        let managed = TRANSITION
            .split_once("impl ManagedCgroup")
            .unwrap()
            .1
            .split_once("pub fn launch_application")
            .unwrap()
            .0;
        let at = |needle: &str| {
            managed
                .find(needle)
                .unwrap_or_else(|| panic!("managed cgroup no longer contains {needle}"))
        };
        assert!(at("CgroupCleanup::spawn(") < at("cgroup::Instance::create("));
        assert!(managed.contains("drop(self.instance.take());\n        drop(self.cleanup.take());"));
        let bootstrap = TRANSITION
            .split_once("pub fn run_cgroup_cleanup_bootstrap")
            .unwrap()
            .1
            .split_once("pub fn run_cgroup_cleanup_watcher")
            .unwrap()
            .0;
        let watcher = TRANSITION
            .split_once("pub fn run_cgroup_cleanup_watcher")
            .unwrap()
            .1
            .split_once("fn cleanup_identity")
            .unwrap()
            .0;
        assert!(
            bootstrap
                .find("require_detached_session(")
                .unwrap_or_else(|| panic!("bootstrap session readback is absent"))
                < bootstrap
                    .find("Command::new(executable)")
                    .unwrap_or_else(|| panic!("watcher spawn is absent"))
        );
        assert!(
            watcher
                .find("require_detached_session(")
                .unwrap_or_else(|| panic!("watcher session readback is absent"))
                < watcher
                    .find("readiness.write_all(&CGROUP_CLEANUP_READY)")
                    .unwrap_or_else(|| panic!("watcher readiness is absent"))
        );
        assert_eq!(bootstrap.matches("sys::start_new_session()?").count(), 1);
        assert_eq!(watcher.matches("sys::start_new_session()?").count(), 1);
        assert_eq!(TRANSITION.matches(".current_dir(\"/\")").count(), 2);
        assert!(TRANSITION.contains("read_exact(&mut readiness)"));
        assert!(TRANSITION.contains("close_inherited_descriptors(cleanup_descriptor)?;"));
        assert!(managed.contains("None if self.instance.is_none() => Ok(None)"));
        assert!(MAIN.contains("run_cgroup_cleanup_bootstrap(&membership)"));
        assert!(MAIN.contains("run_cgroup_cleanup_watcher(&membership)"));
        assert_eq!(CGROUP.matches("wait_until_empty(&directory)").count(), 1);
        let abandoned = CGROUP
            .split_once("pub(crate) fn remove_abandoned")
            .unwrap()
            .1
            .split_once("pub(crate) fn probe_active")
            .unwrap()
            .0;
        assert!(
            abandoned.find("fs::symlink_metadata(&directory)")
                < abandoned.find("require_delegation(root, uid, gid)?")
        );
    }

    #[test]
    fn application_launch_isolates_or_preserves_supervision_before_authority() {
        let shipped_main = MAIN.split_once("#[cfg(test)]").unwrap().0;
        assert!(MAIN.contains("transition::spawn_application_session(argv0, arguments)"));
        assert_eq!(
            shipped_main
                .matches("application_launch_kind(name, arguments.peek())")
                .count(),
            2
        );
        assert!(TRANSITION.contains("std::process::exit(code)"));
        assert!(TRANSITION.contains("std::process::exit(128_i32.saturating_add(signal))"));
        let arm = MAIN
            .split_once("transition::Mode::ApplicationSession")
            .unwrap()
            .1
            .split_once("transition::Mode::Probe")
            .unwrap()
            .0;
        assert!(
            arm.find("transition::contain_application_session(parent)?")
                .unwrap_or_else(|| panic!("application session containment is absent"))
                < arm
                    .find("launch_selected_application(argv0")
                    .unwrap_or_else(|| panic!("authority resolution is absent"))
        );

        let containment = TRANSITION
            .split_once("pub fn contain_application_session")
            .unwrap()
            .1
            .split_once("pub fn probe_transition")
            .unwrap()
            .0;
        let at = |needle: &str| {
            containment
                .find(needle)
                .unwrap_or_else(|| panic!("application containment no longer contains {needle}"))
        };
        assert!(at("sys::set_parent_death_signal()?") < at("require_direct_parent("));
        assert!(at("require_direct_parent(") < at("containment.terminal == 0"));
        assert!(at("containment.terminal == 0") < at("require_supervised_group("));
        assert!(at("require_direct_parent(") < at("sys::start_new_session()?"));
        assert!(at("sys::start_new_session()?") < at("require_detached_session("));
        assert_eq!(TRANSITION.matches("sys::start_new_session()?").count(), 3);
    }

    #[test]
    fn syscall_and_argument_rosters_are_pinned() {
        let shipped_sys = SYS.split_once("#[cfg(test)]").unwrap().0;
        for syscall in [
            "const SYS_CLOSE: usize = 3;",
            "const SYS_IOCTL: usize = 16;",
            "const SYS_WAIT4: usize = 61;",
            "const SYS_KILL: usize = 62;",
            "const SYS_SETSID: usize = 112;",
            "const SYS_CAPGET: usize = 125;",
            "const SYS_CAPSET: usize = 126;",
            "const SYS_PIVOT_ROOT: usize = 155;",
            "const SYS_PRCTL: usize = 157;",
            "const SYS_MOUNT: usize = 165;",
            "const SYS_UMOUNT2: usize = 166;",
            "const SYS_UNSHARE: usize = 272;",
            "const SYS_PRLIMIT64: usize = 302;",
            "const SYS_SECCOMP: usize = 317;",
        ] {
            assert!(SYS.contains(syscall), "missing syscall pin: {syscall}");
        }
        assert_eq!(SYS.matches("const SYS_").count(), 14);
        assert!(SYS.contains("const BASE_NAMESPACE_FLAGS: usize ="));
        assert!(SYS.contains("const ISOLATED_NETWORK_FLAGS: usize ="));
        for flag in [
            "pub const MS_RDONLY: usize = 0x1;",
            "pub const MS_NOSUID: usize = 0x2;",
            "pub const MS_NODEV: usize = 0x4;",
            "pub const MS_NOEXEC: usize = 0x8;",
            "pub const MS_REMOUNT: usize = 0x20;",
            "pub const MS_BIND: usize = 0x1000;",
            "pub const MS_REC: usize = 0x4000;",
            "pub const MS_PRIVATE: usize = 0x4_0000;",
            "pub const MNT_DETACH: usize = 0x2;",
        ] {
            assert!(SYS.contains(flag), "missing mount flag pin: {flag}");
        }
        assert_eq!(SYS.matches("pub const MS_").count(), 8);
        assert!(SYS.contains("const LINUX_CAPABILITY_VERSION_3: u32 = 0x2008_0522;"));
        assert!(SYS.contains("const PR_SET_PDEATHSIG: usize = 1;"));
        assert!(SYS.contains("const PR_GET_DUMPABLE: usize = 3;"));
        assert!(SYS.contains("const PR_SET_DUMPABLE: usize = 4;"));
        assert!(SYS.contains("const PR_CAPBSET_READ: usize = 23;"));
        assert!(SYS.contains("const PR_CAPBSET_DROP: usize = 24;"));
        assert!(SYS.contains("const PR_SET_NO_NEW_PRIVS: usize = 38;"));
        assert!(SYS.contains("const PR_GET_NO_NEW_PRIVS: usize = 39;"));
        assert!(SYS.contains("const PR_CAP_AMBIENT: usize = 47;"));
        assert!(SYS.contains("const PR_CAP_AMBIENT_IS_SET: usize = 1;"));
        assert!(SYS.contains("const PR_CAP_AMBIENT_RAISE: usize = 2;"));
        assert!(SYS.contains("const PR_CAP_AMBIENT_CLEAR_ALL: usize = 4;"));
        assert_eq!(SYS.matches("const PR_").count(), 11);
        assert_eq!(SYS.matches("const PR_CAP_AMBIENT_").count(), 3);
        assert!(SYS.contains("pub const CAP_SETPCAP: u32 = 8;"));
        assert!(SYS.contains("pub const CAP_SYS_ADMIN: u32 = 21;"));
        assert!(SYS.contains("pub(crate) const SIGKILL: i32 = 9;"));
        assert!(SYS.contains("pub(crate) const SIGTERM: i32 = 15;"));
        assert!(SYS.contains(
            "pub fn terminate_namespace() -> io::Result<()> {\n    signal_namespace(SIGTERM)\n}"
        ));
        assert!(SYS.contains(
            "pub fn kill_namespace() -> io::Result<()> {\n    signal_namespace(SIGKILL)\n}"
        ));
        assert_eq!(shipped_sys.matches("SYS_KILL,").count(), 1);
        assert_eq!(shipped_sys.matches("signal_namespace(").count(), 3);
        assert!(SYS.contains("std::ptr::from_mut(&mut data) as usize"));
        assert!(SYS.contains("std::ptr::from_ref(&data) as usize"));
        assert!(SYS.contains("check(syscall5(SYS_UNSHARE, flags, 0, 0, 0, 0))"));
        assert!(SYS.contains("const SECCOMP_SET_MODE_FILTER: usize = 1;"));
        assert!(SYS.contains("const SIOCGIFFLAGS: usize = 0x8913;"));
        assert!(SYS.contains("const SIOCSIFFLAGS: usize = 0x8914;"));
        assert!(SYS.contains("const IFF_UP: i16 = 0x1;"));
        assert_eq!(shipped_sys.matches("SYS_IOCTL,").count(), 2);
        assert_eq!(shipped_sys.matches("SYS_SETSID,").count(), 1);
        assert_eq!(shipped_sys.matches("SIOCGIFFLAGS,").count(), 1);
        assert_eq!(shipped_sys.matches("SIOCSIFFLAGS,").count(), 1);
        assert_eq!(shipped_sys.matches("read_interface_flags(fd,").count(), 2);
        assert_eq!(shipped_sys.matches("write_interface_flags(fd,").count(), 1);
        assert_eq!(shipped_sys.matches("SYS_PRLIMIT64,").count(), 2);
        assert!(SYS.contains("const RLIMIT_DATA: usize = 2;"));
        assert!(SYS.contains("struct Rlimit64"));
        assert!(SYS.contains("std::mem::size_of::<Rlimit64>(), 16"));
        assert!(!shipped_sys.contains("request: usize"));
        assert!(SYS.contains("std::mem::size_of::<IfreqFlags>(), 40"));
        assert!(SYS.contains("SYS_SECCOMP,\n        SECCOMP_SET_MODE_FILTER,\n        0,"));
        assert!(TRANSITION.contains("sys::MS_REC | sys::MS_PRIVATE"));
        let reaper_mount = TRANSITION
            .split_once("fn mount_reaper_probe")
            .unwrap()
            .1
            .split_once("fn last_capability")
            .unwrap()
            .0;
        assert!(
            reaper_mount.contains("sys::MS_BIND | sys::MS_NOSUID | sys::MS_NODEV,")
        );
        assert!(TRANSITION.contains(
            "REAPER_PROBE_PATH,\n            None,\n            &[\"ro\", \"nosuid\", \"nodev\"],\n            &[\"rw\", \"noexec\"],"
        ));
        assert!(!reaper_mount.contains("fs::copy"));
    }

    #[test]
    fn transition_is_the_only_syscall_caller() {
        let shipped_main = MAIN.split_once("#[cfg(test)]").unwrap().0;
        assert_eq!(
            TRANSITION.matches("sys::unshare_namespaces(true)?;").count(),
            1
        );
        assert_eq!(TRANSITION.matches("sys::unshare_namespaces(").count(), 2);
        assert!(TRANSITION.contains(
            "sys::unshare_namespaces(application.isolate_network).map_err(|error|"
        ));
        assert!(TRANSITION.contains(
            ".require_application_change(&before, application.isolate_network)?;"
        ));
        assert!(TRANSITION.contains(
            "if application.isolate_network {\n            sys::bring_up_loopback()"
        ));
        assert!(TRANSITION.contains(
            "let flags = sys::MS_BIND\n        | if source_kind == FilesystemSourceKind::Directory {\n            sys::MS_REC"
        ));
        assert_eq!(TRANSITION.matches("mount_bind_kind(").count(), 3);
        assert_eq!(
            TRANSITION
                .matches("let flags = grant_mount_policy_flags(read_only || row.read_only);")
                .count(),
            1
        );
        let grant_flags = TRANSITION
            .split_once("fn grant_mount_policy_flags")
            .unwrap()
            .1
            .split_once("fn require_grant_mount_policy")
            .unwrap()
            .0;
        assert!(grant_flags.contains(
            "sys::MS_REMOUNT | sys::MS_BIND | sys::MS_NOSUID | sys::MS_NODEV | sys::MS_NOEXEC"
        ));
        assert!(grant_flags.contains("flags |= sys::MS_RDONLY;"));
        for call in [
            "sys::close(",
            "sys::bring_up_loopback(",
            "sys::wait_any(",
            "sys::kill_namespace(",
            "sys::start_new_session(",
            "sys::mount(",
            "sys::pivot_root(",
            "sys::umount_detach(",
            "sys::capabilities(",
            "sys::set_capabilities(",
            "sys::clear_ambient_capabilities(",
            "sys::set_parent_death_signal(",
            "sys::raise_ambient_sys_admin(",
            "sys::ambient_capability(",
            "sys::drop_bounding_capability(",
            "sys::bounding_capability(",
            "sys::set_no_new_privileges(",
            "sys::no_new_privileges(",
            "sys::set_dumpable(",
            "sys::dumpable(",
            "sys::set_and_require_data_limit(",
            "sys::install_seccomp_filter(",
        ] {
            assert!(TRANSITION.contains(call), "missing syscall caller: {call}");
        }
        assert!(TRANSITION.contains("sys::terminate_namespace,"));
        assert!(!shipped_main.contains("sys::"));
        assert!(!AUTHORITY.contains("sys::"));
        assert!(!CGROUP.contains("sys::"));
        assert!(!TRANSITION.contains("pre_exec"));
        assert!(!TRANSITION.contains("use std::os::unix::process::CommandExt;"));
        assert!(!TRANSITION.contains(".process_group(0)"));
        assert!(!TRANSITION.contains("fork("));
        assert!(TRANSITION.contains("const TEST_LEAK_ENV: &str = \"TD_JAIL_TEST_LEAK_FD\";"));
        assert!(TRANSITION.contains(".into_raw_fd()"));
        assert!(TRANSITION.contains("require_descriptor_closed(descriptor)?;"));
        assert!(TRANSITION.contains("clear_and_require_empty_capabilities()?;"));
        assert!(TRANSITION.contains("install_standard_seccomp_filter().map_err(|error|"));
        assert!(TRANSITION.contains("probe_pid1_lifecycle()?;"));
        assert_eq!(TRANSITION.matches(".env_clear()").count(), 4);
        assert_eq!(TRANSITION.matches(".envs(").count(), 1);
        assert!(SECCOMP.contains("pub(crate) const STANDARD_FILTER:"));
        assert!(SECCOMP.contains("const OFFSET_NR: u32 = 0;"));
        assert!(SECCOMP.contains("const OFFSET_ARCH: u32 = 4;"));
        assert!(SECCOMP.contains("const OFFSET_ARG0_LOW: u32 = 16;"));
        assert!(SECCOMP.contains("const OFFSET_ARG1_LOW: u32 = 24;"));
    }

    #[test]
    fn syscall_abi_has_all_five_arguments_and_memory_is_not_hidden() {
        for register in [
            "inlateout(\"rax\") n as isize => ret",
            "in(\"rdi\") a1",
            "in(\"rsi\") a2",
            "in(\"rdx\") a3",
            "in(\"r10\") a4",
            "in(\"r8\") a5",
            "out(\"rcx\") _",
            "out(\"r11\") _",
        ] {
            assert!(
                SYS.contains(register),
                "missing syscall ABI pin: {register}"
            );
        }
        assert!(SYS.contains("options(nostack)"));
        assert!(!SYS.contains("options(nostack, nomem)"));
    }
}

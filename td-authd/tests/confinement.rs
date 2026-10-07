#![allow(clippy::unwrap_used, clippy::indexing_slicing)]
use std::path::Path;

#[test]
fn the_production_source_and_raw_boundary_are_closed() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let manifest = std::fs::read_to_string(root.join("Cargo.toml")).unwrap();
    assert!(manifest.contains("autotests = false"));
    assert_eq!(manifest.matches("[[example]]").count(), 1);
    assert!(manifest
        .contains("name = \"terminal-launch-vm\"\npath = \"tests/launch_vm.rs\"\ntest = true"));
    for target in ["[[bin]]", "[[test]]", "[[bench]]", "[lib]"] {
        assert!(!manifest.contains(target));
    }
    let mut files: Vec<String> = std::fs::read_dir(root.join("src"))
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            assert!(
                entry.file_type().unwrap().is_file(),
                "nested source directory"
            );
            entry.file_name().into_string().unwrap()
        })
        .collect();
    files.sort();
    assert_eq!(
        files,
        [
            "application.rs",
            "application_files.rs",
            "application_shell.rs",
            "channel.rs",
            "consent.rs",
            "deployment.rs",
            "disk_install.rs",
            "elevation.rs",
            "inspection.rs",
            "launch.rs",
            "login.rs",
            "login_status.rs",
            "main.rs",
            "mount_sys.rs",
            "portal_files.rs",
            "primary_account.rs",
            "rollback.rs",
            "secret_intake.rs",
            "secret_request.rs",
            "secret_sys.rs",
            "session.rs",
            "shell_channel.rs",
            "sys.rs",
            "terminal.rs",
            "terminal_sys.rs",
            "unlock.rs"
        ]
    );
    for (name, count) in [
        ("application.rs", 0),
        ("application_files.rs", 0),
        ("application_shell.rs", 0),
        ("shell_channel.rs", 0),
        ("terminal.rs", 0),
        ("terminal_sys.rs", 4),
        ("main.rs", 1),
        ("channel.rs", 0),
        ("consent.rs", 0),
        ("deployment.rs", 0),
        ("disk_install.rs", 0),
        ("elevation.rs", 0),
        ("rollback.rs", 0),
        ("sys.rs", 4),
        ("launch.rs", 0),
        ("unlock.rs", 0),
        ("login.rs", 0),
        ("login_status.rs", 0),
        ("session.rs", 0),
        ("secret_intake.rs", 0),
        ("secret_request.rs", 0),
        ("secret_sys.rs", 4),
        ("inspection.rs", 0),
        ("mount_sys.rs", 4),
        ("portal_files.rs", 0),
        ("primary_account.rs", 0),
    ] {
        let source = std::fs::read_to_string(root.join("src").join(name)).unwrap();
        assert_eq!(source.matches("unsafe").count(), count, "{name}");
        // The first sender is authoritative only when trusted startup has
        // not delegated its inherited endpoint before the greeting.
        for forbidden in [
            "::Command",
            "::thread",
            ".spawn(",
            ".exec(",
            "fork(",
            "cfg_attr",
            "/*",
            "include!",
            "include_str!",
            "include_bytes!",
            "println!",
            "eprintln!",
        ] {
            let child_api = matches!(
                name,
                "deployment.rs"
                    | "disk_install.rs"
                    | "launch.rs"
                    | "application.rs"
                    | "unlock.rs"
                    | "login.rs"
                    | "rollback.rs"
                    | "session.rs"
                    | "inspection.rs"
                    | "application_shell.rs"
                    | "shell_channel.rs"
                    | "terminal.rs"
            ) && ["::Command", "::thread", ".spawn(", ".exec("]
                .contains(&forbidden);
            let mapping_child_api = matches!(name, "portal_files.rs" | "application_files.rs")
                && ["::Command", ".spawn("].contains(&forbidden);
            if !child_api && !mapping_child_api {
                assert!(!source.contains(forbidden), "{name}: {forbidden}");
            }
        }
    }
    assert_eq!(
        fingerprint(include_str!("../src/consent.rs")),
        0x68e38f4ad9c4584d,
        "shared consent changed: reconcile td-secret/src/lib.rs, compositor confinement and this pin"
    );
    assert_eq!(
        fingerprint(
            include_str!("../src/unlock.rs")
                .split("#[cfg(test)]")
                .next()
                .unwrap()
        ),
        0xaa06bfa9fb9de6f8,
        "private unlock supervisor changed"
    );
    let login = include_str!("../src/login.rs")
        .split("#[cfg(test)]")
        .next()
        .unwrap();
    // Writes stay out of production until activation; a login cancel kills
    // and reaps the one fixed worker and never relocks the secret session.
    assert_eq!(
        login
            .matches("pub(crate) const WRITES: bool = cfg!(test);")
            .count(),
        1
    );
    // In login.rs: the `const WRITES` definition and Login::start's check.
    assert_eq!(login.matches("WRITES").count(), 2);
    assert_eq!(
        login.matches("command(\"login-operation\", owner)").count(),
        1
    );
    for forbidden in ["lock-session", "Command::new(", "unlock-operation", "/bin/"] {
        assert!(!login.contains(forbidden), "login.rs: {forbidden}");
    }
    assert_eq!(
        fingerprint(login),
        0x91487acb4fcd817f,
        "login worker supervisor changed"
    );
    assert_eq!(
        fingerprint(
            include_str!("../src/session.rs")
                .split("#[cfg(test)]")
                .next()
                .unwrap()
        ),
        0x26c9c73a09fe3f3c,
        "paired secret controller changed"
    );
    assert_eq!(
        include_str!("../src/session.rs")
            .matches("crate::login::WRITES")
            .count(),
        1
    );
    assert_eq!(
        fingerprint(
            include_str!("../src/inspection.rs")
                .split("#[cfg(test)]")
                .next()
                .unwrap()
        ),
        0xccbc4c8905ecac0b,
        "read-only store controller changed"
    );
    let inspection = include_str!("../src/inspection.rs")
        .split("#[cfg(test)]")
        .next()
        .unwrap();
    // Two fixed read-only helpers, one launch each.
    assert_eq!(inspection.matches("Command::new(").count(), 2);
    assert_eq!(
        inspection
            .matches("Command::new(\"/bin/td-secret\")")
            .count(),
        2
    );
    assert_eq!(inspection.matches("\"inspect-store\"").count(), 1);
    assert_eq!(inspection.matches("\"inspect-login\"").count(), 1);
    // Request 1a: the shared predicate first, the helper only through the
    // inspection launch, and nothing it reads names a path from the peer.
    let login_state = include_str!("../src/login_status.rs")
        .split("#[cfg(test)]")
        .next()
        .unwrap();
    assert_eq!(fingerprint(login_state), LOGIN_STATE_FINGERPRINT);
    assert!(login_state.contains("#[path = \"../../td-secret/src/login_state.rs\"]"));
    assert_eq!(login_state.matches("#[path").count(), 1);
    // One copy of the hostname rules, which consent's hostname descriptions
    // share.
    assert!(login_state.contains("\nuse crate::hostname;\n"));
    assert!(include_str!("../src/main.rs")
        .contains("#[path = \"../../td-firstboot/src/hostname.rs\"]\nmod hostname;\n"));
    assert!(include_str!("../src/consent.rs").contains("\nuse super::hostname::Hostname;\n"));
    assert_eq!(login_state.matches("Inspection::login").count(), 1);
    for forbidden in ["Command", "spawn", "/bin/", "write", "remove", "create"] {
        assert!(
            !login_state.contains(forbidden),
            "login_status.rs: {forbidden}"
        );
    }
    assert_eq!(
        fingerprint(include_str!("../../td-secret/src/login_state.rs")),
        SHARED_LOGIN_STATE_FINGERPRINT,
        "shared login-state predicate changed: reconcile td-firstboot and this pin"
    );
    assert_eq!(
        fingerprint(include_str!("../../td-firstboot/src/hostname.rs")),
        SHARED_HOSTNAME_FINGERPRINT,
        "shared hostname rules, which also serve consent's hostname descriptions, changed: reconcile td-compositor's and td-secret's confinement and this pin"
    );
    let installation = include_str!("../src/deployment.rs")
        .split("#[cfg(test)]")
        .next()
        .unwrap();
    assert_eq!(fingerprint(installation), INSTALLATION_FINGERPRINT);
    // Request 19's tier marker (amendment 8): one reviewed shared reader,
    // compiled once at the crate root, reading only the queued deployment's
    // held directory.
    assert_eq!(installation.matches("#[path").count(), 0);
    assert!(installation.contains("\nuse crate::login_tier;\n"));
    assert_eq!(
        include_str!("../src/main.rs")
            .matches("#[path = \"../../td-secret/src/login_tier.rs\"]\n")
            .count(),
        1
    );
    assert_eq!(installation.matches("login_tier::").count(), 1);
    assert!(installation
        .contains("login_tier::read(&self.source, &self.deployment, self.owner, self.give_up)"));
    // The test seam: production sets the budget only from the constant.
    assert_eq!(installation.matches("give_up").count(), 5);
    assert_eq!(installation.matches("give_up: MARKER_GIVE_UP,").count(), 1);
    assert_eq!(installation.matches("give_up: ready.give_up,").count(), 1);
    assert_eq!(
        installation
            .matches("const MARKER_GIVE_UP: Duration = Duration::from_secs(2);")
            .count(),
        1
    );
    assert_eq!(
        fingerprint(
            include_str!("../../td-secret/src/login_tier.rs")
                .split("#[cfg(test)]")
                .next()
                .unwrap()
        ),
        SHARED_LOGIN_TIER_FINGERPRINT,
        "shared tier reader changed: reconcile td-secret's confinement and this pin"
    );
    let session = include_str!("../src/session.rs")
        .split("#[cfg(test)]")
        .next()
        .unwrap();
    assert_eq!(
        session
            .matches("self.login_state.admits(|| ready.reads())")
            .count(),
        1
    );
    assert_eq!(session.matches("vec![0x99, 1]").count(), 1);
    assert_eq!(installation.matches("Command::new(").count(), 1);
    assert!(installation.contains("Command::new(\"/bin/td-update\")"));
    assert!(installation.contains(".args([\"apply-operation\", deployment])"));
    assert!(installation.contains("sender.descriptor.is_some()"));
    for forbidden in [
        "send_descriptor(",
        "create_credential(",
        "seal_credential(",
        "pre_exec",
        "CommandExt",
        "setsid",
        "process_group",
    ] {
        assert!(
            !installation.contains(forbidden),
            "installation: {forbidden}"
        );
    }
    // Request 1d (td-authd/DESIGN.md, "Elevation operations"): the table
    // before the selectors, both before a description; the selectors only
    // through the shared reader's held volume, read again before acting;
    // and one fixed helper on exactly the approved pair.
    let rollback = include_str!("../src/rollback.rs")
        .split("#[cfg(test)]")
        .next()
        .unwrap();
    assert_eq!(fingerprint(rollback), ROLLBACK_FINGERPRINT);
    assert_eq!(rollback.matches("Command::new(").count(), 1);
    assert!(rollback.contains(
        "            Command::new(\"/bin/td-boot\")\n                .args([\"on-volume\", \"rollback\", \"/run/td-update\", current, previous])\n                .env_clear()\n                .current_dir(\"/\")\n                .stdin(Stdio::null())\n                .stdout(Stdio::null())\n                .stderr(Stdio::inherit())\n                .spawn()\n"
    ));
    assert_eq!(rollback.matches(".spawn()").count(), 1);
    assert_eq!(rollback.matches("login_tier::open_volume(").count(), 1);
    assert_eq!(rollback.matches("login_tier::selected(").count(), 2);
    assert_eq!(rollback.matches("login_tier::").count(), 5);
    assert!(rollback
        .contains("        self.committed = true;\n        if !self.selectors.unchanged() {\n"));
    assert!(rollback.contains(
        "    if !table.is_ok_and(|table| table.grants(owner, elevation::Operation::DeployRollback)) {\n        return Err(Refusal::Principal);\n    }\n    Selectors::read(volume).map_err(|_| Refusal::Selectors)\n"
    ));
    assert!(rollback.contains("ApprovalKey::new(bytes.map(|byte| b'2' + byte % 8))"));
    for forbidden in [
        "send_descriptor(",
        "sys::",
        "pre_exec",
        "CommandExt",
        "setsid",
        "process_group",
        ".arg(",
        ".env(",
        "fs::write",
        "remove",
        "rename",
    ] {
        assert!(!rollback.contains(forbidden), "rollback.rs: {forbidden}");
    }
    assert_eq!(
        session
            .matches("crate::rollback::admit(self.owner, table(), volume)")
            .count(),
        1
    );
    assert!(session.contains(
        "            Request::Rollback => self.begin_rollback(\n                crate::elevation::Table::load,\n                Path::new(crate::login_tier::VOLUME),\n            ),\n"
    ));
    let table = include_str!("../src/elevation.rs")
        .split("#[cfg(test)]")
        .next()
        .unwrap();
    assert_eq!(fingerprint(table), ELEVATION_FINGERPRINT);
    for pin in [
        "const DIRECTORY: &str = \"/etc\";",
        "const TABLE: &str = \"td-elevation.tsv\";",
        "const NOFOLLOW: i32 = 0x20000;",
        "const NONBLOCK: i32 = 0x800;",
        "const DIRECTORY_ONLY: i32 = 0x10000;",
        "Self::load_from(Path::new(DIRECTORY), (0, 0))",
        "|| metadata.mode() & 0o7777 != 0o444",
    ] {
        assert!(table.contains(pin), "elevation.rs: {pin}");
    }
    for forbidden in [
        "Command",
        "spawn",
        "write",
        "remove",
        "create",
        "rename",
        "chmod",
        "set_permissions",
    ] {
        assert!(!table.contains(forbidden), "elevation.rs: {forbidden}");
    }
    // The live installer's service: one fixed program and operands, the
    // installer's socket and td-authd's channel as its only descriptors.
    let disk = include_str!("../src/disk_install.rs")
        .split("#[cfg(test)]")
        .next()
        .unwrap();
    assert_eq!(fingerprint(disk), DISK_INSTALL_FINGERPRINT);
    assert_eq!(disk.matches("Command::new(").count(), 1);
    assert!(disk.contains("Command::new(\"/bin/td-install\")"));
    assert!(disk.contains(
        "\"serve\",\n                \"/bin/td-boot\",\n                \"/run/td-media\",\n                TRUSTED_KEY,\n                \"/\",\n                \"/bin/td-firstboot\","
    ));
    assert!(disk.contains("sys::peer_uid(peer).is_ok_and(|uid| uid == self.installer)"));
    assert!(disk.contains("if self.admits(&installer) {"));
    assert!(disk.contains("bind_intake(SOCKET, INSTALLER_UID)"));
    assert_eq!(disk.matches("self.installer").count(), 1);
    for forbidden in [
        "send_descriptor(",
        "sys::receive(",
        "create_credential(",
        "seal_credential(",
        "pre_exec",
        "CommandExt",
        "setsid",
        "process_group",
    ] {
        assert!(!disk.contains(forbidden), "disk installation: {forbidden}");
    }
    // The shared codec is data only, pinned as reviewed.
    let codec = include_str!("../../td-install/src/installation_consent.rs");
    assert_eq!(fingerprint(codec), CONSENT_CODEC_FINGERPRINT);
    for forbidden in [
        "unsafe",
        "std::fs",
        "std::io",
        "std::process",
        "std::net",
        "std::os",
        "std::thread",
        "std::env",
        "extern",
        "asm!",
        "include",
        "#[path",
        "print",
    ] {
        assert!(!codec.contains(forbidden), "consent codec: {forbidden}");
    }
    let intake_raw = include_str!("../src/secret_sys.rs")
        .split("#[cfg(test)]")
        .next()
        .unwrap();
    assert_eq!(intake_raw.matches("#[allow(unsafe_code)]").count(), 2);
    assert_eq!(intake_raw.matches("core::arch::asm!").count(), 1);
    assert_eq!(intake_raw.matches("OwnedFd::from_raw_fd").count(), 1);
    assert_eq!(intake_raw.matches("const SYS_").count(), 7);
    for constant in [
        "const SYS_POLL: usize = 7;",
        "const SYS_SENDMSG: usize = 46;",
        "const SYS_RECVMSG: usize = 47;",
        "const SYS_SETSOCKOPT: usize = 54;",
        "const SYS_GETSOCKOPT: usize = 55;",
        "const SYS_FCNTL: usize = 72;",
        "const SYS_MEMFD_CREATE: usize = 319;",
        "const F_ADD_SEALS: usize = 1033;",
        "const F_GET_SEALS: usize = 1034;",
        "const REQUIRED_SEALS: usize = 15;",
        "const MEMFD_FLAGS: usize = 3;",
        "const MSG_NOSIGNAL: usize = 0x4000;",
        "const SO_PASSCRED: usize = 16;",
        "const SO_PASSPIDFD: usize = 76;",
        "const SCM_RIGHTS: i32 = 1;",
        "const SCM_CREDENTIALS: i32 = 2;",
        "const SCM_PIDFD: i32 = 4;",
        "const CONTROL: usize = 128;",
        "const MSG_CMSG_CLOEXEC: usize = 0x4000_0000;",
    ] {
        assert!(intake_raw.contains(constant), "{constant}");
    }
    assert_eq!(fingerprint(intake_raw), INTAKE_RAW_FINGERPRINT);
    assert_eq!(
        fingerprint(
            include_str!("../src/secret_intake.rs")
                .split("#[cfg(test)]")
                .next()
                .unwrap()
        ),
        INTAKE_FINGERPRINT
    );
    assert_eq!(
        fingerprint(
            include_str!("../src/secret_request.rs")
                .split("#[cfg(test)]")
                .next()
                .unwrap()
        ),
        WRITE_REQUEST_FINGERPRINT
    );
    let application = include_str!("../src/application.rs")
        .split("#[cfg(test)]")
        .next()
        .unwrap();
    assert_eq!(
        fingerprint(application),
        0x3868affab8b1a69c,
        "application launch controller changed"
    );
    for forbidden in [
        "thread::spawn",
        "thread::Builder",
        "thread::scope",
        "pre_exec",
        ".uid(",
        ".gid(",
        ".groups(",
    ] {
        assert!(
            !application.contains(forbidden),
            "application.rs: {forbidden}"
        );
    }
    assert_eq!(application.matches(".spawn(").count(), 1);
    assert_eq!(application.matches(".exec()").count(), 2);
    assert_eq!(application.matches(".stdin(Stdio::null())").count(), 1);
    assert_eq!(application.matches(".stdin(input)").count(), 1);
    assert_eq!(application.matches(".stderr(Stdio::null())").count(), 2);
    assert_eq!(application.matches(".stdout(Stdio::null())").count(), 1);
    let launch = include_str!("../src/launch.rs");
    for forbidden in [
        "thread::spawn",
        "thread::Builder",
        "thread::scope",
        "pre_exec",
        ".uid(",
        ".gid(",
        ".groups(",
    ] {
        assert!(!launch.contains(forbidden), "launch.rs: {forbidden}");
    }
    assert_eq!(launch.matches("thread::sleep(").count(), 1);
    assert_eq!(launch.matches(".spawn(").count(), 1);
    assert_eq!(launch.matches(".exec()").count(), 1);
    assert_eq!(launch.matches(".process_group(0)").count(), 1);
    assert_eq!(launch.matches("Command::new(").count(), 4);
    assert_eq!(launch.matches(".stdin(Stdio::null())").count(), 1);
    assert_eq!(launch.matches(".stdout(Stdio::null())").count(), 1);
    assert_eq!(launch.matches(".stderr(Stdio::null())").count(), 1);
    // A PIN request lives on only in its clearing owner.
    assert_eq!(launch.matches("bytes.fill(0);").count(), 1);
    assert_eq!(fingerprint(launch), LAUNCH_FINGERPRINT);
    let main = include_str!("../src/main.rs");
    assert!(main.starts_with("#![deny(unsafe_code)]"));
    assert_eq!(main.matches("mod channel;").count(), 1);
    assert_eq!(main.matches("mod sys;").count(), 1);
    let raw = include_str!("../src/sys.rs");
    assert!(raw.contains("#[allow(unsafe_code)]\nfn syscall5("));
    assert!(raw.contains("#[allow(unsafe_code)]\nfn adopt("));
    assert_eq!(raw.matches("core::arch::asm!").count(), 1);
    assert_eq!(raw.matches("OwnedFd::from_raw_fd").count(), 1);
    for constant in [
        "const SYS_POLL: usize = 7;",
        "const SYS_RECVMSG: usize = 47;",
        "const SYS_SETSOCKOPT: usize = 54;",
        "const SYS_GETSOCKOPT: usize = 55;",
        "const SO_PASSCRED: usize = 16;",
        "const SO_PASSPIDFD: usize = 76;",
        "const SO_PEERCRED: usize = 17;",
        "const SOL_SOCKET: usize = 1;",
        "const SCM_PIDFD: i32 = 4;",
        "const SCM_CREDENTIALS: i32 = 2;",
        "const SCM_RIGHTS: i32 = 1;",
        "const CONTROL: usize = 128;",
        "const MSG_CMSG_CLOEXEC: usize = 0x4000_0000;",
    ] {
        assert!(raw.contains(constant), "{constant}");
    }
    assert_eq!(raw.matches("const SYS_").count(), 4);
    assert_eq!(raw.matches("syscall5(").count(), 5);
    assert_eq!(raw.matches("adopt(").count(), 2);
    let mounts = include_str!("../src/mount_sys.rs");
    assert_eq!(
        fingerprint(mounts),
        0x5453b70c86c2349b,
        "fixed mount boundary changed"
    );
    for constant in [
        "const SYS_UNSHARE: usize = 272;",
        "const SYS_OPEN_TREE: usize = 428;",
        "const SYS_MOVE_MOUNT: usize = 429;",
        "const SYS_MOUNT_SETATTR: usize = 442;",
        "const CLONE_NEWUSER: usize = 0x1000_0000;",
        "const AT_EMPTY_PATH: usize = 0x1000;",
        "const OPEN_TREE_FLAGS: usize = 1 | 0x80000 | AT_EMPTY_PATH;",
        "const PORTAL_ATTRIBUTES: u64 = 0x100000 | 1 | 2 | 4 | 8;",
        "const APPLICATION_ATTRIBUTES: u64 = 0x100000 | 2 | 4 | 8;",
        "const MOVE_FLAGS: usize = 4 | 0x40;",
    ] {
        assert!(mounts.contains(constant), "{constant}");
    }
    assert_eq!(mounts.matches("const SYS_").count(), 4);
    assert_eq!(mounts.matches("core::arch::asm!").count(), 1);
    assert_eq!(mounts.matches("OwnedFd::from_raw_fd").count(), 1);
    assert_eq!(mounts.matches("#[allow(unsafe_code)]").count(), 2);
    assert_eq!(mounts.matches("syscall5(").count(), 5);
    assert_eq!(mounts.matches("adopt(").count(), 2);
    let files = include_str!("../src/portal_files.rs");
    assert_eq!(
        fingerprint(files),
        0xfe21b1ca9dfaac8a,
        "root portal grant controller changed"
    );
    assert_eq!(files.matches("Command::new(\"/bin/td-authd\")").count(), 1);
    // The Downloads view and the Firefox handoff view, each its own fixed path.
    assert_eq!(files.matches("Command::new(\"/bin/umount\")").count(), 2);
    assert!(files.contains(".arg(VIEW)"));
    assert!(files.contains(".arg(HANDOFF_VIEW)"));
    assert_eq!(files.matches(".spawn(").count(), 1);
    assert_eq!(
        files.matches("launch::require_launch_startup()?").count(),
        5
    );
    assert_eq!(files.matches("mount_sys::new_user_namespace(").count(), 1);
    assert_eq!(files.matches("mount_sys::clone_directory(").count(), 2);
    assert_eq!(files.matches("mount_sys::portal_attributes(").count(), 1);
    // Only the handoff view is writable, and only Firefox's UID maps to it.
    assert_eq!(
        files.matches("mount_sys::application_attributes(").count(),
        1
    );
    assert_eq!(files.matches("namespace_mapping(uid, PORTAL)").count(), 1);
    assert_eq!(files.matches("admitted_uid(\"firefox\")").count(), 1);
    assert_eq!(files.matches("mount_sys::publish(").count(), 2);
    let application_files = include_str!("../src/application_files.rs");
    assert_eq!(fingerprint(application_files), 0x7e525df007db68ab);
    assert_eq!(
        application_files
            .matches("mount_sys::clone_directory(")
            .count(),
        1
    );
    assert_eq!(
        application_files
            .matches("mount_sys::application_attributes(")
            .count(),
        1
    );
    assert_eq!(application_files.matches("mount_sys::publish(").count(), 1);
    assert_eq!(
        application_files
            .matches("application::admitted_uid(")
            .count(),
        2
    );
    assert_eq!(
        application_files
            .matches("Command::new(\"/bin/umount\")")
            .count(),
        1
    );
    assert_eq!(
        fingerprint(
            include_str!("../src/application_shell.rs")
                .split("#[cfg(test)]")
                .next()
                .unwrap()
        ),
        0x71973ab92448b6e4,
        "application_shell.rs: production boundary changed"
    );
    assert_eq!(
        fingerprint(
            include_str!("../src/shell_channel.rs")
                .split("#[cfg(test)]")
                .next()
                .unwrap()
        ),
        0x8cf83d4d6ed3f6a7,
        "shell_channel.rs: production boundary changed"
    );
    assert_eq!(
        fingerprint(
            include_str!("../src/terminal.rs")
                .split("#[cfg(test)]")
                .next()
                .unwrap()
        ),
        0x06cfa9e717caa1e0,
        "terminal.rs: production boundary changed"
    );
    assert_eq!(
        fingerprint(
            include_str!("../src/terminal_sys.rs")
                .split("#[cfg(test)]")
                .next()
                .unwrap()
        ),
        0xdbf1730949a19a6b,
        "terminal_sys.rs: production boundary changed"
    );
    let terminal_raw = include_str!("../src/terminal_sys.rs");
    assert_eq!(terminal_raw.matches("#[allow(unsafe_code)]").count(), 2);
    assert_eq!(terminal_raw.matches("core::arch::asm!").count(), 1);
    assert_eq!(terminal_raw.matches("File::from_raw_fd").count(), 1);
    assert_eq!(terminal_raw.matches("const SYS_").count(), 2);
    assert_eq!(terminal_raw.matches("const TIOC").count(), 4);
    assert_eq!(terminal_raw.matches("const TC").count(), 2);
    let channel = include_str!("../src/channel.rs");
    assert_eq!(channel.matches("sys::prepare(").count(), 1);
    assert_eq!(channel.matches("sys::receive(").count(), 1);
    assert_eq!(channel.matches("sys::alive(").count(), 2);
    assert!(channel.contains("Self::connect(stream, expected_uid, 0)"));
    assert_eq!(channel.matches("Self::connect(").count(), 1);
    assert_eq!(channel.matches("fn connect(").count(), 1);
    assert!(!main.contains("sys::"));
    // Whole-source pin makes argument layout and adoption provenance an edit
    // to the review record rather than slack in keyword counts.
    let digest = raw.bytes().fold(0xcbf29ce484222325u64, |hash, byte| {
        (hash ^ byte as u64).wrapping_mul(0x100000001b3)
    });
    assert_eq!(digest, RAW_FINGERPRINT);
    // Pin startup as well as raw code: aliases can evade API-name scans.
    assert_eq!(
        fingerprint(main),
        0x88f13b690494563e,
        "main.rs: production startup changed"
    );
    assert_eq!(
        fingerprint(channel),
        0xdf20e4130b2d96e2,
        "channel.rs: production startup changed"
    );
}

const RAW_FINGERPRINT: u64 = 0x42363c39df98214d;

fn fingerprint(source: &str) -> u64 {
    source.bytes().fold(0xcbf29ce484222325u64, |hash, byte| {
        (hash ^ byte as u64).wrapping_mul(0x100000001b3)
    })
}

const LOGIN_STATE_FINGERPRINT: u64 = 0xf7d4c1e582d5ed83;
const SHARED_LOGIN_STATE_FINGERPRINT: u64 = 0x82d5e067ac0d3cb6;
const SHARED_HOSTNAME_FINGERPRINT: u64 = 0x49026f28c1db76ec;

const LAUNCH_FINGERPRINT: u64 = 0x1bb53c4921507828;

const INTAKE_RAW_FINGERPRINT: u64 = 0x320c8b6ddbfe29af;
const INTAKE_FINGERPRINT: u64 = 0xe2f50441f71b4c76;
const WRITE_REQUEST_FINGERPRINT: u64 = 0x188c619caba6ceb8;

const INSTALLATION_FINGERPRINT: u64 = 0xef204bb2bf5e35a3;
const SHARED_LOGIN_TIER_FINGERPRINT: u64 = 0x776bbefe45e5b0c4;
const ROLLBACK_FINGERPRINT: u64 = 0xcd1fcb60e072626d;
const ELEVATION_FINGERPRINT: u64 = 0x3a8baf08764bbaaf;
const DISK_INSTALL_FINGERPRINT: u64 = 0x4dffa721ec6b8471;
const CONSENT_CODEC_FINGERPRINT: u64 = 0x19d3fcff02c2bb2a;

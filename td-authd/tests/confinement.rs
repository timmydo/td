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
            "inspection.rs",
            "launch.rs",
            "main.rs",
            "mount_sys.rs",
            "portal_files.rs",
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
        ("sys.rs", 4),
        ("launch.rs", 0),
        ("unlock.rs", 0),
        ("session.rs", 0),
        ("secret_intake.rs", 0),
        ("secret_request.rs", 0),
        ("secret_sys.rs", 4),
        ("inspection.rs", 0),
        ("mount_sys.rs", 4),
        ("portal_files.rs", 0),
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
                "launch.rs" | "application.rs" | "unlock.rs" | "session.rs" | "inspection.rs" | "application_shell.rs" | "shell_channel.rs" | "terminal.rs"
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
        0x8105ec9fbaf8b219,
        "shared consent changed: reconcile td-secret/src/main.rs, compositor confinement and this pin"
    );
    assert_eq!(
        fingerprint(
            include_str!("../src/unlock.rs")
                .split("#[cfg(test)]")
                .next()
                .unwrap()
        ),
        0xd201f6e066158e30,
        "private unlock supervisor changed"
    );
    assert_eq!(
        fingerprint(
            include_str!("../src/session.rs")
                .split("#[cfg(test)]")
                .next()
                .unwrap()
        ),
        0x70b54b8756abcb88,
        "paired secret controller changed"
    );
    assert_eq!(
        fingerprint(
            include_str!("../src/inspection.rs")
                .split("#[cfg(test)]")
                .next()
                .unwrap()
        ),
        0x13e77bb9effa0802,
        "read-only store controller changed"
    );
    let intake_raw = include_str!("../src/secret_sys.rs").split("#[cfg(test)]").next().unwrap();
    assert_eq!(intake_raw.matches("#[allow(unsafe_code)]").count(), 2);
    assert_eq!(intake_raw.matches("core::arch::asm!").count(), 1);
    assert_eq!(intake_raw.matches("OwnedFd::from_raw_fd").count(), 1);
    assert_eq!(intake_raw.matches("const SYS_").count(), 7);
    for constant in [
        "const SYS_POLL: usize = 7;", "const SYS_SENDMSG: usize = 46;",
        "const SYS_RECVMSG: usize = 47;", "const SYS_SETSOCKOPT: usize = 54;",
        "const SYS_GETSOCKOPT: usize = 55;", "const SYS_FCNTL: usize = 72;",
        "const SYS_MEMFD_CREATE: usize = 319;", "const F_ADD_SEALS: usize = 1033;",
        "const F_GET_SEALS: usize = 1034;", "const REQUIRED_SEALS: usize = 15;",
        "const MEMFD_FLAGS: usize = 3;", "const MSG_NOSIGNAL: usize = 0x4000;",
        "const SO_PASSCRED: usize = 16;", "const SO_PASSPIDFD: usize = 76;",
        "const SCM_RIGHTS: i32 = 1;", "const SCM_CREDENTIALS: i32 = 2;",
        "const SCM_PIDFD: i32 = 4;", "const CONTROL: usize = 128;",
        "const MSG_CMSG_CLOEXEC: usize = 0x4000_0000;",
    ] { assert!(intake_raw.contains(constant), "{constant}"); }
    assert_eq!(fingerprint(intake_raw), INTAKE_RAW_FINGERPRINT);
    assert_eq!(fingerprint(include_str!("../src/secret_intake.rs").split("#[cfg(test)]").next().unwrap()), INTAKE_FINGERPRINT);
    assert_eq!(fingerprint(include_str!("../src/secret_request.rs").split("#[cfg(test)]").next().unwrap()), WRITE_REQUEST_FINGERPRINT);
    let application = include_str!("../src/application.rs")
        .split("#[cfg(test)]")
        .next()
        .unwrap();
    assert_eq!(
        fingerprint(application),
        0x273ae2cfcd07f646,
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
    assert_eq!(launch.matches("Command::new(").count(), 3);
    assert_eq!(launch.matches(".stdin(Stdio::null())").count(), 1);
    assert_eq!(launch.matches(".stdout(Stdio::null())").count(), 1);
    assert_eq!(launch.matches(".stderr(Stdio::null())").count(), 1);
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
        0x682ccae6552948c7,
        "root portal grant controller changed"
    );
    assert_eq!(files.matches("Command::new(\"/bin/td-authd\")").count(), 1);
    assert_eq!(files.matches("Command::new(\"/bin/umount\")").count(), 1);
    assert!(files.contains(".arg(VIEW)"));
    assert_eq!(files.matches(".spawn(").count(), 1);
    assert_eq!(
        files.matches("launch::require_launch_startup()?").count(),
        3
    );
    assert_eq!(files.matches("mount_sys::new_user_namespace(").count(), 1);
    assert_eq!(files.matches("mount_sys::clone_directory(").count(), 1);
    assert_eq!(files.matches("mount_sys::portal_attributes(").count(), 1);
    assert_eq!(files.matches("mount_sys::publish(").count(), 1);
    let application_files = include_str!("../src/application_files.rs");
    assert_eq!(fingerprint(application_files), 0x3cec66b5b2025b31);
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
    assert_eq!(fingerprint(include_str!("../src/application_shell.rs").split("#[cfg(test)]").next().unwrap()), 0x96b87ab192ae992d, "application_shell.rs: production boundary changed");
    assert_eq!(fingerprint(include_str!("../src/shell_channel.rs").split("#[cfg(test)]").next().unwrap()), 0x8cf83d4d6ed3f6a7, "shell_channel.rs: production boundary changed");
    assert_eq!(fingerprint(include_str!("../src/terminal.rs").split("#[cfg(test)]").next().unwrap()), 0x06cfa9e717caa1e0, "terminal.rs: production boundary changed");
    assert_eq!(fingerprint(include_str!("../src/terminal_sys.rs").split("#[cfg(test)]").next().unwrap()), 0xdbf1730949a19a6b, "terminal_sys.rs: production boundary changed");
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
        0xd94d49486e0dfec7,
        "main.rs: production startup changed"
    );
    assert_eq!(
        fingerprint(channel),
        0xbad9a1ce43bb1449,
        "channel.rs: production startup changed"
    );
}

const RAW_FINGERPRINT: u64 = 0x42363c39df98214d;

fn fingerprint(source: &str) -> u64 {
    source.bytes().fold(0xcbf29ce484222325u64, |hash, byte| {
        (hash ^ byte as u64).wrapping_mul(0x100000001b3)
    })
}

const LAUNCH_FINGERPRINT: u64 = 0x67f04d92df199317;

const INTAKE_RAW_FINGERPRINT: u64 = 0x320c8b6ddbfe29af;
const INTAKE_FINGERPRINT: u64 = 0xe2f50441f71b4c76;
const WRITE_REQUEST_FINGERPRINT: u64 = 0x188c619caba6ceb8;

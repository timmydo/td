//! td-secret: the local credential store and console writer, and the
//! portable vault td-pass links through `pass`. `main.rs` only calls `run`.
#![deny(unsafe_code)]
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic
    )
)]

#[path = "../../td-busd/src/bus_client.rs"]
mod bus_client;
mod client;
#[path = "../../td-authd/src/consent.rs"]
#[allow(dead_code, reason = "shared immutable consent description")]
mod consent;
#[allow(dead_code, reason = "the portal shares the authenticated store reader")]
mod crypto;
mod enrollment_operation;
#[allow(dead_code, reason = "td-fido's transport, with td-secret's worker")]
mod fido_device;
#[allow(dead_code, reason = "shared enrollment and recovery metadata")]
mod fido_metadata;
#[path = "../../td-firstboot/src/hostname.rs"]
#[allow(dead_code, reason = "the shared consent's hostname rules")]
mod hostname;
mod login_operation;
mod login_record;
#[allow(
    dead_code,
    reason = "the shared login-state predicate also serves td-firstboot, td-authd and td-login"
)]
mod login_state;
mod login_store;
#[allow(
    dead_code,
    reason = "the queued-update reader serves td-authd's request 19"
)]
mod login_tier;
#[path = "../../td-busd/src/message.rs"]
#[allow(dead_code, reason = "shared bounded D-Bus codec")]
mod message;
#[path = "../../td-busd/src/name.rs"]
mod name;
mod operation;
mod pin_sys;
mod pin_terminal;
mod portable;
pub use portable::pass;
#[path = "../../td-firstboot/src/principals.rs"]
#[allow(dead_code, reason = "shared immutable session identity loader")]
mod principals;
#[path = "../../td-authd/src/secret_request.rs"]
#[allow(dead_code, reason = "shared public credential request codec")]
mod secret_request;
#[path = "../../td-authd/src/secret_sys.rs"]
#[allow(dead_code, reason = "shared public credential descriptor transport")]
mod secret_sys;
mod set_client;
#[allow(dead_code, reason = "the portal shares the authenticated store reader")]
mod store;
#[allow(
    dead_code,
    reason = "the shared descriptor transport also serves td-portal and td-open"
)]
mod sys;
mod token_check;
#[allow(dead_code, reason = "TPM entry points are shared with the provisioner")]
mod tpm;
#[path = "../../td-busd/src/wire.rs"]
#[allow(dead_code, reason = "shared bounded D-Bus codec")]
mod wire;
mod write_operation;

// td-fido's modules, by the names this crate's modules, and the files
// td-firstboot and td-portal compile beside them, call them.
use td_fido::{fido_ctap, fido_enroll, fido_hid, fido_p256, fido_pin, fido_transaction};

/// The one SHA-256 copy, by the name the shared tier reader uses.
use crypto::sha256;
use std::io;

/// The td-secret command line, `main.rs`'s whole body.
pub fn run(args: &[String]) -> Result<(), String> {
    match args {
        [command, flag]
            if command == "check-portable-token" && flag == "--create-test-credential" =>
        {
            token_check::run()
        }
        [command] if command == "selftest" => crypto::selftest(),
        [command, flag, uid] if command == "write-operation" && flag == "--uid" => {
            write_operation::run(parse_uid(uid)?)
        }
        [command, flag, uid] if command == "enroll-operation" && flag == "--uid" => {
            enrollment_operation::run(parse_uid(uid)?)
        }
        [command, flag, uid] if command == "unlock-operation" && flag == "--uid" => {
            operation::run(parse_uid(uid)?)
        }
        [command, flag, uid] if command == "login-operation" && flag == "--uid" => {
            login_operation::run(parse_uid(uid)?)
        }
        [command, flag, uid] if command == "inspect-store" && flag == "--uid" => {
            inspect(uid, |uid| {
                let state =
                    store::Store::inspect_owned(&store::user_path(uid), uid, store_owner(uid)?)?;
                Ok(vec![0x17, state])
            })
        }
        [command, flag, uid] if command == "inspect-login" && flag == "--uid" => {
            inspect(uid, |uid| {
                login_store::inspection(
                    std::path::Path::new(login_store::DIRECTORY),
                    login_store::Owner::ROOT,
                    uid,
                )
            })
        }
        [command, flag, uid] if command == "lock-session" && flag == "--uid" => {
            store::lock_session(parse_uid(uid)?)
        }
        [command, index, inode, rdev] if command == "hid-worker" => {
            fido_device::worker(index, inode, rdev)
        }
        [command, index, inode, rdev, runtime] if command == "hid-worker-desktop" => {
            fido_device::desktop_worker(index, inode, rdev, runtime)
        }
        [command, name] if command == "get" => {
            let mut secret = client::retrieve(name)?;
            use std::io::Write;
            let mut stdout = io::stdout().lock();
            let result = stdout
                .write_all(&secret)
                .and_then(|()| stdout.flush())
                .map_err(|e| e.to_string());
            secret.fill(0);
            result
        }
        [command, target] if command == "set" => set_client::set(target, consent::Role::Primary),
        [command, flag, target] if command == "set" && flag == "--recovery" => {
            set_client::set(target, consent::Role::Recovery)
        }
        _ => Err(concat!(
            "usage: td-secret set [--recovery] APPLICATION/NAME < credential-input; ",
            "use physical secure attention to enroll or unlock the store; ",
            "manual hardware diagnostic: td-secret check-portable-token --create-test-credential"
        )
        .into()),
    }
}

/// A read-only inspection helper (td-secret/DESIGN.md, "Read-only
/// enrollment-state inspection"): the private operation startup
/// admission, the UID, then the whole result on the stdin socket. A
/// failure writes nothing.
fn inspect(uid: &str, result: impl FnOnce(u32) -> Result<Vec<u8>, String>) -> Result<(), String> {
    let endpoint = operation::startup()?;
    let bytes = result(parse_uid(uid)?)?;
    reply(endpoint, &bytes)
}

/// Writes `bytes` under a two-second write timeout; there is no buffered stdout result.
fn reply(mut endpoint: std::os::unix::net::UnixStream, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write;
    endpoint
        .set_write_timeout(Some(std::time::Duration::from_secs(2)))
        .map_err(|e| e.to_string())?;
    endpoint.write_all(bytes).map_err(|e| e.to_string())
}

fn owned_store(uid: u32) -> Result<store::Store, String> {
    store::Store::open_owned(&store::user_path(uid), uid, store_owner(uid)?, false)
}

fn store_owner(uid: u32) -> Result<u32, String> {
    let registry = principals::Registry::load()?;
    registry.verify_installed_accounts(&registry)?;
    let owner = registry
        .sessions()
        .find(|session| session.owner == uid)
        .ok_or("credential user has no deployment reservation")?
        .portal;
    Ok(owner)
}

fn parse_uid(value: &str) -> Result<u32, String> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err("UID must be a decimal user id".into());
    }
    let uid: u32 = value.parse().map_err(|_| "UID is out of range")?;
    if !(1000..=65533).contains(&uid) || uid.to_string() != value {
        return Err("UID must be a canonical human session id".into());
    }
    Ok(uid)
}

#[cfg(test)]
#[path = "../../td-authd/src/terminal_sys.rs"]
#[allow(dead_code, reason = "PTY allocation for PIN terminal fixtures only")]
mod terminal_fixture_sys;

// td-fido's test-only authenticator side (td-fido/DESIGN.md, "Test
// support"): the virtual authenticator and the transcript fixtures, over a
// test copy of its P-256 that can sign. `fido_uhid` presents a virtual key
// to the guest oracles here.
#[cfg(test)]
use td_fido::{fido_aes, fido_cbor};
#[cfg(test)]
#[path = "../../td-fido/src/fido_fixtures.rs"]
#[allow(dead_code, reason = "td-fido's own tests use the rest")]
mod fido_fixtures;
#[cfg(test)]
mod fido_uhid;
#[cfg(test)]
#[path = "../../td-fido/src/fido_virtual.rs"]
#[allow(dead_code, reason = "td-fido's own tests use the rest")]
mod fido_virtual;
#[cfg(test)]
#[path = "../../td-fido/src/fido_p256.rs"]
#[allow(dead_code, reason = "the virtual authenticator signs with this copy")]
mod p256_signer;

/// Valid descriptions of the operations the approval key confirms,
/// consent tags 5 (`deploy-publish`), 11 and 12, for `owner`. td-secret
/// decodes them through the shared consent codec but performs none, so
/// every worker's operation match must refuse them.
#[cfg(test)]
fn elevation_descriptions(owner: u32) -> Vec<Vec<u8>> {
    let key = consent::ApprovalKey::new(*b"47").unwrap();
    [
        consent::Operation::Install {
            key,
            deployment: "c".repeat(64),
            requester: owner,
        },
        consent::Operation::DeployRollback {
            key,
            current: "a".repeat(64),
            previous: "b".repeat(64),
        },
        consent::Operation::SetHostname {
            key,
            requester: owner,
            old: "td".into(),
            new: "my-laptop".into(),
        },
    ]
    .into_iter()
    .map(|operation| {
        let bytes = consent::Request::new([42; 32], owner, operation)
            .unwrap()
            .encode();
        assert!(consent::Request::decode(&bytes).is_ok());
        bytes
    })
    .collect()
}

#[cfg(test)]
mod confinement {
    #[test]
    fn manual_token_check_requires_explicit_creation_and_has_no_input_arguments() {
        for args in [
            vec!["check-portable-token"],
            vec!["check-portable-token", "--pin", "1234"],
            vec!["check-portable-token", "--create-test-credential", "1234"],
            vec![
                "check-portable-token",
                "--create-test-credential",
                "--device",
                "0",
            ],
        ] {
            assert!(
                super::run(&args.into_iter().map(String::from).collect::<Vec<_>>())
                    .unwrap_err()
                    .starts_with("usage:")
            );
        }
    }

    #[test]
    fn private_unlock_controller_and_shared_description_are_pinned() {
        let fingerprint = |source: &str| {
            source.bytes().fold(0xcbf29ce484222325u64, |hash, byte| {
                (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
            })
        };
        assert_eq!(
            fingerprint(include_str!("operation.rs")),
            0x0c03909956647c23
        );
        assert_eq!(
            fingerprint(include_str!("login_operation.rs")),
            0x965bc4d7d0e1ab83
        );
        assert_eq!(
            fingerprint(include_str!("write_operation.rs")),
            0x5991b168732f47ac
        );
        assert_eq!(
            fingerprint(include_str!("enrollment_operation.rs")),
            0x44a7a283c11747d6
        );
        assert_eq!(fingerprint(include_str!("../../td-authd/src/consent.rs")), 0xef19938ed8d1b8a4, "shared consent changed: reconcile td-authd/tests/confinement.rs and td-compositor/src/main.rs pins");
        // The shared consent's hostname rules: firstboot's one copy.
        let production = include_str!("lib.rs").split("#[cfg(test)]").next().unwrap();
        assert!(production.contains(
            "#[path = \"../../td-firstboot/src/hostname.rs\"]\n#[allow(dead_code, reason = \"the shared consent's hostname rules\")]\nmod hostname;\n"
        ));
        assert_eq!(production.matches("mod hostname;").count(), 1);
        assert_eq!(
            fingerprint(include_str!("../../td-firstboot/src/hostname.rs")),
            0x49026f28c1db76ec,
            "shared hostname rules changed: reconcile td-authd/tests/confinement.rs and td-compositor/src/main.rs pins"
        );
    }

    /// TOKEN-LOGIN.md, "Deployments": the shared tier reader only opens and
    /// reads, never following a link or waiting on a FIFO.
    #[test]
    fn the_tier_reader_only_reads_through_held_descriptors() {
        let tier = include_str!("login_tier.rs")
            .split("#[cfg(test)]")
            .next()
            .unwrap();
        assert!(tier.contains("\n#![forbid(unsafe_code)]\n"));
        for forbidden in [
            "Command",
            "spawn",
            "write",
            "remove",
            "create",
            "rename",
            "set_len",
            "permissions",
            "chown",
            "include",
        ] {
            assert!(!tier.contains(forbidden), "login_tier.rs: {forbidden}");
        }
        for pin in [
            "const NOFOLLOW: i32 = 0o400000;",
            "const NONBLOCK: i32 = 0o4000;",
            "const OPEN_DIRECTORY: i32 = 0o200000;",
            "const FD_ROOT: &str = \"/proc/self/fd\";",
        ] {
            assert_eq!(tier.matches(pin).count(), 1, "{pin}");
        }
        assert_eq!(tier.matches(".custom_flags(").count(), 2);
        assert_eq!(
            tier.matches(".custom_flags(NOFOLLOW | NONBLOCK)").count(),
            1
        );
        assert_eq!(
            tier.matches(".custom_flags(OPEN_DIRECTORY | NOFOLLOW)")
                .count(),
            1
        );
        assert_eq!(tier.matches("OpenOptions::new()").count(), 2);
        assert_eq!(tier.matches(".read(true)").count(), 2);
    }

    #[test]
    fn the_library_exposes_only_the_command_line_and_the_notebook_api() {
        let production = include_str!("lib.rs").split("#[cfg(test)]").next().unwrap();
        let public: Vec<&str> = production
            .lines()
            .filter(|line| line.starts_with("pub"))
            .collect();
        assert_eq!(
            public,
            [
                "pub use portable::pass;",
                "pub fn run(args: &[String]) -> Result<(), String> {"
            ]
        );
        let portable = include_str!("portable.rs")
            .split("#[cfg(test)]")
            .next()
            .unwrap();
        assert_eq!(portable.matches("pub mod ").count(), 1);
        assert!(portable.contains("pub mod pass;"));
        // pass defines what it exposes; it re-exports and nests nothing.
        let pass = include_str!("portable_pass.rs")
            .split("#[cfg(test)]")
            .next()
            .unwrap();
        for widening in ["pub use", "pub mod", "pub(crate)", "macro_export"] {
            assert!(!pass.contains(widening), "{widening}");
        }
        let main = include_str!("main.rs");
        assert_eq!(main.matches("td_secret::").count(), 1);
        assert!(!main.contains("mod "));
    }

    #[test]
    fn inspection_helpers_admit_only_their_exact_argv_after_startup() {
        let run =
            |args: &[&str]| super::run(&args.iter().map(|a| a.to_string()).collect::<Vec<_>>());
        for args in [
            &["inspect-login"][..],
            &["inspect-login", "--uid"],
            &["inspect-login", "--uid", "1000", "1000"],
            &["inspect-login", "--user", "1000"],
            &["inspect-login", "1000", "--uid"],
            &["inspect-login", "--uid=1000"],
            &["--uid", "1000", "inspect-login"],
            &["inspect-logins", "--uid", "1000"],
            &["Inspect-login", "--uid", "1000"],
        ] {
            assert!(run(args).unwrap_err().starts_with("usage:"), "{args:?}");
        }
        // Every UID meets the root, thread, descriptor and socket admission
        // before it is parsed or anything is read; this test process passes
        // none of it.
        let not_root = super::store::require_root().err();
        for command in ["inspect-store", "inspect-login"] {
            for uid in ["1000", "0", "01000", "x"] {
                let error = run(&[command, "--uid", uid]).unwrap_err();
                assert!(!error.starts_with("usage:"), "{command} {uid}");
                assert!(!error.contains("UID"), "{command} {uid}: {error}");
                if let Some(not_root) = &not_root {
                    assert_eq!(&error, not_root, "{command} {uid}");
                }
            }
        }
    }

    #[test]
    fn reply_writes_the_whole_result_and_drops_its_stream() {
        use std::io::Read;
        for result in [
            &[0x1a, 0][..],
            &[0x17, 3],
            &[0x1a; super::login_store::MAX_INSPECTION],
        ] {
            let (endpoint, mut parent) = std::os::unix::net::UnixStream::pair().unwrap();
            super::reply(endpoint, result).unwrap();
            let mut seen = Vec::new();
            // EOF here is reply's drop; in the helper it is process exit.
            parent.read_to_end(&mut seen).unwrap();
            assert_eq!(seen, result);
        }
        // A peer that has gone is a failure, not a silent success.
        let (endpoint, parent) = std::os::unix::net::UnixStream::pair().unwrap();
        drop(parent);
        assert!(super::reply(endpoint, &[0x1a, 0]).is_err());
    }

    #[test]
    fn console_targets_require_one_canonical_human_identity() {
        for value in ["0", "991", "01000", "+1000", "65534", "4294967296", "1000 "] {
            assert!(super::parse_uid(value).is_err(), "{value}");
        }
        for value in ["1000", "65533"] {
            assert_eq!(super::parse_uid(value).unwrap().to_string(), value);
        }
        assert!(super::run(&["set".into(), "mail/main".into()]).is_err());
    }

    #[test]
    fn descriptor_transport_is_the_only_raw_surface() {
        let sources = [
            ("lib.rs", include_str!("lib.rs")),
            ("main.rs", include_str!("main.rs")),
            (
                "bus_client.rs",
                include_str!("../../td-busd/src/bus_client.rs"),
            ),
            ("client.rs", include_str!("client.rs")),
            ("set_client.rs", include_str!("set_client.rs")),
            (
                "secret_request.rs",
                include_str!("../../td-authd/src/secret_request.rs"),
            ),
            (
                "secret_sys.rs",
                include_str!("../../td-authd/src/secret_sys.rs"),
            ),
            ("operation.rs", include_str!("operation.rs")),
            (
                "enrollment_operation.rs",
                include_str!("enrollment_operation.rs"),
            ),
            ("write_operation.rs", include_str!("write_operation.rs")),
            ("crypto.rs", include_str!("crypto.rs")),
            ("portable.rs", include_str!("portable.rs")),
            ("portable_events.rs", include_str!("portable_events.rs")),
            (
                "portable_lifecycle.rs",
                include_str!("portable_lifecycle.rs"),
            ),
            ("portable_host.rs", include_str!("portable_host.rs")),
            ("portable_notebook.rs", include_str!("portable_notebook.rs")),
            ("portable_pass.rs", include_str!("portable_pass.rs")),
            ("portable_store.rs", include_str!("portable_store.rs")),
            ("fido_device.rs", include_str!("fido_device.rs")),
            ("fido_metadata.rs", include_str!("fido_metadata.rs")),
            // td-fido's files this crate's tests compile by path; td-fido
            // forbids unsafe code itself.
            (
                "fido_fixtures.rs",
                include_str!("../../td-fido/src/fido_fixtures.rs"),
            ),
            (
                "fido_p256.rs",
                include_str!("../../td-fido/src/fido_p256.rs"),
            ),
            ("fido_uhid.rs", include_str!("fido_uhid.rs")),
            (
                "fido_virtual.rs",
                include_str!("../../td-fido/src/fido_virtual.rs"),
            ),
            ("login_operation.rs", include_str!("login_operation.rs")),
            ("login_vm.rs", include_str!("login_vm.rs")),
            ("login_system_vm.rs", include_str!("login_system_vm.rs")),
            ("login_record.rs", include_str!("login_record.rs")),
            ("login_state.rs", include_str!("login_state.rs")),
            ("login_store.rs", include_str!("login_store.rs")),
            ("login_tier.rs", include_str!("login_tier.rs")),
            (
                "hostname.rs",
                include_str!("../../td-firstboot/src/hostname.rs"),
            ),
            ("pin_sys.rs", include_str!("pin_sys.rs")),
            ("pin_terminal.rs", include_str!("pin_terminal.rs")),
            ("token_check.rs", include_str!("token_check.rs")),
            ("store.rs", include_str!("store.rs")),
            ("scm.rs", include_str!("scm.rs")),
            ("sys.rs", include_str!("sys.rs")),
            ("system_vm.rs", include_str!("system_vm.rs")),
            ("tpm.rs", include_str!("tpm.rs")),
        ];
        for (name, source) in sources {
            let production = source.split("#[cfg(test)]").next().unwrap();
            for (operation, owners) in [
                ("mode", &["pin_terminal.rs"][..]),
                ("set_mode", &["pin_terminal.rs"]),
                ("readable", &["pin_terminal.rs"]),
                ("protect_process", &["token_check.rs", "portable_host.rs"]),
            ] {
                if production.contains(&format!("pin_sys::{operation}(")) {
                    assert!(
                        owners.contains(&name),
                        "unexpected PIN syscall caller {name}"
                    );
                }
            }
            // UNSAFE.md §15: the two descriptor receivers, beside the
            // transport itself (sys.rs's safe child scm.rs).
            if !matches!(name, "sys.rs" | "scm.rs")
                && (production.contains("recv_with_fds") || production.contains("take_received"))
            {
                assert!(
                    matches!(name, "client.rs" | "portable_events.rs"),
                    "unexpected descriptor receiver {name}"
                );
            }
            let keyword = format!("un{}", "safe");
            let lint = format!("{keyword}_code");
            let raw = production.matches(&keyword).count() - production.matches(&lint).count();
            let scopes = 2 * usize::from(matches!(name, "sys.rs" | "secret_sys.rs"))
                + usize::from(name == "pin_sys.rs");
            assert_eq!(raw, scopes, "{name}");
            assert_eq!(
                production.matches(&format!("#[allow({lint})]")).count(),
                scopes
            );
        }
        let fingerprint = |source: &str| {
            source.bytes().fold(0xcbf29ce484222325u64, |hash, byte| {
                (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
            })
        };
        assert_eq!(
            fingerprint(
                include_str!("../../td-authd/src/secret_sys.rs")
                    .split("#[cfg(test)]")
                    .next()
                    .unwrap()
            ),
            0x320c8b6ddbfe29af,
            "intake raw source changed"
        );
        assert_eq!(
            fingerprint(
                include_str!("../../td-authd/src/secret_request.rs")
                    .split("#[cfg(test)]")
                    .next()
                    .unwrap()
            ),
            0x188c619caba6ceb8,
            "intake request source changed"
        );
        assert_eq!(
            fingerprint(
                include_str!("set_client.rs")
                    .split("#[cfg(test)]")
                    .next()
                    .unwrap()
            ),
            0xf402e082e7175844,
            "credential client changed"
        );
        let sys = include_str!("sys.rs");
        // scm.rs reaches its parent only through these five names: a child
        // module sees every private item above it, so the import line is the
        // boundary, and a raw name, a syscall or an adoption is refused.
        let scm_production = include_str!("scm.rs").split("#[cfg(test)]").next().unwrap();
        assert_eq!(
            scm_production
                .matches("use super::{close_raw, raw_errno, recvmsg, sendmsg, CONTROL_CAPACITY};")
                .count(),
            1
        );
        assert_eq!(scm_production.matches("super::").count(), 1);
        assert!(!scm_production.contains("crate::"));
        assert_eq!(scm_production.matches("recvmsg(").count(), 1);
        assert_eq!(scm_production.matches("sendmsg(").count(), 1);
        for raw in [
            "asm!",
            "syscall",
            "SYS_",
            "from_raw_fd",
            "take_received",
            "ReceivedFd",
            "adopt(",
            "MSG_CMSG_CLOEXEC",
            "MSG_NOSIGNAL",
        ] {
            assert!(!scm_production.contains(raw), "scm.rs names {raw}");
        }
        assert_eq!(sys.matches("core::arch::asm!").count(), 1);
        assert_eq!(sys.matches("const SYS_").count(), 3);
        let production = sys.split("#[cfg(test)]").next().unwrap();
        assert_eq!(production.matches("File::from_raw_fd(").count(), 1);
        assert!(!production.contains("/proc/self/fd"));
        assert!(production.contains(
            r#"#[allow(unsafe_code)]
pub fn take_received(fd: RawFd) -> Result<File, String> {
    if fd < 0 {
        return Err(format!("invalid received descriptor {fd}"));
    }
    // SAFETY: callers pass one live descriptor just installed by recvmsg,
    // removed from its sole disposal queue. File now owns its only close.
    Ok(unsafe { File::from_raw_fd(fd) })
}"#
        ));

        for pin in [
            "const SYS_CLOSE: usize = 3;",
            "const SYS_SENDMSG: usize = 46;",
            "const SYS_RECVMSG: usize = 47;",
            "const MSG_CMSG_CLOEXEC: i32 = 0x4000_0000;",
            "const MSG_NOSIGNAL: i32 = 0x4000;",
        ] {
            assert!(sys.contains(pin));
        }
        let mut actual = std::fs::read_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/src"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .filter(|name| name.ends_with(".rs"))
            .collect::<Vec<_>>();
        actual.sort();
        assert_eq!(
            actual,
            [
                "client.rs",
                "crypto.rs",
                "enrollment_operation.rs",
                "fido_device.rs",
                "fido_metadata.rs",
                "fido_uhid.rs",
                "lib.rs",
                "login_operation.rs",
                "login_record.rs",
                "login_state.rs",
                "login_store.rs",
                "login_system_vm.rs",
                "login_tier.rs",
                "login_vm.rs",
                "main.rs",
                "operation.rs",
                "pin_sys.rs",
                "pin_terminal.rs",
                "portable.rs",
                "portable_events.rs",
                "portable_host.rs",
                "portable_lifecycle.rs",
                "portable_notebook.rs",
                "portable_pass.rs",
                "portable_store.rs",
                "scm.rs",
                "set_client.rs",
                "store.rs",
                "sys.rs",
                "system_vm.rs",
                "token_check.rs",
                "tpm.rs",
                "write_operation.rs"
            ]
        );
    }
}

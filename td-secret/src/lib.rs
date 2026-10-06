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

mod client;
#[path = "../../td-authd/src/consent.rs"]
#[allow(dead_code, reason = "shared immutable consent description")]
mod consent;
#[allow(dead_code, reason = "the portal shares the authenticated store reader")]
mod crypto;
mod enrollment_operation;
#[allow(dead_code, reason = "private CTAP AES and PIN protocol support")]
mod fido_aes;
#[allow(dead_code, reason = "shared CTAP enrollment and assertion codec")]
mod fido_cbor;
#[allow(dead_code, reason = "shared CTAP enrollment and assertion codec")]
mod fido_ctap;
#[allow(dead_code, reason = "shared physical token transport")]
mod fido_device;
#[allow(
    dead_code,
    reason = "enrollment construction and verified assertion support"
)]
mod fido_enroll;
#[allow(dead_code, reason = "shared FIDO2 framing and cancellation codec")]
mod fido_hid;
#[allow(dead_code, reason = "shared enrollment and recovery metadata")]
mod fido_metadata;
#[allow(dead_code, reason = "private P-256 and portable protocol support")]
mod fido_p256;
#[allow(dead_code, reason = "portable PIN protocol and manual token check")]
mod fido_pin;
#[allow(dead_code, reason = "private portable transaction runner")]
mod fido_transaction;
mod login_operation;
mod login_record;
mod login_store;
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
    reason = "shared descriptor transport also sends Wayland files"
)]
mod sys;
mod token_check;
#[allow(dead_code, reason = "TPM entry points are shared with the provisioner")]
mod tpm;
#[path = "../../td-busd/src/wire.rs"]
#[allow(dead_code, reason = "shared bounded D-Bus codec")]
mod wire;
mod write_operation;

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
            let mut endpoint = operation::startup()?;
            let uid = parse_uid(uid)?;
            let state =
                store::Store::inspect_owned(&store::user_path(uid), uid, store_owner(uid)?)?;
            use std::io::Write;
            endpoint
                .set_write_timeout(Some(std::time::Duration::from_secs(2)))
                .map_err(|e| e.to_string())?;
            endpoint
                .write_all(&[0x17, state])
                .map_err(|e| e.to_string())
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

#[cfg(test)]
mod fido_virtual;

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
            0x63b7e28b777306cd
        );
        assert_eq!(
            fingerprint(include_str!("login_operation.rs")),
            0x43c67b9c95ce722d
        );
        assert_eq!(
            fingerprint(include_str!("write_operation.rs")),
            0x2f5050ddd2493660
        );
        assert_eq!(
            fingerprint(include_str!("enrollment_operation.rs")),
            0x30dcb428ed75a535
        );
        assert_eq!(fingerprint(include_str!("../../td-authd/src/consent.rs")), 0x06dce85e4b8b42a3, "shared consent changed: reconcile td-authd/tests/confinement.rs and td-compositor/src/main.rs pins");
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
            ("fido_aes.rs", include_str!("fido_aes.rs")),
            ("fido_cbor.rs", include_str!("fido_cbor.rs")),
            ("fido_ctap.rs", include_str!("fido_ctap.rs")),
            ("fido_device.rs", include_str!("fido_device.rs")),
            ("fido_enroll.rs", include_str!("fido_enroll.rs")),
            ("fido_hid.rs", include_str!("fido_hid.rs")),
            ("fido_metadata.rs", include_str!("fido_metadata.rs")),
            ("fido_p256.rs", include_str!("fido_p256.rs")),
            ("fido_pin.rs", include_str!("fido_pin.rs")),
            ("fido_transaction.rs", include_str!("fido_transaction.rs")),
            ("fido_virtual.rs", include_str!("fido_virtual.rs")),
            ("login_operation.rs", include_str!("login_operation.rs")),
            ("login_record.rs", include_str!("login_record.rs")),
            ("login_store.rs", include_str!("login_store.rs")),
            ("pin_sys.rs", include_str!("pin_sys.rs")),
            ("pin_terminal.rs", include_str!("pin_terminal.rs")),
            ("token_check.rs", include_str!("token_check.rs")),
            ("store.rs", include_str!("store.rs")),
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
            // UNSAFE.md §15: the two descriptor receivers.
            if name != "sys.rs"
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
                "fido_aes.rs",
                "fido_cbor.rs",
                "fido_ctap.rs",
                "fido_device.rs",
                "fido_enroll.rs",
                "fido_hid.rs",
                "fido_metadata.rs",
                "fido_p256.rs",
                "fido_pin.rs",
                "fido_transaction.rs",
                "fido_virtual.rs",
                "lib.rs",
                "login_operation.rs",
                "login_record.rs",
                "login_store.rs",
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

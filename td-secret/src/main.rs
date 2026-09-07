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
mod operation;
mod enrollment_operation;
mod write_operation;
#[allow(dead_code, reason = "shared physical token transport")]
mod fido_device;
#[allow(dead_code, reason = "shared FIDO2 framing and cancellation codec")]
mod fido_hid;
#[allow(dead_code, reason = "shared enrollment and recovery metadata")]
mod fido_metadata;
#[allow(dead_code, reason = "shared CTAP enrollment and assertion codec")]
mod fido_cbor;
#[allow(dead_code, reason = "shared CTAP enrollment and assertion codec")]
mod fido_ctap;
#[allow(dead_code, reason = "enrollment construction and verified assertion support")]
mod fido_enroll;
#[path = "../../td-firstboot/src/principals.rs"]
#[allow(dead_code, reason = "shared immutable session identity loader")]
mod principals;
#[allow(dead_code, reason = "the portal shares the authenticated store reader")]
mod crypto;
#[path = "../../td-busd/src/message.rs"]
#[allow(dead_code, reason = "shared bounded D-Bus codec")]
mod message;
#[path = "../../td-busd/src/name.rs"]
mod name;
#[allow(dead_code, reason = "the portal shares the authenticated store reader")]
mod store;
#[allow(
    dead_code,
    reason = "shared descriptor transport also sends Wayland files"
)]
mod sys;
#[allow(dead_code, reason = "TPM entry points are shared with the provisioner")]
mod tpm;
#[path = "../../td-busd/src/wire.rs"]
#[allow(dead_code, reason = "shared bounded D-Bus codec")]
mod wire;

use std::io::{self, Read};

fn run(args: &[String]) -> Result<(), String> {
    match args {
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
        [command, uid_flag, uid, target] if command == "set" && uid_flag == "--uid" => {
            store::require_root()?;
            let uid = parse_uid(uid)?;
            let (app, name) = store::target(target)?;
            let store = owned_store(uid)?;
            let mut secret = Vec::new();
            io::stdin()
                .take((store::MAX_SECRET + 1) as u64)
                .read_to_end(&mut secret)
                .map_err(|e| format!("read credential from stdin: {e}"))?;
            let result = store.set(app, name, &secret);
            secret.fill(0);
            result?;
            eprintln!("td-secret: credential stored (interim console authorization)");
            Ok(())
        }
        _ => Err(concat!(
            "usage: td-secret set --uid UID APPLICATION/NAME < credential-file; ",
            "use physical secure attention to enroll or unlock the store"
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

fn main() -> std::process::ExitCode {
    match run(&std::env::args().skip(1).collect::<Vec<_>>()) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("td-secret: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod confinement {
    #[test]
    fn private_unlock_controller_and_shared_description_are_pinned() {
        let fingerprint = |source: &str| source.bytes().fold(0xcbf29ce484222325u64,
            |hash, byte| (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3));
        assert_eq!(fingerprint(include_str!("operation.rs")), 0x9e52120007bef08f);
        assert_eq!(fingerprint(include_str!("write_operation.rs")), 0x2f5050ddd2493660);
        assert_eq!(fingerprint(include_str!("enrollment_operation.rs")), 0x30dcb428ed75a535);
        assert_eq!(fingerprint(include_str!("../../td-authd/src/consent.rs")), 0x8105ec9fbaf8b219, "shared consent changed: reconcile td-authd/tests/confinement.rs and td-compositor/src/main.rs pins");
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
            ("main.rs", include_str!("main.rs")),
            ("client.rs", include_str!("client.rs")),
            ("operation.rs", include_str!("operation.rs")),
            ("enrollment_operation.rs", include_str!("enrollment_operation.rs")),
            ("write_operation.rs", include_str!("write_operation.rs")),
            ("crypto.rs", include_str!("crypto.rs")),
            ("fido_cbor.rs", include_str!("fido_cbor.rs")),
            ("fido_ctap.rs", include_str!("fido_ctap.rs")),
            ("fido_device.rs", include_str!("fido_device.rs")),
            ("fido_enroll.rs", include_str!("fido_enroll.rs")),
            ("fido_hid.rs", include_str!("fido_hid.rs")),
            ("fido_metadata.rs", include_str!("fido_metadata.rs")),
            ("store.rs", include_str!("store.rs")),
            ("sys.rs", include_str!("sys.rs")),
            ("tpm.rs", include_str!("tpm.rs")),
        ];
        for (name, source) in sources {
            let production = source.split("#[cfg(test)]").next().unwrap();
            let keyword = format!("un{}", "safe");
            let lint = format!("{keyword}_code");
            let raw = production.matches(&keyword).count() - production.matches(&lint).count();
            assert_eq!(raw, 2 * usize::from(name == "sys.rs"), "{name}");
            assert_eq!(
                production.matches(&format!("#[allow({lint})]")).count(),
                2 * usize::from(name == "sys.rs")
            );
        }
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
                "fido_cbor.rs",
                "fido_ctap.rs",
                "fido_device.rs",
                "fido_enroll.rs",
                "fido_hid.rs",
                "fido_metadata.rs",
                "main.rs",
                "operation.rs",
                "store.rs",
                "sys.rs",
                "tpm.rs",
                "write_operation.rs"
            ]
        );
    }
}

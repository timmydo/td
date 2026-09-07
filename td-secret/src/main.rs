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
#[allow(dead_code, reason = "FIDO2 transport prerequisite; no release consumer yet")]
mod fido_hid;
#[allow(dead_code, reason = "enrollment metadata prerequisite; no release consumer yet")]
mod fido_metadata;
#[allow(dead_code, reason = "CTAP codec prerequisite; no release consumer yet")]
mod fido_cbor;
#[allow(dead_code, reason = "CTAP codec prerequisite; no release consumer yet")]
mod fido_ctap;
#[allow(dead_code, reason = "enrollment prerequisite; no release consumer yet")]
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
        [command, uid_flag, uid, pcr_flag, pcrs, recovery]
            if command == "seal"
                && uid_flag == "--uid"
                && pcr_flag == "--pcrs"
                && recovery == "--unrecoverable" =>
        {
            store::require_root()?;
            let uid = parse_uid(uid)?;
            let pcrs = tpm::Pcrs::parse(pcrs)?;
            let store = owned_store(uid)?;
            store.seal(pcrs)?;
            eprintln!(
                "td-secret: store TPM sealed; no recovery; boot release without token consent"
            );
            Ok(())
        }
        [command, uid_flag, uid] if command == "release" && uid_flag == "--uid" => {
            store::require_root()?;
            let uid = parse_uid(uid)?;
            owned_store(uid)?.release()
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
            "td-secret seal --uid UID --pcrs LIST --unrecoverable; ",
            "td-secret release --uid UID"
        )
        .into()),
    }
}

fn owned_store(uid: u32) -> Result<store::Store, String> {
    let registry = principals::Registry::load()?;
    registry.verify_installed_accounts(&registry)?;
    let owner = registry.sessions().find(|session| session.owner == uid)
        .ok_or("credential user has no deployment reservation")?.portal;
    store::Store::open_owned(&store::user_path(uid), uid, owner, false)
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
            ("crypto.rs", include_str!("crypto.rs")),
            ("fido_cbor.rs", include_str!("fido_cbor.rs")),
            ("fido_ctap.rs", include_str!("fido_ctap.rs")),
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
                "fido_cbor.rs",
                "fido_ctap.rs",
                "fido_enroll.rs",
                "fido_hid.rs",
                "fido_metadata.rs",
                "main.rs",
                "store.rs",
                "sys.rs",
                "tpm.rs"
            ]
        );
    }
}

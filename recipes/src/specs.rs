//! The system-spec catalog — td's `system()` declarations, in Rust.
//!
//! The other half of the boa surface (`tests/ts/spec-*.ts`). One entry per
//! `spec-<stem>.ts`, keyed by stem. `spec-bad-fstype.ts` has NO entry — its
//! out-of-union `rootFsType` is not representable in Rust (`RootFsType` has no
//! such variant), so rustc rejects it at compile time, subsuming the boa/tsc
//! negative control.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned

use crate::types::{PersistenceTier, PersistentPath, RootFsType, SystemSpec};

/// Look up a system spec by `.ts` file stem (e.g. "v0", "perturbed", "gen1").
pub fn lookup(stem: &str) -> Option<SystemSpec> {
    all().into_iter().find(|(s, _)| *s == stem).map(|(_, s)| s)
}

/// Every migrated system spec, paired with its `.ts` file stem.
pub fn all() -> Vec<(&'static str, SystemSpec)> {
    vec![
        ("v0", v0()),
        ("perturbed", perturbed()),
        ("gen1", gen1()),
    ]
}

// The shared base (spec-v0); the twins differ by one field, mirroring the .ts.
fn base() -> SystemSpec {
    SystemSpec {
        host_name: "td".into(),
        timezone: "UTC".into(),
        locale: "en_US.utf8".into(),
        bootloader_target: "/dev/vda".into(),
        root_fs_label: "td-root".into(),
        root_mount: "/".into(),
        root_fs_type: RootFsType::Ext4,
        ssh_port: 22,
        ssh_password_auth: false,
        ssh_challenge_response: false,
        ship_guix: false,
        persistent_paths: vec![PersistentPath::new(PersistenceTier::Precious, "/var/lib/ssh")],
        generation: None,
    }
}

fn v0() -> SystemSpec {
    base()
}

fn perturbed() -> SystemSpec {
    SystemSpec {
        ssh_port: 2222,
        ..base()
    }
}

fn gen1() -> SystemSpec {
    SystemSpec {
        persistent_paths: vec![
            PersistentPath::new(PersistenceTier::Precious, "/var/lib/ssh"),
            PersistentPath::new(PersistenceTier::Disposable, "/var/log"),
        ],
        generation: Some(1),
        ..base()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn specs_emit_valid_json_and_round_trip() {
        for (stem, s) in all() {
            let canon = s.to_json().to_canonical();
            let reparsed = crate::json::parse(&canon)
                .unwrap_or_else(|e| panic!("{stem}: emitted invalid JSON: {e}"));
            assert_eq!(reparsed.to_canonical(), canon, "{stem}: not idempotent");
        }
    }

    #[test]
    fn perturbed_and_gen1_diverge_from_v0() {
        let v = lookup("v0").unwrap().to_json().to_canonical();
        assert_ne!(v, lookup("perturbed").unwrap().to_json().to_canonical());
        assert_ne!(v, lookup("gen1").unwrap().to_json().to_canonical());
    }

    #[test]
    fn generation_null_is_emitted_not_omitted() {
        // boa emits the generation key with a null value for the default system.
        let canon = lookup("v0").unwrap().to_json().to_canonical();
        assert!(canon.contains("\"generation\":null"), "got {canon}");
    }
}

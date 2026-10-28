use crate::shared::{split_target_debug, target_rustc};
use crate::types::{Recipe, Step};

// Target-built static disk installer. The shipped source reuses the engine's
// GPT and FAT32 writers and the on-disk shape td-boot declares, so the writer
// and the reader of a td disk are built from one description of it. `gpt.rs`
// reaches its checksum as `crate::crc32`, so those two arrive as a pair or the
// build does not link. `sha256.rs` is the live installation's check of the ESP
// kernel against the authenticated manifest. The device-bound service's TPM
// probe and formatting run over the sibling crates td-protector, whose
// cryptsetup runner formatting uses, and td-tpm (td-protector reaches
// td-json and td-luks, td-luks and td-tpm td-fido), each compiled first
// as an rlib with the binary's profile and passed by `--extern`, as
// td-boot passes td-tpm;
// td-tpm and td-fido include the same engine SHA-256 by `#[path]`. The
// storage worker (`storage-operation`) admits and writes the ESP through
// td-fat, a std-only rlib like the others, asks a disk proof's security key
// through td-fido's disk flows, linked directly, and compiles td-authd's
// consent descriptions and td-secret's worker endpoint and memory rules by
// `#[path]`.
const MAIN_RS: &str = include_str!("../../../td-install/src/main.rs");
const STORAGE_OPERATION_RS: &str = include_str!("../../../td-install/src/storage_operation.rs");
const CONSENT_RS: &str = include_str!("../../../td-authd/src/consent.rs");
const FRAMES_RS: &str = include_str!("../../../td-secret/src/frames.rs");
const MEMORY_RS: &str = include_str!("../../../td-secret/src/memory.rs");
const FAT_LIB_RS: &str = include_str!("../../../td-fat/src/lib.rs");
const TIMEZONES_RS: &str = include_str!("../../../td-install/src/timezones.rs");
const INVENTORY_RS: &str = include_str!("../../../td-install/src/inventory.rs");
const REINSTALL_RS: &str = include_str!("../../../td-install/src/reinstall.rs");
const INSTALLATION_PLAN_RS: &str = include_str!("../../../td-install/src/installation_plan.rs");
const INSTALLATION_PROTOCOL_RS: &str =
    include_str!("../../../td-install/src/installation_protocol.rs");
const INSTALLATION_SERVICE_RS: &str =
    include_str!("../../../td-install/src/installation_service.rs");
const INSTALLATION_CONSENT_RS: &str =
    include_str!("../../../td-install/src/installation_consent.rs");
const LOOP_SYS_RS: &str = include_str!("../../../td-install/src/loop_sys.rs");
const LOOP_DEVICE_RS: &str = include_str!("../../../td-install/src/loop_device.rs");
const DEVICE_BOUND_RS: &str = include_str!("../../../td-install/src/device_bound.rs");
const PROTOCOL_RS: &str = include_str!("../../../td-boot/src/protocol.rs");
const SELECTOR_APPENDIX_RS: &str = include_str!("../../../td-boot/src/selector_appendix.rs");
const TD_FS_RS: &str = include_str!("../../../td-fs/src/real_file.rs");
const CRC32_RS: &str = include_str!("../../../engine/src/crc32.rs");
const GPT_RS: &str = include_str!("../../../engine/src/gpt.rs");
const CPIO_RS: &str = include_str!("../../../engine/src/cpio.rs");
const FAT_RS: &str = include_str!("../../../engine/src/fat.rs");
const SHA256_RS: &str = include_str!("../../../engine/src/sha256.rs");
/// td-tpm's files and td-fido's, whose P-256, AES and HMAC-SHA256 td-tpm's
/// sessions use, each crate's whole `src` (test-only files included, as
/// the staging test below holds). td-protector's PIN derivation also
/// compiles td-fido's `hmac.rs` by `#[path]`.
const TPM_AND_FIDO: &[(&str, &str)] = &[
    (
        "{src}/td-tpm/src/lib.rs",
        include_str!("../../../td-tpm/src/lib.rs"),
    ),
    (
        "{src}/td-tpm/src/auth.rs",
        include_str!("../../../td-tpm/src/auth.rs"),
    ),
    (
        "{src}/td-tpm/src/lockout.rs",
        include_str!("../../../td-tpm/src/lockout.rs"),
    ),
    (
        "{src}/td-tpm/src/session.rs",
        include_str!("../../../td-tpm/src/session.rs"),
    ),
    (
        "{src}/td-tpm/src/emulator_tests.rs",
        include_str!("../../../td-tpm/src/emulator_tests.rs"),
    ),
    (
        "{src}/td-fido/src/lib.rs",
        include_str!("../../../td-fido/src/lib.rs"),
    ),
    (
        "{src}/td-fido/src/crypto.rs",
        include_str!("../../../td-fido/src/crypto.rs"),
    ),
    (
        "{src}/td-fido/src/hmac.rs",
        include_str!("../../../td-fido/src/hmac.rs"),
    ),
    (
        "{src}/td-fido/src/root.rs",
        include_str!("../../../td-fido/src/root.rs"),
    ),
    (
        "{src}/td-fido/src/fido_aes.rs",
        include_str!("../../../td-fido/src/fido_aes.rs"),
    ),
    (
        "{src}/td-fido/src/fido_cbor.rs",
        include_str!("../../../td-fido/src/fido_cbor.rs"),
    ),
    (
        "{src}/td-fido/src/fido_ctap.rs",
        include_str!("../../../td-fido/src/fido_ctap.rs"),
    ),
    (
        "{src}/td-fido/src/fido_device.rs",
        include_str!("../../../td-fido/src/fido_device.rs"),
    ),
    (
        "{src}/td-fido/src/fido_disk.rs",
        include_str!("../../../td-fido/src/fido_disk.rs"),
    ),
    (
        "{src}/td-fido/src/fido_enroll.rs",
        include_str!("../../../td-fido/src/fido_enroll.rs"),
    ),
    (
        "{src}/td-fido/src/fido_fixtures.rs",
        include_str!("../../../td-fido/src/fido_fixtures.rs"),
    ),
    (
        "{src}/td-fido/src/fido_hid.rs",
        include_str!("../../../td-fido/src/fido_hid.rs"),
    ),
    (
        "{src}/td-fido/src/fido_p256.rs",
        include_str!("../../../td-fido/src/fido_p256.rs"),
    ),
    (
        "{src}/td-fido/src/fido_pin.rs",
        include_str!("../../../td-fido/src/fido_pin.rs"),
    ),
    (
        "{src}/td-fido/src/fido_transaction.rs",
        include_str!("../../../td-fido/src/fido_transaction.rs"),
    ),
    (
        "{src}/td-fido/src/fido_virtual.rs",
        include_str!("../../../td-fido/src/fido_virtual.rs"),
    ),
    (
        "{src}/td-fido/src/fido_virtual_tests.rs",
        include_str!("../../../td-fido/src/fido_virtual_tests.rs"),
    ),
];
const JSON_ARENA_EMIT_RS: &str = include_str!("../../../td-json/src/arena_emit.rs");
const JSON_ARENA_RS: &str = include_str!("../../../td-json/src/arena.rs");
const JSON_RS: &str = include_str!("../../../td-json/src/lib.rs");
const JSON_RETAIN_RS: &str = include_str!("../../../td-json/src/retain.rs");
const JSON_SCAN_RS: &str = include_str!("../../../td-json/src/scan.rs");
const JSON_STRING_RS: &str = include_str!("../../../td-json/src/string.rs");
const JSON_STRING_ARRAY_RS: &str = include_str!("../../../td-json/src/string_array.rs");
const LUKS_RS: &str = include_str!("../../../td-luks/src/lib.rs");
const LUKS_HEADER_RS: &str = include_str!("../../../td-luks/src/header.rs");
const LUKS_KEYSLOT_RS: &str = include_str!("../../../td-luks/src/keyslot.rs");
const LUKS_WRITE_RS: &str = include_str!("../../../td-luks/src/write.rs");
const LUKS_REENCRYPT_RS: &str = include_str!("../../../td-luks/src/reencrypt.rs");
const LUKS_REENCRYPT_PROFILE_RS: &str = include_str!("../../../td-luks/src/reencrypt_profile.rs");
const LUKS_REENCRYPT_METADATA_RS: &str = include_str!("../../../td-luks/src/reencrypt_metadata.rs");
const LUKS_REENCRYPT_TRANSITION_RS: &str =
    include_str!("../../../td-luks/src/reencrypt_transition.rs");
const ENCODING_RS: &str = include_str!("../../../td-encoding/src/lib.rs");
const PROTECTOR_RS: &str = include_str!("../../../td-protector/src/lib.rs");
const PROTECTOR_LUKS_RS: &str = include_str!("../../../td-protector/src/luks.rs");
const PROTECTOR_LOCKOUT_RS: &str = include_str!("../../../td-protector/src/lockout.rs");
const PROTECTOR_UPDATE_RETRY_RS: &str = include_str!("../../../td-protector/src/update_retry.rs");
const PROTECTOR_LUKS2_RS: &str = include_str!("../../../td-protector/src/luks2.rs");
const PROTECTOR_MANAGE_RS: &str = include_str!("../../../td-protector/src/manage.rs");
const PROTECTOR_PIN_RS: &str = include_str!("../../../td-protector/src/pin.rs");
const PROTECTOR_PROTECTED_RS: &str = include_str!("../../../td-protector/src/protected.rs");
const PROTECTOR_PROTECTED_PLAN_RS: &str =
    include_str!("../../../td-protector/src/protected_plan.rs");
const PROTECTOR_PROTECTED_RELEASE_RS: &str =
    include_str!("../../../td-protector/src/protected_release.rs");
const PROTECTOR_RECOVERY_RS: &str = include_str!("../../../td-protector/src/recovery.rs");
const PROTECTOR_RELEASE_RS: &str = include_str!("../../../td-protector/src/release.rs");
const PROTECTOR_REQUEST_RS: &str = include_str!("../../../td-protector/src/request.rs");
const PROTECTOR_TOKEN_RS: &str = include_str!("../../../td-protector/src/token.rs");
const PROTECTOR_TRANSITION_RS: &str = include_str!("../../../td-protector/src/transition.rs");
const PROTECTOR_UPGRADE_RS: &str = include_str!("../../../td-protector/src/upgrade.rs");
const PROTECTOR_UPGRADE_TESTS_RS: &str = include_str!("../../../td-protector/src/upgrade_tests.rs");
const PROTECTOR_UPGRADE_BOOT_RS: &str = include_str!("../../../td-protector/src/upgrade_boot.rs");
const PROTECTOR_UPGRADE_BOOT_TESTS_RS: &str =
    include_str!("../../../td-protector/src/upgrade_boot_tests.rs");

pub fn recipe() -> Recipe {
    let rustc = "{in:rust-toolchain}/bin/rustc";

    // The staged tree mirrors the repository, because the `#[path]` includes in
    // `main.rs` are relative and resolve through it: `../../engine/src/gpt.rs`
    // only names the right file if `main.rs` sits at `td-install/src/`.
    let mut steps = vec![
        Step::MkDir {
            path: "{out}/bin".into(),
        },
        Step::MkDir {
            path: "{src}/td-install/src".into(),
        },
        Step::MkDir {
            path: "{src}/td-boot/src".into(),
        },
        Step::MkDir {
            path: "{src}/engine/src".into(),
        },
        Step::MkDir {
            path: "{src}/td-firstboot/src".into(),
        },
        Step::MkDir {
            path: "{src}/td-tpm/src".into(),
        },
        Step::MkDir {
            path: "{src}/td-json/src".into(),
        },
        Step::MkDir {
            path: "{src}/td-protector/src".into(),
        },
        Step::MkDir {
            path: "{src}/td-luks/src".into(),
        },
        Step::MkDir {
            path: "{src}/td-encoding/src".into(),
        },
        Step::MkDir {
            path: "{src}/td-fido/src".into(),
        },
        Step::MkDir {
            path: "{src}/td-fat/src".into(),
        },
        Step::MkDir {
            path: "{src}/td-authd/src".into(),
        },
        Step::MkDir {
            path: "{src}/td-secret/src".into(),
        },
        Step::WriteFile {
            path: "{src}/td-fat/src/lib.rs".into(),
            content: FAT_LIB_RS.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{src}/td-authd/src/consent.rs".into(),
            content: CONSENT_RS.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{src}/td-secret/src/frames.rs".into(),
            content: FRAMES_RS.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{src}/td-secret/src/memory.rs".into(),
            content: MEMORY_RS.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{src}/td-json/src/arena_emit.rs".into(),
            content: JSON_ARENA_EMIT_RS.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{src}/td-json/src/arena.rs".into(),
            content: JSON_ARENA_RS.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{src}/td-json/src/lib.rs".into(),
            content: JSON_RS.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{src}/td-json/src/retain.rs".into(),
            content: JSON_RETAIN_RS.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{src}/td-json/src/scan.rs".into(),
            content: JSON_SCAN_RS.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{src}/td-json/src/string.rs".into(),
            content: JSON_STRING_RS.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{src}/td-json/src/string_array.rs".into(),
            content: JSON_STRING_ARRAY_RS.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{src}/td-luks/src/lib.rs".into(),
            content: LUKS_RS.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{src}/td-luks/src/header.rs".into(),
            content: LUKS_HEADER_RS.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{src}/td-luks/src/keyslot.rs".into(),
            content: LUKS_KEYSLOT_RS.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{src}/td-luks/src/write.rs".into(),
            content: LUKS_WRITE_RS.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{src}/td-luks/src/reencrypt.rs".into(),
            content: LUKS_REENCRYPT_RS.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{src}/td-luks/src/reencrypt_profile.rs".into(),
            content: LUKS_REENCRYPT_PROFILE_RS.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{src}/td-luks/src/reencrypt_metadata.rs".into(),
            content: LUKS_REENCRYPT_METADATA_RS.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{src}/td-luks/src/reencrypt_transition.rs".into(),
            content: LUKS_REENCRYPT_TRANSITION_RS.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{src}/td-encoding/src/lib.rs".into(),
            content: ENCODING_RS.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{src}/td-protector/src/lib.rs".into(),
            content: PROTECTOR_RS.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{src}/td-protector/src/luks.rs".into(),
            content: PROTECTOR_LUKS_RS.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{src}/td-protector/src/lockout.rs".into(),
            content: PROTECTOR_LOCKOUT_RS.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{src}/td-protector/src/update_retry.rs".into(),
            content: PROTECTOR_UPDATE_RETRY_RS.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{src}/td-protector/src/luks2.rs".into(),
            content: PROTECTOR_LUKS2_RS.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{src}/td-protector/src/manage.rs".into(),
            content: PROTECTOR_MANAGE_RS.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{src}/td-protector/src/pin.rs".into(),
            content: PROTECTOR_PIN_RS.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{src}/td-protector/src/protected.rs".into(),
            content: PROTECTOR_PROTECTED_RS.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{src}/td-protector/src/protected_plan.rs".into(),
            content: PROTECTOR_PROTECTED_PLAN_RS.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{src}/td-protector/src/protected_release.rs".into(),
            content: PROTECTOR_PROTECTED_RELEASE_RS.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{src}/td-protector/src/recovery.rs".into(),
            content: PROTECTOR_RECOVERY_RS.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{src}/td-protector/src/release.rs".into(),
            content: PROTECTOR_RELEASE_RS.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{src}/td-protector/src/request.rs".into(),
            content: PROTECTOR_REQUEST_RS.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{src}/td-protector/src/token.rs".into(),
            content: PROTECTOR_TOKEN_RS.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{src}/td-protector/src/transition.rs".into(),
            content: PROTECTOR_TRANSITION_RS.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{src}/td-protector/src/upgrade.rs".into(),
            content: PROTECTOR_UPGRADE_RS.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{src}/td-protector/src/upgrade_tests.rs".into(),
            content: PROTECTOR_UPGRADE_TESTS_RS.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{src}/td-protector/src/upgrade_boot.rs".into(),
            content: PROTECTOR_UPGRADE_BOOT_RS.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{src}/td-protector/src/upgrade_boot_tests.rs".into(),
            content: PROTECTOR_UPGRADE_BOOT_TESTS_RS.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{src}/td-firstboot/src/hostname.rs".into(),
            content: include_str!("../../../td-firstboot/src/hostname.rs").into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{src}/td-install/src/main.rs".into(),
            content: MAIN_RS.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{src}/td-install/src/timezones.rs".into(),
            content: TIMEZONES_RS.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{src}/td-install/src/inventory.rs".into(),
            content: INVENTORY_RS.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{src}/td-install/src/reinstall.rs".into(),
            content: REINSTALL_RS.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{src}/td-install/src/installation_plan.rs".into(),
            content: INSTALLATION_PLAN_RS.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{src}/td-install/src/installation_protocol.rs".into(),
            content: INSTALLATION_PROTOCOL_RS.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{src}/td-install/src/installation_service.rs".into(),
            content: INSTALLATION_SERVICE_RS.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{src}/td-install/src/installation_consent.rs".into(),
            content: INSTALLATION_CONSENT_RS.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{src}/td-install/src/loop_sys.rs".into(),
            content: LOOP_SYS_RS.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{src}/td-install/src/loop_device.rs".into(),
            content: LOOP_DEVICE_RS.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{src}/td-install/src/device_bound.rs".into(),
            content: DEVICE_BOUND_RS.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{src}/td-install/src/storage_operation.rs".into(),
            content: STORAGE_OPERATION_RS.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{src}/td-boot/src/protocol.rs".into(),
            content: PROTOCOL_RS.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{src}/td-boot/src/selector_appendix.rs".into(),
            content: SELECTOR_APPENDIX_RS.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{src}/td-fs/src/real_file.rs".into(),
            content: TD_FS_RS.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{src}/engine/src/crc32.rs".into(),
            content: CRC32_RS.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{src}/engine/src/gpt.rs".into(),
            content: GPT_RS.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{src}/engine/src/sha256.rs".into(),
            content: SHA256_RS.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{src}/engine/src/cpio.rs".into(),
            content: CPIO_RS.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{src}/engine/src/fat.rs".into(),
            content: FAT_RS.into(),
            exec: false,
        },
    ];
    for (path, content) in TPM_AND_FIDO {
        steps.push(Step::WriteFile {
            path: (*path).into(),
            content: (*content).into(),
            exec: false,
        });
    }
    // The sibling libraries, dependencies first, under the binary's profile.
    for (name, lib, externs) in [
        ("td_fido", "{src}/td-fido/src/lib.rs", &[][..]),
        ("td_fat", "{src}/td-fat/src/lib.rs", &[]),
        (
            "td_tpm",
            "{src}/td-tpm/src/lib.rs",
            &["--extern", "td_fido={root}/libtd_fido.rlib"],
        ),
        ("td_json", "{src}/td-json/src/lib.rs", &[]),
        (
            "td_luks",
            "{src}/td-luks/src/lib.rs",
            &[
                "--extern",
                "td_fido={root}/libtd_fido.rlib",
                "--extern",
                "td_json={root}/libtd_json.rlib",
            ],
        ),
        (
            "td_protector",
            "{src}/td-protector/src/lib.rs",
            &[
                "--extern",
                "td_json={root}/libtd_json.rlib",
                "--extern",
                "td_luks={root}/libtd_luks.rlib",
                "--extern",
                "td_tpm={root}/libtd_tpm.rlib",
                // td-tpm's own dependency, td-fido, is found here.
                "-L",
                "dependency={root}",
            ],
        ),
    ] {
        let output = format!("{{root}}/lib{name}.rlib");
        let mut args = vec![
            "--edition",
            "2021",
            "--crate-type",
            "rlib",
            "--crate-name",
            name,
            "-C",
            "opt-level=2",
            "--target",
            "x86_64-unknown-linux-gnu",
            "-C",
            "target-feature=+crt-static",
            "-C",
            "relocation-model=static",
            "-C",
            "panic=abort",
        ];
        args.extend_from_slice(externs);
        args.extend_from_slice(&["-o", &output, lib]);
        steps.push(target_rustc("{src}", rustc, &args).env("SOURCE_DATE_EPOCH", "1"));
    }
    steps.push(
        target_rustc(
            "{src}/td-install/src",
            rustc,
            &[
                "--edition",
                "2021",
                "-C",
                "opt-level=2",
                "--target",
                "x86_64-unknown-linux-gnu",
                "-C",
                "target-feature=+crt-static",
                "-C",
                "relocation-model=static",
                "-C",
                "panic=abort",
                "--extern",
                "td_fat={root}/libtd_fat.rlib",
                "--extern",
                "td_fido={root}/libtd_fido.rlib",
                "--extern",
                "td_protector={root}/libtd_protector.rlib",
                "--extern",
                "td_tpm={root}/libtd_tpm.rlib",
                // td-protector's own dependency, td-json, is found here.
                "-L",
                "dependency={root}",
                "-o",
                "{out}/bin/td-install",
                "{src}/td-install/src/main.rs",
            ],
        )
        .env("SOURCE_DATE_EPOCH", "1"),
    );
    steps.push(Step::Require {
        paths: vec!["{out}/bin/td-install".into()],
        exec: true,
    });
    steps.push(split_target_debug("{out}"));
    steps.push(Step::assert_static(&["{out}/bin/td-install"]));

    Recipe::mesboot("td-install", "0.1")
        .build_root()
        .native_inputs(&["rust-toolchain", "td-link", "glibc-x86-64"])
        .steps(steps)
}

#[cfg(test)]
mod tests {
    #[test]
    fn every_sibling_crate_source_is_staged() {
        crate::shared::assert_sibling_sources_staged(
            super::recipe(),
            &[
                "td-tpm",
                "td-fido",
                "td-json",
                "td-luks",
                "td-encoding",
                "td-protector",
                "td-fat",
            ],
        );
    }
}

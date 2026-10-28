use crate::shared::{split_target_debug, target_rustc_at_roots};
use crate::types::{Recipe, Step};

// Target-built static deployment verifier and kexec boot shim. The shipped
// source reuses the engine's dependency-free SHA-256 implementation, which
// checks every payload against its manifest. Its PCR 11 measurement
// runs over the sibling TPM 2.0 client td-tpm (td-tpm/DESIGN.md), whose
// sessions use td-fido's P-256 and AES (td-fido/DESIGN.md), and its
// volume discovery, the live selector's PCR 12 release cap and the installed
// selector's release and the deployment initramfs's unlock over
// td-protector, which reaches td-json and td-luks's LUKS2 header reader,
// and its FIDO2 client over td-fido
// (`fido.rs`, whose `fido-worker` verb nothing runs yet), and a staged
// selector update's ESP over td-fat, found through engine's GPT reader;
// each is compiled first as an rlib with the binary's profile and passed
// by `--extern`, as td-install passes them. td-tpm and td-fido include the
// same engine SHA-256 by `#[path]`.
const BOOT_INFO_RS: &str = include_str!("../../../engine/src/boot_info.rs");
const MENU_RS: &str = include_str!("../../../td-boot/src/menu.rs");
const CALENDAR_RS: &str = include_str!("../../../td-civil/src/tzif.rs");
const MAIN_RS: &str = include_str!("../../../td-boot/src/main.rs");
const ADMISSION_RS: &str = include_str!("../../../td-boot/src/admission.rs");
const ADMIT_RS: &str = include_str!("../../../td-boot/src/admit.rs");
const AUTHENTICODE_RS: &str = include_str!("../../../td-boot/src/authenticode.rs");
const EVENTLOG_RS: &str = include_str!("../../../td-boot/src/eventlog.rs");
const SELECTOR_APPENDIX_RS: &str = include_str!("../../../td-boot/src/selector_appendix.rs");
const CAP_RS: &str = include_str!("../../../td-boot/src/cap.rs");
const UNLOCK_RS: &str = include_str!("../../../td-boot/src/unlock.rs");
const MEASUREMENT_RS: &str = include_str!("../../../td-boot/src/measurement.rs");
const MIRROR_RS: &str = include_str!("../../../td-boot/src/mirror.rs");
const FIDO_RS: &str = include_str!("../../../td-boot/src/fido.rs");
const SELECTOR_RELEASE_RS: &str = include_str!("../../../td-boot/src/selector_release.rs");
const SELECTOR_UPDATE_RS: &str = include_str!("../../../td-boot/src/selector_update.rs");
const VOLUME_RS: &str = include_str!("../../../td-boot/src/volume.rs");
const PROTOCOL_RS: &str = include_str!("../../../td-boot/src/protocol.rs");
const TD_FS_RS: &str = include_str!("../../../td-fs/src/real_file.rs");
// td-init's console identity, which secret-line and td-boot's VT mirror share.
const CONSOLE_RS: &str = include_str!("../../../td-init/src/console.rs");
const SHA256_RS: &str = include_str!("../../../engine/src/sha256.rs");
const CPIO_RS: &str = include_str!("../../../engine/src/cpio.rs");
const CRC32_RS: &str = include_str!("../../../engine/src/crc32.rs");
const GPT_RS: &str = include_str!("../../../engine/src/gpt.rs");
const FAT_LIB_RS: &str = include_str!("../../../td-fat/src/lib.rs");
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
const ENCODING_RS: &str = include_str!("../../../td-encoding/src/lib.rs");
const PROTECTOR_RS: &str = include_str!("../../../td-protector/src/lib.rs");
const PROTECTOR_CRYPTSETUP_RS: &str = include_str!("../../../td-protector/src/cryptsetup.rs");
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

    let mut steps = vec![
        Step::MkDir {
            path: "{out}/bin".into(),
        },
        Step::MkDir {
            path: "{root}/profile-repro-a/bin".into(),
        },
        Step::MkDir {
            path: "{root}/profile-repro-b/bin".into(),
        },
        Step::MkDir {
            path: "{src}/td-boot/src".into(),
        },
        Step::MkDir {
            path: "{src}/engine/src".into(),
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
        Step::WriteFile {
            path: "{src}/td-fat/src/lib.rs".into(),
            content: FAT_LIB_RS.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{src}/td-boot/src/main.rs".into(),
            content: MAIN_RS.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{src}/td-boot/src/admission.rs".into(),
            content: ADMISSION_RS.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{src}/td-boot/src/admit.rs".into(),
            content: ADMIT_RS.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{src}/td-boot/src/authenticode.rs".into(),
            content: AUTHENTICODE_RS.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{src}/td-boot/src/eventlog.rs".into(),
            content: EVENTLOG_RS.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{src}/td-boot/src/selector_appendix.rs".into(),
            content: SELECTOR_APPENDIX_RS.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{src}/td-boot/src/cap.rs".into(),
            content: CAP_RS.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{src}/td-boot/src/unlock.rs".into(),
            content: UNLOCK_RS.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{src}/td-boot/src/measurement.rs".into(),
            content: MEASUREMENT_RS.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{src}/td-boot/src/mirror.rs".into(),
            content: MIRROR_RS.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{src}/td-boot/src/fido.rs".into(),
            content: FIDO_RS.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{src}/td-boot/src/selector_release.rs".into(),
            content: SELECTOR_RELEASE_RS.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{src}/td-boot/src/selector_update.rs".into(),
            content: SELECTOR_UPDATE_RS.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{src}/td-boot/src/volume.rs".into(),
            content: VOLUME_RS.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{src}/td-boot/src/protocol.rs".into(),
            content: PROTOCOL_RS.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{src}/td-fs/src/real_file.rs".into(),
            content: TD_FS_RS.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{src}/td-init/src/console.rs".into(),
            content: CONSOLE_RS.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{src}/engine/src/boot_info.rs".into(),
            content: BOOT_INFO_RS.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{src}/td-boot/src/menu.rs".into(),
            content: MENU_RS.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{src}/td-civil/src/tzif.rs".into(),
            content: CALENDAR_RS.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{src}/engine/src/sha256.rs".into(),
            content: SHA256_RS.into(),
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
            path: "{src}/engine/src/cpio.rs".into(),
            content: CPIO_RS.into(),
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
            path: "{src}/td-protector/src/cryptsetup.rs".into(),
            content: PROTECTOR_CRYPTSETUP_RS.into(),
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
    ];
    for (path, content) in TPM_AND_FIDO {
        steps.push(Step::WriteFile {
            path: (*path).into(),
            content: (*content).into(),
            exec: false,
        });
    }
    for dest in [
        "{root}/profile-repro-a/source",
        "{root}/profile-repro-b/source",
    ] {
        steps.push(Step::CopyTree {
            from: "{src}".into(),
            dest: dest.into(),
        });
    }
    // Each root compiles its own sibling rlibs, dependencies first, under the
    // same roots, so the two-root oracle below covers the libraries as well
    // as the binary.
    let library = |root: &str, name: &str, krate: &str, externs: &[&str]| {
        let source = format!("{root}/source");
        let output = format!("{root}/lib{name}.rlib");
        let lib = format!("{source}/{krate}/src/lib.rs");
        // A dependency's own dependencies, td-tpm's td-fido, are found here.
        let mut externs: Vec<String> = externs
            .iter()
            .flat_map(|dep| ["--extern".to_owned(), format!("{dep}={root}/lib{dep}.rlib")])
            .collect();
        externs.extend(["-L".to_owned(), format!("dependency={root}")]);
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
        args.extend(externs.iter().map(String::as_str));
        args.extend_from_slice(&["-o", &output, &lib]);
        target_rustc_at_roots(&source, rustc, &args, root, &source).env("SOURCE_DATE_EPOCH", "1")
    };
    let compile = |root: &str| {
        let source = format!("{root}/source");
        let directory = format!("{source}/td-boot/src");
        let output = format!("{root}/bin/td-boot");
        let main = format!("{source}/td-boot/src/main.rs");
        let tpm = format!("td_tpm={root}/libtd_tpm.rlib");
        let protector = format!("td_protector={root}/libtd_protector.rlib");
        let fido = format!("td_fido={root}/libtd_fido.rlib");
        let fat = format!("td_fat={root}/libtd_fat.rlib");
        // td-protector's own dependency, td-json, is found here.
        let dependencies = format!("dependency={root}");
        target_rustc_at_roots(
            &directory,
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
                &tpm,
                "--extern",
                &protector,
                "--extern",
                &fido,
                "--extern",
                &fat,
                "-L",
                &dependencies,
                "-o",
                &output,
                &main,
            ],
            root,
            &source,
        )
        .env("SOURCE_DATE_EPOCH", "1")
    };
    // One small representative direct-rustc program is copied below two
    // different source and build roots, then built independently. Their
    // canonical remaps, deterministic build ID, runtime strip, and companion
    // transform must all converge byte for byte.
    for root in ["{root}/profile-repro-a", "{root}/profile-repro-b"] {
        steps.push(library(root, "td_fido", "td-fido", &[]));
        steps.push(library(root, "td_fat", "td-fat", &[]));
        steps.push(library(root, "td_tpm", "td-tpm", &["td_fido"]));
        steps.push(library(root, "td_json", "td-json", &[]));
        steps.push(library(root, "td_luks", "td-luks", &["td_fido", "td_json"]));
        steps.push(library(
            root,
            "td_protector",
            "td-protector",
            &["td_json", "td_luks", "td_tpm"],
        ));
        steps.push(compile(root));
    }
    steps.push(Step::compare_files(
        "{root}/profile-repro-a/bin/td-boot",
        "{root}/profile-repro-b/bin/td-boot",
    ));
    steps.push(Step::CopyFiles {
        files: vec!["{root}/profile-repro-a/bin/td-boot".into()],
        dest: "{out}/bin".into(),
    });
    steps.push(Step::Require {
        paths: vec!["{out}/bin/td-boot".into()],
        exec: true,
    });
    steps.push(split_target_debug("{out}"));
    steps.push(split_target_debug("{root}/profile-repro-b"));
    steps.push(Step::compare_files(
        "{out}/bin/td-boot",
        "{root}/profile-repro-b/bin/td-boot",
    ));
    steps.push(Step::compare_files(
        "{out}/lib/debug/bin/td-boot.debug",
        "{root}/profile-repro-b/lib/debug/bin/td-boot.debug",
    ));
    steps.push(Step::assert_static(&["{out}/bin/td-boot"]));

    Recipe::mesboot("td-boot", "0.1")
        .build_root()
        .native_inputs(&["rust-toolchain", "td-link", "glibc-x86-64"])
        .steps(steps)
}

#[cfg(test)]
mod tests {
    // td-boot's own files are `builder/src/affected.rs`'s staging guard's.
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

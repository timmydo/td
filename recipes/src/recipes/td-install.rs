use crate::ladder::{split_target_debug, target_rustc};
use crate::types::{Recipe, Step};

// Target-built static disk installer. The shipped source reuses the engine's
// GPT and FAT32 writers and the on-disk shape td-boot declares, so the writer
// and the reader of a td disk are built from one description of it. `gpt.rs`
// reaches its checksum as `crate::crc32`, so those two arrive as a pair or the
// build does not link. `sha256.rs` is the live installation's check of the ESP
// kernel against the authenticated manifest. The device-bound service's TPM
// probe and formatting run over the sibling crates td-protector, whose
// cryptsetup runner formatting uses, and td-tpm (td-protector reaches
// td-json, td-tpm td-fido), each compiled first as an rlib with the
// binary's profile and passed by `--extern`, as td-boot passes td-tpm;
// td-tpm and td-fido include the same engine SHA-256 by `#[path]`.
const MAIN_RS: &str = include_str!("../../../td-install/src/main.rs");
const TIMEZONES_RS: &str = include_str!("../../../td-install/src/timezones.rs");
const INVENTORY_RS: &str = include_str!("../../../td-install/src/inventory.rs");
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
const JSON_RS: &str = include_str!("../../../td-json/src/lib.rs");
const JSON_RETAIN_RS: &str = include_str!("../../../td-json/src/retain.rs");
const JSON_STRING_RS: &str = include_str!("../../../td-json/src/string.rs");
const JSON_STRING_ARRAY_RS: &str = include_str!("../../../td-json/src/string_array.rs");
const PROTECTOR_RS: &str = include_str!("../../../td-protector/src/lib.rs");
const PROTECTOR_CRYPTSETUP_RS: &str = include_str!("../../../td-protector/src/cryptsetup.rs");
const PROTECTOR_LUKS2_RS: &str = include_str!("../../../td-protector/src/luks2.rs");
const PROTECTOR_PIN_RS: &str = include_str!("../../../td-protector/src/pin.rs");
const PROTECTOR_RECOVERY_RS: &str = include_str!("../../../td-protector/src/recovery.rs");
const PROTECTOR_RELEASE_RS: &str = include_str!("../../../td-protector/src/release.rs");
const PROTECTOR_TOKEN_RS: &str = include_str!("../../../td-protector/src/token.rs");
const PROTECTOR_TRANSITION_RS: &str = include_str!("../../../td-protector/src/transition.rs");

pub fn recipe() -> Recipe {
    let rustc = "{in:rust-toolchain}/bin/rustc";
    let gcc = "{in:gcc-x86-64-self}/stage/td/store/gcc-14.3.0-x86_64-self/bin/gcc";
    let gccbin = "{in:gcc-x86-64-self}/stage/td/store/gcc-14.3.0-x86_64-self/bin";
    let bbin = "{in:binutils-x86-64-self}/bin";
    let glib = "{in:glibc-x86-64}/stage/td/store/glibc-2.41-x86_64/lib";
    let objcopy = "{in:binutils-x86-64-self}/bin/objcopy";
    let ranlib = "{in:binutils-x86-64-self}/bin/ranlib";
    let libgcc_a = "{in:gcc-x86-64-self}/stage/td/store/gcc-14.3.0-x86_64-self/lib/gcc/x86_64-pc-linux-gnu/14.3.0/libgcc.a";

    let linker = format!("-Clinker={gcc}");
    let lib_b = format!("-Clink-arg=-B{glib}");
    let bin_b = format!("-Clink-arg=-B{bbin}");
    let path = format!("{bbin}:{gccbin}");

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
            path: "{src}/td-fido/src".into(),
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
            path: "{src}/td-protector/src/luks2.rs".into(),
            content: PROTECTOR_LUKS2_RS.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{src}/td-protector/src/pin.rs".into(),
            content: PROTECTOR_PIN_RS.into(),
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
        Step::MkDir {
            path: "{root}/eh".into(),
        },
    ];
    for (path, content) in TPM_AND_FIDO {
        steps.push(Step::WriteFile {
            path: (*path).into(),
            content: (*content).into(),
            exec: false,
        });
    }
    // The self toolchain folds the unwinder into libgcc.a; rustc's static link
    // still requests the conventional libgcc_eh.a name.
    steps.push(
        Step::run("{root}", &[objcopy, libgcc_a, "{root}/eh/libgcc_eh.a"]).env("PATH", &path),
    );
    steps.push(Step::run("{root}", &[ranlib, "{root}/eh/libgcc_eh.a"]).env("PATH", &path));
    // The sibling libraries, dependencies first, under the binary's profile.
    for (name, lib, externs) in [
        ("td_fido", "{src}/td-fido/src/lib.rs", &[][..]),
        (
            "td_tpm",
            "{src}/td-tpm/src/lib.rs",
            &["--extern", "td_fido={root}/libtd_fido.rlib"],
        ),
        ("td_json", "{src}/td-json/src/lib.rs", &[]),
        (
            "td_protector",
            "{src}/td-protector/src/lib.rs",
            &[
                "--extern",
                "td_json={root}/libtd_json.rlib",
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
        steps.push(
            target_rustc("{src}", rustc, &args)
                .env("PATH", &path)
                .env("SOURCE_DATE_EPOCH", "1"),
        );
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
                "td_protector={root}/libtd_protector.rlib",
                "--extern",
                "td_tpm={root}/libtd_tpm.rlib",
                // td-protector's own dependency, td-json, is found here.
                "-L",
                "dependency={root}",
                &linker,
                "-L",
                glib,
                &lib_b,
                &bin_b,
                "-Clink-arg=-L{root}/eh",
                "-Clink-arg=-static-libgcc",
                "-o",
                "{out}/bin/td-install",
                "{src}/td-install/src/main.rs",
            ],
        )
        .env("PATH", &path)
        .env("SOURCE_DATE_EPOCH", "1"),
    );
    steps.push(Step::Require {
        paths: vec!["{out}/bin/td-install".into()],
        exec: true,
    });
    steps.push(split_target_debug("{out}"));
    steps.push(Step::assert_static(&["{out}/bin/td-install"]));

    Recipe::mesboot("td-install", "0.1")
        .native_inputs(&[
            "rust-toolchain",
            "gcc-x86-64-self",
            "binutils-x86-64-self",
            "glibc-x86-64",
        ])
        .steps(steps)
}

#[cfg(test)]
mod tests {
    #[test]
    fn every_sibling_crate_source_is_staged() {
        crate::ladder::assert_sibling_sources_staged(
            super::recipe(),
            &["td-tpm", "td-fido", "td-json", "td-protector"],
        );
    }
}

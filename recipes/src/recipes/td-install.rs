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
// td-json), each compiled first as an rlib with the binary's profile and
// passed by `--extern`, as td-boot passes td-tpm; td-tpm includes the same
// engine SHA-256 by `#[path]`.
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
const TPM_RS: &str = include_str!("../../../td-tpm/src/lib.rs");
const JSON_RS: &str = include_str!("../../../td-json/src/lib.rs");
const JSON_STRING_RS: &str = include_str!("../../../td-json/src/string.rs");
const JSON_STRING_ARRAY_RS: &str = include_str!("../../../td-json/src/string_array.rs");
const PROTECTOR_RS: &str = include_str!("../../../td-protector/src/lib.rs");
const PROTECTOR_CRYPTSETUP_RS: &str = include_str!("../../../td-protector/src/cryptsetup.rs");
const PROTECTOR_LUKS2_RS: &str = include_str!("../../../td-protector/src/luks2.rs");
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
        Step::WriteFile {
            path: "{src}/td-tpm/src/lib.rs".into(),
            content: TPM_RS.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{src}/td-json/src/lib.rs".into(),
            content: JSON_RS.into(),
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
    // The self toolchain folds the unwinder into libgcc.a; rustc's static link
    // still requests the conventional libgcc_eh.a name.
    steps.push(
        Step::run("{root}", &[objcopy, libgcc_a, "{root}/eh/libgcc_eh.a"]).env("PATH", &path),
    );
    steps.push(Step::run("{root}", &[ranlib, "{root}/eh/libgcc_eh.a"]).env("PATH", &path));
    // The sibling libraries, dependencies first, under the binary's profile.
    for (name, lib, externs) in [
        ("td_tpm", "{src}/td-tpm/src/lib.rs", &[][..]),
        ("td_json", "{src}/td-json/src/lib.rs", &[]),
        (
            "td_protector",
            "{src}/td-protector/src/lib.rs",
            &[
                "--extern",
                "td_json={root}/libtd_json.rlib",
                "--extern",
                "td_tpm={root}/libtd_tpm.rlib",
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
    use super::*;

    // The sibling crates are staged file by file, so a module added to one of
    // them must be staged here too or the recipe build fails.
    #[test]
    fn every_sibling_crate_source_is_staged() {
        let staged: Vec<String> = recipe()
            .steps
            .unwrap_or_default()
            .into_iter()
            .filter_map(|step| match step {
                Step::WriteFile { path, .. } => Some(path),
                _ => None,
            })
            .collect();
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        for krate in ["td-tpm", "td-json", "td-protector"] {
            let dir = root.join(krate).join("src");
            let mut names: Vec<String> = std::fs::read_dir(&dir)
                .unwrap()
                .map(|entry| entry.unwrap().file_name().into_string().unwrap())
                .collect();
            names.sort();
            for name in names {
                assert!(name.ends_with(".rs"), "{krate}/src/{name} is not a file");
                let path = format!("{{src}}/{krate}/src/{name}");
                assert!(staged.contains(&path), "{path} is not staged");
            }
        }
    }
}

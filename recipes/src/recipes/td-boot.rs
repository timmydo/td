use crate::ladder::{split_target_debug, target_rustc_at_roots};
use crate::types::{Recipe, Step};

// Target-built static deployment verifier and kexec boot shim. The shipped
// source reuses the engine's dependency-free SHA-256 implementation, and its
// ed25519 VERIFIER — which reaches its hash as `crate::sha512`, so the two
// arrive as a pair or the build does not link. `ed25519_sign.rs` is NOT here
// and must not be: this binary verifies and never signs. Its PCR 11 measurement
// runs over the sibling TPM 2.0 client td-tpm (td-tpm/DESIGN.md), and its
// volume discovery, the live selector's PCR 12 release cap and the installed
// selector's release and the deployment initramfs's unlock over
// td-protector, which reaches td-json; each is
// compiled first as an rlib with the binary's profile and passed by
// `--extern`, as td-install passes them. td-tpm includes the same engine
// SHA-256 by `#[path]`.
const MAIN_RS: &str = include_str!("../../../td-boot/src/main.rs");
const CAP_RS: &str = include_str!("../../../td-boot/src/cap.rs");
const UNLOCK_RS: &str = include_str!("../../../td-boot/src/unlock.rs");
const MEASUREMENT_RS: &str = include_str!("../../../td-boot/src/measurement.rs");
const SELECTOR_RELEASE_RS: &str = include_str!("../../../td-boot/src/selector_release.rs");
const VOLUME_RS: &str = include_str!("../../../td-boot/src/volume.rs");
const PROTOCOL_RS: &str = include_str!("../../../td-boot/src/protocol.rs");
const TD_FS_RS: &str = include_str!("../../../td-fs/src/real_file.rs");
const SHA256_RS: &str = include_str!("../../../engine/src/sha256.rs");
const SHA512_RS: &str = include_str!("../../../engine/src/sha512.rs");
const ED25519_RS: &str = include_str!("../../../engine/src/ed25519.rs");
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
        Step::WriteFile {
            path: "{src}/td-boot/src/main.rs".into(),
            content: MAIN_RS.into(),
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
            path: "{src}/td-boot/src/selector_release.rs".into(),
            content: SELECTOR_RELEASE_RS.into(),
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
            path: "{src}/engine/src/sha256.rs".into(),
            content: SHA256_RS.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{src}/engine/src/sha512.rs".into(),
            content: SHA512_RS.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{src}/engine/src/ed25519.rs".into(),
            content: ED25519_RS.into(),
            exec: false,
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
        Step::MkDir {
            path: "{root}/eh".into(),
        },
    ];
    for dest in [
        "{root}/profile-repro-a/source",
        "{root}/profile-repro-b/source",
    ] {
        steps.push(Step::CopyTree {
            from: "{src}".into(),
            dest: dest.into(),
        });
    }
    // The self toolchain folds the unwinder into libgcc.a; rustc's static link
    // still requests the conventional libgcc_eh.a name.
    steps.push(
        Step::run("{root}", &[objcopy, libgcc_a, "{root}/eh/libgcc_eh.a"]).env("PATH", &path),
    );
    steps.push(Step::run("{root}", &[ranlib, "{root}/eh/libgcc_eh.a"]).env("PATH", &path));
    // Each root compiles its own sibling rlibs, dependencies first, under the
    // same roots, so the two-root oracle below covers the libraries as well
    // as the binary.
    let library = |root: &str, name: &str, krate: &str, externs: &[&str]| {
        let source = format!("{root}/source");
        let output = format!("{root}/lib{name}.rlib");
        let lib = format!("{source}/{krate}/src/lib.rs");
        let externs: Vec<String> = externs
            .iter()
            .flat_map(|dep| ["--extern".to_owned(), format!("{dep}={root}/lib{dep}.rlib")])
            .collect();
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
        target_rustc_at_roots(&source, rustc, &args, root, &source)
            .env("PATH", &path)
            .env("SOURCE_DATE_EPOCH", "1")
    };
    let compile = |root: &str| {
        let source = format!("{root}/source");
        let directory = format!("{source}/td-boot/src");
        let output = format!("{root}/bin/td-boot");
        let main = format!("{source}/td-boot/src/main.rs");
        let tpm = format!("td_tpm={root}/libtd_tpm.rlib");
        let protector = format!("td_protector={root}/libtd_protector.rlib");
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
                "-L",
                &dependencies,
                &linker,
                "-L",
                glib,
                &lib_b,
                &bin_b,
                "-Clink-arg=-L{root}/eh",
                "-Clink-arg=-static-libgcc",
                "-o",
                &output,
                &main,
            ],
            root,
            &source,
        )
        .env("PATH", &path)
        .env("SOURCE_DATE_EPOCH", "1")
    };
    // One small representative direct-rustc program is copied below two
    // different source and build roots, then built independently. Their
    // canonical remaps, deterministic build ID, runtime strip, and companion
    // transform must all converge byte for byte.
    for root in ["{root}/profile-repro-a", "{root}/profile-repro-b"] {
        steps.push(library(root, "td_tpm", "td-tpm", &[]));
        steps.push(library(root, "td_json", "td-json", &[]));
        steps.push(library(
            root,
            "td_protector",
            "td-protector",
            &["td_json", "td_tpm"],
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
    // td-boot's own files are `builder/src/affected.rs`'s staging guard's.
    #[test]
    fn every_sibling_crate_source_is_staged() {
        crate::ladder::assert_sibling_sources_staged(
            super::recipe(),
            &["td-tpm", "td-json", "td-protector"],
        );
    }
}

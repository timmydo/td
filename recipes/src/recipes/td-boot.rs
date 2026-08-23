use crate::ladder::{split_target_debug, target_rustc_at_roots};
use crate::types::{Recipe, Step};

// Target-built static deployment verifier and kexec boot shim. The shipped
// source reuses the engine's dependency-free SHA-256 implementation, and its
// ed25519 VERIFIER — which reaches its hash as `crate::sha512`, so the two
// arrive as a pair or the build does not link. `ed25519_sign.rs` is NOT here
// and must not be: this binary verifies and never signs.
const MAIN_RS: &str = include_str!("../../../td-boot/src/main.rs");
const PROTOCOL_RS: &str = include_str!("../../../td-boot/src/protocol.rs");
const REALFILE_RS: &str = include_str!("../../../td-boot/src/realfile.rs");
const SHA256_RS: &str = include_str!("../../../engine/src/sha256.rs");
const SHA512_RS: &str = include_str!("../../../engine/src/sha512.rs");
const ED25519_RS: &str = include_str!("../../../engine/src/ed25519.rs");

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
        Step::WriteFile {
            path: "{src}/td-boot/src/main.rs".into(),
            content: MAIN_RS.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{src}/td-boot/src/protocol.rs".into(),
            content: PROTOCOL_RS.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{src}/td-boot/src/realfile.rs".into(),
            content: REALFILE_RS.into(),
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
    let compile = |root: &str| {
        let source = format!("{root}/source");
        let directory = format!("{source}/td-boot/src");
        let output = format!("{root}/bin/td-boot");
        let main = format!("{source}/td-boot/src/main.rs");
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
    steps.push(compile("{root}/profile-repro-a"));
    steps.push(compile("{root}/profile-repro-b"));
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

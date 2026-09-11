use crate::ladder::{split_target_debug, target_rustc};
use crate::types::{CheckRunner, Recipe, RecipeCheck, Step};

const APPLY_RS: &str = include_str!("../../../td-update/src/apply.rs");
const SHA256_RS: &str = include_str!("../../../engine/src/sha256.rs");
const PROTOCOL_RS: &str = include_str!("../../../td-boot/src/protocol.rs");

const MAIN_RS: &str = include_str!("../../../td-update/src/main.rs");

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

    let steps = vec![
        Step::MkDir {
            path: "{out}/bin".into(),
        },
        Step::WriteFile {
            path: "{src}/td-update/src/main.rs".into(),
            content: MAIN_RS.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{src}/td-update/src/apply.rs".into(),
            content: APPLY_RS.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{src}/engine/src/sha256.rs".into(),
            content: SHA256_RS.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{src}/td-boot/src/protocol.rs".into(),
            content: PROTOCOL_RS.into(),
            exec: false,
        },
        Step::MkDir {
            path: "{root}/eh".into(),
        },
        Step::run("{root}", &[objcopy, libgcc_a, "{root}/eh/libgcc_eh.a"]).env("PATH", &path),
        Step::run("{root}", &[ranlib, "{root}/eh/libgcc_eh.a"]).env("PATH", &path),
        target_rustc(
            "{src}",
            rustc,
            &[
                "--edition",
                "2021",
                "-C",
                "opt-level=s",
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
                "{out}/bin/td-update",
                "{src}/td-update/src/main.rs",
            ],
        )
        .env("PATH", &path)
        .env("SOURCE_DATE_EPOCH", "1"),
        Step::Require {
            paths: vec!["{out}/bin/td-update".into()],
            exec: true,
        },
        split_target_debug("{out}"),
        Step::Require {
            paths: vec![
                "{out}/lib/debug/bin/td-update.debug".into(),
                "{out}/lib/debug/.td-assembly-exception".into(),
            ],
            exec: false,
        },
        Step::assert_static(&["{out}/bin/td-update"]),
        Step::run("{out}", &["{out}/bin/td-update", "--help"]),
    ];

    Recipe::mesboot("td-update", "0.1")
        .native_inputs(&[
            "rust-toolchain",
            "gcc-x86-64-self",
            "binutils-x86-64-self",
            "glibc-x86-64",
        ])
        .steps(steps)
        .checks(vec![RecipeCheck::new(
            "exec \"$TD_RECIPE_EVAL\" check-run td-update 1\n",
        )
        .with_runner(CheckRunner::BuildOnly)])
}

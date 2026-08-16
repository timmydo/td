use crate::types::{Recipe, Step};

// Target-built static disk installer. The shipped source reuses the engine's
// GPT and FAT32 writers and the on-disk shape td-boot declares, so the writer
// and the reader of a td disk are built from one description of it. `gpt.rs`
// reaches its checksum as `crate::crc32`, so those two arrive as a pair or the
// build does not link.
const MAIN_RS: &str = include_str!("../../../td-install/src/main.rs");
const PROTOCOL_RS: &str = include_str!("../../../td-boot/src/protocol.rs");
const CRC32_RS: &str = include_str!("../../../engine/src/crc32.rs");
const GPT_RS: &str = include_str!("../../../engine/src/gpt.rs");
const FAT_RS: &str = include_str!("../../../engine/src/fat.rs");

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
        Step::WriteFile {
            path: "{src}/td-install/src/main.rs".into(),
            content: MAIN_RS.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{src}/td-boot/src/protocol.rs".into(),
            content: PROTOCOL_RS.into(),
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
    steps.push(
        Step::run(
            "{src}/td-install/src",
            &[
                rustc,
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
                "-C",
                "strip=symbols",
                &linker,
                "-L",
                glib,
                &lib_b,
                &bin_b,
                "-Clink-arg=-L{root}/eh",
                "-Clink-arg=-static-libgcc",
                "--remap-path-prefix",
                "{src}=/td-build",
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

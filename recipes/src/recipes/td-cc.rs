use crate::ladder::{split_target_debug, target_rustc};
use crate::types::{CheckRunner, Recipe, RecipeCheck, Step};

const MAIN: &str = include_str!("../../../td-cc/src/main.rs");

pub fn recipe() -> Recipe {
    let gcc_root = "{in:gcc-x86-64-self}/stage/td/store/gcc-14.3.0-x86_64-self";
    let gcc = format!("{gcc_root}/bin/gcc");
    let glibc = "{in:glibc-x86-64}/stage/td/store/glibc-2.41-x86_64";
    let binutils = "{in:binutils-x86-64-self}";
    let path = format!("{binutils}/bin:{gcc_root}/bin");
    let mut steps = vec![
        Step::MkDir {
            path: "{out}/bin".into(),
        },
        Step::MkDir {
            path: "{out}/lib".into(),
        },
        Step::WriteFile {
            path: "{src}/main.rs".into(),
            content: MAIN.into(),
            exec: false,
        },
    ];
    // The source-built GCC folds the unwinder into libgcc.a; static Rust
    // links also request libgcc_eh.a. Publish the same compatibility archive.
    steps.push(
        Step::run(
            "{root}",
            &[
                "{in:binutils-x86-64-self}/bin/objcopy",
                &format!("{gcc_root}/lib/gcc/x86_64-pc-linux-gnu/14.3.0/libgcc.a"),
                "{out}/lib/libgcc_eh.a",
            ],
        )
        .env("PATH", &path),
    );
    steps.push(
        Step::run(
            "{root}",
            &[
                "{in:binutils-x86-64-self}/bin/ranlib",
                "{out}/lib/libgcc_eh.a",
            ],
        )
        .env("PATH", &path),
    );
    steps.push(
        target_rustc(
            "{src}",
            "{in:rust-toolchain}/bin/rustc",
            &[
                "--edition=2021",
                "--target=x86_64-unknown-linux-gnu",
                "-Copt-level=s",
                "-Cpanic=abort",
                "-Ctarget-feature=+crt-static",
                "-Crelocation-model=static",
                &format!("-Clinker={gcc}"),
                &format!("-Clink-arg=-B{binutils}/bin/"),
                &format!("-Clink-arg=-B{glibc}/lib/"),
                &format!("-L{glibc}/lib"),
                "-Clink-arg=-L{out}/lib",
                "-Clink-arg=-static-libgcc",
                "-o",
                "{out}/bin/td-cc",
                "{src}/main.rs",
            ],
        )
        .env("PATH", &path)
        .env("TD_CC_GCC", gcc_root)
        .env("TD_CC_BINUTILS", binutils)
        .env("TD_CC_GLIBC", glibc)
        .env("TD_CC_UNWIND", "{out}/lib")
        .env("SOURCE_DATE_EPOCH", "1"),
    );
    for name in ["cc", "gcc", "c++", "g++"] {
        steps.push(Step::Symlink {
            target: "td-cc".into(),
            link: format!("{{out}}/bin/{name}"),
        });
    }
    steps.push(split_target_debug("{out}"));
    steps.push(Step::assert_static(&["{out}/bin/td-cc"]));

    // Exercise the installed launcher with no ambient PATH. These programs
    // need libc headers, a Linux header, startup files, unwinding and C++ std.
    for (name, driver, source) in [
        ("c", "cc", "#include <stdio.h>\n#include <linux/types.h>\nint main(void) { __u32 x = 42; return x == 42 && puts(\"TD-CC-C-OK\") >= 0 ? 0 : 1; }\n"),
        ("cpp", "c++", "#include <vector>\n#include <stdexcept>\nint main() { std::vector<int> x{1,2,3}; try { throw std::runtime_error(\"ok\"); } catch (const std::exception&) { return x.at(2) == 3 ? 0 : 1; } return 2; }\n"),
    ] {
        steps.push(Step::WriteFile { path: format!("{{root}}/probe.{name}"), content: source.into(), exec: false });
        steps.push(Step::run("{root}", &[
            &format!("{{out}}/bin/{driver}"), &format!("{{root}}/probe.{name}"),
            "-o", &format!("{{root}}/probe-{name}"),
        ]).env("PATH", "/nonexistent"));
        steps.push(Step::run("{root}", &[&format!("{{root}}/probe-{name}")]).env("PATH", "/nonexistent"));
    }
    steps.push(Step::WriteFile {
        path: "{root}/freestanding.c".into(),
        content: "#if __has_include(<stdio.h>)\n#error libc header leaked through -nostdinc\n#endif\nint answer(void) { return 42; }\n".into(),
        exec: false,
    });
    steps.push(
        Step::run(
            "{root}",
            &[
                "{out}/bin/cc",
                "-nostdinc",
                "-Werror",
                "-c",
                "{root}/freestanding.c",
                "-o",
                "{root}/freestanding.o",
            ],
        )
        .env("PATH", "/nonexistent"),
    );
    for arguments in [
        vec!["-Werror", "-c", "{root}/probe.cpp", "-o", "{root}/probe.o"],
        vec![
            "-shared",
            "-fPIC",
            "{root}/probe.cpp",
            "-o",
            "{root}/probe.so",
        ],
    ] {
        let mut argv = vec!["{out}/bin/c++"];
        argv.extend(arguments);
        steps.push(Step::run("{root}", &argv).env("PATH", "/nonexistent"));
    }
    steps.push(Step::WriteFile {
        path: "{root}/cargo-probe/Cargo.toml".into(),
        content: "[package]\nname = \"native-probe\"\nversion = \"0.0.0\"\nedition = \"2021\"\n[workspace]\n".into(), exec: false,
    });
    steps.push(Step::WriteFile {
        path: "{root}/cargo-probe/src/main.rs".into(),
        content: "fn main() { println!(\"TD-CC-CARGO-OK\"); }\n#[test] fn smoke() { assert_eq!(6 * 7, 42); }\n".into(), exec: false,
    });
    for verb in ["build", "test"] {
        steps.push(
            Step::run(
                "{root}/cargo-probe",
                &[
                    "{in:rust-toolchain}/bin/cargo",
                    verb,
                    "--offline",
                    "--target-dir",
                    "{root}/cargo-target",
                ],
            )
            .env("PATH", "{out}/bin:{in:rust-toolchain}/bin")
            .env("HOME", "{root}/home")
            .env("CARGO_HOME", "{root}/cargo-home")
            .env("RUSTC", "{in:rust-toolchain}/bin/rustc")
            .env("RUSTDOC", "{in:rust-toolchain}/bin/rustdoc")
            .env(
                "RUSTFLAGS",
                "-Cforce-frame-pointers=yes -Cdebuginfo=line-tables-only",
            )
            .env(
                "CARGO_ENCODED_RUSTFLAGS",
                "-Cforce-frame-pointers=yes\x1f-Cdebuginfo=line-tables-only",
            ),
        );
    }
    steps.push(
        Step::run("{root}", &["{root}/cargo-target/debug/native-probe"])
            .env("PATH", "/nonexistent"),
    );
    steps.push(
        target_rustc(
            "{root}/cargo-probe",
            "{in:rust-toolchain}/bin/rustc",
            &[
                "src/main.rs",
                "--edition=2021",
                "-Ctarget-feature=+crt-static",
                "-Crelocation-model=static",
                "-Cforce-frame-pointers=yes",
                "-Cdebuginfo=line-tables-only",
                "-o",
                "{root}/static-rust-probe",
            ],
        )
        .env("PATH", "{out}/bin"),
    );
    steps.push(Step::assert_static(&["{root}/static-rust-probe"]));
    steps.push(Step::run("{root}", &["{root}/static-rust-probe"]).env("PATH", "/nonexistent"));
    Recipe::mesboot("td-cc", "0.1")
        .native_inputs(&[
            "rust-toolchain",
            "gcc-x86-64-self",
            "binutils-x86-64-self",
            "glibc-x86-64",
        ])
        .steps(steps)
        .checks(vec![RecipeCheck::new(
            "exec \"$TD_RECIPE_EVAL\" check-run td-cc 1\n",
        )
        .with_runner(CheckRunner::BuildOnly)])
}

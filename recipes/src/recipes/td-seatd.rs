use crate::ladder::{split_target_debug, target_rustc};
use crate::types::{Recipe, Step};

const MAIN_RS: &str = include_str!("../../../td-seatd/src/main.rs");

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
            path: "{src}/main.rs".into(),
            content: MAIN_RS.into(),
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
                "{out}/bin/td-seatd",
                "{src}/main.rs",
            ],
        )
        .env("PATH", &path)
        .env("SOURCE_DATE_EPOCH", "1"),
        Step::Require {
            paths: vec!["{out}/bin/td-seatd".into()],
            exec: true,
        },
        split_target_debug("{out}"),
        Step::assert_static(&["{out}/bin/td-seatd"]),
    ];

    Recipe::mesboot("td-seatd", "0.1")
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
    use super::super::system_x86_64::AUDIO_RUNTIME;
    use super::*;

    #[test]
    fn recipe_embeds_the_linted_crate_source() {
        let steps = recipe().steps.expect("td-seatd steps");
        let production = MAIN_RS
            .split("\n#[cfg(test)]")
            .next()
            .expect("td-seatd production source");
        assert!(steps.iter().any(|step| {
            matches!(
                step,
                Step::WriteFile { path, content, .. }
                    if path == "{src}/main.rs" && content == MAIN_RS
            )
        }));
        assert!(production.contains("#![forbid(unsafe_code)]"));
        assert!(production.contains("prepare_owned_runtime(path, account, 0o700)"));
        assert!(production.contains("verify_owner_mode(path, account, mode)"));
        assert!(production.contains(&format!("const AUDIO_RUNTIME: &str = {AUDIO_RUNTIME:?};")));
        assert!(production.contains("verify_runtime_base(base, require_root_base)?;"));
        assert!(production.contains("prepare_owned_runtime(path, account, 0o755)"));
        assert!(production.contains(
            "prepare_audio_runtime(audio_runtime, assignment.audio, require_char)?;"
        ));
        assert!(production.contains(
            "verify_owner_mode(audio_runtime, assignment.audio, 0o755)?;"
        ));
    }
}

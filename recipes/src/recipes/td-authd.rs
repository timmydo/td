use crate::ladder::{split_target_debug, target_rustc};
use crate::types::{CheckRunner, Recipe, RecipeCheck, Step};

const SOURCES: &[(&str, &str)] = &[
    ("src/main.rs", include_str!("../../../td-authd/src/main.rs")),
    (
        "src/channel.rs",
        include_str!("../../../td-authd/src/channel.rs"),
    ),
    ("src/sys.rs", include_str!("../../../td-authd/src/sys.rs")),
    (
        "tests/channel.rs",
        include_str!("../../../td-authd/tests/channel.rs"),
    ),
    (
        "tests/sys.rs",
        include_str!("../../../td-authd/tests/sys.rs"),
    ),
    (
        "tests/confinement.rs",
        include_str!("../../../td-authd/tests/confinement.rs"),
    ),
];

pub fn recipe() -> Recipe {
    // The self-hosted toolchains install under a nested stage/td/store/<pkg>
    // DESTDIR (re the /td/store prefix); rust-toolchain installs flat.
    let rustc = "{in:rust-toolchain}/bin/rustc";
    let gcc = "{in:gcc-x86-64-self}/stage/td/store/gcc-14.3.0-x86_64-self/bin/gcc";
    let gccbin = "{in:gcc-x86-64-self}/stage/td/store/gcc-14.3.0-x86_64-self/bin";
    let bbin = "{in:binutils-x86-64-self}/bin";
    let glib = "{in:glibc-x86-64}/stage/td/store/glibc-2.41-x86_64/lib";
    // gcc-x86-64-self folds the unwinder objects INTO libgcc.a and never emits a
    // separate static libgcc_eh.a. A `-static` rustc link still passes `-lgcc_eh`,
    // so synthesize one from libgcc.a — the same workaround td-util documents.
    let objcopy = "{in:binutils-x86-64-self}/bin/objcopy";
    let ranlib = "{in:binutils-x86-64-self}/bin/ranlib";
    let libgcc_a = "{in:gcc-x86-64-self}/stage/td/store/gcc-14.3.0-x86_64-self/lib/gcc/x86_64-pc-linux-gnu/14.3.0/libgcc.a";

    // Bound so they outlive the argv slice; `&String` deref-coerces to `&str`.
    let linker = format!("-Clinker={gcc}");
    let lib_b = format!("-Clink-arg=-B{glib}");
    let bin_b = format!("-Clink-arg=-B{bbin}");
    let path = format!("{bbin}:{gccbin}");

    let mut steps = Vec::new();
    steps.push(Step::MkDir {
        path: "{out}/bin".into(),
    });
    for directory in ["{src}/td-authd/src", "{src}/td-authd/tests"] {
        steps.push(Step::MkDir {
            path: directory.into(),
        });
    }
    for (name, source) in SOURCES {
        steps.push(Step::WriteFile {
            path: format!("{{src}}/td-authd/{name}"),
            content: (*source).into(),
            exec: false,
        });
    }
    steps.push(Step::MkDir {
        path: "{root}/eh".into(),
    });
    steps.push(
        Step::run("{root}", &[objcopy, libgcc_a, "{root}/eh/libgcc_eh.a"]).env("PATH", &path),
    );
    steps.push(Step::run("{root}", &[ranlib, "{root}/eh/libgcc_eh.a"]).env("PATH", &path));
    steps.push(
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
                // Mirror the crate's [profile.release] (cargo never sees this
                // direct rustc build): abort — not unwind — on panic. The
                // shared target policy deliberately preserves symbols.
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
                "{out}/bin/td-authd",
                "{src}/td-authd/src/main.rs",
            ],
        )
        .env("PATH", &path)
        .env("SOURCE_DATE_EPOCH", "1"),
    );
    steps.push(Step::Require {
        paths: vec!["{out}/bin/td-authd".into()],
        exec: true,
    });
    steps.push(
        target_rustc(
            "{src}",
            rustc,
            &[
                "--edition",
                "2021",
                "--test",
                "--crate-name",
                "td_authd_tests",
                "--target",
                "x86_64-unknown-linux-gnu",
                "-C",
                "target-feature=+crt-static",
                "-C",
                "relocation-model=static",
                &linker,
                "-L",
                glib,
                &lib_b,
                &bin_b,
                "-Clink-arg=-L{root}/eh",
                "-Clink-arg=-static-libgcc",
                "-o",
                "{root}/channel-tests",
                "{src}/td-authd/src/main.rs",
            ],
        )
        .env("PATH", &path)
        .env("CARGO_MANIFEST_DIR", "{src}/td-authd")
        .env("SOURCE_DATE_EPOCH", "1"),
    );
    steps.push(Step::run("{root}", &["{root}/channel-tests"]));
    steps.push(split_target_debug("{out}"));
    steps.push(Step::assert_static(&["{out}/bin/td-authd"]));

    Recipe::mesboot("td-authd", "0.1")
        .native_inputs(&[
            "rust-toolchain",
            "gcc-x86-64-self",
            "binutils-x86-64-self",
            "glibc-x86-64",
        ])
        .steps(steps)
        .checks(vec![RecipeCheck::new(
            r#"
: "${TD_RECIPE_EVAL:=$PWD/target/release/td-recipe-eval}"
exec "$TD_RECIPE_EVAL" check-run td-authd 1
"#,
        )
        .with_runner(CheckRunner::BuildOnly)])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn staged_sources_match_the_complete_crate_and_have_no_live_templates() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../td-authd");
        let mut actual = Vec::new();
        for directory in ["src", "tests"] {
            for entry in std::fs::read_dir(root.join(directory)).unwrap() {
                let entry = entry.unwrap();
                assert!(entry.file_type().unwrap().is_file());
                actual.push(format!(
                    "{directory}/{}",
                    entry.file_name().to_str().unwrap()
                ));
            }
        }
        let mut embedded: Vec<String> = SOURCES
            .iter()
            .map(|(name, _)| (*name).to_string())
            .collect();
        actual.sort();
        embedded.sort();
        assert_eq!(actual, embedded);
        for (name, source) in SOURCES {
            assert_eq!(std::fs::read_to_string(root.join(name)).unwrap(), *source);
            for template in [
                "{root}",
                "{src}",
                "{out}",
                "{tools}",
                "{jobs}",
                "{in:",
                "{payload:",
            ] {
                assert!(
                    !source.contains(template),
                    "{name}: live template {template}"
                );
            }
        }
    }
}

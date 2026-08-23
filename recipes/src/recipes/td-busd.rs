use crate::types::{Recipe, Step};

const MAIN_RS: &str = include_str!("../../../td-busd/src/main.rs");
const MODULES: &[(&str, &str)] = &[
    ("auth", include_str!("../../../td-busd/src/auth.rs")),
    ("corpus", include_str!("../../../td-busd/src/corpus.rs")),
    ("message", include_str!("../../../td-busd/src/message.rs")),
    ("name", include_str!("../../../td-busd/src/name.rs")),
    ("recorded", include_str!("../../../td-busd/src/recorded.rs")),
    ("wire", include_str!("../../../td-busd/src/wire.rs")),
];

#[cfg(test)]
fn declared_modules() -> Vec<&'static str> {
    let mut modules = Vec::new();
    for line in MAIN_RS.lines() {
        if let Some(module) = line
            .trim()
            .strip_prefix("mod ")
            .and_then(|rest| rest.strip_suffix(';'))
        {
            modules.push(module);
        }
    }
    modules
}

pub fn recipe() -> Recipe {
    // The self-hosted toolchains install under a nested stage/td/store/<pkg>
    // DESTDIR (re the /td/store prefix); rust-toolchain installs flat.
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
        Step::WriteFile {
            path: "{src}/main.rs".into(),
            content: MAIN_RS.into(),
            exec: false,
        },
    ];
    for (name, source) in MODULES {
        steps.push(Step::WriteFile {
            path: format!("{{src}}/{name}.rs"),
            content: (*source).into(),
            exec: false,
        });
    }
    // gcc-x86-64-self folds the unwinder objects INTO libgcc.a and emits no
    // separate static libgcc_eh.a, while a `-static` rustc link still passes
    // `-lgcc_eh`. Synthesize one (objcopy preserves the members, ranlib writes
    // the index ld needs) and put it on the search path.
    steps.push(Step::MkDir {
        path: "{root}/eh".into(),
    });
    steps.push(
        Step::run("{root}", &[objcopy, libgcc_a, "{root}/eh/libgcc_eh.a"]).env("PATH", &path),
    );
    steps.push(Step::run("{root}", &[ranlib, "{root}/eh/libgcc_eh.a"]).env("PATH", &path));
    steps.push(
        Step::run(
            "{src}",
            &[
                rustc,
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
                "{out}/bin/td-busd",
                "{src}/main.rs",
            ],
        )
        .env("PATH", &path)
        .env("SOURCE_DATE_EPOCH", "1"),
    );
    steps.push(Step::Require {
        paths: vec!["{out}/bin/td-busd".into()],
        exec: true,
    });
    steps.push(Step::assert_static(&["{out}/bin/td-busd"]));

    Recipe::mesboot("td-busd", "0.1")
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

    #[test]
    fn recipe_stages_every_declared_module() {
        let mut declared = declared_modules();
        let mut staged: Vec<&str> = MODULES.iter().map(|(name, _)| *name).collect();
        declared.sort_unstable();
        staged.sort_unstable();
        assert_eq!(declared, staged);
    }

    /// The broker has no `UNSAFE.md` entry, and the shipped source is what that
    /// claim is about — not the crate a host cargo happens to lint. Taking the
    /// raw-syscall surface #10 needs will red this, which is the point: the
    /// amendment is meant to be a landing rather than a diff nobody noticed.
    #[test]
    fn the_shipped_source_forbids_the_unsafe_lint() {
        assert!(MAIN_RS.contains("#![forbid(unsafe_code)]"));
        let keyword = format!("un{}", "safe");
        let lint = format!("{keyword}_code");
        for (module, text) in std::iter::once(&("main", MAIN_RS)).chain(MODULES.iter()) {
            let bare = text
                .matches(&keyword)
                .count()
                .saturating_sub(text.matches(&lint).count());
            assert_eq!(bare, 0, "{module} names the {keyword} keyword");
        }
    }

    #[test]
    fn recipe_embeds_the_linted_crate_source() {
        let steps = recipe().steps.expect("td-busd steps");
        assert!(steps.iter().any(|step| {
            matches!(
                step,
                Step::WriteFile { path, content, .. }
                    if path == "{src}/main.rs" && content == MAIN_RS
            )
        }));
        for (name, source) in MODULES {
            let staged = format!("{{src}}/{name}.rs");
            assert!(
                steps.iter().any(|step| {
                    matches!(
                        step,
                        Step::WriteFile { path, content, .. }
                            if *path == staged && content == *source
                    )
                }),
                "{name} is not staged"
            );
        }
    }
}

use crate::ladder::{split_target_debug, target_rustc};
use crate::types::{Recipe, Step};

const MAIN_RS: &str = include_str!("../../../td-busd/src/main.rs");
const MODULES: &[(&str, &str)] = &[
    ("auth", include_str!("../../../td-busd/src/auth.rs")),
    ("authscript", include_str!("../../../td-busd/src/authscript.rs")),
    ("corpus", include_str!("../../../td-busd/src/corpus.rs")),
    ("lineage", include_str!("../../../td-busd/src/lineage.rs")),
    ("message", include_str!("../../../td-busd/src/message.rs")),
    ("name", include_str!("../../../td-busd/src/name.rs")),
    ("recorded", include_str!("../../../td-busd/src/recorded.rs")),
    ("registry", include_str!("../../../td-busd/src/registry.rs")),
    ("sys", include_str!("../../../td-busd/src/sys.rs")),
    ("transport", include_str!("../../../td-busd/src/transport.rs")),
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
    steps.push(split_target_debug("{out}"));
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

    /// `UNSAFE.md` §10, checked against the SHIPPED source — not the crate a
    /// host cargo happens to lint. The crate has its own confinement tests;
    /// this is the one that binds the roster to the bytes the recipe writes
    /// into the store, which is what the roster is a claim about.
    ///
    /// Its predecessor asserted the crate carried NO surface, and taking one
    /// redded it. That was the design: the amendment had to be a landing
    /// rather than a diff nobody noticed, and it was.
    #[test]
    fn the_shipped_source_confines_the_unsafe_lint_to_surface_ten() {
        let keyword = format!("un{}", "safe");
        let lint = format!("{keyword}_code");
        assert!(MAIN_RS.contains(&format!("#![deny({lint})]")));
        assert!(!MAIN_RS.contains(&format!("#![forbid({lint})]")));
        for (module, text) in std::iter::once(&("main", MAIN_RS)).chain(MODULES.iter()) {
            let bare = text
                .matches(&keyword)
                .count()
                .saturating_sub(text.matches(&lint).count());
            if *module == "sys" {
                continue;
            }
            assert_eq!(bare, 0, "{module} names the {keyword} keyword");
        }
    }

    /// The roster itself, by number, against the shipped bytes: two scoped
    /// allows and three syscalls. `close(2)` is deliberately off it, because
    /// the `OwnedFd` adoption means `std` performs every close.
    #[test]
    fn the_shipped_syscall_layer_is_the_rostered_surface() {
        let lint = format!("un{}_code", "safe");
        let sys = MODULES
            .iter()
            .find(|(name, _)| *name == "sys")
            .map(|(_, text)| *text)
            .unwrap_or_default();
        assert_eq!(sys.matches(&format!("#[allow({lint})]")).count(), 2);
        assert_eq!(sys.matches("const SYS_").count(), 3);
        for (name, number) in [
            ("SYS_SENDMSG", "46"),
            ("SYS_RECVMSG", "47"),
            ("SYS_GETSOCKOPT", "55"),
        ] {
            assert!(
                sys.contains(&format!("const {name}: usize = {number};")),
                "{name} is not pinned to {number}"
            );
        }
        assert!(!sys.contains("SYS_CLOSE"));
        assert_eq!(sys.matches("OwnedFd::from_raw_fd").count(), 1);
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

use crate::ladder::{split_target_debug, target_rustc};
use crate::types::{Recipe, Step};

const MAIN_RS: &str = include_str!("../../../td-portal/src/main.rs");
const SETTINGS_RS: &str = include_str!("../../../td-portal/src/settings.rs");
const DEFAULT_SETTINGS: &str = include_str!("../../../td-portal/default-settings.conf");
const SHARED_DBUS: &[(&str, &str)] = &[
    (
        "{src}/td-busd/src/message.rs",
        include_str!("../../../td-busd/src/message.rs"),
    ),
    (
        "{src}/td-busd/src/name.rs",
        include_str!("../../../td-busd/src/name.rs"),
    ),
    (
        "{src}/td-busd/src/wire.rs",
        include_str!("../../../td-busd/src/wire.rs"),
    ),
];

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
            path: "{src}/td-portal/src".into(),
        },
        Step::MkDir {
            path: "{src}/td-busd/src".into(),
        },
        Step::WriteFile {
            path: "{src}/td-portal/src/main.rs".into(),
            content: MAIN_RS.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{src}/td-portal/src/settings.rs".into(),
            content: SETTINGS_RS.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{src}/td-portal/default-settings.conf".into(),
            content: DEFAULT_SETTINGS.into(),
            exec: false,
        },
    ];
    for (staged_path, source) in SHARED_DBUS {
        steps.push(Step::WriteFile {
            path: (*staged_path).into(),
            content: (*source).into(),
            exec: false,
        });
    }
    steps.extend([
        Step::MkDir {
            path: "{root}/eh".into(),
        },
        Step::run("{root}", &[objcopy, libgcc_a, "{root}/eh/libgcc_eh.a"]).env("PATH", &path),
        Step::run("{root}", &[ranlib, "{root}/eh/libgcc_eh.a"]).env("PATH", &path),
        target_rustc(
            "{src}/td-portal/src",
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
                "{out}/bin/td-portal",
                "{src}/td-portal/src/main.rs",
            ],
        )
        .env("PATH", &path)
        .env("SOURCE_DATE_EPOCH", "1"),
        Step::Require {
            paths: vec!["{out}/bin/td-portal".into()],
            exec: true,
        },
        Step::run("{out}", &["{out}/bin/td-portal", "selftest"]),
        split_target_debug("{out}"),
        Step::assert_static(&["{out}/bin/td-portal"]),
    ]);

    Recipe::mesboot("td-portal", "0.1")
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
    use crate::ladder::TD_PORTAL_RUNTIME_MARKER;

    #[test]
    fn recipe_stages_the_portal_and_the_canonical_broker_codec() {
        let steps = recipe().steps.expect("td-portal steps");
        for (path, source) in [
            ("{src}/td-portal/src/main.rs", MAIN_RS),
            ("{src}/td-portal/src/settings.rs", SETTINGS_RS),
            ("{src}/td-portal/default-settings.conf", DEFAULT_SETTINGS),
        ] {
            assert!(steps.iter().any(|step| {
                matches!(step, Step::WriteFile { path: got, content, .. }
                    if got == path && content == source)
            }));
        }
        for (path, source) in SHARED_DBUS {
            assert!(steps.iter().any(|step| {
                matches!(step, Step::WriteFile { path: got, content, .. }
                    if got == path && content == *source)
            }));
        }
    }

    #[test]
    fn every_declared_portal_module_is_staged() {
        let declared = MAIN_RS.lines().filter_map(|line| {
            let line = line.trim();
            line.strip_prefix("mod ")
                .and_then(|tail| tail.strip_suffix(';'))
        });
        let steps = recipe().steps.expect("td-portal steps");
        let staged: Vec<&str> = steps
            .iter()
            .filter_map(|step| match step {
                Step::WriteFile { path, .. } => path
                    .rsplit('/')
                    .next()
                    .and_then(|path| path.strip_suffix(".rs")),
                _ => None,
            })
            .collect();
        for module in declared {
            assert!(staged.contains(&module), "module {module} is not staged");
        }
    }

    #[test]
    fn target_selftest_and_static_assertion_cover_the_shipped_binary() {
        let steps = recipe().steps.expect("td-portal steps");
        assert!(steps.iter().any(|step| {
            matches!(step, Step::Run { argv, .. }
                if argv == &vec!["{out}/bin/td-portal".to_string(), "selftest".to_string()])
        }));
        assert!(steps.iter().any(|step| {
            matches!(step, Step::AssertStatic { paths }
                if paths == &vec!["{out}/bin/td-portal".to_string()])
        }));
        assert!(
            MAIN_RS.contains(&format!(
                "pub const READY_MARKER: &str = \"{TD_PORTAL_RUNTIME_MARKER}\";"
            )),
            "the target probe and QEMU scanner must share one exact evidence line"
        );
        assert!(MAIN_RS.contains("println!(\"{READY_MARKER}\");"));
    }
}

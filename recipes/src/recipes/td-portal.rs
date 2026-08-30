use crate::ladder::{split_target_debug, target_rustc};
use crate::types::{Recipe, Step};

#[cfg(test)]
use crate::ladder::TD_JAIL_FIXTURE_DOWNLOAD_TARGET;
#[cfg(test)]
const SYSTEM_X86_64_RS: &str = include_str!("system-x86-64.rs");

const MAIN_RS: &str = include_str!("../../../td-portal/src/main.rs");
const FILE_CHOOSER_RS: &str = include_str!("../../../td-portal/src/file_chooser.rs");
const HANDLES_RS: &str = include_str!("../../../td-portal/src/handles.rs");
const SETTINGS_RS: &str = include_str!("../../../td-portal/src/settings.rs");
const SYS_RS: &str = include_str!("../../../td-portal/src/sys.rs");
const WAYLAND_CHANNEL_RS: &str = include_str!("../../../td-portal/src/wayland_channel.rs");
const WAYLAND_DIALOG_RS: &str = include_str!("../../../td-portal/src/wayland_dialog.rs");
const COMPOSITOR_WIRE_RS: &str = include_str!("../../../td-compositor/src/wire.rs");
const COMPOSITOR_FILTER_RS: &str = include_str!("../../../td-compositor/src/filter.rs");
const COMPOSITOR_FONT_RS: &str = include_str!("../../../td-compositor/src/font.rs");
const COMPOSITOR_FONT_DATA_RS: &str = include_str!("../../../td-compositor/src/font_data.rs");
const COMPOSITOR_KEYBOARD_RS: &str = include_str!("../../../td-compositor/src/keyboard.rs");
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
        Step::MkDir {
            path: "{src}/td-compositor/src".into(),
        },
        Step::WriteFile {
            path: "{src}/td-portal/src/main.rs".into(),
            content: MAIN_RS.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{src}/td-portal/src/file_chooser.rs".into(),
            content: FILE_CHOOSER_RS.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{src}/td-portal/src/handles.rs".into(),
            content: HANDLES_RS.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{src}/td-portal/src/settings.rs".into(),
            content: SETTINGS_RS.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{src}/td-portal/src/sys.rs".into(),
            content: SYS_RS.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{src}/td-portal/src/wayland_channel.rs".into(),
            content: WAYLAND_CHANNEL_RS.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{src}/td-portal/src/wayland_dialog.rs".into(),
            content: WAYLAND_DIALOG_RS.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{src}/td-compositor/src/wire.rs".into(),
            content: COMPOSITOR_WIRE_RS.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{src}/td-compositor/src/filter.rs".into(),
            content: COMPOSITOR_FILTER_RS.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{src}/td-compositor/src/font.rs".into(),
            content: COMPOSITOR_FONT_RS.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{src}/td-compositor/src/font_data.rs".into(),
            content: COMPOSITOR_FONT_DATA_RS.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{src}/td-compositor/src/keyboard.rs".into(),
            content: COMPOSITOR_KEYBOARD_RS.into(),
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
    use crate::ladder::{
        TD_PORTAL_CHANNEL_RUNTIME_MARKER, TD_PORTAL_REQUEST_RUNTIME_MARKER,
        TD_PORTAL_RUNTIME_MARKER,
    };

    #[test]
    fn recipe_stages_the_portal_and_the_canonical_broker_codec() {
        let steps = recipe().steps.expect("td-portal steps");
        for (path, source) in [
            ("{src}/td-portal/src/main.rs", MAIN_RS),
            ("{src}/td-portal/src/file_chooser.rs", FILE_CHOOSER_RS),
            ("{src}/td-portal/src/handles.rs", HANDLES_RS),
            ("{src}/td-portal/src/settings.rs", SETTINGS_RS),
            ("{src}/td-portal/src/sys.rs", SYS_RS),
            ("{src}/td-portal/src/wayland_channel.rs", WAYLAND_CHANNEL_RS),
            ("{src}/td-portal/src/wayland_dialog.rs", WAYLAND_DIALOG_RS),
            ("{src}/td-compositor/src/wire.rs", COMPOSITOR_WIRE_RS),
            ("{src}/td-compositor/src/filter.rs", COMPOSITOR_FILTER_RS),
            ("{src}/td-compositor/src/font.rs", COMPOSITOR_FONT_RS),
            (
                "{src}/td-compositor/src/font_data.rs",
                COMPOSITOR_FONT_DATA_RS,
            ),
            (
                "{src}/td-compositor/src/keyboard.rs",
                COMPOSITOR_KEYBOARD_RS,
            ),
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
            if matches!(
                module,
                "wayland_wire" | "font" | "font_data" | "keyboard" | "list_filter"
            ) {
                let path = match module {
                    "wayland_wire" => "../../td-compositor/src/wire.rs",
                    "font" => "../../td-compositor/src/font.rs",
                    "font_data" => "../../td-compositor/src/font_data.rs",
                    "keyboard" => "../../td-compositor/src/keyboard.rs",
                    "list_filter" => "../../td-compositor/src/filter.rs",
                    _ => "",
                };
                assert!(MAIN_RS.contains(&format!("#[path = \"{path}\"]")));
                assert!(MAIN_RS.contains(&format!("mod {module};")));
                continue;
            }
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
        assert!(MAIN_RS.contains(&format!(
            "pub const REQUEST_READY_MARKER: &str = \"{TD_PORTAL_REQUEST_RUNTIME_MARKER}\";"
        )));
        assert!(WAYLAND_CHANNEL_RS.contains("EXPECTED_GLOBALS.len()"));
        assert!(
            WAYLAND_CHANNEL_RS.contains("TD-PORTAL-CHANNEL-READY globals={} privileged=1 dialog=2")
        );
        assert_eq!(
            TD_PORTAL_CHANNEL_RUNTIME_MARKER,
            "TD-PORTAL-CHANNEL-READY globals=11 privileged=1 dialog=2"
        );
        assert!(MAIN_RS.contains("println!(\"{READY_MARKER}\");"));
        assert!(MAIN_RS.contains("println!(\"{REQUEST_READY_MARKER}\");"));
        assert!(MAIN_RS.contains("println!(\"{}\", wayland_channel::ready_marker());"));
    }

    #[test]
    fn portal_and_firefox_share_the_exact_download_grant_pair() {
        assert_eq!(TD_JAIL_FIXTURE_DOWNLOAD_TARGET, "/home/td/Downloads");
        assert!(MAIN_RS
            .contains("const FIREFOX_HOST_DOWNLOADS: &str = \"/var/home/tester/Downloads\";"));
        assert!(MAIN_RS.contains("const FIREFOX_GUEST_DOWNLOADS: &str = \"/home/td/Downloads\";"));
        assert!(SYSTEM_X86_64_RS
            .contains("const FIREFOX_DOWNLOAD_SOURCE: &str = \"/var/home/tester/Downloads\";"));
        assert!(SYSTEM_X86_64_RS.contains(
            r#"user_pref(\\\"browser.download.dir\\\", \\\"/home/td/Downloads\\\");"#
        ));
    }
}

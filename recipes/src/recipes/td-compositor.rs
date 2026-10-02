use crate::ladder::{split_target_debug, target_rustc};
use crate::types::{Recipe, Step};

// Every source below is written out with a WriteFile, which the ladder
// `no_bootstrap_step_invokes_host_find_or_xargs` guard scans as a command
// surface. A `.rs` body is read only INSIDE its string literals, so an
// identifier like `Iterator::find` is free; what must not appear is a bare
// `find`/`xargs` in a LITERAL, which reads exactly as a command name would.
// That guard's roster exempts named reviewed bodies from even that, and none
// of td-compositor's is on it.
const MAIN_RS: &str = include_str!("../../../td-compositor/src/main.rs");
const MODULES: &[(&str, &str)] = &[
    (
        "secret_client",
        include_str!("../../../td-compositor/src/secret_client.rs"),
    ),
    (
        "app_policy",
        include_str!("../../../td-busd/src/app_policy.rs"),
    ),
    ("atlas", include_str!("../../../td-ui/src/atlas.rs")),
    (
        "attention",
        include_str!("../../../td-compositor/src/attention.rs"),
    ),
    (
        "authority",
        include_str!("../../../td-compositor/src/authority.rs"),
    ),
    ("bar", include_str!("../../../td-compositor/src/bar.rs")),
    (
        "buffer",
        include_str!("../../../td-compositor/src/buffer.rs"),
    ),
    (
        "client",
        include_str!("../../../td-compositor/src/client.rs"),
    ),
    (
        "client_resources",
        include_str!("../../../td-compositor/src/client_resources.rs"),
    ),
    ("clock", include_str!("../../../td-compositor/src/clock.rs")),
    (
        "configure",
        include_str!("../../../td-compositor/src/configure.rs"),
    ),
    ("conn", include_str!("../../../td-compositor/src/conn.rs")),
    (
        "control",
        include_str!("../../../td-compositor/src/control.rs"),
    ),
    ("coverage", include_str!("../../../td-ui/src/coverage.rs")),
    ("drm", include_str!("../../../td-compositor/src/drm.rs")),
    ("face", include_str!("../../../td-ui/src/face.rs")),
    ("face_file", include_str!("../../../td-ui/src/face_file.rs")),
    (
        "filter",
        include_str!("../../../td-compositor/src/filter.rs"),
    ),
    ("font", include_str!("../../../td-compositor/src/font.rs")),
    (
        "font_data",
        include_str!("../../../td-compositor/src/font_data.rs"),
    ),
    (
        "framebuffer",
        include_str!("../../../td-compositor/src/framebuffer.rs"),
    ),
    (
        "headless",
        include_str!("../../../td-compositor/src/headless.rs"),
    ),
    ("help", include_str!("../../../td-compositor/src/help.rs")),
    ("input", include_str!("../../../td-compositor/src/input.rs")),
    (
        "keyboard",
        include_str!("../../../td-compositor/src/keyboard.rs"),
    ),
    (
        "launcher",
        include_str!("../../../td-compositor/src/launcher.rs"),
    ),
    (
        "layout",
        include_str!("../../../td-compositor/src/layout.rs"),
    ),
    (
        "output",
        include_str!("../../../td-compositor/src/output.rs"),
    ),
    (
        "pointer",
        include_str!("../../../td-compositor/src/pointer.rs"),
    ),
    (
        "positioner",
        include_str!("../../../td-compositor/src/positioner.rs"),
    ),
    (
        "proc_status",
        include_str!("../../../td-compositor/src/proc_status.rs"),
    ),
    (
        "reportable",
        include_str!("../../../td-compositor/src/reportable.rs"),
    ),
    (
        "runtime",
        include_str!("../../../td-compositor/src/runtime.rs"),
    ),
    ("scene", include_str!("../../../td-compositor/src/scene.rs")),
    (
        "server",
        include_str!("../../../td-compositor/src/server.rs"),
    ),
    (
        "session",
        include_str!("../../../td-compositor/src/session.rs"),
    ),
    ("sfnt", include_str!("../../../td-ui/src/sfnt.rs")),
    (
        "socket",
        include_str!("../../../td-compositor/src/socket.rs"),
    ),
    ("sys", include_str!("../../../td-compositor/src/sys.rs")),
    ("text", include_str!("../../../td-compositor/src/text.rs")),
    (
        "timezone",
        include_str!("../../../td-compositor/src/timezone.rs"),
    ),
    ("ui", include_str!("../../../td-compositor/src/ui.rs")),
    (
        "vm_bridge",
        include_str!("../../../td-compositor/src/vm_bridge.rs"),
    ),
    (
        "vm_wire",
        include_str!("../../../td-compositor/src/vm_wire.rs"),
    ),
    ("wire", include_str!("../../../td-compositor/src/wire.rs")),
];

/// Sibling sources staged beside the modules, at the paths the
/// `target-recipe` side of each `cfg_attr` mount names.
const STAGED_FILES: &[(&str, &str)] = &[
    (
        "auth/consent.rs",
        include_str!("../../../td-authd/src/consent.rs"),
    ),
    (
        "auth/channel.rs",
        include_str!("../../../td-authd/src/channel.rs"),
    ),
    ("auth/sys.rs", include_str!("../../../td-authd/src/sys.rs")),
    (
        "tests/channel.rs",
        include_str!("../../../td-authd/tests/channel.rs"),
    ),
    (
        "tests/sys.rs",
        include_str!("../../../td-authd/tests/sys.rs"),
    ),
    (
        "tests/fonts.rs",
        include_str!("../../../td-ui/tests/fonts/mod.rs"),
    ),
];

#[cfg(test)]
fn declared_modules() -> Vec<&'static str> {
    let mut names = Vec::new();
    for line in MAIN_RS.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("mod ") {
            if let Some(name) = rest.strip_suffix(';') {
                names.push(name);
            }
        }
    }
    names
}

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
    for directory in ["{src}/auth", "{src}/tests"] {
        steps.push(Step::MkDir {
            path: directory.into(),
        });
    }
    for (name, source) in STAGED_FILES {
        steps.push(Step::WriteFile {
            path: format!("{{src}}/{name}"),
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
            "{src}",
            rustc,
            &[
                "--edition",
                "2021",
                "--cfg",
                "feature=\"target-recipe\"",
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
                "{out}/bin/td-compositor",
                "{src}/main.rs",
            ],
        )
        .env("PATH", &path)
        .env("SOURCE_DATE_EPOCH", "1"),
        Step::Symlink {
            target: "td-compositor".into(),
            link: "{out}/bin/td-ui-demo".into(),
        },
        // And the control client under a third. It shares the artifact for a
        // reason of its own: the request vocabulary and the compositor that
        // answers it are one module, so two binaries built from one source
        // cannot drift apart on the wire.
        Step::Symlink {
            target: "td-compositor".into(),
            link: "{out}/bin/td-ctl".into(),
        },
        Step::Require {
            paths: vec![
                "{out}/bin/td-compositor".into(),
                "{out}/bin/td-ui-demo".into(),
                "{out}/bin/td-ctl".into(),
            ],
            exec: true,
        },
        target_rustc(
            "{src}",
            rustc,
            &[
                "--edition",
                "2021",
                "--test",
                "--crate-name",
                "td_compositor_session_tests",
                "--cfg",
                "feature=\"target-recipe\"",
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
                "{root}/session-tests",
                "{src}/main.rs",
            ],
        )
        .env("PATH", &path)
        .env("SOURCE_DATE_EPOCH", "1"),
        Step::run("{root}", &["{root}/session-tests", "authority::"]),
        Step::run("{root}", &["{root}/session-tests", "secret_client::"]),
        Step::run("{root}", &["{root}/session-tests", "physical_attention_"]),
        Step::run("{root}", &["{root}/session-tests", "timezone::"]),
        Step::run("{root}", &["{root}/session-tests", "clock::"]),
        Step::run("{root}", &["{root}/session-tests", "bar::"]),
        split_target_debug("{out}"),
        Step::assert_static(&[
            "{out}/bin/td-compositor",
            "{out}/bin/td-ui-demo",
            "{out}/bin/td-ctl",
        ]),
    ]);

    Recipe::mesboot("td-compositor", "0.1")
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
    use super::super::system_x86_64::{ROOTCHECK_ETC_NAME, SHADOW_ETC_NAME};
    use super::*;
    use crate::ladder::{
        TD_APPLICATION_CONFIG_PATH, TD_APPLICATION_LAUNCHER_TABLE, TD_APPLICATION_REGISTRY,
        TD_JAIL_FIXTURE_ALIAS, TD_JAIL_FIXTURE_DOWNLOAD_TARGET, TD_JAIL_FIXTURE_ENTRY,
        TD_JAIL_FIXTURE_GRANT_FILE, TD_JAIL_FIXTURE_GRANT_ROOT, TD_JAIL_FIXTURE_PICTURES_TARGET,
        TD_POINTER_ABSOLUTE_MARKER, TD_UI_CLIENT_RUNTIME_MARKER, TD_WAYLAND_RUNTIME_MARKER,
    };
    use crate::ladder::{TD_COMPOSITOR_FLIP_MARKER, TD_COMPOSITOR_KMS_MARKER};

    #[test]
    fn embedded_rust_does_not_contain_live_recipe_templates() {
        for (name, source) in std::iter::once(("main", MAIN_RS)).chain(MODULES.iter().copied()) {
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
                    "{name}.rs contains recipe template {template}"
                );
            }
        }
    }

    #[test]
    #[allow(clippy::unwrap_used, clippy::indexing_slicing)]
    fn authority_producer_stages_shared_sources_and_executes_target_tests() {
        let authority = MODULES
            .iter()
            .find_map(|(name, source)| (*name == "authority").then_some(*source))
            .unwrap();
        for path in ["auth/channel.rs", "auth/sys.rs", "auth/consent.rs"] {
            assert!(authority.contains(&format!("path = {path:?}")));
        }
        let recipe = recipe();
        for (path, expected) in [
            (
                "{src}/auth/consent.rs",
                include_str!("../../../td-authd/src/consent.rs"),
            ),
            (
                "{src}/auth/channel.rs",
                include_str!("../../../td-authd/src/channel.rs"),
            ),
            (
                "{src}/auth/sys.rs",
                include_str!("../../../td-authd/src/sys.rs"),
            ),
            (
                "{src}/tests/channel.rs",
                include_str!("../../../td-authd/tests/channel.rs"),
            ),
            (
                "{src}/tests/sys.rs",
                include_str!("../../../td-authd/tests/sys.rs"),
            ),
        ] {
            let writes: Vec<_> = recipe
                .steps
                .iter()
                .flatten()
                .filter_map(|step| match step {
                    Step::WriteFile {
                        path: actual,
                        content,
                        exec,
                    } if actual == path => {
                        assert!(!exec);
                        Some(content.as_str())
                    }
                    _ => None,
                })
                .collect();
            assert_eq!(writes, [expected]);
        }
        let runs: Vec<_> = recipe
            .steps
            .iter()
            .flatten()
            .filter_map(|step| match step {
                Step::Run { argv, .. } => Some(argv),
                _ => None,
            })
            .collect();
        let compile = runs
            .iter()
            .position(|argv| argv.iter().any(|arg| arg == "td_compositor_session_tests"))
            .unwrap();
        let execute = runs
            .iter()
            .position(|argv| argv.as_slice() == ["{root}/session-tests", "authority::"])
            .unwrap();
        assert!(compile < execute);
        for filter in [
            "secret_client::",
            "physical_attention_",
            "timezone::",
            "clock::",
            "bar::",
        ] {
            assert!(runs
                .iter()
                .any(|argv| argv.as_slice() == ["{root}/session-tests", filter]));
        }
        let args = runs[compile];
        assert!(args.iter().any(|arg| arg == "--test"));
        assert!(args.iter().any(|arg| arg == "{src}/main.rs"));
        assert!(args
            .windows(2)
            .any(|pair| pair == ["--cfg", "feature=\"target-recipe\""]));
        assert!(args
            .windows(2)
            .any(|pair| pair == ["--target", "x86_64-unknown-linux-gnu"]));
        assert!(args
            .windows(2)
            .any(|pair| pair == ["-o", "{root}/session-tests"]));
    }

    /// A `#[path]` the flat layout cannot follow fails only in the target
    /// build, since the host resolves it in the repository. So every path
    /// mount in a staged source is a `cfg_attr` pair: the host side, or a
    /// `target-recipe` side naming a file this recipe writes. main.rs alone
    /// may mount host-only, since a top-level module's default path is the
    /// flat file MODULES writes; a nested module's default is a directory
    /// the recipe never makes.
    #[test]
    fn every_staged_path_mount_names_a_staged_file() {
        let staged: Vec<String> = MODULES
            .iter()
            .map(|(name, _)| format!("{name}.rs"))
            .chain(STAGED_FILES.iter().map(|(name, _)| (*name).to_string()))
            .collect();
        let sources = std::iter::once(("main", MAIN_RS)).chain(MODULES.iter().copied());
        for (name, source) in sources {
            let squeezed: String = source.chars().filter(|c| !c.is_whitespace()).collect();
            assert!(
                !squeezed.contains("#[path="),
                "{name} has a bare path mount"
            );
            const HOST: &str = "#[cfg_attr(not(feature=\"target-recipe\")";
            const TARGET: &str = "#[cfg_attr(feature=\"target-recipe\"";
            // Where the last host-side mount ended: its target side must
            // start exactly there, on the same `mod`.
            let mut host_end = None;
            for (at, _) in squeezed.match_indices("#[cfg_attr(") {
                let attribute = &squeezed[at..];
                let attribute = &attribute[..attribute.find(")]").expect("closed cfg_attr")];
                let Some((_, path)) = attribute.split_once("path=\"") else {
                    continue;
                };
                let path = path.split('"').next().expect("quoted path");
                let end = at + attribute.len() + 2;
                if attribute.starts_with(HOST) {
                    assert!(
                        name == "main" || squeezed[end..].starts_with(TARGET),
                        "{name} has a host-only path mount"
                    );
                    host_end = Some(end);
                    continue;
                }
                assert!(attribute.starts_with(TARGET), "{name}: {attribute}");
                assert_eq!(
                    host_end,
                    Some(at),
                    "{name} mounts {path} without its host side"
                );
                assert!(
                    staged.iter().any(|file| file == path),
                    "{name} mounts {path}, which the recipe does not stage"
                );
            }
        }
    }

    #[test]
    fn recipe_writes_every_declared_module() {
        let mut declared = declared_modules();
        let mut written: Vec<&str> = MODULES.iter().map(|(name, _)| *name).collect();
        declared.sort_unstable();
        written.sort_unstable();
        assert_eq!(declared, written);
        assert!(MAIN_RS.contains("#![deny(unsafe_code)]"));
        let client = MODULES
            .iter()
            .find_map(|(name, source)| (*name == "client").then_some(*source))
            .expect("client source");
        let fixture_boundary = client
            .split_once("fn verify_jail_boundary")
            .expect("jail fixture boundary")
            .1
            .split_once("fn verify_jail_mounts")
            .expect("jail fixture mount boundary")
            .0;
        let fixture_mounts = client
            .split_once("fn verify_jail_mounts")
            .expect("jail fixture mounts")
            .1
            .split_once("fn verify_jail_loopback")
            .expect("jail fixture network boundary")
            .0;
        assert!(client.contains(&format!(
            "pub(crate) const JAIL_FIXTURE_ID: &str = \"{TD_JAIL_FIXTURE_ALIAS}\";"
        )));
        assert!(client.contains(&format!(
            "pub(crate) const JAIL_FIXTURE_ENTRY: &str = \"{TD_JAIL_FIXTURE_ENTRY}\";"
        )));
        assert!(client.contains(&format!(
            "pub(crate) const JAIL_FIXTURE_GRANT_FILE: &str = \"{TD_JAIL_FIXTURE_GRANT_FILE}\";"
        )));
        assert!(client.contains(&format!(
            "pub(crate) const JAIL_FIXTURE_DOWNLOAD_TARGET: &str = \"{TD_JAIL_FIXTURE_DOWNLOAD_TARGET}\";"
        )));
        assert!(client.contains(&format!(
            "pub(crate) const JAIL_FIXTURE_PICTURES_TARGET: &str = \"{TD_JAIL_FIXTURE_PICTURES_TARGET}\";"
        )));
        assert!(client.contains(&format!(
            "pub(crate) const JAIL_FIXTURE_GRANT_ROOT: &str = \"{TD_JAIL_FIXTURE_GRANT_ROOT}\";"
        )));
        assert!(client.contains(&format!(
            "pub(crate) const JAIL_FIXTURE_UID: u32 = {};",
            td_engine::application_spec::APPLICATION_UID
        )));
        for authority in [
            TD_APPLICATION_CONFIG_PATH,
            TD_APPLICATION_REGISTRY,
            TD_APPLICATION_LAUNCHER_TABLE,
        ] {
            assert!(
                fixture_boundary.contains(&format!("        \"{authority}\",")),
                "fixture source lacks authority sentinel {authority}"
            );
        }
        for image_name in [ROOTCHECK_ETC_NAME, SHADOW_ETC_NAME] {
            assert!(
                fixture_boundary.contains(&format!("        \"/etc/{image_name}\",")),
                "fixture source lacks image sentinel /etc/{image_name}"
            );
        }
        assert!(fixture_mounts.contains("(\"/etc\", \"configuration\")"));
        assert!(fixture_mounts.contains("for immutable_root in [\"/app\", \"/usr\", \"/etc\"]"));
        for row in [
            "const JAIL_FIXTURE_STATUS_EXIT_CODE: i32 = 70;",
            "const JAIL_FIXTURE_BOUNDARY_EXIT_CODE: i32 = 71;",
            "const JAIL_FIXTURE_MOUNTS_EXIT_CODE: i32 = 72;",
            "const JAIL_FIXTURE_LOOPBACK_EXIT_CODE: i32 = 73;",
            "const JAIL_FIXTURE_FILESYSTEM_EXIT_CODE: i32 = 74;",
        ] {
            assert!(client.contains(row), "fixture source lacks {row}");
        }
        assert!(client.contains("fs::File::open(\"/proc/1/fd/1\")"));
        assert!(MAIN_RS.contains("executable == client::JAIL_FIXTURE_ENTRY"));
        assert!(MAIN_RS.contains("Some(client::JAIL_FIXTURE_ID.as_ref())"));
        assert!(MAIN_RS.contains("process::exit(error.exit_code())"));
        let launcher = MODULES
            .iter()
            .find_map(|(name, source)| (*name == "launcher").then_some(*source))
            .expect("launcher source");
        assert!(launcher.contains(&format!(
            "const MAX_APPLICATION_NAME_BYTES: usize = {};",
            td_engine::application::MAX_APPLICATION_NAME_BYTES
        )));
        assert!(launcher.contains(&format!(
            "const RESERVED_APPLICATION_NAMES: &[&str] = &{:?};",
            td_engine::application::RESERVED_APPLICATION_NAMES
        )));
        assert!(launcher.contains("pub client: Option<PathBuf>"));
        assert!(
            launcher.contains("exactly one launcher client or launcher application is required")
        );
        for fragment in [
            "application.name.starts_with('-')",
            "application.name == \".\"",
            "application.name.contains(\"..\")",
            "byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-')",
        ] {
            assert!(launcher.contains(fragment));
        }
        assert!(launcher.contains("label: name.to_ascii_uppercase()"));
        assert!(launcher.contains("search: name.to_ascii_lowercase()"));
        assert!(launcher.contains("(LaunchRequest::UiDemo, Some(_))"));
        assert!(launcher.contains("configured launcher application is activation-only"));
        let input = MODULES
            .iter()
            .find_map(|(name, source)| (*name == "input").then_some(*source))
            .expect("input source");
        assert!(input
            .contains("request == LaunchRequest::UiDemo && self.launches.activates_application()"));
        assert!(input.contains(".activate_application()?"));
        let server = MODULES
            .iter()
            .find_map(|(name, source)| (*name == "server").then_some(*source))
            .expect("server source");
        // The pin is on the EMIT, not on a string beside it: a helper that
        // returns the right bytes proves nothing about what reaches stdout.
        assert!(server.contains(&format!(
            "writeln!(out, \"{TD_WAYLAND_RUNTIME_MARKER} socket={{}}\", path.display())"
        )));
        let client = MODULES
            .iter()
            .find_map(|(name, source)| (*name == "client").then_some(*source))
            .expect("client source");
        assert!(client.contains("if is_jail_fixture()"));
        assert!(client.contains("verify_jail_fixture(true, true)?"));
        assert!(client.contains("verify_jail_fixture(!shared_network, false)?"));
        assert!(client.contains(&format!(
            "writeln!(out, \"{TD_UI_CLIENT_RUNTIME_MARKER} surface={{}}x{{}}\", width, height)"
        )));
        // The absolute-pointer marker is the only evidence that a real device
        // answered `EVIOCGABS`, and the whole CALL is pinned as the three
        // above are — a literal alone would be satisfied by a `const` nothing
        // prints. Every argument is in it because their order is the part no
        // runtime check on this gate can see: `minimum` and `maximum` are two
        // fields of one type, so a crossed pair prints a reversed range that
        // latches exactly the same.
        let input = MODULES
            .iter()
            .find_map(|(name, source)| (*name == "input").then_some(*source))
            .expect("input source");
        let emit = [
            "        eprintln!(".to_string(),
            format!(
                "            \"{TD_POINTER_ABSOLUTE_MARKER} device={{}} \
                 x={{}}..{{}} y={{}}..{{}}\","
            ),
            "            path.display(),".to_string(),
            "            axes.x.minimum,".to_string(),
            "            axes.x.maximum,".to_string(),
            "            axes.y.minimum,".to_string(),
            "            axes.y.maximum".to_string(),
            "        );".to_string(),
        ]
        .join("\n");
        assert!(input.contains(&emit), "input.rs no longer emits the marker");
    }

    /// The ladder's markers and the strings the compositor actually prints are
    /// two copies of one fact in two crates that cannot import each other, so
    /// the only thing that can hold them together is a test that reads both.
    ///
    /// The boot check greps for each marker at the START of a line and then
    /// believes the fields after it. A compositor that printed a different
    /// string would fail the boot with "the marker was absent", which reads as
    /// a broken card rather than as a renamed constant.
    #[test]
    fn the_kms_boot_markers_are_the_ones_the_compositor_prints() {
        assert!(
            MAIN_RS.contains(&format!(
                "\"\\n{TD_COMPOSITOR_KMS_MARKER} {{lit}} output={{}}x{{}} stride={{}}\\n\""
            )),
            "td-compositor no longer prints {TD_COMPOSITOR_KMS_MARKER} in the shape the boot \
             check reads"
        );
        assert!(
            MAIN_RS.contains(&format!(
                "\"\\n{TD_COMPOSITOR_FLIP_MARKER} cookie={{:#x}} flip=ok\\n\""
            )),
            "td-compositor no longer prints {TD_COMPOSITOR_FLIP_MARKER} in the shape the boot \
             check reads"
        );
        // The FIELDS inside `{lit}` are a second copy of the same fact:
        // discovery's and the swap chain's describe them and `qemu_boot.rs`
        // reads them with `strip_prefix`. A rename passes every host test in
        // both crates and then fails every agent's `qemu-boot-system` with a
        // report the check reads as a broken card.
        let drm = MODULES
            .iter()
            .find(|(name, _)| *name == "drm")
            .map(|(_, source)| *source)
            .expect("td-compositor no longer declares a drm module");
        assert!(
            drm.contains(r#""driver={} connector={}#{} status={} crtc={} "#),
            "discovery no longer describes the fields the boot check reads"
        );
        assert!(
            drm.contains(r#""{} fb={},{} modeset=ok""#),
            "open_kms no longer describes the swap chain the way the boot check reads it"
        );
    }
}

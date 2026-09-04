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
    ("bar", include_str!("../../../td-compositor/src/bar.rs")),
    (
        "buffer",
        include_str!("../../../td-compositor/src/buffer.rs"),
    ),
    ("client", include_str!("../../../td-compositor/src/client.rs")),
    (
        "client_resources",
        include_str!("../../../td-compositor/src/client_resources.rs"),
    ),
    (
        "configure",
        include_str!("../../../td-compositor/src/configure.rs"),
    ),
    ("conn", include_str!("../../../td-compositor/src/conn.rs")),
    ("control", include_str!("../../../td-compositor/src/control.rs")),
    ("filter", include_str!("../../../td-compositor/src/filter.rs")),
    ("font", include_str!("../../../td-compositor/src/font.rs")),
    (
        "font_data",
        include_str!("../../../td-compositor/src/font_data.rs"),
    ),
    (
        "framebuffer",
        include_str!("../../../td-compositor/src/framebuffer.rs"),
    ),
    ("help", include_str!("../../../td-compositor/src/help.rs")),
    ("input", include_str!("../../../td-compositor/src/input.rs")),
    (
        "keyboard",
        include_str!("../../../td-compositor/src/keyboard.rs"),
    ),
    ("keys", include_str!("../../../td-compositor/src/keys.rs")),
    (
        "launcher",
        include_str!("../../../td-compositor/src/launcher.rs"),
    ),
    ("layout", include_str!("../../../td-compositor/src/layout.rs")),
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
    ("pty", include_str!("../../../td-compositor/src/pty.rs")),
    ("ready", include_str!("../../../td-compositor/src/ready.rs")),
    (
        "render",
        include_str!("../../../td-compositor/src/render.rs"),
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
    ("socket", include_str!("../../../td-compositor/src/socket.rs")),
    ("sys", include_str!("../../../td-compositor/src/sys.rs")),
    ("term", include_str!("../../../td-compositor/src/term.rs")),
    (
        "term_client",
        include_str!("../../../td-compositor/src/term_client.rs"),
    ),
    (
        "terminfo",
        include_str!("../../../td-compositor/src/terminfo.rs"),
    ),
    ("ui", include_str!("../../../td-compositor/src/ui.rs")),
    ("wire", include_str!("../../../td-compositor/src/wire.rs")),
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
        // The terminal is the same artifact under a third name, not a second
        // build of the same modules: argv[0] is what picks the program.
        Step::Symlink {
            target: "td-compositor".into(),
            link: "{out}/bin/td-term".into(),
        },
        // And the control client under a fourth. It shares the artifact for a
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
                "{out}/bin/td-term".into(),
                "{out}/bin/td-ctl".into(),
            ],
            exec: true,
        },
        // The just-built binary writes its own terminfo entry: one encoder,
        // and the bytes the image installs are the bytes its tests decode.
        // `tic` and a host terminfo database are not inputs. This runs after
        // the Require above, so a binary that failed to link is reported as
        // that rather than as a mysteriously failing build step.
        Step::MkDir {
            path: "{out}/share/terminfo/t".into(),
        },
        Step::run(
            "{out}",
            &[
                "{out}/bin/td-compositor",
                "terminfo",
                "{out}/share/terminfo/t/td-term",
            ],
        ),
        Step::Require {
            paths: vec!["{out}/share/terminfo/t/td-term".into()],
            exec: false,
        },
        split_target_debug("{out}"),
        Step::assert_static(&[
            "{out}/bin/td-compositor",
            "{out}/bin/td-ui-demo",
            "{out}/bin/td-term",
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
    use super::*;
    use super::super::system_x86_64::{ROOTCHECK_ETC_NAME, SHADOW_ETC_NAME};
    use crate::ladder::{
        TD_APPLICATION_CONFIG_PATH, TD_APPLICATION_LAUNCHER_TABLE, TD_APPLICATION_REGISTRY,
        TD_JAIL_FIXTURE_ALIAS, TD_JAIL_FIXTURE_DOWNLOAD_TARGET, TD_JAIL_FIXTURE_ENTRY,
        TD_JAIL_FIXTURE_GRANT_FILE, TD_JAIL_FIXTURE_GRANT_ROOT, TD_JAIL_FIXTURE_PICTURES_TARGET,
        TD_POINTER_ABSOLUTE_MARKER, TD_TERM_RUNTIME_MARKER, TD_UI_CLIENT_RUNTIME_MARKER,
        TD_WAYLAND_RUNTIME_MARKER,
    };

    #[test]
    fn embedded_rust_does_not_contain_live_recipe_templates() {
        for (name, source) in
            std::iter::once(("main", MAIN_RS)).chain(MODULES.iter().copied())
        {
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
        assert!(fixture_mounts
            .contains("for immutable_root in [\"/app\", \"/usr\", \"/etc\"]"));
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
        assert!(launcher.contains(
            "exactly one launcher client or launcher application is required"
        ));
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
        assert!(input.contains(
            "request == LaunchRequest::UiDemo && self.launches.activates_application()"
        ));
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
        // The terminal's marker is the boot oracle's first-client proof since
        // the cutover, so the literal is pinned here as the other two are.
        let ready = MODULES
            .iter()
            .find_map(|(name, source)| (*name == "ready").then_some(*source))
            .expect("ready source");
        assert!(ready.contains(&format!(
            "pub const MARKER: &str = \"{TD_TERM_RUNTIME_MARKER}\";"
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

    /// One artifact, three names. The symlink is what makes the terminal
    /// reachable at all: `main` picks its program from argv[0], so a missing
    /// link is not a build error anywhere — it is an image with no terminal
    /// and nothing to say so.
    #[test]
    fn the_terminal_ships_as_a_name_on_the_compositor() {
        let steps = recipe().steps.expect("steps");
        let linked = steps.iter().any(|step| {
            matches!(step, Step::Symlink { target, link }
                if target == "td-compositor" && link == "{out}/bin/td-term")
        });
        assert!(linked, "nothing installs the td-term name");
        // A name nothing requires is one a failed link leaves missing with the
        // build still green, and one nothing asserts static is a name the
        // image could ship dynamically linked.
        let required = steps.iter().any(|step| {
            matches!(step, Step::Require { paths, exec }
                if *exec && paths.iter().any(|path| path == "{out}/bin/td-term"))
        });
        assert!(required, "nothing requires td-term to exist and execute");
        let asserted = steps.iter().any(|step| {
            matches!(step, Step::AssertStatic { paths }
                if paths.iter().any(|path| path == "{out}/bin/td-term"))
        });
        assert!(asserted, "nothing asserts td-term is static");
        // And the binary answers to it. Both halves are needed: a link to a
        // binary that does not know the name would dispatch to the compositor.
        assert!(MAIN_RS.contains(r#"Some("td-term") => Personality::Term,"#));
    }

    /// The recipe writes the terminfo entry at a path the compositor's own
    /// module also spells, and the two never compile against each other, so a
    /// divergence would be caught by nothing without this. The binary refuses
    /// a path not ending in its constant, which makes a wrong STORE path a
    /// build failure.
    ///
    /// Reaching it is the image's half: the child is given
    /// `TERMINFO=/etc/terminfo`, and `IMMUTABLE_ETC` in the system recipe
    /// points that name at this directory — the category that landing added
    /// for exactly this, since every other `/etc` symlink there dangles by
    /// design and this one must not.
    #[test]
    fn the_terminfo_entry_is_installed_where_the_encoder_expects() {
        let terminfo = MODULES
            .iter()
            .filter(|(name, _)| *name == "terminfo")
            .map(|(_, source)| *source)
            .next()
            .expect("terminfo source");
        assert!(terminfo
            .contains(r#"pub(crate) const INSTALL_PATH: &str = "share/terminfo/t/td-term";"#));
        let steps = recipe().steps.expect("steps");
        let writes = steps.iter().any(|step| {
            matches!(step, Step::Run { argv, .. }
                if argv.iter().any(|word| word == "terminfo")
                    && argv.iter().any(|word| word == "{out}/share/terminfo/t/td-term"))
        });
        assert!(writes, "no step installs the terminfo entry");
    }

    /// td-term's child command is an absolute path into a DIFFERENT staged
    /// package plus a flag that package must parse. Neither crate compiles
    /// against the other, so nothing but this would notice `--stdin` being
    /// renamed on one side: the terminal would build, ship, and fail at the
    /// first spawn. Both recipes' sources are embedded here, so pin them
    /// against each other where both are already in hand.
    #[test]
    fn the_terminals_session_wrapper_matches_the_staged_td_init() {
        const CTTYHACK: &str = include_str!("../../../td-init/src/cttyhack.rs");
        let pty = MODULES
            .iter()
            .find_map(|(name, source)| (*name == "pty").then_some(*source))
            .expect("pty source");
        assert!(pty.contains(r#"pub const CTTYHACK: &str = "/bin/cttyhack";"#));
        assert!(pty.contains(r#"pub const CTTYHACK_STDIN: &str = "--stdin";"#));
        assert!(CTTYHACK.contains(r#"const STDIN_FLAG: &str = "--stdin";"#));
        // And that the applet still advertises it, so `cttyhack` alone tells an
        // operator the mode exists.
        assert!(CTTYHACK.contains("usage: cttyhack [--stdin] PROG [ARG...]"));
        // `/bin/cttyhack` is td-init's own symlink name in the image roster.
        const INIT_MAIN: &str = include_str!("../../../td-init/src/main.rs");
        assert!(INIT_MAIN.contains(r#"("cttyhack", cttyhack::run)"#));
    }
}

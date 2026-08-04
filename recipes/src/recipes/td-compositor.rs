use crate::types::{Recipe, Step};

const MAIN_RS: &str = include_str!("../../../td-compositor/src/main.rs");
const MODULES: &[(&str, &str)] = &[
    ("client", include_str!("../../../td-compositor/src/client.rs")),
    (
        "configure",
        include_str!("../../../td-compositor/src/configure.rs"),
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
        "pointer",
        include_str!("../../../td-compositor/src/pointer.rs"),
    ),
    ("pty", include_str!("../../../td-compositor/src/pty.rs")),
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
        Step::Require {
            paths: vec![
                "{out}/bin/td-compositor".into(),
                "{out}/bin/td-ui-demo".into(),
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
        Step::assert_static(&["{out}/bin/td-compositor", "{out}/bin/td-ui-demo"]),
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
    use crate::ladder::{TD_UI_CLIENT_RUNTIME_MARKER, TD_WAYLAND_RUNTIME_MARKER};

    #[test]
    fn recipe_writes_every_declared_module() {
        let mut declared = declared_modules();
        let mut written: Vec<&str> = MODULES.iter().map(|(name, _)| *name).collect();
        declared.sort_unstable();
        written.sort_unstable();
        assert_eq!(declared, written);
        assert!(MAIN_RS.contains("#![deny(unsafe_code)]"));
        let server = MODULES
            .iter()
            .find_map(|(name, source)| (*name == "server").then_some(*source))
            .expect("server source");
        assert!(server.contains(&format!(
            "println!(\"{TD_WAYLAND_RUNTIME_MARKER} socket={{}}\""
        )));
        let client = MODULES
            .iter()
            .find_map(|(name, source)| (*name == "client").then_some(*source))
            .expect("client source");
        assert!(client.contains(&format!(
            "println!(\"{TD_UI_CLIENT_RUNTIME_MARKER} surface={{}}x{{}}\""
        )));
    }

    /// The recipe writes the terminfo entry at a path the compositor's own
    /// module also spells, and the two never compile against each other, so a
    /// divergence would be caught by nothing without this. The binary refuses
    /// a path not ending in its constant, which makes a wrong STORE path a
    /// build failure.
    ///
    /// It does NOT make the entry reachable: the child is given
    /// `TERMINFO=/etc/terminfo`, and nothing in the image exposes this store
    /// directory there yet, so ncurses still cannot look `td-term` up at
    /// runtime. That exposure needs an immutable-symlink category in the
    /// image's read-only-/etc invariant and is a separate landing.
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

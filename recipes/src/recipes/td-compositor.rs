use crate::types::{Recipe, Step};

const MAIN_RS: &str = include_str!("../../../td-compositor/src/main.rs");
const MODULES: &[(&str, &str)] = &[
    ("client", include_str!("../../../td-compositor/src/client.rs")),
    (
        "framebuffer",
        include_str!("../../../td-compositor/src/framebuffer.rs"),
    ),
    ("input", include_str!("../../../td-compositor/src/input.rs")),
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
}

use crate::ladder::{split_target_debug, target_rustc};
use crate::types::{Recipe, Step};
const MAIN_RS: &str = include_str!("../../../td-secret/src/main.rs");
const MODULES: &[(&str, &str)] = &[
    ("client", include_str!("../../../td-secret/src/client.rs")),
    ("crypto", include_str!("../../../td-secret/src/crypto.rs")),
    ("fido_cbor", include_str!("../../../td-secret/src/fido_cbor.rs")),
    ("fido_ctap", include_str!("../../../td-secret/src/fido_ctap.rs")),
    ("fido_enroll", include_str!("../../../td-secret/src/fido_enroll.rs")),
    ("fido_hid", include_str!("../../../td-secret/src/fido_hid.rs")),
    ("fido_metadata", include_str!("../../../td-secret/src/fido_metadata.rs")),
    ("tpm", include_str!("../../../td-secret/src/tpm.rs")),
    ("store", include_str!("../../../td-secret/src/store.rs")),
    ("sys", include_str!("../../../td-secret/src/sys.rs")),
    ("name", include_str!("../../../td-busd/src/name.rs")),
    ("message", include_str!("../../../td-busd/src/message.rs")),
    ("wire", include_str!("../../../td-busd/src/wire.rs")),
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

    let mut steps = Vec::new();
    for directory in [
        "{src}/td-secret/src",
        "{src}/td-firstboot/src",
        "{src}/td-busd/src",
        "{src}/engine/src",
    ] {
        steps.push(Step::MkDir {
            path: directory.into(),
        });
    }
    steps.push(Step::MkDir {
        path: "{out}/bin".into(),
    });
    steps.push(Step::WriteFile {
        path: "{src}/td-secret/src/main.rs".into(),
        content: MAIN_RS.into(),
        exec: false,
    });
    for (name, source) in MODULES {
        steps.push(Step::WriteFile {
            path: match *name {
                "crypto" => "{src}/td-secret/src/crypto.rs".into(),
                "store" => "{src}/td-secret/src/store.rs".into(),
                "name" => "{src}/td-busd/src/name.rs".into(),
                "message" => "{src}/td-busd/src/message.rs".into(),
                "wire" => "{src}/td-busd/src/wire.rs".into(),
                _ => format!("{{src}}/td-secret/src/{name}.rs"),
            },
            content: (*source).into(),
            exec: false,
        });
    }
    for (path, source) in [
        ("{src}/td-firstboot/src/principals.rs", include_str!("../../../td-firstboot/src/principals.rs")),
        ("{src}/td-firstboot/src/principals_tests.rs", include_str!("../../../td-firstboot/src/principals_tests.rs")),
        ("{src}/engine/src/principals.rs", include_str!("../../../engine/src/principals.rs")),
    ] {
        steps.push(Step::WriteFile { path: path.into(), content: source.into(), exec: false });
    }
    steps.push(Step::WriteFile {
        path: "{src}/engine/src/sha256.rs".into(),
        content: include_str!("../../../engine/src/sha256.rs").into(),
        exec: false,
    });
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
                "{out}/bin/td-secret",
                "{src}/td-secret/src/main.rs",
            ],
        )
        .env("PATH", &path)
        .env("SOURCE_DATE_EPOCH", "1"),
    );
    steps.push(Step::Require {
        paths: vec!["{out}/bin/td-secret".into()],
        exec: true,
    });
    steps.push(Step::run("{root}", &["{out}/bin/td-secret", "selftest"]));
    steps.push(
        target_rustc(
            "{src}",
            rustc,
            &[
                "--edition", "2021", "--test", "--crate-name", "td_secret_tests",
                "--target", "x86_64-unknown-linux-gnu",
                "-C", "target-feature=+crt-static", "-C", "relocation-model=static",
                &linker, "-L", glib, &lib_b, &bin_b,
                "-Clink-arg=-L{root}/eh", "-Clink-arg=-static-libgcc",
                "-o", "{root}/secret-tests", "{src}/td-secret/src/main.rs",
            ],
        )
        .env("PATH", &path)
        .env("CARGO_MANIFEST_DIR", "{src}/td-secret")
        .env("SOURCE_DATE_EPOCH", "1"),
    );
    steps.push(Step::run("{root}", &["{root}/secret-tests"]));
    steps.push(split_target_debug("{out}"));
    steps.push(Step::assert_static(&["{out}/bin/td-secret"]));

    Recipe::mesboot("td-secret", "0.1")
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
    fn recipe_embeds_every_declared_module() {
        let production = MAIN_RS.split("#[cfg(test)]").next().unwrap();
        let code = production.lines().map(|line| line.split("//").next().unwrap()).collect::<Vec<_>>().join("\n");
        let mut words = code.split_whitespace();
        let mut declared = Vec::new();
        while let Some(word) = words.next() {
            if word == "mod" {
                if let Some(name) = words.next().and_then(|name| name.strip_suffix(';')) { declared.push(name); }
            }
        }
        let mut embedded: Vec<_> = MODULES.iter().map(|(name, _)| *name).collect();
        embedded.push("principals");
        declared.sort_unstable(); embedded.sort_unstable();
        assert_eq!(declared, embedded);
        assert!(recipe().steps.iter().flatten().any(|step| matches!(step,
            Step::WriteFile { path, content, .. } if path == "{src}/td-firstboot/src/principals.rs"
                && content == include_str!("../../../td-firstboot/src/principals.rs"))));
    }
}

use crate::ladder::{split_target_debug, target_rustc};
use crate::types::{Recipe, Step};
const LIB_RS: &str = include_str!("../../../td-secret/src/lib.rs");
const MAIN_RS: &str = include_str!("../../../td-secret/src/main.rs");
/// The TPM 2.0 client crate td-secret depends on (td-tpm/DESIGN.md).
const TPM_RS: &str = include_str!("../../../td-tpm/src/lib.rs");
const MODULES: &[(&str, &str)] = &[
    (
        "set_client",
        include_str!("../../../td-secret/src/set_client.rs"),
    ),
    (
        "secret_sys",
        include_str!("../../../td-authd/src/secret_sys.rs"),
    ),
    (
        "secret_request",
        include_str!("../../../td-authd/src/secret_request.rs"),
    ),
    (
        "write_operation",
        include_str!("../../../td-secret/src/write_operation.rs"),
    ),
    (
        "operation",
        include_str!("../../../td-secret/src/operation.rs"),
    ),
    (
        "enrollment_operation",
        include_str!("../../../td-secret/src/enrollment_operation.rs"),
    ),
    ("consent", include_str!("../../../td-authd/src/consent.rs")),
    ("client", include_str!("../../../td-secret/src/client.rs")),
    ("crypto", include_str!("../../../td-secret/src/crypto.rs")),
    (
        "portable",
        include_str!("../../../td-secret/src/portable.rs"),
    ),
    (
        "fido_aes",
        include_str!("../../../td-secret/src/fido_aes.rs"),
    ),
    (
        "fido_cbor",
        include_str!("../../../td-secret/src/fido_cbor.rs"),
    ),
    (
        "fido_ctap",
        include_str!("../../../td-secret/src/fido_ctap.rs"),
    ),
    (
        "fido_device",
        include_str!("../../../td-secret/src/fido_device.rs"),
    ),
    (
        "fido_enroll",
        include_str!("../../../td-secret/src/fido_enroll.rs"),
    ),
    (
        "fido_hid",
        include_str!("../../../td-secret/src/fido_hid.rs"),
    ),
    (
        "fido_metadata",
        include_str!("../../../td-secret/src/fido_metadata.rs"),
    ),
    (
        "fido_p256",
        include_str!("../../../td-secret/src/fido_p256.rs"),
    ),
    (
        "fido_pin",
        include_str!("../../../td-secret/src/fido_pin.rs"),
    ),
    (
        "fido_transaction",
        include_str!("../../../td-secret/src/fido_transaction.rs"),
    ),
    (
        "login_operation",
        include_str!("../../../td-secret/src/login_operation.rs"),
    ),
    (
        "login_record",
        include_str!("../../../td-secret/src/login_record.rs"),
    ),
    (
        "login_state",
        include_str!("../../../td-secret/src/login_state.rs"),
    ),
    (
        "login_store",
        include_str!("../../../td-secret/src/login_store.rs"),
    ),
    (
        "login_tier",
        include_str!("../../../td-secret/src/login_tier.rs"),
    ),
    ("pin_sys", include_str!("../../../td-secret/src/pin_sys.rs")),
    (
        "pin_terminal",
        include_str!("../../../td-secret/src/pin_terminal.rs"),
    ),
    (
        "token_check",
        include_str!("../../../td-secret/src/token_check.rs"),
    ),
    ("tpm", include_str!("../../../td-secret/src/tpm.rs")),
    ("store", include_str!("../../../td-secret/src/store.rs")),
    ("sys", include_str!("../../../td-secret/src/sys.rs")),
    (
        "bus_client",
        include_str!("../../../td-busd/src/bus_client.rs"),
    ),
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
        "{src}/td-secret/tests",
        "{src}/td-firstboot/src",
        "{src}/td-busd/src",
        "{src}/td-authd/src",
        "{src}/td-authd/tests",
        "{src}/engine/src",
        "{src}/td-tpm/src",
        "{root}/test-deps",
    ] {
        steps.push(Step::MkDir {
            path: directory.into(),
        });
    }
    steps.push(Step::MkDir {
        path: "{out}/bin".into(),
    });
    for (path, content) in [
        ("{src}/td-secret/src/lib.rs", LIB_RS),
        ("{src}/td-secret/src/main.rs", MAIN_RS),
        ("{src}/td-tpm/src/lib.rs", TPM_RS),
    ] {
        steps.push(Step::WriteFile {
            path: path.into(),
            content: content.into(),
            exec: false,
        });
    }
    for (name, source) in MODULES {
        steps.push(Step::WriteFile {
            path: match *name {
                "secret_sys" => "{src}/td-authd/src/secret_sys.rs".into(),
                "secret_request" => "{src}/td-authd/src/secret_request.rs".into(),
                "consent" => "{src}/td-authd/src/consent.rs".into(),
                "crypto" => "{src}/td-secret/src/crypto.rs".into(),
                "store" => "{src}/td-secret/src/store.rs".into(),
                "bus_client" => "{src}/td-busd/src/bus_client.rs".into(),
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
        // sys.rs's own child, which lib.rs does not declare.
        (
            "{src}/td-secret/src/scm.rs",
            include_str!("../../../td-secret/src/scm.rs"),
        ),
        (
            "{src}/td-secret/src/portable_events.rs",
            include_str!("../../../td-secret/src/portable_events.rs"),
        ),
        (
            "{src}/td-secret/src/portable_host.rs",
            include_str!("../../../td-secret/src/portable_host.rs"),
        ),
        (
            "{src}/td-secret/src/portable_lifecycle.rs",
            include_str!("../../../td-secret/src/portable_lifecycle.rs"),
        ),
        (
            "{src}/td-secret/src/portable_notebook.rs",
            include_str!("../../../td-secret/src/portable_notebook.rs"),
        ),
        (
            "{src}/td-secret/src/portable_pass.rs",
            include_str!("../../../td-secret/src/portable_pass.rs"),
        ),
        (
            "{src}/td-secret/src/portable_store.rs",
            include_str!("../../../td-secret/src/portable_store.rs"),
        ),
        (
            "{src}/td-authd/src/terminal_sys.rs",
            include_str!("../../../td-authd/src/terminal_sys.rs"),
        ),
        (
            "{src}/td-authd/src/primary_account.rs",
            include_str!("../../../td-authd/src/primary_account.rs"),
        ),
        (
            "{src}/td-secret/tests/login_record_vectors.txt",
            include_str!("../../../td-secret/tests/login_record_vectors.txt"),
        ),
        (
            "{src}/td-secret/tests/login_ctap_vectors.txt",
            include_str!("../../../td-secret/tests/login_ctap_vectors.txt"),
        ),
        (
            "{src}/td-secret/tests/aes_vectors.txt",
            include_str!("../../../td-secret/tests/aes_vectors.txt"),
        ),
        (
            "{src}/td-secret/tests/p256_vectors.txt",
            include_str!("../../../td-secret/tests/p256_vectors.txt"),
        ),
        (
            "{src}/td-secret/tests/pin_vectors.txt",
            include_str!("../../../td-secret/tests/pin_vectors.txt"),
        ),
        (
            "{src}/td-secret/tests/token_check_vectors.txt",
            include_str!("../../../td-secret/tests/token_check_vectors.txt"),
        ),
        (
            "{src}/td-secret/src/system_vm.rs",
            include_str!("../../../td-secret/src/system_vm.rs"),
        ),
        (
            "{src}/td-secret/src/fido_virtual.rs",
            include_str!("../../../td-secret/src/fido_virtual.rs"),
        ),
        (
            "{src}/td-secret/src/fido_uhid.rs",
            include_str!("../../../td-secret/src/fido_uhid.rs"),
        ),
        (
            "{src}/td-secret/src/login_vm.rs",
            include_str!("../../../td-secret/src/login_vm.rs"),
        ),
        (
            "{src}/td-secret/src/login_system_vm.rs",
            include_str!("../../../td-secret/src/login_system_vm.rs"),
        ),
        (
            "{src}/td-authd/tests/secret_sys.rs",
            include_str!("../../../td-authd/tests/secret_sys.rs"),
        ),
        (
            "{src}/td-firstboot/src/principals.rs",
            include_str!("../../../td-firstboot/src/principals.rs"),
        ),
        (
            "{src}/td-firstboot/src/principals_tests.rs",
            include_str!("../../../td-firstboot/src/principals_tests.rs"),
        ),
        // The shared consent's hostname rules.
        (
            "{src}/td-firstboot/src/hostname.rs",
            include_str!("../../../td-firstboot/src/hostname.rs"),
        ),
        (
            "{src}/engine/src/principals.rs",
            include_str!("../../../engine/src/principals.rs"),
        ),
    ] {
        steps.push(Step::WriteFile {
            path: path.into(),
            content: source.into(),
            exec: false,
        });
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
    // td-tpm, the TPM client td-secret's sealed-store formats run over, with
    // the shipped profile; the unwinding test harness links its own copy.
    for (dir, profile) in [
        ("{root}", &["-C", "opt-level=s", "-C", "panic=abort"][..]),
        ("{root}/test-deps", &[][..]),
    ] {
        let output = format!("{dir}/libtd_tpm.rlib");
        let mut args = vec![
            "--edition",
            "2021",
            "--crate-type",
            "rlib",
            "--crate-name",
            "td_tpm",
            "--target",
            "x86_64-unknown-linux-gnu",
            "-C",
            "target-feature=+crt-static",
            "-C",
            "relocation-model=static",
        ];
        args.extend_from_slice(profile);
        args.extend_from_slice(&["-o", &output, "{src}/td-tpm/src/lib.rs"]);
        steps.push(
            target_rustc("{src}", rustc, &args)
                .env("PATH", &path)
                .env("SOURCE_DATE_EPOCH", "1"),
        );
    }
    // The library holds every module; the binary only calls its `run`.
    steps.push(
        target_rustc(
            "{src}",
            rustc,
            &[
                "--edition",
                "2021",
                "--crate-type",
                "rlib",
                "--crate-name",
                "td_secret",
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
                "--extern",
                "td_tpm={root}/libtd_tpm.rlib",
                "-o",
                "{root}/libtd_secret.rlib",
                "{src}/td-secret/src/lib.rs",
            ],
        )
        .env("PATH", &path)
        .env("SOURCE_DATE_EPOCH", "1"),
    );
    steps.push(
        target_rustc(
            "{src}",
            rustc,
            &[
                "--edition",
                "2021",
                "--crate-name",
                "td_secret",
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
                "--extern",
                "td_secret={root}/libtd_secret.rlib",
                "-L",
                "dependency={root}",
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
                "--edition",
                "2021",
                "--test",
                "--crate-name",
                "td_secret_tests",
                "--extern",
                "td_tpm={root}/test-deps/libtd_tpm.rlib",
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
                "{root}/secret-tests",
                "{src}/td-secret/src/lib.rs",
            ],
        )
        .env("PATH", &path)
        .env("CARGO_MANIFEST_DIR", "{src}/td-secret")
        .env("SOURCE_DATE_EPOCH", "1"),
    );
    steps.push(Step::run("{root}", &["{root}/secret-tests"]));
    steps.push(
        target_rustc(
            "{src}",
            rustc,
            &[
                "--edition",
                "2021",
                "--test",
                "--crate-name",
                "td_tpm_tests",
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
                "{root}/tpm-tests",
                "{src}/td-tpm/src/lib.rs",
            ],
        )
        .env("PATH", &path)
        .env("SOURCE_DATE_EPOCH", "1"),
    );
    steps.push(Step::run("{root}", &["{root}/tpm-tests"]));
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
    fn embedded_rust_has_no_recipe_substitution_tokens() {
        for step in recipe().steps.iter().flatten() {
            let Step::WriteFile { path, content, .. } = step else {
                continue;
            };
            if !path.ends_with(".rs") {
                continue;
            }
            for token in [
                "{root}",
                "{src}",
                "{out}",
                "{tools}",
                "{jobs}",
                "{in:",
                "{payload:",
            ] {
                assert!(
                    !content.contains(token),
                    "embedded source {path} contains recipe substitution token {token}"
                );
            }
        }
    }

    #[test]
    fn recipe_writes_every_sibling_path_module() {
        let written: Vec<String> = recipe()
            .steps
            .iter()
            .flatten()
            .filter_map(|step| match step {
                Step::WriteFile { path, .. } => Some(path.clone()),
                _ => None,
            })
            .collect();
        for (name, source) in MODULES {
            for marker in ["#[path = \"", "include!(\""] {
                for attribute in source.split(marker).skip(1) {
                    let file = attribute.split('"').next().unwrap_or_default();
                    if file.contains('/') {
                        continue;
                    }
                    let path = format!("{{src}}/td-secret/src/{file}");
                    assert!(written.contains(&path), "{name} declares {file}");
                }
            }
        }
        // lib.rs's own modules, test-only ones included, unless a path names them.
        let lines: Vec<&str> = LIB_RS.lines().collect();
        for (at, line) in lines.iter().enumerate() {
            let declaration = ["pub(crate) ", "pub "]
                .iter()
                .find_map(|visibility| line.strip_prefix(visibility))
                .unwrap_or(line);
            let Some(name) = declaration
                .strip_prefix("mod ")
                .and_then(|n| n.strip_suffix(';'))
            else {
                continue;
            };
            let pathed = lines[..at]
                .iter()
                .rev()
                .take_while(|line| line.starts_with("#["))
                .any(|line| line.starts_with("#[path"));
            let path = format!("{{src}}/td-secret/src/{name}.rs");
            assert!(pathed || written.contains(&path), "lib.rs declares {name}");
        }
    }

    #[test]
    fn recipe_embeds_every_declared_module() {
        let production = LIB_RS.split("#[cfg(test)]").next().unwrap();
        let code = production
            .lines()
            .map(|line| line.split("//").next().unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        let mut words = code.split_whitespace();
        let mut declared = Vec::new();
        while let Some(word) = words.next() {
            if word == "mod" {
                if let Some(name) = words.next().and_then(|name| name.strip_suffix(';')) {
                    declared.push(name);
                }
            }
        }
        let mut embedded: Vec<_> = MODULES.iter().map(|(name, _)| *name).collect();
        embedded.extend(["principals", "hostname"]);
        declared.sort_unstable();
        embedded.sort_unstable();
        assert_eq!(declared, embedded);
        for (path, source) in [
            (
                "{src}/td-firstboot/src/principals.rs",
                include_str!("../../../td-firstboot/src/principals.rs"),
            ),
            (
                "{src}/td-firstboot/src/hostname.rs",
                include_str!("../../../td-firstboot/src/hostname.rs"),
            ),
        ] {
            assert!(
                recipe().steps.iter().flatten().any(|step| matches!(step,
                    Step::WriteFile { path: written, content, .. }
                        if written == path && content == source)),
                "{path}"
            );
        }
    }
}

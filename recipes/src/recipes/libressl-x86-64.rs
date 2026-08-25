use crate::ladder::{post_bootstrap_path, unpack_into, POST_BOOTSTRAP_SH};
use crate::types::{Recipe, Step, TextEdit};

// LibreSSL 4.3.2 provides the OpenSSL-compatible TLS surface curl and Git will
// consume. This rung deliberately exports static libssl.a + libcrypto.a and the
// public headers, not executables or shared objects: downstream packages absorb
// the code into their own runtime ELF and apply td's debug-companion policy at
// that final link boundary.
//
// Upstream's hand-written assembly is disabled. The portable C implementation
// is compiled with td's target-wide frame-pointer, bounded line-table, and
// remapped-path flags; downstream runtime links supply their deterministic build
// IDs. That avoids introducing an assembly coverage exception merely for crypto
// acceleration. Upstream tests are disabled here; the sibling
// libressl-x86-64-test compiles, links, and runs a verified client/server TLS
// handshake against exactly the installed archives.
pub fn recipe() -> Recipe {
    let sgcc = "{in:gcc-x86-64-self}/stage/td/store/gcc-14.3.0-x86_64-self/bin/gcc";
    let sbin = "{in:binutils-x86-64-self}/bin";
    let xglibc = "{in:glibc-x86-64}/stage/td/store/glibc-2.41-x86_64";
    let path = format!("{{root}}/wb:{{tools}}:{sbin}:{}", post_bootstrap_path());

    let mut steps = unpack_into("libressl-x86-64-source", "{src}");
    // GNU libtool expands the convenience libcompat.a into libcrypto.a by
    // walking its extraction directory with `find`. Autoconf and libtool also
    // use utilities that the installed post-bootstrap BusyBox farm deliberately
    // omits. Expose the reviewed multicall applets explicitly; without `find`,
    // libtool still exits successfully but silently emits an incomplete archive.
    steps.push(Step::ToolFarm {
        links: [
            "awk", "basename", "cat", "chmod", "cmp", "cp", "cut", "date", "diff", "dirname",
            "echo", "env", "expr", "false", "find", "grep", "head", "install", "ln", "ls", "mkdir",
            "mktemp", "mv", "printf", "pwd", "rm", "rmdir", "sed", "sort", "tail", "tee", "test",
            "touch", "tr", "true", "uname", "wc", "which", "xargs",
        ]
        .iter()
        .map(|name| ((*name).into(), "{in:busybox-x86-64}/bin/busybox".into()))
        .collect(),
    });
    steps.push(Step::PatchShebangs {
        dir: "{src}".into(),
        shell: POST_BOOTSTRAP_SH.into(),
    });

    // Libtool's generated x86-64 ABI probe names /usr/bin/file directly. The
    // target sandbox intentionally has no host /usr, so route the two possible
    // conftest.o probes through a tiny declared-binutils adapter instead.
    steps.push(Step::substitute_text(
        "{src}/configure",
        vec![TextEdit::new(
            "case `/usr/bin/file conftest.o` in",
            "case `file conftest.o` in",
            2,
        )],
    ));
    steps.push(Step::WriteFile {
        path: "{root}/wb/file".into(),
        content: format!(
            "#!{POST_BOOTSTRAP_SH}\n\
             h=$('{sbin}/readelf' -h \"$1\") || exit 1\n\
             case \"$h\" in\n\
             *'Class:'*'ELF64'*) printf '%s\\n' 'ELF 64-bit LSB relocatable';;\n\
             *'Class:'*'ELF32'*) printf '%s\\n' 'ELF 32-bit LSB relocatable';;\n\
             *) exit 1;;\n\
             esac\n"
        ),
        exec: true,
    });
    steps.push(Step::WriteFile {
        path: "{root}/wb/cc".into(),
        content: format!(
            "#!{POST_BOOTSTRAP_SH}\n\
             exec \"{sgcc}\" -static -isystem \"{xglibc}/include\" \
             -B\"{sbin}/\" -B\"{xglibc}/lib\" \
             -L\"{xglibc}/lib\" -static-libgcc \"$@\" \
             -fno-omit-frame-pointer -g1 \
             -ffile-prefix-map={{root}}=/td-build-root \
             -ffile-prefix-map={{src}}=/td-build -Wl,--build-id=sha1\n"
        ),
        exec: true,
    });

    steps.push(
        Step::run(
            "{src}",
            &[
                POST_BOOTSTRAP_SH,
                "./configure",
                "--build=x86_64-pc-linux-gnu",
                "--host=x86_64-pc-linux-gnu",
                "--prefix={out}",
                "--with-openssldir=/etc/ssl",
                "--disable-shared",
                "--enable-static",
                "--disable-tests",
                "--disable-asm",
                "--disable-dependency-tracking",
            ],
        )
        .env("PATH", &path)
        .env("CONFIG_SHELL", POST_BOOTSTRAP_SH)
        .env("SHELL", POST_BOOTSTRAP_SH)
        .env("CC", "{root}/wb/cc")
        .env("CCAS", "{root}/wb/cc")
        .env("AR", "{in:binutils-x86-64-self}/bin/ar")
        .env("RANLIB", "{in:binutils-x86-64-self}/bin/ranlib")
        .env("NM", "{in:binutils-x86-64-self}/bin/nm")
        .env("CFLAGS", "-O2")
        .env("SOURCE_DATE_EPOCH", "1"),
    );

    // `remove_bs_objects` is LibreSSL's explicit post-link target: it depends on
    // libssl.la, then removes the byte-string objects that libcrypto already
    // supplies. Building libssl.la alone would skip that upstream archive fixup.
    for (dir, target) in [("crypto", "libcrypto.la"), ("ssl", "remove_bs_objects")] {
        steps.push(
            Step::run(
                "{src}",
                &[
                    "{in:make-x86-64-self}/bin/make",
                    "-j{jobs}",
                    "-C",
                    dir,
                    target,
                    &format!("SHELL={POST_BOOTSTRAP_SH}"),
                ],
            )
            .env("PATH", &path)
            .env("CONFIG_SHELL", POST_BOOTSTRAP_SH)
            .env("SHELL", POST_BOOTSTRAP_SH)
            .env("SOURCE_DATE_EPOCH", "1"),
        );
    }

    steps.push(Step::MkDir {
        path: "{out}/lib".into(),
    });
    steps.push(Step::CopyFiles {
        files: vec![
            "{src}/crypto/.libs/libcrypto.a".into(),
            "{src}/ssl/.libs/libssl.a".into(),
        ],
        dest: "{out}/lib".into(),
    });
    // Use upstream's generated install rule so only public headers leave the
    // source tree; Makefile/CMake metadata and internal headers stay behind.
    steps.push(
        Step::run(
            "{src}",
            &[
                "{in:make-x86-64-self}/bin/make",
                "-C",
                "include/openssl",
                "install-data-am",
                "prefix={out}",
                &format!("SHELL={POST_BOOTSTRAP_SH}"),
            ],
        )
        .env("PATH", &path)
        .env("CONFIG_SHELL", POST_BOOTSTRAP_SH)
        .env("SHELL", POST_BOOTSTRAP_SH)
        .env("SOURCE_DATE_EPOCH", "1"),
    );
    steps.push(Step::Require {
        paths: vec![
            "{out}/lib/libcrypto.a".into(),
            "{out}/lib/libssl.a".into(),
            "{out}/include/openssl/crypto.h".into(),
            "{out}/include/openssl/opensslv.h".into(),
            "{out}/include/openssl/ssl.h".into(),
        ],
        exec: false,
    });

    Recipe::mesboot("libressl-x86-64", "4.3.2")
        .source_input("libressl-x86-64-source")
        .native_inputs(&[
            "gcc-x86-64-self",
            "binutils-x86-64-self",
            "glibc-x86-64",
            "make-x86-64-self",
            "busybox-x86-64",
        ])
        .steps(steps)
}

#[cfg(test)]
mod tests {
    use super::{recipe, POST_BOOTSTRAP_SH};
    use crate::types::Step;

    #[test]
    fn portable_static_build_uses_only_post_bootstrap_inputs() {
        let recipe = recipe();
        assert_eq!(
            recipe.native_inputs.as_deref(),
            Some(
                [
                    "gcc-x86-64-self",
                    "binutils-x86-64-self",
                    "glibc-x86-64",
                    "make-x86-64-self",
                    "busybox-x86-64",
                ]
                .map(str::to_string)
                .as_slice()
            )
        );
        assert!(recipe.inputs.is_none());
        let steps = recipe.steps.expect("libressl steps");
        let make_runs = steps
            .iter()
            .filter_map(|step| match step {
                Step::Run { argv, .. }
                    if argv.first().is_some_and(|arg| arg.contains("/bin/make")) =>
                {
                    Some(argv.join(" "))
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            make_runs,
            vec![
                format!(
                    "{{in:make-x86-64-self}}/bin/make -j{{jobs}} -C crypto libcrypto.la \
                     SHELL={POST_BOOTSTRAP_SH}"
                ),
                format!(
                    "{{in:make-x86-64-self}}/bin/make -j{{jobs}} -C ssl remove_bs_objects \
                     SHELL={POST_BOOTSTRAP_SH}"
                ),
                format!(
                    "{{in:make-x86-64-self}}/bin/make -C include/openssl install-data-am \
                     prefix={{out}} SHELL={POST_BOOTSTRAP_SH}"
                ),
            ]
        );
    }

    #[test]
    fn portable_static_build_keeps_the_target_profile_policy_last() {
        let steps = recipe().steps.expect("libressl steps");
        let wrapper = steps
            .iter()
            .find_map(|step| match step {
                Step::WriteFile { path, content, .. } if path == "{root}/wb/cc" => Some(content),
                _ => None,
            })
            .expect("compiler wrapper");
        let package_args = wrapper.find("\"$@\"").expect("package arguments");
        let profile_policy = wrapper
            .rfind("-fno-omit-frame-pointer")
            .expect("frame-pointer policy");
        assert!(package_args < profile_policy);
        for required in [
            "-g1",
            "-ffile-prefix-map={root}=/td-build-root",
            "-ffile-prefix-map={src}=/td-build",
            "-Wl,--build-id=sha1",
        ] {
            assert!(
                wrapper.contains(required),
                "missing target policy {required}"
            );
        }

        let configure = steps.iter().find_map(|step| match step {
            Step::Run { argv, .. } if argv.iter().any(|arg| arg == "./configure") => Some(argv),
            _ => None,
        });
        let configure = configure.expect("configure step");
        assert!(configure.iter().any(|arg| arg == "--prefix={out}"));
        assert!(configure.iter().any(|arg| arg == "--disable-shared"));
        assert!(configure.iter().any(|arg| arg == "--disable-asm"));
        assert!(configure.iter().any(|arg| arg == "--disable-tests"));
    }
}

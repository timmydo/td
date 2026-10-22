use crate::shared::{build_root_static_cc, unpack_into, BUILD_ROOT_PATH, BUILD_ROOT_SH as SH};
use crate::types::{Recipe, Step, TextEdit};

// LibreSSL 4.3.2 provides the OpenSSL-compatible TLS surface curl and Git will
// consume. It exports static libssl.a + libcrypto.a and the public headers,
// and no program: the libraries are absorbed into downstream runtime ELFs,
// which carry td's debug-companion policy at their own final link boundary.
//
// It compiles in the Debian build root against td glibc
// (`build_root_static_cc`); the archives' consumers link them.
//
// Upstream's hand-written assembly is disabled. The portable C implementation
// is compiled with td's target-wide frame-pointer, bounded line-table, and
// remapped-path flags; downstream runtime links supply their deterministic build
// IDs. That avoids introducing an assembly coverage exception merely for crypto
// acceleration. Upstream tests are disabled here; the sibling
// libressl-x86-64-test compiles, links, and runs a verified client/server TLS
// handshake against exactly the installed archives.
pub fn recipe() -> Recipe {
    let path = format!("{{root}}/wb:{BUILD_ROOT_PATH}");

    // GNU libtool expands the convenience libcompat.a into libcrypto.a with
    // `find`, from the root's image layer (findutils is Essential); without
    // it libtool still succeeds and emits an incomplete archive, which
    // libressl-x86-64-test's compat-member check refuses.
    let mut steps = unpack_into("libressl-x86-64-source", "{src}");

    // Libtool's generated x86-64 ABI probe names /usr/bin/file directly; route
    // the two possible conftest.o probes through a readelf adapter, so the
    // answer does not depend on whether the root carries file(1).
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
            "#!{SH}\n\
             h=$(/usr/bin/readelf -h \"$1\") || exit 1\n\
             case \"$h\" in\n\
             *'Class:'*'ELF64'*) printf '%s\\n' 'ELF 64-bit LSB relocatable';;\n\
             *'Class:'*'ELF32'*) printf '%s\\n' 'ELF 32-bit LSB relocatable';;\n\
             *) exit 1;;\n\
             esac\n"
        ),
        exec: true,
    });
    steps.extend(build_root_static_cc());

    steps.push(
        Step::run(
            "{src}",
            &[
                SH,
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
        .env("CONFIG_SHELL", SH)
        .env("SHELL", SH)
        .env("CC", "{root}/wb/cc")
        .env("CCAS", "{root}/wb/cc")
        .env("AR", "/usr/bin/ar")
        .env("RANLIB", "/usr/bin/ranlib")
        .env("NM", "/usr/bin/nm")
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
                    "/usr/bin/make",
                    "-j{jobs}",
                    "-C",
                    dir,
                    target,
                    &format!("SHELL={SH}"),
                ],
            )
            .env("PATH", &path)
            .env("CONFIG_SHELL", SH)
            .env("SHELL", SH)
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
                "/usr/bin/make",
                "-C",
                "include/openssl",
                "install-data-am",
                "prefix={out}",
                &format!("SHELL={SH}"),
            ],
        )
        .env("PATH", &path)
        .env("CONFIG_SHELL", SH)
        .env("SHELL", SH)
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
        .build_root()
        .native_inputs(&["glibc-x86-64", "gcc-runtime"])
        .steps(steps)
}

#[cfg(test)]
mod tests {
    use super::{recipe, SH};
    use crate::shared::assert_build_root_static_c;
    use crate::types::Step;

    #[test]
    fn portable_static_build_uses_only_build_root_inputs() {
        let recipe = recipe();
        assert_eq!(
            recipe.native_inputs.as_deref(),
            Some(
                ["glibc-x86-64", "gcc-runtime"]
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
                format!("/usr/bin/make -j{{jobs}} -C crypto libcrypto.la SHELL={SH}"),
                format!("/usr/bin/make -j{{jobs}} -C ssl remove_bs_objects SHELL={SH}"),
                format!(
                    "/usr/bin/make -C include/openssl install-data-am prefix={{out}} SHELL={SH}"
                ),
            ]
        );
        // Libraries and headers only: no `openssl` command, so no runtime
        // ELF to split or check.
        assert!(!steps.iter().any(|step| match step {
            Step::SplitDebugTree { .. } | Step::AssertStatic { .. } => true,
            Step::Require { paths, .. } => paths.iter().any(|path| path.contains("/bin/")),
            _ => false,
        }));
    }

    #[test]
    fn portable_static_build_keeps_the_target_profile_policy_last() {
        let recipe = recipe();
        assert_build_root_static_c(&recipe);
        let steps = recipe.steps.expect("libressl steps");
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

use crate::ladder::{
    post_rust_inputs, post_rust_tool_farm, split_target_debug, unpack_into, unpack_keep_top,
    POST_RUST_SH,
};
use crate::types::{Recipe, Step};

// A static, target-built cryptsetup for LUKS2 volumes (td-install/
// ENCRYPTION.md increment 3), under the repository owner's principle-2
// sign-off. It uses the kernel crypto backend (AF_ALG) and cryptsetup's
// internal Argon2, so no OpenSSL, libgcrypt or libargon2 enters. udev, SELinux,
// the kernel keyring, hardware OPAL, the external token loader and its plugins,
// veritysetup and integritysetup are disabled; LUKS2 re-encryption stays.
//
// The whole static closure (json-c, popt, libdevmapper, util-linux's libuuid
// and libblkid) is compiled by the self-hosted toolchain under the shipped
// target profile, and the binary is split into a runtime and a debug
// companion (td-profiler/DESIGN.md §2). The system image binds it at
// /bin/cryptsetup for the installer's device-bound formatting, with the
// build-time binding D7 requires of mkfs.btrfs (DESIGN.md D6).
/// The exact feature line the realized binary must report: blkid signature
/// detection and the kernel crypto API, and nothing else.
pub const VERSION_LINE: &str = "cryptsetup 2.8.8 flags: BLKID KERNEL_CAPI ";

pub fn recipe() -> Recipe {
    let sgcc = "{in:gcc-x86-64-self}/stage/td/store/gcc-14.3.0-x86_64-self/bin/gcc";
    let xglibc = "{in:glibc-x86-64}/stage/td/store/glibc-2.41-x86_64";
    let sbin = "{in:binutils-x86-64-self}/bin";
    let ul = "{in:util-linux-libs-x86-64}";
    let jc = "{in:json-c-x86-64}";
    let popt = "{in:popt-x86-64}";
    let dm = "{in:libdevmapper-x86-64}";
    let path = format!("{{root}}/wb:{{tools}}:{{in:make-x86-64-self}}/bin:{sbin}");
    let cip = format!(
        "{ul}/include:{jc}/include:{popt}/include:{dm}/include:{xglibc}/include:{{root}}/kh"
    );

    let mut steps = unpack_into("cryptsetup-x86-64-source", "{src}");
    steps.extend(unpack_keep_top("linux-headers-x86-64", "{root}/kh"));
    steps.push(post_rust_tool_farm("{in:gawk-x86-64-self}/bin/gawk"));
    steps.push(Step::PatchShebangs {
        dir: "{src}".into(),
        shell: POST_RUST_SH.into(),
    });
    steps.push(Step::WriteFile {
        path: "{root}/wb/cc".into(),
        content: format!(
            "#!{POST_RUST_SH}\nexec \"{sgcc}\" -static -B\"{sbin}/\" -B{xglibc}/lib -L{xglibc}/lib \
             -L{ul}/lib -L{jc}/lib -L{popt}/lib -L{dm}/lib \"$@\" \
             -fno-omit-frame-pointer -g1 \
             -ffile-prefix-map={{root}}=/td-build-root \
             -ffile-prefix-map={{src}}=/td-build \
             -Wl,--build-id=sha1\n"
        ),
        exec: true,
    });
    // The release configure script queries pkg-config for these four modules;
    // every answer names an explicit recipe input.
    steps.push(Step::WriteFile {
        path: "{root}/wb/pkg-config".into(),
        content: format!(
            "#!{POST_RUST_SH}\n\
             mod=''; cflags=0; libs=0; version=0\n\
             for a in \"$@\"; do\n\
             \tcase \"$a\" in\n\
             \t--atleast-pkgconfig-version) exit 0;;\n\
             \t--cflags) cflags=1;;\n\
             \t--libs) libs=1;;\n\
             \t--modversion) version=1;;\n\
             \t-*) ;;\n\
             \t*) [ -n \"$mod\" ] || mod=\"${{a%% *}}\";;\n\
             \tesac\n\
             done\n\
             case \"$mod\" in\n\
             \tblkid) inc='-I{ul}/include'; link='-L{ul}/lib -lblkid'; ver=2.42.2;;\n\
             \tuuid) inc='-I{ul}/include'; link='-L{ul}/lib -luuid'; ver=2.42.2;;\n\
             \tjson-c) inc='-I{jc}/include -I{jc}/include/json-c'; link='-L{jc}/lib -ljson-c -lm'; ver=0.18;;\n\
             \tdevmapper) inc='-I{dm}/include'; link='-L{dm}/lib -ldevmapper -lpthread -lm'; ver=1.02.217;;\n\
             \t*) exit 1;;\n\
             esac\n\
             out=''\n\
             [ \"$version\" = 1 ] && out=\"$ver\"\n\
             [ \"$cflags\" = 1 ] && out=\"${{out:+$out }}$inc\"\n\
             [ \"$libs\" = 1 ] && out=\"${{out:+$out }}$link\"\n\
             [ -n \"$out\" ] && printf '%s\\n' \"$out\"\n\
             exit 0\n"
        ),
        exec: true,
    });
    steps.push(
        Step::run(
            "{src}",
            &[
                POST_RUST_SH,
                "./configure",
                "--build=x86_64-pc-linux-gnu",
                "--host=x86_64-pc-linux-gnu",
                "--prefix=/td/store/cryptsetup-2.8.8-x86_64",
                "--disable-shared",
                "--enable-static",
                "--enable-static-cryptsetup",
                "--with-crypto_backend=kernel",
                "--disable-asciidoc",
                "--disable-nls",
                "--disable-rpath",
                "--disable-udev",
                "--disable-selinux",
                "--disable-keyring",
                "--disable-hw-opal",
                "--disable-external-tokens",
                "--disable-ssh-token",
                "--disable-veritysetup",
                "--disable-integritysetup",
                "--disable-fuzz-targets",
                "--without-libiconv-prefix",
                "--without-libintl-prefix",
            ],
        )
        .env("PATH", &path)
        .env("CONFIG_SHELL", POST_RUST_SH)
        .env("SHELL", POST_RUST_SH)
        .env("CC", "{root}/wb/cc")
        .env("CC_FOR_BUILD", "{root}/wb/cc")
        .env("AR", "{in:binutils-x86-64-self}/bin/ar")
        .env("RANLIB", "{in:binutils-x86-64-self}/bin/ranlib")
        .env("PKG_CONFIG", "{root}/wb/pkg-config")
        .env("C_INCLUDE_PATH", &cip)
        .env("SOURCE_DATE_EPOCH", "1"),
    );
    steps.push(
        Step::run(
            "{src}",
            &[
                "{in:make-x86-64-self}/bin/make",
                "-j{jobs}",
                "cryptsetup.static",
                &format!("SHELL={POST_RUST_SH}"),
                &format!("CONFIG_SHELL={POST_RUST_SH}"),
            ],
        )
        .env("PATH", &path)
        .env("CC", "{root}/wb/cc")
        .env("AR", "{in:binutils-x86-64-self}/bin/ar")
        .env("RANLIB", "{in:binutils-x86-64-self}/bin/ranlib")
        .env("C_INCLUDE_PATH", &cip)
        .env("SOURCE_DATE_EPOCH", "1"),
    );
    steps.push(Step::MkDir {
        path: "{out}/bin".into(),
    });
    steps.push(Step::CopyFiles {
        files: vec!["{src}/cryptsetup.static".into()],
        dest: "{out}/bin".into(),
    });
    steps.push(Step::Symlink {
        target: "cryptsetup.static".into(),
        link: "{out}/bin/cryptsetup".into(),
    });
    steps.push(Step::Require {
        paths: vec!["{out}/bin/cryptsetup".into()],
        exec: true,
    });
    steps.push(split_target_debug("{out}"));
    steps.push(Step::assert_static(&["{out}/bin/cryptsetup"]));
    steps.push(
        Step::run(
            "{out}",
            &[
                POST_RUST_SH,
                "-c",
                &format!(
                    "h=$('{{in:binutils-x86-64-self}}/bin/readelf' -h bin/cryptsetup); \
                     printf '%s\\n' \"$h\" | grep -i 'class:' | grep -qi 'ELF64' || {{ echo 'cryptsetup is not ELF64' >&2; exit 1; }}; \
                     printf '%s\\n' \"$h\" | grep -i 'machine:' | grep -qi 'x86-64' || {{ echo 'cryptsetup is not x86-64' >&2; exit 1; }}; \
                     v=$(bin/cryptsetup --version) || exit 1; \
                     [ \"$v\" = '{VERSION_LINE}' ] || {{ echo \"cryptsetup reports '$v', expected '{VERSION_LINE}'\" >&2; exit 1; }}"
                ),
            ],
        )
        .env("PATH", &path),
    );

    Recipe::mesboot("cryptsetup-x86-64", "2.8.8")
        .source_input("cryptsetup-x86-64-source")
        .native_inputs(&post_rust_inputs(
            "gawk-x86-64-self",
            &[
                "util-linux-libs-x86-64",
                "json-c-x86-64",
                "popt-x86-64",
                "libdevmapper-x86-64",
                "gcc-x86-64-self",
                "binutils-x86-64-self",
                "glibc-x86-64",
                "make-x86-64-self",
            ],
        ))
        .inputs(&["linux-headers-x86-64"])
        .steps(steps)
}

#[cfg(test)]
mod tests {
    use super::recipe;
    use crate::types::Step;

    #[test]
    fn shipped_binary_follows_the_target_profile_and_is_split() {
        let recipe = recipe();
        let inputs = recipe.native_inputs.clone().unwrap_or_default();
        for required in [
            "gcc-x86-64-self",
            "binutils-x86-64-self",
            "util-linux-libs-x86-64",
            "json-c-x86-64",
            "popt-x86-64",
            "libdevmapper-x86-64",
        ] {
            assert!(inputs.iter().any(|input| input == required), "{required}");
        }
        for bootstrap in ["gcc-x86-64-native", "binutils-x86-64-native"] {
            assert!(
                !inputs.iter().any(|input| input == bootstrap),
                "{bootstrap}"
            );
        }
        let steps = recipe.steps.unwrap_or_default();
        let wrapper = steps
            .iter()
            .find_map(|step| match step {
                Step::WriteFile { path, content, .. } if path == "{root}/wb/cc" => Some(content),
                _ => None,
            })
            .expect("compiler wrapper");
        let package = wrapper.find("\"$@\"").expect("package flags");
        let policy = wrapper
            .find("-fno-omit-frame-pointer")
            .expect("target profile");
        assert!(package < policy, "package flags could override the profile");
        for required in [
            "-g1",
            "-ffile-prefix-map={root}=/td-build-root",
            "-ffile-prefix-map={src}=/td-build",
            "-Wl,--build-id=sha1",
            "-B\"{in:binutils-x86-64-self}/bin/\"",
        ] {
            assert!(wrapper.contains(required), "missing {required}");
        }
        let split = steps
            .iter()
            .position(|step| matches!(step, Step::SplitDebugTree { root, .. } if root == "{out}"))
            .expect("debug split");
        let installed = steps
            .iter()
            .position(|step| matches!(step, Step::CopyFiles { dest, .. } if dest == "{out}/bin"))
            .expect("install");
        assert!(installed < split, "the split must see the installed binary");
        let checked = steps
            .iter()
            .position(|step| matches!(step, Step::AssertStatic { .. }))
            .expect("static check");
        assert!(split < checked, "the checks must see the shipped runtime");
    }

    #[test]
    fn configure_selects_the_kernel_backend_and_excludes_the_unapproved_closure() {
        let configure = recipe()
            .steps
            .unwrap_or_default()
            .into_iter()
            .find_map(|step| match step {
                Step::Run { argv, .. } if argv.iter().any(|arg| arg == "./configure") => Some(argv),
                _ => None,
            })
            .expect("configure step");
        for required in [
            "--with-crypto_backend=kernel",
            "--enable-static-cryptsetup",
            "--disable-udev",
            "--disable-keyring",
            "--disable-external-tokens",
            "--disable-ssh-token",
            "--disable-hw-opal",
            "--disable-selinux",
            "--disable-veritysetup",
            "--disable-integritysetup",
            "--disable-shared",
        ] {
            assert!(configure.iter().any(|arg| arg == required), "{required}");
        }
        for forbidden in ["--disable-luks2-reencryption", "--enable-libargon2"] {
            assert!(!configure.iter().any(|arg| arg == forbidden), "{forbidden}");
        }
        assert_eq!(
            configure
                .iter()
                .filter(|arg| arg.starts_with("--with-crypto_backend="))
                .count(),
            1
        );
    }

    /// The compiler wrapper and the pkg-config shim are where an undeclared
    /// library would enter; every store reference in them is a recipe input.
    #[test]
    fn wrappers_name_only_declared_inputs() {
        let declared: Vec<String> = recipe()
            .native_inputs
            .unwrap_or_default()
            .into_iter()
            .chain(recipe().inputs.unwrap_or_default())
            .collect();
        for wrapper in ["{root}/wb/cc", "{root}/wb/pkg-config"] {
            let content = recipe()
                .steps
                .unwrap_or_default()
                .into_iter()
                .find_map(|step| match step {
                    Step::WriteFile { path, content, .. } if path == wrapper => Some(content),
                    _ => None,
                })
                .expect("wrapper");
            let mut rest = content.as_str();
            let mut seen = 0;
            while let Some(start) = rest.find("{in:") {
                let tail = &rest[start + 4..];
                let end = tail.find('}').expect("closed input reference");
                let name = &tail[..end];
                assert!(
                    declared.iter().any(|input| input == name),
                    "{wrapper} names undeclared {name}"
                );
                seen += 1;
                rest = &tail[end..];
            }
            assert!(seen > 0, "{wrapper} names no input");
        }
    }
}

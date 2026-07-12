use crate::ladder::{base_inputs, base_path, unpack_into, unpack_keep_top, SH};
use crate::types::{Recipe, Step};

pub fn recipe() -> Recipe {
    let ngcc = "{in:gcc-x86-64-native}/stage/td/store/gcc-14.3.0-x86_64-native/bin/gcc";
    let ngpp = "{in:gcc-x86-64-native}/stage/td/store/gcc-14.3.0-x86_64-native/bin/g++";
    let sbin = "{in:binutils-x86-64-self}/bin";
    let nbin = "{in:binutils-x86-64-native}/bin";
    let xglibc = "{in:glibc-x86-64}/stage/td/store/glibc-2.41-x86_64";
    let path = format!("{sbin}:{nbin}:{}", base_path());
    let cip = "{src}/mpfr/src";
    let lp = "{root}/sysroot/lib";
    let mut steps = unpack_into("gcc-x86-64-self-source", "{src}");

    for t in ["gmp63", "mpfr421", "mpc131"] {
        steps.extend(unpack_keep_top(t, "{src}"));
    }
    steps.push(Step::Symlink {
        target: "gmp-6.3.0".into(),
        link: "{src}/gmp".into(),
    });
    steps.push(Step::Symlink {
        target: "mpfr-4.2.1".into(),
        link: "{src}/mpfr".into(),
    });
    steps.push(Step::Symlink {
        target: "mpc-1.3.1".into(),
        link: "{src}/mpc".into(),
    });
    steps.push(Step::CopyTree {
        from: format!("{xglibc}/include"),
        dest: "{root}/sysroot/include".into(),
    });
    steps.extend(unpack_keep_top(
        "linux-headers-x86-64",
        "{root}/sysroot/include",
    ));
    steps.push(Step::CopyTree {
        from: format!("{xglibc}/lib"),
        dest: "{root}/sysroot/lib".into(),
    });
    steps.push(
        Step::run(
            "{root}",
            &[
                SH,
                "-c",
                "for f in libc.so libm.so libm.a; do p={root}/sysroot/lib/$f; \
                 [ -f \"$p\" ] || continue; \
                 head -c 80 \"$p\" | grep -q 'GNU ld script' || continue; \
                 sed -i 's,/td/store/glibc-2.41-x86_64/lib/,,g' \"$p\"; done",
            ],
        )
        .env("PATH", &base_path()),
    );
    steps.push(Step::WriteFile {
        path: "{root}/wb/gcc".into(),
        content: format!(
            "#!{SH}\n\
             for a in \"$@\"; do case \"$a\" in -shared) exec \"{ngcc}\" -idirafter {{root}}/sysroot/include -B{{root}}/sysroot/lib \"$@\";; esac; done\n\
             exec \"{ngcc}\" -static -idirafter {{root}}/sysroot/include -B{{root}}/sysroot/lib \"$@\"\n"
        ),
        exec: true,
    });
    steps.push(Step::WriteFile {
        path: "{root}/wb/g++".into(),
        content: format!(
            "#!{SH}\n\
             for a in \"$@\"; do case \"$a\" in -shared) exec \"{ngpp}\" -idirafter {{root}}/sysroot/include -B{{root}}/sysroot/lib \"$@\";; esac; done\n\
             exec \"{ngpp}\" -static -idirafter {{root}}/sysroot/include -B{{root}}/sysroot/lib \"$@\"\n"
        ),
        exec: true,
    });
    steps.push(Step::ToolFarm {
        links: vec![
            ("awk".into(), "{in:gawk}/bin/awk".into()),
            ("flex".into(), "{in:flex}/bin/flex".into()),
            ("lex".into(), "{in:flex}/bin/flex".into()),
            ("bison".into(), "{in:bison}/bin/bison".into()),
            ("yacc".into(), "{in:bison}/bin/bison".into()),
            ("m4".into(), "{in:m4}/bin/m4".into()),
            ("make".into(), "{in:make}/bin/make".into()),
        ],
    });
    steps.push(Step::PatchShebangs {
        dir: "{src}".into(),
        shell: SH.into(),
    });
    steps.push(Step::MkDir {
        path: "{src}/bld".into(),
    });
    steps.push(
        Step::run(
            "{src}/bld",
            &[
                SH,
                "../configure",
                "--prefix=/td/store/gcc-14.3.0-x86_64-self",
                "--build=x86_64-pc-linux-gnu",
                "--host=x86_64-pc-linux-gnu",
                "--target=x86_64-pc-linux-gnu",
                &format!("--with-as={sbin}/as"),
                &format!("--with-ld={sbin}/ld"),
                "--with-build-sysroot={root}/sysroot",
                "--with-native-system-header-dir=/include",
                "--disable-bootstrap",
                "--disable-multilib",
                "--disable-shared",
                "--enable-static",
                "--enable-languages=c,c++",
                "--enable-threads=single",
                "--disable-libstdcxx-pch",
                "--disable-libatomic",
                "--disable-libgomp",
                "--disable-libitm",
                "--disable-libsanitizer",
                "--disable-libssp",
                "--disable-libvtv",
                "--disable-libquadmath",
                "--disable-lto",
                "--disable-plugin",
                "--disable-libcc1",
                "--disable-decimal-float",
                "--disable-werror",
            ],
        )
        .env("PATH", &path)
        .env("CONFIG_SHELL", SH)
        .env("CC", "{root}/wb/gcc")
        .env("CXX", "{root}/wb/g++")
        .env("CPP", "{root}/wb/gcc -E")
        .env("CC_FOR_BUILD", "{root}/wb/gcc")
        .env("CXX_FOR_BUILD", "{root}/wb/g++")
        .env("C_INCLUDE_PATH", cip)
        .env("CPLUS_INCLUDE_PATH", cip)
        .env("LIBRARY_PATH", lp)
        .env("LDFLAGS", "-static"),
    );
    steps.push(
        Step::run(
            "{src}/bld",
            &[
                "{in:make}/bin/make",
                "-j{jobs}",
                "SHELL={in:bash-mesboot}/bin/bash",
                "CONFIG_SHELL={in:bash-mesboot}/bin/bash",
                "MAKEINFO=true",
                "LDFLAGS=-static",
                "LDFLAGS_FOR_TARGET=-static",
            ],
        )
        .env("PATH", &path)
        .env("CONFIG_SHELL", SH)
        .env("C_INCLUDE_PATH", cip)
        .env("CPLUS_INCLUDE_PATH", cip)
        .env("LIBRARY_PATH", lp),
    );
    steps.push(
        Step::run(
            "{src}/bld",
            &[
                "{in:make}/bin/make",
                "SHELL={in:bash-mesboot}/bin/bash",
                "MAKEINFO=true",
                "install",
                "DESTDIR={out}/stage",
            ],
        )
        .env("PATH", &path)
        .env("CONFIG_SHELL", SH)
        .env("C_INCLUDE_PATH", cip)
        .env("CPLUS_INCLUDE_PATH", cip)
        .env("LIBRARY_PATH", lp),
    );
    steps.push(Step::Require {
        paths: vec![
            "{out}/stage/td/store/gcc-14.3.0-x86_64-self/bin/gcc".into(),
            "{out}/stage/td/store/gcc-14.3.0-x86_64-self/bin/g++".into(),
        ],
        exec: true,
    });

    Recipe::mesboot("gcc-x86-64-self", "14.3.0")
        .source_input("gcc-14-source")
        .native_inputs(&[
            "gcc-x86-64-native",
            "binutils-x86-64-self",
            "binutils-x86-64-native",
            "glibc-x86-64",
        ])
        .inputs_owned(base_inputs(&[
            "gmp63",
            "mpfr421",
            "mpc131",
            "linux-headers-x86-64",
            "flex",
            "bison",
            "m4",
            "make",
        ]))
        .steps(steps)
}

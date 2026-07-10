use crate::ladder::{base_inputs, base_path, unpack_into, unpack_keep_top, SH};
use crate::types::{Recipe, Step};

// GCC 14.3.0 cross STAGE2 (#378 slice 4, guix's cross gcc final): the FULL cross
// compiler — c,c++ --enable-shared --enable-threads=posix against the x86_64
// glibc 2.41 sysroot → x86_64-pc-linux-gnu-{gcc,g++} + libgcc_s.so.1 (rustc needs
// it dynamically) + libstdc++.so.6. Built STATIC by the i686 gcc 14.3.0 (like
// stage1; NOT incremental on stage1 — a full build against the now-complete
// sysroot). The sysroot is assembled from the x86_64 kernel UAPI headers +
// glibc-x86-64's headers/libs. gmp/mpfr/mpc in-tree. --with-as/--with-ld at the
// cross binutils' stable content-addressed as/ld. Install DESTDIR={out}/stage,
// logical --prefix=/td/store/gcc-14.3.0-x86_64.
pub fn recipe() -> Recipe {
    let xbin = "{in:binutils-x86-64}/bin";
    let path = format!("{xbin}:{{in:binutils-244}}/bin:{}", base_path());
    let xglibc = "{in:glibc-x86-64}/stage/td/store/glibc-2.41-x86_64";
    let mut steps = unpack_into("gcc-x86-64-stage2-source", "{src}");
    for t in ["gmp63", "mpfr421", "mpc131"] {
        steps.push(
            Step::run(
                "{src}",
                &["{in:tar}/bin/tar", "-xf", &format!("{{in:{t}}}")],
            )
            .env("PATH", &base_path()),
        );
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
    // assemble the sysroot: x86_64 kernel UAPI headers + glibc-x86-64 headers/libs.
    steps.extend(unpack_keep_top(
        "linux-headers-x86-64",
        "{root}/sysroot/usr/include",
    ));
    steps.push(Step::CopyTree {
        from: format!("{xglibc}/include"),
        dest: "{root}/sysroot/usr/include".into(),
    });
    steps.push(Step::CopyTree {
        from: format!("{xglibc}/lib"),
        dest: "{root}/sysroot/usr/lib".into(),
    });
    steps.push(Step::Symlink {
        target: "usr/lib".into(),
        link: "{root}/sysroot/lib".into(),
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
    for (name, real) in [("cc", "gcc"), ("cxx", "g++")] {
        steps.push(Step::WriteFile {
            path: format!("{{root}}/wb/{name}"),
            content: format!(
                "#!{SH}\nexec \"{{in:gcc-14}}/stage/td/store/gcc-14.3.0/bin/{real}\" -static -idirafter {{in:glibc-mesboot}}/include -B{{in:glibc-mesboot}}/lib -frandom-seed=tdgcc14repro \"$@\"\n"
            ),
            exec: true,
        });
    }
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
                "--build=i686-pc-linux-gnu",
                "--host=i686-pc-linux-gnu",
                "--target=x86_64-pc-linux-gnu",
                "--prefix=/td/store/gcc-14.3.0-x86_64",
                "--with-sysroot={root}/sysroot",
                &format!("--with-as={xbin}/x86_64-pc-linux-gnu-as"),
                &format!("--with-ld={xbin}/x86_64-pc-linux-gnu-ld"),
                "--enable-languages=c,c++",
                "--enable-shared",
                "--enable-threads=posix",
                "--enable-c99",
                "--with-glibc-version=2.41",
                "--disable-bootstrap",
                "--disable-multilib",
                "--disable-libssp",
                "--disable-libgomp",
                "--disable-libquadmath",
                "--disable-libvtv",
                "--disable-libitm",
                "--disable-libcc1",
                "--disable-libsanitizer",
                "--disable-lto",
                "--disable-plugin",
                "--disable-decimal-float",
                "--disable-libstdcxx-pch",
                "--disable-werror",
            ],
        )
        .env("PATH", &path)
        .env("CONFIG_SHELL", SH)
        .env("CC", "{root}/wb/cc")
        .env("CXX", "{root}/wb/cxx")
        .env("CPP", "{root}/wb/cc -E")
        .env("CC_FOR_BUILD", "{root}/wb/cc")
        .env("CXX_FOR_BUILD", "{root}/wb/cxx"),
    );
    steps.push(
        Step::run(
            "{src}/bld",
            &[
                "{in:make}/bin/make",
                "-j{jobs}",
                "SHELL={in:bash}/bin/bash",
                "CONFIG_SHELL={in:bash}/bin/bash",
                "MAKEINFO=true",
            ],
        )
        .env("PATH", &path)
        .env("CONFIG_SHELL", SH),
    );
    steps.push(
        Step::run(
            "{src}/bld",
            &[
                "{in:make}/bin/make",
                "SHELL={in:bash}/bin/bash",
                "MAKEINFO=true",
                "install",
                "DESTDIR={out}/stage",
            ],
        )
        .env("PATH", &path)
        .env("CONFIG_SHELL", SH),
    );
    steps.push(Step::Require {
        paths: vec![
            "{out}/stage/td/store/gcc-14.3.0-x86_64/bin/x86_64-pc-linux-gnu-gcc".into(),
            "{out}/stage/td/store/gcc-14.3.0-x86_64/bin/x86_64-pc-linux-gnu-g++".into(),
        ],
        exec: true,
    });
    Recipe::mesboot("gcc-x86-64-stage2", "14.3.0")
        .source_input("gcc-14-source")
        .native_inputs(&[
            "gcc-14",
            "glibc-mesboot",
            "binutils-x86-64",
            "glibc-x86-64",
            "binutils-244",
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

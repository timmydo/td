use crate::ladder::{
    gcc14_configure_fixups, gcc_disable_selftest, gcc_install_headers_without_tar, mesboot0_inputs,
    mesboot0_path, unpack_into, unpack_keep_top, SH,
};
use crate::types::{Recipe, Step};

// GCC 14.3.0 cross STAGE1 (#378 slice 4, guix's cross gcc stage1): C only,
// --without-headers --with-newlib --disable-shared, `make all-gcc
// all-target-libgcc` — the minimal cross compiler (x86_64-pc-linux-gnu-gcc, an
// i686 binary emitting x86_64) + a bootstrap libgcc.a, enough to build the
// x86_64 glibc. Built STATIC by the i686 gcc 14.3.0 (via single-token wrappers,
// same munging-proof trick as the i686 gcc-14 rung), against the static glibc
// 2.16 for the host parts. gmp/mpfr/mpc in-tree. --with-as/--with-ld point at
// the cross binutils' STABLE content-addressed as/ld (the shell's
// _x86_stable_tooldir /tmp dance is retired — the input path is already
// deterministic). -frandom-seed pins cc1's file-scope-static symbol naming
// (else gcc reads /dev/urandom → non-reproducible). Install DESTDIR={out}/stage,
// logical --prefix=/td/store/gcc-14.3.0-x86_64.
// Host-free build tools: mesboot0 + make-mesboot; flex/bison/m4 dead (gcc-14-source). re #469.
pub fn recipe() -> Recipe {
    let xbin = "{in:binutils-x86-64}/bin";
    let path = format!("{xbin}:{{in:binutils-244}}/bin:{}", mesboot0_path());
    let mut steps = unpack_into("gcc-x86-64-stage1-source", "{src}");
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
    // the x86_64 kernel UAPI headers → the sysroot (--with-sysroot; no libc yet).
    steps.extend(unpack_keep_top(
        "linux-headers-x86-64",
        "{root}/sysroot/usr/include",
    ));
    steps.push(Step::ToolFarm {
        links: vec![
            ("awk".into(), "{in:gawk-mesboot}/bin/gawk".into()),
            ("gawk".into(), "{in:gawk-mesboot}/bin/gawk".into()),
            ("make".into(), "{in:make-mesboot}/bin/make".into()),
        ],
    });
    // single-token static i686 gcc-14 wrappers (munging-proof) with the repro seed.
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
    // Same bash-mesboot configure fixups as gcc-14 (this is the same GCC 14.3.0
    // source configured under bash-mesboot). No libtool find fix: stage1 is
    // --disable-libstdcxx (--enable-languages=c), so it builds no libstdc++.
    steps.extend(gcc14_configure_fixups());
    steps.push(gcc_disable_selftest());
    steps.push(gcc_install_headers_without_tar());
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
                "--enable-languages=c",
                "--without-headers",
                "--with-newlib",
                "--with-glibc-version=2.41",
                "--disable-bootstrap",
                "--disable-multilib",
                "--disable-shared",
                "--disable-threads",
                "--disable-libssp",
                "--disable-libgomp",
                "--disable-libquadmath",
                "--disable-libatomic",
                "--disable-libvtv",
                "--disable-libitm",
                "--disable-libstdcxx",
                "--disable-libcc1",
                "--disable-lto",
                "--disable-plugin",
                "--disable-decimal-float",
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
                "{in:make-mesboot}/bin/make",
                "-j{jobs}",
                "SHELL={in:bash-mesboot}/bin/bash",
                "CONFIG_SHELL={in:bash-mesboot}/bin/bash",
                "MAKEINFO=true",
                "all-gcc",
                "all-target-libgcc",
            ],
        )
        .env("PATH", &path)
        .env("CONFIG_SHELL", SH),
    );
    steps.push(
        Step::run(
            "{src}/bld",
            &[
                "{in:make-mesboot}/bin/make",
                "SHELL={in:bash-mesboot}/bin/bash",
                "MAKEINFO=true",
                "install-gcc",
                "install-target-libgcc",
                "DESTDIR={out}/stage",
            ],
        )
        .env("PATH", &path)
        .env("CONFIG_SHELL", SH),
    );
    steps.push(Step::Require {
        paths: vec!["{out}/stage/td/store/gcc-14.3.0-x86_64/bin/x86_64-pc-linux-gnu-gcc".into()],
        exec: true,
    });
    Recipe::mesboot("gcc-x86-64-stage1", "14.3.0")
        .source_input("gcc-14-source")
        .native_inputs(&[
            "gcc-14",
            "glibc-mesboot",
            "binutils-x86-64",
            "binutils-244",
            "gawk-mesboot",
            "make-mesboot",
        ])
        .inputs_owned(mesboot0_inputs(&[
            "gmp63",
            "mpfr421",
            "mpc131",
            "linux-headers-x86-64",
        ]))
        .steps(steps)
}

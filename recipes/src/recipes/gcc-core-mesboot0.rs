use crate::ladder::{apply_patch, base_path, unpack_into, SH};
use crate::types::{Recipe, Step};

// GCC 2.95.3 — bootstrap rung 7 (#378, guix's gcc-core-mesboot0): tcc + the
// binutils-mesboot0 as/ld build the first gcc against the mes libc. Faithful
// port of the deleted build_gcc fn: boot patch, config.cache float-format
// hint, texinfo stubs, shebang rewrite, BOOT_LDFLAGS at tcc's crt dir, and the
// install2 ar-assembly of libgcc.a/libc.a into gcc-lib (+ crt/libgcc2.a copies)
// so the compiler can link on its own.
pub fn recipe() -> Recipe {
    let path = base_path();
    let cip = "{in:mes}/include:{in:mes}/include/x86";
    let lp = "{in:tcc}/lib";
    let gccdir = "{out}/lib/gcc-lib/i686-unknown-linux-gnu/2.95.3";
    let mut steps = unpack_into("gcc-core-mesboot0-source", "{src}");
    steps.push(apply_patch("patch-mesboot", "patch-gcc-boot-2.95.3"));
    steps.push(Step::CopyFiles {
        files: vec![
            "{in:tcc}/lib/crt1.o".into(),
            "{in:tcc}/lib/crti.o".into(),
            "{in:tcc}/lib/crtn.o".into(),
            "{in:tcc}/lib/libc.a".into(),
            "{in:tcc}/lib/libtcc1.a".into(),
        ],
        dest: "{src}".into(),
    });
    steps.push(Step::ToolFarm {
        links: vec![
            ("tcc".into(), "{in:tcc}/bin/tcc".into()),
            ("make".into(), "{in:make-mesboot0}/bin/make".into()),
            ("patch".into(), "{in:patch-mesboot}/bin/patch".into()),
            ("awk".into(), "{in:gawk}/bin/awk".into()),
            ("flex".into(), "{in:flex}/bin/flex".into()),
            ("lex".into(), "{in:flex}/bin/flex".into()),
            ("bison".into(), "{in:bison}/bin/bison".into()),
            ("yacc".into(), "{in:bison}/bin/bison".into()),
        ],
    });
    // binutils' whole bin dir onto the farm (as/ld/ar/ranlib/nm/strip/…).
    steps.push(
        Step::run(
            "{root}",
            &[
                "{in:coreutils}/bin/ln",
                "-sf",
                "glob:{in:binutils-mesboot0}/bin/*",
                "{tools}",
            ],
        )
        .env("PATH", &path),
    );
    steps.push(Step::WriteFile {
        path: "{src}/config.cache".into(),
        content: "ac_cv_c_float_format='IEEE (little-endian)'\n".into(),
        exec: false,
    });
    steps.push(
        Step::run(
            "{src}",
            &[
                SH,
                "./configure",
                "--enable-static",
                "--disable-shared",
                "--disable-werror",
                "--build=i686-unknown-linux-gnu",
                "--host=i686-unknown-linux-gnu",
                "--prefix={out}",
            ],
        )
        .env("PATH", &path)
        .env("CONFIG_SHELL", SH)
        .env("C_INCLUDE_PATH", cip)
        .env("LIBRARY_PATH", lp)
        .env("CPPFLAGS", " -D __GLIBC_MINOR__=6")
        .env("CC", "tcc -D __GLIBC_MINOR__=6")
        .env("CC_FOR_BUILD", "tcc -D __GLIBC_MINOR__=6")
        .env("CPP", "tcc -E -D __GLIBC_MINOR__=6"),
    );
    steps.push(Step::Require {
        paths: vec!["{src}/Makefile".into()],
        exec: false,
    });
    steps.push(
        Step::run("{src}", &["{in:coreutils}/bin/rm", "-rf", "texinfo"]).env("PATH", &path),
    );
    steps.push(Step::MkDir {
        path: "{src}/gcc".into(),
    });
    for stub in ["{src}/gcc/cpp.info", "{src}/gcc/gcc.info"] {
        steps.push(Step::WriteFile {
            path: stub.into(),
            content: String::new(),
            exec: false,
        });
    }
    steps.push(Step::PatchShebangs {
        dir: "{src}".into(),
        shell: SH.into(),
    });
    steps.push(
        Step::run(
            "{src}",
            &[
                "{in:make-mesboot0}/bin/make",
                "SHELL={in:bash}/bin/bash",
                "CONFIG_SHELL={in:bash}/bin/bash",
                "CC=tcc -static -D __GLIBC_MINOR__=6",
                "OLDCC=tcc -static -D __GLIBC_MINOR__=6",
                "CC_FOR_BUILD=tcc -static -D __GLIBC_MINOR__=6",
                "AR=ar",
                "RANLIB=ranlib",
                "LIBGCC2_INCLUDES=-I {in:mes}/include",
                "LANGUAGES=c",
                "BOOT_LDFLAGS=-B{in:tcc}/lib/",
            ],
        )
        .env("PATH", &path)
        .env("C_INCLUDE_PATH", cip)
        .env("LIBRARY_PATH", lp),
    );
    steps.push(
        Step::run(
            "{src}",
            &[
                "{in:make-mesboot0}/bin/make",
                "SHELL={in:bash}/bin/bash",
                "CONFIG_SHELL={in:bash}/bin/bash",
                "AR=ar",
                "RANLIB=ranlib",
                "LANGUAGES=c",
                "install",
            ],
        )
        .env("PATH", &path)
        .env("C_INCLUDE_PATH", cip)
        .env("LIBRARY_PATH", lp),
    );
    // install2 (guix gcc-core-mesboot0): libgcc.a = libgcc2.a ∪ libtcc1.a and
    // libc.a = libc.o ∪ libtcc1.o, assembled with binutils' ar into gcc-lib.
    steps.push(Step::MkDir { path: format!("{gccdir}") });
    for d in ["{root}/tg", "{root}/tc2"] {
        steps.push(Step::MkDir { path: d.into() });
    }
    let ar = "{in:binutils-mesboot0}/bin/ar";
    steps.push(Step::run("{root}/tg", &[ar, "x", "{src}/gcc/libgcc2.a"]).env("PATH", &path));
    steps.push(Step::run("{root}/tg", &[ar, "x", "{in:tcc}/lib/libtcc1.a"]).env("PATH", &path));
    steps.push(
        Step::run("{root}/tg", &[ar, "r", &format!("{gccdir}/libgcc.a"), "glob:{root}/tg/*.o"])
            .env("PATH", &path),
    );
    steps.push(Step::run("{root}/tc2", &[ar, "x", "{in:tcc}/lib/libtcc1.a"]).env("PATH", &path));
    steps.push(Step::run("{root}/tc2", &[ar, "x", "{in:tcc}/lib/libc.a"]).env("PATH", &path));
    steps.push(
        Step::run(
            "{root}/tc2",
            &[ar, "r", &format!("{gccdir}/libc.a"), "{root}/tc2/libc.o", "{root}/tc2/libtcc1.o"],
        )
        .env("PATH", &path),
    );
    steps.push(Step::CopyFiles {
        files: vec![
            "{in:tcc}/lib/crt1.o".into(),
            "{in:tcc}/lib/crti.o".into(),
            "{in:tcc}/lib/crtn.o".into(),
            "{src}/gcc/libgcc2.a".into(),
        ],
        dest: "{out}/lib".into(),
    });
    steps.push(Step::Require {
        paths: vec!["{out}/bin/gcc".into()],
        exec: true,
    });
    Recipe::mesboot("gcc-core-mesboot0", "2.95.3")
        .native_inputs(&[
            "mes",
            "tcc",
            "make-mesboot0",
            "patch-mesboot",
            "binutils-mesboot0",
        ])
        .inputs(&[
            "patch-gcc-boot-2.95.3",
            "flex",
            "bison",
            "bash",
            "coreutils",
            "sed",
            "grep",
            "gawk",
            "tar",
            "gzip",
            "bzip2",
            "xz",
            "findutils",
            "diffutils",
        ])
        .steps(steps)
}

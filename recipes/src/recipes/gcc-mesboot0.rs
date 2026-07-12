use crate::ladder::{SH, apply_patch, base_inputs, base_path, link_bins, unpack_into};
use crate::types::{Recipe, Step};

// GCC 2.95.3 #2 — bootstrap rung 9 (#378, guix's gcc-mesboot0): the FIRST gcc
// rebuilds itself, now linking against glibc-mesboot0 instead of the mes libc.
// Same shape as gcc-core-mesboot0 with CC = the first gcc, RANLIB=true, and the
// simpler install2 (no libtcc1 merge).
pub fn recipe() -> Recipe {
    let path = base_path();
    let gccdir1 = "{in:gcc-core-mesboot0}/lib/gcc-lib/i686-unknown-linux-gnu/2.95.3";
    let cip = format!("{{in:glibc-mesboot0}}/include:{gccdir1}/include:{{in:mesboot-headers}}/include");
    let lp = format!("{{in:glibc-mesboot0}}/lib:{gccdir1}");
    let gccdir2 = "{out}/lib/gcc-lib/i686-unknown-linux-gnu/2.95.3";
    let mut steps = unpack_into("gcc-mesboot0-source", "{src}");
    steps.push(apply_patch("patch-mesboot", "patch-gcc-boot-2.95.3"));
    steps.push(Step::ToolFarm {
        links: vec![
            ("gcc".into(), "{in:gcc-core-mesboot0}/bin/gcc".into()),
            ("cpp".into(), "{in:gcc-core-mesboot0}/bin/cpp".into()),
            ("make".into(), "{in:make-mesboot0}/bin/make".into()),
            ("patch".into(), "{in:patch-mesboot}/bin/patch".into()),
            ("awk".into(), "{in:gawk}/bin/awk".into()),
            ("flex".into(), "{in:flex}/bin/flex".into()),
            ("lex".into(), "{in:flex}/bin/flex".into()),
            ("bison".into(), "{in:bison}/bin/bison".into()),
            ("yacc".into(), "{in:bison}/bin/bison".into()),
        ],
    });
    steps.push(
        link_bins("binutils-mesboot0"),
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
                "--disable-shared",
                "--disable-werror",
                "--build=i686-unknown-linux-gnu",
                "--host=i686-unknown-linux-gnu",
                "--prefix={out}",
            ],
        )
        .env("PATH", &path)
        .env("CONFIG_SHELL", SH)
        .env("C_INCLUDE_PATH", &cip)
        .env("LIBRARY_PATH", &lp)
        .env("CC", "{in:gcc-core-mesboot0}/bin/gcc")
        .env("CPP", "{in:gcc-core-mesboot0}/bin/gcc -E"),
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
                "SHELL={in:bash-mesboot}/bin/bash",
                "CONFIG_SHELL={in:bash-mesboot}/bin/bash",
                "CC={in:gcc-core-mesboot0}/bin/gcc",
                "RANLIB=true",
                "LIBGCC2_INCLUDES=-I {in:gcc-core-mesboot0}/include",
                "LANGUAGES=c",
            ],
        )
        .env("PATH", &path)
        .env("C_INCLUDE_PATH", &cip)
        .env("LIBRARY_PATH", &lp),
    );
    steps.push(
        Step::run(
            "{src}",
            &[
                "{in:make-mesboot0}/bin/make",
                "SHELL={in:bash-mesboot}/bin/bash",
                "CC={in:gcc-core-mesboot0}/bin/gcc",
                "RANLIB=true",
                "LANGUAGES=c",
                "install",
            ],
        )
        .env("PATH", &path)
        .env("C_INCLUDE_PATH", &cip)
        .env("LIBRARY_PATH", &lp),
    );
    steps.push(Step::MkDir { path: gccdir2.into() });
    steps.push(Step::MkDir { path: "{root}/tg".into() });
    let ar = "{in:binutils-mesboot0}/bin/ar";
    steps.push(Step::run("{root}/tg", &[ar, "x", "{src}/gcc/libgcc2.a"]).env("PATH", &path));
    steps.push(
        Step::run(
            "{root}/tg",
            &[ar, "r", &format!("{gccdir2}/libgcc.a"), "glob:{root}/tg/*.o"],
        )
        .env("PATH", &path),
    );
    steps.push(Step::CopyFiles {
        files: vec!["{src}/gcc/libgcc2.a".into()],
        dest: "{out}/lib".into(),
    });
    steps.push(Step::Require {
        paths: vec!["{out}/bin/gcc".into()],
        exec: true,
    });
    Recipe::mesboot("gcc-mesboot0", "2.95.3")
        .source_input("gcc-core-source")
        .native_inputs(&[
            "make-mesboot0",
            "patch-mesboot",
            "binutils-mesboot0",
            "gcc-core-mesboot0",
            "glibc-mesboot0",
            "mesboot-headers",
        ])
        .inputs_owned(base_inputs(&["patch-gcc-boot-2.95.3", "flex", "bison"]))
        .steps(steps)
}

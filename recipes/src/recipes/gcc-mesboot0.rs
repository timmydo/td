use crate::ladder::{SH, apply_patch, link_bins_mesboot0, mesboot0_inputs, mesboot0_path, unpack_into};
use crate::types::{Recipe, Step, TextEdit};

// GCC 2.95.3 #2 — bootstrap rung 9 (#378, guix's gcc-mesboot0): the FIRST gcc
// rebuilds itself, now linking against glibc-mesboot0 instead of the mes libc.
// Same shape as gcc-core-mesboot0 with CC = the first gcc, RANLIB=true, and the
// simpler install2 (no libtcc1 merge).
//
// Host-tool ingress closed (re #469): cut over to the `-mesboot0` providers, the
// same way as gcc-core-mesboot0 (which shares this GCC 2.95.3 source) —
// mesboot0_path()/mesboot0_inputs(), `awk` -> gawk-mesboot0, `rm` ->
// coreutils-mesboot0, link_bins_mesboot0, the install-headers-tar pipe -> `cp`
// (td ships no tar), and flex/bison dropped as dead edges (2.95.3 ships its
// pre-generated parsers and #496 keeps them newer than their sources). Per-rung
// cutover for #469; the shared host mechanism goes in the final atomic PR.
pub fn recipe() -> Recipe {
    let path = mesboot0_path();
    let gccdir1 = "{in:gcc-core-mesboot0}/lib/gcc-lib/i686-unknown-linux-gnu/2.95.3";
    let cip = format!("{{in:glibc-mesboot0}}/include:{gccdir1}/include:{{in:mesboot-headers}}/include");
    let lp = format!("{{in:glibc-mesboot0}}/lib:{gccdir1}");
    let gccdir2 = "{out}/lib/gcc-lib/i686-unknown-linux-gnu/2.95.3";
    let mut steps = unpack_into("gcc-mesboot0-source", "{src}");
    steps.push(apply_patch("patch-mesboot", "patch-gcc-boot-2.95.3"));
    // Host-tar-free header install (re #469), identical to gcc-core-mesboot0:
    // gcc-2.95.3 hard-wires INSTALL_HEADERS_DIR to install-headers-tar (a `tar |
    // tar` pipe); td ships no tar executable, so replace it with coreutils-mesboot0
    // `cp -a`. Patched before configure — plain make text, untouched by autoconf.
    steps.push(Step::substitute_text(
        "{src}/gcc/Makefile.in",
        vec![TextEdit::new(
            "\t(cd `pwd`/include ; \\\n\t tar -cf - .; exit 0) | (cd $(libsubdir)/include; tar $(TAROUTOPTS) - )",
            "\tcp -a include/. $(libsubdir)/include",
            1,
        )],
    ));
    steps.push(Step::ToolFarm {
        links: vec![
            ("gcc".into(), "{in:gcc-core-mesboot0}/bin/gcc".into()),
            ("cpp".into(), "{in:gcc-core-mesboot0}/bin/cpp".into()),
            ("make".into(), "{in:make-mesboot0}/bin/make".into()),
            ("patch".into(), "{in:patch-mesboot}/bin/patch".into()),
            ("awk".into(), "{in:gawk-mesboot0}/bin/awk".into()),
        ],
    });
    steps.push(link_bins_mesboot0("binutils-mesboot0"));
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
        Step::run("{src}", &["{in:coreutils-mesboot0}/bin/rm", "-rf", "texinfo"]).env("PATH", &path),
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
        .inputs_owned(mesboot0_inputs(&["patch-gcc-boot-2.95.3"]))
        .steps(steps)
}

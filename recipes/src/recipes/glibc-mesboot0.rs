use crate::ladder::{SH, apply_patch, base_inputs, base_path, link_bins, sed_i, unpack_into};
use crate::types::{Recipe, Step};

// glibc 2.2.5 — bootstrap rung 8 (#378, guix's glibc-mesboot0): the first gcc
// (gcc-core-mesboot0) builds the first real libc against the kernel headers.
// Faithful port of the deleted build_glibc fn: the two boot patches, the
// config.make INSTALL/BASH fixups, shebang rewrite, the seed gcc's cpp on PATH
// (glibc's scripts/cpp does `which cpp`), serial make + install.
pub fn recipe() -> Recipe {
    let path = base_path();
    let gccdir = "{in:gcc-core-mesboot0}/lib/gcc-lib/i686-unknown-linux-gnu/2.95.3";
    let cip = format!("{{in:gcc-core-mesboot0}}/include:{gccdir}/include:{{in:mesboot-headers}}/include");
    let lp = format!("{{in:gcc-core-mesboot0}}/lib:{gccdir}:{{in:tcc}}/lib");
    let cc = "{in:gcc-core-mesboot0}/bin/gcc -D MES_BOOTSTRAP=1 -D BOOTSTRAP_GLIBC=1 -L {src}";
    let cpp = "{in:gcc-core-mesboot0}/bin/gcc -E -D MES_BOOTSTRAP=1 -D BOOTSTRAP_GLIBC=1";
    let mut steps = unpack_into("glibc-mesboot0-source", "{src}");
    steps.push(apply_patch("patch-mesboot", "patch-glibc-boot-2.2.5"));
    steps.push(apply_patch("patch-mesboot", "patch-glibc-bootstrap-system-2.2.5"));
    steps.push(Step::ToolFarm {
        links: vec![
            ("gcc".into(), "{in:gcc-core-mesboot0}/bin/gcc".into()),
            ("cpp".into(), "{in:gcc-core-mesboot0}/bin/cpp".into()),
            ("make".into(), "{in:make-mesboot0}/bin/make".into()),
            ("awk".into(), "{in:gawk}/bin/awk".into()),
        ],
    });
    steps.push(
        link_bins("binutils-mesboot0"),
    );
    steps.push(
        Step::run(
            "{src}",
            &[
                SH,
                "./configure",
                "--disable-shared",
                "--enable-static",
                "--disable-sanity-checks",
                "--build=i686-unknown-linux-gnu",
                "--host=i686-unknown-linux-gnu",
                "--with-headers={in:mesboot-headers}/include",
                "--enable-static-nss",
                "--without-__thread",
                "--without-cvs",
                "--without-gd",
                "--without-tls",
                "--prefix={out}",
            ],
        )
        .env("PATH", &path)
        .env("CONFIG_SHELL", SH)
        .env("SHELL", SH)
        .env("C_INCLUDE_PATH", &cip)
        .env("LIBRARY_PATH", &lp)
        .env("CPP", cpp)
        .env("CC", cc),
    );
    steps.push(Step::Require {
        paths: vec!["{src}/config.make".into()],
        exec: false,
    });
    steps.push(sed_i(
        "s,INSTALL = scripts/,INSTALL = $(..)./scripts/,",
        &["config.make"],
    ));
    steps.push(sed_i(
        "s,^BASH = ,SHELL = {in:bash}/bin/bash\\n         BASH = ,",
        &["config.make"],
    ));
    steps.push(Step::PatchShebangs {
        dir: "{src}".into(),
        shell: SH.into(),
    });
    for target in [None, Some("install")] {
        let ccarg = format!("CC={cc}");
        let mut argv: Vec<&str> = vec![
            "{in:make-mesboot0}/bin/make",
            "SHELL={in:bash}/bin/bash",
            &ccarg,
        ];
        if let Some(t) = target {
            argv.push(t);
        }
        steps.push(
            Step::run("{src}", &argv)
                .env("PATH", &path)
                .env("C_INCLUDE_PATH", &cip)
                .env("LIBRARY_PATH", &lp),
        );
    }
    steps.push(Step::Require {
        paths: vec!["{out}/lib/libc.a".into(), "{out}/lib/crt1.o".into()],
        exec: false,
    });
    Recipe::mesboot("glibc-mesboot0", "2.2.5")
        .source_input("glibc-mesboot0-source")
        .native_inputs(&[
            "mes",
            "tcc",
            "make-mesboot0",
            "patch-mesboot",
            "binutils-mesboot0",
            "gcc-core-mesboot0",
            "mesboot-headers",
        ])
        .inputs_owned(base_inputs(&["patch-glibc-boot-2.2.5", "patch-glibc-bootstrap-system-2.2.5"]))
        .steps(steps)
}

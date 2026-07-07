use crate::ladder::{SH, apply_patch, base_inputs, base_path, unpack_into};
use crate::types::{Recipe, Step};

// GNU Binutils 2.20.1a — bootstrap rung 6 (#378, guix's binutils-mesboot0):
// tcc + the tcc-built make/patch build the first as/ld against the mes libc.
// Faithful port of the deleted build_binutils fn: boot patch via the td-built
// patch rung, CPPFLAGS' MES_BOOTSTRAP defines, AR="tcc -ar", CXX=false,
// RANLIB=true, serial make, install prefix={out}. crt resolves via tcc's baked
// prefix ({in:tcc}/lib — the tcc RECIPE stages crt there at install, retiring
// the ladder's cross-brick out/lib mutation); libc via LIBRARY_PATH; headers
// via C_INCLUDE_PATH (guix's tcc-boot0 search-path setup).
pub fn recipe() -> Recipe {
    let path = base_path();
    let cip = "{in:mes}/include:{in:mes}/include/x86";
    let lp = "{in:tcc}/lib";
    let cc = "CC=tcc -static -D __GLIBC_MINOR__=6 -D MES_BOOTSTRAP=1";
    let mut steps = unpack_into("binutils-mesboot0-source", "{src}");
    steps.push(apply_patch("patch-mesboot", "patch-binutils-boot-2.20.1a"));
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
    steps.push(
        Step::run(
            "{src}",
            &[
                SH,
                "./configure",
                cc,
                "CPPFLAGS=-D __GLIBC_MINOR__=6 -D MES_BOOTSTRAP=1",
                "AR=tcc -ar",
                "CXX=false",
                "RANLIB=true",
                "--disable-nls",
                "--disable-shared",
                "--disable-werror",
                "--build=i686-unknown-linux-gnu",
                "--host=i686-unknown-linux-gnu",
                "--with-sysroot=/",
            ],
        )
        .env("PATH", &path)
        .env("CONFIG_SHELL", SH)
        .env("C_INCLUDE_PATH", cip)
        .env("LIBRARY_PATH", lp),
    );
    for target in [None, Some("install")] {
        let mut argv: Vec<&str> = vec![
            "{in:make-mesboot0}/bin/make",
            "SHELL={in:bash}/bin/bash",
            "CONFIG_SHELL={in:bash}/bin/bash",
        ];
        if let Some(t) = target {
            argv.push(t);
            argv.push("prefix={out}");
        } else {
            argv.extend([cc, "AR=tcc -ar", "CXX=false", "RANLIB=true"]);
        }
        steps.push(
            Step::run("{src}", &argv)
                .env("PATH", &path)
                .env("C_INCLUDE_PATH", cip)
                .env("LIBRARY_PATH", lp),
        );
    }
    steps.push(Step::Require {
        paths: vec!["{out}/bin/as".into(), "{out}/bin/ld".into()],
        exec: true,
    });
    Recipe::mesboot("binutils-mesboot0", "2.20.1a")
        .source_input("binutils-mesboot-source")
        .native_inputs(&["mes", "tcc", "make-mesboot0", "patch-mesboot"])
        .inputs_owned(base_inputs(&["patch-binutils-boot-2.20.1a", "flex", "bison"]))
        .steps(steps)
}

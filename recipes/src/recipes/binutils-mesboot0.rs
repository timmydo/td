use crate::ladder::{SH, apply_patch, mesboot0_inputs, mesboot0_path, unpack_into};
use crate::types::{Recipe, Step};

// GNU Binutils 2.20.1a — bootstrap rung 6 (#378, guix's binutils-mesboot0):
// tcc + the tcc-built make/patch build the first as/ld against the mes libc.
// Faithful port of the deleted build_binutils fn: boot patch via the td-built
// patch rung, CPPFLAGS' MES_BOOTSTRAP defines, AR="tcc -ar", CXX=false,
// RANLIB=true, serial make, install prefix={out}. crt resolves via tcc's baked
// prefix ({in:tcc}/lib — the tcc RECIPE stages crt there at install, retiring
// the ladder's cross-brick out/lib mutation); libc via LIBRARY_PATH; headers
// via C_INCLUDE_PATH (guix's tcc-boot0 search-path setup).
//
// Host-tool ingress closed (re #469): this rung is the FIRST host-tool consumer
// (guix stages host coreutils/sed/grep/gawk/diffutils and this rung farmed host
// `gawk` as `awk`) and the exact recipe the loop's `UNPROVISIONED (69)` gap named
// ("input `coreutils' is neither a td recipe output nor a pinned seed"). It now
// resolves its userland entirely through the td-built `-mesboot0` providers:
// `mesboot0_path()`/`mesboot0_inputs()` supply coreutils/sed/grep/gawk/diffutils
// as `RecipeOutput` edges (built from source under tcc + mes libc), and the `awk`
// ToolFarm points at `gawk-mesboot0`. binutils-2.20.1a builds against exactly
// this tool generation upstream (guix/live-bootstrap), so the `-mesboot0` subset
// is a faithful drop-in. This was the first step of #469's per-rung cutover; the
// host-tool tier (`BASE_TOOLS`/`base_path`/`base_inputs`) has since been deleted,
// closing the ingress.
pub fn recipe() -> Recipe {
    let path = mesboot0_path();
    let cip = "{in:mes}/include:{in:mes}/include/x86";
    let lp = "{in:tcc}/lib";
    let cc = "CC=tcc -static -D __GLIBC_MINOR__=6 -D MES_BOOTSTRAP=1";
    let mut steps = unpack_into("binutils-mesboot0-source", "{src}");
    steps.push(apply_patch("patch-mesboot", "patch-binutils-boot-2.20.1a"));
    // Retarget every `#! /bin/sh` shebang in the tree to the declared shell (the
    // sandbox has no /bin/sh). The recipe's own `configure`/`make` steps name SH
    // explicitly, but `make all` recurses into `binutils/`, whose `configure`
    // runs `AM_PROG_LEX`: with no flex on PATH (flex sits far above this rung on
    // the ladder — depending on it here would be circular), autoconf falls back
    // to the automake `missing` wrapper to fabricate a stub `lex.yy.c`, and it
    // exec's `missing` DIRECTLY — so `missing`'s dead `#! /bin/sh` shebang aborts
    // the sub-configure ("cannot find output from ... missing flex; giving up").
    // This is the same shebang rewrite gcc-core-mesboot0 (rung 7) applies; it was
    // latent until the #469 cutover got this rung past the provenance gate.
    steps.push(Step::PatchShebangs {
        dir: "{src}".into(),
        shell: SH.into(),
    });
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
            ("awk".into(), "{in:gawk-mesboot0}/bin/awk".into()),
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
            "SHELL={in:bash-mesboot}/bin/bash",
            "CONFIG_SHELL={in:bash-mesboot}/bin/bash",
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
        .inputs_owned(mesboot0_inputs(&["patch-binutils-boot-2.20.1a"]))
        .steps(steps)
}

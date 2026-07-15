use crate::ladder::{SH, apply_patch, link_bins_mesboot0, mesboot0_inputs, mesboot0_path, unpack_into};
use crate::types::{Recipe, Step, TextEdit};

// GCC 2.95.3 — bootstrap rung 7 (#378, guix's gcc-core-mesboot0): tcc + the
// binutils-mesboot0 as/ld build the first gcc against the mes libc. Faithful
// port of the deleted build_gcc fn: boot patch, config.cache float-format
// hint, texinfo stubs, shebang rewrite, BOOT_LDFLAGS at tcc's crt dir, and the
// install2 ar-assembly of libgcc.a/libc.a into gcc-lib (+ crt/libgcc2.a copies)
// so the compiler can link on its own.
//
// Host-tool ingress closed (re #469): cut over to the td-built `-mesboot0`
// providers — `mesboot0_path()`/`mesboot0_inputs()` supply coreutils/sed/grep/
// gawk/diffutils, the `awk` ToolFarm points at `gawk-mesboot0`, `rm` and the
// binutils `link_bins_mesboot0` farm use `coreutils-mesboot0`. The one remaining
// host executable this rung reached for was `tar` (gcc's install-headers-tar);
// it is replaced below with a coreutils-mesboot0 `cp' so nothing outside the
// td store is invoked.
//
// No flex or bison: gcc-2.95.3 SHIPS its pre-generated bison parsers
// (c-parse.c/c-parse.h from c-parse.y, cexp.c from cexp.y) and its gperf table
// (c-gperf.h). The Makefile's `$(BISON)`/`$(GPERF)` rules fire only if a source
// is NEWER than its generated file. td's unpacker now preserves tar mtimes (as
// GNU tar/guix do), so the shipped generated files stay newer than their
// sources and make treats them as up-to-date — bison, flex (no `.l` sources at
// all), and gperf are all dead edges here, exactly as upstream guix/live-boot
// build this rung with none of them on PATH. Before the mtime fix every
// extracted file got a "now" mtime in extraction order, which put c-parse.y
// after c-parse.c and spuriously demanded bison; that was the sole reason this
// rung named host flex/bison, and it is gone. Per-rung cutover for #469;
// `BASE_TOOLS`/`base_path`/`base_inputs`/`link_bins` are deleted in the final
// atomic PR once every rung has moved off them.
pub fn recipe() -> Recipe {
    let path = mesboot0_path();
    let cip = "{in:mes}/include:{in:mes}/include/x86";
    let lp = "{in:tcc}/lib";
    let gccdir = "{out}/lib/gcc-lib/i686-unknown-linux-gnu/2.95.3";
    let mut steps = unpack_into("gcc-core-mesboot0-source", "{src}");
    steps.push(apply_patch("patch-mesboot", "patch-gcc-boot-2.95.3"));
    // Host-tar-free header install (re #469). gcc-2.95.3 on i686-linux hard-wires
    // INSTALL_HEADERS_DIR to install-headers-tar, whose recipe copies the built
    // gcc/include tree into $(libsubdir)/include with a `tar -cf - . | tar -xf -'
    // pipe (the only other method, install-headers-cpio, needs cpio). td ships no
    // tar or cpio executable: the control-plane builder unpacks sources with its
    // own in-process tar (builder/src/tar.rs), so the build sandbox never had a
    // tar, and the historical green build got one only from the ambient host PATH
    // #469 removes. Replace the tar pipe with `cp -a include/. $(libsubdir)/include'
    // using the td-built coreutils-mesboot0 `cp' (already on mesboot0_path, already
    // used here for `rm'); `cp -a' copies the same tree and preserves the include
    // symlinks the rule's own follow-up fix-symlinks step expects. Patched in
    // Makefile.in BEFORE configure so config.status (the now-flush-fixed
    // sed-mesboot0) copies the cp rule verbatim into gcc/Makefile — $(libsubdir)
    // and the rule name are plain make text, untouched by autoconf @-substitution.
    steps.push(Step::substitute_text(
        "{src}/gcc/Makefile.in",
        vec![TextEdit::new(
            "\t(cd `pwd`/include ; \\\n\t tar -cf - .; exit 0) | (cd $(libsubdir)/include; tar $(TAROUTOPTS) - )",
            "\tcp -a include/. $(libsubdir)/include",
            1,
        )],
    ));
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
    // binutils' whole bin dir onto the farm (as/ld/ar/ranlib/nm/strip/…).
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
                "SHELL={in:bash-mesboot}/bin/bash",
                "CONFIG_SHELL={in:bash-mesboot}/bin/bash",
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
    steps.push(Step::MkDir { path: gccdir.into() });
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
        .source_input("gcc-core-source")
        .native_inputs(&[
            "mes",
            "tcc",
            "make-mesboot0",
            "patch-mesboot",
            "binutils-mesboot0",
        ])
        .inputs_owned(mesboot0_inputs(&["patch-gcc-boot-2.95.3"]))
        .steps(steps)
}

use crate::ladder::{
    gcc14_configure_fixups, gcc14_libstdcxx_stamp_fixups, gcc_disable_selftest,
    gcc_install_headers_without_tar, libtool_extract_without_find, mesboot0_inputs, mesboot0_path,
    unpack_into, unpack_keep_top, SH,
};
use crate::types::{Recipe, Step};

// GCC 14.3.0, NATIVE x86_64 (x86_64-toolchain rung X2, the port of the shell
// build_gcc_x86_64_native): --build=--host=--target=x86_64-pc-linux-gnu, so
// gcc/cc1/g++ are themselves ELF64 x86_64 binaries that run natively (host ==
// target), NOT the i686 CROSS gcc that emits x86_64. Built STATIC (like the i686
// gcc-14 rung) by the CROSS gcc stage2 (an i686 binary) vs the /td/store x86_64
// glibc 2.41 — the architectural self-hosting rung (a from-source gcc-rebuilds-gcc
// bootstrap is rung X3, not this). as/ld = the freshly built sibling native
// binutils (--with-as/--with-ld); gmp/mpfr/mpc in-tree; a build sysroot assembled
// from the x86_64 glibc 2.41 + kernel UAPI headers, with the glibc GNU ld scripts
// relocated to bare names so the fully-static host link resolves libm.a's GROUP.
// --prefix=/td/store/gcc-14.3.0-x86_64-native + DESTDIR={out}/stage. native_inputs:
// gcc-x86-64-stage2 (the cross builder gcc/g++), binutils-x86-64-native (the native
// as/ld), binutils-x86-64 (the cross as/ld the builder gcc resolves absolutely),
// glibc-x86-64 (the x86_64 libc + the sysroot source).
// Host-free build tools: mesboot0 + make-mesboot; flex/bison/m4 dead (gcc-14-source). re #469.
pub fn recipe() -> Recipe {
    let xgcc =
        "{in:gcc-x86-64-stage2}/stage/td/store/gcc-14.3.0-x86_64/bin/x86_64-pc-linux-gnu-gcc";
    let xgpp =
        "{in:gcc-x86-64-stage2}/stage/td/store/gcc-14.3.0-x86_64/bin/x86_64-pc-linux-gnu-g++";
    let xglibc = "{in:glibc-x86-64}/stage/td/store/glibc-2.41-x86_64";
    let nbin = "{in:binutils-x86-64-native}/bin";
    let path = format!("{nbin}:{{in:binutils-x86-64}}/bin:{}", mesboot0_path());
    // headers reach the compiler via the wrapper's -idirafter (NOT C_INCLUDE_PATH —
    // that breaks libstdc++'s <cstdlib> #include_next); CIP carries only the in-tree
    // mpfr src, LIBRARY_PATH the relocated sysroot lib.
    let cip = "{src}/mpfr/src";
    let lp = "{root}/sysroot/lib";
    let mut steps = unpack_into("gcc-x86-64-native-source", "{src}");
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
    // build sysroot: x86_64 glibc 2.41 headers + kernel UAPI headers (overlay) into
    // include, glibc libs into lib. --with-native-system-header-dir=/include reads
    // {sysroot}/include; the static host link reads {sysroot}/lib.
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
    // relocate glibc's GNU ld scripts to bare names (ld finds the members via -B):
    // libc.so/libm.so AND libm.a — a fully-static host link pulls libm.a, whose
    // GROUP script else points at the absolute /td/store configure prefix (which
    // is not the input-addressed store path here). GUARDED (existence + first-80-bytes
    // "GNU ld script"), mirroring the ported Rust relocate_ld_scripts, so a pin where a
    // name is absent or a real binary archive is skipped rather than errored/corrupted
    // (re #401, the general glob+guard for the glibc-x86-64/glibc-241 recipes too).
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
        .env("PATH", &mesboot0_path()),
    );
    // -shared-aware STATIC wrappers (port of _mk_native_static_wrapper): -static for
    // executables/conftests, DROPPED when the link is -shared; -idirafter (not
    // -isystem) so libstdc++'s <cstdlib> #include_next resolves after gcc's own C++
    // dirs; -B at the relocated sysroot lib.
    steps.push(Step::WriteFile {
        path: "{root}/wb/gcc".into(),
        content: format!(
            "#!{SH}\n\
             for a in \"$@\"; do case \"$a\" in -shared) exec \"{xgcc}\" -idirafter {{root}}/sysroot/include -B{{root}}/sysroot/lib \"$@\";; esac; done\n\
             exec \"{xgcc}\" -static -idirafter {{root}}/sysroot/include -B{{root}}/sysroot/lib \"$@\"\n"
        ),
        exec: true,
    });
    steps.push(Step::WriteFile {
        path: "{root}/wb/g++".into(),
        content: format!(
            "#!{SH}\n\
             for a in \"$@\"; do case \"$a\" in -shared) exec \"{xgpp}\" -idirafter {{root}}/sysroot/include -B{{root}}/sysroot/lib \"$@\";; esac; done\n\
             exec \"{xgpp}\" -static -idirafter {{root}}/sysroot/include -B{{root}}/sysroot/lib \"$@\"\n"
        ),
        exec: true,
    });
    steps.push(Step::ToolFarm {
        links: vec![
            ("awk".into(), "{in:gawk-mesboot}/bin/gawk".into()),
            ("gawk".into(), "{in:gawk-mesboot}/bin/gawk".into()),
            ("make".into(), "{in:make-mesboot}/bin/make".into()),
        ],
    });
    steps.push(Step::PatchShebangs {
        dir: "{src}".into(),
        shell: SH.into(),
    });
    // Same bash-mesboot configure fixups as gcc-14, plus the libtool find fix so
    // this stage's native x86_64 libstdc++.a is assembled complete (re #469).
    steps.extend(gcc14_configure_fixups());
    steps.push(gcc_disable_selftest());
    steps.push(gcc_install_headers_without_tar());
    steps.push(libtool_extract_without_find("{src}/ltmain.sh"));
    steps.push(gcc14_libstdcxx_stamp_fixups());
    steps.push(Step::MkDir {
        path: "{src}/bld".into(),
    });
    steps.push(
        Step::run(
            "{src}/bld",
            &[
                SH,
                "../configure",
                "--prefix=/td/store/gcc-14.3.0-x86_64-native",
                "--build=x86_64-pc-linux-gnu",
                "--host=x86_64-pc-linux-gnu",
                "--target=x86_64-pc-linux-gnu",
                &format!("--with-as={nbin}/as"),
                &format!("--with-ld={nbin}/ld"),
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
                "{in:make-mesboot}/bin/make",
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
                "{in:make-mesboot}/bin/make",
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
            "{out}/stage/td/store/gcc-14.3.0-x86_64-native/bin/gcc".into(),
            "{out}/stage/td/store/gcc-14.3.0-x86_64-native/bin/g++".into(),
        ],
        exec: true,
    });
    Recipe::mesboot("gcc-x86-64-native", "14.3.0")
        .source_input("gcc-14-source")
        .native_inputs(&[
            "gcc-x86-64-stage2",
            "binutils-x86-64-native",
            "binutils-x86-64",
            "glibc-x86-64",
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

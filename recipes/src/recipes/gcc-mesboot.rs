use crate::ladder::{SH, link_bins, mesboot0_inputs, mesboot0_path, unpack_into, unpack_keep_top};
use crate::types::{Recipe, Step, TextEdit};

// GCC 4.9.4 — rung 16 (#378, guix's gcc-mesboot): gcc-mesboot1 (4.6.4 c,c++) +
// binutils-mesboot + the static glibc 2.16.0 build the final mesboot gcc.
// Static divergence from guix (whose shared glibc allows a dynamic link): the
// configure LINK test must be static (LDFLAGS), CC stays clean of -static/-B
// (autoconf stderr poisoning), and CC_FOR_BUILD links static to RUN its build
// tools. Out-of-tree bld/ subdir; no boot patch (guix deletes that phase).
pub fn recipe() -> Recipe {
    let path = format!("{{in:gcc-mesboot1}}/bin:{}", mesboot0_path());
    let cip = "{in:gcc-mesboot1}/lib/gcc/i686-unknown-linux-gnu/4.6.4/include:{root}/kh:{in:glibc-mesboot}/include:{src}/mpfr/src";
    let lp = "{in:glibc-mesboot}/lib:{in:gcc-mesboot1}/lib";
    let ldf = "-static -B{in:glibc-mesboot}/lib";
    let mut steps = unpack_into("gcc-mesboot-source", "{src}");
    for t in ["gmp", "mpfr", "mpc"] {
        steps.extend(unpack_keep_top(t, "{src}"));
    }
    steps.push(Step::Symlink {
        target: "gmp-4.3.2".into(),
        link: "{src}/gmp".into(),
    });
    steps.push(Step::Symlink {
        target: "mpfr-2.4.2".into(),
        link: "{src}/mpfr".into(),
    });
    steps.push(Step::Symlink {
        target: "mpc-1.0.3".into(),
        link: "{src}/mpc".into(),
    });
    steps.extend(unpack_keep_top("linux-headers", "{root}/kh"));
    steps.push(Step::ToolFarm {
        links: vec![
            ("cpp".into(), "{in:gcc-mesboot1}/bin/cpp".into()),
            ("make".into(), "{in:make-mesboot}/bin/make".into()),
            ("patch".into(), "{in:patch-mesboot}/bin/patch".into()),
            // awk regenerates gcc/options.c via optc-gen.awk. gawk-mesboot0
            // (GNU Awk 3.0.4) is tcc-built and miscompiles its double->string
            // path for negatives (tcc 64-bit fold bug, #491), writing the 354
            // non-negatable options' neg_index of -1 as 0 -- a self-cycle that
            // makes cc1's cancel_option() recurse until the stack overflows
            // (the #515 gcc-mesboot1 segfault; GCC 4.9.4 has the identical
            // generator, #517). Use gawk-mesboot 3.1.8 instead: it is built by
            // gcc-mesboot1, not tcc, so it lacks the fold bug, and it is already
            // in this closure (glibc-mesboot builds with it).
            //
            // BOTH `awk` and `gawk` must point at it: GCC's configure resolves
            // AWK via autoconf AC_PROG_AWK = AC_CHECK_PROGS([AWK],[gawk mawk
            // nawk awk]), which PREFERS `gawk`, and mesboot0_path() still carries
            // {in:gawk-mesboot0}/bin, so a `gawk`-only-missing ToolFarm would let
            // the options.c rule run under the buggy 3.0.4 anyway. glibc-mesboot
            // links both names for exactly this reason; mirror it.
            ("awk".into(), "{in:gawk-mesboot}/bin/gawk".into()),
            ("gawk".into(), "{in:gawk-mesboot}/bin/gawk".into()),
        ],
    });
    steps.push(link_bins("binutils-mesboot"));
    steps.push(Step::PatchShebangs {
        dir: "{src}".into(),
        shell: SH.into(),
    });
    // Close the `tar` host-tool ingress in `make install`: config.build has no
    // i686-*-linux-gnu arm, so INSTALL_HEADERS_DIR defaults to
    // install-headers-tar (a `tar` pipeline); mesboot0 ships no tar. Repoint it
    // at install-headers-cp (cp -p -r, coreutils-mesboot0) before configure —
    // same fix as gcc-mesboot1.
    steps.push(Step::substitute_text(
        "{src}/gcc/Makefile.in",
        vec![TextEdit::new(
            "INSTALL_HEADERS_DIR = @build_install_headers_dir@",
            "INSTALL_HEADERS_DIR = install-headers-cp",
            1,
        )],
    ));
    steps.push(Step::MkDir {
        path: "{src}/bld".into(),
    });
    steps.push(
        Step::run(
            "{src}/bld",
            &[
                SH,
                "../configure",
                "--prefix={out}",
                "--build=i686-unknown-linux-gnu",
                "--host=i686-unknown-linux-gnu",
                "--with-host-libstdcxx=-lsupc++",
                "--with-native-system-header-dir={in:glibc-mesboot}/include",
                "--with-build-sysroot={in:glibc-mesboot}/include",
                "--disable-bootstrap",
                "--disable-decimal-float",
                "--disable-libatomic",
                "--disable-libcilkrts",
                "--disable-libgomp",
                "--disable-libitm",
                "--disable-libmudflap",
                "--disable-libquadmath",
                "--disable-libsanitizer",
                "--disable-libssp",
                "--disable-libvtv",
                "--disable-lto",
                "--disable-lto-plugin",
                "--disable-multilib",
                "--disable-plugin",
                "--enable-languages=c,c++",
                "--enable-static",
                "--disable-shared",
                "--enable-threads=single",
                "--disable-libstdcxx-pch",
                "--disable-build-with-cxx",
            ],
        )
        .env("PATH", &path)
        .env("CONFIG_SHELL", SH)
        .env("CC", "{in:gcc-mesboot1}/bin/gcc")
        .env("CPP", "{in:gcc-mesboot1}/bin/gcc -E")
        .env("CC_FOR_BUILD", "{in:gcc-mesboot1}/bin/gcc -static")
        .env("C_INCLUDE_PATH", cip)
        .env("CPLUS_INCLUDE_PATH", cip)
        .env("LIBRARY_PATH", lp)
        .env("LDFLAGS", ldf),
    );
    steps.push(
        Step::run(
            "{src}/bld",
            &[
                "{in:make-mesboot}/bin/make",
                "SHELL={in:bash-mesboot}/bin/bash",
                "CONFIG_SHELL={in:bash-mesboot}/bin/bash",
                "CC_FOR_BUILD={in:gcc-mesboot1}/bin/gcc -static",
                "MAKEINFO=true",
                &format!("LDFLAGS={ldf}"),
                &format!("LDFLAGS_FOR_TARGET={ldf}"),
            ],
        )
        .env("PATH", &path)
        .env("CONFIG_SHELL", SH)
        .env("CC_FOR_BUILD", "{in:gcc-mesboot1}/bin/gcc -static")
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
            ],
        )
        .env("PATH", &path)
        .env("CONFIG_SHELL", SH)
        .env("C_INCLUDE_PATH", cip)
        .env("CPLUS_INCLUDE_PATH", cip)
        .env("LIBRARY_PATH", lp),
    );
    steps.push(Step::Require {
        paths: vec!["{out}/bin/gcc".into(), "{out}/bin/g++".into()],
        exec: true,
    });
    Recipe::mesboot("gcc-mesboot", "4.9.4")
        .source_input("gcc-494-source")
        .native_inputs(&[
            "make-mesboot",
            "patch-mesboot",
            "binutils-mesboot",
            "gcc-mesboot1",
            "glibc-mesboot",
            "gawk-mesboot",
        ])
        .inputs_owned(mesboot0_inputs(&["gmp", "mpfr", "mpc", "linux-headers"]))
        .steps(steps)
}

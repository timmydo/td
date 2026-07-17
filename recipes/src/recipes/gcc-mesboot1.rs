use crate::ladder::{SH, apply_patch, link_bins, mesboot0_inputs, mesboot0_path, unpack_into, unpack_keep_top};
use crate::types::{Recipe, Step, TextEdit};

// GCC 4.6.4 (c,c++) — rung 12 (#378, guix's gcc-mesboot1): gcc-mesboot0 builds
// the first C++-capable gcc. The g++ front-end tarball overlays the core tree;
// gmp/mpfr/mpc unpack in-tree with version-free symlinks; LDFLAGS are static at
// glibc-mesboot0's lib; CPLUS_INCLUDE_PATH mirrors C_INCLUDE_PATH (guix's
// setenv) so libstdc++ finds the C headers. gcc-mesboot0's bin leads PATH.
//
// Host-tool ingress closed (re #469): cut over to the `-mesboot0` providers —
// mesboot0_path()/mesboot0_inputs(), `awk` -> gawk-mesboot0, the binutils-mesboot1
// link_bins farm, and flex/bison dropped as dead edges (4.6.4 ships its
// pre-generated parsers and #496 keeps them newer than their sources). The tar
// ingress in `make install` (config.build defaults i686-linux to
// install-headers-tar) is closed by repointing gcc/Makefile.in's
// INSTALL_HEADERS_DIR at install-headers-cp before configure (see below).
// Per-rung cutover for #469; the shared host mechanism goes in the final atomic PR.
pub fn recipe() -> Recipe {
    let path = format!("{{in:gcc-mesboot0}}/bin:{}", mesboot0_path());
    let gccdir1 = "{in:gcc-mesboot0}/lib/gcc-lib/i686-unknown-linux-gnu/2.95.3";
    let cip = format!("{gccdir1}/include:{{root}}/kh:{{in:glibc-mesboot0}}/include:{{src}}/mpfr/src");
    let lp = "{in:glibc-mesboot0}/lib:{in:gcc-mesboot0}/lib";
    let ldf = "-static -B{in:glibc-mesboot0}/lib";
    let mut steps = unpack_into("gcc-mesboot1-source", "{src}");
    // the g++ front-end OVERLAY into the same tree (strip-top MERGES, the
    // engine unpack's `tar --strip-components=1` semantics)
    steps.extend(unpack_into("gcc-464-gpp", "{src}"));
    steps.push(apply_patch("patch-mesboot", "patch-gcc-boot-4.6.4"));
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
            ("cpp".into(), "{in:gcc-mesboot0}/bin/cpp".into()),
            ("make".into(), "{in:make-mesboot}/bin/make".into()),
            ("patch".into(), "{in:patch-mesboot}/bin/patch".into()),
            ("awk".into(), "{in:gawk-mesboot0}/bin/awk".into()),
        ],
    });
    steps.push(link_bins("binutils-mesboot1"));
    steps.push(Step::PatchShebangs {
        dir: "{src}".into(),
        shell: SH.into(),
    });
    // Close the `tar` host-tool ingress in `make install`: config.build has no
    // i686-*-linux-gnu arm, so @build_install_headers_dir@ falls through to its
    // default install-headers-tar, whose recipe pipes headers through `tar` on a
    // fatal (tab-prefixed) line. mesboot0 ships no tar. Repoint INSTALL_HEADERS_DIR
    // at GCC's own install-headers-cp target (cp -p -r, coreutils-mesboot0). A
    // command-line make override cannot do this: GCC 4.6.4's top-level Makefile
    // sets `MAKEOVERRIDES=` ("Don't pass command-line variables to submakes"), so
    // the value must be baked into gcc/Makefile.in before configure generates the
    // Makefile. Patched before configure — plain autoconf text.
    steps.push(Step::substitute_text(
        "{src}/gcc/Makefile.in",
        vec![TextEdit::new(
            "INSTALL_HEADERS_DIR = @build_install_headers_dir@",
            "INSTALL_HEADERS_DIR = install-headers-cp",
            1,
        )],
    ));
    // Detect the C++ front end without a non-terminal glob. GCC 4.6.4 discovers
    // language fragments by globbing `.../*/config-lang.in` (a `*` in a
    // NON-terminal path component) in BOTH the top-level configure (two scan
    // loops) and the gcc/ subdir configure (run from `make`, one scan loop).
    // bash-mesboot (bash 2.05b on mes libc) expands terminal-component globs but
    // returns a non-terminal one unexpanded, so the loops match no fragments: the
    // top level drops every non-C language ("Supported languages are: c"), and
    // gcc/configure would silently omit the C++ makefile hookup (no cc1plus/g++).
    // Pre-expand every such glob to the tree's actual fragments — the pinned
    // core+g++ 4.6.4 source has exactly cp and lto — so language detection never
    // depends on the glob (matching a working shell's expansion verbatim).
    steps.push(Step::substitute_text(
        "{src}/configure",
        vec![TextEdit::new(
            "${srcdir}/gcc/*/config-lang.in",
            "${srcdir}/gcc/cp/config-lang.in ${srcdir}/gcc/lto/config-lang.in",
            2,
        )],
    ));
    steps.push(Step::substitute_text(
        "{src}/gcc/configure",
        vec![TextEdit::new(
            "${srcdir}/*/config-lang.in",
            "${srcdir}/cp/config-lang.in ${srcdir}/lto/config-lang.in",
            2,
        )],
    ));
    // Run the dependency-style probe without `env`. The mesboot userland ships no
    // `env` binary — coreutils-mesboot0 builds only live-bootstrap's curated
    // 61-binary subset (see coreutils-mesboot0.rs), which omits it, and `env` is
    // not a bash builtin. GCC 4.6.4's libcpp is the ONE subdir whose automake
    // dependency-style probe was generated from the old config/depstand.m4
    // ZW_PROG_COMPILER_DEPENDENCIES macro, which runs each depmode as `env $depcmd`
    // (every other subdir — zlib/intl/gmp/mpfr/mpc/lto-plugin — inlines the VAR=VAL
    // pairs as shell assignment prefixes and needs no `env`). With no `env` on PATH
    // every depmode exits 127, so the probe finds none and the macro's
    // unconditional `test x$type = xnone` aborts with "no usable dependency style
    // found" — and unlike stock automake this variant has no
    // --disable-dependency-tracking guard, so that flag cannot skip it. `eval`
    // re-parses the $depcmd string so its leading VAR=VAL become real assignment
    // prefixes (the exact effect `env` provided, using only a POSIX builtin), after
    // which depmode `gcc` is selected just as in the other subdirs. Count is 2:
    // libcpp's configure runs this probe for BOTH its C (am_cv_CC_dependencies)
    // and C++ (am_cv_CXX_dependencies) compilers, each with its own `env $depcmd`.
    // Both are gated only by `test -f "$am_depcomp"` (depcomp is present) and both
    // abort unconditionally on no style found, so both sites are load-bearing —
    // `--disable-build-with-cxx` governs GCC's own build, not this automake probe.
    steps.push(Step::substitute_text(
        "{src}/libcpp/configure",
        vec![TextEdit::new("env $depcmd", "eval \"$depcmd\"", 2)],
    ));
    // Emit cl_options[].neg_index without asking gawk-mesboot0 to stringify a
    // negative (fixes #515). gawk-mesboot0 is gawk 3.0.4 built by tcc, and tcc's
    // general 64-bit constant fold bug (#491, the same class the HIDDEND_LL
    // rewrite works around in tcc.rs) miscompiles gawk's number->string path:
    // gawk stores every scalar as a C double, and converting a NEGATIVE double to
    // its string form yields garbage (`printf "%s",-1` -> "0"; `int(-1)` -> 998),
    // while the numeric value and integer comparisons stay correct. optc-gen.awk
    // prints each option's neg_index with `printf(" %d,\n", idx)`; the 354
    // non-negatable options carry idx=-1, which the buggy gawk writes as `0`. A
    // neg_index of 0 (or self) turns the option into a self-cancelling cycle, so
    // cc1's cancel_option() — which walks the neg_index chain to prune cancelled
    // options — recurses forever and overflows the stack on EVERY real compile
    // (xgcc always passes -dumpbase/-auxbase, which drives prune_options ->
    // cancel_option). That was the segfault: cc1 ran standalone but died the
    // instant the driver invoked it. Emit the literal `-1` for the only-negative
    // case so the value never round-trips through gawk's broken double->string;
    // the `idx < 0` test uses the intact numeric value, and non-negative indices
    // keep the working `%d`. Verified: the patched script under gawk-mesboot0
    // reproduces host awk's neg_index column byte-for-byte (354 `-1` entries).
    // A proper root fix is tcc's #491 fold bug; that is a separate, larger change.
    steps.push(Step::substitute_text(
        "{src}/gcc/optc-gen.awk",
        vec![TextEdit::new(
            "printf(\" %d,\\n\", idx)",
            "if (idx < 0) printf(\" -1,\\n\"); else printf(\" %d,\\n\", idx)",
            1,
        )],
    ));
    steps.push(
        Step::run(
            "{src}",
            &[
                SH,
                "./configure",
                "--prefix={out}",
                "--build=i686-unknown-linux-gnu",
                "--host=i686-unknown-linux-gnu",
                "--with-native-system-header-dir={in:glibc-mesboot0}/include",
                "--with-build-sysroot={in:glibc-mesboot0}/include",
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
                "--disable-threads",
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
        .env("CC", "{in:gcc-mesboot0}/bin/gcc")
        .env("CPP", "{in:gcc-mesboot0}/bin/gcc -E")
        .env("C_INCLUDE_PATH", &cip)
        .env("CPLUS_INCLUDE_PATH", &cip)
        .env("LIBRARY_PATH", lp),
    );
    steps.push(
        Step::run(
            "{src}",
            &[
                "{in:make-mesboot}/bin/make",
                "SHELL={in:bash-mesboot}/bin/bash",
                "CONFIG_SHELL={in:bash-mesboot}/bin/bash",
                "MAKEINFO=true",
                &format!("LDFLAGS={ldf}"),
                &format!("LDFLAGS_FOR_TARGET={ldf}"),
            ],
        )
        .env("PATH", &path)
        .env("CONFIG_SHELL", SH)
        .env("C_INCLUDE_PATH", &cip)
        .env("CPLUS_INCLUDE_PATH", &cip)
        .env("LIBRARY_PATH", lp),
    );
    steps.push(
        Step::run(
            "{src}",
            &[
                "{in:make-mesboot}/bin/make",
                "SHELL={in:bash-mesboot}/bin/bash",
                "MAKEINFO=true",
                "install",
            ],
        )
        .env("PATH", &path)
        .env("CONFIG_SHELL", SH)
        .env("C_INCLUDE_PATH", &cip)
        .env("CPLUS_INCLUDE_PATH", &cip)
        .env("LIBRARY_PATH", lp),
    );
    steps.push(Step::Require {
        paths: vec!["{out}/bin/gcc".into(), "{out}/bin/g++".into()],
        exec: true,
    });
    Recipe::mesboot("gcc-mesboot1", "4.6.4")
        .source_input("gcc-464-core")
        .native_inputs(&[
            "make-mesboot0",
            "patch-mesboot",
            "binutils-mesboot1",
            "gcc-mesboot0",
            "glibc-mesboot0",
            "make-mesboot",
        ])
        .inputs_owned(mesboot0_inputs(&["gcc-464-gpp", "patch-gcc-boot-4.6.4", "gmp", "mpfr", "mpc", "linux-headers"]))
        .steps(steps)
}

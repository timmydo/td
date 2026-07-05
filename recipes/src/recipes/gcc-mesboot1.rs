use crate::ladder::{SH, apply_patch, base_inputs, base_path, link_bins, unpack_into, unpack_keep_top};
use crate::types::{Recipe, Step};

// GCC 4.6.4 (c,c++) — rung 12 (#378, guix's gcc-mesboot1): gcc-mesboot0 builds
// the first C++-capable gcc. The g++ front-end tarball overlays the core tree;
// gmp/mpfr/mpc unpack in-tree with version-free symlinks; LDFLAGS are static at
// glibc-mesboot0's lib; CPLUS_INCLUDE_PATH mirrors C_INCLUDE_PATH (guix's
// setenv) so libstdc++ finds the C headers. gcc-mesboot0's bin leads PATH.
pub fn recipe() -> Recipe {
    let path = format!("{{in:gcc-mesboot0}}/bin:{}", base_path());
    let gccdir1 = "{in:gcc-mesboot0}/lib/gcc-lib/i686-unknown-linux-gnu/2.95.3";
    let cip = format!("{gccdir1}/include:{{root}}/kh:{{in:glibc-mesboot0}}/include:{{src}}/mpfr/src");
    let lp = "{in:glibc-mesboot0}/lib:{in:gcc-mesboot0}/lib";
    let ldf = "-static -B{in:glibc-mesboot0}/lib";
    let mut steps = unpack_into("gcc-mesboot1-source", "{src}");
    // the g++ front-end OVERLAY into the same tree (strip-components=1, same dir)
    steps.push(
        Step::run(
            "{src}",
            &[
                "{in:tar}/bin/tar",
                "-xf",
                "{in:gcc-464-gpp}",
                "--strip-components=1",
            ],
        )
        .env("PATH", &base_path()),
    );
    steps.push(apply_patch("patch-mesboot", "patch-gcc-boot-4.6.4"));
    for t in ["gmp", "mpfr", "mpc"] {
        steps.push(
            Step::run("{src}", &["{in:tar}/bin/tar", "-xf", &format!("{{in:{t}}}")])
                .env("PATH", &base_path()),
        );
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
            ("awk".into(), "{in:gawk}/bin/awk".into()),
            ("flex".into(), "{in:flex}/bin/flex".into()),
            ("lex".into(), "{in:flex}/bin/flex".into()),
            ("bison".into(), "{in:bison}/bin/bison".into()),
            ("yacc".into(), "{in:bison}/bin/bison".into()),
        ],
    });
    steps.push(
        link_bins("binutils-mesboot1"),
    );
    steps.push(Step::PatchShebangs {
        dir: "{src}".into(),
        shell: SH.into(),
    });
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
                "SHELL={in:bash}/bin/bash",
                "CONFIG_SHELL={in:bash}/bin/bash",
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
                "SHELL={in:bash}/bin/bash",
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
        .native_inputs(&[
            "make-mesboot0",
            "patch-mesboot",
            "binutils-mesboot1",
            "gcc-mesboot0",
            "glibc-mesboot0",
            "make-mesboot",
        ])
        .inputs_owned(base_inputs(&["gcc-464-gpp", "patch-gcc-boot-4.6.4", "gmp", "mpfr", "mpc", "linux-headers", "flex", "bison"]))
        .steps(steps)
}

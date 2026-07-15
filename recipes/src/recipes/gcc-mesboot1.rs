use crate::ladder::{SH, apply_patch, link_bins_mesboot0, mesboot0_inputs, mesboot0_path, unpack_into, unpack_keep_top};
use crate::types::{Recipe, Step};

// GCC 4.6.4 (c,c++) — rung 12 (#378, guix's gcc-mesboot1): gcc-mesboot0 builds
// the first C++-capable gcc. The g++ front-end tarball overlays the core tree;
// gmp/mpfr/mpc unpack in-tree with version-free symlinks; LDFLAGS are static at
// glibc-mesboot0's lib; CPLUS_INCLUDE_PATH mirrors C_INCLUDE_PATH (guix's
// setenv) so libstdc++ finds the C headers. gcc-mesboot0's bin leads PATH.
//
// Host-tool ingress closed (re #469): cut over to the `-mesboot0` providers —
// mesboot0_path()/mesboot0_inputs(), `awk` -> gawk-mesboot0, the binutils-mesboot1
// link_bins_mesboot0 farm, and flex/bison dropped as dead edges (4.6.4 ships its
// pre-generated parsers and #496 keeps them newer than their sources). The tar
// ingress in `make install` (config.build defaults i686-linux to
// install-headers-tar) is closed with INSTALL_HEADERS_DIR=install-headers-cp.
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
    steps.push(link_bins_mesboot0("binutils-mesboot1"));
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
                // config.build has no i686-linux arm, so build_install_headers_dir
                // defaults to install-headers-tar, which pipes headers through `tar`
                // (a fatal, tab-prefixed recipe line). mesboot0 ships no tar; override
                // to install-headers-cp, which copies with `cp` (coreutils-mesboot0).
                "INSTALL_HEADERS_DIR=install-headers-cp",
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

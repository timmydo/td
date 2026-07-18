use crate::ladder::{
    gcc14_configure_fixups, gcc14_libstdcxx_stamp_fixups, gcc_disable_selftest,
    gcc_install_headers_without_tar, libtool_extract_without_find, link_bins, mesboot0_inputs,
    mesboot0_path, unpack_into, unpack_keep_top, SH,
};
use crate::types::{Recipe, Step};

// GCC 14.3.0 — the modern i686 compiler: gcc-10-bridge (10.5.0) + binutils
// 2.44 + the STATIC glibc 2.16.0 build the modern gcc,
// with gmp-6.3.0/mpfr-4.2.1/mpc-1.3.1 in-tree. Built STATIC via single-token
// wrapper scripts (gcc derives CC_FOR_BUILD from CC and strips trailing flags,
// so a bare `gcc -static …` would come apart — the deleted fn's proven trick).
// --prefix=/td/store/gcc-14.3.0 + DESTDIR={out}/stage: the host-consumable
// stage shape the chain tail reads. make-mesboot -j{jobs} (the modern rungs
// parallelize; the mesboot base stays serial). Host-free, re #469.
pub fn recipe() -> Recipe {
    let path = format!(
        "{{in:gcc-10-bridge}}/bin:{{in:binutils-244}}/bin:{}",
        mesboot0_path()
    );
    // Keep libc and kernel headers after GCC's C++ directories: putting them in
    // CPLUS_INCLUDE_PATH makes them precede <cstdlib>, so its #include_next
    // cannot find stdlib.h. The wrappers add those system directories with
    // -idirafter; CIP is only for the in-tree MPFR build helpers.
    let cip = "{src}/mpfr/src";
    let lp = "{in:glibc-mesboot}/lib:{in:gcc-10-bridge}/lib";
    let ldf = "-static -B{in:glibc-mesboot}/lib";
    let mut steps = unpack_into("gcc-14-source", "{src}");
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
    steps.extend(unpack_keep_top("linux-headers", "{root}/kh"));
    steps.push(Step::ToolFarm {
        links: vec![
            ("cpp".into(), "{in:gcc-10-bridge}/bin/cpp".into()),
            ("awk".into(), "{in:gawk-mesboot}/bin/gawk".into()),
            ("gawk".into(), "{in:gawk-mesboot}/bin/gawk".into()),
            ("make".into(), "{in:make-mesboot}/bin/make".into()),
        ],
    });
    steps.push(link_bins("binutils-244"));
    // Single-token static wrappers (see header): CC/CXX survive GCC's munging.
    // -idirafter preserves libstdc++'s #include_next ordering while exposing
    // only the declared glibc and kernel-header inputs to build-host programs.
    // The bridge ships a complete libstdc++, so no ABI-library tail or
    // optimization override is needed.
    for (name, real) in [("gcc", "gcc"), ("g++", "g++")] {
        steps.push(Step::WriteFile {
            path: format!("{{root}}/wb/{name}"),
            content: format!(
                "#!{SH}\nexec \"{{in:gcc-10-bridge}}/bin/{real}\" -static -idirafter {{in:glibc-mesboot}}/include -idirafter {{root}}/kh -B{{in:glibc-mesboot}}/lib \"$@\"\n"
            ),
            exec: true,
        });
    }
    steps.push(Step::PatchShebangs {
        dir: "{src}".into(),
        shell: SH.into(),
    });
    // GCC 14.3.0 configures under bash-mesboot: pre-expand the non-terminal
    // `*/config-lang.in` globs and rewrite the `env $depcmd` dep-probe (shared
    // helper -- same fixups every GCC 14.3.0 rung needs). --enable-languages
    // still selects only c,c++.
    steps.extend(gcc14_configure_fixups());
    // glibc-mesboot's sys_siglist is a deliberate stub; skip GCC's build-host
    // signal-name self-test and rely on the bridge regression plus the existing
    // x86_64 native/self behavioral checks.
    steps.push(gcc_disable_selftest());
    steps.push(gcc_install_headers_without_tar());
    // Assemble this gcc's libstdc++.a WITHOUT `find` (re #469): the same libtool
    // convenience-archive `find` that broke gcc-mesboot's libstdc++.a would break
    // gcc-14's too, leaving it partial. gcc-14's i686 libstdc++ is what the C++
    // build-side generator programs of gcc-x86-64-stage1/stage2/native link
    // against (CXX_FOR_BUILD = this gcc-14 g++), so it must be complete.
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
                "--prefix=/td/store/gcc-14.3.0",
                "--build=i686-unknown-linux-gnu",
                "--host=i686-unknown-linux-gnu",
                "--with-native-system-header-dir=/include",
                "--with-build-sysroot={in:glibc-mesboot}",
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
        .env("LDFLAGS", ldf),
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
                &format!("LDFLAGS={ldf}"),
                &format!("LDFLAGS_FOR_TARGET={ldf}"),
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
            "{out}/stage/td/store/gcc-14.3.0/bin/gcc".into(),
            "{out}/stage/td/store/gcc-14.3.0/bin/g++".into(),
        ],
        exec: true,
    });
    Recipe::mesboot("gcc-14", "14.3.0")
        .source_input("gcc-14-source")
        .native_inputs(&[
            "binutils-244",
            "gcc-10-bridge",
            "gawk-mesboot",
            "glibc-mesboot",
            "make-mesboot",
        ])
        .inputs_owned(mesboot0_inputs(&[
            "gmp63",
            "mpfr421",
            "mpc131",
            "linux-headers",
        ]))
        .steps(steps)
}

use crate::ladder::{
    gcc_configure_fixups, gcc_disable_selftest, gcc_install_headers_without_tar,
    libtool_extract_without_find, link_bins, mesboot0_inputs, mesboot0_path, unpack_into,
    unpack_keep_top, SH,
};
use crate::types::{Recipe, Step};

// GCC 10.5.0 — the transient i686 bridge between gcc-mesboot 4.9.4 and
// GCC 14.3.0. live-bootstrap uses the same release to cross from its old GCC
// 4.x compiler to a current GCC. Keeping this rung separate lets the final GCC
// 14 build use ordinary upstream optimization and option handling; no bridge
// bytes enter the x86_64 final closure.
//
// The build platform is the existing static glibc 2.16.0 closure, paired with
// binutils 2.44 rather than carrying binutils 2.20.1a into another modern GCC.
// Only C and C++ are enabled. The bridge itself is single-stage because it is
// immediately displaced by GCC 14 and the existing x86_64 native/self rungs.
// Host-free, re #469; bridge rationale and regression tracked by #525.
pub fn recipe() -> Recipe {
    let path = format!(
        "{{in:gcc-mesboot}}/bin:{{in:binutils-244}}/bin:{}",
        mesboot0_path()
    );
    let cip = "{in:gcc-mesboot}/lib/gcc/i686-unknown-linux-gnu/4.9.4/include:{root}/kh:{in:glibc-mesboot}/include:{src}/mpfr/src";
    let lp = "{in:glibc-mesboot}/lib:{in:gcc-mesboot}/lib";
    let ldf = "-static -B{in:glibc-mesboot}/lib";
    let mut steps = unpack_into("gcc-10-bridge-source", "{src}");
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
            ("cpp".into(), "{in:gcc-mesboot}/bin/cpp".into()),
            ("make".into(), "{in:make-mesboot}/bin/make".into()),
            // GCC regenerates its option table with AC_PROG_AWK, which prefers
            // `gawk` over `awk`. Both names must select the GCC-built 3.1.8:
            // tcc-built gawk-mesboot0 corrupts negative integers (#491), turning
            // the table's -1 neg_index sentinel into a self-cycle and making the
            // resulting cc1 loop or overflow in cancel_option (#515/#517).
            ("awk".into(), "{in:gawk-mesboot}/bin/gawk".into()),
            ("gawk".into(), "{in:gawk-mesboot}/bin/gawk".into()),
        ],
    });
    steps.push(link_bins("binutils-244"));
    // Single-token static wrappers survive GCC's CC_FOR_BUILD/CXX_FOR_BUILD
    // munging. GCC 4.9 defaults C to gnu90, while the in-tree GMP 6.3 build
    // tools use C99 declarations, so give C a gnu11 default before the build's
    // own arguments. GCC 4.9's C++ ABI runtime is a separate libsupc++.a; append
    // it after the normal g++ argv for GCC 10's C++ generator links.
    for (name, real, head, tail) in [
        ("gcc", "gcc", " -std=gnu11", ""),
        ("g++", "g++", "", " -lsupc++"),
    ] {
        steps.push(Step::WriteFile {
            path: format!("{{root}}/wb/{name}"),
            content: format!(
                "#!{SH}\nexec \"{{in:gcc-mesboot}}/bin/{real}\"{head} -static -B{{in:glibc-mesboot}}/lib \"$@\"{tail}\n"
            ),
            exec: true,
        });
    }
    steps.push(Step::PatchShebangs {
        dir: "{src}".into(),
        shell: SH.into(),
    });
    // GCC 10.5.0 has eleven language fragments (GCC 14 has twelve: it drops
    // brig and adds m2/rust). The helper also replaces the missing `env`
    // dependency in the automake depmode probes.
    steps.extend(gcc_configure_fixups(&[
        "ada", "brig", "c", "cp", "d", "fortran", "go", "jit", "lto", "objc", "objcp",
    ]));
    // The build-host self-test asks glibc 2.16's deliberately stubbed
    // sys_siglist for signal names and crashes. It is a development diagnostic,
    // not part of the installed compiler; the dedicated bridge test and the
    // downstream GCC 14 build provide the behavioral gate.
    steps.push(gcc_disable_selftest());
    // The mesboot userland has no tar. Select GCC's cp-based header installer,
    // as the earlier gcc-mesboot rungs do.
    steps.push(gcc_install_headers_without_tar());
    // libtool otherwise invokes the absent `find` and silently emits a partial
    // libstdc++.a. GCC 14's build-side C++ generators consume this archive.
    steps.push(libtool_extract_without_find("{src}/ltmain.sh"));
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
                "--disable-libcc1",
                "--disable-libgomp",
                "--disable-libitm",
                "--disable-libsanitizer",
                "--disable-libssp",
                "--disable-libvtv",
                "--disable-libquadmath",
                "--disable-lto",
                "--disable-plugin",
                "--disable-decimal-float",
                "--disable-nls",
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
                "CONFIG_SHELL={in:bash-mesboot}/bin/bash",
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
    Recipe::mesboot("gcc-10-bridge", "10.5.0")
        .source_input("gcc-10-bridge-source")
        .native_inputs(&[
            "binutils-244",
            "gcc-mesboot",
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

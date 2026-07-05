use crate::ladder::{SH, base_inputs, base_path, link_bins, unpack_into, unpack_keep_top};
use crate::types::{Recipe, Step};

// GCC 14.3.0 — rung 18 (#378, guix's gcc-boot0/gcc-final version): gcc-mesboot
// (4.9.4) + binutils-mesboot + the STATIC glibc 2.16.0 build the modern gcc,
// with gmp-6.3.0/mpfr-4.2.1/mpc-1.3.1 in-tree. Built STATIC via single-token
// wrapper scripts (gcc derives CC_FOR_BUILD from CC and strips trailing flags,
// so a bare `gcc -static …` would come apart — the deleted fn's proven trick).
// --prefix=/td/store/gcc-14.3.0 + DESTDIR={out}/stage: the host-consumable
// stage shape the chain tail reads. Host make -j{jobs} (the modern rungs
// parallelize; the mesboot base stays serial).
pub fn recipe() -> Recipe {
    let path = format!("{{in:gcc-mesboot}}/bin:{}", base_path());
    let cip = "{in:gcc-mesboot}/lib/gcc/i686-unknown-linux-gnu/4.9.4/include:{root}/kh:{in:glibc-mesboot}/include:{src}/mpfr/src";
    let lp = "{in:glibc-mesboot}/lib:{in:gcc-mesboot}/lib";
    let ldf = "-static -B{in:glibc-mesboot}/lib";
    let mut steps = unpack_into("gcc-14-source", "{src}");
    for t in ["gmp63", "mpfr421", "mpc131"] {
        steps.push(
            Step::run("{src}", &["{in:tar}/bin/tar", "-xf", &format!("{{in:{t}}}")])
                .env("PATH", &base_path()),
        );
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
            ("awk".into(), "{in:gawk}/bin/awk".into()),
            ("flex".into(), "{in:flex}/bin/flex".into()),
            ("lex".into(), "{in:flex}/bin/flex".into()),
            ("bison".into(), "{in:bison}/bin/bison".into()),
            ("yacc".into(), "{in:bison}/bin/bison".into()),
            ("m4".into(), "{in:m4}/bin/m4".into()),
            ("make".into(), "{in:make}/bin/make".into()),
        ],
    });
    steps.push(
        link_bins("binutils-mesboot"),
    );
    // single-token static wrappers (see header): CC/CXX survive gcc's munging
    for (name, real) in [("gcc", "gcc"), ("g++", "g++")] {
        steps.push(Step::WriteFile {
            path: format!("{{root}}/wb/{name}"),
            content: format!(
                "#!{SH}\nexec \"{{in:gcc-mesboot}}/bin/{real}\" -static -B{{in:glibc-mesboot}}/lib \"$@\"\n"
            ),
            exec: true,
        });
    }
    steps.push(Step::PatchShebangs {
        dir: "{src}".into(),
        shell: SH.into(),
    });
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
                "{in:make}/bin/make",
                "-j{jobs}",
                "SHELL={in:bash}/bin/bash",
                "CONFIG_SHELL={in:bash}/bin/bash",
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
                "{in:make}/bin/make",
                "SHELL={in:bash}/bin/bash",
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
        .native_inputs(&["binutils-mesboot", "gcc-mesboot", "glibc-mesboot"])
        .inputs_owned(base_inputs(&["gmp63", "mpfr421", "mpc131", "linux-headers", "flex", "bison", "m4", "make"]))
        .steps(steps)
}

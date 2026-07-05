use crate::ladder::{apply_patch, base_path, unpack_into, unpack_keep_top, SH};
use crate::types::{Recipe, Step};

// GNU Binutils 2.20.1a #2 — rung 10 (#378, guix's binutils-mesboot1): rebuilt
// by gcc-mesboot0 against glibc-mesboot0 + the PURE kernel UAPI headers. Two
// proven constraints from the deleted fn: NO -B in CC (gcc 2.95's "prefix never
// used" stderr poisons autoconf's header probes → fibheap loses LONG_MIN), and
// the PURE headers, never the mes-merged set (its limits.h shadows gcc's).
pub fn recipe() -> Recipe {
    let path = base_path();
    let cip = "{in:glibc-mesboot0}/include:{root}/kh";
    let lp = "{in:glibc-mesboot0}/lib:{in:gcc-mesboot0}/lib/gcc-lib/i686-unknown-linux-gnu/2.95.3";
    let cc = "CC={in:gcc-mesboot0}/bin/gcc -static";
    let mut steps = unpack_into("binutils-mesboot1-source", "{src}");
    steps.push(apply_patch("patch-mesboot", "patch-binutils-boot-2.20.1a"));
    steps.extend(unpack_keep_top("linux-headers", "{root}/kh"));
    steps.push(Step::ToolFarm {
        links: vec![
            ("cpp".into(), "{in:gcc-mesboot0}/bin/cpp".into()),
            ("make".into(), "{in:make-mesboot0}/bin/make".into()),
            ("patch".into(), "{in:patch-mesboot}/bin/patch".into()),
            ("awk".into(), "{in:gawk}/bin/awk".into()),
            ("flex".into(), "{in:flex}/bin/flex".into()),
            ("lex".into(), "{in:flex}/bin/flex".into()),
            ("bison".into(), "{in:bison}/bin/bison".into()),
            ("yacc".into(), "{in:bison}/bin/bison".into()),
        ],
    });
    steps.push(
        Step::run(
            "{root}",
            &[
                "{in:coreutils}/bin/ln",
                "-sf",
                "glob:{in:binutils-mesboot0}/bin/*",
                "{tools}",
            ],
        )
        .env("PATH", &path),
    );
    steps.push(
        Step::run(
            "{src}",
            &[
                SH,
                "./configure",
                cc,
                "AR=ar",
                "RANLIB=ranlib",
                "CXX=false",
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
            "SHELL={in:bash}/bin/bash",
            "CONFIG_SHELL={in:bash}/bin/bash",
        ];
        if let Some(t) = target {
            argv.push(t);
            argv.push("prefix={out}");
        } else {
            argv.extend([cc, "AR=ar", "RANLIB=ranlib", "CXX=false"]);
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
    Recipe::mesboot("binutils-mesboot1", "2.20.1a")
        .native_inputs(&[
            "make-mesboot0",
            "patch-mesboot",
            "binutils-mesboot0",
            "gcc-mesboot0",
            "glibc-mesboot0",
        ])
        .inputs(&[
            "patch-binutils-boot-2.20.1a",
            "linux-headers",
            "flex",
            "bison",
            "bash",
            "coreutils",
            "sed",
            "grep",
            "gawk",
            "tar",
            "gzip",
            "bzip2",
            "xz",
            "findutils",
            "diffutils",
        ])
        .steps(steps)
}

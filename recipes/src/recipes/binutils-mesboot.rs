use crate::ladder::{SH, apply_patch, base_inputs, base_path, link_bins, unpack_into, unpack_keep_top};
use crate::types::{Recipe, Step};

// GNU Binutils 2.20.1a #3 — rung 13 (#378, guix's binutils-mesboot): rebuilt by
// gcc-mesboot1 (4.6.4). Same plain configure as binutils-mesboot1; only the
// compiler steps up (its gcc-lib lives at lib/gcc/<triplet>/4.6.4 now).
pub fn recipe() -> Recipe {
    let path = format!("{{in:gcc-mesboot1}}/bin:{}", base_path());
    let cip = "{in:glibc-mesboot0}/include:{root}/kh";
    let lp = "{in:glibc-mesboot0}/lib:{in:gcc-mesboot1}/lib/gcc/i686-unknown-linux-gnu/4.6.4";
    let cc = "CC={in:gcc-mesboot1}/bin/gcc -static";
    let mut steps = unpack_into("binutils-mesboot-source", "{src}");
    steps.push(apply_patch("patch-mesboot", "patch-binutils-boot-2.20.1a"));
    steps.extend(unpack_keep_top("linux-headers", "{root}/kh"));
    steps.push(Step::ToolFarm {
        links: vec![
            ("cpp".into(), "{in:gcc-mesboot1}/bin/cpp".into()),
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
            "{in:make-mesboot}/bin/make",
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
    Recipe::mesboot("binutils-mesboot", "2.20.1a")
        .source_input("binutils-mesboot-source")
        .native_inputs(&[
            "make-mesboot",
            "patch-mesboot",
            "binutils-mesboot1",
            "gcc-mesboot1",
            "glibc-mesboot0",
        ])
        .inputs_owned(base_inputs(&["patch-binutils-boot-2.20.1a", "linux-headers", "flex", "bison"]))
        .steps(steps)
}

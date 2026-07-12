use crate::ladder::{SH, base_inputs, base_path, link_bins, unpack_into, unpack_keep_top};
use crate::types::{Recipe, Step};

// GNU awk 3.1.8 — rung 14 (#378, guix's gawk-mesboot): gcc-mesboot1 builds the
// awk glibc-mesboot 2.16.0's versions.awk needs (the seed gash awk is too
// weak). No sockets (ac_cv_func_connect=no); only the gawk binary is made.
pub fn recipe() -> Recipe {
    let path = format!("{{in:gcc-mesboot1}}/bin:{}", base_path());
    let cip = "{in:glibc-mesboot0}/include:{root}/kh";
    let lp = "{in:glibc-mesboot0}/lib:{in:gcc-mesboot1}/lib/gcc/i686-unknown-linux-gnu/4.6.4";
    let cc = "CC={in:gcc-mesboot1}/bin/gcc -static";
    let mut steps = unpack_into("gawk-mesboot-source", "{src}");
    steps.extend(unpack_keep_top("linux-headers", "{root}/kh"));
    steps.push(Step::ToolFarm {
        links: vec![
            ("cpp".into(), "{in:gcc-mesboot1}/bin/cpp".into()),
            ("make".into(), "{in:make-mesboot}/bin/make".into()),
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
                "ac_cv_func_connect=no",
                "LIBS=-lc -lnss_files -lnss_dns -lresolv",
                "--build=i686-unknown-linux-gnu",
                "--host=i686-unknown-linux-gnu",
                "--disable-nls",
            ],
        )
        .env("PATH", &path)
        .env("CONFIG_SHELL", SH)
        .env("C_INCLUDE_PATH", cip)
        .env("LIBRARY_PATH", lp),
    );
    steps.push(
        Step::run(
            "{src}",
            &[
                "{in:make-mesboot}/bin/make",
                "SHELL={in:bash-mesboot}/bin/bash",
                "CONFIG_SHELL={in:bash-mesboot}/bin/bash",
                cc,
                "AR=ar",
                "RANLIB=ranlib",
                "gawk",
            ],
        )
        .env("PATH", &path)
        .env("C_INCLUDE_PATH", cip)
        .env("LIBRARY_PATH", lp),
    );
    steps.push(Step::CopyFiles {
        files: vec!["{src}/gawk".into()],
        dest: "{out}/bin".into(),
    });
    steps.push(Step::Symlink {
        target: "gawk".into(),
        link: "{out}/bin/awk".into(),
    });
    steps.push(Step::Require {
        paths: vec!["{out}/bin/gawk".into()],
        exec: true,
    });
    Recipe::mesboot("gawk-mesboot", "3.1.8")
        .source_input("gawk-mesboot-source")
        .native_inputs(&[
            "make-mesboot",
            "binutils-mesboot1",
            "gcc-mesboot1",
            "glibc-mesboot0",
        ])
        .inputs_owned(base_inputs(&["linux-headers"]))
        .steps(steps)
}

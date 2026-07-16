use crate::ladder::{SH, link_bins, mesboot0_inputs, mesboot0_path, unpack_into, unpack_keep_top};
use crate::types::{Recipe, Step};

// GNU awk 3.1.8 — rung 14 (#378, guix's gawk-mesboot): gcc-mesboot1 builds the
// awk glibc-mesboot 2.16.0's versions.awk needs (the seed gash awk is too
// weak). No sockets (ac_cv_func_connect=no); only the gawk binary is made.
//
// Host-tool ingress closed (re #469): cut over to the `-mesboot0` providers —
// mesboot0_path()/mesboot0_inputs() and the binutils link_bins farm. Any
// `awk`/`sed` gawk's own `configure`/Makefile invokes now resolves to the
// `-mesboot0` cycle-breakers (gawk-mesboot0 3.0.4, sed-mesboot0 4.0.9) that
// mesboot0_path() puts ahead of any host tool. Per-rung cutover for #469; the
// shared host mechanism goes in the final atomic PR.
pub fn recipe() -> Recipe {
    let path = format!("{{in:gcc-mesboot1}}/bin:{}", mesboot0_path());
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
    steps.push(link_bins("binutils-mesboot1"));
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
        .inputs_owned(mesboot0_inputs(&["linux-headers"]))
        .steps(steps)
}

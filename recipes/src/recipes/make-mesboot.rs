use crate::ladder::{SH, base_inputs, base_path, link_bins, unpack_into, unpack_keep_top};
use crate::types::{Recipe, Step};

// GNU Make 3.82 — rung 11 (#378, guix's make-mesboot): gcc-mesboot0 rebuilds
// make against glibc-mesboot0; the tcc-built make 3.80 drives it. The static
// glibc names its nss/resolv archives explicitly (LIBS), as guix does.
pub fn recipe() -> Recipe {
    let path = base_path();
    let cip = "{in:glibc-mesboot0}/include:{root}/kh";
    let lp = "{in:glibc-mesboot0}/lib:{in:gcc-mesboot0}/lib/gcc-lib/i686-unknown-linux-gnu/2.95.3";
    let cc = "CC={in:gcc-mesboot0}/bin/gcc -static";
    let mut steps = unpack_into("make-mesboot-source", "{src}");
    steps.extend(unpack_keep_top("linux-headers", "{root}/kh"));
    steps.push(Step::ToolFarm {
        links: vec![
            ("cpp".into(), "{in:gcc-mesboot0}/bin/cpp".into()),
            ("make".into(), "{in:make-mesboot0}/bin/make".into()),
            ("awk".into(), "{in:gawk}/bin/awk".into()),
        ],
    });
    steps.push(
        link_bins("binutils-mesboot0"),
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
                "{in:make-mesboot0}/bin/make",
                "SHELL={in:bash-mesboot}/bin/bash",
                "CONFIG_SHELL={in:bash-mesboot}/bin/bash",
                cc,
                "AR=ar",
                "RANLIB=ranlib",
            ],
        )
        .env("PATH", &path)
        .env("C_INCLUDE_PATH", cip)
        .env("LIBRARY_PATH", lp),
    );
    steps.push(Step::CopyFiles {
        files: vec!["{src}/make".into()],
        dest: "{out}/bin".into(),
    });
    steps.push(Step::Require {
        paths: vec!["{out}/bin/make".into()],
        exec: true,
    });
    Recipe::mesboot("make-mesboot", "3.82")
        .source_input("make-mesboot-source")
        .native_inputs(&[
            "make-mesboot0",
            "binutils-mesboot0",
            "gcc-mesboot0",
            "glibc-mesboot0",
        ])
        .inputs_owned(base_inputs(&["linux-headers"]))
        .steps(steps)
}

use crate::ladder::{SH, base_inputs, base_path, link_bins, unpack_into, unpack_keep_top};
use crate::types::{Recipe, Step};

// GNU sed 4.2.2 — rung 14 tier (re #469, sibling of gawk-mesboot): gcc-mesboot1
// builds the from-source `sed` provider so no rung that can reach this output
// has to lean on the host `sed` (BASE_TOOLS). Same toolchain, env, and configure
// shape as the gawk-mesboot rung one axis over — static CC against glibc-mesboot0
// + the kernel headers, NLS off, only the `sed` binary made (its `doc/` manpage
// wants help2man/makeinfo, which the sandbox does not carry). sed's bundled
// gnulib papers over the glibc 2.2.5 gaps exactly as gawk 3.1.8's does.
pub fn recipe() -> Recipe {
    let path = format!("{{in:gcc-mesboot1}}/bin:{}", base_path());
    let cip = "{in:glibc-mesboot0}/include:{root}/kh";
    let lp = "{in:glibc-mesboot0}/lib:{in:gcc-mesboot1}/lib/gcc/i686-unknown-linux-gnu/4.6.4";
    let cc = "CC={in:gcc-mesboot1}/bin/gcc -static";
    let mut steps = unpack_into("sed-mesboot-source", "{src}");
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
                "--build=i686-unknown-linux-gnu",
                "--host=i686-unknown-linux-gnu",
                "--disable-nls",
                "--disable-acl",
            ],
        )
        .env("PATH", &path)
        .env("CONFIG_SHELL", SH)
        .env("C_INCLUDE_PATH", cip)
        .env("LIBRARY_PATH", lp),
    );
    // gnulib (`lib/`) first, then the binary (`sed/`) — the two subdirs the
    // binary needs, skipping `po/` (NLS off) and `doc/` (no help2man/makeinfo).
    for subdir in ["lib", "sed"] {
        steps.push(
            Step::run(
                "{src}",
                &[
                    "{in:make-mesboot}/bin/make",
                    "-C",
                    subdir,
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
    }
    steps.push(Step::CopyFiles {
        files: vec!["{src}/sed/sed".into()],
        dest: "{out}/bin".into(),
    });
    steps.push(Step::Require {
        paths: vec!["{out}/bin/sed".into()],
        exec: true,
    });
    Recipe::mesboot("sed-mesboot", "4.2.2")
        .source_input("sed-mesboot-source")
        .native_inputs(&[
            "make-mesboot",
            "binutils-mesboot1",
            "gcc-mesboot1",
            "glibc-mesboot0",
        ])
        .inputs_owned(base_inputs(&["linux-headers"]))
        .steps(steps)
}

use crate::ladder::{SH, link_bins_mesboot0, mesboot0_inputs, mesboot0_path, unpack_into, unpack_keep_top};
use crate::types::{Recipe, Step};

// GNU sed 4.2.2 — rung 14 tier (re #469, sibling of gawk-mesboot): gcc-mesboot1
// builds the from-source `sed` provider so a rung that can reach this output can
// consume a td-built `sed` instead of the host one. Same toolchain,
// env, and configure shape as the gawk-mesboot rung one axis over — static CC
// against glibc-mesboot0 + the kernel headers, NLS off, only the `sed` binary
// made (its `doc/` manpage wants help2man/makeinfo, which the sandbox does not
// carry). sed's bundled gnulib papers over the glibc 2.2.5 gaps exactly as gawk
// 3.1.8's does.
//
// Host-tool ingress closed (re #469): cut over to the `-mesboot0` providers —
// mesboot0_path()/mesboot0_inputs() and the binutils link_bins_mesboot0 farm.
// sed's own `configure` and generated `lib/Makefile` invoke `sed`; that `sed`
// now resolves to the `sed-mesboot0` cycle-breaker (GNU sed 4.0.9, built with no
// `sed` on PATH) which mesboot0_path() puts ahead of any host tool — the exact
// cycle-break this recipe's earlier scope note called for. Per-rung cutover for
// #469; the shared host mechanism goes in the final atomic PR. Output contract:
// the binary must exist, be fully static (no host loader/libc leaked in, re
// #469), and actually perform a substitution.
pub fn recipe() -> Recipe {
    let path = format!("{{in:gcc-mesboot1}}/bin:{}", mesboot0_path());
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
    steps.push(link_bins_mesboot0("binutils-mesboot1"));
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
    // Static-linkage contract (re #469): reject a host loader/libc leak, as every
    // static bootstrap rung (tcc/make-mesboot0/oyacc/patch-mesboot/bash-mesboot)
    // does. sed does no networking, so the `-static` glibc-mesboot0 link carries
    // no NSS dynamic pull.
    steps.push(Step::assert_static(&["{out}/bin/sed"]));
    // Behavioral smoke: run the just-built `sed` on a substitution and fail the
    // rung unless it produces the expected output — the same "exec {out} binary"
    // idiom patch-mesboot/oyacc/bash-mesboot use. `printf`/`test` are bash-mesboot
    // builtins, so the check stays host-tool-free.
    steps.push(Step::run(
        "{src}",
        &[
            SH,
            "-c",
            "test \"$(printf %s hello | {out}/bin/sed s/hello/world/)\" = world",
        ],
    ));
    Recipe::mesboot("sed-mesboot", "4.2.2")
        .source_input("sed-mesboot-source")
        .native_inputs(&[
            "make-mesboot",
            "binutils-mesboot1",
            "gcc-mesboot1",
            "glibc-mesboot0",
        ])
        .inputs_owned(mesboot0_inputs(&["linux-headers"]))
        .steps(steps)
}

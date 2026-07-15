use crate::ladder::{SH, apply_patch, link_bins_mesboot0, mesboot0_inputs, mesboot0_path, unpack_into, unpack_keep_top};
use crate::types::{Recipe, Step};

// GNU Binutils 2.20.1a #2 — rung 10 (#378, guix's binutils-mesboot1): rebuilt
// by gcc-mesboot0 against glibc-mesboot0 + the PURE kernel UAPI headers. Two
// proven constraints from the deleted fn: NO -B in CC (gcc 2.95's "prefix never
// used" stderr poisons autoconf's header probes → fibheap loses LONG_MIN), and
// the PURE headers, never the mes-merged set (its limits.h shadows gcc's).
//
// Host-tool ingress closed (re #469): mechanical cutover to the `-mesboot0`
// providers — mesboot0_path()/mesboot0_inputs(), `awk` -> gawk-mesboot0, and the
// binutils link_bins_mesboot0 farm. `make all` recurses into `binutils/`, whose
// `configure` runs `AM_PROG_LEX`; with no flex this far down the ladder autoconf
// falls back to the automake `missing` wrapper to stub `lex.yy.c` and exec's it
// DIRECTLY, so once the sandbox is host-free the PatchShebangs rewrite of
// `missing`'s dead `#! /bin/sh` is required (the same rewrite binutils-mesboot0
// rung 9 applies). Per-rung cutover for #469; the shared host mechanism goes in
// the final atomic PR.
pub fn recipe() -> Recipe {
    let path = mesboot0_path();
    let cip = "{in:glibc-mesboot0}/include:{root}/kh";
    let lp = "{in:glibc-mesboot0}/lib:{in:gcc-mesboot0}/lib/gcc-lib/i686-unknown-linux-gnu/2.95.3";
    let cc = "CC={in:gcc-mesboot0}/bin/gcc -static";
    let mut steps = unpack_into("binutils-mesboot1-source", "{src}");
    steps.push(apply_patch("patch-mesboot", "patch-binutils-boot-2.20.1a"));
    steps.extend(unpack_keep_top("linux-headers", "{root}/kh"));
    // Retarget every `#! /bin/sh` shebang to the declared shell — the host-free
    // sandbox has no /bin/sh, and `binutils/configure`'s AM_PROG_LEX exec's the
    // automake `missing` wrapper directly to stub lex.yy.c (see the header note).
    // Mirrors binutils-mesboot0.rs (rung 9); must precede configure.
    steps.push(Step::PatchShebangs {
        dir: "{src}".into(),
        shell: SH.into(),
    });
    steps.push(Step::ToolFarm {
        links: vec![
            ("cpp".into(), "{in:gcc-mesboot0}/bin/cpp".into()),
            ("make".into(), "{in:make-mesboot0}/bin/make".into()),
            ("patch".into(), "{in:patch-mesboot}/bin/patch".into()),
            ("awk".into(), "{in:gawk-mesboot0}/bin/awk".into()),
        ],
    });
    steps.push(link_bins_mesboot0("binutils-mesboot0"));
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
            "SHELL={in:bash-mesboot}/bin/bash",
            "CONFIG_SHELL={in:bash-mesboot}/bin/bash",
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
        .source_input("binutils-mesboot-source")
        .native_inputs(&[
            "make-mesboot0",
            "patch-mesboot",
            "binutils-mesboot0",
            "gcc-mesboot0",
            "glibc-mesboot0",
        ])
        .inputs_owned(mesboot0_inputs(&["patch-binutils-boot-2.20.1a", "linux-headers"]))
        .steps(steps)
}

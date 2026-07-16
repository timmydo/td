use crate::ladder::{SH, mesboot0_inputs, mesboot0_path, unpack_into};
use crate::types::{Recipe, Step};

// GNU Bison 3.8.2 — the parser generator glibc 2.41's configure requires
// (>= 2.7, critical; re #469). Built at the gcc-14 tier, STATIC against the
// static glibc 2.16.0 (glibc-mesboot), like m4-mesboot and glibc-x86-64's
// BUILD_CC — a static i686 bison runs in BOTH glibc build sandboxes with no
// interp/RUNPATH story. The pristine tarball ships the pre-generated
// parser/scanner C, so no bootstrap bison and no flex run here.
//
// Self-contained via --prefix={out} (the gcc-mesboot pattern): bison's
// compiled-in PKGDATADIR is {out}/share/bison, exactly where its skeletons
// install, so it finds yacc.c/bison.m4/m4sugar at runtime with no
// BISON_PKGDATADIR override. It execs GNU m4, so the absolute path to
// m4-mesboot is baked in at configure (M4=), stable because {in:m4-mesboot}
// is a content-addressed store path. Host-free build tools: mesboot0 +
// make-mesboot; binutils-244 supplies as/ld/ar/ranlib.
pub fn recipe() -> Recipe {
    let path = format!("{{in:binutils-244}}/bin:{}", mesboot0_path());
    let mut steps = unpack_into("bison-mesboot-source", "{src}");
    // Retarget every `#! /bin/sh` shebang to the declared shell — the host-free
    // sandbox has no /bin/sh for a directly-exec'd gnulib/automake helper to
    // fall back on. Must precede configure.
    steps.push(Step::PatchShebangs {
        dir: "{src}".into(),
        shell: SH.into(),
    });
    steps.push(Step::ToolFarm {
        links: vec![
            ("awk".into(), "{in:gawk-mesboot0}/bin/awk".into()),
            ("make".into(), "{in:make-mesboot}/bin/make".into()),
            ("m4".into(), "{in:m4-mesboot}/bin/m4".into()),
        ],
    });
    // static gcc-14 vs the static glibc 2.16.0 (glibc-x86-64's BUILD_CC shape).
    steps.push(Step::WriteFile {
        path: "{root}/wb/gcc".into(),
        content: format!(
            "#!{SH}\nexec \"{{in:gcc-14}}/stage/td/store/gcc-14.3.0/bin/gcc\" -static -idirafter {{in:glibc-mesboot}}/include -B{{in:glibc-mesboot}}/lib \"$@\"\n"
        ),
        exec: true,
    });
    steps.push(
        Step::run(
            "{src}",
            &[
                SH,
                "./configure",
                "--build=i686-unknown-linux-gnu",
                "--host=i686-unknown-linux-gnu",
                "--prefix={out}",
                "--disable-nls",
            ],
        )
        .env("PATH", &path)
        .env("CONFIG_SHELL", SH)
        .env("SHELL", SH)
        .env("CC", "{root}/wb/gcc")
        // GCC 14 defaults to gnu17; pin gnu11 for the pristine 2021 gnulib.
        .env("CFLAGS", "-std=gnu11 -O2")
        // Bake the absolute path to td's m4 (bison execs it for skeletons);
        // {in:m4-mesboot} is a stable content-addressed store path.
        .env("M4", "{in:m4-mesboot}/bin/m4"),
    );
    steps.push(
        Step::run(
            "{src}",
            &[
                "{in:make-mesboot}/bin/make",
                "-j{jobs}",
                "SHELL={in:bash-mesboot}/bin/bash",
                "CONFIG_SHELL={in:bash-mesboot}/bin/bash",
                "MAKEINFO=true",
            ],
        )
        .env("PATH", &path)
        .env("CONFIG_SHELL", SH)
        .env("SHELL", SH),
    );
    steps.push(
        Step::run(
            "{src}",
            &[
                "{in:make-mesboot}/bin/make",
                "SHELL={in:bash-mesboot}/bin/bash",
                "MAKEINFO=true",
                "install",
            ],
        )
        .env("PATH", &path)
        .env("CONFIG_SHELL", SH)
        .env("SHELL", SH),
    );
    steps.push(Step::Require {
        paths: vec!["{out}/bin/bison".into()],
        exec: true,
    });
    Recipe::mesboot("bison-mesboot", "3.8.2")
        .source_input("bison-mesboot-source")
        .native_inputs(&["gcc-14", "glibc-mesboot", "binutils-244", "make-mesboot", "m4-mesboot"])
        .inputs_owned(mesboot0_inputs(&[]))
        .steps(steps)
}

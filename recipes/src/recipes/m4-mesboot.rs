use crate::ladder::{SH, mesboot0_inputs, mesboot0_path, unpack_into};
use crate::types::{Recipe, Step};

// GNU M4 1.4.19 — the macro processor the glibc rungs need (re #469): `bison`
// execs m4 at runtime, and glibc's build declares m4 directly. Built at the
// gcc-14 tier, STATIC against the static glibc 2.16.0 (glibc-mesboot), like
// glibc-x86-64's BUILD_CC and every other gcc-14-tier build tool — a static
// i686 m4 runs in BOTH glibc build sandboxes (native glibc-241 and cross
// glibc-x86-64) with no interp/RUNPATH story. Host-free build tools: mesboot0
// + make-mesboot; binutils-244 supplies as/ld/ar/ranlib.
pub fn recipe() -> Recipe {
    let path = format!("{{in:binutils-244}}/bin:{}", mesboot0_path());
    let mut steps = unpack_into("m4-mesboot-source", "{src}");
    // Retarget every `#! /bin/sh` shebang to the declared shell — the host-free
    // sandbox has no /bin/sh for a directly-exec'd gnulib/automake helper
    // (`missing`, `config.status`, …) to fall back on. Must precede configure.
    steps.push(Step::PatchShebangs {
        dir: "{src}".into(),
        shell: SH.into(),
    });
    steps.push(Step::ToolFarm {
        links: vec![
            ("awk".into(), "{in:gawk-mesboot0}/bin/awk".into()),
            ("make".into(), "{in:make-mesboot}/bin/make".into()),
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
                "--build=i686-pc-linux-gnu",
                "--host=i686-unknown-linux-gnu",
                "--prefix={out}",
                "--disable-nls",
            ],
        )
        .env("PATH", &path)
        .env("CONFIG_SHELL", SH)
        .env("SHELL", SH)
        .env("CC", "{root}/wb/gcc")
        // GCC 14 defaults to gnu17; pin gnu11 so m4 1.4.19's 2021-vintage
        // gnulib compiles clean (the pristine tarball, no autoreconf). No
        // -latomic: i686 inlines <=8-byte atomics and gcc-14 is built
        // --disable-libatomic, so pristine m4 must not reference it.
        .env("CFLAGS", "-std=gnu11 -O2"),
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
        paths: vec!["{out}/bin/m4".into()],
        exec: true,
    });
    Recipe::mesboot("m4-mesboot", "1.4.19")
        .source_input("m4-mesboot-source")
        .native_inputs(&["gcc-14", "glibc-mesboot", "binutils-244", "make-mesboot"])
        .inputs_owned(mesboot0_inputs(&[]))
        .steps(steps)
}

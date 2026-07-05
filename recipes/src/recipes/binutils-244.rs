use crate::ladder::{SH, base_inputs, base_path, unpack_into};
use crate::types::{Recipe, Step};

// GNU Binutils 2.44 — rung 19 (#378): the modern binutils glibc 2.41 needs
// (2.20.1a is too old), built by gcc-mesboot1 (4.6.4) STATIC against the
// static glibc 2.16.0 — like every other mesboot binutils. The deleted ladder
// built it DYNAMIC with a build-dir interp only to make it sandbox-runnable
// (the TMPDIR-length dance); as recipes, static as/ld run ANYWHERE — later
// rung sandboxes, store-ns own-roots — with no interp/RUNPATH story at all.
// -std=gnu99 (2.44 is C99+; 4.6.4 defaults gnu89); deterministic archives;
// install prefix={out}.
pub fn recipe() -> Recipe {
    let path = format!("{{in:binutils-mesboot}}/bin:{}", base_path());
    let mut steps = unpack_into("binutils-244-source", "{src}");
    steps.push(Step::ToolFarm {
        links: vec![
            ("awk".into(), "{in:gawk}/bin/awk".into()),
            ("flex".into(), "{in:flex}/bin/flex".into()),
            ("lex".into(), "{in:flex}/bin/flex".into()),
            ("bison".into(), "{in:bison}/bin/bison".into()),
            ("yacc".into(), "{in:bison}/bin/bison".into()),
            ("make".into(), "{in:make}/bin/make".into()),
        ],
    });
    // single-token static wrapper (the gcc-14 rung's proven shape): gcc 4.6.4
    // vs the STATIC glibc 2.16.0.
    steps.push(Step::WriteFile {
        path: "{root}/wb/gcc".into(),
        content: format!(
            "#!{SH}\nexec \"{{in:gcc-mesboot1}}/bin/gcc\" -std=gnu99 -static -isystem \"{{in:glibc-mesboot}}/include\" -B\"{{in:glibc-mesboot}}/lib\" -L\"{{in:glibc-mesboot}}/lib\" -L\"{{in:gcc-mesboot1}}/lib/gcc/i686-unknown-linux-gnu/4.6.4\" \"$@\"\n"
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
                "--prefix=/td/store/binutils-2.44",
                "--disable-nls",
                "--disable-gold",
                "--disable-werror",
                "--enable-deterministic-archives",
                "--disable-plugins",
                "--disable-gprofng",
            ],
        )
        .env("PATH", &path)
        .env("CONFIG_SHELL", SH)
        .env("SHELL", SH)
        .env("CC", "{root}/wb/gcc")
        .env("CC_FOR_BUILD", "{root}/wb/gcc")
        .env("AR", "{in:binutils-mesboot}/bin/ar")
        .env("RANLIB", "{in:binutils-mesboot}/bin/ranlib"),
    );
    steps.push(
        Step::run(
            "{src}",
            &[
                "{in:make}/bin/make",
                "-j{jobs}",
                "SHELL={in:bash}/bin/bash",
                "CONFIG_SHELL={in:bash}/bin/bash",
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
                "{in:make}/bin/make",
                "SHELL={in:bash}/bin/bash",
                "MAKEINFO=true",
                "install",
                "prefix={out}",
            ],
        )
        .env("PATH", &path)
        .env("CONFIG_SHELL", SH)
        .env("SHELL", SH),
    );
    steps.push(Step::Require {
        paths: vec!["{out}/bin/as".into(), "{out}/bin/ld".into()],
        exec: true,
    });
    Recipe::mesboot("binutils-244", "2.44")
        .native_inputs(&["gcc-mesboot1", "glibc-mesboot", "binutils-mesboot"])
        .inputs_owned(base_inputs(&["flex", "bison", "make"]))
        .steps(steps)
}

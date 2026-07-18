use crate::ladder::{
    libtool_extract_without_find, mesboot0_inputs, mesboot0_path, unpack_into, unpack_keep_top, SH,
};
use crate::types::{Recipe, Step};

// GNU Binutils 2.44, CROSS to x86_64 (#378 slice 4, guix's cross-binutils):
// --target=x86_64-pc-linux-gnu, built STATIC by the completed i686 gcc 14.3.0
// (against the static glibc 2.16, like every host part of the cross build).
// Produces i686 host binaries `x86_64-pc-linux-gnu-{as,ld,ar,…}` that EMIT
// x86_64. --with-sysroot points at the x86_64 kernel UAPI headers (the libc
// isn't built yet). Logical --prefix=/td/store/binutils-2.44-x86_64; install
// to {out}. No /tmp repro scaffolding — the drv sandbox already gives a stable
// build path (the shell fn's fixed-/tmp dance is retired, like #389's TMPDIR
// hack). native_inputs: the i686 gcc-14 (builder) + glibc-mesboot (the static
// libc its host binaries link) + binutils-244 (the i686 host as/ld/ar).
// Host-free build tools: mesboot0 + make-mesboot; flex/bison dead (binutils-244-source). re #469.
pub fn recipe() -> Recipe {
    let path = format!("{{in:binutils-244}}/bin:{}", mesboot0_path());
    let mut steps = unpack_into("binutils-x86-64-source", "{src}");
    // the x86_64 kernel UAPI headers → a sysroot (--with-sysroot); the tarball
    // top level is {linux,asm,…}, landing at usr/include/*.
    steps.extend(unpack_keep_top(
        "linux-headers-x86-64",
        "{root}/sysroot/usr/include",
    ));
    steps.push(Step::ToolFarm {
        links: vec![
            ("awk".into(), "{in:gawk-mesboot0}/bin/awk".into()),
            ("make".into(), "{in:make-mesboot}/bin/make".into()),
        ],
    });
    // single-token static i686 gcc-14 wrapper vs the static glibc 2.16.0.
    // -idirafter (not -isystem): the same wrapper shape the gcc rungs reuse, so
    // libstdc++'s `#include_next` still resolves; harmless for C-only binutils.
    steps.push(Step::WriteFile {
        path: "{root}/wb/cc".into(),
        content: format!(
            "#!{SH}\nexec \"{{in:gcc-14}}/stage/td/store/gcc-14.3.0/bin/gcc\" -static -idirafter {{in:glibc-mesboot}}/include -B{{in:glibc-mesboot}}/lib \"$@\"\n"
        ),
        exec: true,
    });
    // libbfd absorbs libsframe as a convenience archive. Without find, bundled
    // libtool otherwise drops every extracted SFrame object and gas fails to
    // link as-new with unresolved sframe_* symbols.
    steps.push(libtool_extract_without_find("{src}/ltmain.sh"));
    steps.push(
        Step::run(
            "{src}",
            &[
                SH,
                "./configure",
                "--build=i686-pc-linux-gnu",
                "--host=i686-pc-linux-gnu",
                "--target=x86_64-pc-linux-gnu",
                "--prefix=/td/store/binutils-2.44-x86_64",
                "--with-sysroot={root}/sysroot",
                "--disable-nls",
                "--disable-gold",
                "--disable-werror",
                "--enable-deterministic-archives",
                "--disable-plugins",
                "--disable-gprofng",
                "--disable-multilib",
                // The compiler wrapper's implicit -static is invisible to
                // libtool. Declare the mode so libbfd's static libsframe
                // dependency is propagated when gas links as-new.
                "--disable-shared",
                "--enable-static",
            ],
        )
        .env("PATH", &path)
        .env("CONFIG_SHELL", SH)
        .env("SHELL", SH)
        .env("CC", "{root}/wb/cc")
        .env("CC_FOR_BUILD", "{root}/wb/cc")
        .env("AR", "{in:binutils-244}/bin/ar")
        .env("RANLIB", "{in:binutils-244}/bin/ranlib"),
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
                "prefix={out}",
            ],
        )
        .env("PATH", &path)
        .env("CONFIG_SHELL", SH)
        .env("SHELL", SH),
    );
    steps.push(Step::Require {
        paths: vec![
            "{out}/bin/x86_64-pc-linux-gnu-as".into(),
            "{out}/bin/x86_64-pc-linux-gnu-ld".into(),
        ],
        exec: true,
    });
    Recipe::mesboot("binutils-x86-64", "2.44")
        .source_input("binutils-244-source")
        .native_inputs(&["gcc-14", "glibc-mesboot", "binutils-244", "make-mesboot"])
        .inputs_owned(mesboot0_inputs(&["linux-headers-x86-64"]))
        .steps(steps)
}

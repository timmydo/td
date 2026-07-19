use crate::ladder::{mesboot0_inputs, mesboot0_path, unpack_into, unpack_keep_top, SH};
use crate::types::{Recipe, Step};

// elfutils 0.192 — `libelf`, the ELF-access library the modern Linux kernel's
// objtool host tool links against (re #529). objtool is force-selected on
// x86_64 in Linux 7.x (HAVE_STATIC_CALL_INLINE + HAVE_UACCESS_VALIDATION both
// `select OBJTOOL` unconditionally), so unlike the 4.14 rung — which dodged
// objtool with the frame-pointer unwinder — a modern x86_64 vmlinux cannot
// avoid libelf. Built FROM SOURCE by td's OWN native x86_64 toolchain
// (gcc-x86-64-native 14.3.0 + binutils-x86-64-native 2.44), driven by the
// td-built make-x86-64, as a set of STATIC archives objtool can link.
//
// libelf-ONLY build: elfutils' full tree (libdw/libasm/backends/src) needs far
// more, but objtool needs only libelf. The tarball's `libelf/` is pure C (no
// lexers/parsers → no flex/bison), so after `./configure` we build just
// `make -C lib libeu.a` then `make -C libelf libelf.a`. The static libelf.a is
// NOT self-contained the way the shipped libelf.so is: it link-depends on
// libeu.a (eu_tsearch, pulled via elf_getdata) and, via elf_getdata's
// section-decompress path, on zlib. zlib is a HARD elfutils requirement
// (configure errors without it; libelf/elf_compress.c #includes <zlib.h>
// unconditionally), so this rung first builds a STATIC libz.a from the pinned
// zlib source with the same native toolchain, links elfutils against it, and
// ships libelf.a + libeu.a + libz.a + the libelf headers together in {out} —
// everything objtool's `-lelf -leu -lz` link needs from one input.
//
// No flex/bison/m4/pkg-config/gettext: elfutils' configure only hard-requires
// those in maintainer mode (off) or on the compression/demangler paths we
// --without/--disable. Its behavior is validated by the sibling
// `elfutils-x86-64-test` rung, which compiles+links+runs a libelf program
// against exactly these static archives. Host-free tools: mesboot0 +
// make-x86-64. re #469.
pub fn recipe() -> Recipe {
    let ngcc = "{in:gcc-x86-64-native}/stage/td/store/gcc-14.3.0-x86_64-native/bin/gcc";
    let xglibc = "{in:glibc-x86-64}/stage/td/store/glibc-2.41-x86_64";
    let nbin = "{in:binutils-x86-64-native}/bin";
    // make-x86-64 MUST be on PATH (not merely invoked by absolute path): autoconf's
    // config.status bootstraps automake's dependency-tracking `.deps` fragments by
    // running a bare `make`, and elfutils' Makefiles recurse via `$(MAKE)`. With no
    // `make` on PATH the dep-tracking bootstrap dies ("Something went wrong
    // bootstrapping makefile fragments ... consider re-running with MAKE=gmake").
    // Mirrors the flex-x86-64 / kernel rungs. re #529.
    let path = format!("{nbin}:{{in:make-x86-64}}/bin:{}", mesboot0_path());
    let cip = format!("{xglibc}/include:{{root}}/kh");
    // Staging prefix for the static zlib built below; elfutils links against it.
    let zstage = "{root}/zstage";

    // elfutils' libelf includes glibc's <sys/*> → <linux/*>; overlay the x86_64
    // UAPI headers like make-x86-64 does.
    let mut steps = unpack_into("elfutils-x86-64-source", "{src}");
    steps.extend(unpack_keep_top("linux-headers-x86-64", "{root}/kh"));
    steps.extend(unpack_into("zlib-x86-64-source", "{root}/zsrc"));
    steps.push(Step::PatchShebangs {
        dir: "{src}".into(),
        shell: SH.into(),
    });

    // Base static wrapper around the native gcc (make-x86-64 shape): -static, -B
    // at the x86_64 glibc lib for crt*.o + libc.a.
    steps.push(Step::WriteFile {
        path: "{root}/wb/cc".into(),
        content: format!("#!{SH}\nexec \"{ngcc}\" -static -B{xglibc}/lib \"$@\"\n"),
        exec: true,
    });
    // elfutils wrapper: base + the staged zlib headers/lib so configure's
    // AC_SEARCH_LIBS(gzdirect,z) and libelf's #include <zlib.h> resolve.
    steps.push(Step::WriteFile {
        path: "{root}/wb/cc-elf".into(),
        content: format!(
            "#!{SH}\nexec \"{ngcc}\" -static -B{xglibc}/lib -I{zstage}/include -L{zstage}/lib \"$@\"\n"
        ),
        exec: true,
    });

    // 1) Static zlib (libz.a + headers) → {zstage}. zlib's hand-written configure
    //    honours CC from the env; --static builds only the archive. Copy the lib
    //    + zlib.h/zconf.h (zconf.h is configure-generated) by hand to avoid the
    //    install target running example programs.
    steps.push(Step::MkDir {
        path: format!("{zstage}/lib"),
    });
    steps.push(Step::MkDir {
        path: format!("{zstage}/include"),
    });
    steps.push(
        Step::run(
            "{root}/zsrc",
            &[SH, "./configure", &format!("--prefix={zstage}"), "--static"],
        )
        .env("PATH", &path)
        .env("CONFIG_SHELL", SH)
        .env("SHELL", SH)
        .env("CC", "{root}/wb/cc")
        .env("C_INCLUDE_PATH", &cip),
    );
    steps.push(
        Step::run(
            "{root}/zsrc",
            &[
                "{in:make-x86-64}/bin/make",
                "-j{jobs}",
                "libz.a",
                &format!("SHELL={SH}"),
                &format!("CONFIG_SHELL={SH}"),
            ],
        )
        .env("PATH", &path)
        .env("CC", "{root}/wb/cc")
        .env("C_INCLUDE_PATH", &cip),
    );
    steps.push(Step::CopyFiles {
        files: vec!["{root}/zsrc/libz.a".into()],
        dest: format!("{zstage}/lib"),
    });
    steps.push(Step::CopyFiles {
        files: vec![
            "{root}/zsrc/zlib.h".into(),
            "{root}/zsrc/zconf.h".into(),
        ],
        dest: format!("{zstage}/include"),
    });

    // 2) Configure elfutils for a minimal libelf-only build (no debuginfod, no
    //    compression backends, no NLS, no demangler → no libstdc++/pkg-config).
    //
    //    ac_cv_tls=yes: elfutils' `__thread support` probe deliberately links its
    //    conftest with `$dso_LDFLAGS` ("the same flags we use for our DSOs" —
    //    -shared -Wl,-z,defs). Our `cc-elf` wrapper hardcodes -static (needed so
    //    elfutils' AC_RUN_IFELSE probes produce loader-free static binaries the
    //    host-free sandbox can execute), and -static + -shared is a contradictory
    //    link that fails, so the probe reports "no" and configure aborts with
    //    "__thread support required" — even though GCC 14 + glibc 2.41 fully
    //    support TLS (libelf's own elf_errno IS __thread and links + runs fine).
    //    Assert the genuinely-true capability via autoconf's cache override rather
    //    than drop -static (which would break the run-probes). re #529.
    steps.push(
        Step::run(
            "{src}",
            &[
                SH,
                "./configure",
                "--build=x86_64-pc-linux-gnu",
                "--host=x86_64-pc-linux-gnu",
                "--prefix=/td/store/elfutils-0.192-x86-64",
                // A one-shot static-archive build has no use for automake's
                // per-object .deps tracking, and its config.status bootstrap is the
                // step that dies without make on PATH; disable it (as flex does).
                "--disable-dependency-tracking",
                "--disable-debuginfod",
                "--disable-libdebuginfod",
                "--without-bzlib",
                "--without-lzma",
                "--without-zstd",
                "--disable-nls",
                "--disable-demangler",
            ],
        )
        .env("PATH", &path)
        .env("CONFIG_SHELL", SH)
        .env("SHELL", SH)
        .env("CC", "{root}/wb/cc-elf")
        .env("C_INCLUDE_PATH", &cip)
        .env("ac_cv_tls", "yes"),
    );
    // 3) Build ONLY libeu.a then libelf.a (libelf link-depends on libeu; build in
    //    that order). CFLAGS='-O2 -Wno-error' keeps -O2 while demoting elfutils'
    //    default -Werror, so a GCC-14 warning the upstream GCC did not emit can't
    //    fail the archive (AM_CFLAGS' -std=gnu99/-D_GNU_SOURCE still apply).
    for sub in ["lib", "libelf"] {
        let target = if sub == "lib" { "libeu.a" } else { "libelf.a" };
        steps.push(
            Step::run(
                "{src}",
                &[
                    "{in:make-x86-64}/bin/make",
                    "-j{jobs}",
                    "-C",
                    sub,
                    target,
                    "CFLAGS=-O2 -Wno-error",
                    "SHELL={in:bash-mesboot}/bin/bash",
                    "CONFIG_SHELL={in:bash-mesboot}/bin/bash",
                ],
            )
            .env("PATH", &path)
            .env("CONFIG_SHELL", SH)
            .env("SHELL", SH)
            .env("C_INCLUDE_PATH", &cip),
        );
    }

    // 4) Collect the static archives + libelf headers into {out}. libeu.a is a
    //    noinst archive with no install target, so copy every artifact by hand.
    steps.push(Step::MkDir {
        path: "{out}/lib".into(),
    });
    steps.push(Step::MkDir {
        path: "{out}/include".into(),
    });
    steps.push(Step::CopyFiles {
        files: vec![
            "{src}/libelf/libelf.a".into(),
            "{src}/lib/libeu.a".into(),
            format!("{zstage}/lib/libz.a"),
        ],
        dest: "{out}/lib".into(),
    });
    steps.push(Step::CopyFiles {
        files: vec![
            "{src}/libelf/libelf.h".into(),
            "{src}/libelf/gelf.h".into(),
            "{src}/libelf/nlist.h".into(),
            format!("{zstage}/include/zlib.h"),
            format!("{zstage}/include/zconf.h"),
        ],
        dest: "{out}/include".into(),
    });
    steps.push(Step::Require {
        paths: vec![
            "{out}/lib/libelf.a".into(),
            "{out}/lib/libeu.a".into(),
            "{out}/lib/libz.a".into(),
            "{out}/include/libelf.h".into(),
            "{out}/include/gelf.h".into(),
        ],
        exec: false,
    });
    // [native-arch] the archives must hold NATIVE x86_64 objects (not i686/cross):
    // extract one libelf.a member and assert its ELF machine, parity with
    // make-x86-64 / flex-x86-64.
    steps.push(Step::MkDir {
        path: "{root}/archcheck".into(),
    });
    steps.push(
        Step::run(
            "{root}/archcheck",
            &[
                SH,
                "-c",
                "'{in:binutils-x86-64-native}/bin/ar' x '{out}/lib/libelf.a'; \
                 o=$(ls *.o 2>/dev/null | head -n1); \
                 [ -n \"$o\" ] || { echo 'libelf.a contains no objects' >&2; exit 1; }; \
                 h=$('{in:binutils-x86-64-native}/bin/readelf' -h \"$o\"); \
                 printf '%s\\n' \"$h\" | grep -i 'machine:' | grep -qi 'x86-64' || { echo 'libelf.a objects are not x86-64' >&2; exit 1; }",
            ],
        )
        .env("PATH", &mesboot0_path()),
    );

    Recipe::mesboot("elfutils-x86-64", "0.192")
        .source_input("elfutils-x86-64-source")
        .native_inputs(&[
            "gcc-x86-64-native",
            "binutils-x86-64-native",
            "glibc-x86-64",
            "make-x86-64",
        ])
        .inputs_owned(mesboot0_inputs(&[
            "zlib-x86-64-source",
            "linux-headers-x86-64",
        ]))
        .steps(steps)
}

use crate::ladder::{mesboot0_inputs, mesboot0_path, unpack_into, unpack_keep_top, SH};
use crate::types::{Recipe, Step};

// GNU flex 2.6.4 — the fast lexical-analyzer generator the modern Linux kernel
// (>= 4.18) REQUIRES to build: scripts/kconfig's lexer is generated from
// `lexer.l` by flex during the build, and the pre-generated `*_shipped` parsers
// that let the 4.14 rung dodge flex/bison are GONE from the current tree
// (re #529). Built FROM SOURCE by td's OWN native x86_64 toolchain —
// gcc-x86-64-native (GCC 14.3.0) + binutils-x86-64-native (2.44) — driven by the
// td-built make-x86-64, exactly the make-x86-64/busybox-x86-64 shape: STATIC
// against the /td/store x86_64 glibc 2.41, so the output has no ELF interpreter
// and runs in any build sandbox with no glibc staging.
//
// The pristine release tarball ships the generated scanner (`src/scan.c`) and
// parser (`src/parse.c`/`parse.h`), and td's Unpack preserves the archived
// mtimes, so the `%.c: %.l`(flex) / `%.c: %.y`(bison) regeneration rules never
// fire — flex bootstraps with NO pre-existing flex and NO bison, only cc + make
// (+ m4; see below). This is the same "shipped generated sources + preserved
// mtimes" property the bison-mesboot / m4-mesboot rungs rely on.
//
// m4 at RUNTIME: flex expands its skeleton (flex.skl) through m4 every time it
// generates a scanner, so the kernel's kconfig-lexer build execs m4. The
// absolute path to td's m4 (m4-mesboot, a stable content-addressed store path)
// is baked in at configure via M4=, exactly as bison-mesboot bakes it — so flex
// finds m4 with no PATH search or wrapper at kernel-build time. m4-mesboot is a
// static i686 binary; it runs fine on the x86_64 host. Its behavior is validated
// by the sibling `flex-x86-64-test` rung (which runs the built flex). re #469.
pub fn recipe() -> Recipe {
    let ngcc = "{in:gcc-x86-64-native}/stage/td/store/gcc-14.3.0-x86_64-native/bin/gcc";
    let xglibc = "{in:glibc-x86-64}/stage/td/store/glibc-2.41-x86_64";
    let nbin = "{in:binutils-x86-64-native}/bin";
    // make-x86-64 MUST be on PATH, not merely invoked by absolute path: flex's
    // Makefiles recurse via `$(MAKE)` (Makefile `all-recursive`), and autoconf's
    // AC_PROG_MAKE_SET probe at configure time runs a bare `make`. With no `make`
    // on PATH the probe fails, autoconf bakes `MAKE = make` into the generated
    // Makefiles, and `all-recursive` then execs a bare `make` that the host-free
    // sandbox cannot resolve ("make: command not found", Makefile:533). The kernel
    // rung already puts make-x86-64 on PATH for the same reason; flex did not,
    // which broke its (only ever cold) build. re #529.
    let path = format!("{nbin}:{{in:make-x86-64}}/bin:{}", mesboot0_path());
    // glibc 2.41 headers + the x86_64 kernel UAPI headers (flex is pure C; the
    // libstdc++ #include_next hazard that bars C_INCLUDE_PATH for g++ does not
    // apply). Mirrors make-x86-64.
    let cip = format!("{xglibc}/include:{{root}}/kh");
    let m4 = "{in:m4-mesboot}/bin/m4";

    let mut steps = unpack_into("flex-x86-64-source", "{src}");
    // kernel headers keep-top: the tarball top level is {linux,asm,…} → {root}/kh/*.
    steps.extend(unpack_keep_top("linux-headers-x86-64", "{root}/kh"));
    // Retarget every `#! /bin/sh` shebang (missing/config.status/mkskel.sh) to the
    // declared shell — the host-free sandbox has no /bin/sh. Must precede configure.
    steps.push(Step::PatchShebangs {
        dir: "{src}".into(),
        shell: SH.into(),
    });
    // STATIC wrapper around the NATIVE gcc, port of make-x86-64's: -static (no
    // interp, sandbox-runnable), -B at the x86_64 glibc lib for crt*.o + libc.a.
    steps.push(Step::WriteFile {
        path: "{root}/wb/cc".into(),
        content: format!("#!{SH}\nexec \"{ngcc}\" -static -B{xglibc}/lib \"$@\"\n"),
        exec: true,
    });
    steps.push(
        Step::run(
            "{src}",
            &[
                SH,
                "./configure",
                "--build=x86_64-pc-linux-gnu",
                "--host=x86_64-pc-linux-gnu",
                "--prefix=/td/store/flex-2.6.4-x86_64",
                "--disable-dependency-tracking",
                "--disable-nls",
                // No shared libfl.so: the kernel needs only the `flex` binary, and
                // a static-only build keeps the closure to glibc.
                "--disable-shared",
            ],
        )
        .env("PATH", &path)
        .env("CONFIG_SHELL", SH)
        .env("SHELL", SH)
        .env("CC", "{root}/wb/cc")
        .env("CC_FOR_BUILD", "{root}/wb/cc")
        .env("C_INCLUDE_PATH", &cip)
        // Bake the absolute path to td's m4 (flex execs it to expand flex.skl at
        // scanner-generation time); {in:m4-mesboot} is a stable store path.
        .env("M4", m4),
    );
    steps.push(
        Step::run(
            "{src}",
            &[
                "{in:make-x86-64}/bin/make",
                "-j{jobs}",
                "SHELL={in:bash-mesboot}/bin/bash",
                "CONFIG_SHELL={in:bash-mesboot}/bin/bash",
                // flex builds nothing from texinfo/help2man that the kernel needs;
                // stub the doc generators so a missing makeinfo/help2man can't fail
                // the build (the shipped doc/flex.1 + flex.info are used as-is).
                "MAKEINFO=true",
                "HELP2MAN=true",
            ],
        )
        .env("PATH", &path)
        .env("CONFIG_SHELL", SH)
        .env("SHELL", SH)
        .env("C_INCLUDE_PATH", &cip),
    );
    steps.push(
        Step::run(
            "{src}",
            &[
                "{in:make-x86-64}/bin/make",
                "SHELL={in:bash-mesboot}/bin/bash",
                "MAKEINFO=true",
                "HELP2MAN=true",
                "install",
                "prefix={out}",
            ],
        )
        .env("PATH", &path)
        .env("CONFIG_SHELL", SH)
        .env("SHELL", SH),
    );
    steps.push(Step::Require {
        paths: vec!["{out}/bin/flex".into()],
        exec: true,
    });
    // [native-arch] the produced flex is an ELF64 x86_64 binary — a NATIVE build
    // artifact — asserted by the interned native readelf, parity with
    // make-x86-64 / busybox-x86-64.
    steps.push(
        Step::run(
            "{out}",
            &[
                SH,
                "-c",
                "h=$('{in:binutils-x86-64-native}/bin/readelf' -h '{out}/bin/flex'); \
                 printf '%s\\n' \"$h\" | grep -i 'class:'   | grep -qi 'ELF64'  || { echo 'flex is not ELF64' >&2; exit 1; }; \
                 printf '%s\\n' \"$h\" | grep -i 'machine:' | grep -qi 'x86-64' || { echo 'flex is not x86-64' >&2; exit 1; }",
            ],
        )
        .env("PATH", &mesboot0_path()),
    );
    Recipe::mesboot("flex-x86-64", "2.6.4")
        .source_input("flex-x86-64-source")
        .native_inputs(&[
            "gcc-x86-64-native",
            "binutils-x86-64-native",
            "glibc-x86-64",
            "make-x86-64",
            "m4-mesboot",
        ])
        .inputs_owned(mesboot0_inputs(&["linux-headers-x86-64"]))
        .steps(steps)
}

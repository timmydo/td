use crate::ladder::{
    SH, mesboot0_inputs, mesboot0_path, relocate_ld_scripts, sed_i_mesboot0, unpack_into,
    unpack_keep_top,
};
use crate::types::{Recipe, Step};

// glibc 2.41 for x86_64 (#378 slice 4, guix's cross glibc): the MODERN shared
// libc, cross-compiled by the stage1 cross-gcc. CC = x86_64-pc-linux-gnu-gcc
// (from the stage1 bin on PATH), BUILD_CC = the i686 gcc-14 wrapper (the
// build-time helpers run on i686). --host=x86_64 --build=i686. Produces a SHARED
// x86_64 libc: ld-linux-x86-64.so.2 + libc.so.6, at /td/store/glibc-2.41-x86_64.
// DESTDIR={out}/stage. Relocate the ld scripts to bare names + overlay the
// kernel UAPI headers into the staged include (a --sysroot corpus build needs
// <linux/*>). native_inputs: gcc-x86-64-stage1 (the cross CC), gcc-14 +
// glibc-mesboot (the i686 static BUILD_CC wrapper), binutils-x86-64 (the cross
// as/ld).
//
// Host-tool ingress closed (re #469): the i686 build tools glibc's
// configure/make invoke are the td-built gcc-14-tier providers — bison-mesboot,
// m4-mesboot, python-mesboot, gawk-mesboot (3.1.8, glibc needs gawk >= 3.1.2),
// and make-441 (GNU Make 4.4.1, glibc's critical make >= 4.0 gate) — over the
// mesboot0 scripting userland. `flex` is dropped: glibc's build never invokes
// lex/flex, so it was pure phantom host ingress. The static-BUILD_CC helpers need
// no runtime lib path, but python-mesboot is DYNAMIC against the shared glibc
// 2.16 (a fully-static CPython is the finicky dlopen/NSS path), so this rung
// additionally declares glibc-mesboot-shared and sets LD_LIBRARY_PATH to its lib
// on every step that may run python3 (gen-as-const.py during make; configure's
// python probe) — the only dynamic build tool; the static ones ignore it.
pub fn recipe() -> Recipe {
    let xgccbin = "{in:gcc-x86-64-stage1}/stage/td/store/gcc-14.3.0-x86_64/bin";
    let path = format!("{xgccbin}:{{in:binutils-x86-64}}/bin:{}", mesboot0_path());
    let stage = "{out}/stage/td/store/glibc-2.41-x86_64";
    let lp = "{in:glibc-mesboot-shared}/lib";
    let mut steps = unpack_into("glibc-x86-64-source", "{src}");
    steps.extend(unpack_keep_top("linux-headers-x86-64", "{root}/kh"));
    steps.push(Step::ToolFarm {
        links: vec![
            ("awk".into(), "{in:gawk-mesboot}/bin/awk".into()),
            ("gawk".into(), "{in:gawk-mesboot}/bin/gawk".into()),
            ("bison".into(), "{in:bison-mesboot}/bin/bison".into()),
            ("m4".into(), "{in:m4-mesboot}/bin/m4".into()),
            ("make".into(), "{in:make-441}/bin/make".into()),
            ("python3".into(), "{in:python-mesboot}/bin/python3".into()),
        ],
    });
    // the i686 BUILD_CC: static gcc-14 vs glibc-mesboot (build-time helpers are i686).
    steps.push(Step::WriteFile {
        path: "{root}/wb/build-cc".into(),
        content: format!(
            "#!{SH}\nexec \"{{in:gcc-14}}/stage/td/store/gcc-14.3.0/bin/gcc\" -static -idirafter {{in:glibc-mesboot}}/include -B{{in:glibc-mesboot}}/lib \"$@\"\n"
        ),
        exec: true,
    });
    steps.push(Step::PatchShebangs {
        dir: "{src}".into(),
        shell: SH.into(),
    });
    steps.push(sed_i_mesboot0(
        "s,^SHELL := /bin/sh,SHELL := {in:bash-mesboot}/bin/bash,",
        &["Makeconfig"],
    ));
    // gen-as-const.py -> scripts/glibcextract.py shells the compiler through
    // Python `subprocess.check_call(cmd, shell=True)` (the cmd uses a `< file`
    // redirect, so a shell is required). CPython hardcodes /bin/sh for shell=True
    // and ignores SHELL/CONFIG_SHELL/PatchShebangs, but the host-free sandbox has
    // no /bin/sh — so pin that subprocess shell to the declared bash-mesboot via
    // `executable=` (re #469; both call sites, lines 63/93 upstream).
    steps.push(sed_i_mesboot0(
        "s|subprocess\\.check_call(cmd, shell=True)|subprocess.check_call(cmd, shell=True, executable=\"{in:bash-mesboot}/bin/bash\")|g",
        &["scripts/glibcextract.py"],
    ));
    steps.push(Step::MkDir {
        path: "{src}/bld".into(),
    });
    steps.push(
        Step::run(
            "{src}/bld",
            &[
                SH,
                "../configure",
                "--prefix=/td/store/glibc-2.41-x86_64",
                "--build=i686-pc-linux-gnu",
                "--host=x86_64-pc-linux-gnu",
                "--with-headers={root}/kh",
                "--enable-kernel=3.2.0",
                "--disable-werror",
                "--disable-nscd",
                "--with-binutils={in:binutils-x86-64}/x86_64-pc-linux-gnu/bin",
                "libc_cv_slibdir=/td/store/glibc-2.41-x86_64/lib",
            ],
        )
        .env("PATH", &path)
        .env("CONFIG_SHELL", SH)
        .env("SHELL", SH)
        .env("CC", "x86_64-pc-linux-gnu-gcc")
        .env("BUILD_CC", "{root}/wb/build-cc")
        .env("AR", "x86_64-pc-linux-gnu-ar")
        .env("RANLIB", "x86_64-pc-linux-gnu-ranlib")
        .env("LD_LIBRARY_PATH", lp),
    );
    steps.push(
        Step::run(
            "{src}/bld",
            &[
                "{in:make-441}/bin/make",
                "-j{jobs}",
                "SHELL={in:bash-mesboot}/bin/bash",
                "CONFIG_SHELL={in:bash-mesboot}/bin/bash",
            ],
        )
        .env("PATH", &path)
        .env("CONFIG_SHELL", SH)
        .env("SHELL", SH)
        .env("LD_LIBRARY_PATH", lp),
    );
    steps.push(
        Step::run(
            "{src}/bld",
            &[
                "{in:make-441}/bin/make",
                "SHELL={in:bash-mesboot}/bin/bash",
                "install",
                "DESTDIR={out}/stage",
            ],
        )
        .env("PATH", &path)
        .env("CONFIG_SHELL", SH)
        .env("SHELL", SH)
        .env("LD_LIBRARY_PATH", lp),
    );
    // Relocate every GNU ld script under lib/*.so from absolute member paths to
    // bare names (ld finds them via -L); real ELFs and absent scripts are skipped.
    steps.push(relocate_ld_scripts(stage, "/td/store/glibc-2.41-x86_64"));
    // kernel UAPI headers into the staged include dir (glibc-installed headers
    // win collisions, so copy the kernel trees subdir-wise).
    for kd in [
        "linux",
        "asm",
        "asm-generic",
        "mtd",
        "rdma",
        "scsi",
        "sound",
        "video",
        "xen",
        "drm",
        "misc",
    ] {
        steps.push(Step::CopyTree {
            from: format!("{{root}}/kh/{kd}"),
            dest: format!("{stage}/include/{kd}"),
        });
    }
    steps.push(Step::Require {
        paths: vec![
            format!("{stage}/lib/libc.so.6"),
            format!("{stage}/lib/ld-linux-x86-64.so.2"),
            format!("{stage}/include/linux/limits.h"),
        ],
        exec: false,
    });
    Recipe::mesboot("glibc-x86-64", "2.41")
        .source_input("glibc-241-source")
        .native_inputs(&[
            "gcc-x86-64-stage1",
            "gcc-14",
            "glibc-mesboot",
            "glibc-mesboot-shared",
            "binutils-x86-64",
            "gawk-mesboot",
            "bison-mesboot",
            "m4-mesboot",
            "python-mesboot",
            "make-441",
        ])
        .inputs_owned(mesboot0_inputs(&["linux-headers-x86-64"]))
        .steps(steps)
}

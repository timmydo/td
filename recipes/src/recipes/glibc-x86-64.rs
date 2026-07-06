use crate::ladder::{base_inputs, base_path, sed_i, unpack_into, unpack_keep_top, SH};
use crate::types::{Recipe, Step};

// glibc 2.41 for x86_64 (#378 slice 4, guix's cross glibc): the MODERN shared
// libc, cross-compiled by the stage1 cross-gcc. CC = x86_64-pc-linux-gnu-gcc
// (from the stage1 bin on PATH), BUILD_CC = the i686 gcc-14 wrapper (the
// build-time helpers run on i686). --host=x86_64 --build=i686. Produces a SHARED
// x86_64 libc: ld-linux-x86-64.so.2 + libc.so.6, at /td/store/glibc-2.41-x86_64.
// DESTDIR={out}/stage. Relocate the ld scripts to bare names + overlay the
// kernel UAPI headers into the staged include (a --sysroot corpus build needs
// <linux/*>). native_inputs: gcc-x86-64-stage1 (the cross CC), gcc-14 +
// glibc-mesboot (the i686 BUILD_CC wrapper), binutils-x86-64 (the cross as/ld).
pub fn recipe() -> Recipe {
    let xgccbin = "{in:gcc-x86-64-stage1}/stage/td/store/gcc-14.3.0-x86_64/bin";
    let path = format!("{xgccbin}:{{in:binutils-x86-64}}/bin:{}", base_path());
    let stage = "{out}/stage/td/store/glibc-2.41-x86_64";
    let mut steps = unpack_into("glibc-x86-64-source", "{src}");
    steps.extend(unpack_keep_top("linux-headers-x86-64", "{root}/kh"));
    steps.push(Step::ToolFarm {
        links: vec![
            ("awk".into(), "{in:gawk}/bin/awk".into()),
            ("gawk".into(), "{in:gawk}/bin/gawk".into()),
            ("bison".into(), "{in:bison}/bin/bison".into()),
            ("flex".into(), "{in:flex}/bin/flex".into()),
            ("m4".into(), "{in:m4}/bin/m4".into()),
            ("make".into(), "{in:make}/bin/make".into()),
            ("python3".into(), "{in:python}/bin/python3".into()),
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
    steps.push(sed_i(
        "s,^SHELL := /bin/sh,SHELL := {in:bash}/bin/bash,",
        &["Makeconfig"],
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
        .env("RANLIB", "x86_64-pc-linux-gnu-ranlib"),
    );
    steps.push(
        Step::run(
            "{src}/bld",
            &[
                "{in:make}/bin/make",
                "-j{jobs}",
                "SHELL={in:bash}/bin/bash",
                "CONFIG_SHELL={in:bash}/bin/bash",
            ],
        )
        .env("PATH", &path)
        .env("CONFIG_SHELL", SH)
        .env("SHELL", SH),
    );
    steps.push(
        Step::run(
            "{src}/bld",
            &[
                "{in:make}/bin/make",
                "SHELL={in:bash}/bin/bash",
                "install",
                "DESTDIR={out}/stage",
            ],
        )
        .env("PATH", &path)
        .env("CONFIG_SHELL", SH)
        .env("SHELL", SH),
    );
    // relocate the GNU ld scripts' absolute member paths to bare names (ld finds
    // them via -L) — libc.so/libm.so are ld-scripts in 2.41.
    steps.push(sed_i(
        "s,/td/store/glibc-2.41-x86_64/lib/,,g",
        &[&format!("{stage}/lib/libc.so"), &format!("{stage}/lib/libm.so")],
    ));
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
        .native_inputs(&["gcc-x86-64-stage1", "gcc-14", "glibc-mesboot", "binutils-x86-64"])
        .inputs_owned(base_inputs(&[
            "glibc-x86-64-source",
            "linux-headers-x86-64",
            "flex",
            "bison",
            "m4",
            "make",
            "python",
        ]))
        .steps(steps)
}

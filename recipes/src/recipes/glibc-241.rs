use crate::ladder::{
    SH, base_inputs, base_path, link_bins, relocate_ld_scripts, sed_i, unpack_into, unpack_keep_top,
};
use crate::types::{Recipe, Step};

// glibc 2.41 — rung 20, the top (#378, guix's glibc-final): gcc 14.3.0 +
// binutils 2.44 build the MODERN shared libc against the kernel headers. CC
// bakes only the shared glibc-2.16 interp (glibc 2.41 forbids DT_RPATH AND
// DT_RUNPATH in libc.so.6 — no -rpath); build tools find 2.16 via
// LD_LIBRARY_PATH. --prefix=/td/store/glibc-2.41 + DESTDIR={out}/stage (the
// chain-tail stage shape). The deleted chain's FINALIZE moves in here as
// steps: the ld-script relocation (strip the configure prefix to bare names)
// and the kernel-header overlay into the staged include dir.
pub fn recipe() -> Recipe {
    let path = format!("{{in:binutils-244}}/bin:{}", base_path());
    let stage = "{out}/stage/td/store/glibc-2.41";
    let mut steps = unpack_into("glibc-241-source", "{src}");
    steps.extend(unpack_keep_top("linux-headers", "{root}/kh"));
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
    steps.push(
        link_bins("binutils-244"),
    );
    steps.push(Step::WriteFile {
        path: "{root}/wb/gcc".into(),
        content: format!(
            "#!{SH}\nexec \"{{in:gcc-14}}/stage/td/store/gcc-14.3.0/bin/gcc\" -B{{in:glibc-mesboot-shared}}/lib -L{{in:glibc-mesboot-shared}}/lib -isystem {{in:glibc-mesboot-shared}}/include -static-libgcc -Wl,--dynamic-linker -Wl,{{in:glibc-mesboot-shared}}/lib/ld-linux.so.2 \"$@\"\n"
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
                "--prefix=/td/store/glibc-2.41",
                "--build=i686-pc-linux-gnu",
                "--host=i686-unknown-linux-gnu",
                "--with-headers={root}/kh",
                "--enable-kernel=3.2.0",
                "--disable-werror",
                "--disable-nscd",
                "--with-binutils={in:binutils-244}/bin",
                "libc_cv_slibdir=/td/store/glibc-2.41/lib",
            ],
        )
        .env("PATH", &path)
        .env("CONFIG_SHELL", SH)
        .env("SHELL", SH)
        .env("CC", "{root}/wb/gcc")
        .env("LD_LIBRARY_PATH", "{in:glibc-mesboot-shared}/lib"),
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
        .env("SHELL", SH)
        .env("LD_LIBRARY_PATH", "{in:glibc-mesboot-shared}/lib"),
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
        .env("SHELL", SH)
        .env("LD_LIBRARY_PATH", "{in:glibc-mesboot-shared}/lib"),
    );
    // FINALIZE (was bootstrap_modern_toolchain's brick-8 epilogue): relocate
    // every GNU ld script under lib/*.so from absolute member paths to bare
    // names (ld finds them via -L); real ELFs and absent scripts are skipped.
    steps.push(relocate_ld_scripts(stage, "/td/store/glibc-2.41"));
    // kernel UAPI headers into the staged include dir (a --sysroot corpus build
    // needs <linux/*>); the glibc-installed headers win collisions, so copy the
    // kernel trees only where absent — CopyTree overwrites, hence subdir-wise.
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
            format!("{stage}/lib/ld-linux.so.2"),
            format!("{stage}/include/linux/limits.h"),
        ],
        exec: false,
    });
    Recipe::mesboot("glibc-241", "2.41")
        .native_inputs(&["gcc-14", "glibc-mesboot-shared", "binutils-244"])
        .inputs_owned(base_inputs(&["linux-headers", "flex", "bison", "m4", "make", "python"]))
        .steps(steps)
}

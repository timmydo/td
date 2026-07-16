use crate::ladder::{
    SH, link_bins_mesboot0, mesboot0_inputs, mesboot0_path, relocate_ld_scripts, sed_i_mesboot0,
    unpack_into, unpack_keep_top,
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
//
// Host-tool ingress closed (re #469): the build tools glibc's configure/make
// need are the td-built gcc-14-tier providers — bison-mesboot, m4-mesboot,
// python-mesboot (dynamic vs glibc-mesboot-shared, run via the LD_LIBRARY_PATH
// this build already sets), gawk-mesboot (3.1.8, glibc needs gawk >= 3.1.2),
// and make-441 (GNU Make 4.4.1, glibc's critical make >= 4.0 gate) — with the
// mesboot0 scripting userland (mesboot0_path/mesboot0_inputs) and the
// binutils-244 link_bins_mesboot0 farm. `flex` is dropped: glibc's build never
// invokes lex/flex, so it was pure phantom host ingress.
pub fn recipe() -> Recipe {
    let path = format!("{{in:binutils-244}}/bin:{}", mesboot0_path());
    let stage = "{out}/stage/td/store/glibc-2.41";
    let mut steps = unpack_into("glibc-241-source", "{src}");
    steps.extend(unpack_keep_top("linux-headers", "{root}/kh"));
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
    steps.push(link_bins_mesboot0("binutils-244"));
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
    steps.push(sed_i_mesboot0(
        "s,^SHELL := /bin/sh,SHELL := {in:bash-mesboot}/bin/bash,",
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
                "{in:make-441}/bin/make",
                "-j{jobs}",
                "SHELL={in:bash-mesboot}/bin/bash",
                "CONFIG_SHELL={in:bash-mesboot}/bin/bash",
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
                "{in:make-441}/bin/make",
                "SHELL={in:bash-mesboot}/bin/bash",
                "install",
                "DESTDIR={out}/stage",
            ],
        )
        .env("PATH", &path)
        .env("CONFIG_SHELL", SH)
        .env("SHELL", SH)
        .env("LD_LIBRARY_PATH", "{in:glibc-mesboot-shared}/lib"),
    );
    // FINALIZE (was the shell bootstrap chain's brick-8 epilogue): relocate
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
        .source_input("glibc-241-source")
        .native_inputs(&[
            "gcc-14",
            "glibc-mesboot-shared",
            "binutils-244",
            "gawk-mesboot",
            "bison-mesboot",
            "m4-mesboot",
            "python-mesboot",
            "make-441",
        ])
        .inputs_owned(mesboot0_inputs(&["linux-headers"]))
        .steps(steps)
}

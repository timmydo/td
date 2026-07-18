use crate::ladder::{SH, apply_patch, link_bins, mesboot0_inputs, mesboot0_path, sed_i, unpack_into, unpack_keep_top};
use crate::types::{Recipe, Step, TextEdit};

// glibc 2.16.0 SHARED — rung 17 (#378): the runtime libc dynamic /td/store
// binaries load (libc.so.6 + ld-linux.so.2). Single-stage configure exactly as
// guix's glibc-mesboot phases (the two-stage variant defeated the boot patch's
// sunrpc un-hiding); the nis subdir ships no libs (guix-as-oracle: no
// libnsl.so); build TOOLS take <rpc/types.h> from the tree's own sunrpc.
pub fn recipe() -> Recipe {
    let path = format!("{{in:gcc-mesboot1}}/bin:{}", mesboot0_path());
    let btinc = "{src}/sunrpc:{in:glibc-mesboot0}/include:{root}/kh";
    let btlib = "{in:glibc-mesboot0}/lib";
    let cc = "{in:gcc-mesboot1}/bin/gcc -I {src}/nptl/sysdeps/pthread/bits -D BOOTSTRAP_GLIBC=1 -L {src} -L {in:glibc-mesboot0}/lib";
    let cpp = "{in:gcc-mesboot1}/bin/gcc -E -I {src}/nptl/sysdeps/pthread/bits -D BOOTSTRAP_GLIBC=1";
    let mut steps = unpack_into("glibc-mesboot-shared-source", "{src}");
    steps.push(apply_patch("patch-mesboot", "patch-glibc-boot-2.16.0"));
    steps.push(apply_patch("patch-mesboot", "patch-glibc-bootstrap-system-2.16.0"));
    steps.extend(unpack_keep_top("linux-headers", "{root}/kh"));
    steps.push(Step::ToolFarm {
        links: vec![
            ("gcc".into(), "{in:gcc-mesboot1}/bin/gcc".into()),
            ("cpp".into(), "{in:gcc-mesboot1}/bin/cpp".into()),
            ("make".into(), "{in:make-mesboot}/bin/make".into()),
            ("patch".into(), "{in:patch-mesboot}/bin/patch".into()),
            ("awk".into(), "{in:gawk-mesboot}/bin/gawk".into()),
            ("gawk".into(), "{in:gawk-mesboot}/bin/gawk".into()),
        ],
    });
    steps.push(link_bins("binutils-mesboot"));
    steps.push(sed_i(
        "s,\\${vdso_symver//\\./_},$(echo $vdso_symver | sed -e \"s/\\\\./_/g\"),",
        &["sysdeps/unix/make-syscalls.sh"],
    ));
    steps.push(sed_i("s,de\\.po,en_GB.po,", &["catgets/Makefile", "intl/Makefile"]));
    steps.push(sed_i("s,/bin/pwd,pwd,", &["configure"]));
    steps.push(sed_i(
        "/^others *+= *nscd/d; /^others-pie *+= *nscd/d; /^install-sbin *:= *nscd/d",
        &["nscd/Makefile"],
    ));
    steps.push(sed_i(
        "s/^extra-libs[[:space:]]*=.*/extra-libs =/; s/^extra-libs-others[[:space:]]*=.*/extra-libs-others =/",
        &["nis/Makefile"],
    ));
    steps.push(sed_i("s/wctype manual shadow/wctype shadow/", &["Makeconfig"]));
    steps.push(sed_i(
        "s,^SHELL := /bin/sh,SHELL := {in:bash-mesboot}/bin/bash,",
        &["Makeconfig"],
    ));
    // elf/Makefile asks the shell to expand build/*/stamp.os while generating
    // librtld.mk. bash-mesboot cannot expand that non-terminal glob and passes
    // the literal to fgrep, which emits an error line into librtld.mk and makes
    // rtld-Rules fail with "missing separator". Have GNU make expand the same
    // glob first; its wildcard function drops non-matches and preserves the
    // exact list of existing stamp files without introducing a host tool.
    steps.push(Step::substitute_text(
        "{src}/elf/Makefile",
        vec![TextEdit::new(
            "$(common-objpfx)*/stamp.os",
            "$(wildcard $(common-objpfx)*/stamp.os)",
            1,
        )],
    ));
    // Host-gzip-free locale charmap install (re #469), the same fix glibc-mesboot0
    // and glibc-mesboot carry. This shared build runs the identical glibc 2.16.0
    // `make install`, whose localedata/Makefile installs each charmap uncompressed
    // to its raw name ($(INSTALL_DATA) $< $(@:.gz=)) then `gzip -9n`-s it onto the
    // `.gz` target. td ships no gzip executable (the builder decompresses
    // in-process), so drop the compress step to a no-op: the charmap stays under
    // its raw, unsuffixed name, which glibc's charmap_open reads directly, and the
    // `.gz` make target is bookkeeping only. Do NOT install raw bytes under the
    // `.gz` suffix -- localedef's charmap-dir.c treats `.gz` as gzip and would
    // spawn gzip to decode it, a malformed output. The bootstrap never reads these
    // charmaps.
    steps.push(Step::substitute_text(
        "{src}/localedata/Makefile",
        vec![TextEdit::new("\tgzip -9n $(@:.gz=)", "\ttrue", 1)],
    ));
    // csu/Makefile's version-info.h banner rule shells out to host uname/date
    // and can read /proc/version; coreutils-mesboot0 ships no uname/date, so pin
    // all three to deterministic tool-free values (re #469). Kernel version
    // still comes from the declared linux-headers via linux/version.h.
    steps.push(Step::substitute_text(
        "{src}/csu/Makefile",
        vec![
            TextEdit::new("os=`uname -s 2> /dev/null`", "os=Linux", 1),
            TextEdit::new("\"`date +%Y-%m-%d`\"", "\"\"", 1),
            TextEdit::new("[ -r /proc/version ]", "false", 1),
            TextEdit::new("version=`uname -r`", "version=unknown", 1),
        ],
    ));
    steps.push(Step::PatchShebangs {
        dir: "{src}".into(),
        shell: SH.into(),
    });
    steps.push(Step::MkDir {
        path: "{src}/build".into(),
    });
    steps.push(
        Step::run(
            "{src}/build",
            &[
                SH,
                "../configure",
                "--prefix={out}",
                "--with-headers={root}/kh",
                "--enable-shared",
                "--disable-obsolete-rpc",
                "--host=i686-unknown-linux-gnu",
                "--enable-static-nss",
                "--with-pthread",
                "--without-cvs",
                "--without-gd",
                "--enable-add-ons=nptl",
                "libc_cv_predef_stack_protector=no",
            ],
        )
        .env("PATH", &path)
        .env("CONFIG_SHELL", SH)
        .env("SHELL", SH)
        // Seed the build triplet so configure skips config.guess, which shells
        // out to host `uname -m` and is coreutils-mesboot0's only hard
        // dependency here (re #469). This reproduces the prior builds'
        // `build=x86_64-unknown-linux-gnu` exactly while leaving build_alias
        // empty, so cross_compiling stays `maybe` (passing --build would force
        // it to `yes` and change the Makefile paths).
        .env("ac_cv_build", "x86_64-unknown-linux-gnu")
        .env("libc_cv_friendly_stddef", "yes")
        .env("libc_cv_ssp", "false")
        .env("C_INCLUDE_PATH", btinc)
        .env("LIBRARY_PATH", btlib)
        .env("CPP", cpp)
        .env("CC", cc)
        .env("LD", "gcc"),
    );
    steps.push(sed_i(
        "$aSHELL := {in:bash-mesboot}/bin/bash",
        &["build/Makefile"],
    ));
    for target in [None, Some("install")] {
        let mut argv: Vec<&str> = vec![
            "{in:make-mesboot}/bin/make",
            "SHELL={in:bash-mesboot}/bin/bash",
        ];
        if let Some(t) = target {
            argv.push(t);
        }
        steps.push(
            Step::run("{src}/build", &argv)
                .env("PATH", &path)
                .env("C_INCLUDE_PATH", btinc)
                .env("LIBRARY_PATH", btlib),
        );
    }
    steps.push(Step::CopyTree {
        from: "{root}/kh".into(),
        dest: "{out}/include".into(),
    });
    steps.push(Step::Require {
        paths: vec![
            "{out}/lib/libc.so.6".into(),
            "{out}/lib/ld-linux.so.2".into(),
        ],
        exec: false,
    });
    Recipe::mesboot("glibc-mesboot-shared", "2.16.0")
        .source_input("glibc-216-source")
        .native_inputs(&[
            "make-mesboot",
            "patch-mesboot",
            "binutils-mesboot",
            "gcc-mesboot1",
            "glibc-mesboot0",
            "gawk-mesboot",
        ])
        .inputs_owned(mesboot0_inputs(&["patch-glibc-boot-2.16.0", "patch-glibc-bootstrap-system-2.16.0", "linux-headers"]))
        .steps(steps)
}

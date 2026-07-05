use crate::ladder::{apply_patch, base_path, sed_i, unpack_into, unpack_keep_top, SH};
use crate::types::{Recipe, Step};

// glibc 2.16.0 SHARED — rung 17 (#378): the runtime libc dynamic /td/store
// binaries load (libc.so.6 + ld-linux.so.2). Single-stage configure exactly as
// guix's glibc-mesboot phases (the two-stage variant defeated the boot patch's
// sunrpc un-hiding); the nis subdir ships no libs (guix-as-oracle: no
// libnsl.so); build TOOLS take <rpc/types.h> from the tree's own sunrpc.
pub fn recipe() -> Recipe {
    let path = format!("{{in:gcc-mesboot1}}/bin:{}", base_path());
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
            ("make".into(), "{in:make-mesboot0}/bin/make".into()),
            ("patch".into(), "{in:patch-mesboot}/bin/patch".into()),
            ("awk".into(), "{in:gawk-mesboot}/bin/gawk".into()),
            ("gawk".into(), "{in:gawk-mesboot}/bin/gawk".into()),
        ],
    });
    steps.push(
        Step::run(
            "{root}",
            &[
                "{in:coreutils}/bin/ln",
                "-sf",
                "glob:{in:binutils-mesboot}/bin/*",
                "{tools}",
            ],
        )
        .env("PATH", &base_path()),
    );
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
        "s,^SHELL := /bin/sh,SHELL := {in:bash}/bin/bash,",
        &["Makeconfig"],
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
        .env("libc_cv_friendly_stddef", "yes")
        .env("libc_cv_ssp", "false")
        .env("C_INCLUDE_PATH", btinc)
        .env("LIBRARY_PATH", btlib)
        .env("CPP", cpp)
        .env("CC", cc)
        .env("LD", "gcc"),
    );
    steps.push(sed_i(
        "$aSHELL := {in:bash}/bin/bash",
        &["build/Makefile"],
    ));
    for target in [None, Some("install")] {
        let mut argv: Vec<&str> = vec![
            "{in:make-mesboot0}/bin/make",
            "SHELL={in:bash}/bin/bash",
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
        .native_inputs(&[
            "make-mesboot0",
            "patch-mesboot",
            "binutils-mesboot",
            "gcc-mesboot1",
            "glibc-mesboot0",
            "gawk-mesboot",
        ])
        .inputs(&[
            "patch-glibc-boot-2.16.0",
            "patch-glibc-bootstrap-system-2.16.0",
            "linux-headers",
            "bash",
            "coreutils",
            "sed",
            "grep",
            "gawk",
            "tar",
            "gzip",
            "bzip2",
            "xz",
            "findutils",
            "diffutils",
        ])
        .steps(steps)
}

use crate::ladder::{SH, apply_patch, link_bins_mesboot0, mesboot0_inputs, mesboot0_path, sed_i_mesboot0, unpack_into, unpack_keep_top};
use crate::types::{Recipe, Step};

// glibc 2.16.0 STATIC — rung 15 (#378, guix's glibc-headers-mesboot +
// glibc-mesboot): gcc-mesboot1 + binutils-mesboot + gawk-mesboot build the
// nptl libc, two stages in one tree (A: bootstrap headers; B: full library).
// All the deleted fn's fixups ride as steps: remove-bashism, de.po, /bin/pwd,
// nscd/manual drops, Makeconfig SHELL, shebangs, the soversions.mk stub
// (--disable-shared omits it but install wants it), the kernel-header overlay.
pub fn recipe() -> Recipe {
    let path = format!("{{in:gcc-mesboot1}}/bin:{}", mesboot0_path());
    let btinc = "{in:glibc-mesboot0}/include:{root}/kh";
    let btlib = "{in:glibc-mesboot0}/lib";
    let cc = "{in:gcc-mesboot1}/bin/gcc -I {src}/nptl/sysdeps/pthread/bits -D BOOTSTRAP_GLIBC=1 -L {src} -L {in:glibc-mesboot0}/lib";
    let cpp = "{in:gcc-mesboot1}/bin/gcc -E -I {src}/nptl/sysdeps/pthread/bits -D BOOTSTRAP_GLIBC=1";
    let cfg = [
        "--disable-shared",
        "--enable-static",
        "--disable-obsolete-rpc",
        "--host=i686-unknown-linux-gnu",
        "--enable-static-nss",
        "--with-pthread",
        "--without-cvs",
        "--without-gd",
        "--enable-add-ons=nptl",
        "libc_cv_predef_stack_protector=no",
    ];
    let mut steps = unpack_into("glibc-mesboot-source", "{src}");
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
    steps.push(
        link_bins_mesboot0("binutils-mesboot"),
    );
    // the deleted fn's source fixups, verbatim
    steps.push(sed_i_mesboot0(
        "s,\\${vdso_symver//\\./_},$(echo $vdso_symver | sed -e \"s/\\\\./_/g\"),",
        &["sysdeps/unix/make-syscalls.sh"],
    ));
    steps.push(sed_i_mesboot0("s,de\\.po,en_GB.po,", &["catgets/Makefile", "intl/Makefile"]));
    steps.push(sed_i_mesboot0("s,/bin/pwd,pwd,", &["configure"]));
    steps.push(sed_i_mesboot0(
        "/^others *+= *nscd/d; /^others-pie *+= *nscd/d; /^install-sbin *:= *nscd/d",
        &["nscd/Makefile"],
    ));
    steps.push(sed_i_mesboot0("s/wctype manual shadow/wctype shadow/", &["Makeconfig"]));
    steps.push(sed_i_mesboot0(
        "s,^SHELL := /bin/sh,SHELL := {in:bash-mesboot}/bin/bash,",
        &["Makeconfig"],
    ));
    steps.push(Step::PatchShebangs {
        dir: "{src}".into(),
        shell: SH.into(),
    });
    // stage A: bootstrap headers into {out}/bootstrap-hdr
    for (bdir, hdrs, prefix) in [
        ("build-hdr", "{root}/kh", "{out}/bootstrap-hdr"),
        ("build", "{out}/bootstrap-hdr/include", "{out}"),
    ] {
        steps.push(Step::MkDir {
            path: format!("{{src}}/{bdir}"),
        });
        let mut argv: Vec<String> = vec![
            SH.into(),
            "../configure".into(),
            format!("--prefix={prefix}"),
            format!("--with-headers={hdrs}"),
        ];
        argv.extend(cfg.iter().map(|s| s.to_string()));
        let argv_refs: Vec<&str> = argv.iter().map(String::as_str).collect();
        steps.push(
            Step::run(&format!("{{src}}/{bdir}"), &argv_refs)
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
        // fixmk: append SHELL to the generated Makefile so recipes use the shell
        steps.push(sed_i_mesboot0(
            "$aSHELL := {in:bash-mesboot}/bin/bash",
            &[&format!("{bdir}/Makefile")],
        ));
        if bdir == "build-hdr" {
            steps.push(
                Step::run(
                    "{src}/build-hdr",
                    &[
                        "{in:make-mesboot}/bin/make",
                        "SHELL={in:bash-mesboot}/bin/bash",
                        "install-bootstrap-headers=yes",
                        "install-headers",
                    ],
                )
                .env("PATH", &path)
                .env("C_INCLUDE_PATH", btinc)
                .env("LIBRARY_PATH", btlib),
            );
            steps.push(Step::CopyTree {
                from: "{root}/kh".into(),
                dest: "{out}/bootstrap-hdr/include".into(),
            });
        } else {
            steps.push(
                Step::run(
                    "{src}/build",
                    &[
                        "{in:make-mesboot}/bin/make",
                        "SHELL={in:bash-mesboot}/bin/bash",
                    ],
                )
                .env("PATH", &path)
                .env("C_INCLUDE_PATH", btinc)
                .env("LIBRARY_PATH", btlib),
            );
            // --disable-shared generates no soversions.mk, but install wants it
            steps.push(Step::WriteFile {
                path: "{src}/build/soversions.mk".into(),
                content: String::new(),
                exec: false,
            });
            steps.push(
                Step::run(
                    "{src}/build",
                    &[
                        "{in:make-mesboot}/bin/make",
                        "SHELL={in:bash-mesboot}/bin/bash",
                        "install",
                    ],
                )
                .env("PATH", &path)
                .env("C_INCLUDE_PATH", btinc)
                .env("LIBRARY_PATH", btlib),
            );
        }
    }
    steps.push(Step::CopyTree {
        from: "{root}/kh".into(),
        dest: "{out}/include".into(),
    });
    steps.push(Step::Require {
        paths: vec!["{out}/lib/libc.a".into(), "{out}/lib/crt1.o".into()],
        exec: false,
    });
    Recipe::mesboot("glibc-mesboot", "2.16.0")
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

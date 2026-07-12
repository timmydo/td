use crate::ladder::{base_inputs, base_path, relocate_ld_scripts, unpack_into, unpack_keep_top, SH};
use crate::types::{Recipe, Step};

// BusyBox 1.37.0, rung 2 of the #388 build userland: built FROM SOURCE by the
// /td/store NATIVE x86_64 toolchain and driven by the td-built make-x86-64 rung.
// Static, matching the make-x86-64 rung: the output has no ELF interpreter. The
// remaining shell/core tools in base_inputs are declared bootstrap scaffolding
// for Kbuild; the output becomes the POSIX userland tool that later rungs can
// consume instead of that scaffolding.
//
// This recipe only BUILDS BusyBox. Its behavior is validated by the sibling
// `busybox-test` recipe, which depends on this one and RUNS the installed applet
// links through the recipe-check feature, mirroring make-x86-64/make-test.
pub fn recipe() -> Recipe {
    let ngcc = "{in:gcc-x86-64-native}/stage/td/store/gcc-14.3.0-x86_64-native/bin/gcc";
    let xglibc = "{in:glibc-x86-64}/stage/td/store/glibc-2.41-x86_64";
    let nbin = "{in:binutils-x86-64-native}/bin";
    let path = format!("{{in:make-x86-64}}/bin:{nbin}:{}", base_path());
    let cip = "{root}/sysroot/include:{root}/kh";
    let lib = "{root}/sysroot/lib";

    let mut steps = unpack_into("busybox-x86-64-source", "{src}");
    steps.extend(unpack_keep_top("linux-headers-x86-64", "{root}/kh"));
    steps.push(Step::PatchShebangs {
        dir: "{src}".into(),
        shell: SH.into(),
    });
    // Static BusyBox links libm.a, which is a GNU ld script in glibc. Relocate a
    // private sysroot copy so the script's GROUP members resolve via -B/-L. The
    // typed RelocateLdScripts step is the host-tool-free equivalent of the old
    // `head|grep|sed` loop: it strips `{prefix}/lib/` from every `*.so`/`*.a`
    // whose first 80 bytes mark it a GNU ld script (real archives are skipped).
    steps.push(Step::CopyTree {
        from: format!("{xglibc}/include"),
        dest: "{root}/sysroot/include".into(),
    });
    steps.push(Step::CopyTree {
        from: format!("{xglibc}/lib"),
        dest: "{root}/sysroot/lib".into(),
    });
    steps.push(relocate_ld_scripts("{root}/sysroot", "/td/store/glibc-2.41-x86_64"));
    // BusyBox's Kbuild builds host helpers, then runs them during the build. glibc
    // popen(3) in those helpers hardcodes /bin/sh, so provide it inside the
    // ephemeral build sandbox from the declared bash input.
    steps.push(
        Step::run(
            "{root}",
            &[
                SH,
                "-c",
                "if [ ! -e /bin/sh ]; then mkdir -p /bin && ln -sf \"{in:bash-mesboot}/bin/bash\" /bin/sh; fi",
            ],
        )
        .env("PATH", &base_path()),
    );
    steps.push(Step::WriteFile {
        path: "{root}/wb/cc".into(),
        content: format!(
            "#!{SH}\n\
             exec \"{ngcc}\" -static -isystem \"{{root}}/sysroot/include\" -idirafter \"{{root}}/kh\" -B\"{{root}}/sysroot/lib\" -L\"{{root}}/sysroot/lib\" \"$@\"\n"
        ),
        exec: true,
    });
    steps.push(
        Step::run(
            "{src}",
            &[
                "{in:make-x86-64}/bin/make",
                "CC={root}/wb/cc",
                "HOSTCC={root}/wb/cc",
                "SHELL={in:bash-mesboot}/bin/bash",
                "CONFIG_SHELL={in:bash-mesboot}/bin/bash",
                "defconfig",
            ],
        )
        .env("PATH", &path)
        .env("SHELL", SH)
        .env("CONFIG_SHELL", SH)
        .env("C_INCLUDE_PATH", &cip)
        .env("LIBRARY_PATH", &lib),
    );
    steps.push(
        Step::run(
            "{src}",
            &[
                SH,
                "-c",
                "{in:sed}/bin/sed -i -E '/^#? *CONFIG_STATIC[ =]/d; /^#? *CONFIG_PIE[ =]/d; /^#? *CONFIG_EXTRA_LDFLAGS[ =]/d' .config && printf '%s\\n' 'CONFIG_STATIC=y' '# CONFIG_PIE is not set' 'CONFIG_EXTRA_LDFLAGS=\"-static\"' >> .config",
            ],
        )
        .env("PATH", &base_path()),
    );
    steps.push(
        Step::run(
            "{src}",
            &[
                SH,
                "-c",
                "yes '' | \"{in:make-x86-64}/bin/make\" CC=\"{root}/wb/cc\" HOSTCC=\"{root}/wb/cc\" SHELL=\"{in:bash-mesboot}/bin/bash\" CONFIG_SHELL=\"{in:bash-mesboot}/bin/bash\" oldconfig >/dev/null",
            ],
        )
        .env("PATH", &path)
        .env("SHELL", SH)
        .env("CONFIG_SHELL", SH)
        .env("C_INCLUDE_PATH", &cip)
        .env("LIBRARY_PATH", &lib),
    );
    steps.push(
        Step::run(
            "{src}",
            &[
                "{in:make-x86-64}/bin/make",
                "-j{jobs}",
                "CC={root}/wb/cc",
                "HOSTCC={root}/wb/cc",
                "SKIP_STRIP=y",
                "SHELL={in:bash-mesboot}/bin/bash",
                "CONFIG_SHELL={in:bash-mesboot}/bin/bash",
            ],
        )
        .env("PATH", &path)
        .env("SHELL", SH)
        .env("CONFIG_SHELL", SH)
        .env("MAKEFLAGS", "")
        .env("MFLAGS", "")
        .env("GNUMAKEFLAGS", "")
        .env("MAKELEVEL", "")
        .env("C_INCLUDE_PATH", &cip)
        .env("LIBRARY_PATH", &lib),
    );
    steps.push(Step::MkDir {
        path: "{out}/bin".into(),
    });
    steps.push(Step::CopyFiles {
        files: vec!["{src}/busybox".into()],
        dest: "{out}/bin".into(),
    });
    steps.push(
        Step::run(
            "{out}/bin",
            &[
                SH,
                "-c",
                "for a in sh ls sed grep awk tar gzip cat echo mkdir rm cp true false env printf sleep sort head tail basename dirname mktemp tee touch tr test pwd comm; do ln -sf busybox \"$a\"; done",
            ],
        )
        .env("PATH", &base_path()),
    );
    steps.push(Step::Require {
        paths: vec!["{out}/bin/busybox".into()],
        exec: true,
    });
    steps.push(
        Step::run(
            "{out}",
            &[
                SH,
                "-c",
                "h=$('{in:binutils-x86-64-native}/bin/readelf' -h '{out}/bin/busybox'); \
                 printf '%s\\n' \"$h\" | grep -i 'class:'   | grep -qi 'ELF64'  || { echo 'busybox is not ELF64' >&2; exit 1; }; \
                 printf '%s\\n' \"$h\" | grep -i 'machine:' | grep -qi 'x86-64' || { echo 'busybox is not x86-64' >&2; exit 1; }",
            ],
        )
        .env("PATH", &base_path()),
    );

    Recipe::mesboot("busybox-x86-64", "1.37.0")
        .source_input("busybox-x86-64-source")
        .native_inputs(&[
            "gcc-x86-64-native",
            "binutils-x86-64-native",
            "glibc-x86-64",
            "make-x86-64",
        ])
        .inputs_owned(base_inputs(&["linux-headers-x86-64"]))
        .steps(steps)
}

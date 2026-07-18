use crate::ladder::{
    mesboot0_inputs, mesboot0_path, relocate_ld_scripts, unpack_into, unpack_keep_top, SH,
};
use crate::types::{Recipe, Step, TextEdit};

// BusyBox 1.37.0, rung 2 of the #388 build userland: built FROM SOURCE by the
// /td/store NATIVE x86_64 toolchain and driven by the td-built make-x86-64 rung.
// Static, matching the make-x86-64 rung: the output has no ELF interpreter. The
// remaining shell/core tools in mesboot0_inputs are declared bootstrap scaffolding
// for Kbuild; the output becomes the POSIX userland tool that later rungs can
// consume instead of that scaffolding.
//
// This recipe only BUILDS BusyBox. Its behavior is validated by the sibling
// `busybox-test` recipe, which depends on this one and RUNS the installed applet
// links through the recipe-check feature, mirroring make-x86-64/make-test.
// Host-free tools: mesboot0 (incl. sed-mesboot0 for the .config edit); make-x86-64 drives. re #469.
pub fn recipe() -> Recipe {
    let ngcc = "{in:gcc-x86-64-native}/stage/td/store/gcc-14.3.0-x86_64-native/bin/gcc";
    let xglibc = "{in:glibc-x86-64}/stage/td/store/glibc-2.41-x86_64";
    let nbin = "{in:binutils-x86-64-native}/bin";
    let path = format!("{{in:make-x86-64}}/bin:{nbin}:{}", mesboot0_path());
    let cip = "{root}/sysroot/include:{root}/kh";
    let lib = "{root}/sysroot/lib";

    let mut steps = unpack_into("busybox-x86-64-source", "{src}");
    // BusyBox's top-level Makefile probes the build machine with uname even
    // when ARCH is supplied, while gen_build_files.sh walks the pinned source
    // tree with a tool that is intentionally absent from the bootstrap tier.
    // Pin the known target and use a shell-only, sorted recursive glob walk.
    steps.push(Step::substitute_text(
        "{src}/Makefile",
        vec![TextEdit::new(
            "SUBARCH := $(shell uname -m)",
            "SUBARCH := x86_64",
            1,
        )],
    ));
    steps.push(Step::substitute_text(
        "{src}/scripts/gen_build_files.sh",
        vec![TextEdit::new(
            r#"{ cd -- "$srctree" && find . -type d ! '(' -name '.?*' -prune ')'; } \"#,
            r#"walk_dirs()
{
	printf '%s\n' "$1"
	for d in "$1"/*; do
		test -d "$d" || continue
		walk_dirs "$d"
	done
}
{ cd -- "$srctree" && walk_dirs .; } \"#,
            1,
        )],
    ));
    // split-include's directory scan exists only to clear stale config headers.
    // Every derivation starts from a fresh unpack, so feed that cleanup pass an
    // empty list instead of spawning an unavailable external tree walker.
    steps.push(Step::substitute_text(
        "{src}/scripts/basic/split-include.c",
        vec![TextEdit::new(
            r#""find * -type f -name \"*.h\" -print""#,
            r#""printf ''""#,
            1,
        )],
    ));
    // trylink uses xargs only to split, deduplicate, and rejoin a short library
    // list. Express both transformations with the already-declared awk/sort
    // providers so the final static link stays inside the recipe closure.
    steps.push(Step::substitute_text(
        "{src}/scripts/trylink",
        vec![
            TextEdit::new(
                r#"LDLIBS=`echo "$LDLIBS" | xargs -n1 | sort | uniq | xargs`"#,
                r#"LDLIBS=`echo "$LDLIBS" | awk '{ for (i = 1; i <= NF; ++i) print $i }' | sort -u | awk '{ if (NR > 1) printf " "; printf "%s", $0 } END { if (NR) print "" }'`"#,
                1,
            ),
            TextEdit::new(
                r#"without_one=`echo " $LDLIBS " | sed "s/ $one / /g" | xargs`"#,
                r#"without_one=`echo " $LDLIBS " | sed "s/ $one / /g" | awk '{$1=$1; print}'`"#,
                1,
            ),
        ],
    ));
    // Compressed build-config embedding is optional and would add a compressor
    // solely as a build-time dependency. With that feature disabled below, the
    // generated compressed array is unreachable; emit one deterministic byte
    // through the existing od/sed pipeline instead.
    steps.push(Step::substitute_text(
        "{src}/scripts/mkconfigs",
        vec![
            TextEdit::new(
                "bzip2 </dev/null >/dev/null\nif test $? != 0; then\n\techo 'bzip2 is not installed'\n\texit 1\nfi",
                ":",
                1,
            ),
            TextEdit::new(
                "| bzip2 -1 | dd bs=2 skip=1 2>/dev/null \\",
                "| awk 'BEGIN { printf \"000\"; exit }' | dd bs=2 skip=1 2>/dev/null \\",
                1,
            ),
        ],
    ));
    steps.push(Step::substitute_text(
        "{src}/applets/usage_compressed",
        vec![TextEdit::new(
            "| bzip2 -1 | $DD bs=2 skip=1 2>/dev/null | od -v -b \\",
            "| awk 'BEGIN { printf \"000\"; exit }' | $DD bs=2 skip=1 2>/dev/null | od -v -b \\",
            1,
        )],
    ));
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
    steps.push(relocate_ld_scripts(
        "{root}/sysroot",
        "/td/store/glibc-2.41-x86_64",
    ));
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
        .env("PATH", &mesboot0_path()),
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
                "{in:sed-mesboot0}/bin/sed -i -r '/^#? *CONFIG_STATIC[ =]/d; /^#? *CONFIG_PIE[ =]/d; /^#? *CONFIG_EXTRA_LDFLAGS[ =]/d; /^#? *CONFIG_FEATURE_COMPRESS_BBCONFIG[ =]/d; /^#? *CONFIG_FEATURE_SH_EMBEDDED_SCRIPTS[ =]/d; /^#? *CONFIG_FEATURE_COMPRESS_USAGE[ =]/d' .config && printf '%s\\n' 'CONFIG_STATIC=y' '# CONFIG_PIE is not set' 'CONFIG_EXTRA_LDFLAGS=\"-static\"' '# CONFIG_FEATURE_COMPRESS_BBCONFIG is not set' '# CONFIG_FEATURE_SH_EMBEDDED_SCRIPTS is not set' '# CONFIG_FEATURE_COMPRESS_USAGE is not set' >> .config",
            ],
        )
        .env("PATH", &mesboot0_path()),
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
        .env("PATH", &mesboot0_path()),
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
        .env("PATH", &mesboot0_path()),
    );

    Recipe::mesboot("busybox-x86-64", "1.37.0")
        .source_input("busybox-x86-64-source")
        .native_inputs(&[
            "gcc-x86-64-native",
            "binutils-x86-64-native",
            "glibc-x86-64",
            "make-x86-64",
        ])
        .inputs_owned(mesboot0_inputs(&["linux-headers-x86-64"]))
        .steps(steps)
}

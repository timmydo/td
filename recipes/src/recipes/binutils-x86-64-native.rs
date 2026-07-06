use crate::ladder::{base_inputs, base_path, unpack_keep_top, unpack_into, SH};
use crate::types::{Recipe, Step};

// GNU Binutils 2.44, NATIVE x86_64 (x86_64-toolchain rung X2, the port of the
// shell build_binutils_x86_64_native): --build=--host=--target=x86_64-pc-linux-gnu,
// so the produced as/ld/ar/readelf are PLAIN-named ELF64 x86_64 binaries that run
// natively — not the target-prefixed i686 CROSS binutils. Built STATIC by the CROSS
// gcc stage2 (an i686 binary emitting x86_64) vs the /td/store x86_64 glibc 2.41
// (so the tools run in the store-ns own-root with no interp dependency). Logical
// --prefix=/td/store/binutils-2.44-x86_64-native; install to {out} (bin/ at the
// output root, matching run_x86_64_cross's XBU-style export). native_inputs: the
// CROSS gcc stage2 (builder CC, referenced absolutely as x86_64-pc-linux-gnu-gcc),
// binutils-x86-64 (the cross as/ld its baked-in --with-as resolves), glibc-x86-64
// (the x86_64 libc the static link pulls).
pub fn recipe() -> Recipe {
    let xgcc = "{in:gcc-x86-64-stage2}/stage/td/store/gcc-14.3.0-x86_64/bin/x86_64-pc-linux-gnu-gcc";
    let xglibc = "{in:glibc-x86-64}/stage/td/store/glibc-2.41-x86_64";
    let path = format!("{{in:binutils-x86-64}}/bin:{}", base_path());
    // glibc 2.41 headers + the x86_64 kernel UAPI headers (glibc headers #include
    // <linux/…>); the native gcc reads headers via the wrapper, binutils via CIP.
    let cip = format!("{xglibc}/include:{{root}}/kh");
    let mut steps = unpack_into("binutils-x86-64-native-source", "{src}");
    // kernel headers keep-top: the tarball top level is {linux,asm,…} → {root}/kh/*.
    steps.extend(unpack_keep_top("linux-headers-x86-64", "{root}/kh"));
    steps.push(Step::ToolFarm {
        links: vec![
            ("awk".into(), "{in:gawk}/bin/awk".into()),
            ("flex".into(), "{in:flex}/bin/flex".into()),
            ("lex".into(), "{in:flex}/bin/flex".into()),
            ("bison".into(), "{in:bison}/bin/bison".into()),
            ("yacc".into(), "{in:bison}/bin/bison".into()),
            ("make".into(), "{in:make}/bin/make".into()),
        ],
    });
    // -shared-aware STATIC wrapper (port of _mk_native_static_wrapper): -static for
    // executables/conftests, DROPPED when the link is -shared (binutils' ld libdep.la
    // shared module, an x86_64 R_X86_64_32-vs-non-PIC-crt guard). -B at the x86_64
    // glibc lib; headers come via C_INCLUDE_PATH so no -idirafter here.
    steps.push(Step::WriteFile {
        path: "{root}/wb/cc".into(),
        content: format!(
            "#!{SH}\n\
             for a in \"$@\"; do case \"$a\" in -shared) exec \"{xgcc}\" -B{xglibc}/lib \"$@\";; esac; done\n\
             exec \"{xgcc}\" -static -B{xglibc}/lib \"$@\"\n"
        ),
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
                "--target=x86_64-pc-linux-gnu",
                "--prefix=/td/store/binutils-2.44-x86_64-native",
                "--disable-nls",
                "--disable-gold",
                "--disable-werror",
                "--enable-deterministic-archives",
                "--disable-plugins",
                "--disable-gprofng",
                "--disable-multilib",
            ],
        )
        .env("PATH", &path)
        .env("CONFIG_SHELL", SH)
        .env("SHELL", SH)
        .env("CC", "{root}/wb/cc")
        .env("CC_FOR_BUILD", "{root}/wb/cc")
        .env("C_INCLUDE_PATH", &cip),
    );
    steps.push(
        Step::run(
            "{src}",
            &[
                "{in:make}/bin/make",
                "-j{jobs}",
                "SHELL={in:bash}/bin/bash",
                "CONFIG_SHELL={in:bash}/bin/bash",
                "MAKEINFO=true",
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
                "{in:make}/bin/make",
                "SHELL={in:bash}/bin/bash",
                "MAKEINFO=true",
                "install",
                "prefix={out}",
            ],
        )
        .env("PATH", &path)
        .env("CONFIG_SHELL", SH)
        .env("SHELL", SH),
    );
    // plain-named native tools (no x86_64-pc-linux-gnu- prefix — host==target).
    steps.push(Step::Require {
        paths: vec![
            "{out}/bin/as".into(),
            "{out}/bin/ld".into(),
            "{out}/bin/readelf".into(),
        ],
        exec: true,
    });
    // [native-arch] the produced `as` is itself an ELF64 x86_64 binary (a NATIVE
    // assembler, not the i686 cross), asserted by the freshly built native readelf —
    // the readelf_is_elf64(as) check the retired build_binutils_x86_64 ran. Catch a
    // wrong-arch `as` HERE, at the rung that produced it, not indirectly at the
    // downstream gcc-x86-64-native link. The static native readelf runs on the x86_64
    // kernel (the same host the native gcc runs on).
    steps.push(
        Step::run(
            "{out}",
            &[
                SH,
                "-c",
                "h=$('{out}/bin/readelf' -h '{out}/bin/as'); \
                 printf '%s\\n' \"$h\" | grep -i 'class:'   | grep -qi 'ELF64'  || { echo 'native binutils as is not ELF64' >&2; exit 1; }; \
                 printf '%s\\n' \"$h\" | grep -i 'machine:' | grep -qi 'x86-64' || { echo 'native binutils as is not x86-64' >&2; exit 1; }",
            ],
        )
        .env("PATH", &base_path()),
    );
    Recipe::mesboot("binutils-x86-64-native", "2.44")
        .native_inputs(&["gcc-x86-64-stage2", "binutils-x86-64", "glibc-x86-64"])
        .inputs_owned(base_inputs(&[
            "binutils-x86-64-native-source",
            "linux-headers-x86-64",
            "flex",
            "bison",
            "make",
        ]))
        .steps(steps)
}

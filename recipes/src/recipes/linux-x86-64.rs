use crate::ladder::{
    mesboot0_inputs, mesboot0_path, relocate_ld_scripts, unpack_into, unpack_keep_top, SH,
};
use crate::types::{Recipe, Step};

// linux-x86-64 (Linux 6.18.39 `vmlinux`): the capstone of the x86_64 ladder
// (#529). Source-builds an uncompressed `vmlinux` ELF for the CURRENT LTS kernel
// with td's OWN bootstrapped native toolchain — gcc-x86-64-native (GCC 14.3.0)
// as `CC`, binutils-x86-64-native as `LD/AR/NM/OBJCOPY`, driven by make-x86-64 —
// proving the GCC/glibc ladder produces a real, modern-kernel-capable compiler.
// `vmlinux` (not `bzImage`) is landed FIRST: the raw ELF target invokes no
// compressor, and td ships no gzip/xz executable (the builder decompresses
// in-process), so bzImage + qemu boot are a separate later rung (re #529, re
// #469).
//
// A modern (>= 4.18) kernel needs host tools the 4.14 rung dodged; each is now a
// td recipe (AGENTS.md directive 3, pre-authorized as part of this migration):
//   - flex (flex-x86-64) + bison (bison-mesboot): scripts/kconfig's lexer/parser
//     are generated during the build — the `*_shipped` parsers the 4.14 rung
//     relied on are gone. Both are passed as LEX/YACC and are on PATH; each
//     execs m4 (m4-mesboot, baked at their configure).
//   - libelf (elfutils-x86-64): objtool is force-selected on x86_64 in 6.18
//     (HAVE_STATIC_CALL_INLINE + HAVE_UACCESS_VALIDATION both `select OBJTOOL`
//     unconditionally — no .config flip removes it), and objtool links libelf.
//     td's static libelf.a is not self-contained, so a pkg-config shim feeds
//     kbuild `-lelf -leu -lz` from the elfutils-x86-64 output (libelf + libeu +
//     the bundled static zlib).
//   - bc (busybox-x86-64 applet): timeconst.h, as in the 4.14 rung.
// Avoided by config (audited): perl (C recordmcount; ftrace off), openssl (no
// module signing / trusted keys), pahole (BTF off), python/cpio/compressors/
// rsync (no initramfs, no IKHEADERS, uncompressed vmlinux, no modules).
//
// HOSTCC (the Kbuild host programs fixdep/conf/objtool/modpost — ordinary
// userspace) is a STATIC wrapper over the native gcc against the x86_64 glibc
// 2.41 sysroot, plus the libelf headers. CC (the kernel target compiler) is the
// BARE native gcc: kernel code is `-nostdinc` freestanding with its own headers,
// and vmlinux is linked by `LD` (not gcc), so no glibc byte enters the image.
// GCC 14 builds this 2025 source clean — no version-skew shim is needed (unlike
// the retired 4.14 rung).
pub fn recipe() -> Recipe {
    let ngcc = "{in:gcc-x86-64-native}/stage/td/store/gcc-14.3.0-x86_64-native/bin/gcc";
    let xglibc = "{in:glibc-x86-64}/stage/td/store/glibc-2.41-x86_64";
    let nbin = "{in:binutils-x86-64-native}/bin";
    let bb = "{in:busybox-x86-64}/bin/busybox";
    let elfinc = "{in:elfutils-x86-64}/include";
    let elflib = "{in:elfutils-x86-64}/lib";
    // {root}/wb first so the libelf pkg-config shim also answers a bare
    // `pkg-config` (in addition to the command-line HOSTPKG_CONFIG=); flex + bison
    // on PATH as well as passed via LEX/YACC (belt and suspenders); make + native
    // binutils ahead of the mesboot0 userland; bc resolves from the {tools} farm
    // that mesboot0_path() lays down first.
    let path = format!(
        "{{root}}/wb:{nbin}:{{in:make-x86-64}}/bin:{{in:flex-x86-64}}/bin:{{in:bison-mesboot}}/bin:{}",
        mesboot0_path()
    );

    let mut steps = unpack_into("linux-kernel-source", "{src}");

    // Host sysroot for HOSTCC only (the Kbuild host programs are ordinary
    // userspace): the x86_64 glibc 2.41 headers + kernel UAPI headers overlaid
    // into include, glibc libs into lib, with the GNU ld linker scripts relocated
    // to bare names so a fully-static host link resolves libc.so/libm.a's GROUP.
    steps.push(Step::CopyTree {
        from: format!("{xglibc}/include"),
        dest: "{root}/sysroot/include".into(),
    });
    steps.extend(unpack_keep_top("linux-headers-x86-64", "{root}/sysroot/include"));
    steps.push(Step::CopyTree {
        from: format!("{xglibc}/lib"),
        dest: "{root}/sysroot/lib".into(),
    });
    steps.push(relocate_ld_scripts(
        "{root}/sysroot",
        "/td/store/glibc-2.41-x86_64",
    ));

    // bc (busybox applet) for timeconst.h, linked into the {tools} farm that
    // mesboot0_path() lays on PATH, so Kbuild's parse-time `bc -q timeconst.bc`
    // resolves it (Kbuild:21 in 6.18). BusyBox dispatches on argv[0].
    steps.push(Step::ToolFarm {
        links: vec![("bc".into(), bb.into())],
    });
    // Fail FAST if that bc cannot do what timeconst.h needs (read()/arithmetic/
    // print/halt); a missing/POSIX-only bc yields an EMPTY timeconst.h and a
    // confusing failure deep in the build. Probe exactly those features here.
    steps.push(Step::WriteFile {
        path: "{root}/bcprobe/probe.bc".into(),
        content: "h = read()\nprint h * 3 + 1, \"\\n\"\nhalt\n".into(),
        exec: false,
    });
    steps.push(
        Step::run(
            "{root}",
            &[
                SH,
                "-c",
                "out=$(printf '%s\\n' 7 | bc -q {root}/bcprobe/probe.bc) || { echo 'bc probe: bc failed to run (GNU-extension bc for timeconst.h unavailable)' >&2; exit 1; }; \
                 [ \"$out\" = 22 ] || { echo \"bc probe: wrong result (got '$out', want 22) — bc is not the GNU-extension bc timeconst.h needs\" >&2; exit 1; }",
            ],
        )
        .env("PATH", &mesboot0_path()),
    );

    // Some glibc host helpers popen(3)/system(3) hardcode /bin/sh; the host-free
    // sandbox has none, so provide it from the declared bash input (busybox parity).
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

    // STATIC HOSTCC wrapper: native gcc vs the relocated glibc sysroot, plus the
    // libelf headers (objtool/modpost #include <gelf.h>/<libelf.h>). -idirafter
    // keeps these headers lowest-priority so a host program's own includes win;
    // -B/-L point the static link at the sysroot libs.
    steps.push(Step::WriteFile {
        path: "{root}/wb/hostcc".into(),
        content: format!(
            "#!{SH}\n\
             exec \"{ngcc}\" -static -idirafter {{root}}/sysroot/include -idirafter {elfinc} -B{{root}}/sysroot/lib -L{{root}}/sysroot/lib \"$@\"\n"
        ),
        exec: true,
    });

    // pkg-config shim for kbuild's libelf discovery (tools/objtool + scripts/mod
    // query `$(HOSTPKG_CONFIG) libelf --cflags/--libs`). td's libelf is a set of
    // STATIC archives that are not self-contained, so answer `--libs` with the
    // full `-lelf -leu -lz` (libelf → libeu for eu_tsearch → zlib for the
    // section-decompress path). Kept faithful to real pkg-config so kbuild's
    // feature detection behaves correctly regardless of how it phrases the query:
    // flags accumulate (a combined `--cflags --libs` prints both), a query for
    // `libelf` alone or `--exists libelf` succeeds silently (exit 0), and any
    // OTHER module — including a multi-package query like `libelf zlib` — makes
    // `$mod` != libelf and reports the package absent (exit 1).
    steps.push(Step::WriteFile {
        path: "{root}/wb/pkg-config".into(),
        content: format!(
            "#!{SH}\n\
             mod=''; cflags=0; libs=0; modversion=0\n\
             for a in \"$@\"; do\n\
             \tcase \"$a\" in\n\
             \t--cflags) cflags=1;;\n\
             \t--libs) libs=1;;\n\
             \t--modversion) modversion=1;;\n\
             \t-*) ;;\n\
             \t*) mod=\"${{mod:+$mod }}$a\";;\n\
             \tesac\n\
             done\n\
             [ \"$mod\" = libelf ] || exit 1\n\
             out=''\n\
             [ \"$modversion\" = 1 ] && out=0.192\n\
             [ \"$cflags\" = 1 ] && out=\"${{out:+$out }}-I{elfinc}\"\n\
             [ \"$libs\" = 1 ] && out=\"${{out:+$out }}-L{elflib} -lelf -leu -lz\"\n\
             [ -n \"$out\" ] && printf '%s\\n' \"$out\"\n\
             exit 0\n"
        ),
        exec: true,
    });

    steps.push(Step::PatchShebangs {
        dir: "{src}".into(),
        shell: SH.into(),
    });

    // Reproducibility: pin the build identity so mkcompile_h needs no
    // date/whoami/hostname, and SOURCE_DATE_EPOCH stamps any embedded timestamp.
    // LEX/YACC/HOSTPKG_CONFIG point kbuild at td's flex/bison/libelf shim.
    let mk = |args: &[&str]| -> Step {
        let mut argv = vec![
            "{in:make-x86-64}/bin/make",
            "ARCH=x86_64",
            "CC={in:gcc-x86-64-native}/stage/td/store/gcc-14.3.0-x86_64-native/bin/gcc",
            "HOSTCC={root}/wb/hostcc",
            "HOSTPKG_CONFIG={root}/wb/pkg-config",
            "LEX={in:flex-x86-64}/bin/flex",
            "YACC={in:bison-mesboot}/bin/bison",
            "SHELL={in:bash-mesboot}/bin/bash",
            "CONFIG_SHELL={in:bash-mesboot}/bin/bash",
        ];
        argv.extend_from_slice(args);
        Step::run("{src}", &argv)
            .env("PATH", &path)
            .env("SHELL", SH)
            .env("CONFIG_SHELL", SH)
            .env("SOURCE_DATE_EPOCH", "1")
            .env("KBUILD_BUILD_TIMESTAMP", "Thu Jan  1 00:00:01 UTC 1970")
            .env("KBUILD_BUILD_USER", "td")
            .env("KBUILD_BUILD_HOST", "td")
    };

    // 1) allnoconfig: the smallest kconfig (keeps CONFIG_64BIT=y under ARCH=x86_64),
    //    generated by the HOSTCC-built `conf` — which is compiled from the
    //    flex/bison-generated kconfig lexer/parser, so this first step already
    //    exercises flex + bison.
    steps.push(mk(&["allnoconfig"]));

    // 2) Config deltas over allnoconfig. The ONE required change is the unwinder:
    //    allnoconfig on x86_64 takes the choice default UNWINDER_ORC, which needs
    //    the ORC-generation objtool pass; frame-pointer avoids that (objtool ITSELF
    //    still builds+runs for static-call/uaccess — unavoidable on x86_64, hence
    //    libelf). The remaining pins are DEFENSIVE: allnoconfig already leaves them
    //    off, but pinning off the symbols that would pull a new host tool
    //    (perl/openssl/pahole/cpio/kmod) hardens directive-1's no-undeclared-tool
    //    invariant against an allnoconfig default drift across sub-versions.
    steps.push(
        Step::run(
            "{src}",
            &[
                SH,
                "-c",
                "{in:sed-mesboot0}/bin/sed -i -r \
                 '/^#? *CONFIG_UNWINDER_ORC[ =]/d; \
                  /^#? *CONFIG_UNWINDER_FRAME_POINTER[ =]/d; \
                  /^#? *CONFIG_MODULES[ =]/d; \
                  /^#? *CONFIG_MODULE_SIG[ =]/d; \
                  /^#? *CONFIG_SYSTEM_TRUSTED_KEYRING[ =]/d; \
                  /^#? *CONFIG_SYSTEM_TRUSTED_KEYS[ =]/d; \
                  /^#? *CONFIG_DEBUG_INFO_BTF[ =]/d; \
                  /^#? *CONFIG_FTRACE[ =]/d; \
                  /^#? *CONFIG_GCC_PLUGINS[ =]/d; \
                  /^#? *CONFIG_IKHEADERS[ =]/d; \
                  /^#? *CONFIG_INITRAMFS_SOURCE[ =]/d' .config && \
                 printf '%s\\n' \
                   'CONFIG_UNWINDER_FRAME_POINTER=y' \
                   '# CONFIG_UNWINDER_ORC is not set' \
                   '# CONFIG_MODULES is not set' \
                   '# CONFIG_MODULE_SIG is not set' \
                   '# CONFIG_SYSTEM_TRUSTED_KEYRING is not set' \
                   'CONFIG_SYSTEM_TRUSTED_KEYS=\"\"' \
                   '# CONFIG_DEBUG_INFO_BTF is not set' \
                   '# CONFIG_FTRACE is not set' \
                   '# CONFIG_GCC_PLUGINS is not set' \
                   '# CONFIG_IKHEADERS is not set' \
                   'CONFIG_INITRAMFS_SOURCE=\"\"' >> .config",
            ],
        )
        .env("PATH", &mesboot0_path()),
    );

    // 3) olddefconfig normalises the choice (takes defaults for newly-visible
    //    symbols, non-interactive).
    steps.push(mk(&["olddefconfig"]));

    // 4) Guard: the minimal invariant must hold before the full build. Fail HERE,
    //    with a named symbol, if the unwinder flip or the no-modules pin did not
    //    take. (objtool IS expected on x86_64 6.18 — not asserted absent.)
    steps.push(
        Step::run(
            "{src}",
            &[
                SH,
                "-c",
                "grep -q '^CONFIG_UNWINDER_FRAME_POINTER=y' .config || { echo 'frame-pointer unwinder not selected' >&2; exit 1; }; \
                 if grep -q '^CONFIG_MODULES=y' .config; then echo 'MODULES on (would need module tooling)' >&2; exit 1; fi; \
                 if grep -q '^CONFIG_DEBUG_INFO_BTF=y' .config; then echo 'BTF on (would need pahole)' >&2; exit 1; fi",
            ],
        )
        .env("PATH", &mesboot0_path()),
    );

    // 5) Build vmlinux. binutils tools passed explicitly (native as/ld/ar/nm/…),
    //    host make env scrubbed (busybox parity).
    steps.push(
        mk(&[
            "-j{jobs}",
            &format!("LD={nbin}/ld"),
            &format!("AR={nbin}/ar"),
            &format!("NM={nbin}/nm"),
            &format!("OBJCOPY={nbin}/objcopy"),
            &format!("OBJDUMP={nbin}/objdump"),
            &format!("STRIP={nbin}/strip"),
            "vmlinux",
        ])
        .env("MAKEFLAGS", "")
        .env("MFLAGS", "")
        .env("GNUMAKEFLAGS", "")
        .env("MAKELEVEL", ""),
    );

    // Land the uncompressed ELF + its symbol map.
    steps.push(Step::MkDir {
        path: "{out}".into(),
    });
    steps.push(Step::CopyFiles {
        files: vec!["{src}/vmlinux".into(), "{src}/System.map".into()],
        dest: "{out}".into(),
    });
    steps.push(Step::Require {
        paths: vec!["{out}/vmlinux".into()],
        exec: false,
    });
    // [native-arch] vmlinux must be an ELF64 x86-64 linked executable (EXEC, not a
    // stray relocatable .o) — caught at the producer rung, parity with
    // make-x86-64 / gcc-x86-64-native.
    steps.push(
        Step::run(
            "{out}",
            &[
                SH,
                "-c",
                "h=$('{in:binutils-x86-64-native}/bin/readelf' -h '{out}/vmlinux'); \
                 printf '%s\\n' \"$h\" | grep -i 'class:'   | grep -qi 'ELF64'  || { echo 'vmlinux is not ELF64' >&2; exit 1; }; \
                 printf '%s\\n' \"$h\" | grep -i 'machine:' | grep -qi 'x86-64' || { echo 'vmlinux is not x86-64' >&2; exit 1; }; \
                 printf '%s\\n' \"$h\" | grep -i 'type:'    | grep -qi 'EXEC'   || { echo 'vmlinux is not a linked ELF executable (EXEC)' >&2; exit 1; }",
            ],
        )
        .env("PATH", &mesboot0_path()),
    );

    Recipe::mesboot("linux-x86-64", "6.18.39")
        .source_input("linux-kernel-source")
        .native_inputs(&[
            "gcc-x86-64-native",
            "binutils-x86-64-native",
            "glibc-x86-64",
            "make-x86-64",
            "busybox-x86-64",
            "flex-x86-64",
            "bison-mesboot",
            "m4-mesboot",
            "elfutils-x86-64",
        ])
        .inputs_owned(mesboot0_inputs(&["linux-headers-x86-64"]))
        .steps(steps)
}

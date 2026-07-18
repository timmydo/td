use crate::ladder::{
    mesboot0_inputs, mesboot0_path, relocate_ld_scripts, unpack_into, unpack_keep_top, SH,
};
use crate::types::{Recipe, Step};

// linux-x86-64 (Linux 4.14.67 `vmlinux`): the capstone of the x86_64 ladder
// (#529). Source-builds an uncompressed `vmlinux` ELF with td's OWN bootstrapped
// native toolchain — gcc-x86-64-native (GCC 14.3.0) as `CC`, binutils-x86-64-native
// as `LD/AR/NM/OBJCOPY`, driven by make-x86-64 — proving the GCC/glibc ladder
// produces a real, kernel-capable compiler. `vmlinux` (not `bzImage`) is landed
// FIRST: the raw ELF target invokes no compressor, and td ships no gzip/xz
// executable (the builder decompresses in-process), so bzImage + qemu boot are a
// separate later rung (re #529, re #469).
//
// This builds 2018 source with a 2024 compiler: GCC 14's four new default-error
// C warnings (implicit-function-declaration / implicit-int /
// incompatible-pointer-types / int-conversion) are demoted back to warnings, and
// GCC 10+'s -fno-common default is reverted with -fcommon, via the documented
// `gcc14_relax` shim below (HOSTCC wrapper + kernel KCFLAGS). This relaxes only
// the compiler's newly-strict defaults; it is not a blanket -w.
//
// ZERO new external dependency (AGENTS.md directive 3). A minimal `vmlinux` needs
// exactly one host tool td did not already have — `bc`, which generates
// include/generated/timeconst.h (kernel/time/Kbuild; no `_shipped` fallback) and
// must be a GNU-extension bc (uses `print`/`read()`/`obase`/`halt`). That is
// supplied by the ALREADY-BUILT busybox-x86-64 `bc` applet (a full
// arbitrary-precision, Turing-complete bc), symlinked into the {tools} farm — no
// GNU bc / flex / bison / perl / libelf / openssl recipe is introduced. flex/bison
// are avoided by the pristine `scripts/kconfig/*_shipped` parsers (td's Unpack
// preserves the archived mtimes, so the shipped copy rule wins and the bison/flex
// regeneration rules never fire); perl is avoided (recordmcount is the C variant
// via HAVE_C_RECORDMCOUNT, and ftrace/doc targets are off); libelf/objtool is
// avoided by forcing the frame-pointer unwinder (see the .config edit); openssl by
// building no modules. The remaining POSIX userland (sh/sed/grep/awk/coreutils)
// is the mesboot0 tier already declared by every rung.
//
// HOSTCC (the Kbuild host programs fixdep/conf/modpost — ordinary userspace) is a
// STATIC wrapper over the native gcc against the x86_64 glibc 2.41 sysroot, exactly
// the busybox-x86-64 shape. CC (the kernel target compiler) is the BARE native gcc:
// kernel code is `-nostdinc` freestanding with its own headers, and vmlinux is
// linked by `LD` (not gcc), so no glibc byte enters the image. as/ld are the native
// binutils (baked into gcc via --with-as/--with-ld, and on PATH).
pub fn recipe() -> Recipe {
    let ngcc = "{in:gcc-x86-64-native}/stage/td/store/gcc-14.3.0-x86_64-native/bin/gcc";
    let xglibc = "{in:glibc-x86-64}/stage/td/store/glibc-2.41-x86_64";
    let nbin = "{in:binutils-x86-64-native}/bin";
    let bb = "{in:busybox-x86-64}/bin/busybox";
    let path = format!("{nbin}:{{in:make-x86-64}}/bin:{}", mesboot0_path());

    // GCC 14 promoted four long-standing C warnings to hard errors by default
    // (-Werror=implicit-function-declaration / implicit-int /
    // incompatible-pointer-types / int-conversion). Linux 4.14 (2017) predates
    // those tightenings and its kernel + host C trips them under the 2024
    // compiler, so demote exactly those four back to warnings. This relaxes only
    // GCC's *default* -Werror=, it is not a blanket -w and hides no real breakage;
    // the frame-pointer / no-modules config keeps the surface small enough that
    // this is the whole GCC-14-on-4.14 shim. Applied to both HOSTCC (via the
    // wrapper) and the kernel proper (via KCFLAGS on the vmlinux step).
    let gcc14_relax = "-Wno-error=implicit-function-declaration \
                       -Wno-error=implicit-int \
                       -Wno-error=incompatible-pointer-types \
                       -Wno-error=int-conversion";

    let mut steps = unpack_into("linux-source", "{src}");

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

    // The single new host tool: bc (busybox applet) for timeconst.h, linked into
    // the {tools} farm — which mesboot0_path() already lays on PATH, so Kbuild's
    // parse-time `bc -q timeconst.bc` resolves it. BusyBox dispatches on argv[0],
    // so a bc→busybox symlink runs the bc applet. No uname link: the top
    // Makefile's `$(shell uname -m)` SUBARCH probe is cosmetic here (ARCH=x86_64
    // is explicit), and coreutils-mesboot0 already provides uname on PATH anyway.
    steps.push(Step::ToolFarm {
        links: vec![("bc".into(), bb.into())],
    });

    // Fail FAST if that bc cannot do what timeconst.h needs. Kbuild's
    // filechk_gentimeconst pipes CONFIG_HZ into `bc -q timeconst.bc`, a script
    // that uses read()/arithmetic/print/halt; a missing or POSIX-only bc silently
    // yields an EMPTY timeconst.h and a confusing failure deep in the vmlinux
    // build. Probe exactly those features here, resolving bc from {tools} on
    // mesboot0_path() the same way the build will, so a bad bc reds up front.
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

    // STATIC HOSTCC wrapper: native gcc vs the relocated glibc sysroot. -idirafter
    // (not -isystem) keeps these headers lowest-priority so a host program's own
    // includes win; -B/-L point the static link at the sysroot libs.
    steps.push(Step::WriteFile {
        path: "{root}/wb/hostcc".into(),
        content: format!(
            "#!{SH}\n\
             exec \"{ngcc}\" -static {gcc14_relax} -idirafter {{root}}/sysroot/include -B{{root}}/sysroot/lib -L{{root}}/sysroot/lib \"$@\"\n"
        ),
        exec: true,
    });

    steps.push(Step::PatchShebangs {
        dir: "{src}".into(),
        shell: SH.into(),
    });

    // Belt-and-suspenders for the flex/bison-free path: bump the pristine kconfig
    // `_shipped` parsers so they are unambiguously not older than their `.l`/`.y`
    // sources. td's Unpack already preserves the (identical) archived mtimes, so
    // Kbuild's `$(obj)/%: $(src)/%_shipped` copy rule wins and the bison/flex
    // regeneration rules never fire — this only reinforces that direction, never
    // triggers a regeneration.
    steps.push(
        Step::run(
            "{src}/scripts/kconfig",
            &[
                SH,
                "-c",
                "touch zconf.lex.c_shipped zconf.tab.c_shipped",
            ],
        )
        .env("PATH", &mesboot0_path()),
    );

    // Reproducibility: pin the build identity so mkcompile_h needs no
    // date/whoami/hostname, and SOURCE_DATE_EPOCH stamps any embedded timestamp.
    let build_env = |s: Step| -> Step {
        s.env("PATH", &path)
            .env("SHELL", SH)
            .env("CONFIG_SHELL", SH)
            .env("SOURCE_DATE_EPOCH", "1")
            .env("KBUILD_BUILD_TIMESTAMP", "Thu Jan  1 00:00:01 UTC 1970")
            .env("KBUILD_BUILD_USER", "td")
            .env("KBUILD_BUILD_HOST", "td")
    };

    // 1) allnoconfig: the smallest kconfig, generated by the HOSTCC-built `conf`.
    steps.push(build_env(Step::run(
        "{src}",
        &[
            "{in:make-x86-64}/bin/make",
            "ARCH=x86_64",
            &format!("CC={ngcc}"),
            "HOSTCC={root}/wb/hostcc",
            "SHELL={in:bash-mesboot}/bin/bash",
            "CONFIG_SHELL={in:bash-mesboot}/bin/bash",
            "allnoconfig",
        ],
    )));

    // 2) Force the FRAME-POINTER unwinder. allnoconfig on x86_64 takes the "kernel
    //    unwinder" choice's default (UNWINDER_ORC), which selects STACK_VALIDATION
    //    and hard-requires objtool + libelf (`Makefile: *** Cannot generate ORC
    //    metadata ...`). Frame-pointer needs neither. RETPOLINE (which would also
    //    select STACK_VALIDATION) is already off in allnoconfig; keep it explicit.
    steps.push(
        Step::run(
            "{src}",
            &[
                SH,
                "-c",
                "{in:sed-mesboot0}/bin/sed -i -r \
                 '/^#? *CONFIG_UNWINDER_ORC[ =]/d; \
                  /^#? *CONFIG_UNWINDER_FRAME_POINTER[ =]/d; \
                  /^#? *CONFIG_UNWINDER_GUESS[ =]/d; \
                  /^#? *CONFIG_STACK_VALIDATION[ =]/d; \
                  /^#? *CONFIG_RETPOLINE[ =]/d' .config && \
                 printf '%s\\n' \
                   'CONFIG_UNWINDER_FRAME_POINTER=y' \
                   '# CONFIG_UNWINDER_ORC is not set' \
                   '# CONFIG_STACK_VALIDATION is not set' \
                   '# CONFIG_RETPOLINE is not set' >> .config",
            ],
        )
        .env("PATH", &mesboot0_path()),
    );

    // 3) olddefconfig normalises the choice (takes defaults for any newly-visible
    //    symbols, non-interactive).
    steps.push(build_env(Step::run(
        "{src}",
        &[
            "{in:make-x86-64}/bin/make",
            "ARCH=x86_64",
            &format!("CC={ngcc}"),
            "HOSTCC={root}/wb/hostcc",
            "SHELL={in:bash-mesboot}/bin/bash",
            "CONFIG_SHELL={in:bash-mesboot}/bin/bash",
            "olddefconfig",
        ],
    )));

    // 4) Guard: the objtool/libelf-free invariant must hold before we spend the
    //    full build. Fail HERE, with a named symbol, if the unwinder flip did not
    //    take.
    steps.push(
        Step::run(
            "{src}",
            &[
                SH,
                "-c",
                "grep -q '^CONFIG_UNWINDER_FRAME_POINTER=y' .config || { echo 'frame-pointer unwinder not selected' >&2; exit 1; }; \
                 if grep -q '^CONFIG_STACK_VALIDATION=y' .config; then echo 'STACK_VALIDATION still on (would need objtool+libelf)' >&2; exit 1; fi; \
                 if grep -q '^CONFIG_MODULES=y' .config; then echo 'MODULES on (would need module tooling)' >&2; exit 1; fi",
            ],
        )
        .env("PATH", &mesboot0_path()),
    );

    // 5) Build vmlinux. binutils tools passed explicitly (native as/ld/ar/nm/…),
    //    host make env scrubbed (busybox parity).
    steps.push(
        build_env(Step::run(
            "{src}",
            &[
                "{in:make-x86-64}/bin/make",
                "-j{jobs}",
                "ARCH=x86_64",
                &format!("CC={ngcc}"),
                "HOSTCC={root}/wb/hostcc",
                &format!("LD={nbin}/ld"),
                &format!("AR={nbin}/ar"),
                &format!("NM={nbin}/nm"),
                &format!("OBJCOPY={nbin}/objcopy"),
                &format!("OBJDUMP={nbin}/objdump"),
                &format!("STRIP={nbin}/strip"),
                "SHELL={in:bash-mesboot}/bin/bash",
                "CONFIG_SHELL={in:bash-mesboot}/bin/bash",
                // GCC-14-on-4.14 shim for the kernel proper. KCFLAGS is *appended*
                // to KBUILD_CFLAGS, so these win over any earlier -Werror=. -fcommon
                // is added here (kernel only): GCC 10+ defaults to -fno-common, and
                // 4.14 predates the tree-wide -fno-common cleanup, so tentative
                // definitions in headers would otherwise collide at link.
                &format!("KCFLAGS={gcc14_relax} -fcommon"),
                "vmlinux",
            ],
        ))
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
    // [native-arch] vmlinux must be an ELF64 x86-64 image — caught at the producer
    // rung, parity with make-x86-64 / gcc-x86-64-native.
    steps.push(
        Step::run(
            "{out}",
            &[
                SH,
                "-c",
                "h=$('{in:binutils-x86-64-native}/bin/readelf' -h '{out}/vmlinux'); \
                 printf '%s\\n' \"$h\" | grep -i 'class:'   | grep -qi 'ELF64'  || { echo 'vmlinux is not ELF64' >&2; exit 1; }; \
                 printf '%s\\n' \"$h\" | grep -i 'machine:' | grep -qi 'x86-64' || { echo 'vmlinux is not x86-64' >&2; exit 1; }",
            ],
        )
        .env("PATH", &mesboot0_path()),
    );

    Recipe::mesboot("linux-x86-64", "4.14.67")
        .source_input("linux-source")
        .native_inputs(&[
            "gcc-x86-64-native",
            "binutils-x86-64-native",
            "glibc-x86-64",
            "make-x86-64",
            "busybox-x86-64",
        ])
        .inputs_owned(mesboot0_inputs(&["linux-headers-x86-64"]))
        .steps(steps)
}

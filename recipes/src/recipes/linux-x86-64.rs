use crate::ladder::{
    initramfs_cpio_shape_check, mesboot0_inputs, mesboot0_path, relocate_ld_scripts, unpack_into,
    unpack_keep_top, EROFS_MARKER, EROFS_PROBE_CONTENT, EROFS_PROBE_SENTINEL, SH, USERLAND_MARKER,
};
use crate::types::{Recipe, Step};

// linux-x86-64 (Linux 7.1.4): the capstone of the x86_64 ladder (#529).
// Source-builds the latest STABLE mainline kernel (not a longterm/LTS line) with
// td's OWN bootstrapped native toolchain — gcc-x86-64-native (GCC 14.3.0) as
// `CC`, binutils-x86-64-native as `LD/AR/NM/OBJCOPY`, driven by make-x86-64 —
// proving the GCC/glibc ladder produces a real, modern-kernel-capable compiler.
// Three artifacts land in {out}: the uncompressed `vmlinux` ELF, the compressed,
// bootable `bzImage`, and a tiny `initramfs.cpio` userland (see BOOTABLE below).
// bzImage's self-extracting payload is gzip-compressed
// (kbuild's `cat vmlinux.bin | $(KGZIP) -n -f -9 > vmlinux.bin.gz`); td ships no
// gzip executable — the builder only DEcompresses, in-process — so the compressor
// is the busybox gzip applet's `bin/gzip` link, which accepts the kernel's exact
// `-n -f -9` directly (busybox gzip's options include `-n` and `-1..-9`).
// CONFIG_KERNEL_GZIP is pinned so the compressor is always gzip (never xz/zstd,
// which would need a different tool).
//
// BOOTABLE (re #529): the config also turns on the 8250 serial console
// (SERIAL_8250 + SERIAL_8250_CONSOLE, PRINTK/TTY), ELF + #! exec (BINFMT_ELF,
// BINFMT_SCRIPT), and initramfs load (BLK_DEV_INITRD), and the recipe packs a
// tiny EXTERNAL initramfs (gen_init_cpio) holding the td-built STATIC busybox
// plus a /init that prints a marker on ttyS0 and reboots. A THIRD {out} artifact
// (initramfs.cpio) lands alongside vmlinux/bzImage.
//
// USABLE (re #541): the config also turns on the pseudo-filesystems a real
// userland needs — PROC_FS (/proc), SYSFS (/sys), DEVTMPFS (the /dev filesystem
// that init mounts to populate /dev with ttyS0/console/null/…), and TMPFS
// (/tmp,/run). CONFIG_DEVTMPFS_MOUNT is deliberately NOT managed here: it does not
// auto-mount /dev on an initramfs boot (only when the kernel mounts a real root),
// so the `system-x86-64` init mounts devtmpfs itself; a hand-packed /dev/console
// node still covers init's own stdio before that first sysinit line runs. The
// output also EXPORTS the in-tree `gen_init_cpio` packer so the downstream
// `system-x86-64` distro recipe can pack its own root-owned initramfs from the
// same td-built tool.
//
// READ-ONLY ROOT (re #549): the config also turns on what the two-stage boot
// needs to pivot from the initramfs onto a disk-backed, read-only erofs
// `/td/store` root — the block layer (BLOCK), the PCI bus and virtio-over-PCI
// transport the `-M pc` qemu machine exposes (PCI, VIRTIO, VIRTIO_PCI), the
// virtio block device the erofs image rides (VIRTIO_BLK → /dev/vda), the erofs
// filesystem itself (EROFS_FS, uncompressed — matching the `td-builder
// mkfs-erofs` writer, #548), and OVERLAY_FS for the tmpfs overlays the read-only
// root needs over its writable paths. BLK_DEV_INITRD stays on: the kernel still
// boots via a minimal initramfs that then mounts and switch_roots into the erofs
// disk (the switch_root itself is #550). The behavioural proof that
// this source-built kernel boots to a real userland is the HOST-SIDE tool
// `td-recipe-eval qemu-boot linux-x86-64` (checks/qemu_boot.rs): it boots the
// bzImage + initramfs under host qemu (TCG) and asserts the userland marker
// reaches the console. It is host-side (not a daily gate check) because a qemu
// boot needs host qemu, which the gate's host-free sandbox hides — see the note
// at the Recipe builder below. In-sandbox CI coverage is the artifact shape
// checks (this recipe's producer rung + linux-x86-64-test).
//
// A modern (>= 4.18) kernel needs host tools the 4.14 rung dodged; each is now a
// td recipe (AGENTS.md directive 3, pre-authorized as part of this migration):
//   - flex (flex-x86-64) + bison (bison-mesboot): scripts/kconfig's lexer/parser
//     are generated during the build — the `*_shipped` parsers the 4.14 rung
//     relied on are gone. Both are passed as LEX/YACC and are on PATH; each
//     execs m4 (m4-mesboot, baked at their configure).
//   - libelf (elfutils-x86-64): objtool is force-selected on x86_64 in 7.x
//     (HAVE_STATIC_CALL_INLINE + HAVE_UACCESS_VALIDATION both `select OBJTOOL`
//     unconditionally — no .config flip removes it), and objtool links libelf.
//     td's static libelf.a is not self-contained, so a pkg-config shim feeds
//     kbuild `-lelf -leu -lz` from the elfutils-x86-64 output (libelf + libeu +
//     the bundled static zlib).
//   - bc (busybox-x86-64 applet): timeconst.h, as in the 4.14 rung.
// Avoided by config (audited): perl (C recordmcount; ftrace off), openssl (no
// module signing / trusted keys), pahole (BTF off), python/cpio/rsync. The
// initramfs below is packed by the in-tree gen_init_cpio HOSTCC hostprog (not the
// external `cpio` tool), INITRAMFS_SOURCE stays "", and IKHEADERS/modules stay
// off — so none of python, the `cpio` tool, or rsync is pulled in. The ONE
// compressor used is gzip (busybox applet) for the bzImage payload —
// CONFIG_KERNEL_GZIP pinned; xz/lzma/zstd and their host tools stay off.
//
// HOSTCC (the Kbuild host programs fixdep/conf/objtool/modpost — ordinary
// userspace) is a STATIC wrapper over the native gcc against the x86_64 glibc
// 2.41 sysroot, plus the libelf headers. CC (the kernel target compiler) is the
// BARE native gcc: kernel code is `-nostdinc` freestanding with its own headers,
// and vmlinux is linked by `LD` (not gcc), so no glibc byte enters the image.
// GCC 14.3.0 builds this modern (2026) source with no version-skew shim (unlike
// the retired 4.14 rung); the daily backstop is the build-truth for a kernel bump
// and surfaces any new GCC-14 warning or host-tool requirement a 7.x source adds.
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

    // A rung's own source lands in TD_INPUT_MAP under the LOCAL name `{name}-source`
    // (`linux-x86-64-source`), synthesized from the recipe NAME — NOT under its
    // `sourceInput` PIN KEY. When the two differ (this rung renames the shared
    // `linux-kernel-source` pin locally, exactly as gcc-x86-64-native renames
    // `gcc-14-source` / make-441 renames `make-x86-64-source`), steps MUST reference
    // the local `{in:linux-x86-64-source}`; `{in:linux-kernel-source}` is not a map key
    // and expands to nothing ("no input `linux-kernel-source' in TD_INPUT_MAP"). The
    // pin key stays on `.source_input(...)` below, which is what gates/fetches the bytes.
    let mut steps = unpack_into("linux-x86-64-source", "{src}");

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

    // BusyBox applets the kernel's host build execs that neither mesboot0 nor the
    // native toolchain provides, linked by name into the {tools} farm that
    // mesboot0_path() lays on PATH (BusyBox dispatches on argv[0]). {tools} precedes
    // coreutils-mesboot0 on PATH, so ONLY names ABSENT from coreutils-mesboot0 are
    // farmed here — none of these shadow a mesboot0 tool:
    //   - bc:     timeconst.h parse-time `bc -q timeconst.bc` (as in the 4.14 rung).
    //   - xargs:  cmd_ar_builtin (scripts/Makefile.build) builds EVERY built-in.a via
    //             `printf … | xargs $(AR) …`, and cmd_ld_multi links objtool — the
    //             core of `make vmlinux`; xargs is findutils, absent from mesboot0.
    //   - uname:  `uname -m` in scripts/subarch.include (every make) + objtool's
    //             tools/scripts/Makefile.arch; coreutils-mesboot0 omits uname.
    //   - mktemp: usr/gen_initramfs.sh runs `mktemp` under `set -e` for the default
    //             (`-d`, empty INITRAMFS_SOURCE) embedded-initramfs list — a hard fail
    //             without it; coreutils-mesboot0 omits mktemp.
    //   - dd:     arch/x86/boot/Makefile's cmd_image pads setup.bin with `dd` when
    //             assembling bzImage; coreutils-mesboot0 omits dd.
    //   - find:   usr/gen_initramfs.sh print_mtime. Non-fatal for our empty source (it
    //             sits in a `| sort | head` pipeline → blank mtime), and BusyBox find
    //             lacks -printf, but farm it so the probe execs rather than erroring;
    //             the -printf dir_filelist path is only reached by a DIRECTORY
    //             INITRAMFS_SOURCE, which this rung does not use.
    // Only `bc` is feature-probed below (a POSIX-only bc silently mis-builds
    // timeconst.h); the rest fail loudly as "command not found" if the applet is gone.
    steps.push(Step::ToolFarm {
        links: vec![
            ("bc".into(), bb.into()),
            ("xargs".into(), bb.into()),
            ("uname".into(), bb.into()),
            ("mktemp".into(), bb.into()),
            ("dd".into(), bb.into()),
            ("find".into(), bb.into()),
        ],
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

    // gzip compressor for the bzImage payload. The pinned CONFIG_KERNEL_GZIP rule
    // is `cat vmlinux.bin | $(KGZIP) -n -f -9 > vmlinux.bin.gz`. td ships no gzip
    // executable — only the builder's in-process DECOMPRESSORS — so KGZIP is the
    // busybox gzip applet's `bin/gzip` link (busybox is already a kernel input).
    // BusyBox 1.37 gzip's option set includes `-n` (a GNU-compat no-op), `-f`, and
    // `-1..-9`, so it accepts the kernel's exact `-n -f -9` invocation directly —
    // no wrapper needed, and this keeps real max compression, `-n` reproducibility
    // (alongside SOURCE_DATE_EPOCH), and fail-closed behaviour on an unexpected
    // operand. cmd_gzip always pipes the payload stdin->stdout (no file operand),
    // exactly what the applet expects when invoked with no file. Wired as KGZIP=
    // on the build step below.
    //
    // Fail FAST (parity with the bc probe) with the EXACT flags kbuild uses, so a
    // busybox that lacked the gzip applet or rejected `-n -f -9` reds HERE with a
    // named error instead of deep inside arch/x86/boot/compressed. Assert the
    // output carries the gzip magic `1f 8b 08` (id1/id2 + CM=deflate).
    steps.push(
        Step::run(
            "{root}",
            &[
                SH,
                "-c",
                "if ! printf 'td-gzip-probe' | '{in:busybox-x86-64}/bin/gzip' -n -f -9 > {root}/gzprobe.gz 2>/dev/null; then echo 'gzip probe: busybox gzip -n -f -9 failed (applet missing or rejects the kernel flags)' >&2; exit 1; fi; \
                 [ -s {root}/gzprobe.gz ] || { echo 'gzip probe: busybox gzip produced no output' >&2; exit 1; }; \
                 set -- $(od -An -tx1 -N3 {root}/gzprobe.gz); \
                 [ \"$1$2$3\" = 1f8b08 ] || { echo \"gzip probe: not a gzip/deflate stream (magic $1$2$3, want 1f8b08)\" >&2; exit 1; }",
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
    //
    //    READ-ONLY ROOT menuconfig parents (re #549): three of the erofs-root leaves
    //    sit under a PROMPTED `menuconfig` parent that `allnoconfig` forces off, so
    //    the leaf alone is invisible and `olddefconfig` would silently drop it (unlike
    //    e.g. SHMEM behind TMPFS, which has no prompt and keeps its `default y`). The
    //    parents must therefore be set EXPLICITLY: CONFIG_VIRTIO_MENU (gates
    //    VIRTIO_PCI), CONFIG_BLK_DEV (gates VIRTIO_BLK), CONFIG_MISC_FILESYSTEMS
    //    (gates EROFS_FS). Do not drop them as "redundant" — the grep-verify guard on
    //    the leaves (step 4) reds the whole producer build if a parent goes missing.
    //    (OVERLAY_FS / PCI / BLOCK are directly selectable, no menuconfig parent.)
    //
    //    TMPFS_XATTR (re #550): the two-stage boot overlays the writable dirs
    //    (/etc,/var,/home) with an OverlayFS whose UPPER layer is a tmpfs, and a
    //    writable OverlayFS upper MUST support trusted.* extended attributes. tmpfs
    //    only offers xattrs when CONFIG_TMPFS_XATTR=y — a PROMPTED leaf that defaults
    //    OFF, so (like the menuconfig parents) it is set explicitly and grep-verified,
    //    else the overlays fall back to a degraded no-xattr mode or fail outright.
    //
    //    KEXEC (image-based self-boot): the target boots via a two-hop kexec chain —
    //    a tiny immutable shim kernel+initramfs (loaded by qemu -kernel) mounts the
    //    persistent volume and kexec_file_load(2)s the selected deployment's
    //    bzImage+initramfs. Only CONFIG_KEXEC_FILE (the file-based syscall) + the
    //    KEXEC_CORE it selects are used; CONFIG_KEXEC — the legacy segment-based
    //    kexec_load(2) — is pinned OFF and guarded, since nothing here uses it and it
    //    would only widen the syscall surface (in 7.1.4 KEXEC and KEXEC_FILE are
    //    independent siblings — KEXEC_FILE does NOT depend on KEXEC — so dropping
    //    KEXEC leaves KEXEC_FILE intact, and reboot(LINUX_REBOOT_CMD_KEXEC) runs off
    //    KEXEC_CORE). In 7.1.4 KEXEC_FILE `select`s CRYPTO_LIB_SHA256 + KEXEC_CORE on
    //    its own (verified against kernel/Kconfig.kexec), so NO CRYPTO menuconfig is
    //    pulled in — do not add CONFIG_CRYPTO/CRYPTO_SHA256. CONFIG_RELOCATABLE=y is REQUIRED: the x86
    //    kexec-bzimage64 loader rejects a non-relocatable image, so a kernel that
    //    boots fine via `-kernel` still fails via kexec — the classic
    //    works-direct/fails-kexec divergence. allnoconfig drops RELOCATABLE despite
    //    its `default y`, so it is set explicitly; enabling it makes RANDOMIZE_BASE
    //    (KASLR, default y) visible, pinned OFF here for a deterministic boot.
    //    KEXEC_SIG/_FORCE are pinned OFF, consistent with the disabled trusted-keyring
    //    policy above (no signature/keyring machinery ships).
    //
    //    BTRFS + LOOP (persistent volume): the single persistent /dev/vda is one
    //    btrfs filesystem holding @var plus the immutable root images — each a
    //    reproducible EROFS blob loop-mounted read-only — so BLK_DEV_LOOP is needed to
    //    mount an EROFS file out of the btrfs. CONFIG_BTRFS_FS=y self-selects its whole
    //    dependency set (CRC32, CRYPTO_LIB_SHA256/BLAKE2B, ZLIB/LZO/ZSTD, RAID6_PQ,
    //    XOR_BLOCKS, XXHASH, FS_IOMAP — verified against fs/btrfs/Kconfig), so do NOT
    //    pin those individually; note CONFIG_LIBCRC32C does NOT exist in 7.1.4 (btrfs
    //    uses the CRC32 lib, not CRYPTO_CRC32C) — a guard on it would red the build
    //    permanently. BTRFS_FS sits under `if BLOCK` (BLOCK already =y) and
    //    BLK_DEV_LOOP under `menuconfig BLK_DEV` (already =y), so no new menuconfig
    //    parent is required.
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
                  /^#? *CONFIG_KERNEL_GZIP[ =]/d; \
                  /^#? *CONFIG_KERNEL_BZIP2[ =]/d; \
                  /^#? *CONFIG_KERNEL_LZMA[ =]/d; \
                  /^#? *CONFIG_KERNEL_XZ[ =]/d; \
                  /^#? *CONFIG_KERNEL_LZO[ =]/d; \
                  /^#? *CONFIG_KERNEL_LZ4[ =]/d; \
                  /^#? *CONFIG_KERNEL_ZSTD[ =]/d; \
                  /^#? *CONFIG_BINFMT_ELF[ =]/d; \
                  /^#? *CONFIG_BINFMT_SCRIPT[ =]/d; \
                  /^#? *CONFIG_BLK_DEV_INITRD[ =]/d; \
                  /^#? *CONFIG_SERIAL_8250[ =]/d; \
                  /^#? *CONFIG_SERIAL_8250_CONSOLE[ =]/d; \
                  /^#? *CONFIG_INITRAMFS_SOURCE[ =]/d; \
                  /^#? *CONFIG_PROC_FS[ =]/d; \
                  /^#? *CONFIG_SYSFS[ =]/d; \
                  /^#? *CONFIG_DEVTMPFS[ =]/d; \
                  /^#? *CONFIG_TMPFS[ =]/d; \
                  /^#? *CONFIG_TMPFS_XATTR[ =]/d; \
                  /^#? *CONFIG_BLOCK[ =]/d; \
                  /^#? *CONFIG_BLK_DEV[ =]/d; \
                  /^#? *CONFIG_PCI[ =]/d; \
                  /^#? *CONFIG_VIRTIO[ =]/d; \
                  /^#? *CONFIG_VIRTIO_MENU[ =]/d; \
                  /^#? *CONFIG_VIRTIO_PCI[ =]/d; \
                  /^#? *CONFIG_VIRTIO_BLK[ =]/d; \
                  /^#? *CONFIG_MISC_FILESYSTEMS[ =]/d; \
                  /^#? *CONFIG_EROFS_FS[ =]/d; \
                  /^#? *CONFIG_OVERLAY_FS[ =]/d; \
                  /^#? *CONFIG_KEXEC[ =]/d; \
                  /^#? *CONFIG_KEXEC_FILE[ =]/d; \
                  /^#? *CONFIG_KEXEC_SIG[ =]/d; \
                  /^#? *CONFIG_KEXEC_SIG_FORCE[ =]/d; \
                  /^#? *CONFIG_RELOCATABLE[ =]/d; \
                  /^#? *CONFIG_RANDOMIZE_BASE[ =]/d; \
                  /^#? *CONFIG_BLK_DEV_LOOP[ =]/d; \
                  /^#? *CONFIG_BTRFS_FS[ =]/d' .config && \
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
                   'CONFIG_KERNEL_GZIP=y' \
                   'CONFIG_BINFMT_ELF=y' \
                   'CONFIG_BINFMT_SCRIPT=y' \
                   'CONFIG_BLK_DEV_INITRD=y' \
                   'CONFIG_SERIAL_8250=y' \
                   'CONFIG_SERIAL_8250_CONSOLE=y' \
                   'CONFIG_INITRAMFS_SOURCE=\"\"' \
                   'CONFIG_PROC_FS=y' \
                   'CONFIG_SYSFS=y' \
                   'CONFIG_DEVTMPFS=y' \
                   'CONFIG_TMPFS=y' \
                   'CONFIG_TMPFS_XATTR=y' \
                   'CONFIG_BLOCK=y' \
                   'CONFIG_BLK_DEV=y' \
                   'CONFIG_PCI=y' \
                   'CONFIG_VIRTIO=y' \
                   'CONFIG_VIRTIO_MENU=y' \
                   'CONFIG_VIRTIO_PCI=y' \
                   'CONFIG_VIRTIO_BLK=y' \
                   'CONFIG_MISC_FILESYSTEMS=y' \
                   'CONFIG_EROFS_FS=y' \
                   'CONFIG_OVERLAY_FS=y' \
                   '# CONFIG_KEXEC is not set' \
                   'CONFIG_KEXEC_FILE=y' \
                   '# CONFIG_KEXEC_SIG is not set' \
                   '# CONFIG_KEXEC_SIG_FORCE is not set' \
                   'CONFIG_RELOCATABLE=y' \
                   '# CONFIG_RANDOMIZE_BASE is not set' \
                   'CONFIG_BLK_DEV_LOOP=y' \
                   'CONFIG_BTRFS_FS=y' >> .config",
            ],
        )
        .env("PATH", &mesboot0_path()),
    );

    // 3) olddefconfig normalises the choice (takes defaults for newly-visible
    //    symbols, non-interactive).
    steps.push(mk(&["olddefconfig"]));

    // 4) Guard: the minimal invariant must hold before the full build. Fail HERE,
    //    with a named symbol, if the unwinder flip or the no-modules pin did not
    //    take. (objtool IS expected on x86_64 7.x — not asserted absent.)
    steps.push(
        Step::run(
            "{src}",
            &[
                SH,
                "-c",
                "grep -q '^CONFIG_UNWINDER_FRAME_POINTER=y' .config || { echo 'frame-pointer unwinder not selected' >&2; exit 1; }; \
                 grep -q '^CONFIG_KERNEL_GZIP=y' .config || { echo 'gzip kernel compression not selected (bzImage would need another compressor)' >&2; exit 1; }; \
                 grep -q '^CONFIG_BINFMT_ELF=y' .config || { echo 'BINFMT_ELF off — the kernel could not exec the busybox userland' >&2; exit 1; }; \
                 grep -q '^CONFIG_BINFMT_SCRIPT=y' .config || { echo 'BINFMT_SCRIPT off — the kernel could not exec the #! /init script' >&2; exit 1; }; \
                 grep -q '^CONFIG_BLK_DEV_INITRD=y' .config || { echo 'BLK_DEV_INITRD off — the kernel could not load the initramfs' >&2; exit 1; }; \
                 grep -q '^CONFIG_SERIAL_8250_CONSOLE=y' .config || { echo '8250 serial console off — no ttyS0 boot output for the qemu check' >&2; exit 1; }; \
                 grep -q '^CONFIG_DEVTMPFS=y' .config || { echo 'DEVTMPFS off - init cannot mount /dev, so the userland has no ttyS0/console devices' >&2; exit 1; }; \
                 grep -q '^CONFIG_PROC_FS=y' .config || { echo 'PROC_FS off - a usable userland (ps/mount/init) needs /proc' >&2; exit 1; }; \
                 grep -q '^CONFIG_SYSFS=y' .config || { echo 'SYSFS off - a usable userland needs /sys' >&2; exit 1; }; \
                 grep -q '^CONFIG_TMPFS=y' .config || { echo 'TMPFS off - a usable userland needs a writable /tmp,/run' >&2; exit 1; }; \
                 grep -q '^CONFIG_TMPFS_XATTR=y' .config || { echo 'TMPFS_XATTR off - OverlayFS needs xattr support on the tmpfs upperdir, else the writable /etc,/var,/home overlays over the read-only erofs root fail (re #550)' >&2; exit 1; }; \
                 grep -q '^CONFIG_BLOCK=y' .config || { echo 'BLOCK off - no block layer for the virtio-blk erofs disk' >&2; exit 1; }; \
                 grep -q '^CONFIG_PCI=y' .config || { echo 'PCI off - virtio-blk-pci (the -M pc transport) needs the PCI bus' >&2; exit 1; }; \
                 grep -q '^CONFIG_VIRTIO_PCI=y' .config || { echo 'VIRTIO_PCI off - no virtio transport on the -M pc PCI bus' >&2; exit 1; }; \
                 grep -q '^CONFIG_VIRTIO_BLK=y' .config || { echo 'VIRTIO_BLK off - the erofs disk (/dev/vda) would not appear' >&2; exit 1; }; \
                 grep -q '^CONFIG_EROFS_FS=y' .config || { echo 'EROFS_FS off - the read-only erofs root could not be mounted' >&2; exit 1; }; \
                 grep -q '^CONFIG_OVERLAY_FS=y' .config || { echo 'OVERLAY_FS off - no tmpfs overlay for writable paths over the read-only root' >&2; exit 1; }; \
                 grep -q '^CONFIG_PRINTK=y' .config || { echo 'PRINTK off — no kernel console output' >&2; exit 1; }; \
                 grep -q '^CONFIG_TTY=y' .config || { echo 'TTY off — the serial console needs the tty layer' >&2; exit 1; }; \
                 grep -q '^CONFIG_KEXEC_FILE=y' .config || { echo 'KEXEC_FILE off — the shim boots the selected deployment via kexec_file_load(2)' >&2; exit 1; }; \
                 grep -q '^CONFIG_RELOCATABLE=y' .config || { echo 'RELOCATABLE off — a non-relocatable bzImage is rejected by the x86 kexec_file_load loader (boots via -kernel, fails via kexec)' >&2; exit 1; }; \
                 grep -q '^CONFIG_BTRFS_FS=y' .config || { echo 'BTRFS_FS off — the persistent volume is one btrfs filesystem (@var plus the loop-mounted EROFS root blobs)' >&2; exit 1; }; \
                 grep -q '^CONFIG_BLK_DEV_LOOP=y' .config || { echo 'BLK_DEV_LOOP off — the immutable EROFS root is a file inside btrfs, loop-mounted read-only' >&2; exit 1; }; \
                 if grep -q '^CONFIG_KEXEC=y' .config; then echo 'KEXEC (legacy segment-based kexec_load) on — only KEXEC_FILE is used; the legacy syscall is kept off to not widen the surface' >&2; exit 1; fi; \
                 if grep -q '^CONFIG_KEXEC_SIG=y' .config; then echo 'KEXEC_SIG on — would demand a trusted-keyring signature policy td does not ship' >&2; exit 1; fi; \
                 if grep -q '^CONFIG_RANDOMIZE_BASE=y' .config; then echo 'RANDOMIZE_BASE (KASLR) on — pinned off for a deterministic kexec boot' >&2; exit 1; fi; \
                 if grep -q '^CONFIG_MODULES=y' .config; then echo 'MODULES on (would need module tooling)' >&2; exit 1; fi; \
                 if grep -q '^CONFIG_DEBUG_INFO_BTF=y' .config; then echo 'BTF on (would need pahole)' >&2; exit 1; fi",
            ],
        )
        .env("PATH", &mesboot0_path()),
    );

    // 5) Build vmlinux + the bootable bzImage. binutils tools passed explicitly
    //    (native as/ld/ar/nm/…), host make env scrubbed (busybox parity), and
    //    KGZIP pointed at the busybox gzip applet link for the bzImage payload.
    //    (`bzImage` builds `vmlinux` as a prerequisite; both are listed so a
    //    kbuild change that stops re-emitting the raw ELF still lands it.)
    steps.push(
        mk(&[
            "-j{jobs}",
            &format!("LD={nbin}/ld"),
            &format!("AR={nbin}/ar"),
            &format!("NM={nbin}/nm"),
            &format!("OBJCOPY={nbin}/objcopy"),
            &format!("OBJDUMP={nbin}/objdump"),
            &format!("STRIP={nbin}/strip"),
            "KGZIP={in:busybox-x86-64}/bin/gzip",
            "vmlinux",
            "bzImage",
        ])
        .env("MAKEFLAGS", "")
        .env("MFLAGS", "")
        .env("GNUMAKEFLAGS", "")
        .env("MAKELEVEL", ""),
    );

    // ---- Bootable userland: a static-busybox initramfs (re #529) ----
    // The kernel is now serial-console + initramfs capable (the config deltas
    // above add the 8250 console, BINFMT_ELF/SCRIPT, and BLK_DEV_INITRD). Pack a
    // tiny initramfs whose /init prints a marker on ttyS0, optionally mounts an
    // attached erofs disk read-only (re #549) and prints a second marker, then
    // reboots — so the sibling qemu boot checks can prove the td-source-built
    // kernel both reaches a real userland AND mounts the read-only erofs root. The
    // initramfs is an EXTERNAL artifact (qemu -initrd), keeping bzImage a pure
    // kernel — INITRAMFS_SOURCE stays "".
    //
    // gen_init_cpio (usr/gen_init_cpio, a HOSTCC hostprog) packs the newc cpio
    // from a spec WITHOUT needing mknod privilege: the `nod /dev/console` entry
    // is written straight into the archive, which the unprivileged host-free
    // sandbox could not create on a real filesystem. busybox is the td-built
    // STATIC busybox (CONFIG_STATIC=y), so the initramfs is self-contained — no
    // glibc closure, no host bytes.

    // The /init the kernel execs (rdinit=/init): a #! script (BINFMT_SCRIPT) that
    // prints the userland marker the boot check greps for, THEN — if a virtio-blk
    // erofs disk is attached (the `qemu-boot-erofs` tool, re #549) — mounts it
    // read-only and prints a second marker on success, then `reboot -f` so qemu
    // (-no-reboot) exits cleanly. echo is a busybox-sh builtin, so the userland
    // marker prints even if the reboot applet were unavailable (the boot check's
    // wall-clock ceiling then bounds the run).
    //
    // Applet reachability: this initramfs packs only /bin/busybox + a /bin/sh
    // symlink (no per-applet symlinks — the busybox recipe's own /bin symlink farm
    // is NOT what gets packed here) and busybox is NOT built standalone-shell, so
    // EVERY applet (`mkdir`/`mount`/`sleep`/`cat`/`test`/`reboot`) is reached as
    // `/bin/busybox <applet>` explicitly, never bare (a bare `mount` would not
    // resolve on PATH). Only `echo` is used bare — it is an ash SHELL BUILTIN, not
    // an applet, so it needs no symlink. `test` is invoked via busybox (not the `[`
    // builtin) so it does not depend on CONFIG_ASH_TEST being set in the busybox
    // config.
    //
    // Disk probe (gated on /dev/vda so a diskless boot is unaffected — the plain
    // `qemu-boot` check kills qemu on USERLAND_MARKER, printed first, before this
    // runs): mount devtmpfs so the virtio-blk node appears, settle briefly for the
    // async probe, then mount /dev/vda READ-ONLY as erofs and read the sentinel the
    // image was built with. The sentinel's CONTENT is read back with `cat` and
    // string-compared (not just `test -f`), so a corrupt/missing DATA block cannot
    // green — EROFS_MARKER prints only when the mount AND the data read-back both
    // succeed, making it a true read-only-erofs-mount proof. mount stderr is left on
    // the console (no `2>/dev/null`) so a mount failure is visible in the boot
    // oracle's captured serial log. This is the seed of the #550 two-stage boot,
    // where the `reboot -f` becomes the switch_root into the mounted root.
    steps.push(Step::WriteFile {
        path: "{root}/initramfs/init".into(),
        content: format!(
            "#!/bin/sh\n\
             echo {USERLAND_MARKER}\n\
             /bin/busybox mkdir -p /mnt\n\
             /bin/busybox mount -t devtmpfs dev /dev\n\
             n=0\n\
             while /bin/busybox test \"$n\" -lt 5 && ! /bin/busybox test -b /dev/vda; do /bin/busybox sleep 1; n=$((n+1)); done\n\
             if /bin/busybox test -b /dev/vda; then \
             if /bin/busybox mount -t erofs -o ro /dev/vda /mnt; then \
             if /bin/busybox test \"$(/bin/busybox cat /mnt/{EROFS_PROBE_SENTINEL})\" = {EROFS_PROBE_CONTENT}; then echo {EROFS_MARKER}; fi; \
             fi; \
             fi\n\
             exec /bin/busybox reboot -f\n"
        ),
        exec: true,
    });
    // gen_init_cpio spec: /dev/console for init's stdio, the static busybox, a
    // /bin/sh -> busybox multi-call symlink for the #! interpreter, and /init.
    steps.push(Step::WriteFile {
        path: "{root}/initramfs/spec".into(),
        content: "dir /dev 0755 0 0\n\
                  nod /dev/console 0600 0 0 c 5 1\n\
                  dir /bin 0755 0 0\n\
                  file /bin/busybox {in:busybox-x86-64}/bin/busybox 0755 0 0\n\
                  slink /bin/sh /bin/busybox 0777 0 0\n\
                  file /init {root}/initramfs/init 0755 0 0\n"
            .into(),
        exec: false,
    });
    // Build gen_init_cpio explicitly (idempotent — the bzImage run already built
    // it to pack the empty INITRAMFS_SOURCE — but don't rely on that ordering).
    steps.push(mk(&["usr/gen_init_cpio"]));
    // Pack the initramfs; gen_init_cpio writes the newc cpio to stdout. `-t 1`
    // pins a fixed mtime (1s past the epoch) on EVERY entry: without it,
    // gen_init_cpio stamps each `file` with its source's stat mtime — and /init is
    // written fresh by this build, so its mtime would be the wall-clock build time,
    // making initramfs.cpio (a content-addressed /td/store artifact) differ across
    // otherwise-identical builds. A fixed timestamp keeps the output reproducible.
    steps.push(
        Step::run(
            "{src}",
            &[
                SH,
                "-c",
                "'{src}/usr/gen_init_cpio' -t 1 {root}/initramfs/spec > {root}/initramfs.cpio",
            ],
        )
        .env("PATH", &mesboot0_path()),
    );

    // Land the uncompressed ELF + its symbol map + the bootable bzImage + the
    // external busybox initramfs.
    steps.push(Step::MkDir {
        path: "{out}".into(),
    });
    steps.push(Step::CopyFiles {
        files: vec![
            "{src}/vmlinux".into(),
            "{src}/System.map".into(),
            "{src}/arch/x86/boot/bzImage".into(),
            "{root}/initramfs.cpio".into(),
            // Export the in-tree cpio packer so a downstream distro recipe
            // (system-x86-64) can pack its OWN root-owned initramfs — with the
            // unprivileged `nod` device-node trick and per-entry uid/gid 0 — from
            // the SAME td-built gen_init_cpio this kernel already built and ran.
            // Rebuilding it in the distro recipe would need the kernel source; this
            // 3-line export avoids that.
            "{src}/usr/gen_init_cpio".into(),
        ],
        dest: "{out}".into(),
    });
    steps.push(Step::Require {
        paths: vec![
            "{out}/vmlinux".into(),
            "{out}/bzImage".into(),
            "{out}/initramfs.cpio".into(),
        ],
        exec: false,
    });
    steps.push(Step::Require {
        paths: vec!["{out}/gen_init_cpio".into()],
        exec: true,
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
    // [bzImage] the compressed image must carry the x86 boot-setup header: the
    // 0xAA55 boot signature at 0x1fe and the "HdrS" (48 64 72 53) magic at 0x202
    // (arch/x86/boot/header.S), read with od's offset seek (mesboot0 ships no dd).
    // A size floor first rejects a header-only/truncated image that would carry
    // those two constants but no kernel payload (a real allnoconfig bzImage is
    // megabytes; 64 KiB cleanly separates it from a <1 KiB truncation). Asserted
    // here at the producer rung; the sibling test re-checks and also scans for the
    // embedded gzip payload.
    steps.push(
        Step::run(
            "{out}",
            &[
                SH,
                "-c",
                "sz=$(wc -c < '{out}/bzImage'); \
                 [ \"$sz\" -ge 65536 ] || { echo \"bzImage: implausibly small ($sz bytes) — header-only/truncated image\" >&2; exit 1; }; \
                 set -- $(od -An -tx1 -j 510 -N 2 '{out}/bzImage'); \
                 [ \"$1$2\" = 55aa ] || { echo 'bzImage: missing 0xAA55 boot signature at 0x1fe' >&2; exit 1; }; \
                 set -- $(od -An -tx1 -j 514 -N 4 '{out}/bzImage'); \
                 [ \"$1$2$3$4\" = 48647253 ] || { echo 'bzImage: missing HdrS setup-header magic at 0x202' >&2; exit 1; }",
            ],
        )
        .env("PATH", &mesboot0_path()),
    );
    // [initramfs] the packed userland must be a real, COMPLETE newc cpio carrying
    // the whole bootable userland — not merely a well-formed header. The shared
    // `initramfs_cpio_shape_check` helper (recipes/src/ladder.rs) parses the archive
    // with busybox `cpio -t` — a real newc walk that reds on a truncated/corrupt
    // stream and yields the exact member names — then asserts init/bin/busybox/
    // bin/sh/dev/console are all present and the `TD-USERLAND-OK` /init marker is
    // packed. The producer rung and the fast `linux-x86-64-test` tier run the SAME
    // check so they cannot drift. The sibling qemu boot tool is the behavioural proof
    // (it boots this cpio); this is the fast producer-rung shape check.
    let initramfs_check =
        initramfs_cpio_shape_check("{out}/initramfs.cpio", "{in:busybox-x86-64}/bin/busybox");
    steps.push(
        Step::run("{out}", &[SH, "-c", &initramfs_check]).env("PATH", &mesboot0_path()),
    );

    Recipe::mesboot("linux-x86-64", "7.1.4")
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
    // No behavioural boot check is registered here: a qemu boot needs HOST qemu,
    // which the daily gate's host-free `pivot_root` sandbox deliberately hides (the
    // sandbox exposes only td-built tools by absolute /td/store path — that is why
    // the RustToolchain check can run the td-BUILT rustc, but a host binary like
    // qemu is unreachable there). Wiring the boot as a sandboxed daily check would
    // make it fail on `find_qemu` on every real runner — a permanently-red, green-
    // washed check. The boot is instead a HOST-SIDE tool, `td-recipe-eval
    // qemu-boot linux-x86-64` (checks/qemu_boot.rs), run OUTSIDE the sandbox by an
    // operator or developer. Automated in-sandbox coverage is the shape checks
    // above (producer rung) and the linux-x86-64-test BuildOnly daily check, which
    // build the bzImage + initramfs and assert they are well-formed.
}

use crate::ladder::{mesboot0_inputs, mesboot0_path, KEXEC_STAGE1_MARKER, KEXEC_STAGE2_MARKER, SH};
use crate::types::{Recipe, Step};

// kexec-spike-x86-64 — the Phase-0 operator spike proving td's source-built kernel
// can kexec_file_load(2) a SECOND kernel start (the mechanism the image-based boot
// uses to self-boot a refreshed image). It assembles a two-kernel boot artifact; the
// actual boot is the host-side `td-recipe-eval qemu-boot-kexec` oracle (host qemu,
// OUTSIDE the sandbox), exactly as `qemu-boot` boots linux-x86-64.
//
// One qemu run, two kernel starts:
//   * qemu -kernel {out}/bzImage -initrd {out}/outer-initramfs.cpio boots the OUTER
//     kernel; its /init prints KEXEC_STAGE1_MARKER, then execs td-kexec to
//     kexec_file_load the inner kernel + inner initramfs and reboot(KEXEC) into it.
//     A kexec is NOT a machine reset, so qemu's `-no-reboot` does not fire here.
//   * the kexec'd INNER kernel boots the inner initramfs; its /init prints
//     KEXEC_STAGE2_MARKER (the success signal — unreachable without a working kexec),
//     then `reboot -f` (a real reset) makes qemu exit.
// The outer and inner kernel are the SAME td bzImage (it already carries KEXEC_FILE +
// RELOCATABLE); the outer initramfs simply embeds a copy of it as /kernel/bzImage.
//
// Everything in both initramfs is td-built and STATIC, so each cpio is self-contained
// (the kexec'd inner kernel has no /td/store, only its initramfs): busybox and
// td-kexec are static ELFs packed as real `file` entries (not slinks into the store,
// unlike system-x86-64's stage-1), and the packer is the linux-x86-64-exported
// gen_init_cpio (itself HOSTCC `-static`). The inner cpio is packed first, then
// embedded as a `file` in the outer cpio.
pub fn recipe() -> Recipe {
    let mut steps = Vec::new();

    // ── Inner initramfs: static busybox + a /init that prints STAGE2 then resets. ──
    steps.push(Step::WriteFile {
        path: "{root}/inner-init".into(),
        content: format!(
            "#!/bin/sh\n\
             echo {KEXEC_STAGE2_MARKER}\n\
             exec /bin/busybox reboot -f\n"
        ),
        exec: true,
    });
    steps.push(Step::WriteFile {
        path: "{root}/inner-spec".into(),
        content: "dir /dev 0755 0 0\n\
                  nod /dev/console 0600 0 0 c 5 1\n\
                  dir /bin 0755 0 0\n\
                  file /bin/busybox {in:busybox-x86-64}/bin/busybox 0755 0 0\n\
                  slink /bin/sh /bin/busybox 0777 0 0\n\
                  file /init {root}/inner-init 0755 0 0\n"
            .into(),
        exec: false,
    });
    // gen_init_cpio writes the newc cpio to stdout (SH for the `>` redirect). `-t 1`
    // pins a fixed mtime on every entry so the cpio is reproducible (the /init files
    // are written fresh by this build, so their stat mtime would otherwise be the
    // wall-clock build time — see linux-x86-64.rs).
    steps.push(
        Step::run(
            "{root}",
            &[
                SH,
                "-c",
                "'{in:linux-x86-64}/gen_init_cpio' -t 1 '{root}/inner-spec' > '{root}/inner.cpio'",
            ],
        )
        .env("PATH", &mesboot0_path()),
    );

    // ── Outer initramfs: static busybox + td-kexec + a copy of the bzImage + the
    //    inner cpio + a /init that prints STAGE1 then execs td-kexec. The inner cmdline
    //    mirrors the qemu-boot base (console=ttyS0 panic=-1 rdinit=/init) so STAGE2
    //    lands on the same serial and an inner panic resets (=> qemu exits). If td-kexec
    //    returns at all it FAILED (reboot(KEXEC) does not return on success), so the
    //    /init prints a failure line and resets so the oracle reds with STAGE1-but-no-
    //    STAGE2 rather than hanging. ──
    steps.push(Step::WriteFile {
        path: "{root}/outer-init".into(),
        content: format!(
            "#!/bin/sh\n\
             echo {KEXEC_STAGE1_MARKER}\n\
             /bin/td-kexec /kernel/bzImage /inner.cpio 'console=ttyS0 panic=-1 rdinit=/init'\n\
             echo TD-KEXEC-STAGE1-FAILED\n\
             exec /bin/busybox reboot -f\n"
        ),
        exec: true,
    });
    steps.push(Step::WriteFile {
        path: "{root}/outer-spec".into(),
        content: "dir /dev 0755 0 0\n\
                  nod /dev/console 0600 0 0 c 5 1\n\
                  dir /bin 0755 0 0\n\
                  file /bin/busybox {in:busybox-x86-64}/bin/busybox 0755 0 0\n\
                  slink /bin/sh /bin/busybox 0777 0 0\n\
                  file /bin/td-kexec {in:td-kexec}/bin/td-kexec 0755 0 0\n\
                  dir /kernel 0755 0 0\n\
                  file /kernel/bzImage {in:linux-x86-64}/bzImage 0644 0 0\n\
                  file /inner.cpio {root}/inner.cpio 0644 0 0\n\
                  file /init {root}/outer-init 0755 0 0\n"
            .into(),
        exec: false,
    });
    steps.push(
        Step::run(
            "{root}",
            &[
                SH,
                "-c",
                "'{in:linux-x86-64}/gen_init_cpio' -t 1 '{root}/outer-spec' > '{root}/outer-initramfs.cpio'",
            ],
        )
        .env("PATH", &mesboot0_path()),
    );

    // ── Land the bootable bzImage + the outer initramfs. ──
    steps.push(Step::MkDir {
        path: "{out}".into(),
    });
    steps.push(Step::CopyFiles {
        files: vec![
            "{in:linux-x86-64}/bzImage".into(),
            "{root}/outer-initramfs.cpio".into(),
        ],
        dest: "{out}".into(),
    });
    steps.push(Step::Require {
        paths: vec![
            "{out}/bzImage".into(),
            "{out}/outer-initramfs.cpio".into(),
        ],
        exec: false,
    });

    Recipe::mesboot("kexec-spike-x86-64", "0.1")
        .native_inputs(&["linux-x86-64", "busybox-x86-64", "td-kexec"])
        .inputs_owned(mesboot0_inputs(&[]))
        .steps(steps)
}

use crate::ladder::{mesboot0_inputs, mesboot0_path, KEXEC_STAGE1_MARKER, KEXEC_STAGE2_MARKER, SH};
use crate::types::{CheckRunner, Recipe, RecipeCheck, Step};

// kexec-spike-x86-64-test: gated shape validation of the two-kernel spike artifact.
// The actual kexec BOOT needs host qemu and belongs to the operator oracle
// (`td-recipe-eval qemu-boot-kexec`, OUTSIDE the sandbox); this asserts — per repo
// policy that recipes test their output — that the packed artifact is well-formed and
// complete WITHOUT booting it. Four things, mirroring linux-x86-64-test:
//   1. {out}/bzImage carries the x86 boot-setup header (0xAA55 at 0x1fe, "HdrS" at
//      0x202) — it is a real bootable image, the one qemu -kernel loads,
//   2. {out}/outer-initramfs.cpio is a COMPLETE newc cpio (070701 magic; busybox
//      `cpio -t` parses it — reds on a truncated stream) carrying every spike member:
//      init, bin/busybox, bin/sh, bin/td-kexec, dev/console, kernel/bzImage (the
//      embedded second-boot kernel), and inner.cpio (the embedded inner initramfs).
//      A dropped member (a gen_init_cpio spec typo, a missing input) reds on its name.
//   3. STAGE1 is byte-present in the outer archive (the outer /init packs it
//      directly); and the embedded inner.cpio is EXTRACTED from the outer archive
//      and independently validated — `cpio -t`-parsed (reds on a truncated/corrupt
//      inner newc stream), its OWN members listed (init, bin/busybox, bin/sh,
//      dev/console), and byte-grepped for STAGE2. The structural rejection of a
//      truncated/corrupt inner is the `cpio -t` parse of the EXTRACTED inner.cpio
//      (not the STAGE2 byte-grep); extracting first — rather than grepping the whole
//      outer blob for STAGE2 — is what ties the marker to a well-formed inner archive.
// The behavioural proof that it actually kexecs is `qemu-boot-kexec`, which cannot run
// in this host-free BuildOnly rung.
pub fn recipe() -> Recipe {
    let bzimage = "{in:kexec-spike-x86-64}/bzImage";
    let initramfs = "{in:kexec-spike-x86-64}/outer-initramfs.cpio";
    let bb = "{in:busybox-x86-64}/bin/busybox";
    let stage1 = KEXEC_STAGE1_MARKER;
    let stage2 = KEXEC_STAGE2_MARKER;
    let mut steps = Vec::new();

    steps.push(
        Step::run(
            "{root}",
            &[
                SH,
                "-c",
                &format!(
                    "sz=$(wc -c < '{bzimage}'); \
                     [ \"$sz\" -ge 65536 ] || {{ echo \"bzImage implausibly small ($sz bytes)\" >&2; exit 1; }}; \
                     set -- $(od -An -tx1 -j 510 -N 2 '{bzimage}'); \
                     [ \"$1$2\" = 55aa ] || {{ echo 'bzImage missing the 0xAA55 boot signature at 0x1fe' >&2; exit 1; }}; \
                     set -- $(od -An -tx1 -j 514 -N 4 '{bzimage}'); \
                     [ \"$1$2$3$4\" = 48647253 ] || {{ echo 'bzImage missing the HdrS setup-header magic at 0x202' >&2; exit 1; }}"
                ),
            ],
        )
        .env("PATH", &mesboot0_path()),
    );
    steps.push(
        Step::run(
            "{root}",
            &[
                SH,
                "-c",
                &format!(
                    "sz=$(wc -c < '{initramfs}'); \
                     [ \"$sz\" -ge 65536 ] || {{ echo \"outer-initramfs.cpio implausibly small ($sz bytes)\" >&2; exit 1; }}; \
                     set -- $(od -An -tx1 -N 6 '{initramfs}'); \
                     [ \"$1$2$3$4$5$6\" = 303730373031 ] || {{ echo 'outer-initramfs.cpio missing the newc cpio magic 070701' >&2; exit 1; }}; \
                     list=$('{bb}' cpio -t < '{initramfs}' 2>/dev/null) || {{ echo 'outer-initramfs.cpio: busybox cpio -t could not parse the archive (truncated/corrupt newc stream)' >&2; exit 1; }}; \
                     for m in init bin/busybox bin/sh bin/td-kexec dev/console kernel/bzImage inner.cpio; do \
                         printf '%s\\n' \"$list\" | grep -q -x -F \"$m\" || {{ echo \"outer-initramfs.cpio: cpio member '$m' missing — the two-kernel spike is incomplete\" >&2; exit 1; }}; \
                     done; \
                     grep -q -a {stage1} '{initramfs}' || {{ echo 'outer-initramfs.cpio: outer /init STAGE1 marker not packed' >&2; exit 1; }}; \
                     '{bb}' cpio -i inner.cpio < '{initramfs}' >/dev/null 2>&1 || true; \
                     [ -f inner.cpio ] || {{ echo 'outer-initramfs.cpio: could not extract the embedded inner.cpio member' >&2; exit 1; }}; \
                     ilist=$('{bb}' cpio -t < inner.cpio 2>/dev/null) || {{ echo 'inner.cpio is not a parseable newc archive (truncated/corrupt embedded inner initramfs)' >&2; exit 1; }}; \
                     for m in init bin/busybox bin/sh dev/console; do \
                         printf '%s\\n' \"$ilist\" | grep -q -x -F \"$m\" || {{ echo \"inner.cpio: member '$m' missing — the embedded inner initramfs is incomplete\" >&2; exit 1; }}; \
                     done; \
                     grep -q -a {stage2} inner.cpio || {{ echo 'inner.cpio: STAGE2 marker not inside the embedded inner initramfs (the nested second-stage init is not packed)' >&2; exit 1; }}"
                ),
            ],
        )
        .env("PATH", &mesboot0_path()),
    );

    steps.push(Step::MkDir {
        path: "{out}".into(),
    });
    steps.push(Step::WriteFile {
        path: "{out}/result".into(),
        content: "PASS: kexec-spike-x86-64 is a well-formed two-kernel boot artifact — a bootable bzImage (0xAA55 + HdrS) plus a complete newc outer initramfs carrying the static busybox, td-kexec, an embedded second-boot bzImage, and a nested inner initramfs, with both the STAGE1 and STAGE2 /init markers packed. The behavioural kexec boot is the operator qemu-boot-kexec oracle.\n".into(),
        exec: false,
    });
    steps.push(Step::Require {
        paths: vec!["{out}/result".into()],
        exec: false,
    });

    Recipe::mesboot("kexec-spike-x86-64-test", "1.0")
        .native_inputs(&["kexec-spike-x86-64", "busybox-x86-64"])
        .inputs_owned(mesboot0_inputs(&[]))
        .steps(steps)
        .checks(vec![RecipeCheck::daily(
            r#"
echo ">> recipe-check kexec-spike-x86-64-test: build-plan --auto builds kexec-spike-x86-64 (the two-kernel kexec spike artifact: a bootable bzImage + an outer initramfs embedding static busybox, td-kexec, a second-boot bzImage, and a nested inner initramfs) and asserts a complete newc archive carrying every spike member and both /init markers"
: "${TD_RECIPE_EVAL:=$PWD/recipes/target/release/td-recipe-eval}"
exec "$TD_RECIPE_EVAL" check-run kexec-spike-x86-64-test daily 1
"#,
        )
        .with_runner(CheckRunner::BuildOnly)])
}

use crate::ladder::{initramfs_cpio_shape_check, mesboot0_inputs, mesboot0_path, SH};
use crate::types::{CheckRunner, Recipe, RecipeCheck, Step};

// linux-x86-64-test: behavioral validation of the source-built kernel (#529).
// A qemu BOOT is still out of scope for this rung (it belongs with the later
// qemu boot step, which must run OUTSIDE the host-free sandbox); instead this
// asserts both artifacts — the `vmlinux` ELF and the bootable `bzImage` — are
// well-formed, per repo policy that recipes test their output. Checks:
//   1. vmlinux is an ELF64 x86-64 *executable* (readelf: class ELF64, machine
//      x86-64, type EXEC) — the EXEC assertion proves it was linked, so a stray
//      relocatable `.o` (which would still be ELF64/x86-64 and carry the banner
//      via init/version.o) cannot satisfy the check,
//   2. vmlinux carries the `Linux version 7.1.4` banner (init/version.o's
//      linux_banner[], always obj-y) — proof the kernel actually compiled and
//      linked, not just that some ELF exists,
//   3. vmlinux embeds no `/gnu/store` bytes — the no-guix host-free leg, mirroring
//      busybox-test (td's native toolchain is /td/store; a /gnu/store byte would
//      mean a host-guix compiler/lib leaked into the image),
//   4. bzImage is well-formed: a size floor (>= 64 KiB) rejects a header-only or
//      truncated image; the 0xAA55 boot signature at 0x1fe and the "HdrS" magic at
//      0x202 (arch/x86/boot/header.S, read with od's offset seek since mesboot0
//      ships no dd) prove the boot-setup header; and the gzip magic `1f 8b 08`
//      appears somewhere in the file, proving the CONFIG_KERNEL_GZIP payload was
//      actually compressed and embedded — not just the raw ELF wrapped in a stub.
//   5. bzImage has PE32+ x86-64 EFI application headers and a file-backed
//      executable entry point, checked by the bounded Rust header reader.
//      This is a format check, not a firmware boot or Secure Boot proof.
//   6. initramfs.cpio is a real, COMPLETE newc cpio carrying the whole bootable
//      userland — via the shared `initramfs_cpio_shape_check` helper (ladder.rs)
//      that the producer rung runs too, so the two cannot drift. It parses the
//      archive with busybox `cpio -t` (a real newc walk that reds on a truncated/
//      corrupt stream) and asserts init/bin/busybox/bin/sh/dev/console are all
//      present plus the `TD-USERLAND-OK` /init marker. The behavioural proof that it
//      actually boots is the host-side `td-recipe-eval qemu-boot linux-x86-64` tool
//      (host qemu), which cannot run in this host-free BuildOnly rung.
//   7. The linked vmlinux contains dm-crypt, generic/accelerated AES-XTS,
//      HMAC, SHA-256 and the AF_ALG hash/skcipher entry
//      points. Checking the ELF rejects a kernel built without them even if
//      a recipe or stale .config claims they were selected. This is structural
//      evidence; encrypted I/O and unlock belong to the later VM oracle.
pub fn recipe() -> Recipe {
    let vmlinux = "{in:linux-x86-64}/vmlinux";
    let bzimage = "{in:linux-x86-64}/bzImage";
    let initramfs = "{in:linux-x86-64}/initramfs.cpio";
    let readelf = "{in:binutils-x86-64-native}/bin/readelf";
    let nm = "{in:binutils-x86-64-native}/bin/nm";
    let mut steps = Vec::new();

    steps.push(
        Step::run(
            "{root}",
            &[
                SH,
                "-c",
                &format!(
                    "h=$('{readelf}' -h '{vmlinux}'); \
                     printf '%s\\n' \"$h\" | grep -i 'class:'   | grep -qi 'ELF64'  || {{ echo 'vmlinux is not ELF64' >&2; exit 1; }}; \
                     printf '%s\\n' \"$h\" | grep -i 'machine:' | grep -qi 'x86-64' || {{ echo 'vmlinux is not x86-64' >&2; exit 1; }}; \
                     printf '%s\\n' \"$h\" | grep -i 'type:'    | grep -qi 'EXEC'   || {{ echo 'vmlinux is not a linked ELF executable (EXEC) — a stray relocatable .o would still be ELF64/x86-64 and carry the banner' >&2; exit 1; }}"
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
                    "grep -q -a 'Linux version 7.1.4' '{vmlinux}' || {{ echo 'vmlinux is missing the Linux 7.1.4 banner' >&2; exit 1; }}"
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
                    "if grep -q -a /gnu/store '{vmlinux}'; then echo 'vmlinux embeds /gnu/store bytes' >&2; exit 1; fi"
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
                    "sz=$(wc -c < '{bzimage}'); \
                     [ \"$sz\" -ge 65536 ] || {{ echo \"bzImage is implausibly small ($sz bytes) — a header-only/truncated image\" >&2; exit 1; }}; \
                     set -- $(od -An -tx1 -j 510 -N 2 '{bzimage}'); \
                     [ \"$1$2\" = 55aa ] || {{ echo 'bzImage is missing the 0xAA55 boot signature at 0x1fe' >&2; exit 1; }}; \
                     set -- $(od -An -tx1 -j 514 -N 4 '{bzimage}'); \
                     [ \"$1$2$3$4\" = 48647253 ] || {{ echo 'bzImage is missing the HdrS setup-header magic at 0x202' >&2; exit 1; }}; \
                     od -An -tx1 -v '{bzimage}' | tr -d '\\n' | grep -q ' 1f 8b 08' || {{ echo 'bzImage embeds no gzip payload (1f 8b 08) — CONFIG_KERNEL_GZIP payload missing' >&2; exit 1; }}"
                ),
            ],
        )
        .env("PATH", &mesboot0_path()),
    );
    steps.push(Step::AssertEfiApplication {
        path: bzimage.into(),
    });
    let initramfs_check = initramfs_cpio_shape_check(initramfs, "{in:busybox-x86-64}/bin/busybox");
    steps.push(Step::run("{root}", &[SH, "-c", &initramfs_check]).env("PATH", &mesboot0_path()));

    steps.push(
        Step::run(
            "{root}",
            &[
                SH,
                "-c",
                &format!(
                    "'{nm}' '{vmlinux}' > encryption-symbols || exit 1; \
                     for symbol in crypt_ctr crypto_aes_mod_init xts_module_init aesni_init hmac_module_init crypto_sha256_mod_init algif_hash_init algif_skcipher_init; do \
                       grep -q \"[[:space:]][tT][[:space:]]$symbol$\" encryption-symbols || {{ echo \"vmlinux is missing the built-in encryption function $symbol\" >&2; exit 1; }}; \
                     done"
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
        content: "PASS: Linux 7.1.4, source-built by the native /td/store x86_64 toolchain — vmlinux is a well-formed ELF64 x86-64 image carrying the Linux banner and built-in dm-crypt with generic/accelerated AES-XTS and the AF_ALG hash/skcipher interfaces, bzImage carries the x86 boot-setup header (0xAA55 + HdrS) and x86-64 PE headers, and initramfs.cpio is a newc cpio carrying the static busybox userland\n".into(),
        exec: false,
    });
    steps.push(Step::Require {
        paths: vec!["{out}/result".into()],
        exec: false,
    });

    Recipe::mesboot("linux-x86-64-test", "1.0")
        .native_inputs(&["linux-x86-64", "binutils-x86-64-native", "busybox-x86-64"])
        .inputs_owned(mesboot0_inputs(&[]))
        .steps(steps)
        .checks(vec![RecipeCheck::new(
            r#"
echo ">> recipe-check linux-x86-64-test: build-plan --auto builds linux-x86-64 (Linux 7.1.4 vmlinux + bzImage + busybox initramfs, source-built by the native /td/store x86_64 GCC 14 + glibc 2.41 toolchain) and asserts a well-formed ELF64 x86-64 vmlinux with the Linux banner and built-in dm-crypt with generic/accelerated AES-XTS and the AF_ALG hash/skcipher interfaces, a bzImage carrying x86 boot-setup and EFI application headers, and a newc initramfs.cpio carrying the static busybox userland"
: "${TD_RECIPE_EVAL:=$PWD/target/release/td-recipe-eval}"
exec "$TD_RECIPE_EVAL" check-run linux-x86-64-test 1
"#,
        )
        .with_runner(CheckRunner::BuildOnly)])
}

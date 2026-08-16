use crate::ladder::{post_bootstrap_path, POST_BOOTSTRAP_SH};
use crate::types::{CheckRunner, Recipe, RecipeCheck, Step};

// Exercise the target-linked installer against a regular-file destination — the
// one D9 says the oracle exercises, down the same code path the block device
// takes — and read the result back as BYTES AT OFFSETS rather than through the
// crate's own parser. The cargo tests already parse what they wrote with
// `gpt::parse`, so a writer and a reader that agreed with each other and with
// nothing else would satisfy them; what firmware looks for is a signature at a
// fixed offset.
//
// The offsets are for the 512-byte sectors a regular file takes, which is what
// lets them be written down at all: the ESP begins at the 1 MiB alignment, its
// FAT32 boot sector carries the label at `BS_VolLab`, and the GPT header sits
// in LBA 1 with its disk GUID 56 bytes into it. The BACKUP header is read too,
// out of the last LBA, because it is the copy the `last_usable`/backup
// arithmetic can put in the wrong place while every primary-table check passes.
const ESP_AT: u64 = 1024 * 1024;
const DISK_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const SECTOR: u64 = 512;

pub fn recipe() -> Recipe {
    let bin = "{in:td-install}/bin/td-install";
    let disk = "{root}/disk.img";
    // "EFI PART", the GPT header signature, and the 0xaa55 that both a
    // protective MBR and a FAT boot sector end with.
    let gpt_sig = "4546492050415254";
    let boot_sig = "55aa";
    // "TD-ESP" padded to the 11-byte field with spaces.
    let label = "54442d4553502020202020";
    let esp_boot_sig_at = ESP_AT + 510;
    let label_at = ESP_AT + 71;
    let backup_at = DISK_BYTES - SECTOR;
    // The four reads that say a td disk is there, shared so the REINSTALL is
    // held to the same standard as the install rather than to a weaker one: a
    // second pass that rewrote the table and destroyed the ESP under it would
    // otherwise pass, which is the failure the write order exists for.
    let layout_is_intact = format!(
        "set -- $(od -An -tx1 -j 510 -N 2 '{disk}'); \
         [ \"$1$2\" = '{boot_sig}' ] || {{ echo \"no protective-MBR boot signature: $1$2\" >&2; exit 1; }}; \
         set -- $(od -An -tx1 -j 512 -N 8 '{disk}'); \
         [ \"$1$2$3$4$5$6$7$8\" = '{gpt_sig}' ] || {{ echo \"no GPT header signature in LBA 1: $1$2$3$4$5$6$7$8\" >&2; exit 1; }}; \
         set -- $(od -An -tx1 -j {backup_at} -N 8 '{disk}'); \
         [ \"$1$2$3$4$5$6$7$8\" = '{gpt_sig}' ] || {{ echo \"no GPT header signature in the last LBA: $1$2$3$4$5$6$7$8\" >&2; exit 1; }}; \
         set -- $(od -An -tx1 -j {esp_boot_sig_at} -N 2 '{disk}'); \
         [ \"$1$2\" = '{boot_sig}' ] || {{ echo \"the ESP has no FAT boot signature: $1$2\" >&2; exit 1; }}; \
         set -- $(od -An -tx1 -j {label_at} -N 11 '{disk}'); \
         [ \"$1$2$3$4$5$6$7$8$9${{10}}${{11}}\" = '{label}' ] || {{ echo 'the ESP is not labelled TD-ESP' >&2; exit 1; }}"
    );

    let steps = vec![
        // Engine-native, so the destination costs no `dd`: that would be an
        // applet `busybox-x86-64`'s contract does not declare, which is the
        // shape D7 exists to refuse.
        Step::Truncate {
            path: disk.into(),
            bytes: DISK_BYTES,
        },
        Step::run(
            "{root}",
            &[
                POST_BOOTSTRAP_SH,
                "-c",
                &format!(
                    // The size is CHECKED as well as asked for, because
                    // td-install asks the DESTINATION how big it is: a
                    // destination of another size would be a disk this file's
                    // offsets do not describe.
                    "sz=$(wc -c < '{disk}') || exit 1; \
                     [ \"$sz\" -eq {DISK_BYTES} ] || {{ echo \"the destination is $sz bytes, not {DISK_BYTES}\" >&2; exit 1; }}; \
                     '{bin}' layout '{disk}' || {{ echo 'td-install layout failed on a regular file' >&2; exit 1; }}; \
                     {layout_is_intact}"
                ),
            ],
        )
        .env("PATH", &post_bootstrap_path()),
        // A REINSTALL over the disk just written. The disk GUID must be freshly
        // drawn, since a fixed one would have every td disk claim the same
        // identity, and the whole layout must still be there afterwards.
        Step::run(
            "{root}",
            &[
                POST_BOOTSTRAP_SH,
                "-c",
                &format!(
                    "before=$(od -An -tx1 -j 568 -N 16 '{disk}') || exit 1; \
                     '{bin}' layout '{disk}' || {{ echo 'td-install layout failed on a reinstall' >&2; exit 1; }}; \
                     after=$(od -An -tx1 -j 568 -N 16 '{disk}') || exit 1; \
                     [ \"$before\" != \"$after\" ] || {{ echo 'two installs drew the same disk GUID' >&2; exit 1; }}; \
                     {layout_is_intact}"
                ),
            ],
        )
        .env("PATH", &post_bootstrap_path()),
        Step::MkDir {
            path: "{out}".into(),
        },
        Step::WriteFile {
            path: "{out}/result".into(),
            content: "PASS: target-built static td-install writes a GPT with its backup, a labelled FAT32 ESP, and a freshly drawn disk GUID on install and reinstall\n".into(),
            exec: false,
        },
        Step::Require {
            paths: vec!["{out}/result".into()],
            exec: false,
        },
    ];

    Recipe::mesboot("td-install-test", "1.0")
        .native_inputs(&["td-install", "busybox-x86-64"])
        .steps(steps)
        .checks(vec![RecipeCheck::new(
            r#"
echo ">> recipe-check td-install-test: target static installer lays out a GPT and a labelled FAT32 ESP on a regular file"
: "${TD_RECIPE_EVAL:=$PWD/target/release/td-recipe-eval}"
exec "$TD_RECIPE_EVAL" check-run td-install-test 1
"#,
        )
        .with_runner(CheckRunner::BuildOnly)])
}

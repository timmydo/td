use crate::ladder::{post_bootstrap_path, POST_BOOTSTRAP_SH};
use crate::types::{CheckRunner, Recipe, RecipeCheck, Step};

// The on-disk shape, from the one file that declares it: the program name this
// check binds at build time (D7) and the sizes the offsets below are computed
// from, rather than a second copy of either.
#[path = "../../../td-boot/src/protocol.rs"]
#[allow(dead_code)]
mod td_boot_protocol;

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
// The volume follows the ESP, and lands exactly there because the ESP's size is
// a whole number of 1 MiB alignments — so this is the same number `plan()`
// computes rather than a guess, and the check reads it back to say so.
const VOLUME_AT: u64 = ESP_AT + td_boot_protocol::ESP_BYTES;
// Btrfs superblock positions, MEASURED on a real `mkfs.btrfs --byte-count`
// image rather than read off a struct definition: the primary lives at 64 KiB
// and its `magic` 64 bytes into it, the first mirror at 64 MiB, and the label
// 299 bytes into the primary. The mirror is the one that matters most here —
// it is 64 MiB INTO the volume, so it can only be on the destination if the
// sparse copy walked that far rather than stopping after the first live chunk.
const SUPERBLOCK_AT: u64 = 65536;
const SUPERBLOCK_MIRROR_AT: u64 = 64 * 1024 * 1024;
const MAGIC_IN_SUPERBLOCK: u64 = 64;
const LABEL_IN_SUPERBLOCK: u64 = 299;

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
    // The four reads that say a td disk is there, shared by all THREE passes so
    // none is held to a weaker standard than the install: a second pass that
    // rewrote the table and destroyed the ESP under it would otherwise pass,
    // which is the failure the write order exists for. The VOLUME pass needs it
    // most, and got it last — it is the one that writes at offsets it DERIVED
    // rather than at ones this file names, so a region computed wrongly lands
    // on the table or the ESP, and every check that only reads inside the
    // partition stays green while the disk stops booting.
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

    let mkfs = format!(
        "{{in:btrfs-progs-x86-64}}/bin/{}",
        td_boot_protocol::MKFS_BTRFS
    );
    let btrfs = "{in:btrfs-progs-x86-64}/bin/btrfs";
    // `_BHRfS_M`, and "td-system" — the label `protocol.rs` names, read out of
    // the copy on the DISK rather than out of the image it was made in.
    let btrfs_magic = "5f42485266535f4d";
    let volume_label = "74642d73797374656d";
    let magic_at = VOLUME_AT + SUPERBLOCK_AT + MAGIC_IN_SUPERBLOCK;
    let mirror_at = VOLUME_AT + SUPERBLOCK_MIRROR_AT + MAGIC_IN_SUPERBLOCK;
    let volume_label_at = VOLUME_AT + SUPERBLOCK_AT + LABEL_IN_SUPERBLOCK;
    // The `@var` subvolume is the reason the volume exists at all — the boot
    // path mounts it on /var — and nothing above sees it: a superblock, a label
    // and a clean `btrfs check` are all identical with and without it. The root
    // TREE is where a subvolume appears, and `dump-tree -t root` reads it out of
    // an unmounted image, which is the only kind this check has. A plain
    // DIRECTORY of the same name lives in the FS tree instead and does not
    // appear here, so this cannot be satisfied by `--rootdir` alone.
    //
    // Read-WRITE is asserted by the absence of the flag rather than by its
    // presence: `--subvol ro:@var` writes `flags 0x1(RDONLY)` on that root, and
    // a read-only /var is a machine that boots and then cannot write anything
    // down. Matched with `case`, so no applet outside the shell is needed.
    let subvol_exists = format!(
        "tree=$('{btrfs}' inspect-internal dump-tree -t root '{{root}}/scratch/td-volume.img') || \
         {{ echo 'the volume has no readable root tree' >&2; exit 1; }}; \
         case \"$tree\" in *'name {subvol}'*) ;; \
         *) echo 'the volume has no {subvol} subvolume' >&2; exit 1 ;; esac; \
         case \"$tree\" in *RDONLY*) echo 'the {subvol} subvolume is read-only' >&2; exit 1 ;; \
         *) ;; esac",
        subvol = td_boot_protocol::VOLUME_SUBVOL
    );
    let volume_is_on_the_disk = format!(
        "set -- $(od -An -tx1 -j {magic_at} -N 8 '{disk}'); \
         [ \"$1$2$3$4$5$6$7$8\" = '{btrfs_magic}' ] || {{ echo \"no Btrfs superblock at the volume partition: $1$2$3$4$5$6$7$8\" >&2; exit 1; }}; \
         set -- $(od -An -tx1 -j {mirror_at} -N 8 '{disk}'); \
         [ \"$1$2$3$4$5$6$7$8\" = '{btrfs_magic}' ] || {{ echo \"no Btrfs superblock mirror 64 MiB in — the copy stopped short: $1$2$3$4$5$6$7$8\" >&2; exit 1; }}; \
         set -- $(od -An -tx1 -j {volume_label_at} -N 9 '{disk}'); \
         [ \"$1$2$3$4$5$6$7$8$9\" = '{volume_label}' ] || {{ echo 'the volume on the disk is not labelled td-system' >&2; exit 1; }}"
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
        // D7's BINDING, and the reason it is a `Require` rather than a comment:
        // the one third-party program on the install path is named by
        // `protocol.rs` and composed into this path, so a btrfs-progs that
        // stopped shipping it — or a rename on either side — reds the build
        // instead of failing an install on a machine someone is standing in
        // front of. It is checked BEFORE the step that execs it, so the
        // diagnostic is about the missing program rather than about a volume.
        Step::Require {
            paths: vec![mkfs.clone()],
            exec: true,
        },
        Step::run(
            "{root}",
            &[
                POST_BOOTSTRAP_SH,
                "-c",
                &format!(
                    "'{bin}' volume '{disk}' '{mkfs}' '{{root}}/scratch' > '{{root}}/volume-out' || {{ echo 'td-install volume failed' >&2; exit 1; }}; \
                     read -r off len written < '{{root}}/volume-out' || exit 1; \
                     [ \"$off\" -eq {VOLUME_AT} ] || {{ echo \"the volume landed at $off, not {VOLUME_AT}\" >&2; exit 1; }}; \
                     [ \"$len\" -gt 0 ] && [ \"$written\" -gt 0 ] || {{ echo \"the volume is $len bytes and $written were written\" >&2; exit 1; }}; \
                     '{btrfs}' check --readonly --check-data-csum '{{root}}/scratch/td-volume.img' || {{ echo 'the filesystem td-install built does not check out' >&2; exit 1; }}; \
                     {subvol_exists}; \
                     {volume_is_on_the_disk}; \
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
            content: "PASS: target-built static td-install writes a GPT with its backup, a labelled FAT32 ESP, and a freshly drawn disk GUID on install and reinstall, then a checked Btrfs volume copied into its own partition\n".into(),
            exec: false,
        },
        Step::Require {
            paths: vec!["{out}/result".into()],
            exec: false,
        },
    ];

    Recipe::mesboot("td-install-test", "1.0")
        .native_inputs(&["td-install", "busybox-x86-64", "btrfs-progs-x86-64"])
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

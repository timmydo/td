use crate::ladder::{BUILD_ROOT_PATH as PATH, BUILD_ROOT_SH as SH};
use crate::types::{CheckRunner, Recipe, RecipeCheck, Step};

use crate::td_boot_protocol;

// The SAME fixture deployment td-boot's own tests verify, so the signature
// staged here is over the manifest these payloads hash to rather than over a
// second copy of them that could drift. td-boot cannot sign and no recipe
// builds `td-deploy`, so a publish this check can authenticate has to come
// from a committed vector; this is the only payload-matched one there is.
#[path = "../../../td-boot/src/fixture.rs"]
#[allow(dead_code)]
mod td_boot_fixture;

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
//
// The reads are the Debian build root's: its bash, od, cmp and gawk, with PATH
// its /usr/bin alone, and the td programs reached only by their store paths.
const ESP_AT: u64 = 1024 * 1024;
const DISK_BYTES: u64 = 6 * 1024 * 1024 * 1024;
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

/// The PUBLISH pass: a real signed deployment, put in the volume by the real
/// `td-boot`, read back out of the unmounted image.
///
/// This is what the two commits before it could not check. They pin td-install's
/// side of the contract against a shell script standing in for `td-boot`, so a
/// renamed verb or a reordered argument would leave both suites green and break
/// every install; here the two programs are the ones that ship, and the bundle
/// is authenticated rather than taken on trust — the key is passed, so a
/// signature that did not verify is a failed build.
///
/// Offline throughout. `td-btrfs list` walks an unmounted filesystem's
/// directory entries, which is the only kind this check has: nothing mounts,
/// nothing loops, and no partition device is needed, which is the whole of why
/// the publish goes through the staging tree (DESIGN §10 item 7c).
fn publish_steps(
    bin: &str,
    disk: &str,
    mkfs: &str,
    btrfs: &str,
    layout_is_intact: &str,
    subvol_exists: &str,
    volume_is_on_the_disk: &str,
) -> Vec<Step> {
    let td_boot = "{in:td-boot}/bin/td-boot";
    let deployment = "{root}/deployment";
    let key = "{root}/trusted.pub";
    // Built HERE from the shared definition rather than copied: the manifest is
    // the payload digests, so computing it is what makes a drifted payload a
    // signature that stops verifying instead of a fixture nobody rechecked.
    let manifest = td_boot_fixture::manifest("", td_engine::sha256::hex_digest);
    // `sha256(manifest)` — the deployment's identity, and the directory name
    // the publish creates. Computed rather than written down for the same
    // reason, and it is what the check below looks for in the image.
    let id = td_engine::sha256::hex_digest(manifest.as_bytes());
    let signature = td_boot_fixture::signature("").unwrap_or("");

    let mut steps = vec![Step::MkDir {
        path: deployment.into(),
    }];
    for (label, bytes) in td_boot_fixture::payloads("") {
        steps.push(Step::WriteFile {
            path: format!("{deployment}/{label}"),
            content: bytes,
            exec: false,
        });
    }
    steps.push(Step::WriteFile {
        path: format!("{deployment}/{}", td_boot_protocol::MANIFEST_NAME),
        content: manifest,
        exec: false,
    });
    // Hex plus a newline, which is what `td-deploy` writes and what the shipped
    // parser reads — so the fixture goes back in through that parser rather
    // than around it.
    steps.push(Step::WriteFile {
        path: format!("{deployment}/{}", td_boot_protocol::MANIFEST_SIG_NAME),
        content: format!("{signature}\n"),
        exec: false,
    });
    steps.push(Step::WriteFile {
        path: key.into(),
        content: format!("{}\n", td_boot_fixture::PUBLIC_KEY),
        exec: false,
    });
    // Named before the step that execs it, as `mkfs.btrfs` is: a td-boot that
    // stopped being built reds here rather than inside a shell diagnostic.
    steps.push(Step::Require {
        paths: vec![td_boot.into()],
        exec: true,
    });
    // `td-btrfs restore` writes into an existing empty directory.
    steps.push(Step::MkDir {
        path: "{root}/restored".into(),
    });

    // What the publish produced, read back out of the IMAGE rather than out of
    // the tree it was staged from.
    //
    // That distinction is the whole value of this step. The staging tree is an
    // ordinary directory and asserting against it says only that `td-boot`
    // wrote what it should; what has to be true is that `mkfs.btrfs --rootdir`
    // then carried it in UNCHANGED, and a check on the staging tree cannot see
    // the difference. `td-btrfs restore` gives the image's contents back as
    // ordinary files — the shape `td-btrfs-test` already uses — so
    // the selector's SYMLINK-ness and its target are both testable on what was
    // actually installed. A `--rootdir` that dereferenced the selector would
    // leave a regular file of the right name, which every name-only check
    // passes and no machine boots.
    //
    // `readlink` on the restored tree is also the one assertion that ties the
    // selector to THIS deployment rather than to any deployment, and the
    // payload comparison is what stops an empty directory of the right name
    // counting as a published bundle. `td-btrfs list` is kept for the id
    // because it reads the filesystem's own directory entries rather than a
    // restored copy of them.
    //
    // Compared with `cmp -s`, byte for byte, rather than through `cat`:
    // `$(cat f)` strips trailing newlines from BOTH sides, so two files
    // differing only there compare equal — and every file compared here is
    // one whose exact bytes are the assertion, the trust root most of all.
    //
    // The update channel is asserted for all three of its properties: it
    // exists, it is a DIRECTORY, and it is EMPTY. The last is the contract — a
    // machine has a channel and has been offered nothing — and this is also the
    // only check that an empty directory survives `mkfs.btrfs --rootdir` and
    // `td-btrfs restore` at all, which everything `td-boot update` does on an
    // installed machine rests on.
    //
    // `ls -A` and `[`, not `rmdir`, which would answer by changing the tree
    // under test. The exit status is CHECKED rather than discarded: `$(…)`
    // captures stdout only, so an `ls` that did not take `-A` would print its
    // usage to stderr and hand back the empty string, and an emptiness test
    // that reads that as "empty" passes whatever is in the directory.
    let deployment_is_in_the_volume = format!(
        "'{btrfs}' restore '{{root}}/scratch/td-volume.img' '{{root}}/restored' || \
         {{ echo 'the volume could not be restored' >&2; exit 1; }}; \
         target=$(readlink '{{root}}/restored/{boot}/{current}') || \
         {{ echo 'the {current} selector did not survive as a symlink' >&2; exit 1; }}; \
         [ \"$target\" = '{prefix}{id}' ] || \
         {{ echo \"the {current} selector points at $target, not {prefix}{id}\" >&2; exit 1; }}; \
         for f in {manifest} {sig} bzImage; do \
           [ -f \"{{root}}/restored/{deployments}/{id}/$f\" ] || \
           {{ echo \"the published $f is not in the volume at all\" >&2; exit 1; }}; \
           cmp -s \"{{root}}/restored/{deployments}/{id}/$f\" \"{{root}}/deployment/$f\" || \
           {{ echo \"the published $f is not the one that was signed\" >&2; exit 1; }}; \
         done; \
         [ -f '{{root}}/restored/{trusted}' ] && [ ! -L '{{root}}/restored/{trusted}' ] || \
         {{ echo 'the volume trust root is missing or not a regular file' >&2; exit 1; }}; \
         cmp -s '{{root}}/restored/{trusted}' '{key}' || \
         {{ echo 'the volume does not carry the key that authenticated it' >&2; exit 1; }}; \
         [ -d '{{root}}/restored/{channel}' ] || \
         {{ echo 'the volume has no update channel, or it is not a directory' >&2; exit 1; }}; \
         offered=$(ls -A '{{root}}/restored/{channel}') || \
         {{ echo 'the update channel could not be listed' >&2; exit 1; }}; \
         [ -z \"$offered\" ] || \
         {{ echo \"a fresh install offers $offered in its update channel\" >&2; exit 1; }}; \
         '{btrfs}' list '{{root}}/scratch/td-volume.img' > '{{root}}/published-listing' || \
         {{ echo 'the volume has no readable filesystem tree' >&2; exit 1; }}; \
         gawk '$1 == \"d\" && $6 == \"/{deployments}/{id}\" {{ found = 1 }} END {{ exit !found }}' \
         '{{root}}/published-listing' || \
         {{ echo 'the published deployment {id} is not in the volume' >&2; exit 1; }}",
        boot = td_boot_protocol::BOOT_DIR,
        deployments = td_boot_protocol::DEPLOYMENTS_DIR,
        trusted = td_boot_protocol::VOLUME_TRUSTED_KEY,
        channel = td_boot_protocol::VOLUME_CHANNEL_DIR,
        current = td_boot_protocol::CURRENT_SLOT,
        prefix = td_boot_protocol::SELECTOR_PREFIX,
        manifest = td_boot_protocol::MANIFEST_NAME,
        sig = td_boot_protocol::MANIFEST_SIG_NAME
    );

    steps.push(
        Step::run(
            "{root}",
            &[
                SH,
                "-c",
                &format!(
                    "'{bin}' volume '{disk}' '{mkfs}' '{{root}}/scratch' '{td_boot}' \
                     '{deployment}' '{key}' > '{{root}}/publish-out' \
                     || {{ echo 'td-install volume with a publish failed' >&2; exit 1; }}; \
                     read -r off len written < '{{root}}/publish-out' || exit 1; \
                     [ \"$off\" -eq {VOLUME_AT} ] || {{ echo \"the volume landed at $off, not {VOLUME_AT}\" >&2; exit 1; }}; \
                     [ \"$len\" -gt 0 ] && [ \"$written\" -gt 0 ] || {{ echo \"the volume is $len bytes and $written were written\" >&2; exit 1; }}; \
                     '{btrfs}' check '{{root}}/scratch/td-volume.img' \
                     || {{ echo 'the filesystem td-install built does not check out' >&2; exit 1; }}; \
                     {deployment_is_in_the_volume}; \
                     {subvol_exists}; \
                     {volume_is_on_the_disk}; \
                     {layout_is_intact}"
                ),
            ],
        )
        .env("PATH", PATH),
    );

    // ...and the key is LOAD-BEARING rather than passed along: the same publish
    // under an unrelated key must fail. Without this the check would pass just
    // as well if td-install dropped the argument, which is the fail-open its
    // arity rule exists to prevent.
    //
    // The REASON is asserted, not just the exit status. A non-zero exit alone
    // is satisfied by any failure at all — a full disk, a missing directory, a
    // td-install that segfaults before it reaches the key — so a test that
    // asked only for failure would keep passing while the thing it is about
    // stopped happening. `scratch2` is a live example rather than a
    // hypothetical: it does not exist when this runs, and it passes only
    // because `volume` creates its scratch tree BEFORE the publish, so the
    // failure that arrives really is the signature's. Measured on the built
    // binaries, which reported `manifest does not authenticate` and not ENOENT.
    // The pattern is the shared constant, not a third spelling of it: a reworded
    // diagnostic must move every site that greps for one.
    steps.push(Step::WriteFile {
        path: "{root}/other.pub".into(),
        content: format!("{}\n", td_boot_fixture::OTHER_PUBLIC_KEY),
        exec: false,
    });
    steps.push(
        Step::run(
            "{root}",
            &[
                SH,
                "-c",
                &format!(
                    "if '{bin}' volume '{disk}' '{mkfs}' '{{root}}/scratch2' '{td_boot}' \
                        '{deployment}' '{{root}}/other.pub' > '{{root}}/wrong-out' 2>'{{root}}/wrong-err'; \
                     then echo 'a deployment signed by another key was published anyway' >&2; exit 1; fi; \
                     err=$(cat '{{root}}/wrong-err') || exit 1; \
                     case \"$err\" in *'{unauthenticated}'*) ;; \
                     *) echo \"the publish failed, but not on the signature: $err\" >&2; exit 1 ;; esac; \
                     [ ! -e '{{root}}/scratch2/td-volume-root/{boot}/{current}' ] && \
                     [ ! -L '{{root}}/scratch2/td-volume-root/{boot}/{current}' ] || \
                     {{ echo 'a refused publish still left a selector' >&2; exit 1; }}; \
                     [ ! -e '{{root}}/scratch2/td-volume-root/{trusted}' ] && \
                     [ ! -L '{{root}}/scratch2/td-volume-root/{trusted}' ] || \
                     {{ echo 'a refused publish still left a trust root' >&2; exit 1; }}",
                    boot = td_boot_protocol::BOOT_DIR,
                    trusted = td_boot_protocol::VOLUME_TRUSTED_KEY,
                    current = td_boot_protocol::CURRENT_SLOT,
                    unauthenticated = td_boot_protocol::MANIFEST_UNAUTHENTICATED
                ),
            ],
        )
        .env("PATH", PATH),
    );

    steps
}

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

    let mkfs = format!("{{in:td-btrfs}}/bin/{}", td_boot_protocol::MKFS_BTRFS);
    let btrfs = "{in:td-btrfs}/bin/td-btrfs";
    // `_BHRfS_M`, and "td-system" — the label `protocol.rs` names, read out of
    // the copy on the DISK rather than out of the image it was made in.
    let btrfs_magic = "5f42485266535f4d";
    let volume_label = "74642d73797374656d";
    let magic_at = VOLUME_AT + SUPERBLOCK_AT + MAGIC_IN_SUPERBLOCK;
    let mirror_at = VOLUME_AT + SUPERBLOCK_MIRROR_AT + MAGIC_IN_SUPERBLOCK;
    let volume_label_at = VOLUME_AT + SUPERBLOCK_AT + LABEL_IN_SUPERBLOCK;
    // The `@var` subvolume is the reason the volume exists at all — the boot
    // path mounts it on /var — and nothing above sees it: a superblock, a label
    // and a clean `td-btrfs check` are all identical with and without it. The
    // root TREE is where a subvolume appears, and `td-btrfs list` marks the
    // directory a subvolume's root with `[subvolume ID rw|ro]` read out of it, in
    // an unmounted image, which is the only kind this check has. A plain
    // DIRECTORY of the same name carries no mark, so this cannot be satisfied
    // by `--rootdir` alone.
    //
    // Read-WRITE is asserted exactly: `--subvol ro:@var` lists the root `ro`,
    // and a read-only /var is a machine that boots and then cannot write
    // anything down.
    let subvol_exists = format!(
        "'{btrfs}' list '{{root}}/scratch/td-volume.img' > '{{root}}/subvol-listing' || \
         {{ echo 'the volume has no readable root tree' >&2; exit 1; }}; \
         gawk '$6 == \"/{subvol}\" && $7 == \"[subvolume\" {{ found = 1 }} END {{ exit !found }}' \
         '{{root}}/subvol-listing' || \
         {{ echo 'the volume has no {subvol} subvolume' >&2; exit 1; }}; \
         gawk '$6 == \"/{subvol}\" && $7 == \"[subvolume\" && $9 == \"rw]\" {{ found = 1 }} END {{ exit !found }}' \
         '{{root}}/subvol-listing' || \
         {{ echo 'the {subvol} subvolume is read-only' >&2; exit 1; }}",
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

    let mut steps = vec![
        // Engine-native: the size is the builder's, not a shell tool's.
        Step::Truncate {
            path: disk.into(),
            bytes: DISK_BYTES,
        },
        Step::run(
            "{root}",
            &[
                SH,
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
        .env("PATH", PATH),
        // A REINSTALL over the disk just written. The disk GUID must be freshly
        // drawn, since a fixed one would have every td disk claim the same
        // identity, and the whole layout must still be there afterwards.
        Step::run(
            "{root}",
            &[
                SH,
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
        .env("PATH", PATH),
        // D7's BINDING, and the reason it is a `Require` rather than a comment:
        // the formatter on the install path, from its own recipe, is named by
        // `protocol.rs` and composed into this path, so a td-btrfs that
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
                SH,
                "-c",
                &format!(
                    "'{bin}' volume '{disk}' '{mkfs}' '{{root}}/scratch' > '{{root}}/volume-out' || {{ echo 'td-install volume failed' >&2; exit 1; }}; \
                     read -r off len written < '{{root}}/volume-out' || exit 1; \
                     [ \"$off\" -eq {VOLUME_AT} ] || {{ echo \"the volume landed at $off, not {VOLUME_AT}\" >&2; exit 1; }}; \
                     [ \"$len\" -gt 0 ] && [ \"$written\" -gt 0 ] || {{ echo \"the volume is $len bytes and $written were written\" >&2; exit 1; }}; \
                     '{btrfs}' check '{{root}}/scratch/td-volume.img' || {{ echo 'the filesystem td-install built does not check out' >&2; exit 1; }}; \
                     {subvol_exists}; \
                     {volume_is_on_the_disk}; \
                     {layout_is_intact}"
                ),
            ],
        )
        .env("PATH", PATH),
    ];

    // The publish pass gets the DISK checks too, not only the deployment ones.
    // It runs last, so it is the pass that leaves the destination in the state
    // an operator would boot — and every deployment assertion reads the scratch
    // IMAGE, so a sparse copy that went wrong only on this path would leave the
    // disk without a superblock at the volume offset with all of them green.
    steps.extend(publish_steps(
        bin,
        disk,
        &mkfs,
        btrfs,
        &layout_is_intact,
        &subvol_exists,
        &volume_is_on_the_disk,
    ));

    steps.extend(vec![
        Step::MkDir {
            path: "{out}".into(),
        },
        Step::WriteFile {
            path: "{out}/result".into(),
            content: "PASS: target-built static td-install writes a GPT with its backup, a labelled FAT32 ESP, and a freshly drawn disk GUID on install and reinstall, then a checked Btrfs volume copied into its own partition, with a signed deployment published into it by the target-built td-boot, the authenticating key carried onto the volume beside it, and both refused under an unrelated key\n".into(),
            exec: false,
        },
        Step::Require {
            paths: vec!["{out}/result".into()],
            exec: false,
        },
    ]);

    Recipe::mesboot("td-install-test", "1.0")
        .build_root()
        .native_inputs(&["td-install", "td-boot", "td-btrfs"])
        .steps(steps)
        .checks(vec![RecipeCheck::new(
            r#"
echo ">> recipe-check td-install-test: target static installer lays out a GPT and a labelled FAT32 ESP on a regular file, then publishes a signed deployment into the volume through the target-built td-boot"
: "${TD_RECIPE_EVAL:=$PWD/target/release/td-recipe-eval}"
exec "$TD_RECIPE_EVAL" check-run td-install-test 1
"#,
        )
        .with_runner(CheckRunner::BuildOnly)])
}

#[cfg(test)]
mod tests {
    use super::{td_boot_fixture, td_boot_protocol, Step};

    /// The fixture this recipe stages really carries a signature.
    ///
    /// `signature("")` is an `Option` because `fixture.rs` is ordinary compiled
    /// code here and may not panic, and the call site falls back to `""`. That
    /// fallback is fail-CLOSED — an empty `manifest.sig` fails to decode and
    /// the publish is refused — but the refusal arrives at the far end of a
    /// build, in the one tier that does not run on every host. So the fact it
    /// depends on is asserted where `cargo test` can see it.
    #[test]
    fn the_staged_fixture_has_a_signature_to_stage() {
        let signature = td_boot_fixture::signature("").expect("the default tag is signed");
        assert_eq!(
            signature.len(),
            128,
            "an ed25519 signature is 64 bytes, written as 128 hex characters"
        );
        assert!(
            signature
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "the committed signature is not lowercase hexadecimal"
        );
    }

    /// The restored volume is checked for an EMPTY update channel.
    ///
    /// Everything `td-boot update` does on an installed machine rests on that
    /// directory being there and empty, and `td-install`'s own tests cannot
    /// say it: they read the staging tree, before a filesystem exists. Only
    /// this check sees the channel after `mkfs.btrfs --rootdir` and `td-btrfs
    /// restore` have both had it — which is the survival of an empty directory
    /// through them, the fact the whole channel design rests on. It runs in the
    /// tier that does not run on every host, so its PRESENCE is asserted where
    /// `cargo test` can see it, as the signature above is.
    #[test]
    fn the_restored_volume_is_checked_for_an_empty_update_channel() {
        // The EMPTINESS half specifically, since `[ -d … ]` alone would leave
        // the property this item rests on unchecked.
        let wanted = format!(
            "offered=$(ls -A '{{root}}/restored/{}')",
            td_boot_protocol::VOLUME_CHANNEL_DIR
        );
        assert!(
            super::recipe().steps.iter().flatten().any(|step| {
                matches!(step, Step::Run { argv, .. }
                    if argv.iter().any(|arg| arg.contains(&wanted)))
            }),
            "no step checks the restored volume's update channel; wanted {wanted}"
        );
    }

    /// Only the Debian build root's tools are on PATH; the installer, the
    /// td-boot it publishes through and the td-btrfs it formats with, which
    /// also read back its volume, run by their store paths.
    #[test]
    fn only_the_roots_tools_are_on_path() {
        let subjects = ["td-install", "td-boot", "td-btrfs"];
        crate::ladder::assert_build_root_test(&super::recipe(), &subjects, &subjects);
    }

    /// The deployment id the check looks for is the one td-boot will compute:
    /// `sha256(manifest)` over the manifest built from the shared payloads.
    #[test]
    fn the_manifest_names_the_payloads_that_are_staged() {
        let manifest = td_boot_fixture::manifest("", td_engine::sha256::hex_digest);
        assert!(manifest.starts_with("td-deployment-v1\n"));
        for (label, bytes) in td_boot_fixture::payloads("") {
            let digest = td_engine::sha256::hex_digest(bytes.as_bytes());
            assert!(
                manifest.contains(&format!("{digest}  {label}\n")),
                "the manifest does not name {label} by the digest of what is staged"
            );
        }
    }
}

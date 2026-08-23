pub const CURRENT_REJECTED_MARKER: &str = "TD-BOOT-CURRENT-REJECTED";
pub const ATTEMPT_CONSUMED_MARKER: &str = "TD-BOOT-ATTEMPT-CONSUMED";
pub const ATTEMPTS_EXHAUSTED_MARKER: &str = "TD-BOOT-ATTEMPTS-EXHAUSTED";
pub const BOOKKEEPING_UNAVAILABLE_MARKER: &str = "TD-BOOT-BOOKKEEPING-UNAVAILABLE";
pub const BOOKKEEPING_UNAVAILABLE_CMDLINE_TOKEN: &str = "td.boot-bookkeeping-unavailable=1";
pub const SELECTED_CURRENT_MARKER: &str = "TD-BOOT-SELECTED-CURRENT";
pub const SELECTED_PREVIOUS_MARKER: &str = "TD-BOOT-SELECTED-PREVIOUS";
// The manifest contract, shared because BOTH halves of a deployment must agree
// about it: td-boot reads and verifies, and the host-side `td-deploy` signs. A
// bound stated twice is a bound that can disagree with itself — a signer that
// accepted 8192 would emit signatures no machine could ever check, and the
// failure would surface at boot rather than at signing.
pub const MANIFEST_HEADER: &[u8] = b"td-deployment-v1";
pub const MANIFEST_NAME: &str = "manifest";
pub const MAX_MANIFEST_BYTES: u64 = 4096;
// Detached, beside the manifest: the deployment id stays sha256(manifest), so
// re-signing under a new key does not rename the deployment.
pub const MANIFEST_SIG_NAME: &str = "manifest.sig";
// An ed25519 signature is 64 bytes, which `td-deploy` writes as 128 hex
// characters and a newline. The bound is slack enough for a trailing CRLF and
// nothing like a payload: this file is read off a volume anyone who can write
// the disk can write, so it is bounded before it is read, as the manifest is.
pub const MAX_SIGNATURE_BYTES: u64 = 160;
// The trusted deployment key: 32 bytes as 64 hex characters and a newline, the
// shape `td-deploy keygen` writes and `tests/td-subst.pub` already established
// for a committed public half. Bounded before it is read for the signature's
// reason — whatever supplies it, td-boot reads it as a file and a file can be
// any size.
pub const MAX_PUBLIC_KEY_BYTES: u64 = 96;
// Where that key sits in the SELECTOR initramfs, relative to its root — the
// rootfs td-boot is running from when it selects and verifies, which is the
// artifact firmware loads and NOT the deployment's own initramfs. A key inside
// the deployment would be inside the thing being authenticated, and a verifier
// that reads its trust root out of its input authenticates nothing.
//
// Rootfs-relative rather than absolute because the harness writes it as a cpio
// entry name, and `engine/src/cpio.rs` refuses an absolute one; td-boot joins
// it to `/`. Shared here so the writer and the reader cannot disagree about the
// spelling — a mismatch is a key the kernel places somewhere td-boot never
// looks, which reports nothing on either side.
pub const TRUSTED_KEY_PATH: &str = "etc/td/deployment.pub";
pub const BOOT_DIR: &str = "td/boot";
pub const ATTEMPTS_DIR: &str = "td/boot/attempts";
pub const DEPLOYMENTS_DIR: &str = "td/deployments";
pub const SELECTOR_PREFIX: &str = "../deployments/";
// The trust root a RUNNING machine authenticates an update under, on the
// persistent volume rather than in any rootfs. `TRUSTED_KEY_PATH` above is the
// SELECTOR initramfs's copy and is gone after `switch_root`, so a booted system
// has no key at all until this one exists (DESIGN §10 item 10).
//
// Outside `DEPLOYMENTS_DIR` deliberately: a trust root inside the thing it
// authenticates vouches for nothing, and one owned by a deployment would be
// replaced by every update. Named `trusted.pub` rather than repeating the
// selector's `deployment.pub` so a path says WHICH of the two it is — and
// because it would otherwise differ from the `deployments` directory beside it
// by one character.
#[allow(dead_code)]
pub const VOLUME_TRUSTED_KEY: &str = "td/trusted.pub";
// The refusal a bundle gets when its signature, its bytes and the trust root do
// not agree. Shared because the boot oracle's negative pass GREPS for it: that
// pass installs under a valid-but-wrong key and requires a failure, and any
// other failure — a key that is not there, a busy volume lock, a missing
// candidate — would satisfy a test meant to prove the key was consulted. Stated
// once so a reworded diagnostic cannot quietly turn that check into a tautology.
#[allow(dead_code)]
pub const MANIFEST_UNAUTHENTICATED: &str = "manifest does not authenticate";
// An update CHANNEL is a directory holding at most one bundle, under this fixed
// name. Which bundle to install is the producer's decision and not a search:
// a manifest carries a header and payload digests and nothing that orders two
// of them, so td has no notion of "newer" to sort by and would be guessing.
// A producer stages elsewhere in the channel and renames onto this name, so no
// reader ever sees a half-written bundle; DESIGN §10 has why replacing an
// EXISTING one is not atomic and why a timer can afford that.
#[allow(dead_code)]
pub const CHANNEL_CANDIDATE: &str = "candidate";
// Where the volume keeps its own channel, for a producer writing into the disk
// rather than onto removable media. Beside `td/deployments` and outside it, for
// `VOLUME_TRUSTED_KEY`'s reason: an incoming bundle is a claim, not state.
#[allow(dead_code)]
pub const VOLUME_CHANNEL_DIR: &str = "td/incoming";
// The two selector slots, by the names they have on disk. Here rather than as
// literals in td-boot alone because the installer's recipe check reads one back
// out of an unmounted image: a check that spelled its own copy would keep
// passing through a rename, which is the one failure it exists to catch.
#[allow(dead_code)]
pub const CURRENT_SLOT: &str = "current";
#[allow(dead_code)]
pub const PREVIOUS_SLOT: &str = "previous";
// The DISK layout, stated here for D1's reason: `td-install` writes it and
// td-boot reads what sits inside it, and a layout stated twice is a layout that
// can disagree with itself — at the first boot after an install rather than at
// build time.
//
// The ESP is sized for several kernel + initramfs pairs rather than for one,
// because item 8's EFI stub puts them there and a full ESP is discovered by an
// update that cannot finish. Its FAT label is at most 11 characters, which the
// FAT32 boot sector pads rather than checks.
// Scoped `dead_code` because none of the following has a reader in td-boot:
// `td-install` reaches them through the same `#[path]` include — and
// `MKFS_BTRFS` is read by the RECIPE, through a third include of this same file
// — which is the point of their being here rather than in either. Without the
// scope td-boot's clippy, which the preflight really does run, carries a
// warning per constant forever, and a clean lint run is what makes the next
// real one visible.
#[allow(dead_code)]
pub const ESP_PARTITION_NAME: &str = "td-esp";
#[allow(dead_code)]
pub const ESP_VOLUME_LABEL: &str = "TD-ESP";
#[allow(dead_code)]
pub const ESP_BYTES: u64 = 512 * 1024 * 1024;
#[allow(dead_code)]
pub const VOLUME_PARTITION_NAME: &str = "td-volume";
// Three deployment-sized copies are transiently live while an update is
// published before the oldest retained deployment is retired.  The profiled
// image permits one GiB of debug companions in each copy; reserve another GiB
// for their non-debug payloads and one GiB for Btrfs metadata plus @var.  This
// is an admission floor rather than a promise that every deployment fits. A
// larger update can still fail with ENOSPC while staging; selectors change only
// after the complete deployment has been written, synced, and published.
#[allow(dead_code)]
pub const MIN_VOLUME_BYTES: u64 = 5 * 1024 * 1024 * 1024;
#[allow(dead_code)]
pub const VOLUME_LABEL: &str = "td-system";
// The read-write subvolume the boot path mounts on /var, so a deployment's
// mutable state survives an update that replaces everything else. Named here
// for the reason the layout is: the installer CREATES it and the boot path
// MOUNTS it by this name, and the two disagreeing is a machine that boots to a
// read-only /var.
#[allow(dead_code)]
pub const VOLUME_SUBVOL: &str = "@var";

// The verb `td-install` execs to reach the one bundle writer. Stated here for
// the reason the layout above is: the caller spells it and the callee parses
// it, and nothing else ties the two together — a renamed verb would be a usage
// error at install time from a program the operator did not run.
#[allow(dead_code)]
pub const PUBLISH_VERB: &str = "publish";
// Named rather than a bare literal like the verbs beside it, because the
// boot-success script spells this one too — the one verb an image invokes.
#[allow(dead_code)]
pub const UPDATE_VERB: &str = "update";

/// A deployment id is `sha256(manifest)` written as 64 lowercase hex
/// characters, and this is the ONLY statement of that shape.
///
/// Shared because both halves check it against different things: td-boot
/// validates an id it was given as an argument, and td-install validates one a
/// td-boot it execed printed back. The second is not cosmetic — the id is
/// JOINED onto a path, so an id free to contain `/` or `..` would be a
/// directory traversal out of the staging tree from a program's stdout.
#[allow(dead_code)]
pub fn valid_digest(bytes: &[u8]) -> bool {
    bytes.len() == 64
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}
// The one third-party program on the install path (D7). A NAME, not a path:
// `td-install` is told where it is, and this is what the build-time check binds
// against so an image or a recipe that stops providing it reds the build rather
// than failing an install on a machine someone is standing in front of. Same
// argument as `REQUIRED_TD_INIT_APPLETS` above, one program wide.
#[allow(dead_code)]
pub const MKFS_BTRFS: &str = "mkfs.btrfs";
// 1 MiB, the alignment every partition start is held to. A start that ignores
// it reads and writes across a physical block boundary forever, and nothing
// reports it — `gpt.rs` refuses 0 for the same reason.
#[allow(dead_code)]
pub const PARTITION_ALIGN_BYTES: u64 = 1024 * 1024;
// Raising this changes the v1 reader contract; bump or migrate the format with it.
pub const ATTEMPT_V1_MAX_REMAINING: u8 = 3;
pub const DEFAULT_BOOT_ATTEMPTS: u8 = ATTEMPT_V1_MAX_REMAINING;
// The three external programs td-boot runs. All three are td-init applets now,
// called by their /bin names as every other script on the image does. The
// recipe-side initramfs check consumes this roster, so an applet nothing on the
// image provides reds the build rather than stopping a boot at a "not found".
//
// `REQUIRED_BUSYBOX_APPLETS` used to sit beside this holding `losetup`, and
// deleting it is the point of the landing that added td-init's `LOOP_SET_FD`
// request: td-boot now reaches no third-party program at all.
pub const MOUNT_APPLET: &str = "mount";
pub const UMOUNT_APPLET: &str = "umount";
pub const LOSETUP_APPLET: &str = "losetup";
#[allow(dead_code)]
pub const REQUIRED_TD_INIT_APPLETS: &[&str] = &[MOUNT_APPLET, UMOUNT_APPLET, LOSETUP_APPLET];

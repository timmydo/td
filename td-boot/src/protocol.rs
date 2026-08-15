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
#[allow(dead_code)]
pub const MANIFEST_SIG_NAME: &str = "manifest.sig";
pub const BOOT_DIR: &str = "td/boot";
pub const ATTEMPTS_DIR: &str = "td/boot/attempts";
pub const DEPLOYMENTS_DIR: &str = "td/deployments";
pub const SELECTOR_PREFIX: &str = "../deployments/";
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

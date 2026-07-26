pub const CURRENT_REJECTED_MARKER: &str = "TD-BOOT-CURRENT-REJECTED";
pub const ATTEMPT_CONSUMED_MARKER: &str = "TD-BOOT-ATTEMPT-CONSUMED";
pub const ATTEMPTS_EXHAUSTED_MARKER: &str = "TD-BOOT-ATTEMPTS-EXHAUSTED";
pub const BOOKKEEPING_UNAVAILABLE_MARKER: &str = "TD-BOOT-BOOKKEEPING-UNAVAILABLE";
pub const BOOKKEEPING_UNAVAILABLE_CMDLINE_TOKEN: &str = "td.boot-bookkeeping-unavailable=1";
pub const SELECTED_CURRENT_MARKER: &str = "TD-BOOT-SELECTED-CURRENT";
pub const SELECTED_PREVIOUS_MARKER: &str = "TD-BOOT-SELECTED-PREVIOUS";
pub const BOOT_DIR: &str = "td/boot";
pub const ATTEMPTS_DIR: &str = "td/boot/attempts";
pub const DEPLOYMENTS_DIR: &str = "td/deployments";
pub const SELECTOR_PREFIX: &str = "../deployments/";
// Raising this changes the v1 reader contract; bump or migrate the format with it.
pub const ATTEMPT_V1_MAX_REMAINING: u8 = 3;
pub const DEFAULT_BOOT_ATTEMPTS: u8 = ATTEMPT_V1_MAX_REMAINING;
// The three external programs td-boot runs, each grouped under the multicall
// that must serve it. The recipe-side initramfs check consumes both rosters, so
// an applet nothing on the image provides reds the build rather than stopping a
// boot at a "not found".
pub const MOUNT_APPLET: &str = "mount";
pub const UMOUNT_APPLET: &str = "umount";
pub const LOSETUP_APPLET: &str = "losetup";
// `losetup` needs ioctl(2) requests outside td-init's confined syscall
// amendment, so it is the one job td-boot still reaches busybox for.
#[allow(dead_code)]
pub const REQUIRED_BUSYBOX_APPLETS: &[&str] = &[LOSETUP_APPLET];
// mount/umount are td-init's since the mount(2)/umount2(2) amendment; td-boot
// calls them by their /bin names, as every other script on the image does.
#[allow(dead_code)]
pub const REQUIRED_TD_INIT_APPLETS: &[&str] = &[MOUNT_APPLET, UMOUNT_APPLET];

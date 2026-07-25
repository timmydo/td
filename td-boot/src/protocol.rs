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
pub const MOUNT_APPLET: &str = "mount";
pub const UMOUNT_APPLET: &str = "umount";
pub const LOSETUP_APPLET: &str = "losetup";
// The recipe-side initramfs check consumes this shared closure contract.
#[allow(dead_code)]
pub const REQUIRED_BUSYBOX_APPLETS: &[&str] = &[MOUNT_APPLET, UMOUNT_APPLET, LOSETUP_APPLET];

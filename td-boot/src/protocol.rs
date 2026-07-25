pub const CURRENT_REJECTED_MARKER: &str = "TD-BOOT-CURRENT-REJECTED";
pub const BOOT_DIR: &str = "td/boot";
pub const DEPLOYMENTS_DIR: &str = "td/deployments";
pub const SELECTOR_PREFIX: &str = "../deployments/";
pub const MOUNT_APPLET: &str = "mount";
pub const UMOUNT_APPLET: &str = "umount";
pub const LOSETUP_APPLET: &str = "losetup";
// The recipe-side initramfs check consumes this shared closure contract.
#[allow(dead_code)]
pub const REQUIRED_BUSYBOX_APPLETS: &[&str] = &[MOUNT_APPLET, UMOUNT_APPLET, LOSETUP_APPLET];

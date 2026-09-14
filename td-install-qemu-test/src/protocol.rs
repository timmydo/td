//! Diagnostic fixture constants; no production installer interface.
pub const TARGET_SERIAL: &str = "td-install-test";
pub const INSTALL_MARKER: &str = "TD-INSTALL-DISK-WRITTEN";
pub const FIRST_BOOT_MARKER: &str = "TD-INSTALL-PERSISTED-1";
pub const SECOND_BOOT_MARKER: &str = "TD-INSTALL-PERSISTED-2";
pub const REFUSED_PREFIX: &str = "TD-INSTALL-REFUSED:";

pub const DIRECT_MARKER: &str = "TD-INSTALL-PUBLISHED-ON-DISK";
pub const PARTITIONS_MARKER: &str = "TD-INSTALL-PARTITIONS-REFRESHED";

pub const MEDIA_MARKER: &str = "TD-INSTALL-MEDIA-READONLY";
/// ISO names and fixture paths; Linux's normal ISO name map lowercases.
pub const MEDIA_FILES: &[(&str, &str)] = &[
    ("BZIMAGE", "source/bzImage"),
    ("INITRAMFS.CPIO", "source/initramfs.cpio"),
    ("ROOT.EROFS", "source/root.erofs"),
    ("MANIFEST", "source/manifest"),
    ("MANIFEST.SIG", "source/manifest.sig"),
    ("SELECTOR.CPIO", "selector.cpio"),
];

pub const INTERRUPTED_MARKER: &str = "TD-INSTALL-PUBLICATION-INTERRUPTED";

pub const SECTOR_BYTES_MARKER: &str = "TD-INSTALL-TARGET-SECTOR-BYTES";

//! Diagnostic fixture constants; no production installer interface.
pub const TARGET_SERIAL: &str = "td-install-test";
pub const INSTALL_MARKER: &str = "TD-INSTALL-DISK-WRITTEN";
pub const FIRST_BOOT_MARKER: &str = "TD-INSTALL-PERSISTED-1";
pub const SECOND_BOOT_MARKER: &str = "TD-INSTALL-PERSISTED-2";
pub const REFUSED_PREFIX: &str = "TD-INSTALL-REFUSED:";
pub const REFUSAL_COMPLETE_MARKER: &str = "TD-INSTALL-REFUSAL-COMPLETE";
pub const SCRATCH_LIMIT_MARKER: &str = "TD-INSTALL-SCRATCH-LIMITED 65536";
pub const SCRATCH_EXHAUSTED_MARKER: &str = "TD-INSTALL-SCRATCH-ENOSPC";
pub const MEDIA_BUSY_MARKER: &str = "TD-INSTALL-MEDIA-FORMATTERS-BUSY";
pub const MEDIA_RELEASED_MARKER: &str = "TD-INSTALL-MEDIA-CLAIM-RELEASED";

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

pub const INVENTORY_BEFORE_MARKER: &str = "TD-INSTALL-INVENTORY-BEFORE";
pub const INVENTORY_AFTER_MARKER: &str = "TD-INSTALL-INVENTORY-AFTER";
pub const CANDIDATES_BEFORE_MARKER: &str = "TD-INSTALL-CANDIDATES-BEFORE";
pub const CANDIDATES_HELD_MARKER: &str = "TD-INSTALL-CANDIDATES-HELD";
pub const CANDIDATES_MOUNTED_MARKER: &str = "TD-INSTALL-CANDIDATES-MOUNTED";
pub const PLAN_OBSERVATION_MARKER: &str = "TD-INSTALL-PLAN-OBSERVATION";
pub const PLAN_STALE_MARKER: &str = "TD-INSTALL-PLAN-STALE-REFUSED";
pub const PLAN_BUSY_MARKER: &str = "TD-INSTALL-PLAN-BUSY-REFUSED";
pub const PLAN_PROBE_MINIMUM_SECTORS: u64 = 12_582_912;
pub const SOURCE_PLAN_OBSERVATION_MARKER: &str = "TD-INSTALL-SOURCE-PLAN-OBSERVATION";
pub const SOURCE_PLAN_STALE_MARKER: &str = "TD-INSTALL-SOURCE-PLAN-STALE-REFUSED";
pub const SOURCE_PLAN_BUSY_MARKER: &str = "TD-INSTALL-SOURCE-PLAN-BUSY-REFUSED";
pub const SOURCE_PLAN_CLAIM_MARKER: &str = "TD-INSTALL-SOURCE-PLAN-CLAIM-OVERLAP";
pub const MAX_INVENTORY_BYTES: usize = 32 * 1024;

pub const PREVIEW_MARKER: &str = "TD-INSTALL-LAYOUT-PREVIEW";
pub const MAX_PREVIEW_BYTES: usize = 1024;

/// Human name verified on every installed full-system boot.
pub const USERNAME: &str = "alice";
/// Machine name verified in the formatted volume and both cold boots.
pub const HOSTNAME: &str = "td-qemu-installed";
/// Geographic choice verified in the formatted volume and both cold boots.
pub const TIMEZONE_ID: &str = "Europe/London";

/// Only the full-system diagnostic ISO carries these existing SSH test inputs.
pub const SYSTEM_AUTOTEST_PRIVATE: &str = "system-autotest/private";
pub const SYSTEM_AUTOTEST_AUTHORIZED: &str = "system-autotest/authorized_keys";

/// Shape of the generator's canonical UUID, not proof of freshness or authority.
pub fn is_v4_volume_uuid(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => byte == b'-',
            14 => byte == b'4',
            19 => matches!(byte, b'8' | b'9' | b'a' | b'b'),
            _ => byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte),
        })
}

#[cfg(test)]
mod identity_tests {
    use super::is_v4_volume_uuid;

    #[test]
    fn volume_uuid_shape_is_canonical_and_version_four() {
        let good = "12345678-1234-4234-8234-123456789abc";
        assert!(is_v4_volume_uuid(good));
        for bad in [
            "",
            "00000000-0000-0000-0000-000000000000",
            "12345678-1234-4234-8234-123456789ABC",
            "12345678-1234-1234-8234-123456789abc",
            "12345678-1234-4234-7234-123456789abc",
            "12345678-1234-4234-8234-123456789abc\n",
        ] {
            assert!(!is_v4_volume_uuid(bad), "{bad:?}");
        }
        for index in [8, 13, 18, 23] {
            let mut bad = good.to_owned();
            bad.replace_range(index..index + 1, "0");
            assert!(!is_v4_volume_uuid(&bad));
        }
    }
}

//! Which filesystem a state directory's bytes actually land on, decided from
//! /proc/mounts.
//!
//! td's per-machine identity is only an identity if it survives a reboot, so
//! provisioning refuses to write onto a volatile or read-only filesystem. Writing
//! a machine-id to a tmpfs SUCCEEDS: it reports provisioning done and then mints a
//! different machine every boot, with nothing to see in any log. That silent
//! failure is what this module exists to turn into a loud one, and because it is a
//! pure decision over text it is unit-tested here rather than boot-tested.

/// The mount that owns a path, reduced to what the persistence decision needs.
pub(crate) struct Filesystem {
    /// The mount point, unescaped.
    pub(crate) point: String,
    pub(crate) fstype: String,
    /// Mounted `rw`. A read-only mount is refused separately from a volatile one
    /// because the two mean different things to whoever reads the diagnostic: a
    /// missing `/var` mount versus a `/var` that is there but not writable.
    pub(crate) writable: bool,
}

impl Filesystem {
    /// A deny-list, not an allow-list. An allow-list would refuse every future
    /// filesystem (ext4, xfs, f2fs, …) and refusing wrongly costs a provisioned
    /// machine; the failure actually worth catching is the small closed set of
    /// filesystems that are volatile BY DESIGN, which is exactly what td's image
    /// mounts over /run and /tmp.
    pub(crate) fn volatile(&self) -> bool {
        // `rootfs` is the initramfs root: if a pivot did not happen, provisioning
        // would otherwise succeed onto RAM and mint a fresh identity every boot
        // with no diagnostic, which is the exact silent failure above.
        matches!(
            self.fstype.as_str(),
            "tmpfs" | "ramfs" | "devtmpfs" | "rootfs"
        )
    }
}

/// The mount whose point is the longest path prefix of `path` — i.e. the one whose
/// filesystem a write to `path` actually reaches. On a tie the LAST row wins: a
/// later mount over the same point shadows the earlier one.
///
/// `path` must be absolute and already normalized; callers pass a compiled-in
/// default or an operator's argument, neither of which is a resolved symlink
/// chain. That is deliberate — the decision below is about the mount table, and a
/// caller that wants symlinks resolved should resolve them first.
pub(crate) fn covering(mounts: &str, path: &str) -> Option<Filesystem> {
    let mut best: Option<Filesystem> = None;
    for line in mounts.lines() {
        let mut fields = line.split(' ');
        // <source> <point> <fstype> <options> <dump> <pass>
        let (_source, point, fstype, options) = match (
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
        ) {
            (Some(source), Some(point), Some(fstype), Some(options)) => {
                (source, point, fstype, options)
            }
            _ => continue,
        };
        let point = unescape(point);
        if !covers(&point, path) {
            continue;
        }
        let shadowed = match &best {
            Some(current) => point.len() >= current.point.len(),
            None => true,
        };
        if shadowed {
            best = Some(Filesystem {
                point,
                fstype: unescape(fstype),
                writable: has_option(options, "rw"),
            });
        }
    }
    best
}

/// Is `path` inside the subtree `point` names? A textual prefix is not enough:
/// `/var` must not claim `/variable`.
fn covers(point: &str, path: &str) -> bool {
    if point == "/" {
        return path.starts_with('/');
    }
    let point = point.trim_end_matches('/');
    if !path.starts_with(point) {
        return false;
    }
    matches!(path.as_bytes().get(point.len()), None | Some(b'/'))
}

/// Whole-field match against the comma-separated mount options, so `rw` is not
/// read out of `errors=remount-ro` or a subvolume name.
fn has_option(options: &str, want: &str) -> bool {
    for option in options.split(',') {
        if option == want {
            return true;
        }
    }
    false
}

/// /proc/mounts escapes the four characters that would otherwise break its
/// space-separated fields. Decoding is required for the prefix test above to
/// recognise a mount point that contains one.
fn unescape(field: &str) -> String {
    let bytes = field.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while let Some(&byte) = bytes.get(i) {
        if byte == b'\\' {
            // `get` on a str yields None for a non-boundary range, so a partial
            // escape at the end of the field falls through to the literal push.
            if let Some(octal) = field.get(i + 1..i + 4) {
                if let Ok(value) = u8::from_str_radix(octal, 8) {
                    out.push(value);
                    i += 4;
                    continue;
                }
            }
        }
        out.push(byte);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    // The shape td's own image produces: a read-only erofs root, the persistent
    // btrfs @var subvolume, and the volatile tmpfs mounts.
    const IMAGE: &str = "\
/dev/loop0 / erofs ro,relatime 0 0\n\
devtmpfs /dev devtmpfs rw,nosuid,relatime,mode=755 0 0\n\
proc /proc proc rw,relatime 0 0\n\
tmpfs /run tmpfs rw,relatime,mode=755 0 0\n\
tmpfs /tmp tmpfs rw,relatime,mode=1777 0 0\n\
/dev/vda /var btrfs rw,nodev,nosuid,relatime,subvol=/@var 0 0\n\
/dev/vda /run/td-volume btrfs ro,relatime,subvolid=5,subvol=/ 0 0\n";

    #[test]
    fn the_state_dir_resolves_to_the_persistent_var_mount() {
        let fs = covering(IMAGE, "/var/lib/td").unwrap();
        assert_eq!(fs.point, "/var");
        assert_eq!(fs.fstype, "btrfs");
        assert!(fs.writable);
        assert!(!fs.volatile());
    }

    #[test]
    fn a_run_backed_state_dir_is_volatile() {
        let fs = covering(IMAGE, "/run/td/state").unwrap();
        assert_eq!(fs.point, "/run");
        assert!(fs.writable);
        assert!(fs.volatile(), "a tmpfs state dir must be refused");
    }

    /// The failure this whole module is for: no @var mount at all, so the state
    /// dir falls through to the read-only erofs root.
    #[test]
    fn without_a_var_mount_the_state_dir_lands_on_the_read_only_root() {
        let no_var = "\
/dev/loop0 / erofs ro,relatime 0 0\n\
tmpfs /run tmpfs rw,relatime,mode=755 0 0\n";
        let fs = covering(no_var, "/var/lib/td").unwrap();
        assert_eq!(fs.point, "/");
        assert_eq!(fs.fstype, "erofs");
        assert!(!fs.writable);
        assert!(!fs.volatile(), "erofs is persistent, just not writable");
    }

    #[test]
    fn the_longest_prefix_wins_and_a_prefix_must_end_at_a_component() {
        let fs = covering(IMAGE, "/run/td-volume/deployments").unwrap();
        assert_eq!(fs.point, "/run/td-volume");
        assert_eq!(fs.fstype, "btrfs");
        assert!(!fs.writable, "the volume is bound read-only");

        // /var must not claim a sibling whose name merely starts with it.
        let fs = covering(IMAGE, "/variable/lib/td").unwrap();
        assert_eq!(fs.point, "/");
    }

    #[test]
    fn a_later_mount_over_the_same_point_shadows_the_earlier_one() {
        let stacked = "\
/dev/vda /var btrfs rw,relatime 0 0\n\
tmpfs /var tmpfs rw,relatime 0 0\n";
        let fs = covering(stacked, "/var/lib/td").unwrap();
        assert_eq!(fs.fstype, "tmpfs", "the visible /var is the last one mounted");
    }

    #[test]
    fn the_mount_point_itself_is_covered_by_its_own_mount() {
        let fs = covering(IMAGE, "/var").unwrap();
        assert_eq!(fs.point, "/var");
    }

    #[test]
    fn rw_is_matched_as_a_whole_option_not_a_substring() {
        let tricky = "/dev/vda /var btrfs ro,errors=remount-rw,relatime 0 0\n";
        let fs = covering(tricky, "/var/lib/td").unwrap();
        assert!(
            !fs.writable,
            "`remount-rw` is not the `rw` flag - a read-only /var must be refused"
        );
    }

    #[test]
    fn escaped_mount_points_decode_before_the_prefix_test() {
        let escaped = "/dev/vda /var/my\\040state btrfs rw,relatime 0 0\n";
        let fs = covering(escaped, "/var/my state/td").unwrap();
        assert_eq!(fs.point, "/var/my state");
        assert_eq!(fs.fstype, "btrfs");
    }

    #[test]
    fn a_truncated_escape_stays_literal_rather_than_being_dropped() {
        assert_eq!(unescape("/var/x\\04"), "/var/x\\04");
        assert_eq!(unescape("/var/x\\zzz"), "/var/x\\zzz");
        assert_eq!(unescape("/a\\134b"), "/a\\b");
    }

    /// The un-pivoted case: still on the initramfs root, which is writable and
    /// looks persistent unless `rootfs` is named as volatile.
    #[test]
    fn an_initramfs_root_is_volatile() {
        let initramfs = "rootfs / rootfs rw 0 0\n";
        let fs = covering(initramfs, "/var/lib/td").unwrap();
        assert_eq!(fs.point, "/");
        assert!(fs.writable, "the initramfs root IS writable, which is the trap");
        assert!(fs.volatile(), "provisioning onto RAM must be refused");
    }

    #[test]
    fn a_short_or_empty_mount_table_yields_no_decision() {
        assert!(covering("", "/var/lib/td").is_none());
        assert!(covering("garbage\n/dev/vda /var\n", "/var/lib/td").is_none());
    }
}

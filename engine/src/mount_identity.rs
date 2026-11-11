//! Mount identities read from `/proc/self/mountinfo` text: which mount
//! covers a path and what device and root it shows there, and what a view
//! of a directory below the human's home must show (td-authd/DESIGN.md,
//! "Application filesystem grants"). The file is std-only and performs no
//! I/O, so target programs include it by path: td-jail admits mounts by it
//! and td-authd publishes the views td-jail admits, one derivation for both.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::io;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

/// A mount's device and root as mountinfo shows them. Two identities are
/// equal, and ordered, by their bytes: `Path`'s component comparison would
/// take a removed root's `PATH//deleted` for `PATH/deleted`.
#[derive(Clone, Debug)]
pub struct MountIdentity {
    pub device: String,
    pub root: PathBuf,
}

impl MountIdentity {
    fn key(&self) -> (&str, &[u8]) {
        (&self.device, self.root.as_os_str().as_bytes())
    }
}

impl PartialEq for MountIdentity {
    fn eq(&self, other: &Self) -> bool {
        self.key() == other.key()
    }
}

impl Eq for MountIdentity {}

impl PartialOrd for MountIdentity {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for MountIdentity {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.key().cmp(&other.key())
    }
}

/// One row of mountinfo: its id and parent's, device, root and mountpoint
/// (decoded) and per-mount options.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MountRow {
    pub id: u64,
    pub parent: u64,
    pub device: String,
    pub root: PathBuf,
    pub mountpoint: PathBuf,
    pub options: String,
}

/// Every row of `mountinfo`, in order. A row without the separator or
/// one of its first six fields, an invalid escape or a repeated mount id
/// refuses the whole table.
pub fn mount_rows(mountinfo: &str) -> io::Result<Vec<MountRow>> {
    let mut rows = Vec::new();
    let mut ids = BTreeSet::new();
    for line in mountinfo.lines() {
        let left = line
            .split_once(" - ")
            .ok_or_else(|| io::Error::other("mountinfo row has no separator"))?
            .0;
        let mut fields = left.split_whitespace();
        let mut next = |what: &str| {
            fields
                .next()
                .ok_or_else(|| io::Error::other(format!("mountinfo row has no {what}")))
        };
        let id = next("mount ID")?
            .parse::<u64>()
            .map_err(|error| io::Error::other(format!("invalid mount ID: {error}")))?;
        let parent = next("parent ID")?
            .parse::<u64>()
            .map_err(|error| io::Error::other(format!("invalid mount parent ID: {error}")))?;
        let device = next("device")?.to_string();
        let root = decode_mountinfo_path(next("root")?)?;
        let mountpoint = decode_mountinfo_path(next("mount point")?)?;
        let options = next("mount options")?.to_string();
        if !ids.insert(id) {
            return Err(io::Error::other(format!("mountinfo repeats mount ID {id}")));
        }
        rows.push(MountRow {
            id,
            parent,
            device,
            root,
            mountpoint,
            options,
        });
    }
    Ok(rows)
}

/// A mountinfo path field's bytes: the kernel writes space, tab, newline
/// and backslash as three octal digits after a backslash, and nothing else.
pub fn decode_mountinfo_path(field: &str) -> io::Result<PathBuf> {
    let mut decoded = Vec::with_capacity(field.len());
    let mut bytes = field.as_bytes().iter().copied();
    while let Some(byte) = bytes.next() {
        if byte != b'\\' {
            decoded.push(byte);
            continue;
        }
        let escape = [bytes.next(), bytes.next(), bytes.next()];
        let value = match escape {
            [Some(b'0'), Some(b'4'), Some(b'0')] => b' ',
            [Some(b'0'), Some(b'1'), Some(b'1')] => b'\t',
            [Some(b'0'), Some(b'1'), Some(b'2')] => b'\n',
            [Some(b'1'), Some(b'3'), Some(b'4')] => b'\\',
            _ => {
                return Err(io::Error::other(
                    "mountinfo contains an invalid path escape",
                ));
            }
        };
        decoded.push(value);
    }
    Ok(PathBuf::from(OsString::from_vec(decoded)))
}

fn root_mount_row(rows: &[MountRow]) -> Option<MountRow> {
    for row in rows.iter().rev() {
        if row.mountpoint == Path::new("/") {
            return Some(row.clone());
        }
    }
    None
}

fn child_mount_row(rows: &[MountRow], parent: u64) -> Option<MountRow> {
    for row in rows.iter().rev() {
        if row.parent == parent {
            return Some(row.clone());
        }
    }
    None
}

/// The device and root `path` shows: those of the topmost mount at the
/// deepest mountpoint at or above it, followed through the parent tree,
/// joined with the rest of the path below that mountpoint.
pub fn mount_identity_for_path(mountinfo: &str, path: &Path) -> io::Result<MountIdentity> {
    let mut rows = BTreeMap::<usize, Vec<MountRow>>::new();
    for row in mount_rows(mountinfo)? {
        if path.strip_prefix(&row.mountpoint).is_err() {
            continue;
        }
        let depth = row.mountpoint.components().count();
        rows.entry(depth).or_default().push(row);
    }
    let mut selected: Option<MountRow> = None;
    let mut selected_ids = BTreeSet::new();
    for rows_at_depth in rows.into_values() {
        if selected.is_none() {
            selected = root_mount_row(&rows_at_depth);
            if let Some(row) = &selected {
                selected_ids.insert(row.id);
            }
        }
        while let Some(parent) = selected.as_ref().map(|row| row.id) {
            let Some(next) = child_mount_row(&rows_at_depth, parent) else {
                break;
            };
            if !selected_ids.insert(next.id) {
                return Err(io::Error::other("mountinfo parent tree contains a cycle"));
            }
            selected = Some(next);
        }
    }
    let selected = selected
        .ok_or_else(|| io::Error::other(format!("mountinfo does not cover {}", path.display())))?;
    let relative = path.strip_prefix(&selected.mountpoint).map_err(|error| {
        io::Error::other(format!(
            "selected mount {} does not cover {}: {error}",
            selected.mountpoint.display(),
            path.display()
        ))
    })?;
    let mut root = selected.root;
    if !relative.as_os_str().is_empty() {
        root.push(relative);
    }
    Ok(MountIdentity {
        device: selected.device,
        root,
    })
}

/// What a view of the human's `relative` directory must show: the device
/// and root of the mount covering `home` (the topmost mount at the home or,
/// failing one, at its nearest ancestor), joined with the home's path below
/// that mount's mountpoint and then with `relative`. Mounts below the
/// home, between it and the directory or at the directory, do not count,
/// so a view cloned from one stacked over the directory or an intermediate
/// directory, or from a sibling, an ancestor or another device, shows
/// something else, and no mount need mark the directory for its view to
/// be admitted. td-authd publishes, and td-jail admits, by this one
/// derivation; each validates `relative` first.
pub fn human_source_identity(
    mountinfo: &str,
    home: &Path,
    relative: &Path,
) -> io::Result<MountIdentity> {
    let mut identity = mount_identity_for_path(mountinfo, home)?;
    identity.root.push(relative);
    Ok(identity)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    const HOME: &str = "/var/home/alice";

    fn identity(device: &str, root: &str) -> MountIdentity {
        MountIdentity {
            device: device.into(),
            root: root.into(),
        }
    }

    fn derive(mountinfo: &str, relative: &str) -> MountIdentity {
        human_source_identity(mountinfo, Path::new(HOME), Path::new(relative)).expect("derivable")
    }

    /// The stock layout: `/var` is a subvolume mount and the home is a
    /// plain directory below it, so a view of the human's Downloads shows
    /// the subvolume's root joined with the home's offset and the path.
    #[test]
    fn a_home_below_its_mount_keeps_its_offset() {
        let stock = "22 1 0:19 /@ / rw,relatime shared:1 - btrfs /dev/vda2 rw\n\
                     25 22 0:19 /@var /var rw,relatime shared:2 - btrfs /dev/vda2 rw\n";
        assert_eq!(
            derive(stock, "Downloads"),
            identity("0:19", "/@var/home/alice/Downloads")
        );
        assert_eq!(
            derive(stock, "Music/new"),
            identity("0:19", "/@var/home/alice/Music/new")
        );
    }

    /// A home that is its own mount contributes no offset; a mount of
    /// the home's parent contributes the home's name.
    #[test]
    fn a_home_mount_or_its_parents_contributes_its_own_root() {
        let own = "22 1 0:19 /@ / rw - btrfs /dev/vda2 rw\n\
                   30 22 0:31 /@home/alice /var/home/alice rw - btrfs /dev/vdb rw\n";
        assert_eq!(
            derive(own, "Downloads"),
            identity("0:31", "/@home/alice/Downloads")
        );
        let parent = "22 1 0:19 /@ / rw - btrfs /dev/vda2 rw\n\
                      30 22 0:31 / /var/home rw - btrfs /dev/vdb rw\n";
        assert_eq!(
            derive(parent, "Downloads"),
            identity("0:31", "/alice/Downloads")
        );
    }

    /// A mount stacked at the home replaces what it shows; a mount below
    /// the home, at or above the directory, does not count.
    #[test]
    fn only_the_topmost_mount_at_or_above_the_home_counts() {
        let stacked = "22 1 0:19 /@ / rw - btrfs /dev/vda2 rw\n\
                       25 22 0:19 /@var /var rw - btrfs /dev/vda2 rw\n\
                       31 25 0:40 / /var/home/alice rw - tmpfs tmpfs rw\n\
                       32 31 0:41 / /var/home/alice rw - tmpfs tmpfs rw\n\
                       33 32 0:42 / /var/home/alice/Downloads rw - tmpfs tmpfs rw\n";
        assert_eq!(derive(stacked, "Downloads"), identity("0:41", "/Downloads"));
    }

    #[test]
    fn rows_parse_with_options_and_refuse_malformed_tables() {
        let rows = mount_rows(
            "35 25 0:19 /@var/home/alice/My\\040Files /var/lib/td/grant-views/65536/0 rw,nosuid,nodev,noexec,idmapped shared:1 - btrfs none rw\n",
        )
        .expect("parsed");
        assert_eq!(
            rows,
            [MountRow {
                id: 35,
                parent: 25,
                device: "0:19".into(),
                root: "/@var/home/alice/My Files".into(),
                mountpoint: "/var/lib/td/grant-views/65536/0".into(),
                options: "rw,nosuid,nodev,noexec,idmapped".into(),
            }]
        );
        for malformed in [
            "35 25 0:19 / /x rw\n",
            "x 25 0:19 / /x rw - btrfs none rw\n",
            "35 25 0:19 /a\\04 /x rw - btrfs none rw\n",
            "35 25 0:19 /a\\777 /x rw - btrfs none rw\n",
            "35 25 0:19 / /x - btrfs none rw\n",
            "35 25 0:19 / /x rw - a\n35 25 0:19 / /y rw - a\n",
        ] {
            assert!(mount_rows(malformed).is_err(), "{malformed:?}");
        }
    }
}

//! Where the window's frames may be kept. td-ui writes each frame into a
//! file in the directory it is given; a frame shows titles, entry text
//! and the masked PIN's length, so the directory must be memory-backed,
//! never a disk a frame could persist on. Swap is refused by td-secret's
//! host protection, so memory-backed pages stay in memory.

use std::path::{Path, PathBuf};

/// The filesystem types whose files live only in memory.
const MEMORY: [&str; 2] = ["tmpfs", "ramfs"];

/// Whether `path`, an absolute path without links, lies on a
/// memory-backed filesystem by `mountinfo` (`/proc/self/mountinfo`). The
/// table lists mounts in the order they were made, so a mount hides every
/// earlier one at or below its point; of those left, the deepest
/// containing `path` decides. A line that cannot be read refuses the
/// answer.
pub fn memory_backed(mountinfo: &str, path: &Path) -> bool {
    let mut visible: Vec<(PathBuf, bool)> = Vec::new();
    for line in mountinfo.lines() {
        let Some((point, kind)) = mount(line) else {
            return false;
        };
        let point = PathBuf::from(point);
        visible.retain(|(earlier, _)| !earlier.starts_with(&point));
        visible.push((point, MEMORY.contains(&kind)));
    }
    visible
        .iter()
        .filter(|(point, _)| path.starts_with(point))
        .max_by_key(|(point, _)| point.components().count())
        .is_some_and(|(_, memory)| *memory)
}

/// The first of `candidates`, resolved paths in order of preference, that
/// is memory-backed.
pub fn choose(mountinfo: &str, candidates: impl IntoIterator<Item = PathBuf>) -> Option<PathBuf> {
    candidates
        .into_iter()
        .find(|path| memory_backed(mountinfo, path))
}

/// A line's mount point, unescaped, and filesystem type.
fn mount(line: &str) -> Option<(String, &str)> {
    let (fields, rest) = line.split_once(" - ")?;
    let point = fields.split(' ').nth(4)?;
    let kind = rest.split(' ').next()?;
    Some((unescape(point)?, kind))
}

/// The kernel writes space, tab, newline and backslash in a mount point
/// as three octal digits after a backslash.
fn unescape(text: &str) -> Option<String> {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        let digits: String = chars.by_ref().take(3).collect();
        let value = u8::from_str_radix(&digits, 8)
            .ok()
            .filter(|_| digits.len() == 3)?;
        out.push(char::from(value));
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const INFO: &str = "\
22 1 8:1 / / rw,relatime shared:1 - ext4 /dev/sda1 rw
23 22 0:21 / /run rw,nosuid shared:2 - tmpfs tmpfs rw,mode=755
24 23 0:30 / /run/user/1000 rw,nosuid shared:3 - tmpfs tmpfs rw,mode=700
25 22 0:22 / /dev/shm rw shared:4 - tmpfs tmpfs rw
26 22 8:2 / /home/a\\040b rw shared:5 - ext4 /dev/sda2 rw
27 23 8:3 / /run/user/1000/disk rw shared:6 - ext4 /dev/sda3 rw
28 22 0:23 / /mnt rw shared:7 - tmpfs tmpfs rw
29 22 8:4 / /mnt rw shared:8 - ext4 /dev/sda4 rw
";

    #[test]
    fn the_deepest_and_latest_mount_decides() {
        for (path, memory) in [
            ("/run/user/1000", true),
            ("/run/user/1000/td", true),
            ("/run/user/1000/disk", false),
            ("/run/user/1000/diskless", true),
            ("/dev/shm", true),
            ("/tmp", false),
            ("/home/a b/x", false),
            // A disk mounted over a tmpfs at the same point covers it.
            ("/mnt/x", false),
        ] {
            assert_eq!(memory_backed(INFO, Path::new(path)), memory, "{path}");
        }
    }

    #[test]
    fn a_later_mount_hides_the_mounts_below_it() {
        let covered = format!("{INFO}30 22 8:5 / /run rw shared:9 - ext4 /dev/sda5 rw\n");
        assert!(!memory_backed(&covered, Path::new("/run/user/1000")));
        assert!(memory_backed(&covered, Path::new("/dev/shm")));
        // A tmpfs mounted after the disk is visible again.
        let again = format!("{covered}31 30 0:31 / /run/user/1000 rw - tmpfs tmpfs rw\n");
        assert!(memory_backed(&again, Path::new("/run/user/1000")));
    }

    #[test]
    fn the_first_memory_backed_candidate_is_chosen() {
        let path = |p: &str| PathBuf::from(p);
        assert_eq!(
            choose(INFO, [path("/run/user/1000"), path("/dev/shm")]),
            Some(path("/run/user/1000"))
        );
        assert_eq!(
            choose(INFO, [path("/home/a b"), path("/dev/shm")]),
            Some(path("/dev/shm"))
        );
        assert_eq!(choose(INFO, [path("/tmp")]), None);
    }

    #[test]
    fn an_unreadable_table_answers_no() {
        assert!(!memory_backed("", Path::new("/run")));
        assert!(!memory_backed("garbage\n", Path::new("/run")));
        let escaped = "1 1 0:1 / /r\\04 rw - tmpfs tmpfs rw\n";
        assert!(!memory_backed(escaped, Path::new("/r")));
    }
}

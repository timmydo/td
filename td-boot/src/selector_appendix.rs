//! The archive preparing a selector appends to the selector template: the
//! trust root, the volume identity it boots and, on a measured volume, the
//! deployment measurement policy (td-install/DESIGN.md "Selector identity").
//!
//! One description shared by its three users, each by `#[path]`:
//! td-install's `prepare-selector`, its storage worker's selector update
//! (DESIGN.md "Storage operation worker"), and the installed selector's
//! staged-update commit, which rebuilds the archive with its own key,
//! identity and policy and requires the staged `INITRD.NEW` to be the
//! template followed by exactly these bytes (ENCRYPTION.md "Selector
//! update"). Two copies could come to disagree, and the commit would then
//! delete every update the worker staged.
use crate::cpio;
use crate::protocol;

/// The appended archive for `key`, the trusted key file's bytes as read,
/// and `uuid`, the volume UUID's text. A key the booted selector could not
/// decode makes an unbootable selector, so its shape is refused here by
/// td-boot's rule: 64 hex digits once surrounding whitespace is gone.
pub fn appendix(key: &[u8], uuid: &str, measured: bool) -> Result<Vec<u8>, String> {
    let digits = key.trim_ascii();
    if digits.len() != 64 || !digits.iter().all(u8::is_ascii_hexdigit) {
        return Err("trusted deployment key is not 64 hexadecimal digits".into());
    }
    let uuid_line = format!("{uuid}\n");
    let mut members: Vec<(&str, &[u8])> = vec![
        (protocol::TRUSTED_KEY_PATH, key),
        (protocol::VOLUME_UUID_PATH, uuid_line.as_bytes()),
    ];
    if measured {
        members.push((
            protocol::MEASUREMENT_POLICY_PATH,
            protocol::MEASUREMENT_POLICY,
        ));
    }
    let mut entries: Vec<cpio::Entry> = Vec::new();
    for (path, bytes) in members {
        // Every proper directory prefix, shallowest first and once: the
        // kernel creates nothing under a parent the archive does not name.
        let mut end = 0usize;
        while let Some(slash) = path.get(end..).and_then(|tail| tail.find('/')) {
            end = end
                .checked_add(slash)
                .ok_or_else(|| String::from("selector parent offset overflow"))?;
            let parent = path
                .get(..end)
                .ok_or_else(|| String::from("invalid selector parent"))?;
            if !entries.iter().any(|entry| entry.name == parent) {
                entries.push(cpio::Entry {
                    name: parent,
                    mode: 0o755,
                    kind: cpio::Kind::Directory,
                });
            }
            end = end
                .checked_add(1)
                .ok_or_else(|| String::from("selector parent offset overflow"))?;
        }
        entries.push(cpio::Entry {
            name: path,
            mode: 0o644,
            kind: cpio::Kind::File(bytes),
        });
    }
    cpio::build(&entries)
}

/// The zeros between a template of `len` bytes and the appendix, which
/// the kernel's cpio reader requires to start 4-byte aligned.
pub fn padding(len: u64) -> Result<usize, String> {
    let length = usize::try_from(len).map_err(|_| "selector template is too large".to_string())?;
    Ok(cpio::alignment_padding(length))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;

    const KEY: &[u8] = b"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\n";
    const UUID: &str = "5a5a5a5a-5a5a-405a-805a-5a5a5a5a5a5a";

    /// The member names in archive order: each parent once, before what it
    /// holds, and the policy only on a measured volume.
    fn names(archive: &[u8]) -> Vec<String> {
        let mut out = Vec::new();
        let mut at = 0usize;
        while at + 110 <= archive.len() {
            let field = |n: usize| {
                let hex = std::str::from_utf8(&archive[at + 6 + 8 * n..at + 14 + 8 * n]).unwrap();
                usize::from_str_radix(hex, 16).unwrap()
            };
            let (size, name_size) = (field(6), field(11));
            let name = &archive[at + 110..at + 110 + name_size - 1];
            let name = String::from_utf8(name.to_vec()).unwrap();
            if name == "TRAILER!!!" {
                break;
            }
            out.push(name);
            at = (at + 110 + name_size).next_multiple_of(4);
            at = (at + size).next_multiple_of(4);
        }
        out
    }

    #[test]
    fn the_appendix_names_each_parent_once_and_the_policy_when_measured() {
        let plain = names(&appendix(KEY, UUID, false).unwrap());
        let measured = names(&appendix(KEY, UUID, true).unwrap());
        assert!(plain.contains(&protocol::TRUSTED_KEY_PATH.to_string()));
        assert!(plain.contains(&protocol::VOLUME_UUID_PATH.to_string()));
        assert!(!plain.contains(&protocol::MEASUREMENT_POLICY_PATH.to_string()));
        assert!(measured.contains(&protocol::MEASUREMENT_POLICY_PATH.to_string()));
        for list in [&plain, &measured] {
            for (index, name) in list.iter().enumerate() {
                assert_eq!(list.iter().filter(|other| *other == name).count(), 1);
                if let Some((parent, _)) = name.rsplit_once('/') {
                    let at = list.iter().position(|other| other == parent).unwrap();
                    assert!(at < index, "{parent} after {name}");
                }
            }
        }
        assert!(appendix(b"not a key", UUID, true).is_err());
        assert_eq!(padding(5).unwrap(), 3);
        assert_eq!(padding(8).unwrap(), 0);
    }
}

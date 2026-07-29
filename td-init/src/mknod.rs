//! `mknod` — the one device node the deployment initramfs has to create itself.
//!
//! `/dev/loop0` is where td-boot attaches the verified root. `/dev` IS devtmpfs by
//! then, so the node normally comes from the kernel; this is the fallback for when
//! the loop driver registered none, and it cannot come from the cpio because the
//! devtmpfs mount shadows whatever was there.
//!
//! Only BLOCK nodes. `c`, `u` and `p` are refused: nothing on td's boot path
//! creates one, and the type is the part of `mode` that decides which driver the
//! kernel routes the node to.

use std::ffi::{CStr, CString};

use crate::devt;
use crate::sys;

/// `S_IFBLK`. The type half of `mode`; the permission half is below.
const S_IFBLK: usize = 0o60000;

/// The node exists for exactly one consumer — `td-boot root-loop`, running as root
/// in the initramfs — and does not survive the switch_root, so nothing is served by
/// making it group readable. (Like every mknod, the kernel still masks this with the
/// umask; `verify` deliberately does not look at permission bits.)
const MODE: usize = 0o600;

pub fn run(args: &[String]) -> Result<u8, String> {
    let (path, major, minor) = parse(args)?;
    create(&path, major, minor)?;
    Ok(0)
}

fn usage() -> String {
    "usage: mknod PATH b MAJOR MINOR".to_string()
}

fn parse(args: &[String]) -> Result<(String, u32, u32), String> {
    let (Some(path), Some(kind), Some(major), Some(minor), None) = (
        args.first(),
        args.get(1),
        args.get(2),
        args.get(3),
        args.get(4),
    ) else {
        return Err(usage());
    };
    if kind != "b" {
        return Err(format!(
            "'{kind}' is not served: this applet creates BLOCK nodes only, and the type \
             decides which driver the kernel routes the node to\n{}",
            usage()
        ));
    }
    Ok((path.clone(), number(major, "major")?, number(minor, "minor")?))
}

fn number(text: &str, which: &str) -> Result<u32, String> {
    text.parse()
        .map_err(|_| format!("{which} '{text}' is not a number\n{}", usage()))
}

/// Create the node, then REFUSE unless the kernel agrees it is the device that
/// was asked for.
///
/// The readback is the point. `dev` is not a pair of numbers at this interface,
/// it is one integer with the two packed into disjoint bit ranges, and a node
/// built from a wrong packing is a perfectly good node pointing at a different
/// driver — `mknod` returns success either way, and nothing downstream reads the
/// numbers back. This is the same argument that made `losetup` verify its
/// read-only flag out of sysfs rather than trust the flag it passed.
fn create(path: &str, major: u32, minor: u32) -> Result<(), String> {
    create_with(path, major, minor, sys::mknod)
}

/// Split from `create` so the readback and the UNLINK are reachable from a test.
/// `mknod(2)` needs CAP_MKNOD, so nothing here can drive the real thing — and the
/// unlink is the part carrying the safety argument, so "it appears in the source"
/// is not enough for it.
fn create_with(
    path: &str,
    major: u32,
    minor: u32,
    mk: impl FnOnce(&CStr, usize, usize) -> std::io::Result<()>,
) -> Result<(), String> {
    let dev = devt::encode(major, minor).map_err(|e| format!("mknod: {e}"))?;
    let c_path = CString::new(path).map_err(|_| format!("mknod: {path}: embedded NUL"))?;
    mk(&c_path, S_IFBLK | MODE, dev).map_err(|e| format!("mknod: {path}: {e}"))?;
    match verify(path, major, minor) {
        Ok(()) => Ok(()),
        Err(why) => {
            // A node the kernel does not agree about is worse than none: the loop
            // attach would open it and read another driver's device. If it cannot
            // be removed, SAY so — the caller is about to be told the node is bad,
            // and "and it is still there" changes what the operator must do.
            match std::fs::remove_file(path) {
                Ok(()) => Err(why),
                Err(e) => Err(format!("{why}; and it could not be removed: {e}")),
            }
        }
    }
}


fn verify(path: &str, major: u32, minor: u32) -> Result<(), String> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let meta = std::fs::symlink_metadata(path)
        .map_err(|e| format!("mknod: {path}: created but cannot be stat'd: {e}"))?;
    if !meta.file_type().is_block_device() {
        return Err(format!("mknod: {path}: created but is not a block device"));
    }
    let rdev = meta.rdev();
    let got = (devt::major(rdev), devt::minor(rdev));
    if got != (u64::from(major), u64::from(minor)) {
        return Err(format!(
            "mknod: {path}: asked for {major}:{minor}, kernel reports {}:{} — the dev \
             encoding is wrong and the node points at another driver",
            got.0, got.1
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn the_boot_path_invocation_parses() {
        assert_eq!(
            parse(&args(&["/dev/loop0", "b", "7", "0"])).unwrap(),
            ("/dev/loop0".to_string(), 7, 0)
        );
    }

    /// Only block nodes, and every other shape is an error rather than a guess.
    #[test]
    fn anything_but_a_block_node_is_refused() {
        for kind in ["c", "u", "p", "B", ""] {
            let e = parse(&args(&["/dev/x", kind, "7", "0"])).unwrap_err();
            assert!(e.contains("BLOCK nodes only"), "{kind}: {e}");
        }
        assert!(parse(&args(&["/dev/x", "b", "7"])).is_err(), "too few operands");
        assert!(
            parse(&args(&["/dev/x", "b", "7", "0", "extra"])).is_err(),
            "a trailing operand must not be ignored"
        );
        assert!(parse(&args(&["/dev/x", "b", "x", "0"])).is_err(), "major must be a number");
        assert!(parse(&args(&["/dev/x", "b", "7", "y"])).is_err(), "minor must be a number");
    }

    /// The readback itself, against nodes that already exist.
    ///
    /// `create` needs CAP_MKNOD, so nothing here drives it end to end — but
    /// `verify` is the half with the judgement in it, and every branch is
    /// reachable against `/dev` as it stands. The block device is looked up
    /// rather than hardcoded to loop0, so the mismatch branch — the one with no
    /// other symptom — runs wherever /dev has any block device at all.
    #[test]
    fn the_readback_refuses_a_node_the_kernel_disagrees_about() {
        use std::os::unix::fs::{FileTypeExt, MetadataExt};
        // A CHAR device is not a block device, whatever its numbers say.
        if std::fs::metadata("/dev/null").is_ok() {
            let e = verify("/dev/null", 1, 3).unwrap_err();
            assert!(e.contains("not a block device"), "{e}");
        }
        // A plain loop, not an iterator search: this file is embedded verbatim into
        // the recipe, and the ladder guard rejects the host-tool names those
        // combinators happen to spell.
        let mut found: Option<(String, u64)> = None;
        if let Ok(entries) = std::fs::read_dir("/dev") {
            for entry in entries.flatten() {
                let Ok(meta) = entry.metadata() else { continue };
                if meta.file_type().is_block_device() {
                    found = Some((entry.path().to_string_lossy().into_owned(), meta.rdev()));
                    break;
                }
            }
        }
        let Some((path, rdev)) = found else {
            eprintln!("note: /dev has no block device here; the matching branches are unexercised");
            return;
        };
        let (ma, mi) = (devt::major(rdev) as u32, devt::minor(rdev) as u32);
        assert!(verify(&path, ma, mi).is_ok(), "{path}: the true numbers must pass");
        // ...and a node whose numbers are NOT what was asked for is refused. The
        // node exists, is a block device, and points somewhere else — there is no
        // other symptom of this at all.
        let e = verify(&path, ma + 1, mi).unwrap_err();
        assert!(e.contains("kernel reports"), "{e}");
        assert!(e.contains(&path), "the refusal must name the node: {e}");
        assert!(verify(&path, ma, mi + 1).is_err(), "a wrong MINOR is refused too");
    }

    fn scratch(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("td-mknod-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let _ = std::fs::create_dir_all(&d);
        d
    }

    /// A node the kernel disagrees about is UNLINKED, not left behind.
    ///
    /// A regular file stands in for a node built from a wrong `dev` packing: it
    /// exists, the create step reported success, and the readback is the only
    /// thing that can tell. That substitution is why `create_with` takes its
    /// maker — CAP_MKNOD is not available to a unit test, and asserting the unlink
    /// from source text alone would pass on `let _ = remove_file(..)`.
    #[test]
    fn a_node_the_kernel_disagrees_about_is_unlinked() {
        let dir = scratch("unlink");
        let path = dir.join("probe");
        let p = path.to_string_lossy().into_owned();
        let made = path.clone();
        let e = create_with(&p, 7, 0, move |_, _, _| std::fs::write(&made, b"")).unwrap_err();
        assert!(e.contains("not a block device"), "{e}");
        assert!(
            !path.exists(),
            "the node the readback rejected must be removed; leaving it is worse \
             than never creating it, because td-boot would open it"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ...and when the cleanup itself fails, the caller is told BOTH things.
    #[test]
    fn a_cleanup_that_fails_is_reported_alongside_the_refusal() {
        let dir = scratch("cleanup");
        let path = dir.join("as-a-directory");
        let _ = std::fs::create_dir_all(&path);
        let p = path.to_string_lossy().into_owned();
        // `remove_file` on a directory is EISDIR, so this drives the branch where
        // the refusal stands but the bad path is still there.
        let e = create_with(&p, 7, 0, |_, _, _| Ok(())).unwrap_err();
        assert!(e.contains("not a block device"), "{e}");
        assert!(
            e.contains("could not be removed"),
            "the caller must learn it is still there: {e}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The encode gate runs BEFORE the maker, so an unencodable pair never
    /// reaches `mknod(2)` at all.
    #[test]
    fn an_unencodable_pair_never_reaches_the_maker() {
        let mut called = false;
        let e = create_with("/dev/never", 0x1000, 0, |_, _, _| {
            called = true;
            Ok(())
        })
        .unwrap_err();
        assert!(e.contains("does not fit"), "{e}");
        assert!(!called, "the refusal must happen before the syscall, not after");
    }

    /// `create` must reach the verifying path with the REAL syscall.
    ///
    /// The tests above drive `create_with` with a substitute maker, so they say
    /// nothing about the one line that wires the actual `mknod(2)` to it — and
    /// that line cannot be executed without CAP_MKNOD. Asserted from the source,
    /// the way this crate's confinement tests assert what the compiler cannot.
    #[test]
    fn create_delegates_to_the_verifying_path() {
        const SRC: &str = include_str!("mknod.rs");
        let body = SRC
            .split_once("fn create(")
            .and_then(|(_, rest)| rest.split_once("\n}"))
            .map(|(body, _)| body)
            .unwrap_or_default();
        assert!(
            body.contains("create_with(path, major, minor, sys::mknod)"),
            "create must go through create_with, which is what reads the node back; \
             calling sys::mknod directly would skip the refusal entirely"
        );
    }

    /// A path that is not there at all is an error, not a silent pass.
    /// A path that is not there at all is an error, not a silent pass.
    #[test]
    fn a_node_that_vanished_is_an_error() {
        assert!(verify("/dev/td-no-such-node", 7, 0).is_err());
    }

}

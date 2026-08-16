//! Opening and reading a file that must be a REAL, BOUNDED regular file.
//!
//! Shared for `protocol.rs`'s reason: td-boot verifies, `td-install` carries
//! the trust root onto the volume, and `td-net` signs — all three must refuse
//! the same files, or a bundle that signs is one that fails at boot.
//!
//! Each rule answers something that cannot be observed after the fact:
//!
//! - The type is settled by `lstat` BEFORE the open, because opening a FIFO
//!   read-only BLOCKS until a writer appears — a check that runs afterwards
//!   never runs at all, and the caller hangs with no diagnostic.
//! - The open carries `O_NOFOLLOW | O_NONBLOCK`, and the device/inode pair is
//!   compared ACROSS it, because the entry can be replaced in between. The
//!   flags make the two replacements that would otherwise WIN harmless — a
//!   symlink is refused at the open rather than followed, and a FIFO cannot
//!   block a call no later check can reach — and the pair catches the rest.
//! - The read takes one byte PAST the limit and refuses if it gets it, because
//!   a file that grew after the stat would otherwise be cut to exactly the
//!   limit and carried on as if it had always been that size.
//!
//! Std-only and importing nothing from any of them, as `fixture.rs` is, so
//! including it cannot drag anything into the crate that includes it.

use std::fs::{File, Metadata};
use std::io::{self, Read};
use std::path::Path;

/// Spelled by VALUE because this crate has no libc. Both are pinned by TEST
/// rather than by assertion — a symlink opened with `O_NOFOLLOW` must fail and
/// a FIFO opened with `O_NONBLOCK` must not block — so a wrong number is a red
/// test rather than a flag that silently does nothing.
/// These two numbers are per-TARGET, and getting one wrong is silent rather
/// than loud: `build_open_how` masks flag bits it does not know instead of
/// refusing them, so a wrong `O_NOFOLLOW` is an open that follows symlinks
/// again. arm64 is the trap — it overrides the generic header, where
/// `0o400000` is `O_LARGEFILE` and `O_NOFOLLOW` is `0o100000` — and the BSDs
/// differ in both. So the target is pinned WHOLE, in the shape `td-svc` and
/// `td-init` already use for their syscall numbers, rather than by a
/// whitelist of architectures that look close enough.
#[cfg(not(all(target_arch = "x86_64", target_os = "linux")))]
compile_error!("realfile.rs pins O_NONBLOCK/O_NOFOLLOW for x86_64-linux only");
const O_NONBLOCK: i32 = 0o4000;
const O_NOFOLLOW: i32 = 0o400000;

fn refused(message: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

/// Same inode on the same device — the PAIR, because an inode number alone
/// repeats across filesystems and a device alone says nothing about which file.
///
/// A named function so the comparison is testable. The race it guards needs
/// interposition to reproduce, but a comparison that dropped either half would
/// be silently wrong, and that a test can catch. `std::os::unix` rather than
/// `std::os::linux`: `dev`/`ino` are the same two fields as `st_dev`/`st_ino`.
fn same_file(a: &Metadata, b: &Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    a.dev() == b.dev() && a.ino() == b.ino()
}

/// Open `path`, requiring it to be a regular file that did not change into
/// something else while it was being opened. `label` names it in diagnostics.
///
/// No per-function `#[allow(dead_code)]`: every crate that uses only one of
/// the two puts the allow on the `mod`, so leaving it off here is what makes
/// one going unused in td-boot — which uses both — a warning rather than
/// silence.
pub fn open_real_file(path: &Path, label: &str) -> io::Result<(File, Metadata)> {
    let metadata = std::fs::symlink_metadata(path).map_err(|error| {
        io::Error::new(error.kind(), format!("{label} {}: {error}", path.display()))
    })?;
    if !metadata.file_type().is_file() {
        return Err(refused(format!(
            "{label} must be a real regular file: {}",
            path.display()
        )));
    }
    open_checked(path, &metadata, label)
}

/// The half of `open_real_file` that runs AFTER the type is settled, split out
/// so it can be tested against metadata deliberately made stale — which is the
/// race, arranged rather than won.
///
/// Between the `lstat` above and this open the entry can be replaced. The
/// device/inode pin catches a swap the open survives; the two flags catch the
/// two it does not. `O_NOFOLLOW` refuses a symlink AT the open, which the pin
/// cannot do when the symlink points back at the very file that was typed —
/// same device, same inode, and it would otherwise be followed and read.
/// `O_NONBLOCK` is the one that matters most: a FIFO blocks this open until a
/// writer appears, and no check on the far side of a blocking call ever runs.
/// td-boot reads a manifest on the BOOT path, so that is a machine that hangs
/// with nothing printed rather than one that refuses.
///
/// Neither flag changes anything for a regular file — reads ignore
/// `O_NONBLOCK` — which is why the pin still does the work for a
/// regular-for-regular swap.
///
/// What the `lstat` buys is the ORDINARY case and not the raced one: it keeps
/// this off device nodes, some of which act merely on being opened, but a node
/// swapped in after it is still opened here and neither flag prevents that.
/// DESIGN §10 item 10c records why the `O_PATH` route that would is refused.
fn open_checked(path: &Path, expected: &Metadata, label: &str) -> io::Result<(File, Metadata)> {
    use std::os::unix::fs::OpenOptionsExt;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(O_NOFOLLOW | O_NONBLOCK)
        .open(path)
        .map_err(|error| {
            io::Error::new(error.kind(), format!("{label} {}: {error}", path.display()))
        })?;
    let opened = file.metadata().map_err(|error| {
        io::Error::new(error.kind(), format!("{label} {}: {error}", path.display()))
    })?;
    if !opened.file_type().is_file() || !same_file(expected, &opened) {
        return Err(refused(format!(
            "{label} changed while opening: {}",
            path.display()
        )));
    }
    Ok((file, opened))
}

/// `open_real_file` plus a bound of `limit` bytes on what is read.
pub fn read_bounded_real_file(path: &Path, label: &str, limit: u64) -> io::Result<Vec<u8>> {
    let (file, metadata) = open_real_file(path, label)?;
    if metadata.len() > limit {
        return Err(refused(format!(
            "{label} exceeds {limit} bytes: {}",
            path.display()
        )));
    }

    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| {
            io::Error::new(error.kind(), format!("{label} {}: {error}", path.display()))
        })?;
    if bytes.len() as u64 > limit {
        return Err(refused(format!(
            "{label} changed while reading or exceeds {limit} bytes: {}",
            path.display()
        )));
    }
    Ok(bytes)
}

// Tested HERE, at the rule, rather than only through each caller: this file is
// `#[path]`-included by three crates, so these run in each of them and prove
// the rule holds in every inclusion context. The cases are the three the
// td-install copy got wrong before it was deleted, which is why they are the
// ones written down.
#[cfg(test)]
mod tests {
    use super::*;

    const LIMIT: u64 = 96;

    /// A FIFO, or a FAILED test. Skipping when `mkfifo` is absent would make
    /// the two assertions that need one pass on a host that cannot make one —
    /// and those are the safety net for a hardcoded number, so a silent skip
    /// is the one outcome they must not have.
    fn make_fifo(dir: &std::path::Path) -> std::path::PathBuf {
        let fifo = dir.join("fifo");
        let made = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .expect("mkfifo must exist: these assertions cannot run without a FIFO");
        assert!(made.success(), "mkfifo failed to create {fifo:?}");
        fifo
    }

    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "td-realfile-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_symlink_is_refused_even_when_its_target_is_a_regular_file() {
        let dir = scratch("symlink");
        let target = dir.join("real");
        std::fs::write(&target, b"key\n").unwrap();
        let link = dir.join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let refused = read_bounded_real_file(&link, "key", LIMIT)
            .unwrap_err()
            .to_string();
        assert!(refused.contains("must be a real regular file"), "{refused}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The type is settled BEFORE the open, which is the only order that
    /// works: `File::open` on a FIFO blocks until a writer appears, so a
    /// version that opened first does not fail this test — it HANGS it.
    #[test]
    fn a_fifo_is_refused_rather_than_blocking_forever() {
        let dir = scratch("fifo");
        let fifo = make_fifo(&dir);
        let refused = read_bounded_real_file(&fifo, "key", LIMIT)
            .unwrap_err()
            .to_string();
        assert!(refused.contains("must be a real regular file"), "{refused}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The bound is on what is READ and not only on what `stat` claimed. A
    /// file whose reported length disagrees with its contents is the case, and
    /// `/proc/self/status` is one without a race to arrange.
    #[test]
    fn a_file_longer_than_it_claims_is_refused_rather_than_truncated() {
        let path = std::path::Path::new("/proc/self/status");
        assert_eq!(std::fs::metadata(path).unwrap().len(), 0);
        let refused = read_bounded_real_file(path, "key", LIMIT)
            .unwrap_err()
            .to_string();
        assert!(refused.contains("changed while reading"), "{refused}");
    }

    #[test]
    fn a_file_at_the_limit_is_read_and_one_over_it_is_refused() {
        let dir = scratch("bound");
        let at = dir.join("at");
        std::fs::write(&at, vec![b'a'; LIMIT as usize]).unwrap();
        assert_eq!(
            read_bounded_real_file(&at, "key", LIMIT).unwrap().len(),
            LIMIT as usize
        );
        let over = dir.join("over");
        std::fs::write(&over, vec![b'a'; LIMIT as usize + 1]).unwrap();
        let refused = read_bounded_real_file(&over, "key", LIMIT)
            .unwrap_err()
            .to_string();
        assert!(refused.contains("key exceeds 96 bytes"), "{refused}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The identity pin is the one rule here whose failure is SILENT — a
    /// comparison missing either half accepts a different file and says
    /// nothing — so the comparison is a named function and this is its test.
    /// The race itself needs interposition and is not reproduced.
    #[test]
    fn file_identity_needs_the_device_and_the_inode() {
        let dir = scratch("ident");
        let (a, b) = (dir.join("a"), dir.join("b"));
        std::fs::write(&a, b"a").unwrap();
        std::fs::write(&b, b"b").unwrap();
        let (ma, mb) = (
            std::fs::symlink_metadata(&a).unwrap(),
            std::fs::symlink_metadata(&b).unwrap(),
        );
        assert!(same_file(&ma, &ma), "a file is itself");
        assert!(!same_file(&ma, &mb), "two files are not the same file");
        use std::os::unix::fs::MetadataExt;
        assert_ne!(
            ma.ino(),
            mb.ino(),
            "the fixture is only meaningful if the inodes differ"
        );
        assert_eq!(ma.dev(), mb.dev(), "...and the devices do not");
        let reopened = File::open(&a).unwrap().metadata().unwrap();
        assert!(
            same_file(&ma, &reopened),
            "the same path reopened is the same file"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// THE RACE, ARRANGED. `open_checked` is handed metadata deliberately made
    /// stale, which is exactly what the window between the `lstat` and the open
    /// produces — so the two flags are tested THROUGH the production path
    /// rather than through an `OpenOptions` the test built itself.
    ///
    /// The symlink points back at the FILE THAT WAS TYPED, which is the case
    /// the device/inode pin cannot see: same device, same inode, so following
    /// it succeeds every check. `O_NOFOLLOW` is the only thing that refuses,
    /// and removing it from `open_checked` reds this.
    #[test]
    fn a_symlink_swapped_in_after_the_type_check_is_not_followed() {
        let dir = scratch("swap-link");
        let target = dir.join("target");
        std::fs::write(&target, b"x").unwrap();
        let entry = dir.join("entry");
        // A hard link, so the entry that gets typed and the symlink's target
        // are ONE inode — the pin has nothing to notice.
        std::fs::hard_link(&target, &entry).unwrap();
        let stale = std::fs::symlink_metadata(&entry).unwrap();
        std::fs::remove_file(&entry).unwrap();
        std::os::unix::fs::symlink(&target, &entry).unwrap();
        use std::os::unix::fs::MetadataExt;
        assert_eq!(
            stale.ino(),
            std::fs::metadata(&entry).unwrap().ino(),
            "the fixture only tests O_NOFOLLOW if the pin would accept the target"
        );
        assert!(
            open_checked(&entry, &stale, "key").is_err(),
            "a symlink to the typed file was followed — O_NOFOLLOW is not set"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The same window with a FIFO. Without `O_NONBLOCK` this does not fail —
    /// it HANGS, which is the whole of what that flag is for.
    #[test]
    fn a_fifo_swapped_in_after_the_type_check_does_not_block() {
        let dir = scratch("swap-fifo");
        let entry = dir.join("entry");
        std::fs::write(&entry, b"x").unwrap();
        let stale = std::fs::symlink_metadata(&entry).unwrap();
        std::fs::remove_file(&entry).unwrap();
        let fifo = make_fifo(&dir);
        std::fs::rename(&fifo, &entry).unwrap();
        let refused = open_checked(&entry, &stale, "key").unwrap_err().to_string();
        assert!(refused.contains("changed while opening"), "{refused}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The two open flags, pinned by what they DO rather than by their
    /// numbers: this crate has no libc to take them from, so a wrong constant
    /// would be a flag that silently does nothing. The race they close needs
    /// interposition to reproduce; that each flag is the flag it claims to be
    /// does not.
    #[test]
    fn the_open_flags_are_the_numbers_they_claim_to_be() {
        use std::os::unix::fs::OpenOptionsExt;
        let dir = scratch("flags");
        let target = dir.join("real");
        std::fs::write(&target, b"x").unwrap();
        let link = dir.join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        // O_NOFOLLOW: the symlink itself is refused, where a plain open of the
        // same path succeeds by following it.
        assert!(
            std::fs::File::open(&link).is_ok(),
            "the fixture must be followable"
        );
        assert!(
            std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(O_NOFOLLOW)
                .open(&link)
                .is_err(),
            "O_NOFOLLOW did not refuse a symlink — the constant is wrong"
        );
        // ...and REFUSING is not enough: `O_DIRECTORY` sits one bit away and
        // refuses a symlink too, along with the regular file this must accept.
        // Without this half the assertion above passes for the wrong flag,
        // which a mutation to 0o200000 demonstrated.
        assert!(
            std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(O_NOFOLLOW)
                .open(&target)
                .is_ok(),
            "O_NOFOLLOW refused a regular file — the constant is some other flag"
        );
        // O_NONBLOCK: a FIFO with no writer opens AT ONCE. Without the flag
        // this call blocks forever, so a wrong constant hangs rather than
        // fails — which is the same signal the FIFO test above carries.
        let fifo = make_fifo(&dir);
        assert!(
            std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(O_NONBLOCK)
                .open(&fifo)
                .is_ok(),
            "O_NONBLOCK could not open a writerless FIFO"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_directory_is_refused_and_a_missing_file_names_itself() {
        let dir = scratch("other");
        let refused = read_bounded_real_file(&dir, "key", LIMIT)
            .unwrap_err()
            .to_string();
        assert!(refused.contains("must be a real regular file"), "{refused}");
        let missing = dir.join("nope");
        let refused = read_bounded_real_file(&missing, "key", LIMIT)
            .unwrap_err()
            .to_string();
        assert!(
            refused.contains("nope"),
            "a refusal must name the path: {refused}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

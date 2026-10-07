#![allow(clippy::unwrap_used, clippy::indexing_slicing)]
use super::*;
use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::sync::atomic::{AtomicU64, Ordering};

/// A private directory whose `authd` child is the backoff's directory,
/// owned as the private directory is; removed on drop.
pub(crate) struct Scratch {
    pub(crate) path: PathBuf,
    pub(crate) owner: (u32, u32),
}

impl Scratch {
    pub(crate) fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "td-backoff-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        let metadata = fs::metadata(&path).unwrap();
        Self {
            path,
            owner: (metadata.uid(), metadata.gid()),
        }
    }

    pub(crate) fn backoff(&self) -> Backoff {
        Backoff::at(&self.path.join("authd"), self.owner)
    }

    pub(crate) fn file(&self) -> PathBuf {
        self.path.join("authd").join(FILE)
    }

    /// The backoff file holding `text`, mode 0600, in its 0700 directory.
    pub(crate) fn write(&self, text: &[u8]) {
        let directory = self.path.join("authd");
        let _ = fs::create_dir(&directory);
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(self.file(), text).unwrap();
        fs::set_permissions(self.file(), fs::Permissions::from_mode(0o600)).unwrap();
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[test]
fn each_unapproved_request_doubles_the_refusal_from_30_to_960_seconds() {
    let mut entry = Entry::default();
    assert!(!entry.refuses(0));
    for (count, delay) in [
        (1, 30),
        (2, 60),
        (3, 120),
        (4, 240),
        (5, 480),
        (6, 960),
        (7, 960),
    ] {
        entry = entry.after(1000, 0);
        assert_eq!(
            entry,
            Entry {
                count,
                until: 1000 + delay
            },
            "{count}"
        );
        assert!(entry.refuses(1000 + delay - 1));
        assert!(!entry.refuses(1000 + delay));
    }
    // An admitted request's refusal runs past its 180-second life.
    assert_eq!(
        Entry::default().after(1000, LIFETIME),
        Entry {
            count: 1,
            until: 1210
        }
    );
    let worst = Entry {
        count: u32::MAX,
        until: u64::MAX,
    }
    .after(u64::MAX, LIFETIME);
    assert_eq!(
        worst,
        Entry {
            count: u32::MAX,
            until: u64::MAX
        }
    );
}

#[test]
fn the_file_grammar_is_exact() {
    assert_eq!(parse("td-authd-backoff-v1\n").unwrap(), Entry::default());
    assert_eq!(
        parse("td-authd-backoff-v1\nhostname\t3\t1700000000\n").unwrap(),
        Entry {
            count: 3,
            until: 1_700_000_000
        }
    );
    for text in [
        "",
        "td-authd-backoff-v1",
        "td-authd-backoff-v2\n",
        "td-authd-backoff-v1\n\n",
        "td-authd-backoff-v1\nhostname\t0\t5\n",
        "td-authd-backoff-v1\nhostname\t03\t5\n",
        "td-authd-backoff-v1\nhostname\t3\t05\n",
        "td-authd-backoff-v1\nhostname\t3\n",
        "td-authd-backoff-v1\nhostname\t3\t5\t6\n",
        "td-authd-backoff-v1\nhostname 3 5\n",
        "td-authd-backoff-v1\nupdate\t3\t5\n",
        "td-authd-backoff-v1\nhostname\t-3\t5\n",
        "td-authd-backoff-v1\nhostname\t4294967296\t5\n",
        "td-authd-backoff-v1\nhostname\t3\t18446744073709551616\n",
        "td-authd-backoff-v1\nhostname\t3\t5\nhostname\t3\t5\n",
    ] {
        assert!(parse(text).is_err(), "{text:?}");
    }
    for entry in [
        Entry::default(),
        Entry { count: 1, until: 0 },
        Entry {
            count: u32::MAX,
            until: u64::MAX,
        },
    ] {
        let encoded = encode(entry);
        assert_eq!(
            parse(std::str::from_utf8(&encoded).unwrap()).unwrap(),
            entry
        );
    }
    assert_eq!(encode(Entry::default()), b"td-authd-backoff-v1\n");
}

/// Missing reads as zero; each write is synced into a created 0700
/// directory, mode 0600, and a later reader, as a new generation or a
/// reboot is, reads it; only an approval clears it.
#[test]
fn the_count_persists_until_an_approval_clears_it() {
    let scratch = Scratch::new();
    let backoff = scratch.backoff();
    assert_eq!(backoff.read().unwrap(), Entry::default());
    let presented = backoff.admitted(backoff.read().unwrap(), 1000).unwrap();
    assert_eq!(
        presented,
        Entry {
            count: 1,
            until: 1210
        }
    );
    let directory = fs::metadata(scratch.path.join("authd")).unwrap();
    assert_eq!(directory.mode() & 0o7777, 0o700);
    let file = fs::metadata(scratch.file()).unwrap();
    assert_eq!(file.mode() & 0o7777, 0o600);
    assert_eq!(
        fs::read(scratch.file()).unwrap(),
        b"td-authd-backoff-v1\nhostname\t1\t1210\n"
    );
    assert!(!scratch.path.join("authd").join("backoff.new").exists());
    let later = scratch.backoff();
    assert_eq!(later.read().unwrap(), presented);
    assert_eq!(
        later.admitted(later.read().unwrap(), 2000).unwrap(),
        Entry {
            count: 2,
            until: 2240
        }
    );
    assert_eq!(backoff.read().unwrap().count(), 2);
    assert_eq!(backoff.approved().unwrap(), Entry::default());
    assert_eq!(fs::read(scratch.file()).unwrap(), b"td-authd-backoff-v1\n");
    assert_eq!(later.read().unwrap(), Entry::default());
}

/// A malformed file, or one of the wrong kind, mode or owner, refuses;
/// nothing replaces it.
#[test]
fn a_file_root_cannot_admit_refuses_and_is_left_alone() {
    let scratch = Scratch::new();
    let backoff = scratch.backoff();
    scratch.write(b"td-authd-backoff-v1\nhostname\tlots\t5\n");
    assert!(backoff.read().is_err());
    assert_eq!(
        fs::read(scratch.file()).unwrap(),
        b"td-authd-backoff-v1\nhostname\tlots\t5\n"
    );
    scratch.write(b"td-authd-backoff-v1\n");
    assert!(backoff.read().is_ok());
    fs::set_permissions(scratch.file(), fs::Permissions::from_mode(0o644)).unwrap();
    assert!(backoff.read().is_err());
    scratch.write(&[b'x'; 257]);
    assert!(backoff.read().is_err());
    fs::remove_file(scratch.file()).unwrap();
    fs::write(scratch.path.join("elsewhere"), b"td-authd-backoff-v1\n").unwrap();
    symlink(scratch.path.join("elsewhere"), scratch.file()).unwrap();
    assert!(backoff.read().is_err());
    fs::remove_file(scratch.file()).unwrap();
    fs::hard_link(scratch.path.join("elsewhere"), scratch.file()).unwrap();
    fs::set_permissions(scratch.file(), fs::Permissions::from_mode(0o600)).unwrap();
    assert!(backoff.read().is_err());
    fs::remove_file(scratch.file()).unwrap();
    fs::create_dir(scratch.file()).unwrap();
    assert!(backoff.read().is_err());
    fs::remove_dir(scratch.file()).unwrap();
    // The directory: the owner's, mode 0700, never a link.
    fs::set_permissions(
        scratch.path.join("authd"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    assert!(backoff.read().is_err());
    assert!(backoff.admitted(Entry::default(), 1000).is_err());
    fs::remove_dir(scratch.path.join("authd")).unwrap();
    fs::create_dir(scratch.path.join("real")).unwrap();
    fs::set_permissions(scratch.path.join("real"), fs::Permissions::from_mode(0o700)).unwrap();
    symlink(scratch.path.join("real"), scratch.path.join("authd")).unwrap();
    assert!(backoff.read().is_err());
    assert!(backoff.admitted(Entry::default(), 1000).is_err());
    // Another owner's directory and file.
    fs::remove_file(scratch.path.join("authd")).unwrap();
    assert_eq!(
        backoff.admitted(Entry::default(), 1000).unwrap().count(),
        1,
        "created afresh"
    );
    let foreign = Backoff::at(
        &scratch.path.join("authd"),
        (scratch.owner.0.wrapping_add(1), scratch.owner.1),
    );
    assert!(foreign.read().is_err());
    assert!(foreign.admitted(Entry::default(), 1000).is_err());
}

/// A missing directory is created only beneath a parent of the owner's
/// that no other may write.
#[test]
fn the_directory_is_created_only_beneath_a_protected_parent() {
    let scratch = Scratch::new();
    fs::set_permissions(&scratch.path, fs::Permissions::from_mode(0o777)).unwrap();
    assert!(scratch.backoff().admitted(Entry::default(), 1000).is_err());
    assert!(!scratch.path.join("authd").exists());
    fs::set_permissions(&scratch.path, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(scratch.backoff().admitted(Entry::default(), 1000).is_ok());
    let nested = Backoff::at(&scratch.path.join("absent").join("authd"), scratch.owner);
    assert!(nested.admitted(Entry::default(), 1000).is_err());
}

#[test]
fn a_fifo_never_blocks_the_read() {
    let scratch = Scratch::new();
    scratch.write(b"td-authd-backoff-v1\n");
    fs::remove_file(scratch.file()).unwrap();
    if crate::login_tier::tests::fifo(&scratch.file()) {
        assert!(scratch.backoff().read().is_err());
    }
}

/// A deadline past the longest one admission could cause, 180 seconds and
/// then 960, as a clock set back leaves, is cut to it and written back,
/// so the refusal ends; one within the bound is left alone and unwritten.
#[test]
fn a_deadline_past_its_bound_is_cut_and_written_back() {
    let scratch = Scratch::new();
    let backoff = scratch.backoff();
    scratch.write(b"td-authd-backoff-v1\nhostname\t4\t101000\n");
    let entry = backoff.read().unwrap();
    let clamped = backoff.clamp(entry, 1000).unwrap();
    assert_eq!(
        clamped,
        Entry {
            count: 4,
            until: 1000 + LIFETIME + LAST
        }
    );
    assert_eq!(
        fs::read(scratch.file()).unwrap(),
        b"td-authd-backoff-v1\nhostname\t4\t2140\n"
    );
    assert!(clamped.refuses(2139) && !clamped.refuses(2140));
    // Later, the written deadline holds: it no longer moves with the clock.
    assert_eq!(
        backoff.clamp(backoff.read().unwrap(), 1500).unwrap(),
        clamped
    );
    // Within the bound: unchanged, and nothing written.
    fs::remove_file(scratch.file()).unwrap();
    scratch.write(b"td-authd-backoff-v1\nhostname\t1\t2140\n");
    fs::create_dir(scratch.path.join("authd").join("backoff.new")).unwrap();
    assert_eq!(
        backoff.clamp(backoff.read().unwrap(), 1000).unwrap(),
        Entry {
            count: 1,
            until: 2140
        }
    );
    // Past it, with the write failing: the failure is the answer.
    scratch.write(b"td-authd-backoff-v1\nhostname\t1\t9999\n");
    assert!(backoff.clamp(backoff.read().unwrap(), 1000).is_err());
}

//! The volatile cutover record's bytes (td-authd/DESIGN.md amendment 7)
//! and the root-only publication by rename that both its writers use:
//! firstboot's two SSH renders, and td-authd once a cutover is observed
//! complete; and the removal under the same lock with which td-authd
//! begins every cutover. td-authd compiles this one file by its reviewed
//! path.
//!
//! `publish` writes a fixed-name temporary beside the target, created
//! exclusively without following a link, then syncs and renames it into
//! place under an exclusive lock on a persistent root-only file beside the
//! target, so no reader sees a partial file and no two publishers
//! interleave.

use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

const O_NOFOLLOW: i32 = 0x20000;
const O_NONBLOCK: i32 = 0x800;
const O_DIRECTORY: i32 = 0x10000;

/// The record naming the state the console and SSH were last brought to,
/// relative to the root.
pub(crate) const RECORD: &str = "run/td-login-cutover";
/// The running kernel's boot ID, whatever root is rendered under.
pub(crate) const BOOT_ID: &str = "/proc/sys/kernel/random/boot_id";
const RECORD_VERSION: &str = "td-login-cutover-v1";

/// The reduced login state: the record's and `render-ssh-policy`'s word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Reduced {
    Unenrolled,
    Enforced,
}

impl Reduced {
    pub(crate) fn word(self) -> &'static str {
        match self {
            Self::Unenrolled => "unenrolled",
            Self::Enforced => "enforced",
        }
    }
}

/// A validated boot ID: 36 bytes of lowercase hex with hyphens where a
/// UUID's text form has them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BootId(String);

impl BootId {
    fn parse(bytes: &[u8]) -> Result<Self, String> {
        let shaped = bytes.len() == 36
            && bytes.iter().enumerate().all(|(at, byte)| {
                if matches!(at, 8 | 13 | 18 | 23) {
                    *byte == b'-'
                } else {
                    byte.is_ascii_digit() || (b'a'..=b'f').contains(byte)
                }
            });
        match std::str::from_utf8(bytes) {
            Ok(text) if shaped => Ok(Self(text.to_owned())),
            _ => Err("the boot ID is not 36 bytes of lowercase UUID text".into()),
        }
    }

    /// The ID `path` holds as one newline-terminated line.
    pub(crate) fn read(path: &Path) -> Result<Self, String> {
        let mut bytes = Vec::with_capacity(38);
        std::fs::File::open(path)
            .and_then(|file| file.take(38).read_to_end(&mut bytes))
            .map_err(|error| format!("read {}: {error}", path.display()))?;
        let line = bytes
            .strip_suffix(b"\n")
            .ok_or_else(|| format!("{} is not one newline-terminated line", path.display()))?;
        Self::parse(line)
    }
}

impl std::fmt::Display for BootId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// The record's exact bytes: version, boot ID and the state's word, each
/// newline-terminated. A reader compares against these, so it admits
/// exactly this grammar.
pub(crate) fn record(boot: &BootId, state: Reduced) -> String {
    format!("{RECORD_VERSION}\n{boot}\n{}\n", state.word())
}

/// The fixed temporary beside `path`.
pub(crate) fn temporary(path: &Path) -> PathBuf {
    beside(path, ".tmp")
}

pub(crate) fn beside(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

/// Opens, creating it if absent, the persistent lock file beside `path`
/// and takes its exclusive lock, which the kernel drops if the process
/// dies. It must be a single-link regular file of `uid`:`gid`, mode 0600,
/// so only that owner can hold it: an account that could open the lock
/// could stall every publisher.
pub(crate) fn lock(path: &Path, uid: u32, gid: u32) -> std::io::Result<std::fs::File> {
    let lock = beside(path, ".lock");
    let named = |error: std::io::Error| {
        std::io::Error::new(error.kind(), format!("{}: {error}", lock.display()))
    };
    let open = |create: bool| {
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(create)
            .mode(0o600)
            .custom_flags(O_NOFOLLOW | O_NONBLOCK)
            .open(&lock)
    };
    // A new file takes the owner and mode here, so neither a setgid parent nor
    // the umask can leave one the check below refuses forever; an existing one
    // is checked, never repaired.
    let file = match open(true) {
        Ok(file) => {
            std::os::unix::fs::fchown(&file, Some(uid), Some(gid)).map_err(named)?;
            file.set_permissions(std::os::unix::fs::PermissionsExt::from_mode(0o600))
                .map_err(named)?;
            file
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            open(false).map_err(named)?
        }
        Err(error) => return Err(named(error)),
    };
    let meta = file.metadata().map_err(named)?;
    if !meta.file_type().is_file()
        || (meta.uid(), meta.gid()) != (uid, gid)
        || meta.mode() & 0o7777 != 0o600
        || meta.nlink() != 1
    {
        return Err(std::io::Error::other(format!(
            "{} is not a single-link {uid}:{gid} mode-0600 regular file",
            lock.display()
        )));
    }
    file.lock().map_err(named)?;
    Ok(file)
}

/// Publishes `bytes` at `path`, `uid`:`gid`'s and mode 0600, through the
/// fixed temporary: a stale one is unlinked, never followed, and the new
/// one is created with `O_CREAT|O_EXCL|O_NOFOLLOW`. Owner and mode are set
/// through its descriptor before the file and then its directory are
/// synced around the rename, which replaces whatever entry `path` names
/// without following it. The whole sequence holds the lock beside `path`,
/// so an overlapping publisher cannot unlink this one's temporary or
/// rename it half written.
pub(crate) fn publish(path: &Path, bytes: &[u8], uid: u32, gid: u32) -> Result<(), String> {
    let directory = path
        .parent()
        .ok_or_else(|| format!("{} has no directory", path.display()))?;
    let temporary = temporary(path);
    let write = || -> std::io::Result<()> {
        let parent = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(O_DIRECTORY | O_NOFOLLOW)
            .open(directory)?;
        let _held = lock(path, uid, gid)?;
        match std::fs::remove_file(&temporary) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let mut file = create(&temporary)?;
        std::os::unix::fs::fchown(&file, Some(uid), Some(gid))?;
        file.set_permissions(std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temporary, path)?;
        parent.sync_all()
    };
    write().map_err(|error| {
        format!(
            "publish {} through {}: {error}",
            path.display(),
            temporary.display()
        )
    })
}

/// Removes the record at `path` under the same lock `publish` holds, then
/// syncs its directory: a cutover's first step, so one interrupted, or
/// whose record cannot be written, leaves no record that could name a
/// state the console and SSH were not brought to. An absent record is
/// already removed.
#[allow(dead_code, reason = "td-authd's alone: firstboot only publishes")]
pub(crate) fn remove(path: &Path, uid: u32, gid: u32) -> Result<(), String> {
    let directory = path
        .parent()
        .ok_or_else(|| format!("{} has no directory", path.display()))?;
    let unlink = || -> std::io::Result<()> {
        let parent = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(O_DIRECTORY | O_NOFOLLOW)
            .open(directory)?;
        let _held = lock(path, uid, gid)?;
        match std::fs::remove_file(path) {
            Ok(()) => parent.sync_all(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    };
    unlink().map_err(|error| format!("remove {}: {error}", path.display()))
}

pub(crate) fn create(temporary: &Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(O_NOFOLLOW)
        .open(temporary)
}

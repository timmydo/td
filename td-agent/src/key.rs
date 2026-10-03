//! The API key on a development host (DESIGN.md §6): the one line of
//! `$XDG_CONFIG_HOME/td-agent/openrouter.key`, read by the window process
//! alone, which hands it to each conversation process over their
//! socketpair. There is no environment-variable form.
//!
//! The file is accepted only as a regular file with one link, owned by the
//! caller, mode 0600, as its opened descriptor says rather than its path;
//! it is opened without following a final symbolic link, and without
//! blocking, so a FIFO planted there is refused rather than waited on.
//! Every directory the path resolves through, from `/` down, must be owned
//! by the caller or by root and writable by neither group nor others, with
//! no exception for a sticky directory: then nobody but the caller and
//! root can change what the path names. The walk opens each directory by
//! descriptor, `O_PATH` and without following, beneath the one before, so
//! the directory checked is the directory used; a symbolic link on the way
//! (a home under `/var/home`, a dotfiles checkout) is followed by reading
//! it, and the directories its target passes through are checked as well.
//! It needs no `unsafe`: the flags are std's `OpenOptionsExt`, the checks
//! std's metadata.
//!
//! What is refused is refused by name. Neither the key nor any part of the
//! file ever reaches an error, a log or `Debug`.
//!
//! The window also writes the file, from the File menu's key dialog
//! (`write`): the same walk checks every directory down to the
//! configuration home before anything is made, `td-agent` is made mode
//! 0700 when it is missing, and the key goes into a temporary file made
//! new, without following a link, mode 0600, which is synced and renamed
//! over the key file; the directory is synced, and the file is read back
//! through `read`'s own checks.

use std::collections::VecDeque;
use std::ffi::{OsStr, OsString};
use std::fs::{DirBuilder, File, Metadata, OpenOptions, Permissions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::io::AsRawFd;
use std::path::{Component, Path, PathBuf};

// The flags below are x86-64's (aarch64 numbers O_NOFOLLOW and
// O_DIRECTORY otherwise), so the walk is held to it, as td-ui is.
#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
compile_error!("td-agent's key file walk uses Linux x86-64 open flags");

/// x86-64's open flags, as td-ui's control socket spells them.
const O_NONBLOCK: i32 = 0o4000;
const O_NOCTTY: i32 = 0o400;
const O_DIRECTORY: i32 = 0o200000;
const O_NOFOLLOW: i32 = 0o400000;
const O_PATH: i32 = 0o10000000;
/// `ELOOP`: a final symbolic link `O_NOFOLLOW` refused.
const ELOOP: i32 = 40;
/// The symbolic links a walk follows before it gives up, as the kernel's
/// own path resolution does.
const MAX_LINKS: usize = 40;
/// The longest key file read.
const MAX_BYTES: u64 = 4096;
/// The key file's name in td-agent's configuration directory.
pub const FILE: &str = "openrouter.key";
/// The file a write goes through, beside the key file. Its name is fixed,
/// so one a write left behind is refused by name rather than piling up.
pub const TEMPORARY: &str = "openrouter.key.tmp";
/// The longest key the dialog takes, in bytes.
pub const MAX_KEY: usize = 256;
/// What an OpenRouter key starts with.
pub const PREFIX: &str = "sk-or-";

/// The key: kept out of `Debug`, so it reaches no log by accident.
#[derive(Clone, Eq, PartialEq)]
pub struct Secret(String);

impl Secret {
    /// A key from the text that carried it (the socketpair's frame).
    pub fn new(text: String) -> Self {
        Self(text)
    }

    /// The key itself, for the one header and the one frame that carry
    /// it.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(..)")
    }
}

/// Why there is no key.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Problem {
    /// No file at the path: the usual first run.
    Missing(PathBuf),
    /// A file is there and is refused, for the reason given.
    Refused(String),
}

impl std::fmt::Display for Problem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing(path) => write!(
                f,
                "no API key: File \u{2192} Set OpenRouter key\u{2026} (F10) stores one, or write one line to {}, mode 0600",
                path.display()
            ),
            Self::Refused(why) => write!(f, "the API key file is refused: {why}"),
        }
    }
}

/// `$XDG_CONFIG_HOME/td-agent/openrouter.key`, beside the configuration.
pub fn path(config: &Path) -> Option<PathBuf> {
    Some(config.parent()?.join(FILE))
}

/// The key at `path`, checked from `/` down.
pub fn read(path: &Path) -> Result<Secret, Problem> {
    let status = std::fs::read_to_string("/proc/self/status")
        .map_err(|e| Problem::Refused(format!("this process's identity: {e}")))?;
    let uid = td_ui::proc_status::effective_uid(&status).map_err(Problem::Refused)?;
    read_checked(path, uid, &top())
}

/// The directory the walks check from: `/`, always, in every build that
/// ships.
#[cfg(not(feature = "test-key-root"))]
fn top() -> PathBuf {
    PathBuf::from("/")
}

/// The native fixture's (DESIGN.md §17): a test's `/` and `/tmp` are
/// shared (the trusted-root fixture's `/` is mode 1777), so its case names
/// the private directory its configuration home hangs under. Only the
/// `test-key-root` feature, which nothing ships with, reads the variable.
#[cfg(feature = "test-key-root")]
fn top() -> PathBuf {
    std::env::var_os("TD_AGENT_TEST_KEY_ROOT")
        .map(PathBuf::from)
        .filter(|top| top.is_absolute())
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// The key at `path` for the caller `uid`. Directories at or under `top`
/// are checked; `/` in production, and in tests a private directory, since
/// a test's own temporary tree hangs under a shared one.
fn read_checked(path: &Path, uid: u32, top: &Path) -> Result<Secret, Problem> {
    let shown = path.display();
    if !path.is_absolute() {
        return Err(Problem::Refused(format!("{shown} is not an absolute path")));
    }
    let name = match path.components().next_back() {
        Some(Component::Normal(name)) => name,
        _ => return Err(Problem::Refused(format!("{shown} names no file"))),
    };
    let parent = path
        .parent()
        .ok_or_else(|| Problem::Refused(format!("{shown} has no directory")))?;
    let directory = walk(parent, uid, top).map_err(|e| match e {
        Walk::Missing => Problem::Missing(path.to_path_buf()),
        Walk::Refused(why) => Problem::Refused(why),
    })?;
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(O_NOFOLLOW | O_NONBLOCK | O_NOCTTY)
        .open(beneath(&directory.file, name))
    {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Err(Problem::Missing(path.to_path_buf()))
        }
        Err(e) if e.raw_os_error() == Some(ELOOP) => {
            return Err(Problem::Refused(format!("{shown} is a symbolic link")))
        }
        Err(e) => return Err(Problem::Refused(format!("{shown}: {e}"))),
    };
    let meta = file
        .metadata()
        .map_err(|e| Problem::Refused(format!("{shown}: {e}")))?;
    if !meta.is_file() {
        return Err(Problem::Refused(format!("{shown} is not a regular file")));
    }
    if meta.nlink() != 1 {
        return Err(Problem::Refused(format!(
            "{shown} has {} links; a key file has one",
            meta.nlink()
        )));
    }
    if meta.uid() != uid {
        return Err(Problem::Refused(format!(
            "{shown} is owned by uid {}, not by you (uid {uid})",
            meta.uid()
        )));
    }
    if meta.mode() & 0o7777 != 0o600 {
        return Err(Problem::Refused(format!(
            "{shown} is mode {:04o}; a key file is mode 0600",
            meta.mode() & 0o7777
        )));
    }
    let mut bytes = Vec::new();
    file.take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| Problem::Refused(format!("{shown}: {e}")))?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err(Problem::Refused(format!(
            "{shown} is longer than {MAX_BYTES} bytes"
        )));
    }
    key(&bytes).map_err(|why| Problem::Refused(format!("{shown} {why}")))
}

/// The key a file's bytes hold: one line of visible ASCII, its newline
/// optional. The reasons name the fault, never the bytes.
fn key(bytes: &[u8]) -> Result<Secret, &'static str> {
    let line = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    if line.is_empty() {
        return Err("is empty");
    }
    if line.contains(&b'\n') {
        return Err("holds more than one line");
    }
    if !line.iter().all(|b| b.is_ascii_graphic()) {
        return Err("holds a space, a control byte or a byte that is not ASCII");
    }
    // Visible ASCII is UTF-8.
    let text = std::str::from_utf8(line).map_err(|_| "is not UTF-8")?;
    Ok(Secret(text.to_string()))
}

enum Walk {
    Missing,
    Refused(String),
}

/// A directory reached by the walk: its `O_PATH` descriptor and the path
/// it was reached by, symbolic links resolved.
struct Reached {
    file: File,
    path: PathBuf,
}

/// The `O_PATH` entry `name` beneath the directory `at` holds, through the
/// kernel's own link to that directory.
fn beneath(at: &File, name: &OsStr) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", at.as_raw_fd())).join(name)
}

fn open_path(path: &Path, flags: i32) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(O_PATH | O_NOFOLLOW | flags)
        .open(path)
}

/// The directory `path` names, every directory the walk passes through
/// at or under `top` checked.
fn walk(path: &Path, uid: u32, top: &Path) -> Result<Reached, Walk> {
    let root = Reached {
        file: open_path(Path::new("/"), O_DIRECTORY)
            .map_err(|e| Walk::Refused(format!("/: {e}")))?,
        path: PathBuf::from("/"),
    };
    check(&root, uid, top)?;
    let mut stack = vec![root];
    let mut queue: VecDeque<OsString> = names(path)?;
    let mut links = 0usize;
    while let Some(name) = queue.pop_front() {
        if name == ".." {
            if stack.len() > 1 {
                stack.pop();
            }
            continue;
        }
        let Some(here) = stack.last() else {
            return Err(Walk::Refused("the walk lost its root".into()));
        };
        let entry = beneath(&here.file, &name);
        let shown = here.path.join(&name);
        let file = match open_path(&entry, 0) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(Walk::Missing),
            Err(e) => return Err(Walk::Refused(format!("{}: {e}", shown.display()))),
        };
        let meta = file
            .metadata()
            .map_err(|e| Walk::Refused(format!("{}: {e}", shown.display())))?;
        if meta.file_type().is_symlink() {
            links += 1;
            if links > MAX_LINKS {
                return Err(Walk::Refused(format!(
                    "{} passes through more than {MAX_LINKS} symbolic links",
                    path.display()
                )));
            }
            // The link is read beneath its directory, which the walk has
            // checked, so its target is what the checked directory holds.
            let target = std::fs::read_link(&entry)
                .map_err(|e| Walk::Refused(format!("{}: {e}", shown.display())))?;
            if target.is_absolute() {
                stack.truncate(1);
            }
            for name in names(&target)?.into_iter().rev() {
                queue.push_front(name);
            }
            continue;
        }
        if !meta.is_dir() {
            return Err(Walk::Refused(format!(
                "{} is not a directory",
                shown.display()
            )));
        }
        let reached = Reached { file, path: shown };
        check(&reached, uid, top)?;
        stack.push(reached);
    }
    stack
        .pop()
        .ok_or_else(|| Walk::Refused("the walk lost its root".into()))
}

/// A path's names, `..` kept for the walk to take a directory back; a
/// name that is not a plain component is refused.
fn names(path: &Path) -> Result<VecDeque<OsString>, Walk> {
    let mut names = VecDeque::new();
    for component in path.components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::ParentDir => names.push_back(OsString::from("..")),
            Component::Normal(name) => names.push_back(name.to_os_string()),
            Component::Prefix(_) => {
                return Err(Walk::Refused(format!(
                    "{} has a component that is not a name",
                    path.display()
                )))
            }
        }
    }
    Ok(names)
}

/// One directory of the walk: owned by the caller or root, and writable
/// by neither group nor others, sticky or not.
fn check(reached: &Reached, uid: u32, top: &Path) -> Result<(), Walk> {
    if !reached.path.starts_with(top) {
        return Ok(());
    }
    let meta: Metadata = reached
        .file
        .metadata()
        .map_err(|e| Walk::Refused(format!("{}: {e}", reached.path.display())))?;
    let shown = reached.path.display();
    if meta.uid() != uid && meta.uid() != 0 {
        return Err(Walk::Refused(format!(
            "the directory {shown} is owned by uid {}, neither you (uid {uid}) nor root",
            meta.uid()
        )));
    }
    if meta.mode() & 0o022 != 0 {
        let who = match (meta.mode() & 0o020 != 0, meta.mode() & 0o002 != 0) {
            (true, true) => "group and others",
            (true, false) => "its group",
            _ => "others",
        };
        return Err(Walk::Refused(format!(
            "the directory {shown} is writable by {who} (mode {:04o})",
            meta.mode() & 0o7777
        )));
    }
    Ok(())
}

/// Whether `text` could be a key's bytes: what the socketpair's receiver
/// checks before it puts one in a header.
pub fn plausible(text: &str) -> bool {
    key(text.as_bytes()).is_ok() && !text.ends_with('\n')
}

/// A paste as the dialog takes it: the whitespace around it, a trailing
/// newline included, dropped.
pub fn trim_paste(text: &str) -> &str {
    text.trim()
}

/// Whether `text` is a key the dialog stores: refused with why, or taken,
/// with a warning when it does not look like OpenRouter's. One line of
/// printable ASCII, no spaces, at most `MAX_KEY` bytes. The reasons name
/// the fault, never the text.
pub fn check_key(text: &str) -> Result<Option<&'static str>, &'static str> {
    if text.is_empty() {
        return Err("the key is empty");
    }
    if text.len() > MAX_KEY {
        return Err("a key is at most 256 bytes");
    }
    if text.contains(' ') {
        return Err("a key holds no spaces");
    }
    if !text.bytes().all(|b| b.is_ascii_graphic()) {
        return Err("a key is printable ASCII: no control characters, nothing outside ASCII");
    }
    if !text.starts_with(PREFIX) {
        return Ok(Some(
            "an OpenRouter key starts with sk-or-; this one is saved anyway, for another base_url",
        ));
    }
    Ok(None)
}

/// Why a key was not written.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Unwritten {
    /// A key file is there, and replacing it was not asked for.
    Exists,
    /// Refused, for the reason given, which names the path at fault.
    Refused(String),
}

impl std::fmt::Display for Unwritten {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Exists => f.write_str("a key is already stored"),
            Self::Refused(why) => f.write_str(why),
        }
    }
}

/// Writes `secret` to the key file at `path`, replacing one there only
/// when `replace` says so, and answers the key as `read` reads it back.
pub fn write(path: &Path, secret: &Secret, replace: bool) -> Result<Secret, Unwritten> {
    let status = std::fs::read_to_string("/proc/self/status")
        .map_err(|e| Unwritten::Refused(format!("this process's identity: {e}")))?;
    let uid = td_ui::proc_status::effective_uid(&status).map_err(Unwritten::Refused)?;
    write_checked(path, secret, replace, uid, &top())
}

/// The directory a walk reached, opened to be synced or changed: a
/// descriptor `O_PATH` gives can do neither.
fn opened(reached: &Reached) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(O_DIRECTORY | O_NOCTTY)
        .open(format!("/proc/self/fd/{}/.", reached.file.as_raw_fd()))
}

/// `write` for the caller `uid`, directories at or under `top` checked,
/// as `read_checked`.
fn write_checked(
    path: &Path,
    secret: &Secret,
    replace: bool,
    uid: u32,
    top: &Path,
) -> Result<Secret, Unwritten> {
    let refused = |why: String| Unwritten::Refused(why);
    let shown = path.display();
    if let Err(why) = check_key(secret.expose()) {
        return Err(refused(why.into()));
    }
    if !path.is_absolute() {
        return Err(refused(format!("{shown} is not an absolute path")));
    }
    let name = match path.components().next_back() {
        Some(Component::Normal(name)) => name,
        _ => return Err(refused(format!("{shown} names no file"))),
    };
    let directory = path
        .parent()
        .ok_or_else(|| refused(format!("{shown} has no directory")))?;
    let made = match directory.components().next_back() {
        Some(Component::Normal(made)) => made,
        _ => {
            return Err(refused(format!(
                "{} names no directory to make",
                directory.display()
            )))
        }
    };
    let home = directory
        .parent()
        .ok_or_else(|| refused(format!("{} has no parent", directory.display())))?;
    // Every directory down to the configuration home is checked before
    // anything is made in it.
    let above = walk(home, uid, top).map_err(|e| match e {
        Walk::Missing => refused(format!(
            "{} does not exist; make it, mode 0700, and save again",
            home.display()
        )),
        Walk::Refused(why) => refused(why),
    })?;
    let fresh = match DirBuilder::new()
        .mode(0o700)
        .create(beneath(&above.file, made))
    {
        Ok(()) => true,
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => false,
        Err(e) => return Err(refused(format!("{}: {e}", directory.display()))),
    };
    if fresh {
        // Exactly 0700, whatever the umask took away; by name beneath the
        // checked parent, which needs no read access to the new directory
        // (a umask may have taken the owner's), and which only the caller
        // and root can change.
        std::fs::set_permissions(beneath(&above.file, made), Permissions::from_mode(0o700))
            .and_then(|()| opened(&above).and_then(|dir| dir.sync_all()))
            .map_err(|e| refused(format!("{}: {e}", directory.display())))?;
    }
    let reached = walk(directory, uid, top).map_err(|e| match e {
        Walk::Missing => refused(format!("{} is gone", directory.display())),
        Walk::Refused(why) => refused(why),
    })?;
    let target = beneath(&reached.file, name);
    match std::fs::symlink_metadata(&target) {
        Ok(meta) if !meta.file_type().is_file() => {
            return Err(refused(format!(
                "{shown} is not a regular file; remove it to store a key there"
            )))
        }
        Ok(_) if !replace => return Err(Unwritten::Exists),
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(refused(format!("{shown}: {e}"))),
    }
    let temporary = beneath(&reached.file, OsStr::new(TEMPORARY));
    let temporary_shown = directory.join(TEMPORARY);
    // Made new, so a link or a file planted under the name is refused,
    // never followed or reused.
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(O_NOFOLLOW | O_NOCTTY)
        .open(&temporary)
        .map_err(|e| match e.kind() {
            io::ErrorKind::AlreadyExists => refused(format!(
                "{} already exists: a save that did not finish left it, or something else put it there; remove it and save again",
                temporary_shown.display()
            )),
            _ => refused(format!("{}: {e}", temporary_shown.display())),
        })?;
    if let Err(why) = fill(file, secret, uid) {
        let _ = std::fs::remove_file(&temporary);
        return Err(refused(format!("{}: {why}", temporary_shown.display())));
    }
    if replace {
        if let Err(e) = std::fs::rename(&temporary, &target) {
            let _ = std::fs::remove_file(&temporary);
            return Err(refused(format!(
                "{}: renaming it over {shown}: {e}",
                temporary_shown.display()
            )));
        }
    } else {
        // A link, not a rename, so a key file that came since the check
        // above is kept, never replaced unasked.
        let linked = std::fs::hard_link(&temporary, &target);
        let _ = std::fs::remove_file(&temporary);
        match linked {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => return Err(Unwritten::Exists),
            Err(e) => {
                return Err(refused(format!(
                    "{}: linking it as {shown}: {e}",
                    temporary_shown.display()
                )))
            }
        }
    }
    opened(&reached)
        .and_then(|dir| dir.sync_all())
        .map_err(|e| {
            refused(format!(
                "{shown} holds the key, but syncing {}: {e}",
                directory.display()
            ))
        })?;
    verify(path, secret, uid, top)
}

/// The new file, checked by its descriptor and filled: exactly 0600, the
/// key and one newline, synced.
fn fill(mut file: File, secret: &Secret, uid: u32) -> Result<(), String> {
    file.set_permissions(Permissions::from_mode(0o600))
        .map_err(|e| e.to_string())?;
    let meta = file.metadata().map_err(|e| e.to_string())?;
    if !meta.is_file() || meta.nlink() != 1 || meta.uid() != uid {
        return Err("is not a new regular file of yours".into());
    }
    file.write_all(secret.expose().as_bytes())
        .and_then(|()| file.write_all(b"\n"))
        .and_then(|()| file.sync_all())
        .map_err(|e| e.to_string())
}

/// The key file read back through `read`'s checks, which the file just
/// written must pass, holding the key written.
fn verify(path: &Path, secret: &Secret, uid: u32, top: &Path) -> Result<Secret, Unwritten> {
    let read = read_checked(path, uid, top).map_err(|problem| {
        Unwritten::Refused(format!(
            "{} was written, but reading it back: {problem}",
            path.display()
        ))
    })?;
    if &read != secret {
        return Err(Unwritten::Refused(format!(
            "{} was written, but what it holds is not the key written",
            path.display()
        )));
    }
    Ok(read)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]
    use super::*;
    use std::os::unix::fs::{symlink, DirBuilderExt, PermissionsExt};

    const KEY: &str = "sk-or-v1-0123456789abcdef";

    /// A private tree the walk checks from, removed when dropped.
    struct Tree(PathBuf);
    impl Tree {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "td-agent-key-{}-{}",
                std::process::id(),
                crate::store::random_hex(4).unwrap()
            ));
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(&path)
                .unwrap();
            // The temporary directory may itself be reached through a
            // link: the walk reports resolved paths.
            Self(std::fs::canonicalize(&path).unwrap())
        }
        fn dir(&self, rel: &str, mode: u32) -> PathBuf {
            let path = self.0.join(rel);
            std::fs::DirBuilder::new()
                .mode(0o700)
                .recursive(true)
                .create(&path)
                .unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
            path
        }
        fn key(&self, rel: &str, bytes: &[u8], mode: u32) -> PathBuf {
            let path = self.0.join(rel);
            std::fs::write(&path, bytes).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
            path
        }
        fn read(&self, path: &Path) -> Result<Secret, Problem> {
            read_checked(path, uid(), &self.0)
        }
    }
    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn uid() -> u32 {
        let status = std::fs::read_to_string("/proc/self/status").unwrap();
        td_ui::proc_status::effective_uid(&status).unwrap()
    }

    fn refused(result: Result<Secret, Problem>) -> String {
        match result {
            Err(Problem::Refused(why)) => {
                assert!(!why.contains(KEY), "the key in a refusal: {why}");
                why
            }
            other => panic!("not refused: {other:?}"),
        }
    }

    #[test]
    fn a_key_file_as_the_rule_asks_is_read() {
        let tree = Tree::new();
        tree.dir("config/td-agent", 0o700);
        let path = tree.key(
            "config/td-agent/openrouter.key",
            format!("{KEY}\n").as_bytes(),
            0o600,
        );
        assert_eq!(tree.read(&path).unwrap().expose(), KEY);
        // The newline is optional.
        std::fs::write(&path, KEY).unwrap();
        assert_eq!(tree.read(&path).unwrap().expose(), KEY);
        // A path through `..` and `.` resolves.
        let roundabout = tree.0.join("config/./td-agent/../td-agent/openrouter.key");
        assert_eq!(tree.read(&roundabout).unwrap().expose(), KEY);
        // The key stays out of Debug.
        assert_eq!(format!("{:?}", tree.read(&path).unwrap()), "Secret(..)");
    }

    #[test]
    fn a_missing_file_or_directory_is_missing_not_refused() {
        let tree = Tree::new();
        let path = tree.0.join("config/td-agent/openrouter.key");
        assert_eq!(tree.read(&path), Err(Problem::Missing(path.clone())));
        tree.dir("config/td-agent", 0o700);
        assert_eq!(tree.read(&path), Err(Problem::Missing(path.clone())));
        assert!(Problem::Missing(path)
            .to_string()
            .contains("openrouter.key, mode 0600"));
    }

    #[test]
    fn each_fault_of_the_file_is_refused_by_name() {
        let tree = Tree::new();
        let dir = tree.dir("c", 0o700);
        let good = tree.key("c/good", KEY.as_bytes(), 0o600);
        // A final symbolic link, even to a good file.
        symlink(&good, dir.join("link")).unwrap();
        assert!(refused(tree.read(&dir.join("link"))).ends_with("is a symbolic link"));
        // Not a regular file: a directory, a FIFO (not waited on).
        tree.dir("c/sub", 0o700);
        assert!(refused(tree.read(&dir.join("sub"))).ends_with("is not a regular file"));
        let fifo = dir.join("fifo");
        let made = std::process::Command::new("mkfifo")
            .arg("-m")
            .arg("600")
            .arg(&fifo)
            .status();
        if made.is_ok_and(|s| s.success()) {
            assert!(refused(tree.read(&fifo)).ends_with("is not a regular file"));
        }
        // Two links.
        std::fs::hard_link(&good, dir.join("second")).unwrap();
        assert!(refused(tree.read(&good)).contains("has 2 links"));
        std::fs::remove_file(dir.join("second")).unwrap();
        // Any mode but 0600.
        for mode in [0o640, 0o604, 0o400, 0o700, 0o4600] {
            std::fs::set_permissions(&good, std::fs::Permissions::from_mode(mode)).unwrap();
            let why = refused(tree.read(&good));
            assert!(why.contains(&format!("is mode {mode:04o}")), "{why}");
        }
        std::fs::set_permissions(&good, std::fs::Permissions::from_mode(0o600)).unwrap();
        // Owned by someone else: the caller here is another uid, and no
        // directory is at or under the file to be checked for it.
        let why = refused(read_checked(&good, uid().wrapping_add(1), &good));
        assert!(why.contains("is owned by uid"), "{why}");
        assert!(tree.read(&good).is_ok());
    }

    #[test]
    fn a_key_is_one_line_of_visible_ascii() {
        let tree = Tree::new();
        tree.dir("c", 0o700);
        for (bytes, why) in [
            (&b""[..], "is empty"),
            (b"\n", "is empty"),
            (b"sk-1\nsk-2\n", "more than one line"),
            (b"sk 1", "a space"),
            (b"sk-1\r\n", "a control byte"),
            (b"sk-\x01", "a control byte"),
            ("sk-\u{e9}".as_bytes(), "not ASCII"),
        ] {
            let path = tree.key("c/k", bytes, 0o600);
            let refusal = refused(tree.read(&path));
            assert!(refusal.contains(why), "{bytes:?}: {refusal}");
        }
        let path = tree.key("c/k", &vec![b'k'; MAX_BYTES as usize + 1], 0o600);
        assert!(refused(tree.read(&path)).contains("longer than 4096 bytes"));
        assert!(plausible(KEY));
        assert!(!plausible("a b"));
        assert!(!plausible(""));
    }

    #[test]
    fn each_fault_of_a_directory_on_the_way_is_refused_by_name() {
        for (mode, who) in [
            (0o720, "its group"),
            (0o702, "others"),
            (0o722, "group and others"),
            // Sticky is no exception here, as it is for td-ui's sockets.
            (0o1777, "group and others"),
        ] {
            let tree = Tree::new();
            let dir = tree.dir("c", mode);
            let path = tree.key("c/k", KEY.as_bytes(), 0o600);
            let why = refused(tree.read(&path));
            assert!(
                why.contains(&format!("{} is writable by {who}", dir.display())),
                "{why}"
            );
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        // A directory owned by neither the caller nor root.
        let tree = Tree::new();
        tree.dir("c", 0o700);
        let path = tree.key("c/k", KEY.as_bytes(), 0o600);
        let why = refused(read_checked(&path, uid().wrapping_add(1), &tree.0));
        assert!(why.contains("neither you"), "{why}");
        // A plain file where a directory belongs.
        let why = refused(tree.read(&tree.0.join("c/k/k")));
        assert!(why.ends_with("is not a directory"), "{why}");
        assert!(refused(tree.read(Path::new("relative/k"))).contains("not an absolute path"));
    }

    #[test]
    fn a_link_on_the_way_is_followed_and_what_it_reaches_checked() {
        let tree = Tree::new();
        let real = tree.dir("dotfiles/td-agent", 0o700);
        tree.key("dotfiles/td-agent/k", KEY.as_bytes(), 0o600);
        // ~/.config -> dotfiles, relative, as a dotfiles checkout links it.
        symlink("dotfiles", tree.0.join("config")).unwrap();
        assert_eq!(
            tree.read(&tree.0.join("config/td-agent/k"))
                .unwrap()
                .expose(),
            KEY
        );
        // An absolute link too.
        symlink(&real, tree.0.join("direct")).unwrap();
        assert_eq!(tree.read(&tree.0.join("direct/k")).unwrap().expose(), KEY);
        // A link into a shared directory: the target's directories are
        // checked, so the file is refused for where it really is.
        let shared = tree.dir("shared", 0o777);
        tree.dir("shared/td-agent", 0o700);
        tree.key("shared/td-agent/k", KEY.as_bytes(), 0o600);
        symlink("shared/td-agent", tree.0.join("via")).unwrap();
        let why = refused(tree.read(&tree.0.join("via/k")));
        assert!(
            why.contains(&format!("{} is writable", shared.display())),
            "{why}"
        );
        // A loop of links ends.
        symlink("loop-b", tree.0.join("loop-a")).unwrap();
        symlink("loop-a", tree.0.join("loop-b")).unwrap();
        let why = refused(tree.read(&tree.0.join("loop-a/k")));
        assert!(why.contains("more than 40 symbolic links"), "{why}");
    }

    fn written(tree: &Tree, path: &Path, key: &str, replace: bool) -> Result<Secret, Unwritten> {
        write_checked(path, &Secret::new(key.into()), replace, uid(), &tree.0)
    }

    fn unwritten(result: Result<Secret, Unwritten>) -> String {
        match result {
            Err(Unwritten::Refused(why)) => {
                assert!(!why.contains(KEY), "the key in a refusal: {why}");
                why
            }
            other => panic!("not refused: {other:?}"),
        }
    }

    fn mode(path: &Path) -> u32 {
        std::fs::symlink_metadata(path).unwrap().mode() & 0o7777
    }

    /// Every file under `dir` whose bytes hold `needle`.
    fn holding(dir: &Path, needle: &[u8]) -> Vec<PathBuf> {
        let mut found = Vec::new();
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            let kind = entry.file_type().unwrap();
            if kind.is_dir() {
                found.extend(holding(&path, needle));
            } else if kind.is_file() {
                let bytes = std::fs::read(&path).unwrap();
                if bytes.windows(needle.len()).any(|w| w == needle) {
                    found.push(path);
                }
            }
        }
        found
    }

    #[test]
    fn a_dialogs_text_is_checked_and_a_paste_trimmed() {
        assert_eq!(check_key(KEY), Ok(None));
        assert_eq!(trim_paste(&format!("  {KEY}\r\n")), KEY);
        assert_eq!(trim_paste(&format!("\t{KEY}\n\n")), KEY);
        for (text, why) in [
            ("", "empty"),
            ("sk-or-a b", "no spaces"),
            ("sk-or-a\tb", "printable ASCII"),
            ("sk-or-\u{e9}", "printable ASCII"),
            ("sk-or-\u{7f}", "printable ASCII"),
        ] {
            let refused = check_key(text).unwrap_err();
            assert!(refused.contains(why), "{text:?}: {refused}");
        }
        let long = format!("{PREFIX}{}", "k".repeat(MAX_KEY - PREFIX.len()));
        assert_eq!(check_key(&long), Ok(None));
        assert!(check_key(&format!("{long}k"))
            .unwrap_err()
            .contains("256 bytes"));
        // Another provider's key: a warning, not a refusal.
        assert!(check_key("sk-other-1").unwrap().unwrap().contains("sk-or-"));
        // What the dialog takes, the read takes.
        assert!(plausible(KEY) && plausible(&long));
    }

    #[test]
    fn a_write_makes_a_0700_directory_and_a_0600_file_read_back() {
        let tree = Tree::new();
        let config = tree.dir("config", 0o700);
        let path = config.join("td-agent").join(FILE);
        let read = written(&tree, &path, KEY, false).unwrap();
        assert_eq!(read.expose(), KEY);
        assert_eq!(mode(&config.join("td-agent")), 0o700);
        assert_eq!(mode(&path), 0o600);
        assert_eq!(std::fs::read(&path).unwrap(), format!("{KEY}\n").as_bytes());
        assert_eq!(tree.read(&path).unwrap().expose(), KEY);
        // Nothing is left but the key file, and the key is in it alone.
        assert!(!config.join("td-agent").join(TEMPORARY).exists());
        assert_eq!(holding(&tree.0, KEY.as_bytes()), vec![path.clone()]);
        // An existing directory is used as it is.
        std::fs::set_permissions(
            config.join("td-agent"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        written(&tree, &path, "sk-or-v1-second", true).unwrap();
        assert_eq!(mode(&config.join("td-agent")), 0o755);
    }

    #[test]
    fn an_existing_key_is_replaced_only_when_asked() {
        let tree = Tree::new();
        tree.dir("config/td-agent", 0o700);
        let path = tree.key("config/td-agent/openrouter.key", b"sk-or-old\n", 0o600);
        assert_eq!(
            written(&tree, &path, KEY, false).unwrap_err(),
            Unwritten::Exists
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"sk-or-old\n");
        assert_eq!(written(&tree, &path, KEY, true).unwrap().expose(), KEY);
        let meta = std::fs::symlink_metadata(&path).unwrap();
        assert_eq!((meta.nlink(), meta.mode() & 0o7777), (1, 0o600));
        assert_eq!(tree.read(&path).unwrap().expose(), KEY);
        // A refused file (a mode the read refuses) is replaced too: the
        // rename puts a new file in its place.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(matches!(tree.read(&path), Err(Problem::Refused(_))));
        assert_eq!(
            written(&tree, &path, "sk-or-third", true).unwrap().expose(),
            "sk-or-third"
        );
    }

    #[test]
    fn a_write_is_refused_by_name_for_each_fault() {
        // A directory on the way that its group can write: refused before
        // anything is made under it.
        let tree = Tree::new();
        let config = tree.dir("config", 0o770);
        let path = config.join("td-agent").join(FILE);
        let why = unwritten(written(&tree, &path, KEY, false));
        assert!(
            why.contains(&format!("{} is writable by its group", config.display())),
            "{why}"
        );
        assert!(!config.join("td-agent").exists(), "nothing is made");
        // The configuration home missing.
        let tree = Tree::new();
        let path = tree.0.join("nowhere/td-agent").join(FILE);
        assert!(unwritten(written(&tree, &path, KEY, false)).contains("does not exist"));
        // A link where the temporary file goes: not followed.
        let tree = Tree::new();
        let dir = tree.dir("config/td-agent", 0o700);
        let elsewhere = tree.key("elsewhere", b"", 0o600);
        symlink(&elsewhere, dir.join(TEMPORARY)).unwrap();
        let why = unwritten(written(&tree, &dir.join(FILE), KEY, false));
        assert!(why.contains("already exists"), "{why}");
        assert!(why.contains(TEMPORARY), "{why}");
        assert_eq!(std::fs::read(&elsewhere).unwrap(), b"");
        assert!(std::fs::symlink_metadata(dir.join(TEMPORARY))
            .unwrap()
            .file_type()
            .is_symlink());
        // A temporary file already there: refused and left as it is.
        std::fs::remove_file(dir.join(TEMPORARY)).unwrap();
        tree.key("config/td-agent/openrouter.key.tmp", b"left", 0o600);
        assert!(unwritten(written(&tree, &dir.join(FILE), KEY, false)).contains("already exists"));
        assert_eq!(std::fs::read(dir.join(TEMPORARY)).unwrap(), b"left");
        std::fs::remove_file(dir.join(TEMPORARY)).unwrap();
        // A key path that is not a regular file: a directory, a link.
        tree.dir("config/td-agent/openrouter.key", 0o700);
        let why = unwritten(written(&tree, &dir.join(FILE), KEY, true));
        assert!(why.ends_with("remove it to store a key there"), "{why}");
        std::fs::remove_dir(dir.join(FILE)).unwrap();
        symlink(&elsewhere, dir.join(FILE)).unwrap();
        let why = unwritten(written(&tree, &dir.join(FILE), KEY, true));
        assert!(why.contains("is not a regular file"), "{why}");
        assert_eq!(std::fs::read(&elsewhere).unwrap(), b"");
        // A key the dialog would not take is not written.
        std::fs::remove_file(dir.join(FILE)).unwrap();
        assert!(unwritten(written(&tree, &dir.join(FILE), "a b", false)).contains("no spaces"));
        assert!(!dir.join(FILE).exists() && !dir.join(TEMPORARY).exists());
        // Nothing the writes made or refused holds the key.
        assert!(holding(&tree.0, KEY.as_bytes()).is_empty());
    }

    #[test]
    fn the_read_back_must_hold_the_key_written() {
        let tree = Tree::new();
        tree.dir("c", 0o700);
        let path = tree.key("c/k", b"sk-or-other\n", 0o600);
        let secret = Secret::new(KEY.into());
        let why = match verify(&path, &secret, uid(), &tree.0) {
            Err(Unwritten::Refused(why)) => why,
            other => panic!("{other:?}"),
        };
        assert!(why.contains("is not the key written"), "{why}");
        // And must pass the read's checks.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        let why = match verify(&path, &secret, uid(), &tree.0) {
            Err(Unwritten::Refused(why)) => why,
            other => panic!("{other:?}"),
        };
        assert!(why.contains("is mode 0640"), "{why}");
        std::fs::write(&path, format!("{KEY}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(verify(&path, &secret, uid(), &tree.0).unwrap(), secret);
    }

    /// Only `top` knows the fixture feature, and only the manifest's
    /// feature table and gate metadata name it.
    #[test]
    fn the_fixture_feature_moves_only_the_walks_top() {
        let source = include_str!("key.rs");
        assert_eq!(source.matches("feature = \"test-key-root\")").count(), 2);
        assert!(source.contains(
            "#[cfg(not(feature = \"test-key-root\"))]\nfn top() -> PathBuf {\n    PathBuf::from(\"/\")\n}"
        ));
        let manifest = include_str!("../Cargo.toml");
        assert!(manifest.contains("\n[features]\ntest-key-root = []\n"));
        assert!(manifest.contains("native-compositor-fixture-feature = \"test-key-root\""));
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        for entry in std::fs::read_dir(src).unwrap().flatten() {
            let name = entry.file_name();
            if name != "key.rs" {
                let text = std::fs::read_to_string(entry.path()).unwrap();
                assert!(!text.contains("test-key-root"), "{name:?}");
                assert!(!text.contains("TD_AGENT_TEST_KEY_ROOT"), "{name:?}");
            }
        }
    }

    /// The window's read checks every directory from `/`.
    #[test]
    fn the_production_walk_starts_at_the_root() {
        let tree = Tree::new();
        tree.dir("c", 0o700);
        let path = tree.key("c/k", KEY.as_bytes(), 0o600);
        assert_eq!(read(&path), read_checked(&path, uid(), Path::new("/")));
    }
}

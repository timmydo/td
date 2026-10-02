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

use std::collections::VecDeque;
use std::ffi::{OsStr, OsString};
use std::fs::{File, Metadata, OpenOptions};
use std::io::{self, Read};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
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
                "no API key: write one line to {}, mode 0600",
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
    read_checked(path, uid, Path::new("/"))
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

    /// The window's read checks every directory from `/`.
    #[test]
    fn the_production_walk_starts_at_the_root() {
        let tree = Tree::new();
        tree.dir("c", 0o700);
        let path = tree.key("c/k", KEY.as_bytes(), 0o600);
        assert_eq!(read(&path), read_checked(&path, uid(), Path::new("/")));
    }
}

//! The copies OpenURI.OpenFile hands Firefox (APPLICATIONS.md §E row 4).
//!
//! td-authd maps Firefox's private `Opened` directory writable at
//! `/var/td-portal-files/1000/Opened` for this portal alone; Firefox reads
//! the same directory through its read-only `~/Opened` grant. Each copy is
//! one file in a fresh subdirectory, so it keeps the caller's file name for
//! Firefox to pick a viewer by. Firefox owns the source on disk and could
//! plant links in it, so every name is resolved from a held directory
//! descriptor without following links, and every new entry is created
//! exclusively.

use super::*;
use std::collections::VecDeque;
use std::fs::{DirBuilder, File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, FileExt, MetadataExt, OpenOptionsExt};

pub(super) const HOST: &str = "/var/td-portal-files/1000/Opened";
const GUEST: &str = "/home/td/Opened";
/// The largest file copied: a mail attachment or a generated page, not a
/// disk image.
pub(super) const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;
/// Copies kept at once; older ones are removed as new ones arrive.
const KEPT: usize = 16;
/// Entries one removal visits, at most `MAX_DEPTH` deep, so a planted tree
/// cannot stall startup or a copy's retention; anything past them is left.
const MAX_CLEARED: usize = 4096;
const MAX_DEPTH: usize = 4;
const MAX_NAME_BYTES: usize = 128;
const CHUNK: usize = 64 * 1024;
const O_DIRECTORY: i32 = 0x10000;
const O_NOFOLLOW: i32 = 0x20000;

pub(super) struct Handoff {
    host: PathBuf,
    /// Subdirectories this run created, oldest first.
    kept: VecDeque<String>,
    sequence: u64,
}

impl Default for Handoff {
    fn default() -> Self {
        Self::at(PathBuf::from(HOST))
    }
}

/// One copy: the URL Firefox opens it by.
pub(super) struct Copied {
    pub url: String,
}

fn pinned(directory: &File, name: impl AsRef<Path>) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd())).join(name)
}

fn open_directory(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(O_DIRECTORY | O_NOFOLLOW)
        .open(path)
}

/// The directory at `path` when it is a directory this process owns that
/// no one else may enter.
fn private_directory(path: &Path) -> Result<File, &'static str> {
    let directory =
        open_directory(path).map_err(|_| "the Firefox handoff directory is unavailable")?;
    let metadata = directory
        .metadata()
        .map_err(|_| "the Firefox handoff directory is unavailable")?;
    let owner = current_uid().map_err(|_| "the portal cannot identify itself")?;
    if !metadata.is_dir() || metadata.uid() != owner || metadata.mode() & 0o077 != 0 {
        return Err("the Firefox handoff directory is not the portal's private view");
    }
    Ok(directory)
}

/// `name`'s last component reduced to letters, digits and `.`, `_`, `+`,
/// `-`, with no leading dot and at most `MAX_NAME_BYTES` bytes keeping its
/// extension, or `file` when nothing is left. It is one path element and
/// one URL path segment that needs no escaping.
pub(super) fn file_name(name: &str) -> String {
    let base = name
        .strip_suffix(" (deleted)")
        .unwrap_or(name)
        .rsplit('/')
        .next()
        .unwrap_or_default();
    let mut clean: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '+' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if clean.starts_with('.') {
        clean.replace_range(..1, "_");
    }
    if clean.len() > MAX_NAME_BYTES {
        let extension = clean
            .rfind('.')
            .and_then(|at| clean.get(at..))
            .filter(|extension| extension.len() <= 16)
            .unwrap_or_default()
            .to_string();
        clean.truncate(MAX_NAME_BYTES - extension.len());
        clean.push_str(&extension);
    }
    if clean.is_empty() || clean.chars().all(|c| c == '_') {
        "file".into()
    } else {
        clean
    }
}

/// The name a received descriptor was opened by, as the kernel spells it.
pub(super) fn descriptor_name(file: &File) -> String {
    fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd()))
        .ok()
        .and_then(|path| path.to_str().map(file_name))
        .unwrap_or_else(|| "file".into())
}

impl Handoff {
    pub(super) fn at(host: PathBuf) -> Self {
        Self {
            host,
            kept: VecDeque::new(),
            sequence: 0,
        }
    }

    /// Removes what an earlier run left: Firefox has nothing open from it
    /// that a reload needs more than a fresh start does.
    pub(super) fn clear(&mut self) -> Result<(), &'static str> {
        let root = private_directory(&self.host)?;
        let entries = fs::read_dir(pinned(&root, ""))
            .map_err(|_| "the Firefox handoff directory cannot be listed")?;
        let mut budget = MAX_CLEARED;
        for entry in entries {
            if budget == 0 {
                break;
            }
            let entry = entry.map_err(|_| "the Firefox handoff directory cannot be listed")?;
            remove_within(&root, entry.file_name(), &mut budget, MAX_DEPTH);
        }
        self.kept.clear();
        Ok(())
    }

    /// Copies `source`, which must be a readable regular file of at most
    /// `MAX_FILE_BYTES`, as `name` into a fresh subdirectory, and keeps the
    /// newest `KEPT` copies.
    pub(super) fn copy(&mut self, source: &File, name: &str) -> Result<Copied, &'static str> {
        let metadata = source
            .metadata()
            .map_err(|_| "the descriptor cannot be inspected")?;
        if !metadata.is_file() {
            return Err("OpenFile opens only a regular file");
        }
        if metadata.len() > MAX_FILE_BYTES {
            return Err("the file is larger than the portal copies");
        }
        let root = private_directory(&self.host)?;
        self.sequence = self.sequence.wrapping_add(1);
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.as_nanos());
        let directory = format!("{stamp:x}-{:x}", self.sequence);
        DirBuilder::new()
            .mode(0o700)
            .create(pinned(&root, &directory))
            .map_err(|_| "the copy's directory cannot be created")?;
        // A failed copy leaves nothing behind and evicts nothing.
        if let Err(why) = fill(&root, &directory, source, name) {
            remove(&root, &directory);
            return Err(why);
        }
        self.kept.push_back(directory.clone());
        while self.kept.len() > KEPT {
            if let Some(old) = self.kept.pop_front() {
                remove(&root, &old);
            }
        }
        Ok(Copied {
            url: format!("file://{GUEST}/{directory}/{name}"),
        })
    }
}

/// Creates `name` in the new private `directory` and copies `source` into it.
fn fill(root: &File, directory: &str, source: &File, name: &str) -> Result<(), &'static str> {
    let parent = private_directory(&pinned(root, directory))?;
    let mut target = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(O_NOFOLLOW)
        .open(pinned(&parent, name))
        .map_err(|_| "the copy cannot be created")?;
    copy_from_start(source, &mut target)
}

/// Copies `source` from its first byte whatever its offset, refusing one
/// that grows past `MAX_FILE_BYTES` while it is read.
fn copy_from_start(source: &File, target: &mut File) -> Result<(), &'static str> {
    let mut buffer = vec![0u8; CHUNK];
    let mut at = 0u64;
    loop {
        let read = match source.read_at(&mut buffer, at) {
            Ok(0) => return Ok(()),
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return Err("the file cannot be read"),
        };
        at = at
            .checked_add(read as u64)
            .filter(|at| *at <= MAX_FILE_BYTES)
            .ok_or("the file is larger than the portal copies")?;
        target
            .write_all(buffer.get(..read).unwrap_or_default())
            .map_err(|_| "the copy cannot be written")?;
    }
}

/// Removes `name` under `root`, without following a link it may be.
fn remove(root: &File, name: impl AsRef<Path>) {
    let mut budget = MAX_CLEARED;
    remove_within(root, name, &mut budget, MAX_DEPTH);
}

/// Removes `name` in `root` without following links, visiting at most
/// `budget` entries and `depth` levels; whatever is left over stays.
fn remove_within(root: &File, name: impl AsRef<Path>, budget: &mut usize, depth: usize) {
    let Some(left) = budget.checked_sub(1) else {
        return;
    };
    *budget = left;
    let path = pinned(root, name);
    let Ok(metadata) = fs::symlink_metadata(&path) else {
        return;
    };
    if !metadata.is_dir() {
        let _ = fs::remove_file(&path);
        return;
    }
    if let (Some(below), Ok(directory)) = (depth.checked_sub(1), open_directory(&path)) {
        if let Ok(entries) = fs::read_dir(pinned(&directory, "")) {
            for entry in entries.map_while(Result::ok) {
                if *budget == 0 {
                    break;
                }
                remove_within(&directory, entry.file_name(), budget, below);
            }
        }
    }
    let _ = fs::remove_dir(&path);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::PermissionsExt;

    fn private_root(name: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("td-portal-handoff-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        DirBuilder::new().mode(0o700).create(&root).unwrap();
        root
    }

    #[test]
    fn a_name_is_one_safe_element_that_keeps_its_extension() {
        assert_eq!(file_name("/home/td/Downloads/report.pdf"), "report.pdf");
        assert_eq!(
            file_name("/tmp/td-news-digest.html (deleted)"),
            "td-news-digest.html"
        );
        assert_eq!(file_name("/x/Résumé final.pdf"), "R_sum__final.pdf");
        assert_eq!(file_name("/x/.hidden"), "_hidden");
        assert_eq!(file_name("/x/..."), "_..");
        assert_eq!(file_name(""), "file");
        assert_eq!(file_name("/"), "file");
        assert_eq!(file_name("/x/日本"), "file");
        let long = format!("/x/{}.pdf", "a".repeat(300));
        let kept = file_name(&long);
        assert_eq!(kept.len(), MAX_NAME_BYTES);
        assert!(kept.ends_with(".pdf"));
    }

    #[test]
    fn a_copy_is_the_whole_file_in_a_fresh_private_directory() {
        let root = private_root("copy");
        let source_path = root.join("source");
        fs::write(&source_path, b"attachment bytes").unwrap();
        let mut source = File::open(&source_path).unwrap();
        // The caller's offset is not the copy's start.
        let mut skip = [0u8; 4];
        source.read_exact(&mut skip).unwrap();
        let host = root.join("view");
        DirBuilder::new().mode(0o700).create(&host).unwrap();
        let mut handoff = Handoff::at(host.clone());
        let copied = handoff.copy(&source, "report.pdf").unwrap();
        let tail = copied.url.strip_prefix("file:///home/td/Opened/").unwrap();
        let (directory, name) = tail.split_once('/').unwrap();
        assert_eq!(name, "report.pdf");
        let copy = host.join(directory).join(name);
        assert_eq!(fs::read(&copy).unwrap(), b"attachment bytes");
        assert_eq!(fs::metadata(&copy).unwrap().mode() & 0o777, 0o600);
        assert_eq!(
            fs::metadata(host.join(directory)).unwrap().mode() & 0o777,
            0o700
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn only_a_regular_file_is_copied_and_the_view_must_be_private() {
        let root = private_root("refuse");
        let host = root.join("view");
        DirBuilder::new().mode(0o700).create(&host).unwrap();
        let mut handoff = Handoff::at(host.clone());
        let directory = File::open(&root).unwrap();
        assert!(handoff.copy(&directory, "x").is_err());
        fs::write(root.join("f"), b"x").unwrap();
        let file = File::open(root.join("f")).unwrap();
        fs::set_permissions(&host, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(handoff.copy(&file, "x").is_err());
        fs::set_permissions(&host, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(handoff.copy(&file, "x").is_ok());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_planted_link_is_neither_followed_nor_written_through() {
        let root = private_root("link");
        let host = root.join("view");
        DirBuilder::new().mode(0o700).create(&host).unwrap();
        let elsewhere = root.join("elsewhere");
        DirBuilder::new().mode(0o700).create(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, host.join("planted")).unwrap();
        fs::write(root.join("f"), b"x").unwrap();
        let file = File::open(root.join("f")).unwrap();
        // A view that is itself a link is refused.
        let linked = root.join("linked-view");
        std::os::unix::fs::symlink(&host, &linked).unwrap();
        assert!(Handoff::at(linked).copy(&file, "x").is_err());
        let mut handoff = Handoff::at(host.clone());
        handoff.clear().unwrap();
        assert!(!host.join("planted").exists());
        assert!(fs::symlink_metadata(host.join("planted")).is_err());
        assert!(elsewhere.exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn only_the_newest_copies_are_kept() {
        let root = private_root("kept");
        let host = root.join("view");
        DirBuilder::new().mode(0o700).create(&host).unwrap();
        fs::write(root.join("f"), b"x").unwrap();
        let file = File::open(root.join("f")).unwrap();
        let mut handoff = Handoff::at(host.clone());
        for _ in 0..KEPT + 3 {
            handoff.copy(&file, "x").unwrap();
        }
        assert_eq!(fs::read_dir(&host).unwrap().count(), KEPT);
        // A copy that fails leaves no directory and evicts no kept copy.
        let before: Vec<_> = handoff.kept.iter().cloned().collect();
        assert!(handoff.copy(&file, "absent/x").is_err());
        assert_eq!(handoff.kept.iter().cloned().collect::<Vec<_>>(), before);
        assert_eq!(fs::read_dir(&host).unwrap().count(), KEPT);
        // Clearing also removes a name that is not UTF-8, and stops at its
        // depth bound rather than walking a planted tree to its end.
        fs::create_dir(host.join(std::ffi::OsStr::from_bytes(b"\xff"))).unwrap();
        let deep = host.join("a/b/c/d/e/f");
        fs::create_dir_all(&deep).unwrap();
        handoff.clear().unwrap();
        assert!(deep.exists());
        assert!(!host.join(std::ffi::OsStr::from_bytes(b"\xff")).exists());
        fs::remove_dir_all(host.join("a")).unwrap();
        assert_eq!(fs::read_dir(&host).unwrap().count(), 0);
        let _ = fs::remove_dir_all(&root);
    }
}

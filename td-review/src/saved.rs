//! The repositories the window was opened on, most recent first: the list
//! `--choose-repo` offers before its folder browser. One absolute path per
//! line in `$XDG_CONFIG_HOME/td-review/repositories`, else
//! `~/.config/td-review/repositories`; blank lines and `#` comments are
//! skipped. It is rewritten whole on every change, so a hand edit's
//! comments do not survive the next one. It lives outside the `src` tree
//! an application view may write (td-authd/DESIGN.md, "Application
//! filesystem grants").

use std::env;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

/// The most paths kept; recording one more drops the oldest.
pub const MAX_SAVED: usize = 32;
/// A file larger than this is not read, and so not rewritten either.
const MAX_FILE_BYTES: u64 = 64 * 1024;
/// The longest path kept: one the chooser can list as a name.
const MAX_PATH_BYTES: usize = td_ui::finder::NAME_BYTES;

const HEADER: &str = "# Repositories td-review opened, most recent first; one path per line.\n";

/// Where the list lives, from the environment; none without an absolute
/// `XDG_CONFIG_HOME` or `HOME`.
pub fn file() -> Option<PathBuf> {
    let absolute = |name: &str| {
        env::var_os(name)
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
    };
    match absolute("XDG_CONFIG_HOME") {
        Some(config) => Some(config.join("td-review").join("repositories")),
        None => absolute("HOME").map(|home| home.join(".config/td-review/repositories")),
    }
}

/// Whether `path` can be kept: absolute, UTF-8, control-free and short
/// enough to list.
fn keepable(path: &Path) -> bool {
    path.is_absolute()
        && path
            .to_str()
            .is_some_and(|s| s.len() <= MAX_PATH_BYTES && !s.chars().any(char::is_control))
}

/// The paths in a file's text, in order: blank lines, comments, paths
/// that cannot be kept and repeats skipped, at most `MAX_SAVED`. A path is
/// the whole line, so one ending in a space reads back as written.
pub fn parse(text: &str) -> Vec<PathBuf> {
    let mut saved: Vec<PathBuf> = Vec::new();
    for line in text.lines() {
        if saved.len() == MAX_SAVED {
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let path = PathBuf::from(line);
        if keepable(&path) && !saved.contains(&path) {
            saved.push(path);
        }
    }
    saved
}

/// The list in `file`; empty when there is none yet.
pub fn load(file: &Path) -> io::Result<Vec<PathBuf>> {
    let opened = match fs::File::open(file) {
        Ok(opened) => opened,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(io::Error::new(e.kind(), format!("{}: {e}", file.display()))),
    };
    let mut bytes = Vec::new();
    opened.take(MAX_FILE_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(io::Error::other(format!(
            "{} is larger than {} KiB",
            file.display(),
            MAX_FILE_BYTES / 1024
        )));
    }
    let text = String::from_utf8(bytes).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} is not UTF-8", file.display()),
        )
    })?;
    Ok(parse(&text))
}

/// The folder `file` is in, created if it is not there yet.
fn folder(file: &Path) -> io::Result<&Path> {
    let dir = file
        .parent()
        .ok_or_else(|| io::Error::other(format!("{} has no directory", file.display())))?;
    fs::create_dir_all(dir)?;
    Ok(dir)
}

/// Writes `saved` to `file` whole, through a sibling synced and renamed
/// over it, so a reader never sees half a list. A link is written through,
/// to the file it names, so a list kept elsewhere stays linked.
pub fn store(file: &Path, saved: &[PathBuf]) -> io::Result<()> {
    let linked = fs::symlink_metadata(file).is_ok_and(|m| m.file_type().is_symlink());
    let target = if linked {
        fs::canonicalize(file)
            .map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", file.display())))?
    } else {
        file.to_path_buf()
    };
    let file = target.as_path();
    let dir = folder(file)?;
    let mut text = String::from(HEADER);
    for path in saved.iter().filter(|p| keepable(p)).take(MAX_SAVED) {
        if let Some(s) = path.to_str() {
            text.push_str(s);
            text.push('\n');
        }
    }
    let staged = dir.join(format!(".repositories.{}", std::process::id()));
    let written = fs::File::create(&staged)
        .and_then(|mut out| {
            out.write_all(text.as_bytes())?;
            out.sync_all()
        })
        .and_then(|()| fs::rename(&staged, file));
    if written.is_err() {
        let _ = fs::remove_file(&staged);
    }
    written
}

/// `saved` with `repo` moved to the front.
pub fn recorded(mut saved: Vec<PathBuf>, repo: &Path) -> Vec<PathBuf> {
    saved.retain(|p| p != repo);
    if keepable(repo) {
        saved.insert(0, repo.to_path_buf());
    }
    saved.truncate(MAX_SAVED);
    saved
}

/// Changes the list in `file` as it is now, holding a lock on a sibling
/// for the read and the write, so two windows' changes do not undo each
/// other. `change` answers the new list, or none to leave it. A list that
/// cannot be read is left as it is.
fn change(
    file: &Path,
    change: impl FnOnce(Vec<PathBuf>) -> Option<Vec<PathBuf>>,
) -> io::Result<()> {
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(folder(file)?.join(".repositories.lock"))?;
    // Released when `lock` is closed.
    lock.lock()?;
    match change(load(file)?) {
        Some(saved) => store(file, &saved),
        None => Ok(()),
    }
}

/// Drops `repo` from the list in `file`.
pub fn forget(file: &Path, repo: &Path) -> io::Result<()> {
    change(file, |mut saved| {
        let before = saved.len();
        saved.retain(|p| p != repo);
        (saved.len() != before).then_some(saved)
    })
}

/// Puts `repo` first in the list in `file`.
pub fn record(file: &Path, repo: &Path) -> io::Result<()> {
    change(file, |saved| {
        (saved.first().map(PathBuf::as_path) != Some(repo)).then(|| recorded(saved, repo))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let root = env::temp_dir().join(format!("td-review-saved-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        root
    }

    #[test]
    fn parsing_keeps_absolute_paths_once_in_order() {
        let text = "# note\n\n  \n/a/one\nrelative\n /a/indented\n/a/two\n/a/one\n/a/\u{7}bell\n";
        assert_eq!(
            parse(text),
            [PathBuf::from("/a/one"), PathBuf::from("/a/two")]
        );
    }

    #[test]
    fn a_path_is_its_whole_line() {
        assert_eq!(
            parse("/a/trailing \n  # indented comment\n"),
            [PathBuf::from("/a/trailing ")]
        );
    }

    #[test]
    fn forgetting_drops_one_path_from_the_list_as_it_is_now() {
        let root = scratch("forget");
        let file = root.join("repositories");
        store(&file, &[PathBuf::from("/a"), PathBuf::from("/b")]).unwrap();
        // Recorded by another window after this one read the list.
        record(&file, Path::new("/c")).unwrap();
        forget(&file, Path::new("/a")).unwrap();
        assert_eq!(
            load(&file).unwrap(),
            [PathBuf::from("/c"), PathBuf::from("/b")]
        );
        forget(&file, Path::new("/absent")).unwrap();
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_linked_list_is_written_through_its_link() {
        let root = scratch("linked");
        let kept = root.join("dotfiles/repositories");
        fs::create_dir_all(root.join("dotfiles")).unwrap();
        fs::create_dir_all(root.join("config")).unwrap();
        fs::write(&kept, "/a\n").unwrap();
        let file = root.join("config/repositories");
        std::os::unix::fs::symlink(&kept, &file).unwrap();
        record(&file, Path::new("/b")).unwrap();
        assert!(fs::symlink_metadata(&file)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(
            load(&kept).unwrap(),
            [PathBuf::from("/b"), PathBuf::from("/a")]
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_store_that_cannot_write_leaves_nothing_staged() {
        let root = scratch("unwritable");
        fs::create_dir_all(&root).unwrap();
        // The target is a directory, so the rename fails.
        let file = root.join("repositories");
        fs::create_dir(&file).unwrap();
        fs::write(file.join("keep"), "").unwrap();
        assert!(store(&file, &[PathBuf::from("/a")]).is_err());
        assert_eq!(fs::read_dir(&root).unwrap().count(), 1);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn parsing_stops_at_the_cap() {
        let text: String = (0..MAX_SAVED + 5).map(|i| format!("/r/{i}\n")).collect();
        let saved = parse(&text);
        assert_eq!(saved.len(), MAX_SAVED);
        assert_eq!(
            saved.last(),
            Some(&PathBuf::from(format!("/r/{}", MAX_SAVED - 1)))
        );
    }

    #[test]
    fn recording_moves_a_path_to_the_front_and_drops_the_oldest() {
        let saved = vec![PathBuf::from("/a"), PathBuf::from("/b")];
        assert_eq!(
            recorded(saved, Path::new("/b")),
            [PathBuf::from("/b"), PathBuf::from("/a")]
        );
        let full: Vec<PathBuf> = (0..MAX_SAVED)
            .map(|i| PathBuf::from(format!("/{i}")))
            .collect();
        let after = recorded(full, Path::new("/new"));
        assert_eq!(after.len(), MAX_SAVED);
        assert_eq!(after.first(), Some(&PathBuf::from("/new")));
        assert!(!after.contains(&PathBuf::from(format!("/{}", MAX_SAVED - 1))));
        // A path that cannot be kept is not recorded.
        assert_eq!(
            recorded(Vec::new(), Path::new("relative")),
            Vec::<PathBuf>::new()
        );
    }

    #[test]
    fn a_recorded_list_reads_back_and_a_missing_one_is_empty() {
        let root = scratch("round");
        let file = root.join("td-review/repositories");
        assert_eq!(load(&file).unwrap(), Vec::<PathBuf>::new());
        record(&file, Path::new("/src/one")).unwrap();
        record(&file, Path::new("/src/two")).unwrap();
        record(&file, Path::new("/src/one")).unwrap();
        assert_eq!(
            load(&file).unwrap(),
            [PathBuf::from("/src/one"), PathBuf::from("/src/two")]
        );
        let text = fs::read_to_string(&file).unwrap();
        assert!(text.starts_with('#'), "{text}");
        // Nothing staged is left beside it, only the lock.
        let mut beside: Vec<_> = fs::read_dir(root.join("td-review"))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        beside.sort();
        assert_eq!(beside, [".repositories.lock", "repositories"]);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_list_that_cannot_be_read_is_not_overwritten() {
        let root = scratch("refused");
        let file = root.join("repositories");
        fs::create_dir_all(&root).unwrap();
        fs::write(&file, vec![b'#'; MAX_FILE_BYTES as usize + 1]).unwrap();
        assert!(record(&file, Path::new("/src/one")).is_err());
        assert_eq!(fs::metadata(&file).unwrap().len(), MAX_FILE_BYTES + 1);
        fs::write(&file, b"/src/\xff\n").unwrap();
        assert!(load(&file).is_err());
        assert!(record(&file, Path::new("/src/one")).is_err());
        assert_eq!(fs::read(&file).unwrap(), b"/src/\xff\n");
        fs::remove_dir_all(&root).unwrap();
    }
}

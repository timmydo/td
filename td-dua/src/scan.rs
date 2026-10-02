//! The walk that builds a `Tree` from a directory. It never follows a
//! symbolic link, never leaves the starting directory's file system (a
//! directory on another one is listed as a mount and not entered), and
//! counts an inode with several hard links once.

use std::collections::HashSet;
use std::fs::{self, Metadata};
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::tree::{Error as TreeError, Identity, Kind, Node, NodeId, Size, Tree, ROOT};

/// What a running walk shows the window, and how the window stops it: a
/// cancel is final, for the window's end.
#[derive(Debug, Default)]
pub struct Progress {
    pub entries: AtomicU64,
    pub bytes: AtomicU64,
    pub cancel: AtomicBool,
}

impl Progress {
    pub fn reset(&self) {
        self.entries.store(0, Ordering::Relaxed);
        self.bytes.store(0, Ordering::Relaxed);
    }
}

fn kind(meta: &Metadata) -> Kind {
    let kind = meta.file_type();
    if kind.is_dir() {
        Kind::Dir
    } else if kind.is_file() {
        Kind::File
    } else if kind.is_symlink() {
        Kind::Symlink
    } else {
        Kind::Other
    }
}

fn node(name: std::ffi::OsString, meta: &Metadata, counted: &mut HashSet<Identity>) -> Node {
    let identity = Identity {
        device: meta.dev(),
        inode: meta.ino(),
    };
    let kind = kind(meta);
    let links = kind != Kind::Dir && meta.nlink() > 1;
    // A second name for an inode already counted owns none of its blocks.
    let repeat = links && !counted.insert(identity);
    let own = if repeat {
        Size::default()
    } else {
        Size {
            allocated: meta.blocks().saturating_mul(512),
            apparent: meta.len(),
        }
    };
    let mut node = Node::new(name, kind, own, meta.mtime(), identity);
    node.length = meta.len();
    node.links = links;
    node
}

/// Walks the directory at `path`, an absolute path, into a tree whose root
/// is that directory. A listing that cannot be read marks its node
/// unreadable and the walk goes on; a cancel ends it with `Interrupted`.
pub fn scan(path: &Path, progress: &Progress) -> io::Result<Tree> {
    let meta = fs::symlink_metadata(path)?;
    if !meta.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not a directory", path.display()),
        ));
    }
    let device = meta.dev();
    let mut counted = HashSet::new();
    let root = node(path.as_os_str().to_owned(), &meta, &mut counted);
    let mut tree = Tree::new(path.to_path_buf(), root);
    let mut stack: Vec<(NodeId, PathBuf)> = vec![(ROOT, path.to_path_buf())];
    'walk: while let Some((id, dir)) = stack.pop() {
        if progress.cancel.load(Ordering::Relaxed) {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "scan cancelled"));
        }
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => {
                mark_unreadable(&mut tree, id);
                continue;
            }
        };
        for entry in entries {
            let Ok(entry) = entry else {
                mark_unreadable(&mut tree, id);
                continue;
            };
            // DirEntry::metadata does not follow a symbolic link.
            let Ok(meta) = entry.metadata() else {
                mark_unreadable(&mut tree, id);
                continue;
            };
            let mut child = node(entry.file_name(), &meta, &mut counted);
            let descend = child.kind == Kind::Dir && meta.dev() == device;
            if child.kind == Kind::Dir && !descend {
                // Its blocks are another file system's, as `du -x` says.
                child.kind = Kind::Mount;
                child.files = 0;
                child.own = Size::default();
                child.total = Size::default();
            }
            progress.entries.fetch_add(1, Ordering::Relaxed);
            progress
                .bytes
                .fetch_add(child.own.allocated, Ordering::Relaxed);
            match tree.push(id, child) {
                Ok(child) if descend => stack.push((child, entry.path())),
                Ok(_) => {}
                Err(TreeError::Full) => {
                    tree.partial = true;
                    break 'walk;
                }
                Err(TreeError::Gone) => {}
            }
        }
    }
    tree.sum();
    Ok(tree)
}

fn mark_unreadable(tree: &mut Tree, id: NodeId) {
    tree.partial = true;
    tree.mark_unreadable(id);
}

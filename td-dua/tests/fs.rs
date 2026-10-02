//! The scan and the deletion against a real directory.
#![forbid(unsafe_code)]
#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

use std::fs;
use std::os::unix::fs::{symlink, MetadataExt};
use std::path::{Path, PathBuf};

use td_dua::delete::{self, Target};
use td_dua::scan::{scan, Progress};
use td_dua::tree::{Identity, Kind, NodeId, Tree, ROOT};

struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!("td-dua-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        Self(fs::canonicalize(path).unwrap())
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn child(tree: &Tree, parent: NodeId, name: &str) -> NodeId {
    *tree
        .get(parent)
        .unwrap()
        .children
        .iter()
        .find(|c| tree.get(**c).unwrap().name == name)
        .unwrap_or_else(|| panic!("no {name}"))
}

fn identity(path: &Path) -> Identity {
    let meta = fs::symlink_metadata(path).unwrap();
    Identity {
        device: meta.dev(),
        inode: meta.ino(),
    }
}

#[test]
fn scan_counts_hard_links_once_and_follows_no_link() {
    let scratch = Scratch::new("scan");
    let root = &scratch.0;
    let outside = Scratch::new("scan-outside");
    fs::write(outside.0.join("big"), vec![1u8; 64 * 1024]).unwrap();
    fs::create_dir(root.join("a")).unwrap();
    fs::write(root.join("a/one"), vec![1u8; 10_000]).unwrap();
    fs::hard_link(root.join("a/one"), root.join("two")).unwrap();
    symlink(&outside.0, root.join("link")).unwrap();
    let progress = Progress::default();
    let tree = scan(root, &progress).unwrap();
    let top = tree.root().unwrap();
    assert_eq!(tree.root_path(), root.as_path());
    assert_eq!(top.files, 3);
    assert!(!tree.partial);
    let a = child(&tree, ROOT, "a");
    let one = child(&tree, a, "one");
    let two = child(&tree, ROOT, "two");
    let link = child(&tree, ROOT, "link");
    assert_eq!(tree.get(link).unwrap().kind, Kind::Symlink);
    assert!(tree.get(link).unwrap().children.is_empty());
    // One of the two names owns the inode's bytes, the other none.
    let owned = [one, two].map(|id| tree.get(id).unwrap().own.apparent);
    assert_eq!(owned.iter().sum::<u64>(), 10_000);
    assert!(owned.contains(&0));
    // The link's target is not counted.
    assert!(top.total.apparent < 64 * 1024);
    assert_eq!(
        progress.entries.load(std::sync::atomic::Ordering::Relaxed),
        4
    );
    assert!(scan(&root.join("a/one"), &progress).is_err());
}

/// What the scan would have recorded of the entry at `path` now.
fn target(path: &Path) -> Target {
    let meta = fs::symlink_metadata(path).unwrap();
    Target {
        path: path.to_path_buf(),
        identity: identity(path),
        directory: meta.is_dir(),
        length: meta.len(),
        mtime: meta.mtime(),
    }
}

#[test]
fn delete_refuses_a_replaced_path() {
    let scratch = Scratch::new("replaced");
    let path = scratch.0.join("f");
    fs::write(&path, b"old").unwrap();
    let old = target(&path);
    // Another live inode takes the name, so the identities must differ.
    fs::write(scratch.0.join("other"), b"new").unwrap();
    fs::rename(scratch.0.join("other"), &path).unwrap();
    assert_ne!(identity(&path), old.identity);
    assert!(delete::delete(&old).is_err());
    assert_eq!(fs::read(&path).unwrap(), b"new");
    delete::delete(&target(&path)).unwrap();
    assert!(!path.exists());
}

#[test]
fn delete_refuses_a_changed_file_or_type() {
    let scratch = Scratch::new("changed");
    let path = scratch.0.join("f");
    fs::write(&path, b"12345").unwrap();
    let meta = fs::symlink_metadata(&path).unwrap();
    let seen = target(&path);
    // A reused inode number with other contents, or another type.
    let longer = Target {
        length: seen.length + 1,
        ..seen.clone()
    };
    let older = Target {
        mtime: seen.mtime - 1,
        ..seen.clone()
    };
    let directory = Target {
        directory: true,
        ..seen.clone()
    };
    assert_eq!(delete::mismatch(&longer, &meta), Some("changed"));
    assert_eq!(delete::mismatch(&older, &meta), Some("changed"));
    assert_eq!(delete::mismatch(&directory, &meta), Some("changed type"));
    assert_eq!(delete::mismatch(&seen, &meta), None);
    for wrong in [longer, older, directory] {
        assert!(delete::delete(&wrong).is_err());
        assert!(path.exists());
    }
    // A directory's time and length are not compared: its entries move them.
    let dir = scratch.0.join("d");
    fs::create_dir(&dir).unwrap();
    let seen = target(&dir);
    fs::write(dir.join("new"), b"x").unwrap();
    let meta = fs::symlink_metadata(&dir).unwrap();
    assert_eq!(delete::mismatch(&seen, &meta), None);
    delete::delete(&seen).unwrap();
    assert!(!dir.exists());
}

#[test]
fn delete_removes_a_tree_without_following_links() {
    let scratch = Scratch::new("tree");
    let outside = Scratch::new("tree-outside");
    fs::write(outside.0.join("precious"), b"keep").unwrap();
    let dir = scratch.0.join("d");
    fs::create_dir_all(dir.join("x/y")).unwrap();
    fs::write(dir.join("x/y/f"), b"1").unwrap();
    fs::write(dir.join("g"), b"2").unwrap();
    symlink(&outside.0, dir.join("x/out")).unwrap();
    delete::delete(&target(&dir)).unwrap();
    assert!(!dir.exists());
    assert!(outside.0.join("precious").exists());
}

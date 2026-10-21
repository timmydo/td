//! The `--rootdir` tree, read once into memory before anything is written.
//!
//! Directories, regular files and symbolic links are copied; any other file
//! type is refused, and extended attributes are not copied. A regular file
//! reached by more than one path inside one subvolume becomes one inode with
//! that many links; the source's own link count, which also counts links
//! outside the tree, is not used.

use std::ffi::OsStr;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Component, Path, PathBuf};

use crate::format::{invalid, Time, NAME_MAX};

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Meta {
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub atime: Time,
    pub ctime: Time,
    pub mtime: Time,
}

impl Meta {
    fn of(metadata: &fs::Metadata) -> Self {
        let time = |sec: i64, nsec: i64| Time {
            sec: sec as u64,
            nsec: nsec as u32,
        };
        Self {
            mode: metadata.mode(),
            uid: metadata.uid(),
            gid: metadata.gid(),
            atime: time(metadata.atime(), metadata.atime_nsec()),
            ctime: time(metadata.ctime(), metadata.ctime_nsec()),
            mtime: time(metadata.mtime(), metadata.mtime_nsec()),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Body {
    /// Children by name, sorted, as node indices.
    Dir(Vec<(Vec<u8>, usize)>),
    File {
        path: PathBuf,
        size: u64,
        id: (u64, u64),
    },
    Symlink(Vec<u8>),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Node {
    pub meta: Meta,
    pub body: Body,
    /// This directory becomes a subvolume.
    pub subvol: bool,
}

/// The tree, its root at index 0.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Source {
    pub nodes: Vec<Node>,
}

/// Longest symlink target Btrfs stores inline: Linux's PATH_MAX less its NUL.
const SYMLINK_MAX: usize = 4095;

impl Source {
    /// An empty root directory owned by root, for a filesystem made without
    /// `--rootdir`.
    pub fn empty(now: Time) -> Self {
        Self {
            nodes: vec![Node {
                meta: Meta {
                    mode: 0o040755,
                    atime: now,
                    ctime: now,
                    mtime: now,
                    ..Meta::default()
                },
                body: Body::Dir(Vec::new()),
                subvol: false,
            }],
        }
    }

    /// Read `root`'s tree. `exclude` is the target's (device, inode), which
    /// must not be copied into itself; `subvols` are directories, relative
    /// to `root`, that become subvolumes.
    pub fn scan(root: &Path, exclude: Option<(u64, u64)>, subvols: &[PathBuf]) -> io::Result<Self> {
        let metadata = fs::symlink_metadata(root).map_err(|error| {
            io::Error::new(error.kind(), format!("{}: {error}", root.display()))
        })?;
        if !metadata.is_dir() {
            return Err(invalid(format!("{} is not a directory", root.display())));
        }
        let mut source = Self {
            nodes: vec![Node {
                meta: Meta::of(&metadata),
                body: Body::Dir(Vec::new()),
                subvol: false,
            }],
        };
        let mut pending = vec![(0usize, root.to_path_buf())];
        while let Some((index, path)) = pending.pop() {
            let mut children = Vec::new();
            for entry in fs::read_dir(&path).map_err(|error| context(&path, error))? {
                let entry = entry.map_err(|error| context(&path, error))?;
                children.push((entry.file_name().as_bytes().to_vec(), entry.path()));
            }
            children.sort();
            let mut entries = Vec::with_capacity(children.len());
            for (name, child) in children {
                if name.len() > NAME_MAX {
                    return Err(invalid(format!(
                        "{}: names longer than {NAME_MAX} bytes do not fit Btrfs",
                        child.display()
                    )));
                }
                let metadata =
                    fs::symlink_metadata(&child).map_err(|error| context(&child, error))?;
                let kind = metadata.file_type();
                let body = if kind.is_dir() {
                    pending.push((source.nodes.len(), child.clone()));
                    Body::Dir(Vec::new())
                } else if kind.is_file() {
                    let id = (metadata.dev(), metadata.ino());
                    if Some(id) == exclude {
                        return Err(invalid(format!("{} is the target itself", child.display())));
                    }
                    Body::File {
                        path: child.clone(),
                        size: metadata.len(),
                        id,
                    }
                } else if kind.is_symlink() {
                    let target = fs::read_link(&child).map_err(|error| context(&child, error))?;
                    let target = target.as_os_str().as_bytes().to_vec();
                    if target.len() > SYMLINK_MAX {
                        return Err(invalid(format!(
                            "{}: symlink target too long",
                            child.display()
                        )));
                    }
                    Body::Symlink(target)
                } else {
                    let what = if kind.is_block_device() || kind.is_char_device() {
                        "device node"
                    } else if kind.is_fifo() {
                        "FIFO"
                    } else if kind.is_socket() {
                        "socket"
                    } else {
                        "special file"
                    };
                    return Err(invalid(format!(
                        "{}: a {what} cannot be copied; only directories, regular files and symlinks are",
                        child.display()
                    )));
                };
                entries.push((name, source.nodes.len()));
                source.nodes.push(Node {
                    meta: Meta::of(&metadata),
                    body,
                    subvol: false,
                });
            }
            if let Some(Node {
                body: Body::Dir(slot),
                ..
            }) = source.nodes.get_mut(index)
            {
                *slot = entries;
            }
        }
        for subvol in subvols {
            let index = source.lookup(subvol)?;
            match source.nodes.get_mut(index) {
                Some(node) if index != 0 && matches!(node.body, Body::Dir(_)) => {
                    node.subvol = true;
                }
                _ => {
                    return Err(invalid(format!(
                        "subvolume {} must be a directory below the root",
                        subvol.display()
                    )))
                }
            }
        }
        Ok(source)
    }

    fn lookup(&self, relative: &Path) -> io::Result<usize> {
        let mut index = 0;
        for component in relative.components() {
            let Component::Normal(name) = component else {
                return Err(invalid(format!(
                    "subvolume {} must be a plain relative path",
                    relative.display()
                )));
            };
            index = self.child(index, name).ok_or_else(|| {
                invalid(format!(
                    "subvolume {} is not in the root directory",
                    relative.display()
                ))
            })?;
        }
        Ok(index)
    }

    fn child(&self, index: usize, name: &OsStr) -> Option<usize> {
        match &self.nodes.get(index)?.body {
            Body::Dir(entries) => entries
                .iter()
                .find(|(entry, _)| entry.as_slice() == name.as_bytes())
                .map(|(_, child)| *child),
            _ => None,
        }
    }
}

fn context(path: &Path, error: io::Error) -> io::Error {
    io::Error::new(error.kind(), format!("{}: {error}", path.display()))
}

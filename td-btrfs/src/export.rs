//! Listing and restoring a filesystem's contents, across its subvolumes.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use crate::format::*;
use crate::fstree::{FileExtent, FsTree, InodeRec};
use crate::read::{roots, Visits, Volume};

/// One path in the filesystem.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Entry {
    /// `/`-separated from the top-level subvolume, empty for its root.
    pub path: Vec<u8>,
    pub tree: u64,
    pub ino: u64,
    pub inode: Inode,
    /// Set for a subvolume's root directory: its id and whether read-only.
    pub subvol: Option<(u64, bool)>,
    pub target: Option<Vec<u8>>,
}

/// A path still to visit: its tree, inode, path and subvolume.
type Pending = (u64, u64, Vec<u8>, Option<(u64, bool)>);

pub struct Contents {
    volume: Volume,
    trees: BTreeMap<u64, FsTree>,
    pub entries: Vec<Entry>,
}

impl Contents {
    pub fn read(volume: Volume) -> io::Result<Self> {
        let sb = volume.sb.clone();
        let mut visited = Visits::default();
        let root_items = volume.walk(
            sb.root,
            sb.root_level,
            ROOT_TREE,
            Some(sb.generation),
            &mut visited,
        )?;
        let mut records = BTreeMap::new();
        let mut trees = BTreeMap::new();
        for (id, root) in roots(&root_items)? {
            if id == FS_TREE || (FIRST_FREE..=LAST_FREE).contains(&id) {
                let items = volume.walk(
                    root.bytenr,
                    root.level,
                    id,
                    Some(root.generation),
                    &mut visited,
                )?;
                trees.insert(id, FsTree::parse(&items)?);
                records.insert(id, root);
            }
        }
        let mut entries = Vec::new();
        // Each directory is entered once, so a directory entry naming an
        // ancestor cannot make the walk loop.
        let mut entered = HashSet::new();
        // (tree, inode, path, subvolume root), deepest first.
        let mut pending: Vec<Pending> =
            vec![(FS_TREE, FIRST_FREE, Vec::new(), Some((FS_TREE, false)))];
        while let Some((tree_id, ino, path, subvol)) = pending.pop() {
            let tree = trees
                .get(&tree_id)
                .ok_or_else(|| invalid(format!("subvolume {tree_id} is missing")))?;
            let rec = tree
                .inodes
                .get(&ino)
                .ok_or_else(|| invalid(format!("inode {ino} of tree {tree_id} is missing")))?;
            let inode = rec.inode.clone().ok_or_else(|| {
                invalid(format!("inode {ino} of tree {tree_id} has no inode item"))
            })?;
            let target = if inode.mode & S_IFMT == S_IFLNK {
                let mut target = inline(rec)?;
                target.truncate(inode.size as usize);
                Some(target)
            } else {
                None
            };
            entries.push(Entry {
                path: path.clone(),
                tree: tree_id,
                ino,
                inode: inode.clone(),
                subvol,
                target,
            });
            if inode.mode & S_IFMT != S_IFDIR {
                continue;
            }
            if !entered.insert((tree_id, ino)) {
                return Err(invalid(format!(
                    "directory {ino} of tree {tree_id} is reached twice"
                )));
            }
            let mut children: Vec<_> = rec.index.values().collect();
            children.sort_by(|a, b| b.name.cmp(&a.name));
            for child in children {
                let mut child_path = path.clone();
                if !child_path.is_empty() {
                    child_path.push(b'/');
                }
                child_path.extend_from_slice(&child.name);
                match child.location.kind {
                    INODE_ITEM => {
                        pending.push((tree_id, child.location.objectid, child_path, None))
                    }
                    ROOT_ITEM => {
                        let id = child.location.objectid;
                        let root = records
                            .get(&id)
                            .ok_or_else(|| invalid(format!("subvolume {id} is missing")))?;
                        pending.push((
                            id,
                            root.root_dirid,
                            child_path,
                            Some((id, root.flags & ROOT_SUBVOL_RDONLY != 0)),
                        ));
                    }
                    _ => return Err(invalid("directory entry with an unknown location")),
                }
            }
        }
        Ok(Self {
            volume,
            trees,
            entries,
        })
    }

    fn record(&self, entry: &Entry) -> io::Result<&InodeRec> {
        self.trees
            .get(&entry.tree)
            .and_then(|tree| tree.inodes.get(&entry.ino))
            .ok_or_else(|| invalid("inode vanished"))
    }

    fn copy_to(&self, entry: &Entry, path: &Path) -> io::Result<()> {
        use std::os::unix::fs::FileExt;
        let file = fs::File::create_new(path)?;
        file.set_len(entry.inode.size)?;
        for extent in &self.record(entry)?.extents {
            pieces(&self.volume, extent, entry.inode.size, &mut |at, bytes| {
                file.write_all_at(bytes, at)
            })?;
        }
        Ok(())
    }

    /// One line per path: type, mode, owner, size and path, with a
    /// symlink's target and a subvolume's id and writability.
    pub fn list(&self, out: &mut dyn Write) -> io::Result<()> {
        for entry in &self.entries {
            let inode = &entry.inode;
            let kind = match inode.mode & S_IFMT {
                S_IFDIR => 'd',
                S_IFREG => 'f',
                S_IFLNK => 'l',
                _ => '?',
            };
            let path = String::from_utf8_lossy(&entry.path);
            write!(
                out,
                "{kind} {:04o} {} {} {} /{path}",
                inode.mode & 0o7777,
                inode.uid,
                inode.gid,
                inode.size
            )?;
            if let Some(target) = &entry.target {
                write!(out, " -> {}", String::from_utf8_lossy(target))?;
            }
            if let Some((id, rdonly)) = entry.subvol {
                write!(
                    out,
                    " [subvolume {id} {}]",
                    if rdonly { "ro" } else { "rw" }
                )?;
            }
            writeln!(out)?;
        }
        Ok(())
    }

    /// Recreate the tree under `dest`, which must be an empty directory:
    /// directories, file contents and symlinks, without owners or modes.
    pub fn restore(&self, dest: &Path) -> io::Result<()> {
        if fs::read_dir(dest)?.next().is_some() {
            return Err(invalid(format!("{} is not empty", dest.display())));
        }
        for entry in &self.entries {
            if entry.path.is_empty() {
                continue;
            }
            let path: PathBuf = dest.join(std::ffi::OsStr::from_bytes(&entry.path));
            match entry.inode.mode & S_IFMT {
                S_IFDIR => fs::create_dir(&path)?,
                S_IFLNK => std::os::unix::fs::symlink(
                    std::ffi::OsStr::from_bytes(entry.target.as_deref().unwrap_or_default()),
                    &path,
                )?,
                S_IFREG => self.copy_to(entry, &path)?,
                _ => {
                    return Err(invalid(format!(
                        "{} is not a file td-btrfs restores",
                        path.display()
                    )))
                }
            }
        }
        Ok(())
    }
}

fn inline(rec: &InodeRec) -> io::Result<Vec<u8>> {
    match rec.extents.as_slice() {
        [one] if one.kind == FILE_EXTENT_INLINE && one.compression == 0 => Ok(one.inline.clone()),
        _ => Err(invalid("symlink without an inline target")),
    }
}

/// Hand `extent`'s bytes within a file of `size` to `sink`, by file offset,
/// a megabyte at a time. Holes and preallocated ranges are left alone.
fn pieces(
    volume: &Volume,
    extent: &FileExtent,
    size: u64,
    sink: &mut dyn FnMut(u64, &[u8]) -> io::Result<()>,
) -> io::Result<()> {
    if extent.compression != 0 || extent.encryption != 0 {
        return Err(invalid("compressed extents are not supported"));
    }
    let start = extent.file_offset;
    if extent.kind == FILE_EXTENT_INLINE {
        let len = (extent.inline.len() as u64).min(size.saturating_sub(start));
        return sink(start, extent.inline.get(..len as usize).unwrap_or_default());
    }
    if extent.kind != FILE_EXTENT_REG || extent.disk_bytenr == 0 {
        return Ok(());
    }
    let len = extent.num_bytes.min(size.saturating_sub(start));
    let mut buffer = vec![0u8; PIECE.min(len as usize)];
    let mut done = 0;
    while done < len {
        let piece = (len - done).min(PIECE as u64) as usize;
        let chunk = buffer
            .get_mut(..piece)
            .ok_or_else(|| invalid("short buffer"))?;
        let at = extent
            .disk_bytenr
            .saturating_add(extent.offset)
            .saturating_add(done);
        volume.read_into(at, chunk)?;
        sink(start + done, chunk)?;
        done += piece as u64;
    }
    Ok(())
}

const PIECE: usize = 1 << 20;

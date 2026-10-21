//! Subvolume trees: inode numbers, directory entries and file extents for
//! a scanned source.
//!
//! A directory's entries are numbered together in name order, then its
//! subdirectories are expanded depth first, so a tree's items depend only on
//! its source. Subvolumes are numbered from 256 in the order they are found,
//! each after the tree holding it.

use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::{self, Read};
use std::path::PathBuf;

use crate::crc32c::name_hash;
use crate::format::{
    dir_item, file_extent_inline, file_extent_reg, inode_ref, invalid, Inode, Item, Key, Time,
    DIR_INDEX, DIR_ITEM, EXTENT_DATA, FIRST_FREE, FS_TREE, FT_DIR, FT_REG_FILE, FT_SYMLINK,
    INODE_ITEM, INODE_REF, LAST_FREE, MAX_INLINE, ROOT_ITEM, SECTORSIZE, S_IFDIR, S_IFLNK, S_IFMT,
    S_IFREG,
};
use crate::source::{Body, Source};

/// A parent's entry for one child.
#[derive(Clone, Debug, Eq, PartialEq)]
struct Entry {
    name: Vec<u8>,
    index: u64,
    target: Target,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Target {
    Inode { ino: u64, kind: u8 },
    Subvol(u64),
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct InodePlan {
    ino: u64,
    node: usize,
    /// (parent inode, index, name) for each link.
    refs: Vec<(u64, u64, Vec<u8>)>,
    entries: Vec<Entry>,
}

/// Where a subvolume hangs: its parent tree, the directory holding it, the
/// entry's index and name.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Parent {
    pub tree: u64,
    pub dirid: u64,
    pub index: u64,
    pub name: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Tree {
    pub id: u64,
    pub parent: Option<Parent>,
    inodes: Vec<InodePlan>,
}

/// A regular file whose data lives in extents, in placement order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileData {
    pub tree: u64,
    pub ino: u64,
    pub path: PathBuf,
    pub size: u64,
}

/// One data extent of one file.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Extent {
    pub tree: u64,
    pub ino: u64,
    pub file_offset: u64,
    pub logical: u64,
    pub len: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Fs {
    pub trees: Vec<Tree>,
    pub files: Vec<FileData>,
}

fn kind_of(body: &Body) -> u8 {
    match body {
        Body::Dir(_) => FT_DIR,
        Body::File { .. } => FT_REG_FILE,
        Body::Symlink(_) => FT_SYMLINK,
    }
}

fn inline(size: u64) -> bool {
    size > 0 && size <= MAX_INLINE
}

impl Fs {
    pub fn plan(source: &Source) -> io::Result<Self> {
        let mut trees = Vec::new();
        let mut files = Vec::new();
        // (root node, tree id, parent); subvolumes found in one tree are
        // planned after it, in the order found.
        let mut queue = vec![(0usize, FS_TREE, None)];
        let mut next_subvol = FIRST_FREE;
        let mut at = 0;
        while let Some((root, id, parent)) = queue.get(at).cloned() {
            at += 1;
            let mut inodes: Vec<InodePlan> = vec![InodePlan {
                ino: FIRST_FREE,
                node: root,
                refs: vec![(FIRST_FREE, 0, b"..".to_vec())],
                entries: Vec::new(),
            }];
            let mut next_ino = FIRST_FREE + 1;
            let mut links: HashMap<(u64, u64), usize> = HashMap::new();
            // (inode plan slot, source node) for each directory to expand.
            let mut pending = vec![0usize];
            while let Some(slot) = pending.pop() {
                let (dir_ino, node) = match inodes.get(slot) {
                    Some(plan) => (plan.ino, plan.node),
                    None => continue,
                };
                let children = match source.nodes.get(node).map(|node| &node.body) {
                    Some(Body::Dir(children)) => children.clone(),
                    _ => continue,
                };
                let mut entries = Vec::with_capacity(children.len());
                let mut expand = Vec::new();
                for (offset, (name, child)) in children.iter().enumerate() {
                    let index = 2 + offset as u64;
                    let child_node = source
                        .nodes
                        .get(*child)
                        .ok_or_else(|| invalid("source child out of range"))?;
                    if child_node.subvol {
                        let subvol = next_subvol;
                        next_subvol += 1;
                        queue.push((
                            *child,
                            subvol,
                            Some(Parent {
                                tree: id,
                                dirid: dir_ino,
                                index,
                                name: name.clone(),
                            }),
                        ));
                        entries.push(Entry {
                            name: name.clone(),
                            index,
                            target: Target::Subvol(subvol),
                        });
                        continue;
                    }
                    let kind = kind_of(&child_node.body);
                    let link = match &child_node.body {
                        Body::File { id, .. } => links.get(id).copied().map(|slot| (*id, slot)),
                        _ => None,
                    };
                    let ino = if let Some((_, existing)) = link {
                        let plan = inodes
                            .get_mut(existing)
                            .ok_or_else(|| invalid("link slot out of range"))?;
                        plan.refs.push((dir_ino, index, name.clone()));
                        plan.ino
                    } else {
                        let ino = next_ino;
                        if ino > LAST_FREE {
                            return Err(invalid("too many inodes"));
                        }
                        next_ino += 1;
                        if let Body::File {
                            id: file,
                            path,
                            size,
                        } = &child_node.body
                        {
                            links.insert(*file, inodes.len());
                            if !inline(*size) && *size > 0 {
                                files.push(FileData {
                                    tree: id,
                                    ino,
                                    path: path.clone(),
                                    size: *size,
                                });
                            }
                        }
                        if kind == FT_DIR {
                            expand.push(inodes.len());
                        }
                        inodes.push(InodePlan {
                            ino,
                            node: *child,
                            refs: vec![(dir_ino, index, name.clone())],
                            entries: Vec::new(),
                        });
                        ino
                    };
                    entries.push(Entry {
                        name: name.clone(),
                        index,
                        target: Target::Inode { ino, kind },
                    });
                }
                if let Some(plan) = inodes.get_mut(slot) {
                    plan.entries = entries;
                }
                // Reverse, so the first-named directory is expanded first.
                pending.extend(expand.into_iter().rev());
            }
            trees.push(Tree { id, parent, inodes });
        }
        Ok(Self { trees, files })
    }

    /// Bytes of data the regular extents need.
    pub fn data_bytes(&self) -> u64 {
        self.files
            .iter()
            .map(|file| file.size.div_ceil(u64::from(SECTORSIZE)) * u64::from(SECTORSIZE))
            .sum()
    }

    /// Lay the files' data out over `chunks`, each `(logical, length)`, in
    /// order, splitting at `max_extent` and at chunk ends.
    pub fn place(&self, chunks: &[(u64, u64)], max_extent: u64) -> io::Result<Vec<Extent>> {
        let mut out = Vec::new();
        let mut chunk = 0;
        let mut used = 0;
        for file in &self.files {
            let mut remaining = file.size.div_ceil(u64::from(SECTORSIZE)) * u64::from(SECTORSIZE);
            let mut file_offset = 0;
            while remaining > 0 {
                let &(start, length) = chunks
                    .get(chunk)
                    .ok_or_else(|| invalid("the data chunks are too small"))?;
                if used == length {
                    chunk += 1;
                    used = 0;
                    continue;
                }
                let len = remaining.min(max_extent).min(length - used);
                out.push(Extent {
                    tree: file.tree,
                    ino: file.ino,
                    file_offset,
                    logical: start + used,
                    len,
                });
                used += len;
                file_offset += len;
                remaining -= len;
            }
        }
        Ok(out)
    }

    /// The items of every tree, by tree id.
    pub fn items(
        &self,
        source: &Source,
        extents: &[Extent],
        generation: u64,
    ) -> io::Result<BTreeMap<u64, Vec<Item>>> {
        let mut by_inode: HashMap<(u64, u64), Vec<&Extent>> = HashMap::new();
        for extent in extents {
            by_inode
                .entry((extent.tree, extent.ino))
                .or_default()
                .push(extent);
        }
        let mut out = BTreeMap::new();
        for tree in &self.trees {
            let mut items = Vec::new();
            for plan in &tree.inodes {
                let node = source
                    .nodes
                    .get(plan.node)
                    .ok_or_else(|| invalid("source node out of range"))?;
                let meta = &node.meta;
                let (format_bits, size, nbytes) = match &node.body {
                    Body::Dir(_) => {
                        let names: u64 = plan.entries.iter().map(|e| e.name.len() as u64).sum();
                        (S_IFDIR, names * 2, 0)
                    }
                    Body::File { size, .. } => {
                        let nbytes = if inline(*size) {
                            *size
                        } else {
                            size.div_ceil(u64::from(SECTORSIZE)) * u64::from(SECTORSIZE)
                        };
                        (S_IFREG, *size, nbytes)
                    }
                    Body::Symlink(target) => (S_IFLNK, target.len() as u64, target.len() as u64),
                };
                let inode = Inode {
                    generation,
                    transid: generation,
                    size,
                    nbytes,
                    nlink: if format_bits == S_IFDIR {
                        1
                    } else {
                        plan.refs.len() as u32
                    },
                    uid: meta.uid,
                    gid: meta.gid,
                    mode: (meta.mode & !S_IFMT) | format_bits,
                    rdev: 0,
                    flags: 0,
                    atime: meta.atime,
                    ctime: meta.ctime,
                    mtime: meta.mtime,
                    otime: Time::default(),
                };
                let mut data = Vec::new();
                inode.encode(&mut data);
                items.push(Item::new(Key::new(plan.ino, INODE_ITEM, 0), data));
                let mut refs: BTreeMap<u64, Vec<u8>> = BTreeMap::new();
                for (parent, index, name) in &plan.refs {
                    inode_ref(*index, name, refs.entry(*parent).or_default());
                }
                for (parent, data) in refs {
                    items.push(Item::new(Key::new(plan.ino, INODE_REF, parent), data));
                }
                let mut hashed: BTreeMap<u64, Vec<u8>> = BTreeMap::new();
                for entry in &plan.entries {
                    let (location, kind) = match entry.target {
                        Target::Inode { ino, kind } => (Key::new(ino, INODE_ITEM, 0), kind),
                        Target::Subvol(id) => (Key::new(id, ROOT_ITEM, 0), FT_DIR),
                    };
                    let mut one = Vec::new();
                    dir_item(location, generation, kind, &entry.name, &mut one);
                    hashed
                        .entry(name_hash(&entry.name))
                        .or_default()
                        .extend_from_slice(&one);
                    items.push(Item::new(Key::new(plan.ino, DIR_INDEX, entry.index), one));
                }
                for (hash, data) in hashed {
                    items.push(Item::new(Key::new(plan.ino, DIR_ITEM, hash), data));
                }
                match &node.body {
                    Body::File { path, size, .. } if inline(*size) => {
                        let content = read_exactly(path, *size)?;
                        items.push(Item::new(
                            Key::new(plan.ino, EXTENT_DATA, 0),
                            file_extent_inline(generation, &content),
                        ));
                    }
                    Body::File { .. } => {
                        for extent in by_inode.get(&(tree.id, plan.ino)).into_iter().flatten() {
                            items.push(Item::new(
                                Key::new(plan.ino, EXTENT_DATA, extent.file_offset),
                                file_extent_reg(generation, extent.logical, extent.len),
                            ));
                        }
                    }
                    Body::Symlink(target) => items.push(Item::new(
                        Key::new(plan.ino, EXTENT_DATA, 0),
                        file_extent_inline(generation, target),
                    )),
                    Body::Dir(_) => {}
                }
            }
            items.sort_by_key(|item| item.key);
            out.insert(tree.id, items);
        }
        Ok(out)
    }
}

/// Read a whole small file, refusing one whose size changed since the scan.
pub fn read_exactly(path: &std::path::Path, size: u64) -> io::Result<Vec<u8>> {
    let mut content = Vec::with_capacity(size as usize);
    File::open(path)
        .and_then(|file| file.take(size + 1).read_to_end(&mut content))
        .map_err(|error| io::Error::new(error.kind(), format!("{}: {error}", path.display())))?;
    if content.len() as u64 != size {
        return Err(invalid(format!(
            "{} changed size while it was being copied",
            path.display()
        )));
    }
    Ok(content)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn file(ino: u64, size: u64) -> FileData {
        FileData {
            tree: FS_TREE,
            ino,
            path: PathBuf::new(),
            size,
        }
    }

    #[test]
    fn extents_split_at_the_cap_and_at_chunk_ends() {
        let fs = Fs {
            trees: Vec::new(),
            files: vec![file(257, 10 * 4096 - 1), file(258, 3 * 4096)],
        };
        assert_eq!(fs.data_bytes(), 13 * 4096);
        let extents = fs
            .place(&[(1 << 20, 6 * 4096), (8 << 20, 16 * 4096)], 4 * 4096)
            .unwrap();
        let shape: Vec<_> = extents
            .iter()
            .map(|e| (e.ino, e.file_offset, e.logical, e.len))
            .collect();
        assert_eq!(
            shape,
            [
                (257, 0, 1 << 20, 4 * 4096),
                (257, 4 * 4096, (1 << 20) + 4 * 4096, 2 * 4096),
                (257, 6 * 4096, 8 << 20, 4 * 4096),
                (258, 0, (8 << 20) + 4 * 4096, 3 * 4096),
            ]
        );
        assert!(fs.place(&[(1 << 20, 12 * 4096)], 1 << 30).is_err());
    }
}

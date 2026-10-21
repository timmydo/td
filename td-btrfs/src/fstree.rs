//! One subvolume's items, grouped by inode, as check, restore and list
//! read them.

use std::collections::BTreeMap;
use std::io;

use crate::format::*;
use crate::read::ReadItem;

pub const ORPHAN: u64 = -5i64 as u64;
pub const ORPHAN_ITEM: u8 = 48;
pub const INODE_NODATASUM: u64 = 1 << 0;
pub const FILE_EXTENT_PREALLOC: u8 = 2;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DirEntry {
    pub location: Key,
    pub kind: u8,
    pub name: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileExtent {
    /// The key offset: where in the file this extent begins.
    pub file_offset: u64,
    pub kind: u8,
    pub compression: u8,
    pub encryption: u8,
    pub other_encoding: u16,
    pub ram_bytes: u64,
    pub inline: Vec<u8>,
    pub disk_bytenr: u64,
    pub disk_num_bytes: u64,
    pub offset: u64,
    pub num_bytes: u64,
    /// The leaf the item is in.
    pub leaf: u64,
}

impl FileExtent {
    fn decode(file_offset: u64, data: &[u8], leaf: u64) -> io::Result<Self> {
        let kind = get8(data, 20)?;
        let mut extent = Self {
            file_offset,
            kind,
            compression: get8(data, 16)?,
            encryption: get8(data, 17)?,
            other_encoding: get16(data, 18)?,
            ram_bytes: get64(data, 8)?,
            inline: Vec::new(),
            disk_bytenr: 0,
            disk_num_bytes: 0,
            offset: 0,
            num_bytes: 0,
            leaf,
        };
        match kind {
            FILE_EXTENT_INLINE => extent.inline = data.get(21..).unwrap_or_default().to_vec(),
            FILE_EXTENT_REG | FILE_EXTENT_PREALLOC => {
                if data.len() != 53 {
                    return Err(invalid("file extent item has the wrong size"));
                }
                extent.disk_bytenr = get64(data, 21)?;
                extent.disk_num_bytes = get64(data, 29)?;
                extent.offset = get64(data, 37)?;
                extent.num_bytes = get64(data, 45)?;
            }
            other => return Err(invalid(format!("unknown file extent type {other}"))),
        }
        Ok(extent)
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct InodeRec {
    pub inode: Option<Inode>,
    /// (parent, index, name) of every link, from INODE_REF and INODE_EXTREF.
    pub refs: Vec<(u64, u64, Vec<u8>)>,
    /// DIR_INDEX entries by index.
    pub index: BTreeMap<u64, DirEntry>,
    /// DIR_ITEM entries with their key offsets.
    pub hashed: Vec<(u64, DirEntry)>,
    pub extents: Vec<FileExtent>,
    pub xattrs: usize,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct FsTree {
    pub inodes: BTreeMap<u64, InodeRec>,
    pub orphans: Vec<u64>,
    /// Keys of items this reader does not interpret.
    pub unknown: Vec<Key>,
}

/// Decode the `btrfs_dir_item` entries packed in one item.
pub fn dir_entries(data: &[u8]) -> io::Result<Vec<DirEntry>> {
    let mut out = Vec::new();
    let mut at = 0;
    while at < data.len() {
        let location = Key::decode(data, at)?;
        let data_len = usize::from(get16(data, at + 25)?);
        let name_len = usize::from(get16(data, at + 27)?);
        let kind = get8(data, at + 29)?;
        let name = slice(data, at + 30, name_len)?.to_vec();
        if name.is_empty() || name_len > NAME_MAX {
            return Err(invalid("directory entry with an invalid name length"));
        }
        at += 30 + name_len + data_len;
        if at > data.len() {
            return Err(invalid("directory entry overruns its item"));
        }
        out.push(DirEntry {
            location,
            kind,
            name,
        });
    }
    Ok(out)
}

/// A directory entry's name is one path component.
fn component(entry: DirEntry) -> io::Result<DirEntry> {
    if entry.name.contains(&b'/')
        || entry.name.contains(&0)
        || entry.name == b"."
        || entry.name == b".."
    {
        return Err(invalid(
            "directory entry whose name is not a path component",
        ));
    }
    Ok(entry)
}

impl FsTree {
    pub fn parse(items: &[ReadItem]) -> io::Result<Self> {
        let mut tree = Self::default();
        for item in items {
            let key = item.key;
            if key.objectid == ORPHAN && key.kind == ORPHAN_ITEM {
                tree.orphans.push(key.offset);
                continue;
            }
            let entry = tree.inodes.entry(key.objectid).or_default();
            let data = &item.data;
            let what = |message: &str| invalid(format!("item {key:?}: {message}"));
            match key.kind {
                INODE_ITEM => entry.inode = Some(Inode::decode(data)?),
                INODE_REF => {
                    let mut at = 0;
                    while at < data.len() {
                        let index = get64(data, at)?;
                        let len = usize::from(get16(data, at + 8)?);
                        entry
                            .refs
                            .push((key.offset, index, slice(data, at + 10, len)?.to_vec()));
                        at += 10 + len;
                    }
                }
                INODE_EXTREF => {
                    let mut at = 0;
                    while at < data.len() {
                        let parent = get64(data, at)?;
                        let index = get64(data, at + 8)?;
                        let len = usize::from(get16(data, at + 16)?);
                        entry
                            .refs
                            .push((parent, index, slice(data, at + 18, len)?.to_vec()));
                        at += 18 + len;
                    }
                }
                DIR_INDEX => {
                    let mut entries = dir_entries(data)?;
                    if entries.len() != 1 {
                        return Err(what("a DIR_INDEX holds one entry"));
                    }
                    if let Some(one) = entries.pop() {
                        entry.index.insert(key.offset, component(one)?);
                    }
                }
                DIR_ITEM => {
                    for one in dir_entries(data)? {
                        entry.hashed.push((key.offset, component(one)?));
                    }
                }
                XATTR_ITEM => entry.xattrs += dir_entries(data)?.len(),
                EXTENT_DATA => entry
                    .extents
                    .push(FileExtent::decode(key.offset, data, item.leaf)?),
                _ => tree.unknown.push(key),
            }
        }
        tree.inodes.retain(|_, rec| *rec != InodeRec::default());
        Ok(tree)
    }
}

/// The directory-entry type a mode implies.
pub fn mode_kind(mode: u32) -> u8 {
    match mode & S_IFMT {
        S_IFREG => FT_REG_FILE,
        S_IFDIR => FT_DIR,
        0o020000 => 3, // CHRDEV
        0o060000 => 4, // BLKDEV
        0o010000 => 5, // FIFO
        0o140000 => 6, // SOCK
        S_IFLNK => FT_SYMLINK,
        _ => 0,
    }
}

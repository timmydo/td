//! Btrfs on-disk constants and the little-endian item encodings td writes
//! and reads. Layouts follow btrfs-progs v7.0
//! `kernel-shared/uapi/btrfs_tree.h`.

use std::io;

pub const NODESIZE: u32 = 16384;
pub const SECTORSIZE: u32 = 4096;
pub const STRIPE_LEN: u64 = 65536;
pub const SUPER_OFFSET: u64 = 65536;
pub const SUPER_SIZE: usize = 4096;
pub const SUPER_MAGIC: &[u8; 8] = b"_BHRfS_M";
pub const MIB: u64 = 1 << 20;
/// Superblock copies, each written where the device holds it whole.
pub const SUPER_MIRRORS: &[u64] = &[SUPER_OFFSET, 64 * MIB, 256 << 30];
/// The kernel excludes one stripe from allocation at each copy.
pub const SUPER_EXCLUDE: u64 = STRIPE_LEN;
pub const HEADER_SIZE: usize = 101;
pub const ITEM_SIZE: usize = 25;
pub const KEY_PTR_SIZE: usize = 33;
pub const LEAF_DATA: usize = NODESIZE as usize - HEADER_SIZE;
pub const NODE_PTRS: usize = LEAF_DATA / KEY_PTR_SIZE;
/// `BTRFS_MAX_EXTENT_SIZE`: the longest uncompressed data extent.
pub const MAX_EXTENT: u64 = 128 * MIB;
/// `BTRFS_DEFAULT_MAX_INLINE`.
pub const MAX_INLINE: u64 = 2048;
pub const CSUM_SIZE: usize = 4;
/// `MAX_CSUM_ITEMS`: checksums one EXTENT_CSUM item may hold.
pub const MAX_CSUMS: usize = (LEAF_DATA - 2 * ITEM_SIZE) / CSUM_SIZE - 1;
pub const MAX_LEVEL: u8 = 7;
pub const NAME_MAX: usize = 255;

// Tree and special objectids.
pub const ROOT_TREE: u64 = 1;
pub const EXTENT_TREE: u64 = 2;
pub const CHUNK_TREE: u64 = 3;
pub const DEV_TREE: u64 = 4;
pub const FS_TREE: u64 = 5;
pub const ROOT_TREE_DIR: u64 = 6;
pub const CSUM_TREE: u64 = 7;
pub const UUID_TREE: u64 = 9;
pub const FREE_SPACE_TREE: u64 = 10;
pub const BLOCK_GROUP_TREE: u64 = 11;
pub const DEV_STATS: u64 = 0;
pub const DEV_ITEMS: u64 = 1;
pub const FIRST_CHUNK_TREE: u64 = 256;
pub const FIRST_FREE: u64 = 256;
pub const LAST_FREE: u64 = u64::MAX - 256;
pub const DATA_RELOC_TREE: u64 = -9i64 as u64;
pub const EXTENT_CSUM: u64 = -10i64 as u64;

// Key types.
pub const INODE_ITEM: u8 = 1;
pub const INODE_REF: u8 = 12;
pub const INODE_EXTREF: u8 = 13;
pub const XATTR_ITEM: u8 = 24;
pub const DIR_ITEM: u8 = 84;
pub const DIR_INDEX: u8 = 96;
pub const EXTENT_DATA: u8 = 108;
pub const EXTENT_CSUM_KEY: u8 = 128;
pub const ROOT_ITEM: u8 = 132;
pub const ROOT_BACKREF: u8 = 144;
pub const ROOT_REF: u8 = 156;
pub const EXTENT_ITEM: u8 = 168;
pub const METADATA_ITEM: u8 = 169;
pub const TREE_BLOCK_REF: u8 = 176;
pub const EXTENT_DATA_REF: u8 = 178;
pub const BLOCK_GROUP_ITEM: u8 = 192;
pub const FREE_SPACE_INFO: u8 = 198;
pub const FREE_SPACE_EXTENT: u8 = 199;
pub const FREE_SPACE_BITMAP: u8 = 200;
pub const DEV_EXTENT: u8 = 204;
pub const DEV_ITEM: u8 = 216;
pub const CHUNK_ITEM: u8 = 228;
pub const PERSISTENT_ITEM: u8 = 249;
pub const UUID_KEY_SUBVOL: u8 = 251;

// Block group profile and type bits.
pub const BG_DATA: u64 = 1 << 0;
pub const BG_SYSTEM: u64 = 1 << 1;
pub const BG_METADATA: u64 = 1 << 2;
pub const BG_DUP: u64 = 1 << 5;

// Extent flags.
pub const EXTENT_FLAG_DATA: u64 = 1 << 0;
pub const EXTENT_FLAG_TREE_BLOCK: u64 = 1 << 1;

// Feature flags td writes: btrfs-progs v7.0's defaults.
pub const COMPAT_RO_FREE_SPACE_TREE: u64 = 1 << 0;
pub const COMPAT_RO_FREE_SPACE_TREE_VALID: u64 = 1 << 1;
pub const COMPAT_RO_BLOCK_GROUP_TREE: u64 = 1 << 3;
pub const COMPAT_RO: u64 =
    COMPAT_RO_FREE_SPACE_TREE | COMPAT_RO_FREE_SPACE_TREE_VALID | COMPAT_RO_BLOCK_GROUP_TREE;
pub const INCOMPAT_MIXED_BACKREF: u64 = 1 << 0;
pub const INCOMPAT_BIG_METADATA: u64 = 1 << 5;
pub const INCOMPAT_EXTENDED_IREF: u64 = 1 << 6;
pub const INCOMPAT_SKINNY_METADATA: u64 = 1 << 8;
pub const INCOMPAT_NO_HOLES: u64 = 1 << 9;
pub const INCOMPAT: u64 = INCOMPAT_MIXED_BACKREF
    | INCOMPAT_BIG_METADATA
    | INCOMPAT_EXTENDED_IREF
    | INCOMPAT_SKINNY_METADATA
    | INCOMPAT_NO_HOLES;

pub const HEADER_FLAG_WRITTEN: u64 = 1;
pub const MIXED_BACKREF_REV: u64 = 1;
pub const BLOCK_FLAGS: u64 = HEADER_FLAG_WRITTEN | (MIXED_BACKREF_REV << 56);
pub const SUPER_FLAG_WRITTEN: u64 = 1;
pub const ROOT_SUBVOL_RDONLY: u64 = 1 << 0;

// Directory entry types (`BTRFS_FT_*`).
pub const FT_REG_FILE: u8 = 1;
pub const FT_DIR: u8 = 2;
pub const FT_SYMLINK: u8 = 7;

pub const FILE_EXTENT_INLINE: u8 = 0;
pub const FILE_EXTENT_REG: u8 = 1;

pub const S_IFMT: u32 = 0o170000;
pub const S_IFDIR: u32 = 0o040000;
pub const S_IFREG: u32 = 0o100000;
pub const S_IFLNK: u32 = 0o120000;

pub fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd, Hash)]
pub struct Key {
    pub objectid: u64,
    pub kind: u8,
    pub offset: u64,
}

impl Key {
    pub const fn new(objectid: u64, kind: u8, offset: u64) -> Self {
        Self {
            objectid,
            kind,
            offset,
        }
    }

    pub fn encode(&self, out: &mut Vec<u8>) {
        put64(out, self.objectid);
        out.push(self.kind);
        put64(out, self.offset);
    }

    pub fn decode(bytes: &[u8], at: usize) -> io::Result<Self> {
        Ok(Self {
            objectid: get64(bytes, at)?,
            kind: get8(bytes, at + 8)?,
            offset: get64(bytes, at + 9)?,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Item {
    pub key: Key,
    pub data: Vec<u8>,
}

impl Item {
    pub fn new(key: Key, data: Vec<u8>) -> Self {
        Self { key, data }
    }
}

pub fn put8(out: &mut Vec<u8>, value: u8) {
    out.push(value);
}
pub fn put16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_le_bytes());
}
pub fn put32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}
pub fn put64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_le_bytes());
}

pub fn bytes<const N: usize>(data: &[u8], at: usize) -> io::Result<[u8; N]> {
    data.get(at..at.saturating_add(N))
        .and_then(|value| value.try_into().ok())
        .ok_or_else(|| invalid(format!("truncated field at byte {at}")))
}
pub fn get8(data: &[u8], at: usize) -> io::Result<u8> {
    Ok(bytes::<1>(data, at)?[0])
}
pub fn get16(data: &[u8], at: usize) -> io::Result<u16> {
    Ok(u16::from_le_bytes(bytes(data, at)?))
}
pub fn get32(data: &[u8], at: usize) -> io::Result<u32> {
    Ok(u32::from_le_bytes(bytes(data, at)?))
}
pub fn get64(data: &[u8], at: usize) -> io::Result<u64> {
    Ok(u64::from_le_bytes(bytes(data, at)?))
}
pub fn slice(data: &[u8], at: usize, len: usize) -> io::Result<&[u8]> {
    data.get(at..at.saturating_add(len))
        .ok_or_else(|| invalid(format!("truncated {len}-byte field at byte {at}")))
}

/// Overwrite `out[at..at + value.len()]`.
pub fn place(out: &mut [u8], at: usize, value: &[u8]) -> io::Result<()> {
    out.get_mut(at..at.saturating_add(value.len()))
        .ok_or_else(|| invalid(format!("write past the block at byte {at}")))?
        .copy_from_slice(value);
    Ok(())
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Time {
    pub sec: u64,
    pub nsec: u32,
}

impl Time {
    fn encode(&self, out: &mut Vec<u8>) {
        put64(out, self.sec);
        put32(out, self.nsec);
    }
    fn decode(data: &[u8], at: usize) -> io::Result<Self> {
        Ok(Self {
            sec: get64(data, at)?,
            nsec: get32(data, at + 8)?,
        })
    }
}

/// `btrfs_inode_item`, 160 bytes.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Inode {
    pub generation: u64,
    pub transid: u64,
    pub size: u64,
    pub nbytes: u64,
    pub nlink: u32,
    pub uid: u32,
    pub gid: u32,
    pub mode: u32,
    pub rdev: u64,
    pub flags: u64,
    pub atime: Time,
    pub ctime: Time,
    pub mtime: Time,
    pub otime: Time,
}

pub const INODE_SIZE: usize = 160;

impl Inode {
    pub fn encode(&self, out: &mut Vec<u8>) {
        put64(out, self.generation);
        put64(out, self.transid);
        put64(out, self.size);
        put64(out, self.nbytes);
        put64(out, 0); // block_group
        put32(out, self.nlink);
        put32(out, self.uid);
        put32(out, self.gid);
        put32(out, self.mode);
        put64(out, self.rdev);
        put64(out, self.flags);
        put64(out, 0); // sequence
        out.extend_from_slice(&[0; 32]);
        self.atime.encode(out);
        self.ctime.encode(out);
        self.mtime.encode(out);
        self.otime.encode(out);
    }

    pub fn decode(data: &[u8]) -> io::Result<Self> {
        if data.len() < INODE_SIZE {
            return Err(invalid("short inode item"));
        }
        Ok(Self {
            generation: get64(data, 0)?,
            transid: get64(data, 8)?,
            size: get64(data, 16)?,
            nbytes: get64(data, 24)?,
            nlink: get32(data, 40)?,
            uid: get32(data, 44)?,
            gid: get32(data, 48)?,
            mode: get32(data, 52)?,
            rdev: get64(data, 56)?,
            flags: get64(data, 64)?,
            atime: Time::decode(data, 112)?,
            ctime: Time::decode(data, 124)?,
            mtime: Time::decode(data, 136)?,
            otime: Time::decode(data, 148)?,
        })
    }
}

/// `btrfs_root_item`, 439 bytes.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Root {
    pub inode: Inode,
    pub generation: u64,
    pub root_dirid: u64,
    pub bytenr: u64,
    pub bytes_used: u64,
    pub flags: u64,
    pub refs: u32,
    pub level: u8,
    pub uuid: [u8; 16],
    pub ctransid: u64,
    pub otransid: u64,
    pub ctime: Time,
    pub otime: Time,
}

pub const ROOT_ITEM_SIZE: usize = 439;

impl Root {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(ROOT_ITEM_SIZE);
        self.inode.encode(&mut out);
        put64(&mut out, self.generation);
        put64(&mut out, self.root_dirid);
        put64(&mut out, self.bytenr);
        put64(&mut out, 0); // byte_limit
        put64(&mut out, self.bytes_used);
        put64(&mut out, 0); // last_snapshot
        put64(&mut out, self.flags);
        put32(&mut out, self.refs);
        Key::default().encode(&mut out); // drop_progress
        put8(&mut out, 0); // drop_level
        put8(&mut out, self.level);
        put64(&mut out, self.generation); // generation_v2
        out.extend_from_slice(&self.uuid);
        out.extend_from_slice(&[0; 32]); // parent_uuid, received_uuid
        put64(&mut out, self.ctransid);
        put64(&mut out, self.otransid);
        put64(&mut out, 0); // stransid
        put64(&mut out, 0); // rtransid
        self.ctime.encode(&mut out);
        self.otime.encode(&mut out);
        Time::default().encode(&mut out); // stime
        Time::default().encode(&mut out); // rtime
        out.extend_from_slice(&[0; 64]);
        out
    }

    pub fn decode(data: &[u8]) -> io::Result<Self> {
        if data.len() != ROOT_ITEM_SIZE {
            return Err(invalid(format!("root item is {} bytes", data.len())));
        }
        let generation = get64(data, 160)?;
        if get64(data, 239)? != generation {
            return Err(invalid("root item generation_v2 differs from generation"));
        }
        Ok(Self {
            inode: Inode::decode(data)?,
            generation,
            root_dirid: get64(data, 168)?,
            bytenr: get64(data, 176)?,
            bytes_used: get64(data, 192)?,
            flags: get64(data, 208)?,
            refs: get32(data, 216)?,
            level: get8(data, 238)?,
            uuid: bytes(data, 247)?,
            ctransid: get64(data, 295)?,
            otransid: get64(data, 303)?,
            ctime: Time::decode(data, 327)?,
            otime: Time::decode(data, 339)?,
        })
    }
}

/// `btrfs_inode_ref` entries, or a ROOT_REF/ROOT_BACKREF's name.
pub fn inode_ref(index: u64, name: &[u8], out: &mut Vec<u8>) {
    put64(out, index);
    put16(out, name.len() as u16);
    out.extend_from_slice(name);
}

/// `btrfs_dir_item`, one entry; a DIR_ITEM may hold several.
pub fn dir_item(location: Key, transid: u64, kind: u8, name: &[u8], out: &mut Vec<u8>) {
    location.encode(out);
    put64(out, transid);
    put16(out, 0); // data_len
    put16(out, name.len() as u16);
    put8(out, kind);
    out.extend_from_slice(name);
}

pub fn root_ref(dirid: u64, sequence: u64, name: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    put64(&mut out, dirid);
    put64(&mut out, sequence);
    put16(&mut out, name.len() as u16);
    out.extend_from_slice(name);
    out
}

pub fn file_extent_inline(generation: u64, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(21 + data.len());
    put64(&mut out, generation);
    put64(&mut out, data.len() as u64); // ram_bytes
    put8(&mut out, 0); // compression
    put8(&mut out, 0); // encryption
    put16(&mut out, 0); // other_encoding
    put8(&mut out, FILE_EXTENT_INLINE);
    out.extend_from_slice(data);
    out
}

pub fn file_extent_reg(generation: u64, disk_bytenr: u64, num_bytes: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(53);
    put64(&mut out, generation);
    put64(&mut out, num_bytes); // ram_bytes
    put8(&mut out, 0);
    put8(&mut out, 0);
    put16(&mut out, 0);
    put8(&mut out, FILE_EXTENT_REG);
    put64(&mut out, disk_bytenr);
    put64(&mut out, num_bytes); // disk_num_bytes
    put64(&mut out, 0); // offset
    put64(&mut out, num_bytes);
    out
}

pub fn data_extent(generation: u64, root: u64, owner: u64, offset: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(53);
    put64(&mut out, 1); // refs
    put64(&mut out, generation);
    put64(&mut out, EXTENT_FLAG_DATA);
    put8(&mut out, EXTENT_DATA_REF);
    put64(&mut out, root);
    put64(&mut out, owner);
    put64(&mut out, offset);
    put32(&mut out, 1); // count
    out
}

pub fn metadata_extent(generation: u64, owner: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(33);
    put64(&mut out, 1);
    put64(&mut out, generation);
    put64(&mut out, EXTENT_FLAG_TREE_BLOCK);
    put8(&mut out, TREE_BLOCK_REF);
    put64(&mut out, owner);
    out
}

pub fn block_group(used: u64, flags: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(24);
    put64(&mut out, used);
    put64(&mut out, FIRST_CHUNK_TREE);
    put64(&mut out, flags);
    out
}

pub fn free_space_info(extent_count: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(8);
    put32(&mut out, extent_count);
    put32(&mut out, 0); // flags: extents, not bitmaps
    out
}

/// One chunk's device stripe.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Stripe {
    pub devid: u64,
    pub offset: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Chunk {
    pub logical: u64,
    pub length: u64,
    pub flags: u64,
    pub stripes: Vec<Stripe>,
}

impl Chunk {
    pub fn encode(&self, dev_uuid: &[u8; 16]) -> Vec<u8> {
        let mut out = Vec::with_capacity(48 + 32 * self.stripes.len());
        put64(&mut out, self.length);
        put64(&mut out, EXTENT_TREE); // owner
        put64(&mut out, STRIPE_LEN);
        put64(&mut out, self.flags);
        put32(&mut out, STRIPE_LEN as u32); // io_align
        put32(&mut out, STRIPE_LEN as u32); // io_width
        put32(&mut out, SECTORSIZE);
        put16(&mut out, self.stripes.len() as u16);
        put16(&mut out, 1); // sub_stripes
        for stripe in &self.stripes {
            put64(&mut out, stripe.devid);
            put64(&mut out, stripe.offset);
            out.extend_from_slice(dev_uuid);
        }
        out
    }

    /// A CHUNK_ITEM at `at` in `data`, returning it and its encoded length.
    pub fn decode(logical: u64, data: &[u8], at: usize) -> io::Result<(Self, usize)> {
        let length = get64(data, at)?;
        let flags = get64(data, at + 24)?;
        let count = usize::from(get16(data, at + 44)?);
        if count == 0 {
            return Err(invalid(format!("chunk {logical} has no stripes")));
        }
        let mut stripes = Vec::with_capacity(count);
        for index in 0..count {
            let base = at + 48 + 32 * index;
            stripes.push(Stripe {
                devid: get64(data, base)?,
                offset: get64(data, base + 8)?,
            });
        }
        let chunk = Self {
            logical,
            length,
            flags,
            stripes,
        };
        Ok((chunk, 48 + 32 * count))
    }
}

/// `btrfs_dev_item`, 98 bytes.
pub fn dev_item(
    total_bytes: u64,
    bytes_used: u64,
    dev_uuid: &[u8; 16],
    fsid: &[u8; 16],
) -> Vec<u8> {
    let mut out = Vec::with_capacity(98);
    put64(&mut out, 1); // devid
    put64(&mut out, total_bytes);
    put64(&mut out, bytes_used);
    put32(&mut out, SECTORSIZE); // io_align
    put32(&mut out, SECTORSIZE); // io_width
    put32(&mut out, SECTORSIZE);
    put64(&mut out, 0); // type
    put64(&mut out, 0); // generation
    put64(&mut out, 0); // start_offset
    put32(&mut out, 0); // dev_group
    put8(&mut out, 0); // seek_speed
    put8(&mut out, 0); // bandwidth
    out.extend_from_slice(dev_uuid);
    out.extend_from_slice(fsid);
    out
}

pub fn dev_extent(chunk_offset: u64, length: u64, chunk_tree_uuid: &[u8; 16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(48);
    put64(&mut out, CHUNK_TREE);
    put64(&mut out, FIRST_CHUNK_TREE);
    put64(&mut out, chunk_offset);
    put64(&mut out, length);
    out.extend_from_slice(chunk_tree_uuid);
    out
}

/// The tree-block header fields a writer chooses.
#[derive(Clone, Copy, Debug)]
pub struct Header {
    pub fsid: [u8; 16],
    pub chunk_tree_uuid: [u8; 16],
    pub generation: u64,
    pub owner: u64,
}

pub fn round_up(value: u64, to: u64) -> u64 {
    value.div_ceil(to).saturating_mul(to)
}

/// Canonical lowercase `8-4-4-4-12` text.
pub fn uuid_text(uuid: &[u8; 16]) -> String {
    let mut out = String::with_capacity(36);
    for (index, byte) in uuid.iter().enumerate() {
        if matches!(index, 4 | 6 | 8 | 10) {
            out.push('-');
        }
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

pub fn parse_uuid(text: &str) -> io::Result<[u8; 16]> {
    let raw = text.as_bytes();
    if raw.len() != 36 {
        return Err(invalid(format!("{text:?} is not a canonical UUID")));
    }
    let mut out = [0u8; 16];
    let mut digits = raw
        .iter()
        .enumerate()
        .filter(|(index, _)| !matches!(index, 8 | 13 | 18 | 23))
        .map(|(_, digit)| *digit);
    for (index, separator) in raw.iter().enumerate() {
        if matches!(index, 8 | 13 | 18 | 23) && *separator != b'-' {
            return Err(invalid(format!("{text:?} is not a canonical UUID")));
        }
    }
    for slot in out.iter_mut() {
        let (Some(high), Some(low)) = (digits.next(), digits.next()) else {
            return Err(invalid(format!("{text:?} is not a canonical UUID")));
        };
        *slot = hex(high)? << 4 | hex(low)?;
    }
    Ok(out)
}

fn hex(digit: u8) -> io::Result<u8> {
    match digit {
        b'0'..=b'9' => Ok(digit - b'0'),
        b'a'..=b'f' => Ok(digit - b'a' + 10),
        b'A'..=b'F' => Ok(digit - b'A' + 10),
        _ => Err(invalid("invalid UUID digit")),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn encodings_have_their_on_disk_sizes() {
        let mut inode = Vec::new();
        Inode::default().encode(&mut inode);
        assert_eq!(inode.len(), INODE_SIZE);
        assert_eq!(Root::default().encode().len(), ROOT_ITEM_SIZE);
        assert_eq!(dev_item(0, 0, &[0; 16], &[0; 16]).len(), 98);
        assert_eq!(dev_extent(0, 0, &[0; 16]).len(), 48);
        assert_eq!(data_extent(1, 5, 257, 0).len(), 53);
        assert_eq!(metadata_extent(1, 5).len(), 33);
        assert_eq!(file_extent_reg(1, 0, 4096).len(), 53);
        assert_eq!(file_extent_inline(1, b"hello\n").len(), 27);
        let mut dir = Vec::new();
        dir_item(Key::default(), 1, FT_DIR, b"td", &mut dir);
        assert_eq!(dir.len(), 32);
        assert_eq!(root_ref(256, 3, b"@var").len(), 22);
        let chunk = Chunk {
            logical: 0,
            length: 0,
            flags: 0,
            stripes: vec![
                Stripe {
                    devid: 1,
                    offset: 0
                };
                2
            ],
        };
        assert_eq!(chunk.encode(&[0; 16]).len(), 112);
        assert_eq!(NODE_PTRS, 493);
        assert_eq!(MAX_CSUMS, 4057);
    }

    #[test]
    fn root_items_round_trip() {
        let root = Root {
            generation: 7,
            root_dirid: 256,
            bytenr: 1 << 20,
            bytes_used: 16384,
            flags: ROOT_SUBVOL_RDONLY,
            refs: 1,
            level: 2,
            uuid: [9; 16],
            ctransid: 7,
            otransid: 7,
            ctime: Time { sec: 5, nsec: 6 },
            otime: Time { sec: 8, nsec: 9 },
            ..Root::default()
        };
        assert_eq!(Root::decode(&root.encode()).unwrap(), root);
    }

    #[test]
    fn uuids_round_trip() {
        let text = "12345678-1234-4234-8234-123456789abc";
        assert_eq!(uuid_text(&parse_uuid(text).unwrap()), text);
        for bad in [
            "",
            "12345678-1234-4234-8234-123456789abcd",
            "12345678x1234-4234-8234-123456789abc",
            "1234567g-1234-4234-8234-123456789abc",
        ] {
            assert!(parse_uuid(bad).is_err(), "{bad}");
        }
    }
}

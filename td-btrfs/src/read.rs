//! Reading a single-device Btrfs filesystem: superblock, chunk map and
//! whole trees, each block verified as it is read.

use std::collections::HashMap;
use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;
use std::path::Path;

use crate::crc32c;
use crate::format::*;

/// Incompatible features the reader understands.
const KNOWN_INCOMPAT: u64 = INCOMPAT | (1 << 1); // DEFAULT_SUBVOL
const KNOWN_COMPAT_RO: u64 = COMPAT_RO;
pub const TREE_LOG: u64 = -6i64 as u64;
pub const TREE_LOG_FIXUP: u64 = -7i64 as u64;

#[derive(Clone, Debug)]
pub struct Super {
    pub fsid: [u8; 16],
    pub bytenr: u64,
    pub flags: u64,
    pub generation: u64,
    pub root: u64,
    pub chunk_root: u64,
    pub log_root: u64,
    pub total_bytes: u64,
    pub bytes_used: u64,
    pub root_dir: u64,
    pub num_devices: u64,
    pub sectorsize: u32,
    pub nodesize: u32,
    pub chunk_root_generation: u64,
    pub compat_ro: u64,
    pub incompat: u64,
    pub csum_type: u16,
    pub root_level: u8,
    pub chunk_root_level: u8,
    pub log_root_level: u8,
    pub dev_total_bytes: u64,
    pub dev_bytes_used: u64,
    pub devid: u64,
    pub dev_uuid: [u8; 16],
    pub dev_fsid: [u8; 16],
    pub label: Vec<u8>,
    pub uuid_tree_generation: u64,
    pub sys_chunks: Vec<Chunk>,
}

impl Super {
    /// Parse and verify one superblock copy read from `offset`.
    pub fn parse(raw: &[u8], offset: u64) -> io::Result<Self> {
        if raw.len() != SUPER_SIZE {
            return Err(invalid("short superblock"));
        }
        if bytes::<8>(raw, 64)? != *SUPER_MAGIC {
            return Err(invalid(format!("no Btrfs superblock at byte {offset}")));
        }
        let csum_type = get16(raw, 196)?;
        if csum_type != 0 {
            return Err(invalid(format!("checksum type {csum_type} is not crc32c")));
        }
        if get32(raw, 0)? != crc32c::checksum(slice(raw, 32, SUPER_SIZE - 32)?) {
            return Err(invalid(format!(
                "superblock at byte {offset} fails its checksum"
            )));
        }
        let label = slice(raw, 299, 256)?;
        let label = label.split(|byte| *byte == 0).next().unwrap_or_default();
        let sys_size = get32(raw, 160)? as usize;
        if sys_size > 2048 {
            return Err(invalid("system chunk array overflows the superblock"));
        }
        let sys = slice(raw, crate::mkfs::SYS_CHUNK_ARRAY, sys_size)?;
        let mut sys_chunks = Vec::new();
        let mut at = 0;
        while at < sys.len() {
            let key = Key::decode(sys, at)?;
            if key.kind != CHUNK_ITEM {
                return Err(invalid("system chunk array holds a non-chunk key"));
            }
            let (chunk, len) = Chunk::decode(key.offset, sys, at + 17)?;
            sys_chunks.push(chunk);
            at += 17 + len;
        }
        let sb = Self {
            fsid: bytes(raw, 32)?,
            bytenr: get64(raw, 48)?,
            flags: get64(raw, 56)?,
            generation: get64(raw, 72)?,
            root: get64(raw, 80)?,
            chunk_root: get64(raw, 88)?,
            log_root: get64(raw, 96)?,
            total_bytes: get64(raw, 112)?,
            bytes_used: get64(raw, 120)?,
            root_dir: get64(raw, 128)?,
            num_devices: get64(raw, 136)?,
            sectorsize: get32(raw, 144)?,
            nodesize: get32(raw, 148)?,
            chunk_root_generation: get64(raw, 164)?,
            compat_ro: get64(raw, 180)?,
            incompat: get64(raw, 188)?,
            csum_type,
            root_level: get8(raw, 198)?,
            chunk_root_level: get8(raw, 199)?,
            log_root_level: get8(raw, 200)?,
            devid: get64(raw, 201)?,
            dev_total_bytes: get64(raw, 209)?,
            dev_bytes_used: get64(raw, 217)?,
            dev_uuid: bytes(raw, 201 + 66)?,
            dev_fsid: bytes(raw, 201 + 82)?,
            label: label.to_vec(),
            uuid_tree_generation: get64(raw, 563)?,
            sys_chunks,
        };
        if sb.bytenr != offset {
            return Err(invalid(format!(
                "superblock at byte {offset} records byte {}",
                sb.bytenr
            )));
        }
        if sb.nodesize != NODESIZE || sb.sectorsize != SECTORSIZE {
            return Err(invalid(format!(
                "nodesize {} / sectorsize {} are not td's {NODESIZE} / {SECTORSIZE}",
                sb.nodesize, sb.sectorsize
            )));
        }
        if sb.num_devices != 1 || sb.devid != 1 {
            return Err(invalid("only single-device filesystems are supported"));
        }
        if sb.incompat & !KNOWN_INCOMPAT != 0 || sb.compat_ro & !KNOWN_COMPAT_RO != 0 {
            return Err(invalid(format!(
                "unsupported features: incompat {:#x}, compat_ro {:#x}",
                sb.incompat, sb.compat_ro
            )));
        }
        if sb.dev_fsid != sb.fsid {
            return Err(invalid("the device item names another filesystem"));
        }
        Ok(sb)
    }
}

/// One verified tree block.
#[derive(Clone, Debug)]
pub struct Block {
    pub bytes: Vec<u8>,
    pub generation: u64,
    pub owner: u64,
    pub level: u8,
    pub nritems: usize,
    pub chunk_tree_uuid: [u8; 16],
}

/// An item as read, with the leaf holding it.
#[derive(Clone, Debug)]
pub struct ReadItem {
    pub key: Key,
    pub data: Vec<u8>,
    pub leaf: u64,
}

/// A tree block visited by a walk.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Visited {
    pub address: u64,
    pub level: u8,
    pub owner: u64,
}

/// The blocks every walk sharing this record has visited. A block is read
/// once: a second reach, from any tree, refuses the walk before the block's
/// subtree is read again.
#[derive(Default)]
pub struct Visits {
    pub blocks: Vec<Visited>,
    owners: HashMap<u64, u64>,
}

pub struct Volume {
    file: File,
    pub size: u64,
    pub sb: Super,
    pub chunks: Vec<Chunk>,
    pub chunk_tree_uuid: [u8; 16],
}

impl Volume {
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = File::open(path).map_err(|error| {
            io::Error::new(error.kind(), format!("{}: {error}", path.display()))
        })?;
        let size = device_size(&file)?;
        let mut raw = vec![0u8; SUPER_SIZE];
        file.read_exact_at(&mut raw, SUPER_OFFSET)?;
        let sb = Super::parse(&raw, SUPER_OFFSET)?;
        if sb.total_bytes > size || sb.dev_total_bytes > size {
            return Err(invalid(format!(
                "the filesystem records {} bytes but the device has {size}",
                sb.total_bytes
            )));
        }
        // The system chunks map the chunk tree's own walk, so they are held
        // to the same geometry before it.
        if let Some(chunk) = sb.sys_chunks.iter().find(|chunk| !fits(chunk)) {
            return Err(invalid(format!(
                "system chunk {} has an impossible length",
                chunk.logical
            )));
        }
        let mut volume = Self {
            file,
            size,
            chunks: sb.sys_chunks.clone(),
            sb,
            chunk_tree_uuid: [0; 16],
        };
        let root = volume.block(volume.sb.chunk_root)?;
        volume.chunk_tree_uuid = root.chunk_tree_uuid;
        let mut visited = Visits::default();
        let items = volume.walk(
            volume.sb.chunk_root,
            volume.sb.chunk_root_level,
            CHUNK_TREE,
            Some(volume.sb.chunk_root_generation),
            &mut visited,
        )?;
        let mut chunks = Vec::new();
        for item in &items {
            if item.key.kind == CHUNK_ITEM {
                let (chunk, len) = Chunk::decode(item.key.offset, &item.data, 0)?;
                if len != item.data.len() {
                    return Err(invalid(format!(
                        "chunk item {} has trailing bytes",
                        chunk.logical
                    )));
                }
                chunks.push(chunk);
            }
        }
        for sys in &volume.sb.sys_chunks {
            if !chunks.contains(sys) {
                return Err(invalid(format!(
                    "system chunk {} is not in the chunk tree",
                    sys.logical
                )));
            }
        }
        if let Some(chunk) = chunks.iter().find(|chunk| !fits(chunk)) {
            return Err(invalid(format!(
                "chunk {} has an impossible length",
                chunk.logical
            )));
        }
        chunks.sort_by_key(|chunk| chunk.logical);
        for pair in chunks.windows(2) {
            if let [a, b] = pair {
                if a.logical + a.length > b.logical {
                    return Err(invalid(format!(
                        "chunks {} and {} overlap",
                        a.logical, b.logical
                    )));
                }
            }
        }
        for chunk in &chunks {
            let profile = chunk.flags & !(BG_DATA | BG_SYSTEM | BG_METADATA);
            let stripes = match profile {
                0 => 1,
                BG_DUP => 2,
                _ => {
                    return Err(invalid(format!(
                        "chunk {} uses an unsupported profile {profile:#x}",
                        chunk.logical
                    )))
                }
            };
            if chunk.stripes.len() != stripes
                || chunk
                    .stripes
                    .iter()
                    .any(|s| s.devid != 1 || s.offset + chunk.length > size)
            {
                return Err(invalid(format!(
                    "chunk {} has invalid stripes",
                    chunk.logical
                )));
            }
        }
        volume.chunks = chunks;
        Ok(volume)
    }

    pub fn chunk(&self, logical: u64) -> Option<&Chunk> {
        self.chunks
            .iter()
            .find(|chunk| logical >= chunk.logical && logical < chunk.logical + chunk.length)
    }

    /// Read `len` bytes at `logical` from every stripe that holds them.
    pub fn read_copies(&self, logical: u64, len: usize) -> io::Result<Vec<Vec<u8>>> {
        let chunk = self
            .chunk(logical)
            .filter(|chunk| holds(chunk, logical, len as u64))
            .ok_or_else(|| invalid(format!("logical range {logical}+{len} is in no chunk")))?;
        let mut copies = Vec::with_capacity(chunk.stripes.len());
        for stripe in &chunk.stripes {
            let mut buffer = vec![0u8; len];
            self.file
                .read_exact_at(&mut buffer, stripe.offset + (logical - chunk.logical))?;
            copies.push(buffer);
        }
        Ok(copies)
    }

    /// Read from the first stripe into `out`.
    pub fn read_into(&self, logical: u64, out: &mut [u8]) -> io::Result<()> {
        self.read_stripe_into(logical, 0, out)
    }

    /// Read from stripe `copy` into `out`.
    pub fn read_stripe_into(&self, logical: u64, copy: usize, out: &mut [u8]) -> io::Result<()> {
        let chunk = self
            .chunk(logical)
            .filter(|chunk| holds(chunk, logical, out.len() as u64))
            .ok_or_else(|| {
                invalid(format!(
                    "logical range {logical}+{} is in no chunk",
                    out.len()
                ))
            })?;
        let stripe = chunk
            .stripes
            .get(copy)
            .ok_or_else(|| invalid(format!("chunk {} has no stripe {copy}", chunk.logical)))?;
        self.file
            .read_exact_at(out, stripe.offset + (logical - chunk.logical))
    }

    /// Read and verify the block at `logical`, every copy of it.
    pub fn block(&self, logical: u64) -> io::Result<Block> {
        if !logical.is_multiple_of(u64::from(SECTORSIZE)) {
            return Err(invalid(format!(
                "tree block {logical} is not sector aligned"
            )));
        }
        let copies = self.read_copies(logical, NODESIZE as usize)?;
        let mut first: Option<Vec<u8>> = None;
        for (index, copy) in copies.into_iter().enumerate() {
            let sum = get32(&copy, 0)?;
            if sum != crc32c::checksum(slice(&copy, 32, NODESIZE as usize - 32)?) {
                return Err(invalid(format!(
                    "tree block {logical} copy {index} fails its checksum"
                )));
            }
            match &first {
                Some(bytes) if *bytes != copy => {
                    return Err(invalid(format!("tree block {logical}'s copies differ")))
                }
                Some(_) => {}
                None => first = Some(copy),
            }
        }
        let raw = first.ok_or_else(|| invalid("no copy read"))?;
        if get64(&raw, 48)? != logical {
            return Err(invalid(format!(
                "tree block {logical} records another address"
            )));
        }
        if bytes::<16>(&raw, 32)? != self.sb.fsid {
            return Err(invalid(format!(
                "tree block {logical} belongs to another filesystem"
            )));
        }
        let chunk_tree_uuid = bytes::<16>(&raw, 64)?;
        if self.chunk_tree_uuid != [0; 16] && chunk_tree_uuid != self.chunk_tree_uuid {
            return Err(invalid(format!(
                "tree block {logical} names another chunk tree"
            )));
        }
        let level = get8(&raw, 100)?;
        let nritems = get32(&raw, 96)? as usize;
        Ok(Block {
            generation: get64(&raw, 80)?,
            owner: get64(&raw, 88)?,
            level,
            nritems,
            chunk_tree_uuid,
            bytes: raw,
        })
    }

    /// Every item of the tree rooted at `root`, in key order, verifying each
    /// block's structure. Visited blocks are recorded in `visited`.
    pub fn walk(
        &self,
        root: u64,
        level: u8,
        owner: u64,
        generation: Option<u64>,
        visited: &mut Visits,
    ) -> io::Result<Vec<ReadItem>> {
        let mut items = Vec::new();
        self.descend(
            root, level, owner, generation, None, None, &mut items, visited,
        )?;
        Ok(items)
    }

    #[allow(clippy::too_many_arguments)]
    fn descend(
        &self,
        address: u64,
        level: u8,
        owner: u64,
        generation: Option<u64>,
        low: Option<Key>,
        high: Option<Key>,
        items: &mut Vec<ReadItem>,
        visited: &mut Visits,
    ) -> io::Result<()> {
        if let Some(previous) = visited.owners.insert(address, owner) {
            return Err(invalid(format!(
                "tree block {address} is reached twice, from trees {previous} and {owner} \
                 (shared blocks and snapshots are not supported)"
            )));
        }
        let block = self.block(address)?;
        let what = format!("tree {owner} block {address}");
        if block.level != level {
            return Err(invalid(format!(
                "{what} is level {}, not {level}",
                block.level
            )));
        }
        if block.owner != owner {
            return Err(invalid(format!("{what} is owned by {}", block.owner)));
        }
        if let Some(expected) = generation.filter(|expected| *expected != block.generation) {
            return Err(invalid(format!(
                "{what} is generation {}, its parent expects {expected}",
                block.generation
            )));
        }
        let limit = self
            .sb
            .generation
            .saturating_add(u64::from(owner == TREE_LOG));
        if block.generation > limit {
            return Err(invalid(format!("{what} is from a future generation")));
        }
        visited.blocks.push(Visited {
            address,
            level,
            owner,
        });
        let in_bounds =
            |key: Key| low.is_none_or(|low| key >= low) && high.is_none_or(|high| key < high);
        if level == 0 {
            if block.nritems * ITEM_SIZE > LEAF_DATA {
                return Err(invalid(format!("{what} claims too many items")));
            }
            let mut end = LEAF_DATA;
            let mut previous: Option<Key> = None;
            for index in 0..block.nritems {
                let at = HEADER_SIZE + index * ITEM_SIZE;
                let key = Key::decode(&block.bytes, at)?;
                let offset = get32(&block.bytes, at + 17)? as usize;
                let size = get32(&block.bytes, at + 21)? as usize;
                if previous.is_some_and(|previous| previous >= key) || !in_bounds(key) {
                    return Err(invalid(format!(
                        "{what} keys are out of order at item {index}"
                    )));
                }
                if offset + size != end {
                    return Err(invalid(format!("{what} item {index} data is misplaced")));
                }
                end = offset;
                previous = Some(key);
                items.push(ReadItem {
                    key,
                    data: slice(&block.bytes, HEADER_SIZE + offset, size)?.to_vec(),
                    leaf: address,
                });
            }
            if end < block.nritems * ITEM_SIZE {
                return Err(invalid(format!(
                    "{what} item data overlaps its item headers"
                )));
            }
            if block.nritems == 0 && low.is_some() {
                return Err(invalid(format!("{what} is an empty non-root leaf")));
            }
            return Ok(());
        }
        if block.nritems == 0 || block.nritems > NODE_PTRS {
            return Err(invalid(format!("{what} holds {} pointers", block.nritems)));
        }
        let mut children = Vec::with_capacity(block.nritems);
        for index in 0..block.nritems {
            let at = HEADER_SIZE + index * KEY_PTR_SIZE;
            let key = Key::decode(&block.bytes, at)?;
            children.push((
                key,
                get64(&block.bytes, at + 17)?,
                get64(&block.bytes, at + 25)?,
            ));
        }
        for (index, (key, pointer, child_generation)) in children.iter().enumerate() {
            let next = children.get(index + 1).map(|(key, _, _)| *key).or(high);
            if !in_bounds(*key) || next.is_some_and(|next| next <= *key) {
                return Err(invalid(format!(
                    "{what} keys are out of order at pointer {index}"
                )));
            }
            let first = items.len();
            self.descend(
                *pointer,
                level - 1,
                owner,
                Some(*child_generation),
                Some(*key),
                next,
                items,
                visited,
            )?;
            if items.get(first).is_some_and(|item| item.key != *key) {
                return Err(invalid(format!(
                    "{what} pointer {index}'s key is not its child's first key"
                )));
            }
        }
        Ok(())
    }
}

/// Every later sum of a chunk's address or a stripe's offset and the
/// chunk's length is within these. Sector alignment, which the kernel also
/// requires, puts every chunk's exclusive end at most `u64::MAX - 4095`, so a
/// sum that saturates is past every chunk.
fn fits(chunk: &Chunk) -> bool {
    let sector = u64::from(SECTORSIZE);
    chunk.length > 0
        && chunk.logical.is_multiple_of(sector)
        && chunk.length.is_multiple_of(sector)
        && chunk.logical.checked_add(chunk.length).is_some()
        && chunk
            .stripes
            .iter()
            .all(|s| s.offset.checked_add(chunk.length).is_some())
}

/// `chunk` holds `len` bytes at `logical`, an end that is itself a `u64`:
/// every address read is then followed by its length without overflow.
fn holds(chunk: &Chunk, logical: u64, len: u64) -> bool {
    logical >= chunk.logical
        && logical
            .checked_add(len)
            .is_some_and(|end| end <= chunk.logical + chunk.length)
}

/// A regular file's length, or a block device's.
pub fn device_size(file: &File) -> io::Result<u64> {
    use std::io::{Seek, SeekFrom};
    let mut handle = file.try_clone()?;
    handle.seek(SeekFrom::End(0))
}

/// The ROOT_ITEMs of the root tree, by objectid.
pub fn roots(items: &[ReadItem]) -> io::Result<Vec<(u64, Root)>> {
    items
        .iter()
        .filter(|item| item.key.kind == ROOT_ITEM)
        .map(|item| Ok((item.key.objectid, Root::decode(&item.data)?)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A chunk ending at the top of the address space holds no block whose
    /// end would be past it, and is not admitted at all.
    #[test]
    fn a_range_ending_past_u64_max_is_in_no_chunk() {
        let length = 0x80_0000;
        let chunk = Chunk {
            logical: u64::MAX - length,
            length,
            flags: 0,
            stripes: Vec::new(),
        };
        assert!(!fits(&chunk));
        let aligned = Chunk {
            logical: u64::MAX - length - 4095,
            ..chunk.clone()
        };
        assert!(fits(&aligned));
        let node = u64::from(NODESIZE);
        assert!(holds(&chunk, chunk.logical, node));
        assert!(holds(&chunk, u64::MAX - node, node));
        assert!(!holds(&chunk, 0xffff_ffff_ffff_f000, node));
        assert!(!holds(&chunk, chunk.logical - 1, 1));
    }
}

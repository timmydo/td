//! Making a filesystem: the whole image is planned in memory, its data
//! copied, then its metadata and finally its superblocks written.
//!
//! Everything is written in one generation. The trees that describe the
//! other trees' blocks (extent, free space, block group, root) are built
//! to a fixed point: their own block counts change their contents, which
//! can change their counts, until nothing moves.

use std::collections::{BTreeMap, HashMap};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{FileExt, FileTypeExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::crc32c;
use crate::format::*;
use crate::fs::{Extent, Fs};
use crate::layout::{default_metadata, Layout};
use crate::source::Source;
use crate::tree::{seal, Shape};

/// x86-64 Linux `O_EXCL`: on a block device, refuse one that is mounted or
/// otherwise held open exclusively.
const O_EXCL: i32 = 0o200;
/// The one generation every block, item and root is written in.
pub const GENERATION: u64 = 1;
/// How many times metadata may outgrow its chunk before giving up.
const SIZING_ROUNDS: usize = 8;
const COPY_BUFFER: usize = 1 << 20;
/// btrfs-progs clears this much at each end of the device.
const WIPE: u64 = 2 * MIB;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Options {
    pub byte_count: Option<u64>,
    pub uuid: Option<[u8; 16]>,
    pub label: Vec<u8>,
    pub rootdir: Option<PathBuf>,
    pub subvols: Vec<PathBuf>,
    pub target: PathBuf,
}

/// What `make` built, for its report.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Made {
    pub fsid: [u8; 16],
    pub total: u64,
    pub layout: Layout,
    pub bytes_used: u64,
}

/// Every identifier a new filesystem draws at random.
#[derive(Clone, Debug)]
pub struct Ids {
    pub fsid: [u8; 16],
    pub dev_uuid: [u8; 16],
    pub chunk_tree_uuid: [u8; 16],
    /// For FS_TREE and each subvolume, in tree order.
    pub subvols: Vec<[u8; 16]>,
}

fn random(count: usize) -> io::Result<Vec<[u8; 16]>> {
    let mut bytes = vec![0u8; 16 * count];
    File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes
        .as_chunks::<16>()
        .0
        .iter()
        .map(|uuid| {
            let mut uuid = *uuid;
            if let Some(byte) = uuid.get_mut(6) {
                *byte = (*byte & 0x0f) | 0x40;
            }
            if let Some(byte) = uuid.get_mut(8) {
                *byte = (*byte & 0x3f) | 0x80;
            }
            uuid
        })
        .collect())
}

fn now() -> Time {
    let since = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    Time {
        sec: since.as_secs(),
        nsec: since.subsec_nanos(),
    }
}

fn context(path: &Path, error: io::Error) -> io::Error {
    io::Error::new(error.kind(), format!("{}: {error}", path.display()))
}

/// The opened target, the filesystem size, and a regular file's (device, inode).
type Opened = (File, u64, Option<(u64, u64)>);

/// Open the target and settle the filesystem's size. A regular file shorter
/// than `byte_count` is grown to it by `make`, once every input is accepted.
fn open_target(options: &Options) -> io::Result<Opened> {
    let path = &options.target;
    let metadata = std::fs::metadata(path).map_err(|error| context(path, error))?;
    let kind = metadata.file_type();
    if kind.is_block_device() {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(O_EXCL)
            .open(path)
            .map_err(|error| context(path, error))?;
        let size = file.seek(SeekFrom::End(0))?;
        let total = options.byte_count.unwrap_or(size);
        if total > size {
            return Err(invalid(format!(
                "{}: --byte-count {total} exceeds the device's {size} bytes",
                path.display()
            )));
        }
        Ok((file, total, None))
    } else if kind.is_file() {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|error| context(path, error))?;
        let size = file.metadata()?.len();
        let total = options.byte_count.unwrap_or(size);
        Ok((file, total, Some((metadata.dev(), metadata.ino()))))
    } else {
        Err(invalid(format!(
            "{} is neither a block device nor a regular file",
            path.display()
        )))
    }
}

/// Make the filesystem `options` describes.
pub fn make(options: &Options) -> io::Result<Made> {
    if options.label.len() > 255 || options.label.contains(&0) || options.label.contains(&b'\n') {
        return Err(invalid(
            "a label is at most 255 bytes, with no NUL or newline",
        ));
    }
    if options.rootdir.is_none() && !options.subvols.is_empty() {
        return Err(invalid("--subvol needs --rootdir"));
    }
    let (file, total, target_id) = open_target(options)?;
    let total = total / u64::from(SECTORSIZE) * u64::from(SECTORSIZE);
    let stamp = now();
    let source = match &options.rootdir {
        Some(root) => Source::scan(root, target_id, &options.subvols)?,
        None => Source::empty(stamp),
    };
    let fs = Fs::plan(&source)?;
    let mut drawn = random(3 + fs.trees.len())?.into_iter();
    let mut next = || drawn.next().ok_or_else(|| invalid("short random read"));
    let ids = Ids {
        fsid: match options.uuid {
            Some(uuid) => {
                next()?;
                uuid
            }
            None => next()?,
        },
        dev_uuid: next()?,
        chunk_tree_uuid: next()?,
        subvols: fs.trees.iter().map(|_| next()).collect::<io::Result<_>>()?,
    };
    let mut image = Image::plan(&fs, &source, &ids, total, stamp, &options.label)?;
    if target_id.is_some() && file.metadata()?.len() < total {
        file.set_len(total)
            .map_err(|error| context(&options.target, error))?;
    }
    let target = Target {
        file: &file,
        path: &options.target,
    };
    // The wipe is durable before any new block is: a crash in between
    // cannot leave an old superblock naming trees half overwritten.
    target.wipe(total)?;
    target.sync()?;
    let csums = image.copy_data(&fs, &target)?;
    let blocks = image.metadata(&csums)?;
    for (logical, block) in &blocks {
        target.write_logical(&image.layout, *logical, block)?;
    }
    // Every copy is built before any is written, so an error cannot leave
    // the primary without its mirrors. The kernel keeps a copy only where
    // it ends before the device does.
    let supers = SUPER_MIRRORS
        .iter()
        .filter(|&&offset| offset + (SUPER_SIZE as u64) < total)
        .map(|&offset| Ok((offset, image.superblock(offset)?)))
        .collect::<io::Result<Vec<_>>>()?;
    target.sync()?;
    for (offset, block) in &supers {
        target.write(*offset, block)?;
    }
    target.sync()?;
    Ok(Made {
        fsid: ids.fsid,
        total,
        layout: image.layout.clone(),
        bytes_used: image.bytes_used,
    })
}

struct Target<'a> {
    file: &'a File,
    path: &'a Path,
}

impl Target<'_> {
    fn write(&self, offset: u64, bytes: &[u8]) -> io::Result<()> {
        self.file.write_all_at(bytes, offset).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!(
                    "{}: write of {} bytes at {offset} failed: {error}",
                    self.path.display(),
                    bytes.len()
                ),
            )
        })
    }

    fn write_logical(&self, layout: &Layout, logical: u64, bytes: &[u8]) -> io::Result<()> {
        let chunk = layout
            .chunks()
            .find(|chunk| logical >= chunk.logical && logical < chunk.logical + chunk.length)
            .ok_or_else(|| invalid(format!("logical {logical} is in no chunk")))?;
        for stripe in &chunk.stripes {
            self.write(stripe.offset + (logical - chunk.logical), bytes)?;
        }
        Ok(())
    }

    /// Clear both ends of the device and every superblock copy's place, as
    /// btrfs-progs does, so no earlier signature survives there and a run
    /// that fails before its superblocks leaves none at all.
    fn wipe(&self, total: u64) -> io::Result<()> {
        let zeros = vec![0u8; WIPE as usize];
        let head = WIPE.min(total);
        self.write(0, zeros.get(..head as usize).unwrap_or_default())?;
        let tail = total.saturating_sub(WIPE).max(head);
        self.write(
            tail,
            zeros.get(..(total - tail) as usize).unwrap_or_default(),
        )?;
        for &offset in SUPER_MIRRORS {
            if offset + SUPER_SIZE as u64 <= total {
                self.write(offset, zeros.get(..SUPER_SIZE).unwrap_or_default())?;
            }
        }
        Ok(())
    }

    fn sync(&self) -> io::Result<()> {
        self.file
            .sync_all()
            .map_err(|error| context(self.path, error))
    }
}

/// A tree's items, its shape and where its blocks go.
#[derive(Clone, Debug)]
struct Built {
    items: Vec<Item>,
    shape: Shape,
    addresses: Vec<u64>,
}

impl Built {
    fn root(&self) -> u64 {
        self.addresses.last().copied().unwrap_or_default()
    }
}

/// The planned image: layout, data extents and every tree but the
/// checksums, which the data copy supplies.
struct Image {
    layout: Layout,
    extents: Vec<Extent>,
    ids: Ids,
    fs_items: BTreeMap<u64, Vec<Item>>,
    fs_parents: Vec<(u64, Option<crate::fs::Parent>)>,
    label: Vec<u8>,
    stamp: Time,
    trees: BTreeMap<u64, Built>,
    chunk_tree: Built,
    bytes_used: u64,
}

/// Where each tree's blocks go in the metadata chunk, in this order.
fn metadata_order(subvols: impl Iterator<Item = u64>) -> Vec<u64> {
    let mut order = vec![
        ROOT_TREE,
        EXTENT_TREE,
        DEV_TREE,
        FS_TREE,
        CSUM_TREE,
        UUID_TREE,
        FREE_SPACE_TREE,
        BLOCK_GROUP_TREE,
        DATA_RELOC_TREE,
    ];
    order.extend(subvols.filter(|id| *id != FS_TREE));
    order
}

impl Image {
    fn plan(
        fs: &Fs,
        source: &Source,
        ids: &Ids,
        total: u64,
        stamp: Time,
        label: &[u8],
    ) -> io::Result<Self> {
        let data = fs.data_bytes();
        let mut metadata = default_metadata(total);
        for _ in 0..SIZING_ROUNDS {
            let layout = Layout::plan(total, metadata, data)?;
            let chunks: Vec<(u64, u64)> = layout
                .data
                .iter()
                .map(|chunk| (chunk.logical, chunk.length))
                .collect();
            let extents = fs.place(&chunks, MAX_EXTENT)?;
            let fs_items = fs.items(source, &extents, GENERATION)?;
            let mut image = Self {
                layout,
                extents,
                ids: ids.clone(),
                fs_items,
                fs_parents: fs
                    .trees
                    .iter()
                    .map(|tree| (tree.id, tree.parent.clone()))
                    .collect(),
                label: label.to_vec(),
                stamp,
                trees: BTreeMap::new(),
                chunk_tree: Built {
                    items: Vec::new(),
                    shape: Shape::of(&[])?,
                    addresses: Vec::new(),
                },
                bytes_used: 0,
            };
            let placeholder = vec![0u32; (data / u64::from(SECTORSIZE)) as usize];
            match image.assemble(&placeholder)? {
                Ok(()) => return Ok(image),
                Err(needed) => {
                    metadata = round_up(needed + needed / 4, MIB);
                }
            }
        }
        Err(invalid("metadata did not settle into a chunk"))
    }

    /// Build every tree for `csums`, one per data sector in extent order.
    /// The inner `Err` is the metadata bytes needed when the chunk is too
    /// small for them.
    fn assemble(&mut self, csums: &[u32]) -> io::Result<Result<(), u64>> {
        let nodesize = u64::from(NODESIZE);
        // Trees whose items do not depend on where blocks go. The
        // subvolumes' are read in place from `fs_items`, never copied.
        let mut fixed: BTreeMap<u64, Vec<Item>> = BTreeMap::new();
        fixed.insert(DEV_TREE, self.dev_items());
        fixed.insert(CSUM_TREE, self.csum_items(csums)?);
        fixed.insert(UUID_TREE, self.uuid_items());
        fixed.insert(DATA_RELOC_TREE, self.data_reloc_items());
        let chunk_items = self.chunk_items();
        let chunk_shape = Shape::of(&chunk_items)?;
        let chunk_blocks = chunk_shape.blocks() as u64;
        if chunk_blocks * nodesize > self.layout.system.length {
            return Err(invalid("the chunk tree outgrew the system chunk"));
        }
        let chunk_addresses: Vec<u64> = (0..chunk_blocks)
            .map(|n| self.layout.system.logical + n * nodesize)
            .collect();
        self.chunk_tree = Built {
            items: chunk_items,
            shape: chunk_shape,
            addresses: chunk_addresses,
        };
        let order = metadata_order(self.fs_items.keys().copied());
        let mut shapes: HashMap<u64, Shape> = HashMap::new();
        for (id, items) in self.fs_items.iter().chain(&fixed) {
            shapes.insert(*id, Shape::of(items)?);
        }
        for id in [ROOT_TREE, EXTENT_TREE, FREE_SPACE_TREE, BLOCK_GROUP_TREE] {
            shapes.insert(id, Shape::of(&[])?);
        }
        for _ in 0..64 {
            let mut next = self.layout.metadata.logical;
            let mut addresses: HashMap<u64, Vec<u64>> = HashMap::new();
            for id in &order {
                let blocks = shapes.get(id).map(Shape::blocks).unwrap_or_default() as u64;
                addresses.insert(*id, (0..blocks).map(|n| next + n * nodesize).collect());
                next += blocks * nodesize;
            }
            let meta_used = next - self.layout.metadata.logical;
            if meta_used > self.layout.metadata.length {
                return Ok(Err(meta_used));
            }
            // Placement for every tree; only the derived trees' items are
            // built each pass, the fixed ones are moved in once settled.
            let mut trees: BTreeMap<u64, Built> = BTreeMap::new();
            for id in self.fs_items.keys().chain(fixed.keys()) {
                trees.insert(
                    *id,
                    Built {
                        items: Vec::new(),
                        shape: shapes.get(id).cloned().unwrap_or(Shape::of(&[])?),
                        addresses: addresses.get(id).cloned().unwrap_or_default(),
                    },
                );
            }
            let used = self.used(meta_used, chunk_blocks * nodesize);
            let derived = [
                (EXTENT_TREE, self.extent_items(&shapes, &addresses)),
                (FREE_SPACE_TREE, self.free_space_items(&used)),
                (BLOCK_GROUP_TREE, self.block_group_items(&used)),
            ];
            for (id, items) in derived {
                trees.insert(
                    id,
                    Built {
                        items,
                        shape: shapes.get(&id).cloned().unwrap_or(Shape::of(&[])?),
                        addresses: addresses.get(&id).cloned().unwrap_or_default(),
                    },
                );
            }
            let root_items = self.root_items(&trees)?;
            trees.insert(
                ROOT_TREE,
                Built {
                    items: root_items,
                    shape: shapes.get(&ROOT_TREE).cloned().unwrap_or(Shape::of(&[])?),
                    addresses: addresses.get(&ROOT_TREE).cloned().unwrap_or_default(),
                },
            );
            let mut settled = true;
            for id in [ROOT_TREE, EXTENT_TREE, FREE_SPACE_TREE, BLOCK_GROUP_TREE] {
                if let Some(built) = trees.get(&id) {
                    let shape = Shape::of(&built.items)?;
                    if Some(&shape) != shapes.get(&id) {
                        settled = false;
                        shapes.insert(id, shape);
                    }
                }
            }
            if settled {
                for (id, items) in std::mem::take(&mut fixed) {
                    if let Some(built) = trees.get_mut(&id) {
                        built.items = items;
                    }
                }
                self.bytes_used = used.iter().map(|(_, bytes)| bytes).sum();
                self.trees = trees;
                return Ok(Ok(()));
            }
        }
        Err(invalid("the extent tree did not settle"))
    }

    /// Bytes used in each chunk, by logical start: everything is packed
    /// from each chunk's start.
    fn used(&self, metadata: u64, system: u64) -> Vec<(u64, u64)> {
        let mut out = vec![
            (self.layout.system.logical, system),
            (self.layout.metadata.logical, metadata),
        ];
        for chunk in &self.layout.data {
            let bytes = self
                .extents
                .iter()
                .filter(|e| e.logical >= chunk.logical && e.logical < chunk.logical + chunk.length)
                .map(|e| e.len)
                .sum();
            out.push((chunk.logical, bytes));
        }
        out
    }

    fn chunk_items(&self) -> Vec<Item> {
        let mut items = vec![Item::new(
            Key::new(DEV_ITEMS, DEV_ITEM, 1),
            dev_item(
                self.layout.total,
                self.layout.device_used(),
                &self.ids.dev_uuid,
                &self.ids.fsid,
            ),
        )];
        for chunk in self.layout.chunks() {
            items.push(Item::new(
                Key::new(FIRST_CHUNK_TREE, CHUNK_ITEM, chunk.logical),
                chunk.encode(&self.ids.dev_uuid),
            ));
        }
        items.sort_by_key(|item| item.key);
        items
    }

    fn dev_items(&self) -> Vec<Item> {
        let mut items = vec![Item::new(
            Key::new(DEV_STATS, PERSISTENT_ITEM, 1),
            vec![0; 40],
        )];
        for chunk in self.layout.chunks() {
            for stripe in &chunk.stripes {
                items.push(Item::new(
                    Key::new(1, DEV_EXTENT, stripe.offset),
                    dev_extent(chunk.logical, chunk.length, &self.ids.chunk_tree_uuid),
                ));
            }
        }
        items.sort_by_key(|item| item.key);
        items
    }

    /// One EXTENT_CSUM item per run of adjacent extents in one chunk, split
    /// where an item would hold more than `MAX_CSUMS`.
    fn csum_items(&self, csums: &[u32]) -> io::Result<Vec<Item>> {
        let sector = u64::from(SECTORSIZE);
        let starts: Vec<u64> = self.layout.data.iter().map(|chunk| chunk.logical).collect();
        let mut runs: Vec<(u64, u64)> = Vec::new();
        for extent in &self.extents {
            match runs.last_mut() {
                Some((start, len))
                    if *start + *len == extent.logical && !starts.contains(&extent.logical) =>
                {
                    *len += extent.len;
                }
                _ => runs.push((extent.logical, extent.len)),
            }
        }
        let mut items = Vec::new();
        let mut values = csums.iter();
        for (start, len) in runs {
            let sectors = (len / sector) as usize;
            let mut done = 0;
            while done < sectors {
                let count = (sectors - done).min(MAX_CSUMS);
                let mut data = Vec::with_capacity(count * CSUM_SIZE);
                for _ in 0..count {
                    let value = values
                        .next()
                        .ok_or_else(|| invalid("fewer checksums than data sectors"))?;
                    data.extend_from_slice(&value.to_le_bytes());
                }
                items.push(Item::new(
                    Key::new(EXTENT_CSUM, EXTENT_CSUM_KEY, start + done as u64 * sector),
                    data,
                ));
                done += count;
            }
        }
        if values.next().is_some() {
            return Err(invalid("more checksums than data sectors"));
        }
        Ok(items)
    }

    fn uuid_items(&self) -> Vec<Item> {
        let mut items: Vec<Item> = self
            .fs_parents
            .iter()
            .zip(&self.ids.subvols)
            .map(|((id, _), uuid)| {
                let (low, high) = uuid.split_at(8);
                let key = Key::new(
                    u64::from_le_bytes(low.try_into().unwrap_or_default()),
                    UUID_KEY_SUBVOL,
                    u64::from_le_bytes(high.try_into().unwrap_or_default()),
                );
                Item::new(key, id.to_le_bytes().to_vec())
            })
            .collect();
        items.sort_by_key(|item| item.key);
        items.dedup_by_key(|item| item.key);
        items
    }

    fn dir_inode(&self) -> Vec<u8> {
        let mut data = Vec::new();
        Inode {
            generation: GENERATION,
            nbytes: u64::from(NODESIZE),
            nlink: 1,
            mode: S_IFDIR | 0o755,
            atime: self.stamp,
            ctime: self.stamp,
            mtime: self.stamp,
            otime: self.stamp,
            ..Inode::default()
        }
        .encode(&mut data);
        data
    }

    fn data_reloc_items(&self) -> Vec<Item> {
        let mut parent = Vec::new();
        inode_ref(0, b"..", &mut parent);
        vec![
            Item::new(Key::new(FIRST_FREE, INODE_ITEM, 0), self.dir_inode()),
            Item::new(Key::new(FIRST_FREE, INODE_REF, FIRST_FREE), parent),
        ]
    }

    fn extent_items(
        &self,
        shapes: &HashMap<u64, Shape>,
        addresses: &HashMap<u64, Vec<u64>>,
    ) -> Vec<Item> {
        let mut items = Vec::new();
        let mut tree_blocks = |owner: u64, shape: &Shape, at: &[u64]| {
            for (level, address) in shape.block_levels().into_iter().zip(at) {
                items.push(Item::new(
                    Key::new(*address, METADATA_ITEM, u64::from(level)),
                    metadata_extent(GENERATION, owner),
                ));
            }
        };
        tree_blocks(
            CHUNK_TREE,
            &self.chunk_tree.shape,
            &self.chunk_tree.addresses,
        );
        for (id, shape) in shapes {
            if let Some(at) = addresses.get(id) {
                tree_blocks(*id, shape, at);
            }
        }
        for extent in &self.extents {
            items.push(Item::new(
                Key::new(extent.logical, EXTENT_ITEM, extent.len),
                data_extent(GENERATION, extent.tree, extent.ino, extent.file_offset),
            ));
        }
        items.sort_by_key(|item| item.key);
        items
    }

    fn block_group_items(&self, used: &[(u64, u64)]) -> Vec<Item> {
        let mut items: Vec<Item> = self
            .layout
            .chunks()
            .map(|chunk| {
                let bytes = used
                    .iter()
                    .find(|(start, _)| *start == chunk.logical)
                    .map(|(_, bytes)| *bytes)
                    .unwrap_or_default();
                Item::new(
                    Key::new(chunk.logical, BLOCK_GROUP_ITEM, chunk.length),
                    block_group(bytes, chunk.flags),
                )
            })
            .collect();
        items.sort_by_key(|item| item.key);
        items
    }

    /// Each chunk is used from its start, so its free space is one extent
    /// from the end of what is used to the end of the chunk.
    fn free_space_items(&self, used: &[(u64, u64)]) -> Vec<Item> {
        let mut items = Vec::new();
        for chunk in self.layout.chunks() {
            let bytes = used
                .iter()
                .find(|(start, _)| *start == chunk.logical)
                .map(|(_, bytes)| *bytes)
                .unwrap_or_default();
            let free = chunk.length - bytes.min(chunk.length);
            items.push(Item::new(
                Key::new(chunk.logical, FREE_SPACE_INFO, chunk.length),
                free_space_info(u32::from(free > 0)),
            ));
            if free > 0 {
                items.push(Item::new(
                    Key::new(chunk.logical + bytes, FREE_SPACE_EXTENT, free),
                    Vec::new(),
                ));
            }
        }
        items.sort_by_key(|item| item.key);
        items
    }

    fn root_items(&self, trees: &BTreeMap<u64, Built>) -> io::Result<Vec<Item>> {
        let nodesize = u64::from(NODESIZE);
        let mut items = Vec::new();
        let root_inode = Inode {
            generation: 1,
            size: 3,
            nlink: 1,
            nbytes: nodesize,
            mode: S_IFDIR | 0o755,
            ..Inode::default()
        };
        let uuids: HashMap<u64, [u8; 16]> = self
            .fs_parents
            .iter()
            .map(|(id, _)| *id)
            .zip(self.ids.subvols.iter().copied())
            .collect();
        for (id, built) in trees {
            if *id == ROOT_TREE {
                continue;
            }
            let subvol = uuids.get(id).copied();
            let root = Root {
                inode: root_inode.clone(),
                generation: GENERATION,
                root_dirid: if subvol.is_some() || *id == DATA_RELOC_TREE {
                    FIRST_FREE
                } else {
                    0
                },
                bytenr: built.root(),
                bytes_used: built.shape.blocks() as u64 * nodesize,
                flags: 0,
                refs: 1,
                level: built.shape.level(),
                uuid: subvol.unwrap_or_default(),
                ctransid: if subvol.is_some() { GENERATION } else { 0 },
                otransid: if subvol.is_some() { GENERATION } else { 0 },
                ctime: if subvol.is_some() {
                    self.stamp
                } else {
                    Time::default()
                },
                otime: if subvol.is_some() {
                    self.stamp
                } else {
                    Time::default()
                },
            };
            items.push(Item::new(Key::new(*id, ROOT_ITEM, 0), root.encode()));
        }
        for (id, parent) in &self.fs_parents {
            if let Some(parent) = parent {
                let data = root_ref(parent.dirid, parent.index, &parent.name);
                items.push(Item::new(
                    Key::new(parent.tree, ROOT_REF, *id),
                    data.clone(),
                ));
                items.push(Item::new(Key::new(*id, ROOT_BACKREF, parent.tree), data));
            }
        }
        // The root tree's directory, naming the default subvolume.
        let mut default_ref = Vec::new();
        inode_ref(0, b"default", &mut default_ref);
        items.push(Item::new(
            Key::new(FS_TREE, INODE_REF, ROOT_TREE_DIR),
            default_ref,
        ));
        items.push(Item::new(
            Key::new(ROOT_TREE_DIR, INODE_ITEM, 0),
            self.dir_inode(),
        ));
        let mut parent = Vec::new();
        inode_ref(0, b"..", &mut parent);
        items.push(Item::new(
            Key::new(ROOT_TREE_DIR, INODE_REF, ROOT_TREE_DIR),
            parent,
        ));
        let mut default = Vec::new();
        dir_item(
            Key::new(FS_TREE, ROOT_ITEM, u64::MAX),
            GENERATION,
            FT_DIR,
            b"default",
            &mut default,
        );
        items.push(Item::new(
            Key::new(ROOT_TREE_DIR, DIR_ITEM, crc32c::name_hash(b"default")),
            default,
        ));
        items.sort_by_key(|item| item.key);
        if items.windows(2).any(|pair| match pair {
            [a, b] => a.key == b.key,
            _ => false,
        }) {
            return Err(invalid("duplicate root tree key"));
        }
        Ok(items)
    }

    /// Copy every file's data into its extents, returning one checksum per
    /// data sector in extent order.
    fn copy_data(&self, fs: &Fs, target: &Target<'_>) -> io::Result<Vec<u32>> {
        let sector = SECTORSIZE as usize;
        let mut csums = Vec::new();
        let mut buffer = vec![0u8; COPY_BUFFER];
        let mut extents = self.extents.iter().peekable();
        for file in &fs.files {
            let mut source = File::open(&file.path).map_err(|error| context(&file.path, error))?;
            let mut copied = 0u64;
            while let Some(extent) = extents.next_if(|e| e.tree == file.tree && e.ino == file.ino) {
                let mut done = 0u64;
                while done < extent.len {
                    let piece = (extent.len - done).min(COPY_BUFFER as u64) as usize;
                    let chunk = buffer
                        .get_mut(..piece)
                        .ok_or_else(|| invalid("copy buffer too small"))?;
                    let wanted = file.size.saturating_sub(copied).min(piece as u64) as usize;
                    let (data, pad) = chunk.split_at_mut(wanted);
                    source
                        .read_exact(data)
                        .map_err(|error| match error.kind() {
                            io::ErrorKind::UnexpectedEof => invalid(format!(
                                "{} shrank while it was being copied",
                                file.path.display()
                            )),
                            _ => context(&file.path, error),
                        })?;
                    pad.fill(0);
                    copied += wanted as u64;
                    for block in chunk.chunks(sector) {
                        csums.push(crc32c::checksum(block));
                    }
                    target.write(extent.logical + done, chunk)?;
                    done += piece as u64;
                }
            }
            let mut probe = [0u8; 1];
            if copied != file.size || source.read(&mut probe)? != 0 {
                return Err(invalid(format!(
                    "{} changed size while it was being copied",
                    file.path.display()
                )));
            }
        }
        Ok(csums)
    }

    /// Every metadata block, by logical address, for the final checksums.
    /// The trees are rebuilt with them, and must land where they were planned.
    fn metadata(&mut self, csums: &[u32]) -> io::Result<Vec<(u64, Vec<u8>)>> {
        let planned: BTreeMap<u64, Vec<u64>> = self
            .trees
            .iter()
            .map(|(id, built)| (*id, built.addresses.clone()))
            .collect();
        let planned_used = self.bytes_used;
        if self.assemble(csums)?.is_err() || self.bytes_used != planned_used {
            return Err(invalid("metadata changed once checksums were filled in"));
        }
        let mut out = Vec::new();
        let header = |owner| Header {
            fsid: self.ids.fsid,
            chunk_tree_uuid: self.ids.chunk_tree_uuid,
            generation: GENERATION,
            owner,
        };
        let chunk = &self.chunk_tree;
        let blocks = chunk
            .shape
            .serialize(&chunk.items, &chunk.addresses, &header(CHUNK_TREE))?;
        out.extend(chunk.addresses.iter().copied().zip(blocks));
        for (id, built) in &self.trees {
            if planned.get(id) != Some(&built.addresses) {
                return Err(invalid("tree blocks moved once checksums were filled in"));
            }
            let items = self.fs_items.get(id).unwrap_or(&built.items);
            let blocks = built
                .shape
                .serialize(items, &built.addresses, &header(*id))?;
            out.extend(built.addresses.iter().copied().zip(blocks));
        }
        Ok(out)
    }

    fn tree(&self, id: u64) -> io::Result<&Built> {
        self.trees
            .get(&id)
            .ok_or_else(|| invalid(format!("tree {id} was not built")))
    }

    /// The superblock copy at `offset`.
    fn superblock(&self, offset: u64) -> io::Result<Vec<u8>> {
        let root = self.tree(ROOT_TREE)?;
        let chunk_root = self.chunk_tree.root();
        let mut out = Vec::with_capacity(SUPER_SIZE);
        out.extend_from_slice(&[0; 32]);
        out.extend_from_slice(&self.ids.fsid);
        put64(&mut out, offset);
        put64(&mut out, SUPER_FLAG_WRITTEN);
        out.extend_from_slice(SUPER_MAGIC);
        put64(&mut out, GENERATION);
        put64(&mut out, root.root());
        put64(&mut out, chunk_root);
        put64(&mut out, 0); // log_root
        put64(&mut out, 0); // log_root_transid
        put64(&mut out, self.layout.total);
        put64(&mut out, self.bytes_used);
        put64(&mut out, ROOT_TREE_DIR);
        put64(&mut out, 1); // num_devices
        put32(&mut out, SECTORSIZE);
        put32(&mut out, NODESIZE);
        put32(&mut out, NODESIZE); // leafsize
        put32(&mut out, SECTORSIZE); // stripesize
        let system = self.layout.system.encode(&self.ids.dev_uuid);
        let mut sys_array = Vec::new();
        Key::new(FIRST_CHUNK_TREE, CHUNK_ITEM, self.layout.system.logical).encode(&mut sys_array);
        sys_array.extend_from_slice(&system);
        put32(&mut out, sys_array.len() as u32);
        put64(&mut out, GENERATION); // chunk_root_generation
        put64(&mut out, 0); // compat_flags
        put64(&mut out, COMPAT_RO);
        put64(&mut out, INCOMPAT);
        put16(&mut out, 0); // csum_type: crc32c
        put8(&mut out, root.shape.level());
        put8(&mut out, self.chunk_tree.shape.level());
        put8(&mut out, 0); // log_root_level
        out.extend_from_slice(&dev_item(
            self.layout.total,
            self.layout.device_used(),
            &self.ids.dev_uuid,
            &self.ids.fsid,
        ));
        let mut label = self.label.clone();
        label.resize(256, 0);
        out.extend_from_slice(&label);
        put64(&mut out, 0); // cache_generation
        put64(&mut out, GENERATION); // uuid_tree_generation
        out.extend_from_slice(&[0; 16]); // metadata_uuid
        out.resize(SYS_CHUNK_ARRAY, 0);
        sys_array.resize(2048, 0);
        out.extend_from_slice(&sys_array);
        // Backup root 0 describes this generation; the others stay empty.
        let extent = self.tree(EXTENT_TREE)?;
        let fs = self.tree(FS_TREE)?;
        let dev = self.tree(DEV_TREE)?;
        let csum = self.tree(CSUM_TREE)?;
        for (address, _) in [
            (root.root(), ()),
            (chunk_root, ()),
            (extent.root(), ()),
            (fs.root(), ()),
            (dev.root(), ()),
            (csum.root(), ()),
        ] {
            put64(&mut out, address);
            put64(&mut out, GENERATION);
        }
        put64(&mut out, self.layout.total);
        put64(&mut out, self.bytes_used);
        put64(&mut out, 1);
        out.extend_from_slice(&[0; 32]);
        for level in [
            root.shape.level(),
            self.chunk_tree.shape.level(),
            extent.shape.level(),
            fs.shape.level(),
            dev.shape.level(),
            csum.shape.level(),
        ] {
            put8(&mut out, level);
        }
        out.resize(SUPER_SIZE, 0);
        seal(&mut out)?;
        Ok(out)
    }
}

/// Where the superblock's system chunk array begins.
pub const SYS_CHUNK_ARRAY: usize = 811;

/// Parse a size: bytes, or a number with a binary K, M, G, T, P or E
/// suffix, as btrfs-progs reads `--byte-count`.
pub fn parse_size(text: &str) -> io::Result<u64> {
    let bad = || invalid(format!("{text:?} is not a size"));
    let (digits, shift) = match text.as_bytes().last() {
        Some(b'k' | b'K') => (text.get(..text.len() - 1), 10),
        Some(b'm' | b'M') => (text.get(..text.len() - 1), 20),
        Some(b'g' | b'G') => (text.get(..text.len() - 1), 30),
        Some(b't' | b'T') => (text.get(..text.len() - 1), 40),
        Some(b'p' | b'P') => (text.get(..text.len() - 1), 50),
        Some(b'e' | b'E') => (text.get(..text.len() - 1), 60),
        _ => (Some(text), 0),
    };
    let digits = digits.ok_or_else(bad)?;
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(bad());
    }
    let value: u64 = digits.parse().map_err(|_| bad())?;
    value.checked_mul(1u64 << shift).ok_or_else(bad)
}

/// A short account of the new filesystem, for stdout.
pub fn report(made: &Made, label: &[u8], out: &mut dyn Write) -> io::Result<()> {
    let profile = |flags: u64| if flags & BG_DUP != 0 { "DUP" } else { "single" };
    writeln!(out, "td-btrfs mkfs")?;
    writeln!(
        out,
        "Label:              {}",
        String::from_utf8_lossy(label)
    )?;
    writeln!(out, "UUID:               {}", uuid_text(&made.fsid))?;
    writeln!(out, "Filesystem size:    {} bytes", made.total)?;
    writeln!(
        out,
        "Data:               {} {} bytes",
        profile(BG_DATA),
        made.layout.data.iter().map(|c| c.length).sum::<u64>()
    )?;
    writeln!(
        out,
        "Metadata:           {} {} bytes",
        profile(made.layout.metadata.flags),
        made.layout.metadata.length
    )?;
    writeln!(
        out,
        "System:             {} {} bytes",
        profile(made.layout.system.flags),
        made.layout.system.length
    )?;
    writeln!(out, "Used:               {} bytes", made.bytes_used)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn sizes_parse_as_btrfs_progs_reads_them() {
        assert_eq!(parse_size("4096").unwrap(), 4096);
        assert_eq!(parse_size("256M").unwrap(), 256 << 20);
        assert_eq!(parse_size("2t").unwrap(), 2 << 40);
        for bad in ["", "M", "1.5G", "-1", "12Q", "99999999999999999999", "16E"] {
            assert!(parse_size(bad).is_err(), "{bad}");
        }
    }
}

//! Deterministic EROFS image writer for the control-plane packer.
//!
//! Linux 7.1.4 format: 4096-byte blocks, compact 32-byte inodes, metadata at
//! block 1. Directories and symlinks use FLAT_PLAIN. Nonempty regular files
//! use COMPRESSED_FULL with the extent-map advice and 16-byte extent records.
//! Each logical 128-KiB extent is an independent zstd frame, or raw bytes if
//! compression would expand it. Extent payloads are packed without padding;
//! inode IDs address 32-byte slots, including each file's extent metadata.
//! The superblock advertises zstd and its 128-KiB maximum window.
//!
//! Names and symlink targets retain raw bytes. Sorted traversal, root ownership,
//! zero timestamps and deterministic compression produce identical images from
//! identical trees. Modes and hard-link identity are preserved. The complete
//! tree and output are held in memory; compact inodes limit files to <4 GiB.
//! Unit tests round-trip the format; the kernel and integration boot oracles
//! prove real-kernel mount and compressed data readback.

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::MetadataExt;
use std::path::Path;

/// erofs block size. 4096 = `1 << blkszbits` with `blkszbits = 12`.
const BLOCK_SIZE: usize = 4096;
const BLKSZBITS: u8 = 12;
/// `EROFS_SUPER_MAGIC_V1`.
const MAGIC: u32 = 0xE0F5_E1E2;
/// `EROFS_SUPER_OFFSET` — the superblock starts 1024 bytes into block 0.
const SUPER_OFFSET: usize = 1024;
/// Compact on-disk inode size (`1 << islotbits`, islotbits = 5).
const INODE_SIZE: usize = 32;
/// On-disk directory entry size.
const DIRENT_SIZE: usize = 12;
/// The inode table starts at block 1 (block 0 = boot area + superblock).
const META_BLKADDR: u32 = 1;
const EXTENT_SIZE: usize = 128 * 1024;
const EXTENT_RECORDS_OFFSET: usize = 48;
const EXTENT_RECORD_SIZE: usize = 16;
const EXTENT_FMT_ZSTD: u32 = 4 << 28;
const I_FORMAT_COMPRESSED_FULL: u64 = 1 << 1;
const ADVISE_EXTENTS_16B: u64 = 0x1 | (2 << 1);
const SB_EXTSLOTS: u8 = 1;
const INCOMPAT_COMPR_CFGS: u32 = 0x2;
const COMPRESSION_ZSTD: u32 = 3;
const ZSTD_CFG_LEN: u64 = 6;
const ZSTD_WINDOWLOG: u8 = 7;

// mode type bits (S_IFMT family) — std does not expose these as constants.
const S_IFMT: u32 = 0o170000;
const S_IFDIR: u32 = 0o040000;
const S_IFREG: u32 = 0o100000;
const S_IFLNK: u32 = 0o120000;
const S_IFCHR: u32 = 0o020000;
const S_IFBLK: u32 = 0o060000;
const S_IFIFO: u32 = 0o010000;
const S_IFSOCK: u32 = 0o140000;

// erofs dirent file_type values (mirror the kernel FT_* / EROFS_FT_*).
const FT_REG: u8 = 1;
const FT_DIR: u8 = 2;
const FT_CHR: u8 = 3;
const FT_BLK: u8 = 4;
const FT_FIFO: u8 = 5;
const FT_SOCK: u8 = 6;
const FT_SYMLINK: u8 = 7;

/// The per-node body captured while walking the staged tree.
enum Body {
    /// A directory: its sorted `(raw-name-bytes, child-index)` entries plus the parent's
    /// index (for the `..` dirent; the root's parent is itself). Names are stored as raw
    /// bytes — erofs and Linux permit any non-NUL byte sequence, not just UTF-8.
    Dir {
        children: Vec<(Vec<u8>, usize)>,
        parent: usize,
    },
    File {
        data: Vec<u8>,
    },
    Symlink {
        target: Vec<u8>,
    },
    /// FIFO / socket / char / block device — no data blocks; `rdev` is the encoded
    /// device number (0 for FIFO/socket).
    Special {
        rdev: u32,
    },
}

/// One inode in traversal order. Its nid is assigned after sizing extent maps.
struct Node {
    /// Full mode: an `S_IF*` type bit OR the permission bits.
    mode: u16,
    body: Body,
}

fn ceil_div(a: usize, b: usize) -> usize {
    a.div_ceil(b)
}

/// Pad `out` with zero bytes up to the next `BLOCK_SIZE` boundary.
fn pad_to_block(out: &mut Vec<u8>) {
    let rem = out.len() % BLOCK_SIZE;
    if rem != 0 {
        out.resize(out.len() + (BLOCK_SIZE - rem), 0);
    }
}

/// The erofs dirent `file_type` for a full mode.
fn file_type_of(mode: u16) -> u8 {
    match u32::from(mode) & S_IFMT {
        S_IFDIR => FT_DIR,
        S_IFREG => FT_REG,
        S_IFLNK => FT_SYMLINK,
        S_IFCHR => FT_CHR,
        S_IFBLK => FT_BLK,
        S_IFIFO => FT_FIFO,
        S_IFSOCK => FT_SOCK,
        _ => 0,
    }
}

/// Linux `new_encode_dev`: pack a raw `st_rdev` into the 32-bit form erofs stores in
/// `i_u.rdev`. Decodes major/minor with the glibc `gnu_dev_*` bit layout, then re-packs
/// per `include/linux/kdev_t.h`.
fn new_encode_dev(rdev: u64) -> u32 {
    let major = ((rdev >> 8) & 0xfff) | ((rdev >> 32) & !0xfffu64);
    let minor = (rdev & 0xff) | ((rdev >> 12) & !0xffu64);
    ((minor & 0xff) | (major << 8) | ((minor & !0xffu64) << 12)) as u32
}

/// Walk the staged tree at `path` (a child of the image root), appending every node to
/// `nodes` in a deterministic pre-order (sorted children) and returning this node's
/// index. `parent` is the index of the containing directory (the root passes its own
/// index). Reserving the index before recursing keeps a directory's nid below its
/// children's — the order is irrelevant to correctness, only that it is deterministic.
///
/// `hardlinks` maps a source `(st_dev, st_ino)` to the nid of the inode already emitted
/// for it, so multiply-linked regular files share ONE inode (and its data) rather than
/// being duplicated per pathname; each such file's on-disk nlink is derived later from
/// the number of directory references to it.
fn walk(
    path: &Path,
    parent: usize,
    nodes: &mut Vec<Node>,
    hardlinks: &mut HashMap<(u64, u64), usize>,
) -> io::Result<usize> {
    let meta = fs::symlink_metadata(path)?;
    let ft = meta.file_type();

    // Hard-link dedup: a regular file with more than one link that we have already packed
    // is referenced by its existing nid — no second inode, no duplicated data.
    let hl_key = if ft.is_file() && meta.nlink() > 1 {
        let key = (meta.dev(), meta.ino());
        if let Some(&existing) = hardlinks.get(&key) {
            return Ok(existing);
        }
        Some(key)
    } else {
        None
    };

    let my_idx = nodes.len();
    // Reserve this node's slot with a placeholder; filled in after recursion.
    nodes.push(Node {
        mode: 0,
        body: Body::Special { rdev: 0 },
    });

    let (mode, body) = if ft.is_dir() {
        let perm = (meta.mode() & 0o7777) as u16;
        let mut names: Vec<OsString> = Vec::new();
        for ent in fs::read_dir(path)? {
            names.push(ent?.file_name());
        }
        // Deterministic order; sort by raw bytes (the same order the dirents use).
        names.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
        let mut children: Vec<(Vec<u8>, usize)> = Vec::with_capacity(names.len());
        for name in names {
            let child_idx = walk(&path.join(&name), my_idx, nodes, hardlinks)?;
            children.push((name.as_bytes().to_vec(), child_idx));
        }
        (S_IFDIR as u16 | perm, Body::Dir { children, parent })
    } else if ft.is_symlink() {
        // erofs/Linux allow any non-NUL byte sequence for a symlink target.
        let target = fs::read_link(path)?.into_os_string().into_vec();
        // A symlink target must be non-empty and fit a single block (POSIX targets are
        // well under 4096); FLAT_PLAIN stores it as the inode's data.
        if target.is_empty() || target.len() > BLOCK_SIZE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "symlink target at {} is empty or over one block",
                    path.display()
                ),
            ));
        }
        (S_IFLNK as u16 | 0o777, Body::Symlink { target })
    } else if ft.is_file() {
        let data = fs::read(path)?;
        // Compact inodes carry a 32-bit i_size; a >=4 GiB file would need the extended
        // (64-byte) inode. Refuse rather than truncate silently.
        if data.len() > u32::MAX as usize {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "file {} exceeds the 4 GiB compact-inode size limit",
                    path.display()
                ),
            ));
        }
        let perm = (meta.mode() & 0o7777) as u16;
        (S_IFREG as u16 | perm, Body::File { data })
    } else {
        // FIFO / socket / char / block device.
        let raw = meta.mode();
        let perm = (raw & 0o7777) as u16;
        let ifmt = raw & S_IFMT;
        let rdev = if ifmt == S_IFCHR || ifmt == S_IFBLK {
            new_encode_dev(meta.rdev())
        } else {
            0
        };
        match ifmt {
            S_IFCHR | S_IFBLK | S_IFIFO | S_IFSOCK => {
                ((ifmt as u16) | perm, Body::Special { rdev })
            }
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "{}: unsupported file type for an erofs image",
                        path.display()
                    ),
                ))
            }
        }
    };

    match nodes.get_mut(my_idx) {
        Some(slot) => {
            slot.mode = mode;
            slot.body = body;
        }
        None => return Err(io::Error::other("internal: reserved inode slot vanished")),
    }
    // Record the inode so any further hard link to the same source file reuses this nid.
    if let Some(key) = hl_key {
        hardlinks.insert(key, my_idx);
    }
    Ok(my_idx)
}

/// Serialize one directory's data as a sequence of 4096-byte dirent blocks. Every entry
/// (including the sorted `.`/`..`) is `[dirents][names][NUL pad]`; entries are packed
/// greedily into blocks in global sort order. `nids` maps a node index to its nid.
fn build_dir_data(
    idx: usize,
    parent: usize,
    children: &[(Vec<u8>, usize)],
    nodes: &[Node],
    nids: &[u64],
) -> io::Result<Vec<u8>> {
    let nid_of = |i: usize| {
        nids.get(i)
            .copied()
            .ok_or_else(|| io::Error::other("internal: node without a nid"))
    };
    // (name-bytes, nid, file_type)
    let mut entries: Vec<(Vec<u8>, u64, u8)> = Vec::with_capacity(children.len() + 2);
    entries.push((b".".to_vec(), nid_of(idx)?, FT_DIR));
    entries.push((b"..".to_vec(), nid_of(parent)?, FT_DIR));
    for (name, cidx) in children {
        let ft = match nodes.get(*cidx) {
            Some(n) => file_type_of(n.mode),
            None => return Err(io::Error::other("internal: dangling child index")),
        };
        entries.push((name.clone(), nid_of(*cidx)?, ft));
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));

    let mut out: Vec<u8> = Vec::new();
    let mut i = 0usize;
    while i < entries.len() {
        // Greedily choose how many entries fit in this block: k dirents (12 bytes each)
        // plus the sum of their name lengths must fit in BLOCK_SIZE.
        let mut count = 0usize;
        let mut names_len = 0usize;
        while let Some((name, _, _)) = entries.get(i + count) {
            let k = count + 1;
            if k * DIRENT_SIZE + names_len + name.len() > BLOCK_SIZE {
                break;
            }
            names_len += name.len();
            count += 1;
        }
        if count == 0 {
            // A single name that cannot fit one block — impossible for POSIX names
            // (<=255 bytes), but fail loud rather than loop forever.
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "directory entry name does not fit an erofs dirent block",
            ));
        }
        let mut block: Vec<u8> = Vec::with_capacity(BLOCK_SIZE);
        let mut names: Vec<u8> = Vec::with_capacity(names_len);
        let mut nameoff = (count * DIRENT_SIZE) as u16;
        for j in 0..count {
            let (name, nid, ft) = match entries.get(i + j) {
                Some(e) => e,
                None => return Err(io::Error::other("internal: dirent index slipped")),
            };
            block.extend_from_slice(&nid.to_le_bytes());
            block.extend_from_slice(&nameoff.to_le_bytes());
            block.push(*ft);
            block.push(0); // reserved
            nameoff = nameoff.wrapping_add(name.len() as u16);
            names.extend_from_slice(name);
        }
        block.extend_from_slice(&names);
        block.resize(BLOCK_SIZE, 0); // NUL pad — terminates the last name via strnlen
        out.extend_from_slice(&block);
        i += count;
    }
    Ok(out)
}

/// Append VALUE as the low `n` bytes little-endian (n is 1/2/4/8). Keeps superblock and
/// inode serialization to ordered appends — no offset indexing.
fn put(out: &mut Vec<u8>, value: u64, n: usize) {
    let le = value.to_le_bytes();
    out.extend_from_slice(le.get(..n).unwrap_or(&le));
}

/// One compressed extent as stored: its encoded length field (length plus format
/// bits) and its bytes.
struct Extent {
    plen: u32,
    bytes: Vec<u8>,
}

/// Compress every extent of every regular file, in parallel. Each extent is its own
/// frame, so the result does not depend on scheduling.
fn compress_extents(nodes: &[Node]) -> io::Result<Vec<Vec<Extent>>> {
    let mut work: Vec<(usize, usize)> = Vec::new();
    for (idx, n) in nodes.iter().enumerate() {
        if let Body::File { data } = &n.body {
            for k in 0..data.len().div_ceil(EXTENT_SIZE) {
                work.push((idx, k));
            }
        }
    }
    let threads = std::thread::available_parallelism()
        .map_or(1, |n| n.get())
        .min(work.len().max(1));
    let next = std::sync::atomic::AtomicUsize::new(0);
    let results: Vec<io::Result<Vec<(usize, Extent)>>> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                scope.spawn(|| {
                    let mut done = Vec::new();
                    loop {
                        let w = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let Some(&(idx, k)) = work.get(w) else {
                            return Ok(done);
                        };
                        let chunk = match nodes.get(idx).map(|n| &n.body) {
                            Some(Body::File { data }) => {
                                let end = data.len().min((k + 1) * EXTENT_SIZE);
                                data.get(k * EXTENT_SIZE..end).unwrap_or(&[])
                            }
                            _ => return Err(io::Error::other("internal: extent of a non-file")),
                        };
                        done.push((w, encode_extent(chunk)?));
                    }
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| {
                h.join()
                    .unwrap_or_else(|_| Err(io::Error::other("erofs: compression thread failed")))
            })
            .collect()
    });
    let mut slots: Vec<Option<Extent>> = (0..work.len()).map(|_| None).collect();
    for r in results {
        for (w, e) in r? {
            if let Some(slot) = slots.get_mut(w) {
                *slot = Some(e);
            }
        }
    }
    let mut per_node: Vec<Vec<Extent>> = (0..nodes.len()).map(|_| Vec::new()).collect();
    for (&(idx, _), slot) in work.iter().zip(slots) {
        let e = slot.ok_or_else(|| io::Error::other("internal: extent not compressed"))?;
        per_node
            .get_mut(idx)
            .ok_or_else(|| io::Error::other("internal: extent owner"))?
            .push(e);
    }
    Ok(per_node)
}

/// A zstd frame when it is smaller than the extent, otherwise the bytes as they are
/// (format 0, which the kernel copies without decoding).
fn encode_extent(chunk: &[u8]) -> io::Result<Extent> {
    let frame = crate::zstd::compress(chunk)?;
    let len = |n: usize| u32::try_from(n).map_err(|_| io::Error::other("internal: extent length"));
    if frame.len() < chunk.len() {
        Ok(Extent {
            plen: len(frame.len())? | EXTENT_FMT_ZSTD,
            bytes: frame,
        })
    } else {
        Ok(Extent {
            plen: len(chunk.len())?,
            bytes: chunk.to_vec(),
        })
    }
}

/// Inode slots a node occupies: one compact inode, plus for a compressed file its
/// map header, alignment and one extent record per extent.
fn slots_for(n: &Node) -> usize {
    match &n.body {
        Body::File { data } if !data.is_empty() => {
            let records = data.len().div_ceil(EXTENT_SIZE);
            ceil_div(
                EXTENT_RECORDS_OFFSET + records * EXTENT_RECORD_SIZE,
                INODE_SIZE,
            )
        }
        _ => 1,
    }
}

/// Build the complete erofs image bytes for the staged tree rooted at `root`.
pub fn build_image(root: &Path) -> io::Result<Vec<u8>> {
    let root_meta = fs::symlink_metadata(root)?;
    if !root_meta.file_type().is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("erofs image root {} is not a directory", root.display()),
        ));
    }
    let mut nodes: Vec<Node> = Vec::new();
    let mut hardlinks: HashMap<(u64, u64), usize> = HashMap::new();
    // The root's parent is itself (its `..` points back to the root nid, 0).
    walk(root, 0, &mut nodes, &mut hardlinks)?;
    let ninodes = nodes.len();

    // Each node's nid is its first inode slot; a compressed file's extent records
    // follow its inode in the slots after it.
    let mut nids: Vec<u64> = Vec::with_capacity(ninodes);
    let mut total_slots = 0usize;
    for n in &nodes {
        nids.push(total_slots as u64);
        total_slots += slots_for(n);
    }

    // Pass 1: the plain data of directories and symlinks (directories become dirent
    // blocks), and the compressed extents of regular files.
    let mut plain: Vec<Vec<u8>> = Vec::with_capacity(ninodes);
    for (idx, n) in nodes.iter().enumerate() {
        let d = match &n.body {
            Body::Dir { children, parent } => {
                build_dir_data(idx, *parent, children, &nodes, &nids)?
            }
            Body::Symlink { target } => target.clone(),
            Body::File { .. } | Body::Special { .. } => Vec::new(),
        };
        // i_size is a 32-bit field in the compact inode. Files are checked at read time;
        // this also covers a directory whose dirent blocks would exceed 4 GiB.
        if d.len() > u32::MAX as usize {
            return Err(io::Error::other(
                "erofs: inode data exceeds the 32-bit i_size limit",
            ));
        }
        plain.push(d);
    }
    let extents = compress_extents(&nodes)?;

    // How many directory entries across the whole tree reference each node. For a
    // hard-linked regular file this is its in-image link count; a directory's nlink is
    // computed separately (2 + subdir count), so its reference count is unused.
    let mut ref_count: Vec<usize> = vec![0; ninodes];
    for n in &nodes {
        if let Body::Dir { children, .. } = &n.body {
            for (_, cidx) in children {
                if let Some(c) = ref_count.get_mut(*cidx) {
                    *c += 1;
                }
            }
        }
    }

    // Layout: block 0 = superblock; then the inode table; then plain data blocks; then
    // the packed extent region, byte-addressed and unaligned between extents.
    let meta_blocks = ceil_div(total_slots * INODE_SIZE, BLOCK_SIZE);
    let data_start = META_BLKADDR as usize + meta_blocks;

    // Pass 2: assign each plain node's starting data block (FLAT_PLAIN, contiguous, in
    // nid order) and every node's explicit nlink (a directory's is 2 + its subdirs).
    let mut startblk: Vec<u32> = Vec::with_capacity(ninodes);
    let mut nlink: Vec<u16> = Vec::with_capacity(ninodes);
    let mut cursor = data_start as u32;
    for (idx, n) in nodes.iter().enumerate() {
        let nblk = plain.get(idx).map_or(0, |d| ceil_div(d.len(), BLOCK_SIZE));
        if nblk == 0 {
            startblk.push(0);
        } else {
            // `blocks_lo` and `startblk_lo` are 32-bit block addresses. Fail loud if the
            // physical layout would exceed that (16 TiB) rather than silently wrapping and
            // overlapping later nodes onto earlier data.
            let next = u32::try_from(nblk)
                .ok()
                .and_then(|n| cursor.checked_add(n))
                .ok_or_else(|| {
                    io::Error::other("erofs image exceeds the 32-bit block-address capacity")
                })?;
            startblk.push(cursor);
            cursor = next;
        }
        let links = match &n.body {
            // A directory links to itself (`.`), its parent links to it, and each
            // subdirectory's `..` links back — so nlink = 2 + subdirectory count.
            Body::Dir { children, .. } => {
                let subdirs = children
                    .iter()
                    .filter(|(_, cidx)| {
                        nodes
                            .get(*cidx)
                            .is_some_and(|c| u32::from(c.mode) & S_IFMT == S_IFDIR)
                    })
                    .count();
                2 + subdirs
            }
            // Non-directories: the number of in-image directory references (>1 only for
            // hard-linked files). Every packed node has at least its one parent entry.
            _ => ref_count.get(idx).copied().unwrap_or(1).max(1),
        };
        // i_nb (nlink) is a 16-bit field in the compact inode; reject rather than clamp.
        let links = u16::try_from(links)
            .map_err(|_| io::Error::other("erofs: link count exceeds the 16-bit i_nb limit"))?;
        nlink.push(links);
    }
    let packed_start = u64::from(cursor) * BLOCK_SIZE as u64;
    let packed_len: u64 = extents.iter().flatten().map(|e| e.bytes.len() as u64).sum();
    let total_blocks = u32::try_from((packed_start + packed_len).div_ceil(BLOCK_SIZE as u64))
        .map_err(|_| io::Error::other("erofs image exceeds the 32-bit block-address capacity"))?;

    // ---- assemble ----
    let mut img: Vec<u8> = Vec::with_capacity(total_blocks as usize * BLOCK_SIZE);

    // Block 0: [0..1024] boot area, the 144-byte superblock (one extension slot, so
    // its whole struct is in use), then the zstd configuration record.
    img.resize(SUPER_OFFSET, 0);
    put(&mut img, MAGIC as u64, 4); //   0 magic
    put(&mut img, 0, 4); //               4 checksum (feature off)
    put(&mut img, 0, 4); //               8 feature_compat
    img.push(BLKSZBITS); //              12 blkszbits
    img.push(SB_EXTSLOTS); //            13 sb_extslots (superblock = 128 + 16)
    put(&mut img, 0, 2); //              14 rootnid_2b (root nid = 0)
    put(&mut img, ninodes as u64, 8); // 16 inos
    put(&mut img, 0, 8); //              24 epoch
    put(&mut img, 0, 4); //              32 fixed_nsec
    put(&mut img, total_blocks as u64, 4); // 36 blocks_lo
    put(&mut img, META_BLKADDR as u64, 4); // 40 meta_blkaddr
    put(&mut img, 0, 4); //              44 xattr_blkaddr
    img.resize(img.len() + 16, 0); //    48 uuid[16]
    img.resize(img.len() + 16, 0); //    64 volume_name[16]
    put(&mut img, INCOMPAT_COMPR_CFGS.into(), 4); // 80 feature_incompat
    put(&mut img, 1 << COMPRESSION_ZSTD, 2); // 84 available_compr_algs
    put(&mut img, 0, 2); //              86 extra_devices
    put(&mut img, 0, 2); //              88 devt_slotoff
    img.push(0); //                      90 dirblkbits (0 = use blkszbits; non-zero rejected)
    img.push(0); //                      91 xattr_prefix_count
    put(&mut img, 0, 4); //              92 xattr_prefix_start
    put(&mut img, 0, 8); //              96 packed_nid
    img.push(0); //                     104 xattr_filter_reserved
    img.push(0); //                     105 ishare_xattr_prefix_id
    img.resize(img.len() + 2, 0); //    106 reserved[2]
    put(&mut img, 0, 4); //             108 build_time
    put(&mut img, 0, 8); //             112 rootnid_8b (48BIT off -> unused)
    put(&mut img, 0, 8); //             120 reserved2
    put(&mut img, 0, 8); //             128 metabox_nid
    put(&mut img, 0, 8); //             136 reserved3 (-> 144)
                         // Compression configurations follow the superblock, one per available algorithm
                         // in bit order, each a 2-byte length then the struct (`z_erofs_zstd_cfgs`).
    put(&mut img, ZSTD_CFG_LEN, 2);
    img.push(0); //                       format
    img.push(ZSTD_WINDOWLOG); //          windowlog - 10
    img.resize(img.len() + 4, 0); //      reserved[4]
    img.resize(BLOCK_SIZE, 0); // pad block 0

    // Inode table (blocks 1..): per node in nid order, a 32-byte compact inode, and for a
    // compressed file its map header, padding and extent records, padded to whole slots.
    let mut pcursor = packed_start;
    for (idx, n) in nodes.iter().enumerate() {
        let slot_start = img.len();
        let links = u64::from(nlink.get(idx).copied().unwrap_or(1));
        let node_extents = extents.get(idx).map_or(&[][..], |e| e.as_slice());
        if let (Body::File { data }, false) = (&n.body, node_extents.is_empty()) {
            let encoded: u64 = node_extents.iter().map(|e| e.bytes.len() as u64).sum();
            put(&mut img, I_FORMAT_COMPRESSED_FULL, 2); // i_format
            put(&mut img, 0, 2); //                    i_xattr_icount
            put(&mut img, u64::from(n.mode), 2); //    i_mode
            put(&mut img, links, 2); //                i_nb = nlink
            put(&mut img, data.len() as u64, 4); //    i_size
            put(&mut img, 0, 4); //                    i_mtime (epoch delta = 0)
            put(&mut img, encoded.div_ceil(BLOCK_SIZE as u64), 4); // i_u = blocks_lo
            put(&mut img, (idx + 1) as u64, 4); //     i_ino
            put(&mut img, 0, 2); //                    i_uid (root)
            put(&mut img, 0, 2); //                    i_gid (root)
            put(&mut img, 0, 4); //                    i_reserved
                                 // z_erofs_map_header: extent count, advise, extent count MSB.
            put(&mut img, node_extents.len() as u64, 4);
            put(&mut img, ADVISE_EXTENTS_16B, 2);
            put(&mut img, 0, 2);
            img.resize(slot_start + EXTENT_RECORDS_OFFSET, 0);
            for (k, e) in node_extents.iter().enumerate() {
                put(&mut img, u64::from(e.plen), 4); //       plen | format
                put(&mut img, pcursor & 0xFFFF_FFFF, 4); //   pstart_lo
                put(&mut img, pcursor >> 32, 4); //           pstart_hi
                put(&mut img, (k * EXTENT_SIZE) as u64, 4); // lstart_lo
                pcursor += e.bytes.len() as u64;
            }
        } else {
            let size = plain.get(idx).map_or(0, |d| d.len()) as u64;
            let start_or_rdev = match &n.body {
                Body::Special { rdev } => u64::from(*rdev),
                _ => u64::from(startblk.get(idx).copied().unwrap_or(0)),
            };
            put(&mut img, 0, 2); //                    i_format (compact, FLAT_PLAIN)
            put(&mut img, 0, 2); //                    i_xattr_icount
            put(&mut img, u64::from(n.mode), 2); //    i_mode
            put(&mut img, links, 2); //                i_nb = nlink
            put(&mut img, size, 4); //                 i_size
            put(&mut img, 0, 4); //                    i_mtime (epoch delta = 0)
            put(&mut img, start_or_rdev, 4); //        i_u (startblk_lo | rdev)
            put(&mut img, (idx + 1) as u64, 4); //     i_ino
            put(&mut img, 0, 2); //                    i_uid (root)
            put(&mut img, 0, 2); //                    i_gid (root)
            put(&mut img, 0, 4); //                    i_reserved
        }
        img.resize(slot_start + slots_for(n) * INODE_SIZE, 0);
    }
    pad_to_block(&mut img); // pad the metadata area to a block boundary

    // Plain data blocks, in nid order, each block-padded (matches the assigned startblk).
    for d in plain.iter().filter(|d| !d.is_empty()) {
        img.extend_from_slice(d);
        pad_to_block(&mut img);
    }
    // The packed extents, back to back, in the order their records name them.
    for e in extents.iter().flatten() {
        img.extend_from_slice(&e.bytes);
    }
    pad_to_block(&mut img);
    if img.len() != total_blocks as usize * BLOCK_SIZE {
        return Err(io::Error::other(
            "internal: erofs layout and image disagree",
        ));
    }

    Ok(img)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    // ---- minimal in-module reader (the decode side of the format) ------------------

    fn rd_u16(b: &[u8], off: usize) -> u16 {
        u16::from_le_bytes([b[off], b[off + 1]])
    }
    fn rd_u32(b: &[u8], off: usize) -> u32 {
        u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
    }
    fn rd_u64(b: &[u8], off: usize) -> u64 {
        let mut v = [0u8; 8];
        v.copy_from_slice(&b[off..off + 8]);
        u64::from_le_bytes(v)
    }

    struct RInode {
        off: usize,
        format: u16,
        mode: u16,
        nlink: u16,
        size: u64,
        u: u32, // startblk_lo or rdev
    }

    fn read_super(img: &[u8]) -> (u16, u32, u32) {
        let s = SUPER_OFFSET;
        assert_eq!(rd_u32(img, s), MAGIC, "bad magic");
        assert_eq!(img[s + 12], BLKSZBITS, "bad blkszbits");
        assert_eq!(img[s + 90], 0, "dirblkbits must be 0");
        let root_nid = rd_u16(img, s + 14);
        let blocks = rd_u32(img, s + 36);
        let meta = rd_u32(img, s + 40);
        (root_nid, blocks, meta)
    }

    fn read_inode(img: &[u8], meta_blkaddr: u32, nid: u64) -> RInode {
        let off = (meta_blkaddr as usize) * BLOCK_SIZE + (nid as usize) * INODE_SIZE;
        RInode {
            off,
            format: rd_u16(img, off),
            mode: rd_u16(img, off + 4),
            nlink: rd_u16(img, off + 6),
            size: u64::from(rd_u32(img, off + 8)),
            u: rd_u32(img, off + 16),
        }
    }

    /// Parse a directory inode's dirents into `(name, nid, file_type)`.
    fn read_dir(img: &[u8], ino: &RInode) -> Vec<(String, u64, u8)> {
        let mut out = Vec::new();
        let nblocks = (ino.size as usize) / BLOCK_SIZE;
        for b in 0..nblocks {
            let base = (ino.u as usize + b) * BLOCK_SIZE;
            let block = &img[base..base + BLOCK_SIZE];
            let first_nameoff = rd_u16(block, 8) as usize; // de[0].nameoff
            let k = first_nameoff / DIRENT_SIZE;
            for j in 0..k {
                let de = j * DIRENT_SIZE;
                let nid = rd_u64(block, de);
                let nameoff = rd_u16(block, de + 8) as usize;
                let ft = block[de + 10];
                let nameend = if j + 1 < k {
                    rd_u16(block, (j + 1) * DIRENT_SIZE + 8) as usize
                } else {
                    // last dirent: strnlen up to block end
                    let mut e = nameoff;
                    while e < BLOCK_SIZE && block[e] != 0 {
                        e += 1;
                    }
                    e
                };
                let name = String::from_utf8(block[nameoff..nameend].to_vec()).unwrap();
                out.push((name, nid, ft));
            }
        }
        out
    }

    fn read_file(img: &[u8], ino: &RInode) -> Vec<u8> {
        if ino.format == 0 {
            let base = (ino.u as usize) * BLOCK_SIZE;
            return img[base..base + ino.size as usize].to_vec();
        }
        assert_eq!(ino.format, 2);
        assert_eq!(rd_u16(img, ino.off + 36), 5);
        let count = rd_u32(img, ino.off + 32) as usize;
        let mut out = Vec::new();
        for k in 0..count {
            let at = ino.off + 48 + k * 16;
            let plen = rd_u32(img, at);
            let start = u64::from(rd_u32(img, at + 4)) | (u64::from(rd_u32(img, at + 8)) << 32);
            let logical = rd_u32(img, at + 12) as usize;
            assert_eq!(logical, out.len());
            let end = if k + 1 == count {
                ino.size as usize
            } else {
                rd_u32(img, at + 28) as usize
            };
            let bytes = &img[start as usize..start as usize + (plen & 0x0fff_ffff) as usize];
            match plen >> 28 {
                0 => out.extend_from_slice(bytes),
                4 => out.extend(crate::zstd::decompress(bytes, end - logical).unwrap()),
                other => panic!("unexpected extent format {other}"),
            }
            assert_eq!(out.len(), end);
        }
        assert_eq!(out.len(), ino.size as usize);
        out
    }

    /// Like `read_dir`, but returns raw name bytes (for non-UTF8 names).
    fn read_dir_bytes(img: &[u8], ino: &RInode) -> Vec<(Vec<u8>, u64, u8)> {
        let mut out = Vec::new();
        let nblocks = (ino.size as usize) / BLOCK_SIZE;
        for b in 0..nblocks {
            let base = (ino.u as usize + b) * BLOCK_SIZE;
            let block = &img[base..base + BLOCK_SIZE];
            let k = rd_u16(block, 8) as usize / DIRENT_SIZE;
            for j in 0..k {
                let de = j * DIRENT_SIZE;
                let nid = rd_u64(block, de);
                let nameoff = rd_u16(block, de + 8) as usize;
                let ft = block[de + 10];
                let nameend = if j + 1 < k {
                    rd_u16(block, (j + 1) * DIRENT_SIZE + 8) as usize
                } else {
                    let mut e = nameoff;
                    while e < BLOCK_SIZE && block[e] != 0 {
                        e += 1;
                    }
                    e
                };
                out.push((block[nameoff..nameend].to_vec(), nid, ft));
            }
        }
        out
    }

    /// Resolve a path (slash-separated, no leading slash) from the root, returning its
    /// inode. Panics (test-only) if any component is missing.
    fn lookup(img: &[u8], parts: &[&str]) -> RInode {
        let (nid, meta) = lookup_nid(img, parts);
        read_inode(img, meta, nid)
    }

    /// Like `lookup`, but returns the resolved nid (and meta_blkaddr) so tests can assert
    /// two paths share one inode (hard links).
    fn lookup_nid(img: &[u8], parts: &[&str]) -> (u64, u32) {
        let (root_nid, _blocks, meta) = read_super(img);
        let mut nid = u64::from(root_nid);
        for part in parts {
            let cur = read_inode(img, meta, nid);
            let entries = read_dir(img, &cur);
            let (_, found, _) = entries
                .iter()
                .find(|(n, _, _)| n == part)
                .unwrap_or_else(|| panic!("component {part} not found in {entries:?}"));
            nid = *found;
        }
        (nid, meta)
    }

    fn tmpdir(tag: &str) -> crate::test_temp::TempDir {
        crate::test_temp::TempDir::new(&format!("td-erofs-test-{tag}")).unwrap()
    }

    fn set_mode(p: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        let mut perm = fs::metadata(p).unwrap().permissions();
        perm.set_mode(mode);
        fs::set_permissions(p, perm).unwrap();
    }

    /// A store-native-ish rootfs: a /td/store dir with a "package", a /bin symlink farm
    /// into it, nested dirs, an executable, and empty dirs.
    fn make_rootfs(dir: &Path) {
        fs::create_dir_all(dir.join("td/store/abc-busybox/bin")).unwrap();
        fs::write(
            dir.join("td/store/abc-busybox/bin/busybox"),
            vec![0x7f, b'E', b'L', b'F', 42],
        )
        .unwrap();
        set_mode(&dir.join("td/store/abc-busybox/bin/busybox"), 0o755);
        fs::create_dir_all(dir.join("bin")).unwrap();
        symlink("/td/store/abc-busybox/bin/busybox", dir.join("bin/sh")).unwrap();
        symlink("/td/store/abc-busybox/bin/busybox", dir.join("bin/ls")).unwrap();
        fs::create_dir_all(dir.join("etc")).unwrap();
        fs::write(dir.join("etc/hostname"), b"td\n").unwrap();
        fs::create_dir_all(dir.join("dev")).unwrap();
        fs::create_dir_all(dir.join("proc")).unwrap();
    }

    #[test]
    fn packed_extents_round_trip_mixed_data_and_boundaries() {
        let d = tmpdir("extents");
        let mut random = vec![0u8; EXTENT_SIZE];
        let mut state = 42u64;
        for byte in &mut random {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            *byte = state as u8;
        }
        let mut mixed = vec![b'a'; EXTENT_SIZE];
        mixed.extend_from_slice(&random);
        mixed.extend_from_slice(b"last extent");
        fs::write(d.join("mixed"), &mixed).unwrap();
        fs::write(d.join("empty"), []).unwrap();
        for n in 0..100 {
            fs::write(d.join(format!("small{n}")), b"x").unwrap();
        }
        let img = build_image(&d).unwrap();
        assert_eq!(img, build_image(&d).unwrap());
        let ino = lookup(&img, &["mixed"]);
        assert_eq!(read_file(&img, &ino), mixed);
        assert_eq!(rd_u32(&img, ino.off + 48) >> 28, 4);
        assert_eq!(rd_u32(&img, ino.off + 64) >> 28, 0);
        assert!(read_file(&img, &lookup(&img, &["empty"])).is_empty());
        for n in 0..100 {
            assert_eq!(
                read_file(&img, &lookup(&img, &[&format!("small{n}")])),
                b"x"
            );
        }
        assert!(img.len() < EXTENT_SIZE + 32 * BLOCK_SIZE);
    }

    #[test]
    fn superblock_is_well_formed() {
        let d = tmpdir("super");
        let root = d.join("r");
        fs::create_dir_all(&root).unwrap();
        make_rootfs(&root);
        let img = build_image(&root).unwrap();
        let (root_nid, blocks, meta) = read_super(&img);
        assert_eq!(img[SUPER_OFFSET + 13], 1);
        assert_eq!(rd_u32(&img, SUPER_OFFSET + 80), 2);
        assert_eq!(rd_u16(&img, SUPER_OFFSET + 84), 8);
        assert_eq!(&img[1168..1176], &[6, 0, 0, 7, 0, 0, 0, 0]);
        assert_eq!(root_nid, 0, "root nid must be 0");
        assert_eq!(meta, META_BLKADDR);
        assert_eq!(
            img.len(),
            blocks as usize * BLOCK_SIZE,
            "image length must be blocks*blocksize"
        );
        let root_ino = read_inode(&img, meta, 0);
        assert_eq!(
            u32::from(root_ino.mode) & S_IFMT,
            S_IFDIR,
            "root must be a directory"
        );
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn round_trips_tree_symlinks_and_contents() {
        let d = tmpdir("roundtrip");
        let root = d.join("r");
        fs::create_dir_all(&root).unwrap();
        make_rootfs(&root);
        let img = build_image(&root).unwrap();

        // /bin/sh is a symlink into the store.
        let sh = lookup(&img, &["bin", "sh"]);
        assert_eq!(
            u32::from(sh.mode) & S_IFMT,
            S_IFLNK,
            "bin/sh is not a symlink"
        );
        let target = read_file(&img, &sh);
        assert_eq!(
            target, b"/td/store/abc-busybox/bin/busybox",
            "wrong symlink target"
        );

        // the packed store binary is a regular file with its exact bytes + 0755.
        let bb = lookup(&img, &["td", "store", "abc-busybox", "bin", "busybox"]);
        assert_eq!(u32::from(bb.mode) & S_IFMT, S_IFREG);
        assert_eq!(u32::from(bb.mode) & 0o777, 0o755, "mode not preserved");
        assert_eq!(read_file(&img, &bb), vec![0x7f, b'E', b'L', b'F', 42]);

        // etc/hostname content round-trips.
        let hn = lookup(&img, &["etc", "hostname"]);
        assert_eq!(read_file(&img, &hn), b"td\n");

        // empty dirs still carry "." and "..".
        let dev = lookup(&img, &["dev"]);
        let entries = read_dir(&img, &dev);
        let names: Vec<&str> = entries.iter().map(|(n, _, _)| n.as_str()).collect();
        assert!(names.contains(&"."), "empty dir missing .");
        assert!(names.contains(&".."), "empty dir missing ..");
        assert_eq!(names.len(), 2, "empty dir should have exactly . and ..");
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn dirents_are_sorted_and_dot_entries_present() {
        let d = tmpdir("sorted");
        let root = d.join("r");
        fs::create_dir_all(&root).unwrap();
        make_rootfs(&root);
        let img = build_image(&root).unwrap();
        let (root_nid, _b, meta) = read_super(&img);
        let root_ino = read_inode(&img, meta, u64::from(root_nid));
        let entries = read_dir(&img, &root_ino);
        let names: Vec<String> = entries.iter().map(|(n, _, _)| n.clone()).collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(
            names, sorted,
            "root dirents not globally sorted (kernel bsearch needs this)"
        );
        assert_eq!(
            names.first().map(String::as_str),
            Some("."),
            ". should sort first"
        );
        // "." nid points back at the root; ".." at the parent (root itself here).
        let dot = entries.iter().find(|(n, _, _)| n == ".").unwrap();
        let dotdot = entries.iter().find(|(n, _, _)| n == "..").unwrap();
        assert_eq!(dot.1, u64::from(root_nid), ". must reference self");
        assert_eq!(
            dotdot.1,
            u64::from(root_nid),
            ".. of root must reference root"
        );
        // the root's nlink is 2 + subdir count (bin, dev, etc, proc, td = 5).
        assert_eq!(root_ino.nlink, 2 + 5, "root nlink wrong");
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn deterministic_byte_identical() {
        let d = tmpdir("determ");
        let a = d.join("a");
        let b = d.join("b");
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        make_rootfs(&a);
        make_rootfs(&b);
        let ia = build_image(&a).unwrap();
        let ib = build_image(&b).unwrap();
        assert_eq!(ia, ib, "same tree must pack byte-identically");
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn suid_bit_preserved() {
        let d = tmpdir("suid");
        let root = d.join("r");
        fs::create_dir_all(root.join("bin")).unwrap();
        let su = root.join("bin/su");
        fs::write(&su, b"x").unwrap();
        set_mode(&su, 0o4755);
        let img = build_image(&root).unwrap();
        let ino = lookup(&img, &["bin", "su"]);
        assert_eq!(
            u32::from(ino.mode) & 0o4000,
            0o4000,
            "setuid bit dropped (mode {:o})",
            ino.mode
        );
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn many_entries_span_multiple_dir_blocks() {
        let d = tmpdir("bigdir");
        let root = d.join("r");
        fs::create_dir_all(root.join("bin")).unwrap();
        // Enough entries that dirents+names exceed one 4096-byte block, forcing >1 block
        // and exercising the cross-block global-sort invariant.
        for i in 0..600 {
            fs::write(root.join(format!("bin/cmd{i:04}")), b"x").unwrap();
        }
        let img = build_image(&root).unwrap();
        let bin = lookup(&img, &["bin"]);
        assert!(
            bin.size as usize > BLOCK_SIZE,
            "big dir should span >1 block"
        );
        let entries = read_dir(&img, &bin);
        let names: Vec<String> = entries.iter().map(|(n, _, _)| n.clone()).collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(
            names, sorted,
            "multi-block dirents must stay globally sorted"
        );
        assert!(names.contains(&"cmd0000".to_string()) && names.contains(&"cmd0599".to_string()));
        assert_eq!(
            names.len(),
            600 + 2,
            "all entries plus ./.. must be present"
        );
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn fifo_special_inode_roundtrips() {
        // A FIFO exercises the special-inode path with no root privilege (mkfifo).
        let d = tmpdir("fifo");
        let root = d.join("r");
        fs::create_dir_all(&root).unwrap();
        let fifo = root.join("pipe");
        let status = std::process::Command::new("mkfifo").arg(&fifo).status();
        // Skip gracefully where mkfifo is unavailable (keeps the suite hermetic).
        if !matches!(status, Ok(s) if s.success()) {
            fs::remove_dir_all(&d).unwrap();
            return;
        }
        let img = build_image(&root).unwrap();
        let ino = lookup(&img, &["pipe"]);
        assert_eq!(u32::from(ino.mode) & S_IFMT, S_IFIFO, "not a FIFO inode");
        assert_eq!(ino.size, 0, "special inode has no data");
        assert_eq!(ino.u, 0, "FIFO rdev must be 0");
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn new_encode_dev_matches_kernel_layout() {
        // /dev/console is char 5:1. glibc dev_t for makedev(5,1):
        //   (minor & 0xff) | (major << 8) | ((minor & ~0xff) << 12)  == 0x0501.
        // Build the glibc-encoded raw rdev for makedev(5,1) and check the re-encode.
        let raw: u64 = (5u64 << 8) | 1; // gnu_dev_makedev(5,1) for minor<256
        assert_eq!(new_encode_dev(raw), 0x0501);
        // char 1:3 (/dev/null): (3) | (1<<8) == 0x0103.
        let raw2: u64 = (1u64 << 8) | 3;
        assert_eq!(new_encode_dev(raw2), 0x0103);
    }

    #[test]
    fn hard_links_share_one_inode() {
        use std::os::unix::fs::MetadataExt;
        let d = tmpdir("hardlink");
        let root = d.join("r");
        fs::create_dir_all(root.join("bin")).unwrap();
        let a = root.join("bin/a");
        fs::write(&a, b"shared-bytes").unwrap();
        // Two more hard links to the same inode, in different directories.
        fs::create_dir_all(root.join("sbin")).unwrap();
        fs::hard_link(&a, root.join("bin/b")).unwrap();
        fs::hard_link(&a, root.join("sbin/c")).unwrap();
        assert!(
            fs::metadata(&a).unwrap().nlink() >= 3,
            "test setup: need >1 link"
        );

        let img = build_image(&root).unwrap();

        // All three paths must resolve to the SAME nid (one inode, one data copy).
        let (na, _) = lookup_nid(&img, &["bin", "a"]);
        let (nb, _) = lookup_nid(&img, &["bin", "b"]);
        let (nc, _) = lookup_nid(&img, &["sbin", "c"]);
        assert_eq!(na, nb, "hard links must share a nid");
        assert_eq!(na, nc, "hard links must share a nid across directories");

        let ino = lookup(&img, &["bin", "a"]);
        assert_eq!(ino.nlink, 3, "nlink must count the in-image references");
        assert_eq!(
            read_file(&img, &ino),
            b"shared-bytes",
            "shared data must round-trip"
        );

        // Determinism holds with hard links present.
        let b = d.join("r2");
        fs::create_dir_all(b.join("bin")).unwrap();
        fs::create_dir_all(b.join("sbin")).unwrap();
        fs::write(b.join("bin/a"), b"shared-bytes").unwrap();
        fs::hard_link(b.join("bin/a"), b.join("bin/b")).unwrap();
        fs::hard_link(b.join("bin/a"), b.join("sbin/c")).unwrap();
        assert_eq!(
            build_image(&b).unwrap(),
            img,
            "hard-linked tree must pack deterministically"
        );
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn non_utf8_name_roundtrips() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        let d = tmpdir("nonutf8");
        let root = d.join("r");
        fs::create_dir_all(&root).unwrap();
        // 0xFF is not valid UTF-8 but is a legal filename byte (only '/' and NUL are not).
        let raw = b"caf\xe9\xff";
        let name = OsStr::from_bytes(raw);
        fs::write(root.join(name), b"z").unwrap();

        let img = build_image(&root).unwrap();
        let (root_nid, _b, meta) = read_super(&img);
        let root_ino = read_inode(&img, meta, u64::from(root_nid));
        let entries = read_dir_bytes(&img, &root_ino);
        assert!(
            entries.iter().any(|(n, _, _)| n.as_slice() == raw),
            "non-UTF8 name must survive verbatim; got {entries:?}"
        );
        fs::remove_dir_all(&d).unwrap();
    }
}

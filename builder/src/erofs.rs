//! erofs.rs — td-native, zero-dep, DETERMINISTIC **erofs** (Enhanced Read-Only File
//! System) image writer.
//!
//! This is increment 1 of the read-only-root arc (issue #548, re #541). td's minimal
//! distro is moving to a two-stage boot: a tiny static-busybox initramfs mounts a
//! disk-backed, **read-only** erofs image carrying `/td/store` and `switch_root`s into
//! it. A read-only root is the natural physical form of the content-addressed,
//! immutable-by-hash `/td/store`. This module builds that image.
//!
//! Trust model: this is a **control-plane** packer, exposed as the `td-builder
//! mkfs-erofs` subcommand and run only with `ControlPlaneBuilder` provenance as a
//! derivation implementation — never from a recipe's `PATH`/argv. It is a build-time
//! tool (like the `oci.rs` docker-archive writer and the NAR codec), never a shipped
//! target artifact.
//!
//! Format — a minimal, maximally-compatible subset of the erofs on-disk layout, pinned
//! field-for-field against the target kernel's `fs/erofs/erofs_fs.h` (Linux 7.1.4):
//!
//! * 4096-byte blocks (`blkszbits=12`; the erofs block size must be `<= PAGE_SHIFT`,
//!   which is 12 on x86_64). `dirblkbits=0` — the kernel *rejects* any non-zero value
//!   and derives the directory block size from `blkszbits`.
//! * **Compact 32-byte inodes** only (`islotbits=5`). A node id (`nid`) is the inode's
//!   index; its byte offset is `meta_blkaddr*4096 + nid*32`. `meta_blkaddr=1`, so the
//!   inode table starts at block 1 (block 0 holds the boot area + superblock). 32
//!   divides 4096, so an inode never straddles a block.
//! * **`EROFS_INODE_FLAT_PLAIN`** datalayout for every inode: a node's data lives in
//!   whole blocks starting at `startblk_lo` (a 32-bit block address — we never set the
//!   48BIT feature, so the kernel forces `startblk_hi=0`). Simplest correct layout; no
//!   tail-packing/inline, so a small file/symlink rounds up to a block (uncompressed,
//!   correctness-first — compression is a later, dependency-gated increment).
//! * Bit 4 of `i_format` is left clear, so the kernel reads `i_nb` as an explicit
//!   nlink (not the 48-bit `startblk_hi`) and expects the `.` dirent on disk (we emit
//!   it). `i_format` is therefore 0 (compact version 0, FLAT_PLAIN datalayout 0) for
//!   every inode.
//! * Directories: each 4096-byte block is `[erofs_dirent; K]` (12 bytes each, sorted by
//!   name) then the names packed contiguously, then NUL padding. `nameoff` of the first
//!   dirent is `K*12`; the kernel derives `K = nameoff/12` and binary-searches, so
//!   dirents must be **globally sorted by name** across the whole directory and blocks
//!   filled in order. A block's last name is bounded by `strnlen(name, blocksize -
//!   nameoff)`, so the trailing NUL pad terminates it. `.`/`..` are ordinary sorted
//!   entries (nid of self / parent).
//!
//! Determinism (prime directive 1): entries are emitted in sorted order; uid/gid are
//! normalized to 0 (a store root is root-owned, like the packed initramfs); mtimes are
//! 0 (`epoch=0` + per-inode `i_mtime=0`); uuid/volume-name/build_time are zeroed. The
//! same staged tree always packs to byte-identical bytes. File modes are PRESERVED
//! (including suid/sgid/sticky); symlinks carry the conventional `0777`.
//!
//! Validation: the tests round-trip through an in-module reader (the decode side of the
//! same format) and assert determinism, nesting, symlinks, mode/suid preservation, and
//! the special-inode (FIFO) path. A real-kernel mount is the boot test in the
//! kernel-config increment (#549).

use std::fs;
use std::io;
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
    /// A directory: its sorted `(name, child-index)` entries plus the parent's index
    /// (for the `..` dirent; the root's parent is itself).
    Dir { children: Vec<(String, usize)>, parent: usize },
    File { data: Vec<u8> },
    Symlink { target: Vec<u8> },
    /// FIFO / socket / char / block device — no data blocks; `rdev` is the encoded
    /// device number (0 for FIFO/socket).
    Special { rdev: u32 },
}

/// One inode of the image, in assignment (nid) order. `nid == index into the node
/// vector`, `ino == index + 1`.
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
fn walk(path: &Path, parent: usize, nodes: &mut Vec<Node>) -> io::Result<usize> {
    let meta = fs::symlink_metadata(path)?;
    let ft = meta.file_type();
    let my_idx = nodes.len();
    // Reserve this node's slot with a placeholder; filled in after recursion.
    nodes.push(Node { mode: 0, body: Body::Special { rdev: 0 } });

    let (mode, body) = if ft.is_dir() {
        let perm = (meta.mode() & 0o7777) as u16;
        let mut names: Vec<String> = Vec::new();
        for ent in fs::read_dir(path)? {
            let name = ent?.file_name().into_string().map_err(|n| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("non-UTF8 filename under {}: {}", path.display(), n.to_string_lossy()),
                )
            })?;
            names.push(name);
        }
        names.sort();
        let mut children: Vec<(String, usize)> = Vec::with_capacity(names.len());
        for name in names {
            let child_idx = walk(&path.join(&name), my_idx, nodes)?;
            children.push((name, child_idx));
        }
        (S_IFDIR as u16 | perm, Body::Dir { children, parent })
    } else if ft.is_symlink() {
        let target = fs::read_link(path)?;
        let target = target.into_os_string().into_string().map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("non-UTF8 symlink target at {}", path.display()),
            )
        })?;
        // A symlink target must be non-empty and fit a single block (POSIX targets are
        // well under 4096); FLAT_PLAIN stores it as the inode's data.
        if target.is_empty() || target.len() > BLOCK_SIZE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("symlink target at {} is empty or over one block", path.display()),
            ));
        }
        (S_IFLNK as u16 | 0o777, Body::Symlink { target: target.into_bytes() })
    } else if ft.is_file() {
        let data = fs::read(path)?;
        // Compact inodes carry a 32-bit i_size; a >=4 GiB file would need the extended
        // (64-byte) inode. Refuse rather than truncate silently.
        if data.len() > u32::MAX as usize {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("file {} exceeds the 4 GiB compact-inode size limit", path.display()),
            ));
        }
        let perm = (meta.mode() & 0o7777) as u16;
        (S_IFREG as u16 | perm, Body::File { data })
    } else {
        // FIFO / socket / char / block device.
        let raw = meta.mode();
        let perm = (raw & 0o7777) as u16;
        let ifmt = raw & S_IFMT;
        let rdev = if ifmt == S_IFCHR || ifmt == S_IFBLK { new_encode_dev(meta.rdev()) } else { 0 };
        match ifmt {
            S_IFCHR | S_IFBLK | S_IFIFO | S_IFSOCK => {
                ((ifmt as u16) | perm, Body::Special { rdev })
            }
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{}: unsupported file type for an erofs image", path.display()),
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
    Ok(my_idx)
}

/// Serialize one directory's data as a sequence of 4096-byte dirent blocks. Every entry
/// (including the sorted `.`/`..`) is `[dirents][names][NUL pad]`; entries are packed
/// greedily into blocks in global sort order.
fn build_dir_data(
    idx: usize,
    parent: usize,
    children: &[(String, usize)],
    nodes: &[Node],
) -> io::Result<Vec<u8>> {
    // (name-bytes, nid, file_type)
    let mut entries: Vec<(Vec<u8>, u64, u8)> = Vec::with_capacity(children.len() + 2);
    entries.push((b".".to_vec(), idx as u64, FT_DIR));
    entries.push((b"..".to_vec(), parent as u64, FT_DIR));
    for (name, cidx) in children {
        let ft = match nodes.get(*cidx) {
            Some(n) => file_type_of(n.mode),
            None => {
                return Err(io::Error::other("internal: dangling child index"))
            }
        };
        entries.push((name.clone().into_bytes(), *cidx as u64, ft));
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
                None => {
                    return Err(io::Error::other("internal: dirent index slipped"))
                }
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
    // The root's parent is itself (its `..` points back to the root nid, 0).
    walk(root, 0, &mut nodes)?;
    let ninodes = nodes.len();

    // Pass 1: each node's raw data bytes (directories become dirent blocks).
    let mut datas: Vec<Vec<u8>> = Vec::with_capacity(ninodes);
    for (idx, n) in nodes.iter().enumerate() {
        let d = match &n.body {
            Body::Dir { children, parent } => build_dir_data(idx, *parent, children, &nodes)?,
            Body::File { data } => data.clone(),
            Body::Symlink { target } => target.clone(),
            Body::Special { .. } => Vec::new(),
        };
        datas.push(d);
    }

    // Layout: block 0 = superblock; blocks [1 ..) = inode table; then data blocks.
    let meta_blocks = ceil_div(ninodes * INODE_SIZE, BLOCK_SIZE);
    let data_start = META_BLKADDR as usize + meta_blocks;

    // Pass 2: assign each node's starting data block (FLAT_PLAIN, contiguous, in nid
    // order) and its explicit nlink (a directory's nlink is 2 + its subdirectory count).
    let mut startblk: Vec<u32> = Vec::with_capacity(ninodes);
    let mut nlink: Vec<u16> = Vec::with_capacity(ninodes);
    let mut cursor = data_start as u32;
    for (idx, n) in nodes.iter().enumerate() {
        let nblk = datas.get(idx).map_or(0, |d| ceil_div(d.len(), BLOCK_SIZE));
        if nblk == 0 {
            startblk.push(0);
        } else {
            startblk.push(cursor);
            cursor = cursor.saturating_add(nblk as u32);
        }
        let links = match &n.body {
            Body::Dir { children, .. } => {
                let subdirs = children
                    .iter()
                    .filter(|(_, cidx)| {
                        nodes.get(*cidx).is_some_and(|c| u32::from(c.mode) & S_IFMT == S_IFDIR)
                    })
                    .count();
                (2 + subdirs).min(u16::MAX as usize) as u16
            }
            _ => 1,
        };
        nlink.push(links);
    }
    let total_blocks = cursor;

    // ---- assemble ----
    let mut img: Vec<u8> = Vec::with_capacity(total_blocks as usize * BLOCK_SIZE);

    // Block 0: [0..1024] boot area, then the 144-byte superblock, then zero pad.
    img.resize(SUPER_OFFSET, 0);
    put(&mut img, MAGIC as u64, 4); //   0 magic
    put(&mut img, 0, 4); //               4 checksum (feature off)
    put(&mut img, 0, 4); //               8 feature_compat
    img.push(BLKSZBITS); //              12 blkszbits
    img.push(0); //                      13 sb_extslots
    put(&mut img, 0, 2); //              14 rootnid_2b (root nid = 0)
    put(&mut img, ninodes as u64, 8); // 16 inos
    put(&mut img, 0, 8); //              24 epoch
    put(&mut img, 0, 4); //              32 fixed_nsec
    put(&mut img, total_blocks as u64, 4); // 36 blocks_lo
    put(&mut img, META_BLKADDR as u64, 4); // 40 meta_blkaddr
    put(&mut img, 0, 4); //              44 xattr_blkaddr
    img.resize(img.len() + 16, 0); //    48 uuid[16]
    img.resize(img.len() + 16, 0); //    64 volume_name[16]
    put(&mut img, 0, 4); //              80 feature_incompat
    put(&mut img, 0, 2); //              84 u1 (compr algs)
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
    img.resize(BLOCK_SIZE, 0); // pad block 0

    // Inode table (blocks 1..), one 32-byte compact inode per node in nid order.
    for (idx, n) in nodes.iter().enumerate() {
        let size = datas.get(idx).map_or(0, |d| d.len()) as u64;
        let start_or_rdev = match &n.body {
            Body::Special { rdev } => u64::from(*rdev),
            _ => u64::from(startblk.get(idx).copied().unwrap_or(0)),
        };
        let links = u64::from(nlink.get(idx).copied().unwrap_or(1));
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
    pad_to_block(&mut img); // pad the metadata area to a block boundary

    // Data blocks, in nid order, each block-padded (matches the assigned startblk).
    for idx in 0..ninodes {
        if let Some(d) = datas.get(idx) {
            if !d.is_empty() {
                img.extend_from_slice(d);
                pad_to_block(&mut img);
            }
        }
    }

    Ok(img)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use std::path::PathBuf;

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
        let base = (ino.u as usize) * BLOCK_SIZE;
        img[base..base + ino.size as usize].to_vec()
    }

    /// Resolve a path (slash-separated, no leading slash) from the root, returning its
    /// inode. Panics (test-only) if any component is missing.
    fn lookup(img: &[u8], parts: &[&str]) -> RInode {
        let (root_nid, _blocks, meta) = read_super(img);
        let mut cur = read_inode(img, meta, u64::from(root_nid));
        for part in parts {
            let entries = read_dir(img, &cur);
            let (_, nid, _) = entries
                .iter()
                .find(|(n, _, _)| n == part)
                .unwrap_or_else(|| panic!("component {part} not found in {entries:?}"));
            cur = read_inode(img, meta, *nid);
        }
        cur
    }

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("td-erofs-test-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
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
        fs::write(dir.join("td/store/abc-busybox/bin/busybox"), vec![0x7f, b'E', b'L', b'F', 42]).unwrap();
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
    fn superblock_is_well_formed() {
        let d = tmpdir("super");
        let root = d.join("r");
        fs::create_dir_all(&root).unwrap();
        make_rootfs(&root);
        let img = build_image(&root).unwrap();
        let (root_nid, blocks, meta) = read_super(&img);
        assert_eq!(root_nid, 0, "root nid must be 0");
        assert_eq!(meta, META_BLKADDR);
        assert_eq!(img.len(), blocks as usize * BLOCK_SIZE, "image length must be blocks*blocksize");
        let root_ino = read_inode(&img, meta, 0);
        assert_eq!(u32::from(root_ino.mode) & S_IFMT, S_IFDIR, "root must be a directory");
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
        assert_eq!(u32::from(sh.mode) & S_IFMT, S_IFLNK, "bin/sh is not a symlink");
        let target = read_file(&img, &sh);
        assert_eq!(target, b"/td/store/abc-busybox/bin/busybox", "wrong symlink target");

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
        assert_eq!(names, sorted, "root dirents not globally sorted (kernel bsearch needs this)");
        assert_eq!(names.first().map(String::as_str), Some("."), ". should sort first");
        // "." nid points back at the root; ".." at the parent (root itself here).
        let dot = entries.iter().find(|(n, _, _)| n == ".").unwrap();
        let dotdot = entries.iter().find(|(n, _, _)| n == "..").unwrap();
        assert_eq!(dot.1, u64::from(root_nid), ". must reference self");
        assert_eq!(dotdot.1, u64::from(root_nid), ".. of root must reference root");
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
        assert_eq!(u32::from(ino.mode) & 0o4000, 0o4000, "setuid bit dropped (mode {:o})", ino.mode);
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
        assert!(bin.size as usize > BLOCK_SIZE, "big dir should span >1 block");
        let entries = read_dir(&img, &bin);
        let names: Vec<String> = entries.iter().map(|(n, _, _)| n.clone()).collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted, "multi-block dirents must stay globally sorted");
        assert!(names.contains(&"cmd0000".to_string()) && names.contains(&"cmd0599".to_string()));
        assert_eq!(names.len(), 600 + 2, "all entries plus ./.. must be present");
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
}

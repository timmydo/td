//! An offline consistency check of a single-device filesystem, written by
//! td-btrfs or since modified by the kernel.
//!
//! It reads every tree block (every copy) and every checksummed data
//! sector, and cross-checks the extent, block group, device, free space,
//! checksum and subvolume trees against what the trees reference. Shared
//! tree blocks (snapshots), compression, RAID profiles and multiple devices
//! are outside what td writes and are reported as unsupported, never passed.
//! Log trees left by an unclean shutdown are read and verified but not
//! accounted, as the kernel replays them at mount.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io;
use std::path::Path;

use crate::crc32c;
use crate::format::*;
use crate::fstree::{mode_kind, FsTree, INODE_NODATASUM};
use crate::read::{roots, ReadItem, Super, Visited, Visits, Volume, TREE_LOG};

const SHARED_BLOCK_REF: u8 = 182;
const SHARED_DATA_REF: u8 = 184;
const FREE_SPACE_USING_BITMAPS: u32 = 1;
const UUID_KEY_RECEIVED_SUBVOL: u8 = 252;
const VERIFY_BUFFER: usize = 1 << 20;

#[derive(Debug, Default)]
pub struct Report {
    pub errors: Vec<String>,
    pub tree_blocks: usize,
    pub data_bytes: u64,
    pub csum_bytes: u64,
    pub inodes: usize,
    pub subvolumes: usize,
}

impl Report {
    fn error(&mut self, message: impl Into<String>) {
        self.errors.push(message.into());
    }
}

/// Ranges as sorted, merged `[start, end)` pairs.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct Ranges(Vec<(u64, u64)>);

impl Ranges {
    fn from(mut ranges: Vec<(u64, u64)>) -> Self {
        ranges.retain(|(start, end)| start < end);
        ranges.sort();
        let mut out: Vec<(u64, u64)> = Vec::with_capacity(ranges.len());
        for (start, end) in ranges {
            match out.last_mut() {
                Some(last) if start <= last.1 => last.1 = last.1.max(end),
                _ => out.push((start, end)),
            }
        }
        Self(out)
    }

    fn minus(&self, other: &Ranges) -> Ranges {
        let mut out = Vec::new();
        let mut cut = other.0.iter().peekable();
        for &(start, end) in &self.0 {
            let mut at = start;
            while let Some(&&(cut_start, cut_end)) = cut.peek() {
                if cut_end <= at {
                    cut.next();
                    continue;
                }
                if cut_start >= end {
                    break;
                }
                if cut_start > at {
                    out.push((at, cut_start));
                }
                at = at.max(cut_end);
                if cut_end >= end {
                    break;
                }
                cut.next();
            }
            if at < end {
                out.push((at, end));
            }
        }
        Ranges(out)
    }

    fn covers(&self, start: u64, end: u64) -> bool {
        Ranges(vec![(start, end)]).minus(self).0.is_empty()
    }
}

/// Data references by (root, inode, offset), with their counts.
type DataRefs = BTreeMap<(u64, u64, u64), u64>;

/// Inline and keyed back references of one extent.
#[derive(Debug, Default)]
struct Refs {
    refs: u64,
    generation: u64,
    flags: u64,
    level: Option<u8>,
    tree: Vec<u64>,
    data: DataRefs,
    shared: bool,
}

fn parse_inline_refs(data: &[u8], mut at: usize, refs: &mut Refs) -> io::Result<()> {
    while at < data.len() {
        let kind = get8(data, at)?;
        match kind {
            TREE_BLOCK_REF => {
                refs.tree.push(get64(data, at + 1)?);
                at += 9;
            }
            SHARED_BLOCK_REF => {
                refs.shared = true;
                at += 9;
            }
            EXTENT_DATA_REF => {
                let key = (
                    get64(data, at + 1)?,
                    get64(data, at + 9)?,
                    get64(data, at + 17)?,
                );
                let count = refs.data.entry(key).or_default();
                *count = count.saturating_add(u64::from(get32(data, at + 25)?));
                at += 29;
            }
            SHARED_DATA_REF => {
                refs.shared = true;
                at += 13;
            }
            other => {
                return Err(invalid(format!(
                    "unknown inline back reference type {other}"
                )))
            }
        }
    }
    Ok(())
}

pub fn check(path: &Path) -> io::Result<Report> {
    let volume = Volume::open(path)?;
    let mut report = Report::default();
    let sb = &volume.sb;
    check_mirrors(&volume, path, &mut report)?;

    // Every tree, from the root tree.
    // One record for every tree, so a block shared between two is refused
    // when it is reached the second time rather than read again.
    let mut visited = Visits::default();
    let root_items = volume.walk(
        sb.root,
        sb.root_level,
        ROOT_TREE,
        Some(sb.generation),
        &mut visited,
    )?;
    volume.walk(
        sb.chunk_root,
        sb.chunk_root_level,
        CHUNK_TREE,
        Some(sb.chunk_root_generation),
        &mut visited,
    )?;
    let mut trees: BTreeMap<u64, Vec<ReadItem>> = BTreeMap::new();
    let mut root_records: BTreeMap<u64, Root> = BTreeMap::new();
    for (id, root) in roots(&root_items)? {
        let item = root_items
            .iter()
            .find(|item| item.key == Key::new(id, ROOT_ITEM, 0));
        if item.is_none() {
            report.error(format!("root {id} has a ROOT_ITEM with a nonzero offset (a relocation or snapshot root td does not support)"));
            continue;
        }
        if root.refs == 0 {
            report.error(format!("subvolume {id} is deleted but not yet cleaned up"));
        }
        let items = volume.walk(
            root.bytenr,
            root.level,
            id,
            Some(root.generation),
            &mut visited,
        )?;
        trees.insert(id, items);
        root_records.insert(id, root);
    }
    let owners: HashMap<u64, Visited> = visited
        .blocks
        .iter()
        .map(|block| (block.address, *block))
        .collect();
    report.tree_blocks = owners.len();
    for required in [EXTENT_TREE, DEV_TREE, FS_TREE, CSUM_TREE] {
        if !trees.contains_key(&required) {
            report.error(format!("tree {required} is missing"));
        }
    }
    let bgt = sb.compat_ro & COMPAT_RO_BLOCK_GROUP_TREE != 0;
    let fst = sb.compat_ro & COMPAT_RO_FREE_SPACE_TREE != 0;
    if bgt && !trees.contains_key(&BLOCK_GROUP_TREE) {
        report.error("the block group tree is missing");
    }
    if fst && !trees.contains_key(&FREE_SPACE_TREE) {
        report.error("the free space tree is missing");
    }

    // A log tree is verified block by block; its blocks hold no extent
    // items, and may or may not be recorded free.
    let mut log_blocks = Vec::new();
    if sb.log_root != 0 {
        let mut log_visited = Visits::default();
        let log_items = volume.walk(
            sb.log_root,
            sb.log_root_level,
            TREE_LOG,
            None,
            &mut log_visited,
        )?;
        for item in &log_items {
            if item.key.kind == ROOT_ITEM {
                let root = Root::decode(&item.data)?;
                volume.walk(root.bytenr, root.level, TREE_LOG, None, &mut log_visited)?;
            }
        }
        log_blocks = log_visited.blocks;
    }

    // Chunks against device extents.
    let dev_items = trees.get(&DEV_TREE).cloned().unwrap_or_default();
    check_devices(&volume, &dev_items, &mut report);

    // Extents.
    let extent_items = trees.get(&EXTENT_TREE).cloned().unwrap_or_default();
    let mut tree_extents: BTreeMap<u64, Refs> = BTreeMap::new();
    let mut data_extents: BTreeMap<u64, (u64, Refs)> = BTreeMap::new();
    let mut bg_items: Vec<ReadItem> = Vec::new();
    for item in &extent_items {
        let key = item.key;
        match key.kind {
            METADATA_ITEM | EXTENT_ITEM => {
                let mut refs = Refs {
                    refs: get64(&item.data, 0)?,
                    generation: get64(&item.data, 8)?,
                    flags: get64(&item.data, 16)?,
                    ..Refs::default()
                };
                if refs.flags & EXTENT_FLAG_TREE_BLOCK != 0 {
                    let at = if key.kind == METADATA_ITEM {
                        match u8::try_from(key.offset) {
                            Ok(level) => refs.level = Some(level),
                            Err(_) => report.error(format!(
                                "tree block extent {} records level {}",
                                key.objectid, key.offset
                            )),
                        }
                        24
                    } else {
                        if key.offset != u64::from(NODESIZE) {
                            report.error(format!(
                                "tree block extent {} is {} bytes",
                                key.objectid, key.offset
                            ));
                        }
                        refs.level = Some(get8(&item.data, 24 + 17)?);
                        24 + 18
                    };
                    parse_inline_refs(&item.data, at, &mut refs)?;
                    if tree_extents.insert(key.objectid, refs).is_some() {
                        report.error(format!(
                            "tree block extent {} is recorded twice",
                            key.objectid
                        ));
                    }
                } else if refs.flags & EXTENT_FLAG_DATA != 0 && key.kind == EXTENT_ITEM {
                    if key.objectid.checked_add(key.offset).is_none() {
                        report.error(format!("data extent {key:?} overflows"));
                        continue;
                    }
                    parse_inline_refs(&item.data, 24, &mut refs)?;
                    if data_extents
                        .insert(key.objectid, (key.offset, refs))
                        .is_some()
                    {
                        report.error(format!("data extent {} is recorded twice", key.objectid));
                    }
                } else {
                    report.error(format!("extent {key:?} has flags {:#x}", refs.flags));
                }
            }
            TREE_BLOCK_REF => match tree_extents.get_mut(&key.objectid) {
                Some(refs) => refs.tree.push(key.offset),
                None => report.error(format!("back reference {key:?} has no extent")),
            },
            EXTENT_DATA_REF => match data_extents.get_mut(&key.objectid) {
                Some((_, refs)) => {
                    let triple = (
                        get64(&item.data, 0)?,
                        get64(&item.data, 8)?,
                        get64(&item.data, 16)?,
                    );
                    if crc32c::extent_data_ref_hash(triple.0, triple.1, triple.2) != key.offset {
                        report.error(format!("data back reference {key:?} has the wrong hash"));
                    }
                    let count = refs.data.entry(triple).or_default();
                    *count = count.saturating_add(u64::from(get32(&item.data, 24)?));
                }
                None => report.error(format!("back reference {key:?} has no extent")),
            },
            SHARED_BLOCK_REF | SHARED_DATA_REF => report.error(format!(
                "shared back reference {key:?} (snapshots are not supported)"
            )),
            BLOCK_GROUP_ITEM if !bgt => bg_items.push(item.clone()),
            _ => report.error(format!("unexpected extent tree item {key:?}")),
        }
    }
    if bgt {
        for item in trees.get(&BLOCK_GROUP_TREE).into_iter().flatten() {
            if item.key.kind == BLOCK_GROUP_ITEM {
                bg_items.push(item.clone());
            } else {
                report.error(format!("unexpected block group tree item {:?}", item.key));
            }
        }
    }
    for refs in tree_extents
        .values()
        .chain(data_extents.values().map(|(_, refs)| refs))
    {
        if refs.shared {
            report.error("shared back references are not supported (snapshots)");
        }
        if refs.generation > sb.generation {
            report.error("an extent is from a future generation");
        }
    }

    // Tree blocks against their extent items.
    for (address, block) in &owners {
        match tree_extents.get(address) {
            None => report.error(format!(
                "tree block {address} of tree {} has no extent item",
                block.owner
            )),
            Some(refs) => {
                if refs.level != Some(block.level) {
                    report.error(format!(
                        "tree block {address}'s extent records level {:?}, the block is {}",
                        refs.level, block.level
                    ));
                }
                if refs.refs != 1 || refs.tree != [block.owner] {
                    report.error(format!(
                        "tree block {address} of tree {} has references {} {:?}",
                        block.owner, refs.refs, refs.tree
                    ));
                }
                match volume.chunk(*address) {
                    Some(chunk) if block.owner == CHUNK_TREE && chunk.flags & BG_SYSTEM == 0 => {
                        report.error(format!(
                            "chunk tree block {address} is outside a system chunk"
                        ))
                    }
                    Some(chunk) if block.owner != CHUNK_TREE && chunk.flags & BG_METADATA == 0 => {
                        report.error(format!("tree block {address} is outside a metadata chunk"))
                    }
                    _ => {}
                }
            }
        }
    }
    for address in tree_extents.keys() {
        if !owners.contains_key(address) {
            report.error(format!("tree block extent {address} belongs to no tree"));
        }
    }

    // File extents against data extent items, and checksum coverage.
    let mut referenced: BTreeMap<u64, (u64, DataRefs)> = BTreeMap::new();
    let mut needs_csum: Vec<(u64, u64)> = Vec::new();
    let mut fs_trees: BTreeMap<u64, FsTree> = BTreeMap::new();
    for (id, items) in &trees {
        if *id == FS_TREE || (*id >= FIRST_FREE && *id <= LAST_FREE) || *id == DATA_RELOC_TREE {
            fs_trees.insert(*id, FsTree::parse(items)?);
        }
    }
    for (id, tree) in &fs_trees {
        for (ino, rec) in &tree.inodes {
            let nodatasum = rec
                .inode
                .as_ref()
                .is_some_and(|inode| inode.flags & INODE_NODATASUM != 0);
            for extent in &rec.extents {
                if extent.kind == FILE_EXTENT_INLINE || extent.disk_bytenr == 0 {
                    continue;
                }
                let entry = referenced
                    .entry(extent.disk_bytenr)
                    .or_insert_with(|| (extent.disk_num_bytes, BTreeMap::new()));
                if entry.0 != extent.disk_num_bytes {
                    report.error(format!(
                        "inode {ino} of tree {id} references extent {} with length {}",
                        extent.disk_bytenr, extent.disk_num_bytes
                    ));
                }
                *entry
                    .1
                    .entry((*id, *ino, extent.file_offset.wrapping_sub(extent.offset)))
                    .or_default() += 1;
                if extent.kind == FILE_EXTENT_REG && !nodatasum {
                    let start = extent.disk_bytenr.saturating_add(extent.offset);
                    needs_csum.push(if extent.compression == 0 {
                        (start, start.saturating_add(extent.num_bytes))
                    } else {
                        (
                            extent.disk_bytenr,
                            extent.disk_bytenr.saturating_add(extent.disk_num_bytes),
                        )
                    });
                }
            }
        }
    }
    for (bytenr, (len, actual)) in &referenced {
        match data_extents.get(bytenr) {
            None => report.error(format!(
                "data extent {bytenr} is referenced but has no extent item"
            )),
            Some((item_len, refs)) => {
                let total = actual.values().fold(0u64, |a, b| a.saturating_add(*b));
                if item_len != len {
                    report.error(format!(
                        "data extent {bytenr} is {item_len} bytes, referenced as {len}"
                    ));
                }
                if refs.refs != total || refs.data != *actual {
                    report.error(format!(
                        "data extent {bytenr} records {} references {:?}, the trees hold {total} {:?}",
                        refs.refs, refs.data, actual
                    ));
                }
                match volume.chunk(*bytenr) {
                    Some(chunk)
                        if chunk.flags & BG_DATA != 0
                            && bytenr.saturating_add(*len) <= chunk.logical + chunk.length => {}
                    _ => report.error(format!("data extent {bytenr} is not inside a data chunk")),
                }
            }
        }
    }
    for bytenr in data_extents.keys() {
        if !referenced.contains_key(bytenr) {
            report.error(format!("data extent {bytenr} is referenced by no file"));
        }
    }
    report.data_bytes = data_extents
        .values()
        .fold(0u64, |a, (len, _)| a.saturating_add(*len));

    // No two extents overlap.
    let mut allocated: Vec<(u64, u64)> = owners
        .keys()
        .map(|address| (*address, address + u64::from(NODESIZE)))
        .chain(
            data_extents
                .iter()
                .map(|(start, (len, _))| (*start, start.saturating_add(*len))),
        )
        .collect();
    allocated.sort();
    for pair in allocated.windows(2) {
        if let [a, b] = pair {
            if a.1 > b.0 {
                report.error(format!("extents at {} and {} overlap", a.0, b.0));
            }
        }
    }
    let allocated = Ranges::from(allocated);

    // Block groups.
    let mut used_total = 0u64;
    let mut groups = Vec::new();
    for item in &bg_items {
        let used = get64(&item.data, 0)?;
        let flags = get64(&item.data, 16)?;
        let (start, len) = (item.key.objectid, item.key.offset);
        groups.push((start, len, flags));
        used_total = used_total.saturating_add(used);
        match volume.chunks.iter().find(|chunk| chunk.logical == start) {
            Some(chunk) if chunk.length == len && chunk.flags == flags => {}
            _ => report.error(format!("block group {start} does not match a chunk")),
        }
        let actual: u64 = allocated
            .0
            .iter()
            .map(|(a, b)| {
                b.min(&start.saturating_add(len))
                    .saturating_sub(*a.max(&start))
            })
            .fold(0u64, u64::saturating_add);
        if actual != used {
            report.error(format!(
                "block group {start} records {used} bytes used, its extents hold {actual}"
            ));
        }
    }
    for chunk in &volume.chunks {
        if !groups.iter().any(|(start, _, _)| *start == chunk.logical) {
            report.error(format!("chunk {} has no block group", chunk.logical));
        }
    }
    if used_total != sb.bytes_used {
        report.error(format!(
            "the superblock records {} bytes used, the block groups {used_total}",
            sb.bytes_used
        ));
    }

    // Free space.
    if fst && sb.compat_ro & COMPAT_RO_FREE_SPACE_TREE_VALID != 0 {
        let items = trees.get(&FREE_SPACE_TREE).cloned().unwrap_or_default();
        let excused = Ranges::from(
            log_blocks
                .iter()
                .map(|block| (block.address, block.address + u64::from(NODESIZE)))
                .chain(super_stripes(&volume))
                .collect(),
        );
        check_free_space(&items, &groups, &allocated, &excused, &mut report)?;
    }

    // Checksums.
    let csum_items = trees.get(&CSUM_TREE).cloned().unwrap_or_default();
    let data_ranges = Ranges::from(
        data_extents
            .iter()
            .map(|(s, (l, _))| (*s, s.saturating_add(*l)))
            .collect(),
    );
    let mut buffer = vec![0u8; VERIFY_BUFFER];
    let mut csummed = Vec::new();
    for item in &csum_items {
        if item.key.objectid != EXTENT_CSUM
            || item.key.kind != EXTENT_CSUM_KEY
            || item.data.len() % CSUM_SIZE != 0
        {
            report.error(format!("unexpected checksum tree item {:?}", item.key));
            continue;
        }
        let start = item.key.offset;
        let end =
            start.saturating_add((item.data.len() / CSUM_SIZE) as u64 * u64::from(SECTORSIZE));
        if csummed
            .last()
            .is_some_and(|(_, last_end)| *last_end > start)
        {
            report.error(format!("checksum items overlap at {start}"));
        }
        csummed.push((start, end));
        if !data_ranges.covers(start, end) {
            report.error(format!("checksums for {start}..{end} cover no data extent"));
            continue;
        }
        report.csum_bytes += item.data.len() as u64;
        verify_data(&volume, start, &item.data, &mut buffer, &mut report)?;
    }
    let csummed = Ranges::from(csummed);
    for (start, end) in Ranges::from(needs_csum).0 {
        if !csummed.covers(start, end) {
            report.error(format!("data {start}..{end} has no checksums"));
        }
    }

    // Subvolumes and their directory trees.
    check_subvolumes(
        sb,
        &root_items,
        &root_records,
        &fs_trees,
        trees.get(&UUID_TREE),
        &mut report,
    )?;
    Ok(report)
}

fn check_mirrors(volume: &Volume, path: &Path, report: &mut Report) -> io::Result<()> {
    use std::os::unix::fs::FileExt;
    let file = std::fs::File::open(path)?;
    let mut primary = vec![0u8; SUPER_SIZE];
    file.read_exact_at(&mut primary, SUPER_OFFSET)?;
    for &offset in SUPER_MIRRORS.iter().skip(1) {
        if offset + SUPER_SIZE as u64 >= volume.sb.total_bytes {
            continue;
        }
        let mut copy = vec![0u8; SUPER_SIZE];
        file.read_exact_at(&mut copy, offset)?;
        let parsed = match Super::parse(&copy, offset) {
            Ok(parsed) => parsed,
            Err(error) => {
                report.error(format!("superblock copy at {offset}: {error}"));
                continue;
            }
        };
        // A log sync rewrites only the primary, adding its log root; a copy
        // of the same generation matches it but for those fields.
        let without_log = |raw: &[u8]| {
            let mut raw = raw.to_vec();
            for (at, len) in [(0, 32), (48, 8), (96, 16), (200, 1)] {
                if let Some(field) = raw.get_mut(at..at + len) {
                    field.fill(0);
                }
            }
            raw
        };
        if parsed.fsid != volume.sb.fsid {
            report.error(format!(
                "superblock copy at {offset} names another filesystem"
            ));
        } else if parsed.generation > volume.sb.generation {
            report.error(format!(
                "superblock copy at {offset} is newer than the primary"
            ));
        } else if parsed.generation == volume.sb.generation
            && without_log(&copy) != without_log(&primary)
        {
            report.error(format!(
                "superblock copy at {offset} differs from the primary"
            ));
        }
    }
    Ok(())
}

fn check_devices(volume: &Volume, items: &[ReadItem], report: &mut Report) {
    let mut extents: Vec<(u64, u64, u64)> = Vec::new();
    for item in items {
        match item.key.kind {
            DEV_EXTENT => {
                let fields = (get64(&item.data, 16), get64(&item.data, 24));
                match fields {
                    (Ok(chunk), Ok(len)) if item.key.objectid == 1 => {
                        extents.push((item.key.offset, len, chunk))
                    }
                    _ => report.error(format!("bad device extent {:?}", item.key)),
                }
            }
            PERSISTENT_ITEM => {}
            _ => report.error(format!("unexpected device tree item {:?}", item.key)),
        }
    }
    extents.sort();
    for pair in extents.windows(2) {
        if let [a, b] = pair {
            if a.0.saturating_add(a.1) > b.0 {
                report.error(format!("device extents at {} and {} overlap", a.0, b.0));
            }
        }
    }
    let mut stripes: Vec<(u64, u64, u64)> = volume
        .chunks
        .iter()
        .flat_map(|chunk| {
            chunk
                .stripes
                .iter()
                .map(move |s| (s.offset, chunk.length, chunk.logical))
        })
        .collect();
    stripes.sort();
    if stripes != extents {
        report.error("device extents do not match the chunks' stripes");
    }
    let used = extents
        .iter()
        .fold(0u64, |a, (_, len, _)| a.saturating_add(*len));
    if used != volume.sb.dev_bytes_used {
        report.error(format!(
            "the device records {} bytes used, its extents hold {used}",
            volume.sb.dev_bytes_used
        ));
    }
    if extents
        .iter()
        .any(|(start, len, _)| start.saturating_add(*len) > volume.sb.dev_total_bytes)
    {
        report.error("a device extent ends past the device");
    }
    if extents
        .iter()
        .any(|(start, _, _)| *start < crate::layout::RESERVED)
    {
        report.error("a device extent is in the first megabyte");
    }
}

/// The logical ranges the kernel excludes around each superblock copy.
fn super_stripes(volume: &Volume) -> Vec<(u64, u64)> {
    let mut out = Vec::new();
    for chunk in &volume.chunks {
        for stripe in &chunk.stripes {
            for &mirror in SUPER_MIRRORS {
                if mirror >= stripe.offset && mirror < stripe.offset + chunk.length {
                    let logical =
                        chunk.logical + (mirror - stripe.offset) / STRIPE_LEN * STRIPE_LEN;
                    out.push((
                        logical,
                        logical
                            .saturating_add(SUPER_EXCLUDE)
                            .min(chunk.logical + chunk.length),
                    ));
                }
            }
        }
    }
    out
}

fn check_free_space(
    items: &[ReadItem],
    groups: &[(u64, u64, u64)],
    allocated: &Ranges,
    excused: &Ranges,
    report: &mut Report,
) -> io::Result<()> {
    let sector = u64::from(SECTORSIZE);
    let mut infos: BTreeMap<u64, (u64, u32, u32)> = BTreeMap::new();
    let mut free: Vec<(u64, u64)> = Vec::new();
    let mut counted: BTreeMap<u64, u32> = BTreeMap::new();
    let mut current: Option<(u64, u64)> = None;
    for item in items {
        let key = item.key;
        // Every free space key is a range; one that overflows bounds nothing.
        // Past this, each sum below stays at or under `end`.
        let Some(end) = key.objectid.checked_add(key.offset) else {
            report.error(format!("free space item {key:?} overflows"));
            continue;
        };
        match key.kind {
            FREE_SPACE_INFO => {
                infos.insert(
                    key.objectid,
                    (key.offset, get32(&item.data, 0)?, get32(&item.data, 4)?),
                );
                current = Some((key.objectid, end));
            }
            FREE_SPACE_EXTENT | FREE_SPACE_BITMAP => {
                let Some((start, _)) = current.filter(|(s, e)| key.objectid >= *s && end <= *e)
                else {
                    report.error(format!(
                        "free space item {key:?} is outside its block group"
                    ));
                    continue;
                };
                if key.kind == FREE_SPACE_EXTENT {
                    free.push((key.objectid, end));
                    *counted.entry(start).or_default() += 1;
                } else {
                    let bits = key.offset / sector;
                    if item.data.len() as u64 != bits.div_ceil(8) {
                        report.error(format!("free space bitmap {key:?} has the wrong size"));
                        continue;
                    }
                    for bit in 0..bits {
                        let byte = item
                            .data
                            .get((bit / 8) as usize)
                            .copied()
                            .unwrap_or_default();
                        if byte & (1 << (bit % 8)) != 0 {
                            let at = key.objectid + bit * sector;
                            free.push((at, at + sector));
                        }
                    }
                }
            }
            _ => report.error(format!("unexpected free space tree item {key:?}")),
        }
    }
    let free = Ranges::from(free);
    for &(start, len, _) in groups {
        let Some(&(info_len, count, flags)) = infos.get(&start) else {
            report.error(format!("block group {start} has no free space info"));
            continue;
        };
        if info_len != len {
            report.error(format!("free space info for {start} has length {info_len}"));
        }
        if flags & FREE_SPACE_USING_BITMAPS == 0
            && counted.get(&start).copied().unwrap_or(0) != count
        {
            report.error(format!(
                "free space info for {start} counts {count} extents"
            ));
        }
    }
    let whole = Ranges::from(
        groups
            .iter()
            .map(|(s, l, _)| (*s, s.saturating_add(*l)))
            .collect(),
    );
    let expected = whole.minus(allocated);
    let missing = expected.minus(&free).minus(excused);
    let extra = free.minus(&expected).minus(excused);
    if let Some((start, end)) = missing.0.first() {
        report.error(format!("free space {start}..{end} is not recorded free"));
    }
    if let Some((start, end)) = extra.0.first() {
        report.error(format!("{start}..{end} is recorded free but is allocated"));
    }
    Ok(())
}

fn verify_data(
    volume: &Volume,
    start: u64,
    sums: &[u8],
    buffer: &mut [u8],
    report: &mut Report,
) -> io::Result<()> {
    let sector = u64::from(SECTORSIZE);
    let total = (sums.len() / CSUM_SIZE) as u64;
    let mut done = 0u64;
    while done < total {
        let at = start.saturating_add(done * sector);
        let chunk = volume
            .chunk(at)
            .ok_or_else(|| invalid(format!("checksummed data at {at} is in no chunk")))?;
        // A run stays inside one chunk: the kernel extends a checksum item
        // across the boundary of two adjacent data chunks.
        let run = (total - done)
            .min((chunk.logical + chunk.length - at) / sector)
            .min(buffer.len() as u64 / sector);
        if run == 0 {
            return Err(invalid(format!(
                "checksummed data at {at} is not sector aligned"
            )));
        }
        let data = buffer
            .get_mut(..(run * sector) as usize)
            .ok_or_else(|| invalid("verify buffer too small"))?;
        let expected = sums
            .get(done as usize * CSUM_SIZE..(done + run) as usize * CSUM_SIZE)
            .ok_or_else(|| invalid("checksum run out of range"))?;
        // Every copy: a DUP data chunk's second stripe is checked too.
        for copy in 0..chunk.stripes.len() {
            volume.read_stripe_into(at, copy, data)?;
            for (index, (block, sum)) in data
                .chunks(sector as usize)
                .zip(expected.chunks(CSUM_SIZE))
                .enumerate()
            {
                if crc32c::checksum(block).to_le_bytes() != sum {
                    report.error(format!(
                        "data at {} (copy {copy}) fails its checksum",
                        at.saturating_add(index as u64 * sector)
                    ));
                }
            }
        }
        done += run;
    }
    Ok(())
}

fn check_subvolumes(
    sb: &Super,
    root_items: &[ReadItem],
    roots: &BTreeMap<u64, Root>,
    fs_trees: &BTreeMap<u64, FsTree>,
    uuid_items: Option<&Vec<ReadItem>>,
    report: &mut Report,
) -> io::Result<()> {
    let mut refs: HashMap<(u64, u64), (u64, u64, Vec<u8>)> = HashMap::new();
    let mut backrefs: HashMap<(u64, u64), (u64, u64, Vec<u8>)> = HashMap::new();
    for item in root_items {
        let key = item.key;
        if key.kind == ROOT_REF || key.kind == ROOT_BACKREF {
            let len = usize::from(get16(&item.data, 16)?);
            let value = (
                get64(&item.data, 0)?,
                get64(&item.data, 8)?,
                slice(&item.data, 18, len)?.to_vec(),
            );
            if key.kind == ROOT_REF {
                refs.insert((key.objectid, key.offset), value);
            } else {
                backrefs.insert((key.offset, key.objectid), value);
            }
        }
    }
    if refs != backrefs {
        report.error("root references and back references disagree");
    }
    let subvols: Vec<u64> = fs_trees
        .keys()
        .copied()
        .filter(|id| *id == FS_TREE || (*id >= FIRST_FREE && *id <= LAST_FREE))
        .collect();
    report.subvolumes = subvols.len();
    for id in &subvols {
        if *id != FS_TREE && backrefs.keys().filter(|(_, child)| child == id).count() != 1 {
            report.error(format!("subvolume {id} does not have exactly one parent"));
        }
    }
    // Every entry naming a subvolume, as (parent tree, dir, index, name, child).
    let mut subvol_entries: Vec<(u64, u64, u64, Vec<u8>, u64)> = Vec::new();
    for (id, tree) in fs_trees {
        let root_dir = roots
            .get(id)
            .map(|root| root.root_dirid)
            .unwrap_or(FIRST_FREE);
        check_tree(
            *id,
            root_dir,
            tree,
            sb.generation,
            &mut subvol_entries,
            report,
        );
        report.inodes += tree.inodes.len();
    }
    for (tree, dir, index, name, child) in &subvol_entries {
        match refs.get(&(*tree, *child)) {
            Some((dirid, sequence, ref_name))
                if dirid == dir && sequence == index && ref_name == name => {}
            _ => report.error(format!(
                "subvolume {child}'s entry in tree {tree} has no matching root reference"
            )),
        }
        if !roots.contains_key(child) {
            report.error(format!("tree {tree} names a missing subvolume {child}"));
        }
    }
    for (tree, child) in refs.keys() {
        if !subvol_entries
            .iter()
            .any(|(t, _, _, _, c)| t == tree && c == child)
        {
            report.error(format!(
                "subvolume {child} has no directory entry in tree {tree}"
            ));
        }
    }
    if let Some(items) = uuid_items {
        for item in items {
            let key = item.key;
            let mut uuid = [0u8; 16];
            uuid[..8].copy_from_slice(&key.objectid.to_le_bytes());
            uuid[8..].copy_from_slice(&key.offset.to_le_bytes());
            if key.kind != UUID_KEY_SUBVOL && key.kind != UUID_KEY_RECEIVED_SUBVOL {
                report.error(format!("unexpected uuid tree item {key:?}"));
                continue;
            }
            for id in item.data.chunks(8) {
                let id = u64::from_le_bytes(id.try_into().unwrap_or_default());
                match roots.get(&id) {
                    Some(root) if key.kind == UUID_KEY_RECEIVED_SUBVOL || root.uuid == uuid => {}
                    _ => report.error(format!(
                        "uuid tree entry {key:?} names subvolume {id}, which does not carry it"
                    )),
                }
            }
        }
    }
    Ok(())
}

fn check_tree(
    id: u64,
    root_dir: u64,
    tree: &FsTree,
    generation: u64,
    subvol_entries: &mut Vec<(u64, u64, u64, Vec<u8>, u64)>,
    report: &mut Report,
) {
    let mut error = |message: String| report.errors.push(format!("tree {id}: {message}"));
    for key in &tree.unknown {
        error(format!("unexpected item {key:?}"));
    }
    let orphans: HashSet<u64> = tree.orphans.iter().copied().collect();
    let sector = u64::from(SECTORSIZE);
    for (ino, rec) in &tree.inodes {
        let Some(inode) = &rec.inode else {
            error(format!("inode {ino} has items but no inode item"));
            continue;
        };
        let newest = generation.saturating_add(1);
        if inode.generation > newest || inode.transid > newest {
            error(format!("inode {ino} is from a future generation"));
        }
        let kind = mode_kind(inode.mode);
        // Each link must be a directory entry naming this inode.
        for (parent, index, name) in &rec.refs {
            if *ino == root_dir && *parent == root_dir {
                if *index != 0 || name != b".." {
                    error(format!("root directory {ino} has a bad self reference"));
                }
                continue;
            }
            let entry = tree.inodes.get(parent).and_then(|dir| dir.index.get(index));
            match entry {
                Some(entry)
                    if entry.name == *name && entry.location == Key::new(*ino, INODE_ITEM, 0) => {}
                _ => error(format!(
                    "inode {ino}'s link {:?} in {parent} has no matching directory entry",
                    String::from_utf8_lossy(name)
                )),
            }
        }
        let links = rec
            .refs
            .iter()
            .filter(|(parent, _, _)| !(*ino == root_dir && *parent == root_dir))
            .count() as u32;
        if *ino == root_dir {
            if kind != FT_DIR || rec.refs.len() != 1 {
                error(format!(
                    "root directory {ino} is not a directory with only its self reference"
                ));
            }
        } else if kind == FT_DIR {
            // A directory removed while open is an orphan with no link,
            // which the kernel finishes deleting at the next mount.
            let removed = links == 0 && inode.nlink == 0 && orphans.contains(ino);
            if links != 1 && !removed {
                error(format!("directory {ino} has {links} links"));
            }
        } else if links != inode.nlink {
            error(format!(
                "inode {ino} records {} links, {links} exist",
                inode.nlink
            ));
        }
        if kind == FT_DIR && inode.nlink != 1 && !(inode.nlink == 0 && orphans.contains(ino)) {
            error(format!("directory {ino} records {} links", inode.nlink));
        }
        // Directory entries.
        let mut names = 0u64;
        let mut named = HashSet::new();
        for (index, entry) in &rec.index {
            names += entry.name.len() as u64;
            if !named.insert(&entry.name) {
                error(format!(
                    "directory {ino} names {:?} twice",
                    String::from_utf8_lossy(&entry.name)
                ));
            }
            let hashed = rec
                .hashed
                .iter()
                .any(|(hash, one)| *hash == crate::crc32c::name_hash(&one.name) && one == entry);
            if !hashed {
                error(format!(
                    "directory {ino} index {index} has no matching DIR_ITEM"
                ));
            }
            match entry.location.kind {
                INODE_ITEM => {
                    let target = tree
                        .inodes
                        .get(&entry.location.objectid)
                        .and_then(|t| t.inode.as_ref());
                    match target {
                        Some(target) if mode_kind(target.mode) == entry.kind => {}
                        Some(_) => {
                            error(format!("directory {ino} entry {index} has the wrong type"))
                        }
                        None => error(format!(
                            "directory {ino} entry {index} names a missing inode"
                        )),
                    }
                    let backed = tree.inodes.get(&entry.location.objectid).is_some_and(|t| {
                        t.refs
                            .iter()
                            .any(|(p, i, n)| p == ino && i == index && *n == entry.name)
                    });
                    if !backed {
                        error(format!(
                            "directory {ino} entry {index} has no inode reference"
                        ));
                    }
                }
                ROOT_ITEM if entry.kind == FT_DIR => subvol_entries.push((
                    id,
                    *ino,
                    *index,
                    entry.name.clone(),
                    entry.location.objectid,
                )),
                _ => error(format!(
                    "directory {ino} entry {index} has location {:?}",
                    entry.location
                )),
            }
        }
        for (hash, entry) in &rec.hashed {
            names += entry.name.len() as u64;
            if *hash != crate::crc32c::name_hash(&entry.name) {
                error(format!("directory {ino} DIR_ITEM has the wrong hash"));
            }
            if !rec.index.values().any(|one| one == entry) {
                error(format!(
                    "directory {ino} DIR_ITEM {:?} has no index",
                    String::from_utf8_lossy(&entry.name)
                ));
            }
        }
        if kind == FT_DIR && inode.size != names {
            error(format!(
                "directory {ino} records size {}, its names total {names}",
                inode.size
            ));
        }
        if kind != FT_DIR && !(rec.index.is_empty() && rec.hashed.is_empty()) {
            error(format!("non-directory {ino} has directory entries"));
        }
        // File extents.
        let mut nbytes = 0u64;
        let mut end = 0u64;
        for extent in &rec.extents {
            if extent.compression != 0 || extent.encryption != 0 || extent.other_encoding != 0 {
                error(format!(
                    "inode {ino} has a compressed or encoded extent (unsupported)"
                ));
                continue;
            }
            if extent.file_offset < end {
                error(format!(
                    "inode {ino}'s extents overlap at {}",
                    extent.file_offset
                ));
            }
            if extent.file_offset % sector != 0 {
                error(format!(
                    "inode {ino} has an unaligned extent at {}",
                    extent.file_offset
                ));
            }
            match extent.kind {
                FILE_EXTENT_INLINE => {
                    if extent.file_offset != 0 || rec.extents.len() != 1 {
                        error(format!(
                            "inode {ino} has an inline extent that is not its only one"
                        ));
                    }
                    if extent.inline.len() as u64 != extent.ram_bytes {
                        error(format!(
                            "inode {ino}'s inline extent records {} bytes, holds {}",
                            extent.ram_bytes,
                            extent.inline.len()
                        ));
                    }
                    nbytes = nbytes.saturating_add(extent.ram_bytes);
                    end = extent.ram_bytes;
                }
                _ => {
                    if extent.num_bytes == 0 || extent.num_bytes % sector != 0 {
                        error(format!(
                            "inode {ino} has an extent of {} bytes",
                            extent.num_bytes
                        ));
                    }
                    if extent.disk_bytenr != 0 {
                        nbytes = nbytes.saturating_add(extent.num_bytes);
                        let within = extent
                            .offset
                            .checked_add(extent.num_bytes)
                            .is_some_and(|used| used <= extent.disk_num_bytes);
                        if !within {
                            error(format!(
                                "inode {ino}'s extent at {} reaches past its disk extent",
                                extent.file_offset
                            ));
                        }
                    }
                    // The kernel's tree checker refuses an extent whose end
                    // overflows; nothing after it would compare against it.
                    end = match extent.file_offset.checked_add(extent.num_bytes) {
                        Some(next) => next,
                        None => {
                            error(format!(
                                "inode {ino}'s extent at {} overflows",
                                extent.file_offset
                            ));
                            u64::MAX
                        }
                    };
                }
            }
        }
        if kind == FT_SYMLINK {
            let ok = matches!(rec.extents.as_slice(), [one] if one.kind == FILE_EXTENT_INLINE && (one.ram_bytes == inode.size || one.ram_bytes == inode.size.saturating_add(1)) && inode.size > 0);
            if !ok {
                error(format!("symlink {ino} does not hold its target inline"));
            }
        }
        if (kind == FT_DIR) && !rec.extents.is_empty() {
            error(format!("directory {ino} has file extents"));
        }
        if nbytes != inode.nbytes && kind != FT_DIR {
            error(format!(
                "inode {ino} records {} bytes, its extents hold {nbytes}",
                inode.nbytes
            ));
        }
    }
    // Every inode is reached from the root directory, or is an orphan: link
    // counts alone pass a detached cycle of directories.
    let mut reached = HashSet::from([root_dir]);
    let mut pending = vec![root_dir];
    while let Some(dir) = pending.pop() {
        let Some(rec) = tree.inodes.get(&dir) else {
            continue;
        };
        for entry in rec.index.values() {
            if entry.location.kind == INODE_ITEM {
                let child = entry.location.objectid;
                let is_dir = tree
                    .inodes
                    .get(&child)
                    .and_then(|t| t.inode.as_ref())
                    .is_some_and(|t| mode_kind(t.mode) == FT_DIR);
                if reached.insert(child) && is_dir {
                    pending.push(child);
                }
            }
        }
    }
    for (ino, rec) in &tree.inodes {
        if rec.inode.is_some() && !reached.contains(ino) && !orphans.contains(ino) {
            error(format!("inode {ino} is unreachable and not an orphan"));
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn item(objectid: u64, kind: u8, offset: u64, data: Vec<u8>) -> ReadItem {
        ReadItem {
            key: Key::new(objectid, kind, offset),
            data,
            leaf: 0,
        }
    }

    /// A file's last extent ending past `u64::MAX`, which the kernel's tree
    /// checker refuses, is an error though nothing follows it.
    #[test]
    fn a_file_extent_whose_end_overflows_is_refused() {
        use crate::fstree::{FileExtent, InodeRec};
        let extent = FileExtent {
            file_offset: 0xffff_ffff_ffff_f000,
            kind: FILE_EXTENT_REG,
            compression: 0,
            encryption: 0,
            other_encoding: 0,
            ram_bytes: 8192,
            inline: Vec::new(),
            disk_bytenr: 0,
            disk_num_bytes: 0,
            offset: 0,
            num_bytes: 8192,
            leaf: 0,
        };
        let mut tree = FsTree::default();
        tree.inodes.insert(
            257,
            InodeRec {
                inode: Some(Inode {
                    nlink: 0,
                    mode: S_IFREG | 0o644,
                    ..Inode::default()
                }),
                extents: vec![extent],
                ..InodeRec::default()
            },
        );
        tree.orphans.push(257);
        let mut report = Report::default();
        check_tree(5, 256, &tree, 1, &mut Vec::new(), &mut report);
        assert!(
            report.errors.iter().any(|e| e.contains("overflows")),
            "{:#?}",
            report.errors
        );
    }

    /// A free space info spanning to the top of the address space and a
    /// bitmap past it: the bitmap is refused, not summed past `u64::MAX`.
    #[test]
    fn a_free_space_range_that_overflows_is_refused() {
        let top = u64::MAX - 8191;
        let items = [
            item(0, FREE_SPACE_INFO, u64::MAX, vec![0; 8]),
            item(top, FREE_SPACE_BITMAP, 32768, vec![0xff]),
        ];
        let mut report = Report::default();
        check_free_space(
            &items,
            &[],
            &Ranges::from(Vec::new()),
            &Ranges::from(Vec::new()),
            &mut report,
        )
        .unwrap();
        assert_eq!(
            report
                .errors
                .iter()
                .filter(|e| e.contains("overflows"))
                .count(),
            1,
            "{:#?}",
            report.errors
        );
    }
}

//! td-fat: td's bounded FAT32 code for its own EFI system partition
//! (td-fat/DESIGN.md). It admits the ESP td's layout writes, reads a file's
//! chain, reclaims clusters no entry reaches, stages a new selector pair and
//! commits one with a single write of the `EFI/BOOT` directory's first
//! sector (td-install/ENCRYPTION.md, "Selector update"). Pure `std`, no
//! `unsafe`, no dependency.
#![forbid(unsafe_code)]
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic
    )
)]

use std::fmt;
use std::io::{self, Read};

/// The four names an update touches, as 11-byte short-name fields.
pub const KERNEL: [u8; 11] = *b"BOOTX64 EFI";
pub const INITRD: [u8; 11] = *b"INITRD     ";
pub const STAGED_KERNEL: [u8; 11] = *b"BOOTX64 NEW";
pub const STAGED_INITRD: [u8; 11] = *b"INITRD  NEW";
const EFI_DIR: [u8; 11] = *b"EFI        ";
const BOOT_DIR: [u8; 11] = *b"BOOT       ";
const DOT: [u8; 11] = *b".          ";
const DOTDOT: [u8; 11] = *b"..         ";

/// FAT32's floor: below it a volume is FAT16 whatever its BPB says.
pub const MIN_CLUSTERS: u32 = 65_525;
/// td-fat's ceiling, which bounds the two FATs it holds (4 MiB each) and its
/// reachability map. td's 512 MiB ESP has about 65 536.
pub const MAX_CLUSTERS: u32 = 1 << 20;
/// Directory nesting walked below the root.
pub const MAX_DEPTH: usize = 8;
/// Directory entries walked in all, deleted and long-name ones included.
pub const MAX_ENTRIES: usize = 1 << 16;
/// The largest cluster FAT allows.
pub const MAX_CLUSTER_BYTES: u32 = 32 * 1024;
/// Bytes read or written per device call while streaming a file.
const CHUNK: usize = 1 << 20;

const ENTRY: usize = 32;
/// Every entry an update writes lies in the directory's first 512 bytes,
/// the smallest logical sector a device has, so a commit's one changed unit
/// is whole on power loss whatever sector size the BPB claims.
const ATOMIC_ENTRIES: usize = 512 / ENTRY;
const MASK: u32 = 0x0fff_ffff;
const BAD: u32 = 0x0fff_fff7;
const EOC_MIN: u32 = 0x0fff_fff8;
const EOC: u32 = 0x0fff_ffff;
const ATTR_VOLUME: u8 = 0x08;
const ATTR_DIRECTORY: u8 = 0x10;
const ATTR_ARCHIVE: u8 = 0x20;
const ATTR_LONG_NAME: u8 = 0x0f;
/// The attribute bits a long-name entry's test reads: an entry is one only
/// when these are exactly `ATTR_LONG_NAME`, so a directory or archive bit
/// beside the low four leaves it to `kind`, which calls it damage.
const ATTR_LONG_NAME_MASK: u8 = 0x3f;
const DELETED: u8 = 0xe5;
const END: u8 = 0x00;
/// 1980-01-01, the write date td's layout gives every entry
/// (`engine/src/fat.rs`'s `FAT_EPOCH_DATE`).
const FAT_EPOCH_DATE: u16 = 0x0021;
const FSINFO_SECTOR: u32 = 1;
const BACKUP_BOOT_SECTOR: u32 = 6;
const FSINFO_UNKNOWN: u32 = 0xffff_ffff;

/// Positioned I/O on the device that holds the ESP. Every call is whole:
/// a short read or write is an error.
pub trait Device {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()>;
    fn write_at(&mut self, offset: u64, buf: &[u8]) -> io::Result<()>;
    /// Make every write so far durable before any later one.
    fn sync(&mut self) -> io::Result<()>;
}

impl Device for std::fs::File {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        std::os::unix::fs::FileExt::read_exact_at(self, buf, offset)
    }
    fn write_at(&mut self, offset: u64, buf: &[u8]) -> io::Result<()> {
        std::os::unix::fs::FileExt::write_all_at(self, buf, offset)
    }
    fn sync(&mut self) -> io::Result<()> {
        self.sync_data()
    }
}

/// Why td-fat refused. Nothing is written before a refusal that can be
/// decided by reading, so every kind but `Io` leaves the ESP untouched when
/// it comes from `open` or the checks an operation makes first.
#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    /// Not a FAT32 volume td-fat admits.
    Volume(&'static str),
    /// `EFI/BOOT` is not the directory td's layout writes.
    Foreign(&'static str),
    /// Chains, FATs or directories disagree.
    Damaged(&'static str),
    /// Free and reclaimable clusters cannot hold the new pair beside the old.
    Space {
        needed: u32,
        available: u32,
    },
    /// The directory sector is not the one the caller read.
    Changed,
    /// A file the caller supplied is empty, too large, short or long.
    Input(&'static str),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "ESP I/O failed: {e}"),
            Self::Volume(why) => write!(f, "the ESP is not a FAT32 volume td admits: {why}"),
            Self::Foreign(why) => write!(f, "the ESP's EFI/BOOT is not td's: {why}"),
            Self::Damaged(why) => write!(f, "the ESP is damaged: {why}"),
            Self::Space { needed, available } => write!(
                f,
                "the ESP has {available} clusters for the {needed} the new pair needs"
            ),
            Self::Changed => write!(f, "the ESP's EFI/BOOT changed since it was read"),
            Self::Input(why) => write!(f, "a new boot file is unusable: {why}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// One file entry of `EFI/BOOT`'s first sector.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FileEntry {
    /// Index of the 32-byte entry within the first sector.
    pub index: usize,
    pub first_cluster: u32,
    pub size: u32,
}

/// A kernel and initramfs pair.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Pair {
    pub kernel: FileEntry,
    pub initrd: FileEntry,
}

/// `EFI/BOOT`'s first sector as it was read, with the pair it names and the
/// staged pair beside it, if any. An operation that rewrites the sector
/// requires the device to still hold exactly these bytes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BootDir {
    offset: u64,
    sector: Vec<u8>,
    pub pair: Pair,
    pub staged: Option<Pair>,
}

/// A new file to write: exactly `len` bytes from `reader`, then its end.
pub struct Input<'a> {
    pub len: u64,
    pub reader: &'a mut dyn Read,
}

#[derive(Clone, Copy, Debug)]
struct Geometry {
    bps: u32,
    spc: u32,
    reserved: u32,
    fat_sectors: u32,
    clusters: u32,
    root: u32,
}

impl Geometry {
    fn cluster_bytes(&self) -> u32 {
        self.bps.saturating_mul(self.spc)
    }

    /// Offset of the FAT `which` (0 or 1) from the volume's start.
    fn fat_offset(&self, which: u32) -> Option<u64> {
        u64::from(self.reserved)
            .checked_add(u64::from(which).checked_mul(u64::from(self.fat_sectors))?)?
            .checked_mul(u64::from(self.bps))
    }

    fn cluster_offset(&self, cluster: u32) -> Option<u64> {
        let index = cluster.checked_sub(2)?;
        if index >= self.clusters {
            return None;
        }
        u64::from(self.reserved)
            .checked_add(u64::from(self.fat_sectors).checked_mul(2)?)?
            .checked_mul(u64::from(self.bps))?
            .checked_add(u64::from(index).checked_mul(u64::from(self.cluster_bytes()))?)
    }

    fn last_cluster(&self) -> u32 {
        self.clusters.saturating_add(1)
    }

    /// Clusters a file of `len` bytes occupies.
    fn clusters_for(&self, len: u64) -> Option<u32> {
        let cb = u64::from(self.cluster_bytes());
        u32::try_from(len.checked_add(cb.checked_sub(1)?)? / cb).ok()
    }
}

/// The admitted ESP: its geometry, both FATs, which clusters a directory
/// entry reaches, and `EFI/BOOT`. One value serves one operation; open the
/// ESP again for the next.
pub struct Esp<D: Device> {
    dev: D,
    base: u64,
    geo: Geometry,
    /// The FATs' first `clusters + 2` entries, in whole sectors.
    fats: [Vec<u8>; 2],
    /// Per cluster: 0 unreached, 1 reached by an entry, 2 reached only by the
    /// staged pair.
    reach: Vec<u8>,
    boot: BootDir,
}

impl<D: Device> Esp<D> {
    /// Admit the FAT32 volume of `len` bytes at `base` on `dev`.
    pub fn open(dev: D, base: u64, len: u64) -> Result<Self> {
        let geo = read_geometry(&dev, base, len)?;
        let fats = read_fats(&dev, base, &geo)?;
        let mut esp = Self {
            dev,
            base,
            geo,
            fats,
            reach: Vec::new(),
            boot: BootDir {
                offset: 0,
                sector: Vec::new(),
                pair: Pair {
                    kernel: FileEntry {
                        index: 0,
                        first_cluster: 0,
                        size: 0,
                    },
                    initrd: FileEntry {
                        index: 0,
                        first_cluster: 0,
                        size: 0,
                    },
                },
                staged: None,
            },
        };
        esp.walk()?;
        Ok(esp)
    }

    /// Give the device back.
    pub fn into_device(self) -> D {
        self.dev
    }

    pub fn boot(&self) -> &BootDir {
        &self.boot
    }

    pub fn bytes_per_sector(&self) -> u32 {
        self.geo.bps
    }

    pub fn cluster_bytes(&self) -> u32 {
        self.geo.cluster_bytes()
    }

    pub fn cluster_count(&self) -> u32 {
        self.geo.clusters
    }

    /// Clusters free in both FATs.
    pub fn free_clusters(&self) -> u32 {
        self.count(|esp, c| esp.is_free(c))
    }

    /// Clusters allocated in either FAT that no directory entry reaches and
    /// that are not marked bad: what the next update reclaims.
    pub fn lost_clusters(&self) -> u32 {
        self.count(|esp, c| esp.reach_of(c) == 0 && !esp.is_free(c) && !esp.is_bad(c))
    }

    /// Stream `file`'s bytes to `sink`, in chain order, at most one MiB at a
    /// time, exactly `file.size` bytes in all.
    pub fn read(
        &self,
        file: &FileEntry,
        sink: &mut dyn FnMut(&[u8]) -> io::Result<()>,
    ) -> Result<()> {
        let chain = self.chain(file.first_cluster)?;
        let mut left = u64::from(file.size);
        let mut buf = Vec::new();
        for (offset, len) in self.runs(&chain)? {
            if left == 0 {
                break;
            }
            let take = usize::try_from(len.min(left))
                .map_err(|_| Error::Damaged("a run exceeds memory"))?;
            buf.resize(take, 0);
            self.dev.read_at(self.at(offset)?, &mut buf)?;
            sink(&buf)?;
            left = left.saturating_sub(len);
        }
        if left != 0 {
            return Err(Error::Damaged("a chain is shorter than its file"));
        }
        Ok(())
    }

    /// Stage `kernel` and `initrd` as `BOOTX64.NEW` and `INITRD.NEW`: remove
    /// an earlier staged pair, reclaim, write the files and their chains,
    /// then add both entries in one write of the directory's first sector.
    pub fn stage(&mut self, kernel: Input<'_>, initrd: Input<'_>) -> Result<()> {
        let (kslot, islot) = self.staging_slots()?;
        let (k, i) = self.prepare(kernel, initrd)?;
        let mut sector = self.boot.sector.clone();
        put_entry(&mut sector, kslot, &STAGED_KERNEL, k)?;
        put_entry(&mut sector, islot, &STAGED_INITRD, i)?;
        self.write_sector(&sector)?;
        self.finish()
    }

    /// Whether a pair of these sizes would be written, in the staged form
    /// or the direct one, with nothing written: the checks `stage` and
    /// `write_direct` make before their first write, so a caller can refuse
    /// before it asks anyone to approve the update.
    pub fn fits(&self, kernel: u64, initrd: u64, staged: bool) -> Result<()> {
        if staged {
            self.staging_slots()?;
        }
        self.clusters_needed(kernel, initrd).map(|_| ())
    }

    /// The staged pair's two slots, chosen from the sector as it will be
    /// once an earlier staged pair is deleted, before anything is written.
    fn staging_slots(&self) -> Result<(usize, usize)> {
        let mut cleared = self.boot.sector.clone();
        if let Some(staged) = self.boot.staged {
            delete_entry(&mut cleared, staged.kernel.index)?;
            delete_entry(&mut cleared, staged.initrd.index)?;
        }
        let slots: Vec<usize> = (2..ATOMIC_ENTRIES.min(cleared.len() / ENTRY))
            .filter(|index| {
                matches!(
                    cleared.get(index * ENTRY).copied(),
                    Some(END) | Some(DELETED)
                )
            })
            .take(2)
            .collect();
        match slots.as_slice() {
            &[kslot, islot] => Ok((kslot, islot)),
            _ => Err(Error::Foreign(
                "no room for two entries in its first 512 bytes",
            )),
        }
    }

    /// Each file's length and cluster count, once the space check passes:
    /// what an earlier staged pair's deletion and the reclaim will leave
    /// free must hold both files.
    fn clusters_needed(&self, kernel: u64, initrd: u64) -> Result<((u32, u32), (u32, u32))> {
        let klen = file_len(kernel)?;
        let ilen = file_len(initrd)?;
        let kc = self
            .geo
            .clusters_for(u64::from(klen))
            .ok_or(Error::Input("its size overflows"))?;
        let ic = self
            .geo
            .clusters_for(u64::from(ilen))
            .ok_or(Error::Input("its size overflows"))?;
        let needed = kc
            .checked_add(ic)
            .ok_or(Error::Input("its size overflows"))?;
        let available = self.count(|esp, c| esp.reach_of(c) != 1 && !esp.is_bad(c));
        if needed > available {
            return Err(Error::Space { needed, available });
        }
        Ok(((klen, kc), (ilen, ic)))
    }

    /// The direct form: write the new pair and point `BOOTX64.EFI` and
    /// `INITRD` at it in one sector write, then free the old chains.
    pub fn write_direct(&mut self, kernel: Input<'_>, initrd: Input<'_>) -> Result<()> {
        let (k, i) = self.prepare(kernel, initrd)?;
        let old = self.boot.pair;
        let mut sector = self.boot.sector.clone();
        set_target(&mut sector, old.kernel.index, k)?;
        set_target(&mut sector, old.initrd.index, i)?;
        self.write_sector(&sector)?;
        self.free_chains(&[old.kernel.first_cluster, old.initrd.first_cluster])?;
        self.finish()
    }

    /// The staged form's commit: point `BOOTX64.EFI` and `INITRD` at the
    /// first clusters and sizes `read` named and delete the staged entries,
    /// in one sector write, then free the old chains. `read` is the
    /// directory as the caller read and hashed it; the device must still
    /// hold exactly that sector.
    pub fn commit_staged(&mut self, read: &BootDir) -> Result<()> {
        self.require_unchanged(read)?;
        let Some(staged) = read.staged else {
            return Err(Error::Foreign("it holds no staged pair"));
        };
        let old = read.pair;
        let mut sector = read.sector.clone();
        set_target(
            &mut sector,
            old.kernel.index,
            (staged.kernel.first_cluster, staged.kernel.size),
        )?;
        set_target(
            &mut sector,
            old.initrd.index,
            (staged.initrd.first_cluster, staged.initrd.size),
        )?;
        delete_entry(&mut sector, staged.kernel.index)?;
        delete_entry(&mut sector, staged.initrd.index)?;
        self.write_sector(&sector)?;
        self.free_chains(&[old.kernel.first_cluster, old.initrd.first_cluster])?;
        self.finish()
    }

    /// Delete the staged entries `read` names in one sector write, then free
    /// their chains.
    pub fn delete_staged(&mut self, read: &BootDir) -> Result<()> {
        self.require_unchanged(read)?;
        let Some(staged) = read.staged else {
            return Err(Error::Foreign("it holds no staged pair"));
        };
        let mut sector = read.sector.clone();
        delete_entry(&mut sector, staged.kernel.index)?;
        delete_entry(&mut sector, staged.initrd.index)?;
        self.write_sector(&sector)?;
        self.free_chains(&[staged.kernel.first_cluster, staged.initrd.first_cluster])?;
        self.finish()
    }

    /// `read` must be exactly the directory this value admitted, every field
    /// of it, and the device must still hold its sector.
    fn require_unchanged(&self, read: &BootDir) -> Result<()> {
        if *read != self.boot {
            return Err(Error::Changed);
        }
        let mut now = vec![0u8; self.boot.sector.len()];
        self.dev.read_at(self.boot.offset, &mut now)?;
        if now != read.sector {
            return Err(Error::Changed);
        }
        Ok(())
    }

    /// The steps staging and the direct form share, each made durable before
    /// the next: the space check, which writes nothing; an earlier staged
    /// pair's deletion; the reclaim; the new files' data and both FATs'
    /// chains. Returns each new file's first cluster and size.
    fn prepare(
        &mut self,
        kernel: Input<'_>,
        initrd: Input<'_>,
    ) -> Result<((u32, u32), (u32, u32))> {
        let ((klen, kc), (ilen, ic)) = self.clusters_needed(kernel.len, initrd.len)?;
        if let Some(staged) = self.boot.staged {
            let mut sector = self.boot.sector.clone();
            delete_entry(&mut sector, staged.kernel.index)?;
            delete_entry(&mut sector, staged.initrd.index)?;
            self.write_sector(&sector)?;
            self.boot.staged = None;
            for r in &mut self.reach {
                if *r == 2 {
                    *r = 0;
                }
            }
        }
        self.reclaim()?;
        let mut free = (2..=self.geo.last_cluster()).filter(|&c| self.is_free(c));
        let kchain: Vec<u32> = free.by_ref().take(to_usize(kc)?).collect();
        let ichain: Vec<u32> = free.take(to_usize(ic)?).collect();
        if kchain.len() != to_usize(kc)? || ichain.len() != to_usize(ic)? {
            return Err(Error::Damaged(
                "free clusters disappeared during the update",
            ));
        }
        self.write_data(&kchain, u64::from(klen), kernel.reader)?;
        self.write_data(&ichain, u64::from(ilen), initrd.reader)?;
        // The chains' flush makes the data durable with them, before the
        // directory sector can name either.
        let mut dirty = Vec::new();
        for chain in [&kchain, &ichain] {
            for (n, &c) in chain.iter().enumerate() {
                let next = chain.get(n + 1).copied().unwrap_or(EOC);
                self.set_entry(c, next, &mut dirty)?;
            }
        }
        self.flush_fats(&dirty)?;
        let first = |chain: &[u32]| chain.first().copied().unwrap_or(0);
        Ok(((first(&kchain), klen), (first(&ichain), ilen)))
    }

    /// Free every allocated cluster no entry reaches, in both FATs.
    fn reclaim(&mut self) -> Result<()> {
        let mut dirty = Vec::new();
        for c in 2..=self.geo.last_cluster() {
            if self.reach_of(c) == 0 && !self.is_free(c) && !self.is_bad(c) {
                self.set_entry(c, 0, &mut dirty)?;
            }
        }
        self.flush_fats(&dirty)
    }

    fn free_chains(&mut self, firsts: &[u32]) -> Result<()> {
        let mut dirty = Vec::new();
        for &first in firsts {
            for c in self.chain(first)? {
                self.set_entry(c, 0, &mut dirty)?;
            }
        }
        self.flush_fats(&dirty)
    }

    /// Bring FSInfo's free count and hint up to date, then admit the result
    /// afresh, so the value describes the ESP as it now is.
    fn finish(&mut self) -> Result<()> {
        let free = self.free_clusters();
        let hint = (2..=self.geo.last_cluster())
            .find(|&c| self.is_free(c))
            .unwrap_or(FSINFO_UNKNOWN);
        for sector in [FSINFO_SECTOR, BACKUP_BOOT_SECTOR + 1] {
            let at = u64::from(sector)
                .checked_mul(u64::from(self.geo.bps))
                .ok_or(Error::Volume("FSInfo offset overflows"))?;
            let mut bytes = vec![0u8; to_usize(self.geo.bps)?];
            self.dev.read_at(self.at(at)?, &mut bytes)?;
            put(&mut bytes, 488, &free.to_le_bytes())?;
            put(&mut bytes, 492, &hint.to_le_bytes())?;
            self.dev.write_at(self.at(at)?, &bytes)?;
        }
        self.dev.sync()?;
        self.fats = read_fats(&self.dev, self.base, &self.geo)?;
        self.walk()
    }

    fn write_sector(&mut self, sector: &[u8]) -> Result<()> {
        self.dev.write_at(self.boot.offset, sector)?;
        self.dev.sync()?;
        self.boot.sector = sector.to_vec();
        Ok(())
    }

    /// Write `len` bytes from `reader` over `chain`, padding the last cluster
    /// with zeros, then require the reader's end.
    fn write_data(&mut self, chain: &[u32], len: u64, reader: &mut dyn Read) -> Result<()> {
        let mut left = len;
        let mut buf = Vec::new();
        for (offset, run) in self.runs(chain)? {
            let size = to_usize_u64(run)?;
            buf.clear();
            buf.resize(size, 0);
            let take = to_usize_u64(run.min(left))?;
            let target = buf
                .get_mut(..take)
                .ok_or(Error::Input("a run exceeds memory"))?;
            read_full(reader, target)?;
            left = left.saturating_sub(run);
            self.dev.write_at(self.at(offset)?, &buf)?;
        }
        let mut probe = [0u8; 1];
        loop {
            match reader.read(&mut probe) {
                Ok(0) => return Ok(()),
                Ok(_) => return Err(Error::Input("it is longer than declared")),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(Error::Io(e)),
            }
        }
    }

    /// Contiguous runs of `chain` as (volume offset, bytes), each at most
    /// one MiB.
    fn runs(&self, chain: &[u32]) -> Result<Vec<(u64, u64)>> {
        let cb = u64::from(self.geo.cluster_bytes());
        let mut runs: Vec<(u64, u64)> = Vec::new();
        for &c in chain {
            let offset = self
                .geo
                .cluster_offset(c)
                .ok_or(Error::Damaged("a cluster lies outside the volume"))?;
            match runs.last_mut() {
                Some((start, len))
                    if start.checked_add(*len) == Some(offset)
                        && len.saturating_add(cb) <= CHUNK as u64 =>
                {
                    *len += cb;
                }
                _ => runs.push((offset, cb)),
            }
        }
        Ok(runs)
    }

    fn at(&self, offset: u64) -> Result<u64> {
        self.base
            .checked_add(offset)
            .ok_or(Error::Volume("an offset overflows"))
    }

    fn count(&self, pred: impl Fn(&Self, u32) -> bool) -> u32 {
        let mut n = 0u32;
        for c in 2..=self.geo.last_cluster() {
            if pred(self, c) {
                n = n.saturating_add(1);
            }
        }
        n
    }

    fn reach_of(&self, c: u32) -> u8 {
        self.reach.get(c as usize).copied().unwrap_or(0)
    }

    /// Cluster `c`'s entry in FAT `which`, its reserved high bits masked; a
    /// cluster outside the FAT reads as bad.
    fn entry(&self, which: usize, c: u32) -> u32 {
        let at = (c as usize).saturating_mul(4);
        self.fats
            .get(which)
            .and_then(|fat| fat.get(at..at.saturating_add(4)))
            .and_then(|b| b.try_into().ok())
            .map_or(BAD, u32::from_le_bytes)
            & MASK
    }

    fn is_free(&self, c: u32) -> bool {
        self.entry(0, c) == 0 && self.entry(1, c) == 0
    }

    fn is_bad(&self, c: u32) -> bool {
        self.entry(0, c) == BAD || self.entry(1, c) == BAD
    }

    /// Set cluster `c`'s entry in both FATs, keeping each entry's reserved
    /// high four bits, and note the FAT sector it lies in.
    fn set_entry(&mut self, c: u32, value: u32, dirty: &mut Vec<usize>) -> Result<()> {
        let at = to_usize(c)?
            .checked_mul(4)
            .ok_or(Error::Damaged("a FAT offset overflows"))?;
        for fat in &mut self.fats {
            let slot = fat
                .get_mut(at..at.saturating_add(4))
                .ok_or(Error::Damaged("a cluster lies outside the FAT"))?;
            let old = u32::from_le_bytes(four(slot)?);
            slot.copy_from_slice(&((old & !MASK) | (value & MASK)).to_le_bytes());
        }
        let sector = at / to_usize(self.geo.bps)?;
        if !dirty.contains(&sector) {
            dirty.push(sector);
        }
        Ok(())
    }

    /// Write the dirty FAT sectors to both FATs, then sync.
    fn flush_fats(&mut self, dirty: &[usize]) -> Result<()> {
        let bps = to_usize(self.geo.bps)?;
        for which in 0..2u32 {
            let base = self
                .geo
                .fat_offset(which)
                .ok_or(Error::Volume("a FAT offset overflows"))?;
            for &sector in dirty {
                let start = sector.saturating_mul(bps);
                let bytes = self
                    .fats
                    .get(which as usize)
                    .and_then(|fat| fat.get(start..start.saturating_add(bps)))
                    .ok_or(Error::Damaged("a FAT sector lies outside the FAT"))?
                    .to_vec();
                let offset = base
                    .checked_add(start as u64)
                    .ok_or(Error::Volume("a FAT offset overflows"))?;
                self.dev.write_at(self.at(offset)?, &bytes)?;
            }
        }
        self.dev.sync()?;
        Ok(())
    }

    /// Follow a chain from `first` in the first FAT, requiring the second to
    /// agree on every entry. An empty file's first cluster is 0.
    fn chain(&self, first: u32) -> Result<Vec<u32>> {
        let mut chain = Vec::new();
        if first == 0 {
            return Ok(chain);
        }
        let mut c = first;
        loop {
            if c < 2 || c > self.geo.last_cluster() {
                return Err(Error::Damaged("a chain leaves the volume"));
            }
            if chain.len() >= self.geo.clusters as usize {
                return Err(Error::Damaged("a chain loops"));
            }
            let next = self.entry(0, c);
            if self.entry(1, c) != next {
                return Err(Error::Damaged(
                    "the two FATs disagree on a reachable cluster",
                ));
            }
            chain.push(c);
            if next >= EOC_MIN {
                return Ok(chain);
            }
            if next == BAD || next < 2 {
                return Err(Error::Damaged("a chain reaches a bad or free cluster"));
            }
            c = next;
        }
    }

    /// Walk every directory from the root, marking each cluster an entry
    /// reaches and admitting `EFI/BOOT`.
    fn walk(&mut self) -> Result<()> {
        let mut walk = Walk {
            reach: vec![0u8; to_usize(self.geo.last_cluster())? + 1],
            entries: 0,
        };
        let root = self.chain(self.geo.root)?;
        walk.claim(&root, 1)?;
        let mut boot = None;
        let mut efi = None;
        let root_entries = self.dir_entries(&root, &mut walk)?;
        for e in &root_entries {
            if e.name == EFI_DIR && e.kind()? == Kind::Directory {
                if efi.is_some() {
                    return Err(Error::Foreign("two EFI directories"));
                }
                efi = Some(e.first_cluster);
            }
        }
        self.descend(&root_entries, &mut walk, 1)?;
        let efi = efi.ok_or(Error::Foreign("no EFI directory"))?;
        let efi_entries = self.dir_entries(&self.chain(efi)?, &mut Walk::scratch())?;
        for e in &efi_entries {
            if e.name == BOOT_DIR && e.kind()? == Kind::Directory {
                if boot.is_some() {
                    return Err(Error::Foreign("two BOOT directories"));
                }
                boot = Some(e.first_cluster);
            }
        }
        let boot = boot.ok_or(Error::Foreign("no BOOT directory"))?;
        self.reach = walk.reach;
        self.admit_boot(efi, boot)
    }

    /// Walk the subdirectories among `entries`, then mark each file's chain.
    fn descend(&self, entries: &[DirEntry], walk: &mut Walk, depth: usize) -> Result<()> {
        for e in entries {
            let kind = e.kind()?;
            if kind == Kind::Label || e.name == DOT || e.name == DOTDOT {
                continue;
            }
            let chain = self.chain(e.first_cluster)?;
            if kind == Kind::Directory {
                if depth > MAX_DEPTH {
                    return Err(Error::Volume("directories nest too deep"));
                }
                if chain.is_empty() {
                    return Err(Error::Damaged("a directory has no cluster"));
                }
                walk.claim(&chain, 1)?;
                let children = self.dir_entries(&chain, walk)?;
                self.descend(&children, walk, depth + 1)?;
            } else {
                let want = self
                    .geo
                    .clusters_for(u64::from(e.size))
                    .ok_or(Error::Damaged("a file's size overflows"))?;
                if chain.len() != to_usize(want)? {
                    return Err(Error::Damaged("a file's chain does not match its size"));
                }
                walk.claim(&chain, 1)?;
            }
        }
        Ok(())
    }

    /// The live entries of a directory, long-name and deleted ones skipped,
    /// up to its first end marker.
    fn dir_entries(&self, chain: &[u32], walk: &mut Walk) -> Result<Vec<DirEntry>> {
        let mut out = Vec::new();
        let mut buf = vec![0u8; to_usize(self.geo.cluster_bytes())?];
        for &c in chain {
            let offset = self.geo.cluster_offset(c).ok_or(Error::Damaged(
                "a directory cluster lies outside the volume",
            ))?;
            self.dev.read_at(self.at(offset)?, &mut buf)?;
            for raw in buf.as_chunks::<ENTRY>().0 {
                walk.entries += 1;
                if walk.entries > MAX_ENTRIES {
                    return Err(Error::Volume("too many directory entries"));
                }
                match raw.first().copied() {
                    Some(END) => return Ok(out),
                    Some(DELETED) => continue,
                    _ => {}
                }
                let e = DirEntry::parse(raw)?;
                if e.attr & ATTR_LONG_NAME_MASK == ATTR_LONG_NAME {
                    continue;
                }
                out.push(e);
            }
        }
        Ok(out)
    }

    /// `EFI/BOOT` as td's layout writes it: `.` and `..` first, then the
    /// pair and at most one staged pair, each once, as regular files in the
    /// directory's first sector, and every other entry of the directory
    /// free, an end marker followed only by end markers.
    fn admit_boot(&mut self, efi: u32, boot: u32) -> Result<()> {
        let chain = self.chain(boot)?;
        let first = *chain.first().ok_or(Error::Damaged("BOOT has no cluster"))?;
        let cluster_offset = self
            .geo
            .cluster_offset(first)
            .ok_or(Error::Damaged("BOOT lies outside the volume"))?;
        let bps = to_usize(self.geo.bps)?;
        let mut sector = vec![0u8; bps];
        let offset = self.at(cluster_offset)?;
        self.dev.read_at(offset, &mut sector)?;
        let mut ended = false;
        let mut found: [Option<FileEntry>; 4] = [None; 4];
        for (index, raw) in sector.as_chunks::<ENTRY>().0.iter().enumerate() {
            let lead = raw.first().copied().unwrap_or(END);
            if ended {
                if lead != END {
                    return Err(Error::Foreign("an entry follows its end marker"));
                }
                continue;
            }
            if lead == END {
                ended = true;
                continue;
            }
            if index < 2 {
                let e = DirEntry::parse(raw)?;
                let (name, cluster) = if index == 0 {
                    (DOT, first)
                } else {
                    (DOTDOT, efi)
                };
                if e.name != name || e.attr & ATTR_DIRECTORY == 0 || e.first_cluster != cluster {
                    return Err(Error::Foreign("its . and .. are not td's"));
                }
                continue;
            }
            if lead == DELETED {
                continue;
            }
            if index >= ATOMIC_ENTRIES {
                return Err(Error::Foreign("a boot file lies past its first 512 bytes"));
            }
            let e = DirEntry::parse(raw)?;
            let slot = [KERNEL, INITRD, STAGED_KERNEL, STAGED_INITRD]
                .iter()
                .position(|n| *n == e.name)
                .ok_or(Error::Foreign("it holds a file td does not write"))?;
            if e.attr & !ATTR_ARCHIVE != 0 {
                return Err(Error::Foreign("a boot file is not a plain file"));
            }
            if e.size == 0 {
                return Err(Error::Foreign("a boot file is empty"));
            }
            let place = found
                .get_mut(slot)
                .ok_or(Error::Foreign("it holds a file td does not write"))?;
            if place.is_some() {
                return Err(Error::Foreign("a boot file appears twice"));
            }
            *place = Some(FileEntry {
                index,
                first_cluster: e.first_cluster,
                size: e.size,
            });
        }
        // The rest of the directory must hold nothing live.
        let mut rest = vec![0u8; to_usize(self.geo.cluster_bytes())?];
        for (n, &c) in chain.iter().enumerate() {
            let offset = self
                .geo
                .cluster_offset(c)
                .ok_or(Error::Damaged("BOOT lies outside the volume"))?;
            self.dev.read_at(self.at(offset)?, &mut rest)?;
            let skip = if n == 0 { bps } else { 0 };
            for raw in rest.get(skip..).unwrap_or_default().as_chunks::<ENTRY>().0 {
                let lead = raw.first().copied().unwrap_or(END);
                if ended {
                    if lead != END {
                        return Err(Error::Foreign("an entry follows its end marker"));
                    }
                } else if lead == END {
                    ended = true;
                } else if lead != DELETED {
                    return Err(Error::Foreign("an entry lies past its first sector"));
                }
            }
        }
        let [Some(kernel), Some(initrd), staged_kernel, staged_initrd] = found else {
            return Err(Error::Foreign("it lacks BOOTX64.EFI or INITRD"));
        };
        let staged = match (staged_kernel, staged_initrd) {
            (Some(kernel), Some(initrd)) => Some(Pair { kernel, initrd }),
            (None, None) => None,
            _ => return Err(Error::Foreign("it holds half a staged pair")),
        };
        // The walk must have reached BOOT and every boot file, so nothing
        // this directory names can be taken for unreachable and reclaimed.
        let mut named = vec![boot];
        named.extend(
            [Some(kernel), Some(initrd), staged_kernel, staged_initrd]
                .into_iter()
                .flatten()
                .map(|f| f.first_cluster),
        );
        for first in named {
            for c in self.chain(first)? {
                if self.reach_of(c) != 1 {
                    return Err(Error::Damaged("the walk did not reach EFI/BOOT"));
                }
            }
        }
        if let Some(pair) = staged {
            for f in [pair.kernel, pair.initrd] {
                for c in self.chain(f.first_cluster)? {
                    if let Some(r) = self.reach.get_mut(c as usize) {
                        *r = 2;
                    }
                }
            }
        }
        self.boot = BootDir {
            offset,
            sector,
            pair: Pair { kernel, initrd },
            staged,
        };
        Ok(())
    }
}

#[derive(Clone, Copy, Debug)]
struct DirEntry {
    name: [u8; 11],
    attr: u8,
    first_cluster: u32,
    size: u32,
}

/// What a live short-name entry is. One answer serves both the walk and
/// the search for `EFI` and `BOOT`, so a name it finds is always walked.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Kind {
    Label,
    Directory,
    File,
}

impl DirEntry {
    /// A volume label is the volume bit alone, perhaps with the archive bit,
    /// and owns no cluster; any other entry with the volume bit is damage.
    fn kind(&self) -> Result<Kind> {
        if self.attr & ATTR_VOLUME != 0 {
            if self.attr & !(ATTR_VOLUME | ATTR_ARCHIVE) != 0
                || self.first_cluster != 0
                || self.size != 0
            {
                return Err(Error::Damaged("an entry is a volume label and more"));
            }
            return Ok(Kind::Label);
        }
        if self.attr & ATTR_DIRECTORY != 0 {
            return Ok(Kind::Directory);
        }
        Ok(Kind::File)
    }

    fn parse(raw: &[u8]) -> Result<Self> {
        let field = |range: std::ops::Range<usize>| {
            raw.get(range)
                .ok_or(Error::Damaged("a directory entry is short"))
        };
        let mut name = [0u8; 11];
        name.copy_from_slice(field(0..11)?);
        let attr = *field(11..12)?
            .first()
            .ok_or(Error::Damaged("no attribute"))?;
        let hi = u16::from_le_bytes(two(field(20..22)?)?);
        let lo = u16::from_le_bytes(two(field(26..28)?)?);
        let size = u32::from_le_bytes(four(field(28..32)?)?);
        Ok(Self {
            name,
            attr,
            first_cluster: (u32::from(hi) << 16) | u32::from(lo),
            size,
        })
    }
}

struct Walk {
    reach: Vec<u8>,
    entries: usize,
}

impl Walk {
    /// A walk that marks nothing, for re-reading a directory already walked.
    fn scratch() -> Self {
        Self {
            reach: Vec::new(),
            entries: 0,
        }
    }

    fn claim(&mut self, chain: &[u32], mark: u8) -> Result<()> {
        for &c in chain {
            let slot = self
                .reach
                .get_mut(c as usize)
                .ok_or(Error::Damaged("a chain leaves the volume"))?;
            if *slot != 0 {
                return Err(Error::Damaged("two entries share a cluster"));
            }
            *slot = mark;
        }
        Ok(())
    }
}

fn read_geometry<D: Device>(dev: &D, base: u64, len: u64) -> Result<Geometry> {
    let mut head = [0u8; 512];
    dev.read_at(base, &mut head)?;
    let u16_at = |at: usize| -> Result<u32> {
        Ok(u32::from(u16::from_le_bytes(two(head
            .get(at..at + 2)
            .ok_or(Error::Volume("short boot sector"))?)?)))
    };
    let u32_at = |at: usize| -> Result<u32> {
        Ok(u32::from_le_bytes(four(
            head.get(at..at + 4)
                .ok_or(Error::Volume("short boot sector"))?,
        )?))
    };
    let byte = |at: usize| -> u32 { u32::from(head.get(at).copied().unwrap_or(0)) };
    if !matches!(head.first(), Some(0xeb) | Some(0xe9)) {
        return Err(Error::Volume("no jump at the boot sector's start"));
    }
    if head.get(510..512) != Some(&[0x55, 0xaa][..]) {
        return Err(Error::Volume("no boot sector signature"));
    }
    let bps = u16_at(11)?;
    if !matches!(bps, 512 | 1024 | 2048 | 4096) {
        return Err(Error::Volume("unsupported bytes per sector"));
    }
    let spc = byte(13);
    if spc == 0 || !spc.is_power_of_two() || bps.saturating_mul(spc) > MAX_CLUSTER_BYTES {
        return Err(Error::Volume("unsupported sectors per cluster"));
    }
    let reserved = u16_at(14)?;
    if byte(16) != 2 {
        return Err(Error::Volume("not two FATs"));
    }
    if u16_at(17)? != 0 || u16_at(19)? != 0 || u16_at(22)? != 0 {
        return Err(Error::Volume("FAT12 or FAT16 fields are set"));
    }
    let total = u32_at(32)?;
    let fat_sectors = u32_at(36)?;
    if u16_at(40)? != 0 {
        return Err(Error::Volume("the FATs are not both live"));
    }
    if u16_at(42)? != 0 {
        return Err(Error::Volume("an unknown FAT32 version"));
    }
    let root = u32_at(44)?;
    if u16_at(48)? != FSINFO_SECTOR || u16_at(50)? != BACKUP_BOOT_SECTOR {
        return Err(Error::Volume(
            "FSInfo or the backup boot sector is not where td puts it",
        ));
    }
    if reserved <= BACKUP_BOOT_SECTOR + 1 {
        return Err(Error::Volume("too few reserved sectors"));
    }
    if head.get(82..90) != Some(&b"FAT32   "[..]) {
        return Err(Error::Volume("not labelled FAT32"));
    }
    let bytes = u64::from(total)
        .checked_mul(u64::from(bps))
        .ok_or(Error::Volume("the volume size overflows"))?;
    if total == 0 || bytes > len {
        return Err(Error::Volume("the volume is larger than its partition"));
    }
    let data = total
        .checked_sub(reserved)
        .and_then(|s| s.checked_sub(fat_sectors.checked_mul(2)?))
        .ok_or(Error::Volume("the metadata exceeds the volume"))?;
    let clusters = data / spc;
    if clusters < MIN_CLUSTERS {
        return Err(Error::Volume("too few clusters for FAT32"));
    }
    if clusters > MAX_CLUSTERS {
        return Err(Error::Volume("more clusters than td-fat holds"));
    }
    if u64::from(fat_sectors) * u64::from(bps) / 4 < u64::from(clusters) + 2 {
        return Err(Error::Volume("the FAT cannot address every cluster"));
    }
    if root < 2 || root > clusters + 1 {
        return Err(Error::Volume("the root cluster lies outside the volume"));
    }
    let geo = Geometry {
        bps,
        spc,
        reserved,
        fat_sectors,
        clusters,
        root,
    };
    let mut sector = vec![0u8; to_usize(bps)?];
    dev.read_at(base, &mut sector)?;
    if sector.get(510..512) != Some(&[0x55, 0xaa][..]) {
        return Err(Error::Volume("no boot sector signature"));
    }
    let fsinfo = u64::from(FSINFO_SECTOR) * u64::from(bps);
    dev.read_at(
        base.checked_add(fsinfo)
            .ok_or(Error::Volume("an offset overflows"))?,
        &mut sector,
    )?;
    if sector.get(0..4) != Some(&b"RRaA"[..])
        || sector.get(484..488) != Some(&b"rrAa"[..])
        || sector.get(508..512) != Some(&[0, 0, 0x55, 0xaa][..])
    {
        return Err(Error::Volume("FSInfo's signatures are wrong"));
    }
    Ok(geo)
}

fn read_fats<D: Device>(dev: &D, base: u64, geo: &Geometry) -> Result<[Vec<u8>; 2]> {
    let entries = u64::from(geo.clusters) + 2;
    let bps = u64::from(geo.bps);
    let len = to_usize_u64((entries * 4).div_ceil(bps) * bps)?;
    let mut fats = [vec![0u8; len], vec![0u8; len]];
    for (which, fat) in fats.iter_mut().enumerate() {
        let offset = geo
            .fat_offset(which as u32)
            .and_then(|o| o.checked_add(base))
            .ok_or(Error::Volume("a FAT offset overflows"))?;
        dev.read_at(offset, fat)?;
    }
    Ok(fats)
}

/// A new regular file's entry, as td's layout writes one: the archive
/// attribute, no creation or access stamp and FAT's epoch as its write date.
fn put_entry(
    sector: &mut [u8],
    index: usize,
    name: &[u8; 11],
    (cluster, size): (u32, u32),
) -> Result<()> {
    let mut e = [0u8; ENTRY];
    put(&mut e, 0, name)?;
    put(&mut e, 11, &[ATTR_ARCHIVE])?;
    put(&mut e, 24, &FAT_EPOCH_DATE.to_le_bytes())?;
    let at = index.saturating_mul(ENTRY);
    sector
        .get_mut(at..at.saturating_add(ENTRY))
        .ok_or(Error::Foreign("an entry lies outside its sector"))?
        .copy_from_slice(&e);
    set_target(sector, index, (cluster, size))
}

/// Point the entry at `index` at a first cluster and a size.
fn set_target(sector: &mut [u8], index: usize, (cluster, size): (u32, u32)) -> Result<()> {
    let at = index.saturating_mul(ENTRY);
    let e = sector
        .get_mut(at..at.saturating_add(ENTRY))
        .ok_or(Error::Foreign("an entry lies outside its sector"))?;
    put(e, 20, &((cluster >> 16) as u16).to_le_bytes())?;
    put(e, 26, &((cluster & 0xffff) as u16).to_le_bytes())?;
    put(e, 28, &size.to_le_bytes())
}

fn delete_entry(sector: &mut [u8], index: usize) -> Result<()> {
    let slot = sector
        .get_mut(index.saturating_mul(ENTRY))
        .ok_or(Error::Foreign("an entry lies outside its sector"))?;
    *slot = DELETED;
    Ok(())
}

fn put(buf: &mut [u8], at: usize, bytes: &[u8]) -> Result<()> {
    buf.get_mut(at..at.saturating_add(bytes.len()))
        .ok_or(Error::Damaged("a field lies outside its buffer"))?
        .copy_from_slice(bytes);
    Ok(())
}

fn read_full(reader: &mut dyn Read, mut buf: &mut [u8]) -> Result<()> {
    while !buf.is_empty() {
        match reader.read(buf) {
            Ok(0) => return Err(Error::Input("it is shorter than declared")),
            Ok(n) => buf = buf.get_mut(n..).unwrap_or_default(),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(Error::Io(e)),
        }
    }
    Ok(())
}

fn file_len(len: u64) -> Result<u32> {
    if len == 0 {
        return Err(Error::Input("it is empty"));
    }
    u32::try_from(len).map_err(|_| Error::Input("it exceeds FAT32's 4 GiB file limit"))
}

fn two(b: &[u8]) -> Result<[u8; 2]> {
    b.try_into().map_err(|_| Error::Damaged("a field is short"))
}

fn four(b: &[u8]) -> Result<[u8; 4]> {
    b.try_into().map_err(|_| Error::Damaged("a field is short"))
}

fn to_usize(n: u32) -> Result<usize> {
    usize::try_from(n).map_err(|_| Error::Volume("a size exceeds memory"))
}

fn to_usize_u64(n: u64) -> Result<usize> {
    usize::try_from(n).map_err(|_| Error::Volume("a size exceeds memory"))
}

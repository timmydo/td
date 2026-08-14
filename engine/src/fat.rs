//! fat.rs — td-native, zero-dep, DETERMINISTIC FAT32 writer, for the one
//! filesystem UEFI firmware is required to be able to read.
//!
//! The GPT in `gpt.rs` gives the firmware a partition to look at; this gives it
//! something to read there. UEFI 2.10 §13.3 requires firmware to support FAT32
//! on an EFI System Partition, which is why the ESP is the only filesystem in
//! td that is not td's choice — it is the one the machine already knows.
//!
//! Like `gpt.rs` this performs NO I/O and holds no whole image: `build` returns
//! the EXTENTS to write and where, with file contents BORROWED from the caller
//! rather than copied into a buffer. A FAT volume is mostly zeros, so writing
//! the extents onto a zeroed (or freshly created sparse) region is the whole
//! job, and an installer never materializes a 512 MiB image to lay down a
//! 40 MiB kernel.
//!
//! Layout — plain FAT32 as `fatgen103` describes it:
//!
//! * 32 reserved sectors: boot sector with the BPB at sector 0, FSInfo at 1,
//!   and the spec's backup pair at 6 and 7. The backups are not optional in
//!   practice — firmware and `fsck` both fall back to them.
//! * Two FATs, sized so they can address every data cluster. That count is
//!   circular (the FATs come out of the same sectors the clusters do), so it is
//!   solved by iterating to a fixed point rather than by a closed form.
//! * A data region of equal-size clusters numbered from 2, cluster 2 being the
//!   root directory. Every file and directory here is allocated CONTIGUOUSLY in
//!   one pre-order pass, so a cluster chain is always a run and a file occupies
//!   one byte range — which is what lets the caller stream contents to a single
//!   offset.
//!
//! FAT32 means at least 65525 clusters. That is not a stylistic threshold: a
//! volume below it is FAT16 BY DEFINITION however the BPB is labelled, and a
//! reader that counts clusters rather than trusting the label will read it as
//! one. So the cluster size is chosen to keep the count above the line, and a
//! volume too small for FAT32 at any cluster size is REFUSED rather than
//! labelled FAT32 and left for the firmware to disagree about.
//!
//! Names are 8.3 and are refused otherwise. td controls what goes on its own
//! ESP, and the paths that matter — `\EFI\BOOT\BOOTX64.EFI`, the removable-media
//! path every firmware boots without an NVRAM entry — are 8.3-clean already.
//! Long filenames are a second directory-entry format checksummed against the
//! short one; adding it to write names nothing here needs would be surface for
//! nothing. A name that does not fit is an error naming the name, never a
//! silent truncation to `BOOTX6~1.EFI`.
//!
//! Determinism: no clock is read. FAT's stamps are local-time-in-a-struct,
//! which is not a thing a reproducible image can carry, so the optional
//! creation and access ones stay zero — the spec's "unsupported" — and the
//! mandatory WRITE stamp carries FAT's own epoch, since zero is not a legal
//! date there at all (it encodes month 0, day 0). The volume serial is supplied
//! by the caller and allocation follows the caller's declared order. One
//! `Volume` is one byte sequence.

use std::borrow::Cow;

/// FAT32's own floor. Below this a volume IS FAT16, whatever the BPB says.
pub const MIN_FAT32_CLUSTERS: u32 = 65525;
/// FAT32's ceiling: 28 significant bits, minus the reserved high values.
pub const MAX_FAT32_CLUSTERS: u32 = 268_435_445;
/// Sectors before the first FAT. 32 is the conventional FAT32 value and leaves
/// room for the backup boot sector at 6.
pub const RESERVED_SECTORS: u32 = 32;
/// Two, so a damaged FAT has a spare — what every FAT32 formatter writes.
pub const NUM_FATS: u32 = 2;
/// Sector holding the backup boot sector, and (at +1) the backup FSInfo.
pub const BACKUP_BOOT_SECTOR: u32 = 6;
/// The root directory is always cluster 2 here; FAT32 allows any cluster, and
/// nothing gains from putting it elsewhere.
pub const ROOT_CLUSTER: u32 = 2;
/// End-of-chain marker written into the FAT.
pub const EOC: u32 = 0x0fff_ffff;
/// A directory entry is 32 bytes.
pub const DIR_ENTRY_SIZE: u32 = 32;
/// Largest cluster a FAT volume may use.
pub const MAX_CLUSTER_BYTES: u32 = 32 * 1024;
/// FSInfo's "no answer" for the free count and the next-free hint. Any other
/// value must be a real cluster number, so a full volume says this rather than
/// naming the cluster one past the end.
pub const FSINFO_UNKNOWN: u32 = 0xffff_ffff;
/// 1980-01-01 packed as a FAT date: year 0 from 1980, month 1, day 1. Zero is
/// NOT a legal value there — it encodes month 0 and day 0 — and the write
/// stamp, unlike the creation and access ones, is not optional.
pub const FAT_EPOCH_DATE: u16 = 0x0021;
/// `BS_VolLab` when the volume has no label, which the spec spells out rather
/// than leaving blank.
pub const NO_VOLUME_LABEL: &[u8; 11] = b"NO NAME    ";
/// Directory nesting this writer will take. Both walks recurse per level, and a
/// deep enough tree overflows the stack — which ABORTS, with no message and no
/// `Result`, in a crate whose whole discipline is that failures are returned.
/// Nothing on an ESP is remotely this deep; the cap exists so the failure is a
/// sentence instead of a signal.
pub const MAX_DEPTH: usize = 64;

const ATTR_VOLUME_ID: u8 = 0x08;
const ATTR_DIRECTORY: u8 = 0x10;
const ATTR_ARCHIVE: u8 = 0x20;

/// One node of the tree to lay down.
#[derive(Clone, Debug)]
pub enum Node<'a> {
    /// Contents already in hand. They are BORROWED — `build` never copies them
    /// — and come back as an extent to write.
    File(&'a [u8]),
    /// A file of known length whose bytes the caller will write ITSELF, at the
    /// offset the matching `Placement` gives. Nothing is read, copied or
    /// borrowed, so an installer can pipe a 40 MiB kernel through a small
    /// buffer instead of holding it. This is what makes the placement list
    /// more than a description of extents that already exist.
    Stream(u64),
    /// `(name, node)` pairs, written in the order given.
    Dir(Vec<(String, Node<'a>)>),
}

/// The volume to format.
#[derive(Clone, Debug)]
pub struct Volume<'a> {
    pub bytes_per_sector: u32,
    /// Sectors in the PARTITION this volume fills.
    pub total_sectors: u64,
    /// LBA the partition starts at, for the BPB's `HiddSec`.
    pub hidden_sectors: u32,
    /// The volume serial. Caller-supplied so an image is reproducible.
    pub volume_id: u32,
    /// At most 11 characters, space-padded on disk.
    pub label: String,
    /// `None` picks the largest cluster that keeps the count above
    /// `MIN_FAT32_CLUSTERS`.
    pub sectors_per_cluster: Option<u32>,
    pub root: Vec<(String, Node<'a>)>,
}

/// A range of bytes to write at `offset` from the START of the volume.
#[derive(Clone, Debug)]
pub struct Extent<'a> {
    pub offset: u64,
    pub bytes: Cow<'a, [u8]>,
}

/// The formatted volume, as the writes that produce it over zeroed space.
///
/// ZEROED SPACE IS A PRECONDITION, not a tidiness preference — see `build`.
#[derive(Clone, Debug)]
pub struct Image<'a> {
    pub total_bytes: u64,
    pub bytes_per_sector: u32,
    pub sectors_per_cluster: u32,
    pub sectors_per_fat: u32,
    pub cluster_count: u32,
    pub extents: Vec<Extent<'a>>,
    /// Where each file's contents landed, from the SAME walk that produced the
    /// extents — a caller that streams rather than buffers writes these offsets
    /// instead of the file extents, and the two cannot disagree.
    pub placements: Vec<Placement>,
}

impl Image<'_> {
    /// Materialize the whole volume. Tests and small volumes only — an
    /// installer writes the extents onto a sparse file or a device instead.
    pub fn to_vec(&self) -> Result<Vec<u8>, String> {
        let len = usize::try_from(self.total_bytes)
            .map_err(|_| "fat: volume is larger than this machine can address".to_string())?;
        let mut out = vec![0u8; len];
        for e in &self.extents {
            let at = usize::try_from(e.offset)
                .map_err(|_| "fat: extent offset exceeds usize".to_string())?;
            let end = at
                .checked_add(e.bytes.len())
                .ok_or_else(|| "fat: extent overflows".to_string())?;
            out.get_mut(at..end)
                .ok_or_else(|| format!("fat: extent {at}..{end} runs past the volume"))?
                .copy_from_slice(&e.bytes);
        }
        Ok(out)
    }
}

/// A file's contents and where they belong, so a caller can stream them.
///
/// An EMPTY file owns no clusters — FAT gives it first-cluster 0, which is its
/// spelling of "no chain" — so it appears here with `offset` and `len` both 0
/// and nothing to write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Placement {
    /// Path from the volume root, `\`-separated and upper-cased as stored.
    pub path: String,
    pub offset: u64,
    pub len: u64,
}

/// Format `volume`, returning the writes that produce it.
///
/// THE EXTENTS MUST LAND ON ZEROED SPACE. This is a precondition of the result,
/// not advice about tidiness, and it cannot be checked here because nothing here
/// reads the destination. What is emitted is only what must be non-zero: the FAT
/// is written as its LIVE PREFIX (a few dozen bytes of a table that may be half a
/// megabyte), reserved sectors 2..=5 and 8.. are not written at all, and a file's
/// last cluster keeps whatever follows its final byte. Over a device with stale
/// contents, the bytes past the FAT prefix read as ALLOCATED clusters — lost
/// chains, a free count that disagrees with the table, and a later write handing
/// out clusters that already hold something. A freshly created sparse file, or a
/// region explicitly zeroed first, satisfies it.
pub fn build<'a>(volume: &Volume<'a>) -> Result<Image<'a>, String> {
    let bps = volume.bytes_per_sector;
    if !(512..=4096).contains(&bps) || !bps.is_power_of_two() {
        return Err(format!(
            "fat: {bps} bytes per sector is not a power of two in 512..=4096"
        ));
    }
    if volume.label.chars().count() > 11 {
        return Err(format!(
            "fat: volume label {:?} is longer than the 11-byte field",
            volume.label
        ));
    }
    let geometry = solve_geometry(volume)?;
    let cluster_bytes = u64::from(bps)
        .checked_mul(u64::from(geometry.sectors_per_cluster))
        .ok_or_else(|| "fat: cluster size overflows".to_string())?;

    // A BPB label with no matching root entry is an inconsistency `fsck`
    // "repairs" by deleting the label, so the two are written together or not
    // at all.
    let label = if volume.label.trim().is_empty() {
        None
    } else {
        Some(label_field(&volume.label)?)
    };

    // Pre-order allocation from cluster 2, so every chain is a contiguous run.
    let mut planner = Planner {
        cluster_bytes,
        cluster_count: geometry.cluster_count,
        next_free: ROOT_CLUSTER,
    };
    let root = planner.plan_dir("", &volume.root, true, u64::from(label.is_some()), 0)?;
    let next_free = planner.next_free;
    let used = next_free
        .checked_sub(ROOT_CLUSTER)
        .ok_or_else(|| "fat: cluster accounting underflowed".to_string())?;

    let data_start = u64::from(RESERVED_SECTORS)
        .checked_add(u64::from(NUM_FATS) * u64::from(geometry.sectors_per_fat))
        .and_then(|s| s.checked_mul(u64::from(bps)))
        .ok_or_else(|| "fat: data region offset overflows".to_string())?;
    let mut extents = Vec::new();
    let free_clusters = geometry.cluster_count.saturating_sub(used);
    extents.push(Extent {
        offset: 0,
        bytes: Cow::Owned(boot_sector(volume, &geometry, label.as_ref())?),
    });
    extents.push(Extent {
        offset: u64::from(bps),
        bytes: Cow::Owned(fsinfo_sector(bps, free_clusters, next_free, geometry.cluster_count)?),
    });
    // The backup pair. Firmware and fsck both fall back to these, so a volume
    // whose primary is damaged and whose backup was never written is a volume
    // with no second chance.
    extents.push(Extent {
        offset: u64::from(BACKUP_BOOT_SECTOR)
            .checked_mul(u64::from(bps))
            .ok_or_else(|| "fat: backup boot sector offset overflows".to_string())?,
        bytes: Cow::Owned(boot_sector(volume, &geometry, label.as_ref())?),
    });
    extents.push(Extent {
        offset: u64::from(BACKUP_BOOT_SECTOR + 1)
            .checked_mul(u64::from(bps))
            .ok_or_else(|| "fat: backup FSInfo offset overflows".to_string())?,
        bytes: Cow::Owned(fsinfo_sector(bps, free_clusters, next_free, geometry.cluster_count)?),
    });

    // One FAT image, written twice. Only the live prefix is emitted: the rest
    // of the table is free clusters, which are zeros already.
    let fat = build_fat(&root, next_free)?;
    for i in 0..NUM_FATS {
        let at = u64::from(RESERVED_SECTORS)
            .checked_add(u64::from(i) * u64::from(geometry.sectors_per_fat))
            .and_then(|s| s.checked_mul(u64::from(bps)))
            .ok_or_else(|| "fat: FAT offset overflows".to_string())?;
        extents.push(Extent {
            offset: at,
            bytes: Cow::Owned(fat.clone()),
        });
    }

    let mut walk = Emit {
        cluster_bytes,
        data_start,
        extents,
        placements: Vec::new(),
    };
    walk.dir(&root, 0, "", label.as_ref())?;
    let Emit {
        extents,
        placements,
        ..
    } = walk;

    let total_bytes = volume
        .total_sectors
        .checked_mul(u64::from(bps))
        .ok_or_else(|| "fat: volume size overflows".to_string())?;
    for e in &extents {
        let end = e
            .offset
            .checked_add(u64::try_from(e.bytes.len()).unwrap_or(u64::MAX))
            .ok_or_else(|| "fat: extent end overflows".to_string())?;
        if end > total_bytes {
            return Err(format!(
                "fat: an extent ends at {end}, past the {total_bytes}-byte volume"
            ));
        }
    }
    Ok(Image {
        total_bytes,
        bytes_per_sector: bps,
        sectors_per_cluster: geometry.sectors_per_cluster,
        sectors_per_fat: geometry.sectors_per_fat,
        cluster_count: geometry.cluster_count,
        extents,
        placements,
    })
}

struct Geometry {
    sectors_per_cluster: u32,
    sectors_per_fat: u32,
    cluster_count: u32,
}

/// The FAT sizing is circular — the FATs come out of the sectors they address —
/// so iterate to a fixed point instead of inverting it.
fn solve_geometry(volume: &Volume<'_>) -> Result<Geometry, String> {
    let bps = volume.bytes_per_sector;
    let total = volume.total_sectors;
    let candidates: Vec<u32> = match volume.sectors_per_cluster {
        Some(spc) => {
            if spc == 0 || !spc.is_power_of_two() {
                return Err(format!("fat: {spc} sectors per cluster is not a power of two"));
            }
            if spc.saturating_mul(bps) > MAX_CLUSTER_BYTES {
                return Err(format!(
                    "fat: {} byte clusters exceed FAT's {MAX_CLUSTER_BYTES} byte maximum",
                    spc.saturating_mul(bps)
                ));
            }
            vec![spc]
        }
        // Largest first: fewer clusters means a smaller FAT, and the first that
        // still clears the FAT32 floor is the one to take.
        None => (0..=6)
            .rev()
            .map(|shift| 1u32 << shift)
            .filter(|spc| spc.saturating_mul(bps) <= MAX_CLUSTER_BYTES)
            .collect(),
    };

    // Seeded rather than empty: if no candidate cluster size is even eligible,
    // the caller must get a sentence rather than an empty error.
    let mut last_err = format!(
        "fat: no cluster size in 1..=64 sectors fits a {bps}-byte sector under FAT's \
         {MAX_CLUSTER_BYTES} byte cluster maximum"
    );
    for spc in candidates {
        match geometry_for(bps, total, spc) {
            Ok(g) => return Ok(g),
            Err(e) => last_err = e,
        }
    }
    Err(last_err)
}

fn geometry_for(bps: u32, total_sectors: u64, spc: u32) -> Result<Geometry, String> {
    let per_sector = u64::from(bps / 4); // FAT32 entries are 4 bytes
    let mut sectors_per_fat: u64 = 1;
    // Each round recomputes the data region from the current FAT size and the
    // FAT size the resulting cluster count needs. The fixed point is "the FAT is
    // BIG ENOUGH", not "exactly the size the count needs": growing the FAT
    // shrinks the data region, which shrinks the count, which shrinks the need,
    // so demanding equality oscillates — a 64 MiB volume alternates between 1008
    // and 1009 sectors forever. A FAT with a few unused entries at the end is
    // what every real formatter produces and what the BPB is able to describe.
    for _ in 0..64 {
        let overhead = u64::from(RESERVED_SECTORS)
            .checked_add(u64::from(NUM_FATS).saturating_mul(sectors_per_fat))
            .ok_or_else(|| "fat: metadata overhead overflows".to_string())?;
        let data_sectors = total_sectors.checked_sub(overhead).ok_or_else(|| {
            format!("fat: a {total_sectors}-sector volume is too small to hold FAT32 metadata")
        })?;
        let clusters = data_sectors / u64::from(spc);
        // Entries 0 and 1 are reserved, so the table addresses clusters + 2.
        let needed = clusters
            .checked_add(2)
            .map(|n| n.div_ceil(per_sector.max(1)))
            .ok_or_else(|| "fat: FAT size overflows".to_string())?
            .max(1);
        if needed <= sectors_per_fat {
            let clusters = u32::try_from(clusters)
                .map_err(|_| "fat: cluster count exceeds 32 bits".to_string())?;
            if clusters < MIN_FAT32_CLUSTERS {
                return Err(format!(
                    "fat: {clusters} clusters of {} bytes is below FAT32's {MIN_FAT32_CLUSTERS} \
                     cluster floor — a volume this small IS FAT16 however it is labelled",
                    u64::from(spc) * u64::from(bps)
                ));
            }
            if clusters > MAX_FAT32_CLUSTERS {
                return Err(format!(
                    "fat: {clusters} clusters exceeds FAT32's {MAX_FAT32_CLUSTERS} maximum"
                ));
            }
            return Ok(Geometry {
                sectors_per_cluster: spc,
                sectors_per_fat: u32::try_from(sectors_per_fat)
                    .map_err(|_| "fat: FAT is larger than 32 bits of sectors".to_string())?,
                cluster_count: clusters,
            });
        }
        sectors_per_fat = needed;
    }
    Err("fat: FAT sizing did not settle".into())
}

struct Plan<'a> {
    /// The 11-byte on-disk name. Spaces for the root, which has no entry.
    name: [u8; 11],
    attr: u8,
    first_cluster: u32,
    clusters: u32,
    size: u32,
    /// `Some` for a directory, `None` for a file.
    children: Option<Vec<Plan<'a>>>,
    /// Whether this is the volume root, which has no `.`/`..` and may carry the
    /// volume-label entry. The flag `plan_dir` sized the clusters with, so
    /// `emit` cannot disagree with it.
    is_root: bool,
    /// A file's contents, borrowed. `None` for a directory.
    data: Option<&'a [u8]>,
}

/// The pre-order allocator: the geometry a layout is computed against and the
/// running cursor it hands clusters out from, in one place rather than threaded
/// through every level of the walk.
struct Planner {
    cluster_bytes: u64,
    cluster_count: u32,
    next_free: u32,
}

impl Planner {
    /// Hand out `want` CONSECUTIVE clusters, which is what makes every chain a
    /// run and every file one byte range.
    fn take(&mut self, want: u32) -> Result<u32, String> {
        let first = self.next_free;
        let end = first
            .checked_add(want)
            .ok_or_else(|| "fat: cluster allocation overflows".to_string())?;
        // Clusters are numbered from 2, so the last valid number is count + 1.
        if end > self.cluster_count.saturating_add(ROOT_CLUSTER) {
            return Err(format!(
                "fat: the tree needs more than the volume's {} clusters",
                self.cluster_count
            ));
        }
        self.next_free = end;
        Ok(first)
    }

    fn plan_dir<'a>(
        &mut self,
        path: &str,
        entries: &[(String, Node<'a>)],
        is_root: bool,
        extra_entries: u64,
        depth: usize,
    ) -> Result<Plan<'a>, String> {
    let cluster_bytes = self.cluster_bytes;
    if depth > MAX_DEPTH {
        return Err(format!(
            "fat: {path} nests deeper than the {MAX_DEPTH} directory levels this writer takes"
        ));
    }
    // A non-root directory carries `.` and `..` before its children; the root
    // may carry a volume-label entry instead.
    let dots = if is_root { 0u64 } else { 2 };
    let count = u64::try_from(entries.len())
        .map_err(|_| "fat: absurd directory size".to_string())?
        .checked_add(dots)
        .and_then(|n| n.checked_add(extra_entries))
        .ok_or_else(|| "fat: directory entry count overflows".to_string())?;
    let bytes = count
        .checked_mul(u64::from(DIR_ENTRY_SIZE))
        .ok_or_else(|| "fat: directory size overflows".to_string())?;
    // A directory always occupies at least one cluster, even when empty.
    let clusters = u32::try_from(bytes.div_ceil(cluster_bytes).max(1))
        .map_err(|_| "fat: directory needs more clusters than FAT32 can address".to_string())?;
    let first = self.take(clusters)?;

    let mut seen: Vec<[u8; 11]> = Vec::with_capacity(entries.len());
    let mut children = Vec::with_capacity(entries.len());
    for (name, node) in entries {
        let short = short_name(name)?;
        if seen.contains(&short) {
            return Err(format!(
                "fat: {path}\\{name} collides with an earlier entry — FAT names are \
                 case-insensitive, so two that differ only in case are one name"
            ));
        }
        seen.push(short);
        let child_path = format!("{path}\\{}", display_name(&short));
        children.push(match node {
            Node::Dir(sub) => {
                let mut plan = self.plan_dir(&child_path, sub, false, 0, depth + 1)?;
                plan.name = short;
                plan
            }
            // A file, with or without its bytes in hand — the length is all the
            // layout depends on, which is what lets `Stream` exist.
            Node::File(_) | Node::Stream(_) => {
                let (len, data) = match node {
                    Node::File(bytes) => (
                        u64::try_from(bytes.len())
                            .map_err(|_| "fat: file length overflows".to_string())?,
                        Some(*bytes),
                    ),
                    Node::Stream(len) => (*len, None),
                    Node::Dir(_) => (0, None),
                };
                let size = u32::try_from(len).map_err(|_| {
                    format!("fat: {child_path} is larger than FAT32's 4 GiB file limit")
                })?;
                let clusters = u32::try_from(len.div_ceil(cluster_bytes)).map_err(|_| {
                    "fat: file needs more clusters than FAT32 can address".to_string()
                })?;
                // An empty file owns no clusters and points at cluster 0, which
                // is FAT's spelling of "no chain".
                let first = if clusters == 0 { 0 } else { self.take(clusters)? };
                Plan {
                    name: short,
                    attr: ATTR_ARCHIVE,
                    first_cluster: first,
                    clusters,
                    size,
                    children: None,
                    is_root: false,
                    data,
                }
            }
        });
    }
    Ok(Plan {
        name: [b' '; 11],
        attr: ATTR_DIRECTORY,
        first_cluster: first,
        clusters,
        size: 0,
        children: Some(children),
        is_root,
        data: None,
    })
    }
}

/// The FAT itself: entry 0 is the media descriptor, entry 1 is reserved, and
/// every allocated run is a chain ending in `EOC`.
fn build_fat(root: &Plan, next_free: u32) -> Result<Vec<u8>, String> {
    let entries = usize::try_from(next_free)
        .map_err(|_| "fat: FAT prefix exceeds usize".to_string())?;
    let mut table = vec![0u8; entries.saturating_mul(4)];
    put32(&mut table, 0, 0x0fff_fff8)?;
    put32(&mut table, 4, EOC)?;
    chain_into(root, &mut table)?;
    Ok(table)
}

fn chain_into(plan: &Plan, table: &mut [u8]) -> Result<(), String> {
    if plan.clusters > 0 && plan.first_cluster >= ROOT_CLUSTER {
        for i in 0..plan.clusters {
            let cluster = plan
                .first_cluster
                .checked_add(i)
                .ok_or_else(|| "fat: chain overflows".to_string())?;
            let last = i
                .checked_add(1)
                .is_some_and(|next| next == plan.clusters);
            let value = if last {
                EOC
            } else {
                cluster
                    .checked_add(1)
                    .ok_or_else(|| "fat: chain successor overflows".to_string())?
            };
            let at = usize::try_from(cluster)
                .map_err(|_| "fat: cluster exceeds usize".to_string())?
                .checked_mul(4)
                .ok_or_else(|| "fat: FAT offset overflows".to_string())?;
            put32(table, at, value)?;
        }
    }
    for child in plan.children.iter().flatten() {
        chain_into(child, table)?;
    }
    Ok(())
}

/// The one walk that produces both the extents and the placements — the
/// geometry it needs and the two things it fills, so a caller cannot thread
/// them out of step with each other.
struct Emit<'a> {
    cluster_bytes: u64,
    /// Byte offset of cluster 2 from the start of the volume.
    data_start: u64,
    extents: Vec<Extent<'a>>,
    placements: Vec<Placement>,
}

impl<'a> Emit<'a> {
    fn cluster_offset(&self, cluster: u32) -> Result<u64, String> {
        u64::from(cluster.saturating_sub(ROOT_CLUSTER))
            .checked_mul(self.cluster_bytes)
            .and_then(|b| b.checked_add(self.data_start))
            .ok_or_else(|| "fat: cluster offset overflows".to_string())
    }

    fn dir(
        &mut self,
        plan: &Plan<'a>,
        parent_cluster: u32,
        prefix: &str,
        label: Option<&[u8; 11]>,
    ) -> Result<(), String> {
        let cluster_bytes = self.cluster_bytes;
        let children = match &plan.children {
            Some(c) => c,
            None => return Ok(()),
        };
        let mut dir = Vec::new();
        // The volume label is a root entry with no clusters and no size — the
        // counterpart of the BPB's copy.
        if let Some(label) = label {
            dir.extend_from_slice(&dir_entry(label, ATTR_VOLUME_ID, 0, 0)?);
        }
        // `.` and `..` come first in every directory but the root. `..` naming
        // the root is written as cluster 0, which is what FAT means by "the
        // root". `is_root` is the SAME flag `plan_dir` counted the entries with:
        // two expressions for one predicate is how a directory ends up sized for
        // a different entry list than the one written into it.
        if !plan.is_root {
            dir.extend_from_slice(&dir_entry(
                b".          ",
                ATTR_DIRECTORY,
                plan.first_cluster,
                0,
            )?);
            let up = if parent_cluster == ROOT_CLUSTER {
                0
            } else {
                parent_cluster
            };
            dir.extend_from_slice(&dir_entry(b"..         ", ATTR_DIRECTORY, up, 0)?);
        }
        for child in children {
            dir.extend_from_slice(&dir_entry(
                &child.name,
                child.attr,
                child.first_cluster,
                child.size,
            )?);
        }
        let span = usize::try_from(
            u64::from(plan.clusters)
                .checked_mul(cluster_bytes)
                .ok_or_else(|| "fat: directory span overflows".to_string())?,
        )
        .map_err(|_| "fat: directory span exceeds usize".to_string())?;
        // `resize` SHRINKS as readily as it grows, and shrinking here would drop
        // directory entries silently. The count that sized these clusters is
        // computed in `plan_dir` and the entries are written here, so the two
        // sides of that invariant are separate expressions — say so rather than
        // trust them.
        if dir.len() > span {
            return Err(format!(
                "fat: a directory's {} bytes of entries exceed the {span} bytes planned for it",
                dir.len()
            ));
        }
        // The rest of a directory's clusters must be zero: a zero first name
        // byte is what tells a reader the entries have ended.
        dir.resize(span, 0);
        let at = self.cluster_offset(plan.first_cluster)?;
        self.extents.push(Extent {
            offset: at,
            bytes: Cow::Owned(dir),
        });
        for child in children {
            let path = format!("{prefix}\\{}", display_name(&child.name));
            // Directory-ness, not the presence of bytes: a `Stream` file has no
            // bytes either, and is still a file.
            if child.children.is_some() {
                self.dir(child, plan.first_cluster, &path, None)?;
                continue;
            }
            let offset = if child.first_cluster == 0 {
                0
            } else {
                self.cluster_offset(child.first_cluster)?
            };
            // Contents in hand are BORROWED into the extent rather than copied.
            // A `Stream` file contributes no extent at all — only the placement
            // telling the caller where to put it.
            if let Some(bytes) = child.data {
                if !bytes.is_empty() {
                    self.extents.push(Extent {
                        offset,
                        bytes: Cow::Borrowed(bytes),
                    });
                }
            }
            self.placements.push(Placement {
                path,
                offset,
                len: u64::from(child.size),
            });
        }
        Ok(())
    }
}

fn dir_entry(name: &[u8], attr: u8, cluster: u32, size: u32) -> Result<[u8; 32], String> {
    let mut e = [0u8; 32];
    let name = name
        .get(..11)
        .ok_or_else(|| "fat: a directory entry name is not 11 bytes".to_string())?;
    put(&mut e, 0, name)?;
    put(&mut e, 11, &[attr])?;
    // 13..20 are the creation and access stamps, which the spec makes optional
    // and zero means "unsupported". The WRITE stamp is not optional, and zero
    // is not a legal date, so it carries FAT's own epoch — a fixed value, so
    // one tree is still one image.
    put(&mut e, 24, &FAT_EPOCH_DATE.to_le_bytes())?;
    put(&mut e, 20, &u16::try_from(cluster >> 16).unwrap_or(0).to_le_bytes())?;
    put(
        &mut e,
        26,
        &u16::try_from(cluster & 0xffff).unwrap_or(0).to_le_bytes(),
    )?;
    put(&mut e, 28, &size.to_le_bytes())?;
    Ok(e)
}

fn boot_sector(
    volume: &Volume<'_>,
    g: &Geometry,
    label: Option<&[u8; 11]>,
) -> Result<Vec<u8>, String> {
    let bps = volume.bytes_per_sector;
    let mut s = vec![
        0u8;
        usize::try_from(bps).map_err(|_| "fat: sector exceeds usize".to_string())?
    ];
    // A jump nothing executes: firmware reads the BPB, but a volume whose first
    // bytes are not a jump is rejected by readers that check.
    put(&mut s, 0, &[0xeb, 0x58, 0x90])?;
    put(&mut s, 3, b"MSWIN4.1")?;
    // These four narrow into their BPB fields. A substituted default would be a
    // well-formed BPB describing a DIFFERENT filesystem than the one the rest of
    // this function laid out, built with no error — so each is an error instead.
    put(
        &mut s,
        11,
        &u16::try_from(bps)
            .map_err(|_| format!("fat: {bps} bytes per sector does not fit BytsPerSec"))?
            .to_le_bytes(),
    )?;
    put(
        &mut s,
        13,
        &[u8::try_from(g.sectors_per_cluster).map_err(|_| {
            format!(
                "fat: {} sectors per cluster does not fit SecPerClus",
                g.sectors_per_cluster
            )
        })?],
    )?;
    put(
        &mut s,
        14,
        &u16::try_from(RESERVED_SECTORS)
            .map_err(|_| "fat: RESERVED_SECTORS does not fit RsvdSecCnt".to_string())?
            .to_le_bytes(),
    )?;
    put(
        &mut s,
        16,
        &[u8::try_from(NUM_FATS).map_err(|_| "fat: NUM_FATS does not fit NumFATs".to_string())?],
    )?;
    // RootEntCnt and TotSec16 are zero on FAT32; the 32-bit fields carry them.
    put(&mut s, 17, &0u16.to_le_bytes())?;
    put(&mut s, 19, &0u16.to_le_bytes())?;
    put(&mut s, 21, &[0xf8])?; // fixed disk
    put(&mut s, 22, &0u16.to_le_bytes())?; // FATSz16
    put(&mut s, 24, &32u16.to_le_bytes())?; // SecPerTrk, geometry nothing reads
    put(&mut s, 26, &64u16.to_le_bytes())?; // NumHeads, likewise
    put(&mut s, 28, &volume.hidden_sectors.to_le_bytes())?;
    put(
        &mut s,
        32,
        &u32::try_from(volume.total_sectors)
            .map_err(|_| "fat: a FAT32 volume cannot exceed 2^32 sectors".to_string())?
            .to_le_bytes(),
    )?;
    put(&mut s, 36, &g.sectors_per_fat.to_le_bytes())?;
    put(&mut s, 40, &0u16.to_le_bytes())?; // ExtFlags: both FATs live
    put(&mut s, 42, &0u16.to_le_bytes())?; // FSVer 0.0
    put(&mut s, 44, &ROOT_CLUSTER.to_le_bytes())?;
    put(&mut s, 48, &1u16.to_le_bytes())?; // FSInfo sector
    put(
        &mut s,
        50,
        &u16::try_from(BACKUP_BOOT_SECTOR)
            .map_err(|_| "fat: BACKUP_BOOT_SECTOR does not fit BkBootSec".to_string())?
            .to_le_bytes(),
    )?;
    put(&mut s, 64, &[0x80])?; // DrvNum
    put(&mut s, 66, &[0x29])?; // BootSig: the three fields below are present
    put(&mut s, 67, &volume.volume_id.to_le_bytes())?;
    // With no label the spec names the field's contents rather than leaving it
    // blank, and there is no root entry to match.
    put(&mut s, 71, label.unwrap_or(NO_VOLUME_LABEL))?;
    put(&mut s, 82, b"FAT32   ")?;
    put(&mut s, 510, &[0x55, 0xaa])?;
    Ok(s)
}

fn fsinfo_sector(
    bps: u32,
    free_clusters: u32,
    next_free: u32,
    cluster_count: u32,
) -> Result<Vec<u8>, String> {
    let mut s = vec![
        0u8;
        usize::try_from(bps).map_err(|_| "fat: sector exceeds usize".to_string())?
    ];
    // On a FULL volume the running allocator stands one past the last cluster,
    // which is not a cluster number; the hint has to be the "unknown" sentinel
    // rather than a value a reader would try to use.
    let hint = if next_free <= cluster_count.saturating_add(ROOT_CLUSTER).saturating_sub(1) {
        next_free
    } else {
        FSINFO_UNKNOWN
    };
    put(&mut s, 0, b"RRaA")?;
    put(&mut s, 484, b"rrAa")?;
    put(&mut s, 488, &free_clusters.to_le_bytes())?;
    put(&mut s, 492, &hint.to_le_bytes())?;
    put(&mut s, 508, &[0x00, 0x00, 0x55, 0xaa])?;
    Ok(s)
}

/// The 11-byte label field, upper-cased and space-padded. One function for both
/// places a label is written — the BPB and the root's volume-label entry —
/// because a volume whose two labels disagree is one `fsck` "repairs" by
/// deleting the BPB's.
fn label_field(label: &str) -> Result<[u8; 11], String> {
    let mut out = [b' '; 11];
    for (slot, c) in out.iter_mut().zip(label.chars()) {
        let c = c.to_ascii_uppercase();
        if !short_name_char(c) && c != ' ' {
            return Err(format!(
                "fat: volume label {label:?} contains {c:?}, which FAT cannot store"
            ));
        }
        *slot = u8::try_from(u32::from(c))
            .map_err(|_| format!("fat: volume label {label:?} is not ASCII"))?;
    }
    Ok(out)
}

/// FAT's own allowed set for a short name, minus the space — a leading or
/// embedded space is legal and is a name nothing can type.
fn short_name_char(c: char) -> bool {
    c.is_ascii_uppercase()
        || c.is_ascii_digit()
        || "!#$%&'()-@^_`{}~".contains(c)
}

/// Encode `name` as the 11-byte 8.3 field, or say why it cannot be.
fn short_name(name: &str) -> Result<[u8; 11], String> {
    if name.is_empty() || name == "." || name == ".." {
        return Err(format!("fat: {name:?} is not a usable name"));
    }
    // A trailing dot is DOS's spelling of "no extension", so `BOOT.` and `BOOT`
    // are one stored name. Accepting it would hand the caller back a name it
    // did not ask for, which the 8.3 refusals exist to prevent.
    if name.ends_with('.') {
        return Err(format!(
            "fat: {name:?} ends in a dot, which FAT stores as {:?} — the same name without it",
            name.trim_end_matches('.')
        ));
    }
    let upper = name.to_ascii_uppercase();
    let (base, ext) = match upper.rsplit_once('.') {
        Some((b, e)) if !b.is_empty() => (b.to_string(), e.to_string()),
        // A leading dot is part of the base, not an extension — and `.config`
        // is not an 8.3 name, which the length check below reports.
        _ => (upper.clone(), String::new()),
    };
    if base.len() > 8 || ext.len() > 3 {
        return Err(format!(
            "fat: {name:?} is not an 8.3 name ({} + {} characters); this writer refuses \
             rather than truncating it to something that boots a different file",
            base.len(),
            ext.len()
        ));
    }
    for c in base.chars().chain(ext.chars()) {
        if !short_name_char(c) {
            return Err(format!(
                "fat: {name:?} contains {c:?}, which a FAT short name cannot hold"
            ));
        }
    }
    let mut out = [b' '; 11];
    for (slot, c) in out.iter_mut().zip(base.bytes()) {
        *slot = c;
    }
    for (slot, c) in out.iter_mut().skip(8).zip(ext.bytes()) {
        *slot = c;
    }
    Ok(out)
}

/// The 11-byte field back as `BASE.EXT`, for error messages and placements.
fn display_name(raw: &[u8; 11]) -> String {
    let text = |bytes: &[u8]| -> String {
        String::from_utf8_lossy(bytes).trim_end().to_string()
    };
    let base = text(raw.get(..8).unwrap_or_default());
    let ext = text(raw.get(8..).unwrap_or_default());
    if ext.is_empty() {
        base
    } else {
        format!("{base}.{ext}")
    }
}

fn put(buf: &mut [u8], at: usize, bytes: &[u8]) -> Result<(), String> {
    let end = at
        .checked_add(bytes.len())
        .ok_or_else(|| "fat: field offset overflows".to_string())?;
    let have = buf.len();
    buf.get_mut(at..end)
        .ok_or_else(|| format!("fat: field {at}..{end} lies outside a {have}-byte buffer"))?
        .copy_from_slice(bytes);
    Ok(())
}

fn put32(buf: &mut [u8], at: usize, value: u32) -> Result<(), String> {
    put(buf, at, &value.to_le_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOOTX64: &[u8] = b"MZ\x90\x00this stands in for an EFI application";

    /// 64 MiB, the smallest round size that can be FAT32 at all: the 65525
    /// cluster floor puts the real minimum at 66599 sectors (32.52 MiB, pinned
    /// by `the_smallest_fat32_volume_is_where_the_cluster_floor_puts_it`), so a
    /// 16 MiB ESP — the
    /// size a naive installer would pick — cannot be FAT32 at any cluster size.
    fn esp<'a>(root: Vec<(String, Node<'a>)>) -> Volume<'a> {
        Volume {
            bytes_per_sector: 512,
            total_sectors: 131_072,
            hidden_sectors: 2048,
            volume_id: 0x7d1e_9a30,
            label: "TD-ESP".into(),
            sectors_per_cluster: None,
            root,
        }
    }

    /// `\EFI\BOOT\BOOTX64.EFI` — the removable-media path firmware boots with no
    /// NVRAM entry, and the reason 8.3 names are enough.
    fn fallback_tree<'a>() -> Vec<(String, Node<'a>)> {
        vec![(
            "EFI".into(),
            Node::Dir(vec![(
                "BOOT".into(),
                Node::Dir(vec![("BOOTX64.EFI".into(), Node::File(BOOTX64))]),
            )]),
        )]
    }

    fn u16_at(b: &[u8], at: usize) -> u16 {
        u16::from_le_bytes(b.get(at..at + 2).unwrap().try_into().unwrap())
    }

    fn u32_at(b: &[u8], at: usize) -> u32 {
        u32::from_le_bytes(b.get(at..at + 4).unwrap().try_into().unwrap())
    }

    fn data_start(img: &Image) -> usize {
        usize::try_from(
            (u64::from(RESERVED_SECTORS) + u64::from(NUM_FATS) * u64::from(img.sectors_per_fat))
                * u64::from(img.bytes_per_sector),
        )
        .unwrap()
    }

    fn cluster_at(img: &Image, cluster: u32) -> usize {
        data_start(img)
            + usize::try_from(
                u64::from(cluster - ROOT_CLUSTER)
                    * u64::from(img.sectors_per_cluster)
                    * u64::from(img.bytes_per_sector),
            )
            .unwrap()
    }

    /// Scan a directory's first cluster for `name`, returning (cluster, size).
    fn lookup(bytes: &[u8], dir_at: usize, name: &str) -> Option<(u32, u32)> {
        let want = short_name(name).unwrap();
        for i in 0..64 {
            let e = bytes.get(dir_at + i * 32..dir_at + i * 32 + 32)?;
            if e.first() == Some(&0) {
                return None; // a zero first byte ends the directory
            }
            if e.get(..11) == Some(want.as_slice()) {
                let cluster = (u32::from(u16_at(e, 20)) << 16) | u32::from(u16_at(e, 26));
                return Some((cluster, u32_at(e, 28)));
            }
        }
        None
    }

    fn fat_entry(bytes: &[u8], img: &Image, cluster: u32) -> u32 {
        let at = usize::try_from(u64::from(RESERVED_SECTORS) * u64::from(img.bytes_per_sector))
            .unwrap()
            + usize::try_from(cluster).unwrap() * 4;
        u32_at(bytes, at) & 0x0fff_ffff
    }

    #[test]
    fn a_64_mib_volume_is_fat32_and_clears_the_cluster_floor() {
        let img = build(&esp(fallback_tree())).unwrap();
        assert_eq!(img.bytes_per_sector, 512);
        // The largest cluster that still clears 65525 clusters on this volume.
        assert_eq!(img.sectors_per_cluster, 1);
        assert!(
            img.cluster_count >= MIN_FAT32_CLUSTERS,
            "{} clusters",
            img.cluster_count
        );
        assert_eq!(img.total_bytes, 64 * 1024 * 1024);
    }

    /// The exact boundary, because an installer sizes an ESP against it and
    /// "about 34 MiB" was wrong by a megabyte and a half when this was first
    /// written down.
    #[test]
    fn the_smallest_fat32_volume_is_where_the_cluster_floor_puts_it() {
        let fits = |sectors: u64| {
            let mut v = esp(vec![]);
            v.total_sectors = sectors;
            build(&v).is_ok()
        };
        assert!(fits(66_599), "66599 sectors is the smallest that works");
        assert!(!fits(66_598), "one sector less must be refused");
        // 32.52 MiB — so a 32 MiB ESP cannot be FAT32 and a 33 MiB one can.
        assert!(!fits(32 * 2048));
        assert!(fits(33 * 2048));
    }

    /// The failure an installer would otherwise ship: a 16 MiB ESP labelled
    /// FAT32 that every cluster-counting reader sees as FAT16.
    #[test]
    fn a_volume_too_small_for_fat32_is_refused_rather_than_mislabelled() {
        let mut v = esp(fallback_tree());
        v.total_sectors = 32_768; // 16 MiB
        let err = build(&v).unwrap_err();
        assert!(err.contains("cluster floor"), "{err}");
    }

    #[test]
    fn boot_sector_fields_sit_at_their_spec_offsets() {
        let img = build(&esp(fallback_tree())).unwrap();
        let bytes = img.to_vec().unwrap();
        assert_eq!(bytes.get(..3), Some([0xeb, 0x58, 0x90].as_slice()));
        assert_eq!(bytes.get(3..11), Some(b"MSWIN4.1".as_slice()));
        assert_eq!(u16_at(&bytes, 11), 512);
        assert_eq!(bytes.get(13).copied(), Some(1)); // SecPerClus
        assert_eq!(u16_at(&bytes, 14), 32); // RsvdSecCnt
        assert_eq!(bytes.get(16).copied(), Some(2)); // NumFATs
        assert_eq!(u16_at(&bytes, 17), 0, "RootEntCnt is 0 on FAT32");
        assert_eq!(u16_at(&bytes, 19), 0, "TotSec16 is 0 on FAT32");
        assert_eq!(bytes.get(21).copied(), Some(0xf8));
        assert_eq!(u16_at(&bytes, 22), 0, "FATSz16 is 0 on FAT32");
        assert_eq!(u32_at(&bytes, 28), 2048); // HiddSec
        assert_eq!(u32_at(&bytes, 32), 131_072); // TotSec32
        assert_eq!(u32_at(&bytes, 36), img.sectors_per_fat);
        assert_eq!(u32_at(&bytes, 44), 2); // RootClus
        assert_eq!(u16_at(&bytes, 48), 1); // FSInfo
        assert_eq!(u16_at(&bytes, 50), 6); // BkBootSec
        assert_eq!(bytes.get(66).copied(), Some(0x29)); // BootSig
        assert_eq!(u32_at(&bytes, 67), 0x7d1e_9a30);
        assert_eq!(bytes.get(71..82), Some(b"TD-ESP     ".as_slice()));
        assert_eq!(bytes.get(82..90), Some(b"FAT32   ".as_slice()));
        assert_eq!(bytes.get(510..512), Some([0x55, 0xaa].as_slice()));
    }

    /// Firmware and fsck both fall back to these; a volume whose backup was
    /// never written has no second chance.
    #[test]
    fn the_backup_boot_sector_and_fsinfo_are_written() {
        let img = build(&esp(fallback_tree())).unwrap();
        let bytes = img.to_vec().unwrap();
        assert_eq!(bytes.get(..512), bytes.get(6 * 512..7 * 512));
        assert_eq!(bytes.get(512..1024), bytes.get(7 * 512..8 * 512));
    }

    /// A BPB label with no matching root entry is an inconsistency `fsck`
    /// "repairs" by DELETING the label, so the two are written together.
    #[test]
    fn the_volume_label_is_written_in_both_places() {
        let img = build(&esp(fallback_tree())).unwrap();
        let bytes = img.to_vec().unwrap();
        assert_eq!(bytes.get(71..82), Some(b"TD-ESP     ".as_slice()));
        let root = cluster_at(&img, ROOT_CLUSTER);
        assert_eq!(
            bytes.get(root..root + 11),
            Some(b"TD-ESP     ".as_slice()),
            "the root's first entry must be the volume label"
        );
        assert_eq!(bytes.get(root + 11).copied(), Some(ATTR_VOLUME_ID));
        // It owns no clusters and has no size.
        assert_eq!(u16_at(&bytes, root + 20), 0);
        assert_eq!(u16_at(&bytes, root + 26), 0);
        assert_eq!(u32_at(&bytes, root + 28), 0);

        // With no label there is no root entry, and the BPB carries the spec's
        // sentinel rather than blanks — eleven spaces is a label `fsck.vfat`
        // calls invalid, and it REWRITES the boot sector to remove it.
        let mut v = esp(fallback_tree());
        v.label = String::new();
        let unlabelled = build(&v).unwrap();
        let plain = unlabelled.to_vec().unwrap();
        assert_eq!(plain.get(71..82), Some(NO_VOLUME_LABEL.as_slice()));
        // The backup boot sector carries it too, or a repair reads the other.
        assert_eq!(plain.get(6 * 512 + 71..6 * 512 + 82), plain.get(71..82));
        let root = cluster_at(&unlabelled, ROOT_CLUSTER);
        assert_eq!(
            plain.get(root + 11).copied(),
            Some(ATTR_DIRECTORY),
            "with no label the root starts with its first real entry"
        );
        // A whitespace-only label is the same case, not a label of spaces.
        let mut blank = esp(fallback_tree());
        blank.label = "   ".into();
        let blank = build(&blank).unwrap().to_vec().unwrap();
        assert_eq!(blank.get(71..82), Some(NO_VOLUME_LABEL.as_slice()));
    }

    #[test]
    fn fsinfo_carries_its_three_signatures() {
        let img = build(&esp(fallback_tree())).unwrap();
        let bytes = img.to_vec().unwrap();
        let fsinfo = bytes.get(512..1024).unwrap();
        assert_eq!(fsinfo.get(..4), Some(b"RRaA".as_slice()));
        assert_eq!(fsinfo.get(484..488), Some(b"rrAa".as_slice()));
        assert_eq!(
            fsinfo.get(508..512),
            Some([0x00, 0x00, 0x55, 0xaa].as_slice())
        );
        // Four clusters are in use: root, EFI, BOOT, and the one file.
        assert_eq!(u32_at(fsinfo, 488), img.cluster_count - 4);
        assert_eq!(u32_at(fsinfo, 492), 6);
    }

    #[test]
    fn the_fat_reserves_its_first_two_entries() {
        let img = build(&esp(fallback_tree())).unwrap();
        let bytes = img.to_vec().unwrap();
        let fat = usize::try_from(u64::from(RESERVED_SECTORS) * 512).unwrap();
        assert_eq!(u32_at(&bytes, fat), 0x0fff_fff8);
        assert_eq!(u32_at(&bytes, fat + 4), EOC);
        // Both copies are identical — the WHOLE table, not a prefix of it.
        let span = usize::try_from(img.sectors_per_fat).unwrap() * 512;
        let second = fat + span;
        assert_eq!(
            bytes.get(fat..fat + span),
            bytes.get(second..second + span),
            "the two FATs differ"
        );
    }

    /// Walk the tree the way firmware does: root, then each directory, then the
    /// file's chain — which is what proves the entries and the FAT agree.
    #[test]
    fn the_fallback_path_is_reachable_from_the_root() {
        let img = build(&esp(fallback_tree())).unwrap();
        let bytes = img.to_vec().unwrap();
        let (efi, _) = lookup(&bytes, cluster_at(&img, ROOT_CLUSTER), "EFI").unwrap();
        let (boot, _) = lookup(&bytes, cluster_at(&img, efi), "BOOT").unwrap();
        let (file, size) = lookup(&bytes, cluster_at(&img, boot), "BOOTX64.EFI").unwrap();
        assert_eq!(size as usize, BOOTX64.len());
        let at = cluster_at(&img, file);
        assert_eq!(bytes.get(at..at + BOOTX64.len()), Some(BOOTX64));
        // One cluster holds it, so its chain is a single EOC.
        assert_eq!(fat_entry(&bytes, &img, file), EOC);
    }

    /// `..` in a top-level directory names the root as cluster 0, which is
    /// FAT's spelling of it — writing 2 there is a classic bug that most
    /// readers tolerate and `fsck` reports.
    #[test]
    fn a_subdirectory_carries_dot_and_dotdot() {
        let img = build(&esp(fallback_tree())).unwrap();
        let bytes = img.to_vec().unwrap();
        let (efi, _) = lookup(&bytes, cluster_at(&img, ROOT_CLUSTER), "EFI").unwrap();
        let at = cluster_at(&img, efi);
        assert_eq!(bytes.get(at..at + 11), Some(b".          ".as_slice()));
        assert_eq!(bytes.get(at + 32..at + 43), Some(b"..         ".as_slice()));
        let dot_cluster = (u32::from(u16_at(&bytes, at + 20)) << 16) | u32::from(u16_at(&bytes, at + 26));
        assert_eq!(dot_cluster, efi, "`.` must name the directory itself");
        let up = (u32::from(u16_at(&bytes, at + 52)) << 16) | u32::from(u16_at(&bytes, at + 58));
        assert_eq!(up, 0, "`..` naming the root is written as cluster 0");
        // The nested BOOT directory's `..` names EFI by its real cluster.
        let (boot, _) = lookup(&bytes, at, "BOOT").unwrap();
        let bat = cluster_at(&img, boot);
        let up = (u32::from(u16_at(&bytes, bat + 52)) << 16) | u32::from(u16_at(&bytes, bat + 58));
        assert_eq!(up, efi);
    }

    #[test]
    fn file_contents_land_at_their_placement() {
        let big = vec![0xa5u8; 5000]; // spans clusters
        let v = esp(vec![
            ("BIG.BIN".into(), Node::File(&big)),
            ("EMPTY.BIN".into(), Node::File(b"")),
        ]);
        let img = build(&v).unwrap();
        let bytes = img.to_vec().unwrap();
        let placed: Vec<&Placement> = img.placements.iter().collect();
        assert_eq!(placed.len(), 2);
        let p = placed.first().unwrap();
        assert_eq!(p.path, "\\BIG.BIN");
        assert_eq!(p.len, 5000);
        let at = usize::try_from(p.offset).unwrap();
        assert_eq!(bytes.get(at..at + 5000), Some(big.as_slice()));
        // An empty file owns no clusters, so there is nothing to write.
        let e = placed.get(1).unwrap();
        assert_eq!((e.path.as_str(), e.offset, e.len), ("\\EMPTY.BIN", 0, 0));
        let (cluster, size) = lookup(&bytes, cluster_at(&img, ROOT_CLUSTER), "EMPTY.BIN").unwrap();
        assert_eq!((cluster, size), (0, 0));
    }

    /// A multi-cluster file is one contiguous run, so its chain is consecutive
    /// and ends once.
    #[test]
    fn a_multi_cluster_file_chains_consecutively() {
        let big = vec![7u8; 5000];
        let img = build(&esp(vec![("BIG.BIN".into(), Node::File(&big))])).unwrap();
        let bytes = img.to_vec().unwrap();
        let (first, _) = lookup(&bytes, cluster_at(&img, ROOT_CLUSTER), "BIG.BIN").unwrap();
        // 5000 bytes over 512-byte clusters is 10 of them.
        for i in 0..9 {
            assert_eq!(fat_entry(&bytes, &img, first + i), first + i + 1);
        }
        assert_eq!(fat_entry(&bytes, &img, first + 9), EOC);
    }

    /// A directory bigger than one cluster is a CHAIN like a file's, and the
    /// entries past the first cluster are the ones a wrong span would lose.
    #[test]
    fn a_directory_spanning_clusters_chains_and_keeps_every_entry() {
        // 512-byte clusters hold 16 entries; one label plus 40 files needs 3.
        let names: Vec<String> = (0..40).map(|i| format!("F{i:03}.BIN")).collect();
        let root: Vec<(String, Node)> = names
            .iter()
            .map(|n| (n.clone(), Node::File(b"x")))
            .collect();
        let img = build(&esp(root)).unwrap();
        let bytes = img.to_vec().unwrap();
        assert_eq!(img.placements.len(), 40);
        // Root is clusters 2..=4, chained.
        assert_eq!(fat_entry(&bytes, &img, 2), 3);
        assert_eq!(fat_entry(&bytes, &img, 3), 4);
        assert_eq!(fat_entry(&bytes, &img, 4), EOC);
        // Every name is findable, including those past the first cluster.
        let at = cluster_at(&img, ROOT_CLUSTER);
        for (i, name) in names.iter().enumerate() {
            let want = short_name(name).unwrap();
            let found = (0..48).any(|slot| {
                bytes.get(at + slot * 32..at + slot * 32 + 11) == Some(want.as_slice())
            });
            assert!(found, "entry {i} ({name}) is missing from the root");
        }
    }

    #[test]
    fn names_that_are_not_8_3_are_refused() {
        for bad in [
            "initramfs.cpio", // 9 characters of base
            "VMLINUZ.LINUX",  // 5 of extension
            "",
            ".",
            "..",
            "A B.C",     // a space is legal FAT and a name nothing can type
            "HELLO+.TXT", // '+' is not in FAT's short-name set
            ".CONFIG",   // a leading dot is base, not extension
        ] {
            let v = esp(vec![(bad.into(), Node::File(b"x"))]);
            assert!(build(&v).is_err(), "{bad:?} was accepted");
        }
    }

    /// FAT stores short names upper-cased, so two that differ only in case are
    /// one name — writing both would produce a directory with a duplicate.
    #[test]
    fn entries_differing_only_in_case_collide() {
        let v = esp(vec![
            ("BOOTX64.EFI".into(), Node::File(b"a")),
            ("bootx64.efi".into(), Node::File(b"b")),
        ]);
        let err = build(&v).unwrap_err();
        assert!(err.contains("collides"), "{err}");
    }

    /// Lower case is ACCEPTED and stored upper-cased — FAT is case-insensitive,
    /// so refusing it would refuse `\efi\boot\bootx64.efi`, the same path.
    #[test]
    fn lower_case_names_are_stored_upper_cased() {
        let v = esp(vec![(
            "efi".into(),
            Node::Dir(vec![("bootx64.efi".into(), Node::File(BOOTX64))]),
        )]);
        let img = build(&v).unwrap();
        let bytes = img.to_vec().unwrap();
        let (efi, _) = lookup(&bytes, cluster_at(&img, ROOT_CLUSTER), "EFI").unwrap();
        assert!(lookup(&bytes, cluster_at(&img, efi), "BOOTX64.EFI").is_some());
        assert_eq!(
            img.placements.first().map(|p| p.path.as_str()),
            Some("\\EFI\\BOOTX64.EFI")
        );
    }

    #[test]
    fn a_tree_larger_than_the_volume_is_refused() {
        let huge = vec![0u8; 70 * 1024 * 1024];
        let v = esp(vec![("HUGE.BIN".into(), Node::File(&huge))]);
        let err = build(&v).unwrap_err();
        assert!(err.contains("clusters"), "{err}");
    }

    /// Inside the volume AND disjoint: two extents that overlap would write one
    /// structure over another, and only the ordering of the walk keeps them
    /// apart.
    #[test]
    fn extents_lie_inside_the_volume_and_never_overlap() {
        let big = vec![3u8; 9000];
        let img = build(&esp(vec![
            ("EFI".into(), Node::Dir(fallback_tree())),
            ("BIG.BIN".into(), Node::File(&big)),
            ("STREAM.BIN".into(), Node::Stream(4096)),
        ]))
        .unwrap();
        let mut ranges: Vec<(u64, u64)> = img
            .extents
            .iter()
            .map(|e| (e.offset, e.offset + e.bytes.len() as u64))
            .collect();
        ranges.sort_unstable();
        let mut previous_end = 0u64;
        for (start, end) in ranges {
            assert!(end <= img.total_bytes, "extent {start}..{end} runs past the volume");
            assert!(
                start >= previous_end,
                "extent {start}..{end} overlaps the one ending at {previous_end}"
            );
            previous_end = end;
        }
    }

    /// A trailing dot is DOS's "no extension", so `BOOT.` and `BOOT` are one
    /// stored name — accepting it would hand back a name nobody asked for.
    #[test]
    fn a_trailing_dot_is_refused_rather_than_dropped() {
        for bad in ["BOOT.", "A.", "BOOTX64.EFI."] {
            let err = build(&esp(vec![(bad.into(), Node::File(b"x"))])).unwrap_err();
            assert!(err.contains("ends in a dot"), "{bad}: {err}");
        }
    }

    /// Both walks recurse per level, and the stack overflow a deep tree caused
    /// ABORTS — no message, no `Result` — in a crate whose rule is that failures
    /// are returned.
    #[test]
    fn a_tree_deeper_than_the_cap_is_refused_rather_than_overflowing() {
        let nest = |depth: usize| {
            let mut node = Node::File(b"x");
            for _ in 0..depth {
                node = Node::Dir(vec![("D".into(), node)]);
            }
            vec![("TOP".into(), node)]
        };
        assert!(build(&esp(nest(MAX_DEPTH - 2))).is_ok());
        let err = build(&esp(nest(MAX_DEPTH + 8))).unwrap_err();
        assert!(err.contains("nests deeper"), "{err}");
    }

    /// 512 is not the only sector size a disk has, and every field that scales
    /// with it is computed rather than assumed. The volume has to GROW with the
    /// sector, since the smallest cluster grows with it and the 65525 floor is
    /// counted in clusters: a 128 MiB volume of 2048-byte sectors cannot be
    /// FAT32 at all.
    #[test]
    fn other_sector_sizes_produce_a_consistent_volume() {
        for bps in [1024u32, 2048, 4096] {
            let mut v = esp(fallback_tree());
            v.bytes_per_sector = bps;
            v.total_sectors = u64::from(MIN_FAT32_CLUSTERS) + 8192;
            let img = build(&v).unwrap();
            assert!(
                img.cluster_count >= MIN_FAT32_CLUSTERS,
                "{bps}: {} clusters",
                img.cluster_count
            );
            // Read the BPB out of its extent rather than materializing a
            // volume that reaches 286 MiB at the largest sector.
            let boot = &img.extents.first().unwrap().bytes;
            assert_eq!(u16_at(boot, 11), u16::try_from(bps).unwrap());
            assert_eq!(u32_at(boot, 36), img.sectors_per_fat);
            assert_eq!(boot.len(), usize::try_from(bps).unwrap());
            // The backup pair is at SECTORS 6 and 7 whatever a sector is.
            let at = |n: u64| n * u64::from(bps);
            let offsets: Vec<u64> = img.extents.iter().take(4).map(|e| e.offset).collect();
            assert_eq!(offsets, vec![0, at(1), at(6), at(7)]);
        }
        // At 1024-byte sectors the whole tree is still walkable, materialized.
        let mut v = esp(fallback_tree());
        v.bytes_per_sector = 1024;
        v.total_sectors = u64::from(MIN_FAT32_CLUSTERS) + 8192;
        let img = build(&v).unwrap();
        let bytes = img.to_vec().unwrap();
        let (efi, _) = lookup(&bytes, cluster_at(&img, ROOT_CLUSTER), "EFI").unwrap();
        let (boot, _) = lookup(&bytes, cluster_at(&img, efi), "BOOT").unwrap();
        assert!(lookup(&bytes, cluster_at(&img, boot), "BOOTX64.EFI").is_some());
    }

    /// A `Stream` file contributes a PLACEMENT and no extent: the caller writes
    /// those bytes itself, which is the whole point of not requiring them.
    #[test]
    fn a_stream_file_reserves_space_without_being_given_bytes() {
        let bytes = vec![0x5au8; 5000];
        let held = build(&esp(vec![("BIG.BIN".into(), Node::File(&bytes))])).unwrap();
        let streamed = build(&esp(vec![("BIG.BIN".into(), Node::Stream(5000))])).unwrap();
        // Same layout either way — the length is all the layout depends on.
        assert_eq!(held.placements, streamed.placements);
        assert_eq!(held.cluster_count, streamed.cluster_count);
        // ...and one fewer extent, because nothing was handed over to write.
        assert_eq!(held.extents.len(), streamed.extents.len() + 1);
        let p = streamed.placements.first().unwrap();
        assert_eq!((p.path.as_str(), p.len), ("\\BIG.BIN", 5000));
        // The directory entry still records the size and a real first cluster.
        let image = streamed.to_vec().unwrap();
        let (cluster, size) =
            lookup(&image, cluster_at(&streamed, ROOT_CLUSTER), "BIG.BIN").unwrap();
        assert_eq!(size, 5000);
        assert_eq!(cluster_at(&streamed, cluster) as u64, p.offset);
        // Writing the bytes at the placement reproduces the held-bytes image.
        let mut filled = image;
        let at = usize::try_from(p.offset).unwrap();
        filled.get_mut(at..at + 5000).unwrap().copy_from_slice(&bytes);
        assert_eq!(filled, held.to_vec().unwrap());
    }

    /// Zero is not a legal FAT date — it encodes month 0 and day 0 — and the
    /// write stamp is the one the spec does not make optional.
    #[test]
    fn the_write_stamp_is_a_legal_date_and_the_optional_ones_are_absent() {
        let img = build(&esp(fallback_tree())).unwrap();
        let bytes = img.to_vec().unwrap();
        let root = cluster_at(&img, ROOT_CLUSTER);
        // The label entry is first; EFI is the next.
        let e = root + 32;
        assert_eq!(u16_at(&bytes, e + 24), FAT_EPOCH_DATE, "write date");
        assert_eq!(u16_at(&bytes, e + 22), 0, "midnight");
        // Creation and last-access stay zero: the spec's "unsupported".
        assert!(bytes.get(e + 13..e + 20).unwrap().iter().all(|b| *b == 0));
    }

    /// A volume with no free clusters has no next-free cluster to name, and the
    /// hint must be the "unknown" sentinel rather than one past the end.
    #[test]
    fn a_full_volume_reports_an_unknown_next_free_hint() {
        let img = build(&esp(fallback_tree())).unwrap();
        // Fill the volume exactly: the root plus one file over every remaining
        // cluster.
        let all = u64::from(img.cluster_count - 1)
            * u64::from(img.sectors_per_cluster)
            * u64::from(img.bytes_per_sector);
        let full = build(&esp(vec![("FULL.BIN".into(), Node::Stream(all))])).unwrap();
        let bytes = full.to_vec().unwrap();
        let fsinfo = bytes.get(512..1024).unwrap();
        assert_eq!(u32_at(fsinfo, 488), 0, "no free clusters");
        assert_eq!(u32_at(fsinfo, 492), FSINFO_UNKNOWN, "next-free hint");
        // One byte more than fits is refused, so the boundary is exact.
        assert!(build(&esp(vec![("FULL.BIN".into(), Node::Stream(all + 1))])).is_err());
    }

    /// No clock, no allocator address, no generated serial: one volume is one
    /// byte sequence.
    #[test]
    fn two_builds_of_one_volume_are_byte_identical() {
        let a = build(&esp(fallback_tree())).unwrap().to_vec().unwrap();
        let b = build(&esp(fallback_tree())).unwrap().to_vec().unwrap();
        assert_eq!(a, b);
    }

    /// An explicit cluster size is honoured, and one that cannot work is
    /// refused rather than quietly replaced.
    #[test]
    fn an_explicit_cluster_size_is_taken_or_refused() {
        let mut v = esp(fallback_tree());
        v.total_sectors = 1_048_576; // 512 MiB
        v.sectors_per_cluster = Some(8); // 4 KiB clusters
        let img = build(&v).unwrap();
        assert_eq!(img.sectors_per_cluster, 8);
        assert!(img.cluster_count >= MIN_FAT32_CLUSTERS);

        v.sectors_per_cluster = Some(64); // 32 KiB: too few clusters here
        assert!(build(&v).is_err());
        v.sectors_per_cluster = Some(3); // not a power of two
        assert!(build(&v).is_err());
        v.sectors_per_cluster = Some(128); // 64 KiB exceeds FAT's maximum
        assert!(build(&v).is_err());
    }
}

//! td-fat over ESP images built in the test by td's own FAT32 writer
//! (`engine/src/fat.rs`, which td-install formats the ESP with): admission
//! and refusal, every operation cut before, inside and after each of its
//! writes under two power-loss models, reclaim, and agreement with an
//! independent reader (`fat_reader.py`, whose output `fat_reader.txt`
//! holds).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

#[allow(
    dead_code,
    reason = "td's own FAT32 writer builds the images under test"
)]
#[path = "../../engine/src/fat.rs"]
mod engine_fat;
#[allow(dead_code, reason = "shared dependency-free SHA-256 implementation")]
#[path = "../../engine/src/sha256.rs"]
mod sha256;

use engine_fat::{Node, Volume};
use std::collections::BTreeMap;
use std::io;
use td_fat::{BootDir, CommitError, Device, Error, Esp, Input};

/// Where the ESP starts on the test disk, so no offset can pass for zero.
const BASE: u64 = 1 << 20;

/// A sparse disk whose atomic write unit is 512 bytes. A power
/// cut lands after `budget` unit writes: the write that crosses it is cut
/// inside, and every later call fails. `after_cut` gives the disk as it
/// boots next with every write since the last sync, and `cut_variants`
/// each way a device may also lose some of those: none, all, or any one
/// alone, the rest landing.
#[derive(Clone)]
struct Disk {
    unit: usize,
    len: u64,
    units: BTreeMap<u64, Vec<u8>>,
    budget: Option<usize>,
    dead: bool,
    /// Since the last sync: each unit written, what it held before and
    /// what it was given.
    unsynced: Vec<(u64, Option<Vec<u8>>, Vec<u8>)>,
    writes: usize,
}

impl Disk {
    fn new(unit: usize, len: u64) -> Self {
        Self {
            unit,
            len,
            units: BTreeMap::new(),
            budget: None,
            dead: false,
            unsynced: Vec::new(),
            writes: 0,
        }
    }

    fn poke(&mut self, offset: u64, bytes: &[u8]) {
        for (i, b) in bytes.iter().enumerate() {
            let at = offset + i as u64;
            let unit = at / self.unit as u64;
            let within = (at % self.unit as u64) as usize;
            let slot = self.units.entry(unit).or_insert_with(|| vec![0; self.unit]);
            slot[within] = *b;
        }
    }

    fn peek(&self, offset: u64, len: usize) -> Vec<u8> {
        let mut out = vec![0u8; len];
        Device::read_at(self, offset, &mut out).unwrap();
        out
    }

    fn after_cut(&self, lose_unsynced: bool) -> Self {
        let keep = if lose_unsynced {
            vec![false; self.unsynced.len()]
        } else {
            vec![true; self.unsynced.len()]
        };
        self.landing(&keep)
    }

    /// The disk with exactly the unsynced writes `keep` names landed.
    fn landing(&self, keep: &[bool]) -> Self {
        let mut next = self.clone();
        for (unit, old, _) in next.unsynced.iter().rev() {
            match old {
                Some(bytes) => next.units.insert(*unit, bytes.clone()),
                None => next.units.remove(unit),
            };
        }
        for ((unit, _, new), kept) in self.unsynced.iter().zip(keep) {
            if *kept {
                next.units.insert(*unit, new.clone());
            }
        }
        next.unsynced.clear();
        next.budget = None;
        next.dead = false;
        next.writes = 0;
        next
    }

    fn cut_variants(&self) -> Vec<(String, Self)> {
        let n = self.unsynced.len();
        let mut out = vec![
            ("all unsynced landed".to_string(), self.after_cut(false)),
            ("no unsynced landed".to_string(), self.after_cut(true)),
        ];
        for lost in 0..n {
            let mut keep = vec![true; n];
            keep[lost] = false;
            out.push((
                format!("unsynced write {lost} of {n} lost"),
                self.landing(&keep),
            ));
        }
        out
    }

    /// SHA-256 over every non-zero 512-byte unit of the volume, each after
    /// its index as eight big-endian bytes: `fat_reader.py`'s image digest.
    fn digest(&self, volume_len: u64) -> String {
        let mut h = sha256::Sha256::new();
        let zero = [0u8; 512];
        let mut unit = vec![0u8; 512];
        let first = BASE / 512;
        let mut indexes: Vec<u64> = Vec::new();
        for &u in self.units.keys() {
            let start = u * self.unit as u64;
            for k in 0..(self.unit as u64 / 512) {
                let at = start + k * 512;
                if at >= BASE && at < BASE + volume_len {
                    indexes.push(at / 512);
                }
            }
        }
        indexes.sort_unstable();
        indexes.dedup();
        for index in indexes {
            Device::read_at(self, index * 512, &mut unit).unwrap();
            if unit != zero {
                h.update(&(index - first).to_be_bytes());
                h.update(&unit);
            }
        }
        sha256::to_base16(&h.finalize())
    }
}

impl Device for Disk {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        if self.dead {
            return Err(io::Error::other("the power is off"));
        }
        if offset + buf.len() as u64 > self.len {
            return Err(io::Error::other("read past the disk"));
        }
        let unit = self.unit as u64;
        let mut done = 0usize;
        while done < buf.len() {
            let at = offset + done as u64;
            let within = (at % unit) as usize;
            let take = (self.unit - within).min(buf.len() - done);
            let out = &mut buf[done..done + take];
            match self.units.get(&(at / unit)) {
                Some(u) => out.copy_from_slice(&u[within..within + take]),
                None => out.fill(0),
            }
            done += take;
        }
        Ok(())
    }

    fn write_at(&mut self, offset: u64, buf: &[u8]) -> io::Result<()> {
        if self.dead {
            return Err(io::Error::other("the power is off"));
        }
        assert_eq!(offset % self.unit as u64, 0, "unaligned write");
        assert_eq!(buf.len() % self.unit, 0, "partial-sector write");
        assert!(offset + buf.len() as u64 <= self.len, "write past the disk");
        for (n, chunk) in buf.chunks(self.unit).enumerate() {
            if let Some(budget) = self.budget.as_mut() {
                if *budget == 0 {
                    self.dead = true;
                    return Err(io::Error::other("power cut"));
                }
                *budget -= 1;
            }
            let unit = offset / self.unit as u64 + n as u64;
            let old = self.units.insert(unit, chunk.to_vec());
            self.unsynced.push((unit, old, chunk.to_vec()));
            self.writes += 1;
        }
        Ok(())
    }

    fn sync(&mut self) -> io::Result<()> {
        if self.dead {
            return Err(io::Error::other("the power is off"));
        }
        self.unsynced.clear();
        Ok(())
    }
}

/// Deterministic bytes standing in for a kernel or an initramfs.
fn bytes(seed: u32, len: usize) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9e37_79b9) | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            (x >> 24) as u8
        })
        .collect()
}

#[derive(Clone, Debug, PartialEq)]
struct Files {
    kernel: Vec<u8>,
    initrd: Vec<u8>,
}

fn files(seed: u32, kernel: usize, initrd: usize) -> Files {
    Files {
        kernel: bytes(seed, kernel),
        initrd: bytes(seed + 1, initrd),
    }
}

/// A layout's volume: `bps`-byte sectors, `total` of them, one sector per
/// cluster unless `spc` says otherwise.
struct Layout {
    bps: u32,
    total: u64,
    spc: Option<u32>,
}

const SMALL: Layout = Layout {
    bps: 512,
    total: 67_000,
    spc: Some(1),
};

impl Layout {
    fn volume_len(&self) -> u64 {
        self.total * u64::from(self.bps)
    }
}

/// An ESP as td-install formats it, with `tree` under its root.
fn image_with(layout: &Layout, tree: Vec<(String, Node<'_>)>) -> Disk {
    let volume = Volume {
        bytes_per_sector: layout.bps,
        total_sectors: layout.total,
        hidden_sectors: (BASE / u64::from(layout.bps)) as u32,
        volume_id: 0x7d1e_9a30,
        label: "TD-ESP".into(),
        sectors_per_cluster: layout.spc,
        root: tree,
    };
    let image = engine_fat::build(&volume).unwrap();
    // 512 bytes is the device's atomic unit whatever the BPB claims, so a
    // 4096-byte sector write can be cut inside.
    let mut disk = Disk::new(512, BASE + layout.volume_len() + 4096);
    for extent in &image.extents {
        disk.poke(BASE + extent.offset, &extent.bytes);
    }
    disk
}

fn boot_tree<'a>(entries: Vec<(String, Node<'a>)>) -> Vec<(String, Node<'a>)> {
    vec![(
        "EFI".into(),
        Node::Dir(vec![("BOOT".into(), Node::Dir(entries))]),
    )]
}

fn image(layout: &Layout, old: &Files) -> Disk {
    image_with(
        layout,
        boot_tree(vec![
            ("BOOTX64.EFI".into(), Node::File(&old.kernel)),
            ("INITRD".into(), Node::File(&old.initrd)),
        ]),
    )
}

fn open(disk: Disk, layout: &Layout) -> Result<Esp<Disk>, Error> {
    Esp::open(disk, BASE, layout.volume_len())
}

fn read_all(esp: &Esp<Disk>, file: &td_fat::FileEntry) -> Vec<u8> {
    let mut out = Vec::new();
    esp.read(file, &mut |chunk| {
        out.extend_from_slice(chunk);
        Ok(())
    })
    .unwrap();
    out
}

fn stage(esp: &mut Esp<Disk>, new: &Files) -> Result<(), Error> {
    esp.stage(
        Input {
            len: new.kernel.len() as u64,
            reader: &mut new.kernel.as_slice(),
        },
        Input {
            len: new.initrd.len() as u64,
            reader: &mut new.initrd.as_slice(),
        },
    )
}

fn direct(esp: &mut Esp<Disk>, new: &Files) -> Result<(), Error> {
    esp.write_direct(
        Input {
            len: new.kernel.len() as u64,
            reader: &mut new.kernel.as_slice(),
        },
        Input {
            len: new.initrd.len() as u64,
            reader: &mut new.initrd.as_slice(),
        },
    )
}

/// What a boot after a cut finds.
#[derive(Clone, Debug, PartialEq)]
enum Found {
    /// The pair, with no staged entries.
    Pair(Files),
    /// The pair beside a complete staged pair.
    Staged(Files, Files),
}

fn found(disk: Disk, layout: &Layout) -> Found {
    let esp = open(disk, layout).unwrap_or_else(|e| panic!("a cut left an ESP td refuses: {e}"));
    let boot = esp.boot().clone();
    let pair = Files {
        kernel: read_all(&esp, &boot.pair.kernel),
        initrd: read_all(&esp, &boot.pair.initrd),
    };
    match boot.staged {
        None => Found::Pair(pair),
        Some(staged) => Found::Staged(
            pair,
            Files {
                kernel: read_all(&esp, &staged.kernel),
                initrd: read_all(&esp, &staged.initrd),
            },
        ),
    }
}

/// Run `op` on `make()`'s ESP cut after every unit write it makes, under
/// both power-loss models, and require each next boot to find one of
/// `allowed`, then the next update to stage over it and leave no lost
/// cluster.
fn cut_everywhere(
    layout: &Layout,
    make: &dyn Fn() -> Disk,
    op: &dyn Fn(&mut Esp<Disk>) -> Result<(), Error>,
    allowed: &[Found],
    done: &Found,
) {
    let total = {
        let mut esp = open(make(), layout).unwrap();
        op(&mut esp).unwrap();
        let disk = esp.into_device();
        assert_eq!(found(disk.after_cut(false), layout), *done);
        disk.writes
    };
    assert!(total > 0);
    let next = files(900, 2600, 1500);
    for budget in 0..=total {
        let mut disk = make();
        disk.budget = Some(budget);
        let mut esp = open(disk, layout).unwrap();
        let result = op(&mut esp);
        assert_eq!(result.is_ok(), budget == total, "budget {budget}");
        for (how, after) in esp.into_device().cut_variants() {
            let seen = found(after.clone(), layout);
            assert!(
                allowed.contains(&seen),
                "budget {budget} of {total}, {how}: {seen:?}"
            );
            let mut esp = open(after, layout).unwrap();
            stage(&mut esp, &next).unwrap();
            assert_eq!(esp.lost_clusters(), 0, "budget {budget}, {how}: reclaim");
            let staged = esp.boot().staged.unwrap();
            assert_eq!(read_all(&esp, &staged.kernel), next.kernel);
            assert_eq!(read_all(&esp, &staged.initrd), next.initrd);
        }
    }
}

fn old() -> Files {
    files(1, 6000, 3100)
}

fn new() -> Files {
    files(7, 7300, 2000)
}

#[test]
fn td_layout_is_admitted_with_its_pair() {
    let old = old();
    let esp = open(image(&SMALL, &old), &SMALL).unwrap();
    let boot = esp.boot().clone();
    assert_eq!(boot.staged, None);
    assert_eq!(boot.pair.kernel.index, 2);
    assert_eq!(boot.pair.initrd.index, 3);
    assert_eq!(read_all(&esp, &boot.pair.kernel), old.kernel);
    assert_eq!(read_all(&esp, &boot.pair.initrd), old.initrd);
    assert_eq!(esp.lost_clusters(), 0);
}

#[test]
fn td_s_own_512_mib_layout_and_4096_byte_sectors_are_admitted() {
    let old = old();
    for layout in [
        Layout {
            bps: 512,
            total: 1 << 20,
            spc: None,
        },
        Layout {
            bps: 4096,
            total: 66_000,
            spc: Some(1),
        },
    ] {
        let mut esp = open(image(&layout, &old), &layout).unwrap();
        let new = new();
        stage(&mut esp, &new).unwrap();
        let read = esp.boot().clone();
        esp.commit_staged(&read).unwrap();
        assert_eq!(found(esp.into_device(), &layout), Found::Pair(new));
    }
}

#[test]
fn staging_then_committing_installs_the_new_pair() {
    let (old, new) = (old(), new());
    let mut esp = open(image(&SMALL, &old), &SMALL).unwrap();
    let free = esp.free_clusters();
    stage(&mut esp, &new).unwrap();
    let read = esp.boot().clone();
    let staged = read.staged.unwrap();
    assert_eq!((staged.kernel.index, staged.initrd.index), (4, 5));
    assert_eq!(read_all(&esp, &staged.kernel), new.kernel);
    esp.commit_staged(&read).unwrap();
    let boot = esp.boot().clone();
    assert_eq!(boot.staged, None);
    assert_eq!(boot.pair.kernel.first_cluster, staged.kernel.first_cluster);
    assert_eq!(boot.pair.kernel.size, staged.kernel.size);
    assert_eq!(esp.lost_clusters(), 0);
    // The old chains are freed: only the size difference is spent.
    let cb = esp.cluster_bytes() as usize;
    let clusters = |f: &Files| f.kernel.len().div_ceil(cb) + f.initrd.len().div_ceil(cb);
    assert_eq!(
        esp.free_clusters() as usize + clusters(&new),
        free as usize + clusters(&old)
    );
    assert_eq!(found(esp.into_device(), &SMALL), Found::Pair(new));
}

#[test]
fn a_second_staging_replaces_the_first_and_deletion_removes_it() {
    let (old, new) = (old(), new());
    let mut esp = open(image(&SMALL, &old), &SMALL).unwrap();
    stage(&mut esp, &new).unwrap();
    let again = files(30, 4000, 4000);
    stage(&mut esp, &again).unwrap();
    assert_eq!(esp.lost_clusters(), 0);
    let read = esp.boot().clone();
    esp.delete_staged(&read).unwrap();
    assert_eq!(esp.lost_clusters(), 0);
    assert_eq!(found(esp.into_device(), &SMALL), Found::Pair(old));
}

#[test]
fn the_direct_form_replaces_the_pair_in_one_write() {
    let (old, new) = (old(), new());
    let mut esp = open(image(&SMALL, &old), &SMALL).unwrap();
    direct(&mut esp, &new).unwrap();
    assert_eq!(esp.lost_clusters(), 0);
    assert_eq!(found(esp.into_device(), &SMALL), Found::Pair(new));
}

#[test]
fn staging_cut_anywhere_leaves_the_old_pair_or_it_with_the_staged_pair() {
    let (old, new) = (old(), new());
    let make = || image(&SMALL, &old);
    cut_everywhere(
        &SMALL,
        &make,
        &|esp| stage(esp, &new),
        &[
            Found::Pair(old.clone()),
            Found::Staged(old.clone(), new.clone()),
        ],
        &Found::Staged(old.clone(), new.clone()),
    );
}

#[test]
fn restaging_cut_anywhere_never_leaves_half_of_either_staged_pair() {
    let (old, new) = (old(), new());
    let again = files(30, 4000, 4000);
    let make = || {
        let mut esp = open(image(&SMALL, &old), &SMALL).unwrap();
        stage(&mut esp, &new).unwrap();
        esp.into_device().after_cut(false)
    };
    cut_everywhere(
        &SMALL,
        &make,
        &|esp| stage(esp, &again),
        &[
            Found::Staged(old.clone(), new.clone()),
            Found::Pair(old.clone()),
            Found::Staged(old.clone(), again.clone()),
        ],
        &Found::Staged(old.clone(), again.clone()),
    );
}

/// The staged commit's failure as the cut tests compare it, whatever its
/// place.
fn commit(esp: &mut Esp<Disk>) -> Result<(), Error> {
    let read = esp.boot().clone();
    esp.commit_staged(&read).map_err(|error| match error {
        CommitError::Before(e) | CommitError::Uncertain(e) | CommitError::After(e) => e,
    })
}

/// A device that fails at the directory sector's write is uncertain; one
/// that fails once that write is synced, freeing the old chains, is after
/// it, the new pair already `EFI/BOOT`'s.
#[test]
fn a_staged_commit_failure_says_where_it_failed() {
    let (old, new) = (old(), new());
    let staged = || {
        let mut esp = open(image(&SMALL, &old), &SMALL).unwrap();
        stage(&mut esp, &new).unwrap();
        esp.into_device().after_cut(false)
    };
    for (budget, after) in [(0, false), (1, true)] {
        let mut disk = staged();
        disk.budget = Some(budget);
        let mut esp = open(disk, &SMALL).unwrap();
        let read = esp.boot().clone();
        let error = esp.commit_staged(&read).unwrap_err();
        match (&error, after) {
            (CommitError::Uncertain(_), false) | (CommitError::After(_), true) => {}
            _ => panic!("budget {budget}: {error}"),
        }
        let next = esp.into_device().after_cut(false);
        let expected = if after {
            Found::Pair(new.clone())
        } else {
            Found::Staged(old.clone(), new.clone())
        };
        assert_eq!(found(next, &SMALL), expected, "budget {budget}");
    }
}

#[test]
fn the_staged_commit_cut_anywhere_leaves_the_staged_or_the_new_pair() {
    let (old, new) = (old(), new());
    let make = || {
        let mut esp = open(image(&SMALL, &old), &SMALL).unwrap();
        stage(&mut esp, &new).unwrap();
        esp.into_device().after_cut(false)
    };
    cut_everywhere(
        &SMALL,
        &make,
        &commit,
        &[
            Found::Staged(old.clone(), new.clone()),
            Found::Pair(new.clone()),
        ],
        &Found::Pair(new.clone()),
    );
}

#[test]
fn the_direct_commit_cut_anywhere_leaves_the_old_or_the_new_pair() {
    let (old, new) = (old(), new());
    let make = || image(&SMALL, &old);
    cut_everywhere(
        &SMALL,
        &make,
        &|esp| direct(esp, &new),
        &[Found::Pair(old.clone()), Found::Pair(new.clone())],
        &Found::Pair(new.clone()),
    );
}

#[test]
fn deletion_cut_anywhere_leaves_the_staged_pair_or_none() {
    let (old, new) = (old(), new());
    let make = || {
        let mut esp = open(image(&SMALL, &old), &SMALL).unwrap();
        stage(&mut esp, &new).unwrap();
        esp.into_device().after_cut(false)
    };
    cut_everywhere(
        &SMALL,
        &make,
        &|esp| {
            let read = esp.boot().clone();
            esp.delete_staged(&read)
        },
        &[
            Found::Staged(old.clone(), new.clone()),
            Found::Pair(old.clone()),
        ],
        &Found::Pair(old.clone()),
    );
}

/// A cut that leaks: the new files and their chains are on disk, the
/// directory sector that would name them is not.
fn leaked(old: &Files, new: &Files) -> Disk {
    let total = {
        let mut esp = open(image(&SMALL, old), &SMALL).unwrap();
        stage(&mut esp, new).unwrap();
        esp.into_device().writes
    };
    // The staging's last three writes are its directory sector and the two
    // FSInfo copies.
    let mut disk = image(&SMALL, old);
    disk.budget = Some(total - 3);
    let mut esp = open(disk, &SMALL).unwrap();
    assert!(stage(&mut esp, new).is_err());
    esp.into_device().after_cut(false)
}

#[test]
fn the_next_update_reclaims_what_a_cut_leaked() {
    let (old, new) = (old(), new());
    let disk = leaked(&old, &new);
    let mut esp = open(disk, &SMALL).unwrap();
    let cb = esp.cluster_bytes() as usize;
    let leaked = new.kernel.len().div_ceil(cb) + new.initrd.len().div_ceil(cb);
    assert_eq!(esp.lost_clusters() as usize, leaked);
    assert_eq!(esp.boot().staged, None);
    let free = esp.free_clusters() as usize;
    stage(&mut esp, &new).unwrap();
    assert_eq!(esp.lost_clusters(), 0);
    assert_eq!(esp.free_clusters() as usize, free);
}

#[test]
fn reclaim_frees_one_fat_s_strays_and_keeps_bad_clusters() {
    let old = old();
    let mut disk = image(&SMALL, &old);
    let (fat0, fat1) = fat_offsets(&disk, &SMALL);
    let probe = open(disk.clone(), &SMALL).unwrap();
    let last = probe.cluster_count() + 1;
    // A stray allocation in the first FAT alone, one in both, a bad mark.
    disk.poke(fat0 + 4 * u64::from(last), &0x0fff_ffffu32.to_le_bytes());
    disk.poke(
        fat1 + 4 * u64::from(last - 1),
        &0x0fff_ffffu32.to_le_bytes(),
    );
    disk.poke(
        fat0 + 4 * u64::from(last - 2),
        &0x1fff_ffffu32.to_le_bytes(),
    );
    disk.poke(
        fat1 + 4 * u64::from(last - 2),
        &0x0fff_ffffu32.to_le_bytes(),
    );
    for fat in [fat0, fat1] {
        disk.poke(fat + 4 * u64::from(last - 3), &0x0fff_fff7u32.to_le_bytes());
    }
    let mut esp = open(disk, &SMALL).unwrap();
    assert_eq!(esp.lost_clusters(), 3);
    stage(&mut esp, &new()).unwrap();
    assert_eq!(esp.lost_clusters(), 0);
    let disk = esp.into_device();
    for fat in [fat0, fat1] {
        let entry =
            |c: u32| u32::from_le_bytes(disk.peek(fat + 4 * u64::from(c), 4).try_into().unwrap());
        assert_eq!(entry(last) & 0x0fff_ffff, 0);
        assert_eq!(entry(last - 1) & 0x0fff_ffff, 0);
        assert_eq!(entry(last - 3), 0x0fff_fff7);
    }
    // The reserved high bits survive a free.
    let entry = u32::from_le_bytes(
        disk.peek(fat0 + 4 * u64::from(last - 2), 4)
            .try_into()
            .unwrap(),
    );
    assert_eq!(entry, 0x1000_0000);
}

fn fat_offsets(disk: &Disk, layout: &Layout) -> (u64, u64) {
    let bpb = disk.peek(BASE, 512);
    let reserved = u64::from(u16::from_le_bytes([bpb[14], bpb[15]]));
    let fatsz = u64::from(u32::from_le_bytes(bpb[36..40].try_into().unwrap()));
    let bps = u64::from(layout.bps);
    (BASE + reserved * bps, BASE + (reserved + fatsz) * bps)
}

/// The byte offset of `EFI/BOOT`'s first sector in a `layout` image: the
/// third cluster td's writer allocates, after the root's and EFI's.
fn boot_sector_in(disk: &Disk, layout: &Layout) -> u64 {
    let bpb = disk.peek(BASE, 512);
    let reserved = u64::from(u16::from_le_bytes([bpb[14], bpb[15]]));
    let fatsz = u64::from(u32::from_le_bytes(bpb[36..40].try_into().unwrap()));
    let spc = u64::from(bpb[13]);
    let bps = u64::from(layout.bps);
    let boot = BASE + (reserved + 2 * fatsz) * bps + 2 * spc * bps;
    let sector = disk.peek(boot, 96);
    assert_eq!(&sector[0..11], b".          ");
    assert_eq!(&sector[64..75], b"BOOTX64 EFI");
    boot
}

fn boot_sector(disk: &Disk) -> u64 {
    boot_sector_in(disk, &SMALL)
}

#[test]
fn a_foreign_boot_directory_is_refused_before_anything_is_written() {
    let old = old();
    let grub = bytes(5, 100);
    let foreign: Vec<(&str, Disk)> = vec![
        (
            "another file",
            image_with(
                &SMALL,
                boot_tree(vec![
                    ("BOOTX64.EFI".into(), Node::File(&old.kernel)),
                    ("INITRD".into(), Node::File(&old.initrd)),
                    ("GRUBX64.EFI".into(), Node::File(&grub)),
                ]),
            ),
        ),
        (
            "no initrd",
            image_with(
                &SMALL,
                boot_tree(vec![("BOOTX64.EFI".into(), Node::File(&old.kernel))]),
            ),
        ),
        (
            "a subdirectory",
            image_with(
                &SMALL,
                boot_tree(vec![
                    ("BOOTX64.EFI".into(), Node::File(&old.kernel)),
                    ("INITRD".into(), Node::File(&old.initrd)),
                    ("X".into(), Node::Dir(vec![])),
                ]),
            ),
        ),
        ("no EFI", image_with(&SMALL, vec![])),
        ("a long name", {
            let mut disk = image(&SMALL, &old);
            let at = boot_sector(&disk) + 4 * 32;
            let mut lfn = [0u8; 32];
            lfn[0] = 0x41;
            lfn[11] = 0x0f;
            disk.poke(at, &lfn);
            disk
        }),
        ("an entry after the end marker", {
            let mut disk = image(&SMALL, &old);
            let at = boot_sector(&disk);
            let kernel = disk.peek(at + 2 * 32, 32);
            let mut stray = kernel.clone();
            stray[..11].copy_from_slice(b"INITRD  NEW");
            disk.poke(at + 5 * 32, &stray);
            disk
        }),
        ("half a staged pair", {
            let mut esp = open(image(&SMALL, &old), &SMALL).unwrap();
            stage(&mut esp, &new()).unwrap();
            let index = esp.boot().staged.unwrap().initrd.index as u64;
            let mut disk = esp.into_device().after_cut(false);
            let at = boot_sector(&disk) + index * 32;
            disk.poke(at, &[0xe5]);
            disk
        }),
        ("a read-only kernel", {
            let mut disk = image(&SMALL, &old);
            let at = boot_sector(&disk) + 2 * 32 + 11;
            disk.poke(at, &[0x21]);
            disk
        }),
    ];
    for (why, disk) in foreign {
        match open(disk, &SMALL) {
            Err(Error::Foreign(_)) => {}
            Err(other) => panic!("{why}: {other}"),
            Ok(_) => panic!("{why}: admitted"),
        }
    }
}

#[test]
fn an_entry_past_the_first_sector_is_refused() {
    let layout = Layout {
        bps: 512,
        total: 140_000,
        spc: Some(2),
    };
    let old = old();
    let mut disk = image(&layout, &old);
    let boot = boot_sector_in(&disk, &layout);
    // The first sector ends without an end marker, and the second holds a
    // file.
    for i in 4..16u64 {
        disk.poke(boot + i * 32, &[0xe5]);
    }
    assert!(open(disk.clone(), &layout).is_ok());
    // An empty file owns no cluster, so only the directory's own rule
    // refuses it.
    let mut stray = disk.peek(boot + 2 * 32, 32);
    stray[..11].copy_from_slice(b"BOOTX64 NEW");
    stray[20..22].fill(0);
    stray[26..32].fill(0);
    disk.poke(boot + 512, &stray);
    match open(disk, &layout) {
        Err(Error::Foreign(_)) => {}
        other => panic!("{:?}", other.err()),
    }
}

#[test]
fn too_little_space_is_refused_before_anything_is_written() {
    let big = files(11, 16 << 20, 1 << 20);
    let disk = image(&SMALL, &big);
    let mut esp = open(disk, &SMALL).unwrap();
    let free = esp.free_clusters();
    match stage(&mut esp, &big) {
        Err(Error::Space { needed, available }) => {
            assert!(needed > available);
            assert_eq!(available, free);
        }
        other => panic!("{other:?}"),
    }
    match direct(&mut esp, &big) {
        Err(Error::Space { .. }) => {}
        other => panic!("{other:?}"),
    }
    assert_eq!(esp.into_device().writes, 0);
    // What fits beside the old pair is staged.
    let fits = files(12, 14 << 20, 1 << 20);
    let mut esp = open(image(&SMALL, &big), &SMALL).unwrap();
    stage(&mut esp, &fits).unwrap();
}

/// `fits` answers what staging and the direct form would before their
/// first write, and writes nothing itself.
#[test]
fn fits_answers_the_space_check_and_writes_nothing() {
    let big = files(11, 16 << 20, 1 << 20);
    let fitting = files(12, 14 << 20, 1 << 20);
    let esp = open(image(&SMALL, &big), &SMALL).unwrap();
    for staged in [true, false] {
        match esp.fits(big.kernel.len() as u64, big.initrd.len() as u64, staged) {
            Err(Error::Space { needed, available }) => assert!(needed > available),
            other => panic!("{other:?}"),
        }
        esp.fits(
            fitting.kernel.len() as u64,
            fitting.initrd.len() as u64,
            staged,
        )
        .unwrap();
    }
    // An empty file is refused as staging refuses it.
    assert!(matches!(
        esp.fits(0, fitting.initrd.len() as u64, true),
        Err(Error::Input(_))
    ));
    assert_eq!(esp.into_device().writes, 0);
    // A staged pair's clusters count as free, as staging replaces it.
    let old = files(1, 12 << 20, 1 << 20);
    let mut esp = open(image(&SMALL, &old), &SMALL).unwrap();
    stage(&mut esp, &files(2, 12 << 20, 1 << 20)).unwrap();
    esp.fits(12 << 20, 1 << 20, true).unwrap();
}

#[test]
fn space_counts_what_the_reclaim_and_a_replaced_staging_free() {
    let old = files(1, 12 << 20, 1 << 20);
    let first = files(2, 12 << 20, 1 << 20);
    let mut esp = open(image(&SMALL, &old), &SMALL).unwrap();
    stage(&mut esp, &first).unwrap();
    // A second pair of the same size fits only once the first is gone.
    assert!(esp.free_clusters() < 13 << 11);
    stage(&mut esp, &files(3, 12 << 20, 1 << 20)).unwrap();
    assert_eq!(esp.lost_clusters(), 0);
}

#[test]
fn damage_is_refused() {
    let old = old();
    let base = image(&SMALL, &old);
    let (fat0, fat1) = fat_offsets(&base, &SMALL);
    let esp = open(base.clone(), &SMALL).unwrap();
    let kernel = esp.boot().pair.kernel;
    let initrd = esp.boot().pair.initrd;
    drop(esp);
    let mut mirror = base.clone();
    mirror.poke(
        fat1 + 4 * u64::from(kernel.first_cluster),
        &0x0fff_ffffu32.to_le_bytes(),
    );
    let mut shared = base.clone();
    let at = boot_sector(&shared) + 3 * 32;
    shared.poke(at + 26, &(kernel.first_cluster as u16).to_le_bytes());
    shared.poke(at + 28, &kernel.size.to_le_bytes());
    let mut size = base.clone();
    size.poke(at + 28, &(initrd.size + 4096).to_le_bytes());
    let mut looped = base.clone();
    for fat in [fat0, fat1] {
        looped.poke(
            fat + 4 * u64::from(kernel.first_cluster + 1),
            &kernel.first_cluster.to_le_bytes(),
        );
    }
    for (why, disk) in [
        ("mirror", mirror),
        ("shared", shared),
        ("size", size),
        ("loop", looped),
    ] {
        match open(disk, &SMALL) {
            Err(Error::Damaged(_)) => {}
            Err(other) => panic!("{why}: {other}"),
            Ok(_) => panic!("{why}: admitted"),
        }
    }
}

#[test]
fn volumes_td_does_not_write_are_refused() {
    let old = old();
    let base = image(&SMALL, &old);
    let cases: Vec<(&str, usize, Vec<u8>)> = vec![
        ("signature", 510, vec![0, 0]),
        ("one FAT", 16, vec![1]),
        ("FAT16 root entries", 17, vec![0, 2]),
        ("mirroring off", 40, vec![0x80, 0]),
        ("sector size", 11, vec![0, 3]),
        ("FSInfo elsewhere", 48, vec![2, 0]),
    ];
    for (why, at, value) in cases {
        let mut disk = base.clone();
        disk.poke(BASE + at as u64, &value);
        match open(disk, &SMALL) {
            Err(Error::Volume(_)) => {}
            Err(other) => panic!("{why}: {other}"),
            Ok(_) => panic!("{why}: admitted"),
        }
    }
    // A volume larger than its partition.
    match Esp::open(base, BASE, SMALL.volume_len() - 512) {
        Err(Error::Volume(_)) => {}
        other => panic!("short partition: {:?}", other.err()),
    }
}

#[test]
fn a_changed_directory_sector_is_refused_and_nothing_is_written() {
    let (old, new) = (old(), new());
    let mut esp = open(image(&SMALL, &old), &SMALL).unwrap();
    stage(&mut esp, &new).unwrap();
    let read = esp.boot().clone();
    let mut disk = esp.into_device().after_cut(false);
    let at = boot_sector(&disk) + 2 * 32 + 13;
    disk.poke(at, &[1]);
    let mut esp = open(disk, &SMALL).unwrap();
    assert!(matches!(
        esp.commit_staged(&read),
        Err(CommitError::Before(Error::Changed))
    ));
    assert!(matches!(esp.delete_staged(&read), Err(Error::Changed)));
    assert_eq!(esp.into_device().writes, 0);
    // A directory read without a staged pair commits nothing.
    let mut esp = open(image(&SMALL, &old), &SMALL).unwrap();
    let plain: BootDir = esp.boot().clone();
    assert!(matches!(
        esp.commit_staged(&plain),
        Err(CommitError::Before(Error::Foreign(_)))
    ));
}

#[test]
fn a_new_file_must_be_exactly_its_declared_length() {
    let (old, new) = (old(), new());
    let mut esp = open(image(&SMALL, &old), &SMALL).unwrap();
    let short = &new.kernel[..new.kernel.len() - 1];
    let result = esp.stage(
        Input {
            len: new.kernel.len() as u64,
            reader: &mut &short[..],
        },
        Input {
            len: new.initrd.len() as u64,
            reader: &mut new.initrd.as_slice(),
        },
    );
    assert!(matches!(result, Err(Error::Input(_))));
    let long = [new.kernel.as_slice(), b"x"].concat();
    let result = esp.stage(
        Input {
            len: new.kernel.len() as u64,
            reader: &mut long.as_slice(),
        },
        Input {
            len: new.initrd.len() as u64,
            reader: &mut new.initrd.as_slice(),
        },
    );
    assert!(matches!(result, Err(Error::Input(_))));
    let result = esp.stage(
        Input {
            len: 0,
            reader: &mut &b""[..],
        },
        Input {
            len: new.initrd.len() as u64,
            reader: &mut new.initrd.as_slice(),
        },
    );
    assert!(matches!(result, Err(Error::Input(_))));
    // Nothing the refused writes left is reachable or lost.
    let disk = esp.into_device().after_cut(false);
    let esp = open(disk.clone(), &SMALL).unwrap();
    assert_eq!(esp.boot().staged, None);
    assert_eq!(found(disk, &SMALL), Found::Pair(old));
}

/// The images `fat_reader.py` reads, by name.
fn reader_images() -> Vec<(&'static str, Layout, Disk)> {
    let (old, new) = (old(), new());
    let staged = {
        let mut esp = open(image(&SMALL, &old), &SMALL).unwrap();
        stage(&mut esp, &new).unwrap();
        esp.into_device().after_cut(false)
    };
    let committed = {
        let mut esp = open(staged.clone(), &SMALL).unwrap();
        let read = esp.boot().clone();
        esp.commit_staged(&read).unwrap();
        esp.into_device().after_cut(false)
    };
    let deleted = {
        let mut esp = open(staged.clone(), &SMALL).unwrap();
        let read = esp.boot().clone();
        esp.delete_staged(&read).unwrap();
        esp.into_device().after_cut(false)
    };
    let direct_disk = {
        let mut esp = open(image(&SMALL, &old), &SMALL).unwrap();
        direct(&mut esp, &new).unwrap();
        esp.into_device().after_cut(false)
    };
    let leak = leaked(&old, &new);
    let reclaimed = {
        let mut esp = open(leak.clone(), &SMALL).unwrap();
        stage(&mut esp, &files(30, 4000, 4000)).unwrap();
        esp.into_device().after_cut(false)
    };
    vec![
        ("fresh", SMALL, image(&SMALL, &old)),
        ("staged", SMALL, staged),
        ("committed", SMALL, committed),
        ("deleted", SMALL, deleted),
        ("direct", SMALL, direct_disk),
        ("leaked", SMALL, leak),
        ("reclaimed", SMALL, reclaimed),
    ]
}

/// What td-fat sees of an image, in `fat_reader.py`'s line format.
fn td_fat_view(name: &str, layout: &Layout, disk: &Disk) -> Vec<String> {
    let esp = open(disk.clone(), layout).unwrap();
    let mut out = vec![
        format!("image {name} {}", disk.digest(layout.volume_len())),
        format!(
            "geometry {name} {} {} {}",
            esp.bytes_per_sector(),
            esp.cluster_bytes() / esp.bytes_per_sector(),
            esp.cluster_count()
        ),
    ];
    let boot = esp.boot().clone();
    let mut files = vec![
        ("/EFI/BOOT/BOOTX64.EFI", boot.pair.kernel),
        ("/EFI/BOOT/INITRD", boot.pair.initrd),
    ];
    if let Some(staged) = boot.staged {
        files.push(("/EFI/BOOT/BOOTX64.NEW", staged.kernel));
        files.push(("/EFI/BOOT/INITRD.NEW", staged.initrd));
    }
    files.sort_by_key(|(path, _)| *path);
    for (path, file) in files {
        let digest = sha256::hex_digest(&read_all(&esp, &file));
        out.push(format!("file {name} {path} {} {digest}", file.size));
    }
    out.push(format!("free {name} {}", esp.free_clusters()));
    out.push(format!("lost {name} {}", esp.lost_clusters()));
    let info = disk.peek(BASE + u64::from(layout.bps), 512);
    let free = u32::from_le_bytes(info[488..492].try_into().unwrap());
    let hint = u32::from_le_bytes(info[492..496].try_into().unwrap());
    out.push(format!("fsinfo {name} {free} {hint}"));
    out
}

#[test]
fn an_independent_reader_agrees() {
    let vectors = include_str!("fat_reader.txt");
    let mut expected: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    for line in vectors.lines().filter(|l| !l.starts_with('#')) {
        let name = line.split(' ').nth(1).unwrap();
        expected.entry(name).or_default().push(line.to_string());
    }
    let images = reader_images();
    assert_eq!(expected.len(), images.len());
    for (name, layout, disk) in &images {
        let view = td_fat_view(name, layout, disk);
        assert_eq!(expected.get(name).unwrap(), &view, "{name}");
    }
    // The leaked image is the one with lost clusters, and its FSInfo is the
    // stale one a cut leaves; every completed operation's is exact.
    for (name, lines) in &expected {
        let field = |kind: &str| -> Vec<String> {
            lines
                .iter()
                .find(|l| l.starts_with(&format!("{kind} ")))
                .unwrap()
                .split(' ')
                .skip(2)
                .map(str::to_string)
                .collect()
        };
        let lost: u32 = field("lost")[0].parse().unwrap();
        assert_eq!(lost > 0, *name == "leaked", "{name}");
        if *name != "leaked" {
            assert_eq!(field("free")[0], field("fsinfo")[0], "{name}");
        }
    }
}

/// Write the reader's images to `$TD_FAT_IMAGES` for `fat_reader.py`.
#[test]
#[ignore = "host fixture: regenerates fat_reader.txt's inputs"]
fn write_reader_images() {
    let dir = std::env::var("TD_FAT_IMAGES").unwrap();
    for (name, layout, disk) in reader_images() {
        let mut volume = vec![0u8; layout.volume_len() as usize];
        Device::read_at(&disk, BASE, &mut volume).unwrap();
        std::fs::write(format!("{dir}/{name}.img"), volume).unwrap();
    }
}

/// The first sector of the `n`th directory td's writer allocates in a
/// `layout` image: 0 the root, 1 `EFI`, 2 `EFI/BOOT`.
fn dir_sector_in(disk: &Disk, layout: &Layout, n: u64) -> u64 {
    let bpb = disk.peek(BASE, 512);
    let reserved = u64::from(u16::from_le_bytes([bpb[14], bpb[15]]));
    let fatsz = u64::from(u32::from_le_bytes(bpb[36..40].try_into().unwrap()));
    let spc = u64::from(bpb[13]);
    let bps = u64::from(layout.bps);
    BASE + (reserved + 2 * fatsz) * bps + n * spc * bps
}

/// The offset of the live entry named `name` in a directory's first 512
/// bytes.
fn entry_named(disk: &Disk, sector: u64, name: &[u8; 11]) -> u64 {
    (0..16u64)
        .map(|i| sector + i * 32)
        .find(|&at| disk.peek(at, 11) == name)
        .unwrap_or_else(|| panic!("no {:?}", String::from_utf8_lossy(name)))
}

#[test]
fn a_volume_bit_on_efi_or_boot_is_damage_and_nothing_is_written() {
    let old = old();
    for (dir, name) in [(0, b"EFI        "), (1, b"BOOT       ")] {
        for attr in [0x18u8, 0x38, 0x19, 0x1f] {
            let mut disk = image(&SMALL, &old);
            let at = entry_named(&disk, dir_sector_in(&disk, &SMALL, dir), name) + 11;
            disk.poke(at, &[attr]);
            let before = disk.digest(SMALL.volume_len());
            match open(disk.clone(), &SMALL) {
                Err(Error::Damaged(_)) => {}
                Err(other) => panic!("{name:?} {attr:#x}: {other}"),
                Ok(mut esp) => {
                    // What the bug did: the live pair looked unreachable,
                    // so staging reclaimed and overwrote it.
                    let _ = stage(&mut esp, &new());
                    panic!("{name:?} {attr:#x} admitted");
                }
            }
            assert_eq!(disk.digest(SMALL.volume_len()), before);
        }
    }
    // A volume label that owns a cluster hides a chain from the walk.
    let mut disk = image(&SMALL, &old);
    let root = dir_sector_in(&disk, &SMALL, 0);
    let label = (0..16u64)
        .map(|i| root + i * 32)
        .find(|&at| disk.peek(at + 11, 1)[0] & 0x08 != 0)
        .unwrap();
    disk.poke(label + 26, &[3, 0]);
    assert!(matches!(open(disk, &SMALL), Err(Error::Damaged(_))));
}

#[test]
fn a_commit_takes_only_the_directory_this_esp_read() {
    let (old, new) = (old(), new());
    let mut esp = open(image(&SMALL, &old), &SMALL).unwrap();
    stage(&mut esp, &new).unwrap();
    let disk = esp.into_device().after_cut(false);
    let read = open(disk.clone(), &SMALL).unwrap().boot().clone();
    let mut moved = read.clone();
    let mut staged = moved.staged.unwrap();
    staged.kernel.first_cluster += 1;
    moved.staged = Some(staged);
    let mut resized = read.clone();
    resized.pair.initrd.size -= 1;
    for forged in [moved, resized] {
        let mut esp = open(disk.clone(), &SMALL).unwrap();
        assert!(matches!(
            esp.commit_staged(&forged),
            Err(CommitError::Before(Error::Changed))
        ));
        assert!(matches!(esp.delete_staged(&forged), Err(Error::Changed)));
        assert_eq!(esp.into_device().writes, 0);
    }
    let mut esp = open(disk, &SMALL).unwrap();
    esp.commit_staged(&read).unwrap();
}

const BIG_SECTORS: Layout = Layout {
    bps: 4096,
    total: 66_000,
    spc: Some(1),
};

#[test]
fn a_4096_byte_sector_commit_cut_inside_its_sector_leaves_one_pair_or_the_other() {
    let (old, new) = (old(), new());
    let make = || {
        let mut esp = open(image(&BIG_SECTORS, &old), &BIG_SECTORS).unwrap();
        stage(&mut esp, &new).unwrap();
        esp.into_device().after_cut(false)
    };
    cut_everywhere(
        &BIG_SECTORS,
        &make,
        &commit,
        &[
            Found::Staged(old.clone(), new.clone()),
            Found::Pair(new.clone()),
        ],
        &Found::Pair(new.clone()),
    );
}

#[test]
fn a_boot_file_past_the_first_512_bytes_is_refused() {
    let old = old();
    let mut disk = image(&BIG_SECTORS, &old);
    let boot = dir_sector_in(&disk, &BIG_SECTORS, 2);
    let kernel = entry_named(&disk, boot, b"BOOTX64 EFI");
    let entry = disk.peek(kernel, 32);
    // The same entry moved to index 16 of the 4096-byte sector: legal FAT,
    // but a commit's change to it would not share a unit with the rest.
    let mut gap = vec![0xe5u8];
    gap.extend_from_slice(&entry[1..]);
    disk.poke(kernel, &gap);
    for i in 4..16u64 {
        if disk.peek(boot + i * 32, 1)[0] == 0 {
            disk.poke(boot + i * 32, &[0xe5]);
        }
    }
    disk.poke(boot + 16 * 32, &entry);
    match open(disk, &BIG_SECTORS) {
        Err(Error::Foreign(_)) => {}
        other => panic!("{:?}", other.err()),
    }
}

//! Aggregate network and block activity with explicit continuity and topology.
use crate::budget::{Budget, Charge, MemoryString, MemoryVec};
use crate::linux_read::Reader;
use crate::parsers::{self, AGGREGATE_BYTES};
use std::fmt::Write as _;
use std::io;
use std::os::unix::{ffi::OsStrExt, fs::MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
const INTERFACES: usize = 256;
const DISKS: usize = 1024;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Name {
    bytes: [u8; 128],
    length: usize,
}
impl Name {
    fn new(bytes: &[u8]) -> Option<Self> {
        if bytes.is_empty()
            || bytes.len() > 128
            || bytes == b"."
            || bytes == b".."
            || bytes.iter().any(|b| matches!(b, b'/' | 0))
        {
            return None;
        }
        let mut name = Self {
            bytes: [0; 128],
            length: bytes.len(),
        };
        name.bytes.get_mut(..bytes.len())?.copy_from_slice(bytes);
        Some(name)
    }
    fn bytes(&self) -> &[u8] {
        self.bytes.get(..self.length).unwrap_or_default()
    }
    fn path(self, path: &mut PathBuf, base: &Path) {
        path.clear();
        path.push(base);
        path.push(std::ffi::OsStr::from_bytes(self.bytes()));
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Object {
    pub device: u64,
    pub inode: u64,
}
fn object(path: &Path) -> Option<Object> {
    let meta = std::fs::metadata(path).ok()?;
    Some(Object {
        device: meta.dev(),
        inode: meta.ino(),
    })
}
#[derive(Debug)]
pub struct Network {
    pub name: MemoryString,
    pub ifindex: Option<u32>,
    pub up: bool,
    pub device_link: bool,
    pub received: u64,
    pub sent: u64,
    pub receive_rate: Option<u64>,
    pub send_rate: Option<u64>,
    pub loopback: bool,
}
impl Network {
    fn preference(&self) -> u8 {
        if self.loopback {
            3
        } else if self.up && self.device_link {
            0
        } else if self.up {
            1
        } else {
            2
        }
    }
}
#[derive(Debug)]
pub struct Disk {
    pub name: MemoryString,
    pub major: u32,
    pub minor: u32,
    pub whole: Option<bool>,
    pub device_link: bool,
    pub read_bytes: Option<u64>,
    pub written_bytes: Option<u64>,
    pub read_rate: Option<u64>,
    pub write_rate: Option<u64>,
    pub read_iops: Option<u64>,
    pub write_iops: Option<u64>,
    pub busy: Option<u64>,
    excluded_default: bool,
    logical: bool,
}
impl Disk {
    fn preference(&self) -> u8 {
        if self.excluded_default {
            3
        } else if self.whole != Some(true) {
            2
        } else if self.device_link && !self.logical {
            0
        } else {
            1
        }
    }
}
pub fn default_network(networks: &[Network]) -> Option<usize> {
    networks
        .iter()
        .enumerate()
        .min_by(|(_, a), (_, b)| {
            (a.preference(), a.name.as_str()).cmp(&(b.preference(), b.name.as_str()))
        })
        .map(|(index, _)| index)
}
pub fn default_disk(disks: &[Disk]) -> Option<usize> {
    disks
        .iter()
        .enumerate()
        .min_by_key(|(_, disk)| (disk.preference(), disk.major, disk.minor))
        .map(|(index, _)| index)
}
#[derive(Clone, Copy, Debug)]
struct NetRaw {
    name: Name,
    received: u64,
    sent: u64,
}
#[derive(Clone, Copy, Debug)]
struct NetOld {
    ifindex: u32,
    object: Object,
    namespace: Object,
    received: u64,
    sent: u64,
    time: u64,
}
#[derive(Clone, Copy, Debug)]
struct DiskRaw {
    name: Name,
    major: u32,
    minor: u32,
    reads: u64,
    read_sectors: u64,
    writes: u64,
    written_sectors: u64,
    busy_ms: u64,
}
#[derive(Clone, Copy, Debug)]
struct DiskOld {
    raw: DiskRaw,
    object: Object,
    time: u64,
}
fn network_rates(next: NetOld, old: Option<&NetOld>) -> (Option<u64>, Option<u64>) {
    let old = old.filter(|old| {
        old.ifindex == next.ifindex && old.object == next.object && old.namespace == next.namespace
    });
    let receive = old.and_then(|old| {
        parsers::rate(
            next.received,
            old.received,
            1,
            next.time.checked_sub(old.time)?,
        )
    });
    let send =
        old.and_then(|old| parsers::rate(next.sent, old.sent, 1, next.time.checked_sub(old.time)?));
    (receive, send)
}
fn disk_previous(next: DiskOld, old: Option<&DiskOld>) -> Option<(DiskRaw, u64)> {
    let old = old.filter(|old| {
        old.raw.major == next.raw.major
            && old.raw.minor == next.raw.minor
            && old.raw.name == next.raw.name
            && old.object == next.object
    })?;
    Some((
        old.raw,
        next.time
            .checked_sub(old.time)
            .filter(|elapsed| *elapsed > 0)?,
    ))
}
#[derive(Debug)]
pub struct Devices {
    budget: Arc<Budget>,
    old_net: MemoryVec<NetOld>,
    next_net: MemoryVec<NetOld>,
    old_disk: MemoryVec<DiskOld>,
    next_disk: MemoryVec<DiskOld>,
    net_raw: MemoryVec<NetRaw>,
    disk_raw: MemoryVec<DiskRaw>,
    text: String,
    path: PathBuf,
    field: PathBuf,
    _charge: Charge,
}
fn field_path<'a>(scratch: &'a mut PathBuf, base: &Path, field: &str) -> &'a Path {
    scratch.clear();
    scratch.push(base);
    scratch.push(field);
    scratch.as_path()
}
fn read_number(reader: &mut Reader, path: &Path) -> Option<u32> {
    reader
        .path(path, 64)
        .ok()
        .and_then(|bytes| parsers::unsigned(bytes.trim_ascii()))
        .and_then(|n| u32::try_from(n).ok())
}
#[derive(Debug)]
pub struct Sample {
    pub networks: MemoryVec<Network>,
    pub disks: MemoryVec<Disk>,
    pub omitted_networks: usize,
    pub omitted_disks: usize,
    pub network_unavailable: bool,
    pub disk_unavailable: bool,
    pub network_namespace: Option<Object>,
}
impl Devices {
    pub fn new(budget: &Arc<Budget>) -> io::Result<Self> {
        let mut charge = budget
            .charge(std::mem::size_of::<Self>() + 3 * 512)
            .map_err(io::Error::other)?;
        let mut text = String::new();
        text.try_reserve_exact(512).map_err(io::Error::other)?;
        let mut path = PathBuf::new();
        let mut field = PathBuf::new();
        path.try_reserve_exact(512).map_err(io::Error::other)?;
        field.try_reserve_exact(512).map_err(io::Error::other)?;
        charge
            .additional(
                (text.capacity() + path.capacity() + field.capacity()).saturating_sub(3 * 512),
            )
            .map_err(io::Error::other)?;
        Ok(Self {
            budget: Arc::clone(budget),
            old_net: MemoryVec::new(budget, INTERFACES).map_err(io::Error::other)?,
            next_net: MemoryVec::new(budget, INTERFACES).map_err(io::Error::other)?,
            old_disk: MemoryVec::new(budget, DISKS).map_err(io::Error::other)?,
            next_disk: MemoryVec::new(budget, DISKS).map_err(io::Error::other)?,
            net_raw: MemoryVec::new(budget, INTERFACES).map_err(io::Error::other)?,
            disk_raw: MemoryVec::new(budget, DISKS).map_err(io::Error::other)?,
            text,
            path,
            field,
            _charge: charge,
        })
    }
    pub fn sample(
        &mut self,
        reader: &mut Reader,
        mut now: impl FnMut() -> io::Result<u64>,
        mut check_cancel: impl FnMut() -> io::Result<()>,
    ) -> io::Result<Sample> {
        self.net_raw.clear();
        self.disk_raw.clear();
        self.next_net.clear();
        self.next_disk.clear();
        let mut sample = Sample {
            networks: MemoryVec::new(&self.budget, 0).map_err(io::Error::other)?,
            disks: MemoryVec::new(&self.budget, 0).map_err(io::Error::other)?,
            omitted_networks: 0,
            omitted_disks: 0,
            network_unavailable: false,
            disk_unavailable: false,
            network_namespace: object(Path::new("/proc/self/ns/net")),
        };
        let net_time = now()?;
        match reader.path(Path::new("/proc/net/dev"), AGGREGATE_BYTES) {
            Ok(bytes) => {
                for line in bytes
                    .split(|b| *b == b'\n')
                    .skip(2)
                    .filter(|line| !line.trim_ascii().is_empty())
                {
                    let value = parsers::network(line).ok().and_then(|net| {
                        Some(NetRaw {
                            name: Name::new(net.name)?,
                            received: net.received,
                            sent: net.sent,
                        })
                    });
                    if value.is_none_or(|raw| self.net_raw.push(raw).is_err()) {
                        sample.omitted_networks += 1;
                    }
                }
            }
            Err(_) => sample.network_unavailable = true,
        }
        sample
            .networks
            .reserve(self.net_raw.len())
            .map_err(io::Error::other)?;
        for raw in self.net_raw.iter().copied() {
            check_cancel()?;
            raw.name.path(&mut self.path, Path::new("/sys/class/net"));
            let path = &self.path;
            let ifindex = read_number(reader, field_path(&mut self.field, path, "ifindex"))
                .filter(|n| *n > 0);
            let identity = object(path);
            let up = reader
                .path(field_path(&mut self.field, path, "operstate"), 64)
                .is_ok_and(|bytes| bytes.trim_ascii() == b"up");
            let device_link =
                std::fs::symlink_metadata(field_path(&mut self.field, path, "device")).is_ok();
            let loopback = reader
                .path(field_path(&mut self.field, path, "flags"), 64)
                .ok()
                .and_then(|bytes| std::str::from_utf8(bytes.trim_ascii()).ok())
                .and_then(|text| u32::from_str_radix(text.strip_prefix("0x")?, 16).ok())
                .map(|flags| flags & 8 != 0)
                .unwrap_or(raw.name.bytes() == b"lo");
            let mut receive_rate = None;
            let mut send_rate = None;
            if let Some(((ifindex, identity), namespace)) =
                ifindex.zip(identity).zip(sample.network_namespace)
            {
                let next = NetOld {
                    ifindex,
                    object: identity,
                    namespace,
                    received: raw.received,
                    sent: raw.sent,
                    time: net_time,
                };
                (receive_rate, send_rate) =
                    network_rates(next, self.old_net.iter().find(|old| old.ifindex == ifindex));
                let _ = self.next_net.push(next);
            }
            parsers::escaped_text(raw.name.bytes(), &mut self.text, 512)
                .map_err(io::Error::other)?;
            sample
                .networks
                .push(Network {
                    name: MemoryString::new(&self.budget, &self.text).map_err(io::Error::other)?,
                    ifindex,
                    up,
                    device_link,
                    received: raw.received,
                    sent: raw.sent,
                    receive_rate,
                    send_rate,
                    loopback,
                })
                .map_err(|_| io::Error::other("network sample limit"))?;
        }
        let disk_time = now()?;
        match reader.path(Path::new("/proc/diskstats"), AGGREGATE_BYTES) {
            Ok(bytes) => {
                for line in bytes
                    .split(|b| *b == b'\n')
                    .filter(|line| !line.trim_ascii().is_empty())
                {
                    let value = parsers::disk(line).ok().and_then(|disk| {
                        Some(DiskRaw {
                            name: Name::new(disk.name)?,
                            major: disk.major,
                            minor: disk.minor,
                            reads: disk.reads,
                            read_sectors: disk.read_sectors,
                            writes: disk.writes,
                            written_sectors: disk.written_sectors,
                            busy_ms: disk.busy_ms,
                        })
                    });
                    if value.is_none_or(|raw| self.disk_raw.push(raw).is_err()) {
                        sample.omitted_disks += 1;
                    }
                }
            }
            Err(_) => sample.disk_unavailable = true,
        }
        sample
            .disks
            .reserve(self.disk_raw.len())
            .map_err(io::Error::other)?;
        for raw in self.disk_raw.iter().copied() {
            check_cancel()?;
            self.text.clear();
            write!(&mut self.text, "/sys/dev/block/{}:{}", raw.major, raw.minor)
                .map_err(io::Error::other)?;
            self.path.clear();
            self.path.push(&self.text);
            let path = &self.path;
            let identity = object(path);
            let whole = if identity.is_some() {
                match std::fs::metadata(field_path(&mut self.field, path, "partition")) {
                    Ok(_) => Some(false),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => Some(true),
                    Err(_) => None,
                }
            } else {
                None
            };
            let device_link =
                std::fs::symlink_metadata(field_path(&mut self.field, path, "device")).is_ok();
            let previous = identity.and_then(|object| {
                disk_previous(
                    DiskOld {
                        raw,
                        object,
                        time: disk_time,
                    },
                    self.old_disk
                        .iter()
                        .find(|old| old.raw.major == raw.major && old.raw.minor == raw.minor),
                )
            });
            let delta = |next, select: fn(DiskRaw) -> u64, multiplier| {
                previous.and_then(|(old, elapsed)| {
                    parsers::rate(next, select(old), multiplier, elapsed)
                })
            };
            let read_rate = delta(raw.read_sectors, |old| old.read_sectors, 512);
            let write_rate = delta(raw.written_sectors, |old| old.written_sectors, 512);
            let read_iops = delta(raw.reads, |old| old.reads, 1);
            let write_iops = delta(raw.writes, |old| old.writes, 1);
            let busy = delta(raw.busy_ms, |old| old.busy_ms, 10).map(|n| n.min(10000));
            if let Some(identity) = identity {
                let _ = self.next_disk.push(DiskOld {
                    raw,
                    object: identity,
                    time: disk_time,
                });
            }
            parsers::escaped_text(raw.name.bytes(), &mut self.text, 512)
                .map_err(io::Error::other)?;
            sample
                .disks
                .push(Disk {
                    name: MemoryString::new(&self.budget, &self.text).map_err(io::Error::other)?,
                    major: raw.major,
                    minor: raw.minor,
                    whole,
                    device_link,
                    read_bytes: raw.read_sectors.checked_mul(512),
                    written_bytes: raw.written_sectors.checked_mul(512),
                    read_rate,
                    write_rate,
                    read_iops,
                    write_iops,
                    busy,
                    logical: raw.name.bytes().starts_with(b"dm-")
                        || raw.name.bytes().starts_with(b"md"),
                    excluded_default: [b"loop".as_slice(), b"ram", b"zram"]
                        .iter()
                        .any(|prefix| raw.name.bytes().starts_with(prefix)),
                })
                .map_err(|_| io::Error::other("disk sample limit"))?;
        }
        std::mem::swap(&mut self.old_net, &mut self.next_net);
        self.next_net.clear();
        std::mem::swap(&mut self.old_disk, &mut self.next_disk);
        self.next_disk.clear();
        Ok(sample)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
    use super::*;
    fn network(
        budget: &Arc<Budget>,
        name: &str,
        up: bool,
        device_link: bool,
        loopback: bool,
    ) -> Network {
        Network {
            name: MemoryString::new(budget, name).unwrap(),
            ifindex: Some(1),
            up,
            device_link,
            received: 0,
            sent: 0,
            receive_rate: None,
            send_rate: None,
            loopback,
        }
    }
    #[allow(clippy::too_many_arguments)]
    fn disk(
        budget: &Arc<Budget>,
        name: &str,
        major: u32,
        minor: u32,
        whole: Option<bool>,
        device_link: bool,
        logical: bool,
        excluded_default: bool,
    ) -> Disk {
        Disk {
            name: MemoryString::new(budget, name).unwrap(),
            major,
            minor,
            whole,
            device_link,
            read_bytes: Some(0),
            written_bytes: Some(0),
            read_rate: None,
            write_rate: None,
            read_iops: None,
            write_iops: None,
            busy: None,
            logical,
            excluded_default,
        }
    }
    #[test]
    fn defaults_prefer_up_physical_network_and_whole_physical_disk() {
        let budget = Budget::new(crate::budget::LIMIT).unwrap();
        let networks = [
            network(&budget, "lo", true, false, true),
            network(&budget, "veth", true, false, false),
            network(&budget, "eth1", true, true, false),
            network(&budget, "eth0", true, true, false),
        ];
        assert_eq!(default_network(&networks), Some(3));
        assert_eq!(default_network(&networks[..2]), Some(1));
        let disks = [
            disk(&budget, "loop0", 7, 0, Some(true), true, false, true),
            disk(&budget, "dm-0", 8, 0, Some(true), true, true, false),
            disk(&budget, "sda1", 8, 1, Some(false), true, false, false),
            disk(&budget, "nvme0n1", 259, 0, Some(true), true, false, false),
        ];
        assert_eq!(default_disk(&disks), Some(3));
        assert_eq!(default_disk(&disks[..3]), Some(1));
        assert_eq!(default_disk(&[]), None);
    }
    #[test]
    fn unknown_topology_still_prefers_non_memory_devices() {
        let budget = Budget::new(crate::budget::LIMIT).unwrap();
        let disks = [
            disk(&budget, "ram0", 1, 0, None, false, false, true),
            disk(&budget, "loop0", 7, 0, None, false, false, true),
            disk(&budget, "nvme0n1", 259, 0, None, false, false, false),
        ];
        assert_eq!(default_disk(&disks), Some(2));
        assert_eq!(default_disk(&disks[..2]), Some(0));
    }
    #[test]
    fn component_names_cannot_traverse_sysfs_and_actual_device_samples_are_bounded() {
        assert!(Name::new(b"../other").is_none());
        assert!(Name::new(b".").is_none());
        assert!(Name::new(b"x\0y").is_none());
        let budget = Budget::new(crate::budget::LIMIT).unwrap();
        let mut devices = Devices::new(&budget).unwrap();
        let mut reader = Reader::new(&budget, AGGREGATE_BYTES).unwrap();
        let origin = std::time::Instant::now();
        let sample = devices
            .sample(
                &mut reader,
                || Ok(origin.elapsed().as_nanos() as u64),
                || Ok(()),
            )
            .unwrap();
        assert!(sample.networks.len() <= INTERFACES);
        assert!(sample.disks.len() <= DISKS);
        assert_eq!(sample.networks.capacity(), sample.networks.len());
        assert_eq!(sample.disks.capacity(), sample.disks.len());
        assert!(!sample.network_unavailable);
        assert!(sample
            .networks
            .iter()
            .all(|net| net.receive_rate.is_none() && net.send_rate.is_none()));
        assert!(sample
            .disks
            .iter()
            .all(|disk| disk.read_rate.is_none() && disk.write_rate.is_none()));
        assert!(budget.peak() <= budget.maximum());
    }
    #[test]
    fn counter_reset_replacement_namespace_change_and_reappearance_are_gaps() {
        let object = Object {
            device: 1,
            inode: 2,
        };
        let old = NetOld {
            ifindex: 2,
            object,
            namespace: object,
            received: 100,
            sent: 200,
            time: 0,
        };
        let next = NetOld {
            received: 200,
            sent: 400,
            time: 1_000_000_000,
            ..old
        };
        assert_eq!(network_rates(next, Some(&old)), (Some(100), Some(200)));
        assert_eq!(network_rates(next, None), (None, None));
        for replacement in [
            NetOld { ifindex: 3, ..next },
            NetOld {
                object: Object { inode: 3, ..object },
                ..next
            },
            NetOld {
                namespace: Object { inode: 4, ..object },
                ..next
            },
            NetOld { time: 0, ..next },
        ] {
            assert_eq!(network_rates(replacement, Some(&old)), (None, None));
        }
        assert_eq!(
            network_rates(
                NetOld {
                    received: 1,
                    ..next
                },
                Some(&old)
            )
            .0,
            None
        );
        let raw = DiskRaw {
            name: Name::new(b"sda").unwrap(),
            major: 8,
            minor: 0,
            reads: 1,
            read_sectors: 2,
            writes: 3,
            written_sectors: 4,
            busy_ms: 5,
        };
        let old = DiskOld {
            raw,
            object,
            time: 0,
        };
        let next = DiskOld {
            time: 1_000_000_000,
            ..old
        };
        let (_, elapsed) = disk_previous(next, Some(&old)).unwrap();
        assert_eq!(elapsed, 1_000_000_000);
        assert!(disk_previous(next, None).is_none());
        assert!(disk_previous(
            DiskOld {
                object: Object { inode: 9, ..object },
                ..next
            },
            Some(&old)
        )
        .is_none());
        assert!(disk_previous(
            DiskOld {
                raw: DiskRaw {
                    name: Name::new(b"sdb").unwrap(),
                    ..raw
                },
                ..next
            },
            Some(&old)
        )
        .is_none());
    }
}

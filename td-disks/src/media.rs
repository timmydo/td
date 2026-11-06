//! Removable media: USB mass storage and SD/MMC cards, mounted for the
//! session owner while the session is unlocked (DESIGN.md, "Removable
//! media").
//!
//! Two halves. `serve` runs as root and owns every mount; it answers one
//! request per connection on a socket only the owner may open. `watch`
//! runs as the owner inside the session, asks the compositor whether the
//! session is locked, and asks `serve` to mount what appears. `serve` reads
//! no medium's bytes itself: `identify_stdin` does, as a worker of its own
//! holding only the device root opened.

use crate::probe;
use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::fs::{DirBuilderExt, FileExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const NOFOLLOW_NONBLOCK: i32 = 0o400000 | 0o4000;
const MAX_REQUEST: u64 = 256;
const MAX_REPLY: u64 = 64 << 10;
const MAX_MOUNTS: u64 = 1 << 20;
const MAX_LABEL: usize = 32;
const MAX_ENTRIES: usize = 4096;
const IO_TIMEOUT: Duration = Duration::from_secs(5);
/// A whole request, however it trickles in.
const REQUEST_DEADLINE: Duration = Duration::from_secs(5);
/// The identify worker's reads of a device that may stall.
const IDENTIFY_DEADLINE: Duration = Duration::from_secs(15);
/// The identify worker's whole reply: a word and a mapped label.
const MAX_IDENTITY: u64 = 256;
/// What the identify worker runs as. No account names it and nothing else
/// runs as it: td-time's network worker is 65534, and a shared uid could
/// signal or trace it. `system_def_is_self_consistent` names this value too.
const IDENTIFY_UID: u32 = 65532;
/// The worker's verb, shared by the spawner and `main`'s router.
pub const IDENTIFY_VERB: &str = "media-identify";
/// `/bin/mount` and `/bin/umount`, journal replay included.
const CHILD_DEADLINE: Duration = Duration::from_secs(60);
/// How often root opens each USB disk so a card reader notices a card.
const REVALIDATE_INTERVAL: Duration = Duration::from_secs(2);
const TICK: Duration = Duration::from_millis(250);
const WATCH_INTERVAL: Duration = Duration::from_secs(1);
const ISO_SECTOR: u64 = 2048;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Media {
    Vfat,
    Exfat,
    Ext4,
    Iso9660,
    Udf,
}

impl Media {
    pub fn word(self) -> &'static str {
        match self {
            Self::Vfat => "vfat",
            Self::Exfat => "exfat",
            Self::Ext4 => "ext4",
            Self::Iso9660 => "iso9660",
            Self::Udf => "udf",
        }
    }

    fn from_word(word: &str) -> Option<Self> {
        [
            Self::Vfat,
            Self::Exfat,
            Self::Ext4,
            Self::Iso9660,
            Self::Udf,
        ]
        .into_iter()
        .find(|media| media.word() == word)
    }

    /// The `-o` words: never devices, setuid or programs; the owner's
    /// files where the filesystem can say so.
    pub fn options(self, uid: u32, gid: u32, read_only: bool) -> String {
        let mut words = vec!["nodev", "nosuid", "noexec"];
        // The optical formats are read-only, UDF also so its write paths
        // stay unreached from a stick.
        if read_only || matches!(self, Self::Iso9660 | Self::Udf) {
            words.push("ro");
        }
        let owner = format!("uid={uid},gid={gid}");
        let mut text = words.join(",");
        // Mount options apply after a superblock's own, so a stick cannot
        // choose `errors=panic`.
        match self {
            Self::Vfat | Self::Exfat => {
                text.push(',');
                text.push_str(&owner);
                text.push_str(",umask=077,errors=remount-ro");
            }
            Self::Iso9660 | Self::Udf => {
                text.push(',');
                text.push_str(&owner);
            }
            // ext4 keeps the owners its inodes record.
            Self::Ext4 => text.push_str(",errors=remount-ro"),
        }
        text
    }
}

fn trimmed(bytes: &[u8]) -> Option<String> {
    let end = bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bytes.len());
    let text = String::from_utf8_lossy(bytes.get(..end)?)
        .trim()
        .to_string();
    (!text.is_empty()).then_some(text)
}

/// FAT12/16/32 from its boot sector, with the BPB's volume label.
pub fn fat(sector: &[u8]) -> Option<Option<String>> {
    if sector.get(510..512)? != [0x55, 0xaa] {
        return None;
    }
    let per_sector = u16::from_le_bytes(sector.get(11..13)?.try_into().ok()?);
    if !matches!(per_sector, 512 | 1024 | 2048 | 4096) || *sector.get(13)? == 0 {
        return None;
    }
    let label = if sector.get(82..90)? == b"FAT32   " {
        sector.get(71..82)?
    } else if matches!(sector.get(54..62)?, b"FAT12   " | b"FAT16   " | b"FAT     ") {
        sector.get(43..54)?
    } else {
        return None;
    };
    Some(trimmed(label).filter(|label| label != "NO NAME"))
}

/// exFAT from its boot sector; its label lives in a directory entry, so
/// none is read.
pub fn exfat(sector: &[u8]) -> bool {
    sector.get(3..11) == Some(b"EXFAT   ".as_slice()) && sector.get(510..512) == Some(&[0x55, 0xaa])
}

/// ISO 9660's primary volume descriptor (sector 16) and its volume id.
pub fn iso9660(descriptor: &[u8]) -> Option<Option<String>> {
    if descriptor.first() != Some(&1) || descriptor.get(1..6)? != b"CD001" {
        return None;
    }
    Some(trimmed(descriptor.get(40..72)?))
}

/// UDF's volume recognition sequence from 32 KiB: BEA01, then NSR02 or
/// NSR03, its descriptors `spacing` apart (the larger of 2048 and the
/// block size).
pub fn udf(sectors: &[u8], spacing: usize) -> bool {
    let mut begun = false;
    for sector in sectors.chunks(spacing) {
        match sector.get(1..6) {
            Some(b"BEA01") => begun = true,
            Some(b"NSR02") | Some(b"NSR03") if begun => return true,
            Some(b"CD001") | Some(b"BOOT2") | Some(b"CDW02") | Some(b"TEA01") => {}
            _ => return false,
        }
    }
    false
}

fn le32(block: &[u8], at: usize) -> u32 {
    block
        .get(at..at + 4)
        .and_then(|bytes| bytes.try_into().ok())
        .map_or(0, u32::from_le_bytes)
}

/// Why an ext4 superblock is not mounted from a stick: one that names an
/// external journal device makes the kernel open a device the stick
/// chose, and one carrying its own mount options chooses them.
pub fn ext4_refusal(block: &[u8]) -> Option<&'static str> {
    const HAS_JOURNAL: u32 = 0x4;
    let journaled = le32(block, 0x5c) & HAS_JOURNAL != 0;
    if journaled && (le32(block, 0xe0) == 0 || le32(block, 0xe4) != 0) {
        return Some("names an external journal");
    }
    if block.get(0x200).is_some_and(|byte| *byte != 0) {
        return Some("carries its own mount options");
    }
    None
}

/// What one opened device holds, and its label.
pub fn identify(device: &File) -> io::Result<Option<(Media, Option<String>)>> {
    let mut volume = vec![0u8; ISO_SECTOR as usize * 16];
    if device.read_exact_at(&mut volume, 16 * ISO_SECTOR).is_ok() {
        if udf(&volume, 2048) || udf(&volume, 4096) {
            return Ok(Some((Media::Udf, None)));
        }
        if let Some(label) = volume.get(..ISO_SECTOR as usize).and_then(iso9660) {
            return Ok(Some((Media::Iso9660, label)));
        }
    }
    let mut boot = vec![0u8; 512];
    if device.read_exact_at(&mut boot, 0).is_ok() {
        if exfat(&boot) {
            return Ok(Some((Media::Exfat, None)));
        }
        if let Some(label) = fat(&boot) {
            return Ok(Some((Media::Vfat, label)));
        }
    }
    let mut ext = vec![0u8; 1024];
    if device.read_exact_at(&mut ext, 1024).is_ok() && probe::ext4(&ext).is_some() {
        if let Some(why) = ext4_refusal(&ext) {
            return Err(io::Error::other(format!("its ext4 superblock {why}")));
        }
        return Ok(Some((Media::Ext4, ext.get(120..136).and_then(trimmed))));
    }
    Ok(None)
}

/// What a worker found, as the one line root reads: `none`, `ok WORD`,
/// `ok WORD LABEL` with the label already a mount-point name, or
/// `error WHY`.
pub fn identity_line(found: &io::Result<Option<(Media, Option<String>)>>) -> String {
    match found {
        Ok(None) => "none\n".into(),
        Ok(Some((media, label))) => match label.as_deref().and_then(label_name) {
            Some(label) => format!("ok {} {label}\n", media.word()),
            None => format!("ok {}\n", media.word()),
        },
        Err(error) => format!("error {}\n", printable(&error.to_string())),
    }
}

/// Root's reading of that line. The bytes came from a medium's parser, so
/// nothing is taken unchecked: an unknown word or a second line is a
/// refusal, a reason is reduced to printable ASCII before it reaches a
/// reply, and the caller still maps a label through `label_name`.
pub fn parse_identity(reply: &str) -> Result<Option<(Media, Option<String>)>, String> {
    let line = reply
        .strip_suffix('\n')
        .filter(|line| !line.contains('\n'))
        .ok_or("the identify worker's reply is not one line")?;
    if line == "none" {
        return Ok(None);
    }
    if let Some(why) = line.strip_prefix("error ") {
        return Err(printable(why));
    }
    let found = line
        .strip_prefix("ok ")
        .ok_or("the identify worker's reply is malformed")?;
    let (word, label) = found
        .split_once(' ')
        .map_or((found, None), |(word, label)| (word, Some(label)));
    let media =
        Media::from_word(word).ok_or("the identify worker named no filesystem td mounts")?;
    Ok(Some((media, label.map(str::to_string))))
}

fn printable(text: &str) -> String {
    text.chars()
        .filter(|c| c.is_ascii_graphic() || *c == ' ')
        .take(MAX_LABEL * 4)
        .collect()
}

/// Whether `/proc/self/status` shows a process with no root uid or gid in any
/// column and no permitted, effective or ambient capability. The worker asks
/// before it reads a byte, so a launch that kept privilege refuses rather than
/// parses as root.
pub fn unprivileged(status: &str) -> bool {
    let field = |name: &str| {
        status
            .lines()
            .find_map(|line| line.strip_prefix(name)?.strip_prefix(':'))
    };
    let clear = |name: &str| {
        field(name).is_some_and(|ids| {
            let columns: Vec<&str> = ids.split_whitespace().collect();
            columns.len() == 4 && columns.iter().all(|id| *id != "0")
        })
    };
    clear("Uid")
        && clear("Gid")
        && ["CapPrm", "CapEff", "CapAmb"].into_iter().all(|name| {
            field(name).is_some_and(|set| set.trim().bytes().all(|digit| digit == b'0'))
        })
}

/// The `media-identify` worker: identify the device on stdin and print
/// `identity_line`. Root opened that device and validated it; this holds
/// nothing else of root's.
pub fn identify_stdin() -> Result<(), String> {
    let status = fs::read_to_string("/proc/self/status").map_err(|e| e.to_string())?;
    if !unprivileged(&status) {
        return Err(format!(
            "{IDENTIFY_VERB} refuses to read a medium with privilege"
        ));
    }
    let device = io::stdin()
        .as_fd()
        .try_clone_to_owned()
        .map(File::from)
        .map_err(|e| e.to_string())?;
    let line = identity_line(&identify(&device));
    io::stdout()
        .write_all(line.as_bytes())
        .map_err(|e| e.to_string())
}

/// SIGKILL waits out an uninterruptible read or mount(2), so the reaping is
/// another thread's, never the caller's.
fn abandon(mut child: Child) {
    let _ = child.kill();
    let _ = std::thread::Builder::new().spawn(move || {
        let _ = child.wait();
    });
}

/// A label as a mount-point name: `[A-Za-z0-9._-]`, at most 32, never
/// starting with `.` or `-`.
pub fn label_name(label: &str) -> Option<String> {
    let mapped: String = label
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let name: String = mapped
        .trim_start_matches(['.', '-'])
        .chars()
        .take(MAX_LABEL)
        .collect();
    (!name.is_empty() && name.chars().any(|c| c.is_ascii_alphanumeric())).then_some(name)
}

/// Block device names removable media take: `sd*` disks and partitions,
/// and `mmcblkN` with `pM` partitions.
pub fn media_name(name: &str) -> bool {
    if name.starts_with("sd") {
        return probe::supported_name(name);
    }
    let Some(rest) = name.strip_prefix("mmcblk") else {
        return false;
    };
    let (card, part) = rest
        .split_once('p')
        .map_or((rest, None), |(c, p)| (c, Some(p)));
    let number = |v: &str| !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit());
    number(card) && part.is_none_or(number)
}

/// The whole disk's sysfs directory for a disk or one of its partitions.
fn whole(entry: &Path) -> io::Result<PathBuf> {
    let real = fs::canonicalize(entry)?;
    if !real.join("partition").exists() {
        return Ok(real);
    }
    real.parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| io::Error::other("a partition without a disk"))
}

/// An SD card on an MMC host. Soldered eMMC reports `MMC` and is a
/// machine's own storage, never removable media.
fn on_mmc(entry: &Path) -> io::Result<bool> {
    let real = fs::canonicalize(entry)?;
    let on_host = real.components().any(|part| {
        part.as_os_str()
            .to_str()
            .is_some_and(|text| text.starts_with("mmc"))
    });
    Ok(on_host && sysfs_text(&whole(entry)?.join("device/type"))? == "SD")
}

fn sysfs_text(path: &Path) -> io::Result<String> {
    let mut text = String::new();
    File::open(path)?.take(129).read_to_string(&mut text)?;
    Ok(text.trim_end().to_string())
}

/// Whether `name` under `sys` is removable media that may hold a
/// filesystem now: on USB or MMC, not held, not a partitioned whole disk,
/// and with media in it (a card reader reports size 0 when empty).
pub fn candidate(sys: &Path, name: &str) -> bool {
    let entry = sys.join(name);
    media_name(name)
        && (probe::on_usb(&entry).unwrap_or(false) || on_mmc(&entry).unwrap_or(false))
        && fs::read_dir(entry.join("holders")).is_ok_and(|mut held| held.next().is_none())
        && !probe::partitioned(&entry, name).unwrap_or(true)
        && sysfs_text(&entry.join("size")).is_ok_and(|size| size != "0" && !size.is_empty())
}

/// Every candidate under `sys`, sorted.
pub fn candidates(sys: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(sys)
        .map(|entries| {
            entries
                .take(MAX_ENTRIES)
                .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
                .filter(|name| candidate(sys, name))
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

/// Where `serve` reads and mounts; tests point it at a tree.
pub struct Serve {
    pub socket: PathBuf,
    pub root: PathBuf,
    pub sys: PathBuf,
    pub dev: PathBuf,
    pub mounts: PathBuf,
    pub mount: PathBuf,
    pub umount: PathBuf,
    /// The program that serves `IDENTIFY_VERB`.
    pub worker: PathBuf,
    /// How long that program has, spawn to exit.
    pub identify_deadline: Duration,
    pub uid: u32,
    pub gid: u32,
    pub user: String,
}

pub const SOCKET: &str = "/run/td-disks/media";

/// The owner's name and primary group from `/etc/passwd`.
pub fn account(passwd: &str, uid: u32) -> Option<(String, u32)> {
    passwd.lines().find_map(|line| {
        let fields: Vec<&str> = line.split(':').collect();
        let name = fields.first()?;
        let parsed: u32 = fields.get(2)?.parse().ok()?;
        let gid: u32 = fields.get(3)?.parse().ok()?;
        (parsed == uid && label_name(name).as_deref() == Some(*name))
            .then(|| ((*name).to_string(), gid))
    })
}

/// Whether `gid` is `user`'s own group: named for it, with no other
/// member, so a 0750 directory in it admits only the owner.
pub fn private_group(group: &str, user: &str, gid: u32) -> bool {
    group.lines().any(|line| {
        let fields: Vec<&str> = line.split(':').collect();
        fields.len() == 4
            && fields.first() == Some(&user)
            && fields.get(2).and_then(|g| g.parse::<u32>().ok()) == Some(gid)
            && fields
                .get(3)
                .is_some_and(|members| members.split(',').all(|m| m.is_empty() || m == user))
    }) && !group.lines().any(|line| {
        let fields: Vec<&str> = line.split(':').collect();
        fields.first() != Some(&user)
            && fields.get(2).and_then(|g| g.parse::<u32>().ok()) == Some(gid)
    })
}

impl Serve {
    pub fn system(uid: u32) -> Result<Self, String> {
        let passwd = fs::read_to_string("/etc/passwd").map_err(|e| format!("/etc/passwd: {e}"))?;
        let (user, gid) = account(&passwd, uid).ok_or("the media owner has no account")?;
        let group = fs::read_to_string("/etc/group").map_err(|e| format!("/etc/group: {e}"))?;
        if !private_group(&group, &user, gid) {
            return Err("the media owner's primary group is not its own".into());
        }
        Ok(Self {
            socket: SOCKET.into(),
            root: PathBuf::from("/run/media").join(&user),
            sys: "/sys/class/block".into(),
            dev: "/dev".into(),
            mounts: "/proc/self/mounts".into(),
            mount: "/bin/mount".into(),
            umount: "/bin/umount".into(),
            worker: "/bin/td-disks".into(),
            identify_deadline: IDENTIFY_DEADLINE,
            uid,
            gid,
            user,
        })
    }

    fn mounts(&self) -> Result<Vec<(String, String)>, String> {
        let mut text = String::new();
        File::open(&self.mounts)
            .and_then(|file| file.take(MAX_MOUNTS + 1).read_to_string(&mut text))
            .map_err(|e| format!("{}: {e}", self.mounts.display()))?;
        if text.len() as u64 > MAX_MOUNTS {
            return Err("the mount table is too large to read whole".into());
        }
        let unescape = |field: &str| {
            field
                .replace("\\040", " ")
                .replace("\\011", "\t")
                .replace("\\012", "\n")
                .replace("\\134", "\\")
        };
        Ok(text
            .lines()
            .filter_map(|line| {
                let mut fields = line.split(' ');
                Some((unescape(fields.next()?), unescape(fields.next()?)))
            })
            .collect())
    }

    /// `(device name, mount-point name)` of each mount this owns.
    fn owned(&self) -> Result<Vec<(String, String)>, String> {
        Ok(self
            .mounts()?
            .into_iter()
            .filter_map(|(source, target)| {
                let leaf = Path::new(&target).strip_prefix(&self.root).ok()?;
                let leaf = leaf.to_str()?;
                if leaf.is_empty() || leaf.contains('/') {
                    return None;
                }
                let device = Path::new(&source).strip_prefix(&self.dev).ok()?.to_str()?;
                if device.contains('/') {
                    return None;
                }
                Some((device.to_string(), leaf.to_string()))
            })
            .collect())
    }

    /// Run `program`, killed past `CHILD_DEADLINE`, so a stalled device
    /// cannot hold every other medium's request.
    fn run(&self, program: &Path, args: &[&str]) -> Result<(), String> {
        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("{}: {e}", program.display()))?;
        let started = Instant::now();
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if started.elapsed() < CHILD_DEADLINE => {
                    std::thread::sleep(Duration::from_millis(50))
                }
                Ok(None) => {
                    abandon(child);
                    return Err(format!("{} timed out", program.display()));
                }
                Err(e) => return Err(format!("{}: {e}", program.display())),
            }
        };
        if status.success() {
            return Ok(());
        }
        let mut stderr = String::new();
        if let Some(pipe) = child.stderr.take() {
            let _ = pipe.take(4096).read_to_string(&mut stderr);
        }
        Err(stderr.trim_end().to_string())
    }

    /// What `device` holds, asked of a worker that has it as stdin and
    /// nothing else of root's. A hostile medium's bytes meet that parser, not
    /// this process; a stall or a crash there costs this request and not the
    /// daemon. Below root there is no identity to change to, which is how a
    /// test runs this.
    pub fn identify_confined(
        &self,
        device: File,
    ) -> Result<Option<(Media, Option<String>)>, String> {
        let (mut reader, writer) = UnixStream::pair().map_err(|e| e.to_string())?;
        reader.set_nonblocking(true).map_err(|e| e.to_string())?;
        let mut command = Command::new(&self.worker);
        command
            .arg(IDENTIFY_VERB)
            .env_clear()
            .current_dir("/")
            .stdin(device)
            .stdout(Stdio::from(OwnedFd::from(writer)))
            .stderr(Stdio::null());
        if fs::metadata("/proc/self").map_err(|e| e.to_string())?.uid() == 0 {
            command.uid(IDENTIFY_UID).gid(IDENTIFY_UID);
        }
        let mut child = command
            .spawn()
            .map_err(|e| format!("{IDENTIFY_VERB}: {e}"))?;
        // The command keeps its copy of the writer, which would hide EOF.
        drop(command);
        // Polled on this thread, so what survives a deadline is at most the one
        // reaper: a stalled worker, or a descendant holding the socket, pins
        // nothing else of root's.
        let started = Instant::now();
        let mut reply = Vec::new();
        let mut buffer = [0u8; MAX_IDENTITY as usize + 1];
        let mut ended = false;
        let status = loop {
            if !ended {
                match reader.read(&mut buffer) {
                    Ok(0) => ended = true,
                    Ok(read) => {
                        reply.extend_from_slice(buffer.get(..read).unwrap_or(&[]));
                        if reply.len() as u64 > MAX_IDENTITY {
                            abandon(child);
                            return Err(format!("{IDENTIFY_VERB} answered too much"));
                        }
                        continue;
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                    Err(error) => {
                        abandon(child);
                        return Err(format!("{IDENTIFY_VERB}: {error}"));
                    }
                }
            }
            if ended {
                match child.try_wait() {
                    Ok(Some(status)) => break status,
                    Ok(None) => {}
                    Err(error) => {
                        abandon(child);
                        return Err(format!("{IDENTIFY_VERB}: {error}"));
                    }
                }
            }
            if started.elapsed() >= self.identify_deadline {
                abandon(child);
                return Err(format!("{IDENTIFY_VERB} did not finish"));
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        if !status.success() {
            return Err(format!("{IDENTIFY_VERB} failed"));
        }
        parse_identity(&String::from_utf8_lossy(&reply))
    }

    /// `/run/media` root-owned 0755, and the owner's directory beneath it
    /// root-owned too, so no mount point can be swapped for a link, with
    /// the owner's private group at 0750, so no other account reads a
    /// disc's or an ext4 stick's files.
    fn prepare(&self) -> Result<(), String> {
        let parent = self.root.parent().unwrap_or(&self.root);
        for (dir, mode, gid) in [(parent, 0o755, 0), (self.root.as_path(), 0o750, self.gid)] {
            match fs::DirBuilder::new().mode(mode).create(dir) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(format!("{}: {e}", dir.display())),
            }
            let meta = fs::symlink_metadata(dir).map_err(|e| e.to_string())?;
            if !meta.is_dir() || meta.uid() != 0 {
                return Err(format!("untrusted media directory {}", dir.display()));
            }
            // Only a directory nobody else could ever write is repaired: one
            // that was writable may hold planted mount points.
            if meta.mode() & 0o022 != 0 {
                return Err(format!("untrusted media directory {}", dir.display()));
            }
            if meta.gid() != gid || meta.mode() & 0o7777 != mode {
                std::os::unix::fs::lchown(dir, None, Some(gid))
                    .and_then(|()| fs::set_permissions(dir, fs::Permissions::from_mode(mode)))
                    .map_err(|e| format!("{}: {e}", dir.display()))?;
            }
        }
        Ok(())
    }

    /// Whether the disk `disk` also carries something the system uses: a
    /// partition, or the disk itself, held (device-mapper, md) or mounted
    /// anywhere but beneath the owner's media directory. A td installed
    /// to a USB disk or an eMMC card keeps its EFI system partition, and
    /// its root, out of the owner's hands this way.
    fn system_disk(&self, disk: &str) -> Result<bool, String> {
        let entry = self.sys.join(disk);
        let mut members = vec![disk.to_string()];
        for child in fs::read_dir(&entry)
            .map_err(|e| format!("{disk}: {e}"))?
            .take(MAX_ENTRIES)
        {
            let child = child.map_err(|e| format!("{disk}: {e}"))?;
            let Ok(name) = child.file_name().into_string() else {
                continue;
            };
            if name.len() > disk.len()
                && name.starts_with(disk)
                && child.path().join("partition").exists()
            {
                members.push(name);
            }
        }
        let mounts = self.mounts()?;
        for member in &members {
            let held = fs::read_dir(entry.join(member).join("holders"))
                .or_else(|_| fs::read_dir(self.sys.join(member).join("holders")))
                .is_ok_and(|mut held| held.next().is_some());
            let node = self.dev.join(member);
            let elsewhere = mounts.iter().any(|(source, target)| {
                Path::new(source) == node && !Path::new(target).starts_with(&self.root)
            });
            if held || elsewhere {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Mount `name`, rechecking everything the watcher could not.
    pub fn mount(&self, name: &str) -> Result<String, String> {
        if !candidate(&self.sys, name) {
            return Err(format!("{name} is not removable media"));
        }
        if self.system_disk(&self.disk(name)?)? {
            return Err(format!("{name} shares a disk with the system's own"));
        }
        let owned = self.owned()?;
        if let Some((_, leaf)) = owned.iter().find(|(device, _)| device == name) {
            return Ok(self.root.join(leaf).display().to_string());
        }
        let node = self.dev.join(name);
        let expected = sysfs_text(&self.sys.join(name).join("dev"))
            .ok()
            .and_then(|text| probe::device_number(&text))
            .ok_or_else(|| format!("{name} has no device number"))?;
        let before = fs::symlink_metadata(&node).map_err(|e| format!("{}: {e}", node.display()))?;
        if !probe::same(&before, expected) {
            return Err(format!("{} disagrees with sysfs", node.display()));
        }
        if self
            .mounts()?
            .iter()
            .any(|(source, _)| Path::new(source) == node)
        {
            return Err(format!("{name} is already mounted elsewhere"));
        }
        let device = OpenOptions::new()
            .read(true)
            .custom_flags(NOFOLLOW_NONBLOCK)
            .open(&node)
            .map_err(|e| format!("{}: {e}", node.display()))?;
        let meta = device.metadata().map_err(|e| e.to_string())?;
        if !probe::same(&meta, expected) || meta.ino() != before.ino() {
            return Err(format!("{} changed during open", node.display()));
        }
        let (media, label) = self
            .identify_confined(device)
            .map_err(|e| format!("{name}: {e}"))?
            .ok_or_else(|| format!("{name} holds no filesystem td mounts"))?;
        let read_only = sysfs_text(&self.sys.join(name).join("ro")).is_ok_and(|ro| ro == "1");
        self.prepare()?;
        // A label spelling a device name would make `eject sdb1` ambiguous.
        let base = label
            .as_deref()
            .and_then(label_name)
            .filter(|leaf| !media_name(leaf))
            .unwrap_or_else(|| format!("{}-{name}", media.word()));
        let taken: BTreeSet<String> = owned.into_iter().map(|(_, leaf)| leaf).collect();
        let leaf = (1..=9)
            .map(|n| {
                if n == 1 {
                    base.clone()
                } else {
                    format!("{base}-{n}")
                }
            })
            .find(|leaf| !taken.contains(leaf))
            .ok_or("too many media share that label")?;
        let target = self.root.join(&leaf);
        match fs::DirBuilder::new().mode(0o755).create(&target) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                if !fs::symlink_metadata(&target).is_ok_and(|m| m.is_dir()) {
                    return Err(format!("{} is not a directory", target.display()));
                }
            }
            Err(e) => return Err(format!("{}: {e}", target.display())),
        }
        let options = media.options(self.uid, self.gid, read_only);
        let source = node.display().to_string();
        let at = target.display().to_string();
        if let Err(error) = self.run(
            &self.mount,
            &["-t", media.word(), "-o", &options, &source, &at],
        ) {
            let _ = fs::remove_dir(&target);
            return Err(format!("mount {name}: {error}"));
        }
        Ok(at)
    }

    /// The owned mount `which` names: a mount-point name first, then a
    /// device name.
    fn resolve(owned: &[(String, String)], which: &str) -> Option<(String, String)> {
        owned
            .iter()
            .find(|(_, leaf)| leaf == which)
            .or_else(|| owned.iter().find(|(device, _)| device == which))
            .cloned()
    }

    /// Unmount by mount-point name or device name; refused while busy.
    pub fn unmount(&self, which: &str) -> Result<String, String> {
        let (_, leaf) = Self::resolve(&self.owned()?, which)
            .ok_or_else(|| format!("nothing of {which} is mounted"))?;
        let target = self.root.join(&leaf);
        let at = target.display().to_string();
        self.run(&self.umount, &[&at])
            .map_err(|e| format!("{leaf} is in use: {e}"))?;
        let _ = fs::remove_dir(&target);
        Ok(at)
    }

    /// The whole disk `name` (a disk or one of its partitions) is on.
    fn disk(&self, name: &str) -> Result<String, String> {
        let entry = self.sys.join(name);
        if !entry.join("partition").exists() {
            return Ok(name.to_string());
        }
        let real = fs::canonicalize(&entry).map_err(|e| format!("{name}: {e}"))?;
        real.parent()
            .and_then(Path::file_name)
            .and_then(|disk| disk.to_str())
            .filter(|disk| media_name(disk))
            .map(str::to_string)
            .ok_or_else(|| format!("{name} has no parent disk"))
    }

    /// Unmount everything on the disk `which` names (a mount-point or
    /// device name), then, for a USB disk, have the kernel flush and
    /// detach it, so it can be pulled.
    pub fn eject(&self, which: &str) -> Result<String, String> {
        let owned = self.owned()?;
        let device = Self::resolve(&owned, which).map_or_else(|| which.to_string(), |(d, _)| d);
        let entry = self.sys.join(&device);
        let removable = probe::on_usb(&entry).unwrap_or(false) || on_mmc(&entry).unwrap_or(false);
        if !media_name(&device) || !removable {
            return Err(format!("{which} is not removable media"));
        }
        let disk = self.disk(&device)?;
        if self.system_disk(&disk)? {
            return Err(format!("{disk} carries the system's own filesystems"));
        }
        for (on, leaf) in &owned {
            if self.disk(on).ok().as_deref() == Some(disk.as_str()) {
                self.unmount(leaf)?;
            }
        }
        let entry = self.sys.join(&disk);
        if disk.starts_with("sd") && probe::on_usb(&entry).unwrap_or(false) {
            fs::write(entry.join("device/delete"), b"1\n")
                .map_err(|e| format!("detach {disk}: {e}"))?;
            return Ok(format!("{disk} may be removed"));
        }
        Ok(format!("{disk} is unmounted and may be removed"))
    }

    pub fn list(&self) -> Result<String, String> {
        let mut text = String::new();
        for (device, leaf) in self.owned()? {
            text.push_str(&format!(
                "{leaf} {} {}\n",
                self.dev.join(&device).display(),
                self.root.join(&leaf).display()
            ));
        }
        Ok(text)
    }

    /// Whether `device` no longer holds what was mounted: it left sysfs,
    /// or it or its whole disk reports no media. A card pulled from a
    /// reader that stays drops the disk's size to 0 while a mounted
    /// partition keeps its own, since the kernel rescans no partitions
    /// while one is open; the lazy unmount frees it for the next rescan.
    pub fn gone(&self, device: &str) -> bool {
        let entry = self.sys.join(device);
        let empty = |dir: &Path| sysfs_text(&dir.join("size")).map_or(true, |size| size == "0");
        empty(&entry) || whole(&entry).map_or(true, |disk| empty(&disk))
    }

    /// Detach what a device that is `gone` held.
    pub fn sweep(&self) {
        let Ok(owned) = self.owned() else { return };
        for (device, leaf) in owned {
            if self.gone(&device) {
                let target = self.root.join(&leaf);
                let _ = self.run(&self.umount, &["-l", &target.display().to_string()]);
                let _ = fs::remove_dir(&target);
            }
        }
    }

    pub fn answer(&self, request: &str) -> String {
        let words: Vec<&str> = request.split(' ').collect();
        let result = match words.as_slice() {
            ["mount", name] if media_name(name) => self.mount(name),
            ["unmount", which] if label_name(which).as_deref() == Some(*which) => {
                self.unmount(which)
            }
            ["eject", which] if label_name(which).as_deref() == Some(*which) => self.eject(which),
            ["list"] => self.list(),
            _ => Err("expected mount NAME, unmount NAME, eject NAME or list".into()),
        };
        match result {
            Ok(text) if text.ends_with('\n') || text.is_empty() => format!("ok\n{text}"),
            Ok(text) => format!("ok {text}\n"),
            Err(error) => format!("error {}\n", error.replace('\n', " ")),
        }
    }

    /// Bind the owner's socket: created by root in root's 0755
    /// `/run/td-disks`, then handed to the owner at mode 0600, so only the
    /// owner and root can connect. `lchown` and the path's root-owned
    /// directory keep a link from redirecting either change.
    pub fn bind(&self) -> Result<UnixListener, String> {
        if let Some(dir) = self.socket.parent() {
            match fs::DirBuilder::new().mode(0o755).create(dir) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(format!("{}: {e}", dir.display())),
            }
            let meta = fs::symlink_metadata(dir).map_err(|e| e.to_string())?;
            if !meta.is_dir() || meta.uid() != 0 || meta.mode() & 0o022 != 0 {
                return Err(format!("untrusted socket directory {}", dir.display()));
            }
        }
        // Bound under a name nobody connects to, and renamed into place
        // once it is the owner's at 0600: no umask decides who connects.
        let staged = self.socket.with_extension("new");
        for path in [&staged, &self.socket] {
            match fs::remove_file(path) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(format!("{}: {e}", path.display())),
            }
        }
        let listener =
            UnixListener::bind(&staged).map_err(|e| format!("bind {}: {e}", staged.display()))?;
        std::os::unix::fs::lchown(&staged, Some(self.uid), Some(self.gid))
            .map_err(|e| format!("hand {} to the owner: {e}", staged.display()))?;
        fs::set_permissions(&staged, fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("{}: {e}", staged.display()))?;
        fs::rename(&staged, &self.socket).map_err(|e| format!("{}: {e}", self.socket.display()))?;
        listener.set_nonblocking(true).map_err(|e| e.to_string())?;
        Ok(listener)
    }

    pub fn serve(&self) -> Result<(), String> {
        self.prepare()?;
        let listener = self.bind()?;
        let (sys, dev) = (self.sys.clone(), self.dev.clone());
        std::thread::Builder::new()
            .spawn(move || loop {
                revalidate(&sys, &dev);
                std::thread::sleep(REVALIDATE_INTERVAL);
            })
            .map_err(|e| format!("revalidation thread: {e}"))?;
        let mut swept = Instant::now();
        loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    let _ = self.connection(stream);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => std::thread::sleep(TICK),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(format!("accept: {e}")),
            }
            if swept.elapsed() >= WATCH_INTERVAL {
                self.sweep();
                swept = Instant::now();
            }
        }
    }

    fn connection(&self, stream: UnixStream) -> io::Result<()> {
        stream.set_nonblocking(false)?;
        stream.set_write_timeout(Some(IO_TIMEOUT))?;
        let line = read_request(&stream, Instant::now() + REQUEST_DEADLINE)?;
        let reply = self.answer(&line);
        (&stream).write_all(reply.as_bytes())
    }
}

/// One line of at most `MAX_REQUEST` bytes, whole before `deadline`
/// however slowly it arrives.
fn read_request(stream: &UnixStream, deadline: Instant) -> io::Result<String> {
    let mut line = Vec::new();
    let mut byte = [0u8; 64];
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "request deadline"));
        }
        stream.set_read_timeout(Some(left))?;
        let read = (&*stream).read(&mut byte)?;
        let chunk = byte.get(..read).unwrap_or_default();
        if read == 0 {
            break;
        }
        if let Some(end) = chunk.iter().position(|b| *b == b'\n') {
            line.extend_from_slice(chunk.get(..end).unwrap_or_default());
            break;
        }
        line.extend_from_slice(chunk);
        if line.len() as u64 > MAX_REQUEST {
            return Err(io::Error::other("request too long"));
        }
    }
    String::from_utf8(line).map_err(|_| io::Error::other("request is not UTF-8"))
}

/// Open each USB disk that says it is removable, as udev's poll does, so
/// a card reader's LUN notices a card arriving or leaving: without udev
/// nothing else opens it, and the kernel revalidates only on open.
pub fn revalidate(sys: &Path, dev: &Path) {
    let Ok(entries) = fs::read_dir(sys) else {
        return;
    };
    for entry in entries.take(MAX_ENTRIES).flatten() {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        let path = sys.join(&name);
        let reader = name.starts_with("sd")
            && media_name(&name)
            && !path.join("partition").exists()
            && probe::on_usb(&path).unwrap_or(false)
            && sysfs_text(&path.join("removable")).is_ok_and(|r| r == "1");
        let numbered = sysfs_text(&path.join("dev"))
            .ok()
            .and_then(|text| probe::device_number(&text));
        if let (true, Some(expected)) = (reader, numbered) {
            let node = dev.join(&name);
            if fs::symlink_metadata(&node).is_ok_and(|meta| probe::same(&meta, expected)) {
                let _ = OpenOptions::new()
                    .read(true)
                    .custom_flags(NOFOLLOW_NONBLOCK)
                    .open(&node);
            }
        }
    }
}

/// One request to `serve`, and its reply.
pub fn ask(socket: &Path, request: &str) -> Result<String, String> {
    let mut stream =
        UnixStream::connect(socket).map_err(|e| format!("{}: {e}", socket.display()))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(90)))
        .map_err(|e| e.to_string())?;
    stream
        .write_all(format!("{request}\n").as_bytes())
        .map_err(|e| e.to_string())?;
    let mut reply = String::new();
    stream
        .take(MAX_REPLY)
        .read_to_string(&mut reply)
        .map_err(|e| e.to_string())?;
    Ok(reply)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Lock {
    Unlocked,
    Locked,
    /// The compositor did not answer, or answered something else.
    Unknown,
}

pub fn lock_reply(reply: &str) -> Lock {
    match reply {
        "ok\nunlocked\n" => Lock::Unlocked,
        "ok\nlocked\n" => Lock::Locked,
        _ => Lock::Unknown,
    }
}

/// The compositor's answer to `lock-state` on its control socket.
pub fn lock_state(control: &Path) -> Lock {
    let Ok(mut stream) = UnixStream::connect(control) else {
        return Lock::Unknown;
    };
    if stream.set_read_timeout(Some(IO_TIMEOUT)).is_err()
        || stream.write_all(b"lock-state\n").is_err()
    {
        return Lock::Unknown;
    }
    let mut reply = String::new();
    if stream.take(256).read_to_string(&mut reply).is_err() {
        return Lock::Unknown;
    }
    lock_reply(&reply)
}

/// What the watcher does with the candidates present now, given the lock
/// state: the names to ask mounted, and the names passed over. `asked`
/// holds every name already decided, mounted or passed over, until it
/// leaves.
pub fn step(
    asked: &mut BTreeSet<String>,
    present: &BTreeSet<String>,
    lock: Lock,
) -> (Vec<String>, Vec<String>) {
    asked.retain(|name| present.contains(name));
    let fresh: Vec<String> = present.difference(asked).cloned().collect();
    asked.extend(fresh.iter().cloned());
    match lock {
        // Not yet known: decide nothing, so the next answer decides.
        Lock::Unknown => {
            for name in &fresh {
                asked.remove(name);
            }
            (Vec::new(), Vec::new())
        }
        // Present while locked: passed over until it is replugged.
        Lock::Locked => (Vec::new(), fresh),
        Lock::Unlocked => (fresh, Vec::new()),
    }
}

/// The session's watcher: while unlocked, ask root to mount each new
/// candidate once; forget what leaves, so its return mounts again.
pub fn watch(sys: &Path, control: &Path, socket: &Path) -> Result<(), String> {
    let mut asked: BTreeSet<String> = BTreeSet::new();
    loop {
        let present: BTreeSet<String> = candidates(sys).into_iter().collect();
        let fresh = present.difference(&asked).next().is_some();
        let lock = if fresh {
            lock_state(control)
        } else {
            Lock::Unknown
        };
        let (mount, passed) = step(&mut asked, &present, lock);
        for name in passed {
            let _ = writeln!(
                io::stdout(),
                "td-disks: {name} arrived while locked; replug it or run td-disks media mount {name}"
            );
        }
        for name in mount {
            match ask(socket, &format!("mount {name}")) {
                Ok(reply) => {
                    let _ = write!(io::stdout(), "td-disks: {name}: {reply}");
                }
                // Root did not answer: undecided, so the next tick asks again.
                Err(error) => {
                    asked.remove(&name);
                    let _ = writeln!(io::stdout(), "td-disks: {name}: {error}");
                }
            }
        }
        std::thread::sleep(WATCH_INTERVAL);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn boot(kind: &[u8], at: usize, label: &[u8], label_at: usize) -> Vec<u8> {
        let mut sector = vec![0u8; 512];
        sector[11..13].copy_from_slice(&512u16.to_le_bytes());
        sector[13] = 8;
        sector[at..at + kind.len()].copy_from_slice(kind);
        sector[label_at..label_at + label.len()].copy_from_slice(label);
        sector[510] = 0x55;
        sector[511] = 0xaa;
        sector
    }

    #[test]
    fn fat_and_exfat_boot_sectors_are_told_apart_with_labels() {
        let fat32 = boot(b"FAT32   ", 82, b"CAMERA     ", 71);
        assert_eq!(fat(&fat32), Some(Some("CAMERA".into())));
        let fat16 = boot(b"FAT16   ", 54, b"NO NAME    ", 43);
        assert_eq!(fat(&fat16), Some(None));
        let mut bad = fat32.clone();
        bad[511] = 0;
        assert_eq!(fat(&bad), None);
        let mut sectors = fat32.clone();
        sectors[11..13].copy_from_slice(&3u16.to_le_bytes());
        assert_eq!(fat(&sectors), None, "implausible sector size");
        let mut ex = vec![0u8; 512];
        ex[3..11].copy_from_slice(b"EXFAT   ");
        ex[510] = 0x55;
        ex[511] = 0xaa;
        assert!(exfat(&ex));
        assert!(!exfat(&fat32));
        assert_eq!(fat(&ex), None);
    }

    #[test]
    fn optical_descriptors_identify_iso9660_and_udf() {
        let mut pvd = vec![0u8; 2048];
        pvd[0] = 1;
        pvd[1..6].copy_from_slice(b"CD001");
        pvd[40..51].copy_from_slice(b"DEBIAN_13  ");
        assert_eq!(iso9660(&pvd), Some(Some("DEBIAN_13".into())));
        pvd[0] = 2;
        assert_eq!(iso9660(&pvd), None, "a supplementary descriptor");
        let mut vrs = vec![0u8; 2048 * 3];
        vrs[1..6].copy_from_slice(b"CD001");
        vrs[2048 + 1..2048 + 6].copy_from_slice(b"BEA01");
        vrs[4096 + 1..4096 + 6].copy_from_slice(b"NSR03");
        assert!(udf(&vrs, 2048));
        vrs[2048 + 1..2048 + 6].copy_from_slice(b"XXXXX");
        assert!(!udf(&vrs, 2048), "no BEA01 first");
        let mut wide = vec![0u8; 4096 * 3];
        wide[4096 + 1..4096 + 6].copy_from_slice(b"BEA01");
        wide[8192 + 1..8192 + 6].copy_from_slice(b"NSR03");
        wide[1..6].copy_from_slice(b"CD001");
        assert!(udf(&wide, 4096), "4 KiB blocks space the sequence wider");
    }

    #[test]
    fn labels_become_safe_names() {
        assert_eq!(label_name("CAMERA").as_deref(), Some("CAMERA"));
        assert_eq!(label_name("My Disk/2").as_deref(), Some("My_Disk_2"));
        assert_eq!(label_name("../etc").as_deref(), Some("_etc"));
        assert_eq!(label_name("..").as_deref(), None);
        assert_eq!(label_name("   ").as_deref(), None);
        assert_eq!(label_name(&"a".repeat(40)).unwrap().len(), MAX_LABEL);
        assert_eq!(label_name("-x").as_deref(), Some("x"));
    }

    #[test]
    fn media_names_and_options() {
        for name in ["sdc", "sdc1", "mmcblk0", "mmcblk0p1"] {
            assert!(media_name(name), "{name}");
        }
        for name in [
            "nvme0n1",
            "vda",
            "mmcblk",
            "mmcblk0p",
            "mmcblk0boot0",
            "loop0",
        ] {
            assert!(!media_name(name), "{name}");
        }
        assert_eq!(
            Media::Vfat.options(1000, 100, false),
            "nodev,nosuid,noexec,uid=1000,gid=100,umask=077,errors=remount-ro"
        );
        assert_eq!(
            Media::Iso9660.options(1000, 100, false),
            "nodev,nosuid,noexec,ro,uid=1000,gid=100"
        );
        assert_eq!(
            Media::Ext4.options(1000, 100, true),
            "nodev,nosuid,noexec,ro,errors=remount-ro"
        );
        let mut sb = vec![0u8; 1024];
        assert_eq!(ext4_refusal(&sb), None);
        sb[0x5c] = 0x4;
        assert_eq!(ext4_refusal(&sb), Some("names an external journal"));
        sb[0xe0] = 8;
        assert_eq!(ext4_refusal(&sb), None, "an internal journal at inode 8");
        sb[0x200..0x20c].copy_from_slice(b"errors=panic");
        assert_eq!(ext4_refusal(&sb), Some("carries its own mount options"));
    }

    #[test]
    fn the_owners_group_must_be_its_own() {
        assert!(private_group(
            "tester:x:1000:\nwheel:x:10:tester\n",
            "tester",
            1000
        ));
        assert!(!private_group("users:x:100:\n", "tester", 100));
        assert!(!private_group("tester:x:1000:other\n", "tester", 1000));
        assert!(!private_group(
            "tester:x:1000:\nshadow:x:1000:\n",
            "tester",
            1000
        ));
    }

    /// No test below root reaches the identity change or the worker's refusal,
    /// so losing both would put parsing back in root's hands with every gate
    /// green. The contract is pinned where a gate can see it.
    #[test]
    fn the_worker_changes_identity_and_refuses_otherwise() {
        // Comments stripped, so commenting a line out is losing it.
        let source: String = include_str!("media.rs")
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        let spawner = source
            .split_once("pub fn identify_confined(")
            .and_then(|(_, rest)| rest.split_once("\n    }\n"))
            .map(|(body, _)| body)
            .unwrap();
        // The same value in both columns, and this test holds which: 65534 is
        // td-time's worker and a human's uid would pass the worker's own guard.
        assert_eq!(IDENTIFY_UID, 65532);
        assert!(spawner.contains(
            "if fs::metadata(\"/proc/self\").map_err(|e| e.to_string())?.uid() == 0 {\n            \
             command.uid(IDENTIFY_UID).gid(IDENTIFY_UID);\n        }"
        ));
        assert!(spawner.contains(".env_clear()"));
        assert!(spawner.contains(".stdin(device)"));
        assert!(!spawner.contains("identify("), "root must not parse");
        let worker = source
            .split_once("pub fn identify_stdin(")
            .and_then(|(_, rest)| rest.split_once("\n}\n"))
            .map(|(body, _)| body)
            .unwrap();
        let guard = worker
            .find("if !unprivileged(&status) {\n        return Err(")
            .unwrap();
        // Split, so this test's own text is not a call it counts.
        let call = concat!("identify", "(&");
        let parse = worker.find(call).unwrap();
        assert!(guard < parse, "the guard must precede every read");
        assert_eq!(source.matches(call).count(), 1, "one reader of a medium");
    }

    #[test]
    fn a_worker_answer_round_trips_and_a_hostile_one_is_refused() {
        for found in [
            None,
            Some((Media::Udf, None)),
            Some((Media::Vfat, Some("CAMERA".to_string()))),
        ] {
            let line = identity_line(&Ok(found.clone()));
            assert_eq!(parse_identity(&line), Ok(found));
        }
        // A label is mapped before it crosses, so it cannot end the line.
        let line = identity_line(&Ok(Some((Media::Vfat, Some("A\nok udf".into())))));
        assert_eq!(line, "ok vfat A_ok_udf\n");
        let refused = identity_line(&Err(io::Error::other("its ext4\nsuperblock")));
        assert_eq!(parse_identity(&refused), Err("its ext4superblock".into()));

        for hostile in [
            "",
            "none",
            "none\nnone\n",
            "ok\n",
            "ok \n",
            "ok ntfs\n",
            "ok vfat\nok udf\n",
            "mounted\n",
        ] {
            assert!(
                parse_identity(hostile).is_err(),
                "root took {hostile:?} from a worker"
            );
        }
        // Whatever a worker says, its reason reaches a reply as one line of
        // printable ASCII, and a bounded one.
        let long = format!("error \u{1b}[2J{}\n", "x".repeat(4096));
        let why = parse_identity(&long).unwrap_err();
        assert!(why.len() <= MAX_LABEL * 4 && why.bytes().all(|b| b.is_ascii_graphic()));
        // A label a worker sent unmapped still meets root's own rule.
        let (_, label) = parse_identity("ok vfat ../../etc\n").unwrap().unwrap();
        assert_eq!(label.as_deref().and_then(label_name).unwrap(), "_.._etc");
    }

    #[test]
    fn a_worker_with_any_privilege_is_told_apart() {
        let status = |uid: &str, cap: &str| {
            format!("Name:\tx\nUid:\t{uid}\nGid:\t65532\t65532\t65532\t65532\nCapInh:\t0000000000000001\nCapPrm:\t{cap}\nCapEff:\t{cap}\nCapAmb:\t{cap}\n")
        };
        let none = "0000000000000000";
        assert!(unprivileged(&status("65532\t65532\t65532\t65532", none)));
        for uid in ["0\t0\t0\t0", "65532\t0\t65532\t65532", "65532\t65532\t0\t0"] {
            assert!(!unprivileged(&status(uid, none)), "{uid}");
        }
        assert!(!unprivileged(&status(
            "65532\t65532\t65532\t65532",
            "0000000000000400"
        )));
        // A launch that changed the uid and kept root's group.
        let kept_gid = status("65532\t65532\t65532\t65532", none)
            .replace("Gid:\t65532\t65532\t65532\t65532", "Gid:\t0\t0\t0\t0");
        assert!(!unprivileged(&kept_gid));
        // What it cannot read is not unprivileged.
        assert!(!unprivileged(""));
        assert!(!unprivileged(&status("65532\t65532\t65532", none)));
        assert!(!unprivileged("Uid:\t1\t1\t1\t1\n"));
    }

    #[test]
    fn the_owner_is_read_from_passwd() {
        let passwd = "root:x:0:0::/root:/bin/sh\ntester:x:1000:100::/home/tester:/bin/sh\n";
        assert_eq!(account(passwd, 1000), Some(("tester".into(), 100)));
        assert_eq!(account(passwd, 1001), None);
        assert_eq!(account("../x:x:1000:1::/:/bin/sh\n", 1000), None);
    }

    #[test]
    fn candidates_are_usb_or_mmc_with_media_and_requests_are_parsed() {
        let dir = td_fs::TempDir::new("td-disks-media").unwrap();
        let root = dir.path();
        let usb = root.join("devices/pci0/usb2/2-1/host6/block/sdc");
        let sata = root.join("devices/pci0/ata1/host0/block/sda");
        let card = root.join("devices/pci0/mmc_host/mmc0/mmc0:0001/block/mmcblk0");
        let emmc = root.join("devices/pci0/mmc_host/mmc1/mmc1:0001/block/mmcblk1");
        for (path, size) in [
            (&usb, "62521344"),
            (&sata, "100"),
            (&card, "0"),
            (&emmc, "61071360"),
        ] {
            fs::create_dir_all(path.join("holders")).unwrap();
            fs::write(path.join("size"), format!("{size}\n")).unwrap();
        }
        for (path, kind) in [(&card, "SD"), (&emmc, "MMC")] {
            fs::create_dir(path.join("device")).unwrap();
            fs::write(path.join("device/type"), format!("{kind}\n")).unwrap();
        }
        let class = root.join("class");
        fs::create_dir(&class).unwrap();
        for (path, name) in [
            (&usb, "sdc"),
            (&sata, "sda"),
            (&card, "mmcblk0"),
            (&emmc, "mmcblk1"),
        ] {
            std::os::unix::fs::symlink(path, class.join(name)).unwrap();
        }
        assert_eq!(
            candidates(&class),
            ["sdc"],
            "SATA is no media; an empty reader neither"
        );
        fs::write(card.join("size"), "31116288\n").unwrap();
        assert_eq!(
            candidates(&class),
            ["mmcblk0", "sdc"],
            "never soldered eMMC"
        );
        let serve = Serve {
            socket: root.join("media.sock"),
            root: root.join("run-media/tester"),
            sys: class.clone(),
            dev: root.join("dev"),
            mounts: root.join("mounts"),
            mount: "/nonexistent/mount".into(),
            umount: "/nonexistent/umount".into(),
            worker: "/nonexistent/td-disks".into(),
            identify_deadline: IDENTIFY_DEADLINE,
            uid: 1000,
            gid: 100,
            user: "tester".into(),
        };
        fs::write(&serve.mounts, "/dev/nvme0n1p3 / btrfs rw 0 0\n").unwrap();
        assert!(serve
            .answer("mount sda")
            .starts_with("error sda is not removable media"));
        assert!(serve.answer("mount ../x").starts_with("error expected"));
        assert!(serve.answer("unmount a/b").starts_with("error expected"));
        assert!(serve
            .answer("unmount CAMERA")
            .starts_with("error nothing of CAMERA"));
        assert_eq!(serve.answer("list"), "ok\n");
        fs::write(
            &serve.mounts,
            format!(
                "{} {} vfat rw 0 0\n",
                serve.dev.join("sdc1").display(),
                serve.root.join("CAMERA").display()
            ),
        )
        .unwrap();
        let listed = format!("CAMERA {}", serve.dev.join("sdc1").display());
        assert!(serve.answer("list").contains(&listed));
        // sdc1 is gone from sysfs: the sweep tries a lazy unmount.
        assert!(serve.gone("sdc1"));
        serve.sweep();
        // A td installed to a USB disk: its root partition is mounted, so
        // the disk's ESP is the system's, not media.
        let disk = root.join("devices/pci0/usb3/3-1/host7/block/sde");
        for part in ["sde1", "sde2"] {
            fs::create_dir_all(disk.join(part).join("holders")).unwrap();
            fs::write(disk.join(part).join("partition"), "1\n").unwrap();
            fs::write(disk.join(part).join("size"), "1000\n").unwrap();
            std::os::unix::fs::symlink(disk.join(part), class.join(part)).unwrap();
        }
        fs::create_dir_all(disk.join("holders")).unwrap();
        fs::write(disk.join("size"), "2000\n").unwrap();
        std::os::unix::fs::symlink(&disk, class.join("sde")).unwrap();
        assert!(candidates(&class).contains(&"sde1".to_string()));
        assert!(!serve.gone("sde1"));
        fs::write(disk.join("size"), "0\n").unwrap();
        assert!(serve.gone("sde1"), "the card left; its partition did not");
        fs::write(disk.join("size"), "2000\n").unwrap();
        fs::write(
            &serve.mounts,
            format!("{} / ext4 rw 0 0\n", serve.dev.join("sde2").display()),
        )
        .unwrap();
        assert!(serve
            .answer("mount sde1")
            .starts_with("error sde1 shares a disk with the system's own"));
        assert!(serve
            .answer("eject sde1")
            .starts_with("error sde carries the system's own"));
        assert!(serve
            .answer("eject sdq")
            .starts_with("error sdq is not removable media"));
    }

    #[test]
    fn the_watcher_mounts_only_what_arrives_while_unlocked() {
        let set = |names: &[&str]| names.iter().map(|n| n.to_string()).collect::<BTreeSet<_>>();
        let none: (Vec<String>, Vec<String>) = (vec![], vec![]);
        let mut asked = BTreeSet::new();
        assert_eq!(step(&mut asked, &set(&["sdc1"]), Lock::Unknown), none);
        assert!(asked.is_empty(), "an unknown state decides nothing");
        assert_eq!(
            step(&mut asked, &set(&["sdc1"]), Lock::Locked),
            (vec![], vec!["sdc1".to_string()])
        );
        assert_eq!(
            step(&mut asked, &set(&["sdc1", "sdd1"]), Lock::Unlocked),
            (vec!["sdd1".to_string()], vec![]),
            "sdc1 arrived behind the lock and stays passed over"
        );
        assert_eq!(step(&mut asked, &set(&["sdd1"]), Lock::Unlocked), none);
        assert_eq!(
            step(&mut asked, &set(&["sdc1", "sdd1"]), Lock::Unlocked),
            (vec!["sdc1".to_string()], vec![]),
            "replugged"
        );
        assert_eq!(lock_reply("ok\nunlocked\n"), Lock::Unlocked);
        assert_eq!(lock_reply("ok\nlocked\n"), Lock::Locked);
        assert_eq!(lock_reply("error no such request\n"), Lock::Unknown);
    }
}

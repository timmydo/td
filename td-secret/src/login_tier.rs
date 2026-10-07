//! The login-key tier marker (td-login/TOKEN-LOGIN.md, "Deployments"): its
//! grammar, and the bounded reader that takes it from a deployment's
//! `initramfs.cpio` once the manifest hashes to the deployment ID and the
//! archive to the manifest's entry. td-secret's login worker reads the
//! retained deployments through it, and td-authd compiles this one file
//! for request 19's queued update and request 1d's selectors. It uses std
//! alone and hashes with the `engine/src/sha256.rs` copy each crate
//! compiles as `crate::sha256`.
#![forbid(unsafe_code)]

use crate::sha256::{self, Sha256};
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

type Result<T> = std::result::Result<T, String>;

/// The marker's archive member, and its first word.
pub const MEMBER: &[u8] = b"etc/td-login-tier";
pub const HEADER: &[u8] = b"td-login-tier-v1";
/// The read-only volume a booted machine holds, and its two selectors.
pub const VOLUME: &str = "/run/td-volume/td";
pub const CURRENT: &str = "current";
pub const PREVIOUS: &str = "previous";
/// What a selector must name, then 64 lowercase hex digits.
const SELECTOR: &[u8] = b"../deployments/";
const MANIFEST: &str = "manifest";
const ARCHIVE: &str = "initramfs.cpio";
const MANIFEST_HEADER: &[u8] = b"td-deployment-v1";
/// td-boot's manifest bound and td-update's initramfs staging bound.
pub const MANIFEST_LIMIT: u64 = 4096;
pub const ARCHIVE_LIMIT: u64 = 512 << 20;
/// The newc reader's bounds: a member's name field with its NUL, the
/// members before the trailer, and the marker's bytes.
pub const NAME_LIMIT: usize = 4096;
pub const MEMBER_LIMIT: usize = 65536;
pub const MARKER_LIMIT: usize = 256;
const MAGIC: &[u8] = b"070701";
const TRAILER: &[u8] = b"TRAILER!!!";
const NEWC_HEADER: usize = 110;
const CHUNK: usize = 64 * 1024;
const FD_ROOT: &str = "/proc/self/fd";
const NOFOLLOW: i32 = 0o400000;
const NONBLOCK: i32 = 0o4000;
const OPEN_DIRECTORY: i32 = 0o200000;
const FORMAT: u32 = 0o170000;
const REGULAR: u32 = 0o100000;

/// The versions a marker lists, when its bytes are exactly the grammar:
/// `td-login-tier-v1`, then for each version a space and a decimal from 1
/// to 255 without leading zeros, strictly increasing, then one newline.
pub fn parse(marker: &[u8]) -> Option<Vec<u8>> {
    let mut rest = marker.strip_suffix(b"\n")?.strip_prefix(HEADER)?;
    let mut versions: Vec<u8> = Vec::new();
    while !rest.is_empty() {
        rest = rest.strip_prefix(b" ")?;
        let end = rest
            .iter()
            .position(|byte| *byte == b' ')
            .unwrap_or(rest.len());
        let (digits, tail) = rest.split_at_checked(end)?;
        if digits.is_empty() || digits.len() > 3 || digits.first() == Some(&b'0') {
            return None;
        }
        let mut value = 0u32;
        for digit in digits {
            value = value * 10 + char::from(*digit).to_digit(10)?;
        }
        let version = u8::try_from(value).ok()?;
        if versions.last().is_some_and(|last| *last >= version) {
            return None;
        }
        versions.push(version);
        rest = tail;
    }
    (!versions.is_empty()).then_some(versions)
}

/// The record versions the deployment held open as `directory` reads,
/// against the ID `id` it was admitted or selected by. Its manifest and
/// archive must be regular files owned by `owner`. The read gives up
/// after `give_up`, its caller's budget; it checks between reads, so it
/// cannot interrupt a stalled filesystem.
pub fn read(directory: &File, id: &str, owner: u32, give_up: Duration) -> Result<Vec<u8>> {
    read_until(directory, id, owner, deadline(give_up)?)
}

/// The versions the deployment the volume's `slot` selector names reads:
/// the selector is read once, and that deployment opened by its name,
/// within `give_up` as for `read`.
pub fn retained(volume: &Path, slot: &str, owner: u32, give_up: Duration) -> Result<Vec<u8>> {
    let deadline = deadline(give_up)?;
    let volume = open_volume(volume)?;
    let id = selected(&volume, slot)?;
    let deployments = open_directory(&at(&volume, "deployments"))?;
    let deployment = open_directory(&at(&deployments, &id))?;
    read_until(&deployment, &id, owner, deadline)
}

/// The volume, held open as a directory without following a link, for
/// `selected` to read through.
pub fn open_volume(volume: &Path) -> Result<File> {
    open_directory(volume)
}

/// The deployment ID the held volume's `slot` selector names, read once:
/// it must be exactly `../deployments/` and 64 lowercase hex digits.
pub fn selected(volume: &File, slot: &str) -> Result<String> {
    if slot != CURRENT && slot != PREVIOUS {
        return Err(format!("{slot:?} is not a deployment selector"));
    }
    let boot = open_directory(&at(volume, "boot"))?;
    let target =
        fs::read_link(at(&boot, slot)).map_err(|e| format!("read the {slot} selector: {e}"))?;
    target
        .as_os_str()
        .as_bytes()
        .strip_prefix(SELECTOR)
        .filter(|id| is_id(id))
        .and_then(|id| std::str::from_utf8(id).ok())
        .map(str::to_string)
        .ok_or_else(|| format!("the {slot} selector does not name ../deployments/<id>"))
}

fn deadline(give_up: Duration) -> Result<Instant> {
    Instant::now()
        .checked_add(give_up)
        .ok_or_else(|| "tier marker deadline overflow".into())
}

fn read_until(directory: &File, id: &str, owner: u32, deadline: Instant) -> Result<Vec<u8>> {
    if !is_id(id.as_bytes()) {
        return Err("invalid deployment ID".into());
    }
    let (manifest, size) = open(directory, MANIFEST, owner, MANIFEST_LIMIT)?;
    let mut bytes = Vec::with_capacity(MANIFEST_LIMIT as usize + 1);
    manifest
        .take(MANIFEST_LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("read the manifest: {e}"))?;
    if u64::try_from(bytes.len()).ok() != Some(size) {
        return Err("the manifest changed size while reading".into());
    }
    if sha256::hex_digest(&bytes) != id {
        return Err("the manifest does not hash to the deployment ID".into());
    }
    let expected = initramfs_entry(&bytes).ok_or("the manifest is not td-deployment-v1")?;
    let (archive, size) = open(directory, ARCHIVE, owner, ARCHIVE_LIMIT)?;
    verify(&mut &archive, size, expected, deadline, || {
        archive.metadata().map(|meta| meta.len())
    })
}

/// Opens `name` in `directory` without following a link or waiting on a
/// FIFO, and admits only a bounded regular file of `owner`'s.
fn open(directory: &File, name: &str, owner: u32, limit: u64) -> Result<(File, u64)> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(NOFOLLOW | NONBLOCK)
        .open(at(directory, name))
        .map_err(|e| format!("open {name}: {e}"))?;
    let meta = file
        .metadata()
        .map_err(|e| format!("inspect {name}: {e}"))?;
    admit(&meta, owner, limit).map_err(|why| format!("{name} {why}"))?;
    Ok((file, meta.len()))
}

fn admit(meta: &Metadata, owner: u32, limit: u64) -> std::result::Result<(), &'static str> {
    if !meta.is_file() {
        Err("is not a regular file")
    } else if meta.uid() != owner {
        Err("has the wrong owner")
    } else if meta.len() == 0 || meta.len() > limit {
        Err("is empty or larger than its bound")
    } else {
        Ok(())
    }
}

fn open_directory(path: &Path) -> Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(OPEN_DIRECTORY | NOFOLLOW)
        .open(path)
        .map_err(|e| format!("open {path:?}: {e}"))
}

fn at(directory: &File, name: &str) -> PathBuf {
    Path::new(FD_ROOT)
        .join(directory.as_raw_fd().to_string())
        .join(name)
}

fn is_id(bytes: &[u8]) -> bool {
    bytes.len() == 64
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
}

/// The `initramfs.cpio` digest of td-boot's exact four-line manifest.
fn initramfs_entry(manifest: &[u8]) -> Option<&str> {
    let mut lines = manifest.split(|byte| *byte == b'\n');
    if lines.next()? != MANIFEST_HEADER {
        return None;
    }
    let (kernel, initramfs, root) = (lines.next()?, lines.next()?, lines.next()?);
    if !lines.next()?.is_empty() || lines.next().is_some() {
        return None;
    }
    entry(kernel, b"bzImage")?;
    entry(root, b"root.erofs")?;
    std::str::from_utf8(entry(initramfs, b"initramfs.cpio")?).ok()
}

fn entry<'a>(line: &'a [u8], label: &[u8]) -> Option<&'a [u8]> {
    let (digest, rest) = line.split_at_checked(64)?;
    (is_id(digest) && rest.strip_prefix(b"  ")? == label).then_some(digest)
}

/// Scans `source`, `size` bytes, as one newc archive, hashing every byte
/// read; `measure` is the size once it ends. The marker's versions only if
/// the archive is whole, unchanged and hashes to `expected`.
fn verify(
    source: &mut impl Read,
    size: u64,
    expected: &str,
    deadline: Instant,
    measure: impl FnOnce() -> io::Result<u64>,
) -> Result<Vec<u8>> {
    let mut stream = Stream {
        source,
        hash: Sha256::new(),
        read: 0,
        size,
        deadline,
        scratch: vec![0; CHUNK],
    };
    let marker = scan(&mut stream)?;
    let now = measure().map_err(|e| format!("inspect the archive: {e}"))?;
    if now != size {
        return Err("the archive changed size while reading".into());
    }
    // A read that ended past the budget is a late read, however it ended.
    stream.expired()?;
    if sha256::to_base16(&stream.hash.finalize()) != expected {
        return Err("the archive does not hash to the manifest's entry".into());
    }
    marker.ok_or_else(|| "the archive carries no tier marker".into())
}

/// The archive as one bounded stream: every byte read is hashed and
/// counted, none is read past `size`, and each read checks the deadline.
struct Stream<'a, R> {
    source: &'a mut R,
    hash: Sha256,
    read: u64,
    size: u64,
    deadline: Instant,
    /// Skipped bytes pass through here, never buffered whole.
    scratch: Vec<u8>,
}

impl<R: Read> Stream<'_, R> {
    fn fill(&mut self, into: &mut [u8]) -> Result<()> {
        let left = self.size.saturating_sub(self.read);
        if u64::try_from(into.len()).map_or(true, |wanted| wanted > left) {
            return Err("the archive ends inside a member".into());
        }
        let mut done = 0;
        while done < into.len() {
            self.expired()?;
            let rest = into.get_mut(done..).ok_or("archive read cursor")?;
            let count = match self.source.read(rest) {
                Ok(0) => return Err("the archive shrank while reading".into()),
                Ok(count) => count,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(format!("read the archive: {e}")),
            };
            self.hash
                .update(rest.get(..count).ok_or("archive read count")?);
            done += count;
            self.read += u64::try_from(count).map_err(|_| "archive read count")?;
        }
        Ok(())
    }

    fn expired(&self) -> Result<()> {
        if Instant::now() >= self.deadline {
            return Err("gave up reading the archive".into());
        }
        Ok(())
    }

    /// Reads `count` bytes through the scratch buffer; with `zero`, each
    /// must be NUL.
    fn pass(&mut self, mut count: u64, zero: bool) -> Result<()> {
        let mut scratch = std::mem::take(&mut self.scratch);
        let result = (|| {
            while count > 0 {
                let take = usize::try_from(count).map_or(CHUNK, |count| count.min(CHUNK));
                let chunk = scratch.get_mut(..take).ok_or("archive scratch")?;
                self.fill(chunk)?;
                if zero && chunk.iter().any(|byte| *byte != 0) {
                    return Err("the archive has bytes after its trailer".into());
                }
                count -= u64::try_from(take).map_err(|_| "archive scratch")?;
            }
            Ok(())
        })();
        self.scratch = scratch;
        result
    }

    /// The rest of the archive must be NUL padding, and nothing may follow.
    fn finish(&mut self) -> Result<()> {
        self.pass(self.size.saturating_sub(self.read), true)?;
        loop {
            self.expired()?;
            match self.source.read(&mut [0]) {
                Ok(0) => return Ok(()),
                Ok(_) => return Err("the archive grew while reading".into()),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(format!("read the archive: {e}")),
            }
        }
    }
}

/// One header field: eight hex digits.
fn field(header: &[u8], index: usize) -> Result<u32> {
    let start = MAGIC.len() + index * 8;
    let digits = header.get(start..start + 8).ok_or("short newc header")?;
    let mut value = 0u32;
    for digit in digits {
        let digit = char::from(*digit)
            .to_digit(16)
            .ok_or("a newc header field is not hex")?;
        value = value
            .checked_mul(16)
            .and_then(|value| value.checked_add(digit))
            .ok_or("newc header field overflow")?;
    }
    Ok(value)
}

/// What the zero to three bytes after `length` bytes from a member's start
/// pad to four.
fn padding(length: u64) -> u64 {
    (4 - length % 4) % 4
}

/// The marker's versions, or none, once the trailer and its padding end
/// the archive.
fn scan<R: Read>(stream: &mut Stream<'_, R>) -> Result<Option<Vec<u8>>> {
    let mut header = [0u8; NEWC_HEADER];
    let mut name = vec![0u8; NAME_LIMIT];
    let mut marker = None;
    let mut members = 0usize;
    loop {
        stream.fill(&mut header)?;
        if header.get(..MAGIC.len()) != Some(MAGIC) {
            return Err("a member does not start with the newc magic 070701".into());
        }
        let mode = field(&header, 1)?;
        let links = field(&header, 4)?;
        let length = u64::from(field(&header, 6)?);
        let name_size = usize::try_from(field(&header, 11)?).map_err(|_| "newc name size")?;
        if !(2..=NAME_LIMIT).contains(&name_size) {
            return Err("a member's name is empty or longer than 4096 bytes".into());
        }
        let name = name.get_mut(..name_size).ok_or("newc name buffer")?;
        stream.fill(name)?;
        let Some((0, name)) = name.split_last() else {
            return Err("a member's name does not end in NUL".into());
        };
        if name.contains(&0) {
            return Err("a member's name holds a NUL".into());
        }
        let named = u64::try_from(NEWC_HEADER + name_size).map_err(|_| "newc name size")?;
        stream.pass(padding(named), false)?;
        if name == TRAILER {
            stream.finish()?;
            return Ok(marker);
        }
        members += 1;
        if members > MEMBER_LIMIT {
            return Err("the archive has more than 65536 members".into());
        }
        if name == MEMBER {
            if marker.is_some() {
                return Err("the archive carries two tier markers".into());
            }
            if mode & FORMAT != REGULAR || links != 1 {
                return Err("the tier marker is not a single-link regular file".into());
            }
            let size = usize::try_from(length)
                .ok()
                .filter(|size| *size <= MARKER_LIMIT)
                .ok_or("the tier marker is longer than 256 bytes")?;
            let mut bytes = [0u8; MARKER_LIMIT];
            let bytes = bytes.get_mut(..size).ok_or("tier marker buffer")?;
            stream.fill(bytes)?;
            marker = Some(parse(bytes).ok_or("the tier marker is malformed")?);
        } else {
            stream.pass(length, false)?;
        }
        stream.pass(padding(length), false)?;
    }
}

#[cfg(test)]
pub(crate) mod tests {
    #![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
    use super::*;
    use std::os::fd::OwnedFd;
    use std::os::unix::fs::{symlink, FileTypeExt};
    use std::sync::atomic::{AtomicU64, Ordering};

    /// The budget the tests read within when not testing it.
    const BUDGET: Duration = Duration::from_secs(10);

    /// One newc member to pack.
    #[derive(Clone, Copy)]
    pub(crate) struct Member<'a> {
        pub(crate) name: &'a [u8],
        pub(crate) mode: u32,
        pub(crate) links: u32,
        pub(crate) data: &'a [u8],
    }

    pub(crate) fn file<'a>(name: &'a [u8], data: &'a [u8]) -> Member<'a> {
        Member {
            name,
            mode: 0o100444,
            links: 1,
            data,
        }
    }

    fn pad(bytes: &mut Vec<u8>) {
        while !bytes.len().is_multiple_of(4) {
            bytes.push(0);
        }
    }

    /// One member's header, name and data, as gen_init_cpio writes them.
    fn pack(bytes: &mut Vec<u8>, member: Member<'_>) {
        bytes.extend_from_slice(MAGIC);
        for value in [
            1,
            member.mode,
            0,
            0,
            member.links,
            1,
            member.data.len() as u32,
            0,
            0,
            0,
            0,
            member.name.len() as u32 + 1,
            0,
        ] {
            bytes.extend_from_slice(format!("{value:08X}").as_bytes());
        }
        bytes.extend_from_slice(member.name);
        bytes.push(0);
        pad(bytes);
        bytes.extend_from_slice(member.data);
        pad(bytes);
    }

    /// `members`, then the trailer, padded to 512 bytes as gen_init_cpio does.
    pub(crate) fn archive(members: &[Member<'_>]) -> Vec<u8> {
        let mut bytes = Vec::new();
        for member in members {
            pack(&mut bytes, *member);
        }
        pack(&mut bytes, file(TRAILER, b""));
        while !bytes.len().is_multiple_of(512) {
            bytes.push(0);
        }
        bytes
    }

    /// A marker's bytes for `versions`.
    pub(crate) fn marker(versions: &[u8]) -> Vec<u8> {
        let mut bytes = HEADER.to_vec();
        for version in versions {
            bytes.extend_from_slice(format!(" {version}").as_bytes());
        }
        bytes.push(b'\n');
        bytes
    }

    /// A deployment initramfs's shape, with the marker when one is given.
    pub(crate) fn initramfs(tier: Option<&[u8]>) -> Vec<u8> {
        let directory = |name| Member {
            name,
            mode: 0o040755,
            links: 2,
            data: b"",
        };
        let mut members = vec![
            directory(b"bin"),
            file(b"init", b"#!/bin/sh\nexec /bin/switch_root\n"),
            directory(b"etc"),
        ];
        if let Some(tier) = tier {
            members.push(file(MEMBER, tier));
        }
        members.push(file(b"bin/td-boot", &[0x7f; 777]));
        archive(&members)
    }

    /// The four-line manifest for `archive`.
    pub(crate) fn manifest(archive: &[u8]) -> Vec<u8> {
        format!(
            "td-deployment-v1\n{}  bzImage\n{}  initramfs.cpio\n{}  root.erofs\n",
            sha256::hex_digest(b"kernel"),
            sha256::hex_digest(archive),
            sha256::hex_digest(b"root"),
        )
        .into_bytes()
    }

    /// Writes `archive` and its manifest into `directory`; the deployment ID.
    pub(crate) fn bundle(directory: &Path, archive: &[u8]) -> String {
        let manifest = manifest(archive);
        fs::write(directory.join(MANIFEST), &manifest).unwrap();
        fs::write(directory.join(ARCHIVE), archive).unwrap();
        sha256::hex_digest(&manifest)
    }

    /// A temporary directory owned by the test's own UID, removed on drop.
    pub(crate) struct Scratch {
        pub(crate) path: PathBuf,
        pub(crate) owner: u32,
    }

    impl Scratch {
        pub(crate) fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "td-login-tier-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            let owner = fs::metadata(&path).unwrap().uid();
            Self { path, owner }
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    /// Makes a FIFO at `path` where the host has `mkfifo`; std's own is
    /// unstable.
    pub(crate) fn fifo(path: &Path) -> bool {
        std::process::Command::new("mkfifo")
            .arg(path)
            .status()
            .is_ok_and(|status| status.success())
    }

    /// The scanner alone over `bytes`, with its digest.
    fn scanned(bytes: &[u8]) -> Result<Vec<u8>> {
        let digest = sha256::hex_digest(bytes);
        let size = bytes.len() as u64;
        verify(&mut &bytes[..], size, &digest, deadline(BUDGET)?, || {
            Ok(size)
        })
    }

    fn refused(why: &str) -> Result<Vec<u8>> {
        Err(why.into())
    }

    #[test]
    fn the_grammar_admits_exactly_one_spelling_of_each_version_list() {
        assert_eq!(parse(b"td-login-tier-v1 1\n"), Some(vec![1]));
        assert_eq!(
            parse(b"td-login-tier-v1 1 2 9 10 99 255\n"),
            Some(vec![1, 2, 9, 10, 99, 255])
        );
        assert_eq!(parse(&marker(&[3, 200])), Some(vec![3, 200]));
        for bytes in [
            &b"td-login-tier-v1\n"[..],
            b"td-login-tier-v1 \n",
            b"td-login-tier-v1 1",
            b"td-login-tier-v1 1\n\n",
            b"td-login-tier-v1 1\r\n",
            b"td-login-tier-v1  1\n",
            b"td-login-tier-v1 1 \n",
            b"td-login-tier-v1 1  2\n",
            b" td-login-tier-v1 1\n",
            b"td-login-tier-v11\n",
            b"td-login-tier-v2 1\n",
            b"TD-LOGIN-TIER-V1 1\n",
            b"td-login-tier-v1 0\n",
            b"td-login-tier-v1 01\n",
            b"td-login-tier-v1 001\n",
            b"td-login-tier-v1 256\n",
            b"td-login-tier-v1 1000\n",
            b"td-login-tier-v1 +1\n",
            b"td-login-tier-v1 -1\n",
            b"td-login-tier-v1 0x1\n",
            b"td-login-tier-v1 2 1\n",
            b"td-login-tier-v1 1 1\n",
            b"td-login-tier-v1 1\t2\n",
            b"td-login-tier-v1 1\n2\n",
            b"td-login-tier-v1 1\0\n",
            b"",
        ] {
            assert_eq!(parse(bytes), None, "{bytes:?}");
        }
    }

    #[test]
    fn well_formed_archives_yield_their_marker() {
        assert_eq!(scanned(&initramfs(Some(&marker(&[1])))), Ok(vec![1]));
        assert_eq!(scanned(&initramfs(Some(&marker(&[1, 2])))), Ok(vec![1, 2]));
        // Every marker length modulo four pads to the next member.
        for versions in [&[1][..], &[1, 2], &[1, 2, 3], &[1, 2, 3, 40]] {
            let tier = marker(versions);
            let bytes = archive(&[file(MEMBER, &tier), file(b"after", b"x")]);
            assert_eq!(scanned(&bytes), Ok(versions.to_vec()));
        }
        // More NULs may follow the trailer's 512-byte padding.
        let mut longer = initramfs(Some(&marker(&[1])));
        longer.extend_from_slice(&[0; 4096]);
        assert_eq!(scanned(&longer), Ok(vec![1]));
    }

    #[test]
    fn a_missing_duplicated_odd_or_malformed_marker_reads_nothing() {
        let none = refused("the archive carries no tier marker");
        assert_eq!(scanned(&initramfs(None)), none);
        // Another spelling of the name is not the marker.
        let tier = marker(&[1]);
        for name in [
            &b"/etc/td-login-tier"[..],
            b"./etc/td-login-tier",
            b"etc/td-login-tier/",
            b"td-login-tier",
        ] {
            assert_eq!(scanned(&archive(&[file(name, &tier)])), none);
        }
        let twice = archive(&[file(MEMBER, &tier), file(MEMBER, &tier)]);
        assert_eq!(
            scanned(&twice),
            refused("the archive carries two tier markers")
        );
        for (mode, links) in [(0o040755, 2), (0o120777, 1), (0o010644, 1), (0o100444, 2)] {
            let odd = archive(&[Member {
                name: MEMBER,
                mode,
                links,
                data: &tier,
            }]);
            assert_eq!(
                scanned(&odd),
                refused("the tier marker is not a single-link regular file"),
                "{mode:o}"
            );
        }
        let malformed = archive(&[file(MEMBER, b"td-login-tier-v1 01\n")]);
        assert_eq!(scanned(&malformed), refused("the tier marker is malformed"));
    }

    #[test]
    fn a_bad_magic_trailing_bytes_or_a_truncated_archive_reads_nothing() {
        let good = initramfs(Some(&marker(&[1])));
        // The crc variant, and the old ASCII format.
        for magic in [&b"070702"[..], b"070707"] {
            let mut bytes = good.clone();
            bytes[..6].copy_from_slice(magic);
            assert_eq!(
                scanned(&bytes),
                refused("a member does not start with the newc magic 070701")
            );
        }
        // A header field that is not hex.
        let mut bytes = good.clone();
        bytes[6 + 6 * 8] = b'G';
        assert_eq!(scanned(&bytes), refused("a newc header field is not hex"));
        // A byte after the trailer that is not padding, or a second archive.
        let trailing = refused("the archive has bytes after its trailer");
        let mut bytes = good.clone();
        *bytes.last_mut().unwrap() = 1;
        assert_eq!(scanned(&bytes), trailing);
        let mut bytes = good.clone();
        bytes.extend(archive(&[file(MEMBER, &marker(&[2]))]));
        assert_eq!(scanned(&bytes), trailing);
        // Cut at each member boundary, inside each header, and anywhere
        // else before the trailer's name ends.
        let trailer = good
            .windows(TRAILER.len())
            .position(|window| window == TRAILER)
            .unwrap();
        let boundaries: Vec<usize> = (0..good.len())
            .step_by(4)
            .filter(|at| good[*at..].starts_with(MAGIC))
            .collect();
        assert_eq!(boundaries.len(), 6);
        let mut cuts = vec![200, 300, trailer, trailer + TRAILER.len()];
        for at in &boundaries {
            cuts.extend([*at, at + 1, at + NEWC_HEADER - 1, at + NEWC_HEADER]);
        }
        for cut in cuts {
            assert_eq!(
                scanned(&good[..cut]),
                refused("the archive ends inside a member"),
                "{cut}"
            );
        }
        // A name that does not end in its NUL, or holds one.
        let mut bytes = archive(&[file(b"abc", b"")]);
        bytes[110 + 3] = b'd';
        assert_eq!(
            scanned(&bytes),
            refused("a member's name does not end in NUL")
        );
        let mut bytes = archive(&[file(b"abc", b"")]);
        bytes[110 + 1] = 0;
        assert_eq!(scanned(&bytes), refused("a member's name holds a NUL"));
    }

    #[test]
    fn names_members_and_the_marker_are_bounded() {
        let tier = marker(&[1]);
        // A name field of 4096 bytes with its NUL, one past it, and empty.
        let named = refused("a member's name is empty or longer than 4096 bytes");
        let long = vec![b'n'; NAME_LIMIT - 1];
        assert_eq!(
            scanned(&archive(&[file(&long, b"x"), file(MEMBER, &tier)])),
            Ok(vec![1])
        );
        let longer = vec![b'n'; NAME_LIMIT];
        assert_eq!(
            scanned(&archive(&[file(&longer, b"x"), file(MEMBER, &tier)])),
            named
        );
        let mut empty = archive(&[file(b"x", b"")]);
        empty[94..102].copy_from_slice(b"00000001");
        assert_eq!(scanned(&empty), named);
        // 65536 members before the trailer, the marker among them, then one
        // more.
        let names: Vec<String> = (0..MEMBER_LIMIT).map(|n| format!("m{n:05}")).collect();
        let mut members: Vec<Member<'_>> = names.iter().map(|n| file(n.as_bytes(), b"")).collect();
        members[MEMBER_LIMIT / 2] = file(MEMBER, &tier);
        assert_eq!(scanned(&archive(&members)), Ok(vec![1]));
        members.push(file(b"one-more", b""));
        assert_eq!(
            scanned(&archive(&members)),
            refused("the archive has more than 65536 members")
        );
        // A marker of 256 bytes, then a well-formed one of 257.
        let at_most: Vec<u8> = (1..=80).chain([100, 101]).collect();
        assert_eq!(marker(&at_most).len(), MARKER_LIMIT);
        assert_eq!(
            scanned(&archive(&[file(MEMBER, &marker(&at_most))])),
            Ok(at_most)
        );
        let past: Vec<u8> = (1..=83).collect();
        assert_eq!(marker(&past).len(), MARKER_LIMIT + 1);
        assert_eq!(parse(&marker(&past)), Some(past.clone()));
        assert_eq!(
            scanned(&archive(&[file(MEMBER, &marker(&past))])),
            refused("the tier marker is longer than 256 bytes")
        );
    }

    /// Reads through `inner`, running `meanwhile` once, at the first read.
    struct Meddling<'a, R> {
        inner: R,
        meanwhile: Option<&'a dyn Fn()>,
    }

    impl<R: Read> Read for Meddling<'_, R> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if let Some(meanwhile) = self.meanwhile.take() {
                meanwhile();
            }
            self.inner.read(buf)
        }
    }

    /// Hands out at most 16 bytes per read, each after 20 ms.
    struct Slow<'a>(&'a [u8]);

    impl Read for Slow<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            std::thread::sleep(Duration::from_millis(20));
            let count = buf.len().min(self.0.len()).min(16);
            buf[..count].copy_from_slice(&self.0[..count]);
            self.0 = &self.0[count..];
            Ok(count)
        }
    }

    /// Where a [`Lagging`] source stalls.
    #[derive(Clone, Copy)]
    enum Stall {
        /// The read that hands out the last bytes sleeps 300 ms first.
        LastRead,
        /// The probe past the end sleeps 300 ms, then finds nothing.
        Probe,
        /// The probe past the end is interrupted until this instant.
        Interrupted(Instant),
    }

    /// Hands out its bytes at once, stalling only as `stall` says.
    struct Lagging<'a> {
        bytes: &'a [u8],
        stall: Stall,
    }

    impl Read for Lagging<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let pause = Duration::from_millis(300);
            let count = buf.len().min(self.bytes.len());
            match self.stall {
                Stall::LastRead if count > 0 && count == self.bytes.len() => {
                    std::thread::sleep(pause)
                }
                Stall::Probe if count == 0 => std::thread::sleep(pause),
                Stall::Interrupted(until) if count == 0 && Instant::now() < until => {
                    return Err(io::ErrorKind::Interrupted.into())
                }
                _ => {}
            }
            buf[..count].copy_from_slice(&self.bytes[..count]);
            self.bytes = &self.bytes[count..];
            Ok(count)
        }
    }

    #[test]
    fn a_budget_spent_by_the_last_read_or_the_probe_reads_nothing() {
        let bytes = initramfs(Some(&marker(&[1])));
        let digest = sha256::hex_digest(&bytes);
        let size = bytes.len() as u64;
        let run = |stall, budget| {
            let started = Instant::now();
            let deadline = started + budget;
            let mut source = Lagging {
                bytes: &bytes,
                stall,
            };
            let result = verify(&mut source, size, &digest, deadline, || Ok(size));
            (result, started.elapsed())
        };
        let late = refused("gave up reading the archive");
        let ample = Duration::from_secs(10);
        let short = Duration::from_millis(100);
        // Each stall alone is harmless inside the budget. The interrupted
        // probe goes first, while its 50 ms of interruptions still last.
        let interrupted = Stall::Interrupted(Instant::now() + Duration::from_millis(50));
        for stall in [interrupted, Stall::LastRead, Stall::Probe] {
            assert_eq!(run(stall, ample).0, Ok(vec![1]));
        }
        // The last bytes, or the end, arrive after the budget.
        assert_eq!(run(Stall::LastRead, short).0, late);
        assert_eq!(run(Stall::Probe, short).0, late);
        // An endlessly interrupted probe gives up at the budget.
        let endless = Stall::Interrupted(Instant::now() + Duration::from_secs(5));
        let (result, took) = run(endless, short);
        assert_eq!(result, late);
        assert!(took < Duration::from_secs(2), "{took:?}");
    }

    #[test]
    fn a_size_change_while_reading_or_a_late_read_reads_nothing() {
        let scratch = Scratch::new();
        let path = scratch.path.join("archive");
        let bytes = initramfs(Some(&marker(&[1])));
        let digest = sha256::hex_digest(&bytes);
        let size = bytes.len() as u64;
        let run = |meanwhile: &dyn Fn()| {
            fs::write(&path, &bytes).unwrap();
            let file = File::open(&path).unwrap();
            let mut source = Meddling {
                inner: &file,
                meanwhile: Some(meanwhile),
            };
            verify(
                &mut source,
                size,
                &digest,
                deadline(BUDGET).unwrap(),
                || file.metadata().map(|meta| meta.len()),
            )
        };
        assert_eq!(run(&|| ()), Ok(vec![1]));
        // Grown: the probe after the padding finds the new bytes.
        let grow = || {
            let mut append = OpenOptions::new().append(true).open(&path).unwrap();
            io::Write::write_all(&mut append, &[0; 512]).unwrap();
        };
        assert_eq!(run(&grow), refused("the archive grew while reading"));
        // Shrunk: the read ends before the size it was admitted at.
        let shrink = || {
            OpenOptions::new()
                .write(true)
                .open(&path)
                .unwrap()
                .set_len(size / 2)
                .unwrap();
        };
        assert_eq!(run(&shrink), refused("the archive shrank while reading"));
        // A size that disagrees once the read ends.
        assert_eq!(
            verify(
                &mut &bytes[..],
                size,
                &digest,
                deadline(BUDGET).unwrap(),
                || Ok(size + 1)
            ),
            refused("the archive changed size while reading")
        );
        // A deadline already gone, and one a slow source outlives.
        let late = refused("gave up reading the archive");
        assert_eq!(
            verify(&mut &bytes[..], size, &digest, Instant::now(), || Ok(size)),
            late
        );
        let deadline = Instant::now() + Duration::from_millis(100);
        assert_eq!(
            verify(&mut Slow(&bytes), size, &digest, deadline, || Ok(size)),
            late
        );
    }

    #[test]
    fn the_manifest_must_hash_to_the_id_and_the_archive_to_the_manifest() {
        let scratch = Scratch::new();
        let tier = marker(&[1]);
        let id = bundle(&scratch.path, &initramfs(Some(&tier)));
        let directory = open_directory(&scratch.path).unwrap();
        assert_eq!(read(&directory, &id, scratch.owner, BUDGET), Ok(vec![1]));
        // The budget is the caller's.
        assert_eq!(
            read(&directory, &id, scratch.owner, Duration::ZERO),
            refused("gave up reading the archive")
        );
        // The ID read against is the caller's, not the manifest's own.
        let unhashed = refused("the manifest does not hash to the deployment ID");
        let other = sha256::hex_digest(b"another manifest");
        assert_eq!(read(&directory, &other, scratch.owner, BUDGET), unhashed);
        for id in [id.to_uppercase(), id[..63].to_string(), format!("{id}0")] {
            assert_eq!(
                read(&directory, &id, scratch.owner, BUDGET),
                refused("invalid deployment ID")
            );
        }
        // An archive that is not the manifest's, its marker intact.
        let mut changed = initramfs(Some(&tier));
        let at = changed.iter().rposition(|byte| *byte == 0x7f).unwrap();
        changed[at] = 0x7e;
        fs::write(scratch.path.join(ARCHIVE), &changed).unwrap();
        assert_eq!(scanned(&changed), Ok(vec![1]));
        assert_eq!(
            read(&directory, &id, scratch.owner, BUDGET),
            refused("the archive does not hash to the manifest's entry")
        );
        // The manifest is td-boot's exact four lines.
        let archive = initramfs(Some(&tier));
        let digest = sha256::hex_digest(&archive);
        fs::write(scratch.path.join(ARCHIVE), &archive).unwrap();
        let good = String::from_utf8(manifest(&archive)).unwrap();
        assert_eq!(initramfs_entry(good.as_bytes()), Some(digest.as_str()));
        for bad in [
            good.replace("td-deployment-v1", "td-deployment-v2"),
            good.replace("  initramfs.cpio", " initramfs.cpio"),
            good.replace("initramfs.cpio", "initramfs.cpi0"),
            good.replace(&digest, &digest.to_uppercase()),
            format!("{good}extra\n"),
            format!("{good}\n"),
            good.trim_end().to_string(),
            good.replace("  bzImage", "  kernel"),
            good.replace("  root.erofs", "  root.img"),
        ] {
            fs::write(scratch.path.join(MANIFEST), &bad).unwrap();
            let id = sha256::hex_digest(bad.as_bytes());
            assert_eq!(
                read(&directory, &id, scratch.owner, BUDGET),
                refused("the manifest is not td-deployment-v1"),
                "{bad}"
            );
        }
    }

    #[test]
    fn each_file_is_a_bounded_regular_file_of_its_owner() {
        let scratch = Scratch::new();
        let archive = initramfs(Some(&marker(&[1])));
        let id = bundle(&scratch.path, &archive);
        let directory = open_directory(&scratch.path).unwrap();
        let read = || read(&directory, &id, scratch.owner, BUDGET);
        assert_eq!(read(), Ok(vec![1]));
        assert_eq!(
            super::read(&directory, &id, scratch.owner.wrapping_add(1), BUDGET),
            refused("manifest has the wrong owner")
        );
        // Links to the right bytes are still links.
        for name in [MANIFEST, ARCHIVE] {
            let real = scratch.path.join(format!("{name}.real"));
            fs::rename(scratch.path.join(name), &real).unwrap();
            symlink(&real, scratch.path.join(name)).unwrap();
            assert!(
                read().is_err_and(|e| e.starts_with(&format!("open {name}: "))),
                "{name}"
            );
            fs::remove_file(scratch.path.join(name)).unwrap();
            fs::rename(&real, scratch.path.join(name)).unwrap();
        }
        assert_eq!(read(), Ok(vec![1]));
        let not_regular = refused("initramfs.cpio is not a regular file");
        // A directory in the archive's place.
        let path = scratch.path.join(ARCHIVE);
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        assert_eq!(read(), not_regular);
        fs::remove_dir(&path).unwrap();
        // A FIFO opens without waiting for a writer and is refused by type.
        let (reader, _writer) = io::pipe().unwrap();
        let meta = File::from(OwnedFd::from(reader)).metadata().unwrap();
        assert!(meta.file_type().is_fifo());
        assert_eq!(
            admit(&meta, meta.uid(), ARCHIVE_LIMIT),
            Err("is not a regular file")
        );
        if fifo(&path) {
            assert_eq!(read(), not_regular);
            fs::remove_file(&path).unwrap();
        }
        // Empty, and past the bound, refused before any byte is read: a
        // sparse file holds the size without the bytes.
        let bounded = refused("initramfs.cpio is empty or larger than its bound");
        fs::write(&path, b"").unwrap();
        assert_eq!(read(), bounded);
        File::create(&path)
            .unwrap()
            .set_len(ARCHIVE_LIMIT + 1)
            .unwrap();
        assert_eq!(read(), bounded);
        assert_eq!(ARCHIVE_LIMIT, 512 * 1024 * 1024);
        // At the bound it is admitted and read, failing on its NULs.
        File::create(&path).unwrap().set_len(ARCHIVE_LIMIT).unwrap();
        assert_eq!(
            read(),
            refused("a member does not start with the newc magic 070701")
        );
        // A manifest of 4096 bytes is admitted, read whole and hashed to
        // its ID, then refused by its form; one of 4097 is not admitted.
        let full = [b'x'; MANIFEST_LIMIT as usize];
        fs::write(scratch.path.join(MANIFEST), full).unwrap();
        assert_eq!(
            super::read(
                &directory,
                &sha256::hex_digest(&full),
                scratch.owner,
                BUDGET
            ),
            refused("the manifest is not td-deployment-v1")
        );
        fs::write(scratch.path.join(MANIFEST), [b'x'; 4097]).unwrap();
        assert_eq!(
            read(),
            refused("manifest is empty or larger than its bound")
        );
    }

    /// A volume holding `deployments/` and `boot/`, the test's own.
    pub(crate) struct Volume {
        pub(crate) scratch: Scratch,
    }

    impl Volume {
        pub(crate) fn new() -> Self {
            let scratch = Scratch::new();
            fs::create_dir(scratch.path.join("deployments")).unwrap();
            fs::create_dir(scratch.path.join("boot")).unwrap();
            Self { scratch }
        }

        pub(crate) fn path(&self) -> &Path {
            &self.scratch.path
        }

        /// A deployment whose initramfs carries `tier`; its ID.
        pub(crate) fn deploy(&self, tier: Option<&[u8]>) -> String {
            let staging = self.path().join("staging");
            fs::create_dir(&staging).unwrap();
            let id = bundle(&staging, &initramfs(tier));
            let named = self.path().join("deployments").join(&id);
            if named.exists() {
                fs::remove_dir_all(&staging).unwrap();
            } else {
                fs::rename(&staging, named).unwrap();
            }
            id
        }

        /// Points `slot` at `target`, a selector's link text.
        pub(crate) fn select(&self, slot: &str, target: &str) {
            let link = self.path().join("boot").join(slot);
            let _ = fs::remove_file(&link);
            symlink(target, link).unwrap();
        }

        /// Points both selectors at deployments carrying these markers.
        pub(crate) fn retain(&self, current: Option<&[u8]>, previous: Option<&[u8]>) {
            let current = self.deploy(current);
            self.select(CURRENT, &format!("../deployments/{current}"));
            let previous = self.deploy(previous);
            self.select(PREVIOUS, &format!("../deployments/{previous}"));
        }
    }

    /// Both selectors are read through the one held volume, whatever its
    /// path names afterwards; only the two selectors, each only as
    /// `../deployments/<64 hex>`.
    #[test]
    fn selectors_are_read_through_the_held_volume() {
        let volume = Volume::new();
        let one = volume.deploy(None);
        let two = volume.deploy(Some(&marker(&[1])));
        volume.select(CURRENT, &format!("../deployments/{two}"));
        volume.select(PREVIOUS, &format!("../deployments/{one}"));
        let held = open_volume(volume.path()).unwrap();
        let moved = volume.path().with_extension("moved");
        fs::rename(volume.path(), &moved).unwrap();
        assert_eq!(selected(&held, CURRENT), Ok(two.clone()));
        assert_eq!(selected(&held, PREVIOUS), Ok(one.clone()));
        assert_eq!(
            selected(&held, "attempts"),
            Err("\"attempts\" is not a deployment selector".into())
        );
        let link = moved.join("boot").join(CURRENT);
        fs::remove_file(&link).unwrap();
        symlink(format!("../deployments/{two}/"), &link).unwrap();
        assert_eq!(
            selected(&held, CURRENT),
            Err("the current selector does not name ../deployments/<id>".into())
        );
        fs::remove_file(&link).unwrap();
        assert!(
            selected(&held, CURRENT).is_err_and(|e| e.starts_with("read the current selector: "))
        );
        fs::rename(&moved, volume.path()).unwrap();
        assert!(open_volume(&volume.path().join("absent")).is_err());
    }

    #[test]
    fn retained_deployments_are_read_through_their_selectors() {
        let volume = Volume::new();
        let owner = volume.scratch.owner;
        let one = volume.deploy(Some(&marker(&[1])));
        let both = volume.deploy(Some(&marker(&[1, 2])));
        let none = volume.deploy(None);
        let reads = |slot| retained(volume.path(), slot, owner, BUDGET);
        // Both marked.
        volume.select(CURRENT, &format!("../deployments/{both}"));
        volume.select(PREVIOUS, &format!("../deployments/{one}"));
        assert_eq!(reads(CURRENT), Ok(vec![1, 2]));
        assert_eq!(reads(PREVIOUS), Ok(vec![1]));
        // One without the marker.
        volume.select(PREVIOUS, &format!("../deployments/{none}"));
        assert_eq!(
            reads(PREVIOUS),
            refused("the archive carries no tier marker")
        );
        assert_eq!(reads(CURRENT), Ok(vec![1, 2]));
        // Root's files only, in production.
        assert_eq!(
            retained(volume.path(), CURRENT, owner.wrapping_add(1), BUDGET),
            refused("manifest has the wrong owner")
        );
        // Only the two selectors, and only `../deployments/<64 hex>`.
        for slot in ["attempts", "../boot/current", ""] {
            assert_eq!(
                retained(volume.path(), slot, owner, BUDGET),
                Err(format!("{slot:?} is not a deployment selector"))
            );
        }
        for target in [
            format!("/deployments/{both}"),
            format!("deployments/{both}"),
            format!("../deployments/{both}/"),
            format!("../deployments/./{both}"),
            format!("../deployments/{}", both.to_uppercase()),
            format!("../deployments/{}", &both[..63]),
            format!("../deployments/{both}0"),
            "../deployments/".to_string(),
        ] {
            volume.select(CURRENT, &target);
            assert_eq!(
                reads(CURRENT),
                refused("the current selector does not name ../deployments/<id>"),
                "{target}"
            );
        }
        // A selector that is not a link at all.
        let selector = volume.path().join("boot").join(CURRENT);
        fs::remove_file(&selector).unwrap();
        fs::write(&selector, both.as_bytes()).unwrap();
        assert!(reads(CURRENT).is_err_and(|e| e.starts_with("read the current selector: ")));
        fs::remove_file(&selector).unwrap();
        // Named by a selector, but holding another deployment's files.
        let named = volume.path().join("deployments").join(&one);
        let held = volume.path().join("held");
        fs::rename(&named, &held).unwrap();
        fs::create_dir(&named).unwrap();
        bundle(&named, &initramfs(Some(&marker(&[1, 2, 3]))));
        volume.select(CURRENT, &format!("../deployments/{one}"));
        assert_eq!(
            reads(CURRENT),
            refused("the manifest does not hash to the deployment ID")
        );
        fs::remove_dir_all(&named).unwrap();
        // A deployment directory that is a link to the right one.
        symlink(&held, &named).unwrap();
        assert!(reads(CURRENT).is_err_and(|e| e.starts_with("open ")));
        fs::remove_file(&named).unwrap();
        fs::rename(&held, &named).unwrap();
        assert_eq!(reads(CURRENT), Ok(vec![1]));
        // A FIFO in a deployment's archive place.
        let archive = named.join(ARCHIVE);
        fs::remove_file(&archive).unwrap();
        if fifo(&archive) {
            assert_eq!(
                reads(CURRENT),
                refused("initramfs.cpio is not a regular file")
            );
        }
        // No volume, as on a machine that has none; and a volume path that
        // is a link.
        assert!(retained(&volume.path().join("absent"), PREVIOUS, owner, BUDGET).is_err());
        let link = volume.path().join("link");
        symlink(volume.path(), &link).unwrap();
        volume.select(PREVIOUS, &format!("../deployments/{both}"));
        assert!(retained(&link, PREVIOUS, owner, BUDGET).is_err());
        assert_eq!(
            retained(volume.path(), PREVIOUS, owner, BUDGET),
            Ok(vec![1, 2])
        );
        assert_eq!(VOLUME, "/run/td-volume/td");
    }
}

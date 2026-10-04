//! The diagnostics export (DESIGN.md §4): one ustar archive of td-agent's
//! state directory, its configuration file and a manifest, for the human
//! to attach to a report. The key file and its neighbours are never read,
//! and any file holding a key is left out. The archive is written whole,
//! the owner's alone, and then compressed into a file of its own with the
//! host's `zstd`, else `gzip`, when one is on `PATH`.

use std::fs::{File, Metadata};
use std::io::{BufWriter, Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::key::Secret;

const BLOCK: usize = 512;
const ZEROS: [u8; BLOCK] = [0; BLOCK];
/// open(2) flags std does not name, as key.rs has them.
const O_NOFOLLOW: i32 = 0o400000;
const O_NONBLOCK: i32 = 0o4000;
const O_NOCTTY: i32 = 0o400;
/// The archive's top directory.
const TOP: &str = "td-agent-diagnostics";
/// A file the export is writing, not yet in place.
const PART: &str = "part";
/// How deep beneath the state directory files are looked for.
const MAX_DEPTH: usize = 4;
/// The most names one walk of the state directory considers.
const MAX_ENTRIES: usize = 100_000;

/// The export's bounds.
#[derive(Clone, Copy)]
struct Limits {
    /// The largest file taken: a log's bound.
    file: u64,
    /// The most bytes of files one archive takes.
    total: u64,
    /// The most names the walk considers, taken or left out.
    entries: usize,
}

const LIMITS: Limits = Limits {
    file: crate::store::MAX_LOG,
    total: 1 << 30,
    entries: MAX_ENTRIES,
};

/// A host program that writes what it reads, compressed, to its output.
pub struct Compressor {
    pub program: &'static str,
    pub args: &'static [&'static str],
    pub suffix: &'static str,
}

/// zstd, then gzip, each making `x.tar.<suffix>`.
pub const COMPRESSORS: &[Compressor] = &[
    Compressor {
        program: "zstd",
        args: &["-q", "-c"],
        suffix: "zst",
    },
    Compressor {
        program: "gzip",
        args: &["-q", "-c"],
        suffix: "gz",
    },
];

/// What is exported, and what never is.
pub struct Sources {
    /// The state directory, taken whole.
    pub state: PathBuf,
    /// The configuration file, when there is one to look for.
    pub config: Option<PathBuf>,
    /// The key file: it, its temporary, any link to either and the
    /// directory holding them are never taken.
    pub key_file: Option<PathBuf>,
    /// Every key to look for: no file holding one is taken.
    pub keys: Vec<Secret>,
    /// Why the stored key could not be read to look for, when it could
    /// not.
    pub key_problem: Option<String>,
}

/// What an export made.
#[derive(Debug)]
pub struct Exported {
    pub path: PathBuf,
    pub files: usize,
    pub left_out: usize,
    /// What did not go as asked: why the archive is not compressed, or
    /// that the uncompressed one could not be removed.
    pub remark: Option<String>,
}

/// Exports `sources` into directory `into` at `now`, compressing with the
/// first of `compressors` the host has and that works.
pub fn export(
    sources: &Sources,
    into: &Path,
    now: u64,
    compressors: &[Compressor],
) -> Result<Exported, String> {
    export_within(sources, into, now, compressors, LIMITS)
}

fn export_within(
    sources: &Sources,
    into: &Path,
    now: u64,
    compressors: &[Compressor],
    limits: Limits,
) -> Result<Exported, String> {
    let stamp: String = crate::history::utc(now)
        .chars()
        .filter(|c| !matches!(c, '-' | ':'))
        .collect();
    let base = free_name(into, &stamp, compressors)?;
    let archive = with_suffix(&base, "tar");
    let part = with_suffix(&archive, PART);
    let file = create(&part)?;
    let written = write(sources, file, now, limits)
        .and_then(|counts| place(&part, &archive).map(|left| (counts, left)));
    let ((files, left_out), left) = match written {
        Ok(written) => written,
        Err(e) => {
            let _ = std::fs::remove_file(&part);
            return Err(format!("{}: {e}", archive.display()));
        }
    };
    let (path, compressed) = compress(archive, compressors);
    let remark = [left, compressed]
        .into_iter()
        .flatten()
        .reduce(|a, b| format!("{a}; {b}"));
    Ok(Exported {
        path,
        files,
        left_out,
        remark,
    })
}

/// `path` with `.suffix` added.
fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".");
    name.push(suffix);
    PathBuf::from(name)
}

/// The first name in `into` for this export that no file, finished or
/// partial, compressed or not, already has.
fn free_name(into: &Path, stamp: &str, compressors: &[Compressor]) -> Result<PathBuf, String> {
    (0..100)
        .map(|n| match n {
            0 => into.join(format!("{TOP}-{stamp}")),
            n => into.join(format!("{TOP}-{stamp}-{n}")),
        })
        .find(|base| {
            let tar = with_suffix(base, "tar");
            std::iter::once(tar.clone())
                .chain(compressors.iter().map(|c| with_suffix(&tar, c.suffix)))
                .flat_map(|path| [with_suffix(&path, PART), path])
                .all(|path| std::fs::symlink_metadata(path).is_err())
        })
        .ok_or_else(|| format!("{}: no free name for the archive", into.display()))
}

/// A new file at `path`, the owner's alone; an existing one, or a link,
/// is refused, not replaced.
fn create(path: &Path) -> Result<File, String> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| format!("{}: {e}", path.display()))
}

/// Puts the finished `part` at `path`, never replacing a file there: a
/// hard link, else, on a file system that has none (vfat, a CIFS share),
/// a rename once nothing is there. A `part` that then cannot be removed
/// is said, the file being in place.
fn place(part: &Path, path: &Path) -> Result<Option<String>, String> {
    const EPERM: i32 = 1;
    const EXDEV: i32 = 18;
    const EOPNOTSUPP: i32 = 95;
    match std::fs::hard_link(part, path) {
        Ok(()) => Ok(std::fs::remove_file(part)
            .err()
            .map(|e| format!("{} stays: {e}", part.display()))),
        Err(e) if matches!(e.raw_os_error(), Some(EPERM | EXDEV | EOPNOTSUPP)) => {
            if std::fs::symlink_metadata(path).is_ok() {
                return Err(format!("{}: it exists", path.display()));
            }
            std::fs::rename(part, path)
                .map(|()| None)
                .map_err(|e| format!("{}: {e}", path.display()))
        }
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// A file's identity, to know it under another name.
type Identity = (u64, u64);

fn identity(metadata: &Metadata) -> Identity {
    (metadata.dev(), metadata.ino())
}

/// What the export must never take: the key file's and its temporary's
/// identities, and the directory holding them.
struct Never {
    files: Vec<Identity>,
    directory: Option<Identity>,
}

impl Never {
    fn of(sources: &Sources) -> Self {
        let Some(key) = &sources.key_file else {
            return Self {
                files: Vec::new(),
                directory: None,
            };
        };
        let files = [key.clone(), with_tmp(key)]
            .iter()
            .filter_map(|path| std::fs::metadata(path).ok())
            .map(|m| identity(&m))
            .collect();
        let directory = key
            .parent()
            .and_then(|dir| std::fs::metadata(dir).ok())
            .map(|m| identity(&m));
        Self { files, directory }
    }
}

/// The key file's temporary beside it.
fn with_tmp(key: &Path) -> PathBuf {
    key.with_file_name(crate::key::TEMPORARY)
}

/// A file found for the archive.
struct Found {
    name: String,
    path: PathBuf,
    /// Whether a final link is followed: the configuration is read as
    /// td-agent reads it, the state never through a link.
    follow: bool,
}

/// The walk's findings and its budget.
struct Walk {
    found: Vec<Found>,
    left_out: Vec<(String, String)>,
    budget: usize,
    beyond: usize,
}

impl Walk {
    /// Counts one name against the budget: false once it is spent.
    fn spend(&mut self) -> bool {
        match self.budget.checked_sub(1) {
            Some(rest) => {
                self.budget = rest;
                true
            }
            None => {
                self.beyond = self.beyond.saturating_add(1);
                false
            }
        }
    }
}

/// The archive into `file`: every file found, then the manifest. An
/// error writing it ends the export, since what is written is no longer
/// an archive.
fn write(
    sources: &Sources,
    file: File,
    now: u64,
    limits: Limits,
) -> Result<(usize, usize), String> {
    let never = Never::of(sources);
    let mut walk = Walk {
        found: Vec::new(),
        left_out: Vec::new(),
        budget: limits.entries,
        beyond: 0,
    };
    match std::fs::metadata(&sources.state) {
        Ok(m) if Some(identity(&m)) == never.directory => walk.left_out.push((
            "state/".into(),
            "it is the configuration directory, which holds the key".into(),
        )),
        _ => descend(&sources.state, "state", 0, &never, &mut walk),
    }
    walk.found.sort_by(|a, b| a.name.cmp(&b.name));
    if let Some(config) = &sources.config {
        match std::fs::symlink_metadata(config) {
            Ok(_) => walk.found.push(Found {
                name: "config".into(),
                path: config.clone(),
                follow: true,
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => walk.left_out.push(("config".into(), e.to_string())),
        }
    }
    let keys = keys(&sources.keys);
    let mut out = BufWriter::new(file);
    let mut taken: Vec<(String, u64)> = Vec::new();
    let mut total = 0u64;
    let Walk {
        found,
        mut left_out,
        beyond,
        ..
    } = walk;
    for found in found {
        let room = limits.total.saturating_sub(total);
        let (bytes, mtime) = match read_regular(&found, &never, limits.file, room) {
            Ok(read) => read,
            Err(e) => {
                left_out.push((found.name, e));
                continue;
            }
        };
        if keys.iter().any(|key| holds(&bytes, key.as_bytes())) {
            left_out.push((found.name, "it holds the API key".into()));
            continue;
        }
        let header = match header(&format!("{TOP}/{}", found.name), bytes.len() as u64, mtime) {
            Ok(header) => header,
            Err(e) => {
                left_out.push((found.name, e));
                continue;
            }
        };
        entry(&mut out, &header, &bytes)?;
        total = total.saturating_add(bytes.len() as u64);
        taken.push((found.name, bytes.len() as u64));
    }
    left_out.sort();
    let manifest = manifest(sources, now, &taken, &left_out, beyond);
    let header = header(&format!("{TOP}/MANIFEST"), manifest.len() as u64, now)?;
    entry(&mut out, &header, manifest.as_bytes())?;
    out.write_all(&[ZEROS, ZEROS].concat())
        .map_err(|e| e.to_string())?;
    let file = out.into_inner().map_err(|e| e.to_string())?;
    file.sync_all().map_err(|e| e.to_string())?;
    Ok((taken.len(), left_out.len()))
}

/// The regular files beneath `dir`, named from `name`. Links and other
/// kinds are left out, as are the key's files and directory, anything
/// deeper than `MAX_DEPTH`, and every name past the walk's budget.
fn descend(dir: &Path, name: &str, depth: usize, never: &Never, walk: &mut Walk) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) => return walk.left_out.push((format!("{name}/"), e.to_string())),
    };
    for entry in entries {
        if !walk.spend() {
            continue;
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                walk.left_out.push((format!("{name}/"), e.to_string()));
                continue;
            }
        };
        let Some(file) = entry.file_name().to_str().map(str::to_string) else {
            walk.left_out
                .push((format!("{name}/?"), "a name that is not UTF-8".into()));
            continue;
        };
        let named = format!("{name}/{file}");
        if file == crate::key::FILE || file == crate::key::TEMPORARY {
            walk.left_out
                .push((named, "it is named as the key file".into()));
            continue;
        }
        match entry.file_type() {
            Ok(kind) if kind.is_file() => walk.found.push(Found {
                name: named,
                path: entry.path(),
                follow: false,
            }),
            Ok(kind) if kind.is_dir() => {
                let key_directory = std::fs::symlink_metadata(entry.path())
                    .is_ok_and(|m| Some(identity(&m)) == never.directory);
                if depth == 0 && file == crate::workspace::JAIL {
                    walk.left_out.push((
                        named,
                        "the workspaces' jail directories hold the human's work".into(),
                    ));
                } else if key_directory {
                    walk.left_out.push((
                        named,
                        "it is the configuration directory, which holds the key".into(),
                    ));
                } else if depth < MAX_DEPTH {
                    descend(&entry.path(), &named, depth + 1, never, walk)
                } else {
                    walk.left_out.push((named, "too deep".into()))
                }
            }
            Ok(_) => walk.left_out.push((named, "not a regular file".into())),
            Err(e) => walk.left_out.push((named, e.to_string())),
        }
    }
}

/// `found`'s bytes and modification time, read through one descriptor
/// that waits on no FIFO: only a regular file that is not the key's, of
/// at most `limit` bytes and within the `room` left.
fn read_regular(
    found: &Found,
    never: &Never,
    limit: u64,
    room: u64,
) -> Result<(Vec<u8>, u64), String> {
    let flags = O_NONBLOCK | O_NOCTTY | if found.follow { 0 } else { O_NOFOLLOW };
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(flags)
        .open(&found.path)
        .map_err(|e| e.to_string())?;
    let metadata = file.metadata().map_err(|e| e.to_string())?;
    if !metadata.is_file() {
        return Err("not a regular file".into());
    }
    if never.files.contains(&identity(&metadata)) {
        return Err("it is the key file under another name".into());
    }
    if metadata.len() > limit {
        return Err(format!("longer than {limit} bytes"));
    }
    if metadata.len() > room {
        return Err("past the export's bound on bytes".into());
    }
    let bound = limit.min(room);
    let mut bytes = Vec::new();
    file.take(bound.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() as u64 > bound {
        return Err("it grew while it was read".into());
    }
    Ok((bytes, u64::try_from(metadata.mtime()).unwrap_or(0)))
}

/// Each key as stored and as a JSON string holds it, when escaping
/// changes it.
fn keys(secrets: &[Secret]) -> Vec<String> {
    let mut keys: Vec<String> = Vec::new();
    for secret in secrets {
        let key = secret.expose();
        let quoted = td_json::Json::Str(key.to_string()).to_string();
        let escaped = quoted
            .strip_prefix('"')
            .and_then(|q| q.strip_suffix('"'))
            .unwrap_or(key);
        for form in [key, escaped] {
            if !form.is_empty() && !keys.iter().any(|k| k == form) {
                keys.push(form.to_string());
            }
        }
    }
    keys
}

/// Whether `bytes` holds `key` anywhere.
fn holds(bytes: &[u8], key: &[u8]) -> bool {
    !key.is_empty() && bytes.windows(key.len()).any(|window| window == key)
}

/// The manifest: what made the archive, what it holds and what it left
/// out, and why.
fn manifest(
    sources: &Sources,
    now: u64,
    taken: &[(String, u64)],
    left_out: &[(String, String)],
    beyond: usize,
) -> String {
    let kernel = std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .map(|text| text.trim().to_string())
        .unwrap_or_else(|_| "unknown".into());
    let mut text = format!(
        "td-agent diagnostics\nversion: {}\nmade: {}\nkernel: {kernel}\nstate: {}\nconfig: {} (copied as written: read it before sharing)\n",
        env!("CARGO_PKG_VERSION"),
        crate::history::utc(now),
        sources.state.display(),
        sources
            .config
            .as_ref()
            .map_or("none".into(), |c| c.display().to_string()),
    );
    text.push_str("key: its file, its temporary and their directory are never taken");
    text.push_str(match (&sources.key_problem, sources.keys.is_empty()) {
        (Some(_), _) => "; the stored key could not be read to look for (",
        (None, false) => "; no file holding a key td-agent holds is taken\n",
        (None, true) => "; no key is stored to look for\n",
    });
    if let Some(problem) = &sources.key_problem {
        text.push_str(&format!(
            "{problem}), so only the keys td-agent holds are looked for\n"
        ));
    }
    text.push_str("\nfiles:\n");
    for (name, size) in taken {
        text.push_str(&format!("  {name} {size}\n"));
    }
    text.push_str("\nleft out:\n");
    for (name, why) in left_out {
        text.push_str(&format!("  {name}: {why}\n"));
    }
    if beyond > 0 {
        text.push_str(&format!(
            "  at least {beyond} more names past the walk's bound\n"
        ));
    }
    text
}

/// One archive member: its header, its bytes, and padding to a block.
fn entry(out: &mut impl Write, header: &[u8; BLOCK], bytes: &[u8]) -> Result<(), String> {
    let pad = (BLOCK - bytes.len() % BLOCK) % BLOCK;
    out.write_all(header)
        .and_then(|()| out.write_all(bytes))
        .and_then(|()| out.write_all(ZEROS.get(..pad).unwrap_or_default()))
        .map_err(|e| e.to_string())
}

/// A ustar header for a regular file of mode 0600, uid and gid 0.
fn header(name: &str, size: u64, mtime: u64) -> Result<[u8; BLOCK], String> {
    let (prefix, name) = split(name).ok_or("a name too long for a tar header")?;
    let mut header = [0u8; BLOCK];
    put(&mut header, 0, 100, name.as_bytes())?;
    octal(&mut header, 100, 8, 0o600)?;
    octal(&mut header, 108, 8, 0)?;
    octal(&mut header, 116, 8, 0)?;
    octal(&mut header, 124, 12, size)?;
    octal(&mut header, 136, 12, mtime)?;
    put(&mut header, 148, 8, b"        ")?;
    put(&mut header, 156, 1, b"0")?;
    put(&mut header, 257, 6, b"ustar\0")?;
    put(&mut header, 263, 2, b"00")?;
    put(&mut header, 345, 155, prefix.as_bytes())?;
    let sum: u64 = header.iter().map(|b| u64::from(*b)).sum();
    put(&mut header, 148, 8, format!("{sum:06o}\0 ").as_bytes())?;
    Ok(header)
}

/// A name as ustar's prefix and name fields hold it, split at a `/`.
fn split(name: &str) -> Option<(&str, &str)> {
    if name.len() <= 100 {
        return Some(("", name));
    }
    name.char_indices()
        .rfind(|&(at, c)| c == '/' && at <= 155 && name.len() - at - 1 <= 100)
        .and_then(|(at, _)| Some((name.get(..at)?, name.get(at + 1..)?)))
}

fn put(header: &mut [u8; BLOCK], at: usize, len: usize, bytes: &[u8]) -> Result<(), String> {
    if bytes.len() > len {
        return Err("a tar field overflows".into());
    }
    header
        .get_mut(at..at + bytes.len())
        .ok_or("a tar field past its header")?
        .copy_from_slice(bytes);
    Ok(())
}

/// `value` in octal, zero-filled, ending in a NUL.
fn octal(header: &mut [u8; BLOCK], at: usize, len: usize, value: u64) -> Result<(), String> {
    let text = format!("{value:0width$o}", width = len - 1);
    put(header, at, len - 1, text.as_bytes())
}

/// Compresses the archive at `path` with the first compressor the host
/// has that works: the compressed file's path, or the archive's and why.
fn compress(path: PathBuf, compressors: &[Compressor]) -> (PathBuf, Option<String>) {
    let mut whys: Vec<String> = Vec::new();
    for compressor in compressors {
        let out = with_suffix(&path, compressor.suffix);
        let part = with_suffix(&out, PART);
        match run(compressor, &path, &part).and_then(|()| place(&part, &out)) {
            Ok(left) => {
                let stays = std::fs::remove_file(&path)
                    .err()
                    .map(|e| format!("the uncompressed {} stays: {e}", path.display()));
                let remark = [left, stays]
                    .into_iter()
                    .flatten()
                    .reduce(|a, b| format!("{a}; {b}"));
                return (out, remark);
            }
            Err(why) => {
                let _ = std::fs::remove_file(&part);
                if !why.is_empty() {
                    whys.push(why);
                }
            }
        }
    }
    let why = if whys.is_empty() {
        "no zstd or gzip is on PATH".to_string()
    } else {
        whys.join("; ")
    };
    (path, Some(format!("not compressed: {why}")))
}

/// Runs `compressor` over the archive at `path` into a new file at
/// `part`, synced; an empty error for a program the host does not have.
fn run(compressor: &Compressor, path: &Path, part: &Path) -> Result<(), String> {
    let input = File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let output = create(part)?;
    let stdout = output.try_clone().map_err(|e| e.to_string())?;
    let ran = Command::new(compressor.program)
        .args(compressor.args)
        .stdin(input)
        .stdout(stdout)
        .stderr(Stdio::piped())
        .output();
    let program = compressor.program;
    match ran {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(String::new()),
        Err(e) => Err(format!("{program}: {e}")),
        Ok(ran) if !ran.status.success() => {
            let said = String::from_utf8_lossy(&ran.stderr);
            let said: String = said
                .lines()
                .next()
                .unwrap_or("")
                .chars()
                .take(200)
                .collect();
            Err(format!("{program} failed ({}): {said}", ran.status))
        }
        Ok(_) => output
            .sync_all()
            .map_err(|e| format!("{program}'s output: {e}")),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
    use super::*;
    use crate::store::tests::Scratch;
    use std::os::unix::fs::{symlink, PermissionsExt};

    const KEY: &str = "sk-or-v1-0123456789abcdef";
    const NOW: u64 = 1_791_000_000;

    /// The archive's members, as a tar reader sees them.
    fn members(bytes: &[u8]) -> Vec<(String, Vec<u8>)> {
        let mut out = Vec::new();
        let mut at = 0;
        while at + BLOCK <= bytes.len() {
            let header = &bytes[at..at + BLOCK];
            if header.iter().all(|b| *b == 0) {
                break;
            }
            let field = |from: usize, len: usize| {
                let raw = &header[from..from + len];
                let end = raw.iter().position(|b| *b == 0).unwrap_or(len);
                String::from_utf8(raw[..end].to_vec()).unwrap()
            };
            let sum: u64 = header
                .iter()
                .enumerate()
                .map(|(i, b)| {
                    if (148..156).contains(&i) {
                        32
                    } else {
                        u64::from(*b)
                    }
                })
                .sum();
            assert_eq!(u64::from_str_radix(field(148, 6).trim(), 8).unwrap(), sum);
            assert_eq!(&header[257..263], b"ustar\0");
            let size = usize::from_str_radix(&field(124, 11), 8).unwrap();
            let prefix = field(345, 155);
            let name = if prefix.is_empty() {
                field(0, 100)
            } else {
                format!("{prefix}/{}", field(0, 100))
            };
            let body = bytes[at + BLOCK..at + BLOCK + size].to_vec();
            out.push((name, body));
            at += BLOCK + size.div_ceil(BLOCK) * BLOCK;
        }
        out
    }

    fn manifest_of(path: &Path) -> String {
        let bytes = std::fs::read(path).unwrap();
        let members = members(&bytes);
        let (name, body) = members.last().unwrap();
        assert_eq!(name, "td-agent-diagnostics/MANIFEST");
        String::from_utf8(body.clone()).unwrap()
    }

    fn sources(scratch: &Scratch) -> Sources {
        let state = scratch.0.join("state");
        let id = "a".repeat(32);
        let conversation = state.join("conversations").join(&id);
        std::fs::create_dir_all(&conversation).unwrap();
        std::fs::write(conversation.join("log"), "{\"seq\":1}\n").unwrap();
        std::fs::write(conversation.join("meta"), "{}").unwrap();
        let outbox = state.join("outbox").join(&id);
        std::fs::create_dir_all(&outbox).unwrap();
        // Past ustar's 100-byte name, split into its prefix.
        std::fs::write(
            outbox.join(format!("{:020}-{}", 7, "d".repeat(32))),
            "queued",
        )
        .unwrap();
        std::fs::write(state.join("models"), "{}").unwrap();
        let scratch_work = state.join("jail").join(&id).join("scratch");
        std::fs::create_dir_all(&scratch_work).unwrap();
        std::fs::write(scratch_work.join("work"), "the human's").unwrap();
        std::fs::write(state.join("leaky"), format!("before {KEY} after")).unwrap();
        symlink("/etc/passwd", state.join("link")).unwrap();
        let configuration = scratch.0.join("td-agent");
        std::fs::create_dir_all(&configuration).unwrap();
        let config = configuration.join("config");
        std::fs::write(&config, "model = \"m/x\"\n").unwrap();
        let key_file = configuration.join(crate::key::FILE);
        std::fs::write(&key_file, KEY).unwrap();
        Sources {
            state,
            config: Some(config),
            key_file: Some(key_file),
            keys: vec![Secret::new(KEY.into())],
            key_problem: None,
        }
    }

    fn out(scratch: &Scratch, name: &str) -> PathBuf {
        let out = scratch.0.join(name);
        std::fs::create_dir(&out).unwrap();
        out
    }

    #[test]
    fn the_archive_holds_the_state_and_config_and_never_the_key() {
        let scratch = Scratch::new("diagnostics-archive");
        let sources = sources(&scratch);
        let out = out(&scratch, "out");
        let exported = export(&sources, &out, NOW, &[]).unwrap();
        assert!(exported.remark.is_some());
        assert_eq!(
            exported.path,
            out.join("td-agent-diagnostics-20261003T040000Z.tar")
        );
        let mode = std::fs::metadata(&exported.path).unwrap().mode();
        assert_eq!(mode & 0o777, 0o600);
        let bytes = std::fs::read(&exported.path).unwrap();
        assert!(!holds(&bytes, KEY.as_bytes()), "the key is in the archive");
        let members = members(&bytes);
        let names: Vec<&str> = members.iter().map(|(n, _)| n.as_str()).collect();
        let id = "a".repeat(32);
        let queued = format!(
            "td-agent-diagnostics/state/outbox/{id}/{:020}-{}",
            7,
            "d".repeat(32)
        );
        assert!(queued.len() > 100);
        assert_eq!(
            names,
            [
                format!("td-agent-diagnostics/state/conversations/{id}/log").as_str(),
                &format!("td-agent-diagnostics/state/conversations/{id}/meta"),
                "td-agent-diagnostics/state/models",
                &queued,
                "td-agent-diagnostics/config",
                "td-agent-diagnostics/MANIFEST",
            ]
        );
        assert_eq!(members[0].1, b"{\"seq\":1}\n");
        assert_eq!(members[3].1, b"queued");
        let manifest = manifest_of(&exported.path);
        assert!(
            manifest.contains("state/leaky: it holds the API key"),
            "{manifest}"
        );
        assert!(
            manifest.contains("state/link: not a regular file"),
            "{manifest}"
        );
        assert!(manifest.contains("  config 14\n"), "{manifest}");
        // The jail directories hold the human's work: left out whole.
        assert!(
            manifest.contains("state/jail: the workspaces' jail directories"),
            "{manifest}"
        );
        assert!(
            members.iter().all(|(name, _)| !name.contains("/jail/")),
            "{members:?}"
        );
        assert_eq!((exported.files, exported.left_out), (5, 3));
        // No part is left, and a second export in the same second takes
        // a name of its own.
        assert_eq!(std::fs::read_dir(&out).unwrap().count(), 1);
        let again = export(&sources, &out, NOW, &[]).unwrap();
        assert_eq!(
            again.path,
            out.join("td-agent-diagnostics-20261003T040000Z-1.tar")
        );
    }

    /// The state inside the configuration directory, or reaching it: the
    /// key, its temporary (a new key a crash left) and the directory are
    /// never taken, whatever keys are known.
    #[test]
    fn the_key_files_and_their_directory_are_never_taken() {
        let scratch = Scratch::new("diagnostics-overlap");
        let mut sources = sources(&scratch);
        let configuration = scratch.0.join("td-agent");
        std::fs::write(configuration.join(crate::key::TEMPORARY), "sk-or-v1-new").unwrap();
        std::fs::hard_link(
            configuration.join(crate::key::FILE),
            sources.state.join("hard"),
        )
        .unwrap();
        symlink(&configuration, sources.state.join("conf")).unwrap();
        std::fs::create_dir(sources.state.join("bind")).unwrap();
        std::fs::write(sources.state.join("bind").join(crate::key::FILE), "x").unwrap();
        // With no key known at all, only the names and identities keep
        // the key out; the fixture that holds it is not theirs to catch.
        sources.keys.clear();
        std::fs::remove_file(sources.state.join("leaky")).unwrap();
        let out = out(&scratch, "out");
        let exported = export(&sources, &out, NOW, &[]).unwrap();
        let bytes = std::fs::read(&exported.path).unwrap();
        assert!(!holds(&bytes, KEY.as_bytes()));
        assert!(!holds(&bytes, b"sk-or-v1-new"));
        let manifest = manifest_of(&exported.path);
        assert!(
            manifest.contains("state/hard: it is the key file under another name"),
            "{manifest}"
        );
        assert!(
            manifest.contains("state/bind/openrouter.key: it is named as the key file"),
            "{manifest}"
        );
        assert!(
            manifest.contains("no key is stored to look for"),
            "{manifest}"
        );
        // The state directory that is the configuration directory.
        let overlapping = Sources {
            state: configuration.clone(),
            ..sources
        };
        let exported = export(&overlapping, &out, NOW, &[]).unwrap();
        let bytes = std::fs::read(&exported.path).unwrap();
        assert!(!holds(&bytes, KEY.as_bytes()));
        assert!(!holds(&bytes, b"sk-or-v1-new"));
        assert!(manifest_of(&exported.path).contains("state/: it is the configuration directory"));
    }

    #[test]
    fn a_fifo_or_a_linked_config_is_read_as_td_agent_reads_it() {
        let scratch = Scratch::new("diagnostics-fifo");
        let mut sources = sources(&scratch);
        let fifo = scratch.0.join("fifo");
        let made = Command::new("mkfifo").arg(&fifo).status().unwrap();
        assert!(made.success());
        // A FIFO is not waited on.
        sources.config = Some(fifo);
        let out = out(&scratch, "out");
        let manifest = manifest_of(&export(&sources, &out, NOW, &[]).unwrap().path);
        assert!(
            manifest.contains("config: not a regular file"),
            "{manifest}"
        );
        // A configuration that is a link is followed, as config::load
        // follows it, unless it reaches the key.
        let elsewhere = scratch.0.join("dotfiles-config");
        std::fs::write(&elsewhere, "model = \"m/y\"\n").unwrap();
        let link = scratch.0.join("config-link");
        symlink(&elsewhere, &link).unwrap();
        sources.config = Some(link.clone());
        let manifest = manifest_of(&export(&sources, &out, NOW, &[]).unwrap().path);
        assert!(manifest.contains("  config 14\n"), "{manifest}");
        std::fs::remove_file(&link).unwrap();
        symlink(sources.key_file.clone().unwrap(), &link).unwrap();
        sources.keys.clear();
        std::fs::remove_file(sources.state.join("leaky")).unwrap();
        let exported = export(&sources, &out, NOW, &[]).unwrap();
        assert!(!holds(
            &std::fs::read(&exported.path).unwrap(),
            KEY.as_bytes()
        ));
        assert!(
            manifest_of(&exported.path).contains("config: it is the key file under another name")
        );
    }

    #[test]
    fn a_key_escaped_in_json_and_the_bounds_keep_files_out() {
        let scratch = Scratch::new("diagnostics-bounds");
        let mut sources = sources(&scratch);
        let odd = "k\"ey\\with-quotes";
        std::fs::write(
            sources.state.join("escaped"),
            td_json::Json::Str(odd.into()).to_string(),
        )
        .unwrap();
        sources.keys.push(Secret::new(odd.into()));
        sources.key_problem = Some("the API key file is refused: mode 0644".into());
        let out = out(&scratch, "out");
        let limits = Limits {
            file: 12,
            total: 20,
            entries: 4,
        };
        let exported = export_within(&sources, &out, NOW, &[], limits).unwrap();
        let manifest = manifest_of(&exported.path);
        assert!(
            manifest
                .contains("could not be read to look for (the API key file is refused: mode 0644)"),
            "{manifest}"
        );
        assert!(
            manifest.contains("more names past the walk's bound"),
            "{manifest}"
        );
        let full = export(&sources, &out, NOW, &[]).unwrap();
        let manifest = manifest_of(&full.path);
        assert!(
            manifest.contains("state/escaped: it holds the API key"),
            "{manifest}"
        );
        let small = Limits {
            file: 9,
            total: 7,
            entries: MAX_ENTRIES,
        };
        let manifest = manifest_of(&export_within(&sources, &out, NOW, &[], small).unwrap().path);
        assert!(manifest.contains("log: longer than 9 bytes"), "{manifest}");
        assert!(
            manifest.contains("past the export's bound on bytes"),
            "{manifest}"
        );
    }

    #[test]
    fn a_compressor_makes_its_file_and_a_failing_one_gives_way() {
        let scratch = Scratch::new("diagnostics-compress");
        let sources = sources(&scratch);
        let missing = Compressor {
            program: "td-agent-no-such-compressor",
            args: &[],
            suffix: "x",
        };
        // `cat` with a flag it refuses fails, saying why; plain `cat`
        // stands in for a compressor that works.
        let refusing = Compressor {
            program: "cat",
            args: &["--td-agent-no-such-flag"],
            suffix: "bad",
        };
        let copy = Compressor {
            program: "cat",
            args: &[],
            suffix: "copy",
        };
        let out = out(&scratch, "out");
        let exported = export(&sources, &out, NOW, &[missing, refusing, copy]).unwrap();
        assert!(exported.remark.is_none(), "{:?}", exported.remark);
        assert_eq!(exported.path.extension().unwrap(), "copy");
        let metadata = std::fs::metadata(&exported.path).unwrap();
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
        let names: Vec<String> = std::fs::read_dir(&out)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names, ["td-agent-diagnostics-20261003T040000Z.tar.copy"]);
        manifest_of(&exported.path);
        // With every compressor failing, the archive stays, and why.
        let refusing = Compressor {
            program: "cat",
            args: &["--td-agent-no-such-flag"],
            suffix: "bad",
        };
        let out = scratch.0.join("again");
        std::fs::create_dir(&out).unwrap();
        let exported = export(&sources, &out, NOW, &[refusing]).unwrap();
        assert_eq!(exported.path.extension().unwrap(), "tar");
        let remark = exported.remark.unwrap();
        assert!(remark.starts_with("not compressed: cat failed"), "{remark}");
        // Whatever `cat` the host has, it says something.
        let said = remark.split_once("): ").map_or("", |(_, said)| said);
        assert!(!said.trim().is_empty(), "{remark}");
        assert_eq!(std::fs::read_dir(&out).unwrap().count(), 1);
    }

    #[test]
    fn a_name_past_both_ustar_fields_is_refused() {
        assert_eq!(split("a/b"), Some(("", "a/b")));
        let long = format!("{}/{}", "p".repeat(150), "n".repeat(100));
        assert_eq!(
            split(&long).map(|(p, n)| (p.len(), n.len())),
            Some((150, 100))
        );
        assert_eq!(split(&"x".repeat(101)), None);
        assert!(header(&"x".repeat(101), 0, 0).is_err());
    }
}

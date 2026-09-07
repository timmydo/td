// td-feed — td's OWN local HTTP mirror of the artifacts this repo downloads over the
// network. A sibling of fetch/ (td-fetch) reusing the same pure-Rust HTTP(S)+sha256 stack
// (ureq + rustls/ring + sha2); the mirror server uses only std::net (no extra crate).
//
// It is run as a SHARED, persistent host daemon (`td-feed ensure-serve`) serving a shared
// store across worktrees, so a td-native download happens ONCE. Two verification layers:
//
//   - warm = the SUPPLY-CHAIN gate. `td-feed warm INDEX STORE` (host PREP, egress) GETs
//     each pinned `<path> <url> <sha256>` entry, verifies the bytes against the PINNED
//     index sha256, and writes STORE/<path> PLUS a STORE/<path>.sha256 sidecar (the
//     verified hash). Idempotent: an entry already present + matching is skipped.
//
//   - serve = the INTEGRITY gate, and INDEX-FREE. `td-feed serve STORE ADDR` answers
//     `GET /<path>` by reading STORE/<path> + its .sha256 sidecar, RE-VERIFYING the file
//     against the sidecar (store corruption 500s), and streaming it. Because each artifact
//     is self-describing, a persistent daemon serves whatever any branch has warmed into
//     the shared store with no index coupling. Missing path/sidecar 404/500. No egress.
//
//   td-feed selftest   Self-contained LOOPBACK round-trip (offline): an ORIGIN server on
//                      127.0.0.1, `warm` a one-entry index from it, `serve` the store on a
//                      2nd port, fetch the artifact back THROUGH the feed and verify it.
//                      Also asserts both gates are load-bearing: a wrong pinned hash reds
//                      warm, a corrupted store byte reds serve (sidecar mismatch).
mod graphs;
mod vendor;

use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const SERVE_WORKERS: usize = 8;
const SERVE_WORKER_STACK_BYTES: usize = 512 * 1024;
const MAX_REQUEST_LINE_BYTES: usize = 16 * 1024;
const MAX_REQUEST_HEAD_BYTES: usize = 64 * 1024;
const REQUEST_IO_TIMEOUT: Duration = Duration::from_secs(5);
const RESPONSE_DEADLINE: Duration = Duration::from_secs(30 * 60);
const SOURCE_CONSUMER_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_ARTIFACT_BYTES: u64 = 16 * 1024 * 1024 * 1024;
const MAX_INDEX_BYTES: u64 = 64 * 1024 * 1024;
const FEED_NO_DAEMON_ENV: &str = "TD_FEED_NO_DAEMON";
// Linux UAPI on td's x86/ARM targets: refuse final symlinks and avoid blocking
// on a substituted FIFO before fstat can reject its file type.
pub(crate) const OPEN_REGULAR_NOFOLLOW: i32 = 0x20000 | 0x800;

/// One mirror artifact: served at `path`, fetched from `url`, content sha256 `sha256`.
struct Entry {
    path: String,
    url: String,
    sha256: String,
}

#[derive(Clone)]
struct SourcePin {
    key: String,
    url: String,
    sha256: String,
    file: String,
}

pub(crate) fn hex_sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn reader_sha256_before(
    reader: &mut impl Read,
    max_bytes: u64,
    guard: Option<&ResponseGuard<'_>>,
) -> io::Result<String> {
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        if let Some(guard) = guard {
            guard.check()?;
        }
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        total = total.saturating_add(u64::try_from(n).unwrap_or(u64::MAX));
        if total > max_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("feed hash input exceeds its {max_bytes}-byte limit"),
            ));
        }
        let chunk = buf.get(..n).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "hash read exceeded its buffer")
        })?;
        hasher.update(chunk);
    }
    if let Some(guard) = guard {
        guard.check()?;
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn file_sha256(path: &Path) -> io::Result<String> {
    file_sha256_before(path, MAX_ARTIFACT_BYTES, None)
}

fn file_sha256_before(
    path: &Path,
    max_bytes: u64,
    guard: Option<&ResponseGuard<'_>>,
) -> io::Result<String> {
    let mut file = File::open(path)?;
    if file.metadata()?.len() > max_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("feed hash input exceeds its {max_bytes}-byte limit"),
        ));
    }
    reader_sha256_before(&mut file, max_bytes, guard)
}

fn decode_mount_field(value: &str) -> PathBuf {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes.get(index) == Some(&b'\\') {
            let a = bytes.get(index.saturating_add(1)).copied();
            let b = bytes.get(index.saturating_add(2)).copied();
            let c = bytes.get(index.saturating_add(3)).copied();
            if let (Some(a), Some(b), Some(c)) = (a, b, c) {
                if (b'0'..=b'7').contains(&a)
                    && (b'0'..=b'7').contains(&b)
                    && (b'0'..=b'7').contains(&c)
                {
                    decoded.push((a - b'0') * 64 + (b - b'0') * 8 + (c - b'0'));
                    index = index.saturating_add(4);
                    continue;
                }
            }
        }
        if let Some(byte) = bytes.get(index) {
            decoded.push(*byte);
        }
        index = index.saturating_add(1);
    }
    PathBuf::from(std::ffi::OsString::from_vec(decoded))
}

fn mount_is_memory_backed(path: &Path, mountinfo: &str, depth: usize) -> io::Result<bool> {
    if depth > 8 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "overlay backing mount recursion exceeds its limit",
        ));
    }
    let mut chosen: Option<(usize, &str, &str)> = None;
    for line in mountinfo.lines() {
        let Some((left, right)) = line.split_once(" - ") else {
            continue;
        };
        let Some(encoded_mount) = left.split_whitespace().nth(4) else {
            continue;
        };
        let mount = decode_mount_field(encoded_mount);
        if !path.starts_with(&mount) {
            continue;
        }
        let mut fields = right.split_whitespace();
        let Some(fs_type) = fields.next() else {
            continue;
        };
        let _source = fields.next();
        let Some(super_options) = fields.next() else {
            continue;
        };
        let width = mount.as_os_str().as_bytes().len();
        if chosen.is_none_or(|(best, _, _)| width > best) {
            chosen = Some((width, fs_type, super_options));
        }
    }
    let Some((_, fs_type, super_options)) = chosen else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("no mount accounting for {}", path.display()),
        ));
    };
    if matches!(fs_type, "tmpfs" | "ramfs" | "hugetlbfs" | "devtmpfs") {
        return Ok(true);
    }
    if fs_type != "overlay" {
        return Ok(false);
    }
    let upper = super_options
        .split(',')
        .find_map(|option| option.strip_prefix("upperdir="))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("overlay mount for {} has no upperdir", path.display()),
            )
        })?;
    mount_is_memory_backed(
        &decode_mount_field(upper),
        mountinfo,
        depth.saturating_add(1),
    )
}

fn require_disk_backed(path: &Path) -> io::Result<()> {
    require_disk_backed_for(path, "TD_FEED_DIR")
}

fn require_disk_backed_for(path: &Path, setting: &str) -> io::Result<()> {
    let canonical = std::fs::canonicalize(path)?;
    let mountinfo = std::fs::read_to_string("/proc/self/mountinfo")?;
    if mount_is_memory_backed(&canonical, &mountinfo, 0)? {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "feed cache {} is memory-backed; choose a disk-backed {setting}",
                canonical.display()
            ),
        ));
    }
    Ok(())
}

/// Copy and hash one warmed artifact into an already-unlinked disk file, then
/// serve that exact descriptor. Hashing a mutable path and rewinding it left a
/// race where a same-inode writer could change the bytes between those steps;
/// the snapshot preserves constant memory without weakening integrity. Hashing
/// during the copy avoids another full read before serving.
#[cfg(test)]
fn snapshot_for_serve(path: &Path) -> io::Result<(File, String)> {
    let guard = ResponseGuard::without_client(Instant::now() + RESPONSE_DEADLINE);
    snapshot_for_serve_before(path, &guard)
}

fn snapshot_for_serve_before(path: &Path, guard: &ResponseGuard<'_>) -> io::Result<(File, String)> {
    static NEXT_SNAPSHOT: AtomicU64 = AtomicU64::new(0);
    guard.check()?;
    let mut source = File::open(path)?;
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "feed path has no parent"))?;
    require_disk_backed(parent)?;
    for _ in 0..128 {
        guard.check()?;
        let nonce = NEXT_SNAPSHOT.fetch_add(1, Ordering::Relaxed);
        let tmp = parent.join(format!(".td-feed-serve-{}-{nonce}.tmp", std::process::id()));
        let opened = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp);
        let mut snapshot = match opened {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        };
        if let Err(e) = std::fs::remove_file(&tmp) {
            drop(snapshot);
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
        let mut hasher = Sha256::new();
        let mut total = 0u64;
        let mut buf = [0u8; 64 * 1024];
        loop {
            guard.check()?;
            let n = source.read(&mut buf)?;
            if n == 0 {
                break;
            }
            total = total.saturating_add(u64::try_from(n).unwrap_or(u64::MAX));
            if total > MAX_ARTIFACT_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "feed snapshot exceeds its 16 GiB byte limit",
                ));
            }
            let chunk = buf.get(..n).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "snapshot read exceeded its buffer",
                )
            })?;
            hasher.update(chunk);
            snapshot.write_all(chunk)?;
            guard.check()?;
        }
        guard.check()?;
        snapshot.rewind()?;
        return Ok((snapshot, format!("{:x}", hasher.finalize())));
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a unique feed snapshot",
    ))
}

/// Parse an index: `<path> <url> <sha256>` per line; `#` comments and blanks ignored.
fn parse_index(text: &str) -> Result<Vec<Entry>, String> {
    let mut out = Vec::new();
    for (n, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut it = line.split_whitespace();
        match (it.next(), it.next(), it.next(), it.next()) {
            (Some(p), Some(u), Some(h), None) => out.push(Entry {
                path: p.to_string(),
                url: u.to_string(),
                sha256: h.to_lowercase(),
            }),
            _ => return Err(format!("malformed index line {}: {line:?}", n + 1)),
        }
    }
    Ok(out)
}

/// Map a mirror path to a store file, rejecting traversal / absolute components so a
/// crafted index path or request can never escape STORE.
fn store_path(store: &Path, rel: &str) -> Option<PathBuf> {
    if rel.is_empty() || rel.starts_with('/') {
        return None;
    }
    if rel
        .split('/')
        .any(|c| c.is_empty() || c == "." || c == "..")
    {
        return None;
    }
    Some(store.join(rel))
}

/// The integrity sidecar path for a store file: `<file>.sha256` (append, not replace, so
/// `x.crate` -> `x.crate.sha256`).
fn sidecar_path(dst: &Path) -> PathBuf {
    let mut s = dst.as_os_str().to_os_string();
    s.push(".sha256");
    PathBuf::from(s)
}

fn read_digest_sidecar(path: &Path) -> io::Result<String> {
    const DIGEST_BYTES: usize = 64;

    let file = File::open(path)?;
    let mut bytes = Vec::with_capacity(DIGEST_BYTES.saturating_add(1));
    file.take(u64::try_from(DIGEST_BYTES.saturating_add(2)).unwrap_or(u64::MAX))
        .read_to_end(&mut bytes)?;
    if bytes.last() == Some(&b'\n') {
        bytes.pop();
    }
    if bytes.len() != DIGEST_BYTES
        || !bytes
            .iter()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "feed digest sidecar is not one lowercase SHA-256 digest",
        ));
    }
    String::from_utf8(bytes).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "feed digest sidecar is not UTF-8",
        )
    })
}

/// Write `bytes` to `dst` atomically (pid-unique temp + rename), so a concurrent serve /
/// another warming agent never sees a partial file.
fn write_atomic(dst: &Path, bytes: &[u8]) -> Result<(), String> {
    let mut t = dst.as_os_str().to_os_string();
    t.push(format!(".{}.td-feed-tmp", std::process::id()));
    let tmp = PathBuf::from(t);
    std::fs::write(&tmp, bytes).map_err(|e| format!("write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, dst).map_err(|e| format!("rename {}: {e}", dst.display()))
}

/// GET `url` (http/https), returning the body or an error string. Carries the shared
/// read timeout + bounded retry (`http`) — warming a cold source set is exactly where
/// one silent peer used to stall the whole ladder climb.
fn try_get(url: &str) -> Result<Vec<u8>, String> {
    crate::http::get_body(url)
}

fn try_get_before(url: &str, guard: &ResponseGuard<'_>) -> Result<Vec<u8>, String> {
    guard.check().map_err(|e| e.to_string())?;
    let body = crate::http::get_body_before(url, guard.deadline)?;
    guard.check().map_err(|e| e.to_string())?;
    Ok(body)
}

struct RemoveFileOnDrop {
    path: PathBuf,
    _directory_lock: File,
    _reservation: File,
}

struct RemoveDirOnDrop(PathBuf);

impl Drop for RemoveDirOnDrop {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

impl Drop for RemoveFileOnDrop {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn sweep_download_temps(parent: &Path) {
    let Ok(entries) = std::fs::read_dir(parent) else {
        return;
    };
    let abandoned = entries.flatten().filter(|entry| {
        let name = entry.file_name();
        let bytes = name.as_bytes();
        bytes.starts_with(b".td-feed-download-") && bytes.ends_with(b".tmp")
    });
    for entry in abandoned.take(4096) {
        let _ = std::fs::remove_file(entry.path());
    }
}

fn sweep_kernel_header_temps(parent: &Path) {
    let Ok(entries) = std::fs::read_dir(parent) else {
        return;
    };
    let abandoned = entries.flatten().filter(|entry| {
        let name = entry.file_name();
        let bytes = name.as_bytes();
        bytes.starts_with(b".td-feed-kh-work-")
            || (bytes.starts_with(b".td-feed-kh-output-") && bytes.ends_with(b".tmp"))
    });
    for entry in abandoned.take(4096) {
        let path = entry.path();
        if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            let _ = std::fs::remove_dir_all(path);
        } else {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Download and verify a pinned artifact through a constant-memory path, then
/// atomically publish it. The 16 GiB wire ceiling matches the decoded archive
/// and tar-payload ceilings: it is a disk-demand backstop as well as protection
/// against an endless HTTP body.
fn download_verified(url: &str, dst: &Path, want: &str) -> Result<(), String> {
    download_verified_before(url, dst, want, None)
}

fn download_verified_before(
    url: &str,
    dst: &Path,
    want: &str,
    guard: Option<&ResponseGuard<'_>>,
) -> Result<(), String> {
    download_verified_policy(url, dst, want, guard, true)
}

fn download_verified_policy(
    url: &str,
    dst: &Path,
    want: &str,
    guard: Option<&ResponseGuard<'_>>,
    allow_redirects: bool,
) -> Result<(), String> {
    if !allow_redirects && guard.is_none() {
        return Err("redirect refusal requires a transfer deadline".into());
    }
    acquire_verified(
        dst,
        want,
        url,
        guard,
        |path| {
            if let Some(guard) = guard {
                guard.check().map_err(|e| e.to_string())?;
                if allow_redirects {
                    crate::http::get_to_file_before(url, path, MAX_ARTIFACT_BYTES, guard.deadline)?;
                } else {
                    crate::http::get_to_file_accounted_no_redirects_before(
                        url,
                        path,
                        MAX_ARTIFACT_BYTES,
                        &AtomicU64::new(0),
                        MAX_ARTIFACT_BYTES,
                        guard.deadline,
                    )?;
                }
                guard.check().map_err(|e| e.to_string())?;
            } else {
                crate::http::get_to_file(url, path, MAX_ARTIFACT_BYTES)?;
            }
            Ok(())
        },
        || Ok(()),
    )
}

/// One locked publication path for downloaded and locally exported artifacts.
fn acquire_verified(
    dst: &Path,
    want: &str,
    source: &str,
    guard: Option<&ResponseGuard<'_>>,
    acquire: impl FnOnce(&Path) -> Result<(), String>,
    finish: impl FnOnce() -> Result<(), String>,
) -> Result<(), String> {
    static NEXT_DOWNLOAD: AtomicU64 = AtomicU64::new(0);
    let parent = dst
        .parent()
        .ok_or_else(|| format!("download destination {} has no parent", dst.display()))?;
    std::fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    require_disk_backed(parent).map_err(|e| e.to_string())?;
    // Serializing downloads within one destination directory lowers demand
    // and makes crash cleanup race-free. SIGKILL releases this lock, so the
    // next download can remove every abandoned named partial before starting.
    let lock_path = parent.join(".td-feed-download.lock");
    let directory_lock = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .mode(0o600)
        .open(&lock_path)
        .map_err(|e| format!("open {}: {e}", lock_path.display()))?;
    if let Some(guard) = guard {
        loop {
            guard.check().map_err(|e| e.to_string())?;
            match directory_lock.try_lock() {
                Ok(()) => break,
                Err(std::fs::TryLockError::WouldBlock) => {
                    std::thread::sleep(
                        response_remaining(guard.deadline)
                            .map_err(|e| e.to_string())?
                            .min(Duration::from_millis(20)),
                    );
                }
                Err(std::fs::TryLockError::Error(e)) => {
                    return Err(format!("lock {}: {e}", lock_path.display()))
                }
            }
        }
    } else {
        directory_lock
            .lock()
            .map_err(|e| format!("lock {}: {e}", lock_path.display()))?;
    }
    sweep_download_temps(parent);
    let existing = file_sha256_before(dst, MAX_ARTIFACT_BYTES, guard);
    if let Some(guard) = guard {
        guard
            .check()
            .map_err(|e| format!("hash {}: {e}", dst.display()))?;
    }
    if existing.ok().as_deref() == Some(want) {
        return finish();
    }
    let (path, reservation) = loop {
        let nonce = NEXT_DOWNLOAD.fetch_add(1, Ordering::Relaxed);
        let candidate = parent.join(format!(
            ".td-feed-download-{}-{nonce}.tmp",
            std::process::id()
        ));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&candidate)
        {
            Ok(file) => break (candidate, file),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("reserve {}: {e}", candidate.display())),
        }
    };
    let tmp = RemoveFileOnDrop {
        path,
        _directory_lock: directory_lock,
        _reservation: reservation,
    };
    (|| {
        acquire(&tmp.path)?;
        let got = file_sha256_before(&tmp.path, MAX_ARTIFACT_BYTES, guard)
            .map_err(|e| format!("hash {}: {e}", tmp.path.display()))?;
        if got != want {
            return Err(format!(
                "sha256 mismatch for {source}\n  want {want}\n  got  {got}"
            ));
        }
        std::fs::rename(&tmp.path, dst).map_err(|e| format!("publish {}: {e}", dst.display()))?;
        finish()
    })()
}

/// Warm one entry into `store` (+ its sidecar); Ok(true) if fetched, Ok(false) if already
/// warm + verified. Never egresses for an entry already present + matching.
fn warm_one(e: &Entry, store: &Path) -> Result<bool, String> {
    if !source_feed_path_is_safe(&e.path) {
        return Err(format!("unsafe feed artifact path {:?}", e.path));
    }
    let dst = store.join(&e.path);
    let mut fetched = false;
    acquire_verified(
        &dst,
        &e.sha256,
        &e.url,
        None,
        |temp| {
            crate::http::get_to_file(&e.url, temp, MAX_ARTIFACT_BYTES)?;
            fetched = true;
            Ok(())
        },
        || {
            let side = sidecar_path(&dst);
            if read_digest_sidecar(&side).ok().as_deref() != Some(&e.sha256) {
                write_atomic(&side, format!("{}\n", e.sha256).as_bytes())?;
            }
            Ok(())
        },
    )?;
    Ok(fetched)
}

/// Warm every entry; returns (fetched, already-warm).
fn warm(index: &[Entry], store: &Path) -> Result<(usize, usize), String> {
    let mut fetched = 0;
    let mut warm = 0;
    for e in index {
        if warm_one(e, store)? {
            fetched += 1;
        } else {
            warm += 1;
        }
    }
    Ok((fetched, warm))
}

/// Write an HTTP/1.1 response with `Connection: close`.
struct ResponseGuard<'a> {
    deadline: Instant,
    client: Option<&'a TcpStream>,
}

impl<'a> ResponseGuard<'a> {
    fn client(deadline: Instant, client: &'a TcpStream) -> Self {
        Self {
            deadline,
            client: Some(client),
        }
    }

    fn without_client(deadline: Instant) -> Self {
        Self {
            deadline,
            client: None,
        }
    }

    fn check(&self) -> io::Result<()> {
        let _ = response_remaining(self.deadline)?;
        if let Some(client) = self.client {
            if client_disconnected(client)? {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "feed client disconnected before response was ready",
                ));
            }
        }
        Ok(())
    }
}

fn response_remaining(deadline: Instant) -> io::Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "feed response deadline expired"))
}

fn client_disconnected(conn: &TcpStream) -> io::Result<bool> {
    conn.set_nonblocking(true)?;
    let mut probe = [0u8; 1];
    let result = loop {
        match conn.peek(&mut probe) {
            Ok(0) => break Ok(true),
            Ok(_) => break Ok(false),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => break Ok(false),
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::BrokenPipe
                        | io::ErrorKind::ConnectionReset
                        | io::ErrorKind::NotConnected
                        | io::ErrorKind::UnexpectedEof
                ) =>
            {
                break Ok(true);
            }
            Err(e) => break Err(e),
        }
    };
    let restore = conn.set_nonblocking(false);
    match (result, restore) {
        (Ok(disconnected), Ok(())) => Ok(disconnected),
        (Ok(_), Err(e)) | (Err(e), _) => Err(e),
    }
}

fn write_before(conn: &mut TcpStream, mut bytes: &[u8], deadline: Instant) -> io::Result<()> {
    while !bytes.is_empty() {
        let remaining = response_remaining(deadline)?;
        conn.set_write_timeout(Some(remaining.min(REQUEST_IO_TIMEOUT)))?;
        let count = conn.write(bytes)?;
        if count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "feed response made no progress",
            ));
        }
        bytes = bytes.get(count..).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "feed response write exceeded its buffer",
            )
        })?;
    }
    Ok(())
}

fn flush_before(conn: &mut TcpStream, deadline: Instant) -> io::Result<()> {
    let remaining = response_remaining(deadline)?;
    conn.set_write_timeout(Some(remaining.min(REQUEST_IO_TIMEOUT)))?;
    conn.flush()
}

fn respond(conn: &mut TcpStream, code: u16, reason: &str, body: &[u8]) -> io::Result<()> {
    let deadline = Instant::now() + RESPONSE_DEADLINE;
    respond_before(conn, code, reason, body, deadline)
}

fn respond_before(
    conn: &mut TcpStream,
    code: u16,
    reason: &str,
    body: &[u8],
    deadline: Instant,
) -> io::Result<()> {
    let head = format!(
        "HTTP/1.1 {code} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    write_before(conn, head.as_bytes(), deadline)?;
    write_before(conn, body, deadline)?;
    flush_before(conn, deadline)
}

fn respond_file_before(
    conn: &mut TcpStream,
    file: &mut File,
    len: u64,
    deadline: Instant,
) -> io::Result<()> {
    let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n");
    write_before(conn, head.as_bytes(), deadline)?;
    let mut remaining = len;
    let mut buf = [0u8; 64 * 1024];
    while remaining > 0 {
        let width = usize::try_from(remaining.min(buf.len() as u64)).unwrap_or(buf.len());
        let chunk = buf.get_mut(..width).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "feed response read exceeded its buffer",
            )
        })?;
        let count = file.read(chunk)?;
        if count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "feed artifact ended while it was served",
            ));
        }
        let bytes = chunk.get(..count).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "feed response read exceeded its buffer",
            )
        })?;
        write_before(conn, bytes, deadline)?;
        remaining = remaining.saturating_sub(u64::try_from(count).unwrap_or(u64::MAX));
    }
    flush_before(conn, deadline)
}

fn read_request_head(conn: &mut TcpStream) -> io::Result<Vec<u8>> {
    let deadline = Instant::now() + REQUEST_IO_TIMEOUT;
    let mut head = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    "feed request head deadline expired",
                )
            })?;
        conn.set_read_timeout(Some(remaining))?;
        let n = conn.read(&mut chunk)?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "feed request ended before its header",
            ));
        }
        let Some(bytes) = chunk.get(..n) else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "feed request read exceeded its buffer",
            ));
        };
        if head.len().saturating_add(bytes.len()) > MAX_REQUEST_HEAD_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "feed request head exceeds its byte limit",
            ));
        }
        head.extend_from_slice(bytes);
        if head.windows(4).any(|window| window == b"\r\n\r\n")
            || head.windows(2).any(|window| window == b"\n\n")
        {
            return Ok(head);
        }
    }
}

/// Handle one request: route `GET /<path>`, verify the file against its sidecar, stream.
fn handle_conn(mut conn: TcpStream, store: &Path) -> io::Result<()> {
    conn.set_write_timeout(Some(REQUEST_IO_TIMEOUT))?;
    let head = read_request_head(&mut conn)?;
    let response_deadline = Instant::now() + RESPONSE_DEADLINE;
    let line_end = head
        .iter()
        .position(|byte| *byte == b'\n')
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing request line"))?;
    if line_end > MAX_REQUEST_LINE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "feed request line exceeds its byte limit",
        ));
    }
    let req_line = std::str::from_utf8(head.get(..line_end).unwrap_or_default())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "non-UTF-8 request line"))?;
    let mut parts = req_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("");
    if method != "GET" {
        return respond_before(
            &mut conn,
            405,
            "Method Not Allowed",
            b"method not allowed\n",
            response_deadline,
        );
    }
    let path = target.trim_start_matches('/');
    // The integrity sidecars are internal — never serve them.
    if path.ends_with(".sha256") {
        return respond_before(
            &mut conn,
            404,
            "Not Found",
            b"not served\n",
            response_deadline,
        );
    }
    let full = match store_path(store, path) {
        Some(p) => p,
        None => {
            return respond_before(
                &mut conn,
                400,
                "Bad Request",
                b"bad path\n",
                response_deadline,
            )
        }
    };
    let snapshot = {
        let guard = ResponseGuard::client(response_deadline, &conn);
        snapshot_for_serve_before(&full, &guard)
    };
    let (mut file, got) = match snapshot {
        Ok(snapshot) => snapshot,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return respond_before(
                &mut conn,
                404,
                "Not Found",
                b"not warmed\n",
                response_deadline,
            )
        }
        Err(e) => return Err(e),
    };
    ResponseGuard::client(response_deadline, &conn).check()?;
    let want = match read_digest_sidecar(&sidecar_path(&full)) {
        Ok(digest) => digest,
        // No sidecar ⇒ the artifact was not placed by `warm`; refuse to serve unverified.
        Err(_) => {
            return respond_before(
                &mut conn,
                500,
                "No Integrity Sidecar",
                b"no sidecar\n",
                response_deadline,
            )
        }
    };
    ResponseGuard::client(response_deadline, &conn).check()?;
    if got != want {
        // verify-on-serve: the store drifted from the warmed hash — refuse to serve.
        return respond_before(
            &mut conn,
            500,
            "Integrity Failure",
            b"store sha256 mismatch\n",
            response_deadline,
        );
    }
    let len = file.metadata()?.len();
    respond_file_before(&mut conn, &mut file, len, response_deadline)
}

fn serve_worker(listener: TcpListener, store: Arc<PathBuf>) {
    for conn in listener.incoming() {
        let Ok(conn) = conn else { continue };
        let _ = handle_conn(conn, &store);
    }
}

/// Run the mirror server forever with a fixed worker count. Kernel socket
/// backlog supplies bounded backpressure without allocating a thread per peer.
fn serve_loop(listener: TcpListener, store: Arc<PathBuf>) {
    for worker in 1..SERVE_WORKERS {
        let Ok(worker_listener) = listener.try_clone() else {
            break;
        };
        let worker_store = Arc::clone(&store);
        let _ = std::thread::Builder::new()
            .name(format!("td-feed-{worker}"))
            .stack_size(SERVE_WORKER_STACK_BYTES)
            .spawn(move || serve_worker(worker_listener, worker_store));
    }
    serve_worker(listener, store);
}

fn die(msg: String) -> ! {
    eprintln!("td-feed: {msg}");
    std::process::exit(1);
}

fn read_index(path: &str) -> Vec<Entry> {
    let text =
        std::fs::read_to_string(path).unwrap_or_else(|e| die(format!("read index {path}: {e}")));
    parse_index(&text).unwrap_or_else(|e| die(e))
}

/// A one-shot origin responder used only by the selftest.
fn serve_once(conn: &mut TcpStream, body: &[u8]) -> io::Result<()> {
    let mut buf = [0u8; 1024];
    let _ = conn.read(&mut buf)?;
    respond(conn, 200, "OK", body)
}

fn selftest() {
    // A known artifact (non-trivial bytes so a flipped byte is detectable).
    let blob: Vec<u8> = (0u16..4096).map(|x| (x % 251) as u8).collect();
    let want = hex_sha256(&blob);

    // 1. An ORIGIN server on loopback, serving `blob` at /blob.
    let origin = TcpListener::bind("127.0.0.1:0").expect("bind origin");
    let origin_port = origin.local_addr().expect("addr").port();
    let ob = blob.clone();
    std::thread::spawn(move || loop {
        match origin.accept() {
            Ok((mut c, _)) => {
                let _ = serve_once(&mut c, &ob);
            }
            Err(_) => break,
        }
    });

    // 2. A one-entry index: serve it at origin.invalid/blob, fetch it from the origin.
    let path = "origin.invalid/blob".to_string();
    let index = vec![Entry {
        path: path.clone(),
        url: format!("http://127.0.0.1:{origin_port}/blob"),
        sha256: want.clone(),
    }];

    // 3. Warm into a fresh temp store (writes the file + its sidecar).
    let store = std::env::temp_dir().join(format!("td-feed-selftest-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&store);
    let (fetched, _) = warm(&index, &store).unwrap_or_else(|e| die(format!("warm: {e}")));
    if fetched != 1 {
        die(format!("expected to fetch 1 artifact, fetched {fetched}"));
    }
    let stored = store_path(&store, &path).unwrap();
    if hex_sha256(&std::fs::read(&stored).expect("read stored")) != want {
        die("warmed store artifact does not match its pinned sha256".into());
    }
    if !sidecar_path(&stored).exists() {
        die("warm did not write the integrity sidecar".into());
    }

    // 4. Serve the store on a 2nd loopback port (index-free — sidecars carry the hashes).
    let feed = TcpListener::bind("127.0.0.1:0").expect("bind feed");
    let feed_port = feed.local_addr().expect("addr").port();
    {
        let s = Arc::new(store.clone());
        std::thread::spawn(move || serve_loop(feed, s));
    }

    // 5. Fetch the artifact back THROUGH the feed; bytes + sha256 must match the origin.
    let feed_url = format!("http://127.0.0.1:{feed_port}/{path}");
    let got = try_get(&feed_url).unwrap_or_else(|e| die(format!("fetch through feed: {e}")));
    if got != blob {
        die("feed-served bytes differ from the origin artifact".into());
    }
    if hex_sha256(&got) != want {
        die("feed-served sha256 differs from the pin".into());
    }

    // 6. SELF-DISCRIMINATION (warm): a wrong pinned hash must red `warm`.
    let bad_index = vec![Entry {
        path: "origin.invalid/blob-bad".to_string(),
        url: format!("http://127.0.0.1:{origin_port}/blob"),
        sha256: "0".repeat(64),
    }];
    let bad_store =
        std::env::temp_dir().join(format!("td-feed-selftest-bad-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&bad_store);
    if warm(&bad_index, &bad_store).is_ok() {
        die("warm ACCEPTED a wrong sha256 — verification is not load-bearing".into());
    }
    let _ = std::fs::remove_dir_all(&bad_store);

    // 7. SELF-DISCRIMINATION (serve): corrupt the store byte; verify-on-serve must refuse.
    let mut corrupt = std::fs::read(&stored).expect("read stored");
    corrupt[0] ^= 0xff;
    std::fs::write(&stored, &corrupt).expect("corrupt stored");
    if try_get(&feed_url).is_ok() {
        die("feed SERVED a corrupted store artifact — verify-on-serve is not load-bearing".into());
    }

    let _ = std::fs::remove_dir_all(&store);
    println!(
        "td-feed: selftest OK — warmed + served + fetched {} bytes (sha256 {}) over loopback \
         (origin 127.0.0.1:{}, feed 127.0.0.1:{}); a wrong pinned hash reds warm and a corrupted \
         store byte reds serve (sidecar integrity)",
        blob.len(),
        want,
        origin_port,
        feed_port
    );
}

// ---------------------------------------------------------------------------------------
// cargo-proxy: a cargo SPARSE registry mirror. `cargo fetch`/`cargo build` fetch their WHOLE
// crate closure THROUGH td (fetch-then-save + verify), so cargo does the dependency
// resolution + fetching and td owns the verifying, caching, shareable egress — the generic,
// guix-free crate provisioning. Point cargo at `sparse+http://<addr>/` (source replacement).
// Three request kinds (cargo's sparse protocol):
//   GET /config.json                    -> {"dl":"http://<addr>/dl","api":"http://<addr>"}
//   GET /<idx-path>                     -> proxy+cache index.crates.io/<idx-path> (newline
//                                          JSON version metadata incl. each cksum)
//   GET /dl/<crate>/<version>/download  -> fetch static.crates.io, VERIFY sha256 == the index
//                                          cksum, cache, serve (the .crate tarball)
// Cache under STORE: index/<idx-path>, crates/<crate>-<version>.crate (the vendor set,
// shareable). (cargo `vendor` bypasses source replacement; cargo `fetch` honors it.)

/// Upstream bases (env-overridable for the hermetic selftest): the sparse index + `.crate` CDN.
fn index_base() -> String {
    std::env::var("TD_INDEX_BASE").unwrap_or_else(|_| "https://index.crates.io".into())
}
fn crates_base() -> String {
    std::env::var("TD_CRATES_BASE").unwrap_or_else(|_| "https://static.crates.io".into())
}

/// The sparse-registry index path for a crate name (lowercased): `1/{n}`, `2/{n}`,
/// `3/{c}/{n}`, else `{n[0:2]}/{n[2:4]}/{n}`.
fn index_path(name: &str) -> Option<String> {
    if name.is_empty()
        || name.len() > 64
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return None;
    }
    let n = name.to_lowercase();
    Some(match n.len() {
        1 => format!("1/{n}"),
        2 => format!("2/{n}"),
        3 => format!("3/{}/{n}", n.get(0..1)?),
        _ => format!("{}/{}/{n}", n.get(0..2)?, n.get(2..4)?),
    })
}

/// Extract the sha256 `cksum` for `version` from a sparse-index document (newline JSON).
fn cksum_for(index_text: &str, version: &str) -> Option<String> {
    let needle = format!("\"vers\":\"{version}\"");
    for line in index_text.lines() {
        if line.contains(&needle) {
            let k = "\"cksum\":\"";
            let i = line.find(k)? + k.len();
            let j = line[i..].find('"')?;
            return Some(line[i..i + j].to_string());
        }
    }
    None
}

/// Serve (cache-or-proxy) a sparse-index document from index.crates.io.
fn serve_index_before(
    store: &Path,
    idxpath: &str,
    guard: &ResponseGuard<'_>,
) -> Result<Vec<u8>, String> {
    guard.check().map_err(|e| e.to_string())?;
    let cache = store_path(&store.join("index"), idxpath).ok_or("unsafe index path")?;
    if let Ok(metadata) = std::fs::metadata(&cache) {
        if metadata.len() > MAX_INDEX_BYTES {
            return Err(format!(
                "cached sparse index {} exceeds its {MAX_INDEX_BYTES}-byte limit",
                cache.display()
            ));
        }
        if let Ok(bytes) = std::fs::read(&cache) {
            return Ok(bytes);
        }
    }
    let body = try_get_before(&format!("{}/{idxpath}", index_base()), guard)?;
    if u64::try_from(body.len()).unwrap_or(u64::MAX) > MAX_INDEX_BYTES {
        return Err(format!(
            "sparse index {idxpath} exceeds its {MAX_INDEX_BYTES}-byte limit"
        ));
    }
    if let Some(p) = cache.parent() {
        std::fs::create_dir_all(p).map_err(|e| format!("mkdir {}: {e}", p.display()))?;
    }
    guard.check().map_err(|e| e.to_string())?;
    write_atomic(&cache, &body)?;
    Ok(body)
}

/// Serve (cache-or-fetch+verify) a `.crate` from static.crates.io, verified against the
/// index cksum — the td-owned, verifying egress. The cache is NOT trusted blindly: a cache
/// hit is re-verified against the index cksum on every serve, and a corrupted/stale entry
/// is discarded and refetched. So the sha256==index-cksum guarantee holds for cached hits,
/// not just fresh downloads (the integrity the `warm crate`/`warm crate-local` path relies on).
fn serve_crate_before(
    store: &Path,
    cr: &str,
    ver: &str,
    guard: &ResponseGuard<'_>,
) -> Result<(File, u64), String> {
    guard.check().map_err(|e| e.to_string())?;
    let idxpath = index_path(cr).ok_or_else(|| format!("bad download crate name {cr:?}"))?;
    if ver.is_empty()
        || ver.len() > 128
        || !ver
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'+' | b'_'))
    {
        return Err(format!("bad download version {ver:?}"));
    }
    let cache = store_path(&store.join("crates"), &format!("{cr}-{ver}.crate"))
        .ok_or("unsafe crate name")?;
    guard.check().map_err(|e| e.to_string())?;
    let idx = serve_index_before(store, &idxpath, guard)?;
    guard.check().map_err(|e| e.to_string())?;
    let cksum = cksum_for(&String::from_utf8_lossy(&idx), ver)
        .ok_or_else(|| format!("no cksum for {cr} {ver} in the index"))?;
    if cache.is_file() {
        match snapshot_for_serve_before(&cache, guard) {
            Ok((file, got)) => {
                if got == cksum {
                    let len = file
                        .metadata()
                        .map_err(|e| format!("stat {}: {e}", cache.display()))?
                        .len();
                    return Ok((file, len));
                }
            }
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::BrokenPipe | io::ErrorKind::TimedOut
                ) =>
            {
                return Err(e.to_string());
            }
            Err(_) => {}
        }
        // Corrupted/stale cache entry — drop it and refetch rather than serve bad bytes.
        let _ = std::fs::remove_file(&cache);
    }
    let url = format!("{}/crates/{cr}/{cr}-{ver}.crate", crates_base());
    if let Some(p) = cache.parent() {
        std::fs::create_dir_all(p).map_err(|e| format!("mkdir {}: {e}", p.display()))?;
    }
    guard.check().map_err(|e| e.to_string())?;
    download_verified_before(&url, &cache, &cksum, Some(guard))?;
    guard.check().map_err(|e| e.to_string())?;
    let (file, got) = snapshot_for_serve_before(&cache, guard)
        .map_err(|e| format!("snapshot {}: {e}", cache.display()))?;
    if got != cksum {
        return Err(format!(
            "sha256 mismatch for {cr} {ver}: index cksum {cksum}"
        ));
    }
    let len = file
        .metadata()
        .map_err(|e| format!("stat {}: {e}", cache.display()))?
        .len();
    Ok((file, len))
}

enum CargoBody {
    Bytes(Vec<u8>),
    File(File, u64),
}

/// Route one cargo sparse-registry request. `base` is HOST:PORT for the config URLs.
#[cfg(test)]
fn cargo_route(store: &Path, base: &str, path: &str) -> Result<CargoBody, String> {
    let guard = ResponseGuard::without_client(Instant::now() + RESPONSE_DEADLINE);
    cargo_route_before(store, base, path, &guard)
}

fn cargo_route_before(
    store: &Path,
    base: &str,
    path: &str,
    guard: &ResponseGuard<'_>,
) -> Result<CargoBody, String> {
    guard.check().map_err(|e| e.to_string())?;
    if path == "/config.json" {
        return Ok(CargoBody::Bytes(
            format!("{{\"dl\":\"http://{base}/dl\",\"api\":\"http://{base}\"}}").into_bytes(),
        ));
    }
    // The download endpoint is `/dl/<crate>/<version>/download`. A crate whose name starts with
    // "dl" has a sparse-index path that ALSO starts with `dl/` (e.g. `dlv-list` -> `dl/v-/dlv-list`),
    // so `/dl/` alone is ambiguous. It is only a download when the shape matches exactly (3 parts,
    // last == "download"); no index path can be `/dl/XX/download` (that needs a crate named
    // "download", whose index path is `do/wn/download`, not `dl/...`). Anything else under `/dl/`
    // is such an index path — fall through to serve_index, don't 404 it.
    if let Some(rest) = path.strip_prefix("/dl/") {
        let parts: Vec<&str> = rest.split('/').collect();
        if parts.len() == 3
            && parts[2] == "download"
            && !parts[0].is_empty()
            && !parts[1].is_empty()
        {
            let (file, len) = serve_crate_before(store, parts[0], parts[1], guard)?;
            return Ok(CargoBody::File(file, len));
        }
    }
    guard.check().map_err(|e| e.to_string())?;
    serve_index_before(store, path.trim_start_matches('/'), guard).map(CargoBody::Bytes)
}

/// Handle one cargo request: parse `GET /<path>`, route, stream.
fn handle_cargo_conn(mut conn: TcpStream, store: &Path, base: &str) -> io::Result<()> {
    conn.set_write_timeout(Some(REQUEST_IO_TIMEOUT))?;
    let head = read_request_head(&mut conn)?;
    let response_deadline = Instant::now() + RESPONSE_DEADLINE;
    let line_end = head
        .iter()
        .position(|byte| *byte == b'\n')
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing request line"))?;
    if line_end > MAX_REQUEST_LINE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "cargo-proxy request line exceeds its byte limit",
        ));
    }
    let req_line = std::str::from_utf8(head.get(..line_end).unwrap_or_default())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "non-UTF-8 request line"))?;
    let mut parts = req_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("");
    if method != "GET" {
        return respond_before(
            &mut conn,
            405,
            "Method Not Allowed",
            b"method not allowed\n",
            response_deadline,
        );
    }
    let route = {
        let guard = ResponseGuard::client(response_deadline, &conn);
        cargo_route_before(store, base, target, &guard)
    };
    match route {
        Ok(CargoBody::Bytes(bytes)) => {
            respond_before(&mut conn, 200, "OK", &bytes, response_deadline)
        }
        Ok(CargoBody::File(mut file, len)) => {
            respond_file_before(&mut conn, &mut file, len, response_deadline)
        }
        Err(e) => {
            eprintln!("td-feed cargo-proxy: {target}: {e}");
            let code = if e.starts_with("no cksum") || e.starts_with("bad download") {
                404
            } else {
                502
            };
            respond_before(&mut conn, code, "Error", e.as_bytes(), response_deadline)
        }
    }
}

fn cargo_proxy_worker(listener: TcpListener, store: Arc<PathBuf>, base: Arc<String>) {
    for conn in listener.incoming() {
        let Ok(conn) = conn else { continue };
        let _ = handle_cargo_conn(conn, &store, &base);
    }
}

/// Run the cargo-proxy with a fixed worker count. The listen backlog supplies
/// backpressure without allocating one default-stack thread per slow peer.
fn cargo_proxy_loop(listener: TcpListener, store: Arc<PathBuf>, base: String) {
    let base = Arc::new(base);
    for worker in 1..SERVE_WORKERS {
        let Ok(worker_listener) = listener.try_clone() else {
            break;
        };
        let worker_store = Arc::clone(&store);
        let worker_base = Arc::clone(&base);
        let _ = std::thread::Builder::new()
            .name(format!("td-cargo-proxy-{worker}"))
            .stack_size(SERVE_WORKER_STACK_BYTES)
            .spawn(move || cargo_proxy_worker(worker_listener, worker_store, worker_base));
    }
    cargo_proxy_worker(listener, store, base);
}

/// Hermetic loopback selftest of the cargo-proxy: a mock index/static.crates.io on 127.0.0.1
/// (TD_INDEX_BASE/TD_CRATES_BASE), the proxy fetches a `.crate` THROUGH it and verifies it
/// against the index cksum; a crate whose bytes mismatch its index cksum is refused (the
/// verifying egress is load-bearing). Offline (std::net only).
fn cargo_proxy_selftest() {
    let cbytes: Vec<u8> = (0u16..2048).map(|x| (x % 251) as u8).collect();
    let cksum = hex_sha256(&cbytes);
    let badbytes = b"corrupt-upstream-bytes".to_vec();
    let badcksum = "0".repeat(64); // the index claims this; the served bytes won't match

    let up = TcpListener::bind("127.0.0.1:0").expect("bind upstream");
    let uport = up.local_addr().unwrap().port();
    let (cb, bb, ck) = (cbytes.clone(), badbytes.clone(), cksum.clone());
    std::thread::spawn(move || loop {
        match up.accept() {
            Ok((mut c, _)) => {
                let mut buf = [0u8; 1024];
                let n = c.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]);
                let path = req.split_whitespace().nth(1).unwrap_or("");
                let body: Vec<u8> = match path {
                    "/un/ar/unarray" => format!(
                        "{{\"name\":\"unarray\",\"vers\":\"0.1.0\",\"deps\":[],\"cksum\":\"{ck}\",\"features\":{{}},\"yanked\":false}}\n"
                    ).into_bytes(),
                    "/3/b/bad" => format!(
                        "{{\"name\":\"bad\",\"vers\":\"1.0.0\",\"deps\":[],\"cksum\":\"{badcksum}\",\"features\":{{}},\"yanked\":false}}\n"
                    ).into_bytes(),
                    // a crate whose name starts with "dl": its sparse-index path is `dl/te/dltest`,
                    // which collides with the `/dl/` download prefix (the dlv-list bug regression).
                    "/dl/te/dltest" => format!(
                        "{{\"name\":\"dltest\",\"vers\":\"1.0.0\",\"deps\":[],\"cksum\":\"{ck}\",\"features\":{{}},\"yanked\":false}}\n"
                    ).into_bytes(),
                    "/crates/unarray/unarray-0.1.0.crate" => cb.clone(),
                    "/crates/bad/bad-1.0.0.crate" => bb.clone(),
                    _ => Vec::new(),
                };
                // Respond directly: the request was already read above; serve_once would
                // re-read and block, since the client is now awaiting the response.
                let _ = respond(&mut c, 200, "OK", &body);
            }
            Err(_) => break,
        }
    });
    let ubase = format!("http://127.0.0.1:{uport}");
    std::env::set_var("TD_INDEX_BASE", &ubase);
    std::env::set_var("TD_CRATES_BASE", &ubase);

    let store =
        std::env::temp_dir().join(format!("td-cargo-proxy-selftest-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&store);
    std::fs::create_dir_all(&store).expect("mkdir store");
    let plist = TcpListener::bind("127.0.0.1:0").expect("bind proxy");
    let pport = plist.local_addr().unwrap().port();
    {
        let (s, b) = (Arc::new(store.clone()), format!("127.0.0.1:{pport}"));
        std::thread::spawn(move || cargo_proxy_loop(plist, s, b));
    }
    let proxy = format!("http://127.0.0.1:{pport}");

    let cfg = try_get(&format!("{proxy}/config.json"))
        .unwrap_or_else(|e| die(format!("config.json: {e}")));
    if !String::from_utf8_lossy(&cfg).contains("\"dl\"") {
        die("config.json missing dl".into());
    }
    let got = try_get(&format!("{proxy}/dl/unarray/0.1.0/download"))
        .unwrap_or_else(|e| die(format!("dl unarray: {e}")));
    if got != cbytes || hex_sha256(&got) != cksum {
        die("proxy-served crate differs from upstream / its index cksum".into());
    }
    if !store.join("crates/unarray-0.1.0.crate").exists() {
        die("proxy did not cache the fetched crate".into());
    }
    // Cache-integrity: a CACHE HIT is re-verified against the index cksum, not trusted
    // blindly. Corrupt the cached crate, then fetch again — the proxy must reject the bad
    // cached bytes, refetch from upstream, serve the correct bytes, and heal the cache.
    std::fs::write(
        store.join("crates/unarray-0.1.0.crate"),
        b"corrupted-cache-bytes",
    )
    .unwrap_or_else(|e| die(format!("corrupt cache: {e}")));
    let healed = try_get(&format!("{proxy}/dl/unarray/0.1.0/download"))
        .unwrap_or_else(|e| die(format!("dl unarray after cache corruption: {e}")));
    if healed != cbytes {
        die("proxy SERVED a corrupted cache entry — a cache hit is trusted without re-verifying its index cksum".into());
    }
    if std::fs::read(store.join("crates/unarray-0.1.0.crate")).unwrap_or_default() != cbytes {
        die("proxy did not heal the corrupted cache entry after refetch".into());
    }
    if try_get(&format!("{proxy}/dl/bad/1.0.0/download")).is_ok() {
        die("proxy SERVED a crate whose bytes mismatch the index cksum — verify-on-fetch is not load-bearing".into());
    }
    // Regression (the dlv-list bug): a crate whose name starts with "dl" has a sparse-index path
    // starting with `dl/` (dltest -> /dl/te/dltest). The proxy must serve it as an INDEX, not
    // mis-route it to the `/dl/<crate>/<version>/download` handler and 404 it.
    let idx = try_get(&format!("{proxy}/dl/te/dltest")).unwrap_or_else(|e| {
        die(format!(
            "dl-prefixed sparse-index path failed (the dlv-list collision): {e}"
        ))
    });
    if !String::from_utf8_lossy(&idx).contains("\"name\":\"dltest\"") {
        die("proxy did not serve the dl-prefixed path as a sparse index (download/index route collision)".into());
    }
    let _ = std::fs::remove_dir_all(&store);
    println!(
        "td-feed: cargo-proxy selftest OK — fetched + verified a crate through the proxy (upstream \
         127.0.0.1:{uport}, proxy 127.0.0.1:{pport}, cached); a crate whose bytes mismatch its index \
         cksum is refused; a corrupted cache hit is re-verified, refetched, and healed"
    );
}

// =======================================================================================
// warm <action> — the STRUCTURED host-PREP orchestration that consolidates the former
// tools/warm-{cargo-proxy,cargo-proxy-local,bootstrap-sources,kernel-headers{,-x86_64}}.sh
// shell scripts into one typed, in-process subcommand (move-off-shell). These run on the
// HOST during `td-builder check`'s network-permitted prelude (the offline loop has no
// egress) and are BEST-EFFORT by design: a runner without required host tools/network warns to
// stderr and skips (exit 0) — the heavy `rust-*` / `bootstrap-*` gates that CONSUME the
// warmed outputs fail loudly if they actually run cold. The crown-jewel win over the
// shell: the cargo-proxy is bound IN-PROCESS, so we know its loopback address immediately
// — no background process, no log-file scrape, no `sed` parse, no sleep-poll.
//
// Paths resolve relative to the repo root (the prelude's CWD); TD_ROOT overrides it.

/// The repo root: $TD_ROOT, else the current directory.
fn repo_root() -> PathBuf {
    std::env::var_os("TD_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

/// Is `cmd` an executable on PATH? (best-effort `command -v` equivalent).
fn have_cmd(cmd: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let Ok(path) = std::env::var("PATH") else {
        return false;
    };
    path.split(':').filter(|d| !d.is_empty()).any(|dir| {
        let p = Path::new(dir).join(cmd);
        std::fs::metadata(&p)
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    })
}

fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// Count `*.crate` files directly under `dir`.
fn count_crates(dir: &Path) -> usize {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .filter(|e| e.path().extension().is_some_and(|x| x == "crate"))
                .count()
        })
        .unwrap_or(0)
}

/// Copy every `*.crate` from `from` into `to`; returns the count copied.
fn copy_crates(from: &Path, to: &Path) -> usize {
    let mut n = 0;
    if let Ok(rd) = std::fs::read_dir(from) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().is_some_and(|x| x == "crate") {
                if let Some(name) = p.file_name() {
                    if std::fs::copy(&p, to.join(name)).is_ok() {
                        n += 1;
                    }
                }
            }
        }
    }
    n
}

/// A vendor set is COMPLETE only when this marker sits beside its `.crate` files. It is
/// written LAST — after every crate is copied — so a warm killed mid-publish leaves no marker
/// and the next warm redoes it. Without it a partial cache (`count_crates >= 1`) reads as done
/// yet fails the build-time set-equality gate and never self-heals. The marker is not a
/// `.crate`, so the build's `stage_verified_vendor` scan ignores it; a re-warm's vendor wipe
/// clears any stale marker, so it is present only after a fully published set.
///
/// Write it ATOMICALLY (temp + rename): a bare `fs::write` truncates the target to 0 bytes
/// first, so a failed write (ENOSPC — likely the same reason the copy above failed) would
/// leave a 0-byte `.warm-complete`; that no longer reads as done (an empty first line
/// matches no digest), but a torn WRITE could still leave a plausible-looking one,
/// re-introducing the fail-open the marker exists to close. rename is atomic on the vendor
/// dir's filesystem, so the marker either does not exist or is a complete record.
/// The marker records WHICH Cargo.lock the set was published from, because a
/// vendor set is only complete FOR one lock. Before this it recorded a count
/// alone, so a dependency bump left the old set marked done: the next warm
/// reported "already warm" and skipped, and the build's set-equality gate then
/// rejected the closure the lock no longer describes — a failure whose message
/// points at the vendor dir rather than at the bump that invalidated it.
///
/// A marker written by the old scheme carries a count where the digest belongs,
/// never matches, and so reads as cold exactly once, re-warms, and is replaced.
/// That is the whole migration; there is no marker to convert.
fn warm_complete_text(n: usize, lock_digest: &str) -> String {
    format!("{lock_digest}\n{n}\n")
}

fn mark_warm_complete(vendor: &Path, n: usize, lock_digest: &str) {
    let tmp = vendor.join(".warm-complete.tmp");
    if std::fs::write(&tmp, warm_complete_text(n, lock_digest)).is_ok()
        && std::fs::rename(&tmp, vendor.join(".warm-complete")).is_ok()
    {
        return;
    }
    let _ = std::fs::remove_file(&tmp);
}

/// Complete AND for this lock. An unreadable marker, an unreadable lock, and a
/// digest that does not match are all "not warm": every one of them means this
/// cache cannot be shown to describe the lock in hand, and re-warming is
/// cheaper than the set-equality failure that follows from guessing.
fn is_warm_complete(vendor: &Path, lock: &Path) -> bool {
    let Some(marked) = read_marker(&vendor.join(".warm-complete")) else {
        return false;
    };
    let Some(want) = lock_digest(lock) else {
        return false;
    };
    marked.lines().next().map(str::trim) == Some(want.as_str())
}

/// The digest the marker carries: sha256 over the lock's BYTES, so any bump to
/// a pin, a version or a checksum changes it.
///
/// Bounded by the same `MAX_CARGO_LOCK_BYTES` the fetch enforces, and refuses a
/// non-regular file: a marker or lock replaced by a symlink to something huge
/// must read as "not warm", not exhaust memory deciding.
fn lock_digest(lock: &Path) -> Option<String> {
    read_locked_cargo_sources(lock).ok().map(|(_, digest)| digest)
}

/// The marker is one digest line and one count line. Read it under a small
/// ceiling and only as a regular file, for the reason above.
fn read_marker(path: &Path) -> Option<String> {
    const MAX_MARKER_BYTES: u64 = 4096;
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > MAX_MARKER_BYTES {
        return None;
    }
    std::fs::read_to_string(path).ok()
}

/// Run `cmd` with stdio discarded; true on a zero exit (best-effort, never panics).
fn run_quiet(cmd: &mut Command) -> bool {
    cmd.stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Start the cargo-proxy on an OS-picked loopback port IN THIS PROCESS; returns its
/// `HOST:PORT`. A background thread runs the serve loop over `store`. Because we hold the
/// bound listener, the address is known immediately — no subprocess + log scrape + poll
/// (the fragile shell this replaces). The connect from cargo/try_get succeeds against the
/// already-bound listener's backlog, so no readiness wait is needed.
fn start_cargo_proxy(store: &Path) -> Result<String, String> {
    std::fs::create_dir_all(store).map_err(|e| format!("mkdir {}: {e}", store.display()))?;
    let listener =
        TcpListener::bind("127.0.0.1:0").map_err(|e| format!("bind cargo-proxy: {e}"))?;
    let addr = listener
        .local_addr()
        .map_err(|e| format!("cargo-proxy local_addr: {e}"))?
        .to_string();
    let s = Arc::new(store.to_path_buf());
    let base = addr.clone();
    std::thread::spawn(move || cargo_proxy_loop(listener, s, base));
    Ok(addr)
}

const MAX_CARGO_LOCK_BYTES: u64 = 16 * 1024 * 1024;
const MAX_CARGO_LOCK_PACKAGES: usize = 8192;

#[derive(Clone, Debug, PartialEq, Eq)]
struct LockedRegistryPackage {
    name: String,
    version: String,
    checksum: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct LockedCargoSources {
    registry: Vec<LockedRegistryPackage>,
    git_packages: usize,
}

/// Parse only Cargo.lock's generated package records. Path/workspace packages
/// need no fetch; registry packages must carry their exact SHA-256; Git
/// packages are counted but deliberately never handed to a transport here.
fn parse_locked_cargo_sources(text: &str) -> Result<LockedCargoSources, String> {
    let mut result = LockedCargoSources::default();
    let mut seen_registry = std::collections::BTreeSet::new();
    let (mut name, mut version, mut source, mut checksum) =
        (String::new(), String::new(), None, None);
    let mut in_package = false;
    let mut packages = 0usize;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed == "[[package]]" {
            if in_package {
                account_locked_cargo_source(
                    &name,
                    &version,
                    source.as_deref(),
                    checksum.as_deref(),
                    &mut result,
                    &mut seen_registry,
                )?;
            }
            packages = packages
                .checked_add(1)
                .ok_or("Cargo.lock package count overflow")?;
            if packages > MAX_CARGO_LOCK_PACKAGES {
                return Err(format!(
                    "Cargo.lock contains more than {MAX_CARGO_LOCK_PACKAGES} packages"
                ));
            }
            in_package = true;
            name.clear();
            version.clear();
            source = None;
            checksum = None;
        } else if trimmed.starts_with('[') {
            if in_package {
                account_locked_cargo_source(
                    &name,
                    &version,
                    source.as_deref(),
                    checksum.as_deref(),
                    &mut result,
                    &mut seen_registry,
                )?;
                in_package = false;
            }
        } else if in_package {
            let Some((key, value)) = trimmed.split_once('=') else {
                continue;
            };
            let key = key.trim();
            if !matches!(key, "name" | "version" | "source" | "checksum") {
                continue;
            }
            let value = value.trim();
            let value = value
                .strip_prefix('"')
                .and_then(|value| value.strip_suffix('"'))
                .ok_or_else(|| format!("Cargo.lock package field `{key}' is not a string"))?;
            match key {
                "name" => name = value.to_string(),
                "version" => version = value.to_string(),
                "source" => source = Some(value.to_string()),
                "checksum" => checksum = Some(value.to_string()),
                _ => {}
            }
        }
    }
    if in_package {
        account_locked_cargo_source(
            &name,
            &version,
            source.as_deref(),
            checksum.as_deref(),
            &mut result,
            &mut seen_registry,
        )?;
    }
    Ok(result)
}

fn account_locked_cargo_source(
    name: &str,
    version: &str,
    source: Option<&str>,
    checksum: Option<&str>,
    result: &mut LockedCargoSources,
    seen_registry: &mut std::collections::BTreeSet<(String, String)>,
) -> Result<(), String> {
    let Some(source) = source else {
        return Ok(());
    };
    if name.is_empty() || version.is_empty() {
        return Err(format!(
            "Cargo.lock external package from `{source}' has no name or version"
        ));
    }
    if source.starts_with("git+") {
        result.git_packages = result
            .git_packages
            .checked_add(1)
            .ok_or("Cargo.lock Git package count overflow")?;
        return Ok(());
    }
    if !source.starts_with("registry+") {
        return Err(format!(
            "Cargo.lock package `{name}-{version}' has unsupported source `{source}'"
        ));
    }
    let checksum = checksum.filter(|value| {
        value.len() == 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    });
    let Some(checksum) = checksum else {
        return Err(format!(
            "Cargo.lock registry package `{name}-{version}' has no canonical SHA-256 checksum"
        ));
    };
    if index_path(name).is_none()
        || version.is_empty()
        || version.len() > 128
        || !version
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'+' | b'_'))
    {
        return Err(format!(
            "Cargo.lock registry package has an unsafe name or version: `{name}-{version}'"
        ));
    }
    if !seen_registry.insert((name.to_string(), version.to_string())) {
        return Err(format!(
            "Cargo.lock repeats registry package destination `{name}-{version}'"
        ));
    }
    result.registry.push(LockedRegistryPackage {
        name: name.to_string(),
        version: version.to_string(),
        checksum: checksum.to_string(),
    });
    Ok(())
}

/// Read and hash the same bounded lock bytes used to select the archives.
fn read_locked_cargo_sources(path: &Path) -> Result<(LockedCargoSources, String), String> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(OPEN_REGULAR_NOFOLLOW)
        .open(path)
        .map_err(|e| {
            format!(
                "open Cargo.lock {} (regular file required; final symlinks refused): {e}",
                path.display()
            )
        })?;
    let metadata = file
        .metadata()
        .map_err(|e| format!("stat Cargo.lock {}: {e}", path.display()))?;
    if !metadata.is_file() || metadata.len() > MAX_CARGO_LOCK_BYTES {
        return Err(format!(
            "Cargo.lock {} is not a regular file within the {MAX_CARGO_LOCK_BYTES}-byte limit",
            path.display()
        ));
    }
    let mut text = String::new();
    file.take(MAX_CARGO_LOCK_BYTES + 1)
        .read_to_string(&mut text)
        .map_err(|e| format!("read Cargo.lock {}: {e}", path.display()))?;
    if text.len() as u64 > MAX_CARGO_LOCK_BYTES {
        return Err(format!(
            "Cargo.lock {} exceeds the {MAX_CARGO_LOCK_BYTES}-byte limit",
            path.display()
        ));
    }
    if !text.lines().any(|line| line.trim() == "[[package]]") {
        return Err(format!(
            "Cargo.lock {} has no package records; select a generated lockfile",
            path.display()
        ));
    }
    Ok((
        parse_locked_cargo_sources(&text)?,
        hex_sha256(text.as_bytes()),
    ))
}

fn registry_archive_name(package: &LockedRegistryPackage) -> String {
    format!("{}-{}.crate", package.name, package.version)
}

fn registry_feed_path(package: &LockedRegistryPackage) -> String {
    // The lock checksum is part of the key, so distinct revisions or registries
    // cannot replace each other's bytes at a name/version-only destination.
    format!(
        "cargo/sha256/{}/{}",
        package.checksum,
        registry_archive_name(package)
    )
}

fn transfer_locked_registry(
    lock: &Path,
    archives: &Path,
    store: &Path,
    consumer_base: Option<&str>,
) -> Result<(), String> {
    let (sources, _) = read_locked_cargo_sources(lock)?;
    let transfer = match consumer_base {
        Some(base) => RegistryTransfer::Consume(base),
        None => RegistryTransfer::Export(store),
    };
    transfer_registry_sources(&sources, archives, transfer)
}

#[derive(Clone, Copy)]
enum RegistryTransfer<'a> {
    Export(&'a Path),
    Consume(&'a str),
}

fn transfer_registry_sources(
    sources: &LockedCargoSources,
    archives: &Path,
    transfer: RegistryTransfer<'_>,
) -> Result<(), String> {
    let transfer = match transfer {
        RegistryTransfer::Consume(base) => RegistryTransfer::Consume(consumer_feed_base(base)?),
        RegistryTransfer::Export(store) => RegistryTransfer::Export(store),
    };
    let consume = matches!(transfer, RegistryTransfer::Consume(_));
    let action = if consume { "consume" } else { "export" };
    let completed = if consume { "acquired" } else { "exported" };
    let mut names = std::collections::BTreeSet::new();
    for package in &sources.registry {
        let name = registry_archive_name(package);
        if !names.insert(name.clone()) {
            return Err(format!(
                "Cargo.lock packages collide at registry archive filename {name}"
            ));
        }
    }
    let mut failures = Vec::new();
    for package in &sources.registry {
        let name = registry_archive_name(package);
        let archive = archives.join(&name);
        let path = registry_feed_path(package);
        let result = match transfer {
            RegistryTransfer::Consume(base) => {
                let guard = ResponseGuard::without_client(Instant::now() + SOURCE_CONSUMER_TIMEOUT);
                download_verified_policy(
                    &format!("{base}/{path}"),
                    &archive,
                    &package.checksum,
                    Some(&guard),
                    false,
                )
            }
            RegistryTransfer::Export(store) => {
                export_verified_file(&archive, &store.join(path), &package.checksum)
            }
        };
        match result {
            Ok(()) => eprintln!(">> td-feed {action} cargo: {name} verified"),
            Err(error) => failures.push(format!("{name} SHA-256 {}: {error}", package.checksum)),
        }
    }
    eprintln!(
        ">> td-feed {action} cargo: {} registry archive(s) selected; {} Git package(s) require separate recipe-pinned source archives; no Git transport",
        sources.registry.len(), sources.git_packages,
    );
    if failures.is_empty() {
        Ok(())
    } else {
        let remedy = if consume {
            "Fix local destination errors; for missing or mismatched host entries, warm the named bytes on the host and run td-feed export cargo LOCK ARCHIVES."
        } else {
            "Populate missing or repair corrupt files in the host ARCHIVES directory, fix any feed-store errors above, then retry export."
        };
        Err(format!(
            "{} registry archive(s) were not {completed}:\n{}\n{remedy} Neither cargo transfer command downloads upstream or follows redirects.",
            failures.len(), failures.join("\n"),
        ))
    }
}

/// Fetch only the registry members of the committed lock through td's
/// verifying sparse-index/static-crate egress. Git members are represented by
/// separately pinned source archives and are intentionally never contacted.
/// Returns the locked sources AND the sha256 of the bytes they were parsed
/// from. The digest travels with the parse deliberately: the completion marker
/// records which lock a vendor set was published from, and re-reading the path
/// afterwards would let a lock edited mid-fetch stamp the NEW digest onto the
/// set selected by the OLD one — sealing a mismatch as complete (review
/// finding).
fn fetch_locked_registry(
    lock_path: &Path,
    store: &Path,
) -> Result<(LockedCargoSources, String), String> {
    let (sources, digest) = read_locked_cargo_sources(lock_path)?;
    for package in &sources.registry {
        let guard = ResponseGuard {
            deadline: Instant::now() + RESPONSE_DEADLINE,
            client: None,
        };
        let (mut file, _) =
            serve_crate_before(store, &package.name, &package.version, &guard)?;
        let got = reader_sha256_before(&mut file, MAX_ARTIFACT_BYTES, Some(&guard))
            .map_err(|e| format!("hash {}-{}: {e}", package.name, package.version))?;
        if got != package.checksum {
            return Err(format!(
                "Cargo.lock checksum for {}-{} is {} but verified registry bytes are {got}",
                package.name, package.version, package.checksum
            ));
        }
    }
    Ok((sources, digest))
}

/// The cargo `config.toml` body that routes crates.io through the proxy at `addr` (sparse
/// source replacement). Kept as the proxy protocol selftest oracle; dependency warming
/// now reads Cargo.lock itself and never launches Cargo or its Git transport.
fn cargo_config(addr: &str) -> String {
    format!(
        "[source.crates-io]\nreplace-with = \"td-proxy\"\n[source.td-proxy]\nregistry = \"sparse+http://{addr}/\"\n"
    )
}

/// Advance the multi-line-string state (`"""` / `'''`) across one line, reporting
/// whether the line STARTED inside one. Published manifests do carry bracketed
/// column-0 lines inside `package.metadata.release` changelog templates, and a
/// `[workspace]` in that body is string content, not a table.
fn scan_multiline(line: &str, open: &mut Option<&'static str>) -> bool {
    let started_open = open.is_some();
    let mut rest = line;
    loop {
        match *open {
            Some(delim) => match rest.find(delim) {
                Some(i) => {
                    *open = None;
                    rest = rest.get(i + delim.len()..).unwrap_or("");
                }
                None => return started_open,
            },
            None => {
                let (i, delim) = match (rest.find("\"\"\""), rest.find("'''")) {
                    (Some(b), Some(s)) if s < b => (s, "'''"),
                    (Some(b), _) => (b, "\"\"\""),
                    (None, Some(s)) => (s, "'''"),
                    (None, None) => return started_open,
                };
                *open = Some(delim);
                rest = rest.get(i + delim.len()..).unwrap_or("");
            }
        }
    }
}

/// Already a workspace root? A sub-table alone (`[workspace.metadata]`) makes it one.
fn declares_workspace(text: &str) -> bool {
    let mut open = None;
    for line in text.lines() {
        if !scan_multiline(line, &mut open) && is_workspace_header(line) {
            return true;
        }
    }
    false
}

/// A `[workspace]` table header. Tolerates the whitespace and quoting TOML permits:
/// a header read as ordinary text earns the file a SECOND `[workspace]`, which is a
/// parse error rather than a detach.
fn is_workspace_header(line: &str) -> bool {
    let Some(inner) = line.trim_start().strip_prefix('[') else {
        return false;
    };
    let Some(rest) = inner
        .trim_start()
        .trim_start_matches('"')
        .strip_prefix("workspace")
    else {
        return false;
    };
    matches!(rest.chars().next(), Some(']' | '.' | '"')) || rest.starts_with(char::is_whitespace)
}

/// Stop cargo's upward manifest walk at this extracted crate: it lands under the
/// repo's own `.td-build-cache`, so the walk reaches td's workspace root and cargo
/// refuses ("current package believes it's in a workspace when it's not"). An empty
/// `[workspace]` is cargo's own remedy, and unlike an `exclude` entry it holds
/// wherever the checkout sits.
///
/// Only this scratch copy is rewritten; nothing content-addressed reads it.
fn detach_from_workspace(manifest: &Path) -> Result<(), String> {
    let text = std::fs::read_to_string(manifest)
        .map_err(|e| format!("read {}: {e}", manifest.display()))?;
    if declares_workspace(&text) {
        return Ok(());
    }
    let mut out = text;
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str("\n[workspace]\n");
    std::fs::write(manifest, out).map_err(|e| format!("write {}: {e}", manifest.display()))
}

/// warm crate CRATE VERSION [DEST] — provision a crates.io package's SOURCE tree + its FULL
/// locked registry closure through td's verifying egress (each `.crate` sha256 must equal
/// both the Cargo.lock checksum and crates.io sparse-index cksum). Git entries are counted
/// but never fetched; their recipe-owned fixed-output archives use `warm sources`. Leaves, for the
/// offline gate to intern + build via TD_VENDOR_DIR:
///   .td-build-cache/crate-vendor/<dest>/src/<crate>-<ver>/  the extracted source tree
///   .td-build-cache/crate-vendor/<dest>/vendor/*.crate      the locked dep closure
fn warm_crate(root: &Path, krate: &str, ver: &str, dest: &str) {
    let _preparation_lock = match vendor::lock_job(root, dest) {
        Ok(lock) => lock,
        Err(error) => {
            eprintln!("td-feed warm crate: {error}");
            return;
        }
    };
    let cv = root.join(".td-build-cache/crate-vendor").join(dest);
    let srcparent = cv.join("src");
    let srcdir = srcparent.join(format!("{krate}-{ver}"));
    let vendor = cv.join("vendor");
    let work = cv.join("work");
    let srccrate = work.join(format!("{krate}-{ver}.crate"));

    if srcdir.join("Cargo.toml").is_file()
        && srccrate.is_file()
        && is_warm_complete(&vendor, &srcdir.join("Cargo.lock"))
    {
        eprintln!(
            "td-feed warm crate: {krate}-{ver} already warm ({} crates) in {}",
            count_crates(&vendor),
            cv.display()
        );
        return;
    }
    if !have_cmd("tar") {
        eprintln!("td-feed warm crate: no tar — skipping {krate}-{ver}");
        return;
    }

    let _ = std::fs::remove_dir_all(&work);
    let proxy_store = work.join("proxy-store");
    let addr = match start_cargo_proxy(&proxy_store) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("td-feed warm crate: {e} — skipping {krate}-{ver}");
            return;
        }
    };

    // 1) Grab the SOURCE crate through the proxy's VERIFYING /dl endpoint (a plain GET — the
    //    proxy fetches static.crates.io, verifies sha256 == the index cksum, caches, serves).
    //    NOT a throwaway `cargo fetch`: a fresh resolve can FAIL where the shipped Cargo.lock's
    //    exact pins resolve (e.g. coreutils 0.9.0: a fresh ordered-multimap picks a dlv-list
    //    that isn't there). The /dl GET sidesteps resolution; deps come later from the source's
    //    OWN lock (step 3).
    let dlurl = format!("http://{addr}/dl/{krate}/{ver}/download");
    if std::fs::create_dir_all(&work).is_err()
        || crate::http::get_to_file(&dlurl, &srccrate, MAX_ARTIFACT_BYTES).is_err()
        || std::fs::metadata(&srccrate)
            .map(|m| m.len() == 0)
            .unwrap_or(true)
    {
        eprintln!("td-feed warm crate: could not stage the source crate for {krate}-{ver}");
        return;
    }

    // 2) Extract the source crate -> the source tree.
    let _ = std::fs::remove_dir_all(&srcparent);
    if std::fs::create_dir_all(&srcparent).is_err()
        || !run_quiet(
            Command::new("tar")
                .arg("-xzf")
                .arg(&srccrate)
                .arg("-C")
                .arg(&srcparent),
        )
    {
        eprintln!("td-feed warm crate: could not extract the source crate for {krate}-{ver}");
        return;
    }
    if !srcdir.join("Cargo.toml").is_file() {
        eprintln!(
            "td-feed warm crate: extracted source has no Cargo.toml at {}",
            srcdir.display()
        );
        return;
    }
    if !srcdir.join("Cargo.lock").is_file() {
        eprintln!(
            "td-feed warm crate: source {krate}-{ver} ships no Cargo.lock — cannot pin the closure"
        );
        return;
    }
    if let Err(e) = detach_from_workspace(&srcdir.join("Cargo.toml")) {
        eprintln!("td-feed warm crate: {e} — skipping {krate}-{ver}");
        return;
    }

    // 3) Fetch only the registry members of the source's OWN Cargo.lock through
    //    td's verifying egress. Git members never reach Cargo or a Git transport;
    //    their separately pinned archives are warmed by `warm sources`.
    let _ = std::fs::remove_dir_all(proxy_store.join("crates"));
    let _ = std::fs::remove_dir_all(proxy_store.join("index"));
    let (sources, digest) = match fetch_locked_registry(&srcdir.join("Cargo.lock"), &proxy_store) {
        Ok(sources) => sources,
        Err(error) => {
            eprintln!(
                "td-feed warm crate: locked registry fetch failed for {krate}-{ver}: {error}"
            );
            return;
        }
    };

    // 4) Publish the vendor set (the proxy's verified crate cache) + drop cargo build state.
    let _ = std::fs::remove_dir_all(&vendor);
    if std::fs::create_dir_all(&vendor).is_err() {
        eprintln!("td-feed warm crate: could not create vendor dir for {krate}-{ver}");
        return;
    }
    let crates_src = proxy_store.join("crates");
    let want = sources.registry.len();
    let n = copy_crates(&crates_src, &vendor);
    let _ = std::fs::remove_dir_all(srcdir.join("target"));
    if want == 0 && sources.git_packages == 0 {
        eprintln!("td-feed warm crate: Cargo.lock has no external dependencies for {krate}-{ver}");
        return;
    }
    // Mark complete ONLY if EVERY fetched crate copied. copy_crates silently drops per-file
    // errors, so a partial copy (n>=1 but < the fetched set) would otherwise be sealed by the
    // sentinel and skipped forever while the build-time set-equality gate rejects it (Codex
    // review). A short copy leaves no marker, so the next warm re-does it.
    if n != want {
        eprintln!("td-feed warm crate: copied only {n}/{want} crates for {krate}-{ver} - a copy failed; NOT marking complete so the next warm re-does it");
        return;
    }
    // What the DESTINATION holds, not only what this run copied into it. The
    // wipe above ignores its error, so a partial removal can leave stale
    // `.crate` files from an older lock beside the new ones: `n == want` still
    // holds, and the digest would seal a set the build then rejects for extras
    // it can never repair (review finding).
    let have = count_crates(&vendor);
    if have != want {
        eprintln!("td-feed warm crate: {krate}-{ver} vendor holds {have} crates, expected {want} - stale entries survived the wipe; NOT marking complete");
        return;
    }
    mark_warm_complete(&vendor, n, &digest);
    eprintln!(
        "td-feed warm crate: {krate}-{ver} — source + {n} registry crates and {} Git package pin(s) provisioned guix-free \
         (Cargo.lock-pinned, registry sha==index cksum; no Git transport) in {}",
        sources.git_packages,
        cv.display()
    );
}

/// warm crate-source FILE SHA256 LOCK DEST — provision the registry closure for
/// an already-warmed fixed-output source archive from the recipe's exact
/// committed lock. The archive is re-hashed to bind this warm job to its source
/// pin, but dependency selection needs neither its manifest nor an extracted
/// scratch source tree: the committed lock is the build gate's exact oracle.
fn valid_crate_source_coordinates(file: &str, sha256: &str, lock: &str, dest: &str) -> bool {
    let file_is_plain = Path::new(file).file_name() == Some(std::ffi::OsStr::new(file));
    let lock_is_plain = Path::new(lock).file_name() == Some(std::ffi::OsStr::new("Cargo.lock"))
        && Path::new(lock)
            .components()
            .all(|component| matches!(component, std::path::Component::Normal(_)));
    let dest_is_plain = Path::new(dest).file_name() == Some(std::ffi::OsStr::new(dest));
    let sha_is_hex = sha256.len() == 64
        && sha256
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'));
    file_is_plain && lock_is_plain && dest_is_plain && sha_is_hex
}

fn real_relative_file(root: &Path, relative: &str, label: &str) -> Result<PathBuf, String> {
    if relative.is_empty() {
        return Err(format!("{label} is not a plain relative path: {relative}"));
    }
    let mut current = root.to_path_buf();
    let mut components = Path::new(relative).components().peekable();
    while let Some(component) = components.next() {
        let std::path::Component::Normal(name) = component else {
            return Err(format!("{label} is not a plain relative path: {relative}"));
        };
        current.push(name);
        let metadata = std::fs::symlink_metadata(&current)
            .map_err(|error| format!("inspect {label} {}: {error}", current.display()))?;
        let final_component = components.peek().is_none();
        let valid_kind = if final_component {
            metadata.is_file()
        } else {
            metadata.is_dir()
        };
        if metadata.file_type().is_symlink() || !valid_kind {
            return Err(format!(
                "{label} traverses a symlink or wrong file type: {}",
                current.display()
            ));
        }
    }
    Ok(current)
}

fn warm_crate_source(root: &Path, file: &str, sha256: &str, lock: &str, dest: &str) {
    if !valid_crate_source_coordinates(file, sha256, lock, dest) {
        eprintln!(
            "td-feed warm crate-source: FILE, SHA256, LOCK, or DEST is malformed — skipping {dest}"
        );
        return;
    }
    let committed_lock = match real_relative_file(root, lock, "committed Cargo.lock") {
        Ok(path) => path,
        Err(error) => {
            eprintln!("td-feed warm crate-source: {error} — skipping {dest}");
            return;
        }
    };
    let archive = sources_dir().join(file);
    if file_sha256(&archive).ok().as_deref() != Some(sha256) {
        eprintln!(
            "td-feed warm crate-source: {} is absent or does not match the declared SHA-256 — skipping {dest}",
            archive.display()
        );
        return;
    }
    warm_crate_lock(root, &committed_lock, dest, "crate-source");
}

/// warm crate-local SRCDIR DEST — provision a LOCAL (in-tree) crate's locked registry
/// closure through td's verifying egress. No source crate to fetch (the source IS the in-tree dir, which
/// the gate interns itself); only the locked dep closure ->
/// .td-build-cache/crate-vendor/<dest>/vendor/*.crate — the SAME layout `warm crate` writes and
/// `provision_auto_vendor` reads (crate-vendor/<recipe-name>/vendor), so DEST is the recipe name.
fn warm_crate_local(root: &Path, srcrel: &str, dest: &str) {
    let srcdir = match std::fs::canonicalize(root.join(srcrel)) {
        Ok(p) if p.join("Cargo.lock").is_file() => p,
        _ => {
            eprintln!("td-feed warm crate-local: {dest} has no Cargo.lock at the source dir — cannot pin the closure");
            return;
        }
    };
    let lock = srcdir.join("Cargo.lock");
    warm_crate_lock(root, &lock, dest, "crate-local");
}

fn warm_crate_lock(root: &Path, lock: &Path, dest: &str, action: &str) {
    let _preparation_lock = match vendor::lock_job(root, dest) {
        Ok(lock) => lock,
        Err(error) => {
            eprintln!("td-feed warm {action}: {error}");
            return;
        }
    };
    let vendor = root
        .join(".td-build-cache/crate-vendor")
        .join(dest)
        .join("vendor");
    if is_warm_complete(&vendor, lock) {
        eprintln!(
            "td-feed warm {action}: {dest} already warm ({} crates) in {}",
            count_crates(&vendor),
            vendor.display()
        );
        return;
    }
    let work = root
        .join(".td-build-cache/crate-vendor")
        .join(format!("{dest}.work"));
    let _ = std::fs::remove_dir_all(&work);
    let proxy_store = work.join("proxy-store");
    let (sources, digest) = match fetch_locked_registry(lock, &proxy_store) {
        Ok(sources) => sources,
        Err(error) => {
            eprintln!(
                "td-feed warm {action}: locked registry fetch failed for {dest} (lock {}): {error}",
                lock.display()
            );
            return;
        }
    };
    let _ = std::fs::remove_dir_all(&vendor);
    if std::fs::create_dir_all(&vendor).is_err() {
        eprintln!("td-feed warm {action}: could not create vendor dir for {dest}");
        return;
    }
    let crates_src = proxy_store.join("crates");
    let want = sources.registry.len();
    let n = copy_crates(&crates_src, &vendor);
    let _ = std::fs::remove_dir_all(&work);
    if want == 0 && sources.git_packages == 0 {
        eprintln!("td-feed warm {action}: Cargo.lock has no external dependencies for {dest}");
        return;
    }
    // Complete only if EVERY fetched crate copied (see warm_crate) — a short copy leaves no
    // marker so the next warm re-does it, instead of sealing a partial set forever.
    if n != want {
        eprintln!("td-feed warm {action}: copied only {n}/{want} crates for {dest} - a copy failed; NOT marking complete so the next warm re-does it");
        return;
    }
    // The destination set, for the reason given in `warm_crate`.
    let have = count_crates(&vendor);
    if have != want {
        eprintln!("td-feed warm {action}: {dest} vendor holds {have} crates, expected {want} - stale entries survived the wipe; NOT marking complete");
        return;
    }
    mark_warm_complete(&vendor, n, &digest);
    eprintln!(
        "td-feed warm {action}: {dest} — {n} registry crates and {} Git package pin(s) provisioned guix-free \
         (lock {}, registry sha==index cksum; no Git transport) in {}",
        sources.git_packages,
        lock.display(),
        vendor.display()
    );
}

/// Parse `td-recipe-eval source-pins`: `<key>\t<url>\t<sha256>\t<file>`.
fn parse_source_pins(text: &str) -> Result<Vec<SourcePin>, String> {
    let mut pins = Vec::new();
    for (idx, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        let mut fields = line.split('\t');
        let (Some(key), Some(url), Some(sha256), Some(file), None) = (
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
        ) else {
            return Err(format!(
                "source-pins line {} is not four TSV fields",
                idx + 1
            ));
        };
        if key.is_empty() || url.is_empty() || sha256.is_empty() || file.is_empty() {
            return Err(format!("source-pins line {} has an empty field", idx + 1));
        }
        if file.contains('/') {
            return Err(format!(
                "source-pins line {} has non-basename file `{file}`",
                idx + 1
            ));
        }
        pins.push(SourcePin {
            key: key.to_string(),
            url: url.to_string(),
            sha256: sha256.to_lowercase(),
            file: file.to_string(),
        });
    }
    if pins.is_empty() {
        return Err("td-recipe-eval source-pins returned no pins".into());
    }
    Ok(pins)
}

fn recipe_eval_path_from_output(root: &Path, text: &str) -> Result<PathBuf, String> {
    let mut lines = text.lines().map(str::trim).filter(|line| !line.is_empty());
    let Some(path) = lines.next() else {
        return Err("recipe-eval-tool.sh printed no evaluator path".into());
    };
    if lines.next().is_some() {
        return Err("recipe-eval-tool.sh printed multiple evaluator paths".into());
    }
    let path = PathBuf::from(path);
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(root.join(path))
    }
}

fn recipe_eval_tool(root: &Path) -> Result<PathBuf, String> {
    let script = root.join("tests/recipe-eval-tool.sh");
    if !script.is_file() {
        return Err(format!(
            "no tests/recipe-eval-tool.sh under {} to resolve recipe source pins",
            root.display()
        ));
    }
    let builder = match std::env::var_os("TD_BUILDER_SELF").filter(|value| !value.is_empty()) {
        Some(path) => PathBuf::from(path),
        None => {
            let local = root.join("target/release/td-builder");
            if is_executable_file(&local) {
                local
            } else {
                std::env::var_os("PATH").and_then(|paths| {
                    std::env::split_paths(&paths).map(|dir| dir.join("td-builder"))
                        .find(|path| is_executable_file(path))
                }).ok_or("source pin resolution requires td-builder; build it with cargo build --release --manifest-path builder/Cargo.toml or install it on PATH")?
            }
        }
    };
    let out = Command::new("sh")
        .env("TD_BUILDER_SELF", builder)
        .arg(&script)
        .arg(root.join(".td-build-cache/recipe-eval"))
        .current_dir(root)
        .output()
        .map_err(|e| format!("spawn {}: {e}", script.display()))?;
    if !out.status.success() {
        return Err(format!(
            "recipe-eval-tool.sh failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    let text = String::from_utf8(out.stdout)
        .map_err(|e| format!("recipe-eval-tool.sh output not UTF-8: {e}"))?;
    let eval = recipe_eval_path_from_output(root, &text)?;
    if !is_executable_file(&eval) {
        return Err(format!(
            "recipe-eval-tool.sh returned a non-executable evaluator: {}",
            eval.display()
        ));
    }
    Ok(eval)
}

fn recipe_source_pins_result(root: &Path) -> Result<Vec<SourcePin>, String> {
    if !root.join("recipes/Cargo.toml").is_file() {
        return Err(format!(
            "no recipes/Cargo.toml under {} to resolve recipe source pins",
            root.display()
        ));
    }
    let eval = recipe_eval_tool(root)?;
    let out = Command::new(&eval)
        .arg("source-pins")
        .current_dir(root)
        .output()
        .map_err(|e| format!("spawn {} source-pins: {e}", eval.display()))?;
    if !out.status.success() {
        return Err(format!(
            "td-recipe-eval source-pins failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    let text =
        String::from_utf8(out.stdout).map_err(|e| format!("source-pins output not UTF-8: {e}"))?;
    parse_source_pins(&text)
}

/// `https://h/p` / `http://h/p` -> `h/p` (the feed serves a URL-path mirror at `GET /<h>/<p>`).
fn strip_scheme(url: &str) -> String {
    url.strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url)
        .to_string()
}

/// The shared feed directory: $TD_FEED_DIR (non-empty), else $HOME/.td/feed.
fn feed_dir() -> PathBuf {
    resolve_feed_dir(
        std::env::var("TD_FEED_DIR").ok().as_deref(),
        std::env::var("HOME").ok().as_deref(),
    )
}

/// Pure resolution of `feed_dir()` (env passed in so it is unit-testable): a
/// non-empty TD_FEED_DIR wins; else `<HOME>/.td/feed` (HOME unset -> relative).
fn resolve_feed_dir(td_feed_dir: Option<&str>, home: Option<&str>) -> PathBuf {
    match td_feed_dir {
        Some(v) if !v.trim().is_empty() => PathBuf::from(v.trim()),
        _ => PathBuf::from(home.unwrap_or(".")).join(".td/feed"),
    }
}

/// The single shared sources cache: `$HOME/.td/sources` (HOME unset/empty -> relative). The
/// flat, pin-filename-keyed dir the recipe ladder reads its warmed tarballs and generated
/// kernel-headers from. Shared across ALL worktrees with NO env override, so one
/// `td-feed warm sources` warms every tree. Deliberately NOT under `~/.td/build-daemon`
/// (which is bound read-write into every build sandbox); a pinned tarball is re-verified
/// against its sha256 on every read regardless. Keep identical to the builder/recipes copies
/// (`var_os`, so a non-UTF-8 HOME resolves the same in all three).
fn sources_dir() -> PathBuf {
    match std::env::var_os("HOME") {
        Some(h) if !h.is_empty() => PathBuf::from(h).join(".td/sources"),
        _ => PathBuf::from(".td/sources"),
    }
}

#[derive(Debug, Eq, PartialEq)]
enum FeedDaemonPolicy {
    Explicit(String),
    Disabled,
    Ensure,
}

fn feed_daemon_policy(explicit: Option<String>, no_daemon: bool) -> FeedDaemonPolicy {
    match explicit.filter(|value| !value.is_empty()) {
        Some(base) => FeedDaemonPolicy::Explicit(base),
        None if no_daemon => FeedDaemonPolicy::Disabled,
        None => FeedDaemonPolicy::Ensure,
    }
}

/// The shared feed `(addr, store)` — the ONE cross-worktree single-egress path.
/// An explicit TD_FEED_BASE always wins. Otherwise ordinary callers lazily
/// ensure the shared daemon, while a rootless check-host request sets
/// TD_FEED_NO_DAEMON because any daemon it starts would be trapped in that
/// request's private PID namespace. Such checks use the direct, verified,
/// streaming path rather than publishing a short-lived shared endpoint.
fn shared_feed() -> Option<(String, PathBuf)> {
    let dir = feed_dir();
    let store = dir.join("store");
    match feed_daemon_policy(
        std::env::var("TD_FEED_BASE").ok(),
        std::env::var_os(FEED_NO_DAEMON_ENV).is_some(),
    ) {
        FeedDaemonPolicy::Explicit(base) => return Some((strip_scheme(&base), store)),
        FeedDaemonPolicy::Disabled => return None,
        FeedDaemonPolicy::Ensure => {}
    }
    match ensure_serve_daemon(&dir) {
        Ok(addr) => Some((addr, store)),
        Err(e) => {
            eprintln!(
                ">> td-feed warm sources: could not ensure the shared feed ({e}) — \
                 falling back to a direct fetch"
            );
            None
        }
    }
}

fn source_feed_path(pin: &SourcePin) -> Result<&str, String> {
    if pin.file.is_empty()
        || pin.file == "."
        || pin.file == ".."
        || pin.file.contains('/')
        || pin.file.contains('\\')
        || reserved_download_name(&pin.file)
    {
        return Err(format!("unsafe source cache filename for {}", pin.key));
    }
    let path = pin
        .url
        .strip_prefix("https://")
        .or_else(|| pin.url.strip_prefix("http://"))
        .ok_or_else(|| format!("source {} is not an HTTP artifact", pin.key))?;
    if !source_feed_path_is_safe(path) {
        return Err(format!("source {} has no safe feed path", pin.key));
    }
    Ok(path)
}

fn source_feed_path_is_safe(path: &str) -> bool {
    !path.is_empty()
        && !path.contains(['?', '#'])
        && path.split('/').all(|part| {
            !part.is_empty()
                && part != "."
                && part != ".."
                && !part.ends_with(".sha256")
                && !reserved_download_name(part)
        })
}

fn reserved_download_name(name: &str) -> bool {
    name == ".td-feed-download.lock" || name.starts_with(".td-feed-download-")
}

fn export_source_pin(pin: &SourcePin, sources: &Path, store: &Path) -> Result<(), String> {
    let path = source_feed_path(pin)?;
    let dst = store.join(path);
    export_verified_file(&sources.join(&pin.file), &dst, &pin.sha256)
}

pub(crate) fn export_verified_file(
    source: &Path,
    dst: &Path,
    checksum: &str,
) -> Result<(), String> {
    copy_verified_file(source, dst, checksum, || {
        write_atomic(&sidecar_path(dst), format!("{checksum}\n").as_bytes())
    })
}

fn copy_verified_file(
    source: &Path,
    dst: &Path,
    checksum: &str,
    finish: impl FnOnce() -> Result<(), String>,
) -> Result<(), String> {
    let guard = ResponseGuard::without_client(Instant::now() + SOURCE_CONSUMER_TIMEOUT);
    acquire_verified(
        dst,
        checksum,
        &source.display().to_string(),
        Some(&guard),
        |temp| {
            let mut input = OpenOptions::new()
                .read(true)
                .custom_flags(OPEN_REGULAR_NOFOLLOW)
                .open(source)
                .map_err(|e| format!("open {}: {e}", source.display()))?;
            let metadata = input
                .metadata()
                .map_err(|e| format!("stat {}: {e}", source.display()))?;
            if !metadata.is_file() || metadata.len() > MAX_ARTIFACT_BYTES {
                return Err(format!(
                    "source {} is not a bounded regular file",
                    source.display()
                ));
            }
            let mut output =
                File::create(temp).map_err(|e| format!("open {}: {e}", temp.display()))?;
            let mut buffer = [0u8; 64 * 1024];
            let mut count = 0u64;
            loop {
                guard.check().map_err(|e| e.to_string())?;
                let read = input
                    .read(&mut buffer)
                    .map_err(|e| format!("read {}: {e}", source.display()))?;
                if read == 0 {
                    break;
                }
                count = count
                    .checked_add(read as u64)
                    .ok_or("source byte count overflow")?;
                if count > MAX_ARTIFACT_BYTES {
                    return Err("source exceeds artifact byte ceiling".into());
                }
                let bytes = buffer.get(..read).ok_or("invalid source read length")?;
                output
                    .write_all(bytes)
                    .map_err(|e| format!("write {}: {e}", temp.display()))?;
            }
            Ok(())
        },
        finish,
    )
}

/// Populate the read-only HTTP feed from already downloaded private bytes.
/// Missing pins fail visibly; this path never fetches or starts a daemon.
fn export_source_pins(pins: &[SourcePin], sources: &Path, store: &Path) -> Result<(), String> {
    if pins.is_empty() {
        return Err("no recipe source pins to export".into());
    }
    let mut failures = Vec::new();
    for pin in pins {
        match export_source_pin(pin, sources, store) {
            Ok(()) => eprintln!(">> td-feed export sources: {} verified", pin.file),
            Err(error) => failures.push(format!("{} ({}): {error}", pin.key, pin.file)),
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(format!("{} source archive(s) were not exported:\n{}\nWarm the named pins on the host, then retry; export never downloads.", failures.len(), failures.join("\n")))
    }
}

/// Validate the endpoint shared by every source-pin request.
pub(crate) fn consumer_feed_base(base: &str) -> Result<&str, String> {
    let base = base.trim_end_matches('/');
    let authority = base
        .strip_prefix("http://")
        .or_else(|| base.strip_prefix("https://"))
        .ok_or("TD_FEED_BASE must be an http:// or https:// endpoint")?;
    if authority.is_empty()
        || authority.bytes().any(|b| {
            b.is_ascii_whitespace()
                || b.is_ascii_control()
                || matches!(b, b'/' | b'?' | b'#' | b'@' | b'\\')
        })
    {
        return Err(
            "TD_FEED_BASE must name one server without a path, query or credentials".into(),
        );
    }
    Ok(base)
}

/// Explicit consumers never populate a producer store or fall back upstream.
fn consume_source_pins(pins: &[SourcePin], dest: &Path, base: &str) -> Result<(), String> {
    let base = consumer_feed_base(base)?;
    if pins.is_empty() {
        return Err("no recipe source pins to consume".into());
    }
    let mut failures = Vec::new();
    for pin in pins {
        let result = source_feed_path(pin).and_then(|path| {
            let guard = ResponseGuard::without_client(Instant::now() + SOURCE_CONSUMER_TIMEOUT);
            download_verified_policy(
                &format!("{base}/{path}"),
                &dest.join(&pin.file),
                &pin.sha256,
                Some(&guard),
                false,
            )
        });
        match result {
            Ok(()) => eprintln!(">> td-feed consume sources: {} verified", pin.file),
            Err(error) => {
                let failure = format!(
                    "source {} ({}): {error}; resolve any local cache error above; if the host entry is missing or mismatched, export this declared pin on the host with td-feed export sources (warm missing host bytes first; {} SHA-256 {}); upstream fallback is disabled",
                    pin.key, pin.file, pin.url, pin.sha256,
                );
                eprintln!(">> td-feed consume sources: {failure}");
                failures.push(failure);
            }
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "{} source archive(s) were not acquired:\n{}",
            failures.len(),
            failures.join("\n")
        ))
    }
}

fn vm_feed_base(path: &Path, owner: u32) -> Result<String, String> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    // Refuse a final symlink and avoid blocking on a substituted special file.
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(OPEN_REGULAR_NOFOLLOW)
        .open(path)
        .map_err(|e| format!("requires TD_FEED_BASE or a provisioned VM feed endpoint: {e}"))?;
    let meta = file.metadata().map_err(|e| e.to_string())?;
    if !meta.is_file() || meta.uid() != owner || meta.mode() & 0o022 != 0 {
        return Err("VM feed endpoint is not a compositor-owned regular file".into());
    }
    let mut base = String::new();
    file.take(129)
        .read_to_string(&mut base)
        .map_err(|e| e.to_string())?;
    if base.len() > 128 {
        return Err("VM feed endpoint exceeds limit".into());
    }
    let port = base
        .strip_prefix("http://10.0.2.2:")
        .and_then(|text| {
            text.parse::<u16>()
                .ok()
                .filter(|port| *port != 0 && port.to_string() == text)
        })
        .ok_or("VM feed endpoint must be canonical http://10.0.2.2:PORT")?;
    Ok(format!("http://10.0.2.2:{port}"))
}

fn configured_consumer_feed_base() -> Result<String, String> {
    let base = match std::env::var("TD_FEED_BASE") {
        Ok(base) => base,
        Err(std::env::VarError::NotPresent) => vm_feed_base(
            &Path::new(td_engine::permissions::TD_COMPOSITOR_RUNTIME_PATH).join("vm-feed"),
            td_engine::permissions::TD_COMPOSITOR_UID,
        )?,
        Err(_) => return Err("TD_FEED_BASE is not UTF-8".into()),
    };
    consumer_feed_base(&base).map(str::to_owned)
}

fn consume_sources(root: &Path) -> Result<(), String> {
    let base = configured_consumer_feed_base()?;
    let pins = recipe_source_pins_result(root)?;
    consume_source_pins(&pins, &sources_dir(), &base)?;
    warm_kernel_headers_from_pins("i386", &pins);
    warm_kernel_headers_from_pins("x86_64", &pins);
    Ok(())
}

/// warm sources — fetch the recipe-owned pinned source-bootstrap tarballs into the shared
/// sources cache (`sources_dir()`, `$HOME/.td/sources`) for the offline heavy `bootstrap-*`
/// gates, then produce the i386 and x86_64 Linux UAPI headers. Routes through the SHARED feed
/// for cross-worktree
/// single egress: it uses an exported TD_FEED_BASE, else brings the shared daemon up itself
/// (`shared_feed`), so egress does not depend on the caller's env; a direct GET is only the
/// last-resort fallback. td OWNS the fetch (no guix-as-fetcher); each tarball is verified
/// against its recipe sha256.
fn warm_sources(root: &Path) -> Result<(), String> {
    let pins = recipe_source_pins_result(root)?;
    let dest = sources_dir();
    if pins.is_empty() {
        return Err("td-recipe-eval source-pins returned no pins".into());
    }
    std::fs::create_dir_all(&dest).map_err(|e| format!("create {}: {e}", dest.display()))?;

    let feed = shared_feed();
    if let Some((addr, store)) = &feed {
        eprintln!(
            ">> td-feed warm sources: using the shared feed at http://{addr} (store {})",
            store.display()
        );
    }

    for pin in &pins {
        let out = dest.join(&pin.file);
        if let Ok(have) = file_sha256(&out) {
            if have == pin.sha256 {
                continue; // already warm + verified
            }
        }

        let mut via: Option<String> = None;
        // Preferred: through the SHARED feed. Populate it (warm_one egresses only if the
        // shared store is cold — another worktree may already hold it), then GET it back from
        // the feed (offline once warm). So the egress happens ONCE across all worktrees.
        if let Some((addr, store)) = &feed {
            let path = strip_scheme(&pin.url);
            let e = Entry {
                path: path.clone(),
                url: pin.url.clone(),
                sha256: pin.sha256.clone(),
            };
            let _ = warm_one(&e, store);
            if download_verified(&format!("http://{addr}/{path}"), &out, &pin.sha256).is_ok() {
                via = Some(format!("the shared feed (http://{addr})"));
            }
        }
        // Fallback: a direct GET (feed unavailable, or a cold-feed miss).
        if via.is_none() && download_verified(&pin.url, &out, &pin.sha256).is_ok() {
            via = Some("a direct fetch".to_string());
        }
        match via {
            Some(v) => eprintln!(">> td-feed warm sources: warmed {} via {v} (sha256 verified)", out.display()),
            None => eprintln!(
                ">> td-feed warm sources: could not warm {} ({}) (feed + direct both failed) — skipping (the bootstrap gate will report if it runs)",
                pin.file,
                pin.key
            ),
        }
    }

    // Derived inputs: the sanitized Linux UAPI headers for the glibc rungs, produced FROM the
    // pinned linux source (the sandbox can't run the kernel build). Both lanes, best-effort.
    warm_kernel_headers_from_pins("i386", &pins);
    warm_kernel_headers_from_pins("x86_64", &pins);
    Ok(())
}

/// `LINUX_VERSION_CODE` for a `maj.min.sub` version (e.g. 4.14.67 -> 265795).
fn linux_version_code(ver: &str) -> u64 {
    let mut it = ver.split('.').map(|p| p.parse::<u64>().unwrap_or(0));
    let maj = it.next().unwrap_or(0);
    let min = it.next().unwrap_or(0);
    let sub = it.next().unwrap_or(0);
    maj * 65536 + min * 256 + sub
}

/// The hand-written `linux/version.h` body (`headers_install` does NOT emit it, but glibc's
/// configure checks LINUX_VERSION_CODE >= 2.0.10, else "kernel header files TOO OLD!").
fn version_h(code: u64) -> String {
    format!("#define LINUX_VERSION_CODE {code}\n#define KERNEL_VERSION(a,b,c) (((a) << 16) + ((b) << 8) + (c))\n")
}

/// `linux-<ver>.tar.<ext>` -> `<ver>`.
fn linux_ver_from_file(file: &str) -> Option<String> {
    let s = file.strip_prefix("linux-")?;
    let i = s.find(".tar.")?;
    Some(s[..i].to_string())
}

/// `xz -dc src | tar -xf - -C dest --strip-components=1` (don't rely on tar's xz support).
fn extract_xz_tar(src: &Path, dest: &Path) -> bool {
    let mut xz = match Command::new("xz")
        .arg("-dc")
        .arg(src)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return false,
    };
    let Some(xzout) = xz.stdout.take() else {
        return false;
    };
    let tar_ok = Command::new("tar")
        .arg("-xf")
        .arg("-")
        .arg("-C")
        .arg(dest)
        .arg("--strip-components=1")
        // Scrub the ambient TAR_OPTIONS: GNU tar prepends it, and e.g. `TAR_OPTIONS=-z`
        // would make this xz-piped `-xf` try to gunzip an already-decompressed stream and
        // fail, aborting the whole host-free warm on such a host. Removing it keeps the
        // extraction (and therefore the warm) host-env-independent, like the pack step.
        .env_remove("TAR_OPTIONS")
        .stdin(Stdio::from(xzout))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    let xz_ok = xz.wait().map(|s| s.success()).unwrap_or(false);
    tar_ok && xz_ok
}

/// True for a Kbuild byproduct — an install marker (`.install`) or a command
/// record (`*.cmd`, e.g. `..install.cmd`). Never true for a real UAPI header (no
/// header is named `.install` or ends in `.cmd`).
fn is_kbuild_byproduct(p: &Path) -> bool {
    match p.file_name().and_then(|n| n.to_str()) {
        Some(name) => name == ".install" || name.ends_with(".cmd"),
        None => false,
    }
}

/// Recursively delete the Kbuild byproducts `make headers_install` scatters
/// through the header tree (per-dir `.install` markers and `..install.cmd`
/// command records). They are NOT UAPI headers: each `..install.cmd` embeds the
/// absolute build path — a PID-bearing temp dir AND the host `sh` store path — so
/// leaving them in the packed seed makes its digest non-reproducible both
/// run-to-run (the PID) and across hosts (the store path), defeating the entire
/// point of a host-free seed. Only the sanitized `*.h` headers (plus the
/// generated version.h) are real, host-free content. Fails closed (Err) so an
/// un-removable byproduct aborts the warm rather than blessing an unstable
/// tarball.
fn strip_kbuild_byproducts(dir: &Path) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let p = entry.path();
        if entry.file_type()?.is_dir() {
            strip_kbuild_byproducts(&p)?;
        } else if is_kbuild_byproduct(&p) {
            std::fs::remove_file(&p)?;
        }
    }
    Ok(())
}

/// Force fixed permission bits across the header tree — directories 0755, regular
/// files 0644 — so the packed seed is a pure function of the header CONTENT and does
/// NOT depend on the warming host's umask. `make headers_install` creates its dirs
/// (`mkdir -p`) and files (shell redirection), and the generated `version.h` write,
/// all honor the ambient umask: 0022 yields 0755/0644 but 0077 yields 0700/0600, and
/// `tar` records those mode bits — so without this the SAME header bytes would hash to
/// a DIFFERENT seed digest on a host with a different umask (re #469). Every UAPI
/// header is non-executable text, so 0644 (files) / 0755 (dirs) is the canonical set.
/// Symlinks (if any) are skipped: `file_type()` does not follow them and their mode is
/// not meaningful to tar. Fails closed (Err) so an un-chmod-able entry aborts the warm
/// rather than blessing a umask-dependent tarball.
fn normalize_header_modes(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755))?;
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let p = entry.path();
        let ft = entry.file_type()?;
        if ft.is_dir() {
            normalize_header_modes(&p)?;
        } else if ft.is_file() {
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644))?;
        }
    }
    Ok(())
}

/// Reproducible, UNCOMPRESSED tar flags for the generated kernel-headers seed.
/// The committed digest in seed/seed-digests.txt binds the packed bytes, so they
/// must be a host-free function of the header content: `--sort=name` +
/// `--mtime=@0` + zeroed numeric owner + GNU format fix the layout, and `-cf`
/// (NOT `-czf`) keeps out the gzip layer, whose DEFLATE stream varies by zlib
/// version and would re-key the digest per host (re #469).
const KERNEL_HEADERS_TAR_FLAGS: &[&str] = &[
    "--format=gnu",
    "--sort=name",
    "--mtime=@0",
    "--owner=0",
    "--group=0",
    "--numeric-owner",
    "-cf",
];

/// Build the `tar` command that packs the normalized, uncompressed kernel-headers
/// seed (`KERNEL_HEADERS_TAR_FLAGS` into `out`, from the tree at `include_dir`).
/// `TAR_OPTIONS` is scrubbed from the child environment: GNU tar reads that variable
/// and PREPENDS its contents to every invocation, so an ambient `TAR_OPTIONS=-z`
/// (silently re-adds gzip) or `--blocking-factor=N` (changes archive padding) on the
/// warming host would re-key the seed digest despite the fixed flags above (re #469).
/// `LC_ALL=C` pins byte-order sorting/formatting so a host locale cannot reorder or
/// reformat the archive either.
fn kernel_headers_pack_command(out: &Path, include_dir: &Path) -> Command {
    let mut c = Command::new("tar");
    c.args(KERNEL_HEADERS_TAR_FLAGS)
        .arg(out)
        .arg("-C")
        .arg(include_dir)
        .arg(".")
        .env_remove("TAR_OPTIONS")
        .env("LC_ALL", "C");
    c
}

/// warm kernel-headers ARCH — produce the sanitized Linux UAPI headers for `ARCH` (i386 /
/// x86_64) FROM the pinned linux source via `make headers_install`, into the shared sources
/// cache as `linux-headers-<ver>-<ARCH>.tar` (+ a hand-written version.h). guix ships a
/// prebuilt header BLOB; td produces the same headers FROM canonical source.
fn warm_kernel_headers(root: &Path, arch: &str) {
    match recipe_source_pins_result(root) {
        Ok(pins) => warm_kernel_headers_from_pins(arch, &pins),
        Err(e) => eprintln!(
            ">> td-feed warm kernel-headers ({arch}): cannot read recipe source pins: {e}"
        ),
    }
}

fn warm_kernel_headers_from_pins(arch: &str, pins: &[SourcePin]) {
    static NEXT_KERNEL_HEADERS: AtomicU64 = AtomicU64::new(0);
    let Some(pin) = pins.iter().find(|pin| pin.key == "linux-source") else {
        return;
    };
    let file = pin.file.as_str();
    let Some(ver) = linux_ver_from_file(file) else {
        eprintln!(
            ">> td-feed warm kernel-headers ({arch}): cannot parse version from {file} — skipping"
        );
        return;
    };
    let cache = sources_dir();
    let src = cache.join(file);
    let out = cache.join(format!("linux-headers-{ver}-{arch}.tar"));
    if let Err(e) = std::fs::create_dir_all(&cache) {
        eprintln!(
            ">> td-feed warm kernel-headers ({arch}): cannot create {} ({e}) — skipping",
            cache.display()
        );
        return;
    }
    if let Err(e) = require_disk_backed(&cache) {
        eprintln!(
            ">> td-feed warm kernel-headers ({arch}): shared scratch is not disk-backed ({e}) — skipping"
        );
        return;
    }
    // One shared lock covers output publication and crash recovery for both
    // architectures. A killed hosted check releases it; the next warm then
    // removes every named partial before extracting another large Linux tree.
    let lock_path = cache.join(".td-feed-kernel-headers.lock");
    let lock = match OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .mode(0o600)
        .open(&lock_path)
    {
        Ok(lock) => lock,
        Err(e) => {
            eprintln!(
                ">> td-feed warm kernel-headers ({arch}): cannot open {} ({e}) — skipping",
                lock_path.display()
            );
            return;
        }
    };
    if let Err(e) = lock.lock() {
        eprintln!(
            ">> td-feed warm kernel-headers ({arch}): cannot lock {} ({e}) — skipping",
            lock_path.display()
        );
        return;
    }
    sweep_kernel_header_temps(&cache);
    if out.exists() {
        return;
    }
    if !src.is_file() {
        eprintln!(">> td-feed warm kernel-headers ({arch}): linux source not warm ({}) — skipping (PREP best-effort)", src.display());
        return;
    }
    if !(have_cmd("make") && have_cmd("gcc") && have_cmd("xz")) {
        eprintln!(">> td-feed warm kernel-headers ({arch}): need host make+gcc+xz to produce headers — skipping (best-effort)");
        return;
    }
    let nonce = NEXT_KERNEL_HEADERS.fetch_add(1, Ordering::Relaxed);
    let work = cache.join(format!(
        ".td-feed-kh-work-{arch}-{}-{nonce}",
        std::process::id()
    ));
    if std::fs::create_dir(&work).is_err() {
        return;
    }
    let cleanup = RemoveDirOnDrop(work.clone());
    if !extract_xz_tar(&src, &work) {
        eprintln!(">> td-feed warm kernel-headers ({arch}): could not extract {file} — skipping");
        return;
    }
    let hdr = work.join("hdr");
    if !run_quiet(
        Command::new("make")
            .current_dir(&work)
            .arg(format!("ARCH={arch}"))
            .arg(format!("INSTALL_HDR_PATH={}", hdr.display()))
            .arg("headers_install"),
    ) {
        eprintln!(">> td-feed warm kernel-headers ({arch}): headers_install failed — skipping");
        return;
    }
    let code = linux_version_code(&ver);
    let vdir = hdr.join("include/linux");
    let _ = std::fs::create_dir_all(&vdir);
    if std::fs::write(vdir.join("version.h"), version_h(code)).is_err() {
        return;
    }
    // Drop Kbuild byproducts before packing — they embed the PID-bearing build
    // path and the host `sh` store path, which would re-key the seed digest on
    // every run and every host (re #469). Only the sanitized headers survive.
    if let Err(e) = strip_kbuild_byproducts(&hdr.join("include")) {
        eprintln!(">> td-feed warm kernel-headers ({arch}): could not strip Kbuild byproducts ({e}) — skipping");
        return;
    }
    // Force fixed 0755/0644 modes so the seed digest is umask-independent — the tar
    // flags below zero mtime/owner but NOT the permission bits, which headers_install
    // inherits from the ambient umask (re #469).
    if let Err(e) = normalize_header_modes(&hdr.join("include")) {
        eprintln!(">> td-feed warm kernel-headers ({arch}): could not normalize header modes ({e}) — skipping");
        return;
    }
    // PID-unique temp: the shared cache may be warmed by concurrent worktrees, and a fixed
    // `.tmp` would let two of them write the same inode before the atomic rename below.
    let tmp = cache.join(format!(
        ".td-feed-kh-output-{ver}-{arch}-{}-{nonce}.tmp",
        std::process::id()
    ));
    let tmp_cleanup = RemoveFileOnDrop {
        path: tmp.clone(),
        _directory_lock: lock,
        _reservation: match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
        {
            Ok(file) => file,
            Err(e) => {
                eprintln!(
                    ">> td-feed warm kernel-headers ({arch}): cannot reserve {} ({e}) — skipping",
                    tmp.display()
                );
                return;
            }
        },
    };
    // NORMALIZED, UNCOMPRESSED packing (KERNEL_HEADERS_TAR_FLAGS): sorted names,
    // zeroed mtimes, no ownership, GNU format — plus the fixed 0755/0644 modes forced
    // above (tar records mode bits but these flags do not normalize them). Together the
    // tar bytes are a pure function of the header CONTENT, so the same pinned linux
    // source yields the same tarball on any host and under any umask, and the runner's
    // compiled expected digest (seed/seed-digests.txt) can vouch for it (re #469). NO gzip: a `-z`
    // DEFLATE stream is NOT reproducible across zlib/gzip versions, so a
    // compressed tarball hashed to a DIFFERENT seed digest on every host whose
    // gzip differed from the one that blessed the table — reddening the
    // seed-provenance gate off that host (any dev box) even
    // though the header content was identical. The uncompressed tar has no such
    // layer; the seed universe unpacks it via magic-byte sniffing (builder
    // tar::unpack_archive falls through to plain-tar for a non-gzip/xz/bzip2
    // magic), so no consumer changes. Unnormalized host-tar output embeds
    // build-time mtimes and never reproduces either. kernel_headers_pack_command also
    // scrubs the ambient TAR_OPTIONS (and pins LC_ALL=C) so host env cannot re-key it.
    let ok = run_quiet(&mut kernel_headers_pack_command(&tmp, &hdr.join("include")));
    if ok && std::fs::rename(&tmp, &out).is_ok() {
        eprintln!(">> td-feed warm kernel-headers ({arch}): produced {} (LINUX_VERSION_CODE={code}) from the pinned {file}", out.display());
    } else {
        eprintln!(">> td-feed warm kernel-headers ({arch}): could not pack the headers tarball — skipping");
    }
    drop(tmp_cleanup);
    drop(cleanup);
}

/// Hermetic OFFLINE selftest of the warm orchestration's pure + in-process legs (the parts
/// that need NO cargo/make/network). The cargo/make/network legs stay best-effort host PREP,
/// proven by the consuming heavy gates (as the shell scripts were). std::net loopback only.
fn warm_selftest() {
    // 1) parse_source_pins: well-formed recipe TSV parses; malformed input reds.
    let pins = parse_source_pins("x-source\thttps://ftp.gnu.org/x.tar.xz\tdeadbeef\tx.tar.xz\n")
        .unwrap_or_else(|e| die(format!("warm-selftest: source pin TSV rejected: {e}")));
    if pins.len() != 1
        || pins[0].url != "https://ftp.gnu.org/x.tar.xz"
        || pins[0].sha256 != "deadbeef"
        || pins[0].file != "x.tar.xz"
    {
        die("warm-selftest: parse_source_pins did not parse a well-formed line".into());
    }
    if parse_source_pins("garbage\nno fields here\n").is_ok() {
        die("warm-selftest: parse_source_pins accepted malformed TSV".into());
    }
    if parse_source_pins("").is_ok() {
        die("warm-selftest: parse_source_pins accepted an empty source-pin set".into());
    }
    let eval = recipe_eval_path_from_output(Path::new("/repo"), "target/release/td-recipe-eval\n")
        .unwrap_or_else(|e| die(format!("warm-selftest: recipe eval path rejected: {e}")));
    if eval != PathBuf::from("/repo/target/release/td-recipe-eval") {
        die("warm-selftest: relative recipe eval path was not rooted".into());
    }
    if recipe_eval_path_from_output(Path::new("/repo"), "").is_ok() {
        die("warm-selftest: empty recipe eval path accepted".into());
    }
    if recipe_eval_path_from_output(Path::new("/repo"), "a\nb\n").is_ok() {
        die("warm-selftest: ambiguous recipe eval path accepted".into());
    }
    if strip_scheme("https://h/p") != "h/p" || strip_scheme("http://h/p") != "h/p" {
        die("warm-selftest: strip_scheme wrong".into());
    }

    // 2) linux_version_code + version.h (the glibc "TOO OLD!" guard).
    if linux_version_code("4.14.67") != 265795 {
        die(format!(
            "warm-selftest: linux_version_code(4.14.67)={} != 265795",
            linux_version_code("4.14.67")
        ));
    }
    if linux_ver_from_file("linux-4.14.67.tar.xz").as_deref() != Some("4.14.67") {
        die("warm-selftest: linux_ver_from_file wrong".into());
    }
    if !version_h(265795).contains("#define LINUX_VERSION_CODE 265795") {
        die("warm-selftest: version_h missing LINUX_VERSION_CODE".into());
    }

    // 3) cargo_config: routes crates.io at the proxy via sparse source replacement.
    let cfg = cargo_config("127.0.0.1:4321");
    if !cfg.contains("replace-with = \"td-proxy\"")
        || !cfg.contains("registry = \"sparse+http://127.0.0.1:4321/\"")
    {
        die("warm-selftest: cargo_config does not route crates.io through the proxy".into());
    }

    // 4) The IN-PROCESS cargo-proxy (the crown-jewel replacement for the shell's background
    //    process + log scrape): bind it, then drive a verifying source-crate GET through it
    //    against a mock upstream. A crate whose bytes match its index cksum round-trips; one
    //    whose bytes mismatch is REFUSED (the verifying egress is load-bearing).
    let good: Vec<u8> = (0u16..1500).map(|x| (x % 251) as u8).collect();
    let cksum = hex_sha256(&good);
    let up = TcpListener::bind("127.0.0.1:0").expect("bind mock upstream");
    let uport = up.local_addr().unwrap().port();
    let (gb, ck) = (good.clone(), cksum.clone());
    std::thread::spawn(move || loop {
        match up.accept() {
            Ok((mut c, _)) => {
                let mut buf = [0u8; 1024];
                let n = c.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]);
                let path = req.split_whitespace().nth(1).unwrap_or("");
                // index_path("warmcrate") = "wa/rm/warmcrate"; index_path("badcrate") = "ba/dc/badcrate".
                let body: Vec<u8> = match path {
                    "/wa/rm/warmcrate" => format!(
                        "{{\"name\":\"warmcrate\",\"vers\":\"0.1.0\",\"deps\":[],\"cksum\":\"{ck}\",\"features\":{{}},\"yanked\":false}}\n"
                    ).into_bytes(),
                    "/ba/dc/badcrate" => format!(
                        "{{\"name\":\"badcrate\",\"vers\":\"0.1.0\",\"deps\":[],\"cksum\":\"{}\",\"features\":{{}},\"yanked\":false}}\n",
                        "0".repeat(64)
                    ).into_bytes(),
                    "/crates/warmcrate/warmcrate-0.1.0.crate" => gb.clone(),
                    "/crates/badcrate/badcrate-0.1.0.crate" => b"bytes-that-do-not-match-the-cksum".to_vec(),
                    _ => Vec::new(),
                };
                let _ = respond(&mut c, 200, "OK", &body);
            }
            Err(_) => break,
        }
    });
    let ubase = format!("http://127.0.0.1:{uport}");
    std::env::set_var("TD_INDEX_BASE", &ubase);
    std::env::set_var("TD_CRATES_BASE", &ubase);

    let store = std::env::temp_dir().join(format!("td-warm-selftest-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&store);
    let addr = start_cargo_proxy(&store).unwrap_or_else(|e| die(format!("warm-selftest: {e}")));
    let got = try_get(&format!("http://{addr}/dl/warmcrate/0.1.0/download")).unwrap_or_else(|e| {
        die(format!(
            "warm-selftest: source-crate GET through the in-process proxy failed: {e}"
        ))
    });
    if got != good || hex_sha256(&got) != cksum {
        die("warm-selftest: the in-process proxy served a source crate that differs from upstream / its index cksum".into());
    }
    if try_get(&format!("http://{addr}/dl/badcrate/0.1.0/download")).is_ok() {
        die("warm-selftest: the in-process proxy SERVED a crate whose bytes mismatch its index cksum — verify-on-fetch is not load-bearing".into());
    }

    // 5) Lock-driven warming fetches the registry package and only counts the
    // Git member. The deliberately unreachable Git URL must never be contacted.
    let lock = store.join("Cargo.lock");
    let git_commit = "0123456789abcdef0123456789abcdef01234567";
    let lock_text = format!(
        "version = 4\n\n\
         [[package]]\nname = \"warmcrate\"\nversion = \"0.1.0\"\n\
         source = \"registry+https://github.com/rust-lang/crates.io-index\"\n\
         checksum = \"{cksum}\"\n\n\
         [[package]]\nname = \"gitdep\"\nversion = \"1.0.0\"\n\
         source = \"git+https://example.invalid/never-contact?rev={git_commit}#{git_commit}\"\n"
    );
    std::fs::write(&lock, lock_text)
        .unwrap_or_else(|e| die(format!("warm-selftest: write Cargo.lock: {e}")));
    let (locked, _) = fetch_locked_registry(&lock, &store)
        .unwrap_or_else(|e| die(format!("warm-selftest: lock-driven registry fetch: {e}")));
    if locked.registry.len() != 1 || locked.git_packages != 1 {
        die("warm-selftest: lock-driven fetch did not separate registry and Git packages".into());
    }
    let _ = std::fs::remove_dir_all(&store);

    println!(
        "td-feed: warm selftest OK — parse_source_pins (+malformed reject), linux_version_code/version.h \
         (the glibc TOO-OLD guard), cargo_config (sparse source replacement), and the IN-PROCESS cargo-proxy \
         round-trip a verifying source-crate GET over loopback (mock upstream 127.0.0.1:{uport}); a crate whose \
         bytes mismatch its index cksum is refused; lock-driven warming fetched its registry crate while never \
         contacting the counted Git source (the verifying egress and no-Git boundary are load-bearing)"
    );
}

fn warm_usage() -> ! {
    eprintln!(
        "usage:\n  td-feed warm index INDEX STORE        (also: td-feed warm INDEX STORE)\n  \
         td-feed warm crate CRATE VERSION [DEST]\n  td-feed warm crate-local SRCDIR DEST\n  \
         td-feed warm crate-source FILE SHA256 LOCK DEST\n  \
         td-feed warm sources\n  td-feed warm kernel-headers ARCH\n  \
         td-feed warm ostree REPOSITORY REF COMMIT CONTENT DEST"
    );
    std::process::exit(2);
}

fn warm_ostree(a: &[String]) -> Result<(), String> {
    let repository = a
        .get(3)
        .ok_or_else(|| "warm ostree has no repository".to_string())?;
    let exact_ref = a
        .get(4)
        .ok_or_else(|| "warm ostree has no exact ref".to_string())?;
    let commit = a
        .get(5)
        .ok_or_else(|| "warm ostree has no commit".to_string())?;
    let content = a
        .get(6)
        .ok_or_else(|| "warm ostree has no content checksum".to_string())?;
    let destination = PathBuf::from(
        a.get(7)
            .ok_or_else(|| "warm ostree has no destination".to_string())?,
    );
    let parent = crate::ostree::destination_parent(&destination)?;
    std::fs::create_dir_all(parent)
        .map_err(|error| format!("mkdir {}: {error}", parent.display()))?;
    require_disk_backed(parent).map_err(|error| error.to_string())?;
    let spec = crate::ostree::AcquireSpec::parse(repository, exact_ref, commit, content)?;
    let (stats, fetched) = crate::ostree::acquire(&spec, &destination)?;
    println!(
        "td-feed warm ostree: {} {} at {} — {} objects, {} paths ({} directories, {} regular, {} symlinks), {} decoded bytes, {} transfer bytes -> {}",
        if fetched { "fetched" } else { "reused" },
        spec.commit_hex(),
        exact_ref,
        stats.objects,
        stats.paths,
        stats.directories,
        stats.regular_files,
        stats.symlinks,
        stats.decoded_bytes,
        stats.transfer_bytes,
        destination.display()
    );
    Ok(())
}

/// Extract `HOST:PORT` from a `serve` announcement line (`… on http://HOST:PORT/ …`).
/// The one line-format coupling between `serve` (the daemon) and `ensure-serve`
/// (its launcher); unit-tested so a wording drift on one side is caught.
fn parse_serve_addr(line: &str) -> Option<String> {
    let after = line.split("on http://").nth(1)?;
    let addr = after.split('/').next()?;
    // A bound loopback address is `host:port`, both non-empty.
    let (host, port) = addr.rsplit_once(':')?;
    if host.is_empty() || port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(addr.to_string())
}

/// Is `pid` a live process? (`/proc/<pid>` exists — the shell's `kill -0`.)
fn pid_alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

/// Is something actually listening at `addr` (host:port)? A recorded feed.addr is
/// only reusable if its daemon is reachable — `pid_alive` alone would trust a
/// recycled pid whose recorded port is now dead, silently dropping single-egress
/// back to a per-worktree direct fetch. A dead loopback port returns ECONNREFUSED
/// at once regardless of the bound, so the generous 1s timeout never slows the
/// spawn-a-fresh-daemon path; it only guards the live-but-slow case: on a
/// scheduler-starved host a too-short bound would false-negative a healthy daemon
/// and orphan it behind a second one. 1s is far above any real loopback handshake.
fn feed_addr_reachable(addr: &str) -> bool {
    use std::net::ToSocketAddrs;
    let Ok(mut socks) = addr.to_socket_addrs() else {
        return false;
    };
    socks.any(|sa| TcpStream::connect_timeout(&sa, std::time::Duration::from_secs(1)).is_ok())
}

/// `td-feed ensure-serve` — ensure ONE shared, persistent td-feed `serve` daemon
/// is running for this host, and print its loopback address (HOST:PORT) on
/// stdout (native since #318 axis 2 — was tools/feed-ensure.sh). Idempotent +
/// concurrency-safe (an exclusive lock on feed.lock): the FIRST caller starts
/// the daemon; every later caller (any worktree, any agent) reuses it. This is
/// how a bunch of agents on different worktrees SHARE one feed + its store.
///
/// The shared state lives under TD_FEED_DIR (default ~/.td/feed): store/ (the
/// artifacts + .sha256 sidecars), feed.addr, feed.pid, feed.log, feed.lock. The
/// daemon is index-free (serve verifies each file against its sidecar), so it
/// serves whatever any worktree has `td-feed warm`ed into the shared store — no
/// restart when the warmed set grows.
///
/// The daemon binary is TD_FEED_BIN if set, else THIS executable (the natural
/// port: ensure-serve IS a td-feed subcommand, so it launches its own `serve`).
///
/// The daemon lifecycle is factored into `ensure_serve_daemon`, which returns the
/// address instead of exiting so `warm sources` can bring the shared feed up itself
/// when TD_FEED_BASE is not exported — keeping single-egress self-sufficient.
fn ensure_serve() -> ! {
    if std::env::var_os(FEED_NO_DAEMON_ENV).is_some() {
        die(format!(
            "ensure-serve: implicit persistent feed disabled by {FEED_NO_DAEMON_ENV} for this hosted check"
        ));
    }
    match ensure_serve_daemon(&feed_dir()) {
        Ok(addr) => {
            println!("{addr}");
            std::process::exit(0);
        }
        Err(e) => die(format!("ensure-serve: {e}")),
    }
}

/// Ensure ONE shared, persistent `serve` daemon is running for `feed_dir`, returning
/// its loopback address (HOST:PORT). Idempotent + concurrency-safe (an exclusive lock
/// on feed.lock): the FIRST caller starts the daemon; every later caller (any worktree,
/// any agent, `ensure-serve` OR `warm sources`) reuses it. The reuse path touches no
/// daemon binary — only a fresh start resolves TD_FEED_BIN/current_exe — so a live
/// daemon is shared without re-resolving the launcher.
fn ensure_serve_daemon(feed_dir: &Path) -> Result<String, String> {
    let store = feed_dir.join("store");
    let addr_f = feed_dir.join("feed.addr");
    let pid_f = feed_dir.join("feed.pid");
    let log_f = feed_dir.join("feed.log");
    let lock_f = feed_dir.join("feed.lock");
    std::fs::create_dir_all(&store).map_err(|e| format!("mkdir {}: {e}", store.display()))?;

    // Serialize concurrent ensures so two agents never both start a daemon. The
    // lock file is O_CLOEXEC (std default), so the detached daemon does not
    // inherit-and-hold it; it releases when this handle drops.
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false) // a lock handle only; its content is never written
        .open(&lock_f)
        .map_err(|e| format!("open {}: {e}", lock_f.display()))?;
    lock.lock()
        .map_err(|e| format!("lock {}: {e}", lock_f.display()))?;

    // Reuse a live daemon (started by any worktree/agent) — the cross-worktree
    // share. Require BOTH a live pid AND a reachable recorded addr, so a stale
    // feed.pid/feed.addr (crashed daemon, recycled pid) is not trusted; an
    // unreachable record falls through to start a fresh daemon that overwrites it.
    let live_pid = std::fs::read_to_string(&pid_f)
        .ok()
        .and_then(|t| t.trim().parse::<u32>().ok())
        .filter(|p| pid_alive(*p));
    if live_pid.is_some() {
        if let Ok(addr) = std::fs::read_to_string(&addr_f) {
            let addr = addr.trim();
            if !addr.is_empty() && feed_addr_reachable(addr) {
                return Ok(addr.to_string());
            }
        }
    }

    // Start a fresh daemon on an ephemeral loopback port, detached in its own
    // process group so it outlives this launcher and survives ^C/hangup (the
    // shell's `nohup` role). stdout+stderr → the log; we scrape the bound
    // address from serve's announcement line.
    let bin = match std::env::var("TD_FEED_BIN") {
        Ok(v) if !v.trim().is_empty() && Path::new(&v).is_file() => PathBuf::from(v),
        _ => std::env::current_exe().map_err(|e| format!("cannot resolve current_exe: {e}"))?,
    };
    // Route the daemon self-spawn through the umbrella `feed` selector ONLY when the resolved
    // binary IS the td-net multicall: current_exe() resolves /proc/self/exe to the real file
    // (basename `td-net`), NOT the td-feed applet link, so the umbrella needs the selector. A
    // `td-feed`-named link already dispatches by basename; and any OTHER name — an explicit
    // TD_FEED_BIN the operator aimed at a standalone (non-multicall) feed executable — must be
    // invoked verbatim (`<bin> serve …`), never handed a spurious `feed` arg it can't parse.
    let is_umbrella = Path::new(&bin).file_name().and_then(|s| s.to_str()) == Some("td-net");
    let log =
        std::fs::File::create(&log_f).map_err(|e| format!("create {}: {e}", log_f.display()))?;
    let log2 = log.try_clone().map_err(|e| format!("clone log fd: {e}"))?;
    let mut cmd = Command::new(&bin);
    if is_umbrella {
        cmd.arg("feed");
    }
    cmd.arg("serve")
        .arg(&store)
        .arg("127.0.0.1:0")
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log2));
    {
        use std::os::unix::process::CommandExt as _;
        cmd.process_group(0);
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("spawn {} serve: {e}", bin.display()))?;
    let pid = child.id();
    let _ = std::fs::write(&pid_f, format!("{pid}\n"));

    // Wait for it to announce its bound address in the log.
    for _ in 0..100 {
        if let Ok(logtext) = std::fs::read_to_string(&log_f) {
            if let Some(addr) = logtext.lines().find_map(parse_serve_addr) {
                let _ = std::fs::write(&addr_f, format!("{addr}\n"));
                return Ok(addr);
            }
        }
        if child.try_wait().ok().flatten().is_some() {
            let tail = std::fs::read_to_string(&log_f).unwrap_or_default();
            return Err(format!("daemon exited before binding:\n{tail}"));
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let tail = std::fs::read_to_string(&log_f).unwrap_or_default();
    Err(format!("daemon did not report an address:\n{tail}"))
}

pub fn run(a: &[String]) {
    match a.get(1).map(String::as_str) {
        Some("export") if a.len() == 3 && a.get(2).map(String::as_str) == Some("sources") => {
            let result = recipe_source_pins_result(&repo_root()).and_then(|pins| {
                export_source_pins(&pins, &sources_dir(), &feed_dir().join("store"))
            });
            if let Err(error) = result {
                die(format!("export sources: {error}"));
            }
        }
        Some("consume") if a.len() == 3 && a.get(2).map(String::as_str) == Some("sources") => {
            if let Err(error) = consume_sources(&repo_root()) {
                die(format!("consume sources: {error}"));
            }
        }
        Some(action @ ("export" | "consume"))
            if a.len() == 5 && a.get(2).map(String::as_str) == Some("cargo") =>
        {
            let result = (|| {
                let lock = Path::new(a.get(3).ok_or("missing Cargo.lock path")?);
                let archives = Path::new(a.get(4).ok_or("missing archive directory")?);
                let base = if action == "consume" {
                    Some(configured_consumer_feed_base()?)
                } else {
                    None
                };
                transfer_locked_registry(lock, archives, &feed_dir().join("store"), base.as_deref())
            })();
            if let Err(error) = result {
                die(format!("{action} cargo: {error}"));
            }
        }
        Some(action @ ("export" | "consume"))
            if matches!(a.len(), 3 | 4) && a.get(2).map(String::as_str) == Some("vendors") =>
        {
            if let Err(error) =
                vendor::run(&repo_root(), a.get(3).map(String::as_str), action == "consume")
            {
                die(format!("{action} vendors: {error}"));
            }
        }
        Some(action @ ("export" | "consume"))
            if a.len() == 3 && a.get(2).map(String::as_str) == Some("graphs") =>
        {
            if let Err(error) = graphs::run(&repo_root(), action == "consume") {
                die(format!("{action} graphs: {error}"));
            }
        }
        // warm <action> — the structured host-PREP orchestration (consolidated warm-*.sh).
        // The low-level `warm INDEX STORE` primitive (feed-shared gate, feed-ensure serve)
        // stays: dispatch on a known action keyword, else treat it as the legacy 2-arg form.
        Some("warm") => {
            // The legacy primitive: warm an `<path> <url> <sha256>` index into a store.
            let warm_index = |index: &str, store: &str| {
                let entries = read_index(index);
                let store = PathBuf::from(store);
                match warm(&entries, &store) {
                    Ok((fetched, w)) => println!(
                        "td-feed: warm OK — {fetched} fetched, {w} already warm, {} total -> {}",
                        entries.len(),
                        store.display()
                    ),
                    Err(e) => die(e),
                }
            };
            let root = repo_root();
            match a.get(2).map(String::as_str) {
                Some("index") if a.len() == 5 => warm_index(&a[3], &a[4]),
                Some("crate") if a.len() == 5 => warm_crate(&root, &a[3], &a[4], &a[3]),
                Some("crate") if a.len() == 6 => warm_crate(&root, &a[3], &a[4], &a[5]),
                Some("crate-local") if a.len() == 5 => warm_crate_local(&root, &a[3], &a[4]),
                Some("crate-source") if a.len() == 7 => {
                    warm_crate_source(&root, &a[3], &a[4], &a[5], &a[6])
                }
                Some("sources") if a.len() == 3 => {
                    if let Err(e) = warm_sources(&root) {
                        die(format!("warm sources: {e}"));
                    }
                }
                Some("kernel-headers") if a.len() == 4 => warm_kernel_headers(&root, &a[3]),
                Some("ostree") if a.len() == 8 => {
                    if let Err(error) = warm_ostree(a) {
                        die(format!("warm ostree: {error}"));
                    }
                }
                // Legacy: `warm INDEX STORE` (a[2] is an index path, not an action keyword).
                // Exclude every action keyword so a mis-argc'd action (e.g. `warm crate X`)
                // reports usage instead of being misread as an index path.
                Some(kw)
                    if a.len() == 4
                        && !matches!(
                            kw,
                            "index"
                                | "crate"
                                | "crate-local"
                                | "crate-source"
                                | "sources"
                                | "kernel-headers"
                                | "ostree"
                        ) =>
                {
                    warm_index(&a[2], &a[3])
                }
                _ => warm_usage(),
            }
        }
        Some("warm-selftest") if a.len() == 2 => warm_selftest(),
        Some("serve") if a.len() == 4 => {
            let (store, addr) = (PathBuf::from(&a[2]), &a[3]);
            let listener = TcpListener::bind(addr.as_str())
                .unwrap_or_else(|e| die(format!("bind {addr}: {e}")));
            let bound = listener
                .local_addr()
                .unwrap_or_else(|e| die(format!("local_addr: {e}")));
            println!("td-feed: serving {} on http://{}/", store.display(), bound);
            let _ = io::stdout().flush();
            serve_loop(listener, Arc::new(store));
        }
        Some("cargo-proxy") if a.len() == 4 => {
            let (store, addr) = (PathBuf::from(&a[2]), &a[3]);
            let listener = TcpListener::bind(addr.as_str())
                .unwrap_or_else(|e| die(format!("bind {addr}: {e}")));
            let bound = listener
                .local_addr()
                .unwrap_or_else(|e| die(format!("local_addr: {e}")));
            println!(
                "td-feed: cargo-proxy on http://{bound}/ (store {})",
                store.display()
            );
            let _ = io::stdout().flush();
            cargo_proxy_loop(listener, Arc::new(store), bound.to_string());
        }
        Some("ensure-serve") if a.len() == 2 => ensure_serve(),
        Some("selftest") if a.len() == 2 => selftest(),
        Some("cargo-proxy-selftest") if a.len() == 2 => cargo_proxy_selftest(),
        _ => {
            eprintln!(
                "usage:\n  td-feed warm INDEX STORE   (low-level; also: warm index INDEX STORE)\n  \
                 td-feed warm crate CRATE VERSION [DEST]\n  td-feed warm crate-local SRCDIR DEST\n  \
                 td-feed warm crate-source FILE SHA256 LOCK DEST\n  \
                 td-feed warm sources\n  td-feed warm kernel-headers ARCH\n  \
                 td-feed warm ostree REPOSITORY REF COMMIT CONTENT DEST\n  \
                 td-feed serve STORE ADDR\n  \
                 td-feed export sources  (publish existing host archives; no downloads)\n  td-feed consume sources  (TD_FEED_BASE or VM endpoint; no upstream fallback)\n  td-feed export cargo LOCK ARCHIVES\n  td-feed consume cargo LOCK ARCHIVES\n  td-feed export vendors [TARGET]\n  td-feed consume vendors [TARGET]\n  td-feed export graphs\n  td-feed consume graphs\n  td-feed ensure-serve\n  td-feed cargo-proxy STORE ADDR\n  td-feed selftest\n  \
                 td-feed cargo-proxy-selftest\n  td-feed warm-selftest"
            );
            std::process::exit(2);
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::{
        cargo_route, detach_from_workspace, download_verified, ensure_serve_daemon,
        feed_daemon_policy, file_sha256_before, index_path, is_warm_complete, mark_warm_complete,
        mount_is_memory_backed, parse_locked_cargo_sources, parse_serve_addr, read_digest_sidecar,
        real_relative_file, resolve_feed_dir, snapshot_for_serve, snapshot_for_serve_before,
        sweep_download_temps, sweep_kernel_header_temps, valid_crate_source_coordinates,
        write_before, FeedDaemonPolicy, ResponseGuard, RESPONSE_DEADLINE,
    };
    use std::io::Read;
    use std::path::PathBuf;

    /// The marker answers for the lock it was published from, and only that one.
    ///
    /// A vendor set is complete FOR a lock. Before the marker carried a digest
    /// it recorded a count alone, so bumping a dependency left the old set
    /// marked done: the next warm said "already warm" and skipped, and the
    /// build's set-equality gate then rejected a closure the lock no longer
    /// describes.
    #[test]
    fn a_warm_marker_does_not_survive_the_lock_that_produced_it() {
        let dir = unique_tmp_dir("warm-marker");
        let vendor = dir.join("vendor");
        std::fs::create_dir_all(&vendor).unwrap();
        let lock = dir.join("Cargo.lock");
        std::fs::write(&lock, "[[package]]\nname = \"a\"\n").unwrap();

        assert!(
            !is_warm_complete(&vendor, &lock),
            "no marker at all is not warm"
        );

        let digest = super::lock_digest(&lock).expect("digest");
        mark_warm_complete(&vendor, 3, &digest);
        assert!(is_warm_complete(&vendor, &lock), "its own lock is warm");

        // The bump.
        std::fs::write(&lock, "[[package]]\nname = \"a\"\nversion = \"2\"\n").unwrap();
        assert!(
            !is_warm_complete(&vendor, &lock),
            "a superseded lock must re-warm, not report already warm"
        );

        // An unreadable lock cannot be shown to match, so it is not warm.
        std::fs::remove_file(&lock).unwrap();
        assert!(!is_warm_complete(&vendor, &lock), "no lock is not warm");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A marker written by the pre-digest scheme carries a count where the
    /// digest belongs. It never matches, so it reads as cold exactly once and is
    /// replaced — which is the whole migration.
    #[test]
    fn a_pre_digest_marker_reads_as_cold_and_is_replaced() {
        let dir = unique_tmp_dir("warm-marker-old");
        let vendor = dir.join("vendor");
        std::fs::create_dir_all(&vendor).unwrap();
        let lock = dir.join("Cargo.lock");
        std::fs::write(&lock, "[[package]]\nname = \"a\"\n").unwrap();
        // Exactly what the old mark_warm_complete wrote.
        std::fs::write(vendor.join(".warm-complete"), "1189\n").unwrap();

        assert!(
            !is_warm_complete(&vendor, &lock),
            "an old count-only marker must not read as warm"
        );

        let digest = super::lock_digest(&lock).expect("digest");
        mark_warm_complete(&vendor, 1189, &digest);
        assert!(is_warm_complete(&vendor, &lock), "and is replaced in place");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A per-invocation-unique temp dir under $TMPDIR. `cargo test` runs tests
    /// concurrently in ONE process, so `std::process::id()` alone is shared across
    /// threads; a monotonic counter makes each call's path distinct regardless of
    /// how many tests (present or future) allocate one.
    #[cfg(test)]
    fn unique_tmp_dir(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("td-feed-{tag}-{}-{n}", std::process::id()))
    }

    #[test]
    fn serve_snapshot_is_stable_after_the_store_file_changes() {
        let dir = unique_tmp_dir("serve-snapshot");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("artifact");
        std::fs::write(&path, b"verified bytes").unwrap();
        let (mut snapshot, digest) = snapshot_for_serve(&path).unwrap();
        std::fs::write(&path, b"changed bytes").unwrap();
        let mut got = Vec::new();
        snapshot.read_to_end(&mut got).unwrap();
        assert_eq!(got, b"verified bytes");
        assert_eq!(digest, super::hex_sha256(b"verified bytes"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn cargo_lock_warming_separates_registry_from_git_without_transport() {
        let checksum = "a".repeat(64);
        let commit = "0123456789abcdef0123456789abcdef01234567";
        let lock = format!(
            "version = 4\n\n\
             [[package]]\nname = \"root\"\nversion = \"0.1.0\"\n\n\
             [[package]]\nname = \"registry-dep\"\nversion = \"1.2.3\"\n\
             source = \"registry+https://github.com/rust-lang/crates.io-index\"\n\
             checksum = \"{checksum}\"\n\n\
             [[package]]\nname = \"git-dep\"\nversion = \"2.0.0\"\n\
             source = \"git+https://example.invalid/never?rev={commit}#{commit}\"\n"
        );
        let parsed = parse_locked_cargo_sources(&lock).unwrap();
        assert_eq!(parsed.registry.len(), 1);
        assert_eq!(parsed.registry[0].name, "registry-dep");
        assert_eq!(parsed.git_packages, 1);
        assert!(parse_locked_cargo_sources(&lock.replace(&checksum, &"A".repeat(64))).is_err());
        assert!(parse_locked_cargo_sources(
            "version = 4\n\n[[package]]\nname = \"dep\"\nversion = \"1\"\nsource = \"path+file:///tmp/dep\"\n"
        )
        .is_err());
        assert!(parse_locked_cargo_sources(
            "version = 4\n\n[[package]]\nname = \"dep\"\nversion = \"1\"\nsource = false\n"
        )
        .is_err());
    }

    #[test]
    fn serve_snapshot_obeys_response_deadline_before_copying() {
        let dir = unique_tmp_dir("serve-snapshot-deadline");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("artifact");
        std::fs::write(&path, b"verified bytes").unwrap();
        let expired = std::time::Instant::now()
            .checked_sub(std::time::Duration::from_secs(1))
            .unwrap();
        let guard = ResponseGuard::without_client(expired);

        let error = snapshot_for_serve_before(&path, &guard).unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn serve_snapshot_stops_when_client_disconnects_before_response_ready() {
        let dir = unique_tmp_dir("serve-snapshot-disconnect");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("artifact");
        let bytes: Vec<u8> = (0u32..(128 * 1024)).map(|n| (n % 251) as u8).collect();
        std::fs::write(&path, &bytes).unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let client = std::net::TcpStream::connect(addr).unwrap();
        let (server, _) = listener.accept().unwrap();
        client.shutdown(std::net::Shutdown::Both).unwrap();
        drop(client);
        let guard = ResponseGuard::client(std::time::Instant::now() + RESPONSE_DEADLINE, &server);

        let error = snapshot_for_serve_before(&path, &guard).unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
        drop(server);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn digest_sidecars_are_fixed_size_and_strictly_formatted() {
        let dir = unique_tmp_dir("digest-sidecar-bound");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("artifact.sha256");
        let digest = "a".repeat(64);
        std::fs::write(&path, format!("{digest}\n")).unwrap();
        assert_eq!(read_digest_sidecar(&path).unwrap(), digest);

        std::fs::write(&path, vec![b'a'; 1024 * 1024]).unwrap();
        assert!(read_digest_sidecar(&path).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn guarded_hashing_obeys_the_response_deadline() {
        let dir = unique_tmp_dir("guarded-hash-deadline");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("artifact");
        std::fs::write(&path, vec![b'x'; 128 * 1024]).unwrap();
        let expired = std::time::Instant::now()
            .checked_sub(std::time::Duration::from_secs(1))
            .unwrap();
        let guard = ResponseGuard::without_client(expired);

        let error = file_sha256_before(&path, 16 * 1024 * 1024, Some(&guard)).unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn artifact_hashing_rejects_oversized_existing_files_before_reading() {
        let dir = unique_tmp_dir("oversized-existing-hash");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("artifact");
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(super::MAX_ARTIFACT_BYTES + 1).unwrap();

        let error = super::file_sha256(&path).unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn feed_scratch_rejects_tmpfs_and_overlay_on_tmpfs() {
        let mounts = "30 1 0:1 / / rw - btrfs /dev/disk rw\n\
                      31 30 0:2 / /dev/shm rw - tmpfs tmpfs rw\n\
                      32 30 0:3 / /overlay rw - overlay overlay rw,lowerdir=/lower,upperdir=/dev/shm/up,workdir=/dev/shm/work\n\
                      33 30 0:4 / /huge rw - hugetlbfs hugetlbfs rw\n\
                      34 30 0:5 / /devices rw - devtmpfs devtmpfs rw\n";
        assert!(!mount_is_memory_backed(std::path::Path::new("/home/u"), mounts, 0).unwrap());
        assert!(mount_is_memory_backed(std::path::Path::new("/dev/shm/cache"), mounts, 0).unwrap());
        assert!(mount_is_memory_backed(std::path::Path::new("/overlay/cache"), mounts, 0).unwrap());
        assert!(mount_is_memory_backed(std::path::Path::new("/huge/cache"), mounts, 0).unwrap());
        assert!(mount_is_memory_backed(std::path::Path::new("/devices/cache"), mounts, 0).unwrap());
    }

    #[test]
    fn abandoned_download_partials_are_swept() {
        let dir = unique_tmp_dir("download-sweep");
        std::fs::create_dir_all(&dir).unwrap();
        let stale = dir.join(".td-feed-download-1-2.tmp");
        let unrelated = dir.join("keep.tmp");
        std::fs::write(&stale, b"partial").unwrap();
        std::fs::write(&unrelated, b"keep").unwrap();
        sweep_download_temps(&dir);
        assert!(!stale.exists());
        assert!(unrelated.exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn abandoned_kernel_header_trees_and_outputs_are_swept() {
        let dir = unique_tmp_dir("kernel-header-sweep");
        let stale_tree = dir.join(".td-feed-kh-work-i386-1-0");
        let stale_output = dir.join(".td-feed-kh-output-1-i386-1-0.tmp");
        let unrelated = dir.join("linux-source.tar.xz");
        std::fs::create_dir_all(&stale_tree).unwrap();
        std::fs::write(stale_tree.join("partial"), b"partial").unwrap();
        std::fs::write(&stale_output, b"partial").unwrap();
        std::fs::write(&unrelated, b"keep").unwrap();

        sweep_kernel_header_temps(&dir);

        assert!(!stale_tree.exists());
        assert!(!stale_output.exists());
        assert!(unrelated.exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn cargo_registry_paths_reject_non_ascii_without_panicking() {
        assert_eq!(index_path("a"), Some("1/a".to_string()));
        assert_eq!(index_path("AB"), Some("2/ab".to_string()));
        assert_eq!(index_path("abc"), Some("3/a/abc".to_string()));
        assert_eq!(
            index_path("serde_json"),
            Some("se/rd/serde_json".to_string())
        );
        assert_eq!(index_path("aéx"), None);

        let dir = unique_tmp_dir("cargo-route-input");
        let error = cargo_route(&dir, "127.0.0.1:1", "/dl/aéx/1/download")
            .err()
            .unwrap();
        assert!(error.starts_with("bad download crate name"), "{error}");
    }

    #[test]
    fn response_writes_obey_an_absolute_deadline() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let client = std::net::TcpStream::connect(addr).unwrap();
        let (mut server, _) = listener.accept().unwrap();
        let expired = std::time::Instant::now()
            .checked_sub(std::time::Duration::from_secs(1))
            .unwrap();

        let error = write_before(&mut server, b"body", expired).unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        drop(client);
    }

    #[test]
    fn a_waiting_download_reuses_the_file_published_ahead_of_it() {
        let dir = unique_tmp_dir("download-dedup");
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("artifact");
        std::fs::write(&out, b"published bytes").unwrap();
        let want = super::hex_sha256(b"published bytes");

        download_verified("http://127.0.0.1:0/must-not-connect", &out, &want).unwrap();

        assert_eq!(std::fs::read(&out).unwrap(), b"published bytes");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn vm_feed_file_checks_the_opened_object_owner_mode_and_bound() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let dir = unique_tmp_dir("vm-feed-file");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("feed");
        let uid = std::fs::metadata(&dir).unwrap().uid();
        assert!(super::vm_feed_base(&path, uid).is_err());
        std::fs::write(&path, b"http://10.0.2.2:1234").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            super::vm_feed_base(&path, uid).unwrap(),
            "http://10.0.2.2:1234"
        );
        assert!(super::vm_feed_base(&path, uid.wrapping_add(1)).is_err());
        let link = dir.join("link");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(super::vm_feed_base(&link, uid).is_err());
        assert!(super::vm_feed_base(&dir, uid).is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666)).unwrap();
        assert!(super::vm_feed_base(&path, uid).is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        for data in [vec![b'a'; 129], b"not a URL".to_vec(), vec![0xff]] {
            std::fs::write(&path, data).unwrap();
            assert!(super::vm_feed_base(&path, uid).is_err());
        }
        for base in [
            "http://other:1234",
            "https://10.0.2.2:1234",
            "http://10.0.2.2:0",
            "http://10.0.2.2:0123",
            "http://10.0.2.2:65536",
            "http://10.0.2.2:1234/",
        ] {
            std::fs::write(&path, base).unwrap();
            assert!(super::vm_feed_base(&path, uid).is_err(), "{base}");
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn consumer_rejects_coordination_names_and_incomplete_download_policy() {
        let dir = unique_tmp_dir("consumer-reserved");
        for name in [".td-feed-download.lock", ".td-feed-download-12-3.tmp"] {
            let pin = super::SourcePin {
                key: "fixture".into(), url: "https://example.invalid/source".into(),
                sha256: super::hex_sha256(b"bytes"), file: name.into(),
            };
            let error = super::consume_source_pins(&[pin], &dir, "http://127.0.0.1:0").unwrap_err();
            assert!(error.contains("unsafe source cache filename"), "{error}");
        }
        let error = super::download_verified_policy(
            "http://127.0.0.1:0",
            &dir.join("source"),
            "",
            None,
            false,
        )
        .unwrap_err();
        assert!(error.contains("requires a transfer deadline"));
        assert!(!dir.exists());
    }

    pub(crate) struct ConsumerServer {
        pub(crate) base: String,
        requests: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl ConsumerServer {
        pub(crate) fn start(store: PathBuf, response: Option<Vec<u8>>) -> Self {
            use std::io::Write;
            use std::sync::{
                atomic::{AtomicBool, AtomicUsize, Ordering},
                Arc,
            };
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let stop = Arc::new(AtomicBool::new(false));
            let requests = Arc::new(AtomicUsize::new(0));
            let stopped = stop.clone();
            let seen = requests.clone();
            let thread = std::thread::spawn(move || {
                while !stopped.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            seen.fetch_add(1, Ordering::Relaxed);
                            if let Some(bytes) = &response {
                                stream
                                    .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                                    .unwrap();
                                let mut scratch = [0; 4096];
                                let _ = stream.read(&mut scratch);
                                let _ = stream.write_all(bytes);
                            } else {
                                let _ = super::handle_conn(stream, &store);
                            }
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(std::time::Duration::from_millis(2));
                        }
                        Err(error) => panic!("listener: {error}"),
                    }
                }
            });
            Self {
                base,
                requests,
                stop,
                thread: Some(thread),
            }
        }
        pub(crate) fn count(&self) -> usize {
            self.requests.load(std::sync::atomic::Ordering::Relaxed)
        }
    }

    impl Drop for ConsumerServer {
        fn drop(&mut self) {
            self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
            if let Some(thread) = self.thread.take() { let _ = thread.join(); }
        }
    }

    #[test]
    fn export_publication_keeps_its_lock_through_cold_and_warm_sidecars() {
        let dir = unique_tmp_dir("export-publish-lock");
        std::fs::create_dir_all(&dir).unwrap();
        let dst = dir.join("source.tar");
        let bytes = b"verified source";
        let digest = super::hex_sha256(bytes);
        for warm in [false, true] {
            let guard = super::ResponseGuard::without_client(
                std::time::Instant::now() + std::time::Duration::from_secs(2),
            );
            super::acquire_verified(
                &dst,
                &digest,
                "fixture bytes",
                Some(&guard),
                |temp| {
                    assert!(!warm, "warm entry unnecessarily copied");
                    std::fs::write(temp, bytes).map_err(|e| e.to_string())
                },
                || {
                    assert_eq!(std::fs::read(&dst).unwrap(), bytes);
                    let competing = std::fs::OpenOptions::new()
                        .write(true)
                        .open(dir.join(".td-feed-download.lock"))
                        .unwrap();
                    assert!(matches!(
                        competing.try_lock(),
                        Err(std::fs::TryLockError::WouldBlock)
                    ));
                    super::write_atomic(
                        &super::sidecar_path(&dst),
                        format!("{digest}\n").as_bytes(),
                    )
                },
            )
            .unwrap();
            assert_eq!(
                super::read_digest_sidecar(&super::sidecar_path(&dst)).unwrap(),
                digest
            );
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn concurrent_exporters_leave_a_matching_object_and_sidecar() {
        let dir = unique_tmp_dir("export-concurrent");
        let store = dir.join("store");
        let barrier = std::sync::Barrier::new(2);
        std::thread::scope(|scope| {
            for (name, bytes) in [
                ("first", b"first content".as_slice()),
                ("second", b"second content".as_slice()),
            ] {
                let sources = dir.join(name);
                std::fs::create_dir_all(&sources).unwrap();
                std::fs::write(sources.join("source.tar"), bytes).unwrap();
                let store = &store;
                let barrier = &barrier;
                scope.spawn(move || {
                    let pin = super::SourcePin {
                        key: name.into(),
                        url: "https://example.invalid/source.tar".into(),
                        file: "source.tar".into(),
                        sha256: super::hex_sha256(bytes),
                    };
                    barrier.wait();
                    for _ in 0..16 {
                        super::export_source_pin(&pin, &sources, store).unwrap();
                    }
                });
            }
        });
        let dst = store.join("example.invalid/source.tar");
        assert_eq!(
            super::file_sha256(&dst).unwrap(),
            super::read_digest_sidecar(&super::sidecar_path(&dst)).unwrap()
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn warming_and_export_share_the_same_publication_lock() {
        let dir = unique_tmp_dir("warm-export-concurrent");
        let source = dir.join("source");
        let store = dir.join("store");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("source.tar"), b"exported").unwrap();
        let origin = ConsumerServer::start(
            dir.join("upstream"),
            Some(
                b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\nConnection: close\r\n\r\nwarmed".to_vec(),
            ),
        );
        let pin = super::SourcePin {
            key: "fixture".into(),
            url: format!("{}/source.tar", origin.base),
            file: "source.tar".into(),
            sha256: super::hex_sha256(b"exported"),
        };
        let entry = super::Entry {
            path: super::strip_scheme(&pin.url),
            url: pin.url.clone(),
            sha256: super::hex_sha256(b"warmed"),
        };
        let barrier = std::sync::Barrier::new(2);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                barrier.wait();
                for _ in 0..16 {
                    super::warm_one(&entry, &store).unwrap();
                }
            });
            scope.spawn(|| {
                barrier.wait();
                for _ in 0..16 {
                    super::export_source_pin(&pin, &source, &store).unwrap();
                }
            });
        });
        let dst = store.join(&entry.path);
        assert_eq!(
            super::file_sha256(&dst).unwrap(),
            super::read_digest_sidecar(&super::sidecar_path(&dst)).unwrap()
        );
        assert!(origin.count() > 0);
        drop(origin);
        std::fs::remove_dir_all(dir).unwrap();
    }

    fn cargo_transfer_lock(
        dir: &std::path::Path,
        bytes: &[u8],
        git_url: &str,
    ) -> std::path::PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let lock = dir.join("Cargo.lock");
        std::fs::write(&lock, format!(
            "version = 4\n[[package]]\nname = \"dep\"\nversion = \"1.0.0\"\nsource = \"registry+https://example.invalid/index\"\nchecksum = \"{}\"\n[[package]]\nname = \"git-dep\"\nversion = \"1.0.0\"\nsource = \"git+{git_url}#123456\"\n",
            super::hex_sha256(bytes),
        )).unwrap();
        lock
    }

    #[test]
    fn cargo_transfer_refuses_a_manifest_but_accepts_a_dependency_free_lock() {
        let dir = unique_tmp_dir("cargo-lock-selection");
        std::fs::create_dir_all(&dir).unwrap();
        let lock = dir.join("Cargo.lock");
        std::fs::write(&lock, "[package]\nname = \"local\"\nversion = \"1.0.0\"\n").unwrap();
        assert!(super::read_locked_cargo_sources(&lock).is_err());
        std::fs::write(
            &lock,
            "version = 4\n[[package]]\nname = \"local\"\nversion = \"1.0.0\"\n",
        )
        .unwrap();
        let (sources, _) = super::read_locked_cargo_sources(&lock).unwrap();
        assert!(sources.registry.is_empty());
        assert_eq!(sources.git_packages, 0);
        super::transfer_locked_registry(&lock, &dir.join("unused"), &dir.join("store"), None)
            .unwrap();
        let symlink = dir.join("link.lock");
        std::os::unix::fs::symlink(&lock, &symlink).unwrap();
        assert!(super::lock_digest(&symlink).is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn exporting_source_archives_refuses_final_symlinks_and_special_files() {
        let dir = unique_tmp_dir("export-special");
        let sources = dir.join("sources");
        let store = dir.join("store");
        std::fs::create_dir_all(&sources).unwrap();
        let bytes = b"archive bytes";
        let pin = super::SourcePin {
            key: "fixture".into(),
            file: "archive.tar".into(),
            url: "https://example.invalid/archive.tar".into(),
            sha256: super::hex_sha256(bytes),
        };
        let archive = sources.join(&pin.file);
        let plain = sources.join("plain.tar");
        std::fs::write(&plain, bytes).unwrap();
        std::os::unix::fs::symlink(&plain, &archive).unwrap();
        assert!(super::export_source_pin(&pin, &sources, &store).is_err());
        std::fs::remove_file(&archive).unwrap();
        let socket = std::os::unix::net::UnixListener::bind(&archive).unwrap();
        assert!(super::export_source_pin(&pin, &sources, &store).is_err());
        drop(socket);
        std::fs::remove_file(&archive).unwrap();
        std::fs::rename(plain, archive).unwrap();
        super::export_source_pin(&pin, &sources, &store).unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn cargo_feed_reuses_locked_bytes_in_two_private_caches_and_survives_restart() {
        use super::transfer_locked_registry as transfer;
        let dir = unique_tmp_dir("cargo-transfer");
        let origin = ConsumerServer::start(dir.join("upstream"), None);
        let bytes = b"locked crate bytes";
        let lock = cargo_transfer_lock(&dir, bytes, &origin.base);
        let archives = dir.join("archives");
        let store = dir.join("store");
        std::fs::create_dir_all(&archives).unwrap();
        std::fs::write(archives.join("dep-1.0.0.crate"), bytes).unwrap();
        std::fs::write(archives.join("unselected-1.0.0.crate"), b"ignore me").unwrap();
        transfer(&lock, &archives, &store, None).unwrap();
        std::fs::remove_dir_all(&archives).unwrap();
        transfer(&lock, &archives, &store, None).unwrap();
        let feed = ConsumerServer::start(store.clone(), None);
        for guest in ["one", "two"] {
            let cache = dir.join(guest);
            transfer(&lock, &cache, &store, Some(&feed.base)).unwrap();
            assert_eq!(std::fs::read(cache.join("dep-1.0.0.crate")).unwrap(), bytes);
            assert!(!cache.join("unselected-1.0.0.crate").exists());
            assert!(!cache.join(".warm-complete").exists());
        }
        assert_eq!(feed.count(), 2);
        std::fs::write(dir.join("one/dep-1.0.0.crate"), b"guest mutation").unwrap();
        assert_eq!(
            std::fs::read(dir.join("two/dep-1.0.0.crate")).unwrap(),
            bytes
        );
        drop(feed);
        transfer(&lock, &dir.join("two"), &store, Some("http://127.0.0.1:1")).unwrap();
        let restarted = ConsumerServer::start(store.clone(), None);
        transfer(&lock, &dir.join("one"), &store, Some(&restarted.base)).unwrap();
        assert_eq!(
            std::fs::read(dir.join("one/dep-1.0.0.crate")).unwrap(),
            bytes
        );
        assert_eq!(origin.count(), 0);
        drop(restarted);
        drop(origin);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn cargo_feed_namespaces_checksums_and_rejects_bad_or_incomplete_transfers() {
        use super::transfer_locked_registry as transfer;
        let dir = unique_tmp_dir("cargo-transfer-refusal");
        let origin = ConsumerServer::start(dir.join("upstream"), None);
        let first = cargo_transfer_lock(&dir.join("first"), b"first", &origin.base);
        let second = cargo_transfer_lock(&dir.join("second"), b"second", &origin.base);
        let store = dir.join("store");
        for (lock, bytes) in [
            (&first, b"first".as_slice()),
            (&second, b"second".as_slice()),
        ] {
            let archives = lock.parent().unwrap();
            std::fs::write(archives.join("dep-1.0.0.crate"), b"corrupt").unwrap();
            assert!(transfer(lock, archives, &store, None).is_err());
            std::fs::write(archives.join("dep-1.0.0.crate"), bytes).unwrap();
            transfer(lock, archives, &store, None).unwrap();
        }
        let feed = ConsumerServer::start(store.clone(), None);
        for (lock, bytes) in [
            (&first, b"first".as_slice()),
            (&second, b"second".as_slice()),
        ] {
            let cache = dir.join("guest");
            transfer(lock, &cache, &store, Some(&feed.base)).unwrap();
            assert_eq!(std::fs::read(cache.join("dep-1.0.0.crate")).unwrap(), bytes);
        }
        drop(feed);
        for (i, response) in [
            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
            b"HTTP/1.1 200 OK\r\nContent-Length: 7\r\nConnection: close\r\n\r\ncorrupt".to_vec(),
            format!("HTTP/1.1 302 Found\r\nLocation: {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n", origin.base).into_bytes(),
            b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nf".to_vec(),
        ].into_iter().enumerate() {
            let feed = ConsumerServer::start(dir.join("unused"), Some(response));
            let cache = dir.join(format!("bad-{i}"));
            let error = transfer(&first, &cache, &store, Some(&feed.base)).unwrap_err();
            assert!(error.contains("1 registry archive(s)"), "{error}");
            assert!(!cache.join("dep-1.0.0.crate").exists());
            assert_eq!(origin.count(), 0);
        }
        drop(origin);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn cargo_transfer_rejects_unbounded_special_and_unsafe_locks() {
        use super::read_locked_cargo_sources as read_lock;
        let dir = unique_tmp_dir("cargo-transfer-lock");
        let lock = cargo_transfer_lock(&dir, b"bytes", "https://example.invalid/git");
        let original = std::fs::read_to_string(&lock).unwrap();
        for replacement in ["../escape", "a/b", "a\\b"] {
            std::fs::write(
                &lock,
                original.replace("name = \"dep\"", &format!("name = \"{replacement}\"")),
            )
            .unwrap();
            assert!(read_lock(&lock).is_err());
        }
        std::fs::remove_file(&lock).unwrap();
        std::os::unix::fs::symlink("/dev/zero", &lock).unwrap();
        assert!(read_lock(&lock).is_err());
        assert!(read_lock(std::path::Path::new("/dev/zero")).is_err());
        assert!(read_lock(&dir).is_err());
        std::fs::remove_file(&lock).unwrap();
        std::fs::File::create(&lock)
            .unwrap()
            .set_len(super::MAX_CARGO_LOCK_BYTES + 1)
            .unwrap();
        assert!(read_lock(&lock).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn exported_private_sources_feed_two_guests_without_upstream_or_shared_writes() {
        let dir = unique_tmp_dir("export-consumers");
        let sources = dir.join("sources");
        let store = dir.join("store");
        std::fs::create_dir_all(&sources).unwrap();
        let origin = ConsumerServer::start(dir.join("upstream"), None);
        let bytes = b"already downloaded on the host";
        let pin = super::SourcePin {
            key: "fixture".into(),
            url: format!("{}/source.tar", origin.base),
            sha256: super::hex_sha256(bytes),
            file: "source.tar".into(),
        };
        std::fs::write(sources.join(&pin.file), bytes).unwrap();
        super::export_source_pins(std::slice::from_ref(&pin), &sources, &store).unwrap();
        let hosted = store.join(super::strip_scheme(&pin.url));
        assert_eq!(std::fs::read(&hosted).unwrap(), bytes);
        assert_eq!(
            super::read_digest_sidecar(&super::sidecar_path(&hosted)).unwrap(),
            pin.sha256
        );
        std::fs::write(sources.join(&pin.file), b"changed private copy").unwrap();
        assert_eq!(std::fs::read(&hosted).unwrap(), bytes);
        std::fs::remove_file(super::sidecar_path(&hosted)).unwrap();
        super::export_source_pins(std::slice::from_ref(&pin), &sources, &store).unwrap();
        let feed = ConsumerServer::start(store.clone(), None);
        for guest in ["one", "two"] {
            super::consume_source_pins(std::slice::from_ref(&pin), &dir.join(guest), &feed.base)
                .unwrap();
            assert_eq!(
                std::fs::read(dir.join(guest).join(&pin.file)).unwrap(),
                bytes
            );
        }
        std::fs::write(dir.join("one").join(&pin.file), b"guest mutation").unwrap();
        assert_eq!(
            std::fs::read(dir.join("two").join(&pin.file)).unwrap(),
            bytes
        );
        assert_eq!(std::fs::read(&hosted).unwrap(), bytes);
        assert_eq!(origin.count(), 0);
        assert_eq!(feed.count(), 2);
        drop(feed);
        drop(origin);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn export_refuses_corrupt_missing_and_reserved_sources_without_partial_publication() {
        let dir = unique_tmp_dir("export-refusal");
        let sources = dir.join("sources");
        let store = dir.join("store");
        std::fs::create_dir_all(&sources).unwrap();
        let pin = super::SourcePin {
            key: "bad".into(),
            url: "https://example.invalid/bad.tar".into(),
            sha256: super::hex_sha256(b"expected"),
            file: "bad.tar".into(),
        };
        let missing = super::SourcePin {
            key: "missing".into(),
            url: "https://example.invalid/missing.tar".into(),
            sha256: pin.sha256.clone(),
            file: "missing.tar".into(),
        };
        std::fs::write(sources.join(&pin.file), b"corrupt").unwrap();
        let error = super::export_source_pins(&[pin, missing], &sources, &store).unwrap_err();
        assert!(error.contains("2 source archive(s)"), "{error}");
        assert!(
            error.contains(&format!(
                "sha256 mismatch for {}",
                sources.join("bad.tar").display()
            )),
            "{error}"
        );
        assert!(error.contains("missing.tar"), "{error}");
        let parent = store.join("example.invalid");
        assert!(!parent.join("bad.tar").exists());
        assert!(!parent.join("bad.tar.sha256").exists());
        assert!(std::fs::read_dir(&parent).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".tmp")
        }));
        for url in [
            "https://example.invalid/.td-feed-download.lock",
            "https://example.invalid/.td-feed-download-1.tmp/file",
            "https://example.invalid/source.tar.sha256",
            "https://example.invalid/source.tar.sha256/child",
            "https://example.invalid/source.tar?download=1",
            "https://example.invalid/source.tar#fragment",
        ] {
            let pin = super::SourcePin {
                key: "reserved".into(),
                url: url.into(),
                sha256: super::hex_sha256(b"ok"),
                file: "safe.tar".into(),
            };
            assert!(super::export_source_pin(&pin, &sources, &store)
                .unwrap_err()
                .contains("safe feed path"));
            let entry = super::Entry {
                path: super::strip_scheme(&pin.url),
                url: pin.url.clone(),
                sha256: pin.sha256.clone(),
            };
            assert!(super::warm_one(&entry, &store)
                .unwrap_err()
                .contains("unsafe feed artifact path"));
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn two_consumers_use_verified_host_bytes_without_contacting_origin() {
        let dir = unique_tmp_dir("consumers");
        let store = dir.join("host");
        let origin = ConsumerServer::start(dir.join("upstream"), None);
        let bytes = b"one pinned source, two private caches";
        let pin = super::SourcePin {
            key: "fixture".into(), url: format!("{}/source.tar", origin.base),
            sha256: super::hex_sha256(bytes), file: "source.tar".into(),
        };
        let host_file = store.join(super::strip_scheme(&pin.url));
        std::fs::create_dir_all(host_file.parent().unwrap()).unwrap();
        std::fs::write(&host_file, bytes).unwrap();
        std::fs::write(super::sidecar_path(&host_file), format!("{}\n", pin.sha256)).unwrap();
        let feed = ConsumerServer::start(store, None);
        for guest in ["first", "second"] {
            super::consume_source_pins(std::slice::from_ref(&pin), &dir.join(guest), &feed.base)
                .unwrap();
            assert_eq!(
                std::fs::read(dir.join(guest).join(&pin.file)).unwrap(),
                bytes
            );
        }
        assert_eq!(feed.count(), 2);
        assert_eq!(origin.count(), 0);
        std::fs::write(&host_file, b"corrupt host object").unwrap();
        let error =
            super::consume_source_pins(std::slice::from_ref(&pin), &dir.join("third"), &feed.base)
                .unwrap_err();
        assert!(error.contains("500"), "{error}");
        assert!(!dir.join("third").join(&pin.file).exists());
        std::fs::write(&host_file, bytes).unwrap();
        // A warm private cache remains usable after the host feed is unavailable.
        super::consume_source_pins(
            std::slice::from_ref(&pin),
            &dir.join("first"),
            "http://127.0.0.1:0",
        )
        .unwrap();
        std::fs::write(dir.join("first").join(&pin.file), b"private mutation").unwrap();
        assert_eq!(std::fs::read(&host_file).unwrap(), bytes);
        assert_eq!(
            std::fs::read(dir.join("second").join(&pin.file)).unwrap(),
            bytes
        );
        drop(feed);
        let restarted = ConsumerServer::start(dir.join("host"), None);
        super::consume_source_pins(
            std::slice::from_ref(&pin),
            &dir.join("third"),
            &restarted.base,
        )
        .unwrap();
        assert_eq!(origin.count(), 0);
        drop(restarted);
        drop(origin);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn consumer_reports_all_missing_pins_in_one_pass() {
        let dir = unique_tmp_dir("consumer-all-misses");
        let feed = ConsumerServer::start(dir.join("empty"), None);
        let mut pins: Vec<_> = ["first", "second"]
            .into_iter()
            .map(|key| super::SourcePin {
                key: key.into(),
                url: format!("https://example.invalid/{key}"),
                sha256: super::hex_sha256(b"bytes"),
                file: key.into(),
            })
            .collect();
        pins.insert(
            0,
            super::SourcePin {
                key: "invalid".into(),
                url: "https://example.invalid/safe".into(),
                sha256: super::hex_sha256(b"bytes"),
                file: "..".into(),
            },
        );
        let error = super::consume_source_pins(&pins, &dir.join("guest"), &feed.base).unwrap_err();
        assert!(error.contains("3 source archive(s)"), "{error}");
        assert!(error.contains("source first") && error.contains("source second"));
        assert!(error.contains("unsafe source cache filename"));
        assert_eq!(feed.count(), 2);
        drop(feed);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn consumer_misses_corruption_and_redirects_never_fall_back_upstream() {
        let dir = unique_tmp_dir("consumer-refusal");
        let origin = ConsumerServer::start(dir.join("upstream"), None);
        let pin = super::SourcePin {
            key: "fixture".into(), url: format!("{}/source.tar", origin.base),
            sha256: super::hex_sha256(b"expected"), file: "source.tar".into(),
        };
        for (index, response) in [
            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
            b"HTTP/1.1 200 OK\r\nContent-Length: 7\r\nConnection: close\r\n\r\ncorrupt".to_vec(),
            format!("HTTP/1.1 302 Found\r\nLocation: {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n", pin.url).into_bytes(),
            b"HTTP/1.1 200 OK\r\nContent-Length: 8\r\nConnection: close\r\n\r\nex".to_vec(),
        ].into_iter().enumerate() {
            let feed = ConsumerServer::start(dir.join("unused"), Some(response));
            let dest = dir.join(index.to_string());
            let error = super::consume_source_pins(std::slice::from_ref(&pin), &dest, &feed.base).unwrap_err();
            assert!(error.contains("export this declared pin on the host"), "{error}");
            assert!(!dest.join(&pin.file).exists());
            assert_eq!(feed.count(), if index == 3 { 3 } else { 1 });
            assert_eq!(origin.count(), 0);
            assert!(std::fs::read_dir(&dest).unwrap().all(|entry| !entry.unwrap().file_name().to_string_lossy().ends_with(".tmp")));
        }
        drop(origin);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn hosted_checks_never_create_an_implicit_feed_daemon() {
        assert_eq!(feed_daemon_policy(None, true), FeedDaemonPolicy::Disabled);
        assert_eq!(
            feed_daemon_policy(Some("http://127.0.0.1:9".to_string()), true),
            FeedDaemonPolicy::Explicit("http://127.0.0.1:9".to_string()),
            "an explicitly supplied external feed remains usable"
        );
        assert_eq!(feed_daemon_policy(None, false), FeedDaemonPolicy::Ensure);
    }

    // Detaching must be idempotent (a warm re-runs over an already-extracted tree)
    // and must leave a crate that is already its own root untouched.
    #[test]
    fn an_extracted_crate_is_detached_from_any_ambient_workspace() {
        let dir = unique_tmp_dir("detach");
        std::fs::create_dir_all(&dir).unwrap();
        let manifest = dir.join("Cargo.toml");

        // A plain upstream manifest gains the terminating table.
        std::fs::write(&manifest, "[package]\nname = \"rg\"\n").unwrap();
        detach_from_workspace(&manifest).unwrap();
        let once = std::fs::read_to_string(&manifest).unwrap();
        assert!(once.contains("[workspace]"), "{once}");
        assert!(
            once.contains("name = \"rg\""),
            "the package must survive: {once}"
        );
        // Re-running leaves it byte-identical (a warm re-extracts and re-detaches).
        detach_from_workspace(&manifest).unwrap();
        assert_eq!(once, std::fs::read_to_string(&manifest).unwrap());

        // A crate that already declares a workspace — including only a sub-table,
        // which makes it a root just the same — is left exactly as upstream wrote it.
        // A crate that already declares a workspace — including only a sub-table,
        // which makes it a root just the same — is left exactly as upstream wrote it.
        // The last two are TOML the publisher normalizer would not emit but the
        // parser accepts; a second `[workspace]` there is a parse error, not a detach.
        for existing in [
            "[package]\nname = \"a\"\n\n[workspace]\nmembers = [\"x\"]\n",
            "[package]\nname = \"b\"\n\n[workspace.metadata.thing]\nk = 1\n",
            "[package]\nname = \"c\"\n\n[ workspace ]\n",
            "[package]\nname = \"d\"\n\n[\"workspace\"]\n",
        ] {
            std::fs::write(&manifest, existing).unwrap();
            detach_from_workspace(&manifest).unwrap();
            assert_eq!(existing, std::fs::read_to_string(&manifest).unwrap());
        }

        // ... but these are NOT workspace tables, and each must still be detached: a
        // table merely NAMED like one, a comment, and — the case that actually occurs
        // upstream — a bracketed line inside a `package.metadata.release` changelog
        // template, which is string body rather than TOML structure.
        for other in [
            "[package]\nname = \"e\"\n\n[workspacey]\n",
            "[package]\nname = \"f\"\n# [workspace]\n",
            "[package]\nname = \"g\"\n\n[package.metadata.release]\nreplace = \"\"\"\n[workspace]\n\"\"\"\n",
            "[package]\nname = \"h\"\ndescription = '''\n[workspace.metadata]\n'''\n",
        ] {
            std::fs::write(&manifest, other).unwrap();
            detach_from_workspace(&manifest).unwrap();
            let text = std::fs::read_to_string(&manifest).unwrap();
            assert!(text.lines().any(|l| l == "[workspace]"), "{text}");
        }

        // A one-line `x = """…"""` opens and closes on the same line, so a REAL table
        // after it is still seen (the scanner must not be left stuck open).
        std::fs::write(
            &manifest,
            "[package]\nname = \"i\"\ndescription = \"\"\"one\"\"\"\n\n[workspace]\n",
        )
        .unwrap();
        let before = std::fs::read_to_string(&manifest).unwrap();
        detach_from_workspace(&manifest).unwrap();
        assert_eq!(before, std::fs::read_to_string(&manifest).unwrap());

        // A manifest with no trailing newline still yields a parseable table header
        // on its own line, not glued to the last key.
        std::fs::write(&manifest, "[package]\nname = \"c\"").unwrap();
        detach_from_workspace(&manifest).unwrap();
        let text = std::fs::read_to_string(&manifest).unwrap();
        assert!(
            text.lines().any(|l| l == "[workspace]"),
            "the table header must stand alone: {text:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn kernel_headers_tar_is_uncompressed_and_normalized() {
        // The kernel-headers seed is GENERATED (no upstream sha256 pin): its
        // committed row in seed/seed-digests.txt binds the packed bytes, so the
        // bytes must be a host-free function of the header CONTENT. A gzip layer
        // is NOT host-free — its DEFLATE stream varies by zlib/gzip version — so
        // a compressed tarball re-keyed the seed digest on every host whose gzip
        // differed from the one that blessed the table, reddening the seed
        // provenance gate off that host (re #469). Lock both invariants so a
        // future edit cannot silently reintroduce compression or drop a
        // normalization flag.
        let flags = super::KERNEL_HEADERS_TAR_FLAGS;
        assert!(
            flags.contains(&"-cf"),
            "must create an UNCOMPRESSED tar (-cf), got {flags:?}"
        );
        for bad in [
            "-z", "-czf", "-cJf", "-cjf", "--gzip", "--xz", "--bzip2", "--zstd",
        ] {
            assert!(
                !flags.contains(&bad),
                "compression flag {bad} reintroduces host-dependent (non-reproducible) bytes"
            );
        }
        for need in [
            "--sort=name",
            "--mtime=@0",
            "--owner=0",
            "--group=0",
            "--numeric-owner",
        ] {
            assert!(flags.contains(&need), "missing normalization flag {need}");
        }
    }

    #[test]
    fn pack_command_scrubs_host_tar_env_and_stays_uncompressed() {
        use std::path::Path;
        // GNU tar PREPENDS $TAR_OPTIONS to every run, so an ambient `TAR_OPTIONS=-z`
        // or `--blocking-factor=N` on the warming host would re-key the seed digest
        // despite KERNEL_HEADERS_TAR_FLAGS. Lock that the packing command removes it
        // (and pins LC_ALL=C), and still carries the uncompressed normalized flags.
        let cmd = super::kernel_headers_pack_command(Path::new("/x/out.tar"), Path::new("/x/inc"));
        let removes_tar_options = cmd
            .get_envs()
            .any(|(k, v)| k == "TAR_OPTIONS" && v.is_none());
        assert!(
            removes_tar_options,
            "packing must scrub ambient TAR_OPTIONS"
        );
        let pins_c_locale = cmd
            .get_envs()
            .any(|(k, v)| k == "LC_ALL" && v == Some(std::ffi::OsStr::new("C")));
        assert!(pins_c_locale, "packing must pin LC_ALL=C");
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(
            args.contains(&"-cf".to_string()),
            "must stay uncompressed (-cf)"
        );
        for bad in ["-z", "--gzip", "-czf"] {
            assert!(!args.iter().any(|a| a == bad), "must not pack with {bad}");
        }
    }

    #[test]
    fn strips_kbuild_byproducts_but_keeps_headers() {
        use std::fs;
        use std::path::Path;
        // Classification: markers/command-records are byproducts; headers are not.
        assert!(super::is_kbuild_byproduct(Path::new("/k/asm/.install")));
        assert!(super::is_kbuild_byproduct(Path::new(
            "/k/asm/..install.cmd"
        )));
        assert!(!super::is_kbuild_byproduct(Path::new("/k/asm/types.h")));
        assert!(!super::is_kbuild_byproduct(Path::new("/k/linux/version.h")));

        // Recursive strip on a mock headers_install tree: byproducts (which carry
        // the non-reproducible build/store paths) go; the headers stay.
        let root = unique_tmp_dir("strip");
        let _ = fs::remove_dir_all(&root);
        let asm = root.join("asm");
        fs::create_dir_all(&asm).unwrap();
        fs::write(asm.join("types.h"), b"typedef int x;").unwrap();
        fs::write(asm.join(".install"), b"asm/types.h\n").unwrap();
        fs::write(
            asm.join("..install.cmd"),
            b"cmd_/tmp/td-feed-kh-i386-27905/hdr := /gnu/store/abc-bash/bin/sh ...",
        )
        .unwrap();
        fs::write(root.join("version.h"), b"#define LINUX_VERSION_CODE 265795").unwrap();

        super::strip_kbuild_byproducts(&root).unwrap();

        // Collect results, clean up, THEN assert — so a failing assertion cannot leak
        // the temp tree (the cleanup runs before any panic unwinds the test).
        let kept_header = asm.join("types.h").is_file();
        let kept_version = root.join("version.h").is_file();
        let dropped_install = !asm.join(".install").exists();
        let dropped_cmd = !asm.join("..install.cmd").exists();
        let _ = fs::remove_dir_all(&root);

        assert!(kept_header, "must keep a real UAPI header");
        assert!(kept_version, "must keep version.h");
        assert!(dropped_install, "must remove the .install marker");
        assert!(dropped_cmd, "must remove the ..install.cmd command record");
    }

    #[test]
    fn normalize_header_modes_forces_umask_independent_bits() {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;
        use std::path::Path;
        // headers_install honors the ambient umask, so a restrictive warm (umask 0077)
        // yields 0700 dirs / 0600 files, and tar records those bits — re-keying the seed
        // digest per host even for identical header CONTENT (re #469). Simulate that
        // restrictive tree and assert normalize_header_modes rewrites EVERY dir to 0755
        // and EVERY file to 0644, the canonical umask-independent set.
        let root = unique_tmp_dir("mode");
        let _ = fs::remove_dir_all(&root);
        let asm = root.join("asm");
        fs::create_dir_all(&asm).unwrap();
        fs::write(asm.join("types.h"), b"typedef int x;").unwrap();
        fs::write(root.join("version.h"), b"#define V 1").unwrap();
        for (p, mode) in [
            (root.as_path(), 0o700),
            (asm.as_path(), 0o700),
            (asm.join("types.h").as_path(), 0o600),
            (root.join("version.h").as_path(), 0o600),
        ] {
            fs::set_permissions(p, fs::Permissions::from_mode(mode)).unwrap();
        }

        super::normalize_header_modes(&root).unwrap();

        // Read modes, clean up, THEN assert (no temp-tree leak on a failing assertion).
        let mode_of = |p: &Path| fs::symlink_metadata(p).unwrap().permissions().mode() & 0o777;
        let root_mode = mode_of(&root);
        let asm_mode = mode_of(&asm);
        let header_mode = mode_of(&asm.join("types.h"));
        let version_mode = mode_of(&root.join("version.h"));
        let _ = fs::remove_dir_all(&root);

        assert_eq!(root_mode, 0o755, "root dir must be forced to 0755");
        assert_eq!(asm_mode, 0o755, "subdir must be forced to 0755");
        assert_eq!(header_mode, 0o644, "header file must be forced to 0644");
        assert_eq!(version_mode, 0o644, "version.h must be forced to 0644");
    }

    #[test]
    fn parses_the_serve_announcement_line() {
        // The exact line `serve` prints (the format ensure-serve scrapes).
        let line = "td-feed: serving /home/x/.td/feed/store on http://127.0.0.1:54321/";
        assert_eq!(parse_serve_addr(line), Some("127.0.0.1:54321".to_string()));
    }

    #[test]
    fn rejects_non_announcement_lines() {
        // A daemon-startup error, a blank line, and a malformed URL must NOT be
        // mistaken for an address (ensure-serve would otherwise print garbage).
        assert_eq!(
            parse_serve_addr("bind 127.0.0.1:0: Address already in use"),
            None
        );
        assert_eq!(parse_serve_addr(""), None);
        assert_eq!(parse_serve_addr("on http://127.0.0.1/"), None); // no port
        assert_eq!(parse_serve_addr("on http://:8080/"), None); // no host
        assert_eq!(parse_serve_addr("on http://host:port/"), None); // non-numeric port
    }

    #[test]
    fn fixed_output_workspace_warm_coordinates_cannot_traverse() {
        let sha = "a".repeat(64);
        assert!(valid_crate_source_coordinates(
            "source.tar.gz",
            &sha,
            "recipes/locks/codex/Cargo.lock",
            "codex"
        ));
        for (file, digest, lock, dest) in [
            (
                "../source.tar.gz",
                sha.as_str(),
                "recipes/locks/codex/Cargo.lock",
                "codex",
            ),
            (
                "source.tar.gz",
                "A",
                "recipes/locks/codex/Cargo.lock",
                "codex",
            ),
            ("source.tar.gz", sha.as_str(), "../Cargo.lock", "codex"),
            (
                "source.tar.gz",
                sha.as_str(),
                "recipes/locks/codex/Cargo.lock",
                "../codex",
            ),
        ] {
            assert!(!valid_crate_source_coordinates(file, digest, lock, dest));
        }
    }

    #[test]
    fn committed_lock_resolution_rejects_symlinks() {
        let root =
            std::env::temp_dir().join(format!("td-feed-committed-lock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("locks/real")).unwrap();
        std::fs::write(root.join("locks/real/Cargo.lock"), "version = 4\n").unwrap();
        assert_eq!(
            real_relative_file(&root, "locks/real/Cargo.lock", "lock").unwrap(),
            root.join("locks/real/Cargo.lock")
        );
        assert!(real_relative_file(&root, "", "lock").is_err());
        std::os::unix::fs::symlink("real", root.join("locks/link")).unwrap();
        let error = real_relative_file(&root, "locks/link/Cargo.lock", "lock").unwrap_err();
        assert!(error.contains("symlink"), "{error}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn feed_dir_resolution_prefers_non_empty_td_feed_dir() {
        // A non-empty TD_FEED_DIR wins verbatim; empty/whitespace falls to
        // <HOME>/.td/feed; a missing HOME degrades to a relative ./.td/feed. This
        // is the store both `ensure-serve` and the self-ensuring `warm sources`
        // share, so the two MUST resolve identically.
        assert_eq!(
            resolve_feed_dir(Some("/x/feed"), Some("/home/u")),
            PathBuf::from("/x/feed")
        );
        assert_eq!(
            resolve_feed_dir(Some("  /x/feed  "), Some("/home/u")),
            PathBuf::from("/x/feed"),
            "a padded value is trimmed, not turned into a spaced dir name"
        );
        assert_eq!(
            resolve_feed_dir(Some("   "), Some("/home/u")),
            PathBuf::from("/home/u/.td/feed")
        );
        assert_eq!(
            resolve_feed_dir(None, Some("/home/u")),
            PathBuf::from("/home/u/.td/feed")
        );
        assert_eq!(resolve_feed_dir(None, None), PathBuf::from("./.td/feed"));
    }

    #[test]
    fn ensure_serve_daemon_reuses_a_live_reachable_daemon_without_spawning() {
        use std::fs;
        use std::net::TcpListener;
        // The cross-worktree share: a later caller (any worktree, `ensure-serve`
        // OR the self-ensuring `warm sources`) reuses the daemon a prior caller
        // recorded, rather than egressing/serving on its own — but ONLY when that
        // daemon is still reachable (a live pid AND a listener at the recorded
        // addr), so a recycled pid pointing at a dead port is not trusted. Bind a
        // REAL loopback listener, record OUR pid (guaranteed alive) + its addr,
        // then assert ensure_serve_daemon returns that addr WITHOUT spawning. (No
        // env mutation and no subprocess, so it stays hermetic under the
        // concurrent single-process test runner.)
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let dir = unique_tmp_dir("ensure-reuse");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("store")).unwrap();
        fs::write(dir.join("feed.pid"), format!("{}\n", std::process::id())).unwrap();
        fs::write(dir.join("feed.addr"), format!("{addr}\n")).unwrap();

        let got = ensure_serve_daemon(&dir);

        // Read the result, clean up, THEN assert (no temp-tree leak on failure).
        let _ = fs::remove_dir_all(&dir);
        assert_eq!(got, Ok(addr));
    }

    #[test]
    fn feed_addr_reachability_probe_distinguishes_listening_from_dead() {
        use std::net::TcpListener;
        // The reuse guard: a bound listener is reachable; a port with nothing
        // listening and an unparseable address are not. `127.0.0.1:0` never
        // accepts a connection, so it is a stable "dead" endpoint (no drop-then-
        // probe race on a freed ephemeral port).
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        assert!(
            super::feed_addr_reachable(&addr),
            "a bound loopback listener must be reachable"
        );
        assert!(
            !super::feed_addr_reachable("127.0.0.1:0"),
            "port 0 never accepts a connection"
        );
        assert!(
            !super::feed_addr_reachable("not-an-address"),
            "an unparseable addr is unreachable, not a panic"
        );
    }
}

//! kv: a small crash-safe, single-writer key/value store.
//!
//! The store is an append-only log of commit records held open by one process
//! at a time (an exclusive file lock). Each acquired lock is explicitly
//! released on drop, including when a spawning child retains an inherited
//! descriptor before exec. Every committed record is fsynced, so a crash loses
//! at most the commit that was in flight. The whole live state is
//! kept in memory as ordered tables; readers take a cheap `Arc` snapshot that
//! a concurrent commit cannot disturb.
//!
//! # On-disk format
//!
//! ```text
//! file    := record*
//! record  := magic(8) | len:u32le | crc:u32le | payload[len]
//! payload := count:u32le | entry*
//! entry   := tlen:u32le | table[tlen] | op:u8 | klen:u32le | key[klen]
//!            | vlen:u32le | value[vlen]
//! op      := 0 insert | 1 remove (a remove still carries vlen = 0)
//! ```
//!
//! `magic` is `TDKVLOG1`, `crc` is CRC-32/ISO-HDLC over the payload, and all
//! integers are little endian. One record is one atomic commit: its entries
//! become visible together or not at all.
//!
//! # Recovery
//!
//! Replay stops and repairs the file (truncating to the last good record) when
//! the tail is short — a partial header, a payload shorter than its length, or
//! a bad CRC on a record that ends exactly at end-of-file. Those are the shapes
//! a crash mid-append leaves behind. A bad magic, or a bad CRC with records
//! after it, is reported as [`Error::Corrupt`]; callers are expected to delete
//! and recreate the store.
//!
//! # Limits
//!
//! One writer at a time (`write()` blocks other writers in-process, and the
//! file lock refuses other processes). The live state must fit in memory. A
//! single commit must encode to under 1 GiB. Keys and values are byte strings;
//! ordering is bytewise, which is why [`Key::from_u64`] uses big-endian. A
//! commit whose length field is corrupted into a value that runs past
//! end-of-file is indistinguishable from a truncated tail and is repaired
//! rather than reported.

use std::cmp::Ordering;
use std::collections::btree_map::Entry;
use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, RwLock};

const RECORD_MAGIC: [u8; 8] = *b"TDKVLOG1";
const HEADER_LEN: usize = 16;
const ENTRY_OVERHEAD: usize = 13; // tlen + op + klen + vlen
const MAX_RECORD_LEN: u32 = 1 << 30;
const COMPACT_CHUNK: usize = 4 << 20;
const MIN_COMPACT_BYTES: u64 = 1 << 20;
const OP_INSERT: u8 = 0;
const OP_REMOVE: u8 = 1;
const TMP_SUFFIX: &str = ".compact";
const REOPEN_ATTEMPTS: usize = 8;
// What a filesystem with no directory fsync answers (Linux values).
const EINVAL: i32 = 22;
const EOPNOTSUPP: i32 = 95;

type TableMap = BTreeMap<Vec<u8>, Arc<[u8]>>;
type Tables = BTreeMap<String, Arc<TableMap>>;
type Pending = BTreeMap<String, BTreeMap<Vec<u8>, Option<Arc<[u8]>>>>;

/// What can go wrong opening or writing a store.
#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    /// Another process (or another open handle) holds the store lock.
    Locked,
    /// The log is damaged in a way recovery cannot explain as a crash.
    Corrupt(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "kv io error: {}", e),
            Error::Locked => write!(f, "kv store is locked by another process"),
            Error::Corrupt(msg) => write!(f, "kv store is corrupt: {}", msg),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Error {
        Error::Io(e)
    }
}

fn framing_err() -> Error {
    Error::Io(io::Error::new(
        io::ErrorKind::InvalidData,
        "kv: bad framing",
    ))
}

fn oversize_err() -> Error {
    Error::Io(io::Error::new(
        io::ErrorKind::InvalidInput,
        "kv: commit exceeds maximum record size",
    ))
}

fn lock_err(e: TryLockError) -> Error {
    match e {
        TryLockError::WouldBlock => Error::Locked,
        TryLockError::Error(e) => Error::Io(e),
    }
}

/// Key encoding helpers. Ordering is bytewise, so `from_u64` is big endian and
/// numeric order equals iteration order.
pub struct Key;

impl Key {
    // Named for symmetry with `from_u64`, not the `FromStr` trait.
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(key: &str) -> Vec<u8> {
        key.as_bytes().to_vec()
    }

    pub fn from_u64(key: u64) -> Vec<u8> {
        key.to_be_bytes().to_vec()
    }

    pub fn as_str(key: &[u8]) -> Option<&str> {
        std::str::from_utf8(key).ok()
    }

    pub fn as_u64(key: &[u8]) -> Option<u64> {
        let bytes: [u8; 8] = key.try_into().ok()?;
        Some(u64::from_be_bytes(bytes))
    }
}

// CRC-32/ISO-HDLC. The table is built once, on first use, by `from_fn` rather
// than by indexed stores; lookups go through `get`. So nothing here can panic.
fn crc_table() -> [u32; 256] {
    std::array::from_fn(|i| {
        let mut crc = i as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                0xEDB8_8320 ^ (crc >> 1)
            } else {
                crc >> 1
            };
        }
        crc
    })
}

static CRC_TABLE: std::sync::LazyLock<[u32; 256]> = std::sync::LazyLock::new(crc_table);

fn crc32(data: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for byte in data {
        let index = usize::from((crc as u8) ^ *byte);
        crc = (crc >> 8) ^ CRC_TABLE.get(index).copied().unwrap_or(0);
    }
    crc ^ u32::MAX
}

fn as_u64(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}

fn entry_size(table: &str, key: &[u8], value: &[u8]) -> u64 {
    as_u64(ENTRY_OVERHEAD)
        .saturating_add(as_u64(table.len()))
        .saturating_add(as_u64(key.len()))
        .saturating_add(as_u64(value.len()))
}

fn write_at(buf: &mut [u8], at: usize, src: &[u8]) -> Result<(), Error> {
    let end = at.checked_add(src.len()).ok_or_else(framing_err)?;
    let slot = buf.get_mut(at..end).ok_or_else(framing_err)?;
    slot.copy_from_slice(src);
    Ok(())
}

fn read_u32(buf: &[u8], at: usize) -> Option<u32> {
    let end = at.checked_add(4)?;
    let bytes: [u8; 4] = buf.get(at..end)?.try_into().ok()?;
    Some(u32::from_le_bytes(bytes))
}

/// Read a length-prefixed byte string, returning it and the next offset.
fn read_field(buf: &[u8], at: usize) -> Option<(&[u8], usize)> {
    let len = usize::try_from(read_u32(buf, at)?).ok()?;
    let start = at.checked_add(4)?;
    let end = start.checked_add(len)?;
    Some((buf.get(start..end)?, end))
}

/// Accumulates one commit record. The 16-byte header and the entry count are
/// reserved up front and patched in `finish`.
struct RecordBuilder {
    buf: Vec<u8>,
    count: u32,
}

impl RecordBuilder {
    fn new() -> RecordBuilder {
        RecordBuilder {
            buf: vec![0u8; HEADER_LEN + 4],
            count: 0,
        }
    }

    fn is_empty(&self) -> bool {
        self.count == 0
    }

    fn body_len(&self) -> usize {
        self.buf.len().saturating_sub(HEADER_LEN)
    }

    fn push(&mut self, table: &str, op: u8, key: &[u8], value: &[u8]) -> Result<(), Error> {
        let tlen = u32::try_from(table.len()).map_err(|_| oversize_err())?;
        let klen = u32::try_from(key.len()).map_err(|_| oversize_err())?;
        let vlen = u32::try_from(value.len()).map_err(|_| oversize_err())?;
        self.buf.extend_from_slice(&tlen.to_le_bytes());
        self.buf.extend_from_slice(table.as_bytes());
        self.buf.push(op);
        self.buf.extend_from_slice(&klen.to_le_bytes());
        self.buf.extend_from_slice(key);
        self.buf.extend_from_slice(&vlen.to_le_bytes());
        self.buf.extend_from_slice(value);
        self.count = self.count.saturating_add(1);
        Ok(())
    }

    fn finish(mut self) -> Result<Vec<u8>, Error> {
        let count = self.count.to_le_bytes();
        write_at(&mut self.buf, HEADER_LEN, &count)?;
        let body = self.buf.get(HEADER_LEN..).ok_or_else(framing_err)?;
        let len = u32::try_from(body.len()).map_err(|_| oversize_err())?;
        if len > MAX_RECORD_LEN {
            return Err(oversize_err());
        }
        let crc = crc32(body);
        write_at(&mut self.buf, 0, &RECORD_MAGIC)?;
        write_at(&mut self.buf, 8, &len.to_le_bytes())?;
        write_at(&mut self.buf, 12, &crc.to_le_bytes())?;
        Ok(self.buf)
    }
}

fn insert_entry(map: &mut TableMap, table: &str, key: Vec<u8>, value: Arc<[u8]>, live: &mut u64) {
    match map.entry(key) {
        Entry::Occupied(mut slot) => {
            let old = entry_size(table, slot.key(), slot.get());
            let new = entry_size(table, slot.key(), &value);
            *live = live.saturating_sub(old).saturating_add(new);
            slot.insert(value);
        }
        Entry::Vacant(slot) => {
            *live = live.saturating_add(entry_size(table, slot.key(), &value));
            slot.insert(value);
        }
    }
}

fn remove_entry(map: &mut TableMap, table: &str, key: &[u8], live: &mut u64) {
    if let Some(old) = map.remove(key) {
        *live = live.saturating_sub(entry_size(table, key, &old));
    }
}

fn table_mut<'a>(tables: &'a mut Tables, name: &str) -> &'a mut TableMap {
    let slot = tables
        .entry(name.to_string())
        .or_insert_with(|| Arc::new(TableMap::new()));
    // Cheap when nothing else holds the table; clones once per touched table
    // while a reader still holds the previous snapshot.
    Arc::make_mut(slot)
}

fn apply_payload(payload: &[u8], tables: &mut Tables, live: &mut u64) -> Result<(), String> {
    let count = read_u32(payload, 0).ok_or_else(|| "short payload".to_string())?;
    let mut at = 4usize;
    for index in 0..count {
        let bad = |what: &str| format!("entry {} of {}: {}", index, count, what);
        let (table, next) = read_field(payload, at).ok_or_else(|| bad("table name"))?;
        let table = std::str::from_utf8(table).map_err(|_| bad("table name is not utf-8"))?;
        let op = *payload.get(next).ok_or_else(|| bad("op"))?;
        let after_op = next.checked_add(1).ok_or_else(|| bad("op"))?;
        let (key, next) = read_field(payload, after_op).ok_or_else(|| bad("key"))?;
        let (value, next) = read_field(payload, next).ok_or_else(|| bad("value"))?;
        let map = table_mut(tables, table);
        match op {
            OP_INSERT => insert_entry(map, table, key.to_vec(), Arc::from(value), live),
            OP_REMOVE => remove_entry(map, table, key, live),
            other => return Err(bad(&format!("unknown op {}", other))),
        }
        at = next;
    }
    if at != payload.len() {
        return Err(format!("{} trailing payload bytes", payload.len() - at));
    }
    Ok(())
}

fn read_fill(reader: &mut impl Read, buf: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0usize;
    while filled < buf.len() {
        let Some(slot) = buf.get_mut(filled..) else {
            break;
        };
        match reader.read(slot) {
            Ok(0) => break,
            Ok(n) => filled = filled.saturating_add(n),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(filled)
}

struct Replayed {
    tables: Tables,
    live: u64,
    end: u64,
    repaired: bool,
}

fn replay(file: &File) -> Result<Replayed, Error> {
    let file_len = file.metadata()?.len();
    let mut cursor = file;
    cursor.seek(SeekFrom::Start(0))?;
    let mut reader = BufReader::new(cursor);

    let mut tables = Tables::new();
    let mut live = 0u64;
    let mut pos = 0u64;
    let mut repaired = false;
    let mut header = [0u8; HEADER_LEN];

    loop {
        let read = read_fill(&mut reader, &mut header)?;
        if read == 0 {
            break;
        }
        if read < HEADER_LEN {
            repaired = true; // partial header: a crash mid-append
            break;
        }
        if header.get(..8) != Some(RECORD_MAGIC.as_slice()) {
            return Err(Error::Corrupt(format!(
                "bad record magic at offset {}",
                pos
            )));
        }
        let len = read_u32(&header, 8).ok_or_else(framing_err)?;
        let crc = read_u32(&header, 12).ok_or_else(framing_err)?;
        if len > MAX_RECORD_LEN {
            return Err(Error::Corrupt(format!(
                "record at offset {} declares {} bytes",
                pos, len
            )));
        }
        let body_start = pos.saturating_add(as_u64(HEADER_LEN));
        if u64::from(len) > file_len.saturating_sub(body_start) {
            repaired = true; // payload runs past end-of-file
            break;
        }
        let size = usize::try_from(len).map_err(|_| framing_err())?;
        let mut payload = vec![0u8; size];
        if read_fill(&mut reader, &mut payload)? < size {
            repaired = true;
            break;
        }
        let record_end = body_start.saturating_add(u64::from(len));
        if crc32(&payload) != crc {
            if record_end == file_len {
                repaired = true; // torn final record
                break;
            }
            return Err(Error::Corrupt(format!("crc mismatch at offset {}", pos)));
        }
        apply_payload(&payload, &mut tables, &mut live)
            .map_err(|msg| Error::Corrupt(format!("record at offset {}: {}", pos, msg)))?;
        pos = record_end;
    }

    Ok(Replayed {
        tables,
        live,
        end: pos,
        repaired: repaired || pos != file_len,
    })
}

/// Make the directory entry durable: a created or renamed file whose
/// directory was never synced can be missing after a crash, and a store
/// that treated that as success would promise more than the disk holds.
fn sync_parent_dir(path: &Path) -> io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let parent = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };
    match File::open(parent)?.sync_all() {
        Ok(()) => Ok(()),
        // A filesystem with no directory fsync cannot be asked for one:
        // that is its limit, not a failed sync, and a store that refused
        // to open there would lose the function without keeping the
        // promise either way.
        Err(e) if matches!(e.raw_os_error(), Some(EINVAL) | Some(EOPNOTSUPP)) => Ok(()),
        Err(e) => Err(e),
    }
}

/// Whether `file` is the inode `path` names now. A compaction renames a
/// fresh inode over the path and drops the old one, releasing its lock;
/// a handle opened before that rename and locked after it holds an
/// unlinked file, and a store built on it would write where nobody
/// reads.
fn names_this_inode(file: &File, path: &Path) -> io::Result<bool> {
    use std::os::unix::fs::MetadataExt;
    let held = file.metadata()?;
    let named = fs::metadata(path)?;
    Ok(held.ino() == named.ino() && held.dev() == named.dev())
}

/// Open `path` and take its lock, again if the locked inode turns out
/// not to be the one the path names any more.
fn open_locked(path: &Path) -> Result<LockedFile, Error> {
    for _ in 0..REOPEN_ATTEMPTS {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        file.try_lock().map_err(lock_err)?;
        let file = LockedFile(file);
        match names_this_inode(&file, path) {
            Ok(true) => return Ok(file),
            Ok(false) => {}
            // The path went away between the open and the stat (a clear
            // racing this open); the next attempt creates it again.
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(Error::Io(e)),
        }
    }
    Err(Error::Io(io::Error::other(
        "the store was replaced under every attempt to open it",
    )))
}

fn tmp_path_for(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(TMP_SUFFIX);
    PathBuf::from(name)
}

/// Own a lock from acquisition through recovery, compaction and teardown.
struct LockedFile(File);

impl std::ops::Deref for LockedFile {
    type Target = File;

    fn deref(&self) -> &File {
        &self.0
    }
}

impl std::ops::DerefMut for LockedFile {
    fn deref_mut(&mut self) -> &mut File {
        &mut self.0
    }
}

impl Drop for LockedFile {
    fn drop(&mut self) {
        // A concurrent fork can retain this open file description until exec.
        // Closing our descriptor alone would leave its flock held meanwhile.
        let _ = self.0.unlock();
    }
}

/// The log file plus the bookkeeping only a writer needs.
struct Writer {
    file: LockedFile,
    end: u64,
    live: u64,
    /// A compaction's rename whose directory sync failed: until it is
    /// made durable, a crash could bring the old name back, so no commit
    /// is acknowledged on the new inode before the sync is done.
    dir_pending: bool,
}

impl Writer {
    fn append(&mut self, record: &[u8], path: &Path) -> Result<(), Error> {
        if self.dir_pending {
            sync_parent_dir(path)?;
            self.dir_pending = false;
        }
        self.file.seek(SeekFrom::Start(self.end))?;
        if let Err(e) = self
            .file
            .write_all(record)
            .and_then(|()| self.file.sync_data())
        {
            // Roll back a partial append so the next commit starts clean.
            let _ = self.file.set_len(self.end);
            let _ = self.file.sync_data();
            return Err(Error::Io(e));
        }
        self.end = self.end.saturating_add(as_u64(record.len()));
        Ok(())
    }

    fn should_compact(&self) -> bool {
        self.end >= MIN_COMPACT_BYTES && self.end > self.live.saturating_mul(2)
    }

    /// Rewrite the live state to a temp file and rename it over the log. The
    /// temp file is locked before the rename, so the store lock is held
    /// continuously on whatever inode `path` names.
    fn rewrite(&mut self, tables: &Tables, path: &Path, tmp: &Path) -> Result<(), Error> {
        let (file, end, live) = match Writer::write_snapshot(tables, tmp) {
            Ok(v) => v,
            Err(e) => {
                let _ = fs::remove_file(tmp);
                return Err(e);
            }
        };
        if let Err(e) = fs::rename(tmp, path) {
            drop(file);
            let _ = fs::remove_file(tmp);
            return Err(Error::Io(e));
        }
        // The rename is done whichever way the sync goes: the writer
        // follows the inode the path names now. If the directory entry
        // is not durable yet, the caller hears so, and `append` syncs
        // before the next commit is acknowledged.
        self.file = file;
        self.end = end;
        self.live = live;
        match sync_parent_dir(path) {
            Ok(()) => {
                self.dir_pending = false;
                Ok(())
            }
            Err(e) => {
                self.dir_pending = true;
                Err(Error::Io(e))
            }
        }
    }

    fn write_snapshot(tables: &Tables, tmp: &Path) -> Result<(LockedFile, u64, u64), Error> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(tmp)?;
        file.try_lock().map_err(lock_err)?;
        let mut file = LockedFile(file);

        let mut end = 0u64;
        let mut live = 0u64;
        let mut record = RecordBuilder::new();
        for (name, map) in tables {
            for (key, value) in map.iter() {
                record.push(name, OP_INSERT, key, value)?;
                live = live.saturating_add(entry_size(name, key, value));
                if record.body_len() >= COMPACT_CHUNK {
                    let bytes = std::mem::replace(&mut record, RecordBuilder::new()).finish()?;
                    file.write_all(&bytes)?;
                    end = end.saturating_add(as_u64(bytes.len()));
                }
            }
        }
        if !record.is_empty() {
            let bytes = record.finish()?;
            file.write_all(&bytes)?;
            end = end.saturating_add(as_u64(bytes.len()));
        }
        file.sync_all()?;
        Ok((file, end, live))
    }
}

/// A single-writer key/value store backed by one log file.
pub struct Store {
    path: PathBuf,
    tmp_path: PathBuf,
    snapshot: RwLock<Arc<Tables>>,
    writer: Mutex<Writer>,
    /// Counts commits that had to copy the snapshot because a reader held it.
    #[cfg(test)]
    snapshot_clones: std::sync::atomic::AtomicU64,
}

impl Store {
    /// Open (creating if needed) the store at `path`, replaying its log.
    ///
    /// Returns [`Error::Locked`] if another handle already owns the file,
    /// [`Error::Corrupt`] if the log cannot be replayed, and [`Error::Io`]
    /// for the rest, a path replaced under every attempt to open it
    /// included.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Store, Error> {
        let path = path.as_ref().to_path_buf();
        let mut file = open_locked(&path)?;
        // Whether this open created the file or found it, its name is
        // made durable once here rather than on the first commit.
        sync_parent_dir(&path)?;

        let tmp_path = tmp_path_for(&path);
        match fs::remove_file(&tmp_path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(Error::Io(e)),
        }

        let replayed = replay(&file)?;
        if replayed.repaired {
            file.set_len(replayed.end)?;
            file.sync_all()?;
        }
        file.seek(SeekFrom::Start(replayed.end))?;

        Ok(Store {
            path,
            tmp_path,
            snapshot: RwLock::new(Arc::new(replayed.tables)),
            writer: Mutex::new(Writer {
                file,
                end: replayed.end,
                live: replayed.live,
                dir_pending: false,
            }),
            #[cfg(test)]
            snapshot_clones: std::sync::atomic::AtomicU64::new(0),
        })
    }

    #[cfg(test)]
    fn snapshot_clones(&self) -> u64 {
        self.snapshot_clones
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Take a consistent snapshot. Cheap: one `Arc` clone, no copying.
    pub fn read(&self) -> ReadTxn {
        let guard = self
            .snapshot
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        ReadTxn {
            tables: Arc::clone(&guard),
        }
    }

    /// Begin a write transaction, blocking until any other writer finishes.
    /// Dropping it without `commit` discards the buffered operations. Holding
    /// two write transactions on one thread deadlocks; reads never block.
    pub fn write(&self) -> WriteTxn<'_> {
        let guard = self
            .writer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let base = {
            let snapshot = self
                .snapshot
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            Arc::clone(&snapshot)
        };
        WriteTxn {
            store: self,
            guard,
            base,
            pending: Pending::new(),
        }
    }

    /// Rewrite the log so it holds exactly the live state.
    pub fn compact(&self) -> Result<(), Error> {
        let mut guard = self
            .writer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let snapshot = {
            let snapshot = self
                .snapshot
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            Arc::clone(&snapshot)
        };
        guard.rewrite(&snapshot, &self.path, &self.tmp_path)
    }

    /// Size of the log file as this store last wrote it.
    pub fn log_len(&self) -> u64 {
        let guard = self
            .writer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.end
    }
}

/// A point-in-time view of every table. Held snapshots are unaffected by
/// commits made after `Store::read`.
pub struct ReadTxn {
    tables: Arc<Tables>,
}

impl ReadTxn {
    pub fn table(&self, name: &str) -> Option<Table<'_>> {
        self.tables.get(name).map(|map| Table { map })
    }

    /// Names of the tables in this snapshot, in order.
    pub fn tables(&self) -> impl Iterator<Item = &str> {
        self.tables.keys().map(String::as_str)
    }
}

/// One table inside a [`ReadTxn`], ordered bytewise by key.
#[derive(Clone, Copy)]
pub struct Table<'a> {
    map: &'a TableMap,
}

/// Iterator over a [`Table`]'s entries.
pub type TableIter<'a> = std::iter::Map<
    std::collections::btree_map::Iter<'a, Vec<u8>, Arc<[u8]>>,
    fn((&'a Vec<u8>, &'a Arc<[u8]>)) -> (&'a [u8], &'a [u8]),
>;

fn borrow_pair<'a>(pair: (&'a Vec<u8>, &'a Arc<[u8]>)) -> (&'a [u8], &'a [u8]) {
    (pair.0.as_slice(), &**pair.1)
}

impl<'a> Table<'a> {
    pub fn get(&self, key: &[u8]) -> Option<&'a [u8]> {
        self.map.get(key).map(|value| &**value)
    }

    /// Entries in ascending key order.
    pub fn iter(&self) -> TableIter<'a> {
        self.map.iter().map(borrow_pair)
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Greatest key, or `None` when the table is empty.
    pub fn last_key(&self) -> Option<&'a [u8]> {
        self.map.keys().next_back().map(Vec::as_slice)
    }
}

impl<'a> IntoIterator for Table<'a> {
    type Item = (&'a [u8], &'a [u8]);
    type IntoIter = TableIter<'a>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

/// Buffered multi-table write. Operations become durable and visible together
/// at `commit`.
pub struct WriteTxn<'a> {
    store: &'a Store,
    guard: MutexGuard<'a, Writer>,
    base: Arc<Tables>,
    pending: Pending,
}

impl WriteTxn<'_> {
    pub fn insert(&mut self, table: &str, key: &[u8], value: &[u8]) {
        self.pending
            .entry(table.to_string())
            .or_default()
            .insert(key.to_vec(), Some(Arc::from(value)));
    }

    pub fn remove(&mut self, table: &str, key: &[u8]) {
        self.pending
            .entry(table.to_string())
            .or_default()
            .insert(key.to_vec(), None);
    }

    /// Read through the transaction's own uncommitted writes.
    pub fn get(&self, table: &str, key: &[u8]) -> Option<&[u8]> {
        if let Some(pending) = self.pending.get(table) {
            if let Some(value) = pending.get(key) {
                return value.as_deref();
            }
        }
        self.base.get(table)?.get(key).map(|value| &**value)
    }

    /// Greatest live key in `table`, accounting for pending writes. Lets a
    /// caller pick `max(key) + 1` and insert it in the same transaction.
    pub fn last_key(&self, table: &str) -> Option<Vec<u8>> {
        let mut base_iter = self.base.get(table).map(|map| map.keys());
        let mut pending_iter = self.pending.get(table).map(|map| map.iter());
        let mut base = base_iter.as_mut().and_then(|iter| iter.next_back());
        let mut pending = pending_iter.as_mut().and_then(|iter| iter.next_back());
        loop {
            match (base, pending) {
                (None, None) => return None,
                (Some(key), None) => return Some(key.clone()),
                (None, Some((key, value))) => {
                    if value.is_some() {
                        return Some(key.clone());
                    }
                    pending = pending_iter.as_mut().and_then(|iter| iter.next_back());
                }
                (Some(base_key), Some((key, value))) => match base_key.as_slice().cmp(key) {
                    Ordering::Greater => return Some(base_key.clone()),
                    Ordering::Equal => {
                        if value.is_some() {
                            return Some(key.clone());
                        }
                        base = base_iter.as_mut().and_then(|iter| iter.next_back());
                        pending = pending_iter.as_mut().and_then(|iter| iter.next_back());
                    }
                    Ordering::Less => {
                        if value.is_some() {
                            return Some(key.clone());
                        }
                        pending = pending_iter.as_mut().and_then(|iter| iter.next_back());
                    }
                },
            }
        }
    }

    /// Append the record, fsync it, then publish the new snapshot.
    pub fn commit(self) -> Result<(), Error> {
        let WriteTxn {
            store,
            mut guard,
            base,
            pending,
        } = self;
        // Release the transaction's snapshot handle so the swap below can
        // reuse the live tables instead of copying them.
        drop(base);
        if pending.is_empty() {
            return Ok(());
        }

        let mut record = RecordBuilder::new();
        for (table, ops) in &pending {
            for (key, value) in ops {
                match value {
                    Some(value) => record.push(table, OP_INSERT, key, value)?,
                    None => record.push(table, OP_REMOVE, key, &[])?,
                }
            }
        }
        let bytes = record.finish()?;
        guard.append(&bytes, &store.path)?;

        let published = {
            let mut slot = store
                .snapshot
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let previous = std::mem::replace(&mut *slot, Arc::new(Tables::new()));
            // Mutates in place when no reader holds the snapshot.
            let mut tables = match Arc::try_unwrap(previous) {
                Ok(tables) => tables,
                Err(shared) => {
                    #[cfg(test)]
                    store
                        .snapshot_clones
                        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    (*shared).clone()
                }
            };
            for (name, ops) in pending {
                let map = table_mut(&mut tables, &name);
                for (key, value) in ops {
                    match value {
                        Some(value) => insert_entry(map, &name, key, value, &mut guard.live),
                        None => remove_entry(map, &name, &key, &mut guard.live),
                    }
                }
            }
            let published = Arc::new(tables);
            *slot = Arc::clone(&published);
            published
        };

        if guard.should_compact() {
            // The commit is already durable; a failed compaction is not fatal.
            let _ = guard.rewrite(&published, &store.path, &store.tmp_path);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering as AtomicOrdering};
    use std::thread;

    static SEQ: AtomicU64 = AtomicU64::new(0);

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(tag: &str) -> TempDir {
            let seq = SEQ.fetch_add(1, AtomicOrdering::SeqCst);
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let path = std::env::temp_dir().join(format!(
                "tdstd-kv-{}-{}-{}-{}",
                tag,
                std::process::id(),
                nanos,
                seq
            ));
            fs::create_dir_all(&path).expect("create temp dir");
            TempDir { path }
        }

        fn db(&self) -> PathBuf {
            self.path.join("store.tdkv")
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn put(store: &Store, table: &str, key: &[u8], value: &[u8]) {
        let mut txn = store.write();
        txn.insert(table, key, value);
        txn.commit().expect("commit");
    }

    fn get(store: &Store, table: &str, key: &[u8]) -> Option<Vec<u8>> {
        let txn = store.read();
        let value = txn.table(table)?.get(key)?.to_vec();
        Some(value)
    }

    #[test]
    fn crc32_matches_known_vector() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn key_helpers_round_trip() {
        assert_eq!(Key::as_str(&Key::from_str("hello")), Some("hello"));
        assert_eq!(Key::as_u64(&Key::from_u64(42)), Some(42));
        assert_eq!(Key::from_u64(1), vec![0, 0, 0, 0, 0, 0, 0, 1]);
        assert!(Key::from_u64(2) > Key::from_u64(1));
        assert!(Key::from_u64(256) > Key::from_u64(255));
        assert_eq!(Key::as_u64(b"short"), None);
    }

    #[test]
    fn dropping_store_unlocks_while_a_duplicate_descriptor_survives() {
        for compact in [false, true] {
            let dir = TempDir::new("duplicate-lock");
            let store = Store::open(dir.db()).expect("open");
            put(&store, "t", b"k", b"value");
            if compact {
                store.compact().expect("compact");
            }
            // dup and fork retain the same open file description. Keep one
            // alive deterministically instead of racing another test's spawn.
            let duplicate = store
                .writer
                .lock()
                .expect("writer")
                .file
                .try_clone()
                .expect("duplicate");
            assert!(matches!(Store::open(dir.db()), Err(Error::Locked)));
            drop(store);
            let reopened = Store::open(dir.db()).expect("reopen with duplicate alive");
            assert_eq!(get(&reopened, "t", b"k").as_deref(), Some(&b"value"[..]));
            drop(duplicate);
            // Closing an old duplicate cannot release the new writer's lock.
            assert!(matches!(Store::open(dir.db()), Err(Error::Locked)));
            drop(reopened);
            assert!(Store::open(dir.db()).is_ok());
        }
    }

    #[test]
    fn failed_replay_unlocks_while_a_duplicate_descriptor_survives() {
        let dir = TempDir::new("failed-replay-lock");
        fs::write(dir.db(), [0u8; HEADER_LEN]).expect("corrupt header");
        let mut duplicate = None;
        let result = (|| -> Result<(), Error> {
            let file = open_locked(&dir.db())?;
            duplicate = Some(file.try_clone()?);
            replay(&file)?;
            Ok(())
        })();
        assert!(matches!(result, Err(Error::Corrupt(_))));
        let file = open_locked(&dir.db()).expect("relock after failed replay");
        drop(duplicate);
        assert!(matches!(open_locked(&dir.db()), Err(Error::Locked)));
        drop(file);
        assert!(open_locked(&dir.db()).is_ok());
    }

    #[test]
    fn values_round_trip_across_reopen() {
        let dir = TempDir::new("roundtrip");
        {
            let store = Store::open(dir.db()).expect("open");
            put(&store, "articles", b"a1", b"one");
            put(&store, "articles", b"a2", b"two");
            put(&store, "feeds", b"f1", b"feed");
            assert_eq!(get(&store, "articles", b"a1").as_deref(), Some(&b"one"[..]));
        }
        let store = Store::open(dir.db()).expect("reopen");
        assert_eq!(get(&store, "articles", b"a1").as_deref(), Some(&b"one"[..]));
        assert_eq!(get(&store, "articles", b"a2").as_deref(), Some(&b"two"[..]));
        assert_eq!(get(&store, "feeds", b"f1").as_deref(), Some(&b"feed"[..]));
        assert!(store.read().table("missing").is_none());
    }

    #[test]
    fn overwrite_and_remove_survive_reopen() {
        let dir = TempDir::new("overwrite");
        {
            let store = Store::open(dir.db()).expect("open");
            put(&store, "t", b"k", b"first");
            put(&store, "t", b"k", b"second");
            put(&store, "t", b"gone", b"value");
            let mut txn = store.write();
            txn.remove("t", b"gone");
            txn.commit().expect("commit");
        }
        let store = Store::open(dir.db()).expect("reopen");
        assert_eq!(get(&store, "t", b"k").as_deref(), Some(&b"second"[..]));
        assert_eq!(get(&store, "t", b"gone"), None);
        let txn = store.read();
        assert_eq!(txn.table("t").map(|t| t.len()), Some(1));
    }

    #[test]
    fn u64_keys_iterate_in_numeric_order() {
        let dir = TempDir::new("u64order");
        let store = Store::open(dir.db()).expect("open");
        let mut txn = store.write();
        for seq in [300u64, 1, 2, 257, 65_536, 10] {
            txn.insert(
                "op_queue",
                &Key::from_u64(seq),
                format!("op{}", seq).as_bytes(),
            );
        }
        txn.commit().expect("commit");

        let txn = store.read();
        let table = txn.table("op_queue").expect("table");
        let keys: Vec<u64> = table.iter().filter_map(|(k, _)| Key::as_u64(k)).collect();
        assert_eq!(keys, vec![1, 2, 10, 257, 300, 65_536]);
        // `for (key, value) in table` works too.
        let pairs: Vec<(u64, Vec<u8>)> = table
            .into_iter()
            .filter_map(|(k, v)| Key::as_u64(k).map(|k| (k, v.to_vec())))
            .collect();
        assert_eq!(pairs.first(), Some(&(1u64, b"op1".to_vec())));
        assert_eq!(table.last_key().and_then(Key::as_u64), Some(65_536));
        assert_eq!(table.len(), 6);
        assert!(!table.is_empty());
    }

    #[test]
    fn write_txn_sees_and_extends_pending_state() {
        let dir = TempDir::new("pending");
        let store = Store::open(dir.db()).expect("open");
        put(&store, "q", &Key::from_u64(1), b"a");
        put(&store, "q", &Key::from_u64(2), b"b");

        let mut txn = store.write();
        assert_eq!(txn.get("q", &Key::from_u64(1)), Some(&b"a"[..]));
        txn.insert("q", &Key::from_u64(3), b"c");
        assert_eq!(txn.get("q", &Key::from_u64(3)), Some(&b"c"[..]));
        assert_eq!(txn.last_key("q").as_deref().and_then(Key::as_u64), Some(3));
        txn.remove("q", &Key::from_u64(3));
        assert_eq!(txn.get("q", &Key::from_u64(3)), None);
        assert_eq!(txn.last_key("q").as_deref().and_then(Key::as_u64), Some(2));
        txn.remove("q", &Key::from_u64(2));
        assert_eq!(txn.last_key("q").as_deref().and_then(Key::as_u64), Some(1));
        assert_eq!(txn.last_key("nope"), None);
        drop(txn);

        // Dropped without commit: nothing changed.
        let txn = store.read();
        let table = txn.table("q").expect("table");
        assert_eq!(table.len(), 2);
        assert_eq!(table.last_key().and_then(Key::as_u64), Some(2));
    }

    #[test]
    fn multi_table_commit_is_atomic_on_disk() {
        let dir = TempDir::new("atomic");
        {
            let store = Store::open(dir.db()).expect("open");
            let mut txn = store.write();
            txn.insert("articles", b"h1", b"body");
            txn.insert("feeds", b"u1", b"meta");
            txn.insert("feed_index", b"u1", b"[h1]");
            txn.commit().expect("commit");
        }
        let store = Store::open(dir.db()).expect("reopen");
        assert_eq!(
            get(&store, "articles", b"h1").as_deref(),
            Some(&b"body"[..])
        );
        assert_eq!(get(&store, "feeds", b"u1").as_deref(), Some(&b"meta"[..]));
        assert_eq!(
            get(&store, "feed_index", b"u1").as_deref(),
            Some(&b"[h1]"[..])
        );
    }

    #[test]
    fn empty_commit_writes_nothing() {
        let dir = TempDir::new("emptycommit");
        let store = Store::open(dir.db()).expect("open");
        store.write().commit().expect("commit");
        assert_eq!(store.log_len(), 0);
        assert_eq!(fs::metadata(store.path()).expect("stat").len(), 0);
        assert!(store.read().table("any").is_none());
    }

    #[test]
    fn empty_keys_and_values_round_trip() {
        let dir = TempDir::new("empties");
        {
            let store = Store::open(dir.db()).expect("open");
            let mut txn = store.write();
            txn.insert("t", b"", b"");
            txn.insert("t", b"k", b"");
            txn.insert("", b"in-empty-table", b"v");
            txn.commit().expect("commit");
        }
        let store = Store::open(dir.db()).expect("reopen");
        assert_eq!(get(&store, "t", b"").as_deref(), Some(&b""[..]));
        assert_eq!(get(&store, "t", b"k").as_deref(), Some(&b""[..]));
        assert_eq!(
            get(&store, "", b"in-empty-table").as_deref(),
            Some(&b"v"[..])
        );
        let txn = store.read();
        let table = txn.table("t").expect("table");
        assert_eq!(table.last_key(), Some(&b"k"[..]));
        assert_eq!(table.iter().next().map(|(k, _)| k), Some(&b""[..]));
    }

    #[test]
    fn large_values_round_trip() {
        let dir = TempDir::new("large");
        let big: Vec<u8> = (0..1024 * 1024).map(|i| (i % 251) as u8).collect();
        {
            let store = Store::open(dir.db()).expect("open");
            put(&store, "emails", b"e1", &big);
            put(&store, "emails", b"e2", b"small");
        }
        let store = Store::open(dir.db()).expect("reopen");
        assert_eq!(get(&store, "emails", b"e1"), Some(big));
        assert_eq!(get(&store, "emails", b"e2").as_deref(), Some(&b"small"[..]));
    }

    #[test]
    fn truncated_tail_record_is_tolerated_and_repaired() {
        let dir = TempDir::new("truncate");
        let good_len = {
            let store = Store::open(dir.db()).expect("open");
            put(&store, "t", b"k1", b"v1");
            put(&store, "t", b"k2", b"v2");
            let len = store.log_len();
            put(&store, "t", b"k3", b"v3");
            len
        };
        let full = fs::metadata(dir.db()).expect("stat").len();
        let file = OpenOptions::new().write(true).open(dir.db()).expect("open");
        file.set_len(full - 3).expect("truncate");
        drop(file);

        let store = Store::open(dir.db()).expect("reopen");
        assert_eq!(get(&store, "t", b"k1").as_deref(), Some(&b"v1"[..]));
        assert_eq!(get(&store, "t", b"k2").as_deref(), Some(&b"v2"[..]));
        assert_eq!(get(&store, "t", b"k3"), None);
        assert_eq!(fs::metadata(dir.db()).expect("stat").len(), good_len);
        // The repaired store still accepts writes.
        put(&store, "t", b"k4", b"v4");
        drop(store);
        let store = Store::open(dir.db()).expect("reopen again");
        assert_eq!(get(&store, "t", b"k4").as_deref(), Some(&b"v4"[..]));
    }

    #[test]
    fn partial_header_tail_is_repaired() {
        let dir = TempDir::new("partialhdr");
        let good_len = {
            let store = Store::open(dir.db()).expect("open");
            put(&store, "t", b"k1", b"v1");
            store.log_len()
        };
        let mut file = OpenOptions::new()
            .append(true)
            .open(dir.db())
            .expect("open");
        file.write_all(&RECORD_MAGIC[..5]).expect("write");
        drop(file);

        let store = Store::open(dir.db()).expect("reopen");
        assert_eq!(get(&store, "t", b"k1").as_deref(), Some(&b"v1"[..]));
        assert_eq!(fs::metadata(dir.db()).expect("stat").len(), good_len);
    }

    #[test]
    fn torn_final_record_is_repaired() {
        let dir = TempDir::new("tornlast");
        let good_len = {
            let store = Store::open(dir.db()).expect("open");
            put(&store, "t", b"k1", b"v1");
            let len = store.log_len();
            put(&store, "t", b"k2", b"v2");
            len
        };
        let mut bytes = fs::read(dir.db()).expect("read");
        let last = bytes.len() - 1;
        bytes[last] ^= 0xFF;
        fs::write(dir.db(), &bytes).expect("write");

        let store = Store::open(dir.db()).expect("reopen");
        assert_eq!(get(&store, "t", b"k1").as_deref(), Some(&b"v1"[..]));
        assert_eq!(get(&store, "t", b"k2"), None);
        assert_eq!(fs::metadata(dir.db()).expect("stat").len(), good_len);
    }

    #[test]
    fn corrupt_crc_mid_file_is_reported() {
        let dir = TempDir::new("corruptcrc");
        {
            let store = Store::open(dir.db()).expect("open");
            put(&store, "t", b"k1", b"v1");
            put(&store, "t", b"k2", b"v2");
            put(&store, "t", b"k3", b"v3");
        }
        let mut bytes = fs::read(dir.db()).expect("read");
        bytes[HEADER_LEN + 4] ^= 0xFF; // first record's payload
        fs::write(dir.db(), &bytes).expect("write");

        match Store::open(dir.db()) {
            Err(Error::Corrupt(msg)) => assert!(msg.contains("crc mismatch"), "{}", msg),
            other => panic!("expected Corrupt, got {:?}", other.map(|_| "Ok")),
        }
    }

    #[test]
    fn corrupt_magic_is_reported() {
        let dir = TempDir::new("corruptmagic");
        {
            let store = Store::open(dir.db()).expect("open");
            put(&store, "t", b"k1", b"v1");
        }
        let mut bytes = fs::read(dir.db()).expect("read");
        bytes[0] = b'X';
        fs::write(dir.db(), &bytes).expect("write");

        match Store::open(dir.db()) {
            Err(Error::Corrupt(msg)) => assert!(msg.contains("magic"), "{}", msg),
            other => panic!("expected Corrupt, got {:?}", other.map(|_| "Ok")),
        }
    }

    #[test]
    fn lock_refuses_a_second_open_until_dropped() {
        let dir = TempDir::new("lock");
        let first = Store::open(dir.db()).expect("open");
        put(&first, "t", b"k", b"v");
        match Store::open(dir.db()) {
            Err(Error::Locked) => {}
            other => panic!("expected Locked, got {:?}", other.map(|_| "Ok")),
        }
        drop(first);
        let second = Store::open(dir.db()).expect("open after drop");
        assert_eq!(get(&second, "t", b"k").as_deref(), Some(&b"v"[..]));
    }

    /// A handle opened before a compaction names the inode the rename
    /// took out of the path: locking it later would own nothing. `open`
    /// checks this after every lock and opens again.
    #[test]
    fn a_handle_from_before_compaction_is_not_the_store() {
        let dir = TempDir::new("inode");
        let store = Store::open(dir.db()).expect("open");
        put(&store, "t", b"k", b"v");
        let early = File::open(dir.db()).expect("open the log directly");
        assert!(names_this_inode(&early, &dir.db()).expect("stat"));
        store.compact().expect("compact");
        assert!(
            !names_this_inode(&early, &dir.db()).expect("stat"),
            "compaction left the path on the inode it rewrote"
        );
        // The store's own handle moved with the rename, and still holds
        // the lock on what the path names.
        match Store::open(dir.db()) {
            Err(Error::Locked) => {}
            other => panic!("expected Locked, got {:?}", other.map(|_| "Ok")),
        }
        drop(early);
        drop(store);
        let reopened = Store::open(dir.db()).expect("reopen");
        assert_eq!(get(&reopened, "t", b"k").as_deref(), Some(&b"v"[..]));
    }

    #[test]
    fn explicit_compaction_preserves_content_and_shrinks_log() {
        let dir = TempDir::new("compact");
        let store = Store::open(dir.db()).expect("open");
        for round in 0..64u64 {
            let mut txn = store.write();
            txn.insert("t", b"hot", &vec![b'x'; 4096]);
            txn.insert("t", &Key::from_u64(round), b"cold");
            txn.commit().expect("commit");
        }
        let before = store.log_len();
        store.compact().expect("compact");
        let after = store.log_len();
        assert!(after < before, "{} !< {}", after, before);
        assert_eq!(fs::metadata(dir.db()).expect("stat").len(), after);
        assert!(!tmp_path_for(&dir.db()).exists());

        let check = |store: &Store, len: usize| {
            let txn = store.read();
            let table = txn.table("t").expect("table");
            assert_eq!(table.len(), len);
            assert_eq!(table.get(b"hot").map(<[u8]>::len), Some(4096));
            assert_eq!(table.get(&Key::from_u64(63)), Some(&b"cold"[..]));
        };
        check(&store, 65);
        // Writes still land after a compaction, and the file replays.
        put(&store, "t", b"after", b"ok");
        drop(store);
        let store = Store::open(dir.db()).expect("reopen");
        check(&store, 66);
        assert_eq!(get(&store, "t", b"after").as_deref(), Some(&b"ok"[..]));
    }

    #[test]
    fn commits_auto_compact_when_the_log_outgrows_live_state() {
        let dir = TempDir::new("autocompact");
        let store = Store::open(dir.db()).expect("open");
        let value = vec![b'y'; 16 * 1024];
        for _ in 0..128 {
            put(&store, "t", b"same", &value);
        }
        let len = store.log_len();
        assert!(len < MIN_COMPACT_BYTES, "log not compacted: {}", len);
        assert_eq!(get(&store, "t", b"same"), Some(value.clone()));
        drop(store);
        let store = Store::open(dir.db()).expect("reopen");
        assert_eq!(get(&store, "t", b"same"), Some(value));
        assert_eq!(store.read().table("t").map(|t| t.len()), Some(1));
    }

    #[test]
    fn stale_compaction_temp_file_is_removed_on_open() {
        let dir = TempDir::new("staletmp");
        {
            let store = Store::open(dir.db()).expect("open");
            put(&store, "t", b"k", b"v");
        }
        let tmp = tmp_path_for(&dir.db());
        fs::write(&tmp, b"leftover").expect("write tmp");
        let store = Store::open(dir.db()).expect("reopen");
        assert!(!tmp.exists());
        assert_eq!(get(&store, "t", b"k").as_deref(), Some(&b"v"[..]));
    }

    #[test]
    fn many_small_commits_replay_intact() {
        let dir = TempDir::new("torture");
        const COMMITS: u64 = 2000;
        {
            let store = Store::open(dir.db()).expect("open");
            for i in 0..COMMITS {
                let mut txn = store.write();
                txn.insert(
                    "emails",
                    &Key::from_u64(i),
                    format!("body-{}", i).as_bytes(),
                );
                txn.insert(
                    "index",
                    &Key::from_str(&format!("m{}", i)),
                    &Key::from_u64(i),
                );
                if i > 0 && i % 3 == 0 {
                    txn.remove("emails", &Key::from_u64(i - 1));
                }
                txn.commit().expect("commit");
            }
        }
        let store = Store::open(dir.db()).expect("reopen");
        let txn = store.read();
        let emails = txn.table("emails").expect("emails");
        let index = txn.table("index").expect("index");
        assert_eq!(index.len(), COMMITS as usize);
        let expected: usize = (0..COMMITS)
            .filter(|i| !(*i + 1 < COMMITS && (*i + 1) % 3 == 0))
            .count();
        assert_eq!(emails.len(), expected);
        assert_eq!(
            emails.get(&Key::from_u64(COMMITS - 1)).map(<[u8]>::to_vec),
            Some(format!("body-{}", COMMITS - 1).into_bytes())
        );
        assert_eq!(emails.last_key().and_then(Key::as_u64), Some(COMMITS - 1));
        let mut previous: Option<u64> = None;
        for (key, _) in emails.iter() {
            let key = Key::as_u64(key).expect("u64 key");
            assert!(previous.map(|p| p < key).unwrap_or(true));
            previous = Some(key);
        }
    }

    #[test]
    fn commits_reuse_the_live_tables_when_no_reader_holds_them() {
        let dir = TempDir::new("inplace");
        let store = Store::open(dir.db()).expect("open");
        for i in 0..8u64 {
            put(&store, "emails", &Key::from_u64(i), b"body");
        }
        assert_eq!(store.snapshot_clones(), 0, "commit copied the whole store");

        let held = store.read();
        put(&store, "emails", b"x", b"body");
        assert_eq!(store.snapshot_clones(), 1, "held snapshot must be copied");
        drop(held);
        put(&store, "emails", b"y", b"body");
        assert_eq!(store.snapshot_clones(), 1);
        assert_eq!(store.read().table("emails").map(|t| t.len()), Some(10));
    }

    #[test]
    fn readers_see_a_stable_snapshot_across_commits() {
        let dir = TempDir::new("snapshot");
        let store = Arc::new(Store::open(dir.db()).expect("open"));
        {
            let mut txn = store.write();
            txn.insert("a", b"k", b"0");
            txn.insert("b", b"k", b"0");
            txn.commit().expect("commit");
        }

        let held = store.read();
        let stop = Arc::new(AtomicBool::new(false));
        let readers: Vec<_> = (0..4)
            .map(|_| {
                let store = Arc::clone(&store);
                let stop = Arc::clone(&stop);
                thread::spawn(move || {
                    let mut seen = 0usize;
                    while !stop.load(AtomicOrdering::SeqCst) {
                        let txn = store.read();
                        let a = txn.table("a").and_then(|t| t.get(b"k").map(<[u8]>::to_vec));
                        let b = txn.table("b").and_then(|t| t.get(b"k").map(<[u8]>::to_vec));
                        assert_eq!(a, b, "torn multi-table snapshot");
                        seen += 1;
                    }
                    seen
                })
            })
            .collect();

        for round in 1..=200u32 {
            let mut txn = store.write();
            let value = round.to_string();
            txn.insert("a", b"k", value.as_bytes());
            txn.insert("b", b"k", value.as_bytes());
            txn.commit().expect("commit");
        }
        stop.store(true, AtomicOrdering::SeqCst);
        for reader in readers {
            assert!(reader.join().expect("reader thread") > 0);
        }

        // The snapshot taken before the writes still reads its own version.
        assert_eq!(held.table("a").and_then(|t| t.get(b"k")), Some(&b"0"[..]));
        assert_eq!(get(&store, "a", b"k").as_deref(), Some(&b"200"[..]));
    }
}

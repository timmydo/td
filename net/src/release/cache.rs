//! A bounded, disposable object cache, serialized by the caller's cache lease.
use super::*;
use std::collections::{BTreeMap, BTreeSet};

pub(super) const DEFAULT_LIMIT: u64 = 2 * 1024 * 1024 * 1024;
const ENTRY_LIMIT: usize = 200_000;

pub(super) struct Cache {
    path: PathBuf,
    limit: u64,
    bytes: u64,
    entries: BTreeMap<String, (SystemTime, u64)>,
    oldest: BTreeSet<(SystemTime, String)>,
}

impl Cache {
    pub(super) fn open(path: &Path, limit: u64, deadline: Instant) -> Result<Self> {
        let mut cache = Self {
            path: path.to_path_buf(),
            limit,
            bytes: 0,
            entries: BTreeMap::new(),
            oldest: BTreeSet::new(),
        };
        for (count, entry) in io(fs::read_dir(path))?.enumerate() {
            if count >= 1_000_000 || Instant::now() >= deadline {
                return Err("cache scan exceeded its entry or time bound".into());
            }
            let entry = io(entry)?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| "non-UTF-8 cache entry")?;
            if name == "lock" {
                continue;
            }
            let metadata = io(fs::symlink_metadata(entry.path()))?;
            if temporary(&name) && (metadata.is_file() || metadata.is_symlink()) {
                io(fs::remove_file(entry.path()))?;
                continue;
            }
            format::hex(&name, 32)
                .map_err(|_| "unrecognized cache entry; use a dedicated object cache")?;
            if metadata.is_symlink()
                || (metadata.is_file() && metadata.len() > td_engine::image_chunks::MAX as u64)
            {
                io(fs::remove_file(entry.path()))?;
                continue;
            }
            if !metadata.is_file() {
                return Err("object cache contains a non-file entry".into());
            }
            let time = io(metadata.modified())?;
            cache.entries.insert(name.clone(), (time, metadata.len()));
            cache.oldest.insert((time, name));
            cache.bytes += metadata.len();
            cache.trim(0, false)?;
        }
        io(File::open(path).and_then(|file| file.sync_all()))?;
        Ok(cache)
    }

    fn remove(&mut self, digest: &str) -> Result<()> {
        match fs::remove_file(self.path.join(digest)) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.to_string()),
        }
        if let Some((time, size)) = self.entries.remove(digest) {
            self.oldest.remove(&(time, digest.to_string()));
            self.bytes = self
                .bytes
                .checked_sub(size)
                .ok_or("cache accounting underflow")?;
        }
        Ok(())
    }

    fn trim(&mut self, incoming: u64, new_entry: bool) -> Result<()> {
        while self.bytes > self.limit.saturating_sub(incoming)
            || self.entries.len() + usize::from(new_entry) > ENTRY_LIMIT
        {
            let (_, digest) = self
                .oldest
                .first()
                .cloned()
                .ok_or("cache accounting mismatch")?;
            self.remove(&digest)?;
        }
        Ok(())
    }

    pub(super) fn get(&mut self, digest: &str, size: u64) -> Result<Option<Vec<u8>>> {
        if !self.entries.contains_key(digest) {
            return Ok(None);
        }
        let path = self.path.join(digest);
        let bytes = read(&path, size)
            .ok()
            .filter(|bytes| bytes.len() as u64 == size && hash(bytes) == digest);
        let Some(bytes) = bytes else {
            self.remove(digest)?;
            return Ok(None);
        };
        let (file, _) = io(td_fs::open_real_file(&path, "cached object"))?;
        let time = SystemTime::now();
        io(file.set_times(fs::FileTimes::new().set_modified(time)))?;
        if let Some((previous, previous_size)) =
            self.entries.insert(digest.to_string(), (time, size))
        {
            self.oldest.remove(&(previous, digest.to_string()));
            self.bytes = self
                .bytes
                .checked_sub(previous_size)
                .and_then(|n| n.checked_add(size))
                .ok_or("cache accounting overflow")?;
        }
        self.oldest.insert((time, digest.to_string()));
        Ok(Some(bytes))
    }

    pub(super) fn put(&mut self, digest: &str, bytes: &[u8]) -> Result<()> {
        if bytes.len() > td_engine::image_chunks::MAX || hash(bytes) != digest {
            return Err("invalid object supplied to cache".into());
        }
        let size = bytes.len() as u64;
        if self.limit == 0 || size > self.limit {
            return Ok(());
        }
        if self.entries.contains_key(digest) {
            self.remove(digest)?;
        }
        self.trim(size, true)?;
        replace(&self.path.join(digest), bytes)?;
        let time = io(fs::metadata(self.path.join(digest)).and_then(|m| m.modified()))?;
        self.entries.insert(digest.to_string(), (time, size));
        self.oldest.insert((time, digest.to_string()));
        self.bytes += size;
        Ok(())
    }
}

fn temporary(name: &str) -> bool {
    let Some(rest) = name.strip_prefix('.') else {
        return false;
    };
    let mut fields = rest.split('.');
    let (Some(digest), Some(pid), Some(serial), Some("tmp"), None) = (
        fields.next(),
        fields.next(),
        fields.next(),
        fields.next(),
        fields.next(),
    ) else {
        return false;
    };
    format::hex(digest, 32).is_ok() && format::number(pid).is_ok() && format::number(serial).is_ok()
}

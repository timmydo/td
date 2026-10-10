//! Disposable ingress bytes, separately locked and never durable mail authority.
use super::{same_file, LockedRoot, MAX_PATH_BYTES};
use crate::{
    format::row::BlobRow,
    ids::{AccountId, BlobId},
    limits::{ResourcePlan, MAX_MESSAGE_BYTES, SQLITE_BODY_CHUNK_BYTES},
    ports::{self, Clock, Crypto, Deadline, Digest, Tick},
    store_paths::{Name, RootEntry, INGRESS_SLOTS as MAX_SLOTS},
};
use std::{
    fs::{self, File, Metadata, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

/// Full per-slot reservations, including finished files awaiting disposition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SpoolCapacity {
    pub slots: u8,
    pub bytes_each: u64,
    pub total_bytes: u64,
}
impl SpoolCapacity {
    pub fn from_resources(resources: &ResourcePlan) -> Result<Self, ports::Error> {
        let limits = resources.limits();
        let slots = limits
            .smtp_sessions
            .checked_add(limits.https_connections)
            .and_then(|count| u8::try_from(count).ok())
            .ok_or(ports::Error::Capacity)?;
        let bytes_each = u64::try_from(limits.message_bytes).map_err(|_| ports::Error::Capacity)?;
        if slots == 0
            || slots > MAX_SLOTS
            || bytes_each == 0
            || bytes_each > MAX_MESSAGE_BYTES as u64
        {
            return Err(ports::Error::Invalid);
        }
        let total_bytes = bytes_each
            .checked_mul(u64::from(slots))
            .ok_or(ports::Error::Capacity)?;
        Ok(Self {
            slots,
            bytes_each,
            total_bytes,
        })
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SpoolStatus {
    pub occupied_slots: u32,
    pub retired_slots: u32,
    pub reserved_bytes: u64,
}
#[derive(Default)]
struct Slots {
    occupied: AtomicU64,
    retired: AtomicU64,
}

/// One owner for an ingress-only root. Startup discards abandoned temporary files.
///
/// ```compile_fail,E0499
/// use std::sync::Arc;
/// use td_mta::{limits::ResourcePlan, ports::{Clock, Deadline, Error}, store_fs::{IngressSpool, LockedRoot}};
/// fn duplicate(root: &mut LockedRoot, resources: &ResourcePlan,
///              clock: Arc<dyn Clock>, deadline: Deadline) -> Result<(), Error> {
///     let first = IngressSpool::open(root, resources, clock.clone(), deadline)?;
///     let second = IngressSpool::open(root, resources, clock, deadline)?;
///     let _ = (first.capacity(), second.capacity());
///     Ok(())
/// }
/// ```
pub struct IngressSpool<'r> {
    root: &'r LockedRoot,
    clock: Arc<dyn Clock>,
    capacity: SpoolCapacity,
    slots: Slots,
    owner: u32,
}
impl<'r> IngressSpool<'r> {
    /// The separate root must contain only LOCK and canonical slot-00..slot-63.
    /// Validate every artifact before deleting any; cleanup is bounded to 64 files.
    pub fn open(
        root: &'r mut LockedRoot,
        resources: &ResourcePlan,
        clock: Arc<dyn Clock>,
        deadline: Deadline,
    ) -> Result<Self, ports::Error> {
        let capacity = SpoolCapacity::from_resources(resources)?;
        let owner = root.root.directory.metadata()?.uid();
        let mut scope = Scope::new(Arc::clone(&clock), deadline)?;
        let mut present = 0u64;
        let mut entries = 0u8;
        let mut saw_lock = false;
        let directory = &root.root.directory;
        let root_text = std::str::from_utf8(
            directory
                .path
                .get(..directory.length)
                .ok_or(ports::Error::Invalid)?,
        )
        .map_err(|_| ports::Error::Invalid)?;
        for entry in fs::read_dir(root_text)? {
            scope.check()?;
            entries = entries.checked_add(1).ok_or(ports::Error::Capacity)?;
            if entries > MAX_SLOTS + 1 {
                return Err(ports::Error::Capacity);
            }
            let entry = entry?;
            let name = entry.file_name();
            if name == "LOCK" {
                if saw_lock {
                    return Err(ports::Error::Invalid);
                }
                let mut path = [0; MAX_PATH_BYTES];
                let name = Name::root(RootEntry::Lock).map_err(|_| ports::Error::Invalid)?;
                super::check_lock(
                    &fs::symlink_metadata(directory.join(&name, &mut path)?)?,
                    owner,
                )
                .map_err(|_| ports::Error::Invalid)?;
                saw_lock = true;
                continue;
            }
            let slot = parse_slot(&name).ok_or(ports::Error::Invalid)?;
            let mask = bit(slot)?;
            if present & mask != 0 {
                return Err(ports::Error::Invalid);
            }
            let mut path = [0; MAX_PATH_BYTES];
            let name = Name::ingress_slot(slot).map_err(|_| ports::Error::Invalid)?;
            check_cleanup_file(
                &fs::symlink_metadata(directory.join(&name, &mut path)?)?,
                owner,
            )?;
            present |= mask;
        }
        if !saw_lock {
            return Err(ports::Error::Invalid);
        }
        // The complete closed namespace is validated before the first removal.
        for slot in 0..MAX_SLOTS {
            if present & bit(slot)? == 0 {
                continue;
            }
            scope.check()?;
            let mut path = [0; MAX_PATH_BYTES];
            let name = Name::ingress_slot(slot).map_err(|_| ports::Error::Invalid)?;
            fs::remove_file(directory.join(&name, &mut path)?)?;
        }
        scope.check()?;
        Ok(Self {
            root,
            clock,
            capacity,
            slots: Slots::default(),
            owner,
        })
    }
    pub const fn capacity(&self) -> SpoolCapacity {
        self.capacity
    }
    /// A bounded observation; concurrent owners may progress between loads.
    pub fn status(&self) -> Result<SpoolStatus, ports::Error> {
        let occupied = self.slots.occupied.load(Ordering::SeqCst);
        let occupied_slots = occupied.count_ones();
        Ok(SpoolStatus {
            occupied_slots,
            retired_slots: (self.slots.retired.load(Ordering::SeqCst) & occupied).count_ones(),
            reserved_bytes: u64::from(occupied_slots)
                .checked_mul(self.capacity.bytes_each)
                .ok_or(ports::Error::Capacity)?,
        })
    }
    fn reserve(&self) -> Result<u8, ports::Error> {
        let mut occupied = self.slots.occupied.load(Ordering::SeqCst);
        for _ in 0..MAX_SLOTS {
            let slot = (0..self.capacity.slots)
                .find(|slot| bit(*slot).is_ok_and(|mask| occupied & mask == 0))
                .ok_or(ports::Error::Quota)?;
            match self.slots.occupied.compare_exchange(
                occupied,
                occupied | bit(slot)?,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => return Ok(slot),
                Err(actual) => occupied = actual,
            }
        }
        Err(ports::Error::Busy)
    }
    /// Reserve the entire configured maximum before creating a temporary file.
    /// Account and ID are passive caller metadata, never authorization.
    pub fn begin<C: Crypto>(
        &self,
        crypto: &C,
        account: AccountId,
        id: BlobId,
        deadline: Deadline,
    ) -> Result<SpoolWriter<'_, 'r, C::Sha256>, ports::Error> {
        let mut scope = Scope::new(Arc::clone(&self.clock), deadline)?;
        let slot = self.reserve()?;
        let mut owner = Temporary {
            spool: self,
            slot,
            file: None,
            identity: None,
            released: false,
        };
        let name = Name::ingress_slot(slot).map_err(|_| ports::Error::Invalid)?;
        let mut path = [0; MAX_PATH_BYTES];
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(self.root.root.directory.join(&name, &mut path)?)?;
        // Record ownership before another fallible step so cleanup knows our inode.
        owner.identity = Some(file.metadata()?);
        owner.file = Some(file);
        owner
            .file()?
            .set_permissions(fs::Permissions::from_mode(0o600))?;
        check_file(&owner.file()?.metadata()?, self.owner)?;
        let digest = crypto.sha256()?;
        scope.check()?;
        Ok(SpoolWriter {
            owner,
            scope,
            digest: Some(digest),
            account,
            id,
            length: 0,
        })
    }
}
fn bit(slot: u8) -> Result<u64, ports::Error> {
    1u64.checked_shl(u32::from(slot))
        .ok_or(ports::Error::Invalid)
}
fn parse_slot(name: &std::ffi::OsStr) -> Option<u8> {
    use std::os::unix::ffi::OsStrExt;
    let bytes = name.as_bytes();
    if bytes.len() != 7 || bytes.get(..5)? != b"slot-" {
        return None;
    }
    let tens = bytes.get(5)?.checked_sub(b'0')?;
    let ones = bytes.get(6)?.checked_sub(b'0')?;
    if tens > 9 || ones > 9 {
        return None;
    }
    let slot = tens.checked_mul(10)?.checked_add(ones)?;
    (slot < MAX_SLOTS).then_some(slot)
}
fn check_cleanup_file(metadata: &Metadata, owner: u32) -> Result<(), ports::Error> {
    // Creation can die before restoring owner permissions filtered by umask.
    if !metadata.is_file()
        || metadata.uid() != owner
        || metadata.mode() & 0o7777 & !0o600 != 0
        || metadata.nlink() != 1
        || metadata.len() > MAX_MESSAGE_BYTES as u64
    {
        return Err(ports::Error::Invalid);
    }
    Ok(())
}
fn check_file(metadata: &Metadata, owner: u32) -> Result<(), ports::Error> {
    check_cleanup_file(metadata, owner)?;
    if metadata.mode() & 0o7777 != 0o600 {
        return Err(ports::Error::Invalid);
    }
    Ok(())
}

struct Scope {
    clock: Arc<dyn Clock>,
    deadline: Deadline,
    last: Tick,
    failed: Option<ports::Error>,
}
impl Scope {
    fn new(clock: Arc<dyn Clock>, deadline: Deadline) -> Result<Self, ports::Error> {
        let last = clock.sample()?.monotonic;
        let mut scope = Self {
            clock,
            deadline,
            last,
            failed: None,
        };
        scope.check()?;
        Ok(scope)
    }
    fn fail<T>(&mut self, error: ports::Error) -> Result<T, ports::Error> {
        let first = *self.failed.get_or_insert(error);
        Err(first)
    }
    fn check(&mut self) -> Result<(), ports::Error> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        let result = self.clock.sample().and_then(|now| {
            if now.monotonic < self.last {
                return Err(ports::Error::Invalid);
            }
            self.last = now.monotonic;
            if self.deadline.expired(now.monotonic) {
                return Err(ports::Error::Deadline);
            }
            Ok(())
        });
        result.or_else(|error| self.fail(error))
    }
}
struct Temporary<'s, 'r> {
    spool: &'s IngressSpool<'r>,
    slot: u8,
    file: Option<File>,
    identity: Option<Metadata>,
    released: bool,
}
impl Temporary<'_, '_> {
    fn file(&mut self) -> Result<&mut File, ports::Error> {
        self.file.as_mut().ok_or(ports::Error::Invalid)
    }
    fn cleanup(&mut self) -> Result<(), ports::Error> {
        if self.released {
            return Ok(());
        }
        // Unlinked files still occupy storage until the last descriptor closes.
        drop(self.file.take());
        let result = (|| {
            let name = Name::ingress_slot(self.slot).map_err(|_| ports::Error::Invalid)?;
            let mut path = [0; MAX_PATH_BYTES];
            let path = self.spool.root.root.directory.join(&name, &mut path)?;
            let metadata = match fs::symlink_metadata(path) {
                Err(error)
                    if self.identity.is_none() && error.kind() == io::ErrorKind::NotFound =>
                {
                    return Ok(());
                }
                result => result?,
            };
            let identity = self.identity.as_ref().ok_or(ports::Error::Invalid)?;
            if !same_file(identity, &metadata) {
                return Err(ports::Error::Invalid);
            }
            check_cleanup_file(&metadata, self.spool.owner)?;
            fs::remove_file(path)?;
            Ok(())
        })();
        let mask = bit(self.slot)?;
        if result.is_ok() {
            self.spool.slots.occupied.fetch_and(!mask, Ordering::SeqCst);
        } else {
            self.spool.slots.retired.fetch_or(mask, Ordering::SeqCst);
        }
        self.released = true;
        result
    }
}
impl Drop for Temporary<'_, '_> {
    fn drop(&mut self) {
        let _ = self.cleanup();
    }
}

/// Incomplete bytes own their full reservation and cannot be read as prepared mail.
pub struct SpoolWriter<'s, 'r, D: Digest> {
    owner: Temporary<'s, 'r>,
    scope: Scope,
    digest: Option<D>,
    account: AccountId,
    id: BlobId,
    length: u64,
}
impl<D: Digest> SpoolWriter<'_, '_, D> {
    /// All-or-error, at most 64 KiB; any error permanently retires this writer.
    pub fn write(&mut self, bytes: &[u8]) -> Result<(), ports::Error> {
        self.write_with(bytes, |file, bytes| file.write_all(bytes))
    }
    fn write_with(
        &mut self,
        bytes: &[u8],
        write: impl FnOnce(&mut File, &[u8]) -> io::Result<()>,
    ) -> Result<(), ports::Error> {
        self.scope.check()?;
        let next = self
            .length
            .checked_add(bytes.len() as u64)
            .ok_or(ports::Error::Capacity);
        let result = (|| {
            let next = next?;
            if bytes.len() > SQLITE_BODY_CHUNK_BYTES || next > self.owner.spool.capacity.bytes_each
            {
                return Err(ports::Error::Capacity);
            }
            write(self.owner.file()?, bytes)?;
            self.length = next;
            self.digest
                .as_mut()
                .ok_or(ports::Error::Invalid)?
                .update(bytes)?;
            self.scope.check()
        })();
        result.or_else(|error| self.scope.fail(error))
    }
    pub fn discard(mut self) -> Result<(), ports::Error> {
        self.owner.cleanup()
    }
}
impl<'s, 'r, D: Digest> SpoolWriter<'s, 'r, D> {
    /// No fsync or durable publication. Verify exact file length/EOF and rewind.
    pub fn finish(mut self) -> Result<SpoolInput<'s, 'r>, ports::Error> {
        self.scope.check()?;
        let metadata = self.owner.file()?.metadata()?;
        check_file(&metadata, self.owner.spool.owner)?;
        if metadata.len() != self.length
            || !self
                .owner
                .identity
                .as_ref()
                .is_some_and(|identity| same_file(identity, &metadata))
        {
            return Err(ports::Error::Corrupt);
        }
        let mut extra = [0];
        if self.owner.file()?.read(&mut extra)? != 0 {
            return Err(ports::Error::Corrupt);
        }
        self.owner.file()?.seek(SeekFrom::Start(0))?;
        let digest = self.digest.take().ok_or(ports::Error::Invalid)?.finish()?;
        self.scope.check()?;
        Ok(SpoolInput {
            owner: self.owner,
            scope: self.scope,
            account: self.account,
            id: self.id,
            length: self.length,
            digest,
            position: 0,
        })
    }
}

/// Linear prepared file. Its metadata is passive; only SQLite COMMIT is durable.
pub struct SpoolInput<'s, 'r> {
    owner: Temporary<'s, 'r>,
    scope: Scope,
    account: AccountId,
    id: BlobId,
    length: u64,
    digest: [u8; 32],
    position: u64,
}
impl SpoolInput<'_, '_> {
    pub const fn account(&self) -> AccountId {
        self.account
    }
    pub const fn id(&self) -> BlobId {
        self.id
    }
    pub const fn len(&self) -> u64 {
        self.length
    }
    pub const fn is_empty(&self) -> bool {
        self.length == 0
    }
    pub const fn digest(&self) -> &[u8; 32] {
        &self.digest
    }
    /// Preserve the typed terminal error when std::io::Read reports its I/O kind.
    pub const fn failure(&self) -> Option<ports::Error> {
        self.scope.failed
    }
    pub fn row(&self, created_at: i64) -> BlobRow {
        BlobRow {
            length: self.length,
            digest: self.digest,
            created_at,
        }
    }
    pub fn rewind(&mut self) -> Result<(), ports::Error> {
        self.scope.check()?;
        let result = self
            .owner
            .file()
            .and_then(|file| file.seek(SeekFrom::Start(0)).map_err(ports::Error::from));
        if let Err(error) = result {
            return self.scope.fail(error);
        }
        self.position = 0;
        self.scope.check()
    }
    pub fn discard(mut self) -> Result<(), ports::Error> {
        self.owner.cleanup()
    }
    fn read_bounded(&mut self, output: &mut [u8]) -> Result<usize, ports::Error> {
        self.scope.check()?;
        let count = output.len().min(SQLITE_BODY_CHUNK_BYTES);
        let output = output.get_mut(..count).ok_or(ports::Error::Invalid)?;
        let result = (|| {
            let read = self.owner.file()?.read(output)?;
            self.position = self
                .position
                .checked_add(read as u64)
                .ok_or(ports::Error::Corrupt)?;
            if self.position > self.length
                || (read == 0 && !output.is_empty() && self.position != self.length)
            {
                return Err(ports::Error::Corrupt);
            }
            self.scope.check()?;
            Ok(read)
        })();
        result.or_else(|error| self.scope.fail(error))
    }
}
impl Read for SpoolInput<'_, '_> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        self.read_bounded(output).map_err(|error| {
            let kind = match error {
                ports::Error::Deadline => io::ErrorKind::TimedOut,
                ports::Error::Io { kind, .. } => kind,
                _ => io::ErrorKind::InvalidData,
            };
            kind.into()
        })
    }
}

#[cfg(test)]
#[path = "spool/crash_tests.rs"]
mod crash_tests;
#[cfg(test)]
#[path = "spool/tests.rs"]
mod tests;

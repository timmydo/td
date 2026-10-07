//! Offline snapshots retain both cooperative locks through durable publication.
use super::*;
use std::io::{Read, Write};

/// Success certifies a closed, checkpointed snapshot, not semantic verification.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BackupReceipt {
    pub epoch: StoreEpoch,
    pub bytes: u64,
}

/// Failure never grants a completed backup. Partial artifacts require inspection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BackupError {
    /// This invocation did not create the final database name.
    Unpublished(ports::Error),
    /// Publication was attempted; the final name may exist and is not proven durable.
    IncompletePublication(ports::Error),
}
impl std::fmt::Display for BackupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unpublished(error) => write!(f, "backup not published: {error}"),
            Self::IncompletePublication(error) => {
                write!(f, "backup publication incomplete: {error}")
            }
        }
    }
}
impl std::error::Error for BackupError {}
impl From<ports::Error> for BackupError {
    fn from(error: ports::Error) -> Self {
        Self::Unpublished(error)
    }
}
impl From<std::io::Error> for BackupError {
    fn from(error: std::io::Error) -> Self {
        Self::Unpublished(error.into())
    }
}

struct CopyClock<'a> {
    clock: &'a dyn Clock,
    deadline: Deadline,
    last: ports::Tick,
}
impl CopyClock<'_> {
    fn check(&mut self) -> Result<(), ports::Error> {
        let now = self.clock.sample()?.monotonic;
        if now < self.last {
            return Err(ports::Error::Invalid);
        }
        self.last = now;
        if self.deadline.expired(now) {
            return Err(ports::Error::Deadline);
        }
        Ok(())
    }
}

impl IndexStore<'_> {
    /// Consume the engine, checkpoint and close all SQLite connections, then
    /// copy into an empty locked root. The destination is never overwritten.
    ///
    /// Both locks remain held throughout. The caller retains them on return.
    /// Errors can leave `metadata.sqlite3.backup-partial`; publication errors
    /// can also leave the final name. Neither outcome authorizes automatic
    /// deletion or a successful-backup acknowledgement.
    ///
    /// ```compile_fail,E0505
    /// use td_mta::{ids::AccountId, ports::Deadline, store_fs::{IndexStore, LockedRoot}};
    /// fn live(store: IndexStore<'_>, destination: &mut LockedRoot,
    ///         account: AccountId, deadline: Deadline) {
    ///     let view = store.view(account, deadline).unwrap();
    ///     let _ = store.backup(destination, deadline, &mut [0; 65536]);
    ///     drop(view);
    /// }
    /// ```
    pub fn backup(
        self,
        destination: &mut LockedRoot,
        deadline: Deadline,
        scratch: &mut [u8; crate::limits::SQLITE_BODY_CHUNK_BYTES],
    ) -> Result<BackupReceipt, BackupError> {
        let clock = Arc::clone(&self.clock);
        let mut timer = CopyClock {
            clock: clock.as_ref(),
            deadline,
            last: clock.sample()?.monotonic,
        };
        timer.check()?;
        let final_path = db_path(destination, RootEntry::Database)?;
        let partial_path = db_path(destination, RootEntry::BackupPartial)?;
        if optional_metadata(&final_path.with_file_name("FORMAT"))?.is_some() {
            return Err(ports::Error::Conflict.into());
        }
        for entry in [
            RootEntry::Database,
            RootEntry::Wal,
            RootEntry::SharedMemory,
            RootEntry::BackupPartial,
        ] {
            if optional_metadata(&db_path(destination, entry)?)?.is_some() {
                return Err(ports::Error::Conflict.into());
            }
        }
        timer.last = self.checkpoint_after(deadline, Some(timer.last))?;
        timer.check()?;
        let Self {
            root,
            epoch,
            writer,
            readers,
            ..
        } = self;
        // A forgotten view leaves Borrowed behind; checkpoint already refuses it.
        let readers = readers
            .into_inner()
            .map_err(|_| ports::Error::WriterStopped)?;
        for reader in readers {
            match reader {
                ReaderSlot::Available(native) => native.close()?,
                ReaderSlot::Retired => (),
                ReaderSlot::Borrowed => return Err(ports::Error::Busy.into()),
            }
        }
        writer
            .into_inner()
            .map_err(|_| ports::Error::WriterStopped)?
            .native
            .close()?;
        timer.check()?;
        // Only now may a second descriptor touch the DB without affecting
        // SQLite's process-wide POSIX locks on that inode.
        let source_path = db_path(root, RootEntry::Database)?;
        validate_file(root, &source_path, MAX_PAGES * PAGE_BYTES)?;
        let before = fs::symlink_metadata(&source_path)?;
        let mut source = fs::File::open(&source_path)?;
        let metadata = source.metadata()?;
        if !super::super::same_file(&before, &metadata)
            || metadata.len() == 0
            || metadata.len() % PAGE_BYTES != 0
        {
            return Err(ports::Error::Corrupt.into());
        }
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&partial_path)?;
        output.set_permissions(fs::Permissions::from_mode(0o600))?;
        let bytes = metadata.len();
        copy_exact(&mut source, &mut output, bytes, scratch, &mut timer)?;
        output.sync_all()?;
        timer.check()?;
        publish(&partial_path, &final_path, &destination.root.directory.file)?;
        Ok(BackupReceipt { epoch, bytes })
    }
}

fn publish(partial: &Path, final_path: &Path, directory: &fs::File) -> Result<(), BackupError> {
    // Even a failed link can leave uncertain filesystem effects. Never replace.
    fs::hard_link(partial, final_path)
        .map_err(|error| BackupError::IncompletePublication(error.into()))?;
    // Finish publication without a deadline refusal masking completed durability.
    fs::remove_file(partial).map_err(|error| BackupError::IncompletePublication(error.into()))?;
    directory
        .sync_all()
        .map_err(|error| BackupError::IncompletePublication(error.into()))
}

fn copy_exact(
    source: &mut impl Read,
    destination: &mut impl Write,
    bytes: u64,
    scratch: &mut [u8],
    timer: &mut CopyClock<'_>,
) -> Result<(), ports::Error> {
    if bytes > MAX_PAGES * PAGE_BYTES || scratch.is_empty() {
        return Err(ports::Error::Capacity);
    }
    let mut remaining = bytes;
    while remaining != 0 {
        timer.check()?;
        let count = usize::try_from(remaining.min(scratch.len() as u64))
            .map_err(|_| ports::Error::Capacity)?;
        let chunk = scratch.get_mut(..count).ok_or(ports::Error::Invalid)?;
        let read = source.read(chunk)?;
        if read == 0 || read > count {
            return Err(ports::Error::Corrupt);
        }
        timer.check()?;
        destination.write_all(chunk.get(..read).ok_or(ports::Error::Invalid)?)?;
        remaining = remaining
            .checked_sub(read as u64)
            .ok_or(ports::Error::Corrupt)?;
    }
    timer.check()?;
    if source.read(scratch.get_mut(..1).ok_or(ports::Error::Invalid)?)? != 0 {
        return Err(ports::Error::Corrupt);
    }
    timer.check()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::ports::{Tick, Time};
    use crate::store_fs::tests::Fixture;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct Timer(AtomicU64);
    impl Clock for Timer {
        fn sample(&self) -> Result<Time, ports::Error> {
            Ok(Time {
                utc_ms: 0,
                monotonic: Tick(self.0.load(Ordering::Relaxed)),
            })
        }
    }
    fn deadline() -> Deadline {
        Deadline::after(Tick(0), 100).unwrap()
    }
    fn clock() -> Arc<Timer> {
        Arc::new(Timer(AtomicU64::new(1)))
    }
    #[test]
    fn existing_destination_artifacts_are_never_overwritten() {
        for entry in [
            "metadata.sqlite3",
            "metadata.sqlite3-wal",
            "metadata.sqlite3-shm",
            "metadata.sqlite3.backup-partial",
            "FORMAT",
        ] {
            let source = Fixture::new();
            let destination = Fixture::new();
            let mut source_root = source.locked();
            let mut destination_root = destination.locked();
            let store = IndexStore::create(
                &mut source_root,
                StoreEpoch::from_bytes([9; 16]),
                clock(),
                1,
                deadline(),
            )
            .unwrap();
            let path = destination.path.join(entry);
            fs::write(&path, b"retain this").unwrap();
            assert_eq!(
                store.backup(&mut destination_root, deadline(), &mut [0; 65536]),
                Err(BackupError::Unpublished(ports::Error::Conflict))
            );
            assert_eq!(fs::read(path).unwrap(), b"retain this");
        }
    }
    #[test]
    fn expired_stopped_or_forgotten_view_refuses_before_destination_creation() {
        for reason in 0..3 {
            let source = Fixture::new();
            let destination = Fixture::new();
            let mut source_root = source.locked();
            let mut destination_root = destination.locked();
            let clock = clock();
            let store = IndexStore::create(
                &mut source_root,
                StoreEpoch::from_bytes([9; 16]),
                clock.clone(),
                1,
                deadline(),
            )
            .unwrap();
            let expected = match reason {
                0 => {
                    clock.0.store(100, Ordering::Relaxed);
                    ports::Error::Deadline
                }
                1 => {
                    lock(&store.writer).unwrap().stopped = true;
                    ports::Error::WriterStopped
                }
                _ => {
                    // Model the retained marker without actually leaking a connection.
                    *lock(&store.readers).unwrap().get_mut(0).unwrap() = ReaderSlot::Borrowed;
                    ports::Error::Busy
                }
            };
            assert_eq!(
                store.backup(&mut destination_root, deadline(), &mut [0; 65536]),
                Err(BackupError::Unpublished(expected))
            );
            assert!(!destination.path.join("metadata.sqlite3").exists());
            assert!(!destination
                .path
                .join("metadata.sqlite3.backup-partial")
                .exists());
        }
    }
    #[test]
    fn checkpoint_scope_preserves_predecessor_and_returns_last_observation() {
        let source = Fixture::new();
        let mut root = source.locked();
        let store = IndexStore::create(
            &mut root,
            StoreEpoch::from_bytes([9; 16]),
            clock(),
            1,
            deadline(),
        )
        .unwrap();
        assert_eq!(
            store.checkpoint_after(deadline(), Some(Tick(2))),
            Err(ports::Error::Invalid)
        );
        assert_eq!(
            store.checkpoint_after(deadline(), Some(Tick(1))).unwrap(),
            Tick(1)
        );
    }
    struct ScriptedClock(Mutex<(std::collections::VecDeque<Tick>, Tick)>);
    impl Clock for ScriptedClock {
        fn sample(&self) -> Result<Time, ports::Error> {
            let mut state = self.0.lock().unwrap();
            if let Some(next) = state.0.pop_front() {
                state.1 = next;
            }
            Ok(Time {
                utc_ms: 0,
                monotonic: state.1,
            })
        }
    }
    #[test]
    fn reversal_between_writer_acquisition_and_native_scope_refuses_backup() {
        let source = Fixture::new();
        let destination = Fixture::new();
        let mut source_root = source.locked();
        let mut destination_root = destination.locked();
        let clock = Arc::new(ScriptedClock(Mutex::new((
            std::collections::VecDeque::new(),
            Tick(1),
        ))));
        let store = IndexStore::create(
            &mut source_root,
            StoreEpoch::from_bytes([9; 16]),
            clock.clone(),
            1,
            deadline(),
        )
        .unwrap();
        // Backup's two entry observations, writer acquisition, then begin_work.
        clock.0.lock().unwrap().0 = [Tick(1), Tick(1), Tick(9), Tick(8)].into();
        assert_eq!(
            store.backup(&mut destination_root, deadline(), &mut [0; 65536]),
            Err(BackupError::Unpublished(ports::Error::Invalid))
        );
        assert!(clock.0.lock().unwrap().0.is_empty());
        assert!(!destination.path.join("metadata.sqlite3").exists());
        assert!(!destination
            .path
            .join("metadata.sqlite3.backup-partial")
            .exists());
    }
    #[test]
    fn failed_link_is_incomplete_publication_and_preserves_existing_names() {
        let destination = Fixture::new();
        let directory = fs::File::open(&destination.path).unwrap();
        let partial = destination.path.join("metadata.sqlite3.backup-partial");
        let final_path = destination.path.join("metadata.sqlite3");
        fs::write(&partial, b"new snapshot").unwrap();
        fs::write(&final_path, b"existing snapshot").unwrap();
        assert!(matches!(
            publish(&partial, &final_path, &directory),
            Err(BackupError::IncompletePublication(ports::Error::Io {
                kind: std::io::ErrorKind::AlreadyExists,
                ..
            }))
        ));
        assert_eq!(fs::read(&partial).unwrap(), b"new snapshot");
        assert_eq!(fs::read(&final_path).unwrap(), b"existing snapshot");
        fs::remove_file(&partial).unwrap();
        fs::remove_file(&final_path).unwrap();
        assert!(matches!(
            publish(&partial, &final_path, &directory),
            Err(BackupError::IncompletePublication(ports::Error::Io {
                kind: std::io::ErrorKind::NotFound,
                ..
            }))
        ));
        assert!(!final_path.exists());
    }
    #[test]
    fn backup_closes_available_readers_after_snapshot_retirement() {
        let source = Fixture::new();
        let destination = Fixture::new();
        let mut source_root = source.locked();
        let mut destination_root = destination.locked();
        let account = AccountId::from_bytes([2; 16]);
        let store = IndexStore::create(
            &mut source_root,
            StoreEpoch::from_bytes([9; 16]),
            clock(),
            3,
            deadline(),
        )
        .unwrap();
        store.create_account(account, deadline()).unwrap();
        let mut view = store.view(account, deadline()).unwrap();
        lock(&view.native().unwrap().connection)
            .unwrap()
            .execute_batch("ROLLBACK")
            .unwrap();
        assert_eq!(
            view.get(Key::Blob(BlobId::from_bytes([3; 16])), &mut [0; 128]),
            Err(ports::Error::Corrupt)
        );
        drop(view);
        {
            let readers = lock(&store.readers).unwrap();
            assert_eq!(
                readers
                    .iter()
                    .filter(|slot| matches!(slot, ReaderSlot::Available(_)))
                    .count(),
                2
            );
            assert_eq!(
                readers
                    .iter()
                    .filter(|slot| matches!(slot, ReaderSlot::Retired))
                    .count(),
                1
            );
        }
        store
            .backup(&mut destination_root, deadline(), &mut [0; 65536])
            .unwrap();
        let snapshot = IndexStore::open(&mut destination_root, clock(), 1, deadline()).unwrap();
        assert_eq!(
            snapshot
                .view(account, deadline())
                .unwrap()
                .identity()
                .committed_sequence,
            Sequence::default()
        );
    }
    struct FailingWriter;
    impl Write for FailingWriter {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::ErrorKind::WriteZero.into())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    #[test]
    fn copy_refuses_short_extra_oversized_and_failed_output() {
        let clock = clock();
        let mut timer = CopyClock {
            clock: clock.as_ref(),
            deadline: deadline(),
            last: Tick(1),
        };
        let mut scratch = [0; 4];
        let mut output = Vec::new();
        assert_eq!(
            copy_exact(&mut &b"abc"[..], &mut output, 4, &mut scratch, &mut timer),
            Err(ports::Error::Corrupt)
        );
        assert_eq!(
            copy_exact(&mut &b"abcde"[..], &mut output, 4, &mut scratch, &mut timer),
            Err(ports::Error::Corrupt)
        );
        let before = output.len();
        assert_eq!(
            copy_exact(
                &mut &b""[..],
                &mut output,
                MAX_PAGES * PAGE_BYTES + 1,
                &mut scratch,
                &mut timer
            ),
            Err(ports::Error::Capacity)
        );
        assert_eq!(output.len(), before);
        assert!(matches!(
            copy_exact(
                &mut &b"abc"[..],
                &mut FailingWriter,
                3,
                &mut scratch,
                &mut timer
            ),
            Err(ports::Error::Io {
                kind: std::io::ErrorKind::WriteZero,
                ..
            })
        ));
    }
    struct Advancing<'a> {
        clock: &'a Timer,
        value: u64,
    }
    impl Read for Advancing<'_> {
        fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
            self.clock.0.store(self.value, Ordering::Relaxed);
            out.fill(42);
            Ok(out.len())
        }
    }
    #[test]
    fn original_deadline_and_clock_reversal_refuse_before_output() {
        for (value, expected) in [(100, ports::Error::Deadline), (0, ports::Error::Invalid)] {
            let clock = clock();
            let mut timer = CopyClock {
                clock: clock.as_ref(),
                deadline: deadline(),
                last: Tick(1),
            };
            let mut source = Advancing {
                clock: clock.as_ref(),
                value,
            };
            let mut output = Vec::new();
            assert_eq!(
                copy_exact(&mut source, &mut output, 4, &mut [0; 4], &mut timer),
                Err(expected)
            );
            assert!(output.is_empty());
        }
    }
}

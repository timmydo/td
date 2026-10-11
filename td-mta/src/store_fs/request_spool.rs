//! Private disposable response files. Byte quota is owned by StoreCoordinator.
use super::{LockedRoot, MAX_PATH_BYTES};
use crate::{
    ports,
    store_paths::{Name, REQUEST_SLOTS as SLOTS},
};
use std::{
    fs::{self, DirBuilder, File, Metadata, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    sync::atomic::{AtomicU32, Ordering},
};
const MAX_BYTES: u64 = 1 << 30;
const _: () = assert!(SLOTS > 0 && SLOTS <= u32::BITS as u8);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RequestSpoolStatus {
    pub occupied_slots: u32,
    pub retired_slots: u32,
}
/// Exclusively borrowed separate tmp/requests root; never a database directory.
/// Opening is a cold, quiescent operation under its retained LOCK.
///
/// ```compile_fail,E0499
/// use std::sync::Arc;
/// use td_mta::{ports::{Clock, Deadline, Error}, store_fs::{RequestSpool, LockedRoot}};
/// fn duplicate(root: &mut LockedRoot, clock: Arc<dyn Clock>, deadline: Deadline) -> Result<(), Error> {
///     let first = RequestSpool::open(root, clock.clone(), deadline)?;
///     let second = RequestSpool::open(root, clock, deadline)?;
///     let _ = (first.status(), second.status());
///     Ok(())
/// }
/// ```
pub struct RequestSpool<'r> {
    root: &'r LockedRoot,
    owner: u32,
    occupied: AtomicU32,
    retired: AtomicU32,
}
impl<'r> RequestSpool<'r> {
    /// Validate the entire closed namespace before discarding abandoned responses.
    /// Only LOCK and request-00..request-31 directories containing response exist.
    pub fn open(
        root: &'r mut LockedRoot,
        clock: std::sync::Arc<dyn ports::Clock>,
        deadline: ports::Deadline,
    ) -> Result<Self, ports::Error> {
        let mut scope = super::spool::Scope::new(clock, deadline)?;
        let owner = root.root.directory.metadata()?.uid();
        let spool = Self {
            root,
            owner,
            occupied: AtomicU32::new(0),
            retired: AtomicU32::new(0),
        };
        let directory = &spool.root.root.directory;
        let text = std::str::from_utf8(
            directory
                .path
                .get(..directory.length)
                .ok_or(ports::Error::Invalid)?,
        )
        .map_err(|_| ports::Error::Invalid)?;
        let mut directories = 0u32;
        let mut files = 0u32;
        let mut entries = 0u8;
        let mut saw_lock = false;
        for entry in fs::read_dir(text)? {
            scope.check()?;
            entries = entries.checked_add(1).ok_or(ports::Error::Capacity)?;
            if entries > SLOTS + 1 {
                return Err(ports::Error::Capacity);
            }
            let entry = entry?;
            let name = entry.file_name();
            if name == "LOCK" {
                if saw_lock {
                    return Err(ports::Error::Invalid);
                }
                super::check_lock(&fs::symlink_metadata(entry.path())?, owner)
                    .map_err(|_| ports::Error::Invalid)?;
                saw_lock = true;
                continue;
            }
            let slot = (0..SLOTS)
                .find(|slot| {
                    Name::request_slot(*slot, false)
                        .and_then(|candidate| Ok(candidate.as_path()?.as_os_str() == name))
                        .unwrap_or(false)
                })
                .ok_or(ports::Error::Invalid)?;
            let bit = mask(slot)?;
            if directories & bit != 0 {
                return Err(ports::Error::Invalid);
            }
            directories |= bit;
            let metadata = fs::symlink_metadata(entry.path())?;
            if !metadata.is_dir()
                || metadata.uid() != owner
                || metadata.mode() & 0o7777 & !0o700 != 0
            {
                return Err(ports::Error::Invalid);
            }
            // Repair only owner permission bits filtered by a creation-time umask.
            if metadata.mode() & 0o500 != 0o500 {
                fs::set_permissions(
                    entry.path(),
                    fs::Permissions::from_mode((metadata.mode() & 0o700) | 0o500),
                )?;
            }
            for (count, child) in fs::read_dir(entry.path())?.enumerate() {
                if count != 0 {
                    return Err(ports::Error::Invalid);
                }
                let child = child?;
                if child.file_name() != "response" {
                    return Err(ports::Error::Invalid);
                }
                check_file(&fs::symlink_metadata(child.path())?, owner)?;
                files |= bit;
            }
        }
        if !saw_lock {
            return Err(ports::Error::Invalid);
        }
        for slot in 0..SLOTS {
            scope.check()?;
            let bit = mask(slot)?;
            let mut path = [0; MAX_PATH_BYTES];
            if directories & bit != 0 {
                fs::set_permissions(
                    spool.path(slot, false, &mut path)?,
                    fs::Permissions::from_mode(0o700),
                )?;
            }
            if files & bit != 0 {
                fs::remove_file(spool.path(slot, true, &mut path)?)?;
            }
            if directories & bit == 0 {
                let path = spool.path(slot, false, &mut path)?;
                DirBuilder::new().mode(0o700).create(path)?;
                fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
            }
        }
        scope.check()?;
        Ok(spool)
    }
    /// Concurrent observations can change between loads; byte usage belongs to
    /// the coordinator's ResponseBytes ledger, not this slot registry.
    pub fn status(&self) -> RequestSpoolStatus {
        let occupied = self.occupied.load(Ordering::SeqCst);
        RequestSpoolStatus {
            occupied_slots: occupied.count_ones(),
            retired_slots: (self.retired.load(Ordering::SeqCst) & occupied).count_ones(),
        }
    }
    pub(crate) fn reserve(&self) -> Result<Temporary<'_, 'r>, ports::Error> {
        let mut occupied = self.occupied.load(Ordering::SeqCst);
        // Contention may refuse Busy despite spare slots; retries stay bounded.
        for _ in 0..SLOTS {
            let slot = (0..SLOTS)
                .find(|slot| mask(*slot).is_ok_and(|bit| occupied & bit == 0))
                .ok_or(ports::Error::Busy)?;
            match self.occupied.compare_exchange(
                occupied,
                occupied | mask(slot)?,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => {
                    return Ok(Temporary {
                        spool: self,
                        slot,
                        file: None,
                        identity: None,
                        cleaned: None,
                    })
                }
                Err(actual) => occupied = actual,
            }
        }
        Err(ports::Error::Busy)
    }
    fn path<'a>(
        &self,
        slot: u8,
        response: bool,
        bytes: &'a mut [u8; MAX_PATH_BYTES],
    ) -> Result<&'a std::path::Path, ports::Error> {
        let name = Name::request_slot(slot, response).map_err(|_| ports::Error::Invalid)?;
        Ok(self.root.root.directory.join(&name, bytes)?)
    }
}
fn mask(slot: u8) -> Result<u32, ports::Error> {
    1u32.checked_shl(u32::from(slot))
        .ok_or(ports::Error::Invalid)
}
fn check_file(metadata: &Metadata, owner: u32) -> Result<(), ports::Error> {
    super::check_temporary(metadata, owner, MAX_BYTES)
}

pub(crate) struct Temporary<'s, 'r> {
    spool: &'s RequestSpool<'r>,
    slot: u8,
    file: Option<File>,
    identity: Option<Metadata>,
    cleaned: Option<Result<(), ports::Error>>,
}
impl Temporary<'_, '_> {
    pub(crate) fn create(&mut self) -> Result<(), ports::Error> {
        let mut path = [0; MAX_PATH_BYTES];
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(self.spool.path(self.slot, true, &mut path)?)?;
        self.file = Some(file);
        self.identity = Some(self.file()?.metadata()?);
        self.file()?
            .set_permissions(fs::Permissions::from_mode(0o600))?;
        let metadata = self.file()?.metadata()?;
        check_file(&metadata, self.spool.owner)?;
        if metadata.mode() & 0o7777 != 0o600 {
            return Err(ports::Error::Invalid);
        }
        Ok(())
    }
    fn file(&mut self) -> Result<&mut File, ports::Error> {
        self.file.as_mut().ok_or(ports::Error::Invalid)
    }
    pub(crate) fn write_at(&mut self, offset: u64, bytes: &[u8]) -> Result<(), ports::Error> {
        self.file()?.seek(SeekFrom::Start(offset))?;
        self.file()?.write_all(bytes)?;
        Ok(())
    }
    pub(crate) fn read_at(&mut self, offset: u64, bytes: &mut [u8]) -> Result<(), ports::Error> {
        self.file()?.seek(SeekFrom::Start(offset))?;
        self.file()?.read_exact(bytes)?;
        Ok(())
    }
    pub(crate) fn truncate(&mut self, length: u64) -> Result<(), ports::Error> {
        self.file()?.set_len(length)?;
        Ok(())
    }
    pub(crate) fn cleanup(&mut self) -> Result<(), ports::Error> {
        if let Some(result) = self.cleaned {
            return result;
        }
        let mut path = [0; MAX_PATH_BYTES];
        let path = self.spool.path(self.slot, true, &mut path);
        let result = super::cleanup_temporary(
            &mut self.file,
            self.identity.as_ref(),
            path,
            self.spool.owner,
            MAX_BYTES,
        );
        if result.is_ok() {
            self.spool
                .occupied
                .fetch_and(!mask(self.slot)?, Ordering::SeqCst);
        } else {
            self.spool
                .retired
                .fetch_or(mask(self.slot)?, Ordering::SeqCst);
        }
        self.cleaned = Some(result);
        result
    }
}
impl Drop for Temporary<'_, '_> {
    fn drop(&mut self) {
        let _ = self.cleanup();
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::{
        ports::{Clock, Deadline, Tick, Time},
        store_fs::tests::Fixture,
    };
    use std::sync::Arc;
    struct Timer;
    impl Clock for Timer {
        fn sample(&self) -> Result<Time, ports::Error> {
            Ok(Time {
                utc_ms: 0,
                monotonic: Tick(1),
            })
        }
    }
    fn deadline() -> Deadline {
        Deadline::after(Tick(0), 100).unwrap()
    }
    fn abandoned(fixture: &Fixture) -> std::path::PathBuf {
        let path = fixture.path.join("request-00");
        DirBuilder::new().mode(0o700).create(&path).unwrap();
        let file = path.join("response");
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&file)
            .unwrap()
            .write_all(b"private")
            .unwrap();
        file
    }
    #[test]
    fn startup_discards_valid_files_and_repairs_owner_only_directory_modes() {
        let fixture = Fixture::new();
        let mut root = fixture.locked();
        let file = abandoned(&fixture);
        fs::set_permissions(
            fixture.path.join("request-00"),
            fs::Permissions::from_mode(0o500),
        )
        .unwrap();
        let spool = RequestSpool::open(&mut root, Arc::new(Timer), deadline()).unwrap();
        assert!(!file.exists());
        assert_eq!(fs::read_dir(&fixture.path).unwrap().count(), 33);
        let mut owner = spool.reserve().unwrap();
        owner.create().unwrap();
        owner.cleanup().unwrap();
        assert!(!file.exists());
        assert!(owner.cleanup().is_ok());
    }
    #[test]
    fn unknown_namespace_artifact_prevents_any_deletion() {
        let fixture = Fixture::new();
        let mut root = fixture.locked();
        let file = abandoned(&fixture);
        fs::set_permissions(
            fixture.path.join("request-00"),
            fs::Permissions::from_mode(0o500),
        )
        .unwrap();
        fs::write(fixture.path.join("unexpected"), b"x").unwrap();
        assert!(RequestSpool::open(&mut root, Arc::new(Timer), deadline()).is_err());
        assert_eq!(fs::read(&file).unwrap(), b"private");
        assert_eq!(
            fs::metadata(fixture.path.join("request-00"))
                .unwrap()
                .permissions()
                .mode()
                & 0o7777,
            0o500
        );
        fs::set_permissions(
            fixture.path.join("request-00"),
            fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        fs::remove_file(fixture.path.join("unexpected")).unwrap();
        fs::write(fixture.path.join("request-00/extra"), b"x").unwrap();
        assert!(RequestSpool::open(&mut root, Arc::new(Timer), deadline()).is_err());
        assert_eq!(fs::read(&file).unwrap(), b"private");
    }
    #[test]
    fn symlinks_hardlinks_permissions_and_oversize_refuse_cleanup() {
        for variant in 0..4 {
            let fixture = Fixture::new();
            let mut root = fixture.locked();
            let file = abandoned(&fixture);
            let outside = Fixture::new();
            match variant {
                0 => {
                    fs::remove_file(&file).unwrap();
                    std::os::unix::fs::symlink("/etc/passwd", &file).unwrap();
                }
                1 => {
                    fs::hard_link(&file, outside.path.join("other")).unwrap();
                }
                2 => fs::set_permissions(&file, fs::Permissions::from_mode(0o640)).unwrap(),
                _ => OpenOptions::new()
                    .write(true)
                    .open(&file)
                    .unwrap()
                    .set_len(MAX_BYTES + 1)
                    .unwrap(),
            }
            assert!(RequestSpool::open(&mut root, Arc::new(Timer), deadline()).is_err());
            assert!(fs::symlink_metadata(&file).is_ok());
        }
    }
    #[test]
    fn bounded_slot_capacity_and_canonical_names() {
        let fixture = Fixture::new();
        let mut root = fixture.locked();
        let spool = RequestSpool::open(&mut root, Arc::new(Timer), deadline()).unwrap();
        let mut owners = Vec::new();
        for _ in 0..32 {
            owners.push(spool.reserve().unwrap());
        }
        assert!(matches!(spool.reserve(), Err(ports::Error::Busy)));
        owners.pop().unwrap().cleanup().unwrap();
        assert!(spool.reserve().is_ok());
        assert!(Name::request_slot(32, false).is_err());
        assert!(Name::request_slot(32, true).is_err());
        assert!(std::mem::size_of::<RequestSpool<'_>>() <= 32);
    }
    #[test]
    #[ignore = "owned child fixture for response process-death cleanup"]
    fn abrupt_exit_child() {
        let path = std::env::var("TD_MTA_RESPONSE_CRASH_ROOT").unwrap();
        super::super::with_probe_root_path(std::path::Path::new(&path), |root| {
            let spool = RequestSpool::open(root, Arc::new(Timer), deadline()).unwrap();
            let mut owner = spool.reserve().unwrap();
            owner.create().unwrap();
            owner.write_at(0, b"unfinished private response").unwrap();
            std::process::exit(77);
        });
    }
    struct OwnedChild(std::process::Child);
    impl Drop for OwnedChild {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[test]
    fn cold_reopen_discards_response_left_by_process_exit_without_drop() {
        let fixture = Fixture::new();
        let mut child = OwnedChild(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "store_fs::request_spool::tests::abrupt_exit_child",
                    "--nocapture",
                    "--ignored",
                    "--test-threads=1",
                ])
                .env("TD_MTA_RESPONSE_CRASH_ROOT", &fixture.path)
                .stdout(std::process::Stdio::null())
                .spawn()
                .unwrap(),
        );
        let end = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let status = loop {
            if let Some(status) = child.0.try_wait().unwrap() {
                break status;
            }
            assert!(std::time::Instant::now() < end, "response child timed out");
            std::thread::sleep(std::time::Duration::from_millis(1));
        };
        assert_eq!(status.code(), Some(77));
        let file = fixture.path.join("request-00/response");
        assert_eq!(fs::read(&file).unwrap(), b"unfinished private response");
        let mut root = fixture.locked();
        let spool = RequestSpool::open(&mut root, Arc::new(Timer), deadline()).unwrap();
        assert!(!file.exists());
        let mut next = spool.reserve().unwrap();
        next.create().unwrap();
        next.cleanup().unwrap();
    }
    #[test]
    fn inspection_repairs_only_needed_owner_bits_before_refusal() {
        let fixture = Fixture::new();
        let mut root = fixture.locked();
        let file = abandoned(&fixture);
        let directory = fixture.path.join("request-00");
        let extra = directory.join("unexpected");
        fs::write(&extra, b"x").unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o100)).unwrap();
        assert!(RequestSpool::open(&mut root, Arc::new(Timer), deadline()).is_err());
        assert_eq!(fs::metadata(&directory).unwrap().mode() & 0o7777, 0o500);
        assert_eq!(fs::read(&file).unwrap(), b"private");
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        fs::remove_file(extra).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o100)).unwrap();
        let _spool = RequestSpool::open(&mut root, Arc::new(Timer), deadline()).unwrap();
        assert_eq!(fs::metadata(directory).unwrap().mode() & 0o7777, 0o700);
        assert!(!file.exists());
    }
    #[test]
    fn cleanup_ceiling_matches_maximum_configured_response() {
        use crate::admission::{DiskLimits, ViewMode, WorkLimits};
        let resources = crate::limits::Limits::default().plan().unwrap();
        let mut limits = DiskLimits {
            response_bytes: MAX_BYTES,
            response_total_bytes: MAX_BYTES + 1,
            ..DiskLimits::default()
        };
        assert!(limits
            .plan(
                &resources,
                WorkLimits::default(),
                ViewMode::OnlineBackground
            )
            .is_ok());
        limits.response_bytes += 1;
        assert!(matches!(
            limits.plan(
                &resources,
                WorkLimits::default(),
                ViewMode::OnlineBackground
            ),
            Err(crate::admission::Error::Range {
                field: "response_bytes",
                ..
            })
        ));
    }
}

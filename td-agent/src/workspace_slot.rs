//! Persistent conversation-to-account reservations. One private pool in the
//! canonical human home is shared by all state directories and processes.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::intake::{Created, Root, Slot, Status};
use crate::store::{Id, StateDir};

const COUNT: u16 = 960;
const POOL: &str = ".local/state/td-agent/workspace-slots";
const O_NOFOLLOW: i32 = 0x20000;
const O_NONBLOCK: i32 = 0x800;

pub(crate) fn error(e: impl std::fmt::Display) -> String {
    td_alloc::format(format_args!("{e}")).unwrap_or_default()
}

pub(crate) fn joined(base: &Path, name: &str) -> io::Result<PathBuf> {
    let base = base
        .to_str()
        .ok_or_else(|| io::Error::other("workspace state path is not UTF-8"))?;
    td_alloc::format(format_args!("{base}/{name}"))
        .map(PathBuf::from)
        .map_err(|_| io::Error::from(io::ErrorKind::OutOfMemory))
}

pub fn product() -> io::Result<bool> {
    match fs::symlink_metadata(crate::flathub_app::PRODUCT_CONFIG) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Account {
    name: String,
    home: PathBuf,
}

impl Account {
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn launch(
        &self,
        policy: &crate::jail::Policy,
        argv: &[String],
    ) -> io::Result<std::os::unix::net::UnixStream> {
        Root::default().launch(Slot::parse(&self.name)?, &self.home, policy, argv)
    }
}

fn home() -> io::Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .ok_or_else(|| io::Error::other("no human home for workspace slots"))?;
    let home = fs::canonicalize(home)?;
    if home == Path::new("/") {
        return Err(io::Error::other("the human home is /"));
    }
    Ok(home)
}

trait Service {
    fn status(&self, slot: Slot<'_>) -> io::Result<Status>;
    fn create(&self, slot: Slot<'_>) -> io::Result<Created>;
    fn delete(&self, slot: Slot<'_>) -> io::Result<()>;
}
impl Service for Root<'_> {
    fn status(&self, slot: Slot<'_>) -> io::Result<Status> {
        self.status(slot)
    }
    fn create(&self, slot: Slot<'_>) -> io::Result<Created> {
        self.create(slot)
    }
    fn delete(&self, slot: Slot<'_>) -> io::Result<()> {
        self.delete(slot)
    }
}

fn name(n: u16) -> io::Result<String> {
    td_alloc::format(format_args!("agent-{n}")).map_err(|_| io::ErrorKind::OutOfMemory.into())
}

fn lock(pool: &Path) -> io::Result<File> {
    td_fs::private_dir(pool)?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(O_NOFOLLOW | O_NONBLOCK)
        .open(joined(pool, "lock")?)?;
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.nlink() != 1
        || metadata.mode() & 0o077 != 0
        || metadata.uid() != fs::metadata("/proc/self")?.uid()
    {
        return Err(io::Error::other("invalid workspace slot lock"));
    }
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20))
            }
            Err(fs::TryLockError::WouldBlock) => {
                return Err(io::Error::other("workspace slot pool is busy; ask again"))
            }
            Err(fs::TryLockError::Error(e)) => return Err(e),
        }
    }
}

/// `None` is absent; `Some(false)` belongs to another conversation. A
/// malformed record refuses the pool, never silently frees an account.
fn claim(path: &Path, owner: &Path) -> io::Result<Option<bool>> {
    let mut file = match OpenOptions::new()
        .read(true)
        .custom_flags(O_NOFOLLOW | O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.nlink() != 1
        || metadata.mode() & 0o077 != 0
        || metadata.uid() != fs::metadata("/proc/self")?.uid()
    {
        return Err(io::Error::other("invalid workspace slot reservation"));
    }
    let mut bytes = [0; 4097];
    let mut len = 0;
    while len < bytes.len() {
        let read = file.read(bytes.get_mut(len..).unwrap_or_default())?;
        if read == 0 {
            break;
        }
        len += read;
    }
    let bytes = bytes.get(..len).unwrap_or_default();
    if bytes.is_empty() || len > 4096 || bytes.first() != Some(&b'/') || bytes.contains(&0) {
        return Err(io::Error::other("malformed workspace slot reservation"));
    }
    Ok(Some(bytes == owner.as_os_str().as_bytes()))
}

fn owner(state: &StateDir, id: &Id) -> io::Result<PathBuf> {
    joined(
        &joined(&fs::canonicalize(state.root())?, "conversations")?,
        id.as_str(),
    )
}

fn acquire(pool: &Path, owner: &Path, service: &impl Service) -> io::Result<String> {
    let bytes = owner.as_os_str().as_bytes();
    if bytes.len() > 4096 {
        return Err(io::Error::other("workspace owner path exceeds 4096 bytes"));
    }
    let _lock = lock(pool)?;
    // Find an existing reservation before considering any free slot.
    let mut ours = None;
    for n in 1..=COUNT {
        let name = name(n)?;
        if claim(&joined(pool, &name)?, owner)? == Some(true) {
            if ours.is_some() {
                return Err(io::Error::other(
                    "conversation has duplicate workspace slots",
                ));
            }
            ours = Some(name);
        }
    }
    if let Some(name) = ours {
        let slot = Slot::parse(&name)?;
        match service.status(slot)? {
            Status::Held(_) => {} // Its own persistent home and scratch.
            Status::Absent | Status::Free(_) => {
                service.create(slot)?;
            }
        }
        return Ok(name);
    }
    for n in 1..=COUNT {
        let name = name(n)?;
        let path = joined(pool, &name)?;
        if claim(&path, owner)?.is_some() {
            continue;
        }
        let slot = Slot::parse(&name)?;
        if matches!(service.status(slot)?, Status::Held(_)) {
            continue;
        }
        // Reserve durably BEFORE root creates anything. A crash resumes
        // this owner; neither another state directory nor a new process
        // can take its still-empty account between creation and launch.
        td_fs::replace(&path, bytes, 0o600)?;
        match service.create(slot)? {
            Created::Ready(_) => return Ok(name),
            Created::Held => {
                return Err(io::Error::other(
                    "workspace slot changed while it was reserved",
                ))
            }
        }
    }
    Err(io::Error::other(
        "all 960 conversation workspace slots are in use",
    ))
}

pub fn ensure(state: &StateDir, id: &Id) -> Result<Option<Account>, String> {
    if !product().map_err(error)? {
        return Ok(None);
    }
    let home = home().map_err(error)?;
    let pool = joined(&home, POOL).map_err(error)?;
    let owner = owner(state, id).map_err(error)?;
    let service = Root::default();
    let name = acquire(&pool, &owner, &service).map_err(error)?;
    let account = Account { name, home };
    initialize(
        &pool,
        &owner,
        account.name(),
        &service,
        || {
            crate::workspace_upgrade::prepare(
                &account,
                &account.home,
                &crate::workspace::jail_dir(state, id),
            )
        },
        || crate::workspace_upgrade::finish(&crate::workspace::jail_dir(state, id)),
    )
    .map_err(error)?;
    Ok(Some(account))
}

fn ready(pool: &Path, name: &str) -> io::Result<PathBuf> {
    joined(
        pool,
        &td_alloc::format(format_args!("ready-{name}")).map_err(|_| io::ErrorKind::OutOfMemory)?,
    )
}

fn initialize(
    pool: &Path,
    owner: &Path,
    name: &str,
    service: &impl Service,
    import: impl FnOnce() -> io::Result<()>,
    finish: impl FnOnce() -> io::Result<()>,
) -> io::Result<()> {
    let _lock = lock(pool)?;
    if claim(&joined(pool, name)?, owner)? != Some(true) {
        return Err(io::Error::other(
            "workspace reservation changed before initialization",
        ));
    }
    let ready = ready(pool, name)?;
    if claim(&ready, owner)? == Some(true) {
        return finish();
    }
    let slot = Slot::parse(name)?;
    // No entry is launched before this receipt. A crashed partial import
    // must start from an empty account; never merge it with a retry.
    service.delete(slot)?;
    if service.create(slot)? == Created::Held {
        return Err(io::Error::other(
            "workspace account was occupied during initialization",
        ));
    }
    import()?;
    td_fs::replace(&ready, owner.as_os_str().as_bytes(), 0o600)?;
    finish()
}

fn release(pool: &Path, owner: &Path, service: &impl Service) -> io::Result<()> {
    if !pool.try_exists()? {
        return Ok(());
    }
    let _lock = lock(pool)?;
    for n in 1..=COUNT {
        let name = name(n)?;
        let path = joined(pool, &name)?;
        if claim(&path, owner)? == Some(true) {
            // Retain the reservation on refusal or a crash. A deleted
            // conversation's tombstone retries this before removing it.
            service.delete(Slot::parse(&name)?)?;
            match fs::remove_file(ready(pool, &name)?) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
            fs::remove_file(&path)?;
            File::open(pool)?.sync_all()?;
        }
    }
    Ok(())
}

pub fn delete(state: &StateDir, id: &Id) -> Result<(), String> {
    if !product().map_err(error)? {
        return Ok(());
    }
    release(
        &joined(&home().map_err(error)?, POOL).map_err(error)?,
        &owner(state, id).map_err(error)?,
        &Root::default(),
    )
    .map_err(error)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    #[derive(Default)]
    struct Fake {
        held: RefCell<Vec<String>>,
        deleted: RefCell<Vec<String>>,
        fail: std::cell::Cell<bool>,
    }
    impl Service for Fake {
        fn status(&self, slot: Slot<'_>) -> io::Result<Status> {
            Ok(if self.held.borrow().iter().any(|n| n == slot.name()) {
                Status::Held(98304)
            } else {
                Status::Free(98304)
            })
        }
        fn create(&self, slot: Slot<'_>) -> io::Result<Created> {
            if self.fail.get() {
                return Err(io::Error::other("root unavailable"));
            }
            self.held.borrow_mut().push(slot.name().into());
            Ok(Created::Ready(98304))
        }
        fn delete(&self, slot: Slot<'_>) -> io::Result<()> {
            if self.fail.get() {
                return Err(io::Error::other("still running"));
            }
            self.deleted.borrow_mut().push(slot.name().into());
            self.held.borrow_mut().retain(|n| n != slot.name());
            Ok(())
        }
    }
    #[test]
    fn initialization_retries_partial_import_but_not_a_finished_copy() {
        let dir = td_fs::TempDir::new("agent-slot-import").unwrap();
        let pool = dir.path().join("pool");
        let owner = Path::new("/state/conversations/one");
        let service = Fake::default();
        let name = acquire(&pool, owner, &service).unwrap();
        assert!(initialize(
            &pool,
            owner,
            &name,
            &service,
            || Err(io::Error::other("copy failed")),
            || Ok(())
        )
        .is_err());
        assert!(initialize(
            &pool,
            owner,
            &name,
            &service,
            || Ok(()),
            || Err(io::Error::other("crash before conversation receipt"))
        )
        .is_err());
        initialize(
            &pool,
            owner,
            &name,
            &service,
            || panic!("completed copy must not be reset"),
            || Ok(()),
        )
        .unwrap();
        assert_eq!(service.deleted.borrow().len(), 2);
        release(&pool, owner, &service).unwrap();
        assert!(!ready(&pool, &name).unwrap().exists());
    }
    #[test]
    fn reservations_survive_failures_and_separate_state_directories() {
        let dir = td_fs::TempDir::new("agent-slots").unwrap();
        let pool = dir.path().join("pool");
        let service = Fake::default();
        service.held.borrow_mut().push("agent-1".into());
        service.fail.set(true);
        assert!(acquire(&pool, Path::new("/a/conversations/one"), &service).is_err());
        service.fail.set(false);
        assert_eq!(
            acquire(&pool, Path::new("/a/conversations/one"), &service).unwrap(),
            "agent-2"
        );
        assert_eq!(
            acquire(&pool, Path::new("/b/conversations/one"), &service).unwrap(),
            "agent-3"
        );
        assert_eq!(
            acquire(&pool, Path::new("/a/conversations/one"), &service).unwrap(),
            "agent-2"
        );
        service.fail.set(true);
        assert!(release(&pool, Path::new("/a/conversations/one"), &service).is_err());
        assert!(pool.join("agent-2").exists());
        service.fail.set(false);
        release(&pool, Path::new("/a/conversations/one"), &service).unwrap();
        assert_eq!(
            acquire(&pool, Path::new("/c/conversations/new"), &service).unwrap(),
            "agent-2"
        );
        assert_eq!(*service.deleted.borrow(), ["agent-2"]);
    }
    #[test]
    fn malformed_and_linked_reservations_refuse_without_reclaiming() {
        let dir = td_fs::TempDir::new("agent-slots-invalid").unwrap();
        let pool = dir.path().join("pool");
        let service = Fake::default();
        let owner = Path::new("/state/conversations/one");
        acquire(&pool, owner, &service).unwrap();
        td_fs::replace(&pool.join("agent-2"), b"", 0o600).unwrap();
        assert!(acquire(&pool, owner, &service).is_err());
        fs::remove_file(pool.join("agent-2")).unwrap();
        std::os::unix::fs::symlink(pool.join("agent-1"), pool.join("agent-2")).unwrap();
        assert!(acquire(&pool, owner, &service).is_err());
        assert!(service.deleted.borrow().is_empty());
    }
}

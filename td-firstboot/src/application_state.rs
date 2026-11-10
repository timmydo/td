//! Each application account's private, root-controlled home: made at boot,
//! with the human's legacy state moved into it for an application whose
//! account predates the workload account ledger, or empty for one an
//! install allocated, then, while the human's session runs, by the install.

use std::ffi::OsStr;
use std::fs::{self, File, Metadata, OpenOptions, Permissions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use crate::ApplicationHome;

const NOFOLLOW: i32 = 0x20000;
const DIRECTORY: i32 = 0x10000;
const NONBLOCK: i32 = 0x800;
const MAX_ENTRIES: usize = 1_000_000;
const MAX_DEPTH: usize = 64;

fn at(parent: &File, name: impl AsRef<OsStr>) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", parent.as_raw_fd())).join(name.as_ref())
}

fn directory(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(NOFOLLOW | DIRECTORY)
        .open(path)
}

type Owner = (u32, u32);

/// How preparation reads and changes an inode's owner. The live tree uses
/// the inode's own ids; tests give a scratch tree logical owners, so every
/// path runs as any user.
trait Ownership {
    fn of(&self, metadata: &Metadata) -> Owner;
    fn give(&self, file: &File, owner: Owner) -> io::Result<()>;
    fn give_link(&self, path: &Path, owner: Owner) -> io::Result<()>;
}

struct Inodes;

impl Ownership for Inodes {
    fn of(&self, metadata: &Metadata) -> Owner {
        (metadata.uid(), metadata.gid())
    }

    fn give(&self, file: &File, owner: Owner) -> io::Result<()> {
        std::os::unix::fs::fchown(file, Some(owner.0), Some(owner.1))
    }

    fn give_link(&self, path: &Path, owner: Owner) -> io::Result<()> {
        std::os::unix::fs::lchown(path, Some(owner.0), Some(owner.1))
    }
}

fn invalid(why: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, why)
}

/// Where the homes live: below `top`, through `chain`, every directory owned
/// by `root` without other writers. Tests name a scratch tree they own.
struct Place<'a> {
    top: &'a str,
    chain: &'a [&'a str],
    root: Owner,
    ids: &'a dyn Ownership,
}

const LIVE: Place<'static> = Place {
    top: "/",
    chain: &["var", "lib", "td"],
    root: (0, 0),
    ids: &Inodes,
};

fn root_child(
    ids: &dyn Ownership,
    parent: &File,
    name: &str,
    mode: u32,
    root: Owner,
) -> io::Result<File> {
    staging_child(ids, parent, name, mode, root, root)
}

fn staging_child(
    ids: &dyn Ownership,
    parent: &File,
    name: &str,
    mode: u32,
    root: Owner,
    account: Owner,
) -> io::Result<File> {
    let path = at(parent, name);
    match fs::DirBuilder::new().mode(mode).create(&path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    let child = directory(&path)?;
    let metadata = child.metadata()?;
    if (!matches_owner(ids, &metadata, root) && !matches_owner(ids, &metadata, account))
        || metadata.mode() & 0o7777 & !mode != 0
    {
        return Err(invalid(
            "application staging directory is not private root-owned state",
        ));
    }
    child.set_permissions(Permissions::from_mode(mode))?;
    child.sync_all()?;
    parent.sync_all()?;
    Ok(child)
}

fn root_chain(place: &Place) -> io::Result<File> {
    let ids = place.ids;
    let mut current = directory(Path::new(place.top))?;
    for name in place.chain {
        let metadata = current.metadata()?;
        if !matches_owner(ids, &metadata, place.root) || metadata.mode() & 0o022 != 0 {
            return Err(invalid(
                "application state ancestor permits an untrusted writer",
            ));
        }
        current = directory(&at(&current, name))?;
    }
    let metadata = current.metadata()?;
    if !matches_owner(ids, &metadata, place.root) || metadata.mode() & 0o022 != 0 {
        return Err(invalid(
            "application state parent permits an untrusted writer",
        ));
    }
    root_child(ids, &current, "applications", 0o755, place.root)
}

fn migration_lock(ids: &dyn Ownership, base: &File, root: Owner) -> io::Result<File> {
    let path = at(base, ".migration.lock");
    let file = match OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(NOFOLLOW | NONBLOCK)
        .open(&path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(NOFOLLOW | NONBLOCK)
            .open(&path)?,
        Err(error) => return Err(error),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || !matches_owner(ids, &metadata, root)
        || metadata.nlink() != 1
        || metadata.len() != 0
        || metadata.mode() & 0o7777 & !0o600 != 0
    {
        return Err(invalid("application migration lock has invalid metadata"));
    }
    file.set_permissions(Permissions::from_mode(0o600))?;
    file.try_lock().map_err(|error| {
        io::Error::other(format!(
            "application migration is busy or unavailable: {error}"
        ))
    })?;
    Ok(file)
}

/// Called by root at sysinit, before either the human or app processes start.
/// The caller has validated the immutable account and durable UID reservation.
pub(crate) fn prepare(
    former: &ApplicationHome,
    application: &str,
    uid: u32,
) -> Result<ApplicationHome, String> {
    if former.gid == 0 || former.gid == u32::MAX {
        return Err("application migration requires a nonzero, non-sentinel human gid".into());
    }
    if !(1000..=65533).contains(&former.uid) {
        return Err("invalid application state identity".into());
    }
    validate(application, uid)?;
    boot(&LIVE, former, application, uid, (uid, uid))?;
    Ok(ApplicationHome {
        home: crate::principals::application_home(uid),
        uid,
        gid: uid,
    })
}

/// Boot's preparation of `uid`'s home. Only an account of the carried block
/// (`CARRIED_UIDS`: the image's carried rows and the reservations that
/// predate the ledger) can have legacy state of the human's, written when
/// the application ran as the human; boot moves it in. An account an
/// install allocated gets `create`'s home, never the human's state, even
/// over that install's interrupted, root-owned one.
fn boot(
    place: &Place,
    former: &ApplicationHome,
    application: &str,
    uid: u32,
    account: Owner,
) -> Result<(), String> {
    let source = migrates(uid).then_some(former);
    home(place, source, application, account, false)
}

fn migrates(uid: u32) -> bool {
    crate::workload_accounts::CARRIED_UIDS.contains(&uid)
}

/// The home of an account allocated at install, while the human's session
/// runs: what boot makes for one, never a migration of the human's. A
/// published home is left as it is, so a reinstall finds its state and the
/// next boot's `prepare` returns at once; root's interrupted creation is
/// completed. For a `fresh` account, whose ledger row this install added,
/// any existing home is refused: allocation avoided every uid with a home,
/// so this is a defence. The caller holds the principal lock.
pub(crate) fn create(application: &str, uid: u32, fresh: bool) -> Result<(), String> {
    validate(application, uid)?;
    home(&LIVE, None, application, (uid, uid), fresh)
}

fn validate(application: &str, uid: u32) -> Result<(), String> {
    if !crate::workload_accounts::APPLICATION_UIDS.contains(&uid)
        || application.len() > 64
        || !application
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_lowercase)
        || !application.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"._-".contains(&byte)
        })
    {
        return Err("invalid application state identity".into());
    }
    Ok(())
}

/// The private home `applications/UID` of `place`, published to `account`
/// once complete, with `former`'s state moved into it when there is one.
fn home(
    place: &Place,
    former: Option<&ApplicationHome>,
    application: &str,
    account: Owner,
    fresh: bool,
) -> Result<(), String> {
    let (root, ids) = (place.root, place.ids);
    let result = (|| {
        let base = root_chain(place)?;
        let _migration = migration_lock(ids, &base, root)?;
        let name = account.0.to_string();
        match directory(&at(&base, &name)) {
            Ok(_) if fresh => {
                return Err(invalid("a new account's home already exists"));
            }
            Ok(existing) => {
                let metadata = existing.metadata()?;
                if matches_owner(ids, &metadata, account) && metadata.mode() & 0o7777 == 0o700 {
                    return Ok(());
                }
                if !matches_owner(ids, &metadata, root) || metadata.mode() & 0o7777 & !0o700 != 0 {
                    return Err(invalid(
                        "application home has invalid ownership or permissions",
                    ));
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let private = root_child(ids, &base, &name, 0o700, root)?;
        let state = staging_child(ids, &private, ".td", 0o700, root, account)?;
        let applications = staging_child(ids, &state, "app", 0o700, root, account)?;
        migrate(ids, former, application, account, &applications, root)?;
        for child in [&applications, &state] {
            ids.give(child, account)?;
            child.sync_all()?;
        }
        // All child metadata and rename publications precede removal of this
        // root-owned barrier. Interrupted conversion stays inaccessible.
        ids.give(&private, account)?;
        private.sync_all()?;
        base.sync_all()
    })();
    result.map_err(|error| format!("prepare application {application} state: {error}"))
}

fn legacy_parent(ids: &dyn Ownership, former: &ApplicationHome) -> io::Result<Option<File>> {
    let mut current = match directory(&former.home) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    for name in [".td", "app"] {
        let metadata = current.metadata()?;
        if !matches_owner(ids, &metadata, (former.uid, former.gid)) || metadata.mode() & 0o022 != 0
        {
            return Err(invalid(
                "legacy application parent has another owner or permits other writers",
            ));
        }
        current = match directory(&at(&current, name)) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
    }
    let metadata = current.metadata()?;
    if !matches_owner(ids, &metadata, (former.uid, former.gid)) || metadata.mode() & 0o022 != 0 {
        return Err(invalid(
            "legacy application parent has another owner or permits other writers",
        ));
    }
    Ok(Some(current))
}

/// Moves `former`'s legacy tree into `target`, or with no `former` makes an
/// empty one; either way the result belongs to `account`. `root` owns what
/// an interrupted run left unpublished.
fn migrate(
    ids: &dyn Ownership,
    former: Option<&ApplicationHome>,
    application: &str,
    account: Owner,
    target: &File,
    root: Owner,
) -> io::Result<()> {
    // Without a former owner only the application's own inodes are valid.
    let owner = former.map_or(account, |former| (former.uid, former.gid));
    let destination = at(target, application);
    let needs_source = match directory(&destination) {
        Ok(entry) if matches_owner(ids, &entry.metadata()?, root) => {
            require_empty(&entry)?;
            true
        }
        Ok(_) => false,
        Err(error) if error.kind() == io::ErrorKind::NotFound => true,
        Err(error) => return Err(error),
    };
    if needs_source {
        if let Some(source) = former
            .map(|former| legacy_parent(ids, former))
            .transpose()?
            .flatten()
        {
            let path = at(&source, application);
            match directory(&path) {
                Ok(old) => {
                    let metadata = old.metadata()?;
                    if !matches_owner(ids, &metadata, owner) {
                        return Err(invalid("legacy application root has another owner"));
                    }
                    // Refuse malformed legacy state before removing its human
                    // pathname. Startup ordering excludes writers between passes.
                    walk(
                        ids,
                        &old,
                        owner,
                        account,
                        metadata.dev(),
                        0,
                        &mut { MAX_ENTRIES },
                        false,
                    )?;
                    fs::rename(&path, &destination)?;
                    target.sync_all()?;
                    source.sync_all()?;
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        if !exists(&destination)? {
            fs::DirBuilder::new().mode(0o700).create(&destination)?;
            let empty = directory(&destination)?;
            empty.set_permissions(Permissions::from_mode(0o700))?;
            ids.give(&empty, account)?;
            empty.sync_all()?;
            target.sync_all()?;
        }
    }
    let entry = directory(&destination)?;
    if matches_owner(ids, &entry.metadata()?, root) {
        require_empty(&entry)?;
        entry.set_permissions(Permissions::from_mode(0o700))?;
        ids.give(&entry, account)?;
        entry.sync_all()?;
    }
    let device = entry.metadata()?.dev();
    let mut remaining = MAX_ENTRIES;
    walk(ids, &entry, owner, account, device, 0, &mut remaining, true)?;
    target.sync_all()
}

fn require_empty(entry: &File) -> io::Result<()> {
    if fs::read_dir(at(entry, "."))?.next().transpose()?.is_some() {
        return Err(invalid(
            "interrupted empty application creation contains unexpected state",
        ));
    }
    Ok(())
}

fn exists(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

#[allow(clippy::too_many_arguments)]
fn walk(
    ids: &dyn Ownership,
    directory: &File,
    former: (u32, u32),
    uid: (u32, u32),
    device: u64,
    depth: usize,
    remaining: &mut usize,
    change_owner: bool,
) -> io::Result<()> {
    if depth > MAX_DEPTH {
        return Err(invalid(
            "application state exceeds the migration depth bound",
        ));
    }
    check_inode(ids, &directory.metadata()?, former, uid, device)?;
    for entry in fs::read_dir(at(directory, "."))? {
        *remaining = remaining
            .checked_sub(1)
            .ok_or_else(|| invalid("application state exceeds the migration entry bound"))?;
        let path = at(directory, entry?.file_name());
        let metadata = fs::symlink_metadata(&path)?;
        check_inode(ids, &metadata, former, uid, device)?;
        if metadata.is_dir() {
            walk(
                ids,
                &self::directory(&path)?,
                former,
                uid,
                device,
                depth + 1,
                remaining,
                change_owner,
            )?;
        } else if metadata.is_file() {
            let file = OpenOptions::new()
                .read(true)
                .custom_flags(NOFOLLOW | NONBLOCK)
                .open(&path)?;
            check_inode(ids, &file.metadata()?, former, uid, device)?;
            if change_owner {
                ids.give(&file, uid)?;
                file.sync_all()?;
            }
        } else if metadata.file_type().is_symlink() {
            // Preserve the link text without following it. The retained
            // root-owned home excludes writers throughout this migration or
            // install-time creation, until it is published.
            if change_owner {
                ids.give_link(&path, uid)?;
            }
        } else {
            return Err(invalid("application state contains a special file"));
        }
    }
    if change_owner {
        ids.give(directory, uid)?;
        directory.sync_all()?;
    }
    Ok(())
}

fn check_inode(
    ids: &dyn Ownership,
    metadata: &Metadata,
    former: (u32, u32),
    uid: (u32, u32),
    device: u64,
) -> io::Result<()> {
    if (!matches_owner(ids, metadata, former) && !matches_owner(ids, metadata, uid))
        || metadata.dev() != device
        || (!metadata.is_dir() && metadata.nlink() != 1)
        || metadata.mode() & 0o6000 != 0
    {
        return Err(invalid(
            "application state inode has foreign ownership, links, mount, or privilege bits",
        ));
    }
    Ok(())
}

fn matches_owner(ids: &dyn Ownership, metadata: &Metadata, owner: Owner) -> bool {
    ids.of(metadata) == owner
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "td-app-state-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
            Self(path)
        }
        fn convert(&self, limit: usize, depth: usize) -> io::Result<()> {
            let file = directory(&self.0)?;
            let metadata = file.metadata()?;
            let owner = (metadata.uid(), metadata.gid());
            walk(
                &Inodes,
                &file,
                owner,
                owner,
                metadata.dev(),
                depth,
                &mut { limit },
                true,
            )
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// Logical owners for a scratch tree, keyed by inode: an inode nobody
    /// gave an owner is root's, as everything root creates is. Every path
    /// then runs as any user, with no real chown.
    struct Logical {
        root: Owner,
        owners: std::cell::RefCell<std::collections::HashMap<(u64, u64), Owner>>,
    }

    impl Logical {
        fn set(&self, metadata: &Metadata, owner: Owner) {
            self.owners
                .borrow_mut()
                .insert((metadata.dev(), metadata.ino()), owner);
        }
    }

    impl Ownership for Logical {
        fn of(&self, metadata: &Metadata) -> Owner {
            self.owners
                .borrow()
                .get(&(metadata.dev(), metadata.ino()))
                .copied()
                .unwrap_or(self.root)
        }

        fn give(&self, file: &File, owner: Owner) -> io::Result<()> {
            self.set(&file.metadata()?, owner);
            Ok(())
        }

        fn give_link(&self, path: &Path, owner: Owner) -> io::Result<()> {
            self.set(&fs::symlink_metadata(path)?, owner);
            Ok(())
        }
    }

    const ROOT: Owner = (0, 0);
    const ACCOUNT: Owner = (66560, 66560);
    const HUMAN: Owner = (1000, 1000);

    /// A scratch `applications` tree owned by root, and a human home.
    struct Homes {
        temp: td_fs::TempDir,
        ids: Logical,
        root: Owner,
        account: Owner,
        human: ApplicationHome,
    }

    impl Homes {
        fn new() -> Self {
            let temp = td_fs::TempDir::new("td-app-homes").unwrap();
            let homes = Self {
                human: ApplicationHome {
                    home: temp.path().join("human"),
                    uid: HUMAN.0,
                    gid: HUMAN.1,
                },
                temp,
                ids: Logical {
                    root: ROOT,
                    owners: Default::default(),
                },
                root: ROOT,
                account: ACCOUNT,
            };
            homes.left(&homes.human.home, HUMAN);
            homes
        }

        fn place(&self) -> Place<'_> {
            Place {
                top: self.temp.path().to_str().unwrap(),
                chain: &[],
                root: self.root,
                ids: &self.ids,
            }
        }

        fn home(&self) -> PathBuf {
            self.temp
                .path()
                .join("applications")
                .join(self.account.0.to_string())
        }

        fn create(&self, fresh: bool) -> Result<(), String> {
            home(&self.place(), None, "photo", self.account, fresh)
        }

        fn boot(&self, application: &str, uid: u32) -> Result<(), String> {
            boot(&self.place(), &self.human, application, uid, self.account)
        }

        fn set(&self, path: &Path, owner: Owner) {
            self.ids.set(&fs::symlink_metadata(path).unwrap(), owner);
        }

        /// A directory at `path` owned by `owner`, as a crashed run left it.
        fn left(&self, path: &Path, owner: Owner) {
            fs::DirBuilder::new().mode(0o700).create(path).unwrap();
            self.set(path, owner);
        }

        /// A file at `path` holding the human's bytes, owned by `owner`.
        fn record(&self, path: &Path, owner: Owner) {
            fs::write(path, b"the human's").unwrap();
            self.set(path, owner);
        }

        /// The human's legacy `.td/app/APP`, holding one record.
        fn legacy(&self, application: &str) -> PathBuf {
            let mut path = self.human.home.clone();
            for name in [".td", "app", application] {
                path = path.join(name);
                self.left(&path, HUMAN);
            }
            let record = path.join("record");
            self.record(&record, HUMAN);
            record
        }

        fn owner(&self, path: &Path) -> Owner {
            self.ids.of(&fs::symlink_metadata(path).unwrap())
        }
    }

    /// An install's home is empty and the account's alone; a reinstall finds
    /// it as it stands, and a new row refuses it as another's leftover.
    #[test]
    fn an_install_creates_an_empty_home_of_the_accounts_own() {
        let homes = Homes::new();
        homes.create(true).unwrap();
        let home = homes.home();
        for path in [
            home.clone(),
            home.join(".td"),
            home.join(".td/app"),
            home.join(".td/app/photo"),
        ] {
            assert_eq!(homes.owner(&path), homes.account, "{}", path.display());
            assert_eq!(fs::metadata(&path).unwrap().mode() & 0o7777, 0o700);
        }
        assert_eq!(fs::read_dir(home.join(".td/app/photo")).unwrap().count(), 0);
        fs::write(home.join(".td/app/photo/kept"), b"state").unwrap();
        homes.create(false).unwrap();
        assert_eq!(fs::read(home.join(".td/app/photo/kept")).unwrap(), b"state");
        assert!(homes.create(true).is_err());
        assert_eq!(fs::read(home.join(".td/app/photo/kept")).unwrap(), b"state");
    }

    /// A run killed before publication leaves root-owned state, which a
    /// retry (the row already published, so not fresh) completes; a new row
    /// refuses even that.
    #[test]
    fn a_retry_completes_root_owned_interrupted_creation() {
        let homes = Homes::new();
        let home = homes.home();
        let applications = homes.temp.path().join("applications");
        homes.left(&applications, homes.root);
        fs::set_permissions(&applications, Permissions::from_mode(0o755)).unwrap();
        for path in [
            home.clone(),
            home.join(".td"),
            home.join(".td/app"),
            home.join(".td/app/photo"),
        ] {
            homes.left(&path, homes.root);
        }
        assert!(homes.create(true).is_err());
        assert_eq!(homes.owner(&home), homes.root);
        homes.create(false).unwrap();
        for path in [home.clone(), home.join(".td/app/photo")] {
            assert_eq!(homes.owner(&path), homes.account);
        }
    }

    /// The human's inodes inside an unpublished home, as an interrupted
    /// migration leaves them, are refused by a creation that migrates
    /// nothing, and stay as they were.
    #[test]
    fn creation_refuses_the_humans_inodes_in_an_unpublished_home() {
        let homes = Homes::new();
        let human = HUMAN;
        let home = homes.home();
        homes.left(&homes.temp.path().join("applications"), homes.root);
        fs::set_permissions(
            homes.temp.path().join("applications"),
            Permissions::from_mode(0o755),
        )
        .unwrap();
        for path in [home.clone(), home.join(".td"), home.join(".td/app")] {
            homes.left(&path, homes.root);
        }
        let moved = home.join(".td/app/photo");
        homes.left(&moved, human);
        let record = moved.join("record");
        homes.record(&record, human);
        assert!(homes.create(false).is_err());
        assert!(homes.boot("photo", 66560).is_err());
        assert_eq!(homes.owner(&home), homes.root);
        assert_eq!(homes.owner(&record), human);
        // Even a root-owned staging entry holding the human's file.
        fs::remove_file(&record).unwrap();
        homes.set(&moved, homes.root);
        homes.record(&record, human);
        assert!(homes.create(false).is_err());
        assert_eq!(homes.owner(&record), human);
    }

    /// Boot never moves the human's legacy state into an allocated account:
    /// not over the install's published home, which it accepts at once, and
    /// not over its interrupted one. Only a carried account's boot does.
    #[test]
    fn boot_migrates_the_humans_state_only_into_a_carried_account() {
        let homes = Homes::new();
        let record = homes.legacy("photo");
        homes.create(true).unwrap();
        homes.boot("photo", 66560).unwrap();
        assert_eq!(fs::read(&record).unwrap(), b"the human's");
        assert_eq!(
            fs::read_dir(homes.home().join(".td/app/photo"))
                .unwrap()
                .count(),
            0
        );

        let homes = Homes::new();
        let record = homes.legacy("photo");
        let home = homes.home();
        homes.left(&homes.temp.path().join("applications"), homes.root);
        fs::set_permissions(
            homes.temp.path().join("applications"),
            Permissions::from_mode(0o755),
        )
        .unwrap();
        homes.left(&home, homes.root);
        homes.boot("photo", 66560).unwrap();
        assert_eq!(fs::read(&record).unwrap(), b"the human's");
        assert_eq!(homes.owner(&home), homes.account);
        assert_eq!(fs::read_dir(home.join(".td/app/photo")).unwrap().count(), 0);

        let homes = Homes::new();
        let record = homes.legacy("firefox");
        homes.boot("firefox", 65536).unwrap();
        assert!(!record.exists());
        let moved = homes.home().join(".td/app/firefox/record");
        assert_eq!(fs::read(&moved).unwrap(), b"the human's");
        assert_eq!(homes.owner(&moved), homes.account);

        assert!(migrates(65536) && migrates(65539) && migrates(66559));
        assert!(!migrates(66560) && !migrates(98303));
    }

    #[test]
    fn conversion_preserves_bytes_modes_and_links_without_following_them() {
        let fixture = Fixture::new();
        fs::create_dir(fixture.0.join("profile")).unwrap();
        let record = fixture.0.join("profile/cookies");
        fs::write(&record, b"opaque application state\0\xff").unwrap();
        fs::set_permissions(&record, Permissions::from_mode(0o600)).unwrap();
        let external = Fixture::new();
        let outside = external.0.join("untouched");
        fs::write(&outside, b"outside").unwrap();
        fs::set_permissions(&outside, Permissions::from_mode(0o640)).unwrap();
        std::os::unix::fs::symlink(&outside, fixture.0.join("lock")).unwrap();
        for _ in 0..2 {
            fixture.convert(100, 0).unwrap();
            assert_eq!(
                fs::read(&record).unwrap(),
                b"opaque application state\0\xff"
            );
            assert_eq!(fs::metadata(&record).unwrap().mode() & 0o7777, 0o600);
            assert_eq!(fs::read_link(fixture.0.join("lock")).unwrap(), outside);
            assert_eq!(fs::metadata(&outside).unwrap().mode() & 0o7777, 0o640);
            assert_eq!(fs::read(&outside).unwrap(), b"outside");
        }
    }

    #[test]
    fn preflight_checks_a_tree_without_changing_ownership_or_bytes() {
        let fixture = Fixture::new();
        let record = fixture.0.join("preferences");
        fs::write(&record, b"retained profile").unwrap();
        let directory = directory(&fixture.0).unwrap();
        let metadata = directory.metadata().unwrap();
        let owner = (metadata.uid(), metadata.gid());
        let original = fs::metadata(&record).unwrap();
        walk(
            &Inodes,
            &directory,
            owner,
            (owner.0.wrapping_add(1), owner.1),
            metadata.dev(),
            0,
            &mut 10,
            false,
        )
        .unwrap();
        let after = fs::metadata(&record).unwrap();
        assert_eq!(
            (after.uid(), after.gid(), after.mode()),
            (original.uid(), original.gid(), original.mode())
        );
        assert_eq!(fs::read(&record).unwrap(), b"retained profile");
        fs::hard_link(&record, fixture.0.join("alias")).unwrap();
        assert!(walk(
            &Inodes,
            &directory,
            owner,
            owner,
            metadata.dev(),
            0,
            &mut 10,
            false
        )
        .is_err());
        assert!(record.exists());
    }

    #[test]
    fn aliases_privilege_bits_and_bounds_refuse_conversion() {
        let fixture = Fixture::new();
        let file = fixture.0.join("file");
        fs::write(&file, b"retained").unwrap();
        fs::hard_link(&file, fixture.0.join("alias")).unwrap();
        assert!(fixture.convert(100, 0).is_err());
        fs::remove_file(fixture.0.join("alias")).unwrap();
        fs::set_permissions(&file, Permissions::from_mode(0o4600)).unwrap();
        assert!(fixture.convert(100, 0).is_err());
        fs::set_permissions(&file, Permissions::from_mode(0o600)).unwrap();
        assert!(fixture.convert(0, 0).is_err());
        assert!(fixture.convert(100, MAX_DEPTH + 1).is_err());
        fixture.convert(100, 0).unwrap();
        assert_eq!(fs::read(&file).unwrap(), b"retained");
        let metadata = fs::metadata(&file).unwrap();
        let owner = (metadata.uid(), metadata.gid());
        assert!(check_inode(
            &Inodes,
            &metadata,
            (u32::MAX, 0),
            (0, u32::MAX),
            metadata.dev()
        )
        .is_err());
        assert!(check_inode(
            &Inodes,
            &metadata,
            owner,
            owner,
            metadata.dev().wrapping_add(1)
        )
        .is_err());
    }
}

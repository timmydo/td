//! The login-state predicate (td-login/TOKEN-LOGIN.md, "The login record"):
//! whether `<root>/var/lib/td/login` is a valid directory and whether the
//! record's name is in it. It reads no record bytes. td-secret's record
//! store, td-firstboot and td-authd compile this one file, and td-login
//! will (TOKEN-LOGIN.md increment 4's C5); it uses std alone.
#![forbid(unsafe_code)]

use std::fs::{self, File, Metadata, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};

/// The production directory, under `/`.
pub const DIRECTORY: &str = "/var/lib/td/login";
/// Every temporary name starts with this; no other td name does.
pub const TEMPORARY: &str = "tmp-";
/// Where a held descriptor is named again as a path.
pub const FD_ROOT: &str = "/proc/self/fd";
pub const NOFOLLOW: i32 = 0o400000;
pub const OPEN_DIRECTORY: i32 = 0o200000;
const ENOTDIR: i32 = 20;
const ELOOP: i32 = 40;

/// The owner the directory and record must have: root:root in production.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Owner {
    pub uid: u32,
    pub gid: u32,
}

impl Owner {
    pub const ROOT: Self = Self { uid: 0, gid: 0 };
}

/// TOKEN-LOGIN.md's three causes of the unavailable state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cause {
    DirectoryDamaged,
    RecordDamaged,
    Unreadable,
}

/// The directory-and-name answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// A valid directory without the record's name.
    Unenrolled,
    /// The record's name exists; only the record store reads what it holds.
    Enrolled,
    Unavailable(Cause),
}

/// Why a walk or a directory check refused: the state's cause, and one
/// line saying what was refused.
#[derive(Debug, PartialEq, Eq)]
pub struct Refusal {
    pub cause: Cause,
    pub reason: String,
}

impl Refusal {
    fn damaged(reason: String) -> Self {
        Self {
            cause: Cause::DirectoryDamaged,
            reason,
        }
    }

    fn unreadable(reason: String) -> Self {
        Self {
            cause: Cause::Unreadable,
            reason,
        }
    }
}

/// Metadata the checks read; separate so tests can state an owner they cannot create.
pub struct Facts {
    pub directory: bool,
    pub regular: bool,
    pub mode: u32,
    pub links: u64,
    pub uid: u32,
    pub gid: u32,
}

impl Facts {
    pub fn of(meta: &Metadata) -> Self {
        Self {
            directory: meta.is_dir(),
            regular: meta.is_file(),
            mode: meta.mode() & 0o7777,
            links: meta.nlink(),
            uid: meta.uid(),
            gid: meta.gid(),
        }
    }

    pub fn owned(&self, owner: Owner) -> bool {
        self.uid == owner.uid && self.gid == owner.gid
    }

    /// A removed directory has no links; its descriptor must not read as empty.
    pub fn valid_directory(&self, owner: Owner) -> bool {
        self.owned(owner) && self.directory && self.mode == 0o700 && self.links != 0
    }

    pub fn valid_record(&self, owner: Owner) -> bool {
        self.owned(owner) && self.regular && self.mode == 0o600 && self.links == 1
    }

    /// What `valid_directory` refused, for a console line.
    fn directory_fault(&self, owner: Owner) -> String {
        if !self.directory {
            "is not a directory".into()
        } else if !self.owned(owner) {
            format!(
                "is owned by {}:{}, not {}:{}",
                self.uid, self.gid, owner.uid, owner.gid
            )
        } else if self.mode != 0o700 {
            format!("has mode {:04o}, not 0700", self.mode)
        } else {
            "has been removed".into()
        }
    }
}

/// A missing name, a link or a non-directory on the path is the damage; any
/// other failure is transient.
pub fn classify(error: &io::Error, damage: Cause) -> Cause {
    if error.kind() == io::ErrorKind::NotFound
        || matches!(error.raw_os_error(), Some(ENOTDIR | ELOOP))
    {
        damage
    } else {
        Cause::Unreadable
    }
}

/// The refusal of opening the directory `at`, shown as `path`. With
/// `O_DIRECTORY` a link fails as a non-directory does, so the reason looks
/// at the name again.
pub fn refuse_open(error: &io::Error, at: &Path, path: &Path) -> Refusal {
    let cause = classify(error, Cause::DirectoryDamaged);
    let reason = if error.kind() == io::ErrorKind::NotFound {
        format!("{path:?} is missing")
    } else if matches!(error.raw_os_error(), Some(ENOTDIR | ELOOP)) {
        let link = fs::symlink_metadata(at).is_ok_and(|meta| meta.file_type().is_symlink());
        if link {
            format!("{path:?} is a symbolic link")
        } else {
            format!("{path:?} is not a directory")
        }
    } else {
        format!("open {path:?}: {error}")
    };
    Refusal { cause, reason }
}

/// The login directory under `root`.
pub fn directory(root: &Path) -> PathBuf {
    root.join(DIRECTORY.strip_prefix('/').unwrap_or(DIRECTORY))
}

pub fn same_inode(a: &Metadata, b: &Metadata) -> bool {
    a.dev() == b.dev() && a.ino() == b.ino()
}

fn at(fd_root: &Path, directory: &File, name: impl AsRef<Path>) -> PathBuf {
    fd_root.join(directory.as_raw_fd().to_string()).join(name)
}

/// Whether `directory`'s descriptor path names it: a lost or replaced
/// `/proc` must not make a name look absent.
fn resolves(fd_root: &Path, directory: &File, shown: &Path) -> Result<(), Refusal> {
    let lost = |detail: String| {
        Refusal::unreadable(format!(
            "{fd_root:?} does not name the descriptor of {shown:?}{detail}"
        ))
    };
    let named = fs::metadata(at(fd_root, directory, "")).map_err(|e| lost(format!(": {e}")))?;
    let held = directory
        .metadata()
        .map_err(|e| Refusal::unreadable(format!("inspect {shown:?}: {e}")))?;
    if same_inode(&named, &held) {
        Ok(())
    } else {
        Err(lost(String::new()))
    }
}

/// Opens the absolute `path` from `/` one component at a time, each with
/// `O_DIRECTORY | O_NOFOLLOW` through the previous one's descriptor path,
/// so a link anywhere on it refuses. A missing component, a link or a
/// non-directory is damage only while the parent's descriptor path still
/// names the parent; otherwise it is unreadable.
pub fn walk(fd_root: &Path, path: &Path) -> Result<File, Refusal> {
    let open = |at: &Path, shown: &Path| {
        OpenOptions::new()
            .read(true)
            .custom_flags(OPEN_DIRECTORY | NOFOLLOW)
            .open(at)
            .map_err(|error| refuse_open(&error, at, shown))
    };
    let mut components = path.components();
    if components.next() != Some(Component::RootDir) {
        return Err(Refusal::damaged(format!("{path:?} is not absolute")));
    }
    let mut shown = PathBuf::from("/");
    let mut current = open(&shown, &shown)?;
    for component in components {
        let Component::Normal(name) = component else {
            return Err(Refusal::damaged(format!("{path:?} names a parent")));
        };
        let parent = shown.clone();
        shown.push(name);
        current = match open(&at(fd_root, &current, name), &shown) {
            Err(refusal) if refusal.cause == Cause::DirectoryDamaged => {
                resolves(fd_root, &current, &parent)?;
                return Err(refusal);
            }
            opened => opened?,
        };
    }
    Ok(current)
}

/// The login directory, pinned by descriptor from the walk that opened it.
pub struct Directory {
    file: File,
    owner: Owner,
    /// `FD_ROOT`; tests replace it to stand for a lost or replaced `/proc`.
    fd_root: PathBuf,
    path: PathBuf,
}

impl Directory {
    /// Walks `path` and checks the directory it reaches.
    pub fn open(path: &Path, owner: Owner) -> Result<Self, Refusal> {
        Self::open_via(Path::new(FD_ROOT), path, owner)
    }

    pub fn open_via(fd_root: &Path, path: &Path, owner: Owner) -> Result<Self, Refusal> {
        let directory = Self {
            file: walk(fd_root, path)?,
            owner,
            fd_root: fd_root.to_path_buf(),
            path: path.to_path_buf(),
        };
        directory.check()?;
        Ok(directory)
    }

    /// The owner, type, mode and links, read again through the descriptor.
    pub fn check(&self) -> Result<(), Refusal> {
        let meta = self
            .file
            .metadata()
            .map_err(|e| Refusal::unreadable(format!("inspect {:?}: {e}", self.path)))?;
        let facts = Facts::of(&meta);
        if facts.valid_directory(self.owner) {
            Ok(())
        } else {
            Err(Refusal::damaged(format!(
                "{:?} {}",
                self.path,
                facts.directory_fault(self.owner)
            )))
        }
    }

    pub fn resolves(&self) -> Result<(), Refusal> {
        resolves(&self.fd_root, &self.file, &self.path)
    }

    pub fn at(&self, name: impl AsRef<Path>) -> PathBuf {
        at(&self.fd_root, &self.file, name)
    }

    pub fn file(&self) -> &File {
        &self.file
    }

    pub fn owner(&self) -> Owner {
        self.owner
    }

    /// Inspects only the exact `name`, never opening it. A missing name
    /// counts only while the directory's descriptor path names it.
    pub fn lookup(&self, name: &str) -> Result<Option<Metadata>, Refusal> {
        self.check()?;
        match fs::symlink_metadata(self.at(name)) {
            Ok(meta) => Ok(Some(meta)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                self.resolves()?;
                Ok(None)
            }
            Err(error) => Err(Refusal::unreadable(format!(
                "inspect {name:?} in {:?}: {error}",
                self.path
            ))),
        }
    }

    /// Unlinks every name starting with `TEMPORARY`, and only those,
    /// syncing the directory if any went.
    pub fn remove_temporaries(&self) -> Result<(), String> {
        self.check()
            .and_then(|()| self.resolves())
            .map_err(|refusal| refusal.reason)?;
        let entries =
            fs::read_dir(self.at("")).map_err(|e| format!("list {:?}: {e}", self.path))?;
        let mut removed = false;
        for entry in entries {
            let name = entry
                .map_err(|e| format!("list {:?}: {e}", self.path))?
                .file_name();
            if !name.as_bytes().starts_with(TEMPORARY.as_bytes()) {
                continue;
            }
            fs::remove_file(self.at(&name))
                .map_err(|e| format!("unlink {name:?} in {:?}: {e}", self.path))?;
            removed = true;
        }
        if removed {
            self.file
                .sync_all()
                .map_err(|e| format!("sync {:?}: {e}", self.path))?;
        }
        Ok(())
    }

    #[cfg(test)]
    pub fn set_fd_root(&mut self, fd_root: PathBuf) {
        self.fd_root = fd_root;
    }
}

/// The predicate: the login state under `root`, for UID `uid`'s record.
pub fn state(root: &Path, uid: u32) -> State {
    state_as(root, Owner::ROOT, uid)
}

/// `state` with the owner a parameter, for tests that cannot create root's.
pub fn state_as(root: &Path, owner: Owner, uid: u32) -> State {
    match Directory::open(&directory(root), owner).and_then(|d| d.lookup(&uid.to_string())) {
        Ok(None) => State::Unenrolled,
        Ok(Some(_)) => State::Enrolled,
        Err(refusal) => State::Unavailable(refusal.cause),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
    use super::*;
    use std::fs::Permissions;
    use std::os::unix::fs::{symlink, DirBuilderExt, PermissionsExt};
    use std::os::unix::net::UnixListener;
    use std::sync::atomic::{AtomicU64, Ordering};

    const UID: u32 = 1000;

    /// A temporary root holding `var/lib/td/login`, mode 0700, owned by the
    /// test's own IDs.
    struct Root {
        root: PathBuf,
        owner: Owner,
    }

    impl Root {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let root = std::env::temp_dir().join(format!(
                "td-login-state-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
            fs::DirBuilder::new()
                .mode(0o755)
                .recursive(true)
                .create(root.join("var/lib/td"))
                .unwrap();
            fs::DirBuilder::new()
                .mode(0o700)
                .create(directory(&root))
                .unwrap();
            let meta = fs::metadata(&root).unwrap();
            Self {
                root,
                owner: Owner {
                    uid: meta.uid(),
                    gid: meta.gid(),
                },
            }
        }

        fn login(&self) -> PathBuf {
            directory(&self.root)
        }

        fn state(&self) -> State {
            state_as(&self.root, self.owner, UID)
        }
    }

    impl Drop for Root {
        fn drop(&mut self) {
            let _ = fs::set_permissions(self.login(), Permissions::from_mode(0o700));
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn the_directory_is_the_production_path_under_each_root() {
        assert_eq!(DIRECTORY, "/var/lib/td/login");
        assert_eq!(directory(Path::new("/")), Path::new(DIRECTORY));
        assert_eq!(
            directory(Path::new("/sysroot")),
            Path::new("/sysroot/var/lib/td/login")
        );
        assert_eq!(Owner::ROOT, Owner { uid: 0, gid: 0 });
        assert_eq!(TEMPORARY, "tmp-");
    }

    #[test]
    fn unenrolled_is_a_valid_directory_without_the_name() {
        let root = Root::new();
        assert_eq!(root.state(), State::Unenrolled);
        // Other entries, temporaries and another account's name are not the record.
        for name in ["tmp-0123", "cutover-reboot", "1001", "10000", "100"] {
            fs::write(root.login().join(name), b"").unwrap();
        }
        assert_eq!(root.state(), State::Unenrolled);
    }

    #[test]
    fn enrolled_is_the_record_name_whatever_it_holds() {
        let root = Root::new();
        let record = root.login().join(UID.to_string());
        // Nothing about the name's bytes or type makes it read as absent.
        fs::write(&record, b"not a record").unwrap();
        assert_eq!(root.state(), State::Enrolled);
        fs::remove_file(&record).unwrap();
        fs::write(&record, b"").unwrap();
        fs::set_permissions(&record, Permissions::from_mode(0o644)).unwrap();
        assert_eq!(root.state(), State::Enrolled);
        fs::remove_file(&record).unwrap();
        symlink("missing", &record).unwrap();
        assert_eq!(root.state(), State::Enrolled);
        fs::remove_file(&record).unwrap();
        fs::create_dir(&record).unwrap();
        assert_eq!(root.state(), State::Enrolled);
        fs::remove_dir(&record).unwrap();
        let listener = UnixListener::bind(&record).unwrap();
        assert_eq!(root.state(), State::Enrolled);
        drop(listener);
        fs::remove_file(&record).unwrap();
        assert_eq!(root.state(), State::Unenrolled);
    }

    #[test]
    fn a_damaged_directory_is_unavailable_with_its_reason() {
        let damaged = State::Unavailable(Cause::DirectoryDamaged);
        let root = Root::new();
        let login = root.login();
        // Any mode but 0700, the setuid, setgid and sticky bits included.
        for mode in [0o750, 0o701, 0o755, 0o500, 0o1700, 0o2700, 0o4700] {
            fs::set_permissions(&login, Permissions::from_mode(mode)).unwrap();
            assert_eq!(root.state(), damaged, "{mode:o}");
            let refusal = Directory::open(&login, root.owner).err().unwrap();
            assert_eq!(
                refusal.reason,
                format!("{login:?} has mode {mode:04o}, not 0700")
            );
        }
        fs::set_permissions(&login, Permissions::from_mode(0o700)).unwrap();
        // Another owner or group.
        for other in [
            Owner {
                uid: root.owner.uid ^ 1,
                ..root.owner
            },
            Owner {
                gid: root.owner.gid ^ 1,
                ..root.owner
            },
        ] {
            assert_eq!(state_as(&root.root, other, UID), damaged);
            let refusal = Directory::open(&login, other).err().unwrap();
            assert_eq!(refusal.cause, Cause::DirectoryDamaged);
            assert!(refusal.reason.contains("is owned by"), "{}", refusal.reason);
        }
        // The production owner is root's, which the test's directory is not
        // unless the test runs as root.
        if root.owner != Owner::ROOT {
            assert_eq!(state(&root.root, UID), damaged);
        }
        // Missing, a file, a link at the leaf, a link above it.
        let held = root.root.join("held");
        fs::rename(&login, &held).unwrap();
        assert_eq!(root.state(), damaged);
        let refusal = Directory::open(&login, root.owner).err().unwrap();
        assert_eq!(refusal.reason, format!("{login:?} is missing"));
        fs::write(&login, b"").unwrap();
        assert_eq!(root.state(), damaged);
        let refusal = Directory::open(&login, root.owner).err().unwrap();
        assert_eq!(refusal.reason, format!("{login:?} is not a directory"));
        fs::remove_file(&login).unwrap();
        symlink(&held, &login).unwrap();
        assert_eq!(root.state(), damaged);
        let refusal = Directory::open(&login, root.owner).err().unwrap();
        assert_eq!(refusal.reason, format!("{login:?} is a symbolic link"));
        fs::remove_file(&login).unwrap();
        fs::rename(&held, &login).unwrap();
        assert_eq!(root.state(), State::Unenrolled);
        let td = root.root.join("var/lib/td");
        let real = root.root.join("var/lib/real");
        fs::rename(&td, &real).unwrap();
        symlink("real", &td).unwrap();
        assert_eq!(root.state(), damaged);
        let refusal = Directory::open(&login, root.owner).err().unwrap();
        assert_eq!(refusal.reason, format!("{td:?} is a symbolic link"));
        fs::remove_file(&td).unwrap();
        fs::rename(&real, &td).unwrap();
        // A root that is relative, or that names a parent.
        assert_eq!(state_as(Path::new("relative"), root.owner, UID), damaged);
        assert_eq!(
            state_as(&root.root.join("var/.."), root.owner, UID),
            damaged
        );
        assert_eq!(root.state(), State::Unenrolled);
    }

    #[test]
    fn a_lost_or_replaced_proc_is_unreadable_never_absent() {
        let root = Root::new();
        let unreadable = Some(Cause::Unreadable);
        let lost = root.root.join("lost");
        assert_eq!(
            Directory::open_via(&lost, &root.login(), root.owner)
                .err()
                .map(|refusal| refusal.cause),
            unreadable
        );
        let mut directory = Directory::open(&root.login(), root.owner).unwrap();
        assert_eq!(
            directory.lookup("1000").map(|meta| meta.is_some()),
            Ok(false)
        );
        directory.set_fd_root(lost);
        assert_eq!(directory.lookup("1000").err().map(|r| r.cause), unreadable);
        assert!(directory.remove_temporaries().is_err());
    }

    #[test]
    fn only_a_missing_name_a_link_or_a_non_directory_is_damage() {
        for (code, cause) in [
            (2, Cause::DirectoryDamaged),
            (ENOTDIR, Cause::DirectoryDamaged),
            (ELOOP, Cause::DirectoryDamaged),
            (5, Cause::Unreadable),
            (13, Cause::Unreadable),
            (24, Cause::Unreadable),
        ] {
            let error = io::Error::from_raw_os_error(code);
            assert_eq!(classify(&error, Cause::DirectoryDamaged), cause, "{code}");
        }
    }

    #[test]
    fn only_temporaries_are_removed() {
        let root = Root::new();
        let login = root.login();
        let kept = [
            ".tmp-1",
            "1000",
            "1001",
            "TMP-1",
            "cutover-reboot",
            "tmp",
            "tmp_1",
            "xtmp-1",
        ];
        for name in kept {
            fs::write(login.join(name), name).unwrap();
        }
        fs::write(root.root.join("victim"), b"outside").unwrap();
        symlink(root.root.join("victim"), login.join("tmp-link")).unwrap();
        for name in ["tmp-", "tmp-0123456789abcdef", "tmp-whole"] {
            fs::write(login.join(name), b"x").unwrap();
        }
        let directory = Directory::open(&login, root.owner).unwrap();
        directory.remove_temporaries().unwrap();
        let mut names: Vec<String> = fs::read_dir(&login)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        assert_eq!(names, kept);
        for name in kept {
            assert_eq!(fs::read(login.join(name)).unwrap(), name.as_bytes());
        }
        assert_eq!(fs::read(root.root.join("victim")).unwrap(), b"outside");
        // A prefixed name that cannot be unlinked refuses, by name.
        fs::create_dir(login.join("tmp-dir")).unwrap();
        let error = directory.remove_temporaries().unwrap_err();
        assert!(error.starts_with("unlink \"tmp-dir\" in "), "{error}");
        assert!(login.join("tmp-dir").is_dir());
    }
}

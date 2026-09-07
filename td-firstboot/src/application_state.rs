//! Move existing application state behind a private, root-controlled home.

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

fn owned(metadata: &Metadata, uid: u32) -> bool {
    metadata.uid() == uid && metadata.gid() == uid
}

fn invalid(why: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, why)
}

fn root_child(parent: &File, name: &str, mode: u32) -> io::Result<File> {
    staging_child(parent, name, mode, 0)
}

fn staging_child(parent: &File, name: &str, mode: u32, uid: u32) -> io::Result<File> {
    let path = at(parent, name);
    match fs::DirBuilder::new().mode(mode).create(&path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    let child = directory(&path)?;
    let metadata = child.metadata()?;
    if (!owned(&metadata, 0) && !owned(&metadata, uid)) || metadata.mode() & 0o7777 & !mode != 0 {
        return Err(invalid(
            "application staging directory is not private root-owned state",
        ));
    }
    child.set_permissions(Permissions::from_mode(mode))?;
    child.sync_all()?;
    parent.sync_all()?;
    Ok(child)
}

fn root_chain() -> io::Result<File> {
    let mut current = directory(Path::new("/"))?;
    for name in ["var", "lib", "td"] {
        let metadata = current.metadata()?;
        if !owned(&metadata, 0) || metadata.mode() & 0o022 != 0 {
            return Err(invalid(
                "application state ancestor permits an untrusted writer",
            ));
        }
        current = directory(&at(&current, name))?;
    }
    let metadata = current.metadata()?;
    if !owned(&metadata, 0) || metadata.mode() & 0o022 != 0 {
        return Err(invalid(
            "application state parent permits an untrusted writer",
        ));
    }
    root_child(&current, "applications", 0o755)
}

fn migration_lock(base: &File) -> io::Result<File> {
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
        || !owned(&metadata, 0)
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
    if !(1000..=65533).contains(&former.uid)
        || !(65536..=2147483647).contains(&uid)
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
    let result = (|| {
        let base = root_chain()?;
        let _migration = migration_lock(&base)?;
        let name = uid.to_string();
        match directory(&at(&base, &name)) {
            Ok(existing) => {
                let metadata = existing.metadata()?;
                if owned(&metadata, uid) && metadata.mode() & 0o7777 == 0o700 {
                    return Ok(());
                }
                if !owned(&metadata, 0) || metadata.mode() & 0o7777 & !0o700 != 0 {
                    return Err(invalid(
                        "application home has invalid ownership or permissions",
                    ));
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let private = root_child(&base, &name, 0o700)?;
        let state = staging_child(&private, ".td", 0o700, uid)?;
        let applications = staging_child(&state, "app", 0o700, uid)?;
        migrate(former, application, uid, &applications)?;
        for child in [&applications, &state] {
            std::os::unix::fs::fchown(child, Some(uid), Some(uid))?;
            child.sync_all()?;
        }
        // All child metadata and rename publications precede removal of this
        // root-owned barrier. Interrupted conversion stays inaccessible.
        std::os::unix::fs::fchown(&private, Some(uid), Some(uid))?;
        private.sync_all()?;
        base.sync_all()
    })();
    result.map_err(|error| format!("prepare application {application} state: {error}"))?;
    Ok(ApplicationHome {
        home: crate::principals::application_home(uid),
        uid,
        gid: uid,
    })
}

fn legacy_parent(former: &ApplicationHome) -> io::Result<Option<File>> {
    let mut current = match directory(&former.home) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    for name in [".td", "app"] {
        let metadata = current.metadata()?;
        if !matches_owner(&metadata, (former.uid, former.gid)) || metadata.mode() & 0o022 != 0 {
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
    if !matches_owner(&metadata, (former.uid, former.gid)) || metadata.mode() & 0o022 != 0 {
        return Err(invalid(
            "legacy application parent has another owner or permits other writers",
        ));
    }
    Ok(Some(current))
}

fn migrate(former: &ApplicationHome, application: &str, uid: u32, target: &File) -> io::Result<()> {
    let destination = at(target, application);
    let needs_source = match directory(&destination) {
        Ok(entry) if owned(&entry.metadata()?, 0) => {
            require_empty(&entry)?;
            true
        }
        Ok(_) => false,
        Err(error) if error.kind() == io::ErrorKind::NotFound => true,
        Err(error) => return Err(error),
    };
    if needs_source {
        if let Some(source) = legacy_parent(former)? {
            let path = at(&source, application);
            match directory(&path) {
                Ok(old) => {
                    let metadata = old.metadata()?;
                    if !matches_owner(&metadata, (former.uid, former.gid)) {
                        return Err(invalid("legacy application root has another owner"));
                    }
                    // Refuse malformed legacy state before removing its human
                    // pathname. Startup ordering excludes writers between passes.
                    walk(
                        &old,
                        (former.uid, former.gid),
                        (uid, uid),
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
            std::os::unix::fs::fchown(&empty, Some(uid), Some(uid))?;
            empty.sync_all()?;
            target.sync_all()?;
        }
    }
    let entry = directory(&destination)?;
    if owned(&entry.metadata()?, 0) {
        require_empty(&entry)?;
        entry.set_permissions(Permissions::from_mode(0o700))?;
        std::os::unix::fs::fchown(&entry, Some(uid), Some(uid))?;
        entry.sync_all()?;
    }
    let device = entry.metadata()?.dev();
    let mut remaining = MAX_ENTRIES;
    walk(
        &entry,
        (former.uid, former.gid),
        (uid, uid),
        device,
        0,
        &mut remaining,
        true,
    )?;
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

fn walk(
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
    check_inode(&directory.metadata()?, former, uid, device)?;
    for entry in fs::read_dir(at(directory, "."))? {
        *remaining = remaining
            .checked_sub(1)
            .ok_or_else(|| invalid("application state exceeds the migration entry bound"))?;
        let path = at(directory, entry?.file_name());
        let metadata = fs::symlink_metadata(&path)?;
        check_inode(&metadata, former, uid, device)?;
        if metadata.is_dir() {
            walk(
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
            check_inode(&file.metadata()?, former, uid, device)?;
            if change_owner {
                std::os::unix::fs::fchown(&file, Some(uid.0), Some(uid.1))?;
                file.sync_all()?;
            }
        } else if metadata.file_type().is_symlink() {
            // Preserve the link text without following it. The retained
            // root-owned home excludes writers throughout this boot migration.
            if change_owner {
                std::os::unix::fs::lchown(&path, Some(uid.0), Some(uid.1))?;
            }
        } else {
            return Err(invalid("application state contains a special file"));
        }
    }
    if change_owner {
        std::os::unix::fs::fchown(directory, Some(uid.0), Some(uid.1))?;
        directory.sync_all()?;
    }
    Ok(())
}

fn check_inode(
    metadata: &Metadata,
    former: (u32, u32),
    uid: (u32, u32),
    device: u64,
) -> io::Result<()> {
    if (!matches_owner(metadata, former) && !matches_owner(metadata, uid))
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

fn matches_owner(metadata: &Metadata, owner: (u32, u32)) -> bool {
    (metadata.uid(), metadata.gid()) == owner
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
        assert!(walk(&directory, owner, owner, metadata.dev(), 0, &mut 10, false).is_err());
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
        assert!(check_inode(&metadata, (u32::MAX, 0), (0, u32::MAX), metadata.dev()).is_err());
        assert!(check_inode(&metadata, owner, owner, metadata.dev().wrapping_add(1)).is_err());
    }
}

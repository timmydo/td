//! Early, serialized home preparation before any human process starts.

use std::fs::{self, File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::principals::{self, O_DIRECTORY, O_NOFOLLOW};

fn child(parent: &File, name: &str) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", parent.as_raw_fd())).join(name)
}

fn owned_directory(path: &Path, owner: (u32, u32)) -> Result<File, String> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(O_DIRECTORY | O_NOFOLLOW)
        .open(path)
        .map_err(|error| format!("open primary-home directory {}: {error}", path.display()))?;
    let metadata = file.metadata().map_err(|error| error.to_string())?;
    if (metadata.uid(), metadata.gid()) != owner {
        return Err(format!(
            "primary-home directory {} requires owner {}:{}",
            path.display(), owner.0, owner.1
        ));
    }
    Ok(file)
}

fn directory(path: &Path, owner: (u32, u32), mode: u32) -> Result<File, String> {
    let file = owned_directory(path, owner)?;
    if file.metadata().map_err(|error| error.to_string())?.mode() & 0o7777 != mode {
        return Err(format!("primary-home directory {} requires mode {mode:04o}", path.display()));
    }
    Ok(file)
}

/// The caller has authenticated ROOT and mounted its persistent /var. No
/// account consumer or competing privileged writer may run during this step.
pub(crate) fn prepare(root: &Path) -> Result<String, String> {
    let primary = principals::primary_in_root(root)
        .map_err(|error| format!("primary account in {}: {error}", root.display()))?;
    let logical_root = root;
    let root = directory(root, (0, 0), 0o755)?;
    let var = directory(&child(&root, "var"), (0, 0), 0o755)
        .map_err(|error| format!("{}: {error}", logical_root.join("var").display()))?;
    let homes = directory(&child(&var, "home"), (0, 0), 0o755)
        .map_err(|error| format!("{}: {error}", logical_root.join("var/home").display()))?;
    let uid = principals::primary_account::UID;
    let home = primary.persistent_home();
    ensure_home(&homes, primary.name(), (uid, uid))
        .map_err(|error| format!("{}: {error}", home.display()))?;
    home.to_str().map(str::to_owned).ok_or_else(|| "primary home is not UTF-8".into())
}

fn ensure_home(parent: &File, name: &str, owner: (u32, u32)) -> Result<(), String> {
    ensure_home_at(parent, name, owner, SystemTime::now())
}

fn ensure_home_at(parent: &File, name: &str, owner: (u32, u32), now: SystemTime) -> Result<(), String> {
    let destination = child(parent, name);
    match fs::symlink_metadata(&destination) {
        Ok(_) => {
            let home = owned_directory(&destination, owner)?;
            if home.metadata().map_err(|error| error.to_string())?.mode() & 0o7777 != 0o700 {
                home.set_permissions(fs::Permissions::from_mode(0o700))
                    .and_then(|()| home.sync_all())
                    .map_err(|error| format!("restore primary-home private mode: {error}"))?;
            }
            return Ok(());
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("inspect primary home: {error}")),
    }

    // Publish only after ownership, mode and durability are complete. A crash
    // can leave a hidden staging directory, never a half-owned final home.
    // Time and PID are only uniqueness hints; exclusive creation owns the
    // name. An unset/old clock must not prevent preparing a valid home.
    let stamp = now.duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
    for attempt in 0..64 {
        let temporary = child(parent, &format!(".td-primary-home-{}-{stamp}-{attempt}", std::process::id()));
        match fs::DirBuilder::new().mode(0o700).create(&temporary) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(format!("create primary-home staging: {error}")),
        }
        let result = (|| {
            // The root-owned parent excludes other writers; restore owner
            // access before opening when umask removed all permission bits.
            fs::set_permissions(&temporary, fs::Permissions::from_mode(0o700))
                .map_err(|error| format!("set primary-home staging mode: {error}"))?;
            let staged = OpenOptions::new()
                .read(true)
                .custom_flags(O_DIRECTORY | O_NOFOLLOW)
                .open(&temporary)
                .map_err(|error| format!("open primary-home staging: {error}"))?;
            std::os::unix::fs::fchown(&staged, Some(owner.0), Some(owner.1))
                .and_then(|()| staged.sync_all())
                .map_err(|error| format!("finish primary-home staging: {error}"))?;
            fs::rename(&temporary, &destination)
                .and_then(|()| parent.sync_all())
                .map_err(|error| format!("publish primary home: {error}"))
        })();
        if result.is_err() {
            // Only this call's empty staging name is eligible for cleanup.
            let _ = fs::remove_dir(&temporary);
        }
        return result;
    }
    Err("primary-home staging names exhausted".into())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> std::io::Result<Self> {
            static SERIAL: AtomicU64 = AtomicU64::new(0);
            let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
            for _ in 0..64 {
                let serial = SERIAL.fetch_add(1, Ordering::Relaxed);
                let path = std::env::temp_dir().join(format!(
                    "td-primary-home-{}-{stamp}-{serial}", std::process::id()
                ));
                match fs::DirBuilder::new().mode(0o700).create(&path) {
                    Ok(()) => return Ok(Self(path)),
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(error) => return Err(error),
                }
            }
            Err(std::io::Error::other("fixture names exhausted"))
        }

        fn parent(&self) -> File {
            File::open(&self.0).unwrap()
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn new_home_is_private_and_a_repeat_preserves_its_inode_and_contents() {
        let scratch = Scratch::new().unwrap();
        let parent = scratch.parent();
        let metadata = parent.metadata().unwrap();
        let owner = (metadata.uid(), metadata.gid());
        ensure_home(&parent, "alice", owner).unwrap();
        let home = scratch.0.join("alice");
        let before = fs::metadata(&home).unwrap();
        assert_eq!((before.uid(), before.gid(), before.mode() & 0o7777), (owner.0, owner.1, 0o700));
        fs::write(home.join("kept"), "persistent state").unwrap();
        ensure_home(&parent, "alice", owner).unwrap();
        assert_eq!(fs::metadata(&home).unwrap().ino(), before.ino());
        assert_eq!(fs::read_to_string(home.join("kept")).unwrap(), "persistent state");
        assert_eq!(fs::read_dir(&scratch.0).unwrap().count(), 1);
    }

    #[test]
    fn an_old_clock_and_an_existing_staging_name_do_not_replace_state() {
        let scratch = Scratch::new().unwrap();
        let parent = scratch.parent();
        let metadata = parent.metadata().unwrap();
        let owner = (metadata.uid(), metadata.gid());
        let stale = scratch.0.join(format!(".td-primary-home-{}-0-0", std::process::id()));
        fs::create_dir(&stale).unwrap();
        fs::write(stale.join("kept"), "unknown old state").unwrap();
        let old = UNIX_EPOCH.checked_sub(std::time::Duration::from_secs(1)).unwrap();
        ensure_home_at(&parent, "alice", owner, old).unwrap();
        assert_eq!(fs::read_to_string(stale.join("kept")).unwrap(), "unknown old state");
        assert!(directory(&scratch.0.join("alice"), owner, 0o700).is_ok());
        assert_eq!(fs::read_dir(&scratch.0).unwrap().count(), 2);
    }

    #[test]
    fn existing_owned_home_restores_private_mode_without_replacing_contents() {
        let scratch = Scratch::new().unwrap();
        let parent = scratch.parent();
        let metadata = parent.metadata().unwrap();
        let owner = (metadata.uid(), metadata.gid());
        ensure_home(&parent, "alice", owner).unwrap();
        let home = scratch.0.join("alice");
        fs::write(home.join("kept"), "persistent state").unwrap();
        let before = fs::metadata(&home).unwrap();
        for mode in [0o755, 0o777, 0o1700, 0o2700] {
            fs::set_permissions(&home, fs::Permissions::from_mode(mode)).unwrap();
            ensure_home(&parent, "alice", owner).unwrap();
            let retained = fs::metadata(&home).unwrap();
            assert_eq!(retained.ino(), before.ino());
            assert_eq!((retained.uid(), retained.gid()), owner);
            assert_eq!(retained.mode() & 0o7777, 0o700);
            assert_eq!(fs::read_to_string(home.join("kept")).unwrap(), "persistent state");
        }
    }

    #[test]
    fn existing_wrong_owner_or_alias_is_refused_without_repair() {
        let scratch = Scratch::new().unwrap();
        let parent = scratch.parent();
        let metadata = parent.metadata().unwrap();
        let owner = (metadata.uid(), metadata.gid());
        ensure_home(&parent, "alice", owner).unwrap();
        let home = scratch.0.join("alice");
        let before = fs::metadata(&home).unwrap();
        assert!(ensure_home(&parent, "alice", (owner.0.wrapping_add(1), owner.1)).is_err());
        assert_eq!(fs::metadata(&home).unwrap().uid(), owner.0);
        assert!(ensure_home(&parent, "alice", (owner.0, owner.1.wrapping_add(1))).is_err());
        assert_eq!(fs::metadata(&home).unwrap().ino(), before.ino());
        assert_eq!(fs::metadata(&home).unwrap().mode() & 0o7777, 0o700);
        std::os::unix::fs::symlink("alice", scratch.0.join("alias")).unwrap();
        assert!(ensure_home(&parent, "alias", owner).is_err());
        assert!(fs::symlink_metadata(scratch.0.join("alias")).unwrap().is_symlink());
        fs::write(scratch.0.join("file"), "retained").unwrap();
        assert!(ensure_home(&parent, "file", owner).is_err());
        assert_eq!(fs::read_to_string(scratch.0.join("file")).unwrap(), "retained");
    }
}

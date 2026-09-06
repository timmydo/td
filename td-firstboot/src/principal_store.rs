//! Durable UID reservations. Enrollment never releases an old assignment.

use crate::principals::{Registry, MAX_BYTES, O_DIRECTORY, O_NOFOLLOW, O_NONBLOCK};
use std::fs::{self, File, OpenOptions, Permissions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

const PATH_ONLY: i32 = 0x200000;
const LIMIT: u64 = MAX_BYTES as u64;
const RUNTIME_NAME: &str = "td-bus";
const LEDGER: &str = "principals.tsv";
const STAGED: &str = ".principals.next";
const LOCK: &str = ".principals.lock";

/// Publish broker runtimes only after the durable identity reservation passed.
pub(crate) fn prepare_broker_runtimes(registry: &Registry) -> Result<(), String> {
    let run = Directory::open(Path::new("/run"), 0, 0)?;
    let base = runtime_child(&run, RUNTIME_NAME, (0, 0))?;
    for session in registry.sessions() {
        runtime_child(
            &base,
            &session.owner.to_string(),
            (session.broker, session.broker),
        )?;
    }
    Ok(())
}

fn runtime_child(parent: &Directory, name: &str, owner: (u32, u32)) -> Result<Directory, String> {
    let path = parent.path(name);
    match fs::DirBuilder::new().mode(0o755).create(&path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(format!("create broker runtime {}: {error}", path.display())),
    }
    let metadata = fs::symlink_metadata(&path)
        .map_err(|error| format!("inspect broker runtime {}: {error}", path.display()))?;
    if !metadata.is_dir() || metadata.mode() & 0o7022 != 0 {
        return Err(format!(
            "broker runtime {} must be an unredirected directory without other writers",
            path.display()
        ));
    }
    if (metadata.uid(), metadata.gid()) == (parent.uid, parent.gid) {
        // The parent is root-controlled; resume a creation interrupted before
        // chown. The service cannot replace its entry in that parent.
        fs::set_permissions(&path, Permissions::from_mode(0o755))
            .map_err(|error| format!("finish broker runtime mode {}: {error}", path.display()))?;
        std::os::unix::fs::lchown(&path, Some(owner.0), Some(owner.1))
            .map_err(|error| format!("assign broker runtime {}: {error}", path.display()))?;
    }
    let child = Directory::open(&path, owner.0, owner.1)?;
    if child
        .file
        .metadata()
        .map_err(|error| format!("inspect broker runtime {}: {error}", path.display()))?
        .mode()
        & 0o7777
        != 0o755
    {
        return Err(format!(
            "broker runtime {} must have mode 0755",
            path.display()
        ));
    }
    Ok(child)
}

struct Directory {
    file: File,
    uid: u32,
    gid: u32,
}

impl Directory {
    fn open(path: &Path, uid: u32, gid: u32) -> Result<Self, String> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(O_NOFOLLOW | O_DIRECTORY)
            .open(path)
            .map_err(|error| format!("open principal directory {}: {error}", path.display()))?;
        let metadata = file.metadata().map_err(|error| error.to_string())?;
        if !metadata.is_dir()
            || metadata.uid() != uid
            || metadata.gid() != gid
            || metadata.mode() & 0o022 != 0
        {
            return Err("principal directory has the wrong owner or allows other writers".into());
        }
        Ok(Self { file, uid, gid })
    }

    fn path(&self, name: &str) -> PathBuf {
        PathBuf::from(format!("/proc/self/fd/{}/{name}", self.file.as_raw_fd()))
    }

    fn child(&self, name: &str) -> Result<Self, String> {
        Self::open(&self.path(name), self.uid, self.gid)
    }

    fn sync(&self) -> Result<(), String> {
        self.file
            .sync_all()
            .map_err(|error| format!("sync principal directory: {error}"))
    }

    fn validate(&self, file: &File) -> Result<(), String> {
        let metadata = file.metadata().map_err(|error| error.to_string())?;
        if !metadata.is_file()
            || metadata.uid() != self.uid
            || metadata.gid() != self.gid
            || metadata.mode() & 0o7777 != 0o600
            || metadata.nlink() != 1
            || metadata.len() > LIMIT
        {
            return Err(
                "principal file must be a bounded, single-link, private owned regular file".into(),
            );
        }
        Ok(())
    }

    fn read(&self, name: &str) -> Result<Option<String>, String> {
        let file = match OpenOptions::new()
            .read(true)
            .custom_flags(O_NOFOLLOW | O_NONBLOCK)
            .open(self.path(name))
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(format!("open principal file {name}: {error}")),
        };
        self.validate(&file)?;
        let mut text = String::new();
        file.take(LIMIT + 1)
            .read_to_string(&mut text)
            .map_err(|error| format!("read principal file {name}: {error}"))?;
        if text.len() as u64 > LIMIT {
            return Err("principal file grew beyond its limit".into());
        }
        Ok(Some(text))
    }

    fn private_scratch(&self, name: &str, limit: u64) -> Result<Option<File>, String> {
        let file = match OpenOptions::new()
            .read(true)
            .custom_flags(O_NOFOLLOW | PATH_ONLY)
            .open(self.path(name))
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(format!("open principal scratch {name}: {error}")),
        };
        let metadata = file.metadata().map_err(|error| error.to_string())?;
        if !metadata.is_file()
            || metadata.uid() != self.uid
            || metadata.gid() != self.gid
            || metadata.mode() & 0o7777 & !0o600 != 0
            || metadata.nlink() != 1
            || metadata.len() > limit
        {
            return Err("principal scratch must be one bounded, private owned regular file".into());
        }
        Ok(Some(file))
    }

    fn lock(&self) -> Result<File, String> {
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(O_NOFOLLOW | O_NONBLOCK)
            .open(self.path(LOCK))
        {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(format!("create principal lock: {error}")),
        }
        let pinned = self
            .private_scratch(LOCK, 0)?
            .ok_or("principal lock disappeared")?;
        let path = PathBuf::from(format!("/proc/self/fd/{}", pinned.as_raw_fd()));
        // A crash before chmod may leave owner bits masked off. Restore only
        // the validated empty lock inode, never the published ledger.
        fs::set_permissions(&path, Permissions::from_mode(0o600))
            .map_err(|error| format!("initialize principal lock: {error}"))?;
        // Following this live /proc descriptor is intentional; no caller path
        // or replaceable directory entry is resolved again.
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(O_NONBLOCK)
            .open(path)
            .map_err(|error| format!("open pinned principal lock: {error}"))?;
        self.validate(&file)?;
        file.lock()
            .map_err(|error| format!("lock principal enrollment: {error}"))?;
        Ok(file)
    }

    fn verify_enrolled(&self, desired: &Registry) -> Result<Registry, String> {
        let prior = Registry::parse(&self.read(LEDGER)?.ok_or("principal ledger is absent")?)?;
        if prior.enroll(desired)? != prior {
            return Err("deployment identities have not been enrolled".into());
        }
        Ok(prior)
    }

    #[cfg(test)]
    fn enroll(&self, desired: &Registry) -> Result<Registry, String> {
        self.enroll_checked(desired, |_| Ok(()))
    }

    fn enroll_checked(
        &self,
        desired: &Registry,
        validate: impl FnOnce(&Registry) -> Result<(), String>,
    ) -> Result<Registry, String> {
        let _lock = self.lock()?;
        let previous = self
            .read(LEDGER)?
            .map(|text| Registry::parse(&text))
            .transpose()?;
        let retained = match &previous {
            Some(prior) => prior.enroll(desired)?,
            None => desired.clone(),
        };
        validate(&retained)?;
        // A crash before rename may leave this private staging file. Never
        // follow a substitute or unlink a path with unverified ownership.
        if self.private_scratch(STAGED, LIMIT)?.is_some() {
            fs::remove_file(self.path(STAGED))
                .map_err(|error| format!("remove stale principal staging: {error}"))?;
            self.sync()?;
        }
        if previous.as_ref() == Some(&retained) {
            self.sync()?;
            return Ok(retained);
        }
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(O_NOFOLLOW | O_NONBLOCK)
            .open(self.path(STAGED))
            .map_err(|error| format!("create principal staging: {error}"))?;
        // Exact mode even when an inherited umask removes owner bits.
        file.set_permissions(Permissions::from_mode(0o600))
            .map_err(|error| error.to_string())?;
        self.validate(&file)?;
        let result = (|| {
            file.write_all(retained.encode().as_bytes())
                .map_err(|error| format!("write principal ledger: {error}"))?;
            file.sync_all()
                .map_err(|error| format!("sync principal ledger: {error}"))?;
            fs::rename(self.path(STAGED), self.path(LEDGER))
                .map_err(|error| format!("publish principal ledger: {error}"))?;
            self.sync()
        })();
        if result.is_err() {
            // The lock excludes protocol writers and the protected directory
            // excludes unprivileged replacement. Before rename this is our
            // staged inode; after rename the name is absent.
            let _ = fs::remove_file(self.path(STAGED));
        }
        result?;
        Ok(retained)
    }
}

fn state_directory() -> Result<Directory, String> {
    let mut status = String::new();
    File::open("/proc/self/status")
        .map_err(|error| error.to_string())?
        .take(8193)
        .read_to_string(&mut status)
        .map_err(|error| error.to_string())?;
    if status.len() > 8192
        || !["Uid:", "Gid:"].iter().all(|key| {
            status
                .lines()
                .find_map(|line| line.strip_prefix(key))
                .is_some_and(|value| {
                    let columns: Vec<&str> = value.split_whitespace().collect();
                    columns == ["0", "0", "0", "0"]
                })
        })
    {
        return Err("principal provisioning requires root credentials".into());
    }
    // Firstboot already created and synced this chain with its public
    // traversal modes. The ledger never creates or repairs shared parents.
    let state = Path::new(crate::DEFAULT_STATE_DIR);
    if !state.is_absolute() {
        return Err("principal state path must be absolute".into());
    }
    let mut root = Directory::open(Path::new("/"), 0, 0)?;
    for component in state.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(name) => {
                root = root.child(name.to_str().ok_or("principal state path is not UTF-8")?)?;
            }
            _ => return Err("principal state path is not canonical".into()),
        }
    }
    Ok(root)
}

pub(crate) fn provision(desired: &Registry) -> Result<(), String> {
    state_directory()?.enroll_checked(desired, |retained| {
        retained.verify_installed_accounts(desired)
    })?;
    Ok(())
}

pub(crate) fn check_launch_session(user: &str, owner: u32, compositor: u32) -> Result<(), String> {
    let desired = Registry::load()?;
    let prior = state_directory()?.verify_enrolled(&desired)?;
    prior.verify_installed_accounts(&desired)?;
    desired.verify_launch_session(user, owner, compositor)
}

#[cfg(test)]
#[path = "principal_store_tests.rs"]
mod tests;

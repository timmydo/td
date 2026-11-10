//! Durable UID reservations. Enrollment never releases an old assignment.

#[path = "../../engine/src/workload_accounts.rs"]
mod workload_accounts;

use crate::principals::{Registry, MAX_BYTES, O_DIRECTORY, O_NOFOLLOW, O_NONBLOCK};
use std::fs::{self, File, OpenOptions, Permissions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use workload_accounts::Ledger;

const PATH_ONLY: i32 = 0x200000;
const LIMIT: u64 = MAX_BYTES as u64;
const RUNTIME_NAME: &str = "td-bus";
const LEDGER: &str = "principals.tsv";
const STAGED: &str = ".principals.next";
const NOTEBOOK_LEDGER: &str = "principals-notebooks.tsv";
const NOTEBOOK_STAGED: &str = ".principals-notebooks.next";
const LOCK: &str = ".principals.lock";
const PRIVATE_MODE: u32 = 0o600;
/// The workload account ledger (td-authd/DESIGN.md, "Workload account
/// ledger"). It holds only kinds, names and uids, so every account reads it.
const ACCOUNTS_STAGED: &str = ".accounts.next";
const ACCOUNTS_MODE: u32 = 0o644;
const ACCOUNTS_LIMIT: u64 = workload_accounts::MAX_BYTES as u64;
/// `flathub_app::RUNTIME_STORE`'s last component, below `/var/lib/td`, and
/// the runtime account that owns it (`flathub_app::RUNTIME_UID`). This file
/// also builds alone in the recipe's principal tests, so it keeps a copy.
const RUNTIME_UID: u32 = 998;
const RUNTIME_STORE: &str = "runtimes";

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

/// The portal keeps credential and dialog temporaries out of the human runtime.
pub(crate) fn prepare_portal_runtimes(registry: &Registry) -> Result<(), String> {
    let run = Directory::open(Path::new("/run"), 0, 0)?;
    let base = runtime_child(&run, "td-portal", (0, 0))?;
    for session in registry.sessions() {
        runtime_child_mode(
            &base,
            &session.owner.to_string(),
            (session.portal, session.portal),
            0o700,
        )?;
    }
    Ok(())
}

/// Private vault storage is outside every human and application home.
/// The retained reservation is published before these directories exist.
pub(crate) fn prepare_notebooks(registry: &Registry) -> Result<(), String> {
    let var = Directory::open(Path::new("/var"), 0, 0)?;
    let lib = var.child("lib")?;
    let td = lib.child("td")?;
    let base = runtime_child(&td, "notebooks", (0, 0))?;
    for (owner, uid) in registry.notebooks() {
        runtime_child_mode(&base, &owner.to_string(), (uid, uid), 0o700)?.sync()?;
    }
    base.sync()?;
    td.sync()
}

/// The Flathub runtime store's root (WORKLOADS.md, "Runtimes"), below
/// `/var/lib/td`: the runtime account's home, where td-app running as it
/// keeps one store per runtime. Mode 0755, so every application account
/// reads the stores; only the runtime account writes it.
pub(crate) fn prepare_runtime_store() -> Result<(), String> {
    let var = Directory::open(Path::new("/var"), 0, 0)?;
    let lib = var.child("lib")?;
    let td = lib.child("td")?;
    let owner = RUNTIME_UID;
    runtime_child(&td, RUNTIME_STORE, (owner, owner))?.sync()?;
    td.sync()
}

/// Private client endpoints exist only for an installed application account.
pub(crate) fn prepare_application_runtime(uid: u32) -> Result<(), String> {
    if !(65536..=2147483647).contains(&uid) {
        return Err("invalid application runtime identity".into());
    }
    let run = Directory::open(Path::new("/run"), 0, 0)?;
    let users = runtime_child(&run, "user", (0, 0))?;
    runtime_child_mode(&users, &uid.to_string(), (uid, uid), 0o700)?;
    Ok(())
}

fn runtime_child(parent: &Directory, name: &str, owner: (u32, u32)) -> Result<Directory, String> {
    runtime_child_mode(parent, name, owner, 0o755)
}

fn runtime_child_mode(
    parent: &Directory,
    name: &str,
    owner: (u32, u32),
    mode: u32,
) -> Result<Directory, String> {
    let path = parent.path(name);
    match fs::DirBuilder::new().mode(mode).create(&path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => {
            return Err(format!(
                "create service runtime {}: {error}",
                path.display()
            ))
        }
    }
    let metadata = fs::symlink_metadata(&path)
        .map_err(|error| format!("inspect service runtime {}: {error}", path.display()))?;
    if !metadata.is_dir() || metadata.mode() & 0o7022 != 0 {
        return Err(format!(
            "service runtime {} must be an unredirected directory without other writers",
            path.display()
        ));
    }
    if (metadata.uid(), metadata.gid()) == (parent.uid, parent.gid) {
        // The parent is root-controlled; resume a creation interrupted before
        // chown. The service cannot replace its entry in that parent.
        fs::set_permissions(&path, Permissions::from_mode(mode))
            .map_err(|error| format!("finish service runtime mode {}: {error}", path.display()))?;
        std::os::unix::fs::lchown(&path, Some(owner.0), Some(owner.1))
            .map_err(|error| format!("assign service runtime {}: {error}", path.display()))?;
    }
    let child = Directory::open(&path, owner.0, owner.1)?;
    if child
        .file
        .metadata()
        .map_err(|error| format!("inspect service runtime {}: {error}", path.display()))?
        .mode()
        & 0o7777
        != mode
    {
        return Err(format!(
            "service runtime {} must have mode {mode:04o}",
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

    fn validate(&self, file: &File, name: &str, mode: u32, limit: u64) -> Result<(), String> {
        let metadata = file.metadata().map_err(|error| error.to_string())?;
        if !metadata.is_file()
            || metadata.uid() != self.uid
            || metadata.gid() != self.gid
            || metadata.mode() & 0o7777 != mode
            || metadata.nlink() != 1
            || metadata.len() > limit
        {
            return Err(format!(
                "{name} must be a bounded, single-link regular file of mode {mode:04o} owned by its directory's owner"
            ));
        }
        Ok(())
    }

    fn read(&self, name: &str) -> Result<Option<String>, String> {
        self.read_as(name, PRIVATE_MODE, LIMIT)
    }

    fn read_as(&self, name: &str, mode: u32, limit: u64) -> Result<Option<String>, String> {
        let file = match OpenOptions::new()
            .read(true)
            .custom_flags(O_NOFOLLOW | O_NONBLOCK)
            .open(self.path(name))
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(format!("open {name}: {error}")),
        };
        self.validate(&file, name, mode, limit)?;
        let mut text = String::new();
        file.take(limit + 1)
            .read_to_string(&mut text)
            .map_err(|error| format!("read {name}: {error}"))?;
        if text.len() as u64 > limit {
            return Err(format!("{name} grew beyond its limit"));
        }
        Ok(Some(text))
    }

    fn private_scratch(&self, name: &str, limit: u64) -> Result<Option<File>, String> {
        self.scratch(name, limit, PRIVATE_MODE)
    }

    /// Interrupted staging may lack bits of `mode`, never hold others.
    fn scratch(&self, name: &str, limit: u64, mode: u32) -> Result<Option<File>, String> {
        let file = match OpenOptions::new()
            .read(true)
            .custom_flags(O_NOFOLLOW | PATH_ONLY)
            .open(self.path(name))
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(format!("open scratch {name}: {error}")),
        };
        let metadata = file.metadata().map_err(|error| error.to_string())?;
        if !metadata.is_file()
            || metadata.uid() != self.uid
            || metadata.gid() != self.gid
            || metadata.mode() & 0o7777 & !mode != 0
            || metadata.nlink() != 1
            || metadata.len() > limit
        {
            return Err(format!(
                "scratch {name} must be one bounded, owned regular file within mode {mode:04o}"
            ));
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
        self.validate(&file, LOCK, PRIVATE_MODE, LIMIT)?;
        file.lock()
            .map_err(|error| format!("lock principal enrollment: {error}"))?;
        Ok(file)
    }

    fn retained(&self) -> Result<Option<Registry>, String> {
        let mut retained: Option<Registry> = None;
        for name in [LEDGER, NOTEBOOK_LEDGER] {
            if let Some(text) = self.read(name)? {
                let record = Registry::parse(&text)?;
                retained = Some(match retained {
                    Some(prior) => prior.enroll(&record)?,
                    None => record,
                });
            }
        }
        Ok(retained)
    }

    fn verify_enrolled(&self, desired: &Registry) -> Result<Registry, String> {
        let prior = self.retained()?.ok_or("principal ledger is absent")?;
        if prior.enroll(desired)? != prior {
            return Err("deployment identities have not been enrolled".into());
        }
        Ok(prior)
    }

    /// The reservations alone, as a boot before the account ledger made them.
    #[cfg(test)]
    fn enroll(&self, desired: &Registry) -> Result<Registry, String> {
        let _lock = self.lock()?;
        let retained = self.merged(desired)?;
        self.publish_reservations(&retained)?;
        Ok(retained)
    }

    /// Boot enrollment under one hold of the lock: the reservations and the
    /// workload account ledger seeded from what they retain. Both are
    /// computed and checked before either is published, so a refusal
    /// changes nothing.
    fn enroll_checked(
        &self,
        desired: &Registry,
        validate: impl FnOnce(&Registry) -> Result<(), String>,
    ) -> Result<Ledger, String> {
        let _lock = self.lock()?;
        let retained = self.merged(desired)?;
        validate(&retained)?;
        let ledger = self.seeded_accounts(&retained)?;
        self.publish_reservations(&retained)?;
        self.publish_as(
            workload_accounts::FILE_NAME,
            ACCOUNTS_STAGED,
            &ledger.encode(),
            ACCOUNTS_MODE,
            ACCOUNTS_LIMIT,
        )?;
        Ok(ledger)
    }

    /// The retained reservations joined with the deployment's. The caller
    /// holds the lock.
    fn merged(&self, desired: &Registry) -> Result<Registry, String> {
        match self.retained()? {
            Some(prior) => prior.enroll(desired),
            None => Ok(desired.clone()),
        }
    }

    fn publish_reservations(&self, retained: &Registry) -> Result<(), String> {
        // Publish the full reservation first. A crash cannot release it;
        // retry merges both records. Older boots read only the legacy file.
        if retained.notebooks().next().is_some() {
            self.publish(NOTEBOOK_LEDGER, NOTEBOOK_STAGED, &retained.encode())?;
        }
        self.publish(LEDGER, STAGED, &retained.legacy().encode())
    }

    /// The published account ledger with every application row the
    /// reservations retain, a removed one's included, and the carried rows.
    /// A malformed ledger or a reassignment refuses.
    fn seeded_accounts(&self, retained: &Registry) -> Result<Ledger, String> {
        let prior =
            match self.read_as(workload_accounts::FILE_NAME, ACCOUNTS_MODE, ACCOUNTS_LIMIT)? {
                Some(text) => Ledger::parse(&text)?,
                None => Ledger::default(),
            };
        seed(&prior, retained)
    }

    fn publish(&self, name: &str, staged: &str, text: &str) -> Result<(), String> {
        self.publish_as(name, staged, text, PRIVATE_MODE, LIMIT)
    }

    fn publish_as(
        &self,
        name: &str,
        staged: &str,
        text: &str,
        mode: u32,
        limit: u64,
    ) -> Result<(), String> {
        // Refuse substituted scratch before removing an interrupted write.
        if self.scratch(staged, limit, mode)?.is_some() {
            fs::remove_file(self.path(staged)).map_err(|error| error.to_string())?;
            self.sync()?;
        }
        if self.read_as(name, mode, limit)?.as_deref() == Some(text) {
            return self.sync();
        }
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .custom_flags(O_NOFOLLOW | O_NONBLOCK)
            .open(self.path(staged))
            .map_err(|error| format!("create {staged}: {error}"))?;
        // Exact mode even when an inherited umask removes owner bits.
        file.set_permissions(Permissions::from_mode(mode))
            .map_err(|error| error.to_string())?;
        self.validate(&file, staged, mode, limit)?;
        let result = (|| {
            file.write_all(text.as_bytes())
                .map_err(|error| format!("write {staged}: {error}"))?;
            file.sync_all()
                .map_err(|error| format!("sync {staged}: {error}"))?;
            fs::rename(self.path(staged), self.path(name))
                .map_err(|error| format!("publish {name}: {error}"))?;
            self.sync()
        })();
        if result.is_err() {
            // The lock excludes protocol writers and the protected directory
            // excludes unprivileged replacement. Before rename this is our
            // staged inode; after rename the name is absent.
            let _ = fs::remove_file(self.path(staged));
        }
        result
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

/// `prior` with the carried rows and every application row `registry`
/// holds. The ledger keys rows by name alone and admits a narrower uid
/// range than the principal table, so either difference refuses.
fn seed(prior: &Ledger, registry: &Registry) -> Result<Ledger, String> {
    prior.seed(
        registry
            .applications()
            .map(|application| (application.name.as_str(), application.uid)),
    )
}

/// The image build's check: the deployment's application rows fit an
/// account ledger beside the carried rows, so its first boot can seed one.
pub(crate) fn check_accounts(registry: &Registry) -> Result<(), String> {
    seed(&Ledger::default(), registry).map(|_| ())
}

/// Reserve the deployment's identities and seed the workload account
/// ledger from them. No consumer reads that ledger yet.
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

/// Read-only selection for the root application launcher. The caller cannot
/// choose an account or reuse a retired application's reserved identity.
pub(crate) fn check_launch_application(owner: u32, name: &str) -> Result<u32, String> {
    let desired = Registry::load()?;
    let prior = state_directory()?.verify_enrolled(&desired)?;
    prior.verify_installed_accounts(&desired)?;
    let application = desired
        .active_applications()?
        .into_iter()
        .find(|application| application.owner == owner && application.name == name)
        .ok_or("application has no active account in the enrolled deployment")?;
    let policy = crate::app_policy::load()?;
    let rule = policy
        .for_name(name)
        .ok_or("application has no bus assignment")?;
    if policy.owner() != owner || (rule.uid != owner && rule.uid != application.uid) {
        return Err("application launch and retained assignments disagree".into());
    }
    Ok(rule.uid)
}

#[cfg(test)]
#[path = "principal_store_tests.rs"]
mod tests;

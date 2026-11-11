//! Durable UID reservations. Enrollment never releases an old assignment.

use crate::app_policy::Declarations;
use crate::principals::{Registry, MAX_BYTES, O_DIRECTORY, O_NOFOLLOW, O_NONBLOCK};
use crate::workload_accounts::{self, Kind, Ledger};
use crate::workload_grants::{self, Table};
use std::collections::BTreeSet;
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
const NOTEBOOK_LEDGER: &str = "principals-notebooks.tsv";
const NOTEBOOK_STAGED: &str = ".principals-notebooks.next";
const LOCK: &str = ".principals.lock";
const PRIVATE_MODE: u32 = 0o600;
/// The workload account ledger (td-authd/DESIGN.md, "Workload account
/// ledger"). It holds only kinds, names and uids, so every account reads it.
const ACCOUNTS_STAGED: &str = ".accounts.next";
const ACCOUNTS_MODE: u32 = workload_accounts::MODE;
const ACCOUNTS_LIMIT: u64 = workload_accounts::MAX_BYTES as u64;
/// The workload grant table (td-authd/DESIGN.md, "Workload grant table"):
/// what each account may reach of the human's, root's alone to read.
const GRANTS_STAGED: &str = ".grants.next";
const GRANTS_MODE: u32 = workload_grants::MODE;
const GRANTS_LIMIT: u64 = workload_grants::MAX_BYTES as u64;
/// The application homes, `/var/lib/td/applications/UID`, and how many
/// entries an allocation reads in any directory it lists: past every
/// application uid.
const HOMES: &str = "applications";
const MAX_HOMES: usize = 1 << 16;
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

/// The uid every entry of the directory `path` names after `prefix`, in
/// canonical decimal; entries are listed, never followed. More than
/// `MAX_HOMES` entries refuses.
pub(crate) fn uid_entries(path: &Path, prefix: &str) -> Result<BTreeSet<u32>, String> {
    let mut uids = BTreeSet::new();
    let entries =
        fs::read_dir(path).map_err(|error| format!("list {}: {error}", path.display()))?;
    for (index, entry) in entries.enumerate() {
        if index >= MAX_HOMES {
            return Err(format!(
                "{} holds more than {MAX_HOMES} entries",
                path.display()
            ));
        }
        let entry = entry.map_err(|error| format!("list {}: {error}", path.display()))?;
        if let Some(uid) = entry
            .file_name()
            .to_str()
            .and_then(|text| text.strip_prefix(prefix))
            .and_then(|digits| {
                digits
                    .parse::<u32>()
                    .ok()
                    .filter(|uid| uid.to_string() == digits)
            })
        {
            uids.insert(uid);
        }
    }
    Ok(uids)
}

/// The uids of every `/run/user/UID`, whoever owns it.
fn runtime_uids() -> Result<BTreeSet<u32>, String> {
    Directory::open(Path::new("/run"), 0, 0)?.uid_entries("user", "")
}

/// The owner of `/run/user/UID`, if it exists, read without following
/// links: an install's new account refuses one the account already owns.
pub(crate) fn application_runtime_owner(uid: u32) -> Result<Option<(u32, u32)>, String> {
    let run = Directory::open(Path::new("/run"), 0, 0)?;
    match fs::symlink_metadata(run.path("user")) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("inspect /run/user: {error}")),
    }
    let users = run.child("user")?;
    match fs::symlink_metadata(users.path(&uid.to_string())) {
        Ok(metadata) if metadata.is_dir() => Ok(Some((metadata.uid(), metadata.gid()))),
        Ok(_) => Err(format!("/run/user/{uid} is not a directory")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("inspect /run/user/{uid}: {error}")),
    }
}

/// Private client endpoints exist only for an installed application account.
pub(crate) fn prepare_application_runtime(uid: u32) -> Result<(), String> {
    if !crate::workload_accounts::APPLICATION_UIDS.contains(&uid) {
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

    /// Boot enrollment under one hold of the lock: the reservations, the
    /// workload account ledger seeded from what they retain, and the
    /// workload grant table, seeded only when absent. All are computed and
    /// checked before any is published, so a refusal changes nothing.
    fn enroll_checked(
        &self,
        desired: &Registry,
        validate: impl FnOnce(&Registry) -> Result<(), String>,
    ) -> Result<Ledger, String> {
        let _lock = self.lock()?;
        let retained = self.merged(desired)?;
        validate(&retained)?;
        let ledger = self.seeded_accounts(&retained)?;
        let grants = self.seeded_grants()?;
        self.publish_reservations(&retained)?;
        self.publish_as(
            workload_accounts::FILE_NAME,
            ACCOUNTS_STAGED,
            &ledger.encode(),
            ACCOUNTS_MODE,
            ACCOUNTS_LIMIT,
        )?;
        if let Some(grants) = grants {
            self.publish_as(
                workload_grants::FILE_NAME,
                GRANTS_STAGED,
                &grants.encode(),
                GRANTS_MODE,
                GRANTS_LIMIT,
            )?;
        }
        Ok(ledger)
    }

    /// The grant table to publish: the seed (Firefox's `Downloads`) when
    /// none exists, else none, once the present one has been read at its
    /// exact shape and parsed. The seed is written once, so a row the human
    /// revoked stays revoked; a malformed table refuses and is never reset.
    fn seeded_grants(&self) -> Result<Option<Table>, String> {
        match self.read_as(workload_grants::FILE_NAME, GRANTS_MODE, GRANTS_LIMIT)? {
            Some(text) => Table::parse(&text).map(|_| None),
            None => Ok(Some(Table::seeded())),
        }
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

    /// The application account `name` holds in the published ledger, which
    /// this allocates when it holds none; then `prepare` runs with the uid
    /// and whether the row is new, still under the lock, so allocations and
    /// their preparation never overlap. A new row avoids every uid a
    /// retained reservation of any owner names, every id `outside` reads
    /// (passwd and group, and every account cgroup and runtime directory
    /// that exists, all read here under the lock) and every home left below
    /// `applications`: a lost ledger forgets its allocations but never
    /// re-issues a uid whose home, cgroup or runtime remains, so a new row's
    /// preparation finds none of them. Before anything is published the
    /// row must pass the admission a launch applies (`launch_uid`), so an
    /// install refuses exactly what a launch would. An absent ledger
    /// refuses: boot seeds it before anything can install.
    fn allocate(
        &self,
        owner: u32,
        name: &str,
        declarations: &Declarations,
        outside: impl FnOnce() -> Result<BTreeSet<u32>, String>,
        prepare: impl FnOnce(u32, bool) -> Result<(), String>,
    ) -> Result<u32, String> {
        let _lock = self.lock()?;
        let text = self
            .read_as(workload_accounts::FILE_NAME, ACCOUNTS_MODE, ACCOUNTS_LIMIT)?
            .ok_or("the workload account ledger is absent")?;
        let retained = self.retained()?.ok_or("principal ledger is absent")?;
        let mut taken = outside()?;
        taken.extend(retained.applications().map(|application| application.uid));
        taken.extend(self.homes()?);
        let (ledger, uid, new) = Ledger::parse(&text)?.allocate(Kind::Application, name, &taken)?;
        let policy = crate::app_policy::Policy::join(declarations.clone(), ledger.clone());
        if launch_uid(&retained, &policy, owner, name)? != uid {
            return Err("application launch and allocation disagree".into());
        }
        if new {
            self.publish_as(
                workload_accounts::FILE_NAME,
                ACCOUNTS_STAGED,
                &ledger.encode(),
                ACCOUNTS_MODE,
                ACCOUNTS_LIMIT,
            )?;
        }
        prepare(uid, new)?;
        Ok(uid)
    }

    /// The uids of the homes below the child `applications`; none when it
    /// is absent.
    fn homes(&self) -> Result<BTreeSet<u32>, String> {
        self.uid_entries(HOMES, "")
    }

    /// The uid every entry of the child directory `name` names after
    /// `prefix`, in canonical decimal, read without following links; none
    /// when the child is absent.
    fn uid_entries(&self, name: &str, prefix: &str) -> Result<BTreeSet<u32>, String> {
        match fs::symlink_metadata(self.path(name)) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(BTreeSet::new())
            }
            Err(error) => return Err(format!("inspect {name}: {error}")),
        }
        uid_entries(&self.child(name)?.path("."), prefix)
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

/// The image's own principal table reserves no application: an application
/// account is a ledger row. Retained reservations keep theirs.
fn require_no_application_rows(desired: &Registry) -> Result<(), String> {
    match desired.applications().next() {
        Some(application) => Err(format!(
            "the deployment's principal table reserves application {:?}; application \
             accounts come from the workload account ledger",
            application.name
        )),
        None => Ok(()),
    }
}

/// Whether `application` is one of the image's Flathub declarations, the
/// applications td-software installs and td-authd allocates for.
fn flathub(application: &str) -> bool {
    crate::flathub_app::flathub_source(application).is_some()
}

/// The image build's check: the deployment's principal table reserves no
/// application, and every application it ships in the image among those it
/// declares (`declarations`, which a server lacks) has a carried ledger
/// row, since nothing allocates one at boot. A `flathub` declaration may
/// lack one: td-authd allocates it when td-software installs it.
pub(crate) fn check_accounts(
    registry: &Registry,
    declarations: Option<&Declarations>,
) -> Result<(), String> {
    check_accounts_with(registry, declarations, flathub)
}

/// `check_accounts` over the given Flathub declarations.
fn check_accounts_with(
    registry: &Registry,
    declarations: Option<&Declarations>,
    flathub: fn(&str) -> bool,
) -> Result<(), String> {
    require_no_application_rows(registry)?;
    let carried = seed(&Ledger::default(), registry)?;
    for row in declarations.map_or(&[][..], Declarations::rows) {
        if carried
            .uid_of(Kind::Application, &row.application)
            .is_none()
            && !flathub(&row.application)
        {
            return Err(format!(
                "application {:?} ships in the image without a carried workload account",
                row.application
            ));
        }
    }
    Ok(())
}

/// What an install may allocate: an account for `name`, which the session
/// `owner`'s deployment declares as a service application among its
/// `flathub` declarations. A personal application runs as its owner, and an
/// application the image ships has its carried row or none.
fn allocatable(
    declarations: &Declarations,
    owner: u32,
    name: &str,
    flathub: fn(&str) -> bool,
) -> Result<(), String> {
    if declarations.owner() != owner {
        return Err("application allocation names another session".into());
    }
    let row = declarations
        .rows()
        .iter()
        .find(|row| row.application == name)
        .ok_or_else(|| format!("the deployment does not declare application {name:?}"))?;
    if row.class != crate::app_policy::Class::Service {
        return Err(format!(
            "application {name:?} runs as its owner and has no account of its own"
        ));
    }
    if !flathub(name) {
        return Err(format!(
            "application {name:?} ships in the image; only a Flathub application is allocated at install"
        ));
    }
    Ok(())
}

/// td-authd's allocation when td-software installs `name` (td-authd/DESIGN.md,
/// "Workload account ledger"): with the deployment enrolled and its
/// accounts verified, the ledger row of a declared Flathub service
/// application, added when absent and admitted as a launch would be, and
/// `prepare` run with its uid and whether the row is new, under the
/// principal lock. `delegated` reads the uids of the account cgroups that
/// exist, which a new row avoids. No caller supplies a uid.
pub(crate) fn allocate_application(
    owner: u32,
    name: &str,
    delegated: impl FnOnce() -> Result<BTreeSet<u32>, String>,
    prepare: impl FnOnce(u32, bool) -> Result<(), String>,
) -> Result<u32, String> {
    let desired = Registry::load()?;
    let directory = state_directory()?;
    let prior = directory.verify_enrolled(&desired)?;
    prior.verify_installed_accounts(&desired)?;
    let policy = crate::app_policy::load()?;
    allocatable(policy.declarations(), owner, name, flathub)?;
    let outside = || {
        let mut taken = crate::principals::installed_ids()?;
        taken.extend(runtime_uids()?);
        taken.extend(delegated()?);
        Ok(taken)
    };
    directory.allocate(owner, name, policy.declarations(), outside, prepare)
}

/// Reserve the deployment's identities and seed the workload account
/// ledger, the one source of application uids, from them.
pub(crate) fn provision(desired: &Registry) -> Result<(), String> {
    require_no_application_rows(desired)?;
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
    launch_uid(&prior, &crate::app_policy::load()?, owner, name)
}

/// The uid `name` launches as: the session owner for a personal
/// application, else its ledger account. Either way the image declares it
/// and the ledger holds its account, at the uid any retained reservation of
/// it names.
fn launch_uid(
    retained: &Registry,
    policy: &crate::app_policy::Policy,
    owner: u32,
    name: &str,
) -> Result<u32, String> {
    if policy.owner() != owner {
        return Err("application launch names another session".into());
    }
    let account = policy
        .account(name)
        .ok_or("application has no workload account in the enrolled deployment")?;
    let rule = policy
        .for_name(name)
        .ok_or("application has no bus assignment")?;
    // `for_name` gives the owner or this account; only a retained
    // reservation can disagree.
    if retained
        .applications()
        .any(|application| application.name == name && application.uid != account)
    {
        return Err("application launch and retained assignments disagree".into());
    }
    Ok(rule.uid)
}

#[cfg(test)]
#[path = "principal_store_tests.rs"]
mod tests;

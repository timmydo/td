//! Root publication of a launching account's grant views
//! (td-authd/DESIGN.md, "Application filesystem grants"): each directory
//! row the workload grant table holds for the account becomes a detached,
//! nonrecursive idmapped clone of the human's directory, mapping filesystem
//! uid/gid 1000 to the account, nosuid, nodev, noexec and read-only unless
//! the row is `rw`, published at `/var/lib/td/grant-views/UID/N` and listed
//! in the account's manifest for td-jail.

use crate::mount_identity::{self, MountIdentity, MountRow};
use crate::mount_sys::{self, ViewAccess};
use crate::portal_files::{self, pinned};
use crate::workload_accounts::{Kind, APPLICATION_UIDS, O_DIRECTORY, O_NOFOLLOW, O_NONBLOCK};
use crate::workload_grants::{self as grants, Access, Manifest, Table, View};
use crate::{launch, primary_account};
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, AsRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const HUMAN: u32 = primary_account::UID;
const MAX_MOUNTINFO: u64 = 1024 * 1024;
const MAX_FDINFO: u64 = 4096;
const STAGED_MANIFEST: &str = ".manifest.next";
/// Entries an account's view directory may hold: its views, its manifest
/// and an interrupted manifest's staging.
const MAX_ENTRIES: usize = grants::MAX_VIEWS + 2;
/// Root's own directory of launch locks below the view tree, mode 0700, so
/// no account can open, and so hold, the lock its launch takes.
pub(crate) const LOCKS: &str = ".locks";
const LOCK_DEADLINE: Duration = Duration::from_secs(10);
const LOCK_POLL: Duration = Duration::from_millis(50);

/// The grant table, read as root reads it: `/var/lib/td` and its ancestors
/// root-owned without other writers, the table opened through them without
/// following links, a single-link regular file of exact mode 0600 that root
/// owns, read to its bound. An absent table refuses: boot seeds it first.
pub(crate) fn read_table() -> Result<Table, String> {
    let state = state_directory()?;
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(O_NOFOLLOW | O_NONBLOCK)
        .open(pinned(&state, grants::FILE_NAME))
        .map_err(|e| format!("open the workload grant table: {e}"))?;
    let metadata = file.metadata().map_err(|e| e.to_string())?;
    if !published_table(&metadata, (0, 0)) {
        return Err(format!(
            "{} must be one bounded, single-link, root-owned regular file of mode 0600",
            grants::FILE_NAME
        ));
    }
    let mut text = String::new();
    file.take(grants::MAX_BYTES as u64 + 1)
        .read_to_string(&mut text)
        .map_err(|e| format!("read the workload grant table: {e}"))?;
    Table::parse(&text)
}

/// The grant table's file as root admits it: a root-owned regular file of
/// exact mode 0600 within the bound, of one link or, opened just before
/// td-firstboot renamed its successor over it, none: an unlinked inode of
/// that shape was the whole published table, read without the writer's
/// lock as every launch reads it.
fn published_table(metadata: &fs::Metadata, owner: (u32, u32)) -> bool {
    metadata.is_file()
        && (metadata.uid(), metadata.gid()) == owner
        && metadata.mode() & 0o7777 == grants::MODE
        && metadata.nlink() <= 1
        && metadata.len() <= grants::MAX_BYTES as u64
}

fn state_directory() -> Result<File, String> {
    let root = portal_files::directory(Path::new("/"), 0, true)?;
    let var = portal_files::child(&root, "var", 0, true)?;
    let lib = portal_files::child(&var, "lib", 0, true)?;
    portal_files::child(&lib, "td", 0, true)
}

/// The mount a descriptor's file lies on, as `/proc/self/fdinfo` names it:
/// the id of its mountinfo row.
pub(crate) fn mount_id(file: &File) -> Result<u64, String> {
    let mut text = String::new();
    File::open(format!("/proc/self/fdinfo/{}", file.as_raw_fd()))
        .map_err(|e| e.to_string())?
        .take(MAX_FDINFO)
        .read_to_string(&mut text)
        .map_err(|e| e.to_string())?;
    text.lines()
        .find_map(|line| line.strip_prefix("mnt_id:"))
        .and_then(|value| value.trim().parse::<u64>().ok())
        .ok_or_else(|| "descriptor information names no mount".into())
}

/// One opened human directory a grant names.
pub(crate) struct Source {
    pub(crate) file: File,
    device: u64,
    inode: u64,
}

/// The human's directory `path` below `home`: each name opened through its
/// parent without following links, on `home`'s own mount (a directory with
/// a mount stacked on it or on a directory between it and the home is
/// refused), a directory `owner` owns that no one else may write. None when
/// a name is absent; any other refusal is an error.
pub(crate) fn human_directory(
    home: &File,
    path: &str,
    owner: (u32, u32),
) -> Result<Option<Source>, String> {
    grants::validate_path(path.as_bytes())?;
    let mount = mount_id(home)?;
    let mut current = home.try_clone().map_err(|e| e.to_string())?;
    for name in path.split('/') {
        current = match OpenOptions::new()
            .read(true)
            .custom_flags(O_DIRECTORY | O_NOFOLLOW)
            .open(pinned(&current, name))
        {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(format!("open granted directory {path:?}: {error}")),
        };
        if mount_id(&current)? != mount {
            return Err(format!(
                "granted directory {path:?} lies on a mount stacked over the human's home"
            ));
        }
        let metadata = current.metadata().map_err(|e| e.to_string())?;
        if !metadata.is_dir()
            || (metadata.uid(), metadata.gid()) != owner
            || metadata.mode() & 0o022 != 0
        {
            return Err(format!(
                "granted directory {path:?} must be the human's own and writable by no one else"
            ));
        }
    }
    let metadata = current.metadata().map_err(|e| e.to_string())?;
    Ok(Some(Source {
        file: current,
        device: metadata.dev(),
        inode: metadata.ino(),
    }))
}

/// The whole mount table as text. It is read as bytes: a mountpoint that is
/// not UTF-8 elsewhere must not refuse every launch, and none this module
/// compares is, since every path it derives is.
fn mount_table() -> Result<String, String> {
    let mut bytes = Vec::new();
    File::open("/proc/self/mountinfo")
        .map_err(|e| e.to_string())?
        .take(MAX_MOUNTINFO + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() as u64 > MAX_MOUNTINFO {
        return Err("mount table exceeds the grant view bound".into());
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn mount_rows(text: &str) -> Result<Vec<MountRow>, String> {
    mount_identity::mount_rows(text).map_err(|e| e.to_string())
}

/// The access a view of a grant takes.
fn view_access(access: Access) -> ViewAccess {
    match access {
        Access::ReadOnly => ViewAccess::ReadOnly,
        Access::ReadWrite => ViewAccess::Writable,
    }
}

/// Whether mount options hold a view's: its access, nosuid, nodev, noexec
/// and the idmap.
fn view_options(options: &str, access: Access) -> bool {
    [access.as_str(), "nosuid", "nodev", "noexec", "idmapped"]
        .iter()
        .all(|required| options.split(',').any(|option| option == *required))
}

/// A directory row of the account's: its access and path, and what a view
/// of it must show, by the derivation td-jail admits it by.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Grant {
    access: Access,
    path: String,
    identity: MountIdentity,
}

/// A view directory found below the account's: its index, the mounts at
/// it, and for one mount what a stat through it shows (device, inode, uid,
/// gid).
#[derive(Clone, Debug, PartialEq, Eq)]
struct Present {
    index: usize,
    mounts: Vec<MountRow>,
    shown: Option<(u64, u64, u32, u32)>,
}

/// Whether `view` is one mount showing `grant`'s identity, access,
/// restrictions and idmap. What it shows through the stat is the second
/// phase's to compare, once the source is open.
fn shows(view: &Present, grant: &Grant) -> bool {
    let [mount] = view.mounts.as_slice() else {
        return false;
    };
    mount.device == grant.identity.device
        && mount.root.as_os_str().as_bytes() == grant.identity.root.as_os_str().as_bytes()
        && view_options(&mount.options, grant.access)
}

/// The application `name`'s directory rows, each with what a view of it
/// must show below `home` by the derivation td-jail admits it by: the root
/// of the mount covering the home, the home's path below that mount's
/// mountpoint, and the row's path.
fn grants_for(
    table: &Table,
    name: &str,
    mountinfo: &str,
    home: &Path,
) -> Result<Vec<Grant>, String> {
    let mut grants = Vec::new();
    for directory in table.directories(Kind::Application, name) {
        let identity =
            mount_identity::human_source_identity(mountinfo, home, Path::new(&directory.path))
                .map_err(|e| e.to_string())?;
        grants.push(Grant {
            access: directory.access,
            path: directory.path,
            identity,
        });
    }
    Ok(grants)
}

/// A row's source, opened and checked, with its device and inode; None
/// when it is absent.
type Found<S> = Option<(S, (u64, u64))>;

/// What publication does to the system, behind which `reconcile` decides
/// what to do and in which order.
trait Host {
    type Source;
    /// The views below the account's directory now, each with its mounts.
    fn present(&mut self) -> Result<Vec<Present>, String>;
    /// The human's directory a row names, opened and checked, with its
    /// device and inode; None when it is absent.
    fn source(&mut self, path: &str) -> Result<Found<Self::Source>, String>;
    fn publish(
        &mut self,
        index: usize,
        source: &Self::Source,
        access: Access,
    ) -> Result<(), String>;
    /// Unmounts every mount at the view and removes its mountpoint.
    fn release(&mut self, index: usize) -> Result<(), String>;
    /// Replaces the account's manifest whole, by a staged rename.
    fn write_manifest(&mut self, manifest: &Manifest) -> Result<(), String>;
    fn warn(&mut self, message: &str);
}

fn manifest_of(listed: &[(usize, usize)], grants: &[Grant]) -> Result<Manifest, String> {
    let mut views = Vec::new();
    for (index, at) in listed {
        let grant = grants.get(*at).ok_or("grant view plan names no grant")?;
        views.push(View {
            index: *index,
            access: grant.access,
            path: grant.path.clone(),
        });
    }
    Manifest::new(views)
}

/// One launch's publication of `uid`'s views of `grants`, in an order that
/// keeps the manifest true of what is mounted throughout:
///
/// 1. Views no row accounts for (a revoked row's, a renamed or removed
///    source's, one whose access changed, one with a mount stacked on it,
///    an empty mountpoint) are dropped from the manifest and released,
///    before any source is opened, so no later refusal pins them.
/// 2. Each row's source is opened and checked. A row whose source is absent
///    gets no view; one whose source is refused gets none and a warning,
///    and the other rows go on. A present view that still shows the source
///    is kept; any other row gets a new view at a free index.
/// 3. The new views are checked as mounted, the manifest replaced, and the
///    views it no longer lists released.
fn reconcile<H: Host>(host: &mut H, uid: u32, grants: &[Grant]) -> Result<(), String> {
    let present = host.present()?;
    let mut candidates: Vec<(usize, usize)> = Vec::new();
    for view in &present {
        let matching = grants.iter().enumerate().position(|(at, grant)| {
            shows(view, grant) && !candidates.iter().any(|(_, taken)| *taken == at)
        });
        if let Some(at) = matching {
            candidates.push((view.index, at));
        }
    }
    host.write_manifest(&manifest_of(&candidates, grants)?)?;
    for view in &present {
        if !candidates.iter().any(|(index, _)| *index == view.index) {
            host.release(view.index)?;
        }
    }
    let mut occupied: Vec<usize> = candidates.iter().map(|(index, _)| *index).collect();
    let mut listed: Vec<(usize, usize)> = Vec::new();
    let mut expected: Vec<(usize, (u64, u64, u32, u32))> = Vec::new();
    for (at, grant) in grants.iter().enumerate() {
        let (source, (device, inode)) = match host.source(&grant.path) {
            Ok(Some(found)) => found,
            Ok(None) => continue,
            Err(error) => {
                host.warn(&format!("grant of {:?} gets no view: {error}", grant.path));
                continue;
            }
        };
        let shown = (device, inode, uid, uid);
        let kept = candidates.iter().find(|(index, taken)| {
            *taken == at
                && present
                    .iter()
                    .any(|view| view.index == *index && view.shown == Some(shown))
        });
        let index = match kept {
            Some((index, _)) => *index,
            None => {
                let index = (0..grants::MAX_VIEWS)
                    .find(|index| !occupied.contains(index))
                    .ok_or("no free grant view index")?;
                host.publish(index, &source, grant.access)?;
                occupied.push(index);
                index
            }
        };
        listed.push((index, at));
        expected.push((index, shown));
    }
    if listed.iter().any(|listing| !candidates.contains(listing)) {
        let now = host.present()?;
        for ((index, at), (_, shown)) in listed.iter().zip(&expected) {
            let grant = grants.get(*at).ok_or("grant view plan names no grant")?;
            if !now.iter().any(|view| {
                view.index == *index && shows(view, grant) && view.shown == Some(*shown)
            }) {
                return Err("a published grant view does not show its human directory".into());
            }
        }
    }
    host.write_manifest(&manifest_of(&listed, grants)?)?;
    for (index, _) in &candidates {
        if !listed.iter().any(|(listing, _)| listing == index) {
            host.release(*index)?;
        }
    }
    Ok(())
}

/// The system `reconcile` runs against for one account.
struct Live<'a> {
    uid: u32,
    views: &'a File,
    home: &'a File,
    namespace: Option<File>,
}

impl Host for Live<'_> {
    type Source = File;

    fn present(&mut self) -> Result<Vec<Present>, String> {
        present_views(self.views, self.uid, &mount_rows(&mount_table()?)?)
    }

    fn source(&mut self, path: &str) -> Result<Found<File>, String> {
        Ok(human_directory(self.home, path, (HUMAN, HUMAN))?
            .map(|source| (source.file, (source.device, source.inode))))
    }

    fn publish(&mut self, index: usize, source: &File, access: Access) -> Result<(), String> {
        let target = portal_files::ensure_root_child(self.views, &index.to_string())?;
        if fs::read_dir(pinned(&target, ""))
            .map_err(|e| e.to_string())?
            .next()
            .is_some()
        {
            return Err("an unpublished grant view's mountpoint is not empty".into());
        }
        if self.namespace.is_none() {
            self.namespace = Some(portal_files::namespace_for(self.uid)?);
        }
        let namespace = self
            .namespace
            .as_ref()
            .ok_or("grant view mapping is missing")?;
        let mount = mount_sys::clone_directory(source.as_fd()).map_err(|e| e.to_string())?;
        mount_sys::view_attributes(mount.as_fd(), namespace.as_fd(), view_access(access))
            .map_err(|e| e.to_string())?;
        mount_sys::publish(mount.as_fd(), target.as_fd()).map_err(|e| e.to_string())
    }

    fn release(&mut self, index: usize) -> Result<(), String> {
        unmount_all(&format!("{}/{}/{index}", grants::VIEW_ROOT, self.uid))?;
        fs::remove_dir(pinned(self.views, &index.to_string()))
            .map_err(|e| format!("remove grant view mountpoint {index}: {e}"))
    }

    fn write_manifest(&mut self, manifest: &Manifest) -> Result<(), String> {
        write_manifest(self.views, (0, self.uid), manifest)
    }

    fn warn(&mut self, message: &str) {
        let _ = writeln!(io::stderr().lock(), "td-authd: {message}");
    }
}

/// Publishes the grant views of the application account `name` runs as,
/// `uid`, for its launch, and writes its manifest: root's per-launch step
/// after admission and before the account's first process. td-svc runs one
/// unit per application, which serialises its launches; a launch that finds
/// another holding the account's lock past the deadline fails closed.
/// FileChooser's application also brings the portal's Downloads view in
/// line with its grant, which never fails the launch.
pub(crate) fn publish(name: &str, uid: u32) -> Result<(), String> {
    if !APPLICATION_UIDS.contains(&uid) {
        return Err("grant views belong to an application account".into());
    }
    let table = read_table()?;
    let account = primary_account::load().map_err(|error| error.to_string())?;
    let root = portal_files::directory(Path::new("/"), 0, true)?;
    let var = portal_files::child(&root, "var", 0, true)?;
    let homes = portal_files::child(&var, "home", 0, true)?;
    let home = portal_files::child(&homes, account.name(), HUMAN, true)?;
    let mountinfo = mount_table()?;
    let grants = grants_for(&table, name, &mountinfo, &account.persistent_home())?;
    let refused = table.refused(Kind::Application, name);
    let lib = portal_files::child(&var, "lib", 0, true)?;
    let td = portal_files::child(&lib, "td", 0, true)?;
    let tree = portal_files::ensure_root_child(&td, grants::VIEW_ROOT_NAME)?;
    let locks = private_directory(&tree, LOCKS, (0, 0))?;
    let _lock = lock(&locks, &uid.to_string(), (0, 0), LOCK_DEADLINE)?;
    let views = account_directory(&tree, uid)?;
    let mut live = Live {
        uid,
        views: &views,
        home: &home,
        namespace: None,
    };
    // A row this deployment's validator refuses is held, never published.
    for (directory, why) in &refused {
        live.warn(&format!("row {:?} is not published: {why}", directory.path));
    }
    reconcile(&mut live, uid, &grants)?;
    remove_retired_mountpoint(&td, uid, &mount_rows(&mountinfo)?, &mut live);
    if name == portal_files::FILE_CHOOSER_APPLICATION {
        if let Err(error) = portal_files::sync_downloads(&table) {
            live.warn(&format!("the portal's Downloads view: {error}"));
        }
    }
    Ok(())
}

/// The empty root-owned `Downloads` mountpoint the retired boot unit made
/// in an application's private home, removed once: only an empty
/// directory root owns at mode 0755 that no mount covers. Anything else is
/// the application's and is left alone.
fn remove_retired_mountpoint<H: Host>(td: &File, uid: u32, mounts: &[MountRow], host: &mut H) {
    let path = format!("/var/lib/td/applications/{uid}/Downloads");
    if mounts.iter().any(|row| row.mountpoint == Path::new(&path)) {
        return;
    }
    let Ok(applications) = portal_files::child(td, "applications", 0, true) else {
        return;
    };
    let Ok(home) = OpenOptions::new()
        .read(true)
        .custom_flags(O_DIRECTORY | O_NOFOLLOW)
        .open(pinned(&applications, &uid.to_string()))
    else {
        return;
    };
    let Ok(metadata) = fs::symlink_metadata(pinned(&home, "Downloads")) else {
        return;
    };
    if metadata.is_dir() && metadata.uid() == 0 && metadata.mode() & 0o7777 == 0o755 {
        if let Err(error) = fs::remove_dir(pinned(&home, "Downloads")) {
            host.warn(&format!("retired Downloads mountpoint of {uid}: {error}"));
        }
    }
}

/// A directory below `parent` only `owner` may enter: exactly mode 0700.
pub(crate) fn private_directory(
    parent: &File,
    name: &str,
    owner: (u32, u32),
) -> Result<File, String> {
    match fs::DirBuilder::new()
        .mode(0o700)
        .create(pinned(parent, name))
    {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(format!("create {name}: {error}")),
    }
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(O_DIRECTORY | O_NOFOLLOW)
        .open(pinned(parent, name))
        .map_err(|e| format!("open {name}: {e}"))?;
    let metadata = directory.metadata().map_err(|e| e.to_string())?;
    if !metadata.is_dir() || (metadata.uid(), metadata.gid()) != owner {
        return Err(format!("{name} is not root's"));
    }
    if metadata.mode() & 0o7777 != 0o700 {
        directory
            .set_permissions(fs::Permissions::from_mode(0o700))
            .map_err(|e| e.to_string())?;
    }
    Ok(directory)
}

/// An exclusive lock on `name` in `directory`, a single-link regular file
/// of `owner` at mode 0600, waited for no longer than `deadline`. The lock
/// lives where no account can open it, so none can hold a launch up.
pub(crate) fn lock(
    directory: &File,
    name: &str,
    owner: (u32, u32),
    deadline: Duration,
) -> Result<File, String> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(O_NOFOLLOW | O_NONBLOCK)
        .open(pinned(directory, name))
        .map_err(|e| format!("open lock {name}: {e}"))?;
    let metadata = file.metadata().map_err(|e| e.to_string())?;
    if !metadata.is_file()
        || (metadata.uid(), metadata.gid()) != owner
        || metadata.mode() & 0o7777 != 0o600
        || metadata.nlink() != 1
    {
        return Err(format!("lock {name} is not one private file of root's"));
    }
    let until = Instant::now() + deadline;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(TryLockError::WouldBlock) if Instant::now() < until => {
                std::thread::sleep(LOCK_POLL);
            }
            Err(TryLockError::WouldBlock) => {
                return Err(format!("lock {name} is held past its deadline"));
            }
            Err(TryLockError::Error(error)) => return Err(format!("lock {name}: {error}")),
        }
    }
}

/// The account's view directory below the view tree: root-owned, of the
/// account's group, exactly mode 0710, so the account reaches the names
/// td-jail opens without listing them; created or completed here.
fn account_directory(tree: &File, uid: u32) -> Result<File, String> {
    let name = uid.to_string();
    match fs::DirBuilder::new()
        .mode(0o700)
        .create(pinned(tree, &name))
    {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(format!("create grant view directory: {error}")),
    }
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(O_DIRECTORY | O_NOFOLLOW)
        .open(pinned(tree, &name))
        .map_err(|e| format!("open grant view directory: {e}"))?;
    let metadata = directory.metadata().map_err(|e| e.to_string())?;
    if !metadata.is_dir() || metadata.uid() != 0 {
        return Err("a grant view directory is not root's".into());
    }
    if metadata.gid() != uid || metadata.mode() & 0o7777 != grants::VIEWS_MODE {
        std::os::unix::fs::fchown(&directory, Some(0), Some(uid)).map_err(|e| e.to_string())?;
        directory
            .set_permissions(fs::Permissions::from_mode(grants::VIEWS_MODE))
            .map_err(|e| e.to_string())?;
    }
    Ok(directory)
}

/// Every view directory below the account's, with the mounts at it. Its
/// manifest and staging are not views; any other entry refuses.
fn present_views(views: &File, uid: u32, mounts: &[MountRow]) -> Result<Vec<Present>, String> {
    let mut present = Vec::new();
    for (count, entry) in fs::read_dir(pinned(views, ""))
        .map_err(|e| e.to_string())?
        .enumerate()
    {
        if count >= MAX_ENTRIES {
            return Err("a grant view directory holds too many entries".into());
        }
        let entry = entry.map_err(|e| e.to_string())?;
        let name = entry.file_name();
        let name = name.to_str().ok_or("unexpected grant view entry")?;
        if name == grants::MANIFEST_NAME || name == STAGED_MANIFEST {
            continue;
        }
        let index = name
            .parse::<usize>()
            .ok()
            .filter(|index| index.to_string() == name && *index < grants::MAX_VIEWS)
            .ok_or_else(|| format!("unexpected grant view entry {name:?}"))?;
        let mountpoint = PathBuf::from(format!("{}/{uid}/{index}", grants::VIEW_ROOT));
        let at: Vec<MountRow> = mounts
            .iter()
            .filter(|row| row.mountpoint == mountpoint)
            .cloned()
            .collect();
        let shown = if at.len() == 1 {
            let metadata = fs::symlink_metadata(pinned(views, name)).map_err(|e| e.to_string())?;
            Some((
                metadata.dev(),
                metadata.ino(),
                metadata.uid(),
                metadata.gid(),
            ))
        } else {
            None
        };
        present.push(Present {
            index,
            mounts: at,
            shown,
        });
    }
    present.sort_by_key(|view| view.index);
    Ok(present)
}

/// Unmounts the mounts stacked at `mountpoint`, one at a time, until the
/// table shows none, within a bound.
pub(crate) fn unmount_all(mountpoint: &str) -> Result<(), String> {
    let path = Path::new(mountpoint);
    for _ in 0..=grants::MAX_VIEWS {
        if !mount_rows(&mount_table()?)?
            .iter()
            .any(|row| row.mountpoint == path)
        {
            return Ok(());
        }
        portal_files::unmount(mountpoint)?;
    }
    Err(format!("{mountpoint} stays mounted"))
}

fn remove_file(directory: &File, name: &str) -> Result<(), String> {
    match fs::remove_file(pinned(directory, name)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("remove {name}: {error}")),
    }
}

/// Publishes `manifest` in `directory`, owned by `owner` at its mode, so
/// only root writes it and the account's group alone reads it: staged
/// private, given its owner and mode, synced and renamed over the old one,
/// which is never removed first.
fn write_manifest(directory: &File, owner: (u32, u32), manifest: &Manifest) -> Result<(), String> {
    remove_file(directory, STAGED_MANIFEST)?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(O_NOFOLLOW)
        .open(pinned(directory, STAGED_MANIFEST))
        .map_err(|e| format!("stage the grant view manifest: {e}"))?;
    std::os::unix::fs::fchown(&file, Some(owner.0), Some(owner.1)).map_err(|e| e.to_string())?;
    file.set_permissions(fs::Permissions::from_mode(grants::MANIFEST_MODE))
        .map_err(|e| e.to_string())?;
    file.write_all(manifest.encode().as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(|e| format!("write the grant view manifest: {e}"))?;
    fs::rename(
        pinned(directory, STAGED_MANIFEST),
        pinned(directory, grants::MANIFEST_NAME),
    )
    .map_err(|e| format!("publish the grant view manifest: {e}"))
}

/// The supervisor calls this after all services stop and before unmounting
/// /var: every mount below the grant view tree goes, deepest first.
pub(crate) fn release_all() -> Result<(), String> {
    launch::require_launch_startup()?;
    let prefix = Path::new(grants::VIEW_ROOT);
    let mut mountpoints: Vec<PathBuf> = mount_rows(&mount_table()?)?
        .into_iter()
        .map(|row| row.mountpoint)
        .filter(|mountpoint| mountpoint != prefix && mountpoint.starts_with(prefix))
        .collect();
    mountpoints.sort_by(|left, right| {
        let depth = |path: &PathBuf| path.components().count();
        depth(right).cmp(&depth(left)).then(right.cmp(left))
    });
    mountpoints.dedup();
    for mountpoint in &mountpoints {
        let text = mountpoint
            .to_str()
            .ok_or("a grant view mountpoint is not UTF-8")?;
        unmount_all(text)?;
    }
    if mount_rows(&mount_table()?)?
        .iter()
        .any(|row| row.mountpoint != prefix && row.mountpoint.starts_with(prefix))
    {
        return Err("grant view mounts remain after unmount".into());
    }
    Ok(())
}

#[cfg(test)]
#[path = "../tests/grant_views.rs"]
mod tests;

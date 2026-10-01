//! Standalone host adapter: the invoking desktop account's vault directory
//! and its security keys under the transport's desktop admission. It trusts
//! the host kernel, compositor and account.

use super::lifecycle::{Directory, Error, Presented, TokenError};
use crate::fido_device::{self, Cancellation, Device, Session, MAX_LIFETIME};
use std::collections::VecDeque;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::time::Instant;

const NOFOLLOW: i32 = 0o400000;
const DIRECTORY: i32 = 0o200000;
// Linux's encoding of character device 1:9, /dev/urandom.
const URANDOM: u64 = 0x109;

/// Evidence that `protect` succeeded; opening a token requires it.
pub(super) struct Protected(());

/// Refuses unless this is an ordinary desktop account whose PINs and vault
/// keys can stay out of dumps and swap: the process becomes non-dumpable,
/// swap must be off and the core-dump soft limit zero, as for the manual
/// token diagnostic. `open` rechecks memory before every presentation.
pub(super) fn protect() -> Result<Protected, String> {
    fido_device::desktop_account()?;
    crate::secret_request::require_protected_memory()?;
    crate::pin_sys::protect_process().map_err(|_| "disable process dumps")?;
    Ok(Protected(()))
}

/// The account's vault directory: `$XDG_DATA_HOME/td-pass`, or
/// `$HOME/.local/share/td-pass`. A relative value is ignored, as the XDG
/// base directory specification requires.
pub(super) fn location(data_home: Option<OsString>, home: Option<OsString>) -> Option<PathBuf> {
    let absolute = |value: Option<OsString>| value.map(PathBuf::from).filter(|p| p.is_absolute());
    let base = match absolute(data_home) {
        Some(base) => base,
        None => absolute(home)?.join(".local/share"),
    };
    Some(base.join("td-pass"))
}

// Only the account or root may rename entries here: owned by one of them
// and writable by no one else, unless sticky.
fn controlled(uid: u32, mode: u32, owner: u32) -> bool {
    (uid == owner || uid == 0) && (mode & 0o022 == 0 || mode & 0o1000 != 0)
}

// A link is followed only if the account or root made it; another account
// may create one in a sticky directory it cannot otherwise change.
fn trusted_link(uid: u32, owner: u32) -> bool {
    uid == owner || uid == 0
}

// Names `name` inside the held directory, so the kernel resolves it from
// that descriptor rather than from a path another account could change.
fn beneath(dir: &File, name: &OsStr) -> PathBuf {
    Path::new(&format!("/proc/self/fd/{}", dir.as_raw_fd())).join(name)
}

fn open_beneath(dir: &File, name: &OsStr) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(NOFOLLOW | DIRECTORY)
        .open(beneath(dir, name))
}

// Creates `name` at mode 0700 unless present; true when this call made it.
fn make_beneath(dir: &File, name: &OsStr) -> io::Result<bool> {
    match fs::DirBuilder::new().mode(0o700).create(beneath(dir, name)) {
        Ok(()) => dir.sync_all().map(|()| true),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(false),
        Err(error) => Err(error),
    }
}

/// Opens the account's private vault directory, creating it and missing
/// ancestors with mode 0700. The absolute path is walked one component at
/// a time from `/` through held descriptors, so what is checked is what is
/// opened. Every directory on the way must be controlled, so no other
/// account can rename the vault aside or back; a link is followed only if
/// the account or root owns it, and at most 40 are. The final component is
/// a directory, not a link, owned by `owner` at 0700. Every held directory
/// is synced on each open, which also completes an earlier creation whose
/// sync did not finish; one on a filesystem that cannot sync a directory,
/// as a read-only one, is passed over, while a directory this call
/// creates must have its parent synced. Mode bits are the whole check: an
/// ACL granting another account write is not seen, and a namespace that
/// shows `/` under an unmapped owner fails closed.
pub(super) fn directory(path: &Path, owner: u32) -> Result<Directory, Error> {
    directory_below(path, owner, Path::new("/"))
}

const MAX_LINKS: usize = 40;

enum Step {
    Name(OsString),
    Up,
}

// A relative path's walk; `.` and a leading `/` name no step.
fn steps(path: &Path) -> VecDeque<Step> {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(name) => Some(Step::Name(name.to_os_string())),
            Component::ParentDir => Some(Step::Up),
            _ => None,
        })
        .collect()
}

// Tests walk from their own scratch base, whose owner a sandbox may not
// map, and resolve absolute links only beneath it.
fn directory_below(path: &Path, owner: u32, base: &Path) -> Result<Directory, Error> {
    let failed = |what: &str| Error::Store(format!("{what} the vault directory"));
    let private = || Error::State("the vault directory must be private to this account");
    let uncontrolled = || {
        Error::State(
            "every directory and link above the vault must be controlled by this account or root",
        )
    };
    let under = |path: &Path| {
        path.strip_prefix(base)
            .map(steps)
            .map_err(|_| failed("locate"))
    };
    if !path.is_absolute() || path.file_name().is_none() {
        return Err(failed("locate"));
    }
    let mut pending = under(path)?;
    let name = match pending.pop_back() {
        Some(Step::Name(name)) => name,
        _ => return Err(failed("locate")),
    };
    let root = OpenOptions::new()
        .read(true)
        .custom_flags(DIRECTORY)
        .open(base)
        .map_err(|_| failed("open a parent of"))?;
    let inspect = |dir: &File| dir.metadata().map_err(|_| failed("inspect a parent of"));
    if base == Path::new("/") {
        let meta = inspect(&root)?;
        if !controlled(meta.uid(), meta.mode(), owner) {
            return Err(uncontrolled());
        }
    }
    let mut held = vec![root];
    let mut links = 0;
    while let Some(component) = pending.pop_front() {
        let parent = held.last().ok_or_else(|| failed("open a parent of"))?;
        let step = match component {
            Step::Name(step) => step,
            Step::Up => {
                if held.len() > 1 {
                    held.pop();
                }
                continue;
            }
        };
        let step = step.as_os_str();
        let meta = match fs::symlink_metadata(beneath(parent, step)) {
            Ok(meta) => meta,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                make_beneath(parent, step).map_err(|_| failed("create a parent of"))?;
                fs::symlink_metadata(beneath(parent, step))
                    .map_err(|_| failed("inspect a parent of"))?
            }
            Err(_) => return Err(failed("inspect a parent of")),
        };
        if meta.file_type().is_symlink() {
            links += 1;
            if links > MAX_LINKS {
                return Err(failed("resolve"));
            }
            if !trusted_link(meta.uid(), owner) {
                return Err(uncontrolled());
            }
            let target =
                fs::read_link(beneath(parent, step)).map_err(|_| failed("resolve a parent of"))?;
            let mut spliced = if target.is_absolute() {
                held.truncate(1);
                under(&target)?
            } else {
                steps(&target)
            };
            spliced.extend(pending);
            pending = spliced;
            continue;
        }
        let dir = open_beneath(parent, step).map_err(|_| failed("open a parent of"))?;
        let meta = inspect(&dir)?;
        if !controlled(meta.uid(), meta.mode(), owner) {
            return Err(uncontrolled());
        }
        held.push(dir);
    }
    let parent = held.last().ok_or_else(|| failed("open a parent of"))?;
    let created = make_beneath(parent, &name).map_err(|_| failed("create"))?;
    for dir in &held {
        match dir.sync_all() {
            Ok(()) => {}
            // EINVAL or EROFS: a filesystem that cannot sync a directory,
            // as a read-only one. A creation there that never synced stays
            // unrecorded, but the store's own sync fails there too, so no
            // vault is published through it.
            Err(error) if matches!(error.raw_os_error(), Some(22 | 30)) => {}
            Err(_) => return Err(failed("record")),
        }
    }
    let file = open_beneath(parent, &name).map_err(|error| match error.raw_os_error() {
        // ELOOP for a final link, ENOTDIR for a non-directory.
        Some(40 | 20) => private(),
        _ => failed("open"),
    })?;
    let inherited = file
        .metadata()
        .is_ok_and(|meta| meta.uid() == owner && meta.mode() & 0o7777 == 0o2700);
    if created || inherited {
        // A setgid parent passes its bit to a new directory; this also
        // repairs one left by an interrupted earlier creation.
        file.set_permissions(fs::Permissions::from_mode(0o700))
            .map_err(|_| failed("restrict"))?;
    }
    let meta = file.metadata().map_err(|_| failed("inspect"))?;
    if !meta.is_dir() || meta.uid() != owner || meta.mode() & 0o7777 != 0o700 {
        return Err(private());
    }
    Ok(Directory { file, owner })
}

/// The account's vault directory at its standard location.
pub(super) fn account_directory() -> Result<Directory, Error> {
    let owner = fido_device::desktop_account().map_err(Error::Refused)?;
    let path = location(std::env::var_os("XDG_DATA_HOME"), std::env::var_os("HOME"))
        .ok_or(Error::State("no home directory for the vault"))?;
    directory(&path, owner)
}

/// Exactly one connected token, or why not; this does not guess which of
/// several is wanted.
pub(super) fn choose(found: &[Device]) -> Result<Device, TokenError> {
    match found {
        [device] => Ok(*device),
        [] => Err(TokenError::Unavailable),
        _ => Err(TokenError::Several),
    }
}

/// Opens the one connected token for a single bounded operation that
/// `cancellation` can end, after rechecking that memory is still protected.
pub(super) fn open(
    _protected: &Protected,
    _presented: Presented<'_>,
    cancellation: &Cancellation,
) -> Result<Session, TokenError> {
    crate::secret_request::require_protected_memory()
        .map_err(|_| TokenError::Host("swap or core dumps became enabled"))?;
    let found = Device::discover_desktop()
        .map_err(|_| TokenError::Host("this process is not an ordinary desktop account"))?;
    let device = choose(&found)?;
    let deadline = Instant::now()
        .checked_add(MAX_LIFETIME)
        .ok_or(TokenError::Unavailable)?;
    // The worker's open is the kernel's answer to the host's device policy.
    Session::open_cancellable(device, deadline, cancellation.clone()).map_err(|error| {
        if fido_device::denied(&error) {
            TokenError::Denied
        } else {
            TokenError::Unavailable
        }
    })
}

/// The kernel's random device, checked by device number, read directly
/// into the caller's buffer.
pub(super) struct Entropy(File);

impl Entropy {
    pub fn open() -> Result<Self, String> {
        Self::open_device(Path::new("/dev/urandom"))
    }

    fn open_device(path: &Path) -> Result<Self, String> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(NOFOLLOW)
            .open(path)
            .map_err(|_| "open kernel entropy")?;
        let meta = file.metadata().map_err(|_| "inspect kernel entropy")?;
        if !meta.file_type().is_char_device() || meta.rdev() != URANDOM {
            return Err("kernel entropy is not the random device".into());
        }
        Ok(Self(file))
    }

    pub fn fill(&mut self, bytes: &mut [u8]) -> Result<(), String> {
        self.0
            .read_exact(bytes)
            .map_err(|_| "read kernel entropy".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn location_follows_xdg_and_ignores_relative_values() {
        let some = |value: &str| Some(OsString::from(value));
        for (data_home, home, expected) in [
            (some("/data"), some("/home/a"), Some("/data/td-pass")),
            (
                some("data"),
                some("/home/a"),
                Some("/home/a/.local/share/td-pass"),
            ),
            (None, some("/home/a"), Some("/home/a/.local/share/td-pass")),
            (
                some(""),
                some("/home/a"),
                Some("/home/a/.local/share/td-pass"),
            ),
            (None, some("home"), None),
            (None, None, None),
        ] {
            assert_eq!(
                location(data_home.clone(), home.clone()),
                expected.map(PathBuf::from),
                "{data_home:?} {home:?}"
            );
        }
    }

    #[test]
    fn exactly_one_connected_token_is_chosen() {
        let device = Device::synthetic;
        assert!(choose(&[device(1)]).is_ok_and(|chosen| chosen == device(1)));
        assert_eq!(choose(&[]).err(), Some(TokenError::Unavailable));
        assert_eq!(
            choose(&[device(1), device(2)]).err(),
            Some(TokenError::Several)
        );
    }

    // Walks from `root`'s parent, checking every directory from `root` on.
    fn within(root: &Path, path: &Path, owner: u32) -> Result<Directory, Error> {
        directory_below(path, owner, root.parent().unwrap())
    }

    const UNCONTROLLED: Error = Error::State(
        "every directory and link above the vault must be controlled by this account or root",
    );

    #[test]
    fn only_the_account_or_root_may_supply_a_link() {
        assert!(trusted_link(1000, 1000));
        assert!(trusted_link(0, 1000));
        assert!(!trusted_link(1001, 1000));
    }

    #[test]
    fn the_walk_follows_relative_links_and_parent_steps() {
        let root = scratch();
        let owner = fs::metadata(&root).unwrap().uid();
        fs::create_dir_all(root.join("real/inner")).unwrap();
        std::os::unix::fs::symlink("real/inner", root.join("rel")).unwrap();
        let opened = within(&root, &root.join("rel/../inner/td-pass"), owner).unwrap();
        let place = |meta: fs::Metadata| (meta.dev(), meta.ino());
        assert_eq!(
            place(opened.file.metadata().unwrap()),
            place(fs::metadata(root.join("real/inner/td-pass")).unwrap())
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn link_loops_relative_paths_and_escaping_links_are_refused() {
        let root = scratch();
        let owner = fs::metadata(&root).unwrap().uid();
        let resolve = Some(Error::Store("resolve the vault directory".into()));
        std::os::unix::fs::symlink("loop", root.join("loop")).unwrap();
        assert_eq!(
            within(&root, &root.join("loop/td-pass"), owner).err(),
            resolve
        );
        let locate = Some(Error::Store("locate the vault directory".into()));
        for path in ["relative/td-pass", "/", "/x/.."] {
            assert_eq!(within(&root, Path::new(path), owner).err(), locate);
        }
        // Absolute targets resolve beneath the walk's base, here the
        // scratch parent; one outside it is refused.
        std::os::unix::fs::symlink("/proc", root.join("out")).unwrap();
        assert_eq!(
            within(&root, &root.join("out/td-pass"), owner).err(),
            locate
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn only_the_account_or_root_may_rename_entries() {
        for (uid, mode, admitted) in [
            (1000, 0o700, true),
            (1000, 0o755, true),
            (0, 0o755, true),
            (0, 0o1777, true),
            (1000, 0o1777, true),
            (1000, 0o775, false),
            (1000, 0o757, false),
            (0, 0o777, false),
            (1001, 0o700, false),
            (1001, 0o1777, false),
        ] {
            assert_eq!(controlled(uid, mode, 1000), admitted, "{uid} {mode:o}");
        }
    }

    #[test]
    fn a_writable_higher_ancestor_is_refused() {
        let root = scratch();
        let owner = fs::metadata(&root).unwrap().uid();
        let path = root.join("upper/lower/td-pass");
        within(&root, &path, owner).unwrap();
        let upper = root.join("upper");
        fs::set_permissions(&upper, fs::Permissions::from_mode(0o777)).unwrap();
        assert_eq!(
            within(&root, &path, owner).err(),
            Some(Error::State(
                "every directory and link above the vault must be controlled by this account or root",
            ))
        );
        // Reached through a link, the resolved path is what is checked.
        let link = root.join("link");
        std::os::unix::fs::symlink(root.join("upper/lower"), &link).unwrap();
        assert!(within(&root, &link.join("td-pass"), owner).is_err());
        fs::set_permissions(&upper, fs::Permissions::from_mode(0o1777)).unwrap();
        within(&root, &link.join("td-pass"), owner).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn an_interrupted_setgid_creation_is_repaired() {
        let root = scratch();
        let owner = fs::metadata(&root).unwrap().uid();
        let path = root.join("td-pass");
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o2700)).unwrap();
        within(&root, &path, owner).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o7777, 0o700);
        // Any other mode is the account's choice, not repaired.
        fs::set_permissions(&path, fs::Permissions::from_mode(0o2750)).unwrap();
        assert!(within(&root, &path, owner).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    fn scratch() -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "td-secret-host-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        drop(fs::remove_dir_all(&root));
        fs::create_dir(&root).unwrap();
        // Independent of the developer's umask, which may leave group write.
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
        fs::canonicalize(root).unwrap()
    }

    #[test]
    fn the_vault_directory_is_created_private_and_reopened() {
        let root = scratch();
        let owner = fs::metadata(&root).unwrap().uid();
        let path = root.join("data/share/td-pass");
        let created = within(&root, &path, owner).unwrap();
        for dir in [root.join("data"), root.join("data/share"), path.clone()] {
            assert_eq!(
                fs::metadata(&dir).unwrap().mode() & 0o7777,
                0o700,
                "{dir:?}"
            );
        }
        assert_eq!(created.owner, owner);
        assert!(created.file.metadata().unwrap().is_dir());
        within(&root, &path, owner).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_shared_foreign_or_linked_vault_directory_is_refused() {
        let root = scratch();
        let owner = fs::metadata(&root).unwrap().uid();
        let path = root.join("td-pass");
        within(&root, &path, owner).unwrap();
        assert!(within(&root, &path, owner.wrapping_add(1)).is_err());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o750)).unwrap();
        assert!(within(&root, &path, owner).is_err());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        let private = Some(Error::State(
            "the vault directory must be private to this account",
        ));
        let link = root.join("linked");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert_eq!(within(&root, &link, owner).err(), private);
        let file = root.join("file");
        fs::write(&file, b"").unwrap();
        assert_eq!(within(&root, &file, owner).err(), private);
        // A linked prefix is the account's own layout.
        let prefix = root.join("prefix");
        std::os::unix::fs::symlink(&root, &prefix).unwrap();
        within(&root, &prefix.join("td-pass"), owner).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn the_parent_must_be_controlled_by_the_account() {
        let root = scratch();
        let owner = fs::metadata(&root).unwrap().uid();
        let parent = root.join("parent");
        fs::create_dir(&parent).unwrap();
        let path = parent.join("td-pass");
        let refused = Some(Error::State(
            "every directory and link above the vault must be controlled by this account or root",
        ));
        for mode in [0o777, 0o775, 0o757] {
            fs::set_permissions(&parent, fs::Permissions::from_mode(mode)).unwrap();
            assert_eq!(within(&root, &path, owner).err(), refused, "{mode:o}");
        }
        // Another account cannot rename entries out of a sticky directory.
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o1777)).unwrap();
        within(&root, &path, owner).unwrap();
        // A parent owned by neither this account nor root; as root, any
        // owner sees a root-owned parent as controlled.
        if owner != 0 {
            assert_eq!(within(&root, &path, owner.wrapping_add(1)).err(), refused);
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_setgid_parent_does_not_leave_the_vault_directory_unusable() {
        let root = scratch();
        let owner = fs::metadata(&root).unwrap().uid();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o2755)).unwrap();
        let path = root.join("td-pass");
        within(&root, &path, owner).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o7777, 0o700);
        within(&root, &path, owner).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn entropy_reads_only_the_kernel_random_device() {
        let mut entropy = Entropy::open().unwrap();
        let mut bytes = [0; 64];
        entropy.fill(&mut bytes).unwrap();
        assert_ne!(bytes, [0; 64]);
        // /dev/zero is a character device with another number.
        assert!(Entropy::open_device(Path::new("/dev/zero")).is_err());
        assert!(Entropy::open_device(Path::new("/proc/self/status")).is_err());
    }
}

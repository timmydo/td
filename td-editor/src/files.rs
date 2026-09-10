//! Synchronous, worker-owned file transactions. No document mutation or UI.

use crate::text;
use std::collections::BTreeMap;
use std::fs::{self, File, Metadata, OpenOptions, Permissions};
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{
    fchown, DirBuilderExt, FileExt, MetadataExt, OpenOptionsExt, PermissionsExt,
};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const O_NOFOLLOW: i32 = 0o400000;
const O_NONBLOCK: i32 = 0o4000;
const O_DIRECTORY: i32 = 0o200000;
const O_PATH: i32 = 0o10000000;
const BASELINE_BYTES: usize = 64 * 1024 * 1024;
const MAX_FILES: usize = 64;
static TEMP_SERIAL: AtomicU64 = AtomicU64::new(1);

pub type FileId = u64;
type Result<T> = std::result::Result<T, Failure>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Kind {
    Io,
    InvalidPath,
    InvalidText,
    NotRegular,
    Metadata,
    Conflict,
    Exists,
    Limit,
    MissingAssociation,
}

/// A publication failure is not evidence that the old destination survived.
/// Never acknowledge a model save on any Err, even when published is true.
#[derive(Debug)]
pub struct Failure {
    pub kind: Kind,
    pub published: bool,
    pub publication_attempted: bool,
    pub residual: Option<PathBuf>,
    detail: String,
}

impl Failure {
    fn new(kind: Kind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            published: false,
            publication_attempted: false,
            residual: None,
            detail: detail.into(),
        }
    }
}

impl From<io::Error> for Failure {
    fn from(error: io::Error) -> Self {
        Self::new(Kind::Io, error.to_string())
    }
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}: {}", self.kind, self.detail)?;
        if self.published {
            f.write_str("; destination was published; save completion is unconfirmed")?;
        } else if self.publication_attempted {
            f.write_str("; publication returned an error; verify destination before retrying")?;
        }
        if let Some(path) = &self.residual {
            write!(
                f,
                "; temporary cleanup unconfirmed at {:?}; verify ownership before removal",
                path
            )?;
        }
        Ok(())
    }
}
impl std::error::Error for Failure {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Stamp {
    dev: u64,
    ino: u64,
    uid: u32,
    gid: u32,
    mode: u32,
    links: u64,
    len: u64,
    mtime: (i64, i64),
    ctime: (i64, i64),
}

impl Stamp {
    pub(crate) fn read(meta: &Metadata) -> Self {
        Self {
            dev: meta.dev(),
            ino: meta.ino(),
            uid: meta.uid(),
            gid: meta.gid(),
            mode: meta.mode(),
            links: meta.nlink(),
            len: meta.len(),
            mtime: (meta.mtime(), meta.mtime_nsec()),
            ctime: (meta.ctime(), meta.ctime_nsec()),
        }
    }
    fn identity(&self) -> (u64, u64) {
        (self.dev, self.ino)
    }
    fn replaceable(&self) -> Result<()> {
        if self.links != 1 || self.mode & 0o6000 != 0 {
            return Err(Failure::new(
                Kind::Metadata,
                "replacement requires one link and no setuid/setgid bits; use Save As",
            ));
        }
        Ok(())
    }
}

#[derive(Debug)]
struct Location {
    path: PathBuf,
    parent: File,
    parent_identity: (u64, u64),
}

impl Location {
    fn resolve(path: &Path) -> Result<Self> {
        let raw = path.as_os_str().as_bytes();
        let leaf = raw.rsplit(|b| *b == b'/').next().unwrap_or_default();
        if raw.len() > 4096 || raw.contains(&0) || matches!(leaf, b"" | b"." | b"..") {
            return Err(Failure::new(
                Kind::InvalidPath,
                "expected a literal file path of at most 4096 bytes",
            ));
        }
        let name = path
            .file_name()
            .ok_or_else(|| Failure::new(Kind::InvalidPath, "missing filename"))?;
        let parent_path = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let parent_path = fs::canonicalize(parent_path)?;
        let parent = OpenOptions::new()
            .read(true)
            .custom_flags(O_DIRECTORY | O_NOFOLLOW)
            .open(&parent_path)?;
        let meta = parent.metadata()?;
        let path = parent_path.join(name);
        if path.as_os_str().as_bytes().len() > 4096 {
            return Err(Failure::new(
                Kind::InvalidPath,
                "resolved path exceeds 4096 bytes",
            ));
        }
        let location = Self {
            path,
            parent,
            parent_identity: (meta.dev(), meta.ino()),
        };
        location.check_parent()?;
        Ok(location)
    }
    fn check_parent(&self) -> Result<()> {
        let path = self
            .path
            .parent()
            .ok_or_else(|| Failure::new(Kind::InvalidPath, "missing parent"))?;
        let meta = fs::symlink_metadata(path)?;
        if !meta.is_dir() || (meta.dev(), meta.ino()) != self.parent_identity {
            return Err(Failure::new(
                Kind::Conflict,
                "destination directory changed",
            ));
        }
        Ok(())
    }
}

struct Entry {
    location: Location,
    stamp: Option<Stamp>,
    // Pin the inode so removal cannot recycle its identity into another file.
    _baseline_file: Option<File>,
    bytes: Vec<u8>,
    rename_uncertain: bool,
}

/// A no-follow source observation. A mutation refuses if this observation is
/// stale; callers must capture it when selecting the entry, not on approval.
#[derive(Clone)]
pub struct RenameSource {
    path: PathBuf,
    parent_identity: (u64, u64),
    stamp: Stamp,
}

impl RenameSource {
    pub(crate) fn copy_destination(&self, name: &std::ffi::OsStr) -> Result<PathBuf> {
        let raw = name.as_bytes();
        let leaf = raw.rsplit(|b| *b == b'/').next().unwrap_or_default();
        if raw.len() > 4096 || raw.contains(&0) || matches!(leaf, b"" | b"." | b"..") {
            return Err(Failure::new(
                Kind::InvalidPath,
                "destination requires a new filename, not a trailing slash or dot basename",
            ));
        }
        let parent = self
            .path
            .parent()
            .ok_or_else(|| Failure::new(Kind::InvalidPath, "copy source has no parent"))?;
        let path = parent.join(name);
        if path.as_os_str().as_bytes().len() > 4096 {
            return Err(Failure::new(
                Kind::InvalidPath,
                "destination path exceeds 4096 bytes",
            ));
        }
        Ok(path)
    }

    pub fn inspect(path: &Path) -> Result<Self> {
        let location = Location::resolve(path)?;
        let stamp = Stamp::read(&fs::symlink_metadata(&location.path)?);
        location.check_parent()?;
        Ok(Self::observed(
            location.path,
            location.parent_identity,
            stamp,
        ))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn observed(path: PathBuf, parent_identity: (u64, u64), stamp: Stamp) -> Self {
        Self {
            path,
            parent_identity,
            stamp,
        }
    }

    fn check(&self, location: &Location, node: &File) -> Result<()> {
        location.check_parent()?;
        if location.path != self.path
            || location.parent_identity != self.parent_identity
            || Stamp::read(&node.metadata()?) != self.stamp
            || Stamp::read(&fs::symlink_metadata(&location.path)?) != self.stamp
        {
            return Err(Failure::new(
                Kind::Conflict,
                "directory entry changed; refresh and retry",
            ));
        }
        Ok(())
    }
}

/// Identity of the directory displayed when creation was requested.
#[derive(Clone)]
pub struct DirectorySource {
    path: PathBuf,
    identity: (u64, u64),
}

impl DirectorySource {
    pub fn inspect(path: &Path) -> Result<Self> {
        let path = fs::canonicalize(path)?;
        if path.as_os_str().as_bytes().len() > 4096 {
            return Err(Failure::new(
                Kind::InvalidPath,
                "directory path exceeds 4096 bytes",
            ));
        }
        let node = OpenOptions::new()
            .read(true)
            .custom_flags(O_DIRECTORY | O_NOFOLLOW)
            .open(&path)?;
        let metadata = node.metadata()?;
        Ok(Self::observed(path, (metadata.dev(), metadata.ino())))
    }
    pub(crate) fn observed(path: PathBuf, identity: (u64, u64)) -> Self {
        Self { path, identity }
    }

    pub(crate) fn destination(&self, name: &std::ffi::OsStr) -> Result<PathBuf> {
        let raw = name.as_bytes();
        if raw.is_empty()
            || raw.len() > 4096
            || matches!(raw, b"." | b"..")
            || raw.contains(&b'/')
            || raw.contains(&0)
        {
            return Err(Failure::new(
                Kind::InvalidPath,
                "creation requires a new basename, not a path",
            ));
        }
        let path = self.path.join(name);
        if path.as_os_str().as_bytes().len() > 4096 {
            return Err(Failure::new(
                Kind::InvalidPath,
                "created path exceeds 4096 bytes",
            ));
        }
        Ok(path)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Kernel creation succeeded; warnings never trigger cleanup or retry.
pub struct CreatedDirectory {
    pub path: PathBuf,
    pub warning: Option<String>,
}

/// Kernel publication succeeded. No association or saved baseline changes.
pub struct Copied {
    pub path: PathBuf,
    pub warning: Option<String>,
}

/// Immutable, ordered observations captured before deletion confirmation.
/// Shared rename observations pin names and metadata, not display strings.
#[derive(Clone)]
pub struct DeletePlan {
    sources: Vec<RenameSource>,
}

impl DeletePlan {
    pub fn new(sources: Vec<RenameSource>) -> Result<Self> {
        let first = sources
            .first()
            .ok_or_else(|| Failure::new(Kind::InvalidPath, "No entries marked"))?;
        if sources.len() > 64 {
            return Err(Failure::new(
                Kind::Limit,
                "At most 64 deletion marks are allowed",
            ));
        }
        let mut names = std::collections::BTreeSet::new();
        for source in &sources {
            if source.path.parent() != first.path.parent()
                || source.parent_identity != first.parent_identity
                || source.path.as_os_str().as_bytes().len() > 4096
                || !names.insert(source.path.clone())
            {
                return Err(Failure::new(
                    Kind::InvalidPath,
                    "Deletion needs unique entries in one observed directory",
                ));
            }
        }
        Ok(Self { sources })
    }

    pub fn paths(&self) -> impl Iterator<Item = &Path> {
        self.sources.iter().map(RenameSource::path)
    }

    pub fn len(&self) -> usize {
        self.sources.len()
    }

    pub fn is_empty(&self) -> bool {
        self.sources.is_empty()
    }
}

pub struct Deleted {
    pub removed: usize,
    pub requested: usize,
    pub parent: PathBuf,
    /// Stops at the first refusal, syscall error or durability uncertainty.
    /// Earlier successful removals are not rolled back or retried.
    pub failure: Option<String>,
}

/// Kernel publication succeeded. Even when durability/readback cannot be
/// confirmed, associations follow the new name; there is no rollback rename.
pub struct Renamed {
    pub from: PathBuf,
    pub to: PathBuf,
    pub warning: Option<String>,
}

impl Renamed {
    pub fn relocated(&self, path: &Path) -> Option<PathBuf> {
        let suffix = path.strip_prefix(&self.from).ok()?;
        Some(if suffix.as_os_str().is_empty() {
            self.to.clone()
        } else {
            self.to.join(suffix)
        })
    }
}

impl Entry {
    fn matches(&self, location: &Location, stamp: Option<&Stamp>) -> bool {
        self.location.path == location.path
            || match (&self.stamp, stamp) {
                (Some(a), Some(b)) => a.identity() == b.identity(),
                _ => false,
            }
    }
}

/// Own on one file worker. IDs are local to this session, not model tab IDs.
/// Dropping/removing an association never deletes its destination.
pub struct Session {
    entries: BTreeMap<FileId, Entry>,
    next: FileId,
    budget: usize,
}

/// A validated replacement, not yet the save baseline. Its exclusive session
/// borrow prevents intervening association changes. Drop to cancel; commit
/// only after the document accepts these bytes and its discard decision.
///
/// ```compile_fail,E0499
/// let mut files = td_editor::files::Session::default();
/// if let Ok(reload) = files.prepare_reload(1) {
///     files.forget(1); // the prepared replacement still owns the borrow
///     reload.commit();
/// }
/// files.forget(1);
/// ```
#[must_use = "dropping a prepared reload cancels it"]
pub struct Reload<'a> {
    session: &'a mut Session,
    original: FileId,
    replacement: FileId,
    entry: Entry,
}

impl Reload<'_> {
    pub fn file_id(&self) -> FileId {
        self.replacement
    }
    pub fn bytes(&self) -> &[u8] {
        &self.entry.bytes
    }
    pub fn path(&self) -> &Path {
        &self.entry.location.path
    }
    pub fn missing(&self) -> bool {
        self.entry.stamp.is_none()
    }
    /// No I/O or fallible admission remains after preparation. The new ID
    /// lets a worker distinguish accepted and rejected document completions.
    pub fn commit(self) -> FileId {
        self.session.entries.remove(&self.original);
        self.session.entries.insert(self.replacement, self.entry);
        self.replacement
    }
}

impl std::fmt::Debug for Reload<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Reload")
            .field("original", &self.original)
            .field("replacement", &self.replacement)
            .field("bytes", &self.entry.bytes.len())
            .field("missing", &self.missing())
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileSession")
            .field("associations", &self.entries.len())
            .field("baseline_bytes", &self.baseline_bytes())
            .finish_non_exhaustive()
    }
}

impl Default for Session {
    fn default() -> Self {
        Self {
            entries: BTreeMap::new(),
            next: 1,
            budget: BASELINE_BYTES,
        }
    }
}

impl Session {
    /// Permanently remove a confirmed batch, stopping on the first failure.
    /// Open associations are protected; directories are never recursive.
    pub fn delete(&mut self, plan: DeletePlan) -> Result<Deleted> {
        self.delete_with(plan, &mut |_, _| Ok(()))
    }

    fn delete_with(
        &mut self,
        plan: DeletePlan,
        hook: &mut dyn FnMut(Stage, &Path) -> io::Result<()>,
    ) -> Result<Deleted> {
        let parent = plan
            .paths()
            .next()
            .and_then(Path::parent)
            .ok_or_else(|| Failure::new(Kind::InvalidPath, "Deletion needs a parent"))?
            .to_owned();
        // Refuse the entire batch if any association could be affected.
        for source in &plan.sources {
            if self.entries.values().any(|entry| {
                entry.location.path.starts_with(&source.path)
                    || entry
                        .stamp
                        .as_ref()
                        .is_some_and(|stamp| stamp.identity() == source.stamp.identity())
            }) {
                return Err(Failure::new(
                    Kind::Exists,
                    "Deletion includes an open file or its parent; close that tab first",
                ));
            }
        }
        let mut result = Deleted {
            removed: 0,
            requested: plan.len(),
            parent,
            failure: None,
        };
        for (index, source) in plan.sources.iter().enumerate() {
            let mut attempted = false;
            let removal = (|| -> Result<()> {
                let location = Location::resolve(&source.path)?;
                let node = OpenOptions::new()
                    .read(true)
                    .custom_flags(O_PATH | O_NOFOLLOW)
                    .open(&location.path)?;
                source.check(&location, &node)?;
                let meta = node.metadata()?;
                if !meta.is_file() && !meta.is_dir() && !meta.is_symlink() {
                    return Err(Failure::new(
                        Kind::NotRegular,
                        "Deletion supports files, symlinks and empty directories only",
                    ));
                }
                hook(Stage::Publish, &location.path)?;
                source.check(&location, &node)?;
                attempted = true;
                if meta.is_dir() {
                    fs::remove_dir(&location.path)?;
                } else {
                    fs::remove_file(&location.path)?;
                }
                result.removed += 1;
                hook(Stage::SyncParent, &location.path)?;
                location.parent.sync_all()?;
                location.check_parent()?;
                hook(Stage::Readback, &location.path)?;
                match fs::symlink_metadata(&location.path) {
                    Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
                    Err(e) => Err(e.into()),
                    Ok(_) => Err(Failure::new(
                        Kind::Conflict,
                        "Name exists again after removal; not retried",
                    )),
                }
            })();
            if let Err(error) = removal {
                let quoted = format!(
                    "{:?}",
                    source.path.file_name().unwrap_or(source.path.as_os_str())
                );
                let mut chars = quoted.chars();
                let mut label: String = chars.by_ref().take(96).collect();
                if chars.next().is_some() {
                    label.push_str("...");
                }
                result.failure = Some(format!(
                    "{}; entry {}/{}: {error}; name {label}",
                    if attempted {
                        "removal attempted; verify disk state before retrying"
                    } else {
                        "this removal was not attempted"
                    },
                    index + 1,
                    plan.len()
                ));
                break;
            }
        }
        Ok(result)
    }

    /// Copy on-disk regular-file bytes to a new destination filename, without
    /// overwriting or saving any open buffer. Ordinary mode honors umask.
    pub fn copy(&mut self, source: RenameSource, name: &std::ffi::OsStr) -> Result<Copied> {
        self.copy_with(source, name, |_| Ok(()))
    }

    fn copy_with(
        &mut self,
        source: RenameSource,
        name: &std::ffi::OsStr,
        step: impl FnMut(Stage) -> io::Result<()>,
    ) -> Result<Copied> {
        self.copy_impl(source, name, &[], step)
    }

    pub(crate) fn copy_reserved(
        &mut self,
        source: RenameSource,
        name: &std::ffi::OsStr,
        reserved: &[PathBuf],
    ) -> Result<Copied> {
        self.copy_impl(source, name, reserved, |_| Ok(()))
    }

    fn copy_impl(
        &mut self,
        source: RenameSource,
        name: &std::ffi::OsStr,
        reserved: &[PathBuf],
        mut step: impl FnMut(Stage) -> io::Result<()>,
    ) -> Result<Copied> {
        let path = source.copy_destination(name)?;
        let origin = Location::resolve(&source.path)?;
        let (node, _) = open_regular(&origin.path)?
            .ok_or_else(|| Failure::new(Kind::Conflict, "copy source disappeared"))?;
        source.check(&origin, &node)?;
        let destination = Location::resolve(&path)?;
        let path = destination.path.clone();
        if self
            .entries
            .values()
            .any(|entry| entry.location.path == path)
            || reserved.iter().any(|reserved| reserved.starts_with(&path))
        {
            return Err(Failure::new(
                Kind::Exists,
                "copy destination belongs to an open association",
            ));
        }
        verify_destination(&destination, None, &[])?;
        let bytes = read_stable(&node, &source.stamp)?;
        source.check(&origin, &node)?;
        step(Stage::Metadata)?;
        let mode = copy_mode(&destination, source.stamp.mode & 0o777)?;
        let mut temporary = Temporary::create(&destination)?;
        let mut published = false;
        let mut attempted = false;
        let result = (|| -> Result<()> {
            require_no_attributes(&temporary.file)?;
            step(Stage::Write)?;
            temporary.file.write_all(&bytes)?;
            temporary
                .file
                .set_permissions(Permissions::from_mode(mode))?;
            let prepared = Stamp::read(&temporary.file.metadata()?);
            if prepared.mode & 0o7777 != mode {
                return Err(Failure::new(
                    Kind::Metadata,
                    "copy permissions did not match",
                ));
            }
            require_no_attributes(&temporary.file)?;
            step(Stage::SyncFile)?;
            temporary.file.sync_all()?;
            step(Stage::Recheck)?;
            compare_stable(&node, &source.stamp, &bytes)?;
            source.check(&origin, &node)?;
            verify_destination(&destination, None, &[])?;
            check_temporary(&temporary)?;
            step(Stage::Publish)?;
            attempted = true;
            fs::hard_link(&temporary.path, &destination.path).map_err(|error| {
                if error.kind() == io::ErrorKind::AlreadyExists {
                    Failure::new(Kind::Exists, "copy destination was created concurrently")
                } else {
                    Failure::from(error)
                }
            })?;
            published = true;
            step(Stage::Unlink)?;
            temporary.cleanup()?;
            step(Stage::SyncParent)?;
            destination.parent.sync_all()?;
            step(Stage::Readback)?;
            let stamp = Stamp::read(&temporary.file.metadata()?);
            stamp.replaceable()?;
            if stamp.uid != prepared.uid || stamp.gid != prepared.gid || stamp.mode != prepared.mode
            {
                return Err(Failure::new(
                    Kind::Conflict,
                    "published copy metadata changed",
                ));
            }
            require_no_attributes(&temporary.file)?;
            compare_stable(&temporary.file, &stamp, &bytes)?;
            check_name(&destination, &stamp)?;
            Ok(())
        })();
        let mut failure = result.err();
        if let Err(cleanup) = temporary.cleanup() {
            let error =
                failure.get_or_insert_with(|| Failure::new(Kind::Io, "copy cleanup failed"));
            error
                .detail
                .push_str(&format!("; temporary cleanup failed: {cleanup}"));
            error.residual = Some(temporary.path.clone());
        }
        if published {
            return Ok(Copied {
                path,
                warning: failure
                    .map(|error| format!("Copy published; confirmation failed: {error}")),
            });
        }
        let mut failure =
            failure.unwrap_or_else(|| Failure::new(Kind::Io, "copy was not published"));
        failure.publication_attempted = attempted;
        Err(failure)
    }

    /// Create one private directory under the captured parent, without
    /// replacing existing names or retrying uncertain publication.
    pub fn create_directory(
        &mut self,
        source: DirectorySource,
        name: &std::ffi::OsStr,
    ) -> Result<CreatedDirectory> {
        self.create_directory_with(source, name, |_| Ok(()))
    }

    fn create_directory_with(
        &mut self,
        source: DirectorySource,
        name: &std::ffi::OsStr,
        mut step: impl FnMut(Stage) -> io::Result<()>,
    ) -> Result<CreatedDirectory> {
        let path = source.destination(name)?;
        let location = Location::resolve(&path)?;
        if location.path != path || location.parent_identity != source.identity {
            return Err(Failure::new(
                Kind::Conflict,
                "directory changed; refresh and retry",
            ));
        }
        if self
            .entries
            .values()
            .any(|entry| entry.location.path.starts_with(&path))
        {
            return Err(Failure::new(
                Kind::Exists,
                "destination belongs to an open association",
            ));
        }
        step(Stage::Publish)?;
        location.check_parent()?;
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .map_err(|error| {
                let mut failure = if error.kind() == io::ErrorKind::AlreadyExists {
                    Failure::new(Kind::Exists, "creation never replaces an existing name")
                } else {
                    Failure::from(error)
                };
                failure.publication_attempted = true;
                failure
            })?;
        let confirmed = (|| -> Result<()> {
            let node = OpenOptions::new()
                .read(true)
                .custom_flags(O_DIRECTORY | O_NOFOLLOW)
                .open(&path)?;
            let stamp = Stamp::read(&node.metadata()?);
            if stamp.mode & 0o077 != 0 {
                return Err(Failure::new(
                    Kind::Metadata,
                    "created directory is not private",
                ));
            }
            node.sync_all()?;
            step(Stage::SyncParent)?;
            location.parent.sync_all()?;
            step(Stage::Readback)?;
            location.check_parent()?;
            if Stamp::read(&fs::symlink_metadata(&path)?) != stamp {
                return Err(Failure::new(
                    Kind::Conflict,
                    "created directory changed during confirmation",
                ));
            }
            Ok(())
        })();
        Ok(CreatedDirectory {
            path,
            warning: confirmed
                .err()
                .map(|error| format!("Directory created; confirmation failed: {error}")),
        })
    }

    /// Rename an entry, or move a regular file, preserving its inode and bytes.
    /// Existing destinations and paths reserved by other tabs are refused.
    pub fn rename(&mut self, source: RenameSource, name: &std::ffi::OsStr) -> Result<Renamed> {
        self.rename_with(source, name, |_| Ok(()))
    }

    fn rename_with(
        &mut self,
        source: RenameSource,
        name: &std::ffi::OsStr,
        step: impl FnMut(Stage) -> io::Result<()>,
    ) -> Result<Renamed> {
        self.rename_impl(source, name, &[], step)
    }

    pub(crate) fn rename_reserved(
        &mut self,
        source: RenameSource,
        name: &std::ffi::OsStr,
        reserved: &[PathBuf],
    ) -> Result<Renamed> {
        self.rename_impl(source, name, reserved, |_| Ok(()))
    }

    fn rename_impl(
        &mut self,
        source: RenameSource,
        name: &std::ffi::OsStr,
        reserved: &[PathBuf],
        mut step: impl FnMut(Stage) -> io::Result<()>,
    ) -> Result<Renamed> {
        let requested = source.copy_destination(name)?;
        let location = Location::resolve(&source.path)?;
        let destination = Location::resolve(&requested)?;
        let to = destination.path.clone();
        let from_name = location
            .path
            .file_name()
            .ok_or_else(|| Failure::new(Kind::InvalidPath, "rename source has no basename"))?;
        let to_name = to
            .file_name()
            .ok_or_else(|| Failure::new(Kind::InvalidPath, "move destination has no basename"))?;
        if to == source.path {
            return Err(Failure::new(
                Kind::Exists,
                "rename destination is the source",
            ));
        }
        let node = OpenOptions::new()
            .read(true)
            .custom_flags(O_PATH | O_NOFOLLOW)
            .open(&location.path)?;
        source.check(&location, &node)?;
        let cross_parent = location.parent_identity != destination.parent_identity;
        if cross_parent && source.stamp.mode & 0o170000 != 0o100000 {
            return Err(Failure::new(
                Kind::NotRegular,
                "moving across directories requires a regular file",
            ));
        }
        if reserved.iter().any(|path| path.starts_with(&to)) {
            return Err(Failure::new(
                Kind::Exists,
                "move destination belongs to an open directory",
            ));
        }
        for path in reserved {
            if let Ok(suffix) = path.strip_prefix(&source.path) {
                if to.join(suffix).as_os_str().as_bytes().len() > 4096 {
                    return Err(Failure::new(
                        Kind::Limit,
                        "renamed directory-tab path exceeds 4096 bytes",
                    ));
                }
            }
        }
        let directory = source.stamp.mode & 0o170000 == 0o040000;
        let mut paths = BTreeMap::new();
        for (&id, entry) in &self.entries {
            let path = &entry.location.path;
            if path == &to || (directory && path.starts_with(&to)) {
                return Err(Failure::new(
                    Kind::Exists,
                    "rename destination belongs to an open association",
                ));
            }
            let moved = if path == &source.path {
                if entry.rename_uncertain || entry.stamp.as_ref() != Some(&source.stamp) {
                    return Err(Failure::new(
                        Kind::Conflict,
                        "open file changed since its last load/save",
                    ));
                }
                let baseline = entry._baseline_file.as_ref().ok_or_else(|| {
                    Failure::new(Kind::Conflict, "open file has no retained inode")
                })?;
                compare_stable(baseline, &source.stamp, &entry.bytes)?;
                Some(to.clone())
            } else if directory {
                path.strip_prefix(&source.path)
                    .ok()
                    .map(|suffix| to.join(suffix))
            } else {
                None
            };
            if let Some(moved) = moved {
                if moved.as_os_str().as_bytes().len() > 4096 {
                    return Err(Failure::new(
                        Kind::Limit,
                        "renamed open-file path exceeds 4096 bytes",
                    ));
                }
                entry.location.check_parent()?;
                let parent = if path == &source.path {
                    &destination
                } else {
                    &entry.location
                };
                paths.insert(
                    id,
                    Location {
                        path: moved,
                        parent: parent.parent.try_clone()?,
                        parent_identity: parent.parent_identity,
                    },
                );
            }
        }
        step(Stage::Recheck)?;
        source.check(&location, &node)?;
        destination.check_parent()?;
        step(Stage::Publish)?;
        crate::sys::rename_entry(&location.parent, from_name, &destination.parent, to_name)
            .map_err(|error| {
                let mut failure = if error.kind() == io::ErrorKind::AlreadyExists {
                    Failure::new(
                        Kind::Exists,
                        "rename never overwrites an existing destination",
                    )
                } else {
                    Failure::from(error)
                };
                failure.publication_attempted = true;
                failure
            })?;

        // Once published, every outcome carries the new path. A failed
        // readback retains the old baseline stamp so Save detects conflict.
        let confirmed = (|| -> Result<Stamp> {
            step(Stage::SyncParent)?;
            location.parent.sync_all()?;
            if cross_parent {
                destination.parent.sync_all()?;
            }
            step(Stage::Readback)?;
            location.check_parent()?;
            destination.check_parent()?;
            let stamp = Stamp::read(&node.metadata()?);
            let mut expected = source.stamp.clone();
            expected.ctime = stamp.ctime;
            if stamp != expected || Stamp::read(&fs::symlink_metadata(&to)?) != stamp {
                return Err(Failure::new(
                    Kind::Conflict,
                    "renamed entry changed during publication",
                ));
            }
            if let Some(entry) = self
                .entries
                .values()
                .find(|entry| entry.location.path == source.path)
            {
                let file = entry._baseline_file.as_ref().ok_or_else(|| {
                    Failure::new(Kind::Conflict, "open file has no retained inode")
                })?;
                compare_stable(file, &stamp, &entry.bytes)?;
            }
            Ok(stamp)
        })();
        for (id, location) in paths {
            if let Some(entry) = self.entries.get_mut(&id) {
                if entry.location.path == source.path {
                    entry.rename_uncertain = confirmed.is_err();
                    if let Ok(stamp) = &confirmed {
                        entry.stamp = Some(stamp.clone());
                    }
                }
                entry.location = location;
            }
        }
        Ok(Renamed {
            from: source.path,
            to,
            warning: confirmed
                .err()
                .map(|error| format!("Rename published; confirmation failed: {error}")),
        })
    }

    pub fn baseline_bytes(&self) -> usize {
        self.entries.values().map(|e| e.bytes.len()).sum()
    }
    pub fn bytes(&self, id: FileId) -> Result<&[u8]> {
        Ok(&self.entry(id)?.bytes)
    }
    pub fn path(&self, id: FileId) -> Result<&Path> {
        Ok(&self.entry(id)?.location.path)
    }
    pub fn missing(&self, id: FileId) -> Result<bool> {
        Ok(self.entry(id)?.stamp.is_none())
    }
    pub fn forget(&mut self, id: FileId) {
        self.entries.remove(&id);
    }
    fn entry(&self, id: FileId) -> Result<&Entry> {
        self.entries
            .get(&id)
            .ok_or_else(|| Failure::new(Kind::MissingAssociation, "unknown file association"))
    }
    fn admit(&self, old: usize, new: usize) -> Result<()> {
        if new > text::MAX_FILE_BYTES
            || self
                .baseline_bytes()
                .saturating_sub(old)
                .saturating_add(new)
                > self.budget
        {
            return Err(Failure::new(Kind::Limit, "saved baseline budget exceeded"));
        }
        Ok(())
    }

    /// Validate before association admission. Reopening an inode selects its
    /// original association without refreshing its baseline behind the model.
    /// A missing association must become an empty DIRTY tab in the UI adapter.
    pub fn open(&mut self, path: &Path) -> Result<FileId> {
        let location = Location::resolve(path)?;
        let opened = open_regular(&location.path)?;
        let stamp = opened.as_ref().map(|(_, stamp)| stamp);
        if let Some((&id, _)) = self
            .entries
            .iter()
            .find(|(_, e)| e.matches(&location, stamp))
        {
            return Ok(id);
        }
        if self.entries.len() >= MAX_FILES {
            return Err(Failure::new(Kind::Limit, "file association limit exceeded"));
        }
        let next = self
            .next
            .checked_add(1)
            .ok_or_else(|| Failure::new(Kind::Limit, "file IDs exhausted"))?;
        let entry = self.read_entry(location, opened, 0)?;
        let id = self.next;
        self.entries.insert(id, entry);
        self.next = next;
        Ok(id)
    }

    /// Reread the stored resolved pathname, without replacing its baseline.
    /// Failed preparation and dropped candidates preserve all associations.
    /// A successful preparation consumes one ID even when later cancelled.
    pub fn prepare_reload(&mut self, id: FileId) -> Result<Reload<'_>> {
        let original = self.entry(id)?;
        let next = self
            .next
            .checked_add(1)
            .ok_or_else(|| Failure::new(Kind::Limit, "file IDs exhausted"))?;
        let location = Location::resolve(&original.location.path)?;
        if location.path != original.location.path {
            return Err(Failure::new(
                Kind::Conflict,
                "reload parent redirects to another path; Open that path explicitly",
            ));
        }
        let opened = open_regular(&location.path)?;
        let stamp = opened.as_ref().map(|(_, stamp)| stamp);
        if self
            .entries
            .iter()
            .any(|(&other, entry)| other != id && entry.matches(&location, stamp))
        {
            return Err(Failure::new(
                Kind::Exists,
                "reload destination belongs to another open association",
            ));
        }
        let entry = self.read_entry(location, opened, original.bytes.len())?;
        let replacement = self.next;
        self.next = next;
        Ok(Reload {
            session: self,
            original: id,
            replacement,
            entry,
        })
    }

    fn read_entry(
        &self,
        location: Location,
        opened: Option<(File, Stamp)>,
        old_bytes: usize,
    ) -> Result<Entry> {
        let (stamp, bytes, baseline_file) = match opened {
            Some((file, stamp)) => {
                self.admit(
                    old_bytes,
                    usize::try_from(stamp.len)
                        .map_err(|_| Failure::new(Kind::Limit, "file too large"))?,
                )?;
                let bytes = read_stable(&file, &stamp)?;
                validate(&bytes)?;
                check_name(&location, &stamp)?;
                (Some(stamp), bytes, Some(file))
            }
            None => (None, Vec::new(), None),
        };
        location.check_parent()?;
        Ok(Entry {
            location,
            stamp,
            _baseline_file: baseline_file,
            bytes,
            rename_uncertain: false,
        })
    }

    /// Bytes must be an immutable encoded model snapshot. On success the
    /// caller may acknowledge that snapshot, never the then-current text.
    pub fn save(&mut self, id: FileId, bytes: Vec<u8>) -> Result<()> {
        self.save_impl(id, None, bytes, |_, _| Ok(()))
    }

    /// Existing targets are always refused, including this association's path.
    pub fn save_as(&mut self, id: FileId, path: &Path, bytes: Vec<u8>) -> Result<()> {
        self.entry(id)?;
        let target = Location::resolve(path)?;
        self.save_impl(id, Some(target), bytes, |_, _| Ok(()))
    }

    fn save_impl(
        &mut self,
        id: FileId,
        target: Option<Location>,
        bytes: Vec<u8>,
        mut step: impl FnMut(Stage, &File) -> io::Result<()>,
    ) -> Result<()> {
        let entry = self.entry(id)?;
        self.admit(entry.bytes.len(), bytes.len())?;
        validate(&bytes)?;
        if target.is_none() && entry.rename_uncertain {
            return Err(Failure::new(
                Kind::Conflict,
                "rename confirmation failed; Reload or Save As before saving",
            ));
        }
        let location = target.as_ref().unwrap_or(&entry.location);
        let expected = if target.is_some() {
            None
        } else {
            entry.stamp.as_ref()
        };
        location.check_parent()?;
        // An absent path reserved by another tab must not become this tab's.
        if self
            .entries
            .iter()
            .any(|(&other, e)| other != id && e.location.path == location.path)
        {
            return Err(Failure::new(
                Kind::Exists,
                "path belongs to another open association",
            ));
        }
        verify_destination(location, expected, &entry.bytes)?;
        let mut temporary = Temporary::create(location)?;
        let mut published = false;
        let mut publication_attempted = false;
        let result = (|| -> Result<Stamp> {
            step(Stage::Write, &temporary.file)?;
            temporary.file.write_all(&bytes)?;
            step(Stage::Metadata, &temporary.file).map_err(metadata_error)?;
            let mode = if let Some(old) = expected {
                let meta = temporary.file.metadata()?;
                if meta.uid() != old.uid || meta.gid() != old.gid {
                    fchown(&temporary.file, Some(old.uid), Some(old.gid))
                        .map_err(metadata_error)?;
                }
                old.mode & 0o7777
            } else {
                0o600
            };
            temporary
                .file
                .set_permissions(Permissions::from_mode(mode))
                .map_err(metadata_error)?;
            let meta = temporary.file.metadata()?;
            if meta.mode() & 0o7777 != mode
                || expected.is_some_and(|s| s.uid != meta.uid() || s.gid != meta.gid())
            {
                return Err(Failure::new(
                    Kind::Metadata,
                    "temporary ownership or permissions did not match",
                ));
            }
            require_no_attributes(&temporary.file)?;
            let prepared = Stamp::read(&meta);
            step(Stage::SyncFile, &temporary.file)?;
            temporary.file.sync_all()?;
            step(Stage::Recheck, &temporary.file)?;
            verify_destination(location, expected, &entry.bytes)?;
            check_temporary(&temporary)?;
            step(Stage::Publish, &temporary.file)?;
            publication_attempted = true;
            if expected.is_some() {
                fs::rename(&temporary.path, &location.path)?;
                temporary.named = false;
            } else {
                fs::hard_link(&temporary.path, &location.path).map_err(|e| {
                    if e.kind() == io::ErrorKind::AlreadyExists {
                        Failure::new(Kind::Exists, "destination was created concurrently")
                    } else {
                        e.into()
                    }
                })?;
            }
            published = true;
            step(Stage::Unlink, &temporary.file)?;
            temporary.cleanup()?;
            step(Stage::SyncParent, &location.parent)?;
            location.parent.sync_all()?;
            step(Stage::Readback, &temporary.file)?;
            let stamp = Stamp::read(&temporary.file.metadata()?);
            stamp.replaceable()?;
            if stamp.len != bytes.len() as u64
                || stamp.uid != prepared.uid
                || stamp.gid != prepared.gid
                || stamp.mode != prepared.mode
            {
                return Err(Failure::new(Kind::Conflict, "published metadata changed"));
            }
            require_no_attributes(&temporary.file)?;
            compare_stable(&temporary.file, &stamp, &bytes)?;
            check_name(location, &stamp)?;
            Ok(stamp)
        })();
        let stamp = match result {
            Ok(stamp) => stamp,
            Err(mut error) => {
                error.published = published;
                error.publication_attempted = publication_attempted;
                if let Err(cleanup) = temporary.cleanup() {
                    error.residual = Some(temporary.path.clone());
                    error
                        .detail
                        .push_str(&format!("; temporary cleanup failed: {cleanup}"));
                }
                return Err(error);
            }
        };
        // No fallible allocation or admission remains after publication.
        let entry = self
            .entries
            .get_mut(&id)
            .ok_or_else(|| Failure::new(Kind::MissingAssociation, "association disappeared"))?;
        if let Some(location) = target {
            entry.location = location;
        }
        entry.stamp = Some(stamp);
        entry._baseline_file = Some(temporary.file);
        entry.bytes = bytes;
        entry.rename_uncertain = false;
        Ok(())
    }
}

fn validate(bytes: &[u8]) -> Result<()> {
    text::decode(bytes)
        .map(|_| ())
        .map_err(|e| text_failure(e, e.to_string()))
}

fn text_failure(error: crate::Error, detail: impl Into<String>) -> Failure {
    Failure::new(
        if error == crate::Error::Limit {
            Kind::Limit
        } else {
            Kind::InvalidText
        },
        detail,
    )
}

fn metadata_error(error: io::Error) -> Failure {
    Failure::new(
        Kind::Metadata,
        format!("cannot preserve owner/group/mode; use Save As: {error}"),
    )
}

/// Synchronous, read-only dictionary input. Call outside the display loop.
/// No file association, saved baseline or writable path is created.
pub fn read_dictionary(path: &Path) -> Result<crate::spelling::Dictionary> {
    read_dictionary_with(path, || Ok(()))
}

fn read_dictionary_with(
    path: &Path,
    before_check: impl FnOnce() -> io::Result<()>,
) -> Result<crate::spelling::Dictionary> {
    let location = Location::resolve(path)?;
    let (file, stamp) = open_regular(&location.path)?
        .ok_or_else(|| Failure::new(Kind::Io, "dictionary file does not exist"))?;
    let bytes = read_stable(&file, &stamp)?;
    let dictionary = crate::spelling::Dictionary::parse(&bytes)
        .map_err(|error| text_failure(error, format!("dictionary refused: {error}")))?;
    before_check()?;
    check_name(&location, &stamp)?;
    Ok(dictionary)
}

fn open_regular(path: &Path) -> Result<Option<(File, Stamp)>> {
    open_regular_with(path, || Ok(()))
}

fn open_regular_with(
    path: &Path,
    before_open: impl FnOnce() -> io::Result<()>,
) -> Result<Option<(File, Stamp)>> {
    let before = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    if !before.is_file() {
        return Err(Failure::new(
            Kind::NotRegular,
            "symlinks and nonregular files are refused",
        ));
    }
    before_open()?;
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(O_NOFOLLOW | O_NONBLOCK)
        .open(path)?;
    let after = file.metadata()?;
    if !after.is_file() || Stamp::read(&before) != Stamp::read(&after) {
        return Err(Failure::new(Kind::Conflict, "file changed while opening"));
    }
    Ok(Some((file, Stamp::read(&after))))
}

fn read_stable(file: &File, stamp: &Stamp) -> Result<Vec<u8>> {
    if stamp.len > text::MAX_FILE_BYTES as u64 {
        return Err(Failure::new(Kind::Limit, "file exceeds 16 MiB"));
    }
    let mut bytes = Vec::with_capacity(stamp.len as usize);
    visit_bytes(file, stamp, |_, part| {
        bytes.extend_from_slice(part);
        Ok(())
    })?;
    Ok(bytes)
}

fn visit_bytes(
    file: &File,
    stamp: &Stamp,
    mut visit: impl FnMut(usize, &[u8]) -> Result<()>,
) -> Result<()> {
    if stamp.len > text::MAX_FILE_BYTES as u64 {
        return Err(Failure::new(Kind::Limit, "file exceeds 16 MiB"));
    }
    let mut scratch = [0; 8192];
    let mut offset = 0usize;
    while offset < stamp.len as usize {
        let count = (stamp.len as usize - offset).min(scratch.len());
        let part = scratch
            .get_mut(..count)
            .ok_or_else(|| Failure::new(Kind::Limit, "read size overflow"))?;
        file.read_exact_at(part, offset as u64)?;
        visit(offset, part)?;
        offset += count;
    }
    if file.read_at(&mut [0], stamp.len)? != 0 || Stamp::read(&file.metadata()?) != *stamp {
        return Err(Failure::new(Kind::Conflict, "file changed while reading"));
    }
    Ok(())
}

fn compare_stable(file: &File, stamp: &Stamp, bytes: &[u8]) -> Result<()> {
    if stamp.len != bytes.len() as u64 {
        return Err(Failure::new(Kind::Conflict, "destination length changed"));
    }
    visit_bytes(file, stamp, |offset, part| {
        if bytes.get(offset..offset + part.len()) != Some(part) {
            return Err(Failure::new(Kind::Conflict, "destination bytes changed"));
        }
        Ok(())
    })
}

fn check_name(location: &Location, stamp: &Stamp) -> Result<()> {
    location.check_parent()?;
    let meta = fs::symlink_metadata(&location.path)?;
    if !meta.is_file() || Stamp::read(&meta) != *stamp {
        return Err(Failure::new(Kind::Conflict, "destination changed"));
    }
    Ok(())
}

fn require_no_attributes(file: &File) -> Result<()> {
    match crate::sys::has_attributes(file) {
        Ok(false) => Ok(()),
        Ok(true) => Err(Failure::new(
            Kind::Metadata,
            "extended attributes are unsupported for this write",
        )),
        Err(e) => Err(Failure::new(
            Kind::Metadata,
            format!("cannot inspect extended attributes: {e}"),
        )),
    }
}

fn verify_destination(
    location: &Location,
    expected: Option<&Stamp>,
    baseline: &[u8],
) -> Result<()> {
    location.check_parent()?;
    if let Some(expected) = expected {
        expected.replaceable()?;
        let (file, stamp) = open_regular(&location.path)?
            .ok_or_else(|| Failure::new(Kind::Conflict, "destination disappeared"))?;
        if stamp != *expected {
            return Err(Failure::new(Kind::Conflict, "destination metadata changed"));
        }
        require_no_attributes(&file)?;
        compare_stable(&file, &stamp, baseline)?;
        check_name(location, &stamp)?;
    } else {
        match fs::symlink_metadata(&location.path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => (),
            Err(e) => return Err(e.into()),
            Ok(_) => {
                return Err(Failure::new(
                    Kind::Exists,
                    "creating a new file never overwrites an existing path",
                ));
            }
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Stage {
    Write,
    Metadata,
    SyncFile,
    Recheck,
    Publish,
    Unlink,
    SyncParent,
    Readback,
}

struct Temporary {
    path: PathBuf,
    file: File,
    named: bool,
}
impl Temporary {
    fn create(location: &Location) -> Result<Self> {
        Self::create_with_mode(location, 0o600)
    }

    fn create_with_mode(location: &Location, mode: u32) -> Result<Self> {
        let parent = location
            .path
            .parent()
            .ok_or_else(|| Failure::new(Kind::InvalidPath, "missing parent"))?;
        for _ in 0..64 {
            let serial = TEMP_SERIAL
                .try_update(Ordering::Relaxed, Ordering::Relaxed, |s| s.checked_add(1))
                .map_err(|_| Failure::new(Kind::Limit, "temporary IDs exhausted"))?;
            let path = parent.join(format!(".td-editor-{}-{serial}.tmp", std::process::id()));
            if path == location.path {
                continue;
            }
            match OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .mode(mode)
                .open(&path)
            {
                Ok(file) => {
                    return Ok(Self {
                        path,
                        file,
                        named: true,
                    });
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e.into()),
            }
        }
        Err(Failure::new(Kind::Exists, "temporary name collision limit"))
    }
    fn cleanup(&mut self) -> io::Result<()> {
        if self.named {
            let named = match fs::symlink_metadata(&self.path) {
                Ok(meta) => meta,
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    self.named = false;
                    return Ok(());
                }
                Err(e) => return Err(e),
            };
            let owned = self.file.metadata()?;
            if !named.is_file() || (named.dev(), named.ino()) != (owned.dev(), owned.ino()) {
                return Err(io::Error::other(
                    "temporary name no longer identifies the owned file",
                ));
            }
            match fs::remove_file(&self.path) {
                Ok(()) => (),
                Err(e) if e.kind() == io::ErrorKind::NotFound => (),
                Err(e) => return Err(e),
            }
            self.named = false;
        }
        Ok(())
    }
}

// An empty, never-written probe lets the kernel apply umask. Payload bytes
// are staged in a different private inode, never in a broadly readable probe.
fn copy_mode(location: &Location, requested: u32) -> Result<u32> {
    copy_mode_with(location, requested, require_no_attributes)
}

fn copy_mode_with(
    location: &Location,
    requested: u32,
    mut inspect: impl FnMut(&File) -> Result<()>,
) -> Result<u32> {
    location.check_parent()?;
    // A minimal default ACL can override umask without leaving an access ACL
    // on the child. Inspect the parent, not only the resulting probe inode.
    inspect(&location.parent)?;
    let mut probe = Temporary::create_with_mode(location, requested)?;
    let result = (|| -> Result<u32> {
        check_temporary(&probe)?;
        inspect(&probe.file)?;
        Ok(probe.file.metadata()?.mode() & 0o777)
    })();
    if let Err(cleanup) = probe.cleanup() {
        let mut failure = Failure::new(
            Kind::Io,
            format!("empty permission probe cleanup failed: {cleanup}"),
        );
        failure.residual = Some(probe.path.clone());
        return Err(failure);
    }
    let mode = result?;
    location.check_parent()?;
    inspect(&location.parent)?;
    Ok(mode)
}

fn check_temporary(temporary: &Temporary) -> Result<()> {
    let meta = temporary.file.metadata()?;
    let named = fs::symlink_metadata(&temporary.path)?;
    if !meta.is_file()
        || !named.is_file()
        || Stamp::read(&meta) != Stamp::read(&named)
        || meta.nlink() != 1
    {
        return Err(Failure::new(Kind::Conflict, "temporary file changed"));
    }
    Ok(())
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
mod tests {
    use super::*;
    use crate::model::{Command, Editor};
    use std::os::unix::ffi::OsStringExt;
    use std::os::unix::fs::symlink;

    #[test]
    fn file_copy_permission_probe_refuses_parent_attributes_and_query_errors() {
        for failure_at in 1..=3 {
            let dir = Directory::new();
            let destination = Location::resolve(&dir.path("copy")).unwrap();
            let mut inspections = 0;
            let error = copy_mode_with(&destination, 0o644, |file| {
                inspections += 1;
                assert_eq!(file.metadata().unwrap().is_dir(), inspections != 2);
                if inspections == failure_at {
                    return Err(Failure::new(Kind::Metadata, "attribute query refused"));
                }
                require_no_attributes(file)
            })
            .unwrap_err();
            assert_eq!(error.kind, Kind::Metadata);
            assert!(!error.publication_attempted);
            assert!(!dir.path("copy").exists());
            dir.no_temporaries();
        }
    }

    #[test]
    fn file_copy_preserves_read_only_permissions() {
        let dir = Directory::new();
        let source = dir.write("source", b"read only");
        fs::set_permissions(&source, Permissions::from_mode(0o444)).unwrap();
        let result = Session::default()
            .copy(RenameSource::inspect(&source).unwrap(), "copy".as_ref())
            .unwrap();
        assert!(result.warning.is_none());
        assert_eq!(fs::read(&result.path).unwrap(), b"read only");
        assert_eq!(fs::metadata(&result.path).unwrap().mode() & 0o333, 0);
        assert_eq!(fs::metadata(&source).unwrap().mode() & 0o7777, 0o444);
        dir.no_temporaries();
    }

    #[test]
    fn file_copy_preserves_disk_bytes_ordinary_mode_and_kernel_umask() {
        let dir = Directory::new();
        let old = dir.write("source", b"#!/bin/example\nraw\0\xff\r\n");
        fs::set_permissions(&old, Permissions::from_mode(0o6751)).unwrap();
        let expected = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o751)
            .open(dir.path("expected-mode"))
            .unwrap()
            .metadata()
            .unwrap()
            .mode()
            & 0o777;
        let before = Stamp::read(&fs::metadata(&old).unwrap());
        let mut files = Session::default();
        let raw = std::ffi::OsString::from_vec(b"copy-\xff".to_vec());
        let copied = files
            .copy_with(RenameSource::inspect(&old).unwrap(), &raw, |stage| {
                if stage == Stage::Write {
                    let temporaries: Vec<_> = fs::read_dir(&dir.0)?
                        .map(|entry| entry.unwrap())
                        .filter(|entry| entry.file_name().as_bytes().starts_with(b".td-editor-"))
                        .collect();
                    assert_eq!(
                        temporaries.len(),
                        1,
                        "empty mode probe must already be removed"
                    );
                    let meta = temporaries[0].metadata()?;
                    assert_eq!(meta.len(), 0);
                    assert_eq!(
                        meta.mode() & 0o077,
                        0,
                        "payload must be private while writing"
                    );
                }
                Ok(())
            })
            .unwrap();
        assert!(copied.warning.is_none());
        assert_eq!(copied.path, dir.0.join(&raw));
        assert_eq!(fs::read(&copied.path).unwrap(), fs::read(&old).unwrap());
        let meta = fs::metadata(&copied.path).unwrap();
        assert_eq!(meta.mode() & 0o7777, expected);
        assert_eq!(meta.nlink(), 1);
        assert_ne!(meta.ino(), before.ino);
        assert_eq!(Stamp::read(&fs::metadata(&old).unwrap()), before);
        assert_eq!(files.baseline_bytes(), 0);
        assert!(files.entries.is_empty());
        dir.no_temporaries();
        let text = dir.write("text", b"disk");
        let id = files.open(&text).unwrap();
        files
            .copy(RenameSource::inspect(&text).unwrap(), "text-copy".as_ref())
            .unwrap();
        assert_eq!(files.path(id).unwrap(), text);
        assert_eq!(files.bytes(id).unwrap(), b"disk");
        files.save(id, b"new".to_vec()).unwrap();
        assert_eq!(fs::read(dir.path("text-copy")).unwrap(), b"disk");
        assert_eq!(fs::read(text).unwrap(), b"new");
    }

    #[test]
    fn file_copy_accepts_empty_and_exact_byte_limit_without_text_decoding() {
        let dir = Directory::new();
        let mut files = Session::default();
        for size in [0, text::MAX_FILE_BYTES as u64] {
            let old = dir.path("source");
            File::create(&old).unwrap().set_len(size).unwrap();
            let name = format!("copy-{size}");
            let copied = files
                .copy(RenameSource::inspect(&old).unwrap(), name.as_ref())
                .unwrap();
            assert!(copied.warning.is_none());
            assert_eq!(fs::metadata(copied.path).unwrap().len(), size);
            assert_eq!(files.baseline_bytes(), 0);
            dir.no_temporaries();
        }
    }

    #[test]
    fn file_copy_refuses_replaced_parents_before_any_temporary_creation() {
        let dir = Directory::new();
        let parent = dir.path("parent");
        fs::create_dir(&parent).unwrap();
        let old = parent.join("source");
        fs::write(&old, b"disk").unwrap();
        let source = RenameSource::inspect(&old).unwrap();
        let result = Session::default().copy_with(source, "copy".as_ref(), |stage| {
            if stage == Stage::Metadata {
                fs::rename(&parent, dir.path("moved"))?;
                fs::create_dir(&parent)?;
            }
            Ok(())
        });
        assert_eq!(result.err().unwrap().kind, Kind::Conflict);
        assert_eq!(fs::read(dir.path("moved/source")).unwrap(), b"disk");
        assert_eq!(fs::read_dir(&parent).unwrap().count(), 0);
        dir.no_temporaries();
    }

    #[test]
    fn file_copy_refuses_existing_reserved_nonregular_stale_and_large_sources() {
        let dir = Directory::new();
        let old = dir.write("source", b"disk");
        let mut files = Session::default();
        fs::create_dir(dir.path("tree")).unwrap();
        symlink(&old, dir.path("link")).unwrap();
        symlink(dir.path("absent"), dir.path("broken")).unwrap();
        for name in ["source", "tree", "link", "broken"] {
            assert!(files
                .copy(RenameSource::inspect(&old).unwrap(), name.as_ref())
                .is_err());
        }
        files.open(&dir.path("reserved")).unwrap();
        assert_eq!(
            files
                .copy(RenameSource::inspect(&old).unwrap(), "reserved".as_ref())
                .err()
                .unwrap()
                .kind,
            Kind::Exists
        );
        for name in ["", ".", "..", "a/", "/absolute/", "nul\0name"] {
            let error = files
                .copy(RenameSource::inspect(&old).unwrap(), name.as_ref())
                .err()
                .unwrap();
            assert_eq!(error.kind, Kind::InvalidPath);
            assert!(!error.publication_attempted);
        }
        for name in ["tree", "link", "broken"] {
            let error = files
                .copy(
                    RenameSource::inspect(&dir.path(name)).unwrap(),
                    "out".as_ref(),
                )
                .err()
                .unwrap();
            assert_eq!(error.kind, Kind::NotRegular);
        }
        let stale = RenameSource::inspect(&old).unwrap();
        fs::write(&old, b"different").unwrap();
        assert_eq!(
            files.copy(stale, "out".as_ref()).err().unwrap().kind,
            Kind::Conflict
        );
        let large = File::create(dir.path("large")).unwrap();
        large.set_len(text::MAX_FILE_BYTES as u64 + 1).unwrap();
        assert_eq!(
            files
                .copy(
                    RenameSource::inspect(&dir.path("large")).unwrap(),
                    "out".as_ref()
                )
                .err()
                .unwrap()
                .kind,
            Kind::Limit
        );
        assert!(!dir.path("out").exists());
        assert!(!dir.path("reserved").exists());
        dir.no_temporaries();
    }

    #[test]
    fn file_copy_refuses_source_changes_and_concurrent_destination_creation() {
        let dir = Directory::new();
        let old = dir.write("source", b"disk");
        let mut files = Session::default();
        let error = files
            .copy_with(
                RenameSource::inspect(&old).unwrap(),
                "out".as_ref(),
                |stage| {
                    if stage == Stage::Recheck {
                        fs::write(&old, b"changed")?;
                    }
                    Ok(())
                },
            )
            .err()
            .unwrap();
        assert_eq!(error.kind, Kind::Conflict);
        assert!(!error.publication_attempted);
        assert!(!dir.path("out").exists());
        dir.no_temporaries();
        let error = files
            .copy_with(
                RenameSource::inspect(&old).unwrap(),
                "out".as_ref(),
                |stage| {
                    if stage == Stage::Publish {
                        fs::write(dir.path("out"), b"winner")?;
                    }
                    Ok(())
                },
            )
            .err()
            .unwrap();
        assert_eq!(error.kind, Kind::Exists);
        assert!(error.publication_attempted);
        assert_eq!(fs::read(dir.path("out")).unwrap(), b"winner");
        dir.no_temporaries();
    }

    #[test]
    fn file_copy_failures_keep_the_source_and_never_rollback_publication() {
        for stage in [
            Stage::Metadata,
            Stage::Write,
            Stage::SyncFile,
            Stage::Recheck,
            Stage::Publish,
            Stage::Unlink,
            Stage::SyncParent,
            Stage::Readback,
        ] {
            let dir = Directory::new();
            let old = dir.write("source", b"disk");
            let mut files = Session::default();
            let result =
                files.copy_with(RenameSource::inspect(&old).unwrap(), "out".as_ref(), |at| {
                    if at == stage {
                        return Err(io::Error::other("injected copy failure"));
                    }
                    Ok(())
                });
            if matches!(stage, Stage::Unlink | Stage::SyncParent | Stage::Readback) {
                assert!(result
                    .unwrap()
                    .warning
                    .unwrap()
                    .contains("injected copy failure"));
                assert_eq!(fs::read(dir.path("out")).unwrap(), b"disk");
            } else {
                assert!(result.is_err());
                assert!(!dir.path("out").exists());
            }
            assert_eq!(fs::read(old).unwrap(), b"disk");
            dir.no_temporaries();
        }
        let dir = Directory::new();
        let old = dir.write("source", b"disk");
        let copied = Session::default()
            .copy_with(
                RenameSource::inspect(&old).unwrap(),
                "out".as_ref(),
                |stage| {
                    if stage == Stage::Readback {
                        fs::rename(dir.path("out"), dir.path("moved"))?;
                        fs::write(dir.path("out"), b"winner")?;
                    }
                    Ok(())
                },
            )
            .unwrap();
        assert!(copied.warning.is_some());
        assert_eq!(fs::read(dir.path("out")).unwrap(), b"winner");
        assert_eq!(fs::read(dir.path("moved")).unwrap(), b"disk");
        assert_eq!(fs::read(old).unwrap(), b"disk");
        dir.no_temporaries();
    }

    #[test]
    fn mkdir_is_private_literal_nonrecursive_and_never_overwrites() {
        let dir = Directory::new();
        let source = DirectorySource::inspect(&dir.0).unwrap();
        let mut files = Session::default();
        let raw = std::ffi::OsString::from_vec(b"new-\xff".to_vec());
        let created = files.create_directory(source.clone(), &raw).unwrap();
        assert!(created.warning.is_none());
        assert_eq!(created.path, dir.0.join(&raw));
        let meta = fs::symlink_metadata(&created.path).unwrap();
        assert!(meta.is_dir());
        assert_eq!(meta.mode() & 0o077, 0);
        dir.write("file", b"keep");
        symlink(dir.path("absent"), dir.path("link")).unwrap();
        for name in ["file".as_ref(), "link".as_ref(), raw.as_os_str()] {
            let failure = files.create_directory(source.clone(), name).err().unwrap();
            assert_eq!(failure.kind, Kind::Exists);
            assert!(failure.publication_attempted);
        }
        for name in ["", ".", "..", "a/b", "/absolute", "zero\0byte"] {
            let failure = files
                .create_directory(source.clone(), name.as_ref())
                .err()
                .unwrap();
            assert_eq!(failure.kind, Kind::InvalidPath);
            assert!(!failure.publication_attempted);
        }
        assert!(!dir.path("a").exists());
        assert!(!dir.path("absent").exists());
        assert_eq!(fs::read(dir.path("file")).unwrap(), b"keep");
        assert_eq!(
            fs::symlink_metadata(created.path).unwrap().ino(),
            meta.ino()
        );
        files.open(&dir.path("reserved")).unwrap();
        let failure = files
            .create_directory(source, "reserved".as_ref())
            .err()
            .unwrap();
        assert_eq!(failure.kind, Kind::Exists);
        assert!(!failure.publication_attempted);
        assert!(!dir.path("reserved").exists());
    }

    #[test]
    fn mkdir_refuses_replaced_parent_before_publication() {
        let dir = Directory::new();
        let parent = dir.path("parent");
        fs::create_dir(&parent).unwrap();
        let source = DirectorySource::inspect(&parent).unwrap();
        fs::rename(&parent, dir.path("old")).unwrap();
        fs::create_dir(&parent).unwrap();
        let mut files = Session::default();
        let failure = files
            .create_directory(source, "child".as_ref())
            .err()
            .unwrap();
        assert_eq!(failure.kind, Kind::Conflict);
        assert!(!failure.publication_attempted);
        let source = DirectorySource::inspect(&parent).unwrap();
        let failure = files
            .create_directory_with(source, "child".as_ref(), |stage| {
                if stage == Stage::Publish {
                    fs::rename(&parent, dir.path("second"))?;
                    fs::create_dir(&parent)?;
                }
                Ok(())
            })
            .err()
            .unwrap();
        assert_eq!(failure.kind, Kind::Conflict);
        assert!(!failure.publication_attempted);
        for name in ["parent/child", "old/child", "second/child"] {
            assert!(!dir.path(name).exists());
        }
    }

    #[test]
    fn mkdir_races_and_confirmation_failures_never_cleanup_or_retry() {
        let dir = Directory::new();
        let source = DirectorySource::inspect(&dir.0).unwrap();
        let mut files = Session::default();
        let failure = files
            .create_directory_with(source.clone(), "race".as_ref(), |stage| {
                if stage == Stage::Publish {
                    fs::write(dir.path("race"), b"winner")?;
                }
                Ok(())
            })
            .err()
            .unwrap();
        assert_eq!(failure.kind, Kind::Exists);
        assert_eq!(fs::read(dir.path("race")).unwrap(), b"winner");
        let created = files
            .create_directory_with(source.clone(), "sync".as_ref(), |stage| {
                if stage == Stage::SyncParent {
                    return Err(io::Error::other("injected sync failure"));
                }
                Ok(())
            })
            .unwrap();
        assert!(created.warning.unwrap().contains("injected sync failure"));
        assert!(created.path.is_dir());
        let created = files
            .create_directory_with(source, "replaced".as_ref(), |stage| {
                if stage == Stage::Readback {
                    fs::rename(dir.path("replaced"), dir.path("moved"))?;
                    fs::write(dir.path("replaced"), b"winner")?;
                }
                Ok(())
            })
            .unwrap();
        assert!(created
            .warning
            .unwrap()
            .contains("changed during confirmation"));
        assert!(dir.path("moved").is_dir());
        assert_eq!(fs::read(created.path).unwrap(), b"winner");
    }

    struct Directory(PathBuf);
    impl Directory {
        fn new() -> Self {
            Self::new_in(&std::env::temp_dir())
        }
        fn new_in(parent: &Path) -> Self {
            for _ in 0..64 {
                let n = TEMP_SERIAL.fetch_add(1, Ordering::Relaxed);
                let path = parent
                    .join(format!("td-editor-files-test-{}-{n}", std::process::id()));
                match fs::create_dir(&path) {
                    Ok(()) => return Self(path),
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(e) => panic!("fixture directory: {e}"),
                }
            }
            panic!("fixture collisions");
        }
        fn path(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
        fn write(&self, name: &str, bytes: &[u8]) -> PathBuf {
            let path = self.path(name);
            fs::write(&path, bytes).unwrap();
            path
        }
        fn no_temporaries(&self) {
            Self::check_no_temporaries(&self.0);
        }
        fn check_no_temporaries(path: &Path) {
            for entry in fs::read_dir(path).unwrap() {
                assert!(!entry
                    .unwrap()
                    .file_name()
                    .as_bytes()
                    .starts_with(b".td-editor-"));
            }
        }
    }
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn deletion_is_nofollow_nonrecursive_and_reports_partial_results() {
        let dir = Directory::new();
        let keep = dir.write("keep", b"keep");
        let link = dir.path("link");
        symlink(&keep, &link).unwrap();
        let empty = dir.path("empty");
        fs::create_dir(&empty).unwrap();
        let nonempty = dir.path("full");
        fs::create_dir(&nonempty).unwrap();
        fs::write(nonempty.join("child"), b"child").unwrap();
        let later = dir.write("later", b"later");
        let plan = DeletePlan::new(
            [&link, &empty, &nonempty, &later]
                .into_iter()
                .map(|path| RenameSource::inspect(path).unwrap())
                .collect(),
        )
        .unwrap();
        let result = Session::default().delete(plan).unwrap();
        assert_eq!((result.removed, result.requested), (2, 4));
        assert!(result.failure.unwrap().contains("removal attempted"));
        assert!(fs::symlink_metadata(link).is_err());
        assert!(!empty.exists());
        assert_eq!(fs::read(keep).unwrap(), b"keep");
        assert_eq!(fs::read(nonempty.join("child")).unwrap(), b"child");
        assert_eq!(fs::read(later).unwrap(), b"later");
        dir.no_temporaries();
    }

    #[test]
    fn deletion_refuses_open_aliases_stale_observations_and_invalid_batches() {
        let dir = Directory::new();
        let path = dir.write("file", b"disk");
        let alias = dir.path("alias");
        fs::hard_link(&path, &alias).unwrap();
        let mut files = Session::default();
        files.open(&path).unwrap();
        let plan = DeletePlan::new(vec![RenameSource::inspect(&alias).unwrap()]).unwrap();
        assert_eq!(files.delete(plan).err().unwrap().kind, Kind::Exists);
        assert_eq!(fs::read(&alias).unwrap(), b"disk");
        let source = RenameSource::inspect(&path).unwrap();
        assert!(DeletePlan::new(vec![]).is_err());
        assert!(DeletePlan::new(vec![source.clone(), source.clone()]).is_err());
        assert!(DeletePlan::new(vec![source.clone(); 65]).is_err());
        fs::write(&path, b"changed").unwrap();
        let result = Session::default()
            .delete(DeletePlan::new(vec![source]).unwrap())
            .unwrap();
        assert_eq!(result.removed, 0);
        assert!(result.failure.unwrap().contains("not attempted"));
        assert_eq!(fs::read(path).unwrap(), b"changed");
    }

    #[test]
    fn deletion_rechecks_before_removal_and_never_retries_uncertain_results() {
        for fail in [Stage::Publish, Stage::SyncParent, Stage::Readback] {
            let dir = Directory::new();
            let path = dir.write("file", b"disk");
            let later = dir.write("later", b"untouched");
            let plan = DeletePlan::new(vec![
                RenameSource::inspect(&path).unwrap(),
                RenameSource::inspect(&later).unwrap(),
            ])
            .unwrap();
            let result = Session::default()
                .delete_with(plan, &mut |stage, path| {
                    if stage == fail {
                        fs::write(path, b"external replacement")?;
                        if stage == Stage::SyncParent {
                            return Err(io::Error::other("injected sync failure"));
                        }
                    }
                    Ok(())
                })
                .unwrap();
            assert_eq!(result.removed, usize::from(fail != Stage::Publish));
            assert!(result.failure.is_some());
            assert_eq!(fs::read(path).unwrap(), b"external replacement");
            assert_eq!(fs::read(later).unwrap(), b"untouched");
        }
    }

    #[test]
    fn cross_filesystem_copy_succeeds_but_move_never_copies_or_deletes() {
        // The trusted-root runner supplies a separately mounted /dev/shm.
        let source = Directory::new();
        let destination = Directory::new_in(Path::new("/dev/shm"));
        assert_ne!(fs::metadata(&source.0).unwrap().dev(),
            fs::metadata(&destination.0).unwrap().dev(),
            "run with TD_TEST_TRUSTED_ROOT=1 for distinct fixture filesystems");
        let old = source.write("source", b"disk");
        let mut files = Session::default();
        let id = files.open(&old).unwrap();
        let before = Stamp::read(&fs::metadata(&old).unwrap());
        let copied = files.copy(RenameSource::inspect(&old).unwrap(),
            destination.path("copy").as_os_str()).unwrap();
        assert!(copied.warning.is_none());
        assert_eq!(fs::read(&copied.path).unwrap(), b"disk");
        let error = files.rename(RenameSource::inspect(&old).unwrap(),
            destination.path("move").as_os_str()).err().unwrap();
        assert_eq!(error.kind, Kind::Io);
        assert!(error.publication_attempted && !error.published);
        assert_eq!(Stamp::read(&fs::metadata(&old).unwrap()), before);
        assert!(!destination.path("move").exists());
        assert_eq!(files.path(id).unwrap(), old);
        assert_eq!(files.bytes(id).unwrap(), b"disk");
        files.save(id, b"edited".to_vec()).unwrap();
        assert_eq!(fs::read(old).unwrap(), b"edited");
        assert_eq!(fs::read(copied.path).unwrap(), b"disk");
        source.no_temporaries();
        destination.no_temporaries();
    }

    #[test]
    fn cross_directory_copy_and_move_preserve_bytes_and_rebind_save_parent() {
        let dir = Directory::new();
        fs::create_dir(dir.path("source")).unwrap();
        fs::create_dir(dir.path("destination")).unwrap();
        let old = dir.write("source/file", b"disk\n");
        fs::set_permissions(&old, Permissions::from_mode(0o640)).unwrap();
        let mut files = Session::default();
        let id = files.open(&old).unwrap();
        let before = fs::metadata(&old).unwrap();
        let copied = files
            .copy(
                RenameSource::inspect(&old).unwrap(),
                "../destination/copy".as_ref(),
            )
            .unwrap();
        assert!(copied.warning.is_none());
        assert_eq!(copied.path, dir.path("destination/copy"));
        assert_eq!(fs::read(&copied.path).unwrap(), b"disk\n");
        assert_eq!(files.path(id).unwrap(), old);
        assert_eq!(files.bytes(id).unwrap(), b"disk\n");
        assert_ne!(fs::metadata(&copied.path).unwrap().ino(), before.ino());
        let raw = std::ffi::OsString::from_vec(b"moved-\xff".to_vec());
        let target = dir.path("destination").join(raw);
        let moved = files
            .rename(RenameSource::inspect(&old).unwrap(), target.as_os_str())
            .unwrap();
        assert!(moved.warning.is_none());
        assert_eq!(moved.to, target);
        assert_eq!(fs::metadata(&target).unwrap().ino(), before.ino());
        assert_eq!(fs::metadata(&target).unwrap().mode(), before.mode());
        assert!(!old.exists());
        assert_eq!(files.path(id).unwrap(), target);
        files.save(id, b"edited".to_vec()).unwrap();
        assert_eq!(fs::read(target).unwrap(), b"edited");
        assert_eq!(fs::read(copied.path).unwrap(), b"disk\n");
        assert!(!old.exists());
        dir.no_temporaries();
    }

    #[test]
    fn cross_directory_destinations_resolve_aliases_and_respect_reservations() {
        let dir = Directory::new();
        fs::create_dir(dir.path("destination")).unwrap();
        symlink(dir.path("destination"), dir.path("alias")).unwrap();
        let old = dir.write("source", b"disk");
        let mut files = Session::default();
        files.open(&dir.path("destination/reserved")).unwrap();
        for copying in [true, false] {
            for (name, reserved) in [
                ("alias/reserved", vec![]),
                ("alias/stale", vec![dir.path("destination/stale/child")]),
            ] {
                let source = RenameSource::inspect(&old).unwrap();
                let result = if copying {
                    files
                        .copy_reserved(source, name.as_ref(), &reserved)
                        .map(|r| r.path)
                } else {
                    files
                        .rename_reserved(source, name.as_ref(), &reserved)
                        .map(|r| r.to)
                };
                assert_eq!(result.err().unwrap().kind, Kind::Exists);
                assert_eq!(fs::read(&old).unwrap(), b"disk");
            }
        }
        let copied = files
            .copy(RenameSource::inspect(&old).unwrap(), "alias/copy".as_ref())
            .unwrap();
        assert_eq!(copied.path, dir.path("destination/copy"));
        let moved = files
            .rename(RenameSource::inspect(&old).unwrap(), "alias/moved".as_ref())
            .unwrap();
        assert_eq!(moved.to, dir.path("destination/moved"));
    }

    #[test]
    fn cross_directory_move_refuses_nonregular_and_changed_destination_parents() {
        let dir = Directory::new();
        fs::create_dir(dir.path("destination")).unwrap();
        fs::create_dir(dir.path("tree")).unwrap();
        let old = dir.write("source", b"disk");
        symlink(&old, dir.path("link")).unwrap();
        let mut files = Session::default();
        for name in ["tree", "link"] {
            assert_eq!(
                files
                    .rename(
                        RenameSource::inspect(&dir.path(name)).unwrap(),
                        "destination/out".as_ref()
                    )
                    .err()
                    .unwrap()
                    .kind,
                Kind::NotRegular
            );
            assert!(fs::symlink_metadata(dir.path(name)).is_ok());
        }
        for copying in [true, false] {
            let source = RenameSource::inspect(&old).unwrap();
            let mut hook = |stage| {
                if stage == Stage::Recheck {
                    fs::rename(dir.path("destination"), dir.path("displaced"))?;
                    fs::create_dir(dir.path("destination"))?;
                }
                Ok(())
            };
            let result = if copying {
                files
                    .copy_with(source, "destination/out".as_ref(), &mut hook)
                    .map(|r| r.path)
            } else {
                files
                    .rename_with(source, "destination/out".as_ref(), &mut hook)
                    .map(|r| r.to)
            };
            assert_eq!(result.err().unwrap().kind, Kind::Conflict);
            assert_eq!(fs::read(&old).unwrap(), b"disk");
            assert!(!dir.path("destination/out").exists());
            assert!(!dir.path("displaced/out").exists());
            fs::remove_dir(dir.path("destination")).unwrap();
            // Copy cleanup names may remain after parent replacement; the
            // owned test directory removes them when the fixture is dropped.
            fs::rename(dir.path("displaced"), dir.path("destination")).unwrap();
        }
    }

    #[test]
    fn cross_directory_move_never_overwrites_and_uncertain_success_blocks_save() {
        for fail in [Stage::Publish, Stage::SyncParent, Stage::Readback] {
            let dir = Directory::new();
            fs::create_dir(dir.path("destination")).unwrap();
            let old = dir.write("source", b"disk");
            let target = dir.path("destination/out");
            let mut files = Session::default();
            let id = files.open(&old).unwrap();
            let result = files.rename_with(
                RenameSource::inspect(&old).unwrap(),
                target.as_os_str(),
                |stage| {
                    if stage == fail {
                        if fail == Stage::Publish {
                            fs::write(&target, b"racer")?;
                        } else {
                            return Err(io::Error::other("injected confirmation failure"));
                        }
                    }
                    Ok(())
                },
            );
            if fail == Stage::Publish {
                assert_eq!(result.err().unwrap().kind, Kind::Exists);
                assert_eq!(files.path(id).unwrap(), old);
                assert_eq!(fs::read(target).unwrap(), b"racer");
                assert_eq!(fs::read(old).unwrap(), b"disk");
            } else {
                assert!(result.unwrap().warning.is_some());
                assert_eq!(files.path(id).unwrap(), target);
                assert_eq!(
                    files.save(id, b"edited".to_vec()).err().unwrap().kind,
                    Kind::Conflict
                );
                assert_eq!(fs::read(target).unwrap(), b"disk");
                assert!(!old.exists());
            }
        }
    }

    #[test]
    fn rename_preserves_inode_baseline_and_later_save_destination() {
        let dir = Directory::new();
        let old = dir.write("old", b"\xef\xbb\xbfbody\r\n");
        let mut files = Session::default();
        let id = files.open(&old).unwrap();
        let before = fs::metadata(&old).unwrap();
        let outcome = files
            .rename(RenameSource::inspect(&old).unwrap(), "new".as_ref())
            .unwrap();
        assert!(outcome.warning.is_none());
        assert_eq!(
            outcome.relocated(&old).unwrap().as_os_str().as_bytes(),
            dir.path("new").as_os_str().as_bytes()
        );
        assert_eq!(files.path(id).unwrap(), dir.path("new"));
        assert_eq!(files.bytes(id).unwrap(), b"\xef\xbb\xbfbody\r\n");
        assert_eq!(before.ino(), fs::metadata(dir.path("new")).unwrap().ino());
        assert!(!old.exists());
        files.save(id, b"edited\n".to_vec()).unwrap();
        assert_eq!(fs::read(dir.path("new")).unwrap(), b"edited\n");
        assert!(!old.exists());
        dir.no_temporaries();
    }

    #[test]
    fn rename_refuses_existing_reserved_invalid_and_stale_sources() {
        let dir = Directory::new();
        let old = dir.write("old", b"body");
        dir.write("taken", b"other");
        let mut files = Session::default();
        files.open(&dir.path("reserved")).unwrap();
        for name in [
            "taken",
            "reserved",
            "old",
            "",
            ".",
            "..",
            "a/b",
            "bad\0name",
        ] {
            assert!(files
                .rename(RenameSource::inspect(&old).unwrap(), name.as_ref())
                .is_err());
            assert_eq!(fs::read(&old).unwrap(), b"body");
            assert_eq!(fs::read(dir.path("taken")).unwrap(), b"other");
        }
        let source = RenameSource::inspect(&old).unwrap();
        fs::write(&old, b"changed").unwrap();
        assert_eq!(
            files.rename(source, "new".as_ref()).err().unwrap().kind,
            Kind::Conflict
        );
        assert!(!dir.path("new").exists());
        let source = RenameSource::inspect(&old).unwrap();
        let failure = files
            .rename_with(source, "raced".as_ref(), |stage| {
                if stage == Stage::Publish {
                    fs::write(dir.path("raced"), b"racer")?;
                }
                Ok(())
            })
            .err()
            .unwrap();
        assert_eq!(failure.kind, Kind::Exists);
        assert!(!failure.published && failure.publication_attempted);
        assert_eq!(fs::read(dir.path("raced")).unwrap(), b"racer");
        assert_eq!(fs::read(&old).unwrap(), b"changed");
    }

    #[test]
    fn rename_symlink_and_directory_rebase_without_following_or_refreshing_children() {
        let dir = Directory::new();
        let target = dir.write("target", b"target");
        symlink(&target, dir.path("link")).unwrap();
        let mut files = Session::default();
        let raw = std::ffi::OsString::from_vec(b"link-\xff\n".to_vec());
        let moved = files
            .rename(RenameSource::inspect(&dir.path("link")).unwrap(), &raw)
            .unwrap();
        assert!(moved.warning.is_none());
        assert_eq!(fs::read_link(&moved.to).unwrap(), target);
        assert_eq!(fs::read(&target).unwrap(), b"target");
        fs::create_dir_all(dir.path("tree/sub")).unwrap();
        dir.write("tree/sub/file", b"original");
        let id = files.open(&dir.path("tree/sub/file")).unwrap();
        let missing = files.open(&dir.path("tree/sub/missing")).unwrap();
        let outcome = files
            .rename(
                RenameSource::inspect(&dir.path("tree")).unwrap(),
                "forest".as_ref(),
            )
            .unwrap();
        assert!(outcome.warning.is_none());
        assert_eq!(files.path(id).unwrap(), dir.path("forest/sub/file"));
        assert_eq!(files.path(missing).unwrap(), dir.path("forest/sub/missing"));
        files.save(id, b"edited".to_vec()).unwrap();
        files.save(missing, b"created".to_vec()).unwrap();
        assert_eq!(fs::read(dir.path("forest/sub/file")).unwrap(), b"edited");
        assert_eq!(
            fs::read(dir.path("forest/sub/missing")).unwrap(),
            b"created"
        );
        assert!(!dir.path("tree").exists());
    }

    #[test]
    fn rename_published_failures_keep_new_path_and_force_explicit_recovery() {
        for failure in [Stage::SyncParent, Stage::Readback] {
            let dir = Directory::new();
            let old = dir.write("old", b"body");
            let mut files = Session::default();
            let id = files.open(&old).unwrap();
            let source = RenameSource::inspect(&old).unwrap();
            let outcome = files
                .rename_with(source, "new".as_ref(), |stage| {
                    if stage == failure {
                        return Err(io::Error::other("injected"));
                    }
                    Ok(())
                })
                .unwrap();
            assert!(outcome.warning.unwrap().contains("Rename published"));
            assert_eq!(files.path(id).unwrap(), dir.path("new"));
            assert_eq!(files.bytes(id).unwrap(), b"body");
            assert_eq!(
                files.save(id, b"edit".to_vec()).err().unwrap().kind,
                Kind::Conflict
            );
            assert_eq!(fs::read(dir.path("new")).unwrap(), b"body");
            assert!(!old.exists());
            files
                .save_as(id, &dir.path("recovered"), b"edit".to_vec())
                .unwrap();
            assert_eq!(fs::read(dir.path("recovered")).unwrap(), b"edit");
        }
    }

    #[test]
    fn dictionary_read_is_read_only_and_preserves_filesystem_state() {
        let directory = Directory::new();
        let path = directory.write("words", b"\xef\xbb\xbfWord\r\n\nWORD\ncan't");
        let before = fs::read(&path).unwrap();
        let stamp = Stamp::read(&fs::metadata(&path).unwrap());
        let dictionary = read_dictionary(&path).unwrap();
        assert_eq!(dictionary.entry_count(), 2);
        assert_eq!(fs::read(&path).unwrap(), before);
        assert_eq!(Stamp::read(&fs::metadata(&path).unwrap()), stamp);
        let missing = directory.path("missing");
        assert_eq!(read_dictionary(&missing).err().unwrap().kind, Kind::Io);
        assert!(!missing.exists());
        assert_eq!(
            read_dictionary(&directory.path(".")).err().unwrap().kind,
            Kind::InvalidPath
        );
        directory.no_temporaries();
    }

    #[test]
    fn dictionary_read_refuses_special_oversized_and_malformed_files() {
        assert_eq!(text::MAX_FILE_BYTES, crate::spelling::DICTIONARY_BYTES);
        let directory = Directory::new();
        let path = directory.write("words", b"known");
        let linked = directory.path("linked");
        symlink(&path, &linked).unwrap();
        assert_eq!(
            read_dictionary(&linked).err().unwrap().kind,
            Kind::NotRegular
        );
        let socket = directory.path("socket");
        let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        assert_eq!(
            read_dictionary(&socket).err().unwrap().kind,
            Kind::NotRegular
        );
        let subdir = directory.path("dir");
        fs::create_dir(&subdir).unwrap();
        assert_eq!(
            read_dictionary(&subdir).err().unwrap().kind,
            Kind::NotRegular
        );
        for bytes in [b"".as_slice(), b"word word", b"\xff", b"word\r"] {
            fs::write(&path, bytes).unwrap();
            assert_eq!(
                read_dictionary(&path).err().unwrap().kind,
                Kind::InvalidText
            );
        }
        fs::write(&path, "a".repeat(crate::spelling::WORD_SCALARS + 1)).unwrap();
        assert_eq!(read_dictionary(&path).err().unwrap().kind, Kind::Limit);
        File::create(&path)
            .unwrap()
            .set_len(crate::spelling::DICTIONARY_BYTES as u64 + 1)
            .unwrap();
        assert_eq!(read_dictionary(&path).err().unwrap().kind, Kind::Limit);
        assert_eq!(
            fs::metadata(&path).unwrap().len(),
            crate::spelling::DICTIONARY_BYTES as u64 + 1
        );
        directory.no_temporaries();
    }

    #[test]
    fn dictionary_read_rechecks_the_name_after_parsing_without_overwriting_changes() {
        let directory = Directory::new();
        let path = directory.write("words", b"known");
        let result = read_dictionary_with(&path, || fs::write(&path, b"changed\nlist"));
        assert_eq!(result.err().unwrap().kind, Kind::Conflict);
        assert_eq!(fs::read(&path).unwrap(), b"changed\nlist");
        let replacement = directory.write("replacement", b"other");
        let result = read_dictionary_with(&path, || fs::rename(&replacement, &path));
        assert_eq!(result.err().unwrap().kind, Kind::Conflict);
        assert_eq!(fs::read(&path).unwrap(), b"other");
        let result = read_dictionary_with(&path, || fs::remove_file(&path));
        assert_eq!(result.err().unwrap().kind, Kind::Io);
        assert!(!path.exists());
        directory.no_temporaries();
    }

    #[test]
    fn prepared_borrow_can_be_released_before_session_access() {
        let mut files = Session::default();
        if let Ok(reload) = files.prepare_reload(1) {
            reload.commit();
        }
        files.forget(1);
    }

    #[test]
    fn prepared_reload_can_wait_for_the_next_job_before_commit_or_cancel() {
        use std::sync::mpsc;
        use std::time::Duration;
        for accept in [false, true] {
            let directory = Directory::new();
            let path = directory.write("file", b"old");
            let mut files = Session::default();
            let original = files.open(&path).unwrap();
            fs::write(&path, b"candidate").unwrap();
            let (completion, results) = mpsc::sync_channel(1);
            let (submit, jobs) = mpsc::sync_channel(1);
            let worker = std::thread::spawn(move || {
                let candidate = files.prepare_reload(original).unwrap();
                let replacement = candidate.file_id();
                completion
                    .send((replacement, candidate.bytes().to_vec()))
                    .unwrap();
                let keep = jobs.recv_timeout(Duration::from_secs(5)).unwrap();
                if keep == replacement {
                    candidate.commit();
                } else {
                    drop(candidate);
                }
                completion
                    .send((keep, files.bytes(keep).unwrap().to_vec()))
                    .unwrap();
            });
            let (replacement, bytes) = results.recv_timeout(Duration::from_secs(5)).unwrap();
            assert_eq!(bytes, b"candidate");
            // The UI can finish admission or reject before sending another job.
            let keep = if accept { replacement } else { original };
            submit.send(keep).unwrap();
            let (id, baseline) = results.recv_timeout(Duration::from_secs(5)).unwrap();
            assert_eq!(id, keep);
            assert_eq!(
                baseline,
                if accept {
                    b"candidate".as_slice()
                } else {
                    b"old"
                }
            );
            worker.join().unwrap();
        }
    }

    #[test]
    fn reload_cancel_keeps_baseline_and_commit_replaces_it_under_a_fresh_id() {
        let directory = Directory::new();
        let path = directory.write("file", b"old");
        let mut files = Session::default();
        let original = files.open(&path).unwrap();
        fs::write(&path, b"\xef\xbb\xbfnew\r\n").unwrap();
        let candidate = files.prepare_reload(original).unwrap();
        let abandoned = candidate.file_id();
        let debug = format!("{candidate:?}");
        assert!(!debug.contains("new") && !debug.contains(&path.to_string_lossy().to_string()));
        assert_ne!(abandoned, original);
        assert_eq!(candidate.bytes(), b"\xef\xbb\xbfnew\r\n");
        assert_eq!(candidate.path(), path);
        assert!(!candidate.missing());
        drop(candidate);
        assert_eq!(files.bytes(original).unwrap(), b"old");
        assert_eq!(files.open(&path).unwrap(), original);
        assert!(files.bytes(abandoned).is_err());
        assert_eq!(
            files.save(original, b"edit".to_vec()).unwrap_err().kind,
            Kind::Conflict
        );
        let candidate = files.prepare_reload(original).unwrap();
        let adopted = candidate.file_id();
        assert_ne!(adopted, abandoned);
        assert_eq!(candidate.commit(), adopted);
        assert!(files.bytes(original).is_err());
        assert_eq!(files.bytes(adopted).unwrap(), b"\xef\xbb\xbfnew\r\n");
        assert_eq!(files.open(&path).unwrap(), adopted);
        files.save(adopted, b"saved".to_vec()).unwrap();
        assert_eq!(fs::read(path).unwrap(), b"saved");
        directory.no_temporaries();
    }

    #[test]
    fn reload_admission_counts_replacement_not_both_retained_copies() {
        let directory = Directory::new();
        let path = directory.write("file", b"old");
        let other = directory.write("other", b"xy");
        let mut files = Session {
            budget: 5,
            ..Session::default()
        };
        let original = files.open(&path).unwrap();
        files.open(&other).unwrap();
        for i in 2..MAX_FILES {
            files
                .open(&directory.path(&format!("missing-{i}")))
                .unwrap();
        }
        fs::write(&path, b"new").unwrap();
        let candidate = files.prepare_reload(original).unwrap();
        let adopted = candidate.commit();
        assert_eq!(files.entries.len(), MAX_FILES);
        assert_eq!(files.baseline_bytes(), 5);
        fs::write(&path, b"four").unwrap();
        assert!(matches!(
            files.prepare_reload(adopted),
            Err(Failure {
                kind: Kind::Limit,
                ..
            })
        ));
        assert_eq!(files.bytes(adopted).unwrap(), b"new");
        assert_eq!(files.baseline_bytes(), 5);
        assert_eq!(files.entries.len(), MAX_FILES);
    }

    #[test]
    fn reload_refuses_invalid_text_special_targets_and_exhaustion_atomically() {
        let directory = Directory::new();
        let path = directory.write("file", b"old");
        let mut files = Session::default();
        let original = files.open(&path).unwrap();
        let next = files.next;
        for bytes in [b"\xff".as_slice(), b"a\0", b"a\r\nb\n"] {
            fs::write(&path, bytes).unwrap();
            assert!(matches!(
                files.prepare_reload(original),
                Err(Failure {
                    kind: Kind::InvalidText,
                    ..
                })
            ));
            assert_eq!(files.next, next);
            assert_eq!(files.bytes(original).unwrap(), b"old");
        }
        File::create(&path)
            .unwrap()
            .set_len(text::MAX_FILE_BYTES as u64 + 1)
            .unwrap();
        assert!(matches!(
            files.prepare_reload(original),
            Err(Failure {
                kind: Kind::Limit,
                ..
            })
        ));
        fs::remove_file(&path).unwrap();
        symlink(directory.path("absent"), &path).unwrap();
        assert_eq!(
            files.prepare_reload(original).unwrap_err().kind,
            Kind::NotRegular
        );
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        assert_eq!(
            files.prepare_reload(original).unwrap_err().kind,
            Kind::NotRegular
        );
        fs::remove_dir(&path).unwrap();
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        assert_eq!(
            files.prepare_reload(original).unwrap_err().kind,
            Kind::NotRegular
        );
        drop(listener);
        fs::remove_file(&path).unwrap();
        fs::write(&path, b"valid").unwrap();
        files.next = u64::MAX;
        fs::remove_file(&path).unwrap();
        symlink(directory.path("absent"), &path).unwrap();
        assert!(matches!(
            files.prepare_reload(original),
            Err(Failure {
                kind: Kind::Limit,
                ..
            })
        ));
        assert_eq!(files.bytes(original).unwrap(), b"old");
        assert_eq!(files.entries.len(), 1);
        files.next = next;
        assert!(matches!(
            files.prepare_reload(0),
            Err(Failure {
                kind: Kind::MissingAssociation,
                ..
            })
        ));
        directory.no_temporaries();
    }

    #[test]
    fn reload_missing_paths_and_later_disk_changes_keep_normal_save_checks() {
        let directory = Directory::new();
        let path = directory.write("file", b"old");
        let mut files = Session::default();
        let original = files.open(&path).unwrap();
        fs::remove_file(&path).unwrap();
        let candidate = files.prepare_reload(original).unwrap();
        assert!(candidate.missing() && candidate.bytes().is_empty());
        let missing = candidate.commit();
        assert!(files.missing(missing).unwrap());
        assert!(!path.exists());
        fs::write(&path, b"appeared").unwrap();
        assert_eq!(
            files.save(missing, b"edit".to_vec()).unwrap_err().kind,
            Kind::Exists
        );
        let candidate = files.prepare_reload(missing).unwrap();
        assert!(!candidate.missing());
        assert_eq!(candidate.bytes(), b"appeared");
        fs::write(&path, b"changed after read").unwrap();
        let adopted = candidate.commit();
        assert_eq!(files.bytes(adopted).unwrap(), b"appeared");
        assert_eq!(
            files.save(adopted, b"edit".to_vec()).unwrap_err().kind,
            Kind::Conflict
        );
        assert_eq!(fs::read(path).unwrap(), b"changed after read");
        directory.no_temporaries();
    }

    #[test]
    fn reload_refuses_another_live_inode_and_rebinds_a_replaced_parent() {
        let directory = Directory::new();
        let path = directory.write("file", b"old");
        let other = directory.write("other", b"other");
        let mut files = Session::default();
        let original = files.open(&path).unwrap();
        let other_id = files.open(&other).unwrap();
        fs::remove_file(&path).unwrap();
        fs::hard_link(&other, &path).unwrap();
        assert!(matches!(
            files.prepare_reload(original),
            Err(Failure {
                kind: Kind::Exists,
                ..
            })
        ));
        assert_eq!(files.bytes(original).unwrap(), b"old");
        assert_eq!(files.bytes(other_id).unwrap(), b"other");
        let parent = directory.path("parent");
        fs::create_dir(&parent).unwrap();
        let nested = parent.join("text");
        fs::write(&nested, b"before").unwrap();
        let old_nested = files.open(&nested).unwrap();
        fs::rename(&parent, directory.path("retired-parent")).unwrap();
        fs::write(directory.path("text"), b"redirected").unwrap();
        symlink(&directory.0, &parent).unwrap();
        assert_eq!(
            files.prepare_reload(old_nested).unwrap_err().kind,
            Kind::Conflict
        );
        assert_eq!(files.bytes(old_nested).unwrap(), b"before");
        fs::remove_file(&parent).unwrap();
        fs::create_dir(&parent).unwrap();
        fs::write(&nested, b"replacement").unwrap();
        let candidate = files.prepare_reload(old_nested).unwrap();
        assert_eq!(candidate.bytes(), b"replacement");
        let adopted = candidate.commit();
        files.save(adopted, b"saved".to_vec()).unwrap();
        assert_eq!(fs::read(&nested).unwrap(), b"saved");
        assert_eq!(
            fs::read(directory.path("retired-parent/text")).unwrap(),
            b"before"
        );
        directory.no_temporaries();
        Directory::check_no_temporaries(&parent);
        Directory::check_no_temporaries(&directory.path("retired-parent"));
    }

    #[test]
    fn codec_roundtrips_and_consecutive_saves_preserve_metadata() {
        let directory = Directory::new();
        for (i, bytes) in [
            b"".as_slice(),
            b"abc",
            b"a\n",
            b"\xef\xbb\xbfa\r\nb",
            "é\n猫".as_bytes(),
        ]
        .into_iter()
        .enumerate()
        {
            let path = directory.write(&format!("roundtrip-{i}"), bytes);
            fs::set_permissions(&path, Permissions::from_mode(0o640)).unwrap();
            let original = fs::metadata(&path).unwrap();
            let mut session = Session::default();
            let id = session.open(&path).unwrap();
            let mut editor = Editor::default();
            let tab = editor.load_bytes(session.bytes(id).unwrap()).unwrap();
            for _ in 0..2 {
                let (point, bytes) = editor.save_snapshot(tab).unwrap();
                session.save(id, bytes.clone()).unwrap();
                editor.acknowledge_saved(point).unwrap();
                assert_eq!(fs::read(&path).unwrap(), bytes);
                let saved = fs::metadata(&path).unwrap();
                assert_eq!(
                    (saved.uid(), saved.gid(), saved.mode()),
                    (original.uid(), original.gid(), original.mode())
                );
                assert_eq!(saved.nlink(), 1);
                assert!(!editor.document(tab).unwrap().dirty());
                assert_eq!(session.baseline_bytes(), bytes.len());
            }
        }
        directory.no_temporaries();
    }

    #[test]
    fn save_completion_acknowledges_only_the_captured_state() {
        let directory = Directory::new();
        let path = directory.write("draft", b"old");
        let mut files = Session::default();
        let id = files.open(&path).unwrap();
        let mut editor = Editor::default();
        let tab = editor.load_bytes(files.bytes(id).unwrap()).unwrap();
        editor
            .dispatch(tab, 0, Command::Insert("first ".into()))
            .unwrap();
        let (point, bytes) = editor.save_snapshot(tab).unwrap();
        editor
            .dispatch(tab, 1, Command::Insert("second ".into()))
            .unwrap();
        files.save(id, bytes).unwrap();
        editor.acknowledge_saved(point).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"first old");
        assert!(editor.document(tab).unwrap().dirty());
        editor.dispatch(tab, 2, Command::Undo).unwrap();
        assert!(!editor.document(tab).unwrap().dirty());
    }

    #[test]
    fn missing_and_save_as_are_no_clobber_and_reassociate_only_on_success() {
        let directory = Directory::new();
        let original = directory.write("original", b"old");
        let mut session = Session::default();
        let id = session.open(&original).unwrap();
        let new = directory.path("new");
        session.save_as(id, &new, b"new".to_vec()).unwrap();
        assert_eq!(session.path(id).unwrap(), new);
        assert_eq!(fs::read(original).unwrap(), b"old");
        assert_eq!(fs::metadata(&new).unwrap().mode() & 0o7777, 0o600);
        assert_eq!(
            session.save_as(id, &new, b"bad".to_vec()).unwrap_err().kind,
            Kind::Exists
        );
        assert_eq!(fs::read(&new).unwrap(), b"new");
        let missing = directory.path("missing");
        let other = session.open(&missing).unwrap();
        assert!(session.missing(other).unwrap());
        assert_eq!(session.bytes(other).unwrap(), b"");
        assert_eq!(session.open(&missing).unwrap(), other);
        assert_eq!(
            session
                .save_as(id, &missing, b"reserved".to_vec())
                .unwrap_err()
                .kind,
            Kind::Exists
        );
        session.save(other, Vec::new()).unwrap();
        assert!(!session.missing(other).unwrap());
        assert_eq!(fs::read(&missing).unwrap(), b"");
        session.forget(id);
        assert_eq!(
            session.bytes(id).unwrap_err().kind,
            Kind::MissingAssociation
        );
        assert!(new.exists());
        directory.no_temporaries();
    }

    #[test]
    fn literal_paths_identity_dedup_and_bad_file_types() {
        let directory = Directory::new();
        let mut session = Session::default();
        let path = directory.0.join(std::ffi::OsString::from_vec(
            b"-draft \xff;$(literal)".to_vec(),
        ));
        fs::write(&path, b"text").unwrap();
        let id = session.open(&path).unwrap();
        let alias = directory.path("alias");
        fs::hard_link(&path, &alias).unwrap();
        assert_eq!(session.open(&alias).unwrap(), id);
        // Changed link count is a conflict, never authorization to overwrite.
        assert!(session.save(id, b"replacement".to_vec()).is_err());
        let link = directory.path("link");
        symlink(&path, &link).unwrap();
        for bad in [&link, &directory.0, Path::new("/dev/null")] {
            assert!(session.open(bad).is_err());
        }
        let socket_path = directory.path("socket");
        let _listener = std::os::unix::net::UnixListener::bind(&socket_path).unwrap();
        assert_eq!(
            session.open(&socket_path).unwrap_err().kind,
            Kind::NotRegular
        );
        for bad in [b"".as_slice(), b"a/.", b"a/..", b"a/", b"a\0b"] {
            assert_eq!(
                session
                    .open(Path::new(std::ffi::OsStr::from_bytes(bad)))
                    .unwrap_err()
                    .kind,
                Kind::InvalidPath
            );
        }
        assert_eq!(fs::read(path).unwrap(), b"text");
    }

    #[test]
    fn malformed_and_over_budget_opens_and_saves_are_atomic() {
        let directory = Directory::new();
        let mut session = Session {
            budget: 5,
            ..Session::default()
        };
        let good = directory.write("good", b"abc");
        let id = session.open(&good).unwrap();
        let bad = directory.write("bad", b"a\r\nb\n");
        assert!(session.open(&bad).is_err());
        let large = directory.write("large", b"def");
        assert_eq!(session.open(&large).unwrap_err().kind, Kind::Limit);
        assert_eq!(
            session.save(id, b"123456".to_vec()).unwrap_err().kind,
            Kind::Limit
        );
        assert_eq!(
            session.save(id, b"x\0".to_vec()).unwrap_err().kind,
            Kind::InvalidText
        );
        assert_eq!(session.entries.len(), 1);
        assert_eq!(session.baseline_bytes(), 3);
        assert_eq!(fs::read(&good).unwrap(), b"abc");
        session.save(id, b"12345".to_vec()).unwrap();
        assert_eq!(session.baseline_bytes(), 5);
        let huge = File::create(directory.path("huge")).unwrap();
        huge.set_len(text::MAX_FILE_BYTES as u64 + 1).unwrap();
        assert_eq!(
            session.open(&directory.path("huge")).unwrap_err().kind,
            Kind::Limit
        );
        directory.no_temporaries();
    }

    #[test]
    fn every_injected_failure_retains_baseline_and_reports_publication() {
        for absent in [false, true] {
            for fail in [
                Stage::Write,
                Stage::Metadata,
                Stage::SyncFile,
                Stage::Recheck,
                Stage::Publish,
                Stage::Unlink,
                Stage::SyncParent,
                Stage::Readback,
            ] {
                let directory = Directory::new();
                let path = if absent {
                    directory.path("file")
                } else {
                    directory.write("file", b"original")
                };
                let mut session = Session::default();
                let id = session.open(&path).unwrap();
                let baseline = session.bytes(id).unwrap().to_vec();
                let error = session
                    .save_impl(id, None, b"replacement".to_vec(), |stage, file| {
                        if stage == fail {
                            if stage == Stage::Write {
                                file.write_all_at(b"partial", 0)?;
                            }
                            return Err(io::Error::other("injected I/O failure"));
                        }
                        Ok(())
                    })
                    .unwrap_err();
                let published = matches!(fail, Stage::Unlink | Stage::SyncParent | Stage::Readback);
                if fail == Stage::Metadata {
                    assert_eq!(error.kind, Kind::Metadata);
                }
                assert_eq!(error.published, published, "{absent}/{fail:?}");
                assert_eq!(error.publication_attempted, published);
                assert_eq!(session.bytes(id).unwrap(), baseline);
                assert_eq!(session.missing(id).unwrap(), absent);
                if published {
                    assert_eq!(fs::read(&path).unwrap(), b"replacement");
                } else if absent {
                    assert!(!path.exists());
                } else {
                    assert_eq!(fs::read(&path).unwrap(), b"original");
                }
                assert!(error.residual.is_none());
                directory.no_temporaries();
            }
        }
    }

    #[test]
    fn recheck_catches_external_edits_replacement_removal_and_permissions() {
        for variant in 0..4 {
            let directory = Directory::new();
            let path = directory.write("file", b"original");
            let mut session = Session::default();
            let id = session.open(&path).unwrap();
            let result = session.save_impl(id, None, b"replacement".to_vec(), |stage, _| {
                if stage == Stage::Recheck {
                    match variant {
                        0 => fs::write(&path, b"external")?,
                        1 => {
                            fs::remove_file(&path)?;
                            fs::write(&path, b"original")?;
                        }
                        2 => fs::remove_file(&path)?,
                        _ => fs::set_permissions(&path, Permissions::from_mode(0o400))?,
                    }
                }
                Ok(())
            });
            let error = result.unwrap_err();
            assert_eq!(error.kind, Kind::Conflict);
            assert!(!error.publication_attempted);
            assert_eq!(session.bytes(id).unwrap(), b"original");
            assert_ne!(
                fs::read(&path).ok().as_deref(),
                Some(b"replacement".as_slice())
            );
            directory.no_temporaries();
        }
    }

    #[test]
    fn create_race_at_publication_never_clobbers_and_post_publish_edit_is_reported() {
        let directory = Directory::new();
        let path = directory.path("file");
        let mut session = Session::default();
        let id = session.open(&path).unwrap();
        let error = session
            .save_impl(id, None, b"editor".to_vec(), |stage, _| {
                if stage == Stage::Publish {
                    fs::write(&path, b"other")?;
                }
                Ok(())
            })
            .unwrap_err();
        assert_eq!(error.kind, Kind::Exists);
        assert!(!error.published);
        assert!(error.publication_attempted);
        assert_eq!(fs::read(&path).unwrap(), b"other");
        session.forget(id);
        let id = session.open(&path).unwrap();
        let error = session
            .save_impl(id, None, b"editor".to_vec(), |stage, _| {
                if stage == Stage::Readback {
                    fs::write(&path, b"OTHER!")?;
                }
                Ok(())
            })
            .unwrap_err();
        assert!(error.published);
        assert_eq!(error.kind, Kind::Conflict);
        assert_eq!(session.bytes(id).unwrap(), b"other");
        directory.no_temporaries();
    }

    #[test]
    fn replacement_profile_refuses_hardlinks_and_special_mode_bits() {
        for mode in [0o4640, 0o2640, 0o640] {
            let directory = Directory::new();
            let path = directory.write("file", b"original");
            fs::set_permissions(&path, Permissions::from_mode(mode)).unwrap();
            if mode == 0o640 {
                fs::hard_link(&path, directory.path("link")).unwrap();
            }
            let mut session = Session::default();
            let id = session.open(&path).unwrap();
            assert_eq!(
                session.save(id, b"replacement".to_vec()).unwrap_err().kind,
                Kind::Metadata
            );
            session
                .save_as(id, &directory.path("copy"), b"replacement".to_vec())
                .unwrap();
            assert_eq!(fs::read(path).unwrap(), b"original");
            assert_eq!(
                fs::metadata(directory.path("copy")).unwrap().mode() & 0o7777,
                0o600
            );
        }
    }

    #[test]
    fn complete_byte_comparison_and_parent_identity_are_load_bearing() {
        let directory = Directory::new();
        let path = directory.write("file", b"abc");
        let file = File::open(&path).unwrap();
        let stamp = Stamp::read(&file.metadata().unwrap());
        assert_eq!(
            compare_stable(&file, &stamp, b"abd").unwrap_err().kind,
            Kind::Conflict
        );
        let mut session = Session::default();
        let id = session.open(&path).unwrap();
        let moved = directory.path("nested");
        fs::create_dir(&moved).unwrap();
        let nested = moved.join("draft");
        fs::write(&nested, b"old").unwrap();
        let other = session.open(&nested).unwrap();
        fs::rename(&moved, directory.path("old-parent")).unwrap();
        fs::create_dir(&moved).unwrap();
        fs::write(&nested, b"new parent").unwrap();
        assert_eq!(
            session.save(other, b"bad".to_vec()).unwrap_err().kind,
            Kind::Conflict
        );
        assert_eq!(session.bytes(id).unwrap(), b"abc");
        assert_eq!(fs::read(nested).unwrap(), b"new parent");
    }

    #[test]
    fn cleanup_never_unlinks_a_replacement_temporary_name() {
        let directory = Directory::new();
        let location = Location::resolve(&directory.path("file")).unwrap();
        let mut temporary = Temporary::create(&location).unwrap();
        fs::rename(&temporary.path, directory.path("retained")).unwrap();
        fs::write(&temporary.path, b"other").unwrap();
        assert!(temporary.cleanup().is_err());
        assert_eq!(fs::read(&temporary.path).unwrap(), b"other");
    }

    #[test]
    fn cleanup_failure_reports_residual_and_keeps_old_association() {
        let directory = Directory::new();
        let path = directory.write("file", b"old");
        let mut session = Session::default();
        let id = session.open(&path).unwrap();
        let mut residual = None;
        let error = session
            .save_impl(id, None, b"editor".to_vec(), |stage, _| {
                if stage == Stage::Recheck {
                    let temp = fs::read_dir(&directory.0)?
                        .filter_map(|e| e.ok())
                        .find(|e| e.file_name().as_bytes().starts_with(b".td-editor-"))
                        .unwrap()
                        .path();
                    fs::rename(&temp, directory.path("held"))?;
                    fs::write(&temp, b"other")?;
                    residual = Some(temp);
                }
                Ok(())
            })
            .unwrap_err();
        assert_eq!(error.residual, residual);
        assert!(!error.publication_attempted);
        assert_eq!(fs::read(error.residual.unwrap()).unwrap(), b"other");
        assert_eq!(session.bytes(id).unwrap(), b"old");
        assert_eq!(fs::read(path).unwrap(), b"old");
    }

    #[test]
    fn an_already_removed_temporary_name_is_not_a_cleanup_failure() {
        for after_publication in [false, true] {
            let directory = Directory::new();
            let path = directory.path("file");
            let mut session = Session::default();
            let id = session.open(&path).unwrap();
            let result = session.save_impl(id, None, b"editor".to_vec(), |stage, _| {
                if stage
                    == if after_publication {
                        Stage::Unlink
                    } else {
                        Stage::Write
                    }
                {
                    let temp = fs::read_dir(&directory.0)?
                        .filter_map(|e| e.ok())
                        .find(|e| e.file_name().as_bytes().starts_with(b".td-editor-"))
                        .unwrap()
                        .path();
                    fs::remove_file(temp)?;
                    if !after_publication {
                        return Err(io::Error::other("write failed"));
                    }
                }
                Ok(())
            });
            if after_publication {
                result.unwrap();
                assert_eq!(session.bytes(id).unwrap(), b"editor");
                assert_eq!(fs::read(path).unwrap(), b"editor");
            } else {
                let error = result.unwrap_err();
                assert!(!error.publication_attempted);
                assert!(
                    error.residual.is_none(),
                    "already absent name is not a residual: {error}"
                );
                assert!(!path.exists());
            }
            directory.no_temporaries();
        }
    }

    #[test]
    fn association_ceiling_rejected_text_and_stale_save_as_keep_state() {
        let directory = Directory::new();
        let mut session = Session::default();
        for bad in [
            b"\xff".as_slice(),
            b"a\r\nb\n",
            b"\0",
            b"\xef\xbb\xbf\xef\xbb\xbf",
        ] {
            let path = directory.write("bad", bad);
            assert_eq!(session.open(&path).unwrap_err().kind, Kind::InvalidText);
            assert!(session.entries.is_empty());
        }
        let path = directory.write("old", b"old");
        let id = session.open(&path).unwrap();
        let target = Location::resolve(&directory.path("copy")).unwrap();
        let error = session
            .save_impl(id, Some(target), b"new".to_vec(), |stage, _| {
                if stage == Stage::SyncParent {
                    Err(io::Error::other("sync failed"))
                } else {
                    Ok(())
                }
            })
            .unwrap_err();
        assert!(error.published);
        assert_eq!(session.path(id).unwrap(), path);
        assert_eq!(session.bytes(id).unwrap(), b"old");
        assert_eq!(fs::read(directory.path("copy")).unwrap(), b"new");
        for i in 1..MAX_FILES {
            session
                .open(&directory.path(&format!("missing-{i}")))
                .unwrap();
        }
        assert_eq!(
            session.open(&directory.path("overflow")).unwrap_err().kind,
            Kind::Limit
        );
        assert_eq!(session.open(&path).unwrap(), id);
        assert_eq!(session.entries.len(), MAX_FILES);
    }

    #[test]
    #[ignore = "requires a dedicated FIFO path; see README"]
    fn fifo_fixture_is_refused_without_waiting_for_a_writer() {
        let path = PathBuf::from(std::env::var_os("TD_EDITOR_TEST_FIFO").unwrap());
        assert!(std::os::unix::fs::FileTypeExt::is_fifo(
            &fs::symlink_metadata(&path).unwrap().file_type()
        ));
        assert_eq!(
            Session::default().open(&path).unwrap_err().kind,
            Kind::NotRegular
        );
        // The flag, not just lstat, must protect the replacement race.
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        let worker = std::thread::spawn(move || {
            let directory = Directory::new();
            let replaced = directory.write("was-regular", b"file");
            let result = open_regular_with(&replaced, || {
                fs::remove_file(&replaced)?;
                fs::hard_link(&path, &replaced)
            });
            let _ = sender.send(result.map(|_| ()).map_err(|e| e.kind));
        });
        assert_eq!(
            receiver
                .recv_timeout(std::time::Duration::from_secs(2))
                .unwrap(),
            Err(Kind::Conflict)
        );
        worker.join().unwrap();
    }

    #[test]
    fn worker_session_is_send_and_debug_never_dumps_text_or_paths() {
        fn assert_send<T: Send>() {}
        assert_send::<Session>();
        let directory = Directory::new();
        let path = directory.write("private-filename", b"private document bytes");
        let mut session = Session::default();
        session.open(&path).unwrap();
        let debug = format!("{session:?}");
        assert!(debug.contains("associations: 1"));
        assert!(!debug.contains("private"));
        assert!(debug.len() < 100);
    }

    #[test]
    #[ignore = "requires a dedicated UTF-8 file with an extended attribute; see README"]
    fn attribute_fixture_is_refused_without_touching_it() {
        let path = PathBuf::from(std::env::var_os("TD_EDITOR_TEST_XATTR_FILE").unwrap());
        let mut session = Session::default();
        let id = session.open(&path).unwrap();
        let before = fs::read(&path).unwrap();
        let error = session.save(id, b"must not publish".to_vec()).unwrap_err();
        assert_eq!(error.kind, Kind::Metadata);
        assert!(!error.publication_attempted);
        assert_eq!(fs::read(path).unwrap(), before);
    }
}

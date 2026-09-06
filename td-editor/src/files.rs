//! Synchronous, worker-owned file transactions. No document mutation or UI.

use crate::text;
use std::collections::BTreeMap;
use std::fs::{self, File, Metadata, OpenOptions, Permissions};
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{fchown, FileExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const O_NOFOLLOW: i32 = 0o400000;
const O_NONBLOCK: i32 = 0o4000;
const O_DIRECTORY: i32 = 0o200000;
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
struct Stamp {
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
    fn read(meta: &Metadata) -> Self {
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
}

/// Own on one file worker. IDs are local to this session, not model tab IDs.
/// Dropping/removing an association never deletes its destination.
pub struct Session {
    entries: BTreeMap<FileId, Entry>,
    next: FileId,
    budget: usize,
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
        if let Some((&id, _)) = self.entries.iter().find(|(_, e)| {
            e.location.path == location.path
                || match (&e.stamp, stamp) {
                    (Some(a), Some(b)) => a.identity() == b.identity(),
                    _ => false,
                }
        }) {
            return Ok(id);
        }
        if self.entries.len() >= MAX_FILES {
            return Err(Failure::new(Kind::Limit, "file association limit exceeded"));
        }
        let next = self
            .next
            .checked_add(1)
            .ok_or_else(|| Failure::new(Kind::Limit, "file IDs exhausted"))?;
        let (stamp, bytes, baseline_file) = match opened {
            Some((file, stamp)) => {
                self.admit(
                    0,
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
        let id = self.next;
        self.entries.insert(
            id,
            Entry {
                location,
                stamp,
                _baseline_file: baseline_file,
                bytes,
            },
        );
        self.next = next;
        Ok(id)
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
        Ok(())
    }
}

fn validate(bytes: &[u8]) -> Result<()> {
    text::decode(bytes).map(|_| ()).map_err(|e| {
        Failure::new(
            if e == crate::Error::Limit {
                Kind::Limit
            } else {
                Kind::InvalidText
            },
            e.to_string(),
        )
    })
}

fn metadata_error(error: io::Error) -> Failure {
    Failure::new(
        Kind::Metadata,
        format!("cannot preserve owner/group/mode; use Save As: {error}"),
    )
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
            "extended attributes are not replaceable; use Save As",
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
                    "Save As/new-file save never overwrites an existing path",
                ))
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
                .mode(0o600)
                .open(&path)
            {
                Ok(file) => {
                    return Ok(Self {
                        path,
                        file,
                        named: true,
                    })
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

    struct Directory(PathBuf);
    impl Directory {
        fn new() -> Self {
            for _ in 0..64 {
                let n = TEMP_SERIAL.fetch_add(1, Ordering::Relaxed);
                let path = std::env::temp_dir()
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
            for entry in fs::read_dir(&self.0).unwrap() {
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

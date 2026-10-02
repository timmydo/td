//! Bounded std filesystem access within operator-controlled, stable paths.
use crate::store_paths::Name;
use std::{
    fs::{self, File, Metadata},
    io,
    os::unix::fs::MetadataExt,
    path::Path,
};

/// Qualified by the host and portable allocation probes, including errors.
/// This is a service limit, not a promise about every Rust implementation.
pub const MAX_PATH_BYTES: usize = 383;
pub const MAX_ROOT_BYTES: usize = MAX_PATH_BYTES - crate::store_paths::CAPACITY - 1;

#[derive(Debug)]
pub enum RootError {
    Path,
    Io(io::Error),
    Owner,
    WritableAncestor,
    PrivateMode,
}
impl std::fmt::Display for RootError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Path => f.write_str("invalid or overlong storage path"),
            Self::Io(error) => write!(f, "storage directory operation: {error}"),
            Self::Owner => f.write_str("data root owner must be non-root and ancestors trusted"),
            Self::WritableAncestor => f.write_str("data root ancestor permits shared writes"),
            Self::PrivateMode => f.write_str("data root requires mode 0700 without special bits"),
        }
    }
}
impl std::error::Error for RootError {}
impl From<io::Error> for RootError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// Startup checks under the deployment's stable-path assumption. The supervisor
/// must run as this directory's dedicated owner; std does not verify process UID.
/// Root/owner/mount authority and all concurrent namespace writers are trusted.
#[derive(Debug)]
pub struct PrivateRoot {
    directory: Directory,
}
impl PrivateRoot {
    pub fn open(path: &str) -> Result<Self, RootError> {
        if path.len() > MAX_ROOT_BYTES {
            return Err(RootError::Path);
        }
        validate(path).map_err(|_| RootError::Path)?;
        let metadata = fs::symlink_metadata(path)?;
        if !metadata.is_dir() {
            return Err(RootError::Io(io::ErrorKind::NotADirectory.into()));
        }
        if metadata.mode() & 0o7777 != 0o700 {
            return Err(RootError::PrivateMode);
        }
        let owner = metadata.uid();
        if owner == 0 {
            return Err(RootError::Owner);
        }
        for ancestor in Path::new(path).ancestors().skip(1) {
            let metadata = fs::symlink_metadata(ancestor)?;
            if !metadata.is_dir() {
                return Err(RootError::Io(io::ErrorKind::NotADirectory.into()));
            }
            if !trusted_owner(metadata.uid(), owner) {
                return Err(RootError::Owner);
            }
            if metadata.mode() & 0o022 != 0 {
                return Err(RootError::WritableAncestor);
            }
        }
        let directory = Directory::from_path(path)?;
        if !same_file(&metadata, &directory.metadata()?) {
            return Err(RootError::Io(io::ErrorKind::InvalidData.into()));
        }
        Ok(Self { directory })
    }
    pub fn directory(&self) -> &Directory {
        &self.directory
    }
}

/// A retained File for metadata/sync and a fixed pathname for future lookup.
/// New lookups use that pathname, so renames or mount changes during service
/// operation are unsupported. This is not a directory-confinement capability.
#[derive(Debug)]
pub struct Directory {
    file: File,
    path: [u8; MAX_PATH_BYTES],
    length: usize,
}
impl Directory {
    /// Checks each existing component for directory type (rejecting symlinks).
    /// Checks are not atomic with open and do not defend against hostile races.
    /// No ownership, permission or writer-lock authority is established here.
    pub fn from_path(path: &str) -> io::Result<Self> {
        validate(path)?;
        for ancestor in Path::new(path).ancestors() {
            if !fs::symlink_metadata(ancestor)?.is_dir() {
                return Err(io::ErrorKind::NotADirectory.into());
            }
        }
        let before = fs::symlink_metadata(path)?;
        let file = File::open(path)?;
        let after = file.metadata()?;
        if !after.is_dir() || !same_file(&before, &after) {
            return Err(io::ErrorKind::InvalidData.into());
        }
        let mut bytes = [0; MAX_PATH_BYTES];
        bytes
            .get_mut(..path.len())
            .ok_or(io::ErrorKind::InvalidInput)?
            .copy_from_slice(path.as_bytes());
        Ok(Self {
            file,
            path: bytes,
            length: path.len(),
        })
    }

    /// A generated name is relative to this stored path. Keep every ancestor
    /// stable while serving; this lookup does not follow the retained File.
    pub fn open(&self, name: &Name) -> io::Result<Self> {
        let mut path = [0; MAX_PATH_BYTES];
        let mut output = crate::bounded::TextBuffer::new(&mut path);
        let root = std::str::from_utf8(
            self.path
                .get(..self.length)
                .ok_or(io::ErrorKind::InvalidInput)?,
        )
        .map_err(|_| io::ErrorKind::InvalidInput)?;
        let name = name.as_str().map_err(|_| io::ErrorKind::InvalidInput)?;
        let separator = if root == "/" { "" } else { "/" };
        output
            .format(format_args!("{root}{separator}{name}"))
            .map_err(|_| io::ErrorKind::InvalidInput)?;
        Self::from_path(output.as_str().map_err(|_| io::ErrorKind::InvalidInput)?)
    }
    pub fn metadata(&self) -> io::Result<Metadata> {
        self.file.metadata()
    }
}
fn same_file(a: &Metadata, b: &Metadata) -> bool {
    a.dev() == b.dev() && a.ino() == b.ino()
}
fn validate(path: &str) -> io::Result<()> {
    if path.len() > MAX_PATH_BYTES {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    crate::config::values::absolute_path(path).map_err(|_| io::ErrorKind::InvalidInput.into())
}

fn trusted_owner(owner: u32, service: u32) -> bool {
    owner == 0 || owner == service
}

#[cfg(test)]
mod tests {
    #[test]
    fn ancestor_owner_policy_excludes_other_identities() {
        assert!(super::trusted_owner(0, 1000));
        assert!(super::trusted_owner(1000, 1000));
        assert!(!super::trusted_owner(1001, 1000));
        assert!(!super::trusted_owner(u32::MAX, 1000));
    }
}

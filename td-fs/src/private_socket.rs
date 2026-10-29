//! Private pathname Unix endpoints in a trusted, stable filesystem namespace.
//!
//! Root and same-UID namespace writers are trusted. Checks reject symlinks and
//! other-user replacement authority; they do not make pathname lookup atomic
//! against a process already holding the same filesystem authority.
use std::{
    fs, io,
    os::unix::{
        ffi::OsStrExt,
        fs::{FileTypeExt, MetadataExt},
    },
    path::Path,
};

/// A canonical Linux pathname socket address, owned without a heap allocation.
#[derive(Clone, Copy, Eq, PartialEq)]
pub struct SocketPath {
    bytes: [u8; 107],
    length: u8,
}
impl SocketPath {
    pub fn new(path: &Path) -> io::Result<Self> {
        let bytes = path.as_os_str().as_bytes();
        if bytes.len() > 107
            || bytes.first() != Some(&b'/')
            || bytes.contains(&0)
            || bytes
                .split(|byte| *byte == b'/')
                .skip(1)
                .any(|part| part.is_empty() || part == b"." || part == b"..")
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid Unix socket pathname",
            ));
        }
        let mut value = Self {
            bytes: [0; 107],
            length: u8::try_from(bytes.len()).map_err(|_| refused())?,
        };
        value
            .bytes
            .get_mut(..bytes.len())
            .ok_or_else(refused)?
            .copy_from_slice(bytes);
        Ok(value)
    }
    pub fn as_path(&self) -> &Path {
        Path::new(std::ffi::OsStr::from_bytes(
            self.bytes
                .get(..usize::from(self.length))
                .unwrap_or_default(),
        ))
    }
}
impl std::fmt::Debug for SocketPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.as_path().fmt(f)
    }
}

/// Validate a private endpoint before connecting. Call on every connection,
/// including reconnects; a previously checked path is not retained authority.
pub fn check_private_socket(path: &Path) -> io::Result<()> {
    check_private_socket_parent(path)?;
    let metadata = fs::symlink_metadata(path)?;
    let uid = fs::metadata("/proc/self")?.uid();
    if !metadata.file_type().is_socket()
        || metadata.uid() != uid
        || metadata.mode() & 0o7777 != 0o600
    {
        return Err(refused());
    }
    Ok(())
}

/// Validate the socket pathname and private parent before binding. The caller
/// owns listener exclusion, mode-0600 publication and inode-aware cleanup.
pub fn check_private_socket_parent(path: &Path) -> io::Result<()> {
    SocketPath::new(path)?;
    let uid = fs::metadata("/proc/self")?.uid();
    let parent = path.parent().ok_or_else(refused)?;
    let metadata = fs::symlink_metadata(parent)?;
    if !metadata.is_dir() || metadata.uid() != uid || metadata.mode() & 0o7777 != 0o700 {
        return Err(refused());
    }
    for ancestor in parent.ancestors().skip(1) {
        let metadata = fs::symlink_metadata(ancestor)?;
        if !metadata.is_dir()
            || (metadata.uid() != 0 && metadata.uid() != uid)
            || (metadata.mode() & 0o022 != 0 && metadata.mode() & 0o1000 == 0)
        {
            return Err(refused());
        }
    }
    Ok(())
}
fn refused() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "Unix socket requires a private owned directory and mode-0600 owned socket",
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::TempDir;
    use std::os::unix::{
        fs::{symlink, PermissionsExt},
        net::UnixListener,
    };
    fn mode(path: &Path, value: u32) {
        fs::set_permissions(path, fs::Permissions::from_mode(value)).unwrap();
    }

    #[test]
    fn only_private_owned_socket_paths_are_admitted() {
        let root = TempDir::new("private-socket").unwrap();
        let path = root.join("socket");
        check_private_socket_parent(&path).unwrap();
        assert!(check_private_socket(&path).is_err());
        let _listener = UnixListener::bind(&path).unwrap();
        mode(&path, 0o600);
        check_private_socket(&path).unwrap();
        mode(&path, 0o660);
        assert!(check_private_socket(&path).is_err());
        mode(&path, 0o600);
        mode(&root, 0o750);
        assert!(check_private_socket(&path).is_err());
        mode(&root, 0o700);
        check_private_socket(&path).unwrap();
    }
    #[test]
    fn links_regular_files_and_noncanonical_names_are_refused() {
        let root = TempDir::new("socket-path-refusal").unwrap();
        let path = root.join("socket");
        fs::write(&path, b"").unwrap();
        mode(&path, 0o600);
        assert!(check_private_socket(&path).is_err());
        let linked = root.join("link");
        symlink(&path, &linked).unwrap();
        assert!(check_private_socket(&linked).is_err());
        let directory = root.join("directory");
        symlink(root.path(), &directory).unwrap();
        assert!(check_private_socket_parent(&directory.join("socket")).is_err());
        for name in [
            "relative",
            "@abstract",
            "/",
            "/tmp/../socket",
            "/tmp/./socket",
            "/tmp//socket",
            "/tmp/socket/",
        ] {
            assert_eq!(
                check_private_socket_parent(Path::new(name))
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::InvalidInput
            );
        }
        assert!(check_private_socket_parent(Path::new(&format!("/{}", "x".repeat(107)))).is_err());
    }
}

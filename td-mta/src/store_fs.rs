//! Confined directory lookup and private-root path admission; no store activation.
use crate::{store_fs_sys, store_paths::Name};
use std::{ffi::CStr, fmt, os::unix::fs::MetadataExt};
use std::{fs::File, fs::Metadata, io};

/// Which part of root admission failed. Contains no operator pathname.
#[derive(Debug)]
pub enum RootError {
    Path,
    ServiceIdentity,
    Io(io::Error),
    Owner,
    WritableAncestor,
    PrivateMode,
}
impl fmt::Display for RootError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Path => f.write_str("invalid absolute data root path"),
            Self::ServiceIdentity => {
                f.write_str("data root admission requires a non-root service UID")
            }
            Self::Io(error) => write!(f, "data root descriptor operation: {error}"),
            Self::Owner => f.write_str("data root or ancestor has an untrusted owner"),
            Self::WritableAncestor => {
                f.write_str("data root ancestor permits group or other writes")
            }
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

/// A private data directory reached through checked ancestor descriptors.
/// Filesystem admission, the writer lock and store-format validation remain
/// separate requirements; this type alone does not authorize service activation.
#[derive(Debug)]
pub struct PrivateRoot {
    directory: Directory,
    service_uid: u32,
}
impl PrivateRoot {
    /// Startup-only lookup under the actual effective service UID. The process
    /// must retain its unprivileged deployment credentials for the owner's life.
    /// Deployment must ensure UID 0 and the service UID are unambiguous in this
    /// user namespace: map both identities and use an overflow UID equal to
    /// neither. This operation does not validate mappings or the overflow sysctl.
    /// Root and the service UID are trusted; hostile writers with either identity
    /// or authority to change the mount namespace are outside this contract.
    pub fn open(path: &str) -> Result<Self, RootError> {
        crate::config::values::absolute_path(path).map_err(|_| RootError::Path)?;
        let tail = path.strip_prefix('/').ok_or(RootError::Path)?;
        let uid = store_fs_sys::effective_uid()?;
        if uid == 0 {
            return Err(RootError::ServiceIdentity);
        }
        let anchor = File::open("/")?;
        Self::walk(anchor, tail, uid)
    }

    fn walk(mut current: File, tail: &str, uid: u32) -> Result<Self, RootError> {
        let mut scratch = [0_u8; crate::config::values::MAX_PATH_BYTES + 1];
        if !tail.is_empty() {
            for component in tail.split('/') {
                check_ancestor(&current.metadata()?, uid)?;
                let end = component.len().checked_add(1).ok_or(RootError::Path)?;
                let bytes = scratch.get_mut(..end).ok_or(RootError::Path)?;
                bytes
                    .get_mut(..component.len())
                    .ok_or(RootError::Path)?
                    .copy_from_slice(component.as_bytes());
                *bytes.get_mut(component.len()).ok_or(RootError::Path)? = 0;
                let name = CStr::from_bytes_with_nul(bytes).map_err(|_| RootError::Path)?;
                // Keep the checked parent open through the child's lookup.
                current = store_fs_sys::open_directory(&current, name)?;
            }
        }
        let metadata = current.metadata()?;
        if metadata.uid() != uid {
            return Err(RootError::Owner);
        }
        if !metadata.is_dir() || metadata.mode() & 0o7777 != 0o700 {
            return Err(RootError::PrivateMode);
        }
        Ok(Self {
            directory: Directory(current),
            service_uid: uid,
        })
    }

    pub const fn service_uid(&self) -> u32 {
        self.service_uid
    }

    /// Child lookup still establishes only Directory's confinement guarantees.
    /// It does not inherit this root's checked ownership or permission facts.
    pub fn directory(&self) -> &Directory {
        &self.directory
    }
}

fn check_ancestor(metadata: &Metadata, uid: u32) -> Result<(), RootError> {
    if !metadata.is_dir() || !trusted_ancestor_owner(metadata.uid(), uid) {
        return Err(RootError::Owner);
    }
    if metadata.mode() & 0o022 != 0 {
        return Err(RootError::WritableAncestor);
    }
    Ok(())
}

fn trusted_ancestor_owner(owner: u32, service: u32) -> bool {
    owner == 0 || owner == service
}

/// Pins a directory inode independently of its pathname. This does not establish
/// trusted ancestry, ownership, mode, filesystem suitability or a writer lock.
#[derive(Debug)]
pub struct Directory(File);

impl Directory {
    /// Takes a caller-opened anchor, checking only that it is a directory.
    /// The caller remains responsible for how the anchor was obtained.
    pub fn from_file(file: File) -> io::Result<Self> {
        if !file.metadata()?.is_dir() {
            return Err(io::ErrorKind::NotADirectory.into());
        }
        Ok(Self(file))
    }

    /// Resolves a generated name beneath this descriptor, rejecting every
    /// symlink. Names are relative to this handle, normally the store root.
    /// Mount crossings remain possible; this is not filesystem admission.
    /// Kernel errors, including unsupported openat2, propagate without retry.
    pub fn open(&self, name: &Name) -> io::Result<Self> {
        let name = name
            .as_c_str()
            .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
        store_fs_sys::open_directory(&self.0, name).map(Self)
    }

    /// Metadata is read from the retained descriptor, never by reopening a path.
    pub fn metadata(&self) -> io::Result<Metadata> {
        self.0.metadata()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::{
        fs,
        os::unix::fs::{symlink, DirBuilderExt, PermissionsExt},
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
    };

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "td-mta-root-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
            Self(path)
        }
        fn walk(&self, tail: &str) -> Result<PrivateRoot, RootError> {
            PrivateRoot::walk(
                File::open(&self.0).unwrap(),
                tail,
                store_fs_sys::effective_uid().unwrap(),
            )
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn private_walk_validates_each_descriptor_and_retains_the_selected_inode() {
        let fixture = Fixture::new();
        let ancestor = fixture.0.join("ancestor");
        let data = ancestor.join("data");
        fs::DirBuilder::new().mode(0o755).create(&ancestor).unwrap();
        fs::DirBuilder::new().mode(0o700).create(&data).unwrap();
        let root = fixture.walk("ancestor/data").unwrap();
        let metadata = root.directory().metadata().unwrap();
        assert_eq!(root.service_uid(), metadata.uid());
        assert_eq!(metadata.mode() & 0o7777, 0o700);
        fs::rename(&data, ancestor.join("moved")).unwrap();
        fs::DirBuilder::new().mode(0o700).create(&data).unwrap();
        assert_ne!(fs::metadata(&data).unwrap().ino(), metadata.ino());
        assert_eq!(root.directory().metadata().unwrap().ino(), metadata.ino());
    }

    #[test]
    fn writable_ancestors_including_sticky_and_nonprivate_roots_refuse() {
        let fixture = Fixture::new();
        let ancestor = fixture.0.join("ancestor");
        let data = ancestor.join("data");
        fs::create_dir(&ancestor).unwrap();
        fs::DirBuilder::new().mode(0o700).create(&data).unwrap();
        for mode in [0o720, 0o702, 0o777, 0o1777] {
            fs::set_permissions(&ancestor, fs::Permissions::from_mode(mode)).unwrap();
            assert!(matches!(
                fixture.walk("ancestor/data"),
                Err(RootError::WritableAncestor)
            ));
        }
        fs::set_permissions(&ancestor, fs::Permissions::from_mode(0o755)).unwrap();
        for mode in [0o755, 0o750, 0o710, 0o1700, 0o2700, 0o4700] {
            fs::set_permissions(&data, fs::Permissions::from_mode(mode)).unwrap();
            assert!(matches!(
                fixture.walk("ancestor/data"),
                Err(RootError::PrivateMode)
            ));
        }
        fs::set_permissions(&data, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(fixture.walk("ancestor/data").is_ok());
    }

    #[test]
    fn root_owner_is_service_only_and_ancestry_does_not_follow_links() {
        let fixture = Fixture::new();
        let uid = store_fs_sys::effective_uid().unwrap();
        assert_eq!(fs::metadata(&fixture.0).unwrap().uid(), uid);
        let wrong_uid = uid ^ 1;
        assert!(matches!(
            PrivateRoot::walk(File::open(&fixture.0).unwrap(), "", wrong_uid),
            Err(RootError::Owner)
        ));
        symlink(".", fixture.0.join("link")).unwrap();
        assert!(
            matches!(fixture.walk("link"), Err(RootError::Io(error)) if error.raw_os_error() == Some(40))
        );
        fs::write(fixture.0.join("file"), b"still a file").unwrap();
        assert!(
            matches!(fixture.walk("file"), Err(RootError::Io(error)) if error.kind() == io::ErrorKind::NotADirectory)
        );
        assert!(
            matches!(fixture.walk("missing"), Err(RootError::Io(error)) if error.kind() == io::ErrorKind::NotFound)
        );
    }

    #[test]
    fn ancestor_owner_refusal_precedes_child_lookup_and_root_is_trusted() {
        assert!(trusted_ancestor_owner(0, 1000));
        assert!(trusted_ancestor_owner(1000, 1000));
        assert!(!trusted_ancestor_owner(1001, 1000));
        assert!(!trusted_ancestor_owner(u32::MAX, 1000));
        let fixture = Fixture::new();
        let uid = store_fs_sys::effective_uid().unwrap();
        let outcome = PrivateRoot::walk(File::open(&fixture.0).unwrap(), "missing", uid ^ 1);
        if uid == 0 {
            assert!(
                matches!(outcome, Err(RootError::Io(error)) if error.kind() == io::ErrorKind::NotFound)
            );
        } else {
            assert!(matches!(outcome, Err(RootError::Owner)));
        }
    }

    #[test]
    fn public_root_lookup_rejects_lexical_aliases_before_io() {
        for path in [
            "",
            "relative",
            "/tmp/../root",
            "/tmp/./root",
            "//tmp",
            "/tmp/",
            "/bad\0name",
        ] {
            assert!(
                matches!(PrivateRoot::open(path), Err(RootError::Path)),
                "{path:?}"
            );
        }
        assert!(matches!(
            PrivateRoot::open(&format!("/{}", "x".repeat(4095))),
            Err(RootError::Path)
        ));
    }

    #[test]
    fn kernel_refuses_absolute_and_parent_escape_even_without_name_validation() {
        let anchor = File::open("/").unwrap();
        for name in [c"/", c"..", c"../"] {
            assert_eq!(
                store_fs_sys::open_directory(&anchor, name)
                    .unwrap_err()
                    .raw_os_error(),
                Some(18),
            );
        }
        assert_eq!(
            store_fs_sys::open_directory(&anchor, c"")
                .unwrap_err()
                .kind(),
            io::ErrorKind::NotFound,
        );
    }
}

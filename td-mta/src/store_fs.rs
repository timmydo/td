//! Confined directories, private-root path checks and space observations.
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

/// A recognized magic value, not filesystem admission or backing identity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FilesystemFamily {
    Xfs,
    Btrfs,
    /// Ext2/ext3/ext4 share this value; further qualification must select ext4.
    ExtFamily,
}

/// One possibly stale observation. Identity, quota policy, probe-ticket matching
/// and write admission are separate requirements, even on a writable mount.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FilesystemSpace {
    pub family: FilesystemFamily,
    /// Statfs counting unit; qualification must establish allocation granularity.
    pub counting_unit: u64,
    /// Scaled f_bavail, not the service user's quota headroom.
    pub available_bytes: u64,
    pub inodes: crate::admission::space::Inodes,
    pub read_only: bool,
}

const ST_RDONLY: i64 = 1;
const ST_VALID: i64 = 0x20;

fn filesystem_observation(raw: &store_fs_sys::StatFs) -> io::Result<FilesystemSpace> {
    use crate::admission::space::Inodes;
    let invalid = || io::Error::from(io::ErrorKind::InvalidData);
    let counting_unit = u64::try_from(raw.fragment_size).map_err(|_| invalid())?;
    if counting_unit == 0 || raw.block_size <= 0 || raw.flags & ST_VALID == 0 {
        return Err(invalid());
    }
    let family = match raw.kind {
        0x5846_5342 => FilesystemFamily::Xfs,
        0x9123_683e => FilesystemFamily::Btrfs,
        0xef53 => FilesystemFamily::ExtFamily,
        _ => return Err(io::ErrorKind::Unsupported.into()),
    };
    let available_blocks = u64::try_from(raw.available_blocks).map_err(|_| invalid())?;
    let available_bytes = available_blocks
        .checked_mul(counting_unit)
        .ok_or_else(invalid)?;
    let inodes = match family {
        FilesystemFamily::Btrfs => Inodes::Unsupported,
        FilesystemFamily::Xfs | FilesystemFamily::ExtFamily => {
            let files = u64::try_from(raw.files).map_err(|_| invalid())?;
            let free = u64::try_from(raw.free_files).map_err(|_| invalid())?;
            if free > files {
                return Err(invalid());
            }
            Inodes::Available(free)
        }
    };
    Ok(FilesystemSpace {
        family,
        counting_unit,
        available_bytes,
        inodes,
        read_only: raw.flags & ST_RDONLY != 0,
    })
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

    /// Observes this retained directory's filesystem without allocations.
    /// This is not a backing-capacity identity or authorization to write.
    pub fn filesystem_space(&self) -> io::Result<FilesystemSpace> {
        filesystem_observation(&store_fs_sys::filesystem_space(&self.0)?)
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

    fn stats(kind: i64) -> store_fs_sys::StatFs {
        store_fs_sys::StatFs {
            kind,
            block_size: 4096,
            fragment_size: 1024,
            free_blocks: 100,
            available_blocks: 7,
            files: 9000,
            free_files: 5000,
            flags: ST_VALID,
            ..Default::default()
        }
    }

    #[test]
    fn space_observation_uses_available_blocks_and_fragment_units() {
        use crate::admission::space::Inodes;
        for (kind, family) in [
            (0x5846_5342, FilesystemFamily::Xfs),
            (0xef53, FilesystemFamily::ExtFamily),
            (0x9123_683e, FilesystemFamily::Btrfs),
        ] {
            let mut raw = stats(kind);
            let observed = filesystem_observation(&raw).unwrap();
            assert_eq!(observed.family, family);
            assert_eq!(observed.counting_unit, 1024);
            assert_eq!(observed.available_bytes, 7168);
            assert_eq!(
                observed.inodes,
                if family == FilesystemFamily::Btrfs {
                    Inodes::Unsupported
                } else {
                    Inodes::Available(5000)
                }
            );
            assert!(!observed.read_only);
            raw.flags = ST_VALID | ST_RDONLY;
            raw.available_blocks = 0;
            raw.free_files = 0;
            let exhausted = filesystem_observation(&raw).unwrap();
            assert!(exhausted.read_only);
            assert_eq!(exhausted.available_bytes, 0);
            if family != FilesystemFamily::Btrfs {
                assert_eq!(exhausted.inodes, Inodes::Available(0));
            }
        }
        assert_eq!(
            filesystem_observation(&stats(0x9fa0)).unwrap_err().kind(),
            io::ErrorKind::Unsupported
        );
    }

    #[test]
    fn space_observation_rejects_invalid_counts_units_and_overflow() {
        for (unit, blocks, files, free) in [
            (0, 7, 9000, 5000),
            (-1, 7, 9000, 5000),
            (1024, -1, 9000, 5000),
            (1024, i64::MAX, 9000, 5000),
            (1024, 7, -1, 0),
            (1024, 7, 9000, -1),
            (1024, 7, 0, 1),
        ] {
            let mut raw = stats(0x5846_5342);
            raw.fragment_size = unit;
            raw.available_blocks = blocks;
            raw.files = files;
            raw.free_files = free;
            assert_eq!(
                filesystem_observation(&raw).unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
        }
        let mut raw = stats(0x5846_5342);
        raw.block_size = 0;
        assert_eq!(
            filesystem_observation(&raw).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        raw.block_size = -1;
        assert!(filesystem_observation(&raw).is_err());
        raw.block_size = 4096;
        raw.fragment_size = 1;
        raw.available_blocks = i64::MAX;
        assert_eq!(
            filesystem_observation(&raw).unwrap().available_bytes,
            i64::MAX as u64
        );
    }

    #[test]
    fn space_observation_requires_valid_kernel_flags() {
        let mut raw = stats(0x5846_5342);
        for flags in [0, ST_RDONLY, 0x10] {
            raw.flags = flags;
            assert_eq!(
                filesystem_observation(&raw).unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
        }
        raw.flags = ST_VALID;
        assert!(!filesystem_observation(&raw).unwrap().read_only);
        raw.flags = ST_VALID | ST_RDONLY;
        assert!(filesystem_observation(&raw).unwrap().read_only);
    }

    #[test]
    fn kernel_space_probe_checks_procfs_abi_and_refuses_unsupported_family() {
        let proc = Directory::from_file(File::open("/proc").unwrap()).unwrap();
        let raw = store_fs_sys::filesystem_space(&proc.0).unwrap();
        assert_eq!(raw.kind, 0x9fa0);
        assert!(raw.block_size > 0);
        assert!(raw.fragment_size > 0);
        assert_eq!(raw.name_length, 255);
        assert_eq!(raw.flags & ST_VALID, ST_VALID);
        assert_eq!(
            proc.filesystem_space().unwrap_err().kind(),
            io::ErrorKind::Unsupported
        );
    }

    #[test]
    fn space_probe_retains_directory_across_rename_and_path_replacement() {
        let fixture = Fixture::new();
        let original = fixture.0.join("data");
        let moved = fixture.0.join("moved");
        std::fs::create_dir(&original).unwrap();
        let directory = Directory::from_file(File::open(&original).unwrap()).unwrap();
        let before = store_fs_sys::filesystem_space(&directory.0).unwrap();
        std::fs::rename(&original, &moved).unwrap();
        std::os::unix::fs::symlink("/proc", &original).unwrap();
        let replacement = store_fs_sys::filesystem_space(&File::open(&original).unwrap()).unwrap();
        assert_eq!(replacement.kind, 0x9fa0);
        assert_ne!(before.kind, replacement.kind);
        let after = store_fs_sys::filesystem_space(&directory.0).unwrap();
        assert_eq!(after.kind, before.kind);
        assert_eq!(after.fsid, before.fsid);
        assert_eq!(after.fragment_size, before.fragment_size);
    }

    #[test]
    fn space_probe_requires_success_on_executable_filesystem() {
        let executable = std::env::current_exe().unwrap();
        let directory =
            Directory::from_file(File::open(executable.parent().unwrap()).unwrap()).unwrap();
        let raw = store_fs_sys::filesystem_space(&directory.0).unwrap();
        let observed = directory.filesystem_space().unwrap();
        assert_eq!(observed.counting_unit, raw.fragment_size as u64);
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

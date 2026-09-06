//! Private Linux x86-64 Unix socket publication. No worker or editor access.

use std::ffi::OsString;
use std::fs::{self, File, Metadata, OpenOptions, Permissions};
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Component, Path, PathBuf};

const O_DIRECTORY: i32 = 0o200000;
const O_NOFOLLOW: i32 = 0o400000;
const O_PATH: i32 = 0o10000000;
const PATH_BYTES: usize = 107;
const NAME_BYTES: usize = 80;
const STATUS_BYTES: usize = 64 * 1024;
const UID_BYTES: usize = 11;

/// Owns the listener and pins both its parent and filesystem socket inode.
/// Explicit close reports cleanup errors; Drop attempts the same check.
#[derive(Debug)]
pub struct Socket {
    listener: UnixListener,
    parent: File,
    name: OsString,
    node: File,
    cleanup: bool,
}

impl Socket {
    /// Existing names are never replaced or connected to, including stale sockets.
    /// Requires an absolute pathname and an owned mode-0700 parent.
    pub fn bind(path: &Path) -> io::Result<Self> {
        Self::bind_with(path, current_uid()?, || Ok(()))
    }

    fn bind_with(
        path: &Path,
        uid: u32,
        before_check: impl FnOnce() -> io::Result<()>,
    ) -> io::Result<Self> {
        let raw = path.as_os_str().as_bytes();
        let leaf = raw.rsplit(|b| *b == b'/').next().unwrap_or_default();
        if !path.is_absolute()
            || raw.len() > PATH_BYTES
            || leaf.len() > NAME_BYTES
            || raw.contains(&0)
            || matches!(leaf, b"" | b"." | b"..")
        {
            return Err(invalid(
                "control socket needs an absolute non-NUL path of at most 107 bytes and a basename of at most 80 bytes",
            ));
        }
        let name = path
            .file_name()
            .ok_or_else(|| invalid("missing socket name"))?
            .to_owned();
        let parent_path = path
            .parent()
            .ok_or_else(|| invalid("missing socket parent"))?;
        let parent = directory(parent_path, uid)?;
        let original_parent = parent.metadata()?;
        private_parent(&original_parent, uid)?;
        let pinned_path = descriptor_path(&parent).join(&name);
        if pinned_path.as_os_str().as_bytes().len() > PATH_BYTES {
            return Err(invalid(
                "descriptor-pinned socket path exceeds Linux pathname limit",
            ));
        }
        match fs::symlink_metadata(&pinned_path) {
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "control endpoint already exists; remove it explicitly",
                ))
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let listener = UnixListener::bind(&pinned_path)?;
        let node = OpenOptions::new()
            .read(true)
            .custom_flags(O_PATH | O_NOFOLLOW)
            .open(&pinned_path)
            .map_err(|error| {
                io::Error::other(format!(
                    "socket bound but inode could not be pinned; name retained: {error}"
                ))
            })?;
        let metadata = node.metadata()?;
        if !metadata.file_type().is_socket() || metadata.uid() != uid {
            return Err(io::Error::other(
                "bound socket name changed before inode pinning; name retained",
            ));
        }
        let socket = Self {
            listener,
            parent,
            name,
            node,
            cleanup: true,
        };
        // The kernel procfs link addresses the pinned inode, not a renamed leaf.
        fs::set_permissions(descriptor_path(&socket.node), Permissions::from_mode(0o600))?;
        let metadata = socket.node.metadata()?;
        if metadata.mode() & 0o7777 != 0o600 || metadata.uid() != uid {
            return Err(io::Error::other(
                "control socket permissions could not be established",
            ));
        }
        socket.listener.set_nonblocking(true)?;
        before_check()?;
        let visible_parent = directory(parent_path, uid)?.metadata()?;
        private_parent(&visible_parent, uid)?;
        if identity(&visible_parent) != identity(&original_parent) {
            return Err(io::Error::other(
                "control socket parent changed during publication",
            ));
        }
        socket.check_name()?;
        Ok(socket)
    }

    /// Accepted connections are nonblocking too; a separate worker owns all
    /// framing, connection admission, deadlines and response scheduling.
    pub fn accept(&self) -> io::Result<UnixStream> {
        let (stream, _) = self.listener.accept()?;
        stream.set_nonblocking(true)?;
        Ok(stream)
    }

    pub fn close(mut self) -> io::Result<()> {
        self.cleanup = false;
        self.remove_named()
    }

    fn check_name(&self) -> io::Result<()> {
        let named = fs::symlink_metadata(descriptor_path(&self.parent).join(&self.name))?;
        if !named.file_type().is_socket() || identity(&named) != identity(&self.node.metadata()?) {
            return Err(io::Error::other(
                "control socket name no longer identifies the owned inode",
            ));
        }
        Ok(())
    }

    fn remove_named(&self) -> io::Result<()> {
        let result = self
            .check_name()
            .and_then(|()| fs::remove_file(descriptor_path(&self.parent).join(&self.name)));
        match result {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                // Distinguish an absent leaf from lost procfs descriptor access.
                fs::metadata(descriptor_path(&self.parent)).map_err(|error| {
                    io::Error::other(format!("control cleanup parent is unavailable: {error}"))
                })?;
                Ok(())
            }
            Ok(()) => Ok(()),
            Err(error) => Err(error),
        }
    }
}

impl Drop for Socket {
    fn drop(&mut self) {
        if self.cleanup {
            let _ = self.remove_named();
        }
    }
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

fn identity(metadata: &Metadata) -> (u64, u64) {
    (metadata.dev(), metadata.ino())
}

fn private_parent(metadata: &Metadata, uid: u32) -> io::Result<()> {
    if !metadata.is_dir() || metadata.uid() != uid || metadata.mode() & 0o7777 != 0o700 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "control socket parent must be caller-owned mode 0700",
        ));
    }
    Ok(())
}

fn descriptor_path(file: &File) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()))
}

fn directory(path: &Path, uid: u32) -> io::Result<File> {
    if !path.is_absolute() {
        return Err(invalid("socket parent must be absolute"));
    }
    let open = |path: &Path| {
        OpenOptions::new()
            .read(true)
            .custom_flags(O_PATH | O_DIRECTORY | O_NOFOLLOW)
            .open(path)
    };
    let mut current = open(Path::new("/"))?;
    trusted_ancestor(&current.metadata()?, uid)?;
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(name) => {
                current = open(&descriptor_path(&current).join(name))?;
                trusted_ancestor(&current.metadata()?, uid)?;
            }
            _ => {
                return Err(invalid(
                    "socket parent requires normal directory components",
                ))
            }
        }
    }
    Ok(current)
}

fn trusted_ancestor(metadata: &Metadata, uid: u32) -> io::Result<()> {
    if !metadata.is_dir()
        || (metadata.uid() != uid && metadata.uid() != 0)
        || (metadata.mode() & 0o022 != 0 && metadata.mode() & 0o1000 == 0)
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "socket ancestors must be caller/root-owned and not group/other-writable unless sticky",
        ));
    }
    Ok(())
}

fn current_uid() -> io::Result<u32> {
    let mut bytes = Vec::with_capacity(STATUS_BYTES + 1);
    File::open("/proc/self/status")?
        .take(STATUS_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > STATUS_BYTES {
        return Err(invalid("process status exceeds 64 KiB"));
    }
    let uid = status_uid(&bytes)?;
    let mut bytes = Vec::with_capacity(UID_BYTES + 1);
    File::open("/proc/sys/kernel/overflowuid")?
        .take(UID_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    unambiguous_uid(uid, overflow_uid(&bytes)?)
}

fn overflow_uid(bytes: &[u8]) -> io::Result<u32> {
    if bytes.len() > UID_BYTES {
        return Err(invalid("overflow UID record exceeds 11 bytes"));
    }
    let bytes = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    let value = std::str::from_utf8(bytes).map_err(|_| invalid("invalid overflow UID"))?;
    u32::try_from(crate::control::decimal(value).map_err(|_| invalid("invalid overflow UID"))?)
        .map_err(|_| invalid("overflow UID is out of range"))
}

fn unambiguous_uid(uid: u32, overflow: u32) -> io::Result<u32> {
    if overflow == uid || overflow == 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "overflow UID aliases a trusted owner; control socket ownership is ambiguous",
        ));
    }
    Ok(uid)
}

fn status_uid(bytes: &[u8]) -> io::Result<u32> {
    let mut found = None;
    for line in bytes.split(|b| *b == b'\n') {
        let Some(fields) = line.strip_prefix(b"Uid:") else {
            continue;
        };
        if found.is_some() {
            return Err(invalid("duplicate process UID record"));
        }
        let fields =
            std::str::from_utf8(fields).map_err(|_| invalid("invalid process UID record"))?;
        let mut ids = fields.split_ascii_whitespace();
        let first = ids.next().ok_or_else(|| invalid("missing process UID"))?;
        let uid = u32::try_from(
            crate::control::decimal(first).map_err(|_| invalid("invalid process UID"))?,
        )
        .map_err(|_| invalid("process UID overflow"))?;
        for _ in 0..3 {
            let id = ids
                .next()
                .ok_or_else(|| invalid("short process UID record"))?;
            if crate::control::decimal(id).map_err(|_| invalid("invalid process UID"))?
                != u64::from(uid)
            {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "mixed process credentials refuse a control socket",
                ));
            }
        }
        if ids.next().is_some() {
            return Err(invalid("extra process UID fields"));
        }
        found = Some(uid);
    }
    found.ok_or_else(|| invalid("process UID record missing"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::fs::{symlink, DirBuilderExt};
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);
    struct Directory(PathBuf);
    impl Directory {
        fn new() -> Self {
            // Short fixed parent separates the basename and whole-path caps.
            for _ in 0..128 {
                let path = Path::new("/tmp").join(format!(
                    "tds{:x}-{:x}",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed)
                ));
                match fs::DirBuilder::new().mode(0o700).create(&path) {
                    Ok(()) => {
                        fs::set_permissions(&path, Permissions::from_mode(0o700)).unwrap();
                        return Self(path);
                    }
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(error) => panic!("socket fixture: {error}"),
                }
            }
            panic!("socket fixture names exhausted");
        }
        fn path(&self, leaf: &str) -> PathBuf {
            self.0.join(leaf)
        }
        fn child(&self, name: &str) -> PathBuf {
            let path = self.path(name);
            fs::create_dir(&path).unwrap();
            fs::set_permissions(&path, Permissions::from_mode(0o700)).unwrap();
            path
        }
    }
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn flags(file: &impl AsRawFd) -> u32 {
        let info = fs::read_to_string(format!("/proc/self/fdinfo/{}", file.as_raw_fd())).unwrap();
        let flags = info
            .lines()
            .find_map(|line| line.strip_prefix("flags:\t"))
            .unwrap();
        u32::from_str_radix(flags, 8).unwrap()
    }

    #[test]
    fn private_socket_is_reachable_by_requested_path_nonblocking_and_owned_on_close() {
        let dir = Directory::new();
        let path = dir.path("control");
        let socket = Socket::bind(&path).unwrap();
        let meta = fs::symlink_metadata(&path).unwrap();
        assert!(meta.file_type().is_socket());
        assert_eq!(meta.mode() & 0o7777, 0o600);
        assert_eq!(meta.uid(), current_uid().unwrap());
        assert_eq!(flags(&socket.listener) & 0o4000, 0o4000);
        assert_ne!(flags(&socket.listener) & 0o2000000, 0);
        assert_eq!(
            socket.accept().unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        let mut client = UnixStream::connect(&path).unwrap();
        let mut server = socket.accept().unwrap();
        assert_eq!(flags(&server) & 0o4000, 0o4000);
        assert_ne!(flags(&server) & 0o2000000, 0); // close-on-exec
        client.write_all(b"ping").unwrap();
        let mut bytes = [0; 4];
        server.read_exact(&mut bytes).unwrap();
        assert_eq!(&bytes, b"ping");
        socket.close().unwrap();
        assert!(!path.exists());
        assert_eq!(fs::metadata(&dir.0).unwrap().mode() & 0o7777, 0o700);
    }

    #[test]
    fn existing_files_directories_symlinks_live_and_stale_sockets_are_never_replaced() {
        let dir = Directory::new();
        let file = dir.path("file");
        fs::write(&file, b"keep").unwrap();
        let directory = dir.child("directory");
        let link = dir.path("link");
        symlink("missing", &link).unwrap();
        let endpoint = dir.path("socket");
        let listener = UnixListener::bind(&endpoint).unwrap();
        for path in [&file, &directory, &link, &endpoint] {
            let before = fs::symlink_metadata(path).unwrap();
            assert_eq!(
                Socket::bind(path).err().unwrap().kind(),
                io::ErrorKind::AlreadyExists
            );
            assert_eq!(
                identity(&before),
                identity(&fs::symlink_metadata(path).unwrap())
            );
        }
        drop(listener);
        assert_eq!(
            Socket::bind(&endpoint).err().unwrap().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(fs::read(&file).unwrap(), b"keep");
        assert_eq!(fs::read_link(&link).unwrap(), Path::new("missing"));
    }

    #[test]
    fn unowned_nonprivate_symlinked_and_escaping_parents_refuse_before_binding() {
        let dir = Directory::new();
        let path = dir.path("control");
        let uid = current_uid().unwrap();
        assert_eq!(
            Socket::bind_with(&path, uid.wrapping_add(1), || Ok(()))
                .err()
                .unwrap()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        for mode in [0o755, 0o750, 0o770, 0o1700, 0o2700] {
            fs::set_permissions(&dir.0, Permissions::from_mode(mode)).unwrap();
            assert_eq!(
                Socket::bind(&path).unwrap_err().kind(),
                io::ErrorKind::PermissionDenied
            );
            assert!(!path.exists());
        }
        fs::set_permissions(&dir.0, Permissions::from_mode(0o700)).unwrap();
        let real = dir.child("real");
        symlink(&real, dir.path("alias")).unwrap();
        assert_eq!(
            Socket::bind(&dir.path("alias/control")).unwrap_err().kind(),
            io::ErrorKind::NotADirectory
        );
        assert!(!real.join("control").exists());
        let nested = dir.child("real/nested");
        assert_eq!(
            Socket::bind(&dir.path("alias/nested/control"))
                .unwrap_err()
                .kind(),
            io::ErrorKind::NotADirectory
        );
        assert!(!nested.join("control").exists());
        assert_eq!(
            Socket::bind(&dir.path("real/../control"))
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
        for path in [
            Path::new("relative"),
            Path::new("/"),
            Path::new("/tmp/\0control"),
        ] {
            assert_eq!(
                Socket::bind(path).unwrap_err().kind(),
                io::ErrorKind::InvalidInput
            );
        }
        assert_eq!(
            Socket::bind(&dir.path(".")).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            Socket::bind(&dir.path(&"x".repeat(PATH_BYTES)))
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
        assert!(!dir.path("control").exists());
    }

    #[test]
    fn cleanup_refuses_a_replacement_file_or_new_socket_at_the_same_name() {
        let dir = Directory::new();
        let path = dir.path("control");
        let socket = Socket::bind(&path).unwrap();
        fs::rename(&path, dir.path("moved")).unwrap();
        fs::write(&path, b"replacement stays").unwrap();
        assert!(socket.close().is_err());
        assert_eq!(fs::read(&path).unwrap(), b"replacement stays");

        let path = dir.path("second");
        let old = Socket::bind(&path).unwrap();
        fs::rename(&path, dir.path("old-second")).unwrap();
        let new = Socket::bind(&path).unwrap();
        let identity_before = identity(&fs::symlink_metadata(&path).unwrap());
        drop(old);
        assert_eq!(
            identity(&fs::symlink_metadata(&path).unwrap()),
            identity_before
        );
        let _peer = UnixStream::connect(&path).unwrap();
        assert!(new.accept().is_ok());
        new.close().unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn renamed_parent_cleanup_uses_the_pinned_directory_not_a_replacement_path() {
        let dir = Directory::new();
        let parent = dir.child("parent");
        let path = parent.join("control");
        let socket = Socket::bind(&path).unwrap();
        fs::rename(&parent, dir.path("old-parent")).unwrap();
        dir.child("parent");
        fs::write(&path, b"new directory's file").unwrap();
        socket.close().unwrap();
        assert!(!dir.path("old-parent/control").exists());
        assert_eq!(fs::read(&path).unwrap(), b"new directory's file");
    }

    #[test]
    fn publication_rechecks_parent_identity_and_cleans_only_its_pinned_candidate() {
        let dir = Directory::new();
        let parent = dir.child("parent");
        let path = parent.join("control");
        let result = Socket::bind_with(&path, current_uid().unwrap(), || {
            fs::rename(&parent, dir.path("old-parent"))?;
            dir.child("parent");
            fs::write(&path, b"keep")
        });
        assert!(result.is_err());
        assert!(!dir.path("old-parent/control").exists());
        assert_eq!(fs::read(&path).unwrap(), b"keep");
    }

    #[test]
    fn overflow_uid_parsing_and_trusted_owner_collisions_refuse() {
        assert_eq!(overflow_uid(b"65534\n").unwrap(), 65534);
        assert_eq!(overflow_uid(b"4294967295\n").unwrap(), u32::MAX);
        assert_eq!(overflow_uid(b"0").unwrap(), 0);
        for value in [
            b"".as_slice(),
            b"\n",
            b"-1\n",
            b"+1\n",
            b" 1\n",
            b"1 \n",
            b"1\n\n",
            b"1\t2\n",
            b"4294967296\n",
            b"000000000000",
            b"\xff\n",
        ] {
            assert!(overflow_uid(value).is_err(), "{value:?}");
        }
        assert_eq!(unambiguous_uid(0, 65534).unwrap(), 0);
        assert_eq!(unambiguous_uid(1001, 65534).unwrap(), 1001);
        for (uid, overflow) in [(1001, 1001), (65534, 65534), (1001, 0), (0, 0)] {
            assert_eq!(
                unambiguous_uid(uid, overflow).unwrap_err().kind(),
                io::ErrorKind::PermissionDenied
            );
        }
    }

    #[test]
    fn process_uid_parsing_requires_one_complete_unmixed_identity() {
        assert_eq!(
            status_uid(b"Name:\t\xff\nUid:\t12\t12\t12\t12\n").unwrap(),
            12
        );
        assert_eq!(status_uid(b"Uid:\t0\t0\t0\t0\n").unwrap(), 0);
        for status in [
            b"".as_slice(),
            b"Uid: 1 1 1",
            b"Uid: 1 1 1 1 1",
            b"Uid: 1 1 1 1\nUid: 1 1 1 1",
            b"Uid: 1 0 1 1",
            b"Uid: +1 1 1 1",
            b"Uid: 4294967296 4294967296 4294967296 4294967296",
            b"Uid: \xff 1 1 1",
        ] {
            assert!(status_uid(status).is_err());
        }
    }

    #[test]
    fn private_parent_under_shared_ancestor_requires_sticky_protection() {
        let dir = Directory::new();
        let shared = dir.child("shared");
        let parent = dir.child("shared/private");
        fs::set_permissions(&shared, Permissions::from_mode(0o777)).unwrap();
        let path = parent.join("control");
        assert_eq!(
            Socket::bind(&path).err().unwrap().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert!(!path.exists());
        fs::set_permissions(&shared, Permissions::from_mode(0o1777)).unwrap();
        Socket::bind(&path).unwrap().close().unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn basename_limit_trailing_slash_and_missing_cleanup_are_explicit() {
        let dir = Directory::new();
        let accepted = dir.path(&"x".repeat(NAME_BYTES));
        let refused = dir.path(&"x".repeat(NAME_BYTES + 1));
        assert!(refused.as_os_str().as_bytes().len() <= PATH_BYTES);
        Socket::bind(&accepted).unwrap().close().unwrap();
        assert_eq!(
            Socket::bind(&refused).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert!(!refused.exists());
        assert_eq!(
            Socket::bind(&dir.path("control/")).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        let path = dir.path("control");
        let socket = Socket::bind(&path).unwrap();
        fs::remove_file(&path).unwrap();
        socket.close().unwrap();
    }
}

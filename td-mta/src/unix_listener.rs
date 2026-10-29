//! Private pathname listener with cooperative lifetime exclusion.
use crate::{
    ports::Error,
    store_fs::{LockedRoot, PrivateRoot},
};
use std::{
    fs, io,
    os::unix::{
        fs::{FileTypeExt, MetadataExt, PermissionsExt},
        net::{UnixListener, UnixStream},
    },
};
use td_fs::SocketPath;

pub struct PrivateListener {
    listener: UnixListener,
    path: SocketPath,
    device: u64,
    inode: u64,
    _root: LockedRoot,
}
impl PrivateListener {
    pub fn bind(path: SocketPath) -> Result<Self, Error> {
        td_fs::check_private_socket_parent(path.as_path())?;
        let parent = path
            .as_path()
            .parent()
            .and_then(|p| p.to_str())
            .ok_or(Error::Invalid)?;
        let root = PrivateRoot::open(parent).map_err(|_| Error::Forbidden)?;
        let root = root.try_lock().map_err(|error| match error {
            crate::store_fs::LockError::Busy => Error::Busy,
            _ => Error::Forbidden,
        })?;
        Self::bind_locked(path, root)
    }
    pub fn bind_locked(path: SocketPath, root: LockedRoot) -> Result<Self, Error> {
        td_fs::check_private_socket_parent(path.as_path())?;
        let parent = path.as_path().parent().ok_or(Error::Invalid)?;
        let directory = fs::symlink_metadata(parent)?;
        let locked = root.root().directory().metadata()?;
        if directory.dev() != locked.dev() || directory.ino() != locked.ino() {
            return Err(Error::Forbidden);
        }
        match fs::symlink_metadata(path.as_path()) {
            Ok(metadata) if metadata.file_type().is_socket() && metadata.uid() == locked.uid() => {
                fs::remove_file(path.as_path())?;
            }
            Ok(_) => return Err(Error::Forbidden),
            Err(error) if error.kind() == io::ErrorKind::NotFound => (),
            Err(error) => return Err(error.into()),
        }
        let listener = UnixListener::bind(path.as_path())?;
        let metadata = fs::symlink_metadata(path.as_path())?;
        let endpoint = Self {
            listener,
            path,
            device: metadata.dev(),
            inode: metadata.ino(),
            _root: root,
        };
        fs::set_permissions(endpoint.path.as_path(), fs::Permissions::from_mode(0o600))?;
        endpoint.listener.set_nonblocking(true)?;
        td_fs::check_private_socket(endpoint.path.as_path())?;
        let metadata = fs::symlink_metadata(endpoint.path.as_path())?;
        if metadata.dev() != endpoint.device || metadata.ino() != endpoint.inode {
            return Err(Error::Forbidden);
        }
        Ok(endpoint)
    }
    pub fn path(&self) -> &std::path::Path {
        self.path.as_path()
    }
    pub fn accept(&self) -> io::Result<UnixStream> {
        self.listener.accept().map(|(stream, _)| stream)
    }
}
impl Drop for PrivateListener {
    fn drop(&mut self) {
        if fs::symlink_metadata(self.path.as_path()).is_ok_and(|metadata| {
            metadata.file_type().is_socket()
                && metadata.dev() == self.device
                && metadata.ino() == self.inode
        }) {
            let _ = fs::remove_file(self.path.as_path());
        }
    }
}

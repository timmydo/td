//! Descriptor-relative directory lookup, without store or root authorization.
use crate::{store_fs_sys, store_paths::Name};
use std::{fs::File, fs::Metadata, io};

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

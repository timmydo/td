//! Reread a whole disk's partition table after the formatter has synced it.
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt};
use std::path::Path;

use crate::sys;

const O_NONBLOCK: i32 = 0o4000;
const O_NOFOLLOW: i32 = 0o400000;

pub fn run(args: &[String]) -> Result<u8, String> {
    let [device] = args else {
        return Err("usage: reread-partitions <absolute-whole-disk>".into());
    };
    let path = Path::new(device);
    if !path.is_absolute() {
        return Err("partition reread requires an absolute whole-disk path".into());
    }
    let disk = open_disk(path).map_err(|error| format!("open {device}: {error}"))?;
    sys::reread_partitions(&disk)
        .map_err(|error| format!("reread partitions on {device}: {error}"))?;
    Ok(0)
}

fn open_disk(path: &Path) -> io::Result<File> {
    let before = fs::symlink_metadata(path)?;
    if !before.file_type().is_block_device() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "partition reread requires a real block device",
        ));
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(O_NONBLOCK | O_NOFOLLOW)
        .open(path)?;
    let opened = file.metadata()?;
    if !opened.file_type().is_block_device()
        || (before.dev(), before.ino(), before.rdev())
            != (opened.dev(), opened.ino(), opened.rdev())
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "block device changed while opening",
        ));
    }
    Ok(file)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn exact_arity_and_absolute_path_precede_device_access() {
        for args in [vec![], vec!["/dev/absent", "extra"]] {
            assert_eq!(
                run(&args.into_iter().map(str::to_owned).collect::<Vec<_>>()),
                Err("usage: reread-partitions <absolute-whole-disk>".into())
            );
        }
        assert_eq!(
            run(&["relative".into()]),
            Err("partition reread requires an absolute whole-disk path".into())
        );
    }

    #[test]
    fn regular_files_directories_and_character_devices_are_refused() {
        let executable = std::env::current_exe().unwrap();
        for path in [executable.as_path(), Path::new("/"), Path::new("/dev/null")] {
            let error = open_disk(path).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
            assert!(error.to_string().contains("real block device"));
        }
    }

    #[test]
    fn symlinks_are_refused_without_following_their_target() {
        struct Scratch(std::path::PathBuf);
        impl Drop for Scratch {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }
        let scratch = (0..1024)
            .map(|attempt| {
                std::env::temp_dir().join(format!("td-reread-{}-{attempt}", std::process::id()))
            })
            .find_map(|path| match fs::create_dir(&path) {
                Ok(()) => Some(Ok(Scratch(path))),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => None,
                Err(error) => Some(Err(error)),
            })
            .unwrap()
            .unwrap();
        let path = scratch.0.join("link");
        std::os::unix::fs::symlink("/dev/null", &path).unwrap();
        let error = open_disk(&path).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(error.to_string().contains("real block device"));
        assert_eq!(fs::read_link(&path).unwrap(), Path::new("/dev/null"));
    }

    #[test]
    fn open_flags_are_the_x86_linux_values() {
        assert_eq!(O_NONBLOCK, 0x800);
        assert_eq!(O_NOFOLLOW, 0x20000);
    }
}

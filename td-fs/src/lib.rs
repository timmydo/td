//! td's shared file helpers, std-only, one file per helper.
//!
//! A crate cargo builds depends on td-fs by path and calls these through
//! the crate. The direct-rustc td-util recipe links the publication module as an rlib.
//! Single-helper direct-rustc recipes include by
//! `#[path]` only the file of the helper it uses, so a helper it does not
//! use, and that helper's file-system calls, never reach it: td-boot,
//! td-install and the recipe library include `real_file.rs` alone, td-net
//! `real_file.rs` and `replace.rs`, and td-update and td-vm-guest
//! `private_dir.rs` alone. The control-plane workspace includes
//! `temp_dir.rs` only under `cfg(test)` to own disposable fixtures.

#![deny(unsafe_code)]

mod publication;
#[cfg(target_os = "linux")]
pub use publication::replace_stream;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub use publication::{has_attributes, rename_entry};
pub use publication::{replace, replace_if, replace_with};

mod bounded;
mod on_path;
mod private_dir;
mod private_socket;
mod real_file;
mod unix_connect;

pub use bounded::read_bounded;
pub use on_path::{executable_in, executable_on_path};
pub use private_dir::{check_private_dir, private_dir};
pub use private_socket::{
    check_private_socket, check_private_socket_parent, check_server_socket, SocketPath,
};
pub use real_file::{open_real_file, read_bounded_real_file};
pub use unix_connect::connect_until as unix_connect_until;
pub use unix_connect::{
    MAX_CONNECTORS as UNIX_CONNECTORS, STACK_BYTES as UNIX_CONNECT_STACK_BYTES,
};

mod temp_dir;
pub use temp_dir::TempDir;

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod write_tests {
    use super::*;
    use std::io;

    #[test]
    fn a_partial_stream_write_cannot_replace_a_complete_file() {
        use std::io::Write;
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let dir = TempDir::new("replace-short").unwrap();
        let path = dir.join("output");
        std::fs::write(&path, b"previous").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        let old = std::fs::metadata(&path).unwrap();
        let result = replace_stream(
            &std::fs::File::open(dir.path()).unwrap(),
            path.file_name().unwrap(),
            0o600,
            |stage| {
                stage.write_all(b"partial")?;
                Err(io::Error::from_raw_os_error(28))
            },
            |stage, dest| std::fs::rename(stage, dest),
        );
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::StorageFull);
        let after = std::fs::metadata(&path).unwrap();
        assert_eq!(
            (after.ino(), after.mode(), after.nlink()),
            (old.ino(), old.mode(), old.nlink())
        );
        assert_eq!(std::fs::read(path).unwrap(), b"previous");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn a_moved_directory_owns_the_failed_stream_and_its_cleanup() {
        use std::io::Write;
        let dir = TempDir::new("archive-dir-move").unwrap();
        let old = dir.join("old");
        let moved = dir.join("moved");
        std::fs::create_dir(&old).unwrap();
        let directory = std::fs::File::open(&old).unwrap();
        let result = replace_stream(
            &directory,
            "archive".as_ref(),
            0o600,
            |stage| {
                stage.write_all(b"partial")?;
                std::fs::rename(&old, &moved)?;
                std::fs::create_dir(&old)?;
                Err(io::ErrorKind::StorageFull.into())
            },
            |src, dst| std::fs::rename(src, dst),
        );
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::StorageFull);
        assert_eq!(std::fs::read_dir(old).unwrap().count(), 0);
        assert_eq!(std::fs::read_dir(moved).unwrap().count(), 0);
    }

    #[test]
    fn file_and_directory_sync_fail_on_opposite_sides_of_publication() {
        use std::io::Write;
        let dir = TempDir::new("archive-sync").unwrap();
        let path = dir.join("archive");
        for fail_directory in [false, true] {
            std::fs::write(&path, b"old").unwrap();
            let error = crate::publication::staging::stream_with_sync(
                &std::fs::File::open(dir.path()).unwrap(),
                &path,
                0o600,
                |file| file.write_all(b"new"),
                |stage| std::fs::rename(stage, &path),
                |file| {
                    if file.metadata()?.is_dir() == fail_directory {
                        Err(io::ErrorKind::StorageFull.into())
                    } else {
                        file.sync_all()
                    }
                },
            )
            .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::StorageFull);
            assert_eq!(error.to_string().contains("published"), fail_directory);
            assert_eq!(
                std::fs::read(&path).unwrap(),
                if fail_directory { b"new" } else { b"old" }
            );
            assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
        }
    }

    #[test]
    fn a_temporary_left_by_a_crash_is_passed_over_not_removed() {
        let dir = TempDir::new("replace-squat").unwrap();
        let path = dir.join("state");
        // The names the next calls in this process would take, as a crashed
        // process with this pid would have left them. Tests in other
        // threads take serials too, which only moves this one further on.
        let next =
            crate::publication::staging::REPLACE_SERIAL.load(std::sync::atomic::Ordering::Relaxed);
        let squatters: Vec<_> = (next..next + 8)
            .map(|serial| dir.join(format!(".td-replace.{}.{serial}.tmp", std::process::id())))
            .collect();
        for squatter in &squatters {
            std::fs::write(squatter, b"left").unwrap();
        }
        replace(&path, b"placed", 0o600).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"placed");
        for squatter in &squatters {
            assert_eq!(std::fs::read(squatter).unwrap(), b"left");
        }
    }

    #[test]
    fn revoking_a_prepared_write_preserves_the_old_file() {
        let dir = TempDir::new("replace-revoked").unwrap();
        let path = dir.path().join("vault");
        replace(&path, b"previous", 0o600).unwrap();
        let error = replace_if(
            &path,
            b"new",
            0o600,
            |_| Ok(()),
            || Err(io::ErrorKind::Interrupted.into()),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert_eq!(std::fs::read(&path).unwrap(), b"previous");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}

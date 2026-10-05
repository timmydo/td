//! Reading a whole file that must not be larger than a limit. std-only;
//! imports nothing else, so a direct-rustc crate can include it alone.
//!
//! The plain rule, beside `real_file.rs`'s strict one: the path is
//! followed to its end, so a file reached through a final link (a
//! configuration linked into place) is read, and the file's type is not
//! checked. A FIFO would block the open, so a caller that cannot rule one
//! out, or must refuse a link, uses `read_bounded_real_file` instead.

use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

/// The bytes of `path`, refused when there are more than `limit`. One byte
/// past the limit is read, so a file that grows while it is read is refused
/// rather than cut to exactly the limit. Only an open that finds no file
/// answers `NotFound`, so a caller can take that kind as "none"; a read
/// failing with ENOENT (a FUSE file can) is `Other`, never "none". Every
/// error names the path.
pub fn read_bounded(path: &Path, limit: u64) -> io::Result<Vec<u8>> {
    let named =
        |error: io::Error| io::Error::new(error.kind(), format!("{}: {error}", path.display()));
    let file = File::open(path).map_err(named)?;
    let mut bytes = Vec::new();
    file.take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| match error.kind() {
            io::ErrorKind::NotFound => {
                io::Error::other(format!("{}: read: {error}", path.display()))
            }
            _ => named(error),
        })?;
    if u64::try_from(bytes.len()).map_or(true, |read| read > limit) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} exceeds {limit} bytes", path.display()),
        ));
    }
    Ok(bytes)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "td-fs-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_file_at_the_limit_is_read_and_one_past_it_refused() {
        let dir = scratch("bounded");
        let path = dir.join("file");
        std::fs::write(&path, b"12345678").unwrap();
        assert_eq!(read_bounded(&path, 8).unwrap(), b"12345678");
        let error = read_bounded(&path, 7).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(
            error.to_string().contains(&path.display().to_string()),
            "{error}"
        );
        assert_eq!(
            read_bounded(&path, 0).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        std::fs::write(&path, b"").unwrap();
        assert_eq!(read_bounded(&path, 0).unwrap(), b"");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_link_is_followed_and_a_missing_file_says_not_found() {
        let dir = scratch("bounded-link");
        let target = dir.join("target");
        std::fs::write(&target, b"through").unwrap();
        let link = dir.join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert_eq!(read_bounded(&link, 64).unwrap(), b"through");
        let missing = dir.join("absent");
        let error = read_bounded(&missing, 64).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert!(
            error.to_string().contains(&missing.display().to_string()),
            "{error}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_proc_file_is_read_whatever_its_reported_size() {
        // /proc files report a length of 0: the bound is on what is read.
        let status = read_bounded(Path::new("/proc/self/status"), 64 * 1024).unwrap();
        assert!(status.starts_with(b"Name:"));
    }
}

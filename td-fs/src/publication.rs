//! File publication helpers shared by td-fs and the direct-rustc tar build.
#![deny(unsafe_code)]

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[path = "entry.rs"]
mod entry;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub use entry::{has_attributes, rename_entry};

#[path = "replace.rs"]
pub(crate) mod staging;
pub use staging::{replace, replace_if, replace_with};

/// Stage a streamed Linux output beside a literal basename in a pinned
/// directory. Both paths delivered to the publication operation resolve
/// through that descriptor, including when the directory is moved.
#[cfg(target_os = "linux")]
pub fn replace_stream(
    directory: &std::fs::File,
    name: &std::ffi::OsStr,
    mode: u32,
    write: impl FnOnce(&mut std::fs::File) -> std::io::Result<()>,
    publish: impl FnOnce(&std::path::Path, &std::path::Path) -> std::io::Result<()>,
) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;
    if Path::new(name).file_name() != Some(name) {
        return Err(std::io::ErrorKind::InvalidInput.into());
    }
    let mut buffer = [0; 4096];
    let mut out = std::io::Cursor::new(buffer.as_mut_slice());
    write!(out, "/proc/self/fd/{}/", directory.as_raw_fd())?;
    out.write_all(name.as_bytes())?;
    let len = out.position() as usize;
    let bytes = buffer.get(..len).ok_or(std::io::ErrorKind::InvalidInput)?;
    let path = Path::new(std::ffi::OsStr::from_bytes(bytes));
    staging::stream_with_sync(
        directory,
        path,
        mode,
        write,
        |stage| publish(stage, path),
        std::fs::File::sync_all,
    )
}

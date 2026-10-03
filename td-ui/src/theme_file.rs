//! The file a widget window keeps its program's theme in: `theme::path`
//! from this process's environment, the bounded read of the theme it
//! names, and its whole replacement when the chord moves the theme on.

use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use crate::theme::{self, Theme, MAX_FILE_BYTES};

/// Linux's `O_NONBLOCK` on x86-64 and aarch64: opening a FIFO for reading
/// returns at once rather than waiting for a writer.
const O_NONBLOCK: i32 = 0o4000;

/// `theme::path` for `app_id` from this process's environment.
pub fn host_path(app_id: &str) -> Option<PathBuf> {
    theme::path(
        std::env::var_os("XDG_CONFIG_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
        app_id,
    )
}

/// The theme the file at `path` names, or none when there is no file. A
/// regular file of at most `MAX_FILE_BYTES`, checked before it is opened
/// and again once open, as `face_file::read` checks a face; the open does
/// not wait, so a pipe named there cannot hold a program's start. A file
/// that names no theme is an error, and errors name the path.
pub fn read(path: &Path) -> Result<Option<&'static Theme>, String> {
    let named = |why: &dyn std::fmt::Display| format!("{}: {why}", path.display());
    let check = |metadata: fs::Metadata| {
        if !metadata.is_file() {
            return Err(named(&"not a regular file"));
        }
        if metadata.len() > MAX_FILE_BYTES as u64 {
            return Err(named(&format!("larger than {MAX_FILE_BYTES} bytes")));
        }
        Ok(())
    };
    // A file removed between the two looks is no file either.
    let absent = |why: std::io::Error| match why.kind() {
        ErrorKind::NotFound => Ok(None),
        _ => Err(named(&why)),
    };
    match fs::metadata(path) {
        Err(why) => return absent(why),
        Ok(metadata) => check(metadata)?,
    }
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(O_NONBLOCK)
        .open(path)
    {
        Err(why) => return absent(why),
        Ok(file) => file,
    };
    check(file.metadata().map_err(|why| named(&why))?)?;
    let mut bytes = Vec::new();
    file.take(MAX_FILE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|why| named(&why))?;
    theme::parse(&bytes)
        .map(Some)
        .ok_or_else(|| named(&"names no theme"))
}

/// Replaces the file at `path` with `theme`'s name: its directory made
/// first (mode 0700) when missing, then a sibling, the file's name and
/// `.new`, written (mode 0600), flushed and renamed over it, so a crash
/// leaves the old choice or the new one; a link at `path` is replaced,
/// not followed. The directory is locked from the sibling's removal to
/// the rename, so two instances of one program never share the one
/// sibling: a lock held elsewhere is an error at once, not a wait that
/// would hold the window. The sibling's name is fixed, so one a crash
/// left is removed next time.
pub fn write(path: &Path, theme: &Theme) -> Result<(), String> {
    let named = |why: &dyn std::fmt::Display| format!("{}: {why}", path.display());
    let dir = path.parent().ok_or_else(|| named(&"no directory"))?;
    let mut name = path
        .file_name()
        .ok_or_else(|| named(&"no file name"))?
        .to_os_string();
    name.push(".new");
    let sibling = dir.join(name);
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|why| named(&why))?;
    // Released when the handle drops, after the rename or the cleanup.
    let lock = OpenOptions::new()
        .read(true)
        .open(dir)
        .map_err(|why| named(&why))?;
    lock.try_lock().map_err(|why| named(&why))?;
    match fs::remove_file(&sibling) {
        Err(why) if why.kind() != ErrorKind::NotFound => return Err(named(&why)),
        _ => {}
    }
    let written = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&sibling)?;
        file.write_all(theme::text(theme).as_bytes())?;
        file.sync_all()?;
        fs::rename(&sibling, path)
    })();
    if let Err(why) = written {
        let _ = fs::remove_file(&sibling);
        return Err(named(&why));
    }
    Ok(())
}

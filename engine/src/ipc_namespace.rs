//! IPC isolation selected only from a fresh PID-namespace procfs.

use std::fs;
use std::io::{self, Read};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

fn proc_entry(root: &Path, suffix: &[u8]) -> io::Result<PathBuf> {
    let mut path = crate::allocation::copy(root.as_os_str().as_bytes())
        .map_err(|_| io::ErrorKind::OutOfMemory)?;
    crate::allocation::extend_from_slice(&mut path, suffix)
        .map_err(|_| io::ErrorKind::OutOfMemory)?;
    Ok(std::ffi::OsString::from_vec(path).into())
}

/// Call before executing a workload, while its launcher can still unshare.
pub fn isolate(fresh_proc: &Path, unshare: impl FnOnce() -> io::Result<()>) -> io::Result<()> {
    let namespace = proc_entry(fresh_proc, b"/self/ns/ipc")?;
    match fs::read_link(&namespace) {
        Ok(before) => {
            unshare()?;
            if fs::read_link(&namespace)? == before {
                return Err(io::Error::other("IPC namespace did not change"));
            }
            Ok(())
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            require_no_ipc_facilities(fresh_proc)?;
            Ok(())
        }
        Err(error) => Err(error),
    }
}

fn require_no_ipc_facilities(fresh_proc: &Path) -> io::Result<()> {
    for suffix in [b"/sysvipc".as_slice(), b"/sys/fs/mqueue"] {
        match fs::symlink_metadata(proc_entry(fresh_proc, suffix)?) {
            Ok(_) => {
                return Err(io::Error::other(
                    "kernel exposes IPC facilities without an IPC namespace",
                ))
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    let mut filesystems = String::new();
    crate::allocation::reserve_string(&mut filesystems, 65537)
        .map_err(|_| io::ErrorKind::OutOfMemory)?;
    fs::File::open(proc_entry(fresh_proc, b"/filesystems")?)?
        .take(65537)
        .read_to_string(&mut filesystems)?;
    if filesystems.len() > 65536 {
        return Err(io::Error::other("kernel filesystem list exceeds 64 KiB"));
    }
    let mut has_proc = false;
    for line in filesystems.lines() {
        let mut words = line.split_whitespace();
        let first = words
            .next()
            .ok_or_else(|| io::Error::other("empty kernel filesystem row"))?;
        let kind = if first == "nodev" {
            words
                .next()
                .ok_or_else(|| io::Error::other("incomplete kernel filesystem row"))?
        } else {
            first
        };
        if words.next().is_some() {
            return Err(io::Error::other("malformed kernel filesystem row"));
        }
        if kind == "mqueue" {
            return Err(io::Error::other(
                "kernel exposes POSIX message queues without an IPC namespace",
            ));
        }
        has_proc |= kind == "proc";
    }
    if !has_proc {
        return Err(io::Error::other(
            "kernel filesystem list does not identify procfs",
        ));
    }
    Ok(())
}

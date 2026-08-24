//! Boot-time cgroup-v2 controller setup and application delegation.

use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{chown, MetadataExt};
use std::path::Path;

const ROOT: &str = "/sys/fs/cgroup";
const DELEGATE: &str = "/sys/fs/cgroup/td-user-1000";
const SESSION: &str = "/sys/fs/cgroup/td-user-1000/session";
const SESSION_UID: u32 = 1000;
const SESSION_GID: u32 = 1000;
const MAX_CONTROL_BYTES: u64 = 4096;

pub(crate) fn delegate_session() -> io::Result<()> {
    require_cgroup2_mount()?;
    let root = Path::new(ROOT);
    require_words(
        &read_control(&root.join("cgroup.controllers"))?,
        &["memory", "pids"],
        "root cgroup controllers",
    )?;

    // The hierarchy root is exempt from the no-internal-process rule. Keep
    // PID 1 and system services there; only the empty application subtree is
    // delegated below the enabled controllers.
    write_control(&root.join("cgroup.subtree_control"), "+memory +pids")?;
    require_words(
        &read_control(&root.join("cgroup.subtree_control"))?,
        &["memory", "pids"],
        "root cgroup subtree control",
    )?;

    let delegate = Path::new(DELEGATE);
    create_leaf(delegate)?;
    require_empty(&delegate.join("cgroup.procs"), "delegated cgroup")?;
    write_control(&delegate.join("cgroup.subtree_control"), "+memory +pids")?;
    require_words(
        &read_control(&delegate.join("cgroup.subtree_control"))?,
        &["memory", "pids"],
        "delegated cgroup subtree control",
    )?;
    let session = Path::new(SESSION);
    create_leaf(session)?;
    set_owner(session, 0, 0)?;
    for path in [
        delegate.to_path_buf(),
        delegate.join("cgroup.procs"),
        delegate.join("cgroup.subtree_control"),
        delegate.join("cgroup.threads"),
        session.join("cgroup.procs"),
        session.join("cgroup.threads"),
    ] {
        set_owner(&path, SESSION_UID, SESSION_GID)?;
    }
    Ok(())
}

fn create_leaf(path: &Path) -> io::Result<()> {
    match fs::create_dir(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            let canonical = fs::canonicalize(path)?;
            let metadata = fs::symlink_metadata(path)?;
            if canonical == path && metadata.file_type().is_dir() {
                Ok(())
            } else {
                Err(io::Error::other(format!(
                    "cgroup {} is not a canonical directory",
                    path.display()
                )))
            }
        }
        Err(error) => Err(io::Error::new(
            error.kind(),
            format!("create cgroup {}: {error}", path.display()),
        )),
    }
}

fn require_cgroup2_mount() -> io::Result<()> {
    let mountinfo = read_bounded(Path::new("/proc/self/mountinfo"), 1024 * 1024)?;
    match cgroup_mount_filesystem(&mountinfo)? {
        Some("cgroup2") => Ok(()),
        Some(filesystem) => Err(io::Error::other(format!(
            "{ROOT} is mounted as {filesystem:?}, expected cgroup2"
        ))),
        None => Err(io::Error::other(format!("{ROOT} is not mounted"))),
    }
}

fn cgroup_mount_filesystem(text: &str) -> io::Result<Option<&str>> {
    for line in text.lines() {
        let Some((before, after)) = line.split_once(" - ") else {
            return Err(io::Error::other("mountinfo row lacks separator"));
        };
        let mut fields = before.split_ascii_whitespace();
        let mountpoint = fields.nth(4);
        let filesystem = after.split_ascii_whitespace().next();
        if mountpoint == Some(ROOT) {
            return Ok(filesystem);
        }
    }
    Ok(None)
}

fn require_empty(path: &Path, name: &str) -> io::Result<()> {
    let value = read_control(path)?;
    if value.is_empty() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "{name} has internal processes: {value:?}"
        )))
    }
}

fn require_words(text: &str, required: &[&str], name: &str) -> io::Result<()> {
    for required in required {
        if !text.split_ascii_whitespace().any(|word| word == *required) {
            return Err(io::Error::other(format!(
                "{name} lacks required controller {required:?}"
            )));
        }
    }
    Ok(())
}

fn write_control(path: &Path, value: &str) -> io::Result<()> {
    let mut file = OpenOptions::new().write(true).open(path).map_err(|error| {
        io::Error::new(error.kind(), format!("open {}: {error}", path.display()))
    })?;
    let command = format!("{value}\n");
    let written = file.write(command.as_bytes()).map_err(|error| {
        io::Error::new(error.kind(), format!("write {}: {error}", path.display()))
    })?;
    if written != command.len() {
        return Err(io::Error::new(
            io::ErrorKind::WriteZero,
            format!(
                "write {} consumed {written} of {} bytes",
                path.display(),
                command.len()
            ),
        ));
    }
    Ok(())
}

fn set_owner(path: &Path, uid: u32, gid: u32) -> io::Result<()> {
    chown(path, Some(uid), Some(gid)).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("set owner on {}: {error}", path.display()),
        )
    })?;
    let metadata = fs::symlink_metadata(path)?;
    if metadata.uid() != uid || metadata.gid() != gid {
        return Err(io::Error::other(format!(
            "{} did not read back as owned by {uid}:{gid}",
            path.display()
        )));
    }
    Ok(())
}

fn read_control(path: &Path) -> io::Result<String> {
    Ok(read_bounded(path, MAX_CONTROL_BYTES)?.trim().to_string())
}

fn read_bounded(path: &Path, limit: u64) -> io::Result<String> {
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(limit + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(io::Error::other(format!(
            "{} exceeds {limit} bytes",
            path.display()
        )));
    }
    String::from_utf8(bytes)
        .map_err(|error| io::Error::other(format!("{} is not UTF-8: {error}", path.display())))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn controller_names_are_tokens() {
        require_words("cpu memory pids", &["memory", "pids"], "controllers").unwrap();
        assert!(require_words("memoryish pids", &["memory"], "controllers").is_err());
    }

    #[test]
    fn mountinfo_requires_the_exact_cgroup2_shape() {
        let good = "31 22 0:27 / /sys/fs/cgroup rw - cgroup2 cgroup2 rw";
        let bad = "31 22 0:27 / /sys/fs/cgroup rw - tmpfs tmpfs rw";
        assert_eq!(cgroup_mount_filesystem(good).unwrap(), Some("cgroup2"));
        assert_eq!(cgroup_mount_filesystem(bad).unwrap(), Some("tmpfs"));
        assert_eq!(cgroup_mount_filesystem("").unwrap(), None);
        assert!(cgroup_mount_filesystem("malformed").is_err());
    }
}

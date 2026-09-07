//! Root sysinit preparation of activated application runtimes and cgroups.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

const CGROUP: &str = "/sys/fs/cgroup";
const NOFOLLOW: i32 = 0x20000;
const DIRECTORY: i32 = 0x10000;
const NONBLOCK: i32 = 0x800;
const CONTROLLERS: &[&str] = &["cpu", "memory", "pids"];
const CONTROL_LIMIT: u64 = 4096;
const MOUNT_LIMIT: u64 = 1024 * 1024;

fn invalid(reason: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, reason)
}

fn at(parent: &File, name: &str) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}/{name}", parent.as_raw_fd()))
}

fn directory(path: &Path, owners: &[u32]) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(NOFOLLOW | DIRECTORY)
        .open(path)?;
    let metadata = file.metadata()?;
    if !owners.contains(&metadata.uid())
        || metadata.gid() != metadata.uid()
        || metadata.mode() & 0o7022 != 0
    {
        return Err(invalid(
            "application cgroup directory has untrusted ownership or permissions",
        ));
    }
    Ok(file)
}

fn bounded(file: File, limit: u64) -> io::Result<String> {
    let mut text = String::new();
    file.take(limit + 1).read_to_string(&mut text)?;
    if text.len() as u64 > limit {
        return Err(invalid("application cgroup record exceeds its limit"));
    }
    Ok(text)
}

fn control(parent: &File, name: &str, write: bool, owner: u32) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(!write)
        .write(write)
        .custom_flags(NOFOLLOW | NONBLOCK)
        .open(at(parent, name))?;
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.dev() != parent.metadata()?.dev()
        || ![(0, 0), (owner, owner)].contains(&(metadata.uid(), metadata.gid()))
        || metadata.mode() & 0o7022 != 0
    {
        return Err(invalid("application cgroup control has untrusted metadata"));
    }
    Ok(file)
}

fn read(parent: &File, name: &str, owner: u32) -> io::Result<String> {
    bounded(control(parent, name, false, owner)?, CONTROL_LIMIT).map(|text| text.trim().to_string())
}

fn write(parent: &File, name: &str, value: &str, owner: u32) -> io::Result<()> {
    let mut file = control(parent, name, true, owner)?;
    let count = file.write(value.as_bytes())?;
    if count != value.len() {
        return Err(invalid("application cgroup control write was incomplete"));
    }
    Ok(())
}

fn require_controllers(text: &str) -> io::Result<()> {
    if CONTROLLERS
        .iter()
        .all(|required| text.split_ascii_whitespace().any(|word| word == *required))
    {
        Ok(())
    } else {
        Err(invalid(
            "application cgroup lacks cpu, memory or pids controller",
        ))
    }
}

fn require_mount(text: &str, device: u64) -> io::Result<()> {
    let major = ((device >> 8) & 0xfff) | ((device >> 32) & 0xfffff000);
    let minor = (device & 0xff) | ((device >> 12) & 0xffffff00);
    let expected_device = format!("{major}:{minor}");
    let mut matched = false;
    for line in text.lines() {
        let (before, after) = line
            .split_once(" - ")
            .ok_or_else(|| invalid("malformed application cgroup mount record"))?;
        let fields: Vec<_> = before.split_ascii_whitespace().collect();
        if fields.get(4) == Some(&CGROUP) {
            if matched
                || fields.get(2).copied() != Some(expected_device.as_str())
                || fields.get(3) != Some(&"/")
                || after.split_ascii_whitespace().next() != Some("cgroup2")
                || !fields
                    .get(5)
                    .is_some_and(|options| options.split(',').any(|x| x == "rw"))
            {
                return Err(invalid(
                    "application cgroup requires one writable unified hierarchy root",
                ));
            }
            matched = true;
        }
    }
    if !matched {
        return Err(invalid("application cgroup hierarchy is not mounted"));
    }
    Ok(())
}

fn create(parent: &File, name: &str, uid: u32) -> io::Result<File> {
    let path = at(parent, name);
    match fs::create_dir(&path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    // Existing app ownership is valid only at this trusted pre-session point.
    let file = directory(&path, &[0, uid])?;
    if file.metadata()?.dev() != parent.metadata()?.dev() {
        return Err(invalid("application cgroup directory crosses a mount"));
    }
    Ok(file)
}

fn assign(file: &File, uid: u32) -> io::Result<()> {
    std::os::unix::fs::fchown(file, Some(uid), Some(uid))?;
    let metadata = file.metadata()?;
    if metadata.uid() != uid || metadata.gid() != uid {
        return Err(invalid("application cgroup ownership did not read back"));
    }
    Ok(())
}

/// The immutable account and durable reservation must already be validated.
/// Startup precedes every human/app process; this never repairs a live tree.
pub(crate) fn prepare(uid: u32) -> Result<(), String> {
    if !(65536..=2147483647).contains(&uid) {
        return Err("invalid application runtime uid".into());
    }
    let result = (|| {
        let mut hierarchy = directory(Path::new("/"), &[0])?;
        for component in ["sys", "fs", "cgroup"] {
            hierarchy = directory(&at(&hierarchy, component), &[0])?;
        }
        require_mount(
            &bounded(File::open("/proc/self/mountinfo")?, MOUNT_LIMIT)?,
            hierarchy.metadata()?.dev(),
        )?;
        require_controllers(&read(&hierarchy, "cgroup.controllers", 0)?)?;
        // td-svc enables these before firstboot. Failure withholds enrollment;
        // this preparation never alters the system's top-level delegation.
        require_controllers(&read(&hierarchy, "cgroup.subtree_control", 0)?)?;
        let app = create(&hierarchy, &format!("td-app-{uid}"), uid)?;
        if read(&app, "cgroup.type", uid)? != "domain"
            || !read(&app, "cgroup.procs", uid)?.is_empty()
            || !read(&app, "cgroup.threads", uid)?.is_empty()
        {
            return Err(invalid(
                "application delegation must be an empty domain cgroup",
            ));
        }
        if !read(&app, "cgroup.events", uid)?
            .lines()
            .any(|line| line == "populated 0")
        {
            return Err(invalid(
                "application delegation has live descendant processes",
            ));
        }
        require_controllers(&read(&app, "cgroup.controllers", uid)?)?;
        write(&app, "cgroup.subtree_control", "+cpu +memory +pids\n", uid)?;
        require_controllers(&read(&app, "cgroup.subtree_control", uid)?)?;
        let session = create(&app, "session", 0)?;
        if read(&session, "cgroup.type", 0)? != "domain"
            || !read(&session, "cgroup.procs", uid)?.is_empty()
            || !read(&session, "cgroup.threads", uid)?.is_empty()
            || !read(&session, "cgroup.subtree_control", 0)?.is_empty()
        {
            return Err(invalid(
                "application session must be an empty domain leaf before startup",
            ));
        }
        assign(&session, 0)?;
        for name in ["cgroup.procs", "cgroup.subtree_control", "cgroup.threads"] {
            assign(&control(&app, name, false, uid)?, uid)?;
        }
        for name in ["cgroup.procs", "cgroup.threads"] {
            assign(&control(&session, name, false, uid)?, uid)?;
        }
        // Delegate the directory only after controllers and ownership read back.
        assign(&app, uid)
    })();
    result.map_err(|error| format!("prepare application uid {uid} cgroup: {error}"))?;
    crate::principal_store::prepare_application_runtime(uid)
        .map_err(|error| format!("prepare application uid {uid} runtime: {error}"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn mount_admission_requires_one_writable_unified_root() {
        let valid = "42 10 0:21 / /sys/fs/cgroup rw,nosuid,nodev,noexec - cgroup2 cgroup rw\n";
        require_mount(valid, 21).unwrap();
        assert!(require_mount(valid, 22).is_err());
        // Linux's noncontiguous dev_t layout includes high major/minor bits.
        let high = valid.replace("0:21", "703710:305419896");
        require_mount(&high, 0xab123456cde78).unwrap();
        for wrong in [
            valid.replace(" - cgroup2 ", " - tmpfs "),
            valid.replace(" / /sys", " /delegated /sys"),
            valid.replace(" rw,nosuid", " ro,nosuid"),
            format!("{valid}{valid}"),
            String::new(),
            "malformed".into(),
        ] {
            assert!(require_mount(&wrong, 21).is_err(), "{wrong}");
        }
    }

    #[test]
    fn controllers_are_whole_words_and_all_three_are_required() {
        require_controllers("memory pids cpu io").unwrap();
        for wrong in [
            "",
            "cpu memory",
            "cpuset memory pids",
            "cpu memory pids_extra",
        ] {
            assert!(require_controllers(wrong).is_err(), "{wrong}");
        }
    }

    #[test]
    fn non_application_ids_refuse_before_any_filesystem_access() {
        for uid in [0, 991, 1000, 65533, 65534, 65535, 2147483648, u32::MAX] {
            assert_eq!(prepare(uid).unwrap_err(), "invalid application runtime uid");
        }
    }
}

//! Per-instance cgroup-v2 enforcement below td-svc's delegated subtree.

use crate::authority::{self, ResolvedResourceLimits, CGROUP_ROOT};
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const DELEGATE_COMPONENT: &str = "td-user-1000";
const MAX_CONTROL_BYTES: u64 = 4096;
const MAX_ACTIVE_SCAN: usize = 256;
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(10);
const CLEANUP_POLL: Duration = Duration::from_millis(10);

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct MemoryEvents {
    pub(crate) high: u64,
    pub(crate) max: u64,
    pub(crate) oom: u64,
    pub(crate) oom_kill: u64,
    pub(crate) oom_group_kill: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct Report {
    pub(crate) events: MemoryEvents,
    pub(crate) peak: u64,
}

impl Report {
    pub(crate) fn diagnostic(self) -> String {
        format!(
            "memory.events high={} max={} oom={} oom_kill={} oom_group_kill={}; memory.peak={}",
            self.events.high,
            self.events.max,
            self.events.oom,
            self.events.oom_kill,
            self.events.oom_group_kill,
            self.peak,
        )
    }
}

pub(crate) struct Instance {
    directory: PathBuf,
    membership: String,
    remove_on_drop: bool,
}

impl Instance {
    pub(crate) fn create(
        instance: &str,
        limits: ResolvedResourceLimits,
        uid: u32,
        gid: u32,
    ) -> io::Result<Self> {
        validate_instance_name(instance)?;
        let root = Path::new(CGROUP_ROOT);
        require_delegation(root, uid, gid)?;
        let directory = root.join(instance);
        fs::create_dir(&directory).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("create application cgroup {}: {error}", directory.display()),
            )
        })?;
        let mut cgroup = Self {
            directory,
            membership: membership_for_instance(instance)?,
            remove_on_drop: true,
        };
        if let Err(error) = configure(&cgroup.directory, limits) {
            let cleanup = cgroup.remove();
            return match cleanup {
                Ok(()) => Err(error),
                Err(cleanup) => Err(io::Error::other(format!(
                    "{error}; remove refused application cgroup: {cleanup}"
                ))),
            };
        }
        Ok(cgroup)
    }

    pub(crate) fn membership(&self) -> &str {
        &self.membership
    }

    pub(crate) fn attach(&self, pid: u32) -> io::Result<()> {
        let expected = pid.to_string();
        write_control(&self.directory.join("cgroup.procs"), &expected)?;
        let members = read_control(&self.directory.join("cgroup.procs"))?;
        if members != expected {
            return Err(io::Error::other(format!(
                "application cgroup membership readback is {members:?}, expected only PID {pid}"
            )));
        }
        let proc_path = format!("/proc/{pid}/cgroup");
        let actual = read_bounded(Path::new(&proc_path))?;
        require_membership_text(&actual, &self.membership).map_err(|error| {
            io::Error::other(format!("stage-2 cgroup membership at {proc_path}: {error}"))
        })
    }

    pub(crate) fn report_and_release(mut self) -> io::Result<Report> {
        let report = report(&self.directory);
        self.remove_on_drop = false;
        report.map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("read application cgroup diagnostics: {error}"),
            )
        })
    }

    fn remove(&mut self) -> io::Result<()> {
        if !self.remove_on_drop {
            return Ok(());
        }
        fs::remove_dir(&self.directory)?;
        self.remove_on_drop = false;
        Ok(())
    }
}

impl Drop for Instance {
    fn drop(&mut self) {
        if self.remove_on_drop {
            let _ = fs::remove_dir(&self.directory);
        }
    }
}

pub(crate) fn require_current_membership(expected: &str) -> io::Result<()> {
    validate_membership(expected)?;
    let actual = read_bounded(Path::new("/proc/self/cgroup"))?;
    require_membership_text(&actual, expected)
}

pub(crate) fn validate_expected_membership(expected: &str) -> io::Result<()> {
    validate_membership(expected)
}

pub(crate) fn membership_for_instance(instance: &str) -> io::Result<String> {
    validate_instance_name(instance)?;
    Ok(format!("/{DELEGATE_COMPONENT}/{instance}"))
}

pub(crate) fn remove_abandoned(expected: &str, uid: u32, gid: u32) -> io::Result<()> {
    validate_membership(expected)?;
    let prefix = format!("/{DELEGATE_COMPONENT}/");
    let instance = expected
        .strip_prefix(&prefix)
        .ok_or_else(|| io::Error::other("validated cgroup membership lost its prefix"))?;
    let root = Path::new(CGROUP_ROOT);
    match fs::symlink_metadata(root) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
        Ok(_) => {}
    }
    let directory = root.join(instance);
    match fs::symlink_metadata(&directory) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
        Ok(metadata) if !metadata.file_type().is_dir() => {
            return Err(io::Error::other(format!(
                "abandoned application cgroup {} is not a directory",
                directory.display()
            )));
        }
        Ok(_) => {}
    }
    require_delegation(root, uid, gid)?;
    if let Err(error) = wait_until_empty(&directory) {
        if error.kind() == io::ErrorKind::NotFound {
            return Ok(());
        }
        return Err(error);
    }
    match fs::remove_dir(&directory) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(io::Error::new(
            error.kind(),
            format!(
                "remove abandoned application cgroup {}: {error}",
                directory.display()
            ),
        )),
    }
}

pub(crate) fn probe_active(
    application: &str,
    limits: ResolvedResourceLimits,
    uid: u32,
    gid: u32,
) -> io::Result<String> {
    authority::validate_application_name(application)?;
    let root = Path::new(CGROUP_ROOT);
    require_delegation(root, uid, gid)?;
    let entries = fs::read_dir(root)?;
    let mut active: Option<(String, Report)> = None;
    let mut seen = 0usize;
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        seen = seen.saturating_add(1);
        if seen > MAX_ACTIVE_SCAN {
            return Err(io::Error::other(format!(
                "delegated cgroup contains more than {MAX_ACTIVE_SCAN} child cgroups"
            )));
        }
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if !instance_belongs_to(&name, application) {
            continue;
        }
        let directory = entry.path();
        if !event_value(&directory.join("cgroup.events"), "populated")? {
            continue;
        }
        require_configuration(&directory, limits)?;
        if read_control(&directory.join("cgroup.procs"))?.is_empty() {
            return Err(io::Error::other(format!(
                "populated application cgroup {name:?} has no process-group leaders"
            )));
        }
        let candidate = (name, report(&directory)?);
        if active.replace(candidate).is_some() {
            return Err(io::Error::other(format!(
                "more than one active cgroup exists for application {application:?}"
            )));
        }
    }
    let Some((instance, report)) = active else {
        return Err(io::Error::other(format!(
            "no active cgroup exists for application {application:?}"
        )));
    };
    Ok(format!(
        "TD-JAIL-RESOURCE-CAPS-OK instance={instance} {}",
        report.diagnostic()
    ))
}

fn configure(directory: &Path, limits: ResolvedResourceLimits) -> io::Result<()> {
    write_control(
        &directory.join("memory.high"),
        &limits.memory_high_bytes.to_string(),
    )?;
    write_control(
        &directory.join("memory.max"),
        &limits.memory_max_bytes.to_string(),
    )?;
    write_control(&directory.join("memory.oom.group"), "1")?;
    write_control(&directory.join("pids.max"), &limits.pids_max.to_string())?;
    require_configuration(directory, limits)
}

fn require_configuration(directory: &Path, limits: ResolvedResourceLimits) -> io::Result<()> {
    for (name, expected) in [
        ("memory.high", limits.memory_high_bytes.to_string()),
        ("memory.max", limits.memory_max_bytes.to_string()),
        ("memory.oom.group", "1".to_string()),
        ("pids.max", limits.pids_max.to_string()),
    ] {
        let actual = read_control(&directory.join(name))?;
        if actual != expected {
            return Err(io::Error::other(format!(
                "application cgroup {name} read back as {actual:?}, expected {expected:?}"
            )));
        }
    }
    Ok(())
}

fn require_delegation(root: &Path, uid: u32, gid: u32) -> io::Result<()> {
    let canonical = fs::canonicalize(root).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "resolve delegated application cgroup {}: {error}",
                root.display()
            ),
        )
    })?;
    if canonical != root {
        return Err(io::Error::other(format!(
            "delegated application cgroup resolves to {}, expected {}",
            canonical.display(),
            root.display()
        )));
    }
    let metadata = fs::symlink_metadata(root)?;
    if !metadata.file_type().is_dir() {
        return Err(io::Error::other(format!(
            "delegated application cgroup {} is not a directory",
            root.display()
        )));
    }
    require_owner(root, uid, gid)?;
    for name in ["cgroup.procs", "cgroup.subtree_control", "cgroup.threads"] {
        require_owner(&root.join(name), uid, gid)?;
    }
    require_words(
        &read_control(&root.join("cgroup.controllers"))?,
        &["memory", "pids"],
        "delegated cgroup controllers",
    )?;
    require_words(
        &read_control(&root.join("cgroup.subtree_control"))?,
        &["memory", "pids"],
        "delegated cgroup subtree control",
    )
}

fn wait_until_empty(directory: &Path) -> io::Result<()> {
    let deadline = Instant::now()
        .checked_add(CLEANUP_TIMEOUT)
        .ok_or_else(|| io::Error::other("cgroup cleanup deadline overflow"))?;
    loop {
        let procs = read_control(&directory.join("cgroup.procs"))?;
        let populated = event_value(&directory.join("cgroup.events"), "populated")?;
        if procs.is_empty() && !populated {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other(format!(
                "cgroup remained populated past the cleanup deadline: \
                 cgroup.procs={procs:?}, populated={populated}"
            )));
        }
        std::thread::sleep(CLEANUP_POLL);
    }
}

fn report(directory: &Path) -> io::Result<Report> {
    Ok(Report {
        events: parse_memory_events(&read_control(&directory.join("memory.events"))?)?,
        peak: read_control(&directory.join("memory.peak"))?
            .parse()
            .map_err(|error| io::Error::other(format!("invalid memory.peak: {error}")))?,
    })
}

fn parse_memory_events(text: &str) -> io::Result<MemoryEvents> {
    let mut events = MemoryEvents::default();
    let mut found_high = false;
    let mut found_max = false;
    let mut found_oom = false;
    let mut found_oom_kill = false;
    let mut found_oom_group_kill = false;
    for line in text.lines() {
        let mut fields = line.split_ascii_whitespace();
        let name = fields
            .next()
            .ok_or_else(|| io::Error::other("empty memory.events row"))?;
        let value: u64 = fields
            .next()
            .ok_or_else(|| io::Error::other(format!("memory.events {name} has no value")))?
            .parse()
            .map_err(|error| {
                io::Error::other(format!("memory.events {name} is invalid: {error}"))
            })?;
        if fields.next().is_some() {
            return Err(io::Error::other(format!(
                "memory.events {name} has trailing fields"
            )));
        }
        match name {
            "high" => {
                events.high = value;
                found_high = true;
            }
            "max" => {
                events.max = value;
                found_max = true;
            }
            "oom" => {
                events.oom = value;
                found_oom = true;
            }
            "oom_kill" => {
                events.oom_kill = value;
                found_oom_kill = true;
            }
            "oom_group_kill" => {
                events.oom_group_kill = value;
                found_oom_group_kill = true;
            }
            _ => {}
        }
    }
    if !(found_high && found_max && found_oom && found_oom_kill && found_oom_group_kill) {
        return Err(io::Error::other(
            "memory.events lacks high, max, oom, oom_kill or oom_group_kill",
        ));
    }
    Ok(events)
}

fn event_value(path: &Path, key: &str) -> io::Result<bool> {
    let text = read_control(path)?;
    for line in text.lines() {
        let mut fields = line.split_ascii_whitespace();
        if fields.next() != Some(key) {
            continue;
        }
        return match (fields.next(), fields.next()) {
            (Some("0"), None) => Ok(false),
            (Some("1"), None) => Ok(true),
            _ => Err(io::Error::other(format!(
                "{} has an invalid {key} row",
                path.display()
            ))),
        };
    }
    Err(io::Error::other(format!(
        "{} has no {key} row",
        path.display()
    )))
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

fn read_control(path: &Path) -> io::Result<String> {
    Ok(read_bounded(path)?.trim().to_string())
}

fn read_bounded(path: &Path) -> io::Result<String> {
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(MAX_CONTROL_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_CONTROL_BYTES {
        return Err(io::Error::other(format!(
            "{} exceeds {MAX_CONTROL_BYTES} bytes",
            path.display()
        )));
    }
    String::from_utf8(bytes)
        .map_err(|error| io::Error::other(format!("{} is not UTF-8: {error}", path.display())))
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

fn require_owner(path: &Path, uid: u32, gid: u32) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.uid() != uid || metadata.gid() != gid {
        return Err(io::Error::other(format!(
            "{} is not owned by {uid}:{gid}",
            path.display()
        )));
    }
    Ok(())
}

fn validate_instance_name(instance: &str) -> io::Result<()> {
    let Some((application, suffix)) = instance.rsplit_once('-') else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "application cgroup instance has no random suffix",
        ));
    };
    authority::validate_application_name(application)?;
    if suffix.len() != 16
        || !suffix
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "application cgroup instance suffix is not 16 lowercase hexadecimal characters",
        ));
    }
    Ok(())
}

fn instance_belongs_to(instance: &str, application: &str) -> bool {
    validate_instance_name(instance).is_ok()
        && instance
            .strip_prefix(application)
            .is_some_and(|suffix| suffix.starts_with('-') && suffix.len() == 17)
}

fn validate_membership(membership: &str) -> io::Result<()> {
    let prefix = format!("/{DELEGATE_COMPONENT}/");
    let instance = membership.strip_prefix(&prefix).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "application cgroup membership is outside the delegated subtree",
        )
    })?;
    if instance.contains('/') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "application cgroup membership is not one direct child",
        ));
    }
    validate_instance_name(instance)
}

fn require_membership_text(text: &str, expected: &str) -> io::Result<()> {
    validate_membership(expected)?;
    let mut lines = text.lines();
    let wanted = format!("0::{expected}");
    if lines.next() != Some(wanted.as_str()) || lines.next().is_some() {
        return Err(io::Error::other(format!(
            "unified cgroup membership is {text:?}, expected {wanted:?}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn instance_and_membership_grammars_are_closed() {
        let instance = "firefox-0123456789abcdef";
        validate_instance_name(instance).unwrap();
        assert!(instance_belongs_to(instance, "firefox"));
        assert!(!instance_belongs_to(instance, "fire"));
        let membership = format!("/{DELEGATE_COMPONENT}/{instance}");
        assert_eq!(membership_for_instance(instance).unwrap(), membership);
        validate_membership(&membership).unwrap();
        require_membership_text(&format!("0::{membership}\n"), &membership).unwrap();
        for invalid in [
            "firefox",
            "firefox-0123",
            "firefox-0123456789abcdeG",
            "fire/fox-0123456789abcdef",
        ] {
            assert!(
                validate_instance_name(invalid).is_err(),
                "accepted {invalid:?}"
            );
        }
        assert!(require_membership_text("1:name:/elsewhere\n", &membership).is_err());
        assert!(require_membership_text("0::/elsewhere\n", &membership).is_err());
    }

    #[test]
    fn memory_event_diagnostics_require_the_enforcement_rows() {
        let parsed =
            parse_memory_events("low 0\nhigh 2\nmax 3\noom 4\noom_kill 5\noom_group_kill 6\n")
                .unwrap();
        assert_eq!(
            parsed,
            MemoryEvents {
                high: 2,
                max: 3,
                oom: 4,
                oom_kill: 5,
                oom_group_kill: 6,
            }
        );
        assert!(parse_memory_events("high 0\nmax 0\noom 0\noom_kill 0\n").is_err());
        assert!(
            parse_memory_events("high 0 extra\nmax 0\noom 0\noom_kill 0\noom_group_kill 0\n")
                .is_err()
        );
    }

    #[test]
    fn controller_words_are_tokens_not_substrings() {
        require_words("cpu memory pids", &["memory", "pids"], "controllers").unwrap();
        assert!(require_words("cpu memoryish pids", &["memory"], "controllers").is_err());
    }
}
